//! Modem Manager: owns the serial port and serializes AT command exchanges.
//!
//! To be implemented in tasks 6 and 7.
// ---------------------------------------------------------------------------
// Reconnect backoff schedule (Task 7.1, Property 29, Requirement 10.1)
// ---------------------------------------------------------------------------
//
// When the serial port becomes unavailable during operation, the Modem Manager
// reopens it using exponential backoff starting at 1 second and capped at 60
// seconds, for up to a configured maximum number of attempts (default 10).
//
// The delay for attempt `n` (1-indexed) is `min(2^(n-1), 60)` seconds:
//   attempt 1 -> 1s, 2 -> 2s, 3 -> 4s, 4 -> 8s, 5 -> 16s, 6 -> 32s,
//   attempt 7 and beyond -> 60s (since 2^6 = 64 exceeds the 60s cap).
//
// The schedule is monotonically non-decreasing and never exceeds 60 seconds.

/// The maximum reconnect backoff delay in seconds (Requirement 10.1 cap).
pub const RECONNECT_BACKOFF_CAP_SECS: u64 = 60;

/// Pure backoff function: the delay in seconds before reopen attempt `attempt`.
///
/// For a 1-indexed `attempt` number `n`, the delay is `min(2^(n-1), 60)`
/// seconds. The computation is overflow-safe: once the exponent is large
/// enough that `2^(n-1)` would exceed (or overflow past) the 60-second cap,
/// the cap is returned directly. `attempt` values of 0 are treated as the
/// first attempt (exponent 0) so the function never panics.
pub fn reconnect_backoff_secs(attempt: u32) -> u64 {
    // Exponent is (attempt - 1); attempt 0 is treated the same as attempt 1.
    let exponent = attempt.saturating_sub(1);

    // 2^6 = 64 already exceeds the 60s cap, so any exponent >= 6 is capped.
    // Guarding here also prevents shift overflow for large attempt numbers.
    if exponent >= 6 {
        return RECONNECT_BACKOFF_CAP_SECS;
    }

    let delay = 1u64 << exponent; // 2^exponent for exponent in 0..=5
    delay.min(RECONNECT_BACKOFF_CAP_SECS)
}

/// Build the full reconnect backoff schedule bounded by the configured maximum
/// number of attempts.
///
/// Returns one delay per attempt, in order, for attempts `1..=max_attempts`.
/// The resulting vector therefore has exactly `max_attempts` entries (empty
/// when `max_attempts` is 0), is monotonically non-decreasing, and contains no
/// value greater than 60 seconds. This enforces that the number of attempts
/// never exceeds the configured maximum (Requirement 10.1, Property 29).
pub fn reconnect_backoff_schedule(max_attempts: u32) -> Vec<u64> {
    (1..=max_attempts).map(reconnect_backoff_secs).collect()
}
// ---------------------------------------------------------------------------
// AT response parsing and classification (Task 6.1)
// Requirements: 1.4, 1.5, 2.2, 8.4
// ---------------------------------------------------------------------------
//
// The Modem Manager reads response lines from the serial port and must decide,
// for each line, whether it terminates the current AT exchange and, if so,
// with what result. A terminating result code is exactly one of:
//
//   * `OK`                  -> success
//   * `ERROR`               -> generic error
//   * `+CMS ERROR: <code>`  -> SMS-related error with a numeric code
//   * `+CME ERROR: <code>`  -> ME (mobile equipment) error with a numeric code
//
// Any other line (an echoed command, an intermediate `+CMGS:`/`+CMGR:` result,
// a data line, or an empty line) is non-terminating and the manager keeps
// reading until a terminator or the configured timeout fires (Req 8.4).
//
// On top of the classifier this section recovers the structured payloads the
// service needs:
//   * `+CMGS: <ref>` send references, whose outcome maps to status `sent`
//     (Req 1.4).
//   * `+CMS`/`+CME ERROR` codes for a send, whose outcome maps to status
//     `failed` (Req 1.5).
//   * `+CMGR` inbound reads, recovering the sender number and message body
//     (Req 2.2).

use crate::models::MessageStatus;

/// A terminating result code returned by the modem for an AT command exchange.
///
/// `Timeout` is not produced by line classification (it has no on-wire line);
/// it is synthesised by the Modem Manager when no terminator arrives within
/// the configured timeout (Req 8.4, 8.5) and is included here so the manager
/// can represent every terminal outcome with a single type.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AtResult {
    /// `OK` — the command completed successfully.
    Ok,
    /// `ERROR` — a generic, code-less failure.
    Error,
    /// `+CMS ERROR: <code>` — an SMS-related failure with a numeric code.
    CmsError(u16),
    /// `+CME ERROR: <code>` — a mobile-equipment failure with a numeric code.
    CmeError(u16),
    /// No terminating result code arrived before the configured timeout.
    Timeout,
}

impl AtResult {
    /// Whether this result represents a successful exchange (`OK`).
    pub fn is_ok(&self) -> bool {
        matches!(self, AtResult::Ok)
    }

    /// The recovered numeric error code, if this result carries one.
    pub fn error_code(&self) -> Option<u16> {
        match self {
            AtResult::CmsError(code) | AtResult::CmeError(code) => Some(*code),
            _ => None,
        }
    }
}

/// Classification of a single response line read from the modem.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LineClass {
    /// The line is a terminating result code carrying the given [`AtResult`].
    Terminator(AtResult),
    /// The line does not terminate the exchange (echo, intermediate result,
    /// data line, or blank line).
    NonTerminating,
}

/// Classify a single modem response line as a terminating result code or as a
/// non-terminating line (Req 8.4).
///
/// Surrounding whitespace (including the trailing `\r`/`\n` of a serial line)
/// is ignored. A `+CMS ERROR:` / `+CME ERROR:` prefix always terminates the
/// exchange; when the trailing code parses as an integer it is recovered into
/// the typed variant, otherwise the line is treated as a generic [`AtResult::Error`]
/// so the exchange still terminates rather than hanging.
pub fn classify_line(line: &str) -> LineClass {
    let trimmed = line.trim();

    if trimmed == "OK" {
        return LineClass::Terminator(AtResult::Ok);
    }
    if trimmed == "ERROR" {
        return LineClass::Terminator(AtResult::Error);
    }
    if let Some(rest) = trimmed.strip_prefix("+CMS ERROR:") {
        let result = match rest.trim().parse::<u16>() {
            Ok(code) => AtResult::CmsError(code),
            Err(_) => AtResult::Error,
        };
        return LineClass::Terminator(result);
    }
    if let Some(rest) = trimmed.strip_prefix("+CME ERROR:") {
        let result = match rest.trim().parse::<u16>() {
            Ok(code) => AtResult::CmeError(code),
            Err(_) => AtResult::Error,
        };
        return LineClass::Terminator(result);
    }

    LineClass::NonTerminating
}

/// Format a `+CMGS: <ref>` intermediate result line for a send reference.
///
/// This is the inverse of [`parse_cmgs_reference`] and exists so the
/// round-trip property (format then parse recovers the same reference) can be
/// stated directly (Req 1.4).
pub fn format_cmgs_response(reference: u32) -> String {
    format!("+CMGS: {reference}")
}

/// Parse a `+CMGS: <ref>` line, recovering the message reference (Req 1.4).
///
/// Returns `None` when the line is not a `+CMGS:` result or the reference is
/// not a non-negative integer.
pub fn parse_cmgs_reference(line: &str) -> Option<u32> {
    let rest = line.trim().strip_prefix("+CMGS:")?;
    rest.trim().parse::<u32>().ok()
}

/// The outcome of an `AT+CMGS` send exchange parsed from its response lines.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SendOutcome {
    /// `Sent` when a `+CMGS: <ref>` was acknowledged with `OK`; otherwise
    /// `Failed`.
    pub status: MessageStatus,
    /// The recovered message reference, present only on success (Req 1.4).
    pub reference: Option<u32>,
    /// The recovered modem error code, present on a `+CMS`/`+CME ERROR`
    /// failure (Req 1.5).
    pub error_code: Option<u16>,
}

impl SendOutcome {
    fn sent(reference: u32) -> Self {
        SendOutcome {
            status: MessageStatus::Sent,
            reference: Some(reference),
            error_code: None,
        }
    }

    fn failed(error_code: Option<u16>) -> Self {
        SendOutcome {
            status: MessageStatus::Failed,
            reference: None,
            error_code,
        }
    }
}

/// Parse the response lines of an `AT+CMGS` send exchange into a [`SendOutcome`].
///
/// A `+CMGS: <ref>` followed by an `OK` terminator maps to status `sent` with
/// the recovered reference (Req 1.4). A `+CMS ERROR` / `+CME ERROR` terminator
/// maps to status `failed` with the recovered error code, and a bare `ERROR`
/// or a missing terminator maps to `failed` without a code (Req 1.5).
pub fn parse_send_outcome(lines: &[&str]) -> SendOutcome {
    let mut reference: Option<u32> = None;

    for line in lines {
        if let Some(r) = parse_cmgs_reference(line) {
            reference = Some(r);
            continue;
        }
        match classify_line(line) {
            LineClass::Terminator(AtResult::Ok) => {
                return match reference {
                    Some(r) => SendOutcome::sent(r),
                    // `OK` without a `+CMGS` reference is not a successful send.
                    None => SendOutcome::failed(None),
                };
            }
            LineClass::Terminator(AtResult::CmsError(code))
            | LineClass::Terminator(AtResult::CmeError(code)) => {
                return SendOutcome::failed(Some(code));
            }
            LineClass::Terminator(AtResult::Error)
            | LineClass::Terminator(AtResult::Timeout) => {
                return SendOutcome::failed(None);
            }
            LineClass::NonTerminating => {}
        }
    }

    // No terminator was seen in the supplied lines: treat as a failed send.
    SendOutcome::failed(None)
}

/// A parsed inbound message recovered from an `AT+CMGR` response.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedInbound {
    /// The sender (originating address) number.
    pub sender: String,
    /// The message body text.
    pub body: String,
}

/// Format an `AT+CMGR` response for the given sender and body.
///
/// Produces the standard text-mode shape
/// `+CMGR: "REC UNREAD","<sender>",,"<scts>"` header line followed by the body
/// and a terminating `OK`. This is the inverse of [`parse_cmgr`] so the
/// inbound round-trip property can be stated directly (Req 2.2).
pub fn format_cmgr_response(sender: &str, body: &str) -> String {
    // The service center timestamp embeds a comma inside quotes; quote-aware
    // parsing recovers the fields correctly regardless of it.
    format!("+CMGR: \"REC UNREAD\",\"{sender}\",,\"24/01/02,03:04:05+00\"\r\n{body}\r\nOK")
}

/// Parse an `AT+CMGR` response, recovering the sender number and message body
/// (Req 2.2).
///
/// Returns `None` when no `+CMGR:` header line is present or the sender field
/// is absent. The body is everything between the header line and the
/// terminating result code (`OK`/`ERROR`/...); multiple body lines are joined
/// with `\n`.
pub fn parse_cmgr(response: &str) -> Option<ParsedInbound> {
    let mut lines = response.lines();

    // Find the header line; `find` advances the iterator past it so the
    // remaining lines are the body followed by the terminator.
    let header = lines.find(|l| l.trim_start().starts_with("+CMGR:"))?;
    let sender = parse_cmgr_sender(header)?;

    let mut body_lines: Vec<&str> = Vec::new();
    for line in lines {
        match classify_line(line) {
            LineClass::Terminator(_) => break,
            LineClass::NonTerminating => body_lines.push(line),
        }
    }
    let body = body_lines.join("\n");

    Some(ParsedInbound { sender, body })
}

/// Recover the sender (originating address) from a `+CMGR:` header line.
///
/// The header fields are comma-separated, but the trailing timestamp field
/// contains a comma inside its quotes, so the split is quote-aware. The sender
/// is the second field (index 1), with its surrounding quotes removed.
fn parse_cmgr_sender(header: &str) -> Option<String> {
    let rest = header.trim_start().strip_prefix("+CMGR:")?;
    let fields = split_quoted_csv(rest.trim());
    let sender = fields.get(1)?.trim().trim_matches('"').to_string();
    Some(sender)
}

/// Split a comma-separated field list, treating commas inside double quotes as
/// part of the field rather than separators.
fn split_quoted_csv(s: &str) -> Vec<String> {
    let mut fields = Vec::new();
    let mut current = String::new();
    let mut in_quotes = false;

    for c in s.chars() {
        match c {
            '"' => {
                in_quotes = !in_quotes;
                current.push(c);
            }
            ',' if !in_quotes => fields.push(std::mem::take(&mut current)),
            _ => current.push(c),
        }
    }
    fields.push(current);
    fields
}

#[cfg(test)]
mod at_parsing_tests {
    use super::*;

    #[test]
    fn classifies_terminating_result_codes() {
        assert_eq!(classify_line("OK"), LineClass::Terminator(AtResult::Ok));
        assert_eq!(
            classify_line("ERROR"),
            LineClass::Terminator(AtResult::Error)
        );
        assert_eq!(
            classify_line("+CMS ERROR: 500"),
            LineClass::Terminator(AtResult::CmsError(500))
        );
        assert_eq!(
            classify_line("+CME ERROR: 30"),
            LineClass::Terminator(AtResult::CmeError(30))
        );
    }

    #[test]
    fn ignores_surrounding_whitespace_and_carriage_returns() {
        assert_eq!(
            classify_line("  OK\r\n"),
            LineClass::Terminator(AtResult::Ok)
        );
        assert_eq!(
            classify_line("+CMS ERROR:  42 \r"),
            LineClass::Terminator(AtResult::CmsError(42))
        );
    }

    #[test]
    fn classifies_non_terminating_lines() {
        assert_eq!(classify_line(""), LineClass::NonTerminating);
        assert_eq!(classify_line("AT+CMGS=\"+1\""), LineClass::NonTerminating);
        assert_eq!(classify_line("+CMGS: 42"), LineClass::NonTerminating);
        assert_eq!(classify_line("> "), LineClass::NonTerminating);
        assert_eq!(classify_line("hello body"), LineClass::NonTerminating);
    }

    #[test]
    fn error_with_unparseable_code_still_terminates() {
        // A verbose / non-numeric error string still terminates the exchange.
        assert_eq!(
            classify_line("+CME ERROR: SIM not inserted"),
            LineClass::Terminator(AtResult::Error)
        );
    }

    #[test]
    fn cmgs_reference_round_trips() {
        for reference in [0u32, 1, 42, 255, 65_535, 1_000_000] {
            let line = format_cmgs_response(reference);
            assert_eq!(parse_cmgs_reference(&line), Some(reference));
        }
    }

    #[test]
    fn parse_cmgs_reference_rejects_non_cmgs_lines() {
        assert_eq!(parse_cmgs_reference("OK"), None);
        assert_eq!(parse_cmgs_reference("+CMGR: stuff"), None);
        assert_eq!(parse_cmgs_reference("+CMGS: notanumber"), None);
    }

    #[test]
    fn send_outcome_maps_reference_to_sent() {
        let lines = ["AT+CMGS=\"+14155552671\"", "+CMGS: 42", "OK"];
        let outcome = parse_send_outcome(&lines);
        assert_eq!(
            outcome,
            SendOutcome {
                status: MessageStatus::Sent,
                reference: Some(42),
                error_code: None,
            }
        );
    }

    #[test]
    fn send_outcome_maps_cms_error_to_failed() {
        let lines = ["+CMS ERROR: 500"];
        let outcome = parse_send_outcome(&lines);
        assert_eq!(
            outcome,
            SendOutcome {
                status: MessageStatus::Failed,
                reference: None,
                error_code: Some(500),
            }
        );
    }

    #[test]
    fn send_outcome_maps_cme_error_to_failed() {
        let lines = ["+CME ERROR: 30"];
        let outcome = parse_send_outcome(&lines);
        assert_eq!(outcome.status, MessageStatus::Failed);
        assert_eq!(outcome.error_code, Some(30));
        assert_eq!(outcome.reference, None);
    }

    #[test]
    fn send_outcome_ok_without_reference_is_failed() {
        let outcome = parse_send_outcome(&["OK"]);
        assert_eq!(outcome.status, MessageStatus::Failed);
        assert_eq!(outcome.reference, None);
    }

    #[test]
    fn send_outcome_no_terminator_is_failed() {
        let outcome = parse_send_outcome(&["+CMGS: 7"]);
        assert_eq!(outcome.status, MessageStatus::Failed);
    }

    #[test]
    fn cmgr_round_trips_sender_and_body() {
        let parsed = parse_cmgr(&format_cmgr_response("+14155552671", "hello there")).unwrap();
        assert_eq!(parsed.sender, "+14155552671");
        assert_eq!(parsed.body, "hello there");
    }

    #[test]
    fn cmgr_round_trips_empty_body() {
        let parsed = parse_cmgr(&format_cmgr_response("+14155550000", "")).unwrap();
        assert_eq!(parsed.sender, "+14155550000");
        assert_eq!(parsed.body, "");
    }

    #[test]
    fn cmgr_returns_none_without_header() {
        assert_eq!(parse_cmgr("OK"), None);
        assert_eq!(parse_cmgr("just some text\r\nOK"), None);
    }

    #[test]
    fn cmgr_sender_parsing_ignores_timestamp_comma() {
        // The quoted timestamp contains a comma; the sender must still be the
        // second field, not a fragment of the timestamp.
        let header = "+CMGR: \"REC READ\",\"+441234567\",,\"24/01/02,03:04:05+00\"";
        assert_eq!(parse_cmgr_sender(header), Some("+441234567".to_string()));
    }
}

// ---------------------------------------------------------------------------
// Single-owner Modem Manager (Task 7.3)
// Requirements: 1.2, 1.3, 1.9, 2.1, 2.3, 2.5, 2.7, 2.8, 2.9, 8.1, 8.2, 8.3,
//               8.4, 8.5, 8.6, 8.7, 8.8, 10.1, 10.2, 10.3, 10.5, 10.6, 10.7
// ---------------------------------------------------------------------------
//
// The Modem Manager runs as a dedicated Tokio task that exclusively owns the
// serial port. Callers never touch the port directly: they submit a
// [`ModemRequest`] over an `mpsc` channel and await the reply on an embedded
// `oneshot`. Because the manager processes one request to completion before
// reading the next, at most one AT command is ever outstanding (Req 8.3).
//
// Between client requests the manager polls the port for unsolicited result
// codes (`+CMTI` new-message notifications, Req 2.1/2.5). A detected URC is
// handled through the same single-owner loop — read with `AT+CMGR`, persist,
// then delete with `AT+CMGD` (Req 2.3) — so URC handling never races with a
// client send.
//
// Health is surfaced through a shared [`ModemStatusSnapshot`] (see
// [`ModemHandle::status`]) which the manager refreshes periodically and on
// connect/disconnect. This is a deliberate, documented refinement of the
// `ModemRequest::Status` variant sketched in `design.md`: serving status from
// shared state keeps it available even while the port is down and the command
// loop is paused for reconnect backoff.

use std::collections::VecDeque;
use std::future::Future;
use std::io;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use chrono::Utc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::{mpsc, oneshot};
use tokio_serial::{SerialPortBuilderExt, SerialStream};

use crate::config::Config;
use crate::db::Db;
use crate::health::{ModemStatusSnapshot, SimStatus};
use crate::sms::{build_cmgs, segment_message};

/// Overall timeout for an `AT+CMGS` send result (Req 1.9). A send that returns
/// no result within this window is failed without retransmission.
const SEND_RESULT_TIMEOUT_SECS: u64 = 30;

/// Timeout for an `AT+CMGR` inbound read (Req 2.8).
const CMGR_READ_TIMEOUT_SECS: u64 = 10;

/// How long an idle `read_line` poll waits for a URC before yielding so the
/// loop can refresh status or notice a shutdown. Bytes arriving on the port
/// wake the read immediately, so URC detection latency is well under the 1 s
/// requirement (Req 2.1, 2.5) regardless of this cadence.
const URC_POLL_INTERVAL: Duration = Duration::from_millis(1000);

/// How often the manager refreshes the shared status snapshot while idle.
const STATUS_REFRESH_INTERVAL: Duration = Duration::from_secs(30);

impl std::fmt::Display for AtResult {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AtResult::Ok => write!(f, "OK"),
            AtResult::Error => write!(f, "ERROR"),
            AtResult::CmsError(c) => write!(f, "+CMS ERROR: {c}"),
            AtResult::CmeError(c) => write!(f, "+CME ERROR: {c}"),
            AtResult::Timeout => write!(f, "TIMEOUT"),
        }
    }
}

/// A completed AT command exchange: the issued command, the response lines
/// collected before the terminator, and the terminating [`AtResult`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AtExchange {
    /// The command that was issued (without the trailing carriage return).
    pub command: String,
    /// The response lines received before (and including) the terminator.
    pub lines: Vec<String>,
    /// The terminating result code, or [`AtResult::Timeout`].
    pub result: AtResult,
}

/// The outcome of an SMS send dispatched to the Modem Manager.
///
/// `status` is one of:
/// - [`MessageStatus::Sent`] — the modem acknowledged with `+CMGS: <ref>`,
///   recovered into `reference` (Req 1.4).
/// - [`MessageStatus::Failed`] — a modem error, a send timeout, or retry
///   exhaustion; `error_code`/`error` carry the detail (Req 1.5, 1.9, 10.7).
/// - [`MessageStatus::Queued`] — delivery preconditions were not met (modem
///   not registered / SIM not ready); the caller retains the message as
///   `queued` and retries later (Req 10.5).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SendResult {
    /// Resulting message status.
    pub status: MessageStatus,
    /// Modem-assigned reference, present only on a successful send (Req 1.4).
    pub reference: Option<u32>,
    /// Recovered modem error code, present on a `+CMS`/`+CME ERROR` (Req 1.5).
    pub error_code: Option<u16>,
    /// Human-readable error detail, present on failure or deferral.
    pub error: Option<String>,
}

/// A request submitted to the Modem Manager over its command channel.
pub enum ModemRequest {
    /// Issue a raw AT command and return the full [`AtExchange`].
    Raw {
        /// The AT command text (without the trailing carriage return).
        command: String,
        /// Channel on which the exchange result is returned.
        reply: oneshot::Sender<AtExchange>,
    },
    /// Send an SMS to `to` carrying `body`, segmenting as needed (Req 1.8).
    SendSms {
        /// Recipient phone number in E.164 format.
        to: String,
        /// Message body.
        body: String,
        /// Channel on which the [`SendResult`] is returned.
        reply: oneshot::Sender<SendResult>,
    },
}

/// A cheap, cloneable handle used by the rest of the service to talk to the
/// Modem Manager. All clones share the same command channel and status
/// snapshot.
#[derive(Clone)]
pub struct ModemHandle {
    tx: mpsc::Sender<ModemRequest>,
    status: Arc<Mutex<ModemStatusSnapshot>>,
}

impl ModemHandle {
    /// Dispatch an SMS send and await the result. Returns a `Failed`
    /// [`SendResult`] if the Modem Manager has shut down.
    pub async fn send_sms(&self, to: &str, body: &str) -> SendResult {
        let (reply, rx) = oneshot::channel();
        let request = ModemRequest::SendSms {
            to: to.to_string(),
            body: body.to_string(),
            reply,
        };
        if self.tx.send(request).await.is_err() {
            return SendResult {
                status: MessageStatus::Failed,
                reference: None,
                error_code: None,
                error: Some("modem manager unavailable".to_string()),
            };
        }
        rx.await.unwrap_or(SendResult {
            status: MessageStatus::Failed,
            reference: None,
            error_code: None,
            error: Some("modem manager dropped the reply".to_string()),
        })
    }

    /// Issue a raw AT command and await its exchange. Returns a `Timeout`
    /// exchange if the Modem Manager has shut down.
    pub async fn raw(&self, command: &str) -> AtExchange {
        let (reply, rx) = oneshot::channel();
        let request = ModemRequest::Raw {
            command: command.to_string(),
            reply,
        };
        let unavailable = || AtExchange {
            command: command.to_string(),
            lines: Vec::new(),
            result: AtResult::Timeout,
        };
        if self.tx.send(request).await.is_err() {
            return unavailable();
        }
        rx.await.unwrap_or_else(|_| unavailable())
    }

    /// A snapshot of the modem's current health, served from shared state so
    /// it is available even while the port is reconnecting.
    pub fn status(&self) -> ModemStatusSnapshot {
        self.status
            .lock()
            .expect("modem status mutex poisoned")
            .clone()
    }
}

/// The manager-side endpoint produced by [`new_modem`]: the receiving end of
/// the command channel and the shared status snapshot the manager updates.
pub struct ModemEndpoint {
    rx: mpsc::Receiver<ModemRequest>,
    status: Arc<Mutex<ModemStatusSnapshot>>,
}

/// The initial, all-down status snapshot used before the port is opened.
fn initial_snapshot() -> ModemStatusSnapshot {
    ModemStatusSnapshot {
        serial_connected: false,
        sim_status: SimStatus::Unknown,
        registered: false,
        responsive: false,
        signal_percent: None,
        operator: None,
    }
}

/// Create a connected ([`ModemHandle`], [`ModemEndpoint`]) pair. Give the
/// handle to the API layer and pass the endpoint to [`run_modem_manager`].
pub fn new_modem(buffer: usize) -> (ModemHandle, ModemEndpoint) {
    let (tx, rx) = mpsc::channel(buffer.max(1));
    let status = Arc::new(Mutex::new(initial_snapshot()));
    let handle = ModemHandle {
        tx,
        status: Arc::clone(&status),
    };
    let endpoint = ModemEndpoint { rx, status };
    (handle, endpoint)
}

/// An async, line-oriented serial transport. Abstracted as a trait so the
/// Modem Manager loop can be exercised with an in-memory mock (task 7.5)
/// without real hardware.
pub trait SerialTransport: Send {
    /// Write all `data` to the port and flush it.
    fn write_bytes(&mut self, data: &[u8]) -> impl Future<Output = io::Result<()>> + Send;

    /// Read one line (terminated by `\n`, with trailing `\r`/`\n` stripped).
    ///
    /// Resolves to `Ok(Some(line))` when a full line is available,
    /// `Ok(None)` when `timeout` elapses before a line completes (any
    /// partially-read bytes are retained for the next call), and `Err` when
    /// the port is closed or errors.
    fn read_line(
        &mut self,
        timeout: Duration,
    ) -> impl Future<Output = io::Result<Option<String>>> + Send;
}

/// The real serial transport backed by a `tokio-serial` [`SerialStream`].
///
/// A persistent `pending` buffer makes [`SerialTransport::read_line`]
/// cancellation-safe: bytes read but not yet consumed survive a cancelled
/// poll (e.g. when the manager's `select!` picks the request branch instead).
pub struct SerialPortTransport {
    stream: SerialStream,
    pending: Vec<u8>,
}

impl SerialPortTransport {
    /// Wrap an opened serial stream.
    pub fn new(stream: SerialStream) -> Self {
        SerialPortTransport {
            stream,
            pending: Vec::new(),
        }
    }
}

impl SerialTransport for SerialPortTransport {
    async fn write_bytes(&mut self, data: &[u8]) -> io::Result<()> {
        self.stream.write_all(data).await?;
        self.stream.flush().await
    }

    async fn read_line(&mut self, timeout: Duration) -> io::Result<Option<String>> {
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(pos) = self.pending.iter().position(|&b| b == b'\n') {
                let drained: Vec<u8> = self.pending.drain(..=pos).collect();
                let text = String::from_utf8_lossy(&drained);
                let line = text.trim_end_matches(|c| c == '\r' || c == '\n').to_string();
                return Ok(Some(line));
            }

            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Ok(None);
            }

            let mut tmp = [0u8; 256];
            match tokio::time::timeout(remaining, self.stream.read(&mut tmp)).await {
                Err(_) => return Ok(None),
                Ok(Ok(0)) => {
                    return Err(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "serial port closed",
                    ));
                }
                Ok(Ok(n)) => self.pending.extend_from_slice(&tmp[..n]),
                Ok(Err(e)) => return Err(e),
            }
        }
    }
}

/// Open the configured serial port at the configured baud rate (Req 8.1).
fn open_serial(cfg: &Config) -> tokio_serial::Result<SerialStream> {
    tokio_serial::new(&cfg.serial_port, cfg.baud_rate).open_native_async()
}

/// Write an AT command (appending the carriage return) and collect its
/// response up to a terminating result code or `timeout` (Req 8.4).
///
/// Exposed for integration testing (task 7.5) so a mock [`SerialTransport`]
/// can drive a single command exchange and inspect the collected lines.
pub async fn exchange<T: SerialTransport>(
    t: &mut T,
    command: &str,
    timeout: Duration,
) -> io::Result<AtExchange> {
    let mut bytes = Vec::with_capacity(command.len() + 1);
    bytes.extend_from_slice(command.as_bytes());
    bytes.push(b'\r');
    t.write_bytes(&bytes).await?;
    let exchange = collect_until_terminator(t, command, timeout).await?;
    tracing::debug!(command = %exchange.command, result = %exchange.result, "at_exchange");
    Ok(exchange)
}

/// Read response lines until a terminating result code arrives or `timeout`
/// elapses, in which case the exchange resolves with [`AtResult::Timeout`]
/// (Req 8.4, 8.5). The command payload must already have been written.
async fn collect_until_terminator<T: SerialTransport>(
    t: &mut T,
    command: &str,
    timeout: Duration,
) -> io::Result<AtExchange> {
    let deadline = Instant::now() + timeout;
    let mut lines: Vec<String> = Vec::new();
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Ok(AtExchange {
                command: command.to_string(),
                lines,
                result: AtResult::Timeout,
            });
        }
        match t.read_line(remaining).await? {
            None => {
                return Ok(AtExchange {
                    command: command.to_string(),
                    lines,
                    result: AtResult::Timeout,
                });
            }
            Some(line) => match classify_line(&line) {
                LineClass::Terminator(result) => {
                    lines.push(line);
                    return Ok(AtExchange {
                        command: command.to_string(),
                        lines,
                        result,
                    });
                }
                LineClass::NonTerminating => {
                    if !line.trim().is_empty() {
                        lines.push(line);
                    }
                }
            },
        }
    }
}

/// Run the SMS initialization sequence (Req 8.2, 8.6).
///
/// Issues `AT+CMGF=1`, `AT+CSCS="IRA"`, `AT+CSMP=17,167,0,0`, and — when a
/// service center number is configured — `AT+CSCA="<number>"`. Returns
/// `Ok(true)` when every command succeeded, `Ok(false)` when one returned an
/// error (the port is kept open for a later retry, Req 8.8), and `Err` when
/// the port disconnected mid-init.
///
/// Exposed for integration testing (task 7.5).
pub async fn initialize<T: SerialTransport>(
    cfg: &Config,
    t: &mut T,
    timeout: Duration,
) -> io::Result<bool> {
    let mut commands: Vec<String> = vec![
        "AT+CMGF=1".to_string(),
        "AT+CSCS=\"IRA\"".to_string(),
        "AT+CSMP=17,167,0,0".to_string(),
    ];
    if let Some(csca) = &cfg.service_center_number {
        commands.push(format!("AT+CSCA=\"{csca}\""));
    }

    let mut ok = true;
    for command in &commands {
        let exchange = exchange(t, command, timeout).await?;
        if !exchange.result.is_ok() {
            ok = false;
            tracing::error!(command = %command, result = %exchange.result, "modem init command failed");
        }
    }
    if ok {
        tracing::info!("modem initialized");
    }
    Ok(ok)
}

/// Insert an audit-log record, logging (but not propagating) any write error
/// so the caller can continue processing (Req 2.7, 2.9, 10.3, 10.7).
async fn audit(db: &Db, event_type: &str, key_identifier: Option<&str>, detail: &str) {
    let created_at = Utc::now().to_rfc3339();
    let result = sqlx::query(
        "INSERT INTO audit_log (event_type, key_identifier, detail, created_at) \
         VALUES (?, ?, ?, ?)",
    )
    .bind(event_type)
    .bind(key_identifier)
    .bind(detail)
    .bind(created_at)
    .execute(db.pool())
    .await;
    if let Err(e) = result {
        tracing::error!(error = %e, event_type, "failed to write audit log record");
    }
}

/// Scan response lines for `+CMTI` URCs and enqueue their storage indices.
fn scan_cmti(lines: &[String], pending: &mut VecDeque<u32>) {
    for line in lines {
        if let Some(index) = parse_cmti_index(line) {
            pending.push_back(index);
        }
    }
}

/// Parse the storage index from a `+CMTI: "<mem>",<index>` URC (Req 2.1).
pub fn parse_cmti_index(line: &str) -> Option<u32> {
    let rest = line.trim().strip_prefix("+CMTI:")?;
    rest.rsplit(',').next()?.trim().parse::<u32>().ok()
}

/// Map an `AT+CPIN?` response to a [`SimStatus`] (Req 9.3).
pub fn parse_cpin(lines: &[String]) -> SimStatus {
    match lines.iter().find_map(|l| l.trim().strip_prefix("+CPIN:")) {
        Some(rest) if rest.trim() == "READY" => SimStatus::Ready,
        Some(_) => SimStatus::NotReady,
        None => SimStatus::Unknown,
    }
}

/// Whether an `AT+CREG?` response reports network registration (stat 1 home or
/// 5 roaming) (Req 9.5).
pub fn parse_creg_registered(lines: &[String]) -> bool {
    if let Some(rest) = lines.iter().find_map(|l| l.trim().strip_prefix("+CREG:")) {
        if let Some(stat) = rest.split(',').nth(1) {
            return matches!(stat.trim().trim_matches('"'), "1" | "5");
        }
    }
    false
}

/// Convert an `AT+CSQ` response to a 0..=100 signal percentage, or `None` when
/// the RSSI is unknown (99) or out of range (Req 9.2).
pub fn parse_csq_percent(lines: &[String]) -> Option<u8> {
    let rest = lines.iter().find_map(|l| l.trim().strip_prefix("+CSQ:"))?;
    let rssi: u32 = rest.split(',').next()?.trim().parse().ok()?;
    if rssi == 99 || rssi > 31 {
        return None;
    }
    Some(((rssi * 100) / 31) as u8)
}

/// Recover the operator name from an `AT+COPS?` response (Req 9.2).
pub fn parse_cops_operator(lines: &[String]) -> Option<String> {
    let rest = lines.iter().find_map(|l| l.trim().strip_prefix("+COPS:"))?;
    let fields = split_quoted_csv(rest.trim());
    let operator = fields.get(2)?.trim().trim_matches('"').to_string();
    if operator.is_empty() {
        None
    } else {
        Some(operator)
    }
}

/// Refresh the shared status snapshot by querying SIM, registration, signal,
/// and operator (Req 9.2, 9.3, 9.5). Propagates a disconnect as `Err`.
async fn refresh_status<T: SerialTransport>(
    t: &mut T,
    status: &Arc<Mutex<ModemStatusSnapshot>>,
    timeout: Duration,
) -> io::Result<()> {
    let cpin = exchange(t, "AT+CPIN?", timeout).await?;
    let responsive = cpin.result != AtResult::Timeout;
    let sim_status = parse_cpin(&cpin.lines);

    let creg = exchange(t, "AT+CREG?", timeout).await?;
    let registered = parse_creg_registered(&creg.lines);

    let csq = exchange(t, "AT+CSQ", timeout).await?;
    let signal_percent = parse_csq_percent(&csq.lines);

    let cops = exchange(t, "AT+COPS?", timeout).await?;
    let operator = parse_cops_operator(&cops.lines);

    let mut snapshot = status.lock().expect("modem status mutex poisoned");
    snapshot.serial_connected = true;
    snapshot.responsive = responsive;
    snapshot.sim_status = sim_status;
    snapshot.registered = registered;
    snapshot.signal_percent = signal_percent;
    snapshot.operator = operator;
    Ok(())
}

/// Handle a detected inbound-message URC: read, persist, then delete.
///
/// Reads the message with `AT+CMGR` retrying up to 3 times on failure,
/// timeout, or a malformed response (Req 2.8). On a successful read the
/// message is persisted (Req 2.2) and then deleted with `AT+CMGD` (Req 2.3);
/// a delete error is audited and processing continues (Req 2.7). If
/// persistence fails the message is retained in modem storage, the delete is
/// skipped, and the failure is audited (Req 2.9). Returns `Err` only when the
/// port disconnects.
///
/// Exposed for integration testing (task 7.5).
pub async fn handle_inbound<T: SerialTransport>(
    t: &mut T,
    db: &Db,
    index: u32,
    timeout: Duration,
    pending: &mut VecDeque<u32>,
) -> io::Result<()> {
    let read_timeout = Duration::from_secs(CMGR_READ_TIMEOUT_SECS);
    let mut parsed: Option<ParsedInbound> = None;

    for attempt in 1..=3u32 {
        let exchange = exchange(t, &format!("AT+CMGR={index}"), read_timeout).await?;
        scan_cmti(&exchange.lines, pending);
        if exchange.result.is_ok() {
            let response = exchange.lines.join("\n");
            if let Some(message) = parse_cmgr(&response) {
                parsed = Some(message);
                break;
            }
        }
        tracing::warn!(index, attempt, result = %exchange.result, "AT+CMGR read failed or malformed");
    }

    let Some(message) = parsed else {
        audit(
            db,
            "inbound_read_failed",
            None,
            &format!("AT+CMGR for index {index} failed after 3 attempts"),
        )
        .await;
        return Ok(());
    };

    match db
        .create_inbound_message(&message.sender, &message.body, Utc::now())
        .await
    {
        Ok(record) => {
            tracing::info!(id = record.id, "inbound message persisted");
            let delete = exchange(t, &format!("AT+CMGD={index}"), timeout).await?;
            scan_cmti(&delete.lines, pending);
            if !delete.result.is_ok() {
                audit(
                    db,
                    "inbound_delete_failed",
                    None,
                    &format!("AT+CMGD for index {index} returned {}", delete.result),
                )
                .await;
            }
        }
        Err(e) => {
            // Retain in modem storage and skip the delete (Req 2.9).
            audit(
                db,
                "inbound_persist_failed",
                None,
                &format!("persisting inbound at index {index} failed: {e}"),
            )
            .await;
        }
    }
    Ok(())
}

/// Transmit a single message part, retrying transient modem errors (Req 10.6)
/// up to `send_max_attempts`, and failing without retransmission on a send
/// timeout (Req 1.9).
async fn send_part_with_retries<T: SerialTransport>(
    t: &mut T,
    cfg: &Config,
    db: &Db,
    to: &str,
    part: &str,
) -> io::Result<SendResult> {
    let max_attempts = cfg.send_max_attempts.max(1);
    let retry_delay = Duration::from_secs(cfg.send_retry_delay_secs);
    let send_timeout = Duration::from_secs(SEND_RESULT_TIMEOUT_SECS);
    let mut last_code: Option<u16> = None;

    for attempt in 1..=max_attempts {
        let payload = build_cmgs(to, part);
        t.write_bytes(&payload).await?;
        let exchange = collect_until_terminator(t, "AT+CMGS", send_timeout).await?;
        tracing::debug!(command = "AT+CMGS", result = %exchange.result, attempt, "at_exchange");

        let line_refs: Vec<&str> = exchange.lines.iter().map(String::as_str).collect();
        let outcome = parse_send_outcome(&line_refs);

        if outcome.status == MessageStatus::Sent {
            return Ok(SendResult {
                status: MessageStatus::Sent,
                reference: outcome.reference,
                error_code: None,
                error: None,
            });
        }

        // A send timeout is terminal and must not be retransmitted (Req 1.9).
        if exchange.result == AtResult::Timeout {
            audit(db, "send_failed", None, "AT+CMGS timed out with no result").await;
            return Ok(SendResult {
                status: MessageStatus::Failed,
                reference: None,
                error_code: None,
                error: Some("timeout".to_string()),
            });
        }

        // Otherwise treat the modem error as transient and retry (Req 10.6).
        last_code = outcome.error_code;
        tracing::warn!(attempt, code = ?last_code, "transient send error; will retry");
        if attempt < max_attempts {
            tokio::time::sleep(retry_delay).await;
        }
    }

    // Retry budget exhausted (Req 10.7).
    audit(
        db,
        "send_failed",
        None,
        &format!("send failed after {max_attempts} attempts (last code {last_code:?})"),
    )
    .await;
    Ok(SendResult {
        status: MessageStatus::Failed,
        reference: None,
        error_code: last_code,
        error: Some("maximum send attempts exhausted".to_string()),
    })
}

/// Perform a full send: gate on deliverability, segment, set text mode, and
/// transmit each part in order (Req 1.2, 1.3, 1.8, 10.5).
///
/// Exposed for integration testing (task 7.5).
pub async fn handle_send<T: SerialTransport>(
    t: &mut T,
    cfg: &Config,
    db: &Db,
    status: &Arc<Mutex<ModemStatusSnapshot>>,
    to: &str,
    body: &str,
) -> io::Result<SendResult> {
    let timeout = Duration::from_secs(cfg.at_timeout_secs);

    // Defer the send when the modem is not ready to deliver (Req 10.5).
    let deliverable = {
        let snapshot = status.lock().expect("modem status mutex poisoned");
        snapshot.sim_status.is_ready() && snapshot.registered
    };
    if !deliverable {
        return Ok(SendResult {
            status: MessageStatus::Queued,
            reference: None,
            error_code: None,
            error: Some("modem not ready for delivery; will retry".to_string()),
        });
    }

    let segments = match segment_message(body) {
        Ok(segments) => segments,
        Err(e) => {
            audit(db, "send_failed", None, &format!("segmentation error: {e}")).await;
            return Ok(SendResult {
                status: MessageStatus::Failed,
                reference: None,
                error_code: None,
                error: Some(e.to_string()),
            });
        }
    };

    // Set text mode before transmitting (Req 1.2). Recovers text mode even if
    // a prior init attempt failed (Req 8.8).
    let cmgf = exchange(t, "AT+CMGF=1", timeout).await?;
    if !cmgf.result.is_ok() {
        tracing::warn!(result = %cmgf.result, "AT+CMGF=1 before send returned non-OK");
    }

    let mut last_reference = None;
    for segment in &segments {
        let result = send_part_with_retries(t, cfg, db, to, &segment.text).await?;
        match result.status {
            MessageStatus::Sent => last_reference = result.reference,
            // Any failed/deferred part fails the whole message.
            _ => return Ok(result),
        }
    }

    Ok(SendResult {
        status: MessageStatus::Sent,
        reference: last_reference,
        error_code: None,
        error: None,
    })
}

/// Dispatch a single [`ModemRequest`] through the serial port. Returns `Err`
/// when the port disconnects so the caller can trigger a reconnect.
async fn handle_request<T: SerialTransport>(
    request: ModemRequest,
    cfg: &Config,
    db: &Db,
    status: &Arc<Mutex<ModemStatusSnapshot>>,
    t: &mut T,
    timeout: Duration,
    pending: &mut VecDeque<u32>,
) -> io::Result<()> {
    match request {
        ModemRequest::Raw { command, reply } => {
            let exchange = exchange(t, &command, timeout).await?;
            scan_cmti(&exchange.lines, pending);
            {
                let mut snapshot = status.lock().expect("modem status mutex poisoned");
                snapshot.responsive = exchange.result != AtResult::Timeout;
            }
            let _ = reply.send(exchange);
        }
        ModemRequest::SendSms { to, body, reply } => {
            let result = handle_send(t, cfg, db, status, &to, &body).await?;
            let _ = reply.send(result);
        }
    }
    Ok(())
}

/// How a single connected session ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionOutcome {
    /// All command-channel senders were dropped: shut down gracefully.
    ChannelClosed,
    /// The serial port disconnected: attempt to reconnect.
    Disconnected,
}

/// Run one connected session: serialize client requests, interleave URC
/// monitoring, and periodically refresh the status snapshot. The single-owner
/// loop guarantees at most one AT command is outstanding at a time (Req 8.3).
///
/// Exposed for integration testing (task 7.5) so a mock [`SerialTransport`]
/// can drive the session loop and observe its termination (e.g. a disconnect
/// that triggers reconnect-and-reinit).
pub async fn run_session<T: SerialTransport>(
    cfg: &Config,
    db: &Db,
    rx: &mut mpsc::Receiver<ModemRequest>,
    t: &mut T,
    status: &Arc<Mutex<ModemStatusSnapshot>>,
) -> SessionOutcome {
    let timeout = Duration::from_secs(cfg.at_timeout_secs);
    let mut pending: VecDeque<u32> = VecDeque::new();
    let mut last_refresh: Option<Instant> = None;

    loop {
        // Drain any inbound URCs detected so far (Req 2.1, 2.3).
        while let Some(index) = pending.pop_front() {
            if handle_inbound(t, db, index, timeout, &mut pending)
                .await
                .is_err()
            {
                return SessionOutcome::Disconnected;
            }
        }

        // Refresh the status snapshot on connect and at intervals.
        let due = last_refresh.is_none_or(|i| i.elapsed() >= STATUS_REFRESH_INTERVAL);
        if due {
            if refresh_status(t, status, timeout).await.is_err() {
                return SessionOutcome::Disconnected;
            }
            last_refresh = Some(Instant::now());
        }

        // Await the next client request or a URC line, whichever comes first.
        tokio::select! {
            maybe_request = rx.recv() => {
                match maybe_request {
                    None => return SessionOutcome::ChannelClosed,
                    Some(request) => {
                        if handle_request(request, cfg, db, status, t, timeout, &mut pending)
                            .await
                            .is_err()
                        {
                            return SessionOutcome::Disconnected;
                        }
                    }
                }
            }
            line = t.read_line(URC_POLL_INTERVAL) => {
                match line {
                    Ok(Some(line)) => {
                        if let Some(index) = parse_cmti_index(&line) {
                            pending.push_back(index);
                        }
                    }
                    Ok(None) => {}
                    Err(_) => return SessionOutcome::Disconnected,
                }
            }
        }
    }
}

/// The Modem Manager task: own the serial port, serialize AT exchanges,
/// interleave URC monitoring, and reconnect with exponential backoff and
/// re-initialization (Req 8.1–8.8, 10.1–10.3).
///
/// Returns when the command channel closes (graceful shutdown, Req 11.2) or
/// when the configured maximum number of reopen attempts is exhausted
/// (Req 10.3).
pub async fn run_modem_manager(cfg: Config, db: Db, endpoint: ModemEndpoint) {
    let ModemEndpoint { mut rx, status } = endpoint;
    let mut attempt: u32 = 0;

    loop {
        match open_serial(&cfg) {
            Ok(stream) => {
                attempt = 0;
                tracing::info!(port = %cfg.serial_port, baud = cfg.baud_rate, "serial port opened");
                {
                    let mut snapshot = status.lock().expect("modem status mutex poisoned");
                    snapshot.serial_connected = true;
                    snapshot.responsive = true;
                }

                let mut transport = SerialPortTransport::new(stream);
                let timeout = Duration::from_secs(cfg.at_timeout_secs);

                // Initialize SMS handling (Req 8.2, 8.6).
                match initialize(&cfg, &mut transport, timeout).await {
                    Ok(true) => {}
                    Ok(false) => {
                        // Keep the port open for a later retry (Req 8.8). The
                        // per-send AT+CMGF=1 recovers text mode.
                        audit(
                            &db,
                            "modem_init_failed",
                            None,
                            "one or more initialization commands returned an error",
                        )
                        .await;
                    }
                    Err(_) => {
                        tracing::error!("serial port lost during initialization");
                        mark_disconnected(&status);
                        if !backoff_or_giveup(&db, &cfg, &status, &mut attempt).await {
                            return;
                        }
                        continue;
                    }
                }

                // Re-initialization on reconnect happens by re-running the loop
                // body above on the next successful open (Req 10.2).
                let outcome = run_session(&cfg, &db, &mut rx, &mut transport, &status).await;
                mark_disconnected(&status);
                match outcome {
                    SessionOutcome::ChannelClosed => {
                        tracing::info!("modem manager shutting down (command channel closed)");
                        return;
                    }
                    SessionOutcome::Disconnected => {
                        tracing::warn!("serial port disconnected; attempting to reconnect");
                    }
                }
            }
            Err(e) => {
                // Failed to open the port (Req 8.7).
                tracing::error!(error = %e, port = %cfg.serial_port, "failed to open serial port");
                mark_disconnected(&status);
            }
        }

        if !backoff_or_giveup(&db, &cfg, &status, &mut attempt).await {
            return;
        }
    }
}

/// Mark the shared snapshot as disconnected and unresponsive.
fn mark_disconnected(status: &Arc<Mutex<ModemStatusSnapshot>>) {
    let mut snapshot = status.lock().expect("modem status mutex poisoned");
    snapshot.serial_connected = false;
    snapshot.responsive = false;
}

/// Advance the reconnect attempt counter and sleep for the backoff delay
/// (Req 10.1). Returns `false` when the maximum number of attempts has been
/// exhausted (Req 10.3), in which case the manager should give up.
async fn backoff_or_giveup(
    db: &Db,
    cfg: &Config,
    status: &Arc<Mutex<ModemStatusSnapshot>>,
    attempt: &mut u32,
) -> bool {
    *attempt += 1;
    if *attempt > cfg.reopen_max_attempts {
        audit(
            db,
            "modem_reconnect_exhausted",
            None,
            &format!("exhausted {} serial reopen attempts", cfg.reopen_max_attempts),
        )
        .await;
        mark_disconnected(status);
        tracing::error!("exhausted serial reopen attempts; modem manager giving up");
        return false;
    }
    let delay = reconnect_backoff_secs(*attempt);
    tracing::warn!(attempt = *attempt, delay_secs = delay, "reconnect backoff");
    tokio::time::sleep(Duration::from_secs(delay)).await;
    true
}

/// Test/diagnostic seam: drive a single Modem Manager session over an injected
/// [`SerialTransport`] instead of a real serial port.
///
/// Production code reaches the single-owner command loop through
/// [`run_modem_manager`], which owns a real [`SerialPortTransport`]. Exposing
/// the same [`run_session`] loop over an arbitrary transport lets the
/// "at most one AT command outstanding" invariant (Req 8.3, Property 27) be
/// exercised against an in-memory mock transport without real hardware, from a
/// separate integration-test crate that can only see the public API.
///
/// The call returns when the command channel closes (all [`ModemHandle`]
/// clones dropped) or the transport reports the port disconnected. It performs
/// no reconnect/backoff and no initialization — it is purely the per-session
/// request/URC loop whose single ownership of `transport` is the property
/// under test.
#[doc(hidden)]
pub async fn run_session_with_transport<T: SerialTransport>(
    cfg: Config,
    db: Db,
    endpoint: ModemEndpoint,
    mut transport: T,
) {
    let ModemEndpoint { mut rx, status } = endpoint;
    let _ = run_session(&cfg, &db, &mut rx, &mut transport, &status).await;
}

#[cfg(test)]
mod manager_parsing_tests {
    use super::*;

    #[test]
    fn cmti_index_is_parsed_from_urc() {
        assert_eq!(parse_cmti_index("+CMTI: \"SM\",3"), Some(3));
        assert_eq!(parse_cmti_index("+CMTI: \"ME\",12"), Some(12));
        assert_eq!(parse_cmti_index("  +CMTI: \"SM\",0 \r"), Some(0));
        assert_eq!(parse_cmti_index("OK"), None);
        assert_eq!(parse_cmti_index("+CMGS: 5"), None);
    }

    #[test]
    fn cpin_status_is_classified() {
        assert_eq!(parse_cpin(&["+CPIN: READY".to_string()]), SimStatus::Ready);
        assert_eq!(
            parse_cpin(&["+CPIN: SIM PIN".to_string()]),
            SimStatus::NotReady
        );
        assert_eq!(parse_cpin(&["OK".to_string()]), SimStatus::Unknown);
    }

    #[test]
    fn creg_registration_states() {
        assert!(parse_creg_registered(&["+CREG: 0,1".to_string()]));
        assert!(parse_creg_registered(&["+CREG: 2,5".to_string()]));
        assert!(!parse_creg_registered(&["+CREG: 0,2".to_string()]));
        assert!(!parse_creg_registered(&["+CREG: 0,0".to_string()]));
        assert!(!parse_creg_registered(&["OK".to_string()]));
    }

    #[test]
    fn csq_percent_scales_rssi() {
        assert_eq!(parse_csq_percent(&["+CSQ: 31,99".to_string()]), Some(100));
        assert_eq!(parse_csq_percent(&["+CSQ: 0,99".to_string()]), Some(0));
        assert_eq!(parse_csq_percent(&["+CSQ: 99,99".to_string()]), None);
        assert_eq!(parse_csq_percent(&["OK".to_string()]), None);
    }

    #[test]
    fn cops_operator_is_recovered() {
        assert_eq!(
            parse_cops_operator(&["+COPS: 0,0,\"Test Carrier\"".to_string()]),
            Some("Test Carrier".to_string())
        );
        assert_eq!(parse_cops_operator(&["+COPS: 0".to_string()]), None);
        assert_eq!(parse_cops_operator(&["OK".to_string()]), None);
    }
}
