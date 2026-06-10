//! Property-based test for domain log record fields.
//!
//! Feature: sms-microservice, Property 25: Domain log records contain their
//! required fields
//! Validates: Requirements 7.2, 7.3
//!
//! Property 25 (from design.md): *For any* request log (method, path, status)
//! the emitted record contains all three values, and *for any* AT-exchange log
//! (command, result code) the emitted record contains both values.
//!
//! This lives in its own integration-test file (separate from `src/logging.rs`)
//! and exercises the public API only.

use green_relay::logging::{at_exchange_log, request_log};
use proptest::prelude::*;

/// Generate a non-empty, arbitrary HTTP-method-like token. Real methods are
/// short uppercase words, but the builder must faithfully carry whatever it is
/// given, so we exercise it with arbitrary non-empty strings.
fn method_strategy() -> impl Strategy<Value = String> {
    "\\PC{1,16}".prop_filter("non-empty", |s| !s.is_empty())
}

/// Generate an arbitrary request path. Paths begin with `/` in practice; we
/// constrain to non-empty printable strings to keep the cases meaningful while
/// still broad.
fn path_strategy() -> impl Strategy<Value = String> {
    "/\\PC{0,64}"
}

/// Generate an arbitrary AT command string (e.g. `AT+CMGS`).
fn command_strategy() -> impl Strategy<Value = String> {
    "\\PC{1,64}".prop_filter("non-empty", |s| !s.is_empty())
}

/// Generate an arbitrary result-code string (e.g. `OK`, `+CMGS: 42`,
/// `+CMS ERROR: 305`).
fn result_strategy() -> impl Strategy<Value = String> {
    "\\PC{1,64}".prop_filter("non-empty", |s| !s.is_empty())
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 256, ..ProptestConfig::default() })]

    // Feature: sms-microservice, Property 25: Domain log records contain their
    // required fields. For any request log (method, path, status) the emitted
    // record contains all three values.
    //
    // Validates: Requirements 7.2
    #[test]
    fn prop_request_log_contains_method_path_status(
        method in method_strategy(),
        path in path_strategy(),
        status in 100u16..=599u16,
    ) {
        let record = request_log(&method, &path, status);

        // The emitted structured record parses successfully as JSON.
        let parsed: serde_json::Value = serde_json::from_str(&record.to_json_string())
            .expect("emitted request log must be valid JSON");
        let obj = parsed.as_object().expect("record must be a JSON object");

        // All three domain values are present and faithfully preserved.
        let method_field = obj
            .get("method")
            .and_then(|v| v.as_str())
            .expect("request log must contain a string method field");
        prop_assert_eq!(method_field, method.as_str());

        let path_field = obj
            .get("path")
            .and_then(|v| v.as_str())
            .expect("request log must contain a string path field");
        prop_assert_eq!(path_field, path.as_str());

        let status_field = obj
            .get("status")
            .and_then(|v| v.as_i64())
            .expect("request log must contain an integer status field");
        prop_assert_eq!(status_field, status as i64);
    }

    // Feature: sms-microservice, Property 25: Domain log records contain their
    // required fields. For any AT-exchange log (command, result code) the
    // emitted record contains both values.
    //
    // Validates: Requirements 7.3
    #[test]
    fn prop_at_exchange_log_contains_command_and_result(
        command in command_strategy(),
        result in result_strategy(),
    ) {
        let record = at_exchange_log(&command, &result);

        // The emitted structured record parses successfully as JSON.
        let parsed: serde_json::Value = serde_json::from_str(&record.to_json_string())
            .expect("emitted at-exchange log must be valid JSON");
        let obj = parsed.as_object().expect("record must be a JSON object");

        // Both domain values are present and faithfully preserved.
        let command_field = obj
            .get("command")
            .and_then(|v| v.as_str())
            .expect("at-exchange log must contain a string command field");
        prop_assert_eq!(command_field, command.as_str());

        let result_field = obj
            .get("result")
            .and_then(|v| v.as_str())
            .expect("at-exchange log must contain a string result field");
        prop_assert_eq!(result_field, result.as_str());
    }
}
