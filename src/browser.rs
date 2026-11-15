//! Headless-Chromium render engine for the browser tools (`web_fetch`,
//! `page_screenshot`, `site_scrape`). Wraps chromiumoxide (CDP) behind the
//! contract every tool slice codes against:
//!
//! - [`Browser::launch`] spawns Chrome/Chromium once (`--headless=new`, fresh
//!   `--user-data-dir` per instance, `--no-sandbox` when euid==0) and keeps the
//!   CDP event handler alive on a background task.
//! - Every [`Browser::load`]/[`Browser::screenshot`] call creates a FRESH
//!   `Target.createBrowserContext` (`disposeOnDetach`) — the desktop analogue
//!   of Android's per-render unauthenticated WebView profile — so cookies,
//!   storage, and cache never leak between renders or into the operator's
//!   profile. Loads may run concurrently; each gets its own context+page.
//! - `load` waits for the load lifecycle event, settles ~1.2s for late
//!   network-driven content (Android `extraWaitMs`), then optionally polls a
//!   CSS selector, and extracts title/text/links/extracted JS.
//!
//! Residual SSRF gap vs the Android app: Android's WebView intercepts every
//! request (subresources included) through the policy-checked fetch; here
//! only the navigation URL is policy-checked — after commit, Chrome may
//! fetch subresources (img/script/iframe) from anywhere, including LAN
//! addresses. Same-origin policy still blocks cross-origin reads, but
//! resource-loaded oracles exist. Run bare-metal workers in a container or
//! network-isolated environment (Umbrel/StartOS apps already are).
//!
//! URL policy is NOT enforced here: callers must run `crate::url_policy::check`
//! before `load`/`screenshot` (the url_policy slice owns that module).
//!
//! User agent: the stock headless Chrome UA — deliberately not spoofed.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use futures::StreamExt;
use serde_json::Value;

use chromiumoxide::cdp::browser_protocol::emulation::SetDeviceMetricsOverrideParams;
use chromiumoxide::cdp::browser_protocol::target::{CreateBrowserContextParams, CreateTargetParams};
use chromiumoxide::Page as ChromePage;

/// Error from a render: navigation failure, JS evaluation failure, timeout.
/// Carries Chrome's own error text when available.
#[derive(Debug)]
pub struct RenderError(pub String);

impl std::fmt::Display for RenderError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for RenderError {}

impl From<String> for RenderError {
    fn from(s: String) -> Self {
        RenderError(s)
    }
}

impl From<&str> for RenderError {
    fn from(s: &str) -> Self {
        RenderError(s.to_string())
    }
}

/// One rendered page: where it ended up and what an agent consumes.
#[derive(Debug, Clone)]
pub struct Page {
    pub final_url: String,
    pub title: String,
    pub text: String,
    pub links: Vec<String>,
    pub extracted: Option<serde_json::Value>,
}

/// One shared headless browser. `Send + Sync`: the inner chromiumoxide
/// `Browser` sits behind a `tokio::sync::Mutex` (its `Sync`-ness depends on
/// tokio's `Child`, which we don't want to bet the contract on); the CDP
/// `Handler` is driven by a spawned task and dies with it.
pub struct Browser {
    inner: Arc<tokio::sync::Mutex<chromiumoxide::Browser>>,
    /// Keeps the CDP event handler alive; aborted on drop.
    _handler: tokio::task::JoinHandle<()>,
}

// Contract guarantee: tools share `&Browser` across concurrent tokio tasks.
const _: fn() = || {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<Browser>();
};

/// Default viewport for `load` text renders (1280x800 like the desktop tools).
const LOAD_VIEWPORT_W: u32 = 1280;
const LOAD_VIEWPORT_H: u32 = 800;
/// Android parity: fixed settle after load for late XHR/fetch content.
const SETTLE_MS: u64 = 1_200;
/// Poll cadence for wait_for_selector.
const POLL_MS: u64 = 250;
/// Hard cap on any single render, independent of caller's timeout_ms.
const MAX_RENDER_MS: u64 = 120_000;

/// Absolute hrefs of every link, resolved by the browser — port of Android's
/// `LINKS_SCRIPT` (`Array.from(document.links, a => a.href)`).
const LINKS_SCRIPT: &str = "Array.from(document.links, function (a) { return a.href; })";

/// innerText first, textContent if blank — port of Android's fallback:
/// innerText is layout-dependent and can be empty before custom elements lay
/// out (m.youtube.com); never extract outerHTML (wedges the main thread).
const TEXT_SCRIPT: &str = "(function(){ var b = document.body; if (!b) return ''; \
     var t = b.innerText; if (!t || !t.trim()) t = b.textContent; return t; })()";

/// CSS selector presence probe: "found" / "missing" / "invalid".
fn selector_probe(selector: &str) -> String {
    // JSON-encode the selector so quotes/backslashes can't break out of the literal.
    let sel = serde_json::to_string(selector).unwrap_or_else(|_| "\"\"".into());
    format!(
        "(function(){{ try {{ return document.querySelector({}) !== null ? 'found' : 'missing'; }} \
         catch (e) {{ return 'invalid'; }} }})()",
        sel
    )
}

/// RAII guard: dispose the fresh browser context when the render ends
/// (success, error, or early return). `disposeOnDetach` on the context is the
/// backstop if we die mid-render.
struct IsolatedContext<'a> {
    browser: &'a tokio::sync::Mutex<chromiumoxide::Browser>,
    id: chromiumoxide::cdp::browser_protocol::browser::BrowserContextId,
    page: Option<ChromePage>,
}

impl<'a> IsolatedContext<'a> {
    async fn dispose(mut self) {
        if let Some(page) = self.page.take() {
            let _ = page.close().await;
        }
        let _ = self
            .browser
            .lock()
            .await
            .dispose_browser_context(self.id.clone())
            .await;
    }
}

impl Browser {
    /// Launch a headless browser. `binary` is an explicit path (or PATH name);
    /// `None` auto-detects in order: `$HENCH_BROWSER_BIN`, google-chrome-stable,
    /// google-chrome, chromium, chromium-browser (first that exists). Falls back
    /// to chromiumoxide's own detection if none of those resolve.
    pub async fn launch(binary: Option<&str>) -> anyhow::Result<Browser> {
        let executable = match binary {
            Some(b) => Some(resolve_binary(b)?),
            None => detect_browser_binary(),
        };

        let mut builder = chromiumoxide::BrowserConfig::builder()
            .new_headless_mode() // --headless=new --hide-scrollbars --mute-audio
            .window_size(LOAD_VIEWPORT_W, LOAD_VIEWPORT_H)
            // Fresh profile dir per Browser instance (std::env::temp_dir()).
            .user_data_dir(fresh_profile_dir());
        if let Some(exe) = executable {
            builder = builder.chrome_executable(exe);
        }
        if is_root() {
            // Chromium refuses to run sandboxed as root.
            builder = builder.no_sandbox();
        }
        let config = builder
            .build()
            .map_err(|e| anyhow::anyhow!("browser config: {e}"))?;

        let (browser, mut handler) = chromiumoxide::Browser::launch(config).await?;
        let handler_task = tokio::spawn(async move {
            // The handler must be polled or no CDP traffic flows. Events are
            // unused — each Page subscribes through its own channels.
            while let Some(_event) = handler.next().await {}
        });
        Ok(Browser {
            inner: Arc::new(tokio::sync::Mutex::new(browser)),
            _handler: handler_task,
        })
    }

    /// Clean shutdown for probe-launched browsers (registry gating): ask
    /// Chrome to exit, then stop the handler pump. Not for shared render
    /// browsers — callers still rendering would lose their CDP connection.
    pub async fn shutdown(self) {
        let mut b = self.inner.lock().await;
        let _ = b.close().await;
        // Reap the child so chromiumoxide's Drop doesn't warn about a
        // background kill on every registry probe.
        let _ = b.wait().await;
        self._handler.abort();
    }

    /// Render `url` for text: navigate, wait for load + settle, optional CSS
    /// selector wait, then title/text/links/extracted.
    ///
    /// Whole call is bounded by `timeout_ms` (hard-capped at
    /// [`MAX_RENDER_MS`]); on expiry returns `RenderError("render timed out
    /// after Ns")`. `collect_links` and `extract_script` only run on success.
    pub async fn load(
        &self,
        url: &str,
        timeout_ms: u64,
        wait_for_selector: Option<&str>,
        collect_links: bool,
        extract_script: Option<&str>,
    ) -> Result<Page, RenderError> {
        let budget = Duration::from_millis(timeout_ms.min(MAX_RENDER_MS));
        tokio::time::timeout(
            budget,
            self.load_inner(url, budget, wait_for_selector, collect_links, extract_script),
        )
        .await
        .map_err(|_| RenderError(format!("render timed out after {}s", budget.as_secs())))?
    }

    async fn load_inner(
        &self,
        url: &str,
        budget: Duration,
        wait_for_selector: Option<&str>,
        collect_links: bool,
        extract_script: Option<&str>,
    ) -> Result<Page, RenderError> {
        let deadline = Instant::now() + budget;
        let iso = self.new_isolated_page().await?;
        // Clone the cheap handle (Arc internals) so `iso.dispose()` can run at
        // the end without a borrow conflict.
        let page = iso.page.clone().expect("page set by new_isolated_page");

        // Render body; result captured so the context is disposed on EVERY
        // exit path (the `?`s would otherwise leak it until Chrome GCs).
        let result = async {
            navigate(&page, url, deadline).await?;
            // Settle: fixed extra wait like Android's extraWaitMs for late content.
            settle(deadline).await;

            if let Some(sel) = wait_for_selector {
                await_selector(&page, sel, deadline).await?;
            }

            let title = page.get_title().await.unwrap_or_default().unwrap_or_default();
            let text = eval_string(&page, TEXT_SCRIPT).await.unwrap_or_default();
            let links = if collect_links {
                eval_value(&page, LINKS_SCRIPT)
                    .await
                    .ok()
                    .and_then(|v| v.as_array().cloned())
                    .map(|arr| {
                        arr.iter()
                            .filter_map(|l| l.as_str().map(str::to_string))
                            .collect()
                    })
                    .unwrap_or_default()
            } else {
                Vec::new()
            };
            let extracted = match extract_script {
                Some(script) => eval_value(&page, script).await.ok().filter(|v| !v.is_null()),
                None => None,
            };
            let final_url = page
                .url()
                .await
                .ok()
                .flatten()
                .unwrap_or_else(|| url.to_string());
            Ok(Page {
                final_url,
                title,
                text,
                links,
                extracted,
            })
        }
        .await;

        iso.dispose().await;
        result
    }

    /// Navigate `url` at `width`x`height` and capture a PNG of the viewport.
    /// Returns the rendered page (url+title) and the PNG bytes.
    pub async fn screenshot(
        &self,
        url: &str,
        width: u32,
        height: u32,
        timeout_ms: u64,
    ) -> Result<(Page, Vec<u8>), RenderError> {
        let budget = Duration::from_millis(timeout_ms.min(MAX_RENDER_MS));
        tokio::time::timeout(budget, self.screenshot_inner(url, width, height, budget))
            .await
            .map_err(|_| RenderError(format!("render timed out after {}s", budget.as_secs())))?
    }

    async fn screenshot_inner(
        &self,
        url: &str,
        width: u32,
        height: u32,
        budget: Duration,
    ) -> Result<(Page, Vec<u8>), RenderError> {
        let deadline = Instant::now() + budget;
        // Fresh context + blank page; device metrics are overridden BEFORE
        // navigating so layout/CSS viewport match the requested size.
        let iso = self.new_isolated_page().await?;
        let page = iso.page.clone().expect("page set by new_isolated_page");

        let result = async {
            page.execute(SetDeviceMetricsOverrideParams::new(
                width as i64,
                height as i64,
                1.0,
                false,
            ))
            .await
            .map_err(cdp_err)?;

            navigate(&page, url, deadline).await?;
            settle(deadline).await;

            let png = page
                .screenshot(chromiumoxide::page::ScreenshotParams::default())
                .await
                .map_err(cdp_err)?;
            let title = page.get_title().await.unwrap_or_default().unwrap_or_default();
            let final_url = page
                .url()
                .await
                .ok()
                .flatten()
                .unwrap_or_else(|| url.to_string());
            Ok((
                Page {
                    final_url,
                    title,
                    text: String::new(),
                    links: Vec::new(),
                    extracted: None,
                },
                png,
            ))
        }
        .await;

        iso.dispose().await;
        result
    }

    /// Fresh incognito-equivalent context + blank page. Navigation is the
    /// caller's job so viewport emulation can be set before `goto`.
    async fn new_isolated_page(&self) -> Result<IsolatedContext<'_>, RenderError> {
        // Lock only for the two CDP calls — page ops afterwards run lock-free.
        let ctx_id = {
            let browser = self.inner.lock().await;
            browser
                .create_browser_context(CreateBrowserContextParams {
                    dispose_on_detach: Some(true),
                    ..Default::default()
                })
                .await
                .map_err(cdp_err)?
        };

        let params = CreateTargetParams::builder()
            .url("about:blank")
            .browser_context_id(ctx_id.clone())
            .build()
            .map_err(RenderError)?;
        let page = {
            let browser = self.inner.lock().await;
            match browser.new_page(params).await {
                Ok(p) => p,
                Err(e) => {
                    // Don't leak the context if page creation failed.
                    let _ = browser.dispose_browser_context(ctx_id.clone()).await;
                    return Err(cdp_err(e));
                }
            }
        };
        Ok(IsolatedContext {
            browser: &self.inner,
            id: ctx_id,
            page: Some(page),
        })
    }
}

/// Navigate `page` to `url`, bounded by `deadline`. `Page::goto` resolves on
/// the load lifecycle event (or Chrome's errorText on failure).
async fn navigate(page: &ChromePage, url: &str, deadline: Instant) -> Result<(), RenderError> {
    match tokio::time::timeout_at(
        tokio::time::Instant::from_std(deadline),
        page.goto(url),
    )
    .await
    {
        Ok(Ok(_)) => Ok(()),
        Ok(Err(e)) => Err(cdp_err(e)),
        Err(_) => Err(RenderError(format!(
            "render timed out after {}s",
            deadline
                .saturating_duration_since(Instant::now())
                .as_secs()
        ))),
    }
}

/// Poll `document.querySelector(sel)` until found, invalid, or deadline.
/// A missing selector is non-fatal (Android renders anyway) — only an
/// invalid selector errors.
async fn await_selector(page: &ChromePage, selector: &str, deadline: Instant) -> Result<(), RenderError> {
    let probe = selector_probe(selector);
    loop {
        match eval_string(page, &probe).await {
            Ok(v) if v == "found" => return Ok(()),
            Ok(v) if v == "invalid" => {
                return Err(RenderError(format!("invalid CSS selector: {selector}")))
            }
            _ => {}
        }
        if Instant::now() >= deadline {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(POLL_MS)).await;
    }
}

/// Fixed settle for late network-driven content, capped at `deadline`.
async fn settle(deadline: Instant) {
    let wait = Duration::from_millis(SETTLE_MS).min(deadline.saturating_duration_since(Instant::now()));
    tokio::time::sleep(wait).await;
}

/// `page.evaluate` → string value (unwraps JSON string results).
async fn eval_string(page: &ChromePage, script: &str) -> Result<String, RenderError> {
    let res = page.evaluate(script).await.map_err(cdp_err)?;
    match res.into_value::<Value>() {
        Ok(Value::String(s)) => Ok(s),
        Ok(v) => Ok(v.to_string()),
        Err(e) => Err(RenderError(format!("evaluate: {e}"))),
    }
}

/// `page.evaluate` → raw JSON value (arrays/objects/strings all typed).
async fn eval_value(page: &ChromePage, script: &str) -> Result<Value, RenderError> {
    let res = page.evaluate(script).await.map_err(cdp_err)?;
    res.into_value::<Value>()
        .map_err(|e| RenderError(format!("evaluate: {e}")))
}

fn cdp_err(e: chromiumoxide::error::CdpError) -> RenderError {
    RenderError(e.to_string())
}

/// Candidate browser binaries in detection order (after $HENCH_BROWSER_BIN).
const BINARY_CANDIDATES: &[&str] = &[
    "google-chrome-stable",
    "google-chrome",
    "chromium",
    "chromium-browser",
];

/// Resolve `name` to an executable path: absolute/relative path that exists,
/// else PATH lookup. `Err` carries a human-readable reason.
fn resolve_binary(name: &str) -> anyhow::Result<PathBuf> {
    let p = PathBuf::from(name);
    if p.is_absolute() || name.contains('/') {
        if p.exists() {
            return Ok(p);
        }
        anyhow::bail!("browser binary not found: {name}");
    }
    find_on_path(name).ok_or_else(|| anyhow::anyhow!("browser binary not found on PATH: {name}"))
}

/// Minimal PATH search — avoids pulling in the `which` crate for four names.
fn find_on_path(name: &str) -> Option<PathBuf> {
    let paths = std::env::var_os("PATH")?;
    std::env::split_paths(&paths)
        .map(|dir| dir.join(name))
        .find(|p| p.is_file())
}

/// First existing binary among $HENCH_BROWSER_BIN + candidates, in order.
/// `None` if nothing resolves (caller falls back to chromiumoxide detection).
fn detect_browser_binary() -> Option<PathBuf> {
    if let Ok(env) = std::env::var("HENCH_BROWSER_BIN") {
        if !env.trim().is_empty() {
            if let Ok(p) = resolve_binary(env.trim()) {
                return Some(p);
            }
        }
    }
    BINARY_CANDIDATES.iter().find_map(|c| find_on_path(c))
}

/// euid==0 without a libc dep: parse the `Uid:` line of /proc/self/status.
fn is_root() -> bool {
    std::fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|s| {
            s.lines()
                .find(|l| l.starts_with("Uid:"))
                .and_then(|l| l.split_whitespace().nth(1))
                .and_then(|uid| uid.parse::<u32>().ok())
        })
        .map(|uid| uid == 0)
        .unwrap_or(false)
}

/// Unique temp profile dir per Browser instance.
fn fresh_profile_dir() -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    std::env::temp_dir().join(format!("hench-cli-browser-{}-{nanos}", std::process::id()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn binary_detection_order_is_pinned() {
        // The ORDER is the contract: env override first, then these PATH names.
        assert_eq!(
            BINARY_CANDIDATES,
            &["google-chrome-stable", "google-chrome", "chromium", "chromium-browser"]
        );
    }

    #[test]
    fn resolve_binary_finds_absolute_path() {
        // /bin/sh exists everywhere; proves the absolute-path branch works.
        let p = resolve_binary("/bin/sh").expect("/bin/sh should resolve");
        assert!(p.is_absolute());
    }

    #[test]
    fn root_parser_reads_first_uid_field() {
        // Synthetic /proc/self/status: first field of the Uid line is the real
        // uid — must parse 0 from a root-like line.
        let status = "Name:\tsh\nUid:\t0\t0\t0\t0\nGid:\t0\t0\t0\t0\n";
        let uid = status
            .lines()
            .find(|l| l.starts_with("Uid:"))
            .and_then(|l| l.split_whitespace().nth(1))
            .and_then(|u| u.parse::<u32>().ok());
        assert_eq!(uid, Some(0));
    }

    #[test]
    fn selector_probe_quotes_selector() {
        let probe = selector_probe("div[data-x='1']");
        assert!(probe.contains("querySelector"));
        assert!(probe.contains("'found'"));
    }

    #[test]
    fn text_script_has_innertext_fallback() {
        assert!(TEXT_SCRIPT.contains("innerText"));
        assert!(TEXT_SCRIPT.contains("textContent"));
    }

    #[tokio::test]
    #[ignore = "needs a real Chrome; run explicitly"]
    async fn live_render_example_com() {
        let browser = Browser::launch(Some("/usr/bin/google-chrome"))
            .await
            .expect("launch chrome");
        let page = browser
            .load("https://example.com", 30_000, None, false, None)
            .await
            .expect("render example.com");
        assert!(
            page.title.contains("Example"),
            "title was {:?}",
            page.title
        );
        assert!(!page.text.trim().is_empty(), "text empty");
    }

    #[tokio::test]
    #[ignore = "needs a real Chrome; run explicitly"]
    async fn live_screenshot_png() {
        let browser = Browser::launch(Some("/usr/bin/google-chrome"))
            .await
            .expect("launch chrome");
        let (_page, png) = browser
            .screenshot("https://example.com", 800, 600, 30_000)
            .await
            .expect("screenshot example.com");
        assert!(
            png.len() > 8 && &png[..8] == b"\x89PNG\r\n\x1a\n",
            "not a PNG ({} bytes)",
            png.len()
        );
    }
}
