//! REST API layer: Axum endpoint handlers (task 13.1).
//!
//! This module implements the endpoint handlers described in `design.md` §2:
//!
//! | Method | Path                        | Description                         | Requirements        |
//! |--------|-----------------------------|-------------------------------------|---------------------|
//! | POST   | `/api/v1/messages`          | Send an SMS                         | 1.1, 1.4, 1.5, 10.4 |
//! | GET    | `/api/v1/messages/inbound`  | List inbound messages (desc)        | 2.4                 |
//! | GET    | `/api/v1/messages/{id}`     | Fetch one outbound message status   | 1.4, 1.5            |
//! | GET    | `/health`                   | Serial + SIM state                  | 9.1                 |
//! | GET    | `/status`                   | Signal, registration, operator      | 9.2, 9.7            |
//!
//! The send handler validates the request, runs the pure deliverability gate
//! (Req 10.4), persists a `queued` outbound record (Req 1.1), and dispatches
//! the actual transmission to the Modem Manager on a background task so the
//! acceptance response returns promptly (within 2 seconds, Req 1.1). The
//! background dispatch updates the record to `sent`/`failed` from the modem's
//! result (Req 1.4, 1.5), or leaves it `queued` to retry when the modem is not
//! yet registered (Req 10.5).
//!
//! The Modem Manager is reached through the [`ModemPort`] trait rather than a
//! concrete handle. This mirrors the `SerialTransport` abstraction in the
//! `modem` module: production wires the real [`ModemHandle`], while tests can
//! supply a stub with a fixed status snapshot and a canned send result. The
//! authentication and rate-limit middleware and the assembled router are wired
//! in task 13.2.

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
    ApiKeyId, AuthOutcome, FailureTracker, KeyStore, authenticate, build_audit_record,
    key_identifier, passes_guard,
};
use crate::db::Db;
use crate::health::{
    DEFAULT_RETRY_AFTER_SECS, DeliverabilityOutcome, ModemStatusSnapshot, ServiceHealth,
    SimStatus, deliverability_gate, derive_health,
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

/// The slice of Modem Manager behavior the API layer depends on.
///
/// Implemented for the real [`ModemHandle`] in production; abstracting it as a
/// trait (as the `modem` module does for its serial transport) keeps the
/// handlers testable with an in-memory stub. Implementors must be cheap to
/// clone and `Send + Sync + 'static` so handler state can be shared across the
/// Axum router and the background send-dispatch task.
pub trait ModemPort: Clone + Send + Sync + 'static {
    /// The modem's current health/status snapshot, served from shared state so
    /// it is available even while the serial port is reconnecting.
    fn status_snapshot(&self) -> ModemStatusSnapshot;

    /// Dispatch an SMS send to the Modem Manager and await its result.
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

/// Shared state for the REST API handlers.
///
/// Cloning is cheap: the database handle and the modem port are both
/// reference-counted internally. `retry_after_secs` is the value advertised in
/// the `Retry-After` header when a send is rejected by the deliverability gate
/// (Req 10.4) or when the schema is not yet ready (Req 6.5).
#[derive(Clone)]
pub struct ApiState<M: ModemPort> {
    /// Persistence handle.
    pub db: Db,
    /// Handle to the Modem Manager.
    pub modem: M,
    /// `Retry-After` seconds advertised on 503 responses.
    pub retry_after_secs: u64,
    /// Per-identifier authentication failure tracker driving lockout (Req 3.8).
    /// Shared behind a `Mutex`; the guarded critical sections are short and
    /// synchronous, never held across an `.await`.
    pub auth_failures: Arc<Mutex<FailureTracker>>,
    /// Per-key fixed-window rate limiter (Req 4.1–4.5). Shared behind a
    /// `Mutex` for the same reason as `auth_failures`.
    pub rate_limiter: Arc<Mutex<RateLimiter>>,
    /// Default per-key request limit applied when a key sets no custom limit.
    pub default_rate_limit: u32,
    /// Rate-limit window length (Req 4.1, 4.5).
    pub rate_window: Duration,
}

impl<M: ModemPort> ApiState<M> {
    /// Build API state with the default deliverability `Retry-After` interval
    /// and the default rate-limit configuration.
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

    /// Build API state with an explicit `Retry-After` interval (seconds).
    pub fn with_retry_after(db: Db, modem: M, retry_after_secs: u64) -> Self {
        ApiState {
            retry_after_secs,
            ..ApiState::new(db, modem)
        }
    }

    /// Override the rate-limit configuration (default limit and window length).
    pub fn with_rate_config(mut self, default_rate_limit: u32, rate_window: Duration) -> Self {
        self.default_rate_limit = default_rate_limit;
        self.rate_window = rate_window;
        self
    }
}

// ---------------------------------------------------------------------------
// Wire types
// ---------------------------------------------------------------------------

/// Body of a send request. Both fields are optional on the wire so an omitted
/// field can be reported precisely (Req 1.6) rather than rejected as malformed.
#[derive(Debug, Clone, Deserialize)]
pub struct SendRequest {
    /// Recipient phone number (E.164).
    pub to: Option<String>,
    /// Message body.
    pub body: Option<String>,
}

/// Acceptance response for a queued send (Req 1.1).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SendResponse {
    /// Identifier of the persisted outbound message.
    pub id: i64,
    /// Lifecycle status at acceptance time (always `queued`).
    pub status: MessageStatus,
    /// Number of SMS parts the body was segmented into.
    pub parts: u8,
}

/// A client-facing error body. `fields` names the offending request fields,
/// when applicable (Req 1.6, 1.7, 1.10).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ApiError {
    /// Human-readable error description.
    pub error: String,
    /// Names of the request fields the error concerns (may be empty).
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

/// Health endpoint response (Req 9.1).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct HealthResponse {
    /// Overall service health verdict.
    pub health: &'static str,
    /// Whether the serial port is currently connected (Req 9.1, 9.4).
    pub serial_connected: bool,
    /// SIM card status from `AT+CPIN?` (Req 9.1, 9.3).
    pub sim_status: &'static str,
}

/// Status endpoint response (Req 9.2). Each field is `null` and named in
/// `unavailable` when the corresponding modem command did not respond (Req 9.7).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct StatusResponse {
    /// Signal quality 0..=100 from `AT+CSQ`, or `null` when unavailable.
    pub signal_percent: Option<u8>,
    /// Network registration from `AT+CREG?`, or `null` when unavailable.
    pub registered: Option<bool>,
    /// Current operator from `AT+COPS?`, or `null` when unavailable.
    pub operator: Option<String>,
    /// Names of the commands whose values are unavailable (Req 9.7).
    pub unavailable: Vec<String>,
}

// ---------------------------------------------------------------------------
// Router
// ---------------------------------------------------------------------------

/// Build the REST API router for the given state.
///
/// Protected message endpoints sit behind two layers, applied auth-first then
/// rate-limit (Req 3.1–3.4, 4.2, 4.3): the API-key auth layer rejects absent,
/// malformed, unknown, revoked, or locked-out keys with HTTP 401 and performs
/// no business processing (Req 3.2, 3.3, 3.4, 3.7, 3.8), then the per-key
/// rate-limit layer rejects over-limit requests with HTTP 429 and a
/// `Retry-After` header (Req 4.2, 4.3). `/health` and `/status` are mounted
/// without these layers so they remain unauthenticated (Req 9.1, 9.2).
pub fn router<M: ModemPort>(state: ApiState<M>) -> Router {
    // Protected endpoints: auth runs first (outermost), then rate limiting.
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

    // Unauthenticated operational endpoints.
    let public = Router::new()
        .route("/health", get(health_handler::<M>))
        .route("/status", get(status_handler::<M>))
        .with_state(state);

    protected.merge(public)
}

// ---------------------------------------------------------------------------
// Authentication and rate-limit middleware (Req 3.1–3.4, 3.7, 3.8, 4.2–4.4)
// ---------------------------------------------------------------------------

/// Context produced by the auth layer for an authorized request and consumed
/// by the rate-limit layer: the resolved key id, its optional custom rate
/// limit, and the non-reversible key identifier used for per-key accounting.
#[derive(Debug, Clone)]
struct AuthContext {
    /// Database id of the authorized key.
    #[allow(dead_code)]
    id: ApiKeyId,
    /// The key's custom rate limit, if any (Req 4.6, 4.7).
    custom_rate_limit: Option<u32>,
    /// Non-reversible SHA-256 identifier, used as the rate-limiter key (Req 4.4).
    identifier: String,
}

/// A [`KeyStore`] over a single pre-resolved lookup result.
///
/// The async DB lookup is performed before [`authenticate`] is invoked (whose
/// store interface is synchronous); this adapter feeds the already-resolved
/// id back through the tested guard/lockout flow without a second lookup.
struct ResolvedStore {
    id: Option<ApiKeyId>,
}

impl KeyStore for ResolvedStore {
    fn lookup_active(&self, _key_identifier: &str) -> Option<ApiKeyId> {
        self.id
    }
}

/// Extract the presented API key from the request headers.
///
/// `X-API-Key` takes precedence; otherwise the bearer token of an
/// `Authorization: Bearer <key>` header is used. Returns `None` when neither
/// is present, which the auth flow treats as an absent key (Req 3.2).
fn extract_api_key(headers: &HeaderMap) -> Option<String> {
    if let Some(value) = headers.get("x-api-key").and_then(|v| v.to_str().ok()) {
        if !value.is_empty() {
            return Some(value.to_string());
        }
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

/// Look up an active, non-revoked API key by its non-reversible identifier,
/// returning the key id and its optional custom rate limit (Req 3.1, 3.3, 3.4).
///
/// Revoked keys are excluded by the `revoked = 0` predicate, so they resolve to
/// `None` exactly like unknown keys and are rejected with 401 (Req 3.4).
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

/// Build a 401 Unauthorized response with no business processing (Req 3.2–3.4).
fn unauthorized_response() -> Response {
    (
        StatusCode::UNAUTHORIZED,
        Json(ApiError::new("unauthorized", Vec::new())),
    )
        .into_response()
}

/// API-key authentication layer (Req 3.1–3.4, 3.7, 3.8).
///
/// Extracts the presented key, rejects empty/over-length keys before any
/// lookup (Req 3.7), rejects locked-out identifiers (Req 3.8), then resolves
/// the active key and runs the tested [`authenticate`] flow. On success the
/// resolved [`AuthContext`] is attached for the rate-limit layer and the
/// request proceeds (Req 3.1); every rejection maps to HTTP 401 and the
/// handler chain is never reached. Each attempt is audited with only the
/// non-reversible key identifier — never the plaintext key (Req 3.6).
async fn auth_middleware<M: ModemPort>(
    State(state): State<ApiState<M>>,
    mut request: Request,
    next: Next,
) -> Response {
    let now = Instant::now();
    let timestamp = Utc::now();
    let presented = extract_api_key(request.headers()).unwrap_or_default();

    // Pre-lookup guard: empty or over-length keys never reach the store (Req 3.7).
    if !passes_guard(&presented) {
        emit_auth_audit(&presented, &AuthOutcome::Unauthorized, timestamp);
        return unauthorized_response();
    }

    let identifier = key_identifier(&presented);

    // Lockout short-circuit so a locked-out identifier performs no lookup or
    // business processing (Req 3.8).
    {
        let tracker = state.auth_failures.lock().expect("auth tracker poisoned");
        if tracker.is_locked(&identifier, now) {
            emit_auth_audit(&presented, &AuthOutcome::LockedOut, timestamp);
            return unauthorized_response();
        }
    }

    // Resolve the active, non-revoked key (Req 3.1, 3.3, 3.4).
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

    // Run the tested guard/lockout/lookup flow against the resolved result,
    // recording success or failure in the shared tracker.
    let store = ResolvedStore {
        id: resolved.map(|(id, _)| id),
    };
    let outcome = {
        let mut tracker = state.auth_failures.lock().expect("auth tracker poisoned");
        authenticate(&presented, &store, &mut tracker, now)
    };

    emit_auth_audit(&presented, &outcome, timestamp);

    match outcome {
        AuthOutcome::Authorized(id) => {
            let custom_rate_limit = resolved.and_then(|(_, custom)| custom);
            request.extensions_mut().insert(AuthContext {
                id,
                custom_rate_limit,
                identifier,
            });
            next.run(request).await
        }
        AuthOutcome::Unauthorized | AuthOutcome::LockedOut => unauthorized_response(),
    }
}

/// Emit a structured audit/log record for an auth attempt, carrying only the
/// non-reversible key identifier (never the plaintext key) (Req 3.6, 7.6).
fn emit_auth_audit(presented: &str, outcome: &AuthOutcome, timestamp: chrono::DateTime<Utc>) {
    let record = build_audit_record(presented, outcome, timestamp);
    tracing::info!(
        event_type = record.event_type,
        result = ?record.result,
        key_identifier = %record.key_identifier,
        "api key authentication attempt"
    );
}

/// Per-key rate-limit layer (Req 4.1–4.4).
///
/// Runs after the auth layer, keyed by the authorized key's non-reversible
/// identifier so each key is throttled independently (Req 4.4). Requests under
/// the effective limit proceed; requests at or over the limit are rejected with
/// HTTP 429 and a `Retry-After` header, leaving the count unchanged (Req 4.2,
/// 4.3). The effective limit honors a valid custom override, falling back to
/// the default otherwise (Req 4.6, 4.7).
async fn rate_limit_middleware<M: ModemPort>(
    State(state): State<ApiState<M>>,
    Extension(ctx): Extension<AuthContext>,
    request: Request,
    next: Next,
) -> Response {
    let now = Instant::now();
    let (limit, _config_err) = effective_limit(ctx.custom_rate_limit, state.default_rate_limit);

    let decision = {
        let mut limiter = state.rate_limiter.lock().expect("rate limiter poisoned");
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

// ---------------------------------------------------------------------------
// Send handler (Req 1.1, 1.4, 1.5, 10.4)
// ---------------------------------------------------------------------------

/// The outcome of preparing and queuing a send, mapped to HTTP by
/// [`IntoResponse`].
#[derive(Debug, Clone, PartialEq, Eq)]
enum SendDecision {
    /// The send was accepted and queued (Req 1.1).
    Accepted(SendResponse),
    /// The request failed validation (Req 1.6, 1.7, 1.10).
    Invalid(ValidationError),
    /// The body requires more SMS parts than allowed (Req 1.8).
    TooManyParts {
        /// Parts the body would have required.
        required: usize,
    },
    /// Delivery preconditions were unmet; reject with 503 + `Retry-After`
    /// (Req 10.4).
    Gated {
        /// Seconds to advertise in `Retry-After`.
        retry_after_secs: u64,
    },
    /// The database schema is not ready; reject with 503 (Req 6.5).
    NotReady {
        /// Seconds to advertise in `Retry-After`.
        retry_after_secs: u64,
    },
    /// An unexpected persistence error occurred; respond 500 (Req 6.6).
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

/// `POST /api/v1/messages` — validate, gate, persist `queued`, and dispatch.
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

/// Validate the request, run the deliverability gate, persist a `queued`
/// outbound record, and dispatch the send to the Modem Manager on a background
/// task. Returns the [`SendDecision`] describing the synchronous outcome.
async fn queue_send<M: ModemPort>(state: &ApiState<M>, request: &SendRequest) -> SendDecision {
    // 1. Required-field presence (Req 1.6).
    if let Err(err) = check_required_fields(request.to.as_deref(), request.body.as_deref()) {
        return SendDecision::Invalid(err);
    }
    // Safe to unwrap: the presence check above guarantees both are `Some`.
    let to = request.to.as_deref().expect("to present after field check");
    let body = request
        .body
        .as_deref()
        .expect("body present after field check");

    // 2. Phone number (Req 1.7) and body length (Req 1.1, 1.10) validation.
    if let Err(err) = validate_e164(to) {
        return SendDecision::Invalid(err);
    }
    if let Err(err) = validate_body(body) {
        return SendDecision::Invalid(err);
    }

    // 3. Segment to determine the part count (Req 1.8).
    let parts = match segment_message(body) {
        Ok(segments) => segments.len() as u8,
        Err(SegmentError::TooManyParts { required }) => {
            return SendDecision::TooManyParts { required };
        }
    };

    // 4. Deliverability gate (Req 10.4): reject with 503 + Retry-After when a
    //    precondition for immediate delivery is unmet.
    let snapshot = state.modem.status_snapshot();
    if let DeliverabilityOutcome::Rejected { retry_after_secs } =
        deliverability_gate(&snapshot, state.retry_after_secs)
    {
        return SendDecision::Gated { retry_after_secs };
    }

    // 5. Persist the queued record (Req 1.1).
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

    // 6. Dispatch transmission in the background so acceptance returns promptly
    //    (Req 1.1). The background task updates the record from the modem's
    //    result (Req 1.4, 1.5) or leaves it queued to retry (Req 10.5).
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

/// Dispatch a send to the Modem Manager and reconcile the outbound record with
/// the result: `sent` with the returned reference (Req 1.4), `failed` with the
/// returned error code/detail (Req 1.5), or left `queued` for a later retry
/// when the modem was not registered (Req 10.5).
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
            // Prefer the structured modem error code, falling back to the
            // human-readable detail (timeout, manager unavailable, ...).
            let detail = result
                .error_code
                .map(|code| code.to_string())
                .or(result.error);
            let _ = db
                .update_outbound_message(id, MessageStatus::Failed, None, detail.as_deref())
                .await;
        }
        // Preconditions unmet at send time: retain as queued (Req 10.5).
        MessageStatus::Queued => {}
    }
}

// ---------------------------------------------------------------------------
// Inbound listing (Req 2.4) and single outbound status (Req 1.4, 1.5)
// ---------------------------------------------------------------------------

/// `GET /api/v1/messages/inbound` — list persisted inbound messages ordered by
/// receipt timestamp descending, returning an empty array when none exist
/// (Req 2.4).
async fn inbound_handler<M: ModemPort>(State(state): State<ApiState<M>>) -> Response {
    match state.db.list_inbound_messages().await {
        Ok(messages) => (StatusCode::OK, Json(messages)).into_response(),
        Err(err) => db_error_response(&err, state.retry_after_secs),
    }
}

/// `GET /api/v1/messages/{id}` — fetch a single outbound message status
/// (Req 1.4, 1.5), or 404 when no such message exists.
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

// ---------------------------------------------------------------------------
// Health (Req 9.1) and status (Req 9.2, 9.7) handlers
// ---------------------------------------------------------------------------

/// `GET /health` — report the serial connection state and SIM status, plus the
/// overall health verdict (Req 9.1, 9.3, 9.4, 9.6). Responds 503 when the
/// derived health is unhealthy, otherwise 200.
async fn health_handler<M: ModemPort>(State(state): State<ApiState<M>>) -> Response {
    let snapshot = state.modem.status_snapshot();
    let (status, body) = build_health_response(&snapshot);
    (status, Json(body)).into_response()
}

/// `GET /status` — report signal quality, registration, and operator, marking
/// each value unavailable when its modem command did not respond (Req 9.2, 9.7).
async fn status_handler<M: ModemPort>(State(state): State<ApiState<M>>) -> Response {
    let snapshot = state.modem.status_snapshot();
    (StatusCode::OK, Json(build_status_response(&snapshot))).into_response()
}

/// Build the health response and its HTTP status from a snapshot (Req 9.1).
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

/// Build the status response from a snapshot, reporting per-command
/// unavailability (Req 9.7).
///
/// Signal quality and operator come straight from the snapshot's optional
/// fields (a `None` means the corresponding `AT+CSQ` / `AT+COPS?` did not
/// yield a value). Registration is reported only when the modem is responsive;
/// an unresponsive modem could not have answered `AT+CREG?`, so its value is
/// surfaced as unavailable.
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

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// The canonical lowercase name for a [`ServiceHealth`] verdict.
fn health_str(health: ServiceHealth) -> &'static str {
    match health {
        ServiceHealth::Healthy => "healthy",
        ServiceHealth::Degraded => "degraded",
        ServiceHealth::Unhealthy => "unhealthy",
    }
}

/// The canonical lowercase name for a [`SimStatus`].
fn sim_str(sim: SimStatus) -> &'static str {
    match sim {
        SimStatus::Ready => "ready",
        SimStatus::NotReady => "not_ready",
        SimStatus::Unknown => "unknown",
    }
}

/// Map a [`ValidationError`] to its HTTP status and client error body.
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

/// Map a [`DbError`](crate::db::DbError) to a response: `NotReady` becomes 503
/// with a `Retry-After` header (Req 6.5); any other error becomes 500 (Req 6.6).
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

/// Build a JSON response carrying a `Retry-After` header with an integer number
/// of seconds (Req 4.3, 10.4).
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

    /// A stub Modem Manager with a fixed status snapshot and a canned send
    /// result, so handler logic can be exercised without real hardware.
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
        assert_eq!(decision, SendDecision::Invalid(ValidationError::BodyTooLong));
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
        // Connect without running migrations: the schema-ready gate is closed.
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
    use tower::util::ServiceExt; // for `oneshot`

    /// Insert an active, non-revoked API key and return its row id.
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

    /// Mark a key as revoked.
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

        // Exhaust key-a's single-request budget.
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

        // key-b is unaffected by key-a's activity (Req 4.4).
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
