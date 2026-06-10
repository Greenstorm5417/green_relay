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
/// Logging module.
pub mod logging;
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
use std::time::Duration;

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
    let api_state = api::ApiState::new(db.clone(), modem.clone())
        .with_rate_config(
            config.default_rate_limit,
            Duration::from_secs(config.rate_window_secs),
        )
        .with_events(events);

    let modem_provider: Arc<dyn admin::ModemStatusProvider> =
        Arc::new(ModemHandleStatusProvider(modem));
    let admin_state = admin::AdminState::new(db, modem_provider);

    let router = api::router(api_state).merge(admin::router(admin_state));

    #[cfg(feature = "web-ui")]
    let router = router.merge(web::router());

    router
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
            let _ = logging::init_subscriber(logging::Severity::Info);
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
    if let Err(error) = logging::init_subscriber(severity) {
        eprintln!("warning: {error}");
    }

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

    let app = build_router(&config, db.clone(), modem_handle.clone(), events);

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

    tokio::select! {
        result = server => {

            result.map_err(RunError::Serve)?;
            drop(modem_handle);

            let _ = modem_task.await;
            tracing::info!("graceful shutdown complete");
            Ok(())
        }
        _ = shutdown_watchdog(notify_rx) => {

            tracing::error!(
                grace_secs = SHUTDOWN_GRACE.as_secs(),
                "graceful shutdown exceeded the grace period; forcing exit"
            );
            modem_task.abort();
            Err(RunError::ShutdownTimeout)
        }
    }
}
