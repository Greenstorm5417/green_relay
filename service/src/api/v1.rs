//! Version 1 of the public REST API.
//!
//! Owns the v1 request/response types, route handlers, and the send pipeline.
//! Version-agnostic infrastructure (shared state, authentication, rate
//! limiting, OpenAPI assembly, and the top-level router) lives in the parent
//! [`crate::api`] module. A future v2 would be added as a sibling submodule.

use std::convert::Infallible;
use std::time::Duration;

use axum::{
    Json,
    extract::{Path, State, rejection::JsonRejection},
    http::StatusCode,
    response::{
        IntoResponse, Response,
        sse::{Event, KeepAlive, Sse},
    },
};
use serde::{Deserialize, Serialize};
use tokio::sync::oneshot;
use tokio_stream::{Stream, StreamExt, wrappers::BroadcastStream};
use utoipa_axum::{router::OpenApiRouter, routes};

use crate::db::Db;
use crate::error::ApiError;
use crate::events::{EventBus, MessageStatusEvent, ServiceEvent};
use crate::health::{
    DeliverabilityOutcome, ModemStatusSnapshot, ServiceHealth, SimStatus, deliverability_gate,
    derive_health,
};
use crate::models::{InboundMessage, MessageStatus, OutboundMessage};
use crate::sms::{
    SegmentError, ValidationError, check_required_fields, segment_message, validate_body,
    validate_e164,
};

use super::{ApiState, SharedModem, json_with_retry_after};

/// Maximum time the synchronous send endpoint waits for delivery to complete
/// before falling back to a `202 Accepted` queued response.
const SYNC_SEND_WAIT_SECS: u64 = 30;

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

/// Authenticated, rate-limited routes (everything except `/health`/`/status`).
pub(crate) fn protected_routes() -> OpenApiRouter<ApiState> {
    OpenApiRouter::new()
        .routes(routes!(send_handler))
        .routes(routes!(send_sync_handler))
        .routes(routes!(inbound_handler))
        .routes(routes!(outbound_status_handler))
        .routes(routes!(events_handler))
}

/// Unauthenticated operational routes.
pub(crate) fn public_routes() -> OpenApiRouter<ApiState> {
    OpenApiRouter::new()
        .routes(routes!(health_handler))
        .routes(routes!(status_handler))
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

/// Extracts the `SendRequest` body, returning a `400` response when the request
/// body is missing or not valid JSON. Shared by the async and sync send
/// handlers so the error response is defined in one place.
fn parse_send_request(
    body: Result<Json<SendRequest>, JsonRejection>,
) -> Result<SendRequest, Box<Response>> {
    match body {
        Ok(Json(request)) => Ok(request),
        Err(_) => Err(Box::new(
            (
                StatusCode::BAD_REQUEST,
                Json(ApiError::new("request body must be valid JSON", Vec::new())),
            )
                .into_response(),
        )),
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
    let request = match parse_send_request(body) {
        Ok(request) => request,
        Err(response) => return *response,
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
    let request = match parse_send_request(body) {
        Ok(request) => request,
        Err(response) => return *response,
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
        Ok(segments) => match u8::try_from(segments.len()) {
            Ok(parts) => parts,
            Err(_) => {
                return Err(SendDecision::TooManyParts {
                    required: segments.len(),
                });
            }
        },
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
                .set_outbound_status(id, MessageStatus::Sent, reference.as_deref(), None)
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
                .set_outbound_status(id, MessageStatus::Failed, None, detail.as_deref())
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

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::api::testutil::{
        StubModem, down_snapshot, healthy_snapshot, queued_result, ready_db, request, test_state,
    };
    use crate::modem::SendResult;

    #[tokio::test]
    async fn send_accepts_valid_request_and_persists_queued() {
        let db = ready_db().await;
        let state = test_state(db.clone());

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
        let state = test_state(db);

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
        let state = test_state(db);

        let decision = queue_send(&state, &request(Some("not-a-number"), Some("hi"))).await;
        assert_eq!(
            decision,
            SendDecision::Invalid(ValidationError::InvalidPhoneNumber)
        );
    }

    #[tokio::test]
    async fn send_rejects_overlong_body() {
        let db = ready_db().await;
        let state = test_state(db);

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
}
