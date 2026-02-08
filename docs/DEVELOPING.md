# Development Guide

## Getting Started

### Prerequisites

- Rust ≥ 1.75
- An XMPP server with component protocol enabled (ejabberd, Prosody, Openfire...)
- An Anthropic API key (get one at https://console.anthropic.com/)

### Quick Setup

1. **Configure ejabberd** (for local development):

```yaml
listen:
  -
    port: 5275
    module: ejabberd_service
    access: all
    hosts:
      "agent.localhost":
        password: "secret"
```

2. **Copy and configure the agent**:

```bash
cp config/agent.example.toml config/agent.toml
# Edit config/agent.toml and add your XMPP JID to allowed_jids
```

3. **Set environment variables**:

```bash
export ANTHROPIC_API_KEY="sk-ant-..."
export AGENT_SECRET="secret"
```

4. **Run the agent**:

```bash
cargo run
```

5. **Send a message** from any XMPP client to `agent.localhost`

The agent will:
- Receive your XMPP message
- Send it to Claude API
- Reply back via XMPP

### Configuration Reference

See `config/agent.example.toml` for all available options:

- `[server]` — XMPP component connection settings
- `[llm]` — LLM provider (Anthropic Claude)
- `[agent]` — Agent name and allowed JIDs
- `[memory]` — Conversation memory backend (SQLite)

## Remote Component Development

During development, you can connect your Fluux Agent component to a remote ejabberd server from your laptop. This is a common development pattern for XMPP components.

### ejabberd Server Configuration

Configure your ejabberd server to accept component connections from your development machine:

```yaml
listen:
  -
    port: 5275
    ip: "0.0.0.0"  # or bind to specific interface
    module: ejabberd_service
    access: component_access

acl:
  component_dev:
    ip:
      - "YOUR_LAPTOP_IP/32"  # Replace with your laptop's IP

access_rules:
  component_access:
    - allow: component_dev
    - deny: all

hosts:
  "agent.yourdomain.com":
    password: "strong_random_secret"  # Use a strong random secret!
```

### Firewall Rules

Restrict access to the component port at the firewall level:

```bash
# Only allow your laptop IP to port 5275
iptables -A INPUT -p tcp --dport 5275 -s YOUR_LAPTOP_IP -j ACCEPT
iptables -A INPUT -p tcp --dport 5275 -j DROP
```

### Local Configuration

Update your `config/agent.toml`:

```toml
[server]
host = "your-server-hostname.com"  # or IP address
port = 5275
component_domain = "agent.yourdomain.com"
component_secret = "${AGENT_SECRET}"
```

Set the environment variable:

```bash
export AGENT_SECRET="strong_random_secret"
```

## Security Considerations

### Development
- ✅ Use a **strong random secret** (not "secret" or other weak passwords)
- ✅ Restrict IP at both ejabberd ACL and firewall levels
- ✅ Consider SSH tunnel if on untrusted networks:
  ```bash
  ssh -L 5275:localhost:5275 your-server
  # Then connect to localhost:5275 in config
  ```
- ✅ Use VPN/Tailscale for dynamic IPs

### Production
- Run component on the same host as ejabberd
- Bind ejabberd component listener to `127.0.0.1` only:
  ```yaml
  listen:
    -
      port: 5275
      ip: "127.0.0.1"  # localhost only
      module: ejabberd_service
      access: all
  ```
- Remove IP-based ACL restrictions (localhost-only binding is sufficient)

## Why XEP-0114?

Fluux Agent uses XEP-0114 (Jabber Component Protocol) rather than XEP-0225 (Component Connections):

- ✅ **Widely deployed** — Supported by ejabberd, Prosody, Openfire, and others
- ✅ **Simple authentication** — Stream ID + SHA-1 secret handshake
- ✅ **Perfect for trusted components** — Designed for components on trusted networks
- ✅ **Battle-tested** — 20+ years of production use

XEP-0225 is:
- ❌ **Deferred status** — No activity since 2009
- ❌ **Minimal implementations** — Almost no server support
- ❌ **Unnecessary complexity** — SASL overhead for trusted local components

**Federation happens at the server level**, not the component level. Your agent component routes messages through your local XMPP server, which handles server-to-server (s2s) federation with other XMPP servers.
