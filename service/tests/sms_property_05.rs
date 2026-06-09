//! Property-based test for the CMGS payload builder (Property 5).
//!
//! This test lives in its own integration-test crate (separate from
//! `src/sms.rs`) per the spec's test-placement note, and exercises the public
//! `build_cmgs` function of the `green_relay` library.

use proptest::prelude::*;
use green_relay::sms::build_cmgs;

/// The control byte (Ctrl-Z) that terminates an `AT+CMGS` payload and
/// instructs the modem to transmit the message (Req 1.3).
const CTRL_Z: u8 = 0x1A;

/// Generate a well-formed E.164 phone number: `+` followed by 7..=15 decimal
/// digits.
fn valid_e164() -> impl Strategy<Value = String> {
    "\\+[0-9]{7,15}".prop_map(|s| s.to_string())
}

/// Generate a message part. The part is constrained to characters that do not
/// themselves contain the 0x1A control byte, so the "terminated by 0x1A"
/// assertion is meaningful (the only 0x1A is the terminator the builder adds).
fn message_part() -> impl Strategy<Value = String> {
    // Arbitrary printable-ish text plus common GSM-7 / unicode characters,
    // excluding the Ctrl-Z control byte by construction.
    proptest::collection::vec(
        any::<char>().prop_filter("exclude Ctrl-Z", |c| *c as u32 != CTRL_Z as u32),
        0..160,
    )
    .prop_map(|chars| chars.into_iter().collect())
}

/// True iff `haystack` contains `needle` as a contiguous byte subsequence.
fn contains_bytes(haystack: &[u8], needle: &[u8]) -> bool {
    if needle.is_empty() {
        return true;
    }
    haystack.windows(needle.len()).any(|w| w == needle)
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 1000, ..ProptestConfig::default() })]

    // Feature: sms-microservice, Property 5: CMGS payload is well-formed.
    // For any valid phone number and message part, `build_cmgs` produces a
    // payload that contains the phone number and is terminated by the 0x1A
    // control byte.
    //
    // Validates: Requirements 1.3
    #[test]
    fn prop_cmgs_payload_is_well_formed(
        to in valid_e164(),
        part in message_part(),
    ) {
        let payload = build_cmgs(&to, &part);

        // The payload is non-empty and its final byte is the 0x1A terminator.
        prop_assert!(
            !payload.is_empty(),
            "build_cmgs produced an empty payload"
        );
        prop_assert_eq!(
            *payload.last().unwrap(),
            CTRL_Z,
            "payload for to={:?} part={:?} is not terminated by 0x1A",
            to,
            part
        );

        // The payload contains the phone number bytes.
        prop_assert!(
            contains_bytes(&payload, to.as_bytes()),
            "payload does not contain the phone number {:?}",
            to
        );

        // The message part bytes are also present in the payload.
        prop_assert!(
            contains_bytes(&payload, part.as_bytes()),
            "payload does not contain the message part {:?}",
            part
        );
    }
}
