//! Serial transport: the byte-level read/write layer and the AT-command
//! exchange loop that turns a written command into a collected [`AtExchange`].

use std::future::Future;
use std::io;
use std::time::{Duration, Instant};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio_serial::{SerialPortBuilderExt, SerialStream};

use crate::config::Config;

use super::protocol::{AtExchange, AtResult, LineClass, classify_line};

/// A bidirectional, line-oriented serial transport for AT commands.
pub trait SerialTransport: Send {
    fn write_bytes(&mut self, data: &[u8]) -> impl Future<Output = io::Result<()>> + Send;

    fn read_line(
        &mut self,
        timeout: Duration,
    ) -> impl Future<Output = io::Result<Option<String>>> + Send;
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
