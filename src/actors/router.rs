use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc::{self, error::TrySendError};
use tokio::task::JoinHandle;
use tracing::{debug, info, warn};

use super::dead_letter::{DeadLetterRecord, DeadLetterService};
use super::observability;
use super::session::{SessionActor, SessionDependencies};
use super::types::{Envelope, SYSTEM_CONVERSATION_ID};
use crate::xmpp::component::{DisconnectReason, XmppCommand, XmppEvent};
use crate::xmpp::stanzas::{self, MessageType};

pub struct RouterActor {
    session_dependencies: SessionDependencies,
    session_mailbox: usize,
    max_active_sessions: usize,
    session_idle_ttl_secs: u64,
    busy_retry_after_secs: u64,
    dead_letter: Arc<DeadLetterService>,
    restart_backoff_min_ms: u64,
    restart_backoff_max_ms: u64,
    max_restarts_per_minute: u64,
}

struct SessionEntry {
    tx: mpsc::Sender<Envelope>,
    task: JoinHandle<()>,
    last_activity: tokio::time::Instant,
}

struct SessionExit {
    conversation_id: String,
    reason: DisconnectReason,
}

#[derive(Default)]
struct SessionRestartState {
    consecutive_failures: u32,
    recent_restarts: VecDeque<tokio::time::Instant>,
}

const DEFAULT_RESTART_BACKOFF_MIN_MS: u64 = 200;
const DEFAULT_RESTART_BACKOFF_MAX_MS: u64 = 10_000;
const DEFAULT_MAX_RESTARTS_PER_MINUTE: u64 = 60;

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
enum BusyResponseOutcome {
    Sent,
    NotApplicable,
    EgressFull,
    EgressClosed,
}

impl BusyResponseOutcome {
    fn as_str(self) -> &'static str {
        match self {
            Self::Sent => "sent",
            Self::NotApplicable => "not_applicable",
            Self::EgressFull => "egress_full",
            Self::EgressClosed => "egress_closed",
        }
    }
}

impl RouterActor {
    pub fn new(
        session_dependencies: SessionDependencies,
        session_mailbox: usize,
        max_active_sessions: usize,
        session_idle_ttl_secs: u64,
        busy_retry_after_secs: u64,
        dead_letter_path: PathBuf,
    ) -> Self {
        Self::with_supervision(
            session_dependencies,
            session_mailbox,
            max_active_sessions,
            session_idle_ttl_secs,
            busy_retry_after_secs,
            dead_letter_path,
            DEFAULT_RESTART_BACKOFF_MIN_MS,
            DEFAULT_RESTART_BACKOFF_MAX_MS,
            DEFAULT_MAX_RESTARTS_PER_MINUTE,
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub fn with_supervision_service(
        session_dependencies: SessionDependencies,
        session_mailbox: usize,
        max_active_sessions: usize,
        session_idle_ttl_secs: u64,
        busy_retry_after_secs: u64,
        dead_letter: Arc<DeadLetterService>,
        restart_backoff_min_ms: u64,
        restart_backoff_max_ms: u64,
        max_restarts_per_minute: u64,
    ) -> Self {
        Self {
            session_dependencies,
            session_mailbox,
            max_active_sessions,
            session_idle_ttl_secs,
            busy_retry_after_secs,
            dead_letter,
            restart_backoff_min_ms,
            restart_backoff_max_ms,
            max_restarts_per_minute,
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub fn with_supervision(
        session_dependencies: SessionDependencies,
        session_mailbox: usize,
        max_active_sessions: usize,
        session_idle_ttl_secs: u64,
        busy_retry_after_secs: u64,
        dead_letter_path: PathBuf,
        restart_backoff_min_ms: u64,
        restart_backoff_max_ms: u64,
        max_restarts_per_minute: u64,
    ) -> Self {
        Self::with_supervision_service(
            session_dependencies,
            session_mailbox,
            max_active_sessions,
            session_idle_ttl_secs,
            busy_retry_after_secs,
            Arc::new(DeadLetterService::new(dead_letter_path)),
            restart_backoff_min_ms,
            restart_backoff_max_ms,
            max_restarts_per_minute,
        )
    }

    pub async fn run(
        &self,
        mut ingress_rx: mpsc::Receiver<Envelope>,
        egress_tx: mpsc::Sender<XmppCommand>,
    ) -> Result<DisconnectReason> {
        info!("RouterActor started");

        let mut sessions: HashMap<String, SessionEntry> = HashMap::new();
        let mut restart_state: HashMap<String, SessionRestartState> = HashMap::new();
        let mut shutdown_reason: Option<DisconnectReason> = None;
        let exit_mailbox = self.max_active_sessions.max(1);
        let (session_exit_tx, mut session_exit_rx) = mpsc::channel::<SessionExit>(exit_mailbox);
        let (restart_tx, mut restart_rx) = mpsc::channel::<String>(exit_mailbox);
        let idle_ttl = Duration::from_secs(self.session_idle_ttl_secs.max(1));
        let sweep_secs = self.session_idle_ttl_secs.max(1).min(60);
        let mut idle_sweep = tokio::time::interval(Duration::from_secs(sweep_secs));
        idle_sweep.tick().await;

        loop {
            tokio::select! {
                maybe_envelope = ingress_rx.recv() => {
                    let Some(envelope) = maybe_envelope else {
                        break;
                    };
                    let handle_started = tokio::time::Instant::now();
                    observability::inc_counter("dequeue.router");
                    observability::add_gauge("mailbox.router.depth", -1);

                    let conversation_id = envelope.conversation_id.clone();
                    debug!(
                        actor = "router",
                        conversation_id = %conversation_id,
                        correlation_id = %envelope.correlation_id,
                        "Routing envelope to per-conversation session actor",
                    );

                    if !sessions.contains_key(&conversation_id) {
                        let max_active_sessions = self.max_active_sessions.max(1);
                        let at_capacity = sessions.len() >= max_active_sessions;
                        if at_capacity && conversation_id != SYSTEM_CONVERSATION_ID {
                            observability::inc_counter("overflow.router.max_active_sessions");
                            warn!(
                                conversation_id = %conversation_id,
                                active_sessions = sessions.len(),
                                max_active_sessions,
                                "Max active sessions reached; dropping envelope",
                            );
                            let busy_outcome = try_emit_busy_response(
                                &egress_tx,
                                &envelope,
                                self.busy_retry_after_secs,
                            );
                            self.record_dead_letter(
                                "max_active_sessions_reached",
                                &envelope,
                                busy_outcome,
                            )
                            .await;
                            observability::observe_actor_latency("router", handle_started.elapsed());
                            continue;
                        }

                        let entry = self.spawn_session(
                            conversation_id.clone(),
                            egress_tx.clone(),
                            session_exit_tx.clone(),
                        );
                        sessions.insert(conversation_id.clone(), entry);
                        observability::set_gauge("session.active", sessions.len() as i64);

                        info!(
                            conversation_id = %conversation_id,
                            active_sessions = sessions.len(),
                            "Started session actor",
                        );
                    }

                    let send_result = if let Some(entry) = sessions.get_mut(&conversation_id) {
                        let result = entry.tx.try_send(envelope);
                        if result.is_ok() {
                            entry.last_activity = tokio::time::Instant::now();
                            observability::inc_counter("enqueue.session");
                            observability::add_gauge("mailbox.session.depth", 1);
                        }
                        result
                    } else {
                        warn!(
                            conversation_id = %conversation_id,
                            "Session actor missing while routing envelope",
                        );
                        let busy_outcome = try_emit_busy_response(
                            &egress_tx,
                            &envelope,
                            self.busy_retry_after_secs,
                        );
                        self.record_dead_letter("session_missing", &envelope, busy_outcome)
                            .await;
                        observability::observe_actor_latency("router", handle_started.elapsed());
                        continue;
                    };

                    match send_result {
                        Ok(()) => {
                            restart_state.remove(&conversation_id);
                        }
                        Err(TrySendError::Full(envelope)) => {
                            observability::inc_counter("overflow.session.mailbox_full");
                            warn!(
                                conversation_id = %conversation_id,
                                "Session mailbox full; applying busy response",
                            );
                            let busy_outcome = try_emit_busy_response(
                                &egress_tx,
                                &envelope,
                                self.busy_retry_after_secs,
                            );
                            self.record_dead_letter("session_mailbox_full", &envelope, busy_outcome)
                                .await;
                        }
                        Err(TrySendError::Closed(envelope)) => {
                            observability::inc_counter("overflow.session.mailbox_closed");
                            warn!(
                                conversation_id = %conversation_id,
                                "Session mailbox closed while routing envelope",
                            );
                            let busy_outcome = try_emit_busy_response(
                                &egress_tx,
                                &envelope,
                                self.busy_retry_after_secs,
                            );
                            self.record_dead_letter("session_mailbox_closed", &envelope, busy_outcome)
                                .await;
                            if let Some(entry) = sessions.remove(&conversation_id) {
                                entry.task.abort();
                            }
                            observability::set_gauge("session.active", sessions.len() as i64);
                        }
                    }
                    observability::observe_actor_latency("router", handle_started.elapsed());
                }
                maybe_restart = restart_rx.recv() => {
                    let Some(conversation_id) = maybe_restart else {
                        continue;
                    };
                    if conversation_id == SYSTEM_CONVERSATION_ID {
                        continue;
                    }
                    if sessions.contains_key(&conversation_id) {
                        continue;
                    }
                    if sessions.len() >= self.max_active_sessions.max(1) {
                        warn!(
                            conversation_id = %conversation_id,
                            active_sessions = sessions.len(),
                            max_active_sessions = self.max_active_sessions.max(1),
                            "Skipping scheduled session restart due to active-session capacity",
                        );
                        continue;
                    }

                    let entry = self.spawn_session(
                        conversation_id.clone(),
                        egress_tx.clone(),
                        session_exit_tx.clone(),
                    );
                    sessions.insert(conversation_id.clone(), entry);
                    observability::inc_counter("restart.session");
                    observability::set_gauge("session.active", sessions.len() as i64);
                    info!(
                        conversation_id = %conversation_id,
                        active_sessions = sessions.len(),
                        "Restarted session actor after supervised backoff",
                    );
                }
                _ = idle_sweep.tick() => {
                    self.evict_idle_sessions(&mut sessions, idle_ttl).await;
                }
                maybe_exit = session_exit_rx.recv() => {
                    let Some(exit) = maybe_exit else {
                        continue;
                    };

                    sessions.remove(&exit.conversation_id);
                    observability::set_gauge("session.active", sessions.len() as i64);
                    info!(
                        conversation_id = %exit.conversation_id,
                        reason = ?exit.reason,
                        active_sessions = sessions.len(),
                        "Session actor exited",
                    );

                    if exit.conversation_id == SYSTEM_CONVERSATION_ID {
                        shutdown_reason = Some(exit.reason);
                        break;
                    }

                    if matches!(exit.reason, DisconnectReason::ConnectionLost) {
                        if let Some(delay) =
                            self.next_restart_delay(&exit.conversation_id, &mut restart_state)
                        {
                            observability::inc_counter("restart.session.scheduled");
                            let restart_conversation_id = exit.conversation_id.clone();
                            let restart_tx = restart_tx.clone();
                            tokio::spawn(async move {
                                tokio::time::sleep(delay).await;
                                let _ = restart_tx.send(restart_conversation_id).await;
                            });
                        } else {
                            observability::inc_counter("restart.session.suppressed");
                            warn!(
                                conversation_id = %exit.conversation_id,
                                max_restarts_per_minute = self.max_restarts_per_minute,
                                "Session restart budget exhausted; suppressing automatic restart",
                            );
                        }
                    } else {
                        restart_state.remove(&exit.conversation_id);
                    }
                }
            }
        }

        for (_, entry) in sessions {
            entry.task.abort();
            let _ = entry.task.await;
        }
        observability::set_gauge("session.active", 0);

        let reason = shutdown_reason.unwrap_or(DisconnectReason::ConnectionLost);
        info!(reason = ?reason, "RouterActor stopped");
        Ok(reason)
    }

    fn spawn_session(
        &self,
        conversation_id: String,
        egress_tx: mpsc::Sender<XmppCommand>,
        session_exit_tx: mpsc::Sender<SessionExit>,
    ) -> SessionEntry {
        let session_mailbox = self.session_mailbox.max(1);
        let (tx, rx) = mpsc::channel::<Envelope>(session_mailbox);
        let dependencies = self.session_dependencies.clone();
        let task_conversation_id = conversation_id.clone();

        let task = tokio::spawn(async move {
            let session = SessionActor::new(task_conversation_id.clone(), dependencies);
            let reason = match session.run(rx, egress_tx).await {
                Ok(reason) => reason,
                Err(err) => {
                    warn!(
                        conversation_id = %task_conversation_id,
                        "Session actor failed: {err}",
                    );
                    DisconnectReason::ConnectionLost
                }
            };

            let _ = session_exit_tx
                .send(SessionExit {
                    conversation_id: task_conversation_id,
                    reason,
                })
                .await;
        });

        SessionEntry {
            tx,
            task,
            last_activity: tokio::time::Instant::now(),
        }
    }

    async fn evict_idle_sessions(
        &self,
        sessions: &mut HashMap<String, SessionEntry>,
        idle_ttl: Duration,
    ) {
        let now = tokio::time::Instant::now();
        let idle_conversations: Vec<String> = sessions
            .iter()
            .filter_map(|(conversation_id, entry)| {
                if conversation_id == SYSTEM_CONVERSATION_ID {
                    return None;
                }

                if now.duration_since(entry.last_activity) >= idle_ttl {
                    Some(conversation_id.clone())
                } else {
                    None
                }
            })
            .collect();

        for conversation_id in idle_conversations {
            if let Some(entry) = sessions.remove(&conversation_id) {
                info!(
                    conversation_id = %conversation_id,
                    session_idle_ttl_secs = self.session_idle_ttl_secs.max(1),
                    active_sessions = sessions.len(),
                    "Evicting idle session actor",
                );
                entry.task.abort();
                let _ = entry.task.await;
                observability::set_gauge("session.active", sessions.len() as i64);
            }
        }
    }

    fn next_restart_delay(
        &self,
        conversation_id: &str,
        restart_state: &mut HashMap<String, SessionRestartState>,
    ) -> Option<Duration> {
        let now = tokio::time::Instant::now();
        if self.max_restarts_per_minute == 0 {
            return None;
        }
        let max_restarts = self.max_restarts_per_minute as usize;
        let window = Duration::from_secs(60);

        let state = restart_state
            .entry(conversation_id.to_string())
            .or_default();
        while let Some(seen_at) = state.recent_restarts.front() {
            if now.duration_since(*seen_at) > window {
                state.recent_restarts.pop_front();
            } else {
                break;
            }
        }

        if state.recent_restarts.len() >= max_restarts {
            return None;
        }

        let min_backoff_ms = self.restart_backoff_min_ms.max(1);
        let max_backoff_ms = self.restart_backoff_max_ms.max(min_backoff_ms);
        let exp = state.consecutive_failures.min(16);
        let delay_ms = (min_backoff_ms as u128)
            .saturating_mul(1u128 << exp)
            .min(max_backoff_ms as u128) as u64;

        state.consecutive_failures = state.consecutive_failures.saturating_add(1);
        state.recent_restarts.push_back(now);
        Some(Duration::from_millis(delay_ms))
    }

    async fn record_dead_letter(
        &self,
        reason: &str,
        envelope: &Envelope,
        busy_response: BusyResponseOutcome,
    ) {
        let record = DeadLetterRecord::router(
            reason,
            envelope.conversation_id.clone(),
            envelope.correlation_id.clone(),
            envelope.received_at_ms,
            event_kind(&envelope.event),
            busy_response.as_str(),
        );
        if let Err(err) = self.dead_letter.record(record).await {
            warn!(
                actor = "router",
                conversation_id = %envelope.conversation_id,
                correlation_id = %envelope.correlation_id,
                dead_letter_path = %self.dead_letter.path().display(),
                "Failed to persist dead-letter record: {err}",
            );
        } else {
            observability::inc_counter("dead_letter.router");
        }
    }
}

fn try_emit_busy_response(
    egress_tx: &mpsc::Sender<XmppCommand>,
    envelope: &Envelope,
    busy_retry_after_secs: u64,
) -> BusyResponseOutcome {
    let Some(cmd) = build_busy_response_command(envelope, busy_retry_after_secs) else {
        return BusyResponseOutcome::NotApplicable;
    };

    if let Err(err) = egress_tx.try_send(cmd) {
        match err {
            TrySendError::Full(_) => {
                observability::inc_counter("busy_response.egress_full");
                warn!(
                    conversation_id = %envelope.conversation_id,
                    correlation_id = %envelope.correlation_id,
                    "Egress mailbox full; dropping busy response",
                );
                BusyResponseOutcome::EgressFull
            }
            TrySendError::Closed(_) => {
                observability::inc_counter("busy_response.egress_closed");
                warn!(
                    conversation_id = %envelope.conversation_id,
                    correlation_id = %envelope.correlation_id,
                    "Egress mailbox closed; dropping busy response",
                );
                BusyResponseOutcome::EgressClosed
            }
        }
    } else {
        observability::inc_counter("busy_response.sent");
        BusyResponseOutcome::Sent
    }
}

fn build_busy_response_command(
    envelope: &Envelope,
    busy_retry_after_secs: u64,
) -> Option<XmppCommand> {
    let XmppEvent::Message(message) = &envelope.event else {
        return None;
    };

    let body = format!(
        "System busy. Please retry in {busy_retry_after_secs}s. (ref: {})",
        envelope.correlation_id
    );

    match message.message_type {
        MessageType::Chat => Some(XmppCommand::SendMessage {
            to: message.from.clone(),
            body,
            id: None,
        }),
        MessageType::GroupChat => Some(XmppCommand::SendMucMessage {
            to: stanzas::bare_jid(&message.from).to_string(),
            body,
            id: None,
        }),
    }
}

fn event_kind(event: &XmppEvent) -> &'static str {
    match event {
        XmppEvent::Connected => "connected",
        XmppEvent::Message(_) => "message",
        XmppEvent::Presence(_) => "presence",
        XmppEvent::Reaction(_) => "reaction",
        XmppEvent::StreamError(_) => "stream_error",
        XmppEvent::Error(_) => "error",
        XmppEvent::ReadTimeout => "read_timeout",
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::path::Path;
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::time::Duration;

    use super::*;
    use crate::actors::dead_letter::{
        read_dead_letters as read_dead_letter_records, DeadLetterRecord, DeadLetterSource,
    };
    use crate::actors::message::MessageResponder;
    use crate::actors::reaction::ReactionResponder;
    use crate::actors::test_actor_runtime_harness::build_default_actor_runtime_fixture;
    use crate::agent::runtime::ActorRuntimeDependencies;
    use crate::xmpp::stanzas::{IncomingMessage, MessageType};
    use tempfile::TempDir;

    fn test_runtime() -> (ActorRuntimeDependencies, TempDir) {
        build_default_actor_runtime_fixture(vec!["*".to_string()])
    }

    fn test_session_dependencies(runtime: &ActorRuntimeDependencies) -> SessionDependencies {
        let session_responder = Arc::clone(&runtime.session_responder);
        let message_responder: Arc<dyn MessageResponder> = session_responder.clone();
        let reaction_responder: Arc<dyn ReactionResponder> = session_responder;
        SessionDependencies::new(
            Arc::clone(&runtime.config),
            Arc::clone(&runtime.memory_writer),
            message_responder,
            reaction_responder,
        )
    }

    fn chat_envelope(from: &str, msg_id: &str, body: &str) -> Envelope {
        Envelope::from_xmpp_event(XmppEvent::Message(IncomingMessage {
            from: from.to_string(),
            to: "bot@localhost".to_string(),
            body: body.to_string(),
            id: Some(msg_id.to_string()),
            message_type: MessageType::Chat,
            oob: vec![],
        }))
    }

    fn body_tag(body: &str) -> &'static str {
        if body == "pong" {
            "ping"
        } else if body.starts_with("Commands:") {
            "help"
        } else {
            "other"
        }
    }

    fn read_dead_letters(path: &Path) -> Vec<DeadLetterRecord> {
        read_dead_letter_records(path).unwrap()
    }

    #[test]
    fn test_router_restart_backoff_is_exponential_and_capped() {
        let (runtime, _tmp) = test_runtime();
        let router = RouterActor::with_supervision(
            test_session_dependencies(&runtime),
            8,
            8,
            3600,
            5,
            PathBuf::from("data/dead_letters.jsonl"),
            50,
            120,
            10,
        );
        let mut restart_state = HashMap::new();

        let first = router
            .next_restart_delay("alice@localhost", &mut restart_state)
            .expect("first restart delay");
        let second = router
            .next_restart_delay("alice@localhost", &mut restart_state)
            .expect("second restart delay");
        let third = router
            .next_restart_delay("alice@localhost", &mut restart_state)
            .expect("third restart delay");

        assert_eq!(first, Duration::from_millis(50));
        assert_eq!(second, Duration::from_millis(100));
        assert_eq!(third, Duration::from_millis(120));
    }

    #[test]
    fn test_router_restart_budget_limits_restarts_per_minute() {
        let (runtime, _tmp) = test_runtime();
        let router = RouterActor::with_supervision(
            test_session_dependencies(&runtime),
            8,
            8,
            3600,
            5,
            PathBuf::from("data/dead_letters.jsonl"),
            50,
            200,
            2,
        );
        let mut restart_state = HashMap::new();

        assert!(router
            .next_restart_delay("alice@localhost", &mut restart_state)
            .is_some());
        assert!(router
            .next_restart_delay("alice@localhost", &mut restart_state)
            .is_some());
        assert!(router
            .next_restart_delay("alice@localhost", &mut restart_state)
            .is_none());
    }

    #[test]
    fn test_router_restart_budget_zero_disables_auto_restart() {
        let (runtime, _tmp) = test_runtime();
        let router = RouterActor::with_supervision(
            test_session_dependencies(&runtime),
            8,
            8,
            3600,
            5,
            PathBuf::from("data/dead_letters.jsonl"),
            50,
            200,
            0,
        );
        let mut restart_state = HashMap::new();

        assert!(router
            .next_restart_delay("alice@localhost", &mut restart_state)
            .is_none());
    }

    #[test]
    fn test_build_busy_response_for_chat_message() {
        let envelope = Envelope {
            conversation_id: "alice@example.com".to_string(),
            correlation_id: "corr-123".to_string(),
            received_at_ms: 0,
            event: XmppEvent::Message(IncomingMessage {
                from: "alice@example.com/mobile".to_string(),
                to: "bot@example.com".to_string(),
                body: "hello".to_string(),
                id: Some("m1".to_string()),
                message_type: MessageType::Chat,
                oob: vec![],
            }),
        };

        let cmd = build_busy_response_command(&envelope, 5).expect("busy response expected");
        match cmd {
            XmppCommand::SendMessage { to, body, id } => {
                assert_eq!(to, "alice@example.com/mobile");
                assert!(body.contains("System busy. Please retry in 5s."));
                assert!(body.contains("(ref: corr-123)"));
                assert!(id.is_none());
            }
            _ => panic!("expected SendMessage"),
        }
    }

    #[test]
    fn test_build_busy_response_for_groupchat_message() {
        let envelope = Envelope {
            conversation_id: "room@conference.example.com".to_string(),
            correlation_id: "corr-room".to_string(),
            received_at_ms: 0,
            event: XmppEvent::Message(IncomingMessage {
                from: "room@conference.example.com/alice".to_string(),
                to: "bot@example.com".to_string(),
                body: "@bot hello".to_string(),
                id: Some("m2".to_string()),
                message_type: MessageType::GroupChat,
                oob: vec![],
            }),
        };

        let cmd = build_busy_response_command(&envelope, 7).expect("busy response expected");
        match cmd {
            XmppCommand::SendMucMessage { to, body, id } => {
                assert_eq!(to, "room@conference.example.com");
                assert!(body.contains("System busy. Please retry in 7s."));
                assert!(body.contains("(ref: corr-room)"));
                assert!(id.is_none());
            }
            _ => panic!("expected SendMucMessage"),
        }
    }

    #[test]
    fn test_build_busy_response_non_message_event_returns_none() {
        let envelope = Envelope {
            conversation_id: "__system__".to_string(),
            correlation_id: "corr-sys".to_string(),
            received_at_ms: 0,
            event: XmppEvent::Connected,
        };

        assert!(build_busy_response_command(&envelope, 5).is_none());
    }

    #[tokio::test]
    async fn test_router_emits_busy_when_max_active_sessions_reached() {
        let (runtime, tmp) = test_runtime();
        let dead_letter_path = tmp.path().join("dead_letters-max-active.jsonl");
        let router = RouterActor::new(
            test_session_dependencies(&runtime),
            8,
            1,
            3600,
            5,
            dead_letter_path.clone(),
        );

        let (ingress_tx, ingress_rx) = mpsc::channel(32);
        let (egress_tx, mut egress_rx) = mpsc::channel(32);

        let router_task = tokio::spawn(async move { router.run(ingress_rx, egress_tx).await });

        ingress_tx
            .send(chat_envelope("alice@localhost/phone", "msg-1", "hello"))
            .await
            .unwrap();
        ingress_tx
            .send(chat_envelope("bob@localhost/laptop", "msg-2", "hello"))
            .await
            .unwrap();

        let mut saw_busy_for_bob = false;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(2);

        while tokio::time::Instant::now() < deadline {
            let recv_result =
                tokio::time::timeout(Duration::from_millis(100), egress_rx.recv()).await;
            if let Ok(Some(XmppCommand::SendMessage { to, body, .. })) = recv_result {
                if to == "bob@localhost/laptop"
                    && body.contains("System busy. Please retry in 5s.")
                    && body.contains("(ref: msg-2)")
                {
                    saw_busy_for_bob = true;
                    break;
                }
            }
        }

        assert!(
            saw_busy_for_bob,
            "expected busy response for second conversation"
        );

        drop(ingress_tx);
        let reason = router_task.await.unwrap().unwrap();
        assert_eq!(reason, DisconnectReason::ConnectionLost);

        let dead_letters = read_dead_letters(&dead_letter_path);
        assert_eq!(dead_letters.len(), 1, "expected one dropped envelope");
        assert_eq!(dead_letters[0].reason, "max_active_sessions_reached");
        assert_eq!(dead_letters[0].correlation_id, "msg-2");
        assert_eq!(dead_letters[0].source, DeadLetterSource::Router);
        assert_eq!(dead_letters[0].busy_response.as_deref(), Some("sent"));
    }

    #[tokio::test]
    async fn test_router_emits_busy_when_session_mailbox_is_saturated() {
        let (runtime, tmp) = test_runtime();
        let dead_letter_path = tmp.path().join("dead_letters-saturated.jsonl");
        let router = RouterActor::new(
            test_session_dependencies(&runtime),
            1,
            8,
            3600,
            9,
            dead_letter_path.clone(),
        );

        let (ingress_tx, ingress_rx) = mpsc::channel(512);
        let (egress_tx, mut egress_rx) = mpsc::channel(512);

        let router_task = tokio::spawn(async move { router.run(ingress_rx, egress_tx).await });

        for i in 0..200 {
            let msg_id = format!("sat-{i}");
            ingress_tx
                .try_send(chat_envelope("alice@localhost/phone", &msg_id, "flood"))
                .unwrap();
        }

        let mut busy_count = 0usize;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        while tokio::time::Instant::now() < deadline {
            let recv_result =
                tokio::time::timeout(Duration::from_millis(50), egress_rx.recv()).await;
            if let Ok(Some(XmppCommand::SendMessage { to, body, .. })) = recv_result {
                if to == "alice@localhost/phone"
                    && body.contains("System busy. Please retry in 9s.")
                    && body.contains("(ref: sat-")
                {
                    busy_count += 1;
                    if busy_count >= 1 {
                        break;
                    }
                }
            }
        }

        assert!(
            busy_count >= 1,
            "expected at least one busy response under session mailbox saturation"
        );

        drop(ingress_tx);
        let reason = router_task.await.unwrap().unwrap();
        assert_eq!(reason, DisconnectReason::ConnectionLost);

        let dead_letters = read_dead_letters(&dead_letter_path);
        assert!(
            dead_letters.iter().any(|entry| {
                entry.reason == "session_mailbox_full"
                    && entry.source == DeadLetterSource::Router
                    && entry.busy_response.as_deref() == Some("sent")
            }),
            "expected at least one dead-letter record for mailbox saturation"
        );
    }

    #[tokio::test]
    async fn test_router_evicts_idle_session_after_ttl() {
        let (runtime, tmp) = test_runtime();
        let router = RouterActor::new(
            test_session_dependencies(&runtime),
            16,
            1,
            1,
            5,
            tmp.path().join("dead_letters-idle-ttl.jsonl"),
        );

        let (ingress_tx, ingress_rx) = mpsc::channel(64);
        let (egress_tx, mut egress_rx) = mpsc::channel(64);
        let router_task = tokio::spawn(async move { router.run(ingress_rx, egress_tx).await });

        ingress_tx
            .send(chat_envelope("alice@localhost/phone", "ttl-1", "/ping"))
            .await
            .unwrap();

        // Drain initial response to avoid coupling assertions to command ordering timing.
        let first_deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        while tokio::time::Instant::now() < first_deadline {
            let recv_result =
                tokio::time::timeout(Duration::from_millis(100), egress_rx.recv()).await;
            if let Ok(Some(XmppCommand::SendMessage { to, body, .. })) = recv_result {
                if to == "alice@localhost/phone" && body == "pong" {
                    break;
                }
            }
        }

        tokio::time::sleep(Duration::from_millis(2400)).await;

        ingress_tx
            .send(chat_envelope("bob@localhost/laptop", "ttl-2", "/ping"))
            .await
            .unwrap();

        let mut saw_bob_pong = false;
        let mut saw_bob_busy = false;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        while tokio::time::Instant::now() < deadline {
            let recv_result =
                tokio::time::timeout(Duration::from_millis(100), egress_rx.recv()).await;
            if let Ok(Some(XmppCommand::SendMessage { to, body, .. })) = recv_result {
                if to == "bob@localhost/laptop" {
                    if body == "pong" {
                        saw_bob_pong = true;
                        break;
                    }
                    if body.contains("System busy") {
                        saw_bob_busy = true;
                        break;
                    }
                }
            }
        }

        assert!(
            saw_bob_pong,
            "expected bob conversation to be accepted after idle session eviction"
        );
        assert!(
            !saw_bob_busy,
            "did not expect busy response once idle session should have been evicted"
        );

        drop(ingress_tx);
        let reason = router_task.await.unwrap().unwrap();
        assert_eq!(reason, DisconnectReason::ConnectionLost);
    }

    #[tokio::test]
    async fn test_router_preserves_per_conversation_command_ordering() {
        let (runtime, tmp) = test_runtime();
        let router = RouterActor::new(
            test_session_dependencies(&runtime),
            16,
            8,
            3600,
            5,
            tmp.path().join("dead_letters-ordering.jsonl"),
        );

        let (ingress_tx, ingress_rx) = mpsc::channel(64);
        let (egress_tx, mut egress_rx) = mpsc::channel(64);
        let router_task = tokio::spawn(async move { router.run(ingress_rx, egress_tx).await });

        let mut expected_alice = Vec::new();
        let mut expected_bob = Vec::new();

        for i in 0..6 {
            let (alice_cmd, alice_tag) = if i % 2 == 0 {
                ("/help", "help")
            } else {
                ("/ping", "ping")
            };
            let (bob_cmd, bob_tag) = if i % 2 == 0 {
                ("/ping", "ping")
            } else {
                ("/help", "help")
            };

            ingress_tx
                .send(chat_envelope(
                    "alice@localhost/phone",
                    &format!("alice-{i}"),
                    alice_cmd,
                ))
                .await
                .unwrap();
            ingress_tx
                .send(chat_envelope(
                    "bob@localhost/laptop",
                    &format!("bob-{i}"),
                    bob_cmd,
                ))
                .await
                .unwrap();

            expected_alice.push(alice_tag);
            expected_bob.push(bob_tag);
        }

        let mut got_alice = Vec::new();
        let mut got_bob = Vec::new();
        let expected_total = expected_alice.len() + expected_bob.len();
        let mut received = 0usize;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(3);

        while received < expected_total && tokio::time::Instant::now() < deadline {
            let recv_result =
                tokio::time::timeout(Duration::from_millis(100), egress_rx.recv()).await;
            if let Ok(Some(XmppCommand::SendMessage { to, body, .. })) = recv_result {
                let tag = body_tag(&body);
                if tag == "other" {
                    continue;
                }
                if to == "alice@localhost/phone" {
                    got_alice.push(tag);
                    received += 1;
                } else if to == "bob@localhost/laptop" {
                    got_bob.push(tag);
                    received += 1;
                }
            }
        }

        assert_eq!(
            got_alice, expected_alice,
            "alice command ordering should be preserved"
        );
        assert_eq!(
            got_bob, expected_bob,
            "bob command ordering should be preserved"
        );

        drop(ingress_tx);
        let reason = router_task.await.unwrap().unwrap();
        assert_eq!(reason, DisconnectReason::ConnectionLost);
    }

    #[tokio::test]
    async fn test_router_recovers_conversation_after_injected_session_termination() {
        let (runtime, tmp) = test_runtime();
        let router = RouterActor::new(
            test_session_dependencies(&runtime),
            16,
            8,
            3600,
            5,
            tmp.path().join("dead_letters-recover.jsonl"),
        );

        let (ingress_tx, ingress_rx) = mpsc::channel(64);
        let (egress_tx, mut egress_rx) = mpsc::channel(64);
        let router_task = tokio::spawn(async move { router.run(ingress_rx, egress_tx).await });

        // Inject a stream error scoped to one conversation to force that session to terminate.
        ingress_tx
            .send(Envelope {
                conversation_id: "alice@localhost".to_string(),
                correlation_id: "inj-stream-error".to_string(),
                received_at_ms: 0,
                event: XmppEvent::StreamError("injected-test-failure".to_string()),
            })
            .await
            .unwrap();

        let mut recovered = false;

        for i in 0..5 {
            ingress_tx
                .send(chat_envelope(
                    "alice@localhost/phone",
                    &format!("recover-{i}"),
                    "/ping",
                ))
                .await
                .unwrap();

            let deadline = tokio::time::Instant::now() + Duration::from_millis(300);
            while tokio::time::Instant::now() < deadline {
                let recv_result =
                    tokio::time::timeout(Duration::from_millis(50), egress_rx.recv()).await;
                if let Ok(Some(XmppCommand::SendMessage { to, body, .. })) = recv_result {
                    if to == "alice@localhost/phone" && body == "pong" {
                        recovered = true;
                        break;
                    }
                }
            }

            if recovered {
                break;
            }
        }

        assert!(
            recovered,
            "expected conversation session to recover and answer /ping after injected termination"
        );

        drop(ingress_tx);
        let reason = router_task.await.unwrap().unwrap();
        assert_eq!(reason, DisconnectReason::ConnectionLost);
    }
}
