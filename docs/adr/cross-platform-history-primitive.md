# ADR: Cross-Platform History Primitive

- **Status:** Proposed
- **Initial Date:** 2026-06-21
- **Last Updated Date:** 2026-06-21
- **Author:** @canyugs

---

## 1. Problem

When an OpenAB bot is `@`-mentioned in a group chat, it has no context for what was discussed in the room before the mention. Three concrete failure modes:

1. **Cold-open mention.** User starts a thread of discussion in the group, then mentions the bot for the first time. The bot has no idea what is being discussed.
2. **Re-engagement after silence.** Users discuss for hours without the bot, then `@` it. Bot sees only the trigger message — equivalent to looping a colleague into an email reply without `Reply-All` including the prior thread.
3. **Multi-bot ambiguity.** Bot A and Bot B are both in the room. Bot A is `@`-ed but Bot B is not. Should Bot B "see" the discussion that happened in between? There is no universal answer; the current `@`-only dispatch model effectively answers "no" by default.

`@`-only dispatch is correct (avoids unsolicited chatter), but the asymmetry — "trigger present ⇒ full context awareness" — is not satisfied today.

This ADR proposes a **cross-platform** primitive. Two pull requests on the `openabdev/openab` repository motivated this work:

- [#1164 / #1165](https://github.com/openabdev/openab/pull/1165): opt-in implicit buffer for the LINE adapter only. Tactical, ships fast, fails the multi-bot case by design.
- The discussion that followed: should OpenAB grow a generic primitive that works across all nine supported platforms, including Telegram / LINE / LINE WORKS / WeCom which cannot retrieve message history natively?

---

## 2. Platform Inventory

Verified against official documentation 2026-06-20.

| Platform | Native history API | Endpoint / scope | Practical state |
|---|---|---|---|
| Discord | ✅ | `GET /channels/{id}/messages` + `READ_MESSAGE_HISTORY` permission | 100 msgs/page, standard pagination, reliable |
| Slack | ⚠️ Severely degraded | `conversations.history` + `channels:history` etc. | **Non-Marketplace apps: 1 req/min, 15 msgs/req** (effective 2025-05-29 for new installs, 2026-03-03 for all). Effectively unusable for OpenAB self-host until Marketplace listing exists |
| Telegram | ❌ | No `getChatHistory` in Bot API. MTProto user-session can but is not a bot scope. | Hard architectural restriction |
| MS Teams | ✅ | Graph `GET /teams/{tid}/channels/{cid}/messages` + RSC `ChannelMessage.Read.Group` | Requires Resource-Specific Consent at install time |
| Google Chat | ✅ | `spaces.messages.list` + `chat.app.messages.readonly` | 1000 msgs/page, native thread + time-range filters. **Workspace admin must approve** the `chat.app.messages.readonly` scope. **App auth cannot see private/DM messages.** |
| Feishu / Lark | ✅ | `GET im/v1/messages` + `im:message` | Standard pagination, requires scope grant |
| LINE (consumer) | ❌ | Only webhook callback at delivery | Hard restriction |
| LINE WORKS | ❌ | Only webhook callback at delivery (see `line-works-adapter.md` ADR for verification) | Hard restriction |
| WeCom (bot scope) | ❌ | Bot can only send and receive callback | Hard restriction |
| WeCom (会话内容存档 / msgaudit) | ⚠️ Separate compliance app | Requires admin-installed "会话内容存档" application, separate secret, separate scopes. Not bot scope. | **Out of scope for this ADR** — purpose is enterprise compliance/audit, not bot context. Using it for context injection has unclear ToS standing and changes the trust model. |

### Counts

- **Native API usable today (bot-scope, no severe degradation):** 4 — Discord, MS Teams, Google Chat, Feishu.
- **Native API exists but practically unusable for OpenAB:** 1 — Slack (post-2026-03 rate limit).
- **No native API; gateway must buffer if history is wanted at all:** 4 — Telegram, LINE, LINE WORKS, WeCom.

A design that relies on native API alone leaves **5 of 9 platforms** without history. Telegram and the LINE family are not marginal — they cover the JP/KR/TW corporate market that OpenAB explicitly serves, plus the largest free-tier individual-developer base. They cannot be treated as second-class.

---

## 3. Two Storage Models (the part PR #1165 collapses)

The conversation around PR #1165 frames the choice as three UX options (implicit auto-prepend / `/history` slash command / `openab get` agent-pull). They are actually **two storage models** with three different read surfaces.

| Storage model | Lifecycle | Consumers | Cleared by | Multi-bot safe? | Memory profile |
|---|---|---|---|---|---|
| **Ephemeral** (PR #1165) | Mention consumes and clears | 1 (the mentioned bot) | Read | ❌ — Bot A's read clears Bot B's pending view | Near-zero (buffer usually empty) |
| **Persistent rolling window** (this ADR proposes) | Always-on per-room | N (any bot or slash command or agent tool) | TTL / size cap only | ✅ — Reads do not mutate state | High in hot rooms (N msgs × M rooms) |

Persistent is a superset of ephemeral — anything ephemeral can do, persistent can also do, **plus** support slash commands, agent-pull, and multi-bot rooms.

PR #1165's design cannot be incrementally extended into persistent: clear-on-read is the core of its memory savings. The two paths require different storage layers.

---

## 4. Proposed Design: Buffer-as-Canonical, Native-as-Backfill

```
                ┌──────────────────────────────────────────────┐
                │       Gateway-side per-room buffer            │
                │       (rolling window, TTL + size cap,        │
                │        keyed by platform:tenant:channel)      │
                └──────────────────────────────────────────────┘
                          ▲                       │
            always-write  │                       │  reads do not mutate
           every inbound  │                       ▼
           message        │            ┌─────────────────────┐
                          │            │   Read surfaces      │
                          │            │  - /history slash    │
                          │            │  - openab get history│
                          │            │  - opt-in implicit   │
                          │            │    inject on mention │
                          │            └─────────────────────┘
                          │
                          │  (backfill on cold start,
                          │   only for native-capable platforms,
                          │   only when buffer is below floor)
                          │
            ┌─────────────┴───────────────────────────────────┐
            │   Optional native-API backfill                   │
            │                                                  │
            │   if platform.supports_history() and             │
            │      buffer.size(room) < BACKFILL_FLOOR:         │
            │      pull = native.list(room,                    │
            │                         pageSize=BACKFILL_FLOOR) │
            │      buffer.prepend(pull)                        │
            │                                                  │
            │   Eligible platforms (admin/permission           │
            │   prerequisites met):                            │
            │     - Discord (READ_MESSAGE_HISTORY)             │
            │     - MS Teams (ChannelMessage.Read.Group RSC)   │
            │     - Google Chat (chat.app.messages.readonly,   │
            │       Workspace admin approval; public spaces    │
            │       only; not DMs)                             │
            │     - Feishu (im:message)                        │
            │     - Slack: defer (rate limits make it          │
            │       unattractive until Marketplace path)       │
            └──────────────────────────────────────────────────┘
```

Principles:

1. **The buffer is the source of truth.** Every read surface (slash, agent-pull, implicit injection) reads from the same per-room buffer. Behaviour is identical across platforms.
2. **Native API is a *backfill optimization*, not a primary path.** It fills the buffer on cold start or after eviction; it does not service reads directly. This keeps the read path platform-agnostic.
3. **Read does not mutate.** Multi-bot rooms work; multiple reads of the same window return the same data.
4. **One bounded eviction policy.** TTL (e.g. 24h) + size cap (e.g. 500 msgs per room) + LRU on room count cap.

### Read surfaces (the UX layer)

| Surface | Trigger | Use case | Default? |
|---|---|---|---|
| **`/history N`** (or platform equivalent) | User runs explicit command | "Bot, here's the context I think you need" — predictable, observable, audit-friendly | Opt-in per deployment |
| **`openab get history`** (agent tool) | Agent decides during reasoning | Agent self-fetches context only when relevant — token-efficient, composable with skills | Opt-in per deployment |
| **Implicit auto-inject on mention** | Mention triggers automatic prepend of last N | Single-bot rooms where users universally expect the bot to "have been listening" | Opt-in **per platform**, **off by default**; reasonable to default-on for LINE / LINE WORKS / WeCom 1:1 + small group where multi-bot is rare and the no-context UX is severe |

The three surfaces are not mutually exclusive — they co-exist on the same buffer with different invocation models.

---

## 5. Cache Location: Open Question

Two reasonable homes for the buffer; pick before implementation.

| Location | Pros | Cons |
|---|---|---|
| **Gateway-side** (per-adapter, in `openab-gateway` process) | Already sees every inbound webhook event. Natural deduplication point. Survives OAB core restarts as long as gateway is up. | Each adapter holds its own buffer; if two OAB cores connect to one gateway, both see the same history (probably desirable). Multi-tenant gateway must namespace by `domainId` (already a LINE WORKS requirement). |
| **OAB core-side** (alongside `SessionPool`) | One process owns both session + history → simpler reasoning, less IPC. Discord / Slack adapters (which live in OAB core today) get it for free. | Gateway adapters must forward every msg (not just mentions) over the WebSocket — increases gateway↔core traffic substantially. Multi-OAB-core fan-out causes duplicate buffers. |

Recommendation: **gateway-side**, with `GatewayEvent` carrying a `room_buffer_excerpt` field when the read surface requires it. Rationale: the gateway already sees every event; adding a buffer is local. Forcing every msg through to OAB core to populate a core-side buffer is a 10–100× traffic increase for `@`-only deployments.

---

## 6. Retention, Privacy, ToS Considerations

1. **Retention.** TTL and size cap must be configurable. Default conservatively (24h / 500 msgs) to limit exposure on a leaked deployment.
2. **Operator visibility.** The buffer holds messages users did not explicitly send to the bot. The deployment's privacy policy must reflect that the room buffer exists; this is the same constraint PR #1165 already faces.
3. **Native-API ToS.** Workspace / corporate admin who installs the OpenAB app on Google Chat / Teams / Feishu is implicitly authorising the bot to read room history. The OpenAB operator-facing docs must state this clearly so the admin can decline by withholding the scope.
4. **WeCom 会话内容存档.** Explicitly out of scope. Using a compliance/audit pipeline as a bot's context source crosses a trust boundary and likely violates Tencent's ToS for that product. Operators who want compliance-grade message capture should run msgaudit independently, not via OpenAB.
5. **End-user opt-out.** Per-room or per-user opt-out (e.g. a "do not buffer my messages" flag) is desirable but out of scope for v1.

---

## 7. Relationship to Existing Work

| Work | Relationship |
|---|---|
| [PR #1165](https://github.com/openabdev/openab/pull/1165) (LINE implicit buffer) | A tactical subset of the implicit auto-inject read surface, scoped to LINE only with ephemeral storage. This ADR proposes upgrading the storage to persistent and generalising across platforms. PR #1165 can ship first under its current scope; this ADR is the architectural successor. |
| `docs/adr/line-works-adapter.md` | Independent — LINE WORKS adapter does not depend on this ADR and is deferred for unrelated (tenant-availability) reasons. When LINE WORKS ships, it benefits from this primitive automatically because it sits in the same gateway crate. |
| `openab get` / `openab set` (new agent tool surface) | This ADR defines `openab get history` as one of the three read surfaces. Schema and pagination behaviour need to align with the existing `openab get` conventions. |

---

## 8. Open Questions

1. **Cache location** (Section 5) — gateway-side or core-side. Recommendation: gateway-side. Needs maintainer concurrence.
2. **Default retention numbers** — 24h / 500 msgs is a guess. Real numbers should come from operator feedback after a beta deployment.
3. **Implicit-mode default per platform** — off everywhere is safest, but LINE / LINE WORKS / WeCom 1:1 may need default-on to be useful. Per-platform default policy needs to be decided in the implementation PR.
4. **Slack handling** — given the 1 rpm / 15 msg cap on non-Marketplace apps, should backfill be disabled by default on Slack and re-enabled only for Marketplace-listed deployments?
5. **`/history` slash command syntax** — does each platform get its own slash-command surface (Discord slash, Slack slash, etc.), or does the gateway intercept a magic `@bot /history N` text pattern uniformly? Cross-platform consistency vs platform-native UX.
6. **Multi-OAB-core deployments** — if N OAB cores connect to one gateway, do they share one buffer (yes, if gateway-side) and is dedup on agent-pull guaranteed by the schema? Confirm against current `GatewayEvent` semantics.

---

## 9. Consequences

### Positive

- One mental model and one read path across all 9 platforms; agent tools and skills generalise.
- Telegram, LINE, LINE WORKS, WeCom — currently second-class for any history-aware feature — become first-class.
- PR #1165's tactical win for LINE is preserved as a special-case of the implicit read surface.
- Multi-bot rooms (Discord especially) become correctly handled by design rather than ignored.
- Native API integrations become *optional optimizations* rather than load-bearing dependencies — graceful degradation when an admin declines the scope.

### Negative

- Gateway memory pressure scales with `active_rooms × buffer_cap × avg_msg_size`. Hot deployments need capacity planning.
- New operator-facing surface (privacy policy, retention configuration, scope grant docs). Onboarding gets heavier.
- Persistent buffer is a new data-protection surface — leaked gateway dumps now contain user messages, not just bot replies.
- Slash command / agent-tool surfaces need per-platform implementation work even though the underlying buffer is unified.

---

## 10. Notes

- **Version:** 0.1 (Proposed)
- **Changelog:**
  - 0.1 (2026-06-21): Initial proposed version. Synthesises platform-API research and the PR #1164/#1165 discussion thread.

---

## References

### OpenAB internal

- [PR #1165 — opt-in group context buffering for LINE adapter](https://github.com/openabdev/openab/pull/1165) and [Issue #1164](https://github.com/openabdev/openab/issues/1164).
- `docs/adr/line-adapter.md` — Consumer LINE adapter; introduces the always-on / shared-room semantics that motivate this ADR.
- `docs/adr/line-works-adapter.md` — LINE WORKS adapter; also lacks a native history API, would consume this primitive when implemented.
- `gateway/src/schema.rs` — `GatewayEvent` schema that any cross-adapter buffer must be compatible with.

### Platform documentation (verified 2026-06-20)

- [Discord channels API](https://docs.discord.com/developers/resources/channel) — `GET /channels/{id}/messages`.
- [Slack conversations.history](https://docs.slack.dev/reference/methods/conversations.history/) and [non-Marketplace rate limit change (2025-05-29)](https://docs.slack.dev/changelog/2025/05/29/rate-limit-changes-for-non-marketplace-apps/).
- [Telegram Bot API limitations](https://core.telegram.org/bots/api) — no `getChatHistory`.
- [Teams channel messages for bots (RSC)](https://learn.microsoft.com/en-us/microsoftteams/platform/bots/how-to/conversations/channel-messages-for-bots-and-agents).
- [Google Chat `spaces.messages.list`](https://developers.google.com/workspace/chat/api/reference/rest/v1/spaces.messages/list) and [practical guide](https://developers.google.com/workspace/chat/list-messages).
- [Feishu `im/v1/messages` list](https://open.feishu.cn/document/server-docs/im-v1/message/list).
- [LINE WORKS Bot API reference](https://developers.worksmobile.com/en/docs/bot-api/) — no history endpoint.
- [WeCom 会话内容存档](https://developer.work.weixin.qq.com/document/path/91774) — out of scope; compliance product, not bot scope.
