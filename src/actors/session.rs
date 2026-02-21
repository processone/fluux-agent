use std::sync::Arc;

use anyhow::Result;
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

use super::control::SessionControlActor;
use super::memory_actor::SessionMemoryWriter;
use super::message::{MessageResponder, SessionMessageActor};
use super::observability;
use super::presence::SessionPresenceActor;
use super::reaction::{ReactionResponder, SessionReactionActor};
use super::types::Envelope;
use crate::config::Config;
use crate::xmpp::component::{DisconnectReason, XmppCommand, XmppEvent};

#[derive(Clone)]
pub struct SessionDependencies {
    config: Arc<Config>,
    memory_writer: Arc<dyn SessionMemoryWriter>,
    message_responder: Arc<dyn MessageResponder>,
    reaction_responder: Arc<dyn ReactionResponder>,
}

impl SessionDependencies {
    pub fn new(
        config: Arc<Config>,
        memory_writer: Arc<dyn SessionMemoryWriter>,
        message_responder: Arc<dyn MessageResponder>,
        reaction_responder: Arc<dyn ReactionResponder>,
    ) -> Self {
        Self {
            config,
            memory_writer,
            message_responder,
            reaction_responder,
        }
    }
}

pub struct SessionActor {
    conversation_id: String,
    control_actor: SessionControlActor,
    message_actor: SessionMessageActor,
    presence_actor: SessionPresenceActor,
    reaction_actor: SessionReactionActor,
}

impl SessionActor {
    pub fn new(conversation_id: String, dependencies: SessionDependencies) -> Self {
        let message_actor = SessionMessageActor::new(
            Arc::clone(&dependencies.config),
            Arc::clone(&dependencies.memory_writer),
            Arc::clone(&dependencies.message_responder),
        );
        let reaction_actor = SessionReactionActor::new(
            Arc::clone(&dependencies.config),
            Arc::clone(&dependencies.memory_writer),
            Arc::clone(&dependencies.reaction_responder),
        );
        let presence_actor = SessionPresenceActor::new(Arc::clone(&dependencies.config));
        let control_actor = SessionControlActor::new(Arc::clone(&dependencies.config));
        Self {
            conversation_id,
            control_actor,
            message_actor,
            presence_actor,
            reaction_actor,
        }
    }

    pub async fn run(
        &self,
        mut router_rx: mpsc::Receiver<Envelope>,
        egress_tx: mpsc::Sender<XmppCommand>,
    ) -> Result<DisconnectReason> {
        info!(
            conversation_id = %self.conversation_id,
            "SessionActor started"
        );

        let mut exit_reason = DisconnectReason::ConnectionLost;

        while let Some(envelope) = router_rx.recv().await {
            let handle_started = tokio::time::Instant::now();
            observability::inc_counter("dequeue.session");
            observability::add_gauge("mailbox.session.depth", -1);
            debug!(
                actor = "session",
                session_conversation_id = %self.conversation_id,
                conversation_id = %envelope.conversation_id,
                correlation_id = %envelope.correlation_id,
                "Processing envelope in actor-native session loop",
            );

            match envelope.into_event() {
                XmppEvent::Connected => {
                    self.control_actor.on_connected(egress_tx.clone()).await?;
                }
                XmppEvent::Message(msg) => {
                    let turn_started = tokio::time::Instant::now();
                    self.message_actor.handle(msg, egress_tx.clone()).await?;
                    observability::observe_duration(
                        "latency.turn.message_ms",
                        turn_started.elapsed(),
                    );
                }
                XmppEvent::Presence(pres) => {
                    self.presence_actor.handle(pres, egress_tx.clone()).await?;
                }
                XmppEvent::Reaction(reaction) => {
                    let turn_started = tokio::time::Instant::now();
                    self.reaction_actor
                        .handle(reaction, egress_tx.clone())
                        .await?;
                    observability::observe_duration(
                        "latency.turn.reaction_ms",
                        turn_started.elapsed(),
                    );
                }
                XmppEvent::StreamError(condition) => {
                    exit_reason = self
                        .control_actor
                        .on_stream_error(condition, egress_tx.clone())
                        .await?;
                    break;
                }
                XmppEvent::Error(e) => {
                    self.control_actor.on_error(e, egress_tx.clone()).await?;
                }
                XmppEvent::ReadTimeout => {
                    if let Some(reason) = self
                        .control_actor
                        .on_read_timeout(egress_tx.clone())
                        .await?
                    {
                        exit_reason = reason;
                        break;
                    }
                }
            }
            observability::observe_actor_latency("session", handle_started.elapsed());
        }

        info!(
            conversation_id = %self.conversation_id,
            "SessionActor stopped"
        );
        if matches!(exit_reason, DisconnectReason::ConnectionLost) {
            warn!(
                conversation_id = %self.conversation_id,
                "SessionActor exiting due to mailbox closure or transport disconnect",
            );
        }
        Ok(exit_reason)
    }
}
