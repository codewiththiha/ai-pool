//! `MasterKeyProvider`: where the 32-byte AES-256 master key comes from.

use rand::RngCore;
use secrecy::{ExposeSecret, SecretBox};

use crate::error::VaultError;

#[cfg(feature = "os-keychain")]
const KEYCHAIN_SERVICE: &str = "ai-pool";
#[cfg(feature = "os-keychain")]
const KEYCHAIN_USER: &str = "master-key";

/// Source of the Master Encryption Key that seals the on-disk vault.
pub enum MasterKeyProvider {
    /// Fetches/stores a single 32-byte key in the macOS Keychain / Windows
    /// Credential Manager / Secret Service. Created on first run.
    ///
    /// Requires the `os-keychain` cargo feature.
    #[cfg(feature = "os-keychain")]
    OsKeychain,
    /// Developer passes their own 32-byte key (e.g. derived from a passphrase
    /// or read from a custom config file).
    Custom(SecretBox<[u8; 32]>),
    /// Generates a random key in RAM on startup. Used automatically for
    /// `MemoryStore`; with a persistent store this makes old rows unreadable
    /// after restart, so it is only suitable for ephemeral sessions.
    Ephemeral,
}

impl std::fmt::Debug for MasterKeyProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            #[cfg(feature = "os-keychain")]
            Self::OsKeychain => write!(f, "MasterKeyProvider::OsKeychain"),
            Self::Custom(_) => write!(f, "MasterKeyProvider::Custom(<redacted>)"),
            Self::Ephemeral => write!(f, "MasterKeyProvider::Ephemeral"),
        }
    }
}

impl MasterKeyProvider {
    /// Resolves the provider into the actual 32-byte key.
    ///
    /// For `OsKeychain`, a missing entry is generated and persisted on first
    /// run. A locked keychain maps to [`VaultError::KeychainLocked`] so the
    /// application can prompt the user to unlock it rather than failing
    /// with an opaque error.
    pub fn resolve(&self) -> Result<SecretBox<[u8; 32]>, VaultError> {
        match self {
            Self::Custom(key) => Ok(SecretBox::new(Box::new(*key.expose_secret()))),
            Self::Ephemeral => {
                let mut key = [0u8; 32];
                rand::rng().fill_bytes(&mut key);
                Ok(SecretBox::new(Box::new(key)))
            }
            #[cfg(feature = "os-keychain")]
            Self::OsKeychain => resolve_os_keychain(),
        }
    }

    /// Convenience constructor validating an arbitrary byte slice.
    pub fn custom_from_slice(bytes: &[u8]) -> Result<Self, VaultError> {
        let arr: [u8; 32] = bytes.try_into().map_err(|_| VaultError::InvalidMasterKey)?;
        Ok(Self::Custom(SecretBox::new(Box::new(arr))))
    }
}

#[cfg(feature = "os-keychain")]
fn resolve_os_keychain() -> Result<SecretBox<[u8; 32]>, VaultError> {
    use keyring::Entry;

    let entry = Entry::new(KEYCHAIN_SERVICE, KEYCHAIN_USER)
        .map_err(|e| VaultError::Keychain(e.to_string()))?;

    match entry.get_password() {
        Ok(hex_key) => {
            let bytes = hex::decode(hex_key.trim())
                .map_err(|_| VaultError::Keychain("master key entry is not valid hex".into()))?;
            let arr: [u8; 32] = bytes
                .try_into()
                .map_err(|_| VaultError::Keychain("master key entry is not 32 bytes".into()))?;
            Ok(SecretBox::new(Box::new(arr)))
        }
        Err(keyring::Error::NoEntry) => {
            // First run: generate and persist a fresh master key.
            let mut key = [0u8; 32];
            rand::rng().fill_bytes(&mut key);
            entry
                .set_password(&hex::encode(key))
                .map_err(|e| map_keyring_err(&e))?;
            Ok(SecretBox::new(Box::new(key)))
        }
        Err(e) => Err(map_keyring_err(&e)),
    }
}

#[cfg(feature = "os-keychain")]
fn map_keyring_err(e: &keyring::Error) -> VaultError {
    // Locked keychains surface as platform failures; detect the common cases
    // so the app can render a dedicated "please unlock" UI.
    let msg = e.to_string().to_lowercase();
    if msg.contains("lock") || msg.contains("denied") || msg.contains("interaction") {
        VaultError::KeychainLocked
    } else {
        VaultError::Keychain(e.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ephemeral_is_random() {
        let a = MasterKeyProvider::Ephemeral.resolve().unwrap();
        let b = MasterKeyProvider::Ephemeral.resolve().unwrap();
        assert_ne!(a.expose_secret(), b.expose_secret());
    }

    #[test]
    fn custom_roundtrip() {
        let p = MasterKeyProvider::custom_from_slice(&[9u8; 32]).unwrap();
        assert_eq!(p.resolve().unwrap().expose_secret(), &[9u8; 32]);
    }

    #[test]
    fn custom_rejects_wrong_len() {
        assert!(matches!(
            MasterKeyProvider::custom_from_slice(&[1u8; 16]),
            Err(VaultError::InvalidMasterKey)
        ));
    }
}
