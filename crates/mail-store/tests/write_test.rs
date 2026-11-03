use mail_store::{BodyPartInput, DbKey, NewMessage, Store};

fn test_key() -> DbKey {
    DbKey::from_bytes(&[7u8; 32]).unwrap()
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
    let inbox = store
        .ensure_mailbox(account, "INBOX", Some("inbox"))
        .unwrap()
        .id;
    (store, account, inbox)
}

#[test]
fn flags_apply_to_every_live_location() {
    let (store, account, inbox) = setup();
    let sent = store
        .ensure_mailbox(account, "Sent", Some("sent"))
        .unwrap()
        .id;
    store
        .apply_fetch(account, inbox, &[msg(1, "one")], &[])
        .unwrap();
    store
        .apply_fetch(account, sent, &[msg(5, "copy")], &[])
        .unwrap();
    // Give the second copy the same Message-ID so both locations share a message.
    let mut copy = msg(5, "copy");
    copy.message_id = Some("<1@test>".to_string());
    store.apply_fetch(account, sent, &[copy], &[]).unwrap();
    let message_id = store.list_messages(account, 10, 0).unwrap()[0].id;

    let before = store.latest_change_seq(account).unwrap();
    let seen = vec!["\\Seen".to_string()];
    assert_eq!(
        store.update_flags(account, message_id, &seen, &[]).unwrap(),
        2
    );
    let message = &store
        .messages_with_locations(account, &[message_id])
        .unwrap()[0];
    assert_eq!(message.locations.len(), 2);
    assert!(message
        .locations
        .iter()
        .all(|location| location.flags.iter().any(|flag| flag == "\\Seen")));
    assert_eq!(store.latest_change_seq(account).unwrap(), before + 1);

    // IMAP flag matching is case-insensitive.
    let clear = vec!["\\seen".to_string()];
    assert_eq!(
        store
            .update_flags(account, message_id, &[], &clear)
            .unwrap(),
        2
    );
    let message = &store
        .messages_with_locations(account, &[message_id])
        .unwrap()[0];
    assert!(message
        .locations
        .iter()
        .all(|location| location.flags.is_empty()));
}

#[test]
fn removing_a_location_keeps_the_message_until_the_last_copy_is_gone() {
    let (store, account, inbox) = setup();
    let sent = store
        .ensure_mailbox(account, "Sent", Some("sent"))
        .unwrap()
        .id;
    store
        .apply_fetch(account, inbox, &[msg(1, "one")], &[])
        .unwrap();
    let mut copy = msg(2, "one");
    copy.message_id = Some("<1@test>".to_string());
    store.apply_fetch(account, sent, &[copy], &[]).unwrap();
    let message_id = store.list_messages(account, 10, 0).unwrap()[0].id;
    assert_eq!(
        store
            .message_location_targets(account, message_id)
            .unwrap()
            .len(),
        2
    );

    assert!(store.remove_location(account, message_id, inbox).unwrap());
    assert_eq!(
        store
            .messages_with_locations(account, &[message_id])
            .unwrap()
            .len(),
        1
    );
    assert!(store.remove_location(account, message_id, sent).unwrap());
    assert!(store
        .messages_with_locations(account, &[message_id])
        .unwrap()
        .is_empty());
    let changes = store
        .changes_since(account, 0, 100)
        .unwrap()
        .into_iter()
        .filter(|change| change.entity_type == "message")
        .map(|change| change.op)
        .collect::<Vec<_>>();
    assert!(changes.contains(&"removed".to_string()));
}

#[test]
fn blobs_are_returned_only_to_their_account() {
    let (store, account, inbox) = setup();
    store
        .apply_fetch(account, inbox, &[msg(1, "one")], &[])
        .unwrap();
    let message_id = store.list_messages(account, 10, 0).unwrap()[0].id;
    store
        .store_body_parts(
            account,
            message_id,
            &[BodyPartInput {
                part: "1",
                content_type: "text/plain",
                charset: Some("utf-8"),
                is_html: false,
                data: b"hello blob",
            }],
            Some("hello blob"),
        )
        .unwrap();

    let body = store.message_body(message_id).unwrap().expect("body");
    let blob_id = body.parts[0].blob_hash.clone().expect("blob id");
    let blob = store
        .blob_for_account(account, &blob_id)
        .unwrap()
        .expect("blob");
    assert_eq!(blob.bytes, b"hello blob");
    assert_eq!(blob.content_type.as_deref(), Some("text/plain"));
    assert!(!blob.attachment);

    let other = store
        .create_account(
            "imap",
            "other@example.com",
            None,
            "account:other",
            &serde_json::json!({}),
        )
        .unwrap();
    assert!(store.blob_for_account(other, &blob_id).unwrap().is_none());
    assert!(store
        .blob_for_account(account, "not-hex")
        .unwrap()
        .is_none());
}

#[test]
fn mailbox_creation_and_restore_reach_the_change_log() {
    let (store, account, _) = setup();
    let archive = store
        .ensure_mailbox(account, "Archive", Some("archive"))
        .unwrap()
        .id;
    let created = store.changes_since(account, 0, 100).unwrap();
    assert!(created.iter().any(|change| {
        change.entity_type == "mailbox"
            && change.entity_id == archive.to_string()
            && change.op == "created"
    }));

    assert_eq!(
        store
            .mark_missing_mailboxes(account, &["INBOX".to_string()], 1_700_000_000)
            .unwrap(),
        1
    );
    let seq = store.latest_change_seq(account).unwrap();
    store
        .ensure_mailbox(account, "Archive", Some("archive"))
        .unwrap();
    let restored = store.changes_since(account, seq, 100).unwrap();
    assert!(restored.iter().any(|change| {
        change.entity_type == "mailbox"
            && change.entity_id == archive.to_string()
            && change.op == "restored"
    }));
}

#[test]
fn devices_can_be_listed_and_revoked() {
    let (store, _, _) = setup();
    let device = store.create_device("phone", "hash").unwrap();
    let listed = store.list_devices().unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].id, device);
    assert!(store.revoke_device(device).unwrap());
    assert!(store.list_devices().unwrap().is_empty());
    assert!(store.device_by_token_hash("hash").unwrap().is_none());
    assert!(!store.revoke_device(device).unwrap());
}
