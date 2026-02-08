/// Skills registry — stub for v0.2
///
/// In v0.2, the registry will:
/// - Discover available skills (builtin + Wasm)
/// - Expose their declarative capabilities
/// - Make them invocable by the agentic runtime
///
/// For now, the agent only uses the LLM without skills.

pub struct SkillRegistry {
    // TODO v0.2: Vec<Skill>
}

impl SkillRegistry {
    pub fn new() -> Self {
        Self {}
    }
}
