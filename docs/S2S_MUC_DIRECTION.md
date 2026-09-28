# Standalone S2S/MUC service direction

Date: 2026-09-28

Status: Architectural direction; not implemented

The preferred long-term direction is a standalone Rust XMPP service that owns
its domain, implements S2S and MUC, and attaches an agent to each room. This is
an architectural direction, not an implemented server mode.

Users keep their accounts and clients on their existing XMPP servers and join
rooms on the agent service through federation. Operators can host only the
agent service, without hosting human accounts or exposing a C2S endpoint.
Interoperability should require no Fluux-specific installation on remote
servers; those servers must be able to federate with the service domain.

During the transition, retain three connection/deployment options:

- **C2S client:** the agent connects through a regular XMPP account.
- **External component (XEP-0114):** the agent connects to an existing server.
- **Standalone S2S/MUC service:** Fluux Agent hosts the rooms and their agents.

S2S/MUC is the primary architectural target. Keeping only this mode eventually
is likely, but removing C2S or component support is not decided or authorized
by this note. Preserve the existing modes while the server mode matures.

The server mode is more than a third transport adapter: it owns room state,
permissions, participant lifecycle, and the relationship between a room and
its agent. The agent is intrinsic to the room, rather than an external bot
that must connect and rejoin it. It can be represented as an identifiable MUC
occupant for client compatibility.

Keep the reasoning, tools, and memory runtime independent of XMPP connection
management. Separate S2S federation, MUC room management, and agent execution,
so slow or failed agent work does not block room protocol processing. Reuse
the existing Fluux Agent runtime where appropriate; the S2S server and MUC
hosting layers remain new work. Library choices and the initial protocol
coverage still need investigation.
