use mail_store::{
    AttachmentMetaInput, BodyPartInput, BodyState, DbKey, NewMessage, PartMetaInput, Store,
};

fn test_key() -> DbKey {
    DbKey::from_bytes(&[9u8; 32]).expect("key")
}

fn setup() -> (Store, i64, i64) {
    let store = Store::open_in_memory(&test_key()).expect("store");
    let account = store
        .create_account(
            "imap",
            "bodies@example.com",
            None,
            "account:bodies",
            &serde_json::json!({}),
        )
        .expect("account");
    let mailbox = store
        .ensure_mailbox(account, "INBOX", Some("inbox"))
        .expect("mailbox");
    (store, account, mailbox.id)
}

fn message(uid: i64, id: &str) -> NewMessage {
    NewMessage {
        uid,
        message_id: Some(format!("<{id}@test>")),
        subject: Some(id.to_string()),
        from: vec!["sender@test".to_string()],
        to: vec!["bodies@example.com".to_string()],
        sent_at: Some(1_700_000_000 + uid),
        size: Some(2048),
        flags: vec![],
        snippet: None,
    }
}

fn store_message(store: &Store, account: i64, mailbox: i64, uid: i64, id: &str) -> i64 {
    store
        .apply_fetch(account, mailbox, &[message(uid, id)], &[])
        .expect("apply");
    store
        .list_messages(account, 10, 0)
        .expect("list")
        .into_iter()
        .find(|msg| msg.uid == uid)
        .expect("stored message")
        .id
}

fn text_part(part: &str) -> PartMetaInput<'_> {
    PartMetaInput {
        part,
        content_type: "text/plain",
        charset: Some("utf-8"),
        is_html: false,
        size: 64,
    }
}

#[test]
fn bodies_and_attachments_move_the_state_to_full() {
    let (store, account, mailbox) = setup();
    let message_id = store_message(&store, account, mailbox, 1, "one");

    store
        .record_body_structure(
            account,
            message_id,
            &[
                text_part("1"),
                PartMetaInput {
                    part: "2",
                    content_type: "text/html",
                    charset: Some("utf-8"),
                    is_html: true,
                    size: 128,
                },
            ],
            &[AttachmentMetaInput {
                part: "3",
                filename: Some("report.pdf"),
                content_type: "application/pdf",
                encoding: Some("base64"),
                size: 4096,
            }],
        )
        .expect("structure");

    let body = store
        .message_body(message_id)
        .expect("body")
        .expect("exists");
    assert_eq!(body.state, BodyState::Partial);
    assert_eq!(body.parts.len(), 2);
    assert_eq!(body.attachments.len(), 1);
    assert!(!body.attachments[0].downloaded);
    assert!(body.text.is_none());

    store
        .store_body_parts(
            account,
            message_id,
            &[
                BodyPartInput {
                    part: "1",
                    content_type: "text/plain",
                    charset: Some("utf-8"),
                    is_html: false,
                    data: b"hello",
                },
                BodyPartInput {
                    part: "2",
                    content_type: "text/html",
                    charset: Some("utf-8"),
                    is_html: true,
                    data: b"<p>hello</p>",
                },
            ],
        )
        .expect("bodies");

    let body = store
        .message_body(message_id)
        .expect("body")
        .expect("exists");
    assert_eq!(body.state, BodyState::Partial, "attachment still missing");
    assert_eq!(body.text.as_deref(), Some("hello"));
    assert_eq!(body.html.as_deref(), Some("<p>hello</p>"));

    assert!(store
        .attachment_data(message_id, "3")
        .expect("attachment lookup")
        .is_none());

    store
        .store_attachment(
            account,
            message_id,
            "3",
            "application/pdf",
            Some("report.pdf"),
            b"%PDF-1.4",
        )
        .expect("attachment");

    let body = store
        .message_body(message_id)
        .expect("body")
        .expect("exists");
    assert_eq!(body.state, BodyState::Full);
    assert!(body.attachments[0].downloaded);
    assert_eq!(
        store.attachment_data(message_id, "3").expect("data"),
        Some(b"%PDF-1.4".to_vec())
    );
}

#[test]
fn identical_bodies_share_one_blob() {
    let (store, account, mailbox) = setup();
    let first = store_message(&store, account, mailbox, 1, "one");
    let second = store_message(&store, account, mailbox, 2, "two");
    for id in [first, second] {
        store
            .record_body_structure(account, id, &[text_part("1")], &[])
            .expect("structure");
        store
            .store_body_parts(
                account,
                id,
                &[BodyPartInput {
                    part: "1",
                    content_type: "text/plain",
                    charset: Some("utf-8"),
                    is_html: false,
                    data: b"shared body",
                }],
            )
            .expect("body");
    }
    assert_eq!(
        store.blob_bytes().expect("bytes"),
        "shared body".len() as i64,
        "content addressing must deduplicate identical parts"
    );

    store.reset_mailbox(mailbox, 42).expect("reset");
    assert_eq!(
        store.blob_bytes().expect("bytes"),
        0,
        "dropping the mailbox must release every blob"
    );
}

#[test]
fn restoring_the_same_body_does_not_leak_references() {
    let (store, account, mailbox) = setup();
    let message_id = store_message(&store, account, mailbox, 1, "one");
    let part = BodyPartInput {
        part: "1",
        content_type: "text/plain",
        charset: Some("utf-8"),
        is_html: false,
        data: b"repeated",
    };
    store
        .record_body_structure(account, message_id, &[text_part("1")], &[])
        .expect("structure");
    store
        .store_body_parts(account, message_id, &[part])
        .expect("first store");
    store
        .store_body_parts(
            account,
            message_id,
            &[BodyPartInput {
                part: "1",
                content_type: "text/plain",
                charset: Some("utf-8"),
                is_html: false,
                data: b"repeated",
            }],
        )
        .expect("second store");
    assert_eq!(store.blob_bytes().expect("bytes"), 8);

    store.reset_mailbox(mailbox, 43).expect("reset");
    assert_eq!(
        store.blob_bytes().expect("bytes"),
        0,
        "re-storing identical bytes must not leave an extra reference"
    );
}

#[test]
fn a_new_layout_drops_stale_parts_and_their_blobs() {
    let (store, account, mailbox) = setup();
    let message_id = store_message(&store, account, mailbox, 1, "one");
    store
        .record_body_structure(
            account,
            message_id,
            &[text_part("1")],
            &[AttachmentMetaInput {
                part: "2",
                filename: Some("old.bin"),
                content_type: "application/octet-stream",
                encoding: None,
                size: 4,
            }],
        )
        .expect("structure");
    store
        .store_attachment(
            account,
            message_id,
            "2",
            "application/octet-stream",
            Some("old.bin"),
            b"old!",
        )
        .expect("attachment");

    store
        .record_body_structure(account, message_id, &[text_part("1")], &[])
        .expect("re-structure");

    let body = store
        .message_body(message_id)
        .expect("body")
        .expect("exists");
    assert!(body.attachments.is_empty());
    assert_eq!(store.blob_bytes().expect("bytes"), 0);
}

#[test]
fn context_identifies_the_fetch_target() {
    let (store, account, mailbox) = setup();
    let message_id = store_message(&store, account, mailbox, 17, "seventeen");

    let context = store
        .message_context(message_id)
        .expect("context")
        .expect("exists");
    assert_eq!(context.account_id, account);
    assert_eq!(context.mailbox_id, mailbox);
    assert_eq!(context.mailbox_name, "INBOX");
    assert_eq!(context.uid, 17);
    assert_eq!(context.state, BodyState::Headers);

    assert!(store.message_context(9_999).expect("missing").is_none());
    assert!(store.message_body(9_999).expect("missing").is_none());
}
