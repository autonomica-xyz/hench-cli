//! Shared application state + worker lifecycle (start/stop/restart).
//! The transport is not restartable after close, so every settings change
//! that touches the wire rebuilds it — same contract as the app's worker
//! toggle.
use crate::config::{self, Config};
use crate::nwc::{NwcProcessor, NwcWallet};
use crate::pricing;
use crate::store::{self, JobStore};
use anyhow::Context;
use contextvm_sdk::core::types::{EncryptionMode, GiftWrapMode, ServerInfo};
use contextvm_sdk::payments::types::PricedCapability;
use contextvm_sdk::payments::{with_server_payments, ServerPaymentsOptions};
use contextvm_sdk::transport::server::{IncomingRequest, NostrServerTransport, NostrServerTransportConfig};
use nostr_sdk::prelude::{Keys, Tag, ToBech32};
use serde_json::{json, Value};
use std::sync::{Arc, RwLock};
use std::time::Duration;

pub const ABOUT: &str = "Hench worker: a computer that works for AI agents (media downloads) over ContextVM";
pub const PAYMENT_TTL_SECS: u64 = 300;
pub const EXECUTION_BUDGET_SECS: u64 = 600;
const UPLOAD_TIMEOUT_SECS: u64 = 900;

pub struct AppState {
    pub data_dir: std::path::PathBuf,
    pub free: bool,
    pub keys: Keys,
    pub config: Arc<RwLock<Config>>,
    pub store: Arc<JobStore>,
    pub http: reqwest::Client,
    /// NWC wallet cached by connection URI.
    pub nwc: tokio::sync::Mutex<Option<(String, Arc<NwcWallet>)>>,
    pub worker: tokio::sync::Mutex<Option<RunningWorker>>,
    pub startup: std::time::Instant,
}

pub struct RunningWorker {
    pub transport: Arc<tokio::sync::Mutex<NostrServerTransport>>,
    pub started_at: std::time::Instant,
}

impl AppState {
    pub fn npub(&self) -> String {
        self.keys.public_key().to_bech32().unwrap_or_default()
    }

    pub async fn ensure_nwc(&self) -> anyhow::Result<Arc<NwcWallet>> {
        let uri = self
            .config
            .read()
            .unwrap()
            .nwc_uri
            .clone()
            .context("no NWC URI configured")?;
        let mut guard = self.nwc.lock().await;
        if let Some((existing, wallet)) = guard.as_ref() {
            if *existing == uri {
                return Ok(wallet.clone());
            }
        }
        let wallet = NwcWallet::connect(&uri).await?;
        *guard = Some((uri, wallet.clone()));
        Ok(wallet)
    }

    /// Stop the worker if running. Idempotent.
    pub async fn stop_worker(&self) {
        let mut guard = self.worker.lock().await;
        if let Some(running) = guard.take() {
            let _ = running.transport.lock().await.close().await;
            tracing::info!("worker stopped");
        }
    }

    /// Build a fresh transport, register payments, announce, spawn the recv loop.
    pub async fn start_worker(self: &Arc<Self>) -> anyhow::Result<()> {
        if self.worker.lock().await.is_some() {
            return Ok(()); // already running
        }
        let cfg = self.config.read().unwrap().clone();
        if let Some(issue) = config::validate(&cfg, self.free).into_iter().next() {
            anyhow::bail!("cannot start worker: {issue}");
        }

        // 0.45: the transport takes a ContextVmSigner (implemented by Keys).
        let nostr_config = NostrServerTransportConfig::default()
            .with_relay_urls(cfg.relays.clone())
            .with_relay_list_urls(cfg.relays.clone())
            .with_encryption_mode(EncryptionMode::Optional)
            .with_gift_wrap_mode(GiftWrapMode::Optional)
            .with_announced_server(true)
            .with_server_info(
                ServerInfo::default()
                    .with_name(cfg.name.clone())
                    .with_version(env!("CARGO_PKG_VERSION"))
                    .with_about(ABOUT),
            )
            // Paid call budget: payment window + execution (+ margin), mirroring the app's ServerConfig.
            .with_request_timeout(Duration::from_secs(PAYMENT_TTL_SECS + EXECUTION_BUDGET_SECS + 60))
            .with_session_timeout(Duration::from_secs(PAYMENT_TTL_SECS + EXECUTION_BUDGET_SECS * 2 + 60));

        let mut transport = NostrServerTransport::new(self.keys.clone(), nostr_config).await?;

        if !self.free {
            let wallet = self.ensure_nwc().await?;
            let (cap_min, cap_max) = pricing::cap_range_sats(&cfg);
            let options = ServerPaymentsOptions::new(
                vec![Arc::new(NwcProcessor {
                    wallet,
                    ttl_secs: PAYMENT_TTL_SECS,
                })],
                vec![PricedCapability {
                    method: "tools/call".into(),
                    name: Some("download_media".into()),
                    amount: cap_min,
                    max_amount: Some(cap_max),
                    currency_unit: "sats".into(),
                    description: Some("Hench media download".into()),
                }],
            )
            .with_resolve_price(pricing::quoter(self.config.clone(), self.store.clone()))
            .with_payment_ttl(Duration::from_secs(PAYMENT_TTL_SECS));
            with_server_payments(&mut transport, options)?;
            // with_server_payments owns the extra-tag slot; re-set with the payment
            // tags it composed plus the hench identity marker so hench-web's
            // `agent=hench` filter finds this worker.
            transport.set_announcement_extra_tags(vec![
                Tag::custom("pmi", ["bitcoin-lightning-bolt11"]),
                Tag::custom("payment_interaction", ["explicit_gating"]),
                Tag::custom("agent", ["hench"]),
            ]);
        } else {
            transport.set_announcement_extra_tags(vec![Tag::custom("agent", ["hench"])]);
        }

        transport.start().await?;
        transport.spawn_discoverability_publication();
        let mut rx = transport.take_message_receiver().context("receiver taken")?;
        let _ = transport.publish_tools(vec![download_media_tool()]).await;
        let _ = transport.announce().await;
        // Announcements can race relay websocket handshakes (seen on-device in
        // the Android worker); re-announcing is idempotent.
        let t = Arc::new(tokio::sync::Mutex::new(transport));
        for delay in [5_u64, 15] {
            let t2 = t.clone();
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_secs(delay)).await;
                let _ = t2.lock().await.announce().await;
            });
        }

        let state = self.clone();
        let t2 = t.clone();
        tokio::spawn(async move {
            while let Some(req) = rx.recv().await {
                let state = state.clone();
                let t = t2.clone();
                tokio::spawn(async move {
                    if let Err(e) = state.handle_request(&t, req).await {
                        tracing::error!("request handling failed: {e:#}");
                    }
                });
            }
            tracing::info!("recv loop exited");
        });

        self.worker.lock().await.replace(RunningWorker {
            transport: t,
            started_at: std::time::Instant::now(),
        });
        tracing::info!("worker started (name={}, relays={})", cfg.name, cfg.relays.join(", "));
        Ok(())
    }

    /// Restart with current config (settings applied to the wire).
    pub async fn restart_worker(self: &Arc<Self>) -> anyhow::Result<()> {
        self.stop_worker().await;
        self.start_worker().await
    }

    /// JSON-RPC dispatch + the download_media executor with replay caching.
    async fn handle_request(
        self: Arc<Self>,
        transport: &tokio::sync::Mutex<NostrServerTransport>,
        req: IncomingRequest,
    ) -> anyhow::Result<()> {
        let contextvm_sdk::core::types::JsonRpcMessage::Request(r) = &req.message else {
            return Ok(()); // notifications expect no reply
        };
        let id = r.id.clone();
        let event_id = req.event_id.clone();
        let respond = |msg: contextvm_sdk::core::types::JsonRpcMessage| {
            let t = transport;
            async move {
                let _ = t.lock().await.send_response(&event_id, msg).await;
            }
        };
        match r.method.as_str() {
            "initialize" | "ping" => respond(mcp_success(&id, json!({}))).await,
            "tools/list" => respond(mcp_success(&id, json!({ "tools": [download_media_tool()] }))).await,
            "tools/call" => self.handle_tool_call(transport, &req, &id, r.params.as_ref().unwrap_or(&Value::Null)).await?,
            other => {
                respond(mcp_error(&id, -32601, &format!("Method not found: {other}"))).await;
            }
        }
        Ok(())
    }

    async fn handle_tool_call(
        self: Arc<Self>,
        transport: &tokio::sync::Mutex<NostrServerTransport>,
        req: &IncomingRequest,
        id: &Value,
        params: &Value,
    ) -> anyhow::Result<()> {
        let event_id = req.event_id.clone();
        let respond = |msg: contextvm_sdk::core::types::JsonRpcMessage| {
            let t = transport;
            async move {
                let _ = t.lock().await.send_response(&event_id, msg).await;
            }
        };
        let name = params.get("name").and_then(|v| v.as_str()).unwrap_or("");
        let args = params.get("arguments").cloned().unwrap_or(json!({}));
        if name != "download_media" {
            respond(mcp_error(id, -32602, &format!("unknown tool: {name}"))).await;
            return Ok(());
        }

        // Same canonical identity the quoter priced — replays resolve here.
        let identity = contextvm_sdk::payments::canonical::compute_canonical_invocation_identity(
            &req.client_pubkey,
            "tools/call",
            Some(params),
        );
        let canonical = identity
            .map(|i| format!("{}:{}", i.client_pubkey, i.invocation_hash))
            .unwrap_or_else(|_| req.event_id.clone());

        if let Some(record) = self.store.get(&canonical) {
            if record.status == store::COMPLETED || record.status == store::FAILED {
                let cached = record
                    .result
                    .clone()
                    .unwrap_or_else(|| store::error_result("DOWNLOAD", "cached result missing"));
                tracing::info!("serving cached result for {canonical}");
                respond(mcp_success(id, cached)).await;
                return Ok(());
            }
        }

        let cfg = self.config.read().unwrap().clone();
        let url = args.get("url").and_then(|v| v.as_str()).unwrap_or("").to_string();
        let mode = args.get("mode").and_then(|v| v.as_str()).unwrap_or("").to_lowercase();
        let blossom_override = args.get("blossom_server").and_then(|v| v.as_str()).map(str::to_string);
        if url.is_empty() || (mode != "audio" && mode != "full") {
            respond(mcp_error(id, -32602, "tools/call requires url and mode ('audio'|'full')")).await;
            return Ok(());
        }
        if !cfg.hours.open_now() {
            respond(tool_error(id, "CLOSED", "worker is outside its hours of operation")).await;
            return Ok(());
        }
        if let Err(reason) =
            crate::media::check_source(&url, &cfg.allowed_platforms, &cfg.blocked_platforms)
        {
            respond(tool_error(id, "UNSUPPORTED_URL", &reason)).await;
            return Ok(());
        }
        let mut servers = cfg.blossom.clone();
        if let Some(over) = blossom_override {
            if let Err(reason) = crate::media::check_blossom(&over, &servers) {
                respond(tool_error(id, "UNSUPPORTED_URL", &reason)).await;
                return Ok(());
            }
            servers.insert(0, over);
        }
        if !self.store.inflight_add(&canonical) {
            respond(tool_error(id, "BUSY", "execution already in progress for this request")).await;
            return Ok(());
        }
        // Free mode never ran the quoter, so no WAITING record exists yet:
        // create it here (amount 0) so terminal results still cache/replay
        // and the dashboard sees the job. Persistence failure still fails the
        // job — a memory-only record would replay a free job as unpaid.
        if self.store.get(&canonical).is_none() {
            if let Err(e) = self.store.mark_waiting(store::JobRecord {
                canonical_id: canonical.clone(),
                status: store::WAITING.into(),
                amount_msat: 0,
                url: url.clone(),
                mode: mode.clone(),
                client: req.client_pubkey.clone(),
                result: None,
                updated_at: 0,
            }) {
                self.store.inflight_remove(&canonical);
                respond(tool_error(id, "INTERNAL", &format!("job store unavailable: {e:#}"))).await;
                return Ok(());
            }
        }

        let job_id: String = canonical.chars().rev().take(16).collect();
        let _ = transport
            .lock()
            .await
            .send_notification(
                &req.client_pubkey,
                &progress_notification(id, "DOWNLOADING"),
                Some(&req.event_id),
            )
            .await;

        let out_dir = self.data_dir.join("jobs").join(sanitize(&job_id));
        let budget = Duration::from_secs(EXECUTION_BUDGET_SECS);
        let file = match crate::media::run_ytdlp(&cfg.ytdlp, &url, mode == "audio", &out_dir, budget).await {
            Ok(f) => f,
            Err(e) => {
                let code = if is_auth_failure(&e) { "AUTH_FAILURE" } else { "DOWNLOAD" };
                if let Err(p) = self.store.mark_terminal(&canonical, store::FAILED, Some(store::error_result(code, &e))) {
                    tracing::error!("terminal record NOT persisted for {canonical} — replay may re-invoice: {p:#}");
                }
                self.store.inflight_remove(&canonical);
                let _ = std::fs::remove_dir_all(&out_dir);
                respond(tool_error(id, code, &e)).await;
                return Ok(());
            }
        };

        let _ = transport
            .lock()
            .await
            .send_notification(
                &req.client_pubkey,
                &progress_notification(id, "UPLOADING"),
                Some(&req.event_id),
            )
            .await;
        let upload = match crate::media::blossom_upload(&self.http, &servers, &file, &self.keys).await {
            Ok(u) => u,
            Err(e) => {
                if let Err(p) = self.store.mark_terminal(&canonical, store::FAILED, Some(store::error_result("UPLOAD", &e))) {
                    tracing::error!("terminal record NOT persisted for {canonical} — replay may re-invoice: {p:#}");
                }
                self.store.inflight_remove(&canonical);
                let _ = std::fs::remove_dir_all(&out_dir);
                respond(tool_error(id, "UPLOAD", &e)).await;
                return Ok(());
            }
        };
        // Use the produced file's real mime/extension — best can yield
        // webm/mkv, so hardcoding video/mp4 lies to the client.
        let mime = crate::media::mime_for(&file);
        let ext = file
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_string())
            .unwrap_or_else(|| if mode == "audio" { "mp3".into() } else { "mp4".into() });
        let body = json!({ "url": upload.url, "sha256": upload.sha256, "sizeBytes": upload.size, "mimeType": mime });
        let link_name = format!("media.{ext}");
        let result = json!({
            "content": [
                { "type": "text", "text": body.to_string() },
                { "type": "resource_link", "uri": upload.url, "name": link_name, "mimeType": mime, "description": "Downloaded by hench download_media" }
            ],
            "isError": false
        });
        if let Err(e) = self.store.mark_terminal(&canonical, store::COMPLETED, Some(result.clone())) {
            tracing::error!("COMPLETED record NOT persisted for {canonical} — a replay may re-invoice: {e:#}");
        }
        self.store.inflight_remove(&canonical);
        let _ = std::fs::remove_dir_all(&out_dir);
        tracing::info!("job done: {url} ({} bytes) -> {}", upload.size, upload.url);
        respond(mcp_success(id, result)).await;
        Ok(())
    }
}

pub fn download_media_tool() -> Value {
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

pub fn mcp_success(id: &Value, result: Value) -> contextvm_sdk::core::types::JsonRpcMessage {
    contextvm_sdk::core::types::JsonRpcMessage::Response(contextvm_sdk::core::types::JsonRpcResponse {
        jsonrpc: "2.0".into(),
        id: id.clone(),
        result,
    })
}

pub fn mcp_error(id: &Value, code: i64, message: &str) -> contextvm_sdk::core::types::JsonRpcMessage {
    contextvm_sdk::core::types::JsonRpcMessage::ErrorResponse(
        contextvm_sdk::core::types::JsonRpcErrorResponse {
            jsonrpc: "2.0".into(),
            id: id.clone(),
            error: contextvm_sdk::core::types::JsonRpcError {
                code,
                message: message.to_string(),
                data: None,
            },
        },
    )
}

fn tool_error(id: &Value, code: &str, message: &str) -> contextvm_sdk::core::types::JsonRpcMessage {
    mcp_success(id, store::error_result(code, message))
}

fn progress_notification(json_rpc_id: &Value, status: &str) -> contextvm_sdk::core::types::JsonRpcMessage {
    contextvm_sdk::core::types::JsonRpcMessage::Notification(contextvm_sdk::core::types::JsonRpcNotification {
        jsonrpc: "2.0".into(),
        method: "notifications/progress".into(),
        params: Some(json!({ "progressToken": json_rpc_id, "status": status })),
    })
}

fn sanitize(s: &str) -> String {
    s.chars().map(|c| if c.is_ascii_alphanumeric() || c == '-' { c } else { '_' }).take(64).collect()
}

fn is_auth_failure(msg: &str) -> bool {
    let l = msg.to_lowercase();
    l.contains("http error 403") || l.contains("http error 401") || l.contains("sign in")
        || l.contains("login required") || l.contains("authentication") || l.contains("private video")
        || (l.contains("cookies") && l.contains("required"))
}

pub fn upload_http_client() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(UPLOAD_TIMEOUT_SECS))
        .build()
        .expect("http client")
}
