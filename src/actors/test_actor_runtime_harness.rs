use std::path::PathBuf;
use std::sync::Arc;

use tempfile::TempDir;

use crate::agent::files::{AttachmentDownloader, FileDownloader};
use crate::agent::memory::Memory;
use crate::agent::runtime::{build_actor_runtime_dependencies, ActorRuntimeDependencies};
use crate::config::{
    ActorMemoryConfig, ActorObservabilityConfig, ActorSupervisionConfig, ActorToolingConfig,
    ActorsConfig, AgentConfig, Config, ConnectionMode, KeepaliveConfig, LlmConfig, MemoryConfig,
    RoomConfig, ServerConfig, SessionConfig, SkillsConfig,
};
use crate::llm::{AnthropicClient, LlmClient};
use crate::skills::SkillRegistry;

fn default_test_llm_config() -> LlmConfig {
    LlmConfig {
        provider: "anthropic".to_string(),
        model: "claude-haiku-4-5-20250110".to_string(),
        api_key: String::new(),
        max_tokens_per_request: 4096,
        host: None,
    }
}

pub(crate) fn build_actor_runtime_fixture(
    allowed_jids: Vec<String>,
    rooms: Vec<RoomConfig>,
    llm: Arc<dyn LlmClient>,
    file_downloader: Arc<dyn AttachmentDownloader>,
    skills: SkillRegistry,
    tooling: ActorToolingConfig,
    actor_memory: ActorMemoryConfig,
) -> (ActorRuntimeDependencies, TempDir) {
    let tmp = TempDir::new().expect("failed to create temp dir for actor runtime fixture");
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
        llm: default_test_llm_config(),
        agent: AgentConfig {
            name: "Test Agent".to_string(),
            allowed_jids,
            allowed_domains: vec![],
        },
        memory: MemoryConfig {
            backend: "markdown".to_string(),
            path: PathBuf::new(),
        },
        rooms,
        skills: SkillsConfig::default(),
        keepalive: KeepaliveConfig::default(),
        session: SessionConfig::default(),
        actors: ActorsConfig {
            enabled: true,
            router_mailbox: 64,
            session_mailbox: 8,
            max_active_sessions: 8,
            session_idle_ttl_secs: 1800,
            busy_retry_after_secs: 5,
            dedupe_ttl_secs: 600,
            dead_letter_path: PathBuf::from("data/dead_letters.jsonl"),
            tooling,
            supervision: ActorSupervisionConfig::default(),
            memory: actor_memory,
            observability: ActorObservabilityConfig::default(),
        },
    };

    config.memory.path = PathBuf::from(tmp.path());
    let memory = Arc::new(Memory::open(tmp.path()).expect("failed to open memory fixture"));
    let runtime = build_actor_runtime_dependencies(config, llm, memory, file_downloader, skills);

    (runtime, tmp)
}

pub(crate) fn build_default_actor_runtime_fixture(
    allowed_jids: Vec<String>,
) -> (ActorRuntimeDependencies, TempDir) {
    let llm_config = default_test_llm_config();
    let llm: Arc<dyn LlmClient> = Arc::new(AnthropicClient::new(llm_config));
    let file_downloader: Arc<dyn AttachmentDownloader> = Arc::new(FileDownloader::new(3));
    build_actor_runtime_fixture(
        allowed_jids,
        vec![],
        llm,
        file_downloader,
        SkillRegistry::new(),
        ActorToolingConfig::default(),
        ActorMemoryConfig::default(),
    )
}
