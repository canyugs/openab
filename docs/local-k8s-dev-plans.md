# Local Kubernetes Dev Plans

A staged roadmap for the OpenAB local dev platform on **Docker Desktop Kubernetes** (`openab-local` namespace, `docker-desktop` context).

One namespace, multiple deployments — each profile is an independent Deployment + Service that can be scaled up or torn down without affecting others.

## Axes

Every plan composes three axes:

| Axis | Examples |
|------|----------|
| **OpenAB build** | Image tag, `Dockerfile.unified` target (`agentcore`, `kiro`, `claude`, …) |
| **Platform** | Env toggles on the unified binary: WeCom, Google Chat, Telegram, Discord, … |
| **Agent** | Echo (`agentcore`) for smoke tests; real CLI (`kiro`, `claude`, …) for E2E |

Scripts live under `scripts/`. Shared deploy logic: `scripts/_lib.sh`.

---

## Plan 1 — Gateway platforms (WeCom + Google Chat) ✅

**Goal:** Verify unified-mode gateway adapters end-to-end on local k8s — build, deploy, HTTPS tunnel, platform console wiring, trust gate.

**Status:** Implemented.

| Deliverable | Location |
|-------------|----------|
| Build any unified image target | `scripts/dev-build-image.sh` |
| WeCom profile | `scripts/dev-up-wecom.sh` |
| Google Chat profile | `scripts/dev-up-googlechat.sh` |
| Combined profile | `scripts/dev-up-unified.sh` |
| Step-by-step guide | [local-k8s-platform-setup.md](local-k8s-platform-setup.md) |
| Trust gate tests | `crates/openab-core/src/gateway.rs` (`unified_gate_*`) |

**In scope**

- `agentcore` echo agent for webhook / trust-gate smoke tests
- Per-platform k8s secrets (WeCom corp creds, Google Chat SA key)
- In-cluster Cloudflare quick tunnel via [openab-control-plane `dev-tunnel-k8s.sh`](https://github.com/openabdev/openab-control-plane)
- Deny-all default + `--allow-all-users` / `GATEWAY_ALLOWED_USERS` for dev

**Out of scope (later plans)**

- Real agent sessions (kiro login, ACP turns)
- Discord / Slack / Telegram profiles
- OCP council bots (`[gateway] ws://…`)
- Single `dev-up.sh` orchestrator

**Quick start**

```bash
scripts/dev-build-image.sh --target agentcore
scripts/dev-up-wecom.sh --build
scripts/dev-up-googlechat.sh --build
```

Full procedure → [local-k8s-platform-setup.md](local-k8s-platform-setup.md).

---

## Plan 2 — 0.10.0 trust refactor + real agent E2E (draft)

**Goal:** Land the [identity-trust-none ADR v2](https://github.com/openabdev/openab/pull/1291) (Receiver → Trust Gate → Handler) and [first-class per-platform config](https://github.com/openabdev/openab/pull/1263) before **0.10.0**, then validate full ACP turns from WeCom / Google Chat with a real agent.

**Depends on**

- PR #1263 — `[wecom]`, `[googlechat]`, … top-level sections (replaces shared `GATEWAY_*` env)
- PR #1291 — three-layer ingress; per-event platform trust lookup (fixes unified mode)
- Phase 0→3 rollout in ADR §6 (not hard cutover)

**Why after Plan 1**

Plan 1 proved unified webhooks + deny-all echo on local k8s. Plan 2 wires the **structural** guarantee (Trust Gate upstream of Handler) and per-platform `allowed_users` before swapping echo → kiro.

**Likely deliverables**

- Implementation of ADR v2 for WeCom / Google Chat unified path
- `scripts/dev-up-wecom-kiro.sh`, `scripts/dev-up-googlechat-kiro.sh`
- Agent credential secrets (`KIRO_API_KEY`, …)
- Regression tests: `is_dm` derivation, sender ID formats (`zhangsan` vs `users/…`)
- Device-flow login via `kubectl exec`

---

## Plan 3 — Additional platforms (draft)

**Goal:** Extend the same profile pattern to Discord, Slack, Telegram.

**Likely deliverables**

- `scripts/dev-up-discord-echo.sh` / `dev-up-discord-kiro.sh` (scaffold exists)
- `scripts/dev-up-telegram.sh` (scaffold exists)
- Platform-specific secret templates

---

## Plan 4 — OCP / council mode (draft)

**Goal:** Test legacy two-process mode — OAB bot pods connecting to openab-control-plane over WebSocket.

**Likely deliverables**

- Reuse OCP `dev-deploy-bots.sh` against `openab-local` or `oabcp-local`
- `[gateway] url = ws://control-plane:8090/ws` config
- Chair + reviewer bot profiles

---

## Plan 5 — Dev orchestrator (draft)

**Goal:** One entrypoint to build, pick profile, deploy, and print tunnel URLs.

**Likely deliverables**

- `scripts/dev-up.sh --profile wecom|googlechat|unified|discord-kiro|…`
- Optional `--build`, `--tunnel`, `--allow-all-users`

---

## Namespace layout (all plans)

```
openab-local/
├── openab-wecom              # Plan 1
├── openab-googlechat         # Plan 1
├── openab-unified            # Plan 1 (optional combined)
├── cloudflared-wecom         # Plan 1 tunnel
├── cloudflared-googlechat    # Plan 1 tunnel
├── openab-discord-kiro       # Plan 3
└── …
```

Profiles not in use: `scripts/dev-up-<profile>.sh --delete` or scale to 0.