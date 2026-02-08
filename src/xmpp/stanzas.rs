/// XMPP stanza building and parsing.
/// Manual XML handling — we only need a subset for both
/// component (XEP-0114) and C2S client protocols.

/// Message type — distinguishes 1:1 chat from MUC groupchat
#[derive(Debug, Clone, PartialEq)]
pub enum MessageType {
    Chat,
    GroupChat,
}

/// Parsed incoming message
#[derive(Debug, Clone)]
pub struct IncomingMessage {
    pub from: String,
    pub to: String,
    pub body: String,
    pub id: Option<String>,
    pub message_type: MessageType,
}

// ── Message stanzas (shared) ─────────────────────────────

/// Builds an outgoing XMPP message.
/// `from` is Some for component mode, None for C2S (server adds it).
/// Includes `<active/>` chat state (XEP-0085) to signal we've stopped typing.
pub fn build_message(from: Option<&str>, to: &str, body: &str, id: Option<&str>) -> String {
    let from_attr = from
        .map(|f| format!(" from='{f}'"))
        .unwrap_or_default();
    let id_attr = id
        .map(|i| format!(" id='{i}'"))
        .unwrap_or_default();
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
pub fn build_chat_state_composing(from: Option<&str>, to: &str) -> String {
    let from_attr = from
        .map(|f| format!(" from='{f}'"))
        .unwrap_or_default();
    format!(
        "<message{from_attr} to='{to}' type='chat'>\
         <composing xmlns='http://jabber.org/protocol/chatstates'/>\
         </message>"
    )
}

/// Builds a standalone `<paused/>` chat state notification.
/// Sent when the agent stops generating without sending a message
/// (e.g., error during LLM call, or cancelled request).
/// `from` is Some for component mode, None for C2S.
pub fn build_chat_state_paused(from: Option<&str>, to: &str) -> String {
    let from_attr = from
        .map(|f| format!(" from='{f}'"))
        .unwrap_or_default();
    format!(
        "<message{from_attr} to='{to}' type='chat'>\
         <paused xmlns='http://jabber.org/protocol/chatstates'/>\
         </message>"
    )
}

// ── Component protocol (XEP-0114) ────────────────────────

/// Builds the stream opening for component protocol
pub fn build_stream_open(domain: &str) -> String {
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
    format!("<presence to='{to}' type='subscribe'/>")
}

/// Accept an incoming subscription request — allow the contact to see our presence
pub fn build_subscribed(to: &str) -> String {
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

/// Parses an incoming presence stanza from XML.
/// Returns None if the stanza is not a presence stanza.
pub fn parse_presence(data: &str) -> Option<IncomingPresence> {
    if !data.contains("<presence") {
        return None;
    }

    let from = extract_attr(data, "from")?;
    let type_str = extract_attr(data, "type");

    let presence_type = match type_str.as_deref() {
        Some("subscribe") => PresenceType::Subscribe,
        Some("subscribed") => PresenceType::Subscribed,
        Some("unsubscribe") => PresenceType::Unsubscribe,
        Some("unsubscribed") => PresenceType::Unsubscribed,
        Some("unavailable") => PresenceType::Unavailable,
        _ => PresenceType::Available,
    };

    Some(IncomingPresence {
        from,
        presence_type,
    })
}

// ── MUC (XEP-0045) ──────────────────────────────────────

/// Builds a MUC join presence stanza (XEP-0045).
/// `from` is Some for component mode, None for C2S.
pub fn build_muc_join(room_jid: &str, nick: &str, from: Option<&str>) -> String {
    let from_attr = from
        .map(|f| format!(" from='{f}'"))
        .unwrap_or_default();
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
pub fn build_muc_message(from: Option<&str>, to: &str, body: &str) -> String {
    let from_attr = from
        .map(|f| format!(" from='{f}'"))
        .unwrap_or_default();
    format!(
        "<message{from_attr} to='{to}' type='groupchat'>\
         <body>{body}</body>\
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

/// Checks if a message stanza is a chat state notification (XEP-0085)
/// that should be ignored (typing indicators, etc.).
pub fn is_chat_state_notification(data: &str) -> bool {
    // XEP-0085 chat states: composing, paused, active, inactive, gone
    let chat_states = [
        "<composing",
        "<paused",
        "<active",
        "<inactive",
        "<gone",
    ];
    let has_chat_state = chat_states.iter().any(|s| data.contains(s));
    let has_body = data.contains("<body>") || data.contains("<body ");
    // It's a pure chat state notification if it has a chat state but no body
    has_chat_state && !has_body
}

/// Extracts an incoming message from XML.
/// Returns None for stanzas without a body (including chat state notifications).
pub fn parse_message(data: &str) -> Option<IncomingMessage> {
    if !data.contains("<message") {
        return None;
    }

    // Skip chat state notifications (XEP-0085: composing, paused, active, etc.)
    if is_chat_state_notification(data) {
        return None;
    }

    let from = extract_attr(data, "from")?;
    let to = extract_attr(data, "to");
    let id = extract_attr(data, "id");
    let body = extract_element_text(data, "body")?;

    // Extra guard: skip messages with empty or whitespace-only bodies
    let trimmed = body.trim();
    if trimmed.is_empty() {
        return None;
    }

    let message_type = match extract_attr(data, "type").as_deref() {
        Some("groupchat") => MessageType::GroupChat,
        _ => MessageType::Chat,
    };

    Some(IncomingMessage {
        from,
        to: to.unwrap_or_default(),
        body: trimmed.to_string(),
        id,
        message_type,
    })
}

/// Extracts an attribute value from an XML tag
pub fn extract_attr(xml: &str, attr: &str) -> Option<String> {
    let patterns = [format!("{attr}='"), format!("{attr}=\"")];
    for pattern in &patterns {
        if let Some(start) = xml.find(pattern.as_str()) {
            let after = &xml[start + pattern.len()..];
            let quote = pattern.chars().last().unwrap();
            if let Some(end) = after.find(quote) {
                return Some(after[..end].to_string());
            }
        }
    }
    None
}

/// Extracts text between <tag> and </tag>
pub fn extract_element_text(xml: &str, tag: &str) -> Option<String> {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let start = xml.find(&open)?;
    let after = &xml[start + open.len()..];
    let end = after.find(&close)?;
    let text = &after[..end];
    if text.is_empty() {
        None
    } else {
        Some(text.to_string())
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
    fn test_parse_message() {
        let xml = "<message from='user@localhost/res' to='agent.localhost' type='chat' id='msg1'>\
                   <body>Hello agent</body></message>";
        let msg = parse_message(xml).unwrap();
        assert_eq!(msg.from, "user@localhost/res");
        assert_eq!(msg.body, "Hello agent");
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

    #[test]
    fn test_filter_composing_notification() {
        // XEP-0085: <composing/> without body → should be ignored
        let xml = "<message from='user@localhost/res' to='bot@localhost' type='chat'>\
                   <composing xmlns='http://jabber.org/protocol/chatstates'/>\
                   </message>";
        assert!(is_chat_state_notification(xml));
        assert!(parse_message(xml).is_none());
    }

    #[test]
    fn test_filter_paused_notification() {
        let xml = "<message from='user@localhost/res' to='bot@localhost' type='chat'>\
                   <paused xmlns='http://jabber.org/protocol/chatstates'/>\
                   </message>";
        assert!(is_chat_state_notification(xml));
        assert!(parse_message(xml).is_none());
    }

    #[test]
    fn test_filter_active_notification() {
        let xml = "<message from='user@localhost/res' to='bot@localhost' type='chat'>\
                   <active xmlns='http://jabber.org/protocol/chatstates'/>\
                   </message>";
        assert!(is_chat_state_notification(xml));
        assert!(parse_message(xml).is_none());
    }

    #[test]
    fn test_message_with_body_and_active_state_passes() {
        // Message WITH body + chat state → should parse normally
        let xml = "<message from='user@localhost/res' to='bot@localhost' type='chat'>\
                   <body>Hello!</body>\
                   <active xmlns='http://jabber.org/protocol/chatstates'/>\
                   </message>";
        assert!(!is_chat_state_notification(xml));
        let msg = parse_message(xml).unwrap();
        assert_eq!(msg.body, "Hello!");
    }

    #[test]
    fn test_message_with_empty_body_filtered() {
        let xml = "<message from='user@localhost/res' to='bot@localhost' type='chat'>\
                   <body></body>\
                   </message>";
        assert!(parse_message(xml).is_none());
    }

    #[test]
    fn test_message_body_trimmed() {
        let xml = "<message from='user@localhost/res' to='bot@localhost' type='chat'>\
                   <body>  Hello agent  </body></message>";
        let msg = parse_message(xml).unwrap();
        assert_eq!(msg.body, "Hello agent");
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
        let xml = build_chat_state_composing(None, "user@localhost");
        assert!(xml.contains("to='user@localhost'"));
        assert!(xml.contains("<composing xmlns='http://jabber.org/protocol/chatstates'/>"));
        assert!(xml.contains("type='chat'"));
        assert!(!xml.contains("from="));
        assert!(!xml.contains("<body"));
    }

    #[test]
    fn test_build_chat_state_composing_component() {
        let xml = build_chat_state_composing(Some("agent.localhost"), "user@localhost");
        assert!(xml.contains("from='agent.localhost'"));
        assert!(xml.contains("to='user@localhost'"));
        assert!(xml.contains("<composing xmlns='http://jabber.org/protocol/chatstates'/>"));
    }

    #[test]
    fn test_build_chat_state_paused_c2s() {
        let xml = build_chat_state_paused(None, "user@localhost");
        assert!(xml.contains("to='user@localhost'"));
        assert!(xml.contains("<paused xmlns='http://jabber.org/protocol/chatstates'/>"));
        assert!(xml.contains("type='chat'"));
        assert!(!xml.contains("from="));
        assert!(!xml.contains("<body"));
    }

    #[test]
    fn test_build_chat_state_paused_component() {
        let xml = build_chat_state_paused(Some("agent.localhost"), "user@localhost");
        assert!(xml.contains("from='agent.localhost'"));
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

    #[test]
    fn test_parse_presence_subscribe() {
        let xml = "<presence from='user@localhost' to='bot@localhost' type='subscribe'/>";
        let pres = parse_presence(xml).unwrap();
        assert_eq!(pres.from, "user@localhost");
        assert_eq!(pres.presence_type, PresenceType::Subscribe);
    }

    #[test]
    fn test_parse_presence_subscribed() {
        let xml = "<presence from='user@localhost' to='bot@localhost' type='subscribed'/>";
        let pres = parse_presence(xml).unwrap();
        assert_eq!(pres.from, "user@localhost");
        assert_eq!(pres.presence_type, PresenceType::Subscribed);
    }

    #[test]
    fn test_parse_presence_available() {
        // No type attribute = available
        let xml = "<presence from='user@localhost/mobile'><show>chat</show></presence>";
        let pres = parse_presence(xml).unwrap();
        assert_eq!(pres.from, "user@localhost/mobile");
        assert_eq!(pres.presence_type, PresenceType::Available);
    }

    #[test]
    fn test_parse_presence_unavailable() {
        let xml = "<presence from='user@localhost/res' type='unavailable'/>";
        let pres = parse_presence(xml).unwrap();
        assert_eq!(pres.from, "user@localhost/res");
        assert_eq!(pres.presence_type, PresenceType::Unavailable);
    }

    #[test]
    fn test_parse_presence_not_a_presence() {
        let xml = "<message from='user@localhost' type='chat'><body>Hi</body></message>";
        assert!(parse_presence(xml).is_none());
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

    #[test]
    fn test_parse_message_extracts_all_fields() {
        let xml = "<message from='user@localhost/res' to='agent.localhost' type='chat' id='msg42'>\
                   <body>Test message</body></message>";
        let msg = parse_message(xml).unwrap();
        assert_eq!(msg.from, "user@localhost/res");
        assert_eq!(msg.to, "agent.localhost");
        assert_eq!(msg.body, "Test message");
        assert_eq!(msg.id, Some("msg42".to_string()));
    }

    #[test]
    fn test_parse_message_without_id() {
        let xml = "<message from='user@localhost/res' to='bot@localhost' type='chat'>\
                   <body>No ID here</body></message>";
        let msg = parse_message(xml).unwrap();
        assert_eq!(msg.id, None);
    }

    #[test]
    fn test_parse_message_no_message_tag() {
        let xml = "<iq type='result'><query/></iq>";
        assert!(parse_message(xml).is_none());
    }

    #[test]
    fn test_chat_state_gone_filtered() {
        let xml = "<message from='user@localhost/res' to='bot@localhost' type='chat'>\
                   <gone xmlns='http://jabber.org/protocol/chatstates'/>\
                   </message>";
        assert!(is_chat_state_notification(xml));
        assert!(parse_message(xml).is_none());
    }

    #[test]
    fn test_chat_state_inactive_filtered() {
        let xml = "<message from='user@localhost/res' to='bot@localhost' type='chat'>\
                   <inactive xmlns='http://jabber.org/protocol/chatstates'/>\
                   </message>";
        assert!(is_chat_state_notification(xml));
        assert!(parse_message(xml).is_none());
    }

    // ── MessageType tests ──────────────────────────────

    #[test]
    fn test_parse_message_chat_type() {
        let xml = "<message from='user@localhost/res' to='bot@localhost' type='chat'>\
                   <body>Hello</body></message>";
        let msg = parse_message(xml).unwrap();
        assert_eq!(msg.message_type, MessageType::Chat);
    }

    #[test]
    fn test_parse_message_groupchat_type() {
        let xml = "<message from='lobby@conference.localhost/alice' to='bot@localhost' type='groupchat'>\
                   <body>Hey everyone</body></message>";
        let msg = parse_message(xml).unwrap();
        assert_eq!(msg.message_type, MessageType::GroupChat);
        assert_eq!(msg.from, "lobby@conference.localhost/alice");
        assert_eq!(msg.body, "Hey everyone");
    }

    #[test]
    fn test_parse_message_no_type_defaults_to_chat() {
        let xml = "<message from='user@localhost/res' to='bot@localhost'>\
                   <body>Hi</body></message>";
        let msg = parse_message(xml).unwrap();
        assert_eq!(msg.message_type, MessageType::Chat);
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
    }

    #[test]
    fn test_build_muc_message_component() {
        let xml = build_muc_message(Some("agent.localhost"), "lobby@conference.localhost", "Hi!");
        assert!(xml.contains("from='agent.localhost'"));
        assert!(xml.contains("type='groupchat'"));
        assert!(xml.contains("<body>Hi!</body>"));
    }

    #[test]
    fn test_groupchat_composing_notification_filtered() {
        // MUC composing notification without body → should be filtered
        let xml = "<message from='lobby@conference.localhost/alice' to='bot@localhost' type='groupchat'>\
                   <composing xmlns='http://jabber.org/protocol/chatstates'/>\
                   </message>";
        assert!(is_chat_state_notification(xml));
        assert!(parse_message(xml).is_none());
    }

    #[test]
    fn test_groupchat_message_with_body_and_state_passes() {
        // MUC message with body + active state → should parse with GroupChat type
        let xml = "<message from='lobby@conference.localhost/alice' to='bot@localhost' type='groupchat'>\
                   <body>Hello room!</body>\
                   <active xmlns='http://jabber.org/protocol/chatstates'/>\
                   </message>";
        assert!(!is_chat_state_notification(xml));
        let msg = parse_message(xml).unwrap();
        assert_eq!(msg.message_type, MessageType::GroupChat);
        assert_eq!(msg.body, "Hello room!");
    }
}
