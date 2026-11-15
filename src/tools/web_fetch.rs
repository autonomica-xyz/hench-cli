//! The `web_fetch` tool: load a URL in a real desktop browser and return the
//! page as title + text for agent consumption — the CLI port of the Android
//! app's WebFetchTool. Renders run in a fresh unauthenticated browser context
//! per call (the desktop equivalent of the app's isolated WebView profile).
//!
//! v1 launches a fresh `Browser` per call: simple and isolation-correct,
//! at the cost of a Chromium startup per fetch. Upgrade path: a lazily
//! shared `Browser` behind a mutex in `crate::browser` if launch cost hurts.
use crate::config::Config;
use crate::store;
use crate::tools::{mcp_error, mcp_success, tool_error, QuoteDecision, Tool, ToolCtx};
use contextvm_sdk::core::types::JsonRpcMessage;
use contextvm_sdk::payments::types::PricedCapability;
use serde_json::{json, Value};

pub struct WebFetch;

const PRICE_SATS: i64 = 3;
const DEFAULT_TIMEOUT_SECS: u64 = 45;
const MIN_TIMEOUT_SECS: u64 = 5;
const MAX_TIMEOUT_SECS: u64 = 120;
/// CEP-8 message-size sanity: page text is capped, truncation flagged.
const MAX_TEXT_CHARS: usize = 100_000;

#[async_trait::async_trait]
impl Tool for WebFetch {
    fn name(&self) -> &'static str {
        "web_fetch"
    }

    fn descriptor(&self) -> Value {
        json!({
            "name": "web_fetch",
            "description": "Fetch a URL in a real desktop browser (Chromium, unauthenticated session) and return the page text for an agent. Handles bot-checks better than datacenter fetchers.",
            "inputSchema": {
                "type": "object",
                "required": ["url"],
                "properties": {
                    "url": { "type": "string", "description": "http(s) URL to fetch" },
                    "wait_for": { "type": "string", "description": "optional CSS selector to wait for before reading the page" },
                    "timeout_secs": { "type": "integer", "description": "optional load timeout in seconds, 5-120 (default 45)" }
                }
            }
        })
    }

    fn priced_capability(&self, _cfg: &Config) -> Option<PricedCapability> {
        Some(PricedCapability {
            method: "tools/call".into(),
            name: Some("web_fetch".into()),
            amount: PRICE_SATS,
            max_amount: Some(PRICE_SATS),
            currency_unit: "sats".into(),
            description: Some("Hench web fetch".into()),
        })
    }

    async fn quote(&self, _cfg: &Config, args: &Value) -> QuoteDecision {
        let url = args.get("url").and_then(|u| u.as_str()).unwrap_or("");
        if url.trim().is_empty() {
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
            description: "hench web_fetch".into(),
            url: url.to_string(),
            mode: "web_fetch".into(),
        }
    }

    async fn run(&self, ctx: ToolCtx<'_>) -> JsonRpcMessage {
        let url = match ctx.args.get("url").and_then(|u| u.as_str()) {
            Some(u) if !u.trim().is_empty() => u.to_string(),
            _ => return mcp_error(&ctx.id, -32602, "tools/call requires a non-blank string 'url'"),
        };
        if let Err(reason) = crate::url_policy::check_async(&url).await {
            return fail(&ctx, "POLICY", &reason);
        }
        let wait_for = ctx
            .args
            .get("wait_for")
            .and_then(|v| v.as_str())
            .map(str::to_string);
        let timeout_secs = ctx
            .args
            .get("timeout_secs")
            .and_then(|v| v.as_u64())
            .unwrap_or(DEFAULT_TIMEOUT_SECS)
            .clamp(MIN_TIMEOUT_SECS, MAX_TIMEOUT_SECS);
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
                mode: "web_fetch".into(),
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

        (ctx.progress)("FETCHING").await;
        // Fresh browser per call: the render is an isolated, unauthenticated
        // session, same isolation model as the app's WebView profile.
        let browser = match crate::browser::Browser::launch(None).await {
            Ok(b) => b,
            Err(e) => return fail(&ctx, "BROWSER_UNAVAILABLE", &format!("{e:#}")),
        };
        let page = match browser
            .load(&url, timeout_secs * 1_000, wait_for.as_deref(), false, None)
            .await
        {
            Ok(p) => p,
            Err(e) => return fail(&ctx, "DOWNLOAD", &format!("page load failed: {}", e.0)),
        };

        // innerText can be huge on JS-heavy pages; cap and flag truncation.
        let truncated = page.text.chars().count() > MAX_TEXT_CHARS;
        let text: String = page.text.chars().take(MAX_TEXT_CHARS).collect();
        let body = json!({
            "url": url,
            "finalUrl": page.final_url,
            "title": page.title,
            "loadFailed": false,
            "truncated": truncated,
            "text": text,
        });
        finish(
            &ctx,
            json!({
                "content": [{ "type": "text", "text": body.to_string() }],
                "isError": false
            }),
        )
    }
}

/// Terminal success: persist the MCP result for replay, then answer.
fn finish(ctx: &ToolCtx<'_>, result: Value) -> JsonRpcMessage {
    if let Err(e) = ctx
        .store
        .mark_terminal(&ctx.canonical, store::COMPLETED, Some(result.clone()))
    {
        tracing::error!(
            "COMPLETED record NOT persisted for {} — a replay may re-invoice: {e:#}",
            ctx.canonical
        );
    }
    ctx.store.inflight_remove(&ctx.canonical);
    mcp_success(&ctx.id, result)
}

/// Terminal failure (cached error result, no refund — same as the app): the
/// payment record keeps its money and replays serve this error.
fn fail(ctx: &ToolCtx<'_>, code: &str, message: &str) -> JsonRpcMessage {
    if let Err(p) = ctx.store.mark_terminal(
        &ctx.canonical,
        store::FAILED,
        Some(store::error_result(code, message)),
    ) {
        tracing::error!(
            "terminal record NOT persisted for {} — replay may re-invoice: {p:#}",
            ctx.canonical
        );
    }
    ctx.store.inflight_remove(&ctx.canonical);
    tool_error(&ctx.id, code, message)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn descriptor_parses_with_required_fields() {
        let d = WebFetch.descriptor();
        assert_eq!(d["name"], "web_fetch");
        assert_eq!(d["inputSchema"]["required"], json!(["url"]));
        assert!(d["inputSchema"]["properties"]["url"].is_object());
        assert!(d["inputSchema"]["properties"]["wait_for"].is_object());
        assert!(d["inputSchema"]["properties"]["timeout_secs"].is_object());
    }

    #[tokio::test]
    async fn quote_rejects_missing_and_blank_url() {
        for a in [
            json!({}),
            json!({ "url": 42 }),
            json!({ "url": "" }),
            json!({ "url": "   " }),
        ] {
            match WebFetch.quote(&Config::default(), &a).await {
                QuoteDecision::Reject { .. } => {}
                QuoteDecision::Quote { .. } => panic!("expected reject for {a}"),
            }
        }
    }

    #[tokio::test]
    async fn quote_rejects_policy_blocked_url() {
        match WebFetch
            .quote(&Config::default(), &json!({ "url": "http://127.0.0.1/" }))
            .await
        {
            QuoteDecision::Reject { message } => {
                assert!(message.contains("safety policy"), "{message}");
            }
            QuoteDecision::Quote { .. } => panic!("loopback must be rejected"),
        }
    }

    #[tokio::test]
    async fn quote_accepts_public_literal_ip_without_dns() {
        match WebFetch
            .quote(&Config::default(), &json!({ "url": "http://93.184.216.34/" }))
            .await
        {
            QuoteDecision::Quote {
                amount,
                description,
                url,
                mode,
            } => {
                assert_eq!(amount, PRICE_SATS);
                assert_eq!(description, "hench web_fetch");
                assert_eq!(url, "http://93.184.216.34/");
                assert_eq!(mode, "web_fetch");
            }
            QuoteDecision::Reject { message } => panic!("reject: {message}"),
        }
    }
}
