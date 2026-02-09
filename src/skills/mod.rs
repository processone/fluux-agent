pub mod builtin;
pub mod registry;

use std::path::PathBuf;

use async_trait::async_trait;

/// Runtime context passed to skill execution.
///
/// Provides the invoking JID and memory base path so skills
/// can scope their state per-conversation-partner.
pub struct SkillContext {
    /// Bare JID of the conversation partner (user or room).
    pub jid: String,
    /// Base path of the memory store (same as `Memory.base_path`).
    pub base_path: PathBuf,
}

/// A skill that the LLM can invoke via tool_use.
///
/// All skills (builtin, Wasm, MCP) implement this trait.
/// The runtime calls `execute()` when the LLM requests a tool_use.
#[async_trait]
pub trait Skill: Send + Sync {
    /// Unique identifier used in the Anthropic `tools[]` array.
    /// Must be lowercase alphanumeric + underscores (e.g. "web_search").
    fn name(&self) -> &str;

    /// Human-readable description shown to the LLM so it knows
    /// when to invoke this skill.
    fn description(&self) -> &str;

    /// JSON Schema describing the parameters this skill accepts.
    /// Used as the `input_schema` field in the Anthropic tool definition.
    fn parameters_schema(&self) -> serde_json::Value;

    /// Required capabilities (validated at startup, not yet enforced).
    /// Examples: "network:api.example.com:443", "filesystem:/tmp:read"
    fn capabilities(&self) -> Vec<String> {
        vec![]
    }

    /// Execute the skill with the given parameters and return a text result.
    /// The returned string is sent back to the LLM as a `tool_result`.
    /// The `context` provides the invoking JID and memory base path.
    async fn execute(
        &self,
        params: serde_json::Value,
        context: &SkillContext,
    ) -> anyhow::Result<String>;
}

pub use registry::SkillRegistry;
