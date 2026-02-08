# Roadmap

This document tracks the planned features for Fluux Agent, organized by release milestone.

Items marked **done** are merged. Items marked **next** are the current priority.

---

## v0.1 — Foundation (current)

The minimum viable agent: connect, authenticate, converse, remember.

- [x] XMPP component mode (XEP-0114)
- [x] XMPP C2S client mode (SASL PLAIN + SCRAM-SHA-1 + STARTTLS)
- [x] Agentic loop with Claude API (Anthropic)
- [x] Markdown-based conversational memory (per-user `history.md` + `context.md`)
- [x] Allowed JID authorization
- [x] Chat state notification filtering (XEP-0085)
- [x] Configuration with environment variable expansion

### v0.1 — remaining

- [x] Conversation sessions (`/new`, `/reset`, session archival)
- [x] Slash commands (runtime-intercepted, never reach the LLM)
- [ ] Presence subscription for allowed JIDs — **next**
- [ ] Cross-domain message rejection (security default)
- [ ] Reconnection with exponential backoff

### Conversation sessions ✓

Implemented: the agent supports discrete sessions per user.

- **`/new` and `/reset`** — Archives the current `history.md` into `sessions/{YYYYMMDD-HHMMSS}.md` and starts a fresh conversation. The LLM sees an empty history.
- **`/forget`** — Erases the current session history and user context. Archived sessions are preserved.
- **Memory layout** — `{jid}/history.md` (current session), `{jid}/sessions/*.md` (archived), `{jid}/context.md` (user context). All human-readable markdown.
- **`/status`** — Reports message count in current session and number of archived sessions.

#### Future enhancements (not yet implemented)

- **XMPP thread ID mapping** — When the XMPP client sends a `<thread>` element (XEP-0201), the agent maps it to a session. Different thread IDs = different sessions. Messages without a thread ID use the "default" session.
- **Session timeout** — If no message is received for a configurable duration (e.g. 4 hours), the next message implicitly starts a new session. The timeout is per-user.
- **Session context carry-over** — When a new session starts, the agent can optionally summarize the previous session into `context.md`, giving continuity without sending the full old history to the LLM.

### Presence subscription for allowed JIDs (roster management)

When the agent starts and connects in C2S mode:

- For each JID in `allowed_jids`, send a presence subscription request (`<presence type='subscribe'>`) if not already subscribed.
- Auto-accept incoming subscription requests from allowed JIDs (`<presence type='subscribed'>`).
- Ignore or reject subscription requests from JIDs not in the allow list.
- This gives the agent proper roster integration — allowed users see the agent as online in their contact list.
- In component mode, the server typically handles routing differently, so this is primarily a C2S concern.

### Cross-domain message rejection

By default, the agent should reject messages originating from a different XMPP domain than the one it is connected to. This prevents:

- Unsolicited messages from federated servers reaching the agent
- Abuse via federation from unknown domains
- Prompt injection attacks from external senders

The `allowed_jids` list already provides per-JID filtering, but domain-level rejection adds an additional security layer. A new config option controls this:

```toml
[agent]
# Only accept messages from these domains (default: server domain only)
# Set to ["*"] to allow federation (use with caution)
allowed_domains = ["localhost"]
```

If `allowed_domains` is not set, the agent infers it from its own JID or component domain.

### Slash commands ✓

Messages starting with `/` are intercepted by the runtime before they reach the LLM. They cost zero API calls and their behavior is deterministic — no hallucination risk, no latency.

#### Implemented commands

| Command | Description | Status |
|---------|-------------|--------|
| `/new` / `/reset` | Start a new conversation session (archive current) | ✓ |
| `/forget` | Erase the current user's history and context | ✓ |
| `/status` | Agent uptime, mode, LLM info, session stats | ✓ |
| `/help` | List available commands | ✓ |
| `/ping` | Simple liveness check (responds immediately, no LLM) | ✓ |

#### Planned developer commands

For debugging and development. Can be restricted to a specific admin JID or disabled entirely in production via config.

| Command | Description |
|---------|-------------|
| `/debug` | Toggle debug output (echo raw stanzas, LLM request/response, token counts) |
| `/context` | Show the current user's `context.md` content |
| `/history` | Show the last N messages from the current session |
| `/tier` | Show which model tier/model is being used for this conversation |
| `/raw <xml>` | Send a raw XML stanza (dev only — for testing XMPP interactions) |

#### Implementation

In `runtime.rs`, the message handler checks for the `/` prefix before calling the LLM:

```rust
if body.starts_with('/') {
    return self.handle_command(from, body);
}
```

Commands are a simple match on the first token. No parsing framework needed — there are few commands and they take at most one argument. This keeps v0.1 simple. If the command set grows significantly in later versions, a proper command parser can be introduced.

```toml
[agent]
# Restrict dev commands to specific JIDs (empty = disabled)
dev_command_jids = ["admin@localhost"]
```

---

## v0.2 — Skills system

The agent can do things beyond conversation.

- [ ] Skill trait and registry
- [ ] LLM tool use integration (agentic loop)
- [ ] Model tiering (route tasks to appropriate model by complexity/cost)
- [ ] `LlmClient` trait + Ollama provider (local models via Ollama API)
- [ ] Declarative skill capabilities (TOML manifests)
- [ ] Action plan validation (separate from LLM)
- [ ] Builtin skill: web search
- [ ] Builtin skill: URL fetch and summarize
- [ ] Proactive context learning — agent updates `context.md` by summarizing conversations

### How skills are exposed to the LLM

Skills are **LLM tools**. Modern LLMs (Claude, GPT-4, etc.) have native tool/function calling: the model receives a list of tool definitions (name, description, parameters as JSON Schema), and can request tool invocations as part of its response. The agent runtime orchestrates the loop.

The architecture has three layers:

```
┌──────────────────────────────────────────────────┐
│                  LLM (Claude API)                │
│  Receives: system prompt + messages + tools[]    │
│  Returns:  text | tool_use(name, params)         │
└──────────────────┬───────────────────────────────┘
                   │
┌──────────────────▼───────────────────────────────┐
│              Agent Runtime (runtime.rs)           │
│  1. Builds tool definitions from SkillRegistry   │
│  2. Sends to LLM as `tools` parameter            │
│  3. If LLM returns tool_use → validate → execute │
│  4. Feeds tool_result back → LLM continues       │
│  5. Loop until LLM returns text (final answer)   │
└──────────────────┬───────────────────────────────┘
                   │
┌──────────────────▼───────────────────────────────┐
│              Skill Registry                       │
│  - Discovers available skills (builtin + Wasm)   │
│  - Each skill provides: name, description,       │
│    parameter schema, capability requirements     │
│  - Executes skills and returns results           │
└──────────────────────────────────────────────────┘
```

#### The Skill trait

Each skill implements a common trait:

```rust
#[async_trait]
pub trait Skill: Send + Sync {
    /// Unique identifier (e.g. "web_search", "url_fetch")
    fn name(&self) -> &str;

    /// Human-readable description (shown to the LLM)
    fn description(&self) -> &str;

    /// JSON Schema describing accepted parameters
    fn parameters_schema(&self) -> serde_json::Value;

    /// Required capabilities (validated against TOML manifest)
    fn capabilities(&self) -> Vec<String>;

    /// Execute the skill with the given parameters
    async fn execute(&self, params: serde_json::Value) -> Result<String>;
}
```

#### The agentic loop with tool use

Today, `runtime.rs` does a single LLM call and returns the text. With skills, it becomes a loop:

```
User message
    ↓
Build tools[] from SkillRegistry
    ↓
Call LLM(system, messages, tools)
    ↓
┌─── LLM response ───┐
│                     │
│  text block?  ──────┼──→ Send to user (done)
│                     │
│  tool_use block? ───┼──→ Validate against capabilities
│                     │       ↓
│                     │    Execute skill
│                     │       ↓
│                     │    Append tool_result to messages
│                     │       ↓
│                     │    Call LLM again (loop)
└─────────────────────┘
```

Key points:

- **The LLM decides when to use a tool.** It sees the tool definitions and chooses based on the user's request. The agent runtime never guesses — it just orchestrates.
- **The runtime validates before executing.** Even if the LLM requests a tool, the runtime checks: Does this skill exist? Does it have the required capabilities? Are the parameters valid? This is the action plan validation layer from SECURITY.md.
- **Multiple rounds are possible.** The LLM can chain tools: search the web, then fetch a URL, then summarize. Each tool result feeds back into the next LLM call. A configurable max-rounds limit prevents infinite loops.
- **Tool results are opaque to the user.** The user sees the final text response. They don't see intermediate tool calls unless the agent chooses to mention them.

#### Changes to the LLM client (anthropic.rs)

The Anthropic Messages API already supports tool use. The current client sends `messages` and gets back `text` content blocks. We need to:

1. Add `tools` to the request (array of tool definitions with `name`, `description`, `input_schema`).
2. Handle `tool_use` content blocks in the response (with `id`, `name`, `input`).
3. Add `tool_result` content blocks to the conversation (with `tool_use_id`, `content`).
4. Support `Message.content` as a structured type (array of content blocks) instead of a plain string, to carry both text and tool interactions.

The `complete()` method becomes `complete_with_tools()` or we evolve the existing method. The runtime loop replaces the current single-shot call.

#### Skill discovery and registration

At startup:
1. The `SkillRegistry` scans for builtin skills (compiled in) and Wasm skills (loaded from disk in v0.4).
2. Each skill is instantiated and its `parameters_schema()` is cached.
3. The runtime calls `registry.tool_definitions()` to get the list to send to the LLM.

```toml
# config/agent.toml — skill configuration (v0.2)
[skills]
enabled = ["web_search", "url_fetch"]

[skills.web_search]
api_key = "${TAVILY_API_KEY}"
max_results = 5
```

Skills that are not in the `enabled` list are not exposed to the LLM. This gives the operator explicit control over what the agent can do.

### Model tiering

Not every task needs the most expensive model. An image analysis requires vision capabilities. A cron job that checks a calendar and sends a reminder is routine. A creative brainstorming session benefits from the strongest reasoning. Sending all of these to `claude-sonnet-4-5` wastes money and latency.

Model tiering lets the agent route each task to the appropriate model based on what the task actually requires.

#### Tier definitions

```toml
[llm]
# Default model for interactive conversation
default = "anthropic:claude-sonnet-4-5-20250929"

# Model tiers — the runtime picks the right one per task
[llm.tiers]

# Tier 1: Heavy reasoning, complex multi-step planning, creative work
heavy = "anthropic:claude-sonnet-4-5-20250929"

# Tier 2: Standard conversation, most skill orchestration
standard = "anthropic:claude-sonnet-4-5-20250929"

# Tier 3: Simple structured tasks, classification, extraction
light = "anthropic:claude-haiku-3-5-20241022"

# Tier 4: Vision — image analysis, screenshot interpretation
vision = "anthropic:claude-sonnet-4-5-20250929"
```

Each tier maps to a model identifier. The same model can appear in multiple tiers. The operator controls which models to use and can override any tier — e.g., run everything through Haiku during development, or route `light` tasks to a local Ollama model once multi-provider support lands.

#### How tasks get routed

Routing is **declarative, not heuristic**. The runtime doesn't try to guess task complexity. Instead, each entry point declares which tier it needs:

| Source | Tier | Rationale |
|--------|------|-----------|
| Interactive user message (default) | `standard` | General conversation needs good reasoning |
| Skill with `tier = "light"` in manifest | `light` | Skill author knows the task is simple |
| Skill with `tier = "vision"` in manifest | `vision` | Skill requires image understanding |
| Proactive cron job (v0.3) | `light` | Scheduled tasks are typically routine |
| Context summarization | `light` | Summarizing a session into `context.md` is mechanical |
| Complex planning (agent detects multi-step) | `heavy` | Explicit escalation for hard problems |

Skills declare their tier in their TOML manifest:

```toml
[skill.calendar_check]
name = "Calendar Check"
description = "Check upcoming events"
tier = "light"                    # This skill only needs a cheap model
capabilities = ["network:calendar.google.com:443"]

[skill.image_analysis]
name = "Image Analysis"
description = "Analyze an image sent by the user"
tier = "vision"                   # Needs vision capabilities
capabilities = []
```

If a skill doesn't declare a tier, it inherits `standard`.

#### Runtime mechanics

The `LlmClient` trait (which `AnthropicClient` implements) gains a tier parameter:

```rust
pub trait LlmClient: Send + Sync {
    async fn complete(
        &self,
        tier: ModelTier,
        system_prompt: &str,
        messages: &[Message],
        tools: &[ToolDefinition],
    ) -> Result<LlmResponse>;
}
```

The client resolves the tier to a concrete model string from the config, then calls the appropriate API. This abstraction also prepares for multi-provider support — `light` could route to Ollama while `heavy` stays on Claude.

#### Escalation

In the agentic loop, the runtime can **escalate** mid-conversation. If a `light`-tier skill execution produces a result that the LLM needs to reason about in depth, the final synthesis call can use `standard` or `heavy`. The tier applies per-call, not per-conversation.

A typical flow for a cron job:
1. Scheduler triggers "check calendar" → `light` tier
2. Skill executes, returns structured data → no LLM needed
3. If there's something to notify the user about → `light` tier formats the message
4. Total cost: two Haiku calls instead of two Sonnet calls

A typical flow for image analysis:
1. User sends an image with "what's this?"
2. Runtime detects attachment → routes to `vision` tier
3. Vision model analyzes → returns text
4. If follow-up conversation → drops back to `standard` tier

### Local models via Ollama

The `LlmClient` trait abstracts the provider. In v0.2 we ship two implementations:

- **`AnthropicClient`** — existing, talks to the Claude Messages API
- **`OllamaClient`** — new, talks to the [Ollama REST API](https://github.com/ollama/ollama/blob/main/docs/api.md) (`/api/chat` endpoint)

Ollama serves local models (Llama 3, Mistral, Phi, Gemma, etc.) with an OpenAI-compatible chat API that also supports tool use. This makes it a natural fit for the `light` tier — routine tasks run locally at zero API cost and with no network latency.

Configuration:

```toml
[llm.tiers]
heavy = "anthropic:claude-sonnet-4-5-20250929"
standard = "anthropic:claude-sonnet-4-5-20250929"
light = "ollama:llama3.1:8b"           # Runs locally, zero cost
vision = "anthropic:claude-sonnet-4-5-20250929"

[llm.ollama]
host = "http://localhost:11434"        # Default Ollama endpoint
```

The provider prefix (`anthropic:`, `ollama:`) in the tier string tells the runtime which `LlmClient` implementation to dispatch to. This keeps config flat and readable.

**Why Ollama in v0.2, not later:**
- Model tiering without a cheap local option is half the story — the main cost savings come from running `light` tasks locally
- Ollama's `/api/chat` endpoint supports tool use (function calling), so skills work with local models too
- The `LlmClient` trait needs to exist for tiering anyway — adding a second implementation at the same time validates the abstraction is correct
- Privacy-sensitive deployments can run entirely on local models with no external API calls

---

## v0.3 — Proactivity

The agent initiates, not just responds.

- [ ] Cron-based scheduled tasks (via PubSub or internal scheduler)
- [ ] Heartbeat / keepalive for long-lived connections
- [ ] Webhook ingestion — external events trigger agent actions
- [ ] PubSub subscription — agent reacts to XMPP PubSub events

---

## v0.4 — Sandbox

Skills run in isolation.

- [ ] Wasm sandbox via wasmtime (fuel-metered, memory-limited)
- [ ] Landlock + seccomp on Linux
- [ ] App Sandbox on macOS
- [ ] Process isolation (one process per skill execution)
- [ ] Resource limits (CPU, memory, execution time)

See [Security Architecture](docs/SECURITY.md) for the full design.

---

## v0.5 — Agent protocol

Structured machine-readable communication.

- [ ] `urn:fluux:agent:0#skills` — skill discovery via IQ
- [ ] `urn:fluux:agent:0#execute` — skill execution via IQ
- [ ] `urn:fluux:agent:0#confirm` — destructive action confirmation
- [ ] Reactions support — agent sends and receives message reactions (XEP-0444)
- [ ] Message corrections — agent can correct its previous response (XEP-0308)

### Reactions (XEP-0444)

Reactions serve as lightweight feedback and acknowledgment:

- **Agent sends reactions** — The agent can react to user messages (e.g. thumbs-up to acknowledge a command, checkmark when a task completes). This is an action the LLM can trigger.
- **Agent receives reactions** — Users can react to agent messages. The agent interprets these as feedback signals (e.g. thumbs-down on a response could trigger a retry or context adjustment).
- Reactions use XEP-0444 (Message Reactions), which references the original message by `id`.

---

## v1.0 — Federation

Agent-to-agent communication.

- [ ] Agent-to-agent federation via XMPP S2S
- [ ] Agent capability advertisement (disco#info)
- [ ] Delegated task execution (agent A asks agent B to run a skill)
- [ ] End-to-end encryption (OMEMO or custom per-agent keys)
- [ ] Complete documentation and deployment guides

---

## Ideas (unscheduled)

Items that may be valuable but don't have a milestone yet.

### Pairing mode

A discovery mechanism where a new user can "pair" with the agent by exchanging a one-time code or token, rather than requiring pre-configuration in `allowed_jids`. This would work like Bluetooth pairing:

1. Admin generates a pairing code via the agent (e.g. `/pair generate`)
2. New user sends the code to the agent
3. Agent verifies and adds the user to the allow list

**Open question:** This works around the fact that `allowed_jids` must be configured in advance. It might be unnecessary if we have good enough roster management and admin commands. It also introduces a security surface (pairing code brute-force, time windows). Probably not needed for v1.0 — the explicit allow list is more secure and sufficient for personal/small-team use.

### Additional LLM providers

Ollama (local models) and Anthropic are covered in v0.2. Future providers:

- OpenAI / GPT-4o — useful for `vision` tier or as alternative `standard`
- Mistral API — European hosting, GDPR-friendly
- Automatic fallback chain (try Claude, fall back to local Ollama if API is down)
- All configured per-tier in TOML — no code changes needed to switch providers

### Message archive integration (MAM)

- Use XEP-0313 (Message Archive Management) to persist and retrieve history server-side
- Could replace or complement the local markdown memory
- Enables multi-device access to conversation history

### File attachments

Support receiving and sending files via XMPP:

- **Receiving attachments** — Users can send images, PDFs, documents to the agent. The agent downloads the file (via HTTP Upload URL in the message, XEP-0363), passes it to the LLM as context (e.g. Claude's vision for images, text extraction for PDFs), and responds accordingly. This enables "analyze this screenshot", "summarize this PDF", "what's in this photo" workflows.
- **Sending attachments** — The agent can generate and send files back to the user: skill execution results as CSV, generated images, exported data. Uses XEP-0363 (HTTP File Upload) to upload to the server's HTTP upload service, then sends the URL in a message with an `<x xmlns='jabber:x:oob'>` out-of-band reference.
- **Security** — File downloads must respect size limits, content-type validation, and sandbox restrictions. Files are stored temporarily and cleaned up after processing.

### Typing indicators (outbound)

Send `<composing/>` while the LLM is generating a response, and `<active/>` when the response is sent. This gives users visual feedback that the agent is "thinking".
