//! Shared data models and enums (messages, API keys, statuses).
//!
//! These types mirror the database schema and derive `serde` for API/persistence
//! and comparison traits for property tests.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Lifecycle status of an [`OutboundMessage`].
///
/// Stored in the database as lowercase text; `serde(rename_all = "lowercase")`
/// keeps the wire/storage representation stable.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MessageStatus {
    /// Accepted and persisted, not yet transmitted to the modem.
    Queued,
    /// Successfully handed to the modem.
    Sent,
    /// Transmission failed (modem error code, timeout, or retry exhaustion).
    Failed,
}

impl MessageStatus {
    /// The canonical lowercase text stored in the database for this status.
    ///
    /// This matches the `serde(rename_all = "lowercase")` representation so
    /// the database and wire encodings stay identical.
    pub fn as_db_str(&self) -> &'static str {
        match self {
            MessageStatus::Queued => "queued",
            MessageStatus::Sent => "sent",
            MessageStatus::Failed => "failed",
        }
    }

    /// Parse a status from its stored text value, returning `None` for any
    /// value that is not one of the three known statuses.
    pub fn from_db_str(s: &str) -> Option<MessageStatus> {
        match s {
            "queued" => Some(MessageStatus::Queued),
            "sent" => Some(MessageStatus::Sent),
            "failed" => Some(MessageStatus::Failed),
            _ => None,
        }
    }
}

/// An SMS the service sends to a recipient.
///
/// Mirrors the `OUTBOUND_MESSAGES` table.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OutboundMessage {
    /// Primary key.
    pub id: i64,
    /// Recipient phone number in E.164 format.
    pub to_number: String,
    /// The message body as submitted by the client.
    pub body: String,
    /// Current lifecycle status.
    pub status: MessageStatus,
    /// Number of SMS parts the body was segmented into (>= 1).
    pub part_count: u8,
    /// Modem-assigned message reference returned by `+CMGS`, when sent.
    pub msg_reference: Option<String>,
    /// Modem error code (`+CMS`/`+CME ERROR`) or timeout indication, when failed.
    pub error_code: Option<String>,
    /// Time the record was created (request accepted), in UTC.
    pub created_at: DateTime<Utc>,
    /// Time the record was last updated, in UTC.
    pub updated_at: DateTime<Utc>,
}

/// An SMS the service receives from a sender.
///
/// Mirrors the `INBOUND_MESSAGES` table.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InboundMessage {
    /// Primary key.
    pub id: i64,
    /// Sender phone number as reported by the modem.
    pub from_number: String,
    /// The received message body.
    pub body: String,
    /// System UTC time at which the message was read from the modem.
    pub received_at: DateTime<Utc>,
}

/// A stored API key credential.
///
/// Mirrors the `API_KEYS` table. The plaintext key is never persisted;
/// only its cryptographic hash and a non-reversible identifier are stored.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApiKey {
    /// Primary key.
    pub id: i64,
    /// Cryptographic hash of the presented key (never the plaintext).
    pub key_hash: String,
    /// Non-reversible identifier (SHA-256 hex) safe to log and audit.
    pub key_identifier: String,
    /// Optional per-key request limit overriding the default (1..=10_000).
    pub custom_rate_limit: Option<u32>,
    /// Whether the key has been revoked.
    pub revoked: bool,
    /// Time the key was created, in UTC.
    pub created_at: DateTime<Utc>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn sample_time() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2024, 1, 2, 3, 4, 5).unwrap()
    }

    #[test]
    fn message_status_serializes_to_lowercase() {
        assert_eq!(
            serde_json::to_string(&MessageStatus::Queued).unwrap(),
            "\"queued\""
        );
        assert_eq!(
            serde_json::to_string(&MessageStatus::Sent).unwrap(),
            "\"sent\""
        );
        assert_eq!(
            serde_json::to_string(&MessageStatus::Failed).unwrap(),
            "\"failed\""
        );
    }

    #[test]
    fn message_status_round_trips() {
        for status in [
            MessageStatus::Queued,
            MessageStatus::Sent,
            MessageStatus::Failed,
        ] {
            let json = serde_json::to_string(&status).unwrap();
            let back: MessageStatus = serde_json::from_str(&json).unwrap();
            assert_eq!(status, back);
        }
    }

    #[test]
    fn outbound_message_round_trips() {
        let msg = OutboundMessage {
            id: 7,
            to_number: "+14155552671".to_string(),
            body: "hello world".to_string(),
            status: MessageStatus::Sent,
            part_count: 1,
            msg_reference: Some("42".to_string()),
            error_code: None,
            created_at: sample_time(),
            updated_at: sample_time(),
        };
        let json = serde_json::to_string(&msg).unwrap();
        let back: OutboundMessage = serde_json::from_str(&json).unwrap();
        assert_eq!(msg, back);
    }

    #[test]
    fn inbound_message_round_trips() {
        let msg = InboundMessage {
            id: 3,
            from_number: "+14155550000".to_string(),
            body: "incoming".to_string(),
            received_at: sample_time(),
        };
        let json = serde_json::to_string(&msg).unwrap();
        let back: InboundMessage = serde_json::from_str(&json).unwrap();
        assert_eq!(msg, back);
    }

    #[test]
    fn api_key_round_trips() {
        let key = ApiKey {
            id: 1,
            key_hash: "deadbeef".to_string(),
            key_identifier: "abc123".to_string(),
            custom_rate_limit: Some(500),
            revoked: false,
            created_at: sample_time(),
        };
        let json = serde_json::to_string(&key).unwrap();
        let back: ApiKey = serde_json::from_str(&json).unwrap();
        assert_eq!(key, back);
    }
}
