use mail_store::{DbKey, NewMessage, Store, StoreError, SyncState};

/// Deterministic test key; the real key is derived from the keychain master key.
fn test_key() -> DbKey {
    DbKey::from_bytes(&[7u8; 32]).unwrap()
}

fn temp_db_path(name: &str) -> std::path::PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    std::env::temp_dir().join(format!("siskin-{name}-{}-{nanos}.db", std::process::id()))
}

fn remove_db_files(path: &std::path::Path) {
    for suffix in ["", "-wal", "-shm"] {
        let mut target = path.as_os_str().to_owned();
        target.push(suffix);
        let _ = std::fs::remove_file(std::path::PathBuf::from(target));
    }
}

fn msg(uid: i64, subject: &str) -> NewMessage {
    NewMessage {
        uid,
        message_id: Some(format!("<{uid}@test>")),
        subject: Some(subject.to_string()),
        from: vec!["Sender <sender@test>".to_string()],
        to: vec!["me@test".to_string()],
        sent_at: Some(1_700_000_000 + uid),
        size: Some(1024),
        flags: vec![],
        snippet: Some(format!("body for {subject}")),
    }
}

fn setup() -> (Store, i64, i64) {
    let store = Store::open_in_memory(&test_key()).unwrap();
    let account = store
        .create_account(
            "imap",
            "test@example.com",
            None,
            "account:test",
            &serde_json::json!({}),
        )
        .unwrap();
    let mailbox = store
        .ensure_mailbox(account, "INBOX", Some("inbox"))
        .unwrap();
    (store, account, mailbox.id)
}

#[test]
fn account_and_mailbox_are_idempotent() {
    let (store, account_id, mailbox_id) = setup();
    let again = store
        .create_account(
            "imap",
            "test@example.com",
            None,
            "account:test",
            &serde_json::json!({}),
        )
        .unwrap();
    assert_eq!(account_id, again);

    let mailbox = store.ensure_mailbox(account_id, "INBOX", None).unwrap();
    assert_eq!(mailbox.id, mailbox_id);
    assert_eq!(store.list_accounts().unwrap().len(), 1);
}

#[test]
fn applying_a_fetch_inserts_updates_and_removes() {
    let (store, account_id, mailbox_id) = setup();
    let messages = vec![msg(1, "first"), msg(2, "second"), msg(3, "third")];
    let stats = store
        .apply_fetch(account_id, mailbox_id, &messages, &[])
        .unwrap();
    assert_eq!(stats.inserted, 3);
    assert_eq!(store.count_messages(account_id).unwrap(), 3);

    let mut seen = messages[0].clone();
    seen.flags = vec!["\\Seen".to_string()];
    let stats = store
        .apply_fetch(account_id, mailbox_id, &[seen], &[])
        .unwrap();
    assert_eq!(stats.inserted, 0);
    assert_eq!(store.count_messages(account_id).unwrap(), 3);

    let stored = store.list_messages(account_id, 10, 0).unwrap();
    let first = stored.iter().find(|m| m.uid == 1).unwrap();
    assert_eq!(first.flags, vec!["\\Seen".to_string()]);

    let stats = store
        .apply_fetch(account_id, mailbox_id, &[], &[2])
        .unwrap();
    assert_eq!(stats.removed, 1);
    assert_eq!(store.count_messages(account_id).unwrap(), 2);
}

#[test]
fn uidvalidity_reset_clears_the_mailbox() {
    let (store, account_id, mailbox_id) = setup();
    store
        .apply_fetch(account_id, mailbox_id, &[msg(1, "one"), msg(2, "two")], &[])
        .unwrap();
    let removed = store.reset_mailbox(mailbox_id, 999).unwrap();
    assert_eq!(removed, 2);
    assert_eq!(store.count_messages(account_id).unwrap(), 0);
    assert_eq!(store.count_messages_in_mailbox(mailbox_id).unwrap(), 0);
}

#[test]
fn change_log_is_monotonic_and_filtered_by_account() {
    let (store, account_id, mailbox_id) = setup();
    store
        .apply_fetch(account_id, mailbox_id, &[msg(1, "one")], &[])
        .unwrap();
    let other = store
        .create_account(
            "imap",
            "other@example.com",
            None,
            "account:other",
            &serde_json::json!({}),
        )
        .unwrap();
    let other_mailbox = store.ensure_mailbox(other, "INBOX", None).unwrap();
    store
        .apply_fetch(other, other_mailbox.id, &[msg(9, "nine")], &[])
        .unwrap();

    let first = store.changes_since(account_id, 0, 100).unwrap();
    let second = store.latest_change_seq(account_id).unwrap();
    assert!(!first.is_empty());
    assert!(first.iter().all(|c| c.account_id == account_id));
    assert!(first.windows(2).all(|w| w[0].seq < w[1].seq));
    assert!(store
        .changes_since(account_id, second, 100)
        .unwrap()
        .is_empty());
}

#[test]
fn sync_state_roundtrip() {
    let (store, _account_id, mailbox_id) = setup();
    store
        .update_sync_state(
            mailbox_id,
            &SyncState {
                uidvalidity: Some(42),
                uidnext: Some(7),
                highest_modseq: Some(99),
                last_sync_at: Some(1_700_000_000),
                last_flag_sync_at: Some(1_700_000_001),
                last_reconcile_at: Some(1_700_000_002),
            },
        )
        .unwrap();
    let mailbox = store.mailbox_by_name(1, "INBOX").unwrap().unwrap();
    assert_eq!(mailbox.uidvalidity, Some(42));
    assert_eq!(mailbox.uidnext, Some(7));
    assert_eq!(mailbox.highest_modseq, Some(99));
    assert_eq!(mailbox.last_reconcile_at, Some(1_700_000_002));
}

#[test]
fn search_handles_long_and_short_queries() {
    let (store, account_id, mailbox_id) = setup();
    let messages = vec![
        msg(1, "Quarterly report"),
        msg(2, "季度报告"),
        msg(3, "Unrelated note"),
    ];
    store
        .apply_fetch(account_id, mailbox_id, &messages, &[])
        .unwrap();

    let long = store.search_messages(account_id, "Quarterly", 10).unwrap();
    assert_eq!(long.len(), 1);
    assert_eq!(long[0].uid, 1);

    // Two-character CJK query goes through the LIKE fallback.
    let short = store.search_messages(account_id, "季度", 10).unwrap();
    assert_eq!(short.len(), 1);
    assert_eq!(short[0].uid, 2);

    assert!(store
        .search_messages(account_id, "missing", 10)
        .unwrap()
        .is_empty());
}

#[test]
fn a_different_key_cannot_open_the_database() {
    let path = temp_db_path("wrong-key");
    {
        let store = Store::open(&path, &test_key()).unwrap();
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

    let other = DbKey::from_bytes(&[9u8; 32]).unwrap();
    match Store::open(&path, &other) {
        Ok(_) => panic!("database opened with the wrong key"),
        Err(StoreError::WrongKey) => {}
        Err(err) => panic!("unexpected error: {err}"),
    }
    remove_db_files(&path);
}

#[test]
fn a_plaintext_database_is_rejected() {
    let path = temp_db_path("plaintext");
    std::fs::write(&path, b"SQLite format 3\0plaintext payload").unwrap();

    match Store::open(&path, &test_key()) {
        Ok(_) => panic!("plaintext database was opened"),
        Err(StoreError::PlaintextDatabase(reported)) => assert_eq!(reported, path),
        Err(err) => panic!("unexpected error: {err}"),
    }
    remove_db_files(&path);
}

#[test]
fn the_database_file_hides_message_text() {
    let path = temp_db_path("at-rest");
    {
        let store = Store::open(&path, &test_key()).unwrap();
        let account = store
            .create_account(
                "imap",
                "test@example.com",
                None,
                "account:test",
                &serde_json::json!({}),
            )
            .unwrap();
        let mailbox = store
            .ensure_mailbox(account, "INBOX", Some("inbox"))
            .unwrap();
        store
            .apply_fetch(
                account,
                mailbox.id,
                &[msg(1, "unmistakable-subject-marker")],
                &[],
            )
            .unwrap();
    }

    let mut bytes = Vec::new();
    for suffix in ["", "-wal"] {
        let mut target = path.as_os_str().to_owned();
        target.push(suffix);
        if let Ok(mut content) = std::fs::read(std::path::PathBuf::from(target)) {
            bytes.append(&mut content);
        }
    }
    assert!(
        !bytes
            .windows(25)
            .any(|window| window == b"unmistakable-subject-marker"),
        "message subject found in plaintext on disk"
    );
    remove_db_files(&path);
}
