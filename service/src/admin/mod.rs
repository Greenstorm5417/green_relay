//! Admin area: the built-in HTML portal and the JSON API behind the exported
//! web-ui front-end.
//!
//! The implementation is split by concern:
//! - [`session`] — password hashing, session lifetime/storage, login-failure
//!   lockout, authorization, and cookie/token helpers (the pure security core).
//! - [`login`] — the shared login pipeline used by both front-ends.
//! - [`keys`] — API-key create/list/revoke domain logic.
//! - [`dashboard`] — health/signal/recent-activity aggregation.
//! - [`html`] — the server-rendered (no-JS) handlers and templates.
//! - [`json`] — the JSON API consumed by the web-ui.
//!
//! This module holds the shared [`AdminState`], the [`ModemStatusProvider`]
//! port, the [`router`], and re-exports the submodules' public items so callers
//! continue to use `crate::admin::*`.

mod dashboard;
mod html;
mod json;
mod keys;
mod login;
mod session;

use std::sync::{Arc, Mutex};

use axum::{
    Router,
    routing::{get, post},
};

use crate::db::Db;
use crate::health::ModemStatusSnapshot;

pub use dashboard::{
    ActivityEntry, DashboardData, RECENT_ACTIVITY_LIMIT, dashboard_data, recent_activity,
};
pub use keys::{ApiKeyView, create_api_key, list_api_keys, revoke_api_key};
pub use login::{LoginForm, LoginResult, perform_login};
pub use session::{
    ADMIN_FAILURE_WINDOW, ADMIN_LOCK_DURATION, ADMIN_MAX_FAILURES, AdminLoginTracker, Authz,
    SESSION_COOKIE, SESSION_IDLE_TIMEOUT, Session, SessionStore, admin_locked, authorize,
    hash_password, session_valid, verify_password,
};

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
        .route("/admin/login", get(html::login_form).post(html::login_submit))
        .route("/admin/logout", post(html::logout))
        .route("/admin", get(html::dashboard))
        .route("/admin/keys", get(html::keys_view).post(html::keys_create))
        .route("/admin/keys/{id}/revoke", post(html::keys_revoke))
        .route("/api/admin/session", get(json::api_session))
        .route("/api/admin/login", post(json::api_login))
        .route("/api/admin/logout", post(json::api_logout))
        .route("/api/admin/dashboard", get(json::api_dashboard))
        .route(
            "/api/admin/keys",
            get(json::api_keys_list).post(json::api_keys_create),
        )
        .route("/api/admin/keys/{id}/revoke", post(json::api_keys_revoke))
        .with_state(state)
}

#[cfg(test)]
pub(crate) mod testutil {
    use std::sync::Arc;

    use chrono::Utc;

    use crate::db::Db;
    use crate::health::{ModemStatusSnapshot, SimStatus};

    use super::{AdminState, ModemStatusProvider, hash_password};

    pub(crate) struct StubModem(pub ModemStatusSnapshot);

    impl ModemStatusProvider for StubModem {
        fn current(&self) -> ModemStatusSnapshot {
            self.0.clone()
        }
    }

    pub(crate) fn healthy_snapshot() -> ModemStatusSnapshot {
        ModemStatusSnapshot {
            serial_connected: true,
            sim_status: SimStatus::Ready,
            registered: true,
            responsive: true,
            signal_percent: Some(75),
            operator: Some("Carrier".to_string()),
        }
    }

    pub(crate) async fn test_state() -> AdminState {
        let db = Db::connect_in_memory().await.unwrap();
        db.run_migrations().await.unwrap();
        AdminState::new(db, Arc::new(StubModem(healthy_snapshot())))
    }

    pub(crate) async fn seed_admin(state: &AdminState, username: &str, password: &str) -> i64 {
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

    pub(crate) async fn audit_count(state: &AdminState, event_type: &str) -> i64 {
        sqlx::query_scalar("SELECT COUNT(*) FROM audit_log WHERE event_type = ?")
            .bind(event_type)
            .fetch_one(state.db.pool())
            .await
            .unwrap()
    }
}
