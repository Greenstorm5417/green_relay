//! Fault-injection tests for the modem session layer.
//!
//! Real serial I/O and flaky modem behaviour can only be partially exercised in
//! CI, but the session code paths that *react* to those faults (transient send
//! errors, mid-exchange disconnects, and noisy/unsolicited line interleaving)
//! can be driven deterministically through a scripted [`SerialTransport`].
//! These complement `modem_integration_75.rs`, which covers the
//! manager/reconnect behaviour, by pushing the in-session send/parse paths
//! against adverse byte streams without panicking.

use std::collections::VecDeque;
use std::io;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use green_relay::config::{Config, LogLevel};
use green_relay::db::Db;
use green_relay::health::{ModemStatusSnapshot, SimStatus};
use green_relay::logging::LogRotation;
use green_relay::models::MessageStatus;
use green_relay::modem::{SerialTransport, handle_send};

// ---------------------------------------------------------------------------
// Scripted fault transport
// ---------------------------------------------------------------------------

/// One scripted response to a `read_line` call.
#[derive(Debug, Clone)]
enum Step {
    /// Deliver a line to the collector.
    Line(&'static str),
    /// Behave as a read timeout (no data within the window).
    Timeout,
    /// Simulate the serial port dropping mid-exchange.
    Disconnect,
}

/// A [`SerialTransport`] that replays a fixed script of read outcomes,
/// modelling an adverse or flaky modem. Writes always succeed.
struct FaultTransport {
    steps: VecDeque<Step>,
}

impl FaultTransport {
    fn new(steps: Vec<Step>) -> Self {
        FaultTransport {
            steps: steps.into(),
        }
    }
}

impl SerialTransport for FaultTransport {
    async fn write_bytes(&mut self, _data: &[u8]) -> io::Result<()> {
        Ok(())
    }

    async fn read_line(&mut self, _timeout: Duration) -> io::Result<Option<String>> {
        match self.steps.pop_front() {
            Some(Step::Line(line)) => Ok(Some(line.to_string())),
            Some(Step::Timeout) | None => Ok(None),
            Some(Step::Disconnect) => Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "serial port closed",
            )),
        }
    }
}

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

/// A unique temporary on-disk database (in-memory pools can't be shared across
/// the connection pool, so the integration tests use private temp files).
struct TempDbFile {
    path: std::path::PathBuf,
}

impl TempDbFile {
    fn new() -> TempDbFile {
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let seq = SEQ.fetch_add(1, Ordering::Relaxed);
        let name = format!("sms_fault_{}_{}.sqlite", std::process::id(), seq);
        TempDbFile {
            path: std::env::temp_dir().join(name),
        }
    }

    fn path_str(&self) -> &str {
        self.path.to_str().expect("temp path is valid UTF-8")
    }
}

impl Drop for TempDbFile {
    fn drop(&mut self) {
        for suffix in ["", "-wal", "-shm"] {
            let mut p = self.path.clone().into_os_string();
            p.push(suffix);
            let _ = std::fs::remove_file(std::path::PathBuf::from(p));
        }
    }
}

async fn ready_db(file: &TempDbFile) -> Db {
    Db::initialize(file.path_str())
        .await
        .expect("initialize temp database")
}

/// A config with fast (zero-delay) retries and a small attempt budget.
fn test_config() -> Config {
    Config {
        listen_addr: "127.0.0.1:0".parse::<SocketAddr>().unwrap(),
        serial_port: "mock".to_string(),
        baud_rate: 115_200,
        database_path: ":memory:".to_string(),
        service_center_number: None,
        at_timeout_secs: 1,
        default_rate_limit: 100,
        rate_window_secs: 60,
        log_level: LogLevel::Error,
        log_dir: None,
        log_file_prefix: "green_relay".to_string(),
        log_rotation: LogRotation::Daily,
        log_max_files: 7,
        reopen_max_attempts: 10,
        send_max_attempts: 2,
        send_retry_delay_secs: 0,
        admin_cookie_secure: false,
    }
}

/// A modem snapshot that passes the send deliverability gate.
fn ready_status() -> Arc<Mutex<ModemStatusSnapshot>> {
    Arc::new(Mutex::new(ModemStatusSnapshot {
        serial_connected: true,
        sim_status: SimStatus::Ready,
        registered: true,
        responsive: true,
        signal_percent: Some(80),
        operator: Some("Test Carrier".to_string()),
    }))
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn send_retries_after_transient_error_then_succeeds() {
    let cfg = test_config();
    let file = TempDbFile::new();
    let db = ready_db(&file).await;
    let status = ready_status();

    // CMGF ok; first send attempt hits a transient +CMS ERROR; retry succeeds.
    let mut transport = FaultTransport::new(vec![
        Step::Line("OK"),
        Step::Line("+CMS ERROR: 500"),
        Step::Line("+CMGS: 7"),
        Step::Line("OK"),
    ]);

    let result = handle_send(&mut transport, &cfg, &db, &status, "+14155552671", "hello")
        .await
        .expect("send completes without an I/O error");

    assert_eq!(result.status, MessageStatus::Sent);
    assert_eq!(result.reference, Some("7".to_string()));
}

#[tokio::test]
async fn disconnect_mid_send_surfaces_io_error_without_panic() {
    let cfg = test_config();
    let file = TempDbFile::new();
    let db = ready_db(&file).await;
    let status = ready_status();

    // CMGF ok, then the port drops while awaiting the CMGS result.
    let mut transport = FaultTransport::new(vec![Step::Line("OK"), Step::Disconnect]);

    let result = handle_send(&mut transport, &cfg, &db, &status, "+14155552671", "hi").await;

    assert!(
        result.is_err(),
        "a mid-send disconnect must surface as an I/O error so the manager reconnects"
    );
}

#[tokio::test]
async fn noisy_unsolicited_lines_before_terminators_are_tolerated() {
    let cfg = test_config();
    let file = TempDbFile::new();
    let db = ready_db(&file).await;
    let status = ready_status();

    // Unsolicited/noise lines interleaved with the real responses must not
    // derail parsing: the send still resolves to Sent with the right reference.
    let mut transport = FaultTransport::new(vec![
        Step::Line("RING"),
        Step::Line("OK"),
        Step::Line("^BOOT: garbage"),
        Step::Line("+CMGS: 9"),
        Step::Line("OK"),
    ]);

    let result = handle_send(&mut transport, &cfg, &db, &status, "+14155552671", "hello")
        .await
        .expect("send completes without an I/O error");

    assert_eq!(result.status, MessageStatus::Sent);
    assert_eq!(result.reference, Some("9".to_string()));
}

#[tokio::test]
async fn send_exhausts_attempts_on_repeated_transient_errors() {
    let cfg = test_config();
    let file = TempDbFile::new();
    let db = ready_db(&file).await;
    let status = ready_status();

    // CMGF ok, then every attempt (send_max_attempts = 2) returns an error.
    let mut transport = FaultTransport::new(vec![
        Step::Line("OK"),
        Step::Line("+CMS ERROR: 500"),
        Step::Line("+CMS ERROR: 500"),
    ]);

    let result = handle_send(&mut transport, &cfg, &db, &status, "+14155552671", "hello")
        .await
        .expect("send resolves to a terminal result without an I/O error");

    assert_eq!(result.status, MessageStatus::Failed);
    assert_eq!(result.error_code, Some(500));
}

#[tokio::test]
async fn send_times_out_and_fails_without_retransmit() {
    let cfg = test_config();
    let file = TempDbFile::new();
    let db = ready_db(&file).await;
    let status = ready_status();

    // CMGF ok, then the modem never returns a CMGS result within the window.
    let mut transport = FaultTransport::new(vec![Step::Line("OK"), Step::Timeout]);

    let result = handle_send(&mut transport, &cfg, &db, &status, "+14155552671", "hello")
        .await
        .expect("send resolves to a terminal result without an I/O error");

    assert_eq!(result.status, MessageStatus::Failed);
    assert_eq!(result.error.as_deref(), Some("timeout"));
}
