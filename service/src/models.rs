use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Represents the delivery or sending status of a message.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum MessageStatus {
    Queued,

    Sent,

    Failed,
}

impl MessageStatus {
    /// Returns the string representation of the message status used in the database.
    pub fn as_db_str(&self) -> &'static str {
        match self {
            MessageStatus::Queued => "queued",
            MessageStatus::Sent => "sent",
            MessageStatus::Failed => "failed",
        }
    }

    /// Parses a message status from its database string representation.
    pub fn from_db_str(s: &str) -> Option<MessageStatus> {
        match s {
            "queued" => Some(MessageStatus::Queued),
            "sent" => Some(MessageStatus::Sent),
            "failed" => Some(MessageStatus::Failed),
            _ => None,
        }
    }
}

/// Represents an outgoing SMS message.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
pub struct OutboundMessage {
    /// The unique identifier of the message.
    pub id: i64,

    /// The recipient's phone number.
    pub to_number: String,

    /// The body text of the message.
    pub body: String,

    /// The current delivery status of the message.
    pub status: MessageStatus,

    /// The number of parts/segments the message is split into.
    pub part_count: u8,

    /// The reference assigned by the network/modem.
    pub msg_reference: Option<String>,

    /// The error code if sending failed.
    pub error_code: Option<String>,

    /// The timestamp when the message was created.
    pub created_at: DateTime<Utc>,

    /// The timestamp when the message was last updated.
    pub updated_at: DateTime<Utc>,
}

/// Represents an incoming SMS message.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
pub struct InboundMessage {
    /// The unique identifier of the message.
    pub id: i64,

    /// The sender's phone number.
    pub from_number: String,

    /// The body text of the message.
    pub body: String,

    /// The timestamp when the message was received.
    pub received_at: DateTime<Utc>,
}

/// Represents an API key used for authentication.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApiKey {
    /// The unique identifier of the API key.
    pub id: i64,

    /// The hash of the API key.
    pub key_hash: String,

    /// A non-sensitive identifier for the API key.
    pub key_identifier: String,

    /// An optional custom rate limit for the API key.
    pub custom_rate_limit: Option<u32>,

    /// Indicates if the API key has been revoked.
    pub revoked: bool,

    /// The timestamp when the API key was created.
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
