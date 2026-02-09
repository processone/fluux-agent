//! Ollama API provider.
//!
//! Calls `POST {host}/api/chat` with an Ollama-native request format.
//! Translates the shared message/tool types into Ollama's wire format
//! and normalizes responses back into `LlmResponse`.
//!
//! Key differences from Anthropic:
//! - System prompt is sent as a `role: "system"` message (not a top-level field).
//! - Tool definitions use OpenAI-style `{type: "function", function: {...}}` format.
//! - Tool results use `role: "tool"` messages (not `role: "user"` with ToolResult blocks).
//! - Stop reason: `"stop"` → `EndTurn`, presence of `tool_calls` → `ToolUse`.
//! - Token usage: `prompt_eval_count` / `eval_count` (may be absent).
//! - Must set `stream: false` for synchronous responses.

use anyhow::Result;
use async_trait::async_trait;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use tracing::{debug, info, warn};

use crate::config::LlmConfig;
use super::client::LlmClient;
use super::{
    InputContentBlock, LlmResponse, Message, MessageContent, StopReason, ToolCall, ToolDefinition,
};

/// Default Ollama API base URL.
const DEFAULT_OLLAMA_HOST: &str = "http://localhost:11434";

// ── Ollama API request types ─────────────────────────────

/// Ollama `/api/chat` request body.
#[derive(Debug, Serialize)]
struct OllamaChatRequest {
    model: String,
    messages: Vec<OllamaMessage>,
    stream: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    tools: Option<Vec<OllamaToolDef>>,
    options: OllamaOptions,
}

/// Ollama message in the conversation.
#[derive(Debug, Serialize, Deserialize, Clone)]
struct OllamaMessage {
    role: String,
    content: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_calls: Option<Vec<OllamaToolCall>>,
}

/// Ollama tool definition (OpenAI-compatible format).
#[derive(Debug, Serialize)]
struct OllamaToolDef {
    #[serde(rename = "type")]
    tool_type: String,
    function: OllamaFunctionDef,
}

/// Inner function definition within an Ollama tool.
#[derive(Debug, Serialize)]
struct OllamaFunctionDef {
    name: String,
    description: String,
    parameters: serde_json::Value,
}

/// Ollama tool call in assistant responses.
#[derive(Debug, Serialize, Deserialize, Clone)]
struct OllamaToolCall {
    function: OllamaFunctionCall,
}

/// Inner function call within an Ollama tool call.
#[derive(Debug, Serialize, Deserialize, Clone)]
struct OllamaFunctionCall {
    name: String,
    arguments: serde_json::Value,
}

/// Ollama generation options.
#[derive(Debug, Serialize)]
struct OllamaOptions {
    num_predict: u32,
}

// ── Ollama API response types ────────────────────────────

/// Ollama `/api/chat` response.
#[derive(Debug, Deserialize)]
struct OllamaChatResponse {
    message: OllamaResponseMessage,
    #[serde(default)]
    done_reason: Option<String>,
    #[serde(default)]
    prompt_eval_count: Option<u32>,
    #[serde(default)]
    eval_count: Option<u32>,
}

/// Message in an Ollama response.
#[derive(Debug, Deserialize)]
struct OllamaResponseMessage {
    content: String,
    #[serde(default)]
    tool_calls: Option<Vec<OllamaToolCall>>,
}

// ── OllamaClient ────────────────────────────────────────

/// Client for the Ollama API.
pub struct OllamaClient {
    client: Client,
    config: LlmConfig,
    host: String,
}

impl OllamaClient {
    /// Creates a new Ollama client from configuration.
    ///
    /// If `config.host` is `None`, defaults to `http://localhost:11434`.
    pub fn new(config: LlmConfig) -> Self {
        let host = config
            .host
            .clone()
            .unwrap_or_else(|| DEFAULT_OLLAMA_HOST.to_string());
        // Strip trailing slash for consistent URL construction
        let host = host.trim_end_matches('/').to_string();
        Self {
            client: Client::new(),
            config,
            host,
        }
    }
}

#[async_trait]
impl LlmClient for OllamaClient {
    async fn complete(
        &self,
        system_prompt: &str,
        messages: &[Message],
        tools: Option<&[ToolDefinition]>,
    ) -> Result<LlmResponse> {
        // Build Ollama messages: system prompt as first message, then conversation
        let mut ollama_messages = Vec::with_capacity(messages.len() + 1);

        // System prompt as a system message
        if !system_prompt.is_empty() {
            ollama_messages.push(OllamaMessage {
                role: "system".to_string(),
                content: system_prompt.to_string(),
                tool_calls: None,
            });
        }

        // Translate conversation messages
        for msg in messages {
            translate_message(msg, &mut ollama_messages);
        }

        // Translate tool definitions
        let ollama_tools = tools.map(|defs| {
            defs.iter()
                .map(|td| OllamaToolDef {
                    tool_type: "function".to_string(),
                    function: OllamaFunctionDef {
                        name: td.name.clone(),
                        description: td.description.clone(),
                        parameters: td.input_schema.clone(),
                    },
                })
                .collect()
        });

        let request = OllamaChatRequest {
            model: self.config.model.clone(),
            messages: ollama_messages,
            stream: false,
            tools: ollama_tools,
            options: OllamaOptions {
                num_predict: self.config.max_tokens_per_request,
            },
        };

        let url = format!("{}/api/chat", self.host);

        debug!(
            "Calling Ollama API ({}) with {} messages{}",
            self.config.model,
            messages.len(),
            if tools.is_some() { " + tools" } else { "" }
        );

        let response = self
            .client
            .post(&url)
            .json(&request)
            .send()
            .await?;

        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            anyhow::bail!("Ollama API error ({status}): {body}");
        }

        let resp: OllamaChatResponse = response.json().await?;

        // Extract text
        let text = resp.message.content.clone();

        // Extract tool calls (synthesize IDs since Ollama doesn't provide them)
        let tool_calls: Vec<ToolCall> = resp
            .message
            .tool_calls
            .as_ref()
            .map(|tcs| {
                tcs.iter()
                    .enumerate()
                    .map(|(i, tc)| ToolCall {
                        id: format!("ollama_tool_{i}"),
                        name: tc.function.name.clone(),
                        input: tc.function.arguments.clone(),
                    })
                    .collect()
            })
            .unwrap_or_default();

        // Build content_blocks for re-submission in the agentic loop
        let mut content_blocks = Vec::new();
        if !text.is_empty() {
            content_blocks.push(InputContentBlock::Text {
                text: text.clone(),
            });
        }
        for tc in &tool_calls {
            content_blocks.push(InputContentBlock::ToolUse {
                id: tc.id.clone(),
                name: tc.name.clone(),
                input: tc.input.clone(),
            });
        }

        // Determine stop reason
        let stop_reason = if !tool_calls.is_empty() {
            StopReason::ToolUse
        } else {
            match resp.done_reason.as_deref() {
                Some("stop") | None => StopReason::EndTurn,
                Some("length") => StopReason::MaxTokens,
                Some(other) => StopReason::Other(other.to_string()),
            }
        };

        let input_tokens = resp.prompt_eval_count.unwrap_or(0);
        let output_tokens = resp.eval_count.unwrap_or(0);

        info!("LLM response: {input_tokens} in / {output_tokens} out tokens");

        Ok(LlmResponse {
            text,
            tool_calls,
            stop_reason,
            input_tokens,
            output_tokens,
            content_blocks,
        })
    }

    fn description(&self) -> String {
        format!("{} ({})", self.config.provider, self.config.model)
    }
}

// ── Message translation helpers ──────────────────────────

/// Translates a shared `Message` into one or more `OllamaMessage`s.
///
/// Handles the differences between Anthropic and Ollama message formats:
/// - `ToolUse` blocks in assistant messages → `tool_calls` field
/// - `ToolResult` blocks in user messages → `role: "tool"` messages
/// - Image/Document blocks → filtered out with text placeholder
fn translate_message(msg: &Message, out: &mut Vec<OllamaMessage>) {
    match &msg.content {
        MessageContent::Text(text) => {
            out.push(OllamaMessage {
                role: msg.role.clone(),
                content: text.clone(),
                tool_calls: None,
            });
        }
        MessageContent::Blocks(blocks) => {
            // Check what kinds of blocks we have
            let mut text_parts = Vec::new();
            let mut tool_use_calls = Vec::new();
            let mut tool_results = Vec::new();
            let mut has_multimodal = false;

            for block in blocks {
                match block {
                    InputContentBlock::Text { text } => {
                        text_parts.push(text.clone());
                    }
                    InputContentBlock::ToolUse { id: _, name, input } => {
                        tool_use_calls.push(OllamaToolCall {
                            function: OllamaFunctionCall {
                                name: name.clone(),
                                arguments: input.clone(),
                            },
                        });
                    }
                    InputContentBlock::ToolResult {
                        tool_use_id: _,
                        content,
                    } => {
                        tool_results.push(content.clone());
                    }
                    InputContentBlock::Image { .. } | InputContentBlock::Document { .. } => {
                        has_multimodal = true;
                    }
                }
            }

            if has_multimodal {
                warn!("Ollama does not support multi-modal content blocks; images/documents will be skipped");
                text_parts.push("[Unsupported: image/document content omitted]".to_string());
            }

            // Assistant message with tool calls
            if !tool_use_calls.is_empty() {
                out.push(OllamaMessage {
                    role: "assistant".to_string(),
                    content: text_parts.join("\n"),
                    tool_calls: Some(tool_use_calls),
                });
                return;
            }

            // Tool results → each becomes a separate "tool" message
            if !tool_results.is_empty() {
                for result in tool_results {
                    out.push(OllamaMessage {
                        role: "tool".to_string(),
                        content: result,
                        tool_calls: None,
                    });
                }
                return;
            }

            // Plain text blocks
            out.push(OllamaMessage {
                role: msg.role.clone(),
                content: text_parts.join("\n"),
                tool_calls: None,
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::{DocumentSource, ImageSource};

    // ── OllamaClient::description() ─────────────────────

    #[test]
    fn test_description() {
        let client = OllamaClient::new(LlmConfig {
            provider: "ollama".to_string(),
            model: "llama3.2".to_string(),
            api_key: String::new(),
            max_tokens_per_request: 4096,
            host: None,
        });
        assert_eq!(client.description(), "ollama (llama3.2)");
    }

    #[test]
    fn test_default_host() {
        let client = OllamaClient::new(LlmConfig {
            provider: "ollama".to_string(),
            model: "llama3.2".to_string(),
            api_key: String::new(),
            max_tokens_per_request: 4096,
            host: None,
        });
        assert_eq!(client.host, "http://localhost:11434");
    }

    #[test]
    fn test_custom_host() {
        let client = OllamaClient::new(LlmConfig {
            provider: "ollama".to_string(),
            model: "llama3.2".to_string(),
            api_key: String::new(),
            max_tokens_per_request: 4096,
            host: Some("http://myserver:11434/".to_string()),
        });
        // Trailing slash should be stripped
        assert_eq!(client.host, "http://myserver:11434");
    }

    // ── Tool definition translation ──────────────────────

    #[test]
    fn test_tool_definition_translation() {
        let td = ToolDefinition {
            name: "web_search".to_string(),
            description: "Search the web".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "query": {"type": "string"}
                },
                "required": ["query"]
            }),
        };

        let ollama_tool = OllamaToolDef {
            tool_type: "function".to_string(),
            function: OllamaFunctionDef {
                name: td.name.clone(),
                description: td.description.clone(),
                parameters: td.input_schema.clone(),
            },
        };

        let json = serde_json::to_value(&ollama_tool).unwrap();
        assert_eq!(json["type"], "function");
        assert_eq!(json["function"]["name"], "web_search");
        assert_eq!(json["function"]["description"], "Search the web");
        assert_eq!(json["function"]["parameters"]["type"], "object");
    }

    // ── Message translation ──────────────────────────────

    #[test]
    fn test_translate_text_message() {
        let msg = Message {
            role: "user".to_string(),
            content: MessageContent::Text("Hello!".to_string()),
        };
        let mut out = Vec::new();
        translate_message(&msg, &mut out);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].role, "user");
        assert_eq!(out[0].content, "Hello!");
        assert!(out[0].tool_calls.is_none());
    }

    #[test]
    fn test_translate_assistant_tool_use() {
        let msg = Message {
            role: "assistant".to_string(),
            content: MessageContent::Blocks(vec![
                InputContentBlock::Text {
                    text: "Let me search.".to_string(),
                },
                InputContentBlock::ToolUse {
                    id: "tool_1".to_string(),
                    name: "web_search".to_string(),
                    input: serde_json::json!({"query": "rust"}),
                },
            ]),
        };
        let mut out = Vec::new();
        translate_message(&msg, &mut out);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].role, "assistant");
        assert_eq!(out[0].content, "Let me search.");
        let tcs = out[0].tool_calls.as_ref().unwrap();
        assert_eq!(tcs.len(), 1);
        assert_eq!(tcs[0].function.name, "web_search");
        assert_eq!(tcs[0].function.arguments["query"], "rust");
    }

    #[test]
    fn test_translate_tool_result() {
        let msg = Message {
            role: "user".to_string(),
            content: MessageContent::Blocks(vec![
                InputContentBlock::ToolResult {
                    tool_use_id: "tool_1".to_string(),
                    content: "Found 5 results.".to_string(),
                },
            ]),
        };
        let mut out = Vec::new();
        translate_message(&msg, &mut out);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].role, "tool");
        assert_eq!(out[0].content, "Found 5 results.");
    }

    #[test]
    fn test_translate_multiple_tool_results() {
        let msg = Message {
            role: "user".to_string(),
            content: MessageContent::Blocks(vec![
                InputContentBlock::ToolResult {
                    tool_use_id: "tool_1".to_string(),
                    content: "Result 1.".to_string(),
                },
                InputContentBlock::ToolResult {
                    tool_use_id: "tool_2".to_string(),
                    content: "Result 2.".to_string(),
                },
            ]),
        };
        let mut out = Vec::new();
        translate_message(&msg, &mut out);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].role, "tool");
        assert_eq!(out[0].content, "Result 1.");
        assert_eq!(out[1].role, "tool");
        assert_eq!(out[1].content, "Result 2.");
    }

    #[test]
    fn test_translate_multimodal_filtered() {
        let msg = Message {
            role: "user".to_string(),
            content: MessageContent::Blocks(vec![
                InputContentBlock::Text {
                    text: "What is this?".to_string(),
                },
                InputContentBlock::Image {
                    source: ImageSource {
                        source_type: "base64".to_string(),
                        media_type: "image/jpeg".to_string(),
                        data: "aGVsbG8=".to_string(),
                    },
                },
            ]),
        };
        let mut out = Vec::new();
        translate_message(&msg, &mut out);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].role, "user");
        assert!(out[0].content.contains("What is this?"));
        assert!(out[0].content.contains("[Unsupported:"));
    }

    #[test]
    fn test_translate_document_filtered() {
        let msg = Message {
            role: "user".to_string(),
            content: MessageContent::Blocks(vec![InputContentBlock::Document {
                source: DocumentSource {
                    source_type: "base64".to_string(),
                    media_type: "application/pdf".to_string(),
                    data: "cGRm".to_string(),
                },
            }]),
        };
        let mut out = Vec::new();
        translate_message(&msg, &mut out);
        assert_eq!(out.len(), 1);
        assert!(out[0].content.contains("[Unsupported:"));
    }

    // ── Response parsing ─────────────────────────────────

    #[test]
    fn test_response_parsing_text_only() {
        let json = r#"{
            "message": {"role": "assistant", "content": "Hello!"},
            "done_reason": "stop",
            "prompt_eval_count": 100,
            "eval_count": 50
        }"#;
        let resp: OllamaChatResponse = serde_json::from_str(json).unwrap();
        assert_eq!(resp.message.content, "Hello!");
        assert!(resp.message.tool_calls.is_none());
        assert_eq!(resp.done_reason.as_deref(), Some("stop"));
        assert_eq!(resp.prompt_eval_count, Some(100));
        assert_eq!(resp.eval_count, Some(50));
    }

    #[test]
    fn test_response_parsing_with_tool_calls() {
        let json = r#"{
            "message": {
                "role": "assistant",
                "content": "",
                "tool_calls": [
                    {
                        "function": {
                            "name": "web_search",
                            "arguments": {"query": "rust lang"}
                        }
                    }
                ]
            },
            "done_reason": "stop",
            "prompt_eval_count": 80,
            "eval_count": 20
        }"#;
        let resp: OllamaChatResponse = serde_json::from_str(json).unwrap();
        assert_eq!(resp.message.content, "");
        let tcs = resp.message.tool_calls.as_ref().unwrap();
        assert_eq!(tcs.len(), 1);
        assert_eq!(tcs[0].function.name, "web_search");
        assert_eq!(tcs[0].function.arguments["query"], "rust lang");
    }

    #[test]
    fn test_response_parsing_missing_optional_fields() {
        let json = r#"{
            "message": {"role": "assistant", "content": "Hi!"}
        }"#;
        let resp: OllamaChatResponse = serde_json::from_str(json).unwrap();
        assert_eq!(resp.message.content, "Hi!");
        assert!(resp.done_reason.is_none());
        assert!(resp.prompt_eval_count.is_none());
        assert!(resp.eval_count.is_none());
    }

    // ── Request serialization ────────────────────────────

    #[test]
    fn test_request_serialization_without_tools() {
        let request = OllamaChatRequest {
            model: "llama3.2".to_string(),
            messages: vec![OllamaMessage {
                role: "user".to_string(),
                content: "Hello".to_string(),
                tool_calls: None,
            }],
            stream: false,
            tools: None,
            options: OllamaOptions { num_predict: 4096 },
        };
        let json = serde_json::to_value(&request).unwrap();
        assert_eq!(json["model"], "llama3.2");
        assert_eq!(json["stream"], false);
        assert!(json.get("tools").is_none());
        assert_eq!(json["options"]["num_predict"], 4096);
    }

    #[test]
    fn test_request_serialization_with_tools() {
        let request = OllamaChatRequest {
            model: "llama3.2".to_string(),
            messages: vec![],
            stream: false,
            tools: Some(vec![OllamaToolDef {
                tool_type: "function".to_string(),
                function: OllamaFunctionDef {
                    name: "test".to_string(),
                    description: "A test tool".to_string(),
                    parameters: serde_json::json!({"type": "object"}),
                },
            }]),
            options: OllamaOptions { num_predict: 4096 },
        };
        let json = serde_json::to_value(&request).unwrap();
        let tools = json["tools"].as_array().unwrap();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0]["type"], "function");
        assert_eq!(tools[0]["function"]["name"], "test");
    }

    #[test]
    fn test_ollama_message_with_tool_calls_serialization() {
        let msg = OllamaMessage {
            role: "assistant".to_string(),
            content: String::new(),
            tool_calls: Some(vec![OllamaToolCall {
                function: OllamaFunctionCall {
                    name: "web_search".to_string(),
                    arguments: serde_json::json!({"query": "test"}),
                },
            }]),
        };
        let json = serde_json::to_value(&msg).unwrap();
        assert_eq!(json["role"], "assistant");
        let tcs = json["tool_calls"].as_array().unwrap();
        assert_eq!(tcs[0]["function"]["name"], "web_search");
    }

    #[test]
    fn test_ollama_message_without_tool_calls_omits_field() {
        let msg = OllamaMessage {
            role: "user".to_string(),
            content: "Hello".to_string(),
            tool_calls: None,
        };
        let json = serde_json::to_value(&msg).unwrap();
        assert!(json.get("tool_calls").is_none());
    }

    // ── Stop reason mapping ──────────────────────────────

    #[test]
    fn test_stop_reason_with_tool_calls() {
        // When tool_calls are present, stop_reason should be ToolUse
        // regardless of done_reason
        let resp = OllamaChatResponse {
            message: OllamaResponseMessage {
                content: String::new(),
                tool_calls: Some(vec![OllamaToolCall {
                    function: OllamaFunctionCall {
                        name: "test".to_string(),
                        arguments: serde_json::json!({}),
                    },
                }]),
            },
            done_reason: Some("stop".to_string()),
            prompt_eval_count: None,
            eval_count: None,
        };

        let tool_calls: Vec<ToolCall> = resp
            .message
            .tool_calls
            .as_ref()
            .map(|tcs| {
                tcs.iter()
                    .enumerate()
                    .map(|(i, tc)| ToolCall {
                        id: format!("ollama_tool_{i}"),
                        name: tc.function.name.clone(),
                        input: tc.function.arguments.clone(),
                    })
                    .collect()
            })
            .unwrap_or_default();

        let stop_reason = if !tool_calls.is_empty() {
            StopReason::ToolUse
        } else {
            StopReason::EndTurn
        };

        assert_eq!(stop_reason, StopReason::ToolUse);
        assert_eq!(tool_calls[0].id, "ollama_tool_0");
    }

    #[test]
    fn test_stop_reason_length() {
        // done_reason "length" → MaxTokens
        let done_reason = Some("length".to_string());
        let stop_reason = match done_reason.as_deref() {
            Some("stop") | None => StopReason::EndTurn,
            Some("length") => StopReason::MaxTokens,
            Some(other) => StopReason::Other(other.to_string()),
        };
        assert_eq!(stop_reason, StopReason::MaxTokens);
    }

    #[test]
    fn test_stop_reason_unknown() {
        let done_reason = Some("load".to_string());
        let stop_reason = match done_reason.as_deref() {
            Some("stop") | None => StopReason::EndTurn,
            Some("length") => StopReason::MaxTokens,
            Some(other) => StopReason::Other(other.to_string()),
        };
        assert_eq!(stop_reason, StopReason::Other("load".to_string()));
    }

    #[test]
    fn test_synthesized_tool_ids() {
        let tcs = vec![
            OllamaToolCall {
                function: OllamaFunctionCall {
                    name: "tool_a".to_string(),
                    arguments: serde_json::json!({}),
                },
            },
            OllamaToolCall {
                function: OllamaFunctionCall {
                    name: "tool_b".to_string(),
                    arguments: serde_json::json!({}),
                },
            },
        ];

        let tool_calls: Vec<ToolCall> = tcs
            .iter()
            .enumerate()
            .map(|(i, tc)| ToolCall {
                id: format!("ollama_tool_{i}"),
                name: tc.function.name.clone(),
                input: tc.function.arguments.clone(),
            })
            .collect();

        assert_eq!(tool_calls[0].id, "ollama_tool_0");
        assert_eq!(tool_calls[0].name, "tool_a");
        assert_eq!(tool_calls[1].id, "ollama_tool_1");
        assert_eq!(tool_calls[1].name, "tool_b");
    }
}
