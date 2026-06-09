//! Property-based test for credential redaction in auth records and logs.
//!
//! Feature: sms-microservice, Property 12: Auth records and logs never contain
//! plaintext credentials
//! Validates: Requirements 3.6, 7.6
//!
//! Property 12 (from design.md): *For any* API key or password, the audit
//! record and any auth-event log record built for it contain the non-reversible
//! key identifier and never contain the plaintext credential.
//!
//! This lives in its own integration-test file (separate from `src/auth.rs` and
//! `src/logging.rs`) and exercises the public API only.

use std::collections::BTreeMap;

use chrono::{TimeZone, Utc};
use proptest::prelude::*;
use serde_json::Value;

use green_relay::auth::{ApiKeyId, AuthOutcome, build_audit_record, key_identifier};
use green_relay::logging::{REDACTED, auth_event_log, redact_credentials};

/// Generate a realistic credential string (an API key or a password).
///
/// The generated value always embeds at least one "marker" character drawn
/// from `[!@#%&*]`. None of those characters can appear in a lowercase-hex
/// SHA-256 identifier (`0-9a-f`) or in an RFC 3339 timestamp
/// (`0-9`, `T`, `:`, `.`, `-`, `Z`). That guarantees the full plaintext can
/// never be a *coincidental* contiguous substring of the non-reversible
/// identifier or the timestamp, so any appearance of the plaintext in an
/// emitted record is a genuine leak rather than a hash/timestamp collision.
///
/// The surrounding arbitrary parts avoid the JSON-escaping characters `"` and
/// `\` so a leaked value would appear verbatim in the serialized record.
fn credential_strategy() -> impl Strategy<Value = String> {
    (
        "[a-zA-Z0-9 ._/+=:,;?()-]{0,40}",
        "[!@#%&*]",
        "[a-zA-Z0-9 ._/+=:,;?()-]{0,40}",
    )
        .prop_map(|(a, m, b)| format!("{a}{m}{b}"))
        .prop_filter("credential length within 1..=256 chars", |s| {
            let n = s.chars().count();
            (1..=256).contains(&n)
        })
}

/// Generate an arbitrary authentication outcome so every audit-result variant
/// is exercised.
fn outcome_strategy() -> impl Strategy<Value = AuthOutcome> {
    prop_oneof![
        any::<ApiKeyId>().prop_map(AuthOutcome::Authorized),
        Just(AuthOutcome::Unauthorized),
        Just(AuthOutcome::LockedOut),
    ]
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 256, ..ProptestConfig::default() })]

    // Feature: sms-microservice, Property 12: Auth records and logs never
    // contain plaintext credentials.
    //
    // For any credential, the audit record built for it carries the
    // non-reversible identifier and never the plaintext.
    //
    // Validates: Requirements 3.6
    #[test]
    fn prop_audit_record_carries_identifier_never_plaintext(
        credential in credential_strategy(),
        outcome in outcome_strategy(),
        epoch_secs in 0i64..=4_000_000_000i64,
    ) {
        let timestamp = Utc.timestamp_opt(epoch_secs, 0).single()
            .expect("valid timestamp");
        let record = build_audit_record(&credential, &outcome, timestamp);

        let expected_id = key_identifier(&credential);

        // The record carries the non-reversible identifier (Req 3.6) ...
        prop_assert_eq!(&record.key_identifier, &expected_id);
        // ... which is never the plaintext credential itself.
        prop_assert_ne!(&record.key_identifier, &credential);

        // The serialized record never contains the plaintext credential.
        let json = serde_json::to_string(&record)
            .expect("audit record must serialize");
        prop_assert!(
            !json.contains(&credential),
            "audit record leaked plaintext credential: {}",
            json
        );

        // Defensive structural check: parse back and confirm the identifier
        // field is exactly the hash and the plaintext appears in no field.
        let parsed: Value = serde_json::from_str(&json)
            .expect("audit record must be valid JSON");
        let obj = parsed.as_object().expect("record is a JSON object");
        prop_assert_eq!(
            obj.get("key_identifier").and_then(Value::as_str),
            Some(expected_id.as_str())
        );
        for (key, value) in obj {
            if let Some(s) = value.as_str() {
                prop_assert!(
                    !s.contains(&credential),
                    "field {} leaked plaintext credential",
                    key
                );
            }
        }
    }

    // Feature: sms-microservice, Property 12: Auth records and logs never
    // contain plaintext credentials.
    //
    // For any credential, the auth-event log record built from its identifier
    // contains the identifier and never the plaintext.
    //
    // Validates: Requirements 7.6
    #[test]
    fn prop_auth_event_log_carries_identifier_never_plaintext(
        credential in credential_strategy(),
        outcome in prop_oneof![
            Just("authorized"),
            Just("unauthorized"),
            Just("locked_out"),
        ],
    ) {
        let identifier = key_identifier(&credential);
        let record = auth_event_log(&identifier, outcome);

        let json = record.to_json_string();

        // The emitted record is valid JSON containing the identifier ...
        let parsed: Value = serde_json::from_str(&json)
            .expect("auth-event log must be valid JSON");
        let obj = parsed.as_object().expect("record is a JSON object");
        prop_assert_eq!(
            obj.get("key_identifier").and_then(Value::as_str),
            Some(identifier.as_str())
        );

        // ... and never the plaintext credential.
        prop_assert!(
            !json.contains(&credential),
            "auth-event log leaked plaintext credential: {}",
            json
        );
    }

    // Feature: sms-microservice, Property 12: Auth records and logs never
    // contain plaintext credentials.
    //
    // Even when a credential value is attached under a credential-named field,
    // redaction replaces it so the plaintext never survives into a record.
    //
    // Validates: Requirements 7.6
    #[test]
    fn prop_redaction_removes_plaintext_credential_fields(
        credential in credential_strategy(),
        field_name in prop_oneof![
            Just("api_key"),
            Just("password"),
            Just("secret"),
            Just("token"),
            Just("authorization"),
        ],
    ) {
        let mut fields: BTreeMap<String, Value> = BTreeMap::new();
        fields.insert(field_name.to_string(), Value::String(credential.clone()));
        // A non-credential field carrying the same value must be preserved as
        // given (it is not a credential field), so we keep it distinct.
        fields.insert("path".to_string(), Value::String("/login".to_string()));

        redact_credentials(&mut fields);

        // The credential field is masked and no longer carries the plaintext.
        prop_assert_eq!(
            fields.get(field_name),
            Some(&Value::String(REDACTED.to_string()))
        );
        let serialized = serde_json::to_string(&fields)
            .expect("fields must serialize");
        prop_assert!(
            !serialized.contains(&credential),
            "redacted field map leaked plaintext credential: {}",
            serialized
        );
    }
}
