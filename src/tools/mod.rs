//! The worker's tool registry: every job type the worker can run is a [`Tool`]
//! carrying its own MCP descriptor, priced capability, quote logic, and
//! execution. `tools/list`, `publish_tools`, the priced-capability
//! advertisement, and tools/call dispatch all consult the registry — adding a
//! job type means implementing [`Tool`] and registering it in
//! [`default_registry`], with no worker-core edits.
//!
//! The MCP response envelopes ([`mcp_success`]/[`mcp_error`]/[`tool_error`])
//! live here too, because a tool's [`Tool::run`] returns the fully-shaped
//! JSON-RPC message the worker sends back.
use crate::config::Config;
use crate::store::JobStore;
use contextvm_sdk::core::types::JsonRpcMessage;
use contextvm_sdk::payments::types::PricedCapability;
use nostr_sdk::prelude::Keys;
use serde_json::Value;
use std::sync::Arc;

pub mod download_media;

pub use download_media::DownloadMedia;

/// Sends a `notifications/progress` update (a status string such as
/// "DOWNLOADING") to the requesting client mid-execution.
pub type ProgressSink<'a> = &'a (dyn Fn(&str) -> futures::future::BoxFuture<'a, ()> + Send + Sync);

/// Everything a tool needs to execute one call that already passed the
/// worker-generic gates (paid or free, cache-missed, canonical identity
/// computed).
pub struct ToolCtx<'a> {
    /// Config snapshot taken after the replay-cache check, as before.
    pub cfg: Config,
    /// Canonical invocation id — the store/replay key the quoter priced.
    pub canonical: String,
    /// The `arguments` object of the tools/call request.
    pub args: Value,
    /// The JSON-RPC request id (also the progress notification token).
    pub id: Value,
    /// Requesting client's pubkey (hex).
    pub client_pubkey: String,
    /// Worker data dir (job scratch space lives under `<data_dir>/jobs`).
    pub data_dir: &'a std::path::Path,
    /// HTTP client for uploads.
    pub http: &'a reqwest::Client,
    /// Worker signing key (Blossom upload authorization).
    pub keys: &'a Keys,
    /// Job store for free-mode WAITING admission and terminal persistence.
    pub store: &'a JobStore,
    /// Progress-notification channel to the client.
    pub progress: ProgressSink<'a>,
}

/// The tool-specific outcome of price resolution: either a priced quote
/// carrying the fields the job record persists, or a refusal reason.
pub enum QuoteDecision {
    /// A price for this invocation plus the JobRecord fields to persist.
    Quote {
        amount: i64,
        description: String,
        url: String,
        mode: String,
    },
    /// Refuse service (business decision; becomes a payment_rejected).
    Reject { message: String },
}

#[async_trait::async_trait]
pub trait Tool: Send + Sync {
    /// MCP tool name (the `name` field of a tools/call request).
    fn name(&self) -> &'static str;
    /// The MCP tool descriptor JSON served by tools/list and publish_tools.
    fn descriptor(&self) -> Value;
    /// The capability this tool advertises under "tools/call", if priced.
    fn priced_capability(&self, cfg: &Config) -> Option<PricedCapability>;
    /// The tool-specific part of price resolution: argument extraction,
    /// per-mode pricing, and source-safety policy, returning the amount,
    /// invoice description, and the job-record fields to persist. The
    /// worker-generic parts (canonical id, replay waive/reject, hours,
    /// durable-before-invoice persistence) stay in the quoter.
    fn quote(&self, cfg: &Config, args: &Value) -> QuoteDecision;
    /// Execute one call end to end — argument validation, download, upload,
    /// result shaping, terminal persistence, inflight bookkeeping — and
    /// return the JSON-RPC message to send the client.
    async fn run(&self, ctx: ToolCtx<'_>) -> JsonRpcMessage;
}

/// The set of job types this worker offers.
#[derive(Clone)]
pub struct ToolRegistry {
    tools: Vec<Arc<dyn Tool>>,
}

impl ToolRegistry {
    /// The tool registered under `name`, if any.
    pub fn get(&self, name: &str) -> Option<Arc<dyn Tool>> {
        self.tools.iter().find(|t| t.name() == name).cloned()
    }

    /// Descriptor JSON for tools/list and publish_tools, in registry order.
    pub fn descriptors(&self) -> Vec<Value> {
        self.tools.iter().map(|t| t.descriptor()).collect()
    }

    /// Priced capabilities for the advertisement, in registry order.
    pub fn priced_capabilities(&self, cfg: &Config) -> Vec<PricedCapability> {
        self.tools
            .iter()
            .filter_map(|t| t.priced_capability(cfg))
            .collect()
    }
}

/// The registry this worker ships with.
pub fn default_registry() -> ToolRegistry {
    ToolRegistry {
        tools: vec![Arc::new(DownloadMedia)],
    }
}

pub fn mcp_success(id: &Value, result: Value) -> JsonRpcMessage {
    JsonRpcMessage::Response(contextvm_sdk::core::types::JsonRpcResponse {
        jsonrpc: "2.0".into(),
        id: id.clone(),
        result,
    })
}

pub fn mcp_error(id: &Value, code: i64, message: &str) -> JsonRpcMessage {
    JsonRpcMessage::ErrorResponse(contextvm_sdk::core::types::JsonRpcErrorResponse {
        jsonrpc: "2.0".into(),
        id: id.clone(),
        error: contextvm_sdk::core::types::JsonRpcError {
            code,
            message: message.to_string(),
            data: None,
        },
    })
}

/// Error tool result wrapped in a success envelope (isError carries it).
pub fn tool_error(id: &Value, code: &str, message: &str) -> JsonRpcMessage {
    mcp_success(id, crate::store::error_result(code, message))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Captured from the pre-registry code: serde_json::to_string of the
    /// download_media_tool() descriptor (keys sort — no preserve_order).
    const GOLDEN_TOOL: &str = r#"{"description":"Download audio or video from a URL (YouTube and other platforms) via yt-dlp. Returns a hosted file URL.","inputSchema":{"properties":{"blossom_server":{"description":"optional Blossom server override (must be in worker allowlist)","type":"string"},"mode":{"description":"audio = mp3 extract; full = best mp4","enum":["audio","full"],"type":"string"},"url":{"description":"http(s) URL of the media page","type":"string"}},"required":["url","mode"],"type":"object"},"name":"download_media"}"#;

    /// Captured from the pre-registry tools/list response payload.
    const GOLDEN_TOOLS_LIST: &str = r#"{"tools":[{"description":"Download audio or video from a URL (YouTube and other platforms) via yt-dlp. Returns a hosted file URL.","inputSchema":{"properties":{"blossom_server":{"description":"optional Blossom server override (must be in worker allowlist)","type":"string"},"mode":{"description":"audio = mp3 extract; full = best mp4","enum":["audio","full"],"type":"string"},"url":{"description":"http(s) URL of the media page","type":"string"}},"required":["url","mode"],"type":"object"},"name":"download_media"}]}"#;

    #[test]
    fn descriptors_serialize_to_the_golden_tool_json() {
        assert_eq!(
            serde_json::to_string(&default_registry().descriptors()).unwrap(),
            format!("[{GOLDEN_TOOL}]")
        );
    }

    #[test]
    fn tools_list_payload_serializes_to_the_golden_json() {
        assert_eq!(
            serde_json::to_string(&serde_json::json!({
                "tools": default_registry().descriptors()
            }))
            .unwrap(),
            GOLDEN_TOOLS_LIST
        );
    }

    #[test]
    fn priced_capability_matches_the_pre_registry_advertisement() {
        // cap_range_sats(&Config::default()) == (10, 10) pre-refactor.
        let caps = default_registry().priced_capabilities(&Config::default());
        assert_eq!(caps.len(), 1);
        let cap = &caps[0];
        assert_eq!(cap.method, "tools/call");
        assert_eq!(cap.name.as_deref(), Some("download_media"));
        assert_eq!(cap.amount, 10);
        assert_eq!(cap.max_amount, Some(10));
        assert_eq!(cap.currency_unit, "sats");
        assert_eq!(cap.description.as_deref(), Some("Hench media download"));
    }

    #[test]
    fn priced_capability_tracks_cap_range_sats() {
        // With audio off, only the "full" mode is priced; the advertised
        // range must equal what cap_range_sats computes for that config.
        let cfg = Config {
            accept_audio: false,
            ..Config::default()
        };
        let (min, max) = download_media::cap_range_sats(&cfg);
        let caps = default_registry().priced_capabilities(&cfg);
        assert_eq!(caps.len(), 1);
        assert_eq!(caps[0].amount, min);
        assert_eq!(caps[0].max_amount, Some(max));
    }

    #[test]
    fn registry_dispatches_by_name() {
        assert!(default_registry().get("download_media").is_some());
        assert!(default_registry().get("nope").is_none());
    }
}
