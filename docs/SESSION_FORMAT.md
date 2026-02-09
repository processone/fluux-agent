# Session Format (JSONL)

Fluux Agent stores conversation history as **JSONL** (JSON Lines) files — one JSON object per line. This format cleanly separates message content from metadata, making it easy to parse, extend, and debug.

## File layout

```
{memory_path}/
  {jid}/
    history.jsonl          — current active session
    sessions/
      20250208-190000.jsonl — archived session
      20250209-120000.jsonl — archived session
```

Each JID (bare JID for 1:1 chats, room JID for MUC rooms) gets its own directory with a single `history.jsonl` for the active session. When a user runs `/new`, the current session is archived into `sessions/` with a timestamp.

## Entry types

### Session header

The first line of every session file is a header:

```json
{"type":"session","version":1,"created":"2025-02-08T19:00:00Z","jid":"alice@example.com"}
```

| Field     | Type   | Description                          |
|-----------|--------|--------------------------------------|
| `type`    | string | Always `"session"`                   |
| `version` | u32    | Format version (currently `1`)       |
| `created` | string | ISO 8601 timestamp of session start  |
| `jid`     | string | Bare JID or room JID for this session|

### Message entry

Each message (user or assistant) is one line:

```json
{"type":"message","role":"user","content":"Hello!","msg_id":"abc-123","sender":"alice@example.com","ts":"2025-02-08T19:00:01Z"}
{"type":"message","role":"assistant","content":"Hi there!","msg_id":"def-456","ts":"2025-02-08T19:00:02Z"}
```

| Field    | Type            | Description                                      |
|----------|-----------------|--------------------------------------------------|
| `type`   | string          | Always `"message"`                               |
| `role`   | string          | `"user"` or `"assistant"`                        |
| `content`| string          | Message text — clean, no metadata tags           |
| `msg_id` | string (optional) | XMPP stanza ID (inbound) or UUID v4 (outbound)|
| `sender` | string (optional) | Sender label — JID for 1:1, `"nick@muc"` for MUC rooms. Omitted for assistant messages. |
| `ts`     | string (optional) | ISO 8601 timestamp                             |

Optional fields are omitted from JSON when not present (not serialized as `null`).

## Design principles

### Content is clean

The `content` field contains **only** the text that is relevant to the conversation. Metadata like message IDs and sender info is stored in dedicated fields. This prevents the LLM from mimicking metadata patterns in its output.

### Model sees only content

The LLM never sees runtime metadata (msg_id, timestamps). Following OpenClaw's approach, the model just produces content — the runtime manages all metadata (ID generation, timestamps, sender tracking).

When loading history for the LLM, `parse_session()` converts JSONL entries to plain text messages:

- **1:1 chats:** Messages are passed as plain text (sender is redundant — only one user)
- **MUC rooms:** The sender nick is prepended as a natural text prefix for participant attribution

```json
{"role": "user", "content": "alice@muc: Anyone around?"}
{"role": "user", "content": "bob@muc: I'm here!"}
{"role": "assistant", "content": "Hi everyone!"}
```

The `build_message_for_llm()` helper in `memory.rs` handles this construction.

### Reactions

Reactions are stored as regular user messages with descriptive content:

```json
{"type":"message","role":"user","content":"[Reacted to msg_id: def-456 with 👍]","sender":"alice@example.com","ts":"2025-02-08T19:00:03Z"}
```

The reaction text is in `content` because the LLM needs to see it. The `msg_id` field is omitted on reactions (the target message ID is in the text).

### MUC rooms

In group chats, all participants' messages are stored with `"role":"user"`. The `sender` field distinguishes who said what:

```json
{"type":"message","role":"user","content":"Anyone around?","sender":"alice@muc"}
{"type":"message","role":"user","content":"I'm here!","sender":"bob@muc"}
{"type":"message","role":"assistant","content":"Hi everyone!","msg_id":"out-001"}
```

## Example session

```json
{"type":"session","version":1,"created":"2025-02-08T19:00:00Z","jid":"alice@example.com"}
{"type":"message","role":"user","content":"Hello, how are you?","msg_id":"stanza-001","sender":"alice@example.com","ts":"2025-02-08T19:00:01Z"}
{"type":"message","role":"assistant","content":"I'm doing well, thanks for asking! How can I help you today?","msg_id":"a1b2c3d4-e5f6-7890-abcd-ef1234567890","ts":"2025-02-08T19:00:02Z"}
{"type":"message","role":"user","content":"[Reacted to msg_id: a1b2c3d4-e5f6-7890-abcd-ef1234567890 with 👍]","sender":"alice@example.com","ts":"2025-02-08T19:00:05Z"}
{"type":"message","role":"assistant","content":"Glad you liked that! Let me know if you need anything.","msg_id":"f1e2d3c4-b5a6-7890-1234-567890abcdef","ts":"2025-02-08T19:00:06Z"}
```

## Implementation reference

- `src/agent/memory.rs` — `SessionEntry` enum, `parse_session()`, `build_message_for_llm()`, `store_message_structured()`
- `src/agent/runtime.rs` — all call sites that store messages with metadata
- `src/llm/anthropic.rs` — `Message` struct consumed by the LLM API
