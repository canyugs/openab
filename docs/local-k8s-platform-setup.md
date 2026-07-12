# Plan 1: WeCom & Google Chat (Local Kubernetes)

> Part of the [local k8s dev plans](local-k8s-dev-plans.md) roadmap. **Plan 1** — unified gateway platforms on Docker Desktop Kubernetes.

Deploy OpenAB gateway platforms to **Docker Desktop Kubernetes** for end-to-end testing with real WeCom and Google Chat callbacks.

This guide assumes **Unified Mode** (v0.9.0+): a single `openab` binary embeds `/webhook/wecom` and `/webhook/googlechat`. No separate `openab-gateway` container and no `[gateway]` config section.

For platform-specific feature notes, see also [wecom.md](wecom.md) and [google-chat.md](google-chat.md).

## Architecture

```
WeCom / Google Chat
    │ HTTPS POST
    ▼
Cloudflare quick tunnel (in-cluster cloudflared)
    │
    ▼
openab-wecom:8080          openab-googlechat:8080
  /webhook/wecom             /webhook/googlechat
    │                            │
    └─ echo agent (dev) ─────────┘
       or kiro/claude (production)
```

Each platform profile is an independent Deployment + Service in namespace `openab-local`. They can run side by side.

## Prerequisites

| Requirement | Notes |
|-------------|-------|
| Docker Desktop with Kubernetes enabled | Default context: `docker-desktop` |
| `kubectl` | Must point at `docker-desktop` |
| Local image registry (recommended) | `localhost:5555` — Docker Desktop 4.80+ may not bridge images into k8s without it |
| [openab-control-plane](https://github.com/openabdev/openab-control-plane) (optional) | Clone as a sibling repo for `dev-tunnel-k8s.sh` |

Verify the registry:

```bash
curl -fsS http://localhost:5555/v2/ && echo "registry ok"
```

## Scripts

| Script | Purpose |
|--------|---------|
| `scripts/dev-build-image.sh` | Build any `Dockerfile.unified` target (`agentcore`, `kiro`, …) |
| `scripts/dev-up-wecom.sh` | Deploy `openab-wecom` |
| `scripts/dev-up-googlechat.sh` | Deploy `openab-googlechat` |
| `scripts/dev-up-unified.sh` | Deploy `openab-unified` (WeCom + Google Chat in one pod) |
| `scripts/_lib.sh` | Shared deploy helpers (sourced by profile scripts) |

Legacy wrappers (still work):

- `scripts/dev-build-unified-image.sh` → `dev-build-image.sh --target agentcore`
- `scripts/dev-deploy-unified-k8s.sh` → `dev-up-unified.sh`

## Step 1: Build the image

For gateway smoke tests, `agentcore` (echo agent) is enough:

```bash
scripts/dev-build-image.sh --target agentcore --tag agentcore-dev
```

For a real agent, build the matching target (e.g. `kiro`) and point the deploy script at that tag.

## Step 2: Deploy platform profiles

### WeCom only

```bash
scripts/dev-up-wecom.sh --build
# or, if image already built:
IMAGE=localhost:5555/openab:agentcore-dev scripts/dev-up-wecom.sh
```

### Google Chat only

```bash
scripts/dev-up-googlechat.sh --build
```

### Both in one pod (single HTTPS base URL)

```bash
scripts/dev-up-unified.sh --build
```

Verify pods:

```bash
kubectl -n openab-local get pods,svc
kubectl -n openab-local logs deployment/openab-wecom --tail=20
kubectl -n openab-local logs deployment/openab-googlechat --tail=20
```

Expected log lines:

- `unified: wecom adapter enabled path=/webhook/wecom`
- `unified: googlechat adapter enabled path=/webhook/googlechat`
- `unified webhook server listening addr=0.0.0.0:8080`

## Step 3: Expose HTTPS (required)

WeCom and Google Chat both require a **public HTTPS** callback URL. Local `port-forward` is only for smoke tests, not for platform configuration.

Use the in-cluster Cloudflare quick tunnel from openab-control-plane (no host port-forward needed):

```bash
# From the openab repo root; openab-control-plane is a sibling directory.
```

### Option A — Separate profiles (two tunnel URLs)

```bash
# Google Chat
KUBE_NAMESPACE=openab-local \
ORIGIN_URL=http://openab-googlechat:8080 \
WEBHOOK_PATH=/webhook/googlechat \
NAME=cloudflared-googlechat \
../openab-control-plane/scripts/dev-tunnel-k8s.sh

# WeCom
KUBE_NAMESPACE=openab-local \
ORIGIN_URL=http://openab-wecom:8080 \
WEBHOOK_PATH=/webhook/wecom \
NAME=cloudflared-wecom \
../openab-control-plane/scripts/dev-tunnel-k8s.sh
```

The script prints:

```
tunnel URL: https://xxx.trycloudflare.com
webhook URL: https://xxx.trycloudflare.com/webhook/googlechat
```

Copy the **webhook URL** into the platform console.

> Quick tunnel URLs change on every tunnel restart. Update the platform config whenever you redeploy cloudflared.

### Option B — Unified pod (one tunnel URL, both webhooks)

```bash
KUBE_NAMESPACE=openab-local \
ORIGIN_URL=http://openab-unified:8080 \
WEBHOOK_PATH=/webhook/googlechat \
NAME=cloudflared-unified \
../openab-control-plane/scripts/dev-tunnel-k8s.sh
```

Same base URL serves both:

- `https://xxx.trycloudflare.com/webhook/googlechat`
- `https://xxx.trycloudflare.com/webhook/wecom`

### Local smoke test (no tunnel)

```bash
kubectl -n openab-local port-forward svc/openab-googlechat 18081:8080
curl http://127.0.0.1:18081/health

curl -sS -X POST http://127.0.0.1:18081/webhook/googlechat \
  -H "content-type: application/json" \
  -d '{"message":{"name":"spaces/abc/messages/1","text":"hi","sender":{"name":"users/123","displayName":"Test","type":"HUMAN"}},"space":{"name":"spaces/abc"}}'
```

With default deny-all trust, logs should show a request-access reply (dry-run if no SA key).

## Step 4: Kubernetes secrets

Deploy scripts create **placeholder** secrets on first run. Replace them before connecting real platforms.

### WeCom

Collect from the WeCom Admin Console (see [WeCom setup](#wecom-admin-console) below):

| Secret key | Source |
|------------|--------|
| `WECOM_CORP_ID` | 我的企业 → 企业ID |
| `WECOM_AGENT_ID` | App detail → AgentId |
| `WECOM_SECRET` | App detail → Secret |
| `WECOM_TOKEN` | 接收消息 → API接收 → Token |
| `WECOM_ENCODING_AES_KEY` | 接收消息 → API接收 → EncodingAESKey (43 chars) |

```bash
kubectl -n openab-local create secret generic openab-wecom-platform \
  --from-literal=WECOM_CORP_ID='ww...' \
  --from-literal=WECOM_AGENT_ID='1000002' \
  --from-literal=WECOM_SECRET='...' \
  --from-literal=WECOM_TOKEN='...' \
  --from-literal=WECOM_ENCODING_AES_KEY='...' \
  --dry-run=client -o yaml | kubectl apply -f -

kubectl -n openab-local rollout restart deployment/openab-wecom
```

If using `openab-unified`, update secret `openab-platform` instead (same keys).

### Google Chat — Service Account key

1. Create a service account in GCP and download the JSON key ([Google Chat setup](#google-chat-cloud-console) below).
2. Store it as a Kubernetes secret:

```bash
kubectl -n openab-local create secret generic openab-googlechat-sa \
  --from-file=sa-key.json=/path/to/service-account.json \
  --dry-run=client -o yaml | kubectl apply -f -
```

3. Mount the key and set env vars on the deployment:

```bash
TUNNEL_WEBHOOK='https://xxx.trycloudflare.com/webhook/googlechat'

kubectl -n openab-local set env deployment/openab-googlechat \
  GOOGLE_CHAT_SA_KEY_FILE=/secrets/googlechat/sa-key.json \
  GOOGLE_CHAT_AUDIENCE="$TUNNEL_WEBHOOK"

kubectl -n openab-local patch deployment openab-googlechat --type=json -p='[
  {"op":"add","path":"/spec/template/spec/volumes/-","value":{"name":"googlechat-sa","secret":{"secretName":"openab-googlechat-sa"}}},
  {"op":"add","path":"/spec/template/spec/containers/0/volumeMounts/-","value":{"name":"googlechat-sa","mountPath":"/secrets/googlechat","readOnly":true}}
]'
```

`GOOGLE_CHAT_AUDIENCE` must match the **full App URL** exactly when JWT verification is enabled.

Alternative: pass the JSON inline via `GOOGLE_CHAT_SA_KEY_JSON` (see [google-chat.md](google-chat.md)).

## Step 5: Trust gate

Gateway platforms default to **deny-all** for users (`GATEWAY_ALLOW_ALL_USERS=false`). Unknown senders receive a request-access message with their platform ID.

### Development — allow everyone

```bash
scripts/dev-up-googlechat.sh --allow-all-users
scripts/dev-up-wecom.sh --allow-all-users
```

### Production-style — allowlist

```bash
# Google Chat sender IDs look like users/123456789
kubectl -n openab-local set env deployment/openab-googlechat \
  GATEWAY_ALLOW_ALL_USERS=false \
  GATEWAY_ALLOWED_USERS='users/123456789'

# WeCom user IDs (enterprise-specific format)
kubectl -n openab-local set env deployment/openab-wecom \
  GATEWAY_ALLOW_ALL_USERS=false \
  GATEWAY_ALLOWED_USERS='your-wecom-user-id'
```

See [ADR: identity trust](adr/identity-trust-none.md) for the full trust model.

## WeCom Admin Console

1. Log in to [WeCom Admin](https://work.weixin.qq.com/wework_admin/frame).
2. **我的企业** → copy **企业ID** (`WECOM_CORP_ID`).
3. **应用管理 → 自建 → 创建应用** → note **AgentId** and **Secret**.
4. App detail → **接收消息 → 设置API接收**:
   - **URL**: `https://<tunnel-host>/webhook/wecom`
   - **Token**: generate or set your own → `WECOM_TOKEN`
   - **EncodingAESKey**: generate (43 characters) → `WECOM_ENCODING_AES_KEY`
5. **Update the k8s secret and restart the pod first** — do not click Save until the pod has the matching Token/AESKey and the tunnel is reachable.
6. Click **保存** — WeCom sends a verification request. Success shows **保存成功**.

### WeCom troubleshooting

| Symptom | Check |
|---------|-------|
| URL verification fails | Tunnel running? Secret values match console exactly? Pod logs? |
| No messages after verify | Trust gate — add your user ID or `--allow-all-users` |
| Only DMs work | Expected — self-built apps receive 1:1 direct messages via this callback path |

## Google Chat Cloud Console

**Requires Google Workspace** (Business or Enterprise). Consumer `@gmail.com` accounts cannot configure Chat apps.

1. [Google Cloud Console](https://console.cloud.google.com/) → create/select a project.
2. Enable **Google Chat API** (APIs & Services → Library).
3. **APIs & Services → Google Chat API → Configuration**:
   - Enable **Interactive features**
   - **Connection settings → App URL**: `https://<tunnel-host>/webhook/googlechat`
   - **Visibility**: users or domains who can find the bot
   - **Save**
4. **IAM & Admin → Service Accounts** → Create → Keys → JSON download.
5. Apply the SA secret and `GOOGLE_CHAT_AUDIENCE` as in [Step 4](#google-chat--service-account-key).
6. Open Google Chat, find the app, send a DM or @mention it in a space.

### Google Chat troubleshooting

| Symptom | Check |
|---------|-------|
| `401 unauthorized` on webhook | Set `GOOGLE_CHAT_AUDIENCE` to the full App URL |
| Replies not sent | SA key mounted? Logs show `dry-run, no credentials`? |
| Request-access message | Trust gate — add `users/...` to `GATEWAY_ALLOWED_USERS` |
| @mention required in spaces | Google Chat platform limitation |

## Recommended setup order

| # | Action |
|---|--------|
| 1 | `scripts/dev-build-image.sh` + `scripts/dev-up-*.sh` |
| 2 | Start cloudflared tunnel(s), copy HTTPS webhook URL(s) |
| 3 | Create/update k8s secrets, restart pods |
| 4 | Configure WeCom callback URL → Save (verification) |
| 5 | Configure Google Chat App URL + mount SA key |
| 6 | Set trust gate (`--allow-all-users` or allowlist) |
| 7 | Send a test message on each platform |

## Cleanup

```bash
scripts/dev-up-wecom.sh --delete
scripts/dev-up-googlechat.sh --delete
scripts/dev-up-unified.sh --delete

KUBE_NAMESPACE=openab-local NAME=cloudflared-googlechat \
  ../openab-control-plane/scripts/dev-tunnel-k8s.sh --delete
KUBE_NAMESPACE=openab-local NAME=cloudflared-wecom \
  ../openab-control-plane/scripts/dev-tunnel-k8s.sh --delete
```

To remove the entire namespace:

```bash
kubectl delete namespace openab-local
```

## Upgrading to a real agent

Replace the echo agent by rebuilding with an agent target and redeploying with agent credentials:

```bash
scripts/dev-build-image.sh --target kiro --tag kiro-dev

kubectl -n openab-local create secret generic openab-kiro-api \
  --from-literal=KIRO_API_KEY='...' \
  --dry-run=client -o yaml | kubectl apply -f -
```

Update the profile ConfigMap `[agent]` block (or use an image where `OPENAB_AGENT_COMMAND` is preset, as with the `kiro` Docker target). See [kiro.md](kiro.md).