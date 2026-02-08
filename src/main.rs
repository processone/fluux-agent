mod agent;
mod config;
mod llm;
mod sandbox;
mod skills;
mod xmpp;

use anyhow::Result;
use tracing::info;
use tracing_subscriber::EnvFilter;

use crate::agent::memory::Memory;
use crate::agent::runtime::AgentRuntime;
use crate::config::Config;
use crate::llm::AnthropicClient;

#[tokio::main]
async fn main() -> Result<()> {
    // Initialize logging (RUST_LOG=debug for debug mode)
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| EnvFilter::new("fluux_agent=info")),
        )
        .init();

    println!(
        r#"
   _____ _                      _                    _
  |  ___| |_   _ _   ___  __   / \   __ _  ___ _ __ | |_
  | |_  | | | | | | | \ \/ /  / _ \ / _` |/ _ \ '_ \| __|
  |  _| | | |_| | |_| |>  <  / ___ \ (_| |  __/ | | | |_
  |_|   |_|\__,_|\__,_/_/\_\/_/   \_\__, |\___|_| |_|\__|
                                     |___/   v{}
"#,
        env!("CARGO_PKG_VERSION")
    );

    // Load configuration
    let config_path = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "config/agent.toml".to_string());

    info!("Loading configuration from {config_path}");
    let config = Config::load(&config_path)?;

    info!("Agent: {}", config.agent.name);
    info!("XMPP mode: {}", config.server.mode_description());
    info!("LLM: {} ({})", config.llm.provider, config.llm.model);
    info!(
        "Allowed JIDs: {}",
        config.agent.allowed_jids.join(", ")
    );

    // Initialize memory
    let memory = Memory::open(&config.memory.path)?;

    // Initialize LLM client
    let llm = AnthropicClient::new(config.llm.clone());

    // Connect to XMPP server (component or C2S, based on config)
    let (event_rx, cmd_tx) = xmpp::connect(
        config.server.clone(),
        config.agent.allowed_jids.clone(),
    ).await?;

    // Launch agentic runtime
    let runtime = AgentRuntime::new(config, llm, memory);
    runtime.run(event_rx, cmd_tx).await?;

    Ok(())
}
