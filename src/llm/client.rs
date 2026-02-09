//! `LlmClient` trait — abstraction over LLM backends.
//!
//! Providers (Anthropic, Ollama, …) implement this trait so the
//! runtime can be configured to use any supported backend via the
//! `[llm] provider` config field.

use anyhow::Result;
use async_trait::async_trait;

use super::{LlmResponse, Message, ToolDefinition};

/// Abstraction over LLM backends (Anthropic, Ollama, etc.).
///
/// Each provider translates the shared message/tool types into its own
/// wire format and normalizes responses back into [`LlmResponse`].
#[async_trait]
pub trait LlmClient: Send + Sync {
    /// Sends a conversation to the LLM and returns the response.
    ///
    /// When `tools` is `Some`, tool definitions are included and the
    /// response may contain tool_use calls. When `None`, the `tools`
    /// field is omitted.
    async fn complete(
        &self,
        system_prompt: &str,
        messages: &[Message],
        tools: Option<&[ToolDefinition]>,
    ) -> Result<LlmResponse>;

    /// Human-readable description of the provider and model.
    ///
    /// Used in status output, e.g. `"anthropic (claude-sonnet-4-5-20250929)"`.
    fn description(&self) -> String;
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Compile-time verification that `LlmClient` is object-safe.
    #[test]
    fn test_llm_client_is_object_safe() {
        fn _assert_object_safe(_: &dyn LlmClient) {}
    }
}
