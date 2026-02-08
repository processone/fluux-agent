# Fluux Agent

**A personal, secure, and federated AI agent — powered by XMPP.**

Fluux Agent is an AI agent runtime that connects to any XMPP server — either as an external component (XEP-0114) or as a regular client (C2S with SASL + STARTTLS). It transforms the XMPP protocol — designed for human messaging — into a communication bus for autonomous AI agents.

## Why

OpenClaw demonstrated the massive demand for personal AI assistants that actually act. But its architecture — Node.js bridges to every messaging platform, root system access, JSON file storage — poses fundamental security, reliability, and interoperability problems.

XMPP has solved these problems for 20 years: reliable message routing, presence, PubSub for events, message history storage (MAM), multi-device synchronization, and most importantly **federation** — the ability for agents on different servers to communicate with each other without a centralized platform.

Fluux Agent brings these two worlds together: the power of modern AI agents with the robustness of proven messaging infrastructure.

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
│  │  Client     │ │   loop)      │ │  OpenAI...)  │  │
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

- **Total decoupling** — The agent is a standard XMPP component. It works with any XMPP server, not just ejabberd.
- **Security by design** — Defense-in-depth with 5 independent layers: declarative capabilities, action validation, Wasm sandbox, kernel sandboxing (Landlock/seccomp), and process isolation. The LLM never directly touches the system. See [Security Architecture](docs/SECURITY.md).
- **Proactivity** — Cron jobs, PubSub subscriptions, webhooks. The agent can initiate conversations, not just respond.
- **Federation** — My agent `agent.domain-a.com` talks to your agent `agent.domain-b.com` via XMPP federation. No centralized platform.

## Status

> **v0.1** — Foundation. Dual-mode XMPP connection (component + C2S), agentic loop with Claude API, persistent markdown memory, conversation sessions, and slash commands.

### Roadmap

| Phase | Description | Status |
|-------|-------------|--------|
| **v0.1** | XMPP component + C2S client + agentic loop + Claude API + markdown memory + sessions + slash commands | 🚧 In progress |
| **v0.2** | Skills system, model tiering, Ollama support, proactive context learning | Planned |
| **v0.3** | Proactivity (cron via PubSub, heartbeat) | Planned |
| **v0.4** | Wasm sandbox (wasmtime) + Landlock | Planned |
| **v0.5** | Agent protocol (`urn:fluux:agent:0`) — discovery, execute, confirm | Planned |
| **v1.0** | Agent-to-agent federation, complete documentation | Planned |

## Quick Start

### Prerequisites

- Rust ≥ 1.75
- An XMPP server (ejabberd, Prosody, Openfire...)
- An Anthropic API key (Claude)

Two connection modes are supported:

**Component mode (XEP-0114)** — The agent registers as a subdomain (e.g., `agent.localhost`). Requires server-side configuration but gives the agent its own address namespace.

**Client mode (C2S)** — The agent connects as a regular XMPP user (e.g., `bot@localhost`). No server configuration needed beyond a user account. Supports SASL PLAIN, SCRAM-SHA-1, and STARTTLS.

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

export ANTHROPIC_API_KEY="sk-ant-..."
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

**Common sections:**

```toml
[llm]
provider = "anthropic"
model = "claude-sonnet-4-5-20250929"
api_key = "${ANTHROPIC_API_KEY}"
max_tokens_per_request = 4096

[agent]
name = "Fluux Agent"
allowed_jids = ["admin@localhost"]

[memory]
backend = "markdown"
path = "./data/memory"
```

Memory is stored as human-readable markdown files — one directory per user with `history.md` and `context.md`. This makes agent memory inspectable, editable, and git-friendly.

## Commands

Messages starting with `/` are intercepted by the runtime and never reach the LLM. They cost zero API calls and respond instantly.

| Command | Description |
|---------|-------------|
| `/new` or `/reset` | Archive the current conversation and start a fresh session |
| `/forget` | Erase your history and user context (archived sessions are preserved) |
| `/status` | Agent uptime, connection mode, LLM model, session stats |
| `/ping` | Check if the agent is alive |
| `/help` | List available commands |

## Session Management

Each user has a current conversation session (`history.md`) and optionally archived past sessions. This prevents context from growing unboundedly and lets users start fresh when changing topics.

- **`/new`** archives the current session to `sessions/{YYYYMMDD-HHMMSS}.md` and clears the LLM context.
- **`/forget`** erases the current history and user context but preserves archived sessions.
- **`/status`** shows the number of messages in the current session and how many sessions have been archived.

Memory layout per user:

```
data/memory/{jid}/
├── history.md                  # Current session
├── context.md                  # What the agent knows about the user
└── sessions/
    ├── 20250601-143022.md      # Archived session
    └── 20250602-091500.md      # Another archived session
```

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
│   │   └── anthropic.rs        # Claude API client (reqwest + SSE)
│   ├── skills/
│   │   ├── mod.rs
│   │   ├── registry.rs         # Skill discovery and loading
│   │   └── builtin/
│   │       ├── mod.rs
│   │       └── web_search.rs   # Example skill: web search
│   └── sandbox/
│       └── mod.rs              # Stub for v0.4 (Wasm + Landlock)
├── data/memory/                # Agent memory (one dir per user JID)
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

### Why Apache 2.0 for the core

- **Protocol adoption** — For XMPP to become the standard for AI agents, the reference runtime must be frictionless. AGPL blocks integration in many enterprise environments.
- **Skills ecosystem** — Skill developers shouldn't worry about license contamination.
- **Patent protection** — Apache 2.0 includes an explicit patent grant, unlike MIT.
- **Rust consistency** — The Rust ecosystem is culturally Apache 2.0 / MIT.

## Context

Fluux Agent is developed by [ProcessOne](https://www.process-one.net), the company behind [ejabberd](https://www.ejabberd.im) — the XMPP server that powered the early versions of WhatsApp. 20 years of messaging infrastructure expertise, applied to AI agents.

---

### Fluux Agent vs OpenClaw

Fluux Agent is not an OpenClaw clone in Rust. It's a fundamentally different architecture.

OpenClaw gets the abstraction layer wrong. It treats "connecting to messaging platforms" as a core feature, building bridges to Slack, Discord, Telegram, WhatsApp, etc. This creates an ever-growing matrix of protocol adapters that the project must maintain, each with its own quirks, rate limits, and breakage points.

Fluux Agent inverts the approach: **the agent speaks one native protocol (XMPP) with its full extension ecosystem** — presence, PubSub, message archive, federation, end-to-end encryption. Reaching users on other platforms is a *skill*, not a connection mode. A Telegram skill sends messages via the Telegram API. A Slack skill posts via webhooks. These are actions the agent performs, like sending an email or calling an API — not fundamental architectural layers.

This distinction matters:

- **Bot protocols are connection modes** — how the agent receives instructions (XMPP component, XMPP C2S, and eventually Matrix or IRC as alternative bot frontends).
- **Messaging platforms are skills** — how the agent *acts* on the world (send a Slack message, post to Discord, forward to Telegram). They belong in the skills registry alongside "search the web" or "read an email".

The result is a simpler, more maintainable agent that leverages 20 years of proven messaging infrastructure instead of reinventing it with fragile platform bridges.
