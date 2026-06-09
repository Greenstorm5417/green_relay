//! Configuration loader.
//!
//! Loads configuration from a file and environment variables, with
//! environment values overriding file values (Req 11.1). Required values are
//! validated at startup; on a missing or invalid value `load` returns a
//! [`ConfigError`] that names the specific offending key (Req 11.5).
//!
//! The module separates three concerns so they can be tested in isolation:
//! - [`merge_env_over_file`] — a pure merge of a file-sourced map and an
//!   environment-sourced map (env wins). Validated by Property 31.
//! - [`Config::from_map`] — pure validation/typing of a merged string map
//!   into a [`Config`], producing key-named errors. Validated by unit tests.
//! - [`load`] — the impure wiring that reads the file + process environment
//!   and feeds them through the two pure functions above.

use std::collections::HashMap;
use std::fmt;
use std::net::SocketAddr;
use std::str::FromStr;

/// Environment variable / config-file key names.
pub const KEY_LISTEN_ADDR: &str = "LISTEN_ADDR";
pub const KEY_SERIAL_PORT: &str = "SERIAL_PORT";
pub const KEY_BAUD_RATE: &str = "BAUD_RATE";
pub const KEY_DATABASE_PATH: &str = "DATABASE_PATH";
pub const KEY_SERVICE_CENTER_NUMBER: &str = "SERVICE_CENTER_NUMBER";
pub const KEY_AT_TIMEOUT_SECS: &str = "AT_TIMEOUT_SECS";
pub const KEY_DEFAULT_RATE_LIMIT: &str = "DEFAULT_RATE_LIMIT";
pub const KEY_RATE_WINDOW_SECS: &str = "RATE_WINDOW_SECS";
pub const KEY_LOG_LEVEL: &str = "LOG_LEVEL";
pub const KEY_REOPEN_MAX_ATTEMPTS: &str = "REOPEN_MAX_ATTEMPTS";
pub const KEY_SEND_MAX_ATTEMPTS: &str = "SEND_MAX_ATTEMPTS";
pub const KEY_SEND_RETRY_DELAY_SECS: &str = "SEND_RETRY_DELAY_SECS";

/// The environment variable naming the path to the configuration file.
pub const KEY_CONFIG_FILE: &str = "SMS_CONFIG_FILE";

/// Default configuration file path used when [`KEY_CONFIG_FILE`] is unset.
pub const DEFAULT_CONFIG_FILE: &str = "/etc/sms-microservice/config";

/// All configuration keys the loader recognizes from the environment.
pub const KNOWN_KEYS: &[&str] = &[
    KEY_LISTEN_ADDR,
    KEY_SERIAL_PORT,
    KEY_BAUD_RATE,
    KEY_DATABASE_PATH,
    KEY_SERVICE_CENTER_NUMBER,
    KEY_AT_TIMEOUT_SECS,
    KEY_DEFAULT_RATE_LIMIT,
    KEY_RATE_WINDOW_SECS,
    KEY_LOG_LEVEL,
    KEY_REOPEN_MAX_ATTEMPTS,
    KEY_SEND_MAX_ATTEMPTS,
    KEY_SEND_RETRY_DELAY_SECS,
];

/// Minimum emitted log severity level.
///
/// Defined here because it is part of the validated [`Config`]; the logging
/// module consumes it when initializing the subscriber.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogLevel {
    Trace,
    Debug,
    Info,
    Warn,
    Error,
}

impl Default for LogLevel {
    fn default() -> Self {
        LogLevel::Info
    }
}

impl LogLevel {
    /// Parse a severity name case-insensitively. Returns `None` for any value
    /// that is not exactly one of the five supported levels.
    pub fn parse(s: &str) -> Option<LogLevel> {
        match s.trim().to_ascii_uppercase().as_str() {
            "TRACE" => Some(LogLevel::Trace),
            "DEBUG" => Some(LogLevel::Debug),
            "INFO" => Some(LogLevel::Info),
            "WARN" => Some(LogLevel::Warn),
            "ERROR" => Some(LogLevel::Error),
            _ => None,
        }
    }

    /// The canonical uppercase name of this level.
    pub fn as_str(&self) -> &'static str {
        match self {
            LogLevel::Trace => "TRACE",
            LogLevel::Debug => "DEBUG",
            LogLevel::Info => "INFO",
            LogLevel::Warn => "WARN",
            LogLevel::Error => "ERROR",
        }
    }
}

/// Fully validated service configuration. See `design.md` §1.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    pub listen_addr: SocketAddr,
    pub serial_port: String,
    pub baud_rate: u32,
    pub database_path: String,
    pub service_center_number: Option<String>,
    pub at_timeout_secs: u64,
    pub default_rate_limit: u32,
    pub rate_window_secs: u64,
    pub log_level: LogLevel,
    pub reopen_max_attempts: u32,
    pub send_max_attempts: u32,
    pub send_retry_delay_secs: u64,
}

/// An error produced while loading or validating configuration. Every variant
/// carries the specific offending key so startup can report it (Req 11.5).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfigError {
    /// A required key was absent or empty.
    MissingKey(String),
    /// A key was present but its value could not be parsed or was out of range.
    InvalidValue {
        key: String,
        value: String,
        reason: String,
    },
    /// The configuration file existed but could not be read or parsed.
    FileRead { path: String, reason: String },
}

impl ConfigError {
    /// The configuration key this error concerns, when applicable.
    pub fn key(&self) -> Option<&str> {
        match self {
            ConfigError::MissingKey(k) => Some(k),
            ConfigError::InvalidValue { key, .. } => Some(key),
            ConfigError::FileRead { .. } => None,
        }
    }
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ConfigError::MissingKey(key) => {
                write!(f, "missing required configuration value: {key}")
            }
            ConfigError::InvalidValue { key, value, reason } => {
                write!(
                    f,
                    "invalid configuration value for {key} (`{value}`): {reason}"
                )
            }
            ConfigError::FileRead { path, reason } => {
                write!(f, "failed to read configuration file {path}: {reason}")
            }
        }
    }
}

impl std::error::Error for ConfigError {}

/// Merge a file-sourced configuration map with an environment-sourced map,
/// letting the environment win on conflicts (Req 11.1).
///
/// The result contains the union of keys from both maps. For any key present
/// in `env`, the merged value is the environment value; otherwise it is the
/// file value. This is a pure function with no I/O — see Property 31.
pub fn merge_env_over_file(
    file: &HashMap<String, String>,
    env: &HashMap<String, String>,
) -> HashMap<String, String> {
    let mut merged = file.clone();
    for (key, value) in env {
        merged.insert(key.clone(), value.clone());
    }
    merged
}

/// Parse the contents of a configuration file into a key/value map.
///
/// The format is simple line-oriented `KEY = VALUE`:
/// - blank lines and lines whose first non-whitespace character is `#` are
///   ignored,
/// - the key and value are split on the first `=`,
/// - surrounding whitespace is trimmed from both key and value,
/// - a single layer of matching single or double quotes around the value is
///   stripped.
pub fn parse_config_file(contents: &str) -> HashMap<String, String> {
    let mut map = HashMap::new();
    for line in contents.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        let Some((raw_key, raw_value)) = trimmed.split_once('=') else {
            continue;
        };
        let key = raw_key.trim();
        if key.is_empty() {
            continue;
        }
        let value = strip_quotes(raw_value.trim());
        map.insert(key.to_string(), value.to_string());
    }
    map
}

/// Strip one layer of matching surrounding single or double quotes.
fn strip_quotes(s: &str) -> &str {
    let bytes = s.as_bytes();
    if bytes.len() >= 2 {
        let first = bytes[0];
        let last = bytes[bytes.len() - 1];
        if (first == b'"' && last == b'"') || (first == b'\'' && last == b'\'') {
            return &s[1..s.len() - 1];
        }
    }
    s
}

impl Config {
    /// Build and validate a [`Config`] from an already-merged string map.
    ///
    /// Pure: performs no I/O. Required keys that are absent yield
    /// [`ConfigError::MissingKey`]; present-but-unparseable or out-of-range
    /// values yield [`ConfigError::InvalidValue`]. Either way the offending
    /// key is named (Req 11.5). Optional keys fall back to their documented
    /// defaults when absent or empty.
    pub fn from_map(map: &HashMap<String, String>) -> Result<Config, ConfigError> {
        let listen_addr_raw = require(map, KEY_LISTEN_ADDR)?;
        let listen_addr = SocketAddr::from_str(listen_addr_raw).map_err(|e| {
            ConfigError::InvalidValue {
                key: KEY_LISTEN_ADDR.to_string(),
                value: listen_addr_raw.to_string(),
                reason: format!("expected a socket address like 0.0.0.0:8080 ({e})"),
            }
        })?;

        let serial_port = optional_string(map, KEY_SERIAL_PORT)
            .unwrap_or_else(|| "/dev/ttyUSB2".to_string());

        let baud_rate = parse_u32_or_default(map, KEY_BAUD_RATE, 115_200)?;
        if baud_rate == 0 {
            return Err(out_of_range(map, KEY_BAUD_RATE, "must be greater than 0"));
        }

        let database_path = require(map, KEY_DATABASE_PATH)?.to_string();

        let service_center_number = optional_string(map, KEY_SERVICE_CENTER_NUMBER);

        let at_timeout_secs = parse_u64_or_default(map, KEY_AT_TIMEOUT_SECS, 5)?;
        if !(1..=60).contains(&at_timeout_secs) {
            return Err(out_of_range(
                map,
                KEY_AT_TIMEOUT_SECS,
                "must be between 1 and 60 seconds",
            ));
        }

        let default_rate_limit = parse_u32_or_default(map, KEY_DEFAULT_RATE_LIMIT, 100)?;
        if !(1..=10_000).contains(&default_rate_limit) {
            return Err(out_of_range(
                map,
                KEY_DEFAULT_RATE_LIMIT,
                "must be between 1 and 10000 requests",
            ));
        }

        let rate_window_secs = parse_u64_or_default(map, KEY_RATE_WINDOW_SECS, 60)?;
        if rate_window_secs == 0 {
            return Err(out_of_range(
                map,
                KEY_RATE_WINDOW_SECS,
                "must be greater than 0",
            ));
        }

        let log_level = match optional_string(map, KEY_LOG_LEVEL) {
            None => LogLevel::default(),
            Some(raw) => LogLevel::parse(&raw).ok_or_else(|| ConfigError::InvalidValue {
                key: KEY_LOG_LEVEL.to_string(),
                value: raw,
                reason: "must be one of TRACE, DEBUG, INFO, WARN, ERROR".to_string(),
            })?,
        };

        let reopen_max_attempts = parse_u32_or_default(map, KEY_REOPEN_MAX_ATTEMPTS, 10)?;
        if reopen_max_attempts == 0 {
            return Err(out_of_range(
                map,
                KEY_REOPEN_MAX_ATTEMPTS,
                "must be at least 1",
            ));
        }

        let send_max_attempts = parse_u32_or_default(map, KEY_SEND_MAX_ATTEMPTS, 3)?;
        if send_max_attempts == 0 {
            return Err(out_of_range(
                map,
                KEY_SEND_MAX_ATTEMPTS,
                "must be at least 1",
            ));
        }

        let send_retry_delay_secs = parse_u64_or_default(map, KEY_SEND_RETRY_DELAY_SECS, 5)?;

        Ok(Config {
            listen_addr,
            serial_port,
            baud_rate,
            database_path,
            service_center_number,
            at_timeout_secs,
            default_rate_limit,
            rate_window_secs,
            log_level,
            reopen_max_attempts,
            send_max_attempts,
            send_retry_delay_secs,
        })
    }
}

/// Fetch a required, non-empty value or produce [`ConfigError::MissingKey`].
fn require<'a>(map: &'a HashMap<String, String>, key: &str) -> Result<&'a str, ConfigError> {
    match map.get(key) {
        Some(v) if !v.trim().is_empty() => Ok(v.as_str()),
        _ => Err(ConfigError::MissingKey(key.to_string())),
    }
}

/// Fetch an optional string value, treating absent or empty as `None`.
fn optional_string(map: &HashMap<String, String>, key: &str) -> Option<String> {
    map.get(key)
        .map(|v| v.trim())
        .filter(|v| !v.is_empty())
        .map(|v| v.to_string())
}

/// Parse a `u32` value, falling back to `default` when absent or empty.
fn parse_u32_or_default(
    map: &HashMap<String, String>,
    key: &str,
    default: u32,
) -> Result<u32, ConfigError> {
    match optional_string(map, key) {
        None => Ok(default),
        Some(raw) => raw.parse::<u32>().map_err(|_| ConfigError::InvalidValue {
            key: key.to_string(),
            value: raw,
            reason: "expected a non-negative integer".to_string(),
        }),
    }
}

/// Parse a `u64` value, falling back to `default` when absent or empty.
fn parse_u64_or_default(
    map: &HashMap<String, String>,
    key: &str,
    default: u64,
) -> Result<u64, ConfigError> {
    match optional_string(map, key) {
        None => Ok(default),
        Some(raw) => raw.parse::<u64>().map_err(|_| ConfigError::InvalidValue {
            key: key.to_string(),
            value: raw,
            reason: "expected a non-negative integer".to_string(),
        }),
    }
}

/// Build an [`ConfigError::InvalidValue`] for an out-of-range numeric value.
fn out_of_range(map: &HashMap<String, String>, key: &str, reason: &str) -> ConfigError {
    ConfigError::InvalidValue {
        key: key.to_string(),
        value: map.get(key).cloned().unwrap_or_default(),
        reason: reason.to_string(),
    }
}

/// Collect the recognized configuration keys from the process environment.
fn env_map() -> HashMap<String, String> {
    let mut map = HashMap::new();
    for key in KNOWN_KEYS {
        if let Ok(value) = std::env::var(key) {
            map.insert((*key).to_string(), value);
        }
    }
    map
}

/// Read the configuration file referenced by [`KEY_CONFIG_FILE`] (or the
/// default path) into a key/value map.
///
/// A missing file at the default path is treated as an empty map, since the
/// environment alone may supply all required values. If the path was set
/// explicitly via [`KEY_CONFIG_FILE`] but cannot be read, that is reported as
/// a [`ConfigError::FileRead`].
fn file_map() -> Result<HashMap<String, String>, ConfigError> {
    let explicit = std::env::var(KEY_CONFIG_FILE).ok();
    let path = explicit
        .clone()
        .unwrap_or_else(|| DEFAULT_CONFIG_FILE.to_string());

    match std::fs::read_to_string(&path) {
        Ok(contents) => Ok(parse_config_file(&contents)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound && explicit.is_none() => {
            Ok(HashMap::new())
        }
        Err(e) => Err(ConfigError::FileRead {
            path,
            reason: e.to_string(),
        }),
    }
}

/// Load configuration from the config file and process environment, with
/// environment values overriding file values, then validate it (Req 11.1,
/// 11.5).
pub fn load() -> Result<Config, ConfigError> {
    let file = file_map()?;
    let env = env_map();
    let merged = merge_env_over_file(&file, &env);
    Config::from_map(&merged)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a minimal map containing only the two required keys
    /// (`LISTEN_ADDR` and `DATABASE_PATH`). Every other key is optional and
    /// falls back to its documented default.
    fn minimal_valid_map() -> HashMap<String, String> {
        let mut map = HashMap::new();
        map.insert(KEY_LISTEN_ADDR.to_string(), "127.0.0.1:8080".to_string());
        map.insert(KEY_DATABASE_PATH.to_string(), "/var/lib/sms.db".to_string());
        map
    }

    /// Build a map that sets every recognized key to a valid value so we can
    /// assert the full typed result of `from_map`.
    fn complete_valid_map() -> HashMap<String, String> {
        let mut map = HashMap::new();
        map.insert(KEY_LISTEN_ADDR.to_string(), "0.0.0.0:9000".to_string());
        map.insert(KEY_SERIAL_PORT.to_string(), "/dev/ttyUSB3".to_string());
        map.insert(KEY_BAUD_RATE.to_string(), "57600".to_string());
        map.insert(KEY_DATABASE_PATH.to_string(), "/data/sms.db".to_string());
        map.insert(
            KEY_SERVICE_CENTER_NUMBER.to_string(),
            "+14155550000".to_string(),
        );
        map.insert(KEY_AT_TIMEOUT_SECS.to_string(), "10".to_string());
        map.insert(KEY_DEFAULT_RATE_LIMIT.to_string(), "250".to_string());
        map.insert(KEY_RATE_WINDOW_SECS.to_string(), "30".to_string());
        map.insert(KEY_LOG_LEVEL.to_string(), "debug".to_string());
        map.insert(KEY_REOPEN_MAX_ATTEMPTS.to_string(), "5".to_string());
        map.insert(KEY_SEND_MAX_ATTEMPTS.to_string(), "4".to_string());
        map.insert(KEY_SEND_RETRY_DELAY_SECS.to_string(), "7".to_string());
        map
    }

    // --- Missing required keys (Req 11.5) ---------------------------------

    #[test]
    fn missing_listen_addr_reports_that_key() {
        let mut map = minimal_valid_map();
        map.remove(KEY_LISTEN_ADDR);

        let err = Config::from_map(&map).expect_err("missing LISTEN_ADDR should fail");

        assert_eq!(err, ConfigError::MissingKey(KEY_LISTEN_ADDR.to_string()));
        assert_eq!(err.key(), Some(KEY_LISTEN_ADDR));
    }

    #[test]
    fn missing_database_path_reports_that_key() {
        let mut map = minimal_valid_map();
        map.remove(KEY_DATABASE_PATH);

        let err = Config::from_map(&map).expect_err("missing DATABASE_PATH should fail");

        assert_eq!(err, ConfigError::MissingKey(KEY_DATABASE_PATH.to_string()));
        assert_eq!(err.key(), Some(KEY_DATABASE_PATH));
    }

    #[test]
    fn empty_required_value_is_treated_as_missing() {
        let mut map = minimal_valid_map();
        map.insert(KEY_DATABASE_PATH.to_string(), "   ".to_string());

        let err = Config::from_map(&map).expect_err("blank DATABASE_PATH should fail");

        assert_eq!(err, ConfigError::MissingKey(KEY_DATABASE_PATH.to_string()));
        assert_eq!(err.key(), Some(KEY_DATABASE_PATH));
    }

    // --- Out-of-range values name the offending key (Req 11.5) ------------

    #[test]
    fn at_timeout_below_range_reports_that_key() {
        let mut map = minimal_valid_map();
        map.insert(KEY_AT_TIMEOUT_SECS.to_string(), "0".to_string());

        let err = Config::from_map(&map).expect_err("AT_TIMEOUT_SECS=0 should fail");

        assert_eq!(err.key(), Some(KEY_AT_TIMEOUT_SECS));
        assert!(matches!(err, ConfigError::InvalidValue { .. }));
    }

    #[test]
    fn at_timeout_above_range_reports_that_key() {
        let mut map = minimal_valid_map();
        map.insert(KEY_AT_TIMEOUT_SECS.to_string(), "61".to_string());

        let err = Config::from_map(&map).expect_err("AT_TIMEOUT_SECS=61 should fail");

        assert_eq!(err.key(), Some(KEY_AT_TIMEOUT_SECS));
        match err {
            ConfigError::InvalidValue { value, .. } => assert_eq!(value, "61"),
            other => panic!("expected InvalidValue, got {other:?}"),
        }
    }

    #[test]
    fn default_rate_limit_zero_reports_that_key() {
        let mut map = minimal_valid_map();
        map.insert(KEY_DEFAULT_RATE_LIMIT.to_string(), "0".to_string());

        let err = Config::from_map(&map).expect_err("DEFAULT_RATE_LIMIT=0 should fail");

        assert_eq!(err.key(), Some(KEY_DEFAULT_RATE_LIMIT));
        assert!(matches!(err, ConfigError::InvalidValue { .. }));
    }

    #[test]
    fn default_rate_limit_above_range_reports_that_key() {
        let mut map = minimal_valid_map();
        map.insert(KEY_DEFAULT_RATE_LIMIT.to_string(), "10001".to_string());

        let err = Config::from_map(&map).expect_err("DEFAULT_RATE_LIMIT=10001 should fail");

        assert_eq!(err.key(), Some(KEY_DEFAULT_RATE_LIMIT));
        match err {
            ConfigError::InvalidValue { value, .. } => assert_eq!(value, "10001"),
            other => panic!("expected InvalidValue, got {other:?}"),
        }
    }

    #[test]
    fn baud_rate_zero_reports_that_key() {
        let mut map = minimal_valid_map();
        map.insert(KEY_BAUD_RATE.to_string(), "0".to_string());

        let err = Config::from_map(&map).expect_err("BAUD_RATE=0 should fail");

        assert_eq!(err.key(), Some(KEY_BAUD_RATE));
        assert!(matches!(err, ConfigError::InvalidValue { .. }));
    }

    // --- Unparseable values name the offending key (Req 11.5) -------------

    #[test]
    fn non_numeric_baud_rate_reports_that_key() {
        let mut map = minimal_valid_map();
        map.insert(KEY_BAUD_RATE.to_string(), "fast".to_string());

        let err = Config::from_map(&map).expect_err("non-numeric BAUD_RATE should fail");

        assert_eq!(err.key(), Some(KEY_BAUD_RATE));
        match err {
            ConfigError::InvalidValue { value, .. } => assert_eq!(value, "fast"),
            other => panic!("expected InvalidValue, got {other:?}"),
        }
    }

    #[test]
    fn unparseable_listen_addr_reports_that_key() {
        let mut map = minimal_valid_map();
        map.insert(KEY_LISTEN_ADDR.to_string(), "not-an-address".to_string());

        let err = Config::from_map(&map).expect_err("bad LISTEN_ADDR should fail");

        assert_eq!(err.key(), Some(KEY_LISTEN_ADDR));
        match err {
            ConfigError::InvalidValue { value, .. } => assert_eq!(value, "not-an-address"),
            other => panic!("expected InvalidValue, got {other:?}"),
        }
    }

    #[test]
    fn invalid_log_level_reports_that_key() {
        let mut map = minimal_valid_map();
        map.insert(KEY_LOG_LEVEL.to_string(), "verbose".to_string());

        let err = Config::from_map(&map).expect_err("bad LOG_LEVEL should fail");

        assert_eq!(err.key(), Some(KEY_LOG_LEVEL));
        assert!(matches!(err, ConfigError::InvalidValue { .. }));
    }

    // --- Valid maps parse successfully ------------------------------------

    #[test]
    fn minimal_map_uses_documented_defaults() {
        let config = Config::from_map(&minimal_valid_map()).expect("minimal map should parse");

        assert_eq!(config.listen_addr, "127.0.0.1:8080".parse().unwrap());
        assert_eq!(config.database_path, "/var/lib/sms.db");
        assert_eq!(config.serial_port, "/dev/ttyUSB2");
        assert_eq!(config.baud_rate, 115_200);
        assert_eq!(config.service_center_number, None);
        assert_eq!(config.at_timeout_secs, 5);
        assert_eq!(config.default_rate_limit, 100);
        assert_eq!(config.rate_window_secs, 60);
        assert_eq!(config.log_level, LogLevel::Info);
        assert_eq!(config.reopen_max_attempts, 10);
        assert_eq!(config.send_max_attempts, 3);
        assert_eq!(config.send_retry_delay_secs, 5);
    }

    #[test]
    fn complete_map_parses_to_expected_typed_values() {
        let config = Config::from_map(&complete_valid_map()).expect("complete map should parse");

        assert_eq!(config.listen_addr, "0.0.0.0:9000".parse().unwrap());
        assert_eq!(config.serial_port, "/dev/ttyUSB3");
        assert_eq!(config.baud_rate, 57_600);
        assert_eq!(config.database_path, "/data/sms.db");
        assert_eq!(
            config.service_center_number,
            Some("+14155550000".to_string())
        );
        assert_eq!(config.at_timeout_secs, 10);
        assert_eq!(config.default_rate_limit, 250);
        assert_eq!(config.rate_window_secs, 30);
        assert_eq!(config.log_level, LogLevel::Debug);
        assert_eq!(config.reopen_max_attempts, 5);
        assert_eq!(config.send_max_attempts, 4);
        assert_eq!(config.send_retry_delay_secs, 7);
    }

    #[test]
    fn boundary_values_are_accepted() {
        let mut map = minimal_valid_map();
        map.insert(KEY_AT_TIMEOUT_SECS.to_string(), "1".to_string());
        map.insert(KEY_DEFAULT_RATE_LIMIT.to_string(), "10000".to_string());
        let low = Config::from_map(&map).expect("lower bounds should parse");
        assert_eq!(low.at_timeout_secs, 1);
        assert_eq!(low.default_rate_limit, 10_000);

        map.insert(KEY_AT_TIMEOUT_SECS.to_string(), "60".to_string());
        map.insert(KEY_DEFAULT_RATE_LIMIT.to_string(), "1".to_string());
        let high = Config::from_map(&map).expect("upper bounds should parse");
        assert_eq!(high.at_timeout_secs, 60);
        assert_eq!(high.default_rate_limit, 1);
    }
}
