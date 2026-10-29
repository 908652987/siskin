//! POP3 mirroring: one mailbox, UIDL-based dedupe, polling only.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Instant;

use chrono::{DateTime, Utc};
use mail_proto::pop3::Pop3Session;
use mail_proto::Tier;
use mail_store::{NewMessage, Store};
use sha2::{Digest, Sha256};

use crate::{Result, SyncOutcome};

pub struct Pop3SyncEngine {
    store: Arc<Store>,
}

impl Pop3SyncEngine {
    pub fn new(store: Arc<Store>) -> Self {
        Self { store }
    }

    /// Mirrors the server's single mailbox. Messages no longer listed by the
    /// server are removed locally; POP3 has no flags or folder structure.
    pub async fn sync(&self, account_id: i64, session: &mut Pop3Session) -> Result<SyncOutcome> {
        let started = Instant::now();
        let mailbox = self
            .store
            .ensure_mailbox(account_id, "INBOX", Some("inbox"))?;
        let local: HashSet<i64> = self.store.local_uids(mailbox.id)?;
        let listed = session.list().await?;

        let mut remote = HashSet::with_capacity(listed.len());
        let mut messages = Vec::new();
        for item in &listed {
            let uid = pop3_uid(&item.uid);
            remote.insert(uid);
            if local.contains(&uid) {
                continue;
            }
            let headers = session.headers(item.number).await?;
            let parsed = parse_headers(&headers);
            messages.push(NewMessage {
                uid,
                message_id: parsed.message_id,
                subject: parsed.subject,
                from: parsed.from,
                to: parsed.to,
                sent_at: parsed.sent_at,
                size: Some(item.size as i64),
                flags: Vec::new(),
                snippet: None,
            });
        }

        let removed: Vec<i64> = local.difference(&remote).copied().collect();
        let stats = self
            .store
            .apply_fetch(account_id, mailbox.id, &messages, &removed)?;

        Ok(SyncOutcome {
            mailbox: "INBOX".to_string(),
            tier: Tier::C,
            full: local.is_empty(),
            inserted: stats.inserted,
            applied: stats.applied,
            removed: stats.removed,
            duration_ms: started.elapsed().as_millis(),
        })
    }
}

/// UIDL strings are opaque; the store keys locations by a numeric uid, so use a
/// 63-bit digest. A collision would need ~2^31 messages in practice.
pub fn pop3_uid(uidl: &str) -> i64 {
    let digest = Sha256::digest(uidl.as_bytes());
    let mut bytes = [0u8; 8];
    bytes.copy_from_slice(&digest[..8]);
    let value = i64::from_be_bytes(bytes) & i64::MAX;
    if value == 0 {
        1
    } else {
        value
    }
}

#[derive(Debug, Default, PartialEq, Eq)]
struct Headers {
    subject: Option<String>,
    from: Vec<String>,
    to: Vec<String>,
    message_id: Option<String>,
    sent_at: Option<i64>,
}

fn parse_headers(raw: &str) -> Headers {
    let mut headers = Headers::default();
    let mut current: Option<(String, String)> = None;
    let mut fields: Vec<(String, String)> = Vec::new();
    for line in raw.split("\r\n") {
        if line.is_empty() {
            break;
        }
        if line.starts_with(' ') || line.starts_with('\t') {
            if let Some((_, value)) = current.as_mut() {
                value.push(' ');
                value.push_str(line.trim());
            }
            continue;
        }
        if let Some(field) = current.take() {
            fields.push(field);
        }
        if let Some((name, value)) = line.split_once(':') {
            current = Some((name.trim().to_ascii_lowercase(), value.trim().to_string()));
        }
    }
    if let Some(field) = current {
        fields.push(field);
    }

    for (name, value) in fields {
        match name.as_str() {
            "subject" => headers.subject = Some(value),
            "from" => push_address(&mut headers.from, &value),
            "to" => push_address(&mut headers.to, &value),
            "message-id" => {
                headers.message_id = Some(value.trim_matches(['<', '>']).to_string());
            }
            "date" => headers.sent_at = parse_date(&value),
            _ => {}
        }
    }
    headers
}

fn push_address(target: &mut Vec<String>, value: &str) {
    let value = value.trim();
    if !value.is_empty() {
        target.push(value.to_string());
    }
}

fn parse_date(value: &str) -> Option<i64> {
    DateTime::parse_from_rfc2822(value)
        .map(|dt| dt.with_timezone(&Utc).timestamp())
        .ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pop3_uid_is_stable_and_distinct() {
        assert_eq!(pop3_uid("abc"), pop3_uid("abc"));
        assert_ne!(pop3_uid("abc"), pop3_uid("abd"));
        assert!(pop3_uid("abc") > 0);
    }

    #[test]
    fn parsing_folded_headers_extracts_envelope_fields() {
        let raw = "Subject: hello\r\n world\r\nFrom: Alice <a@example.com>\r\n\
                   To: bob@example.com\r\nMessage-ID: <id-1@example.com>\r\n\
                   Date: Tue, 01 Oct 2024 10:00:00 +0800\r\n\r\nbody";
        let headers = parse_headers(raw);
        assert_eq!(headers.subject.as_deref(), Some("hello world"));
        assert_eq!(headers.from, vec!["Alice <a@example.com>"]);
        assert_eq!(headers.to, vec!["bob@example.com"]);
        assert_eq!(headers.message_id.as_deref(), Some("id-1@example.com"));
        assert_eq!(headers.sent_at, Some(1_727_748_000));
    }

    #[test]
    fn missing_fields_stay_empty() {
        let headers = parse_headers("Subject: only\r\n\r\n");
        assert_eq!(headers.subject.as_deref(), Some("only"));
        assert!(headers.from.is_empty());
        assert!(headers.message_id.is_none());
    }
}
