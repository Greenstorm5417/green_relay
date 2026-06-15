use std::collections::BTreeMap;

use chrono::Utc;
use serde::{Serialize, Serializer};
use serde_json::{Map, Value};

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
    "cookie",
    "set_cookie",
    "session",
];

/// The placeholder string used for redacted credentials.
pub const REDACTED: &str = "[REDACTED]";

const RESERVED_KEYS: &[&str] = &["timestamp", "severity", "message"];

/// Represents log severity levels.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Severity {
    Trace,
    Debug,
    Info,
    Warn,
    Error,
}

impl Severity {
    /// An array of all severity levels.
    pub const ALL: [Severity; 5] = [
        Severity::Trace,
        Severity::Debug,
        Severity::Info,
        Severity::Warn,
        Severity::Error,
    ];

    /// Returns the canonical string representation of the severity.
    pub fn as_str(self) -> &'static str {
        match self {
            Severity::Trace => "TRACE",
            Severity::Debug => "DEBUG",
            Severity::Info => "INFO",
            Severity::Warn => "WARN",
            Severity::Error => "ERROR",
        }
    }

    /// Parses a severity level from a string.
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

/// A structured log record containing metadata and key-value fields.
#[derive(Debug, Clone, PartialEq)]
pub struct LogRecord {
    timestamp: String,
    severity: Severity,
    message: String,

    fields: BTreeMap<String, Value>,
}

/// A borrowed log field value, serialized without cloning the source data.
enum FieldRef<'a> {
    Text(&'a str),
    Json(&'a Value),
}

impl Serialize for FieldRef<'_> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        match self {
            FieldRef::Text(text) => serializer.serialize_str(text),
            FieldRef::Json(value) => value.serialize(serializer),
        }
    }
}

impl LogRecord {
    /// Creates a new log record with the current timestamp.
    pub fn new(severity: Severity, message: impl Into<String>) -> Self {
        Self::with_timestamp(severity, message, Self::now_timestamp())
    }

    /// Creates a new log record with a specific timestamp.
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

    /// Returns the current UTC timestamp formatted as a string.
    pub fn now_timestamp() -> String {
        Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
    }

    /// Adds a custom key-value field to the log record.
    pub fn with_field(mut self, key: impl Into<String>, value: impl Into<Value>) -> Self {
        let key = key.into();
        if !RESERVED_KEYS.contains(&key.as_str()) {
            self.fields.insert(key, value.into());
        }
        self
    }

    /// Returns the severity of the log record.
    pub fn severity(&self) -> Severity {
        self.severity
    }

    /// Returns the message of the log record.
    pub fn message(&self) -> &str {
        &self.message
    }

    /// Returns the timestamp of the log record.
    pub fn timestamp(&self) -> &str {
        &self.timestamp
    }

    /// Returns the value of a specific field if it exists.
    pub fn field(&self, key: &str) -> Option<&Value> {
        self.fields.get(key)
    }

    /// Determines if the record should be emitted based on a minimum severity.
    pub fn should_emit(&self, min_severity: Severity) -> bool {
        self.severity >= min_severity
    }

    /// Converts the log record to a JSON value.
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

    /// Converts the log record to a JSON string.
    pub fn to_json_string(&self) -> String {
        let mut ordered: BTreeMap<&str, FieldRef<'_>> = BTreeMap::new();
        ordered.insert("timestamp", FieldRef::Text(&self.timestamp));
        ordered.insert("severity", FieldRef::Text(self.severity.as_str()));
        ordered.insert("message", FieldRef::Text(&self.message));
        for (key, value) in &self.fields {
            ordered.insert(key.as_str(), FieldRef::Json(value));
        }
        serde_json::to_string(&ordered).unwrap_or_default()
    }
}

/// Creates a log record for an HTTP request.
pub fn request_log(method: &str, path: &str, status: u16) -> LogRecord {
    LogRecord::new(Severity::Info, "request")
        .with_field("method", method.to_string())
        .with_field("path", path.to_string())
        .with_field("status", status as i64)
}

/// Creates a log record for an AT command exchange.
pub fn at_exchange_log(command: &str, result_code: &str) -> LogRecord {
    LogRecord::new(Severity::Debug, "at_exchange")
        .with_field("command", command.to_string())
        .with_field("result", result_code.to_string())
}

/// Creates a log record for an authentication event.
pub fn auth_event_log(key_identifier: &str, outcome: &str) -> LogRecord {
    LogRecord::new(Severity::Info, "auth_event")
        .with_field("key_identifier", key_identifier.to_string())
        .with_field("outcome", outcome.to_string())
}

/// Redacts sensitive fields from a map of fields.
pub fn redact_credentials(fields: &mut BTreeMap<String, Value>) {
    for (key, value) in fields.iter_mut() {
        if is_credential_field(key) {
            *value = Value::String(REDACTED.to_string());
        }
    }
}

/// Checks if a field name matches a known credential pattern.
pub fn is_credential_field(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    CREDENTIAL_FIELDS.contains(&lower.as_str())
}

pub use crate::error::SubscriberInitError;

use tracing_appender::non_blocking::WorkerGuard;
use tracing_appender::rolling::{Builder, RollingFileAppender, Rotation};
use tracing_subscriber::filter::LevelFilter;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

/// How often the on-disk log file is rotated.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum LogRotation {
    /// Roll over every minute.
    Minutely,
    /// Roll over every hour.
    Hourly,
    /// Roll over every day.
    #[default]
    Daily,
    /// Never roll over; write to a single file.
    Never,
}

impl LogRotation {
    /// Parses a rotation policy from a string.
    pub fn parse(s: &str) -> Option<LogRotation> {
        match s.trim().to_ascii_uppercase().as_str() {
            "MINUTELY" => Some(LogRotation::Minutely),
            "HOURLY" => Some(LogRotation::Hourly),
            "DAILY" => Some(LogRotation::Daily),
            "NEVER" => Some(LogRotation::Never),
            _ => None,
        }
    }

    /// Returns the canonical string representation of the rotation policy.
    pub fn as_str(self) -> &'static str {
        match self {
            LogRotation::Minutely => "MINUTELY",
            LogRotation::Hourly => "HOURLY",
            LogRotation::Daily => "DAILY",
            LogRotation::Never => "NEVER",
        }
    }

    fn to_appender(self) -> Rotation {
        match self {
            LogRotation::Minutely => Rotation::MINUTELY,
            LogRotation::Hourly => Rotation::HOURLY,
            LogRotation::Daily => Rotation::DAILY,
            LogRotation::Never => Rotation::NEVER,
        }
    }
}

/// Settings for writing rotating logs to disk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileLogConfig {
    /// Directory the log files are written to.
    pub directory: String,
    /// Filename prefix for each log file.
    pub prefix: String,
    /// How often the log file rotates.
    pub rotation: LogRotation,
    /// Maximum number of rotated files to keep; 0 keeps all of them.
    pub max_files: usize,
}

fn build_file_appender(cfg: &FileLogConfig) -> Result<RollingFileAppender, SubscriberInitError> {
    let mut builder = Builder::new()
        .rotation(cfg.rotation.to_appender())
        .filename_prefix(cfg.prefix.clone())
        .filename_suffix("log");
    if cfg.max_files > 0 {
        builder = builder.max_log_files(cfg.max_files);
    }
    builder
        .build(&cfg.directory)
        .map_err(|e| SubscriberInitError(e.to_string()))
}

/// Initializes the global tracing subscriber.
///
/// Logs are always written as JSON to stdout. When `file` is provided, the same
/// records are also written to a rotating on-disk log through a non-blocking
/// writer; the returned [`WorkerGuard`] must be held for the lifetime of the
/// process so the background flushing thread is not dropped early.
pub fn init_subscriber(
    min_severity: Severity,
    file: Option<&FileLogConfig>,
) -> Result<Option<WorkerGuard>, SubscriberInitError> {
    let max_level: tracing::Level = min_severity.into();
    let filter = LevelFilter::from_level(max_level);

    let stdout_layer = tracing_subscriber::fmt::layer()
        .json()
        .with_current_span(false)
        .with_span_list(false)
        .with_writer(std::io::stdout);

    match file {
        None => {
            tracing_subscriber::registry()
                .with(filter)
                .with(stdout_layer)
                .try_init()
                .map_err(|e| SubscriberInitError(e.to_string()))?;
            Ok(None)
        }
        Some(cfg) => {
            let appender = build_file_appender(cfg)?;
            let (writer, guard) = tracing_appender::non_blocking(appender);
            let file_layer = tracing_subscriber::fmt::layer()
                .json()
                .with_ansi(false)
                .with_current_span(false)
                .with_span_list(false)
                .with_writer(writer);
            tracing_subscriber::registry()
                .with(filter)
                .with(stdout_layer)
                .with(file_layer)
                .try_init()
                .map_err(|e| SubscriberInitError(e.to_string()))?;
            Ok(Some(guard))
        }
    }
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
