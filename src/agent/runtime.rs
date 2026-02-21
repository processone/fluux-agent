use std::sync::Arc;

use crate::actors::dead_letter::DeadLetterService;
#[cfg(test)]
use crate::actors::inference::build_system_prompt_static;
use crate::actors::memory_actor::{MemoryActor, SessionMemoryWriter, ShardedMemoryWriter};
use crate::actors::session_responder::SessionResponderActor;
use crate::agent::files::AttachmentDownloader;
use crate::config::Config;
use crate::llm::LlmClient;
#[cfg(test)]
use crate::llm::{Message, MessageContent};

use crate::skills::SkillRegistry;

use super::memory::Memory;
#[cfg(test)]
use super::memory::WorkspaceContext;

pub struct ActorRuntimeDependencies {
    pub config: Arc<Config>,
    pub dead_letter_service: Arc<DeadLetterService>,
    pub memory_writer: Arc<dyn SessionMemoryWriter>,
    pub session_responder: Arc<SessionResponderActor>,
}

fn build_runtime_components(
    config: &Config,
    llm: Arc<dyn LlmClient>,
    memory: Arc<Memory>,
    file_downloader: Arc<dyn AttachmentDownloader>,
    skills: SkillRegistry,
) -> (
    Arc<DeadLetterService>,
    Arc<dyn SessionMemoryWriter>,
    Arc<SessionResponderActor>,
) {
    let start_time = std::time::Instant::now();
    let dead_letter_service = Arc::new(DeadLetterService::new(
        config.actors.dead_letter_path.clone(),
    ));
    let writer_shards = config.actors.memory.writer_shards.max(1);
    let memory_writer: Arc<dyn SessionMemoryWriter> = if writer_shards == 1 {
        Arc::new(MemoryActor::with_dead_letter_service(
            Arc::clone(&memory),
            Arc::clone(&dead_letter_service),
            config.actors.memory.enqueue_timeout_ms,
            config.actors.memory.write_batch_max,
            config.actors.memory.write_batch_max,
            config.actors.memory.write_batch_max_delay_ms,
        ))
    } else {
        let shards: Vec<Arc<MemoryActor>> = (0..writer_shards)
            .map(|_| {
                Arc::new(MemoryActor::with_dead_letter_service(
                    Arc::clone(&memory),
                    Arc::clone(&dead_letter_service),
                    config.actors.memory.enqueue_timeout_ms,
                    config.actors.memory.write_batch_max,
                    config.actors.memory.write_batch_max,
                    config.actors.memory.write_batch_max_delay_ms,
                ))
            })
            .collect();
        Arc::new(ShardedMemoryWriter::new(shards))
    };
    let skills = Arc::new(skills);
    let session_responder = Arc::new(SessionResponderActor::with_dead_letter_service(
        config.clone(),
        Arc::clone(&llm),
        Arc::clone(&memory),
        Arc::clone(&memory_writer),
        Arc::clone(&file_downloader),
        Arc::clone(&skills),
        Arc::clone(&dead_letter_service),
        start_time,
    ));

    (dead_letter_service, memory_writer, session_responder)
}

pub fn build_actor_runtime_dependencies(
    config: Config,
    llm: Arc<dyn LlmClient>,
    memory: Arc<Memory>,
    file_downloader: Arc<dyn AttachmentDownloader>,
    skills: SkillRegistry,
) -> ActorRuntimeDependencies {
    let (dead_letter_service, memory_writer, session_responder) =
        build_runtime_components(&config, llm, memory, file_downloader, skills);
    ActorRuntimeDependencies {
        config: Arc::new(config),
        dead_letter_service,
        memory_writer,
        session_responder,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::actors::message::MessageResponder;
    use crate::agent::files::FileDownloader;
    use crate::config::*;
    use crate::llm::AnthropicClient;
    use tempfile::TempDir;

    struct RuntimeHarness {
        config: Config,
        memory: Arc<Memory>,
        dependencies: ActorRuntimeDependencies,
    }

    fn build_system_prompt(rt: &RuntimeHarness, ctx: &WorkspaceContext) -> String {
        build_system_prompt_static(&rt.config.agent.name, ctx)
    }

    /// Build a test runtime with a temporary memory directory
    fn test_runtime() -> (RuntimeHarness, TempDir) {
        test_runtime_with_skills(SkillRegistry::new())
    }

    fn test_runtime_with_skills(skills: SkillRegistry) -> (RuntimeHarness, TempDir) {
        test_runtime_with_setup(skills, |_| {})
    }

    fn test_runtime_with_config_mutator<F>(mutator: F) -> (RuntimeHarness, TempDir)
    where
        F: FnOnce(&mut Config),
    {
        test_runtime_with_setup(SkillRegistry::new(), mutator)
    }

    fn test_runtime_with_setup<F>(skills: SkillRegistry, mutator: F) -> (RuntimeHarness, TempDir)
    where
        F: FnOnce(&mut Config),
    {
        let tmp = TempDir::new().unwrap();
        let mut config = Config {
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
                host: None,
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
            skills: SkillsConfig::default(),
            keepalive: crate::config::KeepaliveConfig::default(),
            session: crate::config::SessionConfig::default(),
            actors: crate::config::ActorsConfig::default(),
        };
        mutator(&mut config);

        let llm: Arc<dyn LlmClient> = Arc::new(AnthropicClient::new(config.llm.clone()));
        let memory = Arc::new(Memory::open(tmp.path()).unwrap());
        let file_downloader = Arc::new(FileDownloader::new(3));
        let dependencies = build_actor_runtime_dependencies(
            config.clone(),
            llm,
            Arc::clone(&memory),
            file_downloader,
            skills,
        );
        let runtime = RuntimeHarness {
            config,
            memory,
            dependencies,
        };
        (runtime, tmp)
    }

    fn handle_command(rt: &RuntimeHarness, from: &str, body: &str) -> anyhow::Result<String> {
        let responder = Arc::clone(&rt.dependencies.session_responder);
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async {
                responder
                    .respond_to_command(from, body)
                    .await
                    .map(|reply| reply.text)
            })
    }

    #[test]
    fn test_status_in_room_context() {
        let (rt, _tmp) = test_runtime_with_config_mutator(|config| {
            config.rooms = vec![RoomConfig {
                jid: "lobby@conference.localhost".to_string(),
                nick: "bot".to_string(),
            }];
        });
        // Status from a room JID shows room-specific info
        let result = handle_command(&rt, "lobby@conference.localhost/alice", "/status").unwrap();
        assert!(result.contains("Room: lobby@conference.localhost"));
        assert!(result.contains("Room messages:"));
        // Should NOT show user profile/memory fields
        assert!(!result.contains("User profile:"));
        assert!(!result.contains("User memory:"));
    }

    #[test]
    fn test_status_in_direct_chat_context() {
        let (rt, _tmp) = test_runtime();
        let result = handle_command(&rt, "admin@localhost", "/status").unwrap();
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
        let result = handle_command(&rt, "admin@localhost/res", "/ping").unwrap();
        assert_eq!(result, "pong");
    }

    #[test]
    fn test_command_help_lists_all_commands() {
        let (rt, _tmp) = test_runtime();
        let result = handle_command(&rt, "admin@localhost/res", "/help").unwrap();
        assert!(result.contains("/new"));
        assert!(result.contains("/forget"));
        assert!(result.contains("/status"));
        assert!(result.contains("/ping"));
        assert!(result.contains("/help"));
    }

    #[test]
    fn test_command_unknown() {
        let (rt, _tmp) = test_runtime();
        let result = handle_command(&rt, "admin@localhost", "/foobar").unwrap();
        assert!(result.contains("Unknown command"));
        assert!(result.contains("/foobar"));
    }

    #[test]
    fn test_command_case_insensitive() {
        let (rt, _tmp) = test_runtime();
        assert_eq!(
            handle_command(&rt, "admin@localhost", "/PING").unwrap(),
            "pong"
        );
        assert_eq!(
            handle_command(&rt, "admin@localhost", "/Ping").unwrap(),
            "pong"
        );
    }

    #[test]
    fn test_command_new_empty_session() {
        let (rt, _tmp) = test_runtime();
        let result = handle_command(&rt, "admin@localhost/res", "/new").unwrap();
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
        rt.memory
            .store_message("admin@localhost", "user", "Hello")
            .unwrap();
        rt.memory
            .store_message("admin@localhost", "assistant", "Hi!")
            .unwrap();

        let result = handle_command(&rt, "admin@localhost/res", "/new").unwrap();
        assert!(
            result.to_lowercase().contains("archived") || result.to_lowercase().contains("session")
        );

        // History should be empty after /new
        let history = rt.memory.get_history("admin@localhost", 20).unwrap();
        assert!(history.is_empty());

        // Archived session count should be 1
        assert_eq!(rt.memory.session_count("admin@localhost").unwrap(), 1);
    }

    #[test]
    fn test_command_reset_is_alias_for_new() {
        let (rt, _tmp) = test_runtime();
        rt.memory
            .store_message("admin@localhost", "user", "Test")
            .unwrap();

        let result = handle_command(&rt, "admin@localhost", "/reset").unwrap();
        assert!(
            result.to_lowercase().contains("archived") || result.to_lowercase().contains("session")
        );
    }

    #[test]
    fn test_command_forget_clears_memory() {
        let (rt, _tmp) = test_runtime();
        rt.memory
            .store_message("admin@localhost", "user", "Hello")
            .unwrap();
        rt.memory
            .set_user_context("admin@localhost", "Likes Rust")
            .unwrap();

        let _result = handle_command(&rt, "admin@localhost/res", "/forget").unwrap();

        // Both history and context should be gone
        let history = rt.memory.get_history("admin@localhost", 20).unwrap();
        assert!(history.is_empty());
        assert!(rt
            .memory
            .get_user_context("admin@localhost")
            .unwrap()
            .is_none());
    }

    #[test]
    fn test_command_strips_resource_from_jid() {
        let (rt, _tmp) = test_runtime();
        rt.memory
            .store_message("admin@localhost", "user", "Hi")
            .unwrap();

        // Command from full JID with resource should see the bare JID's messages
        let result = handle_command(&rt, "admin@localhost/Conversations.xyz", "/status").unwrap();
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
        let prompt = build_system_prompt(&rt, &ctx);
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
        let prompt = build_system_prompt(&rt, &ctx);
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
        let prompt = build_system_prompt(&rt, &ctx);
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
        let prompt = build_system_prompt(&rt, &ctx);
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
        let prompt = build_system_prompt(&rt, &ctx);
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

        let result = handle_command(&rt, "admin@localhost/res", "/status").unwrap();
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
        assert!(result.contains("Session timeout: disabled"));
    }

    #[test]
    fn test_command_status_session_timeout_enabled() {
        let (rt, _tmp) = test_runtime_with_config_mutator(|config| {
            config.session.idle_timeout_mins = 120;
        });

        let result = handle_command(&rt, "admin@localhost", "/status").unwrap();
        assert!(result.contains("Session timeout: 120m idle"));
    }

    #[test]
    fn test_command_status_with_profile() {
        let (rt, _tmp) = test_runtime();
        rt.memory
            .set_user_context("admin@localhost", "Developer")
            .unwrap();

        let result = handle_command(&rt, "admin@localhost/res", "/status").unwrap();
        assert!(result.contains("User profile: yes"));
    }

    #[test]
    fn test_command_status_with_workspace_files() {
        let (rt, tmp) = test_runtime();
        std::fs::write(tmp.path().join("instructions.md"), "Be concise").unwrap();
        std::fs::write(tmp.path().join("identity.md"), "I am an agent").unwrap();

        let result = handle_command(&rt, "admin@localhost/res", "/status").unwrap();
        assert!(result.contains("instructions=yes"));
        assert!(result.contains("identity=yes"));
        assert!(result.contains("personality=none"));
    }

    #[test]
    fn test_command_status_domain_wildcard() {
        let (rt, _tmp) = test_runtime_with_config_mutator(|config| {
            config.agent.allowed_domains = vec!["*".to_string()];
        });

        let result = handle_command(&rt, "admin@localhost", "/status").unwrap();
        assert!(result.contains("Allowed domains: * (all)"));
    }

    #[test]
    fn test_command_status_domain_explicit() {
        let (rt, _tmp) = test_runtime_with_config_mutator(|config| {
            config.agent.allowed_domains = vec!["localhost".to_string(), "partner.org".to_string()];
        });

        let result = handle_command(&rt, "admin@localhost", "/status").unwrap();
        assert!(result.contains("Allowed domains: localhost, partner.org"));
    }

    // ── Status with files test ──────────────────────────

    #[test]
    fn test_command_status_with_files() {
        let (rt, _tmp) = test_runtime();
        let files_dir = rt.memory.files_dir("admin@localhost").unwrap();
        std::fs::write(files_dir.join("abc_photo.jpg"), b"fake").unwrap();

        let result = handle_command(&rt, "admin@localhost", "/status").unwrap();
        assert!(result.contains("Files: 1"));
    }

    #[test]
    fn test_command_status_no_files_hides_line() {
        let (rt, _tmp) = test_runtime();
        let result = handle_command(&rt, "admin@localhost", "/status").unwrap();
        // When there are no files, the "Files:" line should not appear
        assert!(!result.contains("Files:"));
    }

    // ── Status with skills test ─────────────────────────

    #[test]
    fn test_command_status_with_skills() {
        use crate::skills::Skill;
        use async_trait::async_trait;

        struct StubSkill(&'static str);
        #[async_trait]
        impl Skill for StubSkill {
            fn name(&self) -> &str {
                self.0
            }
            fn description(&self) -> &str {
                ""
            }
            fn parameters_schema(&self) -> serde_json::Value {
                serde_json::json!({})
            }
            async fn execute(
                &self,
                _: serde_json::Value,
                _context: &crate::skills::SkillContext,
            ) -> anyhow::Result<String> {
                Ok(String::new())
            }
        }

        let mut skills = SkillRegistry::new();
        skills.register(Box::new(StubSkill("web_search")));
        skills.register(Box::new(StubSkill("url_fetch")));
        let (rt, _tmp) = test_runtime_with_skills(skills);

        let result = handle_command(&rt, "admin@localhost", "/status").unwrap();
        assert!(result.contains("Skills: url_fetch, web_search"));
        assert!(!result.contains("Skills: none"));
    }

    // ── Agentic loop message structure tests ─────────────

    #[test]
    fn test_tool_result_message_structure() {
        use crate::llm::InputContentBlock;

        let msg = Message {
            role: "user".to_string(),
            content: MessageContent::Blocks(vec![InputContentBlock::ToolResult {
                tool_use_id: "toolu_abc123".to_string(),
                content: "Search returned 5 results".to_string(),
            }]),
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
