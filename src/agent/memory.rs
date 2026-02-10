use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use tracing::info;

use crate::llm::{Message, MessageContent};

/// Structured attachment metadata stored in JSONL session entries.
///
/// Represents a file transferred via XMPP HTTP Upload (XEP-0363) / OOB (XEP-0066).
/// Stored as metadata alongside the message — never embedded in content text.
#[derive(Debug, Serialize, Deserialize, PartialEq, Clone)]
pub struct Attachment {
    pub filename: String,
    pub mime_type: String,
    pub size: String,
}

/// Structured reaction metadata stored in JSONL session entries.
///
/// Represents an XEP-0444 reaction to a previous message.
/// Stored as metadata alongside the message — never embedded in content text.
#[derive(Debug, Serialize, Deserialize, PartialEq, Clone)]
pub struct Reaction {
    pub message_id: String,
    pub emojis: Vec<String>,
}

/// A single entry in a JSONL session file.
///
/// Each line in `history.jsonl` is one of these variants.
/// The `type` field is used as the JSON tag for deserialization.
#[derive(Debug, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type")]
pub enum SessionEntry {
    /// Session header — first line of every session file.
    #[serde(rename = "session")]
    Header {
        version: u32,
        created: String,
        jid: String,
    },
    /// A conversation message (user or assistant).
    #[serde(rename = "message")]
    Message {
        role: String,
        content: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        msg_id: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        sender: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        ts: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        attachments: Option<Vec<Attachment>>,
        #[serde(skip_serializing_if = "Option::is_none")]
        reaction: Option<Reaction>,
    },
}

/// A single knowledge entry in a JID's knowledge store.
///
/// Stored as one JSON object per line in `knowledge.jsonl`.
#[derive(Debug, Serialize, Deserialize, Clone)]
struct KnowledgeEntry {
    key: String,
    content: String,
    ts: String,
}

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
/// Stores conversation history as JSONL and user context as
/// markdown files for transparency and portability.
///
/// Layout:
///   {base_path}/instructions.md             — global agent behavior rules
///   {base_path}/identity.md                 — global agent identity
///   {base_path}/personality.md              — global agent personality/tone
///   {base_path}/{jid}/history.jsonl         — current session (JSONL format)
///   {base_path}/{jid}/user.md               — what the agent knows about the user
///   {base_path}/{jid}/memory.md             — long-term notes about the user
///   {base_path}/{jid}/knowledge.jsonl      — structured knowledge store (key/value)
///   {base_path}/{jid}/sessions/             — archived sessions
///   {base_path}/{jid}/sessions/{ts}.jsonl   — archived session file
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

    /// Returns the base path of the memory store.
    /// Used to construct `SkillContext` for skill execution.
    pub fn base_path(&self) -> &Path {
        &self.base_path
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

    /// Appends a message to the user's current session (history.jsonl).
    ///
    /// Convenience wrapper that uses the JID as the sender label for user messages.
    /// For assistant messages, sender is omitted.
    pub fn store_message(&self, jid: &str, role: &str, content: &str) -> Result<()> {
        let sender = if role == "user" { Some(jid) } else { None };
        self.store_message_structured(jid, role, content, None, sender)
    }

    /// Appends a message with a custom sender label.
    /// Used for MUC rooms where the sender is identified by nick, not JID.
    ///
    /// `sender_label` is stored in the `sender` field of the JSONL entry.
    /// Pass None to omit the sender (assistant messages always omit it).
    pub fn store_message_with_jid(
        &self,
        jid: &str,
        role: &str,
        content: &str,
        sender_label: Option<&str>,
    ) -> Result<()> {
        let sender = if role == "user" { sender_label } else { None };
        self.store_message_structured(jid, role, content, None, sender)
    }

    /// Appends a message with full structured metadata to the JSONL session file.
    ///
    /// This is the core storage function. All metadata (msg_id, sender, timestamp)
    /// is stored as structured JSON fields, never embedded in the content text.
    ///
    /// On first write, a session header line is prepended automatically.
    pub fn store_message_structured(
        &self,
        jid: &str,
        role: &str,
        content: &str,
        msg_id: Option<&str>,
        sender: Option<&str>,
    ) -> Result<()> {
        self.store_message_full(jid, role, content, msg_id, sender, None, None)
    }

    /// Appends a message with full structured metadata (including attachments
    /// and reactions) to the JSONL session file.
    ///
    /// This is the core storage function. All metadata (msg_id, sender,
    /// timestamp, attachments, reaction) is stored as structured JSON fields,
    /// never embedded in the content text.
    ///
    /// On first write, a session header line is prepended automatically.
    pub fn store_message_full(
        &self,
        jid: &str,
        role: &str,
        content: &str,
        msg_id: Option<&str>,
        sender: Option<&str>,
        attachments: Option<Vec<Attachment>>,
        reaction: Option<Reaction>,
    ) -> Result<()> {
        let path = self.user_dir(jid)?.join("history.jsonl");
        let is_new = !path.exists();

        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)?;

        // Write session header on first entry
        if is_new {
            let header = SessionEntry::Header {
                version: 1,
                created: chrono::Utc::now().to_rfc3339(),
                jid: jid.to_string(),
            };
            let header_json = serde_json::to_string(&header)?;
            writeln!(file, "{header_json}")?;
        }

        // Only store non-empty attachment lists
        let attachments = attachments.filter(|a| !a.is_empty());

        let entry = SessionEntry::Message {
            role: role.to_string(),
            content: content.to_string(),
            msg_id: msg_id.map(|s| s.to_string()),
            sender: sender.map(|s| s.to_string()),
            ts: Some(chrono::Utc::now().to_rfc3339()),
            attachments,
            reaction,
        };
        let json = serde_json::to_string(&entry)?;
        writeln!(file, "{json}")?;

        Ok(())
    }

    /// Retrieves the last N messages from the current session
    pub fn get_history(&self, jid: &str, limit: usize) -> Result<Vec<Message>> {
        let path = self.base_path.join(jid).join("history.jsonl");

        if !path.exists() {
            return Ok(Vec::new());
        }

        let content = fs::read_to_string(&path)?;
        let messages = parse_session(&content);

        // Return only the last `limit` messages
        let start = messages.len().saturating_sub(limit);
        Ok(messages[start..].to_vec())
    }

    /// Starts a new session for a user.
    ///
    /// Archives the current history.jsonl into sessions/{timestamp}.jsonl
    /// and clears the current history so the LLM starts fresh.
    /// Returns a human-readable summary of what happened.
    pub fn new_session(&self, jid: &str) -> Result<String> {
        let user_dir = self.user_dir(jid)?;
        let history_path = user_dir.join("history.jsonl");

        if !history_path.exists() {
            return Ok("No active session to archive.".to_string());
        }

        let content = fs::read_to_string(&history_path)?;
        let message_count = parse_session(&content).len();

        if message_count == 0 {
            return Ok("Session is already empty.".to_string());
        }

        // Archive to sessions/ directory with timestamp
        let sessions_dir = user_dir.join("sessions");
        fs::create_dir_all(&sessions_dir)?;

        let timestamp = chrono::Utc::now().format("%Y%m%d-%H%M%S");
        let archive_path = sessions_dir.join(format!("{timestamp}.jsonl"));
        fs::rename(&history_path, &archive_path)?;

        info!(
            "Archived session for {jid}: {message_count} messages → {}",
            archive_path.display()
        );

        Ok(format!(
            "Session archived ({message_count} messages). Starting fresh."
        ))
    }

    /// Checks whether the current session is still fresh, based on the file's
    /// modification time and the configured idle timeout.
    ///
    /// If the session has been idle for longer than `idle_timeout_mins`, it is
    /// automatically archived (same as `/new`) and `Ok(true)` is returned to
    /// indicate that a stale session was rotated.
    ///
    /// Returns `Ok(false)` if the session is still fresh or if there is no
    /// active session. A timeout of 0 disables the check entirely.
    pub fn check_session_freshness(&self, jid: &str, idle_timeout_mins: u64) -> Result<bool> {
        if idle_timeout_mins == 0 {
            return Ok(false);
        }

        let user_dir = self.base_path.join(jid);
        let history_path = user_dir.join("history.jsonl");

        if !history_path.exists() {
            return Ok(false);
        }

        let metadata = fs::metadata(&history_path)?;
        let modified = metadata.modified()?;
        let elapsed = modified.elapsed().unwrap_or_default();
        let timeout = std::time::Duration::from_secs(idle_timeout_mins * 60);

        if elapsed > timeout {
            info!(
                "Session for {jid} idle for {}m (timeout: {idle_timeout_mins}m) — auto-archiving",
                elapsed.as_secs() / 60
            );
            self.new_session(jid)?;
            Ok(true)
        } else {
            Ok(false)
        }
    }

    /// Erases all active memory for a user (history + user profile + memory).
    /// Archived sessions are preserved.
    pub fn forget(&self, jid: &str) -> Result<String> {
        let user_dir = self.base_path.join(jid);

        if !user_dir.exists() {
            return Ok("No memory to erase.".to_string());
        }

        let mut erased = Vec::new();

        // Erase history (JSONL and legacy markdown)
        let history_jsonl = user_dir.join("history.jsonl");
        let history_md = user_dir.join("history.md");
        if history_jsonl.exists() {
            let content = fs::read_to_string(&history_jsonl)?;
            let count = parse_session(&content).len();
            fs::remove_file(&history_jsonl)?;
            erased.push(format!("{count} messages"));
        }
        if history_md.exists() {
            // Legacy cleanup — remove old markdown history if present
            fs::remove_file(&history_md)?;
            if erased.is_empty() {
                erased.push("legacy history".to_string());
            }
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

        // Erase knowledge store
        let knowledge_jsonl = user_dir.join("knowledge.jsonl");
        if knowledge_jsonl.exists() {
            let count = self.knowledge_count(jid).unwrap_or(0);
            fs::remove_file(&knowledge_jsonl)?;
            erased.push(format!("{count} knowledge entries"));
        }

        // Erase downloaded files
        let files_dir = user_dir.join("files");
        if files_dir.exists() {
            let file_count = fs::read_dir(&files_dir)?
                .filter_map(|e| e.ok())
                .filter(|e| e.path().is_file())
                .count();
            if file_count > 0 {
                fs::remove_dir_all(&files_dir)?;
                erased.push(format!("{file_count} files"));
            }
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

    // ── File storage ──────────────────────────────────────

    /// Returns the files directory for a JID, creating it if needed.
    /// Layout: `{base_path}/{jid}/files/`
    pub fn files_dir(&self, jid: &str) -> Result<PathBuf> {
        let dir = self.base_path.join(jid).join("files");
        fs::create_dir_all(&dir)?;
        Ok(dir)
    }

    /// Number of downloaded files stored for a JID
    pub fn file_count(&self, jid: &str) -> Result<usize> {
        let dir = self.base_path.join(jid).join("files");
        if !dir.exists() {
            return Ok(0);
        }
        let count = fs::read_dir(&dir)?
            .filter_map(|e| e.ok())
            .filter(|e| e.path().is_file())
            .count();
        Ok(count)
    }

    /// Total number of messages in the current session for a JID
    pub fn message_count(&self, jid: &str) -> Result<usize> {
        let path = self.base_path.join(jid).join("history.jsonl");

        if !path.exists() {
            return Ok(0);
        }

        let content = fs::read_to_string(&path)?;
        Ok(parse_session(&content).len())
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
                    .map(|ext| ext == "jsonl" || ext == "md")
                    .unwrap_or(false)
            })
            .count();

        Ok(count)
    }

    // ── Knowledge store ──────────────────────────────────

    /// Loads all knowledge entries from a JID's knowledge store.
    /// Returns an empty Vec if the file does not exist.
    fn load_knowledge(&self, jid: &str) -> Result<Vec<KnowledgeEntry>> {
        let path = self.base_path.join(jid).join("knowledge.jsonl");
        if !path.exists() {
            return Ok(Vec::new());
        }
        let content = fs::read_to_string(&path)?;
        let mut entries = Vec::new();
        for line in content.lines() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            if let Ok(entry) = serde_json::from_str::<KnowledgeEntry>(line) {
                entries.push(entry);
            }
        }
        Ok(entries)
    }

    /// Stores or updates a knowledge entry for a JID.
    ///
    /// If `key` already exists, the file is rewritten with the updated value.
    /// If `key` is new, a line is appended.
    /// File: `{base_path}/{jid}/knowledge.jsonl`
    pub fn knowledge_store(&self, jid: &str, key: &str, content: &str) -> Result<()> {
        let dir = self.user_dir(jid)?;
        let path = dir.join("knowledge.jsonl");
        let ts = chrono::Utc::now().to_rfc3339();

        let new_entry = KnowledgeEntry {
            key: key.to_string(),
            content: content.to_string(),
            ts,
        };

        let mut entries = self.load_knowledge(jid)?;

        // Check if key already exists
        if let Some(pos) = entries.iter().position(|e| e.key == key) {
            // Overwrite: replace entry and rewrite file
            entries[pos] = new_entry;
            let mut file = fs::File::create(&path)?;
            for entry in &entries {
                let json = serde_json::to_string(entry)?;
                writeln!(file, "{json}")?;
            }
        } else {
            // New key: append
            let mut file = OpenOptions::new()
                .create(true)
                .append(true)
                .open(&path)?;
            let json = serde_json::to_string(&new_entry)?;
            writeln!(file, "{json}")?;
        }

        Ok(())
    }

    /// Retrieves a specific knowledge entry by exact key match.
    /// Returns `None` if the key does not exist.
    pub fn knowledge_get(&self, jid: &str, key: &str) -> Result<Option<String>> {
        let entries = self.load_knowledge(jid)?;
        Ok(entries.into_iter().find(|e| e.key == key).map(|e| e.content))
    }

    /// Searches knowledge entries by keyword/substring match across keys and content.
    ///
    /// If `query` is empty, returns all entries.
    /// Returns a formatted string with numbered results.
    pub fn knowledge_search(&self, jid: &str, query: &str) -> Result<String> {
        let entries = self.load_knowledge(jid)?;

        if entries.is_empty() {
            return Ok("No knowledge entries stored yet.".to_string());
        }

        let query_lower = query.to_lowercase();
        let matches: Vec<&KnowledgeEntry> = if query.is_empty() {
            entries.iter().collect()
        } else {
            entries
                .iter()
                .filter(|e| {
                    e.key.to_lowercase().contains(&query_lower)
                        || e.content.to_lowercase().contains(&query_lower)
                })
                .collect()
        };

        if matches.is_empty() {
            return Ok(format!("No knowledge entries found matching: {query}"));
        }

        let mut result = format!("Found {} knowledge entries:\n", matches.len());
        for (i, entry) in matches.iter().enumerate() {
            result.push_str(&format!("{}. [{}] {}\n", i + 1, entry.key, entry.content));
        }
        Ok(result)
    }

    /// Returns the number of knowledge entries for a JID.
    pub fn knowledge_count(&self, jid: &str) -> Result<usize> {
        Ok(self.load_knowledge(jid)?.len())
    }
}


// ── JSONL session parsing ─────────────────────────────

/// Builds a `Message` for the LLM from a session entry.
///
/// Only conversational context is passed to the model — runtime metadata
/// (msg_id, timestamps) is never included. The model just produces content;
/// the runtime manages all metadata (following OpenClaw's approach).
///
/// When `sender` is present (MUC rooms with multiple participants), it is
/// prepended as a natural text prefix so the model knows who is speaking:
///
///     "alice@muc: Hello everyone!"
///
/// In 1:1 chats, `sender` should be `None` (only one user, no ambiguity).
/// Reconstructs display content from structured metadata for the LLM.
///
/// Metadata (attachments, reactions) is serialized as compact JSON so the
/// LLM can interpret structured data directly. The content text is appended
/// after any metadata lines.
fn build_display_content(
    content: &str,
    attachments: &Option<Vec<Attachment>>,
    reaction: &Option<Reaction>,
) -> String {
    let mut parts: Vec<String> = Vec::new();
    if let Some(ref r) = reaction {
        // Serialize reaction metadata as JSON
        if let Ok(json) = serde_json::to_string(r) {
            parts.push(json);
        }
    }
    if let Some(ref atts) = attachments {
        // Serialize each attachment as JSON
        for att in atts {
            if let Ok(json) = serde_json::to_string(att) {
                parts.push(json);
            }
        }
    }
    if !content.is_empty() {
        parts.push(content.to_string());
    }
    parts.join("\n")
}

pub(crate) fn build_message_for_llm(
    role: String,
    content: String,
    sender: Option<&str>,
) -> Message {
    let text = if let Some(s) = sender {
        format!("{s}: {content}")
    } else {
        content
    };
    Message {
        role,
        content: MessageContent::Text(text),
    }
}

/// Parses a JSONL session file into a list of messages for the LLM API.
///
/// Each line is a `SessionEntry`. Message entries are converted to plain text
/// `Message` structs. Runtime metadata (msg_id, timestamps) is stripped — only
/// MUC sender labels (`@muc` suffix) are preserved as text prefixes.
///
/// Header entries are skipped. Invalid lines are silently ignored.
fn parse_session(content: &str) -> Vec<Message> {
    let mut messages = Vec::new();

    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }

        let entry: SessionEntry = match serde_json::from_str(line) {
            Ok(e) => e,
            Err(_) => continue, // skip malformed lines
        };

        match entry {
            SessionEntry::Header { .. } => {} // skip header
            SessionEntry::Message {
                role,
                content,
                sender,
                attachments,
                reaction,
                ..
            } => {
                // Reconstruct display text from structured metadata + content
                let display = build_display_content(&content, &attachments, &reaction);
                if display.is_empty() {
                    continue;
                }
                // Only pass MUC sender labels to the LLM (for participant attribution).
                // 1:1 senders are redundant — only one user in the conversation.
                // MUC senders are identified by the "@muc" suffix convention.
                let muc_sender = sender
                    .as_deref()
                    .filter(|s| s.ends_with("@muc"));
                messages.push(build_message_for_llm(
                    role,
                    display,
                    muc_sender,
                ));
            }
        }
    }

    messages
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Test helper: extracts the text content from a message.
    /// All messages are now plain `MessageContent::Text`.
    fn text(content: &MessageContent) -> &str {
        match content {
            MessageContent::Text(s) => s.as_str(),
            MessageContent::Blocks(_) => panic!("Expected Text, got Blocks"),
        }
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
        // 1:1 sender is not prefixed (not MUC)
        assert_eq!(text(&history[0].content), "Hello!");
        assert_eq!(history[1].role, "assistant");
        assert_eq!(text(&history[1].content), "Hi there!");
    }

    #[test]
    fn test_store_message_jsonl_format() {
        let dir = tempfile::tempdir().unwrap();
        let memory = Memory::open(dir.path()).unwrap();

        memory
            .store_message("alice@example.com", "user", "Hello!")
            .unwrap();
        memory
            .store_message("alice@example.com", "assistant", "Hi!")
            .unwrap();

        // Read raw JSONL file to verify format
        let raw = fs::read_to_string(dir.path().join("alice@example.com/history.jsonl")).unwrap();
        let lines: Vec<&str> = raw.lines().collect();
        assert_eq!(lines.len(), 3); // header + 2 messages

        // First line is session header
        assert!(lines[0].contains("\"type\":\"session\""));
        assert!(lines[0].contains("\"version\":1"));
        assert!(lines[0].contains("alice@example.com"));

        // Second line is user message with sender
        assert!(lines[1].contains("\"role\":\"user\""));
        assert!(lines[1].contains("\"sender\":\"alice@example.com\""));
        assert!(lines[1].contains("\"content\":\"Hello!\""));

        // Third line is assistant message (no sender)
        assert!(lines[2].contains("\"role\":\"assistant\""));
        assert!(!lines[2].contains("\"sender\""));

        // Parsed history returns correct messages
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
        // 1:1 sender not prefixed
        assert_eq!(text(&history[0].content), "Message 7");
        assert_eq!(text(&history[2].content), "Message 9");
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
        assert_eq!(text(&history[0].content), "Fresh start!");
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
        assert_eq!(text(&alice_history[0].content), "Alice's message");

        let bob_history = memory.get_history("bob@test", 10).unwrap();
        assert_eq!(bob_history.len(), 1);
        assert_eq!(text(&bob_history[0].content), "Bob's message");

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

        // Verify raw JSONL format
        let raw = fs::read_to_string(dir.path().join(room_jid).join("history.jsonl")).unwrap();
        assert!(raw.contains("\"sender\":\"alice@muc\""));
        assert!(raw.contains("\"role\":\"assistant\""));

        // Verify parsed history includes MUC sender prefix
        let history = memory.get_history(room_jid, 10).unwrap();
        assert_eq!(history.len(), 2);
        assert_eq!(history[0].role, "user");
        assert_eq!(text(&history[0].content), "alice@muc: Hello everyone!");
        assert_eq!(history[1].role, "assistant");
        assert_eq!(text(&history[1].content), "Hi Alice!");
    }

    #[test]
    fn test_store_message_with_jid_none_label() {
        let dir = tempfile::tempdir().unwrap();
        let memory = Memory::open(dir.path()).unwrap();

        memory
            .store_message_with_jid("test@localhost", "user", "No label", None)
            .unwrap();

        let raw = fs::read_to_string(dir.path().join("test@localhost/history.jsonl")).unwrap();
        // User message with None sender should not have sender field
        assert!(!raw.contains("\"sender\""));

        // Parsed history should return plain Text content (no meta block)
        let history = memory.get_history("test@localhost", 10).unwrap();
        assert_eq!(text(&history[0].content), "No label");
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

    // ── File storage tests ─────────────────────────────────

    #[test]
    fn test_files_dir_creates_directory() {
        let dir = tempfile::tempdir().unwrap();
        let memory = Memory::open(dir.path()).unwrap();

        let files_dir = memory.files_dir("user@test").unwrap();
        assert!(files_dir.exists());
        assert!(files_dir.ends_with("user@test/files"));
    }

    #[test]
    fn test_file_count_empty() {
        let dir = tempfile::tempdir().unwrap();
        let memory = Memory::open(dir.path()).unwrap();

        assert_eq!(memory.file_count("user@test").unwrap(), 0);
    }

    #[test]
    fn test_file_count_with_files() {
        let dir = tempfile::tempdir().unwrap();
        let memory = Memory::open(dir.path()).unwrap();

        let files_dir = memory.files_dir("user@test").unwrap();
        fs::write(files_dir.join("abc_photo.jpg"), b"fake image").unwrap();
        fs::write(files_dir.join("def_doc.pdf"), b"fake pdf").unwrap();

        assert_eq!(memory.file_count("user@test").unwrap(), 2);
    }

    #[test]
    fn test_forget_erases_files() {
        let dir = tempfile::tempdir().unwrap();
        let memory = Memory::open(dir.path()).unwrap();

        // Store some messages and files
        memory.store_message("user@test", "user", "Hello!").unwrap();
        let files_dir = memory.files_dir("user@test").unwrap();
        fs::write(files_dir.join("abc_photo.jpg"), b"fake image").unwrap();
        fs::write(files_dir.join("def_doc.pdf"), b"fake pdf").unwrap();

        let result = memory.forget("user@test").unwrap();
        assert!(result.contains("2 files"));

        // Files directory should be gone
        assert_eq!(memory.file_count("user@test").unwrap(), 0);
    }

    #[test]
    fn test_room_jid_uses_same_structure() {
        let dir = tempfile::tempdir().unwrap();
        let memory = Memory::open(dir.path()).unwrap();

        let room_jid = "devroom@conference.example.com";

        // store_message uses jid as sender for user messages
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
        // store_message uses room JID as sender, but it's not a MUC nick
        // (no @muc suffix), so no prefix is added for the LLM
        assert_eq!(text(&history[0].content), "Hello room!");

        assert_eq!(
            memory.get_user_profile(room_jid).unwrap().unwrap(),
            "Development discussion room"
        );
        assert_eq!(
            memory.get_user_memory(room_jid).unwrap().unwrap(),
            "Active project: Fluux Agent"
        );
    }

    // ── JSONL session parsing tests ───────────────────────

    #[test]
    fn test_parse_session_basic() {
        let content = r#"{"type":"session","version":1,"created":"2025-02-08T19:00:00Z","jid":"alice@example.com"}
{"type":"message","role":"user","content":"Hello!","sender":"alice@example.com","ts":"2025-02-08T19:00:01Z"}
{"type":"message","role":"assistant","content":"Hi there!","ts":"2025-02-08T19:00:02Z"}"#;

        let messages = parse_session(content);
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0].role, "user");
        // 1:1 sender not prefixed (not @muc)
        assert_eq!(text(&messages[0].content), "Hello!");
        assert_eq!(messages[1].role, "assistant");
        assert_eq!(text(&messages[1].content), "Hi there!");
    }

    #[test]
    fn test_parse_session_with_msg_id() {
        let content = r#"{"type":"session","version":1,"created":"2025-02-08T19:00:00Z","jid":"alice@example.com"}
{"type":"message","role":"user","content":"Hello!","msg_id":"abc-123","sender":"alice@example.com"}
{"type":"message","role":"assistant","content":"Hi there!","msg_id":"def-456"}"#;

        let messages = parse_session(content);
        assert_eq!(messages.len(), 2);
        // msg_id is runtime metadata — not visible to LLM
        // 1:1 sender not prefixed
        assert_eq!(text(&messages[0].content), "Hello!");
        assert_eq!(text(&messages[1].content), "Hi there!");
    }

    #[test]
    fn test_parse_session_reaction() {
        let content = r#"{"type":"message","role":"user","content":"Hello!","msg_id":"abc-123","sender":"alice@example.com"}
{"type":"message","role":"assistant","content":"Hi there!","msg_id":"def-456"}
{"type":"message","role":"user","content":"[Reacted to msg_id: def-456 with 👍]","sender":"alice@example.com"}"#;

        let messages = parse_session(content);
        assert_eq!(messages.len(), 3);
        // Reaction text contains the target msg_id naturally
        assert_eq!(
            text(&messages[2].content),
            "[Reacted to msg_id: def-456 with 👍]"
        );
    }

    #[test]
    fn test_parse_session_muc_multiple_senders() {
        let content = r#"{"type":"session","version":1,"created":"2025-02-08T19:00:00Z","jid":"room@conference.localhost"}
{"type":"message","role":"user","content":"Anyone around?","sender":"alice@muc"}
{"type":"message","role":"user","content":"I'm here","sender":"bob@muc"}
{"type":"message","role":"assistant","content":"Hi everyone!"}"#;

        let messages = parse_session(content);
        assert_eq!(messages.len(), 3);
        // MUC senders get text prefix for participant attribution
        assert_eq!(text(&messages[0].content), "alice@muc: Anyone around?");
        assert_eq!(text(&messages[1].content), "bob@muc: I'm here");
        assert_eq!(text(&messages[2].content), "Hi everyone!");
    }

    #[test]
    fn test_parse_session_empty_content_skipped() {
        let content = r#"{"type":"message","role":"user","content":"Hello!"}
{"type":"message","role":"user","content":""}
{"type":"message","role":"assistant","content":"Hi!"}"#;

        let messages = parse_session(content);
        assert_eq!(messages.len(), 2);
        assert_eq!(text(&messages[0].content), "Hello!");
        assert_eq!(text(&messages[1].content), "Hi!");
    }

    #[test]
    fn test_parse_session_empty_input() {
        let messages = parse_session("");
        assert!(messages.is_empty());
    }

    #[test]
    fn test_parse_session_malformed_lines_ignored() {
        let content = r#"{"type":"message","role":"user","content":"Hello!"}
not valid json
{"type":"message","role":"assistant","content":"Hi!"}"#;

        let messages = parse_session(content);
        assert_eq!(messages.len(), 2);
    }

    #[test]
    fn test_parse_session_no_optional_fields() {
        let content =
            r#"{"type":"message","role":"user","content":"Hello!"}"#;

        let messages = parse_session(content);
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].role, "user");
        assert_eq!(text(&messages[0].content), "Hello!");
    }

    #[test]
    fn test_session_entry_serialization_roundtrip() {
        let entry = SessionEntry::Message {
            role: "user".to_string(),
            content: "Hello!".to_string(),
            msg_id: Some("abc-123".to_string()),
            sender: Some("alice@example.com".to_string()),
            ts: Some("2025-02-08T19:00:00Z".to_string()),
            attachments: None,
            reaction: None,
        };

        let json = serde_json::to_string(&entry).unwrap();
        let parsed: SessionEntry = serde_json::from_str(&json).unwrap();
        assert_eq!(entry, parsed);
    }

    #[test]
    fn test_session_entry_optional_fields_omitted() {
        let entry = SessionEntry::Message {
            role: "assistant".to_string(),
            content: "Hi!".to_string(),
            msg_id: None,
            sender: None,
            ts: None,
            attachments: None,
            reaction: None,
        };

        let json = serde_json::to_string(&entry).unwrap();
        assert!(!json.contains("msg_id"));
        assert!(!json.contains("sender"));
        assert!(!json.contains("ts"));
        assert!(!json.contains("attachments"));
    }

    #[test]
    fn test_session_header_serialization() {
        let entry = SessionEntry::Header {
            version: 1,
            created: "2025-02-08T19:00:00Z".to_string(),
            jid: "alice@example.com".to_string(),
        };

        let json = serde_json::to_string(&entry).unwrap();
        assert!(json.contains("\"type\":\"session\""));
        assert!(json.contains("\"version\":1"));

        let parsed: SessionEntry = serde_json::from_str(&json).unwrap();
        assert_eq!(entry, parsed);
    }

    // ── Attachment metadata tests ──────────────────────────

    #[test]
    fn test_session_entry_with_attachments_roundtrip() {
        let entry = SessionEntry::Message {
            role: "user".to_string(),
            content: "Check this out".to_string(),
            msg_id: Some("msg-1".to_string()),
            sender: Some("alice@example.com".to_string()),
            ts: Some("2025-02-08T19:00:00Z".to_string()),
            attachments: Some(vec![Attachment {
                filename: "photo.png".to_string(),
                mime_type: "image/png".to_string(),
                size: "926KB".to_string(),
            }]),
            reaction: None,
        };

        let json = serde_json::to_string(&entry).unwrap();
        assert!(json.contains("\"attachments\""));
        assert!(json.contains("photo.png"));
        assert!(json.contains("image/png"));
        assert!(json.contains("926KB"));

        let parsed: SessionEntry = serde_json::from_str(&json).unwrap();
        assert_eq!(entry, parsed);
    }

    #[test]
    fn test_session_entry_with_reaction_roundtrip() {
        let entry = SessionEntry::Message {
            role: "user".to_string(),
            content: String::new(),
            msg_id: None,
            sender: Some("alice@example.com".to_string()),
            ts: Some("2025-02-08T19:00:00Z".to_string()),
            attachments: None,
            reaction: Some(Reaction {
                message_id: "msg-001".to_string(),
                emojis: vec!["👍".to_string(), "🎉".to_string()],
            }),
        };

        let json = serde_json::to_string(&entry).unwrap();
        assert!(json.contains("\"reaction\""));
        assert!(json.contains("msg-001"));
        assert!(!json.contains("\"attachments\""));

        let parsed: SessionEntry = serde_json::from_str(&json).unwrap();
        assert_eq!(entry, parsed);
    }

    #[test]
    fn test_session_entry_without_attachments_backward_compat() {
        // Old JSONL entries without "attachments" or "reaction" fields should parse fine
        let json = r#"{"type":"message","role":"user","content":"Hello!","msg_id":"abc","sender":"alice@example.com","ts":"2025-02-08T19:00:00Z"}"#;
        let entry: SessionEntry = serde_json::from_str(json).unwrap();
        match entry {
            SessionEntry::Message { attachments, reaction, content, .. } => {
                assert!(attachments.is_none());
                assert!(reaction.is_none());
                assert_eq!(content, "Hello!");
            }
            _ => panic!("Expected Message"),
        }
    }

    #[test]
    fn test_store_message_full_with_attachments() {
        let dir = tempfile::tempdir().unwrap();
        let memory = Memory::open(dir.path()).unwrap();

        let atts = vec![
            Attachment {
                filename: "photo.png".to_string(),
                mime_type: "image/png".to_string(),
                size: "926KB".to_string(),
            },
            Attachment {
                filename: "doc.pdf".to_string(),
                mime_type: "application/pdf".to_string(),
                size: "1.2MB".to_string(),
            },
        ];

        memory
            .store_message_full(
                "user@test",
                "user",
                "Check these files",
                Some("msg-1"),
                Some("user@test"),
                Some(atts),
                None,
            )
            .unwrap();

        // Verify raw JSONL
        let raw = fs::read_to_string(dir.path().join("user@test/history.jsonl")).unwrap();
        let lines: Vec<&str> = raw.lines().collect();
        assert_eq!(lines.len(), 2); // header + message
        assert!(lines[1].contains("\"attachments\""));
        assert!(lines[1].contains("photo.png"));
        assert!(lines[1].contains("doc.pdf"));
        // Content should be clean text, no attachment labels
        assert!(!lines[1].contains("[Attached:"));
    }

    #[test]
    fn test_get_history_reconstructs_attachment_labels() {
        let dir = tempfile::tempdir().unwrap();
        let memory = Memory::open(dir.path()).unwrap();

        let atts = vec![Attachment {
            filename: "photo.png".to_string(),
            mime_type: "image/png".to_string(),
            size: "926KB".to_string(),
        }];

        memory
            .store_message_full(
                "user@test",
                "user",
                "What is this?",
                None,
                Some("user@test"),
                Some(atts),
                None,
            )
            .unwrap();

        let history = memory.get_history("user@test", 10).unwrap();
        assert_eq!(history.len(), 1);
        // LLM sees attachment as JSON metadata + text content
        let msg_text = text(&history[0].content);
        assert!(msg_text.contains(r#""filename":"photo.png""#));
        assert!(msg_text.contains(r#""mime_type":"image/png""#));
        assert!(msg_text.contains("What is this?"));
    }

    #[test]
    fn test_get_history_attachment_only_no_body() {
        let dir = tempfile::tempdir().unwrap();
        let memory = Memory::open(dir.path()).unwrap();

        let atts = vec![Attachment {
            filename: "photo.png".to_string(),
            mime_type: "image/png".to_string(),
            size: "500KB".to_string(),
        }];

        memory
            .store_message_full("user@test", "user", "", None, Some("user@test"), Some(atts), None)
            .unwrap();

        let history = memory.get_history("user@test", 10).unwrap();
        assert_eq!(history.len(), 1);
        let msg_text = text(&history[0].content);
        assert!(msg_text.contains(r#""filename":"photo.png""#));
        assert!(msg_text.contains(r#""size":"500KB""#));
    }

    #[test]
    fn test_build_display_content_no_metadata() {
        let result = build_display_content("Hello!", &None, &None);
        assert_eq!(result, "Hello!");
    }

    #[test]
    fn test_build_display_content_with_attachments_and_text() {
        let atts = Some(vec![
            Attachment {
                filename: "a.jpg".to_string(),
                mime_type: "image/jpeg".to_string(),
                size: "200KB".to_string(),
            },
            Attachment {
                filename: "b.pdf".to_string(),
                mime_type: "application/pdf".to_string(),
                size: "1MB".to_string(),
            },
        ]);
        let result = build_display_content("Check these", &atts, &None);
        // Attachments serialized as JSON, then text content
        assert!(result.contains(r#""filename":"a.jpg""#));
        assert!(result.contains(r#""filename":"b.pdf""#));
        assert!(result.ends_with("Check these"));
    }

    #[test]
    fn test_build_display_content_reaction() {
        let reaction = Some(Reaction {
            message_id: "msg-001".to_string(),
            emojis: vec!["👍".to_string()],
        });
        let result = build_display_content("", &None, &reaction);
        assert!(result.contains(r#""message_id":"msg-001""#));
        assert!(result.contains(r#""emojis":["👍"]"#));
    }

    #[test]
    fn test_build_display_content_empty() {
        let result = build_display_content("", &None, &None);
        assert_eq!(result, "");
    }

    // ── store_message_structured integration tests ────────

    #[test]
    fn test_store_message_structured_with_msg_id() {
        let dir = tempfile::tempdir().unwrap();
        let memory = Memory::open(dir.path()).unwrap();

        memory
            .store_message_structured(
                "user@test",
                "user",
                "Hello!",
                Some("abc-123"),
                Some("user@test"),
            )
            .unwrap();
        memory
            .store_message_structured(
                "user@test",
                "assistant",
                "Hi there!",
                Some("def-456"),
                None,
            )
            .unwrap();

        let history = memory.get_history("user@test", 10).unwrap();
        assert_eq!(history.len(), 2);
        // msg_id not visible to LLM; 1:1 sender not prefixed
        assert_eq!(text(&history[0].content), "Hello!");
        assert_eq!(text(&history[1].content), "Hi there!");
    }

    #[test]
    fn test_store_message_structured_creates_session_header() {
        let dir = tempfile::tempdir().unwrap();
        let memory = Memory::open(dir.path()).unwrap();

        memory
            .store_message_structured("alice@test", "user", "Hi", None, Some("alice@test"))
            .unwrap();

        // Verify JSONL file has session header
        let raw = fs::read_to_string(dir.path().join("alice@test/history.jsonl")).unwrap();
        let first_line = raw.lines().next().unwrap();
        let header: SessionEntry = serde_json::from_str(first_line).unwrap();
        match header {
            SessionEntry::Header { version, jid, .. } => {
                assert_eq!(version, 1);
                assert_eq!(jid, "alice@test");
            }
            _ => panic!("Expected session header as first line"),
        }
    }

    #[test]
    fn test_store_message_structured_no_duplicate_header() {
        let dir = tempfile::tempdir().unwrap();
        let memory = Memory::open(dir.path()).unwrap();

        // Store two messages — header should appear only once
        memory
            .store_message_structured("user@test", "user", "One", None, None)
            .unwrap();
        memory
            .store_message_structured("user@test", "user", "Two", None, None)
            .unwrap();

        let raw = fs::read_to_string(dir.path().join("user@test/history.jsonl")).unwrap();
        let header_count = raw
            .lines()
            .filter(|l| l.contains("\"type\":\"session\""))
            .count();
        assert_eq!(header_count, 1);
    }

    #[test]
    fn test_store_message_structured_reaction_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let memory = Memory::open(dir.path()).unwrap();

        // Store original message with msg_id
        memory
            .store_message_structured(
                "user@test",
                "user",
                "Hello!",
                Some("msg-001"),
                Some("alice@test"),
            )
            .unwrap();

        // Store reaction as structured metadata (empty content)
        let reaction = Reaction {
            message_id: "msg-001".to_string(),
            emojis: vec!["👍".to_string()],
        };
        memory
            .store_message_full(
                "user@test",
                "user",
                "",
                None,
                Some("alice@test"),
                None,
                Some(reaction),
            )
            .unwrap();

        let history = memory.get_history("user@test", 10).unwrap();
        assert_eq!(history.len(), 2);
        assert_eq!(text(&history[0].content), "Hello!");
        // Reaction reconstructed from structured metadata as JSON
        let reaction_text = text(&history[1].content);
        assert!(reaction_text.contains(r#""message_id":"msg-001""#));
        assert!(reaction_text.contains(r#""emojis":["👍"]"#));
    }

    // ── build_message_for_llm unit tests ─────────────────

    #[test]
    fn test_build_message_for_llm_no_sender() {
        let msg = build_message_for_llm("user".to_string(), "Hello!".to_string(), None);
        assert_eq!(msg.role, "user");
        assert_eq!(text(&msg.content), "Hello!");
    }

    #[test]
    fn test_build_message_for_llm_with_muc_sender() {
        let msg = build_message_for_llm(
            "user".to_string(),
            "Hello!".to_string(),
            Some("alice@muc"),
        );
        assert_eq!(msg.role, "user");
        assert_eq!(text(&msg.content), "alice@muc: Hello!");
    }

    #[test]
    fn test_build_message_for_llm_assistant_no_sender() {
        let msg = build_message_for_llm("assistant".to_string(), "Hi there!".to_string(), None);
        assert_eq!(msg.role, "assistant");
        assert_eq!(text(&msg.content), "Hi there!");
    }

    #[test]
    fn test_build_message_for_llm_empty_content_with_sender() {
        let msg = build_message_for_llm("user".to_string(), "".to_string(), Some("nick@muc"));
        assert_eq!(text(&msg.content), "nick@muc: ");
    }

    #[test]
    fn test_build_message_for_llm_content_with_colon() {
        // Content that itself contains "sender: " pattern should not be confused
        let msg = build_message_for_llm(
            "user".to_string(),
            "alice@muc: says hello".to_string(),
            Some("bob@muc"),
        );
        assert_eq!(text(&msg.content), "bob@muc: alice@muc: says hello");
    }

    // ── parse_session sender filtering tests ─────────────

    #[test]
    fn test_parse_session_muc_sender_prefixed() {
        let content = r#"{"type":"message","role":"user","content":"Hey","sender":"nick@muc"}"#;
        let messages = parse_session(content);
        assert_eq!(text(&messages[0].content), "nick@muc: Hey");
    }

    #[test]
    fn test_parse_session_non_muc_sender_not_prefixed() {
        let content =
            r#"{"type":"message","role":"user","content":"Hey","sender":"alice@example.com"}"#;
        let messages = parse_session(content);
        assert_eq!(text(&messages[0].content), "Hey");
    }

    #[test]
    fn test_parse_session_sender_containing_muc_but_not_ending() {
        // "alice@muc.server" should NOT be treated as a MUC sender
        let content =
            r#"{"type":"message","role":"user","content":"Hey","sender":"alice@muc.server"}"#;
        let messages = parse_session(content);
        assert_eq!(text(&messages[0].content), "Hey");
    }

    #[test]
    fn test_parse_session_no_sender_field() {
        let content = r#"{"type":"message","role":"user","content":"Hey"}"#;
        let messages = parse_session(content);
        assert_eq!(text(&messages[0].content), "Hey");
    }

    #[test]
    fn test_parse_session_assistant_with_msg_id_no_prefix() {
        // Assistant messages never get sender prefix
        let content = r#"{"type":"message","role":"assistant","content":"Reply","msg_id":"out-001"}"#;
        let messages = parse_session(content);
        assert_eq!(text(&messages[0].content), "Reply");
    }

    #[test]
    fn test_parse_session_whitespace_lines_skipped() {
        let content = "  \n\t\n{\"type\":\"message\",\"role\":\"user\",\"content\":\"Hey\"}\n  \n";
        let messages = parse_session(content);
        assert_eq!(messages.len(), 1);
        assert_eq!(text(&messages[0].content), "Hey");
    }

    #[test]
    fn test_parse_session_mixed_muc_and_non_muc() {
        // Realistic MUC session: participants have @muc, assistant does not
        let content = r#"{"type":"session","version":1,"created":"2025-02-08T19:00:00Z","jid":"room@conference.localhost"}
{"type":"message","role":"user","content":"Question?","sender":"alice@muc"}
{"type":"message","role":"assistant","content":"Answer!","msg_id":"out-001"}
{"type":"message","role":"user","content":"Thanks","sender":"bob@muc"}
{"type":"message","role":"user","content":"[Reacted to msg_id: out-001 with 👍]","sender":"alice@muc"}"#;

        let messages = parse_session(content);
        assert_eq!(messages.len(), 4);
        assert_eq!(text(&messages[0].content), "alice@muc: Question?");
        assert_eq!(text(&messages[1].content), "Answer!");
        assert_eq!(text(&messages[2].content), "bob@muc: Thanks");
        assert_eq!(
            text(&messages[3].content),
            "alice@muc: [Reacted to msg_id: out-001 with 👍]"
        );
    }

    // ── Knowledge store tests ──────────────────────────────

    #[test]
    fn test_knowledge_store_and_get() {
        let dir = tempfile::tempdir().unwrap();
        let memory = Memory::open(dir.path()).unwrap();
        let jid = "alice@example.com";

        memory.knowledge_store(jid, "language", "Rust").unwrap();
        let result = memory.knowledge_get(jid, "language").unwrap();
        assert_eq!(result, Some("Rust".to_string()));
    }

    #[test]
    fn test_knowledge_store_overwrites() {
        let dir = tempfile::tempdir().unwrap();
        let memory = Memory::open(dir.path()).unwrap();
        let jid = "alice@example.com";

        memory.knowledge_store(jid, "language", "Python").unwrap();
        memory.knowledge_store(jid, "language", "Rust").unwrap();

        let result = memory.knowledge_get(jid, "language").unwrap();
        assert_eq!(result, Some("Rust".to_string()));

        // File should have only one entry (not two)
        assert_eq!(memory.knowledge_count(jid).unwrap(), 1);
    }

    #[test]
    fn test_knowledge_get_nonexistent() {
        let dir = tempfile::tempdir().unwrap();
        let memory = Memory::open(dir.path()).unwrap();
        let jid = "alice@example.com";

        let result = memory.knowledge_get(jid, "missing").unwrap();
        assert_eq!(result, None);
    }

    #[test]
    fn test_knowledge_search_by_keyword() {
        let dir = tempfile::tempdir().unwrap();
        let memory = Memory::open(dir.path()).unwrap();
        let jid = "alice@example.com";

        memory.knowledge_store(jid, "language", "Prefers Rust over Go").unwrap();
        memory.knowledge_store(jid, "timezone", "Europe/Paris").unwrap();
        memory.knowledge_store(jid, "project", "Building a web server").unwrap();

        let result = memory.knowledge_search(jid, "Rust").unwrap();
        assert!(result.contains("language"));
        assert!(result.contains("Prefers Rust over Go"));
        assert!(!result.contains("timezone"));
    }

    #[test]
    fn test_knowledge_search_case_insensitive() {
        let dir = tempfile::tempdir().unwrap();
        let memory = Memory::open(dir.path()).unwrap();
        let jid = "alice@example.com";

        memory.knowledge_store(jid, "language", "Prefers Rust").unwrap();

        let result = memory.knowledge_search(jid, "rust").unwrap();
        assert!(result.contains("language"));
        assert!(result.contains("Prefers Rust"));

        let result = memory.knowledge_search(jid, "RUST").unwrap();
        assert!(result.contains("language"));
    }

    #[test]
    fn test_knowledge_search_empty_query_lists_all() {
        let dir = tempfile::tempdir().unwrap();
        let memory = Memory::open(dir.path()).unwrap();
        let jid = "alice@example.com";

        memory.knowledge_store(jid, "language", "Rust").unwrap();
        memory.knowledge_store(jid, "timezone", "UTC").unwrap();

        let result = memory.knowledge_search(jid, "").unwrap();
        assert!(result.contains("language"));
        assert!(result.contains("timezone"));
        assert!(result.contains("2 knowledge entries"));
    }

    #[test]
    fn test_knowledge_search_no_results() {
        let dir = tempfile::tempdir().unwrap();
        let memory = Memory::open(dir.path()).unwrap();
        let jid = "alice@example.com";

        memory.knowledge_store(jid, "language", "Rust").unwrap();

        let result = memory.knowledge_search(jid, "python").unwrap();
        assert!(result.contains("No knowledge entries found matching: python"));
    }

    #[test]
    fn test_knowledge_store_jsonl_format() {
        let dir = tempfile::tempdir().unwrap();
        let memory = Memory::open(dir.path()).unwrap();
        let jid = "alice@example.com";

        memory.knowledge_store(jid, "lang", "Rust").unwrap();
        memory.knowledge_store(jid, "tz", "UTC").unwrap();

        let path = dir.path().join(jid).join("knowledge.jsonl");
        let content = fs::read_to_string(&path).unwrap();
        let lines: Vec<&str> = content.lines().collect();
        assert_eq!(lines.len(), 2);

        // Each line is valid JSON with key, content, ts fields
        let entry1: serde_json::Value = serde_json::from_str(lines[0]).unwrap();
        assert_eq!(entry1["key"], "lang");
        assert_eq!(entry1["content"], "Rust");
        assert!(entry1["ts"].as_str().is_some());

        let entry2: serde_json::Value = serde_json::from_str(lines[1]).unwrap();
        assert_eq!(entry2["key"], "tz");
        assert_eq!(entry2["content"], "UTC");
    }

    #[test]
    fn test_knowledge_jid_isolation() {
        let dir = tempfile::tempdir().unwrap();
        let memory = Memory::open(dir.path()).unwrap();

        memory.knowledge_store("alice@example.com", "color", "blue").unwrap();
        memory.knowledge_store("bob@example.com", "color", "red").unwrap();

        let alice = memory.knowledge_get("alice@example.com", "color").unwrap();
        let bob = memory.knowledge_get("bob@example.com", "color").unwrap();

        assert_eq!(alice, Some("blue".to_string()));
        assert_eq!(bob, Some("red".to_string()));
    }

    #[test]
    fn test_knowledge_room_jid_works() {
        let dir = tempfile::tempdir().unwrap();
        let memory = Memory::open(dir.path()).unwrap();
        let room_jid = "room@conference.example.com";

        memory.knowledge_store(room_jid, "topic", "Rust development").unwrap();
        let result = memory.knowledge_get(room_jid, "topic").unwrap();
        assert_eq!(result, Some("Rust development".to_string()));
    }

    #[test]
    fn test_forget_erases_knowledge() {
        let dir = tempfile::tempdir().unwrap();
        let memory = Memory::open(dir.path()).unwrap();
        let jid = "alice@example.com";

        memory.knowledge_store(jid, "language", "Rust").unwrap();
        assert_eq!(memory.knowledge_count(jid).unwrap(), 1);

        let result = memory.forget(jid).unwrap();
        assert!(result.contains("knowledge"));

        assert_eq!(memory.knowledge_count(jid).unwrap(), 0);
        assert_eq!(memory.knowledge_get(jid, "language").unwrap(), None);
    }

    #[test]
    fn test_knowledge_count() {
        let dir = tempfile::tempdir().unwrap();
        let memory = Memory::open(dir.path()).unwrap();
        let jid = "alice@example.com";

        assert_eq!(memory.knowledge_count(jid).unwrap(), 0);

        memory.knowledge_store(jid, "a", "1").unwrap();
        assert_eq!(memory.knowledge_count(jid).unwrap(), 1);

        memory.knowledge_store(jid, "b", "2").unwrap();
        assert_eq!(memory.knowledge_count(jid).unwrap(), 2);

        // Overwrite doesn't increase count
        memory.knowledge_store(jid, "a", "updated").unwrap();
        assert_eq!(memory.knowledge_count(jid).unwrap(), 2);
    }

    #[test]
    fn test_knowledge_search_no_entries() {
        let dir = tempfile::tempdir().unwrap();
        let memory = Memory::open(dir.path()).unwrap();
        let jid = "alice@example.com";

        let result = memory.knowledge_search(jid, "anything").unwrap();
        assert!(result.contains("No knowledge entries stored yet"));
    }

    // ── Session freshness tests ─────────────────────────────

    #[test]
    fn test_check_freshness_disabled_when_zero() {
        let dir = tempfile::tempdir().unwrap();
        let memory = Memory::open(dir.path()).unwrap();

        memory.store_message("user@test", "user", "Hello").unwrap();

        // Timeout of 0 means disabled — never archives
        let rotated = memory.check_session_freshness("user@test", 0).unwrap();
        assert!(!rotated);

        // Session still intact
        assert_eq!(memory.message_count("user@test").unwrap(), 1);
    }

    #[test]
    fn test_check_freshness_no_session() {
        let dir = tempfile::tempdir().unwrap();
        let memory = Memory::open(dir.path()).unwrap();

        // No history file at all — should return false (no rotation)
        let rotated = memory.check_session_freshness("user@test", 60).unwrap();
        assert!(!rotated);
    }

    #[test]
    fn test_check_freshness_fresh_session() {
        let dir = tempfile::tempdir().unwrap();
        let memory = Memory::open(dir.path()).unwrap();

        memory.store_message("user@test", "user", "Hello").unwrap();

        // Just created — should be fresh with a 60-minute timeout
        let rotated = memory.check_session_freshness("user@test", 60).unwrap();
        assert!(!rotated);

        // Session still intact
        assert_eq!(memory.message_count("user@test").unwrap(), 1);
    }

    #[test]
    fn test_check_freshness_stale_session_archives() {
        use filetime::FileTime;

        let dir = tempfile::tempdir().unwrap();
        let memory = Memory::open(dir.path()).unwrap();

        memory.store_message("user@test", "user", "Hello").unwrap();
        memory.store_message("user@test", "assistant", "Hi!").unwrap();

        // Backdate the file's mtime by 2 hours
        let history_path = dir.path().join("user@test/history.jsonl");
        let two_hours_ago = FileTime::from_unix_time(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_secs() as i64
                - 7200,
            0,
        );
        filetime::set_file_mtime(&history_path, two_hours_ago).unwrap();

        // With a 60-minute timeout, the session should be stale
        let rotated = memory.check_session_freshness("user@test", 60).unwrap();
        assert!(rotated);

        // History should be empty (archived)
        assert_eq!(memory.message_count("user@test").unwrap(), 0);

        // Archived session should exist
        assert_eq!(memory.session_count("user@test").unwrap(), 1);
    }

    #[test]
    fn test_check_freshness_not_stale_within_timeout() {
        use filetime::FileTime;

        let dir = tempfile::tempdir().unwrap();
        let memory = Memory::open(dir.path()).unwrap();

        memory.store_message("user@test", "user", "Hello").unwrap();

        // Backdate by 30 minutes
        let history_path = dir.path().join("user@test/history.jsonl");
        let thirty_mins_ago = FileTime::from_unix_time(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_secs() as i64
                - 1800,
            0,
        );
        filetime::set_file_mtime(&history_path, thirty_mins_ago).unwrap();

        // With a 60-minute timeout, 30 minutes of idle is still fresh
        let rotated = memory.check_session_freshness("user@test", 60).unwrap();
        assert!(!rotated);

        // Session still intact
        assert_eq!(memory.message_count("user@test").unwrap(), 1);
    }

    #[test]
    fn test_check_freshness_new_session_works_after_archive() {
        use filetime::FileTime;

        let dir = tempfile::tempdir().unwrap();
        let memory = Memory::open(dir.path()).unwrap();

        memory.store_message("user@test", "user", "Old message").unwrap();

        // Backdate the file
        let history_path = dir.path().join("user@test/history.jsonl");
        let two_hours_ago = FileTime::from_unix_time(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_secs() as i64
                - 7200,
            0,
        );
        filetime::set_file_mtime(&history_path, two_hours_ago).unwrap();

        // Auto-archive
        memory.check_session_freshness("user@test", 60).unwrap();

        // Now store a new message — should start a fresh session
        memory.store_message("user@test", "user", "New message").unwrap();

        let history = memory.get_history("user@test", 10).unwrap();
        assert_eq!(history.len(), 1);
        assert_eq!(text(&history[0].content), "New message");

        // One archived session
        assert_eq!(memory.session_count("user@test").unwrap(), 1);
    }
}
