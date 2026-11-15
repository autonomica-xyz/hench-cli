//! The `web_search` tool: run a Google search in a real desktop browser and
//! return the parsed SERP as {title,url,snippet} JSON for an agent — the CLI
//! port of the Android app's WebSearchTool. Same isolation model as
//! `web_fetch`: a fresh unauthenticated browser context per render.
//!
//! v1 launches a fresh `Browser` per call (see web_fetch.rs for the upgrade
//! path if launch cost hurts).
use crate::config::Config;
use crate::store;
use crate::tools::{mcp_error, mcp_success, tool_error, QuoteDecision, Tool, ToolCtx};
use contextvm_sdk::core::types::JsonRpcMessage;
use contextvm_sdk::payments::types::PricedCapability;
use serde_json::{json, Value};
use std::collections::HashSet;

pub struct WebSearch;

const PRICE_SATS: i64 = 3;
const DEFAULT_LIMIT: usize = 10;
const MIN_LIMIT: usize = 1;
const MAX_LIMIT: usize = 30;
const SEARCH_TIMEOUT_MS: u64 = 45_000;

/// One parsed SERP row: {title,url,snippet}.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SerpResult {
    pub title: String,
    pub url: String,
    pub snippet: String,
}

#[async_trait::async_trait]
impl Tool for WebSearch {
    fn name(&self) -> &'static str {
        "web_search"
    }

    fn descriptor(&self) -> Value {
        json!({
            "name": "web_search",
            "description": "Google search rendered in a real desktop browser (Chromium, unauthenticated session). Returns a JSON list of {title,url,snippet} results for agent use.",
            "inputSchema": {
                "type": "object",
                "required": ["query"],
                "properties": {
                    "query": { "type": "string", "description": "search query" },
                    "limit": { "type": "integer", "minimum": 1, "maximum": 30, "description": "max results to return, 1-30 (default 10)" }
                }
            }
        })
    }

    fn priced_capability(&self, _cfg: &Config) -> Option<PricedCapability> {
        Some(PricedCapability {
            method: "tools/call".into(),
            name: Some("web_search".into()),
            amount: PRICE_SATS,
            max_amount: Some(PRICE_SATS),
            currency_unit: "sats".into(),
            description: Some("Hench web search".into()),
        })
    }

    async fn quote(&self, _cfg: &Config, args: &Value) -> QuoteDecision {
        let query = args.get("query").and_then(|q| q.as_str()).unwrap_or("");
        if query.trim().is_empty() {
            return QuoteDecision::Reject {
                message: "missing query".into(),
            };
        }
        QuoteDecision::Quote {
            amount: PRICE_SATS,
            description: "hench web_search".into(),
            // The dashboard record never carries the raw query text.
            url: "google.com/search".into(),
            mode: "web_search".into(),
        }
    }

    async fn run(&self, ctx: ToolCtx<'_>) -> JsonRpcMessage {
        let query = match ctx.args.get("query").and_then(|q| q.as_str()) {
            Some(q) if !q.trim().is_empty() => q.to_string(),
            _ => return mcp_error(&ctx.id, -32602, "tools/call requires a non-blank string 'query'"),
        };
        let limit = ctx
            .args
            .get("limit")
            .and_then(|v| v.as_u64())
            .and_then(|n| usize::try_from(n).ok())
            .map(|n| n.clamp(MIN_LIMIT, MAX_LIMIT))
            .unwrap_or(DEFAULT_LIMIT);

        // URLEncoder.encode parity: form-urlencoding (space -> '+').
        let encoded = url::form_urlencoded::Serializer::new(String::new())
            .append_pair("q", &query)
            .append_pair("num", &limit.to_string())
            .append_pair("hl", "en")
            .finish();
        let search_url = format!("https://www.google.com/search?{encoded}");
        if let Err(reason) = crate::url_policy::check_async(&search_url).await {
            return fail(&ctx, "POLICY", &reason);
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
                url: search_url.clone(),
                mode: "web_search".into(),
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

        (ctx.progress)("SEARCHING").await;
        let browser = match crate::browser::Browser::launch(None).await {
            Ok(b) => b,
            Err(e) => return fail(&ctx, "BROWSER_UNAVAILABLE", &format!("{e:#}")),
        };
        let page = match browser
            .load(&search_url, SEARCH_TIMEOUT_MS, None, false, Some(SERP_SCRIPT))
            .await
        {
            Ok(p) => p,
            Err(e) => return fail(&ctx, "DOWNLOAD", &format!("page load failed: {}", e.0)),
        };
        let results = parse_results(page.extracted.as_ref(), limit);
        if results.is_empty() {
            return fail(&ctx, "EMPTY", "no results parsed (possible consent page or block)");
        }
        let body = json!({
            "query": query,
            "results": results.iter().map(|r| json!({
                "title": r.title,
                "url": r.url,
                "snippet": r.snippet,
            })).collect::<Vec<_>>(),
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

/// Parse the SERP_SCRIPT payload: array elements that are objects with
/// non-blank `t` and http(s) `u`; Google self-links are dropped, URLs deduped,
/// capped at `limit`. Pure — unit-tested without a browser.
///
/// SERP_SCRIPT returns JSON.stringify(...): the page's `extracted` value may
/// arrive as a JSON string primitive, so unwrap one level first (the Kotlin
/// port does the same for evaluateJavascript's string result).
pub fn parse_results(extracted: Option<&Value>, limit: usize) -> Vec<SerpResult> {
    let owned;
    let root = match extracted {
        Some(Value::String(s)) => match serde_json::from_str::<Value>(s) {
            Ok(v) => {
                owned = v;
                &owned
            }
            Err(_) => return Vec::new(),
        },
        other => match other {
            Some(v @ Value::Array(_)) => v,
            _ => return Vec::new(),
        },
    };
    let Value::Array(arr) = root else {
        return Vec::new();
    };
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for el in arr {
        if out.len() >= limit {
            break;
        }
        let Some(obj) = el.as_object() else { continue };
        let Some(t) = obj.get("t").and_then(|v| v.as_str()).map(str::trim) else {
            continue;
        };
        if t.is_empty() {
            continue;
        }
        let Some(u) = obj.get("u").and_then(|v| v.as_str()).map(str::trim) else {
            continue;
        };
        if !u.starts_with("http://") && !u.starts_with("https://") {
            continue;
        }
        let Some(host) = reqwest::Url::parse(u)
            .ok()
            .and_then(|p| p.host_str().map(|h| h.to_lowercase()))
        else {
            continue;
        };
        if host == "google.com"
            || host == "www.google.com"
            || host == "m.google.com"
            || host == "googleusercontent.com"
            || host.ends_with(".google.com")
            || host.ends_with(".googleusercontent.com")
        {
            continue;
        }
        if !seen.insert(u.to_string()) {
            continue;
        }
        let s = obj
            .get("s")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .unwrap_or("")
            .to_string();
        out.push(SerpResult {
            title: t.to_string(),
            url: u.to_string(),
            snippet: s,
        });
    }
    out
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

/// Bounded SERP extraction: at most 30 result anchors (those containing an h3
/// or role=heading descendant), snippet from the anchor's closest container
/// div with the heading text removed, sliced to 300 chars. `/url?q=` redirect
/// wrappers are unwrapped via URL searchParams. Returns JSON.stringify of
/// [{t,u,s}] — small payload only. Verbatim port of the Android SERP_SCRIPT.
const SERP_SCRIPT: &str = r#"(function(){
  var out = [];
  var anchors = document.querySelectorAll('a[href]');
  for (var i = 0; i < anchors.length && out.length < 30; i++) {
    var a = anchors[i];
    var h = a.querySelector('h3') || a.querySelector('div[role="heading"]');
    if (!h) continue;
    var u = a.href;
    try {
      var url = new URL(u, location.origin);
      if (url.pathname === '/url' || url.pathname === '/url/') {
        var q = url.searchParams.get('q');
        if (q) u = q;
      }
    } catch (e) {}
    var title = (h.innerText || h.textContent || '').trim();
    if (!title) continue;
    var s = '';
    var box = a.closest('div');
    if (box) {
      s = (box.innerText || box.textContent || '');
      if (title) s = s.replace(title, '');
      s = s.replace(/^\s+|\s+$/g, '');
      if (s.length > 300) s = s.slice(0, 300);
    }
    out.push({t: title, u: u, s: s});
  }
  return JSON.stringify(out);
})()"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn descriptor_parses_with_required_fields() {
        let d = WebSearch.descriptor();
        assert_eq!(d["name"], "web_search");
        assert_eq!(d["inputSchema"]["required"], json!(["query"]));
        assert!(d["inputSchema"]["properties"]["query"].is_object());
        assert!(d["inputSchema"]["properties"]["limit"].is_object());
    }

    #[tokio::test]
    async fn quote_rejects_missing_and_blank_query() {
        for a in [json!({}), json!({ "query": 7 }), json!({ "query": "" }), json!({ "query": "  " })]
        {
            match WebSearch.quote(&Config::default(), &a).await {
                QuoteDecision::Reject { .. } => {}
                QuoteDecision::Quote { .. } => panic!("expected reject for {a}"),
            }
        }
    }

    #[tokio::test]
    async fn quote_accepts_query_with_flat_price() {
        match WebSearch
            .quote(&Config::default(), &json!({ "query": "rust async" }))
            .await
        {
            QuoteDecision::Quote {
                amount,
                description,
                mode,
                ..
            } => {
                assert_eq!(amount, PRICE_SATS);
                assert_eq!(description, "hench web_search");
                assert_eq!(mode, "web_search");
            }
            QuoteDecision::Reject { message } => panic!("reject: {message}"),
        }
    }

    #[test]
    fn parse_results_filters_dedupes_and_caps() {
        let extracted = json!([
            { "t": "  ", "u": "https://a.com/", "s": "blank title" },
            { "t": "No URL", "u": "", "s": "" },
            { "t": "ftp only", "u": "ftp://a.com/", "s": "" },
            { "t": "Google self", "u": "https://www.google.com/search?q=x", "s": "" },
            { "t": "Sub google", "u": "https://news.google.com/x", "s": "" },
            { "t": "Googleusercontent", "u": "https://lh3.googleusercontent.com/x", "s": "" },
            { "t": "First", "u": "https://a.com/1", "s": "one" },
            { "t": "Dup", "u": "https://a.com/1", "s": "dupe" },
            { "t": "Second", "u": "https://b.com/2", "s": "two" },
            { "t": "Third", "u": "https://c.com/3", "s": "three" },
            "not-an-object",
            { "t": "Fourth", "u": "https://d.com/4", "s": "four" },
        ]);
        let out = parse_results(Some(&extracted), 10);
        let urls: Vec<&str> = out.iter().map(|r| r.url.as_str()).collect();
        assert_eq!(urls, ["https://a.com/1", "https://b.com/2", "https://c.com/3", "https://d.com/4"]);
        assert_eq!(out[0].title, "First");
        assert_eq!(out[0].snippet, "one");
        // cap respected
        let capped = parse_results(Some(&extracted), 2);
        assert_eq!(capped.len(), 2);
        assert_eq!(capped[1].url, "https://b.com/2");
    }

    #[test]
    fn parse_results_handles_string_wrapped_and_empty_inputs() {
        // evaluateJavascript parity: JSON.stringify output arrives as a string.
        let stringified = json!(r#"[{"t":"A","u":"https://a.com/","s":"s"}]"#);
        let out = parse_results(Some(&stringified), 10);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].title, "A");

        assert!(parse_results(None, 10).is_empty());
        assert!(parse_results(Some(&json!(null)), 10).is_empty());
        assert!(parse_results(Some(&json!(42)), 10).is_empty());
        assert!(parse_results(Some(&json!({ "t": "x" })), 10).is_empty());
        assert!(parse_results(Some(&json!("not json")), 10).is_empty());
        assert!(parse_results(Some(&json!([])), 10).is_empty());
        // string that parses to a non-array
        assert!(parse_results(Some(&json!(r#"{"a":1}"#)), 10).is_empty());
    }
}
