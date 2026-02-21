use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, Result};
use tokio::sync::{mpsc, oneshot, OnceCell};
use tokio::time::timeout;
use tracing::{debug, error, info, warn};

use crate::config::{
    ActorMemoryConfig, ActorObservabilityConfig, ActorSupervisionConfig, ActorToolingConfig,
    ActorsConfig, AgentConfig, Config, ConnectionMode, KeepaliveConfig, LlmConfig, MemoryConfig,
    RoomConfig, ServerConfig, SessionConfig, SkillsConfig,
};
use crate::xmpp::component::{DisconnectReason, XmppCommand};

const SESSION_CONTROL_BUSY_ERR: &str = "session_control_busy";
const SESSION_CONTROL_UNAVAILABLE_ERR: &str = "session_control_unavailable";

enum SessionControlEvent {
    Connected,
    StreamError(String),
    Error(String),
    ReadTimeout,
}

struct SessionControlRequest {
    event: SessionControlEvent,
    cmd_tx: mpsc::Sender<XmppCommand>,
    reply_tx: oneshot::Sender<Result<Option<DisconnectReason>>>,
}

/// Mailbox-backed wrapper used by `SessionActor` to isolate control handling.
///
/// The inner `ControlActor` logic is unchanged and remains reusable by the
/// per-conversation session actor path.
pub struct SessionControlActor {
    config: Arc<Config>,
    enqueue_timeout: Duration,
    mailbox: OnceCell<mpsc::Sender<SessionControlRequest>>,
}

impl SessionControlActor {
    pub fn new(config: Arc<Config>) -> Self {
        let enqueue_timeout =
            Duration::from_millis(config.actors.tooling.skill_queue_timeout_ms.max(1));
        Self {
            config,
            enqueue_timeout,
            mailbox: OnceCell::new(),
        }
    }

    pub async fn on_connected(&self, cmd_tx: mpsc::Sender<XmppCommand>) -> Result<()> {
        let _ = self
            .dispatch(SessionControlEvent::Connected, cmd_tx)
            .await?;
        Ok(())
    }

    pub async fn on_stream_error(
        &self,
        condition: String,
        cmd_tx: mpsc::Sender<XmppCommand>,
    ) -> Result<DisconnectReason> {
        Ok(self
            .dispatch(SessionControlEvent::StreamError(condition), cmd_tx)
            .await?
            .unwrap_or(DisconnectReason::ConnectionLost))
    }

    pub async fn on_error(&self, message: String, cmd_tx: mpsc::Sender<XmppCommand>) -> Result<()> {
        let _ = self
            .dispatch(SessionControlEvent::Error(message), cmd_tx)
            .await?;
        Ok(())
    }

    pub async fn on_read_timeout(
        &self,
        cmd_tx: mpsc::Sender<XmppCommand>,
    ) -> Result<Option<DisconnectReason>> {
        self.dispatch(SessionControlEvent::ReadTimeout, cmd_tx)
            .await
    }

    async fn dispatch(
        &self,
        event: SessionControlEvent,
        cmd_tx: mpsc::Sender<XmppCommand>,
    ) -> Result<Option<DisconnectReason>> {
        let (reply_tx, reply_rx) = oneshot::channel();
        let request = SessionControlRequest {
            event,
            cmd_tx,
            reply_tx,
        };

        let tx = self.mailbox_tx().await;
        match timeout(self.enqueue_timeout, tx.send(request)).await {
            Ok(Ok(())) => {}
            Ok(Err(_)) => return Err(anyhow!(SESSION_CONTROL_UNAVAILABLE_ERR)),
            Err(_) => return Err(anyhow!(SESSION_CONTROL_BUSY_ERR)),
        }

        reply_rx
            .await
            .map_err(|_| anyhow!(SESSION_CONTROL_UNAVAILABLE_ERR))?
    }

    async fn mailbox_tx(&self) -> mpsc::Sender<SessionControlRequest> {
        self.mailbox
            .get_or_init(|| async { self.spawn_worker() })
            .await
            .clone()
    }

    fn spawn_worker(&self) -> mpsc::Sender<SessionControlRequest> {
        let mailbox_size = self.config.actors.session_mailbox.max(1);
        let (tx, mut rx) = mpsc::channel::<SessionControlRequest>(mailbox_size);
        let config = Arc::clone(&self.config);
        let processor = ControlActor::new();

        tokio::spawn(async move {
            while let Some(request) = rx.recv().await {
                let result = match request.event {
                    SessionControlEvent::Connected => {
                        processor
                            .on_connected(config.as_ref(), request.cmd_tx)
                            .await;
                        Ok(None)
                    }
                    SessionControlEvent::StreamError(condition) => {
                        Ok(Some(processor.on_stream_error(condition)))
                    }
                    SessionControlEvent::Error(message) => {
                        processor.on_error(message);
                        Ok(None)
                    }
                    SessionControlEvent::ReadTimeout => {
                        Ok(processor.on_read_timeout(request.cmd_tx).await)
                    }
                };
                let _ = request.reply_tx.send(result);
            }
        });

        tx
    }
}

#[derive(Default)]
pub struct ControlActor;

impl ControlActor {
    pub fn new() -> Self {
        Self
    }

    pub async fn on_connected(&self, config: &Config, cmd_tx: mpsc::Sender<XmppCommand>) {
        info!("✓ Agent is online and ready");

        // Join configured MUC rooms (XEP-0045)
        for room in &config.rooms {
            info!("Joining MUC room: {} as {}", room.jid, room.nick);
            let _ = cmd_tx
                .send(XmppCommand::JoinMuc {
                    room: room.jid.clone(),
                    nick: room.nick.clone(),
                })
                .await;
        }
    }

    pub fn on_stream_error(&self, condition: String) -> DisconnectReason {
        error!("XMPP stream error: {condition}");
        if condition == "conflict" {
            DisconnectReason::Conflict
        } else {
            DisconnectReason::StreamError(condition)
        }
    }

    pub fn on_error(&self, message: String) {
        error!("XMPP error: {message}");
    }

    pub async fn on_read_timeout(
        &self,
        cmd_tx: mpsc::Sender<XmppCommand>,
    ) -> Option<DisconnectReason> {
        debug!("Read timeout — probing connection with ping");
        if cmd_tx.send(XmppCommand::Ping).await.is_err() {
            warn!("Probe ping failed (channel closed) — connection lost");
            return Some(DisconnectReason::ConnectionLost);
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use tokio::time::Duration;

    use super::*;

    fn test_config() -> Config {
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
                allowed_jids: vec!["*".to_string()],
                allowed_domains: vec![],
            },
            memory: MemoryConfig {
                backend: "markdown".to_string(),
                path: PathBuf::new(),
            },
            rooms: vec![RoomConfig {
                jid: "room@conference.localhost".to_string(),
                nick: "bot".to_string(),
            }],
            skills: SkillsConfig::default(),
            keepalive: KeepaliveConfig::default(),
            session: SessionConfig::default(),
            actors: ActorsConfig {
                enabled: true,
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
    async fn test_control_actor_connected_joins_rooms() {
        let actor = ControlActor::new();
        let config = test_config();
        let (cmd_tx, mut cmd_rx) = mpsc::channel::<XmppCommand>(4);

        actor.on_connected(&config, cmd_tx).await;

        let cmd = tokio::time::timeout(Duration::from_millis(100), cmd_rx.recv())
            .await
            .unwrap()
            .unwrap();
        match cmd {
            XmppCommand::JoinMuc { room, nick } => {
                assert_eq!(room, "room@conference.localhost");
                assert_eq!(nick, "bot");
            }
            other => panic!("expected JoinMuc command, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn test_control_actor_stream_error_conflict_maps_reason() {
        let actor = ControlActor::new();
        let reason = actor.on_stream_error("conflict".to_string());
        assert_eq!(reason, DisconnectReason::Conflict);
    }

    #[tokio::test]
    async fn test_control_actor_read_timeout_returns_disconnect_when_channel_closed() {
        let actor = ControlActor::new();
        let (cmd_tx, cmd_rx) = mpsc::channel::<XmppCommand>(1);
        drop(cmd_rx);

        let reason = actor.on_read_timeout(cmd_tx).await;
        assert_eq!(reason, Some(DisconnectReason::ConnectionLost));
    }
}
