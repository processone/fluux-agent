use anyhow::Result;
use tokio::sync::mpsc;
use tracing::{debug, error, info, warn};

use crate::config::Config;
use crate::llm::{AnthropicClient, Message};
use crate::xmpp::component::{ChatState, XmppCommand, XmppEvent};
use crate::xmpp::stanzas::PresenceType;

use super::memory::Memory;

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
                }
                XmppEvent::Message(msg) => {
                    // Authorization check
                    if !self.config.is_allowed(&msg.from) {
                        warn!("Unauthorized message from {}, ignoring", msg.from);
                        continue;
                    }

                    info!("Processing message from {}: {}", msg.from, msg.body);

                    // Slash commands are intercepted before the LLM — instant, no typing indicator
                    let response = if msg.body.starts_with('/') {
                        self.handle_command(&msg.from, &msg.body)
                    } else {
                        // Send <composing/> before the LLM call so the user sees "typing..."
                        let _ = cmd_tx
                            .send(XmppCommand::SendChatState {
                                to: msg.from.clone(),
                                state: ChatState::Composing,
                            })
                            .await;

                        self.handle_message(&msg.from, &msg.body).await
                    };

                    match response {
                        Ok(text) => {
                            // The response message includes <active/> chat state,
                            // which clears the "typing..." indicator on the client.
                            let _ = cmd_tx
                                .send(XmppCommand::SendMessage {
                                    to: msg.from.clone(),
                                    body: text,
                                })
                                .await;
                        }
                        Err(e) => {
                            error!("Error processing message: {e}");
                            // Send <paused/> to clear "typing..." before the error message
                            let _ = cmd_tx
                                .send(XmppCommand::SendChatState {
                                    to: msg.from.clone(),
                                    state: ChatState::Paused,
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
                XmppEvent::Presence(pres) => {
                    let bare_jid = pres.from.split('/').next().unwrap_or(&pres.from);
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

    /// /status — Agent status overview
    fn cmd_status(&self, bare_jid: &str) -> Result<String> {
        let uptime = self.start_time.elapsed();
        let hours = uptime.as_secs() / 3600;
        let minutes = (uptime.as_secs() % 3600) / 60;

        let msg_count = self.memory.message_count(bare_jid)?;
        let session_count = self.memory.session_count(bare_jid)?;
        let has_context = self.memory.get_user_context(bare_jid)?.is_some();

        Ok(format!(
            "{} — status\n\
             Uptime: {hours}h {minutes}m\n\
             Mode: {}\n\
             LLM: {} ({})\n\
             Your session: {msg_count} messages\n\
             Archived sessions: {session_count}\n\
             User context: {}",
            self.config.agent.name,
            self.config.server.mode_description(),
            self.config.llm.provider,
            self.config.llm.model,
            if has_context { "yes" } else { "none" },
        ))
    }

    /// /help — List available commands
    fn cmd_help(&self) -> String {
        "\
Commands:\n\
  /new     — Start a new conversation (archive current session)\n\
  /forget  — Erase your history and context\n\
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

        // Retrieve conversation history
        let history = self.memory.get_history(bare_jid, MAX_HISTORY)?;
        let user_context = self.memory.get_user_context(bare_jid)?;

        // Build system prompt
        let system_prompt = self.build_system_prompt(user_context.as_deref());

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

    fn build_system_prompt(&self, user_context: Option<&str>) -> String {
        let mut prompt = format!(
            "You are {}, a personal AI assistant accessible via XMPP.\n\
             You are direct, helpful, and concise. You respond in the user's language.\n\n\
             Rules:\n\
             - Respond concisely, no excessive markdown formatting\n\
             - If asked to execute an action (send an email, modify a file...), \
               describe what you would do but clarify that you cannot yet execute \
               actions (skills are coming in v0.2)\n\
             - You have memory of previous conversations with this user",
            self.config.agent.name
        );

        if let Some(ctx) = user_context {
            prompt.push_str(&format!("\n\nContext about this user:\n{ctx}"));
        }

        prompt
    }
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
            },
            memory: MemoryConfig {
                backend: "markdown".to_string(),
                path: tmp.path().to_path_buf(),
            },
        };

        let llm = AnthropicClient::new(config.llm.clone());
        let memory = Memory::open(tmp.path()).unwrap();
        let runtime = AgentRuntime::new(config, llm, memory);
        (runtime, tmp)
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
    fn test_command_status_content() {
        let (rt, _tmp) = test_runtime();
        rt.memory.store_message("admin@localhost", "user", "Hi").unwrap();

        let result = rt.handle_command("admin@localhost/res", "/status").unwrap();
        assert!(result.contains("Test Agent"));
        assert!(result.contains("Uptime:"));
        assert!(result.contains("C2S client"));
        assert!(result.contains("anthropic"));
        assert!(result.contains("1 messages"));
        assert!(result.contains("User context: none"));
    }

    #[test]
    fn test_command_status_with_context() {
        let (rt, _tmp) = test_runtime();
        rt.memory.set_user_context("admin@localhost", "Developer").unwrap();

        let result = rt.handle_command("admin@localhost/res", "/status").unwrap();
        assert!(result.contains("User context: yes"));
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
    fn test_build_system_prompt_without_context() {
        let (rt, _tmp) = test_runtime();
        let prompt = rt.build_system_prompt(None);
        assert!(prompt.contains("Test Agent"));
        assert!(prompt.contains("XMPP"));
        assert!(!prompt.contains("Context about this user"));
    }

    #[test]
    fn test_build_system_prompt_with_context() {
        let (rt, _tmp) = test_runtime();
        let prompt = rt.build_system_prompt(Some("Prefers French. Works at Acme Corp."));
        assert!(prompt.contains("Test Agent"));
        assert!(prompt.contains("Context about this user"));
        assert!(prompt.contains("Prefers French"));
        assert!(prompt.contains("Acme Corp"));
    }
}
