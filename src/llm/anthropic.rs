use anyhow::Result;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use tracing::{debug, info};

use crate::config::LlmConfig;

/// Client for Anthropic Messages API
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

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct Message {
    pub role: String,
    pub content: String,
}

#[derive(Debug, Deserialize)]
struct MessagesResponse {
    content: Vec<ContentBlock>,
    usage: Option<Usage>,
}

#[derive(Debug, Deserialize)]
struct ContentBlock {
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
