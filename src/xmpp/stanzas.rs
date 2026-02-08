/// XMPP stanza building and parsing.
/// Uses quick-xml for XML parsing and escaping.
use std::borrow::Cow;

use quick_xml::escape::escape;

/// Message type — distinguishes 1:1 chat from MUC groupchat
#[derive(Debug, Clone, PartialEq)]
pub enum MessageType {
    Chat,
    GroupChat,
}

/// Out-of-Band Data (XEP-0066) — file attachment URL
#[derive(Debug, Clone)]
pub struct OobData {
    pub url: String,
    pub desc: Option<String>,
}

/// Parsed incoming message
#[derive(Debug, Clone)]
pub struct IncomingMessage {
    pub from: String,
    pub to: String,
    pub body: String,
    pub id: Option<String>,
    pub message_type: MessageType,
    /// Out-of-Band Data (XEP-0066) — file attachment URLs from HTTP Upload.
    /// A single message can contain multiple OOB elements (multiple files).
    pub oob: Vec<OobData>,
}

// ── XML escaping helpers ─────────────────────────────────

/// Escape a value for use inside an XML attribute delimited by single quotes.
fn escape_attr(value: &str) -> Cow<'_, str> {
    escape(value)
}

// ── Message stanzas (shared) ─────────────────────────────

/// Builds an outgoing XMPP message.
/// `from` is Some for component mode, None for C2S (server adds it).
/// Includes `<active/>` chat state (XEP-0085) to signal we've stopped typing.
pub fn build_message(from: Option<&str>, to: &str, body: &str, id: Option<&str>) -> String {
    let from_attr = from
        .map(|f| format!(" from='{}'", escape_attr(f)))
        .unwrap_or_default();
    let id_attr = id
        .map(|i| format!(" id='{}'", escape_attr(i)))
        .unwrap_or_default();
    let to = escape_attr(to);
    let body = escape(body);
    format!(
        "<message{from_attr} to='{to}' type='chat'{id_attr}>\
         <body>{body}</body>\
         <active xmlns='http://jabber.org/protocol/chatstates'/>\
         </message>"
    )
}

// ── Chat state notifications (XEP-0085, outbound) ────────

/// Builds a standalone `<composing/>` chat state notification.
/// Sent when the agent starts generating a response (LLM call begins).
/// `from` is Some for component mode, None for C2S.
/// `msg_type` is `"chat"` for 1:1 or `"groupchat"` for MUC.
pub fn build_chat_state_composing(from: Option<&str>, to: &str, msg_type: &str) -> String {
    let from_attr = from
        .map(|f| format!(" from='{}'", escape_attr(f)))
        .unwrap_or_default();
    let to = escape_attr(to);
    let msg_type = escape_attr(msg_type);
    format!(
        "<message{from_attr} to='{to}' type='{msg_type}'>\
         <composing xmlns='http://jabber.org/protocol/chatstates'/>\
         </message>"
    )
}

/// Builds a standalone `<paused/>` chat state notification.
/// Sent when the agent stops generating without sending a message
/// (e.g., error during LLM call, or cancelled request).
/// `from` is Some for component mode, None for C2S.
/// `msg_type` is `"chat"` for 1:1 or `"groupchat"` for MUC.
pub fn build_chat_state_paused(from: Option<&str>, to: &str, msg_type: &str) -> String {
    let from_attr = from
        .map(|f| format!(" from='{}'", escape_attr(f)))
        .unwrap_or_default();
    let to = escape_attr(to);
    let msg_type = escape_attr(msg_type);
    format!(
        "<message{from_attr} to='{to}' type='{msg_type}'>\
         <paused xmlns='http://jabber.org/protocol/chatstates'/>\
         </message>"
    )
}

// ── Component protocol (XEP-0114) ────────────────────────

/// Builds the stream opening for component protocol
pub fn build_stream_open(domain: &str) -> String {
    let domain = escape_attr(domain);
    format!(
        "<?xml version='1.0'?>\
         <stream:stream \
         xmlns='jabber:component:accept' \
         xmlns:stream='http://etherx.jabber.org/streams' \
         to='{domain}'>"
    )
}

/// Builds the handshake response with SHA-1 hash
pub fn build_handshake(hash: &str) -> String {
    format!("<handshake>{hash}</handshake>")
}

/// Detects if XML contains a successful handshake (empty response)
pub fn is_handshake_success(data: &str) -> bool {
    data.contains("<handshake/>") || data.contains("<handshake></handshake>")
}

// ── C2S client protocol ──────────────────────────────────

/// Builds the stream opening for C2S client protocol
pub fn build_client_stream_open(domain: &str) -> String {
    let domain = escape_attr(domain);
    format!(
        "<?xml version='1.0'?>\
         <stream:stream \
         xmlns='jabber:client' \
         xmlns:stream='http://etherx.jabber.org/streams' \
         to='{domain}' \
         version='1.0'>"
    )
}

/// STARTTLS request
pub fn build_starttls() -> String {
    "<starttls xmlns='urn:ietf:params:xml:ns:xmpp-tls'/>".to_string()
}

/// Checks if server responded with STARTTLS proceed
pub fn is_starttls_proceed(data: &str) -> bool {
    data.contains("<proceed")
}

/// Checks if stream features advertise STARTTLS
pub fn has_starttls(data: &str) -> bool {
    data.contains("<starttls")
}

/// SASL auth with PLAIN mechanism
pub fn build_sasl_auth_plain(username: &str, password: &str) -> String {
    use base64::Engine;
    // PLAIN format: \0username\0password
    let payload = format!("\0{username}\0{password}");
    let encoded = base64::engine::general_purpose::STANDARD.encode(payload.as_bytes());
    format!("<auth xmlns='urn:ietf:params:xml:ns:xmpp-sasl' mechanism='PLAIN'>{encoded}</auth>")
}

/// SASL auth initiation with SCRAM-SHA-1 mechanism
pub fn build_sasl_auth_scram_sha1(initial_message_b64: &str) -> String {
    format!(
        "<auth xmlns='urn:ietf:params:xml:ns:xmpp-sasl' mechanism='SCRAM-SHA-1'>{initial_message_b64}</auth>"
    )
}

/// SASL challenge response
pub fn build_sasl_response(payload_b64: &str) -> String {
    format!("<response xmlns='urn:ietf:params:xml:ns:xmpp-sasl'>{payload_b64}</response>")
}

/// Checks for SASL success
pub fn is_sasl_success(data: &str) -> bool {
    data.contains("<success")
}

/// Checks for SASL challenge
pub fn is_sasl_challenge(data: &str) -> bool {
    data.contains("<challenge")
}

/// Extracts SASL challenge payload (base64 content between tags)
pub fn extract_sasl_challenge(data: &str) -> Option<String> {
    // Handle <challenge xmlns='...'>payload</challenge>
    let start_tag = "<challenge";
    let start_pos = data.find(start_tag)?;
    let after_tag = &data[start_pos..];
    // Find the closing > of the opening tag
    let content_start = after_tag.find('>')? + 1;
    let content = &after_tag[content_start..];
    let end = content.find("</challenge>")?;
    let payload = &content[..end];
    if payload.is_empty() {
        None
    } else {
        Some(payload.to_string())
    }
}

/// Extracts available SASL mechanisms from stream features
pub fn extract_sasl_mechanisms(data: &str) -> Vec<String> {
    let mut mechs = Vec::new();
    let mut search_from = 0;
    while let Some(start) = data[search_from..].find("<mechanism>") {
        let content_start = search_from + start + "<mechanism>".len();
        if let Some(end) = data[content_start..].find("</mechanism>") {
            mechs.push(data[content_start..content_start + end].to_string());
            search_from = content_start + end + "</mechanism>".len();
        } else {
            break;
        }
    }
    mechs
}

/// Resource binding request
pub fn build_bind_request(resource: &str) -> String {
    let resource = escape(resource);
    format!(
        "<iq type='set' id='bind1'>\
         <bind xmlns='urn:ietf:params:xml:ns:xmpp-bind'>\
         <resource>{resource}</resource>\
         </bind></iq>"
    )
}

/// Extracts bound JID from bind response
pub fn extract_bound_jid(data: &str) -> Option<String> {
    extract_element_text(data, "jid")
}

/// Initial presence stanza
pub fn build_initial_presence() -> String {
    "<presence/>".to_string()
}

/// Presence subscription request — ask to see the contact's presence
pub fn build_subscribe(to: &str) -> String {
    let to = escape_attr(to);
    format!("<presence to='{to}' type='subscribe'/>")
}

/// Accept an incoming subscription request — allow the contact to see our presence
pub fn build_subscribed(to: &str) -> String {
    let to = escape_attr(to);
    format!("<presence to='{to}' type='subscribed'/>")
}

// ── Presence parsing ────────────────────────────────────

/// The type of an incoming presence stanza
#[derive(Debug, Clone, PartialEq)]
pub enum PresenceType {
    /// Contact wants to subscribe to our presence
    Subscribe,
    /// Contact approved our subscription request
    Subscribed,
    /// Contact is unsubscribing from our presence
    Unsubscribe,
    /// Contact revoked our subscription
    Unsubscribed,
    /// Contact went offline
    Unavailable,
    /// Contact is available (default / no type attribute)
    Available,
}

/// Parsed incoming presence stanza
#[derive(Debug, Clone)]
pub struct IncomingPresence {
    pub from: String,
    pub presence_type: PresenceType,
}

// ── MUC (XEP-0045) ──────────────────────────────────────

/// Builds a MUC join presence stanza (XEP-0045).
/// `from` is Some for component mode, None for C2S.
pub fn build_muc_join(room_jid: &str, nick: &str, from: Option<&str>) -> String {
    let from_attr = from
        .map(|f| format!(" from='{}'", escape_attr(f)))
        .unwrap_or_default();
    let room_jid = escape_attr(room_jid);
    let nick = escape_attr(nick);
    // Request zero history — we persist messages ourselves.
    // Without this the server replays the last N messages on every
    // reconnect, creating duplicates in our memory store.
    format!(
        "<presence{from_attr} to='{room_jid}/{nick}'>\
         <x xmlns='http://jabber.org/protocol/muc'>\
         <history maxstanzas='0'/>\
         </x>\
         </presence>"
    )
}

/// Builds a groupchat message for a MUC room (XEP-0045).
/// `from` is Some for component mode, None for C2S.
/// Includes `<active/>` chat state (XEP-0085) to clear the typing indicator.
pub fn build_muc_message(from: Option<&str>, to: &str, body: &str) -> String {
    let from_attr = from
        .map(|f| format!(" from='{}'", escape_attr(f)))
        .unwrap_or_default();
    let to = escape_attr(to);
    let body = escape(body);
    format!(
        "<message{from_attr} to='{to}' type='groupchat'>\
         <body>{body}</body>\
         <active xmlns='http://jabber.org/protocol/chatstates'/>\
         </message>"
    )
}

// ── Roster (RFC 6121) ───────────────────────────────────

/// Roster query request — fetch the bot's contact list
pub fn build_roster_get() -> String {
    "<iq type='get' id='roster1'><query xmlns='jabber:iq:roster'/></iq>".to_string()
}

/// Extracts bare JIDs from a roster result.
/// Parses `<item jid='user@domain' .../>` elements inside `<query xmlns='jabber:iq:roster'>`.
/// Returns the set of bare JIDs currently in the roster.
pub fn extract_roster_jids(data: &str) -> Vec<String> {
    let mut jids = Vec::new();
    let mut search_from = 0;

    // Look for <item elements inside the roster query
    while let Some(pos) = data[search_from..].find("<item ") {
        let item_start = search_from + pos;
        let item_data = &data[item_start..];

        // Extract the jid attribute from this <item>
        if let Some(jid) = extract_attr(item_data, "jid") {
            // Only include items that aren't in "remove" subscription state
            let subscription = extract_attr(item_data, "subscription");
            if subscription.as_deref() != Some("remove") {
                jids.push(jid);
            }
        }

        search_from = item_start + "<item ".len();
    }

    jids
}

// ── Shared parsing helpers ───────────────────────────────

/// Extracts stream id from server response
pub fn extract_stream_id(data: &str) -> Option<String> {
    extract_attr(data, "id")
}

// ── Event-based stanza parser (quick-xml) ────────────────

use quick_xml::events::Event;

/// Result of parsing a complete XMPP stanza from the event stream.
#[derive(Debug)]
pub enum XmppStanza {
    Message(IncomingMessage),
    Presence(IncomingPresence),
    StreamError(String),
    /// IQ, SM ack/req, or any other stanza we don't process
    Ignored,
    /// Stream-level elements: `<stream:stream>`, `<?xml?>`, `</stream:stream>`
    StreamLevel,
}

/// Accumulated child element data during stanza parsing.
#[derive(Debug, Default)]
struct ChildElement {
    name: String,
    namespace: Option<String>,
    text: String,
    children: Vec<ChildElement>,
}

/// Accumulates events for a single top-level stanza.
#[derive(Debug, Default)]
struct StanzaBuilder {
    root_name: String,
    root_attrs: Vec<(String, String)>,
    /// Stack of child elements; last element is the one currently being built.
    /// When a child End is seen, it's popped and appended to the parent.
    child_stack: Vec<ChildElement>,
    /// Completed top-level children (popped from stack when depth returns to stanza root).
    children: Vec<ChildElement>,
    /// Direct text content of the root element (rare, used for e.g. `<handshake>hash</handshake>`)
    root_text: String,
}

impl StanzaBuilder {
    fn get_root_attr(&self, name: &str) -> Option<&str> {
        self.root_attrs
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    }

    /// Find a direct child by local name.
    fn find_child(&self, name: &str) -> Option<&ChildElement> {
        self.children.iter().find(|c| c.name == name)
    }

    /// Collect all direct children matching name + xmlns.
    fn find_children_ns(&self, name: &str, ns: &str) -> Vec<&ChildElement> {
        self.children
            .iter()
            .filter(|c| c.name == name && c.namespace.as_deref() == Some(ns))
            .collect()
    }

    /// Check if any direct child has a given local name (used for chat state detection).
    fn has_child_with_name(&self, names: &[&str]) -> bool {
        self.children.iter().any(|c| names.contains(&c.name.as_str()))
    }
}

impl ChildElement {
    fn find_child(&self, name: &str) -> Option<&ChildElement> {
        self.children.iter().find(|c| c.name == name)
    }
}

/// Parser state machine.
#[derive(Debug, Clone, Copy, PartialEq)]
enum ParserState {
    /// Waiting for a stanza to start (between stanzas, or before stream open).
    Idle,
    /// Inside a top-level stanza, collecting events.
    InStanza,
}

/// Event-driven XMPP stanza parser.
///
/// Feed quick-xml `Event`s into `feed()`. When a complete top-level stanza
/// has been collected, `feed()` returns `Some(XmppStanza)`.
///
/// Handles the XMPP stream wrapper: `<stream:stream>` sets a depth offset
/// so that stanzas (depth-1 children of the stream) are recognized correctly.
pub struct StanzaParser {
    depth: u32,
    /// Depth at which stanzas live (0 before stream:stream, 1 after).
    stream_depth: u32,
    state: ParserState,
    builder: StanzaBuilder,
}

impl StanzaParser {
    pub fn new() -> Self {
        Self {
            depth: 0,
            stream_depth: 0,
            state: ParserState::Idle,
            builder: StanzaBuilder::default(),
        }
    }

    /// Feed a quick-xml event into the parser.
    /// Returns `Some(XmppStanza)` when a complete stanza has been parsed.
    pub fn feed(&mut self, event: Event<'_>) -> Option<XmppStanza> {
        match event {
            Event::Decl(_) | Event::PI(_) | Event::Comment(_) | Event::DocType(_) => {
                // Stream-level noise — ignore
                None
            }
            Event::Start(ref e) => {
                let name = local_name_str(e.name().as_ref());

                // Handle stream:stream wrapper
                if self.state == ParserState::Idle
                    && (name == "stream" || e.name().as_ref() == b"stream:stream")
                {
                    self.depth += 1;
                    self.stream_depth = self.depth;
                    return Some(XmppStanza::StreamLevel);
                }

                self.depth += 1;

                if self.state == ParserState::Idle && self.depth == self.stream_depth + 1 {
                    // Start of a new stanza
                    self.state = ParserState::InStanza;
                    self.builder = StanzaBuilder::default();
                    self.builder.root_name = name;
                    self.builder.root_attrs = extract_attrs_from_event(e);
                } else if self.state == ParserState::InStanza {
                    // Child element start
                    let child = ChildElement {
                        name,
                        namespace: extract_xmlns_from_event(e),
                        ..Default::default()
                    };
                    self.builder.child_stack.push(child);
                }
                None
            }
            Event::Empty(ref e) => {
                let name = local_name_str(e.name().as_ref());

                if self.state == ParserState::Idle && self.depth == self.stream_depth {
                    // Self-closing top-level stanza (e.g. <r/>, <presence ... />)
                    let builder = StanzaBuilder {
                        root_name: name,
                        root_attrs: extract_attrs_from_event(e),
                        ..Default::default()
                    };
                    return Some(finalize_stanza(&builder));
                } else if self.state == ParserState::InStanza {
                    // Self-closing child element
                    let child = ChildElement {
                        name,
                        namespace: extract_xmlns_from_event(e),
                        ..Default::default()
                    };
                    // Add to parent or to top-level children
                    if let Some(parent) = self.builder.child_stack.last_mut() {
                        parent.children.push(child);
                    } else {
                        self.builder.children.push(child);
                    }
                }
                None
            }
            Event::Text(ref e) => {
                if self.state == ParserState::InStanza {
                    let text = e.unescape().unwrap_or_default();
                    if let Some(current) = self.builder.child_stack.last_mut() {
                        current.text.push_str(&text);
                    } else {
                        self.builder.root_text.push_str(&text);
                    }
                }
                None
            }
            Event::CData(ref e) => {
                if self.state == ParserState::InStanza {
                    if let Ok(text) = std::str::from_utf8(e.as_ref()) {
                        if let Some(current) = self.builder.child_stack.last_mut() {
                            current.text.push_str(text);
                        } else {
                            self.builder.root_text.push_str(text);
                        }
                    }
                }
                None
            }
            Event::End(ref e) => {
                let name = local_name_str(e.name().as_ref());

                // Handle </stream:stream>
                if (name == "stream" || e.name().as_ref() == b"stream:stream")
                    && self.depth == self.stream_depth
                {
                    self.depth = self.depth.saturating_sub(1);
                    self.stream_depth = 0;
                    self.state = ParserState::Idle;
                    return Some(XmppStanza::StreamLevel);
                }

                self.depth = self.depth.saturating_sub(1);

                if self.state == ParserState::InStanza {
                    if self.depth == self.stream_depth {
                        // Stanza complete
                        self.state = ParserState::Idle;
                        return Some(finalize_stanza(&self.builder));
                    } else {
                        // Child element closed — pop from stack
                        if let Some(child) = self.builder.child_stack.pop() {
                            if let Some(parent) = self.builder.child_stack.last_mut() {
                                parent.children.push(child);
                            } else {
                                self.builder.children.push(child);
                            }
                        }
                    }
                }
                None
            }
            Event::Eof => None,
        }
    }
}

/// Extract the local name as a String from raw bytes.
fn local_name_str(name_bytes: &[u8]) -> String {
    let full = std::str::from_utf8(name_bytes).unwrap_or("");
    // Strip namespace prefix if present (e.g. "stream:stream" -> "stream")
    // But keep full qualified name for stream:stream detection
    full.to_string()
}

/// Extract all attributes from a BytesStart event as (key, value) pairs.
fn extract_attrs_from_event(e: &quick_xml::events::BytesStart<'_>) -> Vec<(String, String)> {
    e.attributes()
        .filter_map(|a| a.ok())
        .map(|a| {
            let key = std::str::from_utf8(a.key.as_ref())
                .unwrap_or("")
                .to_string();
            let value = a
                .unescape_value()
                .unwrap_or_default()
                .into_owned();
            (key, value)
        })
        .collect()
}

/// Extract the `xmlns` attribute from a BytesStart event, if present.
fn extract_xmlns_from_event(e: &quick_xml::events::BytesStart<'_>) -> Option<String> {
    e.try_get_attribute(b"xmlns")
        .ok()
        .flatten()
        .and_then(|a| a.unescape_value().ok().map(|v| v.into_owned()))
}

/// Convert a completed StanzaBuilder into an XmppStanza.
fn finalize_stanza(builder: &StanzaBuilder) -> XmppStanza {
    match builder.root_name.as_str() {
        "message" => finalize_message(builder),
        "presence" => finalize_presence(builder),
        "stream:error" => finalize_stream_error(builder),
        _ => XmppStanza::Ignored,
    }
}

fn finalize_message(builder: &StanzaBuilder) -> XmppStanza {
    let from = match builder.get_root_attr("from") {
        Some(f) => f.to_string(),
        None => return XmppStanza::Ignored,
    };
    let to = builder.get_root_attr("to").unwrap_or("").to_string();
    let id = builder.get_root_attr("id").map(String::from);

    let message_type = match builder.get_root_attr("type") {
        Some("groupchat") => MessageType::GroupChat,
        _ => MessageType::Chat,
    };

    // Check for chat state notification (XEP-0085): composing/paused/active/inactive/gone
    let chat_state_names = ["composing", "paused", "active", "inactive", "gone"];
    let has_chat_state = builder.has_child_with_name(&chat_state_names);

    // Extract body text
    let body_raw = builder
        .find_child("body")
        .map(|c| c.text.clone())
        .unwrap_or_default();

    let has_body = !body_raw.is_empty();

    // If chat state only (no body), skip
    if has_chat_state && !has_body {
        return XmppStanza::Ignored;
    }

    // Parse OOB data (XEP-0066)
    let oob_elements = builder.find_children_ns("x", "jabber:x:oob");
    let oob: Vec<OobData> = oob_elements
        .into_iter()
        .filter_map(|x| {
            let url = x.find_child("url").map(|u| u.text.clone())?;
            if url.is_empty() {
                return None;
            }
            let desc = x
                .find_child("desc")
                .map(|d| d.text.clone())
                .filter(|d| !d.is_empty());
            Some(OobData { url, desc })
        })
        .collect();

    // OOB body fallback: if body == one of the OOB URLs, strip it
    let body = if !oob.is_empty() && oob.iter().any(|o| body_raw.trim() == o.url) {
        String::new()
    } else {
        body_raw.trim().to_string()
    };

    // Skip if no body and no OOB
    if body.is_empty() && oob.is_empty() {
        return XmppStanza::Ignored;
    }

    XmppStanza::Message(IncomingMessage {
        from,
        to,
        body,
        id,
        message_type,
        oob,
    })
}

fn finalize_presence(builder: &StanzaBuilder) -> XmppStanza {
    let from = match builder.get_root_attr("from") {
        Some(f) => f.to_string(),
        None => return XmppStanza::Ignored,
    };

    let presence_type = match builder.get_root_attr("type") {
        Some("subscribe") => PresenceType::Subscribe,
        Some("subscribed") => PresenceType::Subscribed,
        Some("unsubscribe") => PresenceType::Unsubscribe,
        Some("unsubscribed") => PresenceType::Unsubscribed,
        Some("unavailable") => PresenceType::Unavailable,
        _ => PresenceType::Available,
    };

    XmppStanza::Presence(IncomingPresence {
        from,
        presence_type,
    })
}

fn finalize_stream_error(builder: &StanzaBuilder) -> XmppStanza {
    // XMPP stream errors contain a child element from urn:ietf:params:xml:ns:xmpp-streams
    let conditions = [
        "bad-format",
        "bad-namespace-prefix",
        "conflict",
        "connection-timeout",
        "host-gone",
        "host-unknown",
        "improper-addressing",
        "internal-server-error",
        "invalid-from",
        "invalid-namespace",
        "invalid-xml",
        "not-authorized",
        "not-well-formed",
        "policy-violation",
        "remote-connection-failed",
        "reset",
        "resource-constraint",
        "restricted-xml",
        "see-other-host",
        "system-shutdown",
        "undefined-condition",
        "unsupported-encoding",
        "unsupported-feature",
        "unsupported-stanza-type",
        "unsupported-version",
    ];

    for child in &builder.children {
        if conditions.contains(&child.name.as_str()) {
            return XmppStanza::StreamError(child.name.clone());
        }
    }

    XmppStanza::StreamError("unknown".to_string())
}

/// Extracts an attribute value from the first XML element found.
/// Uses quick-xml for correct handling of quotes and entities.
pub fn extract_attr(xml: &str, attr: &str) -> Option<String> {
    use quick_xml::events::Event;
    use quick_xml::Reader;

    let mut reader = Reader::from_str(xml);
    let mut buf = Vec::new();
    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(ref e)) | Ok(Event::Empty(ref e)) => {
                if let Some(a) = e
                    .try_get_attribute(attr.as_bytes())
                    .ok()
                    .flatten()
                {
                    return a
                        .unescape_value()
                        .ok()
                        .map(|v| v.into_owned());
                }
            }
            Ok(Event::Eof) | Err(_) => return None,
            _ => {}
        }
        buf.clear();
    }
}

/// Extracts the bare JID (without resource) from a full or bare JID.
///
/// - `"user@localhost/mobile"` → `"user@localhost"`
/// - `"user@localhost"` → `"user@localhost"`
/// - `"localhost"` → `"localhost"` (domain-only)
pub fn bare_jid(jid: &str) -> &str {
    jid.split('/').next().unwrap_or(jid)
}

/// Extracts text content of the first element matching `tag`.
/// Uses quick-xml for correct handling of entities and CDATA.
pub fn extract_element_text(xml: &str, tag: &str) -> Option<String> {
    use quick_xml::events::Event;
    use quick_xml::Reader;

    let mut reader = Reader::from_str(xml);
    let mut buf = Vec::new();
    let tag_bytes = tag.as_bytes();
    let mut inside_target = false;
    let mut text = String::new();

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(ref e)) => {
                if e.local_name().as_ref() == tag_bytes {
                    inside_target = true;
                    text.clear();
                }
            }
            Ok(Event::Text(ref e)) if inside_target => {
                if let Ok(t) = e.unescape() {
                    text.push_str(&t);
                }
            }
            Ok(Event::CData(ref e)) if inside_target => {
                if let Ok(t) = std::str::from_utf8(e.as_ref()) {
                    text.push_str(t);
                }
            }
            Ok(Event::End(ref e)) if inside_target => {
                if e.local_name().as_ref() == tag_bytes {
                    return if text.is_empty() { None } else { Some(text) };
                }
            }
            Ok(Event::Eof) | Err(_) => return None,
            _ => {}
        }
        buf.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extract_stream_id() {
        let xml = "<stream:stream xmlns='jabber:component:accept' \
                    xmlns:stream='http://etherx.jabber.org/streams' \
                    from='agent.localhost' id='abc123'>";
        assert_eq!(extract_stream_id(xml), Some("abc123".to_string()));
    }

    #[test]
    fn test_build_message_with_from() {
        let xml = build_message(Some("agent.localhost"), "user@localhost", "Hi!", None);
        assert!(xml.contains("from='agent.localhost'"));
        assert!(xml.contains("<body>Hi!</body>"));
    }

    #[test]
    fn test_build_message_without_from() {
        let xml = build_message(None, "user@localhost", "Hi!", None);
        assert!(!xml.contains("from="));
        assert!(xml.contains("to='user@localhost'"));
        assert!(xml.contains("<body>Hi!</body>"));
    }

    // ── XML escaping tests ───────────────────────────────

    #[test]
    fn test_build_message_escapes_body() {
        let xml = build_message(None, "user@localhost", "Tom & Jerry's <show>", None);
        assert!(xml.contains("<body>Tom &amp; Jerry&apos;s &lt;show&gt;</body>"));
    }

    #[test]
    fn test_build_message_escapes_to_attribute() {
        let xml = build_message(None, "user@localhost/it's", "Hi", None);
        assert!(xml.contains("to='user@localhost/it&apos;s'"));
    }

    #[test]
    fn test_build_muc_message_escapes_body() {
        let xml = build_muc_message(None, "room@conf.local", "2 > 1 & 1 < 2");
        assert!(xml.contains("<body>2 &gt; 1 &amp; 1 &lt; 2</body>"));
    }

    #[test]
    fn test_build_message_normal_text_unchanged() {
        // No special chars => body is verbatim
        let xml = build_message(None, "user@localhost", "Hello world", None);
        assert!(xml.contains("<body>Hello world</body>"));
    }

    #[test]
    fn test_build_client_stream_open() {
        let xml = build_client_stream_open("example.com");
        assert!(xml.contains("xmlns='jabber:client'"));
        assert!(xml.contains("version='1.0'"));
        assert!(xml.contains("to='example.com'"));
    }

    #[test]
    fn test_extract_sasl_mechanisms() {
        let xml = "<mechanisms xmlns='urn:ietf:params:xml:ns:xmpp-sasl'>\
                   <mechanism>SCRAM-SHA-1</mechanism>\
                   <mechanism>PLAIN</mechanism>\
                   </mechanisms>";
        let mechs = extract_sasl_mechanisms(xml);
        assert_eq!(mechs, vec!["SCRAM-SHA-1", "PLAIN"]);
    }

    #[test]
    fn test_extract_sasl_challenge() {
        let xml = "<challenge xmlns='urn:ietf:params:xml:ns:xmpp-sasl'>cj1meW...</challenge>";
        assert_eq!(
            extract_sasl_challenge(xml),
            Some("cj1meW...".to_string())
        );
    }

    #[test]
    fn test_extract_bound_jid() {
        let xml = "<iq type='result' id='bind1'>\
                   <bind xmlns='urn:ietf:params:xml:ns:xmpp-bind'>\
                   <jid>bot@localhost/fluux-agent</jid>\
                   </bind></iq>";
        assert_eq!(
            extract_bound_jid(xml),
            Some("bot@localhost/fluux-agent".to_string())
        );
    }

    // ── Outbound chat state tests (XEP-0085) ─────────────

    #[test]
    fn test_build_message_includes_active_chat_state() {
        let xml = build_message(None, "user@localhost", "Hello", None);
        assert!(xml.contains("<active xmlns='http://jabber.org/protocol/chatstates'/>"));
        assert!(xml.contains("<body>Hello</body>"));
    }

    #[test]
    fn test_build_message_with_from_includes_active_chat_state() {
        let xml = build_message(Some("agent.localhost"), "user@localhost", "Hi", None);
        assert!(xml.contains("<active xmlns='http://jabber.org/protocol/chatstates'/>"));
        assert!(xml.contains("from='agent.localhost'"));
    }

    #[test]
    fn test_build_chat_state_composing_c2s() {
        let xml = build_chat_state_composing(None, "user@localhost", "chat");
        assert!(xml.contains("to='user@localhost'"));
        assert!(xml.contains("<composing xmlns='http://jabber.org/protocol/chatstates'/>"));
        assert!(xml.contains("type='chat'"));
        assert!(!xml.contains("from="));
        assert!(!xml.contains("<body"));
    }

    #[test]
    fn test_build_chat_state_composing_component() {
        let xml = build_chat_state_composing(Some("agent.localhost"), "user@localhost", "chat");
        assert!(xml.contains("from='agent.localhost'"));
        assert!(xml.contains("to='user@localhost'"));
        assert!(xml.contains("<composing xmlns='http://jabber.org/protocol/chatstates'/>"));
    }

    #[test]
    fn test_build_chat_state_composing_groupchat() {
        let xml = build_chat_state_composing(None, "lobby@conference.localhost", "groupchat");
        assert!(xml.contains("to='lobby@conference.localhost'"));
        assert!(xml.contains("type='groupchat'"));
        assert!(xml.contains("<composing xmlns='http://jabber.org/protocol/chatstates'/>"));
    }

    #[test]
    fn test_build_chat_state_paused_c2s() {
        let xml = build_chat_state_paused(None, "user@localhost", "chat");
        assert!(xml.contains("to='user@localhost'"));
        assert!(xml.contains("<paused xmlns='http://jabber.org/protocol/chatstates'/>"));
        assert!(xml.contains("type='chat'"));
        assert!(!xml.contains("from="));
        assert!(!xml.contains("<body"));
    }

    #[test]
    fn test_build_chat_state_paused_component() {
        let xml = build_chat_state_paused(Some("agent.localhost"), "user@localhost", "chat");
        assert!(xml.contains("from='agent.localhost'"));
        assert!(xml.contains("<paused xmlns='http://jabber.org/protocol/chatstates'/>"));
    }

    #[test]
    fn test_build_chat_state_paused_groupchat() {
        let xml = build_chat_state_paused(None, "lobby@conference.localhost", "groupchat");
        assert!(xml.contains("to='lobby@conference.localhost'"));
        assert!(xml.contains("type='groupchat'"));
        assert!(xml.contains("<paused xmlns='http://jabber.org/protocol/chatstates'/>"));
    }

    // ── Presence tests ──────────────────────────────────

    #[test]
    fn test_build_subscribe() {
        let xml = build_subscribe("user@localhost");
        assert_eq!(xml, "<presence to='user@localhost' type='subscribe'/>");
    }

    #[test]
    fn test_build_subscribed() {
        let xml = build_subscribed("user@localhost");
        assert_eq!(xml, "<presence to='user@localhost' type='subscribed'/>");
    }

    // ── Roster tests ────────────────────────────────────

    #[test]
    fn test_build_roster_get() {
        let xml = build_roster_get();
        assert!(xml.contains("jabber:iq:roster"));
        assert!(xml.contains("type='get'"));
    }

    #[test]
    fn test_extract_roster_jids() {
        let xml = "<iq type='result' id='roster1'>\
                   <query xmlns='jabber:iq:roster'>\
                   <item jid='alice@localhost' subscription='both'/>\
                   <item jid='bob@localhost' subscription='to'/>\
                   </query></iq>";
        let jids = extract_roster_jids(xml);
        assert_eq!(jids, vec!["alice@localhost", "bob@localhost"]);
    }

    #[test]
    fn test_extract_roster_jids_empty() {
        let xml = "<iq type='result' id='roster1'>\
                   <query xmlns='jabber:iq:roster'/></iq>";
        let jids = extract_roster_jids(xml);
        assert!(jids.is_empty());
    }

    #[test]
    fn test_extract_roster_jids_skips_removed() {
        let xml = "<iq type='result' id='roster1'>\
                   <query xmlns='jabber:iq:roster'>\
                   <item jid='alice@localhost' subscription='both'/>\
                   <item jid='removed@localhost' subscription='remove'/>\
                   </query></iq>";
        let jids = extract_roster_jids(xml);
        assert_eq!(jids, vec!["alice@localhost"]);
    }

    // ── Component protocol stanza tests ─────────────────

    #[test]
    fn test_build_stream_open() {
        let xml = build_stream_open("agent.localhost");
        assert!(xml.contains("xmlns='jabber:component:accept'"));
        assert!(xml.contains("to='agent.localhost'"));
    }

    #[test]
    fn test_build_handshake() {
        let xml = build_handshake("abc123hash");
        assert_eq!(xml, "<handshake>abc123hash</handshake>");
    }

    #[test]
    fn test_is_handshake_success() {
        assert!(is_handshake_success("<handshake/>"));
        assert!(is_handshake_success("<handshake></handshake>"));
        assert!(!is_handshake_success("<error/>"));
    }

    // ── C2S protocol stanza tests ───────────────────────

    #[test]
    fn test_build_starttls() {
        let xml = build_starttls();
        assert!(xml.contains("urn:ietf:params:xml:ns:xmpp-tls"));
    }

    #[test]
    fn test_is_starttls_proceed() {
        assert!(is_starttls_proceed("<proceed xmlns='urn:ietf:params:xml:ns:xmpp-tls'/>"));
        assert!(!is_starttls_proceed("<failure/>"));
    }

    #[test]
    fn test_has_starttls() {
        let features = "<stream:features><starttls xmlns='urn:ietf:params:xml:ns:xmpp-tls'/></stream:features>";
        assert!(has_starttls(features));
        assert!(!has_starttls("<stream:features></stream:features>"));
    }

    #[test]
    fn test_build_sasl_auth_plain() {
        let xml = build_sasl_auth_plain("bot", "secret");
        assert!(xml.contains("mechanism='PLAIN'"));
        assert!(xml.contains("urn:ietf:params:xml:ns:xmpp-sasl"));
        // PLAIN payload is base64(\0bot\0secret)
        use base64::Engine;
        let expected = base64::engine::general_purpose::STANDARD.encode("\0bot\0secret");
        assert!(xml.contains(&expected));
    }

    #[test]
    fn test_is_sasl_success() {
        assert!(is_sasl_success("<success xmlns='urn:ietf:params:xml:ns:xmpp-sasl'/>"));
        assert!(!is_sasl_success("<failure/>"));
    }

    #[test]
    fn test_is_sasl_challenge() {
        assert!(is_sasl_challenge("<challenge xmlns='urn:ietf:params:xml:ns:xmpp-sasl'>data</challenge>"));
        assert!(!is_sasl_challenge("<success/>"));
    }

    #[test]
    fn test_build_bind_request() {
        let xml = build_bind_request("fluux-agent");
        assert!(xml.contains("urn:ietf:params:xml:ns:xmpp-bind"));
        assert!(xml.contains("<resource>fluux-agent</resource>"));
        assert!(xml.contains("type='set'"));
    }

    #[test]
    fn test_build_initial_presence() {
        assert_eq!(build_initial_presence(), "<presence/>");
    }

    // ── Helper function tests ───────────────────────────

    #[test]
    fn test_extract_attr_single_quotes() {
        let xml = "<message from='user@localhost' to='bot@localhost'>";
        assert_eq!(extract_attr(xml, "from"), Some("user@localhost".to_string()));
        assert_eq!(extract_attr(xml, "to"), Some("bot@localhost".to_string()));
    }

    #[test]
    fn test_extract_attr_double_quotes() {
        let xml = r#"<message from="user@localhost" type="chat">"#;
        assert_eq!(extract_attr(xml, "from"), Some("user@localhost".to_string()));
        assert_eq!(extract_attr(xml, "type"), Some("chat".to_string()));
    }

    #[test]
    fn test_extract_attr_missing() {
        let xml = "<message from='user@localhost'>";
        assert_eq!(extract_attr(xml, "id"), None);
    }

    #[test]
    fn test_extract_element_text_found() {
        let xml = "<iq><jid>bot@localhost/res</jid></iq>";
        assert_eq!(
            extract_element_text(xml, "jid"),
            Some("bot@localhost/res".to_string())
        );
    }

    #[test]
    fn test_extract_element_text_empty() {
        let xml = "<iq><jid></jid></iq>";
        assert_eq!(extract_element_text(xml, "jid"), None);
    }

    #[test]
    fn test_extract_element_text_missing() {
        let xml = "<iq><bind/></iq>";
        assert_eq!(extract_element_text(xml, "jid"), None);
    }

    // ── MUC stanza builder tests ───────────────────────

    #[test]
    fn test_build_muc_join_c2s() {
        let xml = build_muc_join("lobby@conference.localhost", "bot", None);
        assert!(!xml.contains("from="));
        assert!(xml.contains("to='lobby@conference.localhost/bot'"));
        assert!(xml.contains("http://jabber.org/protocol/muc"));
        assert!(xml.contains("<history maxstanzas='0'/>"));
    }

    #[test]
    fn test_build_muc_join_component() {
        let xml = build_muc_join("lobby@conference.localhost", "bot", Some("agent.localhost"));
        assert!(xml.contains("from='agent.localhost'"));
        assert!(xml.contains("to='lobby@conference.localhost/bot'"));
        assert!(xml.contains("http://jabber.org/protocol/muc"));
        assert!(xml.contains("<history maxstanzas='0'/>"));
    }

    #[test]
    fn test_build_muc_message_c2s() {
        let xml = build_muc_message(None, "lobby@conference.localhost", "Hello room!");
        assert!(!xml.contains("from="));
        assert!(xml.contains("to='lobby@conference.localhost'"));
        assert!(xml.contains("type='groupchat'"));
        assert!(xml.contains("<body>Hello room!</body>"));
        assert!(xml.contains("<active xmlns='http://jabber.org/protocol/chatstates'/>"));
    }

    #[test]
    fn test_build_muc_message_component() {
        let xml = build_muc_message(Some("agent.localhost"), "lobby@conference.localhost", "Hi!");
        assert!(xml.contains("from='agent.localhost'"));
        assert!(xml.contains("type='groupchat'"));
        assert!(xml.contains("<body>Hi!</body>"));
        assert!(xml.contains("<active xmlns='http://jabber.org/protocol/chatstates'/>"));
    }

    // ── bare_jid tests ─────────────────────────────────

    #[test]
    fn test_bare_jid_full_jid() {
        assert_eq!(bare_jid("user@localhost/mobile"), "user@localhost");
    }

    #[test]
    fn test_bare_jid_already_bare() {
        assert_eq!(bare_jid("user@localhost"), "user@localhost");
    }

    #[test]
    fn test_bare_jid_domain_only() {
        assert_eq!(bare_jid("localhost"), "localhost");
    }

    #[test]
    fn test_bare_jid_long_resource() {
        assert_eq!(
            bare_jid("admin@localhost/Conversations.abc123"),
            "admin@localhost"
        );
    }

    // ── StanzaParser tests (quick-xml event-based) ──────

    /// Helper: parse a complete XML fragment through StanzaParser using sync quick-xml.
    fn parse_xml_to_stanza(xml: &str) -> Option<XmppStanza> {
        use quick_xml::Reader;

        let mut reader = Reader::from_str(xml);
        reader.config_mut().trim_text(true);
        let mut parser = StanzaParser::new();
        let mut buf = Vec::new();
        loop {
            match reader.read_event_into(&mut buf) {
                Ok(Event::Eof) => return None,
                Ok(event) => {
                    if let Some(stanza) = parser.feed(event) {
                        return Some(stanza);
                    }
                }
                Err(_) => return None,
            }
            buf.clear();
        }
    }

    /// Helper: parse within a stream:stream wrapper (like the real event loop).
    fn parse_xml_in_stream(xml: &str) -> Vec<XmppStanza> {
        use quick_xml::Reader;

        let wrapped = format!(
            "<stream:stream xmlns='jabber:client' xmlns:stream='http://etherx.jabber.org/streams'>{xml}</stream:stream>"
        );
        let mut reader = Reader::from_str(&wrapped);
        reader.config_mut().trim_text(true);
        let mut parser = StanzaParser::new();
        let mut buf = Vec::new();
        let mut results = Vec::new();
        loop {
            match reader.read_event_into(&mut buf) {
                Ok(Event::Eof) => break,
                Ok(event) => {
                    if let Some(stanza) = parser.feed(event) {
                        results.push(stanza);
                    }
                }
                Err(_) => break,
            }
            buf.clear();
        }
        results
    }

    #[test]
    fn test_sp_simple_message() {
        let xml = "<message from='user@localhost/res' to='bot@localhost' type='chat' id='m1'>\
                   <body>Hello agent</body></message>";
        let stanza = parse_xml_to_stanza(xml).unwrap();
        match stanza {
            XmppStanza::Message(msg) => {
                assert_eq!(msg.from, "user@localhost/res");
                assert_eq!(msg.to, "bot@localhost");
                assert_eq!(msg.body, "Hello agent");
                assert_eq!(msg.id, Some("m1".to_string()));
                assert_eq!(msg.message_type, MessageType::Chat);
                assert!(msg.oob.is_empty());
            }
            other => panic!("Expected Message, got {other:?}"),
        }
    }

    #[test]
    fn test_sp_groupchat_message() {
        let xml = "<message from='room@conf/nick' to='bot@localhost' type='groupchat'>\
                   <body>Hey room</body></message>";
        match parse_xml_to_stanza(xml).unwrap() {
            XmppStanza::Message(msg) => {
                assert_eq!(msg.message_type, MessageType::GroupChat);
                assert_eq!(msg.body, "Hey room");
            }
            other => panic!("Expected Message, got {other:?}"),
        }
    }

    #[test]
    fn test_sp_chat_state_composing_filtered() {
        let xml = "<message from='user@localhost/res' to='bot@localhost' type='chat'>\
                   <composing xmlns='http://jabber.org/protocol/chatstates'/></message>";
        match parse_xml_to_stanza(xml).unwrap() {
            XmppStanza::Ignored => {}
            other => panic!("Expected Ignored, got {other:?}"),
        }
    }

    #[test]
    fn test_sp_chat_state_paused_filtered() {
        let xml = "<message from='user@localhost/res' to='bot@localhost' type='chat'>\
                   <paused xmlns='http://jabber.org/protocol/chatstates'/></message>";
        match parse_xml_to_stanza(xml).unwrap() {
            XmppStanza::Ignored => {}
            other => panic!("Expected Ignored, got {other:?}"),
        }
    }

    #[test]
    fn test_sp_message_with_body_and_state_passes() {
        let xml = "<message from='user@localhost/res' to='bot@localhost' type='chat'>\
                   <body>Hello!</body>\
                   <active xmlns='http://jabber.org/protocol/chatstates'/></message>";
        match parse_xml_to_stanza(xml).unwrap() {
            XmppStanza::Message(msg) => assert_eq!(msg.body, "Hello!"),
            other => panic!("Expected Message, got {other:?}"),
        }
    }

    #[test]
    fn test_sp_message_empty_body_filtered() {
        let xml = "<message from='user@localhost/res' to='bot@localhost' type='chat'>\
                   <body></body></message>";
        match parse_xml_to_stanza(xml).unwrap() {
            XmppStanza::Ignored => {}
            other => panic!("Expected Ignored, got {other:?}"),
        }
    }

    #[test]
    fn test_sp_message_body_trimmed() {
        let xml = "<message from='user@localhost/res' to='bot@localhost' type='chat'>\
                   <body>  Hello agent  </body></message>";
        match parse_xml_to_stanza(xml).unwrap() {
            XmppStanza::Message(msg) => assert_eq!(msg.body, "Hello agent"),
            other => panic!("Expected Message, got {other:?}"),
        }
    }

    #[test]
    fn test_sp_message_no_from_ignored() {
        let xml = "<message to='bot@localhost' type='chat'><body>Hi</body></message>";
        match parse_xml_to_stanza(xml).unwrap() {
            XmppStanza::Ignored => {}
            other => panic!("Expected Ignored, got {other:?}"),
        }
    }

    #[test]
    fn test_sp_oob_with_body() {
        let xml = "<message from='user@localhost/res' to='bot@localhost' type='chat'>\
                   <body>Check this out</body>\
                   <x xmlns='jabber:x:oob'><url>https://example.com/file.png</url></x></message>";
        match parse_xml_to_stanza(xml).unwrap() {
            XmppStanza::Message(msg) => {
                assert_eq!(msg.body, "Check this out");
                assert_eq!(msg.oob.len(), 1);
                assert_eq!(msg.oob[0].url, "https://example.com/file.png");
            }
            other => panic!("Expected Message, got {other:?}"),
        }
    }

    #[test]
    fn test_sp_oob_fallback_body_stripped() {
        let xml = "<message from='user@localhost/res' to='bot@localhost' type='chat'>\
                   <body>https://example.com/file.png</body>\
                   <x xmlns='jabber:x:oob'><url>https://example.com/file.png</url></x></message>";
        match parse_xml_to_stanza(xml).unwrap() {
            XmppStanza::Message(msg) => {
                assert!(msg.body.is_empty(), "Fallback body should be stripped");
                assert_eq!(msg.oob.len(), 1);
            }
            other => panic!("Expected Message, got {other:?}"),
        }
    }

    #[test]
    fn test_sp_oob_only_no_body() {
        let xml = "<message from='user@localhost/res' to='bot@localhost' type='chat'>\
                   <x xmlns='jabber:x:oob'><url>https://example.com/file.png</url></x></message>";
        match parse_xml_to_stanza(xml).unwrap() {
            XmppStanza::Message(msg) => {
                assert!(msg.body.is_empty());
                assert_eq!(msg.oob.len(), 1);
            }
            other => panic!("Expected Message, got {other:?}"),
        }
    }

    #[test]
    fn test_sp_multiple_oob() {
        let xml = "<message from='user@localhost/res' to='bot@localhost' type='chat'>\
                   <body>Two files</body>\
                   <x xmlns='jabber:x:oob'><url>https://example.com/a.png</url></x>\
                   <x xmlns='jabber:x:oob'><url>https://example.com/b.pdf</url><desc>Doc</desc></x></message>";
        match parse_xml_to_stanza(xml).unwrap() {
            XmppStanza::Message(msg) => {
                assert_eq!(msg.oob.len(), 2);
                assert_eq!(msg.oob[0].url, "https://example.com/a.png");
                assert!(msg.oob[0].desc.is_none());
                assert_eq!(msg.oob[1].url, "https://example.com/b.pdf");
                assert_eq!(msg.oob[1].desc.as_deref(), Some("Doc"));
            }
            other => panic!("Expected Message, got {other:?}"),
        }
    }

    #[test]
    fn test_sp_presence_subscribe() {
        let xml = "<presence from='user@localhost' to='bot@localhost' type='subscribe'/>";
        match parse_xml_to_stanza(xml).unwrap() {
            XmppStanza::Presence(p) => {
                assert_eq!(p.from, "user@localhost");
                assert_eq!(p.presence_type, PresenceType::Subscribe);
            }
            other => panic!("Expected Presence, got {other:?}"),
        }
    }

    #[test]
    fn test_sp_presence_available() {
        let xml = "<presence from='user@localhost/mobile'><show>chat</show></presence>";
        match parse_xml_to_stanza(xml).unwrap() {
            XmppStanza::Presence(p) => {
                assert_eq!(p.from, "user@localhost/mobile");
                assert_eq!(p.presence_type, PresenceType::Available);
            }
            other => panic!("Expected Presence, got {other:?}"),
        }
    }

    #[test]
    fn test_sp_presence_unavailable() {
        let xml = "<presence from='user@localhost/res' type='unavailable'/>";
        match parse_xml_to_stanza(xml).unwrap() {
            XmppStanza::Presence(p) => {
                assert_eq!(p.from, "user@localhost/res");
                assert_eq!(p.presence_type, PresenceType::Unavailable);
            }
            other => panic!("Expected Presence, got {other:?}"),
        }
    }

    #[test]
    fn test_sp_stream_error_conflict() {
        let xml = "<stream:error><conflict xmlns='urn:ietf:params:xml:ns:xmpp-streams'/></stream:error>";
        match parse_xml_to_stanza(xml).unwrap() {
            XmppStanza::StreamError(c) => assert_eq!(c, "conflict"),
            other => panic!("Expected StreamError, got {other:?}"),
        }
    }

    #[test]
    fn test_sp_stream_error_system_shutdown() {
        let xml = "<stream:error><system-shutdown xmlns='urn:ietf:params:xml:ns:xmpp-streams'/></stream:error>";
        match parse_xml_to_stanza(xml).unwrap() {
            XmppStanza::StreamError(c) => assert_eq!(c, "system-shutdown"),
            other => panic!("Expected StreamError, got {other:?}"),
        }
    }

    #[test]
    fn test_sp_iq_ignored() {
        let xml = "<iq type='result' id='1'><query/></iq>";
        match parse_xml_to_stanza(xml).unwrap() {
            XmppStanza::Ignored => {}
            other => panic!("Expected Ignored, got {other:?}"),
        }
    }

    #[test]
    fn test_sp_self_closing_ignored() {
        let xml = "<r xmlns='urn:xmpp:sm:3'/>";
        match parse_xml_to_stanza(xml).unwrap() {
            XmppStanza::Ignored => {}
            other => panic!("Expected Ignored, got {other:?}"),
        }
    }

    #[test]
    fn test_sp_stream_open_returns_stream_level() {
        let xml = "<stream:stream xmlns='jabber:client' xmlns:stream='http://etherx.jabber.org/streams' to='example.com'>";
        // Note: quick-xml may not handle the unclosed stream:stream in from_str.
        // We test the stream wrapper via parse_xml_in_stream instead.
        let stanzas = parse_xml_in_stream("");
        // Should get StreamLevel for the open and StreamLevel for the close
        assert!(stanzas.iter().any(|s| matches!(s, XmppStanza::StreamLevel)));
    }

    #[test]
    fn test_sp_stanzas_inside_stream() {
        let stanzas = parse_xml_in_stream(
            "<message from='a@b' to='c@d' type='chat'><body>hi</body></message>\
             <presence from='a@b' type='unavailable'/>"
        );
        // Should have: StreamLevel (open), Message, Presence, StreamLevel (close)
        let mut msgs = 0;
        let mut pres = 0;
        let mut stream_levels = 0;
        for s in &stanzas {
            match s {
                XmppStanza::Message(_) => msgs += 1,
                XmppStanza::Presence(_) => pres += 1,
                XmppStanza::StreamLevel => stream_levels += 1,
                _ => {}
            }
        }
        assert_eq!(msgs, 1);
        assert_eq!(pres, 1);
        assert!(stream_levels >= 2); // open + close
    }

    #[test]
    fn test_sp_entity_decoding_in_body() {
        let xml = "<message from='a@b' to='c@d' type='chat'>\
                   <body>Tom &amp; Jerry&apos;s &lt;show&gt;</body></message>";
        match parse_xml_to_stanza(xml).unwrap() {
            XmppStanza::Message(msg) => {
                assert_eq!(msg.body, "Tom & Jerry's <show>");
            }
            other => panic!("Expected Message, got {other:?}"),
        }
    }

    #[test]
    fn test_sp_cdata_in_body() {
        let xml = "<message from='a@b' to='c@d' type='chat'>\
                   <body><![CDATA[<not a tag> & stuff]]></body></message>";
        match parse_xml_to_stanza(xml).unwrap() {
            XmppStanza::Message(msg) => {
                assert_eq!(msg.body, "<not a tag> & stuff");
            }
            other => panic!("Expected Message, got {other:?}"),
        }
    }

    #[test]
    fn test_sp_real_oob_message() {
        let xml = "<message from='user@example.com/mobile' to='bot@example.com' type='chat' id='abc123'>\
                   <body>Can you read this ?\nhttps://upload.example.com/file.png</body>\
                   <active xmlns='http://jabber.org/protocol/chatstates'/>\
                   <x xmlns='jabber:x:oob'>\
                   <url>https://upload.example.com/file.png</url>\
                   </x>\
                   </message>";
        match parse_xml_to_stanza(xml).unwrap() {
            XmppStanza::Message(msg) => {
                assert_eq!(msg.from, "user@example.com/mobile");
                assert!(!msg.oob.is_empty());
                assert_eq!(msg.oob[0].url, "https://upload.example.com/file.png");
                // Body should NOT be stripped since it contains more than just the URL
                assert!(msg.body.contains("Can you read this"));
            }
            other => panic!("Expected Message, got {other:?}"),
        }
    }

    #[test]
    fn test_sp_no_type_defaults_to_chat() {
        let xml = "<message from='a@b' to='c@d'><body>hi</body></message>";
        match parse_xml_to_stanza(xml).unwrap() {
            XmppStanza::Message(msg) => {
                assert_eq!(msg.message_type, MessageType::Chat);
            }
            other => panic!("Expected Message, got {other:?}"),
        }
    }

    #[test]
    fn test_sp_presence_with_children_parsed() {
        let xml = "<presence from='a@b'><x xmlns='vcard-temp:x:update'/><show>away</show></presence>";
        match parse_xml_to_stanza(xml).unwrap() {
            XmppStanza::Presence(p) => {
                assert_eq!(p.from, "a@b");
                assert_eq!(p.presence_type, PresenceType::Available);
            }
            other => panic!("Expected Presence, got {other:?}"),
        }
    }
}
