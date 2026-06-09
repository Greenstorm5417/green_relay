//! Property-based test for inbound (`AT+CMGR`) message parsing (Property 8).
//!
//! This test lives in its own integration-test crate (separate from
//! `src/modem.rs`) per the spec's test-placement note, and exercises the
//! public `format_cmgr_response` / `parse_cmgr` pair of the
//! `sms_micro_service` library.

use proptest::prelude::*;
use sms_micro_service::modem::{format_cmgr_response, parse_cmgr, ParsedInbound};

/// Independent oracle mirroring the modem's terminating-result-code grammar.
///
/// A response line terminates an AT exchange when, after trimming, it is
/// exactly `OK`/`ERROR` or carries a `+CMS ERROR:` / `+CME ERROR:` prefix.
/// A message body line that happens to match one of these would be consumed
/// as a terminator rather than as body text, so such bodies fall outside the
/// round-trip's valid input space and are excluded by the body generator.
fn line_is_terminator(line: &str) -> bool {
    let t = line.trim();
    t == "OK" || t == "ERROR" || t.starts_with("+CMS ERROR:") || t.starts_with("+CME ERROR:")
}

/// Generate a realistic sender (originating address): an E.164 number, a bare
/// numeric address, or an alphanumeric sender ID. None contain a double quote
/// (which would clash with the quote-delimited header field) nor leading or
/// trailing whitespace (which the parser trims).
fn sender_strategy() -> impl Strategy<Value = String> {
    prop_oneof![
        "\\+[0-9]{7,15}",             // E.164, e.g. +14155552671
        "[0-9]{3,15}",                // bare numeric address
        "[A-Za-z][A-Za-z0-9]{1,10}",  // alphanumeric sender ID
    ]
    .prop_map(|s| s.to_string())
}

/// Generate a message body of zero or more newline-separated lines. Carriage
/// returns are excluded because `\r\n` is the on-wire line delimiter, and any
/// body whose lines would be read as a terminating result code is filtered
/// out as outside the round-trip's valid input space.
fn body_strategy() -> impl Strategy<Value = String> {
    "[^\r\n]{0,30}(\n[^\r\n]{0,30}){0,4}"
        .prop_map(|s| s.to_string())
        .prop_filter(
            "body line must not look like a terminating result code",
            |b| !b.lines().any(line_is_terminator),
        )
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 256, ..ProptestConfig::default() })]

    // Feature: sms-microservice, Property 8: Inbound message parsing
    // round-trips. For any sender number and message body, formatting an
    // AT+CMGR response and then parsing it recovers the same sender and body
    // in the resulting inbound record.
    //
    // Validates: Requirements 2.2
    #[test]
    fn prop_cmgr_parsing_round_trips(
        sender in sender_strategy(),
        body in body_strategy(),
    ) {
        let response = format_cmgr_response(&sender, &body);
        let parsed = parse_cmgr(&response);

        prop_assert_eq!(
            parsed,
            Some(ParsedInbound {
                sender: sender.clone(),
                body: body.clone(),
            }),
            "round-trip of sender={:?} body={:?} via response {:?} did not recover the originals",
            sender,
            body,
            response
        );
    }
}
