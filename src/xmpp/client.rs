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

use super::component::{ChatState, XmppCommand, XmppEvent};
use super::sasl;
use super::stanzas;
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

        // --- Phase 7: Fetch roster ---
        tls_stream
            .write_all(stanzas::build_roster_get().as_bytes())
            .await?;
        debug!("Sent roster request");

        let roster_response = read_until(&mut tls_stream, "</iq>").await?;
        let roster_jids = stanzas::extract_roster_jids(&roster_response);
        info!("Roster contains {} contact(s)", roster_jids.len());
        for jid in &roster_jids {
            debug!("  roster: {jid}");
        }

        // --- Phase 8: Send initial presence ---
        tls_stream
            .write_all(stanzas::build_initial_presence().as_bytes())
            .await?;
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
            tls_stream.write_all(subscribe.as_bytes()).await?;
            debug!("Sent presence subscribe to {jid}");
            subscribed_count += 1;
        }
        if subscribed_count > 0 {
            info!("Sent {subscribed_count} new presence subscription(s)");
        } else {
            info!("All allowed JIDs already in roster — no new subscriptions needed");
        }

        let _ = event_tx.send(XmppEvent::Connected).await;

        // --- Phase 10: Main read/write loop ---
        let allowed_jids = self.allowed_jids.clone();
        let (reader, writer) = split(tls_stream);
        Self::run_event_loop(reader, writer, event_tx, cmd_rx, allowed_jids).await
    }

    /// Main read/write loop — same pattern as component.rs
    async fn run_event_loop<R, W>(
        reader: R,
        writer: W,
        event_tx: mpsc::Sender<XmppEvent>,
        mut cmd_rx: mpsc::Receiver<XmppCommand>,
        allowed_jids: Vec<String>,
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

                        // Process all complete <presence ... /> or <presence>...</presence> stanzas
                        while let Some(presence) = Self::extract_presence(&xml_buffer) {
                            let (stanza, stanza_end) = presence;

                            if let Some(pres) = stanzas::parse_presence(&stanza) {
                                debug!("Received presence from {}: {:?}", pres.from, pres.presence_type);
                                let _ = event_tx_clone.send(XmppEvent::Presence(pres)).await;
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
        let _allowed_jids = allowed_jids;
        let write_handle = tokio::spawn(async move {
            let mut writer = writer;
            while let Some(cmd) = cmd_rx.recv().await {
                let xml = match cmd {
                    XmppCommand::SendMessage { to, body } => {
                        stanzas::build_message(None, &to, &body, None)
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
                    XmppCommand::SendMucMessage { to, body } => {
                        stanzas::build_muc_message(None, &to, &body)
                    }
                    XmppCommand::JoinMuc { room, nick } => {
                        stanzas::build_muc_join(&room, &nick, None)
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

impl XmppClient {
    /// Extracts a complete presence stanza from the buffer.
    /// Handles both self-closing `<presence ... />` and `<presence>...</presence>`.
    /// Returns (stanza_text, end_position) or None.
    fn extract_presence(buffer: &str) -> Option<(String, usize)> {
        let start = buffer.find("<presence")?;
        let after_tag = &buffer[start..];

        // Check for self-closing first: <presence ... />
        // A self-closing tag has /> before any > that opens the tag body.
        if let Some(close_pos) = after_tag.find("/>") {
            let before_close = &after_tag[..close_pos];
            let tag_opened = before_close
                .find('>')
                .map(|pos| !before_close[..pos + 1].ends_with("/>"))
                .unwrap_or(false);
            if !tag_opened {
                let stanza_end = start + close_pos + "/>".len();
                return Some((buffer[start..stanza_end].to_string(), stanza_end));
            }
        }

        // Full closing tag: <presence>...</presence>
        if let Some(close_pos) = after_tag.find("</presence>") {
            let stanza_end = start + close_pos + "</presence>".len();
            return Some((buffer[start..stanza_end].to_string(), stanza_end));
        }

        None // incomplete stanza
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

#[cfg(test)]
mod tests {
    use super::*;

    // ── extract_presence tests ──────────────────────────

    #[test]
    fn test_extract_presence_self_closing() {
        let buf = "<presence from='user@localhost' type='subscribe'/>";
        let (stanza, end) = XmppClient::extract_presence(buf).unwrap();
        assert_eq!(stanza, buf);
        assert_eq!(end, buf.len());
    }

    #[test]
    fn test_extract_presence_full_closing_tag() {
        let buf = "<presence from='user@localhost/res'><show>chat</show></presence>";
        let (stanza, end) = XmppClient::extract_presence(buf).unwrap();
        assert_eq!(stanza, buf);
        assert_eq!(end, buf.len());
    }

    #[test]
    fn test_extract_presence_with_trailing_data() {
        let buf = "<presence from='u@l' type='subscribed'/>some trailing data";
        let (stanza, end) = XmppClient::extract_presence(buf).unwrap();
        assert_eq!(stanza, "<presence from='u@l' type='subscribed'/>");
        assert!(end < buf.len()); // trailing data is not consumed
    }

    #[test]
    fn test_extract_presence_incomplete_returns_none() {
        let buf = "<presence from='user@localhost' type='sub";
        assert!(XmppClient::extract_presence(buf).is_none());
    }

    #[test]
    fn test_extract_presence_no_presence_returns_none() {
        let buf = "<message from='user@localhost'><body>Hi</body></message>";
        assert!(XmppClient::extract_presence(buf).is_none());
    }

    #[test]
    fn test_extract_presence_preceded_by_other_stanzas() {
        let buf = "<iq type='result'/><presence from='u@l' type='available'/>";
        let (stanza, _end) = XmppClient::extract_presence(buf).unwrap();
        assert_eq!(stanza, "<presence from='u@l' type='available'/>");
    }

    #[test]
    fn test_extract_presence_full_with_child_self_closing() {
        // A presence with <show/> inside — the /> belongs to <show/>, not <presence>
        let buf = "<presence from='u@l'><show>away</show><status>BRB</status></presence>";
        let (stanza, end) = XmppClient::extract_presence(buf).unwrap();
        assert_eq!(stanza, buf);
        assert_eq!(end, buf.len());
    }

    #[test]
    fn test_extract_presence_empty_buffer() {
        assert!(XmppClient::extract_presence("").is_none());
    }
}
