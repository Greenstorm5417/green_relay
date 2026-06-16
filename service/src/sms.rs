pub const MAX_BODY_CHARS: usize = 1530;

const E164_MIN_DIGITS: usize = 7;

const E164_MAX_DIGITS: usize = 15;

pub use crate::error::ValidationError;

pub fn validate_e164(s: &str) -> Result<(), ValidationError> {
    let digits = match s.strip_prefix('+') {
        Some(rest) => rest,
        None => return Err(ValidationError::InvalidPhoneNumber),
    };

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
        assert_eq!(validate_e164("+1234567"), Ok(()));
        assert_eq!(validate_e164("+123456789012345"), Ok(()));
    }

    #[test]
    fn rejects_invalid_e164_numbers() {
        assert_eq!(validate_e164(""), Err(ValidationError::InvalidPhoneNumber));
        assert_eq!(
            validate_e164("14155552671"),
            Err(ValidationError::InvalidPhoneNumber)
        );
        assert_eq!(
            validate_e164("+123456"),
            Err(ValidationError::InvalidPhoneNumber)
        );
        assert_eq!(
            validate_e164("+1234567890123456"),
            Err(ValidationError::InvalidPhoneNumber)
        );
        assert_eq!(
            validate_e164("+1415555267a"),
            Err(ValidationError::InvalidPhoneNumber)
        );
        assert_eq!(
            validate_e164("+1 4155552671"),
            Err(ValidationError::InvalidPhoneNumber)
        );
        assert_eq!(validate_e164("+"), Err(ValidationError::InvalidPhoneNumber));
    }

    #[test]
    fn validates_body_length_bounds() {
        assert_eq!(validate_body("a"), Ok(()));
        assert_eq!(validate_body(&"x".repeat(MAX_BODY_CHARS)), Ok(()));
        assert_eq!(validate_body(""), Err(ValidationError::BodyEmpty));
        assert_eq!(
            validate_body(&"x".repeat(MAX_BODY_CHARS + 1)),
            Err(ValidationError::BodyTooLong)
        );
    }

    #[test]
    fn body_length_counts_characters_not_bytes() {
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

const SINGLE_PART_MAX: usize = 160;

const MULTI_PART_MAX: usize = 153;

const MAX_PARTS: usize = 10;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Segment {
    pub seq: u8,

    pub text: String,
}

pub use crate::error::SegmentError;

fn gsm7_char_len(c: char) -> usize {
    match c {
        '^' | '{' | '}' | '\\' | '[' | '~' | ']' | '|' | '€' => 2,
        _ => 1,
    }
}

fn gsm7_len(s: &str) -> usize {
    s.chars().map(gsm7_char_len).sum()
}

pub fn segment_message(body: &str) -> Result<Vec<Segment>, SegmentError> {
    let total_septets = gsm7_len(body);
    if total_septets <= SINGLE_PART_MAX {
        return Ok(vec![Segment {
            seq: 1,
            text: body.to_string(),
        }]);
    }

    let mut segments: Vec<Segment> = Vec::with_capacity(
        total_septets
            .checked_div(MULTI_PART_MAX)
            .unwrap_or(0)
            .saturating_add(1),
    );
    let mut seg_start = 0usize;
    let mut current_len = 0usize;

    for (idx, c) in body.char_indices() {
        let clen = gsm7_char_len(c);
        if current_len.saturating_add(clen) > MULTI_PART_MAX {
            let seq = segments.len().saturating_add(1) as u8;
            segments.push(Segment {
                seq,
                text: body.get(seg_start..idx).unwrap_or_default().to_string(),
            });
            seg_start = idx;
            current_len = 0;
        }
        current_len = current_len.saturating_add(clen);
    }

    let seq = segments.len().saturating_add(1) as u8;
    segments.push(Segment {
        seq,
        text: body.get(seg_start..).unwrap_or_default().to_string(),
    });

    if segments.len() > MAX_PARTS {
        return Err(SegmentError::TooManyParts {
            required: segments.len(),
        });
    }

    Ok(segments)
}

/// Builds an `AT+CMGS` payload, stripping the `0x1A` terminator from the body
/// so only the appended terminator ends the message (Req 1.3).
pub fn build_cmgs(to: &str, part: &str) -> Vec<u8> {
    let mut payload = Vec::with_capacity(to.len().saturating_add(part.len()).saturating_add(12));
    payload.extend_from_slice(b"AT+CMGS=\"");
    payload.extend(to.bytes().filter(|b| b.is_ascii_digit() || *b == b'+'));
    payload.extend_from_slice(b"\"\r");
    payload.extend(part.bytes().filter(|b| *b != 0x1A));
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

        assert_eq!(segs[0].text.chars().count(), 153);
        assert_eq!(segs[1].text.chars().count(), 8);

        assert_eq!(segs[0].seq, 1);
        assert_eq!(segs[1].seq, 2);

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
        let body = "€".repeat(1530);
        let err = segment_message(&body).unwrap_err();
        match err {
            SegmentError::TooManyParts { required } => assert!(required > MAX_PARTS),
        }
    }

    #[test]
    fn extension_chars_count_as_two_septets() {
        let body = "€".repeat(80);
        assert_eq!(segment_message(&body).unwrap().len(), 1);

        let body = "€".repeat(81);
        assert!(segment_message(&body).unwrap().len() > 1);
    }

    #[test]
    fn build_cmgs_contains_number_and_terminator() {
        let payload = build_cmgs("+14155552671", "hi there");

        assert_eq!(*payload.last().unwrap(), 0x1A);

        let needle = b"+14155552671";
        assert!(
            payload.windows(needle.len()).any(|w| w == needle),
            "payload should contain the phone number"
        );

        let part = b"hi there";
        assert!(payload.windows(part.len()).any(|w| w == part));
    }

    #[test]
    fn build_cmgs_strips_at_metacharacters_from_number() {
        let payload = build_cmgs("+1\"415\x1a5552671", "body");

        let quote_count = payload.iter().filter(|b| **b == b'"').count();
        assert_eq!(quote_count, 2, "only the two AT command delimiter quotes");

        let terminator_count = payload.iter().filter(|b| **b == 0x1A).count();
        assert_eq!(terminator_count, 1, "only the appended 0x1A terminator");

        assert_eq!(*payload.last().unwrap(), 0x1A);

        let needle = b"+14155552671";
        assert!(
            payload.windows(needle.len()).any(|w| w == needle),
            "digits and leading + must be preserved in order"
        );
    }

    #[test]
    fn build_cmgs_strips_terminator_but_keeps_line_breaks() {
        let payload = build_cmgs("+14155552671", "evil\x1abody\r\nmore");

        let terminator_count = payload.iter().filter(|b| **b == 0x1A).count();
        assert_eq!(terminator_count, 1, "only the appended 0x1A terminator");
        assert_eq!(*payload.last().unwrap(), 0x1A);

        // CR/LF are legal SMS body content and must be preserved.
        assert_eq!(
            payload.iter().filter(|b| **b == b'\r').count(),
            2,
            "the command delimiter CR plus the body CR are both present"
        );
        assert_eq!(payload.iter().filter(|b| **b == b'\n').count(), 1);

        let needle = b"evilbody\r\nmore";
        assert!(
            payload.windows(needle.len()).any(|w| w == needle),
            "body content survives verbatim minus the 0x1A terminator"
        );
    }
}
