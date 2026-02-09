//! Builtin skill: fetch a URL and extract readable text content.
//!
//! The LLM invokes this tool when it has a specific URL and needs to
//! read the page content. The HTML is converted to clean plain text
//! and truncated to fit the LLM context window.

use async_trait::async_trait;
use serde_json::{json, Value};
use tracing::{debug, warn};

use crate::skills::{Skill, SkillContext};

/// Maximum raw response body size (5 MB).
const MAX_RESPONSE_SIZE: usize = 5 * 1024 * 1024;

/// Maximum text output returned to the LLM (in characters).
const MAX_TEXT_OUTPUT: usize = 20_000;

/// HTTP read timeout in seconds.
const READ_TIMEOUT_SECS: u64 = 30;

/// HTTP connect timeout in seconds.
const CONNECT_TIMEOUT_SECS: u64 = 10;

/// Text wrapping width for html2text conversion.
const TEXT_WIDTH: usize = 100;

/// User-Agent header sent with requests.
const USER_AGENT: &str = "FluuxAgent/0.1 (+https://github.com/processone/fluux-agent)";

/// Builtin skill that fetches a URL and returns its text content.
pub struct UrlFetchSkill {
    client: reqwest::Client,
}

impl UrlFetchSkill {
    pub fn new() -> Self {
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(READ_TIMEOUT_SECS))
            .connect_timeout(std::time::Duration::from_secs(CONNECT_TIMEOUT_SECS))
            .user_agent(USER_AGENT)
            .redirect(reqwest::redirect::Policy::limited(5))
            .build()
            .unwrap_or_else(|_| reqwest::Client::new());

        Self { client }
    }
}

/// Returns true if the content type looks like HTML.
fn is_html(content_type: &str) -> bool {
    let ct = content_type.to_lowercase();
    ct.contains("text/html") || ct.contains("application/xhtml")
}

/// Returns true if the content type is textual (plain, json, xml, etc.).
fn is_text(content_type: &str) -> bool {
    let ct = content_type.to_lowercase();
    ct.contains("text/") || ct.contains("application/json") || ct.contains("application/xml")
}

/// Extract readable text from raw bytes based on content type.
fn extract_text(content_type: &str, body: &[u8]) -> String {
    if is_html(content_type) {
        html2text::from_read(body, TEXT_WIDTH).unwrap_or_else(|_| {
            String::from_utf8_lossy(body).into_owned()
        })
    } else if is_text(content_type) {
        String::from_utf8_lossy(body).into_owned()
    } else {
        // Unknown/binary — try UTF-8, fall back to error message
        let text = String::from_utf8_lossy(body);
        if text.chars().take(200).any(|c| c == '\0') {
            format!("Cannot extract text from binary content ({content_type})")
        } else {
            text.into_owned()
        }
    }
}

/// Format the final output for the LLM.
fn format_result(url: &str, text: &str) -> String {
    let mut output = format!("Content from: {url}\n\n");

    if text.is_empty() {
        output.push_str("[No text content extracted]");
        return output;
    }

    if text.chars().count() > MAX_TEXT_OUTPUT {
        // Truncate at character boundary
        let truncated: String = text.chars().take(MAX_TEXT_OUTPUT).collect();
        output.push_str(&truncated);
        output.push_str(&format!(
            "\n\n[Content truncated at {} characters]",
            MAX_TEXT_OUTPUT
        ));
    } else {
        output.push_str(text);
    }

    output
}

#[async_trait]
impl Skill for UrlFetchSkill {
    fn name(&self) -> &str {
        "url_fetch"
    }

    fn description(&self) -> &str {
        "Fetch the content of a web page at a given URL and extract readable text. \
         Use this when you have a specific URL and need to read its content — for example, \
         to summarize an article, extract information from a page, or follow a link from \
         search results. Returns the page text content, stripped of HTML markup."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "url": {
                    "type": "string",
                    "description": "The URL of the web page to fetch (http:// or https://)"
                }
            },
            "required": ["url"]
        })
    }

    fn capabilities(&self) -> Vec<String> {
        vec!["network:http:443".to_string()]
    }

    async fn execute(
        &self,
        params: Value,
        _context: &SkillContext,
    ) -> anyhow::Result<String> {
        let url_str = params["url"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("Missing required parameter: url"))?;

        // Validate URL
        let parsed = match url::Url::parse(url_str) {
            Ok(u) => u,
            Err(e) => {
                return Ok(format!("URL fetch failed: invalid URL — {e}"));
            }
        };

        match parsed.scheme() {
            "http" | "https" => {}
            scheme => {
                return Ok(format!(
                    "URL fetch failed: unsupported scheme '{scheme}' (only http/https)"
                ));
            }
        }

        debug!("Fetching URL: {url_str}");

        // Send request
        let response = match self.client.get(url_str).send().await {
            Ok(r) => r,
            Err(e) => {
                warn!("URL fetch failed: {e}");
                return Ok(format!("URL fetch failed: {e}"));
            }
        };

        // Check HTTP status
        let status = response.status();
        if !status.is_success() {
            return Ok(format!("URL fetch failed: HTTP {status}"));
        }

        // Check Content-Length if available
        if let Some(len) = response.content_length() {
            if len as usize > MAX_RESPONSE_SIZE {
                return Ok(format!(
                    "URL fetch failed: response too large ({} bytes, limit is {} bytes)",
                    len, MAX_RESPONSE_SIZE
                ));
            }
        }

        // Extract content type before consuming response
        let content_type = response
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("text/html")
            .to_string();

        // Read body
        let body = match response.bytes().await {
            Ok(b) => b,
            Err(e) => {
                warn!("URL fetch failed reading body: {e}");
                return Ok(format!("URL fetch failed: error reading response — {e}"));
            }
        };

        if body.len() > MAX_RESPONSE_SIZE {
            return Ok(format!(
                "URL fetch failed: response too large ({} bytes, limit is {} bytes)",
                body.len(),
                MAX_RESPONSE_SIZE
            ));
        }

        // Convert to text
        let text = extract_text(&content_type, &body);

        Ok(format_result(url_str, text.trim()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn test_context() -> SkillContext {
        SkillContext {
            jid: "test@localhost".to_string(),
            base_path: PathBuf::from("/tmp/test"),
        }
    }

    // ── Trait method tests ──────────────────────────────────

    #[test]
    fn test_name() {
        let skill = UrlFetchSkill::new();
        assert_eq!(skill.name(), "url_fetch");
    }

    #[test]
    fn test_description_not_empty() {
        let skill = UrlFetchSkill::new();
        assert!(!skill.description().is_empty());
    }

    #[test]
    fn test_parameters_schema_has_url() {
        let skill = UrlFetchSkill::new();
        let schema = skill.parameters_schema();
        assert_eq!(schema["type"], "object");
        assert_eq!(schema["properties"]["url"]["type"], "string");
        assert!(schema["required"]
            .as_array()
            .unwrap()
            .contains(&json!("url")));
    }

    #[test]
    fn test_capabilities() {
        let skill = UrlFetchSkill::new();
        assert_eq!(skill.capabilities(), vec!["network:http:443"]);
    }

    // ── Parameter validation tests ──────────────────────────

    #[tokio::test]
    async fn test_execute_missing_url_param() {
        let skill = UrlFetchSkill::new();
        let result = skill.execute(json!({}), &test_context()).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("url"));
    }

    #[tokio::test]
    async fn test_execute_invalid_url() {
        let skill = UrlFetchSkill::new();
        let result = skill
            .execute(json!({"url": "not-a-url"}), &test_context())
            .await
            .unwrap();
        assert!(result.contains("URL fetch failed"));
        assert!(result.contains("invalid URL"));
    }

    #[tokio::test]
    async fn test_execute_unsupported_scheme() {
        let skill = UrlFetchSkill::new();
        let result = skill
            .execute(json!({"url": "ftp://example.com/file"}), &test_context())
            .await
            .unwrap();
        assert!(result.contains("URL fetch failed"));
        assert!(result.contains("unsupported scheme"));
        assert!(result.contains("ftp"));
    }

    // ── format_result tests ─────────────────────────────────

    #[test]
    fn test_format_result_normal() {
        let result = format_result("https://example.com", "Hello world");
        assert!(result.starts_with("Content from: https://example.com\n\n"));
        assert!(result.contains("Hello world"));
        assert!(!result.contains("truncated"));
    }

    #[test]
    fn test_format_result_empty_content() {
        let result = format_result("https://example.com", "");
        assert!(result.contains("[No text content extracted]"));
    }

    #[test]
    fn test_format_result_truncation() {
        let long_text = "a".repeat(MAX_TEXT_OUTPUT + 100);
        let result = format_result("https://example.com", &long_text);
        assert!(result.contains("[Content truncated at"));
        // The content before truncation should be MAX_TEXT_OUTPUT chars
        let content_start = "Content from: https://example.com\n\n".len();
        let after_header = &result[content_start..];
        // Should contain the truncation marker
        assert!(after_header.contains("[Content truncated"));
    }

    #[test]
    fn test_format_result_exact_limit() {
        let text = "a".repeat(MAX_TEXT_OUTPUT);
        let result = format_result("https://example.com", &text);
        assert!(!result.contains("truncated"));
    }

    #[test]
    fn test_format_result_unicode_truncation() {
        // Multi-byte chars: each emoji is multiple bytes
        let text = "🎉".repeat(MAX_TEXT_OUTPUT + 10);
        let result = format_result("https://example.com", &text);
        assert!(result.contains("[Content truncated"));
        // Should not panic or produce invalid UTF-8
        assert!(result.is_char_boundary(result.len()));
    }

    // ── extract_text tests ──────────────────────────────────

    #[test]
    fn test_extract_text_html() {
        let html = b"<html><body><p>Hello world</p></body></html>";
        let text = extract_text("text/html; charset=utf-8", html);
        assert!(text.contains("Hello world"));
    }

    #[test]
    fn test_extract_text_html_strips_scripts() {
        let html = b"<html><body><script>alert('xss')</script><p>Content here</p></body></html>";
        let text = extract_text("text/html", html);
        assert!(text.contains("Content here"));
        assert!(!text.contains("alert"));
    }

    #[test]
    fn test_extract_text_plain() {
        let body = b"Just plain text content";
        let text = extract_text("text/plain", body);
        assert_eq!(text, "Just plain text content");
    }

    #[test]
    fn test_extract_text_json() {
        let body = b"{\"key\": \"value\"}";
        let text = extract_text("application/json", body);
        assert!(text.contains("\"key\""));
        assert!(text.contains("\"value\""));
    }

    #[test]
    fn test_extract_text_binary() {
        let body = b"\x00\x01\x02\x03binary data";
        let text = extract_text("application/octet-stream", body);
        assert!(text.contains("Cannot extract text from binary"));
    }

    #[test]
    fn test_extract_text_empty_html() {
        let html = b"<html><body></body></html>";
        let text = extract_text("text/html", html);
        // Should not panic; may be empty or whitespace
        let _ = text;
    }

    // ── is_html / is_text tests ─────────────────────────────

    #[test]
    fn test_is_html_various() {
        assert!(is_html("text/html"));
        assert!(is_html("text/html; charset=utf-8"));
        assert!(is_html("TEXT/HTML"));
        assert!(is_html("application/xhtml+xml"));
        assert!(!is_html("text/plain"));
        assert!(!is_html("application/json"));
    }

    #[test]
    fn test_is_text_various() {
        assert!(is_text("text/plain"));
        assert!(is_text("text/html"));
        assert!(is_text("application/json"));
        assert!(is_text("application/xml"));
        assert!(!is_text("application/octet-stream"));
        assert!(!is_text("image/png"));
    }

    // ── Additional extract_text tests ───────────────────────

    #[test]
    fn test_extract_text_unknown_but_textual() {
        // Unknown content type, but body is valid UTF-8 with no NUL bytes
        // → should pass through as text
        let body = b"Some unknown format content";
        let text = extract_text("application/octet-stream", body);
        // No NUL bytes → treated as text
        assert_eq!(text, "Some unknown format content");
    }

    #[test]
    fn test_extract_text_html_with_links() {
        let html = b"<html><body><a href=\"https://example.com\">Click here</a></body></html>";
        let text = extract_text("text/html", html);
        assert!(text.contains("Click here"));
        // html2text typically renders link text
    }

    #[test]
    fn test_extract_text_html_with_headings() {
        let html = b"<html><body><h1>Title</h1><p>Paragraph text.</p></body></html>";
        let text = extract_text("text/html", html);
        assert!(text.contains("Title"));
        assert!(text.contains("Paragraph text."));
    }

    #[test]
    fn test_extract_text_html_with_list() {
        let html = b"<html><body><ul><li>Item one</li><li>Item two</li></ul></body></html>";
        let text = extract_text("text/html", html);
        assert!(text.contains("Item one"));
        assert!(text.contains("Item two"));
    }

    #[test]
    fn test_extract_text_non_utf8() {
        // Latin-1 encoded content — from_utf8_lossy handles it
        let body: &[u8] = &[0xC9, 0x6C, 0xE8, 0x76, 0x65]; // "Élève" in Latin-1
        let text = extract_text("text/plain", body);
        // Should not panic; from_utf8_lossy replaces invalid sequences
        assert!(!text.is_empty());
    }

    #[test]
    fn test_extract_text_xml() {
        let body = b"<root><item>value</item></root>";
        let text = extract_text("application/xml", body);
        assert!(text.contains("<root>"));
        assert!(text.contains("value"));
    }

    // ── Additional format_result tests ──────────────────────

    #[test]
    fn test_format_result_just_under_limit() {
        let text = "x".repeat(MAX_TEXT_OUTPUT - 1);
        let result = format_result("https://example.com", &text);
        assert!(!result.contains("truncated"));
        assert!(result.contains(&text));
    }

    #[test]
    fn test_format_result_one_over_limit() {
        let text = "x".repeat(MAX_TEXT_OUTPUT + 1);
        let result = format_result("https://example.com", &text);
        assert!(result.contains("[Content truncated"));
    }

    #[test]
    fn test_format_result_preserves_url() {
        let url = "https://example.com/path?q=hello&lang=en";
        let result = format_result(url, "content");
        assert!(result.contains(url));
    }

    // ── Additional parameter validation tests ───────────────

    #[tokio::test]
    async fn test_execute_data_scheme_rejected() {
        let skill = UrlFetchSkill::new();
        let result = skill
            .execute(json!({"url": "data:text/html,<h1>hi</h1>"}), &test_context())
            .await
            .unwrap();
        assert!(result.contains("URL fetch failed"));
        assert!(result.contains("unsupported scheme"));
    }

    #[tokio::test]
    async fn test_execute_file_scheme_rejected() {
        let skill = UrlFetchSkill::new();
        let result = skill
            .execute(json!({"url": "file:///etc/passwd"}), &test_context())
            .await
            .unwrap();
        assert!(result.contains("URL fetch failed"));
        assert!(result.contains("unsupported scheme"));
    }

    #[tokio::test]
    async fn test_execute_url_param_wrong_type() {
        let skill = UrlFetchSkill::new();
        let result = skill.execute(json!({"url": 42}), &test_context()).await;
        assert!(result.is_err());
    }

    // ── Constructor / registry tests ────────────────────────

    #[test]
    fn test_new_does_not_panic() {
        let _skill = UrlFetchSkill::new();
    }

    #[test]
    fn test_tool_definition_from_skill() {
        use crate::skills::SkillRegistry;
        let mut registry = SkillRegistry::new();
        registry.register(Box::new(UrlFetchSkill::new()));

        let defs = registry.tool_definitions();
        assert_eq!(defs.len(), 1);
        assert_eq!(defs[0].name, "url_fetch");
        assert!(!defs[0].description.is_empty());
        assert_eq!(defs[0].input_schema["type"], "object");
        assert!(defs[0].input_schema["properties"]["url"].is_object());
    }

    #[test]
    fn test_registry_lookup() {
        use crate::skills::SkillRegistry;
        let mut registry = SkillRegistry::new();
        registry.register(Box::new(UrlFetchSkill::new()));

        assert!(registry.get("url_fetch").is_some());
        assert!(registry.get("nonexistent").is_none());
    }
}
