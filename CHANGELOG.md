# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/),
and this project adheres to [Semantic Versioning](https://semver.org/).

## [Unreleased]

## [0.2.0] - 2026-02-10

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

## [0.1.0] - 2026-02-08

### Added

- **XMPP Core**: Component protocol (XEP-0114) and client (C2S) connection modes
- **XMPP Core**: TLS/STARTTLS support with SASL SCRAM authentication
- **XMPP Core**: Presence stanza handling and roster support
- **XMPP Core**: MUC room support (XEP-0045) with nick-based sender attribution
- **XMPP Core**: Chat state notifications (XEP-0085) with XEP-0334 processing hints
- **XMPP Core**: XEP-0444 reaction parsing (inbound)
- **XMPP Core**: Cross-domain message filtering
- **XMPP Core**: Out-of-band (OOB) URL stripping from message bodies
- **XMPP Core**: Reconnection with exponential backoff and error classification
- **XML Parsing**: Event-based parser using `quick-xml` 0.36 (`StanzaParser`)
- **XML Parsing**: Proper XML escaping for outbound stanzas (body text and attributes)
- **Memory**: JSONL session format with structured metadata (attachments, reactions)
- **Memory**: Per-JID workspace overrides for configurable identities and personas
- **LLM**: Anthropic Claude provider with streaming SSE support
- **LLM**: Multi-modal content support (images, documents) via Anthropic API
- **Media**: File download and transfer support with attachment metadata
- **Config**: TOML-based configuration with environment variable substitution
- **Config**: Per-JID workspace overrides for identities, personas, and instructions
