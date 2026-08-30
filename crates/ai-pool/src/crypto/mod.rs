//! Envelope encryption logic (AES-256-GCM).
//!
//! The Vault never writes a plaintext API key to disk. Each record is sealed
//! with the 32-byte Master Key using AES-256-GCM (authenticated encryption)
//! under a fresh random 12-byte nonce. The stored blob layout is:
//!
//! ```text
//! [ 12-byte nonce | ciphertext + 16-byte GCM tag ]
//! ```
//!
//! Because GCM is *authenticated*, flipping a single byte of the `SQLite` blob
//! makes decryption fail with [`VaultError::Corrupted`] rather than yielding
//! garbage — the Vault marks that key `Banned("corrupted")` and moves on.

pub mod master_key;

pub use master_key::MasterKeyProvider;

use aes_gcm::{
    aead::{Aead, KeyInit},
    Aes256Gcm, Nonce,
};
use rand::RngCore;
use secrecy::{ExposeSecret, SecretBox, SecretString};

use crate::error::VaultError;

/// Length of the AES-GCM nonce prefix in the stored blob.
pub const NONCE_LEN: usize = 12;

/// Stateless helper that seals/opens API keys with the Master Key.
///
/// The Master Key itself lives inside [`SecretBox`] and is zeroized on drop.
pub struct Envelope {
    master: SecretBox<[u8; 32]>,
}

impl Clone for Envelope {
    fn clone(&self) -> Self {
        Self {
            master: SecretBox::new(Box::new(*self.master.expose_secret())),
        }
    }
}

impl Envelope {
    /// Wraps a resolved 32-byte master key.
    pub const fn new(master: SecretBox<[u8; 32]>) -> Self {
        Self { master }
    }

    fn cipher(&self) -> Result<Aes256Gcm, VaultError> {
        Aes256Gcm::new_from_slice(self.master.expose_secret())
            .map_err(|e| VaultError::Crypto(e.to_string()))
    }

    /// Encrypts a plaintext API key. Returns `nonce || ciphertext+tag`.
    pub fn seal(&self, plaintext: &SecretString) -> Result<Vec<u8>, VaultError> {
        let cipher = self.cipher()?;

        let mut nonce_bytes = [0u8; NONCE_LEN];
        rand::rng().fill_bytes(&mut nonce_bytes);
        let nonce = Nonce::from_slice(&nonce_bytes);

        let ciphertext = cipher
            .encrypt(nonce, plaintext.expose_secret().as_bytes())
            .map_err(|e| VaultError::Crypto(e.to_string()))?;

        let mut blob = Vec::with_capacity(NONCE_LEN + ciphertext.len());
        blob.extend_from_slice(&nonce_bytes);
        blob.extend_from_slice(&ciphertext);
        Ok(blob)
    }

    /// Decrypts a `nonce || ciphertext+tag` blob back into a [`SecretString`].
    ///
    /// Returns [`VaultError::Corrupted`] if the blob fails GCM authentication
    /// (database tampering or a rotated master key).
    pub fn open(&self, blob: &[u8]) -> Result<SecretString, VaultError> {
        if blob.len() < NONCE_LEN + 16 {
            return Err(VaultError::Corrupted("blob too short".into()));
        }
        let cipher = self.cipher()?;
        let (nonce_bytes, ciphertext) = blob.split_at(NONCE_LEN);
        let nonce = Nonce::from_slice(nonce_bytes);

        let plaintext = cipher
            .decrypt(nonce, ciphertext)
            .map_err(|_| VaultError::Corrupted("AES-GCM authentication failed".into()))?;

        let s = String::from_utf8(plaintext)
            .map_err(|_| VaultError::Corrupted("decrypted key is not valid UTF-8".into()))?;
        Ok(SecretString::from(s))
    }
}

/// Deterministic id for a plaintext key (idempotent seeding): if the builder
/// runs again after an app restart, the same key hashes to the same id and no
/// duplicate row is inserted.
pub fn deterministic_id(plaintext: &str) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(plaintext.as_bytes());
    // 16 hex chars is plenty for a local pool while staying readable.
    format!("k-{}", hex::encode(&digest[..8]))
}

/// Censors a key for UI display: `sk-proj-...8f92`.
pub fn censor(plaintext: &str) -> String {
    let chars: Vec<char> = plaintext.chars().collect();
    if chars.len() <= 8 {
        return "****".to_string();
    }
    let prefix: String = chars[..chars.len().min(7)].iter().collect();
    let suffix: String = chars[chars.len() - 4..].iter().collect();
    format!("{prefix}...{suffix}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env() -> Envelope {
        Envelope::new(SecretBox::new(Box::new([7u8; 32])))
    }

    #[test]
    fn roundtrip() {
        let e = env();
        let blob = e.seal(&SecretString::from("sk-test-123")).unwrap();
        let out = e.open(&blob).unwrap();
        assert_eq!(out.expose_secret(), "sk-test-123");
    }

    #[test]
    fn tamper_detected() {
        let e = env();
        let mut blob = e.seal(&SecretString::from("sk-test-123")).unwrap();
        let last = blob.len() - 1;
        blob[last] ^= 0xFF;
        assert!(matches!(e.open(&blob), Err(VaultError::Corrupted(_))));
    }

    #[test]
    fn nonce_uniqueness() {
        let e = env();
        let a = e.seal(&SecretString::from("same")).unwrap();
        let b = e.seal(&SecretString::from("same")).unwrap();
        assert_ne!(a, b, "two seals of the same plaintext must differ");
    }

    #[test]
    fn censor_formats() {
        assert_eq!(censor("sk-proj-abcdef8f92"), "sk-proj...8f92");
        assert_eq!(censor("short"), "****");
    }

    #[test]
    fn deterministic_ids_stable() {
        assert_eq!(deterministic_id("key1"), deterministic_id("key1"));
        assert_ne!(deterministic_id("key1"), deterministic_id("key2"));
    }
}
