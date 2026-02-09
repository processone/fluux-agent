# Fluux Agent

**A framework for open and federated AI agent networks — built on XMPP with security in mind.**

Fluux Agent is the foundation for a new kind of AI infrastructure: autonomous agents that communicate over an open, federated protocol instead of walled-garden APIs. Today it's a single-agent runtime that connects to any XMPP server. Tomorrow it's a network where your agent talks to mine — across domains, across organizations — without a centralized platform controlling the conversation.

This is the beginning. The runtime you see here — XMPP connectivity, conversational memory, LLM integration — is the first building block. The roadmap leads to a skills system, sandboxed execution, an agent-to-agent protocol, and ultimately **federation**: agents on different servers discovering each other, delegating tasks, and collaborating, all over standard XMPP infrastructure that has been battle-tested for 20 years.

## Why

OpenClaw demonstrated the massive demand for personal AI assistants that actually act. But its architecture — Node.js bridges to every messaging platform, root system access, JSON file storage — poses fundamental security, reliability, and interoperability problems. More critically, it's a closed system: agents can't talk to each other, and every integration is a bespoke bridge that the project must maintain.

The AI agent ecosystem needs what email gave us for messages and the web gave us for documents: **an open protocol where any agent can reach any other agent, regardless of who hosts it.**

XMPP already provides this. It has solved reliable message routing, authentication, presence, PubSub for events, message history (MAM), multi-device synchronization, and most importantly **federation** for 20 years. Billions of messages have been routed through XMPP infrastructure. The protocol is extensible by design — adding agent-specific semantics (skill discovery, task delegation, action confirmation) is exactly the kind of problem XMPP extensions were made for.

Fluux Agent brings these two worlds together: the power of modern AI agents with the robustness of proven, federated messaging infrastructure.

## Architecture

```
┌─────────────────────────────────────────────────────┐
│                   XMPP Server                       │
│         (ejabberd, Prosody, Openfire...)            │
│                                                     │
│  Routing · Presence · PubSub · MAM · Federation     │
└────────────────────┬────────────────────────────────┘
                     │ XEP-0114 or C2S (SASL + STARTTLS)
                     │
┌────────────────────▼────────────────────────────────┐
│                  Fluux Agent                        │
│              (standalone Rust binary)               │
│                                                     │
│  ┌─────────────┐ ┌──────────────┐ ┌──────────────┐  │
│  │    XMPP     │ │    Agent     │ │     LLM      │  │
│  │  Component  │ │   Runtime    │ │   Client     │  │
│  │  or C2S     │ │  (agentic    │ │  (Claude,    │  │
│  │  Client     │ │   loop)      │ │  Ollama...)  │  │
│  └─────────────┘ └──────────────┘ └──────────────┘  │
│                                                     │
│  ┌─────────────┐ ┌──────────────┐ ┌──────────────┐  │
│  │   Skills    │ │   Memory     │ │   Sandbox    │  │
│  │  Registry   │ │  (Markdown / │ │  (Wasm +     │  │
│  │  + Builtin  │ │   PEP)       │ │  Landlock)   │  │
│  └─────────────┘ └──────────────┘ └──────────────┘  │
└─────────────────────────────────────────────────────┘
```

### Key Principles

- **Open and federated** — Agents communicate over XMPP, an open standard with native federation. No vendor lock-in, no centralized platform. Your agent, your server, your rules.
- **Total decoupling** — The agent is a standard XMPP component. It works with any XMPP server, not just ejabberd.
- **Designed for teams** — Each user has their own isolated conversation context and memory directory, while shared skills and procedures are accessible to everyone. One agent instance serves the whole team with clean separation of personal data and shared capabilities.
- **Security by design** — Defense-in-depth with 5 independent layers: declarative capabilities, action validation, Wasm sandbox, kernel sandboxing (Landlock/seccomp), and process isolation. The LLM never directly touches the system. In corporate deployments, the agent will be able to scan conversations and detect prompt injection attempts, limiting the risk of adversarial manipulation through crafted messages. See [Security Architecture](docs/SECURITY.md).
- **No public endpoint required** — XMPP acts as your inbound transport. The agent runs on your laptop or private network and connects to an XMPP server (your own or a public one like `conversations.im`). Services can reach your agent by sending XMPP messages — no need to expose webhooks or open firewall ports. This makes local development and secure production deployments trivial.
- **Enterprise control layer** — XMPP + Fluux Agent creates an AI gateway for organizations. The XMPP server can act as an LLM firewall, scanning traffic for prompt injection before messages reach the agent. Model changes, cost optimization, and configuration updates happen server-side without touching client applications. End users interact with a stable XMPP address while the backend switches between Claude, Ollama, or other models transparently.
- **Proactivity** — Cron jobs, PubSub subscriptions, webhooks. The agent can initiate conversations, not just respond.
- **Federation** — My agent `agent.domain-a.com` talks to your agent `agent.domain-b.com` via XMPP federation. No centralized platform.

## Vision

```
Today (v0.1)                         Tomorrow (v1.0)

 User ↔ Agent ↔ LLM                  User ↔ Agent A ↔ Agent B ↔ Agent C
                                              │            │
                                          domain-a.com  domain-b.com
                                              │            │
                                          ┌───┴────────────┴───┐
                                          │  XMPP Federation   │
                                          │  (open, standard)  │
                                          └────────────────────┘
```

Fluux Agent starts as a personal AI assistant — one user, one agent, one LLM. But the architecture is designed from day one to scale to a federated network of agents that discover each other's skills, delegate tasks, and collaborate across organizational boundaries. The protocol layer (XMPP) already handles the hard parts: routing, presence, authentication, encryption, and federation. Fluux Agent adds the AI semantics on top.

## Status

> **v0.1** — Foundation (tagged). Dual-mode XMPP connection (component + C2S), agentic loop with Claude API, JSONL session memory, conversation sessions, slash commands, and MUC rooms. **v0.2 in progress** — skills system (web search), LlmClient trait with Anthropic and Ollama support. See the [Roadmap](ROADMAP.md) for what's next.

### Roadmap

| Phase    | Description                                                                                                                        | Status         |
|----------|------------------------------------------------------------------------------------------------------------------------------------|----------------|
| **v0.1** | XMPP component + C2S client + agentic loop + Claude API + JSONL memory + sessions + slash commands + MUC rooms                     | ✅ Tagged       |
| **v0.2** | Skills system (web search), LlmClient trait, Ollama support, model tiering, proactive context learning, prompt injection detection | 🔨 In progress |
| **v0.3** | Proactivity (cron via PubSub, heartbeat), advanced MUC (room-specific prompts, invite handling)                                    | Planned        |
| **v0.4** | Wasm sandbox (wasmtime) + Landlock                                                                                                 | Planned        |
| **v0.5** | Agent protocol (`urn:fluux:agent:0`) — discovery, execute, confirm                                                                 | Planned        |
| **v1.0** | Agent-to-agent federation, complete documentation                                                                                  | Planned        |

## Quick Start

### Prerequisites

- Rust ≥ 1.75
- An XMPP server (ejabberd, Prosody, Openfire...)
- An LLM backend, either:
  - **Ollama** running locally for private/offline deployments (no API key needed)
  - **Anthropic API key** (Claude) for cloud-hosted models, or

Two connection modes are supported, suited to different deployment contexts:

**Client mode (C2S)** — The agent connects as a regular XMPP user (e.g., `bot@localhost`). No server configuration needed beyond a user account. Supports SASL PLAIN, SCRAM-SHA-1, and STARTTLS. **This is the easiest way to get started** — ideal for individuals and small teams who just want a personal AI assistant on their existing XMPP server.

**Component mode (XEP-0114)** — The agent registers as a subdomain (e.g., `agent.localhost`). Requires server-side configuration but gives the agent its own address namespace, better isolation, and the ability to handle messages for an entire subdomain. **Companies and organizations** will typically prefer this mode for production deployments — it integrates the agent as a first-class service on the XMPP infrastructure rather than as a regular user account.

### ejabberd Configuration (component mode)

```yaml
listen:
  -
    port: 5275
    module: ejabberd_service
    access: all
    hosts:
      "agent.localhost":
        password: "secret"
```

### Launch

```bash
cp config/agent.example.toml config/agent.toml
# Edit config/agent.toml with your settings

# With Anthropic (cloud):
export ANTHROPIC_API_KEY="sk-ant-..."

# With Ollama (local — no API key needed):
# Set provider = "ollama" and model = "llama3.2" in config/agent.toml
# Make sure Ollama is running: ollama serve

export AGENT_SECRET="secret"       # component mode
# or: export BOT_PASSWORD="pass"   # client mode

cargo run
```

Then, send a message to `agent.localhost` (component mode) or `bot@localhost` (client mode) from any XMPP client.

## Configuration

**Component mode:**

```toml
[server]
host = "localhost"
port = 5275
mode = "component"
component_domain = "agent.localhost"
component_secret = "${AGENT_SECRET}"
```

**Client mode (C2S):**

```toml
[server]
host = "localhost"
port = 5222
mode = "client"
jid = "bot@localhost"
password = "${BOT_PASSWORD}"
resource = "fluux-agent"
tls_verify = false  # for self-signed certs (dev)
```

**LLM — Anthropic (cloud):**

```toml
[llm]
provider = "anthropic"
model = "claude-sonnet-4-5-20250929"
api_key = "${ANTHROPIC_API_KEY}"
max_tokens_per_request = 4096
```

**LLM — Ollama (local):**

```toml
[llm]
provider = "ollama"
model = "llama3.2"
host = "http://localhost:11434"   # optional, this is the default
max_tokens_per_request = 4096
```

No API key is required for Ollama — just install [Ollama](https://ollama.com), pull a model (`ollama pull llama3.2`), and point the agent at it. This enables fully private, offline deployments with no cloud dependency.

**Common sections:**

```toml
[agent]
name = "Fluux Agent"
allowed_jids = ["admin@localhost"]

[memory]
backend = "markdown"
path = "./data/memory"
```

Memory is stored as human-readable markdown files — workspace files for global agent configuration and per-JID directories for isolated user data. This makes agent memory inspectable, editable, and git-friendly. Admins can customize agent behavior by creating `instructions.md`, `identity.md`, and `personality.md` in the memory root directory.

## Commands

Messages starting with `/` are intercepted by the runtime and never reach the LLM. They cost zero API calls and respond instantly.

| Command            | Description                                                               |
|--------------------|---------------------------------------------------------------------------|
| `/new` or `/reset` | Archive the current conversation and start a fresh session                |
| `/forget`          | Erase your history, profile, and memory (archived sessions are preserved) |
| `/status`          | Agent uptime, connection mode, LLM model, session stats                   |
| `/ping`            | Check if the agent is alive                                               |
| `/help`            | List available commands                                                   |

## Session Management

Each user has a current conversation session (`history.md`) and optionally archived past sessions. This prevents context from growing unboundedly and lets users start fresh when changing topics.

- **`/new`** archives the current session to `sessions/{YYYYMMDD-HHMMSS}.md` and clears the LLM context.
- **`/forget`** erases the current history, user profile (`user.md`), and memory (`memory.md`) but preserves archived sessions.
- **`/status`** shows the number of messages in the current session and how many sessions have been archived.

Memory layout:

```
data/memory/
├── instructions.md              # Global: agent behavior rules
├── identity.md                  # Global: agent name, personality, background
├── personality.md               # Global: tone, style, quirks
├── {jid}/
│   ├── user.md                  # What the agent knows about this user
│   ├── memory.md                # Long-term notes about this user
│   ├── history.md               # Current session
│   └── sessions/
│       ├── 20250601-143022.md   # Archived session
│       └── 20250602-091500.md   # Another archived session
```

Global workspace files (`instructions.md`, `identity.md`, `personality.md`) are shared across all users and let admins customize the agent without touching code. When no workspace files exist, a built-in default prompt is used. Per-JID directories are strictly isolated — each user (or room) has their own `user.md`, `memory.md`, and conversation history.

See [`data/memory/README.md`](data/memory/README.md) for the full workspace reference, file format details, and OpenClaw migration guide.

## Multi-User Chat (MUC)

Fluux Agent can join XMPP group chat rooms (XEP-0045) and respond when mentioned.

### Configuration

```toml
[[rooms]]
jid = "lobby@conference.localhost"
nick = "FluuxBot"

[[rooms]]
jid = "dev@conference.localhost"
nick = "FluuxBot"
```

The agent joins configured rooms on connect. It records all room messages for context and responds when its nickname is mentioned (e.g., `@FluuxBot what's the status?` or `FluuxBot: hello`). This means the LLM sees the full conversation when it's asked a question, not just the mention.

Each room has its own isolated memory directory, just like 1:1 conversations — the room JID is used as the memory key. All participants in the same room share conversation context.

### Per-room identity

You can give the agent a different persona per room (or per user) by placing workspace files in the JID's memory directory. These override the global files:

```
data/memory/
  identity.md                              # Global: "I am Fluux Agent"
  instructions.md                          # Global rules
  lobby@conference.localhost/
    instructions.md                        # Room override: "You are a support bot..."
  dev@conference.localhost/
    identity.md                            # Room override: "I am DevBot"
    personality.md                         # Room override: "Terse, technical"
```

The lookup order is: **per-JID file → global file → none**. If a per-JID file exists and is non-empty, it wins. Otherwise the global file is used. This works for rooms *and* individual users — no config changes needed, just drop files into the directory.

## Project Structure

```
fluux-agent/
├── src/
│   ├── main.rs                 # Entry point, config loading
│   ├── config.rs               # TOML deserialization + ConnectionMode enum
│   ├── xmpp/
│   │   ├── mod.rs              # Connection factory (dispatches component/client)
│   │   ├── component.rs        # XEP-0114 connection, SHA-1 handshake
│   │   ├── client.rs           # C2S connection (STARTTLS + SASL + bind)
│   │   ├── sasl.rs             # SASL PLAIN + SCRAM-SHA-1 (RFC 5802)
│   │   └── stanzas.rs          # Stanza parsing/construction
│   ├── agent/
│   │   ├── mod.rs
│   │   ├── runtime.rs          # Main agentic loop
│   │   └── memory.rs           # Conversational memory (markdown files)
│   ├── llm/
│   │   ├── mod.rs
│   │   ├── client.rs           # LlmClient trait (provider abstraction)
│   │   ├── anthropic.rs        # Anthropic Claude API client
│   │   └── ollama.rs           # Ollama local model client
│   ├── skills/
│   │   ├── mod.rs
│   │   ├── registry.rs         # Skill discovery and loading
│   │   └── builtin/
│   │       ├── mod.rs
│   │       └── web_search/     # Web search skill (Tavily + Perplexity)
│   └── sandbox/
│       └── mod.rs              # Stub for v0.4 (Wasm + Landlock)
├── data/memory/                # Agent memory (workspace files + per-JID dirs)
├── config/
│   └── agent.example.toml
├── Cargo.toml
├── LICENSE                     # Apache 2.0
└── README.md
```

## Agent Protocol (draft)

Fluux Agent introduces an experimental XMPP namespace `urn:fluux:agent:0` for structured communication between humans and agents:

```xml
<!-- Skill discovery -->
<iq type='get' to='agent.example.com'>
  <query xmlns='urn:fluux:agent:0#skills'/>
</iq>

<!-- Skill execution -->
<iq type='set' to='agent.example.com'>
  <execute xmlns='urn:fluux:agent:0#execute' skill='email-summary'>
    <param name='max_emails'>20</param>
  </execute>
</iq>

<!-- Confirmation request (destructive action) -->
<message from='agent.example.com' to='user@example.com'>
  <body>Send this email to Platform24? (reply yes/no)</body>
  <confirm xmlns='urn:fluux:agent:0#confirm' id='action-7742'>
    <action type='send-email'/>
    <expires>2026-02-08T19:00:00Z</expires>
  </confirm>
</message>
```

## License

The core is under [Apache License 2.0](LICENSE).

Enterprise features (multi-agent federation, multi-tenant, audit, SSO) will be distributed under BSL 1.1 (automatic conversion to Apache 2.0 after 4 years).

## Context

Fluux Agent is developed by [ProcessOne](https://www.process-one.net), the company behind [ejabberd](https://www.ejabberd.im) — the XMPP server that powered the early versions of WhatsApp. 20 years of messaging infrastructure expertise, now applied to building the open foundation for federated AI agent networks.

---

### Fluux Agent vs OpenClaw

Fluux Agent is not an OpenClaw clone in Rust. It's a fundamentally different architecture.

OpenClaw gets the abstraction layer wrong. It treats "connecting to messaging platforms" as a core feature, building bridges to Slack, Discord, Telegram, WhatsApp, etc. This creates an ever-growing matrix of protocol adapters that the project must maintain, each with its own quirks, rate limits, and breakage points.

Fluux Agent inverts the approach: **the agent speaks one native protocol (XMPP) with its full extension ecosystem** — presence, PubSub, message archive, federation, end-to-end encryption. Reaching users on other platforms is a *skill*, not a connection mode. A Telegram skill sends messages via the Telegram API. A Slack skill posts via webhooks. These are actions the agent performs, like sending an email or calling an API — not fundamental architectural layers.

This distinction matters:

- **Bot protocols are connection modes** — how the agent receives instructions (XMPP component, XMPP C2S, and eventually Matrix or IRC as alternative bot frontends).
- **Messaging platforms are skills** — how the agent *acts* on the world (send a Slack message, post to Discord, forward to Telegram). They belong in the skills registry alongside "search the web" or "read an email".

The result is a simpler, more maintainable agent that leverages 20 years of proven messaging infrastructure instead of reinventing it with fragile platform bridges.
