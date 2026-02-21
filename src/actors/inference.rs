use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Result;
use chrono::Local;
use tracing::info;

use super::dead_letter::DeadLetterService;
use super::memory_actor::SessionMemoryWriter;
use super::planner::PlannerActor;
use crate::agent::memory::{build_message_for_llm, Memory, WorkspaceContext};
use crate::config::{ActorToolingConfig, Config};
use crate::llm::{LlmClient, Message};
use crate::skills::{SkillContext, SkillRegistry};
use crate::xmpp::stanzas;

/// Maximum number of history messages sent to the LLM
const MAX_HISTORY: usize = 20;

pub struct InferenceActor {
    planner: PlannerActor,
    memory_writer: Arc<dyn SessionMemoryWriter>,
}

impl InferenceActor {
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

    /// Processes an incoming message and produces a response via LLM.
    /// `msg_id` is the inbound XMPP stanza id (stored as structured metadata).
    pub async fn handle_message(
        &self,
        from: &str,
        body: &str,
        msg_id: Option<&str>,
        config: &Config,
        memory: &Memory,
    ) -> Result<String> {
        // Bare JID for memory (without resource)
        let bare_jid = stanzas::bare_jid(from);

        // Auto-archive stale sessions before loading history
        memory.check_session_freshness(bare_jid, config.session.idle_timeout_mins)?;

        // Retrieve conversation history and workspace context
        let history = memory.get_history(bare_jid, MAX_HISTORY)?;
        let workspace = memory.get_workspace_context(bare_jid)?;

        // Build system prompt from workspace files
        let system_prompt = build_system_prompt_static(&config.agent.name, &workspace);

        // Build message list for LLM — 1:1 chat, no sender prefix needed
        let mut messages = history;
        messages.push(build_message_for_llm(
            "user".to_string(),
            body.to_string(),
            None,
        ));

        // Agentic loop (returns immediately if no tools registered)
        let (text, input_tokens, output_tokens) = self
            .call_llm_with_tools(&system_prompt, &mut messages, bare_jid, memory)
            .await?;

        // Generate outbound message id
        let out_id = uuid::Uuid::new_v4().to_string();

        // Persist messages with structured metadata (clean content, metadata as fields)
        self.memory_writer
            .store_message_structured(bare_jid, "user", body, msg_id, Some(bare_jid))
            .await?;
        self.memory_writer
            .store_message_structured(bare_jid, "assistant", &text, Some(&out_id), None)
            .await?;

        info!(
            "Response to {bare_jid}: {} chars ({} tokens used)",
            text.len(),
            input_tokens + output_tokens
        );

        Ok(text)
    }

    /// Processes a reaction via LLM.
    /// The reaction is already stored in history by the caller.
    /// The LLM decides whether a response is warranted based on the full context.
    /// Returns the LLM response text (caller stores and sends it).
    pub async fn handle_reaction(
        &self,
        jid: &str,
        config: &Config,
        memory: &Memory,
    ) -> Result<String> {
        // Auto-archive stale sessions before loading history
        memory.check_session_freshness(jid, config.session.idle_timeout_mins)?;

        let history = memory.get_history(jid, MAX_HISTORY)?;
        let workspace = memory.get_workspace_context(jid)?;
        let system_prompt = build_system_prompt_static(&config.agent.name, &workspace);

        // The reaction is already the last entry in history (stored by caller)
        let mut messages = history;

        let (text, input_tokens, output_tokens) = self
            .call_llm_with_tools(&system_prompt, &mut messages, jid, memory)
            .await?;

        info!(
            "Reaction response to {jid}: {} chars ({} tokens used)",
            text.len(),
            input_tokens + output_tokens
        );

        Ok(text)
    }

    /// Processes a MUC message via LLM.
    /// The user message is already stored in history by the caller.
    /// Returns the LLM response text (caller stores the assistant message).
    pub async fn handle_muc_message(
        &self,
        room_jid: &str,
        config: &Config,
        memory: &Memory,
    ) -> Result<String> {
        // Auto-archive stale sessions before loading history
        memory.check_session_freshness(room_jid, config.session.idle_timeout_mins)?;

        // Retrieve conversation history and workspace context
        let history = memory.get_history(room_jid, MAX_HISTORY)?;
        let workspace = memory.get_workspace_context(room_jid)?;

        // Build system prompt
        let system_prompt = build_system_prompt_static(&config.agent.name, &workspace);

        // The user message is already the last entry in history (stored by caller)
        let mut messages = history;

        // Agentic loop (returns immediately if no tools registered)
        let (text, input_tokens, output_tokens) = self
            .call_llm_with_tools(&system_prompt, &mut messages, room_jid, memory)
            .await?;

        info!(
            "MUC response to {room_jid}: {} chars ({} tokens used)",
            text.len(),
            input_tokens + output_tokens
        );

        Ok(text)
    }

    /// Calls the LLM with optional tool support, running the agentic loop.
    ///
    /// Delegates to `PlannerActor`. When no skills are registered, this
    /// is equivalent to a single `llm.complete()` call.
    async fn call_llm_with_tools(
        &self,
        system_prompt: &str,
        messages: &mut Vec<Message>,
        jid: &str,
        memory: &Memory,
    ) -> Result<(String, u32, u32)> {
        let context = SkillContext {
            jid: jid.to_string(),
            base_path: memory.base_path().to_path_buf(),
        };
        self.planner
            .plan_reply(system_prompt, messages, &context)
            .await
    }
}

/// Builds the system prompt from workspace files.
///
/// Assembly order:
/// 1. identity.md (who the agent is)
/// 2. personality.md (how the agent behaves)
/// 3. instructions.md (rules and constraints)
/// 4. Hardcoded fallback if none of the 3 global files exist
/// 5. Per-JID user.md under "About this user"
/// 6. Per-JID memory.md under "Notes and memory"
pub(crate) fn build_system_prompt_static(agent_name: &str, ctx: &WorkspaceContext) -> String {
    let has_global_files =
        ctx.identity.is_some() || ctx.personality.is_some() || ctx.instructions.is_some();

    let mut prompt = String::new();

    if has_global_files {
        if let Some(ref identity) = ctx.identity {
            prompt.push_str(identity.trim());
            prompt.push_str("\n\n");
        }

        if let Some(ref personality) = ctx.personality {
            prompt.push_str(personality.trim());
            prompt.push_str("\n\n");
        }

        if let Some(ref instructions) = ctx.instructions {
            prompt.push_str(instructions.trim());
        }
    } else {
        prompt.push_str(&format!(
            "You are {agent_name}, a personal AI assistant accessible via XMPP.\n\
             You are direct, helpful, and concise. You respond in the user's language.\n\n\
             Rules:\n\
             - Respond concisely, no excessive markdown formatting\n\
             - If asked to execute an action (send an email, modify a file...), \
               describe what you would do but clarify that you cannot yet execute \
               actions (skills are coming in v0.2)\n\
             - You have memory of previous conversations with this user"
        ));
    }

    if let Some(ref profile) = ctx.user_profile {
        prompt.push_str(&format!("\n\n## About this user\n{}", profile.trim()));
    }

    if let Some(ref memory) = ctx.user_memory {
        prompt.push_str(&format!("\n\n## Notes and memory\n{}", memory.trim()));
    }

    // Inject current date so the LLM has temporal awareness
    // (e.g. for forming time-relevant web search queries).
    let now = Local::now();
    prompt.push_str(&format!(
        "\n\nCurrent date: {}",
        now.format("%A, %B %-d, %Y")
    ));

    prompt
}
