use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, Result};
use tokio::sync::{mpsc, oneshot, OnceCell};
use tokio::time::timeout;
use tracing::{debug, info, warn};

use crate::config::{
    ActorMemoryConfig, ActorObservabilityConfig, ActorSupervisionConfig, ActorToolingConfig,
    ActorsConfig, AgentConfig, Config, ConnectionMode, KeepaliveConfig, LlmConfig, MemoryConfig,
    ServerConfig, SessionConfig, SkillsConfig,
};
use crate::xmpp::component::XmppCommand;
use crate::xmpp::stanzas::{self, IncomingPresence, PresenceType};

const SESSION_PRESENCE_BUSY_ERR: &str = "session_presence_busy";
const SESSION_PRESENCE_UNAVAILABLE_ERR: &str = "session_presence_unavailable";

struct SessionPresenceRequest {
    pres: IncomingPresence,
    cmd_tx: mpsc::Sender<XmppCommand>,
    reply_tx: oneshot::Sender<Result<()>>,
}

/// Mailbox-backed wrapper used by `SessionActor` to isolate presence handling.
///
/// The inner `PresenceActor` logic is unchanged and remains reusable by the
/// per-conversation session actor path.
pub struct SessionPresenceActor {
    config: Arc<Config>,
    enqueue_timeout: Duration,
    mailbox: OnceCell<mpsc::Sender<SessionPresenceRequest>>,
}

impl SessionPresenceActor {
    pub fn new(config: Arc<Config>) -> Self {
        let enqueue_timeout =
            Duration::from_millis(config.actors.tooling.skill_queue_timeout_ms.max(1));
        Self {
            config,
            enqueue_timeout,
            mailbox: OnceCell::new(),
        }
    }

    pub async fn handle(
        &self,
        pres: IncomingPresence,
        cmd_tx: mpsc::Sender<XmppCommand>,
    ) -> Result<()> {
        let (reply_tx, reply_rx) = oneshot::channel();
        let request = SessionPresenceRequest {
            pres,
            cmd_tx,
            reply_tx,
        };

        let tx = self.mailbox_tx().await;
        match timeout(self.enqueue_timeout, tx.send(request)).await {
            Ok(Ok(())) => {}
            Ok(Err(_)) => return Err(anyhow!(SESSION_PRESENCE_UNAVAILABLE_ERR)),
            Err(_) => return Err(anyhow!(SESSION_PRESENCE_BUSY_ERR)),
        }

        reply_rx
            .await
            .map_err(|_| anyhow!(SESSION_PRESENCE_UNAVAILABLE_ERR))?
    }

    async fn mailbox_tx(&self) -> mpsc::Sender<SessionPresenceRequest> {
        self.mailbox
            .get_or_init(|| async { self.spawn_worker() })
            .await
            .clone()
    }

    fn spawn_worker(&self) -> mpsc::Sender<SessionPresenceRequest> {
        let mailbox_size = self.config.actors.session_mailbox.max(1);
        let (tx, mut rx) = mpsc::channel::<SessionPresenceRequest>(mailbox_size);
        let config = Arc::clone(&self.config);
        let processor = PresenceActor::new();

        tokio::spawn(async move {
            while let Some(request) = rx.recv().await {
                let result = processor
                    .handle(request.pres, config.as_ref(), request.cmd_tx)
                    .await;
                let _ = request.reply_tx.send(result);
            }
        });

        tx
    }
}

#[derive(Default)]
pub struct PresenceActor;

impl PresenceActor {
    pub fn new() -> Self {
        Self
    }

    pub async fn handle(
        &self,
        pres: IncomingPresence,
        config: &Config,
        cmd_tx: mpsc::Sender<XmppCommand>,
    ) -> Result<()> {
        let bare_jid = stanzas::bare_jid(&pres.from);

        // Domain-level security check for subscription requests
        if matches!(pres.presence_type, PresenceType::Subscribe)
            && !config.is_domain_allowed(&pres.from)
        {
            warn!("Cross-domain subscription rejected from {bare_jid} (domain not allowed)");
            return Ok(());
        }

        match pres.presence_type {
            PresenceType::Subscribe => {
                // Auto-accept subscription requests from allowed JIDs
                if config.is_allowed(&pres.from) {
                    info!("Auto-accepting subscription from {bare_jid}");
                    let _ = cmd_tx
                        .send(XmppCommand::SendRaw(stanzas::build_subscribed(bare_jid)))
                        .await;
                } else {
                    warn!("Ignoring subscription request from unauthorized JID: {bare_jid}");
                }
            }
            PresenceType::Subscribed => {
                info!("Subscription accepted by {bare_jid}");
            }
            PresenceType::Available => {
                debug!("{bare_jid} is now online");
            }
            PresenceType::Unavailable => {
                debug!("{bare_jid} went offline");
            }
            _ => {
                debug!("Presence from {bare_jid}: {:?}", pres.presence_type);
            }
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use tokio::time::Duration;

    use super::*;

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
    async fn test_presence_actor_accepts_allowed_subscription() {
        let actor = PresenceActor::new();
        let config = test_config(vec!["*"]);
        let presence = IncomingPresence {
            from: "alice@localhost/phone".to_string(),
            presence_type: PresenceType::Subscribe,
        };
        let (cmd_tx, mut cmd_rx) = mpsc::channel::<XmppCommand>(4);

        actor.handle(presence, &config, cmd_tx).await.unwrap();

        let cmd = tokio::time::timeout(Duration::from_millis(100), cmd_rx.recv())
            .await
            .unwrap()
            .unwrap();
        match cmd {
            XmppCommand::SendRaw(raw) => {
                assert!(raw.contains("subscribed"));
                assert!(raw.contains("alice@localhost"));
            }
            other => panic!("expected SendRaw subscribed stanza, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn test_presence_actor_rejects_cross_domain_subscription() {
        let actor = PresenceActor::new();
        let config = test_config(vec!["*"]);
        let presence = IncomingPresence {
            from: "alice@example.com/phone".to_string(),
            presence_type: PresenceType::Subscribe,
        };
        let (cmd_tx, mut cmd_rx) = mpsc::channel::<XmppCommand>(4);

        actor.handle(presence, &config, cmd_tx).await.unwrap();

        let maybe_cmd = tokio::time::timeout(Duration::from_millis(100), cmd_rx.recv()).await;
        assert!(
            maybe_cmd.is_err() || maybe_cmd.unwrap().is_none(),
            "expected no outbound command for cross-domain subscription"
        );
    }
}
