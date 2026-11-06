use mail_store::{DbKey, NewMessage, Store};

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

fn msg(uid: i64, flags: &[&str]) -> NewMessage {
    NewMessage {
        uid,
        message_id: Some(format!("<{uid}@test>")),
        in_reply_to: None,
        subject: Some(format!("subject {uid}")),
        from: vec!["Sender <sender@test>".to_string()],
        to: vec!["me@test".to_string()],
        sent_at: Some(1_700_000_000 + uid),
        size: Some(1024),
        flags: flags.iter().map(|flag| flag.to_string()).collect(),
        snippet: Some(format!("body for {uid}")),
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
fn compaction_bounds_the_log_and_preserves_the_watermark() {
    let (store, account_id, mailbox_id) = setup();
    let batch: Vec<NewMessage> = (1..=30).map(|uid| msg(uid, &[])).collect();
    store
        .apply_fetch(account_id, mailbox_id, &batch, &[])
        .unwrap();

    let before = store.change_window(account_id).unwrap();
    assert_eq!(
        before.latest_seq,
        store.latest_change_seq(account_id).unwrap()
    );
    assert_eq!(before.snapshot_seq, None);
    assert_eq!(store.count_changes(account_id).unwrap(), 32);

    let compaction = store.compact_changes(account_id, 10).unwrap().unwrap();
    assert_eq!(compaction.snapshot_seq, before.latest_seq);
    assert_eq!(compaction.removed_changes, 32);
    assert_eq!(store.count_changes(account_id).unwrap(), 0);

    let after = store.change_window(account_id).unwrap();
    assert_eq!(after.latest_seq, before.latest_seq);
    assert_eq!(after.snapshot_seq, Some(before.latest_seq));
    assert_eq!(after.oldest_seq, None);
    assert!(store
        .changes_since(account_id, after.latest_seq, 100)
        .unwrap()
        .is_empty());
}

#[test]
fn compaction_is_skipped_below_the_threshold() {
    let (store, account_id, mailbox_id) = setup();
    store
        .apply_fetch(account_id, mailbox_id, &[msg(1, &[])], &[])
        .unwrap();

    assert_eq!(store.compact_changes(account_id, 10).unwrap(), None);
    let window = store.change_window(account_id).unwrap();
    assert_eq!(window.snapshot_seq, None);
    assert_eq!(window.oldest_seq, Some(1));
    assert_eq!(window.latest_seq, 3);
}

#[test]
fn snapshot_rebuilds_the_state_and_survives_reopen() {
    let path = temp_db_path("snapshot");
    let expected;
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
        let inbox = store
            .ensure_mailbox(account, "INBOX", Some("inbox"))
            .unwrap();
        let archive = store
            .ensure_mailbox(account, "Archive", Some("archive"))
            .unwrap();
        store
            .apply_fetch(account, inbox.id, &[msg(1, &["\\Seen"]), msg(2, &[])], &[])
            .unwrap();
        store
            .apply_fetch(account, archive.id, &[msg(3, &["\\Flagged"])], &[])
            .unwrap();

        let compaction = store.compact_changes(account, 1).unwrap().unwrap();
        let snapshot = store.latest_snapshot(account).unwrap().unwrap();
        assert_eq!(snapshot.seq, compaction.snapshot_seq);
        assert_eq!(store.account_state(account).unwrap(), snapshot.state);
        expected = snapshot;
    }

    let store = Store::open(&path, &test_key()).unwrap();
    let account = store
        .account_by_email("test@example.com")
        .unwrap()
        .unwrap()
        .id;
    let snapshot = store.latest_snapshot(account).unwrap().unwrap();
    assert_eq!(snapshot, expected);
    assert_eq!(snapshot.state.mailboxes.len(), 2);
    assert_eq!(snapshot.state.messages.len(), 3);
    let seen = snapshot
        .state
        .messages
        .iter()
        .find(|message| message.id == 1)
        .unwrap();
    assert_eq!(seen.locations.len(), 1);
    assert_eq!(seen.locations[0].flags, vec!["\\Seen".to_string()]);
    drop(store);
    remove_db_files(&path);
}

#[test]
fn changes_after_compaction_continue_above_the_snapshot() {
    let (store, account_id, mailbox_id) = setup();
    store
        .apply_fetch(account_id, mailbox_id, &[msg(1, &[])], &[])
        .unwrap();
    let snapshot_seq = store
        .compact_changes(account_id, 1)
        .unwrap()
        .unwrap()
        .snapshot_seq;

    store
        .apply_fetch(account_id, mailbox_id, &[msg(2, &[])], &[])
        .unwrap();
    let window = store.change_window(account_id).unwrap();
    assert!(window.latest_seq > snapshot_seq);
    assert_eq!(window.oldest_seq, Some(snapshot_seq + 1));

    let changes = store.changes_since(account_id, snapshot_seq, 100).unwrap();
    assert_eq!(changes.len(), 2);
    assert!(changes.iter().all(|change| change.seq > snapshot_seq));
    assert!(changes
        .iter()
        .any(|change| change.entity_type == "message" && change.entity_id == "2"));
}

#[test]
fn compaction_only_touches_the_requested_account() {
    let (store, account_id, mailbox_id) = setup();
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
        .apply_fetch(account_id, mailbox_id, &[msg(1, &[])], &[])
        .unwrap();
    store
        .apply_fetch(other, other_mailbox.id, &[msg(2, &[])], &[])
        .unwrap();
    let other_before = store.change_window(other).unwrap();

    store.compact_changes(account_id, 1).unwrap().unwrap();

    assert_eq!(store.change_window(other).unwrap(), other_before);
    assert_eq!(store.latest_snapshot(other).unwrap(), None);
}
