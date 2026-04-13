# mcp-proxy

A reverse proxy that aggregates multiple [MCP](https://modelcontextprotocol.io) servers into a single endpoint. Define your servers once, switch between profiles (work/home/minimal), and connect from any MCP client.

## Quick Start

```bash
cargo install --path .

# Create a config and edit it
mcp-proxy init
$EDITOR ~/.config/mcp-proxy/servers.json

# Start the proxy
mcp-proxy serve
```

## Configuration

Config lives at `~/.config/mcp-proxy/servers.json` by default (override with `-c` or `CONFIG_PATH`).

```json
{
  "servers": {
    "github": {
      "command": "github-mcp-server",
      "args": ["stdio"],
      "env": { "GITHUB_PERSONAL_ACCESS_TOKEN": "${GITHUB_TOKEN}" }
    },
    "memory": {
      "command": "mcp-server-memory"
    }
  },
  "profiles": {
    "work": {
      "description": "Work context",
      "include": ["github", "memory"],
      "servers": {
        "github": {
          "env": { "GITHUB_PERSONAL_ACCESS_TOKEN": "${GITHUB_TOKEN_WORK}" }
        }
      }
    },
    "minimal": {
      "description": "Just memory",
      "include": ["memory"]
    }
  }
}
```

See [`config/servers.example.json`](config/servers.example.json) for a full example.

### Servers

Each server entry has:

| Field       | Required | Description                                                   |
| ----------- | -------- | ------------------------------------------------------------- |
| `command`   | yes      | Binary to run                                                 |
| `args`      | no       | Command-line arguments                                        |
| `env`       | no       | Environment variables (`${VAR}` expanded from process env)    |
| `envToggle` | no       | Env var name — set to `false` to disable this server          |
| `install`   | no       | Install method for Docker generation (`npm`, `pip`, `binary`) |

### Profiles

Profiles control which servers run and can override their parameters:

- **`include`** / **`exclude`** — filter which base servers are active
- **`servers`** — override env/args of base servers, or add new ones
- **`description`** — human-readable label

Profile server overrides merge onto the base: `env` is shallow-merged (your keys win), `args`/`command` replace if specified.

## Usage

```bash
# Start with a profile
mcp-proxy serve --profile work

# List available profiles
mcp-proxy profiles

# Switch profile (persisted across restarts)
mcp-proxy switch work

# Install into MCP clients (Augment, Claude CLI, Claude Desktop)
mcp-proxy clients

# Check what's installed where
mcp-proxy status

# Generate Dockerfile + .env.example
mcp-proxy generate
```

## Client Setup

`mcp-proxy clients` interactively installs into detected MCP clients. For HTTP-capable clients (Augment, Claude CLI), it writes:

```json
{ "type": "http", "url": "http://localhost:3000/mcp" }
```

For stdio-only clients (Claude Desktop), it writes a bridge command:

```json
{
  "command": "mcp-proxy",
  "args": ["bridge", "--url", "http://localhost:3000/mcp"]
}
```

## Remote Usage

Run `mcp-proxy serve` on a remote server. Clients connect via the bridge:

```json
{
  "command": "mcp-proxy",
  "args": ["bridge", "--url", "http://remote:3000/mcp", "--profile", "work"]
}
```

To forward local credentials to the remote server:

```json
{
  "command": "mcp-proxy",
  "args": [
    "bridge",
    "--url",
    "http://remote:3000/mcp",
    "--forward-env",
    "GITHUB_TOKEN"
  ],
  "env": { "GITHUB_TOKEN": "ghp_my_token" }
}
```

Each unique (profile + env) combination gets its own isolated backend pool on the server, so multiple users can share one instance with different credentials.

## Environment Variables

| Variable        | Description                       |
| --------------- | --------------------------------- |
| `CONFIG_PATH`   | Path to servers.json              |
| `MCP_PROFILE`   | Default profile                   |
| `MCP_PORT`      | HTTP port (default: 3000)         |
| `MCP_TRANSPORT` | Transport mode: `http` or `stdio` |
