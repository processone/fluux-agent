# Skills Architecture

Skills extend Fluux Agent's capabilities beyond conversation. They are actions the LLM can invoke to interact with external systems, fetch data, or perform computations.

## Overview

```
┌─────────────────────────────────────────────────────────────┐
│                      LLM (Claude)                           │
│  Receives: system prompt + messages + tools[]               │
│  Returns:  text | tool_use(name, params)                    │
└──────────────────────────┬──────────────────────────────────┘
                           │
┌──────────────────────────▼──────────────────────────────────┐
│                    Agent Runtime                            │
│  1. Builds tool definitions from SkillRegistry              │
│  2. Sends to LLM as `tools` parameter                       │
│  3. If LLM returns tool_use → validate → execute            │
│  4. Feeds tool_result back → LLM continues                  │
│  5. Loop until LLM returns text (final answer)              │
└──────────────────────────┬──────────────────────────────────┘
                           │
┌──────────────────────────▼──────────────────────────────────┐
│                    Skill Registry                           │
├──────────────┬──────────────────┬───────────────────────────┤
│   Builtin    │   Wasm Plugins   │       MCP Bridge          │
│   (v0.2)     │   (v0.4)         │       (v0.3)              │
│              │                  │                           │
│  Compiled    │  *.wasm files    │  External MCP servers     │
│  into binary │  loaded at       │  spawned as child         │
│              │  startup         │  processes                │
└──────────────┴──────────────────┴───────────────────────────┘
```

## Skill Types

Fluux Agent supports three types of skills, each with different trade-offs:

| Type | Introduced | Distribution | Sandboxing | Best For |
|------|------------|--------------|------------|----------|
| **Builtin** | v0.2 | Compiled in | Full (Rust) | Core functionality |
| **Wasm** | v0.4 | `.wasm` file | Full (Wasm + kernel) | Custom plugins |
| **MCP** | v0.3 | External process | Process-level | Ecosystem integration |

### Builtin Skills

Compiled directly into the agent binary. These are first-party skills maintained as part of the project:

- `web_search` — Search the web via DuckDuckGo or Tavily
- `url_fetch` — Fetch and summarize a URL
- `memory_search` — RAG search over conversation history

Builtin skills have full access to Rust's ecosystem and run with the same privileges as the agent. They are the most performant option but require recompiling to modify.

### Wasm Skills

WebAssembly modules loaded from disk at startup. This is the **plugin system** for third-party and custom skills:

```
skills/
├── email-summary/
│   ├── skill.toml        # Capability manifest
│   └── skill.wasm        # Compiled WebAssembly
├── calendar-check.wasm   # Standalone skill
└── translate.wasm
```

Wasm skills run in a sandboxed runtime (wasmtime) with:
- Memory isolation (Wasm linear memory)
- CPU metering (fuel limits)
- Capability-gated host functions (no direct syscalls)
- Optional kernel sandboxing (Landlock/seccomp)

Skills can be written in any language that compiles to Wasm: Rust, Go, AssemblyScript, C/C++, etc.

### MCP Bridge

Leverages the [Model Context Protocol](https://modelcontextprotocol.io/) to connect existing MCP servers. The agent spawns MCP servers as child processes and exposes their tools alongside native skills.

```toml
[[skills.mcp.servers]]
name = "front"
command = "npx"
args = ["-y", "@anthropic/mcp-server-front"]
env = { FRONT_API_KEY = "${FRONT_API_KEY}" }
```

MCP skills are less sandboxed than Wasm (they run as full processes) but provide instant access to the MCP ecosystem: GitHub, Slack, Notion, filesystem, databases, etc.

---

## The Skill Trait

All skills implement a common interface:

```rust
#[async_trait]
pub trait Skill: Send + Sync {
    /// Unique identifier (e.g. "web_search", "url_fetch")
    fn name(&self) -> &str;

    /// Human-readable description (shown to the LLM)
    fn description(&self) -> &str;

    /// JSON Schema describing accepted parameters
    fn parameters_schema(&self) -> serde_json::Value;

    /// Required capabilities (validated against manifest)
    fn capabilities(&self) -> Vec<String>;

    /// Execute the skill with the given parameters
    async fn execute(&self, params: serde_json::Value) -> Result<String>;
}
```

The `parameters_schema()` returns a JSON Schema that the LLM uses to understand what parameters the skill accepts. The runtime validates parameters against this schema before execution.

---

## Capability System

Skills declare their required capabilities upfront. This enables:
- **Human review** — Operators see exactly what a skill needs before enabling it
- **Audit trail** — Capability grants are logged and version-controlled
- **Defense in depth** — Multiple layers validate capabilities

### Capability Types

| Capability | Syntax | Example |
|------------|--------|---------|
| Network | `network:<host>:<port>` | `network:api.github.com:443` |
| Filesystem read | `filesystem:<path>:read` | `filesystem:/home/user/docs:read` |
| Filesystem write | `filesystem:<path>:write` | `filesystem:/tmp/output:write` |
| Credential | `credential:<name>` | `credential:github-token` |
| LLM | `llm:<model>:<max_tokens>` | `llm:claude-haiku:2000` |
| Shell | `shell` | `shell` (dangerous) |

### Skill Manifest (skill.toml)

Every skill has a TOML manifest declaring its metadata and capabilities:

```toml
[skill]
id = "email-summary"
name = "Email Summary"
version = "0.1.0"
description = "Fetches and summarizes unread emails from IMAP"

[skill.capabilities]
network = ["imaps://mail.example.com:993"]
filesystem_read = []
filesystem_write = ["~/agent/output/"]
llm = { model = "claude-haiku-3-5-20241022", max_tokens = 2000 }
shell = false
browser = false

[skill.execution]
tier = "light"                    # Model tier for this skill
max_execution_time = 30           # Seconds before timeout
destructive = false               # Requires user confirmation?

[skill.schedule]
cron = "0 7 * * 1-5"              # Optional: run on schedule
notify = "user@example.com"       # Who to notify with results
```

### Capability Validation

Capabilities are validated at multiple layers:

1. **Installation** — Human reviews manifest before adding skill
2. **Startup** — Registry validates manifest syntax
3. **Runtime** — Action validator checks capability before execution
4. **Kernel** — Landlock/seccomp enforce at syscall level (v0.4)

---

## Configuration

### Enabling Skills

```toml
[skills]
# Which skills are available to the LLM
enabled = ["web_search", "url_fetch", "email-summary"]

# Skill-specific configuration
[skills.web_search]
provider = "tavily"               # "duckduckgo" or "tavily"
api_key = "${TAVILY_API_KEY}"
max_results = 5

[skills.url_fetch]
max_size_bytes = 1048576          # 1MB limit
timeout_seconds = 30
```

### MCP Servers

```toml
[skills.mcp]
enabled = true

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

### Wasm Skills Directory

```toml
[skills.wasm]
# Directory containing .wasm skills
path = "./skills"
# Fuel limit per execution (CPU metering)
fuel_limit = 10_000_000
# Memory limit in bytes
memory_limit = 67108864           # 64MB
```

---

## Agentic Loop

When a user sends a message, the runtime orchestrates skill execution:

```
User message: "What are the latest updates on issue #42?"
    │
    ▼
┌─────────────────────────────────────────┐
│ Build tools[] from SkillRegistry        │
│ [web_search, github_get_issue, ...]     │
└────────────────┬────────────────────────┘
                 │
                 ▼
┌─────────────────────────────────────────┐
│ Call LLM(system, messages, tools)       │
└────────────────┬────────────────────────┘
                 │
                 ▼
┌─────────────────────────────────────────┐
│ LLM returns:                            │
│ tool_use: github_get_issue              │
│ params: { "repo": "org/repo", "id": 42 }│
└────────────────┬────────────────────────┘
                 │
                 ▼
┌─────────────────────────────────────────┐
│ Validate:                               │
│ - Skill exists? ✓                       │
│ - Params match schema? ✓                │
│ - Capabilities authorized? ✓            │
└────────────────┬────────────────────────┘
                 │
                 ▼
┌─────────────────────────────────────────┐
│ Execute skill                           │
│ → Fetches issue from GitHub API         │
│ → Returns JSON with issue details       │
└────────────────┬────────────────────────┘
                 │
                 ▼
┌─────────────────────────────────────────┐
│ Append tool_result to messages          │
│ Call LLM again                          │
└────────────────┬────────────────────────┘
                 │
                 ▼
┌─────────────────────────────────────────┐
│ LLM returns: text                       │
│ "Issue #42 was updated yesterday..."    │
└────────────────┬────────────────────────┘
                 │
                 ▼
           Send to user
```

### Loop Limits

To prevent runaway execution:

```toml
[skills]
max_tool_rounds = 10              # Max tool calls per message
max_execution_time = 120          # Total seconds for all skills
```

---

## Security

See [SECURITY.md](SECURITY.md) for the full defense-in-depth architecture.

### Summary

| Layer | Protection | Version |
|-------|------------|---------|
| Declarative capabilities | Human-reviewed TOML manifests | v0.2 |
| Action validation | Separate process validates before execution | v0.2 |
| Wasm sandbox | Memory isolation, fuel metering | v0.4 |
| Kernel sandbox | Landlock + seccomp (Linux), App Sandbox (macOS) | v0.4 |
| Process isolation | Each skill in separate process | v0.4 |
| Destructive action confirmation | User must approve high-risk actions | v0.5 |

### Comparison: Skill Types

| Security Aspect | Builtin | Wasm | MCP |
|-----------------|---------|------|-----|
| Memory isolation | Rust safety | Wasm linear memory | Process boundary |
| Syscall filtering | N/A | seccomp | None |
| Capability enforcement | Code review | Host functions | Declared only |
| Crash impact | Agent crash | Contained | Process restart |
| Trust level | Full | Sandboxed | Process trust |

---

## Writing a Skill

### Builtin Skill (Rust)

1. Create a new file in `src/skills/builtin/`:

```rust
// src/skills/builtin/hello.rs
use crate::skills::Skill;
use async_trait::async_trait;
use serde_json::{json, Value};

pub struct HelloSkill;

#[async_trait]
impl Skill for HelloSkill {
    fn name(&self) -> &str {
        "hello"
    }

    fn description(&self) -> &str {
        "Says hello to someone"
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "name": {
                    "type": "string",
                    "description": "The name to greet"
                }
            },
            "required": ["name"]
        })
    }

    fn capabilities(&self) -> Vec<String> {
        vec![] // No special capabilities needed
    }

    async fn execute(&self, params: Value) -> Result<String, anyhow::Error> {
        let name = params["name"].as_str().unwrap_or("World");
        Ok(format!("Hello, {}!", name))
    }
}
```

2. Register in `src/skills/builtin/mod.rs`:

```rust
pub mod hello;
pub use hello::HelloSkill;
```

3. Add to registry in `src/skills/registry.rs`.

### Wasm Skill (Rust → Wasm)

1. Create skill directory:

```
skills/my-skill/
├── Cargo.toml
├── skill.toml
└── src/
    └── lib.rs
```

2. Define the manifest (`skill.toml`):

```toml
[skill]
id = "my-skill"
name = "My Custom Skill"
version = "0.1.0"
description = "Does something useful"

[skill.capabilities]
network = ["api.example.com:443"]

[skill.execution]
tier = "light"
max_execution_time = 10
```

3. Implement the skill (`src/lib.rs`):

```rust
use fluux_skill_sdk::*;

#[skill]
fn my_skill(params: Params) -> Result<String> {
    let query = params.get_str("query")?;

    // Use capability-gated host functions
    let response = http_get(&format!(
        "https://api.example.com/search?q={}",
        query
    ))?;

    Ok(response)
}
```

4. Build to Wasm:

```bash
cargo build --target wasm32-wasi --release
cp target/wasm32-wasi/release/my_skill.wasm ../skill.wasm
```

5. The agent loads it automatically from the `skills/` directory.

### MCP Server

Use an existing MCP server or write one following the [MCP specification](https://modelcontextprotocol.io/):

```toml
# config/agent.toml
[[skills.mcp.servers]]
name = "my-mcp-server"
command = "node"
args = ["./my-mcp-server/index.js"]
env = { API_KEY = "${MY_API_KEY}" }
capabilities = ["network:api.myservice.com:443"]
```

---

## Model Tiering

Skills can specify which model tier they need:

```toml
[skill.execution]
tier = "light"    # Use cheap/fast model (e.g., Haiku, local Ollama)
# tier = "standard"  # Default conversation model
# tier = "heavy"     # Complex reasoning (e.g., Opus)
# tier = "vision"    # Image understanding required
```

This lets routine skills run on cheaper models while reserving expensive models for complex tasks.

---

## Proactive Skills

Skills can run on a schedule (v0.3):

```toml
[skill.schedule]
cron = "0 7 * * 1-5"              # Every weekday at 7am
notify = "user@example.com"       # Send results via XMPP
```

Use cases:
- Morning email digest
- Daily calendar summary
- Periodic health checks
- Automated reports

---

## Roadmap

| Version | Feature |
|---------|---------|
| v0.2 | Skill trait, registry, builtin skills, LLM tool use |
| v0.3 | MCP bridge, proactive scheduling |
| v0.4 | Wasm plugins, kernel sandboxing, process isolation |
| v0.5 | Destructive action confirmation protocol |

---

## References

- [SECURITY.md](SECURITY.md) — Defense-in-depth architecture
- [ROADMAP.md](../ROADMAP.md) — Feature timeline
- [Model Context Protocol](https://modelcontextprotocol.io/) — MCP specification
- [wasmtime](https://wasmtime.dev/) — WebAssembly runtime
- [Anthropic Tool Use](https://docs.anthropic.com/claude/docs/tool-use) — Claude function calling
