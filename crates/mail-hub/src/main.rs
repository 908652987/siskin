//! Optional Hub: a local HTTP service that exposes the synced mailboxes over a
//! JMAP subset. Desktops talk to `mail-core` directly; thin clients pair with
//! the Hub and use JMAP.
//!
//! Configuration is environment-only:
//! - `EYRIE_HUB_DB` (default `hub.db`)
//! - `EYRIE_HUB_BIND` (default `127.0.0.1:8080`)
//! - `EYRIE_HUB_SECRET_DIR` (file-backed secret store for headless/container
//!   deployments; the platform keychain is the default)
//! - `EYRIE_DEV_SECRET_DIR` (older name for the same file store, kept for
//!   development scripts)
//! - `EYRIE_HUB_MASTER_KEY_FILE` (optional base64 master key; imported before
//!   the database opens so the key can live outside the database volume)
//! - `EYRIE_HUB_PAIRING_CODE` (fixed pairing code; otherwise one is generated
//!   per process and printed to stdout)
//! - `EYRIE_HUB_SYNC` (`0`/`false`/`off` disables the background sync task)
//! - `EYRIE_HUB_SWEEP_SECONDS` (default 120)
//! - `EYRIE_HUB_IDLE_SESSIONS` (default 4)

use std::collections::HashMap;
use std::convert::Infallible;
use std::future::Future;
use std::net::SocketAddr;
use std::path::Path as FsPath;
use std::pin::Pin;
use std::sync::atomic::{AtomicI64, AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, Path, Query, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use futures::stream;
use mail_api::{
    BodySource, FetchedBody, Jmap, RequestError, SubmissionEnvelope, SubmissionOutcome,
    SubmissionSink, WriteError, WriteSink, MAX_UPLOAD_BYTES,
};
use mail_core::compose::{derive_envelope, strip_bcc_header};
use mail_core::{Core, CoreError, WatchOptions};
use mail_crypto::{FileStore, KeyringStore, SecretStore};
use mail_store::{Device, Store};
use rand::RngCore;
use serde::Deserialize;
use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};
use tokio::time::Instant;
use tracing::{info, warn};

const SECRET_SERVICE: &str = "mail-client-dev";

pub struct AppState {
    jmap: Arc<Jmap>,
    store: Arc<Store>,
    pairing_code: String,
    health: Arc<Health>,
}

/// Process and background-sync counters served by `/healthz`. Holds no
/// secrets: only timestamps, counts, and the sync configuration.
struct Health {
    started_at: SystemTime,
    sync: Option<SyncHealth>,
}

struct SyncHealth {
    idle_sessions: usize,
    sweep_seconds: u64,
    watchers: AtomicUsize,
    events: AtomicU64,
    last_event_at: AtomicI64,
}

impl Health {
    fn new(sync: Option<SyncConfig>) -> Self {
        Self {
            started_at: SystemTime::now(),
            sync: sync.map(|config| SyncHealth {
                idle_sessions: config.idle_sessions,
                sweep_seconds: config.sweep_seconds,
                watchers: AtomicUsize::new(0),
                events: AtomicU64::new(0),
                last_event_at: AtomicI64::new(0),
            }),
        }
    }

    fn uptime_seconds(&self) -> u64 {
        SystemTime::now()
            .duration_since(self.started_at)
            .map(|elapsed| elapsed.as_secs())
            .unwrap_or(0)
    }

    fn watcher_started(&self) {
        if let Some(sync) = self.sync.as_ref() {
            sync.watchers.fetch_add(1, Ordering::Relaxed);
        }
    }

    fn record_event(&self) {
        if let Some(sync) = self.sync.as_ref() {
            sync.events.fetch_add(1, Ordering::Relaxed);
            sync.last_event_at.store(now_unix(), Ordering::Relaxed);
        }
    }

    fn sync_json(&self) -> Value {
        let Some(sync) = self.sync.as_ref() else {
            return json!({ "enabled": false });
        };
        let last_event_at = sync.last_event_at.load(Ordering::Relaxed);
        json!({
            "enabled": true,
            "watchers": sync.watchers.load(Ordering::Relaxed),
            "events": sync.events.load(Ordering::Relaxed),
            "lastEventAt": if last_event_at > 0 { json!(last_event_at) } else { Value::Null },
            "sweepSeconds": sync.sweep_seconds,
            "idleSessions": sync.idle_sessions,
        })
    }
}

/// Background sync mirrors `mail-cli watch`: one IDLE pool per account plus a
/// sweep connection for mailboxes that cannot push.
#[derive(Clone, Copy)]
struct SyncConfig {
    idle_sessions: usize,
    sweep_seconds: u64,
}

impl Default for SyncConfig {
    fn default() -> Self {
        let defaults = WatchOptions::default();
        Self {
            idle_sessions: defaults.idle_sessions,
            sweep_seconds: defaults.sweep_seconds,
        }
    }
}

impl SyncConfig {
    /// `None` disables the background task; accounts then only sync when a
    /// client asks for data through JMAP or a CLI runs `sync`.
    fn from_env() -> Option<Self> {
        let disabled = std::env::var("EYRIE_HUB_SYNC").ok().is_some_and(|value| {
            matches!(
                value.trim().to_ascii_lowercase().as_str(),
                "0" | "false" | "off" | "no"
            )
        });
        if disabled {
            return None;
        }
        let defaults = Self::default();
        Some(Self {
            idle_sessions: env_number("EYRIE_HUB_IDLE_SESSIONS").unwrap_or(defaults.idle_sessions),
            sweep_seconds: env_number("EYRIE_HUB_SWEEP_SECONDS")
                .unwrap_or(defaults.sweep_seconds)
                .max(1),
        })
    }
}

fn env_number<T: std::str::FromStr>(name: &str) -> Option<T> {
    std::env::var(name).ok()?.trim().parse().ok()
}

fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs() as i64)
        .unwrap_or(0)
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let db = std::env::var("EYRIE_HUB_DB").unwrap_or_else(|_| "hub.db".to_string());
    let bind: SocketAddr = std::env::var("EYRIE_HUB_BIND")
        .unwrap_or_else(|_| "127.0.0.1:8080".to_string())
        .parse()?;

    let secrets = secret_store()?;
    import_master_key_from_file(secrets.as_ref(), &db)?;
    let core = Arc::new(Core::open(&db, secrets)?);
    let store = core.store();
    let jmap = Jmap::new(Arc::clone(&store))
        .with_bodies(Arc::new(CoreBodies {
            core: Arc::clone(&core),
            store: Arc::clone(&store),
        }))
        .with_writes(Arc::new(CoreWrites {
            core: Arc::clone(&core),
            store: Arc::clone(&store),
        }))
        .with_submissions(Arc::new(CoreSubmission {
            core: Arc::clone(&core),
            store: Arc::clone(&store),
        }));

    let pairing_code = match std::env::var("EYRIE_HUB_PAIRING_CODE") {
        Ok(code) if !code.trim().is_empty() => code.trim().to_string(),
        _ => {
            let code = random_code();
            // Printed once so the user can pair a device; it is not persisted.
            println!("pairing code: {code}");
            code
        }
    };
    let sync_config = SyncConfig::from_env();
    let health = Arc::new(Health::new(sync_config));
    match sync_config {
        Some(config) => {
            info!(
                sweep_seconds = config.sweep_seconds,
                idle_sessions = config.idle_sessions,
                "background sync enabled"
            );
            spawn_sync(
                Arc::clone(&core),
                Arc::clone(&store),
                Arc::clone(&health),
                config,
            );
        }
        None => info!("background sync disabled by EYRIE_HUB_SYNC"),
    }

    let state = Arc::new(AppState {
        jmap: Arc::new(jmap),
        store,
        pairing_code,
        health,
    });

    let listener = tokio::net::TcpListener::bind(bind).await?;
    info!(%bind, %db, "hub listening");
    axum::serve(listener, router(state)).await?;
    Ok(())
}

/// Starts one watcher task per account. Accounts can be added while the Hub
/// runs (the CLI writes to the same database), so the watcher set is resolved
/// again on every pass instead of being fixed at startup.
fn spawn_sync(core: Arc<Core>, store: Arc<Store>, health: Arc<Health>, config: SyncConfig) {
    tokio::spawn(async move {
        let mut watched: HashMap<String, tokio::task::JoinHandle<()>> = HashMap::new();
        loop {
            watched.retain(|_, handle| !handle.is_finished());
            match store.list_accounts() {
                Ok(accounts) => {
                    for account in accounts {
                        if watched.contains_key(&account.email) {
                            continue;
                        }
                        info!(account = %account.email, "starting background sync");
                        let email = account.email.clone();
                        let handle = tokio::spawn(watch_account_forever(
                            Arc::clone(&core),
                            Arc::clone(&health),
                            email.clone(),
                            config,
                        ));
                        watched.insert(email, handle);
                    }
                }
                Err(err) => warn!(%err, "listing accounts for background sync failed"),
            }
            tokio::time::sleep(Duration::from_secs(SYNC_RESOLVE_SECONDS)).await;
        }
    });
}

/// `watch_account` reconnects on its own; this loop only restarts it when the
/// watcher returns (event budget, missing account, or a fatal setup error).
async fn watch_account_forever(
    core: Arc<Core>,
    health: Arc<Health>,
    email: String,
    config: SyncConfig,
) {
    health.watcher_started();
    let mut backoff = 1u64;
    loop {
        let sink = Arc::clone(&health);
        let result = core
            .watch_account(
                &email,
                usize::MAX,
                WatchOptions {
                    idle_sessions: config.idle_sessions,
                    sweep_seconds: config.sweep_seconds,
                },
                move |_outcome| sink.record_event(),
            )
            .await;
        match result {
            Ok(()) => warn!(account = %email, "background sync stopped, restarting"),
            Err(err) => warn!(account = %email, %err, "background sync failed, retrying"),
        }
        tokio::time::sleep(Duration::from_secs(backoff)).await;
        backoff = (backoff * 2).min(60);
    }
}

const SYNC_RESOLVE_SECONDS: u64 = 30;

/// Unauthenticated liveness/readiness probe. Reports the database and the
/// background task; it deliberately does not dial the mail servers, because a
/// transient IMAP outage must not mark the Hub itself unhealthy.
async fn healthz(State(state): State<Arc<AppState>>) -> Response {
    let accounts = match state.store.list_accounts() {
        Ok(accounts) => accounts,
        Err(err) => {
            warn!(%err, "health check: database unavailable");
            return problem(
                StatusCode::SERVICE_UNAVAILABLE,
                "urn:ietf:params:jmap:error:serverFail",
                "database unavailable",
            );
        }
    };
    let mut last_sync_at: Option<i64> = None;
    for account in &accounts {
        match state.store.list_mailboxes(account.id) {
            Ok(mailboxes) => {
                for mailbox in mailboxes {
                    last_sync_at = last_sync_at.max(mailbox.last_sync_at);
                }
            }
            Err(err) => warn!(%err, account_id = account.id, "health check: mailbox query failed"),
        }
    }
    let body = json!({
        "status": "ok",
        "version": env!("CARGO_PKG_VERSION"),
        "uptimeSeconds": state.health.uptime_seconds(),
        "database": "ok",
        "accounts": accounts.len(),
        "lastSyncAt": last_sync_at,
        "sync": state.health.sync_json(),
    });
    (StatusCode::OK, Json(body)).into_response()
}

/// Routes are split out so tests can drive the same handlers without a socket.
fn router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/.well-known/jmap", get(session))
        .route("/jmap", post(jmap))
        .route("/pair", post(pair))
        .route("/download/{account_id}/{blob_id}/{name}", get(download))
        // The Session advertises `/upload/{accountId}/`; clients substitute the
        // id and usually keep the trailing slash, so both forms are routed.
        .route(
            "/upload/{account_id}",
            post(upload).layer(DefaultBodyLimit::max(MAX_UPLOAD_BYTES as usize)),
        )
        .route(
            "/upload/{account_id}/",
            post(upload).layer(DefaultBodyLimit::max(MAX_UPLOAD_BYTES as usize)),
        )
        .route("/eventsource/", get(eventsource))
        .route("/healthz", get(healthz))
        .with_state(state)
}

async fn session(State(state): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    let account_id = match account_of(&headers, &state) {
        Ok(account_id) => account_id,
        Err(err) => return auth_error(err),
    };
    match state.jmap.session_for(account_id) {
        Ok(Some(value)) => Json(value).into_response(),
        Ok(None) => problem(
            StatusCode::NOT_FOUND,
            "urn:ietf:params:jmap:error:notFound",
            "account not found",
        ),
        Err(err) => server_error(err),
    }
}

async fn jmap(State(state): State<Arc<AppState>>, headers: HeaderMap, body: Bytes) -> Response {
    let account_id = match account_of(&headers, &state) {
        Ok(account_id) => account_id,
        Err(err) => return auth_error(err),
    };
    let request: Value = match serde_json::from_slice(&body) {
        Ok(value) => value,
        Err(err) => return problem(StatusCode::BAD_REQUEST, "notJSON", &err.to_string()),
    };
    match state.jmap.handle_for_account(request, account_id).await {
        Ok(value) => Json(value).into_response(),
        Err(err) => request_error(err),
    }
}

#[derive(Deserialize)]
struct PairRequest {
    code: String,
    name: Option<String>,
    #[serde(rename = "accountId")]
    account_id: Option<Value>,
}

async fn pair(State(state): State<Arc<AppState>>, Json(request): Json<PairRequest>) -> Response {
    if !constant_time_eq(request.code.trim(), &state.pairing_code) {
        return problem(
            StatusCode::FORBIDDEN,
            "urn:ietf:params:jmap:error:forbidden",
            "pairing code does not match",
        );
    }
    let account_id = match resolve_pairing_account(&state, request.account_id.as_ref()) {
        Ok(account_id) => account_id,
        Err(response) => return *response,
    };
    let token = random_token();
    let name = request
        .name
        .map(|name| name.trim().to_string())
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| "device".to_string());
    match state
        .store
        .create_device(&name, account_id, &token_hash(&token))
    {
        Ok(device_id) => {
            info!(device_id, %name, account_id, "device paired");
            (
                StatusCode::CREATED,
                Json(json!({
                    "token": token,
                    "device": name,
                    "accountId": account_id.to_string(),
                })),
            )
                .into_response()
        }
        Err(err) => server_error(err),
    }
}

/// Resolves which account a newly paired device may use. A store with exactly
/// one account binds it implicitly; otherwise the request must name one.
fn resolve_pairing_account(
    state: &AppState,
    requested: Option<&Value>,
) -> Result<i64, Box<Response>> {
    let accounts = match state.store.list_accounts() {
        Ok(accounts) => accounts,
        Err(err) => return Err(Box::new(server_error(err))),
    };
    let requested = requested.and_then(|value| {
        value
            .as_i64()
            .or_else(|| value.as_str().and_then(|raw| raw.parse().ok()))
    });
    match requested {
        Some(account_id) => {
            if state
                .store
                .account_by_id(account_id)
                .map_err(|err| Box::new(server_error(err)))?
                .is_some()
            {
                Ok(account_id)
            } else {
                Err(Box::new(problem(
                    StatusCode::BAD_REQUEST,
                    "urn:ietf:params:jmap:error:notFound",
                    "account not found",
                )))
            }
        }
        None => match accounts.as_slice() {
            [account] => Ok(account.id),
            _ => Err(Box::new(problem(
                StatusCode::BAD_REQUEST,
                "urn:ietf:params:jmap:error:invalidArguments",
                "accountId is required when the store holds more than one account",
            ))),
        },
    }
}

/// Stores bytes for later `Email/import`. The id is content addressed, so a
/// retry of the same body returns the same `blobId`.
async fn upload(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(account_id): Path<i64>,
    body: Bytes,
) -> Response {
    let bound = match account_of(&headers, &state) {
        Ok(account_id) => account_id,
        Err(err) => return auth_error(err),
    };
    if bound != account_id {
        return problem(
            StatusCode::NOT_FOUND,
            "urn:ietf:params:jmap:error:notFound",
            "account not found",
        );
    }
    match state.store.account_by_id(account_id) {
        Ok(Some(_)) => {}
        Ok(None) => {
            return problem(
                StatusCode::NOT_FOUND,
                "urn:ietf:params:jmap:error:notFound",
                "account not found",
            )
        }
        Err(err) => return server_error(err),
    }
    if body.len() as i64 > MAX_UPLOAD_BYTES {
        return problem(
            StatusCode::PAYLOAD_TOO_LARGE,
            "urn:ietf:params:jmap:error:limit",
            "upload exceeds maxSizeUpload",
        );
    }
    let content_type = headers
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("application/octet-stream")
        .to_string();
    match state.store.store_upload(account_id, &content_type, &body) {
        Ok(blob_id) => (
            StatusCode::CREATED,
            Json(json!({
                "accountId": account_id.to_string(),
                "blobId": blob_id,
                "type": content_type,
                "size": body.len(),
            })),
        )
            .into_response(),
        Err(err) => server_error(err),
    }
}

/// Serves a cached body part or attachment. The blob id is scoped to the
/// account so a paired device cannot read another account's content.
async fn download(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((account_id, blob_id, _name)): Path<(i64, String, String)>,
) -> Response {
    let bound = match account_of(&headers, &state) {
        Ok(account_id) => account_id,
        Err(err) => return auth_error(err),
    };
    if bound != account_id {
        return problem(
            StatusCode::NOT_FOUND,
            "urn:ietf:params:jmap:error:notFound",
            "blob not found",
        );
    }
    let blob = match state.store.blob_for_account(account_id, &blob_id) {
        Ok(Some(blob)) => blob,
        Ok(None) => {
            return problem(
                StatusCode::NOT_FOUND,
                "urn:ietf:params:jmap:error:notFound",
                "blob not found",
            )
        }
        Err(err) => return server_error(err),
    };
    let content_type = blob
        .content_type
        .as_deref()
        .unwrap_or("application/octet-stream");
    let disposition = if blob.attachment {
        "attachment"
    } else {
        "inline"
    };
    (
        StatusCode::OK,
        [
            (header::CONTENT_TYPE, content_type.to_string()),
            (header::CONTENT_DISPOSITION, disposition.to_string()),
        ],
        blob.bytes,
    )
        .into_response()
}

#[derive(Deserialize)]
struct EventQuery {
    #[serde(default)]
    types: Option<String>,
    #[serde(default)]
    closeafter: Option<String>,
    #[serde(default)]
    ping: Option<String>,
}

/// JMAP event source (RFC 8620 §7.3). State is polled from the change log;
/// the Hub has no push transport of its own yet.
async fn eventsource(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Query(params): Query<EventQuery>,
) -> Response {
    let bound = match account_of(&headers, &state) {
        Ok(account_id) => account_id,
        Err(err) => return auth_error(err),
    };
    let ping = parse_seconds(params.ping.as_deref(), 10);
    let closeafter = parse_seconds(params.closeafter.as_deref(), 60);
    let types: Vec<String> = params
        .types
        .as_deref()
        .unwrap_or("Email,Mailbox")
        .split(',')
        .map(str::trim)
        .filter(|name| matches!(*name, "Email" | "Mailbox"))
        .map(str::to_string)
        .collect();
    let types = if types.is_empty() {
        vec!["Email".to_string(), "Mailbox".to_string()]
    } else {
        types
    };

    let accounts = vec![bound];
    let mut interval = tokio::time::interval(ping);
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    let initial = StreamState {
        store: Arc::clone(&state.store),
        accounts,
        types,
        last: HashMap::new(),
        interval,
        deadline: Instant::now() + closeafter,
        first: true,
    };
    let stream = stream::unfold(initial, |mut state| async move {
        if Instant::now() >= state.deadline {
            return None;
        }
        if state.first {
            state.first = false;
        } else {
            state.interval.tick().await;
            if Instant::now() >= state.deadline {
                return None;
            }
        }

        let mut changed = Map::new();
        for account in &state.accounts {
            let seq = match state.store.latest_change_seq(*account) {
                Ok(seq) => seq,
                Err(err) => {
                    warn!(%err, account = *account, "event source state read failed");
                    continue;
                }
            };
            if state.last.get(account) == Some(&seq) {
                continue;
            }
            let mut per_type = Map::new();
            for name in &state.types {
                per_type.insert(name.clone(), json!(seq.to_string()));
            }
            changed.insert(account.to_string(), Value::Object(per_type));
            state.last.insert(*account, seq);
        }

        let event = if changed.is_empty() {
            Event::default().event("ping").data("{}")
        } else {
            Event::default()
                .event("state")
                .data(json!({"@type": "StateChange", "changed": changed}).to_string())
        };
        Some((Ok::<_, Infallible>(event), state))
    });

    Sse::new(stream)
        .keep_alive(KeepAlive::default())
        .into_response()
}

struct StreamState {
    store: Arc<Store>,
    accounts: Vec<i64>,
    types: Vec<String>,
    last: HashMap<i64, i64>,
    interval: tokio::time::Interval,
    deadline: Instant,
    first: bool,
}

/// Accepts both plain seconds and the `PT30S` duration form.
fn parse_seconds(raw: Option<&str>, default: u64) -> Duration {
    let Some(raw) = raw.map(str::trim).filter(|raw| !raw.is_empty()) else {
        return Duration::from_secs(default);
    };
    let seconds = raw.parse::<u64>().ok().or_else(|| {
        raw.strip_prefix("PT")
            .and_then(|raw| raw.strip_suffix('S'))?
            .parse()
            .ok()
    });
    Duration::from_secs(seconds.unwrap_or(default).clamp(1, 300))
}

#[derive(Debug)]
enum AuthError {
    MissingToken,
    UnknownToken,
    UnboundAccount,
    Store(mail_store::StoreError),
}

/// Authenticates and returns the single account the device may use. Devices
/// paired before accounts were bound cannot be attributed, so they must pair
/// again instead of inheriting access to every account.
fn account_of(headers: &HeaderMap, state: &AppState) -> Result<i64, AuthError> {
    let device = authenticate(headers, state)?;
    bound_account(&device)
}

fn bound_account(device: &Device) -> Result<i64, AuthError> {
    device.account_id.ok_or(AuthError::UnboundAccount)
}

fn authenticate(headers: &HeaderMap, state: &AppState) -> Result<Device, AuthError> {
    let token = headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .map(str::trim)
        .filter(|token| !token.is_empty())
        .ok_or(AuthError::MissingToken)?;
    let device = state
        .store
        .device_by_token_hash(&token_hash(token))
        .map_err(AuthError::Store)?
        .ok_or(AuthError::UnknownToken)?;
    if let Err(err) = state.store.touch_device(device.id) {
        warn!(%err, "failed to update device last_seen_at");
    }
    Ok(device)
}

fn auth_error(err: AuthError) -> Response {
    match err {
        AuthError::Store(err) => server_error(err),
        AuthError::MissingToken => unauthorized("missing bearer token"),
        AuthError::UnknownToken => unauthorized("unknown token"),
        AuthError::UnboundAccount => problem(
            StatusCode::FORBIDDEN,
            "urn:ietf:params:jmap:error:forbidden",
            "device is not bound to an account; pair it again",
        ),
    }
}

fn unauthorized(detail: &str) -> Response {
    problem(
        StatusCode::UNAUTHORIZED,
        "urn:ietf:params:jmap:error:forbidden",
        detail,
    )
}

fn request_error(err: RequestError) -> Response {
    let status = match err {
        RequestError::Store(_) => StatusCode::INTERNAL_SERVER_ERROR,
        _ => StatusCode::BAD_REQUEST,
    };
    problem(
        status,
        &format!("urn:ietf:params:jmap:error:{}", err.problem_type()),
        &err.to_string(),
    )
}

fn server_error(err: impl std::fmt::Display) -> Response {
    problem(
        StatusCode::INTERNAL_SERVER_ERROR,
        "urn:ietf:params:jmap:error:serverFail",
        &err.to_string(),
    )
}

fn problem(status: StatusCode, kind: &str, detail: &str) -> Response {
    let body = json!({ "type": kind, "detail": detail });
    (
        status,
        [(header::CONTENT_TYPE, "application/problem+json")],
        body.to_string(),
    )
        .into_response()
}

fn random_token() -> String {
    let mut bytes = [0u8; 32];
    rand::rngs::OsRng.fill_bytes(&mut bytes);
    URL_SAFE_NO_PAD.encode(bytes)
}

fn random_code() -> String {
    let mut bytes = [0u8; 4];
    rand::rngs::OsRng.fill_bytes(&mut bytes);
    URL_SAFE_NO_PAD.encode(bytes)
}

fn token_hash(token: &str) -> String {
    URL_SAFE_NO_PAD.encode(Sha256::digest(token.as_bytes()))
}

fn constant_time_eq(left: &str, right: &str) -> bool {
    if left.len() != right.len() {
        return false;
    }
    left.bytes()
        .zip(right.bytes())
        .fold(0u8, |acc, (a, b)| acc | (a ^ b))
        == 0
}

/// Headless deployments have no platform keychain, so a 0600 file store on a
/// mounted volume is the supported server backend. The directory holds the
/// master key and every account password; keep it on a separate volume from
/// the database and back both up independently.
fn secret_store() -> anyhow::Result<Arc<dyn SecretStore>> {
    let dir =
        std::env::var("EYRIE_HUB_SECRET_DIR").or_else(|_| std::env::var("EYRIE_DEV_SECRET_DIR"));
    match dir {
        Ok(dir) => Ok(Arc::new(FileStore::new(dir)?)),
        Err(_) => Ok(Arc::new(KeyringStore::new(SECRET_SERVICE))),
    }
}

/// Imports a master key supplied by the deployment (Docker/Kubernetes secret)
/// before the database opens. The key is validated against an existing
/// database first, so a wrong secret aborts startup instead of replacing the
/// key that protects the data.
fn import_master_key_from_file(secrets: &dyn SecretStore, db: &str) -> anyhow::Result<()> {
    let path = std::env::var("EYRIE_HUB_MASTER_KEY_FILE")
        .ok()
        .map(|path| path.trim().to_string())
        .filter(|path| !path.is_empty());
    let Some(path) = path else {
        return Ok(());
    };
    let encoded =
        std::fs::read_to_string(&path).map_err(|err| anyhow::anyhow!("reading {path}: {err}"))?;
    let updated =
        mail_core::import_master_key(secrets, encoded.trim(), Some(FsPath::new(db)), true)?;
    info!(path, updated, "imported master key file");
    Ok(())
}

/// Fetches bodies through `mail-core` so the Hub can serve uncached messages.
struct CoreBodies {
    core: Arc<Core>,
    store: Arc<Store>,
}

impl BodySource for CoreBodies {
    fn fetch<'a>(
        &'a self,
        account_id: i64,
        message_id: i64,
    ) -> Pin<Box<dyn Future<Output = Result<FetchedBody, String>> + Send + 'a>> {
        Box::pin(async move {
            let account = self
                .store
                .account_by_id(account_id)
                .map_err(|err| err.to_string())?
                .ok_or_else(|| format!("account not found: {account_id}"))?;
            let content = self
                .core
                .fetch_body(&account.email, message_id)
                .await
                .map_err(|err| err.to_string())?;
            Ok(FetchedBody {
                text: content.text,
                html: content.html,
            })
        })
    }
}

/// Applies JMAP writes through `mail-core`; credentials stay in the core.
struct CoreWrites {
    core: Arc<Core>,
    store: Arc<Store>,
}

impl CoreWrites {
    fn email(&self, account_id: i64) -> Result<String, WriteError> {
        self.store
            .account_by_id(account_id)
            .map_err(|err| WriteError::Server(err.to_string()))?
            .map(|account| account.email)
            .ok_or_else(|| WriteError::NotFound(format!("account {account_id} not found")))
    }
}

impl WriteSink for CoreWrites {
    fn set_keywords<'a>(
        &'a self,
        account_id: i64,
        message_id: i64,
        add: Vec<String>,
        remove: Vec<String>,
    ) -> Pin<Box<dyn Future<Output = Result<(), WriteError>> + Send + 'a>> {
        Box::pin(async move {
            let email = self.email(account_id)?;
            self.core
                .set_keywords(&email, message_id, &add, &remove)
                .await
                .map_err(core_write_error)
        })
    }

    fn move_message<'a>(
        &'a self,
        account_id: i64,
        message_id: i64,
        mailbox_id: i64,
    ) -> Pin<Box<dyn Future<Output = Result<(), WriteError>> + Send + 'a>> {
        Box::pin(async move {
            let email = self.email(account_id)?;
            self.core
                .move_message(&email, message_id, mailbox_id)
                .await
                .map_err(core_write_error)
        })
    }

    fn import_message<'a>(
        &'a self,
        account_id: i64,
        mailbox_id: i64,
        raw: &'a [u8],
        keywords: Vec<String>,
    ) -> Pin<Box<dyn Future<Output = Result<i64, WriteError>> + Send + 'a>> {
        Box::pin(async move {
            let email = self.email(account_id)?;
            self.core
                .import_message(&email, mailbox_id, raw, &keywords)
                .await
                .map_err(core_write_error)
        })
    }

    fn destroy_message<'a>(
        &'a self,
        account_id: i64,
        message_id: i64,
    ) -> Pin<Box<dyn Future<Output = Result<(), WriteError>> + Send + 'a>> {
        Box::pin(async move {
            let email = self.email(account_id)?;
            self.core
                .destroy_message(&email, message_id)
                .await
                .map_err(core_write_error)
        })
    }
}

fn core_write_error(err: CoreError) -> WriteError {
    match err {
        CoreError::MessageNotFound(_) | CoreError::MailboxNotFound(_) => {
            WriteError::NotFound(err.to_string())
        }
        CoreError::Unsupported(_) | CoreError::UnknownKeyword(_) => {
            WriteError::NotSupported(err.to_string())
        }
        other => WriteError::Server(other.to_string()),
    }
}

/// Delivers `EmailSubmission` messages through `mail-core`: SMTP first, then
/// the Sent copy. The SMTP envelope is the client's, or derived from the
/// message headers when the client omitted it.
struct CoreSubmission {
    core: Arc<Core>,
    store: Arc<Store>,
}

impl SubmissionSink for CoreSubmission {
    fn submit<'a>(
        &'a self,
        account_id: i64,
        raw: Vec<u8>,
        envelope: Option<SubmissionEnvelope>,
    ) -> Pin<Box<dyn Future<Output = Result<SubmissionOutcome, WriteError>> + Send + 'a>> {
        Box::pin(async move {
            let account = self
                .store
                .account_by_id(account_id)
                .map_err(|err| WriteError::Server(err.to_string()))?
                .ok_or_else(|| WriteError::NotFound(format!("account {account_id} not found")))?;
            let envelope = match envelope {
                Some(envelope) => envelope,
                None => SubmissionEnvelope {
                    mail_from: account.email.clone(),
                    rcpt_to: derive_envelope(&raw)
                        .map_err(|err| WriteError::NotSupported(err.to_string()))?,
                },
            };
            // RFC 8621 §7.5: Bcc addresses stay in the envelope only.
            let raw = strip_bcc_header(&raw);
            let outcome = self
                .core
                .send_raw(&account.email, &envelope.rcpt_to, &raw)
                .await
                .map_err(core_write_error)?;
            Ok(SubmissionOutcome {
                envelope,
                recipients: outcome.recipients,
                saved_to: outcome.saved_to,
                save_error: outcome.save_error,
            })
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::Request;
    use mail_store::{BodyPartInput, DbKey, NewMessage};
    use tower::ServiceExt;

    fn state() -> Arc<AppState> {
        let key = DbKey::from_bytes(&[9u8; 32]).expect("key");
        let store = Arc::new(Store::open_in_memory(&key).expect("store"));
        let jmap = Arc::new(Jmap::new(Arc::clone(&store)));
        Arc::new(AppState {
            jmap,
            store,
            pairing_code: "pair-me".to_string(),
            health: Arc::new(Health::new(Some(SyncConfig::default()))),
        })
    }

    fn seed_account(state: &Arc<AppState>) -> i64 {
        if let Some(account) = state.store.list_accounts().expect("accounts").first() {
            return account.id;
        }
        state
            .store
            .create_account("imap", "me@example.com", None, "secret", &json!({}))
            .expect("account")
    }

    async fn body_json(response: Response) -> Value {
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("body");
        serde_json::from_slice(&bytes).expect("json")
    }

    async fn pair_with(state: &Arc<AppState>, code: &str, account_id: Option<i64>) -> Response {
        let body = match account_id {
            Some(account_id) => {
                format!(r#"{{"code":"{code}","name":"phone","accountId":{account_id}}}"#)
            }
            None => format!(r#"{{"code":"{code}","name":"phone"}}"#),
        };
        router(Arc::clone(state))
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/pair")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(body))
                    .expect("request"),
            )
            .await
            .expect("response")
    }

    async fn paired_token_for(state: &Arc<AppState>, account_id: i64) -> String {
        body_json(pair_with(state, "pair-me", Some(account_id)).await).await["token"]
            .as_str()
            .expect("token")
            .to_string()
    }

    async fn paired_token(state: &Arc<AppState>) -> String {
        let account_id = seed_account(state);
        paired_token_for(state, account_id).await
    }

    #[tokio::test]
    async fn pairing_checks_the_code_and_stores_only_a_hash() {
        let state = state();
        let account = seed_account(&state);
        let wrong = pair_with(&state, "nope", Some(account)).await;
        assert_eq!(wrong.status(), StatusCode::FORBIDDEN);

        let response = pair_with(&state, "pair-me", Some(account)).await;
        assert_eq!(response.status(), StatusCode::CREATED);
        let payload = body_json(response).await;
        assert_eq!(payload["accountId"], json!(account.to_string()));
        let token = payload["token"].as_str().expect("token").to_string();
        let device = state
            .store
            .device_by_token_hash(&token_hash(&token))
            .expect("lookup")
            .expect("device");
        assert_eq!(device.account_id, Some(account));
        assert!(state
            .store
            .device_by_token_hash(&token)
            .expect("lookup")
            .is_none());
    }

    #[tokio::test]
    async fn pairing_requires_an_account_when_several_exist() {
        let state = state();
        seed_account(&state);
        state
            .store
            .create_account("imap", "other@example.com", None, "secret", &json!({}))
            .expect("account");
        let response = pair_with(&state, "pair-me", None).await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn protected_endpoints_require_a_device_token() {
        let state = state();
        let response = router(Arc::clone(&state))
            .oneshot(
                Request::builder()
                    .uri("/.well-known/jmap")
                    .body(Body::empty())
                    .expect("request"),
            )
            .await
            .expect("response");
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);

        let token = paired_token(&state).await;
        let response = router(state)
            .oneshot(
                Request::builder()
                    .uri("/.well-known/jmap")
                    .header(header::AUTHORIZATION, format!("Bearer {token}"))
                    .body(Body::empty())
                    .expect("request"),
            )
            .await
            .expect("response");
        assert_eq!(response.status(), StatusCode::OK);
        let session = body_json(response).await;
        assert!(session["capabilities"][mail_api::MAIL_CAPABILITY].is_object());
    }

    #[tokio::test]
    async fn jmap_endpoint_answers_method_calls() {
        let state = state();
        let account = seed_account(&state);
        let token = paired_token(&state).await;
        let response = router(state)
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/jmap")
                    .header(header::CONTENT_TYPE, "application/json")
                    .header(header::AUTHORIZATION, format!("Bearer {token}"))
                    .body(Body::from(format!(
                        r#"{{"using":["urn:ietf:params:jmap:core"],"methodCalls":[["Mailbox/get",{{"accountId":"{account}"}},"c0"]]}}"#
                    )))
                    .expect("request"),
            )
            .await
            .expect("response");
        assert_eq!(response.status(), StatusCode::OK);
        let payload = body_json(response).await;
        assert_eq!(payload["methodResponses"][0][0], json!("Mailbox/get"));
        assert_eq!(
            payload["methodResponses"][0][1]["accountId"],
            json!(account.to_string())
        );
    }

    #[tokio::test]
    async fn a_device_cannot_reach_another_account() {
        let state = state();
        let account = seed_account(&state);
        let other = state
            .store
            .create_account("imap", "other@example.com", None, "secret", &json!({}))
            .expect("account");
        let token = paired_token_for(&state, account).await;

        let response = router(Arc::clone(&state))
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/jmap")
                    .header(header::CONTENT_TYPE, "application/json")
                    .header(header::AUTHORIZATION, format!("Bearer {token}"))
                    .body(Body::from(format!(
                        r#"{{"using":["urn:ietf:params:jmap:core"],"methodCalls":[
                             ["Mailbox/get",{{"accountId":"{account}"}},"a"],
                             ["Mailbox/get",{{"accountId":"{other}"}},"b"]
                           ]}}"#
                    )))
                    .expect("request"),
            )
            .await
            .expect("response");
        assert_eq!(response.status(), StatusCode::OK);
        let payload = body_json(response).await;
        assert_eq!(payload["methodResponses"][0][0], json!("Mailbox/get"));
        assert_eq!(payload["methodResponses"][1][0], json!("error"));
        assert_eq!(
            payload["methodResponses"][1][1]["type"],
            json!("accountNotFound")
        );

        // Uploads and downloads for another account are indistinguishable from
        // missing blobs.
        let upload = router(Arc::clone(&state))
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(format!("/upload/{other}"))
                    .header(header::CONTENT_TYPE, "message/rfc822")
                    .header(header::AUTHORIZATION, format!("Bearer {token}"))
                    .body(Body::from("Subject: x\r\n\r\n"))
                    .expect("request"),
            )
            .await
            .expect("response");
        assert_eq!(upload.status(), StatusCode::NOT_FOUND);
    }

    #[test]
    fn a_legacy_device_without_an_account_is_refused() {
        let device = Device {
            id: 1,
            name: "phone".to_string(),
            account_id: None,
            created_at: 0,
            last_seen_at: None,
        };
        assert!(matches!(
            bound_account(&device),
            Err(AuthError::UnboundAccount)
        ));
        let bound = Device {
            account_id: Some(7),
            ..device
        };
        assert_eq!(bound_account(&bound).expect("bound"), 7);
    }

    fn seed_blob(state: &Arc<AppState>) -> (i64, String) {
        let account = state
            .store
            .create_account("imap", "me@example.com", None, "secret", &json!({}))
            .expect("account");
        let inbox = state
            .store
            .ensure_mailbox(account, "INBOX", Some("inbox"))
            .expect("inbox")
            .id;
        state
            .store
            .apply_fetch(
                account,
                inbox,
                &[NewMessage {
                    uid: 1,
                    message_id: Some("<1@example.com>".to_string()),
                    in_reply_to: None,
                    subject: Some("hello".to_string()),
                    from: vec!["Alice <alice@example.com>".to_string()],
                    to: vec!["me@example.com".to_string()],
                    sent_at: Some(1_700_000_000),
                    size: Some(10),
                    flags: Vec::new(),
                    snippet: Some("hello".to_string()),
                }],
                &[],
            )
            .expect("fetch");
        let message_id = state.store.list_messages(account, 10, 0).unwrap()[0].id;
        state
            .store
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
                None,
            )
            .expect("body");
        let body = state
            .store
            .message_body(message_id)
            .expect("message")
            .expect("body");
        (account, body.parts[0].blob_hash.clone().expect("blob id"))
    }

    #[tokio::test]
    async fn download_requires_a_token_and_serves_only_known_blobs() {
        let state = state();
        let (account, blob_id) = seed_blob(&state);

        let anonymous = router(Arc::clone(&state))
            .oneshot(
                Request::builder()
                    .uri(format!("/download/{account}/{blob_id}/file.txt"))
                    .body(Body::empty())
                    .expect("request"),
            )
            .await
            .expect("response");
        assert_eq!(anonymous.status(), StatusCode::UNAUTHORIZED);

        let token = paired_token(&state).await;
        let missing = router(Arc::clone(&state))
            .oneshot(
                Request::builder()
                    .uri(format!("/download/{account}/{}/file.txt", "0".repeat(64)))
                    .header(header::AUTHORIZATION, format!("Bearer {token}"))
                    .body(Body::empty())
                    .expect("request"),
            )
            .await
            .expect("response");
        assert_eq!(missing.status(), StatusCode::NOT_FOUND);

        let response = router(Arc::clone(&state))
            .oneshot(
                Request::builder()
                    .uri(format!("/download/{account}/{blob_id}/file.txt"))
                    .header(header::AUTHORIZATION, format!("Bearer {token}"))
                    .body(Body::empty())
                    .expect("request"),
            )
            .await
            .expect("response");
        assert_eq!(response.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("body");
        assert_eq!(&bytes[..], b"hello blob");
    }

    #[tokio::test]
    async fn upload_stores_bytes_and_download_serves_them() {
        let state = state();
        let account = state
            .store
            .create_account("imap", "me@example.com", None, "secret", &json!({}))
            .expect("account");
        let other = state
            .store
            .create_account("imap", "other@example.com", None, "secret", &json!({}))
            .expect("account");
        let raw = b"Message-ID: <up@example.com>\r\nSubject: hi\r\n\r\nbody";

        let anonymous = router(Arc::clone(&state))
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(format!("/upload/{account}"))
                    .header(header::CONTENT_TYPE, "message/rfc822")
                    .body(Body::from(raw.to_vec()))
                    .expect("request"),
            )
            .await
            .expect("response");
        assert_eq!(anonymous.status(), StatusCode::UNAUTHORIZED);

        let token = paired_token(&state).await;
        let response = router(Arc::clone(&state))
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(format!("/upload/{account}"))
                    .header(header::CONTENT_TYPE, "message/rfc822")
                    .header(header::AUTHORIZATION, format!("Bearer {token}"))
                    .body(Body::from(raw.to_vec()))
                    .expect("request"),
            )
            .await
            .expect("response");
        assert_eq!(response.status(), StatusCode::CREATED);
        let payload = body_json(response).await;
        assert_eq!(payload["accountId"], json!(account.to_string()));
        assert_eq!(payload["type"], json!("message/rfc822"));
        assert_eq!(payload["size"], json!(raw.len()));
        let blob_id = payload["blobId"].as_str().expect("blobId").to_string();

        // The advertised uploadUrl keeps its trailing slash.
        let with_slash = router(Arc::clone(&state))
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(format!("/upload/{account}/"))
                    .header(header::CONTENT_TYPE, "message/rfc822")
                    .header(header::AUTHORIZATION, format!("Bearer {token}"))
                    .body(Body::from(raw.to_vec()))
                    .expect("request"),
            )
            .await
            .expect("response");
        assert_eq!(with_slash.status(), StatusCode::CREATED);

        let download = router(Arc::clone(&state))
            .oneshot(
                Request::builder()
                    .uri(format!("/download/{account}/{blob_id}/message.eml"))
                    .header(header::AUTHORIZATION, format!("Bearer {token}"))
                    .body(Body::empty())
                    .expect("request"),
            )
            .await
            .expect("response");
        assert_eq!(download.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(download.into_body(), usize::MAX)
            .await
            .expect("body");
        assert_eq!(&bytes[..], raw);

        let foreign = router(state)
            .oneshot(
                Request::builder()
                    .uri(format!("/download/{other}/{blob_id}/message.eml"))
                    .header(header::AUTHORIZATION, format!("Bearer {token}"))
                    .body(Body::empty())
                    .expect("request"),
            )
            .await
            .expect("response");
        assert_eq!(foreign.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn upload_rejects_unknown_accounts() {
        let state = state();
        let token = paired_token(&state).await;
        let response = router(state)
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/upload/999")
                    .header(header::CONTENT_TYPE, "message/rfc822")
                    .header(header::AUTHORIZATION, format!("Bearer {token}"))
                    .body(Body::from("Subject: x\r\n\r\n"))
                    .expect("request"),
            )
            .await
            .expect("response");
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn eventsource_streams_state_until_closeafter() {
        let state = state();
        let anonymous = router(Arc::clone(&state))
            .oneshot(
                Request::builder()
                    .uri("/eventsource/?closeafter=1&ping=1")
                    .body(Body::empty())
                    .expect("request"),
            )
            .await
            .expect("response");
        assert_eq!(anonymous.status(), StatusCode::UNAUTHORIZED);

        let token = paired_token(&state).await;
        let response = router(state)
            .oneshot(
                Request::builder()
                    .uri("/eventsource/?types=Email&closeafter=1&ping=1")
                    .header(header::AUTHORIZATION, format!("Bearer {token}"))
                    .body(Body::empty())
                    .expect("request"),
            )
            .await
            .expect("response");
        assert_eq!(response.status(), StatusCode::OK);
        assert!(response
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .is_some_and(|value| value.starts_with("text/event-stream")));
        let bytes = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            axum::body::to_bytes(response.into_body(), usize::MAX),
        )
        .await
        .expect("stream closed")
        .expect("body");
        let text = String::from_utf8_lossy(&bytes);
        assert!(text.contains("ping") || text.contains("StateChange"));
    }

    #[tokio::test]
    async fn healthz_is_anonymous_and_reports_database_state() {
        let state = state();
        seed_account(&state);

        let response = router(state)
            .oneshot(
                Request::builder()
                    .uri("/healthz")
                    .body(Body::empty())
                    .expect("request"),
            )
            .await
            .expect("response");
        assert_eq!(response.status(), StatusCode::OK);
        let payload = body_json(response).await;
        assert_eq!(payload["status"], json!("ok"));
        assert_eq!(payload["database"], json!("ok"));
        assert_eq!(payload["accounts"], json!(1));
        assert_eq!(payload["lastSyncAt"], Value::Null);
        assert_eq!(payload["sync"]["enabled"], json!(true));
        assert_eq!(payload["sync"]["watchers"], json!(0));
    }
}
