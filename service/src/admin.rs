
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use argon2::{
    Argon2,
    password_hash::{
        PasswordHash, PasswordHasher, PasswordVerifier, SaltString,
        rand_core::{OsRng, RngCore},
    },
};
use axum::{
    Router,
    extract::{Form, Json, Path, State},
    http::{HeaderMap, HeaderValue, StatusCode, header},
    response::{Html, IntoResponse, Redirect, Response},
    routing::{get, post},
};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::Row;

use crate::auth::key_identifier;
use crate::db::{Db, DbError};
use crate::health::{ModemStatusSnapshot, ServiceHealth, SimStatus, derive_health};

/// Hashes a password.
pub fn hash_password(password: &str) -> String {
    let salt = SaltString::generate(&mut OsRng);
    Argon2::default()
        .hash_password(password.as_bytes(), &salt)
        .map(|hash| hash.to_string())
        .unwrap_or_default()
}

/// Verifies a password against a stored hash.
pub fn verify_password(password: &str, stored_hash: &str) -> bool {
    let parsed = match PasswordHash::new(stored_hash) {
        Ok(hash) => hash,
        Err(_) => return false,
    };
    Argon2::default()
        .verify_password(password.as_bytes(), &parsed)
        .is_ok()
}

/// The idle timeout duration for admin sessions.
pub const SESSION_IDLE_TIMEOUT: Duration = Duration::from_secs(30 * 60);

/// An admin session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Session {
    /// The ID of the admin.
    pub admin_id: i64,
    /// The timestamp of the last activity.
    pub last_activity: Instant,
}

/// Checks if a session is still valid.
pub fn session_valid(session: &Session, now: Instant) -> bool {
    now.saturating_duration_since(session.last_activity) < SESSION_IDLE_TIMEOUT
}

/// The maximum number of login failures allowed before lock out.
pub const ADMIN_MAX_FAILURES: usize = 5;
/// The window of time in which failures are tracked.
pub const ADMIN_FAILURE_WINDOW: Duration = Duration::from_secs(15 * 60);
/// The duration of an admin lock out.
pub const ADMIN_LOCK_DURATION: Duration = Duration::from_secs(15 * 60);

/// Checks if the admin login is locked.
pub fn admin_locked(failures: &[Instant], now: Instant) -> bool {
    let mut sorted: Vec<Instant> = failures.to_vec();
    sorted.sort_unstable();

    for (i, &trigger) in sorted.iter().enumerate() {
        let count = sorted
            .iter()
            .take(i.saturating_add(1))
            .filter(|&&f| trigger.saturating_duration_since(f) <= ADMIN_FAILURE_WINDOW)
            .count();

        if count >= ADMIN_MAX_FAILURES
            && now >= trigger
            && now.saturating_duration_since(trigger) < ADMIN_LOCK_DURATION
        {
            return true;
        }
    }

    false
}

/// The limit of recent activity entries to return.
pub const RECENT_ACTIVITY_LIMIT: usize = 10;

/// An activity entry recording a recent event.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActivityEntry {
    /// The timestamp of the activity.
    pub timestamp: DateTime<Utc>,
    /// Description of the activity.
    pub description: String,
}

/// Retrieves the recent activities within the timeframe limit.
pub fn recent_activity(entries: &[ActivityEntry], now: DateTime<Utc>) -> Vec<ActivityEntry> {
    let window = chrono::Duration::hours(24);

    let mut recent: Vec<ActivityEntry> = entries
        .iter()
        .filter(|entry| {
            let age = now.signed_duration_since(entry.timestamp);
            age >= chrono::Duration::zero() && age <= window
        })
        .cloned()
        .collect();

    recent.sort_by_key(|e| std::cmp::Reverse(e.timestamp));
    recent.truncate(RECENT_ACTIVITY_LIMIT);
    recent
}

/// Name of the session cookie.
pub const SESSION_COOKIE: &str = "admin_session";

/// A store for active admin sessions.
#[derive(Default)]
pub struct SessionStore {
    sessions: HashMap<String, Session>,
}

impl SessionStore {
    /// Creates a new SessionStore.
    pub fn new() -> Self {
        SessionStore {
            sessions: HashMap::new(),
        }
    }

    /// Creates a new session in the store.
    pub fn create(&mut self, admin_id: i64, now: Instant) -> String {
        let token = random_token();
        self.sessions.insert(
            token.clone(),
            Session {
                admin_id,
                last_activity: now,
            },
        );
        token
    }

    /// Validates a session token.
    pub fn validate(&mut self, token: &str, now: Instant) -> Option<i64> {
        match self.sessions.get_mut(token) {
            Some(session) if session_valid(session, now) => {
                session.last_activity = now;
                Some(session.admin_id)
            }
            Some(_) => {
                self.sessions.remove(token);
                None
            }
            None => None,
        }
    }

    /// Removes a session token from the store.
    pub fn remove(&mut self, token: &str) {
        self.sessions.remove(token);
    }
}

/// Tracker for failed admin login attempts.
#[derive(Debug, Default)]
pub struct AdminLoginTracker {
    failures: HashMap<String, Vec<Instant>>,
}

impl AdminLoginTracker {
    /// Creates a new AdminLoginTracker.
    pub fn new() -> Self {
        AdminLoginTracker {
            failures: HashMap::new(),
        }
    }

    /// Checks if a username is locked out.
    pub fn is_locked(&self, username: &str, now: Instant) -> bool {
        self.failures
            .get(username)
            .is_some_and(|f| admin_locked(f, now))
    }

    /// Records a failed login attempt.
    pub fn record_failure(&mut self, username: &str, now: Instant) {
        let history = self.failures.entry(username.to_string()).or_default();
        history.push(now);
        let horizon = ADMIN_FAILURE_WINDOW.saturating_add(ADMIN_LOCK_DURATION);
        history.retain(|t| now.saturating_duration_since(*t) <= horizon);
    }

    /// Records a successful login, clearing historical failures.
    pub fn record_success(&mut self, username: &str) {
        self.failures.remove(username);
    }
}

/// Provider trait for fetching the modem status.
pub trait ModemStatusProvider: Send + Sync {
    /// Retrieves the current status snapshot of the modem.
    fn current(&self) -> ModemStatusSnapshot;
}

/// Holds the application state for the admin area.
#[derive(Clone)]
pub struct AdminState {
    db: Db,
    sessions: Arc<Mutex<SessionStore>>,
    login_tracker: Arc<Mutex<AdminLoginTracker>>,
    modem: Arc<dyn ModemStatusProvider>,
}

impl AdminState {
    /// Creates a new AdminState.
    pub fn new(db: Db, modem: Arc<dyn ModemStatusProvider>) -> Self {
        AdminState {
            db,
            sessions: Arc::new(Mutex::new(SessionStore::new())),
            login_tracker: Arc::new(Mutex::new(AdminLoginTracker::new())),
            modem,
        }
    }
}

/// Builds the admin routes router.
pub fn router(state: AdminState) -> Router {
    Router::new()
        .route("/admin/login", get(login_form).post(login_submit))
        .route("/admin/logout", post(logout))
        .route("/admin", get(dashboard))
        .route("/admin/keys", get(keys_view).post(keys_create))
        .route("/admin/keys/{id}/revoke", post(keys_revoke))
        // JSON API consumed by the static admin panel (web-ui). Cookie-session
        // authenticated, camelCase payloads matching the front-end types.
        .route("/api/admin/session", get(api_session))
        .route("/api/admin/login", post(api_login))
        .route("/api/admin/logout", post(api_logout))
        .route("/api/admin/dashboard", get(api_dashboard))
        .route("/api/admin/keys", get(api_keys_list).post(api_keys_create))
        .route("/api/admin/keys/{id}/revoke", post(api_keys_revoke))
        .with_state(state)
}

/// The login form input.
#[derive(Debug, Deserialize)]
pub struct LoginForm {
    /// The username.
    pub username: String,
    /// The password.
    pub password: String,
}

/// Represents the outcome of a login attempt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LoginResult {
    /// Login succeeded.
    Success {
        /// The active session token.
        token: String,
    },
    /// Login failed.
    Failed,
    /// The user is locked out.
    LockedOut,
}

/// Performs the login validation.
pub async fn perform_login(
    state: &AdminState,
    username: &str,
    password: &str,
    now: Instant,
    now_utc: DateTime<Utc>,
) -> Result<LoginResult, DbError> {
    let already_locked = {
        let tracker = state
            .login_tracker
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        tracker.is_locked(username, now)
    };
    if already_locked {
        audit(
            &state.db,
            "admin_login_locked_out",
            None,
            Some(&format!("login rejected for locked account `{username}`")),
            now_utc,
        )
        .await?;
        return Ok(LoginResult::LockedOut);
    }

    let row = sqlx::query("SELECT id, password_hash FROM admin_users WHERE username = ?")
        .bind(username)
        .fetch_optional(state.db.pool())
        .await?;

    let credentials_ok = match &row {
        Some(row) => {
            let stored_hash: String = row.try_get("password_hash")?;
            verify_password(password, &stored_hash)
        }
        None => false,
    };

    if let (true, Some(row)) = (credentials_ok, row.as_ref()) {
        let admin_id: i64 = row.try_get("id")?;
        {
            let mut tracker = state
                .login_tracker
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            tracker.record_success(username);
        }
        let token = {
            let mut sessions = state
                .sessions
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            sessions.create(admin_id, now)
        };
        audit(
            &state.db,
            "admin_login_success",
            None,
            Some(&format!("admin `{username}` authenticated")),
            now_utc,
        )
        .await?;
        return Ok(LoginResult::Success { token });
    }

    let now_locked = {
        let mut tracker = state
            .login_tracker
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        tracker.record_failure(username, now);
        tracker.is_locked(username, now)
    };

    audit(
        &state.db,
        "admin_login_failed",
        None,
        Some(&format!("failed login for `{username}`")),
        now_utc,
    )
    .await?;

    if now_locked {
        audit(
            &state.db,
            "admin_login_locked_out",
            None,
            Some(&format!(
                "account `{username}` locked after repeated failures"
            )),
            now_utc,
        )
        .await?;
        return Ok(LoginResult::LockedOut);
    }

    Ok(LoginResult::Failed)
}

async fn login_form() -> Html<String> {
    Html(render_login(None))
}

async fn login_submit(State(state): State<AdminState>, Form(form): Form<LoginForm>) -> Response {
    match perform_login(
        &state,
        &form.username,
        &form.password,
        Instant::now(),
        Utc::now(),
    )
    .await
    {
        Ok(LoginResult::Success { token }) => {
            let mut response = Redirect::to("/admin").into_response();
            if let Ok(cookie) = HeaderValue::from_str(&session_cookie(&token)) {
                response.headers_mut().insert(header::SET_COOKIE, cookie);
            }
            response
        }
        Ok(LoginResult::Failed) => (
            StatusCode::UNAUTHORIZED,
            Html(render_login(Some("Invalid username or password."))),
        )
            .into_response(),
        Ok(LoginResult::LockedOut) => (
            StatusCode::UNAUTHORIZED,
            Html(render_login(Some(
                "Account temporarily locked due to repeated failed logins. Try again later.",
            ))),
        )
            .into_response(),
        Err(_) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Html(render_login(Some(
                "A server error occurred. Please try again.",
            ))),
        )
            .into_response(),
    }
}

async fn logout(State(state): State<AdminState>, headers: HeaderMap) -> Response {
    if let Some(token) = session_token_from_headers(&headers) {
        state
            .sessions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&token);
    }
    let mut response = Redirect::to("/admin/login").into_response();
    if let Ok(cookie) = HeaderValue::from_str(&clear_cookie()) {
        response.headers_mut().insert(header::SET_COOKIE, cookie);
    }
    response
}

/// Authorization result.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Authz {
    /// Authorized with the given admin ID.
    Authorized(i64),
    /// Redirect to login.
    Redirect,
}

/// Authorizes an admin user from session cookie in headers.
pub fn authorize(state: &AdminState, headers: &HeaderMap, now: Instant) -> Authz {
    let Some(token) = session_token_from_headers(headers) else {
        return Authz::Redirect;
    };
    let mut sessions = state
        .sessions
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    match sessions.validate(&token, now) {
        Some(admin_id) => Authz::Authorized(admin_id),
        None => Authz::Redirect,
    }
}

/// Admin dashboard statistics and recent activity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DashboardData {
    /// Service health status.
    pub health: ServiceHealth,
    /// Signal strength percentage.
    pub signal_percent: Option<u8>,
    /// Recent activities.
    pub recent: Vec<ActivityEntry>,
}

/// Retrieves data required for rendering the admin dashboard.
pub async fn dashboard_data(
    state: &AdminState,
    now_utc: DateTime<Utc>,
) -> Result<DashboardData, DbError> {
    let snapshot = state.modem.current();
    let health = derive_health(&snapshot);
    let signal_percent = snapshot.signal_percent;
    let recent = recent_message_activity(&state.db, now_utc).await?;
    Ok(DashboardData {
        health,
        signal_percent,
        recent: recent_activity(&recent, now_utc),
    })
}

async fn recent_message_activity(
    db: &Db,
    now_utc: DateTime<Utc>,
) -> Result<Vec<ActivityEntry>, DbError> {
    let cutoff = now_utc
        .checked_sub_signed(chrono::Duration::hours(24))
        .unwrap_or(now_utc)
        .to_rfc3339();
    let mut entries = Vec::new();

    let outbound = sqlx::query(
        "SELECT created_at, status, to_number FROM outbound_messages \
         WHERE created_at >= ? ORDER BY created_at DESC LIMIT 50",
    )
    .bind(&cutoff)
    .fetch_all(db.pool())
    .await?;
    for row in &outbound {
        let created_at: String = row.try_get("created_at")?;
        let status: String = row.try_get("status")?;
        let to_number: String = row.try_get("to_number")?;
        if let Ok(ts) = DateTime::parse_from_rfc3339(&created_at) {
            entries.push(ActivityEntry {
                timestamp: ts.with_timezone(&Utc),
                description: format!("Outbound to {to_number} ({status})"),
            });
        }
    }

    let inbound = sqlx::query(
        "SELECT received_at, from_number FROM inbound_messages \
         WHERE received_at >= ? ORDER BY received_at DESC LIMIT 50",
    )
    .bind(&cutoff)
    .fetch_all(db.pool())
    .await?;
    for row in &inbound {
        let received_at: String = row.try_get("received_at")?;
        let from_number: String = row.try_get("from_number")?;
        if let Ok(ts) = DateTime::parse_from_rfc3339(&received_at) {
            entries.push(ActivityEntry {
                timestamp: ts.with_timezone(&Utc),
                description: format!("Inbound from {from_number}"),
            });
        }
    }

    Ok(entries)
}

async fn dashboard(State(state): State<AdminState>, headers: HeaderMap) -> Response {
    if authorize(&state, &headers, Instant::now()) == Authz::Redirect {
        return Redirect::to("/admin/login").into_response();
    }
    match dashboard_data(&state, Utc::now()).await {
        Ok(data) => Html(render_dashboard(&data)).into_response(),
        Err(_) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Html("<h1>Dashboard unavailable</h1>".to_string()),
        )
            .into_response(),
    }
}

/// View model representing an API key.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ApiKeyView {
    /// The unique identifier.
    pub id: i64,
    /// The key's public identifier.
    pub key_identifier: String,
    /// Custom rate limit override.
    pub custom_rate_limit: Option<u32>,
    /// Revocation flag.
    pub revoked: bool,
    /// Key creation timestamp.
    pub created_at: DateTime<Utc>,
}

/// Creates a new API key and returns its plaintext value.
pub async fn create_api_key(
    state: &AdminState,
    now_utc: DateTime<Utc>,
) -> Result<(String, ApiKeyView), DbError> {
    let plaintext = format!("sk_{}", random_token());
    let identifier = key_identifier(&plaintext);
    let created_text = now_utc.to_rfc3339();

    let mut tx = state.db.pool().begin().await?;
    let insert = sqlx::query(
        "INSERT INTO api_keys (key_hash, key_identifier, custom_rate_limit, revoked, created_at) \
         VALUES (?, ?, NULL, 0, ?)",
    )
    .bind(&identifier)
    .bind(&identifier)
    .bind(&created_text)
    .execute(&mut *tx)
    .await;

    let id = match insert {
        Ok(result) => result.last_insert_rowid(),
        Err(e) => {
            let _ = tx.rollback().await;
            return Err(DbError::Sqlx(e));
        }
    };
    tx.commit().await?;

    audit(
        &state.db,
        "api_key_created",
        Some(&identifier),
        Some("admin created a new API key"),
        now_utc,
    )
    .await?;

    Ok((
        plaintext,
        ApiKeyView {
            id,
            key_identifier: identifier,
            custom_rate_limit: None,
            revoked: false,
            created_at: now_utc,
        },
    ))
}

/// Lists all registered API keys.
pub async fn list_api_keys(state: &AdminState) -> Result<Vec<ApiKeyView>, DbError> {
    let rows = sqlx::query(
        "SELECT id, key_identifier, custom_rate_limit, revoked, created_at \
         FROM api_keys ORDER BY created_at DESC, id DESC",
    )
    .fetch_all(state.db.pool())
    .await?;

    let mut keys = Vec::with_capacity(rows.len());
    for row in &rows {
        let created_at: String = row.try_get("created_at")?;
        let custom: Option<i64> = row.try_get("custom_rate_limit")?;
        let revoked: i64 = row.try_get("revoked")?;
        keys.push(ApiKeyView {
            id: row.try_get("id")?,
            key_identifier: row.try_get("key_identifier")?,
            custom_rate_limit: custom.map(|v| v as u32),
            revoked: revoked != 0,
            created_at: DateTime::parse_from_rfc3339(&created_at)
                .map(|dt| dt.with_timezone(&Utc))
                .unwrap_or(Utc::now()),
        });
    }
    Ok(keys)
}

/// Revokes an API key.
pub async fn revoke_api_key(
    state: &AdminState,
    id: i64,
    now_utc: DateTime<Utc>,
) -> Result<u64, DbError> {
    let identifier: Option<String> =
        sqlx::query("SELECT key_identifier FROM api_keys WHERE id = ?")
            .bind(id)
            .fetch_optional(state.db.pool())
            .await?
            .map(|row| row.try_get("key_identifier"))
            .transpose()?;

    let mut tx = state.db.pool().begin().await?;
    let update = sqlx::query("UPDATE api_keys SET revoked = 1 WHERE id = ?")
        .bind(id)
        .execute(&mut *tx)
        .await;
    let affected = match update {
        Ok(result) => result.rows_affected(),
        Err(e) => {
            let _ = tx.rollback().await;
            return Err(DbError::Sqlx(e));
        }
    };
    tx.commit().await?;

    if affected > 0 {
        audit(
            &state.db,
            "api_key_revoked",
            identifier.as_deref(),
            Some("admin revoked an API key"),
            now_utc,
        )
        .await?;
    }
    Ok(affected)
}

async fn keys_view(State(state): State<AdminState>, headers: HeaderMap) -> Response {
    if authorize(&state, &headers, Instant::now()) == Authz::Redirect {
        return Redirect::to("/admin/login").into_response();
    }
    match list_api_keys(&state).await {
        Ok(keys) => Html(render_keys(&keys, None)).into_response(),
        Err(_) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Html("<h1>Unable to load API keys</h1>".to_string()),
        )
            .into_response(),
    }
}

async fn keys_create(State(state): State<AdminState>, headers: HeaderMap) -> Response {
    if authorize(&state, &headers, Instant::now()) == Authz::Redirect {
        return Redirect::to("/admin/login").into_response();
    }
    match create_api_key(&state, Utc::now()).await {
        Ok((plaintext, _)) => match list_api_keys(&state).await {
            Ok(keys) => Html(render_keys(&keys, Some(&plaintext))).into_response(),
            Err(_) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                Html("<h1>Key created but listing failed</h1>".to_string()),
            )
                .into_response(),
        },
        Err(_) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Html("<h1>Unable to create API key</h1>".to_string()),
        )
            .into_response(),
    }
}

async fn keys_revoke(
    State(state): State<AdminState>,
    headers: HeaderMap,
    Path(id): Path<i64>,
) -> Response {
    if authorize(&state, &headers, Instant::now()) == Authz::Redirect {
        return Redirect::to("/admin/login").into_response();
    }
    match revoke_api_key(&state, id, Utc::now()).await {
        Ok(_) => Redirect::to("/admin/keys").into_response(),
        Err(_) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Html("<h1>Unable to revoke API key</h1>".to_string()),
        )
            .into_response(),
    }
}

// ---------------------------------------------------------------------------
// JSON API for the static admin panel (web-ui)
// ---------------------------------------------------------------------------

/// Login request body posted by the admin panel.
#[derive(Debug, Deserialize)]
struct ApiLoginRequest {
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
async fn api_session(State(state): State<AdminState>, headers: HeaderMap) -> Response {
    match authorize(&state, &headers, Instant::now()) {
        Authz::Authorized(_) => StatusCode::OK.into_response(),
        Authz::Redirect => StatusCode::UNAUTHORIZED.into_response(),
    }
}

/// `POST /api/admin/login` — authenticate and set the session cookie.
async fn api_login(
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
            if let Ok(cookie) = HeaderValue::from_str(&session_cookie(&token)) {
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
            StatusCode::LOCKED,
            Json(ApiErrorBody {
                error: "Account temporarily locked after repeated failed logins.".to_string(),
            }),
        )
            .into_response(),
        Err(_) => json_server_error("A server error occurred. Please try again."),
    }
}

/// `POST /api/admin/logout` — drop the session and clear the cookie.
async fn api_logout(State(state): State<AdminState>, headers: HeaderMap) -> Response {
    if let Some(token) = session_token_from_headers(&headers) {
        state
            .sessions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&token);
    }
    let mut response = StatusCode::OK.into_response();
    if let Ok(cookie) = HeaderValue::from_str(&clear_cookie()) {
        response.headers_mut().insert(header::SET_COOKIE, cookie);
    }
    response
}

/// `GET /api/admin/dashboard` — modem status, health, and recent activity.
async fn api_dashboard(State(state): State<AdminState>, headers: HeaderMap) -> Response {
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
async fn api_keys_list(State(state): State<AdminState>, headers: HeaderMap) -> Response {
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
async fn api_keys_create(State(state): State<AdminState>, headers: HeaderMap) -> Response {
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
async fn api_keys_revoke(
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

async fn audit(
    db: &Db,
    event_type: &str,
    key_identifier: Option<&str>,
    detail: Option<&str>,
    now_utc: DateTime<Utc>,
) -> Result<(), DbError> {
    sqlx::query(
        "INSERT INTO audit_log (event_type, key_identifier, detail, created_at) \
         VALUES (?, ?, ?, ?)",
    )
    .bind(event_type)
    .bind(key_identifier)
    .bind(detail)
    .bind(now_utc.to_rfc3339())
    .execute(db.pool())
    .await?;
    Ok(())
}

fn random_token() -> String {
    let mut bytes = [0u8; 32];
    OsRng.fill_bytes(&mut bytes);
    to_hex(&bytes)
}

fn to_hex(bytes: &[u8]) -> String {
    use core::fmt::Write;
    let mut s = String::with_capacity(bytes.len().saturating_mul(2));
    for b in bytes {
        let _ = write!(s, "{b:02x}");
    }
    s
}

fn session_cookie(token: &str) -> String {
    format!(
        "{SESSION_COOKIE}={token}; HttpOnly; SameSite=Strict; Path=/; Max-Age={}",
        SESSION_IDLE_TIMEOUT.as_secs()
    )
}

fn clear_cookie() -> String {
    format!("{SESSION_COOKIE}=; HttpOnly; SameSite=Strict; Path=/; Max-Age=0")
}

fn session_token_from_headers(headers: &HeaderMap) -> Option<String> {
    let cookie_header = headers.get(header::COOKIE)?.to_str().ok()?;
    parse_cookie(cookie_header, SESSION_COOKIE)
}

fn parse_cookie(header_value: &str, name: &str) -> Option<String> {
    header_value.split(';').find_map(|pair| {
        let (k, v) = pair.split_once('=')?;
        if k.trim() == name {
            Some(v.trim().to_string())
        } else {
            None
        }
    })
}

fn esc(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

fn render_login(error: Option<&str>) -> String {
    let banner = match error {
        Some(msg) => format!("<p class=\"error\">{}</p>", esc(msg)),
        None => String::new(),
    };
    format!(
        "<!DOCTYPE html><html><head><title>Admin Login</title></head><body>\
         <h1>Admin Login</h1>{banner}\
         <form method=\"post\" action=\"/admin/login\">\
         <label>Username <input type=\"text\" name=\"username\" autocomplete=\"username\"></label><br>\
         <label>Password <input type=\"password\" name=\"password\" autocomplete=\"current-password\"></label><br>\
         <button type=\"submit\">Sign in</button>\
         </form></body></html>"
    )
}

fn health_label(health: ServiceHealth) -> &'static str {
    match health {
        ServiceHealth::Healthy => "online",
        ServiceHealth::Degraded => "degraded",
        ServiceHealth::Unhealthy => "offline / error",
    }
}

fn render_dashboard(data: &DashboardData) -> String {
    let signal = match data.signal_percent {
        Some(p) => format!("{p}%"),
        None => "unavailable".to_string(),
    };
    let activity = if data.recent.is_empty() {
        "<li>No recent activity.</li>".to_string()
    } else {
        data.recent
            .iter()
            .map(|e| {
                format!(
                    "<li>{} — {}</li>",
                    esc(&e.timestamp.to_rfc3339()),
                    esc(&e.description)
                )
            })
            .collect::<String>()
    };
    format!(
        "<!DOCTYPE html><html><head><title>Admin Dashboard</title></head><body>\
         <h1>Dashboard</h1>\
         <p>Modem health: <strong>{}</strong></p>\
         <p>Signal quality: <strong>{signal}</strong></p>\
         <h2>Recent activity (last 24h)</h2><ul>{activity}</ul>\
         <p><a href=\"/admin/keys\">Manage API keys</a></p>\
         <form method=\"post\" action=\"/admin/logout\"><button type=\"submit\">Sign out</button></form>\
         </body></html>",
        health_label(data.health)
    )
}

fn render_keys(keys: &[ApiKeyView], new_key: Option<&str>) -> String {
    let banner = match new_key {
        Some(key) => format!(
            "<p class=\"new-key\">New API key (copy it now, it will not be shown again): \
             <code>{}</code></p>",
            esc(key)
        ),
        None => String::new(),
    };
    let rows = if keys.is_empty() {
        "<tr><td colspan=\"4\">No API keys.</td></tr>".to_string()
    } else {
        keys.iter()
            .map(|k| {
                let revoke_cell = if k.revoked {
                    "revoked".to_string()
                } else {
                    format!(
                        "<form method=\"post\" action=\"/admin/keys/{}/revoke\">\
                         <button type=\"submit\">Revoke</button></form>",
                        k.id
                    )
                };
                format!(
                    "<tr><td>{}</td><td><code>{}</code></td><td>{}</td><td>{}</td></tr>",
                    k.id,
                    esc(&k.key_identifier),
                    esc(&k.created_at.to_rfc3339()),
                    revoke_cell
                )
            })
            .collect::<String>()
    };
    format!(
        "<!DOCTYPE html><html><head><title>API Keys</title></head><body>\
         <h1>API Keys</h1>{banner}\
         <form method=\"post\" action=\"/admin/keys\"><button type=\"submit\">Create new key</button></form>\
         <table><thead><tr><th>ID</th><th>Identifier</th><th>Created</th><th>Action</th></tr></thead>\
         <tbody>{rows}</tbody></table>\
         <p><a href=\"/admin\">Back to dashboard</a></p>\
         </body></html>"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    #[test]
    fn hash_then_verify_succeeds() {
        let hash = hash_password("correct horse battery staple");
        assert!(verify_password("correct horse battery staple", &hash));
    }

    #[test]
    fn verify_rejects_wrong_password() {
        let hash = hash_password("correct horse battery staple");
        assert!(!verify_password("Tr0ub4dor&3", &hash));
    }

    #[test]
    fn hash_is_not_plaintext_and_is_salted() {
        let password = "hunter2";
        let a = hash_password(password);
        let b = hash_password(password);
        assert_ne!(a, password);
        assert_ne!(a, b);
        assert!(verify_password(password, &a));
        assert!(verify_password(password, &b));
    }

    #[test]
    fn verify_returns_false_for_malformed_hash() {
        assert!(!verify_password("anything", "not-a-valid-phc-hash"));
    }

    #[test]
    fn session_valid_just_under_timeout() {
        let now = Instant::now();
        let session = Session {
            admin_id: 1,
            last_activity: now - (SESSION_IDLE_TIMEOUT - Duration::from_secs(1)),
        };
        assert!(session_valid(&session, now));
    }

    #[test]
    fn session_invalid_at_and_after_timeout() {
        let now = Instant::now();
        let at_boundary = Session {
            admin_id: 1,
            last_activity: now - SESSION_IDLE_TIMEOUT,
        };
        let past_boundary = Session {
            admin_id: 1,
            last_activity: now - (SESSION_IDLE_TIMEOUT + Duration::from_secs(1)),
        };
        assert!(!session_valid(&at_boundary, now));
        assert!(!session_valid(&past_boundary, now));
    }

    #[test]
    fn not_locked_with_fewer_than_five_failures() {
        let start = Instant::now();
        let failures: Vec<Instant> = (0..4)
            .map(|i| start + Duration::from_secs(i * 10))
            .collect();
        assert!(!admin_locked(&failures, start + Duration::from_secs(60)));
    }

    #[test]
    fn locked_after_five_failures_within_window() {
        let start = Instant::now();
        let failures: Vec<Instant> = (0..5)
            .map(|i| start + Duration::from_secs(i * 120))
            .collect();
        let trigger = start + Duration::from_secs(4 * 120);
        assert!(admin_locked(&failures, trigger));
        assert!(admin_locked(
            &failures,
            trigger + ADMIN_LOCK_DURATION - Duration::from_secs(1)
        ));
        assert!(!admin_locked(&failures, trigger + ADMIN_LOCK_DURATION));
    }

    #[test]
    fn not_locked_when_failures_span_more_than_window() {
        let start = Instant::now();
        let failures: Vec<Instant> = (0..5)
            .map(|i| start + Duration::from_secs(i * 300))
            .collect();
        let last = start + Duration::from_secs(4 * 300);
        assert!(!admin_locked(&failures, last));
    }

    fn at(hours_ago: i64, now: DateTime<Utc>) -> DateTime<Utc> {
        now - chrono::Duration::hours(hours_ago)
    }

    #[test]
    fn recent_activity_filters_and_orders() {
        let now = Utc.with_ymd_and_hms(2024, 6, 1, 12, 0, 0).unwrap();
        let entries = vec![
            ActivityEntry {
                timestamp: at(1, now),
                description: "recent".into(),
            },
            ActivityEntry {
                timestamp: at(25, now),
                description: "too old".into(),
            },
            ActivityEntry {
                timestamp: at(3, now),
                description: "older recent".into(),
            },
        ];
        let selected = recent_activity(&entries, now);
        assert_eq!(selected.len(), 2);
        assert_eq!(selected[0].description, "recent");
        assert_eq!(selected[1].description, "older recent");
    }

    #[test]
    fn recent_activity_caps_at_ten() {
        let now = Utc.with_ymd_and_hms(2024, 6, 1, 12, 0, 0).unwrap();
        let entries: Vec<ActivityEntry> = (0..20)
            .map(|i| ActivityEntry {
                timestamp: now - chrono::Duration::minutes(i),
                description: format!("entry {i}"),
            })
            .collect();
        let selected = recent_activity(&entries, now);
        assert_eq!(selected.len(), RECENT_ACTIVITY_LIMIT);
        assert_eq!(selected[0].description, "entry 0");
    }

    #[test]
    fn recent_activity_excludes_future_entries() {
        let now = Utc.with_ymd_and_hms(2024, 6, 1, 12, 0, 0).unwrap();
        let entries = vec![ActivityEntry {
            timestamp: now + chrono::Duration::hours(1),
            description: "future".into(),
        }];
        assert!(recent_activity(&entries, now).is_empty());
    }
}

#[cfg(test)]
mod handler_tests {
    use super::*;
    use crate::health::SimStatus;
    use crate::models::MessageStatus;

    struct StubModem(ModemStatusSnapshot);

    impl ModemStatusProvider for StubModem {
        fn current(&self) -> ModemStatusSnapshot {
            self.0.clone()
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

    async fn test_state() -> AdminState {
        let db = Db::connect_in_memory().await.unwrap();
        db.run_migrations().await.unwrap();
        AdminState::new(db, Arc::new(StubModem(healthy_snapshot())))
    }

    async fn seed_admin(state: &AdminState, username: &str, password: &str) -> i64 {
        let hash = hash_password(password);
        let result = sqlx::query(
            "INSERT INTO admin_users (username, password_hash, failed_attempts, locked_until, created_at) \
             VALUES (?, ?, 0, NULL, ?)",
        )
        .bind(username)
        .bind(&hash)
        .bind(Utc::now().to_rfc3339())
        .execute(state.db.pool())
        .await
        .unwrap();
        result.last_insert_rowid()
    }

    async fn audit_count(state: &AdminState, event_type: &str) -> i64 {
        sqlx::query_scalar("SELECT COUNT(*) FROM audit_log WHERE event_type = ?")
            .bind(event_type)
            .fetch_one(state.db.pool())
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn login_success_establishes_session() {
        let state = test_state().await;
        let id = seed_admin(&state, "admin", "s3cret-pass").await;
        let now = Instant::now();
        let result = perform_login(&state, "admin", "s3cret-pass", now, Utc::now())
            .await
            .unwrap();
        let token = match result {
            LoginResult::Success { token } => token,
            other => panic!("expected success, got {other:?}"),
        };
        let mut headers = HeaderMap::new();
        headers.insert(
            header::COOKIE,
            HeaderValue::from_str(&format!("{SESSION_COOKIE}={token}")).unwrap(),
        );
        assert_eq!(authorize(&state, &headers, now), Authz::Authorized(id));
        assert_eq!(audit_count(&state, "admin_login_success").await, 1);
    }

    #[tokio::test]
    async fn login_failure_is_rejected_and_audited() {
        let state = test_state().await;
        seed_admin(&state, "admin", "correct").await;

        let result = perform_login(&state, "admin", "wrong", Instant::now(), Utc::now())
            .await
            .unwrap();
        assert_eq!(result, LoginResult::Failed);
        assert_eq!(audit_count(&state, "admin_login_failed").await, 1);
    }

    #[tokio::test]
    async fn unknown_user_is_rejected_and_audited() {
        let state = test_state().await;
        let result = perform_login(&state, "ghost", "whatever", Instant::now(), Utc::now())
            .await
            .unwrap();
        assert_eq!(result, LoginResult::Failed);
        assert_eq!(audit_count(&state, "admin_login_failed").await, 1);
    }

    #[tokio::test]
    async fn five_failures_lock_account_and_audit_lockout() {
        let state = test_state().await;
        seed_admin(&state, "admin", "correct").await;
        let base = Instant::now();
        let mut last = LoginResult::Failed;
        for i in 0..5 {
            last = perform_login(
                &state,
                "admin",
                "wrong",
                base + Duration::from_secs(i * 10),
                Utc::now(),
            )
            .await
            .unwrap();
        }
        assert_eq!(last, LoginResult::LockedOut);
        assert!(audit_count(&state, "admin_login_locked_out").await >= 1);
        let during = base + Duration::from_secs(60);
        let blocked = perform_login(&state, "admin", "correct", during, Utc::now())
            .await
            .unwrap();
        assert_eq!(blocked, LoginResult::LockedOut);
    }

    #[tokio::test]
    async fn unauthenticated_request_redirects() {
        let state = test_state().await;
        let headers = HeaderMap::new();
        assert_eq!(authorize(&state, &headers, Instant::now()), Authz::Redirect);
        let mut headers = HeaderMap::new();
        headers.insert(
            header::COOKIE,
            HeaderValue::from_static("admin_session=deadbeef"),
        );
        assert_eq!(authorize(&state, &headers, Instant::now()), Authz::Redirect);
    }

    #[tokio::test]
    async fn expired_session_redirects() {
        let state = test_state().await;
        let id = seed_admin(&state, "admin", "pw").await;
        let start = Instant::now();
        let result = perform_login(&state, "admin", "pw", start, Utc::now())
            .await
            .unwrap();
        let token = match result {
            LoginResult::Success { token } => token,
            other => panic!("expected success, got {other:?}"),
        };

        let mut headers = HeaderMap::new();
        headers.insert(
            header::COOKIE,
            HeaderValue::from_str(&format!("{SESSION_COOKIE}={token}")).unwrap(),
        );

        let active = start + SESSION_IDLE_TIMEOUT - Duration::from_secs(1);
        assert_eq!(authorize(&state, &headers, active), Authz::Authorized(id));

        let expired = active + SESSION_IDLE_TIMEOUT;
        assert_eq!(authorize(&state, &headers, expired), Authz::Redirect);
    }

    #[tokio::test]
    async fn create_list_and_revoke_keys() {
        let state = test_state().await;
        let (plaintext, view) = create_api_key(&state, Utc::now()).await.unwrap();
        assert!(plaintext.starts_with("sk_"));
        assert_ne!(view.key_identifier, plaintext);
        let _ = create_api_key(&state, Utc::now()).await.unwrap();
        let keys = list_api_keys(&state).await.unwrap();
        assert_eq!(keys.len(), 2);
        assert!(keys.iter().all(|k| !k.revoked));
        let target = keys[0].id;
        let affected = revoke_api_key(&state, target, Utc::now()).await.unwrap();
        assert_eq!(affected, 1);
        let keys = list_api_keys(&state).await.unwrap();
        let revoked = keys.iter().find(|k| k.id == target).unwrap();
        assert!(revoked.revoked);
        assert_eq!(audit_count(&state, "api_key_created").await, 2);
        assert_eq!(audit_count(&state, "api_key_revoked").await, 1);
    }

    #[tokio::test]
    async fn revoke_unknown_key_affects_no_rows() {
        let state = test_state().await;
        let affected = revoke_api_key(&state, 9999, Utc::now()).await.unwrap();
        assert_eq!(affected, 0);
    }

    #[tokio::test]
    async fn dashboard_reports_health_signal_and_activity() {
        let state = test_state().await;

        state
            .db
            .create_outbound_message("+14155552671", "hi", MessageStatus::Queued, 1)
            .await
            .unwrap();

        let data = dashboard_data(&state, Utc::now()).await.unwrap();
        assert_eq!(data.health, ServiceHealth::Healthy);
        assert_eq!(data.signal_percent, Some(75));
        assert_eq!(data.recent.len(), 1);
        assert!(data.recent[0].description.contains("Outbound"));
    }

    #[tokio::test]
    async fn dashboard_caps_recent_activity_at_ten() {
        let state = test_state().await;
        for _ in 0..15 {
            state
                .db
                .create_outbound_message("+14155552671", "hi", MessageStatus::Queued, 1)
                .await
                .unwrap();
        }
        let data = dashboard_data(&state, Utc::now()).await.unwrap();
        assert_eq!(data.recent.len(), RECENT_ACTIVITY_LIMIT);
    }

    #[test]
    fn parse_cookie_finds_named_value() {
        assert_eq!(
            parse_cookie("a=1; admin_session=tok123; b=2", SESSION_COOKIE).as_deref(),
            Some("tok123")
        );
        assert_eq!(parse_cookie("a=1; b=2", SESSION_COOKIE), None);
    }

    #[test]
    fn session_cookie_is_httponly_and_scoped() {
        let cookie = session_cookie("abc");
        assert!(cookie.contains("admin_session=abc"));
        assert!(cookie.contains("HttpOnly"));
        assert!(cookie.contains("SameSite=Strict"));
        assert!(cookie.contains("Path=/"));
    }

    #[test]
    fn rendered_views_escape_dynamic_values() {
        let keys = vec![ApiKeyView {
            id: 1,
            key_identifier: "<script>".to_string(),
            custom_rate_limit: None,
            revoked: false,
            created_at: Utc::now(),
        }];
        let html = render_keys(&keys, Some("<b>plain</b>"));
        assert!(!html.contains("<script>"));
        assert!(html.contains("&lt;script&gt;"));
    }
}
