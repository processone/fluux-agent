//! Tavily Search API provider.
//!
//! Calls `POST https://api.tavily.com/search` with the API key in
//! the request body.  Returns structured results with an optional
//! pre-built answer.

use async_trait::async_trait;
use reqwest::Client;
use serde::{Deserialize, Serialize};

use super::{SearchProvider, SearchResponse, SearchResult};

// ── Tavily API types ─────────────────────────────────────

/// Tavily Search API request body.
#[derive(Serialize)]
struct TavilyRequest<'a> {
    api_key: &'a str,
    query: &'a str,
    max_results: u8,
    include_answer: bool,
}

/// Tavily Search API response.
#[derive(Deserialize)]
pub(super) struct TavilyApiResponse {
    pub answer: Option<String>,
    pub results: Vec<TavilyApiResult>,
}

/// A single result from the Tavily API.
#[derive(Deserialize)]
pub(super) struct TavilyApiResult {
    pub title: String,
    pub url: String,
    pub content: String,
}

// ── TavilyProvider ───────────────────────────────────────

pub(super) struct TavilyProvider {
    client: Client,
    api_key: String,
    max_results: u8,
}

impl TavilyProvider {
    pub fn new(api_key: &str, max_results: u8) -> Self {
        Self {
            client: Client::new(),
            api_key: api_key.to_string(),
            max_results,
        }
    }
}

#[async_trait]
impl SearchProvider for TavilyProvider {
    async fn search(&self, query: &str) -> anyhow::Result<SearchResponse> {
        let request = TavilyRequest {
            api_key: &self.api_key,
            query,
            max_results: self.max_results,
            include_answer: true,
        };

        let response = self
            .client
            .post("https://api.tavily.com/search")
            .json(&request)
            .send()
            .await?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            anyhow::bail!("Tavily API returned {status}: {body}");
        }

        let tavily: TavilyApiResponse = response.json().await?;

        Ok(SearchResponse {
            summary: tavily.answer,
            results: tavily
                .results
                .into_iter()
                .map(|r| SearchResult {
                    title: r.title,
                    url: r.url,
                    snippet: r.content,
                })
                .collect(),
        })
    }

    fn provider_name(&self) -> &str {
        "tavily"
    }

    fn capability(&self) -> String {
        "network:api.tavily.com:443".to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_tavily_response_parsing() {
        let json = r#"{
            "answer": "Rust is great.",
            "results": [
                {"title": "Rust", "url": "https://rust-lang.org", "content": "A language."}
            ]
        }"#;
        let parsed: TavilyApiResponse = serde_json::from_str(json).unwrap();
        assert_eq!(parsed.answer.as_deref(), Some("Rust is great."));
        assert_eq!(parsed.results.len(), 1);
        assert_eq!(parsed.results[0].title, "Rust");
    }

    /// Verify that a Tavily API response maps correctly to the normalized
    /// SearchResponse (answer → summary, content → snippet).
    #[test]
    fn test_tavily_mapping_to_search_response() {
        let tavily = TavilyApiResponse {
            answer: Some("Concise answer.".to_string()),
            results: vec![
                TavilyApiResult {
                    title: "First".to_string(),
                    url: "https://first.example".to_string(),
                    content: "First content.".to_string(),
                },
                TavilyApiResult {
                    title: "Second".to_string(),
                    url: "https://second.example".to_string(),
                    content: "Second content.".to_string(),
                },
            ],
        };
        let response = SearchResponse {
            summary: tavily.answer,
            results: tavily
                .results
                .into_iter()
                .map(|r| SearchResult {
                    title: r.title,
                    url: r.url,
                    snippet: r.content,
                })
                .collect(),
        };
        assert_eq!(response.summary.as_deref(), Some("Concise answer."));
        assert_eq!(response.results.len(), 2);
        assert_eq!(response.results[0].title, "First");
        assert_eq!(response.results[0].snippet, "First content.");
        assert_eq!(response.results[1].url, "https://second.example");
    }

    /// Verify that a Tavily response with no answer maps summary to None.
    #[test]
    fn test_tavily_mapping_no_answer() {
        let json = r#"{
            "answer": null,
            "results": [
                {"title": "Only", "url": "https://only.example", "content": "Text."}
            ]
        }"#;
        let tavily: TavilyApiResponse = serde_json::from_str(json).unwrap();
        let response = SearchResponse {
            summary: tavily.answer,
            results: tavily
                .results
                .into_iter()
                .map(|r| SearchResult {
                    title: r.title,
                    url: r.url,
                    snippet: r.content,
                })
                .collect(),
        };
        assert!(response.summary.is_none());
        assert_eq!(response.results.len(), 1);
    }
}
