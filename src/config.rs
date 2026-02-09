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
    /// Supports ${ENV_VAR} substitution
    pub api_key: String,
    #[serde(default = "default_max_tokens")]
    pub max_tokens_per_request: u32,
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
            ConnectionMode::Component { component_domain, .. } => {
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
            ConnectionMode::Client { jid, .. } => {
                jid.split('@').nth(1).unwrap_or(jid.as_str())
            }
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
        self.agent.allowed_jids.iter().any(|allowed| {
            allowed == bare || allowed == "*"
        })
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
        assert_eq!(
            server.mode_description(),
            "component (agent.localhost)"
        );
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
        assert!(config.find_room("nonexistent@conference.localhost").is_none());
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
        config.agent.allowed_domains = vec![
            "localhost".to_string(),
            "partner.org".to_string(),
        ];
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
}
