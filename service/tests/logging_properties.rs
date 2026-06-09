//! Property-based tests for the structured logging module.
//!
//! These tests live in their own integration-test crate (separate from
//! `src/logging.rs`) per the spec's test-placement note, and exercise the
//! public `LogRecord` API of the `green_relay` library.

use proptest::prelude::*;
use green_relay::logging::{LogRecord, Severity};

/// The canonical set of severity strings allowed by Req 7.1.
const ALLOWED_SEVERITIES: [&str; 5] = ["TRACE", "DEBUG", "INFO", "WARN", "ERROR"];

/// Generate any one of the five severities.
fn any_severity() -> impl Strategy<Value = Severity> {
    (0usize..Severity::ALL.len()).prop_map(|i| Severity::ALL[i])
}

/// Generate a non-empty message string (1..=200 arbitrary characters).
fn non_empty_message() -> impl Strategy<Value = String> {
    proptest::collection::vec(any::<char>(), 1..200)
        .prop_map(|chars| chars.into_iter().collect::<String>())
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 256, ..ProptestConfig::default() })]

    // Feature: sms-microservice, Property 24: Log records are structurally
    // well-formed. For any log level and non-empty message, the emitted
    // structured record parses successfully and contains a non-empty timestamp
    // that includes both calendar date and time-of-day, a severity field whose
    // value is exactly one of TRACE, DEBUG, INFO, WARN, or ERROR, and a
    // non-empty message field.
    //
    // Validates: Requirements 7.1
    #[test]
    fn prop_log_records_are_structurally_well_formed(
        severity in any_severity(),
        message in non_empty_message(),
    ) {
        let record = LogRecord::new(severity, message.clone());

        // The emitted structured record parses successfully as JSON.
        let parsed: serde_json::Value = serde_json::from_str(&record.to_json_string())
            .expect("emitted log record must be valid JSON");
        let obj = parsed.as_object().expect("record must be a JSON object");

        // --- Timestamp: non-empty and includes calendar date + time-of-day. ---
        let timestamp = obj
            .get("timestamp")
            .and_then(|v| v.as_str())
            .expect("record must contain a string timestamp field");
        prop_assert!(!timestamp.is_empty(), "timestamp must be non-empty");
        // Parsing as an RFC 3339 datetime confirms the value carries both a
        // calendar date (year-month-day) and a time-of-day (hour:min:sec).
        let dt = chrono::DateTime::parse_from_rfc3339(timestamp);
        prop_assert!(
            dt.is_ok(),
            "timestamp {:?} must parse as an RFC 3339 date-time (date + time-of-day)",
            timestamp
        );

        // --- Severity: exactly one of the five canonical values. ---
        let severity_field = obj
            .get("severity")
            .and_then(|v| v.as_str())
            .expect("record must contain a string severity field");
        prop_assert!(
            ALLOWED_SEVERITIES.contains(&severity_field),
            "severity {:?} must be one of {:?}",
            severity_field,
            ALLOWED_SEVERITIES
        );

        // --- Message: present and non-empty, and faithfully preserved. ---
        let message_field = obj
            .get("message")
            .and_then(|v| v.as_str())
            .expect("record must contain a string message field");
        prop_assert!(!message_field.is_empty(), "message must be non-empty");
        prop_assert_eq!(message_field, message.as_str());
    }
}
