use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, Result};
use tokio::sync::Semaphore;
use tokio::sync::{mpsc, oneshot, OnceCell};
use tokio::time::timeout;
use tracing::warn;

use super::dead_letter::{DeadLetterRecord, DeadLetterService, DeadLetterSkillReplayPayload};
use super::observability;
use crate::config::ActorToolingConfig;
use crate::llm::ToolDefinition;
use crate::skills::{Skill, SkillContext, SkillRegistry};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkillErrorClass {
    Timeout,
    PolicyDenied,
    ExecutionError,
    UnknownTool,
}

pub struct SkillExecution {
    pub content: String,
    pub error_class: Option<SkillErrorClass>,
}

impl SkillExecution {
    fn success(content: String) -> Self {
        Self {
            content,
            error_class: None,
        }
    }

    fn error(content: String, error_class: SkillErrorClass) -> Self {
        Self {
            content,
            error_class: Some(error_class),
        }
    }
}

struct SkillActorRequest {
    params: serde_json::Value,
    context: SkillContext,
    reply_tx: oneshot::Sender<Result<String>>,
}

/// Mailbox-backed worker for one concrete skill implementation.
struct SkillActor {
    skill_name: String,
    gauge_name: String,
    dead_letter: Arc<DeadLetterService>,
    required_capabilities: Vec<String>,
    skill: Arc<dyn Skill>,
    mailbox_size: usize,
    enqueue_timeout: Duration,
    skill_timeout: Option<Duration>,
    mailbox: OnceCell<mpsc::Sender<SkillActorRequest>>,
}

impl SkillActor {
    fn new(
        skill_name: String,
        dead_letter: Arc<DeadLetterService>,
        required_capabilities: Vec<String>,
        skill: Arc<dyn Skill>,
        mailbox_size: usize,
        enqueue_timeout: Duration,
        skill_timeout: Option<Duration>,
    ) -> Self {
        let gauge_name = format!("mailbox.skill.{skill_name}.depth");
        Self {
            skill_name,
            gauge_name,
            dead_letter,
            required_capabilities,
            skill,
            mailbox_size,
            enqueue_timeout,
            skill_timeout,
            mailbox: OnceCell::new(),
        }
    }

    fn required_capabilities(&self) -> &[String] {
        &self.required_capabilities
    }

    async fn execute(&self, params: serde_json::Value, context: &SkillContext) -> Result<String> {
        let replay_payload = DeadLetterSkillReplayPayload {
            skill_name: self.skill_name.clone(),
            params: params.clone(),
            context: context.clone(),
        };
        let (reply_tx, reply_rx) = oneshot::channel();
        let request = SkillActorRequest {
            params,
            context: context.clone(),
            reply_tx,
        };

        let tx = self.mailbox_tx().await;
        match timeout(self.enqueue_timeout, tx.send(request)).await {
            Ok(Ok(())) => {
                observability::inc_counter("enqueue.skill");
                observability::add_gauge(&self.gauge_name, 1);
            }
            Ok(Err(_)) => {
                observability::inc_counter("overflow.skill.unavailable");
                self.record_dead_letter("skill_mailbox_closed", &replay_payload)
                    .await;
                return Err(anyhow!("execution_error"));
            }
            Err(_) => {
                observability::inc_counter("overflow.skill.enqueue_timeout");
                self.record_dead_letter("skill_enqueue_timeout", &replay_payload)
                    .await;
                return Err(anyhow!("timeout"));
            }
        }

        reply_rx.await.map_err(|_| anyhow!("execution_error"))?
    }

    async fn mailbox_tx(&self) -> mpsc::Sender<SkillActorRequest> {
        self.mailbox
            .get_or_init(|| async { self.spawn_worker() })
            .await
            .clone()
    }

    fn spawn_worker(&self) -> mpsc::Sender<SkillActorRequest> {
        let (tx, mut rx) = mpsc::channel::<SkillActorRequest>(self.mailbox_size.max(1));
        let skill = Arc::clone(&self.skill);
        let skill_timeout = self.skill_timeout;
        let skill_name = self.skill_name.clone();
        let gauge_name = self.gauge_name.clone();

        tokio::spawn(async move {
            while let Some(request) = rx.recv().await {
                observability::inc_counter("dequeue.skill");
                observability::add_gauge(&gauge_name, -1);
                let started = std::time::Instant::now();
                let execute_fut = skill.execute(request.params, &request.context);
                let result = match skill_timeout {
                    Some(timeout_dur) => match timeout(timeout_dur, execute_fut).await {
                        Ok(output) => output,
                        Err(_) => Err(anyhow!("timeout")),
                    },
                    None => execute_fut.await,
                };

                if let Err(ref err) = result {
                    warn!("Skill {skill_name} failed: {err}");
                }
                observability::observe_duration("latency.skill_execution_ms", started.elapsed());
                let _ = request.reply_tx.send(result);
            }
        });

        tx
    }

    async fn record_dead_letter(
        &self,
        reason: &str,
        replay_payload: &DeadLetterSkillReplayPayload,
    ) {
        let record = DeadLetterRecord::skill_router_with_payload(
            reason,
            replay_payload.context.jid.clone(),
            uuid::Uuid::new_v4().to_string(),
            &self.skill_name,
            serde_json::to_value(replay_payload).ok(),
        );
        if let Err(err) = self.dead_letter.record(record).await {
            warn!(
                reason = reason,
                skill_name = %self.skill_name,
                dead_letter_path = %self.dead_letter.path().display(),
                "Failed to persist skill dead-letter record: {err}",
            );
        } else {
            observability::inc_counter("dead_letter.skill_router");
        }
    }
}

pub struct SkillRouterActor {
    tool_definitions: Vec<ToolDefinition>,
    skill_actors: HashMap<String, SkillActor>,
    dead_letter: Arc<DeadLetterService>,
    allowed_capabilities: Vec<String>,
    max_parallel_skills: usize,
    skill_queue_timeout_ms: u64,
    skill_queue_timeout: Duration,
    permits: Semaphore,
}

impl SkillRouterActor {
    pub fn new(
        skills: &SkillRegistry,
        config: &ActorToolingConfig,
        dead_letter_path: PathBuf,
    ) -> Self {
        Self::with_dead_letter_service(
            skills,
            config,
            Arc::new(DeadLetterService::new(dead_letter_path)),
        )
    }

    pub fn with_dead_letter_service(
        skills: &SkillRegistry,
        config: &ActorToolingConfig,
        dead_letter: Arc<DeadLetterService>,
    ) -> Self {
        let max_parallel_skills = config.max_parallel_skills.max(1);
        let skill_timeout = if config.skill_timeout_secs == 0 {
            None
        } else {
            Some(Duration::from_secs(config.skill_timeout_secs))
        };
        let skill_queue_timeout = Duration::from_millis(config.skill_queue_timeout_ms);

        let skill_actors = skills
            .skill_names()
            .into_iter()
            .filter_map(|name| {
                skills.get_shared(name).map(|skill| {
                    let required_capabilities = skill.capabilities();
                    let actor = SkillActor::new(
                        name.to_string(),
                        Arc::clone(&dead_letter),
                        required_capabilities,
                        skill,
                        max_parallel_skills,
                        skill_queue_timeout,
                        skill_timeout,
                    );
                    (name.to_string(), actor)
                })
            })
            .collect();

        Self {
            tool_definitions: skills.tool_definitions(),
            skill_actors,
            dead_letter,
            allowed_capabilities: config.allowed_capabilities.clone(),
            max_parallel_skills,
            skill_queue_timeout_ms: config.skill_queue_timeout_ms,
            skill_queue_timeout,
            permits: Semaphore::new(max_parallel_skills),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.tool_definitions.is_empty()
    }

    pub fn tool_definitions(&self) -> Vec<ToolDefinition> {
        self.tool_definitions.clone()
    }

    pub async fn execute(
        &self,
        skill_name: &str,
        params: serde_json::Value,
        context: &SkillContext,
    ) -> SkillExecution {
        let handle_started = std::time::Instant::now();
        let Some(skill_actor) = self.skill_actors.get(skill_name) else {
            observability::inc_counter("error.skill.unknown_tool");
            warn!("Unknown skill requested: {skill_name}");
            return SkillExecution::error(
                format!("Error: unknown tool '{skill_name}'"),
                SkillErrorClass::UnknownTool,
            );
        };

        if let Some(missing_capability) = skill_actor
            .required_capabilities()
            .iter()
            .find(|capability| !self.is_capability_allowed(capability))
        {
            observability::inc_counter("error.skill.policy_denied");
            warn!(
                "Skill {skill_name} denied by policy (missing capability allowance: {missing_capability})"
            );
            return SkillExecution::error(
                format!("Error: policy_denied (missing capability '{missing_capability}')"),
                SkillErrorClass::PolicyDenied,
            );
        }

        let _permit = match timeout(self.skill_queue_timeout, self.permits.acquire()).await {
            Ok(Ok(permit)) => permit,
            Ok(Err(_)) => {
                observability::inc_counter("overflow.skill.permit_closed");
                warn!(
                    "Skill router concurrency gate closed (max_parallel_skills={})",
                    self.max_parallel_skills
                );
                self.record_dead_letter("skill_permit_closed", skill_name, params.clone(), context)
                    .await;
                return SkillExecution::error(
                    "Error: execution_error".to_string(),
                    SkillErrorClass::ExecutionError,
                );
            }
            Err(_elapsed) => {
                observability::inc_counter("overflow.skill.queue_timeout");
                warn!(
                    "Skill {skill_name} queue wait timed out after {}ms (max_parallel_skills={})",
                    self.skill_queue_timeout_ms, self.max_parallel_skills
                );
                self.record_dead_letter("skill_queue_timeout", skill_name, params.clone(), context)
                    .await;
                return SkillExecution::error(
                    "Error: timeout".to_string(),
                    SkillErrorClass::Timeout,
                );
            }
        };

        match skill_actor.execute(params, context).await {
            Ok(output) => {
                observability::observe_actor_latency("skill_router", handle_started.elapsed());
                SkillExecution::success(output)
            }
            Err(e) => {
                let message = e.to_string();
                let error_class = classify_skill_error(&message);
                observability::inc_counter("error.skill.execution");
                observability::observe_actor_latency("skill_router", handle_started.elapsed());
                SkillExecution::error(format!("Error: {message}"), error_class)
            }
        }
    }

    fn is_capability_allowed(&self, required_capability: &str) -> bool {
        self.allowed_capabilities
            .iter()
            .any(|allow_rule| capability_matches_rule(required_capability, allow_rule))
    }

    async fn record_dead_letter(
        &self,
        reason: &str,
        skill_name: &str,
        params: serde_json::Value,
        context: &SkillContext,
    ) {
        let replay_payload = DeadLetterSkillReplayPayload {
            skill_name: skill_name.to_string(),
            params,
            context: context.clone(),
        };
        let record = DeadLetterRecord::skill_router_with_payload(
            reason,
            context.jid.clone(),
            uuid::Uuid::new_v4().to_string(),
            skill_name,
            serde_json::to_value(replay_payload).ok(),
        );
        if let Err(err) = self.dead_letter.record(record).await {
            warn!(
                reason = reason,
                skill_name = %skill_name,
                dead_letter_path = %self.dead_letter.path().display(),
                "Failed to persist skill-router dead-letter record: {err}",
            );
        } else {
            observability::inc_counter("dead_letter.skill_router");
        }
    }
}

fn classify_skill_error(message: &str) -> SkillErrorClass {
    let normalized = message.to_ascii_lowercase();
    if normalized.contains("timeout") {
        SkillErrorClass::Timeout
    } else if normalized.contains("policy_denied") || normalized.contains("policy denied") {
        SkillErrorClass::PolicyDenied
    } else {
        SkillErrorClass::ExecutionError
    }
}

fn capability_matches_rule(required_capability: &str, allow_rule: &str) -> bool {
    if allow_rule == "*" {
        return true;
    }

    if let Some(prefix) = allow_rule.strip_suffix('*') {
        return required_capability.starts_with(prefix);
    }

    required_capability == allow_rule
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use anyhow::Result;
    use async_trait::async_trait;
    use serde_json::json;
    use tempfile::TempDir;
    use tokio::time::{sleep, Duration as TokioDuration};

    use super::*;
    use crate::actors::dead_letter::{
        read_dead_letters as read_dead_letter_records, DeadLetterSkillReplayPayload,
        DeadLetterSource,
    };
    use crate::skills::Skill;

    struct EchoSkill;

    #[async_trait]
    impl Skill for EchoSkill {
        fn name(&self) -> &str {
            "echo"
        }

        fn description(&self) -> &str {
            "Echoes query input"
        }

        fn parameters_schema(&self) -> serde_json::Value {
            json!({
                "type": "object",
                "properties": {
                    "query": { "type": "string" }
                },
                "required": ["query"]
            })
        }

        async fn execute(
            &self,
            params: serde_json::Value,
            _context: &SkillContext,
        ) -> Result<String> {
            let query = params
                .get("query")
                .and_then(|value| value.as_str())
                .unwrap_or_default();
            Ok(format!("echo: {query}"))
        }
    }

    struct DenySkill;

    #[async_trait]
    impl Skill for DenySkill {
        fn name(&self) -> &str {
            "deny"
        }

        fn description(&self) -> &str {
            "Always denied by policy"
        }

        fn parameters_schema(&self) -> serde_json::Value {
            json!({"type": "object", "properties": {}})
        }

        async fn execute(
            &self,
            _params: serde_json::Value,
            _context: &SkillContext,
        ) -> Result<String> {
            Err(anyhow::anyhow!("policy_denied: blocked by test"))
        }
    }

    struct SlowSkill;

    #[async_trait]
    impl Skill for SlowSkill {
        fn name(&self) -> &str {
            "slow"
        }

        fn description(&self) -> &str {
            "Sleeps until timeout"
        }

        fn parameters_schema(&self) -> serde_json::Value {
            json!({"type": "object", "properties": {}})
        }

        async fn execute(
            &self,
            _params: serde_json::Value,
            _context: &SkillContext,
        ) -> Result<String> {
            sleep(TokioDuration::from_secs(2)).await;
            Ok("done".to_string())
        }
    }

    struct BlockingSkill;

    #[async_trait]
    impl Skill for BlockingSkill {
        fn name(&self) -> &str {
            "blocking"
        }

        fn description(&self) -> &str {
            "Holds a permit for queue-timeout saturation tests"
        }

        fn parameters_schema(&self) -> serde_json::Value {
            json!({"type": "object", "properties": {}})
        }

        async fn execute(
            &self,
            _params: serde_json::Value,
            _context: &SkillContext,
        ) -> Result<String> {
            sleep(TokioDuration::from_millis(200)).await;
            Ok("blocking-done".to_string())
        }
    }

    struct NetworkSkill;

    #[async_trait]
    impl Skill for NetworkSkill {
        fn name(&self) -> &str {
            "network_tool"
        }

        fn description(&self) -> &str {
            "Skill requiring network capability"
        }

        fn parameters_schema(&self) -> serde_json::Value {
            json!({"type": "object", "properties": {}})
        }

        fn capabilities(&self) -> Vec<String> {
            vec!["network:api.example.com:443".to_string()]
        }

        async fn execute(
            &self,
            _params: serde_json::Value,
            _context: &SkillContext,
        ) -> Result<String> {
            Ok("network-ok".to_string())
        }
    }

    fn test_context() -> SkillContext {
        SkillContext {
            jid: "alice@localhost".to_string(),
            base_path: PathBuf::from("."),
        }
    }

    fn tooling_config(skill_timeout_secs: u64) -> ActorToolingConfig {
        ActorToolingConfig {
            max_tool_rounds: 5,
            skill_timeout_secs,
            max_parallel_skills: 8,
            skill_queue_timeout_ms: 5000,
            allowed_capabilities: vec!["*".to_string()],
        }
    }

    fn dead_letter_path() -> PathBuf {
        std::env::temp_dir().join(format!(
            "fluux-skill-dead-letters-{}.jsonl",
            uuid::Uuid::new_v4()
        ))
    }

    #[tokio::test]
    async fn test_router_executes_skill_successfully() {
        let mut skills = SkillRegistry::new();
        skills.register(Box::new(EchoSkill));
        let router = SkillRouterActor::new(&skills, &tooling_config(10), dead_letter_path());

        let result = router
            .execute("echo", json!({"query": "hello"}), &test_context())
            .await;

        assert_eq!(result.content, "echo: hello");
        assert_eq!(result.error_class, None);
    }

    #[tokio::test]
    async fn test_router_returns_unknown_tool_error() {
        let skills = SkillRegistry::new();
        let router = SkillRouterActor::new(&skills, &tooling_config(10), dead_letter_path());

        let result = router.execute("missing", json!({}), &test_context()).await;

        assert_eq!(result.content, "Error: unknown tool 'missing'");
        assert_eq!(result.error_class, Some(SkillErrorClass::UnknownTool));
    }

    #[tokio::test]
    async fn test_router_classifies_policy_denied_errors() {
        let mut skills = SkillRegistry::new();
        skills.register(Box::new(DenySkill));
        let router = SkillRouterActor::new(&skills, &tooling_config(10), dead_letter_path());

        let result = router.execute("deny", json!({}), &test_context()).await;

        assert!(result.content.contains("policy_denied: blocked by test"));
        assert_eq!(result.error_class, Some(SkillErrorClass::PolicyDenied));
    }

    #[tokio::test]
    async fn test_router_applies_timeout() {
        let mut skills = SkillRegistry::new();
        skills.register(Box::new(SlowSkill));
        let router = SkillRouterActor::new(&skills, &tooling_config(1), dead_letter_path());

        let result = router.execute("slow", json!({}), &test_context()).await;

        assert_eq!(result.content, "Error: timeout");
        assert_eq!(result.error_class, Some(SkillErrorClass::Timeout));
    }

    #[tokio::test]
    async fn test_router_returns_timeout_when_skill_queue_wait_expires() {
        let mut skills = SkillRegistry::new();
        skills.register(Box::new(BlockingSkill));
        let tmp = TempDir::new().unwrap();
        let dead_letter_path = tmp.path().join("skill-queue-timeout-dead-letters.jsonl");
        let router = SkillRouterActor::new(
            &skills,
            &ActorToolingConfig {
                max_tool_rounds: 5,
                skill_timeout_secs: 10,
                max_parallel_skills: 1,
                skill_queue_timeout_ms: 50,
                allowed_capabilities: vec!["*".to_string()],
            },
            dead_letter_path.clone(),
        );

        let first_context = test_context();
        let second_context = test_context();

        let first_call = router.execute("blocking", json!({}), &first_context);
        let second_call = async {
            sleep(TokioDuration::from_millis(10)).await;
            router.execute("blocking", json!({}), &second_context).await
        };

        let (first_result, second_result) = tokio::join!(first_call, second_call);
        assert_eq!(first_result.error_class, None);
        assert_eq!(first_result.content, "blocking-done");
        assert_eq!(second_result.content, "Error: timeout");
        assert_eq!(second_result.error_class, Some(SkillErrorClass::Timeout));

        let dead_letters = read_dead_letter_records(&dead_letter_path).unwrap();
        assert_eq!(dead_letters.len(), 1);
        assert_eq!(dead_letters[0].source, DeadLetterSource::SkillRouter);
        assert_eq!(dead_letters[0].reason, "skill_queue_timeout");
        assert_eq!(
            dead_letters[0].conversation_id.as_deref(),
            Some("alice@localhost")
        );
        assert_eq!(dead_letters[0].payload_kind, "blocking");
        let payload: DeadLetterSkillReplayPayload =
            serde_json::from_value(dead_letters[0].payload.clone().unwrap()).unwrap();
        assert_eq!(payload.skill_name, "blocking");
    }

    #[tokio::test]
    async fn test_router_denies_skill_when_capability_not_allowed() {
        let mut skills = SkillRegistry::new();
        skills.register(Box::new(NetworkSkill));
        let router = SkillRouterActor::new(
            &skills,
            &ActorToolingConfig {
                max_tool_rounds: 5,
                skill_timeout_secs: 10,
                max_parallel_skills: 1,
                skill_queue_timeout_ms: 50,
                allowed_capabilities: vec!["filesystem:*".to_string()],
            },
            dead_letter_path(),
        );

        let result = router
            .execute("network_tool", json!({}), &test_context())
            .await;

        assert!(result
            .content
            .contains("Error: policy_denied (missing capability"));
        assert_eq!(result.error_class, Some(SkillErrorClass::PolicyDenied));
    }

    #[tokio::test]
    async fn test_router_allows_skill_when_capability_matches_prefix_rule() {
        let mut skills = SkillRegistry::new();
        skills.register(Box::new(NetworkSkill));
        let router = SkillRouterActor::new(
            &skills,
            &ActorToolingConfig {
                max_tool_rounds: 5,
                skill_timeout_secs: 10,
                max_parallel_skills: 1,
                skill_queue_timeout_ms: 50,
                allowed_capabilities: vec!["network:*".to_string()],
            },
            dead_letter_path(),
        );

        let result = router
            .execute("network_tool", json!({}), &test_context())
            .await;

        assert_eq!(result.content, "network-ok");
        assert_eq!(result.error_class, None);
    }
}
