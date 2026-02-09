pub mod client;
pub mod component;
pub mod sasl;
pub mod stanzas;

use std::fmt;
use std::time::Duration;
use tokio::sync::mpsc;

use crate::config::{ConnectionMode, KeepaliveConfig, ServerConfig};
use component::{XmppCommand, XmppComponent, XmppEvent};

/// Categorized XMPP connection errors.
///
/// Used by the reconnection loop to decide whether to retry.
#[derive(Debug)]
pub enum XmppError {
    /// Authentication failure (bad credentials) — permanent.
    Auth(String),
    /// Configuration error (missing STARTTLS, unsupported SASL) — permanent.
    Config(String),
    /// Session replaced by another client (conflict) — permanent.
    Conflict(String),
    /// Transient network/server error — retry is appropriate.
    Transient(String),
}

impl XmppError {
    /// Returns true if this error is worth retrying.
    pub fn is_retriable(&self) -> bool {
        matches!(self, XmppError::Transient(_))
    }
}

impl fmt::Display for XmppError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            XmppError::Auth(msg) => write!(f, "authentication error: {msg}"),
            XmppError::Config(msg) => write!(f, "configuration error: {msg}"),
            XmppError::Conflict(msg) => write!(f, "session conflict: {msg}"),
            XmppError::Transient(msg) => write!(f, "transient error: {msg}"),
        }
    }
}

impl std::error::Error for XmppError {}

/// Connects to the XMPP server using the mode specified in config.
/// Returns the same channel pair regardless of mode.
/// `allowed_jids` is used for automatic presence subscription in C2S mode.
/// `keepalive` controls whitespace pings and read timeout.
pub async fn connect(
    config: ServerConfig,
    allowed_jids: Vec<String>,
    keepalive: &KeepaliveConfig,
) -> Result<(mpsc::Receiver<XmppEvent>, mpsc::Sender<XmppCommand>), XmppError> {
    let read_timeout = if keepalive.enabled {
        Some(Duration::from_secs(keepalive.read_timeout_secs))
    } else {
        None
    };

    match &config.mode {
        ConnectionMode::Component { .. } => {
            XmppComponent::new(config).connect(read_timeout).await
        }
        ConnectionMode::Client { .. } => {
            client::XmppClient::new(config)
                .with_allowed_jids(allowed_jids)
                .connect(read_timeout)
                .await
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_xmpp_error_is_retriable() {
        assert!(!XmppError::Auth("bad password".into()).is_retriable());
        assert!(!XmppError::Config("no STARTTLS".into()).is_retriable());
        assert!(!XmppError::Conflict("session replaced".into()).is_retriable());
        assert!(XmppError::Transient("connection refused".into()).is_retriable());
    }

    #[test]
    fn test_xmpp_error_display() {
        assert_eq!(
            XmppError::Auth("bad password".into()).to_string(),
            "authentication error: bad password"
        );
        assert_eq!(
            XmppError::Transient("timeout".into()).to_string(),
            "transient error: timeout"
        );
    }
}
