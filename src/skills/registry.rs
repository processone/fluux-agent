use std::collections::HashMap;

use crate::llm::ToolDefinition;

use super::Skill;

/// Registry of available skills.
///
/// Owns all registered skill instances and provides:
/// - Name-based lookup for skill execution
/// - Tool definition generation for the Anthropic API
///
/// Skills are registered at startup and never modified afterward.
/// The registry is owned by `AgentRuntime` and accessed via `&self`.
pub struct SkillRegistry {
    skills: HashMap<String, Box<dyn Skill>>,
}

impl SkillRegistry {
    /// Creates an empty registry (no skills registered).
    pub fn new() -> Self {
        Self {
            skills: HashMap::new(),
        }
    }

    /// Registers a skill. If a skill with the same name already exists,
    /// it is replaced (last-write-wins).
    pub fn register(&mut self, skill: Box<dyn Skill>) {
        let name = skill.name().to_string();
        self.skills.insert(name, skill);
    }

    /// Looks up a skill by name. Returns `None` if not found.
    pub fn get(&self, name: &str) -> Option<&dyn Skill> {
        self.skills.get(name).map(|s| s.as_ref())
    }

    /// Returns the number of registered skills.
    pub fn len(&self) -> usize {
        self.skills.len()
    }

    /// Returns true if no skills are registered.
    pub fn is_empty(&self) -> bool {
        self.skills.is_empty()
    }

    /// Builds Anthropic API tool definitions from all registered skills.
    ///
    /// Returns a `Vec<ToolDefinition>` suitable for the `tools` parameter
    /// of the Anthropic Messages API. Sorted by name for determinism.
    pub fn tool_definitions(&self) -> Vec<ToolDefinition> {
        let mut defs: Vec<ToolDefinition> = self
            .skills
            .values()
            .map(|skill| ToolDefinition {
                name: skill.name().to_string(),
                description: skill.description().to_string(),
                input_schema: skill.parameters_schema(),
            })
            .collect();
        defs.sort_by(|a, b| a.name.cmp(&b.name));
        defs
    }

    /// Returns a sorted list of all registered skill names.
    pub fn skill_names(&self) -> Vec<&str> {
        let mut names: Vec<&str> = self.skills.keys().map(|s| s.as_str()).collect();
        names.sort();
        names
    }
}

impl Default for SkillRegistry {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use serde_json::json;

    /// Test-only skill implementation for registry tests.
    struct DummySkill {
        skill_name: String,
        skill_description: String,
        schema: serde_json::Value,
    }

    impl DummySkill {
        fn new(name: &str) -> Self {
            Self {
                skill_name: name.to_string(),
                skill_description: format!("A dummy {name} skill"),
                schema: json!({
                    "type": "object",
                    "properties": {
                        "query": {"type": "string", "description": "The query"}
                    },
                    "required": ["query"]
                }),
            }
        }
    }

    #[async_trait]
    impl Skill for DummySkill {
        fn name(&self) -> &str {
            &self.skill_name
        }
        fn description(&self) -> &str {
            &self.skill_description
        }
        fn parameters_schema(&self) -> serde_json::Value {
            self.schema.clone()
        }
        async fn execute(&self, params: serde_json::Value) -> anyhow::Result<String> {
            let query = params["query"].as_str().unwrap_or("none");
            Ok(format!("result for: {query}"))
        }
    }

    #[test]
    fn test_new_registry_is_empty() {
        let registry = SkillRegistry::new();
        assert_eq!(registry.len(), 0);
        assert!(registry.is_empty());
    }

    #[test]
    fn test_register_and_get() {
        let mut registry = SkillRegistry::new();
        registry.register(Box::new(DummySkill::new("search")));

        let skill = registry.get("search");
        assert!(skill.is_some());
        assert_eq!(skill.unwrap().name(), "search");
    }

    #[test]
    fn test_get_nonexistent_returns_none() {
        let registry = SkillRegistry::new();
        assert!(registry.get("nonexistent").is_none());
    }

    #[test]
    fn test_register_replaces_duplicate() {
        let mut registry = SkillRegistry::new();
        registry.register(Box::new(DummySkill::new("search")));
        registry.register(Box::new(DummySkill::new("search")));
        assert_eq!(registry.len(), 1);
    }

    #[test]
    fn test_len_increments() {
        let mut registry = SkillRegistry::new();
        registry.register(Box::new(DummySkill::new("alpha")));
        assert_eq!(registry.len(), 1);
        assert!(!registry.is_empty());

        registry.register(Box::new(DummySkill::new("beta")));
        assert_eq!(registry.len(), 2);
    }

    #[test]
    fn test_tool_definitions_empty() {
        let registry = SkillRegistry::new();
        assert!(registry.tool_definitions().is_empty());
    }

    #[test]
    fn test_tool_definitions_correct_format() {
        let mut registry = SkillRegistry::new();
        registry.register(Box::new(DummySkill::new("search")));

        let defs = registry.tool_definitions();
        assert_eq!(defs.len(), 1);
        assert_eq!(defs[0].name, "search");
        assert_eq!(defs[0].description, "A dummy search skill");
        assert_eq!(defs[0].input_schema["type"], "object");
        assert_eq!(
            defs[0].input_schema["properties"]["query"]["type"],
            "string"
        );
    }

    #[test]
    fn test_tool_definitions_sorted_by_name() {
        let mut registry = SkillRegistry::new();
        registry.register(Box::new(DummySkill::new("zebra")));
        registry.register(Box::new(DummySkill::new("alpha")));
        registry.register(Box::new(DummySkill::new("mid")));

        let defs = registry.tool_definitions();
        let names: Vec<&str> = defs.iter().map(|d| d.name.as_str()).collect();
        assert_eq!(names, vec!["alpha", "mid", "zebra"]);
    }

    #[test]
    fn test_skill_names_sorted() {
        let mut registry = SkillRegistry::new();
        registry.register(Box::new(DummySkill::new("zebra")));
        registry.register(Box::new(DummySkill::new("alpha")));

        assert_eq!(registry.skill_names(), vec!["alpha", "zebra"]);
    }

    #[tokio::test]
    async fn test_execute_skill() {
        let mut registry = SkillRegistry::new();
        registry.register(Box::new(DummySkill::new("search")));

        let skill = registry.get("search").unwrap();
        let result = skill
            .execute(json!({"query": "hello world"}))
            .await
            .unwrap();
        assert_eq!(result, "result for: hello world");
    }

    #[test]
    fn test_default_creates_empty_registry() {
        let registry = SkillRegistry::default();
        assert!(registry.is_empty());
    }

    #[test]
    fn test_skill_names_empty() {
        let registry = SkillRegistry::new();
        let names = registry.skill_names();
        assert!(names.is_empty());
    }

    #[tokio::test]
    async fn test_execute_skill_error() {
        struct FailSkill;

        #[async_trait]
        impl Skill for FailSkill {
            fn name(&self) -> &str { "fail" }
            fn description(&self) -> &str { "Always fails" }
            fn parameters_schema(&self) -> serde_json::Value {
                json!({"type": "object", "properties": {}})
            }
            async fn execute(&self, _params: serde_json::Value) -> anyhow::Result<String> {
                Err(anyhow::anyhow!("intentional failure"))
            }
        }

        let mut registry = SkillRegistry::new();
        registry.register(Box::new(FailSkill));

        let skill = registry.get("fail").unwrap();
        let result = skill.execute(json!({})).await;
        assert!(result.is_err());
        assert_eq!(result.unwrap_err().to_string(), "intentional failure");
    }
}
