//! CEP-8 dynamic pricing with replay protection:
//!  - terminal record in the store -> Waive (the handler answers from cache,
//!    a replay never mints a second invoice)
//!  - in-flight or interrupted job  -> Reject (no charge)
//!  - outside hours / disabled mode / unsafe URL -> Reject
//!  - else Quote (base + hourly rate over min billable seconds; the Android
//!    platform risk premium is cookie-gated and dropped on CLI)
use crate::config::Config;
use crate::store::{self, JobStore, COMPLETED, WAITING};
use crate::tools::{QuoteDecision, Tool, ToolRegistry};
use contextvm_sdk::payments::canonical::compute_canonical_invocation_identity;
use contextvm_sdk::payments::traits::ResolvePrice;
use contextvm_sdk::payments::types::{ResolvePriceParams, ResolvePriceResult};
use serde_json::Value;
use std::sync::{Arc, RwLock};

// Public API continuity: the pricing functions moved into the download_media
// tool; re-exported so existing callers (and tests) keep resolving them.
#[allow(unused_imports)] // bin crate: only tests reference these by path
pub use crate::tools::download_media::{cap_range_sats, quote_sats};

pub struct Quoter {
    pub config: Arc<RwLock<Config>>,
    pub store: Arc<JobStore>,
    pub registry: ToolRegistry,
}

#[async_trait::async_trait]
impl ResolvePrice for Quoter {
    async fn resolve_price(
        &self,
        params: ResolvePriceParams,
    ) -> Result<ResolvePriceResult, contextvm_sdk::payments::errors::PaymentError> {
        let cfg = self.config.read().unwrap().clone();

        let identity = compute_canonical_invocation_identity(
            &params.client_pubkey,
            &params.request.method,
            params.request.params.as_ref(),
        )
        .map_err(|e| contextvm_sdk::payments::errors::PaymentError::Processor(e.to_string()))?;
        let canonical = format!("{}:{}", identity.client_pubkey, identity.invocation_hash);

        // Replay of a terminal call: waive the gate; the handler serves the
        // cached result. This is the no-double-charge fast path.
        if let Some(record) = self.store.get(&canonical) {
            if record.status == COMPLETED || record.status == store::FAILED {
                tracing::info!("replay of terminal job {}: {}", canonical, record.status);
                return Ok(ResolvePriceResult::Waive { meta: None });
            }
            if record.status == WAITING {
                let reason = if self.store.inflight(&canonical) {
                    "execution already in progress for this request"
                } else {
                    "a previous attempt of this exact request was interrupted; change the request to retry"
                };
                return Ok(ResolvePriceResult::Reject {
                    message: Some(reason.into()),
                });
            }
        }

        if !cfg.hours.open_now() {
            return Ok(ResolvePriceResult::Reject {
                message: Some("worker is outside its hours of operation".into()),
            });
        }

        let args = params
            .request
            .params
            .as_ref()
            .and_then(|p| p.get("arguments"))
            .cloned()
            .unwrap_or(Value::Null);

        // The SDK routes a request here only after it matched a registered
        // PricedCapability ("Self-gate 3: an unpriced request is free" in
        // server_payments.rs / server_explicit_gating.rs:
        // match_priced_capability requires the method to equal "tools/call"
        // and params.name to equal the capability's name), so in practice
        // `name` below is always a registered tool. The quoter has never
        // looked at the name, though — a missing or unknown name keeps
        // pricing with the download_media tool, exactly as before.
        let name = params
            .request
            .params
            .as_ref()
            .and_then(|p| p.get("name"))
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let registered = self.registry.get(name);
        let fallback = crate::tools::DownloadMedia;
        let tool: &dyn Tool = match registered.as_ref() {
            Some(tool) => tool.as_ref(),
            None => &fallback,
        };
        match tool.quote(&cfg, &args) {
            QuoteDecision::Reject { message } => Ok(ResolvePriceResult::Reject {
                message: Some(message),
            }),
            QuoteDecision::Quote {
                amount,
                description,
                url,
                mode,
            } => {
                // Durable-before-invoice: the WAITING record must hit disk before the
                // client can ever hold a payable request. If persistence fails there
                // is no durable state to settle against — refuse the invoice.
                self.store
                    .mark_waiting(store::JobRecord {
                        canonical_id: canonical.clone(),
                        status: WAITING.into(),
                        amount_msat: amount * 1_000,
                        url,
                        mode,
                        client: params.client_pubkey.clone(),
                        result: None,
                        updated_at: 0,
                    })
                    .map_err(|e| {
                        contextvm_sdk::payments::errors::PaymentError::Processor(format!(
                            "cannot persist job record, refusing invoice: {e:#}"
                        ))
                    })?;
                Ok(ResolvePriceResult::Quote {
                    amount,
                    description: Some(description),
                    meta: None,
                })
            }
        }
    }
}

pub fn quoter(
    config: Arc<RwLock<Config>>,
    store: Arc<JobStore>,
    registry: ToolRegistry,
) -> Arc<Quoter> {
    Arc::new(Quoter {
        config,
        store,
        registry,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quote_saturates_instead_of_wrapping() {
        // Regression: i64::MAX rate * min_billable_secs previously wrapped
        // to a negative amount — a free or negative-priced invoice.
        let cfg = Config {
            audio_msat_per_hour: i64::MAX,
            min_billable_secs: i64::MAX,
            ..Config::default()
        };
        assert!(quote_sats(&cfg, "audio").unwrap() > 0);
    }

    #[test]
    fn quote_matches_mode_gates() {
        let cfg = Config {
            accept_audio: false,
            ..Config::default()
        };
        assert!(quote_sats(&cfg, "audio").is_none());
        assert!(quote_sats(&cfg, "full").unwrap() > 0);
    }

    fn price_params(name: Option<&str>, url: &str, mode: &str) -> ResolvePriceParams {
        ResolvePriceParams {
            capability: crate::tools::DownloadMedia
                .priced_capability(&Config::default())
                .unwrap(),
            request: contextvm_sdk::core::types::JsonRpcRequest {
                jsonrpc: "2.0".into(),
                id: serde_json::json!(1),
                method: "tools/call".into(),
                params: Some(serde_json::json!({
                    "name": name,
                    "arguments": { "url": url, "mode": mode }
                })),
            },
            client_pubkey: "aa".repeat(32),
            request_event_id: "event".into(),
        }
    }

    fn test_quoter(dir: &std::path::Path, cfg: Config) -> Arc<Quoter> {
        quoter(
            Arc::new(RwLock::new(cfg)),
            Arc::new(JobStore::open(dir).unwrap()),
            crate::tools::default_registry(),
        )
    }

    #[tokio::test]
    async fn quoter_rejects_bad_mode_with_the_same_message() {
        let dir = std::env::temp_dir().join(format!("hench-quoter-reject-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let q = test_quoter(&dir, Config::default());
        match q
            .resolve_price(price_params(
                Some("download_media"),
                "https://youtube.com/watch?v=x",
                "mp3",
            ))
            .await
            .unwrap()
        {
            ResolvePriceResult::Reject { message } => {
                assert_eq!(message.as_deref(), Some("mode must be 'audio' or 'full'"));
            }
            other => panic!("expected reject, got {other:?}"),
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn quoter_prices_missing_or_unknown_names_like_download_media() {
        // Pins the fallback documented in resolve_price: the SDK only routes
        // capability-matched requests there, and the quoter has never
        // inspected the name — a request arriving anyway must price exactly
        // like a download_media one.
        let dir = std::env::temp_dir().join(format!("hench-quoter-name-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let q = test_quoter(&dir, Config::default());
        for name in [Some("download_media"), Some("totally_other_tool"), None] {
            match q
                .resolve_price(price_params(name, "https://youtube.com/watch?v=x", "flac"))
                .await
                .unwrap()
            {
                ResolvePriceResult::Reject { message } => {
                    assert_eq!(
                        message.as_deref(),
                        Some("mode must be 'audio' or 'full'"),
                        "name={name:?}"
                    );
                }
                other => panic!("expected reject for {name:?}, got {other:?}"),
            }
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn quoter_quotes_audio_and_persists_a_waiting_record() {
        let dir = std::env::temp_dir().join(format!("hench-quoter-quote-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let url = "https://youtube.com/watch?v=dQw4w9WgXcQ";
        // check_source resolves DNS; degrade to a skip when offline.
        if crate::media::check_source(url, &[], &[]).is_err() {
            eprintln!("skipping: no DNS available for check_source");
            return;
        }
        let client = "aa".repeat(32);
        let q = test_quoter(&dir, Config::default());
        let params = price_params(Some("download_media"), url, "audio");
        let identity = compute_canonical_invocation_identity(
            &client,
            "tools/call",
            params.request.params.as_ref(),
        )
        .unwrap();
        let canonical = format!("{}:{}", identity.client_pubkey, identity.invocation_hash);
        match q.resolve_price(params).await.unwrap() {
            ResolvePriceResult::Quote {
                amount,
                description,
                meta,
            } => {
                assert_eq!(description.as_deref(), Some("hench download_media (audio)"));
                assert_eq!(amount, quote_sats(&Config::default(), "audio").unwrap());
                assert!(meta.is_none());
            }
            other => panic!("expected quote, got {other:?}"),
        }
        let record = q.store.get(&canonical).unwrap();
        assert_eq!(record.status, WAITING);
        assert_eq!(record.url, url);
        assert_eq!(record.mode, "audio");
        assert_eq!(
            record.amount_msat,
            quote_sats(&Config::default(), "audio").unwrap() * 1_000
        );
        assert_eq!(record.client, client);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
