# OpenAB — Open Agent Broker

[![Stars](https://img.shields.io/github/stars/openabdev/openab?style=flat-square)](https://github.com/openabdev/openab) [![GitHub Release](https://img.shields.io/github/v/release/openabdev/openab?style=flat-square&logo=github)](https://github.com/openabdev/openab/releases/latest) ![License](https://img.shields.io/badge/license-MIT-A374ED?style=flat-square)

![OpenAB banner](images/banner.jpg)

A lightweight, secure, cloud-native ACP harness that bridges **Discord, Slack**, and any [Agent Client Protocol](https://github.com/anthropics/agent-protocol)-compatible coding CLI (Kiro CLI, Claude Code, Codex, Gemini, OpenCode, Copilot CLI, etc.) over stdio JSON-RPC — delivering the next-generation development experience.

🪼 **Join our community!** Come say hi on Discord — we'd love to have you: **[🪼 OpenAB — Official](https://discord.gg/YNksK9M6)** 🎉

```
┌──────────────┐  Gateway WS   ┌──────────────┐  ACP stdio    ┌──────────────┐
│   Discord    │◄─────────────►│              │──────────────►│  coding CLI  │
│   User       │               │    openab    │◄── JSON-RPC ──│  (acp mode)  │
├──────────────┤  Socket Mode  │    (Rust)    │               └──────────────┘
│   Slack      │◄─────────────►│              │
│   User       │               └──────────────┘
└──────────────┘
```

## Demo

![openab demo](images/demo.png)

## Features

- **Multi-platform** — supports Discord and Slack, run one or both simultaneously
- **Pluggable agent backend** — swap between Kiro CLI, Claude Code, Codex, Gemini, OpenCode, Copilot CLI via config
- **@mention trigger** — mention the bot in an allowed channel to start a conversation
- **Thread-based multi-turn** — auto-creates threads; no @mention needed for follow-ups
- **Edit-streaming** — live-updates the Discord message every 1.5s as tokens arrive
- **Emoji status reactions** — 👀→🤔→🔥/👨‍💻/⚡→👍+random mood face
- **Session pool** — one CLI process per thread, auto-managed lifecycle
- **ACP protocol** — JSON-RPC over stdio with tool call, thinking, and permission auto-reply support
- **Kubernetes-ready** — Dockerfile + k8s manifests with PVC for auth persistence
- **Voice message STT** — auto-transcribes Discord voice messages via Groq, OpenAI, or local Whisper server ([docs/stt.md](docs/stt.md))

## Quick Start

### 1. Create a Bot

<details>
<summary><strong>Discord</strong></summary>

See [docs/discord-bot-howto.md](docs/discord-bot-howto.md) for a detailed step-by-step guide.

</details>

<details>
<summary><strong>Slack</strong></summary>

See [docs/slack-bot-howto.md](docs/slack-bot-howto.md) for a detailed step-by-step guide.

</details>

### 2. Install with Helm (Kiro CLI — default)

```bash
helm repo add openab https://openabdev.github.io/openab
helm repo update

helm install openab openab/openab \
  --set agents.kiro.discord.botToken="$DISCORD_BOT_TOKEN" \
  --set-string 'agents.kiro.discord.allowedChannels[0]=YOUR_CHANNEL_ID'

# Slack
helm install openab openab/openab \
  --set agents.kiro.slack.enabled=true \
  --set agents.kiro.slack.botToken="$SLACK_BOT_TOKEN" \
  --set agents.kiro.slack.appToken="$SLACK_APP_TOKEN" \
  --set-string 'agents.kiro.slack.allowedChannels[0]=C0123456789'
```

### 3. Authenticate (first time only)

```bash
kubectl exec -it deployment/openab-kiro -- kiro-cli login --use-device-flow
kubectl rollout restart deployment/openab-kiro
```

### 4. Use

In your Discord channel:
```
@YourBot explain this code
```

The bot creates a thread. After that, just type in the thread — no @mention needed.

**Slack:** `@YourBot explain this code` in a channel — same thread-based workflow as Discord.

## Other Agents

| Agent | CLI | ACP Adapter | Guide |
|-------|-----|-------------|-------|
| Kiro (default) | `kiro-cli acp` | Native | [docs/kiro.md](docs/kiro.md) |
| Claude Code | `claude-agent-acp` | [@agentclientprotocol/claude-agent-acp](https://github.com/agentclientprotocol/claude-agent-acp) | [docs/claude-code.md](docs/claude-code.md) |
| Codex | `codex-acp` | [@zed-industries/codex-acp](https://github.com/zed-industries/codex-acp) | [docs/codex.md](docs/codex.md) |
| Gemini | `gemini --acp` | Native | [docs/gemini.md](docs/gemini.md) |
| OpenCode | `opencode acp` | Native | [docs/opencode.md](docs/opencode.md) |
| Copilot CLI ⚠️ | `copilot --acp --stdio` | Native | [docs/copilot.md](docs/copilot.md) |
| Cursor | `cursor-agent acp` | Native | [docs/cursor.md](docs/cursor.md) |

> 🔧 Running multiple agents? See [docs/multi-agent.md](docs/multi-agent.md)

## Local Development

```bash
cp config.toml.example config.toml
# Edit config.toml with your bot token and channel ID

export DISCORD_BOT_TOKEN="your-token"
cargo run
```

## Configuration Reference

```toml
[discord]
bot_token = "${DISCORD_BOT_TOKEN}"   # supports env var expansion
allowed_channels = ["123456789"]      # channel ID allowlist
# allowed_users = ["987654321"]       # user ID allowlist (empty = all users)

[slack]
bot_token = "${SLACK_BOT_TOKEN}"     # Bot User OAuth Token (xoxb-...)
app_token = "${SLACK_APP_TOKEN}"     # App-Level Token (xapp-...) for Socket Mode
allowed_channels = ["C0123456789"]   # channel ID allowlist (empty = allow all)
# allowed_users = ["U0123456789"]    # user ID allowlist (empty = allow all)

[agent]
command = "kiro-cli"                  # CLI command
args = ["acp", "--trust-all-tools"]   # ACP mode args
working_dir = "/tmp"                  # agent working directory
env = {}                              # extra env vars passed to the agent

[pool]
max_sessions = 10                     # max concurrent sessions
session_ttl_hours = 24                # idle session TTL

[reactions]
enabled = true                        # enable emoji status reactions
remove_after_reply = false            # remove reactions after reply
```

<details>
<summary>Full reactions config</summary>

```toml
[reactions.emojis]
queued = "👀"
thinking = "🤔"
tool = "🔥"
coding = "👨‍💻"
web = "⚡"
done = "🆗"
error = "😱"

[reactions.timing]
debounce_ms = 700
stall_soft_ms = 10000
stall_hard_ms = 30000
done_hold_ms = 1500
error_hold_ms = 2500
```

</details>

## Environment Variable Configuration

For PaaS deployments (Zeabur, Railway, Fly.io, etc.) where mounting a `config.toml` is awkward, OpenAB can run with **zero config file** — just set `OPENAB_*` env vars. The file-based path is unchanged; env-only mode activates only when no config file is found and `OPENAB_AGENT_COMMAND` is set.

**Required:** `OPENAB_AGENT_COMMAND`

| Variable | Maps to | Notes |
|----------|---------|-------|
| `OPENAB_AGENT_COMMAND` | `[agent] command` | **Required.** e.g. `kiro-cli`, `claude-agent-acp` |
| `OPENAB_AGENT_ARGS` | `[agent] args` | Comma-separated, e.g. `acp,--trust-all-tools`. Values cannot contain literal commas. |
| `OPENAB_AGENT_WORKING_DIR` | `[agent] working_dir` | Defaults to `/tmp` |
| `OPENAB_AGENT_ENV_<KEY>` | `[agent] env.<KEY>` | Passed through to the child CLI, e.g. `OPENAB_AGENT_ENV_ANTHROPIC_API_KEY` |
| `OPENAB_DISCORD_BOT_TOKEN` | `[discord] bot_token` | Presence activates the Discord adapter |
| `OPENAB_DISCORD_ALLOWED_CHANNELS` | `[discord] allowed_channels` | Comma-separated channel IDs |
| `OPENAB_DISCORD_ALLOWED_USERS` | `[discord] allowed_users` | Comma-separated user IDs |
| `OPENAB_DISCORD_ALLOW_BOT_MESSAGES` | `[discord] allow_bot_messages` | `off` \| `mentions` \| `all` |
| `OPENAB_DISCORD_TRUSTED_BOT_IDS` | `[discord] trusted_bot_ids` | Comma-separated |
| `OPENAB_DISCORD_ALLOW_USER_MESSAGES` | `[discord] allow_user_messages` | `involved` (default) \| `mentions` |
| `OPENAB_SLACK_BOT_TOKEN` | `[slack] bot_token` | Slack adapter activates only when both Slack tokens are set |
| `OPENAB_SLACK_APP_TOKEN` | `[slack] app_token` | App-Level Token (`xapp-...`) for Socket Mode |
| `OPENAB_SLACK_ALLOWED_CHANNELS` / `ALLOWED_USERS` / `ALLOW_BOT_MESSAGES` / `TRUSTED_BOT_IDS` / `ALLOW_USER_MESSAGES` | `[slack] ...` | Same shape as Discord |
| `OPENAB_POOL_MAX_SESSIONS` | `[pool] max_sessions` | Integer. Defaults to `10`. Invalid values fail at startup. |
| `OPENAB_POOL_SESSION_TTL_HOURS` | `[pool] session_ttl_hours` | Integer. Defaults to `4`. Invalid values fail at startup. |

Reactions and STT cannot be configured via env vars yet — if you need those, use a `config.toml`.

**Precedence:** when a config file exists, file-based config is used (env-var mode does not merge on top). Env-var mode is a fallback for file-less deployments, not an override layer.

**Example (Zeabur / Railway):**

```bash
OPENAB_AGENT_COMMAND=kiro-cli
OPENAB_AGENT_ARGS=acp,--trust-all-tools
OPENAB_DISCORD_BOT_TOKEN=...
OPENAB_DISCORD_ALLOWED_CHANNELS=111222333,444555666
```

## Kubernetes Deployment

The Docker image bundles both `openab` and `kiro-cli` in a single container.

```
┌─ Kubernetes Pod ──────────────────────────────────────┐
│  openab (PID 1)                                       │
│    └─ kiro-cli acp --trust-all-tools (child process)  │
│       ├─ stdin  ◄── JSON-RPC requests                 │
│       └─ stdout ──► JSON-RPC responses                │
│                                                       │
│  PVC (/data)                                          │
│    ├─ ~/.kiro/                  (settings, sessions)  │
│    └─ ~/.local/share/kiro-cli/  (OAuth tokens)        │
└───────────────────────────────────────────────────────┘
```

### Build & Push

```bash
docker build -t openab:latest .
docker tag openab:latest <your-registry>/openab:latest
docker push <your-registry>/openab:latest
```

### Deploy without Helm

```bash
kubectl create secret generic openab-secret \
  --from-literal=discord-bot-token="your-token"

kubectl apply -f k8s/configmap.yaml
kubectl apply -f k8s/pvc.yaml
kubectl apply -f k8s/deployment.yaml
```

| Manifest | Purpose |
|----------|---------|
| `k8s/deployment.yaml` | Single-container pod with config + data volume mounts |
| `k8s/configmap.yaml` | `config.toml` mounted at `/etc/openab/` |
| `k8s/secret.yaml` | `DISCORD_BOT_TOKEN` injected as env var |
| `k8s/pvc.yaml` | Persistent storage for auth + settings |

## Project Structure

```
├── Dockerfile          # multi-stage: rust build + debian-slim runtime with kiro-cli
├── config.toml.example # example config with all agent backends
├── k8s/                # Kubernetes manifests
└── src/
    ├── main.rs         # entrypoint: multi-adapter startup, cleanup, shutdown
    ├── adapter.rs      # ChatAdapter trait, AdapterRouter (platform-agnostic)
    ├── config.rs       # TOML config + ${ENV_VAR} expansion
    ├── discord.rs      # DiscordAdapter: serenity EventHandler + ChatAdapter impl
    ├── slack.rs        # SlackAdapter: Socket Mode + ChatAdapter impl
    ├── media.rs        # shared image resize/compress + STT download
    ├── format.rs       # message splitting, thread name shortening
    ├── reactions.rs    # status reaction controller (debounce, stall detection)
    └── acp/
        ├── protocol.rs # JSON-RPC types + ACP event classification
        ├── connection.rs # spawn CLI, stdio JSON-RPC communication
        └── pool.rs     # session key → AcpConnection map
```

## Inspired By

- [sample-acp-bridge](https://github.com/aws-samples/sample-acp-bridge) — ACP protocol + process pool architecture
- [OpenClaw](https://github.com/openclaw/openclaw) — StatusReactionController emoji pattern

## License

MIT
