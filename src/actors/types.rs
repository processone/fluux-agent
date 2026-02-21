use chrono::Utc;
use uuid::Uuid;

use crate::xmpp::component::XmppEvent;
use crate::xmpp::stanzas;

pub const SYSTEM_CONVERSATION_ID: &str = "__system__";

#[derive(Debug)]
pub struct Envelope {
    pub conversation_id: String,
    pub correlation_id: String,
    pub received_at_ms: i64,
    pub event: XmppEvent,
}

impl Envelope {
    pub fn from_xmpp_event(event: XmppEvent) -> Self {
        let conversation_id = match &event {
            XmppEvent::Connected
            | XmppEvent::StreamError(_)
            | XmppEvent::Error(_)
            | XmppEvent::ReadTimeout => SYSTEM_CONVERSATION_ID.to_string(),
            XmppEvent::Message(msg) => stanzas::bare_jid(&msg.from).to_string(),
            XmppEvent::Presence(pres) => stanzas::bare_jid(&pres.from).to_string(),
            XmppEvent::Reaction(reaction) => stanzas::bare_jid(&reaction.from).to_string(),
        };

        let correlation_id = correlation_id_from_event(&event);

        Self {
            conversation_id,
            correlation_id,
            received_at_ms: Utc::now().timestamp_millis(),
            event,
        }
    }

    pub fn into_event(self) -> XmppEvent {
        self.event
    }
}

fn correlation_id_from_event(event: &XmppEvent) -> String {
    match event {
        XmppEvent::Message(msg) => msg
            .id
            .as_ref()
            .filter(|id| !id.is_empty())
            .cloned()
            .unwrap_or_else(new_correlation_id),
        XmppEvent::Reaction(reaction) => {
            if reaction.message_id.is_empty() {
                new_correlation_id()
            } else {
                reaction.message_id.clone()
            }
        }
        XmppEvent::Connected
        | XmppEvent::Presence(_)
        | XmppEvent::StreamError(_)
        | XmppEvent::Error(_)
        | XmppEvent::ReadTimeout => new_correlation_id(),
    }
}

fn new_correlation_id() -> String {
    Uuid::new_v4().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::xmpp::stanzas::{IncomingMessage, MessageType};

    #[test]
    fn test_envelope_from_message_uses_bare_jid() {
        let event = XmppEvent::Message(IncomingMessage {
            from: "alice@example.com/mobile".to_string(),
            to: "bot@example.com".to_string(),
            body: "hello".to_string(),
            id: Some("msg-123".to_string()),
            message_type: MessageType::Chat,
            oob: vec![],
        });

        let envelope = Envelope::from_xmpp_event(event);
        assert_eq!(envelope.conversation_id, "alice@example.com");
        assert_eq!(envelope.correlation_id, "msg-123");
        assert!(envelope.received_at_ms > 0);
    }

    #[test]
    fn test_envelope_from_connected_is_system_scoped() {
        let envelope = Envelope::from_xmpp_event(XmppEvent::Connected);
        assert_eq!(envelope.conversation_id, SYSTEM_CONVERSATION_ID);
        assert!(!envelope.correlation_id.is_empty());
    }
}
