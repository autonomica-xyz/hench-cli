//! CEP-8 dynamic pricing with replay protection:
//!  - terminal record in the store -> Waive (the handler answers from cache,
//!    a replay never mints a second invoice)
//!  - in-flight or interrupted job  -> Reject (no charge)
//!  - outside hours / disabled mode / unsafe URL -> Reject
//!  - else Quote (base + hourly rate over min billable seconds; the Android
//!    platform risk premium is cookie-gated and dropped on CLI)
use crate::config::Config;
use crate::store::{self, JobStore, COMPLETED, WAITING};
use contextvm_sdk::payments::canonical::compute_canonical_invocation_identity;
use contextvm_sdk::payments::traits::ResolvePrice;
use contextvm_sdk::payments::types::{ResolvePriceParams, ResolvePriceResult};
use serde_json::Value;
use std::sync::{Arc, RwLock};

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

pub struct Quoter {
    pub config: Arc<RwLock<Config>>,
    pub store: Arc<JobStore>,
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

        let Some(amount) = quote_sats(&cfg, &mode) else {
            return Ok(ResolvePriceResult::Reject {
                message: Some(if cfg.accept_audio || cfg.accept_video {
                    "mode must be 'audio' or 'full'".into()
                } else {
                    "worker does not accept this media mode".into()
                }),
            });
        };
        if let Err(reason) =
            crate::media::check_source(&url, &cfg.allowed_platforms, &cfg.blocked_platforms)
        {
            return Ok(ResolvePriceResult::Reject {
                message: Some(format!("url rejected by worker safety policy: {reason}")),
            });
        }

        // Durable-before-invoice: the WAITING record must hit disk before the
        // client can ever hold a payable request. If persistence fails there
        // is no durable state to settle against — refuse the invoice.
        self.store
            .mark_waiting(store::JobRecord {
                canonical_id: canonical.clone(),
                status: WAITING.into(),
                amount_msat: amount * 1_000,
                url,
                mode: mode.clone(),
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
            description: Some(format!("hench download_media ({mode})")),
            meta: None,
        })
    }
}

pub fn quoter(config: Arc<RwLock<Config>>, store: Arc<JobStore>) -> Arc<Quoter> {
    Arc::new(Quoter { config, store })
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
}
