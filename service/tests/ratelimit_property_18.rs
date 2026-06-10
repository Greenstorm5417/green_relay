//! Property-based test for effective rate limit resolution (Property 18).
//!
//! This test lives in its own integration-test crate (separate from
//! `src/ratelimit.rs`) per the spec's test-placement note, and exercises the
//! public `effective_limit` function of the `green_relay` library.

use green_relay::ratelimit::{
    CUSTOM_LIMIT_MAX, CUSTOM_LIMIT_MIN, RateLimitConfigError, effective_limit,
};
use proptest::prelude::*;

proptest! {
    #![proptest_config(ProptestConfig { cases: 256, ..ProptestConfig::default() })]

    // Feature: sms-microservice, Property 18: Effective rate limit resolution.
    // For any custom limit value: if it lies within 1 to 10,000 inclusive,
    // `effective_limit` returns the custom value with no error; otherwise it
    // returns the default limit together with an out-of-range error.
    //
    // Validates: Requirements 4.6, 4.7
    #[test]
    fn prop_effective_limit_resolution(
        // Cover the full u32 range so both in-range and out-of-range custom
        // values (including 0 and the boundaries) are exercised, plus the
        // no-custom case via the Option.
        custom in proptest::option::of(any::<u32>()),
        default in any::<u32>(),
    ) {
        let (limit, err) = effective_limit(custom, default);

        match custom {
            Some(c) if (CUSTOM_LIMIT_MIN..=CUSTOM_LIMIT_MAX).contains(&c) => {
                // In-range custom limit: the custom value is used, no error.
                prop_assert_eq!(limit, c, "in-range custom limit must be used");
                prop_assert!(err.is_none(), "in-range custom limit must not error");
            }
            Some(c) => {
                // Out-of-range custom limit: fall back to default and surface
                // an error naming the offending value (Req 4.7).
                prop_assert_eq!(limit, default, "out-of-range custom must fall back to default");
                prop_assert_eq!(
                    err,
                    Some(RateLimitConfigError { configured: c }),
                    "out-of-range custom must surface an error naming the value"
                );
            }
            None => {
                // No custom override: the default limit is used, no error.
                prop_assert_eq!(limit, default, "absent custom must use the default");
                prop_assert!(err.is_none(), "absent custom must not error");
            }
        }
    }
}
