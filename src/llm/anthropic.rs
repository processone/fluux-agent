use anyhow::Result;
use async_trait::async_trait;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use tracing::{debug, info};

use crate::config::LlmConfig;
use super::client::LlmClient;

/// Client for Anthropic Messages API
#[derive(Clone)]
pub struct AnthropicClient {
    client: Client,
    config: LlmConfig,
}

#[derive(Debug, Serialize)]
struct MessagesRequest {
    model: String,
    max_tokens: u32,
    system: String,
    messages: Vec<Message>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tools: Option<Vec<ToolDefinition>>,
}

/// A message in the conversation (sent to the API).
///
/// `content` can be either a plain text string or an array of content blocks
/// for multi-modal messages (text + images + documents).
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct Message {
    pub role: String,
    pub content: MessageContent,
}

/// Message content — either a plain string or structured content blocks.
///
/// `#[serde(untagged)]` ensures:
/// - `Text("hello")` serializes as `"hello"` (plain JSON string)
/// - `Blocks([...])` serializes as `[{"type": "text", ...}, ...]` (JSON array)
///
/// This is backward-compatible with the Anthropic Messages API which accepts
/// both formats.
#[derive(Debug, Serialize, Deserialize, Clone, PartialEq)]
#[serde(untagged)]
pub enum MessageContent {
    /// Plain text content (most messages)
    Text(String),
    /// Structured content blocks (multi-modal: text + images + documents)
    Blocks(Vec<InputContentBlock>),
}

impl MessageContent {
    /// Returns the text content, if this is a Text variant.
    /// For Blocks, returns None.
    pub fn as_text(&self) -> Option<&str> {
        match self {
            MessageContent::Text(s) => Some(s),
            MessageContent::Blocks(_) => None,
        }
    }

    /// Returns true if this is a text content that contains the given substring.
    pub fn contains(&self, pattern: &str) -> bool {
        match self {
            MessageContent::Text(s) => s.contains(pattern),
            MessageContent::Blocks(_) => false,
        }
    }
}

/// Allow `msg.content == "hello"` comparison for convenience in tests and assertions.
impl PartialEq<&str> for MessageContent {
    fn eq(&self, other: &&str) -> bool {
        match self {
            MessageContent::Text(s) => s == *other,
            MessageContent::Blocks(_) => false,
        }
    }
}

impl From<String> for MessageContent {
    fn from(s: String) -> Self {
        MessageContent::Text(s)
    }
}

impl From<&str> for MessageContent {
    fn from(s: &str) -> Self {
        MessageContent::Text(s.to_string())
    }
}

/// Input content block for the Anthropic Messages API.
/// Used in request messages for multi-modal content.
#[derive(Debug, Serialize, Deserialize, Clone, PartialEq)]
#[serde(tag = "type")]
pub enum InputContentBlock {
    /// Text content block
    #[serde(rename = "text")]
    Text { text: String },
    /// Image content block (base64-encoded or URL)
    #[serde(rename = "image")]
    Image { source: ImageSource },
    /// Document content block (base64-encoded PDF or URL)
    #[serde(rename = "document")]
    Document { source: DocumentSource },
    /// Tool use block (from assistant messages in the agentic loop).
    /// Serializes to: `{"type": "tool_use", "id": "...", "name": "...", "input": {...}}`
    #[serde(rename = "tool_use")]
    ToolUse {
        id: String,
        name: String,
        input: serde_json::Value,
    },
    /// Tool result block (from user messages in the agentic loop).
    /// Serializes to: `{"type": "tool_result", "tool_use_id": "...", "content": "..."}`
    #[serde(rename = "tool_result")]
    ToolResult {
        tool_use_id: String,
        content: String,
    },
}

/// Source for an image content block (base64-encoded).
#[derive(Debug, Serialize, Deserialize, Clone, PartialEq)]
pub struct ImageSource {
    #[serde(rename = "type")]
    pub source_type: String, // "base64"
    pub media_type: String, // "image/jpeg", "image/png", "image/gif", "image/webp"
    pub data: String,       // base64-encoded image bytes
}

/// Source for a document content block (base64-encoded PDF).
#[derive(Debug, Serialize, Deserialize, Clone, PartialEq)]
pub struct DocumentSource {
    #[serde(rename = "type")]
    pub source_type: String, // "base64"
    pub media_type: String, // "application/pdf"
    pub data: String,       // base64-encoded document bytes
}

// ── Tool definition (for API `tools[]` parameter) ────────

/// Tool definition for the Anthropic Messages API `tools[]` parameter.
///
/// Serializes to:
/// ```json
/// {"name": "web_search", "description": "...", "input_schema": {...}}
/// ```
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ToolDefinition {
    /// Tool name (matches `Skill::name()`)
    pub name: String,
    /// Human-readable description (matches `Skill::description()`)
    pub description: String,
    /// JSON Schema for accepted parameters (matches `Skill::parameters_schema()`)
    pub input_schema: serde_json::Value,
}

// ── Response types (from API) ─────────────────────────────

#[derive(Debug, Deserialize)]
struct MessagesResponse {
    content: Vec<ResponseContentBlock>,
    stop_reason: Option<String>,
    usage: Option<Usage>,
}

/// Content block in an API response.
/// The Anthropic API returns text and/or tool_use blocks.
#[derive(Debug, Deserialize)]
#[serde(tag = "type")]
enum ResponseContentBlock {
    #[serde(rename = "text")]
    Text { text: String },
    #[serde(rename = "tool_use")]
    ToolUse {
        id: String,
        name: String,
        input: serde_json::Value,
    },
}

#[derive(Debug, Deserialize)]
struct Usage {
    input_tokens: u32,
    output_tokens: u32,
}

/// LLM response with metadata and optional tool calls.
#[derive(Debug)]
pub struct LlmResponse {
    /// Concatenated text from all text content blocks.
    pub text: String,
    /// Tool calls requested by the LLM (empty if stop_reason != ToolUse).
    pub tool_calls: Vec<ToolCall>,
    /// Why the model stopped generating.
    pub stop_reason: StopReason,
    /// Input tokens consumed by this API call.
    pub input_tokens: u32,
    /// Output tokens generated by this API call.
    pub output_tokens: u32,
    /// Raw response content blocks for re-submission in the agentic loop.
    pub content_blocks: Vec<InputContentBlock>,
}

/// A tool invocation requested by the LLM.
#[derive(Debug, Clone)]
pub struct ToolCall {
    /// Unique identifier for this tool use (from the API response).
    /// Must be sent back in the corresponding tool_result.
    pub id: String,
    /// Tool name (matches a registered `Skill::name()`).
    pub name: String,
    /// Parameters for the tool, as a JSON object.
    pub input: serde_json::Value,
}

/// Why the LLM stopped generating.
#[derive(Debug, Clone, PartialEq)]
pub enum StopReason {
    /// Normal completion — the model is done.
    EndTurn,
    /// The model wants to call one or more tools.
    ToolUse,
    /// The response hit the max_tokens limit.
    MaxTokens,
    /// Unknown or unexpected stop reason.
    Other(String),
}

impl AnthropicClient {
    pub fn new(config: LlmConfig) -> Self {
        let client = Client::new();
        Self { client, config }
    }
}

#[async_trait]
impl LlmClient for AnthropicClient {
    async fn complete(
        &self,
        system_prompt: &str,
        messages: &[Message],
        tools: Option<&[ToolDefinition]>,
    ) -> Result<LlmResponse> {
        let request = MessagesRequest {
            model: self.config.model.clone(),
            max_tokens: self.config.max_tokens_per_request,
            system: system_prompt.to_string(),
            messages: messages.to_vec(),
            tools: tools.map(|t| t.to_vec()),
        };

        debug!(
            "Calling Claude API ({}) with {} messages{}",
            self.config.model,
            messages.len(),
            if tools.is_some() { " + tools" } else { "" }
        );

        let response = self
            .client
            .post("https://api.anthropic.com/v1/messages")
            .header("x-api-key", &self.config.api_key)
            .header("anthropic-version", "2023-06-01")
            .header("content-type", "application/json")
            .json(&request)
            .send()
            .await?;

        let status = response.status();
        if !status.is_success() {
            let body = response.text().await?;
            anyhow::bail!("Claude API error ({status}): {body}");
        }

        let resp: MessagesResponse = response.json().await?;

        // Parse response content blocks into text, tool calls, and
        // InputContentBlock copies for re-submission in the agentic loop.
        let mut text_parts = Vec::new();
        let mut tool_calls = Vec::new();
        let mut content_blocks = Vec::new();

        for block in &resp.content {
            match block {
                ResponseContentBlock::Text { text } => {
                    text_parts.push(text.clone());
                    content_blocks.push(InputContentBlock::Text {
                        text: text.clone(),
                    });
                }
                ResponseContentBlock::ToolUse { id, name, input } => {
                    tool_calls.push(ToolCall {
                        id: id.clone(),
                        name: name.clone(),
                        input: input.clone(),
                    });
                    content_blocks.push(InputContentBlock::ToolUse {
                        id: id.clone(),
                        name: name.clone(),
                        input: input.clone(),
                    });
                }
            }
        }

        let text = text_parts.join("\n");

        let stop_reason = match resp.stop_reason.as_deref() {
            Some("end_turn") => StopReason::EndTurn,
            Some("tool_use") => StopReason::ToolUse,
            Some("max_tokens") => StopReason::MaxTokens,
            Some(other) => StopReason::Other(other.to_string()),
            None => StopReason::EndTurn,
        };

        let (input_tokens, output_tokens) = resp
            .usage
            .map(|u| (u.input_tokens, u.output_tokens))
            .unwrap_or((0, 0));

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_message_content_text_serializes_as_string() {
        let msg = Message {
            role: "user".to_string(),
            content: MessageContent::Text("Hello!".to_string()),
        };
        let json = serde_json::to_value(&msg).unwrap();
        assert_eq!(json["content"], "Hello!");
    }

    #[test]
    fn test_message_content_blocks_serializes_as_array() {
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
        let json = serde_json::to_value(&msg).unwrap();
        let content = json["content"].as_array().unwrap();
        assert_eq!(content.len(), 2);
        assert_eq!(content[0]["type"], "text");
        assert_eq!(content[0]["text"], "What is this?");
        assert_eq!(content[1]["type"], "image");
        assert_eq!(content[1]["source"]["type"], "base64");
        assert_eq!(content[1]["source"]["media_type"], "image/jpeg");
    }

    #[test]
    fn test_message_content_document_block() {
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
        let json = serde_json::to_value(&msg).unwrap();
        let content = json["content"].as_array().unwrap();
        assert_eq!(content[0]["type"], "document");
        assert_eq!(content[0]["source"]["media_type"], "application/pdf");
    }

    #[test]
    fn test_message_content_from_string() {
        let content: MessageContent = "Hello".to_string().into();
        assert!(matches!(content, MessageContent::Text(s) if s == "Hello"));
    }

    #[test]
    fn test_message_content_from_str() {
        let content: MessageContent = "Hello".into();
        assert!(matches!(content, MessageContent::Text(s) if s == "Hello"));
    }

    #[test]
    fn test_message_content_as_text() {
        let text = MessageContent::Text("Hello".to_string());
        assert_eq!(text.as_text(), Some("Hello"));

        let blocks = MessageContent::Blocks(vec![]);
        assert_eq!(blocks.as_text(), None);
    }

    #[test]
    fn test_message_content_text_deserializes_from_string() {
        let json = r#"{"role":"user","content":"Hello"}"#;
        let msg: Message = serde_json::from_str(json).unwrap();
        assert!(matches!(msg.content, MessageContent::Text(s) if s == "Hello"));
    }

    #[test]
    fn test_message_content_blocks_deserializes_from_array() {
        let json = r#"{"role":"user","content":[{"type":"text","text":"Hi"}]}"#;
        let msg: Message = serde_json::from_str(json).unwrap();
        match msg.content {
            MessageContent::Blocks(blocks) => {
                assert_eq!(blocks.len(), 1);
                match &blocks[0] {
                    InputContentBlock::Text { text } => assert_eq!(text, "Hi"),
                    _ => panic!("Expected text block"),
                }
            }
            _ => panic!("Expected Blocks variant"),
        }
    }

    // ── ToolDefinition tests ────────────────────────────

    #[test]
    fn test_tool_definition_serializes_to_api_format() {
        let tool = ToolDefinition {
            name: "web_search".to_string(),
            description: "Search the web".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "query": {"type": "string", "description": "Search query"}
                },
                "required": ["query"]
            }),
        };
        let json = serde_json::to_value(&tool).unwrap();
        assert_eq!(json["name"], "web_search");
        assert_eq!(json["description"], "Search the web");
        assert_eq!(json["input_schema"]["type"], "object");
        assert_eq!(
            json["input_schema"]["properties"]["query"]["type"],
            "string"
        );
        assert_eq!(json["input_schema"]["required"][0], "query");
    }

    #[test]
    fn test_tool_definition_roundtrip() {
        let tool = ToolDefinition {
            name: "hello".to_string(),
            description: "Says hello".to_string(),
            input_schema: serde_json::json!({"type": "object", "properties": {}}),
        };
        let json = serde_json::to_string(&tool).unwrap();
        let parsed: ToolDefinition = serde_json::from_str(&json).unwrap();
        assert_eq!(tool, parsed);
    }

    // ── InputContentBlock ToolUse/ToolResult tests ───────

    #[test]
    fn test_input_content_block_tool_use_serialization() {
        let block = InputContentBlock::ToolUse {
            id: "tool_abc123".to_string(),
            name: "web_search".to_string(),
            input: serde_json::json!({"query": "rust async"}),
        };
        let json = serde_json::to_value(&block).unwrap();
        assert_eq!(json["type"], "tool_use");
        assert_eq!(json["id"], "tool_abc123");
        assert_eq!(json["name"], "web_search");
        assert_eq!(json["input"]["query"], "rust async");
    }

    #[test]
    fn test_input_content_block_tool_use_roundtrip() {
        let block = InputContentBlock::ToolUse {
            id: "tool_xyz".to_string(),
            name: "url_fetch".to_string(),
            input: serde_json::json!({"url": "https://example.com"}),
        };
        let json = serde_json::to_string(&block).unwrap();
        let parsed: InputContentBlock = serde_json::from_str(&json).unwrap();
        assert_eq!(block, parsed);
    }

    #[test]
    fn test_input_content_block_tool_result_serialization() {
        let block = InputContentBlock::ToolResult {
            tool_use_id: "tool_abc123".to_string(),
            content: "Search results: Rust is great".to_string(),
        };
        let json = serde_json::to_value(&block).unwrap();
        assert_eq!(json["type"], "tool_result");
        assert_eq!(json["tool_use_id"], "tool_abc123");
        assert_eq!(json["content"], "Search results: Rust is great");
    }

    #[test]
    fn test_input_content_block_tool_result_roundtrip() {
        let block = InputContentBlock::ToolResult {
            tool_use_id: "tool_xyz".to_string(),
            content: "Result text".to_string(),
        };
        let json = serde_json::to_string(&block).unwrap();
        let parsed: InputContentBlock = serde_json::from_str(&json).unwrap();
        assert_eq!(block, parsed);
    }

    #[test]
    fn test_message_with_tool_use_blocks_serializes() {
        let msg = Message {
            role: "assistant".to_string(),
            content: MessageContent::Blocks(vec![
                InputContentBlock::Text {
                    text: "Let me search for that.".to_string(),
                },
                InputContentBlock::ToolUse {
                    id: "tool_1".to_string(),
                    name: "web_search".to_string(),
                    input: serde_json::json!({"query": "hello"}),
                },
            ]),
        };
        let json = serde_json::to_value(&msg).unwrap();
        assert_eq!(json["role"], "assistant");
        let blocks = json["content"].as_array().unwrap();
        assert_eq!(blocks.len(), 2);
        assert_eq!(blocks[0]["type"], "text");
        assert_eq!(blocks[1]["type"], "tool_use");
        assert_eq!(blocks[1]["id"], "tool_1");
    }

    #[test]
    fn test_message_with_tool_result_blocks_serializes() {
        let msg = Message {
            role: "user".to_string(),
            content: MessageContent::Blocks(vec![InputContentBlock::ToolResult {
                tool_use_id: "tool_1".to_string(),
                content: "Found 5 results".to_string(),
            }]),
        };
        let json = serde_json::to_value(&msg).unwrap();
        assert_eq!(json["role"], "user");
        let blocks = json["content"].as_array().unwrap();
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0]["type"], "tool_result");
        assert_eq!(blocks[0]["tool_use_id"], "tool_1");
    }

    // ── ResponseContentBlock deserialization tests ────────

    #[test]
    fn test_response_content_block_text_deserializes() {
        let json = r#"{"type": "text", "text": "Hello world"}"#;
        let block: ResponseContentBlock = serde_json::from_str(json).unwrap();
        match block {
            ResponseContentBlock::Text { text } => assert_eq!(text, "Hello world"),
            _ => panic!("Expected Text variant"),
        }
    }

    #[test]
    fn test_response_content_block_tool_use_deserializes() {
        let json = r#"{
            "type": "tool_use",
            "id": "toolu_01abc",
            "name": "web_search",
            "input": {"query": "rust programming"}
        }"#;
        let block: ResponseContentBlock = serde_json::from_str(json).unwrap();
        match block {
            ResponseContentBlock::ToolUse { id, name, input } => {
                assert_eq!(id, "toolu_01abc");
                assert_eq!(name, "web_search");
                assert_eq!(input["query"], "rust programming");
            }
            _ => panic!("Expected ToolUse variant"),
        }
    }

    // ── MessagesRequest serialization tests ──────────────

    #[test]
    fn test_messages_request_without_tools_omits_field() {
        let request = MessagesRequest {
            model: "claude-sonnet-4-5-20250929".to_string(),
            max_tokens: 4096,
            system: "You are helpful.".to_string(),
            messages: vec![],
            tools: None,
        };
        let json = serde_json::to_value(&request).unwrap();
        assert!(json.get("tools").is_none());
    }

    #[test]
    fn test_messages_request_with_tools_includes_field() {
        let request = MessagesRequest {
            model: "claude-sonnet-4-5-20250929".to_string(),
            max_tokens: 4096,
            system: "You are helpful.".to_string(),
            messages: vec![],
            tools: Some(vec![ToolDefinition {
                name: "web_search".to_string(),
                description: "Search the web".to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "query": {"type": "string"}
                    },
                    "required": ["query"]
                }),
            }]),
        };
        let json = serde_json::to_value(&request).unwrap();
        let tools = json["tools"].as_array().unwrap();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0]["name"], "web_search");
    }
}
