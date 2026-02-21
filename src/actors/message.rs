use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, Result};
use async_trait::async_trait;
use tokio::sync::{mpsc, oneshot, OnceCell};
use tokio::time::timeout;
use tracing::{debug, error, info, warn};

use super::user_error;
use crate::actors::memory_actor::SessionMemoryWriter;
use crate::agent::memory::Attachment;
use crate::config::Config;
use crate::xmpp::component::{ChatState, XmppCommand};
use crate::xmpp::stanzas::{self, IncomingMessage, MessageType, OobData};

pub struct CommandReply {
    pub text: String,
    pub replay_commands: Vec<XmppCommand>,
}

impl CommandReply {
    pub fn text_only(text: String) -> Self {
        Self {
            text,
            replay_commands: vec![],
        }
    }
}

#[async_trait]
pub trait MessageResponder: Send + Sync {
    async fn respond_to_command(&self, from: &str, body: &str) -> Result<CommandReply>;

    async fn respond_to_chat_message(
        &self,
        from: &str,
        body: &str,
        msg_id: Option<&str>,
    ) -> Result<String>;

    async fn respond_to_muc_message(&self, room_jid: &str, body: &str) -> Result<String>;

    async fn respond_to_attachment_message(
        &self,
        message: IncomingMessage,
        cmd_tx: mpsc::Sender<XmppCommand>,
    ) -> Result<()>;
}

const SESSION_MESSAGE_BUSY_ERR: &str = "session_message_busy";
const SESSION_MESSAGE_UNAVAILABLE_ERR: &str = "session_message_unavailable";

struct SessionMessageRequest {
    msg: IncomingMessage,
    cmd_tx: mpsc::Sender<XmppCommand>,
    reply_tx: oneshot::Sender<Result<()>>,
}

/// Mailbox-backed wrapper used by `SessionActor` to isolate message handling.
///
/// The inner `MessageActor` logic is unchanged and remains reusable by the
/// per-conversation session actor path.
pub struct SessionMessageActor {
    config: Arc<Config>,
    memory_writer: Arc<dyn SessionMemoryWriter>,
    responder: Arc<dyn MessageResponder>,
    enqueue_timeout: Duration,
    mailbox: OnceCell<mpsc::Sender<SessionMessageRequest>>,
}

impl SessionMessageActor {
    pub fn new(
        config: Arc<Config>,
        memory_writer: Arc<dyn SessionMemoryWriter>,
        responder: Arc<dyn MessageResponder>,
    ) -> Self {
        let enqueue_timeout =
            Duration::from_millis(config.actors.tooling.skill_queue_timeout_ms.max(1));
        Self {
            config,
            memory_writer,
            responder,
            enqueue_timeout,
            mailbox: OnceCell::new(),
        }
    }

    pub async fn handle(
        &self,
        msg: IncomingMessage,
        cmd_tx: mpsc::Sender<XmppCommand>,
    ) -> Result<()> {
        let (reply_tx, reply_rx) = oneshot::channel();
        let request = SessionMessageRequest {
            msg,
            cmd_tx,
            reply_tx,
        };

        let tx = self.mailbox_tx().await;
        match timeout(self.enqueue_timeout, tx.send(request)).await {
            Ok(Ok(())) => {}
            Ok(Err(_)) => return Err(anyhow!(SESSION_MESSAGE_UNAVAILABLE_ERR)),
            Err(_) => return Err(anyhow!(SESSION_MESSAGE_BUSY_ERR)),
        }

        reply_rx
            .await
            .map_err(|_| anyhow!(SESSION_MESSAGE_UNAVAILABLE_ERR))?
    }

    async fn mailbox_tx(&self) -> mpsc::Sender<SessionMessageRequest> {
        self.mailbox
            .get_or_init(|| async { self.spawn_worker() })
            .await
            .clone()
    }

    fn spawn_worker(&self) -> mpsc::Sender<SessionMessageRequest> {
        let mailbox_size = self.config.actors.session_mailbox.max(1);
        let (tx, mut rx) = mpsc::channel::<SessionMessageRequest>(mailbox_size);
        let config = Arc::clone(&self.config);
        let memory_writer = Arc::clone(&self.memory_writer);
        let responder = Arc::clone(&self.responder);
        let processor = MessageActor::new();

        tokio::spawn(async move {
            while let Some(request) = rx.recv().await {
                let result = processor
                    .handle(
                        request.msg,
                        config.as_ref(),
                        memory_writer.as_ref(),
                        responder.as_ref(),
                        request.cmd_tx,
                    )
                    .await;
                let _ = request.reply_tx.send(result);
            }
        });

        tx
    }
}

#[derive(Default)]
pub struct MessageActor;

impl MessageActor {
    pub fn new() -> Self {
        Self
    }

    pub async fn handle(
        &self,
        msg: IncomingMessage,
        config: &Config,
        memory_writer: &dyn SessionMemoryWriter,
        responder: &dyn MessageResponder,
        cmd_tx: mpsc::Sender<XmppCommand>,
    ) -> Result<()> {
        let bare_from = stanzas::bare_jid(&msg.from);
        let is_muc = msg.message_type == MessageType::GroupChat;

        if is_muc {
            // ── MUC groupchat message ───────────────────
            let room_config = match config.find_room(bare_from) {
                Some(rc) => rc.clone(),
                None => {
                    debug!("Ignoring MUC message from unconfigured room {bare_from}");
                    return Ok(());
                }
            };

            // Filter self-messages (MUC reflects bot's own messages)
            let sender_nick = msg.from.split('/').nth(1).unwrap_or("");
            if sender_nick == room_config.nick {
                return Ok(());
            }

            // Store ALL room messages to history (for full context)
            let sender_label = format!("{sender_nick}@muc");
            let muc_attachments = build_oob_attachments(&msg.oob);
            if let Err(e) = memory_writer
                .store_message_full(
                    bare_from,
                    "user",
                    &msg.body,
                    msg.id.as_deref(),
                    Some(&sender_label),
                    muc_attachments,
                    None,
                )
                .await
            {
                error!("Failed to store MUC message: {e}");
            }

            // Only respond if the bot is mentioned
            if !is_mentioned(&room_config.nick, &msg.body) {
                return Ok(());
            }

            info!("MUC mention from {sender_nick} in {bare_from}");

            // Strip mention prefix before sending to LLM
            let clean_body = strip_mention(&room_config.nick, &msg.body);

            // Process via LLM using room JID as memory key
            let response = if clean_body.starts_with('/') {
                responder.respond_to_command(&msg.from, &clean_body).await
            } else {
                // Send <composing/> to the room before the LLM call
                let _ = cmd_tx
                    .send(XmppCommand::SendChatState {
                        to: bare_from.to_string(),
                        state: ChatState::Composing,
                        msg_type: "groupchat".to_string(),
                    })
                    .await;

                responder
                    .respond_to_muc_message(bare_from, &clean_body)
                    .await
                    .map(CommandReply::text_only)
            };

            let room_jid = bare_from.to_string();
            match response {
                Ok(reply) => {
                    for command in reply.replay_commands {
                        let _ = cmd_tx.send(command).await;
                    }
                    // Generate outbound message id
                    let out_id = uuid::Uuid::new_v4().to_string();
                    if let Err(e) = memory_writer
                        .store_message_structured(
                            &room_jid,
                            "assistant",
                            &reply.text,
                            Some(&out_id),
                            None,
                        )
                        .await
                    {
                        error!("Failed to store MUC response: {e}");
                    }
                    let _ = cmd_tx
                        .send(XmppCommand::SendMucMessage {
                            to: room_jid,
                            body: reply.text,
                            id: Some(out_id),
                        })
                        .await;
                }
                Err(e) => {
                    let correlation_id = user_error::correlation_id_or_new(msg.id.as_deref());
                    error!(
                        correlation_id = %correlation_id,
                        "Error processing MUC message: {e}"
                    );
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
                            body: user_error::to_user_safe_message(&e, &correlation_id),
                            id: None,
                        })
                        .await;
                }
            }
        } else {
            // ── 1:1 chat message ────────────────────────

            // Domain-level security check (rejects cross-domain messages)
            if !config.is_domain_allowed(&msg.from) {
                warn!(
                    "Cross-domain message rejected from {} (domain not allowed)",
                    msg.from
                );
                return Ok(());
            }

            // Per-JID authorization check
            if !config.is_allowed(&msg.from) {
                warn!("Unauthorized message from {}, ignoring", msg.from);
                return Ok(());
            }

            info!("Processing message from {}", msg.from);
            debug!("Message body from {}: {}", msg.from, msg.body);

            // Slash commands are intercepted before the LLM
            if msg.body.starts_with('/') {
                let response = responder.respond_to_command(&msg.from, &msg.body).await;
                match response {
                    Ok(reply) => {
                        for command in reply.replay_commands {
                            let _ = cmd_tx.send(command).await;
                        }
                        let _ = cmd_tx
                            .send(XmppCommand::SendMessage {
                                to: msg.from.clone(),
                                body: reply.text,
                                id: None,
                            })
                            .await;
                    }
                    Err(e) => {
                        let correlation_id = user_error::correlation_id_or_new(msg.id.as_deref());
                        error!(
                            correlation_id = %correlation_id,
                            "Error processing command: {e}"
                        );
                        let _ = cmd_tx
                            .send(XmppCommand::SendMessage {
                                to: msg.from.clone(),
                                body: user_error::to_user_safe_message(&e, &correlation_id),
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

                let from = msg.from.clone();
                let correlation_id = user_error::correlation_id_or_new(msg.id.as_deref());
                if let Err(e) = responder
                    .respond_to_attachment_message(msg, cmd_tx.clone())
                    .await
                {
                    error!(
                        correlation_id = %correlation_id,
                        "Error scheduling attachment message processing: {e}"
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

                let response = responder
                    .respond_to_chat_message(&msg.from, &msg.body, msg.id.as_deref())
                    .await;

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
                        let correlation_id = user_error::correlation_id_or_new(msg.id.as_deref());
                        error!(
                            correlation_id = %correlation_id,
                            "Error processing message: {e}"
                        );
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
                                body: user_error::to_user_safe_message(&e, &correlation_id),
                                id: None,
                            })
                            .await;
                    }
                }
            }
        }

        Ok(())
    }
}

fn build_oob_attachments(oob_list: &[OobData]) -> Option<Vec<Attachment>> {
    if oob_list.is_empty() {
        return None;
    }
    let atts: Vec<Attachment> = oob_list
        .iter()
        .map(|oob| {
            let filename = oob
                .url
                .rsplit('/')
                .next()
                .filter(|s| !s.is_empty())
                .unwrap_or("file")
                .to_string();
            Attachment {
                filename,
                mime_type: "unknown".to_string(),
                size: "unknown".to_string(),
            }
        })
        .collect();
    Some(atts)
}

fn is_mentioned(nick: &str, body: &str) -> bool {
    let lower_body = body.to_lowercase();
    let lower_nick = nick.to_lowercase();
    lower_body.contains(&format!("@{lower_nick}"))
        || lower_body.starts_with(&format!("{lower_nick}:"))
        || lower_body.starts_with(&format!("{lower_nick} "))
}

fn strip_mention(nick: &str, body: &str) -> String {
    let lower_body = body.to_lowercase();
    let lower_nick = nick.to_lowercase();

    // Try "@nick:" or "@nick " at the beginning
    let at_nick = format!("@{lower_nick}");
    if lower_body.starts_with(&at_nick) {
        let rest = &body[at_nick.len()..];
        let trimmed = rest
            .trim_start_matches(':')
            .trim_start_matches(',')
            .trim_start();
        if !trimmed.is_empty() {
            return trimmed.to_string();
        }
    }

    // Try "nick:" or "nick " at the beginning
    if lower_body.starts_with(&format!("{lower_nick}:"))
        || lower_body.starts_with(&format!("{lower_nick} "))
    {
        let rest = &body[nick.len()..];
        let trimmed = rest
            .trim_start_matches(':')
            .trim_start_matches(',')
            .trim_start();
        if !trimmed.is_empty() {
            return trimmed.to_string();
        }
    }

    // "@nick" in the middle — no stripping needed, return full body
    body.to_string()
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::sync::Mutex;

    use super::*;
    use crate::config::{
        ActorMemoryConfig, ActorObservabilityConfig, ActorSupervisionConfig, ActorToolingConfig,
        ActorsConfig, AgentConfig, ConnectionMode, KeepaliveConfig, LlmConfig, MemoryConfig,
        RoomConfig, ServerConfig, SessionConfig, SkillsConfig,
    };

    #[derive(Default)]
    struct TestMemoryWriter {
        structured_calls: Mutex<usize>,
        full_calls: Mutex<usize>,
    }

    impl TestMemoryWriter {
        fn counts(&self) -> (usize, usize) {
            (
                *self.structured_calls.lock().expect("structured lock"),
                *self.full_calls.lock().expect("full lock"),
            )
        }
    }

    #[async_trait::async_trait]
    impl SessionMemoryWriter for TestMemoryWriter {
        async fn store_message_structured(
            &self,
            _jid: &str,
            _role: &str,
            _content: &str,
            _msg_id: Option<&str>,
            _sender: Option<&str>,
        ) -> Result<()> {
            let mut count = self.structured_calls.lock().expect("structured lock");
            *count += 1;
            Ok(())
        }

        async fn store_message_full(
            &self,
            _jid: &str,
            _role: &str,
            _content: &str,
            _msg_id: Option<&str>,
            _sender: Option<&str>,
            _attachments: Option<Vec<Attachment>>,
            _reaction: Option<crate::agent::memory::Reaction>,
        ) -> Result<()> {
            let mut count = self.full_calls.lock().expect("full lock");
            *count += 1;
            Ok(())
        }
    }

    struct StubResponder {
        command_text: String,
        replay_commands: Vec<XmppCommand>,
    }

    impl StubResponder {
        fn with_command_reply(command_text: &str, replay_commands: Vec<XmppCommand>) -> Self {
            Self {
                command_text: command_text.to_string(),
                replay_commands,
            }
        }
    }

    #[async_trait::async_trait]
    impl MessageResponder for StubResponder {
        async fn respond_to_command(&self, _from: &str, _body: &str) -> Result<CommandReply> {
            Ok(CommandReply {
                text: self.command_text.clone(),
                replay_commands: self.replay_commands.clone(),
            })
        }

        async fn respond_to_chat_message(
            &self,
            _from: &str,
            _body: &str,
            _msg_id: Option<&str>,
        ) -> Result<String> {
            Err(anyhow::anyhow!("unexpected chat message call"))
        }

        async fn respond_to_muc_message(&self, _room_jid: &str, _body: &str) -> Result<String> {
            Err(anyhow::anyhow!("unexpected muc message call"))
        }

        async fn respond_to_attachment_message(
            &self,
            _message: IncomingMessage,
            _cmd_tx: mpsc::Sender<XmppCommand>,
        ) -> Result<()> {
            Err(anyhow::anyhow!("unexpected attachment message call"))
        }
    }

    fn test_config(rooms: Vec<RoomConfig>) -> Config {
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
                backend: "markdown".to_string(),
                path: PathBuf::from("./data/memory"),
            },
            rooms,
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

    fn make_message(
        from: &str,
        to: &str,
        body: &str,
        message_type: MessageType,
        id: Option<&str>,
    ) -> IncomingMessage {
        IncomingMessage {
            from: from.to_string(),
            to: to.to_string(),
            body: body.to_string(),
            id: id.map(ToString::to_string),
            message_type,
            oob: vec![],
        }
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
        assert_eq!(strip_mention("bot", "hey @bot help me"), "hey @bot help me");
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

    // ── build_oob_attachments tests ─────────────────────

    #[test]
    fn test_build_oob_attachments_empty() {
        assert!(build_oob_attachments(&[]).is_none());
    }

    #[test]
    fn test_build_oob_attachments_single() {
        let oob = vec![OobData {
            url: "https://upload.example.com/abc/photo.jpg".to_string(),
            desc: None,
        }];
        let atts = build_oob_attachments(&oob).unwrap();
        assert_eq!(atts.len(), 1);
        assert_eq!(atts[0].filename, "photo.jpg");
        assert_eq!(atts[0].mime_type, "unknown");
        assert_eq!(atts[0].size, "unknown");
    }

    #[test]
    fn test_build_oob_attachments_multiple() {
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
        let atts = build_oob_attachments(&oob).unwrap();
        assert_eq!(atts.len(), 2);
        assert_eq!(atts[0].filename, "a.jpg");
        assert_eq!(atts[1].filename, "b.pdf");
    }

    #[test]
    fn test_build_oob_attachments_trailing_slash_fallback() {
        let oob = vec![OobData {
            url: "https://example.com/files/".to_string(),
            desc: None,
        }];
        let atts = build_oob_attachments(&oob).unwrap();
        // Trailing slash: rsplit('/').next() yields "", filter(non-empty) → fallback "file"
        assert_eq!(atts[0].filename, "file");
    }

    #[tokio::test]
    async fn test_chat_command_replay_dispatches_command_before_reply() {
        let actor = MessageActor::new();
        let config = test_config(vec![]);
        let memory_writer = TestMemoryWriter::default();
        let responder =
            StubResponder::with_command_reply("Replay enqueued", vec![XmppCommand::Ping]);
        let (cmd_tx, mut cmd_rx) = mpsc::channel(8);
        let msg = make_message(
            "alice@localhost/mobile",
            "bot@localhost",
            "/deadletters replay corr-chat",
            MessageType::Chat,
            Some("in-chat-1"),
        );

        actor
            .handle(msg, &config, &memory_writer, &responder, cmd_tx)
            .await
            .expect("handle should succeed");

        let first = timeout(Duration::from_millis(200), cmd_rx.recv())
            .await
            .expect("first command timeout")
            .expect("first command");
        assert!(matches!(first, XmppCommand::Ping));

        let second = timeout(Duration::from_millis(200), cmd_rx.recv())
            .await
            .expect("second command timeout")
            .expect("second command");
        match second {
            XmppCommand::SendMessage { to, body, id } => {
                assert_eq!(to, "alice@localhost/mobile");
                assert_eq!(body, "Replay enqueued");
                assert!(id.is_none());
            }
            other => panic!("expected SendMessage, got {other:?}"),
        }

        let third = timeout(Duration::from_millis(30), cmd_rx.recv())
            .await
            .expect("third recv should complete");
        assert!(third.is_none());
        assert_eq!(memory_writer.counts(), (0, 0));
    }

    #[tokio::test]
    async fn test_muc_command_replay_dispatches_command_before_room_reply() {
        let actor = MessageActor::new();
        let config = test_config(vec![RoomConfig {
            jid: "lobby@conference.localhost".to_string(),
            nick: "bot".to_string(),
        }]);
        let memory_writer = TestMemoryWriter::default();
        let responder =
            StubResponder::with_command_reply("Replay enqueued in room", vec![XmppCommand::Ping]);
        let (cmd_tx, mut cmd_rx) = mpsc::channel(8);
        let msg = make_message(
            "lobby@conference.localhost/alice",
            "bot@localhost",
            "@bot /deadletters replay corr-room",
            MessageType::GroupChat,
            Some("in-room-1"),
        );

        actor
            .handle(msg, &config, &memory_writer, &responder, cmd_tx)
            .await
            .expect("handle should succeed");

        let first = timeout(Duration::from_millis(200), cmd_rx.recv())
            .await
            .expect("first command timeout")
            .expect("first command");
        assert!(matches!(first, XmppCommand::Ping));

        let second = timeout(Duration::from_millis(200), cmd_rx.recv())
            .await
            .expect("second command timeout")
            .expect("second command");
        match second {
            XmppCommand::SendMucMessage { to, body, id } => {
                assert_eq!(to, "lobby@conference.localhost");
                assert_eq!(body, "Replay enqueued in room");
                assert!(id.is_some());
            }
            other => panic!("expected SendMucMessage, got {other:?}"),
        }

        let third = timeout(Duration::from_millis(30), cmd_rx.recv())
            .await
            .expect("third recv should complete");
        assert!(third.is_none());
        assert_eq!(memory_writer.counts(), (1, 1));
    }
}
