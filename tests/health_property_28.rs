//! Property-based test for health-state derivation (Property 28).
//!
//! This test lives in its own integration-test crate (separate from
//! `src/health.rs`) per the spec's test-placement note, and exercises the
//! public `derive_health` function of the `sms_micro_service` library.

use proptest::prelude::*;
use sms_micro_service::health::{derive_health, ModemStatusSnapshot, ServiceHealth, SimStatus};

/// Strategy over all three SIM states so the "not READY" branch is covered by
/// both `NotReady` and `Unknown`.
fn sim_status() -> impl Strategy<Value = SimStatus> {
    prop_oneof![
        Just(SimStatus::Ready),
        Just(SimStatus::NotReady),
        Just(SimStatus::Unknown),
    ]
}

/// Generate an arbitrary modem status snapshot. The boolean axes and the SIM
/// state range over every combination; the optional signal/operator fields are
/// irrelevant to health derivation but are varied to confirm they have no
/// effect on the verdict.
fn snapshot() -> impl Strategy<Value = ModemStatusSnapshot> {
    (
        any::<bool>(),
        sim_status(),
        any::<bool>(),
        any::<bool>(),
        any::<Option<u8>>(),
        any::<Option<String>>(),
    )
        .prop_map(
            |(serial_connected, sim_status, registered, responsive, signal_percent, operator)| {
                ModemStatusSnapshot {
                    serial_connected,
                    sim_status,
                    registered,
                    responsive,
                    signal_percent,
                    operator,
                }
            },
        )
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 1000, ..ProptestConfig::default() })]

    // Feature: sms-microservice, Property 28: Health-state derivation.
    // For any modem status snapshot, `derive_health` returns Unhealthy if the
    // serial port is disconnected OR the modem is unresponsive OR the SIM
    // status is not READY; otherwise it returns Degraded if the modem is not
    // registered to a network; otherwise it returns Healthy.
    //
    // Validates: Requirements 9.3, 9.4, 9.5, 9.6
    #[test]
    fn prop_health_state_derivation(s in snapshot()) {
        // Independent oracle, written separately from the implementation so the
        // property checks the implementation against the specification rather
        // than against itself.
        let sim_ready = matches!(s.sim_status, SimStatus::Ready);
        let expected = if !s.serial_connected || !s.responsive || !sim_ready {
            ServiceHealth::Unhealthy
        } else if !s.registered {
            ServiceHealth::Degraded
        } else {
            ServiceHealth::Healthy
        };

        prop_assert_eq!(
            derive_health(&s),
            expected,
            "derive_health({:?}) disagreed with the specified derivation",
            s
        );
    }
}
