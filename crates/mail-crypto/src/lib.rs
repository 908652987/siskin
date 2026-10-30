//! Secret storage and authenticated encryption.
//!
//! Credentials never live in SQLite: they are addressed by a key and stored in
//! the platform keychain. [`FileStore`] exists only for development, where
//! macOS re-prompts on every ad-hoc signed rebuild. Envelope encryption is
//! available for data that must be sealed at rest (server vault, blobs).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine as _;
use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::{XChaCha20Poly1305, XNonce};
use hkdf::Hkdf;
use rand::RngCore;
use sha2::{Digest as _, Sha256};
use thiserror::Error;
use zeroize::Zeroizing;

pub const MASTER_KEY_LEN: usize = 32;

#[derive(Debug, Error)]
pub enum CryptoError {
    #[error("keychain: {0}")]
    Keychain(String),
    #[error("secret not found: {0}")]
    NotFound(String),
    #[error("invalid master key")]
    InvalidKey,
    #[error("decryption failed")]
    Decrypt,
    #[error("encoding: {0}")]
    Encoding(String),
    #[error("secret file: {0}")]
    Io(String),
}

pub type Result<T, E = CryptoError> = std::result::Result<T, E>;

/// Storage for account credentials, addressed by an opaque key.
pub trait SecretStore: Send + Sync {
    fn set(&self, key: &str, secret: &[u8]) -> Result<()>;
    fn get(&self, key: &str) -> Result<Option<Vec<u8>>>;
    fn delete(&self, key: &str) -> Result<()>;
}

/// macOS Keychain / Windows Credential Manager / Linux Secret Service.
pub struct KeyringStore {
    service: String,
}

impl KeyringStore {
    pub fn new(service: impl Into<String>) -> Self {
        Self {
            service: service.into(),
        }
    }

    fn entry(&self, key: &str) -> Result<keyring::Entry> {
        keyring::Entry::new(&self.service, key).map_err(|e| CryptoError::Keychain(e.to_string()))
    }
}

impl SecretStore for KeyringStore {
    fn set(&self, key: &str, secret: &[u8]) -> Result<()> {
        self.entry(key)?
            .set_password(&B64.encode(secret))
            .map_err(|e| CryptoError::Keychain(e.to_string()))
    }

    fn get(&self, key: &str) -> Result<Option<Vec<u8>>> {
        match self.entry(key)?.get_password() {
            Ok(value) => B64
                .decode(value)
                .map(Some)
                .map_err(|e| CryptoError::Encoding(e.to_string())),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(e) => Err(CryptoError::Keychain(e.to_string())),
        }
    }

    fn delete(&self, key: &str) -> Result<()> {
        match self.entry(key)?.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
            Err(e) => Err(CryptoError::Keychain(e.to_string())),
        }
    }
}

/// Development-only store: one 0600 file per secret inside a 0700 directory.
///
/// macOS keys keychain grants to the binary's CDHash, so every rebuild of an
/// ad-hoc signed dev binary triggers a new authorisation prompt. This store
/// lets a developer opt out of the keychain explicitly; it must never be the
/// default, because the secrets then live in plain files.
pub struct FileStore {
    dir: PathBuf,
}

impl FileStore {
    pub fn new(dir: impl Into<PathBuf>) -> Result<Self> {
        let dir = dir.into();
        std::fs::create_dir_all(&dir).map_err(|e| CryptoError::Io(e.to_string()))?;
        restrict(&dir, 0o700);
        Ok(Self { dir })
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    fn path(&self, key: &str) -> PathBuf {
        let digest = Sha256::digest(key.as_bytes());
        let name: String = digest.iter().map(|byte| format!("{byte:02x}")).collect();
        self.dir.join(name)
    }
}

impl SecretStore for FileStore {
    fn set(&self, key: &str, secret: &[u8]) -> Result<()> {
        write_private(&self.path(key), secret).map_err(|e| CryptoError::Io(e.to_string()))
    }

    fn get(&self, key: &str) -> Result<Option<Vec<u8>>> {
        match std::fs::read(self.path(key)) {
            Ok(bytes) => Ok(Some(bytes)),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(err) => Err(CryptoError::Io(err.to_string())),
        }
    }

    fn delete(&self, key: &str) -> Result<()> {
        match std::fs::remove_file(self.path(key)) {
            Ok(()) => Ok(()),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(err) => Err(CryptoError::Io(err.to_string())),
        }
    }
}

#[cfg(unix)]
fn restrict(path: &Path, mode: u32) {
    use std::os::unix::fs::PermissionsExt;
    // Best effort: the directory may live on a filesystem without permissions.
    let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode));
}

#[cfg(not(unix))]
fn restrict(_path: &Path, _mode: u32) {}

/// Creates the file with 0600 from the start, so the secret is never briefly
/// readable by other users.
fn write_private(path: &Path, data: &[u8]) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::io::Write as _;
        use std::os::unix::fs::OpenOptionsExt;
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(path)?;
        file.write_all(data)?;
        restrict(path, 0o600);
        Ok(())
    }
    #[cfg(not(unix))]
    {
        std::fs::write(path, data)
    }
}

/// In-memory store for tests and for headless runs.
#[derive(Default)]
pub struct MemoryStore {
    inner: Mutex<HashMap<String, Vec<u8>>>,
}

impl MemoryStore {
    pub fn new() -> Self {
        Self::default()
    }
}

impl SecretStore for MemoryStore {
    fn set(&self, key: &str, secret: &[u8]) -> Result<()> {
        self.inner
            .lock()
            .expect("poisoned")
            .insert(key.to_string(), secret.to_vec());
        Ok(())
    }

    fn get(&self, key: &str) -> Result<Option<Vec<u8>>> {
        Ok(self.inner.lock().expect("poisoned").get(key).cloned())
    }

    fn delete(&self, key: &str) -> Result<()> {
        self.inner.lock().expect("poisoned").remove(key);
        Ok(())
    }
}

/// 32-byte key used to wrap per-account data keys.
pub struct MasterKey(Zeroizing<[u8; MASTER_KEY_LEN]>);

impl MasterKey {
    pub fn generate() -> Self {
        let mut bytes = [0u8; MASTER_KEY_LEN];
        rand::thread_rng().fill_bytes(&mut bytes);
        Self(Zeroizing::new(bytes))
    }

    pub fn from_slice(bytes: &[u8]) -> Result<Self> {
        if bytes.len() != MASTER_KEY_LEN {
            return Err(CryptoError::InvalidKey);
        }
        let mut buf = [0u8; MASTER_KEY_LEN];
        buf.copy_from_slice(bytes);
        Ok(Self(Zeroizing::new(buf)))
    }

    pub fn as_bytes(&self) -> &[u8] {
        &*self.0
    }

    /// Derives a purpose-bound subkey (HKDF-SHA256) so one master key can feed
    /// several subsystems without key reuse. `info` separates the purposes.
    pub fn derive(&self, info: &[u8]) -> Result<Zeroizing<[u8; MASTER_KEY_LEN]>> {
        let hkdf = Hkdf::<Sha256>::new(None, self.as_bytes());
        let mut out = Zeroizing::new([0u8; MASTER_KEY_LEN]);
        hkdf.expand(info, &mut *out)
            .map_err(|_| CryptoError::InvalidKey)?;
        Ok(out)
    }

    /// Loads the key from the secret store, creating one on first use.
    pub fn load_or_create(store: &dyn SecretStore, key: &str) -> Result<Self> {
        if let Some(bytes) = store.get(key)? {
            return Self::from_slice(&bytes);
        }
        let master = Self::generate();
        store.set(key, master.as_bytes())?;
        Ok(master)
    }

    /// Base64 form for manual backup (password manager). The result is the key
    /// itself: never log it, never send it anywhere except user-chosen storage.
    pub fn encode(&self) -> Zeroizing<String> {
        Zeroizing::new(B64.encode(self.as_bytes()))
    }

    /// Parses output of [`MasterKey::encode`], tolerating surrounding whitespace.
    pub fn decode(encoded: &str) -> Result<Self> {
        let bytes = B64
            .decode(encoded.trim())
            .map_err(|e| CryptoError::Encoding(e.to_string()))?;
        Self::from_slice(&bytes)
    }
}

/// Encrypts `plaintext`; output is `nonce || ciphertext`.
pub fn seal(key: &MasterKey, plaintext: &[u8], aad: &[u8]) -> Result<Vec<u8>> {
    let cipher =
        XChaCha20Poly1305::new_from_slice(key.as_bytes()).map_err(|_| CryptoError::InvalidKey)?;
    let mut nonce = [0u8; 24];
    rand::thread_rng().fill_bytes(&mut nonce);
    let ciphertext = cipher
        .encrypt(
            XNonce::from_slice(&nonce),
            Payload {
                msg: plaintext,
                aad,
            },
        )
        .map_err(|_| CryptoError::Decrypt)?;
    let mut out = Vec::with_capacity(nonce.len() + ciphertext.len());
    out.extend_from_slice(&nonce);
    out.extend_from_slice(&ciphertext);
    Ok(out)
}

/// Decrypts output produced by [`seal`].
pub fn open(key: &MasterKey, sealed: &[u8], aad: &[u8]) -> Result<Vec<u8>> {
    if sealed.len() <= 24 {
        return Err(CryptoError::Decrypt);
    }
    let (nonce, ciphertext) = sealed.split_at(24);
    let cipher =
        XChaCha20Poly1305::new_from_slice(key.as_bytes()).map_err(|_| CryptoError::InvalidKey)?;
    cipher
        .decrypt(
            XNonce::from_slice(nonce),
            Payload {
                msg: ciphertext,
                aad,
            },
        )
        .map_err(|_| CryptoError::Decrypt)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn memory_store_roundtrip() {
        let store = MemoryStore::new();
        assert!(store.get("k").unwrap().is_none());
        store.set("k", b"secret").unwrap();
        assert_eq!(store.get("k").unwrap().unwrap(), b"secret");
        store.delete("k").unwrap();
        assert!(store.get("k").unwrap().is_none());
    }

    fn temp_dir(name: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("eyrie-{name}-{}-{nanos}", std::process::id()))
    }

    #[test]
    fn file_store_roundtrip_isolates_keys() {
        let dir = temp_dir("filestore");
        let store = FileStore::new(&dir).unwrap();
        assert!(store.get("account:a").unwrap().is_none());
        store.set("account:a", b"one").unwrap();
        store.set("account:b", b"two").unwrap();
        store.set("account:a", b"one-updated").unwrap();
        assert_eq!(store.get("account:a").unwrap().unwrap(), b"one-updated");
        assert_eq!(store.get("account:b").unwrap().unwrap(), b"two");
        store.delete("account:a").unwrap();
        assert!(store.get("account:a").unwrap().is_none());
        assert_eq!(store.get("account:b").unwrap().unwrap(), b"two");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn file_store_files_are_private() {
        use std::os::unix::fs::PermissionsExt;
        let dir = temp_dir("filemode");
        let store = FileStore::new(&dir).unwrap();
        store.set("master-key", b"secret").unwrap();
        let dir_mode = std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777;
        assert_eq!(dir_mode, 0o700);
        let files: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .collect();
        assert_eq!(files.len(), 1);
        let file_mode = std::fs::metadata(&files[0]).unwrap().permissions().mode() & 0o777;
        assert_eq!(file_mode, 0o600);
        assert_eq!(std::fs::read(&files[0]).unwrap(), b"secret");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn master_key_is_persisted() {
        let store = MemoryStore::new();
        let first = MasterKey::load_or_create(&store, "master").unwrap();
        let second = MasterKey::load_or_create(&store, "master").unwrap();
        assert_eq!(first.as_bytes(), second.as_bytes());
    }

    #[test]
    fn encoded_master_key_roundtrips_and_rejects_junk() {
        let key = MasterKey::generate();
        let encoded = key.encode();
        assert_eq!(
            MasterKey::decode(&format!("  {}\n", encoded.as_str()))
                .unwrap()
                .as_bytes(),
            key.as_bytes()
        );
        assert!(matches!(
            MasterKey::decode("not-base64!!"),
            Err(CryptoError::Encoding(_))
        ));
        assert!(matches!(
            MasterKey::decode(&B64.encode([0u8; 31])),
            Err(CryptoError::InvalidKey)
        ));
    }

    #[test]
    fn derived_keys_are_separated_by_info() {
        let key = MasterKey::generate();
        let db = key.derive(b"eyrie/sqlite/v1").unwrap();
        let again = key.derive(b"eyrie/sqlite/v1").unwrap();
        let other = key.derive(b"eyrie/attachments/v1").unwrap();

        assert_eq!(*db, *again);
        assert_ne!(*db, *other);
        assert_ne!(*db, *key.as_bytes());
    }

    #[test]
    fn seal_roundtrip_and_tamper_detection() {
        let key = MasterKey::generate();
        let sealed = seal(&key, b"token-value", b"account:1").unwrap();
        assert_eq!(open(&key, &sealed, b"account:1").unwrap(), b"token-value");
        assert!(open(&key, &sealed, b"account:2").is_err());

        let mut tampered = sealed.clone();
        let last = tampered.len() - 1;
        tampered[last] ^= 0x01;
        assert!(open(&key, &tampered, b"account:1").is_err());
    }
}
