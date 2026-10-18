//! The `download_media` tool: yt-dlp backed audio/video download with a
//! Blossom-hosted result. Descriptor, pricing, and execution all live here;
//! URL safety and the yt-dlp/Blossom mechanics stay in [`crate::media`].
use crate::config::Config;
use crate::state::EXECUTION_BUDGET_SECS;
use crate::store;
use crate::tools::{mcp_error, mcp_success, tool_error, QuoteDecision, Tool, ToolCtx};
use contextvm_sdk::core::types::JsonRpcMessage;
use contextvm_sdk::payments::types::PricedCapability;
use serde_json::{json, Value};
use std::time::Duration;

pub struct DownloadMedia;

#[async_trait::async_trait]
impl Tool for DownloadMedia {
    fn name(&self) -> &'static str {
        "download_media"
    }

    fn descriptor(&self) -> Value {
        json!({
            "name": "download_media",
            "description": "Download audio or video from a URL (YouTube and other platforms) via yt-dlp. Returns a hosted file URL.",
            "inputSchema": {
                "type": "object",
                "required": ["url", "mode"],
                "properties": {
                    "url": { "type": "string", "description": "http(s) URL of the media page" },
                    "mode": { "type": "string", "enum": ["audio", "full"], "description": "audio = mp3 extract; full = best mp4" },
                    "blossom_server": { "type": "string", "description": "optional Blossom server override (must be in worker allowlist)" }
                }
            }
        })
    }

    fn priced_capability(&self, cfg: &Config) -> Option<PricedCapability> {
        let (cap_min, cap_max) = cap_range_sats(cfg);
        Some(PricedCapability {
            method: "tools/call".into(),
            name: Some("download_media".into()),
            amount: cap_min,
            max_amount: Some(cap_max),
            currency_unit: "sats".into(),
            description: Some("Hench media download".into()),
        })
    }

    async fn quote(&self, cfg: &Config, args: &Value) -> QuoteDecision {
        let mode = args
            .get("mode")
            .and_then(|m| m.as_str())
            .unwrap_or("")
            .to_lowercase();
        let url = args
            .get("url")
            .and_then(|u| u.as_str())
            .unwrap_or("")
            .to_string();

        let Some(amount) = quote_sats(cfg, &mode) else {
            return QuoteDecision::Reject {
                message: if cfg.accept_audio || cfg.accept_video {
                    "mode must be 'audio' or 'full'".into()
                } else {
                    "worker does not accept this media mode".into()
                },
            };
        };
        if let Err(reason) =
            crate::media::check_source(&url, &cfg.allowed_platforms, &cfg.blocked_platforms)
        {
            return QuoteDecision::Reject {
                message: format!("url rejected by worker safety policy: {reason}"),
            };
        }
        QuoteDecision::Quote {
            amount,
            description: format!("hench download_media ({mode})"),
            url,
            mode,
        }
    }

    async fn run(&self, ctx: ToolCtx<'_>) -> JsonRpcMessage {
        let canonical = ctx.canonical.as_str();
        let cfg = &ctx.cfg;
        let url = ctx
            .args
            .get("url")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let mode = ctx
            .args
            .get("mode")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_lowercase();
        let blossom_override = ctx
            .args
            .get("blossom_server")
            .and_then(|v| v.as_str())
            .map(str::to_string);
        if url.is_empty() || (mode != "audio" && mode != "full") {
            return mcp_error(
                &ctx.id,
                -32602,
                "tools/call requires url and mode ('audio'|'full')",
            );
        }
        if !cfg.hours.open_now() {
            return tool_error(
                &ctx.id,
                "CLOSED",
                "worker is outside its hours of operation",
            );
        }
        if let Err(reason) =
            crate::media::check_source(&url, &cfg.allowed_platforms, &cfg.blocked_platforms)
        {
            return tool_error(&ctx.id, "UNSUPPORTED_URL", &reason);
        }
        let mut servers = cfg.blossom.clone();
        if let Some(over) = blossom_override {
            if let Err(reason) = crate::media::check_blossom(&over, &servers) {
                return tool_error(&ctx.id, "UNSUPPORTED_URL", &reason);
            }
            servers.insert(0, over);
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
        // and the dashboard sees the job. Persistence failure still fails the
        // job — a memory-only record would replay a free job as unpaid.
        if ctx.store.get(&ctx.canonical).is_none() {
            if let Err(e) = ctx.store.mark_waiting(store::JobRecord {
                canonical_id: ctx.canonical.clone(),
                status: store::WAITING.into(),
                amount_msat: 0,
                url: url.clone(),
                mode: mode.clone(),
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

        let job_id: String = ctx.canonical.chars().rev().take(16).collect();
        (ctx.progress)("DOWNLOADING").await;

        let out_dir = ctx.data_dir.join("jobs").join(sanitize(&job_id));
        let budget = Duration::from_secs(EXECUTION_BUDGET_SECS);
        let file = match crate::media::run_ytdlp(
            &cfg.ytdlp,
            &url,
            mode == "audio",
            &out_dir,
            budget,
        )
        .await
        {
            Ok(f) => f,
            Err(e) => {
                let code = if is_auth_failure(&e) {
                    "AUTH_FAILURE"
                } else {
                    "DOWNLOAD"
                };
                if let Err(p) = ctx.store.mark_terminal(
                    &ctx.canonical,
                    store::FAILED,
                    Some(store::error_result(code, &e)),
                ) {
                    tracing::error!("terminal record NOT persisted for {canonical} — replay may re-invoice: {p:#}");
                }
                ctx.store.inflight_remove(&ctx.canonical);
                let _ = std::fs::remove_dir_all(&out_dir);
                return tool_error(&ctx.id, code, &e);
            }
        };

        (ctx.progress)("UPLOADING").await;
        let upload = match crate::media::blossom_upload(ctx.http, &servers, &file, ctx.keys).await {
            Ok(u) => u,
            Err(e) => {
                if let Err(p) = ctx.store.mark_terminal(
                    &ctx.canonical,
                    store::FAILED,
                    Some(store::error_result("UPLOAD", &e)),
                ) {
                    tracing::error!("terminal record NOT persisted for {canonical} — replay may re-invoice: {p:#}");
                }
                ctx.store.inflight_remove(&ctx.canonical);
                let _ = std::fs::remove_dir_all(&out_dir);
                return tool_error(&ctx.id, "UPLOAD", &e);
            }
        };
        // Use the produced file's real mime/extension — best can yield
        // webm/mkv, so hardcoding video/mp4 lies to the client.
        let mime = crate::media::mime_for(&file);
        let ext = file
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_string())
            .unwrap_or_else(|| {
                if mode == "audio" {
                    "mp3".into()
                } else {
                    "mp4".into()
                }
            });
        let body = json!({ "url": upload.url, "sha256": upload.sha256, "sizeBytes": upload.size, "mimeType": mime });
        let link_name = format!("media.{ext}");
        let result = json!({
            "content": [
                { "type": "text", "text": body.to_string() },
                { "type": "resource_link", "uri": upload.url, "name": link_name, "mimeType": mime, "description": "Downloaded by hench download_media" }
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
        let _ = std::fs::remove_dir_all(&out_dir);
        tracing::info!("job done: {url} ({} bytes) -> {}", upload.size, upload.url);
        mcp_success(&ctx.id, result)
    }
}

pub fn quote_sats(cfg: &Config, mode: &str) -> Option<i64> {
    let rate = match mode {
        "audio" if cfg.accept_audio => cfg.audio_msat_per_hour,
        "full" if cfg.accept_video => cfg.video_msat_per_hour,
        _ => return None,
    };
    // saturating: dashboard-supplied i64 values can overflow otherwise.
    let usage = rate.saturating_mul(cfg.min_billable_secs) / 3_600;
    Some(cfg.base_msat.saturating_add(usage).saturating_add(999) / 1_000)
}

/// Advertised cap range over the accepted modes.
pub fn cap_range_sats(cfg: &Config) -> (i64, i64) {
    let sats: Vec<i64> = ["audio", "full"]
        .iter()
        .filter_map(|m| quote_sats(cfg, m))
        .collect();
    if sats.is_empty() {
        (0, 0)
    } else {
        (*sats.iter().min().unwrap(), *sats.iter().max().unwrap())
    }
}

fn sanitize(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' {
                c
            } else {
                '_'
            }
        })
        .take(64)
        .collect()
}

fn is_auth_failure(msg: &str) -> bool {
    let l = msg.to_lowercase();
    l.contains("http error 403")
        || l.contains("http error 401")
        || l.contains("sign in")
        || l.contains("login required")
        || l.contains("authentication")
        || l.contains("private video")
        || (l.contains("cookies") && l.contains("required"))
}
