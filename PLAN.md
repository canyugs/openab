# LINE WORKS Adapter Plan

## Status (2026-07-25)

**v1 implemented and verified end-to-end** against the real LINE WORKS API,
with the real OAB stack (unified binary + claude-agent-acp) running in local
Kubernetes (docker-desktop) behind a Cloudflare tunnel.

Verified with production credentials:

- Token exchange (service-account JWT → access token), cache + refresh.
- Webhook signature accept/reject through the public callback URL.
- Real inbound messages (1:1 `user:` prefix and group channel) → agent
  session → reply delivered by the Claude agent.
- Cron scheduled messages to a channel (fires on schedule, agent replies).
- 1:1 sends accept **loginId (email, e.g. `can@zeabur`) in place of the UUID
  userId** in `users/{userId}/messages` — cron 1:1 targets can be written as
  `channel = "user:<loginId>"` with no directory lookup or extra scope.

### Platform-list gotchas (checklist for any new gateway platform)

Adding a platform requires touching several hardcoded platform lists beyond
the adapter itself. All were hit (and fixed for lineworks) during bring-up:

1. `crates/openab-core/src/gateway.rs` — `NON_EDITABLE_PLATFORMS`
   (platforms without message edit must be listed or streaming reposts).
2. Root `Cargo.toml` — per-platform feature + membership in `unified`
   (a `#[cfg(feature)]` for an undeclared feature silently compiles out).
3. `src/main.rs` — `has_unified_platform()` (embedded webhook server gate).
4. `src/main.rs` — gateway trust registry platform list (missing platform
   = deny-all identity).
5. `src/main.rs` — cron `configured_platforms` + `cron_adapters` map, and
   `crates/openab-core/src/cron.rs` — `VALID_PLATFORMS`.

### Remaining roadmap (v2)

Priority order agreed: inbound/outbound attachments (fileId content API) →
flex-template rendering for markdown (precedent: `feishu_card.rs`) →
group @mention gating (text-match on bot name; no `isSelf` equivalent).
Ceiling stays: no streaming/edit, no reactions, no threads.

## Goal

Add a LINE WORKS bot adapter to the gateway crate (`crates/openab-gateway`),
following the existing webhook-adapter pattern (LINE, Feishu, Google Chat).

## Why gateway mode (not websocket)

LINE WORKS bots only support webhook/callback delivery: the platform POSTs
events to a registered HTTPS callback URL (CA-signed cert required). There is
no websocket or long-polling alternative, so the core-crate style (Slack
Socket Mode / Discord gateway WS) does not apply. Outbound messages go
through a REST API authenticated with an OAuth 2.0 service-account JWT flow.

## Architecture recap (existing)

- `openab-gateway::serve()` (lib.rs) builds an axum Router; each platform
  registers a `/webhook/<platform>` POST route gated on its env vars.
- Incoming webhooks are normalized into `schema::GatewayEvent` and broadcast
  over the `/ws` endpoint; the core bot connects via
  `openab-core/src/gateway.rs::run_gateway_adapter`.
- Replies come back as `schema::GatewayReply`; lib.rs matches
  `reply.platform` and calls the platform's `dispatch_*_reply`.
- `googlechat.rs::GoogleChatTokenCache` already implements the exact
  JWT-bearer grant (`urn:ietf:params:oauth:grant-type:jwt-bearer`, RS256
  private key, cached access token) that LINE WORKS uses — reuse this shape.

## LINE WORKS platform facts

- Callback: HTTPS POST, JSON body; headers `X-WORKS-BotId`,
  `X-WORKS-Signature` (Base64(HMAC-SHA256(body, Bot Secret))). Must verify
  before processing.
- Events: `message` (text/image/file/location/sticker), `join`, `leave`,
  `joined`, `left`, `postback`. Sender is `source.userId`; room is
  `source.channelId` (group talk) or absent for 1:1 (use userId).
- Send API (base `https://www.worksapis.com/v1.0`, success = HTTP 201):
  - 1:1 — `POST /bots/{botId}/users/{userId}/messages`
  - Channel — `POST /bots/{botId}/channels/{channelId}/messages`
  - No reply-token mechanism (unlike LINE): push-style only, so no reply
    token cache needed. No message edit/delete.
- Auth: RS256 JWT (iss=Client ID, sub=service account email, exp ≤ 60 min)
  signed with the Console-downloaded private key, exchanged at
  `https://auth.worksmobile.com/oauth2/v2.0/token` with
  `grant_type=urn:ietf:params:oauth:grant-type:jwt-bearer`, scope `bot`.
  Access token lifetime 1h or 24h (Console setting) → cache + refresh.
- Text message max length: 10,000 chars (plain text only; no markdown).

## Feature parity vs Slack / Discord

Capability comparison against the core-crate adapters, based on the
`ChatAdapter` surface actually used by openab. LINE WORKS column is the
platform API ceiling, not an implementation choice.

| Capability | Slack | Discord | LINE WORKS |
| --- | --- | --- | --- |
| Text messages in/out | ✅ | ✅ | ✅ |
| Connection | Socket Mode WS | Gateway WS | Webhook only |
| Message length limit | ~40,000 | 2,000 | 10,000 |
| Threads | ✅ native | ✅ native | ❌ no concept |
| Reactions | ✅ | ✅ | ❌ no API |
| Edit sent message | ✅ | ✅ | ❌ (no edit/delete) |
| Streaming output | ✅ (via edit) | ✅ (via edit) | ❌ (depends on edit) |
| Images / files | ✅ | ✅ | ✅ (dedicated up/download API) |
| Markdown | mrkdwn | full | ❌ plain text (button/list/carousel/flex templates as partial substitute) |
| Typing / status indicator | ✅ assistant status | ✅ typing | ❌ |
| Voice | ❌ | ✅ (in progress) | ❌ |

Net effect: the LINE WORKS adapter lands at the Telegram/LINE gateway-adapter
tier, not the Slack/Discord tier. Consequences and mitigations:

- **No streaming** — replies arrive as one final message. Mitigation: push a
  short "processing…" message first, then the final reply (two messages,
  since edit is impossible). Consider making this configurable.
- **No reactions** — the ack/done reaction flow (`reactions.rs`) degrades to
  text or is skipped; `dispatch` logs and ignores reaction commands.
- **No threads** — conversations are flat; acceptable for 1:1, noisy for
  group channels. `create_topic` command is a no-op.
- **Plain text** — code blocks/tables/bold render literally. v1 ships plain
  text; a later iteration can map rich output to flex templates (precedent:
  `feishu_card.rs`).

## Configuration (env vars)

| Var | Meaning |
| --- | --- |
| `LINEWORKS_BOT_ID` | Bot ID (also cross-checked vs `X-WORKS-BotId`) |
| `LINEWORKS_BOT_SECRET` | Bot Secret for webhook signature verification |
| `LINEWORKS_CLIENT_ID` | App Client ID (JWT `iss`) |
| `LINEWORKS_CLIENT_SECRET` | App Client Secret (token request) |
| `LINEWORKS_SERVICE_ACCOUNT` | Service account email (JWT `sub`) |
| `LINEWORKS_PRIVATE_KEY` / `LINEWORKS_PRIVATE_KEY_FILE` | RS256 private key (inline PEM or path) |
| `LINEWORKS_WEBHOOK_PATH` | Optional, default `/webhook/lineworks` |

Adapter enabled when bot id + secret + auth material are all present.

## Implementation steps

1. **Token provider** (`adapters/lineworks.rs`):
   `LineWorksTokenCache` modeled on `GoogleChatTokenCache` — build RS256 JWT
   (jsonwebtoken crate, already a dependency via googlechat), POST to the
   LINE WORKS token endpoint, cache `(token, expiry)` behind a lock, refresh
   with a safety margin (60 s before expiry).
   - Tests: JWT claims content, expiry/refresh logic (mock clock), token
     endpoint request shape against a mock server.

2. **Webhook handler** (`adapters/lineworks.rs::webhook`):
   - Verify `X-WORKS-Signature` (HMAC-SHA256 with Bot Secret, constant-time
     compare) over the raw body; reject 401 on mismatch. Check
     `X-WORKS-BotId` matches configured bot.
   - Parse event JSON; map `message` events to `GatewayEvent`
     (platform `"lineworks"`, channel = channelId or `user:{userId}`,
     content text; image/file via media download endpoint → `Attachment`,
     same pattern as `download_line_image`).
   - Ack 200 immediately after signature check; process post-ack under a
     small semaphore (same pattern as LINE webhook).
   - Tests: signature accept/reject, event → GatewayEvent mapping fixtures
     for text / image / join / postback.

3. **Reply dispatch** (`adapters/lineworks.rs::dispatch_lineworks_reply`):
   - Choose users/channels endpoint from the reply's channel ref.
   - Text messages only in v1; split at 10,000 chars; ignore unsupported
     commands (`add_reaction`, `edit`, `delete`, `create_topic`) with a log,
     like `dispatch_line_reply` does.
   - On 401, force token refresh once and retry.
   - Tests: endpoint selection, payload shape, unsupported-command no-op,
     401-retry path against a mock server.

4. **Wiring**:
   - `adapters/mod.rs`: `pub mod lineworks;`
   - `lib.rs::AppState`: add `lineworks: Option<LineWorksAdapter>` (single
     struct holding config + token cache, Google Chat style).
   - `lib.rs::serve()`: env-gated route registration.
   - `lib.rs` reply switch: `"lineworks" => dispatch_lineworks_reply(...)`.
   - `AppState::for_testing()` stays adapter-agnostic (defaults to None).

5. **Docs & config**: document env vars in `config.toml.example` /
   `docs/` alongside the other gateway platforms; note the CA-signed-cert
   callback requirement.

## Out of scope (v1)

- Rich messages (flex/button templates), stickers out; text-only replies.
- Streaming (no edit API → `GatewayReply` final-only, like LINE).
- Board/Calendar/Drive APIs.
- Multi-bot / multi-domain support.

## Verification

- `cargo test -p openab-gateway`
- Manual: `GATEWAY_LISTEN=0.0.0.0:8080` + tunnel (e.g. cloudflared) with a
  real LINE WORKS Developer Console bot; confirm inbound event → `/ws`
  broadcast and outbound reply round-trip.
