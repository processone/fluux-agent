use anyhow::Result;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use tracing::{debug, info};

use crate::config::LlmConfig;

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

// ── Response types (from API) ─────────────────────────────

#[derive(Debug, Deserialize)]
struct MessagesResponse {
    content: Vec<ResponseContentBlock>,
    usage: Option<Usage>,
}

/// Content block in an API response (text only for now).
#[derive(Debug, Deserialize)]
struct ResponseContentBlock {
    #[serde(rename = "type")]
    block_type: String,
    text: Option<String>,
}

#[derive(Debug, Deserialize)]
struct Usage {
    input_tokens: u32,
    output_tokens: u32,
}

/// LLM response with metadata
#[derive(Debug)]
pub struct LlmResponse {
    pub text: String,
    pub input_tokens: u32,
    pub output_tokens: u32,
}

impl AnthropicClient {
    pub fn new(config: LlmConfig) -> Self {
        let client = Client::new();
        Self { client, config }
    }

    /// Sends a conversation to the LLM and returns the response
    pub async fn complete(
        &self,
        system_prompt: &str,
        messages: &[Message],
    ) -> Result<LlmResponse> {
        let request = MessagesRequest {
            model: self.config.model.clone(),
            max_tokens: self.config.max_tokens_per_request,
            system: system_prompt.to_string(),
            messages: messages.to_vec(),
        };

        debug!(
            "Calling Claude API ({}) with {} messages",
            self.config.model,
            messages.len()
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

        let text = resp
            .content
            .iter()
            .filter_map(|block| {
                if block.block_type == "text" {
                    block.text.clone()
                } else {
                    None
                }
            })
            .collect::<Vec<_>>()
            .join("\n");

        let (input_tokens, output_tokens) = resp
            .usage
            .map(|u| (u.input_tokens, u.output_tokens))
            .unwrap_or((0, 0));

        info!("LLM response: {input_tokens} in / {output_tokens} out tokens");

        Ok(LlmResponse {
            text,
            input_tokens,
            output_tokens,
        })
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
}
