use mail_store::{DbKey, NewMessage, Store};

fn test_key() -> DbKey {
    DbKey::from_bytes(&[7u8; 32]).unwrap()
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
    let inbox = store
        .ensure_mailbox(account, "INBOX", Some("inbox"))
        .unwrap()
        .id;
    (store, account, inbox)
}

fn msg(uid: i64, message_id: Option<&str>, in_reply_to: Option<&str>) -> NewMessage {
    NewMessage {
        uid,
        message_id: message_id.map(str::to_string),
        in_reply_to: in_reply_to.map(str::to_string),
        subject: Some(format!("subject {uid}")),
        from: vec!["Sender <sender@test>".to_string()],
        to: vec!["me@test".to_string()],
        sent_at: Some(1_700_000_000 + uid),
        size: Some(1024),
        flags: vec![],
        snippet: Some(format!("body {uid}")),
    }
}

fn thread_of(store: &Store, account: i64, message_id: &str) -> String {
    let id = store
        .message_by_message_id(account, message_id)
        .unwrap()
        .expect("message");
    let messages = store.messages_with_locations(account, &[id]).unwrap();
    messages[0].thread_id.clone().expect("thread id")
}

#[test]
fn replies_join_the_thread_of_their_parent() {
    let (store, account, inbox) = setup();
    store
        .apply_fetch(
            account,
            inbox,
            &[
                msg(1, Some("<a@test>"), None),
                msg(2, Some("<b@test>"), Some("<a@test>")),
                msg(3, Some("<c@test>"), Some("<b@test>")),
            ],
            &[],
        )
        .unwrap();

    let thread = thread_of(&store, account, "<a@test>");
    assert_eq!(thread, "<a@test>");
    assert_eq!(thread_of(&store, account, "<b@test>"), thread);
    assert_eq!(thread_of(&store, account, "<c@test>"), thread);

    let threads = store
        .threads(account, std::slice::from_ref(&thread))
        .unwrap();
    assert_eq!(threads.len(), 1);
    assert_eq!(threads[0].id, thread);
    assert_eq!(threads[0].email_ids.len(), 3);
    // Oldest first: sent_at grows with the uid in this fixture.
    let by_uid: Vec<i64> = [1, 2, 3]
        .iter()
        .map(|uid| store.message_at(inbox, *uid).unwrap().expect("location"))
        .collect();
    assert_eq!(threads[0].email_ids, by_uid);
}

#[test]
fn a_reply_arriving_before_its_parent_is_rethreaded() {
    let (store, account, inbox) = setup();
    store
        .apply_fetch(
            account,
            inbox,
            &[
                msg(1, Some("<reply@test>"), Some("<root@test>")),
                msg(2, Some("<nested@test>"), Some("<reply@test>")),
            ],
            &[],
        )
        .unwrap();
    assert_eq!(thread_of(&store, account, "<reply@test>"), "<reply@test>");

    store
        .apply_fetch(account, inbox, &[msg(3, Some("<root@test>"), None)], &[])
        .unwrap();

    assert_eq!(thread_of(&store, account, "<root@test>"), "<root@test>");
    assert_eq!(thread_of(&store, account, "<reply@test>"), "<root@test>");
    assert_eq!(thread_of(&store, account, "<nested@test>"), "<root@test>");
    let threads = store
        .threads(account, &["<root@test>".to_string()])
        .unwrap();
    assert_eq!(threads[0].email_ids.len(), 3);
}

#[test]
fn messages_without_an_id_get_a_local_thread() {
    let (store, account, inbox) = setup();
    store
        .apply_fetch(account, inbox, &[msg(1, None, None)], &[])
        .unwrap();
    let id = store.message_at(inbox, 1).unwrap().expect("message");
    let thread = format!("t{id}");
    let threads = store
        .threads(account, std::slice::from_ref(&thread))
        .unwrap();
    assert_eq!(
        threads,
        vec![mail_store::ThreadInfo {
            id: thread.clone(),
            email_ids: vec![id],
        }]
    );
    let listed = store.list_threads(account, 10).unwrap();
    assert_eq!(listed[0].id, thread);
}

#[test]
fn threads_only_cover_live_mailboxes() {
    let (store, account, inbox) = setup();
    store
        .apply_fetch(account, inbox, &[msg(1, Some("<a@test>"), None)], &[])
        .unwrap();
    let archive = store.ensure_mailbox(account, "Archive", None).unwrap().id;
    store
        .apply_fetch(
            account,
            archive,
            &[msg(2, Some("<archived@test>"), None)],
            &[],
        )
        .unwrap();
    assert_eq!(store.list_threads(account, 10).unwrap().len(), 2);

    store
        .mark_missing_mailboxes(account, &["INBOX".to_string()], 1_700_000_000)
        .unwrap();
    assert!(store.list_threads(account, 10).unwrap().len() == 1);
    assert!(store
        .threads(account, &["<archived@test>".to_string()])
        .unwrap()
        .is_empty());
    assert_eq!(
        store
            .threads(account, &["<a@test>".to_string()])
            .unwrap()
            .len(),
        1
    );
}

#[test]
fn thread_changes_track_creation_updates_and_late_parents() {
    let (store, account, inbox) = setup();
    assert_eq!(store.latest_change_seq_type(account, "thread").unwrap(), 0);

    store
        .apply_fetch(
            account,
            inbox,
            &[msg(1, Some("<reply@test>"), Some("<root@test>"))],
            &[],
        )
        .unwrap();
    let first = store.latest_change_seq_type(account, "thread").unwrap();
    let changes = store.changes_since_type(account, "thread", 0, 100).unwrap();
    assert_eq!(changes.len(), 1);
    assert_eq!(changes[0].entity_id, "<reply@test>");
    assert_eq!(changes[0].op, "created");

    store
        .apply_fetch(
            account,
            inbox,
            &[msg(2, Some("<nested@test>"), Some("<reply@test>"))],
            &[],
        )
        .unwrap();
    let changes = store
        .changes_since_type(account, "thread", first, 100)
        .unwrap();
    assert_eq!(changes.len(), 1);
    assert_eq!(changes[0].entity_id, "<reply@test>");
    assert_eq!(changes[0].op, "updated");

    // The parent arrives late: a new thread is created and the old
    // reply-rooted thread disappears once its messages are folded in.
    let before_parent = store.latest_change_seq_type(account, "thread").unwrap();
    store
        .apply_fetch(account, inbox, &[msg(3, Some("<root@test>"), None)], &[])
        .unwrap();
    let changes = store
        .changes_since_type(account, "thread", before_parent, 100)
        .unwrap();
    assert!(changes
        .iter()
        .any(|change| change.entity_id == "<root@test>" && change.op == "created"));
    assert!(changes
        .iter()
        .any(|change| change.entity_id == "<reply@test>" && change.op == "removed"));
}

#[test]
fn removing_the_last_message_destroys_the_thread() {
    let (store, account, inbox) = setup();
    store
        .apply_fetch(account, inbox, &[msg(1, Some("<one@test>"), None)], &[])
        .unwrap();
    let id = store.message_at(inbox, 1).unwrap().expect("message");
    let before = store.latest_change_seq_type(account, "thread").unwrap();

    assert_eq!(store.remove_all_locations(account, id).unwrap(), 1);
    assert!(store
        .messages_with_locations(account, &[id])
        .unwrap()
        .is_empty());
    let changes = store
        .changes_since_type(account, "thread", before, 100)
        .unwrap();
    assert_eq!(changes.len(), 1);
    assert_eq!(changes[0].entity_id, "<one@test>");
    assert_eq!(changes[0].op, "removed");

    // A message with two copies keeps its thread until the last copy goes.
    store
        .apply_fetch(account, inbox, &[msg(2, Some("<two@test>"), None)], &[])
        .unwrap();
    let sent = store
        .ensure_mailbox(account, "Sent", Some("sent"))
        .unwrap()
        .id;
    store
        .apply_fetch(account, sent, &[msg(9, Some("<two@test>"), None)], &[])
        .unwrap();
    let id = store
        .message_by_message_id(account, "<two@test>")
        .unwrap()
        .unwrap();
    let before = store.latest_change_seq_type(account, "thread").unwrap();
    store.remove_location(account, id, inbox).unwrap();
    let changes = store
        .changes_since_type(account, "thread", before, 100)
        .unwrap();
    assert!(
        changes.is_empty(),
        "a surviving copy must not change the thread"
    );
}

#[test]
fn change_watermarks_are_per_entity_type() {
    let (store, account, inbox) = setup();
    assert!(store.latest_change_seq_type(account, "mailbox").unwrap() > 0);
    assert_eq!(store.latest_change_seq_type(account, "message").unwrap(), 0);
    assert_eq!(store.latest_change_seq_type(account, "thread").unwrap(), 0);

    store
        .apply_fetch(account, inbox, &[msg(1, Some("<one@test>"), None)], &[])
        .unwrap();
    let message = store.latest_change_seq_type(account, "message").unwrap();
    let thread = store.latest_change_seq_type(account, "thread").unwrap();
    assert!(message > 0 && thread > 0);
    assert!(store.latest_change_seq(account).unwrap() >= thread);
}

#[test]
fn uploads_are_scoped_to_their_account() {
    let (store, account, _) = setup();
    let other = store
        .create_account(
            "imap",
            "other@example.com",
            None,
            "account:other",
            &serde_json::json!({}),
        )
        .unwrap();
    let bytes = b"Subject: hi\r\nMessage-ID: <up@test>\r\n\r\nbody";
    let blob_id = store
        .store_upload(account, "message/rfc822", bytes)
        .unwrap();

    let upload = store
        .upload_for_account(account, &blob_id)
        .unwrap()
        .expect("upload");
    assert_eq!(upload.content_type, "message/rfc822");
    assert_eq!(upload.bytes, bytes);
    assert_eq!(upload.size, bytes.len() as i64);

    assert!(store.upload_for_account(other, &blob_id).unwrap().is_none());
    assert!(store.blob_for_account(other, &blob_id).unwrap().is_none());
    let blob = store
        .blob_for_account(account, &blob_id)
        .unwrap()
        .expect("downloadable");
    assert_eq!(blob.bytes, bytes);
    assert!(!blob.attachment);
    assert_eq!(blob.content_type.as_deref(), Some("message/rfc822"));

    // Re-uploading the same bytes must not multiply the stored copy.
    let before = store.blob_bytes().unwrap();
    let again = store
        .store_upload(account, "message/rfc822", bytes)
        .unwrap();
    assert_eq!(again, blob_id);
    assert_eq!(store.blob_bytes().unwrap(), before);
}

#[test]
fn expired_uploads_release_their_bytes() {
    let (store, account, _) = setup();
    let bytes = b"Subject: gone\r\n\r\nbody";
    let blob_id = store
        .store_upload(account, "message/rfc822", bytes)
        .unwrap();
    assert_eq!(store.blob_bytes().unwrap(), bytes.len() as i64);

    let now = chrono::Utc::now().timestamp();
    assert_eq!(store.purge_expired_uploads(now).unwrap(), 0);
    assert_eq!(
        store
            .purge_expired_uploads(now + mail_store::UPLOAD_TTL_SECONDS + 1)
            .unwrap(),
        1
    );

    assert!(store
        .upload_for_account(account, &blob_id)
        .unwrap()
        .is_none());
    assert!(store.blob_for_account(account, &blob_id).unwrap().is_none());
    assert_eq!(store.blob_bytes().unwrap(), 0);
}
