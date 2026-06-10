//! Property-based test for the send deliverability gate (Property 30).
//!
//! This test lives in its own integration-test crate (separate from
//! `src/health.rs`) per the spec's test-placement note, and exercises the
//! public `deliverability_gate` function of the `green_relay` library.

use green_relay::health::{
    DeliverabilityOutcome, ModemStatusSnapshot, SimStatus, deliverability_gate,
};
use proptest::prelude::*;

/// Generate an arbitrary SIM status across all variants so that the `Ready`
/// and not-ready (`NotReady`/`Unknown`) paths are both exercised.
fn any_sim_status() -> impl Strategy<Value = SimStatus> {
    prop_oneof![
        Just(SimStatus::Ready),
        Just(SimStatus::NotReady),
        Just(SimStatus::Unknown),
    ]
}

/// Generate an arbitrary operator string (or `None`); this field is not part
/// of the gate's preconditions, so it ranges freely.
fn any_operator() -> impl Strategy<Value = Option<String>> {
    proptest::option::of("[a-zA-Z0-9 ]{0,12}")
}

/// Generate an arbitrary deliverability snapshot. Every field that could
/// influence (or deliberately not influence) the gate is randomized so the
/// property covers the full input space.
fn any_snapshot() -> impl Strategy<Value = ModemStatusSnapshot> {
    (
        any::<bool>(),
        any_sim_status(),
        any::<bool>(),
        any::<bool>(),
        proptest::option::of(0u8..=100),
        any_operator(),
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
    #![proptest_config(ProptestConfig { cases: 256, ..ProptestConfig::default() })]

    // Feature: sms-microservice, Property 30: Send deliverability gate.
    // For any deliverability snapshot, a send request is rejected with a
    // 503-and-`Retry-After` outcome if the serial port is unavailable OR the
    // SIM is not ready OR the modem is not registered to a network, and is
    // otherwise accepted.
    //
    // Validates: Requirements 10.4
    #[test]
    fn prop_send_deliverability_gate(
        snapshot in any_snapshot(),
        retry_after_secs in any::<u64>(),
    ) {
        let outcome = deliverability_gate(&snapshot, retry_after_secs);

        // The gate must reject precisely when any precondition is unmet.
        let should_reject = !snapshot.serial_connected
            || !snapshot.sim_status.is_ready()
            || !snapshot.registered;

        match outcome {
            DeliverabilityOutcome::Rejected { retry_after_secs: got } => {
                prop_assert!(
                    should_reject,
                    "gate rejected a snapshot whose preconditions are all met: {:?}",
                    snapshot
                );
                // A rejection must carry through the supplied Retry-After value
                // (the 503-and-Retry-After outcome).
                prop_assert_eq!(
                    got,
                    retry_after_secs,
                    "rejection must advertise the supplied Retry-After value"
                );
            }
            DeliverabilityOutcome::Deliverable => {
                prop_assert!(
                    !should_reject,
                    "gate accepted a snapshot with an unmet precondition: {:?}",
                    snapshot
                );
            }
        }
    }
}
