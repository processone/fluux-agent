## What's New in v0.2.0

### Added

- **Skills**: Trait-based skill system with registry and tool definitions
- **Skills**: Agentic tool-use loop with automatic tool result feedback (max 10 rounds)
- **Skills**: `web_search` skill with Tavily and Perplexity providers
- **Skills**: `memory_store` / `memory_recall` skills for per-JID knowledge management
- **Skills**: `url_fetch` skill for URL content extraction and summarization
- **LLM**: Abstracted `LlmClient` trait with provider dispatch
- **LLM**: Ollama provider for local model inference (`POST /api/chat`)
- **LLM**: Temporal awareness — current date injected into system prompt
- **Memory**: Structured metadata for attachments and reactions in JSONL session history
- **Memory**: Session timeout with lazy auto-archival of idle sessions (configurable `idle_timeout_mins`)
- **XMPP**: Connection keepalive with whitespace pings and read timeout detection
- **XMPP**: Connection probing and refined event flow on read timeout
- **CI**: GitHub Actions for PR checks (test + clippy) and release builds

---
[Full Changelog](https://github.com/processone/fluux-agent/blob/main/CHANGELOG.md)
