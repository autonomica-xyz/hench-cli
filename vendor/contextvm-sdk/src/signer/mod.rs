//! Nostr signer utilities.
//!
//! Re-exports key types from nostr-sdk and provides convenience constructors.

use std::fmt::Debug;
use std::future::Future;
use std::pin::Pin;

pub use nostr_sdk::prelude::{Keys, PublicKey};
use nostr_sdk::prelude::{AsyncGetPublicKey, AsyncNip44, AsyncSignEvent, Event, UnsignedEvent};

/// Signer capabilities the ContextVM SDK requires.
///
/// nostr 0.45 removed the monolithic `NostrSigner` trait (and `IntoNostrSigner`),
/// replacing it with granular async traits. This object-safe trait re-bundles the
/// capabilities the SDK needs (`AsyncGetPublicKey` + `AsyncSignEvent` +
/// `AsyncNip44`) behind its own methods so `Arc<dyn ContextVmSigner>` works where
/// `Arc<dyn NostrSigner>` used to — supertrait composition alone is not dyn-safe
/// because the upstream traits carry unconstrained `Error` associated types.
/// Implemented automatically for `Keys` and every signer satisfying the
/// supertraits.
pub trait ContextVmSigner: Debug + Send + Sync + 'static {
    /// Get the signer's public key.
    fn get_public_key<'a>(
        &'a self,
    ) -> Pin<Box<dyn Future<Output = Result<PublicKey, nostr_sdk::prelude::Error>> + Send + 'a>>;

    /// Sign an unsigned event.
    fn sign_event<'a>(
        &'a self,
        unsigned: UnsignedEvent,
    ) -> Pin<Box<dyn Future<Output = Result<Event, nostr_sdk::prelude::Error>> + Send + 'a>>;

    /// NIP-44 encrypt `content` for `public_key`.
    fn nip44_encrypt<'a>(
        &'a self,
        public_key: &'a PublicKey,
        content: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<String, nostr_sdk::prelude::Error>> + Send + 'a>>;

    /// NIP-44 decrypt `payload` from `public_key`.
    fn nip44_decrypt<'a>(
        &'a self,
        public_key: &'a PublicKey,
        payload: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<String, nostr_sdk::prelude::Error>> + Send + 'a>>;
}

impl<T> ContextVmSigner for T
where
    T: AsyncGetPublicKey + AsyncSignEvent + AsyncNip44,
{
    fn get_public_key<'a>(
        &'a self,
    ) -> Pin<Box<dyn Future<Output = Result<PublicKey, nostr_sdk::prelude::Error>> + Send + 'a>>
    {
        Box::pin(async move {
            self.get_public_key_async()
                .await
                .map_err(nostr_sdk::prelude::Error::other)
        })
    }

    fn sign_event<'a>(
        &'a self,
        unsigned: UnsignedEvent,
    ) -> Pin<Box<dyn Future<Output = Result<Event, nostr_sdk::prelude::Error>> + Send + 'a>> {
        Box::pin(async move {
            self.sign_event_async(unsigned)
                .await
                .map_err(nostr_sdk::prelude::Error::other)
        })
    }

    fn nip44_encrypt<'a>(
        &'a self,
        public_key: &'a PublicKey,
        content: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<String, nostr_sdk::prelude::Error>> + Send + 'a>> {
        Box::pin(async move {
            self.nip44_encrypt_async(public_key, content)
                .await
                .map_err(nostr_sdk::prelude::Error::other)
        })
    }

    fn nip44_decrypt<'a>(
        &'a self,
        public_key: &'a PublicKey,
        payload: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<String, nostr_sdk::prelude::Error>> + Send + 'a>> {
        Box::pin(async move {
            self.nip44_decrypt_async(public_key, payload)
                .await
                .map_err(nostr_sdk::prelude::Error::other)
        })
    }
}

/// Create keys from a private key string (hex or nsec/bech32).
pub fn from_sk(sk: &str) -> std::result::Result<Keys, nostr::error::Error> {
    Keys::parse(sk)
}

/// Generate a new random keypair.
pub fn generate() -> Keys {
    Keys::generate()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_generate_produces_valid_keys() {
        let keys = generate();
        let pubkey = keys.public_key();
        // Public key hex should be 64 chars
        assert_eq!(pubkey.to_hex().len(), 64);
    }

    #[test]
    fn test_generate_produces_unique_keys() {
        let k1 = generate();
        let k2 = generate();
        assert_ne!(k1.public_key(), k2.public_key());
    }

    #[test]
    fn test_from_sk_hex() {
        let keys = generate();
        let sk_hex = keys.secret_key().to_secret_hex();
        let restored = from_sk(&sk_hex).unwrap();
        assert_eq!(restored.public_key(), keys.public_key());
    }

    #[test]
    fn test_from_sk_nsec() {
        let keys = generate();
        use nostr_sdk::prelude::ToBech32;
        let nsec = keys.secret_key().to_bech32().unwrap();
        let restored = from_sk(&nsec).unwrap();
        assert_eq!(restored.public_key(), keys.public_key());
    }

    #[test]
    fn test_from_sk_invalid() {
        assert!(from_sk("not_a_valid_key").is_err());
        assert!(from_sk("").is_err());
    }
}
