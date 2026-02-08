use anyhow::Result;
use tokio::sync::mpsc;
use tracing::{error, info, warn};

use crate::config::Config;
use crate::llm::{AnthropicClient, Message};
use crate::xmpp::component::{XmppCommand, XmppEvent};

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

                    // Slash commands are intercepted before the LLM
                    let response = if msg.body.starts_with('/') {
                        self.handle_command(&msg.from, &msg.body)
                    } else {
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
                                .send(XmppCommand::SendMessage {
                                    to: msg.from.clone(),
                                    body: format!("Sorry, an error occurred: {e}"),
                                })
                                .await;
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
