//! Builtin skill: web search via multiple providers.
//!
//! Gives the agent access to current information from the web.
//! The LLM invokes this skill when it needs up-to-date facts,
//! recent events, or information it doesn't have in its training data.
//!
//! Supported providers:
//! - **Tavily** — dedicated search API with structured results
//! - **Perplexity** — Sonar models with web-grounded chat completions

mod perplexity;
mod tavily;

use async_trait::async_trait;
use serde_json::{json, Value};
use tracing::{debug, warn};

use crate::config::WebSearchConfig;
use crate::skills::Skill;

use perplexity::PerplexityProvider;
use tavily::TavilyProvider;

// ── Normalized types (provider-agnostic) ─────────────────

/// A single search result, normalized across all providers.
pub(super) struct SearchResult {
    pub title: String,
    pub url: String,
    pub snippet: String,
}

/// Aggregated search response from any provider.
pub(super) struct SearchResponse {
    /// A pre-built summary/answer (if the provider returns one).
    pub summary: Option<String>,
    /// Individual search results.
    pub results: Vec<SearchResult>,
}

// ── SearchProvider trait ─────────────────────────────────

/// Abstraction over different web search backends.
///
/// Each provider implements this trait to normalize its API response
/// into a common `SearchResponse` structure.
#[async_trait]
pub(super) trait SearchProvider: Send + Sync {
    /// Perform a web search and return normalized results.
    async fn search(&self, query: &str) -> anyhow::Result<SearchResponse>;

    /// The provider name (e.g. `"tavily"`, `"perplexity"`).
    fn provider_name(&self) -> &str;

    /// The network capability declaration for this provider.
    fn capability(&self) -> String;
}

// ── WebSearchSkill ───────────────────────────────────────

/// Builtin web search skill supporting multiple providers.
///
/// The LLM can invoke this skill to search the web for current information.
/// Results are formatted as concise text suitable for LLM consumption.
pub struct WebSearchSkill {
    provider: Box<dyn SearchProvider>,
}

impl WebSearchSkill {
    /// Creates a new web search skill from configuration.
    ///
    /// The `provider` field in `config` determines which backend is used:
    /// - `"tavily"` — Tavily Search API
    /// - `"perplexity"` — Perplexity Sonar API
    ///
    /// # Panics
    ///
    /// Panics if the provider is not supported. This is validated at startup
    /// so misconfiguration is caught early.
    pub fn new(config: &WebSearchConfig) -> Self {
        let provider: Box<dyn SearchProvider> = match config.provider.as_str() {
            "tavily" => Box::new(TavilyProvider::new(&config.api_key, config.max_results)),
            "perplexity" => Box::new(PerplexityProvider::new(
                &config.api_key,
                config.model.as_deref().unwrap_or("sonar"),
            )),
            other => panic!(
                "Unsupported web search provider: '{other}'. \
                 Supported: 'tavily', 'perplexity'."
            ),
        };

        Self { provider }
    }

    /// Formats a provider-agnostic `SearchResponse` into an LLM-friendly string.
    fn format_results(query: &str, response: &SearchResponse) -> String {
        let mut output = format!("Web search results for: {query}\n");

        if let Some(ref summary) = response.summary {
            if !summary.is_empty() {
                output.push_str(&format!("\nSummary: {summary}\n"));
            }
        }

        if response.results.is_empty() {
            output.push_str("\nNo results found.");
            return output;
        }

        output.push_str(&format!("\n{} results:\n", response.results.len()));

        for (i, result) in response.results.iter().enumerate() {
            output.push_str(&format!(
                "\n{}. {}\n   {}\n   {}\n",
                i + 1,
                result.title,
                result.url,
                result.snippet,
            ));
        }

        output
    }
}

#[async_trait]
impl Skill for WebSearchSkill {
    fn name(&self) -> &str {
        "web_search"
    }

    fn description(&self) -> &str {
        "Search the web for current information. Use this when the user asks about \
         recent events, facts you're unsure about, or anything that requires up-to-date \
         information."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "query": {
                    "type": "string",
                    "description": "The search query to look up on the web"
                }
            },
            "required": ["query"]
        })
    }

    fn capabilities(&self) -> Vec<String> {
        vec![self.provider.capability()]
    }

    async fn execute(&self, params: Value) -> anyhow::Result<String> {
        let query = params["query"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("Missing required parameter: query"))?;

        debug!("Web search ({}): {query}", self.provider.provider_name());

        // Catch API/network errors and return them as text
        // so the LLM can inform the user instead of aborting.
        match self.provider.search(query).await {
            Ok(response) => Ok(Self::format_results(query, &response)),
            Err(e) => {
                warn!("Web search failed: {e}");
                Ok(format!("Web search failed: {e}"))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Helper to create a Tavily-backed test skill (no real API calls).
    fn tavily_skill() -> WebSearchSkill {
        WebSearchSkill {
            provider: Box::new(TavilyProvider::new("test-key", 5)),
        }
    }

    /// Helper to create a Perplexity-backed test skill (no real API calls).
    fn perplexity_skill() -> WebSearchSkill {
        WebSearchSkill {
            provider: Box::new(PerplexityProvider::new("test-key", "sonar")),
        }
    }

    // ── Skill trait tests ────────────────────────────────

    #[test]
    fn test_name() {
        assert_eq!(tavily_skill().name(), "web_search");
        assert_eq!(perplexity_skill().name(), "web_search");
    }

    #[test]
    fn test_description_not_empty() {
        assert!(!tavily_skill().description().is_empty());
    }

    #[test]
    fn test_parameters_schema_has_query() {
        let schema = tavily_skill().parameters_schema();
        assert_eq!(schema["type"], "object");
        assert_eq!(schema["properties"]["query"]["type"], "string");
        let required = schema["required"].as_array().unwrap();
        assert!(required.iter().any(|v| v == "query"));
    }

    #[test]
    fn test_capabilities_tavily() {
        let caps = tavily_skill().capabilities();
        assert_eq!(caps, vec!["network:api.tavily.com:443"]);
    }

    #[test]
    fn test_capabilities_perplexity() {
        let caps = perplexity_skill().capabilities();
        assert_eq!(caps, vec!["network:api.perplexity.ai:443"]);
    }

    #[tokio::test]
    async fn test_execute_missing_query_param() {
        let skill = tavily_skill();
        let result = skill.execute(json!({})).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("query"));
    }

    // ── Constructor tests ────────────────────────────────

    #[test]
    fn test_new_tavily_provider() {
        let config = WebSearchConfig {
            provider: "tavily".to_string(),
            api_key: "test-key".to_string(),
            max_results: 3,
            model: None,
        };
        let skill = WebSearchSkill::new(&config);
        assert_eq!(skill.provider.provider_name(), "tavily");
    }

    #[test]
    fn test_new_perplexity_provider() {
        let config = WebSearchConfig {
            provider: "perplexity".to_string(),
            api_key: "test-key".to_string(),
            max_results: 5,
            model: Some("sonar-pro".to_string()),
        };
        let skill = WebSearchSkill::new(&config);
        assert_eq!(skill.provider.provider_name(), "perplexity");
    }

    #[test]
    fn test_new_perplexity_defaults_to_sonar() {
        let config = WebSearchConfig {
            provider: "perplexity".to_string(),
            api_key: "test-key".to_string(),
            max_results: 5,
            model: None,
        };
        // Should not panic — defaults to "sonar"
        let skill = WebSearchSkill::new(&config);
        assert_eq!(skill.provider.provider_name(), "perplexity");
    }

    #[test]
    #[should_panic(expected = "Unsupported web search provider")]
    fn test_new_unsupported_provider_panics() {
        let config = WebSearchConfig {
            provider: "bing".to_string(),
            api_key: "test-key".to_string(),
            max_results: 5,
            model: None,
        };
        WebSearchSkill::new(&config);
    }

    // ── format_results tests (provider-agnostic) ─────────

    #[test]
    fn test_format_results_with_summary() {
        let response = SearchResponse {
            summary: Some("Rust is a systems programming language.".to_string()),
            results: vec![SearchResult {
                title: "Rust Language".to_string(),
                url: "https://www.rust-lang.org".to_string(),
                snippet: "Rust is blazingly fast and memory-efficient.".to_string(),
            }],
        };
        let output = WebSearchSkill::format_results("what is rust", &response);
        assert!(output.contains("Web search results for: what is rust"));
        assert!(output.contains("Summary: Rust is a systems programming language."));
        assert!(output.contains("1. Rust Language"));
        assert!(output.contains("https://www.rust-lang.org"));
        assert!(output.contains("Rust is blazingly fast"));
    }

    #[test]
    fn test_format_results_no_summary() {
        let response = SearchResponse {
            summary: None,
            results: vec![
                SearchResult {
                    title: "Result A".to_string(),
                    url: "https://a.com".to_string(),
                    snippet: "Content A".to_string(),
                },
                SearchResult {
                    title: "Result B".to_string(),
                    url: "https://b.com".to_string(),
                    snippet: "Content B".to_string(),
                },
            ],
        };
        let output = WebSearchSkill::format_results("test query", &response);
        assert!(!output.contains("Summary:"));
        assert!(output.contains("2 results:"));
        assert!(output.contains("1. Result A"));
        assert!(output.contains("2. Result B"));
    }

    #[test]
    fn test_format_results_empty() {
        let response = SearchResponse {
            summary: None,
            results: vec![],
        };
        let output = WebSearchSkill::format_results("obscure query", &response);
        assert!(output.contains("No results found"));
    }

    #[test]
    fn test_format_results_empty_summary_string() {
        let response = SearchResponse {
            summary: Some(String::new()),
            results: vec![SearchResult {
                title: "Title".to_string(),
                url: "https://example.com".to_string(),
                snippet: "Content".to_string(),
            }],
        };
        let output = WebSearchSkill::format_results("query", &response);
        // Empty summary should not produce "Summary:" line
        assert!(!output.contains("Summary:"));
    }

    /// Summary present but zero results → shows summary + "No results found".
    #[test]
    fn test_format_results_summary_with_no_results() {
        let response = SearchResponse {
            summary: Some("There's an answer.".to_string()),
            results: vec![],
        };
        let output = WebSearchSkill::format_results("q", &response);
        assert!(output.contains("Summary: There's an answer."));
        assert!(output.contains("No results found"));
    }

    /// Multiple results are numbered sequentially.
    #[test]
    fn test_format_results_numbering() {
        let response = SearchResponse {
            summary: None,
            results: (1..=4)
                .map(|i| SearchResult {
                    title: format!("Title {i}"),
                    url: format!("https://{i}.example"),
                    snippet: format!("Snippet {i}"),
                })
                .collect(),
        };
        let output = WebSearchSkill::format_results("multi", &response);
        assert!(output.contains("4 results:"));
        assert!(output.contains("1. Title 1"));
        assert!(output.contains("2. Title 2"));
        assert!(output.contains("3. Title 3"));
        assert!(output.contains("4. Title 4"));
    }
}
