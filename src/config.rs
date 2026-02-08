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
