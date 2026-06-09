
pub const RECONNECT_BACKOFF_CAP_SECS: u64 = 60;

pub fn reconnect_backoff_secs(attempt: u32) -> u64 {
    let exponent = attempt.saturating_sub(1);

    if exponent >= 6 {
        return RECONNECT_BACKOFF_CAP_SECS;
    }

    let delay = 1u64 << exponent;
    delay.min(RECONNECT_BACKOFF_CAP_SECS)
}

pub fn reconnect_backoff_schedule(max_attempts: u32) -> Vec<u64> {
    (1..=max_attempts).map(reconnect_backoff_secs).collect()
}

use crate::models::MessageStatus;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AtResult {
    Ok,
    Error,
    CmsError(u16),
    CmeError(u16),
    Timeout,
}

impl AtResult {
    
    pub fn is_ok(&self) -> bool {
        matches!(self, AtResult::Ok)
    }

    pub fn error_code(&self) -> Option<u16> {
        match self {
            AtResult::CmsError(code) | AtResult::CmeError(code) => Some(*code),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LineClass {
    Terminator(AtResult),
    NonTerminating,
}

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

pub fn format_cmgs_response(reference: u32) -> String {
    format!("+CMGS: {reference}")
}

pub fn parse_cmgs_reference(line: &str) -> Option<u32> {
    let rest = line.trim().strip_prefix("+CMGS:")?;
    rest.trim().parse::<u32>().ok()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SendOutcome {
    pub status: MessageStatus,
    pub reference: Option<u32>,
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
                    None => SendOutcome::failed(None),
                };
            }
            LineClass::Terminator(AtResult::CmsError(code))
            | LineClass::Terminator(AtResult::CmeError(code)) => {
                return SendOutcome::failed(Some(code));
            }
            LineClass::Terminator(AtResult::Error) | LineClass::Terminator(AtResult::Timeout) => {
                return SendOutcome::failed(None);
            }
            LineClass::NonTerminating => {}
        }
    }

    SendOutcome::failed(None)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedInbound {
    pub sender: String,
    pub body: String,
}

pub fn format_cmgr_response(sender: &str, body: &str) -> String {
    format!("+CMGR: \"REC UNREAD\",\"{sender}\",,\"24/01/02,03:04:05+00\"\r\n{body}\r\nOK")
}

pub fn parse_cmgr(response: &str) -> Option<ParsedInbound> {
    let mut lines = response.lines();

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

fn parse_cmgr_sender(header: &str) -> Option<String> {
    let rest = header.trim_start().strip_prefix("+CMGR:")?;
    let fields = split_quoted_csv(rest.trim());
    let sender = fields.get(1)?.trim().trim_matches('"').to_string();
    Some(sender)
}

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
        let header = "+CMGR: \"REC READ\",\"+441234567\",,\"24/01/02,03:04:05+00\"";
        assert_eq!(parse_cmgr_sender(header), Some("+441234567".to_string()));
    }
}

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

const SEND_RESULT_TIMEOUT_SECS: u64 = 30;
const CMGR_READ_TIMEOUT_SECS: u64 = 10;
const URC_POLL_INTERVAL: Duration = Duration::from_millis(1000);
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AtExchange {
    pub command: String,
    pub lines: Vec<String>,
    pub result: AtResult,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SendResult {
    pub status: MessageStatus,
    pub reference: Option<u32>,
    pub error_code: Option<u16>,
    pub error: Option<String>,
}

pub enum ModemRequest {
    Raw {
        command: String,
        reply: oneshot::Sender<AtExchange>,
    },
    SendSms {
        to: String,
        body: String,
        reply: oneshot::Sender<SendResult>,
    },
}

#[derive(Clone)]
pub struct ModemHandle {
    tx: mpsc::Sender<ModemRequest>,
    status: Arc<Mutex<ModemStatusSnapshot>>,
}

impl ModemHandle {
    
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

    pub fn status(&self) -> ModemStatusSnapshot {
        self.status
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }
}

pub struct ModemEndpoint {
    rx: mpsc::Receiver<ModemRequest>,
    status: Arc<Mutex<ModemStatusSnapshot>>,
}

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

pub trait SerialTransport: Send {
    
    fn write_bytes(&mut self, data: &[u8]) -> impl Future<Output = io::Result<()>> + Send;

    fn read_line(
        &mut self,
        timeout: Duration,
    ) -> impl Future<Output = io::Result<Option<String>>> + Send;
}

pub struct SerialPortTransport {
    stream: SerialStream,
    pending: Vec<u8>,
}

impl SerialPortTransport {
    
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
        let deadline = Instant::now()
            .checked_add(timeout)
            .unwrap_or_else(Instant::now);
        loop {
            if let Some(pos) = self.pending.iter().position(|&b| b == b'\n') {
                let drained: Vec<u8> = self.pending.drain(..=pos).collect();
                let text = String::from_utf8_lossy(&drained);
                let line = text.trim_end_matches(['\r', '\n']).to_string();
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
                Ok(Ok(n)) => {
                    if let Some(chunk) = tmp.get(..n) {
                        self.pending.extend_from_slice(chunk);
                    }
                }
                Ok(Err(e)) => return Err(e),
            }
        }
    }
}

fn open_serial(cfg: &Config) -> tokio_serial::Result<SerialStream> {
    tokio_serial::new(&cfg.serial_port, cfg.baud_rate).open_native_async()
}

pub async fn exchange<T: SerialTransport>(
    t: &mut T,
    command: &str,
    timeout: Duration,
) -> io::Result<AtExchange> {
    let mut bytes = Vec::with_capacity(command.len().saturating_add(1));
    bytes.extend_from_slice(command.as_bytes());
    bytes.push(b'\r');
    t.write_bytes(&bytes).await?;
    let exchange = collect_until_terminator(t, command, timeout).await?;
    tracing::debug!(command = %exchange.command, result = %exchange.result, "at_exchange");
    Ok(exchange)
}

async fn collect_until_terminator<T: SerialTransport>(
    t: &mut T,
    command: &str,
    timeout: Duration,
) -> io::Result<AtExchange> {
    let deadline = Instant::now()
        .checked_add(timeout)
        .unwrap_or_else(Instant::now);
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

fn scan_cmti(lines: &[String], pending: &mut VecDeque<u32>) {
    for line in lines {
        if let Some(index) = parse_cmti_index(line) {
            pending.push_back(index);
        }
    }
}

pub fn parse_cmti_index(line: &str) -> Option<u32> {
    let rest = line.trim().strip_prefix("+CMTI:")?;
    rest.rsplit(',').next()?.trim().parse::<u32>().ok()
}

pub fn parse_cpin(lines: &[String]) -> SimStatus {
    match lines.iter().find_map(|l| l.trim().strip_prefix("+CPIN:")) {
        Some(rest) if rest.trim() == "READY" => SimStatus::Ready,
        Some(_) => SimStatus::NotReady,
        None => SimStatus::Unknown,
    }
}

pub fn parse_creg_registered(lines: &[String]) -> bool {
    if let Some(rest) = lines.iter().find_map(|l| l.trim().strip_prefix("+CREG:"))
        && let Some(stat) = rest.split(',').nth(1)
    {
        return matches!(stat.trim().trim_matches('"'), "1" | "5");
    }
    false
}

pub fn parse_csq_percent(lines: &[String]) -> Option<u8> {
    let rest = lines.iter().find_map(|l| l.trim().strip_prefix("+CSQ:"))?;
    let rssi: u32 = rest.split(',').next()?.trim().parse().ok()?;
    if rssi == 99 || rssi > 31 {
        return None;
    }
    Some((rssi.saturating_mul(100) / 31) as u8)
}

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

    let mut snapshot = status
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    snapshot.serial_connected = true;
    snapshot.responsive = responsive;
    snapshot.sim_status = sim_status;
    snapshot.registered = registered;
    snapshot.signal_percent = signal_percent;
    snapshot.operator = operator;
    Ok(())
}

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

        if exchange.result == AtResult::Timeout {
            audit(db, "send_failed", None, "AT+CMGS timed out with no result").await;
            return Ok(SendResult {
                status: MessageStatus::Failed,
                reference: None,
                error_code: None,
                error: Some("timeout".to_string()),
            });
        }

        last_code = outcome.error_code;
        tracing::warn!(attempt, code = ?last_code, "transient send error; will retry");
        if attempt < max_attempts {
            tokio::time::sleep(retry_delay).await;
        }
    }

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

pub async fn handle_send<T: SerialTransport>(
    t: &mut T,
    cfg: &Config,
    db: &Db,
    status: &Arc<Mutex<ModemStatusSnapshot>>,
    to: &str,
    body: &str,
) -> io::Result<SendResult> {
    let timeout = Duration::from_secs(cfg.at_timeout_secs);

    let deliverable = {
        let snapshot = status
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
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

    let cmgf = exchange(t, "AT+CMGF=1", timeout).await?;
    if !cmgf.result.is_ok() {
        tracing::warn!(result = %cmgf.result, "AT+CMGF=1 before send returned non-OK");
    }

    let mut last_reference = None;
    for segment in &segments {
        let result = send_part_with_retries(t, cfg, db, to, &segment.text).await?;
        match result.status {
            MessageStatus::Sent => last_reference = result.reference,
            
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
                let mut snapshot = status
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionOutcome {
    
    ChannelClosed,
    
    Disconnected,
}

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
        while let Some(index) = pending.pop_front() {
            if handle_inbound(t, db, index, timeout, &mut pending)
                .await
                .is_err()
            {
                return SessionOutcome::Disconnected;
            }
        }

        let due = last_refresh.is_none_or(|i| i.elapsed() >= STATUS_REFRESH_INTERVAL);
        if due {
            if refresh_status(t, status, timeout).await.is_err() {
                return SessionOutcome::Disconnected;
            }
            last_refresh = Some(Instant::now());
        }

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

pub async fn run_modem_manager(cfg: Config, db: Db, endpoint: ModemEndpoint) {
    let ModemEndpoint { mut rx, status } = endpoint;
    let mut attempt: u32 = 0;

    loop {
        match open_serial(&cfg) {
            Ok(stream) => {
                attempt = 0;
                tracing::info!(port = %cfg.serial_port, baud = cfg.baud_rate, "serial port opened");
                {
                    let mut snapshot = status
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    snapshot.serial_connected = true;
                    snapshot.responsive = true;
                }

                let mut transport = SerialPortTransport::new(stream);
                let timeout = Duration::from_secs(cfg.at_timeout_secs);

                match initialize(&cfg, &mut transport, timeout).await {
                    Ok(true) => {}
                    Ok(false) => {
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
                
                tracing::error!(error = %e, port = %cfg.serial_port, "failed to open serial port");
                mark_disconnected(&status);
            }
        }

        if !backoff_or_giveup(&db, &cfg, &status, &mut attempt).await {
            return;
        }
    }
}

fn mark_disconnected(status: &Arc<Mutex<ModemStatusSnapshot>>) {
    let mut snapshot = status
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    snapshot.serial_connected = false;
    snapshot.responsive = false;
}

async fn backoff_or_giveup(
    db: &Db,
    cfg: &Config,
    status: &Arc<Mutex<ModemStatusSnapshot>>,
    attempt: &mut u32,
) -> bool {
    *attempt = attempt.saturating_add(1);
    if *attempt > cfg.reopen_max_attempts {
        audit(
            db,
            "modem_reconnect_exhausted",
            None,
            &format!(
                "exhausted {} serial reopen attempts",
                cfg.reopen_max_attempts
            ),
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
