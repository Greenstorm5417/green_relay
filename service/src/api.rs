//! REST API: Axum handlers for send, inbound, status, health endpoints.
//! Validates requests, runs deliverability gate, persists messages, dispatches
//! to Modem Manager. Authentication & rate-limit layers via middleware.
//! ModemPort trait enables testing with stubs.

use std::future::Future;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::{
    Extension, Json, Router,
    extract::{Path, Request, State, rejection::JsonRejection},
    http::{HeaderMap, HeaderValue, StatusCode, header},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use tower::ServiceBuilder;

use crate::auth::{
    ApiKeyId, AuthOutcome, FailureTracker, KeyStore, authenticate_identified,
    build_audit_record_with_identifier, key_identifier, passes_guard,
};
use crate::db::Db;
use crate::health::{
    DEFAULT_RETRY_AFTER_SECS, DeliverabilityOutcome, ModemStatusSnapshot, ServiceHealth, SimStatus,
    deliverability_gate, derive_health,
};
use crate::models::MessageStatus;
use crate::modem::{ModemHandle, SendResult};
use crate::ratelimit::{RateDecision, RateLimiter, effective_limit};
use crate::sms::{
    SegmentError, ValidationError, check_required_fields, segment_message, validate_body,
    validate_e164,
};

/// Default per-key request limit when a key defines no custom limit (Req 4.1).
pub const DEFAULT_RATE_LIMIT: u32 = 100;

/// Default rate-limit window length in seconds (Req 4.1, 4.5).
pub const DEFAULT_RATE_WINDOW_SECS: u64 = 60;

// ---------------------------------------------------------------------------
// Modem access abstraction
// ---------------------------------------------------------------------------

/// Abstraction over Modem Manager for testability.
pub trait ModemPort: Clone + Send + Sync + 'static {
    fn status_snapshot(&self) -> ModemStatusSnapshot;
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

// ---------------------------------------------------------------------------
// Shared handler state
// ---------------------------------------------------------------------------

/// Shared state for REST API handlers. Cloning is cheap (internal Arcs).
#[derive(Clone)]
pub struct ApiState<M: ModemPort> {
    pub db: Db,
    pub modem: M,
    pub retry_after_secs: u64,
    pub auth_failures: Arc<Mutex<FailureTracker>>,
    pub rate_limiter: Arc<Mutex<RateLimiter>>,
    pub default_rate_limit: u32,
    pub rate_window: Duration,
}

impl<M: ModemPort> ApiState<M> {
    pub fn new(db: Db, modem: M) -> Self {
        ApiState {
            db,
            modem,
            retry_after_secs: DEFAULT_RETRY_AFTER_SECS,
            auth_failures: Arc::new(Mutex::new(FailureTracker::new())),
            rate_limiter: Arc::new(Mutex::new(RateLimiter::new())),
            default_rate_limit: DEFAULT_RATE_LIMIT,
            rate_window: Duration::from_secs(DEFAULT_RATE_WINDOW_SECS),
        }
    }

    pub fn with_retry_after(db: Db, modem: M, retry_after_secs: u64) -> Self {
        ApiState {
            retry_after_secs,
            ..ApiState::new(db, modem)
        }
    }

    pub fn with_rate_config(mut self, default_rate_limit: u32, rate_window: Duration) -> Self {
        self.default_rate_limit = default_rate_limit;
        self.rate_window = rate_window;
        self
    }
}

// ---------------------------------------------------------------------------
// Wire types
// ---------------------------------------------------------------------------

/// Send request body.
#[derive(Debug, Clone, Deserialize)]
pub struct SendRequest {
    pub to: Option<String>,
    pub body: Option<String>,
}

/// Acceptance response.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SendResponse {
    pub id: i64,
    pub status: MessageStatus,
    pub parts: u8,
}

/// Client-facing error.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ApiError {
    pub error: String,
    pub fields: Vec<String>,
}

impl ApiError {
    fn new(error: impl Into<String>, fields: Vec<String>) -> Self {
        ApiError {
            error: error.into(),
            fields,
        }
    }
}

/// Health endpoint response.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct HealthResponse {
    pub health: &'static str,
    pub serial_connected: bool,
    pub sim_status: &'static str,
}

/// Status endpoint response.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct StatusResponse {
    pub signal_percent: Option<u8>,
    pub registered: Option<bool>,
    pub operator: Option<String>,
    pub unavailable: Vec<String>,
}

// ---------------------------------------------------------------------------
// Router
// ---------------------------------------------------------------------------

/// Build REST API router with protected (auth + rate-limit) and public endpoints.
pub fn router<M: ModemPort>(state: ApiState<M>) -> Router {
    let protected = Router::new()
        .route("/api/v1/messages", post(send_handler::<M>))
        .route("/api/v1/messages/inbound", get(inbound_handler::<M>))
        .route("/api/v1/messages/{id}", get(outbound_status_handler::<M>))
        .route_layer(
            ServiceBuilder::new()
                .layer(middleware::from_fn_with_state(
                    state.clone(),
                    auth_middleware::<M>,
                ))
                .layer(middleware::from_fn_with_state(
                    state.clone(),
                    rate_limit_middleware::<M>,
                )),
        )
        .with_state(state.clone());

    let public = Router::new()
        .route("/health", get(health_handler::<M>))
        .route("/status", get(status_handler::<M>))
        .with_state(state);

    protected.merge(public)
}

// -- Middleware: auth & rate-limit

/// Context from auth layer for rate-limit layer.
#[derive(Debug, Clone)]
struct AuthContext {
    custom_rate_limit: Option<u32>,
    identifier: String,
}

/// KeyStore adapter for pre-resolved key lookups.
struct ResolvedStore {
    id: Option<ApiKeyId>,
}

impl KeyStore for ResolvedStore {
    fn lookup_active(&self, _key_identifier: &str) -> Option<ApiKeyId> {
        self.id
    }
}

/// Extract API key from x-api-key header or Bearer token.
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

/// Look up active, non-revoked key by identifier.
async fn lookup_active_key(
    db: &Db,
    identifier: &str,
) -> Result<Option<(ApiKeyId, Option<u32>)>, crate::db::DbError> {
    use sqlx::Row;

    let row = sqlx::query(
        "SELECT id, custom_rate_limit FROM api_keys \
           WHERE key_identifier = ? AND revoked = 0",
    )
    .bind(identifier)
    .fetch_optional(db.pool())
    .await?;

    match row {
        Some(row) => {
            let id: ApiKeyId = row.try_get("id")?;
            let custom: Option<i64> = row.try_get("custom_rate_limit")?;
            Ok(Some((id, custom.map(|c| c as u32))))
        }
        None => Ok(None),
    }
}

/// Build 401 response.
fn unauthorized_response() -> Response {
    (
        StatusCode::UNAUTHORIZED,
        Json(ApiError::new("unauthorized", Vec::new())),
    )
        .into_response()
}

/// API-key authentication middleware.
async fn auth_middleware<M: ModemPort>(
    State(state): State<ApiState<M>>,
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

    let resolved = match lookup_active_key(&state.db, &identifier).await {
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

/// Emit structured audit record (never logs plaintext key).
fn emit_auth_audit(identifier: &str, outcome: &AuthOutcome, timestamp: chrono::DateTime<Utc>) {
    let record = build_audit_record_with_identifier(identifier.to_string(), outcome, timestamp);
    tracing::info!(
        event_type = record.event_type,
        result = ?record.result,
        key_identifier = %record.key_identifier,
        "api key authentication attempt"
    );
}

/// Per-key rate-limit middleware.
async fn rate_limit_middleware<M: ModemPort>(
    State(state): State<ApiState<M>>,
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

// -- Send handler

/// Outcome of preparing and queuing a send.
#[derive(Debug, Clone, PartialEq, Eq)]
enum SendDecision {
    Accepted(SendResponse),
    Invalid(ValidationError),
    TooManyParts { required: usize },
    Gated { retry_after_secs: u64 },
    NotReady { retry_after_secs: u64 },
    ServerError,
}

impl IntoResponse for SendDecision {
    fn into_response(self) -> Response {
        match self {
            SendDecision::Accepted(resp) => (StatusCode::ACCEPTED, Json(resp)).into_response(),
            SendDecision::Invalid(err) => {
                let (status, body) = validation_response(&err);
                (status, Json(body)).into_response()
            }
            SendDecision::TooManyParts { required } => (
                StatusCode::BAD_REQUEST,
                Json(ApiError::new(
                    format!("message requires {required} parts which exceeds the maximum allowed"),
                    vec!["body".to_string()],
                )),
            )
                .into_response(),
            SendDecision::Gated { retry_after_secs } => json_with_retry_after(
                StatusCode::SERVICE_UNAVAILABLE,
                retry_after_secs,
                ApiError::new(
                    "the modem cannot deliver messages right now; retry later",
                    Vec::new(),
                ),
            ),
            SendDecision::NotReady { retry_after_secs } => json_with_retry_after(
                StatusCode::SERVICE_UNAVAILABLE,
                retry_after_secs,
                ApiError::new("the service is not ready to accept messages", Vec::new()),
            ),
            SendDecision::ServerError => (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ApiError::new("internal server error", Vec::new())),
            )
                .into_response(),
        }
    }
}

/// POST /api/v1/messages
async fn send_handler<M: ModemPort>(
    State(state): State<ApiState<M>>,
    body: Result<Json<SendRequest>, JsonRejection>,
) -> Response {
    let Json(request) = match body {
        Ok(json) => json,
        Err(_) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(ApiError::new("request body must be valid JSON", Vec::new())),
            )
                .into_response();
        }
    };
    queue_send(&state, &request).await.into_response()
}

/// Validate, gate, persist queued message, dispatch to modem.
async fn queue_send<M: ModemPort>(state: &ApiState<M>, request: &SendRequest) -> SendDecision {
    let (Some(to), Some(body)) = (request.to.as_deref(), request.body.as_deref()) else {
        let err = check_required_fields(request.to.as_deref(), request.body.as_deref())
            .err()
            .unwrap_or(ValidationError::MissingFields(Vec::new()));
        return SendDecision::Invalid(err);
    };

    if let Err(err) = validate_e164(to) {
        return SendDecision::Invalid(err);
    }
    if let Err(err) = validate_body(body) {
        return SendDecision::Invalid(err);
    }

    let parts = match segment_message(body) {
        Ok(segments) => segments.len() as u8,
        Err(SegmentError::TooManyParts { required }) => {
            return SendDecision::TooManyParts { required };
        }
    };

    let snapshot = state.modem.status_snapshot();
    if let DeliverabilityOutcome::Rejected { retry_after_secs } =
        deliverability_gate(&snapshot, state.retry_after_secs)
    {
        return SendDecision::Gated { retry_after_secs };
    }

    let record = match state
        .db
        .create_outbound_message(to, body, MessageStatus::Queued, parts)
        .await
    {
        Ok(record) => record,
        Err(err) if err.is_not_ready() => {
            return SendDecision::NotReady {
                retry_after_secs: state.retry_after_secs,
            };
        }
        Err(_) => return SendDecision::ServerError,
    };

    let id = record.id;
    let db = state.db.clone();
    let modem = state.modem.clone();
    let to_owned = to.to_string();
    let body_owned = body.to_string();
    tokio::spawn(async move {
        dispatch_send(&db, &modem, id, to_owned, body_owned).await;
    });

    SendDecision::Accepted(SendResponse {
        id,
        status: MessageStatus::Queued,
        parts,
    })
}

/// Dispatch send, reconcile result (sent/failed/queued).
async fn dispatch_send<M: ModemPort>(db: &Db, modem: &M, id: i64, to: String, body: String) {
    let result = modem.send(to, body).await;
    match result.status {
        MessageStatus::Sent => {
            let reference = result.reference.map(|r| r.to_string());
            let _ = db
                .update_outbound_message(id, MessageStatus::Sent, reference.as_deref(), None)
                .await;
        }
        MessageStatus::Failed => {
            let detail = result
                .error_code
                .map(|code| code.to_string())
                .or(result.error);
            let _ = db
                .update_outbound_message(id, MessageStatus::Failed, None, detail.as_deref())
                .await;
        }
        MessageStatus::Queued => {}
    }
}

// -- Inbound & outbound status

/// GET /api/v1/messages/inbound
async fn inbound_handler<M: ModemPort>(State(state): State<ApiState<M>>) -> Response {
    match state.db.list_inbound_messages().await {
        Ok(messages) => (StatusCode::OK, Json(messages)).into_response(),
        Err(err) => db_error_response(&err, state.retry_after_secs),
    }
}

/// GET /api/v1/messages/{id}
async fn outbound_status_handler<M: ModemPort>(
    State(state): State<ApiState<M>>,
    Path(id): Path<i64>,
) -> Response {
    match state.db.get_outbound_message(id).await {
        Ok(Some(message)) => (StatusCode::OK, Json(message)).into_response(),
        Ok(None) => (
            StatusCode::NOT_FOUND,
            Json(ApiError::new("message not found", Vec::new())),
        )
            .into_response(),
        Err(err) => db_error_response(&err, state.retry_after_secs),
    }
}

// -- Health & status

/// GET /health
async fn health_handler<M: ModemPort>(State(state): State<ApiState<M>>) -> Response {
    let snapshot = state.modem.status_snapshot();
    let (status, body) = build_health_response(&snapshot);
    (status, Json(body)).into_response()
}

/// GET /status
async fn status_handler<M: ModemPort>(State(state): State<ApiState<M>>) -> Response {
    let snapshot = state.modem.status_snapshot();
    (StatusCode::OK, Json(build_status_response(&snapshot))).into_response()
}

/// Build health response from snapshot.
fn build_health_response(snapshot: &ModemStatusSnapshot) -> (StatusCode, HealthResponse) {
    let health = derive_health(snapshot);
    let status = match health {
        ServiceHealth::Unhealthy => StatusCode::SERVICE_UNAVAILABLE,
        ServiceHealth::Healthy | ServiceHealth::Degraded => StatusCode::OK,
    };
    let body = HealthResponse {
        health: health_str(health),
        serial_connected: snapshot.serial_connected,
        sim_status: sim_str(snapshot.sim_status),
    };
    (status, body)
}

/// Build status response, marking unavailable commands.
///
/// Registration unavailable when modem unresponsive; signal/operator from
/// snapshot optionals.
fn build_status_response(snapshot: &ModemStatusSnapshot) -> StatusResponse {
    let mut unavailable = Vec::new();

    let signal_percent = snapshot.signal_percent;
    if signal_percent.is_none() {
        unavailable.push("signal".to_string());
    }

    let registered = if snapshot.responsive {
        Some(snapshot.registered)
    } else {
        None
    };
    if registered.is_none() {
        unavailable.push("registration".to_string());
    }

    let operator = snapshot.operator.clone();
    if operator.is_none() {
        unavailable.push("operator".to_string());
    }

    StatusResponse {
        signal_percent,
        registered,
        operator,
        unavailable,
    }
}

// -- Response helpers

fn health_str(health: ServiceHealth) -> &'static str {
    match health {
        ServiceHealth::Healthy => "healthy",
        ServiceHealth::Degraded => "degraded",
        ServiceHealth::Unhealthy => "unhealthy",
    }
}

fn sim_str(sim: SimStatus) -> &'static str {
    match sim {
        SimStatus::Ready => "ready",
        SimStatus::NotReady => "not_ready",
        SimStatus::Unknown => "unknown",
    }
}

fn validation_response(err: &ValidationError) -> (StatusCode, ApiError) {
    let body = match err {
        ValidationError::MissingFields(fields) => {
            ApiError::new("missing required fields", fields.clone())
        }
        ValidationError::InvalidPhoneNumber => ApiError::new(
            "phone number is not a valid E.164 number",
            vec!["to".to_string()],
        ),
        ValidationError::BodyEmpty => {
            ApiError::new("message body must not be empty", vec!["body".to_string()])
        }
        ValidationError::BodyTooLong => ApiError::new(
            "message body exceeds the maximum allowed length",
            vec!["body".to_string()],
        ),
    };
    (StatusCode::BAD_REQUEST, body)
}

fn db_error_response(err: &crate::db::DbError, retry_after_secs: u64) -> Response {
    if err.is_not_ready() {
        json_with_retry_after(
            StatusCode::SERVICE_UNAVAILABLE,
            retry_after_secs,
            ApiError::new("the service is not ready", Vec::new()),
        )
    } else {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ApiError::new("internal server error", Vec::new())),
        )
            .into_response()
    }
}

fn json_with_retry_after(status: StatusCode, retry_after_secs: u64, body: ApiError) -> Response {
    let mut response = (status, Json(body)).into_response();
    if let Ok(value) = HeaderValue::from_str(&retry_after_secs.to_string()) {
        response.headers_mut().insert(header::RETRY_AFTER, value);
    }
    response
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Stub Modem Manager with fixed snapshot & canned result.
    #[derive(Clone)]
    struct StubModem {
        snapshot: ModemStatusSnapshot,
        result: SendResult,
    }

    impl ModemPort for StubModem {
        fn status_snapshot(&self) -> ModemStatusSnapshot {
            self.snapshot.clone()
        }

        async fn send(&self, _to: String, _body: String) -> SendResult {
            self.result.clone()
        }
    }

    fn healthy_snapshot() -> ModemStatusSnapshot {
        ModemStatusSnapshot {
            serial_connected: true,
            sim_status: SimStatus::Ready,
            registered: true,
            responsive: true,
            signal_percent: Some(75),
            operator: Some("Carrier".to_string()),
        }
    }

    fn down_snapshot() -> ModemStatusSnapshot {
        ModemStatusSnapshot {
            serial_connected: false,
            sim_status: SimStatus::Unknown,
            registered: false,
            responsive: false,
            signal_percent: None,
            operator: None,
        }
    }

    fn queued_result() -> SendResult {
        // Leaves the record queued (Req 10.5) so post-send state is deterministic.
        SendResult {
            status: MessageStatus::Queued,
            reference: None,
            error_code: None,
            error: None,
        }
    }

    async fn ready_db() -> Db {
        let db = Db::connect_in_memory().await.unwrap();
        db.run_migrations().await.unwrap();
        db
    }

    fn request(to: Option<&str>, body: Option<&str>) -> SendRequest {
        SendRequest {
            to: to.map(|s| s.to_string()),
            body: body.map(|s| s.to_string()),
        }
    }

    #[tokio::test]
    async fn send_accepts_valid_request_and_persists_queued() {
        let db = ready_db().await;
        let modem = StubModem {
            snapshot: healthy_snapshot(),
            result: queued_result(),
        };
        let state = ApiState::new(db.clone(), modem);

        let decision = queue_send(&state, &request(Some("+14155552671"), Some("hello"))).await;

        let id = match decision {
            SendDecision::Accepted(resp) => {
                assert_eq!(resp.status, MessageStatus::Queued);
                assert_eq!(resp.parts, 1);
                resp.id
            }
            other => panic!("expected Accepted, got {other:?}"),
        };

        // The record is persisted; the queued stub result leaves it queued.
        let stored = db.get_outbound_message(id).await.unwrap().unwrap();
        assert_eq!(stored.to_number, "+14155552671");
        assert_eq!(stored.body, "hello");
        assert_eq!(stored.part_count, 1);
    }

    #[tokio::test]
    async fn send_rejects_missing_fields() {
        let db = ready_db().await;
        let state = ApiState::new(
            db,
            StubModem {
                snapshot: healthy_snapshot(),
                result: queued_result(),
            },
        );

        let decision = queue_send(&state, &request(None, None)).await;
        match decision {
            SendDecision::Invalid(ValidationError::MissingFields(fields)) => {
                assert_eq!(fields, vec!["to".to_string(), "body".to_string()]);
            }
            other => panic!("expected MissingFields, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn send_rejects_invalid_phone() {
        let db = ready_db().await;
        let state = ApiState::new(
            db,
            StubModem {
                snapshot: healthy_snapshot(),
                result: queued_result(),
            },
        );

        let decision = queue_send(&state, &request(Some("not-a-number"), Some("hi"))).await;
        assert_eq!(
            decision,
            SendDecision::Invalid(ValidationError::InvalidPhoneNumber)
        );
    }

    #[tokio::test]
    async fn send_rejects_overlong_body() {
        let db = ready_db().await;
        let state = ApiState::new(
            db,
            StubModem {
                snapshot: healthy_snapshot(),
                result: queued_result(),
            },
        );

        let long = "x".repeat(1531);
        let decision = queue_send(&state, &request(Some("+14155552671"), Some(&long))).await;
        assert_eq!(
            decision,
            SendDecision::Invalid(ValidationError::BodyTooLong)
        );
    }

    #[tokio::test]
    async fn send_gated_when_modem_undeliverable() {
        let db = ready_db().await;
        let state = ApiState::with_retry_after(
            db,
            StubModem {
                snapshot: down_snapshot(),
                result: queued_result(),
            },
            17,
        );

        let decision = queue_send(&state, &request(Some("+14155552671"), Some("hi"))).await;
        assert_eq!(
            decision,
            SendDecision::Gated {
                retry_after_secs: 17
            }
        );
    }

    #[tokio::test]
    async fn send_not_ready_when_schema_closed() {
        let db = Db::connect_in_memory().await.unwrap();
        let state = ApiState::with_retry_after(
            db,
            StubModem {
                snapshot: healthy_snapshot(),
                result: queued_result(),
            },
            30,
        );

        let decision = queue_send(&state, &request(Some("+14155552671"), Some("hi"))).await;
        assert_eq!(
            decision,
            SendDecision::NotReady {
                retry_after_secs: 30
            }
        );
    }

    #[tokio::test]
    async fn dispatch_marks_sent_with_reference() {
        let db = ready_db().await;
        let created = db
            .create_outbound_message("+14155552671", "hi", MessageStatus::Queued, 1)
            .await
            .unwrap();
        let modem = StubModem {
            snapshot: healthy_snapshot(),
            result: SendResult {
                status: MessageStatus::Sent,
                reference: Some(42),
                error_code: None,
                error: None,
            },
        };

        dispatch_send(&db, &modem, created.id, "+14155552671".into(), "hi".into()).await;

        let stored = db.get_outbound_message(created.id).await.unwrap().unwrap();
        assert_eq!(stored.status, MessageStatus::Sent);
        assert_eq!(stored.msg_reference.as_deref(), Some("42"));
    }

    #[tokio::test]
    async fn dispatch_marks_failed_with_error_code() {
        let db = ready_db().await;
        let created = db
            .create_outbound_message("+14155552671", "hi", MessageStatus::Queued, 1)
            .await
            .unwrap();
        let modem = StubModem {
            snapshot: healthy_snapshot(),
            result: SendResult {
                status: MessageStatus::Failed,
                reference: None,
                error_code: Some(500),
                error: Some("ignored when code present".into()),
            },
        };

        dispatch_send(&db, &modem, created.id, "+14155552671".into(), "hi".into()).await;

        let stored = db.get_outbound_message(created.id).await.unwrap().unwrap();
        assert_eq!(stored.status, MessageStatus::Failed);
        assert_eq!(stored.error_code.as_deref(), Some("500"));
    }

    #[tokio::test]
    async fn dispatch_leaves_queued_when_result_queued() {
        let db = ready_db().await;
        let created = db
            .create_outbound_message("+14155552671", "hi", MessageStatus::Queued, 1)
            .await
            .unwrap();
        let modem = StubModem {
            snapshot: healthy_snapshot(),
            result: queued_result(),
        };

        dispatch_send(&db, &modem, created.id, "+14155552671".into(), "hi".into()).await;

        let stored = db.get_outbound_message(created.id).await.unwrap().unwrap();
        assert_eq!(stored.status, MessageStatus::Queued);
    }

    #[test]
    fn health_response_healthy_is_200() {
        let (status, body) = build_health_response(&healthy_snapshot());
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body.health, "healthy");
        assert!(body.serial_connected);
        assert_eq!(body.sim_status, "ready");
    }

    #[test]
    fn health_response_unhealthy_is_503() {
        let (status, body) = build_health_response(&down_snapshot());
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(body.health, "unhealthy");
        assert!(!body.serial_connected);
        assert_eq!(body.sim_status, "unknown");
    }

    #[test]
    fn status_response_reports_all_values_when_available() {
        let resp = build_status_response(&healthy_snapshot());
        assert_eq!(resp.signal_percent, Some(75));
        assert_eq!(resp.registered, Some(true));
        assert_eq!(resp.operator.as_deref(), Some("Carrier"));
        assert!(resp.unavailable.is_empty());
    }

    #[test]
    fn status_response_marks_unavailable_commands() {
        let resp = build_status_response(&down_snapshot());
        assert_eq!(resp.signal_percent, None);
        assert_eq!(resp.registered, None);
        assert_eq!(resp.operator, None);
        assert_eq!(
            resp.unavailable,
            vec![
                "signal".to_string(),
                "registration".to_string(),
                "operator".to_string()
            ]
        );
    }

    // -- Middleware wiring tests (Req 3.1–3.4, 4.2, 4.3) --------------------

    use axum::body::Body;
    use tower::util::ServiceExt;

    async fn insert_key(db: &Db, plaintext: &str, custom: Option<i64>) -> i64 {
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

    /// Mark key as revoked.
    async fn revoke_key(db: &Db, id: i64) {
        sqlx::query("UPDATE api_keys SET revoked = 1 WHERE id = ?")
            .bind(id)
            .execute(db.pool())
            .await
            .unwrap();
    }

    fn test_state(db: Db) -> ApiState<StubModem> {
        ApiState::new(
            db,
            StubModem {
                snapshot: healthy_snapshot(),
                result: queued_result(),
            },
        )
    }

    fn get_request(path: &str, api_key: Option<&str>) -> Request {
        let mut builder = axum::http::Request::builder().method("GET").uri(path);
        if let Some(key) = api_key {
            builder = builder.header("x-api-key", key);
        }
        builder.body(Body::empty()).unwrap()
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
        // Default limit of 1 request per window so the second request trips it.
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
