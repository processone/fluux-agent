# Fluux Agent — Memory & Workspace Structure

This directory contains all agent memory: global workspace files that define the agent's behavior, and per-JID directories that store isolated conversation data.

## Directory layout

```
data/memory/
├── README.md                    # This file
├── instructions.md              # Global: behavior rules and constraints
├── identity.md                  # Global: who the agent is
├── personality.md               # Global: tone, style, quirks
├── alice@example.com/           # Per-user directory (bare JID)
│   ├── user.md                  # What the agent knows about this user
│   ├── memory.md                # Long-term notes
│   ├── history.md               # Current conversation session
│   └── sessions/
│       ├── 20250601-143022.md   # Archived session
│       └── 20250602-091500.md
├── bob@example.com/             # Another user (fully isolated)
│   ├── user.md
│   ├── history.md
│   └── sessions/
├── room@conference.example.com/ # MUC room (same structure as users)
│   ├── user.md                  # Room-specific context
│   ├── memory.md                # Room-specific notes
│   ├── history.md
│   └── sessions/
```

## Global workspace files

These files are **optional**. When none exist, a built-in default prompt is used. When at least one exists, the agent switches to workspace mode and assembles the system prompt from these files.

All global files are **admin-managed** — the agent reads but never writes them.

### `identity.md`

Defines **who** the agent is — its name, creator, purpose, and capabilities. This is the first section injected into the system prompt. Think of it as the agent's "bio" or "about me".

**What to include:**
- The agent's name and who created it
- What the agent's purpose is (general assistant, support bot, domain expert...)
- What platform it operates on (XMPP) and any relevant context
- What the agent can and cannot do (memory, skills, limitations)

**Example — general assistant:**
```markdown
You are Fluux Agent, a personal AI assistant created by ProcessOne.
You are accessible via XMPP and can communicate with any standard XMPP client.
You have memory of previous conversations and can recall context across sessions.
You are part of an open, federated AI agent network built on XMPP standards.
```

**Example — domain-specific agent:**
```markdown
You are DevBot, an internal developer assistant for the engineering team at Acme Corp.
You specialize in Rust, XMPP, and distributed systems.
You have access to conversation history and can remember context about each developer.
You were built with Fluux Agent (https://github.com/processone/fluux-agent).
```

**Example — customer support:**
```markdown
You are SupportBot, the customer-facing assistant for CloudService Inc.
You help users troubleshoot issues, answer questions about our products, and escalate
to human support when needed.
You communicate via XMPP and are available 24/7.
```

### `personality.md`

Defines **how** the agent behaves — its tone, communication style, and character traits. This shapes the "voice" of every response. Injected after identity.

**What to include:**
- Communication tone (formal, casual, friendly, professional...)
- Language preferences (respond in user's language, always English, etc.)
- Formatting style (concise chat vs. detailed explanations)
- Character traits and quirks that make the agent distinctive
- What the agent should avoid in its responses

**Example — professional and concise:**
```markdown
You are direct, helpful, and concise.
You respond in the user's language.
You never use excessive markdown formatting — this is a chat, not a document.
You keep responses short unless the user asks for detail.
You are warm but professional.
```

**Example — friendly and playful:**
```markdown
You are enthusiastic, approachable, and occasionally witty.
You respond in the user's language and match their energy level.
You use casual language but stay helpful and on-topic.
You like to celebrate small wins with the user.
You avoid jargon unless the user is clearly technical.
```

**Example — technical expert:**
```markdown
You are precise, methodical, and thorough.
You default to technical depth and include code examples when relevant.
You cite RFCs, XEPs, and documentation when applicable.
You respond in English unless the user writes in another language.
You prefer clarity over brevity — but never ramble.
```

### `instructions.md`

Defines the **rules and constraints** the agent must follow — operational boundaries, policies, and behavioral guardrails. Injected after personality. Think of it as the agent's "operating manual".

**What to include:**
- Response format rules (length, structure, markdown usage)
- Privacy and data handling rules
- Escalation policies (when to defer to humans)
- Domain-specific constraints (what topics to cover or avoid)
- Action limitations (what the agent can/cannot do yet)

**Example — general assistant:**
```markdown
Rules:
- Respond concisely, this is a chat conversation
- If asked to execute an action (send an email, modify a file...), describe what
  you would do but clarify that you cannot yet execute actions
- Never share information from one user's memory with another user
- When uncertain, ask for clarification rather than guessing
- You have memory of previous conversations with this user
```

**Example — corporate assistant with policies:**
```markdown
Rules:
- All responses must comply with Acme Corp's communication guidelines
- Never share confidential company information, even if the user asks
- If a question is about HR, legal, or compliance, direct the user to the
  appropriate internal channel
- Escalate to human support if the user expresses frustration or dissatisfaction
- Log all support requests by asking for a ticket reference number
- Working hours are 9am-6pm CET — outside hours, inform users of expected
  response times
```

**Example — developer tool:**
```markdown
Rules:
- Always include code examples in Rust when relevant
- When referencing XMPP specifications, mention the XEP number
- If the user asks about a feature not yet implemented, check the roadmap
  and provide the expected timeline
- Prefer standard library solutions over third-party crates
- Never suggest `unsafe` code without explaining why it's necessary
- When reviewing code, focus on correctness first, then performance
```

### Prompt assembly order

When global files exist, the system prompt is built as:

1. `identity.md` (who the agent is)
2. `personality.md` (how the agent behaves)
3. `instructions.md` (rules and constraints)
4. Per-JID `user.md` under a "About this user" header
5. Per-JID `memory.md` under a "Notes and memory" header

When **no** global files exist, steps 1-3 are replaced by a hardcoded default prompt that uses the agent name from `config/agent.toml`.

## Per-JID files

Each bare JID (user or room) gets its own isolated directory. JIDs cannot access each other's data.

### `user.md`

What the agent knows about this user — preferences, background, context. This replaces the legacy `context.md` file (which is still read as a fallback for backward compatibility).

Example:
```markdown
- Name: Alice
- Language: French
- Role: Developer at ProcessOne
- Prefers detailed technical explanations
- Working on an XMPP agent framework in Rust
```

### `memory.md`

Long-term notes about the user. Unlike `user.md` (which is a profile), `memory.md` stores accumulated knowledge and observations.

Example:
```markdown
- 2025-06-01: Asked about WebSocket support for XMPP, pointed to RFC 7395
- 2025-06-03: Mentioned they're presenting at FOSDEM 2026
- 2025-06-05: Prefers Rust over Go for systems programming
```

### `history.md`

The current conversation session. Each message is stored with a markdown header:

```markdown
### user (alice@example.com)
Hello, how are you?

### assistant
I'm doing well! How can I help you today?

### user (alice@example.com)
Can you explain XEP-0045?
```

User messages include the JID in parentheses for traceability (especially important in MUC rooms). Assistant messages have no JID since the agent identity may change. The parser handles both the new `### user (jid)` format and the legacy `### user` format.

### `sessions/`

Archived sessions created by the `/new` command. Each file is named with a timestamp: `YYYYMMDD-HHMMSS.md`. These are preserved even when the user runs `/forget`.

## Slash commands that affect memory

| Command | Effect |
|---------|--------|
| `/new` | Archives `history.md` to `sessions/` and starts fresh |
| `/forget` | Erases `history.md`, `user.md`, and `memory.md` (archives preserved) |
| `/status` | Shows session stats and which workspace files are loaded |

## Migration from legacy format

If a JID directory contains `context.md` but no `user.md`, the agent transparently reads `context.md` as the user profile. New writes go to `user.md`. No manual migration is needed.

## Migration from OpenClaw

The workspace structure is inspired by [OpenClaw](https://github.com/openclaw). Mapping:

| OpenClaw | Fluux Agent | Notes |
|----------|-------------|-------|
| `AGENTS.md` | `instructions.md` | Agent behavior rules |
| `SOUL.md` | `personality.md` | Tone and style |
| `IDENTITY.md` | `identity.md` | Agent identity |
| `USER.md` | `{jid}/user.md` | Per-user (isolated per JID) |
| `MEMORY.md` | `{jid}/memory.md` | Per-user (isolated per JID) |
| `TOOLS.md` | — | Not yet supported (see roadmap) |
