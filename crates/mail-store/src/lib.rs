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
}

pub type Result<T, E = StoreError> = std::result::Result<T, E>;

const SCHEMA_VERSION: i64 = 3;

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
        let conn = self.conn.lock().expect("poisoned");
        conn.execute_batch(SCHEMA)?;
        let current: i64 = conn.pragma_query_value(None, "user_version", |row| row.get(0))?;
        if current == 1 {
            conn.execute_batch("ALTER TABLE mailboxes ADD COLUMN last_reconcile_at INTEGER;")?;
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
                 role = COALESCE(excluded.role, mailboxes.role)",
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
        }

        let raw: Option<Row> = conn
            .query_row(
                "SELECT id, account_id, remote_name, role, uidvalidity, uidnext,
                        highest_modseq, last_sync_at, last_flag_sync_at, last_reconcile_at
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
        // Messages left without any location are dropped below; release the
        // blobs they referenced first so the cache does not leak.
        {
            let mut stmt = tx.prepare(
                "SELECT blob_hash, COUNT(*) FROM (
                     SELECT blob_hash FROM message_parts
                      WHERE message_id IN (
                          SELECT id FROM messages WHERE id NOT IN (SELECT message_id FROM message_locations)
                      ) AND blob_hash IS NOT NULL
                     UNION ALL
                     SELECT blob_hash FROM attachments
                      WHERE message_id IN (
                          SELECT id FROM messages WHERE id NOT IN (SELECT message_id FROM message_locations)
                      ) AND blob_hash IS NOT NULL
                 ) GROUP BY blob_hash",
            )?;
            let rows = stmt.query_map([], |row| {
                Ok((row.get::<_, Vec<u8>>(0)?, row.get::<_, i64>(1)?))
            })?;
            let orphans: Vec<(Vec<u8>, i64)> = rows.collect::<rusqlite::Result<_>>()?;
            drop(stmt);
            for (hash, count) in orphans {
                tx.execute(
                    "UPDATE blobs SET refcount = refcount - ?2 WHERE hash = ?1",
                    params![hash, count],
                )?;
                tx.execute(
                    "DELETE FROM blobs WHERE hash = ?1 AND refcount <= 0",
                    [hash],
                )?;
            }
        }
        tx.execute(
            "DELETE FROM messages WHERE id NOT IN (SELECT message_id FROM message_locations)",
            [],
        )?;
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

        for uid in removed_uids {
            stats.removed += tx.execute(
                "DELETE FROM message_locations WHERE mailbox_id = ?1 AND uid = ?2",
                params![mailbox_id, *uid],
            )?;
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
        let mut out = Vec::new();
        if trimmed.chars().count() < 3 {
            let pattern = format!("%{trimmed}%");
            let mut stmt = conn.prepare(&format!(
                "SELECT {columns} FROM messages m
                 JOIN message_locations l ON l.message_id = m.id
                 WHERE m.account_id = ?1
                   AND (m.subject LIKE ?2 OR m.from_json LIKE ?2 OR m.snippet LIKE ?2)
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
            "SELECT COUNT(*) FROM message_locations l
             JOIN messages m ON m.id = l.message_id WHERE m.account_id = ?1",
            [account_id],
            |row| row.get(0),
        )?)
    }

    /// Routing data needed to fetch one message's content from its server.
    pub fn message_context(&self, message_id: i64) -> Result<Option<MessageContext>> {
        let conn = self.conn.lock().expect("poisoned");
        conn.query_row(
            "SELECT m.account_id, l.mailbox_id, b.remote_name, l.uid, m.body_state
             FROM messages m
             JOIN message_locations l ON l.message_id = m.id
             JOIN mailboxes b ON b.id = l.mailbox_id
             WHERE m.id = ?1
             ORDER BY l.mailbox_id LIMIT 1",
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

    pub fn latest_change_seq(&self, account_id: i64) -> Result<i64> {
        let conn = self.conn.lock().expect("poisoned");
        Ok(conn
            .query_row(
                "SELECT COALESCE(MAX(seq), 0) FROM changes WHERE account_id = ?1",
                [account_id],
                |row| row.get(0),
            )
            .unwrap_or(0))
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
                index_message(tx, id, msg)?;
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
        "upsert",
    )?;
    Ok(usize::from(existing.is_none()))
}

fn index_message(tx: &Transaction<'_>, id: i64, msg: &NewMessage) -> Result<()> {
    tx.execute(
        "INSERT INTO message_fts (rowid, subject, from_text, to_text, snippet)
         VALUES (?1, ?2, ?3, ?4, ?5)",
        params![
            id,
            msg.subject.clone().unwrap_or_default(),
            msg.from.join(", "),
            msg.to.join(", "),
            msg.snippet.clone().unwrap_or_default()
        ],
    )?;
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
  UNIQUE (account_id, remote_name)
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
  body_state  TEXT NOT NULL DEFAULT 'headers'
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

CREATE VIRTUAL TABLE IF NOT EXISTS message_fts USING fts5(
  subject, from_text, to_text, snippet,
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
