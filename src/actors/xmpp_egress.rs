use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;
use tokio::time::{sleep, timeout};
use tracing::{debug, info, warn};

use super::dead_letter::{DeadLetterRecord, DeadLetterService};
use super::observability;
use crate::xmpp::component::XmppCommand;

#[derive(Debug, Clone)]
pub struct XmppEgressActor {
    send_timeout: Duration,
    retry_backoff: Duration,
    max_retries: usize,
    dead_letter: Arc<DeadLetterService>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
enum EgressFailureReason {
    SendTimeoutExhausted,
    TransportClosed,
}

impl EgressFailureReason {
    fn as_str(self) -> &'static str {
        match self {
            Self::SendTimeoutExhausted => "send_timeout_exhausted",
            Self::TransportClosed => "transport_closed",
        }
    }
}

impl XmppEgressActor {
    pub fn new(
        send_timeout_ms: u64,
        retry_backoff_ms: u64,
        max_retries: usize,
        dead_letter_path: PathBuf,
    ) -> Self {
        Self::with_dead_letter_service(
            send_timeout_ms,
            retry_backoff_ms,
            max_retries,
            Arc::new(DeadLetterService::new(dead_letter_path)),
        )
    }

    pub fn with_dead_letter_service(
        send_timeout_ms: u64,
        retry_backoff_ms: u64,
        max_retries: usize,
        dead_letter: Arc<DeadLetterService>,
    ) -> Self {
        Self {
            send_timeout: Duration::from_millis(send_timeout_ms.max(1)),
            retry_backoff: Duration::from_millis(retry_backoff_ms),
            max_retries,
            dead_letter,
        }
    }

    pub async fn run(
        &self,
        mut session_cmd_rx: mpsc::Receiver<XmppCommand>,
        transport_cmd_tx: mpsc::Sender<XmppCommand>,
    ) -> Result<()> {
        info!("XmppEgressActor started");

        while let Some(cmd) = session_cmd_rx.recv().await {
            let handle_started = std::time::Instant::now();
            observability::inc_counter("dequeue.egress");
            observability::set_gauge("mailbox.egress.depth", session_cmd_rx.len() as i64);
            debug!(actor = "xmpp_egress", "Forwarding outbound XMPP command");
            match self.forward_with_retries(&transport_cmd_tx, &cmd).await {
                Ok(()) => {}
                Err((reason, attempts)) => {
                    observability::inc_counter("overflow.egress.send_failed");
                    self.record_dead_letter(&cmd, reason, attempts).await;
                    if matches!(reason, EgressFailureReason::TransportClosed) {
                        warn!("Transport command channel closed; stopping XmppEgressActor");
                        break;
                    }
                }
            }
            observability::observe_actor_latency("xmpp_egress", handle_started.elapsed());
        }

        info!("XmppEgressActor stopped");
        Ok(())
    }

    async fn forward_with_retries(
        &self,
        transport_cmd_tx: &mpsc::Sender<XmppCommand>,
        cmd: &XmppCommand,
    ) -> std::result::Result<(), (EgressFailureReason, usize)> {
        for attempt in 0..=self.max_retries {
            let attempt_num = attempt + 1;
            match timeout(self.send_timeout, transport_cmd_tx.send(cmd.clone())).await {
                Ok(Ok(())) => return Ok(()),
                Ok(Err(_)) => return Err((EgressFailureReason::TransportClosed, attempt_num)),
                Err(_) => {
                    if attempt == self.max_retries {
                        return Err((EgressFailureReason::SendTimeoutExhausted, attempt_num));
                    }
                    let backoff = self.backoff_for_retry(attempt);
                    if !backoff.is_zero() {
                        sleep(backoff).await;
                    }
                }
            }
        }

        Err((
            EgressFailureReason::SendTimeoutExhausted,
            self.max_retries + 1,
        ))
    }

    fn backoff_for_retry(&self, retry_index: usize) -> Duration {
        let base_ms = self.retry_backoff.as_millis();
        if base_ms == 0 {
            return Duration::ZERO;
        }
        let factor = 1u128 << (retry_index.min(16) as u32);
        let delay_ms = base_ms.saturating_mul(factor).min(u64::MAX as u128) as u64;
        Duration::from_millis(delay_ms)
    }

    async fn record_dead_letter(
        &self,
        cmd: &XmppCommand,
        reason: EgressFailureReason,
        attempts: usize,
    ) {
        let record = DeadLetterRecord::xmpp_egress_with_payload(
            reason.as_str(),
            correlation_id_for_command(cmd),
            command_kind(cmd),
            command_destination(cmd),
            attempts,
            serde_json::to_value(cmd).ok(),
        );
        if let Err(err) = self.dead_letter.record(record).await {
            warn!(
                reason = reason.as_str(),
                command_kind = command_kind(cmd),
                dead_letter_path = %self.dead_letter.path().display(),
                "Failed to persist egress dead-letter record: {err}",
            );
        } else {
            observability::inc_counter("dead_letter.egress");
        }
    }
}

fn correlation_id_for_command(cmd: &XmppCommand) -> String {
    match cmd {
        XmppCommand::SendMessage { id: Some(id), .. }
        | XmppCommand::SendMucMessage { id: Some(id), .. } => id.clone(),
        XmppCommand::SendMessage { body, .. } | XmppCommand::SendMucMessage { body, .. } => {
            extract_ref_correlation_id(body).unwrap_or_else(|| "unknown".to_string())
        }
        XmppCommand::SendChatState { .. }
        | XmppCommand::JoinMuc { .. }
        | XmppCommand::SendRaw(_)
        | XmppCommand::Ping => "unknown".to_string(),
    }
}

fn extract_ref_correlation_id(body: &str) -> Option<String> {
    const PREFIX: &str = "(ref: ";
    let start = body.find(PREFIX)?;
    let rest = &body[start + PREFIX.len()..];
    let end = rest.find(')')?;
    let value = rest[..end].trim();
    if value.is_empty() {
        None
    } else {
        Some(value.to_string())
    }
}

fn command_kind(cmd: &XmppCommand) -> &'static str {
    match cmd {
        XmppCommand::SendMessage { .. } => "send_message",
        XmppCommand::SendChatState { .. } => "send_chat_state",
        XmppCommand::SendMucMessage { .. } => "send_muc_message",
        XmppCommand::JoinMuc { .. } => "join_muc",
        XmppCommand::SendRaw(_) => "send_raw",
        XmppCommand::Ping => "ping",
    }
}

fn command_destination(cmd: &XmppCommand) -> Option<String> {
    match cmd {
        XmppCommand::SendMessage { to, .. }
        | XmppCommand::SendChatState { to, .. }
        | XmppCommand::SendMucMessage { to, .. } => Some(to.clone()),
        XmppCommand::JoinMuc { room, .. } => Some(room.clone()),
        XmppCommand::SendRaw(_) | XmppCommand::Ping => None,
    }
}

#[cfg(test)]
mod tests {
    use tempfile::TempDir;

    use super::*;
    use crate::actors::dead_letter::{
        read_dead_letters as read_dead_letter_records, DeadLetterSource,
    };

    fn read_dead_letters(path: &std::path::Path) -> Vec<DeadLetterRecord> {
        read_dead_letter_records(path).unwrap()
    }

    #[tokio::test]
    async fn test_egress_forwards_command_successfully() {
        let tmp = TempDir::new().unwrap();
        let dead_letter_path = tmp.path().join("egress-success.jsonl");
        let actor = XmppEgressActor::new(50, 5, 2, dead_letter_path.clone());

        let (session_tx, session_rx) = mpsc::channel(4);
        let (transport_tx, mut transport_rx) = mpsc::channel(4);

        let handle = tokio::spawn(async move { actor.run(session_rx, transport_tx).await });
        session_tx.send(XmppCommand::Ping).await.unwrap();
        drop(session_tx);

        let forwarded = tokio::time::timeout(Duration::from_millis(300), transport_rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(forwarded, XmppCommand::Ping));

        handle.await.unwrap().unwrap();
        assert!(read_dead_letters(&dead_letter_path).is_empty());
    }

    #[tokio::test]
    async fn test_egress_records_dead_letter_after_timeout_exhaustion() {
        let tmp = TempDir::new().unwrap();
        let dead_letter_path = tmp.path().join("egress-timeout.jsonl");
        let actor = XmppEgressActor::new(20, 5, 2, dead_letter_path.clone());

        let (session_tx, session_rx) = mpsc::channel(4);
        let (transport_tx, _transport_rx) = mpsc::channel(1);
        transport_tx.send(XmppCommand::Ping).await.unwrap();

        let handle = tokio::spawn(async move { actor.run(session_rx, transport_tx).await });
        session_tx.send(XmppCommand::Ping).await.unwrap();
        drop(session_tx);

        tokio::time::timeout(Duration::from_secs(2), handle)
            .await
            .expect("egress task should finish")
            .unwrap()
            .unwrap();

        let dead_letters = read_dead_letters(&dead_letter_path);
        assert_eq!(dead_letters.len(), 1);
        assert_eq!(dead_letters[0].reason, "send_timeout_exhausted");
        assert_eq!(dead_letters[0].source, DeadLetterSource::XmppEgress);
        assert_eq!(dead_letters[0].attempts, Some(3));
        assert_eq!(dead_letters[0].payload_kind, "ping");
    }

    #[tokio::test]
    async fn test_egress_records_dead_letter_when_transport_is_closed() {
        let tmp = TempDir::new().unwrap();
        let dead_letter_path = tmp.path().join("egress-closed.jsonl");
        let actor = XmppEgressActor::new(50, 5, 3, dead_letter_path.clone());

        let (session_tx, session_rx) = mpsc::channel(4);
        let (transport_tx, transport_rx) = mpsc::channel(4);
        drop(transport_rx);

        let handle = tokio::spawn(async move { actor.run(session_rx, transport_tx).await });
        session_tx
            .send(XmppCommand::SendMessage {
                to: "alice@localhost/mobile".to_string(),
                body: "System busy. Please retry in 5s. (ref: corr-123)".to_string(),
                id: None,
            })
            .await
            .unwrap();
        drop(session_tx);

        tokio::time::timeout(Duration::from_secs(2), handle)
            .await
            .expect("egress task should finish")
            .unwrap()
            .unwrap();

        let dead_letters = read_dead_letters(&dead_letter_path);
        assert_eq!(dead_letters.len(), 1);
        assert_eq!(dead_letters[0].reason, "transport_closed");
        assert_eq!(dead_letters[0].source, DeadLetterSource::XmppEgress);
        assert_eq!(dead_letters[0].attempts, Some(1));
        assert_eq!(dead_letters[0].payload_kind, "send_message");
        assert_eq!(dead_letters[0].correlation_id, "corr-123");
        assert_eq!(
            dead_letters[0].destination.as_deref(),
            Some("alice@localhost/mobile")
        );
    }
}
