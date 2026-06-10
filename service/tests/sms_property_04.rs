//! Property-based test for SMS segmentation (Property 4).
//!
//! This test lives in its own integration-test crate (separate from
//! `src/sms.rs`) per the spec's test-placement note, and exercises the public
//! `segment_message` function of the `green_relay` library.

use green_relay::sms::{SegmentError, segment_message};
use proptest::prelude::*;

/// Per-part GSM-7 budget for a concatenated (multi-part) SMS.
const MULTI_PART_MAX: usize = 153;

/// Single-part GSM-7 budget for an unsegmented SMS.
const SINGLE_PART_MAX: usize = 160;

/// Maximum number of parts a message may be split into (Req 1.8).
const MAX_PARTS: usize = 10;

/// GSM-7 "extension table" characters, mirrored from the implementation.
/// Each of these occupies two septets; every other character occupies one.
const GSM7_EXTENSION_CHARS: [char; 9] = ['^', '{', '}', '\\', '[', '~', ']', '|', '€'];

/// Independent oracle for the GSM-7 length (in septets) of a single character,
/// written separately from the implementation so the property checks the
/// implementation against the specification's GSM-7 model rather than itself.
fn gsm7_char_len(c: char) -> usize {
    if GSM7_EXTENSION_CHARS.contains(&c) {
        2
    } else {
        1
    }
}

/// Oracle GSM-7 length of a string.
fn gsm7_len(s: &str) -> usize {
    s.chars().map(gsm7_char_len).sum()
}

/// Pool of characters used to build bodies: a spread of single-septet ASCII,
/// multibyte single-septet characters, and the two-septet extension chars so
/// generation exercises both length classes and split boundaries.
fn body_char() -> impl Strategy<Value = char> {
    prop_oneof![
        70 => prop::sample::select(
            "abcdefghijklmnopqrstuvwxyz0123456789 .,!?".chars().collect::<Vec<_>>(),
        ),
        15 => prop::sample::select(vec!['é', 'ñ', 'ü', '中', 'あ', 'Ω']),
        15 => prop::sample::select(GSM7_EXTENSION_CHARS.to_vec()),
    ]
}

/// Generate message bodies across the full range of interesting sizes:
/// empty, short (single-part), and long (multi-part, including bodies large
/// enough to exceed the 10-part maximum).
fn body() -> impl Strategy<Value = String> {
    prop::collection::vec(body_char(), 0..1400).prop_map(|chars| chars.into_iter().collect())
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 1000, ..ProptestConfig::default() })]

    // Feature: sms-microservice, Property 4: Segmentation preserves content
    // and bounds. For any valid message body, the segments produced by
    // `segment_message` concatenate back to the original body, each segment is
    // at most 153 GSM-7 characters, sequence numbers are contiguous and
    // ascending starting at 1, there are at most 10 segments, and any body of
    // 160 or fewer GSM-7 characters yields exactly one segment.
    //
    // Validates: Requirements 1.8
    #[test]
    fn prop_segmentation_preserves_content_and_bounds(body in body()) {
        match segment_message(&body) {
            Ok(segments) => {
                // At most 10 segments (Req 1.8).
                prop_assert!(
                    segments.len() <= MAX_PARTS,
                    "produced {} segments, exceeding the {}-part maximum",
                    segments.len(),
                    MAX_PARTS
                );

                // At least one segment is always produced.
                prop_assert!(!segments.is_empty(), "expected at least one segment");

                // Sequence numbers are contiguous and ascending starting at 1.
                for (i, seg) in segments.iter().enumerate() {
                    prop_assert_eq!(
                        seg.seq as usize,
                        i + 1,
                        "segment at index {} has seq {}, expected {}",
                        i,
                        seg.seq,
                        i + 1
                    );
                }

                // Per-part GSM-7 bound (Req 1.8): a single (unsegmented) part
                // may carry up to 160 septets, while each part of a multi-part
                // message is at most 153 septets.
                let per_part_max = if segments.len() == 1 {
                    SINGLE_PART_MAX
                } else {
                    MULTI_PART_MAX
                };
                for seg in &segments {
                    prop_assert!(
                        gsm7_len(&seg.text) <= per_part_max,
                        "segment {} has GSM-7 length {}, exceeding {}",
                        seg.seq,
                        gsm7_len(&seg.text),
                        per_part_max
                    );
                }

                // Content is preserved: concatenating the parts reproduces the body.
                let joined: String = segments.iter().map(|s| s.text.as_str()).collect();
                prop_assert_eq!(&joined, &body, "concatenated segments do not match the body");

                // Any body of <= 160 GSM-7 chars yields exactly one segment.
                if gsm7_len(&body) <= SINGLE_PART_MAX {
                    prop_assert_eq!(
                        segments.len(),
                        1,
                        "body of GSM-7 length {} should be a single segment, got {}",
                        gsm7_len(&body),
                        segments.len()
                    );
                }
            }
            Err(SegmentError::TooManyParts { required }) => {
                // The only failure mode is needing more than the allowed parts.
                // A body that fits in a single part must never fail this way.
                prop_assert!(
                    required > MAX_PARTS,
                    "TooManyParts reported required={}, which does not exceed {}",
                    required,
                    MAX_PARTS
                );
                prop_assert!(
                    gsm7_len(&body) > SINGLE_PART_MAX,
                    "a body of GSM-7 length {} (<= {}) must not be rejected as too many parts",
                    gsm7_len(&body),
                    SINGLE_PART_MAX
                );
            }
        }
    }
}
