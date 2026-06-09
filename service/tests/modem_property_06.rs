//! Property-based test for send-reference parsing round-trips (Property 6).
//!
//! This test lives in its own integration-test crate (separate from
//! `src/modem.rs`) per the spec's test-placement note, and exercises the
//! public `format_cmgs_response`, `parse_cmgs_reference`, and
//! `parse_send_outcome` functions of the `green_relay` library.

use proptest::prelude::*;
use green_relay::models::MessageStatus;
use green_relay::modem::{format_cmgs_response, parse_cmgs_reference, parse_send_outcome};

proptest! {
    #![proptest_config(ProptestConfig { cases: 1000, ..ProptestConfig::default() })]

    // Feature: sms-microservice, Property 6: Send-reference parsing round-trips.
    // For any non-negative integer reference, formatting a `+CMGS: <ref>`
    // response and then parsing it recovers the same reference and maps the
    // outcome to status `sent`.
    //
    // Validates: Requirements 1.4
    #[test]
    fn prop_send_reference_parsing_round_trips(reference in any::<u32>()) {
        // Formatting then parsing the intermediate `+CMGS: <ref>` line recovers
        // exactly the same reference.
        let line = format_cmgs_response(reference);
        prop_assert_eq!(
            parse_cmgs_reference(&line),
            Some(reference),
            "parse_cmgs_reference({:?}) did not round-trip back to {}",
            line,
            reference
        );

        // A full send exchange (`+CMGS: <ref>` acknowledged with `OK`) maps the
        // outcome to status `sent` and carries the recovered reference.
        let ok = "OK";
        let lines = [line.as_str(), ok];
        let outcome = parse_send_outcome(&lines);

        prop_assert_eq!(
            outcome.status,
            MessageStatus::Sent,
            "send outcome for reference {} was not Sent: {:?}",
            reference,
            outcome
        );
        prop_assert_eq!(
            outcome.reference,
            Some(reference),
            "send outcome did not recover reference {}: {:?}",
            reference,
            outcome
        );
        prop_assert_eq!(outcome.error_code, None);
    }
}
