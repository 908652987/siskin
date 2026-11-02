//! Incremental synchronisation between an IMAP mailbox and the local store.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Instant;

use chrono::Utc;
use mail_proto::{FetchedMessage, ImapSession, MailboxInfo, SpecialUse, Tier};
use mail_store::{NewMessage, Store, SyncState};
use thiserror::Error;

mod pop3;
pub use pop3::{pop3_uid, Pop3SyncEngine};

/// How long tier B/C servers may go before flags are re-read in full.
const FLAG_RESYNC_SECONDS: i64 = 15 * 60;

#[derive(Debug, Error)]
pub enum SyncError {
    #[error("proto: {0}")]
    Proto(#[from] mail_proto::ProtoError),
    #[error("store: {0}")]
    Store(#[from] mail_store::StoreError),
    #[error("mailbox disappeared: {0}")]
    MissingMailbox(String),
    #[error("server did not report UIDVALIDITY for {0}")]
    NoUidValidity(String),
}

pub type Result<T, E = SyncError> = std::result::Result<T, E>;

#[derive(Debug, Clone)]
pub struct SyncOutcome {
    pub mailbox: String,
    pub tier: Tier,
    pub full: bool,
    pub inserted: usize,
    pub applied: usize,
    pub removed: usize,
    pub duration_ms: u128,
}

/// One mailbox that failed while the rest of the account kept syncing.
#[derive(Debug, Clone)]
pub struct MailboxFailure {
    pub mailbox: String,
    pub error: String,
}

/// Result of syncing every selectable mailbox of an account.
#[derive(Debug, Clone)]
pub struct AccountSyncReport {
    pub outcomes: Vec<SyncOutcome>,
    pub failures: Vec<MailboxFailure>,
}

/// One mailbox the account sync should visit; `role` comes from SPECIAL-USE
/// first and from the mailbox name only as a fallback.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MailboxTarget {
    pub name: String,
    pub role: Option<&'static str>,
}

pub struct SyncEngine {
    store: Arc<Store>,
}

impl SyncEngine {
    pub fn new(store: Arc<Store>) -> Self {
        Self { store }
    }

    pub async fn sync_mailbox(
        &self,
        account_id: i64,
        session: &mut ImapSession,
        mailbox_name: &str,
    ) -> Result<SyncOutcome> {
        self.sync_mailbox_with_role(account_id, session, mailbox_name, role_for(mailbox_name))
            .await
    }

    /// Syncs every selectable, non-virtual mailbox of the account and moves
    /// mailboxes the server stopped listing into the local trash. A single
    /// broken mailbox does not stop the others; its error is reported.
    pub async fn sync_account(
        &self,
        account_id: i64,
        session: &mut ImapSession,
    ) -> Result<AccountSyncReport> {
        self.sync_account_except(account_id, session, &[]).await
    }

    /// Like [`SyncEngine::sync_account`], but leaves `skip` mailboxes to another
    /// session that already watches them. Skipped mailboxes still count as
    /// present, so the trash detection stays correct.
    pub async fn sync_account_except(
        &self,
        account_id: i64,
        session: &mut ImapSession,
        skip: &[String],
    ) -> Result<AccountSyncReport> {
        let listed = session.list_mailboxes().await?;
        let targets = plan_mailboxes(&listed);
        let present: Vec<String> = listed.iter().map(|info| info.name.clone()).collect();

        let mut outcomes = Vec::new();
        let mut failures = Vec::new();
        for target in &targets {
            if skip.iter().any(|name| name == &target.name) {
                continue;
            }
            match self
                .sync_mailbox_with_role(account_id, session, &target.name, target.role)
                .await
            {
                Ok(outcome) => outcomes.push(outcome),
                Err(err) => {
                    tracing::warn!(mailbox = target.name.as_str(), %err, "mailbox sync failed");
                    failures.push(MailboxFailure {
                        mailbox: target.name.clone(),
                        error: err.to_string(),
                    });
                }
            }
        }

        let marked =
            self.store
                .mark_missing_mailboxes(account_id, &present, Utc::now().timestamp())?;
        if marked > 0 {
            tracing::info!(
                marked,
                "mailboxes missing on the server moved to local trash"
            );
        }
        Ok(AccountSyncReport { outcomes, failures })
    }

    pub async fn sync_mailbox_with_role(
        &self,
        account_id: i64,
        session: &mut ImapSession,
        mailbox_name: &str,
        role: Option<&str>,
    ) -> Result<SyncOutcome> {
        let started = Instant::now();
        let tier = session.tier();
        let existing = self.store.mailbox_by_name(account_id, mailbox_name)?;

        let selected = session.select(mailbox_name).await?;
        let uid_validity = selected
            .uid_validity
            .ok_or_else(|| SyncError::NoUidValidity(mailbox_name.to_string()))?
            as i64;

        // A folder deleted and recreated under the same name is a different
        // mailbox: retire the old row so its messages keep their retention
        // instead of being replaced by the new incarnation.
        if let Some(old) = existing
            .filter(|local| local.missing_at.is_some() && local.uidvalidity != Some(uid_validity))
        {
            let retired = self.store.retire_mailbox(old.id)?;
            tracing::info!(mailbox = %retired, "vanished mailbox kept in local trash");
        }

        let mailbox = self.store.ensure_mailbox(account_id, mailbox_name, role)?;
        let local = self
            .store
            .mailbox_by_name(account_id, mailbox_name)?
            .ok_or_else(|| SyncError::MissingMailbox(mailbox_name.to_string()))?;

        let mut full = local.uidnext.is_none();
        if local.uidvalidity != Some(uid_validity) {
            tracing::info!(
                mailbox = mailbox_name,
                old = ?local.uidvalidity,
                new = uid_validity,
                "UIDVALIDITY changed, rebuilding mailbox"
            );
            self.store.reset_mailbox(mailbox.id, uid_validity)?;
            full = true;
        }

        let mut fetched: Vec<FetchedMessage> = Vec::new();
        let mut removed: Vec<i64> = Vec::new();
        let mut flags_synced = false;
        let now = Utc::now().timestamp();

        if full {
            fetched = session.fetch_range("1:*").await?;
            flags_synced = true;
        } else {
            let from = local.uidnext.unwrap_or(1);
            if let Some(uid_next) = selected.uid_next {
                if (uid_next as i64) > from {
                    fetched.extend(session.fetch_range(&format!("{from}:*")).await?);
                }
            }

            let flags_due = local
                .last_flag_sync_at
                .map(|ts| now - ts >= FLAG_RESYNC_SECONDS)
                .unwrap_or(true);

            match tier {
                Tier::A => {
                    if let Some(modseq) = local.highest_modseq {
                        fetched.extend(session.fetch_flags_changed_since(modseq as u64).await?);
                        flags_synced = true;
                    } else if flags_due {
                        fetched.extend(session.fetch_flags("1:*").await?);
                        flags_synced = true;
                    }
                }
                Tier::B | Tier::C => {
                    if flags_due {
                        fetched.extend(session.fetch_flags("1:*").await?);
                        flags_synced = true;
                    }
                }
            }

            // Scanning every UID is O(n) on the server; only do it when the
            // message count disagrees or the periodic reconcile is due.
            let known_in_mailbox = self.store.count_messages_in_mailbox(mailbox.id)?;
            let count_changed = known_in_mailbox != i64::from(selected.exists);
            let reconcile_due = local
                .last_reconcile_at
                .map(|ts| now - ts >= FLAG_RESYNC_SECONDS)
                .unwrap_or(true);
            if count_changed || reconcile_due {
                let remote: HashSet<i64> = session
                    .list_uids()
                    .await?
                    .into_iter()
                    .map(i64::from)
                    .collect();
                let known = self.store.local_uids(mailbox.id)?;
                removed = known.difference(&remote).copied().collect();
            }
        }

        let reconciled = full
            || local
                .last_reconcile_at
                .map(|ts| now - ts >= FLAG_RESYNC_SECONDS)
                .unwrap_or(true)
            || self.store.count_messages_in_mailbox(mailbox.id)? != i64::from(selected.exists);

        let messages = merge_messages(fetched);
        let stats = self
            .store
            .apply_fetch(account_id, mailbox.id, &messages, &removed)?;

        self.store.update_sync_state(
            mailbox.id,
            &SyncState {
                uidvalidity: Some(uid_validity),
                uidnext: selected.uid_next.map(i64::from),
                highest_modseq: selected.highest_modseq.map(|v| v as i64),
                last_sync_at: Some(now),
                last_flag_sync_at: flags_synced.then_some(now),
                last_reconcile_at: reconciled.then_some(now),
            },
        )?;

        Ok(SyncOutcome {
            mailbox: mailbox_name.to_string(),
            tier,
            full,
            inserted: stats.inserted,
            applied: stats.applied,
            removed: stats.removed,
            duration_ms: started.elapsed().as_millis(),
        })
    }
}

/// Later fetches override earlier ones for the same UID (flags are the latest state).
fn merge_messages(fetched: Vec<FetchedMessage>) -> Vec<NewMessage> {
    let mut by_uid: HashMap<u32, NewMessage> = HashMap::new();
    for msg in fetched {
        let entry = by_uid
            .entry(msg.uid)
            .or_insert_with(|| to_new_message(&msg));
        if entry.subject.is_none() && msg.subject.is_some() {
            *entry = to_new_message(&msg);
        } else {
            entry.flags = msg.flags;
            entry.size = entry.size.or(msg.size.map(i64::from));
            entry.sent_at = entry.sent_at.or(msg.sent_at);
        }
    }
    by_uid.into_values().collect()
}

fn to_new_message(msg: &FetchedMessage) -> NewMessage {
    NewMessage {
        uid: i64::from(msg.uid),
        message_id: msg.message_id.clone(),
        subject: msg.subject.clone(),
        from: msg.from.clone(),
        to: msg.to.clone(),
        sent_at: msg.sent_at.or(msg.internal_date),
        size: msg.size.map(i64::from),
        flags: msg.flags.clone(),
        snippet: None,
    }
}

/// Selectable mailboxes to sync, with SPECIAL-USE roles where the server
/// declares them. `\All` and `\Flagged` are virtual: they aggregate messages
/// that the real mailboxes already store, so syncing them duplicates rows.
pub fn plan_mailboxes(listed: &[MailboxInfo]) -> Vec<MailboxTarget> {
    let mut targets = Vec::new();
    let mut seen = HashSet::new();
    for info in listed {
        if !info.selectable || info.is_virtual() || !seen.insert(info.name.as_str()) {
            continue;
        }
        let role = info
            .special_use
            .map(role_for_special_use)
            .or_else(|| role_for(&info.name));
        targets.push(MailboxTarget {
            name: info.name.clone(),
            role,
        });
    }
    // INBOX must always be synced, even if a broken LIST response omits it.
    if !targets
        .iter()
        .any(|target| target.name.eq_ignore_ascii_case("INBOX"))
    {
        targets.insert(
            0,
            MailboxTarget {
                name: "INBOX".to_string(),
                role: Some("inbox"),
            },
        );
    }
    targets.sort_by_key(|target| target.role != Some("inbox"));
    targets
}

fn role_for_special_use(use_kind: SpecialUse) -> &'static str {
    match use_kind {
        SpecialUse::Archive => "archive",
        SpecialUse::Drafts => "drafts",
        SpecialUse::Junk => "spam",
        SpecialUse::Sent => "sent",
        SpecialUse::Trash => "trash",
        // Virtual mailboxes are filtered before planning; keep them distinct
        // so a future caller never mistakes them for a real folder.
        SpecialUse::All => "all",
        SpecialUse::Flagged => "flagged",
    }
}

fn role_for(name: &str) -> Option<&'static str> {
    // Providers nest roles (`INBOX.Sent`, `[Gmail]/Sent Mail`); match the leaf.
    let leaf = name.rsplit(['/', '.']).next().unwrap_or(name);
    match leaf.trim().to_ascii_lowercase().as_str() {
        "inbox" | "收件箱" => Some("inbox"),
        "sent" | "sent items" | "sent mail" | "outbox" | "已发送" | "已发送邮件" | "发件箱" => {
            Some("sent")
        }
        "drafts" | "draft" | "草稿" | "草稿箱" => Some("drafts"),
        "trash" | "deleted items" | "deleted messages" | "已删除" | "已删除邮件" | "废纸篓"
        | "垃圾箱" => Some("trash"),
        "junk" | "spam" | "bulk mail" | "垃圾邮件" | "垃圾邮件箱" | "广告邮件" => {
            Some("spam")
        }
        "archive" | "归档" | "存档" => Some("archive"),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fetched(uid: u32, subject: Option<&str>, flags: &[&str]) -> FetchedMessage {
        FetchedMessage {
            uid,
            flags: flags.iter().map(|f| f.to_string()).collect(),
            subject: subject.map(str::to_string),
            message_id: Some(format!("<{uid}@test>")),
            from: vec!["a@test".into()],
            to: vec!["b@test".into()],
            sent_at: Some(1_700_000_000 + uid as i64),
            size: Some(10),
            internal_date: None,
        }
    }

    #[test]
    fn later_fetch_wins_for_flags_but_keeps_headers() {
        let merged = merge_messages(vec![
            fetched(7, Some("subject"), &[]),
            fetched(7, None, &["\\Seen"]),
        ]);
        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0].subject.as_deref(), Some("subject"));
        assert_eq!(merged[0].flags, vec!["\\Seen".to_string()]);
    }

    #[test]
    fn higher_uid_headers_replace_lower_ones_for_the_same_uid() {
        let merged = merge_messages(vec![
            fetched(1, None, &["\\Seen"]),
            fetched(1, Some("full headers"), &[]),
        ]);
        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0].subject.as_deref(), Some("full headers"));
    }

    #[test]
    fn special_use_folders_are_recognised() {
        assert_eq!(role_for("INBOX"), Some("inbox"));
        assert_eq!(role_for("Sent Items"), Some("sent"));
        assert_eq!(role_for("Deleted Items"), Some("trash"));
        assert_eq!(role_for("Bulk Mail"), Some("spam"));
        assert_eq!(role_for("INBOX.Sent"), Some("sent"));
        assert_eq!(role_for("[Gmail]/Sent Mail"), Some("sent"));
        assert_eq!(role_for("已发送"), Some("sent"));
        assert_eq!(role_for("Projects"), None);
    }

    fn info(
        name: &str,
        special_use: Option<SpecialUse>,
        selectable: bool,
    ) -> mail_proto::MailboxInfo {
        mail_proto::MailboxInfo {
            name: name.to_string(),
            attributes: Vec::new(),
            special_use,
            selectable,
        }
    }

    #[test]
    fn planning_skips_virtual_and_placeholder_mailboxes() {
        let planned = plan_mailboxes(&[
            info("INBOX", None, true),
            info("[Gmail]", None, false),
            info("[Gmail]/All Mail", Some(SpecialUse::All), true),
            info("[Gmail]/Important", Some(SpecialUse::Flagged), true),
            info("[Gmail]/Sent Mail", Some(SpecialUse::Sent), true),
            info("Archive", Some(SpecialUse::Archive), true),
        ]);
        let names: Vec<&str> = planned.iter().map(|target| target.name.as_str()).collect();
        assert_eq!(
            names,
            vec!["INBOX", "[Gmail]/Sent Mail", "Archive"],
            "INBOX first, virtual and non-selectable mailboxes skipped"
        );
        assert_eq!(planned[1].role, Some("sent"));
        assert_eq!(planned[2].role, Some("archive"));
    }

    #[test]
    fn planning_falls_back_to_names_and_always_includes_inbox() {
        let planned = plan_mailboxes(&[info("已发送", None, true)]);
        let names: Vec<&str> = planned.iter().map(|target| target.name.as_str()).collect();
        assert_eq!(names, vec!["INBOX", "已发送"]);
        assert_eq!(planned[1].role, Some("sent"));
    }
}
