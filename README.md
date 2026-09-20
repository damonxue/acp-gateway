# Agent Gateway

**Run coding agents on your computer. Drive them from your phone.**

[中文文档](README.zh-CN.md) · [Architecture](docs/architecture.md) · [Remote protocol](docs/remote-protocol.md) · [Security](docs/security.md) · [IDE integration](docs/ide-integration.md) · [Chat adapters](docs/chat-adapters.md)

Agent Gateway is a local daemon that speaks the [Agent Client Protocol (ACP)][acp] to
coding agents — Codex, Claude Code, OpenCode, Gemini CLI — and exposes their sessions to
remote clients over an authenticated WebSocket. Close your laptop lid on a running agent,
open your phone, and the session is still there: the same output, the same tool calls, the
same pending permission prompt.

It is **not** a new agent runtime. It is an ACP proxy, a session manager and a remote
transport:

```text
           phone / browser
                 │  wss + single-use ticket
                 ▼
        ┌────────────────────────────────────┐
        │            Agent Gateway              │
        │  ACP proxy · sessions · event log     │
        │  device pairing · tickets · tunnel    │
        └──────────────────┬─────────────────┘
                         │ ACP (JSON-RPC 2.0 over stdio)
                         ▼
            Codex · Claude Code · OpenCode · …
```

## Why this exists

Agents run for minutes. You do not want to sit in front of the machine for all of them,
and you do not want the agent to stop because you walked away. The gateway keeps three
promises:

1. **The session belongs to the gateway, not to your phone.** Disconnecting a client never
   touches the agent process.
2. **Every event is persisted before anyone sees it.** Reconnecting replays exactly what
   was missed — no gaps, no duplicates, no re-run prompts.
3. **Dangerous operations still ask you.** Permission requests travel to whatever client is
   watching, and the agent stays blocked until one of them answers.

## Quick start

```bash
cargo build --release

# 1. Write a starter config to ~/.agent-gateway/config.toml
./target/release/agent-gateway config init

# 2. Point it at an agent you have installed, then check it
$EDITOR ~/.agent-gateway/config.toml
./target/release/agent-gateway config check

# 3. Run the daemon
./target/release/agent-gateway run
```

In another terminal:

```bash
agent-gateway agents                                  # what can be launched
SID=$(agent-gateway sessions new --agent codex)       # start an agent in $PWD
agent-gateway sessions prompt $SID "why do the tests fail?"
agent-gateway sessions watch $SID                     # follow the event stream
```

### From your phone

The gateway serves a web client at **`/app`** — that is the phone UI. Build it once:

```bash
cd web && pnpm install && pnpm build
cargo build --release -p gateway-cli --features web-ui
```

Then:

```bash
agent-gateway pair          # prints a QR code and a 6-digit pairing code
agent-gateway devices list
agent-gateway devices revoke device_…
agent-gateway zed config     # print Zed's agent_servers JSON
agent-gateway zed copy       # copy it with pbcopy/wl-copy/xclip
```

Open `http://127.0.0.1:48100/app` on the same machine, or
`https://<your tunnel hostname>/app` from the phone, and scan the code. The phone
generates its own key, signs for a single-use ticket and connects over the WebSocket — no
account, no third party. See [docs/remote-protocol.md](docs/remote-protocol.md).

### macOS app and installer

```bash
cargo dmg          # "Agent Gateway.app" + a .dmg, web client included
cargo dmg-cli      # same, without the menu bar helper (no macOS SDK needed)
cargo app          # just build the menu bar helper
```

`cargo dmg` needs a macOS SDK to build the AppKit helper. The resulting dmg is
ad-hoc signed, so on another Mac: `xattr -dr com.apple.quarantine "/Applications/Agent Gateway.app"`. Set
`MACOS_SIGNING_KEY` (and `APPLE_NOTARIZATION_*`) and pass `--sign` for a distributable
build.

The optional `app/` target is a small macOS menu bar companion. It has no large
session window: it starts the bundled CLI daemon when the status item opens, and
**Copy Zed agent_servers** calls the same Rust generator as `agent-gateway zed copy`.
**Choose gateway project…** is available for development checkouts; select the
directory containing the workspace `Cargo.toml`, then build `gateway-cli`. The
selection is stored under `~/Library/Application Support/Agent Gateway/`.

The App bundle contains two CLI binaries: `agent-gateway` is the bundle-local
wrapper that Zed invokes, and `agent-gateway-daemon` is the web-enabled daemon
started by the status item. Both link the same `gateway-cli` Rust library.

### Configuration

`~/.agent-gateway/config.toml`; every field is documented in the generated template.

```toml
[gateway]
bind = "127.0.0.1:48100"   # loopback only; remote access goes through the tunnel
data_dir = "~/.agent-gateway"
log_level = "info"

[security]
ticket_ttl = "60s"          # WebSocket tickets are single-use and short-lived
pairing_ttl = "5m"
trust_loopback = true       # local callers may skip credentials

[relay]
endpoint = "https://relay.example.com"

[tunnel]
mode = "token"              # or "config"
token = "…"                 # never written to a log
hostname = "gw.example.com"

[[agents]]
id = "codex"
name = "Codex"
command = "npx"
args = ["-y", "@agentclientprotocol/codex-acp@latest"]
```

The Zed helper emits one entry per configured agent. Its shape is:

```json
{
  "agent_servers": {
    "Codex via Agent Gateway": {
      "default_config_options": { "model": "gpt-6-astra" },
      "type": "custom",
      "command": "/path/to/agent-gateway",
      "args": ["acp-bridge", "--agent", "codex"],
      "env": { "AGENT_GATEWAY_CONFIG": "/Users/me/.agent-gateway/config.toml" }
    }
  }
}
```

Paste it under `agent_servers` in Zed's `settings.json`, or use the menu bar
action/`zed copy` command.

The command path is discovered in this order: `AGENT_GATEWAY_BIN`, the CLI next
to the app bundle, a selected Cargo project (`AGENT_GATEWAY_PROJECT`), the
workspace `target/debug` or `target/release` binary, then `PATH`.
Inside the installed App bundle the first matching sibling is always the
bundle-local `agent-gateway` wrapper.

Agents are **always explicit**. The gateway never scans your machine for executables.

## The two protocols

| | Between | Shape |
|---|---|---|
| **ACP** | gateway ↔ agent | JSON-RPC 2.0 over the agent's stdio, unmodified |
| **Remote protocol** | gateway ↔ phone/browser | JSON over WebSocket — see [docs/remote-protocol.md](docs/remote-protocol.md) |

Keeping them separate is deliberate: the gateway can follow ACP releases without shipping
a new mobile app, and the remote protocol can carry gateway concepts (machines, tickets,
replay cursors) that ACP has no opinion about. Gateway extensions never change standard
ACP semantics.

### Local HTTP API

Loopback only, for the CLI, scripts and IDE plugins:

| Method | Path | Purpose |
|---|---|---|
| `GET` | `/health` | machine, session counts, tunnel/relay health |
| `GET` | `/machines/current` | this machine |
| `GET` | `/agents` | configured agents |
| `GET`/`POST` | `/sessions` | list / create |
| `GET` | `/sessions/{id}` | session plus pending permissions |
| `POST` | `/sessions/{id}/prompt` | send a prompt |
| `POST` | `/sessions/{id}/cancel` | cancel the running turn |
| `POST` | `/sessions/{id}/permission` | answer a permission request |
| `POST` | `/sessions/{id}/close` | close the session, stop the agent |
| `GET` | `/sessions/{id}/events?after_seq=` | replay history |
| `WS` | `/sessions/{id}/stream?after_seq=` | replay + live stream |
| `GET` | `/ahp/status` | local AHP channel and QR login state |
| `POST` | `/ahp/bind` | local explicit binding of an existing session |
| `POST` | `/ahp/unbind` | local channel unbinding |

Reachable from anywhere the gateway is (i.e. through the tunnel), each with its own
credential: `GET /health`, `POST /pairing/consume`, `POST /devices/{id}/ws-ticket`,
`WS /remote?ticket=…`.

## Security model

* The machine's Ed25519 private key never leaves the computer (`0600`, in `identity/`).
* Pairing codes are single-use, short-lived, and carry a nonce the device must echo.
* WebSocket tickets are single-use, short-lived, bound to one device **and** one machine.
* Revoking a device invalidates tickets it already holds.
* A request arriving with tunnel headers is never treated as local, even though
  `cloudflared` connects over loopback.
* Secrets are unprintable by type: tunnel tokens are `Secret`, whose `Debug` and `Display`
  render `***`.
* The relay never receives prompts, agent output, terminal output or source code — push
  notifications carry a *state*, never content.

Details and threat model: [docs/security.md](docs/security.md).

## Project layout

```text
crates/
  gateway-core/     domain: sessions, events, permissions, ports, SessionManager
  gateway-config/   TOML configuration, validation, secrets
  gateway-store/    SQLite adapters for the persistence ports
  gateway-auth/     machine identity, device pairing, WSS tickets
  gateway-acp/      ACP agent runtime, event mapper, client-side fs
  gateway-remote/   local HTTP API + remote WebSocket protocol + /app hosting
  gateway-tunnel/   cloudflared supervisor
  gateway-relay/    relay client, push worker, reference relay server
  gateway-ahp/      outbound Agent Host Protocol / WeChat channel
  gateway-cli/      the `agent-gateway` binary (composition root)
  gateway-wechat/   embedded WeChat Bot adapter
  gateway-lark/     Lark/Feishu signed webhook and message API adapter
  gateway-telegram/ Telegram Bot API long-polling adapter
app/                macOS menu bar companion — its own cargo workspace
web/                phone and browser client (TypeScript + Preact)
xtask/              build tasks behind `cargo dmg`
script/             bundle-mac.sh: .app + .dmg
migrations/         SQLite schema
```

`gateway-core` depends on no transport, no database and no agent implementation: every
external dependency is a trait in `gateway_core::ports` or `gateway_core::agent`. That
inversion is what lets a phone drive a session identically whether the agent was started
by the gateway or, in future, by an IDE.

## Development

```bash
cargo test --workspace      # unit + integration, including a real ACP agent process
cargo clippy --workspace --all-targets
cargo fmt --all
```

The test suite spawns `fake-acp-agent`, a real ACP agent implemented in `gateway-acp`, so
prompts, streaming, tool calls, permission requests and cancellation are exercised over an
actual JSON-RPC stdio connection — no mocks of the protocol.

See [docs/development.md](docs/development.md).

## Status

Working today: agent launch, prompt/cancel, streaming, tool calls, permissions, event
replay, reconnect, device pairing, tickets, the local API, the remote WebSocket protocol,
the cloudflared supervisor, the relay client, a reference relay, and the outbound AHP
channel with explicit session binding and QR status. Telegram long polling and Lark signed
webhook adapters are available with explicit chat/session bindings. See
[docs/ahp.md](docs/ahp.md) and [docs/chat-adapters.md](docs/chat-adapters.md).

Partially implemented: the IDE bridge that lets Zed/VS Code attach to a gateway session
now has an `agent-gateway acp-bridge` entrypoint and a daemon `/bridge` socket, but
multi-session orchestration and richer arbitration are still future work.

Still pending: client-side terminals and real APNs/FCM delivery in the reference relay.

[acp]: https://agentclientprotocol.com/
