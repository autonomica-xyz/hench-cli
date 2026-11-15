//! The `page_screenshot` tool: render a URL in a real (headless Chromium)
//! browser and return the PNG hosted on Blossom — the CLI port of the
//! app's PageScreenshotTool. Flat price; URL safety and the render/upload
//! mechanics live in [`crate::url_policy`], [`crate::browser`], and
//! [`crate::media`].
use crate::config::Config;
use crate::store;
use crate::tools::{mcp_success, tool_error, QuoteDecision, Tool, ToolCtx};
use contextvm_sdk::core::types::JsonRpcMessage;
use contextvm_sdk::payments::types::PricedCapability;
use serde_json::{json, Value};

pub struct PageScreenshot;

/// Flat price, sats — Android PageScreenshotTool.PRICE_SATS is 5; the CLI
/// keeps parity (a render is heavier than a plain fetch).
const PRICE_SATS: i64 = 5;
const MODE: &str = "page_screenshot";

#[async_trait::async_trait]
impl Tool for PageScreenshot {
    fn name(&self) -> &'static str {
        "page_screenshot"
    }

    fn descriptor(&self) -> Value {
        json!({
            "name": "page_screenshot",
            "description": "Render a URL in a real browser (full JS, unauthenticated session) and return a screenshot PNG hosted on Blossom. Viewport-sized.",
            "inputSchema": {
                "type": "object",
                "required": ["url"],
                "properties": {
                    "url": { "type": "string", "description": "http(s) URL to render" },
                    "width": { "type": "integer", "description": "viewport width (default 1280, range 320-1920)" },
                    "height": { "type": "integer", "description": "viewport height (default 800, range 240-1080)" },
                    "timeout_secs": { "type": "integer", "description": "load timeout in seconds (default 45, range 5-120)" }
                }
            }
        })
    }

    fn priced_capability(&self, _cfg: &Config) -> Option<PricedCapability> {
        Some(PricedCapability {
            method: "tools/call".into(),
            name: Some("page_screenshot".into()),
            amount: PRICE_SATS,
            max_amount: Some(PRICE_SATS),
            currency_unit: "sats".into(),
            description: Some("Hench page screenshot".into()),
        })
    }

    async fn quote(&self, _cfg: &Config, args: &Value) -> QuoteDecision {
        let url = args.get("url").and_then(|u| u.as_str()).unwrap_or("");
        if url.is_empty() {
            return QuoteDecision::Reject {
                message: "missing url".into(),
            };
        }
        if let Err(reason) = crate::url_policy::check(url) {
            return QuoteDecision::Reject {
                message: format!("url rejected by worker safety policy: {reason}"),
            };
        }
        QuoteDecision::Quote {
            amount: PRICE_SATS,
            description: format!("hench {MODE}"),
            url: url.to_string(),
            mode: MODE.into(),
        }
    }

    async fn run(&self, ctx: ToolCtx<'_>) -> JsonRpcMessage {
        let cfg = &ctx.cfg;
        let url = ctx
            .args
            .get("url")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let width = parse_usize(ctx.args.get("width"))
            .map(|w| clamp_dim(w, 320, 1920))
            .unwrap_or(1280);
        let height = parse_usize(ctx.args.get("height"))
            .map(|h| clamp_dim(h, 240, 1080))
            .unwrap_or(800);
        let timeout_secs = parse_usize(ctx.args.get("timeout_secs"))
            .map(|t| clamp_dim(t, 5, 120))
            .unwrap_or(45);
        if url.is_empty() {
            return tool_error(&ctx.id, "INVALID_PARAMS", "missing url");
        }
        if let Err(reason) = crate::url_policy::check(&url) {
            return tool_error(&ctx.id, "UNSUPPORTED_URL", &reason);
        }
        if !cfg.hours.open_now() {
            return tool_error(
                &ctx.id,
                "CLOSED",
                "worker is outside its hours of operation",
            );
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
                url: url.clone(),
                mode: MODE.into(),
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

        (ctx.progress)("RENDERING").await;
        let browser = match crate::browser::Browser::launch(None).await {
            Ok(b) => b,
            Err(e) => {
                return fail_terminal(&ctx, "DOWNLOAD", &format!("browser launch failed: {e:#}"));
            }
        };
        let (page, png) = match browser
            .screenshot(&url, width as u32, height as u32, (timeout_secs as u64) * 1_000)
            .await
        {
            Ok(r) => r,
            Err(crate::browser::RenderError(msg)) => {
                let (code, message) = if is_timeout(&msg) {
                    (
                        "TIMEOUT",
                        format!("page did not finish loading within {timeout_secs}s"),
                    )
                } else {
                    ("DOWNLOAD", msg)
                };
                return fail_terminal(&ctx, code, &message);
            }
        };

        (ctx.progress)("UPLOADING").await;
        let uploaded = match crate::media::upload_bytes(&ctx, png, "image/png", "png").await {
            Ok(u) => u,
            Err(e) => return fail_terminal(&ctx, "UPLOAD", &format!("{e:#}")),
        };

        // Android parity: the text body carries the page metadata plus the
        // hosted artifact; the resource_link points clients at the PNG. The
        // Android tool emits no inline image block (ContentBlock has only
        // Text/ResourceLink), so neither does this port.
        let body = json!({
            "url": url,
            "finalUrl": page.final_url,
            "title": page.title,
            "width": width,
            "height": height,
            "image_url": uploaded.url,
            "sha256": uploaded.sha256,
            "sizeBytes": uploaded.size,
            "mimeType": "image/png"
        });
        let link_name = format!(
            "{}.png",
            uploaded.sha256.chars().take(16).collect::<String>()
        );
        let result = json!({
            "content": [
                { "type": "text", "text": body.to_string() },
                { "type": "resource_link", "uri": uploaded.url, "name": link_name, "mimeType": "image/png", "description": "Rendered by hench page_screenshot" }
            ],
            "isError": false
        });
        if let Err(e) =
            ctx.store
                .mark_terminal(&ctx.canonical, store::COMPLETED, Some(result.clone()))
        {
            tracing::error!(
                "COMPLETED record NOT persisted for {canonical} — a replay may re-invoice: {e:#}",
                canonical = ctx.canonical
            );
        }
        ctx.store.inflight_remove(&ctx.canonical);
        tracing::info!(
            "job done: {url} ({} bytes) -> {}",
            uploaded.size,
            uploaded.url
        );
        mcp_success(&ctx.id, result)
    }
}

/// Persist the terminal FAILED record (a WAITING record always exists by the
/// time this runs — free mode creates its own above) and answer the client.
fn fail_terminal(ctx: &ToolCtx<'_>, code: &str, message: &str) -> JsonRpcMessage {
    if let Err(p) = ctx.store.mark_terminal(
        &ctx.canonical,
        store::FAILED,
        Some(store::error_result(code, message)),
    ) {
        tracing::error!("terminal record NOT persisted for {canonical} — replay may re-invoice: {p:#}", canonical = ctx.canonical);
    }
    ctx.store.inflight_remove(&ctx.canonical);
    tool_error(&ctx.id, code, message)
}

fn is_timeout(msg: &str) -> bool {
    let m = msg.to_lowercase();
    m.contains("timed out") || m.contains("timeout")
}

fn parse_usize(v: Option<&Value>) -> Option<usize> {
    v.and_then(|v| {
        v.as_str()
            .and_then(|s| s.parse::<usize>().ok())
            .or_else(|| v.as_u64().map(|n| n as usize))
    })
}

fn clamp_dim(v: usize, min: usize, max: usize) -> usize {
    v.clamp(min, max)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn descriptor_is_valid_and_requires_url() {
        let desc = PageScreenshot.descriptor();
        assert_eq!(desc["name"], "page_screenshot");
        let schema = &desc["inputSchema"];
        assert_eq!(schema["type"], "object");
        assert_eq!(schema["required"], json!(["url"]));
        for key in ["url", "width", "height", "timeout_secs"] {
            assert!(schema["properties"][key].is_object(), "missing {key}");
        }
    }

    #[tokio::test]
    async fn quote_rejects_missing_url() {
        match PageScreenshot.quote(&Config::default(), &json!({})).await {
            QuoteDecision::Reject { message } => assert_eq!(message, "missing url"),
            other => panic!("expected reject, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn quote_rejects_private_literal_ip() {
        match PageScreenshot
            .quote(&Config::default(), &json!({"url": "http://192.168.1.1/"}))
            .await
        {
            QuoteDecision::Reject { message } => {
                assert!(message.contains("safety policy"), "{message}")
            }
            other => panic!("expected reject, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn quote_accepts_public_literal_ip() {
        match PageScreenshot
            .quote(&Config::default(), &json!({"url": "http://93.184.216.34/"}))
            .await
        {
            QuoteDecision::Quote {
                amount,
                url,
                mode,
                ..
            } => {
                assert_eq!(amount, PRICE_SATS);
                assert_eq!(url, "http://93.184.216.34/");
                assert_eq!(mode, "page_screenshot");
            }
            other => panic!("expected quote, got {other:?}"),
        }
    }

    #[test]
    fn clamp_dim_bounds() {
        assert_eq!(clamp_dim(100, 320, 1920), 320);
        assert_eq!(clamp_dim(5000, 320, 1920), 1920);
        assert_eq!(clamp_dim(1280, 320, 1920), 1280);
        assert_eq!(clamp_dim(1, 5, 120), 5);
        assert_eq!(clamp_dim(999, 5, 120), 120);
    }

    #[test]
    fn parse_usize_handles_numbers_and_strings() {
        assert_eq!(parse_usize(Some(&json!(800))), Some(800));
        assert_eq!(parse_usize(Some(&json!("1024"))), Some(1024));
        assert_eq!(parse_usize(Some(&json!("wide"))), None);
        assert_eq!(parse_usize(Some(&json!(-5))), None);
        assert_eq!(parse_usize(Some(&json!(1.5))), None);
        assert_eq!(parse_usize(None), None);
    }
}
