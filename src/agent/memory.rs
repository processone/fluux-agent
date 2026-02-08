use anyhow::Result;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use tracing::info;

use crate::llm::Message;

/// Aggregated workspace context for system prompt assembly.
///
/// Global files (instructions, identity, personality) are shared across all JIDs.
/// Per-JID files (user_profile, user_memory) are isolated per conversation partner.
pub struct WorkspaceContext {
    pub instructions: Option<String>,
    pub identity: Option<String>,
    pub personality: Option<String>,
    pub user_profile: Option<String>,
    pub user_memory: Option<String>,
}

/// Persistent conversational memory per user.
/// Stores conversation history and user context as
/// markdown files for transparency and portability.
///
/// Layout:
///   {base_path}/instructions.md           — global agent behavior rules
///   {base_path}/identity.md               — global agent identity
///   {base_path}/personality.md            — global agent personality/tone
///   {base_path}/{jid}/history.md          — current session conversation log
///   {base_path}/{jid}/user.md             — what the agent knows about the user
///   {base_path}/{jid}/memory.md           — long-term notes about the user
///   {base_path}/{jid}/sessions/           — archived sessions
///   {base_path}/{jid}/sessions/{ts}.md    — archived session file
pub struct Memory {
    base_path: PathBuf,
}

impl Memory {
    pub fn open(path: &Path) -> Result<Self> {
        fs::create_dir_all(path)?;
        info!("Memory store opened at {}", path.display());

        // Log workspace file discovery
        let global_files = ["instructions.md", "identity.md", "personality.md"];
        let found: Vec<&str> = global_files
            .iter()
            .filter(|f| path.join(f).exists() && fs::read_to_string(path.join(f)).map(|c| !c.trim().is_empty()).unwrap_or(false))
            .copied()
            .collect();

        if found.is_empty() {
            info!("Workspace: no global files found (instructions.md, identity.md, personality.md) — using default prompt");
        } else {
            let missing: Vec<&str> = global_files
                .iter()
                .filter(|f| !found.contains(f))
                .copied()
                .collect();
            if missing.is_empty() {
                info!("Workspace: loaded {}", found.join(", "));
            } else {
                info!("Workspace: loaded {} ({} not found)", found.join(", "), missing.join(", "));
            }
        }

        Ok(Self {
            base_path: path.to_path_buf(),
        })
    }

    /// Returns the per-user directory, creating it if needed
    fn user_dir(&self, jid: &str) -> Result<PathBuf> {
        let dir = self.base_path.join(jid);
        fs::create_dir_all(&dir)?;
        Ok(dir)
    }

    // ── Global workspace files ────────────────────────────

    /// Reads a global workspace file (e.g., "instructions.md").
    /// Returns None if the file doesn't exist or is empty.
    pub fn get_global_file(&self, filename: &str) -> Result<Option<String>> {
        let path = self.base_path.join(filename);
        if !path.exists() {
            return Ok(None);
        }
        let content = fs::read_to_string(&path)?;
        if content.trim().is_empty() {
            Ok(None)
        } else {
            Ok(Some(content))
        }
    }

    /// Reads a workspace file with per-JID override support.
    ///
    /// Checks `{jid}/{filename}` first — if present and non-empty, it wins.
    /// Otherwise falls back to the global `{base_path}/{filename}`.
    ///
    /// This lets rooms (or individual users) override identity, personality,
    /// or instructions by placing files in their memory directory.
    fn get_workspace_file(&self, jid: &str, filename: &str) -> Result<Option<String>> {
        // Per-JID override
        let local_path = self.base_path.join(jid).join(filename);
        if local_path.exists() {
            let content = fs::read_to_string(&local_path)?;
            if !content.trim().is_empty() {
                return Ok(Some(content));
            }
        }

        // Global fallback
        self.get_global_file(filename)
    }

    /// Assembles the full workspace context for a JID.
    ///
    /// For instructions, identity, and personality: checks the per-JID
    /// directory first (e.g., `{room_jid}/instructions.md`), falling back
    /// to the global file. This allows per-room or per-user identity
    /// without any config changes — just drop files into the JID directory.
    pub fn get_workspace_context(&self, jid: &str) -> Result<WorkspaceContext> {
        Ok(WorkspaceContext {
            instructions: self.get_workspace_file(jid, "instructions.md")?,
            identity: self.get_workspace_file(jid, "identity.md")?,
            personality: self.get_workspace_file(jid, "personality.md")?,
            user_profile: self.get_user_profile(jid)?,
            user_memory: self.get_user_memory(jid)?,
        })
    }

    // ── Per-JID files ─────────────────────────────────────

    /// Reads the user profile (user.md), falling back to context.md
    /// for backward compatibility with pre-workspace memory layout.
    pub fn get_user_profile(&self, jid: &str) -> Result<Option<String>> {
        let user_md = self.base_path.join(jid).join("user.md");
        if user_md.exists() {
            let content = fs::read_to_string(&user_md)?;
            return if content.trim().is_empty() {
                Ok(None)
            } else {
                Ok(Some(content))
            };
        }

        // Backward compatibility: try context.md
        let context_md = self.base_path.join(jid).join("context.md");
        if context_md.exists() {
            let content = fs::read_to_string(&context_md)?;
            return if content.trim().is_empty() {
                Ok(None)
            } else {
                Ok(Some(content))
            };
        }

        Ok(None)
    }

    /// Writes the user profile to user.md
    pub fn set_user_profile(&self, jid: &str, content: &str) -> Result<()> {
        let path = self.user_dir(jid)?.join("user.md");
        fs::write(&path, content)?;
        Ok(())
    }

    /// Checks if a user profile exists (user.md or context.md fallback)
    pub fn has_user_profile(&self, jid: &str) -> Result<bool> {
        Ok(self.get_user_profile(jid)?.is_some())
    }

    /// Reads the per-JID long-term memory (memory.md)
    pub fn get_user_memory(&self, jid: &str) -> Result<Option<String>> {
        let path = self.base_path.join(jid).join("memory.md");
        if !path.exists() {
            return Ok(None);
        }
        let content = fs::read_to_string(&path)?;
        if content.trim().is_empty() {
            Ok(None)
        } else {
            Ok(Some(content))
        }
    }

    /// Writes long-term memory for a JID
    pub fn set_user_memory(&self, jid: &str, content: &str) -> Result<()> {
        let path = self.user_dir(jid)?.join("memory.md");
        fs::write(&path, content)?;
        Ok(())
    }

    // ── Backward-compatible context API ───────────────────

    /// Stores or updates the user context (delegates to user.md).
    /// Kept for backward compatibility.
    pub fn set_user_context(&self, jid: &str, context: &str) -> Result<()> {
        self.set_user_profile(jid, context)
    }

    /// Retrieves the user context (delegates to user profile with context.md fallback).
    /// Kept for backward compatibility.
    pub fn get_user_context(&self, jid: &str) -> Result<Option<String>> {
        self.get_user_profile(jid)
    }

    // ── History (conversation log) ────────────────────────

    /// Appends a message to the user's current session (history.md).
    ///
    /// For user messages, the JID is included in the header for traceability:
    ///   `### user (alice@example.com)`
    /// For assistant messages, no JID is included (agent identity may change):
    ///   `### assistant`
    pub fn store_message(&self, jid: &str, role: &str, content: &str) -> Result<()> {
        self.store_message_with_jid(jid, role, content, Some(jid))
    }

    /// Appends a message with a custom sender label in the header.
    /// Used for MUC rooms where the sender is identified by nick, not JID.
    ///
    /// `sender_label` is included in user headers: `### user (sender_label)`
    /// Pass None to omit the label (assistant messages always omit it).
    pub fn store_message_with_jid(
        &self,
        jid: &str,
        role: &str,
        content: &str,
        sender_label: Option<&str>,
    ) -> Result<()> {
        let path = self.user_dir(jid)?.join("history.md");
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)?;

        if role == "user" {
            if let Some(label) = sender_label {
                writeln!(file, "### user ({label})\n{content}\n")?;
            } else {
                writeln!(file, "### user\n{content}\n")?;
            }
        } else {
            writeln!(file, "### {role}\n{content}\n")?;
        }
        Ok(())
    }

    /// Retrieves the last N messages from the current session
    pub fn get_history(&self, jid: &str, limit: usize) -> Result<Vec<Message>> {
        let path = self.base_path.join(jid).join("history.md");

        if !path.exists() {
            return Ok(Vec::new());
        }

        let content = fs::read_to_string(&path)?;
        let messages = parse_history(&content);

        // Return only the last `limit` messages
        let start = messages.len().saturating_sub(limit);
        Ok(messages[start..].to_vec())
    }

    /// Starts a new session for a user.
    ///
    /// Archives the current history.md into sessions/{timestamp}.md
    /// and clears the current history so the LLM starts fresh.
    /// Returns a human-readable summary of what happened.
    pub fn new_session(&self, jid: &str) -> Result<String> {
        let user_dir = self.user_dir(jid)?;
        let history_path = user_dir.join("history.md");

        if !history_path.exists() {
            return Ok("No active session to archive.".to_string());
        }

        let content = fs::read_to_string(&history_path)?;
        let message_count = parse_history(&content).len();

        if message_count == 0 {
            return Ok("Session is already empty.".to_string());
        }

        // Archive to sessions/ directory with timestamp
        let sessions_dir = user_dir.join("sessions");
        fs::create_dir_all(&sessions_dir)?;

        let timestamp = chrono::Utc::now().format("%Y%m%d-%H%M%S");
        let archive_path = sessions_dir.join(format!("{timestamp}.md"));
        fs::rename(&history_path, &archive_path)?;

        info!(
            "Archived session for {jid}: {message_count} messages → {}",
            archive_path.display()
        );

        Ok(format!(
            "Session archived ({message_count} messages). Starting fresh."
        ))
    }

    /// Erases all active memory for a user (history + user profile + memory).
    /// Archived sessions are preserved.
    pub fn forget(&self, jid: &str) -> Result<String> {
        let user_dir = self.base_path.join(jid);

        if !user_dir.exists() {
            return Ok("No memory to erase.".to_string());
        }

        let mut erased = Vec::new();

        // Erase history
        let history_path = user_dir.join("history.md");
        if history_path.exists() {
            let content = fs::read_to_string(&history_path)?;
            let count = parse_history(&content).len();
            fs::remove_file(&history_path)?;
            erased.push(format!("{count} messages"));
        }

        // Erase user profile (user.md and legacy context.md)
        let user_md = user_dir.join("user.md");
        let context_md = user_dir.join("context.md");
        if user_md.exists() {
            fs::remove_file(&user_md)?;
            erased.push("user profile".to_string());
        }
        if context_md.exists() {
            fs::remove_file(&context_md)?;
            if !erased.iter().any(|s| s.contains("profile")) {
                erased.push("context".to_string());
            }
        }

        // Erase long-term memory
        let memory_md = user_dir.join("memory.md");
        if memory_md.exists() {
            fs::remove_file(&memory_md)?;
            erased.push("memory".to_string());
        }

        if erased.is_empty() {
            Ok("No active memory to erase (archived sessions preserved).".to_string())
        } else {
            Ok(format!(
                "Erased: {}. Archived sessions preserved.",
                erased.join(", ")
            ))
        }
    }

    /// Total number of messages in the current session for a JID
    pub fn message_count(&self, jid: &str) -> Result<usize> {
        let path = self.base_path.join(jid).join("history.md");

        if !path.exists() {
            return Ok(0);
        }

        let content = fs::read_to_string(&path)?;
        Ok(parse_history(&content).len())
    }

    /// Number of archived sessions for a JID
    pub fn session_count(&self, jid: &str) -> Result<usize> {
        let sessions_dir = self.base_path.join(jid).join("sessions");

        if !sessions_dir.exists() {
            return Ok(0);
        }

        let count = fs::read_dir(&sessions_dir)?
            .filter_map(|e| e.ok())
            .filter(|e| {
                e.path()
                    .extension()
                    .map(|ext| ext == "md")
                    .unwrap_or(false)
            })
            .count();

        Ok(count)
    }
}

/// Parses a history.md file into a list of messages.
///
/// Supports both old and new header formats:
///   `### user`                    — old format (no JID)
///   `### user (alice@example.com)` — new format (with JID)
///   `### assistant`               — assistant (no JID in either format)
///
/// The returned `Message.role` is always the bare role ("user" or "assistant"),
/// with the JID suffix stripped.
fn parse_history(content: &str) -> Vec<Message> {
    let mut messages = Vec::new();
    let mut current_role: Option<String> = None;
    let mut current_content = String::new();

    for line in content.lines() {
        if let Some(role_raw) = line.strip_prefix("### ") {
            // Flush previous message
            if let Some(r) = current_role.take() {
                let text = current_content.trim().to_string();
                if !text.is_empty() {
                    messages.push(Message {
                        role: r,
                        content: text,
                    });
                }
            }
            // Extract bare role, stripping optional " (jid)" suffix
            let role = extract_bare_role(role_raw.trim());
            current_role = Some(role);
            current_content.clear();
        } else if current_role.is_some() {
            current_content.push_str(line);
            current_content.push('\n');
        }
    }

    // Flush last message
    if let Some(r) = current_role {
        let text = current_content.trim().to_string();
        if !text.is_empty() {
            messages.push(Message {
                role: r,
                content: text,
            });
        }
    }

    messages
}

/// Extracts the bare role from a header like "user (alice@example.com)" → "user"
/// or "assistant" → "assistant".
fn extract_bare_role(role_raw: &str) -> String {
    if let Some(paren_pos) = role_raw.find(" (") {
        role_raw[..paren_pos].to_string()
    } else {
        role_raw.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── parse_history tests ───────────────────────────────

    #[test]
    fn test_parse_history_basic() {
        let content = "\
### user
Hello!

### assistant
Hi there! How can I help?

### user
What's the weather?

### assistant
I can't check yet, but that feature is coming soon!
";
        let messages = parse_history(content);
        assert_eq!(messages.len(), 4);
        assert_eq!(messages[0].role, "user");
        assert_eq!(messages[0].content, "Hello!");
        assert_eq!(messages[1].role, "assistant");
        assert_eq!(messages[1].content, "Hi there! How can I help?");
        assert_eq!(messages[2].role, "user");
        assert_eq!(messages[2].content, "What's the weather?");
        assert_eq!(messages[3].role, "assistant");
        assert_eq!(
            messages[3].content,
            "I can't check yet, but that feature is coming soon!"
        );
    }

    #[test]
    fn test_parse_history_multiline_content() {
        let content = "\
### user
Can you help me with:
1. First thing
2. Second thing

### assistant
Sure! Let me address both:

1. For the first thing, do X.
2. For the second, do Y.
";
        let messages = parse_history(content);
        assert_eq!(messages.len(), 2);
        assert!(messages[0].content.contains("1. First thing"));
        assert!(messages[1].content.contains("1. For the first thing"));
    }

    #[test]
    fn test_parse_history_empty() {
        let messages = parse_history("");
        assert!(messages.is_empty());
    }

    #[test]
    fn test_parse_history_with_jid_format() {
        let content = "\
### user (alice@example.com)
Hello!

### assistant
Hi there!

### user (alice@example.com)
How are you?
";
        let messages = parse_history(content);
        assert_eq!(messages.len(), 3);
        assert_eq!(messages[0].role, "user");
        assert_eq!(messages[0].content, "Hello!");
        assert_eq!(messages[1].role, "assistant");
        assert_eq!(messages[1].content, "Hi there!");
        assert_eq!(messages[2].role, "user");
        assert_eq!(messages[2].content, "How are you?");
    }

    #[test]
    fn test_parse_history_mixed_old_and_new_format() {
        let content = "\
### user
Old message without JID

### assistant
Response

### user (alice@example.com)
New message with JID
";
        let messages = parse_history(content);
        assert_eq!(messages.len(), 3);
        assert_eq!(messages[0].role, "user");
        assert_eq!(messages[0].content, "Old message without JID");
        assert_eq!(messages[2].role, "user");
        assert_eq!(messages[2].content, "New message with JID");
    }

    #[test]
    fn test_extract_bare_role() {
        assert_eq!(extract_bare_role("user"), "user");
        assert_eq!(extract_bare_role("assistant"), "assistant");
        assert_eq!(extract_bare_role("user (alice@example.com)"), "user");
        assert_eq!(
            extract_bare_role("user (room@conference.example.com)"),
            "user"
        );
    }

    // ── store / retrieve tests ────────────────────────────

    #[test]
    fn test_store_and_retrieve() {
        let dir = tempfile::tempdir().unwrap();
        let memory = Memory::open(dir.path()).unwrap();

        memory
            .store_message("user@test", "user", "Hello!")
            .unwrap();
        memory
            .store_message("user@test", "assistant", "Hi there!")
            .unwrap();

        let history = memory.get_history("user@test", 10).unwrap();
        assert_eq!(history.len(), 2);
        assert_eq!(history[0].role, "user");
        assert_eq!(history[0].content, "Hello!");
        assert_eq!(history[1].role, "assistant");
        assert_eq!(history[1].content, "Hi there!");
    }

    #[test]
    fn test_store_message_includes_jid_in_header() {
        let dir = tempfile::tempdir().unwrap();
        let memory = Memory::open(dir.path()).unwrap();

        memory
            .store_message("alice@example.com", "user", "Hello!")
            .unwrap();
        memory
            .store_message("alice@example.com", "assistant", "Hi!")
            .unwrap();

        // Read raw file to verify format
        let raw = fs::read_to_string(dir.path().join("alice@example.com/history.md")).unwrap();
        assert!(raw.contains("### user (alice@example.com)"));
        assert!(raw.contains("### assistant"));
        assert!(!raw.contains("### assistant ("));

        // Parsed history should strip the JID
        let history = memory.get_history("alice@example.com", 10).unwrap();
        assert_eq!(history[0].role, "user");
        assert_eq!(history[1].role, "assistant");
    }

    #[test]
    fn test_history_limit() {
        let dir = tempfile::tempdir().unwrap();
        let memory = Memory::open(dir.path()).unwrap();

        for i in 0..10 {
            memory
                .store_message("user@test", "user", &format!("Message {i}"))
                .unwrap();
        }

        let history = memory.get_history("user@test", 3).unwrap();
        assert_eq!(history.len(), 3);
        assert_eq!(history[0].content, "Message 7");
        assert_eq!(history[2].content, "Message 9");
    }

    // ── User profile / context tests ──────────────────────

    #[test]
    fn test_user_context() {
        let dir = tempfile::tempdir().unwrap();
        let memory = Memory::open(dir.path()).unwrap();

        assert!(memory.get_user_context("user@test").unwrap().is_none());

        memory
            .set_user_context("user@test", "Prefers concise answers.")
            .unwrap();
        let ctx = memory.get_user_context("user@test").unwrap();
        assert_eq!(ctx.unwrap(), "Prefers concise answers.");

        // Update overwrites
        memory
            .set_user_context("user@test", "Prefers detailed answers.")
            .unwrap();
        let ctx = memory.get_user_context("user@test").unwrap();
        assert_eq!(ctx.unwrap(), "Prefers detailed answers.");
    }

    #[test]
    fn test_user_profile_fallback_from_context_md() {
        let dir = tempfile::tempdir().unwrap();
        let memory = Memory::open(dir.path()).unwrap();

        // Write a legacy context.md directly
        let jid_dir = dir.path().join("user@test");
        fs::create_dir_all(&jid_dir).unwrap();
        fs::write(jid_dir.join("context.md"), "Legacy context data").unwrap();

        // get_user_profile should find it via fallback
        let profile = memory.get_user_profile("user@test").unwrap();
        assert_eq!(profile.unwrap(), "Legacy context data");
    }

    #[test]
    fn test_user_profile_prefers_user_md_over_context_md() {
        let dir = tempfile::tempdir().unwrap();
        let memory = Memory::open(dir.path()).unwrap();

        // Write both user.md and context.md
        let jid_dir = dir.path().join("user@test");
        fs::create_dir_all(&jid_dir).unwrap();
        fs::write(jid_dir.join("context.md"), "Old context").unwrap();
        fs::write(jid_dir.join("user.md"), "New user profile").unwrap();

        // user.md should take precedence
        let profile = memory.get_user_profile("user@test").unwrap();
        assert_eq!(profile.unwrap(), "New user profile");
    }

    #[test]
    fn test_set_user_profile_writes_user_md() {
        let dir = tempfile::tempdir().unwrap();
        let memory = Memory::open(dir.path()).unwrap();

        memory
            .set_user_profile("user@test", "Profile content")
            .unwrap();

        // Verify it wrote to user.md
        let content = fs::read_to_string(dir.path().join("user@test/user.md")).unwrap();
        assert_eq!(content, "Profile content");

        // And get_user_profile reads it back
        let profile = memory.get_user_profile("user@test").unwrap();
        assert_eq!(profile.unwrap(), "Profile content");
    }

    #[test]
    fn test_has_user_profile() {
        let dir = tempfile::tempdir().unwrap();
        let memory = Memory::open(dir.path()).unwrap();

        assert!(!memory.has_user_profile("user@test").unwrap());

        memory
            .set_user_profile("user@test", "Something")
            .unwrap();
        assert!(memory.has_user_profile("user@test").unwrap());
    }

    // ── User memory tests ─────────────────────────────────

    #[test]
    fn test_set_and_get_user_memory() {
        let dir = tempfile::tempdir().unwrap();
        let memory = Memory::open(dir.path()).unwrap();

        assert!(memory.get_user_memory("user@test").unwrap().is_none());

        memory
            .set_user_memory("user@test", "Likes Rust and coffee")
            .unwrap();

        let mem = memory.get_user_memory("user@test").unwrap();
        assert_eq!(mem.unwrap(), "Likes Rust and coffee");
    }

    // ── Global file tests ─────────────────────────────────

    #[test]
    fn test_get_global_file_exists() {
        let dir = tempfile::tempdir().unwrap();
        let memory = Memory::open(dir.path()).unwrap();

        fs::write(dir.path().join("instructions.md"), "Be helpful and concise.").unwrap();

        let content = memory.get_global_file("instructions.md").unwrap();
        assert_eq!(content.unwrap(), "Be helpful and concise.");
    }

    #[test]
    fn test_get_global_file_missing() {
        let dir = tempfile::tempdir().unwrap();
        let memory = Memory::open(dir.path()).unwrap();

        assert!(memory.get_global_file("instructions.md").unwrap().is_none());
    }

    #[test]
    fn test_get_global_file_empty() {
        let dir = tempfile::tempdir().unwrap();
        let memory = Memory::open(dir.path()).unwrap();

        fs::write(dir.path().join("instructions.md"), "  \n  ").unwrap();

        assert!(memory.get_global_file("instructions.md").unwrap().is_none());
    }

    // ── Workspace context tests ───────────────────────────

    #[test]
    fn test_workspace_context_all_files() {
        let dir = tempfile::tempdir().unwrap();
        let memory = Memory::open(dir.path()).unwrap();

        // Create global files
        fs::write(dir.path().join("instructions.md"), "Be concise").unwrap();
        fs::write(dir.path().join("identity.md"), "I am Fluux Agent").unwrap();
        fs::write(dir.path().join("personality.md"), "Friendly and direct").unwrap();

        // Create per-JID files
        memory
            .set_user_profile("user@test", "Developer at Acme")
            .unwrap();
        memory
            .set_user_memory("user@test", "Prefers Rust over Go")
            .unwrap();

        let ctx = memory.get_workspace_context("user@test").unwrap();
        assert_eq!(ctx.instructions.unwrap(), "Be concise");
        assert_eq!(ctx.identity.unwrap(), "I am Fluux Agent");
        assert_eq!(ctx.personality.unwrap(), "Friendly and direct");
        assert_eq!(ctx.user_profile.unwrap(), "Developer at Acme");
        assert_eq!(ctx.user_memory.unwrap(), "Prefers Rust over Go");
    }

    #[test]
    fn test_workspace_context_no_files() {
        let dir = tempfile::tempdir().unwrap();
        let memory = Memory::open(dir.path()).unwrap();

        let ctx = memory.get_workspace_context("user@test").unwrap();
        assert!(ctx.instructions.is_none());
        assert!(ctx.identity.is_none());
        assert!(ctx.personality.is_none());
        assert!(ctx.user_profile.is_none());
        assert!(ctx.user_memory.is_none());
    }

    // ── Message count tests ───────────────────────────────

    #[test]
    fn test_message_count() {
        let dir = tempfile::tempdir().unwrap();
        let memory = Memory::open(dir.path()).unwrap();

        assert_eq!(memory.message_count("user@test").unwrap(), 0);

        memory.store_message("user@test", "user", "One").unwrap();
        memory
            .store_message("user@test", "assistant", "Two")
            .unwrap();
        memory
            .store_message("user@test", "user", "Three")
            .unwrap();

        assert_eq!(memory.message_count("user@test").unwrap(), 3);
    }

    // ── Session tests ─────────────────────────────────────

    #[test]
    fn test_new_session_archives_history() {
        let dir = tempfile::tempdir().unwrap();
        let memory = Memory::open(dir.path()).unwrap();

        // Build a conversation
        memory
            .store_message("user@test", "user", "Hello!")
            .unwrap();
        memory
            .store_message("user@test", "assistant", "Hi!")
            .unwrap();
        assert_eq!(memory.message_count("user@test").unwrap(), 2);

        // Start new session
        let result = memory.new_session("user@test").unwrap();
        assert!(result.contains("2 messages"));
        assert!(result.contains("archived"));

        // Current history is now empty
        assert_eq!(memory.message_count("user@test").unwrap(), 0);
        let history = memory.get_history("user@test", 10).unwrap();
        assert!(history.is_empty());

        // Archived session exists
        assert_eq!(memory.session_count("user@test").unwrap(), 1);

        // Can start a new conversation
        memory
            .store_message("user@test", "user", "Fresh start!")
            .unwrap();
        let history = memory.get_history("user@test", 10).unwrap();
        assert_eq!(history.len(), 1);
        assert_eq!(history[0].content, "Fresh start!");
    }

    #[test]
    fn test_new_session_empty_noop() {
        let dir = tempfile::tempdir().unwrap();
        let memory = Memory::open(dir.path()).unwrap();

        // No history at all
        let result = memory.new_session("user@test").unwrap();
        assert!(result.contains("No active session"));

        // Create and immediately archive
        memory.store_message("user@test", "user", "Hi").unwrap();
        memory.new_session("user@test").unwrap();

        // Second archive with empty session
        let result = memory.new_session("user@test").unwrap();
        assert!(result.contains("No active session") || result.contains("already empty"));
    }

    #[test]
    fn test_new_session_multiple_archives() {
        let dir = tempfile::tempdir().unwrap();
        let memory = Memory::open(dir.path()).unwrap();

        // Session 1
        memory
            .store_message("user@test", "user", "Session 1")
            .unwrap();
        memory.new_session("user@test").unwrap();

        // Brief pause to ensure different timestamps
        std::thread::sleep(std::time::Duration::from_millis(1100));

        // Session 2
        memory
            .store_message("user@test", "user", "Session 2")
            .unwrap();
        memory.new_session("user@test").unwrap();

        assert_eq!(memory.session_count("user@test").unwrap(), 2);
    }

    // ── Forget tests ──────────────────────────────────────

    #[test]
    fn test_forget_erases_active_memory() {
        let dir = tempfile::tempdir().unwrap();
        let memory = Memory::open(dir.path()).unwrap();

        memory
            .store_message("user@test", "user", "Hello!")
            .unwrap();
        memory
            .set_user_context("user@test", "Likes coffee")
            .unwrap();

        let result = memory.forget("user@test").unwrap();
        assert!(result.contains("1 messages"));
        assert!(result.contains("profile") || result.contains("context"));

        // History and context are gone
        assert_eq!(memory.message_count("user@test").unwrap(), 0);
        assert!(memory.get_user_context("user@test").unwrap().is_none());
    }

    #[test]
    fn test_forget_erases_user_md_and_memory_md() {
        let dir = tempfile::tempdir().unwrap();
        let memory = Memory::open(dir.path()).unwrap();

        memory
            .store_message("user@test", "user", "Hello!")
            .unwrap();
        memory
            .set_user_profile("user@test", "Developer")
            .unwrap();
        memory
            .set_user_memory("user@test", "Likes Rust")
            .unwrap();

        let result = memory.forget("user@test").unwrap();
        assert!(result.contains("1 messages"));
        assert!(result.contains("user profile"));
        assert!(result.contains("memory"));

        // All active memory is gone
        assert_eq!(memory.message_count("user@test").unwrap(), 0);
        assert!(memory.get_user_profile("user@test").unwrap().is_none());
        assert!(memory.get_user_memory("user@test").unwrap().is_none());
    }

    #[test]
    fn test_forget_preserves_archives() {
        let dir = tempfile::tempdir().unwrap();
        let memory = Memory::open(dir.path()).unwrap();

        // Create and archive a session
        memory.store_message("user@test", "user", "Old").unwrap();
        memory.new_session("user@test").unwrap();
        assert_eq!(memory.session_count("user@test").unwrap(), 1);

        // New session + context
        memory.store_message("user@test", "user", "New").unwrap();
        memory
            .set_user_context("user@test", "Some context")
            .unwrap();

        // Forget
        memory.forget("user@test").unwrap();

        // Archives still exist
        assert_eq!(memory.session_count("user@test").unwrap(), 1);
    }

    #[test]
    fn test_forget_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let memory = Memory::open(dir.path()).unwrap();

        let result = memory.forget("nobody@test").unwrap();
        assert!(result.contains("No memory"));
    }

    // ── JID isolation tests ───────────────────────────────

    #[test]
    fn test_jid_isolation() {
        let dir = tempfile::tempdir().unwrap();
        let memory = Memory::open(dir.path()).unwrap();

        memory
            .store_message("alice@test", "user", "Alice's message")
            .unwrap();
        memory
            .set_user_profile("alice@test", "Alice's profile")
            .unwrap();

        memory
            .store_message("bob@test", "user", "Bob's message")
            .unwrap();
        memory
            .set_user_profile("bob@test", "Bob's profile")
            .unwrap();

        // Each JID sees only their own data
        let alice_history = memory.get_history("alice@test", 10).unwrap();
        assert_eq!(alice_history.len(), 1);
        assert_eq!(alice_history[0].content, "Alice's message");

        let bob_history = memory.get_history("bob@test", 10).unwrap();
        assert_eq!(bob_history.len(), 1);
        assert_eq!(bob_history[0].content, "Bob's message");

        assert_eq!(
            memory.get_user_profile("alice@test").unwrap().unwrap(),
            "Alice's profile"
        );
        assert_eq!(
            memory.get_user_profile("bob@test").unwrap().unwrap(),
            "Bob's profile"
        );
    }

    // ── MUC-specific memory tests ──────────────────────

    #[test]
    fn test_store_message_with_muc_nick_label() {
        let dir = tempfile::tempdir().unwrap();
        let memory = Memory::open(dir.path()).unwrap();

        let room_jid = "lobby@conference.localhost";
        memory
            .store_message_with_jid(room_jid, "user", "Hello everyone!", Some("alice@muc"))
            .unwrap();
        memory
            .store_message(room_jid, "assistant", "Hi Alice!")
            .unwrap();

        // Verify raw format
        let raw = fs::read_to_string(dir.path().join(room_jid).join("history.md")).unwrap();
        assert!(raw.contains("### user (alice@muc)"));
        assert!(raw.contains("### assistant"));

        // Verify parsed history strips labels
        let history = memory.get_history(room_jid, 10).unwrap();
        assert_eq!(history.len(), 2);
        assert_eq!(history[0].role, "user");
        assert_eq!(history[0].content, "Hello everyone!");
        assert_eq!(history[1].role, "assistant");
        assert_eq!(history[1].content, "Hi Alice!");
    }

    #[test]
    fn test_store_message_with_jid_none_label() {
        let dir = tempfile::tempdir().unwrap();
        let memory = Memory::open(dir.path()).unwrap();

        memory
            .store_message_with_jid("test@localhost", "user", "No label", None)
            .unwrap();

        let raw = fs::read_to_string(dir.path().join("test@localhost/history.md")).unwrap();
        assert!(raw.contains("### user\n"));
        assert!(!raw.contains("### user ("));
    }

    #[test]
    fn test_muc_history_multiple_participants() {
        let dir = tempfile::tempdir().unwrap();
        let memory = Memory::open(dir.path()).unwrap();

        let room = "dev@conference.localhost";
        memory
            .store_message_with_jid(room, "user", "Anyone around?", Some("alice@muc"))
            .unwrap();
        memory
            .store_message_with_jid(room, "user", "I'm here", Some("bob@muc"))
            .unwrap();
        memory
            .store_message_with_jid(room, "user", "@bot help us", Some("alice@muc"))
            .unwrap();
        memory
            .store_message(room, "assistant", "How can I help?")
            .unwrap();

        let history = memory.get_history(room, 20).unwrap();
        assert_eq!(history.len(), 4);
        // All participant messages are role "user" (labels stripped)
        assert_eq!(history[0].role, "user");
        assert_eq!(history[1].role, "user");
        assert_eq!(history[2].role, "user");
        assert_eq!(history[3].role, "assistant");
    }

    // ── Per-JID workspace override tests ────────────────

    #[test]
    fn test_workspace_context_per_jid_override_all() {
        let dir = tempfile::tempdir().unwrap();
        let memory = Memory::open(dir.path()).unwrap();

        // Create global files
        fs::write(dir.path().join("instructions.md"), "Global instructions").unwrap();
        fs::write(dir.path().join("identity.md"), "Global identity").unwrap();
        fs::write(dir.path().join("personality.md"), "Global personality").unwrap();

        // Create per-room overrides
        let room = "lobby@conference.localhost";
        let room_dir = dir.path().join(room);
        fs::create_dir_all(&room_dir).unwrap();
        fs::write(room_dir.join("instructions.md"), "Room instructions").unwrap();
        fs::write(room_dir.join("identity.md"), "Room identity").unwrap();
        fs::write(room_dir.join("personality.md"), "Room personality").unwrap();

        // Room context should use per-room files
        let ctx = memory.get_workspace_context(room).unwrap();
        assert_eq!(ctx.instructions.unwrap(), "Room instructions");
        assert_eq!(ctx.identity.unwrap(), "Room identity");
        assert_eq!(ctx.personality.unwrap(), "Room personality");

        // Other JID should still get global files
        let ctx2 = memory.get_workspace_context("user@localhost").unwrap();
        assert_eq!(ctx2.instructions.unwrap(), "Global instructions");
        assert_eq!(ctx2.identity.unwrap(), "Global identity");
        assert_eq!(ctx2.personality.unwrap(), "Global personality");
    }

    #[test]
    fn test_workspace_context_partial_override() {
        let dir = tempfile::tempdir().unwrap();
        let memory = Memory::open(dir.path()).unwrap();

        // Create global files
        fs::write(dir.path().join("instructions.md"), "Global instructions").unwrap();
        fs::write(dir.path().join("identity.md"), "Global identity").unwrap();
        fs::write(dir.path().join("personality.md"), "Global personality").unwrap();

        // Override only instructions for a room
        let room = "support@conference.localhost";
        let room_dir = dir.path().join(room);
        fs::create_dir_all(&room_dir).unwrap();
        fs::write(room_dir.join("instructions.md"), "Support room rules").unwrap();

        let ctx = memory.get_workspace_context(room).unwrap();
        assert_eq!(ctx.instructions.unwrap(), "Support room rules");
        assert_eq!(ctx.identity.unwrap(), "Global identity");
        assert_eq!(ctx.personality.unwrap(), "Global personality");
    }

    #[test]
    fn test_workspace_context_empty_override_falls_back_to_global() {
        let dir = tempfile::tempdir().unwrap();
        let memory = Memory::open(dir.path()).unwrap();

        // Create global file
        fs::write(dir.path().join("identity.md"), "Global identity").unwrap();

        // Create empty per-room override (should be ignored)
        let room = "dev@conference.localhost";
        let room_dir = dir.path().join(room);
        fs::create_dir_all(&room_dir).unwrap();
        fs::write(room_dir.join("identity.md"), "   \n  ").unwrap();

        let ctx = memory.get_workspace_context(room).unwrap();
        assert_eq!(ctx.identity.unwrap(), "Global identity");
    }

    #[test]
    fn test_workspace_context_per_user_override() {
        let dir = tempfile::tempdir().unwrap();
        let memory = Memory::open(dir.path()).unwrap();

        // Create global identity
        fs::write(dir.path().join("identity.md"), "I am a helpful assistant").unwrap();

        // Per-user identity override for a specific JID
        let jid = "vip@localhost";
        let jid_dir = dir.path().join(jid);
        fs::create_dir_all(&jid_dir).unwrap();
        fs::write(jid_dir.join("identity.md"), "I am your personal concierge").unwrap();

        let ctx = memory.get_workspace_context(jid).unwrap();
        assert_eq!(ctx.identity.unwrap(), "I am your personal concierge");

        // Other user still gets global
        let ctx2 = memory.get_workspace_context("other@localhost").unwrap();
        assert_eq!(ctx2.identity.unwrap(), "I am a helpful assistant");
    }

    #[test]
    fn test_workspace_context_override_with_no_global() {
        let dir = tempfile::tempdir().unwrap();
        let memory = Memory::open(dir.path()).unwrap();

        // No global files at all — per-room file still works
        let room = "lab@conference.localhost";
        let room_dir = dir.path().join(room);
        fs::create_dir_all(&room_dir).unwrap();
        fs::write(room_dir.join("instructions.md"), "Lab-specific rules").unwrap();

        let ctx = memory.get_workspace_context(room).unwrap();
        assert_eq!(ctx.instructions.unwrap(), "Lab-specific rules");
        assert!(ctx.identity.is_none());
        assert!(ctx.personality.is_none());
    }

    #[test]
    fn test_room_jid_uses_same_structure() {
        let dir = tempfile::tempdir().unwrap();
        let memory = Memory::open(dir.path()).unwrap();

        let room_jid = "devroom@conference.example.com";

        memory
            .store_message(room_jid, "user", "Hello room!")
            .unwrap();
        memory
            .set_user_profile(room_jid, "Development discussion room")
            .unwrap();
        memory
            .set_user_memory(room_jid, "Active project: Fluux Agent")
            .unwrap();

        let history = memory.get_history(room_jid, 10).unwrap();
        assert_eq!(history.len(), 1);
        assert_eq!(history[0].content, "Hello room!");

        assert_eq!(
            memory.get_user_profile(room_jid).unwrap().unwrap(),
            "Development discussion room"
        );
        assert_eq!(
            memory.get_user_memory(room_jid).unwrap().unwrap(),
            "Active project: Fluux Agent"
        );
    }
}
