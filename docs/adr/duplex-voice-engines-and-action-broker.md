# ADR: Discord Voice Intent Delegation

- **Status:** Accepted — Slice 1 implemented and local startup-validated; live voice flow pending
- **Date:** 2026-07-12
- **Implementation:** Voice receive foundation and opt-in text-confirmed intent broker implemented
- **Implementation branch:** `feat/discord-voice-receive`
- **Direction update:** [canyugs/openab#20 comment 4951151976](https://github.com/canyugs/openab/issues/20#issuecomment-4951151976)
- **Related:** [Discord Voice receive ADR](discord-voice.md), [Discord multi-agent guide](../multi-agent.md), [Issue #1364](https://github.com/openabdev/openab/issues/1364), [Issue #1368](https://github.com/openabdev/openab/issues/1368)

---

## 1. Decision

The first daily voice-delegation path will build on OpenAB's existing Discord
Voice receive implementation.

```text
Discord Voice Channel
  → OpenAB Songbird receive
  → STT or a future Realtime/Live engine
  → semantic intent proposal
  → one semantic confirmation
  → deterministic Discord text mention
  → existing OpenAB bot-to-bot and ACP flow
```

The voice-dispatching OpenAB instance will gain a small deterministic intent
broker inside or immediately beside its Discord Voice subsystem. It receives an
intent proposal, asks the operator to confirm what it understood, and dispatches
the confirmed task exactly once as a Discord message to an existing OAB agent.

The receiving B0-B15 agents continue using their existing Discord and ACP code.
They do not receive a new voice protocol or a new ACP executor.

The following are explicit decisions:

- use the current Discord Voice receive branch as the foundation;
- modify the voice-dispatching OpenAB instance;
- keep target OAB agents unchanged except for Discord configuration;
- keep Discord text as the dispatch and audit surface;
- confirm semantic intent once before dispatch;
- let parser, LLM, Realtime, or GPT-Live implementations only propose intents;
- let deterministic OpenAB state, not the model, perform final dispatch;
- do not involve OCP in the first path; and
- do not block initial dispatch on structured ACP lifecycle observation.

## 2. Product Goal

The product goal is a daily voice interface for delegating real work to the
existing OpenAB agent fleet.

The target interaction is:

```text
Can, in Discord Voice:
  "叫 Sam 看 canyugs/openab issue 20"

OpenAB drafts:
  target = Sam
  task = inspect canyugs/openab#20

OpenAB asks:
  "要請 Sam 看 canyugs/openab#20 嗎？"

Can:
  "對"

OpenAB posts in the pinned text channel:
  <@SAM_DISCORD_ID> Can asked via voice:
  請看 canyugs/openab#20

OpenAB says:
  "已送出。"
```

The confirmation checks the target and intended task. It is not a review of the
exact generated Discord prose and it does not require another approval after a
clear spoken yes.

The final daily-use experience should not require looking at the phone. The
implementation is deliberately sliced, however, so the first engineering slice
uses text confirmation before spoken confirmation and TTS are added.

## 3. Revised Boundary

### 3.1 What changes in OpenAB

The voice-dispatching instance adds:

- an intent proposal contract;
- target alias resolution;
- one pending-intent state machine per guild/voice session;
- yes/no/correction/timeout handling;
- exactly-once Discord mention dispatch;
- later, spoken confirmation input;
- later, Songbird TTS playback; and
- optionally, thread/result observation.

### 3.2 What stays unchanged

Receiving OAB agents retain the current flow:

```text
Discord bot message
  → trusted bot mention admission
  → normal thread creation/detection
  → normal OpenAB dispatch
  → existing ACP coding agent
  → Discord result
```

Each target bot only needs to trust the voice-dispatching bot identity and allow
the pinned channel:

```toml
[discord]
allowed_channels = ["<VOICE_CONTROL_CHANNEL_ID>"]
allow_bot_messages = "off"
trusted_bot_ids = ["<VOICE_DISPATCH_BOT_USER_ID>"]
```

The dispatch must contain a real Discord mention token such as
`<@TARGET_BOT_USER_ID>`, not plain display text such as `@Sam`.

### 3.3 What is no longer the first path

The external phone application plus standalone Discord broker described by the
previous revision is no longer the selected first implementation. It remains a
possible alternative, but it would duplicate Discord Voice receive, STT, and
platform ownership that OpenAB already has.

OCP is also not part of this path. OpenAB already owns Discord adapters and the
voice session. Adding a second coordination plane would duplicate responsibility
without helping the initial intent-confirmation loop.

### 3.4 First-path non-goals

- no OCP integration;
- no new ACP executor;
- no direct Discord dispatch by a parser, LLM, Realtime, or Live model;
- no requirement to turn every voice session into a meeting summary;
- no automatic task dispatch without semantic confirmation;
- no full-duplex, wake-word, barge-in, or echo-cancellation requirement in v1;
- no requirement to finish thread/result observation before the first dispatch
  slice; and
- no redesign of existing OpenAB permission or security policy in this ADR.

## 4. Current Status

### 4.1 Capability status on 2026-07-12

| Capability | State | Evidence / next gap |
|---|---|---|
| `/voice join`, `/voice status`, `/voice transcript`, `/voice summary`, `/voice stop` | **Implemented** | Existing branch commands are wired through the Discord adapter. |
| Songbird Discord Voice receive | **Implemented** | Decode receive, DAVE registration, and the voice session lifecycle exist. |
| Discord speaker attribution | **Implemented; partially live-validated** | SSRC maps to Discord user ID; one-speaker attribution and timestamps passed. Two-speaker soak remains pending. |
| PCM segmentation and bounded STT queue | **Implemented** | Per-user 48 kHz stereo segmentation, in-memory WAV encoding, bounded workers, and drop accounting exist. |
| STT and retained transcript | **Implemented; accuracy retry pending** | Groq `whisper-large-v3` with `language = "zh"` is deployed. The first sample was not reliable enough. |
| Explicit ACP transcript summary | **Implemented** | `/voice summary` uses the current normal ACP path. This remains separate from intent delegation. |
| Intent proposal/parser | **Implemented for the Slice 1 simple grammar** | Resolves one configured target in command-shaped speech. Target-head coordination grammar and typed clarification remain pending. |
| Pending intent state machine | **Implemented and unit-tested** | Session/operator binding, posting/waiting/dispatching phases, correction, timeout generations, replay protection, and stale-session cleanup exist. |
| Text confirmation and voice-specific mention dispatch | **Implemented; local startup passed, live flow pending** | Text yes/no/correction and nonce-enforced real Discord mention dispatch are wired. The Slice 1 image starts cleanly in local Kubernetes; a real spoken proposal and target-agent handoff still require operator validation. |
| Spoken yes/no/correction | **Not started** | Captured transcript is not interpreted as confirmation state. |
| TTS and Songbird playback | **Not started** | There is no TTS provider or playback consumer. |
| Realtime / GPT-Live proposal backend | **Not started** | No Realtime session or tool/event integration exists. |
| Thread/result observation | **Not started** | Existing Discord seams are available, but no voice job observer is wired. |
| Typed ACP completion seam | **Implemented and unit-tested; orthogonal** | Useful later for structured observation, but not a v1 dispatch blocker. |

### 4.2 Local Kubernetes status

The current Slice 1 deployment in `docker-desktop/openab-local` is healthy:

- deployment `openab-voice-voice`: `1/1` ready;
- pod restarts: `0`;
- Helm release `openab-voice`: revision `7`, status `deployed`;
- image:
  `localhost:5555/openab:claude-voice-intent-s1-8acd16522718`; and
- image digest:
  `sha256:3369087b88df1c6b29458d0b61e628d31098b8cc21bb59b9acc0afacd2286d66`.

Startup logs confirm `voice_intent_enabled=true`, Groq STT configuration,
Aragorn's Discord connection, and global slash-command registration. This is
configuration and runtime-startup evidence, not yet proof of a spoken proposal,
text confirmation, Sam/Frodo handoff, TTS, or Realtime. The previous
`claude-voice-zh-stt` image predates Slice 1 and is no longer the active local
artifact.

### 4.3 Daily-UX readiness

| Slice | User experience | Daily hands-free ready? |
|---|---|---|
| Existing receive/STT | Meeting-style capture, transcript, explicit summary | No |
| Slice 1: text confirmation | Validates intent broker and dispatch, but requires looking at Discord | No |
| Slice 2: spoken confirmation | The operator can answer by voice, but the prompt is still text-only | Not fully |
| Slice 3: TTS playback | Confirmation, sent, cancelled, and error prompts are audible | **First hands-free daily milestone** |
| Slice 4: Realtime/Live | Improves conversational parsing and response latency | Enhancement |
| Slice 5: observation | Enables "I will keep tracking and report back" | Full daily-assistant loop |

Text-first confirmation is implementation scaffolding, not the final product
experience.

## 5. V1 Intent State Machine

V1 keeps one pending intent per guild and active voice-session generation.

```text
Idle
  │
  ▼
DraftingIntent
  │
  ├─ ambiguous ───────────────► Idle / ask for a new request
  │
  ▼
WaitingConfirmation
  │
  ├─ explicit no ─────────────► Abandoned ─► Idle
  ├─ timeout ─────────────────► Abandoned ─► Idle
  ├─ correction ──────────────► DraftingIntent
  └─ explicit yes ────────────► Dispatching
                                    │
                                    ├─ post failed ─► Abandoned / retryable
                                    └─ post once ───► Dispatched ─► Idle
```

Rules:

1. Only one intent may wait for confirmation per guild/voice session in v1.
2. Only an explicit yes or no resolves the confirmation question.
3. A correction replaces the pending intent and produces a new paraphrase.
4. A timeout abandons the intent; it never dispatches automatically.
5. Stopping or replacing the voice session abandons its pending intent.
6. A proposal engine cannot transition directly to `Dispatching`.
7. Dispatch is idempotent and occurs at most once for an intent ID.
8. After a successful post, repeated STT or provider events cannot post it again.
9. A new task starts only after the previous pending confirmation is resolved.

These are interaction semantics, not model prompt conventions. They must be
represented in Rust state and tested independently of STT, TTS, and Realtime.

## 6. Intent Contract

All proposal engines emit the same semantic contract:

```rust
struct IntentProposal {
    target: String,
    task: String,
    context_refs: Vec<ContextRef>,
}

enum ContextRef {
    GitHubIssue { repository: String, number: u64 },
    GitHubPullRequest { repository: String, number: u64 },
    Url(String),
    Text(String),
}
```

Equivalent provider output:

```json
{
  "type": "propose_delegation",
  "target": "sam",
  "task": "看 canyugs/openab issue 20",
  "context_refs": ["github:issue/canyugs/openab#20"]
}
```

OpenAB normalizes this into a pending intent:

```rust
struct PendingIntent {
    intent_id: IntentId,
    voice_session: VoiceSessionToken,
    guild_id: GuildId,
    speaker_id: UserId,
    target: ResolvedTarget,
    task: String,
    context_refs: Vec<ContextRef>,
    paraphrase: String,
    state: IntentState,
}
```

The proposal engine may be:

- a small deterministic target/task grammar;
- an LLM over the latest attributed transcript segment;
- OpenAI Realtime function calling; or
- a future GPT-Live delegation event.

Changing the proposal engine must not change confirmation or dispatch
semantics.

## 7. Target Resolution and Dispatch

### 7.1 Target registry

Voice aliases resolve through configuration owned by the voice-dispatching bot:

```toml
[discord.voice.intent]
enabled = false
confirmation_timeout_seconds = 30

[discord.voice.intent.targets.sam]
discord_user_id = "<SAM_DISCORD_BOT_USER_ID>"
aliases = ["sam", "山姆"]
```

The exact configuration shape may change during implementation. Required
properties are:

- a stable canonical target name;
- the real Discord bot user ID;
- spoken aliases; and
- deterministic rejection of unknown or ambiguous targets.

### 7.2 Dispatch message

After confirmation, OpenAB constructs the Discord message itself:

```text
<@TARGET_BOT_USER_ID> Can asked via voice:
請看 canyugs/openab#20
```

The model does not generate the mention token, destination channel, author, or
idempotency key.

The voice session's pinned control channel is the initial dispatch destination.
The existing target OpenAB instance then creates or joins its normal task thread
and executes through ACP.

### 7.3 Receiving bot configuration

The receiving target must include the voice bot in `trusted_bot_ids`. A trusted
bot's explicit mention already passes the current Discord admission path even
when `allow_bot_messages = "off"`.

No target-agent Rust change is required.

## 8. Confirmation UX

### 8.1 Slice 1: text scaffold

The first implementation posts the paraphrase in the pinned text channel and
accepts an explicit text yes/no/correction. This validates parsing, state,
timeout, and exactly-once dispatch.

It is not described as hands-free or daily-ready because the operator must look
at Discord.

### 8.2 Slice 2: spoken response

While `WaitingConfirmation`, new final STT segments from the same voice-session
generation are interpreted as:

- affirmative;
- negative;
- correction; or
- unrelated speech.

Unrelated speech leaves the intent pending. A correction replaces the intent
and asks again.

### 8.3 Slice 3: TTS prompt and feedback

TTS speaks only bounded broker messages:

```text
intent drafted → "要請 Sam 看 canyugs/openab#20 嗎？"
dispatched     → "已送出。"
abandoned      → "已取消。"
error          → "沒有送出，請稍後再試。"
```

V1 playback is half-duplex. While the bot is speaking, its own playback is
suppressed or ignored by the capture/confirmation pipeline. Full duplex,
barge-in, wake words, and application-level echo cancellation are later work.

## 9. Realtime and GPT-Live Backends

Realtime or GPT-Live sits behind the same `IntentProposal` contract.

The only delegation-related model tool/event is equivalent to:

```text
propose_delegation(target, task, context_refs)
```

It is not given `send_discord_message`, raw Discord REST access, or direct ACP
execution.

When a proposal event arrives:

1. OpenAB resolves the target;
2. OpenAB stores a pending intent;
3. OpenAB renders and speaks the semantic paraphrase;
4. the same deterministic confirmation state handles yes/no/correction; and
5. OpenAB performs the final Discord dispatch.

This lets the first slice use existing STT plus a simple parser and later swap
in Realtime/Live without rewriting dispatch behavior.

## 10. Optional Observation and Spoken Results

Observation is not a prerequisite for the first confirmed dispatch, but it is
required for the complete daily-assistant experience described by:

```text
"Sam has started. I will keep tracking it and report back later."
```

The first observer may use existing Discord-visible state:

- the dispatched root message ID;
- the task thread created by the target bot;
- queued/working/done/error reactions; and
- final thread messages.

The observer stores a small job record keyed by intent ID and root/thread IDs.
It reports only meaningful changes and can speak a concise final result through
the same TTS path.

The typed ACP completion seam may later provide a more authoritative structured
signal, but the first intent broker does not depend on it.

## 11. Repository Seams

| File | Existing responsibility | Intent-delegation change |
|---|---|---|
| `crates/openab-core/src/discord_voice.rs` | PCM segmentation and retained transcript primitives | Add transport-neutral intent/confirmation state types or keep them in a new sibling module. |
| `crates/openab-core/src/discord_voice_runtime.rs` | Songbird receive, STT workers, session generation | Feed final attributed transcript events to the intent broker; later own playback suppression. |
| `crates/openab-core/src/discord.rs` | `/voice` commands and pinned control channel | Add intent lifecycle command/status output and deterministic mention dispatch. |
| `crates/openab-core/src/config.rs` | Discord Voice and STT configuration | Add opt-in intent targets, timeout, parser, TTS, and later Realtime settings. Defaults remain disabled. |
| `crates/openab-core/src/stt.rs` | OpenAI-compatible transcription | Reuse unchanged for the first parser backend. |
| `crates/openab-core/src/acp_turn.rs` | Typed ACP result boundary | Optional later observation input; not required for initial dispatch. |
| `src/main.rs` | Adapter and voice manager construction | Construct the intent broker and optional TTS/proposal backend. |

A new sibling module such as `discord_voice_intent.rs` is preferred over adding
all state-machine logic directly to the Songbird callback/runtime file.

## 12. Implementation Slices

### Slice 0 — Discord Voice receive foundation

**State: implemented on the branch.**

- Voice commands;
- Songbird receive;
- Discord speaker mapping;
- PCM segmentation;
- STT workers;
- retained transcript; and
- explicit ACP summary.

### Slice 1 — Intent broker without TTS

**State: implemented in code, automated tests, and local Kubernetes startup;
real Discord Voice proposal/confirmation/handoff validation pending.**

- add target/task proposal types;
- implement one-pending-intent state per guild/voice session;
- start with a small target/task grammar;
- post the confirmation question in the pinned text channel;
- accept text yes/no/correction;
- implement timeout and session replacement;
- dispatch one real Discord mention exactly once;
- add unit tests for every state transition; and
- validate in `docker-desktop/openab-local`.

The first deterministic parser intentionally has two documented limitations:

- it supports command-shaped speech with one configured target, such as
  `叫 Sam review openab issue 20`; if another configured target alias appears
  later in the task text, the utterance is rejected rather than guessing which
  bot is the addressee; and
- unknown, ambiguous, or missing-target requests are currently ignored instead
  of producing the clarification required by acceptance scenario 2. Use
  `/voice transcript` while validating STT/parser mismatches. Typed resolution
  reasons and spoken/text clarification are follow-up work.

### Slice 2 — Spoken confirmation

**State: not started.**

- route final STT segments to confirmation matching while an intent is pending;
- accept spoken yes/no/correction from the same session;
- ensure unrelated conversation does not resolve the pending state;
- make duplicate/replayed STT harmless; and
- retain text confirmation as a development fallback.

### Slice 3 — Songbird TTS playback

**State: not started. This is the first hands-free daily milestone.**

- add a TTS provider contract;
- add Songbird playback;
- speak confirmation/sent/cancelled/error prompts;
- suppress or ignore playback during capture;
- keep v1 half-duplex; and
- validate the complete no-screen flow in real Discord Voice.

### Slice 4 — Realtime / GPT-Live proposal backend

**State: not started.**

- add a backend that emits `propose_delegation`;
- preserve deterministic OpenAB confirmation and dispatch;
- compare latency and intent quality against STT + parser; and
- keep provider selection configurable.

### Slice 5 — Thread observation and spoken reporting

**State: not started.**

- correlate the dispatch root message with the target agent thread;
- observe accepted/running/question/completed/error states;
- coalesce updates;
- speak meaningful progress and final results; and
- retain job state across reconnect or restart if daily use requires it.

## 13. Acceptance Scenarios

### Intent broker

1. "叫 Sam 看 canyugs/openab issue 20" produces one normalized proposal.
2. Unknown or ambiguous target produces clarification, not dispatch.
3. Explicit yes dispatches one real mention exactly once.
4. Explicit no abandons without dispatch.
5. A correction replaces the proposal and asks again.
6. Timeout, `/voice stop`, or session replacement abandons the pending intent.
7. Duplicate STT/provider events cannot post the task twice.
8. A model proposal alone cannot dispatch.

### Discord integration

9. The message is posted in the pinned text channel under the voice bot's own
   Discord identity.
10. The target receives a real `<@...>` mention and enters its unchanged normal
    bot-to-bot/ACP path.
11. `trusted_bot_ids` admits the voice-dispatching bot without enabling all bot
    chatter.
12. A dispatch failure is reported as not sent and remains safe to retry.

### Daily voice UX

13. The operator hears the semantic paraphrase and can answer without looking
    at Discord after Slice 3.
14. "對", "不是", and a corrected task are distinguished consistently.
15. The bot does not hear its own TTS as an affirmative confirmation.
16. Confirmation, sent, cancelled, and error speech is short and bounded;
    `/voice stop` terminates playback cleanly while conversational barge-in is
    deferred.

### Optional observation

17. After Slice 5, OpenAB can say that the target started only after observing
    corresponding Discord evidence.
18. A final result is associated with the correct dispatched intent and is not
    spoken into a replacement voice session accidentally.

## 14. Alternatives Considered

### External phone voice broker with no OpenAB changes

Not selected for the first implementation. It offers a clean independent voice
client, but duplicates Discord Voice/session behavior already implemented on
this branch and moves the product away from its strongest existing foundation.

It may remain useful later for a device-native assistant outside Discord Voice.

### OCP-mediated voice delegation

Not selected. OCP coordinates stock OpenAB runtimes; it does not need to own a
platform interaction already handled by OpenAB's Discord adapter.

### Send every transcript directly to ACP or Discord

Rejected. The desired unit is a confirmed semantic intent, not a raw transcript.

### Let Realtime call Discord directly

Rejected. Realtime proposes an intent; deterministic application state performs
the dispatch after confirmation.

### Build TTS/full duplex before the intent state machine

Rejected. Confirmation and exactly-once dispatch are the reusable core. TTS is
added after those semantics exist, and full duplex can wait.

## 15. Consequences

### Positive

- Reuses the branch's working Discord Voice receive and STT code.
- Keeps real program execution in existing ACP agents.
- Adds one small state machine instead of another execution runtime.
- Preserves Discord bot identity and the existing multi-agent audit trail.
- Allows a simple parser first and Realtime/Live later.
- Provides a clear path from engineering scaffold to hands-free daily use.
- Keeps OCP and target-agent code out of the first implementation.

### Trade-offs

- The voice-dispatching OpenAB instance now owns additional conversation state.
- Slice 1 is not yet the intended no-screen UX.
- TTS requires Discord playback and half-duplex handling.
- V1 allows only one pending confirmation per guild/session.
- Background progress/reporting arrives after the core dispatch loop.
- Full duplex, barge-in, wake words, and echo cancellation remain later work.

## References

- [Updated direction on canyugs/openab#20](https://github.com/canyugs/openab/issues/20#issuecomment-4951151976)
- [Discord Voice receive ADR](discord-voice.md)
- [OpenAB Discord multi-agent guide](../multi-agent.md)
- [OpenAB trusted bot admission](../discord.md#trusted_bot_ids)
- [OpenAI Realtime conversations and function calling](https://developers.openai.com/api/docs/guides/realtime-conversations)
- [Songbird](https://github.com/serenity-rs/songbird)
- [Issue #1364](https://github.com/openabdev/openab/issues/1364)
- [Issue #1368](https://github.com/openabdev/openab/issues/1368)
