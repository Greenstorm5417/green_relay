//! In-session work: device initialization, status refresh, inbound retrieval,
//! outbound send with retries, and the per-connection request/URC loop. These
//! functions drive a connected [`SerialTransport`]; reconnection and the actor
//! plumbing live in the parent [`crate::modem`] module.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use chrono::Utc;
use tokio::sync::mpsc;

use crate::config::Config;
use crate::db::Db;
use crate::events::{EventBus, InboundSmsEvent, ServiceEvent};
use crate::health::ModemStatusSnapshot;
use crate::models::MessageStatus;
use crate::sms::{build_cmgs, segment_message};

use super::protocol::{
    AtResult, ParsedInbound, parse_cmgr, parse_cmti_index, parse_cops_operator, parse_cpin,
    parse_creg_registered, parse_csq_percent, parse_send_outcome,
};
use super::transport::{SerialTransport, collect_until_terminator, exchange};
use super::{ModemRequest, SendResult};

const SEND_RESULT_TIMEOUT_SECS: u64 = 30;
const CMGR_READ_TIMEOUT_SECS: u64 = 10;
const URC_POLL_INTERVAL: Duration = Duration::from_millis(1000);
const STATUS_REFRESH_INTERVAL: Duration = Duration::from_secs(30);

pub(crate) async fn audit(db: &Db, event_type: &str, key_identifier: Option<&str>, detail: &str) {
    if let Err(e) = db
        .insert_audit(event_type, key_identifier, Some(detail), Utc::now())
        .await
    {
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

/// Runs the modem's one-time SMS setup sequence; returns false if any command
/// returned a non-OK result.
pub async fn initialize<T: SerialTransport>(
    cfg: &Config,
    t: &mut T,
    timeout: Duration,
) -> std::io::Result<bool> {
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

async fn refresh_status<T: SerialTransport>(
    t: &mut T,
    status: &Arc<Mutex<ModemStatusSnapshot>>,
    timeout: Duration,
) -> std::io::Result<()> {
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

/// Reads, persists, and deletes the inbound message at the given storage index.
pub async fn handle_inbound<T: SerialTransport>(
    t: &mut T,
    db: &Db,
    index: u32,
    timeout: Duration,
    pending: &mut VecDeque<u32>,
) -> std::io::Result<()> {
    handle_inbound_inner(t, db, index, timeout, pending, None).await
}

async fn handle_inbound_inner<T: SerialTransport>(
    t: &mut T,
    db: &Db,
    index: u32,
    timeout: Duration,
    pending: &mut VecDeque<u32>,
    events: Option<&EventBus>,
) -> std::io::Result<()> {
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
            if let Some(events) = events {
                events.publish(ServiceEvent::InboundSms(InboundSmsEvent {
                    id: record.id,
                    from: message.sender.clone(),
                    body: message.body.clone(),
                }));
            }
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
) -> std::io::Result<SendResult> {
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
                reference: outcome.reference.map(|r| r.to_string()),
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

/// Segments and sends an outbound message, returning the terminal result.
pub async fn handle_send<T: SerialTransport>(
    t: &mut T,
    cfg: &Config,
    db: &Db,
    status: &Arc<Mutex<ModemStatusSnapshot>>,
    to: &str,
    body: &str,
) -> std::io::Result<SendResult> {
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
) -> std::io::Result<()> {
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

/// Why a session loop ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionOutcome {
    ChannelClosed,

    Disconnected,
}

/// Runs the per-connection request/URC loop until the channel closes or the
/// transport disconnects.
pub async fn run_session<T: SerialTransport>(
    cfg: &Config,
    db: &Db,
    rx: &mut mpsc::Receiver<ModemRequest>,
    t: &mut T,
    status: &Arc<Mutex<ModemStatusSnapshot>>,
) -> SessionOutcome {
    run_session_inner(cfg, db, rx, t, status, None).await
}

pub(crate) async fn run_session_inner<T: SerialTransport>(
    cfg: &Config,
    db: &Db,
    rx: &mut mpsc::Receiver<ModemRequest>,
    t: &mut T,
    status: &Arc<Mutex<ModemStatusSnapshot>>,
    events: Option<&EventBus>,
) -> SessionOutcome {
    let timeout = Duration::from_secs(cfg.at_timeout_secs);
    let mut pending: VecDeque<u32> = VecDeque::new();
    let mut last_refresh: Option<Instant> = None;

    loop {
        while let Some(index) = pending.pop_front() {
            if handle_inbound_inner(t, db, index, timeout, &mut pending, events)
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
