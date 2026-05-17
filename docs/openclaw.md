# OpenClaw

[OpenClaw](https://github.com/openclaw/openclaw) is a self-hosted AI agent
gateway. OpenAB connects to it via the `openclaw acp` bridge, which speaks ACP
over stdio and forwards prompts to a running OpenClaw gateway over WebSocket.

Unlike other ACP backends, OpenClaw requires a **separately-running gateway
service** — the OpenAB container does not embed the gateway. Provider API keys
(OpenAI, Anthropic, etc.), agent definitions, and model selection all live in
the gateway, not in OpenAB.

## Architecture

```
Discord ──► openab ──stdio──► openclaw acp ──WS──► openclaw gateway ──HTTPS──► LLM
                              (inside container)   (separate service)
```

## Prerequisites

- A running OpenClaw gateway, reachable from the OpenAB container. The
  upstream [Quick Start](https://github.com/openclaw/openclaw#quick-start)
  walks through `openclaw onboard --install-daemon` + `openclaw gateway`.
- A gateway token. Generated on first gateway start; persisted at
  `~/.openclaw/gateway.token` on the gateway host.

## Docker Image

```bash
docker build -f Dockerfile.openclaw -t openab-openclaw:latest .
```

The image installs the `openclaw` npm package globally and requires Node 22.16+.

## Helm Install

```bash
helm install openab openab/openab \
  --set agents.kiro.enabled=false \
  --set agents.openclaw.discord.enabled=true \
  --set agents.openclaw.discord.botToken="$DISCORD_BOT_TOKEN" \
  --set-string 'agents.openclaw.discord.allowedChannels[0]=YOUR_CHANNEL_ID' \
  --set agents.openclaw.image=ghcr.io/openabdev/openab-openclaw:latest \
  --set agents.openclaw.command=openclaw \
  --set-json 'agents.openclaw.args=["acp","--url","ws://openclaw-gateway:18789","--session","agent:main:main"]' \
  --set agents.openclaw.workingDir=/home/node \
  --set agents.openclaw.env.OPENCLAW_GATEWAY_TOKEN="$OPENCLAW_GATEWAY_TOKEN"
```

> Set `agents.kiro.enabled=false` to disable the default Kiro agent.

## Manual config.toml

```toml
[agent]
command = "openclaw"
args = [
  "acp",
  "--url", "ws://openclaw-gateway:18789",
  "--session", "agent:main:main",
]
working_dir = "/home/node"
env = { OPENCLAW_GATEWAY_TOKEN = "${OPENCLAW_GATEWAY_TOKEN}" }
```

### Required flags

| Flag | Purpose |
|---|---|
| `--url <ws://...>` | Gateway WebSocket URL. Use `wss://` for TLS. |
| `--session <key>` | OpenClaw session key — see [Sessions and Models](#sessions-and-models). |
| `--token-file <path>` *(alt. to env var)* | Path to a file holding the gateway token. Useful for Kubernetes Secret mounts. |

### Gateway authentication

Provide the shared secret via **one** of:

- Env var: `OPENCLAW_GATEWAY_TOKEN`
- Token file: `--token-file /path/to/token`
- CLI flag: `--token <value>` (avoid — visible in process list)

## Sessions and Models

OpenAB's `/model` slash command **does not** select LLM models for the OpenClaw
backend.

OpenClaw routes by **session key** (e.g., `agent:main:main`), and each session
resolves to an **agent definition** on the gateway side. The agent definition
determines which provider and which model to use.

To switch models or providers:

1. Edit the agent definition in the gateway's `~/.openclaw/openclaw.json`, or
2. Change `--session` in `config.toml` to point at a different agent and
   restart the pod.

In-band ACP options the bridge does pass through:

| Option | Effect |
|---|---|
| `thought_level` | Verbosity of agent thinking output |
| `reasoning_level` | Reasoning effort hint to the model |
| `verbose_level` / `trace_level` | Diagnostic detail |
| `fast_mode` | Latency-optimized routing |
| `response_usage` | Include token usage in responses |
| `timeout_seconds` | Per-prompt timeout |

## Capabilities and Limits

| Feature | Supported |
|---|---|
| Text prompts | ✅ |
| Image attachments (inbound) | ✅ |
| Audio attachments | ❌ |
| Embedded context resources | ✅ |
| `session/load` | ✅ (only for sessions created through the bridge) |
| Per-session MCP servers | ❌ — rejected by the bridge; configure on the gateway |
| `/reset`, `/agent` slash commands | ✅ |
| `/model` slash command | ⚠️ See [Sessions and Models](#sessions-and-models) |
| Prompt size cap | 2 MB |

## Persisted Paths (PVC)

| Path | Contents |
|------|----------|
| `/home/node/.openclaw/` | Bridge state (small — token file if used) |

The bulk of OpenClaw state — provider API keys, agent definitions, session
transcripts, event ledger — lives on the **gateway side**, not in the OpenAB
container.

## Troubleshooting

### Bridge exits immediately with `gateway closed before ready`

The gateway is not reachable at the configured `--url`. Check:

- The gateway service is running and listening on the expected port
  (default 18789).
- DNS resolution for the gateway hostname works from inside the OpenAB pod.
- The token (env var or `--token-file`) matches the gateway's configured token.

### `invalid token` / authentication failed

Token mismatch. Regenerate the token on the gateway host per the upstream
docs, then update `OPENCLAW_GATEWAY_TOKEN` (or the file referenced by
`--token-file`) and restart the OpenAB pod.

### Messages take a long time, then return empty

Likely a gateway-side issue — the agent definition references a provider
without an API key, or the model name is invalid. The bridge cannot see this;
only the gateway logs it:

```bash
kubectl logs deployment/openclaw-gateway --tail=200
```

### `/model gpt-4o` has no effect

Expected — see [Sessions and Models](#sessions-and-models). Change the
gateway-side agent definition or use a different `--session` key.

### Per-session MCP servers don't work

The bridge rejects per-session `mcpServers` in `session/new`. Configure MCP
servers at the gateway level instead — see the upstream OpenClaw docs.
