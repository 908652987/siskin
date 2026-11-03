//! Facade: account setup, credential handling, and sync orchestration.

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;

use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use chrono::Utc;
use mail_crypto::{CryptoError, MasterKey, SecretStore};
use mail_proto::pop3::{Pop3Config, Pop3Session};
use mail_proto::smtp::{SmtpConfig, SmtpSession};
use mail_proto::{IdleEvent, ImapConfig, ImapSession, ProtoError, Security, Tier};
use mail_store::{
    Account, AttachmentMetaInput, BodyPartInput, BodyState, ChangeWindow, Compaction, DbKey,
    LocationTarget, Mailbox, MessageBody, MessageContext, PartMetaInput, PurgeStats, Store,
    StoreError,
};
use mail_sync::{plan_mailboxes, pop3_uid, MailboxTarget, Pop3SyncEngine, SyncEngine, SyncError};
pub use mail_sync::{AccountSyncReport, SyncOutcome};
use tokio::sync::watch;
use tokio::task::{AbortHandle, JoinSet};
use uuid::Uuid;

pub mod presets;
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
    #[error("message not found: {0}")]
    MessageNotFound(i64),
    #[error("the server no longer lists message uid {0}")]
    MessageGone(i64),
    #[error("message {message_id} has no attachment part {part}")]
    AttachmentNotFound { message_id: i64, part: String },
    #[error("operation not supported: {0}")]
    Unsupported(&'static str),
    #[error("message {0} is stored in more than one mailbox; this write needs a single copy")]
    MultipleLocations(i64),
    #[error("unknown keyword: {0}")]
    UnknownKeyword(String),
    #[error("unknown mailbox: {0}")]
    MailboxNotFound(i64),
}

pub type Result<T, E = CoreError> = std::result::Result<T, E>;

/// Keychain entry for the master key, created on first open.
const MASTER_KEY_REF: &str = "master-key";
/// HKDF info string; changing it makes existing databases unreadable.
const DB_KEY_INFO: &[u8] = b"eyrie/sqlite/v1";

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ReceiveProtocol {
    #[default]
    Imap,
    Pop3,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SecurityMode {
    #[default]
    Tls,
    StartTls,
    Plaintext,
}

impl From<SecurityMode> for Security {
    fn from(mode: SecurityMode) -> Self {
        match mode {
            SecurityMode::Tls => Security::Tls,
            SecurityMode::StartTls => Security::StartTls,
            SecurityMode::Plaintext => Security::Plaintext,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerSettings {
    pub host: String,
    pub port: u16,
    pub username: String,
    #[serde(default)]
    pub protocol: ReceiveProtocol,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub security: Option<SecurityMode>,
    /// Legacy field from password-era accounts; `false` meant plaintext.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tls: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub smtp: Option<SmtpSettings>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sent_mailbox: Option<String>,
}

impl ServerSettings {
    fn security(&self) -> SecurityMode {
        match (self.security, self.tls) {
            (Some(mode), _) => mode,
            (None, Some(false)) => SecurityMode::Plaintext,
            (None, _) => SecurityMode::Tls,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SmtpSettings {
    pub host: String,
    pub port: u16,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub security: Option<SecurityMode>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub username: Option<String>,
}

impl SmtpSettings {
    pub fn security(&self) -> SecurityMode {
        self.security.unwrap_or(SecurityMode::Tls)
    }
}

#[derive(Debug, Clone)]
pub struct AccountSpec {
    pub email: String,
    pub display_name: Option<String>,
    pub provider: String,
    pub host: String,
    pub port: u16,
    pub username: String,
    pub password: String,
    pub protocol: ReceiveProtocol,
    pub security: SecurityMode,
    pub smtp: Option<SmtpSettings>,
    pub sent_mailbox: Option<String>,
}

/// One attachment as reported by the cached layout.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AttachmentInfo {
    pub part: String,
    pub filename: Option<String>,
    pub content_type: String,
    pub size: i64,
    pub downloaded: bool,
}

/// Cached content of one message. `text` always exists for messages that have
/// a body: HTML-only mail is rendered to text, so lists and search have text.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MessageContent {
    pub message_id: i64,
    pub body_state: BodyState,
    pub text: Option<String>,
    pub html: Option<String>,
    pub attachments: Vec<AttachmentInfo>,
}

#[derive(Debug, Clone)]
pub struct SendOutcome {
    pub recipients: usize,
    pub saved_to: Option<String>,
    pub save_error: Option<String>,
}

/// Connection budget of the multi-mailbox watcher.
#[derive(Debug, Clone, Copy)]
pub struct WatchOptions {
    /// Mailboxes that get a dedicated IDLE session; the rest are polled.
    pub idle_sessions: usize,
    /// Seconds between sweeps for mailboxes without a session of their own.
    pub sweep_seconds: u64,
}

impl Default for WatchOptions {
    fn default() -> Self {
        // Gmail allows 15 IMAP connections per account; one sweep plus four
        // IDLE sessions stays far below that with several accounts open.
        Self {
            idle_sessions: 4,
            sweep_seconds: 120,
        }
    }
}

/// Event budget and stop signal shared by every session of one account watcher.
struct WatchControl {
    max_events: usize,
    delivered: AtomicUsize,
    stop: watch::Sender<bool>,
}

impl WatchControl {
    fn new(max_events: usize) -> Self {
        let (stop, _receiver) = watch::channel(false);
        Self {
            max_events,
            delivered: AtomicUsize::new(0),
            stop,
        }
    }

    fn subscribe(&self) -> watch::Receiver<bool> {
        self.stop.subscribe()
    }

    /// Counts one delivered event and stops every watcher once the budget is
    /// spent. Returns true when this event exhausted the budget.
    fn record(&self) -> bool {
        let delivered = self.delivered.fetch_add(1, Ordering::Relaxed) + 1;
        let spent = delivered >= self.max_events;
        if spent {
            self.stop.send_replace(true);
        }
        spent
    }
}

fn emit<F>(sink: &StdMutex<F>, outcome: &SyncOutcome)
where
    F: FnMut(&SyncOutcome),
{
    let mut callback = sink.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    callback(outcome);
}

/// Whether an outcome is worth forwarding: new or dropped messages, or a
/// mailbox that was rebuilt or newly discovered. Flag-only outcomes are not
/// forwarded because a periodic full flag rescan looks exactly like a real
/// change; consumers read those through the change log instead.
fn is_event(outcome: &SyncOutcome) -> bool {
    outcome.inserted > 0 || outcome.removed > 0 || outcome.full
}

/// Sleeps unless the watcher is asked to stop first; returns true when stopping.
async fn sleep_or_stop(stop: &mut watch::Receiver<bool>, delay: Duration) -> bool {
    if *stop.borrow() {
        return true;
    }
    tokio::select! {
        _ = stop.changed() => true,
        _ = tokio::time::sleep(delay) => false,
    }
}

/// Keeps one mailbox live: sync, then wait on IDLE, reconnecting with backoff
/// when the session dies. Stopping the task drops the connection, because an
/// IDLE session cannot be handed back while it is waiting.
async fn watch_mailbox_worker<F>(
    core: Core,
    account: Account,
    mailbox: String,
    control: Arc<WatchControl>,
    sink: Arc<StdMutex<F>>,
    mut stop: watch::Receiver<bool>,
) where
    F: FnMut(&SyncOutcome) + Send + 'static,
{
    let engine = SyncEngine::new(core.store());
    let mut backoff = Duration::from_secs(1);
    loop {
        if *stop.borrow() {
            return;
        }
        let mut session = match core.connect_imap(&account).await {
            Ok(session) => {
                backoff = Duration::from_secs(1);
                session
            }
            Err(err) => {
                tracing::warn!(%err, mailbox, "connect failed, retrying");
                if sleep_or_stop(&mut stop, backoff).await {
                    return;
                }
                backoff = (backoff * 2).min(Duration::from_secs(60));
                continue;
            }
        };

        match engine
            .sync_mailbox(account.id, &mut session, &mailbox)
            .await
        {
            Ok(outcome) => {
                core.compact_quietly(account.id);
                if is_event(&outcome) {
                    emit(&sink, &outcome);
                    if control.record() {
                        session.logout().await;
                        return;
                    }
                }
            }
            Err(err) => {
                tracing::warn!(%err, mailbox, "initial sync failed, reconnecting");
                session.logout().await;
                if sleep_or_stop(&mut stop, backoff).await {
                    return;
                }
                backoff = (backoff * 2).min(Duration::from_secs(60));
                continue;
            }
        }

        loop {
            if *stop.borrow() {
                return;
            }
            let event = tokio::select! {
                _ = stop.changed() => return,
                event = session.idle_wait(Duration::from_secs(29 * 60)) => event,
            };
            match event {
                Ok(IdleEvent::NewData(_)) => {
                    match engine
                        .sync_mailbox(account.id, &mut session, &mailbox)
                        .await
                    {
                        Ok(outcome) => {
                            core.compact_quietly(account.id);
                            if is_event(&outcome) {
                                emit(&sink, &outcome);
                                if control.record() {
                                    session.logout().await;
                                    return;
                                }
                            }
                        }
                        Err(err) => {
                            tracing::warn!(%err, mailbox, "sync after idle failed, reconnecting");
                            break;
                        }
                    }
                }
                Ok(IdleEvent::Timeout) => continue,
                Ok(IdleEvent::Interrupted) => break,
                Err(err) => {
                    tracing::warn!(%err, mailbox, "idle failed, reconnecting");
                    break;
                }
            }
        }

        session.logout().await;
        if sleep_or_stop(&mut stop, backoff).await {
            return;
        }
        backoff = (backoff * 2).min(Duration::from_secs(60));
    }
}

/// Account-level supervisor: one sweep connection re-plans the mailbox set and
/// syncs everything without a session of its own, while IDLE workers cover the
/// mailboxes the server can push for.
struct AccountWatcher<F> {
    core: Core,
    account: Account,
    options: WatchOptions,
    control: Arc<WatchControl>,
    sink: Arc<StdMutex<F>>,
    workers: HashMap<String, AbortHandle>,
    tasks: JoinSet<()>,
}

impl<F> AccountWatcher<F>
where
    F: FnMut(&SyncOutcome) + Send + 'static,
{
    async fn run(&mut self) -> Result<()> {
        let mut stop = self.control.subscribe();
        let mut backoff = Duration::from_secs(1);
        while !*stop.borrow() {
            match self.core.connect_imap(&self.account).await {
                Ok(mut session) => {
                    backoff = Duration::from_secs(1);
                    let tier = session.tier();
                    let listed = match session.list_mailboxes().await {
                        Ok(listed) => listed,
                        Err(err) => {
                            tracing::warn!(%err, "listing mailboxes failed, reconnecting");
                            session.logout().await;
                            if sleep_or_stop(&mut stop, backoff).await {
                                break;
                            }
                            backoff = (backoff * 2).min(Duration::from_secs(60));
                            continue;
                        }
                    };
                    let targets = plan_mailboxes(&listed);
                    let present: HashSet<&str> =
                        listed.iter().map(|info| info.name.as_str()).collect();
                    self.replan(tier, &targets, &present);

                    let watched: Vec<String> = self.workers.keys().cloned().collect();
                    let sweep = SyncEngine::new(self.core.store())
                        .sync_account_except(self.account.id, &mut session, &watched)
                        .await;
                    match sweep {
                        Ok(report) => {
                            for outcome in &report.outcomes {
                                if !is_event(outcome) {
                                    continue;
                                }
                                emit(&self.sink, outcome);
                                self.core.compact_quietly(self.account.id);
                                if self.control.record() {
                                    break;
                                }
                            }
                            for failure in &report.failures {
                                tracing::warn!(
                                    mailbox = %failure.mailbox,
                                    error = %failure.error,
                                    "mailbox sync failed"
                                );
                            }
                        }
                        Err(err) => tracing::warn!(%err, "account sweep failed"),
                    }
                    session.logout().await;
                    self.core.compact_quietly(self.account.id);
                    if let Err(err) = self.core.purge_expired_trash() {
                        tracing::warn!(%err, "purging expired trash failed");
                    }
                }
                Err(err) => {
                    tracing::warn!(%err, "connect failed, retrying");
                    if sleep_or_stop(&mut stop, backoff).await {
                        break;
                    }
                    backoff = (backoff * 2).min(Duration::from_secs(60));
                    continue;
                }
            }
            let sweep = Duration::from_secs(self.options.sweep_seconds.max(1));
            if sleep_or_stop(&mut stop, sweep).await {
                break;
            }
        }
        self.tasks.shutdown().await;
        Ok(())
    }

    /// Stops workers whose mailbox vanished and gives idle-capable mailboxes a
    /// session while slots are free. Tier C servers cannot push, so every
    /// mailbox stays on the sweep.
    fn replan(&mut self, tier: Tier, targets: &[MailboxTarget], present: &HashSet<&str>) {
        let vanished: Vec<String> = self
            .workers
            .keys()
            .filter(|name| !present.contains(name.as_str()))
            .cloned()
            .collect();
        for name in vanished {
            if let Some(handle) = self.workers.remove(&name) {
                tracing::info!(mailbox = %name, "mailbox vanished, stopping its watcher");
                handle.abort();
            }
        }
        if !matches!(tier, Tier::A | Tier::B) {
            return;
        }
        for target in targets {
            if self.workers.len() >= self.options.idle_sessions {
                break;
            }
            if self.workers.contains_key(&target.name) {
                continue;
            }
            let worker = watch_mailbox_worker(
                self.core.clone(),
                self.account.clone(),
                target.name.clone(),
                Arc::clone(&self.control),
                Arc::clone(&self.sink),
                self.control.subscribe(),
            );
            let handle = self.tasks.spawn(worker);
            self.workers.insert(target.name.clone(), handle);
        }
    }
}

/// Facade handle; clones share the store and the secret store, which lets the
/// watcher run one session per mailbox.
#[derive(Clone)]
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
            username: spec.username.clone(),
            protocol: spec.protocol,
            security: Some(spec.security),
            tls: None,
            smtp: spec.smtp.clone(),
            sent_mailbox: spec.sent_mailbox.clone(),
        };
        let id = self.store.create_account(
            &spec.provider,
            &spec.email,
            spec.display_name.as_deref(),
            &secret_ref,
            &serde_json::to_value(&settings)?,
        )?;
        Ok(id)
    }

    fn account(&self, email: &str) -> Result<Account> {
        self.store
            .account_by_email(email)?
            .ok_or_else(|| CoreError::AccountNotFound(email.to_string()))
    }

    pub async fn sync(&self, email: &str, mailbox: &str) -> Result<Vec<SyncOutcome>> {
        let account = self.account(email)?;
        let settings = settings_of(&account)?;
        self.purge_expired_trash()?;
        let outcomes = match settings.protocol {
            ReceiveProtocol::Imap => {
                let mut session = self.connect_imap(&account).await?;
                let engine = SyncEngine::new(Arc::clone(&self.store));
                let outcome = engine
                    .sync_mailbox(account.id, &mut session, mailbox)
                    .await?;
                session.logout().await;
                vec![outcome]
            }
            ReceiveProtocol::Pop3 => {
                let mut session = self.connect_pop3(&account).await?;
                let engine = Pop3SyncEngine::new(Arc::clone(&self.store));
                let outcome = engine.sync(account.id, &mut session).await?;
                session.quit().await;
                vec![outcome]
            }
        };
        self.compact_account(account.id)?;
        Ok(outcomes)
    }

    /// Syncs every selectable mailbox of an IMAP account and moves mailboxes
    /// that disappeared on the server into the local trash. POP3 stays a
    /// single INBOX mirror. Expired trash is purged first.
    pub async fn sync_account(&self, email: &str) -> Result<AccountSyncReport> {
        let account = self.account(email)?;
        let settings = settings_of(&account)?;
        self.purge_expired_trash()?;
        let report = match settings.protocol {
            ReceiveProtocol::Imap => {
                let mut session = self.connect_imap(&account).await?;
                let engine = SyncEngine::new(Arc::clone(&self.store));
                let report = engine.sync_account(account.id, &mut session).await?;
                session.logout().await;
                report
            }
            ReceiveProtocol::Pop3 => {
                let mut session = self.connect_pop3(&account).await?;
                let engine = Pop3SyncEngine::new(Arc::clone(&self.store));
                let outcome = engine.sync(account.id, &mut session).await?;
                session.quit().await;
                AccountSyncReport {
                    outcomes: vec![outcome],
                    failures: Vec::new(),
                }
            }
        };
        self.compact_account(account.id)?;
        Ok(report)
    }

    /// Cursor bookkeeping for incremental consumers; a cursor below
    /// `oldest_seq` (or `snapshot_seq`) must reload the snapshot.
    pub fn change_window(&self, email: &str) -> Result<ChangeWindow> {
        let account = self.account(email)?;
        Ok(self.store.change_window(account.id)?)
    }

    /// Replaces the change log with a snapshot once it grows past
    /// `change_log_max_rows` (20,000 by default). Runs after every sync.
    pub fn compact_changes(&self, email: &str) -> Result<Option<Compaction>> {
        let account = self.account(email)?;
        self.compact_account(account.id)
    }

    /// Long-running watch loops must not die because compaction failed; the
    /// log stays bounded at the next successful sync instead.
    fn compact_quietly(&self, account_id: i64) {
        if let Err(err) = self.compact_account(account_id) {
            tracing::warn!(%err, "change log compaction failed");
        }
    }

    fn compact_account(&self, account_id: i64) -> Result<Option<Compaction>> {
        let max_rows = self.store.change_log_max_rows()?;
        let compaction = self.store.compact_changes(account_id, max_rows)?;
        if let Some(compaction) = compaction {
            tracing::info!(
                account_id,
                snapshot_seq = compaction.snapshot_seq,
                removed = compaction.removed_changes,
                "compacted change log"
            );
        }
        Ok(compaction)
    }

    /// Deletes trash older than the configured retention (three months by
    /// default). Runs automatically at the start of every sync.
    pub fn purge_expired_trash(&self) -> Result<PurgeStats> {
        let days = self.store.trash_retention_days()?;
        let stats = self.store.purge_trash(days, Utc::now().timestamp())?;
        if stats.mailboxes > 0 || stats.messages > 0 {
            tracing::info!(
                mailboxes = stats.mailboxes,
                messages = stats.messages,
                "purged expired trash"
            );
        }
        Ok(stats)
    }

    pub fn trash_retention_days(&self) -> Result<u32> {
        Ok(self.store.trash_retention_days()?)
    }

    pub fn set_trash_retention_days(&self, days: u32) -> Result<()> {
        Ok(self.store.set_trash_retention_days(days)?)
    }

    /// Keeps a mailbox in sync: IMAP uses IDLE and reconnect/backoff, POP3 polls.
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
        let account = self.account(email)?;
        let settings = settings_of(&account)?;
        match settings.protocol {
            ReceiveProtocol::Imap => {
                self.watch_imap(&account, mailbox, max_events, &mut on_event)
                    .await
            }
            ReceiveProtocol::Pop3 => self.watch_pop3(&account, max_events, &mut on_event).await,
        }
    }

    async fn watch_imap<F>(
        &self,
        account: &Account,
        mailbox: &str,
        max_events: usize,
        on_event: &mut F,
    ) -> Result<()>
    where
        F: FnMut(&SyncOutcome),
    {
        let engine = SyncEngine::new(Arc::clone(&self.store));
        let mut backoff = Duration::from_secs(1);
        let mut delivered = 0usize;

        loop {
            let mut session = match self.connect_imap(account).await {
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
                    self.compact_quietly(account.id);
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
                                self.compact_quietly(account.id);
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

    async fn watch_pop3<F>(
        &self,
        account: &Account,
        max_events: usize,
        on_event: &mut F,
    ) -> Result<()>
    where
        F: FnMut(&SyncOutcome),
    {
        self.watch_pop3_events(account, max_events, false, on_event)
            .await
    }

    /// POP3 poll loop. With `events_only`, quiet polls do not reach `on_event`
    /// and do not spend the event budget.
    async fn watch_pop3_events<F>(
        &self,
        account: &Account,
        max_events: usize,
        events_only: bool,
        on_event: &mut F,
    ) -> Result<()>
    where
        F: FnMut(&SyncOutcome),
    {
        let engine = Pop3SyncEngine::new(Arc::clone(&self.store));
        let mut delivered = 0usize;
        let mut backoff = Duration::from_secs(60);
        loop {
            match self.connect_pop3(account).await {
                Ok(mut session) => {
                    backoff = Duration::from_secs(60);
                    match engine.sync(account.id, &mut session).await {
                        Ok(outcome) => {
                            if !events_only || is_event(&outcome) {
                                on_event(&outcome);
                                delivered += 1;
                                self.compact_quietly(account.id);
                            }
                        }
                        Err(err) => tracing::warn!(%err, "POP3 sync failed"),
                    }
                    session.quit().await;
                    if delivered >= max_events {
                        return Ok(());
                    }
                }
                Err(err) => {
                    tracing::warn!(%err, "POP3 connect failed, retrying");
                    backoff = (backoff * 2).min(Duration::from_secs(15 * 60));
                }
            }
            tokio::time::sleep(backoff).await;
        }
    }

    /// Keeps every selectable mailbox of an account live. Mailboxes whose
    /// server can push get their own IDLE session (up to
    /// [`WatchOptions::idle_sessions`]); a sweep connection polls the rest and
    /// re-plans the mailbox set, moving vanished mailboxes into the local
    /// trash. POP3 accounts have a single INBOX and stay on polling.
    ///
    /// Only outcomes that carry new/dropped messages or a rebuild reach
    /// `on_event`; `max_events` counts those, so a quiet account never wakes
    /// the caller just because a sweep ran.
    pub async fn watch_account<F>(
        &self,
        email: &str,
        max_events: usize,
        options: WatchOptions,
        on_event: F,
    ) -> Result<()>
    where
        F: FnMut(&SyncOutcome) + Send + 'static,
    {
        let account = self.account(email)?;
        let settings = settings_of(&account)?;
        match settings.protocol {
            ReceiveProtocol::Imap => {
                let mut watcher = AccountWatcher {
                    core: self.clone(),
                    account,
                    options,
                    control: Arc::new(WatchControl::new(max_events)),
                    sink: Arc::new(StdMutex::new(on_event)),
                    workers: HashMap::new(),
                    tasks: JoinSet::new(),
                };
                watcher.run().await
            }
            ReceiveProtocol::Pop3 => {
                let sink = Arc::new(StdMutex::new(on_event));
                let mut forward = |outcome: &SyncOutcome| emit(&sink, outcome);
                self.watch_pop3_events(&account, max_events, true, &mut forward)
                    .await
            }
        }
    }

    // ---- message content ---------------------------------------------------

    /// Loads a message's body, fetching it from the server on first use.
    ///
    /// A second call for the same message is served entirely from the local
    /// cache; attachments are fetched separately by [`Core::fetch_attachment`].
    pub async fn fetch_body(&self, email: &str, message_id: i64) -> Result<MessageContent> {
        let account = self.account(email)?;
        let context = self
            .store
            .message_context(message_id)?
            .ok_or(CoreError::MessageNotFound(message_id))?;

        if let Some(content) = self.cached_content(message_id)? {
            // Bodies cached before the index existed (or by an older build) get
            // indexed the first time the message is opened.
            if let Some(text) = content.text.as_deref() {
                if let Err(err) = self.store.index_body_text_if_missing(message_id, text) {
                    tracing::warn!(%err, "indexing cached body failed");
                }
            }
            return Ok(content);
        }

        let settings = settings_of(&account)?;
        match settings.protocol {
            ReceiveProtocol::Imap => {
                let mut session = self.connect_imap(&account).await?;
                let outcome = self
                    .fetch_imap_content(&mut session, message_id, &context)
                    .await;
                session.logout().await;
                outcome?;
            }
            ReceiveProtocol::Pop3 => {
                let mut session = self.connect_pop3(&account).await?;
                let outcome = self
                    .store_pop3_content(&mut session, message_id, &context)
                    .await;
                session.quit().await;
                outcome?;
            }
        }

        self.cached_content(message_id)?
            .ok_or(CoreError::MessageNotFound(message_id))
    }

    /// Downloads one attachment on demand. POP3 cannot fetch a single part, so
    /// that path re-reads the whole message and caches it.
    pub async fn fetch_attachment(
        &self,
        email: &str,
        message_id: i64,
        part: &str,
    ) -> Result<Vec<u8>> {
        if let Some(data) = self.store.attachment_data(message_id, part)? {
            return Ok(data);
        }
        let account = self.account(email)?;
        let context = self
            .store
            .message_context(message_id)?
            .ok_or(CoreError::MessageNotFound(message_id))?;
        let meta = self
            .store
            .attachment_meta(message_id, part)?
            .ok_or_else(|| CoreError::AttachmentNotFound {
                message_id,
                part: part.to_string(),
            })?;

        let settings = settings_of(&account)?;
        let data = match settings.protocol {
            ReceiveProtocol::Imap => {
                let mut session = self.connect_imap(&account).await?;
                let outcome = self
                    .fetch_imap_attachment(&mut session, &context, part, meta.encoding.as_deref())
                    .await;
                session.logout().await;
                outcome?
            }
            ReceiveProtocol::Pop3 => {
                let mut session = self.connect_pop3(&account).await?;
                let outcome = self
                    .store_pop3_content(&mut session, message_id, &context)
                    .await;
                session.quit().await;
                outcome?;
                self.store
                    .attachment_data(message_id, part)?
                    .ok_or_else(|| CoreError::AttachmentNotFound {
                        message_id,
                        part: part.to_string(),
                    })?
            }
        };

        self.store.store_attachment(
            context.account_id,
            message_id,
            part,
            &meta.content_type,
            meta.filename.as_deref(),
            &data,
        )?;
        Ok(data)
    }

    /// Adds and removes JMAP keywords on every live copy of a message. The
    /// server is updated first; local flags change only after every STORE
    /// succeeded, so a failed write never fakes success.
    pub async fn set_keywords(
        &self,
        email: &str,
        message_id: i64,
        add: &[String],
        remove: &[String],
    ) -> Result<()> {
        let account = self.account(email)?;
        let context = self
            .store
            .message_context(message_id)?
            .ok_or(CoreError::MessageNotFound(message_id))?;
        if context.account_id != account.id {
            return Err(CoreError::MessageNotFound(message_id));
        }
        let targets = self
            .store
            .message_location_targets(account.id, message_id)?;
        if targets.is_empty() {
            return Err(CoreError::Unsupported("message is only in the local trash"));
        }
        let settings = settings_of(&account)?;
        if !matches!(settings.protocol, ReceiveProtocol::Imap) {
            return Err(CoreError::Unsupported("keywords on a POP3 account"));
        }
        let remove_flags: Vec<&str> = remove
            .iter()
            .map(|keyword| keyword_flag(keyword))
            .collect::<Result<_>>()?;
        let add_flags: Vec<&str> = add
            .iter()
            .map(|keyword| keyword_flag(keyword))
            .collect::<Result<Vec<_>>>()?
            .into_iter()
            .filter(|flag| {
                !remove_flags
                    .iter()
                    .any(|other| other.eq_ignore_ascii_case(flag))
            })
            .collect();

        let mut session = self.connect_imap(&account).await?;
        let outcome = self
            .apply_flags_imap(&mut session, &targets, &add_flags, &remove_flags)
            .await;
        session.logout().await;
        outcome?;

        let added: Vec<String> = add_flags.iter().map(|flag| flag.to_string()).collect();
        let removed: Vec<String> = remove_flags.iter().map(|flag| flag.to_string()).collect();
        self.store
            .update_flags(account.id, message_id, &added, &removed)?;
        Ok(())
    }

    async fn apply_flags_imap(
        &self,
        session: &mut ImapSession,
        targets: &[LocationTarget],
        add: &[&str],
        remove: &[&str],
    ) -> Result<()> {
        let mut selected: Option<&str> = None;
        for target in targets {
            if selected != Some(target.mailbox_name.as_str()) {
                session.select(&target.mailbox_name).await?;
                selected = Some(target.mailbox_name.as_str());
            }
            let uid = uid_of(target.uid)?;
            for flag in add {
                session.add_flag(uid, flag).await?;
            }
            for flag in remove {
                session.remove_flag(uid, flag).await?;
            }
        }
        Ok(())
    }

    /// Moves a message to another mailbox. Only messages with a single live
    /// copy are accepted, because an IMAP move affects one copy.
    pub async fn move_message(&self, email: &str, message_id: i64, mailbox_id: i64) -> Result<()> {
        let account = self.account(email)?;
        let context = self
            .store
            .message_context(message_id)?
            .ok_or(CoreError::MessageNotFound(message_id))?;
        if context.account_id != account.id {
            return Err(CoreError::MessageNotFound(message_id));
        }
        let settings = settings_of(&account)?;
        if !matches!(settings.protocol, ReceiveProtocol::Imap) {
            return Err(CoreError::Unsupported("move on a POP3 account"));
        }
        let targets = self
            .store
            .message_location_targets(account.id, message_id)?;
        if targets.is_empty() {
            return Err(CoreError::Unsupported("message is only in the local trash"));
        }
        if targets.len() > 1 {
            return Err(CoreError::MultipleLocations(message_id));
        }
        let source = &targets[0];
        let destination = self
            .store
            .mailbox_by_id(mailbox_id)?
            .ok_or(CoreError::MailboxNotFound(mailbox_id))?;
        if destination.account_id != account.id || destination.missing_at.is_some() {
            return Err(CoreError::MailboxNotFound(mailbox_id));
        }
        if destination.id == source.mailbox_id {
            return Ok(());
        }

        let mut session = self.connect_imap(&account).await?;
        let outcome = self
            .move_message_imap(&mut session, &account, message_id, source, &destination)
            .await;
        session.logout().await;
        outcome
    }

    async fn move_message_imap(
        &self,
        session: &mut ImapSession,
        account: &Account,
        message_id: i64,
        source: &LocationTarget,
        destination: &Mailbox,
    ) -> Result<()> {
        session.select(&source.mailbox_name).await?;
        let method = session
            .move_message(uid_of(source.uid)?, &destination.remote_name)
            .await?;
        tracing::info!(
            message_id,
            method,
            from = source.mailbox_name.as_str(),
            to = destination.remote_name.as_str(),
            "message moved"
        );

        // Syncing the destination discovers the new UID, which the store
        // cannot know without UIDPLUS response codes. The source location is
        // dropped only after the message is visible in the destination.
        let engine = SyncEngine::new(Arc::clone(&self.store));
        let report = engine
            .sync_mailbox(account.id, session, &destination.remote_name)
            .await?;
        if report.applied == 0 && report.inserted == 0 {
            return Err(CoreError::Unsupported(
                "the server did not report the moved message yet",
            ));
        }
        self.store
            .remove_location(account.id, message_id, source.mailbox_id)?;
        Ok(())
    }

    /// Cached content, but only once every body alternative is present.
    fn cached_content(&self, message_id: i64) -> Result<Option<MessageContent>> {
        let Some(body) = self.store.message_body(message_id)? else {
            return Ok(None);
        };
        if body.state == BodyState::Headers {
            return Ok(None);
        }
        if body.parts.iter().any(|part| !part.cached) {
            return Ok(None);
        }
        Ok(Some(content_of(message_id, body)))
    }

    async fn fetch_imap_content(
        &self,
        session: &mut ImapSession,
        message_id: i64,
        context: &MessageContext,
    ) -> Result<()> {
        session.select(&context.mailbox_name).await?;
        let uid = uid_of(context.uid)?;
        let layout = session.fetch_body_layout(uid).await?;

        let names: Vec<Option<String>> = layout
            .parts
            .iter()
            .map(|part| part.filename.as_deref().map(clean_attachment_name))
            .collect();
        let mut part_meta = Vec::new();
        let mut attachment_meta = Vec::new();
        for (part, name) in layout.parts.iter().zip(&names) {
            if part.is_body_text() {
                part_meta.push(PartMetaInput {
                    part: &part.part,
                    content_type: &part.content_type,
                    charset: part.charset.as_deref(),
                    is_html: part.content_type == "text/html",
                    size: i64::from(part.size),
                });
            } else {
                attachment_meta.push(AttachmentMetaInput {
                    part: &part.part,
                    filename: name.as_deref(),
                    content_type: &part.content_type,
                    encoding: Some(part.encoding.as_str()),
                    size: i64::from(part.size),
                });
            }
        }
        self.store.record_body_structure(
            context.account_id,
            message_id,
            &part_meta,
            &attachment_meta,
        )?;

        let mut decoded: Vec<DecodedPart> = Vec::new();
        for part in &layout.parts {
            if !part.is_body_text() {
                continue;
            }
            let raw = session.fetch_part(uid, &part.part).await?;
            let bytes = mail_parse::decode_transfer(&part.encoding, &raw);
            let text = mail_parse::decode_text(&bytes, part.charset.as_deref());
            let is_html = part.content_type == "text/html";
            let stored = if is_html {
                mail_parse::sanitize_html(&text)
            } else {
                text
            };
            decoded.push(DecodedPart {
                part: part.part.clone(),
                content_type: part.content_type.clone(),
                is_html,
                data: stored,
            });
        }
        self.store_decoded_parts(context.account_id, message_id, &decoded)?;
        Ok(())
    }

    async fn fetch_imap_attachment(
        &self,
        session: &mut ImapSession,
        context: &MessageContext,
        part: &str,
        encoding: Option<&str>,
    ) -> Result<Vec<u8>> {
        session.select(&context.mailbox_name).await?;
        let uid = uid_of(context.uid)?;
        let raw = session.fetch_part(uid, part).await?;
        Ok(mail_parse::decode_transfer(encoding.unwrap_or(""), &raw))
    }

    /// POP3 has no sections: read the whole message, parse MIME locally, and
    /// cache bodies plus attachments in one pass.
    async fn store_pop3_content(
        &self,
        session: &mut Pop3Session,
        message_id: i64,
        context: &MessageContext,
    ) -> Result<()> {
        let listed = session.list().await?;
        let number = listed
            .iter()
            .find(|item| pop3_uid(&item.uid) == context.uid)
            .map(|item| item.number)
            .ok_or(CoreError::MessageGone(context.uid))?;
        let raw = session.retrieve(number).await?;
        let parsed = mail_parse::parse_message(&raw);

        // POP3 has no IMAP section ids; synthetic ones keep the cache keyed
        // without pretending they are fetchable.
        let mut part_meta = Vec::new();
        let mut decoded = Vec::new();
        if let Some(text) = parsed.text.as_ref() {
            part_meta.push(PartMetaInput {
                part: TEXT_PART,
                content_type: "text/plain",
                charset: Some("utf-8"),
                is_html: false,
                size: text.len() as i64,
            });
            decoded.push(DecodedPart {
                part: TEXT_PART.to_string(),
                content_type: "text/plain".to_string(),
                is_html: false,
                data: text.clone(),
            });
        }
        if let Some(html) = parsed.html.as_ref() {
            part_meta.push(PartMetaInput {
                part: HTML_PART,
                content_type: "text/html",
                charset: Some("utf-8"),
                is_html: true,
                size: html.len() as i64,
            });
            decoded.push(DecodedPart {
                part: HTML_PART.to_string(),
                content_type: "text/html".to_string(),
                is_html: true,
                data: html.clone(),
            });
        }
        let attachment_meta: Vec<AttachmentMetaInput<'_>> = parsed
            .attachments
            .iter()
            .map(|attachment| AttachmentMetaInput {
                part: &attachment.part,
                filename: attachment.filename.as_deref(),
                content_type: &attachment.content_type,
                // parse_message already decodes transfer encodings.
                encoding: None,
                size: attachment.bytes.len() as i64,
            })
            .collect();
        self.store.record_body_structure(
            context.account_id,
            message_id,
            &part_meta,
            &attachment_meta,
        )?;
        self.store_decoded_parts(context.account_id, message_id, &decoded)?;
        for attachment in &parsed.attachments {
            self.store.store_attachment(
                context.account_id,
                message_id,
                &attachment.part,
                &attachment.content_type,
                attachment.filename.as_deref(),
                &attachment.bytes,
            )?;
        }
        Ok(())
    }

    fn store_decoded_parts(
        &self,
        account_id: i64,
        message_id: i64,
        parts: &[DecodedPart],
    ) -> Result<()> {
        if parts.is_empty() {
            return Ok(());
        }
        let inputs: Vec<BodyPartInput<'_>> = parts
            .iter()
            .map(|part| BodyPartInput {
                part: &part.part,
                content_type: &part.content_type,
                charset: Some("utf-8"),
                is_html: part.is_html,
                data: part.data.as_bytes(),
            })
            .collect();
        // Text/plain wins over HTML→text, matching what the reader shows.
        let index_text = parts
            .iter()
            .find(|part| !part.is_html)
            .map(|part| part.data.clone())
            .or_else(|| {
                parts
                    .iter()
                    .find(|part| part.is_html)
                    .map(|part| mail_parse::html_to_text(&part.data))
            });
        self.store
            .store_body_parts(account_id, message_id, &inputs, index_text.as_deref())?;
        Ok(())
    }

    pub async fn connect_imap(&self, account: &Account) -> Result<ImapSession> {
        let settings = settings_of(account)?;
        let security = settings.security();
        let password = self.password(account)?;
        let (session, _caps) = ImapSession::connect(&ImapConfig {
            host: settings.host,
            port: settings.port,
            security: security.into(),
            username: settings.username,
            password,
        })
        .await?;
        Ok(session)
    }

    async fn connect_pop3(&self, account: &Account) -> Result<Pop3Session> {
        let settings = settings_of(account)?;
        let security = settings.security();
        let password = self.password(account)?;
        Ok(Pop3Session::connect(&Pop3Config {
            host: settings.host,
            port: settings.port,
            security: security.into(),
            username: settings.username,
            password,
        })
        .await?)
    }

    /// Verifies both the receive login and, when configured, SMTP AUTH.
    pub async fn check(&self, email: &str) -> Result<()> {
        let account = self.account(email)?;
        let settings = settings_of(&account)?;
        match settings.protocol {
            ReceiveProtocol::Imap => self.connect_imap(&account).await?.logout().await,
            ReceiveProtocol::Pop3 => self.connect_pop3(&account).await?.quit().await,
        }
        if let Some(smtp) = settings.smtp.as_ref() {
            let password = self.password(&account)?;
            let session = SmtpSession::connect(&SmtpConfig {
                host: smtp.host.clone(),
                port: smtp.port,
                security: smtp.security().into(),
                username: Some(
                    smtp.username
                        .clone()
                        .unwrap_or_else(|| settings.username.clone()),
                ),
                password: Some(password),
            })
            .await?;
            session.quit().await;
        }
        Ok(())
    }

    /// Sends a plain-text UTF-8 message over SMTP, then files it in the Sent
    /// mailbox for IMAP accounts. A filing failure does not hide a successful
    /// SMTP submission; it is reported in the outcome.
    pub async fn send(
        &self,
        email: &str,
        recipients: &[String],
        subject: &str,
        body: &str,
    ) -> Result<SendOutcome> {
        let account = self.account(email)?;
        let settings = settings_of(&account)?;
        let smtp = settings
            .smtp
            .as_ref()
            .ok_or_else(|| CoreError::Settings("no SMTP settings for this account".into()))?;
        let password = self.password(&account)?;
        let message = build_message(&account.email, recipients, subject, body);

        let mut session = SmtpSession::connect(&SmtpConfig {
            host: smtp.host.clone(),
            port: smtp.port,
            security: smtp.security().into(),
            username: Some(
                smtp.username
                    .clone()
                    .unwrap_or_else(|| settings.username.clone()),
            ),
            password: Some(password),
        })
        .await?;
        session
            .send(&account.email, recipients, message.as_bytes())
            .await?;
        session.quit().await;

        let mut saved_to = None;
        let mut save_error = None;
        if settings.protocol == ReceiveProtocol::Imap {
            let mailbox = settings
                .sent_mailbox
                .clone()
                .unwrap_or_else(|| "Sent".to_string());
            match self.connect_imap(&account).await {
                Ok(mut imap) => {
                    match imap
                        .append(&mailbox, Some("\\Seen"), message.as_bytes())
                        .await
                    {
                        Ok(()) => saved_to = Some(mailbox),
                        Err(err) => save_error = Some(err.to_string()),
                    }
                    imap.logout().await;
                }
                Err(err) => save_error = Some(err.to_string()),
            }
        }

        Ok(SendOutcome {
            recipients: recipients.len(),
            saved_to,
            save_error,
        })
    }

    fn password(&self, account: &Account) -> Result<String> {
        let secret = self
            .secrets
            .get(&account.secret_ref)?
            .ok_or_else(|| CoreError::MissingSecret(account.secret_ref.clone()))?;
        String::from_utf8(secret).map_err(|_| CoreError::Settings("password is not UTF-8".into()))
    }
}

fn secret_key(email: &str) -> String {
    format!("account:{email}:password")
}

fn settings_of(account: &Account) -> Result<ServerSettings> {
    serde_json::from_value(account.settings.clone()).map_err(|e| CoreError::Settings(e.to_string()))
}

/// Synthetic part ids for POP3, where there are no IMAP sections.
const TEXT_PART: &str = "text";
const HTML_PART: &str = "html";

struct DecodedPart {
    part: String,
    content_type: String,
    is_html: bool,
    data: String,
}

fn content_of(message_id: i64, body: MessageBody) -> MessageContent {
    let text = body
        .text
        .or_else(|| body.html.as_deref().map(mail_parse::html_to_text));
    MessageContent {
        message_id,
        body_state: body.state,
        text,
        html: body.html,
        attachments: body
            .attachments
            .into_iter()
            .map(|attachment| AttachmentInfo {
                part: attachment.part,
                filename: attachment.filename,
                content_type: attachment.content_type,
                size: attachment.size,
                downloaded: attachment.downloaded,
            })
            .collect(),
    }
}

fn uid_of(uid: i64) -> Result<u32> {
    u32::try_from(uid)
        .map_err(|_| CoreError::Settings(format!("uid {uid} is outside the IMAP range")))
}

/// JMAP keyword to IMAP flag. Custom keywords are rejected until the store can
/// round-trip them consistently.
fn keyword_flag(keyword: &str) -> Result<&'static str> {
    match keyword {
        "$seen" => Ok("\\Seen"),
        "$flagged" => Ok("\\Flagged"),
        "$answered" => Ok("\\Answered"),
        "$draft" => Ok("\\Draft"),
        "$forwarded" => Ok("\\Forwarded"),
        other => Err(CoreError::UnknownKeyword(other.to_string())),
    }
}

/// Reduces a server-supplied attachment name to a safe basename.
fn clean_attachment_name(raw: &str) -> String {
    let decoded = mail_parse::decode_encoded_words(raw);
    let base = decoded.rsplit(['/', '\\']).next().unwrap_or(&decoded);
    let cleaned: String = base.chars().filter(|c| !c.is_control()).collect();
    let trimmed = cleaned.trim();
    if trimmed.is_empty() {
        "attachment".to_string()
    } else if trimmed.chars().count() > 255 {
        trimmed.chars().take(255).collect()
    } else {
        trimmed.to_string()
    }
}

/// Builds a minimal RFC 5322 message. The body is base64 so any UTF-8 content
/// survives SMTP line rules without quoted-printable edge cases.
fn build_message(from: &str, recipients: &[String], subject: &str, body: &str) -> String {
    let domain = from.rsplit_once('@').map(|(_, d)| d).unwrap_or("localhost");
    let subject = if subject.is_ascii() {
        subject.to_string()
    } else {
        format!("=?UTF-8?B?{}?=", STANDARD.encode(subject))
    };
    let encoded_body = wrap_base64(&STANDARD.encode(body));
    format!(
        "From: {from}\r\nTo: {}\r\nSubject: {subject}\r\nDate: {}\r\nMessage-ID: <{}@{}>\r\nMIME-Version: 1.0\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Transfer-Encoding: base64\r\n\r\n{encoded_body}\r\n",
        recipients.join(", "),
        Utc::now().to_rfc2822(),
        Uuid::new_v4(),
        domain,
    )
}

fn wrap_base64(encoded: &str) -> String {
    let mut out = String::with_capacity(encoded.len() + encoded.len() / 76 * 2);
    for (index, chunk) in encoded.as_bytes().chunks(76).enumerate() {
        if index > 0 {
            out.push_str("\r\n");
        }
        out.push_str(&String::from_utf8_lossy(chunk));
    }
    out
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

    fn outcome(full: bool, inserted: usize, applied: usize, removed: usize) -> SyncOutcome {
        SyncOutcome {
            mailbox: "INBOX".to_string(),
            tier: Tier::A,
            full,
            inserted,
            applied,
            removed,
            duration_ms: 0,
        }
    }

    #[test]
    fn flag_rescans_are_not_events() {
        // A periodic full flag rescan reports every row as applied; that must
        // not wake a caller that only cares about real changes.
        assert!(!is_event(&outcome(false, 0, 10_000, 0)));
        assert!(is_event(&outcome(false, 1, 1, 0)));
        assert!(is_event(&outcome(false, 0, 0, 2)));
        assert!(is_event(&outcome(true, 0, 0, 0)));
    }

    #[test]
    fn event_budget_stops_every_watcher() {
        let control = WatchControl::new(2);
        let stop = control.subscribe();
        assert!(!control.record());
        assert!(!*stop.borrow());
        assert!(control.record());
        assert!(*stop.borrow());
        assert!(control.record());
    }

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

#[cfg(test)]
mod transport_tests {
    use super::*;

    #[test]
    fn provider_presets_map_hosts_per_domain() {
        let gmail = presets::hosts("gmail", "me@gmail.com").expect("gmail");
        assert_eq!(gmail.imap, "imap.gmail.com");
        assert_eq!(gmail.pop3, "pop.gmail.com");
        assert_eq!(gmail.smtp, "smtp.gmail.com");

        let qq = presets::hosts("qq", "123@qq.com").expect("qq");
        assert_eq!(qq.imap, "imap.qq.com");
        assert_eq!(qq.pop3, "pop.qq.com");
        assert_eq!(qq.smtp, "smtp.qq.com");

        let perfect = presets::hosts("88", "name@111.com").expect("88");
        assert_eq!(perfect.imap, "imap.111.com");
        assert_eq!(perfect.pop3, "pop.111.com");
        assert_eq!(perfect.smtp, "smtp.111.com");

        assert!(presets::hosts("88", "name@example.com").is_err());
        assert!(presets::hosts("unknown", "name@example.com").is_err());
    }

    #[test]
    fn default_ports_follow_protocol_and_security() {
        assert_eq!(
            presets::default_port(ReceiveProtocol::Imap, SecurityMode::Tls),
            993
        );
        assert_eq!(
            presets::default_port(ReceiveProtocol::Imap, SecurityMode::StartTls),
            143
        );
        assert_eq!(
            presets::default_port(ReceiveProtocol::Pop3, SecurityMode::Tls),
            995
        );
        assert_eq!(
            presets::default_port(ReceiveProtocol::Pop3, SecurityMode::Plaintext),
            110
        );
        assert_eq!(presets::default_smtp_port(SecurityMode::Tls), 465);
        assert_eq!(presets::default_smtp_port(SecurityMode::StartTls), 587);
    }

    #[test]
    fn legacy_settings_without_protocol_stay_readable() {
        let settings: ServerSettings =
            serde_json::from_str(r#"{"host":"h","port":993,"tls":false,"username":"u"}"#)
                .expect("old settings parse");
        assert_eq!(settings.protocol, ReceiveProtocol::Imap);
        assert_eq!(settings.security(), SecurityMode::Plaintext);
        assert!(settings.smtp.is_none());
    }

    #[test]
    fn built_message_encodes_subject_and_body() {
        let message = build_message(
            "sender@example.com",
            &["peer@example.com".to_string()],
            "你好",
            "hello",
        );
        assert!(message.contains("To: peer@example.com\r\n"));
        assert!(message.contains("Subject: =?UTF-8?B?"));
        assert!(message.contains("Message-ID: <"));
        assert!(message.contains("@example.com>"));
        assert!(message.contains("aGVsbG8="));
        assert!(message.ends_with("\r\n"));
    }
}
