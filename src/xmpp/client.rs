/// XMPP C2S (client-to-server) connection.
///
/// Connects as a regular XMPP user with SASL authentication
/// and STARTTLS, providing the same channel interface as the
/// component module.
use std::time::Duration;
use tokio::io::{split, AsyncReadExt, AsyncWriteExt, ReadHalf, WriteHalf};
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tokio_native_tls::TlsConnector;
use tracing::{debug, error, info, warn};

use quick_xml::events::Event;

use super::component::{ChatState, XmppCommand, XmppEvent};
use super::sasl;
use super::stanzas::{self, StanzaParser, XmppStanza};
use super::XmppError;
use crate::config::{ConnectionMode, ServerConfig};

pub struct XmppClient {
    config: ServerConfig,
    /// JIDs to subscribe to after connecting (presence whitelist)
    allowed_jids: Vec<String>,
}

impl XmppClient {
    pub fn new(config: ServerConfig) -> Self {
        Self {
            config,
            allowed_jids: Vec::new(),
        }
    }

    /// Set the list of allowed JIDs for automatic presence subscription
    pub fn with_allowed_jids(mut self, jids: Vec<String>) -> Self {
        self.allowed_jids = jids;
        self
    }

    /// Starts the connection and returns communication channels.
    ///
    /// The full connection handshake (TCP → STARTTLS → SASL → bind →
    /// roster → initial presence) is completed synchronously (awaited).
    /// If any phase fails, a classified `XmppError` is returned.
    /// On success, the event loop is spawned as a background task.
    pub async fn connect(
        self,
        read_timeout: Option<Duration>,
    ) -> Result<(mpsc::Receiver<XmppEvent>, mpsc::Sender<XmppCommand>), XmppError> {
        let (event_tx, event_rx) = mpsc::channel::<XmppEvent>(100);
        let (cmd_tx, cmd_rx) = mpsc::channel::<XmppCommand>(100);

        // Complete the full connection handshake
        let (reader, writer) = self.establish().await?;

        // Connection established — notify runtime
        let _ = event_tx.send(XmppEvent::Connected).await;

        // Spawn the event loop as a background task
        tokio::spawn(Self::run_event_loop(
            reader, writer, event_tx, cmd_rx, read_timeout,
        ));

        Ok((event_rx, cmd_tx))
    }

    /// Establishes a fully authenticated XMPP C2S connection.
    ///
    /// Phases: TCP → STARTTLS → TLS → SASL → bind → roster → presence → subscribe.
    /// Returns the split TLS reader/writer on success.
    async fn establish(
        &self,
    ) -> Result<
        (
            ReadHalf<tokio_native_tls::TlsStream<TcpStream>>,
            WriteHalf<tokio_native_tls::TlsStream<TcpStream>>,
        ),
        XmppError,
    > {
        let (jid, password, resource, tls_verify) = match &self.config.mode {
            ConnectionMode::Client {
                jid,
                password,
                resource,
                tls_verify,
            } => (
                jid.clone(),
                password.clone(),
                resource.clone(),
                *tls_verify,
            ),
            _ => {
                return Err(XmppError::Config(
                    "XmppClient requires client mode config".into(),
                ))
            }
        };

        let domain = jid
            .split('@')
            .nth(1)
            .ok_or_else(|| XmppError::Config(format!("Invalid JID (missing @): {jid}")))?
            .to_string();
        let username = jid.split('@').next().unwrap().to_string();

        let addr = format!("{}:{}", self.config.host, self.config.port);
        info!("Connecting to XMPP server at {addr} (C2S)...");

        let mut stream = TcpStream::connect(&addr)
            .await
            .map_err(|e| XmppError::Transient(format!("TCP connect to {addr}: {e}")))?;
        info!("TCP connected to {addr}");

        // --- Phase 1: Initial stream open (plaintext) ---
        let stream_open = stanzas::build_client_stream_open(&domain);
        stream
            .write_all(stream_open.as_bytes())
            .await
            .map_err(|e| XmppError::Transient(format!("Stream open write: {e}")))?;
        debug!("Sent client stream open");

        let features = read_until(&mut stream, "</stream:features>")
            .await
            .map_err(|e| XmppError::Transient(format!("Stream features read: {e}")))?;
        debug!("Stream features: {features}");

        // --- Phase 2: STARTTLS ---
        if stanzas::has_starttls(&features) {
            stream
                .write_all(stanzas::build_starttls().as_bytes())
                .await
                .map_err(|e| XmppError::Transient(format!("STARTTLS write: {e}")))?;
            debug!("Sent STARTTLS request");

            let response = read_until(&mut stream, "/>")
                .await
                .map_err(|e| XmppError::Transient(format!("STARTTLS response read: {e}")))?;
            if !stanzas::is_starttls_proceed(&response) {
                return Err(XmppError::Transient(format!(
                    "STARTTLS failed: {response}"
                )));
            }
            debug!("STARTTLS proceed received");
        } else {
            return Err(XmppError::Config(
                "Server does not advertise STARTTLS — refusing plaintext auth".into(),
            ));
        }

        // Upgrade to TLS
        let connector = native_tls::TlsConnector::builder()
            .danger_accept_invalid_certs(!tls_verify)
            .build()
            .map_err(|e| XmppError::Transient(format!("TLS connector build: {e}")))?;
        let connector = TlsConnector::from(connector);
        let mut tls_stream = connector
            .connect(&domain, stream)
            .await
            .map_err(|e| XmppError::Transient(format!("TLS handshake: {e}")))?;
        info!("TLS established");

        // --- Phase 3: Re-open stream over TLS ---
        let stream_open = stanzas::build_client_stream_open(&domain);
        tls_stream
            .write_all(stream_open.as_bytes())
            .await
            .map_err(|e| XmppError::Transient(format!("Post-TLS stream open: {e}")))?;

        let features = read_until(&mut tls_stream, "</stream:features>")
            .await
            .map_err(|e| XmppError::Transient(format!("Post-TLS features read: {e}")))?;
        debug!("Post-TLS features: {features}");

        // --- Phase 4: SASL authentication ---
        let mechanisms = stanzas::extract_sasl_mechanisms(&features);
        info!("SASL mechanisms: {mechanisms:?}");

        if mechanisms.iter().any(|m| m == "SCRAM-SHA-1") {
            sasl::authenticate_scram_sha1(&mut tls_stream, &username, &password)
                .await
                .map_err(|e| XmppError::Auth(format!("SCRAM-SHA-1 auth: {e}")))?;
        } else if mechanisms.iter().any(|m| m == "PLAIN") {
            sasl::authenticate_plain(&mut tls_stream, &username, &password)
                .await
                .map_err(|e| XmppError::Auth(format!("PLAIN auth: {e}")))?;
        } else {
            return Err(XmppError::Config(
                "No supported SASL mechanism (need SCRAM-SHA-1 or PLAIN)".into(),
            ));
        }
        info!("SASL authentication successful");

        // --- Phase 5: Re-open stream after SASL ---
        let stream_open = stanzas::build_client_stream_open(&domain);
        tls_stream
            .write_all(stream_open.as_bytes())
            .await
            .map_err(|e| XmppError::Transient(format!("Post-SASL stream open: {e}")))?;

        let _features = read_until(&mut tls_stream, "</stream:features>")
            .await
            .map_err(|e| XmppError::Transient(format!("Post-SASL features read: {e}")))?;

        // --- Phase 6: Resource binding ---
        let bind_req = stanzas::build_bind_request(&resource);
        tls_stream
            .write_all(bind_req.as_bytes())
            .await
            .map_err(|e| XmppError::Transient(format!("Bind request write: {e}")))?;
        debug!("Sent bind request");

        let bind_response = read_until(&mut tls_stream, "</iq>")
            .await
            .map_err(|e| XmppError::Transient(format!("Bind response read: {e}")))?;
        let full_jid = stanzas::extract_bound_jid(&bind_response).ok_or_else(|| {
            XmppError::Transient(format!("Failed to bind resource: {bind_response}"))
        })?;
        info!("Bound as: {full_jid}");

        // --- Phase 7: Fetch roster ---
        tls_stream
            .write_all(stanzas::build_roster_get().as_bytes())
            .await
            .map_err(|e| XmppError::Transient(format!("Roster request write: {e}")))?;
        debug!("Sent roster request");

        let roster_response = read_until(&mut tls_stream, "</iq>")
            .await
            .map_err(|e| XmppError::Transient(format!("Roster response read: {e}")))?;
        let roster_jids = stanzas::extract_roster_jids(&roster_response);
        info!("Roster contains {} contact(s)", roster_jids.len());
        for jid in &roster_jids {
            debug!("  roster: {jid}");
        }

        // --- Phase 8: Send initial presence ---
        tls_stream
            .write_all(stanzas::build_initial_presence().as_bytes())
            .await
            .map_err(|e| XmppError::Transient(format!("Initial presence write: {e}")))?;
        info!("Sent initial presence");

        // --- Phase 9: Subscribe to allowed JIDs not already in roster ---
        let mut subscribed_count = 0;
        for jid in &self.allowed_jids {
            if jid == "*" {
                continue; // wildcard is not a real JID
            }
            if roster_jids.iter().any(|r| r == jid) {
                debug!("Already in roster, skipping subscribe: {jid}");
                continue;
            }
            let subscribe = stanzas::build_subscribe(jid);
            tls_stream
                .write_all(subscribe.as_bytes())
                .await
                .map_err(|e| XmppError::Transient(format!("Subscribe write: {e}")))?;
            debug!("Sent presence subscribe to {jid}");
            subscribed_count += 1;
        }
        if subscribed_count > 0 {
            info!("Sent {subscribed_count} new presence subscription(s)");
        } else {
            info!("All allowed JIDs already in roster — no new subscriptions needed");
        }

        let (reader, writer) = split(tls_stream);
        Ok((reader, writer))
    }

    /// Main read/write loop — spawned as a background task after
    /// successful connection establishment.
    async fn run_event_loop<R, W>(
        reader: R,
        writer: W,
        event_tx: mpsc::Sender<XmppEvent>,
        mut cmd_rx: mpsc::Receiver<XmppCommand>,
        read_timeout: Option<Duration>,
    ) where
        R: AsyncReadExt + Unpin + Send + 'static,
        W: AsyncWriteExt + Unpin + Send + 'static,
    {
        // Read task — uses quick-xml async Reader for proper XML parsing
        let event_tx_clone = event_tx.clone();
        let read_handle = tokio::spawn(async move {
            let buf_reader = tokio::io::BufReader::new(reader);
            let mut xml_reader = quick_xml::Reader::from_reader(buf_reader);
            xml_reader.config_mut().trim_text(true);
            let mut parser = StanzaParser::new();
            let mut buf = Vec::new();

            loop {
                let read_result = if let Some(timeout_dur) = read_timeout {
                    match tokio::time::timeout(
                        timeout_dur,
                        xml_reader.read_event_into_async(&mut buf),
                    )
                    .await
                    {
                        Ok(result) => result,
                        Err(_elapsed) => {
                            warn!(
                                "Read timeout ({timeout_dur:?}) — connection appears dead"
                            );
                            let _ = event_tx_clone
                                .send(XmppEvent::Error(
                                    "Read timeout — connection dead".into(),
                                ))
                                .await;
                            break;
                        }
                    }
                } else {
                    xml_reader.read_event_into_async(&mut buf).await
                };

                match read_result {
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

        // Write task — C2S: no 'from' attribute (server adds it)
        let write_handle = tokio::spawn(async move {
            let mut writer = writer;
            while let Some(cmd) = cmd_rx.recv().await {
                // Handle keepalive ping separately (no XML payload)
                if matches!(cmd, XmppCommand::Ping) {
                    if let Err(e) = writer.write_all(b" ").await {
                        error!("Keepalive write error: {e}");
                        break;
                    }
                    debug!("Sent keepalive ping");
                    continue;
                }

                let xml = match cmd {
                    XmppCommand::SendMessage { to, body, id } => {
                        stanzas::build_message(None, &to, &body, id.as_deref())
                    }
                    XmppCommand::SendChatState {
                        to,
                        state,
                        msg_type,
                    } => match state {
                        ChatState::Composing => {
                            stanzas::build_chat_state_composing(None, &to, &msg_type)
                        }
                        ChatState::Paused => {
                            stanzas::build_chat_state_paused(None, &to, &msg_type)
                        }
                    },
                    XmppCommand::SendMucMessage { to, body, id } => {
                        stanzas::build_muc_message(None, &to, &body, id.as_deref())
                    }
                    XmppCommand::JoinMuc { room, nick } => {
                        stanzas::build_muc_join(&room, &nick, None)
                    }
                    XmppCommand::SendRaw(raw) => raw,
                    XmppCommand::Ping => unreachable!(),
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

/// Reads from the stream until `marker` appears in the accumulated data.
/// Handles the common XMPP pattern where the server sends the stream
/// header and features as separate TCP segments.
async fn read_until<S: AsyncReadExt + Unpin>(
    stream: &mut S,
    marker: &str,
) -> anyhow::Result<String> {
    let mut buf = vec![0u8; 8192];
    let mut accumulated = String::new();
    let timeout = Duration::from_secs(10);

    loop {
        let read_future = stream.read(&mut buf);
        let n = match tokio::time::timeout(timeout, read_future).await {
            Ok(Ok(0)) => {
                return Err(anyhow::anyhow!(
                    "Connection closed while waiting for {marker}"
                ))
            }
            Ok(Ok(n)) => n,
            Ok(Err(e)) => return Err(anyhow::anyhow!("Read error: {e}")),
            Err(_) => {
                return Err(anyhow::anyhow!(
                    "Timeout waiting for {marker} (accumulated: {accumulated})"
                ))
            }
        };

        accumulated.push_str(&String::from_utf8_lossy(&buf[..n]));

        if accumulated.contains(marker) {
            return Ok(accumulated);
        }
    }
}

// Tests for extract_presence_stanza are in stanzas.rs (shared utility).
