use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use std::{
    collections::hash_map::DefaultHasher,
    hash::{Hash, Hasher},
};

use anyhow::{anyhow, Result};
use async_trait::async_trait;
use tokio::sync::{mpsc, oneshot, OnceCell};
use tokio::time::{timeout, Instant};
use tracing::warn;

use super::dead_letter::{DeadLetterMemoryReplayPayload, DeadLetterRecord, DeadLetterService};
use super::observability;
use crate::agent::memory::{Attachment, Memory, Reaction};

const MEMORY_BUSY_ERR: &str = "memory_busy";
const MEMORY_UNAVAILABLE_ERR: &str = "memory_unavailable";

enum MemoryRequest {
    StoreMessageStructured {
        jid: String,
        role: String,
        content: String,
        msg_id: Option<String>,
        sender: Option<String>,
        reply_tx: oneshot::Sender<Result<()>>,
    },
    StoreMessageFull {
        jid: String,
        role: String,
        content: String,
        msg_id: Option<String>,
        sender: Option<String>,
        attachments: Option<Vec<Attachment>>,
        reaction: Option<Reaction>,
        reply_tx: oneshot::Sender<Result<()>>,
    },
}

impl MemoryRequest {
    fn conversation_id(&self) -> &str {
        match self {
            Self::StoreMessageStructured { jid, .. } | Self::StoreMessageFull { jid, .. } => jid,
        }
    }

    fn correlation_id(&self) -> Option<&str> {
        match self {
            Self::StoreMessageStructured { msg_id, .. } | Self::StoreMessageFull { msg_id, .. } => {
                msg_id.as_deref()
            }
        }
    }

    fn operation(&self) -> &'static str {
        match self {
            Self::StoreMessageStructured { .. } => "store_message_structured",
            Self::StoreMessageFull { .. } => "store_message_full",
        }
    }

    fn replay_payload(&self) -> DeadLetterMemoryReplayPayload {
        match self {
            Self::StoreMessageStructured {
                jid,
                role,
                content,
                msg_id,
                sender,
                ..
            } => DeadLetterMemoryReplayPayload::StoreMessageStructured {
                jid: jid.clone(),
                role: role.clone(),
                content: content.clone(),
                msg_id: msg_id.clone(),
                sender: sender.clone(),
            },
            Self::StoreMessageFull {
                jid,
                role,
                content,
                msg_id,
                sender,
                attachments,
                reaction,
                ..
            } => DeadLetterMemoryReplayPayload::StoreMessageFull {
                jid: jid.clone(),
                role: role.clone(),
                content: content.clone(),
                msg_id: msg_id.clone(),
                sender: sender.clone(),
                attachments: attachments.clone(),
                reaction: reaction.clone(),
            },
        }
    }
}

/// Session write-lane abstraction so runtime components can be decoupled from
/// direct file writes.
#[async_trait]
pub trait SessionMemoryWriter: Send + Sync {
    async fn store_message_structured(
        &self,
        jid: &str,
        role: &str,
        content: &str,
        msg_id: Option<&str>,
        sender: Option<&str>,
    ) -> Result<()>;

    async fn store_message_full(
        &self,
        jid: &str,
        role: &str,
        content: &str,
        msg_id: Option<&str>,
        sender: Option<&str>,
        attachments: Option<Vec<Attachment>>,
        reaction: Option<Reaction>,
    ) -> Result<()>;
}

#[async_trait]
impl SessionMemoryWriter for Memory {
    async fn store_message_structured(
        &self,
        jid: &str,
        role: &str,
        content: &str,
        msg_id: Option<&str>,
        sender: Option<&str>,
    ) -> Result<()> {
        self.store_message_structured(jid, role, content, msg_id, sender)
    }

    async fn store_message_full(
        &self,
        jid: &str,
        role: &str,
        content: &str,
        msg_id: Option<&str>,
        sender: Option<&str>,
        attachments: Option<Vec<Attachment>>,
        reaction: Option<Reaction>,
    ) -> Result<()> {
        self.store_message_full(jid, role, content, msg_id, sender, attachments, reaction)
    }
}

/// Mailbox-backed single-writer actor for session persistence.
pub struct MemoryActor {
    memory: Arc<Memory>,
    dead_letter: Arc<DeadLetterService>,
    enqueue_timeout: Duration,
    mailbox_size: usize,
    write_batch_max: usize,
    write_batch_max_delay: Duration,
    mailbox: OnceCell<mpsc::Sender<MemoryRequest>>,
}

impl MemoryActor {
    pub fn new(
        memory: Arc<Memory>,
        dead_letter_path: PathBuf,
        enqueue_timeout_ms: u64,
        mailbox_size: usize,
        write_batch_max: usize,
        write_batch_max_delay_ms: u64,
    ) -> Self {
        Self::with_dead_letter_service(
            memory,
            Arc::new(DeadLetterService::new(dead_letter_path)),
            enqueue_timeout_ms,
            mailbox_size,
            write_batch_max,
            write_batch_max_delay_ms,
        )
    }

    pub fn with_dead_letter_service(
        memory: Arc<Memory>,
        dead_letter: Arc<DeadLetterService>,
        enqueue_timeout_ms: u64,
        mailbox_size: usize,
        write_batch_max: usize,
        write_batch_max_delay_ms: u64,
    ) -> Self {
        Self {
            memory,
            dead_letter,
            enqueue_timeout: Duration::from_millis(enqueue_timeout_ms.max(1)),
            mailbox_size: mailbox_size.max(1),
            write_batch_max: write_batch_max.max(1),
            write_batch_max_delay: Duration::from_millis(write_batch_max_delay_ms),
            mailbox: OnceCell::new(),
        }
    }

    async fn mailbox_tx(&self) -> mpsc::Sender<MemoryRequest> {
        self.mailbox
            .get_or_init(|| async { self.spawn_worker() })
            .await
            .clone()
    }

    fn spawn_worker(&self) -> mpsc::Sender<MemoryRequest> {
        let (tx, mut rx) = mpsc::channel::<MemoryRequest>(self.mailbox_size);
        let memory = Arc::clone(&self.memory);
        let write_batch_max = self.write_batch_max;
        let write_batch_max_delay = self.write_batch_max_delay;

        tokio::spawn(async move {
            while let Some(first_request) = rx.recv().await {
                observability::inc_counter("dequeue.memory");
                observability::add_gauge("mailbox.memory.depth", -1);
                let mut batch = Vec::with_capacity(write_batch_max);
                batch.push(first_request);

                if write_batch_max_delay > Duration::ZERO && write_batch_max > 1 {
                    let deadline = Instant::now() + write_batch_max_delay;
                    while batch.len() < write_batch_max {
                        let now = Instant::now();
                        if now >= deadline {
                            break;
                        }
                        let wait_for = deadline - now;
                        match timeout(wait_for, rx.recv()).await {
                            Ok(Some(request)) => {
                                observability::inc_counter("dequeue.memory");
                                observability::add_gauge("mailbox.memory.depth", -1);
                                batch.push(request);
                            }
                            Ok(None) => break,
                            Err(_) => break,
                        }
                    }
                }

                for request in batch {
                    process_request(memory.as_ref(), request);
                }
            }
        });

        tx
    }

    async fn dispatch(&self, request: MemoryRequest) -> Result<()> {
        let conversation_id = request.conversation_id().to_string();
        let correlation_id = request
            .correlation_id()
            .map(ToString::to_string)
            .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
        let operation = request.operation();
        let replay_payload = request.replay_payload();
        let tx = self.mailbox_tx().await;
        match timeout(self.enqueue_timeout, tx.send(request)).await {
            Ok(Ok(())) => {
                observability::inc_counter("enqueue.memory");
                observability::add_gauge("mailbox.memory.depth", 1);
            }
            Ok(Err(_)) => {
                observability::inc_counter("overflow.memory.unavailable");
                self.record_dead_letter(
                    "memory_mailbox_closed",
                    conversation_id,
                    correlation_id,
                    operation,
                    replay_payload,
                )
                .await;
                return Err(anyhow!(MEMORY_UNAVAILABLE_ERR));
            }
            Err(_) => {
                observability::inc_counter("overflow.memory.busy");
                self.record_dead_letter(
                    "memory_enqueue_timeout",
                    conversation_id,
                    correlation_id,
                    operation,
                    replay_payload,
                )
                .await;
                return Err(anyhow!(MEMORY_BUSY_ERR));
            }
        }
        Ok(())
    }

    async fn record_dead_letter(
        &self,
        reason: &str,
        conversation_id: String,
        correlation_id: String,
        operation: &str,
        replay_payload: DeadLetterMemoryReplayPayload,
    ) {
        let record = DeadLetterRecord::memory_actor_with_payload(
            reason,
            conversation_id,
            correlation_id,
            operation,
            serde_json::to_value(replay_payload).ok(),
        );
        if let Err(err) = self.dead_letter.record(record).await {
            warn!(
                reason = reason,
                operation = operation,
                dead_letter_path = %self.dead_letter.path().display(),
                "Failed to persist memory dead-letter record: {err}",
            );
        } else {
            observability::inc_counter("dead_letter.memory_actor");
        }
    }
}

fn process_request(memory: &Memory, request: MemoryRequest) {
    let started = std::time::Instant::now();
    match request {
        MemoryRequest::StoreMessageStructured {
            jid,
            role,
            content,
            msg_id,
            sender,
            reply_tx,
        } => {
            let result = memory.store_message_structured(
                &jid,
                &role,
                &content,
                msg_id.as_deref(),
                sender.as_deref(),
            );
            let _ = reply_tx.send(result);
        }
        MemoryRequest::StoreMessageFull {
            jid,
            role,
            content,
            msg_id,
            sender,
            attachments,
            reaction,
            reply_tx,
        } => {
            let result = memory.store_message_full(
                &jid,
                &role,
                &content,
                msg_id.as_deref(),
                sender.as_deref(),
                attachments,
                reaction,
            );
            let _ = reply_tx.send(result);
        }
    }
    observability::observe_duration("latency.memory_write_ms", started.elapsed());
}

#[async_trait]
impl SessionMemoryWriter for MemoryActor {
    async fn store_message_structured(
        &self,
        jid: &str,
        role: &str,
        content: &str,
        msg_id: Option<&str>,
        sender: Option<&str>,
    ) -> Result<()> {
        let (reply_tx, reply_rx) = oneshot::channel();
        let request = MemoryRequest::StoreMessageStructured {
            jid: jid.to_string(),
            role: role.to_string(),
            content: content.to_string(),
            msg_id: msg_id.map(ToString::to_string),
            sender: sender.map(ToString::to_string),
            reply_tx,
        };
        self.dispatch(request).await?;
        reply_rx
            .await
            .map_err(|_| anyhow!(MEMORY_UNAVAILABLE_ERR))?
    }

    async fn store_message_full(
        &self,
        jid: &str,
        role: &str,
        content: &str,
        msg_id: Option<&str>,
        sender: Option<&str>,
        attachments: Option<Vec<Attachment>>,
        reaction: Option<Reaction>,
    ) -> Result<()> {
        let (reply_tx, reply_rx) = oneshot::channel();
        let request = MemoryRequest::StoreMessageFull {
            jid: jid.to_string(),
            role: role.to_string(),
            content: content.to_string(),
            msg_id: msg_id.map(ToString::to_string),
            sender: sender.map(ToString::to_string),
            attachments,
            reaction,
            reply_tx,
        };
        self.dispatch(request).await?;
        reply_rx
            .await
            .map_err(|_| anyhow!(MEMORY_UNAVAILABLE_ERR))?
    }
}

/// Hash-sharded session writer that routes each conversation key to a
/// deterministic lane, preserving per-conversation ordering while allowing
/// cross-conversation parallelism.
pub struct ShardedMemoryWriter {
    shards: Vec<Arc<MemoryActor>>,
}

impl ShardedMemoryWriter {
    pub fn new(shards: Vec<Arc<MemoryActor>>) -> Self {
        assert!(
            !shards.is_empty(),
            "sharded writer requires at least one shard"
        );
        Self { shards }
    }

    fn shard_index_for(&self, jid: &str) -> usize {
        let mut hasher = DefaultHasher::new();
        jid.hash(&mut hasher);
        (hasher.finish() as usize) % self.shards.len()
    }

    #[cfg(test)]
    fn shard_index_for_test(&self, jid: &str) -> usize {
        self.shard_index_for(jid)
    }

    fn shard_for(&self, jid: &str) -> &Arc<MemoryActor> {
        &self.shards[self.shard_index_for(jid)]
    }
}

#[async_trait]
impl SessionMemoryWriter for ShardedMemoryWriter {
    async fn store_message_structured(
        &self,
        jid: &str,
        role: &str,
        content: &str,
        msg_id: Option<&str>,
        sender: Option<&str>,
    ) -> Result<()> {
        self.shard_for(jid)
            .store_message_structured(jid, role, content, msg_id, sender)
            .await
    }

    async fn store_message_full(
        &self,
        jid: &str,
        role: &str,
        content: &str,
        msg_id: Option<&str>,
        sender: Option<&str>,
        attachments: Option<Vec<Attachment>>,
        reaction: Option<Reaction>,
    ) -> Result<()> {
        self.shard_for(jid)
            .store_message_full(jid, role, content, msg_id, sender, attachments, reaction)
            .await
    }
}

#[cfg(test)]
mod tests {
    use tempfile::TempDir;

    use super::*;
    use crate::actors::dead_letter::{
        read_dead_letters as read_dead_letter_records, DeadLetterMemoryReplayPayload,
        DeadLetterSource,
    };

    fn test_memory(tmp: &TempDir) -> Arc<Memory> {
        Arc::new(Memory::open(tmp.path()).expect("memory open"))
    }

    fn actor_for_tests(memory: Arc<Memory>, dead_letter_path: PathBuf) -> MemoryActor {
        MemoryActor::new(memory, dead_letter_path, 20, 4, 4, 0)
    }

    fn sharded_for_tests(memory: Arc<Memory>, shard_count: usize) -> ShardedMemoryWriter {
        let shards = (0..shard_count)
            .map(|_| {
                Arc::new(actor_for_tests(
                    Arc::clone(&memory),
                    std::env::temp_dir().join(format!(
                        "fluux-memory-dead-letters-{}.jsonl",
                        uuid::Uuid::new_v4()
                    )),
                ))
            })
            .collect();
        ShardedMemoryWriter::new(shards)
    }

    #[tokio::test]
    async fn test_memory_actor_store_message_structured_success() {
        let tmp = TempDir::new().expect("tmpdir");
        let memory = test_memory(&tmp);
        let actor = actor_for_tests(
            Arc::clone(&memory),
            tmp.path().join("memory-structured-dead-letters.jsonl"),
        );

        actor
            .store_message_structured(
                "alice@test",
                "user",
                "hello",
                Some("m1"),
                Some("alice@test"),
            )
            .await
            .expect("store should succeed");

        let history = memory.get_history("alice@test", 10).expect("history");
        assert_eq!(history.len(), 1);
        assert_eq!(history[0].role, "user");
    }

    #[tokio::test]
    async fn test_memory_actor_returns_busy_when_mailbox_saturated() {
        let tmp = TempDir::new().expect("tmpdir");
        let memory = test_memory(&tmp);
        let dead_letter_path = tmp.path().join("memory-busy-dead-letters.jsonl");
        let actor = actor_for_tests(memory, dead_letter_path.clone());

        let (tx, _rx) = mpsc::channel::<MemoryRequest>(1);
        actor.mailbox.set(tx.clone()).expect("mailbox set");

        let (prefill_reply_tx, _prefill_reply_rx) = oneshot::channel();
        tx.send(MemoryRequest::StoreMessageStructured {
            jid: "alice@test".to_string(),
            role: "user".to_string(),
            content: "prefill".to_string(),
            msg_id: None,
            sender: None,
            reply_tx: prefill_reply_tx,
        })
        .await
        .expect("prefill send");

        let err = actor
            .store_message_structured("alice@test", "user", "busy", None, None)
            .await
            .expect_err("expected busy error");
        assert_eq!(err.to_string(), MEMORY_BUSY_ERR);

        let dead_letters = read_dead_letter_records(&dead_letter_path).unwrap();
        assert_eq!(dead_letters.len(), 1);
        assert_eq!(dead_letters[0].source, DeadLetterSource::MemoryActor);
        assert_eq!(dead_letters[0].reason, "memory_enqueue_timeout");
        assert_eq!(
            dead_letters[0].conversation_id.as_deref(),
            Some("alice@test")
        );
        assert_eq!(dead_letters[0].payload_kind, "store_message_structured");
        let payload: DeadLetterMemoryReplayPayload =
            serde_json::from_value(dead_letters[0].payload.clone().unwrap()).unwrap();
        assert!(matches!(
            payload,
            DeadLetterMemoryReplayPayload::StoreMessageStructured { .. }
        ));
    }

    #[tokio::test]
    async fn test_memory_actor_returns_unavailable_when_mailbox_closed() {
        let tmp = TempDir::new().expect("tmpdir");
        let memory = test_memory(&tmp);
        let dead_letter_path = tmp.path().join("memory-unavailable-dead-letters.jsonl");
        let actor = actor_for_tests(memory, dead_letter_path.clone());

        let (tx, rx) = mpsc::channel::<MemoryRequest>(1);
        drop(rx);
        actor.mailbox.set(tx).expect("mailbox set");

        let err = actor
            .store_message_structured("alice@test", "user", "down", None, None)
            .await
            .expect_err("expected unavailable error");
        assert_eq!(err.to_string(), MEMORY_UNAVAILABLE_ERR);

        let dead_letters = read_dead_letter_records(&dead_letter_path).unwrap();
        assert_eq!(dead_letters.len(), 1);
        assert_eq!(dead_letters[0].source, DeadLetterSource::MemoryActor);
        assert_eq!(dead_letters[0].reason, "memory_mailbox_closed");
        assert_eq!(
            dead_letters[0].conversation_id.as_deref(),
            Some("alice@test")
        );
        assert!(dead_letters[0].payload.is_some());
    }

    #[tokio::test]
    async fn test_sharded_memory_writer_writes_across_conversations() {
        let tmp = TempDir::new().expect("tmpdir");
        let memory = test_memory(&tmp);
        let sharded = sharded_for_tests(Arc::clone(&memory), 4);

        sharded
            .store_message_structured("alice@test", "user", "hello-a", Some("a1"), None)
            .await
            .expect("alice write");
        sharded
            .store_message_structured("bob@test", "user", "hello-b", Some("b1"), None)
            .await
            .expect("bob write");

        let alice_history = memory.get_history("alice@test", 10).expect("alice history");
        let bob_history = memory.get_history("bob@test", 10).expect("bob history");
        assert_eq!(alice_history.len(), 1);
        assert_eq!(bob_history.len(), 1);
        assert_eq!(alice_history[0].content, "hello-a");
        assert_eq!(bob_history[0].content, "hello-b");
    }

    #[test]
    fn test_sharded_memory_writer_consistent_hashing_for_same_jid() {
        let tmp = TempDir::new().expect("tmpdir");
        let memory = test_memory(&tmp);
        let sharded = sharded_for_tests(memory, 8);

        let idx_a_1 = sharded.shard_index_for_test("alice@test");
        let idx_a_2 = sharded.shard_index_for_test("alice@test");
        let idx_b = sharded.shard_index_for_test("bob@test");

        assert_eq!(idx_a_1, idx_a_2, "same jid must map to same shard");
        assert!(idx_a_1 < 8);
        assert!(idx_b < 8);
    }
}
