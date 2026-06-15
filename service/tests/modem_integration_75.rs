//! Integration tests for the Modem Manager driven by an in-memory serial
//! transport (task 7.5).
//!
//! These tests exercise the Modem Manager's behaviour without real hardware by
//! implementing the public [`SerialTransport`] trait with a scriptable mock
//! that records every command written to the "port" and serves canned response
//! lines back to the manager. Each test asserts an ordering or error-handling
//! requirement against the recorded commands and the persisted database state.
//!
//! Validates: Requirements 1.2, 1.9, 2.1, 2.3, 2.5, 2.7, 2.8, 2.9, 8.2, 8.5,
//! 8.6, 8.7, 8.8, 10.2, 10.5, 10.6, 10.7

use std::collections::VecDeque;
use std::io;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use green_relay::config::{Config, LogLevel};
use green_relay::db::Db;
use green_relay::health::{ModemStatusSnapshot, SimStatus};
use green_relay::models::MessageStatus;
use green_relay::modem::{
    ModemRequest, SerialTransport, SessionOutcome, handle_inbound, handle_send, initialize,
    parse_cmti_index, run_session,
};

// ---------------------------------------------------------------------------
// Scriptable in-memory serial transport
// ---------------------------------------------------------------------------

/// A single scripted result for a `read_line` call.
enum Read {
    /// A full line is available.
    Line(String),
    /// No line arrived before the timeout (`read_line` resolves `Ok(None)`).
    Timeout,
    /// The port closed/errored (`read_line` resolves `Err`).
    Disconnect,
}

/// An in-memory [`SerialTransport`] that records all written commands and
/// serves a pre-scripted sequence of read results.
struct MockModem {
    writes: Vec<String>,
    reads: VecDeque<Read>,
}

impl MockModem {
    fn new() -> Self {
        MockModem {
            writes: Vec::new(),
            reads: VecDeque::new(),
        }
    }

    /// Queue a single response line returned by one `read_line` call.
    fn push_line(&mut self, line: &str) {
        self.reads.push_back(Read::Line(line.to_string()));
    }

    /// Queue a `read_line` timeout (no line within the window).
    fn push_timeout(&mut self) {
        self.reads.push_back(Read::Timeout);
    }

    /// Queue a port disconnect surfaced as a read error.
    fn push_disconnect(&mut self) {
        self.reads.push_back(Read::Disconnect);
    }

    /// The recorded command writes, with trailing CR/LF trimmed for easy
    /// comparison. The Ctrl-Z (`0x1A`) that terminates an `AT+CMGS` payload is
    /// intentionally left in place.
    fn commands(&self) -> Vec<String> {
        self.writes
            .iter()
            .map(|w| w.trim_end_matches(['\r', '\n']).to_string())
            .collect()
    }
}

impl SerialTransport for MockModem {
    async fn write_bytes(&mut self, data: &[u8]) -> io::Result<()> {
        self.writes.push(String::from_utf8_lossy(data).to_string());
        Ok(())
    }

    async fn read_line(&mut self, _timeout: Duration) -> io::Result<Option<String>> {
        match self.reads.pop_front() {
            Some(Read::Line(line)) => Ok(Some(line)),
            Some(Read::Timeout) => Ok(None),
            Some(Read::Disconnect) => Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "serial port closed",
            )),
            // Nothing scripted: behave as a benign timeout so a test never
            // blocks waiting for a line that will not arrive.
            None => Ok(None),
        }
    }
}

// ---------------------------------------------------------------------------
// Test fixtures
// ---------------------------------------------------------------------------

/// Monotonic counter giving every database a unique on-disk filename.
static DB_SEQ: AtomicU64 = AtomicU64::new(0);

/// A temporary SQLite database file that deletes itself (and its `-wal` /
/// `-shm` sidecars) when dropped. A shared file path (rather than a bare
/// `:memory:` database) ensures a write and its read-back observe the same
/// store across pooled connections.
struct TempDbFile {
    path: PathBuf,
}

impl TempDbFile {
    fn new() -> TempDbFile {
        let seq = DB_SEQ.fetch_add(1, Ordering::Relaxed);
        let name = format!("sms_modem75_{}_{}.sqlite", std::process::id(), seq);
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
            let _ = std::fs::remove_file(PathBuf::from(p));
        }
    }
}

/// Build a fresh, migrated database backed by a private temporary file.
async fn fresh_db(file: &TempDbFile) -> Db {
    Db::initialize(file.path_str())
        .await
        .expect("initialize temporary database")
}

/// A test [`Config`] with fast (zero-delay) retries; `service_center` controls
/// whether an `AT+CSCA` initialization command is expected.
fn test_config(service_center: Option<&str>) -> Config {
    Config {
        listen_addr: "127.0.0.1:8080".parse::<SocketAddr>().unwrap(),
        serial_port: "/dev/null".to_string(),
        baud_rate: 115_200,
        database_path: ":memory:".to_string(),
        service_center_number: service_center.map(|s| s.to_string()),
        at_timeout_secs: 5,
        default_rate_limit: 100,
        rate_window_secs: 60,
        log_level: LogLevel::Info,
        log_dir: None,
        log_file_prefix: "green_relay".to_string(),
        log_rotation: green_relay::logging::LogRotation::Daily,
        log_max_files: 7,
        reopen_max_attempts: 10,
        send_max_attempts: 3,
        send_retry_delay_secs: 0,
    }
}

/// A modem status snapshot that satisfies the send deliverability gate
/// (SIM ready and registered to a network).
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

/// The list of audit-log event types recorded so far, in insertion order.
async fn audit_events(db: &Db) -> Vec<String> {
    let events: Vec<String> = sqlx::query_scalar("SELECT event_type FROM audit_log ORDER BY id")
        .fetch_all(db.pool())
        .await
        .expect("read audit_log");
    events
}

const AT_TIMEOUT: Duration = Duration::from_secs(5);

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// Initialization issues the SMS-setup commands in the required order,
/// including `AT+CSCA` when a service-center number is configured
/// (Req 8.2, 8.6).
#[tokio::test]
async fn initialization_issues_sms_setup_sequence_in_order() {
    let cfg = test_config(Some("+12085551212"));
    let mut modem = MockModem::new();
    // Each of the four init commands is acknowledged with OK.
    for _ in 0..4 {
        modem.push_line("OK");
    }

    let ok = initialize(&cfg, &mut modem, AT_TIMEOUT).await.unwrap();
    assert!(ok, "all init commands succeeded");

    assert_eq!(
        modem.commands(),
        vec![
            "AT+CMGF=1".to_string(),
            "AT+CSCS=\"IRA\"".to_string(),
            "AT+CSMP=17,167,0,0".to_string(),
            "AT+CSCA=\"+12085551212\"".to_string(),
        ]
    );
}

/// An initialization command that returns an error does not abort the
/// sequence; initialization reports failure but keeps the port open for a
/// later retry (Req 8.8).
#[tokio::test]
async fn initialization_error_reports_failure_but_continues() {
    let cfg = test_config(None);
    let mut modem = MockModem::new();
    modem.push_line("OK"); // AT+CMGF=1
    modem.push_line("ERROR"); // AT+CSCS="IRA" fails
    modem.push_line("OK"); // AT+CSMP=17,167,0,0 still issued

    let ok = initialize(&cfg, &mut modem, AT_TIMEOUT).await.unwrap();
    assert!(!ok, "init reports failure when a command errors");
    // All three commands were still issued (the error did not abort the run).
    assert_eq!(
        modem.commands(),
        vec![
            "AT+CMGF=1".to_string(),
            "AT+CSCS=\"IRA\"".to_string(),
            "AT+CSMP=17,167,0,0".to_string(),
        ]
    );
}

/// A send sets SMS text mode with `AT+CMGF=1` before transmitting the message
/// with `AT+CMGS` (Req 1.2).
#[tokio::test]
async fn send_sets_text_mode_before_transmitting() {
    let file = TempDbFile::new();
    let db = fresh_db(&file).await;
    let cfg = test_config(None);
    let status = ready_status();

    let mut modem = MockModem::new();
    modem.push_line("OK"); // AT+CMGF=1
    modem.push_line("+CMGS: 7"); // AT+CMGS intermediate result
    modem.push_line("OK"); // AT+CMGS terminator

    let result = handle_send(&mut modem, &cfg, &db, &status, "+14155552671", "hello")
        .await
        .unwrap();
    assert_eq!(result.status, MessageStatus::Sent);
    assert_eq!(result.reference, Some(7));

    let cmds = modem.commands();
    let cmgf = cmds
        .iter()
        .position(|c| c.contains("AT+CMGF=1"))
        .expect("AT+CMGF=1 issued");
    let cmgs = cmds
        .iter()
        .position(|c| c.contains("AT+CMGS"))
        .expect("AT+CMGS issued");
    assert!(
        cmgf < cmgs,
        "AT+CMGF=1 must precede AT+CMGS, got commands: {cmds:?}"
    );
}

/// A detected new-message URC drives the read/persist/delete pipeline in the
/// correct order: `+CMTI` index -> `AT+CMGR` read -> persist -> `AT+CMGD`
/// delete (Req 2.1, 2.3, 2.5).
#[tokio::test]
async fn inbound_urc_reads_persists_then_deletes_in_order() {
    let file = TempDbFile::new();
    let db = fresh_db(&file).await;

    // URC detection: a +CMTI notification yields the storage index (Req 2.1).
    let index = parse_cmti_index("+CMTI: \"SM\",4").expect("URC index parsed");
    assert_eq!(index, 4);

    let mut modem = MockModem::new();
    // AT+CMGR=4 response: header, body, terminator.
    modem.push_line("+CMGR: \"REC UNREAD\",\"+14155550123\",,\"24/01/02,03:04:05+00\"");
    modem.push_line("Hello from the network");
    modem.push_line("OK");
    // AT+CMGD=4 acknowledged.
    modem.push_line("OK");

    let mut pending = VecDeque::new();
    handle_inbound(&mut modem, &db, index, AT_TIMEOUT, &mut pending)
        .await
        .unwrap();

    let cmds = modem.commands();
    let cmgr = cmds
        .iter()
        .position(|c| c.contains(&format!("AT+CMGR={index}")))
        .expect("AT+CMGR issued");
    let cmgd = cmds
        .iter()
        .position(|c| c.contains(&format!("AT+CMGD={index}")))
        .expect("AT+CMGD issued");
    assert!(
        cmgr < cmgd,
        "AT+CMGR must precede AT+CMGD, got commands: {cmds:?}"
    );

    // The message is persisted (between the read and the delete) (Req 2.3).
    let inbound = db.list_inbound_messages().await.unwrap();
    assert_eq!(inbound.len(), 1);
    assert_eq!(inbound[0].from_number, "+14155550123");
    assert_eq!(inbound[0].body, "Hello from the network");
}

/// A failing `AT+CMGR` read is retried up to 3 times; when every attempt fails
/// the failure is audited and no delete is issued (Req 2.8).
#[tokio::test]
async fn inbound_read_failure_retries_three_times_then_audits() {
    let file = TempDbFile::new();
    let db = fresh_db(&file).await;

    let mut modem = MockModem::new();
    for _ in 0..3 {
        modem.push_line("ERROR"); // each AT+CMGR attempt fails
    }

    let mut pending = VecDeque::new();
    handle_inbound(&mut modem, &db, 9, AT_TIMEOUT, &mut pending)
        .await
        .unwrap();

    let cmgr_attempts = modem
        .commands()
        .iter()
        .filter(|c| c.contains("AT+CMGR=9"))
        .count();
    assert_eq!(cmgr_attempts, 3, "AT+CMGR should be retried up to 3 times");
    assert!(
        !modem.commands().iter().any(|c| c.contains("AT+CMGD")),
        "no delete should be issued when the read fails"
    );
    assert!(db.list_inbound_messages().await.unwrap().is_empty());
    assert!(
        audit_events(&db)
            .await
            .contains(&"inbound_read_failed".to_string()),
        "read failure should be audited"
    );
}

/// A failing `AT+CMGD` delete after a successful persist is audited, and the
/// persisted message is retained (Req 2.7).
#[tokio::test]
async fn inbound_delete_failure_is_audited_and_message_retained() {
    let file = TempDbFile::new();
    let db = fresh_db(&file).await;

    let mut modem = MockModem::new();
    modem.push_line("+CMGR: \"REC UNREAD\",\"+14155550999\",,\"24/01/02,03:04:05+00\"");
    modem.push_line("body text");
    modem.push_line("OK"); // AT+CMGR terminator
    modem.push_line("ERROR"); // AT+CMGD fails

    let mut pending = VecDeque::new();
    handle_inbound(&mut modem, &db, 2, AT_TIMEOUT, &mut pending)
        .await
        .unwrap();

    // The message remains persisted; the delete failure does not undo it.
    assert_eq!(db.list_inbound_messages().await.unwrap().len(), 1);
    assert!(modem.commands().iter().any(|c| c.contains("AT+CMGD=2")));
    assert!(
        audit_events(&db)
            .await
            .contains(&"inbound_delete_failed".to_string()),
        "delete failure should be audited"
    );
}

/// When persisting an inbound message fails, the message is retained in modem
/// storage (no `AT+CMGD` is issued) and the failure is audited (Req 2.9).
#[tokio::test]
async fn inbound_persist_failure_skips_delete_and_audits() {
    let file = TempDbFile::new();
    let db = fresh_db(&file).await;

    // Force inbound persistence to fail while leaving the audit_log table
    // intact, so the persist-failure branch is exercised faithfully.
    sqlx::query("DROP TABLE inbound_messages")
        .execute(db.pool())
        .await
        .expect("drop inbound_messages to induce a persist failure");

    let mut modem = MockModem::new();
    modem.push_line("+CMGR: \"REC UNREAD\",\"+14155550000\",,\"24/01/02,03:04:05+00\"");
    modem.push_line("undeliverable");
    modem.push_line("OK"); // AT+CMGR terminator
    // No AT+CMGD response is scripted: the delete must be skipped entirely.

    let mut pending = VecDeque::new();
    handle_inbound(&mut modem, &db, 5, AT_TIMEOUT, &mut pending)
        .await
        .unwrap();

    assert!(
        !modem.commands().iter().any(|c| c.contains("AT+CMGD")),
        "delete must be skipped when persistence fails (Req 2.9)"
    );
    assert!(
        audit_events(&db)
            .await
            .contains(&"inbound_persist_failed".to_string()),
        "persist failure should be audited"
    );
}

/// A send that never returns a result within the window fails and is NOT
/// retransmitted (Req 1.9): exactly one `AT+CMGS` is written despite the
/// configured retry budget.
#[tokio::test]
async fn send_timeout_fails_without_retransmit() {
    let file = TempDbFile::new();
    let db = fresh_db(&file).await;
    let cfg = test_config(None); // send_max_attempts = 3
    let status = ready_status();

    let mut modem = MockModem::new();
    modem.push_line("OK"); // AT+CMGF=1
    modem.push_timeout(); // AT+CMGS: no result within the window

    let result = handle_send(&mut modem, &cfg, &db, &status, "+14155552671", "hello")
        .await
        .unwrap();
    assert_eq!(result.status, MessageStatus::Failed);
    assert_eq!(result.error.as_deref(), Some("timeout"));

    let cmgs_writes = modem
        .commands()
        .iter()
        .filter(|c| c.contains("AT+CMGS"))
        .count();
    assert_eq!(
        cmgs_writes, 1,
        "a timed-out send must not be retransmitted (Req 1.9)"
    );
}

/// A send is deferred (kept `queued`) when the modem is not ready to deliver
/// (SIM not ready / not registered), and no AT command is issued (Req 10.5).
#[tokio::test]
async fn send_is_deferred_when_not_deliverable() {
    let file = TempDbFile::new();
    let db = fresh_db(&file).await;
    let cfg = test_config(None);

    // SIM not ready and not registered -> not deliverable.
    let status = Arc::new(Mutex::new(ModemStatusSnapshot {
        serial_connected: true,
        sim_status: SimStatus::NotReady,
        registered: false,
        responsive: true,
        signal_percent: None,
        operator: None,
    }));

    let mut modem = MockModem::new();
    let result = handle_send(&mut modem, &cfg, &db, &status, "+14155552671", "hello")
        .await
        .unwrap();

    assert_eq!(result.status, MessageStatus::Queued);
    assert!(
        modem.commands().is_empty(),
        "no AT command should be issued when delivery is deferred"
    );
}

/// A losing the port mid-session is reported as a disconnect, and on the
/// subsequent reconnect the SMS initialization commands are re-issued
/// (Req 8.7, 10.2). This composes the disconnect-detection path
/// (`run_session` returning `Disconnected`) with the re-initialization path
/// (`initialize`) that the manager runs on each successful reopen.
#[tokio::test]
async fn disconnect_triggers_reconnect_and_reinitialization() {
    let file = TempDbFile::new();
    let db = fresh_db(&file).await;
    let cfg = test_config(None);
    let status = ready_status();

    let mut modem = MockModem::new();

    // Connection epoch 1: initialization succeeds (CMGF, CSCS, CSMP).
    for _ in 0..3 {
        modem.push_line("OK");
    }
    assert!(initialize(&cfg, &mut modem, AT_TIMEOUT).await.unwrap());

    // The session loses the port on its first status-refresh read.
    modem.push_disconnect();
    let (_tx, mut rx) = tokio::sync::mpsc::channel::<ModemRequest>(4);
    let outcome = run_session(&cfg, &db, &mut rx, &mut modem, &status).await;
    assert_eq!(
        outcome,
        SessionOutcome::Disconnected,
        "a port read error must end the session as Disconnected"
    );

    // Connection epoch 2: on reconnect the manager re-issues initialization.
    let before_reinit = modem.commands().len();
    for _ in 0..3 {
        modem.push_line("OK");
    }
    assert!(initialize(&cfg, &mut modem, AT_TIMEOUT).await.unwrap());

    let reinit = &modem.commands()[before_reinit..];
    assert_eq!(
        reinit,
        &[
            "AT+CMGF=1".to_string(),
            "AT+CSCS=\"IRA\"".to_string(),
            "AT+CSMP=17,167,0,0".to_string(),
        ],
        "initialization commands must be re-issued on reconnect (Req 10.2)"
    );
}
