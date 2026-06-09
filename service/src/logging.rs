//! Structured logging: `tracing` JSON subscriber and log-record builders.
//!
//! This module provides two things:
//!
//! 1. [`init_subscriber`] — installs a `tracing` JSON subscriber that writes to
//!    stdout (so the systemd journal captures it, Req 11.6) with a configurable
//!    minimum severity (Req 7.4, 7.5).
//! 2. A pure, testable [`LogRecord`] builder. Every record carries a timestamp
//!    (calendar date + time-of-day), a [`Severity`] in
//!    `{TRACE, DEBUG, INFO, WARN, ERROR}`, and a non-empty message (Req 7.1).
//!    Domain helpers build request logs (method, path, status — Req 7.2) and
//!    AT-exchange logs (command, result code — Req 7.3). Auth-event helpers
//!    redact credential fields so plaintext keys/passwords never reach a record
//!    (Req 7.6, 7.7).
//!
//! The builder types are public so the property tests (tasks 2.2–2.4) can
//! assert well-formedness, domain-field presence, and severity filtering
//! without standing up a live subscriber.

use std::collections::BTreeMap;

use chrono::Utc;
use serde_json::{Map, Value};

/// Field names whose values are treated as credentials and never emitted in
/// plaintext on an auth-event record (Req 7.6, 7.7).
const CREDENTIAL_FIELDS: &[&str] = &[
    "api_key",
    "apikey",
    "key",
    "password",
    "passwd",
    "secret",
    "credential",
    "credentials",
    "authorization",
    "token",
];

/// Marker substituted for a redacted credential value.
pub const REDACTED: &str = "[REDACTED]";

/// Reserved top-level keys that domain fields may not override.
const RESERVED_KEYS: &[&str] = &["timestamp", "severity", "message"];

/// Log severity levels (Req 7.1). Ordering is ascending by importance:
/// `Trace < Debug < Info < Warn < Error`, which drives severity filtering
/// (Req 7.4, 7.5).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Severity {
    Trace,
    Debug,
    Info,
    Warn,
    Error,
}

impl Severity {
    /// All severities, ascending. Useful for exhaustive testing.
    pub const ALL: [Severity; 5] = [
        Severity::Trace,
        Severity::Debug,
        Severity::Info,
        Severity::Warn,
        Severity::Error,
    ];

    /// The canonical uppercase string for this severity, exactly one of
    /// `TRACE`, `DEBUG`, `INFO`, `WARN`, `ERROR` (Req 7.1).
    pub fn as_str(self) -> &'static str {
        match self {
            Severity::Trace => "TRACE",
            Severity::Debug => "DEBUG",
            Severity::Info => "INFO",
            Severity::Warn => "WARN",
            Severity::Error => "ERROR",
        }
    }

    /// Parse a severity from its canonical name (case-insensitive). Used by the
    /// configuration loader to map the configured minimum level.
    pub fn parse(s: &str) -> Option<Severity> {
        match s.trim().to_ascii_uppercase().as_str() {
            "TRACE" => Some(Severity::Trace),
            "DEBUG" => Some(Severity::Debug),
            "INFO" => Some(Severity::Info),
            "WARN" | "WARNING" => Some(Severity::Warn),
            "ERROR" => Some(Severity::Error),
            _ => None,
        }
    }
}

impl From<Severity> for tracing::Level {
    fn from(s: Severity) -> tracing::Level {
        match s {
            Severity::Trace => tracing::Level::TRACE,
            Severity::Debug => tracing::Level::DEBUG,
            Severity::Info => tracing::Level::INFO,
            Severity::Warn => tracing::Level::WARN,
            Severity::Error => tracing::Level::ERROR,
        }
    }
}

/// A structured log record (Req 7.1). Holds a timestamp, a severity, a message,
/// and any number of additional domain fields. Render it to JSON with
/// [`LogRecord::to_json_value`] / [`LogRecord::to_json_string`].
#[derive(Debug, Clone, PartialEq)]
pub struct LogRecord {
    /// RFC 3339 / ISO 8601 timestamp including calendar date and time-of-day,
    /// e.g. `2024-01-15T12:34:56.789Z`.
    timestamp: String,
    severity: Severity,
    message: String,
    /// Extra structured fields keyed by name. Kept in a `BTreeMap` for stable,
    /// deterministic JSON ordering.
    fields: BTreeMap<String, Value>,
}

impl LogRecord {
    /// Build a record with the current UTC time as its timestamp. The message
    /// is stored as given; callers are expected to supply a non-empty message
    /// (Req 7.1).
    pub fn new(severity: Severity, message: impl Into<String>) -> Self {
        Self::with_timestamp(severity, message, Self::now_timestamp())
    }

    /// Build a record with an explicit timestamp string. Primarily for tests
    /// that need deterministic output.
    pub fn with_timestamp(
        severity: Severity,
        message: impl Into<String>,
        timestamp: impl Into<String>,
    ) -> Self {
        LogRecord {
            timestamp: timestamp.into(),
            severity,
            message: message.into(),
            fields: BTreeMap::new(),
        }
    }

    /// Current UTC timestamp formatted with both date and time-of-day.
    pub fn now_timestamp() -> String {
        Utc::now().format("%Y-%m-%dT%H:%M:%S%.3fZ").to_string()
    }

    /// Attach an arbitrary structured field. Reserved keys are ignored.
    pub fn with_field(mut self, key: impl Into<String>, value: impl Into<Value>) -> Self {
        let key = key.into();
        if !RESERVED_KEYS.contains(&key.as_str()) {
            self.fields.insert(key, value.into());
        }
        self
    }

    /// The record's severity.
    pub fn severity(&self) -> Severity {
        self.severity
    }

    /// The record's message.
    pub fn message(&self) -> &str {
        &self.message
    }

    /// The record's timestamp string.
    pub fn timestamp(&self) -> &str {
        &self.timestamp
    }

    /// Look up an attached field by name.
    pub fn field(&self, key: &str) -> Option<&Value> {
        self.fields.get(key)
    }

    /// Whether this record should be emitted given a minimum severity.
    pub fn should_emit(&self, min_severity: Severity) -> bool {
        self.severity >= min_severity
    }

    /// Render the record to a JSON object value.
    pub fn to_json_value(&self) -> Value {
        let mut map = Map::new();
        map.insert(
            "timestamp".to_string(),
            Value::String(self.timestamp.clone()),
        );
        map.insert(
            "severity".to_string(),
            Value::String(self.severity.as_str().to_string()),
        );
        map.insert("message".to_string(), Value::String(self.message.clone()));
        for (k, v) in &self.fields {
            map.insert(k.clone(), v.clone());
        }
        Value::Object(map)
    }

    /// Render the record to a compact JSON string.
    pub fn to_json_string(&self) -> String {
        // Serializing a `serde_json::Value` cannot fail.
        self.to_json_value().to_string()
    }
}

/// Build a request log record carrying the HTTP method, path, and resulting
/// status code (Req 7.2).
pub fn request_log(method: &str, path: &str, status: u16) -> LogRecord {
    LogRecord::new(Severity::Info, "request")
        .with_field("method", method.to_string())
        .with_field("path", path.to_string())
        .with_field("status", status as i64)
}

/// Build an AT-exchange log record carrying the issued command and the result
/// code returned by the modem (Req 7.3).
pub fn at_exchange_log(command: &str, result_code: &str) -> LogRecord {
    LogRecord::new(Severity::Debug, "at_exchange")
        .with_field("command", command.to_string())
        .with_field("result", result_code.to_string())
}

/// Build an authentication-event log record.
pub fn auth_event_log(key_identifier: &str, outcome: &str) -> LogRecord {
    LogRecord::new(Severity::Info, "auth_event")
        .with_field("key_identifier", key_identifier.to_string())
        .with_field("outcome", outcome.to_string())
}

/// Redact credential-named fields from a field map in place.
pub fn redact_credentials(fields: &mut BTreeMap<String, Value>) {
    for (key, value) in fields.iter_mut() {
        if is_credential_field(key) {
            *value = Value::String(REDACTED.to_string());
        }
    }
}

/// Whether a field name denotes a credential that must be redacted.
pub fn is_credential_field(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    CREDENTIAL_FIELDS.contains(&lower.as_str())
}

/// Error returned when the global tracing subscriber cannot be installed.
#[derive(Debug)]
pub struct SubscriberInitError(pub String);

impl std::fmt::Display for SubscriberInitError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "failed to initialize logging subscriber: {}", self.0)
    }
}

impl std::error::Error for SubscriberInitError {}

/// Initialize the global `tracing` JSON subscriber writing to stdout.
///
/// Uses `try_init` so repeated initialization returns an error rather than panicking.
pub fn init_subscriber(min_severity: Severity) -> Result<(), SubscriberInitError> {
    let max_level: tracing::Level = min_severity.into();
    tracing_subscriber::fmt()
        .json()
        .with_writer(std::io::stdout)
        .with_max_level(max_level)
        .with_current_span(false)
        .with_span_list(false)
        .try_init()
        .map_err(|e| SubscriberInitError(e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn severity_strings_are_canonical() {
        assert_eq!(Severity::Trace.as_str(), "TRACE");
        assert_eq!(Severity::Debug.as_str(), "DEBUG");
        assert_eq!(Severity::Info.as_str(), "INFO");
        assert_eq!(Severity::Warn.as_str(), "WARN");
        assert_eq!(Severity::Error.as_str(), "ERROR");
    }

    #[test]
    fn severity_ordering_is_ascending_by_importance() {
        assert!(Severity::Trace < Severity::Debug);
        assert!(Severity::Debug < Severity::Info);
        assert!(Severity::Info < Severity::Warn);
        assert!(Severity::Warn < Severity::Error);
    }

    #[test]
    fn severity_parse_round_trips_and_handles_aliases() {
        for s in Severity::ALL {
            assert_eq!(Severity::parse(s.as_str()), Some(s));
        }
        assert_eq!(Severity::parse("warning"), Some(Severity::Warn));
        assert_eq!(Severity::parse("info"), Some(Severity::Info));
        assert_eq!(Severity::parse("nonsense"), None);
    }

    #[test]
    fn record_renders_core_fields() {
        let record = LogRecord::new(Severity::Info, "hello");
        let value = record.to_json_value();
        assert_eq!(value["severity"], "INFO");
        assert_eq!(value["message"], "hello");
        let ts = value["timestamp"].as_str().unwrap();
        assert!(!ts.is_empty());
        // Includes both date and time-of-day separated by 'T'.
        assert!(ts.contains('T'));
        assert!(ts.contains('-'));
        assert!(ts.contains(':'));
    }

    #[test]
    fn record_json_parses_back() {
        let record = LogRecord::new(Severity::Warn, "careful");
        let parsed: Value = serde_json::from_str(&record.to_json_string()).unwrap();
        assert_eq!(parsed["severity"], "WARN");
        assert_eq!(parsed["message"], "careful");
    }

    #[test]
    fn reserved_fields_cannot_be_overridden() {
        let record = LogRecord::new(Severity::Info, "msg")
            .with_field("message", "evil")
            .with_field("severity", "evil")
            .with_field("timestamp", "evil");
        let value = record.to_json_value();
        assert_eq!(value["message"], "msg");
        assert_eq!(value["severity"], "INFO");
        assert_ne!(value["timestamp"], "evil");
    }

    #[test]
    fn request_log_carries_method_path_status() {
        let record = request_log("POST", "/api/v1/messages", 202);
        assert_eq!(record.field("method").unwrap(), "POST");
        assert_eq!(record.field("path").unwrap(), "/api/v1/messages");
        assert_eq!(record.field("status").unwrap(), 202);
    }

    #[test]
    fn at_exchange_log_carries_command_and_result() {
        let record = at_exchange_log("AT+CMGS", "+CMGS: 42");
        assert_eq!(record.field("command").unwrap(), "AT+CMGS");
        assert_eq!(record.field("result").unwrap(), "+CMGS: 42");
    }

    #[test]
    fn auth_event_log_contains_identifier_not_plaintext() {
        let plaintext = "super-secret-key";
        let identifier = "abc123hash";
        let record = auth_event_log(identifier, "authorized");
        let json = record.to_json_string();
        assert!(json.contains(identifier));
        assert!(!json.contains(plaintext));
    }

    #[test]
    fn redact_credentials_masks_credential_fields_only() {
        let mut fields = BTreeMap::new();
        fields.insert("api_key".to_string(), Value::String("plaintext".into()));
        fields.insert("password".to_string(), Value::String("hunter2".into()));
        fields.insert("path".to_string(), Value::String("/login".into()));
        redact_credentials(&mut fields);
        assert_eq!(fields["api_key"], Value::String(REDACTED.into()));
        assert_eq!(fields["password"], Value::String(REDACTED.into()));
        assert_eq!(fields["path"], Value::String("/login".into()));
    }

    #[test]
    fn should_emit_respects_minimum_severity() {
        let warn = LogRecord::new(Severity::Warn, "w");
        assert!(warn.should_emit(Severity::Trace));
        assert!(warn.should_emit(Severity::Warn));
        assert!(!warn.should_emit(Severity::Error));
    }
}
