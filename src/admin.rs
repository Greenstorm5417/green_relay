//! Admin dashboard: auth primitives, session logic, and selection helpers.
//!
//! This file implements the pure / self-contained primitives the admin
//! dashboard is built on (task 11.1):
//!
//! - [`hash_password`] / [`verify_password`] — Argon2 password hashing and
//!   verification (Req 5.2, 5.3).
//! - [`session_valid`] — an administrative session is valid while it has been
//!   active within the last 30 minutes (Req 5.8, 5.9).
//! - [`admin_locked`] — the admin login lockout predicate: 5 failed logins
//!   within any trailing 15-minute window lock the account for 15 minutes
//!   (Req 5.5).
//! - [`recent_activity`] — the dashboard's recent-activity selection: at most
//!   10 entries from the preceding 24 hours, most-recent-first (Req 5.7).
//!
//! The Axum handlers, views, and cookie session middleware that consume these
//! primitives are implemented separately (task 11.6).
//!
//! Validates: Requirements 5.2, 5.3, 5.5, 5.7, 5.8, 5.9

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
    extract::{Form, Path, State},
    http::{HeaderMap, HeaderValue, StatusCode, header},
    response::{Html, IntoResponse, Redirect, Response},
    routing::{get, post},
};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::Row;

use crate::auth::key_identifier;
use crate::db::{Db, DbError};
use crate::health::{ModemStatusSnapshot, ServiceHealth, derive_health};

// ---------------------------------------------------------------------------
// Password hashing (task 11.1, Requirements 5.2, 5.3)
// ---------------------------------------------------------------------------

/// Hash a plaintext password using Argon2 with a freshly generated random salt.
///
/// The returned string is the standard PHC-format encoding of the Argon2 hash
/// (algorithm, parameters, salt, and digest), suitable for storing in the
/// `password_hash` column of the `ADMIN_USERS` table. Because a random salt is
/// used, hashing the same password twice produces different strings; the hash
/// is never equal to the plaintext password (Req 5.2).
pub fn hash_password(password: &str) -> String {
    let salt = SaltString::generate(&mut OsRng);
    Argon2::default()
        .hash_password(password.as_bytes(), &salt)
        .map(|hash| hash.to_string())
        // Hashing only fails for invalid parameters; `Argon2::default()` and a
        // generated salt are always valid, so the fallback is unreachable. An
        // empty hash here would simply fail every later verification (closed),
        // so this stays panic-free without weakening security.
        .unwrap_or_default()
}

/// Verify a plaintext `password` against a stored Argon2 `stored_hash`.
///
/// Returns `true` only when `stored_hash` is a well-formed Argon2 hash and the
/// supplied password matches it (Req 5.3). A malformed or unparseable stored
/// hash yields `false` rather than an error, so a corrupt record can never be
/// treated as a successful authentication.
pub fn verify_password(password: &str, stored_hash: &str) -> bool {
    let parsed = match PasswordHash::new(stored_hash) {
        Ok(hash) => hash,
        Err(_) => return false,
    };
    Argon2::default()
        .verify_password(password.as_bytes(), &parsed)
        .is_ok()
}

// ---------------------------------------------------------------------------
// Session validity (task 11.1, Requirements 5.8, 5.9)
// ---------------------------------------------------------------------------

/// Maximum idle time before an administrative session expires (Req 5.9).
pub const SESSION_IDLE_TIMEOUT: Duration = Duration::from_secs(30 * 60);

/// An authenticated administrative session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Session {
    /// Identifier of the authenticated admin user.
    pub admin_id: i64,
    /// Instant of the session's most recent activity.
    pub last_activity: Instant,
}

/// Determine whether an administrative session is still valid at `now`.
///
/// A session is valid if and only if the time elapsed since its last activity
/// is strictly less than 30 minutes. At or beyond the 30-minute idle boundary
/// the session has expired and re-authentication is required (Req 5.8, 5.9).
pub fn session_valid(session: &Session, now: Instant) -> bool {
    now.saturating_duration_since(session.last_activity) < SESSION_IDLE_TIMEOUT
}

// ---------------------------------------------------------------------------
// Admin login lockout (task 11.1, Requirement 5.5)
// ---------------------------------------------------------------------------

/// Number of failed logins within the failure window that triggers a lockout
/// (Req 5.5).
pub const ADMIN_MAX_FAILURES: usize = 5;

/// Trailing window over which failed logins are counted toward a lockout
/// (Req 5.5).
pub const ADMIN_FAILURE_WINDOW: Duration = Duration::from_secs(15 * 60);

/// Duration an account remains locked once a lockout is triggered (Req 5.5).
pub const ADMIN_LOCK_DURATION: Duration = Duration::from_secs(15 * 60);

/// Determine whether an admin account is locked at `now`, given the timeline
/// of its failed-login instants.
///
/// The account is locked if and only if there exists a failure that was the
/// 5th (or later) failure within a trailing 15-minute window — the trigger —
/// such that `now` falls within the 15-minute lock window that begins at that
/// trigger. The lock takes effect at the trigger instant and remains in effect
/// for exactly 15 minutes, expiring at the boundary (Req 5.5).
///
/// `failures` need not be sorted; an empty timeline is never locked.
pub fn admin_locked(failures: &[Instant], now: Instant) -> bool {
    let mut sorted: Vec<Instant> = failures.to_vec();
    sorted.sort_unstable();

    for (i, &trigger) in sorted.iter().enumerate() {
        // Count failures within the trailing 15-minute window ending at this
        // failure. Because `sorted` is ascending, every earlier entry is <=
        // `trigger`, so the elapsed duration is well-defined and non-negative.
        let count = sorted
            .iter()
            .take(i.saturating_add(1))
            .filter(|&&f| trigger.saturating_duration_since(f) <= ADMIN_FAILURE_WINDOW)
            .count();

        // A qualifying trigger locks the account for [trigger, trigger + 15m).
        if count >= ADMIN_MAX_FAILURES
            && now >= trigger
            && now.saturating_duration_since(trigger) < ADMIN_LOCK_DURATION
        {
            return true;
        }
    }

    false
}

// ---------------------------------------------------------------------------
// Recent-activity selection (task 11.1, Requirement 5.7)
// ---------------------------------------------------------------------------

/// Maximum number of recent-activity entries shown on the dashboard (Req 5.7).
pub const RECENT_ACTIVITY_LIMIT: usize = 10;

/// A single message-activity entry displayed on the admin dashboard.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActivityEntry {
    /// When the activity occurred, in UTC.
    pub timestamp: DateTime<Utc>,
    /// A human-readable description of the activity.
    pub description: String,
}

/// Select the entries to display in the dashboard's recent-activity panel.
///
/// Returns at most 10 entries, each having a timestamp within the 24 hours
/// preceding `now` (entries in the future or older than 24 hours are
/// excluded), ordered most-recent-first (Req 5.7).
pub fn recent_activity(entries: &[ActivityEntry], now: DateTime<Utc>) -> Vec<ActivityEntry> {
    let window = chrono::Duration::hours(24);

    let mut recent: Vec<ActivityEntry> = entries
        .iter()
        .filter(|entry| {
            let age = now.signed_duration_since(entry.timestamp);
            // Within the preceding 24 hours: not in the future and no older
            // than the 24-hour window.
            age >= chrono::Duration::zero() && age <= window
        })
        .cloned()
        .collect();

    // Most-recent-first.
    recent.sort_by_key(|e| std::cmp::Reverse(e.timestamp));
    recent.truncate(RECENT_ACTIVITY_LIMIT);
    recent
}

// ===========================================================================
// Admin handlers, session middleware, and views (task 11.6)
//
// Requirements 5.1, 5.4, 5.6, 5.7, 5.10. This section wires the pure
// primitives above (password verification, session validity, the login
// lockout predicate, and recent-activity selection) into Axum handlers with
// cookie-based session tokens, backed by the SQLite persistence layer.
//
// The HTTP-independent core (login, authorization, key management, dashboard
// assembly) is factored into plain async functions so it can be exercised
// directly in tests without a full HTTP harness; the Axum handlers are thin
// wrappers over that core.
// ===========================================================================

/// Name of the cookie carrying the opaque administrative session token.
pub const SESSION_COOKIE: &str = "admin_session";

// ---------------------------------------------------------------------------
// Session store
// ---------------------------------------------------------------------------

/// In-memory store mapping opaque session tokens to their [`Session`].
///
/// Tokens are high-entropy random hex strings; the plaintext token lives only
/// in the client's cookie and as a map key here. Validation refreshes the
/// session's `last_activity` so an active admin keeps direct access (Req 5.8),
/// and expires (removes) a session that has been idle for 30 minutes
/// (Req 5.9).
#[derive(Default)]
pub struct SessionStore {
    sessions: HashMap<String, Session>,
}

impl SessionStore {
    /// Create a new, empty store.
    pub fn new() -> Self {
        SessionStore {
            sessions: HashMap::new(),
        }
    }

    /// Establish a new session for `admin_id` active as of `now`, returning the
    /// freshly generated session token (Req 5.1).
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

    /// Validate `token` at `now`. Returns the associated `admin_id` and
    /// refreshes the session's activity when the session exists and is still
    /// within the 30-minute idle window (Req 5.8); otherwise the session is
    /// removed (if present) and `None` is returned (Req 5.9).
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

    /// Remove a session (e.g. on logout).
    pub fn remove(&mut self, token: &str) {
        self.sessions.remove(token);
    }
}

// ---------------------------------------------------------------------------
// Admin login failure tracking / lockout
// ---------------------------------------------------------------------------

/// Per-account failed-login tracker driving the lockout predicate
/// ([`admin_locked`]): 5 failures within any trailing 15-minute window lock
/// the account for 15 minutes (Req 5.5).
#[derive(Debug, Default)]
pub struct AdminLoginTracker {
    failures: HashMap<String, Vec<Instant>>,
}

impl AdminLoginTracker {
    /// Create an empty tracker.
    pub fn new() -> Self {
        AdminLoginTracker {
            failures: HashMap::new(),
        }
    }

    /// Whether `username` is currently locked out at `now` (Req 5.5).
    pub fn is_locked(&self, username: &str, now: Instant) -> bool {
        self.failures
            .get(username)
            .is_some_and(|f| admin_locked(f, now))
    }

    /// Record a failed login for `username` at `now`, pruning entries that can
    /// no longer influence any future lockout decision.
    pub fn record_failure(&mut self, username: &str, now: Instant) {
        let history = self.failures.entry(username.to_string()).or_default();
        history.push(now);
        let horizon = ADMIN_FAILURE_WINDOW.saturating_add(ADMIN_LOCK_DURATION);
        history.retain(|t| now.saturating_duration_since(*t) <= horizon);
    }

    /// Clear the failure history for `username` after a successful login.
    pub fn record_success(&mut self, username: &str) {
        self.failures.remove(username);
    }
}

// ---------------------------------------------------------------------------
// Modem status provider
// ---------------------------------------------------------------------------

/// Source of the current modem status snapshot for the dashboard.
///
/// The Modem Manager (task 7.3) owns the serial port; the process wiring
/// (task 14) supplies an implementation that returns the latest snapshot.
/// Keeping it behind a trait lets the admin layer stay decoupled from the
/// modem module and be tested with a stub.
pub trait ModemStatusProvider: Send + Sync {
    /// The most recent modem status snapshot.
    fn current(&self) -> ModemStatusSnapshot;
}

// ---------------------------------------------------------------------------
// Shared handler state
// ---------------------------------------------------------------------------

/// Shared state for the admin dashboard handlers.
///
/// Cloning is cheap: the database handle, session store, login tracker, and
/// modem-status provider are all reference-counted, so every clone observes
/// the same sessions and lockout history.
#[derive(Clone)]
pub struct AdminState {
    db: Db,
    sessions: Arc<Mutex<SessionStore>>,
    login_tracker: Arc<Mutex<AdminLoginTracker>>,
    modem: Arc<dyn ModemStatusProvider>,
}

impl AdminState {
    /// Build admin state from a database handle and a modem-status provider.
    pub fn new(db: Db, modem: Arc<dyn ModemStatusProvider>) -> Self {
        AdminState {
            db,
            sessions: Arc::new(Mutex::new(SessionStore::new())),
            login_tracker: Arc::new(Mutex::new(AdminLoginTracker::new())),
            modem,
        }
    }
}

// ---------------------------------------------------------------------------
// Router
// ---------------------------------------------------------------------------

/// Build the admin dashboard router.
///
/// Routes:
/// - `GET  /admin/login`     — render the login form.
/// - `POST /admin/login`     — authenticate and establish a session (Req 5.1).
/// - `GET  /admin`           — dashboard: health, signal %, recent activity
///   (Req 5.7); redirects to login when unauthenticated (Req 5.10).
/// - `GET  /admin/keys`      — list API keys (Req 5.6).
/// - `POST /admin/keys`      — create a new API key (Req 5.6).
/// - `POST /admin/keys/{id}/revoke` — revoke an API key (Req 5.6).
/// - `POST /admin/logout`    — clear the current session.
pub fn router(state: AdminState) -> Router {
    Router::new()
        .route("/admin/login", get(login_form).post(login_submit))
        .route("/admin/logout", post(logout))
        .route("/admin", get(dashboard))
        .route("/admin/keys", get(keys_view).post(keys_create))
        .route("/admin/keys/{id}/revoke", post(keys_revoke))
        .with_state(state)
}

// ---------------------------------------------------------------------------
// Login
// ---------------------------------------------------------------------------

/// Login form fields.
#[derive(Debug, Deserialize)]
pub struct LoginForm {
    /// Submitted username.
    pub username: String,
    /// Submitted plaintext password (never stored or logged).
    pub password: String,
}

/// Outcome of a login attempt produced by [`perform_login`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LoginResult {
    /// Credentials matched; carries the new session token (Req 5.1).
    Success { token: String },
    /// Credentials did not match a stored admin record (Req 5.4).
    Failed,
    /// The account is locked out due to repeated failures (Req 5.5).
    LockedOut,
}

/// Authenticate an admin login against the database and update session /
/// lockout state, auditing the result.
///
/// `now` is the monotonic clock used for session/lockout bookkeeping and
/// `now_utc` is the wall-clock time recorded in the audit log. On success a
/// session is established and its token returned (Req 5.1). A mismatch (or
/// unknown user) records the attempt in the audit log (Req 5.4); when the
/// failure trips the lockout threshold the lockout is also audited (Req 5.5).
pub async fn perform_login(
    state: &AdminState,
    username: &str,
    password: &str,
    now: Instant,
    now_utc: DateTime<Utc>,
) -> Result<LoginResult, DbError> {
    // A locked-out account is rejected before any password work (Req 5.5).
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

    // Look up the stored password hash for this username.
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

    // Record the failure and determine whether it just triggered a lockout.
    let now_locked = {
        let mut tracker = state
            .login_tracker
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        tracker.record_failure(username, now);
        tracker.is_locked(username, now)
    };

    // Failed-login attempt is always audited (Req 5.4).
    audit(
        &state.db,
        "admin_login_failed",
        None,
        Some(&format!("failed login for `{username}`")),
        now_utc,
    )
    .await?;

    // A newly tripped lockout is audited separately (Req 5.5).
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

/// `GET /admin/login` — render the login form.
async fn login_form() -> Html<String> {
    Html(render_login(None))
}

/// `POST /admin/login` — authenticate and, on success, set the session cookie
/// and redirect to the dashboard (Req 5.1); otherwise re-render the form with
/// an authentication error (Req 5.4, 5.5).
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

/// `POST /admin/logout` — clear the current session and return to login.
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

// ---------------------------------------------------------------------------
// Session authorization (redirect-to-login middleware, Req 5.10)
// ---------------------------------------------------------------------------

/// Result of authorizing a request against the session store.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Authz {
    /// A valid, active session exists for this `admin_id`.
    Authorized(i64),
    /// No valid session — the request must be redirected to login (Req 5.10).
    Redirect,
}

/// Authorize a protected request from its headers at `now`.
///
/// Reads the session token from the `Cookie` header and validates it against
/// the store, refreshing activity on success (Req 5.8). A missing, unknown, or
/// expired session yields [`Authz::Redirect`] (Req 5.9, 5.10).
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

// ---------------------------------------------------------------------------
// Dashboard (Req 5.7)
// ---------------------------------------------------------------------------

/// Assembled data shown on the dashboard.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DashboardData {
    /// Overall modem health: online, offline, or error.
    pub health: ServiceHealth,
    /// Signal quality as a 0..=100 percentage, when known.
    pub signal_percent: Option<u8>,
    /// The 10 most recent message-activity entries from the last 24 hours.
    pub recent: Vec<ActivityEntry>,
}

/// Gather the dashboard's modem health, signal percentage, and recent activity
/// (Req 5.7). Health and signal come from the modem-status provider; recent
/// activity is read from the message tables and filtered to the last 24 hours.
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

/// Read recent message activity (outbound + inbound) from the database within
/// the 24 hours preceding `now_utc`, as raw [`ActivityEntry`] values for
/// [`recent_activity`] to filter and order.
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

/// `GET /admin` — dashboard view, gated by a valid session (Req 5.7, 5.10).
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

// ---------------------------------------------------------------------------
// API key management (Req 5.6)
// ---------------------------------------------------------------------------

/// A view of a stored API key for display (never includes the plaintext key).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ApiKeyView {
    /// Primary key.
    pub id: i64,
    /// Non-reversible identifier (SHA-256 hex), safe to display and audit.
    pub key_identifier: String,
    /// Optional per-key custom rate limit.
    pub custom_rate_limit: Option<u32>,
    /// Whether the key has been revoked.
    pub revoked: bool,
    /// Creation timestamp.
    pub created_at: DateTime<Utc>,
}

/// Create a new API key, persisting only its hash and non-reversible
/// identifier, and return the one-time plaintext key alongside its stored
/// view (Req 5.6). The plaintext is shown to the admin exactly once and is
/// never persisted or logged.
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

/// List all stored API keys for the management view (Req 5.6).
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

/// Revoke the API key with the given `id` (Req 5.6). Returns the number of
/// rows affected so callers can detect an unknown id.
pub async fn revoke_api_key(
    state: &AdminState,
    id: i64,
    now_utc: DateTime<Utc>,
) -> Result<u64, DbError> {
    // Capture the identifier first so the audit record names the revoked key.
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

/// `GET /admin/keys` — render the API key management view (Req 5.6, 5.10).
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

/// `POST /admin/keys` — create a new API key and show it once (Req 5.6, 5.10).
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

/// `POST /admin/keys/{id}/revoke` — revoke a key and return to the list
/// (Req 5.6, 5.10).
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
// Audit helper
// ---------------------------------------------------------------------------

/// Insert a record into the `audit_log` table. Audit failures are surfaced as
/// [`DbError`] so the caller can decide how to react; no plaintext credential
/// is ever passed here (Req 5.4, 5.5).
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

// ---------------------------------------------------------------------------
// Cookies and tokens
// ---------------------------------------------------------------------------

/// Generate a high-entropy random session/API token as a 64-char hex string.
fn random_token() -> String {
    let mut bytes = [0u8; 32];
    OsRng.fill_bytes(&mut bytes);
    to_hex(&bytes)
}

/// Lowercase hex-encode a byte slice.
fn to_hex(bytes: &[u8]) -> String {
    use core::fmt::Write;
    let mut s = String::with_capacity(bytes.len().saturating_mul(2));
    for b in bytes {
        let _ = write!(s, "{b:02x}");
    }
    s
}

/// Build the `Set-Cookie` value establishing the session cookie. The cookie is
/// `HttpOnly` and `SameSite=Strict`, scoped to the whole site, and expires
/// after the 30-minute idle window (Req 5.8, 5.9).
fn session_cookie(token: &str) -> String {
    format!(
        "{SESSION_COOKIE}={token}; HttpOnly; SameSite=Strict; Path=/; Max-Age={}",
        SESSION_IDLE_TIMEOUT.as_secs()
    )
}

/// Build the `Set-Cookie` value that clears the session cookie on logout.
fn clear_cookie() -> String {
    format!("{SESSION_COOKIE}=; HttpOnly; SameSite=Strict; Path=/; Max-Age=0")
}

/// Extract the session token from a request's `Cookie` header, if present.
fn session_token_from_headers(headers: &HeaderMap) -> Option<String> {
    let cookie_header = headers.get(header::COOKIE)?.to_str().ok()?;
    parse_cookie(cookie_header, SESSION_COOKIE)
}

/// Find the value of `name` in a `Cookie` header value of the form
/// `a=1; b=2; c=3`.
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

// ---------------------------------------------------------------------------
// Views (server-rendered HTML)
// ---------------------------------------------------------------------------

/// Minimal HTML-escape for interpolated dynamic values.
fn esc(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// Render the login page, optionally with an error banner.
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

/// Render the textual label for an overall health verdict.
fn health_label(health: ServiceHealth) -> &'static str {
    match health {
        ServiceHealth::Healthy => "online",
        ServiceHealth::Degraded => "degraded",
        ServiceHealth::Unhealthy => "offline / error",
    }
}

/// Render the dashboard view (Req 5.7).
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

/// Render the API key management view (Req 5.6). When `new_key` is supplied it
/// is shown once as the freshly created plaintext key.
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

    // -- password hashing ---------------------------------------------------

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
        // Random salt => the same password hashes to different strings.
        assert_ne!(a, b);
        // Both still verify against the original password.
        assert!(verify_password(password, &a));
        assert!(verify_password(password, &b));
    }

    #[test]
    fn verify_returns_false_for_malformed_hash() {
        assert!(!verify_password("anything", "not-a-valid-phc-hash"));
    }

    // -- session validity ---------------------------------------------------

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

    // -- admin lockout ------------------------------------------------------

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
        // Five failures spread across 10 minutes (< 15-minute window).
        let failures: Vec<Instant> = (0..5)
            .map(|i| start + Duration::from_secs(i * 120))
            .collect();
        let trigger = start + Duration::from_secs(4 * 120); // 5th failure
        // Locked immediately at the trigger.
        assert!(admin_locked(&failures, trigger));
        // Still locked just before the 15-minute lock expires.
        assert!(admin_locked(
            &failures,
            trigger + ADMIN_LOCK_DURATION - Duration::from_secs(1)
        ));
        // Unlocked once the 15-minute lock has elapsed.
        assert!(!admin_locked(&failures, trigger + ADMIN_LOCK_DURATION));
    }

    #[test]
    fn not_locked_when_failures_span_more_than_window() {
        let start = Instant::now();
        // Five failures spread over 20 minutes: no trailing 15-minute window
        // ever contains all five.
        let failures: Vec<Instant> = (0..5)
            .map(|i| start + Duration::from_secs(i * 300)) // 5 min apart
            .collect();
        // At the last failure only the failures within the prior 15 minutes
        // count (the 0-minute one falls outside), so fewer than five.
        let last = start + Duration::from_secs(4 * 300);
        assert!(!admin_locked(&failures, last));
    }

    // -- recent activity ----------------------------------------------------

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
        // Most-recent-first.
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
        // First entry is the most recent (smallest minutes-ago).
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

    /// Stub modem-status provider returning a fixed snapshot.
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

    /// Seed an admin user with the given plaintext password, returning its id.
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

    // -- login --------------------------------------------------------------

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

        // The returned token authorizes a protected request (Req 5.1, 5.8).
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
        // Failed login attempt is recorded in the audit log (Req 5.4).
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

        // Five failures within the window: the fifth trips the lockout.
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

        // Even the correct password is rejected while locked (Req 5.5).
        let during = base + Duration::from_secs(60);
        let blocked = perform_login(&state, "admin", "correct", during, Utc::now())
            .await
            .unwrap();
        assert_eq!(blocked, LoginResult::LockedOut);
    }

    // -- session authorization / redirect ----------------------------------

    #[tokio::test]
    async fn unauthenticated_request_redirects() {
        let state = test_state().await;
        // No cookie at all.
        let headers = HeaderMap::new();
        assert_eq!(authorize(&state, &headers, Instant::now()), Authz::Redirect);

        // Unknown token.
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

        // Still valid just under the idle timeout (Req 5.8).
        let active = start + SESSION_IDLE_TIMEOUT - Duration::from_secs(1);
        assert_eq!(authorize(&state, &headers, active), Authz::Authorized(id));

        // After 30 minutes of inactivity the session expires (Req 5.9).
        // (Activity was refreshed at `active`, so measure from there.)
        let expired = active + SESSION_IDLE_TIMEOUT;
        assert_eq!(authorize(&state, &headers, expired), Authz::Redirect);
    }

    // -- API key management -------------------------------------------------

    #[tokio::test]
    async fn create_list_and_revoke_keys() {
        let state = test_state().await;

        // Create two keys (Req 5.6).
        let (plaintext, view) = create_api_key(&state, Utc::now()).await.unwrap();
        assert!(plaintext.starts_with("sk_"));
        // The stored identifier is never the plaintext key (Req 3.5).
        assert_ne!(view.key_identifier, plaintext);
        let _ = create_api_key(&state, Utc::now()).await.unwrap();

        let keys = list_api_keys(&state).await.unwrap();
        assert_eq!(keys.len(), 2);
        assert!(keys.iter().all(|k| !k.revoked));

        // Revoke the first key.
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

    // -- dashboard ----------------------------------------------------------

    #[tokio::test]
    async fn dashboard_reports_health_signal_and_activity() {
        let state = test_state().await;

        // Seed a recent outbound message so activity is non-empty.
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

    // -- cookie parsing -----------------------------------------------------

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
