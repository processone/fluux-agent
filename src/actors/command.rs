use std::time::Duration;

use anyhow::{anyhow, Result};
use tracing::info;

use super::dead_letter::{
    DeadLetterMemoryReplayPayload, DeadLetterRecord, DeadLetterService,
    DeadLetterSkillReplayPayload, DeadLetterSource,
};
use super::message::CommandReply;
use super::skill_router::SkillRouterActor;
use crate::agent::memory::Memory;
use crate::config::Config;
use crate::skills::SkillRegistry;
use crate::xmpp::component::XmppCommand;
use crate::xmpp::stanzas;

#[derive(Default)]
pub struct CommandActor;

impl CommandActor {
    pub fn new() -> Self {
        Self
    }

    pub async fn handle(
        &self,
        from: &str,
        body: &str,
        config: &Config,
        memory: &Memory,
        dead_letter: &DeadLetterService,
        skills: &SkillRegistry,
        llm_description: &str,
        skill_names: &[&str],
        uptime: Duration,
    ) -> Result<CommandReply> {
        let bare_jid = stanzas::bare_jid(from);
        let parts: Vec<&str> = body.splitn(2, ' ').collect();
        let command = parts[0].to_lowercase();

        info!("Slash command from {bare_jid}: {command}");

        match command.as_str() {
            "/new" | "/reset" => self
                .cmd_new_session(memory, bare_jid)
                .map(CommandReply::text_only),
            "/forget" => self
                .cmd_forget(memory, bare_jid)
                .map(CommandReply::text_only),
            "/status" => self
                .cmd_status(
                    bare_jid,
                    config,
                    memory,
                    llm_description,
                    skill_names,
                    uptime,
                )
                .map(CommandReply::text_only),
            "/deadletters" => {
                self.cmd_deadletters(parts.get(1).copied(), config, memory, dead_letter, skills)
                    .await
            }
            "/help" => Ok(CommandReply::text_only(self.cmd_help())),
            "/ping" => Ok(CommandReply::text_only("pong".to_string())),
            _ => Ok(CommandReply::text_only(format!(
                "Unknown command: {command}\nType /help for available commands."
            ))),
        }
    }

    fn cmd_new_session(&self, memory: &Memory, bare_jid: &str) -> Result<String> {
        memory.new_session(bare_jid)
    }

    fn cmd_forget(&self, memory: &Memory, bare_jid: &str) -> Result<String> {
        memory.forget(bare_jid)
    }

    #[allow(clippy::too_many_arguments)]
    fn cmd_status(
        &self,
        bare_jid: &str,
        config: &Config,
        memory: &Memory,
        llm_description: &str,
        skill_names: &[&str],
        uptime: Duration,
    ) -> Result<String> {
        let hours = uptime.as_secs() / 3600;
        let minutes = (uptime.as_secs() % 3600) / 60;

        let msg_count = memory.message_count(bare_jid)?;
        let session_count = memory.session_count(bare_jid)?;

        let yn = |b: bool| if b { "yes" } else { "none" };

        // Workspace file presence — contextual (checks per-JID overrides)
        let workspace = memory.get_workspace_context(bare_jid)?;
        let has_instructions = workspace.instructions.is_some();
        let has_identity = workspace.identity.is_some();
        let has_personality = workspace.personality.is_some();

        let is_room = config.find_room(bare_jid).is_some();

        let file_count = memory.file_count(bare_jid)?;
        let file_info = if file_count > 0 {
            format!("\nFiles: {file_count}")
        } else {
            String::new()
        };

        let knowledge_count = memory.knowledge_count(bare_jid)?;
        let knowledge_info = if knowledge_count > 0 {
            format!("\nKnowledge entries: {knowledge_count}")
        } else {
            String::new()
        };

        // Context-specific section: room info vs. user info
        let context_info = if is_room {
            format!(
                "Room: {bare_jid}\n\
                 Room messages: {msg_count}\n\
                 Archived sessions: {session_count}{file_info}{knowledge_info}"
            )
        } else {
            let has_profile = memory.has_user_profile(bare_jid)?;
            let has_memory = memory.get_user_memory(bare_jid)?.is_some();
            format!(
                "Your session: {msg_count} messages\n\
                 Archived sessions: {session_count}{file_info}{knowledge_info}\n\
                 User profile: {}\n\
                 User memory: {}",
                yn(has_profile),
                yn(has_memory),
            )
        };

        // Domain security info
        let domain_info = if config.agent.allowed_domains.is_empty() {
            format!("Allowed domains: {} (default)", config.server.domain())
        } else if config.agent.allowed_domains.iter().any(|d| d == "*") {
            "Allowed domains: * (all)".to_string()
        } else {
            format!(
                "Allowed domains: {}",
                config.agent.allowed_domains.join(", ")
            )
        };

        let skills_info = if skill_names.is_empty() {
            "Skills: none".to_string()
        } else {
            format!("Skills: {}", skill_names.join(", "))
        };

        let keepalive_info = if config.keepalive.enabled {
            format!(
                "Keepalive: ping {}s / timeout {}s",
                config.keepalive.ping_interval_secs, config.keepalive.read_timeout_secs,
            )
        } else {
            "Keepalive: disabled".to_string()
        };

        let session_timeout_info = if config.session.idle_timeout_mins > 0 {
            format!(
                "Session timeout: {}m idle",
                config.session.idle_timeout_mins,
            )
        } else {
            "Session timeout: disabled".to_string()
        };

        Ok(format!(
            "{} — status\n\
             Uptime: {hours}h {minutes}m\n\
             Mode: {}\n\
             LLM: {}\n\
             {skills_info}\n\
             {keepalive_info}\n\
             {session_timeout_info}\n\
             {context_info}\n\
             Workspace: instructions={}, identity={}, personality={}\n\
             {domain_info}",
            config.agent.name,
            config.server.mode_description(),
            llm_description,
            yn(has_instructions),
            yn(has_identity),
            yn(has_personality),
        ))
    }

    async fn cmd_deadletters(
        &self,
        args: Option<&str>,
        config: &Config,
        memory: &Memory,
        dead_letter: &DeadLetterService,
        skills: &SkillRegistry,
    ) -> Result<CommandReply> {
        if let Some(raw_args) = args.map(str::trim) {
            if let Some(cid) = raw_args.strip_prefix("replay ") {
                return self
                    .cmd_deadletters_replay(cid.trim(), config, memory, dead_letter, skills)
                    .await;
            }
        }

        let count = parse_deadletter_count(args)?;
        let records = dead_letter.replay().await?;
        if records.is_empty() {
            return Ok(CommandReply::text_only("Dead letters: none".to_string()));
        }

        let total = records.len();
        let mut lines = Vec::new();
        lines.push(format!(
            "Dead letters: showing latest {} / {}",
            count.min(total),
            total
        ));
        for record in records.iter().rev().take(count) {
            lines.push(format_deadletter_line(record));
        }
        Ok(CommandReply::text_only(lines.join("\n")))
    }

    async fn cmd_deadletters_replay(
        &self,
        correlation_id: &str,
        config: &Config,
        memory: &Memory,
        dead_letter: &DeadLetterService,
        skills: &SkillRegistry,
    ) -> Result<CommandReply> {
        if correlation_id.is_empty() {
            return Err(anyhow!("Usage: /deadletters replay <correlation_id>"));
        }

        let records = dead_letter.replay().await?;
        let Some(record) = records
            .iter()
            .rev()
            .find(|entry| entry.correlation_id == correlation_id)
        else {
            return Ok(CommandReply::text_only(format!(
                "Dead-letter not found for correlation_id={correlation_id}"
            )));
        };

        let Some(payload) = record.payload.clone() else {
            return Ok(CommandReply::text_only(format!(
                "Dead-letter {correlation_id} has no replay payload (legacy record)"
            )));
        };

        match record.source {
            DeadLetterSource::XmppEgress => {
                let command: XmppCommand = serde_json::from_value(payload).map_err(|err| {
                    anyhow!("Dead-letter payload decode failed for {correlation_id}: {err}")
                })?;
                Ok(CommandReply {
                    text: format!(
                        "Replay dispatched for correlation_id={correlation_id} (reason={})",
                        record.reason
                    ),
                    replay_commands: vec![command],
                })
            }
            DeadLetterSource::MemoryActor => {
                let replay_payload: DeadLetterMemoryReplayPayload = serde_json::from_value(payload)
                    .map_err(|err| {
                        anyhow!("Dead-letter payload decode failed for {correlation_id}: {err}")
                    })?;
                self.replay_memory(correlation_id, memory, replay_payload)
            }
            DeadLetterSource::SkillRouter => {
                let replay_payload: DeadLetterSkillReplayPayload = serde_json::from_value(payload)
                    .map_err(|err| {
                        anyhow!("Dead-letter payload decode failed for {correlation_id}: {err}")
                    })?;
                self.replay_skill(correlation_id, config, dead_letter, skills, replay_payload)
                    .await
            }
            other => Ok(CommandReply::text_only(format!(
                "Replay not supported for source {:?} (correlation_id={correlation_id})",
                other
            ))),
        }
    }

    fn replay_memory(
        &self,
        correlation_id: &str,
        memory: &Memory,
        replay_payload: DeadLetterMemoryReplayPayload,
    ) -> Result<CommandReply> {
        match replay_payload {
            DeadLetterMemoryReplayPayload::StoreMessageStructured {
                jid,
                role,
                content,
                msg_id,
                sender,
            } => {
                memory.store_message_structured(
                    &jid,
                    &role,
                    &content,
                    msg_id.as_deref(),
                    sender.as_deref(),
                )?;
                Ok(CommandReply::text_only(format!(
                    "Replay applied for correlation_id={correlation_id} (memory store_message_structured)"
                )))
            }
            DeadLetterMemoryReplayPayload::StoreMessageFull {
                jid,
                role,
                content,
                msg_id,
                sender,
                attachments,
                reaction,
            } => {
                memory.store_message_full(
                    &jid,
                    &role,
                    &content,
                    msg_id.as_deref(),
                    sender.as_deref(),
                    attachments,
                    reaction,
                )?;
                Ok(CommandReply::text_only(format!(
                    "Replay applied for correlation_id={correlation_id} (memory store_message_full)"
                )))
            }
        }
    }

    async fn replay_skill(
        &self,
        correlation_id: &str,
        config: &Config,
        dead_letter: &DeadLetterService,
        skills: &SkillRegistry,
        replay_payload: DeadLetterSkillReplayPayload,
    ) -> Result<CommandReply> {
        let skill_name = replay_payload.skill_name.clone();
        let router = SkillRouterActor::new(
            skills,
            &config.actors.tooling,
            dead_letter.path().to_path_buf(),
        );
        let result = router
            .execute(
                &replay_payload.skill_name,
                replay_payload.params,
                &replay_payload.context,
            )
            .await;
        Ok(CommandReply::text_only(format!(
            "Replay executed for correlation_id={correlation_id} (skill={skill_name}): {}",
            result.content
        )))
    }

    fn cmd_help(&self) -> String {
        "\
Commands:\n\
  /new     — Start a new conversation (archive current session)\n\
  /forget  — Erase your history, profile, and memory\n\
  /status  — Agent info, uptime, session stats\n\
  /deadletters [N] — Show latest dead-letter records (default: 10)\n\
  /deadletters replay <correlation_id> — Replay a dead-letter when payload is available\n\
  /ping    — Check if the agent is alive\n\
  /help    — This message"
            .to_string()
    }
}

fn parse_deadletter_count(args: Option<&str>) -> Result<usize> {
    const DEFAULT_COUNT: usize = 10;
    const MAX_COUNT: usize = 50;
    let Some(raw) = args.map(str::trim) else {
        return Ok(DEFAULT_COUNT);
    };
    if raw.is_empty() {
        return Ok(DEFAULT_COUNT);
    }
    if raw.starts_with("replay ") {
        return Ok(DEFAULT_COUNT);
    }

    let parsed = raw
        .parse::<usize>()
        .map_err(|_| anyhow!("Usage: /deadletters [N] (N must be a positive integer)"))?;
    Ok(parsed.clamp(1, MAX_COUNT))
}

fn format_deadletter_line(record: &DeadLetterRecord) -> String {
    let source = format!("{:?}", record.source);
    let conversation_id = record.conversation_id.as_deref().unwrap_or("-");
    format!(
        "- [{}] reason={} corr={} conv={} kind={}",
        source, record.reason, record.correlation_id, conversation_id, record.payload_kind
    )
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use async_trait::async_trait;
    use serde_json::json;
    use tempfile::TempDir;

    use super::*;
    use crate::config::{
        ActorMemoryConfig, ActorObservabilityConfig, ActorSupervisionConfig, ActorToolingConfig,
        ActorsConfig, AgentConfig, ConnectionMode, KeepaliveConfig, LlmConfig, MemoryConfig,
        RoomConfig, ServerConfig, SessionConfig, SkillsConfig,
    };
    use crate::skills::{Skill, SkillContext};

    struct EchoSkill;

    #[async_trait]
    impl Skill for EchoSkill {
        fn name(&self) -> &str {
            "echo"
        }

        fn description(&self) -> &str {
            "Echoes query input"
        }

        fn parameters_schema(&self) -> serde_json::Value {
            json!({
                "type": "object",
                "properties": {
                    "query": { "type": "string" }
                },
                "required": ["query"]
            })
        }

        async fn execute(
            &self,
            params: serde_json::Value,
            _context: &SkillContext,
        ) -> anyhow::Result<String> {
            let query = params
                .get("query")
                .and_then(|value| value.as_str())
                .unwrap_or("");
            Ok(format!("echo:{query}"))
        }
    }

    fn test_config() -> Config {
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
                allowed_jids: vec!["*".to_string()],
                allowed_domains: vec![],
            },
            memory: MemoryConfig {
                backend: "markdown".to_string(),
                path: PathBuf::new(),
            },
            rooms: vec![RoomConfig {
                jid: "lobby@conference.localhost".to_string(),
                nick: "bot".to_string(),
            }],
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

    #[tokio::test]
    async fn test_command_actor_ping() {
        let actor = CommandActor::new();
        let config = test_config();
        let skills = SkillRegistry::new();
        let tmp = TempDir::new().unwrap();
        let memory = Memory::open(tmp.path()).unwrap();
        let dead_letter = DeadLetterService::new(tmp.path().join("dead_letters.jsonl"));
        let output = actor
            .handle(
                "admin@localhost/res",
                "/ping",
                &config,
                &memory,
                &dead_letter,
                &skills,
                "stub-llm",
                &[],
                Duration::from_secs(75),
            )
            .await
            .unwrap();
        assert_eq!(output.text, "pong");
        assert!(output.replay_commands.is_empty());
    }

    #[tokio::test]
    async fn test_command_actor_status_room_context() {
        let actor = CommandActor::new();
        let config = test_config();
        let skills = SkillRegistry::new();
        let tmp = TempDir::new().unwrap();
        let memory = Memory::open(tmp.path()).unwrap();
        let dead_letter = DeadLetterService::new(tmp.path().join("dead_letters.jsonl"));
        let output = actor
            .handle(
                "lobby@conference.localhost/alice",
                "/status",
                &config,
                &memory,
                &dead_letter,
                &skills,
                "stub-llm",
                &[],
                Duration::from_secs(75),
            )
            .await
            .unwrap();
        assert!(output.text.contains("Room: lobby@conference.localhost"));
        assert!(output.text.contains("LLM: stub-llm"));
        assert!(output.replay_commands.is_empty());
    }

    #[tokio::test]
    async fn test_command_actor_deadletters_returns_latest_records() {
        let actor = CommandActor::new();
        let config = test_config();
        let skills = SkillRegistry::new();
        let tmp = TempDir::new().unwrap();
        let memory = Memory::open(tmp.path()).unwrap();
        let dead_letter = DeadLetterService::new(tmp.path().join("dead_letters.jsonl"));

        dead_letter
            .record(DeadLetterRecord::router(
                "session_mailbox_full",
                "alice@localhost".to_string(),
                "corr-1".to_string(),
                1,
                "message",
                "sent",
            ))
            .await
            .unwrap();
        dead_letter
            .record(DeadLetterRecord::xmpp_egress(
                "transport_closed",
                "corr-2".to_string(),
                "send_message",
                Some("alice@localhost/mobile".to_string()),
                1,
            ))
            .await
            .unwrap();

        let output = actor
            .handle(
                "admin@localhost/res",
                "/deadletters 1",
                &config,
                &memory,
                &dead_letter,
                &skills,
                "stub-llm",
                &[],
                Duration::from_secs(75),
            )
            .await
            .unwrap();

        assert!(output.text.contains("Dead letters: showing latest 1 / 2"));
        assert!(output.text.contains("corr-2"));
        assert!(!output.text.contains("corr-1"));
        assert!(output.replay_commands.is_empty());
    }

    #[tokio::test]
    async fn test_command_actor_deadletters_replay_returns_command() {
        let actor = CommandActor::new();
        let config = test_config();
        let skills = SkillRegistry::new();
        let tmp = TempDir::new().unwrap();
        let memory = Memory::open(tmp.path()).unwrap();
        let dead_letter = DeadLetterService::new(tmp.path().join("dead_letters.jsonl"));
        let expected = XmppCommand::Ping;

        dead_letter
            .record(DeadLetterRecord::xmpp_egress_with_payload(
                "send_timeout_exhausted",
                "corr-replay".to_string(),
                "ping",
                None,
                3,
                serde_json::to_value(&expected).ok(),
            ))
            .await
            .unwrap();

        let output = actor
            .handle(
                "admin@localhost/res",
                "/deadletters replay corr-replay",
                &config,
                &memory,
                &dead_letter,
                &skills,
                "stub-llm",
                &[],
                Duration::from_secs(75),
            )
            .await
            .unwrap();

        assert!(output
            .text
            .contains("Replay dispatched for correlation_id=corr-replay"));
        assert_eq!(output.replay_commands.len(), 1);
        assert!(matches!(output.replay_commands[0], XmppCommand::Ping));
    }

    #[tokio::test]
    async fn test_command_actor_deadletters_replay_legacy_record_reports_not_replayable() {
        let actor = CommandActor::new();
        let config = test_config();
        let skills = SkillRegistry::new();
        let tmp = TempDir::new().unwrap();
        let memory = Memory::open(tmp.path()).unwrap();
        let dead_letter = DeadLetterService::new(tmp.path().join("dead_letters.jsonl"));

        dead_letter
            .record(DeadLetterRecord::xmpp_egress(
                "transport_closed",
                "corr-legacy".to_string(),
                "send_message",
                Some("alice@localhost/mobile".to_string()),
                1,
            ))
            .await
            .unwrap();

        let output = actor
            .handle(
                "admin@localhost/res",
                "/deadletters replay corr-legacy",
                &config,
                &memory,
                &dead_letter,
                &skills,
                "stub-llm",
                &[],
                Duration::from_secs(75),
            )
            .await
            .unwrap();

        assert!(output
            .text
            .contains("Dead-letter corr-legacy has no replay payload (legacy record)"));
        assert!(output.replay_commands.is_empty());
    }

    #[tokio::test]
    async fn test_command_actor_deadletters_replay_uses_latest_record_for_same_correlation_id() {
        let actor = CommandActor::new();
        let config = test_config();
        let skills = SkillRegistry::new();
        let tmp = TempDir::new().unwrap();
        let memory = Memory::open(tmp.path()).unwrap();
        let dead_letter = DeadLetterService::new(tmp.path().join("dead_letters.jsonl"));

        dead_letter
            .record(DeadLetterRecord::xmpp_egress(
                "transport_closed",
                "corr-same".to_string(),
                "send_message",
                Some("alice@localhost/mobile".to_string()),
                1,
            ))
            .await
            .unwrap();
        dead_letter
            .record(DeadLetterRecord::xmpp_egress_with_payload(
                "send_timeout_exhausted",
                "corr-same".to_string(),
                "ping",
                None,
                3,
                serde_json::to_value(XmppCommand::Ping).ok(),
            ))
            .await
            .unwrap();

        let output = actor
            .handle(
                "admin@localhost/res",
                "/deadletters replay corr-same",
                &config,
                &memory,
                &dead_letter,
                &skills,
                "stub-llm",
                &[],
                Duration::from_secs(75),
            )
            .await
            .unwrap();

        assert!(output
            .text
            .contains("Replay dispatched for correlation_id=corr-same"));
        assert_eq!(output.replay_commands.len(), 1);
        assert!(matches!(output.replay_commands[0], XmppCommand::Ping));
    }

    #[tokio::test]
    async fn test_command_actor_deadletters_replay_replays_memory_actor_payload() {
        let actor = CommandActor::new();
        let config = test_config();
        let skills = SkillRegistry::new();
        let tmp = TempDir::new().unwrap();
        let memory = Memory::open(tmp.path()).unwrap();
        let dead_letter = DeadLetterService::new(tmp.path().join("dead_letters.jsonl"));

        dead_letter
            .record(DeadLetterRecord::memory_actor_with_payload(
                "memory_enqueue_timeout",
                "alice@localhost".to_string(),
                "corr-memory".to_string(),
                "store_message_structured",
                serde_json::to_value(DeadLetterMemoryReplayPayload::StoreMessageStructured {
                    jid: "alice@localhost".to_string(),
                    role: "user".to_string(),
                    content: "hello-memory-replay".to_string(),
                    msg_id: Some("m-memory-1".to_string()),
                    sender: Some("alice@localhost".to_string()),
                })
                .ok(),
            ))
            .await
            .unwrap();

        let output = actor
            .handle(
                "admin@localhost/res",
                "/deadletters replay corr-memory",
                &config,
                &memory,
                &dead_letter,
                &skills,
                "stub-llm",
                &[],
                Duration::from_secs(75),
            )
            .await
            .unwrap();

        assert!(output
            .text
            .contains("Replay applied for correlation_id=corr-memory"));
        assert!(output.replay_commands.is_empty());

        let history = memory.get_history("alice@localhost", 10).unwrap();
        assert_eq!(history.len(), 1);
        assert_eq!(history[0].role, "user");
        assert_eq!(history[0].content, "hello-memory-replay");
    }

    #[tokio::test]
    async fn test_command_actor_deadletters_replay_replays_skill_router_payload() {
        let actor = CommandActor::new();
        let config = test_config();
        let mut skills = SkillRegistry::new();
        skills.register(Box::new(EchoSkill));
        let tmp = TempDir::new().unwrap();
        let memory = Memory::open(tmp.path()).unwrap();
        let dead_letter = DeadLetterService::new(tmp.path().join("dead_letters.jsonl"));

        dead_letter
            .record(DeadLetterRecord::skill_router_with_payload(
                "skill_queue_timeout",
                "alice@localhost".to_string(),
                "corr-skill".to_string(),
                "echo",
                serde_json::to_value(DeadLetterSkillReplayPayload {
                    skill_name: "echo".to_string(),
                    params: json!({ "query": "hello-skill-replay" }),
                    context: SkillContext {
                        jid: "alice@localhost".to_string(),
                        base_path: tmp.path().to_path_buf(),
                    },
                })
                .ok(),
            ))
            .await
            .unwrap();

        let output = actor
            .handle(
                "admin@localhost/res",
                "/deadletters replay corr-skill",
                &config,
                &memory,
                &dead_letter,
                &skills,
                "stub-llm",
                &[],
                Duration::from_secs(75),
            )
            .await
            .unwrap();

        assert!(output
            .text
            .contains("Replay executed for correlation_id=corr-skill"));
        assert!(output.text.contains("skill=echo"));
        assert!(output.text.contains("echo:hello-skill-replay"));
        assert!(output.replay_commands.is_empty());
    }
}
