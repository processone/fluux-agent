use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, Result};
use async_trait::async_trait;
use tokio::sync::{mpsc, oneshot, OnceCell};
use tokio::time::timeout;
use tracing::{error, info, warn};

use crate::actors::memory_actor::SessionMemoryWriter;
use crate::agent::memory::Reaction as StoredReaction;
use crate::config::Config;
use crate::xmpp::component::{ChatState, XmppCommand};
use crate::xmpp::stanzas::{self, IncomingReaction, MessageType};

#[async_trait]
pub trait ReactionResponder: Send + Sync {
    async fn respond_to_reaction(&self, jid: &str) -> Result<String>;
}

const SESSION_REACTION_BUSY_ERR: &str = "session_reaction_busy";
const SESSION_REACTION_UNAVAILABLE_ERR: &str = "session_reaction_unavailable";

struct SessionReactionRequest {
    reaction: IncomingReaction,
    cmd_tx: mpsc::Sender<XmppCommand>,
    reply_tx: oneshot::Sender<Result<()>>,
}

/// Mailbox-backed wrapper used by `SessionActor` to isolate reaction handling.
///
/// The inner `ReactionActor` logic is unchanged and remains reusable by the
/// per-conversation session actor path.
pub struct SessionReactionActor {
    config: Arc<Config>,
    memory_writer: Arc<dyn SessionMemoryWriter>,
    responder: Arc<dyn ReactionResponder>,
    enqueue_timeout: Duration,
    mailbox: OnceCell<mpsc::Sender<SessionReactionRequest>>,
}

impl SessionReactionActor {
    pub fn new(
        config: Arc<Config>,
        memory_writer: Arc<dyn SessionMemoryWriter>,
        responder: Arc<dyn ReactionResponder>,
    ) -> Self {
        let enqueue_timeout =
            Duration::from_millis(config.actors.tooling.skill_queue_timeout_ms.max(1));
        Self {
            config,
            memory_writer,
            responder,
            enqueue_timeout,
            mailbox: OnceCell::new(),
        }
    }

    pub async fn handle(
        &self,
        reaction: IncomingReaction,
        cmd_tx: mpsc::Sender<XmppCommand>,
    ) -> Result<()> {
        let (reply_tx, reply_rx) = oneshot::channel();
        let request = SessionReactionRequest {
            reaction,
            cmd_tx,
            reply_tx,
        };

        let tx = self.mailbox_tx().await;
        match timeout(self.enqueue_timeout, tx.send(request)).await {
            Ok(Ok(())) => {}
            Ok(Err(_)) => return Err(anyhow!(SESSION_REACTION_UNAVAILABLE_ERR)),
            Err(_) => return Err(anyhow!(SESSION_REACTION_BUSY_ERR)),
        }

        reply_rx
            .await
            .map_err(|_| anyhow!(SESSION_REACTION_UNAVAILABLE_ERR))?
    }

    async fn mailbox_tx(&self) -> mpsc::Sender<SessionReactionRequest> {
        self.mailbox
            .get_or_init(|| async { self.spawn_worker() })
            .await
            .clone()
    }

    fn spawn_worker(&self) -> mpsc::Sender<SessionReactionRequest> {
        let mailbox_size = self.config.actors.session_mailbox.max(1);
        let (tx, mut rx) = mpsc::channel::<SessionReactionRequest>(mailbox_size);
        let config = Arc::clone(&self.config);
        let memory_writer = Arc::clone(&self.memory_writer);
        let responder = Arc::clone(&self.responder);
        let processor = ReactionActor::new();

        tokio::spawn(async move {
            while let Some(request) = rx.recv().await {
                let result = processor
                    .handle(
                        request.reaction,
                        config.as_ref(),
                        memory_writer.as_ref(),
                        responder.as_ref(),
                        request.cmd_tx,
                    )
                    .await;
                let _ = request.reply_tx.send(result);
            }
        });

        tx
    }
}

#[derive(Default)]
pub struct ReactionActor;

impl ReactionActor {
    pub fn new() -> Self {
        Self
    }

    pub async fn handle(
        &self,
        reaction: IncomingReaction,
        config: &Config,
        memory_writer: &dyn SessionMemoryWriter,
        responder: &dyn ReactionResponder,
        cmd_tx: mpsc::Sender<XmppCommand>,
    ) -> Result<()> {
        let bare_from = stanzas::bare_jid(&reaction.from);
        let is_muc = reaction.message_type == MessageType::GroupChat;

        // Apply same authorization checks as regular messages
        if !is_muc {
            if !config.is_domain_allowed(&reaction.from) {
                warn!("Cross-domain reaction rejected from {}", reaction.from);
                return Ok(());
            }
            if !config.is_allowed(&reaction.from) {
                warn!("Unauthorized reaction from {}, ignoring", reaction.from);
                return Ok(());
            }
        }

        let emojis = reaction.emojis.join(" ");
        info!(
            "Reaction from {bare_from}: {emojis} on msg {}",
            reaction.message_id
        );

        // Store reaction as structured metadata in history
        let reaction_meta = StoredReaction {
            message_id: reaction.message_id.clone(),
            emojis: reaction.emojis.clone(),
        };

        let sender_label = if is_muc {
            let nick = reaction.from.split('/').nth(1).unwrap_or("unknown");
            format!("{nick}@muc")
        } else {
            bare_from.to_string()
        };

        if let Err(e) = memory_writer
            .store_message_full(
                bare_from,
                "user",
                "",
                None,
                Some(&sender_label),
                None,
                Some(reaction_meta),
            )
            .await
        {
            error!("Failed to store reaction: {e}");
        }

        // Send reaction through LLM — it decides whether to respond
        let reply_to = if is_muc {
            bare_from.to_string()
        } else {
            reaction.from.clone()
        };

        // Send <composing/> before the LLM call
        let msg_type_str = if is_muc { "groupchat" } else { "chat" };
        let _ = cmd_tx
            .send(XmppCommand::SendChatState {
                to: if is_muc {
                    bare_from.to_string()
                } else {
                    reply_to.clone()
                },
                state: ChatState::Composing,
                msg_type: msg_type_str.to_string(),
            })
            .await;

        let response = responder.respond_to_reaction(bare_from).await;
        match response {
            Ok(text) => {
                let out_id = uuid::Uuid::new_v4().to_string();
                let jid_key = bare_from.to_string();
                if let Err(e) = memory_writer
                    .store_message_structured(&jid_key, "assistant", &text, Some(&out_id), None)
                    .await
                {
                    error!("Failed to store reaction response: {e}");
                }
                if is_muc {
                    let _ = cmd_tx
                        .send(XmppCommand::SendMucMessage {
                            to: jid_key,
                            body: text,
                            id: Some(out_id),
                        })
                        .await;
                } else {
                    let _ = cmd_tx
                        .send(XmppCommand::SendMessage {
                            to: reply_to,
                            body: text,
                            id: Some(out_id),
                        })
                        .await;
                }
            }
            Err(e) => {
                error!("Error processing reaction: {e}");
                let _ = cmd_tx
                    .send(XmppCommand::SendChatState {
                        to: if is_muc {
                            bare_from.to_string()
                        } else {
                            reply_to
                        },
                        state: ChatState::Paused,
                        msg_type: msg_type_str.to_string(),
                    })
                    .await;
            }
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use tempfile::TempDir;
    use tokio::time::Duration;

    use super::*;
    use crate::agent::memory::Memory;
    use crate::config::{
        ActorMemoryConfig, ActorObservabilityConfig, ActorSupervisionConfig, ActorToolingConfig,
        ActorsConfig, AgentConfig, Config, ConnectionMode, KeepaliveConfig, LlmConfig,
        MemoryConfig, ServerConfig, SessionConfig, SkillsConfig,
    };

    struct StubResponder {
        response_text: String,
        fail: bool,
    }

    #[async_trait]
    impl ReactionResponder for StubResponder {
        async fn respond_to_reaction(&self, _jid: &str) -> Result<String> {
            if self.fail {
                anyhow::bail!("stub failure");
            }
            Ok(self.response_text.clone())
        }
    }

    fn test_config(allowed_jids: Vec<&str>) -> Config {
        Config {
            server: ServerConfig {
                host: "localhost".to_string(),
                port: 5222,
                mode: ConnectionMode::Client {
                    jid: "bot@localhost".to_string(),
                    password: "pass".to_string(),
                    resource: "fluux-agent".to_string(),
                    tls_verify: false,
                },
            },
            llm: LlmConfig {
                provider: "anthropic".to_string(),
                model: "claude-haiku-4-5-20250110".to_string(),
                api_key: String::new(),
                max_tokens_per_request: 4096,
                host: None,
            },
            agent: AgentConfig {
                name: "Test Agent".to_string(),
                allowed_jids: allowed_jids
                    .into_iter()
                    .map(|jid| jid.to_string())
                    .collect(),
                allowed_domains: vec![],
            },
            memory: MemoryConfig {
                backend: "markdown".to_string(),
                path: PathBuf::new(),
            },
            rooms: vec![],
            skills: SkillsConfig::default(),
            keepalive: KeepaliveConfig::default(),
            session: SessionConfig::default(),
            actors: ActorsConfig {
                router_mailbox: 64,
                session_mailbox: 8,
                max_active_sessions: 8,
                session_idle_ttl_secs: 1800,
                busy_retry_after_secs: 5,
                dedupe_ttl_secs: 600,
                dead_letter_path: PathBuf::from("data/dead_letters.jsonl"),
                tooling: ActorToolingConfig::default(),
                supervision: ActorSupervisionConfig::default(),
                memory: ActorMemoryConfig::default(),
                observability: ActorObservabilityConfig::default(),
            },
        }
    }

    #[tokio::test]
    async fn test_reaction_actor_emits_composing_and_reply_for_chat() {
        let tmp = TempDir::new().unwrap();
        let memory = Memory::open(tmp.path()).unwrap();
        let config = test_config(vec!["*"]);
        let responder = StubResponder {
            response_text: "thanks".to_string(),
            fail: false,
        };
        let actor = ReactionActor::new();
        let reaction = IncomingReaction {
            from: "alice@localhost/phone".to_string(),
            to: "bot@localhost".to_string(),
            message_id: "msg-1".to_string(),
            emojis: vec!["👍".to_string()],
            message_type: MessageType::Chat,
        };

        let (cmd_tx, mut cmd_rx) = mpsc::channel::<XmppCommand>(8);
        actor
            .handle(reaction, &config, &memory, &responder, cmd_tx)
            .await
            .unwrap();

        let first = tokio::time::timeout(Duration::from_millis(100), cmd_rx.recv())
            .await
            .unwrap()
            .unwrap();
        let second = tokio::time::timeout(Duration::from_millis(100), cmd_rx.recv())
            .await
            .unwrap()
            .unwrap();

        match first {
            XmppCommand::SendChatState { to, msg_type, .. } => {
                assert_eq!(to, "alice@localhost/phone");
                assert_eq!(msg_type, "chat");
            }
            other => panic!("expected chat state, got {other:?}"),
        }

        match second {
            XmppCommand::SendMessage { to, body, .. } => {
                assert_eq!(to, "alice@localhost/phone");
                assert_eq!(body, "thanks");
            }
            other => panic!("expected message, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn test_reaction_actor_ignores_unauthorized_chat_reaction() {
        let tmp = TempDir::new().unwrap();
        let memory = Memory::open(tmp.path()).unwrap();
        let config = test_config(vec!["admin@localhost"]);
        let responder = StubResponder {
            response_text: "should-not-send".to_string(),
            fail: false,
        };
        let actor = ReactionActor::new();
        let reaction = IncomingReaction {
            from: "alice@localhost/phone".to_string(),
            to: "bot@localhost".to_string(),
            message_id: "msg-2".to_string(),
            emojis: vec!["🔥".to_string()],
            message_type: MessageType::Chat,
        };

        let (cmd_tx, mut cmd_rx) = mpsc::channel::<XmppCommand>(8);
        actor
            .handle(reaction, &config, &memory, &responder, cmd_tx)
            .await
            .unwrap();

        let maybe_cmd = tokio::time::timeout(Duration::from_millis(100), cmd_rx.recv()).await;
        assert!(
            maybe_cmd.is_err() || maybe_cmd.unwrap().is_none(),
            "expected no outbound commands for unauthorized reaction"
        );
    }
}
