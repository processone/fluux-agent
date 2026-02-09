# Contributing to Fluux Agent

Thank you for your interest in contributing to Fluux Agent!

## Development Setup

### Prerequisites

- Rust 1.75+ (install via [rustup](https://rustup.rs/))
- An XMPP server (e.g., ejabberd, Prosody, Openfire) with either:
  - Component protocol support, or
  - A dedicated bot account
- API access to an LLM provider (Anthropic Claude or Ollama)

### Getting Started

```bash
# Clone the repository
git clone https://github.com/processone/fluux-agent.git
cd fluux-agent

# Build the project
cargo build

# Run tests
cargo test

# Run the agent (requires config.toml)
cargo run
```

## Project Structure

```
fluux-agent/
├── src/
│   ├── agent/           # Agent runtime and skill system
│   ├── xmpp/            # XMPP component protocol implementation
│   ├── llm/             # LLM client abstraction and backends
│   ├── config/          # Configuration management
│   └── main.rs          # Entry point
├── docs/                # Documentation
├── data/                # Runtime data (memory, sessions)
└── Cargo.toml           # Project dependencies
```

## Contribution Workflow

### 1. Create a Branch

```bash
git checkout -b feature/your-feature-name
# or
git checkout -b fix/your-bug-fix
```

### 2. Make Your Changes

- Follow existing code style
- Add tests for new functionality
- Update documentation if needed

### 3. Test Your Changes

```bash
# Run all tests
cargo test

# Run clippy for linting
cargo clippy -- -D warnings

# Check formatting
cargo fmt --check

# Build to verify no errors
cargo build
```

### 4. Commit Your Changes

Write clear commit messages:

```bash
git commit -m "feat: add new feature description"
git commit -m "fix: resolve bug description"
git commit -m "docs: update documentation"
git commit -m "test: add tests for feature"
git commit -m "chore: update dependencies"
```

### 5. Create a Pull Request

- Push your branch to GitHub
- Open a Pull Request against `main`
- Fill in the PR template
- Wait for CI checks to pass

### 6. Merge

- PRs are squash-merged to keep history clean
- Each PR becomes a single commit on main

## Code Guidelines

### Rust Style

- Follow Rust idioms and conventions
- Use `rustfmt` for consistent formatting
- Address all `clippy` warnings
- Write idiomatic async/await code with Tokio
- Prefer `Result` and `?` operator for error handling

### Architecture Principles

This project emphasizes **collaborative design** and **human-in-the-loop** approaches:
- The agent should ask for clarification rather than making assumptions
- Users should guide the agent's actions through conversation
- Implement clear skill boundaries and approval mechanisms
- Design for transparency and user control

### Testing

- Write unit tests for new functionality
- Use Rust's built-in test framework
- Mock external dependencies (XMPP, LLM APIs)
- Test files go in the same module (`#[cfg(test)]`) or `tests/` directory

### Skills Development

When adding new skills:
- Follow the trait-based architecture in `src/agent/skill.rs`
- Document required permissions and capabilities
- Include examples in skill descriptions
- Consider security implications of the skill's actions

## Branching Strategy

### Feature Development

1. Create a feature or fix branch from `main`:
   ```bash
   git checkout -b feature/your-feature
   git checkout -b fix/your-bugfix
   ```

2. Open a Pull Request against `main`

3. After review, merge to `main` (squash merge)

### Releases

- When ready to release, tag directly on `main`:
  ```bash
  git tag -a v0.9.0 -m "Release v0.9.0"
  git push origin v0.9.0
  ```

### Hotfixes

If a critical fix is needed for a released version:

1. Create a hotfix branch from the release tag:
   ```bash
   git checkout -b hotfix/0.9.1 v0.9.0
   ```

2. Apply the fix and tag:
   ```bash
   git commit -m "fix: critical bug description"
   git tag -a v0.9.1 -m "Release v0.9.1"
   git push origin v0.9.1
   ```

3. Merge the fix back to `main`:
   ```bash
   git checkout main
   git merge hotfix/0.9.1
   ```

## Getting Help

- Open an issue for bugs or feature requests
- Check existing issues before creating new ones

## Contributor License Agreement

Before we can accept your contribution, you must sign our Contributor License Agreement (CLA). This is a one-time process that takes only a few minutes.

**[Sign the CLA](https://cla.process-one.net)**

The CLA ensures that:
- You have the right to contribute the code
- ProcessOne can distribute your contribution under the project license
- Your contribution can be relicensed by ProcessOne in its Business offering.

## License

By contributing, you agree that your contributions will be licensed under the Apache-2.0 license.

Note: Enterprise features (multi-agent federation, multi-tenant, audit, SSO) will be distributed under BSL 1.1 (Business Source License), which automatically converts to Apache-2.0 after 4 years. The core agent runtime remains Apache-2.0.
