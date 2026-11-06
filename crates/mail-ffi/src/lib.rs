//! UniFFI surface for the native desktop clients.
//!
//! The facade is deliberately thin: it opens [`mail_core::Core`], exposes the
//! list/mailbox/body/write operations the UI needs, and maps every failure to a
//! single flat error so Swift/Kotlin do not have to mirror Rust enums.

use std::collections::HashMap;
use std::sync::Arc;

use mail_core::{
    presets, AccountSpec, Core, OutgoingAttachment, OutgoingMessage, ReceiveProtocol, SecurityMode,
    SmtpSettings, SyncOutcome,
};
use mail_crypto::{FileStore, KeyringStore, SecretStore};
use mail_store::{Account, Mailbox, MessageQuery, StoredMessage};
use thiserror::Error;

uniffi::setup_scaffolding!();

const SECRET_SERVICE: &str = "mail-client-dev";

/// Every FFI failure is flattened to a message; callers only show errors.
#[derive(Debug, Error, uniffi::Error)]
#[uniffi(flat_error)]
pub enum FfiError {
    #[error("{message}")]
    Failure { message: String },
}

fn failure(err: impl std::fmt::Display) -> FfiError {
    FfiError::Failure {
        message: err.to_string(),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum ReceiveProtocolFfi {
    Imap,
    Pop3,
}

impl From<ReceiveProtocolFfi> for ReceiveProtocol {
    fn from(value: ReceiveProtocolFfi) -> Self {
        match value {
            ReceiveProtocolFfi::Imap => ReceiveProtocol::Imap,
            ReceiveProtocolFfi::Pop3 => ReceiveProtocol::Pop3,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum SecurityFfi {
    Tls,
    StartTls,
    Plaintext,
}

impl From<SecurityFfi> for SecurityMode {
    fn from(value: SecurityFfi) -> Self {
        match value {
            SecurityFfi::Tls => SecurityMode::Tls,
            SecurityFfi::StartTls => SecurityMode::StartTls,
            SecurityFfi::Plaintext => SecurityMode::Plaintext,
        }
    }
}

#[derive(Debug, Clone, uniffi::Record)]
pub struct AccountView {
    pub id: i64,
    pub email: String,
    pub display_name: Option<String>,
}

#[derive(Debug, Clone, uniffi::Record)]
pub struct MailboxView {
    pub id: i64,
    pub name: String,
    pub role: Option<String>,
    pub total: i64,
    pub unread: i64,
}

#[derive(Debug, Clone, uniffi::Record)]
pub struct MessageView {
    pub id: i64,
    pub mailbox_id: i64,
    pub from: String,
    pub subject: String,
    pub snippet: String,
    pub sent_at: Option<i64>,
    pub unread: bool,
    pub flagged: bool,
}

#[derive(Debug, Clone, uniffi::Record)]
pub struct MessagePageView {
    pub messages: Vec<MessageView>,
    pub total: i64,
}

#[derive(Debug, Clone, uniffi::Record)]
pub struct AttachmentView {
    pub part: String,
    pub filename: Option<String>,
    pub content_type: String,
    pub size: i64,
    pub downloaded: bool,
}

#[derive(Debug, Clone, uniffi::Record)]
pub struct MessageBodyView {
    pub text: Option<String>,
    pub html: Option<String>,
    pub attachments: Vec<AttachmentView>,
}

#[derive(Debug, Clone, uniffi::Record)]
pub struct SyncSummaryView {
    pub inserted: u64,
    pub removed: u64,
    pub failures: Vec<String>,
}

#[derive(Debug, Clone, uniffi::Record)]
pub struct ProviderPresetView {
    pub id: String,
    pub label: String,
    pub host: String,
    pub port: u16,
    pub security: SecurityFfi,
    pub smtp_host: String,
    pub smtp_port: u16,
}

#[derive(Debug, Clone, uniffi::Record)]
pub struct NewAccountView {
    pub email: String,
    pub display_name: Option<String>,
    pub provider: Option<String>,
    pub host: String,
    pub port: u16,
    pub username: String,
    pub password: String,
    pub protocol: ReceiveProtocolFfi,
    pub security: SecurityFfi,
    /// Required for `custom` providers; preset providers fall back to the
    /// provider's SMTP host on port 465 with implicit TLS.
    pub smtp_host: Option<String>,
    pub smtp_port: Option<u16>,
    pub smtp_security: Option<SecurityFfi>,
    pub smtp_username: Option<String>,
    pub sent_mailbox: Option<String>,
}

#[derive(Debug, Clone, uniffi::Record)]
pub struct OutgoingAttachmentView {
    pub filename: String,
    pub content_type: Option<String>,
    pub data: Vec<u8>,
}

#[derive(Debug, Clone, uniffi::Record)]
pub struct SendRequestView {
    pub email: String,
    pub to: Vec<String>,
    pub cc: Vec<String>,
    pub bcc: Vec<String>,
    pub subject: String,
    pub body_text: String,
    pub attachments: Vec<OutgoingAttachmentView>,
}

#[derive(Debug, Clone, uniffi::Record)]
pub struct SendResultView {
    pub recipients: u32,
    pub saved_to: Option<String>,
    pub save_error: Option<String>,
}

/// Quick presets for the account sheet. The 88 host follows the address domain
/// (88.com / 111.com / email.cn) when one is present.
#[uniffi::export]
pub fn provider_presets(email: Option<String>) -> Vec<ProviderPresetView> {
    let address = email.as_deref().unwrap_or_default();
    let mut out = Vec::new();
    for (id, label) in [("gmail", "Gmail"), ("qq", "QQ"), ("88", "88")] {
        let (host, smtp_host) = match presets::hosts(id, address) {
            Ok(hosts) => (hosts.imap, hosts.smtp),
            Err(_) if id == "88" => ("imap.88.com".to_string(), "smtp.88.com".to_string()),
            Err(_) => continue,
        };
        out.push(ProviderPresetView {
            id: id.to_string(),
            label: label.to_string(),
            host,
            port: 993,
            security: SecurityFfi::Tls,
            smtp_host,
            smtp_port: presets::default_smtp_port(SecurityMode::Tls),
        });
    }
    out
}

#[derive(uniffi::Object)]
pub struct MailClient {
    core: Arc<Core>,
}

#[uniffi::export]
impl MailClient {
    /// Opens (or creates) the encrypted database. The keychain is the default;
    /// `EYRIE_DEV_SECRET_DIR` selects the development file store.
    #[uniffi::constructor]
    pub fn open(db_path: String) -> Result<Arc<Self>, FfiError> {
        let secrets = secret_store()?;
        let core = Core::open(db_path, secrets).map_err(failure)?;
        Ok(Arc::new(Self {
            core: Arc::new(core),
        }))
    }
}

#[uniffi::export(async_runtime = "tokio")]
impl MailClient {
    pub async fn accounts(&self) -> Result<Vec<AccountView>, FfiError> {
        let accounts = self.core.store().list_accounts().map_err(failure)?;
        Ok(accounts
            .into_iter()
            .map(|account| AccountView {
                id: account.id,
                email: account.email,
                display_name: account.display_name,
            })
            .collect())
    }

    pub async fn add_account(&self, account: NewAccountView) -> Result<i64, FfiError> {
        let smtp = account.smtp_host.clone().map(|host| {
            let security = account
                .smtp_security
                .map(SecurityMode::from)
                .unwrap_or(SecurityMode::Tls);
            SmtpSettings {
                host,
                port: account
                    .smtp_port
                    .unwrap_or_else(|| presets::default_smtp_port(security)),
                security: Some(security),
                username: Some(
                    account
                        .smtp_username
                        .clone()
                        .unwrap_or_else(|| account.username.clone()),
                ),
            }
        });
        let spec = AccountSpec {
            email: account.email,
            display_name: account.display_name,
            provider: account.provider.unwrap_or_else(|| "custom".to_string()),
            host: account.host,
            port: account.port,
            username: account.username,
            password: account.password,
            protocol: account.protocol.into(),
            security: account.security.into(),
            smtp,
            sent_mailbox: account.sent_mailbox,
        };
        self.core.add_account(&spec).map_err(failure)
    }

    pub async fn mailboxes(&self, email: String) -> Result<Vec<MailboxView>, FfiError> {
        let account = self.account(&email)?;
        let counts: HashMap<i64, (i64, i64)> = self
            .core
            .store()
            .mailbox_counts(account.id)
            .map_err(failure)?
            .into_iter()
            .map(|count| (count.mailbox_id, (count.total, count.unread)))
            .collect();
        let mut out = Vec::new();
        for mailbox in self
            .core
            .store()
            .list_mailboxes(account.id)
            .map_err(failure)?
        {
            if mailbox.missing_at.is_some() {
                continue;
            }
            let (total, unread) = counts.get(&mailbox.id).copied().unwrap_or((0, 0));
            out.push(mailbox_view(&mailbox, total, unread));
        }
        Ok(out)
    }

    pub async fn messages(
        &self,
        email: String,
        mailbox_id: Option<i64>,
        query: Option<String>,
        limit: u32,
        offset: u32,
    ) -> Result<MessagePageView, FfiError> {
        let account = self.account(&email)?;
        let text = query
            .map(|query| query.trim().to_string())
            .filter(|query| !query.is_empty());
        let page = self
            .core
            .store()
            .query_messages(
                account.id,
                &MessageQuery {
                    mailbox_id,
                    text,
                    limit: i64::from(limit.clamp(1, 500)),
                    offset: i64::from(offset),
                    ..MessageQuery::default()
                },
            )
            .map_err(failure)?;
        Ok(MessagePageView {
            messages: page.messages.iter().map(message_view).collect(),
            total: page.total,
        })
    }

    pub async fn body(&self, email: String, message_id: i64) -> Result<MessageBodyView, FfiError> {
        let content = self
            .core
            .fetch_body(&email, message_id)
            .await
            .map_err(failure)?;
        Ok(MessageBodyView {
            text: content.text,
            html: content.html,
            attachments: content
                .attachments
                .into_iter()
                .map(|attachment| AttachmentView {
                    part: attachment.part,
                    filename: attachment.filename,
                    content_type: attachment.content_type,
                    size: attachment.size,
                    downloaded: attachment.downloaded,
                })
                .collect(),
        })
    }

    pub async fn set_seen(
        &self,
        email: String,
        message_id: i64,
        seen: bool,
    ) -> Result<(), FfiError> {
        self.set_keyword(&email, message_id, "$seen", seen).await
    }

    pub async fn set_flagged(
        &self,
        email: String,
        message_id: i64,
        flagged: bool,
    ) -> Result<(), FfiError> {
        self.set_keyword(&email, message_id, "$flagged", flagged)
            .await
    }

    /// Downloads one attachment part; the UI decides where to save it.
    pub async fn attachment(
        &self,
        email: String,
        message_id: i64,
        part: String,
    ) -> Result<Vec<u8>, FfiError> {
        self.core
            .fetch_attachment(&email, message_id, &part)
            .await
            .map_err(failure)
    }

    pub async fn move_message(
        &self,
        email: String,
        message_id: i64,
        mailbox_id: i64,
    ) -> Result<(), FfiError> {
        self.core
            .move_message(&email, message_id, mailbox_id)
            .await
            .map_err(failure)
    }

    pub async fn sync(&self, email: String) -> Result<SyncSummaryView, FfiError> {
        let report = self.core.sync_account(&email).await.map_err(failure)?;
        let mut inserted = 0u64;
        let mut removed = 0u64;
        for outcome in &report.outcomes {
            insert_sync(&mut inserted, &mut removed, outcome);
        }
        Ok(SyncSummaryView {
            inserted,
            removed,
            failures: report
                .failures
                .into_iter()
                .map(|failure| format!("{}: {}", failure.mailbox, failure.error))
                .collect(),
        })
    }

    pub async fn send(&self, request: SendRequestView) -> Result<SendResultView, FfiError> {
        let message = OutgoingMessage {
            to: request.to,
            cc: request.cc,
            bcc: request.bcc,
            subject: request.subject,
            body_text: request.body_text,
            attachments: request
                .attachments
                .into_iter()
                .map(|attachment| OutgoingAttachment {
                    filename: attachment.filename,
                    content_type: attachment.content_type,
                    data: attachment.data,
                })
                .collect(),
        };
        let outcome = self
            .core
            .send(&request.email, &message)
            .await
            .map_err(failure)?;
        Ok(SendResultView {
            recipients: outcome.recipients as u32,
            saved_to: outcome.saved_to,
            save_error: outcome.save_error,
        })
    }
}

impl MailClient {
    fn account(&self, email: &str) -> Result<Account, FfiError> {
        self.core
            .store()
            .account_by_email(email)
            .map_err(failure)?
            .ok_or_else(|| failure(format!("unknown account: {email}")))
    }

    async fn set_keyword(
        &self,
        email: &str,
        message_id: i64,
        keyword: &str,
        enabled: bool,
    ) -> Result<(), FfiError> {
        let add = if enabled {
            vec![keyword.to_string()]
        } else {
            Vec::new()
        };
        let remove = if enabled {
            Vec::new()
        } else {
            vec![keyword.to_string()]
        };
        self.core
            .set_keywords(email, message_id, &add, &remove)
            .await
            .map_err(failure)
    }
}

fn insert_sync(inserted: &mut u64, removed: &mut u64, outcome: &SyncOutcome) {
    *inserted += outcome.inserted as u64;
    *removed += outcome.removed as u64;
}

fn mailbox_view(mailbox: &Mailbox, total: i64, unread: i64) -> MailboxView {
    MailboxView {
        id: mailbox.id,
        name: mailbox.remote_name.clone(),
        role: mailbox.role.clone(),
        total,
        unread,
    }
}

fn message_view(message: &StoredMessage) -> MessageView {
    let unread = !message
        .flags
        .iter()
        .any(|flag| flag.eq_ignore_ascii_case("\\Seen"));
    let flagged = message
        .flags
        .iter()
        .any(|flag| flag.eq_ignore_ascii_case("\\Flagged"));
    MessageView {
        id: message.id,
        mailbox_id: message.mailbox_id,
        from: message
            .from
            .first()
            .map(|from| mail_parse::decode_encoded_words(from))
            .unwrap_or_default(),
        subject: message
            .subject
            .as_deref()
            .map(mail_parse::decode_encoded_words)
            .unwrap_or_default(),
        snippet: message.snippet.clone().unwrap_or_default(),
        sent_at: message.sent_at,
        unread,
        flagged,
    }
}

fn secret_store() -> Result<Arc<dyn SecretStore>, FfiError> {
    match std::env::var("EYRIE_DEV_SECRET_DIR") {
        Ok(dir) => Ok(Arc::new(FileStore::new(dir).map_err(failure)?)),
        Err(_) => Ok(Arc::new(KeyringStore::new(SECRET_SERVICE))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mail_crypto::MemoryStore;
    use mail_store::NewMessage;

    fn client() -> MailClient {
        let core = Core::open_in_memory(Arc::new(MemoryStore::new())).expect("core");
        MailClient {
            core: Arc::new(core),
        }
    }

    fn message(uid: i64, subject: &str) -> NewMessage {
        NewMessage {
            uid,
            message_id: Some(format!("<{uid}@example.com>")),
            in_reply_to: None,
            subject: Some(subject.to_string()),
            from: vec!["Alice <alice@example.com>".to_string()],
            to: vec!["me@example.com".to_string()],
            sent_at: Some(1_700_000_000 + uid),
            size: Some(10),
            flags: Vec::new(),
            snippet: Some(format!("{subject} snippet")),
        }
    }

    #[test]
    fn provider_presets_follow_the_domain() {
        let presets = provider_presets(Some("someone@111.com".to_string()));
        let host = presets
            .iter()
            .find(|preset| preset.id == "88")
            .map(|preset| preset.host.as_str());
        assert_eq!(host, Some("imap.111.com"));
        let gmail = presets.iter().find(|preset| preset.id == "gmail").unwrap();
        assert_eq!(gmail.port, 993);
        assert_eq!(gmail.security, SecurityFfi::Tls);
        assert_eq!(gmail.smtp_host, "smtp.gmail.com");
        assert_eq!(gmail.smtp_port, 465);

        let fallback = provider_presets(None);
        assert_eq!(
            fallback
                .iter()
                .find(|preset| preset.id == "88")
                .map(|preset| preset.host.as_str()),
            Some("imap.88.com")
        );
        assert_eq!(
            fallback
                .iter()
                .find(|preset| preset.id == "88")
                .map(|preset| preset.smtp_host.as_str()),
            Some("smtp.88.com")
        );
    }

    #[tokio::test]
    async fn encoded_words_are_decoded_for_display() {
        let client = client();
        let store = client.core.store();
        let account = store
            .create_account(
                "imap",
                "me@example.com",
                None,
                "secret",
                &serde_json::json!({}),
            )
            .expect("account");
        let inbox = store
            .ensure_mailbox(account, "INBOX", Some("inbox"))
            .expect("inbox")
            .id;
        let mut encoded = message(1, "=?UTF-8?B?5byg5LiJ?=");
        encoded.from = vec!["=?UTF-8?B?5byg5LiJ?= <alice@example.com>".to_string()];
        store
            .apply_fetch(account, inbox, &[encoded], &[])
            .expect("fetch");

        let page = client
            .messages("me@example.com".into(), None, None, 10, 0)
            .await
            .expect("messages");
        assert_eq!(page.messages[0].subject, "张三");
        assert!(page.messages[0].from.contains("张三"));
    }

    #[tokio::test]
    async fn lists_accounts_mailboxes_and_messages() {
        let client = client();
        let store = client.core.store();
        let account = store
            .create_account(
                "imap",
                "me@example.com",
                None,
                "secret",
                &serde_json::json!({}),
            )
            .expect("account");
        let inbox = store
            .ensure_mailbox(account, "INBOX", Some("inbox"))
            .expect("inbox")
            .id;
        store
            .apply_fetch(account, inbox, &[message(1, "hello")], &[])
            .expect("fetch");

        assert_eq!(client.accounts().await.expect("accounts").len(), 1);
        let mailboxes = client
            .mailboxes("me@example.com".into())
            .await
            .expect("mailboxes");
        assert_eq!(mailboxes[0].name, "INBOX");
        assert_eq!(mailboxes[0].total, 1);
        assert_eq!(mailboxes[0].unread, 1);

        let page = client
            .messages("me@example.com".into(), Some(inbox), None, 50, 0)
            .await
            .expect("messages");
        assert_eq!(page.total, 1);
        assert_eq!(page.messages[0].subject, "hello");
        assert!(page.messages[0].unread);
    }
}
