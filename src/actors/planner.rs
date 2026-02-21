use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, Result};
use tokio::sync::{mpsc, oneshot, OnceCell};
use tokio::time::timeout;

use super::dead_letter::DeadLetterService;
use super::tool_executor::ToolExecutorActor;
use crate::config::ActorToolingConfig;
use crate::llm::{LlmClient, Message};
use crate::skills::{SkillContext, SkillRegistry};

const PLANNER_BUSY_ERR: &str = "planner_busy";
const PLANNER_UNAVAILABLE_ERR: &str = "planner_unavailable";

struct PlannerRequest {
    system_prompt: String,
    messages: Vec<Message>,
    context: SkillContext,
    reply_tx: oneshot::Sender<Result<PlannerReply>>,
}

struct PlannerReply {
    text: String,
    messages: Vec<Message>,
    input_tokens: u32,
    output_tokens: u32,
}

pub struct PlannerActor {
    config: ActorToolingConfig,
    dead_letter: Arc<DeadLetterService>,
    llm: Arc<dyn LlmClient>,
    skills: Arc<SkillRegistry>,
    enqueue_timeout: Duration,
    mailbox: OnceCell<mpsc::Sender<PlannerRequest>>,
}

impl PlannerActor {
    #[allow(dead_code)]
    pub fn new(
        tooling: ActorToolingConfig,
        dead_letter_path: PathBuf,
        llm: Arc<dyn LlmClient>,
        skills: Arc<SkillRegistry>,
    ) -> Self {
        Self::with_dead_letter_service(
            tooling,
            Arc::new(DeadLetterService::new(dead_letter_path)),
            llm,
            skills,
        )
    }

    pub fn with_dead_letter_service(
        tooling: ActorToolingConfig,
        dead_letter: Arc<DeadLetterService>,
        llm: Arc<dyn LlmClient>,
        skills: Arc<SkillRegistry>,
    ) -> Self {
        let enqueue_timeout = Duration::from_millis(tooling.skill_queue_timeout_ms.max(1));
        Self {
            config: tooling,
            dead_letter,
            llm,
            skills,
            enqueue_timeout,
            mailbox: OnceCell::new(),
        }
    }

    pub async fn plan_reply(
        &self,
        system_prompt: &str,
        messages: &mut Vec<Message>,
        context: &SkillContext,
    ) -> Result<(String, u32, u32)> {
        let (reply_tx, reply_rx) = oneshot::channel();
        let request = PlannerRequest {
            system_prompt: system_prompt.to_string(),
            messages: messages.clone(),
            context: context.clone(),
            reply_tx,
        };

        let tx = self.mailbox_tx().await;
        match timeout(self.enqueue_timeout, tx.send(request)).await {
            Ok(Ok(())) => {}
            Ok(Err(_)) => return Err(anyhow!(PLANNER_UNAVAILABLE_ERR)),
            Err(_) => return Err(anyhow!(PLANNER_BUSY_ERR)),
        }

        let reply = reply_rx
            .await
            .map_err(|_| anyhow!(PLANNER_UNAVAILABLE_ERR))??;
        *messages = reply.messages;
        Ok((reply.text, reply.input_tokens, reply.output_tokens))
    }

    async fn mailbox_tx(&self) -> mpsc::Sender<PlannerRequest> {
        self.mailbox
            .get_or_init(|| async { self.spawn_worker() })
            .await
            .clone()
    }

    fn spawn_worker(&self) -> mpsc::Sender<PlannerRequest> {
        let mailbox_size = self.config.max_parallel_skills.max(1);
        let (tx, mut rx) = mpsc::channel::<PlannerRequest>(mailbox_size);
        let tool_executor = ToolExecutorActor::with_dead_letter_service(
            self.config.clone(),
            Arc::clone(&self.dead_letter),
            Arc::clone(&self.llm),
            Arc::clone(&self.skills),
        );

        tokio::spawn(async move {
            while let Some(mut request) = rx.recv().await {
                let result = tool_executor
                    .execute(
                        &request.system_prompt,
                        &mut request.messages,
                        &request.context,
                    )
                    .await
                    .map(|(text, input_tokens, output_tokens)| PlannerReply {
                        text,
                        messages: request.messages,
                        input_tokens,
                        output_tokens,
                    });
                let _ = request.reply_tx.send(result);
            }
        });

        tx
    }
}
