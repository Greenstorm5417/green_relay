//! Property-based test for the rate-limiter decision semantics (Property 15).
//!
//! This test lives in its own integration-test crate (separate from
//! `src/ratelimit.rs`) per the spec's test-placement note, and exercises the
//! pure `decide` transition function of the `green_relay` library.

use std::time::{Duration, Instant};

use green_relay::ratelimit::{RateDecision, WindowState, decide};
use proptest::prelude::*;

proptest! {
    #![proptest_config(ProptestConfig { cases: 256, ..ProptestConfig::default() })]

    // Feature: sms-microservice, Property 15: Rate-limiter decision semantics.
    // For any window state, configured limit, and window duration: if the
    // window has elapsed the decision resets the window and allows the
    // request; otherwise while the count is below the limit the request is
    // allowed and the count increases by exactly one; and when the count is at
    // or above the limit the request is rejected and the count is left
    // unchanged.
    //
    // Validates: Requirements 4.1, 4.2, 4.5
    #[test]
    fn prop_decide_semantics(
        limit in 1u32..=10_000,
        window_secs in 1u64..=3_600,
        initial_count in 0u32..=10_000,
        elapsed_secs in 0u64..=7_200,
        elapsed_nanos in 0u64..1_000_000_000,
    ) {
        let window = Duration::from_secs(window_secs);

        // Build a window start and a `now` that is `elapsed` after it, using a
        // fixed base so the two instants are comparable. `Instant` values
        // cannot be constructed arbitrarily, so we derive both from a single
        // base instant.
        let base = Instant::now();
        let window_start = base;
        let elapsed = Duration::new(elapsed_secs, elapsed_nanos as u32);
        let now = base + elapsed;

        let mut state = WindowState {
            count: initial_count,
            window_start,
        };

        let decision = decide(&mut state, limit, window, now);

        if elapsed >= window {
            // Window elapsed (Req 4.5): the window resets to start at `now`,
            // this request is counted as the first of the new window, and the
            // request is allowed (Req 4.1).
            prop_assert_eq!(
                decision,
                RateDecision::Allow { remaining: limit.saturating_sub(1) },
                "elapsed window must allow with remaining = limit - 1"
            );
            prop_assert_eq!(state.count, 1, "reset window counts this request as the first");
            prop_assert_eq!(
                state.window_start,
                now,
                "reset window must start at the current instant"
            );
        } else if initial_count < limit {
            // Under the limit (Req 4.1): allow and increase the count by
            // exactly one; remaining reflects the new count.
            prop_assert_eq!(
                decision,
                RateDecision::Allow { remaining: limit - (initial_count + 1) },
                "under-limit request must be allowed with remaining = limit - new count"
            );
            prop_assert_eq!(
                state.count,
                initial_count + 1,
                "under-limit request must increment the count by exactly one"
            );
            prop_assert_eq!(
                state.window_start,
                window_start,
                "an in-window decision must not move the window start"
            );
        } else {
            // At or over the limit (Req 4.2): reject and leave the count
            // unchanged.
            prop_assert!(
                matches!(decision, RateDecision::Reject { .. }),
                "at-or-over-limit request must be rejected"
            );
            prop_assert_eq!(
                state.count,
                initial_count,
                "a rejected request must leave the count unchanged"
            );
            prop_assert_eq!(
                state.window_start,
                window_start,
                "a rejected request must not move the window start"
            );
        }
    }
}
