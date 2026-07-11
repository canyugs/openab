# ADR: Pluggable Duplex Voice Engines and Mediated ACP Actions

- **Status:** Proposed — implementation seams and prior art confirmed; runtime spike pending
- **Date:** 2026-07-12
- **Implementation branch:** `feat/discord-voice-receive`
- **Related:** [Discord Voice receive ADR](discord-voice.md), [Turn-boundary batching ADR](turn-boundary-batching.md), [Context-aware platform token ADR](context-aware-token.md), [Identity trust-none ADR](identity-trust-none.md), [PR contribution guidelines](pr-contribution-guidelines.md)

---

## 1. Decision Summary

OpenAB will treat conversational voice and executable agent work as separate
planes joined by a mediated action boundary:

1. A pluggable voice engine listens and speaks. It may be a composed
   STT–dialogue–TTS pipeline, a public realtime speech model, or a future
   GPT-Live API provider.
2. Voice engines may propose a small set of semantic actions, but they never
   receive direct access to shell, repository, Discord, Kubernetes, or ACP
   sessions.
3. An OpenAB `ActionBroker` authenticates the attributed Discord speaker,
   validates the exact voice-session generation, applies effect and approval
   policy, records an audit event, and creates an asynchronous action job.
4. An `AcpActionExecutor` runs accepted work through a fresh, action-specific
   ACP connection to the existing coding CLI. Claude Code, Codex, or any other
   configured ACP implementation remains the component that edits files, runs
   programs, and uses its normal coding tools.
5. The pinned Discord text channel is the canonical action audit and approval
   surface. Action proposals and results may be spoken only after their text is
   delivered. Ephemeral, non-authoritative dialogue audio may remain realtime
   with best-effort captions.

This preserves the important property that changing the speech provider does
not change what OpenAB is authorized to do.

This ADR is a proposal and does not change current runtime behavior. Until its
phases are implemented and separately accepted, the receive-only decisions in
[the existing Discord Voice ADR](discord-voice.md) remain authoritative.

### Relationship to the Existing Voice ADR

If this ADR is accepted and implemented, it will extend or supersede only these
future boundaries:

| Existing decision | Proposed evolution |
|---|---|
| The first version is receive-only. | Keep receive-only as the default; add opt-in playback and dialogue. |
| TTS, duplex conversation, and barge-in are non-goals. | Add them in explicit phases, with half-duplex first and full-duplex only after live soak evidence. |
| Only `/voice summary` invokes ACP. | Add speaker-attributed action proposals routed through `ActionBroker`; never dispatch raw speech directly as authority. |
| `[stt]` is required whenever Discord Voice is enabled. | Validate dependencies per selected engine; a native realtime engine may own transcription. |

The following foundations are retained unchanged: explicit join, visible
consent, a pinned control channel, one active voice session per guild, Discord
speaker attribution, bounded receive callbacks and queues, deterministic stop,
and backward-compatible opt-in defaults.

## 2. Objective and Current Evidence

The objective is not merely to add spoken replies. It is to let a human talk to
OpenAB naturally while OpenAB can still perform real, long-running coding work
through ACP, with enforceable authorization and a useful audit trail.

The implementation must support operators who choose their own STT, TTS, or
realtime provider. Full-duplex audio is therefore a provider capability, not an
assumption baked into Discord or ACP.

### Evidence Snapshot on 2026-07-12

| Area | Confirmed state | Architectural implication |
|---|---|---|
| Discord receive | One-speaker Songbird receive, speaker attribution, timestamps, and raw transcript download passed in local Kubernetes. | The Discord transport can be extended instead of replaced. |
| STT quality | The first Groq `whisper-large-v3` sample was not reliable enough for action authority; a language-hinted retry remains pending. | Transcript text is untrusted input. Speaker provenance and explicit policy must survive independently of STT accuracy. |
| Claude ACP | Discord-to-Claude ACP turns work in the local deployment and OAuth persists on its PVC. | Claude can be the first `AcpActionExecutor`; a separate Anthropic chat integration is unnecessary. |
| Outbound audio | Not implemented. | Songbird playback, TTS, queueing, and cancellation must be added explicitly. |
| ACP completion | `Dispatcher::submit` only enqueues, while the authoritative final text is local to `AdapterRouter::stream_prompt_blocks`. | A typed completion result is a prerequisite; scraping Discord messages is not acceptable. |
| Long-running actions | No action job or approval store exists. | Action lifecycle must be a first-class subsystem rather than a blocking model tool call. |
| ACP permissions | `connection.rs` currently auto-selects the most permissive `allow_always`, then `allow_once`, for `session/request_permission`. | This is a release blocker for voice-triggered actions. Prompt instructions cannot compensate for it. |
| GPT-Live API | GPT-Live is available in ChatGPT, while OpenAI says API availability is coming later. | GPT-Live is a future engine target, not an implementation dependency. |

The local runtime evidence above is intentionally narrow. It does not establish
two-speaker accuracy, outbound playback, interruption, reconnect safety, or
production readiness.

### Confirmed Repository Seams

| File | Current responsibility | Required implementation seam |
|---|---|---|
| `crates/openab-core/src/discord_voice.rs` | PCM segmentation and bounded transcript primitives | Keep transport-neutral audio primitives; add explicit audio-format metadata/conversion contracts. |
| `crates/openab-core/src/discord_voice_runtime.rs` | Songbird receive, STT worker, session token, per-guild lifecycle | Extract transport/coordinator/engine responsibilities while preserving current receive behavior. |
| `crates/openab-core/src/discord.rs` | `/voice` commands, Discord interactions, adapter wiring | Add playback status plus action proposal/approval component routing; keep approvals in the pinned text channel. |
| `crates/openab-core/src/adapter.rs` | ACP event accumulation, authoritative final answer, platform delivery | Extract `AcpTurnDriver` and typed execution/output/delivery completion without changing legacy delivery behavior. |
| `crates/openab-core/src/dispatch.rs` | Bounded per-thread/per-lane text batching | Continue normal text dispatch through the extracted turn driver; voice actions do not become synthetic chat messages. |
| `crates/openab-core/src/acp/connection.rs` | ACP process, JSON-RPC reader/writer, automatic permission response | Forward non-blocking permission challenges, accept a connection-fixed resolver/profile, and expose targeted turn cancellation. |
| `crates/openab-core/src/acp/pool.rs` | Persistent text session mapping, resume, eviction, current-turn cancel | Remain the normal text owner. Action execution uses an ephemeral connection factory outside this pool. |
| `crates/openab-core/src/config.rs` | Discord Voice and agent configuration | Add opt-in dialogue/playback/action config and engine capability validation with old defaults unchanged. |
| `src/main.rs` | Adapter/manager construction and current unconditional Voice STT validation | Construct engine/broker/executor dependencies and validate STT/TTS/realtime requirements per selected engine. |
| `Dockerfile.unified`, Helm, and Claude docs | Agent packaging, auth persistence, local deployment | Add any playback/runtime dependencies once, preserve pinned builder discipline, and resolve `claude` versus `claude-agent-acp` before relying on the command. |

## 3. Terms and Decision Drivers

### Terms

- **Duplex:** the session accepts audio input and can produce audio output. It
  does not necessarily do both simultaneously.
- **Half-duplex:** capture is suppressed or ignored while bot audio is playing.
- **Full-duplex:** listening and speaking may overlap, with explicit echo,
  interruption, and cancellation semantics.
- **Dialogue turn:** a low-latency conversational exchange that does not by
  itself authorize side effects.
- **Ephemeral dialogue audio:** low-latency acknowledgement or conversation that
  carries no approval, action result, or durable authority. Captions may arrive
  later or be best-effort for a native realtime engine.
- **Action speech:** a concise projection of a canonical action proposal,
  progress update, or result. Its text must be delivered first; if exact speech
  cannot be guaranteed, use external TTS or do not speak it.
- **Action:** durable or tool-backed work delegated to an ACP agent, such as
  reading a repository, editing files, running tests, or publishing a change.
- **Approval:** an authenticated Discord text interaction bound to one action
  ID, operator ID, scope, nonce, and expiry. Spoken words are not approval.

### Drivers

1. **Actually perform work.** A natural voice interface that cannot reach the
   coding agent is insufficient.
2. **Provider portability.** Operators may use their own STT/TTS combination,
   OpenAI Realtime, or a later GPT-Live API.
3. **Speaker provenance.** Authorization derives from Discord identity mapped
   from the voice transport, never from a claimed name in transcription.
4. **Fail-closed actions.** Unknown speakers, stale sessions, unknown effect
   classes, expired approvals, and ambiguous ACP permissions must not run.
5. **Independent conversation and work.** The bot should be able to acknowledge
   and continue listening while a long ACP job runs.
6. **Deterministic cancellation.** Cancelling speech, a dialogue response, an
   ACP request, and an irreversible external side effect are different events.
7. **Text auditability.** Proposed actions, approvals, progress, and final
   results must exist in Discord text even when TTS or playback fails.
8. **Backward compatibility.** Omitted configuration preserves the current
   receive-only behavior and does not add provider calls, Discord permissions,
   or agent capabilities.

## 4. Prior Art and Industry Research

The repository's [PR contribution guidelines](pr-contribution-guidelines.md)
require OpenClaw and Hermes Agent research for architecture/runtime changes.
This section also examines official OpenAI, LiveKit, and Songbird behavior.
Source snapshots were checked on 2026-07-12; model availability remains
time-sensitive.

### 4.1 OpenAI GPT-Live

OpenAI's [GPT-Live announcement](https://openai.com/index/introducing-gpt-live/)
describes a full-duplex model that can listen and speak simultaneously. For
deeper reasoning or complex work, the ChatGPT product delegates to a frontier
model in the background while the live conversation continues.

That product shape strongly supports separating a foreground conversation plane
from a background action plane. It does not provide an API contract OpenAB can
implement today: the announcement says API availability is planned, and there
is no public GPT-Live model identifier, delegation protocol, or action schema to
target yet.

**Adopt:** continuous foreground conversation plus asynchronous background
delegation.

**Do not assume:** that ChatGPT's internal delegation executes OpenAB ACP jobs,
or that a future API will preserve the same product-internal interface.

### 4.2 OpenAI Realtime API

The public [Realtime API](https://platform.openai.com/docs/api-reference/realtime)
already supports audio input/output, VAD, response interruption, and function
tools. As of this decision, the official
[`gpt-realtime-2.1` model page](https://developers.openai.com/api/docs/models/gpt-realtime-2.1)
documents audio input/output and function calling.

The official [Realtime conversation guide](https://developers.openai.com/api/docs/guides/realtime-conversations)
is explicit about the execution boundary: the model emits function arguments,
the application executes custom code, and the application sends a
`function_call_output` back. Function calling is therefore a transport for an
action request, not an execution sandbox.

**Adopt:** Realtime as one voice-engine implementation with a bounded semantic
tool surface.

**Do not adopt:** direct shell, raw ACP, broad MCP, Discord, or Kubernetes tools
inside the realtime session.

### 4.3 OpenClaw Discord Voice

[OpenClaw's pinned Discord Voice documentation](https://github.com/openclaw/openclaw/blob/6bd14d9129d2bbadb856bd1cc11bb9eb620a9d99/docs/channels/discord.md)
is the closest direct precedent. It provides `stt-tts`, `agent-proxy`, and
realtime bidirectional modes. In `agent-proxy`, a realtime voice front end uses
an explicit consult tool to delegate substantive work to a routed agent session.
Voice may target an existing text-channel session, and playback remains owned by
the Discord voice layer. Its run-control design also distinguishes status,
cancel, steering, and follow-up from the initial consult.

Relevant patterns:

- separate the realtime voice model from the agent brain;
- make `agent-proxy` the action-capable default rather than giving the realtime
  model every coding tool;
- pin voice work to a chosen text/agent session;
- force or strongly prefer a consult for substantive work;
- queue exact agent answers rather than replacing speech in the middle;
- synchronize remote response cancellation with local playback state; and
- use wake-name and speaker policy as input gates, not as tool approval.

OpenClaw currently documents owner-equivalent tool access as a default for an
owner speaker. OpenAB deliberately does **not** adopt that default. An authorized
speaker may propose an action, but write or external effects still require the
independent policy and approval path in this ADR.

### 4.4 Hermes Agent

Hermes Agent is not currently a Discord Voice Channel/full-duplex precedent; its
useful prior art is executable tool design and safety. Its official
[tools documentation](https://hermes-agent.nousresearch.com/docs/user-guide/features/tools/)
describes explicit toolsets, terminal/file tools, code execution, and isolated
container backends. Its [security documentation](https://hermes-agent.nousresearch.com/docs/user-guide/security/)
adds user authorization, dangerous-command approval, timeouts that deny by
default, container isolation, credential filtering, and always-on denial rules.

The repository's existing [context-aware token ADR](context-aware-token.md)
also records Hermes patterns relevant here: capability-dependent schemas,
configuration allowlists, runtime re-checks, platform permission enforcement,
and error redaction.

**Adopt:** schema filtering plus a runtime authorization check, fail-closed
approval timeouts, credential scrubbing, and an isolated execution boundary.

**Divergence:** this ADR does not give the voice model a terminal tool. It
delegates to an existing ACP coding agent after mediation.

### 4.5 LiveKit Agents

LiveKit's [pipeline comparison](https://docs.livekit.io/agents/models/pipelines/)
distinguishes:

- a modular STT–LLM–TTS pipeline with mature tool calling and a full text audit;
- a low-latency realtime speech model with less provider flexibility and a
  weaker intermediate text trail; and
- a half-cascade using realtime understanding with separate TTS output control.

Its [tool documentation](https://docs.livekit.io/agents/logic/tools/) allows
tools to run in the background while the agent keeps talking, and its
[function-tool guidance](https://docs.livekit.io/agents/logic/tools/definition/)
distinguishes interruptible lookup work from mutations that must not be
casually cancelled and discarded.

**Adopt:** a capability-based engine contract, asynchronous action jobs, and
separate interruption semantics for read-only and mutating work.

**Do not adopt now:** adding a Python/Node voice framework beside the Rust
runtime. OpenAB already owns Discord transport through Songbird; LiveKit is
architecture prior art, not a new runtime dependency in the first cut.

### 4.6 Songbird

Songbird 0.6 already provides the Discord DAVE/RTP/Opus connection, decoded
receive events, outbound inputs, track handles, stop, and volume control. See
the [0.6 release](https://github.com/serenity-rs/songbird/releases/tag/v0.6.0),
[`Input`](https://serenity-rs.github.io/songbird/current/songbird/input/enum.Input.html),
and [`TrackHandle`](https://serenity-rs.github.io/songbird/current/songbird/tracks/struct.TrackHandle.html).

Songbird does not provide STT, TTS, dialogue policy, action authorization,
approval jobs, or application-level echo cancellation. It remains a transport
implementation, not the voice-engine or action abstraction.

### 4.7 Agent Client Protocol Cancellation

The official [ACP v1 Prompt Turn specification](https://agentclientprotocol.com/protocol/v1/prompt-turn)
defines `session/cancel` as a notification, not a request with its own response.
After sending it, the client must resolve every pending
`session/request_permission` for that turn as `cancelled`; the agent then must
finish the original `session/prompt` with `stopReason = cancelled` after aborting
its work. Late tool updates may still arrive before that final response.

**Adopt:** cancellation drains permission challenges, continues reading updates,
and treats the original prompt response as protocol confirmation. A missing
final response remains `OutcomeUnknown`, followed by process-tree termination
and workspace quarantine.

### 4.8 Comparison and Chosen Lessons

| Prior art | Media/dialogue owner | How real work runs | Lesson adopted by OpenAB |
|---|---|---|---|
| GPT-Live in ChatGPT | Native full-duplex model | Product-internal background delegation | Keep conversation responsive while work runs asynchronously; wait for a public API before implementing a provider. |
| OpenAI Realtime API | Public realtime session | Application executes function calls | Treat model tool calls as proposals delivered to `ActionBroker`. |
| OpenClaw | Discord voice layer plus realtime/pipeline mode | Explicit consult to routed agent session | Default action-capable mode is `agent-proxy`; voice and coding brain remain separate. |
| Hermes Agent | Text/messaging agent toolsets | Tool registry and isolated terminal/code backends | Double-gate tools at schema and runtime; require human approval for dangerous effects. |
| LiveKit Agents | Provider-neutral pipeline/realtime session | Background tools/tasks/workflows | Capability negotiation and asynchronous job lifecycle. |
| Songbird | Discord media transport | None | Reuse transport and playback primitives; keep policy above it. |
| ACP v1 | Prompt-turn JSON-RPC lifecycle | Client cancellation plus final prompt result | Cancel pending permission requests, keep draining updates, and use the original prompt result as protocol confirmation. |

## 5. Chosen Architecture

```text
Discord Voice Channel
        │
        │ speaker-tagged PCM / outbound PCM
        ▼
DiscordSongbirdTransport
        │
        │ VoiceInput / PlaybackEvent
        ▼
VoiceCoordinator ──────────────► canonical Discord text audit
        │
        │ normalized engine input/events
        ▼
DuplexVoiceEngine
  ├─ pipeline: streaming/batch STT + dialogue + TTS
  ├─ realtime: public speech-to-speech API
  └─ future: GPT-Live API, only after public documentation
        │
        │ ActionRequested (semantic, typed, untrusted)
        ▼
ActionBroker
  ├─ speaker/session provenance
  ├─ policy + effect classification
  ├─ Discord text approval
  ├─ idempotency + timeout + audit
  └─ ActionJobManager
        │
        │ accepted AcpTurn
        ▼
AcpActionExecutor → ephemeral action-specific ACP connection ─┐
                                                             │
normal text: Dispatcher → AdapterRouter → SessionPool ────────┤
                                                             ▼
                                                        AcpTurnDriver
                                                             │
                                                             ▼
                                                        ACP coding CLI
                                                             │
                                             TurnCompletion / ActionResult
                                                             ├─► canonical Discord text result
                                                             └─► speech projection → engine/TTS → Songbird playback
```

### 5.1 Component Responsibilities

#### `DiscordSongbirdTransport`

- owns join, leave, DAVE, SSRC-to-user mapping, decoded PCM, output tracks, and
  connection/reconnect events;
- emits attributed audio and accepts already-produced playback audio;
- does no STT, model calls, ACP work, or authorization in Songbird callbacks;
- keeps all callback-to-worker queues bounded and non-blocking.

The first implementation is an extraction/evolution of the transport portions
of `discord_voice_runtime.rs`, not a second independent Discord connection.

#### `VoiceCoordinator`

- owns the exact `VoiceSessionToken` generation and one session per guild;
- maintains connection, dialogue, action, and playback state independently;
- routes normalized events between transport, engine, broker, and text audit;
- rejects late engine, action, TTS, and playback results from stale sessions;
- owns bounded input, engine-event, action-result, TTS, and playback queues;
- applies phase-specific echo suppression and interruption rules;
- distinguishes realtime ephemeral dialogue from text-first action speech;
- snapshots the current Voice Channel audience and enforces the action-speech
  confidentiality policy before and during playback.

#### `DuplexVoiceEngine`

- owns provider-specific STT/VAD/dialogue/TTS or speech-to-speech behavior;
- declares capabilities at startup instead of relying on engine-name
  heuristics;
- receives only the minimum semantic tools configured for its mode;
- never receives Discord bot, STT/TTS provider, Kubernetes, or ACP credentials
  belonging to another component;
- never accesses `SessionPool` or spawns a coding CLI directly.

#### `ActionBroker`

- authenticates the Discord speaker and exact session generation;
- validates typed action payloads and effect hints, then derives the effect
  ceiling enforced by approval and runtime permission checks;
- creates idempotent jobs with deadlines and one terminal outcome;
- requires the canonical proposal/audit message to be delivered before any
  read-only or mutating execution may enter the queue;
- requests and resolves Discord text approval when required;
- records proposal, approval, start, progress, cancellation, and result events;
- is the only route from a voice engine to an action executor.

#### `AcpActionExecutor`

- translates one accepted typed `AcpTurn` into the reusable ACP turn driver;
- creates a broker-minted, ephemeral execution connection that is never loaded,
  resumed, or registered as the pinned text session;
- fixes the agent command, safe config options, environment, workspace,
  permission resolver, and containment profile before the child is spawned;
- injects only the operator-confirmed, text-visible goal and explicitly approved
  context references. The raw/ambient transcript is audit data, not an ACP
  instruction. Current OpenAB has no safe seam for copying an ACP
  agent's hidden text-session history, so v1 does not claim to share it;
- supplies a voice-action-specific permission policy to ACP;
- returns typed progress and a final `TurnCompletion`;
- terminates the child, confirms quiescence, and only then records a confirmed
  terminal outcome and removes ephemeral state; otherwise it records unknown
  outcome and quarantines the workspace;
- never exposes agent credentials back to the voice engine.

`AcpTurnDriver` is extracted from the current
`AdapterRouter::stream_prompt_blocks` finalization loop. Normal text continues
to obtain a pooled connection through `SessionPool`; action execution supplies
its own ephemeral connection. Both reuse the same ACP event handling, final
answer selection, adapter delivery, and typed completion semantics.

## 6. Provider Capability Model

Engine selection and behavior must be based on declared capabilities:

```rust
struct VoiceEngineCapabilities {
    streaming_input: bool,
    partial_transcripts: bool,
    streaming_output: bool,
    interrupt_output: bool,
    semantic_tool_calls: bool,
    continuous_duplex: bool,
    exact_speech_output: bool,
    accepted_input_formats: Vec<AudioFormat>,
    output_formats: Vec<AudioFormat>,
}

struct AudioFormat {
    sample_rate_hz: u32,
    channels: u16,
    sample_type: SampleType,
    codec: AudioCodec,
}

struct AudioChunk {
    format: AudioFormat,
    media_timestamp: Duration,
    frame_count: u32,
    data: Bytes,
}
```

The final Rust shape may differ, but these distinctions are required. A batch
STT plus batch TTS provider is a valid half-duplex engine; it must not be labeled
full-duplex. If an operator requests a capability the selected engine lacks,
startup either fails with an actionable message or explicitly downgrades to a
named mode. It must never silently claim the stronger mode.

Initial engine families:

| Engine family | Input/output path | Expected mode | Action routing |
|---|---|---|---|
| `pipeline` | Existing or streaming STT → dialogue/router → TTS | Half-duplex first; streaming overlap later | `agent-proxy` |
| `realtime` | Public realtime speech-to-speech provider | Provider-dependent duplex | `agent-proxy` by default; dialogue-only direct mode optional |
| `gpt-live` | Future public API | Unknown until API contract exists | Not implemented until official API documentation is available |

`agent-proxy` means the voice engine handles turn-taking and concise speech,
while substantive answers and all executable work come from the routed ACP
agent. A `direct` dialogue mode may answer harmless conversational questions but
must have actions disabled or still route every action through `ActionBroker`.

Audio is never passed as untyped bytes. Current Songbird receive is annotated as
48 kHz stereo decoded PCM; providers may require 16/24/48 kHz mono PCM or an
encoded stream. The coordinator owns an `AudioConverter` that negotiates one of
the engine's declared formats and performs resampling/channel remixing in a
bounded worker, never in the 20 ms Songbird callback. The playback sink likewise
declares its accepted format, and conversion preserves media timestamps and
frame counts so interruption and queue timing remain testable.

## 7. Normalized Contracts

The following types express required invariants rather than a frozen API:

```rust
#[async_trait]
trait DuplexVoiceEngine: Send + Sync {
    fn capabilities(&self) -> VoiceEngineCapabilities;
    async fn start_session(&self, ctx: VoiceEngineContext)
        -> Result<VoiceEngineSession>;
}

enum VoiceEngineEvent {
    PartialTranscript(EngineTranscript),
    FinalTranscript(EngineTranscript),
    EphemeralDialogueText(String),
    EphemeralDialogueAudio(AudioChunk),
    ActionRequested(ActionProposal),
    Health(EngineHealth),
}

struct ActionProposal {
    provider_call_id: String,
    source_turn: VoiceInputTurnToken,
    effect_hint: EffectClass,
    action: ActionKind,
}

struct ActionRequest {
    action_id: ActionId,
    voice_session: VoiceSessionToken,
    speaker: AttributedDiscordSpeaker,
    transcript_span: TranscriptSpan,
    control_channel: ChannelRef,
    context_refs: Vec<ContextRef>,
    target_workspace: WorkspaceRef,
    expires_at_utc: DateTime<Utc>,
    runtime_deadline: Instant,
    effect_ceiling: EffectClass,
    action: ActionKind,
}

enum ActionKind {
    AcpTurn(AcpTurnRequest),
}
```

The initial action contract intentionally has no arbitrary JSON action and no
raw shell action.
`AcpTurnRequest` contains an operator-confirmed, text-visible goal and bounded
explicit references. The broker constructs the final ACP content block; the
voice model does not provide invisible system instructions. The attributed
source transcript span is retained for audit but is not automatically injected
into ACP.

`ActionProposal` is provider output and is untrusted. The coordinator, not the
engine, constructs `ActionRequest`: it resolves the opaque `source_turn` against
the active voice-session generation and copies the Discord speaker, transcript
span, channel, explicit context references, and workspace from OpenAB-owned
state. A provider may
not supply or override those authority fields. `effect_hint` is only a
provider claim; the broker derives the required effect from the typed action and
the permission layer checks every actual ACP request again. Understated or
unclassifiable effects fail closed.

The broker also mints the global `ActionId`. `provider_call_id` is only a
provider-local replay key scoped by engine session and input-turn token. A
repeated scoped call returns the same broker-minted action ID; an untrusted
provider cannot choose, collide with, or probe another job's global identifier.

The only semantic tools exposed to a realtime voice model are equivalent to:

- `start_agent_task(goal, effect_hint)`;
- `get_agent_task(action_id)`; and
- `cancel_agent_task(action_id)`.

There is no `approve_agent_task` voice tool. Approval is an independent Discord
text interaction and cannot be granted by generated speech, transcribed speech,
or another model tool call.

Native realtime audio events are ephemeral dialogue unless OpenAB has an exact,
canonical text representation. Action proposals, progress, and results do not
reuse arbitrary provider audio. After text delivery, the coordinator may ask an
engine with `exact_speech_output` to speak the bounded `speech_text`; otherwise
it uses a configured external TTS or remains silent.

### 7.1 Action Job Lifecycle

```text
Proposed → AuditPending → AwaitingApproval → Queued → Running → Succeeded
                │                  │                     │
                ├─ delivery fail → Failed                ├─ proven no-effect error → Failed
                ├─ policy deny ──→ Denied                ├─ no final proof ─→ OutcomeUnknown
                │                  ├─ deny ──────────────→ Denied
                │                  └─ expiry ────────────→ Expired
                │
                └─ every executable path requires proposal delivery + approval

Running → CancelRequested → Terminating
                              ├─ confirmed cancelled → Cancelled
                              ├─ completed first ────→ Succeeded
                              └─ cannot prove ───────→ OutcomeUnknown

AwaitingApproval | Queued → Cancelled (no executor side effect)
```

Every transition is idempotent. Only one terminal state may be recorded. A
repeated provider function call with the same scoped provider-call key returns
the existing broker-minted `ActionId` instead of running work twice. Status and
cancel calls re-check the engine session, attributed action owner, control
channel, workspace/execution profile, and operator authority for that ID.
Possessing or guessing an ID alone never grants visibility or cancellation
rights.

`AuditPending` is a hard gate for every action. If OpenAB cannot post
the canonical proposal to the control channel, no executor is invoked. Result
delivery is different: execution may already have succeeded, so a later
delivery failure is recorded truthfully rather than rewriting history.

In v1 every provider-generated or provider-rephrased goal enters
`AwaitingApproval`, including read-only work. The attributed source-turn token
proves which Discord stream triggered the proposal; it does not prove that STT
or the voice model described the operator's intent faithfully. Hands-free
read-only is reserved for a later, explicit high-risk mode using a deterministic
exact attributed transcript goal, operator-only input, and status-only speech.

`expires_at_utc` is the persisted/audited expiry. `runtime_deadline` is rebuilt
as a monotonic deadline for the current process. Restart never extends the
wall-clock expiry; already-expired jobs fail closed.

Cancellation is best-effort and state-specific:

- queued or awaiting-approval work is cancelled without execution;
- an active ACP prompt may send the current protocol's `session/cancel`
  notification and record only that the request was sent; there is no protocol
  acknowledgement on that notification;
- all pending permission challenges for that turn are immediately resolved with
  the ACP `cancelled` outcome while the reader continues accepting late updates;
- terminal state comes from the original prompt response, connection failure,
  or deadline, and a completion race remains truthfully `Succeeded`;
- already-completed filesystem or external mutations are not described as
  rolled back;
- TTS/playback cancellation does not imply action cancellation, or vice versa.

Once an ACP prompt starts, EOF, timeout, unconfirmed cancellation, or worker loss
cannot prove that no filesystem/tool effect occurred. Such jobs finish as
`OutcomeUnknown`, include the observed tool/effect evidence, and require
workspace reconciliation before another write job. They are not mislabeled
`Failed`, `TimedOut`, or `Cancelled` merely because a final response is missing.

Ephemeral cleanup has a `Terminating` phase: request cancellation, wait a bounded
grace period, terminate the backend-specific process tree, and confirm child
exit/quiescence before recording a confirmed terminal outcome. If exit cannot
be confirmed, keep or quarantine the workspace lease and record
`OutcomeUnknown`; do not release the workspace for another mutating job.

Execution state and delivery state are recorded separately. If an ACP turn
successfully edits the workspace but Discord delivery fails, the job remains
`Succeeded` with `DeliveryFailed`; it must not be mislabeled as an execution
failure or spoken without the canonical text audit.

## 8. Session, Ordering, and Provenance

1. One `VoiceSessionToken` identifies a specific generation, not merely a guild
   or channel. Replacement, stop, expiry, or reconnect that creates a new
   generation invalidates old engine and speech results.
2. Only audio attributed by Songbird's SSRC-to-Discord-user mapping may carry an
   authenticated `speaker_id`. A provider-side mixed transcript is
   `unattributed` even if it contains a name.
3. Only a configured operator speaker may propose an action automatically.
   Other participants remain transcript/dialogue-only. Unattributed speech
   cannot start work and may instead generate a text confirmation request.
4. The pinned control channel identifies the audit destination and requested
   workspace/context route. V1 does not reuse that channel's long-lived ACP
   connection or hidden agent history; it injects only bounded, explicit context
   into a new action connection.
5. The broker mints a unique execution key and creates an ephemeral ACP child for
   one action job. It does not use `Dispatcher::session_key`, write a normal
   session mapping, load/resume prior state, or remain alive after a terminal
   outcome.
6. The action queue permits at most one running job per target workspace. The
   local spike uses a dedicated disposable workspace not concurrently touched by
   a normal text ACP session. Public beta requires either a workspace lease
   honored by every execution path or a separate worktree/volume per action.
7. When a voice session closes, an already-running action may finish and post its
   canonical text result. It must not speak into a new or absent session.
8. The exact attributed transcript span used to propose the action is included
   in the audit record, subject to retention/redaction limits.

An action-capable native realtime session must preserve the provider event's
association with an OpenAB-minted input-turn token. If the provider accepts only
one mixed audio stream or cannot associate a tool call with a specific
attributed turn, automatic actions are disabled. An implementation may instead
feed only the authorized operator's stream into the action-capable session,
while keeping other speakers transcript/dialogue-only, but it must not infer
authority after mixing.

Speech from other participants is never action instruction in v1. If the
operator wants a participant's statement used as evidence, the exact attributed
span is shown in text and explicitly selected/confirmed as a context reference.
It remains delimited untrusted data, and adding it makes the action
approval-required regardless of effect class.

Speaker authorization permits the attributed Discord account/audio endpoint to
propose work. It does not prove the physical speaker behind that endpoint, and
it does not permit the STT output or voice model to bypass tool approval.

## 9. Typed ACP Completion

Outbound speech requires the authoritative final agent answer. It must not
intercept `ChatAdapter::send_message`, because that stream also contains
placeholders, edits, tool narration, split chunks, and errors.

Phase zero introduces a typed result around the existing finalization seam:

```rust
struct TurnOutput {
    display_text: String,
    speech_text: String,
}

struct TurnCompletion {
    execution: ExecutionOutcome,
    output: Option<TurnOutput>,
    delivery: DeliveryOutcome,
    legacy_dispatch: LegacyDispatchDisposition,
}

enum ExecutionOutcome {
    Succeeded { turn_result: TurnResult },
    Failed { error: RedactedTurnError, no_effects: NoEffectsProof },
    Cancelled { observed_effects: Vec<EffectEvidence> },
    OutcomeUnknown { reason: UnknownReason, observed_effects: Vec<EffectEvidence> },
}
```

- `display_text` is the existing finalized, directive-applied content used for
  platform delivery.
- `speech_text` is derived from the directive-stripped final answer before
  Discord Markdown conversion and tool-line decoration. It is bounded again by
  the speech policy and may be a concise summary rather than the whole output.
- `execution` retains structured ACP stop/error/token metadata and never infers
  success from delivery or partial text.
- `output` may contain redacted partial/final display text even when execution
  failed or became unknown; only a succeeded outcome has speakable action text.
- `TurnCompletion::delivery` reports canonical platform delivery independently
  from execution.
- `legacy_dispatch` records the success/error disposition the pre-refactor text
  path would have returned for that exact branch.

`DispatchTarget::stream_prompt_blocks` and
`AdapterRouter::stream_prompt_blocks` then return a typed completion result. An
ACP turn that produced a result but failed Discord delivery returns a completion
containing `DeliveryFailed`. Outer `TurnRunError` is restricted to spawn,
initialize, session setup, serialization, or write failure before any
`session/prompt` bytes are flushed to the child.

After the prompt is flushed, an error response, EOF, timeout, missing text, or
connection loss does not prove that effects are absent. It returns
`OutcomeUnknown` unless the containment backend supplies an explicit
`NoEffectsProof`; only then may it be `Failed`. A final error string by itself is
not such proof, and an unknown outcome quarantines the workspace.

The normal text dispatcher maps only `legacy_dispatch` back to its result, so
current reactions, warning/fallback messages, and branches that historically
returned `Ok(())` remain unchanged. It must not reinterpret every typed
execution failure as a new dispatcher error. `AcpActionExecutor` instead
inspects execution and delivery: it may record execution `Succeeded` plus
`DeliveryFailed`, while EOF/timeout with delivered partial text remains
`OutcomeUnknown`. It never releases action speech unless execution is
`Succeeded` and delivery is `Delivered`.

Phase zero extracts the ACP event/finalization portion into `AcpTurnDriver` (the
final name may differ). It accepts an already-selected connection plus delivery
context and resolves `TurnCompletion` exactly once on completion, cancellation,
delivery failure, EOF, or timeout. It does not own pooling or session identity.

The normal dispatcher keeps its existing `BufferedMessage`, batching, and
`SessionPool::with_connection` path, then calls the driver. The action executor
does not enqueue a synthetic chat message: it creates an ephemeral connection
with its action profile and calls the same driver directly. This avoids both
scraping platform output and accidentally inheriting dispatcher grouping or a
long-lived session's permission state.

The driver returns a typed `TurnTicket` or request ID only after the action owns
an active prompt. Cancellation uses that ticket so a queued job cannot cancel a
different turn. It must not hold dispatcher, pool, connection-map, or job-state
locks while awaiting ACP. `speech_text` becomes eligible for action playback
only after canonical result delivery succeeds.

## 10. Permission Mediation Is a Blocking Prerequisite

Today `acp/connection.rs` handles `session/request_permission` inside the reader
loop and selects `allow_always` before `allow_once`; absent options also fall
back to `allow_always`. Existing tests preserve that behavior. Therefore an
`ActionBroker` layered only above `Dispatcher` would not be a technical safety
boundary: once ACP starts, the child can receive broad approval without the
operator.

Before voice actions ship, ACP permission handling must emit a challenge to a
per-session or per-turn resolver:

```rust
#[async_trait]
trait PermissionResolver: Send + Sync {
    async fn resolve(&self, request: AcpPermissionRequest)
        -> PermissionDecision;
}

enum PermissionDecision {
    SelectOnce { option_id: String, expected_kind: OnceKind },
    Cancel,
}

enum OnceKind {
    AllowOnce,
    RejectOnce,
}
```

The ACP reader loop must continue draining the child's stdout. It publishes a
`PermissionChallenge` containing the request/tool-call IDs, selectable options,
and a one-shot responder. A separate task waits for policy, Discord approval,
timeout, or cancellation and writes the answer through the connection's single
writer. The reader loop must not block while a human decides.

The voice action resolver follows these rules:

1. `SelectOnce` validates that the option ID belongs to that exact challenge and
   has the expected `allow_once` or `reject_once` kind. No `*_always` option is
   ever selected for a voice action.
2. An explicit action grant carries an effect ceiling:
   `Inspect`, `WorkspaceWrite`, or `ExternalSideEffect`.
3. Each actual ACP permission request is classified again at runtime. It is
   allowed once only when it fits the grant; unknown or broader requests fail
   closed and cancel the turn. V1 does not escalate a running job in place; the
   operator must create and approve a new action with a broader effect ceiling.
4. V1 read-only inspection still requires text confirmation of the proposed
   goal. After confirmation, matching `allow_once` inspection requests may be
   resolved automatically within that one job's grant.
5. Workspace writes require an explicit text approval in the first public
   phase. External side effects, including push, deployment, posting, billing,
   credential changes, or infrastructure mutation, require a separately named
   approval and may remain entirely disabled.
6. Approval UI binds operator ID, action ID, context references, target
   workspace, execution profile, effect ceiling, nonce, and TTL. Only a
   configured operator Discord account may resolve it.
7. The model cannot alter the grant, approve itself, or persist an approval.
8. Unknown agents or ACP permission formats are denied for voice action mode.
9. Agent modes that suppress permission requests, such as bypass/trust-all
   profiles, are rejected at startup when approval-required voice actions are
   enabled; the absence of a challenge must not be described as approval.
10. Human denial selects the challenge's `reject_once` when available. Approval
    timeout, action cancellation, or a challenge without a safe once option uses
    the ACP `cancelled` outcome.

For backward compatibility, the legacy text path may retain its current policy
in the first permission-refactor PR. It must never share a connection with a
voice action: an earlier text turn may already have selected `allow_always`, in
which case the agent might not emit another permission request. A per-turn
resolver cannot revoke state the agent already cached.

Every v1 action therefore starts a fresh ACP process/session under an
`ActionConnectionProfile` that is part of the connection identity and fixed
before spawn. The profile includes agent command, safe config options, a
dedicated settings/home view where required, explicit environment, workspace,
containment mode, and the fail-closed resolver. The connection is not pooled,
persisted, resumed, or reused after the job. Agent-side settings that suppress
permission challenges are rejected; if an agent cannot provide that guarantee,
it is unsupported for voice actions. A later ADR may tighten the global text
default or prove a safe reusable action-session design.

This mediated voice boundary is a deliberate exception to the proposed
[context-aware token ADR](context-aware-token.md), which chose direct agent token
access for trusted platform operations. Voice combines untrusted ambient speech,
imperfect STT, multiple participants, and a realtime model, so direct credentials
or behavioral-only restrictions are not acceptable here.

## 11. Audio, Lifecycle, and Backpressure

Connection, dialogue, action, and playback are separate state machines. For
example, playback failure must not mark an already-delivered ACP action result as
failed, and an ACP timeout must not disconnect Discord receive.

A native realtime engine may stream ephemeral dialogue audio immediately and
post its caption later on a best-effort basis. That audio cannot announce an
approval, claim an action succeeded, or serve as the audit record. Action speech
uses canonical `speech_text` only after text delivery; an engine without exact
speech output must use external TTS or skip action playback.

Required queue boundaries:

- decoded/segmented input to STT or realtime engine;
- normalized engine events;
- proposed action jobs;
- action progress/results;
- TTS requests/chunks; and
- Songbird playback items.

Every queue has a configured bound, drop/reject policy, metric, and user-visible
degradation path. No network, disk, model, or ACP work runs inside a Songbird
callback or while holding a `std::sync::Mutex`.

### Phased Audio Behavior

1. **Playback smoke:** play a generated tone or fixed local sample, then stop it
   deterministically.
2. **Half-duplex pipeline:** suppress capture/STT while bot playback is active,
   then add a short post-playback guard interval. This avoids feeding the bot's
   own voice back into batch STT.
3. **Streaming playback:** use a bounded producer/consumer buffer and stop the
   track on stale session or cancellation.
4. **Barge-in:** only for engines with `interrupt_output`. User speech cancels
   the remote response, truncates provider conversation state when required,
   and clears local queued audio. Thresholds prevent a brief echo from
   cancelling every response.
5. **Full-duplex:** enabled only after live echo/interruption testing. Songbird
   transport alone does not establish echo cancellation.

Stop, expiry, replacement, or fatal reconnect must cancel engine tasks, pending
TTS, and active tracks, close senders, drain or reject bounded work according to
policy, and make late results harmless through the session token.

## 12. Configuration and Backward Compatibility

Exact names will be finalized with implementation and documented in
`docs/config-reference.md`. The intended shape is:

```toml
[discord.voice]
enabled = true                    # Existing receive capability.

[discord.voice.dialogue]
enabled = false                   # New; omitted/false preserves receive-only.
engine = "pipeline"               # pipeline | realtime | future gpt-live
mode = "agent-proxy"              # Agent handles substantive work.
full_duplex = false               # Never inferred from engine name.

[discord.voice.playback]
enabled = false                   # Requires Discord Speak permission.
max_pending_items = 4
action_detail = "status-only"     # Default: do not read private results aloud.

[discord.voice.actions]
enabled = false                   # Separate from dialogue/playback.
operator_users = []
auto_allow_read_only = false       # Reserved; v1 rejects true.
allow_workspace_write = false
allow_external_side_effects = false
approval_timeout_seconds = 60
max_running_jobs = 1
execution_backend = "local-disposable" # Spike only; production requires containment.
```

Provider credentials remain environment references resolved by OpenAB/Kubernetes
Secrets. They are not serialized into provider instructions, action requests,
logs, or ACP child environments. The existing ACP `env_clear()` invariant
remains mandatory: only `HOME`, `PATH`, platform-required user/system variables,
and explicit `[agent].env` keys reach the child. Discord, STT, TTS, and realtime
provider credentials must not be added to `[agent].env`.

Startup validation becomes engine-specific:

- receive-only and `pipeline` with external STT require valid STT config;
- a realtime engine that owns input transcription does not require `[stt]`;
- playback startup validates the selected engine's output/TTS config. Discord
  `Connect`/`Speak` are channel-specific and are checked against the resolved
  Voice Channel at join/playback time; revocation stops playback and reports the
  runtime failure;
- action mode requires an operator allowlist and a non-legacy permission
  resolver;
- v1 rejects `auto_allow_read_only = true`; every voice-generated goal requires
  authenticated text confirmation;
- action mode requires an ephemeral `ActionConnectionProfile`; it cannot reuse
  a pooled text session or a bypass/trust-all agent configuration;
- `local-disposable` execution is rejected outside an explicit development
  profile; public beta requires a reviewed containment backend;
- unknown engine names or impossible capability requests fail with actionable
  errors.

All new booleans default to `false`. Enabling current receive must not silently
enable dialogue, playback, or actions.

## 13. Security, Privacy, and Audit

### Trust Boundaries

- Discord user ID from attributed transport audio is identity input.
- Raw audio, STT text, participant speech, realtime-model output, and ACP output
  are untrusted data.
- `ActionBroker` policy and authenticated Discord interactions are authority.
- Execution containment is deployment- and agent-specific. `working_dir` is a
  current directory, and `env_clear()` is environment minimization; neither is
  a filesystem, process, credential, or network sandbox.

### Required Controls

1. Show an audible or text-visible notice when capture, external STT/realtime
   processing, agent execution, or playback begins.
2. Do not store raw audio by default. Keep OpenAB-local transcript and job audit
   retention bounded and configurable. Canonical messages posted to the control
   channel follow the Discord server's own retention and access policy; OpenAB
   cannot promise to delete those copies.
3. Do not log raw audio, full transcripts, provider response payloads, secrets,
   OAuth codes, or approval nonces.
4. Redact provider and ACP errors before displaying them to the voice engine or
   Discord.
5. Keep the voice engine's semantic tool schema minimal and deployment-scoped.
6. Bind every action and approval to speaker, guild, voice session generation,
   control channel, explicit context references, target workspace, execution
   profile, and deadline.
7. Deliver proposal and result text before speaking. A spoken response must not
   be the only audit record.
8. Never interpret the words "approve", "yes", or an operator's spoken token as
   permission for a side effect.
9. Run voice-action ACP work in a dedicated, least-privilege environment before
   public beta: a disposable or explicitly scoped workspace/PVC, no unrelated
   mounts, no platform/provider secrets, a Kubernetes service account without
   mutation privileges, and restricted egress appropriate to the task.
10. Treat every current Voice Channel listener as an action-speech recipient.
    The default `status-only` policy says only that details were posted to text.
    Detailed output may be spoken only when every listener other than this
    OpenAB bot—including other bots that may record audio—is explicitly trusted
    and can view the pinned text channel. A join, move, permission change, or
    unknown listener during playback cancels/downgrades detailed speech.
    Redaction and length limits still apply.
11. Do not inject ambient or non-operator transcript into ACP. V1 executes only
    the text-confirmed goal; additional transcript evidence requires explicit
    operator selection and remains delimited untrusted data.

`env_clear()` remains required defense in depth, but the current same-container
ACP child is trusted code with the pod user's filesystem/process reach and may
be able to inspect parent process state or shared mounts. It must not be
described as a secret boundary. The local spike is acceptable only with a
disposable repository and no valuable external credentials, and it validates
functionality rather than containment.

Production action mode is blocked until the executor runs in a separate worker
pod or equivalently isolated runtime with a distinct process/user boundary,
only its scoped workspace and agent authentication, no Discord/STT/TTS/realtime
Secret mounts, no Kubernetes API credential, and reviewed egress. The broker
sends that worker only the typed, approved job and receives typed progress/
completion; provider/platform credentials remain in the owning OpenAB process.

The current `/voice summary` prompt guard is not a sandbox. Until permission
mediation exists, transcript summary remains tool-capable and must not be reused
as the implementation of automatic action turns.

## 14. Implementation Plan and Issue Slices

Each phase is a separate logical PR. Later phases depend on the acceptance of
earlier safety boundaries.

### Phase 0 — Typed ACP Turn Driver and Completion

**Goal:** expose the authoritative final result without changing voice behavior.

- add `TurnOutput`, `TurnCompletion`, execution outcome, and delivery outcome;
- extract an `AcpTurnDriver` seam from
  `AdapterRouter::stream_prompt_blocks` while keeping normal calls through
  `DispatchTarget` and `SessionPool`;
- give active turns typed tickets for targeted cancellation;
- map typed failures back to the current normal-dispatch error behavior;
- preserve all existing platform delivery and batching behavior;
- test exactly-once completion and explicit execution/delivery outcomes on
  success, partial output, ACP error, delivery error, cancellation, timeout,
  EOF, and dead consumer.

### Phase 1 — ACP Permission Resolver

**Goal:** create an enforceable, per-turn permission boundary.

- replace unconditional permission auto-response with a resolver interface;
- implement a legacy resolver for backward-compatible text behavior;
- implement a fail-closed voice-action resolver with `AllowOnce` only;
- forward challenges without blocking the ACP reader loop;
- classify actual ACP requests against an effect ceiling;
- test unknown formats, timeout, stale approval, escalation, and denial.

No voice action ships in this phase.

### Phase 2 — ActionBroker and ActionJobManager

**Goal:** prove asynchronous lifecycle and Discord authorization with a fake
executor before any coding CLI can mutate state.

- implement typed `AcpTurn` proposal, provenance, idempotency, state machine,
  timeout, progress, cancel, and audit;
- mint action IDs in the broker and scope provider replay keys;
- make successful proposal/audit delivery a pre-execution gate;
- add Discord button/modal approval bound to the configured operator;
- use a fake executor to test denial, expiry, cancellation, delivery failure,
  restart, and terminal idempotency with zero side effects.

The first local spike may keep active jobs in memory only because its entire
workspace is disposable and is discarded/recreated after an OpenAB or worker
restart before another job can run. A retained workspace cannot be made safe by
forgetting the in-memory job.

Before public beta on any retained workspace, the broker durably writes the job
and workspace-lease marker before spawning the child. Restart reconciliation
marks an unfinished lease `OutcomeUnknown`, quarantines the workspace, and
requires review before another write.

### Phase 3 — Ephemeral ACP Action Executor and Containment

**Goal:** perform real coding work in a disposable local Kubernetes workspace
without inheriting the text session's permission state.

- add `ActionConnectionProfile` and an ephemeral connection factory outside the
  normal session mapping/resume lifecycle;
- fix safe agent options and the permission resolver before process spawn;
- apply and verify every security-relevant config option fail-closed; do not
  reuse `SessionPool::get_or_create` behavior that only warns when a default
  config option cannot be applied;
- run one action per fresh ACP process/session and destroy it at terminal state;
- call `AcpTurnDriver` directly, return typed progress/completion, and use a
  `TurnTicket` for targeted cancellation;
- terminate the process tree with bounded grace, confirm quiescence before
  releasing the workspace, and quarantine it on `OutcomeUnknown`;
- inject only the operator-confirmed, text-visible goal and explicitly approved
  context references, not raw ambient transcript or hidden text-session history;
- use one dedicated disposable workspace, `max_running_jobs = 1`, no valuable
  external credentials, and explicit local-development configuration;
- smoke test `initialize`, `session/new`, and `session/prompt` with
  `claude-agent-acp` until the repository's `claude` versus `claude-agent-acp`
  command documentation is reconciled;
- implement and review the separate production action-worker boundary described
  in section 13 before public beta; the same-container executor remains a
  development-only functional spike.

### Phase 4 — Songbird Outbound Playback

**Goal:** validate Discord output independently of a model.

- request/document Discord `Speak` permission;
- add a bounded playback queue and track cancellation;
- play a local tone/sample in `docker-desktop/openab-local`;
- test stop, replacement, disconnect, reconnect, stale items, and queue overflow.

### Phase 5 — Pipeline `agent-proxy`

**Goal:** deliver the first useful bidirectional implementation with provider
choice and an ACP coding brain.

- add pipeline engine contracts around current STT plus configurable TTS;
- route operator action intent to `ActionBroker`;
- synthesize only the concise `speech_text` after text delivery;
- use half-duplex capture suppression while playback runs;
- exercise both read-only and approved workspace-write jobs through the
  ephemeral Claude ACP executor.

### Phase 6 — Public Realtime Engine

**Goal:** prove that the abstraction is genuinely provider-pluggable.

- implement one public realtime API behind `DuplexVoiceEngine`;
- expose only start/status/cancel semantic action tools;
- preserve attributed-speaker provenance outside provider transcript claims;
- treat native audio as ephemeral dialogue; use exact speech or external TTS
  for text-first action results;
- synchronize response cancellation with local playback;
- compare latency, interruption, audit completeness, and cost with pipeline mode.

### Phase 7 — GPT-Live Provider

**Goal:** implement only after OpenAI publishes a stable API contract.

- re-check official model availability, modalities, tool/delegation semantics,
  interruption, retention, and pricing;
- map the public contract to existing engine/action interfaces;
- do not bypass `ActionBroker` even if the provider offers built-in delegation.

### Phase 8 — Full-Duplex and Barge-In

**Goal:** enable simultaneous listen/speak only after controlled live evidence.

- add echo/interruption thresholds and provider-state truncation;
- test headphones, speakers, two human speakers, noise, and rapid interruption;
- run a minimum 30-minute reconnect/soak session;
- retain a configuration switch back to half-duplex.

## 15. Local Kubernetes Validation

All first implementation phases are validated in the local cluster only:

- Kubernetes context: `docker-desktop`;
- namespace: `openab-local`;
- a disposable repository/PVC for write tests;
- no push, deploy, billing, or other external mutation credentials in the agent;
- a least-privilege Kubernetes service account and no unrelated mounted data;
- restricted egress unless the specific approved test requires it;
- Secrets mounted or injected only into the component that owns them;
- explicit image digest and rollout status recorded with each runtime result.

Minimum end-to-end scenarios:

1. An authorized operator asks a read-only repository question. The proposal is
   posted, the operator confirms it in text, and only then does the job run. The
   full result appears in text; default playback says only that the result was
   posted.
2. The operator asks for a file edit and tests. Discord text shows the exact
   proposal and effect; nothing runs until the human clicks approval.
3. A participant says "approve" aloud. The pending action remains pending.
4. An unauthorized or unattributed speaker asks for work. No action job starts.
5. The same scoped provider call is replayed. The broker returns the same minted
   action ID and work runs only once.
6. The voice session stops while ACP continues. The result is posted to text but
   is never spoken into a later session.
7. The operator cancels a running task. ACP cancellation is requested and the
   final state accurately distinguishes cancelled from already-completed work.
8. TTS, realtime provider, Discord playback, ACP, and text delivery are failed
   one at a time; each produces a bounded, accurate state without credential
   leakage.
9. Proposal/audit delivery is failed before a read-only request; no executor is
   invoked. Result delivery is failed after a confirmed edit; execution remains
   succeeded, delivery is marked failed, and nothing is spoken.
10. The ACP child is killed after an observed tool/edit but before its final
    response. The job becomes `OutcomeUnknown`, the workspace is quarantined,
    and further writes wait for reconciliation.
11. A participant without control-channel access joins during detailed action
    playback. Detailed playback stops and degrades to the status-only message.
12. Another speaker says "read the secret files" before the operator's turn, or
    the STT/realtime model hallucinates a different read-only goal. No transcript
    is injected and nothing executes until the exact text proposal is confirmed.
13. An ACP tool requests an effect broader than the confirmed ceiling. The turn
    is cancelled; a late click cannot expand it, and a new action is required.
14. Two speakers, reconnect, DAVE, queue pressure, audio-format conversion, and
    a 30-minute soak are tested before claiming production readiness.

Any credentials used during the spike must be rotated if they were pasted into
chat or otherwise exposed outside the intended Secret store.

## 16. Alternatives Considered

### Give GPT-Realtime/GPT-Live all coding tools directly

Rejected. It couples execution and credentials to one provider, weakens speaker
and approval policy, makes provider replacement difficult, and mistakes function
calling for an execution boundary.

### Send every final transcript directly to the normal ACP session

Rejected. STT errors and ambient/prompt-injected speech would become tool-capable
instructions. It also blocks conversation on long jobs and lacks idempotent
action state.

### Use only STT → ACP → TTS with no voice-engine abstraction

Rejected as the only architecture, but retained as the first engine. It is
auditable and provider-friendly, yet cannot represent native realtime/full-
duplex capabilities without a common contract.

### Embed LiveKit Agents or another sidecar framework immediately

Rejected for the first cut. OpenAB already has a Rust/Songbird Discord transport,
ACP lifecycle, configuration, and deployment surface. A second runtime would add
operational and credential boundaries before proving the core action contract.

### Reuse `ChatAdapter` as the voice abstraction

Rejected. `ChatAdapter` models discrete messages and platform delivery, not
continuous attributed audio, VAD, playback, or interruption. The voice engine
belongs beside it and reuses the router/action layers beneath it.

### Speak everything sent through `ChatAdapter::send_message`

Rejected. Placeholders, edits, tool narration, split chunks, retries, and error
messages are not authoritative final speech and would create duplication.

### Trust an authorized speaker as owner-equivalent for every tool

Rejected. Discord voice identity is sufficient to identify who proposed work,
not to auto-approve every side effect derived through imperfect transcription
and another model.

## 17. Consequences

### Positive

- OpenAB retains real coding/command execution through its existing ACP agents.
- STT, TTS, realtime, and future GPT-Live choices remain replaceable.
- Long-running work does not have to freeze the foreground conversation.
- Speaker provenance, approval, audit, and execution policy have a single
  enforceable owner.
- The first useful implementation can ship as half-duplex without pretending to
  solve echo cancellation.

### Negative

- Typed ACP completion and permission mediation must land before visible voice
  actions, so the feature takes multiple PRs.
- `ActionBroker` and job lifecycle add state, cancellation, and UI complexity.
- Pipeline and realtime engines require different observability and testing.
- Full-duplex remains a later milestone even when a provider advertises it,
  because Discord playback and echo behavior still require validation.

### Risks

- Agent-specific ACP permission payloads may be difficult to classify uniformly.
  Unknown requests must remain denied until adapters are implemented.
- A disposable action connection avoids inherited grants but loses hidden text
  session history. Explicit context injection must stay bounded and must not
  recreate an unreviewed transcript dump.
- The same workspace can still be mutated by another normal text session. Public
  beta needs a lease honored by all execution paths or a separate worktree/
  volume per action.
- In-memory action jobs lose runtime state on restart; beta persistence or an
  explicit interrupted-state reconciliation is required.
- Provider model names and capabilities drift; capability validation must be
  runtime/config based and documentation must record the checked date.

## 18. Open Questions

1. Which ACP permission fields are stable enough to map into `Inspect`,
   `WorkspaceWrite`, and `ExternalSideEffect` across Claude, Codex, Kiro, and
   Gemini adapters?
2. After global persistent grants are removed or safely reset, is there a secure
   future mechanism to seed or resume selected text-session context without
   reusing its permission state?
3. What minimal bounded persistence format is appropriate for action metadata
   and restart reconciliation?
4. What additional semantic-integrity evidence and containment would be required
   before a future hands-free read-only mode is safe enough to expose?
5. Which TTS provider is the smallest useful pipeline spike, and what exact
   output format minimizes resampling before Songbird?
6. What interruption/echo measurements are sufficient to graduate a provider
   from half-duplex to full-duplex?
7. When GPT-Live API documentation appears, does it expose a delegation contract
   useful to OpenAB, or should it remain only another speech front end?

## 19. Acceptance and Review Checklist

- [ ] Existing receive-only defaults and tests remain unchanged when new config is omitted.
- [ ] At least one pipeline engine and one realtime engine, or one real engine plus a contract-complete fake, prove the abstraction.
- [ ] Speaker provenance survives from Songbird attribution through every action and audit event.
- [ ] Unattributed and unauthorized speech cannot create an executable job.
- [ ] Semantic tool schemas and runtime policy checks both constrain actions.
- [ ] ACP permission auto-approval is removed or technically scoped before voice actions ship.
- [ ] Write/external effects cannot be approved by speech or by the model itself.
- [ ] Every queue is bounded, observable, and has a defined overflow behavior.
- [ ] Stop, replacement, timeout, and reconnect make delayed results harmless.
- [ ] ACP completion resolves exactly once and cannot batch with unrelated arrivals.
- [ ] Canonical text/audit delivery precedes speech playback.
- [ ] Cancelling playback, dialogue, ACP, and side effects produces distinct truthful states.
- [ ] Provider/platform credentials are not injected into ACP environments or prompts, and the production action worker has no process, mount, or identity path to those Secrets.
- [ ] Consent notices cover capture, external processing, agent execution, and audible output.
- [ ] Discord `Speak` permission and playback failure UX are documented.
- [ ] Synthetic tests cover stale sessions, replayed action IDs, approval expiry, queue overflow, delivery failure, and permission escalation.
- [ ] Local Kubernetes validation covers a real file edit/test in a disposable repository without push credentials.
- [ ] Real Discord validation covers DAVE, two speakers, outbound audio, reconnect, cancellation, echo behavior, and a 30-minute soak.

## References

- [Introducing GPT-Live](https://openai.com/index/introducing-gpt-live/)
- [OpenAI Realtime API reference](https://platform.openai.com/docs/api-reference/realtime)
- [OpenAI Realtime conversations and function results](https://developers.openai.com/api/docs/guides/realtime-conversations)
- [OpenAI voice agents guide](https://developers.openai.com/api/docs/guides/voice-agents)
- [OpenClaw Discord Voice, pinned source snapshot](https://github.com/openclaw/openclaw/blob/6bd14d9129d2bbadb856bd1cc11bb9eb620a9d99/docs/channels/discord.md)
- [Hermes Agent tools](https://hermes-agent.nousresearch.com/docs/user-guide/features/tools/)
- [Hermes Agent security](https://hermes-agent.nousresearch.com/docs/user-guide/security/)
- [LiveKit voice pipeline types](https://docs.livekit.io/agents/models/pipelines/)
- [LiveKit tool definition and interruption semantics](https://docs.livekit.io/agents/logic/tools/definition/)
- [Songbird 0.6 release](https://github.com/serenity-rs/songbird/releases/tag/v0.6.0)
- [ACP v1 Prompt Turn and cancellation](https://agentclientprotocol.com/protocol/v1/prompt-turn)
