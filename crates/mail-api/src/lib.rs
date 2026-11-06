//! JMAP subset (RFC 8620 / RFC 8621) over the local store.
//!
//! Transport-agnostic: the Hub owns HTTP, authentication and network access.
//! Implemented methods are `Mailbox/get`, `Mailbox/changes`, `Email/query`,
//! `Email/get`, `Email/changes`, `Email/set`, `Email/import`,
//! `Thread/get|changes`, `Identity/get` and `EmailSubmission/get|set`;
//! everything else returns a JMAP `unknownMethod` error. Bodies are read from
//! the store and fetched through [`BodySource`]; writes go through
//! [`WriteSink`] and [`SubmissionSink`] so this crate keeps the
//! `mail-api -> mail-store` dependency edge. Requests scoped to one Hub device
//! use [`Jmap::handle_for_account`], which refuses calls naming another account.

use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use chrono::{DateTime, SecondsFormat, Utc};
use mail_store::{
    Account, EmailSubmission, MessageBody, MessageLocationInfo, MessageQuery, MessageWithLocations,
    Store, StoredAttachment, StoredPart,
};
use serde_json::{json, Map, Value};
use thiserror::Error;

pub const CORE_CAPABILITY: &str = "urn:ietf:params:jmap:core";
pub const MAIL_CAPABILITY: &str = "urn:ietf:params:jmap:mail";
pub const SUBMISSION_CAPABILITY: &str = "urn:ietf:params:jmap:submission";

/// Upper bounds advertised in the Session object and enforced here.
pub const MAX_CALLS_IN_REQUEST: usize = 32;
pub const MAX_OBJECTS_IN_GET: usize = 256;
pub const MAX_OBJECTS_IN_SET: usize = 64;
pub const MAX_MESSAGES_PER_QUERY: i64 = 1_000;
pub const MAX_CHANGES: i64 = 1_000;
/// Largest blob a client may upload; also advertised as `maxSizeUpload`.
pub const MAX_UPLOAD_BYTES: i64 = 50 * 1024 * 1024;

/// Keywords the store can round-trip to IMAP flags.
pub const SUPPORTED_KEYWORDS: [&str; 5] =
    ["$seen", "$flagged", "$answered", "$draft", "$forwarded"];

#[derive(Debug, Error)]
pub enum ApiError {
    #[error(transparent)]
    Store(#[from] mail_store::StoreError),
    #[error("invalid arguments")]
    InvalidArguments,
    #[error("unknown account: {0}")]
    UnknownAccount(String),
    #[error("unknown method: {0}")]
    UnknownMethod(String),
    #[error("unsupported filter")]
    UnsupportedFilter,
    #[error("unsupported sort")]
    UnsupportedSort,
    #[error("cannot calculate changes")]
    CannotCalculateChanges,
    #[error("state mismatch")]
    StateMismatch,
    #[error("body fetch failed: {0}")]
    BodyFetch(String),
}

impl ApiError {
    /// JMAP method-level error name (RFC 8620 §3.6.2).
    pub fn jmap_type(&self) -> &'static str {
        match self {
            ApiError::Store(_) => "serverFail",
            ApiError::InvalidArguments => "invalidArguments",
            ApiError::UnknownAccount(_) => "accountNotFound",
            ApiError::UnknownMethod(_) => "unknownMethod",
            ApiError::UnsupportedFilter => "unsupportedFilter",
            ApiError::UnsupportedSort => "unsupportedSort",
            ApiError::CannotCalculateChanges => "cannotCalculateChanges",
            ApiError::StateMismatch => "stateMismatch",
            ApiError::BodyFetch(_) => "serverFail",
        }
    }
}

/// A request-level failure; the Hub turns it into an HTTP problem document.
#[derive(Debug, Error)]
pub enum RequestError {
    #[error("request is not a JMAP request object")]
    NotRequest,
    #[error("unknown capability: {0}")]
    UnknownCapability(String),
    #[error("request exceeds maxCallsInRequest")]
    TooManyCalls,
    #[error(transparent)]
    Store(#[from] mail_store::StoreError),
}

impl RequestError {
    /// `urn:ietf:params:jmap:error:*` suffix, or `None` for a plain bad request.
    pub fn problem_type(&self) -> &'static str {
        match self {
            RequestError::UnknownCapability(_) => "unknownCapability",
            RequestError::TooManyCalls => "limit",
            _ => "notRequest",
        }
    }
}

/// Body bytes that a transport (the Hub) can fetch from the mail server.
#[derive(Debug, Clone, Default)]
pub struct FetchedBody {
    pub text: Option<String>,
    pub html: Option<String>,
}

/// One blob filed by `Email/import` or `Email/set create`.
struct ImportedEmail {
    message_id: i64,
    blob_id: String,
    size: i64,
}

/// On-demand body access. Implemented by the Hub with `mail-core`; keeping it
/// behind a trait preserves the `mail-api -> mail-store` dependency edge.
pub trait BodySource: Send + Sync {
    fn fetch<'a>(
        &'a self,
        account_id: i64,
        message_id: i64,
    ) -> Pin<Box<dyn Future<Output = Result<FetchedBody, String>> + Send + 'a>>;
}

/// Failure of one write, mapped to a JMAP per-object error type.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WriteError {
    NotFound(String),
    NotSupported(String),
    Server(String),
}

/// Write access to the mail server. Implemented by the Hub with `mail-core`;
/// the trait keeps IMAP types and credentials out of this crate.
pub trait WriteSink: Send + Sync {
    fn set_keywords<'a>(
        &'a self,
        account_id: i64,
        message_id: i64,
        add: Vec<String>,
        remove: Vec<String>,
    ) -> Pin<Box<dyn Future<Output = Result<(), WriteError>> + Send + 'a>>;

    fn move_message<'a>(
        &'a self,
        account_id: i64,
        message_id: i64,
        mailbox_id: i64,
    ) -> Pin<Box<dyn Future<Output = Result<(), WriteError>> + Send + 'a>>;

    /// Files raw RFC 5322 bytes into `mailbox_id` and returns the new message
    /// id. The sink owns the server round trip: this crate never sees IMAP.
    fn import_message<'a>(
        &'a self,
        account_id: i64,
        mailbox_id: i64,
        raw: &'a [u8],
        keywords: Vec<String>,
    ) -> Pin<Box<dyn Future<Output = Result<i64, WriteError>> + Send + 'a>>;

    /// Permanently removes the message from every mailbox. Moving to Trash is
    /// a `move_message` call, matching JMAP's model where Trash is a mailbox.
    fn destroy_message<'a>(
        &'a self,
        account_id: i64,
        message_id: i64,
    ) -> Pin<Box<dyn Future<Output = Result<(), WriteError>> + Send + 'a>>;
}

/// SMTP envelope of one submission (RFC 8621 §7.1). `Bcc` recipients exist
/// only here, never in the message header.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubmissionEnvelope {
    pub mail_from: String,
    pub rcpt_to: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubmissionOutcome {
    /// The envelope actually used; the server derives one when the client
    /// omitted it.
    pub envelope: SubmissionEnvelope,
    pub recipients: usize,
    pub saved_to: Option<String>,
    pub save_error: Option<String>,
}

/// Delivers a message the client stored in the account. Implemented by the Hub
/// with `mail-core`; the trait keeps SMTP out of this crate.
pub trait SubmissionSink: Send + Sync {
    fn submit<'a>(
        &'a self,
        account_id: i64,
        raw: Vec<u8>,
        envelope: Option<SubmissionEnvelope>,
    ) -> Pin<Box<dyn Future<Output = Result<SubmissionOutcome, WriteError>> + Send + 'a>>;
}

pub struct Jmap {
    store: Arc<Store>,
    bodies: Option<Arc<dyn BodySource>>,
    writes: Option<Arc<dyn WriteSink>>,
    submissions: Option<Arc<dyn SubmissionSink>>,
}

impl Jmap {
    pub fn new(store: Arc<Store>) -> Self {
        Self {
            store,
            bodies: None,
            writes: None,
            submissions: None,
        }
    }

    pub fn with_bodies(mut self, bodies: Arc<dyn BodySource>) -> Self {
        self.bodies = Some(bodies);
        self
    }

    pub fn with_writes(mut self, writes: Arc<dyn WriteSink>) -> Self {
        self.writes = Some(writes);
        self
    }

    pub fn with_submissions(mut self, submissions: Arc<dyn SubmissionSink>) -> Self {
        self.submissions = Some(submissions);
        self
    }

    /// Session object served from `/.well-known/jmap`. Every local account is
    /// listed; the first one is primary.
    pub fn session(&self) -> Result<Value, mail_store::StoreError> {
        let accounts = self.store.list_accounts()?;
        self.session_for_accounts(&accounts)
    }

    /// Session limited to the account a paired device may use. The Hub must
    /// never advertise accounts the token cannot reach.
    pub fn session_for(&self, account_id: i64) -> Result<Option<Value>, mail_store::StoreError> {
        match self.store.account_by_id(account_id)? {
            Some(account) => Ok(Some(self.session_for_accounts(&[account])?)),
            None => Ok(None),
        }
    }

    fn session_for_accounts(&self, accounts: &[Account]) -> Result<Value, mail_store::StoreError> {
        let mut account_objects = Map::new();
        for account in accounts {
            let id = account.id.to_string();
            account_objects.insert(
                id,
                json!({
                    "name": account.display_name.clone().unwrap_or_else(|| account.email.clone()),
                    "email": account.email,
                    "isPersonal": true,
                    "isReadOnly": false,
                    "accountCapabilities": {
                        CORE_CAPABILITY: {},
                        MAIL_CAPABILITY: {},
                        SUBMISSION_CAPABILITY: {},
                    },
                }),
            );
        }
        let primary = accounts.first().map(|account| account.id.to_string());
        let mut primary_accounts = Map::new();
        if let Some(primary) = &primary {
            primary_accounts.insert(CORE_CAPABILITY.to_string(), json!(primary));
            primary_accounts.insert(MAIL_CAPABILITY.to_string(), json!(primary));
            primary_accounts.insert(SUBMISSION_CAPABILITY.to_string(), json!(primary));
        }
        Ok(json!({
            "capabilities": {
                CORE_CAPABILITY: {
                    "maxSizeUpload": MAX_UPLOAD_BYTES,
                    "maxConcurrentUpload": if self.writes.is_some() { 4 } else { 0 },
                    "maxSizeRequest": 10_000_000,
                    "maxConcurrentRequests": 4,
                    "maxCallsInRequest": MAX_CALLS_IN_REQUEST,
                    "maxObjectsInGet": MAX_OBJECTS_IN_GET,
                    "maxObjectsInSet": if self.writes.is_some() { MAX_OBJECTS_IN_SET } else { 0 },
                    "collationAlgorithms": ["i;ascii-casemap"],
                },
                MAIL_CAPABILITY: {},
                SUBMISSION_CAPABILITY: {
                    "maxDelayedSend": 0,
                    "submissionExtensions": {},
                },
            },
            "accounts": account_objects,
            "primaryAccounts": primary_accounts,
            "username": accounts
                .first()
                .map(|account| account.email.clone())
                .unwrap_or_default(),
            "apiUrl": "/jmap",
            "downloadUrl": "/download/{accountId}/{blobId}/{name}?accept={type}",
            "uploadUrl": "/upload/{accountId}/",
            "eventSourceUrl": "/eventsource/?types={types}&closeafter={closeafter}&ping={ping}",
            "state": format!("s{}", accounts.len()),
        }))
    }

    /// Executes one JMAP request. Method failures stay inside `methodResponses`
    /// as `error` entries; only malformed requests or unknown capabilities
    /// fail the whole call.
    pub async fn handle(&self, request: Value) -> Result<Value, RequestError> {
        self.handle_inner(request, None).await
    }

    /// Executes a request on behalf of a device bound to one account. Every
    /// method call must name that account; a call naming another account fails
    /// with `accountNotFound` and never touches the store.
    pub async fn handle_for_account(
        &self,
        request: Value,
        account_id: i64,
    ) -> Result<Value, RequestError> {
        self.handle_inner(request, Some(account_id)).await
    }

    async fn handle_inner(
        &self,
        request: Value,
        bound: Option<i64>,
    ) -> Result<Value, RequestError> {
        let object = request.as_object().ok_or(RequestError::NotRequest)?;
        if let Some(using) = object.get("using") {
            let using = using.as_array().ok_or(RequestError::NotRequest)?;
            for capability in using {
                let capability = capability.as_str().ok_or(RequestError::NotRequest)?;
                if capability != CORE_CAPABILITY
                    && capability != MAIL_CAPABILITY
                    && capability != SUBMISSION_CAPABILITY
                {
                    return Err(RequestError::UnknownCapability(capability.to_string()));
                }
            }
        }
        let calls = object
            .get("methodCalls")
            .and_then(Value::as_array)
            .ok_or(RequestError::NotRequest)?;
        if calls.len() > MAX_CALLS_IN_REQUEST {
            return Err(RequestError::TooManyCalls);
        }
        let bound_name = bound.map(|id| id.to_string());

        let mut responses = Vec::with_capacity(calls.len());
        for call in calls {
            let parts = call.as_array().ok_or(RequestError::NotRequest)?;
            if parts.len() != 3 {
                return Err(RequestError::NotRequest);
            }
            let name = parts[0].as_str().ok_or(RequestError::NotRequest)?;
            let args = parts[1].as_object().ok_or(RequestError::NotRequest)?;
            let call_id = parts[2].as_str().ok_or(RequestError::NotRequest)?;
            if let Some(bound_name) = &bound_name {
                let requested = args.get("accountId").and_then(Value::as_str);
                if requested != Some(bound_name.as_str()) {
                    responses.push(json!([
                        "error",
                        {
                            "type": "accountNotFound",
                            "description": "the device is not authorized for this account",
                        },
                        call_id
                    ]));
                    continue;
                }
            }
            match self.dispatch(name, args).await {
                Ok(items) => {
                    // A method may produce a follow-up response (the implicit
                    // `Email/set` of `EmailSubmission/set`); both keep the same
                    // method call id, as RFC 8621 §7.5 requires.
                    for (method, value) in items {
                        responses.push(json!([method, value, call_id]));
                    }
                }
                Err(err) => responses.push(json!([
                    "error",
                    {
                        "type": err.jmap_type(),
                        "description": err.to_string(),
                    },
                    call_id
                ])),
            }
        }

        Ok(json!({
            "methodResponses": responses,
            "sessionState": self.session_state()?,
        }))
    }

    fn session_state(&self) -> Result<String, mail_store::StoreError> {
        Ok(format!("s{}", self.store.list_accounts()?.len()))
    }

    async fn dispatch(
        &self,
        name: &str,
        args: &Map<String, Value>,
    ) -> Result<Vec<(&'static str, Value)>, ApiError> {
        let single = |value: Value, method: &'static str| Ok(vec![(method, value)]);
        match name {
            "Mailbox/get" => single(self.mailbox_get(args)?, "Mailbox/get"),
            "Mailbox/changes" => single(self.changes(args, "mailbox")?, "Mailbox/changes"),
            "Email/query" => single(self.email_query(args)?, "Email/query"),
            "Email/get" => single(self.email_get(args).await?, "Email/get"),
            "Email/changes" => single(self.changes(args, "message")?, "Email/changes"),
            "Email/set" => single(self.email_set(args).await?, "Email/set"),
            "Email/import" => single(self.email_import(args).await?, "Email/import"),
            "Thread/get" => single(self.thread_get(args)?, "Thread/get"),
            "Thread/changes" => single(self.changes(args, "thread")?, "Thread/changes"),
            "Identity/get" => single(self.identity_get(args)?, "Identity/get"),
            "EmailSubmission/get" => {
                single(self.email_submission_get(args)?, "EmailSubmission/get")
            }
            "EmailSubmission/set" => self.email_submission_set(args).await,
            other => Err(ApiError::UnknownMethod(other.to_string())),
        }
    }

    fn account_id(&self, args: &Map<String, Value>) -> Result<i64, ApiError> {
        let raw = args
            .get("accountId")
            .and_then(Value::as_str)
            .ok_or(ApiError::InvalidArguments)?;
        let id: i64 = raw.parse().map_err(|_| ApiError::InvalidArguments)?;
        if self.store.account_by_id(id)?.is_none() {
            return Err(ApiError::UnknownAccount(raw.to_string()));
        }
        Ok(id)
    }

    fn mailbox_get(&self, args: &Map<String, Value>) -> Result<Value, ApiError> {
        let account_id = self.account_id(args)?;
        let wanted: Option<HashSet<i64>> = match args.get("ids") {
            None | Some(Value::Null) => None,
            Some(value) => {
                let ids = value.as_array().ok_or(ApiError::InvalidArguments)?;
                let mut out = HashSet::with_capacity(ids.len());
                for id in ids {
                    let raw = id.as_str().ok_or(ApiError::InvalidArguments)?;
                    out.insert(raw.parse().map_err(|_| ApiError::InvalidArguments)?);
                }
                Some(out)
            }
        };
        let properties = properties(args)?;
        let counts: HashMap<i64, (i64, i64)> = self
            .store
            .mailbox_counts(account_id)?
            .into_iter()
            .map(|count| (count.mailbox_id, (count.total, count.unread)))
            .collect();

        let mut list = Vec::new();
        let mut found = HashSet::new();
        for mailbox in self.store.list_mailboxes(account_id)? {
            if mailbox.missing_at.is_some() {
                continue;
            }
            if let Some(wanted) = &wanted {
                if !wanted.contains(&mailbox.id) {
                    continue;
                }
            }
            found.insert(mailbox.id);
            let (total, unread) = counts.get(&mailbox.id).copied().unwrap_or((0, 0));
            let mut object = Map::new();
            object.insert("id".into(), json!(mailbox.id.to_string()));
            object.insert("name".into(), json!(mailbox.remote_name));
            object.insert("parentId".into(), Value::Null);
            object.insert("role".into(), mailbox_role(mailbox.role.as_deref()));
            object.insert("sortOrder".into(), json!(0));
            object.insert("totalEmails".into(), json!(total));
            object.insert("unreadEmails".into(), json!(unread));
            object.insert("totalThreads".into(), json!(total));
            object.insert("unreadThreads".into(), json!(unread));
            object.insert("isSubscribed".into(), json!(true));
            let can_write = self.writes.is_some();
            object.insert(
                "myRights".into(),
                json!({
                    "mayReadItems": true,
                    "mayAddItems": false,
                    "mayRemoveItems": can_write,
                    "maySetSeen": can_write,
                    "maySetKeywords": can_write,
                    "mayCreateChild": false,
                    "mayRename": false,
                    "mayDelete": false,
                    "maySubmit": false,
                }),
            );
            list.push(select_properties(object, &properties));
        }
        let not_found: Vec<String> = wanted
            .unwrap_or_default()
            .difference(&found)
            .map(|id| id.to_string())
            .collect();

        Ok(json!({
            "accountId": account_id.to_string(),
            "state": self.store.latest_change_seq_type(account_id, "mailbox")?.to_string(),
            "list": list,
            "notFound": not_found,
        }))
    }

    fn email_query(&self, args: &Map<String, Value>) -> Result<Value, ApiError> {
        let account_id = self.account_id(args)?;
        let filter = parse_filter(args.get("filter"))?;
        parse_sort(args.get("sort"))?;
        let position = args.get("position").and_then(Value::as_i64).unwrap_or(0);
        if position < 0 {
            return Err(ApiError::InvalidArguments);
        }
        let limit = match args.get("limit") {
            None | Some(Value::Null) => MAX_MESSAGES_PER_QUERY,
            Some(value) => value
                .as_i64()
                .ok_or(ApiError::InvalidArguments)?
                .clamp(1, MAX_MESSAGES_PER_QUERY),
        };
        let query = MessageQuery {
            mailbox_id: filter.mailbox,
            text: filter.text,
            seen: filter.seen,
            flagged: filter.flagged,
            before: filter.before,
            after: filter.after,
            limit,
            offset: position,
        };
        let page = self.store.query_messages(account_id, &query)?;
        let ids: Vec<String> = page
            .messages
            .iter()
            .map(|message| message.id.to_string())
            .collect();
        Ok(json!({
            "accountId": account_id.to_string(),
            "queryState": self.store.latest_change_seq_type(account_id, "message")?.to_string(),
            "canCalculateChanges": false,
            "position": position,
            "ids": ids,
            "total": page.total,
            "limit": limit,
        }))
    }

    async fn email_get(&self, args: &Map<String, Value>) -> Result<Value, ApiError> {
        let account_id = self.account_id(args)?;
        let ids = args
            .get("ids")
            .and_then(Value::as_array)
            .ok_or(ApiError::InvalidArguments)?;
        if ids.len() > MAX_OBJECTS_IN_GET {
            return Err(ApiError::InvalidArguments);
        }
        let mut numeric = Vec::with_capacity(ids.len());
        for id in ids {
            let raw = id.as_str().ok_or(ApiError::InvalidArguments)?;
            numeric.push(raw.parse::<i64>().map_err(|_| ApiError::InvalidArguments)?);
        }
        let properties = properties(args)?;
        let fetch_text = flag(args, "fetchTextBodyValues");
        let fetch_html = flag(args, "fetchHTMLBodyValues");
        let max_body_bytes = args.get("maxBodyValueBytes").and_then(Value::as_i64);

        let mut list = Vec::new();
        let mut found = HashSet::new();
        for message in self.store.messages_with_locations(account_id, &numeric)? {
            found.insert(message.id);
            let body = self
                .body_view(account_id, message.id, fetch_text || fetch_html)
                .await?;
            let object = email_object(&message, &body, fetch_text, fetch_html, max_body_bytes);
            list.push(select_properties(object, &properties));
        }
        let not_found: Vec<String> = numeric
            .iter()
            .filter(|id| !found.contains(id))
            .map(|id| id.to_string())
            .collect();

        Ok(json!({
            "accountId": account_id.to_string(),
            "state": self.store.latest_change_seq_type(account_id, "message")?.to_string(),
            "list": list,
            "notFound": not_found,
        }))
    }

    /// `Email/changes` and `Mailbox/changes` share cursor rules; only the
    /// entity type and operation names differ.
    fn changes(&self, args: &Map<String, Value>, entity_type: &str) -> Result<Value, ApiError> {
        let account_id = self.account_id(args)?;
        let since: i64 = args
            .get("sinceState")
            .and_then(Value::as_str)
            .ok_or(ApiError::InvalidArguments)?
            .parse()
            .map_err(|_| ApiError::InvalidArguments)?;
        let limit = match args.get("maxChanges") {
            None | Some(Value::Null) => MAX_CHANGES,
            Some(value) => value
                .as_i64()
                .ok_or(ApiError::InvalidArguments)?
                .clamp(1, MAX_CHANGES),
        };

        let window = self.store.change_window(account_id)?;
        let snapshot_covers = window.snapshot_seq.is_some_and(|seq| seq > since);
        let history_dropped = window.oldest_seq.is_some_and(|oldest| since < oldest - 1);
        if since > window.latest_seq || snapshot_covers || history_dropped {
            return Err(ApiError::CannotCalculateChanges);
        }

        let rows = self
            .store
            .changes_since_type(account_id, entity_type, since, limit + 1)?;
        let has_more = rows.len() as i64 > limit;
        let rows = &rows[..rows.len().min(limit as usize)];

        #[derive(Clone, Copy, PartialEq, Eq)]
        enum Last {
            Created,
            Updated,
            Removed,
        }
        let mut order: Vec<String> = Vec::new();
        let mut last: HashMap<String, Last> = HashMap::new();
        let mut created_since: HashSet<String> = HashSet::new();
        for row in rows {
            let op = match (entity_type, row.op.as_str()) {
                ("message", "created") => Last::Created,
                ("message", "removed") => Last::Removed,
                // `upsert` is what builds written before the created/updated
                // split recorded; treat it as an update.
                ("message", "updated" | "upsert") => Last::Updated,
                ("mailbox", "created") => Last::Created,
                ("mailbox", "removed" | "retired") => Last::Removed,
                ("mailbox", "updated" | "restored") => Last::Updated,
                ("thread", "created") => Last::Created,
                ("thread", "removed") => Last::Removed,
                ("thread", "updated") => Last::Updated,
                _ => continue,
            };
            if op == Last::Created {
                created_since.insert(row.entity_id.clone());
            }
            if last.insert(row.entity_id.clone(), op).is_none() {
                order.push(row.entity_id.clone());
            }
        }

        let mut created = Vec::new();
        let mut updated = Vec::new();
        let mut destroyed = Vec::new();
        for id in order {
            let op = last.get(&id).copied();
            let was_created = created_since.contains(&id);
            match op {
                Some(Last::Created) => created.push(id),
                // An object created after `sinceState` is reported as created
                // even if it was modified again in the same window.
                Some(Last::Updated) if was_created => created.push(id),
                Some(Last::Updated) => updated.push(id),
                // Created and destroyed inside the window: the client never
                // saw it, so it belongs to neither list.
                Some(Last::Removed) if !was_created => destroyed.push(id),
                _ => {}
            }
        }

        let new_state = rows
            .last()
            .map(|row| row.seq.to_string())
            .unwrap_or_else(|| since.to_string());
        Ok(json!({
            "accountId": account_id.to_string(),
            "oldState": since.to_string(),
            "newState": new_state,
            "hasMoreChanges": has_more,
            "created": created,
            "updated": updated,
            "destroyed": destroyed,
        }))
    }

    /// `Email/set` creates emails from uploaded blobs, destroys them (JMAP
    /// destroy is permanent; deleting to Trash is a `mailboxIds` update), and
    /// applies keyword/mailbox updates. Per-object failures stay in the
    /// `not*` maps instead of failing the whole method.
    async fn email_set(&self, args: &Map<String, Value>) -> Result<Value, ApiError> {
        let account_id = self.account_id(args)?;
        let old_state = self
            .store
            .latest_change_seq_type(account_id, "message")?
            .to_string();
        if let Some(value) = args.get("ifInState") {
            if !value.is_null() && value.as_str() != Some(old_state.as_str()) {
                return Err(ApiError::StateMismatch);
            }
        }

        let create = optional_object(args, "create")?;
        let destroy = optional_array(args, "destroy")?;
        let update = optional_object(args, "update")?;
        let total =
            create.map_or(0, Map::len) + destroy.map_or(0, Vec::len) + update.map_or(0, Map::len);
        if total > MAX_OBJECTS_IN_SET {
            return Err(ApiError::InvalidArguments);
        }

        let mut created = Map::new();
        let mut updated = Map::new();
        let mut not_created = Map::new();
        let mut not_updated = Map::new();
        let mut not_destroyed = Map::new();
        let mut destroyed = Vec::new();

        if let Some(create) = create {
            let mut order: Vec<&String> = create.keys().collect();
            order.sort();
            for creation_id in order {
                let Some(object) = create[creation_id.as_str()].as_object() else {
                    not_created.insert(
                        creation_id.clone(),
                        json!({
                            "type": "invalidProperties",
                            "description": "a creation must be an object",
                        }),
                    );
                    continue;
                };
                let Some(sink) = self.writes.as_ref() else {
                    not_created.insert(
                        creation_id.clone(),
                        json!({"type": "notSupported", "description": "writing is disabled"}),
                    );
                    continue;
                };
                match self.import_blob(account_id, sink, object).await {
                    Ok(imported) => match self.email_view(account_id, imported.message_id).await {
                        Ok(object) => {
                            created.insert(creation_id.clone(), Value::Object(object));
                        }
                        Err(err) => {
                            not_created.insert(creation_id.clone(), write_error_object(&err));
                        }
                    },
                    Err(err) => {
                        not_created.insert(creation_id.clone(), err);
                    }
                }
            }
        }

        if let Some(destroy) = destroy {
            for value in destroy {
                let id = value.as_str().ok_or(ApiError::InvalidArguments)?;
                let message_id: i64 = id.parse().map_err(|_| ApiError::InvalidArguments)?;
                let Some(sink) = self.writes.as_ref() else {
                    not_destroyed.insert(
                        id.to_string(),
                        json!({"type": "notSupported", "description": "writing is disabled"}),
                    );
                    continue;
                };
                match sink.destroy_message(account_id, message_id).await {
                    Ok(()) => destroyed.push(id.to_string()),
                    Err(err) => {
                        not_destroyed.insert(id.to_string(), write_error_object(&err));
                    }
                }
            }
        }

        if let Some(update) = update {
            for (raw_id, patch) in update {
                let message_id: i64 = raw_id.parse().map_err(|_| ApiError::InvalidArguments)?;
                let patch = patch.as_object().ok_or(ApiError::InvalidArguments)?;
                match self.email_update(account_id, message_id, patch).await {
                    Ok(()) => {
                        updated.insert(raw_id.clone(), json!({}));
                    }
                    Err(err) => {
                        not_updated.insert(raw_id.clone(), write_error_object(&err));
                    }
                }
            }
        }

        Ok(json!({
            "accountId": account_id.to_string(),
            "oldState": old_state,
            "newState": self
                .store
                .latest_change_seq_type(account_id, "message")?
                .to_string(),
            "created": created,
            "updated": updated,
            "destroyed": destroyed,
            "notCreated": not_created,
            "notUpdated": not_updated,
            "notDestroyed": not_destroyed,
        }))
    }

    /// Full JMAP Email object for a message the store already holds. Used as
    /// the `created` value of `Email/set`, where the spec requires the object
    /// rather than the short `Email/import` response.
    async fn email_view(
        &self,
        account_id: i64,
        message_id: i64,
    ) -> Result<Map<String, Value>, WriteError> {
        let mut messages = self
            .store
            .messages_with_locations(account_id, &[message_id])
            .map_err(|err| WriteError::Server(err.to_string()))?;
        let message = messages
            .pop()
            .ok_or_else(|| WriteError::Server(format!("message {message_id} vanished")))?;
        let body = self
            .body_view(account_id, message_id, false)
            .await
            .map_err(|err| WriteError::Server(err.to_string()))?;
        Ok(email_object(&message, &body, false, false, None))
    }

    /// Validates and files one uploaded blob; shared by `Email/import` and
    /// `Email/set create`. Errors come back as ready-to-send JMAP error
    /// objects so both methods keep the same per-object types.
    async fn import_blob(
        &self,
        account_id: i64,
        sink: &Arc<dyn WriteSink>,
        object: &Map<String, Value>,
    ) -> Result<ImportedEmail, Value> {
        let blob_id = match object.get("blobId").and_then(Value::as_str) {
            Some(blob_id) => blob_id.to_string(),
            None => {
                return Err(
                    json!({"type": "invalidProperties", "description": "blobId is required"}),
                )
            }
        };
        let upload = match self.store.upload_for_account(account_id, &blob_id) {
            Ok(Some(upload)) => upload,
            Ok(None) => {
                return Err(json!({
                    "type": "blobNotFound",
                    "description": format!("blob {blob_id} is not an upload of this account"),
                }))
            }
            Err(err) => return Err(json!({"type": "serverFail", "description": err.to_string()})),
        };
        if upload.size > MAX_UPLOAD_BYTES {
            return Err(json!({"type": "tooLarge", "description": "upload exceeds maxSizeUpload"}));
        }
        let mailbox_id =
            import_mailbox(object.get("mailboxIds")).map_err(|err| write_error_object(&err))?;
        let keywords =
            import_keywords(object.get("keywords")).map_err(|err| write_error_object(&err))?;
        let message_id = sink
            .import_message(account_id, mailbox_id, &upload.bytes, keywords)
            .await
            .map_err(|err| write_error_object(&err))?;
        // Keep the bytes the client uploaded so EmailSubmission can deliver
        // them later. Synced messages have no raw copy.
        self.store
            .attach_raw_blob(account_id, message_id, &upload.bytes)
            .map_err(|err| json!({"type": "serverFail", "description": err.to_string()}))?;
        Ok(ImportedEmail {
            message_id,
            blob_id,
            size: upload.size,
        })
    }

    /// `Email/import` files bytes a client uploaded earlier. The server round
    /// trip lives in the sink; per-object failures stay in `notCreated`.
    async fn email_import(&self, args: &Map<String, Value>) -> Result<Value, ApiError> {
        let account_id = self.account_id(args)?;
        let old_state = self
            .store
            .latest_change_seq_type(account_id, "message")?
            .to_string();
        if let Some(value) = args.get("ifInState") {
            if !value.is_null() && value.as_str() != Some(old_state.as_str()) {
                return Err(ApiError::StateMismatch);
            }
        }
        let emails = optional_object(args, "emails")?.ok_or(ApiError::InvalidArguments)?;
        if emails.len() > MAX_OBJECTS_IN_SET {
            return Err(ApiError::InvalidArguments);
        }

        let mut created = Map::new();
        let mut not_created = Map::new();
        let sink = match self.writes.as_ref() {
            Some(sink) => sink,
            None => {
                for creation_id in emails.keys() {
                    not_created.insert(
                        creation_id.clone(),
                        json!({"type": "notSupported", "description": "writing is disabled"}),
                    );
                }
                return Ok(json!({
                    "accountId": account_id.to_string(),
                    "oldState": old_state,
                    "newState": old_state,
                    "created": created,
                    "notCreated": not_created,
                }));
            }
        };

        let mut order: Vec<&String> = emails.keys().collect();
        order.sort();
        for creation_id in order {
            let Some(object) = emails[creation_id.as_str()].as_object() else {
                not_created.insert(
                    creation_id.clone(),
                    json!({
                        "type": "invalidProperties",
                        "description": "an import must be an object",
                    }),
                );
                continue;
            };
            match self.import_blob(account_id, sink, object).await {
                Ok(imported) => {
                    created.insert(
                        creation_id.clone(),
                        json!({
                            "id": imported.message_id.to_string(),
                            "blobId": imported.blob_id,
                            "threadId": self.thread_id_of(account_id, imported.message_id)?,
                            "size": imported.size,
                        }),
                    );
                }
                Err(err) => {
                    not_created.insert(creation_id.clone(), err);
                }
            }
        }

        Ok(json!({
            "accountId": account_id.to_string(),
            "oldState": old_state,
            "newState": self
                .store
                .latest_change_seq_type(account_id, "message")?
                .to_string(),
            "created": created,
            "notCreated": not_created,
        }))
    }

    /// `Identity/get`. The Hub exposes one sending identity per account: the
    /// account's own address.
    fn identity_get(&self, args: &Map<String, Value>) -> Result<Value, ApiError> {
        let account_id = self.account_id(args)?;
        let account = self
            .store
            .account_by_id(account_id)?
            .ok_or_else(|| ApiError::UnknownAccount(account_id.to_string()))?;
        let identity = identity_id(account_id);
        let mut not_found = Vec::new();
        if let Some(ids) = args.get("ids") {
            let ids = ids.as_array().ok_or(ApiError::InvalidArguments)?;
            let mut requested = Vec::with_capacity(ids.len());
            for id in ids {
                requested.push(id.as_str().ok_or(ApiError::InvalidArguments)?.to_string());
            }
            if !requested.iter().any(|id| id == &identity) {
                not_found = requested;
                return Ok(json!({
                    "accountId": account_id.to_string(),
                    "state": identity,
                    "list": [],
                    "notFound": not_found,
                }));
            }
        }
        let object = json!({
            "id": identity,
            "name": account.display_name.clone().unwrap_or_else(|| account.email.clone()),
            "email": account.email,
            "replyTo": Value::Null,
            "bcc": Value::Null,
            "textSignature": "",
            "htmlSignature": "",
            "mayDelete": false,
        });
        Ok(json!({
            "accountId": account_id.to_string(),
            "state": identity,
            "list": [object],
            "notFound": not_found,
        }))
    }

    /// `EmailSubmission/get`. Submissions are recorded when accepted; there is
    /// no change log for them yet.
    fn email_submission_get(&self, args: &Map<String, Value>) -> Result<Value, ApiError> {
        let account_id = self.account_id(args)?;
        let ids: Vec<i64> = match args.get("ids") {
            None | Some(Value::Null) => Vec::new(),
            Some(value) => {
                let values = value.as_array().ok_or(ApiError::InvalidArguments)?;
                if values.len() > MAX_OBJECTS_IN_GET {
                    return Err(ApiError::InvalidArguments);
                }
                let mut out = Vec::with_capacity(values.len());
                for value in values {
                    out.push(
                        value
                            .as_str()
                            .ok_or(ApiError::InvalidArguments)?
                            .parse()
                            .map_err(|_| ApiError::InvalidArguments)?,
                    );
                }
                out
            }
        };
        let rows = self
            .store
            .submissions(account_id, &ids, MAX_OBJECTS_IN_GET as i64)?;
        let mut found = HashSet::new();
        let mut list = Vec::with_capacity(rows.len());
        for row in &rows {
            found.insert(row.id);
            list.push(self.submission_object(account_id, row)?);
        }
        let not_found: Vec<String> = ids
            .iter()
            .filter(|id| !found.contains(id))
            .map(|id| id.to_string())
            .collect();
        Ok(json!({
            "accountId": account_id.to_string(),
            "state": self.submission_state(account_id)?,
            "list": list,
            "notFound": not_found,
        }))
    }

    /// `EmailSubmission/set` (RFC 8621 §7.5), create only. Sending is
    /// synchronous: `undoStatus` is `final` and `sendAt` in the future is
    /// rejected because delayed send is not offered (`maxDelayedSend` is 0).
    /// `onSuccessUpdateEmail` becomes the implicit `Email/set` response.
    async fn email_submission_set(
        &self,
        args: &Map<String, Value>,
    ) -> Result<Vec<(&'static str, Value)>, ApiError> {
        let account_id = self.account_id(args)?;
        let old_state = self.submission_state(account_id)?;
        let create = optional_object(args, "create")?;
        let update = optional_object(args, "update")?;
        let destroy = optional_array(args, "destroy")?;
        let total =
            create.map_or(0, Map::len) + update.map_or(0, Map::len) + destroy.map_or(0, Vec::len);
        if total > MAX_OBJECTS_IN_SET {
            return Err(ApiError::InvalidArguments);
        }

        let mut created = Map::new();
        let mut not_created = Map::new();
        let mut not_updated = Map::new();
        let mut not_destroyed = Map::new();
        for (key, _) in update.into_iter().flatten() {
            not_updated.insert(
                key.clone(),
                json!({"type": "notSupported", "description": "updating a submission is not supported"}),
            );
        }
        for key in destroy.into_iter().flatten() {
            let id = key.as_str().ok_or(ApiError::InvalidArguments)?;
            not_destroyed.insert(
                id.to_string(),
                json!({"type": "notSupported", "description": "destroying a submission is not supported"}),
            );
        }

        // creation id -> email id, used by `onSuccessUpdateEmail`.
        let mut submission_emails: HashMap<String, i64> = HashMap::new();
        if let Some(create) = create {
            let sink = match self.submissions.as_ref() {
                Some(sink) => sink,
                None => {
                    for creation_id in create.keys() {
                        not_created.insert(
                            creation_id.clone(),
                            json!({"type": "notSupported", "description": "submitting is disabled"}),
                        );
                    }
                    return Ok(vec![(
                        "EmailSubmission/set",
                        submission_set_response(
                            account_id,
                            &old_state,
                            &self.submission_state(account_id)?,
                            created,
                            not_created,
                            not_updated,
                            not_destroyed,
                        ),
                    )]);
                }
            };
            let mut order: Vec<&String> = create.keys().collect();
            order.sort();
            for creation_id in order {
                let Some(object) = create[creation_id.as_str()].as_object() else {
                    not_created.insert(
                        creation_id.clone(),
                        json!({"type": "invalidProperties", "description": "a creation must be an object"}),
                    );
                    continue;
                };
                match self.submit_one(account_id, sink, creation_id, object).await {
                    Ok((value, email_id)) => {
                        submission_emails.insert(creation_id.clone(), email_id);
                        created.insert(creation_id.clone(), value);
                    }
                    Err(err) => {
                        not_created.insert(creation_id.clone(), err);
                    }
                }
            }
        }

        let new_state = self.submission_state(account_id)?;
        let mut responses = vec![(
            "EmailSubmission/set",
            submission_set_response(
                account_id,
                &old_state,
                &new_state,
                created,
                not_created,
                not_updated,
                not_destroyed,
            ),
        )];

        // The implicit Email/set must come after the submission response and
        // share its method call id.
        if let Some(patches) = args.get("onSuccessUpdateEmail") {
            if !patches.is_null() {
                let patches = patches.as_object().ok_or(ApiError::InvalidArguments)?;
                let mut update = Map::new();
                for (key, patch) in patches {
                    let email_id = match key.strip_prefix('#') {
                        Some(creation_id) => submission_emails.get(creation_id).copied(),
                        None => None,
                    };
                    let email_id = match email_id {
                        Some(id) => Some(id),
                        None => key.parse::<i64>().ok().and_then(|submission_id| {
                            self.store
                                .submissions(account_id, &[submission_id], 1)
                                .ok()
                                .and_then(|rows| rows.first().map(|row| row.email_id))
                        }),
                    };
                    if let Some(email_id) = email_id {
                        update.insert(email_id.to_string(), patch.clone());
                    }
                }
                if !update.is_empty() {
                    let mut set_args = Map::new();
                    set_args.insert("accountId".into(), json!(account_id.to_string()));
                    set_args.insert("update".into(), Value::Object(update));
                    let value = self.email_set(&set_args).await?;
                    responses.push(("Email/set", value));
                }
            }
        }

        Ok(responses)
    }

    async fn submit_one(
        &self,
        account_id: i64,
        sink: &Arc<dyn SubmissionSink>,
        _creation_id: &str,
        object: &Map<String, Value>,
    ) -> Result<(Value, i64), Value> {
        let email_id = match object.get("emailId").and_then(Value::as_str) {
            Some(raw) => raw
                .parse::<i64>()
                .map_err(|_| json!({"type": "invalidProperties", "description": "emailId must be an email id"}))?,
            None => {
                return Err(
                    json!({"type": "invalidProperties", "description": "emailId is required"}),
                )
            }
        };
        if self
            .store
            .messages_with_locations(account_id, &[email_id])
            .map_err(|err| json!({"type": "serverFail", "description": err.to_string()}))?
            .is_empty()
        {
            return Err(
                json!({"type": "invalidProperties", "description": "emailId is not in this account"}),
            );
        }
        let expected_identity = identity_id(account_id);
        if object.get("identityId").and_then(Value::as_str) != Some(expected_identity.as_str()) {
            return Err(json!({
                "type": "invalidProperties",
                "description": "unknown identityId",
                "properties": ["identityId"],
            }));
        }
        if let Some(send_at) = object.get("sendAt") {
            if !send_at.is_null() {
                return Err(json!({
                    "type": "invalidProperties",
                    "description": "delayed send is not supported",
                    "properties": ["sendAt"],
                }));
            }
        }
        let envelope = match object.get("envelope") {
            None | Some(Value::Null) => None,
            Some(value) => Some(parse_envelope(value)?),
        };
        let raw = match self.store.raw_blob(account_id, email_id) {
            Ok(Some(raw)) => raw,
            Ok(None) => {
                return Err(json!({
                    "type": "invalidEmail",
                    "description": "this message has no stored raw copy to send",
                }))
            }
            Err(err) => return Err(json!({"type": "serverFail", "description": err.to_string()})),
        };
        let outcome = sink
            .submit(account_id, raw, envelope)
            .await
            .map_err(|err| write_error_object(&err))?;
        let send_at = Utc::now().timestamp();
        let envelope_json = envelope_json(&outcome.envelope);
        let submission_id = self
            .store
            .create_submission(
                account_id,
                email_id,
                &expected_identity,
                &envelope_json.to_string(),
                send_at,
            )
            .map_err(|err| json!({"type": "serverFail", "description": err.to_string()}))?;
        let row = EmailSubmission {
            id: submission_id,
            email_id,
            identity_id: expected_identity,
            envelope: envelope_json.to_string(),
            send_at,
            undo_status: "final".to_string(),
        };
        let mut value = self
            .submission_object(account_id, &row)
            .map_err(|err| json!({"type": "serverFail", "description": err.to_string()}))?;
        if let Some(object) = value.as_object_mut() {
            object.insert("savedTo".into(), json!(outcome.saved_to));
            object.insert("saveError".into(), json!(outcome.save_error));
        }
        Ok((value, email_id))
    }

    fn submission_object(&self, account_id: i64, row: &EmailSubmission) -> Result<Value, ApiError> {
        let envelope: Value = serde_json::from_str(&row.envelope).unwrap_or(Value::Null);
        let mut delivery = Map::new();
        if let Some(recipients) = envelope.get("rcptTo").and_then(Value::as_array) {
            for recipient in recipients {
                if let Some(email) = recipient.get("email").and_then(Value::as_str) {
                    delivery.insert(email.to_string(), json!({"delivered": "yes"}));
                }
            }
        }
        Ok(json!({
            "id": row.id.to_string(),
            "identityId": row.identity_id,
            "emailId": row.email_id.to_string(),
            "threadId": self.thread_id_of(account_id, row.email_id)?,
            "envelope": envelope,
            "sendAt": utc_date(Some(row.send_at)),
            "undoStatus": row.undo_status,
            "deliveryStatus": Value::Object(delivery),
        }))
    }

    fn submission_state(&self, account_id: i64) -> Result<String, ApiError> {
        let newest = self
            .store
            .submissions(account_id, &[], 1)?
            .first()
            .map(|row| row.id)
            .unwrap_or(0);
        Ok(format!("e{newest}"))
    }

    /// `Thread/get` over the threads the store assigned while syncing.
    fn thread_get(&self, args: &Map<String, Value>) -> Result<Value, ApiError> {
        let account_id = self.account_id(args)?;
        let (threads, not_found) = match args.get("ids") {
            None | Some(Value::Null) => (
                self.store
                    .list_threads(account_id, MAX_OBJECTS_IN_GET as i64)?,
                Vec::new(),
            ),
            Some(Value::Array(ids)) => {
                if ids.len() > MAX_OBJECTS_IN_GET {
                    return Err(ApiError::InvalidArguments);
                }
                let mut wanted = Vec::with_capacity(ids.len());
                for id in ids {
                    wanted.push(id.as_str().ok_or(ApiError::InvalidArguments)?.to_string());
                }
                let found = self.store.threads(account_id, &wanted)?;
                let seen: HashSet<&str> = found.iter().map(|thread| thread.id.as_str()).collect();
                let not_found = wanted
                    .iter()
                    .filter(|id| !seen.contains(id.as_str()))
                    .cloned()
                    .collect();
                (found, not_found)
            }
            Some(_) => return Err(ApiError::InvalidArguments),
        };
        let list: Vec<Value> = threads
            .iter()
            .map(|thread| {
                json!({
                    "id": thread.id,
                    "emailIds": thread
                        .email_ids
                        .iter()
                        .map(|id| id.to_string())
                        .collect::<Vec<_>>(),
                })
            })
            .collect();
        Ok(json!({
            "accountId": account_id.to_string(),
            "state": self.store.latest_change_seq_type(account_id, "thread")?.to_string(),
            "list": list,
            "notFound": not_found,
        }))
    }

    fn thread_id_of(&self, account_id: i64, message_id: i64) -> Result<Value, ApiError> {
        let found = self
            .store
            .messages_with_locations(account_id, &[message_id])?;
        Ok(found
            .first()
            .map(|message| {
                json!(message
                    .thread_id
                    .clone()
                    .unwrap_or_else(|| format!("t{}", message.id)))
            })
            .unwrap_or(Value::Null))
    }

    async fn email_update(
        &self,
        account_id: i64,
        message_id: i64,
        patch: &Map<String, Value>,
    ) -> Result<(), WriteError> {
        let existing = self
            .store
            .messages_with_locations(account_id, &[message_id])
            .map_err(|err| WriteError::Server(err.to_string()))?;
        if existing.is_empty() {
            return Err(WriteError::NotFound(format!(
                "email {message_id} not found"
            )));
        }

        let mut add = Vec::new();
        let mut remove = Vec::new();
        let mut destination = None;
        // `mailboxIds/<id>: null` entries. A single removal paired with one
        // addition is JMAP's move; the IMAP backend satisfies it with MOVE.
        let mut remove_mailboxes = Vec::new();
        for (key, value) in patch {
            match key.as_str() {
                "keywords" => match value {
                    Value::Null => remove.extend(SUPPORTED_KEYWORDS.map(str::to_string)),
                    Value::Object(map) if map.is_empty() => {
                        remove.extend(SUPPORTED_KEYWORDS.map(str::to_string));
                    }
                    Value::Object(map) => {
                        for (keyword, present) in map {
                            match present {
                                Value::Bool(true) => add.push(keyword.clone()),
                                Value::Bool(false) | Value::Null => remove.push(keyword.clone()),
                                _ => {
                                    return Err(WriteError::NotSupported(
                                        "keyword values must be booleans".to_string(),
                                    ))
                                }
                            }
                        }
                    }
                    _ => {
                        return Err(WriteError::NotSupported(
                            "keywords must be an object or null".to_string(),
                        ))
                    }
                },
                "mailboxIds" => {
                    let map = value.as_object().ok_or_else(|| {
                        WriteError::NotSupported("mailboxIds must be an object".to_string())
                    })?;
                    let mut ids = Vec::new();
                    for (raw, present) in map {
                        if present.as_bool() == Some(true) {
                            ids.push(raw.parse::<i64>().map_err(|_| {
                                WriteError::NotSupported(format!("bad mailbox id {raw}"))
                            })?);
                        }
                    }
                    if ids.len() != 1 {
                        return Err(WriteError::NotSupported(
                            "mailboxIds must name exactly one mailbox".to_string(),
                        ));
                    }
                    destination = Some(ids[0]);
                }
                other if other.starts_with("keywords/") => {
                    let keyword = &other["keywords/".len()..];
                    match value {
                        Value::Bool(true) => add.push(keyword.to_string()),
                        Value::Bool(false) | Value::Null => remove.push(keyword.to_string()),
                        _ => {
                            return Err(WriteError::NotSupported(format!(
                                "{other} must be a boolean"
                            )))
                        }
                    }
                }
                other if other.starts_with("mailboxIds/") => {
                    let raw = &other["mailboxIds/".len()..];
                    let mailbox_id: i64 = raw
                        .parse()
                        .map_err(|_| WriteError::NotSupported(format!("bad mailbox id {raw}")))?;
                    match value {
                        Value::Bool(true) => destination = Some(mailbox_id),
                        Value::Bool(false) | Value::Null => remove_mailboxes.push(mailbox_id),
                        _ => {
                            return Err(WriteError::NotSupported(format!(
                                "{other} must be a boolean"
                            )))
                        }
                    }
                }
                other => {
                    return Err(WriteError::NotSupported(format!(
                        "property {other} is not writable"
                    )))
                }
            }
        }

        if !remove_mailboxes.is_empty() {
            let Some(target) = destination else {
                return Err(WriteError::NotSupported(
                    "removing a mailbox copy without a destination is not supported; move the message instead"
                        .to_string(),
                ));
            };
            if remove_mailboxes.len() > 1 {
                return Err(WriteError::NotSupported(
                    "a move can remove the message from one mailbox only".to_string(),
                ));
            }
            if remove_mailboxes.contains(&target) {
                return Err(WriteError::NotSupported(
                    "the patch both moves to and removes from the same mailbox".to_string(),
                ));
            }
        }

        // A patch may name the same keyword through `keywords` and
        // `keywords/<name>`; removal wins and duplicates are dropped.
        remove.sort();
        remove.dedup();
        add.retain(|keyword| !remove.contains(keyword));
        add.sort();
        add.dedup();

        for keyword in add.iter().chain(remove.iter()) {
            if !SUPPORTED_KEYWORDS.contains(&keyword.as_str()) {
                return Err(WriteError::NotSupported(format!(
                    "keyword {keyword} is not supported"
                )));
            }
        }
        if add.is_empty() && remove.is_empty() && destination.is_none() {
            return Ok(());
        }

        let sink = self
            .writes
            .as_ref()
            .ok_or_else(|| WriteError::NotSupported("writing is disabled".to_string()))?;
        if !add.is_empty() || !remove.is_empty() {
            sink.set_keywords(account_id, message_id, add, remove)
                .await?;
        }
        if let Some(mailbox_id) = destination {
            sink.move_message(account_id, message_id, mailbox_id)
                .await?;
        }
        Ok(())
    }

    /// Store body first, then fetch from the server only when the client asked
    /// for values that are not cached yet.
    async fn body_view(
        &self,
        account_id: i64,
        message_id: i64,
        want_values: bool,
    ) -> Result<BodyView, ApiError> {
        let stored = self.store.message_body(message_id)?;
        let mut view = stored.map(BodyView::from).unwrap_or_default();
        if !want_values {
            return Ok(view);
        }
        let has_html_part = view.parts.iter().any(|part| part.is_html);
        let need_text = view.text.is_none();
        let need_html = view.html.is_none() && (view.parts.is_empty() || has_html_part);
        if !(need_text || need_html) {
            return Ok(view);
        }
        let Some(source) = &self.bodies else {
            return Ok(view);
        };
        let fetched = source
            .fetch(account_id, message_id)
            .await
            .map_err(ApiError::BodyFetch)?;
        // Fetching usually writes the decoded parts to the store; prefer those
        // so a repeated call does not produce duplicate part ids.
        match self.store.message_body(message_id)? {
            Some(stored) => {
                view = BodyView::from(stored);
                view.text = view.text.or(fetched.text);
                view.html = view.html.or(fetched.html);
            }
            None => {
                view.text = view.text.or(fetched.text);
                view.html = view.html.or(fetched.html);
            }
        }
        Ok(view)
    }
}

#[derive(Default)]
struct BodyView {
    text: Option<String>,
    html: Option<String>,
    parts: Vec<StoredPart>,
    attachments: Vec<StoredAttachment>,
}

impl From<MessageBody> for BodyView {
    fn from(body: MessageBody) -> Self {
        Self {
            text: body.text,
            html: body.html,
            parts: body.parts,
            attachments: body.attachments,
        }
    }
}

#[derive(Default)]
struct Filter {
    mailbox: Option<i64>,
    text: Option<String>,
    seen: Option<bool>,
    flagged: Option<bool>,
    before: Option<i64>,
    after: Option<i64>,
}

fn parse_filter(filter: Option<&Value>) -> Result<Filter, ApiError> {
    let mut out = Filter::default();
    let filter = match filter {
        None | Some(Value::Null) => return Ok(out),
        Some(value) => value,
    };
    let object = filter.as_object().ok_or(ApiError::UnsupportedFilter)?;
    for (key, value) in object {
        match key.as_str() {
            "inMailbox" => {
                let raw = value.as_str().ok_or(ApiError::InvalidArguments)?;
                out.mailbox = Some(raw.parse().map_err(|_| ApiError::InvalidArguments)?);
            }
            "text" => {
                out.text = Some(
                    value
                        .as_str()
                        .ok_or(ApiError::InvalidArguments)?
                        .to_string(),
                );
            }
            "hasKeyword" | "notKeyword" => {
                let keyword = value.as_str().ok_or(ApiError::InvalidArguments)?;
                let present = key == "hasKeyword";
                match keyword {
                    "$seen" => out.seen = Some(present),
                    "$flagged" => out.flagged = Some(present),
                    _ => return Err(ApiError::UnsupportedFilter),
                }
            }
            "before" => out.before = Some(parse_date(value)?),
            "after" => out.after = Some(parse_date(value)?),
            _ => return Err(ApiError::UnsupportedFilter),
        }
    }
    Ok(out)
}

/// The store always orders by `receivedAt` (which maps to `sent_at`) and id,
/// both descending; anything else is rejected instead of silently reordered.
fn parse_sort(sort: Option<&Value>) -> Result<(), ApiError> {
    let sort = match sort {
        None | Some(Value::Null) => return Ok(()),
        Some(value) => value,
    };
    let comparators = sort.as_array().ok_or(ApiError::UnsupportedSort)?;
    for comparator in comparators {
        let object = comparator.as_object().ok_or(ApiError::UnsupportedSort)?;
        let property = object
            .get("property")
            .and_then(Value::as_str)
            .ok_or(ApiError::UnsupportedSort)?;
        let ascending = object
            .get("isAscending")
            .and_then(Value::as_bool)
            .unwrap_or(true);
        if !matches!(property, "receivedAt" | "sentAt" | "id") || ascending {
            return Err(ApiError::UnsupportedSort);
        }
    }
    Ok(())
}

fn parse_date(value: &Value) -> Result<i64, ApiError> {
    let raw = value.as_str().ok_or(ApiError::InvalidArguments)?;
    DateTime::parse_from_rfc3339(raw)
        .map(|date| date.timestamp())
        .map_err(|_| ApiError::InvalidArguments)
}

fn flag(args: &Map<String, Value>, name: &str) -> bool {
    args.get(name).and_then(Value::as_bool).unwrap_or(false)
}

fn optional_object<'a>(
    args: &'a Map<String, Value>,
    name: &str,
) -> Result<Option<&'a Map<String, Value>>, ApiError> {
    match args.get(name) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Object(map)) => Ok(Some(map)),
        _ => Err(ApiError::InvalidArguments),
    }
}

fn optional_array<'a>(
    args: &'a Map<String, Value>,
    name: &str,
) -> Result<Option<&'a Vec<Value>>, ApiError> {
    match args.get(name) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Array(values)) => Ok(Some(values)),
        _ => Err(ApiError::InvalidArguments),
    }
}

/// `mailboxIds` of one import: exactly one live mailbox, which is all the
/// store can move a message into today.
fn import_mailbox(value: Option<&Value>) -> Result<i64, WriteError> {
    let map = value
        .and_then(Value::as_object)
        .ok_or_else(|| WriteError::NotSupported("mailboxIds must be an object".to_string()))?;
    let mut ids = Vec::new();
    for (raw, present) in map {
        if present.as_bool() == Some(true) {
            ids.push(
                raw.parse::<i64>()
                    .map_err(|_| WriteError::NotSupported(format!("bad mailbox id {raw}")))?,
            );
        }
    }
    match ids.as_slice() {
        [id] => Ok(*id),
        [] => Err(WriteError::NotSupported(
            "mailboxIds must name one mailbox".to_string(),
        )),
        _ => Err(WriteError::NotSupported(
            "importing into several mailboxes is not supported".to_string(),
        )),
    }
}

fn import_keywords(value: Option<&Value>) -> Result<Vec<String>, WriteError> {
    let Some(value) = value else {
        return Ok(Vec::new());
    };
    let map = value
        .as_object()
        .ok_or_else(|| WriteError::NotSupported("keywords must be an object".to_string()))?;
    let mut out = Vec::new();
    for (keyword, present) in map {
        match present.as_bool() {
            Some(true) => {
                if !SUPPORTED_KEYWORDS.contains(&keyword.as_str()) {
                    return Err(WriteError::NotSupported(format!(
                        "keyword {keyword} is not supported"
                    )));
                }
                out.push(keyword.clone());
            }
            Some(false) => {}
            None => {
                return Err(WriteError::NotSupported(
                    "keyword values must be booleans".to_string(),
                ))
            }
        }
    }
    out.sort();
    Ok(out)
}

fn write_error_object(err: &WriteError) -> Value {
    match err {
        WriteError::NotFound(detail) => {
            json!({"type": "notFound", "description": detail})
        }
        WriteError::NotSupported(detail) => {
            json!({"type": "notSupported", "description": detail})
        }
        WriteError::Server(detail) => {
            json!({"type": "serverFail", "description": detail})
        }
    }
}

/// Identity id of the single sending identity the Hub exposes per account.
fn identity_id(account_id: i64) -> String {
    format!("i{account_id}")
}

fn submission_set_response(
    account_id: i64,
    old_state: &str,
    new_state: &str,
    created: Map<String, Value>,
    not_created: Map<String, Value>,
    not_updated: Map<String, Value>,
    not_destroyed: Map<String, Value>,
) -> Value {
    json!({
        "accountId": account_id.to_string(),
        "oldState": old_state,
        "newState": new_state,
        "created": created,
        "notCreated": not_created,
        "updated": {},
        "notUpdated": not_updated,
        "destroyed": [],
        "notDestroyed": not_destroyed,
    })
}

/// The RFC 8621 §7.1 envelope object, as echoed in responses.
fn envelope_json(envelope: &SubmissionEnvelope) -> Value {
    json!({
        "mailFrom": { "email": envelope.mail_from, "parameters": Value::Null },
        "rcptTo": envelope
            .rcpt_to
            .iter()
            .map(|email| json!({ "email": email, "parameters": Value::Null }))
            .collect::<Vec<_>>(),
    })
}

fn set_error(kind: &str, detail: &str) -> Value {
    json!({ "type": kind, "description": detail })
}

/// Parses a client-supplied envelope. The server derives one from the message
/// when the client omits it; this only runs for explicit envelopes.
fn parse_envelope(value: &Value) -> Result<SubmissionEnvelope, Value> {
    let object = value
        .as_object()
        .ok_or_else(|| set_error("invalidProperties", "envelope must be an object"))?;
    let mail_from = object
        .get("mailFrom")
        .and_then(|value| value.get("email"))
        .and_then(Value::as_str)
        .ok_or_else(|| set_error("invalidProperties", "envelope.mailFrom.email is required"))?;
    if !valid_address(mail_from) {
        return Err(json!({
            "type": "invalidRecipients",
            "description": "envelope.mailFrom is not a valid address",
            "invalidRecipients": [mail_from],
        }));
    }
    let recipients = object
        .get("rcptTo")
        .and_then(Value::as_array)
        .ok_or_else(|| set_error("invalidProperties", "envelope.rcptTo must be an array"))?;
    if recipients.is_empty() {
        return Err(set_error("noRecipients", "envelope has no recipients"));
    }
    let mut rcpt_to = Vec::with_capacity(recipients.len());
    let mut invalid = Vec::new();
    for recipient in recipients {
        let email = recipient
            .get("email")
            .and_then(Value::as_str)
            .ok_or_else(|| set_error("invalidProperties", "each rcptTo needs an email"))?;
        if valid_address(email) {
            rcpt_to.push(email.to_string());
        } else {
            invalid.push(email.to_string());
        }
    }
    if !invalid.is_empty() {
        return Err(json!({
            "type": "invalidRecipients",
            "description": "envelope contains invalid recipients",
            "invalidRecipients": invalid,
        }));
    }
    Ok(SubmissionEnvelope {
        mail_from: mail_from.to_string(),
        rcpt_to,
    })
}

/// Matches the address syntax `mail-core::compose` accepts, so a submission
/// cannot smuggle a display name or header injection into the SMTP envelope.
fn valid_address(value: &str) -> bool {
    if value.is_empty() || value.trim() != value {
        return false;
    }
    if value.chars().any(|c| {
        c.is_whitespace() || c.is_control() || matches!(c, ',' | ';' | ':' | '<' | '>' | '"')
    }) {
        return false;
    }
    match value.split_once('@') {
        Some((local, domain)) => !local.is_empty() && !domain.is_empty() && !domain.contains('@'),
        None => false,
    }
}

fn properties(args: &Map<String, Value>) -> Result<Vec<String>, ApiError> {
    match args.get("properties") {
        None | Some(Value::Null) => Ok(Vec::new()),
        Some(value) => {
            let values = value.as_array().ok_or(ApiError::InvalidArguments)?;
            let mut out = Vec::with_capacity(values.len());
            for value in values {
                out.push(
                    value
                        .as_str()
                        .ok_or(ApiError::InvalidArguments)?
                        .to_string(),
                );
            }
            Ok(out)
        }
    }
}

/// Keeps `id` plus every requested property that the object actually carries.
fn select_properties(object: Map<String, Value>, properties: &[String]) -> Value {
    if properties.is_empty() {
        return Value::Object(object);
    }
    let mut out = Map::new();
    if let Some(id) = object.get("id") {
        out.insert("id".into(), id.clone());
    }
    for name in properties {
        if let Some(value) = object.get(name) {
            out.insert(name.clone(), value.clone());
        }
    }
    Value::Object(out)
}

fn mailbox_role(role: Option<&str>) -> Value {
    match role {
        Some("inbox") => json!("inbox"),
        Some("archive") => json!("archive"),
        Some("drafts") => json!("drafts"),
        Some("sent") => json!("sent"),
        Some("trash") => json!("trash"),
        Some("spam") | Some("junk") => json!("junk"),
        _ => Value::Null,
    }
}

fn keywords(locations: &[MessageLocationInfo]) -> Value {
    let mut map = Map::new();
    for location in locations {
        for flag in &location.flags {
            let keyword = match flag.as_str() {
                "\\Seen" => "$seen",
                "\\Flagged" => "$flagged",
                "\\Answered" => "$answered",
                "\\Draft" => "$draft",
                "\\Forwarded" => "$forwarded",
                _ => continue,
            };
            map.insert(keyword.to_string(), json!(true));
        }
    }
    Value::Object(map)
}

fn address_list(values: &[String]) -> Value {
    Value::Array(values.iter().map(|raw| address(raw)).collect())
}

fn address(raw: &str) -> Value {
    let raw = raw.trim();
    let mut object = Map::new();
    if let (Some(start), Some(end)) = (raw.rfind('<'), raw.rfind('>')) {
        if end > start {
            let email = raw[start + 1..end].trim();
            let name = raw[..start].trim().trim_matches('"').trim();
            object.insert("email".into(), json!(email));
            object.insert(
                "name".into(),
                if name.is_empty() {
                    Value::Null
                } else {
                    json!(name)
                },
            );
            return Value::Object(object);
        }
    }
    object.insert("email".into(), json!(raw));
    object.insert("name".into(), Value::Null);
    Value::Object(object)
}

fn utc_date(timestamp: Option<i64>) -> String {
    let date = timestamp
        .and_then(|value| DateTime::from_timestamp(value, 0))
        .unwrap_or_else(Utc::now);
    date.to_rfc3339_opts(SecondsFormat::Secs, true)
}

fn email_object(
    message: &MessageWithLocations,
    body: &BodyView,
    fetch_text: bool,
    fetch_html: bool,
    max_body_bytes: Option<i64>,
) -> Map<String, Value> {
    let mut object = Map::new();
    object.insert("id".into(), json!(message.id.to_string()));
    object.insert(
        "threadId".into(),
        json!(message
            .thread_id
            .clone()
            .unwrap_or_else(|| format!("t{}", message.id))),
    );
    let mut mailbox_ids = Map::new();
    for location in &message.locations {
        mailbox_ids.insert(location.mailbox_id.to_string(), json!(true));
    }
    object.insert("mailboxIds".into(), Value::Object(mailbox_ids));
    object.insert("keywords".into(), keywords(&message.locations));
    object.insert("size".into(), json!(message.size.unwrap_or(0)));
    object.insert("receivedAt".into(), json!(utc_date(message.sent_at)));
    object.insert("sentAt".into(), json!(utc_date(message.sent_at)));
    object.insert(
        "messageId".into(),
        match &message.message_id {
            Some(id) => json!([id]),
            None => json!([]),
        },
    );
    object.insert("from".into(), address_list(&message.from));
    object.insert("to".into(), address_list(&message.to));
    object.insert("subject".into(), json!(message.subject));
    object.insert("preview".into(), json!(message.snippet));
    object.insert("hasAttachment".into(), json!(message.has_attach));

    // Single-part messages are cached under the empty section id; JMAP needs a
    // non-empty partId, so those become "text" / "html".
    let text_entry = body
        .parts
        .iter()
        .find(|part| !part.is_html && !part.part.is_empty())
        .or_else(|| body.parts.iter().find(|part| !part.is_html));
    let html_entry = body
        .parts
        .iter()
        .find(|part| part.is_html && !part.part.is_empty())
        .or_else(|| body.parts.iter().find(|part| part.is_html));
    let text_id = text_entry
        .map(|part| part.part.clone())
        .unwrap_or_else(|| "text".to_string());
    let html_id = html_entry
        .map(|part| part.part.clone())
        .unwrap_or_else(|| "html".to_string());
    let text_blob = text_entry.and_then(|part| part.blob_hash.clone());
    let html_blob = html_entry.and_then(|part| part.blob_hash.clone());

    let mut text_body = Vec::new();
    let mut html_body = Vec::new();
    let mut body_values = Map::new();
    if let Some(text) = &body.text {
        text_body.push(body_part(
            &text_id,
            text_blob.as_deref(),
            "text/plain",
            text.len(),
            None,
        ));
        if fetch_text && !text.is_empty() {
            body_values.insert(text_id.clone(), body_value(text, max_body_bytes));
        }
    }
    if let Some(html) = &body.html {
        html_body.push(body_part(
            &html_id,
            html_blob.as_deref(),
            "text/html",
            html.len(),
            None,
        ));
        if fetch_html && !html.is_empty() {
            body_values.insert(html_id.clone(), body_value(html, max_body_bytes));
        }
    }
    object.insert("textBody".into(), Value::Array(text_body));
    object.insert("htmlBody".into(), Value::Array(html_body));
    object.insert("bodyValues".into(), Value::Object(body_values));

    let attachments = body
        .attachments
        .iter()
        .map(|attachment| {
            body_part(
                &attachment.part,
                attachment.blob_hash.as_deref(),
                &attachment.content_type,
                attachment.size as usize,
                attachment.filename.as_deref(),
            )
        })
        .collect();
    object.insert("attachments".into(), Value::Array(attachments));
    object
}

/// One `EmailBodyPart` (RFC 8621 §4.1.4) with the properties the client needs
/// to render or list it. `blobId` is null until the bytes are cached.
fn body_part(
    part_id: &str,
    blob_id: Option<&str>,
    content_type: &str,
    size: usize,
    name: Option<&str>,
) -> Value {
    json!({
        "partId": part_id,
        "blobId": blob_id,
        "size": size,
        "name": name,
        "type": content_type,
        "charset": if content_type.starts_with("text/") { json!("utf-8") } else { Value::Null },
        "disposition": if name.is_some() { json!("attachment") } else { Value::Null },
        "cid": Value::Null,
        "language": Value::Null,
        "location": Value::Null,
    })
}

fn body_value(text: &str, max_bytes: Option<i64>) -> Value {
    let (value, truncated) = match max_bytes {
        Some(limit) if limit > 0 && text.len() > limit as usize => {
            truncate_utf8(text, limit as usize)
        }
        _ => (text.to_string(), false),
    };
    json!({
        "value": value,
        "isEncodingProblem": false,
        "isTruncated": truncated,
    })
}

/// Byte limit that never splits a multi-byte character.
fn truncate_utf8(text: &str, limit: usize) -> (String, bool) {
    if text.len() <= limit {
        return (text.to_string(), false);
    }
    let mut end = limit;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    (text[..end].to_string(), true)
}
