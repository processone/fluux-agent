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
/// `allowed_jids` is used for automatic presence subscription in C2S mode.
pub async fn connect(
    config: ServerConfig,
    allowed_jids: Vec<String>,
) -> Result<(mpsc::Receiver<XmppEvent>, mpsc::Sender<XmppCommand>)> {
    match &config.mode {
        ConnectionMode::Component { .. } => XmppComponent::new(config).connect().await,
        ConnectionMode::Client { .. } => {
            client::XmppClient::new(config)
                .with_allowed_jids(allowed_jids)
                .connect()
                .await
        }
    }
}
