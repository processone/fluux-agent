use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use tokio::sync::mpsc;
use tracing::{info, warn};

use super::dead_letter::DeadLetterService;
use super::memory_actor::SessionMemoryWriter;
use super::message::MessageResponder;
use super::observability;
use super::reaction::ReactionResponder;
use super::router::RouterActor;
use super::session::SessionDependencies;
use super::types::Envelope;
use super::xmpp_egress::XmppEgressActor;
use super::xmpp_ingress::XmppIngressActor;
use crate::config::{ActorsConfig, Config, KeepaliveConfig};
use crate::xmpp::component::{DisconnectReason, XmppCommand, XmppEvent};

pub struct ActorSupervisorDependencies {
    config: Arc<Config>,
    memory_writer: Arc<dyn SessionMemoryWriter>,
    message_responder: Arc<dyn MessageResponder>,
    reaction_responder: Arc<dyn ReactionResponder>,
    dead_letter: Arc<DeadLetterService>,
}

impl ActorSupervisorDependencies {
    pub fn new(
        config: Arc<Config>,
        memory_writer: Arc<dyn SessionMemoryWriter>,
        message_responder: Arc<dyn MessageResponder>,
        reaction_responder: Arc<dyn ReactionResponder>,
        dead_letter: Arc<DeadLetterService>,
    ) -> Self {
        Self {
            config,
            memory_writer,
            message_responder,
            reaction_responder,
            dead_letter,
        }
    }
}

pub struct ActorSupervisor {
    config: ActorsConfig,
    keepalive: KeepaliveConfig,
    dependencies: ActorSupervisorDependencies,
}

impl ActorSupervisor {
    pub fn new(
        config: ActorsConfig,
        keepalive: KeepaliveConfig,
        dependencies: ActorSupervisorDependencies,
    ) -> Self {
        Self {
            config,
            keepalive,
            dependencies,
        }
    }

    pub async fn run(
        &self,
        event_rx: mpsc::Receiver<XmppEvent>,
        cmd_tx: mpsc::Sender<XmppCommand>,
    ) -> Result<DisconnectReason> {
        observability::configure(&self.config.observability);
        let metrics_exporter = observability::spawn_exporter();

        let router_mailbox = self.config.router_mailbox.max(1);
        info!(
            router_mailbox,
            session_mailbox = self.config.session_mailbox,
            max_active_sessions = self.config.max_active_sessions,
            "ActorSupervisor starting phase-B topology",
        );

        let (ingress_to_router_tx, ingress_to_router_rx) =
            mpsc::channel::<Envelope>(router_mailbox);
        let (actor_to_egress_tx, actor_to_egress_rx) = mpsc::channel::<XmppCommand>(router_mailbox);

        let ingress = XmppIngressActor::new(self.config.dedupe_ttl_secs);
        let session_dependencies = SessionDependencies::new(
            Arc::clone(&self.dependencies.config),
            Arc::clone(&self.dependencies.memory_writer),
            Arc::clone(&self.dependencies.message_responder),
            Arc::clone(&self.dependencies.reaction_responder),
        );
        let dead_letter = Arc::clone(&self.dependencies.dead_letter);
        let router = RouterActor::with_supervision_service(
            session_dependencies,
            self.config.session_mailbox,
            self.config.max_active_sessions,
            self.config.session_idle_ttl_secs,
            self.config.busy_retry_after_secs,
            Arc::clone(&dead_letter),
            self.config.supervision.restart_backoff_min_ms,
            self.config.supervision.restart_backoff_max_ms,
            self.config.supervision.max_restarts_per_minute,
        );
        let egress = XmppEgressActor::with_dead_letter_service(
            self.config.supervision.egress_send_timeout_ms,
            self.config.supervision.egress_retry_backoff_ms,
            self.config.supervision.egress_max_retries,
            dead_letter,
        );

        let ingress_handle =
            tokio::spawn(async move { ingress.run(event_rx, ingress_to_router_tx).await });
        let egress_handle =
            tokio::spawn(async move { egress.run(actor_to_egress_rx, cmd_tx).await });

        let ping_handle = if self.keepalive.enabled {
            let ping_interval_secs = self.keepalive.ping_interval_secs.max(1);
            let ping_tx = actor_to_egress_tx.clone();
            Some(tokio::spawn(async move {
                let mut ping_interval =
                    tokio::time::interval(Duration::from_secs(ping_interval_secs));
                // Avoid sending a ping immediately on connect.
                ping_interval.tick().await;
                loop {
                    ping_interval.tick().await;
                    if ping_tx.send(XmppCommand::Ping).await.is_err() {
                        break;
                    }
                }
            }))
        } else {
            None
        };

        let router_result = router.run(ingress_to_router_rx, actor_to_egress_tx).await;

        ingress_handle.abort();
        egress_handle.abort();
        if let Some(handle) = ping_handle {
            handle.abort();
            let _ = handle.await;
        }

        let _ = ingress_handle.await;
        let _ = egress_handle.await;

        if let Err(ref err) = router_result {
            warn!("Router actor failed: {err}");
        }

        if let Some(exporter) = metrics_exporter {
            exporter.shutdown().await;
        }

        router_result
    }
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};
    use std::sync::Arc;
    use std::time::Duration;

    use async_trait::async_trait;

    use super::*;
    use crate::actors::test_actor_runtime_harness::{
        build_actor_runtime_fixture, build_default_actor_runtime_fixture,
    };
    use crate::agent::files::{AttachmentDownloader, DownloadedFile, FileCategory, FileDownloader};
    use crate::agent::runtime::ActorRuntimeDependencies;
    use crate::config::{
        ActorMemoryConfig, ActorObservabilityConfig, ActorSupervisionConfig, ActorToolingConfig,
        RoomConfig,
    };
    use crate::llm::{
        InputContentBlock, LlmClient, LlmResponse, Message, MessageContent, StopReason, ToolCall,
        ToolDefinition,
    };
    use crate::skills::{Skill, SkillContext, SkillRegistry};
    use crate::xmpp::stanzas::{
        IncomingMessage, IncomingPresence, MessageType, OobData, PresenceType,
    };
    use tempfile::TempDir;
    use tokio::sync::Mutex;

    #[derive(Debug, Clone, PartialEq, Eq)]
    enum NormalizedCommand {
        SendMessage {
            to: String,
            body: String,
        },
        SendChatState {
            to: String,
            state: String,
            msg_type: String,
        },
        SendMucMessage {
            to: String,
            body: String,
        },
        JoinMuc {
            room: String,
            nick: String,
        },
        SendRaw(String),
        Ping,
    }

    fn normalize_command(cmd: XmppCommand) -> NormalizedCommand {
        match cmd {
            XmppCommand::SendMessage { to, body, .. } => {
                NormalizedCommand::SendMessage { to, body }
            }
            XmppCommand::SendChatState {
                to,
                state,
                msg_type,
            } => NormalizedCommand::SendChatState {
                to,
                state: match state {
                    crate::xmpp::component::ChatState::Composing => "composing".to_string(),
                    crate::xmpp::component::ChatState::Paused => "paused".to_string(),
                },
                msg_type,
            },
            XmppCommand::SendMucMessage { to, body, .. } => {
                NormalizedCommand::SendMucMessage { to, body }
            }
            XmppCommand::JoinMuc { room, nick } => NormalizedCommand::JoinMuc { room, nick },
            XmppCommand::SendRaw(raw) => NormalizedCommand::SendRaw(raw),
            XmppCommand::Ping => NormalizedCommand::Ping,
        }
    }

    fn chat_event(from: &str, msg_id: &str, body: &str) -> XmppEvent {
        XmppEvent::Message(IncomingMessage {
            from: from.to_string(),
            to: "bot@localhost".to_string(),
            body: body.to_string(),
            id: Some(msg_id.to_string()),
            message_type: MessageType::Chat,
            oob: vec![],
        })
    }

    fn groupchat_event(from: &str, msg_id: &str, body: &str) -> XmppEvent {
        XmppEvent::Message(IncomingMessage {
            from: from.to_string(),
            to: "bot@localhost".to_string(),
            body: body.to_string(),
            id: Some(msg_id.to_string()),
            message_type: MessageType::GroupChat,
            oob: vec![],
        })
    }

    fn chat_event_with_oob(from: &str, msg_id: &str, body: &str, url: &str) -> XmppEvent {
        XmppEvent::Message(IncomingMessage {
            from: from.to_string(),
            to: "bot@localhost".to_string(),
            body: body.to_string(),
            id: Some(msg_id.to_string()),
            message_type: MessageType::Chat,
            oob: vec![OobData {
                url: url.to_string(),
                desc: None,
            }],
        })
    }

    fn parity_fixture_events() -> Vec<XmppEvent> {
        vec![
            XmppEvent::Connected,
            chat_event("alice@localhost/phone", "parity-1", "/ping"),
            chat_event("alice@localhost/phone", "parity-2", "/help"),
        ]
    }

    fn presence_subscribe_event(from: &str) -> XmppEvent {
        XmppEvent::Presence(IncomingPresence {
            from: from.to_string(),
            presence_type: PresenceType::Subscribe,
        })
    }

    #[derive(Clone)]
    struct DeterministicLlm {
        response: String,
    }

    impl DeterministicLlm {
        fn new(response: impl Into<String>) -> Self {
            Self {
                response: response.into(),
            }
        }
    }

    #[async_trait]
    impl LlmClient for DeterministicLlm {
        async fn complete(
            &self,
            _system_prompt: &str,
            _messages: &[Message],
            _tools: Option<&[ToolDefinition]>,
        ) -> anyhow::Result<LlmResponse> {
            Ok(LlmResponse {
                text: self.response.clone(),
                tool_calls: vec![],
                stop_reason: StopReason::EndTurn,
                input_tokens: 1,
                output_tokens: 1,
                content_blocks: vec![InputContentBlock::Text {
                    text: self.response.clone(),
                }],
            })
        }

        fn description(&self) -> String {
            "deterministic-test-llm".to_string()
        }
    }

    #[derive(Default)]
    struct ToolProbeState {
        sent_tool: bool,
    }

    #[derive(Clone)]
    struct ToolProbeLlm {
        tool_name: String,
        tool_input: serde_json::Value,
        state: Arc<Mutex<ToolProbeState>>,
    }

    impl ToolProbeLlm {
        fn new(tool_name: impl Into<String>, tool_input: serde_json::Value) -> Self {
            Self {
                tool_name: tool_name.into(),
                tool_input,
                state: Arc::new(Mutex::new(ToolProbeState::default())),
            }
        }
    }

    fn latest_tool_result(messages: &[Message]) -> Option<String> {
        messages
            .iter()
            .rev()
            .find_map(|message| match &message.content {
                MessageContent::Blocks(blocks) => {
                    blocks.iter().rev().find_map(|block| match block {
                        InputContentBlock::ToolResult { content, .. } => Some(content.clone()),
                        _ => None,
                    })
                }
                _ => None,
            })
    }

    #[async_trait]
    impl LlmClient for ToolProbeLlm {
        async fn complete(
            &self,
            _system_prompt: &str,
            messages: &[Message],
            _tools: Option<&[ToolDefinition]>,
        ) -> anyhow::Result<LlmResponse> {
            let mut state = self.state.lock().await;
            if !state.sent_tool {
                state.sent_tool = true;
                return Ok(LlmResponse {
                    text: String::new(),
                    tool_calls: vec![ToolCall {
                        id: "toolu-fixture-1".to_string(),
                        name: self.tool_name.clone(),
                        input: self.tool_input.clone(),
                    }],
                    stop_reason: StopReason::ToolUse,
                    input_tokens: 5,
                    output_tokens: 7,
                    content_blocks: vec![InputContentBlock::ToolUse {
                        id: "toolu-fixture-1".to_string(),
                        name: self.tool_name.clone(),
                        input: self.tool_input.clone(),
                    }],
                });
            }

            let observed =
                latest_tool_result(messages).unwrap_or_else(|| "<missing tool_result>".to_string());
            let text = format!("tool-result: {observed}");
            Ok(LlmResponse {
                text: text.clone(),
                tool_calls: vec![],
                stop_reason: StopReason::EndTurn,
                input_tokens: 3,
                output_tokens: 3,
                content_blocks: vec![InputContentBlock::Text { text }],
            })
        }

        fn description(&self) -> String {
            "tool-probe-test-llm".to_string()
        }
    }

    struct FastFixtureSkill;

    #[async_trait]
    impl Skill for FastFixtureSkill {
        fn name(&self) -> &str {
            "fixture_fast_skill"
        }

        fn description(&self) -> &str {
            "Fast fixture skill used by parity tests"
        }

        fn parameters_schema(&self) -> serde_json::Value {
            serde_json::json!({
                "type": "object",
                "properties": {
                    "query": { "type": "string" }
                },
                "required": ["query"]
            })
        }

        fn capabilities(&self) -> Vec<String> {
            vec!["network:api.example.com:443".to_string()]
        }

        async fn execute(
            &self,
            _params: serde_json::Value,
            _context: &SkillContext,
        ) -> anyhow::Result<String> {
            Ok("fixture-fast-ok".to_string())
        }
    }

    struct SlowFixtureSkill;

    #[async_trait]
    impl Skill for SlowFixtureSkill {
        fn name(&self) -> &str {
            "fixture_slow_skill"
        }

        fn description(&self) -> &str {
            "Slow fixture skill used by parity timeout tests"
        }

        fn parameters_schema(&self) -> serde_json::Value {
            serde_json::json!({
                "type": "object",
                "properties": {
                    "query": { "type": "string" }
                },
                "required": ["query"]
            })
        }

        fn capabilities(&self) -> Vec<String> {
            vec!["network:api.example.com:443".to_string()]
        }

        async fn execute(
            &self,
            _params: serde_json::Value,
            _context: &SkillContext,
        ) -> anyhow::Result<String> {
            tokio::time::sleep(Duration::from_secs(2)).await;
            Ok("fixture-slow-ok".to_string())
        }
    }

    fn fixture_fast_skills() -> SkillRegistry {
        let mut skills = SkillRegistry::new();
        skills.register(Box::new(FastFixtureSkill));
        skills
    }

    fn fixture_slow_skills() -> SkillRegistry {
        let mut skills = SkillRegistry::new();
        skills.register(Box::new(SlowFixtureSkill));
        skills
    }

    #[derive(Clone)]
    struct MockAttachmentDownloader {
        filename: String,
        mime_type: String,
        bytes: Vec<u8>,
        category: FileCategory,
    }

    impl MockAttachmentDownloader {
        fn new_pdf_fixture(filename: impl Into<String>, bytes: Vec<u8>) -> Self {
            Self {
                filename: filename.into(),
                mime_type: "application/pdf".to_string(),
                bytes,
                category: FileCategory::Document,
            }
        }
    }

    #[async_trait]
    impl AttachmentDownloader for MockAttachmentDownloader {
        async fn download(&self, _url: &str, files_dir: &Path) -> anyhow::Result<DownloadedFile> {
            tokio::fs::create_dir_all(files_dir).await?;
            let path = files_dir.join(&self.filename);
            tokio::fs::write(&path, &self.bytes).await?;
            Ok(DownloadedFile {
                path,
                filename: self.filename.clone(),
                mime_type: self.mime_type.clone(),
                size: self.bytes.len() as u64,
                category: self.category.clone(),
            })
        }
    }

    async fn collect_exact_commands(
        cmd_rx: &mut mpsc::Receiver<XmppCommand>,
        expected: usize,
        label: &str,
    ) -> Vec<NormalizedCommand> {
        let mut commands = Vec::with_capacity(expected);
        let deadline = tokio::time::Instant::now() + Duration::from_secs(3);

        while commands.len() < expected && tokio::time::Instant::now() < deadline {
            match tokio::time::timeout(Duration::from_millis(100), cmd_rx.recv()).await {
                Ok(Some(cmd)) => commands.push(normalize_command(cmd)),
                Ok(None) => break,
                Err(_) => {}
            }
        }

        assert_eq!(
            commands.len(),
            expected,
            "{label}: expected {expected} outbound commands, got {}",
            commands.len()
        );
        commands
    }

    async fn collect_remaining_commands(
        cmd_rx: &mut mpsc::Receiver<XmppCommand>,
    ) -> Vec<NormalizedCommand> {
        let mut commands = Vec::new();
        loop {
            match tokio::time::timeout(Duration::from_millis(100), cmd_rx.recv()).await {
                Ok(Some(cmd)) => commands.push(normalize_command(cmd)),
                Ok(None) | Err(_) => break,
            }
        }
        commands
    }

    async fn run_actor_fixture(
        supervisor: ActorSupervisor,
        events: Vec<XmppEvent>,
        expected_commands: usize,
    ) -> (DisconnectReason, Vec<NormalizedCommand>) {
        let (event_tx, event_rx) = mpsc::channel(32);
        let (cmd_tx, mut cmd_rx) = mpsc::channel(64);

        let handle = tokio::spawn(async move { supervisor.run(event_rx, cmd_tx).await });

        for event in events {
            event_tx.send(event).await.unwrap();
        }

        let mut commands = collect_exact_commands(&mut cmd_rx, expected_commands, "actor").await;

        event_tx
            .send(XmppEvent::StreamError("conflict".to_string()))
            .await
            .unwrap();

        let reason = tokio::time::timeout(Duration::from_secs(3), handle)
            .await
            .expect("actor supervisor timed out")
            .unwrap()
            .unwrap();

        drop(event_tx);
        commands.extend(collect_remaining_commands(&mut cmd_rx).await);
        (reason, commands)
    }

    fn fixture_actors_config(dead_letter_path: PathBuf) -> ActorsConfig {
        ActorsConfig {
            enabled: true,
            router_mailbox: 32,
            session_mailbox: 8,
            max_active_sessions: 8,
            session_idle_ttl_secs: 1800,
            busy_retry_after_secs: 5,
            dedupe_ttl_secs: 600,
            dead_letter_path,
            tooling: ActorToolingConfig::default(),
            supervision: ActorSupervisionConfig::default(),
            memory: ActorMemoryConfig::default(),
            observability: ActorObservabilityConfig::default(),
        }
    }

    fn keepalive_disabled() -> KeepaliveConfig {
        KeepaliveConfig {
            enabled: false,
            ping_interval_secs: 60,
            read_timeout_secs: 300,
        }
    }

    async fn run_actor_fixture_with_runtime(
        runtime: &ActorRuntimeDependencies,
        dead_letter_path: PathBuf,
        events: Vec<XmppEvent>,
        expected_commands: usize,
    ) -> (DisconnectReason, Vec<NormalizedCommand>) {
        let supervisor = ActorSupervisor::new(
            fixture_actors_config(dead_letter_path),
            keepalive_disabled(),
            supervisor_dependencies(runtime),
        );
        run_actor_fixture(supervisor, events, expected_commands).await
    }

    fn test_runtime_with_allowed_jids(
        allowed_jids: Vec<String>,
    ) -> (ActorRuntimeDependencies, TempDir) {
        build_default_actor_runtime_fixture(allowed_jids)
    }

    fn test_runtime_with_llm(
        allowed_jids: Vec<String>,
        llm: Arc<dyn LlmClient>,
    ) -> (ActorRuntimeDependencies, TempDir) {
        test_runtime_with_llm_and_rooms(allowed_jids, vec![], llm)
    }

    fn test_runtime_with_llm_and_actor_memory(
        allowed_jids: Vec<String>,
        llm: Arc<dyn LlmClient>,
        actor_memory: ActorMemoryConfig,
    ) -> (ActorRuntimeDependencies, TempDir) {
        let file_downloader: Arc<dyn AttachmentDownloader> = Arc::new(FileDownloader::new(3));
        test_runtime_with_llm_and_rooms_and_downloader_and_skills_and_tooling_and_memory(
            allowed_jids,
            vec![],
            llm,
            file_downloader,
            SkillRegistry::new(),
            ActorToolingConfig::default(),
            actor_memory,
        )
    }

    fn test_runtime_with_llm_and_rooms(
        allowed_jids: Vec<String>,
        rooms: Vec<RoomConfig>,
        llm: Arc<dyn LlmClient>,
    ) -> (ActorRuntimeDependencies, TempDir) {
        let file_downloader: Arc<dyn AttachmentDownloader> = Arc::new(FileDownloader::new(3));
        test_runtime_with_llm_and_rooms_and_downloader(allowed_jids, rooms, llm, file_downloader)
    }

    fn test_runtime_with_llm_and_rooms_and_downloader(
        allowed_jids: Vec<String>,
        rooms: Vec<RoomConfig>,
        llm: Arc<dyn LlmClient>,
        file_downloader: Arc<dyn AttachmentDownloader>,
    ) -> (ActorRuntimeDependencies, TempDir) {
        test_runtime_with_llm_and_rooms_and_downloader_and_skills_and_tooling(
            allowed_jids,
            rooms,
            llm,
            file_downloader,
            SkillRegistry::new(),
            ActorToolingConfig::default(),
        )
    }

    fn test_runtime_with_llm_and_rooms_and_downloader_and_skills_and_tooling(
        allowed_jids: Vec<String>,
        rooms: Vec<RoomConfig>,
        llm: Arc<dyn LlmClient>,
        file_downloader: Arc<dyn AttachmentDownloader>,
        skills: SkillRegistry,
        tooling: ActorToolingConfig,
    ) -> (ActorRuntimeDependencies, TempDir) {
        test_runtime_with_llm_and_rooms_and_downloader_and_skills_and_tooling_and_memory(
            allowed_jids,
            rooms,
            llm,
            file_downloader,
            skills,
            tooling,
            ActorMemoryConfig::default(),
        )
    }

    fn test_runtime_with_llm_and_rooms_and_downloader_and_skills_and_tooling_and_memory(
        allowed_jids: Vec<String>,
        rooms: Vec<RoomConfig>,
        llm: Arc<dyn LlmClient>,
        file_downloader: Arc<dyn AttachmentDownloader>,
        skills: SkillRegistry,
        tooling: ActorToolingConfig,
        actor_memory: ActorMemoryConfig,
    ) -> (ActorRuntimeDependencies, TempDir) {
        build_actor_runtime_fixture(
            allowed_jids,
            rooms,
            llm,
            file_downloader,
            skills,
            tooling,
            actor_memory,
        )
    }

    fn test_runtime() -> (ActorRuntimeDependencies, TempDir) {
        test_runtime_with_allowed_jids(vec![])
    }

    fn supervisor_dependencies(runtime: &ActorRuntimeDependencies) -> ActorSupervisorDependencies {
        let session_responder = Arc::clone(&runtime.session_responder);
        let message_responder: Arc<dyn MessageResponder> = session_responder.clone();
        let reaction_responder: Arc<dyn ReactionResponder> = session_responder;
        ActorSupervisorDependencies::new(
            Arc::clone(&runtime.config),
            Arc::clone(&runtime.memory_writer),
            message_responder,
            reaction_responder,
            Arc::clone(&runtime.dead_letter_service),
        )
    }

    async fn assert_tool_chat_fixture_actor_only(
        fixture_name: &str,
        actor_runtime: &ActorRuntimeDependencies,
        actor_dead_letter_path: PathBuf,
        msg_id: &str,
        prompt: &str,
        expected_reply: &str,
    ) {
        let fixture = || {
            vec![
                XmppEvent::Connected,
                chat_event("alice@localhost/phone", msg_id, prompt),
            ]
        };

        let (actor_reason, actor_commands) =
            run_actor_fixture_with_runtime(actor_runtime, actor_dead_letter_path, fixture(), 2)
                .await;
        assert_eq!(
            actor_reason,
            DisconnectReason::Conflict,
            "{fixture_name}: actor shutdown reason mismatch"
        );
        assert_eq!(
            actor_commands,
            vec![
                NormalizedCommand::SendChatState {
                    to: "alice@localhost/phone".to_string(),
                    state: "composing".to_string(),
                    msg_type: "chat".to_string(),
                },
                NormalizedCommand::SendMessage {
                    to: "alice@localhost/phone".to_string(),
                    body: expected_reply.to_string(),
                },
            ],
            "{fixture_name}: expected tool-result chat reply",
        );
    }

    #[tokio::test]
    async fn test_supervisor_emits_periodic_keepalive_ping() {
        let (runtime, _tmp) = test_runtime();

        let actors = ActorsConfig {
            enabled: true,
            router_mailbox: 16,
            session_mailbox: 4,
            max_active_sessions: 4,
            session_idle_ttl_secs: 1800,
            busy_retry_after_secs: 5,
            dedupe_ttl_secs: 600,
            dead_letter_path: PathBuf::from("data/dead_letters.jsonl"),
            tooling: ActorToolingConfig::default(),
            supervision: ActorSupervisionConfig::default(),
            memory: ActorMemoryConfig::default(),
            observability: ActorObservabilityConfig::default(),
        };

        let keepalive = KeepaliveConfig {
            enabled: true,
            ping_interval_secs: 1,
            read_timeout_secs: 300,
        };

        let supervisor = ActorSupervisor::new(actors, keepalive, supervisor_dependencies(&runtime));

        let (event_tx, event_rx) = mpsc::channel(8);
        let (cmd_tx, mut cmd_rx) = mpsc::channel(8);

        let handle = tokio::spawn(async move { supervisor.run(event_rx, cmd_tx).await });

        let saw_ping = tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                match cmd_rx.recv().await {
                    Some(XmppCommand::Ping) => return true,
                    Some(_) => continue,
                    None => return false,
                }
            }
        })
        .await
        .unwrap_or(false);

        assert!(saw_ping, "expected at least one periodic keepalive ping");

        drop(event_tx);
        let reason = handle.await.unwrap().unwrap();
        assert_eq!(reason, DisconnectReason::ConnectionLost);
    }

    #[tokio::test]
    async fn test_supervisor_can_restart_after_disconnect_reason() {
        let (runtime, _tmp) = test_runtime();

        let actors = ActorsConfig {
            enabled: true,
            router_mailbox: 16,
            session_mailbox: 4,
            max_active_sessions: 4,
            session_idle_ttl_secs: 1800,
            busy_retry_after_secs: 5,
            dedupe_ttl_secs: 600,
            dead_letter_path: PathBuf::from("data/dead_letters.jsonl"),
            tooling: ActorToolingConfig::default(),
            supervision: ActorSupervisionConfig::default(),
            memory: ActorMemoryConfig::default(),
            observability: ActorObservabilityConfig::default(),
        };

        let keepalive_disabled = KeepaliveConfig {
            enabled: false,
            ping_interval_secs: 60,
            read_timeout_secs: 300,
        };

        let supervisor_first = ActorSupervisor::new(
            actors.clone(),
            keepalive_disabled.clone(),
            supervisor_dependencies(&runtime),
        );
        let (event_tx_1, event_rx_1) = mpsc::channel(8);
        let (cmd_tx_1, _cmd_rx_1) = mpsc::channel(8);
        let handle_1 =
            tokio::spawn(async move { supervisor_first.run(event_rx_1, cmd_tx_1).await });

        event_tx_1
            .send(XmppEvent::StreamError("conflict".to_string()))
            .await
            .unwrap();

        let reason_1 = tokio::time::timeout(Duration::from_secs(2), handle_1)
            .await
            .expect("first supervisor run timed out")
            .unwrap()
            .unwrap();
        assert_eq!(reason_1, DisconnectReason::Conflict);
        drop(event_tx_1);

        let supervisor_second = ActorSupervisor::new(
            actors,
            keepalive_disabled,
            supervisor_dependencies(&runtime),
        );
        let (event_tx_2, event_rx_2) = mpsc::channel(8);
        let (cmd_tx_2, _cmd_rx_2) = mpsc::channel(8);
        let handle_2 =
            tokio::spawn(async move { supervisor_second.run(event_rx_2, cmd_tx_2).await });

        drop(event_tx_2);
        let reason_2 = tokio::time::timeout(Duration::from_secs(2), handle_2)
            .await
            .expect("second supervisor run timed out")
            .unwrap()
            .unwrap();
        assert_eq!(reason_2, DisconnectReason::ConnectionLost);
    }

    #[tokio::test]
    async fn test_supervisor_handles_conflict_disconnect_under_load() {
        let (runtime, _tmp) = test_runtime();

        let actors = ActorsConfig {
            enabled: true,
            router_mailbox: 256,
            session_mailbox: 32,
            max_active_sessions: 16,
            session_idle_ttl_secs: 1800,
            busy_retry_after_secs: 5,
            dedupe_ttl_secs: 600,
            dead_letter_path: PathBuf::from("data/dead_letters.jsonl"),
            tooling: ActorToolingConfig::default(),
            supervision: ActorSupervisionConfig::default(),
            memory: ActorMemoryConfig::default(),
            observability: ActorObservabilityConfig::default(),
        };

        let keepalive_disabled = KeepaliveConfig {
            enabled: false,
            ping_interval_secs: 60,
            read_timeout_secs: 300,
        };

        let supervisor = ActorSupervisor::new(
            actors,
            keepalive_disabled,
            supervisor_dependencies(&runtime),
        );
        let (event_tx, event_rx) = mpsc::channel(512);
        let (cmd_tx, mut cmd_rx) = mpsc::channel(2048);
        let handle = tokio::spawn(async move { supervisor.run(event_rx, cmd_tx).await });

        // Drain outbound traffic so egress backpressure does not mask disconnect handling.
        let drain_handle = tokio::spawn(async move { while cmd_rx.recv().await.is_some() {} });

        for i in 0..200 {
            let from = format!("user{}@localhost/res", i % 10);
            let event = XmppEvent::Message(IncomingMessage {
                from,
                to: "bot@localhost".to_string(),
                body: "/ping".to_string(),
                id: Some(format!("load-{i}")),
                message_type: MessageType::Chat,
                oob: vec![],
            });
            event_tx.send(event).await.unwrap();
        }

        event_tx
            .send(XmppEvent::StreamError("conflict".to_string()))
            .await
            .unwrap();

        let reason = tokio::time::timeout(Duration::from_secs(3), handle)
            .await
            .expect("supervisor run timed out under load")
            .unwrap()
            .unwrap();
        assert_eq!(reason, DisconnectReason::Conflict);

        drop(event_tx);
        drain_handle.abort();
        let _ = drain_handle.await;
    }

    #[tokio::test]
    async fn test_supervisor_slash_command_fixture() {
        let (runtime, tmp) = test_runtime_with_allowed_jids(vec!["*".to_string()]);
        let (reason, commands) = run_actor_fixture_with_runtime(
            &runtime,
            tmp.path().join("dead_letters-slash-command.jsonl"),
            parity_fixture_events(),
            2,
        )
        .await;

        assert_eq!(reason, DisconnectReason::Conflict);
        assert!(
            matches!(
                commands.as_slice(),
                [
                    NormalizedCommand::SendMessage { to: to_1, body: body_1 },
                    NormalizedCommand::SendMessage { to: to_2, body: body_2 }
                ] if to_1 == "alice@localhost/phone"
                    && body_1 == "pong"
                    && to_2 == "alice@localhost/phone"
                    && body_2.contains("Commands:")
            ),
            "slash-command: expected /ping then /help responses"
        );
    }

    #[tokio::test]
    async fn test_supervisor_presence_subscribe_fixture() {
        let (runtime, tmp) = test_runtime_with_allowed_jids(vec!["*".to_string()]);
        let (reason, commands) = run_actor_fixture_with_runtime(
            &runtime,
            tmp.path().join("dead_letters-presence-subscribe.jsonl"),
            vec![
                XmppEvent::Connected,
                presence_subscribe_event("alice@localhost/phone"),
            ],
            1,
        )
        .await;

        assert_eq!(reason, DisconnectReason::Conflict);
        assert!(
            matches!(commands.as_slice(), [NormalizedCommand::SendRaw(raw)] if raw.contains("type='subscribed'")),
            "presence-subscribe: expected one subscribed raw stanza"
        );
    }

    #[tokio::test]
    async fn test_supervisor_read_timeout_probe_fixture() {
        let (runtime, tmp) = test_runtime_with_allowed_jids(vec!["*".to_string()]);
        let (reason, commands) = run_actor_fixture_with_runtime(
            &runtime,
            tmp.path().join("dead_letters-read-timeout.jsonl"),
            vec![XmppEvent::Connected, XmppEvent::ReadTimeout],
            1,
        )
        .await;

        assert_eq!(reason, DisconnectReason::Conflict);
        assert!(
            matches!(commands.as_slice(), [NormalizedCommand::Ping]),
            "read-timeout-probe: expected one ping probe command"
        );
    }

    #[tokio::test]
    async fn test_supervisor_unauthorized_jid_fixture() {
        let (runtime, tmp) = test_runtime_with_allowed_jids(vec!["alice@localhost".to_string()]);
        let (reason, commands) = run_actor_fixture_with_runtime(
            &runtime,
            tmp.path().join("dead_letters-unauthorized.jsonl"),
            vec![
                XmppEvent::Connected,
                chat_event("bob@localhost/laptop", "unauth-1", "/ping"),
            ],
            0,
        )
        .await;

        assert_eq!(reason, DisconnectReason::Conflict);
        assert!(
            commands.is_empty(),
            "unauthorized-jid: expected no outbound commands"
        );
    }

    #[tokio::test]
    async fn test_supervisor_cross_domain_fixture() {
        let (runtime, tmp) = test_runtime_with_allowed_jids(vec!["*".to_string()]);
        let (reason, commands) = run_actor_fixture_with_runtime(
            &runtime,
            tmp.path().join("dead_letters-cross-domain.jsonl"),
            vec![
                XmppEvent::Connected,
                chat_event("mallory@evil.com/pc", "cross-1", "/ping"),
            ],
            0,
        )
        .await;

        assert_eq!(reason, DisconnectReason::Conflict);
        assert!(
            commands.is_empty(),
            "cross-domain: expected no outbound commands"
        );
    }

    #[tokio::test]
    async fn test_supervisor_non_command_chat_with_deterministic_llm() {
        let llm: Arc<dyn LlmClient> = Arc::new(DeterministicLlm::new("stubbed chat reply"));
        let (actor_runtime, actor_tmp) = test_runtime_with_llm(vec!["*".to_string()], llm);
        let fixture = || {
            vec![
                XmppEvent::Connected,
                chat_event("alice@localhost/phone", "chat-1", "hello from fixture"),
            ]
        };

        let (actor_reason, actor_commands) = run_actor_fixture_with_runtime(
            &actor_runtime,
            actor_tmp.path().join("dead_letters-chat.jsonl"),
            fixture(),
            2,
        )
        .await;

        assert_eq!(actor_reason, DisconnectReason::Conflict);
        assert_eq!(
            actor_commands,
            vec![
                NormalizedCommand::SendChatState {
                    to: "alice@localhost/phone".to_string(),
                    state: "composing".to_string(),
                    msg_type: "chat".to_string(),
                },
                NormalizedCommand::SendMessage {
                    to: "alice@localhost/phone".to_string(),
                    body: "stubbed chat reply".to_string(),
                },
            ]
        );
    }

    #[tokio::test]
    async fn test_supervisor_non_command_chat_with_sharded_memory_writers() {
        let llm: Arc<dyn LlmClient> = Arc::new(DeterministicLlm::new("stubbed chat reply"));
        let mut actor_memory = ActorMemoryConfig::default();
        actor_memory.writer_shards = 4;
        let (actor_runtime, actor_tmp) =
            test_runtime_with_llm_and_actor_memory(vec!["*".to_string()], llm, actor_memory);
        let fixture = || {
            vec![
                XmppEvent::Connected,
                chat_event(
                    "alice@localhost/phone",
                    "chat-sharded-1",
                    "hello from fixture",
                ),
            ]
        };

        let (actor_reason, actor_commands) = run_actor_fixture_with_runtime(
            &actor_runtime,
            actor_tmp.path().join("dead_letters-chat-sharded.jsonl"),
            fixture(),
            2,
        )
        .await;

        assert_eq!(actor_reason, DisconnectReason::Conflict);
        assert_eq!(
            actor_commands,
            vec![
                NormalizedCommand::SendChatState {
                    to: "alice@localhost/phone".to_string(),
                    state: "composing".to_string(),
                    msg_type: "chat".to_string(),
                },
                NormalizedCommand::SendMessage {
                    to: "alice@localhost/phone".to_string(),
                    body: "stubbed chat reply".to_string(),
                },
            ]
        );
    }

    #[tokio::test]
    async fn test_supervisor_non_command_muc_chat_with_deterministic_llm() {
        let llm: Arc<dyn LlmClient> = Arc::new(DeterministicLlm::new("stubbed muc reply"));
        let rooms = vec![RoomConfig {
            jid: "lobby@conference.localhost".to_string(),
            nick: "bot".to_string(),
        }];
        let (actor_runtime, actor_tmp) =
            test_runtime_with_llm_and_rooms(vec!["*".to_string()], rooms, llm);
        let fixture = || {
            vec![groupchat_event(
                "lobby@conference.localhost/alice",
                "muc-1",
                "@bot hello room",
            )]
        };

        let (actor_reason, actor_commands) = run_actor_fixture_with_runtime(
            &actor_runtime,
            actor_tmp.path().join("dead_letters-muc.jsonl"),
            fixture(),
            2,
        )
        .await;

        assert_eq!(actor_reason, DisconnectReason::Conflict);
        assert_eq!(
            actor_commands,
            vec![
                NormalizedCommand::SendChatState {
                    to: "lobby@conference.localhost".to_string(),
                    state: "composing".to_string(),
                    msg_type: "groupchat".to_string(),
                },
                NormalizedCommand::SendMucMessage {
                    to: "lobby@conference.localhost".to_string(),
                    body: "stubbed muc reply".to_string(),
                },
            ]
        );
    }

    #[tokio::test]
    async fn test_supervisor_attachment_chat_with_deterministic_llm() {
        let attachment_url = "https://upload.localhost/fixture.pdf";
        let mock_downloader: Arc<dyn AttachmentDownloader> =
            Arc::new(MockAttachmentDownloader::new_pdf_fixture(
                "fixture.pdf",
                b"%PDF-1.4 test fixture".to_vec(),
            ));

        let llm: Arc<dyn LlmClient> = Arc::new(DeterministicLlm::new("stubbed attachment reply"));
        let (actor_runtime, actor_tmp) = test_runtime_with_llm_and_rooms_and_downloader(
            vec!["*".to_string()],
            vec![],
            llm,
            mock_downloader,
        );
        let fixture = || {
            vec![chat_event_with_oob(
                "alice@localhost/phone",
                "attach-1",
                "please review this file",
                attachment_url,
            )]
        };

        let (actor_reason, actor_commands) = run_actor_fixture_with_runtime(
            &actor_runtime,
            actor_tmp.path().join("dead_letters-attachment.jsonl"),
            fixture(),
            2,
        )
        .await;

        assert_eq!(actor_reason, DisconnectReason::Conflict);
        assert_eq!(
            actor_commands,
            vec![
                NormalizedCommand::SendChatState {
                    to: "alice@localhost/phone".to_string(),
                    state: "composing".to_string(),
                    msg_type: "chat".to_string(),
                },
                NormalizedCommand::SendMessage {
                    to: "alice@localhost/phone".to_string(),
                    body: "stubbed attachment reply".to_string(),
                },
            ]
        );
    }

    #[tokio::test]
    async fn test_supervisor_tool_success_chat_fixture() {
        let tooling = ActorToolingConfig::default();
        let actor_llm: Arc<dyn LlmClient> = Arc::new(ToolProbeLlm::new(
            "fixture_fast_skill",
            serde_json::json!({"query": "success"}),
        ));
        let actor_downloader: Arc<dyn AttachmentDownloader> = Arc::new(FileDownloader::new(3));

        let (actor_runtime, actor_tmp) =
            test_runtime_with_llm_and_rooms_and_downloader_and_skills_and_tooling(
                vec!["*".to_string()],
                vec![],
                actor_llm,
                actor_downloader,
                fixture_fast_skills(),
                tooling,
            );

        assert_tool_chat_fixture_actor_only(
            "tool-success-chat",
            &actor_runtime,
            actor_tmp
                .path()
                .join("dead_letters-parity-tool-success.jsonl"),
            "tool-success-1",
            "please use your tool",
            "tool-result: fixture-fast-ok",
        )
        .await;
    }

    #[tokio::test]
    async fn test_supervisor_tool_timeout_chat_fixture() {
        let tooling = ActorToolingConfig {
            max_tool_rounds: 10,
            skill_timeout_secs: 1,
            max_parallel_skills: 32,
            skill_queue_timeout_ms: 5000,
            allowed_capabilities: vec!["*".to_string()],
        };
        let actor_llm: Arc<dyn LlmClient> = Arc::new(ToolProbeLlm::new(
            "fixture_slow_skill",
            serde_json::json!({"query": "timeout"}),
        ));
        let actor_downloader: Arc<dyn AttachmentDownloader> = Arc::new(FileDownloader::new(3));

        let (actor_runtime, actor_tmp) =
            test_runtime_with_llm_and_rooms_and_downloader_and_skills_and_tooling(
                vec!["*".to_string()],
                vec![],
                actor_llm,
                actor_downloader,
                fixture_slow_skills(),
                tooling,
            );

        assert_tool_chat_fixture_actor_only(
            "tool-timeout-chat",
            &actor_runtime,
            actor_tmp
                .path()
                .join("dead_letters-parity-tool-timeout.jsonl"),
            "tool-timeout-1",
            "please use your slow tool",
            "tool-result: Error: timeout",
        )
        .await;
    }

    #[tokio::test]
    async fn test_supervisor_tool_policy_denied_chat_fixture() {
        let tooling = ActorToolingConfig {
            max_tool_rounds: 10,
            skill_timeout_secs: 30,
            max_parallel_skills: 32,
            skill_queue_timeout_ms: 5000,
            allowed_capabilities: vec!["filesystem:*".to_string()],
        };
        let actor_llm: Arc<dyn LlmClient> = Arc::new(ToolProbeLlm::new(
            "fixture_fast_skill",
            serde_json::json!({"query": "policy"}),
        ));
        let actor_downloader: Arc<dyn AttachmentDownloader> = Arc::new(FileDownloader::new(3));

        let (actor_runtime, actor_tmp) =
            test_runtime_with_llm_and_rooms_and_downloader_and_skills_and_tooling(
                vec!["*".to_string()],
                vec![],
                actor_llm,
                actor_downloader,
                fixture_fast_skills(),
                tooling,
            );

        assert_tool_chat_fixture_actor_only(
            "tool-policy-denied-chat",
            &actor_runtime,
            actor_tmp
                .path()
                .join("dead_letters-parity-tool-policy-denied.jsonl"),
            "tool-policy-1",
            "please use restricted tool",
            "tool-result: Error: policy_denied (missing capability 'network:api.example.com:443')",
        )
        .await;
    }
}
