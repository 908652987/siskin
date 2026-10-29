//! Facade: account setup, credential handling, and sync orchestration.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use chrono::Utc;
use mail_crypto::{CryptoError, MasterKey, SecretStore};
use mail_proto::pop3::{Pop3Config, Pop3Session};
use mail_proto::smtp::{SmtpConfig, SmtpSession};
use mail_proto::{IdleEvent, ImapConfig, ImapSession, ProtoError, Security};
use mail_store::{
    Account, AttachmentMetaInput, BodyPartInput, BodyState, DbKey, MessageBody, MessageContext,
    PartMetaInput, Store, StoreError,
};
use mail_sync::{pop3_uid, Pop3SyncEngine, SyncEngine, SyncError, SyncOutcome};
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
        match settings.protocol {
            ReceiveProtocol::Imap => {
                let mut session = self.connect_imap(&account).await?;
                let engine = SyncEngine::new(Arc::clone(&self.store));
                let outcome = engine
                    .sync_mailbox(account.id, &mut session, mailbox)
                    .await?;
                session.logout().await;
                Ok(vec![outcome])
            }
            ReceiveProtocol::Pop3 => {
                let mut session = self.connect_pop3(&account).await?;
                let engine = Pop3SyncEngine::new(Arc::clone(&self.store));
                let outcome = engine.sync(account.id, &mut session).await?;
                session.quit().await;
                Ok(vec![outcome])
            }
        }
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

    async fn watch_pop3<F>(
        &self,
        account: &Account,
        max_events: usize,
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
                            on_event(&outcome);
                            delivered += 1;
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
        self.store
            .store_body_parts(account_id, message_id, &inputs)?;
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
