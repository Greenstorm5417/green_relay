//! Public REST API surface.
//!
//! This module holds the version-agnostic infrastructure shared by every API
//! version: the request-handling state ([`ApiState`]), the modem port
//! abstraction, API-key authentication and per-key rate limiting middleware,
//! the assembled OpenAPI document, and the top-level [`router`].
//!
//! The concrete endpoints live in versioned submodules. Today that is
//! [`v1`]; a future v2 would be added as a sibling submodule and merged into
//! [`openapi`] and [`router`] alongside it.

use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::{
    Extension, Json, Router,
    extract::{Request, State},
    http::{HeaderMap, HeaderValue, StatusCode, header},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::get,
};
use chrono::Utc;
use tower::ServiceBuilder;
use utoipa::{
    Modify, OpenApi,
    openapi::security::{ApiKey, ApiKeyValue, SecurityScheme},
};
use utoipa_axum::router::OpenApiRouter;

use crate::auth::{
    ApiKeyId, AuthOutcome, FailureTracker, KeyStore, authenticate_identified,
    build_audit_record_with_identifier, key_identifier, passes_guard,
};
use crate::db::Db;
use crate::events::EventBus;
use crate::health::{DEFAULT_RETRY_AFTER_SECS, ModemStatusSnapshot};
use crate::modem::{ModemHandle, SendResult};
use crate::ratelimit::{RateDecision, RateLimiter, effective_limit};

/// Version 1 of the public REST API.
pub mod v1;

/// Default rate limit value.
pub const DEFAULT_RATE_LIMIT: u32 = 100;

/// Default rate limit window in seconds.
pub const DEFAULT_RATE_WINDOW_SECS: u64 = 60;

/// Port interface for modem interactions.
pub trait ModemPort: Send + Sync + 'static {
    /// Retrieves a status snapshot of the modem.
    fn status_snapshot(&self) -> ModemStatusSnapshot;
    /// Sends an SMS message.
    fn send(&self, to: String, body: String) -> impl Future<Output = SendResult> + Send;
}

impl ModemPort for ModemHandle {
    fn status_snapshot(&self) -> ModemStatusSnapshot {
        self.status()
    }

    async fn send(&self, to: String, body: String) -> SendResult {
        self.send_sms(&to, &body).await
    }
}

/// Object-safe bridge over [`ModemPort`] so the API layer can hold a single
/// concrete state type behind dynamic dispatch regardless of which modem
/// implementation is wired in.
pub(crate) trait DynModemPort: Send + Sync + 'static {
    fn status_snapshot(&self) -> ModemStatusSnapshot;
    fn send<'a>(
        &'a self,
        to: String,
        body: String,
    ) -> Pin<Box<dyn Future<Output = SendResult> + Send + 'a>>;
}

impl<M: ModemPort> DynModemPort for M {
    fn status_snapshot(&self) -> ModemStatusSnapshot {
        ModemPort::status_snapshot(self)
    }

    fn send<'a>(
        &'a self,
        to: String,
        body: String,
    ) -> Pin<Box<dyn Future<Output = SendResult> + Send + 'a>> {
        Box::pin(ModemPort::send(self, to, body))
    }
}

/// A shared, type-erased handle to the modem port.
pub(crate) type SharedModem = Arc<dyn DynModemPort>;

/// API shared state configuration.
#[derive(Clone)]
pub struct ApiState {
    /// Database pool handle.
    pub db: Db,
    /// Type-erased modem port instance.
    pub(crate) modem: SharedModem,
    /// Retry after header default value.
    pub retry_after_secs: u64,
    /// Authentication failure tracker.
    pub auth_failures: Arc<Mutex<FailureTracker>>,
    /// Rate limiter instance.
    pub rate_limiter: Arc<Mutex<RateLimiter>>,
    /// Default rate limit threshold.
    pub default_rate_limit: u32,
    /// Default rate limit window.
    pub rate_window: Duration,
    /// Real-time event broadcast bus.
    pub events: EventBus,
}

impl ApiState {
    /// Creates a new ApiState from any [`ModemPort`] implementation.
    pub fn new<M: ModemPort>(db: Db, modem: M) -> Self {
        ApiState {
            db,
            modem: Arc::new(modem),
            retry_after_secs: DEFAULT_RETRY_AFTER_SECS,
            auth_failures: Arc::new(Mutex::new(FailureTracker::new())),
            rate_limiter: Arc::new(Mutex::new(RateLimiter::new())),
            default_rate_limit: DEFAULT_RATE_LIMIT,
            rate_window: Duration::from_secs(DEFAULT_RATE_WINDOW_SECS),
            events: EventBus::default(),
        }
    }

    /// Creates a new ApiState with custom retry after value.
    pub fn with_retry_after<M: ModemPort>(db: Db, modem: M, retry_after_secs: u64) -> Self {
        ApiState {
            retry_after_secs,
            ..ApiState::new(db, modem)
        }
    }

    /// Configures the default rate limits.
    pub fn with_rate_config(mut self, default_rate_limit: u32, rate_window: Duration) -> Self {
        self.default_rate_limit = default_rate_limit;
        self.rate_window = rate_window;
        self
    }

    /// Sets the event bus used to broadcast real-time updates.
    pub fn with_events(mut self, events: EventBus) -> Self {
        self.events = events;
        self
    }
}

pub use crate::error::ApiError;

/// Registers the API-key security scheme on the generated OpenAPI document.
struct SecurityAddon;

impl Modify for SecurityAddon {
    fn modify(&self, openapi: &mut utoipa::openapi::OpenApi) {
        if let Some(components) = openapi.components.as_mut() {
            components.add_security_scheme(
                "api_key",
                SecurityScheme::ApiKey(ApiKey::Header(ApiKeyValue::new("x-api-key"))),
            );
        }
    }
}

/// OpenAPI document for the public REST API.
///
/// Admin dashboard routes are intentionally excluded from the generated spec.
#[derive(OpenApi)]
#[openapi(
    info(
        title = "SMS Microservice API",
        description = "Send and receive SMS through a Waveshare SIM7600X modem, with a real-time event stream."
    ),
    modifiers(&SecurityAddon),
    components(schemas(
        v1::SendRequest,
        v1::SendResponse,
        v1::SyncSendResponse,
        v1::HealthResponse,
        v1::StatusResponse,
        ApiError,
        crate::models::MessageStatus,
        crate::models::InboundMessage,
        crate::models::OutboundMessage,
        crate::events::MessageStatusEvent,
        crate::events::InboundSmsEvent
    )),
    tags(
        (name = "messages", description = "Send and retrieve SMS messages"),
        (name = "events", description = "Real-time Server-Sent Events stream"),
        (name = "health", description = "Service health and modem status")
    )
)]
pub struct ApiDoc;

/// Path at which the generated OpenAPI document is served as JSON.
pub const OPENAPI_JSON_PATH: &str = "/api-docs/openapi.json";

/// Returns the complete generated OpenAPI document, including every public
/// path collected from the route handlers.
///
/// This is the single source of truth shared by the served
/// `/api-docs/openapi.json` endpoint and the `openapi` CLI subcommand, so the
/// deployed docs always match the running service.
pub fn openapi() -> utoipa::openapi::OpenApi {
    let (_router, api): (Router<ApiState>, _) = OpenApiRouter::with_openapi(ApiDoc::openapi())
        .merge(v1::protected_routes())
        .merge(v1::public_routes())
        .split_for_parts();
    api
}

/// Returns the generated OpenAPI document serialized as pretty-printed JSON.
///
/// Used by the `openapi` CLI subcommand and the docs pipeline to emit the spec
/// without starting the HTTP server.
pub fn openapi_json() -> Result<String, serde_json::Error> {
    openapi().to_pretty_json()
}

/// Creates the router containing all API routes plus the OpenAPI document.
///
/// Public REST routes are collected through [`OpenApiRouter`] so the routing
/// table and the generated spec stay in sync; the admin dashboard router is
/// merged separately and is deliberately absent from the OpenAPI document.
pub fn router(state: ApiState) -> Router {
    let protected = v1::protected_routes().route_layer(
        ServiceBuilder::new()
            .layer(middleware::from_fn_with_state(
                state.clone(),
                auth_middleware,
            ))
            .layer(middleware::from_fn_with_state(
                state.clone(),
                rate_limit_middleware,
            )),
    );

    let (router, api) = OpenApiRouter::with_openapi(ApiDoc::openapi())
        .merge(protected)
        .merge(v1::public_routes())
        .split_for_parts();

    router
        .route(
            OPENAPI_JSON_PATH,
            get(move || {
                let api = api.clone();
                async move { Json(api) }
            }),
        )
        .with_state(state)
}

#[derive(Debug, Clone)]
struct AuthContext {
    custom_rate_limit: Option<u32>,
    identifier: String,
}

struct ResolvedStore {
    id: Option<ApiKeyId>,
}

impl KeyStore for ResolvedStore {
    fn lookup_active(&self, _key_identifier: &str) -> Option<ApiKeyId> {
        self.id
    }
}

fn extract_api_key(headers: &HeaderMap) -> Option<String> {
    if let Some(value) = headers.get("x-api-key").and_then(|v| v.to_str().ok())
        && !value.is_empty()
    {
        return Some(value.to_string());
    }
    if let Some(value) = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
    {
        let token = value
            .strip_prefix("Bearer ")
            .or_else(|| value.strip_prefix("bearer "))
            .unwrap_or(value);
        return Some(token.to_string());
    }
    None
}

fn unauthorized_response() -> Response {
    (
        StatusCode::UNAUTHORIZED,
        Json(ApiError::new("unauthorized", Vec::new())),
    )
        .into_response()
}

async fn auth_middleware(
    State(state): State<ApiState>,
    mut request: Request,
    next: Next,
) -> Response {
    let now = Instant::now();
    let timestamp = Utc::now();
    let presented = extract_api_key(request.headers()).unwrap_or_default();

    if !passes_guard(&presented) {
        emit_auth_audit(
            &key_identifier(&presented),
            &AuthOutcome::Unauthorized,
            timestamp,
        );
        return unauthorized_response();
    }

    let identifier = key_identifier(&presented);

    {
        let tracker = state
            .auth_failures
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if tracker.is_locked(&identifier, now) {
            emit_auth_audit(&identifier, &AuthOutcome::LockedOut, timestamp);
            return unauthorized_response();
        }
    }

    let resolved = match state.db.lookup_active_key(&identifier).await {
        Ok(resolved) => resolved,
        Err(_) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ApiError::new("internal server error", Vec::new())),
            )
                .into_response();
        }
    };

    let store = ResolvedStore {
        id: resolved.map(|(id, _)| id),
    };
    let outcome = {
        let mut tracker = state
            .auth_failures
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        authenticate_identified(&identifier, &store, &mut tracker, now)
    };

    emit_auth_audit(&identifier, &outcome, timestamp);

    match outcome {
        AuthOutcome::Authorized(_id) => {
            let custom_rate_limit = resolved.and_then(|(_, custom)| custom);
            request.extensions_mut().insert(AuthContext {
                custom_rate_limit,
                identifier,
            });
            next.run(request).await
        }
        AuthOutcome::Unauthorized | AuthOutcome::LockedOut => unauthorized_response(),
    }
}

fn emit_auth_audit(identifier: &str, outcome: &AuthOutcome, timestamp: chrono::DateTime<Utc>) {
    let record = build_audit_record_with_identifier(identifier.to_string(), outcome, timestamp);
    tracing::info!(
        event_type = record.event_type,
        result = ?record.result,
        key_identifier = %record.key_identifier,
        "api key authentication attempt"
    );
}

async fn rate_limit_middleware(
    State(state): State<ApiState>,
    Extension(ctx): Extension<AuthContext>,
    request: Request,
    next: Next,
) -> Response {
    let now = Instant::now();
    let (limit, _config_err) = effective_limit(ctx.custom_rate_limit, state.default_rate_limit);

    let decision = {
        let mut limiter = state
            .rate_limiter
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        limiter.check(&ctx.identifier, limit, state.rate_window, now)
    };

    match decision {
        RateDecision::Allow { .. } => next.run(request).await,
        RateDecision::Reject { retry_after_secs } => json_with_retry_after(
            StatusCode::TOO_MANY_REQUESTS,
            retry_after_secs,
            ApiError::new("rate limit exceeded", Vec::new()),
        ),
    }
}

/// Builds a JSON error response carrying a `Retry-After` header.
pub(crate) fn json_with_retry_after(
    status: StatusCode,
    retry_after_secs: u64,
    body: ApiError,
) -> Response {
    let mut response = (status, Json(body)).into_response();
    if let Ok(value) = HeaderValue::from_str(&retry_after_secs.to_string()) {
        response.headers_mut().insert(header::RETRY_AFTER, value);
    }
    response
}

#[cfg(test)]
pub(crate) mod testutil {
    use axum::body::Body;
    use axum::extract::Request;
    use chrono::Utc;

    use crate::auth::key_identifier;
    use crate::db::Db;
    use crate::health::{ModemStatusSnapshot, SimStatus};
    use crate::models::MessageStatus;
    use crate::modem::SendResult;

    use super::v1::SendRequest;
    use super::{ApiState, ModemPort};

    #[derive(Clone)]
    pub(crate) struct StubModem {
        pub snapshot: ModemStatusSnapshot,
        pub result: SendResult,
    }

    impl ModemPort for StubModem {
        fn status_snapshot(&self) -> ModemStatusSnapshot {
            self.snapshot.clone()
        }

        async fn send(&self, _to: String, _body: String) -> SendResult {
            self.result.clone()
        }
    }

    pub(crate) fn healthy_snapshot() -> ModemStatusSnapshot {
        ModemStatusSnapshot {
            serial_connected: true,
            sim_status: SimStatus::Ready,
            registered: true,
            responsive: true,
            signal_percent: Some(75),
            operator: Some("Carrier".to_string()),
        }
    }

    pub(crate) fn down_snapshot() -> ModemStatusSnapshot {
        ModemStatusSnapshot {
            serial_connected: false,
            sim_status: SimStatus::Unknown,
            registered: false,
            responsive: false,
            signal_percent: None,
            operator: None,
        }
    }

    pub(crate) fn queued_result() -> SendResult {
        SendResult {
            status: MessageStatus::Queued,
            reference: None,
            error_code: None,
            error: None,
        }
    }

    pub(crate) async fn ready_db() -> Db {
        let db = Db::connect_in_memory().await.unwrap();
        db.run_migrations().await.unwrap();
        db
    }

    pub(crate) fn request(to: Option<&str>, body: Option<&str>) -> SendRequest {
        SendRequest {
            to: to.map(|s| s.to_string()),
            body: body.map(|s| s.to_string()),
        }
    }

    pub(crate) async fn insert_key(db: &Db, plaintext: &str, custom: Option<i64>) -> i64 {
        let ident = key_identifier(plaintext);
        let hash = format!("hash-{ident}");
        let result = sqlx::query(
            "INSERT INTO api_keys (key_hash, key_identifier, custom_rate_limit, revoked, created_at) \
             VALUES (?, ?, ?, 0, ?)",
        )
        .bind(&hash)
        .bind(&ident)
        .bind(custom)
        .bind(Utc::now().to_rfc3339())
        .execute(db.pool())
        .await
        .unwrap();
        result.last_insert_rowid()
    }

    pub(crate) async fn revoke_key(db: &Db, id: i64) {
        sqlx::query("UPDATE api_keys SET revoked = 1 WHERE id = ?")
            .bind(id)
            .execute(db.pool())
            .await
            .unwrap();
    }

    pub(crate) fn test_state(db: Db) -> ApiState {
        ApiState::new(
            db,
            StubModem {
                snapshot: healthy_snapshot(),
                result: queued_result(),
            },
        )
    }

    pub(crate) fn get_request(path: &str, api_key: Option<&str>) -> Request {
        let mut builder = axum::http::Request::builder().method("GET").uri(path);
        if let Some(key) = api_key {
            builder = builder.header("x-api-key", key);
        }
        builder.body(Body::empty()).unwrap()
    }
}

#[cfg(test)]
mod tests {
    use super::testutil::{get_request, insert_key, ready_db, revoke_key, test_state};
    use super::*;
    use tower::util::ServiceExt;

    #[test]
    fn openapi_document_includes_every_public_path() {
        let json = openapi_json().expect("serialize openapi");
        let value: serde_json::Value = serde_json::from_str(&json).expect("valid json");
        let paths = value
            .get("paths")
            .and_then(|p| p.as_object())
            .expect("paths object present");
        for path in [
            "/api/v1/messages",
            "/api/v1/messages/sync",
            "/api/v1/messages/inbound",
            "/api/v1/messages/{id}",
            "/api/v1/events",
            "/health",
            "/status",
        ] {
            assert!(paths.contains_key(path), "spec is missing path {path}");
        }
    }

    #[tokio::test]
    async fn protected_endpoint_without_key_is_401() {
        let db = ready_db().await;
        let app = router(test_state(db));

        let resp = app
            .oneshot(get_request("/api/v1/messages/inbound", None))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn protected_endpoint_with_unknown_key_is_401() {
        let db = ready_db().await;
        let app = router(test_state(db));

        let resp = app
            .oneshot(get_request("/api/v1/messages/inbound", Some("no-such-key")))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn protected_endpoint_with_revoked_key_is_401() {
        let db = ready_db().await;
        let id = insert_key(&db, "revoked-key", None).await;
        revoke_key(&db, id).await;
        let app = router(test_state(db));

        let resp = app
            .oneshot(get_request("/api/v1/messages/inbound", Some("revoked-key")))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn protected_endpoint_with_active_key_proceeds() {
        let db = ready_db().await;
        insert_key(&db, "good-key", None).await;
        let app = router(test_state(db));

        let resp = app
            .oneshot(get_request("/api/v1/messages/inbound", Some("good-key")))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn health_and_status_are_unauthenticated() {
        let db = ready_db().await;
        let app = router(test_state(db));

        let health = app
            .clone()
            .oneshot(get_request("/health", None))
            .await
            .unwrap();
        assert_eq!(health.status(), StatusCode::OK);

        let status = app.oneshot(get_request("/status", None)).await.unwrap();
        assert_eq!(status.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn rate_limit_rejects_over_limit_with_retry_after() {
        let db = ready_db().await;
        insert_key(&db, "limited-key", None).await;

        let state = test_state(db).with_rate_config(1, Duration::from_secs(60));
        let app = router(state);

        let first = app
            .clone()
            .oneshot(get_request("/api/v1/messages/inbound", Some("limited-key")))
            .await
            .unwrap();
        assert_eq!(first.status(), StatusCode::OK);

        let second = app
            .oneshot(get_request("/api/v1/messages/inbound", Some("limited-key")))
            .await
            .unwrap();
        assert_eq!(second.status(), StatusCode::TOO_MANY_REQUESTS);

        let retry_after = second
            .headers()
            .get(header::RETRY_AFTER)
            .expect("Retry-After header present on 429");
        let secs: u64 = retry_after.to_str().unwrap().parse().unwrap();
        assert!((1..=60).contains(&secs));
    }

    #[tokio::test]
    async fn rate_limit_is_isolated_per_key() {
        let db = ready_db().await;
        insert_key(&db, "key-a", None).await;
        insert_key(&db, "key-b", None).await;
        let state = test_state(db).with_rate_config(1, Duration::from_secs(60));
        let app = router(state);

        let a1 = app
            .clone()
            .oneshot(get_request("/api/v1/messages/inbound", Some("key-a")))
            .await
            .unwrap();
        assert_eq!(a1.status(), StatusCode::OK);
        let a2 = app
            .clone()
            .oneshot(get_request("/api/v1/messages/inbound", Some("key-a")))
            .await
            .unwrap();
        assert_eq!(a2.status(), StatusCode::TOO_MANY_REQUESTS);

        let b1 = app
            .oneshot(get_request("/api/v1/messages/inbound", Some("key-b")))
            .await
            .unwrap();
        assert_eq!(b1.status(), StatusCode::OK);
    }

    #[test]
    fn extract_api_key_prefers_x_api_key_then_bearer() {
        let mut headers = HeaderMap::new();
        assert_eq!(extract_api_key(&headers), None);

        headers.insert(header::AUTHORIZATION, "Bearer abc123".parse().unwrap());
        assert_eq!(extract_api_key(&headers), Some("abc123".to_string()));

        headers.insert("x-api-key", "xyz789".parse().unwrap());
        assert_eq!(extract_api_key(&headers), Some("xyz789".to_string()));
    }
}
