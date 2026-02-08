pub mod client;
pub mod component;
pub mod sasl;
pub mod stanzas;

use anyhow::Result;
use tokio::sync::mpsc;

use crate::config::{ConnectionMode, ServerConfig};
use component::{XmppCommand, XmppComponent, XmppEvent};

/// Connects to the XMPP server using the mode specified in config.
/// Returns the same channel pair regardless of mode.
pub async fn connect(
    config: ServerConfig,
) -> Result<(mpsc::Receiver<XmppEvent>, mpsc::Sender<XmppCommand>)> {
    match &config.mode {
        ConnectionMode::Component { .. } => XmppComponent::new(config).connect().await,
        ConnectionMode::Client { .. } => client::XmppClient::new(config).connect().await,
    }
}
