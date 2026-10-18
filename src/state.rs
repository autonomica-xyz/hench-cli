//! Shared application state + worker lifecycle (start/stop/restart).
//! The transport is not restartable after close, so every settings change
//! that touches the wire rebuilds it — same contract as the app's worker
//! toggle.
use crate::config::{self, Config};
use crate::nwc::{NwcProcessor, NwcWallet};
use crate::pricing;
use crate::store;
use crate::tools::{self, mcp_error, mcp_success, ToolCtx};
use anyhow::Context;
use contextvm_sdk::core::types::{EncryptionMode, GiftWrapMode, ServerInfo};
use contextvm_sdk::payments::{with_server_payments, ServerPaymentsOptions};
use contextvm_sdk::transport::server::{
    IncomingRequest, NostrServerTransport, NostrServerTransportConfig,
};
use futures::FutureExt;
use nostr_sdk::prelude::{Keys, Tag, ToBech32};
use serde_json::{json, Value};
use std::sync::{Arc, RwLock};
use std::time::Duration;

pub const ABOUT: &str =
    "Hench worker: a computer that works for AI agents (media downloads) over ContextVM";
pub const PAYMENT_TTL_SECS: u64 = 300;
pub const EXECUTION_BUDGET_SECS: u64 = 600;
const UPLOAD_TIMEOUT_SECS: u64 = 900;

pub struct AppState {
    pub data_dir: std::path::PathBuf,
    pub free: bool,
    pub keys: Keys,
    pub tools: tools::ToolRegistry,
    pub config: Arc<RwLock<Config>>,
    pub store: Arc<store::JobStore>,
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
            .with_request_timeout(Duration::from_secs(
                PAYMENT_TTL_SECS + EXECUTION_BUDGET_SECS + 60,
            ))
            .with_session_timeout(Duration::from_secs(
                PAYMENT_TTL_SECS + EXECUTION_BUDGET_SECS * 2 + 60,
            ));

        let mut transport = NostrServerTransport::new(self.keys.clone(), nostr_config).await?;

        if !self.free {
            let wallet = self.ensure_nwc().await?;
            let options = ServerPaymentsOptions::new(
                vec![Arc::new(NwcProcessor {
                    wallet,
                    ttl_secs: PAYMENT_TTL_SECS,
                })],
                self.tools.priced_capabilities(&cfg),
            )
            .with_resolve_price(pricing::quoter(
                self.config.clone(),
                self.store.clone(),
                self.tools.clone(),
            ))
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
        let mut rx = transport
            .take_message_receiver()
            .context("receiver taken")?;
        let _ = transport.publish_tools(self.tools.descriptors()).await;
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
        tracing::info!(
            "worker started (name={}, relays={})",
            cfg.name,
            cfg.relays.join(", ")
        );
        Ok(())
    }

    /// Restart with current config (settings applied to the wire).
    pub async fn restart_worker(self: &Arc<Self>) -> anyhow::Result<()> {
        self.stop_worker().await;
        self.start_worker().await
    }

    /// JSON-RPC dispatch + registry-driven tool execution with replay caching.
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
            "tools/list" => {
                respond(mcp_success(
                    &id,
                    json!({ "tools": self.tools.descriptors() }),
                ))
                .await
            }
            "tools/call" => {
                self.handle_tool_call(
                    transport,
                    &req,
                    &id,
                    r.params.as_ref().unwrap_or(&Value::Null),
                )
                .await?
            }
            other => {
                respond(mcp_error(
                    &id,
                    -32601,
                    &format!("Method not found: {other}"),
                ))
                .await;
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
        let Some(tool) = self.tools.get(name) else {
            respond(mcp_error(id, -32602, &format!("unknown tool: {name}"))).await;
            return Ok(());
        };

        // Same canonical identity the quoter priced — replays resolve here.
        let identity = contextvm_sdk::payments::canonical::compute_canonical_invocation_identity(
            &req.client_pubkey,
            "tools/call",
            Some(params),
        );
        let canonical = identity
            .map(|i| format!("{}:{}", i.client_pubkey, i.invocation_hash))
            .unwrap_or_else(|_| req.event_id.clone());

        if let Some(cached) = cached_terminal_result(&self.store, &canonical) {
            tracing::info!("serving cached result for {canonical}");
            respond(mcp_success(id, cached)).await;
            return Ok(());
        }

        let cfg = self.config.read().unwrap().clone();
        // Mid-execution progress notifications (DOWNLOADING/UPLOADING) go out
        // through the transport, correlated to the request's event id.
        let progress = |status: &str| {
            let t = transport;
            let msg = progress_notification(id, status);
            async move {
                let _ = t
                    .lock()
                    .await
                    .send_notification(&req.client_pubkey, &msg, Some(&req.event_id))
                    .await;
            }
            .boxed()
        };
        let msg = tool
            .run(ToolCtx {
                cfg,
                canonical,
                args,
                id: id.clone(),
                client_pubkey: req.client_pubkey.clone(),
                data_dir: &self.data_dir,
                http: &self.http,
                keys: &self.keys,
                store: self.store.as_ref(),
                progress: &progress,
            })
            .await;
        respond(msg).await;
        Ok(())
    }
}

/// The terminal-replay cache lookup behind handle_tool_call: a COMPLETED or
/// FAILED record answers from its cached MCP result (the quoter waived the
/// gate for the same record), so a repeat call is never re-run nor
/// re-invoiced. A WAITING record — including a paid resumable one — returns
/// None and falls through to the tool.
pub(crate) fn cached_terminal_result(store: &store::JobStore, canonical: &str) -> Option<Value> {
    let record = store.get(canonical)?;
    if record.status != store::COMPLETED && record.status != store::FAILED {
        return None;
    }
    Some(
        record
            .result
            .clone()
            .unwrap_or_else(|| store::error_result("DOWNLOAD", "cached result missing")),
    )
}

fn progress_notification(
    json_rpc_id: &Value,
    status: &str,
) -> contextvm_sdk::core::types::JsonRpcMessage {
    contextvm_sdk::core::types::JsonRpcMessage::Notification(
        contextvm_sdk::core::types::JsonRpcNotification {
            jsonrpc: "2.0".into(),
            method: "notifications/progress".into(),
            params: Some(json!({ "progressToken": json_rpc_id, "status": status })),
        },
    )
}

pub fn upload_http_client() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(UPLOAD_TIMEOUT_SECS))
        .build()
        .expect("http client")
}
