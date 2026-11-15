//! The `site_scrape` tool: crawl a website breadth-first in the real browser
//! and deliver every page's title + text as one JSONL file on Blossom. Port of
//! the Android SiteScrapeTool: billed up front per requested page, crawl stays
//! on the host the start URL lands on, minus fragments and obvious non-page
//! files. Seed discovery (robots.txt + sitemaps) runs in parallel with the
//! first renders and fails open.
use crate::browser::Browser;
use crate::config::Config;
use crate::sitemap;
use crate::state::EXECUTION_BUDGET_SECS;
use crate::store;
use crate::tools::{
    mcp_error, mcp_success, tool_error, ProgressSink, QuoteDecision, Tool, ToolCtx,
};
use contextvm_sdk::core::types::JsonRpcMessage;
use contextvm_sdk::payments::types::PricedCapability;
use futures::StreamExt;
use serde_json::{json, Value};
use std::collections::{HashSet, VecDeque};
use std::io::Read;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// A page costs what a single web_fetch render does (Android WebFetchTool.PRICE_SATS).
pub const PRICE_PER_PAGE_SATS: i64 = 3;
/// 50 pages took 30-37 s on test phones (3 renders at a time), far inside the
/// crawl budget.
pub const MAX_PAGES: i64 = 50;
/// Renders in flight; each load() gets its own isolated browser context, so
/// this also caps memory (mirrors Android's shared-renderer-process cap).
const CONCURRENCY: usize = 3;
/// Android CRAWL_BUDGET_MS = 420s inside CvmServer's 600s call budget; the CLI
/// keeps the same headroom math off EXECUTION_BUDGET_SECS.
const CRAWL_BUDGET_CAP_SECS: u64 = 420;
const UPLOAD_HEADROOM_SECS: u64 = 60;
const PAGE_TIMEOUT_MS: u64 = 30_000;
const MAX_TEXT_CHARS: usize = 100_000;
const MIME: &str = "application/x-ndjson";

/// Seed discovery is best-effort: robots.txt, then sitemaps, all fail-open.
const SEED_TIMEOUT: Duration = Duration::from_secs(15);
const MAX_SEED_SITEMAPS: usize = 3;
const MAX_SEED_BYTES: usize = 512 * 1024;

/// Short-timeout client for seed fetches; redirects off so a sitemap can't
/// bounce off-host. Mirrors Android's SEED_HTTP OkHttpClient.
fn seed_client() -> reqwest::Client {
    reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(5))
        .timeout(Duration::from_secs(10))
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap_or_else(|_| reqwest::Client::new())
}


pub struct SiteScrape;

#[async_trait::async_trait]
impl Tool for SiteScrape {
    fn name(&self) -> &'static str {
        "site_scrape"
    }

    fn descriptor(&self) -> Value {
        json!({
            "name": "site_scrape",
            "description": "Crawl a website in a real mobile browser (full JS, unauthenticated session): follows same-host links breadth-first and delivers every page's title and text as a hosted JSONL file. Billed per page of max_pages.",
            "inputSchema": {
                "type": "object",
                "required": ["url"],
                "properties": {
                    "url": { "type": "string", "description": "http(s) start URL; the crawl stays on the host it lands on" },
                    "max_pages": { "type": "integer", "description": format!("pages to crawl, 1-{MAX_PAGES} (default {MAX_PAGES}), billed up front at {PRICE_PER_PAGE_SATS} sats each") }
                }
            }
        })
    }

    fn priced_capability(&self, _cfg: &Config) -> Option<PricedCapability> {
        Some(PricedCapability {
            method: "tools/call".into(),
            name: Some("site_scrape".into()),
            amount: PRICE_PER_PAGE_SATS,
            max_amount: Some(PRICE_PER_PAGE_SATS * MAX_PAGES),
            currency_unit: "sats".into(),
            description: Some("Hench site scrape".into()),
        })
    }

    async fn quote(&self, _cfg: &Config, args: &Value) -> QuoteDecision {
        let Some(url) = normalize_url(args.get("url").and_then(Value::as_str).unwrap_or(""))
        else {
            return QuoteDecision::Reject {
                message: "url must be an http(s) URL".into(),
            };
        };
        if let Err(reason) = crate::url_policy::check_async(&url).await {
            return QuoteDecision::Reject {
                message: format!("url rejected by worker safety policy: {reason}"),
            };
        }
        QuoteDecision::Quote {
            amount: PRICE_PER_PAGE_SATS * max_pages(args),
            description: "hench site_scrape".into(),
            url,
            mode: "site_scrape".into(),
        }
    }

    async fn run(&self, ctx: ToolCtx<'_>) -> JsonRpcMessage {
        let canonical = ctx.canonical.as_str();
        let Some(start) = normalize_url(ctx.args.get("url").and_then(Value::as_str).unwrap_or(""))
        else {
            return mcp_error(&ctx.id, -32602, "url must be an http(s) URL");
        };
        let max_pages = max_pages(&ctx.args) as usize;
        if !ctx.cfg.hours.open_now() {
            return tool_error(
                &ctx.id,
                "CLOSED",
                "worker is outside its hours of operation",
            );
        }
        if let Err(reason) = crate::url_policy::check_async(&start).await {
            return tool_error(&ctx.id, "UNSUPPORTED_URL", &reason);
        }
        if !ctx.store.inflight_add(&ctx.canonical) {
            return tool_error(
                &ctx.id,
                "BUSY",
                "execution already in progress for this request",
            );
        }
        // Free mode never ran the quoter, so no WAITING record exists yet:
        // create it here (amount 0) so terminal results still cache/replay
        // and the dashboard sees the job — same contract as download_media.
        if ctx.store.get(&ctx.canonical).is_none() {
            if let Err(e) = ctx.store.mark_waiting(store::JobRecord {
                canonical_id: ctx.canonical.clone(),
                status: store::WAITING.into(),
                amount_msat: 0,
                url: start.clone(),
                mode: "site_scrape".into(),
                client: ctx.client_pubkey.clone(),
                result: None,
                tool_state: None,
                updated_at: 0,
            }) {
                ctx.store.inflight_remove(&ctx.canonical);
                return tool_error(
                    &ctx.id,
                    "INTERNAL",
                    &format!("job store unavailable: {e:#}"),
                );
            }
        }

        (ctx.progress)("CRAWLING").await;

        // One shared browser for the whole crawl; every load() gets its own
        // isolated context — the engine's per-call isolation is the desktop
        // equivalent of Android's fresh unauthenticated profile per render.
        let browser = match Browser::launch(None).await {
            Ok(b) => Some(Arc::new(b)),
            Err(e) => {
                tracing::warn!("site_scrape: browser launch failed: {e:#}");
                None
            }
        };

        let budget_secs = EXECUTION_BUDGET_SECS
            .saturating_sub(UPLOAD_HEADROOM_SECS)
            .min(CRAWL_BUDGET_CAP_SECS);
        let deadline = Instant::now() + Duration::from_secs(budget_secs);
        let stats = crawl(
            &start,
            max_pages,
            deadline,
            browser.clone(),
            seed_client(),
            ctx.progress,
        )
        .await;

        if stats.scraped == 0 {
            let msg = format!(
                "no page could be loaded: {}",
                stats.first_error.as_deref().unwrap_or("crawl produced no pages")
            );
            if let Err(p) = ctx.store.mark_terminal(
                &ctx.canonical,
                store::FAILED,
                Some(store::error_result("DOWNLOAD", &msg)),
            ) {
                tracing::error!("terminal record NOT persisted for {canonical} — replay may re-invoice: {p:#}");
            }
            ctx.store.inflight_remove(&ctx.canonical);
            return tool_error(&ctx.id, "DOWNLOAD", &msg);
        }

        (ctx.progress)("UPLOADING").await;
        let upload = match crate::media::upload_bytes(&ctx, stats.jsonl, MIME, "jsonl").await {
            Ok(u) => u,
            Err(e) => {
                let msg = format!("{e:#}");
                if let Err(p) = ctx.store.mark_terminal(
                    &ctx.canonical,
                    store::FAILED,
                    Some(store::error_result("UPLOAD", &msg)),
                ) {
                    tracing::error!("terminal record NOT persisted for {canonical} — replay may re-invoice: {p:#}");
                }
                ctx.store.inflight_remove(&ctx.canonical);
                return tool_error(&ctx.id, "UPLOAD", &msg);
            }
        };

        let short_hash: String = upload.sha256.chars().take(16).collect();
        let result = json!({
            "content": [
                { "type": "text", "text": json!({
                    "url": start,
                    "pages": stats.scraped,
                    "failed": stats.failed,
                    "complete": stats.complete,
                    "url_result": upload.url,
                    "sha256": upload.sha256,
                    "sizeBytes": upload.size,
                    "mimeType": MIME,
                }).to_string() },
                { "type": "resource_link", "uri": upload.url, "name": format!("{short_hash}.jsonl"), "mimeType": MIME, "description": "Crawled by hench site_scrape" }
            ],
            "isError": false
        });
        if let Err(e) =
            ctx.store
                .mark_terminal(&ctx.canonical, store::COMPLETED, Some(result.clone()))
        {
            tracing::error!(
                "COMPLETED record NOT persisted for {canonical} — a replay may re-invoice: {e:#}"
            );
        }
        ctx.store.inflight_remove(&ctx.canonical);
        tracing::info!(
            "site_scrape done: {start} ({}/{} pages) -> {}",
            stats.scraped,
            stats.scraped + stats.failed,
            upload.url
        );
        mcp_success(&ctx.id, result)
    }
}

/// Pages attempted vs delivered, plus the NDJSON payload.
struct CrawlStats {
    scraped: usize,
    failed: usize,
    complete: bool,
    first_error: Option<String>,
    jsonl: Vec<u8>,
}

/// Breadth-first from `start` with up to CONCURRENCY renders in flight; writes
/// one JSONL line per page attempted, in completion order. Seed discovery
/// (robots.txt + sitemaps) runs concurrently and merges once the landing host
/// is pinned by the first successful render — same contract as Android's crawl.
async fn crawl<'a>(
    start: &str,
    max_pages: usize,
    deadline: Instant,
    browser: Option<Arc<Browser>>,
    seed_http: reqwest::Client,
    on_progress: ProgressSink<'a>,
) -> CrawlStats {
    let (tx, mut rx) =
        tokio::sync::mpsc::unbounded_channel::<(String, Result<crate::browser::Page, String>)>();

    // Sitemap/robots seeds resolve in parallel with the first renders.
    let start_owned = start.to_string();
    let mut seeds = Some(tokio::spawn(async move {
        fetch_seeds(&start_owned, &seed_http).await
    }));

    let mut queue: VecDeque<String> = VecDeque::from([start.to_string()]);
    let mut seen: HashSet<String> = HashSet::from([start.to_string()]);
    let mut host: Option<String> = None;
    let mut started = 0usize;
    let mut in_flight = 0usize;
    let mut scraped = 0usize;
    let mut failed = 0usize;
    let mut first_error: Option<String> = None;
    let mut cut_short = false;
    let mut jsonl: Vec<u8> = Vec::new();

    loop {
        // Fill free render slots while pages remain and budget allows.
        while in_flight < CONCURRENCY && !queue.is_empty() && started < max_pages {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                cut_short = true;
                break;
            }
            let next = queue.pop_front().unwrap();
            started += 1;
            in_flight += 1;
            let tx = tx.clone();
            let browser = browser.clone();
            let timeout_ms = remaining.as_millis().min(PAGE_TIMEOUT_MS as u128) as u64;
            tokio::spawn(async move {
                let res = render_page(browser.as_deref(), &next, timeout_ms).await;
                let _ = tx.send((next, res));
            });
        }
        if started >= max_pages && !queue.is_empty() {
            cut_short = true;
        }
        if let Some(pinned) = host.clone() {
            // Merge seeds once the landing host is known: non-blocking while
            // renders are in flight; awaited once when the crawl would end.
            // `take` makes the await happen at most once.
            let ready = seeds
                .as_ref()
                .is_some_and(|s| s.is_finished())
                || (in_flight == 0 && queue.is_empty());
            if ready {
                if let Some(seeds) = seeds.take() {
                    if let Ok(seed_urls) = seeds.await {
                        for url in filter_seeds(seed_urls.iter().map(String::as_str), &pinned) {
                            if seen.insert(url.clone()) {
                                queue.push_back(url);
                            }
                        }
                    }
                }
            }
        }
        if in_flight == 0 {
            break;
        }
        let Some((url, res)) = rx.recv().await else { break };
        in_flight -= 1;
        match res {
            Err(e) => {
                failed += 1;
                if first_error.is_none() {
                    first_error = Some(e.clone());
                }
                push_line(&mut jsonl, &json!({ "url": url, "error": e }));
            }
            Ok(page) => {
                scraped += 1;
                // Scope is where the start URL landed; marking the landing URL
                // seen stops its home link re-crawling it.
                let landed = normalize_url(&page.final_url).unwrap_or_else(|| url.clone());
                if host.is_none() {
                    host = host_of(&landed);
                }
                seen.insert(landed);
                for href in &page.links {
                    let Some(link) = normalize_url(href) else { continue };
                    let link_host = host_of(&link).unwrap_or_default();
                    let is_page = url::Url::parse(&link)
                        .map(|u| !sitemap::is_non_page(u.path()))
                        .unwrap_or(false);
                    if host.as_deref() == Some(link_host.as_str())
                        && is_page
                        && seen.insert(link.clone())
                    {
                        queue.push_back(link);
                    }
                }
                let truncated = page.text.chars().count() > MAX_TEXT_CHARS;
                let text: String = page.text.chars().take(MAX_TEXT_CHARS).collect();
                let mut line = json!({
                    "url": page.final_url,
                    "title": page.title,
                    "text": text,
                });
                if truncated {
                    line["truncated"] = json!(true);
                }
                push_line(&mut jsonl, &line);
            }
        }
        (on_progress)(&format!("page {}/{}", scraped + failed, max_pages)).await;
    }
    // Seeds merge only after the landing host is pinned; if nothing ever
    // landed (start page failed and no page succeeded) they stay unused —
    // same as Android.
    CrawlStats {
        scraped,
        failed,
        complete: !cut_short && queue.is_empty(),
        first_error,
        jsonl,
    }
}

/// One page render; a failure becomes that page's error line instead of
/// ending the crawl. `browser == None` means launch failed — every queued
/// page reports the same error, like Android's per-page render failures.
async fn render_page(
    browser: Option<&Browser>,
    url: &str,
    timeout_ms: u64,
) -> Result<crate::browser::Page, String> {
    let Some(browser) = browser else {
        return Err("browser unavailable".into());
    };
    // Defense in depth: the engine does not policy-check loads itself.
    crate::url_policy::check_async(url).await?;
    browser
        .load(url, timeout_ms, None, true, None)
        .await
        .map_err(|e| e.to_string())
}

/// http(s) URL minus its fragment and any default port; None for
/// mailto:/javascript:/userinfo/junk — the Rust port of Android `normalize`.
fn normalize_url(raw: &str) -> Option<String> {
    let mut u = url::Url::parse(raw.trim()).ok()?;
    if u.scheme() != "http" && u.scheme() != "https" {
        return None;
    }
    if !u.username().is_empty() || u.password().is_some() {
        return None;
    }
    let default = if u.scheme() == "https" { 443 } else { 80 };
    if u.port() == Some(default) {
        let _ = u.set_port(None);
    }
    u.set_fragment(None);
    Some(u.to_string())
}

fn host_of(url: &str) -> Option<String> {
    url::Url::parse(url)
        .ok()?
        .host_str()
        .map(str::to_lowercase)
}

/// Filter raw seed URLs down to same-host crawl candidates: normalize, drop
/// non-page extensions, dedupe. Pure — the network half is fetch_seeds.
fn filter_seeds<'a>(seeds: impl Iterator<Item = &'a str>, pinned_host: &str) -> Vec<String> {
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for raw in seeds {
        let Some(url) = normalize_url(raw) else { continue };
        if host_of(&url).as_deref() != Some(pinned_host) {
            continue;
        }
        let is_page = url::Url::parse(&url)
            .map(|u| !sitemap::is_non_page(u.path()))
            .unwrap_or(false);
        if !is_page {
            continue;
        }
        if seen.insert(url.clone()) {
            out.push(url);
            if out.len() >= MAX_PAGES as usize {
                break;
            }
        }
    }
    out
}

/// robots.txt-declared + /sitemap.xml page URLs on the start host, as crawl
/// seeds. Every failure (network, parse, policy) just shrinks the result;
/// this never throws and never leaves the start host. Mirrors Android
/// defaultFetchSeeds.
async fn fetch_seeds(start: &str, client: &reqwest::Client) -> Vec<String> {
    let Ok(start_url) = url::Url::parse(start) else {
        return Vec::new();
    };
    let Some(start_host) = start_url.host_str().map(str::to_lowercase) else {
        return Vec::new();
    };
    let work = async {
        let mut out: Vec<String> = Vec::new();
        let mut seen_out: HashSet<String> = HashSet::new();
        let mut queue: VecDeque<String> = VecDeque::new();
        let mut seen_sitemaps: HashSet<String> = HashSet::new();

        // robots.txt on the start host; untrusted — every sitemap URL it
        // lists is scheme/host/policy checked before use.
        let mut robots = start_url.clone();
        robots.set_path("/robots.txt");
        robots.set_query(None);
        robots.set_fragment(None);
        if let Some(body) = fetch_text(client, robots.as_str()).await {
            for s in sitemap::parse_robots_sitemaps(&body) {
                enqueue(&mut queue, &mut seen_sitemaps, &s, &start_host).await;
            }
        }
        if queue.is_empty() {
            let mut fallback = start_url.clone();
            fallback.set_path("/sitemap.xml");
            fallback.set_query(None);
            fallback.set_fragment(None);
            enqueue(&mut queue, &mut seen_sitemaps, fallback.as_str(), &start_host).await;
        }
        while !queue.is_empty() && seen_sitemaps.len() < MAX_SEED_SITEMAPS {
            let Some(sitemap_url) = queue.pop_front() else { break };
            let Some(body) = fetch_text(client, &sitemap_url).await else {
                continue;
            };
            let parsed = sitemap::parse_sitemap(&body);
            for page in &parsed.pages {
                if let Some(u) = normalize_url(page) {
                    let is_page = url::Url::parse(&u)
                        .map(|x| !sitemap::is_non_page(x.path()))
                        .unwrap_or(false);
                    if host_of(&u).as_deref() == Some(start_host.as_str())
                        && is_page
                        && seen_out.insert(u.clone())
                    {
                        out.push(u);
                    }
                }
            }
            for child in &parsed.children {
                enqueue(&mut queue, &mut seen_sitemaps, child, &start_host).await;
            }
        }
        out.truncate(MAX_PAGES as usize);
        out
    };
    match tokio::time::timeout(SEED_TIMEOUT, work).await {
        Ok(v) => v,
        Err(_) => Vec::new(),
    }
}

/// Queue a sitemap URL while under the 3-sitemap budget. `seen` counts every
/// sitemap URL ever enqueued (including ones already fetched), so at most
/// MAX_SEED_SITEMAPS bodies are downloaded — mirrors Android's
/// `queue.size + seenSitemaps.size < MAX`. Only same-host http(s) URLs that
/// pass the worker URL policy are queued (Android `allowedSitemap`).
async fn enqueue(queue: &mut VecDeque<String>, seen: &mut HashSet<String>, url: &str, host: &str) {
    let Some(u) = normalize_url(url) else { return };
    if host_of(&u).as_deref() != Some(host) || !policy_ok(&u).await {
        return;
    }
    if seen.len() < MAX_SEED_SITEMAPS && seen.insert(u.clone()) {
        queue.push_back(u);
    }
}

/// Body of a seed fetch, or None on any failure; capped at MAX_SEED_BYTES and
/// gunzipped (still capped) when the magic bytes say gzip.
async fn fetch_text(client: &reqwest::Client, url: &str) -> Option<String> {
    let resp = client.get(url).send().await.ok()?;
    if !resp.status().is_success() {
        return None;
    }
    let bytes = collect_capped(resp, MAX_SEED_BYTES).await?;
    let text = if bytes.len() >= 2 && bytes[0] == 0x1f && bytes[1] == 0x8b {
        let mut out = Vec::new();
        flate2::read::GzDecoder::new(&bytes[..])
            .take(MAX_SEED_BYTES as u64)
            .read_to_end(&mut out)
            .ok()?;
        out
    } else {
        bytes
    };
    String::from_utf8(text).ok()
}

/// First `cap` bytes of a response body; None on stream error.
async fn collect_capped(resp: reqwest::Response, cap: usize) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    let mut stream = resp.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.ok()?;
        let room = cap.saturating_sub(out.len());
        if room == 0 {
            break;
        }
        let take = room.min(chunk.len());
        out.extend_from_slice(&chunk[..take]);
        if take < chunk.len() {
            break;
        }
    }
    Some(out)
}

/// url_policy check for an untrusted seed URL; failures just drop the seed.
async fn policy_ok(url: &str) -> bool {
    crate::url_policy::check_async(url).await.is_ok()
}

fn push_line(jsonl: &mut Vec<u8>, line: &Value) {
    serde_json::to_writer(&mut *jsonl, line).ok();
    jsonl.push(b'\n');
}

/// Android `maxPages`: absent/unparseable -> MAX_PAGES, else clamp 1..=50.
fn max_pages(args: &Value) -> i64 {
    args.get("max_pages")
        .and_then(Value::as_i64)
        .unwrap_or(MAX_PAGES)
        .clamp(1, MAX_PAGES)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quote_math_scales_with_max_pages() {
        // amount = 3 sats/page * clamped max_pages
        assert_eq!(max_pages(&json!({})), 50);
        assert_eq!(max_pages(&json!({"max_pages": 7})), 7);
        assert_eq!(max_pages(&json!({"max_pages": 0})), 1); // clamp, not default
        assert_eq!(max_pages(&json!({"max_pages": 999})), 50);
        assert_eq!(max_pages(&json!({"max_pages": "x"})), 50);
        assert_eq!(PRICE_PER_PAGE_SATS * max_pages(&json!({"max_pages": 10})), 30);
        assert_eq!(PRICE_PER_PAGE_SATS * max_pages(&json!({})), 150);
    }

    #[test]
    fn normalize_strips_fragment_and_default_port() {
        assert_eq!(
            normalize_url("https://Ex.com:443/a/b?x=1#frag"),
            Some("https://ex.com/a/b?x=1".into())
        );
        assert_eq!(
            normalize_url("http://ex.com:80/p#f"),
            Some("http://ex.com/p".into())
        );
        assert_eq!(normalize_url("mailto:a@b.c"), None);
        assert_eq!(normalize_url("https://u:p@ex.com/"), None);
        assert_eq!(normalize_url("javascript:alert(1)"), None);
        assert_eq!(normalize_url("not a url"), None);
        // non-default port kept
        assert_eq!(
            normalize_url("https://ex.com:8443/x"),
            Some("https://ex.com:8443/x".into())
        );
    }

    #[test]
    fn filter_seeds_host_ext_dedupe_cap() {
        let seeds = vec![
            "https://ex.com/a",
            "https://ex.com/a#dup",   // dedupes after fragment strip
            "https://ex.com/b.pdf",  // non-page ext dropped
            "https://other.com/c",   // wrong host
            "http://ex.com:80/d",    // normalizes to ex.com/d
            "ftp://ex.com/e",        // bad scheme
            "https://ex.com/f",
            "https://ex.com/g",
            "https://ex.com/h",
        ];
        let out = filter_seeds(seeds.iter().copied(), "ex.com");
        // Android parity: no seed-count cap here (the crawl's max_pages and
        // deadline bound the work); every normalized same-host page URL is a
        // seed, deduped.
        assert_eq!(
            out,
            vec![
                "https://ex.com/a",
                "http://ex.com/d",
                "https://ex.com/f",
                "https://ex.com/g",
                "https://ex.com/h"
            ]
        );
    }

    #[test]
    fn gzipped_body_decodes_capped() {
        // gzip("hello sitemap") fixture, generated offline.
        let gz = [
            0x1f, 0x8b, 0x08, 0x00, 0x00, 0x00, 0x00, 0x00, 0x02, 0xff, 0xcb, 0x48, 0xcd, 0xc9,
            0xc9, 0x57, 0x28, 0xce, 0x2c, 0x49, 0xcd, 0x4d, 0x2c, 0x00, 0x00, 0x38, 0x3e, 0xf1,
            0x9e, 0x0d, 0x00, 0x00, 0x00,
        ];
        let mut out = Vec::new();
        flate2::read::GzDecoder::new(&gz[..])
            .take(MAX_SEED_BYTES as u64)
            .read_to_end(&mut out)
            .unwrap();
        assert_eq!(String::from_utf8(out).unwrap(), "hello sitemap");
    }
}
