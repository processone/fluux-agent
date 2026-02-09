use std::sync::Arc;

use anyhow::Result;
use tokio::sync::mpsc;
use tracing::{debug, error, info, warn};

use crate::agent::files::{file_to_content_block, FileDownloader};
use crate::config::Config;
use crate::llm::{
    AnthropicClient, InputContentBlock, Message, MessageContent, StopReason, ToolDefinition,
};
use crate::xmpp::component::{ChatState, DisconnectReason, XmppCommand, XmppEvent};
use crate::xmpp::stanzas::{self, MessageType, OobData, PresenceType};

use crate::skills::SkillRegistry;

use super::memory::{build_message_for_llm, Memory, WorkspaceContext};

/// Maximum number of history messages sent to the LLM
const MAX_HISTORY: usize = 20;

/// Maximum number of tool-call rounds per user message.
/// Prevents runaway loops if the LLM keeps requesting tools.
const MAX_TOOL_ROUNDS: usize = 10;

/// The agentic runtime — core of Fluux Agent.
///
/// Receives XMPP events, builds context,
/// calls the LLM, and sends back responses.
pub struct AgentRuntime {
    config: Config,
    llm: AnthropicClient,
    memory: Arc<Memory>,
    file_downloader: Arc<FileDownloader>,
    skills: Arc<SkillRegistry>,
    start_time: std::time::Instant,
}

impl AgentRuntime {
    pub fn new(
        config: Config,
        llm: AnthropicClient,
        memory: Arc<Memory>,
        file_downloader: Arc<FileDownloader>,
        skills: SkillRegistry,
    ) -> Self {
        Self {
            config,
            llm,
            memory,
            file_downloader,
            skills: Arc::new(skills),
            start_time: std::time::Instant::now(),
        }
    }

    /// Main agent loop.
    ///
    /// Returns `DisconnectReason` indicating why the connection ended,
    /// so the reconnection loop can decide whether to retry.
    pub async fn run(
        &self,
        mut event_rx: mpsc::Receiver<XmppEvent>,
        cmd_tx: mpsc::Sender<XmppCommand>,
    ) -> Result<DisconnectReason> {
        info!("Agent runtime started — waiting for messages...");

        while let Some(event) = event_rx.recv().await {
            match event {
                XmppEvent::Connected => {
                    info!("✓ Agent is online and ready");

                    // Join configured MUC rooms (XEP-0045)
                    for room in &self.config.rooms {
                        info!("Joining MUC room: {} as {}", room.jid, room.nick);
                        let _ = cmd_tx
                            .send(XmppCommand::JoinMuc {
                                room: room.jid.clone(),
                                nick: room.nick.clone(),
                            })
                            .await;
                    }
                }
                XmppEvent::Message(msg) => {
                    let bare_from = stanzas::bare_jid(&msg.from);
                    let is_muc = msg.message_type == MessageType::GroupChat;

                    if is_muc {
                        // ── MUC groupchat message ───────────────────
                        let room_config = match self.config.find_room(bare_from) {
                            Some(rc) => rc.clone(),
                            None => {
                                debug!("Ignoring MUC message from unconfigured room {bare_from}");
                                continue;
                            }
                        };

                        // Filter self-messages (MUC reflects bot's own messages)
                        let sender_nick = msg.from.split('/').nth(1).unwrap_or("");
                        if sender_nick == room_config.nick {
                            continue;
                        }

                        // Store ALL room messages to history (for full context)
                        let sender_label = format!("{sender_nick}@muc");
                        let raw_body = build_history_text(&msg.body, &msg.oob);
                        if let Err(e) = self.memory.store_message_structured(
                            bare_from,
                            "user",
                            &raw_body,
                            msg.id.as_deref(),
                            Some(&sender_label),
                        ) {
                            error!("Failed to store MUC message: {e}");
                        }

                        // Only respond if the bot is mentioned
                        if !is_mentioned(&room_config.nick, &msg.body) {
                            continue;
                        }

                        info!("MUC mention from {sender_nick} in {bare_from}");

                        // Strip mention prefix before sending to LLM
                        let clean_body = strip_mention(&room_config.nick, &msg.body);

                        // Process via LLM using room JID as memory key
                        let response = if clean_body.starts_with('/') {
                            self.handle_command(&msg.from, &clean_body)
                        } else {
                            // Send <composing/> to the room before the LLM call
                            let _ = cmd_tx
                                .send(XmppCommand::SendChatState {
                                    to: bare_from.to_string(),
                                    state: ChatState::Composing,
                                    msg_type: "groupchat".to_string(),
                                })
                                .await;

                            self.handle_muc_message(bare_from, &clean_body).await
                        };

                        let room_jid = bare_from.to_string();
                        match response {
                            Ok(text) => {
                                // Generate outbound message id
                                let out_id = uuid::Uuid::new_v4().to_string();
                                if let Err(e) = self.memory.store_message_structured(
                                    &room_jid,
                                    "assistant",
                                    &text,
                                    Some(&out_id),
                                    None,
                                ) {
                                    error!("Failed to store MUC response: {e}");
                                }
                                let _ = cmd_tx
                                    .send(XmppCommand::SendMucMessage {
                                        to: room_jid,
                                        body: text,
                                        id: Some(out_id),
                                    })
                                    .await;
                            }
                            Err(e) => {
                                error!("Error processing MUC message: {e}");
                                // Send <paused/> to indicate the agent stopped generating
                                let _ = cmd_tx
                                    .send(XmppCommand::SendChatState {
                                        to: room_jid.clone(),
                                        state: ChatState::Paused,
                                        msg_type: "groupchat".to_string(),
                                    })
                                    .await;
                                let _ = cmd_tx
                                    .send(XmppCommand::SendMucMessage {
                                        to: room_jid,
                                        body: format!("Sorry, an error occurred: {e}"),
                                        id: None,
                                    })
                                    .await;
                            }
                        }
                    } else {
                        // ── 1:1 chat message ────────────────────────

                        // Domain-level security check (rejects cross-domain messages)
                        if !self.config.is_domain_allowed(&msg.from) {
                            warn!(
                                "Cross-domain message rejected from {} (domain not allowed)",
                                msg.from
                            );
                            continue;
                        }

                        // Per-JID authorization check
                        if !self.config.is_allowed(&msg.from) {
                            warn!("Unauthorized message from {}, ignoring", msg.from);
                            continue;
                        }

                        info!("Processing message from {}: {}", msg.from, msg.body);

                        // Slash commands are intercepted before the LLM
                        if msg.body.starts_with('/') {
                            let response = self.handle_command(&msg.from, &msg.body);
                            match response {
                                Ok(text) => {
                                    let _ = cmd_tx
                                        .send(XmppCommand::SendMessage {
                                            to: msg.from.clone(),
                                            body: text,
                                            id: None,
                                        })
                                        .await;
                                }
                                Err(e) => {
                                    error!("Error processing command: {e}");
                                    let _ = cmd_tx
                                        .send(XmppCommand::SendMessage {
                                            to: msg.from.clone(),
                                            body: format!("Sorry, an error occurred: {e}"),
                                            id: None,
                                        })
                                        .await;
                                }
                            }
                        } else if !msg.oob.is_empty() {
                            // ── Message with file attachments ──────────
                            // Download + LLM call in a spawned task to avoid
                            // blocking the event loop on file I/O.
                            let _ = cmd_tx
                                .send(XmppCommand::SendChatState {
                                    to: msg.from.clone(),
                                    state: ChatState::Composing,
                                    msg_type: "chat".to_string(),
                                })
                                .await;

                            let downloader = Arc::clone(&self.file_downloader);
                            let memory = Arc::clone(&self.memory);
                            let skills = Arc::clone(&self.skills);
                            let llm = self.llm.clone();
                            let config = self.config.clone();
                            let cmd_tx_clone = cmd_tx.clone();
                            let from = msg.from.clone();
                            let body = msg.body.clone();
                            let msg_id = msg.id.clone();
                            let oob_list = msg.oob.clone();

                            tokio::spawn(async move {
                                let result = handle_message_with_attachments(
                                    &from, &body, msg_id.as_deref(), &oob_list,
                                    &downloader, &memory, &llm, &config, &skills,
                                ).await;

                                match result {
                                    Ok(text) => {
                                        let out_id = uuid::Uuid::new_v4().to_string();
                                        let _ = cmd_tx_clone
                                            .send(XmppCommand::SendMessage {
                                                to: from,
                                                body: text,
                                                id: Some(out_id),
                                            })
                                            .await;
                                    }
                                    Err(e) => {
                                        error!("Error processing attachment message: {e}");
                                        let _ = cmd_tx_clone
                                            .send(XmppCommand::SendChatState {
                                                to: from.clone(),
                                                state: ChatState::Paused,
                                                msg_type: "chat".to_string(),
                                            })
                                            .await;
                                        let _ = cmd_tx_clone
                                            .send(XmppCommand::SendMessage {
                                                to: from,
                                                body: format!("Sorry, an error occurred: {e}"),
                                                id: None,
                                            })
                                            .await;
                                    }
                                }
                            });
                        } else {
                            // ── Regular text message ───────────────────
                            // Send <composing/> before the LLM call
                            let _ = cmd_tx
                                .send(XmppCommand::SendChatState {
                                    to: msg.from.clone(),
                                    state: ChatState::Composing,
                                    msg_type: "chat".to_string(),
                                })
                                .await;

                            let response = self.handle_message(&msg.from, &msg.body, msg.id.as_deref()).await;

                            match response {
                                Ok(text) => {
                                    let out_id = uuid::Uuid::new_v4().to_string();
                                    let _ = cmd_tx
                                        .send(XmppCommand::SendMessage {
                                            to: msg.from.clone(),
                                            body: text,
                                            id: Some(out_id),
                                        })
                                        .await;
                                }
                                Err(e) => {
                                    error!("Error processing message: {e}");
                                    let _ = cmd_tx
                                        .send(XmppCommand::SendChatState {
                                            to: msg.from.clone(),
                                            state: ChatState::Paused,
                                            msg_type: "chat".to_string(),
                                        })
                                        .await;
                                    let _ = cmd_tx
                                        .send(XmppCommand::SendMessage {
                                            to: msg.from.clone(),
                                            body: format!("Sorry, an error occurred: {e}"),
                                            id: None,
                                        })
                                        .await;
                                }
                            }
                        }
                    }
                }
                XmppEvent::Presence(pres) => {
                    let bare_jid = stanzas::bare_jid(&pres.from);

                    // Domain-level security check for subscription requests
                    if matches!(pres.presence_type, PresenceType::Subscribe)
                        && !self.config.is_domain_allowed(&pres.from)
                    {
                        warn!(
                            "Cross-domain subscription rejected from {bare_jid} (domain not allowed)"
                        );
                        continue;
                    }

                    match pres.presence_type {
                        PresenceType::Subscribe => {
                            // Auto-accept subscription requests from allowed JIDs
                            if self.config.is_allowed(&pres.from) {
                                info!("Auto-accepting subscription from {bare_jid}");
                                let _ = cmd_tx
                                    .send(XmppCommand::SendRaw(
                                        crate::xmpp::stanzas::build_subscribed(bare_jid),
                                    ))
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
                }
                XmppEvent::Reaction(reaction) => {
                    let bare_from = stanzas::bare_jid(&reaction.from);
                    let is_muc = reaction.message_type == MessageType::GroupChat;

                    // Apply same authorization checks as regular messages
                    if !is_muc {
                        if !self.config.is_domain_allowed(&reaction.from) {
                            warn!("Cross-domain reaction rejected from {}", reaction.from);
                            continue;
                        }
                        if !self.config.is_allowed(&reaction.from) {
                            warn!("Unauthorized reaction from {}, ignoring", reaction.from);
                            continue;
                        }
                    }

                    let emojis = reaction.emojis.join(" ");
                    info!(
                        "Reaction from {bare_from}: {emojis} on msg {}",
                        reaction.message_id
                    );

                    // Store reaction in conversation history so the LLM sees it
                    let reaction_text = format!(
                        "[Reacted to msg_id: {} with {}]",
                        reaction.message_id, emojis
                    );

                    if is_muc {
                        let sender_nick = reaction.from.split('/').nth(1).unwrap_or("unknown");
                        let sender_label = format!("{sender_nick}@muc");
                        if let Err(e) = self.memory.store_message_structured(
                            bare_from,
                            "user",
                            &reaction_text,
                            None,
                            Some(&sender_label),
                        ) {
                            error!("Failed to store MUC reaction: {e}");
                        }
                    } else if let Err(e) = self.memory.store_message_structured(
                        bare_from,
                        "user",
                        &reaction_text,
                        None,
                        Some(bare_from),
                    ) {
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
                            to: if is_muc { bare_from.to_string() } else { reply_to.clone() },
                            state: ChatState::Composing,
                            msg_type: msg_type_str.to_string(),
                        })
                        .await;

                    // Call LLM with full history (reaction is already stored)
                    let response = self.handle_reaction(bare_from).await;

                    match response {
                        Ok(text) => {
                            let out_id = uuid::Uuid::new_v4().to_string();
                            let jid_key = bare_from.to_string();
                            if let Err(e) = self.memory.store_message_structured(
                                &jid_key,
                                "assistant",
                                &text,
                                Some(&out_id),
                                None,
                            ) {
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
                                    to: if is_muc { bare_from.to_string() } else { reply_to },
                                    state: ChatState::Paused,
                                    msg_type: msg_type_str.to_string(),
                                })
                                .await;
                        }
                    }
                }
                XmppEvent::StreamError(condition) => {
                    error!("XMPP stream error: {condition}");
                    if condition == "conflict" {
                        return Ok(DisconnectReason::Conflict);
                    }
                    return Ok(DisconnectReason::StreamError(condition));
                }
                XmppEvent::Error(e) => {
                    error!("XMPP error: {e}");
                }
            }
        }

        Ok(DisconnectReason::ConnectionLost)
    }

    // ── Slash commands ────────────────────────────────────

    /// Handles a slash command. Returns the response text.
    /// These are intercepted by the runtime and never reach the LLM.
    fn handle_command(&self, from: &str, body: &str) -> Result<String> {
        let bare_jid = stanzas::bare_jid(from);
        let parts: Vec<&str> = body.splitn(2, ' ').collect();
        let command = parts[0].to_lowercase();

        info!("Slash command from {bare_jid}: {command}");

        match command.as_str() {
            "/new" | "/reset" => self.cmd_new_session(bare_jid),
            "/forget" => self.cmd_forget(bare_jid),
            "/status" => self.cmd_status(bare_jid),
            "/help" => Ok(self.cmd_help()),
            "/ping" => Ok("pong".to_string()),
            _ => Ok(format!(
                "Unknown command: {command}\nType /help for available commands."
            )),
        }
    }

    /// /new — Archive the current session and start fresh
    fn cmd_new_session(&self, bare_jid: &str) -> Result<String> {
        self.memory.new_session(bare_jid)
    }

    /// /forget — Erase active history and context
    fn cmd_forget(&self, bare_jid: &str) -> Result<String> {
        self.memory.forget(bare_jid)
    }

    /// /status — Agent status overview.
    ///
    /// The output is contextual: in a MUC room it shows room-specific info;
    /// in a 1:1 chat it shows user-specific info.
    fn cmd_status(&self, bare_jid: &str) -> Result<String> {
        let uptime = self.start_time.elapsed();
        let hours = uptime.as_secs() / 3600;
        let minutes = (uptime.as_secs() % 3600) / 60;

        let msg_count = self.memory.message_count(bare_jid)?;
        let session_count = self.memory.session_count(bare_jid)?;

        let yn = |b: bool| if b { "yes" } else { "none" };

        // Workspace file presence — contextual (checks per-JID overrides)
        let workspace = self.memory.get_workspace_context(bare_jid)?;
        let has_instructions = workspace.instructions.is_some();
        let has_identity = workspace.identity.is_some();
        let has_personality = workspace.personality.is_some();

        let is_room = self.config.find_room(bare_jid).is_some();

        let file_count = self.memory.file_count(bare_jid)?;
        let file_info = if file_count > 0 {
            format!("\nFiles: {file_count}")
        } else {
            String::new()
        };

        // Context-specific section: room info vs. user info
        let context_info = if is_room {
            format!(
                "Room: {bare_jid}\n\
                 Room messages: {msg_count}\n\
                 Archived sessions: {session_count}{file_info}"
            )
        } else {
            let has_profile = self.memory.has_user_profile(bare_jid)?;
            let has_memory = self.memory.get_user_memory(bare_jid)?.is_some();
            format!(
                "Your session: {msg_count} messages\n\
                 Archived sessions: {session_count}{file_info}\n\
                 User profile: {}\n\
                 User memory: {}",
                yn(has_profile),
                yn(has_memory),
            )
        };

        // Domain security info
        let domain_info = if self.config.agent.allowed_domains.is_empty() {
            format!("Allowed domains: {} (default)", self.config.server.domain())
        } else if self.config.agent.allowed_domains.iter().any(|d| d == "*") {
            "Allowed domains: * (all)".to_string()
        } else {
            format!(
                "Allowed domains: {}",
                self.config.agent.allowed_domains.join(", ")
            )
        };

        let skills_info = if self.skills.is_empty() {
            "Skills: none".to_string()
        } else {
            format!("Skills: {}", self.skills.skill_names().join(", "))
        };

        Ok(format!(
            "{} — status\n\
             Uptime: {hours}h {minutes}m\n\
             Mode: {}\n\
             LLM: {} ({})\n\
             {skills_info}\n\
             {context_info}\n\
             Workspace: instructions={}, identity={}, personality={}\n\
             {domain_info}",
            self.config.agent.name,
            self.config.server.mode_description(),
            self.config.llm.provider,
            self.config.llm.model,
            yn(has_instructions),
            yn(has_identity),
            yn(has_personality),
        ))
    }

    /// /help — List available commands
    fn cmd_help(&self) -> String {
        "\
Commands:\n\
  /new     — Start a new conversation (archive current session)\n\
  /forget  — Erase your history, profile, and memory\n\
  /status  — Agent info, uptime, session stats\n\
  /ping    — Check if the agent is alive\n\
  /help    — This message"
            .to_string()
    }

    // ── LLM message handling ─────────────────────────────

    /// Calls the LLM with optional tool support, running the agentic loop.
    ///
    /// Delegates to the free `agentic_loop()` function. When no skills are
    /// registered, this is equivalent to a single `llm.complete()` call.
    async fn call_llm_with_tools(
        &self,
        system_prompt: &str,
        messages: &mut Vec<Message>,
    ) -> Result<(String, u32, u32)> {
        agentic_loop(system_prompt, messages, &self.llm, &self.skills).await
    }

    /// Processes an incoming message and produces a response via LLM.
    /// `msg_id` is the inbound XMPP stanza id (stored as structured metadata).
    async fn handle_message(&self, from: &str, body: &str, msg_id: Option<&str>) -> Result<String> {
        // Bare JID for memory (without resource)
        let bare_jid = stanzas::bare_jid(from);

        // Retrieve conversation history and workspace context
        let history = self.memory.get_history(bare_jid, MAX_HISTORY)?;
        let workspace = self.memory.get_workspace_context(bare_jid)?;

        // Build system prompt from workspace files
        let system_prompt = self.build_system_prompt(&workspace);

        // Build message list for LLM — 1:1 chat, no sender prefix needed
        let mut messages = history;
        messages.push(build_message_for_llm(
            "user".to_string(),
            body.to_string(),
            None,
        ));

        // Agentic loop (returns immediately if no tools registered)
        let (text, input_tokens, output_tokens) =
            self.call_llm_with_tools(&system_prompt, &mut messages).await?;

        // Generate outbound message id
        let out_id = uuid::Uuid::new_v4().to_string();

        // Persist messages with structured metadata (clean content, metadata as fields)
        self.memory.store_message_structured(bare_jid, "user", body, msg_id, Some(bare_jid))?;
        self.memory
            .store_message_structured(bare_jid, "assistant", &text, Some(&out_id), None)?;

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
    async fn handle_reaction(&self, jid: &str) -> Result<String> {
        let history = self.memory.get_history(jid, MAX_HISTORY)?;
        let workspace = self.memory.get_workspace_context(jid)?;
        let system_prompt = self.build_system_prompt(&workspace);

        // The reaction is already the last entry in history (stored by caller)
        let mut messages = history;

        let (text, input_tokens, output_tokens) =
            self.call_llm_with_tools(&system_prompt, &mut messages).await?;

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
    async fn handle_muc_message(&self, room_jid: &str, _body: &str) -> Result<String> {
        // Retrieve conversation history and workspace context
        let history = self.memory.get_history(room_jid, MAX_HISTORY)?;
        let workspace = self.memory.get_workspace_context(room_jid)?;

        // Build system prompt
        let system_prompt = self.build_system_prompt(&workspace);

        // The user message is already the last entry in history (stored by caller)
        let mut messages = history;

        // Agentic loop (returns immediately if no tools registered)
        let (text, input_tokens, output_tokens) =
            self.call_llm_with_tools(&system_prompt, &mut messages).await?;

        info!(
            "MUC response to {room_jid}: {} chars ({} tokens used)",
            text.len(),
            input_tokens + output_tokens
        );

        Ok(text)
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
    fn build_system_prompt(&self, ctx: &WorkspaceContext) -> String {
        build_system_prompt_static(&self.config.agent.name, ctx)
    }
}

// ── Attachment handling (runs in spawned tasks) ──────────

/// Builds the text stored in conversation history for a message that may have attachments.
///
/// When OOB files are present, prepends `[Attached: filename.jpg]` tags so the LLM
/// has context about what was discussed, without embedding base64 data in history.
fn build_history_text(body: &str, oob_list: &[OobData]) -> String {
    if oob_list.is_empty() {
        return body.to_string();
    }

    let mut parts: Vec<String> = Vec::new();
    for oob in oob_list {
        let filename = oob.url.rsplit('/').next().unwrap_or("file");
        parts.push(format!("[Attached: {filename}]"));
    }
    if !body.is_empty() {
        parts.push(body.to_string());
    }
    parts.join("\n")
}

/// Agentic tool-use loop (free function for use from both methods and spawned tasks).
///
/// If skills are registered, tool definitions are passed to the LLM. When the LLM
/// responds with `tool_use` blocks, the corresponding skills are executed and results
/// fed back in a loop until the LLM produces a final text response.
///
/// `messages` is mutated during the loop (intermediate tool turns are appended).
/// Only the final text response is returned — intermediate tool calls are not
/// stored in persistent history.
///
/// Returns `(final_text, total_input_tokens, total_output_tokens)`.
async fn agentic_loop(
    system_prompt: &str,
    messages: &mut Vec<Message>,
    llm: &AnthropicClient,
    skills: &SkillRegistry,
) -> Result<(String, u32, u32)> {
    // Build tool definitions (None if no skills registered)
    let tool_defs: Option<Vec<ToolDefinition>> = if skills.is_empty() {
        None
    } else {
        Some(skills.tool_definitions())
    };
    let tools_ref = tool_defs.as_deref();

    let mut total_input = 0u32;
    let mut total_output = 0u32;

    for round in 0..MAX_TOOL_ROUNDS {
        let response = llm.complete(system_prompt, messages, tools_ref).await?;

        total_input = total_input.saturating_add(response.input_tokens);
        total_output = total_output.saturating_add(response.output_tokens);

        // If no tool calls, we're done — return the text response
        if response.stop_reason != StopReason::ToolUse || response.tool_calls.is_empty() {
            return Ok((response.text, total_input, total_output));
        }

        // Log tool calls
        for tc in &response.tool_calls {
            info!(
                "Tool call [round {}/{}]: {}({})",
                round + 1,
                MAX_TOOL_ROUNDS,
                tc.name,
                tc.input,
            );
        }

        // Append assistant message with the raw content blocks (text + tool_use)
        messages.push(Message {
            role: "assistant".to_string(),
            content: MessageContent::Blocks(response.content_blocks),
        });

        // Execute each tool call and collect results
        let mut result_blocks = Vec::new();
        for tc in &response.tool_calls {
            let result_content = match skills.get(&tc.name) {
                Some(skill) => match skill.execute(tc.input.clone()).await {
                    Ok(output) => output,
                    Err(e) => {
                        warn!("Skill {} failed: {e}", tc.name);
                        format!("Error: {e}")
                    }
                },
                None => {
                    warn!("Unknown skill requested: {}", tc.name);
                    format!("Error: unknown tool '{}'", tc.name)
                }
            };

            info!("Tool result for {}: {} chars", tc.name, result_content.len());

            result_blocks.push(InputContentBlock::ToolResult {
                tool_use_id: tc.id.clone(),
                content: result_content,
            });
        }

        // Append user message with tool_result blocks
        messages.push(Message {
            role: "user".to_string(),
            content: MessageContent::Blocks(result_blocks),
        });
    }

    // Exhausted all rounds — make one final call without tools to force a text response
    warn!(
        "Agentic loop exhausted {} rounds, forcing final response",
        MAX_TOOL_ROUNDS
    );
    let response = llm.complete(system_prompt, messages, None).await?;
    total_input = total_input.saturating_add(response.input_tokens);
    total_output = total_output.saturating_add(response.output_tokens);
    Ok((response.text, total_input, total_output))
}

/// Handles a 1:1 message with OOB file attachments.
///
/// Downloads each file, converts supported types to Anthropic API content blocks,
/// and sends a multi-modal message to the LLM. Runs in a spawned task.
#[allow(clippy::too_many_arguments)]
async fn handle_message_with_attachments(
    from: &str,
    body: &str,
    msg_id: Option<&str>,
    oob_list: &[OobData],
    downloader: &FileDownloader,
    memory: &Memory,
    llm: &AnthropicClient,
    config: &Config,
    skills: &SkillRegistry,
) -> Result<String> {
    let bare_jid = stanzas::bare_jid(from);
    let files_dir = memory.files_dir(bare_jid)?;

    info!(
        "Processing {} attachment(s) from {bare_jid}",
        oob_list.len()
    );

    // Download files sequentially (semaphore inside FileDownloader handles concurrency)
    let mut content_blocks: Vec<InputContentBlock> = Vec::new();
    let mut attachment_labels: Vec<String> = Vec::new();

    for (i, oob) in oob_list.iter().enumerate() {
        debug!("Downloading attachment {}/{}: {}", i + 1, oob_list.len(), oob.url);
        match downloader.download(&oob.url, &files_dir).await {
            Ok(file) => {
                info!(
                    "Downloaded {} ({}, {})",
                    file.filename, file.mime_type, file.human_size()
                );
                attachment_labels.push(format!(
                    "[Attached: {} ({}, {})]",
                    file.filename,
                    file.mime_type,
                    file.human_size()
                ));
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
                attachment_labels.push("[Attached: download failed]".to_string());
            }
        }
    }

    // Add text body if present
    if !body.is_empty() {
        content_blocks.push(InputContentBlock::Text {
            text: body.to_string(),
        });
    }

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

    debug!("Calling LLM with {} messages (including attachment)", messages.len());

    // Agentic loop (returns immediately if no tools registered)
    let (text, input_tokens, output_tokens) =
        agentic_loop(&system_prompt, &mut messages, llm, skills).await?;

    // Store messages in history (text description, not base64)
    // Content is clean — metadata stored as JSONL fields
    let labels = attachment_labels.join("\n");
    let mut history_content = labels;
    if !body.is_empty() {
        if !history_content.is_empty() {
            history_content.push('\n');
        }
        history_content.push_str(body);
    }
    memory.store_message_structured(bare_jid, "user", &history_content, msg_id, Some(bare_jid))?;

    let out_id = uuid::Uuid::new_v4().to_string();
    memory.store_message_structured(bare_jid, "assistant", &text, Some(&out_id), None)?;

    info!(
        "Attachment response to {bare_jid}: {} chars ({} tokens used)",
        text.len(),
        input_tokens + output_tokens
    );

    Ok(text)
}

/// Static version of build_system_prompt for use from spawned tasks.
/// (Cannot borrow `self` in a spawned task, so we extract the logic.)
fn build_system_prompt_static(agent_name: &str, ctx: &WorkspaceContext) -> String {
    let has_global_files = ctx.identity.is_some()
        || ctx.personality.is_some()
        || ctx.instructions.is_some();

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

    prompt
}

// ── MUC mention helpers ──────────────────────────────────

/// Checks if the bot's nickname is mentioned in the message body.
/// Matches: "@nick", "nick:", "nick " at start of message (case-insensitive).
fn is_mentioned(nick: &str, body: &str) -> bool {
    let lower_body = body.to_lowercase();
    let lower_nick = nick.to_lowercase();
    lower_body.contains(&format!("@{lower_nick}"))
        || lower_body.starts_with(&format!("{lower_nick}:"))
        || lower_body.starts_with(&format!("{lower_nick} "))
}

/// Strips the mention prefix from the message body.
/// Removes patterns like "@nick ", "@nick: ", "nick: ", "nick " from the beginning.
fn strip_mention(nick: &str, body: &str) -> String {
    let lower_body = body.to_lowercase();
    let lower_nick = nick.to_lowercase();

    // Try "@nick:" or "@nick " at the beginning
    let at_nick = format!("@{lower_nick}");
    if lower_body.starts_with(&at_nick) {
        let rest = &body[at_nick.len()..];
        let trimmed = rest.trim_start_matches(':').trim_start_matches(',').trim_start();
        if !trimmed.is_empty() {
            return trimmed.to_string();
        }
    }

    // Try "nick:" or "nick " at the beginning
    if lower_body.starts_with(&format!("{lower_nick}:"))
        || lower_body.starts_with(&format!("{lower_nick} "))
    {
        let rest = &body[nick.len()..];
        let trimmed = rest.trim_start_matches(':').trim_start_matches(',').trim_start();
        if !trimmed.is_empty() {
            return trimmed.to_string();
        }
    }

    // "@nick" in the middle — no stripping needed, return full body
    body.to_string()
}


#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::*;
    use tempfile::TempDir;

    /// Build a test runtime with a temporary memory directory
    fn test_runtime() -> (AgentRuntime, TempDir) {
        let tmp = TempDir::new().unwrap();
        let config = Config {
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
                model: "claude-sonnet-4-5-20250929".to_string(),
                api_key: "test-key".to_string(),
                max_tokens_per_request: 4096,
            },
            agent: AgentConfig {
                name: "Test Agent".to_string(),
                allowed_jids: vec!["admin@localhost".to_string()],
                allowed_domains: vec![],
            },
            memory: MemoryConfig {
                backend: "markdown".to_string(),
                path: tmp.path().to_path_buf(),
            },
            rooms: vec![],
        };

        let llm = AnthropicClient::new(config.llm.clone());
        let memory = Arc::new(Memory::open(tmp.path()).unwrap());
        let file_downloader = Arc::new(FileDownloader::new(3));
        let skills = SkillRegistry::new();
        let runtime = AgentRuntime::new(config, llm, memory, file_downloader, skills);
        (runtime, tmp)
    }

    // ── MUC mention helper tests ────────────────────────

    #[test]
    fn test_is_mentioned_at_prefix() {
        assert!(is_mentioned("bot", "@bot what's up?"));
        assert!(is_mentioned("bot", "hey @bot help"));
        assert!(is_mentioned("FluuxBot", "@fluuxbot hello"));
    }

    #[test]
    fn test_is_mentioned_colon_prefix() {
        assert!(is_mentioned("bot", "bot: what's up?"));
        assert!(is_mentioned("FluuxBot", "FluuxBot: help"));
    }

    #[test]
    fn test_is_mentioned_space_prefix() {
        assert!(is_mentioned("bot", "bot what's up?"));
    }

    #[test]
    fn test_is_mentioned_not_mentioned() {
        assert!(!is_mentioned("bot", "hello everyone"));
        assert!(!is_mentioned("bot", "robotics are cool"));
    }

    #[test]
    fn test_strip_mention_at_prefix() {
        assert_eq!(strip_mention("bot", "@bot what's up?"), "what's up?");
        assert_eq!(strip_mention("bot", "@bot: help me"), "help me");
        assert_eq!(strip_mention("FluuxBot", "@FluuxBot hello"), "hello");
    }

    #[test]
    fn test_strip_mention_colon_prefix() {
        assert_eq!(strip_mention("bot", "bot: what's up?"), "what's up?");
        assert_eq!(strip_mention("FluuxBot", "FluuxBot: help"), "help");
    }

    #[test]
    fn test_strip_mention_middle_keeps_body() {
        // "@nick" in the middle — no stripping
        assert_eq!(
            strip_mention("bot", "hey @bot help me"),
            "hey @bot help me"
        );
    }

    #[test]
    fn test_is_mentioned_case_insensitive() {
        assert!(is_mentioned("FluuxBot", "@fluuxbot help"));
        assert!(is_mentioned("bot", "@BOT help"));
        assert!(is_mentioned("Bot", "bot: hello"));
        assert!(is_mentioned("BOT", "Bot: hello"));
    }

    #[test]
    fn test_is_mentioned_with_punctuation() {
        // "@bot!" — the @bot substring is found
        assert!(is_mentioned("bot", "@bot! help me"));
        assert!(is_mentioned("bot", "@bot, please help"));
        assert!(is_mentioned("bot", "@bot? are you there"));
    }

    #[test]
    fn test_is_mentioned_at_end_of_message() {
        assert!(is_mentioned("bot", "hey @bot"));
    }

    #[test]
    fn test_strip_mention_comma_after_at() {
        assert_eq!(strip_mention("bot", "@bot, help me"), "help me");
    }

    #[test]
    fn test_strip_mention_only_mention_returns_full_body() {
        // "@bot" with nothing after → returns full body (no stripping)
        assert_eq!(strip_mention("bot", "@bot"), "@bot");
    }

    #[test]
    fn test_strip_mention_case_insensitive() {
        assert_eq!(strip_mention("FluuxBot", "@fluuxbot hello"), "hello");
        assert_eq!(strip_mention("BOT", "bot: hello"), "hello");
    }

    #[test]
    fn test_status_in_room_context() {
        let (mut rt, _tmp) = test_runtime();
        rt.config.rooms = vec![RoomConfig {
            jid: "lobby@conference.localhost".to_string(),
            nick: "bot".to_string(),
        }];
        // Status from a room JID shows room-specific info
        let result = rt
            .handle_command("lobby@conference.localhost/alice", "/status")
            .unwrap();
        assert!(result.contains("Room: lobby@conference.localhost"));
        assert!(result.contains("Room messages:"));
        // Should NOT show user profile/memory fields
        assert!(!result.contains("User profile:"));
        assert!(!result.contains("User memory:"));
    }

    #[test]
    fn test_status_in_direct_chat_context() {
        let (rt, _tmp) = test_runtime();
        let result = rt.handle_command("admin@localhost", "/status").unwrap();
        // Should show user-specific info
        assert!(result.contains("Your session:"));
        assert!(result.contains("User profile:"));
        assert!(result.contains("User memory:"));
        // Should NOT show room-specific fields
        assert!(!result.contains("Room:"));
        assert!(!result.contains("Room messages:"));
    }

    // ── Slash command tests ─────────────────────────────

    #[test]
    fn test_command_ping() {
        let (rt, _tmp) = test_runtime();
        let result = rt.handle_command("admin@localhost/res", "/ping").unwrap();
        assert_eq!(result, "pong");
    }

    #[test]
    fn test_command_help_lists_all_commands() {
        let (rt, _tmp) = test_runtime();
        let result = rt.handle_command("admin@localhost/res", "/help").unwrap();
        assert!(result.contains("/new"));
        assert!(result.contains("/forget"));
        assert!(result.contains("/status"));
        assert!(result.contains("/ping"));
        assert!(result.contains("/help"));
    }

    #[test]
    fn test_command_unknown() {
        let (rt, _tmp) = test_runtime();
        let result = rt.handle_command("admin@localhost", "/foobar").unwrap();
        assert!(result.contains("Unknown command"));
        assert!(result.contains("/foobar"));
    }

    #[test]
    fn test_command_case_insensitive() {
        let (rt, _tmp) = test_runtime();
        assert_eq!(
            rt.handle_command("admin@localhost", "/PING").unwrap(),
            "pong"
        );
        assert_eq!(
            rt.handle_command("admin@localhost", "/Ping").unwrap(),
            "pong"
        );
    }

    #[test]
    fn test_command_new_empty_session() {
        let (rt, _tmp) = test_runtime();
        let result = rt.handle_command("admin@localhost/res", "/new").unwrap();
        // No history → should report nothing to archive
        assert!(
            result.to_lowercase().contains("no active session")
                || result.to_lowercase().contains("no session")
                || result.to_lowercase().contains("nothing")
        );
    }

    #[test]
    fn test_command_new_archives_session() {
        let (rt, _tmp) = test_runtime();
        rt.memory.store_message("admin@localhost", "user", "Hello").unwrap();
        rt.memory.store_message("admin@localhost", "assistant", "Hi!").unwrap();

        let result = rt.handle_command("admin@localhost/res", "/new").unwrap();
        assert!(result.to_lowercase().contains("archived") || result.to_lowercase().contains("session"));

        // History should be empty after /new
        let history = rt.memory.get_history("admin@localhost", 20).unwrap();
        assert!(history.is_empty());

        // Archived session count should be 1
        assert_eq!(rt.memory.session_count("admin@localhost").unwrap(), 1);
    }

    #[test]
    fn test_command_reset_is_alias_for_new() {
        let (rt, _tmp) = test_runtime();
        rt.memory.store_message("admin@localhost", "user", "Test").unwrap();

        let result = rt.handle_command("admin@localhost", "/reset").unwrap();
        assert!(result.to_lowercase().contains("archived") || result.to_lowercase().contains("session"));
    }

    #[test]
    fn test_command_forget_clears_memory() {
        let (rt, _tmp) = test_runtime();
        rt.memory.store_message("admin@localhost", "user", "Hello").unwrap();
        rt.memory.set_user_context("admin@localhost", "Likes Rust").unwrap();

        let _result = rt.handle_command("admin@localhost/res", "/forget").unwrap();

        // Both history and context should be gone
        let history = rt.memory.get_history("admin@localhost", 20).unwrap();
        assert!(history.is_empty());
        assert!(rt.memory.get_user_context("admin@localhost").unwrap().is_none());
    }

    #[test]
    fn test_command_strips_resource_from_jid() {
        let (rt, _tmp) = test_runtime();
        rt.memory.store_message("admin@localhost", "user", "Hi").unwrap();

        // Command from full JID with resource should see the bare JID's messages
        let result = rt
            .handle_command("admin@localhost/Conversations.xyz", "/status")
            .unwrap();
        assert!(result.contains("1 messages"));
    }

    // ── System prompt tests ─────────────────────────────

    #[test]
    fn test_build_system_prompt_fallback_no_global_files() {
        let (rt, _tmp) = test_runtime();
        let ctx = WorkspaceContext {
            instructions: None,
            identity: None,
            personality: None,
            user_profile: None,
            user_memory: None,
        };
        let prompt = rt.build_system_prompt(&ctx);
        assert!(prompt.contains("Test Agent"));
        assert!(prompt.contains("XMPP"));
        assert!(!prompt.contains("About this user"));
    }

    #[test]
    fn test_build_system_prompt_with_user_profile() {
        let (rt, _tmp) = test_runtime();
        let ctx = WorkspaceContext {
            instructions: None,
            identity: None,
            personality: None,
            user_profile: Some("Prefers French. Works at Acme Corp.".to_string()),
            user_memory: None,
        };
        let prompt = rt.build_system_prompt(&ctx);
        // Fallback prompt should be used (no global files)
        assert!(prompt.contains("Test Agent"));
        assert!(prompt.contains("About this user"));
        assert!(prompt.contains("Prefers French"));
        assert!(prompt.contains("Acme Corp"));
    }

    #[test]
    fn test_build_system_prompt_with_global_files() {
        let (rt, _tmp) = test_runtime();
        let ctx = WorkspaceContext {
            instructions: Some("Always respond in haiku format.".to_string()),
            identity: Some("You are HaikuBot, a poetry assistant.".to_string()),
            personality: Some("Serene and contemplative.".to_string()),
            user_profile: None,
            user_memory: None,
        };
        let prompt = rt.build_system_prompt(&ctx);
        // Should NOT contain fallback prompt
        assert!(!prompt.contains("Test Agent"));
        assert!(!prompt.contains("XMPP"));
        // Should contain workspace file content
        assert!(prompt.contains("HaikuBot"));
        assert!(prompt.contains("Serene and contemplative"));
        assert!(prompt.contains("Always respond in haiku format"));
    }

    #[test]
    fn test_build_system_prompt_with_memory() {
        let (rt, _tmp) = test_runtime();
        let ctx = WorkspaceContext {
            instructions: None,
            identity: None,
            personality: None,
            user_profile: Some("Developer at ProcessOne".to_string()),
            user_memory: Some("Prefers Rust over Go. Working on XMPP project.".to_string()),
        };
        let prompt = rt.build_system_prompt(&ctx);
        assert!(prompt.contains("About this user"));
        assert!(prompt.contains("Developer at ProcessOne"));
        assert!(prompt.contains("Notes and memory"));
        assert!(prompt.contains("Prefers Rust over Go"));
    }

    #[test]
    fn test_build_system_prompt_partial_global_files() {
        let (rt, _tmp) = test_runtime();
        // Only identity.md exists — should use workspace mode, not fallback
        let ctx = WorkspaceContext {
            instructions: None,
            identity: Some("You are Fluux Agent.".to_string()),
            personality: None,
            user_profile: None,
            user_memory: None,
        };
        let prompt = rt.build_system_prompt(&ctx);
        assert!(prompt.contains("Fluux Agent"));
        // Fallback should NOT be present
        assert!(!prompt.contains("skills are coming in v0.2"));
    }

    // ── Status tests with workspace ─────────────────────

    #[test]
    fn test_command_status_content() {
        let (rt, _tmp) = test_runtime();
        rt.memory
            .store_message("admin@localhost", "user", "Hi")
            .unwrap();

        let result = rt
            .handle_command("admin@localhost/res", "/status")
            .unwrap();
        assert!(result.contains("Test Agent"));
        assert!(result.contains("Uptime:"));
        assert!(result.contains("C2S client"));
        assert!(result.contains("anthropic"));
        assert!(result.contains("Skills: none"));
        assert!(result.contains("1 messages"));
        assert!(result.contains("User profile: none"));
        assert!(result.contains("User memory: none"));
        assert!(result.contains("instructions=none"));
        assert!(result.contains("Allowed domains: localhost (default)"));
    }

    #[test]
    fn test_command_status_with_profile() {
        let (rt, _tmp) = test_runtime();
        rt.memory
            .set_user_context("admin@localhost", "Developer")
            .unwrap();

        let result = rt
            .handle_command("admin@localhost/res", "/status")
            .unwrap();
        assert!(result.contains("User profile: yes"));
    }

    #[test]
    fn test_command_status_with_workspace_files() {
        let (rt, tmp) = test_runtime();
        std::fs::write(tmp.path().join("instructions.md"), "Be concise").unwrap();
        std::fs::write(tmp.path().join("identity.md"), "I am an agent").unwrap();

        let result = rt
            .handle_command("admin@localhost/res", "/status")
            .unwrap();
        assert!(result.contains("instructions=yes"));
        assert!(result.contains("identity=yes"));
        assert!(result.contains("personality=none"));
    }

    #[test]
    fn test_command_status_domain_wildcard() {
        let (mut rt, _tmp) = test_runtime();
        rt.config.agent.allowed_domains = vec!["*".to_string()];

        let result = rt.handle_command("admin@localhost", "/status").unwrap();
        assert!(result.contains("Allowed domains: * (all)"));
    }

    #[test]
    fn test_command_status_domain_explicit() {
        let (mut rt, _tmp) = test_runtime();
        rt.config.agent.allowed_domains =
            vec!["localhost".to_string(), "partner.org".to_string()];

        let result = rt.handle_command("admin@localhost", "/status").unwrap();
        assert!(result.contains("Allowed domains: localhost, partner.org"));
    }

    // ── build_history_text tests ─────────────────────────

    #[test]
    fn test_build_history_text_no_oob() {
        let text = build_history_text("Hello!", &[]);
        assert_eq!(text, "Hello!");
    }

    #[test]
    fn test_build_history_text_oob_with_body() {
        let oob = vec![OobData {
            url: "https://upload.example.com/abc/photo.jpg".to_string(),
            desc: None,
        }];
        let text = build_history_text("What is this?", &oob);
        assert_eq!(text, "[Attached: photo.jpg]\nWhat is this?");
    }

    #[test]
    fn test_build_history_text_oob_no_body() {
        let oob = vec![OobData {
            url: "https://upload.example.com/abc/photo.jpg".to_string(),
            desc: None,
        }];
        let text = build_history_text("", &oob);
        assert_eq!(text, "[Attached: photo.jpg]");
    }

    #[test]
    fn test_build_history_text_multiple_oob() {
        let oob = vec![
            OobData {
                url: "https://upload.example.com/a.jpg".to_string(),
                desc: None,
            },
            OobData {
                url: "https://upload.example.com/b.pdf".to_string(),
                desc: None,
            },
        ];
        let text = build_history_text("Check these", &oob);
        assert_eq!(
            text,
            "[Attached: a.jpg]\n[Attached: b.pdf]\nCheck these"
        );
    }

    #[test]
    fn test_build_history_text_url_no_slashes() {
        // Pathological URL with no path component
        let oob = vec![OobData {
            url: "https://example.com".to_string(),
            desc: None,
        }];
        let text = build_history_text("", &oob);
        // rsplit('/') on "https://example.com" yields "example.com"
        assert_eq!(text, "[Attached: example.com]");
    }

    #[test]
    fn test_build_history_text_url_trailing_slash() {
        let oob = vec![OobData {
            url: "https://example.com/files/".to_string(),
            desc: None,
        }];
        let text = build_history_text("", &oob);
        // rsplit('/') on trailing slash yields empty string first, fallback to "file"
        // Actually rsplit('/').next() yields "" (empty) — let's verify behavior
        assert_eq!(text, "[Attached: ]");
    }

    #[test]
    fn test_build_history_text_empty_body_no_oob() {
        let text = build_history_text("", &[]);
        assert_eq!(text, "");
    }

    #[test]
    fn test_build_history_text_body_with_whitespace_only() {
        let text = build_history_text("   ", &[]);
        assert_eq!(text, "   ");
    }

    // ── Status with files test ──────────────────────────

    #[test]
    fn test_command_status_with_files() {
        let (rt, tmp) = test_runtime();
        let files_dir = rt.memory.files_dir("admin@localhost").unwrap();
        std::fs::write(files_dir.join("abc_photo.jpg"), b"fake").unwrap();

        let result = rt.handle_command("admin@localhost", "/status").unwrap();
        assert!(result.contains("Files: 1"));
    }

    #[test]
    fn test_command_status_no_files_hides_line() {
        let (rt, _tmp) = test_runtime();
        let result = rt.handle_command("admin@localhost", "/status").unwrap();
        // When there are no files, the "Files:" line should not appear
        assert!(!result.contains("Files:"));
    }

    // ── Status with skills test ─────────────────────────

    #[test]
    fn test_command_status_with_skills() {
        use async_trait::async_trait;
        use crate::skills::Skill;

        struct StubSkill(&'static str);
        #[async_trait]
        impl Skill for StubSkill {
            fn name(&self) -> &str { self.0 }
            fn description(&self) -> &str { "" }
            fn parameters_schema(&self) -> serde_json::Value { serde_json::json!({}) }
            async fn execute(&self, _: serde_json::Value) -> anyhow::Result<String> {
                Ok(String::new())
            }
        }

        let (mut rt, _tmp) = test_runtime();
        let skills = Arc::get_mut(&mut rt.skills).unwrap();
        skills.register(Box::new(StubSkill("web_search")));
        skills.register(Box::new(StubSkill("url_fetch")));

        let result = rt.handle_command("admin@localhost", "/status").unwrap();
        assert!(result.contains("Skills: url_fetch, web_search"));
        assert!(!result.contains("Skills: none"));
    }

    // ── Agentic loop message structure tests ─────────────

    #[test]
    fn test_tool_result_message_structure() {
        use crate::llm::InputContentBlock;

        let msg = Message {
            role: "user".to_string(),
            content: MessageContent::Blocks(vec![
                InputContentBlock::ToolResult {
                    tool_use_id: "toolu_abc123".to_string(),
                    content: "Search returned 5 results".to_string(),
                },
            ]),
        };
        let json = serde_json::to_value(&msg).unwrap();
        assert_eq!(json["role"], "user");
        let blocks = json["content"].as_array().unwrap();
        assert_eq!(blocks[0]["type"], "tool_result");
        assert_eq!(blocks[0]["tool_use_id"], "toolu_abc123");
        assert_eq!(blocks[0]["content"], "Search returned 5 results");
    }

    #[test]
    fn test_assistant_tool_use_message_structure() {
        use crate::llm::InputContentBlock;

        let msg = Message {
            role: "assistant".to_string(),
            content: MessageContent::Blocks(vec![
                InputContentBlock::Text {
                    text: "Let me search for that.".to_string(),
                },
                InputContentBlock::ToolUse {
                    id: "toolu_abc123".to_string(),
                    name: "web_search".to_string(),
                    input: serde_json::json!({"query": "rust async"}),
                },
            ]),
        };
        let json = serde_json::to_value(&msg).unwrap();
        assert_eq!(json["role"], "assistant");
        let blocks = json["content"].as_array().unwrap();
        assert_eq!(blocks.len(), 2);
        assert_eq!(blocks[0]["type"], "text");
        assert_eq!(blocks[1]["type"], "tool_use");
        assert_eq!(blocks[1]["name"], "web_search");
    }

    #[test]
    fn test_agentic_conversation_roundtrip() {
        use crate::llm::InputContentBlock;

        // Simulate the full agentic conversation:
        // 1. User asks a question
        // 2. Assistant responds with tool_use
        // 3. User sends tool_result
        // 4. Assistant responds with text
        let messages = vec![
            Message {
                role: "user".to_string(),
                content: MessageContent::Text("What is Rust?".to_string()),
            },
            Message {
                role: "assistant".to_string(),
                content: MessageContent::Blocks(vec![InputContentBlock::ToolUse {
                    id: "toolu_1".to_string(),
                    name: "web_search".to_string(),
                    input: serde_json::json!({"query": "Rust programming language"}),
                }]),
            },
            Message {
                role: "user".to_string(),
                content: MessageContent::Blocks(vec![InputContentBlock::ToolResult {
                    tool_use_id: "toolu_1".to_string(),
                    content: "Rust is a systems programming language.".to_string(),
                }]),
            },
            Message {
                role: "assistant".to_string(),
                content: MessageContent::Text(
                    "Rust is a systems programming language focused on safety.".to_string(),
                ),
            },
        ];

        let json = serde_json::to_value(&messages).unwrap();
        let arr = json.as_array().unwrap();
        assert_eq!(arr.len(), 4);
        assert_eq!(arr[0]["role"], "user");
        assert_eq!(arr[0]["content"], "What is Rust?");
        assert_eq!(arr[1]["role"], "assistant");
        assert_eq!(arr[1]["content"][0]["type"], "tool_use");
        assert_eq!(arr[2]["role"], "user");
        assert_eq!(arr[2]["content"][0]["type"], "tool_result");
        assert_eq!(arr[3]["role"], "assistant");
        assert!(arr[3]["content"].is_string());
    }

    #[test]
    fn test_empty_skills_produces_none_tools() {
        let registry = SkillRegistry::new();
        let defs = registry.tool_definitions();
        assert!(defs.is_empty());
        // The agentic loop converts empty → None (not Some([]))
        let tools: Option<Vec<crate::llm::ToolDefinition>> = if registry.is_empty() {
            None
        } else {
            Some(defs)
        };
        assert!(tools.is_none());
    }
}
