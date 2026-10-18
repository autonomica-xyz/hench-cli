//! CEP-17 multi-stage relay resolution.
//!
//! Resolves operational relays via: config → nprofile hints → kind 10002
//! discovery → fallback probing → bootstrap defaults.
//! Mirrors the TS SDK's `resolveOperationalRelays()` and
//! `connectFallbackOperationalRelays()`.

use std::sync::Arc;
use std::time::Duration;

use nostr_sdk::prelude::*;
use tokio::pin;

use crate::signer::ContextVmSigner;
use crate::relay::RelayPool;
use crate::transport::client::server_relay_discovery::{
    fetch_server_relay_list, select_operational_relay_urls,
};

const LOG_TARGET: &str = "contextvm_sdk::transport::client::relay_resolution";

/// Inputs for multi-stage relay resolution.
pub struct RelayResolutionConfig {
    /// Explicitly configured relay URLs (stage 1).
    pub configured_relay_urls: Vec<String>,
    /// Relay hints from nprofile identity (stage 2).
    pub hinted_relay_urls: Vec<String>,
    /// Bootstrap relays for CEP-17 kind 10002 discovery (stage 4).
    pub discovery_relay_urls: Vec<String>,
    /// Fallback relays probed in parallel with discovery (stage 4).
    pub fallback_operational_relay_urls: Vec<String>,
    /// Server public key for kind 10002 filter.
    pub server_pubkey: PublicKey,
    /// Signer for temporary relay pool connections.
    pub signer: Arc<dyn ContextVmSigner>,
    /// Timeout for discovery and fallback probing.
    pub timeout: Duration,
}

/// Resolve operational relay URLs through a multi-stage pipeline.
///
/// Stages (returns on first non-empty result):
/// 1. Configured relays (explicit `relay_urls`)
/// 2. Hinted relays (from nprofile)
/// 3. If no discovery relays available: use fallback or return empty
/// 4. Race CEP-17 discovery vs fallback probing (`tokio::select!`)
/// 5. Sequential fallback if race winner was empty
/// 6. Last resort: use discovery relay URLs as operational
///
/// Mirrors the TS SDK `resolveOperationalRelays()`.
pub async fn resolve_operational_relays(config: RelayResolutionConfig) -> Vec<String> {
    // Stage 1: configured relays
    if !config.configured_relay_urls.is_empty() {
        return config.configured_relay_urls;
    }

    // Stage 2: hinted relays (from nprofile)
    if !config.hinted_relay_urls.is_empty() {
        tracing::info!(
            target: LOG_TARGET,
            relay_count = config.hinted_relay_urls.len(),
            "Using relay hints from server identity"
        );
        return config.hinted_relay_urls;
    }

    // Stage 3: no discovery relays available
    if config.discovery_relay_urls.is_empty() {
        if !config.fallback_operational_relay_urls.is_empty() {
            tracing::info!(
                target: LOG_TARGET,
                relay_count = config.fallback_operational_relay_urls.len(),
                "Using configured fallback operational relays"
            );
            return config.fallback_operational_relay_urls;
        }
        return vec![];
    }

    // Stage 4: race discovery vs fallback
    let discovery_urls = config.discovery_relay_urls;
    let last_resort_urls = discovery_urls.clone();
    let fallback_urls = config.fallback_operational_relay_urls;
    let server_pubkey = config.server_pubkey;
    let signer = config.signer;
    let timeout = config.timeout;

    let discovery_fut = async {
        let entries =
            fetch_server_relay_list(&server_pubkey, &discovery_urls, signer.clone(), timeout)
                .await
                .unwrap_or_default();
        select_operational_relay_urls(&entries)
    };

    let fallback_fut = connect_fallback_operational_relays(&fallback_urls, signer.clone(), timeout);

    pin!(discovery_fut);
    pin!(fallback_fut);

    // Race: first non-empty wins. If winner is empty, await the loser.
    enum RaceWinner {
        Discovery(Vec<String>),
        Fallback(Vec<String>),
    }

    // When the winner is non-empty the losing future is dropped.
    // If it was connect_fallback_operational_relays, the temporary
    // pool disconnects when the Client drops -- resource leak is minimal.
    let winner = tokio::select! {
        result = &mut discovery_fut => RaceWinner::Discovery(result),
        result = &mut fallback_fut => RaceWinner::Fallback(result),
    };

    match winner {
        RaceWinner::Discovery(urls) if !urls.is_empty() => {
            tracing::info!(target: LOG_TARGET, relay_count = urls.len(), "Resolved operational relays");
            return urls;
        }
        RaceWinner::Fallback(urls) if !urls.is_empty() => {
            tracing::info!(target: LOG_TARGET, relay_count = urls.len(), "Resolved operational relays");
            return urls;
        }
        RaceWinner::Discovery(_) => {
            // Discovery returned empty; await fallback
            let fallback_result = fallback_fut.await;
            if !fallback_result.is_empty() {
                tracing::info!(target: LOG_TARGET, relay_count = fallback_result.len(), "Using configured fallback operational relays");
                return fallback_result;
            }
        }
        RaceWinner::Fallback(_) => {
            // Fallback returned empty; await discovery
            let discovery_result = discovery_fut.await;
            if !discovery_result.is_empty() {
                tracing::info!(target: LOG_TARGET, relay_count = discovery_result.len(), "Resolved operational relays from server relay list");
                return discovery_result;
            }
        }
    }

    // Stage 6: last resort — use discovery relays as operational
    tracing::warn!(
        target: LOG_TARGET,
        relay_count = last_resort_urls.len(),
        "No operational relays discovered from kind 10002; falling back to discovery relays"
    );
    last_resort_urls
}

/// Probe fallback operational relays for connectivity.
///
/// Creates a temporary pool, attempts to connect within `timeout`.
/// Returns the fallback URLs on success, empty vec on failure.
/// Mirrors the TS SDK `connectFallbackOperationalRelays()`.
pub async fn connect_fallback_operational_relays(
    fallback_urls: &[String],
    signer: Arc<dyn ContextVmSigner>,
    timeout: Duration,
) -> Vec<String> {
    if fallback_urls.is_empty() {
        return vec![];
    }

    let pool = match RelayPool::from_arc(signer).await {
        Ok(p) => p,
        Err(_) => return vec![],
    };

    let connect_result = tokio::time::timeout(timeout, pool.connect(fallback_urls)).await;

    let _ = pool.disconnect().await;

    match connect_result {
        Ok(Ok(())) => fallback_urls.to_vec(),
        Ok(Err(e)) => {
            tracing::warn!(
                target: LOG_TARGET,
                error = %e,
                "Fallback operational relay connection failed"
            );
            vec![]
        }
        Err(_) => {
            tracing::warn!(
                target: LOG_TARGET,
                "Fallback operational relay probing timed out"
            );
            vec![]
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::relay::MockRelayPool;

    fn make_signer() -> Arc<dyn ContextVmSigner> {
        let keys = Keys::generate();
        Arc::new(keys) as Arc<dyn ContextVmSigner>
    }

    fn make_config(
        configured: Vec<String>,
        hinted: Vec<String>,
        discovery: Vec<String>,
        fallback: Vec<String>,
    ) -> RelayResolutionConfig {
        RelayResolutionConfig {
            configured_relay_urls: configured,
            hinted_relay_urls: hinted,
            discovery_relay_urls: discovery,
            fallback_operational_relay_urls: fallback,
            server_pubkey: Keys::generate().public_key(),
            signer: make_signer(),
            timeout: Duration::from_secs(5),
        }
    }

    #[tokio::test]
    async fn configured_relays_returned_immediately() {
        let config = make_config(
            vec!["wss://configured.example.com".to_string()],
            vec!["wss://hinted.example.com".to_string()],
            vec!["wss://discovery.example.com".to_string()],
            vec!["wss://fallback.example.com".to_string()],
        );
        let result = resolve_operational_relays(config).await;
        assert_eq!(result, vec!["wss://configured.example.com"]);
    }

    #[tokio::test]
    async fn hinted_relays_returned_when_no_configured() {
        let config = make_config(
            vec![],
            vec!["wss://hint1.example.com".to_string()],
            vec!["wss://discovery.example.com".to_string()],
            vec![],
        );
        let result = resolve_operational_relays(config).await;
        assert_eq!(result, vec!["wss://hint1.example.com"]);
    }

    #[tokio::test]
    async fn no_discovery_with_fallback_returns_fallback() {
        let config = make_config(
            vec![],
            vec![],
            vec![],
            vec!["wss://fallback.example.com".to_string()],
        );
        let result = resolve_operational_relays(config).await;
        assert_eq!(result, vec!["wss://fallback.example.com"]);
    }

    #[tokio::test]
    async fn no_discovery_no_fallback_returns_empty() {
        let config = make_config(vec![], vec![], vec![], vec![]);
        let result = resolve_operational_relays(config).await;
        assert!(result.is_empty());
    }

    #[tokio::test]
    async fn both_empty_falls_back_to_discovery_relays() {
        // discovery_relay_urls are non-empty but the server has no kind 10002 event,
        // and fallback is empty — last resort returns discovery URLs.
        let config = make_config(
            vec![],
            vec![],
            vec!["wss://bootstrap.example.com".to_string()],
            vec![],
        );
        let result = resolve_operational_relays(config).await;
        assert_eq!(result, vec!["wss://bootstrap.example.com"]);
    }

    #[tokio::test]
    async fn connect_fallback_empty_urls_returns_empty() {
        let result =
            connect_fallback_operational_relays(&[], make_signer(), Duration::from_secs(1)).await;
        assert!(result.is_empty());
    }

    #[tokio::test]
    async fn discovery_with_mock_returns_relay_entries() {
        // Test the fetch path via fetch_relay_list_from_pool directly
        use crate::core::constants::{tags, RELAY_LIST_METADATA_KIND};
        use crate::transport::client::server_relay_discovery::fetch_relay_list_from_pool;

        let pool = MockRelayPool::new();
        let server_keys = Keys::generate();

        let tags = vec![Tag::custom(
            tags::RELAY,
            vec!["wss://discovered.example.com"],
        )];
        let event = EventBuilder::new(Kind::Custom(RELAY_LIST_METADATA_KIND), "")
            .tags(tags)
            .custom_created_at(Timestamp::from(1000u64))
            .finalize(&server_keys)
            .unwrap();
        pool.inject_event(event).await;

        let entries =
            fetch_relay_list_from_pool(&server_keys.public_key(), &pool, Duration::from_secs(5))
                .await
                .unwrap();
        let urls = select_operational_relay_urls(&entries);
        assert_eq!(urls, vec!["wss://discovered.example.com"]);
    }

    #[tokio::test]
    async fn marker_precedence_unmarked_preferred() {
        use crate::core::constants::{tags, RELAY_LIST_METADATA_KIND};
        use crate::transport::client::server_relay_discovery::fetch_relay_list_from_pool;

        let pool = MockRelayPool::new();
        let server_keys = Keys::generate();

        let tags = vec![
            Tag::custom(
                tags::RELAY,
                vec!["wss://unmarked.example.com"],
            ),
            Tag::custom(
                tags::RELAY,
                vec!["wss://read.example.com", "read"],
            ),
        ];
        let event = EventBuilder::new(Kind::Custom(RELAY_LIST_METADATA_KIND), "")
            .tags(tags)
            .custom_created_at(Timestamp::from(1000u64))
            .finalize(&server_keys)
            .unwrap();
        pool.inject_event(event).await;

        let entries =
            fetch_relay_list_from_pool(&server_keys.public_key(), &pool, Duration::from_secs(5))
                .await
                .unwrap();
        let urls = select_operational_relay_urls(&entries);
        assert_eq!(urls, vec!["wss://unmarked.example.com"]);
    }

    #[test]
    fn with_discovery_relay_urls_overrides_bootstrap() {
        use crate::transport::client::NostrClientTransportConfig;

        let config = NostrClientTransportConfig::default()
            .with_discovery_relay_urls(vec!["wss://custom-discovery.example.com".to_string()]);
        assert_eq!(
            config.discovery_relay_urls,
            Some(vec!["wss://custom-discovery.example.com".to_string()])
        );
    }

    #[test]
    fn with_fallback_operational_relay_urls_stored_separately() {
        use crate::transport::client::NostrClientTransportConfig;

        let config = NostrClientTransportConfig::default()
            .with_fallback_operational_relay_urls(vec!["wss://fallback.example.com".to_string()]);
        assert_eq!(
            config.fallback_operational_relay_urls,
            Some(vec!["wss://fallback.example.com".to_string()])
        );
        assert!(config.relay_urls.is_empty());
    }
}
