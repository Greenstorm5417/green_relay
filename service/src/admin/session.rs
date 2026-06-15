//! The admin security core: password hashing/verification, session lifetime
//! and storage, login-failure lockout tracking, request authorization, and the
//! session cookie/token helpers. This module is pure (no SQL) and is the most
//! heavily unit-tested part of the admin area.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use argon2::{
    Argon2,
    password_hash::{
        PasswordHash, PasswordHasher, PasswordVerifier, SaltString,
        rand_core::{OsRng, RngCore},
    },
};
use axum::http::{HeaderMap, header};

use super::AdminState;

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

pub(crate) fn random_token() -> String {
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

pub(crate) fn session_cookie(token: &str) -> String {
    format!(
        "{SESSION_COOKIE}={token}; HttpOnly; SameSite=Strict; Path=/; Max-Age={}",
        SESSION_IDLE_TIMEOUT.as_secs()
    )
}

pub(crate) fn clear_cookie() -> String {
    format!("{SESSION_COOKIE}=; HttpOnly; SameSite=Strict; Path=/; Max-Age=0")
}

pub(crate) fn session_token_from_headers(headers: &HeaderMap) -> Option<String> {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::admin::login::{LoginResult, perform_login};
    use crate::admin::testutil::{seed_admin, test_state};
    use axum::http::HeaderValue;
    use chrono::Utc;

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
}
