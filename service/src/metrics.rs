//! Process-wide counters exposed at `/metrics` in Prometheus text exposition
//! format.
//!
//! Hand-rolled with atomic counters rather than a metrics framework to keep the
//! dependency surface small and stay within the crate's panic-free lints. All
//! counters use relaxed ordering: exact inter-counter consistency is not
//! required for monitoring, only eventual accuracy per counter.

use std::sync::atomic::{AtomicU64, Ordering};

use crate::health::ModemStatusSnapshot;

/// Monotonic service counters plus a renderer for the current modem gauges.
#[derive(Debug, Default)]
pub struct Metrics {
    auth_failures: AtomicU64,
    rate_limited: AtomicU64,
    messages_accepted: AtomicU64,
    messages_sent: AtomicU64,
    messages_failed: AtomicU64,
}

impl Metrics {
    /// Creates a fresh metrics registry with all counters at zero.
    pub fn new() -> Self {
        Metrics::default()
    }

    /// Records a rejected authentication attempt (invalid, unknown, or locked).
    pub fn record_auth_failure(&self) {
        self.auth_failures.fetch_add(1, Ordering::Relaxed);
    }

    /// Records a request rejected by the rate limiter.
    pub fn record_rate_limited(&self) {
        self.rate_limited.fetch_add(1, Ordering::Relaxed);
    }

    /// Records an outbound message accepted and persisted for delivery.
    pub fn record_message_accepted(&self) {
        self.messages_accepted.fetch_add(1, Ordering::Relaxed);
    }

    /// Records an outbound message confirmed sent by the modem.
    pub fn record_message_sent(&self) {
        self.messages_sent.fetch_add(1, Ordering::Relaxed);
    }

    /// Records an outbound message that failed delivery.
    pub fn record_message_failed(&self) {
        self.messages_failed.fetch_add(1, Ordering::Relaxed);
    }

    /// Renders all counters, plus the current modem gauges, as Prometheus text.
    pub fn render(&self, modem: &ModemStatusSnapshot) -> String {
        let mut out = String::new();

        counter(
            &mut out,
            "green_relay_auth_failures_total",
            "Authentication attempts rejected (invalid, unknown, or locked out).",
            self.auth_failures.load(Ordering::Relaxed),
        );
        counter(
            &mut out,
            "green_relay_rate_limited_total",
            "Requests rejected by the per-key rate limiter.",
            self.rate_limited.load(Ordering::Relaxed),
        );
        counter(
            &mut out,
            "green_relay_messages_accepted_total",
            "Outbound messages accepted and queued for delivery.",
            self.messages_accepted.load(Ordering::Relaxed),
        );
        counter(
            &mut out,
            "green_relay_messages_sent_total",
            "Outbound messages confirmed sent by the modem.",
            self.messages_sent.load(Ordering::Relaxed),
        );
        counter(
            &mut out,
            "green_relay_messages_failed_total",
            "Outbound messages that failed delivery.",
            self.messages_failed.load(Ordering::Relaxed),
        );

        gauge(
            &mut out,
            "green_relay_modem_serial_connected",
            "Whether the modem serial port is currently connected (1) or not (0).",
            u64::from(modem.serial_connected),
        );
        gauge(
            &mut out,
            "green_relay_modem_registered",
            "Whether the modem is registered to a network (1) or not (0).",
            u64::from(modem.registered),
        );
        if let Some(percent) = modem.signal_percent {
            gauge(
                &mut out,
                "green_relay_modem_signal_percent",
                "Modem signal strength as a percentage (0-100); absent when unknown.",
                u64::from(percent),
            );
        }

        out
    }
}

fn counter(out: &mut String, name: &str, help: &str, value: u64) {
    write_metric(out, name, help, "counter", value);
}

fn gauge(out: &mut String, name: &str, help: &str, value: u64) {
    write_metric(out, name, help, "gauge", value);
}

fn write_metric(out: &mut String, name: &str, help: &str, kind: &str, value: u64) {
    use core::fmt::Write;
    let _ = writeln!(out, "# HELP {name} {help}");
    let _ = writeln!(out, "# TYPE {name} {kind}");
    let _ = writeln!(out, "{name} {value}");
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::health::SimStatus;

    fn snapshot(connected: bool, registered: bool, signal: Option<u8>) -> ModemStatusSnapshot {
        ModemStatusSnapshot {
            serial_connected: connected,
            sim_status: SimStatus::Ready,
            registered,
            responsive: true,
            signal_percent: signal,
            operator: None,
        }
    }

    #[test]
    fn counters_start_at_zero_and_increment() {
        let m = Metrics::new();
        m.record_auth_failure();
        m.record_auth_failure();
        m.record_message_sent();

        let text = m.render(&snapshot(true, true, Some(80)));
        assert!(text.contains("green_relay_auth_failures_total 2"));
        assert!(text.contains("green_relay_messages_sent_total 1"));
        assert!(text.contains("green_relay_messages_failed_total 0"));
    }

    #[test]
    fn each_counter_has_help_and_type_lines() {
        let text = Metrics::new().render(&snapshot(false, false, None));
        for name in [
            "green_relay_auth_failures_total",
            "green_relay_rate_limited_total",
            "green_relay_messages_accepted_total",
            "green_relay_messages_sent_total",
            "green_relay_messages_failed_total",
        ] {
            assert!(
                text.contains(&format!("# HELP {name} ")),
                "missing HELP for {name}"
            );
            assert!(
                text.contains(&format!("# TYPE {name} counter")),
                "missing TYPE for {name}"
            );
        }
    }

    #[test]
    fn modem_gauges_reflect_snapshot() {
        let connected = Metrics::new().render(&snapshot(true, true, Some(42)));
        assert!(connected.contains("green_relay_modem_serial_connected 1"));
        assert!(connected.contains("green_relay_modem_registered 1"));
        assert!(connected.contains("green_relay_modem_signal_percent 42"));

        let down = Metrics::new().render(&snapshot(false, false, None));
        assert!(down.contains("green_relay_modem_serial_connected 0"));
        assert!(down.contains("green_relay_modem_registered 0"));
        // Signal gauge is omitted when unknown.
        assert!(!down.contains("green_relay_modem_signal_percent"));
    }
}
