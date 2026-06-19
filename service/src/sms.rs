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

// GSM-7 single-part / concatenated-part capacities, in septets.
const SINGLE_PART_MAX: usize = 160;
const MULTI_PART_MAX: usize = 153;

// UCS2 single-part / concatenated-part capacities, in UTF-16 code units
// (a UCS2 part is 140 bytes = 70 units; concatenated parts reserve header room).
const UCS2_SINGLE_MAX: usize = 70;
const UCS2_MULTI_MAX: usize = 67;

const MAX_PARTS: usize = 10;

/// The on-air text encoding selected for a message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SmsEncoding {
    /// GSM 03.38 7-bit default alphabet (ASCII plus common European letters).
    Gsm7,
    /// 16-bit UCS2 (UTF-16BE), required for any character outside GSM-7 such as
    /// emoji or CJK text.
    Ucs2,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Segment {
    pub seq: u8,

    pub text: String,
}

pub use crate::error::SegmentError;

/// GSM 03.38 default-alphabet characters that occupy a single septet.
const GSM7_BASIC: &str = "@£$¥èéùìòÇ\nØø\rÅåΔ_ΦΓΛΩΠΨΣΘΞÆæßÉ !\"#¤%&'()*+,-./0123456789:;<=>?¡ABCDEFGHIJKLMNOPQRSTUVWXYZÄÖÑÜ§¿abcdefghijklmnopqrstuvwxyzäöñüà";

/// GSM 03.38 extension characters that occupy two septets (ESC + char).
const GSM7_EXTENDED: &str = "^{}\\[~]|€";

/// Returns the septet cost of `c` in the GSM 03.38 alphabet, or `None` if the
/// character is not representable in GSM-7 (so the message must use UCS2).
fn gsm7_septets(c: char) -> Option<usize> {
    if GSM7_EXTENDED.contains(c) {
        Some(2)
    } else if GSM7_BASIC.contains(c) {
        Some(1)
    } else {
        None
    }
}

/// Returns true if every character in `s` is representable in GSM-7.
pub fn is_gsm7(s: &str) -> bool {
    s.chars().all(|c| gsm7_septets(c).is_some())
}

/// Selects the on-air encoding for a message body: GSM-7 when every character
/// fits the default alphabet, otherwise UCS2.
pub fn message_encoding(body: &str) -> SmsEncoding {
    if is_gsm7(body) {
        SmsEncoding::Gsm7
    } else {
        SmsEncoding::Ucs2
    }
}

/// The septet length of a GSM-7 string (extension characters count as two).
/// Used by tests to assert per-part bounds; the segmenter measures inline.
#[cfg(test)]
fn gsm7_len(s: &str) -> usize {
    s.chars().map(|c| gsm7_septets(c).unwrap_or(1)).sum()
}

/// Splits `body` into segments using a per-character `cost` and the single- and
/// multi-part capacities for the chosen encoding. Segmentation always happens on
/// character boundaries, so neither a GSM-7 extension pair nor a UTF-16 surrogate
/// pair is ever split across parts.
fn segment_by<F: Fn(char) -> usize>(
    body: &str,
    cost: F,
    single_max: usize,
    multi_max: usize,
) -> Result<Vec<Segment>, SegmentError> {
    let total: usize = body.chars().map(&cost).sum();
    if total <= single_max {
        return Ok(vec![Segment {
            seq: 1,
            text: body.to_string(),
        }]);
    }

    let mut segments: Vec<Segment> =
        Vec::with_capacity(total.checked_div(multi_max).unwrap_or(0).saturating_add(1));
    let mut seg_start = 0usize;
    let mut current_len = 0usize;

    for (idx, c) in body.char_indices() {
        let clen = cost(c);
        if current_len.saturating_add(clen) > multi_max {
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

/// Segments a message body using the encoding chosen by [`message_encoding`]:
/// GSM-7 bodies are measured in septets (160/153), UCS2 bodies in UTF-16 code
/// units (70/67).
pub fn segment_message(body: &str) -> Result<Vec<Segment>, SegmentError> {
    match message_encoding(body) {
        SmsEncoding::Gsm7 => segment_by(
            body,
            |c| gsm7_septets(c).unwrap_or(1),
            SINGLE_PART_MAX,
            MULTI_PART_MAX,
        ),
        SmsEncoding::Ucs2 => segment_by(body, |c| c.len_utf16(), UCS2_SINGLE_MAX, UCS2_MULTI_MAX),
    }
}

/// Encodes a string as a UCS2 (UTF-16BE) uppercase-hex string — the on-the-wire
/// form the modem expects for the address and body when `AT+CSCS="UCS2"`.
pub fn ucs2_hex_encode(s: &str) -> String {
    use std::fmt::Write as _;
    let mut out = String::with_capacity(s.len().saturating_mul(4));
    for unit in s.encode_utf16() {
        let _ = write!(out, "{unit:04X}");
    }
    out
}

/// Decodes a UCS2 (UTF-16BE) hex string back into a Rust string. Returns `None`
/// if the input is not whole, even-length hex, or is not valid UTF-16 — callers
/// then fall back to treating the modem output as plain text.
pub fn ucs2_hex_decode(hex: &str) -> Option<String> {
    let hex = hex.trim();
    if hex.is_empty() || !hex.len().is_multiple_of(4) || !hex.bytes().all(|b| b.is_ascii_hexdigit())
    {
        return None;
    }
    let bytes = hex.as_bytes();
    let mut units = Vec::with_capacity(hex.len() / 4);
    let mut i = 0;
    while i < bytes.len() {
        let chunk = std::str::from_utf8(bytes.get(i..i.saturating_add(4))?).ok()?;
        units.push(u16::from_str_radix(chunk, 16).ok()?);
        i = i.saturating_add(4);
    }
    String::from_utf16(&units).ok()
}

/// Builds the `AT+CMGS="<to>"\r` header that opens SMS text-entry. The number is
/// sanitized to ASCII digits and `+` so no metacharacter can break framing; for
/// a [`SmsEncoding::Ucs2`] message the address is additionally UCS2-hex encoded,
/// because under `AT+CSCS="UCS2"` the modem reads the address as UCS2 too.
///
/// On the SIM7600 this command must be sent on its own; the modem answers with
/// the `>` prompt before it will accept the body (see [`build_cmgs_body`]).
pub fn build_cmgs_header(to: &str, encoding: SmsEncoding) -> Vec<u8> {
    let sanitized: String = to
        .chars()
        .filter(|c| c.is_ascii_digit() || *c == '+')
        .collect();
    let address = match encoding {
        SmsEncoding::Gsm7 => sanitized,
        SmsEncoding::Ucs2 => ucs2_hex_encode(&sanitized),
    };
    let mut payload = Vec::with_capacity(address.len().saturating_add(11));
    payload.extend_from_slice(b"AT+CMGS=\"");
    payload.extend_from_slice(address.as_bytes());
    payload.extend_from_slice(b"\"\r");
    payload
}

/// Builds the SMS body payload written at the `>` prompt, followed by the Ctrl-Z
/// submit terminator so only the appended terminator ends the message (Req 1.3).
///
/// GSM-7 bodies are sent verbatim (with any embedded `0x1A` stripped); UCS2
/// bodies are sent as a UTF-16BE hex string.
pub fn build_cmgs_body(part: &str, encoding: SmsEncoding) -> Vec<u8> {
    let mut payload = match encoding {
        SmsEncoding::Gsm7 => part.bytes().filter(|b| *b != 0x1A).collect::<Vec<u8>>(),
        SmsEncoding::Ucs2 => ucs2_hex_encode(part).into_bytes(),
    };
    payload.push(0x1A);
    payload
}

/// Builds the full single-shot GSM-7 `AT+CMGS` payload (header followed by body).
///
/// The two-phase send path uses [`build_cmgs_header`] and [`build_cmgs_body`]
/// separately so it can wait for the modem's `>` prompt in between; this
/// convenience composition frames the whole exchange at once.
pub fn build_cmgs(to: &str, part: &str) -> Vec<u8> {
    let mut payload = build_cmgs_header(to, SmsEncoding::Gsm7);
    payload.extend(build_cmgs_body(part, SmsEncoding::Gsm7));
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

    #[test]
    fn ascii_and_european_text_select_gsm7() {
        assert_eq!(message_encoding("hello world"), SmsEncoding::Gsm7);
        assert_eq!(message_encoding("Grüße aus Köln! €5"), SmsEncoding::Gsm7);
        assert_eq!(message_encoding("café à la carte"), SmsEncoding::Gsm7);
        assert!(is_gsm7("@£$¥èéù {curly} [bracket] ~tilde |pipe €"));
        // Lowercase c-cedilla is NOT in GSM-7 (only uppercase Ç is).
        assert_eq!(message_encoding("façade"), SmsEncoding::Ucs2);
    }

    #[test]
    fn emoji_and_cjk_select_ucs2() {
        assert_eq!(message_encoding("hello 😀"), SmsEncoding::Ucs2);
        assert_eq!(message_encoding("こんにちは"), SmsEncoding::Ucs2);
        assert_eq!(message_encoding("Привет"), SmsEncoding::Ucs2);
        assert!(!is_gsm7("emoji 🎉"));
    }

    #[test]
    fn ucs2_hex_roundtrips_including_astral_emoji() {
        for s in ["hi", "héllo", "こんにちは", "tea 🍵 time", "👨‍👩‍👧"] {
            let hex = ucs2_hex_encode(s);
            assert_eq!(hex.len() % 4, 0, "each UTF-16 unit is 4 hex digits: {s}");
            assert!(hex.bytes().all(|b| b.is_ascii_hexdigit()));
            assert_eq!(ucs2_hex_decode(&hex).as_deref(), Some(s));
        }
    }

    #[test]
    fn ucs2_hex_encodes_known_values() {
        // 'A' -> 0041, '😀' (U+1F600) -> surrogate pair D83D DE00.
        assert_eq!(ucs2_hex_encode("A"), "0041");
        assert_eq!(ucs2_hex_encode("😀"), "D83DDE00");
    }

    #[test]
    fn ucs2_hex_decode_rejects_malformed_input() {
        assert_eq!(ucs2_hex_decode(""), None);
        assert_eq!(ucs2_hex_decode("004"), None); // not a multiple of 4
        assert_eq!(ucs2_hex_decode("00ZZ"), None); // not hex
        assert_eq!(ucs2_hex_decode("DE00"), None); // lone low surrogate -> invalid UTF-16
    }

    #[test]
    fn ucs2_message_segments_by_utf16_units() {
        // 70 BMP characters fit a single UCS2 part; 71 spill into two.
        let single = "あ".repeat(70);
        assert_eq!(segment_message(&single).unwrap().len(), 1);
        let two = "あ".repeat(71);
        let segs = segment_message(&two).unwrap();
        assert_eq!(segs.len(), 2);
        assert_eq!(segs[0].text.chars().count(), 67); // multi-part cap
        let joined: String = segs.iter().map(|s| s.text.as_str()).collect();
        assert_eq!(joined, two);
    }

    #[test]
    fn ucs2_segmentation_never_splits_a_surrogate_pair() {
        // Astral emoji cost two UTF-16 units each; a part must never cut one.
        let body = "😀".repeat(50); // 100 UTF-16 units -> 2 parts
        let segs = segment_message(&body).unwrap();
        assert!(segs.len() >= 2);
        for seg in &segs {
            // Every part is independently valid UTF-8 with whole emoji.
            assert!(seg.text.chars().all(|c| c == '😀'));
            assert!(seg.text.encode_utf16().count() <= UCS2_MULTI_MAX);
        }
        let joined: String = segs.iter().map(|s| s.text.as_str()).collect();
        assert_eq!(joined, body);
    }

    #[test]
    fn ucs2_header_hex_encodes_the_address() {
        let header = build_cmgs_header("+14155552671", SmsEncoding::Ucs2);
        let text = String::from_utf8(header).unwrap();
        // '+','1','4'... -> 002B,0031,0034...
        assert!(text.contains("002B00310034"), "address is UCS2 hex: {text}");
        assert!(!text.contains("+14155552671"));
    }

    #[test]
    fn ucs2_body_is_hex_with_ctrl_z() {
        let payload = build_cmgs_body("😀", SmsEncoding::Ucs2);
        assert_eq!(*payload.last().unwrap(), 0x1A);
        let hex = String::from_utf8(payload[..payload.len() - 1].to_vec()).unwrap();
        assert_eq!(hex, "D83DDE00");
    }
}
