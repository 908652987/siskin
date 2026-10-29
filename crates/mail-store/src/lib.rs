//! Local store: accounts, mailboxes, messages, change log, full-text index.

use std::path::Path;
use std::sync::Mutex;

use chrono::Utc;
use rusqlite::{params, Connection, OptionalExtension, Transaction};
use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum StoreError {
    #[error("sqlite: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
    #[error("account not found: {0}")]
    AccountNotFound(String),
    #[error("mailbox not found: {0}")]
    MailboxNotFound(String),
}

pub type Result<T, E = StoreError> = std::result::Result<T, E>;

const SCHEMA_VERSION: i64 = 2;

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

pub struct Store {
    conn: Mutex<Connection>,
}

impl Store {
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        Self::init(Connection::open(path)?)
    }

    pub fn open_in_memory() -> Result<Self> {
        Self::init(Connection::open_in_memory()?)
    }

    fn init(conn: Connection) -> Result<Self> {
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
