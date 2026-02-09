//! Perplexity Sonar API provider.
//!
//! Calls `POST https://api.perplexity.ai/chat/completions` with an
//! OpenAI-compatible request format and Bearer token authentication.
//! Returns a chat completion with optional citations and structured
//! search results.

use async_trait::async_trait;
use reqwest::Client;
use serde::{Deserialize, Serialize};

use super::{SearchProvider, SearchResponse, SearchResult};

// ── Perplexity API types ─────────────────────────────────

/// Perplexity Sonar API request (OpenAI-compatible chat completions).
#[derive(Serialize)]
struct PerplexityRequest {
    model: String,
    messages: Vec<PerplexityMessage>,
}

#[derive(Serialize)]
struct PerplexityMessage {
    role: String,
    content: String,
}

/// Perplexity Sonar API response.
#[derive(Deserialize)]
pub(super) struct PerplexityApiResponse {
    pub choices: Vec<PerplexityChoice>,
    pub citations: Option<Vec<String>>,
    pub search_results: Option<Vec<PerplexitySearchResult>>,
}

#[derive(Deserialize)]
pub(super) struct PerplexityChoice {
    pub message: PerplexityChoiceMessage,
}

#[derive(Deserialize)]
pub(super) struct PerplexityChoiceMessage {
    pub content: String,
}

/// A search result from the Perplexity API response.
#[derive(Deserialize)]
pub(super) struct PerplexitySearchResult {
    pub title: String,
    pub url: String,
    #[serde(default)]
    pub snippet: Option<String>,
}

// ── PerplexityProvider ───────────────────────────────────

pub(super) struct PerplexityProvider {
    client: Client,
    api_key: String,
    model: String,
}

impl PerplexityProvider {
    pub fn new(api_key: &str, model: &str) -> Self {
        Self {
            client: Client::new(),
            api_key: api_key.to_string(),
            model: model.to_string(),
        }
    }
}

#[async_trait]
impl SearchProvider for PerplexityProvider {
    async fn search(&self, query: &str) -> anyhow::Result<SearchResponse> {
        let request = PerplexityRequest {
            model: self.model.clone(),
            messages: vec![PerplexityMessage {
                role: "user".to_string(),
                content: query.to_string(),
            }],
        };

        let response = self
            .client
            .post("https://api.perplexity.ai/chat/completions")
            .header("Authorization", format!("Bearer {}", self.api_key))
            .json(&request)
            .send()
            .await?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            anyhow::bail!("Perplexity API returned {status}: {body}");
        }

        let pplx: PerplexityApiResponse = response.json().await?;

        // Extract the assistant's answer as the summary
        let summary = pplx
            .choices
            .first()
            .map(|c| c.message.content.clone());

        // Build results from search_results if present,
        // otherwise fall back to citations (URLs only).
        let results = if let Some(search_results) = pplx.search_results {
            search_results
                .into_iter()
                .map(|r| SearchResult {
                    title: r.title,
                    url: r.url,
                    snippet: r.snippet.unwrap_or_default(),
                })
                .collect()
        } else if let Some(citations) = pplx.citations {
            citations
                .into_iter()
                .enumerate()
                .map(|(i, url)| SearchResult {
                    title: format!("Source {}", i + 1),
                    url,
                    snippet: String::new(),
                })
                .collect()
        } else {
            vec![]
        };

        Ok(SearchResponse { summary, results })
    }

    fn provider_name(&self) -> &str {
        "perplexity"
    }

    fn capability(&self) -> String {
        "network:api.perplexity.ai:443".to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_perplexity_response_parsing_with_search_results() {
        let json = r#"{
            "choices": [{"message": {"role": "assistant", "content": "Rust is great."}}],
            "citations": ["https://rust-lang.org"],
            "search_results": [
                {"title": "Rust", "url": "https://rust-lang.org", "snippet": "A language."}
            ]
        }"#;
        let parsed: PerplexityApiResponse = serde_json::from_str(json).unwrap();
        assert_eq!(parsed.choices[0].message.content, "Rust is great.");
        assert_eq!(parsed.citations.as_ref().unwrap().len(), 1);
        assert_eq!(parsed.search_results.as_ref().unwrap().len(), 1);
        assert_eq!(parsed.search_results.as_ref().unwrap()[0].title, "Rust");
    }

    #[test]
    fn test_perplexity_response_parsing_citations_only() {
        let json = r#"{
            "choices": [{"message": {"role": "assistant", "content": "Answer."}}],
            "citations": ["https://a.com", "https://b.com"]
        }"#;
        let parsed: PerplexityApiResponse = serde_json::from_str(json).unwrap();
        assert!(parsed.search_results.is_none());
        assert_eq!(parsed.citations.as_ref().unwrap().len(), 2);
    }

    #[test]
    fn test_perplexity_response_parsing_no_citations() {
        let json = r#"{
            "choices": [{"message": {"role": "assistant", "content": "Answer."}}]
        }"#;
        let parsed: PerplexityApiResponse = serde_json::from_str(json).unwrap();
        assert!(parsed.search_results.is_none());
        assert!(parsed.citations.is_none());
    }

    /// Perplexity with search_results → prefer structured results over citations.
    #[test]
    fn test_perplexity_mapping_prefers_search_results() {
        let pplx = PerplexityApiResponse {
            choices: vec![PerplexityChoice {
                message: PerplexityChoiceMessage {
                    content: "Summary text.".to_string(),
                },
            }],
            citations: Some(vec!["https://a.com".to_string()]),
            search_results: Some(vec![
                PerplexitySearchResult {
                    title: "Structured".to_string(),
                    url: "https://structured.example".to_string(),
                    snippet: Some("Detailed snippet.".to_string()),
                },
            ]),
        };
        let summary = pplx.choices.first().map(|c| c.message.content.clone());
        let results = if let Some(search_results) = pplx.search_results {
            search_results
                .into_iter()
                .map(|r| SearchResult {
                    title: r.title,
                    url: r.url,
                    snippet: r.snippet.unwrap_or_default(),
                })
                .collect()
        } else if let Some(citations) = pplx.citations {
            citations
                .into_iter()
                .enumerate()
                .map(|(i, url)| SearchResult {
                    title: format!("Source {}", i + 1),
                    url,
                    snippet: String::new(),
                })
                .collect()
        } else {
            vec![]
        };
        let response = SearchResponse { summary, results };

        assert_eq!(response.summary.as_deref(), Some("Summary text."));
        assert_eq!(response.results.len(), 1);
        assert_eq!(response.results[0].title, "Structured");
        assert_eq!(response.results[0].snippet, "Detailed snippet.");
    }

    /// Perplexity with only citations (no search_results) falls back to
    /// synthetic results from citation URLs.
    #[test]
    fn test_perplexity_mapping_citations_fallback() {
        let pplx = PerplexityApiResponse {
            choices: vec![PerplexityChoice {
                message: PerplexityChoiceMessage {
                    content: "Answer.".to_string(),
                },
            }],
            citations: Some(vec![
                "https://a.com".to_string(),
                "https://b.com".to_string(),
            ]),
            search_results: None,
        };
        let summary = pplx.choices.first().map(|c| c.message.content.clone());
        let results = if let Some(search_results) = pplx.search_results {
            search_results
                .into_iter()
                .map(|r| SearchResult {
                    title: r.title,
                    url: r.url,
                    snippet: r.snippet.unwrap_or_default(),
                })
                .collect()
        } else if let Some(citations) = pplx.citations {
            citations
                .into_iter()
                .enumerate()
                .map(|(i, url)| SearchResult {
                    title: format!("Source {}", i + 1),
                    url,
                    snippet: String::new(),
                })
                .collect()
        } else {
            vec![]
        };
        let response = SearchResponse { summary, results };

        assert_eq!(response.results.len(), 2);
        assert_eq!(response.results[0].title, "Source 1");
        assert_eq!(response.results[0].url, "https://a.com");
        assert!(response.results[0].snippet.is_empty());
        assert_eq!(response.results[1].title, "Source 2");
    }

    /// Perplexity with neither search_results nor citations → empty results.
    #[test]
    fn test_perplexity_mapping_no_results_no_citations() {
        let pplx = PerplexityApiResponse {
            choices: vec![PerplexityChoice {
                message: PerplexityChoiceMessage {
                    content: "I don't know.".to_string(),
                },
            }],
            citations: None,
            search_results: None,
        };
        let summary = pplx.choices.first().map(|c| c.message.content.clone());
        let results: Vec<SearchResult> =
            if let Some(search_results) = pplx.search_results {
                search_results
                    .into_iter()
                    .map(|r| SearchResult {
                        title: r.title,
                        url: r.url,
                        snippet: r.snippet.unwrap_or_default(),
                    })
                    .collect()
            } else if let Some(citations) = pplx.citations {
                citations
                    .into_iter()
                    .enumerate()
                    .map(|(i, url)| SearchResult {
                        title: format!("Source {}", i + 1),
                        url,
                        snippet: String::new(),
                    })
                    .collect()
            } else {
                vec![]
            };
        let response = SearchResponse { summary, results };

        assert_eq!(response.summary.as_deref(), Some("I don't know."));
        assert!(response.results.is_empty());
    }

    /// Perplexity with empty choices → summary is None.
    #[test]
    fn test_perplexity_mapping_empty_choices() {
        let pplx = PerplexityApiResponse {
            choices: vec![],
            citations: Some(vec!["https://a.com".to_string()]),
            search_results: None,
        };
        let summary = pplx.choices.first().map(|c| c.message.content.clone());
        assert!(summary.is_none());
    }

    /// Perplexity search result with missing snippet defaults to empty string.
    #[test]
    fn test_perplexity_snippet_missing_defaults_to_empty() {
        let json = r#"{
            "choices": [{"message": {"role": "assistant", "content": "X."}}],
            "search_results": [
                {"title": "No Snippet", "url": "https://example.com"}
            ]
        }"#;
        let parsed: PerplexityApiResponse = serde_json::from_str(json).unwrap();
        let sr = &parsed.search_results.as_ref().unwrap()[0];
        assert!(sr.snippet.is_none());
        // When mapped, should become empty string
        let snippet = sr.snippet.clone().unwrap_or_default();
        assert!(snippet.is_empty());
    }
}
