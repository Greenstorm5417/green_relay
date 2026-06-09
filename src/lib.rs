//! SMS Microservice library crate.
//!
//! This crate is organized into layers (see `design.md`):
//! - Domain layer (pure logic): `sms`, `ratelimit`, `health`
//! - Infrastructure layer: `modem`, `db`, `logging`
//! - REST/Admin layer: `api`, `auth`, `admin`
//! - Cross-cutting: `config`, `error` (central `ServiceError`), `models`
//!
//! The [`run`] entry point wires these layers into the full process lifecycle:
//! load and validate configuration, initialize structured logging to stdout,
//! run database migrations before serving, spawn the single-owner Modem
//! Manager, serve the merged REST + admin router, and shut down gracefully on
//! `SIGTERM`/Ctrl-C within a bounded grace period (Req 11.2–11.6).

// Panic-hardening: the shipped library + binary must not panic at runtime, so
// the common panic sources are denied here. These attributes live on the crate
// (not in `Cargo.toml`'s `[lints]`) so they apply only to this crate and the
// binary, leaving integration tests and benches free to panic. Panicking
// inside this crate's own `#[cfg(test)]`/`#[test]` code stays allowed via
// `clippy.toml`. `unsafe` is forbidden in `Cargo.toml`.
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
// `indexing_slicing`/`arithmetic_side_effects` have no `clippy.toml` in-test
// allowance, so relax those two inside `#[cfg(test)]` builds only; the normal
// (non-test) lib compilation still denies them in production code.
#![cfg_attr(test, allow(clippy::indexing_slicing, clippy::arithmetic_side_effects))]

pub mod admin;
pub mod api;
pub mod auth;
pub mod config;
pub mod db;
pub mod error;
pub mod health;
pub mod logging;
pub mod models;
pub mod modem;
pub mod ratelimit;
pub mod sms;

use std::sync::Arc;
use std::time::Duration;

use crate::health::ModemStatusSnapshot;
use crate::modem::ModemHandle;

/// Buffer size for the Modem Manager command channel. Sized so transient
/// bursts of concurrent requests queue without blocking handlers; the manager
/// still processes them one at a time (Req 8.3).
const MODEM_CHANNEL_BUFFER: usize = 32;

/// Total grace period for shutdown after a termination signal is received
/// (Req 11.2). If in-flight work does not complete within this budget the
/// process aborts and exits non-zero (Req 11.3).
const SHUTDOWN_GRACE: Duration = Duration::from_secs(30);

/// A fatal error encountered while starting or running the service.
///
/// Each variant maps to a non-zero process exit. [`RunError::Config`] carries
/// the offending configuration key so startup can name it (Req 11.5).
#[derive(Debug)]
pub enum RunError {
    /// Configuration was missing or invalid (Req 11.5).
    Config(config::ConfigError),
    /// A database connection or migration error occurred before serving
    /// (Req 6.7, 11.3).
    Db(db::DbError),
    /// The listen address could not be bound.
    Bind(std::io::Error),
    /// The HTTP server failed while serving.
    Serve(std::io::Error),
    /// Graceful shutdown did not complete within the grace period; pending
    /// work was aborted (Req 11.3).
    ShutdownTimeout,
}

impl RunError {
    /// The offending configuration key, when this is a configuration error
    /// (Req 11.5).
    pub fn config_key(&self) -> Option<&str> {
        match self {
            RunError::Config(e) => e.key(),
            _ => None,
        }
    }
}

impl std::fmt::Display for RunError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RunError::Config(e) => write!(f, "configuration error: {e}"),
            RunError::Db(e) => write!(f, "database error during startup: {e}"),
            RunError::Bind(e) => write!(f, "failed to bind listen address: {e}"),
            RunError::Serve(e) => write!(f, "http server error: {e}"),
            RunError::ShutdownTimeout => write!(
                f,
                "graceful shutdown exceeded the {}s grace period; aborting",
                SHUTDOWN_GRACE.as_secs()
            ),
        }
    }
}

impl std::error::Error for RunError {}

/// Adapter exposing a [`ModemHandle`]'s status snapshot to the admin dashboard
/// through the [`admin::ModemStatusProvider`] trait.
///
/// The admin layer is decoupled from the `modem` module behind this trait; the
/// process wiring supplies this adapter so the dashboard reads the latest
/// snapshot from the Modem Manager's shared state.
struct ModemHandleStatusProvider(ModemHandle);

impl admin::ModemStatusProvider for ModemHandleStatusProvider {
    fn current(&self) -> ModemStatusSnapshot {
        self.0.status()
    }
}

/// Build the merged REST API + admin dashboard router for the given resources.
fn build_router(config: &config::Config, db: db::Db, modem: ModemHandle) -> axum::Router {
    let api_state = api::ApiState::new(db.clone(), modem.clone()).with_rate_config(
        config.default_rate_limit,
        Duration::from_secs(config.rate_window_secs),
    );

    let modem_provider: Arc<dyn admin::ModemStatusProvider> =
        Arc::new(ModemHandleStatusProvider(modem));
    let admin_state = admin::AdminState::new(db, modem_provider);

    api::router(api_state).merge(admin::router(admin_state))
}

/// Resolve once a termination signal is received: `SIGTERM` on Unix (the
/// systemd stop signal, Req 11.2) or Ctrl-C on any platform. `SIGTERM` is
/// gated behind `cfg(unix)` since it is unavailable on Windows; Ctrl-C is the
/// cross-platform trigger there.
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
            // If the handler cannot be installed, never fire on this branch.
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

/// Watchdog future that resolves [`SHUTDOWN_GRACE`] after the termination
/// signal fires, enforcing the shutdown budget (Req 11.2, 11.3).
///
/// It waits for the signal notification on `notified`; if the sender is dropped
/// first (the server finished for another reason) it never resolves so the
/// server branch wins the race.
async fn shutdown_watchdog(notified: tokio::sync::oneshot::Receiver<()>) {
    if notified.await.is_ok() {
        tokio::time::sleep(SHUTDOWN_GRACE).await;
    } else {
        std::future::pending::<()>().await;
    }
}

/// Library entry point invoked by the binary.
///
/// Loads and validates configuration (Req 11.1, 11.5), initializes structured
/// logging to stdout (Req 11.6), runs database migrations before serving
/// (Req 6.2, 6.3, 11.3), spawns the Modem Manager (Req 8.1), serves the merged
/// router, emits a startup log naming the listen address and serial port
/// (Req 11.4), and handles graceful shutdown on `SIGTERM`/Ctrl-C within the
/// 30-second grace period — forcing a non-zero exit on timeout (Req 11.2,
/// 11.3).
pub async fn run() -> Result<(), RunError> {
    // 1. Load and validate configuration first (Req 11.1, 11.5). Logging is
    //    not yet initialized at its configured level, so a failure here is
    //    surfaced via a best-effort stdout log record naming the offending key.
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

    // 2. Initialize logging at the configured minimum severity (Req 7.4, 11.6).
    let severity =
        logging::Severity::parse(config.log_level.as_str()).unwrap_or(logging::Severity::Info);
    if let Err(error) = logging::init_subscriber(severity) {
        // A subscriber may already be installed (e.g. across test runs in the
        // same process); continue rather than abort.
        eprintln!("warning: {error}");
    }

    // 3. Connect to the database and run migrations before serving (Req 11.3).
    //    A migration failure keeps the service from accepting requests
    //    (Req 6.7) and exits non-zero.
    let db = db::Db::connect(&config.database_path)
        .await
        .map_err(RunError::Db)?;
    db.run_migrations().await.map_err(RunError::Db)?;

    // 4. Spawn the single-owner Modem Manager (Req 8.1). It owns the serial
    //    port and returns when its command channel closes during shutdown.
    let (modem_handle, modem_endpoint) = modem::new_modem(MODEM_CHANNEL_BUFFER);
    let modem_task = tokio::spawn(modem::run_modem_manager(
        config.clone(),
        db.clone(),
        modem_endpoint,
    ));

    // 5. Assemble the merged REST + admin router.
    let app = build_router(&config, db.clone(), modem_handle.clone());

    // 6. Bind the listener before announcing startup so the log reflects a
    //    truly bound address (Req 11.4).
    let listener = tokio::net::TcpListener::bind(config.listen_addr)
        .await
        .map_err(RunError::Bind)?;

    // 7. Startup log identifying the bound listen address and serial port
    //    (Req 11.4). Written to stdout by the subscriber (Req 11.6).
    tracing::info!(
        listen_addr = %config.listen_addr,
        serial_port = %config.serial_port,
        "sms microservice started"
    );

    // 8. Serve with graceful shutdown on SIGTERM/Ctrl-C, bounded by the grace
    //    period (Req 11.2, 11.3). The signal future notifies the watchdog when
    //    shutdown begins so the watchdog can enforce the 30s budget.
    let (notify_tx, notify_rx) = tokio::sync::oneshot::channel();
    let graceful = async move {
        shutdown_signal().await;
        tracing::info!("termination signal received; beginning graceful shutdown");
        let _ = notify_tx.send(());
    };

    let server = axum::serve(listener, app).with_graceful_shutdown(graceful);

    tokio::select! {
        result = server => {
            // The HTTP server has drained existing connections. `app` (and its
            // ModemHandle clones) are dropped by `select!` before this block
            // runs; drop our remaining handle so the Modem Manager's command
            // channel closes and the manager finishes its in-flight exchange
            // and exits (Req 11.2).
            result.map_err(RunError::Serve)?;
            drop(modem_handle);
            // Wait for the manager to close the serial port. The watchdog
            // branch enforces the overall 30s budget, so no inner timeout is
            // needed here.
            let _ = modem_task.await;
            tracing::info!("graceful shutdown complete");
            Ok(())
        }
        _ = shutdown_watchdog(notify_rx) => {
            // The grace period elapsed before shutdown completed: abort pending
            // work (the Modem Manager task is aborted as the runtime unwinds)
            // and exit non-zero (Req 11.3).
            tracing::error!(
                grace_secs = SHUTDOWN_GRACE.as_secs(),
                "graceful shutdown exceeded the grace period; forcing exit"
            );
            modem_task.abort();
            Err(RunError::ShutdownTimeout)
        }
    }
}
