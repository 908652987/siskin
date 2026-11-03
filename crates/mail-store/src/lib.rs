//! Local store: accounts, mailboxes, messages, change log, full-text index.

use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use chrono::Utc;
use rusqlite::{params, Connection, OptionalExtension, Transaction};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum StoreError {
    #[error("sqlite: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
    #[error("database key must be 32 bytes")]
    InvalidKey,
    #[error("database key does not match this file")]
    WrongKey,
    #[error("refusing to open unencrypted database: {0}")]
    PlaintextDatabase(PathBuf),
    #[error("account not found: {0}")]
    AccountNotFound(String),
    #[error("mailbox not found: {0}")]
    MailboxNotFound(String),
    #[error("invalid setting {key}: {value}")]
    InvalidSetting { key: String, value: String },
}

pub type Result<T, E = StoreError> = std::result::Result<T, E>;

const SCHEMA_VERSION: i64 = 7;
/// Trash retention when the user never chose one (three months).
pub const DEFAULT_TRASH_RETENTION_DAYS: u32 = 90;
/// Upper bound for retention; ten years is already effectively "keep forever".
pub const MAX_TRASH_RETENTION_DAYS: u32 = 3650;
const TRASH_RETENTION_KEY: &str = "trash_retention_days";
/// Change log rows kept before a snapshot replaces the log.
pub const DEFAULT_CHANGE_LOG_MAX_ROWS: i64 = 20_000;
/// Upper bound on the change log; one million rows is already ~50 MB.
pub const MAX_CHANGE_LOG_MAX_ROWS: i64 = 1_000_000;
const CHANGE_LOG_MAX_ROWS_KEY: &str = "change_log_max_rows";

/// Characters of body text kept for the full-text index. Measured on 10k
/// messages with CJK bodies: 1,000 chars add ~50 MB and 4,000 add ~160 MB of
/// index on top of the 124 MB of blobs, and a two-character LIKE costs 52 ms
/// versus 137 ms. 4,000 covers a typical message and keeps the index bounded.
pub const BODY_INDEX_CHARS: usize = 4_000;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Account {
    pub id: i64,
    pub provider: String,
    pub email: String,
    pub display_name: Option<String>,
    pub secret_ref: String,
    pub settings: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Mailbox {
    pub id: i64,
    pub account_id: i64,
    pub remote_name: String,
    pub role: Option<String>,
    pub uidvalidity: Option<i64>,
    pub uidnext: Option<i64>,
    pub highest_modseq: Option<i64>,
    pub last_sync_at: Option<i64>,
    pub last_flag_sync_at: Option<i64>,
    pub last_reconcile_at: Option<i64>,
    /// Set when the server no longer lists the mailbox; its messages are kept
    /// in the local trash until retention expires.
    pub missing_at: Option<i64>,
}

/// Outcome of one trash purge.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PurgeStats {
    pub mailboxes: usize,
    pub messages: usize,
}

#[derive(Debug, Clone, Default)]
pub struct SyncState {
    pub uidvalidity: Option<i64>,
    pub uidnext: Option<i64>,
    pub highest_modseq: Option<i64>,
    pub last_sync_at: Option<i64>,
    pub last_flag_sync_at: Option<i64>,
    pub last_reconcile_at: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoredMessage {
    pub id: i64,
    pub uid: i64,
    pub mailbox_id: i64,
    pub message_id: Option<String>,
    pub subject: Option<String>,
    pub from: Vec<String>,
    pub to: Vec<String>,
    pub sent_at: Option<i64>,
    pub size: Option<i64>,
    pub flags: Vec<String>,
    pub snippet: Option<String>,
}

/// Total and unread counts of one live mailbox, as the sync layer sees them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MailboxCount {
    pub mailbox_id: i64,
    pub total: i64,
    pub unread: i64,
}

/// One live location of a message. Keywords are per location in IMAP, so the
/// union across locations becomes the message's JMAP keywords.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MessageLocationInfo {
    pub mailbox_id: i64,
    pub flags: Vec<String>,
}

/// A message with every live location, for callers that must expose mailbox
/// membership (the Hub does; the CLI only shows one representative).
#[derive(Debug, Clone)]
pub struct MessageWithLocations {
    pub id: i64,
    pub message_id: Option<String>,
    pub thread_id: Option<String>,
    pub subject: Option<String>,
    pub from: Vec<String>,
    pub to: Vec<String>,
    pub sent_at: Option<i64>,
    pub size: Option<i64>,
    pub snippet: Option<String>,
    pub has_attach: bool,
    pub locations: Vec<MessageLocationInfo>,
}

/// Filter/sort parameters of the Hub's message list. `text` searches subject,
/// addresses, snippet and indexed body; `seen`/`flagged` filter on the union of
/// keywords across live locations.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MessageQuery {
    pub mailbox_id: Option<i64>,
    pub text: Option<String>,
    pub seen: Option<bool>,
    pub flagged: Option<bool>,
    /// Exclusive bounds on `sent_at` (Unix seconds); messages without a date
    /// never match them.
    pub before: Option<i64>,
    pub after: Option<i64>,
    pub limit: i64,
    pub offset: i64,
}

/// One page plus the total number of matches before paging.
#[derive(Debug, Clone, Default)]
pub struct MessagePage {
    pub messages: Vec<StoredMessage>,
    pub total: i64,
}

/// One paired Hub device. The token itself is never stored.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Device {
    pub id: i64,
    pub name: String,
    pub created_at: i64,
    pub last_seen_at: Option<i64>,
}

#[derive(Debug, Clone)]
pub struct NewMessage {
    pub uid: i64,
    pub message_id: Option<String>,
    pub subject: Option<String>,
    pub from: Vec<String>,
    pub to: Vec<String>,
    pub sent_at: Option<i64>,
    pub size: Option<i64>,
    pub flags: Vec<String>,
    pub snippet: Option<String>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ApplyStats {
    pub applied: usize,
    pub inserted: usize,
    pub removed: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChangeRecord {
    pub seq: i64,
    pub account_id: i64,
    pub entity_type: String,
    pub entity_id: String,
    pub op: String,
    pub at: i64,
}

/// Everything a client needs to rebuild an account without the change log.
/// Snapshots carry ids, locations and flags only; bodies stay content-addressed
/// and are fetched on demand.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AccountState {
    pub mailboxes: Vec<MailboxState>,
    pub messages: Vec<MessageState>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MailboxState {
    pub id: i64,
    pub remote_name: String,
    pub role: Option<String>,
    /// Set when the mailbox only exists in the local trash.
    pub missing_at: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MessageState {
    pub id: i64,
    pub message_id: Option<String>,
    pub locations: Vec<MessageLocationState>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MessageLocationState {
    pub mailbox_id: i64,
    pub uid: i64,
    pub flags: Vec<String>,
}

/// Full account state at `seq`; changes at or below `seq` are not retained.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChangeSnapshot {
    pub seq: i64,
    pub created_at: i64,
    pub state: AccountState,
}

/// What one compaction did; `None` means the log was already small enough.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Compaction {
    pub snapshot_seq: i64,
    pub removed_changes: usize,
}

/// Cursor bookkeeping for incremental consumers. A cursor older than
/// `oldest_seq` (or `snapshot_seq` when the log is empty) cannot be served and
/// the consumer must reload the snapshot instead.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChangeWindow {
    pub oldest_seq: Option<i64>,
    pub latest_seq: i64,
    pub snapshot_seq: Option<i64>,
}

/// How much of a message's content is cached locally.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum BodyState {
    /// Only headers were synced; the server layout is unknown.
    #[default]
    Headers,
    /// Layout or some parts are cached, but bodies or attachments are missing.
    Partial,
    /// Every body part and attachment is available offline.
    Full,
}

impl BodyState {
    pub fn as_str(self) -> &'static str {
        match self {
            BodyState::Headers => "headers",
            BodyState::Partial => "partial",
            BodyState::Full => "full",
        }
    }

    fn parse(value: &str) -> Self {
        match value {
            "full" => BodyState::Full,
            "partial" => BodyState::Partial,
            _ => BodyState::Headers,
        }
    }
}

/// One cached body alternative, as reported by BODYSTRUCTURE.
#[derive(Debug, Clone)]
pub struct StoredPart {
    pub part: String,
    pub content_type: String,
    pub charset: Option<String>,
    pub is_html: bool,
    pub cached: bool,
}

/// Attachment metadata; `downloaded` is false until its blob is stored.
#[derive(Debug, Clone)]
pub struct StoredAttachment {
    pub part: String,
    pub filename: Option<String>,
    pub content_type: String,
    pub size: i64,
    pub downloaded: bool,
}

/// Download information for one attachment.
#[derive(Debug, Clone)]
pub struct AttachmentMeta {
    pub filename: Option<String>,
    pub content_type: String,
    pub encoding: Option<String>,
    pub size: i64,
}

/// Everything cached for one message body. Text is decoded UTF-8; HTML is
/// already sanitised.
#[derive(Debug, Clone, Default)]
pub struct MessageBody {
    pub state: BodyState,
    pub text: Option<String>,
    pub html: Option<String>,
    pub parts: Vec<StoredPart>,
    pub attachments: Vec<StoredAttachment>,
}

/// Enough routing information to fetch a message's content from its server.
#[derive(Debug, Clone)]
pub struct MessageContext {
    pub account_id: i64,
    pub mailbox_id: i64,
    pub mailbox_name: String,
    pub uid: i64,
    pub state: BodyState,
}

/// Body alternative to cache; `data` is the decoded UTF-8 payload.
pub struct BodyPartInput<'a> {
    pub part: &'a str,
    pub content_type: &'a str,
    pub charset: Option<&'a str>,
    pub is_html: bool,
    pub data: &'a [u8],
}

/// Layout entry recorded before any body bytes exist.
pub struct PartMetaInput<'a> {
    pub part: &'a str,
    pub content_type: &'a str,
    pub charset: Option<&'a str>,
    pub is_html: bool,
    pub size: i64,
}

/// Attachment entry recorded before its bytes are downloaded.
pub struct AttachmentMetaInput<'a> {
    pub part: &'a str,
    pub filename: Option<&'a str>,
    pub content_type: &'a str,
    /// Transfer encoding (`base64`, `quoted-printable`, ...) for decoding.
    pub encoding: Option<&'a str>,
    pub size: i64,
}

/// 32-byte key for the encrypted database. SQLCipher derives its own page and
/// HMAC keys from it, so no second KDF runs here.
pub struct DbKey(zeroize::Zeroizing<[u8; 32]>);

impl DbKey {
    pub fn from_bytes(bytes: &[u8]) -> Result<Self> {
        if bytes.len() != 32 {
            return Err(StoreError::InvalidKey);
        }
        let mut buf = [0u8; 32];
        buf.copy_from_slice(bytes);
        Ok(Self(zeroize::Zeroizing::new(buf)))
    }

    /// SQLCipher raw-key syntax; the key never passes through a derived
    /// passphrase, so the database is only openable with these exact bytes.
    fn raw_key(&self) -> zeroize::Zeroizing<String> {
        use std::fmt::Write as _;
        let mut out = zeroize::Zeroizing::new(String::with_capacity(68));
        out.push_str("x'");
        for byte in self.0.iter() {
            let _ = write!(out, "{byte:02x}");
        }
        out.push('\'');
        out
    }
}

const PLAINTEXT_HEADER: &[u8; 16] = b"SQLite format 3\0";

pub struct Store {
    conn: Mutex<Connection>,
}

impl Store {
    pub fn open(path: impl AsRef<Path>, key: &DbKey) -> Result<Self> {
        let path = path.as_ref();
        reject_plaintext(path)?;
        Self::init(Connection::open(path)?, key)
    }

    pub fn open_in_memory(key: &DbKey) -> Result<Self> {
        Self::init(Connection::open_in_memory()?, key)
    }

    fn init(conn: Connection, key: &DbKey) -> Result<Self> {
        let key_pragma = key.raw_key();
        conn.execute_batch(&format!("PRAGMA key = \"{}\";", key_pragma.as_str()))?;
        // Reading the schema forces SQLCipher to decrypt page 1, so a wrong key
        // fails here instead of on the first real query.
        match conn.query_row("SELECT count(*) FROM sqlite_master", [], |_| Ok(())) {
            Ok(()) => {}
            Err(rusqlite::Error::SqliteFailure(err, _))
                if err.code == rusqlite::ErrorCode::NotADatabase =>
            {
                return Err(StoreError::WrongKey);
            }
            Err(err) => return Err(err.into()),
        }
        let _ = conn.pragma_update(None, "journal_mode", "WAL");
        conn.pragma_update(None, "foreign_keys", "ON")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        let store = Self {
            conn: Mutex::new(conn),
        };
        store.migrate()?;
        Ok(store)
    }

    fn migrate(&self) -> Result<()> {
        let mut conn = self.conn.lock().expect("poisoned");
        conn.execute_batch(SCHEMA)?;
        let current: i64 = conn.pragma_query_value(None, "user_version", |row| row.get(0))?;
        // Columns added after the first release; the check keeps fresh databases
        // (created from SCHEMA) from applying the same ALTER twice.
        if !has_column(&conn, "mailboxes", "last_reconcile_at")? {
            conn.execute_batch("ALTER TABLE mailboxes ADD COLUMN last_reconcile_at INTEGER;")?;
        }
        if !has_column(&conn, "mailboxes", "missing_at")? {
            conn.execute_batch("ALTER TABLE mailboxes ADD COLUMN missing_at INTEGER;")?;
        }
        if !has_column(&conn, "messages", "body_text")? {
            conn.execute_batch("ALTER TABLE messages ADD COLUMN body_text TEXT;")?;
        }
        conn.execute_batch(FTS_SCHEMA)?;
        if !has_column(&conn, "message_fts", "body_text")? {
            rebuild_fts(&mut conn)?;
        }
        if current < SCHEMA_VERSION {
            conn.pragma_update(None, "user_version", SCHEMA_VERSION)?;
        }
        Ok(())
    }

    pub fn create_account(
        &self,
        provider: &str,
        email: &str,
        display_name: Option<&str>,
        secret_ref: &str,
        settings: &serde_json::Value,
    ) -> Result<i64> {
        let conn = self.conn.lock().expect("poisoned");
        conn.execute(
            "INSERT INTO accounts (provider, email, display_name, secret_ref, settings, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT(email) DO UPDATE SET provider = excluded.provider,
                 display_name = excluded.display_name, secret_ref = excluded.secret_ref,
                 settings = excluded.settings",
            params![
                provider,
                email,
                display_name,
                secret_ref,
                settings.to_string(),
                Utc::now().timestamp()
            ],
        )?;
        Ok(
            conn.query_row("SELECT id FROM accounts WHERE email = ?1", [email], |row| {
                row.get(0)
            })?,
        )
    }

    pub fn account_by_email(&self, email: &str) -> Result<Option<Account>> {
        self.query_account("email", email)
    }

    pub fn account_by_id(&self, id: i64) -> Result<Option<Account>> {
        self.query_account("id", &id.to_string())
    }

    fn query_account(&self, column: &str, value: &str) -> Result<Option<Account>> {
        let conn = self.conn.lock().expect("poisoned");
        let sql = format!(
            "SELECT id, provider, email, display_name, secret_ref, settings FROM accounts WHERE {column} = ?1"
        );
        let raw: Option<(i64, String, String, Option<String>, String, String)> = conn
            .query_row(&sql, [value], |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                    row.get(5)?,
                ))
            })
            .optional()?;
        match raw {
            None => Ok(None),
            Some((id, provider, email, display_name, secret_ref, settings)) => Ok(Some(Account {
                id,
                provider,
                email,
                display_name,
                secret_ref,
                settings: serde_json::from_str(&settings)?,
            })),
        }
    }

    pub fn list_accounts(&self) -> Result<Vec<Account>> {
        let conn = self.conn.lock().expect("poisoned");
        let mut stmt = conn.prepare(
            "SELECT id, provider, email, display_name, secret_ref, settings FROM accounts ORDER BY id",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, Option<String>>(3)?,
                row.get::<_, String>(4)?,
                row.get::<_, String>(5)?,
            ))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let (id, provider, email, display_name, secret_ref, settings) = row?;
            out.push(Account {
                id,
                provider,
                email,
                display_name,
                secret_ref,
                settings: serde_json::from_str(&settings)?,
            });
        }
        Ok(out)
    }

    pub fn ensure_mailbox(
        &self,
        account_id: i64,
        remote_name: &str,
        role: Option<&str>,
    ) -> Result<Mailbox> {
        let conn = self.conn.lock().expect("poisoned");
        conn.execute(
            "INSERT INTO mailboxes (account_id, remote_name, role) VALUES (?1, ?2, ?3)
             ON CONFLICT(account_id, remote_name) DO UPDATE SET
                 role = COALESCE(excluded.role, mailboxes.role),
                 missing_at = NULL",
            params![account_id, remote_name, role],
        )?;
        let id: i64 = conn.query_row(
            "SELECT id FROM mailboxes WHERE account_id = ?1 AND remote_name = ?2",
            params![account_id, remote_name],
            |row| row.get(0),
        )?;
        Self::load_mailbox(&conn, id)?
            .ok_or_else(|| StoreError::MailboxNotFound(remote_name.into()))
    }

    /// Every mailbox of an account, ordered by remote name.
    pub fn list_mailboxes(&self, account_id: i64) -> Result<Vec<Mailbox>> {
        let conn = self.conn.lock().expect("poisoned");
        let mut stmt =
            conn.prepare("SELECT id FROM mailboxes WHERE account_id = ?1 ORDER BY remote_name")?;
        let ids: Vec<i64> = stmt
            .query_map([account_id], |row| row.get(0))?
            .collect::<rusqlite::Result<_>>()?;
        drop(stmt);
        let mut out = Vec::with_capacity(ids.len());
        for id in ids {
            if let Some(mailbox) = Self::load_mailbox(&conn, id)? {
                out.push(mailbox);
            }
        }
        Ok(out)
    }

    /// Marks mailboxes that the server no longer lists as missing. Their
    /// messages stay in the local trash until retention expires. INBOX is
    /// always treated as present: servers must list it, but a broken LIST
    /// response must not drop the primary mailbox.
    pub fn mark_missing_mailboxes(
        &self,
        account_id: i64,
        present: &[String],
        now: i64,
    ) -> Result<usize> {
        let mut conn = self.conn.lock().expect("poisoned");
        let tx = conn.transaction()?;
        let rows: Vec<(i64, String, Option<i64>)> = {
            let mut stmt = tx.prepare(
                "SELECT id, remote_name, missing_at FROM mailboxes WHERE account_id = ?1",
            )?;
            let rows = stmt.query_map([account_id], |row| {
                Ok((row.get(0)?, row.get(1)?, row.get(2)?))
            })?;
            rows.collect::<rusqlite::Result<_>>()?
        };

        let mut marked = 0usize;
        for (id, name, missing_at) in rows {
            let still_present = name.eq_ignore_ascii_case("INBOX")
                || present.iter().any(|present| present == &name);
            match (still_present, missing_at) {
                (false, None) => {
                    tx.execute(
                        "UPDATE mailboxes SET missing_at = ?2 WHERE id = ?1",
                        params![id, now],
                    )?;
                    record_change(&tx, account_id, "mailbox", &id.to_string(), "removed")?;
                    marked += 1;
                }
                (true, Some(_)) => {
                    tx.execute("UPDATE mailboxes SET missing_at = NULL WHERE id = ?1", [id])?;
                    record_change(&tx, account_id, "mailbox", &id.to_string(), "restored")?;
                }
                _ => {}
            }
        }
        tx.commit()?;
        Ok(marked)
    }

    /// Renames a vanished mailbox out of the way so a recreated mailbox can
    /// own its server name; the retired row keeps its messages (and retention
    /// clock) until purge. Returns the retired name.
    pub fn retire_mailbox(&self, mailbox_id: i64) -> Result<String> {
        let mut conn = self.conn.lock().expect("poisoned");
        let tx = conn.transaction()?;
        let (account_id, name): (i64, String) = tx.query_row(
            "SELECT account_id, remote_name FROM mailboxes WHERE id = ?1",
            [mailbox_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        let retired = format!("{name} (local trash {mailbox_id})");
        tx.execute(
            "UPDATE mailboxes SET remote_name = ?2, role = NULL WHERE id = ?1",
            params![mailbox_id, retired],
        )?;
        record_change(
            &tx,
            account_id,
            "mailbox",
            &mailbox_id.to_string(),
            "retired",
        )?;
        tx.commit()?;
        Ok(retired)
    }

    fn load_mailbox(conn: &Connection, id: i64) -> Result<Option<Mailbox>> {
        struct Row {
            id: i64,
            account_id: i64,
            remote_name: String,
            role: Option<String>,
            uidvalidity: Option<i64>,
            uidnext: Option<i64>,
            highest_modseq: Option<i64>,
            last_sync_at: Option<i64>,
            last_flag_sync_at: Option<i64>,
            last_reconcile_at: Option<i64>,
            missing_at: Option<i64>,
        }

        let raw: Option<Row> = conn
            .query_row(
                "SELECT id, account_id, remote_name, role, uidvalidity, uidnext,
                        highest_modseq, last_sync_at, last_flag_sync_at, last_reconcile_at,
                        missing_at
                 FROM mailboxes WHERE id = ?1",
                [id],
                |row| {
                    Ok(Row {
                        id: row.get(0)?,
                        account_id: row.get(1)?,
                        remote_name: row.get(2)?,
                        role: row.get(3)?,
                        uidvalidity: row.get(4)?,
                        uidnext: row.get(5)?,
                        highest_modseq: row.get(6)?,
                        last_sync_at: row.get(7)?,
                        last_flag_sync_at: row.get(8)?,
                        last_reconcile_at: row.get(9)?,
                        missing_at: row.get(10)?,
                    })
                },
            )
            .optional()?;

        Ok(raw.map(|row| Mailbox {
            id: row.id,
            account_id: row.account_id,
            remote_name: row.remote_name,
            role: row.role,
            uidvalidity: row.uidvalidity,
            uidnext: row.uidnext,
            highest_modseq: row.highest_modseq,
            last_sync_at: row.last_sync_at,
            last_flag_sync_at: row.last_flag_sync_at,
            last_reconcile_at: row.last_reconcile_at,
            missing_at: row.missing_at,
        }))
    }

    pub fn mailbox_by_name(&self, account_id: i64, name: &str) -> Result<Option<Mailbox>> {
        let conn = self.conn.lock().expect("poisoned");
        let id: Option<i64> = conn
            .query_row(
                "SELECT id FROM mailboxes WHERE account_id = ?1 AND remote_name = ?2",
                params![account_id, name],
                |row| row.get(0),
            )
            .optional()?;
        match id {
            Some(id) => Self::load_mailbox(&conn, id),
            None => Ok(None),
        }
    }

    pub fn update_sync_state(&self, mailbox_id: i64, state: &SyncState) -> Result<()> {
        let conn = self.conn.lock().expect("poisoned");
        conn.execute(
            "UPDATE mailboxes SET uidvalidity = ?2, uidnext = ?3, highest_modseq = ?4,
                    last_sync_at = COALESCE(?5, last_sync_at),
                    last_flag_sync_at = COALESCE(?6, last_flag_sync_at),
                    last_reconcile_at = COALESCE(?7, last_reconcile_at)
             WHERE id = ?1",
            params![
                mailbox_id,
                state.uidvalidity,
                state.uidnext,
                state.highest_modseq,
                state.last_sync_at,
                state.last_flag_sync_at,
                state.last_reconcile_at
            ],
        )?;
        Ok(())
    }

    /// Drops every cached message of a mailbox whose UIDVALIDITY changed.
    pub fn reset_mailbox(&self, mailbox_id: i64, uidvalidity: i64) -> Result<usize> {
        let mut conn = self.conn.lock().expect("poisoned");
        let tx = conn.transaction()?;
        let removed = tx.execute(
            "DELETE FROM message_locations WHERE mailbox_id = ?1",
            [mailbox_id],
        )?;
        purge_orphan_messages(&tx)?;
        tx.execute(
            "UPDATE mailboxes SET uidvalidity = ?2, uidnext = NULL, highest_modseq = NULL,
                    last_sync_at = NULL, last_flag_sync_at = NULL WHERE id = ?1",
            params![mailbox_id, uidvalidity],
        )?;
        tx.commit()?;
        Ok(removed)
    }

    pub fn apply_fetch(
        &self,
        account_id: i64,
        mailbox_id: i64,
        messages: &[NewMessage],
        removed_uids: &[i64],
    ) -> Result<ApplyStats> {
        let mut conn = self.conn.lock().expect("poisoned");
        let tx = conn.transaction()?;
        let mut stats = ApplyStats::default();

        for msg in messages {
            stats.inserted += upsert_message(&tx, account_id, mailbox_id, msg)?;
            stats.applied += 1;
        }

        // Removals must be visible to incremental consumers (the Hub's
        // Email/changes reports them as destroyed), so capture the message id
        // before dropping the location.
        let mut orphans = false;
        for uid in removed_uids {
            let message_id: Option<i64> = tx
                .query_row(
                    "SELECT message_id FROM message_locations WHERE mailbox_id = ?1 AND uid = ?2",
                    params![mailbox_id, *uid],
                    |row| row.get(0),
                )
                .optional()?;
            let removed = tx.execute(
                "DELETE FROM message_locations WHERE mailbox_id = ?1 AND uid = ?2",
                params![mailbox_id, *uid],
            )?;
            stats.removed += removed;
            if let (1, Some(message_id)) = (removed, message_id) {
                let remaining: i64 = tx.query_row(
                    "SELECT COUNT(*) FROM message_locations WHERE message_id = ?1",
                    [message_id],
                    |row| row.get(0),
                )?;
                record_change(
                    &tx,
                    account_id,
                    "message",
                    &message_id.to_string(),
                    if remaining == 0 { "removed" } else { "updated" },
                )?;
                orphans = true;
            }
        }
        if orphans {
            purge_orphan_messages(&tx)?;
        }

        if stats.applied > 0 || stats.removed > 0 {
            record_change(
                &tx,
                account_id,
                "mailbox",
                &mailbox_id.to_string(),
                "updated",
            )?;
        }
        tx.commit()?;
        Ok(stats)
    }

    /// Messages visible outside the trash: one row per message even when the
    /// same message is stored in several mailboxes (INBOX and Archive, say).
    pub fn list_messages(
        &self,
        account_id: i64,
        limit: i64,
        offset: i64,
    ) -> Result<Vec<StoredMessage>> {
        let conn = self.conn.lock().expect("poisoned");
        let mut stmt = conn.prepare(
            "SELECT m.id, l.uid, l.mailbox_id, m.message_id, m.subject, m.from_json, m.to_json,
                    m.sent_at, m.size, l.flags, m.snippet
             FROM messages m
             JOIN message_locations l ON l.message_id = m.id
             WHERE m.account_id = ?1
               AND l.mailbox_id = (
                   SELECT l2.mailbox_id FROM message_locations l2
                   JOIN mailboxes b2 ON b2.id = l2.mailbox_id
                   WHERE l2.message_id = m.id AND b2.missing_at IS NULL
                   ORDER BY l2.mailbox_id LIMIT 1)
             ORDER BY COALESCE(m.sent_at, 0) DESC, m.id DESC
             LIMIT ?2 OFFSET ?3",
        )?;
        let rows = stmt.query_map(params![account_id, limit, offset], |row| {
            Ok(StoredMessage {
                id: row.get(0)?,
                uid: row.get(1)?,
                mailbox_id: row.get(2)?,
                message_id: row.get(3)?,
                subject: row.get(4)?,
                from: serde_json::from_str(&row.get::<_, String>(5)?).unwrap_or_default(),
                to: serde_json::from_str(&row.get::<_, String>(6)?).unwrap_or_default(),
                sent_at: row.get(7)?,
                size: row.get(8)?,
                flags: serde_json::from_str(&row.get::<_, String>(9)?).unwrap_or_default(),
                snippet: row.get(10)?,
            })
        })?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    /// Full-text search. Queries shorter than three characters fall back to
    /// LIKE, because the trigram tokenizer cannot index shorter sequences
    /// (relevant for Chinese, where two-character words are common).
    pub fn search_messages(
        &self,
        account_id: i64,
        query: &str,
        limit: i64,
    ) -> Result<Vec<StoredMessage>> {
        let trimmed = query.trim();
        if trimmed.is_empty() {
            return Ok(Vec::new());
        }
        let conn = self.conn.lock().expect("poisoned");
        let columns = "m.id, l.uid, l.mailbox_id, m.message_id, m.subject, m.from_json, m.to_json,
                       m.sent_at, m.size, l.flags, m.snippet";
        let live_location = "l.mailbox_id = (
                   SELECT l2.mailbox_id FROM message_locations l2
                   JOIN mailboxes b2 ON b2.id = l2.mailbox_id
                   WHERE l2.message_id = m.id AND b2.missing_at IS NULL
                   ORDER BY l2.mailbox_id LIMIT 1)";
        let mut out = Vec::new();
        if trimmed.chars().count() < 3 {
            let pattern = format!("%{trimmed}%");
            let mut stmt = conn.prepare(&format!(
                "SELECT {columns} FROM messages m
                 JOIN message_locations l ON l.message_id = m.id
                 WHERE m.account_id = ?1
                   AND {live_location}
                   AND (m.subject LIKE ?2 OR m.from_json LIKE ?2 OR m.snippet LIKE ?2
                        OR m.body_text LIKE ?2)
                 ORDER BY COALESCE(m.sent_at, 0) DESC LIMIT ?3"
            ))?;
            let rows = stmt.query_map(params![account_id, pattern, limit], map_message)?;
            for row in rows {
                out.push(row?);
            }
        } else {
            let phrase = format!("\"{}\"", trimmed.replace('"', "\"\""));
            let mut stmt = conn.prepare(&format!(
                "SELECT {columns} FROM message_fts f
                 JOIN messages m ON m.id = f.rowid
                 JOIN message_locations l ON l.message_id = m.id
                 WHERE message_fts MATCH ?1 AND m.account_id = ?2
                   AND {live_location}
                 ORDER BY COALESCE(m.sent_at, 0) DESC LIMIT ?3"
            ))?;
            let rows = stmt.query_map(params![phrase, account_id, limit], map_message)?;
            for row in rows {
                out.push(row?);
            }
        }
        Ok(out)
    }

    pub fn count_messages(&self, account_id: i64) -> Result<i64> {
        let conn = self.conn.lock().expect("poisoned");
        Ok(conn.query_row(
            "SELECT COUNT(*) FROM messages m
             WHERE m.account_id = ?1 AND EXISTS (
                 SELECT 1 FROM message_locations l
                 JOIN mailboxes b ON b.id = l.mailbox_id
                 WHERE l.message_id = m.id AND b.missing_at IS NULL)",
            [account_id],
            |row| row.get(0),
        )?)
    }

    /// Live mailbox totals and unread counts. `\Seen` is tracked per location,
    /// so each location is counted once; mailboxes without messages are absent.
    pub fn mailbox_counts(&self, account_id: i64) -> Result<Vec<MailboxCount>> {
        let conn = self.conn.lock().expect("poisoned");
        let mut stmt = conn.prepare(
            r#"SELECT l.mailbox_id, COUNT(*),
                      SUM(CASE WHEN EXISTS (
                            SELECT 1 FROM json_each(l.flags) WHERE json_each.value = '\Seen'
                          ) THEN 0 ELSE 1 END)
               FROM message_locations l
               JOIN mailboxes b ON b.id = l.mailbox_id
               WHERE b.account_id = ?1 AND b.missing_at IS NULL
               GROUP BY l.mailbox_id"#,
        )?;
        let rows = stmt.query_map([account_id], |row| {
            Ok(MailboxCount {
                mailbox_id: row.get(0)?,
                total: row.get(1)?,
                unread: row.get(2)?,
            })
        })?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    /// Filtered, paged message list. All filters are applied in SQL with the
    /// same representative-location rule as `list_messages`, so paging and
    /// totals agree.
    pub fn query_messages(&self, account_id: i64, query: &MessageQuery) -> Result<MessagePage> {
        let conn = self.conn.lock().expect("poisoned");
        let mut conditions = String::from(
            " AND EXISTS (SELECT 1 FROM message_locations ll
                           JOIN mailboxes bb ON bb.id = ll.mailbox_id
                           WHERE ll.message_id = m.id AND bb.missing_at IS NULL)",
        );
        let mut params: Vec<rusqlite::types::Value> = vec![account_id.into()];

        if let Some(mailbox_id) = query.mailbox_id {
            conditions.push_str(
                " AND EXISTS (SELECT 1 FROM message_locations lm
                               JOIN mailboxes bm ON bm.id = lm.mailbox_id
                               WHERE lm.message_id = m.id AND lm.mailbox_id = ?
                                 AND bm.missing_at IS NULL)",
            );
            params.push(mailbox_id.into());
        }
        // Keywords are the union across live locations, so a message archived
        // after being read stays seen.
        let keyword_clause = |flag: &str, present: bool| {
            let op = if present { " AND" } else { " AND NOT" };
            format!(
                r#"{op} EXISTS (SELECT 1 FROM message_locations lk
                                 JOIN mailboxes bk ON bk.id = lk.mailbox_id
                                 WHERE lk.message_id = m.id AND bk.missing_at IS NULL
                                   AND EXISTS (SELECT 1 FROM json_each(lk.flags)
                                               WHERE json_each.value = '{flag}'))"#
            )
        };
        if let Some(value) = query.seen {
            conditions.push_str(&keyword_clause("\\Seen", value));
        }
        if let Some(value) = query.flagged {
            conditions.push_str(&keyword_clause("\\Flagged", value));
        }
        if let Some(text) = query.text.as_deref() {
            let trimmed = text.trim();
            if !trimmed.is_empty() {
                if trimmed.chars().count() < 3 {
                    let pattern = format!("%{trimmed}%");
                    conditions.push_str(
                        " AND (m.subject LIKE ? OR m.from_json LIKE ? OR m.to_json LIKE ?
                               OR m.snippet LIKE ? OR m.body_text LIKE ?)",
                    );
                    for _ in 0..5 {
                        params.push(pattern.clone().into());
                    }
                } else {
                    let phrase = format!("\"{}\"", trimmed.replace('"', "\"\""));
                    conditions.push_str(
                        " AND m.id IN (SELECT rowid FROM message_fts WHERE message_fts MATCH ?)",
                    );
                    params.push(phrase.into());
                }
            }
        }
        if let Some(before) = query.before {
            conditions.push_str(" AND m.sent_at IS NOT NULL AND m.sent_at < ?");
            params.push(before.into());
        }
        if let Some(after) = query.after {
            conditions.push_str(" AND m.sent_at IS NOT NULL AND m.sent_at > ?");
            params.push(after.into());
        }

        let total: i64 = conn.query_row(
            &format!("SELECT COUNT(*) FROM messages m WHERE m.account_id = ?1{conditions}"),
            rusqlite::params_from_iter(params.iter()),
            |row| row.get(0),
        )?;

        let sql = format!(
            "SELECT m.id, l.uid, l.mailbox_id, m.message_id, m.subject, m.from_json, m.to_json,
                    m.sent_at, m.size, l.flags, m.snippet
             FROM messages m
             JOIN message_locations l ON l.message_id = m.id
             WHERE m.account_id = ?1
               AND l.mailbox_id = (
                   SELECT l2.mailbox_id FROM message_locations l2
                   JOIN mailboxes b2 ON b2.id = l2.mailbox_id
                   WHERE l2.message_id = m.id AND b2.missing_at IS NULL
                   ORDER BY l2.mailbox_id LIMIT 1)
               {conditions}
             ORDER BY COALESCE(m.sent_at, 0) DESC, m.id DESC
             LIMIT ? OFFSET ?"
        );
        params.push(query.limit.into());
        params.push(query.offset.into());
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map(rusqlite::params_from_iter(params.iter()), map_message)?;
        let mut messages = Vec::new();
        for row in rows {
            messages.push(row?);
        }
        Ok(MessagePage { messages, total })
    }

    /// Messages by local id with every live location, for callers that must
    /// report all mailbox memberships. Ids that do not exist are omitted.
    pub fn messages_with_locations(
        &self,
        account_id: i64,
        ids: &[i64],
    ) -> Result<Vec<MessageWithLocations>> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        let conn = self.conn.lock().expect("poisoned");
        let placeholders = std::iter::repeat_n("?", ids.len())
            .collect::<Vec<_>>()
            .join(", ");
        let mut params: Vec<rusqlite::types::Value> = vec![account_id.into()];
        params.extend(ids.iter().map(|id| rusqlite::types::Value::from(*id)));

        let sql = format!(
            "SELECT m.id, m.message_id, m.thread_id, m.subject, m.from_json, m.to_json,
                    m.sent_at, m.size, m.snippet, m.has_attach
             FROM messages m
             WHERE m.account_id = ?1 AND m.id IN ({placeholders})
             ORDER BY m.id"
        );
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map(rusqlite::params_from_iter(params.iter()), |row| {
            Ok((
                MessageWithLocations {
                    id: row.get(0)?,
                    message_id: row.get(1)?,
                    thread_id: row.get(2)?,
                    subject: row.get(3)?,
                    from: serde_json::from_str(&row.get::<_, String>(4)?).unwrap_or_default(),
                    to: serde_json::from_str(&row.get::<_, String>(5)?).unwrap_or_default(),
                    sent_at: row.get(6)?,
                    size: row.get(7)?,
                    snippet: row.get(8)?,
                    has_attach: row.get::<_, i64>(9)? != 0,
                    locations: Vec::new(),
                },
                row.get::<_, i64>(0)?,
            ))
        })?;
        let mut messages = Vec::new();
        for row in rows {
            messages.push(row?);
        }

        let location_sql = format!(
            "SELECT l.message_id, l.mailbox_id, l.flags
             FROM message_locations l
             JOIN mailboxes b ON b.id = l.mailbox_id
             WHERE b.account_id = ?1 AND b.missing_at IS NULL
               AND l.message_id IN ({placeholders})
             ORDER BY l.message_id, l.mailbox_id"
        );
        let mut stmt = conn.prepare(&location_sql)?;
        let rows = stmt.query_map(rusqlite::params_from_iter(params.iter()), |row| {
            Ok((
                row.get::<_, i64>(0)?,
                MessageLocationInfo {
                    mailbox_id: row.get(1)?,
                    flags: serde_json::from_str(&row.get::<_, String>(2)?).unwrap_or_default(),
                },
            ))
        })?;
        let mut by_id: std::collections::HashMap<i64, Vec<MessageLocationInfo>> =
            std::collections::HashMap::new();
        for row in rows {
            let (id, location) = row?;
            by_id.entry(id).or_default().push(location);
        }
        let mut out = Vec::with_capacity(messages.len());
        for (message, id) in messages {
            let mut message = message;
            message.locations = by_id.remove(&id).unwrap_or_default();
            out.push(message);
        }
        Ok(out)
    }

    /// Registers a paired device. `token_hash` is a hash of the bearer token,
    /// never the token itself.
    pub fn create_device(&self, name: &str, token_hash: &str) -> Result<i64> {
        let conn = self.conn.lock().expect("poisoned");
        conn.execute(
            "INSERT INTO hub_devices (name, token_hash, created_at) VALUES (?1, ?2, ?3)",
            params![name, token_hash, Utc::now().timestamp()],
        )?;
        Ok(conn.last_insert_rowid())
    }

    pub fn device_by_token_hash(&self, token_hash: &str) -> Result<Option<Device>> {
        let conn = self.conn.lock().expect("poisoned");
        Ok(conn
            .query_row(
                "SELECT id, name, created_at, last_seen_at FROM hub_devices
                 WHERE token_hash = ?1",
                [token_hash],
                |row| {
                    Ok(Device {
                        id: row.get(0)?,
                        name: row.get(1)?,
                        created_at: row.get(2)?,
                        last_seen_at: row.get(3)?,
                    })
                },
            )
            .optional()?)
    }

    pub fn touch_device(&self, id: i64) -> Result<()> {
        let conn = self.conn.lock().expect("poisoned");
        conn.execute(
            "UPDATE hub_devices SET last_seen_at = ?2 WHERE id = ?1",
            params![id, Utc::now().timestamp()],
        )?;
        Ok(())
    }

    /// Messages whose only copies live in mailboxes the server no longer
    /// lists; they are purged once retention expires.
    pub fn list_trash(
        &self,
        account_id: i64,
        limit: i64,
        offset: i64,
    ) -> Result<Vec<StoredMessage>> {
        let conn = self.conn.lock().expect("poisoned");
        let mut stmt = conn.prepare(
            "SELECT m.id, l.uid, l.mailbox_id, m.message_id, m.subject, m.from_json, m.to_json,
                    m.sent_at, m.size, l.flags, m.snippet
             FROM messages m
             JOIN message_locations l ON l.message_id = m.id
             JOIN mailboxes b ON b.id = l.mailbox_id
             WHERE m.account_id = ?1 AND b.missing_at IS NOT NULL
               AND l.mailbox_id = (
                   SELECT l2.mailbox_id FROM message_locations l2
                   JOIN mailboxes b2 ON b2.id = l2.mailbox_id
                   WHERE l2.message_id = m.id AND b2.missing_at IS NOT NULL
                   ORDER BY l2.mailbox_id LIMIT 1)
             ORDER BY COALESCE(m.sent_at, 0) DESC, m.id DESC
             LIMIT ?2 OFFSET ?3",
        )?;
        let rows = stmt.query_map(params![account_id, limit, offset], map_message)?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    /// Drops mailboxes missing for at least `retention_days`, then removes the
    /// messages they kept alive. Ephemeral `now` keeps the cutoff testable.
    pub fn purge_trash(&self, retention_days: u32, now: i64) -> Result<PurgeStats> {
        let cutoff = now - i64::from(retention_days) * 86_400;
        let mut conn = self.conn.lock().expect("poisoned");
        let tx = conn.transaction()?;
        let mailboxes = tx.execute(
            "DELETE FROM mailboxes WHERE missing_at IS NOT NULL AND missing_at <= ?1",
            [cutoff],
        )?;
        let messages = purge_orphan_messages(&tx)?;
        tx.commit()?;
        Ok(PurgeStats {
            mailboxes,
            messages,
        })
    }

    pub fn setting(&self, key: &str) -> Result<Option<String>> {
        let conn = self.conn.lock().expect("poisoned");
        Ok(conn
            .query_row("SELECT value FROM settings WHERE key = ?1", [key], |row| {
                row.get(0)
            })
            .optional()?)
    }

    pub fn set_setting(&self, key: &str, value: &str) -> Result<()> {
        let conn = self.conn.lock().expect("poisoned");
        conn.execute(
            "INSERT INTO settings (key, value) VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![key, value],
        )?;
        Ok(())
    }

    /// Days a missing mailbox is kept locally before its messages are deleted.
    pub fn trash_retention_days(&self) -> Result<u32> {
        let Some(raw) = self.setting(TRASH_RETENTION_KEY)? else {
            return Ok(DEFAULT_TRASH_RETENTION_DAYS);
        };
        raw.parse()
            .ok()
            .filter(|days| *days >= 1 && *days <= MAX_TRASH_RETENTION_DAYS)
            .ok_or_else(|| StoreError::InvalidSetting {
                key: TRASH_RETENTION_KEY.to_string(),
                value: raw,
            })
    }

    pub fn set_trash_retention_days(&self, days: u32) -> Result<()> {
        if !(1..=MAX_TRASH_RETENTION_DAYS).contains(&days) {
            return Err(StoreError::InvalidSetting {
                key: TRASH_RETENTION_KEY.to_string(),
                value: days.to_string(),
            });
        }
        self.set_setting(TRASH_RETENTION_KEY, &days.to_string())
    }

    /// Change log size that triggers compaction into a snapshot.
    pub fn change_log_max_rows(&self) -> Result<i64> {
        let Some(raw) = self.setting(CHANGE_LOG_MAX_ROWS_KEY)? else {
            return Ok(DEFAULT_CHANGE_LOG_MAX_ROWS);
        };
        raw.parse()
            .ok()
            .filter(|rows| (1..=MAX_CHANGE_LOG_MAX_ROWS).contains(rows))
            .ok_or_else(|| StoreError::InvalidSetting {
                key: CHANGE_LOG_MAX_ROWS_KEY.to_string(),
                value: raw,
            })
    }

    pub fn set_change_log_max_rows(&self, rows: i64) -> Result<()> {
        if !(1..=MAX_CHANGE_LOG_MAX_ROWS).contains(&rows) {
            return Err(StoreError::InvalidSetting {
                key: CHANGE_LOG_MAX_ROWS_KEY.to_string(),
                value: rows.to_string(),
            });
        }
        self.set_setting(CHANGE_LOG_MAX_ROWS_KEY, &rows.to_string())
    }

    /// Routing data needed to fetch one message's content from its server.
    /// Live mailboxes win over trash copies of the same message.
    pub fn message_context(&self, message_id: i64) -> Result<Option<MessageContext>> {
        let conn = self.conn.lock().expect("poisoned");
        conn.query_row(
            "SELECT m.account_id, l.mailbox_id, b.remote_name, l.uid, m.body_state
             FROM messages m
             JOIN message_locations l ON l.message_id = m.id
             JOIN mailboxes b ON b.id = l.mailbox_id
             WHERE m.id = ?1
             ORDER BY (b.missing_at IS NOT NULL), l.mailbox_id LIMIT 1",
            [message_id],
            |row| {
                Ok(MessageContext {
                    account_id: row.get(0)?,
                    mailbox_id: row.get(1)?,
                    mailbox_name: row.get(2)?,
                    uid: row.get(3)?,
                    state: BodyState::parse(&row.get::<_, String>(4)?),
                })
            },
        )
        .optional()
        .map_err(Into::into)
    }

    /// Records what a server says a message contains, without fetching bytes.
    /// Parts missing from the new layout are dropped and their blobs released.
    pub fn record_body_structure(
        &self,
        account_id: i64,
        message_id: i64,
        parts: &[PartMetaInput<'_>],
        attachments: &[AttachmentMetaInput<'_>],
    ) -> Result<()> {
        let mut conn = self.conn.lock().expect("poisoned");
        let tx = conn.transaction()?;

        let kept_parts: std::collections::HashSet<&str> =
            parts.iter().map(|part| part.part).collect();
        let kept_attachments: std::collections::HashSet<&str> = attachments
            .iter()
            .map(|attachment| attachment.part)
            .collect();

        prune_rows(&tx, "message_parts", message_id, &kept_parts)?;
        prune_rows(&tx, "attachments", message_id, &kept_attachments)?;

        for part in parts {
            tx.execute(
                "INSERT INTO message_parts (message_id, part, content_type, charset, is_html)
                 VALUES (?1, ?2, ?3, ?4, ?5)
                 ON CONFLICT(message_id, part) DO UPDATE SET
                     content_type = excluded.content_type,
                     charset = COALESCE(excluded.charset, message_parts.charset),
                     is_html = excluded.is_html",
                params![
                    message_id,
                    part.part,
                    part.content_type,
                    part.charset,
                    i64::from(part.is_html)
                ],
            )?;
        }
        for attachment in attachments {
            tx.execute(
                "INSERT INTO attachments (message_id, part, filename, content_type, encoding, size)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)
                 ON CONFLICT(message_id, part) DO UPDATE SET
                     filename = COALESCE(excluded.filename, attachments.filename),
                     content_type = excluded.content_type,
                     encoding = COALESCE(excluded.encoding, attachments.encoding),
                     size = excluded.size",
                params![
                    message_id,
                    attachment.part,
                    attachment.filename,
                    attachment.content_type,
                    attachment.encoding,
                    attachment.size
                ],
            )?;
        }

        refresh_body_state(&tx, message_id)?;
        record_change(
            &tx,
            account_id,
            "message",
            &message_id.to_string(),
            "updated",
        )?;
        tx.commit()?;
        Ok(())
    }

    /// Caches decoded body alternatives. Re-storing identical bytes keeps the
    /// existing blob so refcounts do not grow.
    pub fn store_body_parts(
        &self,
        account_id: i64,
        message_id: i64,
        parts: &[BodyPartInput<'_>],
        index_text: Option<&str>,
    ) -> Result<()> {
        let mut conn = self.conn.lock().expect("poisoned");
        let tx = conn.transaction()?;
        for part in parts {
            let hash = hash_bytes(part.data);
            let existing: Option<Option<Vec<u8>>> = tx
                .query_row(
                    "SELECT blob_hash FROM message_parts WHERE message_id = ?1 AND part = ?2",
                    params![message_id, part.part],
                    |row| row.get(0),
                )
                .optional()?;
            let already_stored = existing.as_ref().and_then(|hash| hash.as_ref()) == Some(&hash);
            if !already_stored {
                store_blob(&tx, &hash, part.data)?;
            }
            tx.execute(
                "INSERT INTO message_parts (message_id, part, content_type, charset, is_html, blob_hash)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)
                 ON CONFLICT(message_id, part) DO UPDATE SET
                     content_type = excluded.content_type,
                     charset = COALESCE(excluded.charset, message_parts.charset),
                     is_html = excluded.is_html,
                     blob_hash = excluded.blob_hash",
                params![
                    message_id,
                    part.part,
                    part.content_type,
                    part.charset,
                    i64::from(part.is_html),
                    hash
                ],
            )?;
            if !already_stored {
                if let Some(Some(old)) = existing {
                    release_blob(&tx, &old)?;
                }
            }
        }
        refresh_body_state(&tx, message_id)?;
        if let Some(text) = index_text {
            index_body_text(&tx, message_id, text)?;
        }
        record_change(
            &tx,
            account_id,
            "message",
            &message_id.to_string(),
            "updated",
        )?;
        tx.commit()?;
        Ok(())
    }

    /// Writes the indexed body text for a message whose body was cached before
    /// the index existed. Returns whether the index changed.
    pub fn index_body_text_if_missing(&self, message_id: i64, text: &str) -> Result<bool> {
        let mut conn = self.conn.lock().expect("poisoned");
        let tx = conn.transaction()?;
        let written = tx.execute(
            "UPDATE messages SET body_text = ?2 WHERE id = ?1 AND body_text IS NULL",
            params![message_id, truncate_body(text)],
        )?;
        if written > 0 {
            rewrite_fts(&tx, message_id)?;
        }
        tx.commit()?;
        Ok(written > 0)
    }

    /// Indexed body text of one message, `None` until the body was fetched.
    pub fn body_text(&self, message_id: i64) -> Result<Option<String>> {
        let conn = self.conn.lock().expect("poisoned");
        Ok(conn
            .query_row(
                "SELECT body_text FROM messages WHERE id = ?1",
                [message_id],
                |row| row.get(0),
            )
            .optional()?
            .flatten())
    }

    /// Caches one attachment's bytes, replacing any previously stored copy.
    pub fn store_attachment(
        &self,
        account_id: i64,
        message_id: i64,
        part: &str,
        content_type: &str,
        filename: Option<&str>,
        data: &[u8],
    ) -> Result<()> {
        let mut conn = self.conn.lock().expect("poisoned");
        let tx = conn.transaction()?;
        let hash = hash_bytes(data);
        let existing: Option<Option<Vec<u8>>> = tx
            .query_row(
                "SELECT blob_hash FROM attachments WHERE message_id = ?1 AND part = ?2",
                params![message_id, part],
                |row| row.get(0),
            )
            .optional()?;
        let already_stored = existing.as_ref().and_then(|hash| hash.as_ref()) == Some(&hash);
        if !already_stored {
            store_blob(&tx, &hash, data)?;
        }
        tx.execute(
            "INSERT INTO attachments (message_id, part, filename, content_type, size, blob_hash)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT(message_id, part) DO UPDATE SET
                 filename = COALESCE(excluded.filename, attachments.filename),
                 content_type = excluded.content_type,
                 size = excluded.size,
                 blob_hash = excluded.blob_hash",
            params![
                message_id,
                part,
                filename,
                content_type,
                data.len() as i64,
                hash
            ],
        )?;
        if !already_stored {
            if let Some(Some(old)) = existing {
                release_blob(&tx, &old)?;
            }
        }
        refresh_body_state(&tx, message_id)?;
        record_change(
            &tx,
            account_id,
            "message",
            &message_id.to_string(),
            "updated",
        )?;
        tx.commit()?;
        Ok(())
    }

    /// Cached content of one message; `None` when the message is unknown.
    pub fn message_body(&self, message_id: i64) -> Result<Option<MessageBody>> {
        let conn = self.conn.lock().expect("poisoned");
        let state: Option<String> = conn
            .query_row(
                "SELECT body_state FROM messages WHERE id = ?1",
                [message_id],
                |row| row.get(0),
            )
            .optional()?;
        let Some(state) = state else {
            return Ok(None);
        };

        let mut body = MessageBody {
            state: BodyState::parse(&state),
            ..Default::default()
        };
        {
            let mut stmt = conn.prepare(
                "SELECT p.part, p.content_type, p.charset, p.is_html, b.data
                 FROM message_parts p
                 LEFT JOIN blobs b ON b.hash = p.blob_hash
                 WHERE p.message_id = ?1
                 ORDER BY p.part",
            )?;
            let rows = stmt.query_map([message_id], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, Option<String>>(2)?,
                    row.get::<_, i64>(3)? != 0,
                    row.get::<_, Option<Vec<u8>>>(4)?,
                ))
            })?;
            for row in rows {
                let (part, content_type, charset, is_html, data) = row?;
                if let Some(bytes) = data.as_deref() {
                    let text = String::from_utf8_lossy(bytes).to_string();
                    if is_html {
                        body.html.get_or_insert(text);
                    } else {
                        body.text.get_or_insert(text);
                    }
                }
                body.parts.push(StoredPart {
                    part,
                    content_type,
                    charset,
                    is_html,
                    cached: data.is_some(),
                });
            }
        }
        {
            let mut stmt = conn.prepare(
                "SELECT part, filename, content_type, size, blob_hash IS NOT NULL
                 FROM attachments WHERE message_id = ?1 ORDER BY part",
            )?;
            let rows = stmt.query_map([message_id], |row| {
                Ok(StoredAttachment {
                    part: row.get(0)?,
                    filename: row.get(1)?,
                    content_type: row.get(2)?,
                    size: row.get(3)?,
                    downloaded: row.get::<_, i64>(4)? != 0,
                })
            })?;
            for row in rows {
                body.attachments.push(row?);
            }
        }
        Ok(Some(body))
    }

    /// Cached bytes of one attachment, if it was downloaded already.
    pub fn attachment_data(&self, message_id: i64, part: &str) -> Result<Option<Vec<u8>>> {
        let conn = self.conn.lock().expect("poisoned");
        Ok(conn
            .query_row(
                "SELECT b.data FROM attachments a
                 JOIN blobs b ON b.hash = a.blob_hash
                 WHERE a.message_id = ?1 AND a.part = ?2",
                params![message_id, part],
                |row| row.get(0),
            )
            .optional()?)
    }

    /// Metadata needed to download and decode one attachment.
    pub fn attachment_meta(&self, message_id: i64, part: &str) -> Result<Option<AttachmentMeta>> {
        let conn = self.conn.lock().expect("poisoned");
        Ok(conn
            .query_row(
                "SELECT filename, content_type, encoding, size FROM attachments
                 WHERE message_id = ?1 AND part = ?2",
                params![message_id, part],
                |row| {
                    Ok(AttachmentMeta {
                        filename: row.get(0)?,
                        content_type: row.get(1)?,
                        encoding: row.get(2)?,
                        size: row.get(3)?,
                    })
                },
            )
            .optional()?)
    }

    /// Total bytes held by cached bodies and attachments.
    pub fn blob_bytes(&self) -> Result<i64> {
        let conn = self.conn.lock().expect("poisoned");
        Ok(
            conn.query_row("SELECT COALESCE(SUM(size), 0) FROM blobs", [], |row| {
                row.get(0)
            })?,
        )
    }

    pub fn local_uids(&self, mailbox_id: i64) -> Result<std::collections::HashSet<i64>> {
        let conn = self.conn.lock().expect("poisoned");
        let mut stmt = conn.prepare("SELECT uid FROM message_locations WHERE mailbox_id = ?1")?;
        let rows = stmt.query_map([mailbox_id], |row| row.get::<_, i64>(0))?;
        let mut out = std::collections::HashSet::new();
        for row in rows {
            out.insert(row?);
        }
        Ok(out)
    }

    pub fn count_messages_in_mailbox(&self, mailbox_id: i64) -> Result<i64> {
        let conn = self.conn.lock().expect("poisoned");
        Ok(conn.query_row(
            "SELECT COUNT(*) FROM message_locations WHERE mailbox_id = ?1",
            [mailbox_id],
            |row| row.get(0),
        )?)
    }

    pub fn changes_since(
        &self,
        account_id: i64,
        seq: i64,
        limit: i64,
    ) -> Result<Vec<ChangeRecord>> {
        let conn = self.conn.lock().expect("poisoned");
        let mut stmt = conn.prepare(
            "SELECT seq, account_id, entity_type, entity_id, op, at
             FROM changes WHERE account_id = ?1 AND seq > ?2 ORDER BY seq LIMIT ?3",
        )?;
        let rows = stmt.query_map(params![account_id, seq, limit], |row| {
            Ok(ChangeRecord {
                seq: row.get(0)?,
                account_id: row.get(1)?,
                entity_type: row.get(2)?,
                entity_id: row.get(3)?,
                op: row.get(4)?,
                at: row.get(5)?,
            })
        })?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    /// Watermark for the account. Compaction deletes log rows, so the latest
    /// snapshot seq counts as well; otherwise a compacted account would appear
    /// to have gone backwards.
    pub fn latest_change_seq(&self, account_id: i64) -> Result<i64> {
        Ok(self.change_window(account_id)?.latest_seq)
    }

    pub fn count_changes(&self, account_id: i64) -> Result<i64> {
        let conn = self.conn.lock().expect("poisoned");
        Ok(conn.query_row(
            "SELECT COUNT(*) FROM changes WHERE account_id = ?1",
            [account_id],
            |row| row.get(0),
        )?)
    }

    pub fn change_window(&self, account_id: i64) -> Result<ChangeWindow> {
        let conn = self.conn.lock().expect("poisoned");
        let oldest_seq = conn.query_row(
            "SELECT MIN(seq) FROM changes WHERE account_id = ?1",
            [account_id],
            |row| row.get::<_, Option<i64>>(0),
        )?;
        let newest_change = conn.query_row(
            "SELECT COALESCE(MAX(seq), 0) FROM changes WHERE account_id = ?1",
            [account_id],
            |row| row.get::<_, i64>(0),
        )?;
        let snapshot_seq = conn
            .query_row(
                "SELECT seq FROM change_snapshots WHERE account_id = ?1",
                [account_id],
                |row| row.get(0),
            )
            .optional()?;
        Ok(ChangeWindow {
            oldest_seq,
            latest_seq: newest_change.max(snapshot_seq.unwrap_or(0)),
            snapshot_seq,
        })
    }

    /// Current account state, matching what a snapshot would record right now.
    pub fn account_state(&self, account_id: i64) -> Result<AccountState> {
        let conn = self.conn.lock().expect("poisoned");
        read_account_state(&conn, account_id)
    }

    pub fn latest_snapshot(&self, account_id: i64) -> Result<Option<ChangeSnapshot>> {
        let conn = self.conn.lock().expect("poisoned");
        let row = conn
            .query_row(
                "SELECT seq, created_at, state FROM change_snapshots WHERE account_id = ?1",
                [account_id],
                |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, i64>(1)?,
                        row.get::<_, Vec<u8>>(2)?,
                    ))
                },
            )
            .optional()?;
        let Some((seq, created_at, payload)) = row else {
            return Ok(None);
        };
        Ok(Some(ChangeSnapshot {
            seq,
            created_at,
            state: serde_json::from_slice(&payload)?,
        }))
    }

    /// Replaces the change log with a snapshot once it holds more than
    /// `max_rows` rows. The snapshot is taken at the current watermark, so
    /// every deleted row is covered by it; later writes keep appending above it.
    pub fn compact_changes(&self, account_id: i64, max_rows: i64) -> Result<Option<Compaction>> {
        let mut conn = self.conn.lock().expect("poisoned");
        let tx = conn.transaction()?;
        let count: i64 = tx.query_row(
            "SELECT COUNT(*) FROM changes WHERE account_id = ?1",
            [account_id],
            |row| row.get(0),
        )?;
        if count <= max_rows {
            return Ok(None);
        }
        let snapshot_seq: i64 = tx.query_row(
            "SELECT COALESCE(MAX(seq), 0) FROM changes WHERE account_id = ?1",
            [account_id],
            |row| row.get(0),
        )?;
        if snapshot_seq == 0 {
            return Ok(None);
        }
        let payload = serde_json::to_vec(&read_account_state(&tx, account_id)?)?;
        tx.execute(
            "INSERT INTO change_snapshots (account_id, seq, created_at, state)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(account_id) DO UPDATE SET seq = excluded.seq,
                 created_at = excluded.created_at, state = excluded.state",
            params![account_id, snapshot_seq, Utc::now().timestamp(), payload],
        )?;
        let removed = tx.execute(
            "DELETE FROM changes WHERE account_id = ?1 AND seq <= ?2",
            params![account_id, snapshot_seq],
        )?;
        tx.commit()?;
        Ok(Some(Compaction {
            snapshot_seq,
            removed_changes: removed,
        }))
    }
}

/// Refuses to open a legacy plaintext SQLite file: SQLCipher reports such a
/// file as "not a database", which reads like a wrong key, so detect it first.
fn reject_plaintext(path: &Path) -> Result<()> {
    let mut header = [0u8; 16];
    let mut file = match std::fs::File::open(path) {
        Ok(file) => file,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(err) => return Err(err.into()),
    };
    match file.read_exact(&mut header) {
        Ok(()) if header == *PLAINTEXT_HEADER => {
            Err(StoreError::PlaintextDatabase(path.to_path_buf()))
        }
        Ok(()) => Ok(()),
        Err(err) if err.kind() == std::io::ErrorKind::UnexpectedEof => Ok(()),
        Err(err) => Err(err.into()),
    }
}

/// Inserts or updates one message; returns 1 when a new row was created.
fn upsert_message(
    tx: &Transaction<'_>,
    account_id: i64,
    mailbox_id: i64,
    msg: &NewMessage,
) -> Result<usize> {
    let from_json = serde_json::to_string(&msg.from)?;
    let to_json = serde_json::to_string(&msg.to)?;
    let flags_json = serde_json::to_string(&msg.flags)?;

    let existing: Option<i64> = if let Some(mid) = msg.message_id.as_deref() {
        tx.query_row(
            "SELECT id FROM messages WHERE account_id = ?1 AND message_id = ?2",
            params![account_id, mid],
            |row| row.get(0),
        )
        .optional()?
    } else {
        tx.query_row(
            "SELECT message_id FROM message_locations WHERE mailbox_id = ?1 AND uid = ?2",
            params![mailbox_id, msg.uid],
            |row| row.get(0),
        )
        .optional()?
    };

    let message_row =
        match existing {
            Some(id) => {
                tx.execute(
                "UPDATE messages SET subject = COALESCE(?2, subject), from_json = ?3, to_json = ?4,
                        sent_at = COALESCE(?5, sent_at), size = COALESCE(?6, size),
                        snippet = COALESCE(?7, snippet)
                 WHERE id = ?1",
                params![id, msg.subject, from_json, to_json, msg.sent_at, msg.size, msg.snippet],
            )?;
                id
            }
            None => {
                tx.execute(
                    "INSERT INTO messages (account_id, message_id, subject, from_json, to_json,
                        sent_at, size, snippet)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                    params![
                        account_id,
                        msg.message_id,
                        msg.subject,
                        from_json,
                        to_json,
                        msg.sent_at,
                        msg.size,
                        msg.snippet
                    ],
                )?;
                let id = tx.last_insert_rowid();
                rewrite_fts(tx, id)?;
                id
            }
        };

    tx.execute(
        "INSERT INTO message_locations (message_id, mailbox_id, uid, flags)
         VALUES (?1, ?2, ?3, ?4)
         ON CONFLICT(mailbox_id, uid) DO UPDATE SET message_id = excluded.message_id,
             flags = excluded.flags",
        params![message_row, mailbox_id, msg.uid, flags_json],
    )?;
    record_change(
        tx,
        account_id,
        "message",
        &message_row.to_string(),
        if existing.is_none() {
            "created"
        } else {
            "updated"
        },
    )?;
    Ok(usize::from(existing.is_none()))
}

/// One message as the full-text index needs it: subject, address JSON, snippet
/// and indexed body text.
type FtsRow = (
    Option<String>,
    String,
    String,
    Option<String>,
    Option<String>,
);

/// Rebuilds one message's full-text row from what the store holds. FTS5
/// contentless tables cannot update a row, so the old row is dropped first.
fn rewrite_fts(tx: &Transaction<'_>, message_id: i64) -> Result<()> {
    let row: Option<FtsRow> = tx
        .query_row(
            "SELECT subject, from_json, to_json, snippet, body_text
             FROM messages WHERE id = ?1",
            [message_id],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                ))
            },
        )
        .optional()?;
    let Some((subject, from_json, to_json, snippet, body_text)) = row else {
        return Ok(());
    };
    tx.execute("DELETE FROM message_fts WHERE rowid = ?1", [message_id])?;
    tx.execute(
        "INSERT INTO message_fts (rowid, subject, from_text, to_text, snippet, body_text)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![
            message_id,
            subject.unwrap_or_default(),
            addresses_text(&from_json),
            addresses_text(&to_json),
            snippet.unwrap_or_default(),
            body_text.unwrap_or_default()
        ],
    )?;
    Ok(())
}

/// Address lists are stored as JSON arrays; the index wants one searchable line.
fn addresses_text(json: &str) -> String {
    serde_json::from_str::<Vec<String>>(json)
        .map(|addresses| addresses.join(", "))
        .unwrap_or_default()
}

/// Writes the indexed form of a body and refreshes its full-text row.
fn index_body_text(tx: &Transaction<'_>, message_id: i64, text: &str) -> Result<()> {
    tx.execute(
        "UPDATE messages SET body_text = ?2 WHERE id = ?1",
        params![message_id, truncate_body(text)],
    )?;
    rewrite_fts(tx, message_id)
}

fn truncate_body(text: &str) -> String {
    text.chars().take(BODY_INDEX_CHARS).collect()
}

/// Drops and recreates the full-text table with the body column, recovering
/// plain-text bodies from the blob cache. HTML-only bodies need the parser, so
/// they are indexed the next time the message is opened.
fn rebuild_fts(conn: &mut Connection) -> Result<()> {
    let tx = conn.transaction()?;
    tx.execute_batch("DROP TABLE IF EXISTS message_fts;")?;
    tx.execute_batch(FTS_SCHEMA)?;
    tx.execute_batch(
        "UPDATE messages SET body_text = (
             SELECT CAST(group_concat(CAST(b.data AS TEXT), char(10)) AS TEXT)
             FROM message_parts p
             JOIN blobs b ON b.hash = p.blob_hash
             WHERE p.message_id = messages.id AND p.is_html = 0
               AND b.encoding = 'identity')
         WHERE body_text IS NULL",
    )?;
    let rows: Vec<(i64, FtsRow)> = {
        let mut stmt =
            tx.prepare("SELECT id, subject, from_json, to_json, snippet, body_text FROM messages")?;
        let rows = stmt.query_map([], |row| {
            Ok((
                row.get(0)?,
                (
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                    row.get(5)?,
                ),
            ))
        })?;
        rows.collect::<rusqlite::Result<_>>()?
    };
    for (id, (subject, from_json, to_json, snippet, body_text)) in rows {
        tx.execute(
            "INSERT INTO message_fts (rowid, subject, from_text, to_text, snippet, body_text)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                id,
                subject.unwrap_or_default(),
                addresses_text(&from_json),
                addresses_text(&to_json),
                snippet.unwrap_or_default(),
                body_text
                    .map(|text| truncate_body(&text))
                    .unwrap_or_default()
            ],
        )?;
    }
    tx.commit()?;
    Ok(())
}

/// Content hash used as the blob key; content addressing gives dedupe for free.
fn hash_bytes(data: &[u8]) -> Vec<u8> {
    Sha256::digest(data).to_vec()
}

/// Inserts a blob or bumps its refcount when the same bytes already exist.
fn store_blob(tx: &Transaction<'_>, hash: &[u8], data: &[u8]) -> Result<()> {
    tx.execute(
        "INSERT INTO blobs (hash, size, encoding, refcount, data)
         VALUES (?1, ?2, 'identity', 1, ?3)
         ON CONFLICT(hash) DO UPDATE SET refcount = blobs.refcount + 1",
        params![hash, data.len() as i64, data],
    )?;
    Ok(())
}

/// Drops one reference and deletes the blob once nothing points at it.
fn release_blob(tx: &Transaction<'_>, hash: &[u8]) -> Result<()> {
    tx.execute(
        "UPDATE blobs SET refcount = refcount - 1 WHERE hash = ?1",
        [hash],
    )?;
    tx.execute(
        "DELETE FROM blobs WHERE hash = ?1 AND refcount <= 0",
        [hash],
    )?;
    Ok(())
}

/// Deletes messages no mailbox references any more, releasing their blobs and
/// full-text rows. Returns the number of messages dropped.
fn purge_orphan_messages(tx: &Transaction<'_>) -> Result<usize> {
    let orphans =
        "SELECT id FROM messages WHERE id NOT IN (SELECT message_id FROM message_locations)";
    let blob_refs = format!(
        "SELECT blob_hash, COUNT(*) FROM (
             SELECT blob_hash FROM message_parts
              WHERE message_id IN ({orphans}) AND blob_hash IS NOT NULL
             UNION ALL
             SELECT blob_hash FROM attachments
              WHERE message_id IN ({orphans}) AND blob_hash IS NOT NULL
         ) GROUP BY blob_hash"
    );
    let blobs: Vec<(Vec<u8>, i64)> = {
        let mut stmt = tx.prepare(&blob_refs)?;
        let rows = stmt.query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?;
        rows.collect::<rusqlite::Result<_>>()?
    };
    for (hash, count) in blobs {
        tx.execute(
            "UPDATE blobs SET refcount = refcount - ?2 WHERE hash = ?1",
            params![hash, count],
        )?;
        tx.execute(
            "DELETE FROM blobs WHERE hash = ?1 AND refcount <= 0",
            [hash],
        )?;
    }
    // FTS5 contentless-delete tables accept DELETE by rowid.
    tx.execute(
        &format!("DELETE FROM message_fts WHERE rowid IN ({orphans})"),
        [],
    )?;
    let dropped = tx.execute(&format!("DELETE FROM messages WHERE id IN ({orphans})"), [])?;
    Ok(dropped)
}

/// True when `table` already has `column`; migrations run against both fresh
/// databases (columns from SCHEMA) and upgraded ones.
fn has_column(conn: &Connection, table: &str, column: &str) -> Result<bool> {
    let mut stmt = conn.prepare(&format!("PRAGMA table_info({table})"))?;
    let names: Vec<String> = stmt
        .query_map([], |row| row.get(1))?
        .collect::<rusqlite::Result<_>>()?;
    Ok(names.iter().any(|name| name == column))
}

/// Removes layout rows that the server no longer reports, releasing blobs.
fn prune_rows(
    tx: &Transaction<'_>,
    table: &'static str,
    message_id: i64,
    kept: &std::collections::HashSet<&str>,
) -> Result<()> {
    let (select, delete) = match table {
        "message_parts" => (
            "SELECT part, blob_hash FROM message_parts WHERE message_id = ?1",
            "DELETE FROM message_parts WHERE message_id = ?1 AND part = ?2",
        ),
        "attachments" => (
            "SELECT part, blob_hash FROM attachments WHERE message_id = ?1",
            "DELETE FROM attachments WHERE message_id = ?1 AND part = ?2",
        ),
        other => {
            return Err(StoreError::Sqlite(rusqlite::Error::InvalidParameterName(
                other.to_string(),
            )))
        }
    };
    let stale: Vec<(String, Option<Vec<u8>>)> = {
        let mut stmt = tx.prepare(select)?;
        let rows = stmt.query_map([message_id], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, Option<Vec<u8>>>(1)?))
        })?;
        rows.collect::<rusqlite::Result<_>>()?
    };
    for (part, hash) in stale {
        if kept.contains(part.as_str()) {
            continue;
        }
        tx.execute(delete, params![message_id, part])?;
        if let Some(hash) = hash {
            release_blob(tx, &hash)?;
        }
    }
    Ok(())
}

/// Recomputes `messages.body_state` and `has_attach` from the cache tables.
fn refresh_body_state(tx: &Transaction<'_>, message_id: i64) -> Result<()> {
    let counts =
        |sql: &str| -> Result<i64> { Ok(tx.query_row(sql, [message_id], |row| row.get(0))?) };
    let body_total = counts("SELECT COUNT(*) FROM message_parts WHERE message_id = ?1")?;
    let body_missing =
        counts("SELECT COUNT(*) FROM message_parts WHERE message_id = ?1 AND blob_hash IS NULL")?;
    let attachment_total = counts("SELECT COUNT(*) FROM attachments WHERE message_id = ?1")?;
    let attachment_missing =
        counts("SELECT COUNT(*) FROM attachments WHERE message_id = ?1 AND blob_hash IS NULL")?;

    let state = if body_total == 0 && attachment_total == 0 {
        BodyState::Headers
    } else if body_missing == 0 && attachment_missing == 0 {
        BodyState::Full
    } else {
        BodyState::Partial
    };
    tx.execute(
        "UPDATE messages SET body_state = ?2, has_attach = ?3 WHERE id = ?1",
        params![message_id, state.as_str(), i64::from(attachment_total > 0)],
    )?;
    Ok(())
}

/// Reads the snapshot payload for one account within the caller's transaction
/// so the state and the watermark cannot disagree.
fn read_account_state(conn: &Connection, account_id: i64) -> Result<AccountState> {
    let mut mailbox_stmt = conn.prepare(
        "SELECT id, remote_name, role, missing_at FROM mailboxes
         WHERE account_id = ?1 ORDER BY id",
    )?;
    let mailboxes = mailbox_stmt
        .query_map([account_id], |row| {
            Ok(MailboxState {
                id: row.get(0)?,
                remote_name: row.get(1)?,
                role: row.get(2)?,
                missing_at: row.get(3)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;

    let mut location_stmt = conn.prepare(
        "SELECT m.id, m.message_id, l.mailbox_id, l.uid, l.flags
         FROM messages m JOIN message_locations l ON l.message_id = m.id
         WHERE m.account_id = ?1 ORDER BY m.id, l.mailbox_id",
    )?;
    let rows = location_stmt
        .query_map([account_id], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, Option<String>>(1)?,
                MessageLocationState {
                    mailbox_id: row.get(2)?,
                    uid: row.get(3)?,
                    flags: serde_json::from_str(&row.get::<_, String>(4)?).unwrap_or_default(),
                },
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;

    let mut messages: Vec<MessageState> = Vec::new();
    for (id, message_id, location) in rows {
        match messages.last_mut() {
            Some(last) if last.id == id => last.locations.push(location),
            _ => messages.push(MessageState {
                id,
                message_id,
                locations: vec![location],
            }),
        }
    }
    Ok(AccountState {
        mailboxes,
        messages,
    })
}

fn record_change(
    tx: &Transaction<'_>,
    account_id: i64,
    entity_type: &str,
    entity_id: &str,
    op: &str,
) -> Result<()> {
    tx.execute(
        "INSERT INTO changes (account_id, entity_type, entity_id, op, at)
         VALUES (?1, ?2, ?3, ?4, ?5)",
        params![
            account_id,
            entity_type,
            entity_id,
            op,
            Utc::now().timestamp()
        ],
    )?;
    Ok(())
}

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS accounts (
  id           INTEGER PRIMARY KEY,
  provider     TEXT NOT NULL,
  email        TEXT NOT NULL UNIQUE,
  display_name TEXT,
  secret_ref   TEXT NOT NULL,
  settings     TEXT NOT NULL DEFAULT '{}',
  created_at   INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS mailboxes (
  id                 INTEGER PRIMARY KEY,
  account_id         INTEGER NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
  remote_name        TEXT NOT NULL,
  role               TEXT,
  uidvalidity        INTEGER,
  uidnext            INTEGER,
  highest_modseq     INTEGER,
  last_sync_at       INTEGER,
  last_flag_sync_at  INTEGER,
  last_reconcile_at  INTEGER,
  missing_at         INTEGER,
  UNIQUE (account_id, remote_name)
);

CREATE TABLE IF NOT EXISTS settings (
  key   TEXT PRIMARY KEY,
  value TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS messages (
  id          INTEGER PRIMARY KEY,
  account_id  INTEGER NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
  message_id  TEXT,
  thread_id   TEXT,
  subject     TEXT,
  from_json   TEXT NOT NULL DEFAULT '[]',
  to_json     TEXT NOT NULL DEFAULT '[]',
  sent_at     INTEGER,
  size        INTEGER,
  snippet     TEXT,
  has_attach  INTEGER NOT NULL DEFAULT 0,
  body_state  TEXT NOT NULL DEFAULT 'headers',
  body_text   TEXT
);
CREATE UNIQUE INDEX IF NOT EXISTS messages_account_message_id
  ON messages (account_id, message_id) WHERE message_id IS NOT NULL;

CREATE TABLE IF NOT EXISTS message_locations (
  message_id  INTEGER NOT NULL REFERENCES messages(id) ON DELETE CASCADE,
  mailbox_id  INTEGER NOT NULL REFERENCES mailboxes(id) ON DELETE CASCADE,
  uid         INTEGER NOT NULL,
  flags       TEXT NOT NULL DEFAULT '[]',
  PRIMARY KEY (message_id, mailbox_id),
  UNIQUE (mailbox_id, uid)
);

CREATE TABLE IF NOT EXISTS changes (
  seq          INTEGER PRIMARY KEY AUTOINCREMENT,
  account_id   INTEGER NOT NULL,
  entity_type  TEXT NOT NULL,
  entity_id    TEXT NOT NULL,
  op           TEXT NOT NULL,
  at           INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS changes_account_seq ON changes (account_id, seq);

-- Full account state at a watermark; replaces the log rows it covers.
CREATE TABLE IF NOT EXISTS change_snapshots (
  account_id INTEGER PRIMARY KEY,
  seq        INTEGER NOT NULL,
  created_at INTEGER NOT NULL,
  state      BLOB NOT NULL
);

-- Paired devices of the optional Hub. Only a hash of the bearer token is
-- stored, so a database leak does not hand out API access.
CREATE TABLE IF NOT EXISTS hub_devices (
  id           INTEGER PRIMARY KEY,
  name         TEXT NOT NULL,
  token_hash   TEXT NOT NULL UNIQUE,
  created_at   INTEGER NOT NULL,
  last_seen_at INTEGER
);

-- Content-addressed bodies and attachments; refcount keeps one copy per hash.
CREATE TABLE IF NOT EXISTS blobs (
  hash     BLOB PRIMARY KEY,
  size     INTEGER NOT NULL,
  encoding TEXT NOT NULL DEFAULT 'identity',
  refcount INTEGER NOT NULL DEFAULT 1,
  data     BLOB NOT NULL
);

-- Cached body alternatives of one message (already decoded and sanitised).
CREATE TABLE IF NOT EXISTS message_parts (
  message_id   INTEGER NOT NULL REFERENCES messages(id) ON DELETE CASCADE,
  part         TEXT NOT NULL,
  content_type TEXT NOT NULL,
  charset      TEXT,
  is_html      INTEGER NOT NULL DEFAULT 0,
  blob_hash    BLOB,
  PRIMARY KEY (message_id, part)
);

-- Attachment metadata; blob_hash stays NULL until the bytes are downloaded.
CREATE TABLE IF NOT EXISTS attachments (
  message_id   INTEGER NOT NULL REFERENCES messages(id) ON DELETE CASCADE,
  part         TEXT NOT NULL,
  filename     TEXT,
  content_type TEXT NOT NULL,
  encoding     TEXT,
  size         INTEGER NOT NULL DEFAULT 0,
  blob_hash    BLOB,
  PRIMARY KEY (message_id, part)
);
"#;

/// FTS5 cannot add a column, so the body index is a separate statement that
/// both schema creation and the migration rebuild use verbatim.
const FTS_SCHEMA: &str = r#"
CREATE VIRTUAL TABLE IF NOT EXISTS message_fts USING fts5(
  subject, from_text, to_text, snippet, body_text,
  content='',
  contentless_delete=1,
  tokenize='trigram'
);
"#;

fn map_message(row: &rusqlite::Row<'_>) -> rusqlite::Result<StoredMessage> {
    Ok(StoredMessage {
        id: row.get(0)?,
        uid: row.get(1)?,
        mailbox_id: row.get(2)?,
        message_id: row.get(3)?,
        subject: row.get(4)?,
        from: serde_json::from_str(&row.get::<_, String>(5)?).unwrap_or_default(),
        to: serde_json::from_str(&row.get::<_, String>(6)?).unwrap_or_default(),
        sent_at: row.get(7)?,
        size: row.get(8)?,
        flags: serde_json::from_str(&row.get::<_, String>(9)?).unwrap_or_default(),
        snippet: row.get(10)?,
    })
}

#[cfg(test)]
mod migration_tests {
    use super::*;
    use std::path::PathBuf;

    fn temp_path(name: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        std::env::temp_dir().join(format!(
            "siskin-store-{name}-{}-{nanos}.db",
            std::process::id()
        ))
    }

    /// Rolls a fresh database back to the pre-body-index shape: four-column
    /// FTS table, no `body_text`, and one message whose body sits in the blob
    /// cache but not in the index.
    #[test]
    fn adding_the_body_index_upgrades_an_existing_database() {
        let path = temp_path("migrate");
        let key = DbKey::from_bytes(&[3u8; 32]).expect("key");
        let (account, message_id) = {
            let store = Store::open(&path, &key).expect("store");
            let account = store
                .create_account(
                    "imap",
                    "m@example.com",
                    None,
                    "account:m@example.com",
                    &serde_json::json!({}),
                )
                .expect("account");
            let mailbox = store
                .ensure_mailbox(account, "INBOX", Some("inbox"))
                .expect("mailbox");
            store
                .apply_fetch(
                    account,
                    mailbox.id,
                    &[NewMessage {
                        uid: 1,
                        message_id: Some("<m1@example.com>".to_string()),
                        subject: Some("legacy subject".to_string()),
                        from: vec!["a@example.com".to_string()],
                        to: vec!["m@example.com".to_string()],
                        sent_at: Some(1),
                        size: Some(20),
                        flags: vec![],
                        snippet: None,
                    }],
                    &[],
                )
                .expect("fetch");
            let message_id = store.list_messages(account, 1, 0).expect("list")[0].id;
            store
                .record_body_structure(
                    account,
                    message_id,
                    &[PartMetaInput {
                        part: "1",
                        content_type: "text/plain",
                        charset: Some("utf-8"),
                        is_html: false,
                        size: 17,
                    }],
                    &[],
                )
                .expect("structure");
            store
                .store_body_parts(
                    account,
                    message_id,
                    &[BodyPartInput {
                        part: "1",
                        content_type: "text/plain",
                        charset: Some("utf-8"),
                        is_html: false,
                        data: b"legacy body token",
                    }],
                    None,
                )
                .expect("body");

            {
                let conn = store.conn.lock().expect("poisoned");
                conn.execute_batch(&format!(
                    "DROP TABLE message_fts;
                     CREATE VIRTUAL TABLE message_fts USING fts5(
                         subject, from_text, to_text, snippet,
                         content='', contentless_delete=1, tokenize='trigram');
                     INSERT INTO message_fts (rowid, subject, from_text, to_text, snippet)
                         VALUES ({message_id}, 'legacy subject', 'a@example.com',
                                 'm@example.com', '');
                     ALTER TABLE messages DROP COLUMN body_text;
                     PRAGMA user_version = 5;"
                ))
                .expect("degrade to v5");
            }
            (account, message_id)
        };

        let store = Store::open(&path, &key).expect("reopen");
        let version: i64 = {
            let conn = store.conn.lock().expect("poisoned");
            conn.pragma_query_value(None, "user_version", |row| row.get(0))
                .expect("version")
        };
        assert_eq!(version, SCHEMA_VERSION);
        assert_eq!(
            store
                .search_messages(account, "legacy subject", 10)
                .expect("search")
                .len(),
            1,
            "the subject index must survive the rebuild"
        );
        assert_eq!(
            store.body_text(message_id).expect("read").as_deref(),
            Some("legacy body token"),
            "plain-text bodies are recovered from the blob cache"
        );
        assert_eq!(
            store
                .search_messages(account, "legacy body", 10)
                .expect("search")
                .len(),
            1
        );

        drop(store);
        let _ = std::fs::remove_file(path);
    }
}
