use sha1::{Digest, Sha1};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tracing::{debug, error, info, warn};

use quick_xml::events::Event;

use super::stanzas::{self, IncomingMessage, IncomingPresence, IncomingReaction, StanzaParser, XmppStanza};
use super::XmppError;
use crate::config::{ConnectionMode, ServerConfig};

/// Events emitted by the XMPP layer to the runtime
#[derive(Debug)]
pub enum XmppEvent {
    Connected,
    Message(IncomingMessage),
    Presence(IncomingPresence),
    Reaction(IncomingReaction),
    /// A `<stream:error>` was received (e.g. `conflict`, `system-shutdown`).
    StreamError(String),
    Error(String),
}

/// Commands sent by the runtime to the XMPP layer
#[derive(Debug)]
pub enum XmppCommand {
    SendMessage {
        to: String,
        body: String,
        id: Option<String>,
    },
    /// Send a chat state notification (XEP-0085) — composing, paused, etc.
    /// `msg_type` is `"chat"` for 1:1 or `"groupchat"` for MUC.
    SendChatState {
        to: String,
        state: ChatState,
        msg_type: String,
    },
    /// Send a groupchat message to a MUC room (XEP-0045)
    SendMucMessage {
        to: String,
        body: String,
        id: Option<String>,
    },
    /// Join a MUC room (XEP-0045)
    JoinMuc { room: String, nick: String },
    SendRaw(String),
}

/// Outbound chat state types (XEP-0085)
#[derive(Debug, Clone, PartialEq)]
pub enum ChatState {
    /// Agent is generating a response (LLM call in progress)
    Composing,
    /// Agent stopped generating without sending a message (error, cancellation)
    Paused,
}

/// Reason the XMPP connection was lost.
/// Returned by `AgentRuntime::run()` so the reconnection loop
/// can decide whether to retry.
#[derive(Debug, Clone, PartialEq)]
pub enum DisconnectReason {
    /// Normal disconnection (server closed stream, network error).
    ConnectionLost,
    /// Session replaced by another client with the same resource.
    Conflict,
    /// Server sent a stream error other than conflict.
    StreamError(String),
}

/// XMPP Component (XEP-0114)
///
/// Connects to an XMPP server as an external component,
/// receives messages destined for the agent's subdomain,
/// and sends back responses.
pub struct XmppComponent {
    config: ServerConfig,
}

impl XmppComponent {
    pub fn new(config: ServerConfig) -> Self {
        Self { config }
    }

    /// Starts the connection and returns communication channels.
    ///
    /// The connection handshake is completed synchronously (awaited).
    /// If the handshake fails, an `XmppError` is returned immediately.
    /// On success, the event loop is spawned as a background task.
    ///
    /// - `event_rx`: receives XMPP events (incoming messages, etc.)
    /// - `cmd_tx`: sends commands (outgoing messages, etc.)
    pub async fn connect(
        self,
    ) -> Result<(mpsc::Receiver<XmppEvent>, mpsc::Sender<XmppCommand>), XmppError> {
        let (event_tx, event_rx) = mpsc::channel::<XmppEvent>(100);
        let (cmd_tx, cmd_rx) = mpsc::channel::<XmppCommand>(100);

        // Phase 1+2: Establish connection and complete handshake
        let (reader, writer, domain) = self.establish().await?;

        // Connection established — notify runtime
        let _ = event_tx.send(XmppEvent::Connected).await;

        // Phase 3: Spawn the event loop as a background task
        tokio::spawn(Self::run_event_loop(
            reader, writer, domain, event_tx, cmd_rx,
        ));

        Ok((event_rx, cmd_tx))
    }

    /// Establishes TCP connection and completes the XEP-0114 handshake.
    /// Returns the split stream and the component domain on success.
    async fn establish(
        &self,
    ) -> Result<(OwnedReadHalf, OwnedWriteHalf, String), XmppError> {
        let (domain, secret) = match &self.config.mode {
            ConnectionMode::Component {
                component_domain,
                component_secret,
            } => (component_domain.clone(), component_secret.clone()),
            _ => {
                return Err(XmppError::Config(
                    "XmppComponent requires component mode config".into(),
                ))
            }
        };

        let addr = format!("{}:{}", self.config.host, self.config.port);
        info!("Connecting to XMPP server at {addr}...");

        let mut stream = TcpStream::connect(&addr)
            .await
            .map_err(|e| XmppError::Transient(format!("TCP connect to {addr}: {e}")))?;
        info!("TCP connected to {addr}");

        // --- Phase 1: Stream opening ---
        let stream_open = stanzas::build_stream_open(&domain);
        stream
            .write_all(stream_open.as_bytes())
            .await
            .map_err(|e| XmppError::Transient(format!("Stream open write: {e}")))?;
        debug!("Sent stream open");

        // Read server response to get stream ID
        let mut buf = vec![0u8; 4096];
        let n = stream
            .read(&mut buf)
            .await
            .map_err(|e| XmppError::Transient(format!("Stream open read: {e}")))?;
        if n == 0 {
            return Err(XmppError::Transient(
                "Connection closed during stream open".into(),
            ));
        }
        let response = String::from_utf8_lossy(&buf[..n]).to_string();
        debug!("Server response: {response}");

        let stream_id = stanzas::extract_stream_id(&response).ok_or_else(|| {
            XmppError::Transient(format!("No stream ID in server response: {response}"))
        })?;
        info!("Got stream ID: {stream_id}");

        // --- Phase 2: Handshake (SHA-1 of stream_id + secret) ---
        let hash_input = format!("{stream_id}{secret}");
        let hash = hex::encode(Sha1::digest(hash_input.as_bytes()));
        let handshake = stanzas::build_handshake(&hash);
        stream
            .write_all(handshake.as_bytes())
            .await
            .map_err(|e| XmppError::Transient(format!("Handshake write: {e}")))?;
        debug!("Sent handshake");

        let n = stream
            .read(&mut buf)
            .await
            .map_err(|e| XmppError::Transient(format!("Handshake read: {e}")))?;
        if n == 0 {
            return Err(XmppError::Transient(
                "Connection closed during handshake".into(),
            ));
        }
        let response = String::from_utf8_lossy(&buf[..n]).to_string();

        if !stanzas::is_handshake_success(&response) {
            return Err(XmppError::Auth(format!("Handshake failed: {response}")));
        }

        info!("Connected as component: {domain}");

        let (reader, writer) = stream.into_split();
        Ok((reader, writer, domain))
    }

    /// Main read/write event loop — spawned as a background task after
    /// successful connection establishment.
    async fn run_event_loop(
        reader: OwnedReadHalf,
        writer: OwnedWriteHalf,
        domain: String,
        event_tx: mpsc::Sender<XmppEvent>,
        mut cmd_rx: mpsc::Receiver<XmppCommand>,
    ) {
        // Read task — uses quick-xml async Reader for proper XML parsing
        let event_tx_clone = event_tx.clone();
        let read_handle = tokio::spawn(async move {
            let buf_reader = tokio::io::BufReader::new(reader);
            let mut xml_reader = quick_xml::Reader::from_reader(buf_reader);
            xml_reader.config_mut().trim_text(true);
            let mut parser = StanzaParser::new();
            let mut buf = Vec::new();

            loop {
                match xml_reader.read_event_into_async(&mut buf).await {
                    Ok(Event::Eof) => {
                        warn!("XMPP connection closed by server");
                        let _ = event_tx_clone
                            .send(XmppEvent::Error("Connection closed".into()))
                            .await;
                        break;
                    }
                    Ok(event) => {
                        if let Some(stanza) = parser.feed(event) {
                            match stanza {
                                XmppStanza::Message(msg) => {
                                    debug!("Received message from {}: {}", msg.from, msg.body);
                                    let _ = event_tx_clone
                                        .send(XmppEvent::Message(msg))
                                        .await;
                                }
                                XmppStanza::Presence(pres) => {
                                    debug!(
                                        "Received presence from {}: {:?}",
                                        pres.from, pres.presence_type
                                    );
                                    let _ = event_tx_clone
                                        .send(XmppEvent::Presence(pres))
                                        .await;
                                }
                                XmppStanza::Reaction(reaction) => {
                                    debug!(
                                        "Received reaction from {}: {} on msg {}",
                                        reaction.from,
                                        reaction.emojis.join(""),
                                        reaction.message_id
                                    );
                                    let _ = event_tx_clone
                                        .send(XmppEvent::Reaction(reaction))
                                        .await;
                                }
                                XmppStanza::StreamError(condition) => {
                                    error!("Stream error received: {condition}");
                                    let _ = event_tx_clone
                                        .send(XmppEvent::StreamError(condition))
                                        .await;
                                    break;
                                }
                                XmppStanza::Ignored | XmppStanza::StreamLevel => {}
                            }
                        }
                    }
                    Err(e) => {
                        error!("XML parse error: {e}");
                        let _ = event_tx_clone
                            .send(XmppEvent::Error(format!("XML parse error: {e}")))
                            .await;
                        break;
                    }
                }
                buf.clear();
            }
        });

        // Write task — component mode includes 'from' attribute
        let write_handle = tokio::spawn(async move {
            let mut writer = writer;
            while let Some(cmd) = cmd_rx.recv().await {
                let xml = match cmd {
                    XmppCommand::SendMessage { to, body, id } => {
                        stanzas::build_message(Some(&domain), &to, &body, id.as_deref())
                    }
                    XmppCommand::SendChatState {
                        to,
                        state,
                        msg_type,
                    } => match state {
                        ChatState::Composing => {
                            stanzas::build_chat_state_composing(Some(&domain), &to, &msg_type)
                        }
                        ChatState::Paused => {
                            stanzas::build_chat_state_paused(Some(&domain), &to, &msg_type)
                        }
                    },
                    XmppCommand::SendMucMessage { to, body, id } => {
                        stanzas::build_muc_message(Some(&domain), &to, &body, id.as_deref())
                    }
                    XmppCommand::JoinMuc { room, nick } => {
                        stanzas::build_muc_join(&room, &nick, Some(&domain))
                    }
                    XmppCommand::SendRaw(raw) => raw,
                };

                if let Err(e) = writer.write_all(xml.as_bytes()).await {
                    error!("Write error: {e}");
                    break;
                }
                debug!("Sent: {xml}");
            }
        });

        tokio::select! {
            _ = read_handle => {},
            _ = write_handle => {},
        }
    }
}

// Tests for extract_presence_stanza are in stanzas.rs (shared utility).
