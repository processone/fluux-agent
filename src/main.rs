mod agent;
mod backoff;
mod config;
mod llm;
mod sandbox;
mod skills;
mod xmpp;

use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{anyhow, Result};
use tracing::{error, info, warn};
use tracing_subscriber::EnvFilter;

use crate::agent::files::FileDownloader;
use crate::agent::memory::Memory;
use crate::agent::runtime::AgentRuntime;
use crate::backoff::Backoff;
use crate::config::Config;
use crate::llm::AnthropicClient;
use crate::skills::builtin::WebSearchSkill;
use crate::skills::SkillRegistry;
use crate::xmpp::component::DisconnectReason;

/// How long a connection must be up before we consider it "stable"
/// and reset the backoff to initial values.
const STABILITY_THRESHOLD: Duration = Duration::from_secs(60);

/// Maximum consecutive transient failures before giving up.
const MAX_RECONNECT_ATTEMPTS: u32 = 20;

fn print_help() {
    println!(
        "\
fluux-agent v{}

An AI agent runtime that connects to any XMPP server.

USAGE:
    fluux-agent [OPTIONS] [CONFIG_PATH]

ARGUMENTS:
    CONFIG_PATH    Path to TOML configuration file [default: config/agent.toml]

OPTIONS:
    -h, --help       Print this help message and exit
    -V, --version    Print version and exit

ENVIRONMENT VARIABLES:
    Variables are referenced in the config file via ${{VAR_NAME}} syntax.

    RUST_LOG              Log level filter for tracing
                          (e.g. debug, fluux_agent=debug,warn)
    ANTHROPIC_API_KEY     API key for Anthropic Claude models
                          (from https://console.anthropic.com/)
    AGENT_SECRET          Shared secret for XMPP component connection
                          (XEP-0114 mode)
    BOT_PASSWORD          XMPP account password for client connection
                          (C2S mode)
    TAVILY_API_KEY        API key for Tavily web search
                          (from https://tavily.com)
    PERPLEXITY_API_KEY    API key for Perplexity Sonar search
                          (from https://perplexity.ai)

EXAMPLES:
    fluux-agent                          # uses config/agent.toml
    fluux-agent /etc/fluux/agent.toml    # custom config path
    RUST_LOG=debug fluux-agent           # with debug logging",
        env!("CARGO_PKG_VERSION"),
    );
}

#[tokio::main]
async fn main() -> Result<()> {
    // Handle --help / --version before anything else
    for arg in std::env::args().skip(1) {
        match arg.as_str() {
            "--version" | "-V" => {
                println!("fluux-agent v{}", env!("CARGO_PKG_VERSION"));
                std::process::exit(0);
            }
            "--help" | "-h" => {
                print_help();
                std::process::exit(0);
            }
            _ => {}
        }
    }

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
    if config.agent.allowed_domains.is_empty() {
        info!(
            "Allowed domains: {} (default — own domain only)",
            config.server.domain()
        );
    } else {
        info!(
            "Allowed domains: {}",
            config.agent.allowed_domains.join(", ")
        );
    }
    if !config.rooms.is_empty() {
        info!(
            "MUC rooms: {}",
            config.rooms.iter().map(|r| format!("{} (as {})", r.jid, r.nick)).collect::<Vec<_>>().join(", ")
        );
    }

    // Initialize components that persist across reconnections
    let memory = Arc::new(Memory::open(&config.memory.path)?);
    let llm = AnthropicClient::new(config.llm.clone());
    let file_downloader = Arc::new(FileDownloader::with_tls_verify(3, config.server.tls_verify()));
    let mut skills = SkillRegistry::new();

    // Register builtin skills based on config
    if let Some(ref ws_config) = config.skills.web_search {
        info!(
            "Registering builtin skill: web_search (provider: {})",
            ws_config.provider
        );
        skills.register(Box::new(WebSearchSkill::new(ws_config)));
    }

    info!("Skills: {} registered", skills.len());
    let runtime = AgentRuntime::new(config.clone(), llm, memory, file_downloader, skills);

    let mut backoff = Backoff::new(
        Duration::from_secs(2),
        Duration::from_secs(60),
        2,
    );

    // ── Reconnection loop ──────────────────────────────────────────
    loop {
        info!(
            "Connecting to XMPP server (attempt {})...",
            backoff.attempt + 1
        );

        match xmpp::connect(
            config.server.clone(),
            config.agent.allowed_jids.clone(),
        )
        .await
        {
            Ok((event_rx, cmd_tx)) => {
                let connected_at = Instant::now();

                // Run the agent runtime until the connection drops
                let disconnect_reason = tokio::select! {
                    result = runtime.run(event_rx, cmd_tx) => {
                        match result {
                            Ok(reason) => reason,
                            Err(e) => {
                                error!("Runtime error: {e}");
                                DisconnectReason::ConnectionLost
                            }
                        }
                    }
                    _ = tokio::signal::ctrl_c() => {
                        info!("Shutdown signal received, exiting");
                        return Ok(());
                    }
                };

                // Session replaced by another client — do NOT reconnect
                // (would cause a ping-pong between the two clients)
                if matches!(disconnect_reason, DisconnectReason::Conflict) {
                    error!("Session replaced by another client (conflict), exiting");
                    return Err(anyhow!("Session replaced by another client (conflict)"));
                }

                // Non-retriable stream errors
                if let DisconnectReason::StreamError(ref condition) = disconnect_reason {
                    warn!("Stream error: {condition}");
                }

                // Reset backoff if the connection was stable (up long enough)
                if connected_at.elapsed() >= STABILITY_THRESHOLD {
                    backoff.reset();
                    info!("Connection was stable, backoff reset");
                } else {
                    warn!(
                        "Connection lasted only {}s",
                        connected_at.elapsed().as_secs()
                    );
                }

                warn!("XMPP connection lost, preparing to reconnect...");
            }
            Err(e) => {
                // Permanent errors — exit immediately
                if !e.is_retriable() {
                    error!("Permanent connection error: {e}");
                    return Err(anyhow!("Cannot connect: {e}"));
                }

                warn!("Connection failed: {e}");

                if backoff.exceeded_max_attempts(MAX_RECONNECT_ATTEMPTS) {
                    error!(
                        "Exceeded {} reconnection attempts, giving up",
                        MAX_RECONNECT_ATTEMPTS
                    );
                    return Err(anyhow!(
                        "Max reconnection attempts ({MAX_RECONNECT_ATTEMPTS}) exceeded"
                    ));
                }
            }
        }

        // Wait before retrying, but allow graceful shutdown during the wait
        let delay = backoff.next_delay();
        info!(
            "Reconnecting in {}s (attempt {})...",
            delay.as_secs(),
            backoff.attempt + 1
        );

        tokio::select! {
            _ = tokio::time::sleep(delay) => {}
            _ = tokio::signal::ctrl_c() => {
                info!("Shutdown signal received during backoff, exiting");
                return Ok(());
            }
        }
    }
}
