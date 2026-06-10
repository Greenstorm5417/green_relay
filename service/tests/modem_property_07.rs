//! Property-based test for modem result-line classification (Property 7).
//!
//! This test lives in its own integration-test crate (separate from
//! `src/modem.rs`) per the spec's test-placement note, and exercises the
//! public `classify_line` terminator classifier plus `parse_send_outcome`
//! error-code mapping of the `green_relay` library.

use green_relay::models::MessageStatus;
use green_relay::modem::{AtResult, LineClass, classify_line, parse_send_outcome};
use proptest::prelude::*;

/// Independent oracle for the terminator classifier, written separately from
/// the implementation so the property checks the implementation against the
/// specification rather than against itself.
///
/// Surrounding whitespace is ignored. A line terminates the exchange iff it is
/// exactly `OK`, exactly `ERROR`, or carries a `+CMS ERROR:` / `+CME ERROR:`
/// prefix; the numeric trailing code is recovered into the typed variant when
/// it parses as an integer, otherwise the line still terminates as a generic
/// `Error`. Any other line is non-terminating.
fn expected_class(line: &str) -> LineClass {
    let trimmed = line.trim();

    if trimmed == "OK" {
        return LineClass::Terminator(AtResult::Ok);
    }
    if trimmed == "ERROR" {
        return LineClass::Terminator(AtResult::Error);
    }
    if let Some(rest) = trimmed.strip_prefix("+CMS ERROR:") {
        return match rest.trim().parse::<u16>() {
            Ok(code) => LineClass::Terminator(AtResult::CmsError(code)),
            Err(_) => LineClass::Terminator(AtResult::Error),
        };
    }
    if let Some(rest) = trimmed.strip_prefix("+CME ERROR:") {
        return match rest.trim().parse::<u16>() {
            Ok(code) => LineClass::Terminator(AtResult::CmeError(code)),
            Err(_) => LineClass::Terminator(AtResult::Error),
        };
    }

    LineClass::NonTerminating
}

/// Generate lines biased toward the interesting terminator shapes, plus
/// arbitrary strings so the broad input space is also covered.
fn modem_line() -> impl Strategy<Value = String> {
    prop_oneof![
        // Exact OK / ERROR, optionally with surrounding whitespace and CR/LF.
        Just("OK".to_string()),
        Just("ERROR".to_string()),
        "[ \t]*OK[ \t]*\r?\n?".prop_map(|s| s.to_string()),
        "[ \t]*ERROR[ \t]*\r?\n?".prop_map(|s| s.to_string()),
        // +CMS / +CME ERROR with a numeric code in the u16 range.
        (0u16..=u16::MAX).prop_map(|c| format!("+CMS ERROR: {c}")),
        (0u16..=u16::MAX).prop_map(|c| format!("+CME ERROR: {c}")),
        // +CMS / +CME ERROR with a non-numeric (verbose) code.
        "\\+CMS ERROR: [A-Za-z ]{1,12}".prop_map(|s| s.to_string()),
        "\\+CME ERROR: [A-Za-z ]{1,12}".prop_map(|s| s.to_string()),
        // Common non-terminating lines: echoes, intermediate results, prompts.
        Just("".to_string()),
        Just("> ".to_string()),
        "\\+CMGS: [0-9]{1,5}".prop_map(|s| s.to_string()),
        "AT\\+[A-Z]{3,5}".prop_map(|s| s.to_string()),
        any::<String>(),
    ]
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 1000, ..ProptestConfig::default() })]

    // Feature: sms-microservice, Property 7: Modem result-line classification.
    // For any modem response line, the terminator classifier identifies it as
    // exactly one of OK, ERROR, +CMS ERROR: <code>, +CME ERROR: <code>, or
    // non-terminating; only the four result codes terminate an exchange, and
    // error codes are recovered as the parsed integer with the outcome mapped
    // to status `failed`.
    //
    // Validates: Requirements 1.5, 8.4
    #[test]
    fn prop_result_line_classification(line in modem_line()) {
        let actual = classify_line(&line);
        let expected = expected_class(&line);

        // The classifier matches the independent oracle exactly: it is one of
        // the four terminating result codes or non-terminating, never anything
        // else.
        prop_assert_eq!(
            &actual,
            &expected,
            "classify_line({:?}) returned {:?}, expected {:?}",
            line,
            actual,
            expected
        );

        match actual {
            LineClass::Terminator(result) => {
                match result {
                    // Numeric +CMS/+CME error codes are recovered as the parsed
                    // integer, and a send terminated by them maps to `failed`
                    // carrying that exact code (Req 1.5).
                    AtResult::CmsError(code) | AtResult::CmeError(code) => {
                        let outcome = parse_send_outcome(&[line.as_str()]);
                        prop_assert_eq!(outcome.status, MessageStatus::Failed);
                        prop_assert_eq!(outcome.error_code, Some(code));
                        prop_assert_eq!(outcome.reference, None);
                    }
                    // A bare ERROR (including a verbose, unparseable error)
                    // still terminates and maps a send to `failed` with no
                    // recovered code (Req 1.5).
                    AtResult::Error => {
                        let outcome = parse_send_outcome(&[line.as_str()]);
                        prop_assert_eq!(outcome.status, MessageStatus::Failed);
                        prop_assert_eq!(outcome.error_code, None);
                    }
                    // OK without a +CMGS reference is not a successful send.
                    AtResult::Ok => {
                        let outcome = parse_send_outcome(&[line.as_str()]);
                        prop_assert_eq!(outcome.status, MessageStatus::Failed);
                    }
                    // `Timeout` has no on-wire line and must never be produced
                    // by line classification.
                    AtResult::Timeout => {
                        prop_assert!(false, "classify_line produced Timeout for {:?}", line);
                    }
                }
            }
            LineClass::NonTerminating => {
                // A non-terminating line on its own never terminates a send
                // exchange; with no terminator the send is treated as failed.
                let outcome = parse_send_outcome(&[line.as_str()]);
                prop_assert_eq!(outcome.status, MessageStatus::Failed);
            }
        }
    }
}
