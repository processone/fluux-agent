use serde::Deserialize;
use std::path::PathBuf;

#[derive(Debug, Deserialize, Clone)]
pub struct Config {
    pub server: ServerConfig,
    pub llm: LlmConfig,
    pub agent: AgentConfig,
    pub memory: MemoryConfig,
    /// MUC rooms to join on connect (XEP-0045)
    #[serde(default)]
    pub rooms: Vec<RoomConfig>,
    /// Skill configuration. If absent, no skills are registered.
    #[serde(default)]
    pub skills: SkillsConfig,
    /// Connection keepalive configuration.
    /// Enabled by default with sensible defaults.
    #[serde(default)]
    pub keepalive: KeepaliveConfig,
    /// Session timeout configuration.
    /// When enabled, idle sessions are automatically archived on next message.
    #[serde(default)]
    pub session: SessionConfig,
    /// Actor runtime configuration.
    /// Runtime actor options. Actor topology is always used at startup.
    /// The `enabled` field is kept for backward-compatibility and ignored.
    #[serde(default)]
    pub actors: ActorsConfig,
}

/// Configuration for a MUC room (XEP-0045)
#[derive(Debug, Deserialize, Clone)]
pub struct RoomConfig {
    /// Room JID, e.g. "lobby@conference.localhost"
    pub jid: String,
    /// Bot's nickname in the room
    #[serde(default = "default_room_nick")]
    pub nick: String,
}

fn default_room_nick() -> String {
    "fluux-agent".to_string()
}

#[derive(Debug, Deserialize, Clone)]
pub struct ServerConfig {
    pub host: String,
    pub port: u16,
    #[serde(flatten)]
    pub mode: ConnectionMode,
}

#[derive(Debug, Deserialize, Clone)]
#[serde(tag = "mode", rename_all = "snake_case")]
pub enum ConnectionMode {
    Component {
        component_domain: String,
        /// Supports ${ENV_VAR} substitution
        component_secret: String,
    },
    Client {
        /// Bot JID, e.g. "bot@example.com"
        jid: String,
        /// Supports ${ENV_VAR} substitution
        password: String,
        #[serde(default = "default_resource")]
        resource: String,
        /// Set to false for self-signed certs (dev servers)
        #[serde(default = "default_tls_verify")]
        tls_verify: bool,
    },
}

fn default_resource() -> String {
    "fluux-agent".to_string()
}

fn default_tls_verify() -> bool {
    true
}

#[derive(Debug, Deserialize, Clone)]
pub struct LlmConfig {
    pub provider: String,
    pub model: String,
    /// Supports ${ENV_VAR} substitution.
    /// Required for Anthropic; not needed for Ollama.
    #[serde(default)]
    pub api_key: String,
    #[serde(default = "default_max_tokens")]
    pub max_tokens_per_request: u32,
    /// Base URL for the LLM API.
    /// Used by Ollama (defaults to `"http://localhost:11434"` in the client).
    /// Ignored by Anthropic.
    #[serde(default)]
    pub host: Option<String>,
}

#[derive(Debug, Deserialize, Clone)]
pub struct AgentConfig {
    pub name: String,
    /// JIDs allowed to talk to the agent
    pub allowed_jids: Vec<String>,
    /// Domains allowed to send messages to the agent.
    /// If omitted, only the agent's own domain is allowed (safe default).
    /// Set to ["*"] to allow all domains (federation — use with caution).
    #[serde(default)]
    pub allowed_domains: Vec<String>,
}

#[derive(Debug, Deserialize, Clone)]
pub struct MemoryConfig {
    #[serde(default = "default_memory_backend")]
    pub backend: String,
    #[serde(default = "default_memory_path")]
    pub path: PathBuf,
}

/// Top-level skills configuration.
///
/// Each field corresponds to a builtin skill. If the field is `None`
/// (i.e. the TOML section is absent), that skill is not registered.
#[derive(Debug, Deserialize, Clone, Default)]
pub struct SkillsConfig {
    /// Web search skill configuration.
    pub web_search: Option<WebSearchConfig>,
    /// Memory (knowledge store) skill configuration.
    pub memory: Option<MemorySkillConfig>,
    /// URL fetch skill configuration.
    pub url_fetch: Option<UrlFetchConfig>,
}

/// Configuration for the `memory_store` and `memory_recall` builtin skills.
///
/// These skills let the LLM store and retrieve per-JID knowledge entries.
/// No API keys needed — uses local filesystem only.
#[derive(Debug, Deserialize, Clone)]
pub struct MemorySkillConfig {
    /// Enable the memory skills. Must be `true` to register them.
    #[serde(default)]
    pub enabled: bool,
}

/// Configuration for the `url_fetch` builtin skill.
///
/// Lets the LLM fetch a URL and extract readable text content.
/// No API keys needed — uses direct HTTP(S) requests.
#[derive(Debug, Deserialize, Clone)]
pub struct UrlFetchConfig {
    /// Enable the url_fetch skill. Must be `true` to register it.
    #[serde(default)]
    pub enabled: bool,
}

/// Keepalive configuration for detecting dead XMPP connections.
///
/// When enabled, the agent periodically sends whitespace pings (RFC 6120 §4.6.1)
/// and applies a read timeout to detect stale TCP connections
/// (e.g. after machine sleep/wake cycles).
#[derive(Debug, Deserialize, Clone)]
pub struct KeepaliveConfig {
    /// Enable keepalive. Default: true.
    #[serde(default = "default_keepalive_enabled")]
    pub enabled: bool,
    /// Interval between whitespace pings, in seconds. Default: 60.
    #[serde(default = "default_ping_interval")]
    pub ping_interval_secs: u64,
    /// Read timeout in seconds. If no data is received for this duration,
    /// a whitespace ping probe is sent to check if the connection is alive.
    /// Default: 300 (5 minutes).
    #[serde(default = "default_read_timeout")]
    pub read_timeout_secs: u64,
}

fn default_keepalive_enabled() -> bool {
    true
}

fn default_ping_interval() -> u64 {
    60
}

fn default_read_timeout() -> u64 {
    300
}

impl Default for KeepaliveConfig {
    fn default() -> Self {
        Self {
            enabled: default_keepalive_enabled(),
            ping_interval_secs: default_ping_interval(),
            read_timeout_secs: default_read_timeout(),
        }
    }
}

/// Session timeout configuration.
///
/// When enabled, sessions that have been idle for longer than
/// `idle_timeout_mins` are automatically archived when the next
/// message arrives (lazy evaluation — no background timer).
#[derive(Debug, Deserialize, Clone)]
pub struct SessionConfig {
    /// Idle timeout in minutes. If the session has been idle for longer
    /// than this, it is archived on the next inbound message.
    /// Default: 0 (disabled).
    #[serde(default)]
    pub idle_timeout_mins: u64,
}

impl Default for SessionConfig {
    fn default() -> Self {
        Self {
            idle_timeout_mins: 0,
        }
    }
}

/// Actor runtime configuration.
///
/// This is intentionally disabled by default so existing deployments keep
/// current runtime behavior until actor mode is explicitly enabled.
#[derive(Debug, Deserialize, Clone)]
pub struct ActorsConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default = "default_router_mailbox")]
    pub router_mailbox: usize,
    #[serde(default = "default_session_mailbox")]
    pub session_mailbox: usize,
    #[serde(default = "default_max_active_sessions")]
    pub max_active_sessions: usize,
    #[serde(default = "default_session_idle_ttl_secs")]
    pub session_idle_ttl_secs: u64,
    #[serde(default = "default_busy_retry_after_secs")]
    pub busy_retry_after_secs: u64,
    #[serde(default = "default_dedupe_ttl_secs")]
    pub dedupe_ttl_secs: u64,
    #[serde(default = "default_dead_letter_path")]
    pub dead_letter_path: PathBuf,
    #[serde(default)]
    pub tooling: ActorToolingConfig,
    #[serde(default)]
    pub supervision: ActorSupervisionConfig,
    #[serde(default)]
    pub memory: ActorMemoryConfig,
    #[serde(default)]
    pub observability: ActorObservabilityConfig,
}

#[derive(Debug, Deserialize, Clone)]
pub struct ActorToolingConfig {
    #[serde(default = "default_max_tool_rounds")]
    pub max_tool_rounds: usize,
    #[serde(default = "default_skill_timeout_secs")]
    pub skill_timeout_secs: u64,
    #[serde(default = "default_max_parallel_skills")]
    pub max_parallel_skills: usize,
    #[serde(default = "default_skill_queue_timeout_ms")]
    pub skill_queue_timeout_ms: u64,
    #[serde(default = "default_allowed_capabilities")]
    pub allowed_capabilities: Vec<String>,
}

#[derive(Debug, Deserialize, Clone)]
pub struct ActorSupervisionConfig {
    #[serde(default = "default_restart_backoff_min_ms")]
    pub restart_backoff_min_ms: u64,
    #[serde(default = "default_restart_backoff_max_ms")]
    pub restart_backoff_max_ms: u64,
    #[serde(default = "default_max_restarts_per_minute")]
    pub max_restarts_per_minute: u64,
    #[serde(default = "default_egress_send_timeout_ms")]
    pub egress_send_timeout_ms: u64,
    #[serde(default = "default_egress_retry_backoff_ms")]
    pub egress_retry_backoff_ms: u64,
    #[serde(default = "default_egress_max_retries")]
    pub egress_max_retries: usize,
}

#[derive(Debug, Deserialize, Clone)]
pub struct ActorMemoryConfig {
    #[serde(default = "default_memory_enqueue_timeout_ms")]
    pub enqueue_timeout_ms: u64,
    #[serde(default = "default_memory_writer_shards")]
    pub writer_shards: usize,
    #[serde(default = "default_memory_write_batch_max")]
    pub write_batch_max: usize,
    #[serde(default = "default_memory_write_batch_max_delay_ms")]
    pub write_batch_max_delay_ms: u64,
}

#[derive(Debug, Deserialize, Clone)]
pub struct ActorObservabilityConfig {
    #[serde(default = "default_actor_metrics_enabled")]
    pub metrics_enabled: bool,
    #[serde(default = "default_queue_depth_export_interval_secs")]
    pub queue_depth_export_interval_secs: u64,
    #[serde(default = "default_slow_actor_warn_ms")]
    pub slow_actor_warn_ms: u64,
}

impl Default for ActorsConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            router_mailbox: default_router_mailbox(),
            session_mailbox: default_session_mailbox(),
            max_active_sessions: default_max_active_sessions(),
            session_idle_ttl_secs: default_session_idle_ttl_secs(),
            busy_retry_after_secs: default_busy_retry_after_secs(),
            dedupe_ttl_secs: default_dedupe_ttl_secs(),
            dead_letter_path: default_dead_letter_path(),
            tooling: ActorToolingConfig::default(),
            supervision: ActorSupervisionConfig::default(),
            memory: ActorMemoryConfig::default(),
            observability: ActorObservabilityConfig::default(),
        }
    }
}

impl Default for ActorToolingConfig {
    fn default() -> Self {
        Self {
            max_tool_rounds: default_max_tool_rounds(),
            skill_timeout_secs: default_skill_timeout_secs(),
            max_parallel_skills: default_max_parallel_skills(),
            skill_queue_timeout_ms: default_skill_queue_timeout_ms(),
            allowed_capabilities: default_allowed_capabilities(),
        }
    }
}

impl Default for ActorSupervisionConfig {
    fn default() -> Self {
        Self {
            restart_backoff_min_ms: default_restart_backoff_min_ms(),
            restart_backoff_max_ms: default_restart_backoff_max_ms(),
            max_restarts_per_minute: default_max_restarts_per_minute(),
            egress_send_timeout_ms: default_egress_send_timeout_ms(),
            egress_retry_backoff_ms: default_egress_retry_backoff_ms(),
            egress_max_retries: default_egress_max_retries(),
        }
    }
}

impl Default for ActorMemoryConfig {
    fn default() -> Self {
        Self {
            enqueue_timeout_ms: default_memory_enqueue_timeout_ms(),
            writer_shards: default_memory_writer_shards(),
            write_batch_max: default_memory_write_batch_max(),
            write_batch_max_delay_ms: default_memory_write_batch_max_delay_ms(),
        }
    }
}

impl Default for ActorObservabilityConfig {
    fn default() -> Self {
        Self {
            metrics_enabled: default_actor_metrics_enabled(),
            queue_depth_export_interval_secs: default_queue_depth_export_interval_secs(),
            slow_actor_warn_ms: default_slow_actor_warn_ms(),
        }
    }
}

fn default_router_mailbox() -> usize {
    1024
}

fn default_session_mailbox() -> usize {
    256
}

fn default_max_active_sessions() -> usize {
    2000
}

fn default_session_idle_ttl_secs() -> u64 {
    1800
}

fn default_busy_retry_after_secs() -> u64 {
    5
}

fn default_dedupe_ttl_secs() -> u64 {
    600
}

fn default_dead_letter_path() -> PathBuf {
    PathBuf::from("data/dead_letters.jsonl")
}

fn default_max_tool_rounds() -> usize {
    10
}

fn default_skill_timeout_secs() -> u64 {
    30
}

fn default_max_parallel_skills() -> usize {
    32
}

fn default_skill_queue_timeout_ms() -> u64 {
    5000
}

fn default_allowed_capabilities() -> Vec<String> {
    vec!["*".to_string()]
}

fn default_restart_backoff_min_ms() -> u64 {
    200
}

fn default_restart_backoff_max_ms() -> u64 {
    10_000
}

fn default_max_restarts_per_minute() -> u64 {
    60
}

fn default_egress_send_timeout_ms() -> u64 {
    200
}

fn default_egress_retry_backoff_ms() -> u64 {
    50
}

fn default_egress_max_retries() -> usize {
    3
}

fn default_memory_enqueue_timeout_ms() -> u64 {
    200
}

fn default_memory_writer_shards() -> usize {
    1
}

fn default_memory_write_batch_max() -> usize {
    32
}

fn default_memory_write_batch_max_delay_ms() -> u64 {
    20
}

fn default_actor_metrics_enabled() -> bool {
    true
}

fn default_queue_depth_export_interval_secs() -> u64 {
    5
}

fn default_slow_actor_warn_ms() -> u64 {
    200
}

/// Configuration for the `web_search` builtin skill.
#[derive(Debug, Deserialize, Clone)]
pub struct WebSearchConfig {
    /// Search API provider: `"tavily"` or `"perplexity"`.
    pub provider: String,
    /// API key for the search provider. Supports `${ENV_VAR}` substitution.
    pub api_key: String,
    /// Maximum number of search results to return (default: 5).
    /// Used by Tavily; ignored by Perplexity.
    #[serde(default = "default_max_results")]
    pub max_results: u8,
    /// Model name for providers that support it (e.g. `"sonar"`, `"sonar-pro"`).
    /// Used by Perplexity (defaults to `"sonar"`); ignored by Tavily.
    pub model: Option<String>,
}

fn default_max_results() -> u8 {
    5
}

fn default_max_tokens() -> u32 {
    4096
}

fn default_memory_backend() -> String {
    "jsonl".to_string()
}

fn default_memory_path() -> PathBuf {
    PathBuf::from("./data/memory")
}

impl ServerConfig {
    /// Human-readable description of the connection mode
    pub fn mode_description(&self) -> String {
        match &self.mode {
            ConnectionMode::Component {
                component_domain, ..
            } => {
                format!("component ({component_domain})")
            }
            ConnectionMode::Client { jid, .. } => {
                format!("C2S client ({jid})")
            }
        }
    }

    /// Whether TLS certificate verification is enabled.
    ///
    /// Returns the `tls_verify` setting from client mode, or `true` (default)
    /// for component mode.
    pub fn tls_verify(&self) -> bool {
        match &self.mode {
            ConnectionMode::Client { tls_verify, .. } => *tls_verify,
            ConnectionMode::Component { .. } => true,
        }
    }

    /// Returns the agent's own XMPP domain.
    ///
    /// - Component mode: the component domain (e.g. "agent.localhost")
    /// - Client mode: the domain part of the JID (e.g. "localhost" from "bot@localhost")
    pub fn domain(&self) -> &str {
        match &self.mode {
            ConnectionMode::Component {
                component_domain, ..
            } => component_domain.as_str(),
            ConnectionMode::Client { jid, .. } => jid.split('@').nth(1).unwrap_or(jid.as_str()),
        }
    }
}

impl Config {
    pub fn load(path: &str) -> anyhow::Result<Self> {
        let content = std::fs::read_to_string(path)?;
        // Expand environment variables like ${ANTHROPIC_API_KEY}
        let expanded = shellexpand::env(&content)?;
        let config: Config = toml::from_str(&expanded)?;
        Ok(config)
    }

    /// Returns the room config for a given room JID, if configured
    pub fn find_room(&self, room_jid: &str) -> Option<&RoomConfig> {
        self.rooms.iter().find(|r| r.jid == room_jid)
    }

    /// Checks if a JID is allowed to talk to the agent
    pub fn is_allowed(&self, jid: &str) -> bool {
        let bare = crate::xmpp::stanzas::bare_jid(jid);
        self.agent
            .allowed_jids
            .iter()
            .any(|allowed| allowed == bare || allowed == "*")
    }

    /// Checks if a JID's domain is allowed.
    ///
    /// If `allowed_domains` is empty (the default), only the agent's own domain
    /// is accepted. If `allowed_domains` contains `"*"`, all domains pass.
    /// Otherwise, the sender's domain must be in the list.
    pub fn is_domain_allowed(&self, jid: &str) -> bool {
        let bare = crate::xmpp::stanzas::bare_jid(jid);
        let sender_domain = bare.split('@').nth(1).unwrap_or(bare);

        if self.agent.allowed_domains.is_empty() {
            // Default: only accept the agent's own domain
            sender_domain == self.server.domain()
        } else if self.agent.allowed_domains.iter().any(|d| d == "*") {
            true
        } else {
            self.agent
                .allowed_domains
                .iter()
                .any(|d| d == sender_domain)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Helper to build a Config with specific allowed_jids
    fn config_with_jids(jids: Vec<&str>) -> Config {
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
                api_key: "test-key".to_string(),
                max_tokens_per_request: 4096,
                host: None,
            },
            agent: AgentConfig {
                name: "Test Agent".to_string(),
                allowed_jids: jids.into_iter().map(String::from).collect(),
                allowed_domains: vec![],
            },
            memory: MemoryConfig {
                backend: "markdown".to_string(),
                path: PathBuf::from("./data/memory"),
            },
            rooms: vec![],
            skills: SkillsConfig::default(),
            keepalive: KeepaliveConfig::default(),
            session: SessionConfig::default(),
            actors: ActorsConfig::default(),
        }
    }

    // ── is_allowed tests ────────────────────────────────

    #[test]
    fn test_is_allowed_bare_jid() {
        let config = config_with_jids(vec!["admin@localhost"]);
        assert!(config.is_allowed("admin@localhost"));
    }

    #[test]
    fn test_is_allowed_full_jid_strips_resource() {
        let config = config_with_jids(vec!["admin@localhost"]);
        assert!(config.is_allowed("admin@localhost/mobile"));
        assert!(config.is_allowed("admin@localhost/Conversations.abc123"));
    }

    #[test]
    fn test_is_allowed_rejects_unauthorized() {
        let config = config_with_jids(vec!["admin@localhost"]);
        assert!(!config.is_allowed("hacker@evil.com"));
        assert!(!config.is_allowed("hacker@evil.com/res"));
    }

    #[test]
    fn test_is_allowed_wildcard() {
        let config = config_with_jids(vec!["*"]);
        assert!(config.is_allowed("anyone@anywhere.com"));
        assert!(config.is_allowed("user@domain.org/res"));
    }

    #[test]
    fn test_is_allowed_multiple_jids() {
        let config = config_with_jids(vec!["alice@localhost", "bob@localhost"]);
        assert!(config.is_allowed("alice@localhost"));
        assert!(config.is_allowed("bob@localhost/phone"));
        assert!(!config.is_allowed("charlie@localhost"));
    }

    #[test]
    fn test_is_allowed_empty_list_rejects_all() {
        let config = config_with_jids(vec![]);
        assert!(!config.is_allowed("admin@localhost"));
    }

    #[test]
    fn test_is_allowed_different_domain() {
        let config = config_with_jids(vec!["admin@localhost"]);
        // Same username, different domain → rejected
        assert!(!config.is_allowed("admin@otherdomain.com"));
    }

    // ── mode_description tests ──────────────────────────

    #[test]
    fn test_mode_description_client() {
        let config = config_with_jids(vec![]);
        assert_eq!(
            config.server.mode_description(),
            "C2S client (bot@localhost)"
        );
    }

    #[test]
    fn test_mode_description_component() {
        let server = ServerConfig {
            host: "localhost".to_string(),
            port: 5275,
            mode: ConnectionMode::Component {
                component_domain: "agent.localhost".to_string(),
                component_secret: "secret".to_string(),
            },
        };
        assert_eq!(server.mode_description(), "component (agent.localhost)");
    }

    // ── find_room tests ──────────────────────────────────

    #[test]
    fn test_find_room_found() {
        let mut config = config_with_jids(vec!["admin@localhost"]);
        config.rooms = vec![
            RoomConfig {
                jid: "lobby@conference.localhost".to_string(),
                nick: "bot".to_string(),
            },
            RoomConfig {
                jid: "dev@conference.localhost".to_string(),
                nick: "fluux-agent".to_string(),
            },
        ];
        let room = config.find_room("dev@conference.localhost").unwrap();
        assert_eq!(room.jid, "dev@conference.localhost");
        assert_eq!(room.nick, "fluux-agent");
    }

    #[test]
    fn test_find_room_not_found() {
        let config = config_with_jids(vec![]);
        assert!(config
            .find_room("nonexistent@conference.localhost")
            .is_none());
    }

    // ── domain() tests ──────────────────────────────────

    #[test]
    fn test_domain_client_mode() {
        let config = config_with_jids(vec![]);
        assert_eq!(config.server.domain(), "localhost");
    }

    #[test]
    fn test_domain_component_mode() {
        let server = ServerConfig {
            host: "localhost".to_string(),
            port: 5275,
            mode: ConnectionMode::Component {
                component_domain: "agent.example.com".to_string(),
                component_secret: "secret".to_string(),
            },
        };
        assert_eq!(server.domain(), "agent.example.com");
    }

    // ── is_domain_allowed() tests ───────────────────────

    #[test]
    fn test_domain_default_accepts_own_domain() {
        // No allowed_domains configured → only own domain accepted
        let config = config_with_jids(vec!["*"]);
        assert!(config.is_domain_allowed("alice@localhost"));
        assert!(config.is_domain_allowed("alice@localhost/res"));
    }

    #[test]
    fn test_domain_default_rejects_foreign_domain() {
        let config = config_with_jids(vec!["*"]);
        assert!(!config.is_domain_allowed("hacker@evil.com"));
        assert!(!config.is_domain_allowed("user@other.org/mobile"));
    }

    #[test]
    fn test_domain_wildcard_allows_all() {
        let mut config = config_with_jids(vec!["*"]);
        config.agent.allowed_domains = vec!["*".to_string()];
        assert!(config.is_domain_allowed("anyone@anywhere.com"));
        assert!(config.is_domain_allowed("user@evil.org/res"));
    }

    #[test]
    fn test_domain_explicit_list() {
        let mut config = config_with_jids(vec!["*"]);
        config.agent.allowed_domains = vec!["localhost".to_string(), "partner.org".to_string()];
        assert!(config.is_domain_allowed("alice@localhost"));
        assert!(config.is_domain_allowed("bob@partner.org/phone"));
        assert!(!config.is_domain_allowed("hacker@evil.com"));
    }

    #[test]
    fn test_domain_component_mode_default() {
        // Component mode: own domain is "agent.localhost"
        let mut config = config_with_jids(vec!["*"]);
        config.server = ServerConfig {
            host: "localhost".to_string(),
            port: 5275,
            mode: ConnectionMode::Component {
                component_domain: "agent.localhost".to_string(),
                component_secret: "secret".to_string(),
            },
        };
        // With no allowed_domains, only agent.localhost is accepted
        assert!(config.is_domain_allowed("user@agent.localhost"));
        assert!(!config.is_domain_allowed("user@localhost"));
        assert!(!config.is_domain_allowed("user@evil.com"));
    }

    #[test]
    fn test_domain_check_strips_resource() {
        let config = config_with_jids(vec!["*"]);
        assert!(config.is_domain_allowed("alice@localhost/Conversations.abc"));
        assert!(!config.is_domain_allowed("alice@evil.com/Conversations.abc"));
    }

    // ── KeepaliveConfig tests ────────────────────────────

    #[test]
    fn test_keepalive_defaults() {
        let ka = KeepaliveConfig::default();
        assert!(ka.enabled);
        assert_eq!(ka.ping_interval_secs, 60);
        assert_eq!(ka.read_timeout_secs, 300);
    }

    #[test]
    fn test_keepalive_default_when_absent() {
        // Config without [keepalive] section → defaults apply
        let config = config_with_jids(vec![]);
        assert!(config.keepalive.enabled);
        assert_eq!(config.keepalive.ping_interval_secs, 60);
        assert_eq!(config.keepalive.read_timeout_secs, 300);
    }

    #[test]
    fn test_keepalive_disabled_toml() {
        let toml = r#"
            enabled = false
        "#;
        let ka: KeepaliveConfig = toml::from_str(toml).unwrap();
        assert!(!ka.enabled);
        // Other fields still get defaults
        assert_eq!(ka.ping_interval_secs, 60);
        assert_eq!(ka.read_timeout_secs, 300);
    }

    #[test]
    fn test_keepalive_custom_values_toml() {
        let toml = r#"
            enabled = true
            ping_interval_secs = 30
            read_timeout_secs = 120
        "#;
        let ka: KeepaliveConfig = toml::from_str(toml).unwrap();
        assert!(ka.enabled);
        assert_eq!(ka.ping_interval_secs, 30);
        assert_eq!(ka.read_timeout_secs, 120);
    }

    // ── SessionConfig tests ─────────────────────────────

    #[test]
    fn test_session_defaults() {
        let sc = SessionConfig::default();
        assert_eq!(sc.idle_timeout_mins, 0);
    }

    #[test]
    fn test_session_default_when_absent() {
        let config = config_with_jids(vec![]);
        assert_eq!(config.session.idle_timeout_mins, 0);
    }

    #[test]
    fn test_session_custom_timeout_toml() {
        let toml = r#"
            idle_timeout_mins = 120
        "#;
        let sc: SessionConfig = toml::from_str(toml).unwrap();
        assert_eq!(sc.idle_timeout_mins, 120);
    }

    // ── ActorsConfig tests ──────────────────────────────

    #[test]
    fn test_actors_defaults_disabled() {
        let ac = ActorsConfig::default();
        assert!(!ac.enabled);
        assert_eq!(ac.router_mailbox, 1024);
        assert_eq!(ac.session_mailbox, 256);
        assert_eq!(ac.supervision.egress_send_timeout_ms, 200);
        assert_eq!(ac.supervision.egress_retry_backoff_ms, 50);
        assert_eq!(ac.supervision.egress_max_retries, 3);
        assert_eq!(ac.tooling.allowed_capabilities, vec!["*"]);
        assert_eq!(
            ac.dead_letter_path,
            PathBuf::from("data/dead_letters.jsonl")
        );
    }

    #[test]
    fn test_actors_custom_values_toml() {
        let toml = r#"
            enabled = true
            router_mailbox = 2048
            session_mailbox = 512

            [tooling]
            max_tool_rounds = 12
            skill_timeout_secs = 45
            allowed_capabilities = ["network:*"]

            [supervision]
            egress_send_timeout_ms = 350
            egress_retry_backoff_ms = 75
            egress_max_retries = 5
        "#;
        let ac: ActorsConfig = toml::from_str(toml).unwrap();
        assert!(ac.enabled);
        assert_eq!(ac.router_mailbox, 2048);
        assert_eq!(ac.session_mailbox, 512);
        assert_eq!(ac.tooling.max_tool_rounds, 12);
        assert_eq!(ac.tooling.skill_timeout_secs, 45);
        assert_eq!(ac.supervision.egress_send_timeout_ms, 350);
        assert_eq!(ac.supervision.egress_retry_backoff_ms, 75);
        assert_eq!(ac.supervision.egress_max_retries, 5);
        // Non-overridden fields keep defaults.
        assert_eq!(ac.tooling.max_parallel_skills, 32);
        assert_eq!(ac.tooling.allowed_capabilities, vec!["network:*"]);
    }
}
