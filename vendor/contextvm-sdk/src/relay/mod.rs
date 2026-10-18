//! Nostr relay pool management.
//!
//! Wraps nostr-sdk's Client for relay connection, event publishing, and subscription.

#[cfg(any(test, feature = "test-utils"))]
pub mod mock;
#[cfg(any(test, feature = "test-utils"))]
pub use mock::MockRelayPool;

use async_trait::async_trait;

use crate::core::error::{Error, Result};
use crate::signer::ContextVmSigner;
use nostr_sdk::prelude::*;
use std::sync::Arc;
use std::time::Duration;

/// Trait abstracting relay pool operations, enabling dependency injection and testing.
#[async_trait]
pub trait RelayPoolTrait: Send + Sync {
    /// Connect to the given relay URLs.
    async fn connect(&self, relay_urls: &[String]) -> Result<()>;
    /// Disconnect from all relays.
    async fn disconnect(&self) -> Result<()>;
    /// Publish a pre-built event to relays.
    async fn publish_event(&self, event: &Event) -> Result<EventId>;
    /// Build, sign, and publish an event from a builder.
    async fn publish(&self, builder: EventBuilder) -> Result<EventId>;
    /// Sign an event builder without publishing.
    async fn sign(&self, builder: EventBuilder) -> Result<Event>;
    /// Get the signer associated with this relay pool.
    async fn signer(&self) -> Result<Arc<dyn ContextVmSigner>>;
    /// Get notifications receiver for event streaming.
    fn notifications(&self) -> tokio::sync::broadcast::Receiver<ClientNotification>;
    /// Get the public key of the signer.
    async fn public_key(&self) -> Result<PublicKey>;
    /// Subscribe to events matching filters.
    async fn subscribe(&self, filters: Vec<Filter>) -> Result<()>;
    /// Sign and publish an event to specific relay URLs.
    async fn publish_to(&self, urls: &[String], builder: EventBuilder) -> Result<EventId>;
    /// Fetch events matching filters from connected relays.
    async fn fetch_events(&self, filters: Vec<Filter>, timeout: Duration) -> Result<Vec<Event>>;
}

/// Relay pool wrapper for managing Nostr relay connections.
pub struct RelayPool {
    client: Arc<Client>,
    signer: Arc<dyn ContextVmSigner>,
    notifications: tokio::sync::broadcast::Sender<ClientNotification>,
}

impl RelayPool {
    /// Create a new relay pool with the given signer.
    pub async fn new<T>(signer: T) -> Result<Self>
    where
        T: ContextVmSigner + 'static,
    {
        Self::from_arc(Arc::new(signer)).await
    }

    /// Create a new relay pool from an already boxed signer.
    pub async fn from_arc(signer: Arc<dyn ContextVmSigner>) -> Result<Self> {
        let client = Arc::new(Client::new());
        let (tx, _rx) = tokio::sync::broadcast::channel(1024);

        // nostr-sdk 0.45 turned `Client::notifications()` into a call-scoped
        // `Stream` and removed `handle_notifications`; forward it onto our own
        // broadcast channel so consumers keep the old Receiver shape.
        let mut stream = client.notifications();
        let forward_tx = tx.clone();
        tokio::spawn(async move {
            use futures::StreamExt;
            while let Some(notification) = stream.next().await {
                // Dropping notifications is fine when no receiver is listening.
                let _ = forward_tx.send(notification);
            }
        });

        Ok(Self {
            client,
            signer,
            notifications: tx,
        })
    }

    /// Connect to the given relay URLs.
    pub async fn connect(&self, relay_urls: &[String]) -> Result<()> {
        for url in relay_urls {
            self.client
                .add_relay(url)
                .await
                .map_err(|e| Error::Transport(e.to_string()))?;
        }

        self.client.connect().await;

        Ok(())
    }

    /// Disconnect from all relays.
    pub async fn disconnect(&self) -> Result<()> {
        self.client.disconnect().await;
        Ok(())
    }

    /// Publish a pre-built event to relays.
    pub async fn publish_event(&self, event: &Event) -> Result<EventId> {
        let output = self
            .client
            .send_event(event)
            .await
            .map_err(|e| Error::Transport(e.to_string()))?;
        Ok(output.value)
    }

    /// Build, sign, and publish an event from a builder.
    pub async fn publish(&self, builder: EventBuilder) -> Result<EventId> {
        let event = self.sign(builder).await?;
        self.publish_event(&event).await
    }

    /// Sign an event builder without publishing.
    pub async fn sign(&self, builder: EventBuilder) -> Result<Event> {
        // `finalize_async` needs `AsyncSignEvent`, which `dyn ContextVmSigner`
        // cannot satisfy (unconstrained `Error` assoc types) — split into
        // finalize_unsigned + our own object-safe sign_event.
        let pubkey = self
            .signer
            .get_public_key()
            .await
            .map_err(|e| Error::Transport(e.to_string()))?;
        let unsigned = builder.finalize_unsigned(pubkey);
        self.signer
            .sign_event(unsigned)
            .await
            .map_err(|e| Error::Transport(e.to_string()))
    }

    /// Get the underlying nostr-sdk Client.
    pub fn client(&self) -> &Arc<Client> {
        &self.client
    }

    /// Get notifications receiver for event streaming.
    pub fn notifications(&self) -> tokio::sync::broadcast::Receiver<ClientNotification> {
        self.notifications.subscribe()
    }

    /// Get the public key of the signer.
    pub async fn public_key(&self) -> Result<PublicKey> {
        self.signer
            .get_public_key()
            .await
            .map_err(|e| Error::Other(e.to_string()))
    }

    /// Subscribe to events matching filters.
    pub async fn subscribe(&self, filters: Vec<Filter>) -> Result<()> {
        for filter in filters {
            self.client
                .subscribe(filter)
                .await
                .map_err(|e| Error::Transport(e.to_string()))?;
        }
        Ok(())
    }

    /// Sign and publish an event to specific relay URLs.
    pub async fn publish_to(&self, urls: &[String], builder: EventBuilder) -> Result<EventId> {
        let event = self.sign(builder).await?;
        let output = self
            .client
            .send_event(&event)
            .to(urls)
            .await
            .map_err(|e| Error::Transport(e.to_string()))?;
        Ok(output.value)
    }

    /// Fetch events matching filters from connected relays.
    pub async fn fetch_events(
        &self,
        filters: Vec<Filter>,
        timeout: Duration,
    ) -> Result<Vec<Event>> {
        let mut all_events = Vec::new();
        for filter in filters {
            let events = self
                .client
                .fetch_events(filter)
                .timeout(timeout)
                .await
                .map_err(|e| Error::Transport(e.to_string()))?;
            all_events.extend(events);
        }
        Ok(all_events)
    }
}

#[async_trait]
impl RelayPoolTrait for RelayPool {
    async fn connect(&self, relay_urls: &[String]) -> Result<()> {
        RelayPool::connect(self, relay_urls).await
    }

    async fn disconnect(&self) -> Result<()> {
        RelayPool::disconnect(self).await
    }

    async fn publish_event(&self, event: &Event) -> Result<EventId> {
        RelayPool::publish_event(self, event).await
    }

    async fn publish(&self, builder: EventBuilder) -> Result<EventId> {
        RelayPool::publish(self, builder).await
    }

    async fn sign(&self, builder: EventBuilder) -> Result<Event> {
        RelayPool::sign(self, builder).await
    }

    async fn signer(&self) -> Result<Arc<dyn ContextVmSigner>> {
        Ok(self.signer.clone())
    }

    fn notifications(&self) -> tokio::sync::broadcast::Receiver<ClientNotification> {
        RelayPool::notifications(self)
    }

    async fn public_key(&self) -> Result<PublicKey> {
        RelayPool::public_key(self).await
    }

    async fn subscribe(&self, filters: Vec<Filter>) -> Result<()> {
        RelayPool::subscribe(self, filters).await
    }

    async fn publish_to(&self, urls: &[String], builder: EventBuilder) -> Result<EventId> {
        RelayPool::publish_to(self, urls, builder).await
    }

    async fn fetch_events(&self, filters: Vec<Filter>, timeout: Duration) -> Result<Vec<Event>> {
        RelayPool::fetch_events(self, filters, timeout).await
    }
}
