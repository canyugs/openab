# Google Chat Setup

Connect a Google Chat app to OpenAB via the Custom Gateway.

```
Google Chat ──POST──▶ Gateway (:8080) ◀──WebSocket── OAB Pod
                                          (OAB connects out)
```

## Prerequisites

- **A Google Workspace (Business or Enterprise) account** — required by Google to configure the Chat API. Regular `@gmail.com` consumer accounts cannot create Google Chat apps. Workspace Individual or Business Starter is the cheapest qualifying tier. See [Configure the Google Chat API](https://developers.google.com/workspace/chat/configure-chat-api).
- A running OAB instance (with kiro-cli or any ACP agent authenticated)
- The Custom Gateway deployed ([gateway/README.md](../gateway/README.md))
- A Google Cloud project with the Google Chat API enabled
- A Google Cloud Service Account (JSON key recommended; no special IAM roles needed)

## 1. Create a Google Chat App

1. Go to the [Google Cloud Console](https://console.cloud.google.com/) and create or select a project.
2. Enable the **Google Chat API** under **APIs & Services → Library**.
3. Go to **APIs & Services → Google Chat API → Configuration**:
   - **App name**: your bot name (e.g. "OpenAB")
   - **Avatar URL**: any public image URL
   - **Description**: anything
   - **Interactive features**: Enable
   - **Connection settings**: select **App URL** and enter your gateway's webhook URL:
     ```
     https://your-gateway-host/webhook/googlechat
     ```
   - **Visibility**: select the users or domains that can use the bot
4. Click **Save**.

## 2. Create a Service Account

Google Chat uses a service account to authenticate outbound API calls (bot replies).

1. Go to **IAM & Admin → Service Accounts** → **Create Service Account**.
2. Name it (e.g. `openab-google-chat`) and grant it no special roles.
3. After creation, click the service account → **Keys** → **Add Key** → **Create New Key** → JSON.
4. Save the downloaded JSON file securely.

## 3. Configure the Gateway

The gateway supports two authentication methods for sending replies:

### Option A: Service Account Key (recommended — auto-refresh)

Pass the service account JSON key directly. The gateway handles JWT signing and token refresh automatically.

```bash
# Via JSON string
docker run -d --name openab-gateway \
  -e GOOGLE_CHAT_ENABLED=true \
  -e GOOGLE_CHAT_SA_KEY_JSON='{"type":"service_account","client_email":"...","private_key":"..."}' \
  -e GATEWAY_WS_TOKEN="your-ws-auth-token" \
  -p 8080:8080 \
  ghcr.io/openabdev/openab-gateway:latest

# Via file path
docker run -d --name openab-gateway \
  -e GOOGLE_CHAT_ENABLED=true \
  -e GOOGLE_CHAT_SA_KEY_FILE="/secrets/service-account.json" \
  -v /path/to/service-account.json:/secrets/service-account.json:ro \
  -e GATEWAY_WS_TOKEN="your-ws-auth-token" \
  -p 8080:8080 \
  ghcr.io/openabdev/openab-gateway:latest
```

### Option B: Static Access Token (for quick testing)

Generate a token manually. It expires after 1 hour.

```bash
docker run -d --name openab-gateway \
  -e GOOGLE_CHAT_ENABLED=true \
  -e GOOGLE_CHAT_ACCESS_TOKEN="ya29.c..." \
  -e GATEWAY_WS_TOKEN="your-ws-auth-token" \
  -p 8080:8080 \
  ghcr.io/openabdev/openab-gateway:latest
```

### Local development

```bash
export GOOGLE_CHAT_ENABLED=true
export GOOGLE_CHAT_SA_KEY_FILE="/path/to/service-account.json"
cargo run --release
```

## 4. Expose the Gateway (for local dev)

Google Chat requires a public HTTPS endpoint for webhooks.

### Cloudflare Tunnel (quickest)

```bash
cloudflared tunnel --url http://localhost:8080
# Copy the https://xxx.trycloudflare.com URL
```

Then update the webhook URL in the Google Chat API Configuration page:
```
https://xxx.trycloudflare.com/webhook/googlechat
```

### Reverse proxy (production)

Use nginx, Caddy, or a cloud load balancer with TLS termination pointing to the gateway's `:8080`.

## 5. Configure OAB

```toml
[gateway]
url = "ws://openab-gateway:8080/ws"
platform = "googlechat"
allow_all_channels = true
allow_all_users = true

[agent]
command = "kiro-cli"
args = ["acp", "--trust-all-tools"]
working_dir = "/home/agent"
```

## Features

### Supported

- **DM chat** — opt-in via `GOOGLE_CHAT_ALLOW_DM=true`; send a direct message to the bot, get an AI agent response
- **Space chat** — add the bot to a Google Chat Space, @mention it to start a conversation; optionally restricted via `GOOGLE_CHAT_ALLOWED_SPACES`
- **Access control** — `GOOGLE_CHAT_ALLOWED_USERS` gates which human senders the bot will respond to (empty = all)
- **Bot-to-bot mesh** — `GOOGLE_CHAT_ALLOW_BOTS` + `_TRUSTED_BOT_IDS` + `_MAX_BOT_TURNS` (mirrors `feishu`); off by default. Hard safety cap of 10 turns when `allow_bots=all`
- **Thread replies** — in Spaces, bot replies are posted in the same thread as the user's message (note: @mention is required for every message in a Space, even within a thread — this is a Google Chat platform limitation)
- **`argument_text` extraction** — strips the @mention prefix to get the clean user message
- **Bot message filtering** — bot messages (`user_type: "BOT"`) are filtered at the gateway level
- **Message splitting** — long replies (>4096 chars) are automatically split at newline/space boundaries
- **Token auto-refresh** — service account JWT tokens are refreshed automatically before expiry
- **Markdown formatting** — replies are converted via `markdown_to_gchat` to Google Chat's native formatting:
  - Bold: `**text**` / `__text__` → `*text*`
  - Italic: `*text*` → `_text_` (single-underscore `_text_` passes through)
  - Strikethrough: `~~text~~` → `~text~`
  - Headings: `# / ## / ###` → `*text*` (rendered as bold)
  - Links: `[text](url)` → `<url|text>`
  - Inline code, fenced code blocks: pass through unchanged
  - Tables and other unsupported syntax pass through as-is
- **Streaming (edit_message)** — when OAB streaming is enabled, the bot edits its initial reply in-place as tokens arrive (typewriter effect)
- **Inbound attachments** — image, text file, and audio attachments are downloaded via Google Chat Media API and forwarded to the agent as base64 (PR #731 pattern):
  - Images: resized to ≤1200px JPEG (q75); GIFs preserved. Max 10 MB.
  - Text files: only known text extensions (`.txt`, `.md`, `.json`, `.py`, `.rs`, etc.). Max 512 KB.
  - Audio: forwarded as-is for STT processing by core. Max 25 MB.
- **Drive-linked attachments** — opt-in via `GOOGLE_CHAT_ENABLE_DRIVE_ATTACHMENTS=true`. Files uploaded to Google Drive and then attached to a Chat message are fetched via the Drive v3 API and pass through the same image / file / audio post-processing as `UPLOADED_CONTENT` attachments. **Requires** the SA to be authorized for the `drive.readonly` OAuth scope. Google-native types (Docs / Sheets / Slides) are skipped — they need the Drive `export` API which is out of scope for this version.
- **Outbound attachments** — bot can send back attachments via the Google Chat Media API (`spaces.messages.attachments.upload` + `messages.create` `attachment[]`):
  - MIME whitelist mirrors inbound: `image/*` (≤10 MB), `audio/*` (≤25 MB), `text/*` (≤1 MB), `application/pdf` (≤10 MB).
  - Upload failures are logged and skipped — text delivery is not blocked by a failed attachment; the partial failure is reported back via the `error` field of the `GatewayResponse`.
  - When a reply is split across multiple messages (text > 4096 chars), attachments are piggybacked on the first chunk only.
  - `video/*` is intentionally skipped (matches inbound behavior).
  - Uses the same `chat.bot` OAuth scope as the message API — no SA changes required.

### Not Supported

- **Reactions** — Google Chat API does not support message reactions on behalf of bots
- **Outbound Drive attachments** — bot replies use direct upload (`UPLOADED_CONTENT`); the bot cannot attach a file by Drive ID
- **Google-native Drive types (Docs / Sheets / Slides)** — would need the Drive `export` API; not implemented
- **`cardsV2` image cards** — outbound supports `attachment[]` (uploaded bytes) only; rendering an external URL as an image card is not implemented

## Environment Variables (Gateway)

### Core

| Variable | Required | Default | Description |
|---|---|---|---|
| `GOOGLE_CHAT_ENABLED` | Yes | `false` | Set to `true` or `1` to enable the adapter |
| `GOOGLE_CHAT_AUDIENCE` | Recommended | — | JWT audience for webhook verification — set to your full webhook URL (e.g. `https://your-domain.com/webhook/googlechat`) |
| `GOOGLE_CHAT_SA_KEY_JSON` | No | — | Service account key JSON string (enables auto-refresh) |
| `GOOGLE_CHAT_SA_KEY_FILE` | No | — | Path to service account key JSON file (alternative to `SA_KEY_JSON`) |
| `GOOGLE_CHAT_ACCESS_TOKEN` | No | — | Static OAuth2 access token (fallback, expires in 1 hour) |
| `GOOGLE_CHAT_WEBHOOK_PATH` | No | `/webhook/googlechat` | Webhook endpoint path |

### Access control (`messaging.md` Layer 0–1)

| Variable | Default | Description |
|---|---|---|
| `GOOGLE_CHAT_ALLOW_DM` | `false` | Set to `true` to forward DM (direct message) conversations. **Off by default** — DMs are opt-in (Layer 0). |
| `GOOGLE_CHAT_ALLOWED_SPACES` | — (allow all) | CSV of Space resource names (`spaces/AAAA...`) that may reach the agent. Empty = all Spaces allowed. |
| `GOOGLE_CHAT_ALLOWED_USERS` | — (allow all) | CSV of user IDs (stripped of `users/` prefix) that may message the bot. Empty = all users. Applies to **human** senders only; bot senders go through the mesh gates below. |

### Bot-to-bot mesh (`messaging.md` Layer 4)

| Variable | Default | Description |
|---|---|---|
| `GOOGLE_CHAT_ALLOW_BOTS` | `off` | `off` / `mentions` / `all`. `off` (default) blocks all bot→bot messages. `mentions` and `all` are functionally equivalent on Google Chat because the platform only delivers Space messages to a bot when @mentioned. |
| `GOOGLE_CHAT_TRUSTED_BOT_IDS` | — (any bot) | CSV of bot user IDs (stripped of `users/` prefix). When non-empty, only listed bots may participate (applies under `mentions`/`all`). |
| `GOOGLE_CHAT_MAX_BOT_TURNS` | `20` | Maximum consecutive bot-to-bot turns per Space before responses stop. Reset on any human message. Hard-capped to `10` when `GOOGLE_CHAT_ALLOW_BOTS=all`. |
| `GOOGLE_CHAT_ALLOW_USER_MESSAGES` | `involved` | `involved` / `mentions` / `multibot-mentions` — kept for parity with `feishu`. Note: Google Chat platform forces @mention in every Space message, so these modes behave equivalently for normal Space messaging. |
| `GOOGLE_CHAT_SESSION_TTL_HOURS` | `24` | TTL for participated-thread cache (hours). `0` disables participation tracking. |

### Drive attachments (opt-in)

| Variable | Default | Description |
|---|---|---|
| `GOOGLE_CHAT_ENABLE_DRIVE_ATTACHMENTS` | `false` | Set to `true` to download Drive-sourced attachments. **Requires the SA to be authorized for the `https://www.googleapis.com/auth/drive.readonly` scope** — see SA scope note below. Token exchange will fail with a 401/403 if the SA hasn't been pre-authorized. |

**SA scope note**: When `GOOGLE_CHAT_ENABLE_DRIVE_ATTACHMENTS=true`, the SA JWT requests `chat.bot drive.readonly`. For a deployment that hasn't already authorized the SA for Drive, this is a **breaking change at token-exchange time**, not at handler time — once the flag is on, the SA must be re-authorized via Workspace Admin's Domain-wide Delegation page or the Drive API will reject the OAuth token request entirely. Leave the flag off if your deployment doesn't need Drive attachments.

## Security: Webhook Verification

Google Chat signs every webhook request with a JWT Bearer token. The gateway verifies this token to ensure requests come from Google Chat specifically (not just any Google service).

**Setup:**

In the Google Chat API **Configuration** page, leave **Authentication Audience** at its default — **HTTP Endpoint URL**. Then set `GOOGLE_CHAT_AUDIENCE` to your full webhook URL:

```bash
export GOOGLE_CHAT_AUDIENCE="https://your-domain.com/webhook/googlechat"
```

The gateway will:
- Reject requests without a valid `Authorization: Bearer <jwt>` header
- Verify the JWT signature against Google's public keys (JWKS, cached for 1 hour)
- Validate `iss == https://accounts.google.com` and `aud` matches the configured webhook URL
- Validate `email` ends with `@gcp-sa-gsuiteaddons.iam.gserviceaccount.com` (proves the token came from Google Chat, not another Google service)

If `GOOGLE_CHAT_AUDIENCE` is not set, the gateway logs a warning and accepts all requests (insecure — for local development only).

> **Note:** Only the "HTTP Endpoint URL" Authentication Audience mode is supported. The "Project Number" mode uses a different JWT flow that this adapter does not implement.

## Troubleshooting

| Problem | Fix |
|---|---|
| Bot doesn't respond | Check `GOOGLE_CHAT_ENABLED=true` is set. Check gateway logs for parse errors. |
| "not responding" in Google Chat | Ensure the gateway returns a `200` with `{}` body. Check gateway is reachable via the webhook URL. |
| Replies not sent | Use `GOOGLE_CHAT_SA_KEY_JSON` or `GOOGLE_CHAT_SA_KEY_FILE` for auto-refresh. If using static token, check it hasn't expired (1-hour TTL). |
| Replies not in thread | Verify the thread name is passed correctly. The gateway appends `?messageReplyOption=REPLY_MESSAGE_FALLBACK_TO_NEW_THREAD` automatically. |
| Bot responds to its own messages | Bot messages have `user_type: "BOT"` and are filtered out automatically. |
| Webhook returns 400 | Check the Google Chat API configuration uses **App URL** (not Dialogflow or Cloud Pub/Sub). The webhook expects the v2 envelope format with a `chat` wrapper. |

## References

- [Google Chat API Documentation](https://developers.google.com/workspace/chat/api/reference/rest)
- [Google Chat App Setup](https://developers.google.com/workspace/chat/overview)
- [Service Account Authentication](https://developers.google.com/workspace/chat/authenticate-authorize-chat-app)
