//! The JSON admin API consumed by the exported web-ui front-end: session check,
//! login/logout, dashboard, and key management, plus the camelCase response
//! shapes the front-end expects.

use std::time::Instant;

use axum::{
    extract::{Json, Path, State},
    http::{HeaderMap, HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
};
use chrono::Utc;
use serde::{Deserialize, Serialize};

use crate::health::{ModemStatusSnapshot, ServiceHealth, SimStatus, derive_health};

use super::AdminState;
use super::dashboard::{recent_activity, recent_message_activity};
use super::keys::{ApiKeyView, create_api_key, list_api_keys, revoke_api_key};
use super::login::{LoginResult, perform_login};
use super::session::{Authz, authorize, clear_cookie, session_cookie, session_token_from_headers};

/// Login request body posted by the admin panel.
#[derive(Debug, Deserialize)]
pub(crate) struct ApiLoginRequest {
    username: String,
    password: String,
}

/// Generic error envelope: the front-end reads the `error` field.
#[derive(Debug, Serialize)]
struct ApiErrorBody {
    error: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ModemStatusJson {
    serial_connected: bool,
    sim_status: &'static str,
    registered: bool,
    responsive: bool,
    signal_percent: Option<u8>,
    operator: Option<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ActivityEntryJson {
    timestamp: String,
    description: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct DashboardJson {
    health: &'static str,
    modem: ModemStatusJson,
    activity: Vec<ActivityEntryJson>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ApiKeyJson {
    id: i64,
    key_identifier: String,
    custom_rate_limit: Option<u32>,
    revoked: bool,
    created_at: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct CreatedApiKeyJson {
    plaintext: String,
    key: ApiKeyJson,
}

fn health_json(health: ServiceHealth) -> &'static str {
    match health {
        ServiceHealth::Healthy => "healthy",
        ServiceHealth::Degraded => "degraded",
        ServiceHealth::Unhealthy => "unhealthy",
    }
}

fn sim_status_json(status: SimStatus) -> &'static str {
    match status {
        SimStatus::Ready => "ready",
        SimStatus::NotReady => "not_ready",
        SimStatus::Unknown => "unknown",
    }
}

fn modem_json(snapshot: &ModemStatusSnapshot) -> ModemStatusJson {
    ModemStatusJson {
        serial_connected: snapshot.serial_connected,
        sim_status: sim_status_json(snapshot.sim_status),
        registered: snapshot.registered,
        responsive: snapshot.responsive,
        signal_percent: snapshot.signal_percent,
        operator: snapshot.operator.clone(),
    }
}

fn api_key_json(view: &ApiKeyView) -> ApiKeyJson {
    ApiKeyJson {
        id: view.id,
        key_identifier: view.key_identifier.clone(),
        custom_rate_limit: view.custom_rate_limit,
        revoked: view.revoked,
        created_at: view.created_at.to_rfc3339(),
    }
}

/// Compact 500 response with a JSON error envelope.
fn json_server_error(message: &str) -> Response {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(ApiErrorBody {
            error: message.to_string(),
        }),
    )
        .into_response()
}

/// `GET /api/admin/session` — 200 when the session cookie is valid, else 401.
pub(crate) async fn api_session(State(state): State<AdminState>, headers: HeaderMap) -> Response {
    match authorize(&state, &headers, Instant::now()) {
        Authz::Authorized(_) => StatusCode::OK.into_response(),
        Authz::Redirect => StatusCode::UNAUTHORIZED.into_response(),
    }
}

/// `POST /api/admin/login` — authenticate and set the session cookie.
pub(crate) async fn api_login(
    State(state): State<AdminState>,
    Json(body): Json<ApiLoginRequest>,
) -> Response {
    match perform_login(
        &state,
        &body.username,
        &body.password,
        Instant::now(),
        Utc::now(),
    )
    .await
    {
        Ok(LoginResult::Success { token }) => {
            let mut response = StatusCode::OK.into_response();
            if let Ok(cookie) =
                HeaderValue::from_str(&session_cookie(&token, state.cookie_secure()))
            {
                response.headers_mut().insert(header::SET_COOKIE, cookie);
            }
            response
        }
        Ok(LoginResult::Failed) => (
            StatusCode::UNAUTHORIZED,
            Json(ApiErrorBody {
                error: "Invalid username or password.".to_string(),
            }),
        )
            .into_response(),
        Ok(LoginResult::LockedOut) => (
            StatusCode::TOO_MANY_REQUESTS,
            Json(ApiErrorBody {
                error: "Account temporarily locked after repeated failed logins.".to_string(),
            }),
        )
            .into_response(),
        Err(_) => json_server_error("A server error occurred. Please try again."),
    }
}

/// `POST /api/admin/logout` — drop the session and clear the cookie.
pub(crate) async fn api_logout(State(state): State<AdminState>, headers: HeaderMap) -> Response {
    if let Some(token) = session_token_from_headers(&headers) {
        state
            .sessions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&token);
    }
    let mut response = StatusCode::OK.into_response();
    if let Ok(cookie) = HeaderValue::from_str(&clear_cookie(state.cookie_secure())) {
        response.headers_mut().insert(header::SET_COOKIE, cookie);
    }
    response
}

/// `GET /api/admin/dashboard` — modem status, health, and recent activity.
pub(crate) async fn api_dashboard(State(state): State<AdminState>, headers: HeaderMap) -> Response {
    if authorize(&state, &headers, Instant::now()) == Authz::Redirect {
        return StatusCode::UNAUTHORIZED.into_response();
    }

    let now = Utc::now();
    let snapshot = state.modem.current();
    let recent = match recent_message_activity(&state.db, now).await {
        Ok(entries) => entries,
        Err(_) => return json_server_error("Failed to load dashboard."),
    };

    let activity = recent_activity(&recent, now)
        .into_iter()
        .map(|entry| ActivityEntryJson {
            timestamp: entry.timestamp.to_rfc3339(),
            description: entry.description,
        })
        .collect();

    Json(DashboardJson {
        health: health_json(derive_health(&snapshot)),
        modem: modem_json(&snapshot),
        activity,
    })
    .into_response()
}

/// `GET /api/admin/keys` — list all API keys.
pub(crate) async fn api_keys_list(State(state): State<AdminState>, headers: HeaderMap) -> Response {
    if authorize(&state, &headers, Instant::now()) == Authz::Redirect {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    match list_api_keys(&state).await {
        Ok(keys) => {
            let body: Vec<ApiKeyJson> = keys.iter().map(api_key_json).collect();
            Json(body).into_response()
        }
        Err(_) => json_server_error("Unable to load API keys."),
    }
}

/// `POST /api/admin/keys` — create a key, returning its one-time plaintext.
pub(crate) async fn api_keys_create(
    State(state): State<AdminState>,
    headers: HeaderMap,
) -> Response {
    if authorize(&state, &headers, Instant::now()) == Authz::Redirect {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    match create_api_key(&state, Utc::now()).await {
        Ok((plaintext, view)) => (
            StatusCode::CREATED,
            Json(CreatedApiKeyJson {
                plaintext,
                key: api_key_json(&view),
            }),
        )
            .into_response(),
        Err(_) => json_server_error("Unable to create API key."),
    }
}

/// `POST /api/admin/keys/{id}/revoke` — revoke a key.
pub(crate) async fn api_keys_revoke(
    State(state): State<AdminState>,
    headers: HeaderMap,
    Path(id): Path<i64>,
) -> Response {
    if authorize(&state, &headers, Instant::now()) == Authz::Redirect {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    match revoke_api_key(&state, id, Utc::now()).await {
        Ok(_) => StatusCode::OK.into_response(),
        Err(_) => json_server_error("Unable to revoke API key."),
    }
}
