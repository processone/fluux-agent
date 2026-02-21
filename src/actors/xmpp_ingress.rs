use std::collections::HashMap;
use std::time::{Duration, Instant};

use anyhow::Result;
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

use super::observability;
use super::types::Envelope;
use crate::xmpp::component::XmppEvent;
use crate::xmpp::stanzas;

#[derive(Debug, Clone)]
pub struct XmppIngressActor {
    dedupe_ttl: Duration,
}

impl XmppIngressActor {
    pub fn new(dedupe_ttl_secs: u64) -> Self {
        Self {
            dedupe_ttl: Duration::from_secs(dedupe_ttl_secs),
        }
    }

    pub async fn run(
        &self,
        mut event_rx: mpsc::Receiver<XmppEvent>,
        router_tx: mpsc::Sender<Envelope>,
    ) -> Result<()> {
        info!("XmppIngressActor started");
        let mut recent_msg_ids: HashMap<(String, String), Instant> = HashMap::new();

        while let Some(event) = event_rx.recv().await {
            observability::inc_counter("dequeue.xmpp_ingress");
            if should_drop_duplicate(&event, self.dedupe_ttl, &mut recent_msg_ids) {
                observability::inc_counter("drop.xmpp_ingress.duplicate");
                if let Some((conversation_id, msg_id)) = dedupe_key(&event) {
                    debug!(
                        actor = "xmpp_ingress",
                        conversation_id = %conversation_id,
                        msg_id = %msg_id,
                        "Dropping duplicate inbound message",
                    );
                }
                continue;
            }

            let envelope = Envelope::from_xmpp_event(event);

            debug!(
                actor = "xmpp_ingress",
                conversation_id = %envelope.conversation_id,
                correlation_id = %envelope.correlation_id,
                received_at_ms = envelope.received_at_ms,
                "Forwarding inbound event to router",
            );

            if router_tx.send(envelope).await.is_err() {
                warn!("Router mailbox closed; stopping XmppIngressActor");
                break;
            }
            observability::inc_counter("enqueue.router");
            observability::add_gauge("mailbox.router.depth", 1);
        }

        info!("XmppIngressActor stopped");
        Ok(())
    }
}

fn dedupe_key(event: &XmppEvent) -> Option<(String, String)> {
    match event {
        XmppEvent::Message(msg) => msg
            .id
            .as_ref()
            .filter(|id| !id.is_empty())
            .map(|id| (stanzas::bare_jid(&msg.from).to_string(), id.clone())),
        XmppEvent::Connected
        | XmppEvent::Presence(_)
        | XmppEvent::Reaction(_)
        | XmppEvent::StreamError(_)
        | XmppEvent::Error(_)
        | XmppEvent::ReadTimeout => None,
    }
}

fn should_drop_duplicate(
    event: &XmppEvent,
    dedupe_ttl: Duration,
    recent_msg_ids: &mut HashMap<(String, String), Instant>,
) -> bool {
    if dedupe_ttl.is_zero() {
        return false;
    }

    let Some(key) = dedupe_key(event) else {
        return false;
    };

    let now = Instant::now();
    recent_msg_ids.retain(|_, seen_at| now.duration_since(*seen_at) <= dedupe_ttl);

    if let Some(last_seen) = recent_msg_ids.get(&key) {
        if now.duration_since(*last_seen) <= dedupe_ttl {
            return true;
        }
    }

    recent_msg_ids.insert(key, now);
    false
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    use crate::xmpp::stanzas::{IncomingMessage, MessageType};
    use tokio::time::{sleep, timeout};

    fn message_event(from: &str, id: Option<&str>, body: &str) -> XmppEvent {
        XmppEvent::Message(IncomingMessage {
            from: from.to_string(),
            to: "bot@localhost".to_string(),
            body: body.to_string(),
            id: id.map(ToString::to_string),
            message_type: MessageType::Chat,
            oob: vec![],
        })
    }

    #[tokio::test]
    async fn test_ingress_dedupes_same_conversation_msg_id() {
        let actor = XmppIngressActor::new(60);
        let (event_tx, event_rx) = mpsc::channel(8);
        let (router_tx, mut router_rx) = mpsc::channel(8);

        let handle = tokio::spawn(async move { actor.run(event_rx, router_tx).await });

        event_tx
            .send(message_event(
                "alice@localhost/phone",
                Some("dup-1"),
                "hello",
            ))
            .await
            .unwrap();
        event_tx
            .send(message_event(
                "alice@localhost/laptop",
                Some("dup-1"),
                "hello again",
            ))
            .await
            .unwrap();

        let first = timeout(Duration::from_millis(300), router_rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(first.conversation_id, "alice@localhost");
        assert_eq!(first.correlation_id, "dup-1");

        let second = timeout(Duration::from_millis(150), router_rx.recv()).await;
        assert!(second.is_err(), "duplicate should be dropped");

        drop(event_tx);
        handle.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn test_ingress_allows_same_msg_id_across_conversations() {
        let actor = XmppIngressActor::new(60);
        let (event_tx, event_rx) = mpsc::channel(8);
        let (router_tx, mut router_rx) = mpsc::channel(8);

        let handle = tokio::spawn(async move { actor.run(event_rx, router_tx).await });

        event_tx
            .send(message_event(
                "alice@localhost/phone",
                Some("shared"),
                "hello",
            ))
            .await
            .unwrap();
        event_tx
            .send(message_event(
                "bob@localhost/laptop",
                Some("shared"),
                "hello",
            ))
            .await
            .unwrap();

        let first = timeout(Duration::from_millis(300), router_rx.recv())
            .await
            .unwrap()
            .unwrap();
        let second = timeout(Duration::from_millis(300), router_rx.recv())
            .await
            .unwrap()
            .unwrap();

        assert_eq!(first.correlation_id, "shared");
        assert_eq!(second.correlation_id, "shared");
        assert_eq!(first.conversation_id, "alice@localhost");
        assert_eq!(second.conversation_id, "bob@localhost");

        drop(event_tx);
        handle.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn test_ingress_accepts_replay_after_ttl_expiry() {
        let actor = XmppIngressActor::new(1);
        let (event_tx, event_rx) = mpsc::channel(8);
        let (router_tx, mut router_rx) = mpsc::channel(8);

        let handle = tokio::spawn(async move { actor.run(event_rx, router_tx).await });

        event_tx
            .send(message_event(
                "alice@localhost/phone",
                Some("replay"),
                "hello",
            ))
            .await
            .unwrap();

        let first = timeout(Duration::from_millis(300), router_rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(first.correlation_id, "replay");

        event_tx
            .send(message_event(
                "alice@localhost/laptop",
                Some("replay"),
                "dup",
            ))
            .await
            .unwrap();
        let duplicate = timeout(Duration::from_millis(150), router_rx.recv()).await;
        assert!(duplicate.is_err(), "duplicate inside ttl should be dropped");

        sleep(Duration::from_millis(1200)).await;

        event_tx
            .send(message_event(
                "alice@localhost/tablet",
                Some("replay"),
                "new",
            ))
            .await
            .unwrap();
        let replay = timeout(Duration::from_millis(300), router_rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(replay.correlation_id, "replay");

        drop(event_tx);
        handle.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn test_ingress_does_not_dedupe_messages_without_id() {
        let actor = XmppIngressActor::new(60);
        let (event_tx, event_rx) = mpsc::channel(8);
        let (router_tx, mut router_rx) = mpsc::channel(8);

        let handle = tokio::spawn(async move { actor.run(event_rx, router_tx).await });

        event_tx
            .send(message_event("alice@localhost/phone", None, "hello"))
            .await
            .unwrap();
        event_tx
            .send(message_event("alice@localhost/phone", None, "hello-again"))
            .await
            .unwrap();

        let first = timeout(Duration::from_millis(300), router_rx.recv())
            .await
            .unwrap()
            .unwrap();
        let second = timeout(Duration::from_millis(300), router_rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(first.conversation_id, "alice@localhost");
        assert_eq!(second.conversation_id, "alice@localhost");

        drop(event_tx);
        handle.await.unwrap().unwrap();
    }
}
