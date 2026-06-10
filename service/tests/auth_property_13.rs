//! Property-based test for the pre-lookup API-key guard (Property 13).
//!
//! This test lives in its own integration-test crate (separate from
//! `src/auth.rs`) per the spec's test-placement note, and exercises the public
//! `passes_guard` function of the `green_relay` library.
//!
//! The guard is the pre-lookup gate from Req 3.7: a presented key is only
//! acceptable for a store lookup when it is non-empty and no longer than
//! `MAX_KEY_LEN` (256) characters. The property checks the implementation
//! against an independent oracle expressed directly from that rule, so it is
//! the boundary of the *character* count (not byte length) that matters.

use green_relay::auth::{MAX_KEY_LEN, passes_guard};
use proptest::prelude::*;

/// Independent oracle for the guard, written separately from the
/// implementation: a key is rejected (no lookup) if and only if its character
/// length is 0 or greater than `MAX_KEY_LEN`. Equivalently it is accepted iff
/// its length is within `1..=MAX_KEY_LEN`.
fn guard_should_accept(s: &str) -> bool {
    let len = s.chars().count();
    (1..=MAX_KEY_LEN).contains(&len)
}

/// Generate strings whose character length clusters around the interesting
/// boundaries: empty, just inside the range, exactly at the limit, and just
/// over the limit, in addition to broad arbitrary strings. Using a repeated
/// multi-byte character also guards against any char-vs-byte confusion.
fn boundary_keys() -> impl Strategy<Value = String> {
    prop_oneof![
        // Empty string: must be rejected.
        Just(String::new()),
        // Exactly one character: smallest accepted length.
        Just("k".to_string()),
        // Exactly at the limit: still accepted.
        Just("k".repeat(MAX_KEY_LEN)),
        // One past the limit: rejected.
        Just("k".repeat(MAX_KEY_LEN + 1)),
        // Multi-byte chars exactly at the limit (256 chars, 512 bytes):
        // accepted, since the guard counts characters, not bytes.
        Just("é".repeat(MAX_KEY_LEN)),
        // Multi-byte chars one past the limit: rejected.
        Just("é".repeat(MAX_KEY_LEN + 1)),
        // Lengths sampled across and beyond the boundary.
        (0usize..=(MAX_KEY_LEN + 5)).prop_map(|n| "x".repeat(n)),
    ]
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 1000, ..ProptestConfig::default() })]

    // Feature: sms-microservice, Property 13: Pre-lookup key guard rejects
    // out-of-bounds keys. For any presented key string, the pre-lookup guard
    // rejects it (without performing a key lookup) if and only if its length
    // is 0 or greater than 256.
    //
    // Validates: Requirements 3.7
    #[test]
    fn prop_guard_rejects_out_of_bounds_keys(
        s in prop_oneof![
            boundary_keys(),
            any::<String>(),
        ],
    ) {
        let accepted = passes_guard(&s);
        let expected = guard_should_accept(&s);

        prop_assert_eq!(
            accepted,
            expected,
            "passes_guard(len={} chars) returned accepted={}, but the guard rule expects {}",
            s.chars().count(),
            accepted,
            expected
        );
    }
}
