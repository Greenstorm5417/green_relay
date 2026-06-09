//! Property-based test for the rate-limiter `Retry-After` bound (Property 16).
//!
//! This test lives in its own integration-test crate (separate from
//! `src/ratelimit.rs`) per the spec's test-placement note, and exercises the
//! public `decide` function of the `sms_micro_service` library through its
//! `ratelimit` module.

use std::time::{Duration, Instant};

use proptest::prelude::*;
use sms_micro_service::ratelimit::{RateDecision, WindowState, decide};

/// Generate a window length in whole seconds within a realistic range. The
/// window is always at least 1 second so the upper bound on `Retry-After`
/// (the window length) is never zero.
fn any_window_secs() -> impl Strategy<Value = u64> {
    1u64..=3_600
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 256, ..ProptestConfig::default() })]

    // Feature: sms-microservice, Property 16: Retry-After is within the window
    // bound. For any rejected rate-limit decision, the `Retry-After` value is
    // an integer between 1 and the configured window length in seconds,
    // inclusive.
    //
    // Validates: Requirements 4.3
    #[test]
    fn prop_retry_after_within_window_bound(
        window_secs in any_window_secs(),
        // Limit at which the request will be rejected. Use a count already at
        // or above this limit to force a rejection inside the window.
        limit in 0u32..=10_000,
        extra_count in 0u32..=50,
        // Elapsed fraction of the window, expressed in milliseconds from the
        // window start, kept strictly below the window length so the window
        // has not yet elapsed (an elapsed window would reset and allow).
        elapsed_ms in 0u64..u64::MAX,
    ) {
        let window = Duration::from_secs(window_secs);
        let window_ms = window_secs.saturating_mul(1_000).max(1);
        // Constrain elapsed strictly inside the window [0, window_ms - 1] so
        // the decision rejects rather than resetting the window.
        let elapsed = Duration::from_millis(elapsed_ms % window_ms);

        let start = Instant::now();
        let now = start + elapsed;

        // Seed the state at or above the limit so `decide` must reject. The
        // count is irrelevant to the bound, only that it is >= limit.
        let mut state = WindowState {
            count: limit.saturating_add(extra_count),
            window_start: start,
        };

        let decision = decide(&mut state, limit, window, now);

        match decision {
            RateDecision::Reject { retry_after_secs } => {
                prop_assert!(
                    retry_after_secs >= 1,
                    "Retry-After {} must be at least 1 second",
                    retry_after_secs
                );
                prop_assert!(
                    retry_after_secs <= window_secs,
                    "Retry-After {} must not exceed the window length {} seconds",
                    retry_after_secs,
                    window_secs
                );
            }
            RateDecision::Allow { .. } => {
                // With a count seeded at or above the limit and a non-elapsed
                // window, the only correct outcome is a rejection.
                prop_assert!(
                    false,
                    "expected a rejection for count >= limit within the window, got Allow"
                );
            }
        }
    }
}
