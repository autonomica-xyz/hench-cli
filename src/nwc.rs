//! NWC (NIP-47) wallet client + CEP-8 PaymentProcessor over it.
//! Invoice creation via `make_invoice`, settlement via `lookup_invoice` polling.
use contextvm_sdk::payments::constants::PMI_BITCOIN_LIGHTNING_BOLT11;
use contextvm_sdk::payments::errors::PaymentError;
use contextvm_sdk::payments::traits::PaymentProcessor;
use contextvm_sdk::payments::types::{
    PaymentProcessorCreateParams, PaymentProcessorVerifyParams, PaymentRequiredParams,
    VerifyOutcome,
};
use futures::StreamExt;
use nostr::nips::nip47::{
    LookupInvoiceRequest, MakeInvoiceRequest, Method, Nip47Ciphers, NostrWalletConnectUri, Request,
    Response, ResponseResult, TransactionState,
};
use nostr::prelude::{Event, Filter, Keys, Kind};
use nostr_sdk::prelude::{Client, ClientNotification};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::oneshot;

pub struct NwcWallet {
    uri: NostrWalletConnectUri,
    /// This NWC connection's client (sender) keys — responses must be
    /// addressed to this pubkey in their `p` tag.
    client_keys: Keys,
    client: Client,
    /// req event id -> (expected response method, channel).
    pending: Mutex<HashMap<String, (Method, oneshot::Sender<Response>)>>,
}

struct PendingRequest<'a> {
    wallet: &'a NwcWallet,
    id: String,
}

impl Drop for PendingRequest<'_> {
    fn drop(&mut self) {
        self.wallet.pending.lock().unwrap().remove(&self.id);
    }
}

impl NwcWallet {
    pub async fn connect(uri_str: &str) -> anyhow::Result<Arc<Self>> {
        let uri = NostrWalletConnectUri::parse(uri_str)
            .map_err(|e| anyhow::anyhow!("invalid NWC URI: {e}"))?;
        let client_keys = Keys::new(uri.secret.clone());
        let client = Client::new();
        for relay in &uri.relays {
            client.add_relay(relay.clone()).await?;
        }
        client.connect().await;
        // WalletConnect responses are AUTHORED BY THE WALLET and addressed to
        // the client pubkey. Filtering by our own author key (the previous
        // behavior) either matched nothing or matched attacker-shaped events;
        // filter on the wallet author and the recipient tag instead.
        let filter = Filter::new()
            .kind(Kind::WalletConnectResponse)
            .author(uri.public_key)
            .pubkey(client_keys.public_key());
        let _ = client.subscribe(filter).await?;

        let wallet = Arc::new(Self {
            uri,
            client_keys,
            client,
            pending: Mutex::new(HashMap::new()),
        });

        // Route kind-23195 events to waiting requests, correlated by `e` tag.
        let w = wallet.clone();
        let client = wallet.client.clone();
        tokio::spawn(async move {
            let mut notifications = client.notifications();
            while let Some(n) = notifications.next().await {
                if let ClientNotification::Event { event, .. } = n {
                    if event.kind == Kind::WalletConnectResponse {
                        w.deliver(&event).await;
                    }
                }
            }
        });
        Ok(wallet)
    }

    async fn deliver(&self, event: &Event) {
        // Responses must come from the configured wallet and be addressed to
        // this client's pubkey — an attacker-signed event carrying replayed
        // wallet ciphertext is dropped here, not decrypted.
        if event.pubkey != self.uri.public_key {
            return;
        }
        let mine = self.client_keys.public_key().to_hex();
        let addressed = event
            .tags
            .iter()
            .any(|t| t.kind() == "p" && t.content() == Some(mine.as_str()));
        if !addressed {
            return;
        }
        // Correlation: the `e` tag must reference a pending request WE sent,
        // and the decrypted result type must match that request's method.
        let req_id = event
            .tags
            .iter()
            .find(|t| t.kind() == "e")
            .and_then(|t| t.content())
            .unwrap_or_default();
        // from_event verifies id + signature + author==wallet, decrypts, parses.
        let Ok(response) = Response::from_event(&self.uri, event, Nip47Ciphers::NIP04) else {
            return;
        };
        let mut pending = self.pending.lock().unwrap();
        match pending.get(req_id) {
            Some((method, _)) if *method == response.result_type => {
                if let Some((_, tx)) = pending.remove(req_id) {
                    let _ = tx.send(response);
                }
            }
            Some(_) => tracing::warn!("NWC response method mismatch for {req_id}, ignoring"),
            None => {}
        }
    }

    /// Send one NWC request and await its correlated response.
    pub async fn request(&self, req: Request) -> anyhow::Result<Response> {
        let method = req.method.clone();
        let event = req.to_event(&self.uri, Nip47Ciphers::NIP04)?;
        let req_id = event.id.to_hex();
        let (tx, rx) = oneshot::channel();
        self.pending
            .lock()
            .unwrap()
            .insert(req_id.clone(), (method, tx));
        // Drop also cleans up a failed publication or a cancelled caller.
        let _pending = PendingRequest {
            wallet: self,
            id: req_id,
        };
        self.client.send_event(&event).await?;
        match tokio::time::timeout(Duration::from_secs(30), rx).await {
            Ok(Ok(resp)) => Ok(resp),
            _ => {
                anyhow::bail!("NWC response timed out")
            }
        }
    }

    /// Create a bolt11 invoice. Returns (bolt11, payment_hash).
    pub async fn make_invoice(
        &self,
        amount_msat: u64,
        description: &str,
    ) -> anyhow::Result<(String, String)> {
        let req = Request::make_invoice(MakeInvoiceRequest {
            amount: amount_msat,
            description: Some(description.to_string()),
            description_hash: None,
            expiry: Some(600),
        });
        let resp = self.request(req).await?;
        if let Some(err) = resp.error {
            anyhow::bail!("NWC make_invoice error: {}", err.message);
        }
        match resp.result {
            Some(ResponseResult::MakeInvoice(r)) => {
                let hash = r.payment_hash.clone().unwrap_or_default();
                if r.invoice.is_empty() || hash.is_empty() {
                    anyhow::bail!("NWC make_invoice response missing invoice/payment_hash");
                }
                Ok((r.invoice, hash))
            }
            _ => anyhow::bail!("unexpected NWC make_invoice response shape"),
        }
    }

    /// Settled? for a bolt11 invoice (looked up by invoice string, so wallets
    /// that omit payment_hash in make_invoice still work).
    pub async fn is_settled(&self, bolt11: &str) -> anyhow::Result<bool> {
        let req = Request::lookup_invoice(LookupInvoiceRequest {
            payment_hash: None,
            invoice: Some(bolt11.to_string()),
        });
        let resp = self.request(req).await?;
        if let Some(err) = resp.error {
            anyhow::bail!("NWC lookup_invoice error: {}", err.message);
        }
        match resp.result {
            Some(ResponseResult::LookupInvoice(r)) => {
                // The transaction must be the one we asked about — a settled
                // state for a DIFFERENT invoice must never authorize this job.
                if let Some(invoice) = r.invoice.as_deref() {
                    if !invoice.eq_ignore_ascii_case(bolt11) {
                        tracing::warn!(
                            "NWC lookup_invoice returned a different invoice — not ours"
                        );
                        return Ok(false);
                    }
                }
                Ok(matches!(r.state, Some(TransactionState::Settled)))
            }
            _ => anyhow::bail!("unexpected NWC lookup_invoice response shape"),
        }
    }
}

/// CEP-8 processor: invoices through NWC, verifies by polling.
pub struct NwcProcessor {
    pub wallet: Arc<NwcWallet>,
    /// Invoice TTL offered to clients (also the middleware's polling bound).
    pub ttl_secs: u64,
}

const POLL_INTERVAL: Duration = Duration::from_secs(3);

#[async_trait::async_trait]
impl PaymentProcessor for NwcProcessor {
    fn pmi(&self) -> &str {
        PMI_BITCOIN_LIGHTNING_BOLT11
    }

    async fn create_payment_required(
        &self,
        params: PaymentProcessorCreateParams,
    ) -> Result<PaymentRequiredParams, PaymentError> {
        let desc = params
            .description
            .clone()
            .unwrap_or_else(|| "hench worker job".into());
        let (bolt11, _hash) = self
            .wallet
            .make_invoice(params.amount as u64 * 1_000, &desc)
            .await
            .map_err(|e| PaymentError::Processor(format!("make_invoice failed: {e:#}")))?;
        tracing::info!("invoice created: {} sats for {}", params.amount, desc);
        Ok(PaymentRequiredParams {
            amount: params.amount,
            pay_req: bolt11,
            pmi: self.pmi().to_string(),
            description: Some(desc),
            ttl: Some(self.ttl_secs),
            meta: None,
        })
    }

    async fn verify_payment(
        &self,
        params: PaymentProcessorVerifyParams,
    ) -> Result<VerifyOutcome, PaymentError> {
        // The middleware wraps this in its own ttl-bound timeout; we poll until
        // settled. Cancel on shutdown.
        loop {
            match self.wallet.is_settled(&params.pay_req).await {
                Ok(true) => {
                    tracing::info!("payment settled");
                    return Ok(VerifyOutcome { meta: None });
                }
                Ok(false) => {}
                Err(e) => tracing::warn!("lookup_invoice failed (retrying): {e:#}"),
            }
            tokio::select! {
                _ = tokio::time::sleep(POLL_INTERVAL) => {}
                _ = params.cancel.cancelled() => {
                    return Err(PaymentError::Processor("cancelled".into()));
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nostr::prelude::{EventBuilder, EventId, FinalizeEvent, Tag};

    fn wallet_fixture() -> (Arc<NwcWallet>, Keys, Keys) {
        let wallet_keys = Keys::generate();
        let client_keys = Keys::generate();
        let uri = format!(
            "nostr+walletconnect://{}?relay=wss://relay.invalid&secret={}",
            wallet_keys.public_key().to_hex(),
            client_keys.secret_key().to_secret_hex()
        );
        let wallet = NwcWallet {
            uri: NostrWalletConnectUri::parse(&uri).unwrap(),
            client: Client::new(),
            client_keys: client_keys.clone(),
            pending: Mutex::new(HashMap::new()),
        };
        (Arc::new(wallet), wallet_keys, client_keys)
    }

    fn settled_response_json() -> String {
        serde_json::json!({
            "result_type": "lookup_invoice",
            "result": {
                "transaction_type": "incoming",
                "state": "settled",
                "invoice": "lnbc1historic",
                "payment_hash": "ab".repeat(32),
                "amount": 1000,
                "fees_paid": 0,
                "created_at": 1
            }
        })
        .to_string()
    }

    #[tokio::test]
    async fn failed_publish_releases_pending_request() {
        let (wallet, _, _) = wallet_fixture();
        assert!(wallet.request(Request::get_balance()).await.is_err());
        assert!(wallet.pending.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn attacker_signed_replayed_ciphertext_is_dropped() {
        // Regression: replaying a genuine wallet response's ciphertext inside
        // a DIFFERENT signed event (attacker keys, victim e/p tags) previously
        // decrypted and marked a new lookup settled. Author must be the
        // configured wallet; recipient must be us; e-tag must be pending.
        let (wallet, wallet_keys, client_keys) = wallet_fixture();
        let (tx, mut rx) = oneshot::channel();
        let req_id = "11".repeat(32);
        wallet
            .pending
            .lock()
            .unwrap()
            .insert(req_id.clone(), (Method::LookupInvoice, tx));

        // Authentic wallet ciphertext — but published under attacker keys.
        // Responses are encrypted wallet-secret -> client-pubkey; keep that
        // direction so the ciphertext is genuinely authentic.
        let ct = nostr::nips::nip04::encrypt(
            wallet_keys.secret_key(),
            &client_keys.public_key(),
            settled_response_json(),
        )
        .unwrap();
        let attacker = Keys::generate();
        let forged = EventBuilder::new(Kind::WalletConnectResponse, ct)
            .tag(Tag::public_key(client_keys.public_key()))
            .tag(Tag::event(EventId::from_hex(&req_id).unwrap()))
            .finalize(&attacker)
            .unwrap();
        wallet.deliver(&forged).await;
        assert!(
            rx.try_recv().is_err(),
            "attacker-authored response must not fulfill a request"
        );

        // Wallet-authored but not addressed to us (missing p tag): dropped.
        let ct = nostr::nips::nip04::encrypt(
            wallet_keys.secret_key(),
            &client_keys.public_key(),
            settled_response_json(),
        )
        .unwrap();
        let wrong_recipient = EventBuilder::new(Kind::WalletConnectResponse, ct)
            .tag(Tag::event(EventId::from_hex(&req_id).unwrap()))
            .finalize(&wallet_keys)
            .unwrap();
        wallet.deliver(&wrong_recipient).await;
        assert!(
            rx.try_recv().is_err(),
            "response must carry our pubkey in p tag"
        );

        // Wallet-signed reply to a request we never sent: nothing pending.
        let ghost_id = "22".repeat(32);
        let ghost = EventBuilder::new(Kind::WalletConnectResponse, settled_response_json())
            .tag(Tag::public_key(client_keys.public_key()))
            .tag(Tag::event(EventId::from_hex(&ghost_id).unwrap()))
            .finalize(&wallet_keys)
            .unwrap();
        wallet.deliver(&ghost).await;
        assert!(rx.try_recv().is_err());
    }
}
