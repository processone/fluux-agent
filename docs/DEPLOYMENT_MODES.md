# Deployment Modes: Bot First, Assistant Optional

Status: Proposal  
Project: Fluux Agent  
Priority: Bot mode

## 1. Why Two Modes

Fluux addresses two distinct needs:

- Shared team assistant (multi-user service).
- Personal assistant running as the user identity.

These should be treated as different products with different trust, memory, and operations models.

## 2. Guiding Principle

Bot mode is the default and primary deployment target.

- Stable, auditable, multi-user operation is the priority.
- Assistant mode is optional and can be enabled later without changing core bot behavior.

## 3. Mode A: Bot (Default)

Main characteristics:

- Dedicated bot identity (component JID or dedicated client JID).
- One shared runtime instance for multiple authorized users.
- Per-user/per-room memory isolation in a shared service.
- Centralized governance (ACLs, policies, observability, operations).

Best for:

- Team and organization deployments.
- Reliable always-on service.
- Shared tooling and controlled rollout.

## 4. Mode B: Assistant (Optional)

Main characteristics:

- User-scoped runtime, usually local (for example on a laptop).
- C2S login with the user JID on a dedicated resource.
- Private local memory by default.
- Ability to act as the user identity, with stricter local policy and explicit consent.

Best for:

- Personal automation.
- User-owned workflows and local privacy.
- Experiments that should not impact shared production bot behavior.

## 5. Memory Strategy Across Modes

Default decision:

- No full memory replication between bot and assistant modes.

Rationale:

- Different trust boundaries (server vs personal machine).
- Different availability and lifecycle constraints.
- Full sync of raw history increases complexity and leakage risk.

Recommended model:

- Local-first private memory for assistant mode.
- Shared memory on server for bot mode.
- Optional explicit knowledge bridge, not full sync:
  - Promote selected facts/summaries from personal memory to shared memory.
  - Pull shared knowledge as read-only context into assistant mode.
  - Use append-only records with IDs for deduplication.

## 6. Security and Governance

Minimum guardrails:

- Separate policy profiles per mode.
- Explicit confirmation for destructive actions.
- Clear audit attribution:
  - `mode=bot|assistant`
  - `actor_jid=...`
  - `source=live|carbon|mam` (when applicable)
- No implicit cross-mode memory access.

## 7. Rollout Plan

1. Stabilize and harden bot mode (default path).
2. Add assistant mode as single-user local runtime.
3. Add optional selective knowledge bridge.
4. Evaluate server-side delegation extensions only after mode boundaries are stable.

## 8. Non-Goals (Initial Scope)

- Full bidirectional sync of raw conversation history.
- Automatic silent sharing of personal memory into shared team memory.
- Coupling assistant-mode reliability requirements to bot-mode SLA.

## 9. Personal Assistant XMPP API (Future Direction)

Assistant mode can rely on a dedicated XMPP API profile for personal assistants.
This is optional and must not change the bot-first deployment priority.

Approach:

- Reuse existing XEPs where possible (for example Carbons, MAM, privilege/delegation).
- Add a Fluux-specific namespace only for gaps that are not covered by existing standards.

Proposed initial namespace:

- `urn:fluux:assistant:0`

Candidate API capabilities:

- Capability discovery for assistant features and limits.
- Explicit user authorization grant/revoke with scopes and expiration.
- On-behalf execution envelope with idempotency key and correlation ID.
- Proactive/offline job lifecycle (create, list, cancel).
- Structured audit events (`actor`, `on_behalf_of`, `reason`, `timestamp`).

Standardization path:

1. Implement and validate as a private Fluux namespace.
2. Collect interoperability and security feedback.
3. Propose a formal XMPP extension once flows are stable.
