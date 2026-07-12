# ADR Review: identity-trust-none v2 (#1291) — WeCom & Google Chat

**Reviewer perspective:** @Can (WeCom / Google Chat unified-mode testing, Plan 1 local k8s)  
**PR:** https://github.com/openabdev/openab/pull/1291  
**Related:** https://github.com/openabdev/openab/pull/1263 (first-class platform config)

## Verdict

**Direction is reasonable and should ship before 0.10.0.** The v1 ADR's chokepoint problem (`handle_message()` not on the live path) is real — we hit it on the unified path where `process_gateway_event()` + `gate_incoming()` is the actual gate today, not `handle_message()`. Moving to Receiver → Trust Gate → Handler with `GatedEvent` closes that gap structurally.

The per-platform **current vs expected** diagrams for WeCom and Google Chat are largely accurate. A few platform-specific gaps should be addressed before implementation.

---

## WeCom

### What matches today

| Item | ADR | Code today |
|------|-----|------------|
| L1 | Token signature + AES decrypt | `openab-gateway` wecom adapter |
| `platform` | `"wecom"` | `GatewayEvent::new("wecom", …)` |
| `sender_id` | UserID string (e.g. `"zhangsan"`) | `FromUserName` → `sender.id` |
| L3 deny + echo | Trust Gate | `process_gateway_event` → `gate_incoming` + throttled echo (Plan 1 verified) |
| Trust config source (interim) | `[wecom].allowed_users` (future) | Shared `GATEWAY_*` env for all gateway platforms in unified mode (`main.rs`) |

### Gaps / suggestions

1. **`is_dm` should always be `true` for WeCom self-built apps.**  
   Current adapter sets `channel_type: "direct"` and `channel_id = wecom:{corp_id}:{from_user}`. There is no group callback path for this API ([wecom.md](../wecom.md) — 1:1 only). The Receiver should set `is_dm = true` explicitly so L2 `allow_dm` and echo policy ("DM preferred") behave correctly. Today `process_gateway_event` hardcodes `is_dm = false` with a phase-2 TODO — that is wrong for WeCom.

2. **Handler "group routing" in the expected diagram is misleading.**  
   WeCom self-built apps do not receive group messages on this callback. Expected Handler should be minimal: parse attachments, dispatch — no group routing until `appchat` support exists.

3. **Echo delivery is in-channel only.**  
   WeCom has no "open DM" API separate from the active 1:1 session. The ADR's "DM preferred, silent drop in groups" policy should note WeCom is always in-conversation echo (acceptable — 1:1 only).

4. **Unified mode Receiver path.**  
   ADR diagrams label WeCom as "via openab-gateway" (WS). In **unified mode** (v0.9.0+), L1 runs in-process: `wecom::webhook` → `GatewayEvent` → `process_gateway_event` — no WebSocket hop. Add a third diagram variant or footnote so implementers do not require a standalone gateway container for unified deployments.

5. **`[wecom]` section vs `GATEWAY_*` env (depends #1263).**  
   Interim unified deploys (Plan 1) use one `GATEWAY_ALLOWED_USERS` for all six platforms. First-class `[wecom].allowed_users` is required for production WeCom-only allowlists. ADR §6 precedence should explicitly say: when `[wecom]` exists, it overrides `GATEWAY_*` for `platform == "wecom"` events.

---

## Google Chat

### What matches today

| Item | ADR | Code today |
|------|-----|------------|
| L1 | JWT RS256 via Google JWKS | When `GOOGLE_CHAT_AUDIENCE` is set |
| `platform` | `"googlechat"` | `GatewayEvent::new("googlechat", …)` |
| `sender_id` | `users/123456789` resource name | `sender.name` (full string, not stripped) |
| Bot filter | Handler-level | `user_type == "BOT"` dropped in webhook before event emit |
| L3 deny + echo | Trust Gate | Verified in Plan 1 (deny-all → request-access message) |

### Gaps / suggestions

1. **`is_dm` must be derived from `space_type`.**  
   Adapter already has `space_type` (`DM` vs `ROOM`). Receiver should set `is_dm = (space_type == "DM")`. Hardcoding `is_dm = false` in `process_gateway_event` breaks L2 `allow_dm` for Google Chat DMs and breaks echo policy in spaces (UID leakage risk in `ROOM`).

2. **L1 is conditional today — document in ADR L1 table.**  
   JWT verification only runs when `GOOGLE_CHAT_AUDIENCE` is set. Without it, webhooks are accepted (dev convenience). ADR should mark Google Chat L1 as "JWT when `GOOGLE_CHAT_AUDIENCE` configured; otherwise dev-only passthrough" so operators know production requires audience = full App URL.

3. **Echo in spaces — align with ADR safeguards.**  
   Plan 1 echo works in dev (dry-run reply logged). In production with SA key, echo goes to the space thread. ADR "silent drop if DM unavailable" is correct for `ROOM` — implementers should not echo `users/…` into a shared space. Confirm `send_echo` uses DM space when `space_type == "ROOM"`.

4. **`allowed_users` format — document both stripped and full?**  
   Code stores `users/123456789` in `sender.id`. Echo message shows full resource name. Docs/examples should use **full** `users/…` in `[googlechat].allowed_users`, not numeric-only, to match `HashSet::contains`.

5. **Handler scope.**  
   "Space routing" + thread via `thread_id` is accurate. @mention required per message in spaces is platform logic — stays in Handler, not Trust Gate. ✅

---

## Cross-cutting (both platforms)

| Topic | Feedback |
|-------|----------|
| Per-event `platform` lookup | **Critical for unified mode.** Single `UnifiedGatewayAdapter` must not use `adapter.platform() == "unified"`. Already called out in ADR — keep it. |
| `GatedEvent` type seal | Good compile-time guarantee. Unified path should use same gate as WS path. |
| Phased rollout (§6) | Support — Plan 1 deployments rely on interim `gate_incoming`; Phase 2 should flip defaults only after `[wecom]` / `[googlechat]` config exists. |
| Bot bypass L3 | Google Chat bots never reach core (filtered at webhook). WeCom `is_bot: false` always. ADR bot bypass is fine; no loop risk on these two platforms. |
| Rate-limited echo | Already in `process_gateway_event` (`echo_allowed`). Align ADR 5-minute throttle with existing implementation or document change. |

---

## Plan mapping

| Plan | ADR work |
|------|----------|
| Plan 1 ✅ | Interim `gate_incoming` on unified path; local k8s scripts; deny-all smoke tests |
| Plan 2 | Implement ADR v2 + #1263; `dev-up-*-kiro.sh`; per-platform allowlists |
| 0.10.0 | Ship after Plan 2 + phased migration docs |

---

## Summary for maintainers

- ✅ Approve three-layer architecture and per-event platform trust lookup.
- ⚠️ Fix unified-mode documentation (no WS hop in v0.9.0+).
- ⚠️ Implement `is_dm` derivation for WeCom (`true`) and Google Chat (`space_type == "DM"`) before L2/echo policy is trustworthy.
- ⚠️ Narrow WeCom Handler scope (no group routing).
- ⚠️ Document Google Chat L1 as audience-gated.
- 🔗 Block per-platform production config on #1263 merging first.