//! SMS domain logic: validation, segmentation, and CMGS payload building.
//!
//! This module holds pure functions that are the heart of the property-based
//! tests. This file currently contains the phone-number / body validation and
//! field-presence checks (task 5.1). Segmentation and CMGS payload building
//! are added separately (task 5.5).

/// Maximum number of characters allowed in a message body (Req 1.1, 1.10).
pub const MAX_BODY_CHARS: usize = 1530;

/// Minimum number of decimal digits in an E.164 number (after the `+`).
const E164_MIN_DIGITS: usize = 7;

/// Maximum number of decimal digits in an E.164 number (after the `+`).
const E164_MAX_DIGITS: usize = 15;

/// A validation failure for a send request.
///
/// Variants map to the HTTP 400 client-error responses described in the
/// design's Error Handling section (Req 1.6, 1.7, 1.10).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ValidationError {
    /// One or more required fields were absent from the request. The vector
    /// names exactly the missing fields (e.g. `"to"`, `"body"`) (Req 1.6).
    MissingFields(Vec<String>),
    /// The supplied phone number is not in E.164 format (Req 1.7).
    InvalidPhoneNumber,
    /// The message body is empty (fewer than 1 character) (Req 1.1).
    BodyEmpty,
    /// The message body exceeds the maximum allowed length (Req 1.10).
    BodyTooLong,
}

/// Validate that `s` is a well-formed E.164 phone number.
///
/// A valid number is a leading `+` followed by 7 to 15 decimal digits and no
/// other characters (Req 1.7).
pub fn validate_e164(s: &str) -> Result<(), ValidationError> {
    let digits = match s.strip_prefix('+') {
        Some(rest) => rest,
        None => return Err(ValidationError::InvalidPhoneNumber),
    };

    // Every remaining character must be an ASCII decimal digit. Counting bytes
    // is equivalent to counting characters here because ASCII digits are
    // single-byte, and any non-ASCII-digit character is rejected outright.
    if !digits.bytes().all(|b| b.is_ascii_digit()) {
        return Err(ValidationError::InvalidPhoneNumber);
    }

    let len = digits.len();
    if (E164_MIN_DIGITS..=E164_MAX_DIGITS).contains(&len) {
        Ok(())
    } else {
        Err(ValidationError::InvalidPhoneNumber)
    }
}

/// Validate that `body` has a character length between 1 and 1,530 inclusive
/// (Req 1.1, 1.10).
pub fn validate_body(body: &str) -> Result<(), ValidationError> {
    let len = body.chars().count();
    if len == 0 {
        Err(ValidationError::BodyEmpty)
    } else if len > MAX_BODY_CHARS {
        Err(ValidationError::BodyTooLong)
    } else {
        Ok(())
    }
}

/// Check that the required `to` and `body` fields are present in a send
/// request.
///
/// `to` and `body` are `Some` when the field was supplied and `None` when it
/// was omitted. The returned error names exactly the absent fields, in a
/// stable order (`to` before `body`) (Req 1.6).
pub fn check_required_fields(to: Option<&str>, body: Option<&str>) -> Result<(), ValidationError> {
    let mut missing = Vec::new();
    if to.is_none() {
        missing.push("to".to_string());
    }
    if body.is_none() {
        missing.push("body".to_string());
    }

    if missing.is_empty() {
        Ok(())
    } else {
        Err(ValidationError::MissingFields(missing))
    }
}

#[cfg(test)]
mod validation_tests {
    use super::*;

    #[test]
    fn accepts_valid_e164_numbers() {
        assert_eq!(validate_e164("+14155552671"), Ok(()));
        assert_eq!(validate_e164("+1234567"), Ok(())); // exactly 7 digits
        assert_eq!(validate_e164("+123456789012345"), Ok(())); // exactly 15 digits
    }

    #[test]
    fn rejects_invalid_e164_numbers() {
        assert_eq!(validate_e164(""), Err(ValidationError::InvalidPhoneNumber));
        assert_eq!(
            validate_e164("14155552671"),
            Err(ValidationError::InvalidPhoneNumber)
        ); // no '+'
        assert_eq!(
            validate_e164("+123456"),
            Err(ValidationError::InvalidPhoneNumber)
        ); // 6 digits
        assert_eq!(
            validate_e164("+1234567890123456"),
            Err(ValidationError::InvalidPhoneNumber)
        ); // 16 digits
        assert_eq!(
            validate_e164("+1415555267a"),
            Err(ValidationError::InvalidPhoneNumber)
        ); // letter
        assert_eq!(
            validate_e164("+1 4155552671"),
            Err(ValidationError::InvalidPhoneNumber)
        ); // space
        assert_eq!(validate_e164("+"), Err(ValidationError::InvalidPhoneNumber)); // no digits
    }

    #[test]
    fn validates_body_length_bounds() {
        assert_eq!(validate_body("a"), Ok(())); // lower bound
        assert_eq!(validate_body(&"x".repeat(MAX_BODY_CHARS)), Ok(())); // upper bound
        assert_eq!(validate_body(""), Err(ValidationError::BodyEmpty));
        assert_eq!(
            validate_body(&"x".repeat(MAX_BODY_CHARS + 1)),
            Err(ValidationError::BodyTooLong)
        );
    }

    #[test]
    fn body_length_counts_characters_not_bytes() {
        // A 1530-char multibyte string is valid even though it exceeds 1530 bytes.
        let multibyte = "é".repeat(MAX_BODY_CHARS);
        assert_eq!(validate_body(&multibyte), Ok(()));
        assert_eq!(
            validate_body(&"é".repeat(MAX_BODY_CHARS + 1)),
            Err(ValidationError::BodyTooLong)
        );
    }

    #[test]
    fn names_exactly_the_missing_fields() {
        assert_eq!(
            check_required_fields(Some("+14155552671"), Some("hi")),
            Ok(())
        );
        assert_eq!(
            check_required_fields(None, Some("hi")),
            Err(ValidationError::MissingFields(vec!["to".to_string()]))
        );
        assert_eq!(
            check_required_fields(Some("+14155552671"), None),
            Err(ValidationError::MissingFields(vec!["body".to_string()]))
        );
        assert_eq!(
            check_required_fields(None, None),
            Err(ValidationError::MissingFields(vec![
                "to".to_string(),
                "body".to_string()
            ]))
        );
    }
}

// ---------------------------------------------------------------------------
// SMS segmentation and CMGS payload building (task 5.5, Requirements 1.3, 1.8)
// ---------------------------------------------------------------------------

/// Maximum number of GSM-7 characters (septets) in a single, unsegmented SMS.
const SINGLE_PART_MAX: usize = 160;

/// Maximum number of GSM-7 characters (septets) in each part of a
/// concatenated (multi-part) SMS. The remaining 7 septets of the 160-septet
/// budget are consumed by the concatenation User Data Header.
const MULTI_PART_MAX: usize = 153;

/// Maximum number of parts a single message may be split into (Req 1.8).
const MAX_PARTS: usize = 10;

/// A single SMS segment with its 1-based sequence number and text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Segment {
    /// 1-based sequence number; segments are returned in ascending order.
    pub seq: u8,
    /// The text carried by this segment.
    pub text: String,
}

/// Error returned when a message cannot be segmented.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SegmentError {
    /// The message requires more than [`MAX_PARTS`] parts to send.
    TooManyParts {
        /// The number of parts the message would have required.
        required: usize,
    },
}

impl core::fmt::Display for SegmentError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            SegmentError::TooManyParts { required } => write!(
                f,
                "message requires {required} parts which exceeds the maximum of {MAX_PARTS}"
            ),
        }
    }
}

impl std::error::Error for SegmentError {}

/// Number of GSM-7 septets a single character occupies: characters in the
/// GSM-7 extension table take two septets, all others take one.
///
/// Implemented as a direct `match` rather than an array `contains` scan: this
/// runs once per character during length measurement and segmentation, so the
/// branch-friendly match keeps the per-character cost minimal.
fn gsm7_char_len(c: char) -> usize {
    match c {
        '^' | '{' | '}' | '\\' | '[' | '~' | ']' | '|' | '€' => 2,
        _ => 1,
    }
}

/// Total GSM-7 length (in septets) of a string.
fn gsm7_len(s: &str) -> usize {
    s.chars().map(gsm7_char_len).sum()
}

/// Segment a message body for transmission.
///
/// If the body fits in [`SINGLE_PART_MAX`] GSM-7 characters (septets) it is
/// returned as a single segment. Otherwise it is split into parts of at most
/// [`MULTI_PART_MAX`] GSM-7 characters each, in ascending sequence order
/// starting at 1, up to [`MAX_PARTS`] parts. Splits always fall on character
/// boundaries, so concatenating the segment texts reproduces the original
/// body exactly. (Req 1.8)
pub fn segment_message(body: &str) -> Result<Vec<Segment>, SegmentError> {
    // Single part if the whole body fits within the single-part budget.
    let total_septets = gsm7_len(body);
    if total_septets <= SINGLE_PART_MAX {
        return Ok(vec![Segment {
            seq: 1,
            text: body.to_string(),
        }]);
    }

    // Otherwise split into <= MULTI_PART_MAX-septet parts on char boundaries,
    // building the `Segment`s directly (no intermediate `Vec<String>` and
    // re-collect). The part count is bounded by the validated body length, so
    // it comfortably fits the `u8` sequence number.
    let mut segments: Vec<Segment> = Vec::with_capacity(
        total_septets
            .checked_div(MULTI_PART_MAX)
            .unwrap_or(0)
            .saturating_add(1),
    );
    let mut current = String::new();
    let mut current_len = 0usize;

    for c in body.chars() {
        let clen = gsm7_char_len(c);
        // Start a new part if adding this character would overflow the
        // per-part budget. clen is at most 2 and MULTI_PART_MAX is >= 2, so a
        // single character always fits in a fresh part.
        if current_len.saturating_add(clen) > MULTI_PART_MAX {
            let seq = segments.len().saturating_add(1) as u8;
            segments.push(Segment {
                seq,
                text: std::mem::take(&mut current),
            });
            current_len = 0;
        }
        current.push(c);
        current_len = current_len.saturating_add(clen);
    }
    if !current.is_empty() {
        let seq = segments.len().saturating_add(1) as u8;
        segments.push(Segment { seq, text: current });
    }

    if segments.len() > MAX_PARTS {
        return Err(SegmentError::TooManyParts {
            required: segments.len(),
        });
    }

    Ok(segments)
}

/// Build the `AT+CMGS` payload for a single message part.
///
/// Produces `AT+CMGS="<to>"\r<part>` terminated by the `0x1A` (Ctrl-Z)
/// control byte that instructs the modem to transmit the message. (Req 1.3)
pub fn build_cmgs(to: &str, part: &str) -> Vec<u8> {
    let mut payload = Vec::with_capacity(to.len().saturating_add(part.len()).saturating_add(12));
    payload.extend_from_slice(b"AT+CMGS=\"");
    payload.extend_from_slice(to.as_bytes());
    payload.extend_from_slice(b"\"\r");
    payload.extend_from_slice(part.as_bytes());
    payload.push(0x1A);
    payload
}

#[cfg(test)]
mod segment_tests {
    use super::*;

    #[test]
    fn short_body_is_single_segment() {
        let segs = segment_message("hello world").unwrap();
        assert_eq!(segs.len(), 1);
        assert_eq!(segs[0].seq, 1);
        assert_eq!(segs[0].text, "hello world");
    }

    #[test]
    fn exactly_160_chars_is_single_segment() {
        let body = "a".repeat(160);
        let segs = segment_message(&body).unwrap();
        assert_eq!(segs.len(), 1);
        assert_eq!(segs[0].text, body);
    }

    #[test]
    fn body_161_chars_is_split_into_two_parts() {
        let body = "a".repeat(161);
        let segs = segment_message(&body).unwrap();
        assert_eq!(segs.len(), 2);
        // First part is full (153), second carries the remainder.
        assert_eq!(segs[0].text.chars().count(), 153);
        assert_eq!(segs[1].text.chars().count(), 8);
        // Sequence numbers ascend from 1 and are contiguous.
        assert_eq!(segs[0].seq, 1);
        assert_eq!(segs[1].seq, 2);
        // Concatenation reproduces the original body.
        let joined: String = segs.iter().map(|s| s.text.as_str()).collect();
        assert_eq!(joined, body);
    }

    #[test]
    fn every_part_within_bound_and_content_preserved() {
        let body = "x".repeat(1530);
        let segs = segment_message(&body).unwrap();
        assert_eq!(segs.len(), 10);
        for (i, seg) in segs.iter().enumerate() {
            assert_eq!(seg.seq as usize, i + 1);
            assert!(gsm7_len(&seg.text) <= MULTI_PART_MAX);
        }
        let joined: String = segs.iter().map(|s| s.text.as_str()).collect();
        assert_eq!(joined, body);
    }

    #[test]
    fn too_many_parts_errors() {
        // Extension characters consume two septets each, so 1530 of them
        // require 3060 septets => 21 parts, which exceeds the 10-part max.
        let body = "€".repeat(1530);
        let err = segment_message(&body).unwrap_err();
        match err {
            SegmentError::TooManyParts { required } => assert!(required > MAX_PARTS),
        }
    }

    #[test]
    fn extension_chars_count_as_two_septets() {
        // 80 '€' = 160 septets => still a single part.
        let body = "€".repeat(80);
        assert_eq!(segment_message(&body).unwrap().len(), 1);
        // 81 '€' = 162 septets => must split.
        let body = "€".repeat(81);
        assert!(segment_message(&body).unwrap().len() > 1);
    }

    #[test]
    fn build_cmgs_contains_number_and_terminator() {
        let payload = build_cmgs("+14155552671", "hi there");
        // Terminated by Ctrl-Z (0x1A).
        assert_eq!(*payload.last().unwrap(), 0x1A);
        // Contains the phone number bytes.
        let needle = b"+14155552671";
        assert!(
            payload.windows(needle.len()).any(|w| w == needle),
            "payload should contain the phone number"
        );
        // Contains the message part bytes.
        let part = b"hi there";
        assert!(payload.windows(part.len()).any(|w| w == part));
    }
}
