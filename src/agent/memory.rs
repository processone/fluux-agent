use anyhow::Result;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use tracing::info;

use crate::llm::Message;

/// Persistent conversational memory per user.
/// Stores conversation history and user context as
/// markdown files for transparency and portability.
///
/// Layout:
///   {base_path}/{jid}/history.md        — current session conversation log
///   {base_path}/{jid}/context.md        — what the agent knows about the user
///   {base_path}/{jid}/sessions/         — archived sessions
///   {base_path}/{jid}/sessions/{ts}.md  — archived session file
pub struct Memory {
    base_path: PathBuf,
}

impl Memory {
    pub fn open(path: &Path) -> Result<Self> {
        fs::create_dir_all(path)?;
        info!("Memory store opened at {}", path.display());
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

    /// Appends a message to the user's current session (history.md)
    pub fn store_message(&self, jid: &str, role: &str, content: &str) -> Result<()> {
        let path = self.user_dir(jid)?.join("history.md");
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)?;
        writeln!(file, "### {role}\n{content}\n")?;
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

    /// Erases all memory for a user (history + context).
    /// Archived sessions are preserved.
    pub fn forget(&self, jid: &str) -> Result<String> {
        let user_dir = self.base_path.join(jid);

        if !user_dir.exists() {
            return Ok("No memory to erase.".to_string());
        }

        let history_path = user_dir.join("history.md");
        let context_path = user_dir.join("context.md");

        let mut erased = Vec::new();

        if history_path.exists() {
            let content = fs::read_to_string(&history_path)?;
            let count = parse_history(&content).len();
            fs::remove_file(&history_path)?;
            erased.push(format!("{count} messages"));
        }

        if context_path.exists() {
            fs::remove_file(&context_path)?;
            erased.push("context".to_string());
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

    /// Stores or updates the user context
    /// (summary of what we know about the user)
    pub fn set_user_context(&self, jid: &str, context: &str) -> Result<()> {
        let path = self.user_dir(jid)?.join("context.md");
        fs::write(&path, context)?;
        Ok(())
    }

    /// Retrieves the user context
    pub fn get_user_context(&self, jid: &str) -> Result<Option<String>> {
        let path = self.base_path.join(jid).join("context.md");

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
/// Expected format:
/// ```markdown
/// ### user
/// Hello!
///
/// ### assistant
/// Hi there!
/// ```
fn parse_history(content: &str) -> Vec<Message> {
    let mut messages = Vec::new();
    let mut current_role: Option<String> = None;
    let mut current_content = String::new();

    for line in content.lines() {
        if let Some(role) = line.strip_prefix("### ") {
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
            current_role = Some(role.trim().to_string());
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

#[cfg(test)]
mod tests {
    use super::*;

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
    fn test_store_and_retrieve() {
        let dir = tempfile::tempdir().unwrap();
        let memory = Memory::open(dir.path()).unwrap();

        memory.store_message("user@test", "user", "Hello!").unwrap();
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
    fn test_message_count() {
        let dir = tempfile::tempdir().unwrap();
        let memory = Memory::open(dir.path()).unwrap();

        assert_eq!(memory.message_count("user@test").unwrap(), 0);

        memory.store_message("user@test", "user", "One").unwrap();
        memory
            .store_message("user@test", "assistant", "Two")
            .unwrap();
        memory.store_message("user@test", "user", "Three").unwrap();

        assert_eq!(memory.message_count("user@test").unwrap(), 3);
    }

    #[test]
    fn test_new_session_archives_history() {
        let dir = tempfile::tempdir().unwrap();
        let memory = Memory::open(dir.path()).unwrap();

        // Build a conversation
        memory.store_message("user@test", "user", "Hello!").unwrap();
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
        memory.store_message("user@test", "user", "Session 1").unwrap();
        memory.new_session("user@test").unwrap();

        // Brief pause to ensure different timestamps
        std::thread::sleep(std::time::Duration::from_millis(1100));

        // Session 2
        memory.store_message("user@test", "user", "Session 2").unwrap();
        memory.new_session("user@test").unwrap();

        assert_eq!(memory.session_count("user@test").unwrap(), 2);
    }

    #[test]
    fn test_forget_erases_active_memory() {
        let dir = tempfile::tempdir().unwrap();
        let memory = Memory::open(dir.path()).unwrap();

        memory.store_message("user@test", "user", "Hello!").unwrap();
        memory
            .set_user_context("user@test", "Likes coffee")
            .unwrap();

        let result = memory.forget("user@test").unwrap();
        assert!(result.contains("1 messages"));
        assert!(result.contains("context"));

        // History and context are gone
        assert_eq!(memory.message_count("user@test").unwrap(), 0);
        assert!(memory.get_user_context("user@test").unwrap().is_none());
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
}
