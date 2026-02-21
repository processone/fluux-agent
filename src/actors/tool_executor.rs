use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, Result};
use tokio::sync::{mpsc, oneshot, OnceCell};
use tokio::time::timeout;
use tracing::{debug, info, warn};

use super::dead_letter::DeadLetterService;
use super::skill_router::SkillRouterActor;
use crate::config::ActorToolingConfig;
use crate::llm::{
    InputContentBlock, LlmClient, Message, MessageContent, StopReason, ToolDefinition,
};
use crate::skills::{SkillContext, SkillRegistry};

const TOOL_EXECUTOR_BUSY_ERR: &str = "tool_executor_busy";
const TOOL_EXECUTOR_UNAVAILABLE_ERR: &str = "tool_executor_unavailable";

struct ToolExecutorRequest {
    system_prompt: String,
    messages: Vec<Message>,
    context: SkillContext,
    reply_tx: oneshot::Sender<Result<ToolExecutorReply>>,
}

struct ToolExecutorReply {
    text: String,
    messages: Vec<Message>,
    input_tokens: u32,
    output_tokens: u32,
}

pub struct ToolExecutorActor {
    config: ActorToolingConfig,
    dead_letter: Arc<DeadLetterService>,
    llm: Arc<dyn LlmClient>,
    skills: Arc<SkillRegistry>,
    enqueue_timeout: Duration,
    mailbox: OnceCell<mpsc::Sender<ToolExecutorRequest>>,
}

impl ToolExecutorActor {
    pub fn new(
        config: ActorToolingConfig,
        dead_letter_path: PathBuf,
        llm: Arc<dyn LlmClient>,
        skills: Arc<SkillRegistry>,
    ) -> Self {
        Self::with_dead_letter_service(
            config,
            Arc::new(DeadLetterService::new(dead_letter_path)),
            llm,
            skills,
        )
    }

    pub fn with_dead_letter_service(
        config: ActorToolingConfig,
        dead_letter: Arc<DeadLetterService>,
        llm: Arc<dyn LlmClient>,
        skills: Arc<SkillRegistry>,
    ) -> Self {
        let enqueue_timeout = Duration::from_millis(config.skill_queue_timeout_ms.max(1));
        Self {
            config,
            dead_letter,
            llm,
            skills,
            enqueue_timeout,
            mailbox: OnceCell::new(),
        }
    }

    pub async fn execute(
        &self,
        system_prompt: &str,
        messages: &mut Vec<Message>,
        context: &SkillContext,
    ) -> Result<(String, u32, u32)> {
        let (reply_tx, reply_rx) = oneshot::channel();
        let request = ToolExecutorRequest {
            system_prompt: system_prompt.to_string(),
            messages: messages.clone(),
            context: context.clone(),
            reply_tx,
        };

        let tx = self.mailbox_tx().await;
        match timeout(self.enqueue_timeout, tx.send(request)).await {
            Ok(Ok(())) => {}
            Ok(Err(_)) => return Err(anyhow!(TOOL_EXECUTOR_UNAVAILABLE_ERR)),
            Err(_) => return Err(anyhow!(TOOL_EXECUTOR_BUSY_ERR)),
        }

        let reply = reply_rx
            .await
            .map_err(|_| anyhow!(TOOL_EXECUTOR_UNAVAILABLE_ERR))??;
        *messages = reply.messages;
        Ok((reply.text, reply.input_tokens, reply.output_tokens))
    }

    async fn mailbox_tx(&self) -> mpsc::Sender<ToolExecutorRequest> {
        self.mailbox
            .get_or_init(|| async { self.spawn_worker() })
            .await
            .clone()
    }

    fn spawn_worker(&self) -> mpsc::Sender<ToolExecutorRequest> {
        let mailbox_size = self.config.max_parallel_skills.max(1);
        let (tx, mut rx) = mpsc::channel::<ToolExecutorRequest>(mailbox_size);
        let config = self.config.clone();
        let dead_letter = Arc::clone(&self.dead_letter);
        let llm = Arc::clone(&self.llm);
        let skills = Arc::clone(&self.skills);

        tokio::spawn(async move {
            let skill_router =
                SkillRouterActor::with_dead_letter_service(skills.as_ref(), &config, dead_letter);
            let tool_defs = if skill_router.is_empty() {
                None
            } else {
                Some(skill_router.tool_definitions())
            };

            while let Some(request) = rx.recv().await {
                let result = execute_inner(
                    &config,
                    &request.system_prompt,
                    request.messages,
                    llm.as_ref(),
                    &skill_router,
                    tool_defs.as_deref(),
                    &request.context,
                )
                .await;
                let _ = request.reply_tx.send(result);
            }
        });

        tx
    }
}

async fn execute_inner(
    config: &ActorToolingConfig,
    system_prompt: &str,
    mut messages: Vec<Message>,
    llm: &dyn LlmClient,
    skill_router: &SkillRouterActor,
    tools_ref: Option<&[ToolDefinition]>,
    context: &SkillContext,
) -> Result<ToolExecutorReply> {
    let mut total_input = 0u32;
    let mut total_output = 0u32;
    let max_rounds = config.max_tool_rounds.max(1);

    for round in 0..max_rounds {
        let response = llm.complete(system_prompt, &messages, tools_ref).await?;

        total_input = total_input.saturating_add(response.input_tokens);
        total_output = total_output.saturating_add(response.output_tokens);

        if response.stop_reason != StopReason::ToolUse || response.tool_calls.is_empty() {
            return Ok(ToolExecutorReply {
                text: response.text,
                messages,
                input_tokens: total_input,
                output_tokens: total_output,
            });
        }

        for tc in &response.tool_calls {
            info!(
                "Tool call [round {}/{}]: {}",
                round + 1,
                max_rounds,
                tc.name
            );
            debug!("Tool input for {}: {}", tc.name, tc.input);
        }

        messages.push(Message {
            role: "assistant".to_string(),
            content: MessageContent::Blocks(response.content_blocks),
        });

        let mut result_blocks = Vec::new();
        for tc in &response.tool_calls {
            let skill_result = skill_router
                .execute(&tc.name, tc.input.clone(), context)
                .await;

            info!(
                "Tool result for {}: {} chars",
                tc.name,
                skill_result.content.len()
            );
            result_blocks.push(InputContentBlock::ToolResult {
                tool_use_id: tc.id.clone(),
                content: skill_result.content,
            });
        }

        messages.push(Message {
            role: "user".to_string(),
            content: MessageContent::Blocks(result_blocks),
        });
    }

    warn!(
        "ToolExecutorActor exhausted {} rounds, forcing final response",
        max_rounds
    );
    let response = llm.complete(system_prompt, &messages, None).await?;
    total_input = total_input.saturating_add(response.input_tokens);
    total_output = total_output.saturating_add(response.output_tokens);
    Ok(ToolExecutorReply {
        text: response.text,
        messages,
        input_tokens: total_input,
        output_tokens: total_output,
    })
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::path::PathBuf;

    use async_trait::async_trait;
    use tokio::sync::Mutex;

    use super::*;
    use crate::llm::{LlmResponse, ToolCall};

    struct StubLlm {
        responses: Mutex<VecDeque<LlmResponse>>,
    }

    impl StubLlm {
        fn new(responses: Vec<LlmResponse>) -> Self {
            Self {
                responses: Mutex::new(VecDeque::from(responses)),
            }
        }
    }

    #[async_trait]
    impl LlmClient for StubLlm {
        async fn complete(
            &self,
            _system_prompt: &str,
            _messages: &[Message],
            _tools: Option<&[ToolDefinition]>,
        ) -> Result<LlmResponse> {
            let mut guard = self.responses.lock().await;
            Ok(guard.pop_front().expect("missing stub response"))
        }

        fn description(&self) -> String {
            "stub-llm".to_string()
        }
    }

    fn end_turn_response(text: &str, in_tokens: u32, out_tokens: u32) -> LlmResponse {
        LlmResponse {
            text: text.to_string(),
            tool_calls: vec![],
            stop_reason: StopReason::EndTurn,
            input_tokens: in_tokens,
            output_tokens: out_tokens,
            content_blocks: vec![InputContentBlock::Text {
                text: text.to_string(),
            }],
        }
    }

    fn tool_use_response(tool_name: &str, tool_id: &str) -> LlmResponse {
        LlmResponse {
            text: String::new(),
            tool_calls: vec![ToolCall {
                id: tool_id.to_string(),
                name: tool_name.to_string(),
                input: serde_json::json!({}),
            }],
            stop_reason: StopReason::ToolUse,
            input_tokens: 5,
            output_tokens: 7,
            content_blocks: vec![InputContentBlock::ToolUse {
                id: tool_id.to_string(),
                name: tool_name.to_string(),
                input: serde_json::json!({}),
            }],
        }
    }

    #[tokio::test]
    async fn test_tool_executor_returns_end_turn_without_tools() {
        let llm: Arc<dyn LlmClient> =
            Arc::new(StubLlm::new(vec![end_turn_response("done", 11, 13)]));
        let skills = Arc::new(SkillRegistry::new());
        let executor = ToolExecutorActor::new(
            ActorToolingConfig {
                max_tool_rounds: 3,
                skill_timeout_secs: 30,
                max_parallel_skills: 1,
                skill_queue_timeout_ms: 100,
                allowed_capabilities: vec!["*".to_string()],
            },
            std::env::temp_dir().join(format!(
                "fluux-tool-exec-dead-letters-{}.jsonl",
                uuid::Uuid::new_v4()
            )),
            Arc::clone(&llm),
            Arc::clone(&skills),
        );
        let mut messages = vec![Message {
            role: "user".to_string(),
            content: MessageContent::Text("hello".to_string()),
        }];
        let context = SkillContext {
            jid: "alice@localhost".to_string(),
            base_path: PathBuf::from("."),
        };

        let (text, in_tokens, out_tokens) = executor
            .execute("sys", &mut messages, &context)
            .await
            .unwrap();

        assert_eq!(text, "done");
        assert_eq!(in_tokens, 11);
        assert_eq!(out_tokens, 13);
        assert_eq!(messages.len(), 1, "no tool rounds should mutate messages");
    }

    #[tokio::test]
    async fn test_tool_executor_unknown_tool_inserts_error_and_completes() {
        let llm: Arc<dyn LlmClient> = Arc::new(StubLlm::new(vec![
            tool_use_response("missing_tool", "toolu-1"),
            end_turn_response("final", 3, 4),
        ]));
        let skills = Arc::new(SkillRegistry::new());
        let executor = ToolExecutorActor::new(
            ActorToolingConfig {
                max_tool_rounds: 3,
                skill_timeout_secs: 30,
                max_parallel_skills: 1,
                skill_queue_timeout_ms: 100,
                allowed_capabilities: vec!["*".to_string()],
            },
            std::env::temp_dir().join(format!(
                "fluux-tool-exec-dead-letters-{}.jsonl",
                uuid::Uuid::new_v4()
            )),
            Arc::clone(&llm),
            Arc::clone(&skills),
        );
        let mut messages = vec![Message {
            role: "user".to_string(),
            content: MessageContent::Text("do something".to_string()),
        }];
        let context = SkillContext {
            jid: "alice@localhost".to_string(),
            base_path: PathBuf::from("."),
        };

        let (text, in_tokens, out_tokens) = executor
            .execute("sys", &mut messages, &context)
            .await
            .unwrap();

        assert_eq!(text, "final");
        assert_eq!(in_tokens, 8);
        assert_eq!(out_tokens, 11);
        assert_eq!(messages.len(), 3);

        match &messages[2].content {
            MessageContent::Blocks(blocks) => match &blocks[0] {
                InputContentBlock::ToolResult {
                    tool_use_id,
                    content,
                } => {
                    assert_eq!(tool_use_id, "toolu-1");
                    assert!(content.contains("unknown tool"));
                }
                _ => panic!("expected tool_result block"),
            },
            _ => panic!("expected structured blocks"),
        }
    }
}
