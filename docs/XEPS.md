# Supported XMPP Extension Protocols (XEPs) and RFCs

Fluux Agent implements the following XMPP standards:

## Core XMPP RFCs

### RFC 6120: XMPP Core ✓

Core XMPP protocol — XML streams, stanza routing, error handling.

**Implementation:** Stream establishment, stanza parsing (quick-xml event-based `StanzaParser`), error condition handling (§4.9.3).

**References:**
- `src/xmpp/stanzas.rs:724` — stream error conditions (25 RFC 6120 conditions)
- `src/xmpp/stanzas.rs:446` — `StanzaParser` (event-based XML stream parser)
- `src/xmpp/component.rs` — stream management

---

### RFC 6121: XMPP Instant Messaging and Presence ✓

Instant messaging, presence subscriptions, and roster management.

**Implementation:** Message and presence stanza handling, roster operations.

**References:**
- `src/xmpp/stanzas.rs:327` — roster management
- `src/xmpp/stanzas.rs:273` — presence types and subscription handling
- `src/agent/runtime.rs` — message and presence processing

---

### RFC 4616: SASL PLAIN ✓

Simple authentication mechanism for C2S client mode.

**Implementation:** Username/password authentication over TLS.

**References:**
- `src/xmpp/sasl.rs:46-63` — PLAIN mechanism implementation

---

### RFC 5802: SASL SCRAM-SHA-1 ✓

Challenge-response authentication with stronger security guarantees.

**Implementation:** Full SCRAM-SHA-1 flow with test vectors validation.

**References:**
- `src/xmpp/sasl.rs:65-202` — SCRAM-SHA-1 implementation
- `src/xmpp/sasl.rs:204` — RFC 5802 test vector validation

---

## Core Protocol Extensions

### XEP-0114: Jabber Component Protocol ✓

Component mode connection — the agent registers as a subdomain (e.g., `agent.localhost`) with its own address namespace. Used for production deployments where the agent runs as a first-class service on the XMPP infrastructure.

**Implementation:** SHA-1 handshake, stream management, full stanza routing.

**References:**
- `src/xmpp/component.rs` — connection handling and handshake
- `src/xmpp/stanzas.rs` — protocol stanza builders
- `docs/DEVELOPING.md` — architecture notes

---

## Messaging Extensions

### XEP-0085: Chat State Notifications ✓

Chat state indicators for typing awareness.

**Inbound filtering:** The agent ignores chat state notifications (composing, paused, active, inactive, gone) when they arrive without a message body, preventing unnecessary LLM invocations.

**Outbound signaling:** The agent sends proper chat state notifications throughout its response lifecycle:
- **`<composing/>`** — Sent as a standalone message when the agent starts generating a response
- **`<paused/>`** — Sent when streaming pauses (e.g., waiting for tool results)
- **`<active/>`** — Bundled inside the response `<message>` stanza to clear the typing indicator when the reply arrives

**Implementation:** Both component (XEP-0114) and C2S client modes.

**References:**
- `src/xmpp/stanzas.rs:69-98` — outbound chat state builders
- `src/xmpp/stanzas.rs:634` — inbound filtering logic (in `finalize_message`)
- `src/agent/runtime.rs` — lifecycle integration

---

### XEP-0066: Out of Band Data ✓

File attachment support via out-of-band URLs.

**Inbound parsing:** The agent extracts `<x xmlns='jabber:x:oob'>` elements from incoming messages, supporting multiple attachments per message with optional `<desc>` metadata. When the message body is just a fallback copy of the OOB URL (common in clients like Conversations), the body is automatically stripped to avoid sending the raw URL to the LLM.

**Attachment handling:** OOB URLs are downloaded and their content is included in the LLM prompt alongside the message text, enabling the agent to reason about shared files and images.

**References:**
- `src/xmpp/stanzas.rs:16` — `OobData` struct
- `src/xmpp/stanzas.rs:664-689` — OOB parsing and fallback body stripping
- `src/agent/runtime.rs` — attachment download and LLM prompt integration
- `src/agent/files.rs` — file download handling

---

### XEP-0444: Message Reactions (inbound) ✓

Receive and process emoji reactions to messages.

**Inbound parsing:** The agent detects `<reactions xmlns='urn:xmpp:reactions:0'>` elements inside `<message>` stanzas. The `id` attribute on `<reactions>` identifies the target message, and each `<reaction>` child provides an emoji. Both 1:1 chat and MUC groupchat reactions are supported.

**Message ID tracking:** Message IDs are stored as structured JSONL metadata but not passed to the LLM (runtime-only metadata). Outbound agent messages use UUID v4 stanza IDs. When a reaction arrives, it is stored as `[Reacted to msg_id: {id} with {emojis}]` in the content field, so the LLM sees the reaction text and can correlate it with context.

**References:**
- `src/xmpp/stanzas.rs:36` — `IncomingReaction` struct
- `src/xmpp/stanzas.rs:674` — reaction parsing in `finalize_message`
- `src/agent/runtime.rs:386` — reaction storage in conversation history

---

## Multi-User Chat

### XEP-0045: Multi-User Chat (MUC) ✓

Group chat room support with mention-based interaction.

**Features:**
- Join configured MUC rooms on connect
- Store **all room messages** to history for full conversational context
- Respond only when explicitly mentioned (`@bot`, `/bot`, etc.)
- Per-room memory isolation (history, user profiles, workspace overrides)
- MUC-aware sender tracking (messages stored with sender nick: `### user (alice@muc)`)
- Proper groupchat message handling (type `groupchat`, MUC reflection filtering)

**Configuration:** `config/agent.toml` — `[[rooms]]` section with `jid`, `nick`, and optional `mention_pattern`.

**Implementation:**
- `src/agent/runtime.rs` — MUC joining, mention detection, response logic
- `src/agent/memory.rs` — MUC-specific message storage with sender labels
- `src/xmpp/stanzas.rs:282-318` — MUC join/message stanza builders

---

## Planned Extensions

### XEP-0163: Personal Eventing Protocol (PEP)

Subscribe to user status events (mood, activity, tune, location, avatar changes) for contextual awareness.

**Status:** Roadmap (future)

---

### XEP-0198: Stream Management

Message acknowledgment, session resumption, and reliability for unstable networks.

**Status:** Roadmap (future)

---

### XEP-0201: Best Practices for Message Threads

Thread ID mapping — different `<thread>` IDs map to different agent sessions.

**Status:** Roadmap (future)

---

### XEP-0308: Last Message Correction

Allow the agent to correct its previous responses.

**Status:** Roadmap (future)

---

### XEP-0313: Message Archive Management (MAM)

Server-side message history persistence and retrieval.

**Status:** Roadmap (future)

---

### XEP-0363: HTTP File Upload

Send and receive file attachments (images, PDFs, documents, generated files).

**Status:** Roadmap (future)

---

### XEP-0444: Message Reactions (outbound)

Send emoji reactions to messages.

**Status:** Roadmap (future)

---

## Related XEPs (mentioned but not directly implemented)

### XEP-0225: Component Connections

**Not used** — Fluux Agent uses XEP-0114 instead. XEP-0225 is marked as Deferred and not widely supported by XMPP servers.

See `docs/DEVELOPING.md` for rationale.

---

## Version History

- **v0.1** — XEP-0114 (component mode), XEP-0085 (chat states), XEP-0045 (MUC), XEP-0066 (OOB file attachments)
- **v0.2** — XEP-0444 inbound reactions, message ID embedding, C2S client mode (RFC 4616 PLAIN, RFC 5802 SCRAM-SHA-1, STARTTLS), JSONL session format

---

For implementation details and contribution guidelines, see:
- `docs/DEVELOPING.md` — architecture and protocol notes
- `ROADMAP.md` — planned features and extensions
- `src/xmpp/` — protocol implementation
