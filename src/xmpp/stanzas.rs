/// XMPP stanza building and parsing.
/// Manual XML handling — we only need a subset for both
/// component (XEP-0114) and C2S client protocols.

/// Parsed incoming message
#[derive(Debug, Clone)]
pub struct IncomingMessage {
    pub from: String,
    pub to: String,
    pub body: String,
    pub id: Option<String>,
}

// ── Message stanzas (shared) ─────────────────────────────

/// Builds an outgoing XMPP message.
/// `from` is Some for component mode, None for C2S (server adds it).
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

    Some(IncomingMessage {
        from,
        to: to.unwrap_or_default(),
        body: trimmed.to_string(),
        id,
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
}
