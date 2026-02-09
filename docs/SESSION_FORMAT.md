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
{"type":"message","role":"user","content":"Test again in debug mode","msg_id":"d3b0-86c5","sender":"alice@example.com","ts":"2025-02-08T19:01:00Z","attachments":[{"filename":"photo.png","mime_type":"image/png","size":"926KB"}]}
```

| Field         | Type              | Description                                      |
|---------------|-------------------|--------------------------------------------------|
| `type`        | string            | Always `"message"`                               |
| `role`        | string            | `"user"` or `"assistant"`                        |
| `content`     | string            | Message text — clean, no metadata tags           |
| `msg_id`      | string (optional) | XMPP stanza ID (inbound) or UUID v4 (outbound)  |
| `sender`      | string (optional) | Sender label — JID for 1:1, `"nick@muc"` for MUC rooms. Omitted for assistant messages. |
| `ts`          | string (optional) | ISO 8601 timestamp                               |
| `attachments` | array (optional)  | List of file attachment metadata (see below)     |

Optional fields are omitted from JSON when not present (not serialized as `null`).

### Attachment metadata

When a message includes file transfers (via XMPP HTTP Upload / XEP-0363 + OOB / XEP-0066), attachment metadata is stored as a structured list — never embedded in the `content` text.

```json
{"filename": "photo.png", "mime_type": "image/png", "size": "926KB"}
```

| Field       | Type   | Description                               |
|-------------|--------|-------------------------------------------|
| `filename`  | string | Original filename from the upload URL     |
| `mime_type` | string | MIME type (e.g. `"image/png"`) or `"unknown"` for MUC messages where files are not downloaded |
| `size`      | string | Human-readable size (e.g. `"926KB"`) or `"unknown"` |

When loading history for the LLM, `parse_session()` passes this metadata as compact JSON via `build_display_content()`. The LLM can interpret structured data directly, which is more precise than natural-language descriptions.

### Reaction metadata

When a message is a reaction (XEP-0444), the reaction data is stored as structured metadata with an empty `content` field:

```json
{"type":"message","role":"user","content":"","sender":"alice@example.com","ts":"2025-02-08T19:00:03Z","reaction":{"message_id":"def-456","emojis":["👍"]}}
```

| Field        | Type     | Description                       |
|--------------|----------|-----------------------------------|
| `message_id` | string  | ID of the message being reacted to |
| `emojis`     | array   | List of emoji strings              |

When loading history for the LLM, reactions are also passed as compact JSON via `build_display_content()`.

## Design principles

### Content is clean

The `content` field contains **only** the text that is relevant to the conversation. Metadata like message IDs, sender info, and file attachment details are stored in dedicated fields. This prevents the LLM from mimicking metadata patterns in its output.

For messages with file attachments, the `content` field holds the user's text only — OOB fallback URLs are stripped from the XMPP body before storage, and attachment details go into the structured `attachments` array.

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

The `build_message_for_llm()` helper in `memory.rs` handles this construction. For messages with attachments or reactions, `build_display_content()` serializes the structured metadata as compact JSON so the LLM can interpret it directly.

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
{"type":"message","role":"user","content":"Can you read this?","msg_id":"stanza-002","sender":"alice@example.com","ts":"2025-02-08T19:00:03Z","attachments":[{"filename":"document.pdf","mime_type":"application/pdf","size":"1.2MB"}]}
{"type":"message","role":"assistant","content":"I can see the PDF. It appears to be a project proposal...","msg_id":"b2c3d4e5-f6a7-8901-bcde-f12345678901","ts":"2025-02-08T19:00:04Z"}
{"type":"message","role":"user","content":"","sender":"alice@example.com","ts":"2025-02-08T19:00:05Z","reaction":{"message_id":"a1b2c3d4-e5f6-7890-abcd-ef1234567890","emojis":["👍"]}}
{"type":"message","role":"assistant","content":"Glad you liked that! Let me know if you need anything.","msg_id":"f1e2d3c4-b5a6-7890-1234-567890abcdef","ts":"2025-02-08T19:00:06Z"}
```

## Implementation reference

- `src/agent/memory.rs` — `SessionEntry` enum, `Attachment` struct, `Reaction` struct, `parse_session()`, `build_display_content()`, `build_message_for_llm()`, `store_message_full()`, `store_message_structured()`
- `src/agent/runtime.rs` — all call sites that store messages with metadata, `build_oob_attachments()`
- `src/xmpp/stanzas.rs` — OOB body stripping (removes all OOB URLs from body text)
- `src/llm/anthropic.rs` — `Message` struct consumed by the LLM API
