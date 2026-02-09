# Skill Example: XMPP Messaging

This document explores the design of an XMPP messaging skill that allows the agent to send messages to other XMPP contacts on behalf of the user.

## The Question

Should the agent be able to send XMPP messages to other contacts? If so, how should this capability be implemented?

| Option | Description | Trade-offs |
|--------|-------------|------------|
| **Core Feature** | Built into the agent's base functionality | Always available, but can't be disabled; increases attack surface |
| **Skill** | Modular capability loaded on demand | Can be enabled/disabled, fits capability system |
| **Tool** | LLM-invokable action during reasoning | Enables proactive behavior, but highest autonomy risk |

## Recommendation: Skill with Tool Exposure

The recommended approach is a **skill that exposes a tool to the LLM**, with strong guardrails:

```
┌─────────────────────────────────────────────────────────────┐
│                        User                                  │
│  "Can you let Alice know I'll be late to the meeting?"       │
└──────────────────────────┬──────────────────────────────────┘
                           │
                           ▼
┌─────────────────────────────────────────────────────────────┐
│                      LLM (Claude)                            │
│  Decides to use xmpp_send tool                               │
│  params: { "to": "alice@example.com", "body": "..." }        │
└──────────────────────────┬──────────────────────────────────┘
                           │
                           ▼
┌─────────────────────────────────────────────────────────────┐
│                    Guardrail Layer                           │
│  ✓ Contact in allowlist?                                     │
│  ✓ Rate limit not exceeded?                                  │
│  ✓ First message to contact? → Requires confirmation         │
│  ✓ Content policy check?                                     │
└──────────────────────────┬──────────────────────────────────┘
                           │
                           ▼
┌─────────────────────────────────────────────────────────────┐
│                    XMPP Send Skill                           │
│  Sends message via established XMPP connection               │
│  Logs action to audit trail                                  │
└──────────────────────────┬──────────────────────────────────┘
                           │
                           ▼
                    Message delivered
```

---

## Security Considerations

This is a **high-risk capability**. An agent that can message arbitrary contacts could:

| Risk | Description | Mitigation |
|------|-------------|------------|
| **Information leakage** | LLM shares conversation context with third parties | Content filtering, context isolation |
| **Spam/harassment** | Agent sends unwanted messages | Rate limiting, allowlist |
| **Social engineering** | Agent used to manipulate contacts | Audit logging, user review |
| **Impersonation** | Messages appear to come from user | Clear agent attribution option |

### Why Not Core Feature?

A core feature would mean the messaging capability is always present, even when not needed. This:
- Increases attack surface permanently
- Makes it harder to audit which deployments have messaging enabled
- Doesn't fit the principle of least privilege

### Why Not Tool-Only?

A pure tool approach (without the skill layer) would bypass the capability system, making it harder to:
- Declare required permissions in a manifest
- Enforce consistent guardrails
- Disable the feature without code changes

---

## Required Guardrails

### 1. Contact Allowlist

The skill should only send messages to pre-approved contacts:

```toml
[skills.xmpp_send]
# Explicit allowlist of permitted recipients
allowed_contacts = [
    "alice@example.com",
    "bob@example.com",
    "team@conference.example.com",
]

# Or use roster-based allowlist
allow_roster_contacts = true    # Allow any contact in user's roster
allow_subscribed_only = true    # Only contacts with mutual subscription
```

### 2. Rate Limiting

Prevent message flooding:

```toml
[skills.xmpp_send.rate_limit]
max_messages_per_minute = 5
max_messages_per_hour = 30
max_messages_per_contact_per_hour = 10
cooldown_after_limit = 300      # Seconds to wait after hitting limit
```

### 3. First-Contact Confirmation

Require user approval before messaging a contact for the first time:

```toml
[skills.xmpp_send]
require_first_contact_confirmation = true
```

When enabled, the agent prompts:

```
Agent: I'd like to send a message to alice@example.com for the first time.
       Message: "Hi Alice, [user] will be 10 minutes late to the meeting."

       Do you want me to send this? (yes/no/always for this contact)
```

### 4. Audit Logging

All sent messages are logged for review:

```toml
[skills.xmpp_send.audit]
enabled = true
log_path = "~/.fluux-agent/audit/xmpp_send.jsonl"
retention_days = 90
```

Log entry format:

```json
{
    "timestamp": "2025-01-15T10:30:00Z",
    "skill": "xmpp_send",
    "action": "send_message",
    "to": "alice@example.com",
    "body_hash": "sha256:abc123...",
    "body_preview": "Hi Alice, [user] will be...",
    "triggered_by": "user_request",
    "conversation_id": "conv_12345",
    "result": "delivered"
}
```

### 5. Draft Mode (Optional)

For cautious deployments, the agent proposes messages but doesn't send them:

```toml
[skills.xmpp_send]
mode = "draft"    # "send" or "draft"
```

In draft mode:

```
Agent: I've drafted a message for alice@example.com:

       "Hi Alice, I'll be 10 minutes late to the meeting."

       Would you like me to send it?
```

---

## Skill Manifest

```toml
[skill]
id = "xmpp_send"
name = "XMPP Send Message"
version = "0.1.0"
description = "Send XMPP messages to allowed contacts on behalf of the user"

[skill.capabilities]
# Uses existing XMPP connection (no additional network capability needed)
network = []
filesystem_read = []
filesystem_write = ["~/.fluux-agent/audit/"]
credential = ["xmpp-session"]    # Access to active XMPP session
shell = false
browser = false

[skill.execution]
tier = "standard"
max_execution_time = 10
destructive = true               # Requires user confirmation by default

[skill.guardrails]
# These are skill-specific safety settings
contact_allowlist = true         # Enforce allowlist
rate_limiting = true             # Enforce rate limits
audit_logging = true             # Log all actions
first_contact_confirmation = true
```

---

## Implementation

### Skill Interface

```rust
pub struct XmppSendSkill {
    config: XmppSendConfig,
    rate_limiter: RateLimiter,
    audit_logger: AuditLogger,
    first_contact_tracker: FirstContactTracker,
}

#[async_trait]
impl Skill for XmppSendSkill {
    fn name(&self) -> &str {
        "xmpp_send"
    }

    fn description(&self) -> &str {
        "Send an XMPP message to an allowed contact. Use this when the user \
         explicitly asks you to message someone or when notifying a contact \
         is clearly appropriate (e.g., 'tell Alice I'll be late')."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "to": {
                    "type": "string",
                    "description": "The JID (XMPP address) of the recipient"
                },
                "body": {
                    "type": "string",
                    "description": "The message content to send"
                },
                "type": {
                    "type": "string",
                    "enum": ["chat", "groupchat"],
                    "default": "chat",
                    "description": "Message type: 'chat' for 1:1, 'groupchat' for MUC"
                }
            },
            "required": ["to", "body"]
        })
    }

    fn capabilities(&self) -> Vec<String> {
        vec![
            "credential:xmpp-session".to_string(),
            "filesystem:~/.fluux-agent/audit/:write".to_string(),
        ]
    }

    async fn execute(&self, params: Value) -> Result<String> {
        let to: Jid = params["to"].as_str()
            .ok_or_else(|| anyhow!("Missing 'to' parameter"))?
            .parse()?;

        let body = params["body"].as_str()
            .ok_or_else(|| anyhow!("Missing 'body' parameter"))?;

        let msg_type = params["type"].as_str().unwrap_or("chat");

        // Guardrail checks
        self.check_allowlist(&to)?;
        self.rate_limiter.check(&to)?;

        // First contact confirmation (handled by runtime if needed)
        if self.first_contact_tracker.is_first_contact(&to) {
            return Ok(json!({
                "status": "confirmation_required",
                "to": to.to_string(),
                "body": body,
                "message": "This is the first message to this contact. User confirmation required."
            }).to_string());
        }

        // Send the message
        let result = self.send_message(&to, body, msg_type).await?;

        // Audit log
        self.audit_logger.log(AuditEntry {
            action: "send_message",
            to: to.to_string(),
            body_preview: truncate(body, 50),
            result: "delivered",
            ..Default::default()
        }).await?;

        Ok(json!({
            "status": "sent",
            "to": to.to_string(),
            "message_id": result.id
        }).to_string())
    }
}
```

### Configuration

```toml
# config/agent.toml

[skills]
enabled = ["web_search", "url_fetch", "xmpp_send"]

[skills.xmpp_send]
# Allowlist
allowed_contacts = [
    "alice@example.com",
    "bob@example.com",
]
allow_roster_contacts = true
allow_subscribed_only = true

# Rate limiting
max_messages_per_minute = 5
max_messages_per_hour = 30

# Safety
mode = "send"                     # "send" or "draft"
require_first_contact_confirmation = true

# Audit
audit_enabled = true
audit_path = "~/.fluux-agent/audit/xmpp_send.jsonl"
```

---

## Usage Examples

### Basic Usage

**User:** "Tell Alice I'll be 10 minutes late"

**Agent:** *(uses xmpp_send tool)*
```json
{
    "to": "alice@example.com",
    "body": "Hi Alice, I'll be about 10 minutes late to our meeting."
}
```

**Agent:** "I've sent Alice a message letting her know you'll be 10 minutes late."

### First Contact Scenario

**User:** "Message charlie@example.com about the project update"

**Agent:** "I haven't messaged charlie@example.com before. Here's the message I'd send:

> Hi Charlie, I wanted to share a project update...

Should I send this message?"

**User:** "Yes, go ahead"

**Agent:** *(sends message)* "Done! I've sent the update to Charlie."

### Draft Mode

**User:** "Let the team know the meeting is cancelled"

**Agent:** "I've drafted a message to team@conference.example.com:

> The meeting scheduled for today has been cancelled. We'll reschedule for next week.

Would you like me to send this, or would you like to modify it?"

---

## Alternatives Considered

### 1. Separate "Notify" vs "Converse" Skills

Split into two skills:
- `xmpp_notify`: One-way notifications (lower risk)
- `xmpp_converse`: Back-and-forth conversations (higher risk)

**Rejected because:** Adds complexity without significant security benefit. The guardrails apply equally to both use cases.

### 2. Always Require Confirmation

Every message requires explicit user approval.

**Rejected because:** Too disruptive for legitimate use cases. First-contact confirmation plus allowlist provides sufficient protection while maintaining usability.

### 3. Read-Only Alternative

Only allow reading messages, not sending.

**Considered for:** Deployments where messaging risk is unacceptable. This could be a separate `xmpp_read` skill.

---

## Related Documents

- [SKILLS.md](SKILLS.md) — Skills architecture overview
- [SECURITY.md](SECURITY.md) — Defense-in-depth architecture
- [ROADMAP.md](../ROADMAP.md) — Feature timeline
