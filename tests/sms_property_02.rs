//! Property-based test for message-body length validation (Property 2).
//!
//! This test lives in its own integration-test crate (separate from
//! `src/sms.rs`) per the spec's test-placement note, and exercises the public
//! `validate_body` function of the `sms_micro_service` library.

use proptest::prelude::*;
use sms_micro_service::sms::{MAX_BODY_CHARS, validate_body};

/// Independent oracle for the body-length rule, written separately from the
/// implementation so the property checks the implementation against the
/// specification rather than against itself.
///
/// A body is valid iff its character length (counting Unicode scalar values,
/// not bytes) is between 1 and `MAX_BODY_CHARS` (1,530) inclusive.
fn is_valid_body(s: &str) -> bool {
    let len = s.chars().count();
    (1..=MAX_BODY_CHARS).contains(&len)
}

/// Generate strings whose character length lands near the interesting
/// boundaries (empty, 1, just under / over the maximum) in addition to broad
/// arbitrary coverage, so the bounds get exercised thoroughly.
fn boundary_body() -> impl Strategy<Value = String> {
    prop_oneof![
        // Lengths clustered around the lower bound (0..=3 chars).
        proptest::collection::vec(any::<char>(), 0..=3).prop_map(|v| v.into_iter().collect()),
        // Lengths clustered around the upper bound (1528..=1533 chars).
        proptest::collection::vec(any::<char>(), (MAX_BODY_CHARS - 2)..=(MAX_BODY_CHARS + 3))
            .prop_map(|v| v.into_iter().collect()),
        // Multibyte characters around the upper bound to confirm character
        // (not byte) counting at the boundary.
        ((MAX_BODY_CHARS - 2)..=(MAX_BODY_CHARS + 3)).prop_map(|n| "é".repeat(n)),
    ]
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 1000, ..ProptestConfig::default() })]

    // Feature: sms-microservice, Property 2: Body length validation respects
    // bounds. For any string, `validate_body` accepts it if and only if its
    // character length is between 1 and 1,530 inclusive (counting characters,
    // not bytes).
    //
    // Validates: Requirements 1.1, 1.10
    #[test]
    fn prop_body_validation_respects_bounds(
        s in prop_oneof![
            boundary_body(),
            any::<String>(),
        ],
    ) {
        let accepted = validate_body(&s).is_ok();
        let expected = is_valid_body(&s);

        prop_assert_eq!(
            accepted,
            expected,
            "validate_body(<{} chars>) returned accepted={}, but the length rule expects {}",
            s.chars().count(),
            accepted,
            expected
        );
    }
}
