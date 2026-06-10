use std::convert::Infallible;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::{
    Extension, Json, Router,
    extract::{Path, Request, State, rejection::JsonRejection},
    http::{HeaderMap, HeaderValue, StatusCode, header},
    middleware::{self, Next},
    response::{
        IntoResponse, Response,
        sse::{Event, KeepAlive, Sse},
    },
    routing::get,
};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use tokio::sync::oneshot;
use tokio_stream::{Stream, StreamExt, wrappers::BroadcastStream};
use tower::ServiceBuilder;
use utoipa::{
    Modify, OpenApi,
    openapi::security::{ApiKey, ApiKeyValue, SecurityScheme},
};
use utoipa_axum::{router::OpenApiRouter, routes};

use crate::auth::{
    ApiKeyId, AuthOutcome, FailureTracker, KeyStore, authenticate_identified,
    build_audit_record_with_identifier, key_identifier, passes_guard,
};
use crate::db::Db;
use crate::events::{EventBus, MessageStatusEvent, ServiceEvent};
use crate::health::{
    DEFAULT_RETRY_AFTER_SECS, DeliverabilityOutcome, ModemStatusSnapshot, ServiceHealth, SimStatus,
    deliverability_gate, derive_health,
};
use crate::models::{InboundMessage, MessageStatus, OutboundMessage};
use crate::modem::{ModemHandle, SendResult};
use crate::ratelimit::{RateDecision, RateLimiter, effective_limit};
use crate::sms::{
    SegmentError, ValidationError, check_required_fields, segment_message, validate_body,
    validate_e164,
};

/// Default rate limit value.
pub const DEFAULT_RATE_LIMIT: u32 = 100;

/// Default rate limit window in seconds.
pub const DEFAULT_RATE_WINDOW_SECS: u64 = 60;

/// Maximum time the synchronous send endpoint waits for delivery to complete
/// before falling back to a `202 Accepted` queued response.
const SYNC_SEND_WAIT_SECS: u64 = 30;

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
trait DynModemPort: Send + Sync + 'static {
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
type SharedModem = Arc<dyn DynModemPort>;

/// API shared state configuration.
#[derive(Clone)]
pub struct ApiState {
    /// Database pool handle.
    pub db: Db,
    /// Type-erased modem port instance.
    modem: SharedModem,
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

/// Payload for sending an SMS.
#[derive(Debug, Clone, Deserialize, utoipa::ToSchema)]
pub struct SendRequest {
    /// Recipient E.164 phone number.
    #[schema(example = "+14155552671")]
    pub to: Option<String>,
    /// Message body.
    #[schema(example = "Hello from the SMS microservice")]
    pub body: Option<String>,
}

/// Response returned on successful SMS submission.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, utoipa::ToSchema)]
pub struct SendResponse {
    /// The unique message ID.
    pub id: i64,
    /// Current delivery status.
    pub status: MessageStatus,
    /// Number of split segments.
    pub parts: u8,
}

/// Response returned by the synchronous send endpoint once delivery resolves.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, utoipa::ToSchema)]
pub struct SyncSendResponse {
    /// The unique message ID.
    pub id: i64,
    /// Terminal delivery status (`sent` or `failed`).
    pub status: MessageStatus,
    /// The modem-assigned reference when the message was sent.
    pub reference: Option<String>,
    /// Number of split segments.
    pub parts: u8,
}

pub use crate::error::ApiError;

/// Response representing the service health status.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, utoipa::ToSchema)]
pub struct HealthResponse {
    /// Overall health value.
    pub health: &'static str,
    /// Connection state of serial port.
    pub serial_connected: bool,
    /// Sim status description.
    pub sim_status: &'static str,
}

/// Response representing the detailed modem status.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, utoipa::ToSchema)]
pub struct StatusResponse {
    /// Signal strength percentage.
    pub signal_percent: Option<u8>,
    /// Registration status.
    pub registered: Option<bool>,
    /// Current network operator name.
    pub operator: Option<String>,
    /// List of unavailable status conditions.
    pub unavailable: Vec<String>,
}

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
        SendRequest,
        SendResponse,
        SyncSendResponse,
        HealthResponse,
        StatusResponse,
        ApiError,
        MessageStatus,
        InboundMessage,
        OutboundMessage,
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
        .merge(protected_routes())
        .merge(public_routes())
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

/// Authenticated, rate-limited routes (everything except `/health`/`/status`).
fn protected_routes() -> OpenApiRouter<ApiState> {
    OpenApiRouter::new()
        .routes(routes!(send_handler))
        .routes(routes!(send_sync_handler))
        .routes(routes!(inbound_handler))
        .routes(routes!(outbound_status_handler))
        .routes(routes!(events_handler))
}

/// Unauthenticated operational routes.
fn public_routes() -> OpenApiRouter<ApiState> {
    OpenApiRouter::new()
        .routes(routes!(health_handler))
        .routes(routes!(status_handler))
}

/// Creates the router containing all API routes plus the OpenAPI document.
///
/// Public REST routes are collected through [`OpenApiRouter`] so the routing
/// table and the generated spec stay in sync; the admin dashboard router is
/// merged separately and is deliberately absent from the OpenAPI document.
pub fn router(state: ApiState) -> Router {
    let protected = protected_routes().route_layer(
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
        .merge(public_routes())
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

/// Send an SMS message asynchronously.
///
/// Validates and persists the message as `queued`, then dispatches delivery in
/// the background and returns immediately.
#[utoipa::path(
    post,
    path = "/api/v1/messages",
    tag = "messages",
    request_body = SendRequest,
    security(("api_key" = [])),
    responses(
        (status = 202, description = "Accepted and queued for delivery", body = SendResponse),
        (status = 400, description = "Invalid or incomplete request", body = ApiError),
        (status = 401, description = "Missing or invalid API key", body = ApiError),
        (status = 429, description = "Rate limit exceeded", body = ApiError,
            headers(("Retry-After" = String, description = "Seconds until requests are permitted again"))),
        (status = 500, description = "Internal server error", body = ApiError),
        (status = 503, description = "Delivery preconditions unmet or service not ready", body = ApiError,
            headers(("Retry-After" = String, description = "Seconds to wait before retrying")))
    )
)]
async fn send_handler(
    State(state): State<ApiState>,
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

/// Send an SMS message and block for the delivery outcome.
///
/// Applies the same validation, gating, and persistence as the asynchronous
/// endpoint, but waits for delivery to resolve before responding.
#[utoipa::path(
    post,
    path = "/api/v1/messages/sync",
    tag = "messages",
    request_body = SendRequest,
    security(("api_key" = [])),
    responses(
        (status = 200, description = "Delivery resolved within the wait window", body = SyncSendResponse,
            example = json!({ "id": 142, "status": "sent", "reference": "25", "parts": 1 })),
        (status = 202, description = "Still queued; delivery continues in the background", body = SendResponse),
        (status = 400, description = "Invalid or incomplete request", body = ApiError),
        (status = 401, description = "Missing or invalid API key", body = ApiError),
        (status = 429, description = "Rate limit exceeded", body = ApiError,
            headers(("Retry-After" = String, description = "Seconds until requests are permitted again"))),
        (status = 500, description = "Internal server error", body = ApiError),
        (status = 503, description = "Delivery preconditions unmet or service not ready", body = ApiError,
            headers(("Retry-After" = String, description = "Seconds to wait before retrying")))
    )
)]
async fn send_sync_handler(
    State(state): State<ApiState>,
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
    sync_send(&state, &request).await
}

/// A validated, persisted send ready to be dispatched to the Modem Manager.
struct PreparedSend {
    id: i64,
    to: String,
    body: String,
    parts: u8,
}

/// The resolved outcome of a single dispatch.
struct DispatchOutcome {
    status: MessageStatus,
    reference: Option<String>,
}

async fn prepare_send(
    state: &ApiState,
    request: &SendRequest,
) -> Result<PreparedSend, SendDecision> {
    let (Some(to), Some(body)) = (request.to.as_deref(), request.body.as_deref()) else {
        let err = check_required_fields(request.to.as_deref(), request.body.as_deref())
            .err()
            .unwrap_or(ValidationError::MissingFields(Vec::new()));
        return Err(SendDecision::Invalid(err));
    };

    if let Err(err) = validate_e164(to) {
        return Err(SendDecision::Invalid(err));
    }
    if let Err(err) = validate_body(body) {
        return Err(SendDecision::Invalid(err));
    }

    let parts = match segment_message(body) {
        Ok(segments) => segments.len() as u8,
        Err(SegmentError::TooManyParts { required }) => {
            return Err(SendDecision::TooManyParts { required });
        }
    };

    let snapshot = state.modem.status_snapshot();
    if let DeliverabilityOutcome::Rejected { retry_after_secs } =
        deliverability_gate(&snapshot, state.retry_after_secs)
    {
        return Err(SendDecision::Gated { retry_after_secs });
    }

    let record = match state
        .db
        .create_outbound_message(to, body, MessageStatus::Queued, parts)
        .await
    {
        Ok(record) => record,
        Err(err) if err.is_not_ready() => {
            return Err(SendDecision::NotReady {
                retry_after_secs: state.retry_after_secs,
            });
        }
        Err(_) => return Err(SendDecision::ServerError),
    };

    Ok(PreparedSend {
        id: record.id,
        to: to.to_string(),
        body: body.to_string(),
        parts,
    })
}

async fn queue_send(state: &ApiState, request: &SendRequest) -> SendDecision {
    let PreparedSend {
        id,
        to,
        body,
        parts,
    } = match prepare_send(state, request).await {
        Ok(prepared) => prepared,
        Err(decision) => return decision,
    };

    let db = state.db.clone();
    let modem = state.modem.clone();
    let events = state.events.clone();
    tokio::spawn(async move {
        dispatch_send(&db, &modem, &events, id, to, body, None).await;
    });

    SendDecision::Accepted(SendResponse {
        id,
        status: MessageStatus::Queued,
        parts,
    })
}

async fn sync_send(state: &ApiState, request: &SendRequest) -> Response {
    let PreparedSend {
        id,
        to,
        body,
        parts,
    } = match prepare_send(state, request).await {
        Ok(prepared) => prepared,
        Err(decision) => return decision.into_response(),
    };

    let (reply, rx) = oneshot::channel();
    let db = state.db.clone();
    let modem = state.modem.clone();
    let events = state.events.clone();
    tokio::spawn(async move {
        dispatch_send(&db, &modem, &events, id, to, body, Some(reply)).await;
    });

    let wait = Duration::from_secs(SYNC_SEND_WAIT_SECS);
    match tokio::time::timeout(wait, rx).await {
        Ok(Ok(outcome)) if outcome.status != MessageStatus::Queued => (
            StatusCode::OK,
            Json(SyncSendResponse {
                id,
                status: outcome.status,
                reference: outcome.reference,
                parts,
            }),
        )
            .into_response(),
        // Timed out, deferred (still queued), or the dispatcher dropped the
        // reply: the send continues in the background, so report the queued
        // acceptance rather than blocking the client indefinitely.
        _ => (
            StatusCode::ACCEPTED,
            Json(SendResponse {
                id,
                status: MessageStatus::Queued,
                parts,
            }),
        )
            .into_response(),
    }
}

async fn dispatch_send(
    db: &Db,
    modem: &SharedModem,
    events: &EventBus,
    id: i64,
    to: String,
    body: String,
    reply: Option<oneshot::Sender<DispatchOutcome>>,
) {
    let result = modem.send(to, body).await;
    let outcome = match result.status {
        MessageStatus::Sent => {
            let reference = result.reference.map(|r| r.to_string());
            let _ = db
                .update_outbound_message(id, MessageStatus::Sent, reference.as_deref(), None)
                .await;
            DispatchOutcome {
                status: MessageStatus::Sent,
                reference,
            }
        }
        MessageStatus::Failed => {
            let detail = result
                .error_code
                .map(|code| code.to_string())
                .or(result.error);
            let _ = db
                .update_outbound_message(id, MessageStatus::Failed, None, detail.as_deref())
                .await;
            DispatchOutcome {
                status: MessageStatus::Failed,
                reference: None,
            }
        }
        MessageStatus::Queued => DispatchOutcome {
            status: MessageStatus::Queued,
            reference: None,
        },
    };

    if outcome.status != MessageStatus::Queued {
        events.publish(ServiceEvent::MessageStatus(MessageStatusEvent {
            id,
            status: outcome.status,
            reference: outcome.reference.clone(),
        }));
    }

    if let Some(reply) = reply {
        let _ = reply.send(outcome);
    }
}

/// Streams real-time service events to the client as Server-Sent Events.
///
/// Each connected client receives a private subscription to the event bus.
/// Outbound `message_status` transitions and `inbound_sms` arrivals are
/// emitted as named SSE events with a JSON `data` payload. Clients that fall
/// behind the broadcast buffer silently skip the dropped events.
#[utoipa::path(
    get,
    path = "/api/v1/events",
    tag = "events",
    security(("api_key" = [])),
    responses(
        (status = 200, description = "An open text/event-stream of message_status and inbound_sms events", content_type = "text/event-stream"),
        (status = 401, description = "Missing or invalid API key", body = ApiError),
        (status = 429, description = "Rate limit exceeded", body = ApiError,
            headers(("Retry-After" = String, description = "Seconds until requests are permitted again"))),
        (status = 500, description = "Internal server error", body = ApiError)
    )
)]
async fn events_handler(
    State(state): State<ApiState>,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let rx = state.events.subscribe();
    let stream = BroadcastStream::new(rx).filter_map(|item| {
        let event = item.ok()?;
        sse_event(&event).ok().map(Ok)
    });
    Sse::new(stream).keep_alive(KeepAlive::default())
}

fn sse_event(event: &ServiceEvent) -> Result<Event, axum::Error> {
    match event {
        ServiceEvent::MessageStatus(payload) => {
            Event::default().event(event.name()).json_data(payload)
        }
        ServiceEvent::InboundSms(payload) => {
            Event::default().event(event.name()).json_data(payload)
        }
    }
}

/// List received inbound SMS messages, newest first.
#[utoipa::path(
    get,
    path = "/api/v1/messages/inbound",
    tag = "messages",
    security(("api_key" = [])),
    responses(
        (status = 200, description = "Inbound messages ordered by receipt time descending", body = Vec<InboundMessage>),
        (status = 401, description = "Missing or invalid API key", body = ApiError),
        (status = 429, description = "Rate limit exceeded", body = ApiError,
            headers(("Retry-After" = String, description = "Seconds until requests are permitted again"))),
        (status = 500, description = "Internal server error", body = ApiError),
        (status = 503, description = "Service not ready", body = ApiError,
            headers(("Retry-After" = String, description = "Seconds to wait before retrying")))
    )
)]
async fn inbound_handler(State(state): State<ApiState>) -> Response {
    match state.db.list_inbound_messages().await {
        Ok(messages) => (StatusCode::OK, Json(messages)).into_response(),
        Err(err) => db_error_response(&err, state.retry_after_secs),
    }
}

/// Fetch the current status of a single outbound message by ID.
#[utoipa::path(
    get,
    path = "/api/v1/messages/{id}",
    tag = "messages",
    security(("api_key" = [])),
    params(("id" = i64, Path, description = "Outbound message ID")),
    responses(
        (status = 200, description = "The outbound message record", body = OutboundMessage),
        (status = 401, description = "Missing or invalid API key", body = ApiError),
        (status = 404, description = "No message with that ID", body = ApiError),
        (status = 429, description = "Rate limit exceeded", body = ApiError,
            headers(("Retry-After" = String, description = "Seconds until requests are permitted again"))),
        (status = 500, description = "Internal server error", body = ApiError),
        (status = 503, description = "Service not ready", body = ApiError,
            headers(("Retry-After" = String, description = "Seconds to wait before retrying")))
    )
)]
async fn outbound_status_handler(State(state): State<ApiState>, Path(id): Path<i64>) -> Response {
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

/// Report overall service health and the serial/SIM connection state.
#[utoipa::path(
    get,
    path = "/health",
    tag = "health",
    responses(
        (status = 200, description = "Service is healthy or degraded", body = HealthResponse),
        (status = 503, description = "Service is unhealthy", body = HealthResponse)
    )
)]
async fn health_handler(State(state): State<ApiState>) -> Response {
    let snapshot = state.modem.status_snapshot();
    let (status, body) = build_health_response(&snapshot);
    (status, Json(body)).into_response()
}

/// Report detailed modem status: signal, registration, and operator.
#[utoipa::path(
    get,
    path = "/status",
    tag = "health",
    responses(
        (status = 200, description = "Modem status, with unavailable fields listed", body = StatusResponse)
    )
)]
async fn status_handler(State(state): State<ApiState>) -> Response {
    let snapshot = state.modem.status_snapshot();
    (StatusCode::OK, Json(build_status_response(&snapshot))).into_response()
}

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

        let events = EventBus::default();
        let modem: SharedModem = Arc::new(modem);
        dispatch_send(
            &db,
            &modem,
            &events,
            created.id,
            "+14155552671".into(),
            "hi".into(),
            None,
        )
        .await;

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

        let events = EventBus::default();
        let modem: SharedModem = Arc::new(modem);
        dispatch_send(
            &db,
            &modem,
            &events,
            created.id,
            "+14155552671".into(),
            "hi".into(),
            None,
        )
        .await;

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

        let events = EventBus::default();
        let modem: SharedModem = Arc::new(modem);
        dispatch_send(
            &db,
            &modem,
            &events,
            created.id,
            "+14155552671".into(),
            "hi".into(),
            None,
        )
        .await;

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

    async fn revoke_key(db: &Db, id: i64) {
        sqlx::query("UPDATE api_keys SET revoked = 1 WHERE id = ?")
            .bind(id)
            .execute(db.pool())
            .await
            .unwrap();
    }

    fn test_state(db: Db) -> ApiState {
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
