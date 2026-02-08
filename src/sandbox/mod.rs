//! Execution sandbox — stub for v0.4
//!
//! In v0.4, this module will implement:
//! - Wasmtime runtime for isolated skill execution
//! - Landlock (Linux) for kernel-level filesystem sandboxing
//! - seccomp-bpf for syscall filtering
//! - Capability validation before each execution
//!
//! The security model is layered:
//!
//! 1. Declarative capabilities (TOML) — human reads and approves
//! 2. Action plan validation — each LLM action verified
//! 3. Wasm runtime (wasmtime) — isolated skills, fuel-metered
//! 4. Landlock + seccomp (Linux) — kernel enforced, irreversible
//! 5. Process isolation — each skill = separate process
