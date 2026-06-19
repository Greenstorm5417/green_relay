//! Property-based test for SMS segmentation (Property 4).
//!
//! This test lives in its own integration-test crate (separate from
//! `src/sms.rs`) per the spec's test-placement note, and exercises the public
//! `segment_message` function of the `green_relay` library.
//!
//! Segmentation is encoding-aware: a body that is fully GSM-7 representable is
//! measured in septets (160 single / 153 concatenated), while a body containing
//! any non-GSM-7 character (here 中 / あ) is sent as UCS2 and measured in UTF-16
//! code units (70 single / 67 concatenated). Note the GSM-7 alphabet includes
//! the Greek capitals (Δ Φ Γ Λ Ω Π Ψ Σ Θ Ξ) and the common European accented
//! letters, so those stay GSM-7. The oracle below mirrors that model
//! independently of the implementation.

use green_relay::sms::{SegmentError, segment_message};
use proptest::prelude::*;

/// GSM-7 single-part / concatenated-part budgets, in septets.
const GSM7_SINGLE_MAX: usize = 160;
const GSM7_MULTI_MAX: usize = 153;

/// UCS2 single-part / concatenated-part budgets, in UTF-16 code units.
const UCS2_SINGLE_MAX: usize = 70;
const UCS2_MULTI_MAX: usize = 67;

/// Maximum number of parts a message may be split into (Req 1.8).
const MAX_PARTS: usize = 10;

/// GSM-7 "extension table" characters, mirrored from the implementation.
/// Each of these occupies two septets; every other GSM-7 character occupies one.
const GSM7_EXTENSION_CHARS: [char; 9] = ['^', '{', '}', '\\', '[', '~', ']', '|', '€'];

/// The non-GSM-7 characters in the generator pool. Their presence forces UCS2.
/// (Ω is intentionally absent — it is a GSM-7 character.)
const NON_GSM7_CHARS: [char; 2] = ['中', 'あ'];

/// Independent oracle: does this body require UCS2 encoding?
fn is_ucs2(s: &str) -> bool {
    s.chars().any(|c| NON_GSM7_CHARS.contains(&c))
}

/// Oracle GSM-7 length of a string, in septets.
fn gsm7_len(s: &str) -> usize {
    s.chars()
        .map(|c| {
            if GSM7_EXTENSION_CHARS.contains(&c) {
                2
            } else {
                1
            }
        })
        .sum()
}

/// Oracle UCS2 length of a string, in UTF-16 code units.
fn ucs2_len(s: &str) -> usize {
    s.chars().map(char::len_utf16).sum()
}

/// Oracle length of a string measured in the units of the chosen encoding.
/// The encoding is a property of the *whole message*, so a segment of a UCS2
/// message that happens to contain only GSM-7 characters is still measured in
/// UTF-16 units — hence the explicit `ucs2` flag rather than re-deciding here.
fn encoded_len(s: &str, ucs2: bool) -> usize {
    if ucs2 { ucs2_len(s) } else { gsm7_len(s) }
}

/// The (single, multi) per-part budgets for the chosen encoding.
fn budgets(ucs2: bool) -> (usize, usize) {
    if ucs2 {
        (UCS2_SINGLE_MAX, UCS2_MULTI_MAX)
    } else {
        (GSM7_SINGLE_MAX, GSM7_MULTI_MAX)
    }
}

/// Pool of characters used to build bodies: a spread of single-septet ASCII,
/// multibyte single-septet GSM-7 characters, non-GSM-7 characters that force
/// UCS2, and the two-septet extension chars so generation exercises every
/// length class and split boundary.
fn body_char() -> impl Strategy<Value = char> {
    prop_oneof![
        70 => prop::sample::select(
            "abcdefghijklmnopqrstuvwxyz0123456789 .,!?".chars().collect::<Vec<_>>(),
        ),
        8 => prop::sample::select(vec!['é', 'ñ', 'ü', 'Ω']),
        7 => prop::sample::select(NON_GSM7_CHARS.to_vec()),
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
    // within the per-part budget of the selected encoding, sequence numbers are
    // contiguous and ascending starting at 1, there are at most 10 segments, and
    // any body within the single-part budget yields exactly one segment.
    //
    // Validates: Requirements 1.8
    #[test]
    fn prop_segmentation_preserves_content_and_bounds(body in body()) {
        // The encoding is decided once, from the whole body, and all lengths
        // below are measured in that encoding's units.
        let ucs2 = is_ucs2(&body);
        let (single_max, multi_max) = budgets(ucs2);

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

                // Per-part bound (Req 1.8): a single (unsegmented) part may
                // carry up to the single-part budget; each part of a multi-part
                // message is at most the concatenated budget.
                let per_part_max = if segments.len() == 1 {
                    single_max
                } else {
                    multi_max
                };
                for seg in &segments {
                    prop_assert!(
                        encoded_len(&seg.text, ucs2) <= per_part_max,
                        "segment {} has encoded length {}, exceeding {}",
                        seg.seq,
                        encoded_len(&seg.text, ucs2),
                        per_part_max
                    );
                }

                // Content is preserved: concatenating the parts reproduces the body.
                let joined: String = segments.iter().map(|s| s.text.as_str()).collect();
                prop_assert_eq!(&joined, &body, "concatenated segments do not match the body");

                // Any body within the single-part budget yields exactly one segment.
                if encoded_len(&body, ucs2) <= single_max {
                    prop_assert_eq!(
                        segments.len(),
                        1,
                        "body of encoded length {} should be a single segment, got {}",
                        encoded_len(&body, ucs2),
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
                    encoded_len(&body, ucs2) > single_max,
                    "a body of encoded length {} (<= {}) must not be rejected as too many parts",
                    encoded_len(&body, ucs2),
                    single_max
                );
            }
        }
    }
}
