//! Optional Hub: a local HTTP service that exposes the synced mailboxes over a
//! JMAP subset. Desktops talk to `mail-core` directly; thin clients pair with
//! the Hub and use JMAP.
//!
//! Configuration is environment-only:
//! - `EYRIE_HUB_DB` (default `hub.db`)
//! - `EYRIE_HUB_BIND` (default `127.0.0.1:8080`)
//! - `EYRIE_DEV_SECRET_DIR` (development file store; the keychain is default)
//! - `EYRIE_HUB_PAIRING_CODE` (fixed pairing code; otherwise one is generated
//!   per process and printed to stdout)

use std::future::Future;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::State;
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use mail_api::{BodySource, FetchedBody, Jmap, RequestError};
use mail_core::Core;
use mail_crypto::{FileStore, KeyringStore, SecretStore};
use mail_store::{Device, Store};
use rand::RngCore;
use serde::Deserialize;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use tracing::{info, warn};

const SECRET_SERVICE: &str = "mail-client-dev";

pub struct AppState {
    jmap: Arc<Jmap>,
    store: Arc<Store>,
    pairing_code: String,
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

    let core = Arc::new(Core::open(&db, secret_store()?)?);
    let store = core.store();
    let jmap = Jmap::new(Arc::clone(&store)).with_bodies(Arc::new(CoreBodies {
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
    let state = Arc::new(AppState {
        jmap: Arc::new(jmap),
        store,
        pairing_code,
    });

    let listener = tokio::net::TcpListener::bind(bind).await?;
    info!(%bind, %db, "hub listening");
    axum::serve(listener, router(state)).await?;
    Ok(())
}

/// Routes are split out so tests can drive the same handlers without a socket.
fn router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/.well-known/jmap", get(session))
        .route("/jmap", post(jmap))
        .route("/pair", post(pair))
        .route("/download/{account_id}/{blob_id}/{name}", get(unsupported))
        .route("/upload/{account_id}", post(unsupported))
        .route("/eventsource/", get(unsupported))
        .with_state(state)
}

async fn session(State(state): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    if let Err(err) = authenticate(&headers, &state) {
        return auth_error(err);
    }
    match state.jmap.session() {
        Ok(value) => Json(value).into_response(),
        Err(err) => server_error(err),
    }
}

async fn jmap(State(state): State<Arc<AppState>>, headers: HeaderMap, body: Bytes) -> Response {
    if let Err(err) = authenticate(&headers, &state) {
        return auth_error(err);
    }
    let request: Value = match serde_json::from_slice(&body) {
        Ok(value) => value,
        Err(err) => return problem(StatusCode::BAD_REQUEST, "notJSON", &err.to_string()),
    };
    match state.jmap.handle(request).await {
        Ok(value) => Json(value).into_response(),
        Err(err) => request_error(err),
    }
}

#[derive(Deserialize)]
struct PairRequest {
    code: String,
    name: Option<String>,
}

async fn pair(State(state): State<Arc<AppState>>, Json(request): Json<PairRequest>) -> Response {
    if !constant_time_eq(request.code.trim(), &state.pairing_code) {
        return problem(
            StatusCode::FORBIDDEN,
            "urn:ietf:params:jmap:error:forbidden",
            "pairing code does not match",
        );
    }
    let token = random_token();
    let name = request
        .name
        .map(|name| name.trim().to_string())
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| "device".to_string());
    match state.store.create_device(&name, &token_hash(&token)) {
        Ok(device_id) => {
            info!(device_id, %name, "device paired");
            (
                StatusCode::CREATED,
                Json(json!({ "token": token, "device": name })),
            )
                .into_response()
        }
        Err(err) => server_error(err),
    }
}

/// Push and blob transfer are not implemented yet; answer explicitly instead
/// of leaving clients to guess from a 404.
async fn unsupported() -> Response {
    problem(
        StatusCode::NOT_IMPLEMENTED,
        "urn:ietf:params:jmap:error:serverFail",
        "this Hub endpoint is not implemented",
    )
}

enum AuthError {
    MissingToken,
    UnknownToken,
    Store(mail_store::StoreError),
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

fn secret_store() -> anyhow::Result<Arc<dyn SecretStore>> {
    match std::env::var("EYRIE_DEV_SECRET_DIR") {
        Ok(dir) => Ok(Arc::new(FileStore::new(dir)?)),
        Err(_) => Ok(Arc::new(KeyringStore::new(SECRET_SERVICE))),
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::Request;
    use mail_store::DbKey;
    use tower::ServiceExt;

    fn state() -> Arc<AppState> {
        let key = DbKey::from_bytes(&[9u8; 32]).expect("key");
        let store = Arc::new(Store::open_in_memory(&key).expect("store"));
        let jmap = Arc::new(Jmap::new(Arc::clone(&store)));
        Arc::new(AppState {
            jmap,
            store,
            pairing_code: "pair-me".to_string(),
        })
    }

    async fn body_json(response: Response) -> Value {
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("body");
        serde_json::from_slice(&bytes).expect("json")
    }

    async fn pair_with(state: &Arc<AppState>, code: &str) -> Response {
        router(Arc::clone(state))
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/pair")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(format!(r#"{{"code":"{code}","name":"phone"}}"#)))
                    .expect("request"),
            )
            .await
            .expect("response")
    }

    async fn paired_token(state: &Arc<AppState>) -> String {
        body_json(pair_with(state, "pair-me").await).await["token"]
            .as_str()
            .expect("token")
            .to_string()
    }

    #[tokio::test]
    async fn pairing_checks_the_code_and_stores_only_a_hash() {
        let state = state();
        let wrong = pair_with(&state, "nope").await;
        assert_eq!(wrong.status(), StatusCode::FORBIDDEN);

        let response = pair_with(&state, "pair-me").await;
        assert_eq!(response.status(), StatusCode::CREATED);
        let payload = body_json(response).await;
        let token = payload["token"].as_str().expect("token").to_string();
        assert!(state
            .store
            .device_by_token_hash(&token_hash(&token))
            .expect("lookup")
            .is_some());
        assert!(state
            .store
            .device_by_token_hash(&token)
            .expect("lookup")
            .is_none());
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
        let token = paired_token(&state).await;
        let response = router(state)
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/jmap")
                    .header(header::CONTENT_TYPE, "application/json")
                    .header(header::AUTHORIZATION, format!("Bearer {token}"))
                    .body(Body::from(
                        r#"{"using":["urn:ietf:params:jmap:core"],"methodCalls":[["Mailbox/get",{"accountId":"1"},"c0"]]}"#,
                    ))
                    .expect("request"),
            )
            .await
            .expect("response");
        assert_eq!(response.status(), StatusCode::OK);
        let payload = body_json(response).await;
        assert_eq!(
            payload["methodResponses"][0][1]["type"],
            json!("accountNotFound")
        );
    }
}
