use std::sync::Arc;

use anyhow::Result;
use async_trait::async_trait;
use tokio::sync::mpsc;
use tracing::error;

use super::attachment::AttachmentActor;
use super::command::CommandActor;
use super::dead_letter::DeadLetterService;
use super::inference::InferenceActor;
use super::memory_actor::SessionMemoryWriter;
use super::message::{CommandReply, MessageResponder};
use super::reaction::ReactionResponder;
use crate::agent::files::AttachmentDownloader;
use crate::agent::memory::Memory;
use crate::config::Config;
use crate::llm::LlmClient;
use crate::skills::SkillRegistry;
use crate::xmpp::component::{ChatState, XmppCommand};
use crate::xmpp::stanzas::IncomingMessage;

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

    fn spawn_attachment_message_task(
        &self,
        msg: IncomingMessage,
        cmd_tx: mpsc::Sender<XmppCommand>,
    ) {
        let attachment_actor = Arc::clone(&self.attachment_actor);
        let downloader = Arc::clone(&self.file_downloader);
        let memory = Arc::clone(&self.memory);
        let config = self.config.clone();
        let from = msg.from;
        let body = msg.body;
        let msg_id = msg.id;
        let oob_list = msg.oob;

        tokio::spawn(async move {
            let result = attachment_actor
                .handle_message_with_attachments(
                    &from,
                    &body,
                    msg_id.as_deref(),
                    &oob_list,
                    downloader.as_ref(),
                    &memory,
                    &config,
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
                    error!("Error processing attachment message: {e}");
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
                            body: format!("Sorry, an error occurred: {e}"),
                            id: None,
                        })
                        .await;
                }
            }
        });
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
        self.spawn_attachment_message_task(message, cmd_tx);
        Ok(())
    }
}

#[async_trait]
impl ReactionResponder for SessionResponderActor {
    async fn respond_to_reaction(&self, jid: &str) -> Result<String> {
        self.handle_reaction(jid).await
    }
}
