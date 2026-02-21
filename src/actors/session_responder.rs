use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, Result};
use async_trait::async_trait;
use tokio::sync::{mpsc, OnceCell, Semaphore};
use tokio::time::timeout;
use tracing::error;

use super::attachment::AttachmentActor;
use super::command::CommandActor;
use super::dead_letter::DeadLetterService;
use super::inference::InferenceActor;
use super::memory_actor::SessionMemoryWriter;
use super::message::{CommandReply, MessageResponder};
use super::reaction::ReactionResponder;
use super::user_error;
use crate::agent::files::AttachmentDownloader;
use crate::agent::memory::Memory;
use crate::config::Config;
use crate::llm::LlmClient;
use crate::skills::SkillRegistry;
use crate::xmpp::component::{ChatState, XmppCommand};
use crate::xmpp::stanzas::IncomingMessage;

const ATTACHMENT_PIPELINE_BUSY_ERR: &str = "attachment_pipeline_busy";
const ATTACHMENT_PIPELINE_UNAVAILABLE_ERR: &str = "attachment_pipeline_unavailable";

struct AttachmentMessageRequest {
    message: IncomingMessage,
    cmd_tx: mpsc::Sender<XmppCommand>,
}

/// Session-level responder that encapsulates command/inference/attachment behavior.
pub struct SessionResponderActor {
    config: Config,
    llm: Arc<dyn LlmClient>,
    memory: Arc<Memory>,
    dead_letter: Arc<DeadLetterService>,
    file_downloader: Arc<dyn AttachmentDownloader>,
    skills: Arc<SkillRegistry>,
    inference_actor: InferenceActor,
    attachment_actor: Arc<AttachmentActor>,
    command_actor: CommandActor,
    start_time: std::time::Instant,
    attachment_enqueue_timeout: Duration,
    attachment_mailbox: OnceCell<mpsc::Sender<AttachmentMessageRequest>>,
}

impl SessionResponderActor {
    #[allow(dead_code)]
    pub fn new(
        config: Config,
        llm: Arc<dyn LlmClient>,
        memory: Arc<Memory>,
        memory_writer: Arc<dyn SessionMemoryWriter>,
        file_downloader: Arc<dyn AttachmentDownloader>,
        skills: Arc<SkillRegistry>,
        start_time: std::time::Instant,
    ) -> Self {
        let dead_letter_path = config.actors.dead_letter_path.clone();
        Self::with_dead_letter_service(
            config,
            llm,
            memory,
            memory_writer,
            file_downloader,
            skills,
            Arc::new(DeadLetterService::new(dead_letter_path)),
            start_time,
        )
    }

    pub fn with_dead_letter_service(
        config: Config,
        llm: Arc<dyn LlmClient>,
        memory: Arc<Memory>,
        memory_writer: Arc<dyn SessionMemoryWriter>,
        file_downloader: Arc<dyn AttachmentDownloader>,
        skills: Arc<SkillRegistry>,
        dead_letter: Arc<DeadLetterService>,
        start_time: std::time::Instant,
    ) -> Self {
        let inference_actor = InferenceActor::with_dead_letter_service(
            config.actors.tooling.clone(),
            Arc::clone(&dead_letter),
            Arc::clone(&llm),
            Arc::clone(&skills),
            Arc::clone(&memory_writer),
        );
        let attachment_actor = Arc::new(AttachmentActor::with_dead_letter_service(
            config.actors.tooling.clone(),
            Arc::clone(&dead_letter),
            Arc::clone(&llm),
            Arc::clone(&skills),
            Arc::clone(&memory_writer),
        ));
        let attachment_enqueue_timeout =
            Duration::from_millis(config.actors.tooling.skill_queue_timeout_ms.max(1));
        let command_actor = CommandActor::new();
        Self {
            config,
            llm,
            memory,
            dead_letter: Arc::clone(&dead_letter),
            file_downloader,
            skills,
            inference_actor,
            attachment_actor,
            command_actor,
            start_time,
            attachment_enqueue_timeout,
            attachment_mailbox: OnceCell::new(),
        }
    }

    async fn handle_command(&self, from: &str, body: &str) -> Result<CommandReply> {
        let llm_description = self.llm.description();
        let skill_names = self.skills.skill_names();
        self.command_actor
            .handle(
                from,
                body,
                &self.config,
                self.memory.as_ref(),
                self.dead_letter.as_ref(),
                self.skills.as_ref(),
                &llm_description,
                &skill_names,
                self.start_time.elapsed(),
            )
            .await
    }

    async fn handle_message(&self, from: &str, body: &str, msg_id: Option<&str>) -> Result<String> {
        self.inference_actor
            .handle_message(from, body, msg_id, &self.config, self.memory.as_ref())
            .await
    }

    async fn handle_reaction(&self, jid: &str) -> Result<String> {
        self.inference_actor
            .handle_reaction(jid, &self.config, self.memory.as_ref())
            .await
    }

    async fn handle_muc_message(&self, room_jid: &str, _body: &str) -> Result<String> {
        self.inference_actor
            .handle_muc_message(room_jid, &self.config, self.memory.as_ref())
            .await
    }

    async fn enqueue_attachment_message(
        &self,
        message: IncomingMessage,
        cmd_tx: mpsc::Sender<XmppCommand>,
    ) -> Result<()> {
        let request = AttachmentMessageRequest { message, cmd_tx };
        let tx = self.attachment_mailbox_tx().await;
        match timeout(self.attachment_enqueue_timeout, tx.send(request)).await {
            Ok(Ok(())) => Ok(()),
            Ok(Err(_)) => Err(anyhow!(ATTACHMENT_PIPELINE_UNAVAILABLE_ERR)),
            Err(_) => Err(anyhow!(ATTACHMENT_PIPELINE_BUSY_ERR)),
        }
    }

    async fn attachment_mailbox_tx(&self) -> mpsc::Sender<AttachmentMessageRequest> {
        self.attachment_mailbox
            .get_or_init(|| async { self.spawn_attachment_worker() })
            .await
            .clone()
    }

    fn spawn_attachment_worker(&self) -> mpsc::Sender<AttachmentMessageRequest> {
        let max_parallel = self.config.actors.tooling.max_parallel_skills.max(1);
        let (tx, mut rx) = mpsc::channel::<AttachmentMessageRequest>(max_parallel);
        let attachment_actor = Arc::clone(&self.attachment_actor);
        let downloader = Arc::clone(&self.file_downloader);
        let memory = Arc::clone(&self.memory);
        let config = Arc::new(self.config.clone());
        let permits = Arc::new(Semaphore::new(max_parallel));

        tokio::spawn(async move {
            while let Some(request) = rx.recv().await {
                let permit = match permits.clone().acquire_owned().await {
                    Ok(permit) => permit,
                    Err(_) => {
                        let correlation_id =
                            user_error::correlation_id_or_new(request.message.id.as_deref());
                        let user_message = user_error::to_user_safe_message(
                            &anyhow!(ATTACHMENT_PIPELINE_UNAVAILABLE_ERR),
                            &correlation_id,
                        );
                        let to = request.message.from;
                        let _ = request
                            .cmd_tx
                            .send(XmppCommand::SendChatState {
                                to: to.clone(),
                                state: ChatState::Paused,
                                msg_type: "chat".to_string(),
                            })
                            .await;
                        let _ = request
                            .cmd_tx
                            .send(XmppCommand::SendMessage {
                                to,
                                body: user_message,
                                id: None,
                            })
                            .await;
                        continue;
                    }
                };

                let attachment_actor = Arc::clone(&attachment_actor);
                let downloader = Arc::clone(&downloader);
                let memory = Arc::clone(&memory);
                let config = Arc::clone(&config);

                tokio::spawn(async move {
                    let _permit = permit;
                    process_attachment_message_request(
                        attachment_actor,
                        downloader,
                        memory,
                        config,
                        request,
                    )
                    .await;
                });
            }
        });

        tx
    }
}

#[async_trait]
impl MessageResponder for SessionResponderActor {
    async fn respond_to_command(&self, from: &str, body: &str) -> Result<CommandReply> {
        self.handle_command(from, body).await
    }

    async fn respond_to_chat_message(
        &self,
        from: &str,
        body: &str,
        msg_id: Option<&str>,
    ) -> Result<String> {
        self.handle_message(from, body, msg_id).await
    }

    async fn respond_to_muc_message(&self, room_jid: &str, body: &str) -> Result<String> {
        self.handle_muc_message(room_jid, body).await
    }

    async fn respond_to_attachment_message(
        &self,
        message: IncomingMessage,
        cmd_tx: mpsc::Sender<XmppCommand>,
    ) -> Result<()> {
        self.enqueue_attachment_message(message, cmd_tx).await
    }
}

#[async_trait]
impl ReactionResponder for SessionResponderActor {
    async fn respond_to_reaction(&self, jid: &str) -> Result<String> {
        self.handle_reaction(jid).await
    }
}

async fn process_attachment_message_request(
    attachment_actor: Arc<AttachmentActor>,
    downloader: Arc<dyn AttachmentDownloader>,
    memory: Arc<Memory>,
    config: Arc<Config>,
    request: AttachmentMessageRequest,
) {
    let msg = request.message;
    let cmd_tx = request.cmd_tx;
    let from = msg.from;
    let body = msg.body;
    let msg_id = msg.id;
    let oob_list = msg.oob;
    let correlation_id = user_error::correlation_id_or_new(msg_id.as_deref());

    let result = attachment_actor
        .handle_message_with_attachments(
            &from,
            &body,
            msg_id.as_deref(),
            &oob_list,
            downloader.as_ref(),
            &memory,
            config.as_ref(),
        )
        .await;

    match result {
        Ok(text) => {
            let out_id = uuid::Uuid::new_v4().to_string();
            let _ = cmd_tx
                .send(XmppCommand::SendMessage {
                    to: from,
                    body: text,
                    id: Some(out_id),
                })
                .await;
        }
        Err(e) => {
            error!(
                correlation_id = %correlation_id,
                "Error processing attachment message: {e}"
            );
            let _ = cmd_tx
                .send(XmppCommand::SendChatState {
                    to: from.clone(),
                    state: ChatState::Paused,
                    msg_type: "chat".to_string(),
                })
                .await;
            let _ = cmd_tx
                .send(XmppCommand::SendMessage {
                    to: from,
                    body: user_error::to_user_safe_message(&e, &correlation_id),
                    id: None,
                })
                .await;
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::sync::Arc;

    use tokio::sync::mpsc;

    use super::*;
    use crate::agent::files::FileDownloader;
    use crate::config::{
        ActorMemoryConfig, ActorObservabilityConfig, ActorSupervisionConfig, ActorToolingConfig,
        ActorsConfig, AgentConfig, ConnectionMode, KeepaliveConfig, LlmConfig, MemoryConfig,
        ServerConfig, SessionConfig, SkillsConfig,
    };
    use crate::llm::{AnthropicClient, LlmClient};
    use tempfile::TempDir;

    fn test_config(queue_timeout_ms: u64) -> Config {
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
                allowed_jids: vec!["alice@localhost".to_string()],
                allowed_domains: vec![],
            },
            memory: MemoryConfig {
                backend: "jsonl".to_string(),
                path: PathBuf::from("./data/memory"),
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
                tooling: ActorToolingConfig {
                    max_tool_rounds: 3,
                    skill_timeout_secs: 30,
                    max_parallel_skills: 1,
                    skill_queue_timeout_ms: queue_timeout_ms,
                    allowed_capabilities: vec!["*".to_string()],
                },
                supervision: ActorSupervisionConfig::default(),
                memory: ActorMemoryConfig::default(),
                observability: ActorObservabilityConfig::default(),
            },
        }
    }

    fn attachment_message(id: Option<&str>) -> IncomingMessage {
        IncomingMessage {
            from: "alice@localhost/mobile".to_string(),
            to: "bot@localhost".to_string(),
            body: "please inspect".to_string(),
            id: id.map(ToString::to_string),
            message_type: crate::xmpp::stanzas::MessageType::Chat,
            oob: vec![crate::xmpp::stanzas::OobData {
                url: "https://upload.localhost/file.pdf".to_string(),
                desc: None,
            }],
        }
    }

    fn build_responder(queue_timeout_ms: u64, tmp: &TempDir) -> SessionResponderActor {
        let config = test_config(queue_timeout_ms);
        let memory = Arc::new(Memory::open(tmp.path()).expect("memory open"));
        let memory_writer: Arc<dyn SessionMemoryWriter> = memory.clone();
        let llm: Arc<dyn LlmClient> = Arc::new(AnthropicClient::new(config.llm.clone()));
        let downloader: Arc<dyn AttachmentDownloader> = Arc::new(FileDownloader::new(1));
        let skills = Arc::new(SkillRegistry::new());
        let dead_letter = Arc::new(DeadLetterService::new(
            tmp.path().join("session-responder-dead-letters.jsonl"),
        ));
        SessionResponderActor::with_dead_letter_service(
            config,
            llm,
            memory,
            memory_writer,
            downloader,
            skills,
            dead_letter,
            std::time::Instant::now(),
        )
    }

    #[tokio::test]
    async fn test_attachment_pipeline_returns_unavailable_when_mailbox_closed() {
        let tmp = TempDir::new().expect("tmp");
        let responder = build_responder(10, &tmp);

        let (pipeline_tx, pipeline_rx) = mpsc::channel::<AttachmentMessageRequest>(1);
        drop(pipeline_rx);
        responder
            .attachment_mailbox
            .set(pipeline_tx)
            .expect("set attachment mailbox");

        let (cmd_tx, _cmd_rx) = mpsc::channel(1);
        let result = responder
            .respond_to_attachment_message(attachment_message(Some("att-unavailable")), cmd_tx)
            .await;

        let err = result.expect_err("expected unavailable error");
        assert!(err
            .to_string()
            .contains(ATTACHMENT_PIPELINE_UNAVAILABLE_ERR));
    }

    #[tokio::test]
    async fn test_attachment_pipeline_returns_busy_when_mailbox_full() {
        let tmp = TempDir::new().expect("tmp");
        let responder = build_responder(5, &tmp);

        let (pipeline_tx, _pipeline_rx) = mpsc::channel::<AttachmentMessageRequest>(1);
        let (prefill_cmd_tx, _prefill_cmd_rx) = mpsc::channel(1);
        pipeline_tx
            .send(AttachmentMessageRequest {
                message: attachment_message(Some("prefill")),
                cmd_tx: prefill_cmd_tx,
            })
            .await
            .expect("prefill");
        responder
            .attachment_mailbox
            .set(pipeline_tx)
            .expect("set attachment mailbox");

        let (cmd_tx, _cmd_rx) = mpsc::channel(1);
        let result = responder
            .respond_to_attachment_message(attachment_message(Some("att-busy")), cmd_tx)
            .await;

        let err = result.expect_err("expected busy error");
        assert!(err.to_string().contains(ATTACHMENT_PIPELINE_BUSY_ERR));
    }
}
