use mail_store::{
    BodyPartInput, DbKey, NewMessage, PartMetaInput, Store, StoreError,
    DEFAULT_TRASH_RETENTION_DAYS,
};

fn test_key() -> DbKey {
    DbKey::from_bytes(&[9u8; 32]).unwrap()
}

fn msg(uid: i64, message_id: &str) -> NewMessage {
    NewMessage {
        uid,
        message_id: Some(message_id.to_string()),
        subject: Some(format!("subject {message_id}")),
        from: vec!["Sender <sender@test>".to_string()],
        to: vec!["me@test".to_string()],
        sent_at: Some(1_700_000_000 + uid),
        size: Some(128),
        flags: vec![],
        snippet: Some(format!("body {message_id}")),
    }
}

fn setup() -> (Store, i64, i64, i64) {
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
    let inbox = store
        .ensure_mailbox(account, "INBOX", Some("inbox"))
        .unwrap();
    let archive = store
        .ensure_mailbox(account, "Archive", Some("archive"))
        .unwrap();
    (store, account, inbox.id, archive.id)
}

fn mark_archive_missing(store: &Store, account: i64, now: i64) {
    let marked = store
        .mark_missing_mailboxes(account, &["INBOX".to_string()], now)
        .unwrap();
    assert_eq!(marked, 1);
}

#[test]
fn vanished_mailbox_moves_its_messages_to_trash() {
    let (store, account, _inbox, archive) = setup();
    store
        .apply_fetch(account, archive, &[msg(1, "one"), msg(2, "two")], &[])
        .unwrap();

    mark_archive_missing(&store, account, 1_000);

    assert!(
        store.list_messages(account, 10, 0).unwrap().is_empty(),
        "trashed messages must not appear in the normal list"
    );
    assert_eq!(store.list_trash(account, 10, 0).unwrap().len(), 2);
    assert_eq!(store.count_messages(account).unwrap(), 0);
    assert!(store
        .search_messages(account, "one", 10)
        .unwrap()
        .is_empty());

    let mailboxes = store.list_mailboxes(account).unwrap();
    let archived = mailboxes
        .iter()
        .find(|mailbox| mailbox.remote_name == "Archive")
        .expect("archive");
    assert_eq!(archived.missing_at, Some(1_000));
}

#[test]
fn inbox_is_never_treated_as_deleted() {
    let (store, account, inbox, _archive) = setup();
    store
        .apply_fetch(account, inbox, &[msg(1, "one")], &[])
        .unwrap();

    let marked = store.mark_missing_mailboxes(account, &[], 1_000).unwrap();
    assert_eq!(marked, 1, "only Archive is marked");
    let mailboxes = store.list_mailboxes(account).unwrap();
    let inbox_row = mailboxes
        .iter()
        .find(|mailbox| mailbox.remote_name == "INBOX")
        .expect("inbox");
    assert_eq!(
        inbox_row.missing_at, None,
        "INBOX stays present even if LIST omits it"
    );
    assert_eq!(store.list_messages(account, 10, 0).unwrap().len(), 1);
}

#[test]
fn a_returning_mailbox_restores_its_messages() {
    let (store, account, _inbox, archive) = setup();
    store
        .apply_fetch(account, archive, &[msg(1, "one")], &[])
        .unwrap();
    mark_archive_missing(&store, account, 1_000);

    let restored = store
        .ensure_mailbox(account, "Archive", Some("archive"))
        .unwrap();
    assert_eq!(restored.missing_at, None);
    assert_eq!(store.list_messages(account, 10, 0).unwrap().len(), 1);
    assert!(store.list_trash(account, 10, 0).unwrap().is_empty());
}

#[test]
fn one_message_in_two_mailboxes_is_listed_once() {
    let (store, account, inbox, archive) = setup();
    store
        .apply_fetch(account, inbox, &[msg(1, "shared")], &[])
        .unwrap();
    store
        .apply_fetch(account, archive, &[msg(41, "shared")], &[])
        .unwrap();

    assert_eq!(store.list_messages(account, 10, 0).unwrap().len(), 1);
    assert_eq!(store.count_messages(account).unwrap(), 1);
    assert_eq!(
        store.search_messages(account, "shared", 10).unwrap().len(),
        1
    );
}

#[test]
fn a_recreated_mailbox_retires_the_trashed_one() {
    let (store, account, _inbox, archive) = setup();
    store
        .apply_fetch(account, archive, &[msg(1, "one")], &[])
        .unwrap();
    mark_archive_missing(&store, account, 1_000);

    let retired = store.retire_mailbox(archive).unwrap();
    assert!(retired.starts_with("Archive (local trash "));

    let fresh = store
        .ensure_mailbox(account, "Archive", Some("archive"))
        .unwrap();
    assert_ne!(fresh.id, archive);
    assert_eq!(
        store.list_trash(account, 10, 0).unwrap().len(),
        1,
        "retired messages keep their retention clock"
    );

    store
        .apply_fetch(account, fresh.id, &[msg(9, "nine")], &[])
        .unwrap();
    assert_eq!(store.list_messages(account, 10, 0).unwrap().len(), 1);
    assert_eq!(store.count_messages(account).unwrap(), 1);
}

#[test]
fn purging_waits_for_retention_then_drops_messages_and_blobs() {
    let (store, account, _inbox, archive) = setup();
    store
        .apply_fetch(account, archive, &[msg(1, "one")], &[])
        .unwrap();
    let message_id = store.list_messages(account, 10, 0).unwrap()[0].id;
    store
        .record_body_structure(
            account,
            message_id,
            &[PartMetaInput {
                part: "1",
                content_type: "text/plain",
                charset: Some("utf-8"),
                is_html: false,
                size: 5,
            }],
            &[],
        )
        .unwrap();
    store
        .store_body_parts(
            account,
            message_id,
            &[BodyPartInput {
                part: "1",
                content_type: "text/plain",
                charset: Some("utf-8"),
                is_html: false,
                data: b"hello",
            }],
        )
        .unwrap();
    assert!(store.blob_bytes().unwrap() > 0);

    mark_archive_missing(&store, account, 1_000);
    let kept = store.purge_trash(90, 1_000 + 89 * 86_400).unwrap();
    assert_eq!(kept.mailboxes, 0, "retention has not elapsed yet");
    assert_eq!(store.list_trash(account, 10, 0).unwrap().len(), 1);

    let purged = store.purge_trash(90, 1_000 + 90 * 86_400).unwrap();
    assert_eq!(purged.mailboxes, 1);
    assert_eq!(purged.messages, 1);
    assert!(store.list_trash(account, 10, 0).unwrap().is_empty());
    assert_eq!(store.blob_bytes().unwrap(), 0, "bodies must not leak");
    assert!(store.list_mailboxes(account).unwrap().len() == 1);
}

#[test]
fn retention_defaults_to_three_months_and_is_validated() {
    let (store, _account, _inbox, _archive) = setup();
    assert_eq!(
        store.trash_retention_days().unwrap(),
        DEFAULT_TRASH_RETENTION_DAYS
    );

    store.set_trash_retention_days(30).unwrap();
    assert_eq!(store.trash_retention_days().unwrap(), 30);

    assert!(matches!(
        store.set_trash_retention_days(0),
        Err(StoreError::InvalidSetting { .. })
    ));
    store.set_trash_retention_days(7).unwrap();
    assert_eq!(store.trash_retention_days().unwrap(), 7);
}
