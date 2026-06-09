//! Integration tests for the admin dashboard HTTP flows (task 11.7).
//!
//! These are Axum harness tests: they construct the real admin router with a
//! temp-file SQLite database and a stub modem-status provider, then drive HTTP
//! requests through it with `tower::ServiceExt::oneshot` (no TCP socket is
//! bound). They exercise the request/response surface end to end — routing,
//! the `Form`/`Path` extractors, cookie session handling, the
//! redirect-to-login authorization gate, and the rendered HTML.
//!
//! Covered acceptance criteria:
//! - Req 5.1  — a successful login establishes an authenticated session.
//! - Req 5.4  — bad credentials are rejected with an authentication error and
//!   no session is established.
//! - Req 5.6  — an authenticated admin can create, view, and revoke API keys.
//! - Req 5.10 — a request to a protected view without a valid session is
//!   redirected to the login view.
//!
//! Session *grant* is verified through HTTP here (login -> cookie -> protected
//! access succeeds, then logout invalidates the session so the next request is
//! redirected). The precise 30-minute *inactivity-expiry boundary* (Req 5.9)
//! is clock-driven and is covered by the unit tests in `src/admin.rs`
//! (`session_valid`, `SessionStore::validate`, `authorize`), since the HTTP
//! harness cannot inject a monotonic clock into the handlers.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use chrono::Utc;
use green_relay::admin::{AdminState, ModemStatusProvider, SESSION_COOKIE, hash_password};
use green_relay::db::Db;
use green_relay::health::{ModemStatusSnapshot, SimStatus};
use tower::ServiceExt; // for `oneshot`

// ---------------------------------------------------------------------------
// Test harness
// ---------------------------------------------------------------------------

/// A temp-file SQLite database that is removed (along with its `-wal`/`-shm`
/// sidecars) when the guard is dropped. A file-backed DB is used rather than
/// `:memory:` because the production `Db::connect` uses a multi-connection
/// pool, and each connection to `:memory:` would see a separate database.
struct TempDbFile {
    path: String,
}

impl TempDbFile {
    fn new() -> Self {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let mut path = std::env::temp_dir();
        path.push(format!(
            "sms_admin_it_{}_{}_{}.sqlite",
            std::process::id(),
            n,
            Utc::now().timestamp_nanos_opt().unwrap_or_default()
        ));
        TempDbFile {
            path: path.to_string_lossy().into_owned(),
        }
    }
}

impl Drop for TempDbFile {
    fn drop(&mut self) {
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(format!("{}{}", self.path, suffix));
        }
    }
}

/// Stub modem-status provider returning a fixed, healthy snapshot so the
/// dashboard renders deterministically.
struct StubModem;

impl ModemStatusProvider for StubModem {
    fn current(&self) -> ModemStatusSnapshot {
        ModemStatusSnapshot {
            serial_connected: true,
            sim_status: SimStatus::Ready,
            registered: true,
            responsive: true,
            signal_percent: Some(75),
            operator: Some("Test Carrier".to_string()),
        }
    }
}

/// A built harness: the live router plus the `Db` handle (kept so tests can
/// seed rows and inspect persisted state) and the temp-file guard.
struct Harness {
    router: Router,
    db: Db,
    _temp: TempDbFile,
}

impl Harness {
    /// Build a fresh harness with a migrated database and an admin router.
    async fn build() -> Harness {
        let temp = TempDbFile::new();
        let db = Db::initialize(&temp.path)
            .await
            .expect("initialize temp database");
        let state = AdminState::new(db.clone(), Arc::new(StubModem));
        let router = green_relay::admin::router(state);
        Harness {
            router,
            db,
            _temp: temp,
        }
    }

    /// Seed an admin user with the given plaintext password.
    async fn seed_admin(&self, username: &str, password: &str) {
        let hash = hash_password(password);
        sqlx::query(
            "INSERT INTO admin_users (username, password_hash, failed_attempts, locked_until, created_at) \
             VALUES (?, ?, 0, NULL, ?)",
        )
        .bind(username)
        .bind(&hash)
        .bind(Utc::now().to_rfc3339())
        .execute(self.db.pool())
        .await
        .expect("seed admin user");
    }

    /// Drive a single request through the router, returning the full response.
    async fn send(&self, request: Request<Body>) -> Response {
        let response = self
            .router
            .clone()
            .oneshot(request)
            .await
            .expect("router responds");
        let status = response.status();
        let location = response
            .headers()
            .get(header::LOCATION)
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned);
        let set_cookie = response
            .headers()
            .get(header::SET_COOKIE)
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned);
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("read response body");
        let body = String::from_utf8(bytes.to_vec()).expect("utf-8 body");
        Response {
            status,
            location,
            set_cookie,
            body,
        }
    }
}

/// A flattened response captured for assertions.
struct Response {
    status: StatusCode,
    location: Option<String>,
    set_cookie: Option<String>,
    body: String,
}

impl Response {
    /// Extract the `admin_session` token from the `Set-Cookie` header, if any.
    fn session_token(&self) -> Option<String> {
        let header = self.set_cookie.as_ref()?;
        let prefix = format!("{SESSION_COOKIE}=");
        let rest = header.strip_prefix(&prefix)?;
        let token = rest.split(';').next().unwrap_or("");
        if token.is_empty() {
            None
        } else {
            Some(token.to_string())
        }
    }
}

// -- request builders -------------------------------------------------------

fn get(path: &str) -> Request<Body> {
    Request::builder()
        .method("GET")
        .uri(path)
        .body(Body::empty())
        .unwrap()
}

fn get_with_cookie(path: &str, token: &str) -> Request<Body> {
    Request::builder()
        .method("GET")
        .uri(path)
        .header(header::COOKIE, format!("{SESSION_COOKIE}={token}"))
        .body(Body::empty())
        .unwrap()
}

fn post_form(path: &str, body: &str) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri(path)
        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
        .body(Body::from(body.to_owned()))
        .unwrap()
}

fn post_with_cookie(path: &str, token: &str) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri(path)
        .header(header::COOKIE, format!("{SESSION_COOKIE}={token}"))
        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
        .body(Body::empty())
        .unwrap()
}

/// Log in as the seeded admin and return the granted session token.
async fn login(harness: &Harness, username: &str, password: &str) -> String {
    let response = harness
        .send(post_form(
            "/admin/login",
            &format!("username={username}&password={password}"),
        ))
        .await;
    assert_eq!(
        response.status,
        StatusCode::SEE_OTHER,
        "successful login should redirect"
    );
    response
        .session_token()
        .expect("login sets a session cookie")
}

// ---------------------------------------------------------------------------
// Login success / failure (Req 5.1, 5.4)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn login_success_establishes_session_and_grants_access() {
    let harness = Harness::build().await;
    harness.seed_admin("admin", "s3cret-pass").await;

    // Correct credentials -> 303 redirect to the dashboard, with a session
    // cookie (Req 5.1).
    let login = harness
        .send(post_form(
            "/admin/login",
            "username=admin&password=s3cret-pass",
        ))
        .await;
    assert_eq!(login.status, StatusCode::SEE_OTHER);
    assert_eq!(login.location.as_deref(), Some("/admin"));
    let token = login
        .session_token()
        .expect("a session cookie is set on success");

    // The granted session authorizes a protected view (Req 5.1, session grant).
    let dashboard = harness.send(get_with_cookie("/admin", &token)).await;
    assert_eq!(dashboard.status, StatusCode::OK);
    assert!(
        dashboard.body.contains("Dashboard"),
        "dashboard HTML should render for an authenticated admin"
    );
}

#[tokio::test]
async fn login_failure_is_rejected_with_no_session() {
    let harness = Harness::build().await;
    harness.seed_admin("admin", "correct-pass").await;

    // Wrong password -> 401 with an authentication error and no session
    // cookie establishing access (Req 5.4).
    let response = harness
        .send(post_form(
            "/admin/login",
            "username=admin&password=wrong-pass",
        ))
        .await;
    assert_eq!(response.status, StatusCode::UNAUTHORIZED);
    assert!(response.session_token().is_none(), "no session is granted");
    assert!(
        response.body.contains("Invalid username or password"),
        "an authentication error is shown"
    );

    // An unknown user is likewise rejected (Req 5.4).
    let unknown = harness
        .send(post_form(
            "/admin/login",
            "username=ghost&password=whatever",
        ))
        .await;
    assert_eq!(unknown.status, StatusCode::UNAUTHORIZED);
    assert!(unknown.session_token().is_none());
}

// ---------------------------------------------------------------------------
// Session grant / expiry (Req 5.1, 5.9, 5.10)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn logout_invalidates_session_and_blocks_further_access() {
    let harness = Harness::build().await;
    harness.seed_admin("admin", "pw1234").await;
    let token = login(&harness, "admin", "pw1234").await;

    // While the session is active, the protected view is served (grant).
    let before = harness.send(get_with_cookie("/admin", &token)).await;
    assert_eq!(before.status, StatusCode::OK);

    // Logout terminates the session and clears the cookie.
    let logout = harness
        .send(post_with_cookie("/admin/logout", &token))
        .await;
    assert_eq!(logout.status, StatusCode::SEE_OTHER);
    assert_eq!(logout.location.as_deref(), Some("/admin/login"));

    // The now-invalid session is treated like an expired one: the next
    // protected request is redirected to login (Req 5.9 expiry path, 5.10).
    let after = harness.send(get_with_cookie("/admin", &token)).await;
    assert_eq!(after.status, StatusCode::SEE_OTHER);
    assert_eq!(after.location.as_deref(), Some("/admin/login"));
}

// ---------------------------------------------------------------------------
// Unauthenticated redirect (Req 5.10)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn unauthenticated_protected_views_redirect_to_login() {
    let harness = Harness::build().await;

    // No session cookie at all: every protected view redirects to login.
    for path in ["/admin", "/admin/keys"] {
        let response = harness.send(get(path)).await;
        assert_eq!(
            response.status,
            StatusCode::SEE_OTHER,
            "{path} without a session should redirect"
        );
        assert_eq!(response.location.as_deref(), Some("/admin/login"));
    }

    // An unknown/garbage session token is also redirected (Req 5.9, 5.10).
    let bogus = harness
        .send(get_with_cookie("/admin", "deadbeefnotarealtoken"))
        .await;
    assert_eq!(bogus.status, StatusCode::SEE_OTHER);
    assert_eq!(bogus.location.as_deref(), Some("/admin/login"));
}

// ---------------------------------------------------------------------------
// API key management: create / view / revoke (Req 5.6)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn create_view_and_revoke_api_keys() {
    let harness = Harness::build().await;
    harness.seed_admin("admin", "pw1234").await;
    let token = login(&harness, "admin", "pw1234").await;

    // Initially the key list is empty.
    let empty = harness.send(get_with_cookie("/admin/keys", &token)).await;
    assert_eq!(empty.status, StatusCode::OK);
    assert!(empty.body.contains("No API keys."));

    // Create a key (Req 5.6). The one-time plaintext key is shown once.
    let created = harness.send(post_with_cookie("/admin/keys", &token)).await;
    assert_eq!(created.status, StatusCode::OK);
    assert!(
        created.body.contains("sk_"),
        "the freshly created plaintext key is shown once"
    );

    // The key now appears in the management view (Req 5.6).
    let listed = harness.send(get_with_cookie("/admin/keys", &token)).await;
    assert_eq!(listed.status, StatusCode::OK);
    assert!(!listed.body.contains("No API keys."));

    // Look up the persisted key id directly to drive the revoke route.
    let id: i64 = sqlx::query_scalar("SELECT id FROM api_keys LIMIT 1")
        .fetch_one(harness.db.pool())
        .await
        .expect("one api key persisted");

    // Revoke it (Req 5.6) -> redirect back to the key list.
    let revoke = harness
        .send(post_with_cookie(
            &format!("/admin/keys/{id}/revoke"),
            &token,
        ))
        .await;
    assert_eq!(revoke.status, StatusCode::SEE_OTHER);
    assert_eq!(revoke.location.as_deref(), Some("/admin/keys"));

    // The key is now persisted as revoked and reflected in the view.
    let revoked_flag: i64 = sqlx::query_scalar("SELECT revoked FROM api_keys WHERE id = ?")
        .bind(id)
        .fetch_one(harness.db.pool())
        .await
        .expect("key still present");
    assert_eq!(revoked_flag, 1, "the key is marked revoked in the database");

    let after = harness.send(get_with_cookie("/admin/keys", &token)).await;
    assert_eq!(after.status, StatusCode::OK);
    assert!(
        after.body.contains("revoked"),
        "the management view shows the key as revoked"
    );
}

#[tokio::test]
async fn key_management_requires_authentication() {
    let harness = Harness::build().await;

    // Creating a key without a session redirects to login (Req 5.6 + 5.10).
    let create = harness.send(post_form("/admin/keys", "")).await;
    assert_eq!(create.status, StatusCode::SEE_OTHER);
    assert_eq!(create.location.as_deref(), Some("/admin/login"));

    // And no key was created.
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM api_keys")
        .fetch_one(harness.db.pool())
        .await
        .expect("count keys");
    assert_eq!(count, 0, "an unauthenticated request creates no key");
}
