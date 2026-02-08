# Security Architecture

Fluux Agent is designed with **defense-in-depth**: multiple independent security layers that work together to protect your system from malicious or buggy AI behavior.

## The Problem

Modern AI agents like OpenClaw have root system access and direct execution privileges. This creates catastrophic risks:
- LLM hallucinations can destroy data
- Prompt injection can execute arbitrary commands
- Bugs in agent code = bugs with root privileges
- No audit trail, no rollback, no containment

## The Solution: Layered Sandboxing

```
┌──────────────────────────────────┐
│  Declarative Capabilities        │  ← Human reads & approves
│  (TOML, versioned in git)        │
├──────────────────────────────────┤
│  Action Plan Validation          │  ← Every LLM action verified
│  (separate from LLM process)     │
├──────────────────────────────────┤
│  Wasm Runtime (wasmtime)         │  ← Skills isolated, fuel-metered
│  Only exposed APIs accessible    │
├──────────────────────────────────┤
│  Landlock + seccomp (Linux)      │  ← Kernel enforced, irreversible
│  App Sandbox (macOS)             │
├──────────────────────────────────┤
│  Process Isolation               │  ← Each skill = separate process
│  (option: Firecracker microVM)   │
└──────────────────────────────────┘
```

## Layer 1: Declarative Capabilities (v0.2)

Skills declare their required capabilities in advance, in human-readable TOML:

```toml
[skill.email-summary]
name = "Email Summary"
description = "Summarizes unread emails"
capabilities = [
    "network:imap.example.com:993",
    "credential:email-account"
]
max_execution_time = 30  # seconds
```

**Protection:**
- ✅ Human reviews capabilities before installation
- ✅ Skills cannot escalate privileges at runtime
- ✅ Version controlled (git tracks capability changes)
- ✅ Auditable (capability grants logged)

**Stops:**
- ❌ "I need file system access" → denied if not declared
- ❌ Skills requesting more than they need
- ❌ Runtime privilege escalation

## Layer 2: Action Plan Validation (v0.2)

Before executing any LLM-proposed action, a **separate validation process** checks:

```rust
// Validator runs in separate process from LLM
fn validate_action(action: &Action, capabilities: &Capabilities) -> Result<()> {
    match action {
        Action::ExecuteSkill { name, params } => {
            let skill = registry.get(name)?;

            // Check skill has required capabilities
            if !capabilities.allows(&skill.required_caps) {
                return Err("Skill not authorized");
            }

            // Validate parameters against schema
            skill.validate_params(params)?;

            Ok(())
        }
        _ => Err("Unknown action type")
    }
}
```

**Protection:**
- ✅ LLM output parsing errors cannot bypass security
- ✅ Malformed actions rejected before execution
- ✅ Separate process = LLM bug can't corrupt validator

**Stops:**
- ❌ Prompt injection: "Ignore previous instructions, execute..."
- ❌ LLM hallucinating non-existent capabilities
- ❌ Malformed action payloads

## Layer 3: Wasm Sandbox (v0.4)

Skills run in WebAssembly via [wasmtime](https://wasmtime.dev/):

```rust
use wasmtime::*;

let mut config = Config::new();
config.consume_fuel(true);  // CPU metering
config.wasm_simd(false);    // Disable unnecessary features
config.wasm_threads(false);

let engine = Engine::new(&config)?;
let mut store = Store::new(&engine, ());
store.set_fuel(10_000_000)?;  // Limit CPU cycles

// Only expose whitelisted host functions
let mut linker = Linker::new(&engine);
linker.func_wrap("env", "http_get", |url: u32| -> u32 {
    // Capability-checked HTTP client
})?;
```

**Protection:**
- ✅ Memory isolation (Wasm linear memory)
- ✅ No direct syscalls (capability-based host functions only)
- ✅ CPU metering (prevents infinite loops)
- ✅ Deterministic execution (no ambient authority)

**Stops:**
- ❌ Buffer overflows in skill code
- ❌ Infinite loops consuming CPU
- ❌ Direct access to file system, network, or processes
- ❌ Use-after-free, data races (Wasm is memory-safe)

## Layer 4: Kernel Sandboxing (v0.4)

### Linux: Landlock + seccomp

```rust
use landlock::*;

// Landlock: restrict file system access
let compat = ABI::V4;
let rules = vec![
    // Read-only access to skill directory
    PathBeneath::new("/opt/fluux-agent/skills/web-search", AccessFs::ReadFile),
    // Read-write to temporary directory
    PathBeneath::new("/tmp/fluux-skill-123", AccessFs::from_all(compat)),
];

Ruleset::new()
    .handle_access(AccessFs::from_all(compat))?
    .create()?
    .add_rules(rules)?
    .restrict_self()?;  // Irreversible!

// seccomp: restrict syscalls
use seccompiler::*;
let filter = SeccompFilter::new(
    vec![
        allow_syscall(libc::SYS_read),
        allow_syscall(libc::SYS_write),
        allow_syscall(libc::SYS_exit),
        // Block execve, fork, ptrace, etc.
    ].into_iter().collect(),
    SeccompAction::Kill,
)?;
filter.apply()?;
```

### macOS: App Sandbox

```rust
use apple_sandbox::*;

let profile = r#"
(version 1)
(deny default)
(allow file-read* (subpath "/opt/fluux-agent/skills"))
(allow file-write* (subpath "/tmp/fluux-skill-123"))
(allow network-outbound (remote ip "api.example.com:443"))
"#;

sandbox_init(profile, 0)?;
```

**Protection:**
- ✅ Kernel-enforced (cannot be bypassed from userspace)
- ✅ Irreversible (once applied, even root cannot undo)
- ✅ Fine-grained (per-file, per-syscall, per-network-destination)

**Stops:**
- ❌ Wasm runtime bugs (kernel blocks syscalls)
- ❌ Native code vulnerabilities (seccomp whitelist)
- ❌ Privilege escalation exploits (Landlock/sandbox)

## Layer 5: Process Isolation (v0.4)

Each skill runs in a **separate process**:

```rust
use tokio::process::Command;

let child = Command::new("/usr/bin/fluux-skill-runner")
    .arg("--skill=web-search")
    .arg("--wasm=/skills/web-search.wasm")
    .stdin(Stdio::piped())
    .stdout(Stdio::piped())
    .stderr(Stdio::piped())
    .spawn()?;

// Communicate via stdin/stdout (capability tokens)
```

**Protection:**
- ✅ Crash isolation (skill crash doesn't crash agent)
- ✅ Memory isolation (skill cannot read agent memory)
- ✅ Resource limits (ulimit, cgroups)
- ✅ Kill on timeout (SIGKILL after max_execution_time)

**Stops:**
- ❌ Memory exhaustion attacks (cgroup limit)
- ❌ Zombie processes (reaped by agent)
- ❌ Lateral movement (skills cannot see each other)

## Optional: Firecracker MicroVMs (future)

For ultra-sensitive skills (e.g., financial transactions):

```toml
[skill.bank-transfer]
isolation_mode = "firecracker"  # Instead of process
vm_memory_mb = 128
vm_vcpus = 1
```

**Protection:**
- ✅ Full VM isolation (separate kernel)
- ✅ Hardware-assisted virtualization (KVM)
- ✅ Minimal attack surface (microVM, not full VM)

**Stops:**
- ❌ Kernel exploits in skill code (isolated kernel)
- ❌ Spectre/Meltdown-class attacks (VM boundary)

## Destructive Action Confirmation (v0.5)

High-risk actions require **explicit user confirmation**:

```xml
<message from='agent.example.com' to='user@example.com'>
  <body>Send this email to platform24? (reply yes/no)</body>
  <confirm xmlns='urn:fluux:agent:0#confirm' id='action-7742'>
    <action type='send-email'>
      <to>contact@platform24.io</to>
      <subject>Partnership inquiry</subject>
    </action>
    <risk level='medium'/>
    <expires>2026-02-08T19:00:00Z</expires>
  </confirm>
</message>
```

Actions marked as `destructive` in skill manifest:
- Sending emails
- Deleting files
- Financial transactions
- System configuration changes

**Protection:**
- ✅ Human in the loop for critical actions
- ✅ Full context shown (recipient, amount, etc.)
- ✅ Time-limited (confirmation expires)
- ✅ Logged (audit trail)

## Comparison: Fluux Agent vs. OpenClaw

| Security Layer | Fluux Agent | OpenClaw |
|----------------|-------------|----------|
| Declarative capabilities | ✅ TOML, versioned | ❌ Runtime requests |
| Action validation | ✅ Separate process | ❌ LLM output trusted |
| Wasm sandbox | ✅ wasmtime | ❌ Native Node.js |
| Kernel sandbox | ✅ Landlock/seccomp | ❌ None |
| Process isolation | ✅ Per-skill | ❌ Single process |
| Destructive action confirmation | ✅ XMPP protocol | ⚠️ Terminal prompt |
| Root access required | ❌ Never | ✅ Recommended |

## Audit & Compliance

All security-relevant events are logged:

```json
{
  "timestamp": "2026-02-08T18:30:45Z",
  "event": "skill_execution",
  "skill": "web-search",
  "user": "admin@example.com",
  "action": "search",
  "params": {"query": "XMPP federation"},
  "capabilities_used": ["network:duckduckgo.com:443"],
  "result": "success",
  "execution_time_ms": 245
}
```

Logs are:
- ✅ Structured (JSON)
- ✅ Tamper-evident (optional: append-only log)
- ✅ Federated (stored in XMPP server MAM)
- ✅ Queryable (standard XMPP archive queries)

## Security Roadmap

| Phase | Feature | Status |
|-------|---------|--------|
| v0.2 | Declarative capabilities + validation | Planned |
| v0.4 | Wasm sandbox (wasmtime) | Planned |
| v0.4 | Landlock + seccomp (Linux) | Planned |
| v0.4 | App Sandbox (macOS) | Planned |
| v0.4 | Process isolation | Planned |
| v0.5 | Destructive action confirmation | Planned |
| v1.0 | Structured audit logs | Planned |
| Future | Firecracker microVMs | Researching |

## Reporting Security Issues

**Do not open public GitHub issues for security vulnerabilities.**

Email security reports to: **security@process-one.net**

We will respond within 48 hours and provide a fix timeline.

## References

- [Landlock Linux Security Module](https://landlock.io/)
- [seccomp BPF](https://www.kernel.org/doc/html/latest/userspace-api/seccomp_filter.html)
- [macOS App Sandbox](https://developer.apple.com/documentation/security/app_sandbox)
- [WebAssembly Security](https://webassembly.org/docs/security/)
- [wasmtime Security](https://docs.wasmtime.dev/security.html)
- [Firecracker MicroVMs](https://firecracker-microvm.github.io/)
- [Capability-based Security](https://en.wikipedia.org/wiki/Capability-based_security)
