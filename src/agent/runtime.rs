use anyhow::Result;
use tokio::sync::mpsc;
use tracing::{debug, error, info, warn};

use crate::config::Config;
use crate::llm::{AnthropicClient, Message};
use crate::xmpp::component::{ChatState, XmppCommand, XmppEvent};
use crate::xmpp::stanzas::{MessageType, PresenceType};

use super::memory::{Memory, WorkspaceContext};

/// Maximum number of history messages sent to the LLM
const MAX_HISTORY: usize = 20;

/// The agentic runtime — core of Fluux Agent.
///
/// Receives XMPP events, builds context,
/// calls the LLM, and sends back responses.
pub struct AgentRuntime {
    config: Config,
    llm: AnthropicClient,
    memory: Memory,
    start_time: std::time::Instant,
}

impl AgentRuntime {
    pub fn new(config: Config, llm: AnthropicClient, memory: Memory) -> Self {
        Self {
            config,
            llm,
            memory,
            start_time: std::time::Instant::now(),
        }
    }

    /// Main agent loop
    pub async fn run(
        &self,
        mut event_rx: mpsc::Receiver<XmppEvent>,
        cmd_tx: mpsc::Sender<XmppCommand>,
    ) -> Result<()> {
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
                    let bare_from = msg.from.split('/').next().unwrap_or(&msg.from);
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
                        // Use sender's nick in the header: "### user (alice)"
                        let sender_label = format!("{sender_nick}@muc");
                        if let Err(e) = self.memory.store_message_with_jid(
                            bare_from,
                            "user",
                            &msg.body,
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
                                // Store assistant response to room history
                                if let Err(e) =
                                    self.memory.store_message(&room_jid, "assistant", &text)
                                {
                                    error!("Failed to store MUC response: {e}");
                                }
                                let _ = cmd_tx
                                    .send(XmppCommand::SendMucMessage {
                                        to: room_jid,
                                        body: text,
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
                        let response = if msg.body.starts_with('/') {
                            self.handle_command(&msg.from, &msg.body)
                        } else {
                            // Send <composing/> before the LLM call
                            let _ = cmd_tx
                                .send(XmppCommand::SendChatState {
                                    to: msg.from.clone(),
                                    state: ChatState::Composing,
                                    msg_type: "chat".to_string(),
                                })
                                .await;

                            self.handle_message(&msg.from, &msg.body).await
                        };

                        match response {
                            Ok(text) => {
                                let _ = cmd_tx
                                    .send(XmppCommand::SendMessage {
                                        to: msg.from.clone(),
                                        body: text,
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
                                    })
                                    .await;
                            }
                        }
                    }
                }
                XmppEvent::Presence(pres) => {
                    let bare_jid = pres.from.split('/').next().unwrap_or(&pres.from);

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
                XmppEvent::Error(e) => {
                    error!("XMPP error: {e}");
                }
            }
        }

        Ok(())
    }

    // ── Slash commands ────────────────────────────────────

    /// Handles a slash command. Returns the response text.
    /// These are intercepted by the runtime and never reach the LLM.
    fn handle_command(&self, from: &str, body: &str) -> Result<String> {
        let bare_jid = from.split('/').next().unwrap_or(from);
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

        // Context-specific section: room info vs. user info
        let context_info = if is_room {
            format!(
                "Room: {bare_jid}\n\
                 Room messages: {msg_count}\n\
                 Archived sessions: {session_count}"
            )
        } else {
            let has_profile = self.memory.has_user_profile(bare_jid)?;
            let has_memory = self.memory.get_user_memory(bare_jid)?.is_some();
            format!(
                "Your session: {msg_count} messages\n\
                 Archived sessions: {session_count}\n\
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

        Ok(format!(
            "{} — status\n\
             Uptime: {hours}h {minutes}m\n\
             Mode: {}\n\
             LLM: {} ({})\n\
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

    /// Processes an incoming message and produces a response via LLM
    async fn handle_message(&self, from: &str, body: &str) -> Result<String> {
        // Bare JID for memory (without resource)
        let bare_jid = from.split('/').next().unwrap_or(from);

        // Retrieve conversation history and workspace context
        let history = self.memory.get_history(bare_jid, MAX_HISTORY)?;
        let workspace = self.memory.get_workspace_context(bare_jid)?;

        // Build system prompt from workspace files
        let system_prompt = self.build_system_prompt(&workspace);

        // Build message list for LLM
        let mut messages = history;
        messages.push(Message {
            role: "user".to_string(),
            content: body.to_string(),
        });

        // Call LLM
        let response = self.llm.complete(&system_prompt, &messages).await?;

        // Persist messages
        self.memory.store_message(bare_jid, "user", body)?;
        self.memory
            .store_message(bare_jid, "assistant", &response.text)?;

        info!(
            "Response to {bare_jid}: {} chars ({} tokens used)",
            response.text.len(),
            response.input_tokens + response.output_tokens
        );

        Ok(response.text)
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
        let messages = history;

        // Call LLM
        let response = self.llm.complete(&system_prompt, &messages).await?;

        info!(
            "MUC response to {room_jid}: {} chars ({} tokens used)",
            response.text.len(),
            response.input_tokens + response.output_tokens
        );

        Ok(response.text)
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
        let has_global_files = ctx.identity.is_some()
            || ctx.personality.is_some()
            || ctx.instructions.is_some();

        let mut prompt = String::new();

        if has_global_files {
            // Use workspace files for agent configuration
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
            // Hardcoded fallback when no workspace files exist
            prompt.push_str(&format!(
                "You are {}, a personal AI assistant accessible via XMPP.\n\
                 You are direct, helpful, and concise. You respond in the user's language.\n\n\
                 Rules:\n\
                 - Respond concisely, no excessive markdown formatting\n\
                 - If asked to execute an action (send an email, modify a file...), \
                   describe what you would do but clarify that you cannot yet execute \
                   actions (skills are coming in v0.2)\n\
                 - You have memory of previous conversations with this user",
                self.config.agent.name
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
        let memory = Memory::open(tmp.path()).unwrap();
        let runtime = AgentRuntime::new(config, llm, memory);
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
}
