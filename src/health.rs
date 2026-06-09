//! Health-state derivation and the send deliverability gate.
//!
//! This module holds the pure logic for two related concerns (task 10.1):
//!
//! - [`derive_health`] folds a [`ModemStatusSnapshot`] into an overall
//!   [`ServiceHealth`] verdict (Req 9.3–9.6).
//! - [`deliverability_gate`] decides whether an SMS send may proceed, or
//!   must be rejected with an HTTP 503 and a `Retry-After` header (Req 10.4).
//!
//! Both functions are pure so they can be exhaustively property-tested
//! (Properties 28 and 30) independent of any I/O.

/// SIM card status as reported by `AT+CPIN?`.
///
/// Only the `Ready` state permits SMS operations; every other reported
/// state (awaiting a PIN/PUK, no SIM, an unrecognized reply, etc.) is
/// treated as not ready for the purposes of health and deliverability
/// (Req 9.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SimStatus {
    /// The SIM reported `READY` and is usable.
    Ready,
    /// The SIM reported a state other than `READY` (e.g. `SIM PIN`,
    /// `SIM PUK`, not inserted).
    NotReady,
    /// The SIM status could not be determined (no/!malformed `AT+CPIN?`
    /// response).
    Unknown,
}

impl SimStatus {
    /// Whether the SIM is in the `READY` state.
    pub fn is_ready(self) -> bool {
        matches!(self, SimStatus::Ready)
    }
}

/// Overall service health derived from a modem status snapshot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServiceHealth {
    /// SIM ready, serial connected, modem responsive, and registered to a
    /// network (Req 9.6).
    Healthy,
    /// Operational but not registered to a network, while the SIM is ready
    /// and the modem is reachable (Req 9.5).
    Degraded,
    /// Serial disconnected, modem unresponsive, or SIM not ready
    /// (Req 9.3, 9.4).
    Unhealthy,
}

/// A point-in-time view of the modem used to derive health and gate sends.
///
/// Mirrors the snapshot described in `design.md`. The `signal_percent` and
/// `operator` fields are surfaced for the status endpoint; neither affects
/// the health verdict or the deliverability decision, so they are optional
/// and ignored by the functions in this module.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModemStatusSnapshot {
    /// Whether the serial port is currently connected (Req 9.4).
    pub serial_connected: bool,
    /// SIM card status from `AT+CPIN?` (Req 9.3).
    pub sim_status: SimStatus,
    /// Whether the modem is registered to a network, from `AT+CREG?`
    /// (Req 9.5).
    pub registered: bool,
    /// Whether the modem returned a valid response to an AT exchange within
    /// 5 seconds across up to 3 attempts (Req 9.4).
    pub responsive: bool,
    /// Signal quality as a percentage (0..=100) from `AT+CSQ`, when known.
    pub signal_percent: Option<u8>,
    /// Current operator from `AT+COPS?`, when known.
    pub operator: Option<String>,
}

/// Derive the overall [`ServiceHealth`] from a modem status snapshot.
///
/// The verdict is, in priority order (Req 9.3–9.6):
///
/// 1. **Unhealthy** if the serial port is disconnected, OR the modem is
///    unresponsive, OR the SIM status is not `READY`.
/// 2. Otherwise **Degraded** if the modem is not registered to a network.
/// 3. Otherwise **Healthy**.
pub fn derive_health(s: &ModemStatusSnapshot) -> ServiceHealth {
    if !s.serial_connected || !s.responsive || !s.sim_status.is_ready() {
        ServiceHealth::Unhealthy
    } else if !s.registered {
        ServiceHealth::Degraded
    } else {
        ServiceHealth::Healthy
    }
}

/// Default `Retry-After` value (in seconds) for a gated send.
///
/// Matches the default network-registration retry interval (Req 10.5),
/// giving clients a sensible hint for when to retry a request rejected by
/// the deliverability gate.
pub const DEFAULT_RETRY_AFTER_SECS: u64 = 30;

/// Outcome of the pure send deliverability gate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeliverabilityOutcome {
    /// All preconditions are met; the send may proceed.
    Deliverable,
    /// A precondition is unmet; the request must be rejected with an HTTP
    /// 503 status and a `Retry-After` header carrying `retry_after_secs`.
    Rejected {
        /// Number of seconds to advertise in the `Retry-After` header.
        retry_after_secs: u64,
    },
}

/// Decide whether an SMS send may proceed (Req 10.4).
///
/// A send is rejected with a 503-and-`Retry-After` outcome when any condition
/// that prevents immediate delivery is present: the serial port is
/// unavailable, OR the SIM is not ready, OR the modem is not registered to a
/// network. Otherwise the send is [`Deliverable`](DeliverabilityOutcome::Deliverable).
///
/// `retry_after_secs` is the value to advertise in the `Retry-After` header
/// when the request is rejected; callers typically pass
/// [`DEFAULT_RETRY_AFTER_SECS`] or a configured interval.
///
/// Note that, unlike [`derive_health`], modem responsiveness does not affect
/// the gate: the gate concerns only the preconditions for delivery named in
/// Req 10.4.
pub fn deliverability_gate(
    s: &ModemStatusSnapshot,
    retry_after_secs: u64,
) -> DeliverabilityOutcome {
    if !s.serial_connected || !s.sim_status.is_ready() || !s.registered {
        DeliverabilityOutcome::Rejected { retry_after_secs }
    } else {
        DeliverabilityOutcome::Deliverable
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Baseline healthy snapshot for test mutations.
    fn healthy_snapshot() -> ModemStatusSnapshot {
        ModemStatusSnapshot {
            serial_connected: true,
            sim_status: SimStatus::Ready,
            registered: true,
            responsive: true,
            signal_percent: Some(80),
            operator: Some("Carrier".to_string()),
        }
    }

    #[test]
    fn healthy_when_all_conditions_met() {
        assert_eq!(derive_health(&healthy_snapshot()), ServiceHealth::Healthy);
    }

    #[test]
    fn degraded_when_not_registered_but_otherwise_ok() {
        let mut s = healthy_snapshot();
        s.registered = false;
        assert_eq!(derive_health(&s), ServiceHealth::Degraded);
    }

    #[test]
    fn unhealthy_when_serial_disconnected() {
        let mut s = healthy_snapshot();
        s.serial_connected = false;
        assert_eq!(derive_health(&s), ServiceHealth::Unhealthy);
    }

    #[test]
    fn unhealthy_when_unresponsive() {
        let mut s = healthy_snapshot();
        s.responsive = false;
        assert_eq!(derive_health(&s), ServiceHealth::Unhealthy);
    }

    #[test]
    fn unhealthy_when_sim_not_ready() {
        for sim in [SimStatus::NotReady, SimStatus::Unknown] {
            let mut s = healthy_snapshot();
            s.sim_status = sim;
            assert_eq!(derive_health(&s), ServiceHealth::Unhealthy);
        }
    }

    #[test]
    fn unhealthy_takes_priority_over_degraded() {
        let mut s = healthy_snapshot();
        s.registered = false;
        s.serial_connected = false;
        assert_eq!(derive_health(&s), ServiceHealth::Unhealthy);
    }

    #[test]
    fn gate_allows_when_all_preconditions_met() {
        assert_eq!(
            deliverability_gate(&healthy_snapshot(), DEFAULT_RETRY_AFTER_SECS),
            DeliverabilityOutcome::Deliverable
        );
    }

    #[test]
    fn gate_rejects_when_serial_unavailable() {
        let mut s = healthy_snapshot();
        s.serial_connected = false;
        assert_eq!(
            deliverability_gate(&s, 30),
            DeliverabilityOutcome::Rejected {
                retry_after_secs: 30
            }
        );
    }

    #[test]
    fn gate_rejects_when_sim_not_ready() {
        let mut s = healthy_snapshot();
        s.sim_status = SimStatus::NotReady;
        assert_eq!(
            deliverability_gate(&s, 15),
            DeliverabilityOutcome::Rejected {
                retry_after_secs: 15
            }
        );
    }

    #[test]
    fn gate_rejects_when_not_registered() {
        let mut s = healthy_snapshot();
        s.registered = false;
        assert_eq!(
            deliverability_gate(&s, 42),
            DeliverabilityOutcome::Rejected {
                retry_after_secs: 42
            }
        );
    }

    #[test]
    fn gate_ignores_responsiveness() {
        let mut s = healthy_snapshot();
        s.responsive = false;
        assert_eq!(
            deliverability_gate(&s, DEFAULT_RETRY_AFTER_SECS),
            DeliverabilityOutcome::Deliverable
        );
    }
}
