use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Result;
use tracing::{debug, info, warn};

use super::dead_letter::DeadLetterService;
use super::inference::build_system_prompt_static;
use super::memory_actor::SessionMemoryWriter;
use super::planner::PlannerActor;
use crate::agent::files::{file_to_content_block, AttachmentDownloader};
use crate::agent::memory::{Attachment, Memory};
use crate::config::{ActorToolingConfig, Config};
use crate::llm::{InputContentBlock, LlmClient, Message, MessageContent};
use crate::skills::{SkillContext, SkillRegistry};
use crate::xmpp::stanzas::{self, OobData};

/// Maximum number of history messages sent to the LLM
const MAX_HISTORY: usize = 20;

pub struct AttachmentActor {
    planner: PlannerActor,
    memory_writer: Arc<dyn SessionMemoryWriter>,
}

impl AttachmentActor {
    #[allow(dead_code)]
    pub fn new(
        tooling: ActorToolingConfig,
        dead_letter_path: PathBuf,
        llm: Arc<dyn LlmClient>,
        skills: Arc<SkillRegistry>,
        memory_writer: Arc<dyn SessionMemoryWriter>,
    ) -> Self {
        Self::with_dead_letter_service(
            tooling,
            Arc::new(DeadLetterService::new(dead_letter_path)),
            llm,
            skills,
            memory_writer,
        )
    }

    pub fn with_dead_letter_service(
        tooling: ActorToolingConfig,
        dead_letter: Arc<DeadLetterService>,
        llm: Arc<dyn LlmClient>,
        skills: Arc<SkillRegistry>,
        memory_writer: Arc<dyn SessionMemoryWriter>,
    ) -> Self {
        Self {
            planner: PlannerActor::with_dead_letter_service(tooling, dead_letter, llm, skills),
            memory_writer,
        }
    }

    /// Handles a 1:1 message with OOB file attachments.
    ///
    /// Downloads each file, converts supported types to Anthropic API content blocks,
    /// and sends a multi-modal message to the LLM.
    #[allow(clippy::too_many_arguments)]
    pub async fn handle_message_with_attachments(
        &self,
        from: &str,
        body: &str,
        msg_id: Option<&str>,
        oob_list: &[OobData],
        downloader: &dyn AttachmentDownloader,
        memory: &Memory,
        config: &Config,
    ) -> Result<String> {
        let bare_jid = stanzas::bare_jid(from);
        let files_dir = memory.files_dir(bare_jid)?;

        info!(
            "Processing {} attachment(s) from {bare_jid}",
            oob_list.len()
        );

        // Download files sequentially (semaphore inside FileDownloader handles concurrency)
        let mut content_blocks: Vec<InputContentBlock> = Vec::new();
        let mut attachment_meta: Vec<Attachment> = Vec::new();

        for (i, oob) in oob_list.iter().enumerate() {
            debug!(
                "Downloading attachment {}/{}: {}",
                i + 1,
                oob_list.len(),
                oob.url
            );
            match downloader.download(&oob.url, &files_dir).await {
                Ok(file) => {
                    info!(
                        "Downloaded {} ({}, {})",
                        file.filename,
                        file.mime_type,
                        file.human_size()
                    );
                    attachment_meta.push(Attachment {
                        filename: file.filename.clone(),
                        mime_type: file.mime_type.clone(),
                        size: file.human_size(),
                    });
                    match file_to_content_block(&file).await {
                        Ok(Some(block)) => content_blocks.push(block),
                        Ok(None) => {
                            // Unsupported type — add text note
                            content_blocks.push(InputContentBlock::Text {
                                text: format!(
                                    "[File received: {} ({}) — unsupported type, cannot analyze]",
                                    file.filename, file.mime_type
                                ),
                            });
                        }
                        Err(e) => {
                            warn!("Failed to encode file {}: {e}", file.filename);
                            content_blocks.push(InputContentBlock::Text {
                                text: format!(
                                    "[File received: {} — encoding error]",
                                    file.filename
                                ),
                            });
                        }
                    }
                }
                Err(e) => {
                    warn!("Failed to download {}: {e}", oob.url);
                    content_blocks.push(InputContentBlock::Text {
                        text: format!("[File download failed: {e}]"),
                    });
                    attachment_meta.push(Attachment {
                        filename: "unknown".to_string(),
                        mime_type: "unknown".to_string(),
                        size: "download failed".to_string(),
                    });
                }
            }
        }

        // Add text body if present
        if !body.is_empty() {
            content_blocks.push(InputContentBlock::Text {
                text: body.to_string(),
            });
        }

        // Auto-archive stale sessions before loading history
        memory.check_session_freshness(bare_jid, config.session.idle_timeout_mins)?;

        // Build the multi-modal message with structured JSON metadata block
        let history = memory.get_history(bare_jid, MAX_HISTORY)?;
        let workspace = memory.get_workspace_context(bare_jid)?;
        let system_prompt = build_system_prompt_static(&config.agent.name, &workspace);

        // Build multi-modal message — content blocks only, no runtime metadata
        let mut messages = history;
        messages.push(Message {
            role: "user".to_string(),
            content: MessageContent::Blocks(content_blocks),
        });

        debug!(
            "Calling LLM with {} messages (including attachment)",
            messages.len()
        );

        // Agentic loop (returns immediately if no tools registered)
        let context = SkillContext {
            jid: bare_jid.to_string(),
            base_path: memory.base_path().to_path_buf(),
        };
        let (text, input_tokens, output_tokens) = self
            .planner
            .plan_reply(&system_prompt, &mut messages, &context)
            .await?;

        // Store messages in history — attachments as structured metadata, not text labels
        let attachments = if attachment_meta.is_empty() {
            None
        } else {
            Some(attachment_meta)
        };
        self.memory_writer
            .store_message_full(
                bare_jid,
                "user",
                body,
                msg_id,
                Some(bare_jid),
                attachments,
                None,
            )
            .await?;

        let out_id = uuid::Uuid::new_v4().to_string();
        self.memory_writer
            .store_message_structured(bare_jid, "assistant", &text, Some(&out_id), None)
            .await?;

        info!(
            "Attachment response to {bare_jid}: {} chars ({} tokens used)",
            text.len(),
            input_tokens + output_tokens
        );

        Ok(text)
    }
}
