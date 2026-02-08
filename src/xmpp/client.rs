/// XMPP C2S (client-to-server) connection.
///
/// Connects as a regular XMPP user with SASL authentication
/// and STARTTLS, providing the same channel interface as the
/// component module.
use anyhow::{anyhow, Result};
use std::time::Duration;
use tokio::io::{split, AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tokio_native_tls::TlsConnector;
use tracing::{debug, error, info, warn};

use super::component::{XmppCommand, XmppEvent};
use super::sasl;
use super::stanzas;
use crate::config::{ConnectionMode, ServerConfig};

pub struct XmppClient {
    config: ServerConfig,
}

impl XmppClient {
    pub fn new(config: ServerConfig) -> Self {
        Self { config }
    }

    /// Same interface as XmppComponent::connect()
    pub async fn connect(
        self,
    ) -> Result<(mpsc::Receiver<XmppEvent>, mpsc::Sender<XmppCommand>)> {
        let (event_tx, event_rx) = mpsc::channel::<XmppEvent>(100);
        let (cmd_tx, cmd_rx) = mpsc::channel::<XmppCommand>(100);

        tokio::spawn(async move {
            if let Err(e) = self.run(event_tx, cmd_rx).await {
                error!("XMPP client error: {e}");
            }
        });

        Ok((event_rx, cmd_tx))
    }

    async fn run(
        &self,
        event_tx: mpsc::Sender<XmppEvent>,
        cmd_rx: mpsc::Receiver<XmppCommand>,
    ) -> Result<()> {
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
            _ => return Err(anyhow!("XmppClient requires client mode config")),
        };

        let domain = jid
            .split('@')
            .nth(1)
            .ok_or_else(|| anyhow!("Invalid JID (missing @): {jid}"))?
            .to_string();
        let username = jid.split('@').next().unwrap().to_string();

        let addr = format!("{}:{}", self.config.host, self.config.port);
        info!("Connecting to XMPP server at {addr} (C2S)...");

        let mut stream = TcpStream::connect(&addr).await?;
        info!("TCP connected to {addr}");

        // --- Phase 1: Initial stream open (plaintext) ---
        let stream_open = stanzas::build_client_stream_open(&domain);
        stream.write_all(stream_open.as_bytes()).await?;
        debug!("Sent client stream open");

        let features = read_until(&mut stream, "</stream:features>").await?;
        debug!("Stream features: {features}");

        // --- Phase 2: STARTTLS ---
        if stanzas::has_starttls(&features) {
            stream
                .write_all(stanzas::build_starttls().as_bytes())
                .await?;
            debug!("Sent STARTTLS request");

            let response = read_until(&mut stream, "/>").await?;
            if !stanzas::is_starttls_proceed(&response) {
                return Err(anyhow!("STARTTLS failed: {response}"));
            }
            debug!("STARTTLS proceed received");
        } else {
            return Err(anyhow!(
                "Server does not advertise STARTTLS — refusing plaintext auth"
            ));
        }

        // Upgrade to TLS
        let connector = native_tls::TlsConnector::builder()
            .danger_accept_invalid_certs(!tls_verify)
            .build()?;
        let connector = TlsConnector::from(connector);
        let mut tls_stream = connector.connect(&domain, stream).await?;
        info!("TLS established");

        // --- Phase 3: Re-open stream over TLS ---
        let stream_open = stanzas::build_client_stream_open(&domain);
        tls_stream.write_all(stream_open.as_bytes()).await?;

        let features = read_until(&mut tls_stream, "</stream:features>").await?;
        debug!("Post-TLS features: {features}");

        // --- Phase 4: SASL authentication ---
        let mechanisms = stanzas::extract_sasl_mechanisms(&features);
        info!("SASL mechanisms: {mechanisms:?}");

        if mechanisms.iter().any(|m| m == "SCRAM-SHA-1") {
            sasl::authenticate_scram_sha1(&mut tls_stream, &username, &password).await?;
        } else if mechanisms.iter().any(|m| m == "PLAIN") {
            sasl::authenticate_plain(&mut tls_stream, &username, &password).await?;
        } else {
            return Err(anyhow!(
                "No supported SASL mechanism (need SCRAM-SHA-1 or PLAIN)"
            ));
        }
        info!("SASL authentication successful");

        // --- Phase 5: Re-open stream after SASL ---
        let stream_open = stanzas::build_client_stream_open(&domain);
        tls_stream.write_all(stream_open.as_bytes()).await?;

        let _features = read_until(&mut tls_stream, "</stream:features>").await?;

        // --- Phase 6: Resource binding ---
        let bind_req = stanzas::build_bind_request(&resource);
        tls_stream.write_all(bind_req.as_bytes()).await?;
        debug!("Sent bind request");

        let bind_response = read_until(&mut tls_stream, "</iq>").await?;
        let full_jid = stanzas::extract_bound_jid(&bind_response)
            .ok_or_else(|| anyhow!("Failed to bind resource: {bind_response}"))?;
        info!("Bound as: {full_jid}");

        // --- Phase 7: Send initial presence ---
        tls_stream
            .write_all(stanzas::build_initial_presence().as_bytes())
            .await?;
        info!("Sent initial presence");

        let _ = event_tx.send(XmppEvent::Connected).await;

        // --- Phase 8: Main read/write loop ---
        let (reader, writer) = split(tls_stream);
        Self::run_event_loop(reader, writer, event_tx, cmd_rx).await
    }

    /// Main read/write loop — same pattern as component.rs
    async fn run_event_loop<R, W>(
        reader: R,
        writer: W,
        event_tx: mpsc::Sender<XmppEvent>,
        mut cmd_rx: mpsc::Receiver<XmppCommand>,
    ) -> Result<()>
    where
        R: AsyncReadExt + Unpin + Send + 'static,
        W: AsyncWriteExt + Unpin + Send + 'static,
    {
        // Read task
        let event_tx_clone = event_tx.clone();
        let read_handle = tokio::spawn(async move {
            let mut reader = reader;
            let mut buf = vec![0u8; 65536];
            let mut xml_buffer = String::new();

            loop {
                match reader.read(&mut buf).await {
                    Ok(0) => {
                        warn!("XMPP connection closed by server");
                        let _ = event_tx_clone
                            .send(XmppEvent::Error("Connection closed".into()))
                            .await;
                        break;
                    }
                    Ok(n) => {
                        let chunk = String::from_utf8_lossy(&buf[..n]);
                        xml_buffer.push_str(&chunk);

                        // Process all complete <message>...</message> stanzas
                        while let Some(end) = xml_buffer.find("</message>") {
                            let stanza_end = end + "</message>".len();
                            let stanza = &xml_buffer[..stanza_end];

                            if let Some(msg) = stanzas::parse_message(stanza) {
                                debug!("Received message from {}: {}", msg.from, msg.body);
                                let _ = event_tx_clone.send(XmppEvent::Message(msg)).await;
                            } else {
                                debug!("Skipping non-message stanza (chat state or no body)");
                            }

                            xml_buffer = xml_buffer[stanza_end..].to_string();
                        }
                    }
                    Err(e) => {
                        error!("Read error: {e}");
                        let _ = event_tx_clone
                            .send(XmppEvent::Error(e.to_string()))
                            .await;
                        break;
                    }
                }
            }
        });

        // Write task — C2S: no 'from' attribute (server adds it)
        let write_handle = tokio::spawn(async move {
            let mut writer = writer;
            while let Some(cmd) = cmd_rx.recv().await {
                let xml = match cmd {
                    XmppCommand::SendMessage { to, body } => {
                        stanzas::build_message(None, &to, &body, None)
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

        Ok(())
    }
}

/// Reads from the stream until `marker` appears in the accumulated data.
/// Handles the common XMPP pattern where the server sends the stream
/// header and features as separate TCP segments.
async fn read_until<S: AsyncReadExt + Unpin>(
    stream: &mut S,
    marker: &str,
) -> Result<String> {
    let mut buf = vec![0u8; 8192];
    let mut accumulated = String::new();
    let timeout = Duration::from_secs(10);

    loop {
        let read_future = stream.read(&mut buf);
        let n = match tokio::time::timeout(timeout, read_future).await {
            Ok(Ok(0)) => return Err(anyhow!("Connection closed while waiting for {marker}")),
            Ok(Ok(n)) => n,
            Ok(Err(e)) => return Err(anyhow!("Read error: {e}")),
            Err(_) => {
                return Err(anyhow!(
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
