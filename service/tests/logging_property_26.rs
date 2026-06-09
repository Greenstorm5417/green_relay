//! Property-based test for structured-logging severity filtering.
//!
//! Feature: sms-microservice, Property 26: Severity filtering
//! Validates: Requirements 7.4, 7.5
//!
//! Property 26 (from design.md): *For any* pair of record severity and
//! configured minimum severity, the record is emitted if and only if its
//! severity is greater than or equal to the configured minimum.
//!
//! This lives in its own integration-test file (separate from `src/logging.rs`)
//! and exercises the public API only.

use proptest::prelude::*;
use green_relay::logging::{LogRecord, Severity};

/// Canonical rank of a severity per Req 7.1 ordering
/// `TRACE < DEBUG < INFO < WARN < ERROR`. This is an *independent* oracle: it
/// does not rely on the type's own `PartialOrd` implementation, so the property
/// genuinely cross-checks `should_emit` against the requirement.
fn canonical_rank(s: Severity) -> u8 {
    match s {
        Severity::Trace => 0,
        Severity::Debug => 1,
        Severity::Info => 2,
        Severity::Warn => 3,
        Severity::Error => 4,
    }
}

/// Strategy producing any of the five severities with uniform-ish coverage.
fn any_severity() -> impl Strategy<Value = Severity> {
    (0u8..5).prop_map(|i| Severity::ALL[i as usize])
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(1000))]

    /// For any record severity and any configured minimum severity, a record
    /// built at that severity is emitted iff its canonical rank is at least the
    /// configured minimum's canonical rank (Req 7.4, 7.5).
    #[test]
    fn severity_filtering_emits_iff_at_or_above_minimum(
        record_severity in any_severity(),
        min_severity in any_severity(),
    ) {
        let record = LogRecord::new(record_severity, "msg");

        let emitted = record.should_emit(min_severity);
        let expected = canonical_rank(record_severity) >= canonical_rank(min_severity);

        prop_assert_eq!(
            emitted,
            expected,
            "should_emit({:?}) with record {:?}: got {}, expected {} (ranks {} >= {})",
            min_severity,
            record_severity,
            emitted,
            expected,
            canonical_rank(record_severity),
            canonical_rank(min_severity),
        );
    }

    /// A record is always emitted at its own severity threshold (reflexivity of
    /// the "at or above" rule), regardless of the message content.
    #[test]
    fn record_always_emitted_at_its_own_severity(
        severity in any_severity(),
        message in "\\PC{1,64}",
    ) {
        let record = LogRecord::new(severity, message);
        prop_assert!(record.should_emit(severity));
    }
}
