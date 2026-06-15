//! Property-based test for per-key rate isolation (Property 17).
//!
//! This test lives in its own integration-test crate (separate from
//! `src/ratelimit.rs`) per the spec's test-placement note, and exercises the
//! public `RateLimiter` type of the `green_relay` library. The limiter is now
//! async (moka-backed), so each operation is driven on a small current-thread
//! runtime and the resulting values are asserted synchronously.

use std::time::{Duration, Instant};

use green_relay::ratelimit::RateLimiter;
use proptest::prelude::*;
use tokio::runtime::Builder;

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

        let rt = Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("build current-thread runtime");

        let now = Instant::now();
        let window = Duration::from_secs(window_secs);

        // Limiter 1: process the full interleaving of requests across both
        // keys. Key A's activity is freely interleaved with key B's.
        let interleaved = RateLimiter::new();
        // Limiter 2 (reference): process ONLY key B's requests, in the same
        // relative order, with no key A activity at all.
        let b_only = RateLimiter::new();

        for &is_a in &ops {
            if is_a {
                // Key A activity. This must not affect key B in any way.
                rt.block_on(interleaved.check(&key_a, limit, window, now));
            } else {
                // Key B request: issued in both limiters at the same instant
                // and in the same order.
                let d_interleaved = rt.block_on(interleaved.check(&key_b, limit, window, now));
                let d_b_only = rt.block_on(b_only.check(&key_b, limit, window, now));

                // The decision for key B is identical whether or not key A is
                // active alongside it.
                prop_assert_eq!(
                    d_interleaved,
                    d_b_only,
                    "key B decision diverged due to key A activity"
                );

                // Key B's accumulated count stays in lock-step regardless of
                // key A's interleaved requests.
                let count_interleaved = rt.block_on(interleaved.count_for(&key_b));
                let count_b_only = rt.block_on(b_only.count_for(&key_b));
                prop_assert_eq!(
                    count_interleaved,
                    count_b_only,
                    "key B count diverged due to key A activity"
                );
            }
        }

        // After the whole timeline, key B's final accumulated count is exactly
        // what it would have been with no key A activity at all.
        let final_interleaved = rt.block_on(interleaved.count_for(&key_b));
        let final_b_only = rt.block_on(b_only.count_for(&key_b));
        prop_assert_eq!(
            final_interleaved,
            final_b_only,
            "final key B count differs from the isolated reference"
        );
    }
}
