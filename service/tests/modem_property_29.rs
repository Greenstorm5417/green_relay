//! Property-based test for the reconnect backoff schedule (Property 29).
//!
//! This test lives in its own integration-test crate (separate from
//! `src/modem.rs`) per the spec's test-placement note, and exercises the pure
//! `reconnect_backoff_secs` / `reconnect_backoff_schedule` functions of the
//! `green_relay` library.

use proptest::prelude::*;
use green_relay::modem::{
    RECONNECT_BACKOFF_CAP_SECS, reconnect_backoff_schedule, reconnect_backoff_secs,
};

/// Reference implementation of the expected delay for attempt `n` (1-indexed):
/// `min(2^(n-1), 60)` seconds, computed with overflow-safe arithmetic so the
/// oracle itself never panics for large attempt numbers.
fn expected_delay(attempt: u32) -> u64 {
    let exponent = attempt.saturating_sub(1);
    if exponent >= 6 {
        // 2^6 = 64 already exceeds the 60s cap.
        return RECONNECT_BACKOFF_CAP_SECS;
    }
    (1u64 << exponent).min(RECONNECT_BACKOFF_CAP_SECS)
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 256, ..ProptestConfig::default() })]

    // Feature: sms-microservice, Property 29: Reconnect backoff schedule.
    // For any reopen attempt number `n` from 1 to the configured maximum, the
    // backoff delay equals `min(2^(n-1), 60)` seconds, the schedule is
    // monotonically non-decreasing, never exceeds 60 seconds, and the number
    // of attempts never exceeds the configured maximum.
    //
    // Validates: Requirements 10.1
    #[test]
    fn prop_reconnect_backoff_schedule(max_attempts in 0u32..=64) {
        let schedule = reconnect_backoff_schedule(max_attempts);

        // 1. The number of attempts never exceeds the configured maximum: the
        //    schedule has exactly one delay per attempt 1..=max_attempts.
        prop_assert_eq!(
            schedule.len(),
            max_attempts as usize,
            "schedule must contain exactly one delay per configured attempt"
        );

        let mut previous: u64 = 0;
        for (index, &delay) in schedule.iter().enumerate() {
            let attempt = (index as u32) + 1;

            // 2. The delay for attempt n equals min(2^(n-1), 60).
            prop_assert_eq!(
                delay,
                expected_delay(attempt),
                "delay for attempt {} must equal min(2^(n-1), 60)",
                attempt
            );

            // The schedule entry must agree with the standalone per-attempt
            // function for the same attempt number.
            prop_assert_eq!(
                delay,
                reconnect_backoff_secs(attempt),
                "schedule and per-attempt function must agree for attempt {}",
                attempt
            );

            // 3. The delay never exceeds the 60-second cap.
            prop_assert!(
                delay <= RECONNECT_BACKOFF_CAP_SECS,
                "delay {} for attempt {} must not exceed the 60s cap",
                delay,
                attempt
            );

            // 4. The schedule is monotonically non-decreasing.
            prop_assert!(
                delay >= previous,
                "schedule must be non-decreasing: attempt {} delay {} < previous {}",
                attempt,
                delay,
                previous
            );
            previous = delay;
        }
    }

    // Feature: sms-microservice, Property 29: Reconnect backoff schedule.
    // The per-attempt backoff function itself returns min(2^(n-1), 60) for any
    // attempt number, never exceeding the 60-second cap.
    //
    // Validates: Requirements 10.1
    #[test]
    fn prop_reconnect_backoff_secs_matches_formula(attempt in 0u32..=u32::MAX) {
        let delay = reconnect_backoff_secs(attempt);

        prop_assert_eq!(
            delay,
            expected_delay(attempt),
            "delay for attempt {} must equal min(2^(n-1), 60)",
            attempt
        );
        prop_assert!(
            delay <= RECONNECT_BACKOFF_CAP_SECS,
            "delay {} for attempt {} must not exceed the 60s cap",
            delay,
            attempt
        );
    }
}
