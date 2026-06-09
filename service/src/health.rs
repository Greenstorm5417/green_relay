
/// Represents the status of the SIM card.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SimStatus {
    
    Ready,
    
    NotReady,
    
    Unknown,
}

impl SimStatus {
    
    /// Returns true if the SIM status is ready.
    pub fn is_ready(self) -> bool {
        matches!(self, SimStatus::Ready)
    }
}

/// Represents the overall health status of the service.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServiceHealth {
    
    Healthy,
    
    Degraded,
    
    Unhealthy,
}

/// A snapshot of the current modem status.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModemStatusSnapshot {
    
    /// Indicates if the serial connection is active.
    pub serial_connected: bool,
    
    /// The status of the SIM card.
    pub sim_status: SimStatus,
    
    /// Indicates if the modem is registered on a network.
    pub registered: bool,
    
    /// Indicates if the modem is responsive to commands.
    pub responsive: bool,
    
    /// The signal strength percentage.
    pub signal_percent: Option<u8>,
    
    /// The network operator name.
    pub operator: Option<String>,
}

/// Derives the overall service health from a modem status snapshot.
pub fn derive_health(s: &ModemStatusSnapshot) -> ServiceHealth {
    if !s.serial_connected || !s.responsive || !s.sim_status.is_ready() {
        ServiceHealth::Unhealthy
    } else if !s.registered {
        ServiceHealth::Degraded
    } else {
        ServiceHealth::Healthy
    }
}

/// The default number of seconds to wait before retrying.
pub const DEFAULT_RETRY_AFTER_SECS: u64 = 30;

/// The outcome of checking message deliverability.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeliverabilityOutcome {
    
    Deliverable,
    
    Rejected {
        
        retry_after_secs: u64,
    },
}

/// Determines message deliverability based on modem status.
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
