use anyhow::{anyhow, Result};
use sha1::{Digest, Sha1};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tracing::{debug, error, info, warn};

use super::stanzas::{self, IncomingMessage, IncomingPresence};
use crate::config::{ConnectionMode, ServerConfig};

/// Events emitted by the XMPP layer to the runtime
#[derive(Debug)]
pub enum XmppEvent {
    Connected,
    Message(IncomingMessage),
    Presence(IncomingPresence),
    Error(String),
}

/// Commands sent by the runtime to the XMPP layer
#[derive(Debug)]
pub enum XmppCommand {
    SendMessage { to: String, body: String },
    /// Send a chat state notification (XEP-0085) — composing, paused, etc.
    /// `msg_type` is `"chat"` for 1:1 or `"groupchat"` for MUC.
    SendChatState {
        to: String,
        state: ChatState,
        msg_type: String,
    },
    /// Send a groupchat message to a MUC room (XEP-0045)
    SendMucMessage { to: String, body: String },
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

/// Extracts a complete presence stanza from the buffer.
/// Handles both self-closing `<presence ... />` and `<presence>...</presence>`.
/// Returns (stanza_text, end_position) or None.
fn extract_presence_stanza(buffer: &str) -> Option<(String, usize)> {
    let start = buffer.find("<presence")?;
    let after_tag = &buffer[start..];

    // Check for self-closing first: <presence ... />
    // A self-closing tag has /> before any > that opens the tag body.
    // e.g. <presence from='u@l' type='subscribe'/>
    // vs   <presence from='u@l'><x xmlns='muc'/></presence>
    if let Some(close_pos) = after_tag.find("/>") {
        // Check if there's a plain '>' before the '/>' — that would mean the
        // <presence> tag body was opened and the /> belongs to a child element.
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

    /// Starts the connection and returns communication channels
    ///
    /// - `event_rx`: receives XMPP events (incoming messages, etc.)
    /// - `cmd_tx`: sends commands (outgoing messages, etc.)
    pub async fn connect(
        self,
    ) -> Result<(mpsc::Receiver<XmppEvent>, mpsc::Sender<XmppCommand>)> {
        let (event_tx, event_rx) = mpsc::channel::<XmppEvent>(100);
        let (cmd_tx, cmd_rx) = mpsc::channel::<XmppCommand>(100);

        tokio::spawn(async move {
            if let Err(e) = self.run(event_tx, cmd_rx).await {
                error!("XMPP component error: {e}");
            }
        });

        Ok((event_rx, cmd_tx))
    }

    async fn run(
        &self,
        event_tx: mpsc::Sender<XmppEvent>,
        mut cmd_rx: mpsc::Receiver<XmppCommand>,
    ) -> Result<()> {
        let (domain, secret) = match &self.config.mode {
            ConnectionMode::Component {
                component_domain,
                component_secret,
            } => (component_domain.clone(), component_secret.clone()),
            _ => return Err(anyhow!("XmppComponent requires component mode config")),
        };

        let addr = format!("{}:{}", self.config.host, self.config.port);
        info!("Connecting to XMPP server at {addr}...");

        let mut stream = TcpStream::connect(&addr).await?;
        info!("TCP connected to {addr}");

        // --- Phase 1: Stream opening ---
        let stream_open = stanzas::build_stream_open(&domain);
        stream.write_all(stream_open.as_bytes()).await?;
        debug!("Sent stream open");

        // Read server response to get stream ID
        let mut buf = vec![0u8; 4096];
        let n = stream.read(&mut buf).await?;
        let response = String::from_utf8_lossy(&buf[..n]).to_string();
        debug!("Server response: {response}");

        let stream_id = stanzas::extract_stream_id(&response)
            .ok_or_else(|| anyhow!("No stream ID in server response"))?;
        info!("Got stream ID: {stream_id}");

        // --- Phase 2: Handshake (SHA-1 of stream_id + secret) ---
        let hash_input = format!("{stream_id}{secret}");
        let hash = hex::encode(Sha1::digest(hash_input.as_bytes()));
        let handshake = stanzas::build_handshake(&hash);
        stream.write_all(handshake.as_bytes()).await?;
        debug!("Sent handshake");

        let n = stream.read(&mut buf).await?;
        let response = String::from_utf8_lossy(&buf[..n]).to_string();

        if !stanzas::is_handshake_success(&response) {
            return Err(anyhow!("Handshake failed: {response}"));
        }

        info!("Connected as component: {domain}");
        let _ = event_tx.send(XmppEvent::Connected).await;

        // --- Phase 3: Main loop — concurrent read/write ---
        let (mut reader, mut writer) = stream.into_split();

        // Read task
        let event_tx_clone = event_tx.clone();
        let read_handle = tokio::spawn(async move {
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

                        // Process all complete presence stanzas
                        while let Some(presence) = extract_presence_stanza(&xml_buffer) {
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

        // Write task — component mode includes 'from' attribute
        let write_handle = tokio::spawn(async move {
            while let Some(cmd) = cmd_rx.recv().await {
                let xml = match cmd {
                    XmppCommand::SendMessage { to, body } => {
                        stanzas::build_message(Some(&domain), &to, &body, None)
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
                    XmppCommand::SendMucMessage { to, body } => {
                        stanzas::build_muc_message(Some(&domain), &to, &body)
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

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── extract_presence_stanza tests ──────────────────

    #[test]
    fn test_extract_presence_self_closing() {
        let buf = "<presence from='room@conf/nick' type='available'/>";
        let (stanza, end) = extract_presence_stanza(buf).unwrap();
        assert_eq!(stanza, buf);
        assert_eq!(end, buf.len());
    }

    #[test]
    fn test_extract_presence_full_closing() {
        let buf = "<presence from='room@conf/nick'><x xmlns='http://jabber.org/protocol/muc'/></presence>";
        let (stanza, end) = extract_presence_stanza(buf).unwrap();
        assert_eq!(stanza, buf);
        assert_eq!(end, buf.len());
    }

    #[test]
    fn test_extract_presence_incomplete() {
        let buf = "<presence from='room@conf/nick' type='avail";
        assert!(extract_presence_stanza(buf).is_none());
    }

    #[test]
    fn test_extract_presence_with_trailing_data() {
        let buf = "<presence from='u@l' type='available'/><message from='u@l'><body>Hi</body></message>";
        let (stanza, end) = extract_presence_stanza(buf).unwrap();
        assert_eq!(stanza, "<presence from='u@l' type='available'/>");
        assert!(end < buf.len());
    }

    #[test]
    fn test_extract_presence_muc_join_with_children() {
        // MUC presence has child elements with self-closing tags — the /> belongs to <x/>, not <presence>
        let buf = "<presence from='room@conf/nick'><x xmlns='http://jabber.org/protocol/muc#user'><item affiliation='member' role='participant'/></x></presence>";
        let (stanza, end) = extract_presence_stanza(buf).unwrap();
        assert_eq!(stanza, buf);
        assert_eq!(end, buf.len());
    }

    #[test]
    fn test_extract_presence_no_presence() {
        let buf = "<message from='user@localhost'><body>Hi</body></message>";
        assert!(extract_presence_stanza(buf).is_none());
    }

    #[test]
    fn test_extract_presence_empty() {
        assert!(extract_presence_stanza("").is_none());
    }
}
