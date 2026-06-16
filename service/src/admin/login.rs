//! The shared login pipeline used by both the HTML form and JSON entry points:
//! lockout enforcement, credential verification, session creation, and audit.

use std::time::Instant;

use chrono::{DateTime, Utc};
use serde::Deserialize;

use crate::db::DbError;

use super::AdminState;
use super::session::{verify_dummy_password, verify_password};

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
///
/// `client_ip` is the lockout key: repeated failures throttle the originating
/// host rather than the named account, so an attacker cannot lock a legitimate
/// admin out of their own account by spraying failed logins at their username.
pub async fn perform_login(
    state: &AdminState,
    username: &str,
    password: &str,
    client_ip: &str,
    now: Instant,
    now_utc: DateTime<Utc>,
) -> Result<LoginResult, DbError> {
    let already_locked = {
        let tracker = state
            .login_tracker
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        tracker.is_locked(client_ip, now)
    };
    if already_locked {
        state
            .db
            .insert_audit(
                "admin_login_locked_out",
                None,
                Some(&format!(
                    "login rejected from locked source for `{username}`"
                )),
                now_utc,
            )
            .await?;
        return Ok(LoginResult::LockedOut);
    }

    let credentials = state.db.find_admin_credentials(username).await?;

    let credentials_ok = match &credentials {
        Some((_, stored_hash)) => verify_password(password, stored_hash),
        None => {
            // Spend the same Argon2 verification time as a real user so a
            // non-existent username cannot be detected by faster responses.
            verify_dummy_password(password);
            false
        }
    };

    if let (true, Some((admin_id, _))) = (credentials_ok, credentials.as_ref()) {
        let admin_id = *admin_id;
        {
            let mut tracker = state
                .login_tracker
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            tracker.record_success(client_ip);
        }
        let token = {
            let mut sessions = state
                .sessions
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            sessions.create(admin_id, now)
        };
        state
            .db
            .insert_audit(
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
        tracker.record_failure(client_ip, now);
        tracker.is_locked(client_ip, now)
    };

    state
        .db
        .insert_audit(
            "admin_login_failed",
            None,
            Some(&format!("failed login for `{username}`")),
            now_utc,
        )
        .await?;

    if now_locked {
        state
            .db
            .insert_audit(
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

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use axum::http::{HeaderMap, HeaderValue, header};
    use chrono::Utc;

    use super::*;
    use crate::admin::session::{Authz, SESSION_COOKIE, authorize};
    use crate::admin::testutil::{audit_count, seed_admin, test_state};

    #[tokio::test]
    async fn login_success_establishes_session() {
        let state = test_state().await;
        let id = seed_admin(&state, "admin", "s3cret-pass").await;
        let now = Instant::now();
        let result = perform_login(
            &state,
            "admin",
            "s3cret-pass",
            "203.0.113.7",
            now,
            Utc::now(),
        )
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

        let result = perform_login(
            &state,
            "admin",
            "wrong",
            "203.0.113.7",
            Instant::now(),
            Utc::now(),
        )
        .await
        .unwrap();
        assert_eq!(result, LoginResult::Failed);
        assert_eq!(audit_count(&state, "admin_login_failed").await, 1);
    }

    #[tokio::test]
    async fn unknown_user_is_rejected_and_audited() {
        let state = test_state().await;
        let result = perform_login(
            &state,
            "ghost",
            "whatever",
            "203.0.113.7",
            Instant::now(),
            Utc::now(),
        )
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
                "203.0.113.7",
                base + Duration::from_secs(i * 10),
                Utc::now(),
            )
            .await
            .unwrap();
        }
        assert_eq!(last, LoginResult::LockedOut);
        assert!(audit_count(&state, "admin_login_locked_out").await >= 1);
        let during = base + Duration::from_secs(60);
        let blocked = perform_login(
            &state,
            "admin",
            "correct",
            "203.0.113.7",
            during,
            Utc::now(),
        )
        .await
        .unwrap();
        assert_eq!(blocked, LoginResult::LockedOut);
    }

    #[tokio::test]
    async fn lockout_is_per_source_not_per_account() {
        let state = test_state().await;
        seed_admin(&state, "admin", "correct").await;
        let base = Instant::now();

        // An attacker hammers the admin username from one host until it locks.
        for i in 0..5 {
            perform_login(
                &state,
                "admin",
                "wrong",
                "198.51.100.9",
                base + Duration::from_secs(i * 10),
                Utc::now(),
            )
            .await
            .unwrap();
        }
        let attacker = perform_login(
            &state,
            "admin",
            "wrong",
            "198.51.100.9",
            base + Duration::from_secs(60),
            Utc::now(),
        )
        .await
        .unwrap();
        assert_eq!(attacker, LoginResult::LockedOut);

        // The real admin, on a different host, still authenticates.
        let legit = perform_login(
            &state,
            "admin",
            "correct",
            "203.0.113.50",
            base + Duration::from_secs(61),
            Utc::now(),
        )
        .await
        .unwrap();
        assert!(matches!(legit, LoginResult::Success { .. }));
    }
}
