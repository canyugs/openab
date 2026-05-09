# WeCom (企业微信) Setup

Connect a WeCom (Enterprise WeChat) bot to OpenAB via the Custom Gateway.

```
WeCom ──POST──▶ Gateway (:8080) ◀──WebSocket── OAB Pod
                                   (OAB connects out)
```

## Prerequisites

- A running OAB instance (with any ACP agent authenticated)
- The Custom Gateway deployed ([gateway/README.md](../gateway/README.md))
- A WeCom enterprise account with admin access

## 1. Create a WeCom App

1. Log in to [WeCom Admin Console](https://work.weixin.qq.com/wework_admin/frame)
2. Go to **应用管理** (App Management) → **自建** (Self-built) → **创建应用** (Create App)
3. Fill in the app name and description, select visible scope
4. After creation, note down:
   - **AgentId** — on the app detail page
   - **Secret** — click to view/copy on the app detail page
5. Go to **我的企业** (My Enterprise) → copy the **企业ID** (Corp ID)

## 2. Configure the Callback URL

1. In the app detail page, scroll to **接收消息** (Receive Messages)
2. Click **设置API接收** (Set API Receive)
3. Fill in:
   - **URL**: `https://your-gateway-host/webhook/wecom` (must be HTTPS)
   - **Token**: click "随机获取" (Random Generate) or set your own
   - **EncodingAESKey**: click "随机获取" (Random Generate) or set your own
4. **Do NOT click Save yet** — you need the gateway running first to verify the URL

## 3. Configure the Gateway

Set the following environment variables:

| Variable | Required | Description |
|---|---|---|
| `WECOM_CORP_ID` | Yes | Enterprise Corp ID (from My Enterprise page) |
| `WECOM_AGENT_ID` | Yes | App Agent ID |
| `WECOM_SECRET` | Yes | App Secret |
| `WECOM_TOKEN` | Yes | Callback Token (from step 2) |
| `WECOM_ENCODING_AES_KEY` | Yes | Callback EncodingAESKey (43 characters) |
| `WECOM_WEBHOOK_PATH` | No | Webhook path (default: `/webhook/wecom`) |
| `WECOM_STREAMING_ENABLED` | No | Stream replies via "thinking" placeholder + recall + resend (default: `false`). WeCom has no edit-message API; enabling this causes a brief client flicker during streaming. |
| `WECOM_DEBOUNCE_SECS` | No | Quiet-period seconds before flushing buffered streamed text (default: `3`, minimum: `1` — `0` is silently ignored by Helm's truthy check and disables the buffer purpose) |

```bash
docker run -d --name openab-gateway \
  -e WECOM_CORP_ID="ww1234567890abcdef" \
  -e WECOM_AGENT_ID="1000002" \
  -e WECOM_SECRET="your-app-secret" \
  -e WECOM_TOKEN="your-callback-token" \
  -e WECOM_ENCODING_AES_KEY="your-43-char-encoding-aes-key" \
  -p 8080:8080 \
  ghcr.io/openabdev/openab-gateway:latest
```

For Kubernetes with Helm, see [`charts/openab/values.yaml`](../charts/openab/values.yaml) — set values under `agents.<name>.gateway.wecom`.

## 4. Verify the Callback URL

Once the gateway is running with the correct env vars:

1. Go back to the WeCom Admin Console → App → 接收消息 → 设置API接收
2. Click **保存** (Save)
3. WeCom will send a verification request to your URL — if the gateway decrypts and responds correctly, you'll see "保存成功" (Save Successful)

If verification fails:
- Check that the gateway is reachable over HTTPS
- Verify `WECOM_TOKEN` and `WECOM_ENCODING_AES_KEY` match exactly what's shown in the WeCom console
- Check gateway logs for errors

## 5. Configure OAB

```toml
[gateway]
url = "ws://openab-gateway:8080/ws"
platform = "wecom"
allow_all_channels = true
allow_all_users = true

[agent]
command = "claude-agent-acp"
args = []
working_dir = "/home/node"
env = { CLAUDE_CODE_OAUTH_TOKEN = "${OPENAB_AUTH_TOKEN}" }

[pool]
max_sessions = 10
```

| Key | Required | Description |
|---|---|---|
| `url` | Yes | WebSocket URL of the gateway |
| `platform` | No | Session key namespace (default: `wecom`) |
| `allow_all_channels` | No | Allow messages from all channels (default: `false`) |
| `allow_all_users` | No | Allow messages from all users (default: `false`) |

## 6. Expose the Gateway (HTTPS)

WeCom requires a publicly accessible HTTPS URL for callbacks.

### Option A: Zeabur (one-click HTTPS for quick testing)

Deploy the gateway to [Zeabur](https://zeabur.com) — HTTPS is automatically provisioned.

### Option B: Cloudflare Tunnel

```bash
cloudflared tunnel --url http://localhost:8080
```

### Option C: Reverse proxy (production)

Use nginx, Caddy, or a cloud load balancer with TLS termination pointing to the gateway's `:8080`.

## 7. Set Trusted IP (Optional)

For production, restrict the callback to WeCom's IP ranges:

1. In the WeCom Admin Console → App → **企业可信IP** (Trusted IP)
2. Add your gateway's public IP

## Usage

Send a direct message to the bot in the WeCom mobile or desktop app:

```
你好，帮我解释一下这段代码
```

The bot will reply directly in the same conversation.

> **Note on group chats:** WeCom self-built enterprise apps only deliver **1:1 direct messages** to the callback URL. Group chat messages are not forwarded by this API path; group chat support would require the `appchat` API (not yet implemented). For group chat use cases, see the WeCom AI Bot WebSocket API as a future adapter.

## Features

| Feature | Status |
|---|---|
| Direct message (1:1) | ✅ |
| Text message receive/reply | ✅ |
| AES-256-CBC message decryption | ✅ |
| Message deduplication | ✅ |
| Auto-split long replies (2048 bytes) | ✅ |
| Access token auto-refresh | ✅ |
| Image receive | ✅ |
| Text file receive | ✅ |
| Streaming replies (thinking placeholder + debounce flush) | ✅ |
| Group chat | ❌ Not supported (callback API limitation) |
| Voice/video messages | Planned |
| Markdown card replies | Planned |

## Production Hardening

The gateway does no application-level rate limiting on `/webhook/wecom`. Each request triggers an XML envelope parse, a SHA1 signature computation, and (if signature passes) AES-256-CBC decryption. A 5-minute timestamp freshness check rejects stale callbacks before any crypto runs, so old replays are cheap to drop, but fresh-but-invalid requests still consume CPU.

Run the gateway behind a reverse proxy or load balancer that enforces rate limits at the IP / connection level:

| Layer | Example |
|---|---|
| Edge / CDN | Cloudflare WAF rate limiting rules on `/webhook/wecom` |
| Cloud LB | AWS ALB rate-based rules, GCP Cloud Armor |
| Reverse proxy | nginx `limit_req_zone`, Caddy `rate_limit` directive |

In addition, restrict the callback URL to WeCom's published IP ranges via the **企业可信IP** (Trusted IP) list in the WeCom Admin Console. This is the most effective control because all legitimate callbacks originate from those ranges.

### Redact `corpsecret` from access logs

WeCom's `gettoken` API mandates `corpsecret` as a query parameter (the protocol does not support a header alternative). The gateway itself does not log this URL, but if the gateway sits behind a reverse proxy with default access logging enabled, the secret will appear in access logs. Configure the proxy to redact query strings on `/cgi-bin/gettoken` outbound calls (or sanitize at log-shipping time).

### Known limitations

- **Streaming task lifetime on shutdown** — the optional streaming mode (`WECOM_STREAMING_ENABLED=true`) spawns one debounce task per in-flight reply. On SIGTERM these tasks are dropped by the tokio runtime; any text buffered but not yet flushed is lost. The agent will typically re-emit on the next interaction. If you need flush-on-shutdown semantics, keep streaming off (default) so each reply is sent synchronously.
- **DedupeCache eviction is lazy** — entries are TTL-checked on lookup and bulk-evicted only when the cache reaches `DEDUPE_MAX_SIZE` (10K). For low-traffic deployments the HashMap can sit just below the cap with stale entries; max memory is bounded (~500 KB) and the dedup window itself is honored, so this does not affect correctness.

## Troubleshooting

| Symptom | Cause | Fix |
|---|---|---|
| Callback verification fails | Token/EncodingAESKey mismatch | Double-check values match WeCom console exactly |
| Bot receives but doesn't reply | Agent auth token not configured | Set `env = { CLAUDE_CODE_OAUTH_TOKEN = "${OPENAB_AUTH_TOKEN}" }` in OAB config |
| Intermittent "no response" | WeCom disabled callback after errors | Re-save callback config in WeCom console to re-verify |
| "IP not in whitelist" on reply | Trusted IP not set | Add gateway IP to app's trusted IP list, or leave it empty for dev |
