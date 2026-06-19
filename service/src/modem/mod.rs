//! Cellular modem actor.
//!
//! The modem is owned by a single async task (the "modem manager") that holds
//! the serial port and serializes all access to it. Callers interact through a
//! cloneable [`ModemHandle`] that sends [`ModemRequest`]s over a channel and
//! awaits replies, so the hardware is never touched concurrently.
//!
//! The implementation is split across submodules:
//! - [`protocol`] — pure AT parsing/formatting and the backoff schedule (no I/O).
//! - [`transport`] — the byte-level serial read/write and command exchange loop.
//! - [`session`] — initialization, status refresh, inbound/outbound handling,
//!   and the per-connection request/URC loop.
//!
//! This module holds the actor surface (the handle, request types, and the
//! reconnecting manager loop) and re-exports the public items of the
//! submodules so callers continue to use `crate::modem::*`.

mod protocol;
mod session;
mod transport;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::{mpsc, oneshot};

use crate::config::Config;
use crate::db::Db;
use crate::events::EventBus;
use crate::health::{ModemStatusSnapshot, SimStatus};
use crate::models::MessageStatus;

pub use protocol::{
    AtExchange, AtResult, LineClass, ModemMode, ParsedInbound, RECONNECT_BACKOFF_CAP_SECS,
    SMS_ABORT, SMS_SUBMIT, SendOutcome, classify_line, format_cmgr_response, format_cmgs_response,
    is_prompt, parse_cmgr, parse_cmgs_reference, parse_cmti_index, parse_cops_operator, parse_cpin,
    parse_creg_registered, parse_csq_percent, parse_send_outcome, reconnect_backoff_schedule,
    reconnect_backoff_secs,
};
pub use session::{SessionOutcome, handle_inbound, handle_send, initialize, run_session};
pub use transport::{
    SerialPortTransport, SerialTransport, abort_to_command_mode, exchange, recover_command_mode,
    send_sms_part,
};

use session::{audit, run_session_inner};
use transport::open_serial;

/// The terminal result of an outbound send delivered back to a caller.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SendResult {
    pub status: MessageStatus,
    pub reference: Option<String>,
    pub error_code: Option<u16>,
    pub error: Option<String>,
}

/// A request submitted to the modem manager over its command channel.
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

/// A cloneable handle for submitting work to the modem manager.
#[derive(Clone)]
pub struct ModemHandle {
    tx: mpsc::Sender<ModemRequest>,
    status: Arc<Mutex<ModemStatusSnapshot>>,
}

impl ModemHandle {
    /// Sends an SMS, awaiting the manager's terminal result.
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

    /// Runs a raw AT command, awaiting the collected exchange.
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

    /// Returns the latest modem status snapshot.
    pub fn status(&self) -> ModemStatusSnapshot {
        self.status
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }
}

/// The manager-side end of a modem channel: the receiver and shared status.
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

/// Creates a linked [`ModemHandle`]/[`ModemEndpoint`] pair.
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

/// Owns the serial port and reconnects on loss, running a session per connection.
pub async fn run_modem_manager(cfg: Config, db: Db, endpoint: ModemEndpoint, events: EventBus) {
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

                // Rescue a modem left at the SMS `>` prompt by a prior crashed
                // or timed-out send before we try to initialize: an ESC reverts
                // it to command mode so the init commands are not swallowed as
                // message text.
                if recover_command_mode(&mut transport).await.is_err() {
                    tracing::error!("serial port lost during modem recovery");
                    mark_disconnected(&status);
                    if !backoff_or_giveup(&db, &cfg, &status, &mut attempt).await {
                        return;
                    }
                    continue;
                }

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

                let outcome =
                    run_session_inner(&cfg, &db, &mut rx, &mut transport, &status, Some(&events))
                        .await;
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
