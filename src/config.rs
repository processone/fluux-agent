use serde::Deserialize;
use std::path::PathBuf;

#[derive(Debug, Deserialize, Clone)]
pub struct Config {
    pub server: ServerConfig,
    pub llm: LlmConfig,
    pub agent: AgentConfig,
    pub memory: MemoryConfig,
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
    "markdown".to_string()
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
}

impl Config {
    pub fn load(path: &str) -> anyhow::Result<Self> {
        let content = std::fs::read_to_string(path)?;
        // Expand environment variables like ${ANTHROPIC_API_KEY}
        let expanded = shellexpand::env(&content)?;
        let config: Config = toml::from_str(&expanded)?;
        Ok(config)
    }

    /// Checks if a JID is allowed to talk to the agent
    pub fn is_allowed(&self, jid: &str) -> bool {
        // Extract bare JID (without resource)
        let bare = jid.split('/').next().unwrap_or(jid);
        self.agent.allowed_jids.iter().any(|allowed| {
            allowed == bare || allowed == "*"
        })
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
                model: "claude-sonnet-4-5-20250929".to_string(),
                api_key: "test-key".to_string(),
                max_tokens_per_request: 4096,
            },
            agent: AgentConfig {
                name: "Test Agent".to_string(),
                allowed_jids: jids.into_iter().map(String::from).collect(),
            },
            memory: MemoryConfig {
                backend: "markdown".to_string(),
                path: PathBuf::from("./data/memory"),
            },
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
}
