# Handoff: native-agent-anthropic-oauth

**Parent branch:** `feat/native-agent-anthropic-oauth` (on the `fork` remote = `git@github.com:canyugs/openab.git`)
**Last handoff:** 2026-06-24

## Current status

PR **https://github.com/openabdev/openab/pull/1187** is OPEN (Closes issue #1186). Adds native **Anthropic OAuth (Claude Pro/Max)** login to `openab-agent` (a 2nd `anthropic-oauth` tenant in `~/.openab/agent/auth.json` beside the existing `codex` tenant), plus default model bump → `claude-opus-4-8`.

Reviewed by Copilot + `chaodu-agent` bots. **Blocker is cleared, all CI checks are green** (CI openab-agent: fmt/clippy/test 194+11; every Docker smoke test incl. native-sandbox). Re-review is positive. 4 commits:
- `06e90ff` feat: Anthropic OAuth login + provider
- `5b625f4` fix: default model → claude-opus-4-8
- `73bf9a2` fix: address review (CI workspace exclude, PKCE state, error UX)
- `4ef7c49` fix: flush stdout drain on ACP server shutdown

Nothing is broken. What remains is **7 non-blocking polish items** (#5–#11 from the review) + 3 human/maintainer actions.

## Next steps

### Code polish (reviewer findings #5–#11 — all 🟡/🟢 non-blocking). Recommended subset:
1. **#6 (do it):** `openab-agent/src/acp.rs` `handle_set_config_option` rebuild uses `AnthropicProvider::auto_with_model`, which prefers `ANTHROPIC_API_KEY` — a forced `anthropic-oauth` provider is silently dropped if an API key is also present. Make the rebuild honor the session's actual auth mode.
2. **#7 (do it):** `openab-agent/src/llm.rs` OAuth 401 branch does `let _ = crate::auth::force_refresh_for(...)` then `continue` — a *failed* refresh retries with the stale token instead of surfacing the error. Bubble the refresh error.
3. **#11 (do it):** `openab-agent/src/auth.rs` `refresh_token` failure message still says bare `openab-agent auth ... again`; name the tenant's subcommand (F3 leftover).
4. **#5 (recommended):** default `claude-opus-4-8` applies to API-key users too (pricier). User CONFIRMED opus-4-8 is the intended default — but reviewer suggests defaulting **by auth type** (API key → Sonnet, OAuth → Opus). Cleaner; consider doing it in `anthropic_model_from_env()` / `acp.rs` fallbacks.
5. **#8 / #9 / #10 (defer, note in PR as follow-up):** #8 `--no-browser` bare-code paste skips `state` validation (`auth.rs login_anthropic_browser_flow`, low impact); #9 `save_tokens_for` keys by `store.provider` (latent, no trigger); #10 non-Unix `write_auth_file` lacks atomic/0600 (openab runs Linux pods — same rationale as the HOME-isolation non-issue).

After edits, run the full CI-equivalent gate (see Key decisions for the build trick):
```
cd openab-agent && cargo fmt && cargo fmt --check && cargo clippy -- -D warnings && cargo test && cargo test -- --ignored
```
Then commit, `git push fork handoff-or-feature-branch`, and the PR re-runs CI.

### Human / maintainer actions (cannot be done from this session)
- **Discord Discussion URL** — PR body still says `pending`; repo auto-closes PRs without it within ~24h. User must create a Discord thread and paste the URL into the PR description.
- **Maintainer approval** — reviewers are bots; PR is `pending-maintainer`, `reviewDecision: REVIEW_REQUIRED`.
- **Delete stale fork image** `ghcr.io/canyugs/openab-native:anthropic-oauth` (built via the *legacy* Dockerfile.native path; misleading). The session's `gh` token lacks `delete:packages` scope — delete via GitHub UI or `gh auth refresh -h github.com -s delete:packages` then `gh api -X DELETE /user/packages/container/openab-native`. The CORRECT image is `ghcr.io/canyugs/openab:anthropic-oauth-native` (canonical Dockerfile.unified build).

## Related

- PR: https://github.com/openabdev/openab/pull/1187
- Issue: https://github.com/openabdev/openab/issues/1186
- Linear: none
- Reference source ported from: `/Users/can/Documents/zeabur/pi` (Pi clone — `packages/ai/src/utils/oauth/anthropic.ts`, `packages/ai/src/api/anthropic-messages.ts`)
- Other handoff branches: none

## Key decisions this session (things git log won't tell you)

- **Working folders:** This is an *independent clone* at `/Users/can/Documents/zeabur/openab-anthropic-oauth` (user wanted a separate copy folder, NOT a git worktree, because another concurrent session shares the main repo `/Users/can/Documents/zeabur/openab` and its `git checkout` was clobbering edits). Here `origin` = the local main repo; the real GitHub remote is `fork` = `canyugs/openab`. The main repo was restored to `feat/discord-api-proxy` for the other session.
- **Building openab-agent:** it's a standalone crate inside a workspace repo. In-repo `cd openab-agent && cargo …` ONLY works because of the committed root `Cargo.toml` `exclude = ["openab-agent"]` (the F1 fix). Do NOT add `[workspace]` to `openab-agent/Cargo.toml` instead — `Dockerfile.unified:28` appends `[workspace]` at build time, so a committed one causes a **duplicate-key** error and breaks the canonical image build. The `exclude` approach is the non-conflicting fix.
- **Isolated build/test trick:** to avoid the parent-workspace issue (or another session touching files), I rsync `openab-agent/{src,Cargo.toml,Cargo.lock}` into a scratchpad dir and `cargo build/test` there. The canonical image is `docker build -f Dockerfile.unified --target native .` (NOT the legacy broken `Dockerfile.native`).
- **PKCE state (F2):** claude.ai's `authorize` rejects a *short* independent `state` with "Invalid request format". Solution = independent **32-byte** random state (matches the verifier's length, value independent). 16-byte failed; 32-byte verified end-to-end with a real Pro/Max login.
- **ACP flush race (`4ef7c49`):** `run()` fed responses to a detached stdout-drain task; `#[tokio::main]` aborted it on stdin-EOF before flushing the last line. Latent (main wins by timing; this branch lost it ~85% locally → red smoke test). Fixed by capturing the drain handle and bounded-awaiting it after the loop. Race test: was 3/20, now 20/20.
- **Local test gotcha:** 5 `acp::tests` read the REAL `~/.openab/agent/auth.json` and assume no token; they FAIL on this dev machine because a live `anthropic-oauth` token exists from testing. Hide it (`mv ~/.openab/agent/auth.json{,.aside}`) to get 194/194. CI has no token → passes. Do NOT "fix" this — it's a local-dev artifact, confirmed out of scope.
- **Legacy `Dockerfile.native` is genuinely broken** (missing the `[workspace]` injection `Dockerfile.unified` does) and is referenced only by `docs/native-agent.md` + the stale `snapshot-build.yml`. Intentionally OUT OF SCOPE for this PR — separate follow-up if anyone cares.
- **Live OAuth token** is sitting in `~/.openab/agent/auth.json` (valid). Test harness: `scratchpad/agent-build-copy/drive_prompt.py` drives one ACP prompt; `docker-agent*.sh` wrappers run the agent inside an image.
