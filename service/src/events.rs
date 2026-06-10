use serde::Serialize;
use tokio::sync::broadcast;

use crate::models::MessageStatus;

/// Default capacity of the in-process event broadcast channel.
pub const EVENT_BUS_CAPACITY: usize = 256;

/// Payload published when an outbound message changes delivery status.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, utoipa::ToSchema)]
pub struct MessageStatusEvent {
    /// The unique outbound message ID.
    pub id: i64,
    /// The new delivery status.
    pub status: MessageStatus,
    /// The modem-assigned reference when the message was sent.
    pub reference: Option<String>,
}

/// Payload published when a new inbound message is persisted.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, utoipa::ToSchema)]
pub struct InboundSmsEvent {
    /// The unique inbound message ID.
    pub id: i64,
    /// The sender phone number.
    pub from: String,
    /// The message body.
    pub body: String,
}

/// A real-time event broadcast to connected SSE clients.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ServiceEvent {
    /// An outbound message transitioned to a terminal status.
    MessageStatus(MessageStatusEvent),
    /// A new inbound message was received and persisted.
    InboundSms(InboundSmsEvent),
}

impl ServiceEvent {
    /// Returns the SSE `event:` name for this event.
    pub fn name(&self) -> &'static str {
        match self {
            ServiceEvent::MessageStatus(_) => "message_status",
            ServiceEvent::InboundSms(_) => "inbound_sms",
        }
    }
}

/// A cloneable handle to the in-process event broadcast channel.
///
/// Producers (the REST send dispatch and the Modem Manager) call
/// [`EventBus::publish`]; each connected Server-Sent Events client holds a
/// subscription obtained from [`EventBus::subscribe`]. Publishing with no
/// subscribers is a no-op rather than an error.
#[derive(Clone)]
pub struct EventBus {
    tx: broadcast::Sender<ServiceEvent>,
}

impl EventBus {
    /// Creates a new event bus with the given channel capacity.
    pub fn new(capacity: usize) -> Self {
        let (tx, _rx) = broadcast::channel(capacity.max(1));
        EventBus { tx }
    }

    /// Subscribes a new receiver to the event stream.
    pub fn subscribe(&self) -> broadcast::Receiver<ServiceEvent> {
        self.tx.subscribe()
    }

    /// Publishes an event to all current subscribers.
    pub fn publish(&self, event: ServiceEvent) {
        let _ = self.tx.send(event);
    }
}

impl Default for EventBus {
    fn default() -> Self {
        EventBus::new(EVENT_BUS_CAPACITY)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn subscribers_receive_published_events() {
        let bus = EventBus::new(8);
        let mut rx = bus.subscribe();

        bus.publish(ServiceEvent::MessageStatus(MessageStatusEvent {
            id: 1,
            status: MessageStatus::Sent,
            reference: Some("42".to_string()),
        }));

        let received = rx.recv().await.unwrap();
        assert_eq!(
            received,
            ServiceEvent::MessageStatus(MessageStatusEvent {
                id: 1,
                status: MessageStatus::Sent,
                reference: Some("42".to_string()),
            })
        );
    }

    #[tokio::test]
    async fn publishing_without_subscribers_is_a_noop() {
        let bus = EventBus::new(8);
        bus.publish(ServiceEvent::InboundSms(InboundSmsEvent {
            id: 7,
            from: "+14155550123".to_string(),
            body: "hi".to_string(),
        }));
    }

    #[test]
    fn event_names_are_stable() {
        let status = ServiceEvent::MessageStatus(MessageStatusEvent {
            id: 1,
            status: MessageStatus::Queued,
            reference: None,
        });
        let inbound = ServiceEvent::InboundSms(InboundSmsEvent {
            id: 2,
            from: "+1".to_string(),
            body: String::new(),
        });
        assert_eq!(status.name(), "message_status");
        assert_eq!(inbound.name(), "inbound_sms");
    }
}
