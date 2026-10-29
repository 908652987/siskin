//! Facade: account setup, credential handling, and sync orchestration.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use mail_crypto::{CryptoError, SecretStore};
use mail_proto::{IdleEvent, ImapConfig, ImapSession, ProtoError};
use mail_store::{Account, Store, StoreError};
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
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
}

pub type Result<T, E = CoreError> = std::result::Result<T, E>;

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
        Ok(Self {
            store: Arc::new(Store::open(path)?),
            secrets,
        })
    }

    pub fn open_in_memory(secrets: Arc<dyn SecretStore>) -> Result<Self> {
        Ok(Self {
            store: Arc::new(Store::open_in_memory()?),
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
