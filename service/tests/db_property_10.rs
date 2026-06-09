//! Property-based test for the storage-capacity warning threshold (Property 10).
//!
//! This test lives in its own integration-test crate (separate from
//! `src/db.rs`) per the spec's test-placement note, and exercises the public
//! pure `storage_capacity_warn` function of the `green_relay` library.

use proptest::prelude::*;
use green_relay::db::storage_capacity_warn;

proptest! {
    #![proptest_config(ProptestConfig { cases: 256, ..ProptestConfig::default() })]

    // Feature: sms-microservice, Property 10: Storage-capacity warning
    // threshold. For any used and total storage counts with total greater than
    // zero, the warn decision is true if and only if used is at least 90% of
    // total.
    //
    // Validates: Requirements 2.6
    #[test]
    fn prop_storage_capacity_warn_threshold(
        total in 1u32..=u32::MAX,
        used in 0u32..=u32::MAX,
    ) {
        // Reference predicate computed with exact 64-bit integer arithmetic so
        // it is free of floating-point rounding error: warn iff used/total >=
        // 0.90, i.e. used*10 >= total*9. Quantified over total > 0.
        let expected = u64::from(used) * 10 >= u64::from(total) * 9;

        prop_assert_eq!(
            storage_capacity_warn(used, total),
            expected,
            "warn decision must hold iff used ({}) is at least 90% of total ({})",
            used,
            total
        );
    }
}
