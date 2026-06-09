//! Property-based test for per-key rate isolation (Property 17).
//!
//! This test lives in its own integration-test crate (separate from
//! `src/ratelimit.rs`) per the spec's test-placement note, and exercises the
//! public `RateLimiter` type of the `sms_micro_service` library.

use std::time::{Duration, Instant};

use proptest::prelude::*;
use sms_micro_service::ratelimit::RateLimiter;

/// Generate two distinct key strings. The second is derived from the first by
/// appending a marker so the two are guaranteed to differ regardless of the
/// generated content.
fn two_distinct_keys() -> impl Strategy<Value = (String, String)> {
    ("[a-zA-Z0-9_]{0,8}", "[a-zA-Z0-9_]{0,8}").prop_map(|(a, b)| {
        let key_a = format!("a:{a}");
        let key_b = format!("b:{b}");
        (key_a, key_b)
    })
}

/// An interleaving of requests across the two keys. `true` means "issue a
/// request for key A", `false` means "issue a request for key B".
fn interleaving() -> impl Strategy<Value = Vec<bool>> {
    proptest::collection::vec(any::<bool>(), 0..=300)
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 256, ..ProptestConfig::default() })]

    // Feature: sms-microservice, Property 17: Per-key rate isolation.
    // For any two distinct API keys, processing any number of requests for one
    // key leaves the other key's accumulated request count unchanged.
    //
    // Validates: Requirements 4.4
    #[test]
    fn prop_per_key_rate_isolation(
        (key_a, key_b) in two_distinct_keys(),
        ops in interleaving(),
        limit in 1u32..=200,
        window_secs in 1u64..=120,
    ) {
        prop_assume!(key_a != key_b);

        let now = Instant::now();
        let window = Duration::from_secs(window_secs);

        // Limiter 1: process the full interleaving of requests across both
        // keys. Key A's activity is freely interleaved with key B's.
        let mut interleaved = RateLimiter::new();
        // Limiter 2 (reference): process ONLY key B's requests, in the same
        // relative order, with no key A activity at all.
        let mut b_only = RateLimiter::new();

        for &is_a in &ops {
            if is_a {
                // Key A activity. This must not affect key B in any way.
                interleaved.check(&key_a, limit, window, now);
            } else {
                // Key B request: issued in both limiters at the same instant
                // and in the same order.
                let d_interleaved = interleaved.check(&key_b, limit, window, now);
                let d_b_only = b_only.check(&key_b, limit, window, now);

                // The decision for key B is identical whether or not key A is
                // active alongside it.
                prop_assert_eq!(
                    d_interleaved,
                    d_b_only,
                    "key B decision diverged due to key A activity"
                );

                // Key B's accumulated count stays in lock-step regardless of
                // key A's interleaved requests.
                prop_assert_eq!(
                    interleaved.count_for(&key_b),
                    b_only.count_for(&key_b),
                    "key B count diverged due to key A activity"
                );
            }
        }

        // After the whole timeline, key B's final accumulated count is exactly
        // what it would have been with no key A activity at all.
        prop_assert_eq!(
            interleaved.count_for(&key_b),
            b_only.count_for(&key_b),
            "final key B count differs from the isolated reference"
        );

        // And the full window state for key B is unchanged by key A's activity.
        prop_assert_eq!(
            interleaved.state_for(&key_b).copied(),
            b_only.state_for(&key_b).copied(),
            "final key B window state differs from the isolated reference"
        );
    }
}
