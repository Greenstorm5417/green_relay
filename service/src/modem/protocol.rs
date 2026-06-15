//! Pure AT-command protocol parsing and formatting.
//!
//! Everything here is free of I/O and timing: result-code classification,
//! `+CMGS`/`+CMGR` formatting and parsing, the unsolicited-result-code (URC)
//! field parsers, and the reconnect backoff schedule. This is the most heavily
//! unit-tested layer of the modem actor.

use crate::health::SimStatus;
use crate::models::MessageStatus;

/// The maximum reconnect backoff delay, in seconds.
pub const RECONNECT_BACKOFF_CAP_SECS: u64 = 60;

/// Returns the reconnect backoff delay (seconds) for a 1-based attempt number.
pub fn reconnect_backoff_secs(attempt: u32) -> u64 {
    let exponent = attempt.saturating_sub(1);

    if exponent >= 6 {
        return RECONNECT_BACKOFF_CAP_SECS;
    }

    let delay = 1u64 << exponent;
    delay.min(RECONNECT_BACKOFF_CAP_SECS)
}

/// Returns the full backoff schedule for attempts `1..=max_attempts`.
pub fn reconnect_backoff_schedule(max_attempts: u32) -> Vec<u64> {
    (1..=max_attempts).map(reconnect_backoff_secs).collect()
}

/// The classified result of an AT command exchange.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AtResult {
    Ok,
    Error,
    CmsError(u16),
    CmeError(u16),
    Timeout,
}

impl AtResult {
    /// Returns true if the result is a plain `OK`.
    pub fn is_ok(&self) -> bool {
        matches!(self, AtResult::Ok)
    }

    /// Returns the numeric error code for `+CMS`/`+CME` errors.
    pub fn error_code(&self) -> Option<u16> {
        match self {
            AtResult::CmsError(code) | AtResult::CmeError(code) => Some(*code),
            _ => None,
        }
    }
}

impl std::fmt::Display for AtResult {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AtResult::Ok => write!(f, "OK"),
            AtResult::Error => write!(f, "ERROR"),
            AtResult::CmsError(c) => write!(f, "+CMS ERROR: {c}"),
            AtResult::CmeError(c) => write!(f, "+CME ERROR: {c}"),
            AtResult::Timeout => write!(f, "TIMEOUT"),
        }
    }
}

/// Whether a response line terminates an exchange and, if so, with what result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LineClass {
    Terminator(AtResult),
    NonTerminating,
}

/// Classifies a single response line as a terminator or a data line.
pub fn classify_line(line: &str) -> LineClass {
    let trimmed = line.trim();

    if trimmed == "OK" {
        return LineClass::Terminator(AtResult::Ok);
    }
    if trimmed == "ERROR" {
        return LineClass::Terminator(AtResult::Error);
    }
    if let Some(rest) = trimmed.strip_prefix("+CMS ERROR:") {
        let result = match rest.trim().parse::<u16>() {
            Ok(code) => AtResult::CmsError(code),
            Err(_) => AtResult::Error,
        };
        return LineClass::Terminator(result);
    }
    if let Some(rest) = trimmed.strip_prefix("+CME ERROR:") {
        let result = match rest.trim().parse::<u16>() {
            Ok(code) => AtResult::CmeError(code),
            Err(_) => AtResult::Error,
        };
        return LineClass::Terminator(result);
    }

    LineClass::NonTerminating
}

/// Formats a `+CMGS` response line for a given message reference.
pub fn format_cmgs_response(reference: u32) -> String {
    format!("+CMGS: {reference}")
}

/// Parses the message reference from a `+CMGS` response line.
pub fn parse_cmgs_reference(line: &str) -> Option<u32> {
    let rest = line.trim().strip_prefix("+CMGS:")?;
    rest.trim().parse::<u32>().ok()
}

/// The parsed outcome of an outbound send.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SendOutcome {
    pub status: MessageStatus,
    pub reference: Option<u32>,
    pub error_code: Option<u16>,
}

impl SendOutcome {
    fn sent(reference: u32) -> Self {
        SendOutcome {
            status: MessageStatus::Sent,
            reference: Some(reference),
            error_code: None,
        }
    }

    fn failed(error_code: Option<u16>) -> Self {
        SendOutcome {
            status: MessageStatus::Failed,
            reference: None,
            error_code,
        }
    }
}

/// Parses the result lines of an `AT+CMGS` exchange into a [`SendOutcome`].
pub fn parse_send_outcome(lines: &[&str]) -> SendOutcome {
    let mut reference: Option<u32> = None;

    for line in lines {
        if let Some(r) = parse_cmgs_reference(line) {
            reference = Some(r);
            continue;
        }
        match classify_line(line) {
            LineClass::Terminator(AtResult::Ok) => {
                return match reference {
                    Some(r) => SendOutcome::sent(r),
                    None => SendOutcome::failed(None),
                };
            }
            LineClass::Terminator(AtResult::CmsError(code))
            | LineClass::Terminator(AtResult::CmeError(code)) => {
                return SendOutcome::failed(Some(code));
            }
            LineClass::Terminator(AtResult::Error) | LineClass::Terminator(AtResult::Timeout) => {
                return SendOutcome::failed(None);
            }
            LineClass::NonTerminating => {}
        }
    }

    SendOutcome::failed(None)
}

/// A parsed inbound message: its sender and body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedInbound {
    pub sender: String,
    pub body: String,
}

/// Formats a `+CMGR` response for a given sender and body.
pub fn format_cmgr_response(sender: &str, body: &str) -> String {
    format!("+CMGR: \"REC UNREAD\",\"{sender}\",,\"24/01/02,03:04:05+00\"\r\n{body}\r\nOK")
}

/// Parses a `+CMGR` response into the inbound sender and body.
pub fn parse_cmgr(response: &str) -> Option<ParsedInbound> {
    let mut lines = response.lines();

    let header = lines.find(|l| l.trim_start().starts_with("+CMGR:"))?;
    let sender = parse_cmgr_sender(header)?;

    let mut body_lines: Vec<&str> = Vec::new();
    for line in lines {
        match classify_line(line) {
            LineClass::Terminator(_) => break,
            LineClass::NonTerminating => body_lines.push(line),
        }
    }
    let body = body_lines.join("\n");

    Some(ParsedInbound { sender, body })
}

fn parse_cmgr_sender(header: &str) -> Option<String> {
    let rest = header.trim_start().strip_prefix("+CMGR:")?;
    let fields = split_quoted_csv(rest.trim());
    let sender = fields.get(1)?.trim().trim_matches('"').to_string();
    Some(sender)
}

fn split_quoted_csv(s: &str) -> Vec<String> {
    let mut fields = Vec::new();
    let mut current = String::new();
    let mut in_quotes = false;

    for c in s.chars() {
        match c {
            '"' => {
                in_quotes = !in_quotes;
                current.push(c);
            }
            ',' if !in_quotes => fields.push(std::mem::take(&mut current)),
            _ => current.push(c),
        }
    }
    fields.push(current);
    fields
}

/// A completed AT command exchange: the command, its data lines, and result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AtExchange {
    pub command: String,
    pub lines: Vec<String>,
    pub result: AtResult,
}

/// Parses the storage index from a `+CMTI` new-message URC.
pub fn parse_cmti_index(line: &str) -> Option<u32> {
    let rest = line.trim().strip_prefix("+CMTI:")?;
    rest.rsplit(',').next()?.trim().parse::<u32>().ok()
}

/// Classifies a `+CPIN?` response into a [`SimStatus`].
pub fn parse_cpin(lines: &[String]) -> SimStatus {
    match lines.iter().find_map(|l| l.trim().strip_prefix("+CPIN:")) {
        Some(rest) if rest.trim() == "READY" => SimStatus::Ready,
        Some(_) => SimStatus::NotReady,
        None => SimStatus::Unknown,
    }
}

/// Returns true if a `+CREG?` response indicates network registration.
pub fn parse_creg_registered(lines: &[String]) -> bool {
    if let Some(rest) = lines.iter().find_map(|l| l.trim().strip_prefix("+CREG:"))
        && let Some(stat) = rest.split(',').nth(1)
    {
        return matches!(stat.trim().trim_matches('"'), "1" | "5");
    }
    false
}

/// Parses a signal-strength percentage from a `+CSQ` response.
pub fn parse_csq_percent(lines: &[String]) -> Option<u8> {
    let rest = lines.iter().find_map(|l| l.trim().strip_prefix("+CSQ:"))?;
    let rssi: u32 = rest.split(',').next()?.trim().parse().ok()?;
    if rssi == 99 || rssi > 31 {
        return None;
    }
    u8::try_from(rssi.saturating_mul(100) / 31).ok()
}

/// Parses the operator name from a `+COPS?` response.
pub fn parse_cops_operator(lines: &[String]) -> Option<String> {
    let rest = lines.iter().find_map(|l| l.trim().strip_prefix("+COPS:"))?;
    let fields = split_quoted_csv(rest.trim());
    let operator = fields.get(2)?.trim().trim_matches('"').to_string();
    if operator.is_empty() {
        None
    } else {
        Some(operator)
    }
}

#[cfg(test)]
mod at_parsing_tests {
    use super::*;

    #[test]
    fn classifies_terminating_result_codes() {
        assert_eq!(classify_line("OK"), LineClass::Terminator(AtResult::Ok));
        assert_eq!(
            classify_line("ERROR"),
            LineClass::Terminator(AtResult::Error)
        );
        assert_eq!(
            classify_line("+CMS ERROR: 500"),
            LineClass::Terminator(AtResult::CmsError(500))
        );
        assert_eq!(
            classify_line("+CME ERROR: 30"),
            LineClass::Terminator(AtResult::CmeError(30))
        );
    }

    #[test]
    fn ignores_surrounding_whitespace_and_carriage_returns() {
        assert_eq!(
            classify_line("  OK\r\n"),
            LineClass::Terminator(AtResult::Ok)
        );
        assert_eq!(
            classify_line("+CMS ERROR:  42 \r"),
            LineClass::Terminator(AtResult::CmsError(42))
        );
    }

    #[test]
    fn classifies_non_terminating_lines() {
        assert_eq!(classify_line(""), LineClass::NonTerminating);
        assert_eq!(classify_line("AT+CMGS=\"+1\""), LineClass::NonTerminating);
        assert_eq!(classify_line("+CMGS: 42"), LineClass::NonTerminating);
        assert_eq!(classify_line("> "), LineClass::NonTerminating);
        assert_eq!(classify_line("hello body"), LineClass::NonTerminating);
    }

    #[test]
    fn error_with_unparseable_code_still_terminates() {
        assert_eq!(
            classify_line("+CME ERROR: SIM not inserted"),
            LineClass::Terminator(AtResult::Error)
        );
    }

    #[test]
    fn cmgs_reference_round_trips() {
        for reference in [0u32, 1, 42, 255, 65_535, 1_000_000] {
            let line = format_cmgs_response(reference);
            assert_eq!(parse_cmgs_reference(&line), Some(reference));
        }
    }

    #[test]
    fn parse_cmgs_reference_rejects_non_cmgs_lines() {
        assert_eq!(parse_cmgs_reference("OK"), None);
        assert_eq!(parse_cmgs_reference("+CMGR: stuff"), None);
        assert_eq!(parse_cmgs_reference("+CMGS: notanumber"), None);
    }

    #[test]
    fn send_outcome_maps_reference_to_sent() {
        let lines = ["AT+CMGS=\"+14155552671\"", "+CMGS: 42", "OK"];
        let outcome = parse_send_outcome(&lines);
        assert_eq!(
            outcome,
            SendOutcome {
                status: MessageStatus::Sent,
                reference: Some(42),
                error_code: None,
            }
        );
    }

    #[test]
    fn send_outcome_maps_cms_error_to_failed() {
        let lines = ["+CMS ERROR: 500"];
        let outcome = parse_send_outcome(&lines);
        assert_eq!(
            outcome,
            SendOutcome {
                status: MessageStatus::Failed,
                reference: None,
                error_code: Some(500),
            }
        );
    }

    #[test]
    fn send_outcome_maps_cme_error_to_failed() {
        let lines = ["+CME ERROR: 30"];
        let outcome = parse_send_outcome(&lines);
        assert_eq!(outcome.status, MessageStatus::Failed);
        assert_eq!(outcome.error_code, Some(30));
        assert_eq!(outcome.reference, None);
    }

    #[test]
    fn send_outcome_ok_without_reference_is_failed() {
        let outcome = parse_send_outcome(&["OK"]);
        assert_eq!(outcome.status, MessageStatus::Failed);
        assert_eq!(outcome.reference, None);
    }

    #[test]
    fn send_outcome_no_terminator_is_failed() {
        let outcome = parse_send_outcome(&["+CMGS: 7"]);
        assert_eq!(outcome.status, MessageStatus::Failed);
    }

    #[test]
    fn cmgr_round_trips_sender_and_body() {
        let parsed = parse_cmgr(&format_cmgr_response("+14155552671", "hello there")).unwrap();
        assert_eq!(parsed.sender, "+14155552671");
        assert_eq!(parsed.body, "hello there");
    }

    #[test]
    fn cmgr_round_trips_empty_body() {
        let parsed = parse_cmgr(&format_cmgr_response("+14155550000", "")).unwrap();
        assert_eq!(parsed.sender, "+14155550000");
        assert_eq!(parsed.body, "");
    }

    #[test]
    fn cmgr_returns_none_without_header() {
        assert_eq!(parse_cmgr("OK"), None);
        assert_eq!(parse_cmgr("just some text\r\nOK"), None);
    }

    #[test]
    fn cmgr_sender_parsing_ignores_timestamp_comma() {
        let header = "+CMGR: \"REC READ\",\"+441234567\",,\"24/01/02,03:04:05+00\"";
        assert_eq!(parse_cmgr_sender(header), Some("+441234567".to_string()));
    }
}

#[cfg(test)]
mod manager_parsing_tests {
    use super::*;

    #[test]
    fn cmti_index_is_parsed_from_urc() {
        assert_eq!(parse_cmti_index("+CMTI: \"SM\",3"), Some(3));
        assert_eq!(parse_cmti_index("+CMTI: \"ME\",12"), Some(12));
        assert_eq!(parse_cmti_index("  +CMTI: \"SM\",0 \r"), Some(0));
        assert_eq!(parse_cmti_index("OK"), None);
        assert_eq!(parse_cmti_index("+CMGS: 5"), None);
    }

    #[test]
    fn cpin_status_is_classified() {
        assert_eq!(parse_cpin(&["+CPIN: READY".to_string()]), SimStatus::Ready);
        assert_eq!(
            parse_cpin(&["+CPIN: SIM PIN".to_string()]),
            SimStatus::NotReady
        );
        assert_eq!(parse_cpin(&["OK".to_string()]), SimStatus::Unknown);
    }

    #[test]
    fn creg_registration_states() {
        assert!(parse_creg_registered(&["+CREG: 0,1".to_string()]));
        assert!(parse_creg_registered(&["+CREG: 2,5".to_string()]));
        assert!(!parse_creg_registered(&["+CREG: 0,2".to_string()]));
        assert!(!parse_creg_registered(&["+CREG: 0,0".to_string()]));
        assert!(!parse_creg_registered(&["OK".to_string()]));
    }

    #[test]
    fn csq_percent_scales_rssi() {
        assert_eq!(parse_csq_percent(&["+CSQ: 31,99".to_string()]), Some(100));
        assert_eq!(parse_csq_percent(&["+CSQ: 0,99".to_string()]), Some(0));
        assert_eq!(parse_csq_percent(&["+CSQ: 99,99".to_string()]), None);
        assert_eq!(parse_csq_percent(&["OK".to_string()]), None);
    }

    #[test]
    fn cops_operator_is_recovered() {
        assert_eq!(
            parse_cops_operator(&["+COPS: 0,0,\"Test Carrier\"".to_string()]),
            Some("Test Carrier".to_string())
        );
        assert_eq!(parse_cops_operator(&["+COPS: 0".to_string()]), None);
        assert_eq!(parse_cops_operator(&["OK".to_string()]), None);
    }
}
