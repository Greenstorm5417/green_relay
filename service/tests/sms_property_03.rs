//! Property-based test for missing-field validation (Property 3).
//!
//! This test lives in its own integration-test crate (separate from
//! `src/sms.rs`) per the spec's test-placement note, and exercises the public
//! `check_required_fields` function of the `green_relay` library.

use green_relay::sms::{ValidationError, check_required_fields};
use proptest::prelude::*;

/// Independent oracle: the set of field names that are absent, in the stable
/// order the implementation is specified to use (`to` before `body`).
///
/// Written separately from the implementation so the property checks the
/// implementation against the specification rather than against itself.
fn expected_missing(to: &Option<String>, body: &Option<String>) -> Vec<String> {
    let mut missing = Vec::new();
    if to.is_none() {
        missing.push("to".to_string());
    }
    if body.is_none() {
        missing.push("body".to_string());
    }
    missing
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 1000, ..ProptestConfig::default() })]

    // Feature: sms-microservice, Property 3: Missing-field errors name exactly
    // the missing fields. For any combination of present/absent `to` and
    // `body` fields in a send request, the resulting validation error names
    // exactly the fields that are missing (and validation otherwise passes the
    // field-presence check).
    //
    // Validates: Requirements 1.6
    #[test]
    fn prop_missing_field_errors_name_exactly_missing_fields(
        to in proptest::option::of(any::<String>()),
        body in proptest::option::of(any::<String>()),
    ) {
        let result = check_required_fields(to.as_deref(), body.as_deref());
        let expected = expected_missing(&to, &body);

        if expected.is_empty() {
            // Both fields present: the field-presence check passes.
            prop_assert_eq!(
                result,
                Ok(()),
                "both fields present but check_required_fields rejected them"
            );
        } else {
            // At least one field absent: the error must name exactly the
            // missing fields, in order, and nothing else.
            match result {
                Err(ValidationError::MissingFields(named)) => {
                    prop_assert_eq!(
                        &named,
                        &expected,
                        "missing-field error named {:?} but exactly {:?} were absent",
                        named,
                        expected
                    );
                }
                other => prop_assert!(
                    false,
                    "expected MissingFields({:?}) but got {:?}",
                    expected,
                    other
                ),
            }
        }
    }
}
