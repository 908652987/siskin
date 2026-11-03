//! Behaviour of the JMAP subset against an in-memory store.

use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use mail_api::{
    BodySource, FetchedBody, Jmap, RequestError, WriteError, WriteSink, CORE_CAPABILITY,
    MAIL_CAPABILITY,
};
use mail_store::{BodyPartInput, DbKey, NewMessage, Store};
use serde_json::{json, Value};

const ACCOUNT_EMAIL: &str = "me@example.com";

struct Fixture {
    store: Arc<Store>,
    account: i64,
    inbox: i64,
    sent: i64,
}

fn setup() -> Fixture {
    let key = DbKey::from_bytes(&[7u8; 32]).expect("key");
    let store = Arc::new(Store::open_in_memory(&key).expect("store"));
    let account = store
        .create_account("imap", ACCOUNT_EMAIL, None, "secret", &json!({}))
        .expect("account");
    let inbox = store
        .ensure_mailbox(account, "INBOX", Some("inbox"))
        .expect("inbox")
        .id;
    let sent = store
        .ensure_mailbox(account, "Sent", Some("sent"))
        .expect("sent")
        .id;
    Fixture {
        store,
        account,
        inbox,
        sent,
    }
}

fn message(uid: i64, subject: &str) -> NewMessage {
    NewMessage {
        uid,
        message_id: Some(format!("<{uid}@example.com>")),
        subject: Some(subject.to_string()),
        from: vec!["Alice <alice@example.com>".to_string()],
        to: vec![ACCOUNT_EMAIL.to_string()],
        sent_at: Some(1_700_000_000 + uid),
        size: Some(100 + uid),
        flags: Vec::new(),
        snippet: Some(format!("{subject} snippet")),
    }
}

async fn call(jmap: &Jmap, name: &str, args: Value) -> Value {
    let request = json!({
        "using": [CORE_CAPABILITY, MAIL_CAPABILITY],
        "methodCalls": [[name, args, "c0"]],
    });
    let response = jmap.handle(request).await.expect("request");
    response["methodResponses"][0].clone()
}

fn result(response: &Value) -> Value {
    assert_ne!(response[0], json!("error"), "method failed: {response}");
    response[1].clone()
}

fn ids(value: &Value) -> Vec<String> {
    value
        .as_array()
        .expect("array")
        .iter()
        .map(|id| id.as_str().expect("id").to_string())
        .collect()
}

#[tokio::test]
async fn session_lists_capabilities_and_accounts() {
    let fixture = setup();
    let jmap = Jmap::new(Arc::clone(&fixture.store));
    let session = jmap.session().expect("session");

    assert!(session["capabilities"][CORE_CAPABILITY].is_object());
    assert!(session["capabilities"][MAIL_CAPABILITY].is_object());
    let account = &session["accounts"][fixture.account.to_string()];
    assert_eq!(account["email"], json!(ACCOUNT_EMAIL));
    assert_eq!(
        session["primaryAccounts"][MAIL_CAPABILITY],
        json!(fixture.account.to_string())
    );
}

#[tokio::test]
async fn mailbox_get_reports_roles_and_unread_counts() {
    let fixture = setup();
    let mut read = message(1, "one");
    read.flags = vec!["\\Seen".to_string()];
    fixture
        .store
        .apply_fetch(
            fixture.account,
            fixture.inbox,
            &[read, message(2, "two")],
            &[],
        )
        .expect("fetch");
    fixture
        .store
        .apply_fetch(fixture.account, fixture.sent, &[message(3, "sent")], &[])
        .expect("fetch");

    let jmap = Jmap::new(Arc::clone(&fixture.store));
    let response = call(
        &jmap,
        "Mailbox/get",
        json!({"accountId": fixture.account.to_string()}),
    )
    .await;
    let body = result(&response);
    let list = body["list"].as_array().expect("list");

    let inbox = list
        .iter()
        .find(|mailbox| mailbox["role"] == json!("inbox"))
        .expect("inbox");
    assert_eq!(inbox["totalEmails"], json!(2));
    assert_eq!(inbox["unreadEmails"], json!(1));
    assert_eq!(inbox["totalThreads"], json!(2));

    let sent = list
        .iter()
        .find(|mailbox| mailbox["role"] == json!("sent"))
        .expect("sent");
    assert_eq!(sent["totalEmails"], json!(1));
    assert_eq!(sent["unreadEmails"], json!(1));
}

#[tokio::test]
async fn email_query_filters_by_mailbox_text_and_keyword() {
    let fixture = setup();
    let mut read = message(1, "alpha report");
    read.flags = vec!["\\Seen".to_string()];
    fixture
        .store
        .apply_fetch(
            fixture.account,
            fixture.inbox,
            &[read, message(2, "beta invoice"), message(3, "gamma report")],
            &[],
        )
        .expect("fetch");
    fixture
        .store
        .apply_fetch(
            fixture.account,
            fixture.sent,
            &[message(4, "sent report")],
            &[],
        )
        .expect("fetch");

    let jmap = Jmap::new(Arc::clone(&fixture.store));
    let account = fixture.account.to_string();

    let response = call(
        &jmap,
        "Email/query",
        json!({
            "accountId": account,
            "filter": {"inMailbox": fixture.inbox.to_string(), "text": "report"},
        }),
    )
    .await;
    let page = result(&response);
    assert_eq!(page["total"], json!(2));
    assert_eq!(ids(&page["ids"]), vec!["3", "1"]);

    let response = call(
        &jmap,
        "Email/query",
        json!({"accountId": account, "filter": {"hasKeyword": "$seen"}}),
    )
    .await;
    let page = result(&response);
    assert_eq!(ids(&page["ids"]), vec!["1"]);

    let response = call(
        &jmap,
        "Email/query",
        json!({"accountId": account, "filter": {"notKeyword": "$seen"}}),
    )
    .await;
    let page = result(&response);
    assert_eq!(page["total"], json!(3));
    assert_eq!(ids(&page["ids"]), vec!["4", "3", "2"]);

    let response = call(
        &jmap,
        "Email/query",
        json!({"accountId": account, "position": 2, "limit": 2}),
    )
    .await;
    let page = result(&response);
    assert_eq!(page["total"], json!(4));
    assert_eq!(page["position"], json!(2));
    assert_eq!(ids(&page["ids"]).len(), 2);
}

#[tokio::test]
async fn email_get_returns_locations_keywords_and_cached_body() {
    let fixture = setup();
    let mut stored = message(1, "hello");
    stored.from = vec!["Alice <alice@example.com>".to_string()];
    stored.flags = vec!["\\Seen".to_string(), "\\Flagged".to_string()];
    fixture
        .store
        .apply_fetch(fixture.account, fixture.inbox, &[stored], &[])
        .expect("fetch");
    fixture
        .store
        .apply_fetch(fixture.account, fixture.sent, &[message(1, "hello")], &[])
        .expect("same message in Sent");
    let message_id = fixture
        .store
        .list_messages(fixture.account, 1, 0)
        .expect("list")[0]
        .id;
    fixture
        .store
        .store_body_parts(
            fixture.account,
            message_id,
            &[BodyPartInput {
                part: "1",
                content_type: "text/plain",
                charset: Some("utf-8"),
                is_html: false,
                data: b"hello body",
            }],
            Some("hello body"),
        )
        .expect("body");

    let jmap = Jmap::new(Arc::clone(&fixture.store));
    let response = call(
        &jmap,
        "Email/get",
        json!({
            "accountId": fixture.account.to_string(),
            "ids": [message_id.to_string()],
            "fetchTextBodyValues": true,
            "properties": ["subject", "mailboxIds", "keywords", "from", "textBody", "bodyValues"],
        }),
    )
    .await;
    let body = result(&response);
    let email = &body["list"][0];

    assert_eq!(email["subject"], json!("hello"));
    assert_eq!(email["keywords"]["$seen"], json!(true));
    assert_eq!(email["keywords"]["$flagged"], json!(true));
    assert_eq!(email["mailboxIds"][fixture.inbox.to_string()], json!(true));
    assert_eq!(email["mailboxIds"][fixture.sent.to_string()], json!(true));
    assert_eq!(email["from"][0]["email"], json!("alice@example.com"));
    assert_eq!(email["from"][0]["name"], json!("Alice"));
    assert_eq!(email["bodyValues"]["1"]["value"], json!("hello body"));
    assert_eq!(email["textBody"][0]["partId"], json!("1"));
}

struct StubBodies {
    calls: AtomicUsize,
}

impl BodySource for StubBodies {
    fn fetch<'a>(
        &'a self,
        _account_id: i64,
        _message_id: i64,
    ) -> Pin<Box<dyn Future<Output = Result<FetchedBody, String>> + Send + 'a>> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Box::pin(async {
            Ok(FetchedBody {
                text: Some("fetched from the server".to_string()),
                html: None,
            })
        })
    }
}

#[tokio::test]
async fn email_get_fetches_uncached_bodies_through_the_source() {
    let fixture = setup();
    fixture
        .store
        .apply_fetch(fixture.account, fixture.inbox, &[message(1, "hello")], &[])
        .expect("fetch");
    let message_id = fixture
        .store
        .list_messages(fixture.account, 1, 0)
        .expect("list")[0]
        .id;

    let bodies = Arc::new(StubBodies {
        calls: AtomicUsize::new(0),
    });
    let jmap = Jmap::new(Arc::clone(&fixture.store))
        .with_bodies(Arc::clone(&bodies) as Arc<dyn BodySource>);
    let response = call(
        &jmap,
        "Email/get",
        json!({
            "accountId": fixture.account.to_string(),
            "ids": [message_id.to_string()],
            "fetchTextBodyValues": true,
        }),
    )
    .await;
    let body = result(&response);

    assert_eq!(
        body["list"][0]["bodyValues"]["text"]["value"],
        json!("fetched from the server")
    );
    assert_eq!(bodies.calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn email_changes_reports_created_updated_and_destroyed() {
    let fixture = setup();
    fixture
        .store
        .apply_fetch(fixture.account, fixture.inbox, &[message(1, "one")], &[])
        .expect("fetch");
    let message_id = fixture
        .store
        .list_messages(fixture.account, 1, 0)
        .expect("list")[0]
        .id;

    let jmap = Jmap::new(Arc::clone(&fixture.store));
    let account = fixture.account.to_string();
    let response = call(
        &jmap,
        "Email/changes",
        json!({"accountId": account, "sinceState": "0"}),
    )
    .await;
    let changes = result(&response);
    assert_eq!(ids(&changes["created"]), vec![message_id.to_string()]);
    let first_state = changes["newState"].as_str().expect("state").to_string();

    let mut seen = message(1, "one");
    seen.flags = vec!["\\Seen".to_string()];
    fixture
        .store
        .apply_fetch(fixture.account, fixture.inbox, &[seen], &[])
        .expect("flag update");
    let response = call(
        &jmap,
        "Email/changes",
        json!({"accountId": account, "sinceState": first_state}),
    )
    .await;
    let changes = result(&response);
    assert_eq!(ids(&changes["updated"]), vec![message_id.to_string()]);
    let second_state = changes["newState"].as_str().expect("state").to_string();

    fixture
        .store
        .apply_fetch(fixture.account, fixture.inbox, &[], &[1])
        .expect("removal");
    let response = call(
        &jmap,
        "Email/changes",
        json!({"accountId": account, "sinceState": second_state}),
    )
    .await;
    let changes = result(&response);
    assert_eq!(ids(&changes["destroyed"]), vec![message_id.to_string()]);
    assert_eq!(changes["hasMoreChanges"], json!(false));
}

#[tokio::test]
async fn email_changes_requires_a_reload_after_compaction() {
    let fixture = setup();
    fixture
        .store
        .apply_fetch(
            fixture.account,
            fixture.inbox,
            &[message(1, "one"), message(2, "two")],
            &[],
        )
        .expect("fetch");
    let compacted = fixture
        .store
        .compact_changes(fixture.account, 1)
        .expect("compact");
    assert!(compacted.is_some());

    let jmap = Jmap::new(Arc::clone(&fixture.store));
    let response = call(
        &jmap,
        "Email/changes",
        json!({"accountId": fixture.account.to_string(), "sinceState": "0"}),
    )
    .await;
    assert_eq!(response[0], json!("error"));
    assert_eq!(response[1]["type"], json!("cannotCalculateChanges"));
}

#[tokio::test]
async fn unsupported_inputs_fail_explicitly() {
    let fixture = setup();
    let jmap = Jmap::new(Arc::clone(&fixture.store));
    let account = fixture.account.to_string();

    let request = json!({
        "using": ["urn:example:nope"],
        "methodCalls": [["Mailbox/get", {"accountId": account}, "c0"]],
    });
    let err = jmap.handle(request).await.expect_err("unknown capability");
    assert!(matches!(err, RequestError::UnknownCapability(_)));

    let response = call(
        &jmap,
        "Email/query",
        json!({"accountId": account, "filter": {"operator": "AND"}}),
    )
    .await;
    assert_eq!(response[0], json!("error"));
    assert_eq!(response[1]["type"], json!("unsupportedFilter"));

    let response = call(
        &jmap,
        "Email/query",
        json!({"accountId": account, "sort": [{"property": "receivedAt", "isAscending": true}]}),
    )
    .await;
    assert_eq!(response[1]["type"], json!("unsupportedSort"));

    let response = call(&jmap, "Thread/get", json!({"accountId": account})).await;
    assert_eq!(response[1]["type"], json!("unknownMethod"));
}

/// Applies keyword writes to the store so `Email/get` and `Email/changes`
/// observe what the sink reported as done.
struct StoreWrites {
    store: Arc<Store>,
}

impl WriteSink for StoreWrites {
    fn set_keywords<'a>(
        &'a self,
        account_id: i64,
        message_id: i64,
        add: Vec<String>,
        remove: Vec<String>,
    ) -> Pin<Box<dyn Future<Output = Result<(), WriteError>> + Send + 'a>> {
        Box::pin(async move {
            let flags = |keywords: Vec<String>| -> Vec<String> {
                keywords
                    .into_iter()
                    .map(|keyword| match keyword.as_str() {
                        "$seen" => "\\Seen".to_string(),
                        "$flagged" => "\\Flagged".to_string(),
                        "$answered" => "\\Answered".to_string(),
                        "$draft" => "\\Draft".to_string(),
                        "$forwarded" => "\\Forwarded".to_string(),
                        other => other.to_string(),
                    })
                    .collect()
            };
            self.store
                .update_flags(account_id, message_id, &flags(add), &flags(remove))
                .map_err(|err| WriteError::Server(err.to_string()))?;
            Ok(())
        })
    }

    fn move_message<'a>(
        &'a self,
        _account_id: i64,
        _message_id: i64,
        _mailbox_id: i64,
    ) -> Pin<Box<dyn Future<Output = Result<(), WriteError>> + Send + 'a>> {
        Box::pin(async { Ok(()) })
    }
}

/// Records move calls without touching the store.
#[derive(Default)]
struct RecordingWrites {
    moves: Mutex<Vec<(i64, i64)>>,
}

impl WriteSink for RecordingWrites {
    fn set_keywords<'a>(
        &'a self,
        _account_id: i64,
        _message_id: i64,
        _add: Vec<String>,
        _remove: Vec<String>,
    ) -> Pin<Box<dyn Future<Output = Result<(), WriteError>> + Send + 'a>> {
        Box::pin(async { Ok(()) })
    }

    fn move_message<'a>(
        &'a self,
        _account_id: i64,
        message_id: i64,
        mailbox_id: i64,
    ) -> Pin<Box<dyn Future<Output = Result<(), WriteError>> + Send + 'a>> {
        Box::pin(async move {
            self.moves
                .lock()
                .expect("moves")
                .push((message_id, mailbox_id));
            Ok(())
        })
    }
}

#[tokio::test]
async fn email_set_without_a_sink_reports_not_supported() {
    let fixture = setup();
    fixture
        .store
        .apply_fetch(fixture.account, fixture.inbox, &[message(1, "one")], &[])
        .expect("fetch");
    let id = fixture.store.list_messages(fixture.account, 10, 0).unwrap()[0]
        .id
        .to_string();
    let jmap = Jmap::new(Arc::clone(&fixture.store));

    let response = call(
        &jmap,
        "Email/set",
        json!({
            "accountId": fixture.account.to_string(),
            "update": { id.clone(): {"keywords": {"$seen": true}} },
        }),
    )
    .await;
    let body = result(&response);
    assert_eq!(body["updated"].as_object().expect("updated").len(), 0);
    assert_eq!(body["notUpdated"][&id]["type"], json!("notSupported"));
}

#[tokio::test]
async fn email_set_applies_keywords_and_advances_state() {
    let fixture = setup();
    fixture
        .store
        .apply_fetch(fixture.account, fixture.inbox, &[message(1, "one")], &[])
        .expect("fetch");
    let id = fixture.store.list_messages(fixture.account, 10, 0).unwrap()[0]
        .id
        .to_string();
    let old_state = fixture
        .store
        .latest_change_seq(fixture.account)
        .unwrap()
        .to_string();
    let jmap = Jmap::new(Arc::clone(&fixture.store)).with_writes(Arc::new(StoreWrites {
        store: Arc::clone(&fixture.store),
    }));

    let response = call(
        &jmap,
        "Email/set",
        json!({
            "accountId": fixture.account.to_string(),
            "update": { id.clone(): {"keywords": {"$seen": true}} },
        }),
    )
    .await;
    let body = result(&response);
    assert!(body["updated"].get(&id).is_some());
    assert_ne!(body["newState"], json!(old_state));

    let fetched = result(
        &call(
            &jmap,
            "Email/get",
            json!({"accountId": fixture.account.to_string(), "ids": [id.clone()]}),
        )
        .await,
    );
    assert_eq!(fetched["list"][0]["keywords"]["$seen"], json!(true));

    let changes = result(
        &call(
            &jmap,
            "Email/changes",
            json!({"accountId": fixture.account.to_string(), "sinceState": old_state}),
        )
        .await,
    );
    assert!(ids(&changes["updated"]).contains(&id));
}

#[tokio::test]
async fn email_set_rejects_a_stale_if_in_state() {
    let fixture = setup();
    let jmap = Jmap::new(Arc::clone(&fixture.store));
    let response = call(
        &jmap,
        "Email/set",
        json!({
            "accountId": fixture.account.to_string(),
            "ifInState": "999999",
            "update": {},
        }),
    )
    .await;
    assert_eq!(response[0], json!("error"));
    assert_eq!(response[1]["type"], json!("stateMismatch"));

    let response = call(
        &jmap,
        "Email/set",
        json!({
            "accountId": fixture.account.to_string(),
            "ifInState": null,
            "update": {},
        }),
    )
    .await;
    assert_ne!(response[0], json!("error"));
}

#[tokio::test]
async fn email_set_moves_through_the_sink() {
    let fixture = setup();
    fixture
        .store
        .apply_fetch(
            fixture.account,
            fixture.inbox,
            &[message(7, "move me")],
            &[],
        )
        .expect("fetch");
    let id = fixture.store.list_messages(fixture.account, 10, 0).unwrap()[0].id;
    let writes = Arc::new(RecordingWrites::default());
    let jmap = Jmap::new(Arc::clone(&fixture.store)).with_writes(writes.clone());

    let response = call(
        &jmap,
        "Email/set",
        json!({
            "accountId": fixture.account.to_string(),
            "update": { id.to_string(): {"mailboxIds": { fixture.sent.to_string(): true }} },
        }),
    )
    .await;
    let body = result(&response);
    assert!(body["updated"].get(id.to_string()).is_some());
    assert_eq!(
        *writes.moves.lock().expect("moves"),
        vec![(id, fixture.sent)]
    );
}

#[tokio::test]
async fn mailbox_changes_reports_new_mailboxes_only() {
    let fixture = setup();
    let baseline = fixture
        .store
        .latest_change_seq(fixture.account)
        .unwrap()
        .to_string();
    let archive = fixture
        .store
        .ensure_mailbox(fixture.account, "Archive", Some("archive"))
        .expect("archive")
        .id
        .to_string();
    let jmap = Jmap::new(Arc::clone(&fixture.store));

    let changes = result(
        &call(
            &jmap,
            "Mailbox/changes",
            json!({"accountId": fixture.account.to_string(), "sinceState": baseline.clone()}),
        )
        .await,
    );
    assert!(ids(&changes["created"]).contains(&archive));

    let email_changes = result(
        &call(
            &jmap,
            "Email/changes",
            json!({"accountId": fixture.account.to_string(), "sinceState": baseline}),
        )
        .await,
    );
    assert!(!ids(&email_changes["created"]).contains(&archive));
}

#[tokio::test]
async fn email_get_exposes_blob_ids_for_cached_parts() {
    let fixture = setup();
    fixture
        .store
        .apply_fetch(fixture.account, fixture.inbox, &[message(3, "body")], &[])
        .expect("fetch");
    let id = fixture.store.list_messages(fixture.account, 10, 0).unwrap()[0].id;
    fixture
        .store
        .store_body_parts(
            fixture.account,
            id,
            &[BodyPartInput {
                part: "1",
                content_type: "text/plain",
                charset: Some("utf-8"),
                is_html: false,
                data: b"hello body",
            }],
            Some("hello body"),
        )
        .expect("body");
    let jmap = Jmap::new(Arc::clone(&fixture.store));

    let fetched = result(
        &call(
            &jmap,
            "Email/get",
            json!({"accountId": fixture.account.to_string(), "ids": [id.to_string()]}),
        )
        .await,
    );
    let blob_id = fetched["list"][0]["textBody"][0]["blobId"]
        .as_str()
        .expect("blob id");
    assert_eq!(blob_id.len(), 64);
    let blob = fixture
        .store
        .blob_for_account(fixture.account, blob_id)
        .unwrap()
        .expect("blob");
    assert_eq!(blob.bytes, b"hello body");
}
