//! Long-lived ed25519 device identity, persisted through a [`KeyStore`].
use anyhow::{Context, Result, bail};
use ed25519_dalek::SigningKey;
use ed25519_dalek::pkcs8::EncodePrivateKey;
use novalink_proto::DeviceId;
use std::sync::Mutex;

const SERVICE: &str = "com.novalink.agent";
const ACCOUNT: &str = "device-key";

/// Where the 32-byte secret seed lives.
pub trait KeyStore {
    fn get(&self) -> Result<Option<Vec<u8>>>;
    fn set(&self, secret: &[u8]) -> Result<()>;
}

/// In-memory store for tests; never touches the OS keychain.
#[derive(Default)]
pub struct MemoryStore(Mutex<Option<Vec<u8>>>);

impl KeyStore for MemoryStore {
    fn get(&self) -> Result<Option<Vec<u8>>> {
        Ok(self
            .0
            .lock()
            .map_err(|_| anyhow::anyhow!("poisoned"))?
            .clone())
    }
    fn set(&self, secret: &[u8]) -> Result<()> {
        *self.0.lock().map_err(|_| anyhow::anyhow!("poisoned"))? = Some(secret.to_vec());
        Ok(())
    }
}

/// OS keychain store (service `com.novalink.agent`, account `device-key`).
pub struct KeyringStore;

impl KeyringStore {
    fn entry() -> Result<keyring::Entry> {
        keyring::Entry::new(SERVICE, ACCOUNT).context("open keychain entry")
    }
}

impl KeyStore for KeyringStore {
    fn get(&self) -> Result<Option<Vec<u8>>> {
        match Self::entry()?.get_secret() {
            Ok(s) => Ok(Some(s)),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(e) => Err(e).context("read device key from keychain"),
        }
    }
    fn set(&self, secret: &[u8]) -> Result<()> {
        Self::entry()?
            .set_secret(secret)
            .context("write device key to keychain")
    }
}

pub struct Identity {
    key: SigningKey,
}

impl Identity {
    /// Load the stored key, or generate and store a new one.
    pub fn load_or_create(store: &dyn KeyStore) -> Result<Identity> {
        if let Some(bytes) = store.get()? {
            let Ok(seed) = <[u8; 32]>::try_from(bytes) else {
                bail!("stored device key has wrong length");
            };
            return Ok(Identity {
                key: SigningKey::from_bytes(&seed),
            });
        }
        let key = SigningKey::generate(&mut rand_core::OsRng);
        store.set(&key.to_bytes())?;
        Ok(Identity { key })
    }

    pub fn device_id(&self) -> DeviceId {
        DeviceId(self.key.verifying_key().to_bytes())
    }

    /// PKCS#8 DER of the private key (for building the TLS certificate).
    pub fn pkcs8_der(&self) -> Vec<u8> {
        self.key
            .to_pkcs8_der()
            .expect("ed25519 pkcs8 encoding is infallible")
            .as_bytes()
            .to_vec()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn identity_persists() -> anyhow::Result<()> {
        let s = MemoryStore::default();
        assert_eq!(
            Identity::load_or_create(&s)?.device_id(),
            Identity::load_or_create(&s)?.device_id()
        );
        Ok(())
    }

    #[test]
    fn pkcs8_is_stable_and_bad_length_rejected() -> anyhow::Result<()> {
        let s = MemoryStore::default();
        assert_eq!(
            Identity::load_or_create(&s)?.pkcs8_der(),
            Identity::load_or_create(&s)?.pkcs8_der()
        );
        let bad = MemoryStore::default();
        bad.set(&[1, 2, 3])?;
        assert!(Identity::load_or_create(&bad).is_err());
        Ok(())
    }

    /// Manual: touches the real OS keychain. Run with `cargo test -- --ignored`.
    #[test]
    #[ignore = "manual: uses the real OS keychain"]
    fn keyring_store_roundtrip() -> anyhow::Result<()> {
        let a = Identity::load_or_create(&KeyringStore)?.device_id();
        let b = Identity::load_or_create(&KeyringStore)?.device_id();
        assert_eq!(a, b);
        Ok(())
    }
}
