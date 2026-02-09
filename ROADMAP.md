# Roadmap

This document tracks the planned features for Fluux Agent, organized by release milestone.

Items marked **done** are merged. Items marked **next** are the current priority.

---

## v0.1 — Foundation

The minimum viable agent: connect, authenticate, converse, remember.

- [x] XMPP component mode (XEP-0114)
- [x] XMPP C2S client mode (SASL PLAIN + SCRAM-SHA-1 + STARTTLS)
- [x] Agentic loop with Claude API (Anthropic)
- [x] Markdown-based conversational memory (per-user `history.md` + `context.md`)
- [x] Allowed JID authorization
- [x] Chat state notification filtering (XEP-0085)
- [x] Configuration with environment variable expansion
- [x] Conversation sessions (`/new`, `/reset`, session archival)
- [x] Slash commands (runtime-intercepted, never reach the LLM)
- [x] Presence subscription for allowed JIDs (auto-subscribe + auto-accept)
- [x] Typing indicators (outbound `<composing/>` / `<paused/>` / `<active/>`, XEP-0085)
- [x] MUC room joining (XEP-0045) — join configured rooms, respond to mentions, full room context
- [x] Cross-domain message rejection (security default)
- [x] Reconnection with exponential backoff

### Conversation sessions ✓

Implemented: the agent supports discrete sessions per user.

- **`/new` and `/reset`** — Archives the current `history.jsonl` into `sessions/{YYYYMMDD-HHMMSS}.jsonl` and starts a fresh conversation. The LLM sees an empty history.
- **`/forget`** — Erases the current session history and user context. Archived sessions are preserved.
- **Memory layout** — `{jid}/history.jsonl` (current session, JSONL format), `{jid}/sessions/*.jsonl` (archived), `{jid}/user.md` (user profile), `{jid}/memory.md` (long-term notes).
- **`/status`** — Reports message count in current session and number of archived sessions.

#### Future enhancements (not yet implemented)

- **XMPP thread ID mapping** — When the XMPP client sends a `<thread>` element (XEP-0201), the agent maps it to a session. Different thread IDs = different sessions. Messages without a thread ID use the "default" session.
- **Session timeout** — If no message is received for a configurable duration (e.g. 4 hours), the next message implicitly starts a new session. The timeout is per-user.
- **Session context carry-over** — When a new session starts, the agent can optionally summarize the previous session into `context.md`, giving continuity without sending the full old history to the LLM.

### Presence subscription for allowed JIDs ✓

Implemented: automatic roster integration in C2S mode.

- **Proactive subscribe on connect** — After initial presence, the agent sends `<presence type='subscribe'>` to every JID in `allowed_jids`. Allowed users see the agent appear in their contact list.
- **Auto-accept incoming subscriptions** — When an allowed JID requests to subscribe, the runtime responds with `<presence type='subscribed'>` automatically. Unauthorized subscription requests are silently ignored.
- **Presence tracking** — The read loop parses all `<presence>` stanzas (available, unavailable, subscribe, subscribed, unsubscribe, unsubscribed) and dispatches them as `XmppEvent::Presence` to the runtime.
- **C2S only** — In component mode, the server handles routing differently. Presence subscription is a C2S concern.

### Typing indicators (XEP-0085, outbound) ✓

Implemented: the agent sends proper XEP-0085 chat state notifications throughout the response lifecycle.

- **`<composing/>`** — Sent immediately when the agent starts processing a non-slash-command message (before the LLM call). The user's XMPP client shows "bot is typing…".
- **`<active/>`** — Bundled inside the response `<message>` stanza. Clears the typing indicator when the reply arrives. This is the XEP-0085 recommended pattern: chat state rides alongside the body.
- **`<paused/>`** — Sent if the LLM call fails, before the error message. Properly signals that the agent stopped generating without successfully producing a response.
- **Slash commands skip composing** — Commands like `/ping`, `/status` are instant and deterministic. No typing indicator is sent for them.

### Cross-domain message rejection ✓

Implemented: domain-level message filtering as a defense-in-depth security layer.

- **Default: own domain only** — When `allowed_domains` is not set, only messages from the agent's own XMPP domain are accepted. The domain is inferred automatically from the JID (client mode) or component domain (component mode).
- **Explicit allow list** — `allowed_domains = ["localhost", "partner.org"]` accepts messages from specific domains.
- **Wildcard** — `allowed_domains = ["*"]` disables domain filtering (allows federation from any domain — use with caution).
- **Layered with JID filtering** — Domain check runs first, then `allowed_jids`. Both must pass for a message to be processed.
- **Applies to** — 1:1 chat messages and presence subscription requests. MUC messages are already filtered by room configuration.
- **Startup logging** — The effective domain policy is logged at startup.

```toml
[agent]
# Only accept messages from these domains (default: agent's own domain)
# allowed_domains = ["localhost", "partner.example.com"]
# Use ["*"] to allow all domains (not recommended in production)
```

### Slash commands ✓

Messages starting with `/` are intercepted by the runtime before they reach the LLM. They cost zero API calls and their behavior is deterministic — no hallucination risk, no latency.

#### Implemented commands

| Command           | Description                                          | Status |
|-------------------|------------------------------------------------------|--------|
| `/new` / `/reset` | Start a new conversation session (archive current)   | ✓      |
| `/forget`         | Erase the current user's history and context         | ✓      |
| `/status`         | Agent uptime, mode, LLM info, session stats          | ✓      |
| `/help`           | List available commands                              | ✓      |
| `/ping`           | Simple liveness check (responds immediately, no LLM) | ✓      |

#### Planned developer commands

For debugging and development. Can be restricted to a specific admin JID or disabled entirely in production via config.

| Command      | Description                                                                |
|--------------|----------------------------------------------------------------------------|
| `/debug`     | Toggle debug output (echo raw stanzas, LLM request/response, token counts) |
| `/context`   | Show the current user's `context.md` content                               |
| `/history`   | Show the last N messages from the current session                          |
| `/tier`      | Show which model tier/model is being used for this conversation            |
| `/raw <xml>` | Send a raw XML stanza (dev only — for testing XMPP interactions)           |

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

- [x] Skill trait and registry
- [x] LLM tool use integration (agentic loop)
- [x] `LlmClient` trait + Ollama provider (local models via Ollama API)
- [x] Builtin skill: web search (Tavily + Perplexity providers)
- [x] Session file structure migration (markdown → JSONL)
- [x] File attachments — receive images/documents via OOB (XEP-0066), download, pass to LLM
- [x] Release build chain as GitHub Action
- [ ] Builtin skill: URL fetch and summarize
- [ ] Builtin skill: GitHub (issues, PRs, repositories, notifications)
- [ ] Sub-agent spawning (built-in runtime tool, one level deep)
- [ ] Model tiering (route tasks to appropriate model by complexity/cost) + sub-agent model overrides
- [ ] Declarative skill capabilities (TOML manifests)
- [ ] Action plan validation (separate from LLM)
- [ ] Prompt injection detection — scan incoming messages for adversarial patterns before they reach the LLM
- [ ] Credential management (env vars, `.env` fallback, per-skill OAuth storage)
- [ ] Agent-generated skills: template-based REST API skills (no code execution)
- [ ] Bundled REST API skills: JIRA, Front (shipped templates using the REST skill system)
- [ ] Proactive context learning — agent updates `context.md` by summarizing conversations
- [ ] Cost estimation and per-JID quota (token tracking, usage limits, `/usage` command)
- [ ] Persona packages (bundled identity/personality/instructions, `/persona` commands)
- [ ] LLM prompt caching (`cache_control` markers for system prompt and history prefix)
- [ ] Context window management (token-budget history, compaction, memory flush)

### Persona packages

A **persona** is a complete personality configuration that bundles multiple aspects of how the agent behaves. Rather than switching just an identity file, users can switch entire persona packages that include identity, personality, instructions, and custom context.

#### Persona structure

Each persona is a directory containing the files that define it:

```
data/memory/personas/
├── coding-mentor/
│   ├── persona.toml          # Metadata and settings
│   ├── identity.md           # Who the agent is
│   ├── personality.md        # Tone, style, quirks
│   ├── instructions.md       # Behavioral rules
│   └── context.md            # Domain knowledge (optional)
├── support-agent/
│   ├── persona.toml
│   ├── identity.md
│   ├── personality.md
│   └── instructions.md
└── creative-writer/
    └── ...
```

The `persona.toml` manifest:

```toml
[persona]
id = "coding-mentor"
name = "Coding Mentor"
description = "Patient programming tutor focused on teaching concepts"
icon = "👨‍💻"                      # Optional, shown in /persona list

[persona.defaults]
tier = "standard"                  # Default model tier
skills = ["web_search", "url_fetch"]  # Skills available in this persona

[persona.restrictions]
# Optional: limit what this persona can do
allowed_jids = []                  # Empty = all allowed users
max_tokens_per_message = 4096
```

#### Slash commands

| Command           | Description                   |
|-------------------|-------------------------------|
| `/persona`        | Show current active persona   |
| `/persona list`   | List all available personas   |
| `/persona <name>` | Switch to a different persona |
| `/persona reset`  | Return to default persona     |

Example interaction:

```
User: /persona list
Agent: Available personas:
       • coding-mentor — Patient programming tutor focused on teaching concepts
       • support-agent — Professional customer support representative
       • creative-writer — Imaginative storytelling partner
       Current: coding-mentor

User: /persona creative-writer
Agent: Switched to creative-writer persona. I'm now your imaginative storytelling partner!
```

#### Resolution order

When building the system prompt, files are resolved in this order (first found wins):

1. `{jid}/identity.md` — Per-user override (always takes precedence)
2. `personas/{active}/identity.md` — Active persona's file
3. `identity.md` — Global default

This applies to each file type: `identity.md`, `personality.md`, `instructions.md`.

#### Per-user persona persistence

The user's active persona is stored in their JID directory:

```
data/memory/user@example.com/
├── active_persona.txt          # Contains "coding-mentor"
├── history.md
└── context.md
```

The choice persists across sessions. Users can have different active personas — Alice might use `coding-mentor` while Bob uses `support-agent`.

#### Per-JID persona restrictions

Operators can restrict which personas are available to specific users or rooms:

```toml
# In config/agent.toml
[personas]
default = "general-assistant"

# Restrict dev room to coding personas only
[[personas.restrictions]]
jid = "dev@conference.example.com"
allowed = ["coding-mentor", "code-reviewer"]

# Give admin access to all personas
[[personas.restrictions]]
jid = "admin@example.com"
allowed = ["*"]
```

#### Use cases

- **Role switching** — A user switches between "coding mentor" for learning and "code reviewer" for PR feedback
- **Room-specific personas** — The support room gets `support-agent`, the dev room gets `coding-mentor`
- **White-label deployments** — Different personas for different clients, all served by one agent
- **Experimentation** — Try different personality configurations without editing files
- **User preference** — Some users prefer a terse assistant, others prefer a friendly one — let them choose

#### User-defined personas

Beyond admin-defined global personas, users can create their own custom personas:

| Command                  | Description                                |
|--------------------------|--------------------------------------------|
| `/persona create <name>` | Start creating a new persona interactively |
| `/persona edit <name>`   | Modify an existing user persona            |
| `/persona delete <name>` | Remove a user-created persona              |
| `/persona export <name>` | Export persona as shareable TOML           |
| `/persona import`        | Import a shared persona                    |

**Conversational creation** — Creating a persona is interactive:

```
User: /persona create study-buddy
Agent: Let's create your "study-buddy" persona. I'll ask a few questions.

       What should this persona's role be?
User: A patient tutor who helps me study for exams using the Socratic method

Agent: How should it communicate? (tone, style, quirks)
User: Encouraging but challenging, asks lots of questions, uses analogies

Agent: Any specific instructions or rules?
User: Never give direct answers, always guide me to discover them myself

Agent: Created persona "study-buddy". Switch to it with /persona study-buddy
```

**Storage** — User personas are stored in the user's JID directory, isolated from global personas:

```
data/memory/user@example.com/
├── personas/
│   ├── study-buddy/
│   │   ├── persona.toml
│   │   ├── identity.md
│   │   ├── personality.md
│   │   └── instructions.md
│   └── my-therapist/
│       └── ...
├── active_persona.txt
└── history.md
```

**Resolution order** (updated):

1. `{jid}/identity.md` — Per-user file override
2. `{jid}/personas/{active}/identity.md` — User-created persona
3. `personas/{active}/identity.md` — Global persona
4. `identity.md` — Global default

**Sharing personas** — Users can export their personas and share them:

```
User: /persona export study-buddy
Agent: Here's your persona configuration:
       [attached: study-buddy.toml]

       Share this file with others, or ask an admin to add it as a global persona.
```

Admins can promote popular user personas to global availability:

```toml
# In config/agent.toml
[personas]
allow_user_personas = true         # Enable user-created personas
max_user_personas = 5              # Limit per user
allow_sharing = true               # Allow export/import
```

### Agent-generated skills

The agent can create new skills on-the-fly when it encounters an API or service it doesn't have a skill for. This evolves across versions with increasing capability and appropriate safeguards.

#### Evolution across versions

| Version | Capability                 | Safety Mechanism                         |
|---------|----------------------------|------------------------------------------|
| v0.2    | Template-based REST skills | No code execution — declarative only     |
| v0.3    | Supervised proposals       | Human approval gate before enabling      |
| v0.4    | Wasm code generation       | Sandbox enforces capability restrictions |

#### v0.2: Template-based REST skills

The LLM generates a declarative skill definition (no code) for REST APIs:

```toml
# Generated by agent, stored at ~/.fluux-agent/skills/front_conversations/skill.toml
[skill]
type = "rest_api"
id = "front_conversations"
name = "Front Conversations"
description = "List open conversations from Front support inbox"

[skill.capabilities]
network = ["api2.frontapp.com:443"]
credential = ["front-api-key"]

[skill.request]
method = "GET"
url = "https://api2.frontapp.com/conversations"
query = { limit = "100" }
headers = { Authorization = "Bearer ${FRONT_API_KEY}" }

[skill.response]
format = "json"
# jq-style extraction (interpreted, not executed)
extract = "._results[] | select(.status_category == \"open\")"
output_template = """
Open conversations: {{count}}
{{#each items}}
- {{subject}} (from: {{recipient.name}})
{{/each}}
"""
```

The runtime interprets the template — no shell, no arbitrary code. The agent can only:
- Make HTTP requests to declared endpoints
- Use declared credentials
- Transform responses with safe extractors

**Creation flow:**

```
User: "Can you check my Front inbox?"

Agent: I don't have a Front skill yet. Let me create one.

       I'll need your Front API key. You can set it as:
       export FRONT_API_KEY="your-key-here"

       Creating skill: front_conversations
       Endpoint: https://api2.frontapp.com/conversations

       [Skill created and enabled]

       Now let me check your inbox...
```

**Slash commands:**

| Command                 | Description                            |
|-------------------------|----------------------------------------|
| `/skills`               | List all skills (builtin + generated)  |
| `/skill info <name>`    | Show skill definition and capabilities |
| `/skill disable <name>` | Disable a generated skill              |
| `/skill delete <name>`  | Remove a generated skill               |

#### Bundled REST API skills

The agent ships with pre-built REST API skill templates for common services. These use the same declarative system as agent-generated skills but are curated and tested:

| Skill | Service | Operations |
|-------|---------|------------|
| `jira` | Atlassian JIRA | List/search issues, get issue details, create issues, add comments, transition status |
| `front` | Front | List conversations, get conversation details, reply to conversations, list inboxes |

**Configuration:**

```toml
[skills.jira]
enabled = true
host = "https://yourcompany.atlassian.net"
email = "${JIRA_EMAIL}"
api_token = "${JIRA_API_TOKEN}"
default_project = "PROJ"          # Optional: default project key

[skills.front]
enabled = true
api_key = "${FRONT_API_KEY}"
```

**Why bundled instead of native:**

- **Simpler maintenance** — REST API changes only require updating TOML templates, not recompiling
- **User-extensible** — Users can copy and modify bundled skills for custom workflows
- **Demonstrates the system** — Shows how agent-generated skills work with real-world APIs
- **Lower priority APIs** — GitHub warrants native integration; JIRA and Front are common but not universal

The bundled skills live in `data/skills/` and are registered at startup alongside native skills.

#### v0.3: Supervised proposals

For skills that need more than REST templates (complex logic, multiple API calls), the agent proposes a skill but requires human approval:

```
User: "Monitor my Front inbox and notify me of urgent tickets"

Agent: This requires a custom skill with scheduling. Here's my proposal:

       Name: front_urgent_monitor
       Type: Scheduled task (every 15 min)
       Logic:
         1. Fetch conversations from Front API
         2. Filter by priority tag "urgent"
         3. Compare against last check (stored state)
         4. Notify user of new urgent tickets

       Capabilities needed:
         - network:api2.frontapp.com:443
         - credential:front-api-key
         - schedule:*/15 * * * *
         - state:front_urgent_monitor (persistent storage)

       Draft saved to: ~/.fluux-agent/skills/front_urgent_monitor/

       To enable: fluux-agent skill enable front_urgent_monitor
       To review code: fluux-agent skill show front_urgent_monitor
```

The skill is inert until explicitly enabled. Operators can review the generated code before activation.

#### v0.4: Wasm code generation

With the Wasm sandbox in place, the agent can generate actual code (Rust, AssemblyScript) that compiles to Wasm:

```
Agent: I'll create a custom Front skill with advanced filtering.

       Generating Rust code...
       Compiling to Wasm...

       Skill: front_advanced
       Capabilities (enforced by sandbox):
         ✓ network:api2.frontapp.com:443
         ✓ credential:front-api-key
         ✗ filesystem (not requested)
         ✗ shell (not available)

       The skill runs in a memory-isolated sandbox with CPU metering.
       Even if the code has bugs, it cannot escape the sandbox.

       Enable with: /skill enable front_advanced
```

The Wasm sandbox enforces declared capabilities at runtime — the code physically cannot access resources it didn't declare, regardless of what the LLM generated.

#### Security considerations

| Risk                               | v0.2 Mitigation                     | v0.3 Mitigation              | v0.4 Mitigation              |
|------------------------------------|-------------------------------------|------------------------------|------------------------------|
| Arbitrary code execution           | No code — templates only            | Human review before enable   | Wasm sandbox                 |
| Prompt injection → malicious skill | Limited to REST calls               | Human approval gate          | Capability enforcement       |
| Data exfiltration                  | Declared endpoints only             | Human reviews network access | Sandbox + audit logging      |
| Credential theft                   | Credentials never in generated code | Credential access logged     | Sandbox isolates credentials |

#### Storage layout

```
~/.fluux-agent/
└── skills/
    ├── front_conversations/        # Generated skill
    │   ├── skill.toml              # Skill manifest
    │   ├── generated.json          # Generation metadata (prompt, timestamp)
    │   └── enabled                 # Marker file (skill is active)
    ├── front_urgent_monitor/       # Proposed but not yet enabled
    │   ├── skill.toml
    │   ├── src/
    │   │   └── lib.rs              # Generated code (v0.4)
    │   └── generated.json
    └── registry.toml               # Index of all generated skills
```

### Credential management

Skills need access to API keys, OAuth tokens, and other secrets. The agent uses a **layered credential resolution** approach that prioritizes security while remaining practical for development and deployment.

#### Resolution order

Credentials are resolved in priority order (first match wins):

1. **Process environment** — Explicit env vars set by the parent process or container orchestrator
2. **Config file** — Values in `config/agent.toml` using `${VAR}` substitution
3. **Local `.env` file** — `.env` in the current working directory
4. **User fallback** — `~/.fluux-agent/.env` for user-level defaults
5. **Per-skill storage** — `~/.fluux-agent/credentials/<skill>/` for OAuth tokens

Neither `.env` file overrides existing environment variables — explicit always wins.

#### Environment variable substitution

The existing `${VAR}` syntax in TOML config is expanded at load time:

```toml
[skills.web_search]
provider = "tavily"
api_key = "${TAVILY_API_KEY}"

[[skills.mcp.servers]]
name = "github"
command = "npx"
args = ["-y", "@anthropic/mcp-server-github"]
env = { GITHUB_TOKEN = "${GITHUB_TOKEN}" }
```

- Missing or empty variables cause a startup error (fail-fast)
- Escape literal `$` with `$$` (e.g., `$${NOT_A_VAR}`)
- Substitution happens once at config load, not per-request

#### Per-skill credential storage

Some skills need persistent credentials beyond simple API keys:

- **OAuth tokens** — Skills that authenticate via OAuth (Google, Microsoft, Slack) store refresh tokens on disk
- **Session cookies** — Skills that scrape authenticated web pages may need persistent sessions
- **Per-user credentials** — Some skills may need different credentials per JID (e.g., each user's own GitHub token)

Storage layout:

```
~/.fluux-agent/
├── .env                           # User-level env fallback
└── credentials/
    ├── github/
    │   └── oauth.json             # { "access_token": "...", "refresh_token": "...", "expires_at": ... }
    ├── google/
    │   └── oauth.json
    └── web_search/
        └── api_key.txt            # Simple key storage (alternative to env var)
```

#### Credential capability

Skills declare credential requirements in their manifest:

```toml
[skill.capabilities]
credential = ["github-token", "slack-oauth"]
```

The runtime validates that required credentials are available before enabling the skill. Missing credentials cause the skill to be disabled with a warning, not a fatal error.

#### Security considerations

- **Never log credentials** — The config loader redacts `${...}` values in debug output
- **File permissions** — Credential files are created with `0600` (owner read/write only)
- **No disk storage by default** — API keys from env vars are never written to disk; only OAuth flows that require persistence use disk storage
- **Encryption at rest (future)** — Optional encryption of credential files using a master key from env var or system keychain

### Session file structure migration ✓

Implemented: conversation history migrated from markdown to JSONL format.

- **JSONL format** — Session files are `history.jsonl` with one JSON object per line. `SessionEntry` enum: `Header` (version, created, jid) and `Message` (role, content, msg_id, sender, ts).
- **Structured metadata** — Message ID, sender, and timestamp are stored as structured JSON fields, never embedded in content text.
- **Clean LLM context** — `build_message_for_llm()` constructs messages with only conversational content (no runtime metadata). MUC sender attribution uses text prefix (`"alice@muc: Hello!"`); 1:1 messages have no prefix.
- **Backward compatibility** — `session_count()` counts both `.jsonl` and `.md` files. Old markdown archives are preserved.

### LLM prompt caching

The Anthropic Messages API is stateless — every request must include the full conversation history. For multi-turn conversations, this means replaying the system prompt and all prior messages on every turn, which is expensive in both cost and latency.

Anthropic's **prompt caching** (`cache_control` breakpoints) lets the API reuse cached KV computations when the prefix of the messages array matches a previous request. The replayed portion is processed at reduced cost (~10% of base input price on cache hits) and significantly lower latency.

**Implementation plan:**
- Add `cache_control: { type: "ephemeral" }` breakpoints to the system prompt and at the boundary between history and the new user message
- The system prompt (persona + workspace context) changes rarely and benefits most from caching
- History messages form a growing prefix — each new turn extends the cached prefix by one exchange
- Track cache hit/miss rates via the `cache_creation_input_tokens` and `cache_read_input_tokens` fields in the API response
- Expose cache stats alongside token usage in `/status` or logging

This pairs well with increasing `MAX_HISTORY` beyond the current 20 messages, since caching makes longer histories affordable.

### Context window management

Currently, the agent uses a simple `MAX_HISTORY = 20` message limit: only the last 20 messages are replayed to the LLM on each turn. Older messages are silently dropped from the LLM's view (though they remain in `history.md` on disk). This is cheap and simple, but has serious drawbacks:

- **MUC rooms exhaust the window quickly** — with 5 participants, 20 messages covers only ~4 exchanges
- **Fixed message count ignores message size** — 20 short messages use far fewer tokens than 20 long messages with attachments
- **No graceful degradation** — when messages fall off the window, the context is lost abruptly with no summary

**Planned improvements**, inspired by OpenClaw's multi-layered approach:

#### 1. Token-budget-based history (replace message count limit)

Replace `MAX_HISTORY = 20` with a configurable token budget. Instead of counting messages, estimate tokens and include as many messages as fit within the budget. This adapts naturally to message length and model context size.

```toml
[llm]
history_token_budget = 50000   # max tokens allocated to conversation history
# Remaining context budget goes to: system prompt + workspace + new message + response
```

Token estimation can use a simple heuristic (4 chars ≈ 1 token) or the `tiktoken` crate for precision. The budget should be model-aware — a 200K context model can afford more history than an 8K model.

Separate defaults for 1:1 and MUC:

```toml
[llm]
dm_history_token_budget = 50000     # 1:1 conversations
muc_history_token_budget = 80000    # MUC rooms (more participants, need more context)
```

#### 2. Compaction (summarization of older history)

When the conversation exceeds the token budget, instead of dropping old messages silently, summarize them into a compact preamble. This preserves important context (decisions made, facts stated, user preferences) while freeing token space for recent messages.

**How it works:**
- When a new message would push the total beyond the budget, trigger compaction
- Take the oldest N messages that need to be evicted and send them to a cheap/fast model (e.g. Haiku) with the prompt: "Summarize this conversation segment concisely, preserving key facts, decisions, and user preferences"
- Replace those N messages with a single `[system]` or `[summary]` block containing the summary
- Recent messages after the compaction point are preserved verbatim
- The summary is written to disk (in the session file) so it persists across restarts

**Compaction result format** in history (JSONL):

```jsonl
{"Summary":{"compacted_count":45,"content":"Alice asked about deploying Fluux Agent. Bob suggested using Docker. The team decided on a Kubernetes deployment with Helm charts. Key decisions: PostgreSQL for persistence, Redis for caching.","ts":"2025-01-15T10:30:00Z"}}
{"Message":{"role":"user","content":"OK, I'll draft the Helm chart today.","msg_id":"abc-789","sender":"alice@muc","ts":"2025-01-15T10:31:00Z"}}
{"Message":{"role":"assistant","content":"Great! Let me know if you need help with the values.yaml structure.","msg_id":"def-012","sender":null,"ts":"2025-01-15T10:31:05Z"}}
```

The `Summary` entry type is added to the `SessionEntry` enum alongside `Header` and `Message`, containing the compaction metadata and summarized content.

#### 3. Memory flush (persistent context extraction)

Before compaction runs, optionally trigger a "memory flush" — an LLM call that extracts important long-term facts from the about-to-be-compacted messages and writes them to `context.md` (or `memory.md`). This ensures that critical information survives compaction:

- User preferences and facts ("Alice prefers Python", "Bob's timezone is CET")
- Decisions and agreements ("Team decided to use gRPC for inter-service communication")
- Project context ("Current sprint focuses on authentication refactor")

This is essentially the "proactive context learning" item already in the v0.2 checklist, but triggered automatically by compaction rather than only manually.

#### 4. MUC-specific tuning

MUC rooms have unique context management needs:

- **Higher default token budget** — multiple participants generate more messages
- **Participant-aware compaction** — when summarizing, preserve who said what (the `[from:]` tags are critical here)
- **Selective attention** — if the bot is only mentioned occasionally, most room messages are background context. Compaction could prioritize preserving messages that mention the bot or contain decisions

#### 5. Configurable per-user/per-room overrides

```toml
# Global defaults
[llm]
dm_history_token_budget = 50000
muc_history_token_budget = 80000

# Per-room override
[[rooms]]
jid = "dev@conference.example.com"
history_token_budget = 120000    # busy room, needs more context

# Per-user override (future)
# [users."alice@example.com"]
# history_token_budget = 100000
```

#### Implementation priority

1. **Token-budget history** — replace `MAX_HISTORY = 20`, biggest immediate impact
2. **Prompt caching** — reduce cost of longer histories (see section above)
3. **Compaction** — graceful degradation for long conversations
4. **Memory flush** — long-term fact extraction tied to compaction
5. **Per-room/per-user overrides** — fine-tuning for specific use cases

### Skills system and agentic loop ✓

Implemented: skills are LLM tools with a full agentic loop.

- **`Skill` trait** in `src/skills/mod.rs` — `name()`, `description()`, `parameters_schema()`, `capabilities()`, `execute()`. Each skill provides a JSON Schema for its parameters.
- **`SkillRegistry`** in `src/skills/registry.rs` — HashMap-based registry with `register()`, `get()`, `tool_definitions()`. Skills are registered at startup based on config.
- **`ToolDefinition`** — `name`, `description`, `input_schema` — serializes to both Anthropic and Ollama API formats.
- **Agentic loop** — `agentic_loop()` in `runtime.rs` loops calling the LLM, executes skills on `tool_use`, feeds `tool_result` back. `MAX_TOOL_ROUNDS = 10` safety limit. After the limit, forces a final call without tools.
- **Anthropic tool use** — `InputContentBlock::ToolUse`/`ToolResult`, `ResponseContentBlock` tagged enum (Text/ToolUse). `StopReason` enum: `EndTurn`, `ToolUse`, `MaxTokens`.
- **Ollama tool use** — OpenAI-style tool definitions, `role: "tool"` for results, synthesized tool IDs.
- **Error handling** — Skill errors are sent as `tool_result` content (don't abort the loop); unknown tool names produce error `tool_result`.
- **Web search skill** — `WebSearchSkill` with multi-provider architecture (`SearchProvider` trait). Tavily and Perplexity providers, configurable via `[skills.web_search]` TOML section.

```toml
# config/agent.toml — skill configuration
[skills.web_search]
provider = "tavily"               # "tavily" or "perplexity"
api_key = "${TAVILY_API_KEY}"
max_results = 5
```

### GitHub skill

A native builtin skill for interacting with GitHub repositories, issues, pull requests, and notifications. Unlike using the MCP bridge with `@anthropic/mcp-server-github`, a native skill has no Node.js dependency and integrates tightly with the agent's capabilities system.

#### Tools exposed to the LLM

| Tool                     | Description                                           |
|--------------------------|-------------------------------------------------------|
| `github_list_issues`     | List issues in a repository (with filters)            |
| `github_get_issue`       | Get details of a specific issue                       |
| `github_create_issue`    | Create a new issue                                    |
| `github_comment_issue`   | Add a comment to an issue                             |
| `github_list_prs`        | List pull requests (open, closed, merged)             |
| `github_get_pr`          | Get PR details including diff stats                   |
| `github_list_repos`      | List repositories for a user or organization          |
| `github_search`          | Search issues, PRs, code, or repositories             |
| `github_notifications`   | List unread notifications                             |

#### Configuration

```toml
[skills.github]
enabled = true
token = "${GITHUB_TOKEN}"         # Personal access token or GitHub App token
default_owner = "fluux"           # Optional: default org/user for unqualified repo names
```

#### Use cases

- **Issue triage** — "Show me open issues labeled 'bug' in fluux-agent"
- **PR review assistance** — "Summarize the changes in PR #42"
- **Notifications** — "What GitHub notifications do I have?"
- **Cross-repo search** — "Find all issues mentioning 'memory leak' across my repos"
- **Project management** — "Create an issue for the auth refactor we discussed"

#### Why native instead of MCP

- **No runtime dependency** — Works without Node.js/npx installed
- **Lower latency** — No process spawning overhead per request
- **Capability enforcement** — Integrated with the agent's capability system
- **Optimized for common flows** — Can batch API calls, cache responses, handle pagination intelligently

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

| Source                                      | Tier       | Rationale                                             |
|---------------------------------------------|------------|-------------------------------------------------------|
| Interactive user message (default)          | `standard` | General conversation needs good reasoning             |
| Skill with `tier = "light"` in manifest     | `light`    | Skill author knows the task is simple                 |
| Skill with `tier = "vision"` in manifest    | `vision`   | Skill requires image understanding                    |
| Proactive cron job (v0.3)                   | `light`    | Scheduled tasks are typically routine                 |
| Context summarization                       | `light`    | Summarizing a session into `context.md` is mechanical |
| Complex planning (agent detects multi-step) | `heavy`    | Explicit escalation for hard problems                 |

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

#### Named agent definitions

When a skill or delegated task spawns a sub-agent (an independent LLM call chain with its own system prompt and context), the operator defines it as a **named agent** in TOML. Each named agent is a complete profile — not just a model override, but a full definition of what the agent can do, how it behaves, and what it has access to:

```toml
[llm.tiers]
heavy = "anthropic:claude-sonnet-4-5-20250929"
standard = "anthropic:claude-sonnet-4-5-20250929"
light = "ollama:llama3.1:8b"

# Named agent definitions
[llm.agents.researcher]
model = "anthropic:claude-sonnet-4-5-20250929"
persona = "researcher"                          # Persona package to use (from data/memory/personas/)
skills = ["web_search", "url_fetch"]            # Skills this agent can access (allowlist)
max_tokens = 4096                               # Per-request token limit

[llm.agents.summarizer]
model = "ollama:llama3.1:8b"
instructions = "Summarize concisely in 3 bullet points. Never exceed 200 words."
skills = []                                     # No tool access — pure text generation

[llm.agents.code-reviewer]
model = "anthropic:claude-sonnet-4-5-20250929"
persona = "code-reviewer"
skills = ["web_search", "url_fetch"]
instructions = "Focus on correctness, security, and performance. Be direct."
max_tokens = 8192

[llm.agents.translator]
model = "ollama:mistral:7b"
instructions = "Translate accurately, preserving tone and intent. Return only the translation."
skills = []
```

**Agent definition fields:**

| Field          | Required | Description                                                                                                                            |
|----------------|----------|----------------------------------------------------------------------------------------------------------------------------------------|
| `model`        | No       | Model identifier (provider:model). Falls back to tier default.                                                                         |
| `persona`      | No       | Persona package name. Loads identity, personality, instructions from the persona directory.                                            |
| `instructions` | No       | Inline system prompt override. If both `persona` and `instructions` are set, instructions are appended to the persona's prompt.        |
| `skills`       | No       | Allowlist of skills the agent can use. If omitted, inherits the parent's full skill set. If set to `[]`, the agent has no tool access. |
| `max_tokens`   | No       | Per-request token limit. Falls back to `[llm].max_tokens_per_request`.                                                                 |

Resolution order for model: `[llm.agents.<name>].model` → skill manifest `tier` → `[llm.tiers]` default.

Resolution order for system prompt: `instructions` field → `persona` package → parent agent's prompt.

**The agent list is operator-defined, not hardcoded.** The runtime does not ship with predefined agents. Whatever names appear under `[llm.agents.*]` become available to the `spawn_agent` tool. The LLM sees the list of available agent names in its tool description and chooses which to spawn based on the task.

**Use cases:**

- **Cost control** — A summarizer sub-agent runs on a cheap local model with no tool access, while a code-review sub-agent uses the strongest reasoning model with full skills. The operator chooses per sub-agent rather than per tier.
- **Specialization** — A translator sub-agent uses a model fine-tuned for multilingual tasks with constrained instructions. A researcher sub-agent gets web search skills and a research-focused persona. Each agent gets exactly the model, tools, and behavior profile its job requires.
- **Security isolation** — A sub-agent that only needs to summarize text gets `skills = []` — no tool access at all. A sub-agent that needs web access gets only `["web_search", "url_fetch"]`. The operator controls the blast radius per agent.
- **Development vs. production** — During development, override all sub-agents to use a local Ollama model for fast iteration. In production, point critical sub-agents at Claude while keeping routine ones local.
- **Agent-to-agent federation (v1.0)** — When agent A delegates a task to agent B, agent B's model config is independent. Named agent definitions are the single-process version of this pattern — they establish the config shape that later maps to federated agents, where each `[llm.agents.*]` entry could become a remote JID.

Named agent definitions compose with tier escalation: a sub-agent can start at its configured model but escalate to `heavy` if the agentic loop determines the task requires deeper reasoning.

### Sub-agent spawning

Sub-agent spawning is a **core runtime capability**, not a skill. The runtime exposes a built-in `spawn_agent` tool to the LLM, which the model can invoke to delegate subtasks to independent worker sessions. This follows the same architectural pattern as OpenClaw's `sessions_spawn`: the orchestrating LLM decides when task decomposition is warranted, and the runtime manages the lifecycle.

#### Why a built-in tool, not a skill

Skills are sandboxed, capability-restricted modules for interacting with external services. Sub-agent spawning is fundamentally different — it creates new LLM sessions within the runtime itself, with access to memory, tools, and XMPP messaging. Making it a built-in tool means:

- **No capability escape** — The runtime controls which sub-agents can be spawned and what they can access, rather than delegating this to a sandboxed module.
- **Session isolation** — Each sub-agent gets its own conversation context, history, and tool set. The runtime manages this directly.
- **Lifecycle management** — Timeouts, cancellation, result collection, and cleanup are runtime concerns.

#### The `spawn_agent` tool

Exposed to the LLM alongside skills in the `tools[]` array:

```json
{
  "name": "spawn_agent",
  "description": "Spawn a sub-agent to handle a subtask independently. The sub-agent runs with its own context and returns the result when done.",
  "input_schema": {
    "type": "object",
    "properties": {
      "task": { "type": "string", "description": "The task for the sub-agent" },
      "agent_name": { "type": "string", "description": "Named agent config from [llm.agents.*]" },
      "timeout_seconds": { "type": "integer", "description": "Max execution time (default: 120)" }
    },
    "required": ["task"]
  }
}
```

The LLM decides when to spawn. Typical triggers:
- Complex request that benefits from decomposition (research + summarize + format)
- Parallel independent subtasks (translate to 3 languages simultaneously)
- Task requiring a specialized model (code review needs heavy reasoning)

#### One level deep — no recursive spawning

Sub-agents **cannot spawn further sub-agents**. This is enforced by the runtime: the `spawn_agent` tool is not included in a sub-agent's tool set. This prevents:

- Unbounded recursion and runaway costs
- Complex dependency chains that are hard to debug
- Context explosion from deeply nested agent trees

If a task truly requires multi-level decomposition, the orchestrating agent should plan the full decomposition upfront and spawn all workers itself.

#### Session isolation

Each sub-agent runs in an isolated session. When a named agent config is specified, the sub-agent inherits its persona, skills allowlist, instructions, and model:

| Aspect               | Main agent                         | Sub-agent                                                                                                      |
|----------------------|------------------------------------|----------------------------------------------------------------------------------------------------------------|
| Conversation history | Full user history                  | Empty (only the task prompt)                                                                                   |
| System prompt        | Global persona + per-JID overrides | Named agent's `persona` + `instructions`, or parent's prompt if unnamed                                        |
| Workspace files      | Per-JID overrides                  | Inherits main agent's workspace                                                                                |
| Memory access        | Full read/write                    | Read-only (no side effects)                                                                                    |
| Tool set             | All skills + `spawn_agent`         | Named agent's `skills` allowlist (no `spawn_agent`). If unnamed, inherits parent's skills minus `spawn_agent`. |
| XMPP messaging       | Can send messages                  | Cannot send messages directly                                                                                  |
| Model                | Config default                     | Named agent's `model`, or skill tier, or config default                                                        |
| Token limit          | `max_tokens_per_request`           | Named agent's `max_tokens`, or config default                                                                  |

#### Result flow

```
User message
    ↓
Main agent (agentic loop)
    ↓
LLM decides: "I need to research X and summarize Y in parallel"
    ↓
tool_use: spawn_agent(task="research X", agent_name="researcher")
tool_use: spawn_agent(task="summarize Y", agent_name="summarizer")
    ↓
Runtime spawns two sub-agent sessions concurrently
    ↓
Sub-agents complete → results returned as tool_result
    ↓
Main agent synthesizes results → final response to user
```

#### Configuration

Sub-agent spawning is enabled by default but can be restricted:

```toml
[agent]
# Sub-agent limits
max_concurrent_subagents = 3    # Per conversation (default: 3)
subagent_timeout = 120          # Default timeout in seconds
allow_subagents = true          # Set to false to disable entirely

# Named agent definitions live under [llm.agents.*] (see "Named agent definitions" above).
# The agent_name parameter in spawn_agent maps to these definitions.
# If spawn_agent is called without an agent_name, the sub-agent inherits the parent's
# config minus spawn_agent access.
```

#### XMPP-native sub-agents (v1.0)

In v1.0 (federation), sub-agent spawning evolves into agent-to-agent delegation over XMPP. Instead of spawning an in-process session, the orchestrating agent sends a task to another XMPP agent via IQ stanza. The config shape is the same — `[llm.agents.*]` — but the runtime dispatches to a remote JID instead of a local session. This is why sub-agent spawning is designed as a runtime capability from the start: the abstraction must support both local and federated execution.

### Local models via Ollama ✓

Implemented: the `LlmClient` trait abstracts the LLM provider, with two implementations shipping in v0.2.

- **`LlmClient` trait** in `src/llm/client.rs` — `complete()` + `description()`, `Send + Sync`, object-safe. `Arc<dyn LlmClient>` enables thread-safe sharing across spawned tasks.
- **`AnthropicClient`** — talks to the Claude Messages API, supports tool use, multi-modal content (images, documents).
- **`OllamaClient`** — talks to the [Ollama REST API](https://github.com/ollama/ollama/blob/main/docs/api.md) (`POST /api/chat`). Translates tool definitions to OpenAI-style format, tool results to `role: "tool"` messages, synthesizes tool IDs (`ollama_tool_{index}`). Multi-modal content gracefully degraded with `[Unsupported: ...]` placeholder.
- **Provider dispatch** in `main.rs` — `match config.llm.provider.as_str()` selects `AnthropicClient` or `OllamaClient`.
- **No API key required for Ollama** — enables fully private, offline deployments with no cloud dependency.

Configuration:

```toml
# Anthropic (cloud)
[llm]
provider = "anthropic"
model = "claude-haiku-4-5-20250110"
api_key = "${ANTHROPIC_API_KEY}"

# Ollama (local)
[llm]
provider = "ollama"
model = "llama3.2"
host = "http://localhost:11434"   # optional, this is the default
```

---

## v0.3 — Proactivity

The agent initiates, not just responds.

- [ ] Advanced MUC — room-specific system prompts, invite handling, activation modes (mention vs. all)
- [ ] React to user presence changes (e.g., greet on login, trigger deferred tasks when user comes online)
- [ ] React to user PEP events (XEP-0163) — mood, activity, tune, location, avatar changes
- [ ] Cron-based scheduled tasks (via PubSub or internal scheduler)
- [ ] Heartbeat / keepalive for long-lived connections
- [ ] Webhook ingestion — external events trigger agent actions
- [ ] PubSub subscription — agent reacts to XMPP PubSub events
- [ ] Mastodon integration — skill + inbound event channel via ActivityPub
- [ ] MCP bridge — leverage existing MCP servers as skills
- [ ] Agent-generated skills: supervised proposals (LLM drafts, human approves)
- [ ] XMPP Stream Management (XEP-0198) — message acknowledgment, session resumption, reliability for unstable networks

### Presence-based proactivity

The agent already tracks presence events (available/unavailable) for allowed JIDs. In v0.3, the runtime can **act on** these events instead of just logging them:

- **Greeting on login** — When a user comes online, the agent can send a welcome message, daily summary, or pending notifications.
- **Deferred task delivery** — If the agent completes a background task while the user is offline, it queues the result and delivers it when the user's presence changes to `available`.
- **Offline cleanup** — When a user goes offline, the agent can archive the session or save context.
- **Configurable triggers** — Not all presence changes should trigger actions. A TOML config controls which events fire which behaviors, so operators can disable greetings or limit proactivity.

### PEP event reactions (XEP-0163)

XMPP Personal Eventing Protocol (PEP) lets users publish rich status information: mood (XEP-0107), user activity (XEP-0108), user tune (XEP-0118), geolocation (XEP-0080), avatar changes (XEP-0084). The agent can subscribe to these events and use them as contextual signals:

- **Context enrichment** — If a user publishes "mood: stressed", the agent can adjust its tone. If "activity: on the phone", it can defer non-urgent messages.
- **Proactive suggestions** — Location changes could trigger travel-related reminders. Activity changes could prompt relevant information.
- **Privacy-first** — PEP events are only processed for allowed JIDs. The agent never stores or forwards PEP data to third parties. PEP subscription is opt-in via config.

### Advanced MUC (XEP-0045)

Basic MUC support (join rooms, respond to mentions, room-scoped memory with full conversation context) is implemented in **v0.1**. In v0.3, we extend it with:

- **Room-specific system prompts** — A `{room_jid}/instructions.md` file that overrides the global instructions for that room. Useful for specialized rooms (e.g., a support room gets support-specific instructions).
- **Activation modes** — Configurable per-room: `"mention"` (default, respond only when @mentioned) vs. `"all"` (respond to every message). Useful for rooms where the agent is the primary assistant.
- **Invite handling** — The agent can accept MUC invitations from allowed JIDs and auto-join.
- **Participant awareness** — Track MUC occupant list (via presence) and address responses to specific participants.
- **Leave/rejoin** — Handle room kicks, disconnections, and automatic rejoin with backoff.

```toml
[[rooms]]
jid = "dev@conference.localhost"
nick = "fluux-agent"
activation = "mention"       # "mention" (default) or "all"
```

### Cost estimation and per-JID quota

Track LLM token usage per user and enforce configurable spending limits. This prevents runaway costs and lets operators control who consumes how many resources:

- **Token tracking** — Every LLM call records input/output tokens per bare JID. Stored in memory alongside conversation data.
- **Cost estimation** — Map token counts to approximate USD cost based on model pricing (configurable per-model in TOML).
- **Per-JID quotas** — Configurable daily/monthly token or cost limits per user. When a user exceeds their quota, the agent responds with a friendly limit message instead of calling the LLM.
- **`/usage` command** — Users can check their own token consumption and remaining quota.
- **Admin visibility** — Admin JIDs can query any user's usage via `/usage <jid>`.

```toml
[quota]
# Default daily token limit per user (0 = unlimited)
daily_tokens = 100000
# Per-model cost (USD per million tokens) for estimation
[quota.cost]
"claude-sonnet-4-5-20250929" = { input = 3.0, output = 15.0 }
"claude-haiku-3-5-20241022" = { input = 0.25, output = 1.25 }
```

### MCP bridge

The [Model Context Protocol](https://modelcontextprotocol.io/) (MCP) is an open standard for connecting AI assistants to external tools and data sources. Rather than requiring all skills to be rewritten as native Wasm modules, the agent can leverage the existing MCP ecosystem through a bridge skill.

#### How it works

The agent acts as an **MCP client**, spawning and managing MCP servers as child processes:

```
┌─────────────────────────────────────────────────────┐
│                    LLM (Claude)                     │
│         sees: web_search, front_list_inbox,         │
│               github_create_issue, ...              │
└──────────────────────┬──────────────────────────────┘
                       │
┌──────────────────────▼──────────────────────────────┐
│                  Skill Registry                     │
├─────────────────┬───────────────┬───────────────────┤
│  Native Wasm    │   Builtin     │    MCP Bridge     │
│  (v0.4)         │   (v0.2)      │    (v0.3)         │
│                 │               │                   │
│  *.wasm files   │  web_search   │  ┌─────────────┐  │
│                 │  url_fetch    │  │ MCP Client  │  │
│                 │               │  └──────┬──────┘  │
└─────────────────┴───────────────┴─────────┼─────────┘
                                            │ stdio/SSE
                              ┌─────────────┴─────────────┐
                              │      MCP Servers          │
                              │  (external processes)     │
                              │                           │
                              │  front-server  github-srv │
                              └───────────────────────────┘
```

1. **Startup** — The agent spawns configured MCP servers as child processes
2. **Discovery** — Connects via stdio/SSE and discovers available tools via `tools/list`
3. **Registration** — MCP tools are registered in the SkillRegistry alongside native skills
4. **Execution** — When the LLM requests an MCP tool, the bridge forwards the call and returns the result
5. **Lifecycle** — MCP servers are monitored, restarted on crash, and cleanly shut down

#### Configuration

```toml
[skills.mcp]
enabled = true

[[skills.mcp.servers]]
name = "front"
command = "npx"
args = ["-y", "@anthropic/mcp-server-front"]
env = { FRONT_API_KEY = "${FRONT_API_KEY}" }
# Capability restrictions (validated before forwarding)
capabilities = ["network:api.frontapp.com:443"]

[[skills.mcp.servers]]
name = "github"
command = "npx"
args = ["-y", "@anthropic/mcp-server-github"]
env = { GITHUB_TOKEN = "${GITHUB_TOKEN}" }
capabilities = ["network:api.github.com:443"]

[[skills.mcp.servers]]
name = "filesystem"
command = "/usr/local/bin/mcp-fs-server"
args = ["--root", "/home/user/documents"]
transport = "stdio"
capabilities = ["filesystem:/home/user/documents:read"]
```

#### Security considerations

MCP servers are **less sandboxed** than native Wasm skills:

| Aspect | Native Wasm | MCP Bridge |
|--------|-------------|------------|
| Memory isolation | Wasm linear memory | Process boundary |
| Syscall filtering | seccomp whitelist | None (trusts server) |
| Capability enforcement | Host functions | Declared, not enforced |
| Crash isolation | Wasm trap | Process restart |

To mitigate risks:

- **Capability declaration required** — MCP servers must declare their capabilities in the config. The bridge logs violations but cannot enforce them at runtime (the MCP server runs unsandboxed).
- **Process isolation** — Each MCP server runs in a separate process. A crash or hang doesn't affect the agent.
- **Optional Landlock wrapper** — On Linux, MCP servers can be spawned inside a Landlock sandbox that restricts filesystem access to declared paths.
- **Audit logging** — All MCP tool calls are logged with parameters and results for compliance.

#### Use cases

- **Migration from OpenClaw** — Bring existing MCP server configurations with minimal changes
- **Rapid prototyping** — Use community MCP servers before building native skills
- **Third-party integrations** — Leverage MCP servers for services like Slack, Notion, Linear, etc.
- **Hybrid architecture** — MCP for convenience, native Wasm for security-critical operations

#### Comparison with native skills

| Consideration                           | Recommendation               |
|-----------------------------------------|------------------------------|
| Security-critical (financial, PII)      | Native Wasm                  |
| Rapid integration with existing service | MCP bridge                   |
| High-frequency calls                    | Native Wasm (lower overhead) |
| Community/third-party maintained        | MCP bridge                   |
| Custom business logic                   | Native Wasm                  |

### Mastodon integration

The agent can connect to Mastodon (and other ActivityPub-compatible services) both as a **skill** for posting/reading and as an **inbound event channel** for receiving notifications and mentions.

#### Dual-mode architecture

```
┌─────────────────────────────────────────────────────────────┐
│                        LLM (Claude)                         │
│    sees: mastodon_post, mastodon_search, mastodon_reply     │
└──────────────────────────┬──────────────────────────────────┘
                           │
┌──────────────────────────▼──────────────────────────────────┐
│                     Skill Registry                          │
│                                                             │
│  ┌─────────────────────────────────────────────────────┐    │
│  │              MastodonSkill (builtin)                │    │
│  │   • Post status updates                             │    │
│  │   • Reply to conversations                          │    │
│  │   • Search posts and users                          │    │
│  │   • Read timelines (home, local, federated)         │    │
│  │   • Manage favorites and boosts                     │    │
│  └─────────────────────────────────────────────────────┘    │
└─────────────────────────────────────────────────────────────┘

┌─────────────────────────────────────────────────────────────┐
│                   Event Channel (inbound)                   │
│                                                             │
│  ┌─────────────────────────────────────────────────────┐    │
│  │           Mastodon Streaming API Client             │    │
│  │   • Mentions → trigger agent response               │    │
│  │   • DMs → route to conversation handler             │    │
│  │   • Followed users' posts → optional monitoring     │    │
│  │   • Hashtag streams → topic-based triggers          │    │
│  └─────────────────────────────────────────────────────┘    │
└─────────────────────────────────────────────────────────────┘
```

#### Skill: Mastodon tools

Tools exposed to the LLM for outbound actions:

| Tool                  | Description                                      |
|-----------------------|--------------------------------------------------|
| `mastodon_post`       | Post a new status (with optional media, CW, visibility) |
| `mastodon_reply`      | Reply to a specific status                       |
| `mastodon_search`     | Search for posts, users, or hashtags             |
| `mastodon_timeline`   | Read home, local, or federated timeline          |
| `mastodon_thread`     | Get full conversation thread for a status        |
| `mastodon_favorite`   | Favorite a status                                |
| `mastodon_boost`      | Boost (reblog) a status                          |
| `mastodon_dm`         | Send a direct message to a user                  |

#### Inbound event channel

The agent maintains a persistent WebSocket connection to Mastodon's streaming API:

- **Mentions** — When someone @mentions the agent's Mastodon account, the mention is routed to the LLM for response. The agent can reply directly on Mastodon.
- **Direct messages** — Mastodon DMs are treated like XMPP 1:1 chats. The agent can maintain per-user conversation context keyed by Mastodon account ID.
- **Followed accounts** — Optionally monitor posts from specific accounts and trigger actions (e.g., summarize news from followed journalists, alert on posts from a monitored service account).
- **Hashtag streams** — Subscribe to hashtag streams and react to relevant posts (e.g., monitor #fluuxagent mentions).

#### Configuration

```toml
[channels.mastodon]
enabled = true
instance = "https://mastodon.social"        # Or any ActivityPub server
access_token = "${MASTODON_ACCESS_TOKEN}"   # OAuth token

# Inbound event handling
[channels.mastodon.events]
mentions = true                             # Respond to @mentions
direct_messages = true                      # Handle DMs as conversations
hashtags = ["fluuxagent", "aiassistant"]    # Monitor these hashtags

# Optional: monitor specific accounts
follow_accounts = ["@news@mastodon.social"]

# Rate limiting and behavior
[channels.mastodon.limits]
max_posts_per_hour = 10                     # Prevent spam
reply_delay_seconds = 5                     # Avoid appearing too bot-like
```

#### Memory and context

Mastodon conversations are stored in the agent's memory system:

```
data/memory/
├── mastodon/
│   ├── @user@instance/                     # Per-user context (like JID directories)
│   │   ├── history.jsonl
│   │   └── context.md
│   └── threads/                            # Thread context for multi-post conversations
│       └── {status_id}.jsonl
```

#### Cross-channel bridging

The agent can bridge XMPP and Mastodon:

- **XMPP user requests Mastodon action** — "Post to Mastodon: Just deployed the new version!"
- **Mastodon mention triggers XMPP notification** — A mention on Mastodon can notify the admin via XMPP DM
- **Unified identity** — The agent maintains consistent persona across both channels

#### Security considerations

- **Access token scope** — Request minimal OAuth scopes (read, write:statuses, write:conversations). Avoid admin scopes.
- **Rate limiting** — Respect instance rate limits. Implement local rate limiting to prevent accidental spam.
- **Content filtering** — Apply the same prompt injection detection used for XMPP messages.
- **Instance rules** — Comply with instance ToS. Don't auto-boost or auto-favorite without explicit user instruction.

#### Use cases

- **Social media presence** — Agent maintains a Mastodon presence, responds to questions, shares updates
- **Cross-platform assistant** — Users can interact via XMPP or Mastodon based on preference
- **Monitoring and alerts** — Track mentions of a brand/project across the fediverse
- **Community engagement** — Participate in hashtag discussions, answer questions from the community

---

## v0.4 — Sandbox

Skills run in isolation.

- [ ] Wasm sandbox via wasmtime (fuel-metered, memory-limited)
- [ ] Landlock + seccomp on Linux
- [ ] App Sandbox on macOS
- [ ] Process isolation (one process per skill execution)
- [ ] Resource limits (CPU, memory, execution time)
- [ ] Agent-generated skills: Wasm code generation (LLM writes code, sandbox enforces capabilities)

See [Security Architecture](docs/SECURITY.md) for the full design.

---

## v0.5 — Agent protocol

Structured machine-readable communication.

- [ ] `urn:fluux:agent:0#skills` — skill discovery via IQ
- [ ] `urn:fluux:agent:0#execute` — skill execution via IQ
- [ ] `urn:fluux:agent:0#confirm` — destructive action confirmation
- [x] Inbound reactions — agent receives and stores emoji reactions (XEP-0444)
- [ ] Outbound reactions — agent sends reactions to user messages (XEP-0444)
- [ ] Message corrections — agent can correct its previous response (XEP-0308)

### Reactions (XEP-0444)

Reactions serve as lightweight feedback and acknowledgment:

- **Agent receives reactions** ✓ — Users can react to agent messages with emoji. The parser extracts `<reactions xmlns='urn:xmpp:reactions:0'>` stanzas, correlates them to the target message via the `id` attribute, and stores them in conversation history as `[Reacted to msg_id: {id} with {emojis}]`. The LLM sees reactions in context and can reason about user feedback.
- **Message ID tracking** ✓ — All messages (inbound and outbound) are tagged with `[msg_id: {id}]` in conversation history. Outbound messages get UUID v4 stanza IDs. This enables the LLM to correlate reactions with specific messages.
- **Agent sends reactions** (planned) — The agent can react to user messages (e.g. thumbs-up to acknowledge a command, checkmark when a task completes). This will be an action the LLM can trigger via tool use.
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

## Shared memory and RAG search

As the agent accumulates per-JID memory (`memory.md`, `sessions/*.md`) and workspace-level knowledge, simple file reads won't scale. A **Retrieval-Augmented Generation (RAG)** layer will let the agent semantically search through its memory and shared knowledge base to find relevant context before answering.

### Shared memory

Today, memory is strictly per-JID — each user's context is isolated. For teams and organizations, some knowledge should be **shared across users**:

- **Shared knowledge base** — A `data/memory/shared/` directory containing markdown files that any user's conversation can draw from. Admins curate shared context: project documentation, team decisions, onboarding material, FAQ, product specs.
- **User-contributed shared memory** — With appropriate permissions, the agent can promote facts from per-JID memory into the shared pool (e.g., a user says "our API endpoint changed to api.v2.example.com" — the agent stores this in shared memory so all users benefit).
- **Scoped sharing** — Shared memory can be scoped by room, team, or organization. A room's `memory.md` is already shared among room participants; this extends the concept to cross-conversation shared knowledge.

### RAG search over memory

Inspired by [OpenClaw's memory search architecture](https://github.com/AshishKumar4/openclaw), the agent will index its memory files and use hybrid retrieval (semantic + keyword) to find relevant context:

- **Chunking and embedding** — Memory files (`memory.md`, `sessions/*.md`, shared knowledge) are chunked and embedded using a configurable embedding provider (local GGUF model for privacy, or remote API like OpenAI/Voyage for quality).
- **Hybrid search** — Combine vector similarity (semantic paraphrase matching) with BM25 keyword search (exact tokens, code symbols, IDs). Configurable weights (e.g., 70% vector, 30% keyword).
- **Vector store** — SQLite with vector extensions (sqlite-vec) for lightweight, self-contained storage. Per-JID indexes for private memory, plus a shared index for the knowledge base.
- **Agent-facing tool** — A `memory_search` skill the LLM can call to retrieve relevant context before answering. The agent uses this as a "mandatory recall step" for questions about prior conversations, decisions, people, or project context.
- **Automatic indexing** — File watchers detect changes to memory files and re-index automatically. Embedding cache avoids redundant API calls.
- **Privacy boundaries** — RAG search respects JID isolation. A user's `memory_search` only hits their own memory + shared memory. Per-JID memory is never leaked across users.

### Configuration

```toml
[memory.search]
enabled = true
provider = "local"                    # "local" (GGUF), "openai", "voyage"
model = "nomic-embed-text-v1.5"      # Embedding model
sources = ["memory", "sessions", "shared"]  # What to index

[memory.search.hybrid]
vector_weight = 0.7
keyword_weight = 0.3

[memory.shared]
path = "./data/memory/shared"
# Who can contribute to shared memory
contributors = ["admin@localhost"]
```

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
- Google Gemini — competitive reasoning and vision capabilities
- DeepSeek — cost-effective reasoning model
- Mistral API — European hosting, GDPR-friendly
- Automatic fallback chain (try Claude, fall back to local Ollama if API is down)
- All configured per-tier in TOML — no code changes needed to switch providers

### Message archive integration (MAM)

- Use XEP-0313 (Message Archive Management) to persist and retrieve history server-side
- Could replace or complement the local markdown memory
- Enables multi-device access to conversation history

### File attachments

- **Receiving attachments** ✓ — Implemented in v0.2. Users can send images, PDFs, documents to the agent via OOB URLs (XEP-0066). The agent downloads the file, passes it to the LLM as multi-modal content (image blocks for Claude, gracefully degraded for Ollama), and responds accordingly. OOB fallback body stripping removes redundant URL-only bodies.
- **Sending attachments** (planned) — The agent can generate and send files back to the user: skill execution results as CSV, generated images, exported data. Uses XEP-0363 (HTTP File Upload) to upload to the server's HTTP upload service, then sends the URL in a message with an `<x xmlns='jabber:x:oob'>` out-of-band reference.

### Team memory via MUC (Multi-User Chat)

The agent supports joining XMPP chat rooms (MUC, XEP-0045) since v0.1. For enhanced team collaboration, it can maintain **shared team memory** alongside individual user memory:

- **Personal memory** — `{jid}/history.md` + `{jid}/context.md` — what the agent knows about each individual user. Private, per-user.
- **Team memory** — `{room_jid}/history.md` + `{room_jid}/context.md` — shared context from group conversations. The agent participates in the room, observes discussions, and builds team-level context (project names, decisions, recurring topics).
- **Memory scoping** — When responding in a MUC, the agent uses the room's shared context. When responding in a 1:1 chat, it uses the user's personal context. If the user is also in a team room, the agent could optionally blend both.
- **Use cases** — Team standup assistant, project knowledge base, shared action tracking, meeting summaries posted to the room.
- **XMPP integration** — The agent joins rooms as a participant. Room history (via MAM or MUC history) provides bootstrap context. Presence in the room is automatic.

**Open question:** Should team memory be opt-in per room (configured in TOML), or should the agent join any room it's invited to? For security, explicit configuration is safer — but an `/invite` workflow could work for trusted domains.
