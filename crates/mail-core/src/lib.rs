//! Facade: account setup, credential handling, and sync orchestration.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use mail_crypto::{CryptoError, MasterKey, SecretStore};
use mail_proto::{IdleEvent, ImapConfig, ImapSession, ProtoError};
use mail_store::{Account, DbKey, Store, StoreError};
use mail_sync::{SyncEngine, SyncError, SyncOutcome};
use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum CoreError {
    #[error("store: {0}")]
    Store(#[from] StoreError),
    #[error("crypto: {0}")]
    Crypto(#[from] CryptoError),
    #[error("proto: {0}")]
    Proto(#[from] ProtoError),
    #[error("sync: {0}")]
    Sync(#[from] SyncError),
    #[error("account not found: {0}")]
    AccountNotFound(String),
    #[error("account has no stored secret: {0}")]
    MissingSecret(String),
    #[error("invalid account settings: {0}")]
    Settings(String),
    #[error("a different master key is already stored; pass --force to replace it")]
    KeyConflict,
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
}

pub type Result<T, E = CoreError> = std::result::Result<T, E>;

/// Keychain entry for the master key, created on first open.
const MASTER_KEY_REF: &str = "master-key";
/// HKDF info string; changing it makes existing databases unreadable.
const DB_KEY_INFO: &[u8] = b"eyrie/sqlite/v1";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerSettings {
    pub host: String,
    pub port: u16,
    pub tls: bool,
    pub username: String,
}

#[derive(Debug, Clone)]
pub struct AccountSpec {
    pub email: String,
    pub display_name: Option<String>,
    pub host: String,
    pub port: u16,
    pub tls: bool,
    pub username: String,
    pub password: String,
}

pub struct Core {
    store: Arc<Store>,
    secrets: Arc<dyn SecretStore>,
}

impl Core {
    pub fn open(path: impl AsRef<Path>, secrets: Arc<dyn SecretStore>) -> Result<Self> {
        let store = Store::open(path, &db_key(secrets.as_ref())?)?;
        Ok(Self {
            store: Arc::new(store),
            secrets,
        })
    }

    pub fn open_in_memory(secrets: Arc<dyn SecretStore>) -> Result<Self> {
        let store = Store::open_in_memory(&db_key(secrets.as_ref())?)?;
        Ok(Self {
            store: Arc::new(store),
            secrets,
        })
    }

    pub fn store(&self) -> Arc<Store> {
        Arc::clone(&self.store)
    }

    pub fn add_account(&self, spec: &AccountSpec) -> Result<i64> {
        let secret_ref = secret_key(&spec.email);
        self.secrets.set(&secret_ref, spec.password.as_bytes())?;
        let settings = ServerSettings {
            host: spec.host.clone(),
            port: spec.port,
            tls: spec.tls,
            username: spec.username.clone(),
        };
        let id = self.store.create_account(
            "imap",
            &spec.email,
            spec.display_name.as_deref(),
            &secret_ref,
            &serde_json::to_value(&settings)?,
        )?;
        Ok(id)
    }

    pub async fn sync(&self, email: &str, mailbox: &str) -> Result<Vec<SyncOutcome>> {
        let account = self
            .store
            .account_by_email(email)?
            .ok_or_else(|| CoreError::AccountNotFound(email.to_string()))?;
        let mut session = self.connect(&account).await?;
        let engine = SyncEngine::new(Arc::clone(&self.store));
        let outcome = engine
            .sync_mailbox(account.id, &mut session, mailbox)
            .await?;
        session.logout().await;
        Ok(vec![outcome])
    }

    /// Keeps a mailbox in sync: initial sync, then IDLE with reconnect/backoff.
    pub async fn watch<F>(
        &self,
        email: &str,
        mailbox: &str,
        max_events: usize,
        mut on_event: F,
    ) -> Result<()>
    where
        F: FnMut(&SyncOutcome),
    {
        let account = self
            .store
            .account_by_email(email)?
            .ok_or_else(|| CoreError::AccountNotFound(email.to_string()))?;
        let engine = SyncEngine::new(Arc::clone(&self.store));
        let mut backoff = Duration::from_secs(1);
        let mut delivered = 0usize;

        loop {
            let mut session = match self.connect(&account).await {
                Ok(session) => {
                    backoff = Duration::from_secs(1);
                    session
                }
                Err(err) => {
                    tracing::warn!(%err, "connect failed, retrying");
                    tokio::time::sleep(backoff).await;
                    backoff = (backoff * 2).min(Duration::from_secs(60));
                    continue;
                }
            };

            match engine.sync_mailbox(account.id, &mut session, mailbox).await {
                Ok(outcome) => {
                    on_event(&outcome);
                    delivered += 1;
                    if delivered >= max_events {
                        session.logout().await;
                        return Ok(());
                    }
                }
                Err(err) => {
                    tracing::warn!(%err, "initial sync failed, reconnecting");
                    session.logout().await;
                    tokio::time::sleep(backoff).await;
                    backoff = (backoff * 2).min(Duration::from_secs(60));
                    continue;
                }
            }

            loop {
                match session.idle_wait(Duration::from_secs(29 * 60)).await {
                    Ok(IdleEvent::NewData(_)) => {
                        match engine.sync_mailbox(account.id, &mut session, mailbox).await {
                            Ok(outcome) => {
                                on_event(&outcome);
                                delivered += 1;
                                if delivered >= max_events {
                                    session.logout().await;
                                    return Ok(());
                                }
                            }
                            Err(err) => {
                                tracing::warn!(%err, "sync after idle failed, reconnecting");
                                break;
                            }
                        }
                    }
                    Ok(IdleEvent::Timeout) => continue,
                    Ok(IdleEvent::Interrupted) => break,
                    Err(err) => {
                        tracing::warn!(%err, "idle failed, reconnecting");
                        break;
                    }
                }
            }

            session.logout().await;
            tokio::time::sleep(backoff).await;
            backoff = (backoff * 2).min(Duration::from_secs(60));
        }
    }

    pub async fn connect(&self, account: &Account) -> Result<ImapSession> {
        let settings: ServerSettings = serde_json::from_value(account.settings.clone())
            .map_err(|e| CoreError::Settings(e.to_string()))?;
        let secret = self
            .secrets
            .get(&account.secret_ref)?
            .ok_or_else(|| CoreError::MissingSecret(account.secret_ref.clone()))?;
        let password = String::from_utf8(secret)
            .map_err(|_| CoreError::Settings("password is not UTF-8".into()))?;

        let (session, _caps) = ImapSession::connect(&ImapConfig {
            host: settings.host,
            port: settings.port,
            tls: settings.tls,
            username: settings.username,
            password,
        })
        .await?;
        Ok(session)
    }
}

fn secret_key(email: &str) -> String {
    format!("account:{email}:password")
}

/// Master key lives in the keychain; the database key is derived from it, so
/// the same bytes never encrypt both database pages and sealed blobs.
fn db_key(secrets: &dyn SecretStore) -> Result<DbKey> {
    let master = MasterKey::load_or_create(secrets, MASTER_KEY_REF)?;
    db_key_from(&master)
}

fn db_key_from(master: &MasterKey) -> Result<DbKey> {
    let derived = master.derive(DB_KEY_INFO)?;
    Ok(DbKey::from_bytes(&derived[..])?)
}

/// Base64 form of the stored master key, for manual backup. Fails when no key
/// has been created yet (the first [`Core::open`] creates one).
pub fn export_master_key(secrets: &dyn SecretStore) -> Result<String> {
    let stored = secrets
        .get(MASTER_KEY_REF)?
        .ok_or_else(|| CoreError::MissingSecret(MASTER_KEY_REF.to_string()))?;
    Ok(MasterKey::from_slice(&stored)?.encode().to_string())
}

/// Replaces the stored master key with `encoded`. When `database` exists and
/// already holds data, the imported key must open it, otherwise nothing is
/// written. A different existing key is replaced only with `force`.
///
/// Returns whether the keychain was updated.
pub fn import_master_key(
    secrets: &dyn SecretStore,
    encoded: &str,
    database: Option<&Path>,
    force: bool,
) -> Result<bool> {
    let master = MasterKey::decode(encoded)?;
    let key = db_key_from(&master)?;
    if let Some(path) = database {
        if path.exists() {
            Store::open(path, &key)?;
        }
    }

    if let Some(existing) = secrets.get(MASTER_KEY_REF)? {
        if existing == master.as_bytes() {
            return Ok(false);
        }
        if !force {
            return Err(CoreError::KeyConflict);
        }
    }
    secrets.set(MASTER_KEY_REF, master.as_bytes())?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use mail_crypto::MemoryStore;

    fn temp_db_path(name: &str) -> std::path::PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!(
            "siskin-core-{name}-{}-{nanos}.db",
            std::process::id()
        ))
    }

    fn remove_db_files(path: &Path) {
        for suffix in ["", "-wal", "-shm"] {
            let mut target = path.as_os_str().to_owned();
            target.push(suffix);
            let _ = std::fs::remove_file(std::path::PathBuf::from(target));
        }
    }

    fn store_with_account(path: &Path, master: &MasterKey) {
        let store = Store::open(path, &db_key_from(master).unwrap()).unwrap();
        let account = store
            .create_account(
                "imap",
                "test@example.com",
                None,
                "account:test",
                &serde_json::json!({}),
            )
            .unwrap();
        store
            .ensure_mailbox(account, "INBOX", Some("inbox"))
            .unwrap();
    }

    #[test]
    fn export_requires_an_existing_key() {
        let secrets = MemoryStore::new();
        assert!(matches!(
            export_master_key(&secrets),
            Err(CoreError::MissingSecret(_))
        ));
    }

    #[test]
    fn exported_key_survives_reimport() {
        let source = MemoryStore::new();
        let master = MasterKey::generate();
        source.set(MASTER_KEY_REF, master.as_bytes()).unwrap();
        let encoded = export_master_key(&source).unwrap();

        let target = MemoryStore::new();
        assert!(import_master_key(&target, &encoded, None, false).unwrap());
        assert_eq!(export_master_key(&target).unwrap(), encoded);
        assert!(
            !import_master_key(&target, &encoded, None, false).unwrap(),
            "re-importing the same key must not rewrite the keychain"
        );
    }

    #[test]
    fn a_different_key_needs_force() {
        let secrets = MemoryStore::new();
        let first = MasterKey::generate();
        secrets.set(MASTER_KEY_REF, first.as_bytes()).unwrap();
        let other = MasterKey::generate().encode().to_string();

        assert!(matches!(
            import_master_key(&secrets, &other, None, false),
            Err(CoreError::KeyConflict)
        ));
        assert_eq!(
            export_master_key(&secrets).unwrap(),
            first.encode().to_string()
        );

        assert!(import_master_key(&secrets, &other, None, true).unwrap());
        assert_eq!(export_master_key(&secrets).unwrap(), other);
    }

    #[test]
    fn import_is_refused_when_the_key_cannot_open_the_database() {
        let path = temp_db_path("import-wrong");
        store_with_account(&path, &MasterKey::generate());

        let secrets = MemoryStore::new();
        let stranger = MasterKey::generate().encode().to_string();
        assert!(matches!(
            import_master_key(&secrets, &stranger, Some(&path), false),
            Err(CoreError::Store(StoreError::WrongKey))
        ));
        assert!(
            secrets.get(MASTER_KEY_REF).unwrap().is_none(),
            "a rejected import must not write the keychain"
        );
        remove_db_files(&path);
    }

    #[test]
    fn import_verifies_against_the_database_and_then_opens_it() {
        let path = temp_db_path("import-ok");
        let master = MasterKey::generate();
        store_with_account(&path, &master);

        let secrets: Arc<dyn SecretStore> = Arc::new(MemoryStore::new());
        assert!(import_master_key(secrets.as_ref(), &master.encode(), Some(&path), false).unwrap());

        let core = Core::open(&path, Arc::clone(&secrets)).unwrap();
        assert_eq!(core.store().list_accounts().unwrap().len(), 1);
        remove_db_files(&path);
    }
}
