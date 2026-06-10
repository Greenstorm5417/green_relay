use std::fmt;
use std::io;
use serde::{Serialize, Deserialize};

/// Error representing standard API error responses.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
pub struct ApiError {
    /// The error message.
    pub error: String,
    /// Invalid fields that caused the error.
    pub fields: Vec<String>,
}

impl ApiError {
    /// Creates a new ApiError.
    pub fn new(error: impl Into<String>, fields: Vec<String>) -> Self {
        ApiError {
            error: error.into(),
            fields,
        }
    }
}

/// Errors that can occur when validating message payloads.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ValidationError {
    /// Required fields are missing.
    MissingFields(Vec<String>),
    /// Phone number is not a valid E.164 number.
    InvalidPhoneNumber,
    /// Message body is empty.
    BodyEmpty,
    /// Message body exceeds the maximum allowed length.
    BodyTooLong,
}

/// Errors that can occur when segmenting a message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SegmentError {
    /// The message requires too many parts to be transmitted.
    TooManyParts {
        /// The number of parts required.
        required: usize,
    },
}

impl fmt::Display for SegmentError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SegmentError::TooManyParts { required } => write!(
                f,
                "message requires {required} parts which exceeds the maximum of 10"
            ),
        }
    }
}

impl std::error::Error for SegmentError {}

/// Errors that can occur when parsing or loading configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfigError {
    /// A required configuration key was missing.
    MissingKey(String),
    /// A configuration value was invalid or could not be parsed.
    InvalidValue {
        /// The configuration key.
        key: String,
        /// The raw invalid value.
        value: String,
        /// The details of the validation failure.
        reason: String,
    },
    /// Failed to read the configuration file.
    FileRead {
        /// The path to the configuration file.
        path: String,
        /// The underlying file system or parsing error reason.
        reason: String,
    },
}

impl ConfigError {
    /// Returns the name of the configuration key associated with the error, if any.
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

/// Errors that can occur during database operations.
#[derive(Debug)]
pub enum DbError {
    /// The database schema is not yet ready or fully migrated.
    NotReady,
    /// Failed to execute a schema migration.
    Migration {
        /// The migration version that failed.
        version: i64,
        /// The underlying sqlx error.
        source: sqlx::Error,
    },
    /// A general sqlx database error.
    Sqlx(sqlx::Error),
}

impl DbError {
    /// Returns true if the error is due to the database schema not being ready.
    pub fn is_not_ready(&self) -> bool {
        matches!(self, DbError::NotReady)
    }
}

impl fmt::Display for DbError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DbError::NotReady => write!(f, "database schema is not ready"),
            DbError::Migration { version, source } => {
                write!(f, "migration {version} failed: {source}")
            }
            DbError::Sqlx(e) => write!(f, "database error: {e}"),
        }
    }
}

impl std::error::Error for DbError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            DbError::Migration { source, .. } => Some(source),
            DbError::Sqlx(e) => Some(e),
            DbError::NotReady => None,
        }
    }
}

/// Errors that can occur during the execution of the service.
#[derive(Debug)]
pub enum RunError {
    /// A configuration error.
    Config(ConfigError),
    /// A database error.
    Db(DbError),
    /// Failed to bind to the listen address.
    Bind(io::Error),
    /// HTTP server serving error.
    Serve(io::Error),
    /// Graceful shutdown timeout exceeded.
    ShutdownTimeout,
    /// Creating or resetting an admin user failed.
    AdminSetup(String),
}

impl RunError {
    /// Returns the configuration key associated with the error, if applicable.
    pub fn config_key(&self) -> Option<&str> {
        match self {
            RunError::Config(e) => e.key(),
            _ => None,
        }
    }
}

impl fmt::Display for RunError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RunError::Config(e) => write!(f, "configuration error: {e}"),
            RunError::Db(e) => write!(f, "database error during startup: {e}"),
            RunError::Bind(e) => write!(f, "failed to bind listen address: {e}"),
            RunError::Serve(e) => write!(f, "http server error: {e}"),
            RunError::ShutdownTimeout => write!(
                f,
                "graceful shutdown exceeded the 30s grace period; aborting"
            ),
            RunError::AdminSetup(reason) => write!(f, "admin setup error: {reason}"),
        }
    }
}

impl std::error::Error for RunError {}

/// Error that can occur during logger subscriber initialization.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubscriberInitError(pub String);

impl fmt::Display for SubscriberInitError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "failed to initialize logging subscriber: {}", self.0)
    }
}

impl std::error::Error for SubscriberInitError {}
