# mcp-proxy

A reverse proxy that aggregates multiple [MCP](https://modelcontextprotocol.io) servers into a single endpoint. Define your servers once, switch between profiles, and connect from any MCP client — locally or remotely.

## Features

- **Aggregation** — expose tools from many MCP servers through one endpoint
- **Profiles** — switch between server sets (work / home / minimal) without restarting
- **Hot-reload** — edit `servers.json` and changes apply automatically
- **Backend isolation** — per-session, per-credential, or global sharing modes
- **Auto-restart** — crashed backends recover transparently (configurable retries)
- **Idle reaping** — unused session backends are cleaned up after a timeout
- **Custom tools** — define lightweight shell or HTTP tools directly in config
- **Streamable-HTTP** — SSE-based progress streaming during tool discovery
- **Stdio bridge** — connect stdio-only clients (Claude Desktop) to the HTTP server
- **Docker support** — auto-build Docker images for servers with `install` configs
- **Client installer** — one command to register with Augment, Claude CLI, Claude Desktop

## Quick Start

```bash
cargo install --path .

# Create a config and edit it
mcp-proxy init
$EDITOR ~/.config/mcp-proxy/servers.json

# Start the proxy
mcp-proxy serve
```

The proxy listens on `http://localhost:3000/mcp` by default.

## Configuration

Config lives at `~/.config/mcp-proxy/servers.json` (override with `-c` or `CONFIG_PATH`).

```json
{
  "servers": {
    "github": {
      "command": "github-mcp-server",
      "args": ["stdio"],
      "env": { "GITHUB_PERSONAL_ACCESS_TOKEN": "${GITHUB_TOKEN}" }
    },
    "memory": {
      "command": "mcp-server-memory",
      "shared": "global"
    }
  }
}
```

See [`config/servers.example.json`](config/servers.example.json) for a full example.

### Server Options

| Field              | Required | Default     | Description                                                        |
| ------------------ | -------- | ----------- | ------------------------------------------------------------------ |
| `command`          | yes      |             | Binary to run                                                      |
| `args`             | no       | `[]`        | Command-line arguments                                             |
| `env`              | no       | `{}`        | Environment variables (`${VAR}` expanded from process env)         |
| `envToggle`        | no       |             | Env var name — set to `false` or `0` to disable this server       |
| `shared`           | no       | `"session"` | Backend sharing mode: `session`, `credentials`, or `global`        |
| `timeoutSecs`      | no       | `30`        | Per-tool RPC timeout in seconds                                    |
| `idleTimeoutSecs`  | no       | `900`       | Seconds before an idle backend is reaped (15 min)                  |
| `autoRestart`      | no       | `true`      | Restart the backend automatically on crash                         |
| `maxRestarts`      | no       | `5`         | Maximum restart attempts before giving up                          |
| `includeTools`     | no       | `[]`        | Glob patterns — only matching tools are exposed                    |
| `excludeTools`     | no       | `[]`        | Glob patterns — matching tools are hidden (applied after include)  |
| `toolAliases`      | no       | `{}`        | Rename tools: `{ "original_name": "new_name" }`                   |
| `install`          | no       |             | Auto-install config (`npm`, `pip`, or `binary`)                    |
| `runtime`          | no       | `"docker"`  | Where to run installed servers: `docker` or `local`                |

### Sharing Modes

Controls how backend processes are shared across client connections:

| Mode          | Description                                                                 |
| ------------- | --------------------------------------------------------------------------- |
| `session`     | One process per client session — fully isolated (default)                   |
| `credentials` | Shared across sessions with matching env vars for this server               |
| `global`      | Single instance shared by all clients — never reaped                        |

### Profiles

Profiles live in a separate file (`profiles.json`, adjacent to `servers.json`) and control which servers run. Manage them via CLI or edit directly.

- **`include`** / **`exclude`** — filter which base servers are active
- **`servers`** — override env/args of base servers, or add profile-only servers
- **`description`** — human-readable label

Profile server overrides merge onto the base: `env` is shallow-merged (your keys win), `args`/`command` replace if specified.

### Custom Tools

Define lightweight tools directly in config without a full MCP server:

```json
{
  "customTools": {
    "ping": {
      "type": "shell",
      "description": "Ping a host",
      "inputSchema": { "type": "object", "properties": { "host": { "type": "string" } } },
      "command": "ping -c 1 {{host}}"
    },
    "lookup": {
      "type": "http",
      "description": "Look up a domain",
      "inputSchema": { "type": "object", "properties": { "domain": { "type": "string" } } },
      "url": "https://api.example.com/lookup?domain={{domain}}"
    }
  }
}
```

Custom tools are prefixed with `custom_` to avoid name collisions (e.g. `custom_ping`).

## CLI Reference

```
mcp-proxy [OPTIONS] <COMMAND>

Options:
  -c, --config <PATH>    Path to servers.json [env: CONFIG_PATH]
      --profile <NAME>   Profile to use [env: MCP_PROFILE]

Commands:
  serve       Start the aggregator (default: http on port 3000)
  bridge      Stdio-to-HTTP bridge for stdio-only clients
  server      Manage servers (add, remove, edit, list)
  profile     Manage profiles (add, remove, set, unset, switch, list)
  clients     Install/sync mcp-proxy into editor clients
  health      Check health of a running hub
  init        Create a starter servers.json
  test        Validate config and verify MCP handshakes
```

### `serve`

```bash
mcp-proxy serve                          # HTTP on :3000
mcp-proxy serve -p 8080                  # custom port
mcp-proxy serve -t stdio                 # stdio mode (single client)
mcp-proxy serve --profile work           # start with a profile
```

### `bridge`

Connects a stdio-only client to a running HTTP server:

```bash
mcp-proxy bridge --url http://localhost:3000/mcp
mcp-proxy bridge --url http://remote:3000/mcp --forward-env GITHUB_TOKEN
mcp-proxy bridge --url http://remote:3000/mcp --servers github,memory
```

### `server`

```bash
mcp-proxy server list
mcp-proxy server add my-server --command my-mcp-server --args stdio
mcp-proxy server edit my-server --env API_KEY='${MY_KEY}'
mcp-proxy server remove my-server
```

### `profile`

```bash
mcp-proxy profile list
mcp-proxy profile add work --description "Work context"
mcp-proxy profile set work github --env GITHUB_TOKEN='${GITHUB_TOKEN_WORK}'
mcp-proxy profile switch work
mcp-proxy profile unset work github
mcp-proxy profile remove work
```

## Client Setup

`mcp-proxy clients` interactively installs into detected MCP clients. All clients use the **bridge** (stdio) transport — this gives clean session lifecycle, env forwarding, and graceful disconnect regardless of whether the client supports HTTP natively.

The entry written to each client config looks like:

```json
{
  "type": "stdio",
  "command": "/path/to/mcp-proxy",
  "args": ["bridge"]
}
```

Supported clients: **Claude Desktop**, **Claude CLI**, **Augment**.

## Remote / Multi-User Setup

Run `mcp-proxy serve` on a remote server. Clients connect via the bridge:

```json
{
  "command": "mcp-proxy",
  "args": ["bridge", "--url", "http://remote:3000/mcp", "--forward-env", "GITHUB_TOKEN"],
  "env": { "GITHUB_TOKEN": "ghp_my_token" }
}
```

Each unique combination of profile + forwarded env gets its own isolated backend pool on the server. Two users with different `GITHUB_TOKEN` values get separate `github` processes; a `global` server like `memory` is shared by everyone.

## Health Check

```bash
mcp-proxy health            # CLI check
curl localhost:3000/health  # HTTP endpoint
```

Returns JSON with per-backend status (ready, crashed, tool count, restart count) and session count — all read lock-free via atomics.

## Environment Variables

| Variable        | Description                                   |
| --------------- | --------------------------------------------- |
| `CONFIG_PATH`   | Path to `servers.json`                        |
| `MCP_PROFILE`   | Default profile                               |
| `MCP_PORT`      | HTTP port (default: `3000`)                   |
| `MCP_TRANSPORT` | Transport mode: `http` or `stdio`             |
| `MCP_BIND`      | Bind address (default: `127.0.0.1`)           |

## Architecture

```
┌─────────────┐   HTTP/SSE    ┌──────────────┐   stdio    ┌─────────────┐
│  MCP Client ├──────────────►│  mcp-proxy   ├───────────►│ MCP Server  │
│  (Augment)  │◄──────────────┤  (hub)       │◄───────────┤ (github)    │
└─────────────┘               │              │            └─────────────┘
                              │   ┌──────┐   │   stdio    ┌─────────────┐
┌─────────────┐   stdio       │   │ Pool ├───┼───────────►│ MCP Server  │
│  MCP Client ├──► bridge ───►│   └──────┘   │◄───────────┤ (memory)    │
│  (Desktop)  │◄── bridge ◄───┤              │            └─────────────┘
└─────────────┘               └──────────────┘
```

- **Pool** manages backend lifecycle: spawn, handshake, health tracking, idle reaping, crash recovery
- **Sessions** are tracked via `X-MCP-Session-ID` headers with a 30-minute idle TTL
- **Config watcher** detects file changes and hot-reloads without dropping in-flight requests
- **Tool names** are prefixed with the server name (e.g. `github_create_issue`) to avoid collisions
