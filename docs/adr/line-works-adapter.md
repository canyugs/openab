# ADR: LINE WORKS Adapter

- **Status:** Proposed — deferred pending testable tenant
- **Initial Date:** 2026-06-18
- **Last Updated Date:** 2026-06-21
- **Author:** @canyugs

---

## 1. User Story & Requirements

As a LINE WORKS user (typically inside an enterprise tenant), I want to interact with an OpenAB agent inside LINE WORKS — both in 1:1 DMs and in talk-room groups — so my team can use the same AI coding assistant without leaving our corporate messaging tool or routing work through a personal Discord / Slack workspace.

Requirements:
- Receive user messages from LINE WORKS and route them to an agent session.
- Send agent responses back via the LINE WORKS Bot API.
- Validate webhook signatures so messages are authentically from LINE WORKS.
- Support tenant / user / channel allowlists for access control (LINE WORKS is multi-tenant; each domain is a separately licensed organization).
- Integrate into the existing multi-adapter architecture (run alongside Discord / Slack / LINE / WeCom).

### When to Use LINE WORKS

LINE WORKS is the right choice when:
- The deploying organization is a **corporate tenant** that already uses LINE WORKS as its primary messaging stack (common in JP / KR / TW enterprise).
- Compliance requires the conversation to stay inside the corporate IM rather than a consumer / public chat platform.
- The primary use case is **1:1 private conversations** with the agent (per-user session, similar to Discord DM).

### When to Use LINE (Consumer), Discord or Slack Instead

LINE WORKS is **not** the right choice when:
- The audience is consumers or an open community → consumer LINE / Discord / Slack is the right tool.
- Multiple users need **per-conversation isolation in groups** — LINE WORKS, like consumer LINE, has no thread primitive in talk rooms.
- The deploying team is a small / hobby group without a LINE WORKS tenant — provisioning a Developer Console app + service account just for OpenAB is heavy.

### Summary: Best Fit by Scenario

| Scenario | Recommended Platform | Why |
|---|---|---|
| JP / KR / TW enterprise team, 1:1 + small group | **LINE WORKS** | Already inside the corporate IM, OAuth-gated, audit-trail-friendly |
| Consumer / mobile-first individual | **LINE** (consumer) | No tenant overhead |
| Open developer community | **Discord / Slack** | Threads + reactions + public access |
| Large team, concurrent collaboration | **Discord / Slack** | On-demand @mention sessions scale better |

---

## 2. High-Level Design

### Architecture Overview

The LINE WORKS adapter lives in the **`openab-gateway` crate**, alongside the existing `line`, `feishu`, `googlechat`, `teams`, `telegram`, `wecom` adapters (`gateway/src/adapters/`). The OAB core binary (`openab`) is **not** modified — it only sees `GatewayEvent` / `GatewayReply` messages over its existing WebSocket link to the gateway.

```
┌──────────────────────┐
│ LINE WORKS Platform  │  (worksapis.com)
└──────┬───────────────┘
       │ HTTPS POST (callback)                              ▲
       ▼                                                    │ HTTPS (Bot Send Message API)
┌──────────────────┐                                        │
│ TLS Termination  │  (CDN / Ingress)                       │
└──────┬───────────┘                                        │
       │                                                    │
       ▼                                                    │
┌───────────────────────────────────────────────────────────┴──┐
│  openab-gateway Pod  (separate binary, separate image)       │
│                                                              │
│  ┌──────────────────────────────────────────────────────┐    │
│  │  axum HTTP server                                    │    │
│  │  /webhook/line          → adapters::line             │    │
│  │  /webhook/lineworks     → adapters::lineworks   ◀── NEW   │
│  │  /webhook/feishu        → adapters::feishu           │    │
│  │  /webhook/{teams,telegram,wecom,googlechat} ...      │    │
│  └─────────────────────┬────────────────────────────────┘    │
│                        │ build GatewayEvent (schema v1)      │
│                        ▼                                     │
│  ┌──────────────────────────────────────────────────────┐    │
│  │  WebSocket fan-out to connected OAB cores            │    │
│  └─────────────────────┬────────────────────────────────┘    │
└────────────────────────┼─────────────────────────────────────┘
                         │ WS (GatewayEvent / GatewayReply)
                         ▼
                ┌────────────────────┐
                │  openab Pod        │  (unchanged for LINE WORKS)
                │  AdapterRouter →   │
                │  ACP Session Pool  │
                └────────────────────┘
```

Concretely, this ADR adds:
- `gateway/src/adapters/lineworks.rs` — webhook parsing, signature check, OAuth client, Bot Send Message dispatcher.
- A new route in the gateway's axum router.
- A new `[lineworks]` section in the gateway config schema.

OAB core requires **no** code changes — the existing `platform = "lineworks"` branch in `GatewayEvent` / `GatewayReply` handling is sufficient as long as the gateway uses the agreed schema.

### Message Flow

All gateway-side; OAB core only sees steps 6 and 9.

```
1. LINE WORKS user sends a message in a 1:1 or talk-room chat.
2. LINE WORKS platform POSTs to the gateway's callback URL with JSON
   payload and HMAC-SHA256 signature in `X-WORKS-Signature`.
3. Gateway adapter validates the signature against the raw request body
   using the bot secret.
4. Adapter extracts:
     - source.userId           (sender)
     - source.channelId        (talk-room / 1:1 channel)
     - source.domainId         (tenant)
     - content.text / content.type
5. Adapter builds a `GatewayEvent` (schema `openab.gateway.event.v1`):
     - platform   = "lineworks"
     - event_id   = UUID
     - channel.id = "{domainId}:{userId|channelId}"  (see Section 4)
     - sender, content, mentions, message_id
   and fans it out over WebSocket to every connected OAB core.
6. OAB core routes: AdapterRouter → ACP Session Pool → agent CLI.
7. Agent emits reply text; OAB core sends `GatewayReply`
   (schema `openab.gateway.reply.v1`) with reply_to = event_id,
   platform = "lineworks", channel.id = same tenant-namespaced id.
8. Gateway adapter calls the LINE WORKS Bot Send Message API:
     POST /v1.0/bots/{botId}/channels/{channelId}/messages    (talk room)
     POST /v1.0/bots/{botId}/users/{userId}/messages          (1:1 DM)
   authenticated with a cached OAuth 2.0 access token.
9. Outbound delivery result is observed only by the gateway; OAB core
   does not receive a per-message ack (matches LINE / WeCom behaviour).
```

### Authentication: OAuth 2.0 Service Account (JWT)

LINE WORKS does **not** use a long-lived channel access token like consumer LINE. Outbound API calls require an OAuth 2.0 access token obtained via the JWT bearer flow. Confirmed against [developers.worksmobile.com/en/docs/auth-jwt](https://developers.worksmobile.com/en/docs/auth-jwt):

```
1. Build JWT (signed RS256 with Service Account private key from Developer Console):
     iss   = Client ID (issued by Developer Console)
     sub   = Service Account name in email-address format
     iat   = now (Unix seconds)
     exp   = iat + N seconds, N <= 3600   (spec maximum: 60 min)
     scope = comma-separated (e.g. "bot,bot.message")

2. POST https://auth.worksmobile.com/oauth2/v2.0/token
     grant_type     = urn:ietf:params:oauth:grant-type:jwt-bearer
     assertion      = <signed JWT>
     client_id      = <Client ID>
     client_secret  = <Client Secret>
     scope          = bot,bot.message
   → { access_token, expires_in (1h or 24h depending on app config),
       refresh_token, token_type: "Bearer", scope }

3. Cache access_token in the gateway process; refresh proactively
   (~5 min before expiry) using the refresh_token.
4. Send as `Authorization: Bearer <access_token>` on every Bot API call to
   https://www.worksapis.com/v1.0/...
```

Operational implication: the **gateway pod** (not OAB core) must hold the Service Account **private key**, not just a static token. This raises secrets-handling expectations above the LINE / Slack / Discord / Telegram / Feishu / Teams / WeCom / Google Chat adapters — LINE WORKS is the first gateway adapter that needs asymmetric key material.

Note: the Service Account private key (RS256, for JWT signing) and the **Bot Secret** (symmetric, for webhook signature verification — see below) are **two separate values** issued from the Developer Console. Both live in the gateway pod.

### Webhook Signature Verification

Per [developers.worksmobile.com/jp/docs/bot-callback](https://developers.worksmobile.com/jp/docs/bot-callback):

- Header: `X-WORKS-Signature` (HTTP header name comparison is case-insensitive per the spec; treat accordingly).
- Algorithm: **HMAC-SHA256**.
- Key: the **Bot Secret** (not the Service Account private key) — symmetric, supplied from the Developer Console.
- Encoding: **Base64**.
- Signed payload: the **raw request body bytes** sent by the LINE WORKS platform.

Implementation must compute HMAC-SHA256 over the raw body bytes, base64-encode the result, and compare in constant time against the header value.

### Reply Strategy: Push-Only

LINE WORKS does not have the reply-token / free-Reply-API mechanism that consumer LINE has. **Every outbound message is a push to the channel (talk room) or user**, addressed by `channelId` or `userId`. There is no hybrid optimization path; quota and rate limits apply uniformly.

This **simplifies** the gateway dispatch path compared to the LINE adapter — `ChannelRef.origin_event_id` and reply-token caching are not needed.

### Rate Limits

Per [developers.worksmobile.com/en/docs/rate-limits](https://developers.worksmobile.com/en/docs/rate-limits) (Standard / Advanced plans):

- **240 requests/min per API operation** as the general per-endpoint limit.
- **Exception:** "Create a message room with a bot" — 10 requests/min.
- **Concurrent requests across all APIs: 5** simultaneous.
- Tenants with more than 500 members in a single message room: send succeeds but push notifications do not fire.

Operational planning implications:
- 240 rpm ≈ 4 messages/sec per endpoint. Comfortable for normal agent traffic; tight if the agent emits streaming-style rapid edits. The gateway must rate-limit outbound dispatch — uncontrolled emission can 429-trip the entire bot for all users.
- The 5-concurrent cap is the more aggressive constraint. Outbound calls from the gateway must be serialized or pooled below this threshold per tenant.

---

## 3. Position in the Existing Architecture

### Where LINE WORKS Lives

LINE WORKS is a **gateway-crate adapter**, the same architectural slot as `line`, `feishu`, `googlechat`, `teams`, `telegram`, `wecom`. OAB core stays outbound-only — it never sees the webhook, the OAuth flow, or the Bot Send Message HTTP call. From OAB core's perspective, LINE WORKS is just another value of the `platform` field on `GatewayEvent` / `GatewayReply`.

This is the architecture the LINE ADR (`line-adapter.md`, Section 3) sketched as "v2 — independent webhook bridge service" and the gateway crate now realises. No further architectural move is required for LINE WORKS; it slots into the existing pattern.

### Comparison Matrix

| Dimension | Discord / Slack | LINE (consumer) | **LINE WORKS** |
|---|---|---|---|
| Code location | OAB core (`src/`) | `gateway/src/adapters/line.rs` | **`gateway/src/adapters/lineworks.rs`** |
| Transport | Outbound WS to platform | Inbound webhook | Inbound webhook |
| Public ingress required | No | Gateway only | Gateway only |
| Auth model | Bot token (long-lived) | Channel access token (long-lived) | **OAuth 2.0 JWT bearer, short-lived access_token, refresh required** |
| Secret material | Static token in OAB | Static token in gateway | **Service Account private key (RS256) in gateway** |
| Free reply path | N/A | Reply token (60s) hybrid | **None — push only** |
| Tenant model | Single | Single | **Multi-tenant by `domainId`** |
| Thread primitive | Yes | No | No |
| Signature header | N/A | `X-Line-Signature` | `X-WORKS-Signature` |
| OAB core code change | N/A | None for new gateway adapter | **None expected** |

### Why OAB Core Needs No Change

OAB core already dispatches `GatewayReply` by `platform` string (see `src/gateway.rs` — the `match` arm uses string keys like `"line"`, `"wecom"`). Adding `"lineworks"` as a new value the gateway emits and OAB echoes back in `GatewayReply.platform` requires only:

- the gateway adapter to construct events with `platform = "lineworks"`;
- OAB's existing platform-agnostic AdapterRouter path to handle them (no special-case needed unless LINE WORKS reveals a contract gap, in which case that gap belongs in the gateway schema, not in OAB).

### Concrete Delta vs `gateway/src/adapters/line.rs` (Consolidated)

Implementation should fork `adapters/line.rs`, then apply the deltas in this table row-by-row. The Action column says exactly what the implementer does.

| # | Concern | LINE (`adapters/line.rs`) | LINE WORKS | Action |
|---|---|---|---|---|
| 1 | Axum route | `POST /webhook/line` | `POST /webhook/lineworks` | **Reuse pattern**, change path |
| 2 | Signature header | `X-Line-Signature` | `X-WORKS-Signature` | **Modify**: header name only |
| 3 | Signature algorithm | HMAC-SHA256 + Base64 over raw body | HMAC-SHA256 + Base64 over raw body | **Identical** — see "Shared Infrastructure" below |
| 4 | Signature key field | `line_channel_secret` | `lineworks_bot_secret` | **Modify**: config field name |
| 5 | Event JSON envelope | `LineWebhookBody { events: Vec<LineEvent> }` | LINE WORKS envelope with similar `events[]` shape; per-event fields differ slightly | **Reuse pattern**, rename + adjust fields |
| 6 | `GatewayEvent` construction | UUID `event_id`, fan-out to WS clients | Same | **Reuse as-is** |
| 7 | Media / attachment pipeline | `crate::media`, `crate::store` | Same | **Reuse as-is** (already cross-adapter) |
| 8 | Outbound auth header | `Authorization: Bearer <static line_access_token>` | `Authorization: Bearer <OAuth access_token>` | **Add** OAuth client (see row 13) |
| 9 | Send endpoint | `https://api.line.me/v2/bot/message/{push,reply}` | `https://www.worksapis.com/v1.0/bots/{botId}/{users\|channels}/{id}/messages` | **Modify** URL + body |
| 10 | Reply-token cache | `Arc<Mutex<HashMap<event_id, (replyToken, ts)>>>` + 50s TTL sweep | n/a — no reply token in LINE WORKS | **Drop** entire mechanism |
| 11 | Reply vs Push dispatch | hybrid (try Reply API if token fresh, else Push) | push-only | **Drop** branch; single push path |
| 12 | `ChannelRef.origin_event_id` | propagated for reply-token lookup | propagated only as routing/dedup id | **Reuse**, drop the lookup use |
| 13 | OAuth 2.0 client | none | required: JWT sign (RS256) → token exchange → in-process cache → refresh ~5 min before expiry | **Add** new module (uses existing `jsonwebtoken` crate) |
| 14 | Outbound concurrency cap | `LINE_WEBHOOK_CONCURRENCY_MAX = 8` on *inbound* webhook processing | `Semaphore(≤ 5)` on *outbound* API calls, per tenant (hard LINE WORKS cap) | **Add** — different placement than LINE's cap, do not conflate |
| 15 | Outbound rate limit | none (hybrid masks bursts) | token-bucket ~4 req/sec (240 rpm) per endpoint per tenant | **Add** |
| 16 | Channel ID emitted to OAB | `userId` / `groupId` / `roomId` directly | `"{domainId}:{userId\|channelId}"` (tenant prefix mandatory) | **Modify** — see Section 4 |
| 17 | Config block | `[line]` with channel secret + access token | `[lineworks]` (or `[[lineworks.tenants]]`) with `bot_id`, `client_id`, `client_secret`, `service_account`, `private_key_pem`, `bot_secret`, `domain_id` | **Add** |
| 18 | Inbound 1-per-tenant webhook routing | n/a (single tenant) | optional path suffix `/webhook/lineworks/{tenant_slug}` if multi-tenant (Section 5 Q5) | **Add** if multi-tenant shape adopted |

### Impact on the Existing LINE Adapter

Verified 2026-06-20 by reading `gateway/src/main.rs` and `src/gateway.rs`. **Adding LINE WORKS does not modify any LINE code path.** The integration is purely additive:

| Touchpoint | What LINE WORKS adds | Effect on LINE |
|---|---|---|
| `AppState` struct (`gateway/src/main.rs:43-77`) | New `lineworks_*` fields | None — LINE's 5 fields (`line_channel_secret`, `line_access_token`, `reply_token_cache`, `line_webhook_semaphore`, `client`) untouched |
| Reply dispatch `match reply.platform.as_str()` (`gateway/src/main.rs:134+`) | New `"lineworks" =>` arm | None — existing `"line" =>` arm untouched |
| Route registration (`gateway/src/main.rs:265`) | New `app.route("/webhook/lineworks", post(...))` | None — `/webhook/line` route untouched |
| Env-var loading | New `LINEWORKS_*` vars | None — existing `LINE_*` vars untouched |
| OAB core (`src/gateway.rs`) | None | None — OAB's only `event.platform.as_str()` use (line 976, thread cancellation routing) is platform-agnostic |

LINE's existing tests, telemetry, and customer behaviour are unaffected. The only LINE-side compile-time effect is that `AppState` becomes a larger struct (more `Option<String>` fields); access is by name so no breakage.

### Extension Options (How to Share Code Between LINE and LINE WORKS)

| Option | Approach | Recommended? |
|---|---|---|
| **A — Sibling adapter (copy)** | Create `gateway/src/adapters/lineworks.rs` next to `line.rs`. Inline the ~4-line HMAC verify. No refactor of `line.rs`. | ✅ **v1 default.** Smallest review surface, easiest rollback, zero risk of regressing the existing LINE adapter. |
| **B — Extract HMAC helper as follow-up** | After Option A ships, open a separate refactor PR that adds `gateway/src/sig.rs::verify_hmac_sha256_base64(body, key, signature)` and updates both `line.rs` and `lineworks.rs` to use it. | ✅ **Follow-up only.** Do not bundle with the LINE WORKS introduction PR — mixing new-platform and shared-helper refactor expands the diff and the regression surface. |
| **C — `LineFamilyAdapter` trait** | Define a trait abstracting "HMAC-over-body + push-style send + events[] envelope" and refactor both adapters into implementations. | ❌ **Not recommended.** Only two platforms qualify; abstracting on n=2 picks the wrong boundaries. Revisit only if a third HMAC-over-body adapter appears. |
| **D — Extend `line.rs` with a mode flag** | Single `line.rs` module that branches on `cfg.is_works` for auth, signature header, reply-token presence, send endpoint. | ❌ **Reject.** Auth model (static token vs JWT exchange) and reply-token semantics (present vs absent) differ at the core of every code path. Every function would grow `if works { ... } else { ... }`; test matrix doubles; LINE bug fixes risk breaking LINE WORKS and vice versa. |

The recommended path is **A now, B later**.

### Shared Infrastructure

What's **already shared** across every gateway adapter today (use as-is):

- `gateway/src/schema.rs` — `GatewayEvent`, `GatewayReply`, `ChannelInfo`, `SenderInfo`, `Content`, `Attachment`. LINE WORKS adds no new schema fields.
- `gateway/src/media.rs` — `resize_and_compress`, `audio_extension`, `is_text_extension`, `IMAGE_MAX_DOWNLOAD`. Attachment handling reuses this verbatim.
- `gateway/src/store.rs` — `media_dir`, `store_media`, eviction loop for inbound media.
- `gateway/src/main.rs` — axum router setup, WebSocket fan-out to OAB cores, gateway config loading.
- `gateway/Cargo.toml` dependencies that LINE WORKS needs are **all already present**: `jsonwebtoken` (JWT), `hmac` + `sha2` + `base64` (webhook signature), `subtle` (constant-time compare), `reqwest` (outbound HTTP). **No new dependency.**

What is **identical between LINE and LINE WORKS but not yet a shared helper** (refactor opportunity, not v1 scope):

- HMAC-SHA256-over-raw-body + Base64-encode signature verifier. Currently inlined in `adapters/line.rs:96-99`. Could be extracted to a tiny helper used by both adapters in a follow-up PR. **Recommend: ship LINE WORKS copying the inline pattern; extract in a separate refactor PR. Mixing refactor and new-platform in one PR makes review noisy and broadens regression risk.**

What is **not shareable** (genuinely platform-specific):

- WeCom (`adapters/wecom.rs`) uses SHA1 over `token+timestamp+nonce+encrypt` — completely different signature model from LINE/LINE WORKS, no overlap.
- Feishu (`adapters/feishu.rs`) uses SHA256 hex over `timestamp+nonce+encrypt_key+body` — different model again, hex not base64, no overlap.
- Per-platform event JSON envelopes (`LineEvent`, `FeishuEvent`, etc.) are inherently different shapes.
- Per-platform send-message body schemas differ — only the `Bearer` token mechanic and the `reqwest::Client` are reusable boilerplate.

Summary: LINE WORKS reuses most of the gateway's cross-adapter substrate (schema, media, store, main, deps) and shares signature *algorithm* with LINE (but currently copies the inline impl). The truly new code is the OAuth client + per-tenant concurrency/rate limiter + the tenant-aware config — none of which fit cleanly into a shared module today.

---

## 4. ACP Session Model: Impact & Mitigations

### Session Key Convention

| Source type | Session key | Notes |
|---|---|---|
| 1:1 DM | `lineworks:{domainId}:{userId}` | Tenant-namespaced; one user in two tenants is two sessions |
| Talk room (group) | `lineworks:{domainId}:{channelId}` | Shared across all members of the talk room |

**Why `{domainId}` is part of the key:** unlike consumer LINE, a LINE WORKS bot can be installed into multiple tenants, and `userId` / `channelId` are only unique within a tenant. Cross-tenant key collision would leak conversation state between organizations — unacceptable.

### Group Behaviour

Identical model to consumer LINE: talk rooms have no thread primitive, so the session is **shared across all members of the room**. The same "shared-room assistant" semantics from the LINE ADR apply verbatim — context pollution from multiple speakers, shared visibility of replies, no per-user isolation. See `line-adapter.md` Section 4 for the full analysis; it is not duplicated here.

### Always-On vs On-Demand

LINE WORKS triggers on **every message in any channel the bot is a member of** — the same always-on profile as consumer LINE. Memory pressure math from `line-adapter.md` Section 4 ("Impact 2: 1:1 DM Memory Pressure" and "Impact 3: Always-On vs On-Demand") applies unchanged.

### Mitigation

Reuse the mitigation menu from `line-adapter.md` Section 4. Specific recommendation:
- **@mention gating is more important here than for consumer LINE.** Enterprise talk rooms tend to be larger and noisier than consumer LINE groups (whole-team channels, project rooms). Without @mention gating, every standup message turns into agent work.
- LINE WORKS message events include `mention` data, so the gating implementation is straightforward.

### Recommended Approach for v1

- **1:1 DM**: per-user, tenant-namespaced session.
- **Talk room**: per-channel shared session; **ship @mention gating on by default** for talk rooms (different from consumer LINE which ships always-on by default).
- **Capacity planning**: same memory math as consumer LINE; document the multi-tenant multiplier (sessions ≈ active users × tenants).

---

## 5. Open Questions

Updated 2026-06-19 against the official LINE WORKS developer documentation. Questions 1–3 and 6 are resolved; Questions 4–5 require team / operator decisions and remain open.

1. ~~**API surface.**~~ **Resolved 2026-06-19.** Base URL: `https://www.worksapis.com/v1.0/...` ([api-call](https://developers.worksmobile.com/en/docs/api-call)). Token endpoint: `https://auth.worksmobile.com/oauth2/v2.0/token` ([auth-jwt](https://developers.worksmobile.com/en/docs/auth-jwt)). Bot endpoints: `POST /bots/{botId}/users/{userId}/messages` and `POST /bots/{botId}/channels/{channelId}/messages` ([bot-api](https://developers.worksmobile.com/en/docs/bot-api/)).
2. ~~**Webhook signature.**~~ **Resolved 2026-06-19.** `X-WORKS-Signature`, HMAC-SHA256, Bot Secret as key, Base64-encoded, over raw request body ([bot-callback](https://developers.worksmobile.com/jp/docs/bot-callback)). See Section 2.
3. ~~**Rate limits.**~~ **Resolved 2026-06-19.** 240 requests/min per endpoint, 10/min for "create message room", 5 concurrent across all APIs ([rate-limits](https://developers.worksmobile.com/en/docs/rate-limits)). See Section 2 — the gateway adapter must implement outbound rate-limiting / concurrency control.
4. **Open: secrets storage for the Service Account private key (gateway-side).** Current gateway adapter secrets are static tokens / channel secrets in env vars or the gateway config file. The RS256 private key is materially different (larger, RSA-2048 PEM-encoded multi-line blob, rotated by re-issuing from Developer Console which invalidates the previous key). Open question for the secrets-management owner: does `docs/secrets-management.md` need a new section, or can the existing env-var / file-mount pattern accept multi-line PEM cleanly?
5. **Open: multi-tenant deployment story.** A single LINE WORKS bot belongs to one tenant (one set of `domainId` + Client ID + Service Account + Bot Secret + private key). Two valid deployment shapes:
   - **One gateway per tenant** — simplest, each gateway holds one set of credentials. Operationally heavier (N gateway deployments for N customers).
   - **One gateway, many tenants** — the gateway config grows `[[lineworks.tenants]]` array, each entry with its own credential set. Webhook routing distinguishes tenants by URL path (`/webhook/lineworks/{tenant_slug}`) or by Bot ID in payload. The gateway must namespace OAuth tokens, signature secrets, and session keys by tenant.

   This ADR proposes shape #2 (config supports multiple `[[lineworks.tenants]]` entries; default is one) because (a) channel-id design already namespaces by `domainId`, (b) Bot Secret + private key per tenant is only an extra config block, not extra code paths, (c) Zeabur's customer model wants one operator to potentially serve multiple end-customer tenants. Confirm with platform team before locking the config schema.
6. ~~**`GatewayEvent.channel.id` shape.**~~ **Resolved 2026-06-19.** A scan of `src/gateway.rs` (the sole `GatewayEvent` consumer in OAB core) confirms `event.channel.id` is used only as an opaque string: HashSet allowlist membership, `ChannelRef.channel_id` / `SenderContext.channel_id` pass-through, and `format!("{}:{}", platform, thread_id_or_channel_id)` keys. No code path splits on `:` or assumes a single-segment id. Therefore `"{domainId}:{userId|channelId}"` is safe and no `gateway/src/schema.rs` change is required for tenant identity.

---

## 6. Consequences

### Positive

- Unblocks JP / KR / TW enterprise teams that cannot use consumer LINE or Discord / Slack for compliance reasons.
- Slots into the existing gateway-crate adapter pattern alongside line / feishu / googlechat / teams / telegram / wecom — no new architectural mode, no OAB core changes expected.
- Push-only reply path is simpler than consumer LINE's hybrid Reply / Push dispatch — no `event_id` → `replyToken` cache, no 50s TTL sweep, no Reply API fallback logic in the gateway.
- Tenant-namespaced channel ids give a clean isolation boundary for multi-org deployments.

### Negative

- New secrets-handling burden **in the gateway pod**: must hold an RS256 private key and perform JWT signing + OAuth token refresh. This is the first gateway adapter requiring asymmetric key material.
- Push-only means no free reply path — message quota cost is uniformly higher than consumer LINE's hybrid model.
- Same group-chat context-pollution and always-on memory-pressure problems as consumer LINE, applied to typically-larger enterprise talk rooms. @mention gating becomes load-bearing rather than nice-to-have.
- Adds an eighth platform adapter in the gateway crate; widens the gateway's webhook attack surface and increases the volume of upstream-API contract drift the team must track.

---

## 7. Compliance

To ensure this ADR is followed in implementation and future changes:

1. **Webhook correctness**: signature verification must operate on the raw request body bytes obtained via the gateway's existing axum server. No hand-rolled TCP / lossy UTF-8 paths (same rule as `line-adapter.md` Compliance §1).
2. **Code location**: the adapter lives in `gateway/src/adapters/lineworks.rs`. No LINE WORKS code may be added to the OAB core crate (`src/`). If the implementation finds it needs OAB-side support, the gap belongs in the gateway IPC schema (`gateway/src/schema.rs` / `src/gateway.rs`), not in an OAB-side LINE WORKS module.
3. **Channel id convention**: `GatewayEvent.channel.id` must be `"{domainId}:{userId}"` (1:1) or `"{domainId}:{channelId}"` (talk room). The `{domainId}` prefix is mandatory; deviations require a new ADR because they affect cross-tenant isolation.
4. **Auth path**: outbound calls must use the OAuth 2.0 JWT-bearer flow with in-process access-token caching and proactive refresh. Long-lived static tokens (if LINE WORKS ever exposes one) must not be used as a shortcut — service-account-based auth is the audit trail enterprises rely on.
5. **Secrets storage**: the Service Account private key must be handled per the (forthcoming) entry in `secrets-management.md`. PRs introducing inline private keys in committed config files must be rejected.
6. **Talk room defaults**: `@mention gating` must be **enabled by default** for talk rooms in v1, distinct from consumer LINE which ships always-on. The default may be revisited after real usage data.
7. **Documentation**: any LINE WORKS adapter PR must include or update operator-facing documentation covering:
   - The Bot + Service Account setup flow in the LINE WORKS Developer Console.
   - Multi-tenant deployment guidance (one gateway ↔ N domains).
   - Capacity planning math (sessions ≈ active users × tenants).

---

## 8. Notes

- **Version:** 0.6 (Proposed — deferred)
- **Changelog:**
  - 0.6 (2026-06-21): Status updated to "Proposed — deferred pending testable tenant". The design is complete enough to implement, but LINE WORKS has no sandbox environment and the Free plan signup path is unverified for non-Japanese individual developers. Implementation should wait until either (a) a real customer tenant becomes available, or (b) a Free-plan signup is confirmed self-serviceable and the Bot API is reachable on Free. Until then, no `gateway/src/adapters/lineworks.rs` work should start — the smoke-test playbook in conversation history must be runnable first to validate the ADR's API-surface assumptions against the live platform.
  - 0.5 (2026-06-20): Added Section 3 subsections "Impact on the Existing LINE Adapter" (verified additive-only; LINE code path untouched) and "Extension Options" (A: sibling adapter for v1; B: extract HMAC helper as follow-up; C/D rejected). Verified against `gateway/src/main.rs` and `src/gateway.rs`.
  - 0.4 (2026-06-20): Section 3 Concrete Delta consolidated into a single 18-row table (Concern × LINE × LINE WORKS × Action), plus an explicit "Shared Infrastructure" subsection categorising (already shared / extractable later / genuinely platform-specific). Confirmed `gateway/Cargo.toml` already carries every dependency LINE WORKS needs.
  - 0.3 (2026-06-20): Added Section 3 "Concrete Delta vs `gateway/src/adapters/line.rs`" — reuse / modify / drop / add breakdown for implementer guidance.
  - 0.2 (2026-06-19): Architecture corrected to live in the `openab-gateway` crate alongside other webhook adapters; OAB core code path no longer touched. Q1–Q3 + Q6 of Section 5 resolved against official LINE WORKS documentation. Webhook signature, JWT auth, base URL, and rate limits now have verified specifics.
  - 0.1 (2026-06-18): Initial proposed version. Architecture diagram placed the handler in OAB core (incorrect).

---

## References

### OpenAB internal

- `docs/adr/line-adapter.md` — Consumer LINE adapter; this ADR explicitly delegates the shared-session / memory-pressure analysis there and only documents the LINE WORKS deltas.
- `docs/adr/multi-platform-adapters.md` — `ChatAdapter` trait and `AdapterRouter` contract.
- `gateway/src/schema.rs` — `GatewayEvent` / `GatewayReply` schemas (`openab.gateway.event.v1`, `openab.gateway.reply.v1`).
- `src/gateway.rs` — OAB-side consumer of `GatewayEvent.channel.id` (referenced in Section 5 Q6).

### LINE WORKS official documentation (verified 2026-06-19)

- [API Call — base URL](https://developers.worksmobile.com/en/docs/api-call): `https://www.worksapis.com/v1.0/...`
- [Authentication with a Service Account (JWT)](https://developers.worksmobile.com/en/docs/auth-jwt): RS256, claims, token endpoint, response format.
- [Bot — overview](https://developers.worksmobile.com/en/docs/bot): callback delivery model, channel/room semantics.
- [Bot API reference](https://developers.worksmobile.com/en/docs/bot-api/): send-to-user vs send-to-channel endpoints.
- [Bot Callback signature verification (JP)](https://developers.worksmobile.com/jp/docs/bot-callback): `X-WORKS-Signature` + HMAC-SHA256 + Base64 over raw body.
- [API rate limits](https://developers.worksmobile.com/en/docs/rate-limits): 240 rpm per endpoint, 5 concurrent.
