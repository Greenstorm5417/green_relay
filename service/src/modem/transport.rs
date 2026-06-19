//! Serial transport: the byte-level read/write layer and the AT-command
//! exchange loop that turns a written command into a collected [`AtExchange`].

use std::future::Future;
use std::io;
use std::time::{Duration, Instant};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio_serial::{SerialPortBuilderExt, SerialStream};

use crate::config::Config;
use crate::sms::{SmsEncoding, build_cmgs_body, build_cmgs_header};

use super::protocol::{
    AtExchange, AtResult, LineClass, ModemMode, SMS_ABORT, classify_line, is_prompt,
};

/// How long to wait for the modem to acknowledge an ESC abort before giving up.
const ABORT_DRAIN_TIMEOUT: Duration = Duration::from_millis(800);

/// A bidirectional, line-oriented serial transport for AT commands.
pub trait SerialTransport: Send {
    fn write_bytes(&mut self, data: &[u8]) -> impl Future<Output = io::Result<()>> + Send;

    fn read_line(
        &mut self,
        timeout: Duration,
    ) -> impl Future<Output = io::Result<Option<String>>> + Send;

    /// Reads from the port until the SMS text-entry prompt (`>`) appears or the
    /// timeout elapses. Returns `Ok(true)` if the prompt was seen, `Ok(false)`
    /// on timeout, and `Err` if the port dropped.
    ///
    /// The default implementation consumes whole lines and is suitable for
    /// line-oriented (test) transports; [`SerialPortTransport`] overrides it to
    /// detect the real, newline-less `"> "` prompt at the byte level.
    fn wait_for_prompt(
        &mut self,
        timeout: Duration,
    ) -> impl Future<Output = io::Result<bool>> + Send {
        async move {
            loop {
                match self.read_line(timeout).await? {
                    Some(line) if is_prompt(&line) => return Ok(true),
                    Some(_) => continue,
                    None => return Ok(false),
                }
            }
        }
    }
}

/// A [`SerialTransport`] backed by a real OS serial port.
pub struct SerialPortTransport {
    stream: SerialStream,
    pending: Vec<u8>,
}

impl SerialPortTransport {
    /// Wraps an opened serial stream.
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

    async fn wait_for_prompt(&mut self, timeout: Duration) -> io::Result<bool> {
        let deadline = Instant::now()
            .checked_add(timeout)
            .unwrap_or_else(Instant::now);
        loop {
            // The prompt is a bare `>` (optionally followed by a space) with no
            // newline, so scan the raw buffer rather than waiting for a line.
            if let Some(pos) = self.pending.iter().position(|&b| b == b'>') {
                let mut end = pos.saturating_add(1);
                if self.pending.get(end) == Some(&b' ') {
                    end = end.saturating_add(1);
                }
                self.pending.drain(..end);
                return Ok(true);
            }

            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Ok(false);
            }

            let mut tmp = [0u8; 256];
            match tokio::time::timeout(remaining, self.stream.read(&mut tmp)).await {
                Err(_) => return Ok(false),
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

/// Opens the configured serial port in async mode.
pub(crate) fn open_serial(cfg: &Config) -> tokio_serial::Result<SerialStream> {
    tokio_serial::new(&cfg.serial_port, cfg.baud_rate).open_native_async()
}

/// Writes a command and collects the response until a terminator or timeout.
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

/// Reads response lines until a terminator line or the timeout elapses.
pub(crate) async fn collect_until_terminator<T: SerialTransport>(
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

/// Reverts the modem from SMS text-entry back to command mode.
///
/// Writes the ESC abort byte (`0x1B`) and drains the modem's acknowledgement so
/// a stranded `>` prompt can never swallow the next AT command as message text.
/// A port drop during the abort surfaces as an error so the manager reconnects;
/// a quiet modem (no echo) is tolerated via a short drain timeout.
pub async fn abort_to_command_mode<T: SerialTransport>(t: &mut T) -> io::Result<()> {
    t.write_bytes(&[SMS_ABORT]).await?;
    // The cancel is acknowledged with OK/ERROR (or nothing); consume it best-effort.
    let _ = collect_until_terminator(t, "ESC", ABORT_DRAIN_TIMEOUT).await?;
    Ok(())
}

/// Proactively returns a possibly-wedged modem to command mode at session
/// start: a modem left at the `>` prompt by a prior crashed or timed-out send
/// is rescued here (via ESC) before initialization runs.
pub async fn recover_command_mode<T: SerialTransport>(t: &mut T) -> io::Result<()> {
    tracing::debug!("recovering modem to command mode (ESC)");
    abort_to_command_mode(t).await
}

/// Submits one SMS part through the SIM7600 two-phase exchange, tracking the
/// line state and *always* reverting to command mode on any non-success path.
///
/// 1. write `AT+CMGS="<to>"`            ([`ModemMode::Command`] → [`ModemMode::AwaitingPrompt`])
/// 2. wait for the `>` prompt           ([`ModemMode::AwaitingPrompt`] → [`ModemMode::TextEntry`])
/// 3. write `<body>` + Ctrl-Z, then collect `+CMGS:`/`OK`/`ERROR`
/// 4. revert to command mode (ESC) whenever the prompt or result times out, so
///    a stranded text-entry state can never swallow later commands.
///
/// The returned [`AtExchange`] carries the collected result lines; a missing
/// prompt is reported as [`AtResult::Timeout`]. On return the modem is
/// guaranteed to be back in command mode.
pub async fn send_sms_part<T: SerialTransport>(
    t: &mut T,
    to: &str,
    body: &str,
    encoding: SmsEncoding,
    prompt_timeout: Duration,
    result_timeout: Duration,
) -> io::Result<AtExchange> {
    // Phase 1: open text-entry with the CMGS header.
    t.write_bytes(&build_cmgs_header(to, encoding)).await?;
    let mut mode = ModemMode::AwaitingPrompt;
    tracing::debug!(?mode, "sms send: header written, awaiting '>' prompt");

    // Phase 2: the modem must answer with the `>` prompt before it will take
    // the body. No prompt → abort so the half-issued command does not strand us.
    if !t.wait_for_prompt(prompt_timeout).await? {
        tracing::warn!(?mode, "sms send: no '>' prompt within timeout; aborting");
        abort_to_command_mode(t).await?;
        return Ok(AtExchange {
            command: "AT+CMGS".to_string(),
            lines: Vec::new(),
            result: AtResult::Timeout,
        });
    }
    mode = ModemMode::TextEntry;
    tracing::debug!(?mode, "sms send: prompt received, writing body + Ctrl-Z");

    // Phase 3: body terminated by Ctrl-Z, then collect the terminal result.
    t.write_bytes(&build_cmgs_body(body, encoding)).await?;
    let exchange = collect_until_terminator(t, "AT+CMGS", result_timeout).await?;

    // Phase 4: guarantee we leave in command mode. A timeout means the modem is
    // still at the prompt holding our body, so abort to release it.
    if exchange.result == AtResult::Timeout {
        tracing::warn!("sms send: timed out awaiting result; aborting to command mode");
        abort_to_command_mode(t).await?;
    }
    mode = ModemMode::Command;
    tracing::debug!(?mode, result = %exchange.result, "sms send: reverted to command mode");

    Ok(exchange)
}
