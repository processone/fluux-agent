use async_trait::async_trait;
use serde_json::{json, Value};

use crate::agent::memory::Memory;
use crate::skills::{Skill, SkillContext};

/// Skill that stores knowledge entries for later recall.
///
/// The LLM calls this tool to persist facts, preferences, or context
/// about the current conversation partner. Each entry has a unique key;
/// storing with the same key replaces the previous value.
///
/// Storage is per-JID (user or room) — no cross-JID leakage.
pub struct MemoryStoreSkill;

#[async_trait]
impl Skill for MemoryStoreSkill {
    fn name(&self) -> &str {
        "memory_store"
    }

    fn description(&self) -> &str {
        "Store a piece of knowledge for later recall. Use this to remember important facts, \
         preferences, or context about the current conversation partner. Each entry has a \
         unique key — storing with the same key replaces the previous value. \
         Examples of good keys: 'preferred_language', 'project_name', 'timezone', 'tech_stack'."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "key": {
                    "type": "string",
                    "description": "A short, descriptive key for this knowledge entry \
                                    (e.g. 'preferred_language', 'project_name', 'timezone')"
                },
                "content": {
                    "type": "string",
                    "description": "The knowledge content to store"
                }
            },
            "required": ["key", "content"]
        })
    }

    fn capabilities(&self) -> Vec<String> {
        vec!["filesystem:knowledge:write".to_string()]
    }

    async fn execute(&self, params: Value, context: &SkillContext) -> anyhow::Result<String> {
        let key = params["key"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("Missing required parameter: key"))?;
        let content = params["content"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("Missing required parameter: content"))?;

        let memory = Memory::open(&context.base_path)?;
        memory.knowledge_store(&context.jid, key, content)?;

        Ok(format!("Stored knowledge entry: '{key}'"))
    }
}

/// Skill that recalls stored knowledge entries.
///
/// The LLM calls this tool to search for previously stored facts.
/// Searches by keyword/substring match across keys and content.
pub struct MemoryRecallSkill;

#[async_trait]
impl Skill for MemoryRecallSkill {
    fn name(&self) -> &str {
        "memory_recall"
    }

    fn description(&self) -> &str {
        "Recall stored knowledge about the current conversation partner. \
         Search by keyword to find relevant entries, or use an empty query \
         to list all stored knowledge. Use this when you need to remember \
         something from a past conversation — user preferences, project details, \
         technical context, or corrections to your behavior."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "query": {
                    "type": "string",
                    "description": "Search query to find matching knowledge entries. \
                                    Use an empty string to list all entries."
                }
            },
            "required": ["query"]
        })
    }

    fn capabilities(&self) -> Vec<String> {
        vec!["filesystem:knowledge:read".to_string()]
    }

    async fn execute(&self, params: Value, context: &SkillContext) -> anyhow::Result<String> {
        let query = params["query"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("Missing required parameter: query"))?;

        let memory = Memory::open(&context.base_path)?;
        let result = memory.knowledge_search(&context.jid, query)?;

        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_context(dir: &std::path::Path) -> SkillContext {
        SkillContext {
            jid: "user@example.com".to_string(),
            base_path: dir.to_path_buf(),
        }
    }

    #[test]
    fn test_memory_store_name() {
        let skill = MemoryStoreSkill;
        assert_eq!(skill.name(), "memory_store");
    }

    #[test]
    fn test_memory_recall_name() {
        let skill = MemoryRecallSkill;
        assert_eq!(skill.name(), "memory_recall");
    }

    #[test]
    fn test_memory_store_description_not_empty() {
        let skill = MemoryStoreSkill;
        assert!(!skill.description().is_empty());
    }

    #[test]
    fn test_memory_recall_description_not_empty() {
        let skill = MemoryRecallSkill;
        assert!(!skill.description().is_empty());
    }

    #[test]
    fn test_memory_store_schema_has_key_and_content() {
        let skill = MemoryStoreSkill;
        let schema = skill.parameters_schema();
        assert_eq!(schema["properties"]["key"]["type"], "string");
        assert_eq!(schema["properties"]["content"]["type"], "string");
        let required = schema["required"].as_array().unwrap();
        assert!(required.contains(&json!("key")));
        assert!(required.contains(&json!("content")));
    }

    #[test]
    fn test_memory_recall_schema_has_query() {
        let skill = MemoryRecallSkill;
        let schema = skill.parameters_schema();
        assert_eq!(schema["properties"]["query"]["type"], "string");
        let required = schema["required"].as_array().unwrap();
        assert!(required.contains(&json!("query")));
    }

    #[test]
    fn test_capabilities() {
        let store = MemoryStoreSkill;
        assert_eq!(store.capabilities(), vec!["filesystem:knowledge:write"]);

        let recall = MemoryRecallSkill;
        assert_eq!(recall.capabilities(), vec!["filesystem:knowledge:read"]);
    }

    #[tokio::test]
    async fn test_memory_store_missing_key_param() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = test_context(dir.path());
        let skill = MemoryStoreSkill;
        let result = skill.execute(json!({"content": "value"}), &ctx).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("key"));
    }

    #[tokio::test]
    async fn test_memory_store_missing_content_param() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = test_context(dir.path());
        let skill = MemoryStoreSkill;
        let result = skill.execute(json!({"key": "test"}), &ctx).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("content"));
    }

    #[tokio::test]
    async fn test_memory_recall_missing_query_param() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = test_context(dir.path());
        let skill = MemoryRecallSkill;
        let result = skill.execute(json!({}), &ctx).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("query"));
    }

    #[tokio::test]
    async fn test_memory_store_execute() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = test_context(dir.path());
        let skill = MemoryStoreSkill;

        let result = skill
            .execute(json!({"key": "lang", "content": "Rust"}), &ctx)
            .await
            .unwrap();
        assert!(result.contains("lang"));

        // Verify via file inspection
        let knowledge_path = dir.path().join("user@example.com").join("knowledge.jsonl");
        assert!(knowledge_path.exists());
        let content = std::fs::read_to_string(&knowledge_path).unwrap();
        assert!(content.contains("\"key\":\"lang\""));
        assert!(content.contains("\"content\":\"Rust\""));
    }

    #[tokio::test]
    async fn test_memory_recall_execute_no_entries() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = test_context(dir.path());
        let skill = MemoryRecallSkill;

        let result = skill
            .execute(json!({"query": "anything"}), &ctx)
            .await
            .unwrap();
        assert!(result.contains("No knowledge entries stored yet"));
    }

    #[tokio::test]
    async fn test_memory_store_and_recall_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = test_context(dir.path());

        let store = MemoryStoreSkill;
        let recall = MemoryRecallSkill;

        // Store two entries
        store
            .execute(json!({"key": "language", "content": "Prefers Rust over Go"}), &ctx)
            .await
            .unwrap();
        store
            .execute(json!({"key": "timezone", "content": "Europe/Paris"}), &ctx)
            .await
            .unwrap();

        // Recall by keyword
        let result = recall
            .execute(json!({"query": "Rust"}), &ctx)
            .await
            .unwrap();
        assert!(result.contains("language"));
        assert!(result.contains("Prefers Rust over Go"));

        // Recall all
        let result = recall
            .execute(json!({"query": ""}), &ctx)
            .await
            .unwrap();
        assert!(result.contains("language"));
        assert!(result.contains("timezone"));
        assert!(result.contains("2 knowledge entries"));
    }
}
