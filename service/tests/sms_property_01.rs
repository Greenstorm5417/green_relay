//! Property-based test for E.164 phone-number validation (Property 1).
//!
//! This test lives in its own integration-test crate (separate from
//! `src/sms.rs`) per the spec's test-placement note, and exercises the public
//! `validate_e164` function of the `sms_micro_service` library.

use proptest::prelude::*;
use sms_micro_service::sms::validate_e164;

/// Independent oracle for the E.164 grammar, written separately from the
/// implementation so the property checks the implementation against the
/// specification rather than against itself.
///
/// A string is in E.164 format iff it is a leading `+` followed by 7 to 15
/// ASCII decimal digits and nothing else.
fn is_valid_e164(s: &str) -> bool {
    match s.strip_prefix('+') {
        Some(rest) => {
            let all_digits = !rest.is_empty() && rest.chars().all(|c| c.is_ascii_digit());
            all_digits && (7..=15).contains(&rest.chars().count())
        }
        None => false,
    }
}

/// Generate a well-formed E.164 number: `+` followed by 7..=15 decimal digits.
fn valid_e164() -> impl Strategy<Value = String> {
    "\\+[0-9]{7,15}".prop_map(|s| s.to_string())
}

/// Generate strings that are close to valid but likely violate the grammar in
/// one dimension (wrong digit count, missing `+`, embedded non-digit), so the
/// boundaries get good coverage in addition to the broad `any::<String>()`
/// arm.
fn near_miss_e164() -> impl Strategy<Value = String> {
    prop_oneof![
        // Too few digits (0..=6) after the '+'.
        "\\+[0-9]{0,6}".prop_map(|s| s.to_string()),
        // Too many digits (16..=20) after the '+'.
        "\\+[0-9]{16,20}".prop_map(|s| s.to_string()),
        // Right length but no leading '+'.
        "[0-9]{7,15}".prop_map(|s| s.to_string()),
        // Leading '+' with a non-digit character mixed in.
        "\\+[0-9a-zA-Z !#]{7,15}".prop_map(|s| s.to_string()),
        // Leading '+' with a leading/trailing space among digits.
        "\\+[0-9]{3} [0-9]{4}".prop_map(|s| s.to_string()),
    ]
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 1000, ..ProptestConfig::default() })]

    // Feature: sms-microservice, Property 1: E.164 validation matches the
    // grammar exactly. For any input string, `validate_e164` accepts it if and
    // only if it consists of a leading `+` followed by 7 to 15 decimal digits
    // and no other characters.
    //
    // Validates: Requirements 1.7
    #[test]
    fn prop_e164_validation_matches_grammar(
        s in prop_oneof![
            // Bias generation toward all three interesting regions: clearly
            // valid numbers, near-miss strings, and fully arbitrary strings.
            valid_e164(),
            near_miss_e164(),
            any::<String>(),
        ],
    ) {
        let accepted = validate_e164(&s).is_ok();
        let expected = is_valid_e164(&s);

        prop_assert_eq!(
            accepted,
            expected,
            "validate_e164({:?}) returned accepted={}, but the E.164 grammar expects {}",
            s,
            accepted,
            expected
        );
    }
}
