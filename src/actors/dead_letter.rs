use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use tokio::io::AsyncWriteExt;
use tokio::sync::{mpsc, oneshot, OnceCell};
use tokio::time::{sleep, timeout};

use crate::agent::memory::{Attachment, Reaction};
use crate::skills::SkillContext;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum DeadLetterSource {
    Router,
    XmppEgress,
    SkillRouter,
    MemoryActor,
    #[default]
    Unknown,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DeadLetterRecord {
    pub timestamp_ms: i64,
    #[serde(default)]
    pub source: DeadLetterSource,
    pub reason: String,
    pub correlation_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub conversation_id: Option<String>,
    #[serde(default, alias = "event_kind", alias = "command_kind")]
    pub payload_kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub destination: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attempts: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub received_at_ms: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub busy_response: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub payload: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DeadLetterSkillReplayPayload {
    pub skill_name: String,
    pub params: serde_json::Value,
    pub context: SkillContext,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "operation", rename_all = "snake_case")]
pub enum DeadLetterMemoryReplayPayload {
    StoreMessageStructured {
        jid: String,
        role: String,
        content: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        msg_id: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        sender: Option<String>,
    },
    StoreMessageFull {
        jid: String,
        role: String,
        content: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        msg_id: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        sender: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        attachments: Option<Vec<Attachment>>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reaction: Option<Reaction>,
    },
}

enum DeadLetterRequest {
    Record {
        record: DeadLetterRecord,
        reply_tx: oneshot::Sender<Result<()>>,
    },
    Replay {
        reply_tx: oneshot::Sender<Result<Vec<DeadLetterRecord>>>,
    },
}

/// Mailbox-backed service responsible for dead-letter persistence and replay.
#[derive(Debug)]
pub struct DeadLetterService {
    path: PathBuf,
    enqueue_timeout: Duration,
    mailbox_size: usize,
    write_max_retries: usize,
    write_retry_backoff: Duration,
    mailbox: OnceCell<mpsc::Sender<DeadLetterRequest>>,
}

impl DeadLetterService {
    pub fn new(path: PathBuf) -> Self {
        Self::with_config(path, 1024, 200, 3, 20)
    }

    pub fn with_config(
        path: PathBuf,
        mailbox_size: usize,
        enqueue_timeout_ms: u64,
        write_max_retries: usize,
        write_retry_backoff_ms: u64,
    ) -> Self {
        Self {
            path,
            enqueue_timeout: Duration::from_millis(enqueue_timeout_ms.max(1)),
            mailbox_size: mailbox_size.max(1),
            write_max_retries,
            write_retry_backoff: Duration::from_millis(write_retry_backoff_ms),
            mailbox: OnceCell::new(),
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub async fn record(&self, record: DeadLetterRecord) -> Result<()> {
        let (reply_tx, reply_rx) = oneshot::channel();
        let request = DeadLetterRequest::Record { record, reply_tx };
        let tx = self.mailbox_tx().await;
        match timeout(self.enqueue_timeout, tx.send(request)).await {
            Ok(Ok(())) => {}
            Ok(Err(_)) => return Err(anyhow!("dead_letter_service_unavailable")),
            Err(_) => return Err(anyhow!("dead_letter_service_busy")),
        }
        reply_rx
            .await
            .map_err(|_| anyhow!("dead_letter_service_unavailable"))?
    }

    pub async fn replay(&self) -> Result<Vec<DeadLetterRecord>> {
        let (reply_tx, reply_rx) = oneshot::channel();
        let request = DeadLetterRequest::Replay { reply_tx };
        let tx = self.mailbox_tx().await;
        match timeout(self.enqueue_timeout, tx.send(request)).await {
            Ok(Ok(())) => {}
            Ok(Err(_)) => return Err(anyhow!("dead_letter_service_unavailable")),
            Err(_) => return Err(anyhow!("dead_letter_service_busy")),
        }
        reply_rx
            .await
            .map_err(|_| anyhow!("dead_letter_service_unavailable"))?
    }

    async fn mailbox_tx(&self) -> mpsc::Sender<DeadLetterRequest> {
        self.mailbox
            .get_or_init(|| async { self.spawn_worker() })
            .await
            .clone()
    }

    fn spawn_worker(&self) -> mpsc::Sender<DeadLetterRequest> {
        let (tx, mut rx) = mpsc::channel::<DeadLetterRequest>(self.mailbox_size);
        let path = self.path.clone();
        let write_max_retries = self.write_max_retries;
        let write_retry_backoff = self.write_retry_backoff;

        tokio::spawn(async move {
            while let Some(request) = rx.recv().await {
                match request {
                    DeadLetterRequest::Record { record, reply_tx } => {
                        let result = append_with_retries(
                            &path,
                            &record,
                            write_max_retries,
                            write_retry_backoff,
                        )
                        .await;
                        let _ = reply_tx.send(result);
                    }
                    DeadLetterRequest::Replay { reply_tx } => {
                        let _ = reply_tx.send(read_dead_letters(&path));
                    }
                }
            }
        });

        tx
    }
}

impl DeadLetterRecord {
    pub fn router(
        reason: &str,
        conversation_id: String,
        correlation_id: String,
        received_at_ms: i64,
        event_kind: &str,
        busy_response: &str,
    ) -> Self {
        Self {
            timestamp_ms: Utc::now().timestamp_millis(),
            source: DeadLetterSource::Router,
            reason: reason.to_string(),
            correlation_id,
            conversation_id: Some(conversation_id),
            payload_kind: event_kind.to_string(),
            destination: None,
            attempts: None,
            received_at_ms: Some(received_at_ms),
            busy_response: Some(busy_response.to_string()),
            payload: None,
        }
    }

    pub fn xmpp_egress(
        reason: &str,
        correlation_id: String,
        command_kind: &str,
        destination: Option<String>,
        attempts: usize,
    ) -> Self {
        Self::xmpp_egress_with_payload(
            reason,
            correlation_id,
            command_kind,
            destination,
            attempts,
            None,
        )
    }

    pub fn xmpp_egress_with_payload(
        reason: &str,
        correlation_id: String,
        command_kind: &str,
        destination: Option<String>,
        attempts: usize,
        payload: Option<serde_json::Value>,
    ) -> Self {
        Self {
            timestamp_ms: Utc::now().timestamp_millis(),
            source: DeadLetterSource::XmppEgress,
            reason: reason.to_string(),
            correlation_id,
            conversation_id: None,
            payload_kind: command_kind.to_string(),
            destination,
            attempts: Some(attempts),
            received_at_ms: None,
            busy_response: None,
            payload,
        }
    }

    pub fn skill_router_with_payload(
        reason: &str,
        conversation_id: String,
        correlation_id: String,
        skill_name: &str,
        payload: Option<serde_json::Value>,
    ) -> Self {
        Self {
            timestamp_ms: Utc::now().timestamp_millis(),
            source: DeadLetterSource::SkillRouter,
            reason: reason.to_string(),
            correlation_id,
            conversation_id: Some(conversation_id),
            payload_kind: skill_name.to_string(),
            destination: None,
            attempts: None,
            received_at_ms: None,
            busy_response: None,
            payload,
        }
    }

    pub fn memory_actor_with_payload(
        reason: &str,
        conversation_id: String,
        correlation_id: String,
        operation: &str,
        payload: Option<serde_json::Value>,
    ) -> Self {
        Self {
            timestamp_ms: Utc::now().timestamp_millis(),
            source: DeadLetterSource::MemoryActor,
            reason: reason.to_string(),
            correlation_id,
            conversation_id: Some(conversation_id),
            payload_kind: operation.to_string(),
            destination: None,
            attempts: None,
            received_at_ms: None,
            busy_response: None,
            payload,
        }
    }
}

pub async fn append_dead_letter(path: &Path, record: &DeadLetterRecord) -> Result<()> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            tokio::fs::create_dir_all(parent).await?;
        }
    }

    let mut file = tokio::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .await?;
    let line = serde_json::to_string(record)?;
    file.write_all(line.as_bytes()).await?;
    file.write_all(b"\n").await?;
    Ok(())
}

async fn append_with_retries(
    path: &Path,
    record: &DeadLetterRecord,
    max_retries: usize,
    retry_backoff: Duration,
) -> Result<()> {
    for attempt in 0..=max_retries {
        match append_dead_letter(path, record).await {
            Ok(()) => return Ok(()),
            Err(err) => {
                if attempt == max_retries {
                    return Err(err);
                }
                let backoff = retry_delay(retry_backoff, attempt);
                if !backoff.is_zero() {
                    sleep(backoff).await;
                }
            }
        }
    }

    Ok(())
}

fn retry_delay(base: Duration, retry_index: usize) -> Duration {
    let base_ms = base.as_millis();
    if base_ms == 0 {
        return Duration::ZERO;
    }
    let factor = 1u128 << (retry_index.min(16) as u32);
    let delay_ms = base_ms.saturating_mul(factor).min(u64::MAX as u128) as u64;
    Duration::from_millis(delay_ms)
}

pub fn read_dead_letters(path: &Path) -> Result<Vec<DeadLetterRecord>> {
    let Ok(raw) = std::fs::read_to_string(path) else {
        return Ok(vec![]);
    };

    raw.lines()
        .enumerate()
        .filter(|(_, line)| !line.trim().is_empty())
        .map(|(idx, line)| {
            serde_json::from_str::<DeadLetterRecord>(line)
                .with_context(|| format!("invalid dead-letter JSON on line {}", idx + 1))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use tempfile::TempDir;

    use super::*;

    #[tokio::test]
    async fn test_read_dead_letters_parses_mixed_sources() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("dead_letters.jsonl");

        append_dead_letter(
            &path,
            &DeadLetterRecord::router(
                "session_mailbox_full",
                "alice@localhost".to_string(),
                "corr-router".to_string(),
                123,
                "message",
                "sent",
            ),
        )
        .await
        .unwrap();
        append_dead_letter(
            &path,
            &DeadLetterRecord::xmpp_egress(
                "transport_closed",
                "corr-egress".to_string(),
                "send_message",
                Some("alice@localhost/mobile".to_string()),
                1,
            ),
        )
        .await
        .unwrap();

        let records = read_dead_letters(&path).unwrap();
        assert_eq!(records.len(), 2);
        assert_eq!(records[0].source, DeadLetterSource::Router);
        assert_eq!(records[0].payload_kind, "message");
        assert_eq!(records[1].source, DeadLetterSource::XmppEgress);
        assert_eq!(records[1].payload_kind, "send_message");
    }

    #[test]
    fn test_read_dead_letters_supports_legacy_router_and_egress_fields() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("dead_letters-legacy.jsonl");
        std::fs::write(
            &path,
            concat!(
                "{\"timestamp_ms\":1,\"reason\":\"session_mailbox_full\",\"conversation_id\":\"alice\",\"correlation_id\":\"corr1\",\"received_at_ms\":1,\"event_kind\":\"message\",\"busy_response\":\"sent\"}\n",
                "{\"timestamp_ms\":2,\"reason\":\"transport_closed\",\"correlation_id\":\"corr2\",\"command_kind\":\"send_message\",\"destination\":\"alice@localhost/mobile\",\"attempts\":1}\n"
            ),
        )
        .unwrap();

        let records = read_dead_letters(&path).unwrap();
        assert_eq!(records.len(), 2);
        assert_eq!(records[0].source, DeadLetterSource::Unknown);
        assert_eq!(records[0].payload_kind, "message");
        assert_eq!(records[1].source, DeadLetterSource::Unknown);
        assert_eq!(records[1].payload_kind, "send_message");
    }

    #[tokio::test]
    async fn test_service_record_and_replay() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("dead_letters-service.jsonl");
        let service = DeadLetterService::with_config(path, 8, 50, 1, 1);

        service
            .record(DeadLetterRecord::router(
                "session_mailbox_full",
                "alice@localhost".to_string(),
                "corr-router".to_string(),
                123,
                "message",
                "sent",
            ))
            .await
            .unwrap();
        service
            .record(DeadLetterRecord::xmpp_egress(
                "transport_closed",
                "corr-egress".to_string(),
                "send_message",
                Some("alice@localhost/mobile".to_string()),
                1,
            ))
            .await
            .unwrap();

        let records = service.replay().await.unwrap();
        assert_eq!(records.len(), 2);
        assert_eq!(records[0].source, DeadLetterSource::Router);
        assert_eq!(records[1].source, DeadLetterSource::XmppEgress);
    }
}
