//! Process-lifecycle integration tests (task 14.2).
//!
//! These are *out-of-process* tests: they spawn the compiled service binary
//! (`env!("CARGO_BIN_EXE_sms_micro_service")`) with a controlled environment,
//! capture its stdout/stderr, and assert on the real process behavior wired up
//! in `src/lib.rs::run` and `src/main.rs`.
//!
//! Coverage:
//! - Startup log content identifies the bound listen address and serial port
//!   and is machine-parseable JSON on stdout (Req 11.4, 11.6).
//! - Missing/invalid required configuration causes a non-zero exit and names
//!   the offending key (Req 11.5).
//! - `SIGTERM` drives a graceful shutdown with a zero exit code within the
//!   grace period, and a stuck shutdown is forced to a non-zero exit once the
//!   grace period elapses (Req 11.2, 11.3). These signal-driven tests are
//!   gated behind `#[cfg(unix)]` since `SIGTERM` is a Unix concept; the binary
//!   still compiles and the non-signal tests still run on Windows.

use std::io::{BufRead, BufReader};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// Absolute path to the freshly built service binary, provided by Cargo.
const BIN: &str = env!("CARGO_BIN_EXE_sms_micro_service");

/// A serial port path that does not exist on any test host. The Modem Manager
/// tolerates an open failure and retries with backoff, so startup still
/// proceeds; we assert this exact value appears in the startup log.
const NONEXISTENT_SERIAL: &str = "/tmp/sms-microservice-nonexistent-serial-port";

/// The exact message emitted by the startup log record (see `lib.rs::run`).
const STARTUP_MESSAGE: &str = "sms microservice started";

/// Monotonic counter ensuring unique temp filenames within a single test run.
static UNIQUE: AtomicU64 = AtomicU64::new(0);

/// Build a unique temp path with the given prefix and suffix.
fn temp_path(prefix: &str, suffix: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let n = UNIQUE.fetch_add(1, Ordering::Relaxed);
    let mut p = std::env::temp_dir();
    p.push(format!(
        "{prefix}_{}_{nanos}_{n}{suffix}",
        std::process::id()
    ));
    p
}

/// Create an empty config file so the file-sourced configuration is empty and
/// every value is supplied (or omitted) via the environment deterministically.
fn empty_config_file() -> PathBuf {
    let path = temp_path("sms_cfg", ".conf");
    std::fs::write(&path, "").expect("write empty config file");
    path
}

/// Reserve a free TCP port on the loopback interface, then release it so the
/// spawned service can bind it. There is a small race window, but binding to
/// port 0 and reading the assigned port avoids fixed-port collisions.
fn free_port() -> u16 {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind ephemeral port");
    let port = listener.local_addr().expect("local addr").port();
    drop(listener);
    port
}

/// Best-effort cleanup of a temp file.
fn cleanup(path: &PathBuf) {
    let _ = std::fs::remove_file(path);
}

/// Spawn the service binary configured to serve on `addr`, persist to `db`,
/// and target a nonexistent serial port. `reopen_max_attempts` bounds how long
/// the Modem Manager stays alive retrying the (failing) serial open.
///
/// stdout is piped (so we can watch the startup log); stderr is discarded to
/// avoid any pipe-buffer back-pressure while the service runs.
fn spawn_service(addr: &str, db: &PathBuf, cfg: &PathBuf, reopen_max_attempts: u32) -> Child {
    Command::new(BIN)
        .env("SMS_CONFIG_FILE", cfg)
        .env("LISTEN_ADDR", addr)
        .env("DATABASE_PATH", db)
        .env("SERIAL_PORT", NONEXISTENT_SERIAL)
        .env("LOG_LEVEL", "INFO")
        .env("REOPEN_MAX_ATTEMPTS", reopen_max_attempts.to_string())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn service binary")
}

/// Take the child's stdout and stream its lines over a channel from a reader
/// thread. The thread ends when the pipe closes (child exits or is killed).
fn line_reader(child: &mut Child) -> Receiver<String> {
    let stdout = child.stdout.take().expect("child stdout piped");
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let reader = BufReader::new(stdout);
        for line in reader.lines() {
            match line {
                Ok(l) => {
                    if tx.send(l).is_err() {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
    });
    rx
}

/// Whether a parsed log record is the startup record (`fields.message`).
fn is_startup_record(v: &serde_json::Value) -> bool {
    v.get("fields")
        .and_then(|f| f.get("message"))
        .and_then(|m| m.as_str())
        == Some(STARTUP_MESSAGE)
}

/// Wait up to `timeout` for the startup JSON record to appear on stdout,
/// returning the raw line. Returns `None` if the deadline passes first.
fn wait_for_startup(rx: &Receiver<String>, timeout: Duration) -> Option<String> {
    let deadline = Instant::now() + timeout;
    loop {
        let remaining = deadline.checked_duration_since(Instant::now())?;
        match rx.recv_timeout(remaining) {
            Ok(line) => {
                if let Ok(v) = serde_json::from_str::<serde_json::Value>(&line)
                    && is_startup_record(&v)
                {
                    return Some(line);
                }
            }
            Err(RecvTimeoutError::Timeout) | Err(RecvTimeoutError::Disconnected) => return None,
        }
    }
}

/// Poll for process exit up to `timeout`, returning the status once it exits.
/// Only the Unix-gated shutdown tests await an orderly exit; on other targets
/// this helper is unused.
#[cfg(unix)]
fn wait_with_timeout(child: &mut Child, timeout: Duration) -> Option<std::process::ExitStatus> {
    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return Some(status),
            Ok(None) => {
                if Instant::now() >= deadline {
                    return None;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(_) => return None,
        }
    }
}

/// Force-terminate a still-running child and reap it (test teardown).
fn kill(child: &mut Child) {
    let _ = child.kill();
    let _ = child.wait();
}

// ---------------------------------------------------------------------------
// Startup log content + stdout smoke check (Req 11.4, 11.6)
// ---------------------------------------------------------------------------

#[test]
fn startup_log_identifies_listen_address_and_serial_port_on_stdout() {
    let port = free_port();
    let addr = format!("127.0.0.1:{port}");
    let db = temp_path("sms_db", ".sqlite");
    let cfg = empty_config_file();

    let mut child = spawn_service(&addr, &db, &cfg, 1);
    let rx = line_reader(&mut child);

    let line = wait_for_startup(&rx, Duration::from_secs(20));

    // Tear down before asserting so a failure never leaks the process.
    kill(&mut child);
    cleanup(&db);
    cleanup(&cfg);

    let line = line.expect("startup log record was not observed on stdout within 20s");

    // The record reaches stdout and is machine-parseable JSON (Req 11.6).
    let v: serde_json::Value =
        serde_json::from_str(&line).expect("startup record is valid JSON on stdout");

    // It identifies the bound listen address and the serial port in use
    // (Req 11.4).
    let fields = v.get("fields").expect("record has a fields object");
    assert_eq!(
        fields.get("listen_addr").and_then(|x| x.as_str()),
        Some(addr.as_str()),
        "startup record must name the bound listen address; record: {line}"
    );
    assert_eq!(
        fields.get("serial_port").and_then(|x| x.as_str()),
        Some(NONEXISTENT_SERIAL),
        "startup record must name the serial port in use; record: {line}"
    );

    // Structured severity is present and well-formed.
    assert_eq!(
        v.get("level").and_then(|x| x.as_str()),
        Some("INFO"),
        "startup record should be INFO level; record: {line}"
    );
}

// ---------------------------------------------------------------------------
// Missing / invalid configuration exit behavior (Req 11.5)
// ---------------------------------------------------------------------------

#[test]
fn missing_required_config_exits_nonzero_and_names_key() {
    let cfg = empty_config_file();

    // No LISTEN_ADDR / DATABASE_PATH provided and an empty config file: the
    // first required key (LISTEN_ADDR) is missing.
    let output = Command::new(BIN)
        .env("SMS_CONFIG_FILE", &cfg)
        .env_remove("LISTEN_ADDR")
        .env_remove("DATABASE_PATH")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("run service binary");

    cleanup(&cfg);

    assert!(
        !output.status.success(),
        "missing config must cause a non-zero exit; status: {:?}",
        output.status
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stdout.contains("LISTEN_ADDR") || stderr.contains("LISTEN_ADDR"),
        "the offending key must be named; stdout=<{stdout}> stderr=<{stderr}>"
    );
}

#[test]
fn invalid_config_value_exits_nonzero_and_names_key() {
    let cfg = empty_config_file();
    let db = temp_path("sms_db", ".sqlite");

    // LISTEN_ADDR present but unparseable: validation fails naming the key.
    let output = Command::new(BIN)
        .env("SMS_CONFIG_FILE", &cfg)
        .env("LISTEN_ADDR", "not-a-socket-address")
        .env("DATABASE_PATH", &db)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("run service binary");

    cleanup(&cfg);
    cleanup(&db);

    assert!(
        !output.status.success(),
        "invalid config must cause a non-zero exit; status: {:?}",
        output.status
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stdout.contains("LISTEN_ADDR") || stderr.contains("LISTEN_ADDR"),
        "the offending key must be named; stdout=<{stdout}> stderr=<{stderr}>"
    );
}

// ---------------------------------------------------------------------------
// SIGTERM graceful / forced shutdown (Req 11.2, 11.3) — Unix only.
// ---------------------------------------------------------------------------

#[cfg(unix)]
fn send_sigterm(child: &Child) {
    let pid = child.id() as libc::pid_t;
    // Safety: `kill` with a valid pid and signal number has no memory effects.
    unsafe {
        libc::kill(pid, libc::SIGTERM);
    }
}

#[cfg(unix)]
#[test]
fn sigterm_triggers_graceful_shutdown_with_zero_exit() {
    let port = free_port();
    let addr = format!("127.0.0.1:{port}");
    let db = temp_path("sms_db", ".sqlite");
    let cfg = empty_config_file();

    // reopen_max_attempts = 1 keeps the Modem Manager short-lived, so the
    // command channel closing on shutdown is followed by a prompt exit well
    // inside the 30s grace period.
    let mut child = spawn_service(&addr, &db, &cfg, 1);
    let rx = line_reader(&mut child);

    let started = wait_for_startup(&rx, Duration::from_secs(20));
    if started.is_none() {
        kill(&mut child);
        cleanup(&db);
        cleanup(&cfg);
        panic!("service did not log startup within 20s; cannot test shutdown");
    }

    send_sigterm(&child);

    // Graceful shutdown should complete far inside the 30s budget.
    let status = wait_with_timeout(&mut child, Duration::from_secs(30));

    // Teardown regardless of outcome.
    if status.is_none() {
        kill(&mut child);
    }
    cleanup(&db);
    cleanup(&cfg);

    let status = status.expect("service did not exit within 30s after SIGTERM");
    assert!(
        status.success(),
        "graceful shutdown must exit 0; got {status:?}"
    );
}

#[cfg(unix)]
#[test]
fn sigterm_forces_nonzero_exit_when_shutdown_exceeds_grace_period() {
    let port = free_port();
    let addr = format!("127.0.0.1:{port}");
    let db = temp_path("sms_db", ".sqlite");
    let cfg = empty_config_file();

    // A high reopen budget keeps the Modem Manager busy in reconnect backoff
    // (it never observes the closed command channel), so in-flight work cannot
    // complete inside the 30s grace period and the process is forced to a
    // non-zero exit (Req 11.3).
    let mut child = spawn_service(&addr, &db, &cfg, 10);
    let rx = line_reader(&mut child);

    let started = wait_for_startup(&rx, Duration::from_secs(20));
    if started.is_none() {
        kill(&mut child);
        cleanup(&db);
        cleanup(&cfg);
        panic!("service did not log startup within 20s; cannot test shutdown");
    }

    send_sigterm(&child);

    // Allow the full 30s grace period plus margin for the forced exit.
    let status = wait_with_timeout(&mut child, Duration::from_secs(45));

    if status.is_none() {
        kill(&mut child);
    }
    cleanup(&db);
    cleanup(&cfg);

    let status = status.expect("service did not exit within 45s after SIGTERM");
    assert!(
        !status.success(),
        "a shutdown that exceeds the grace period must exit non-zero; got {status:?}"
    );
}
