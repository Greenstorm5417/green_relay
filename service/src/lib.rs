#![deny(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::unreachable,
    clippy::todo,
    clippy::unimplemented,
    clippy::panic_in_result_fn,
    clippy::unwrap_in_result,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects
)]
#![cfg_attr(test, allow(clippy::indexing_slicing, clippy::arithmetic_side_effects))]

/// Admin module.
pub mod admin;
/// API module.
pub mod api;
/// Auth module.
pub mod auth;
/// Config module.
pub mod config;
/// Database module.
pub mod db;
/// Error module.
pub mod error;
/// Real-time event broadcast module.
pub mod events;
/// Health module.
pub mod health;
/// API-key cache module.
pub mod keycache;
/// Logging module.
pub mod logging;
/// Metrics module.
pub mod metrics;
/// Models module.
pub mod models;
/// Modem module.
pub mod modem;
/// Ratelimit module.
pub mod ratelimit;
/// SMS module.
pub mod sms;
#[cfg(feature = "web-ui")]
/// Web UI module.
pub mod web;

use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;
use std::time::Instant;

use crate::health::ModemStatusSnapshot;
use crate::modem::ModemHandle;

const MODEM_CHANNEL_BUFFER: usize = 32;

const SHUTDOWN_GRACE: Duration = Duration::from_secs(30);

pub use error::RunError;

struct ModemHandleStatusProvider(ModemHandle);

impl admin::ModemStatusProvider for ModemHandleStatusProvider {
    fn current(&self) -> ModemStatusSnapshot {
        self.0.status()
    }
}

fn build_router(
    config: &config::Config,
    db: db::Db,
    modem: ModemHandle,
    events: events::EventBus,
) -> axum::Router {
    // One API-key cache shared by the auth path and admin, so revoking a key
    // invalidates the entry the auth path reads.
    let key_cache = keycache::ApiKeyCache::new();

    let api_state = api::ApiState::new(db.clone(), modem.clone())
        .with_rate_config(
            config.default_rate_limit,
            Duration::from_secs(config.rate_window_secs),
        )
        .with_events(events)
        .with_key_cache(key_cache.clone());

    let modem_provider: Arc<dyn admin::ModemStatusProvider> =
        Arc::new(ModemHandleStatusProvider(modem));
    let admin_state = admin::AdminState::new(db, modem_provider)
        .with_key_cache(key_cache)
        .with_cookie_secure(config.admin_cookie_secure);

    spawn_session_sweeper(admin_state.sessions());

    let router = api::router(api_state).merge(admin::router(admin_state));

    #[cfg(feature = "web-ui")]
    let router = router.merge(web::router());

    router.layer(axum::middleware::from_fn(security_headers))
}

/// Sets baseline security response headers on every route: block MIME sniffing,
/// deny framing (clickjacking), and suppress referrer leakage. A stricter
/// `Content-Security-Policy` is applied to the server-rendered admin pages.
async fn security_headers(
    request: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    use axum::http::{HeaderValue, header};

    let mut response = next.run(request).await;
    let headers = response.headers_mut();
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    headers.insert(header::X_FRAME_OPTIONS, HeaderValue::from_static("DENY"));
    headers.insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("no-referrer"),
    );

    // Strict Content-Security-Policy for the server-rendered admin HTML. Scoped
    // to text/html responses (the API serves JSON) and gated off when the JS
    // web-ui is bundled, since the exported Next app relies on inline scripts.
    #[cfg(not(feature = "web-ui"))]
    {
        let is_html = response
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|ct| ct.starts_with("text/html"));
        if is_html {
            response.headers_mut().insert(
                header::CONTENT_SECURITY_POLICY,
                HeaderValue::from_static(
                    "default-src 'self'; frame-ancestors 'none'; base-uri 'self'; \
                     object-src 'none'; form-action 'self'",
                ),
            );
        }
    }
    response
}

async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };

    #[cfg(unix)]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut signal) => {
                signal.recv().await;
            }

            Err(_) => std::future::pending::<()>().await,
        }
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {}
        _ = terminate => {}
    }
}

async fn shutdown_watchdog(notified: tokio::sync::oneshot::Receiver<()>) {
    if notified.await.is_ok() {
        tokio::time::sleep(SHUTDOWN_GRACE).await;
    } else {
        std::future::pending::<()>().await;
    }
}

const QUEUED_REAP_INTERVAL: Duration = Duration::from_secs(60);

/// How many stuck messages the reaper re-dispatches per cycle. Sized so a large
/// outage backlog drains in minutes rather than hours.
const QUEUED_REAP_BATCH: i64 = 200;

/// How often the background sweep drops expired admin sessions.
const SESSION_SWEEP_INTERVAL: Duration = Duration::from_secs(5 * 60);

/// Periodically evicts expired admin sessions so abandoned sessions that are
/// never accessed again cannot accumulate for the life of the process.
fn spawn_session_sweeper(sessions: Arc<Mutex<admin::SessionStore>>) {
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(SESSION_SWEEP_INTERVAL);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            ticker.tick().await;
            sessions
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .sweep_expired(Instant::now());
        }
    });
}

/// Only retry messages that have been `queued` for at least this long, so the
/// reaper never races the foreground dispatch (which resolves within the send
/// timeout) and never double-sends a delivery still in flight.
const QUEUED_REAP_GRACE_SECS: i64 = 300;

/// Periodically re-dispatches outbound messages stuck in `queued` — e.g. when
/// the modem lost registration between acceptance and the initial dispatch, so
/// the foreground send deferred delivery. Without this, such messages would
/// linger `queued` forever. Runs until aborted at shutdown.
async fn reap_queued_messages(db: db::Db, modem: ModemHandle, events: events::EventBus) {
    let mut ticker = tokio::time::interval(QUEUED_REAP_INTERVAL);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        ticker.tick().await;

        let cutoff = match chrono::Utc::now()
            .checked_sub_signed(chrono::Duration::seconds(QUEUED_REAP_GRACE_SECS))
        {
            Some(time) => time.to_rfc3339(),
            None => continue,
        };

        let stuck = match db
            .queued_outbound_messages(&cutoff, QUEUED_REAP_BATCH)
            .await
        {
            Ok(messages) => messages,
            Err(error) => {
                tracing::error!(error = %error, "reaper: failed to query queued messages");
                continue;
            }
        };

        for message in stuck {
            let result = modem.send_sms(&message.to_number, &message.body).await;
            match result.status {
                crate::models::MessageStatus::Sent => {
                    let reference = result.reference;
                    let _ = db
                        .set_outbound_status(
                            message.id,
                            crate::models::MessageStatus::Sent,
                            reference.as_deref(),
                            None,
                        )
                        .await;
                    // Mirror the foreground dispatch: notify SSE subscribers of
                    // the terminal status transition.
                    events.publish(events::ServiceEvent::MessageStatus(
                        events::MessageStatusEvent {
                            id: message.id,
                            status: crate::models::MessageStatus::Sent,
                            reference,
                        },
                    ));
                    tracing::info!(
                        id = message.id,
                        "reaper: delivered previously queued message"
                    );
                }
                crate::models::MessageStatus::Failed => {
                    let detail = result
                        .error_code
                        .map(|code| code.to_string())
                        .or(result.error);
                    let _ = db
                        .set_outbound_status(
                            message.id,
                            crate::models::MessageStatus::Failed,
                            None,
                            detail.as_deref(),
                        )
                        .await;
                    events.publish(events::ServiceEvent::MessageStatus(
                        events::MessageStatusEvent {
                            id: message.id,
                            status: crate::models::MessageStatus::Failed,
                            reference: None,
                        },
                    ));
                }
                crate::models::MessageStatus::Queued => {
                    // Still not deliverable; leave it for a later cycle.
                }
            }
        }
    }
}

/// Create or reset an administrator account, then exit.
///
/// Bootstraps the first admin (the `admin_users` table starts empty) and can
/// recover access by resetting an existing user's password. Loads the same
/// configuration as [`run`] to locate the database, applies migrations, hashes
/// the password, and upserts the user. Returns an error if the password is
/// empty or any step fails.
pub async fn create_admin(username: &str, password: &str) -> Result<(), RunError> {
    if username.trim().is_empty() {
        return Err(RunError::AdminSetup(
            "username must not be empty".to_string(),
        ));
    }
    if password.is_empty() {
        return Err(RunError::AdminSetup(
            "password must not be empty".to_string(),
        ));
    }

    let config = config::load().map_err(RunError::Config)?;

    let db = db::Db::connect(&config.database_path)
        .await
        .map_err(RunError::Db)?;
    db.run_migrations().await.map_err(RunError::Db)?;

    let hash = admin::hash_password(password);
    if hash.is_empty() {
        return Err(RunError::AdminSetup(
            "failed to hash the password".to_string(),
        ));
    }

    let created = db
        .upsert_admin_user(username, &hash)
        .await
        .map_err(RunError::Db)?;

    if created {
        println!("created admin user `{username}`");
    } else {
        println!("reset password for existing admin user `{username}`");
    }
    Ok(())
}

/// Runs the main application service loop.
pub async fn run() -> Result<(), RunError> {
    let config = match config::load() {
        Ok(config) => config,
        Err(error) => {
            let _ = logging::init_subscriber(logging::Severity::Info, None);
            let key = error.key().unwrap_or("<unknown>");
            tracing::error!(
                config_key = key,
                error = %error,
                "invalid configuration; aborting startup"
            );
            return Err(RunError::Config(error));
        }
    };

    let severity =
        logging::Severity::parse(config.log_level.as_str()).unwrap_or(logging::Severity::Info);
    let file_log = config.log_dir.as_ref().map(|dir| logging::FileLogConfig {
        directory: dir.clone(),
        prefix: config.log_file_prefix.clone(),
        rotation: config.log_rotation,
        max_files: config.log_max_files as usize,
    });
    let _log_guard = match logging::init_subscriber(severity, file_log.as_ref()) {
        Ok(guard) => guard,
        Err(error) => {
            eprintln!("warning: {error}");
            None
        }
    };

    let db = db::Db::connect(&config.database_path)
        .await
        .map_err(RunError::Db)?;
    db.run_migrations().await.map_err(RunError::Db)?;

    let events = events::EventBus::default();

    let (modem_handle, modem_endpoint) = modem::new_modem(MODEM_CHANNEL_BUFFER);
    let modem_task = tokio::spawn(modem::run_modem_manager(
        config.clone(),
        db.clone(),
        modem_endpoint,
        events.clone(),
    ));

    let app = build_router(&config, db.clone(), modem_handle.clone(), events.clone());

    let reaper_task = tokio::spawn(reap_queued_messages(
        db.clone(),
        modem_handle.clone(),
        events,
    ));

    let listener = tokio::net::TcpListener::bind(config.listen_addr)
        .await
        .map_err(RunError::Bind)?;

    tracing::info!(
        listen_addr = %config.listen_addr,
        serial_port = %config.serial_port,
        "sms microservice started"
    );

    let (notify_tx, notify_rx) = tokio::sync::oneshot::channel();
    let graceful = async move {
        shutdown_signal().await;
        tracing::info!("termination signal received; beginning graceful shutdown");
        let _ = notify_tx.send(());
    };

    let server = axum::serve(listener, app).with_graceful_shutdown(graceful);

    let graceful_shutdown = async move {
        server.await.map_err(RunError::Serve)?;
        // Stop the reaper first so it releases its modem handle clone; otherwise
        // the modem manager's command channel never closes and the await hangs.
        reaper_task.abort();
        drop(modem_handle);
        let _ = modem_task.await;
        Ok::<(), RunError>(())
    };

    tokio::select! {
        result = graceful_shutdown => {
            result?;
            tracing::info!("graceful shutdown complete");
            Ok(())
        }
        _ = shutdown_watchdog(notify_rx) => {
            tracing::error!(
                grace_secs = SHUTDOWN_GRACE.as_secs(),
                "graceful shutdown exceeded the grace period; forcing exit"
            );
            Err(RunError::ShutdownTimeout)
        }
    }
}
