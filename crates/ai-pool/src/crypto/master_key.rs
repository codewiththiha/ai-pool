//! Where the 32-byte master key that seals every API key comes from.

use std::path::{Path, PathBuf};

use rand::RngCore;
use secrecy::{ExposeSecret, SecretBox};

use crate::error::VaultError;

/// The keychain account every service stores its master key under.
#[cfg(feature = "os-keychain")]
const KEYCHAIN_ACCOUNT: &str = "master-key";

/// Source of the master encryption key.
pub enum MasterKeyProvider {
    /// The OS credential store under a service name of the app's choosing.
    #[cfg(feature = "os-keychain")]
    OsKeychain {
        /// Service name, e.g. the app's bundle id; never share it between apps.
        service: String,
    },
    /// A hex file created with owner-only permissions on first use.
    File(PathBuf),
    /// A key the caller manages.
    Custom(SecretBox<[u8; 32]>),
    /// A fresh random key per process; persisted keys never decrypt again.
    Ephemeral,
}

/// A resolved key, and whether it was created just now.
pub struct ResolvedKey {
    /// The key itself.
    pub key: SecretBox<[u8; 32]>,
    /// True when no key existed, so older records cannot decrypt.
    pub fresh: bool,
}

impl std::fmt::Debug for MasterKeyProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            #[cfg(feature = "os-keychain")]
            Self::OsKeychain { service } => write!(f, "OsKeychain({service})"),
            Self::File(path) => write!(f, "File({})", path.display()),
            Self::Custom(_) => write!(f, "Custom(<redacted>)"),
            Self::Ephemeral => write!(f, "Ephemeral"),
        }
    }
}

impl MasterKeyProvider {
    /// The OS credential store under `service`.
    #[cfg(feature = "os-keychain")]
    pub fn os_keychain(service: impl Into<String>) -> Self {
        Self::OsKeychain {
            service: service.into(),
        }
    }

    /// A caller-managed key from exactly 32 bytes.
    pub fn custom_from_slice(bytes: &[u8]) -> Result<Self, VaultError> {
        let arr: [u8; 32] = bytes.try_into().map_err(|_| VaultError::InvalidMasterKey)?;
        Ok(Self::Custom(SecretBox::new(Box::new(arr))))
    }

    /// The key, created and stored when none exists. May block or prompt.
    pub fn resolve(&self) -> Result<ResolvedKey, VaultError> {
        match self {
            Self::Custom(key) => Ok(ResolvedKey {
                key: SecretBox::new(Box::new(*key.expose_secret())),
                fresh: false,
            }),
            Self::Ephemeral => Ok(ResolvedKey {
                key: random_key(),
                fresh: true,
            }),
            Self::File(path) => resolve_file(path),
            #[cfg(feature = "os-keychain")]
            Self::OsKeychain { service } => resolve_os_keychain(service),
        }
    }
}

fn random_key() -> SecretBox<[u8; 32]> {
    let mut key = [0u8; 32];
    rand::rng().fill_bytes(&mut key);
    SecretBox::new(Box::new(key))
}

fn decode_hex(text: &str) -> Result<SecretBox<[u8; 32]>, VaultError> {
    let bytes = hex::decode(text.trim()).map_err(|_| VaultError::InvalidMasterKey)?;
    let arr: [u8; 32] = bytes.try_into().map_err(|_| VaultError::InvalidMasterKey)?;
    Ok(SecretBox::new(Box::new(arr)))
}

fn io_err(path: &Path, e: &std::io::Error) -> VaultError {
    VaultError::Storage(format!("{}: {e}", path.display()))
}

fn resolve_file(path: &Path) -> Result<ResolvedKey, VaultError> {
    match std::fs::read_to_string(path) {
        Ok(text) => Ok(ResolvedKey {
            key: decode_hex(&text)?,
            fresh: false,
        }),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            let key = random_key();
            write_private(path, &hex::encode(key.expose_secret()))?;
            Ok(ResolvedKey { key, fresh: true })
        }
        Err(e) => Err(io_err(path, &e)),
    }
}

/// Writes `text` readable by the owner only, never replacing a file.
fn write_private(path: &Path, text: &str) -> Result<(), VaultError> {
    use std::io::Write;
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent).map_err(|e| io_err(parent, &e))?;
    }
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    let mut file = options.open(path).map_err(|e| io_err(path, &e))?;
    file.write_all(text.as_bytes())
        .and_then(|()| file.sync_all())
        .map_err(|e| io_err(path, &e))
}

#[cfg(feature = "os-keychain")]
fn resolve_os_keychain(service: &str) -> Result<ResolvedKey, VaultError> {
    let entry = keyring::Entry::new(service, KEYCHAIN_ACCOUNT).map_err(|e| map_keyring_err(&e))?;
    match entry.get_password() {
        Ok(text) => Ok(ResolvedKey {
            key: decode_hex(&text)?,
            fresh: false,
        }),
        Err(keyring::Error::NoEntry) => {
            let key = random_key();
            entry
                .set_password(&hex::encode(key.expose_secret()))
                .map_err(|e| map_keyring_err(&e))?;
            Ok(ResolvedKey { key, fresh: true })
        }
        Err(e) => Err(map_keyring_err(&e)),
    }
}

#[cfg(feature = "os-keychain")]
fn map_keyring_err(e: &keyring::Error) -> VaultError {
    match e {
        keyring::Error::NoStorageAccess(_) | keyring::Error::PlatformFailure(_) => {
            VaultError::KeychainLocked(e.to_string())
        }
        _ => VaultError::Keychain(e.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ephemeral_is_random_and_fresh() {
        let a = MasterKeyProvider::Ephemeral.resolve().unwrap();
        let b = MasterKeyProvider::Ephemeral.resolve().unwrap();
        assert!(a.fresh);
        assert_ne!(a.key.expose_secret(), b.key.expose_secret());
    }

    #[test]
    fn custom_roundtrip() {
        let p = MasterKeyProvider::custom_from_slice(&[9u8; 32]).unwrap();
        let resolved = p.resolve().unwrap();
        assert!(!resolved.fresh);
        assert_eq!(resolved.key.expose_secret(), &[9u8; 32]);
    }

    #[test]
    fn custom_rejects_wrong_len() {
        assert!(matches!(
            MasterKeyProvider::custom_from_slice(&[1u8; 16]),
            Err(VaultError::InvalidMasterKey)
        ));
    }

    #[test]
    fn file_key_is_created_once_then_reused() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested").join("master.key");
        let provider = MasterKeyProvider::File(path.clone());
        let first = provider.resolve().unwrap();
        let second = provider.resolve().unwrap();
        assert!(first.fresh);
        assert!(!second.fresh);
        assert_eq!(first.key.expose_secret(), second.key.expose_secret());
        assert_eq!(std::fs::read_to_string(&path).unwrap().len(), 64);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
    }

    #[test]
    fn a_garbled_key_file_is_an_error_not_a_new_key() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("master.key");
        std::fs::write(&path, "not hex").unwrap();
        assert!(matches!(
            MasterKeyProvider::File(path).resolve(),
            Err(VaultError::InvalidMasterKey)
        ));
    }
}
