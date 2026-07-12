# ADR: Discord Voice Channel Receive and Transcription

- **Status:** Proposed — one-speaker live receive passed; accuracy and soak validation pending
- **Date:** 2026-07-12
- **Implementation branch:** `feat/discord-voice-receive`
- **Related:** [Discord guide](../discord.md), [STT guide](../stt.md), [Discord Voice Channel guide](../discord-voice.md), [Discord Voice intent routing ADR](duplex-voice-engines-and-action-broker.md)

---

## 1. Status Snapshot

This table records implementation state separately from runtime validation. Update it as the branch advances; do not infer production readiness from code completion alone.

| Capability | State on 2026-07-12 | Notes |
|---|---|---|
| Discord voice-message attachment STT | **Implemented (existing baseline)** | OpenAB downloads an attached audio file and sends its bytes to the configured OpenAI-compatible STT endpoint. |
| Songbird 0.6 receive integration | **Implemented on branch** | When voice is enabled, Serenity registers Songbird in receive `Decode` mode and adds `GUILD_VOICE_STATES`. Receive handlers are installed before join. |
| Backward-compatible opt-in | **Implemented on branch** | `[discord.voice].enabled` defaults to `false`; disabled deployments do not add the voice intent, command, or runtime. |
| `/voice join`, `/voice stop`, `/voice status`, `/voice transcript`, `/voice summary` | **Implemented on branch** | A global command controls one session per guild. Raw retained text can be downloaded ephemerally for STT validation. Stop never requests a summary automatically. |
| `VoiceTick`/`SpeakingStateUpdate` capture | **Implemented on branch** | SSRCs map to Discord users; per-user 48 kHz stereo buffers segment decoded PCM in the callback using audio-sample silence and duration bounds. |
| Bounded STT and transcript pipeline | **Implemented on branch** | Only completed segments enter a bounded queue. Workers encode WAV in memory, call `stt::transcribe`, and append to a bounded transcript; no audio files are written. |
| ACP summary dispatch | **Implemented with an unresolved security limitation** | Only explicit `/voice summary` uses the pinned control channel's normal, tool-capable ACP path. Transcript-as-data behavior is prompt-enforced, not sandbox-enforced. |
| Automated verification | **Partial pass** | `cargo clippy -p openab-core --lib -- -D warnings`, 46 focused hybrid intent/runtime tests, and 16 binary tests pass. The current core library run reaches 740/741 with one pre-existing macOS `/bin/false` assertion failure. Repository-wide fmt still reports pre-existing drift; Windows verification remains pending. |
| Local Kubernetes artifact/runtime smoke | **Passed** | Image `sha256:661d58c5934332ccb4222b7b18e4063cb0d8f9949aec110938bf1928bb1250b9` reached `1/1 Running` with zero restarts in `docker-desktop/openab-local` as Helm release `openab-voice-smoke`. The smoke ran without Discord/STT credentials and is not Voice receive evidence. |
| Local Discord/Groq/Claude readiness | **Passed; hybrid Slice 1 startup passed** | Helm revision 8 runs `localhost:5555/openab:claude-voice-hybrid-66d8363` at digest `sha256:944e281b673afe8ab69fdd7eebac2a91943f80ff226f4b9447cf175b9dcc4c94`, `1/1` ready with zero restarts. Logs confirm `voice_intent_enabled=true`, Groq STT, Aragorn connected, and slash commands registered. Local Sam revision 2 is also ready on the matching Codex image. A real spoken proposal, local ACP task, and target-agent handoff remain pending. |
| Real Discord receive | **Passed for one speaker** | The first Songbird join exposed a Rustls 0.23 provider ambiguity in unified builds. Commit `7b8f90f` explicitly selects AWS-LC when AgentCore features are present and ring for Discord-only builds. After that fix, join, decoded receive, attribution, timestamps, and raw transcript download passed. The rollout ended the session; explicit stop and DAVE remain separate checks. |
| STT accuracy sample | **Failed; controlled retry pending** | The first four-segment raw transcript preserved one-speaker attribution and timing, but contained an ellipsis-only segment, an incorrect-language hallucination, and unreliable Chinese text. Image `sha256:423cdf29675a4d8666cf19a686626dee2a75217312789b1fb057deda806fae88` is deployed with `whisper-large-v3` plus `language = "zh"` for the next A/B sample. |
| Voice intent broker | **Hybrid Slice 1 implemented; rollout/live validation pending** | With explicit opt-in, an unaddressed command-shaped transcript asks for text confirmation and then enters Aragorn's existing Dispatcher/ACP path; a request naming one configured target still dispatches a nonce-enforced real Discord mention. Target-head coordination grammar and typed clarification remain follow-up work. |
| Spoken confirmation and TTS | **Not started** | Text confirmation is the first engineering slice. Spoken yes/no plus Songbird TTS is the first hands-free daily milestone. |
| Realtime/Live and result observation | **Not started; optional later slices** | Realtime/Live must emit the same proposal contract. Thread observation must not block the initial confirmed-dispatch loop. |
| Two-speaker and reconnect soak test | **Pending real-world validation** | Attribution, reconnect health beyond a unit state transition, and a 30-minute minimum soak remain unverified. |
| Production readiness | **Not established** | Requires the acceptance checks in section 12. |

### Baseline at Branch Creation

OpenAB already supports Discord voice **messages**, which are uploaded audio attachments. The existing media pipeline downloads an attachment, calls `stt::transcribe`, and forwards the resulting text to the ACP agent.

At branch creation, OpenAB did **not** yet:

- join a Discord Voice Channel;
- request the `GUILD_VOICE_STATES` gateway intent;
- receive or decode live Discord voice packets;
- map live audio to individual Discord users;
- maintain a Voice Channel transcript; or
- expose Voice Channel lifecycle commands.

The branch now implements those code paths, but the baseline distinction remains:
voice-message STT and Voice Channel capture are separate features. Enabling STT
does not silently enable live capture.

This ADR remains authoritative for Discord Voice Channel receive and transcript
retention. The [Discord Voice intent routing ADR](duplex-voice-engines-and-action-broker.md)
builds directly on this subsystem: attributed transcript becomes a pending
semantic intent, receives one confirmation, and is routed either through this
instance's existing ACP path or the existing Discord bot-to-bot path.

## 2. Context and Purpose

The goal is to let an OpenAB Discord bot join a Voice Channel, transcribe a meeting with Discord user identity attached to each segment, and create text summaries through the existing ACP session flow.

This provides a more controllable input path than consuming a platform-generated meeting transcript: OpenAB receives audio as a participant, chooses the STT provider, retains speaker IDs and timestamps, and can mark or expose transcription uncertainty. It also introduces higher privacy and operational risk, because the bot becomes an audio endpoint inside an active call.

This feature belongs beside the existing Discord adapter. It is not a new `ChatAdapter` or gateway platform: connection control, speaker identity, permissions, and the output destination are all Discord-specific, while STT and ACP execution reuse OpenAB core services.

## 3. Decision

OpenAB will add an **opt-in Discord Voice subsystem** with these boundaries:

1. Songbird handles the Discord voice connection, DAVE transport, packet scheduling, and Opus decoding.
2. OpenAB maps Songbird receive events into bounded, per-speaker PCM buffers and closes segments inside the non-async receive callback using processed sample counts.
3. Only completed segments enter a bounded queue. A worker encodes them to in-memory WAV and calls the existing OpenAI-compatible STT client.
4. A Voice session stores ordered transcript segments with Discord user identity and timestamps.
5. Slash commands control the session. The text channel or thread where `/voice join` is invoked becomes the control and output channel; the bot joins the caller's current Voice Channel.
6. Only explicit `/voice summary` sends the accumulated transcript through the normal ACP/session path and posts text output to the pinned control channel. `/voice stop` never submits it automatically.
7. The first version is receive-only. The bot does not synthesize or play speech.

The branch configuration surface is:

```toml
[discord.voice]
enabled = false
allowed_channels = []
silence_ms = 2000
max_segment_seconds = 30
max_session_minutes = 120
max_pending_segments = 8
max_transcript_bytes = 80000
```

`false` is the default, including when the section is omitted. Existing deployments must retain their current Discord intents, commands, active runtime tasks, and observable behavior unless operators explicitly opt in.

## 4. Goals

- Join the invoking user's current Discord Voice Channel through an explicit command.
- Preserve speaker identity by keeping audio streams separate per Discord user.
- Convert bounded audio segments to WAV or another STT-compatible payload and reuse `stt::transcribe`.
- Accumulate a time-ordered transcript suitable for `/voice summary`.
- Let an authorized operator download the retained raw transcript ephemerally so STT accuracy can be measured independently of ACP summarization.
- Pin status and agent output to the text channel or thread that started the session.
- Keep voice receive callbacks free of async network/disk work and bound memory use under STT backpressure.
- Leave the channel and release capture state and in-memory audio deterministically, while reporting any bounded STT drain timeout.
- Make recording/transcription visible to participants and operators.
- Keep all existing deployments unchanged by default.

## 5. Non-Goals for the First Version

- Text-to-speech or playing ACP responses into the Voice Channel.
- Wake words, full-duplex realtime conversation, barge-in, or echo cancellation.
- Sending every speech segment to the ACP agent as an independent turn.
- Permanent raw-audio recording, downloadable multitrack archives, or podcast mixing.
- Voice-print diarization. Discord user/SSRC mapping is the primary speaker source.
- Automatically joining channels based on presence or following users between channels.
- Stage Channel support.
- Claiming that DAVE receive, reconnect, or long-running stability works before live testing.
- Treating the feature as a compliance solution. Operators remain responsible for consent and retention requirements in their jurisdiction and organization.

## 6. Prior Art

### Songbird 0.6

[Songbird](https://github.com/serenity-rs/songbird) is the Rust voice library in the Serenity ecosystem and is the transport choice for this subsystem. [Songbird 0.6.0](https://github.com/serenity-rs/songbird/releases/tag/v0.6.0) added Discord DAVE support, moved Opus encoding/decoding to `opus2`, and documents DAVE as mandatory for Discord voice connections in 2026.

With the `receive` feature, Songbird exposes:

- `SpeakingStateUpdate` to obtain the SSRC-to-user relationship;
- `VoiceTick`, fired every 20 ms with scheduled and decoded audio for live users;
- connection, reconnect, disconnect, RTP, and RTCP events.

The design uses `VoiceTick` rather than building a jitter buffer directly on raw RTP. The receive handler still needs `SpeakingStateUpdate` and Discord voice-state events to maintain user identity. See the [Songbird receive events documentation](https://serenity-rs.github.io/songbird/current/songbird/events/enum.CoreEvent.html) and [official Serenity receive example](https://github.com/serenity-rs/songbird/blob/current/examples/serenity/voice_receive/src/main.rs).

Songbird support makes the approach plausible; it does not substitute for a real Discord/DAVE soak test.

### Craig

[Craig](https://github.com/CraigChat/craig) is a mature multi-track Discord recorder. Its useful precedent is the product model rather than its storage stack:

- an explicit start/stop lifecycle;
- one track per Discord user;
- visible recording state;
- bounded recording duration; and
- deterministic finalization.

OpenAB adopts those lifecycle and per-user principles, but does not copy Craig's download, mixing, web application, or long-term recording scope.

### OpenClaw

[OpenClaw's Discord voice support](https://github.com/openclaw/openclaw/blob/main/docs/channels/discord.md#voice) is the closest agent-oriented precedent. Relevant lessons include:

- voice is opt-in and enables the Voice States intent only when needed;
- the voice session can route work to a pinned text session;
- capture, segmentation, STT, agent execution, and playback are separate stages;
- a silence grace period avoids fragmenting normal pauses;
- transcript logs are bounded; and
- repeated decrypt failures require explicit recovery rather than silent success.

OpenAB intentionally stops before OpenClaw's TTS, realtime model, wake-name, follow-user, and barge-in features.

## 7. Implemented Spike Architecture

```text
Discord slash interaction
  │ /voice join from a text channel or thread
  ▼
DiscordVoiceManager
  ├─ pins control/output ChannelRef
  ├─ resolves caller's current Voice Channel
  ├─ owns one VoiceSession per guild
  └─ joins through Songbird
        │
        ├─ SpeakingStateUpdate ──► SSRC → Discord User ID map
        │
        └─ VoiceTick ──► per-user PCM buffers
                       ├─ sample-count silence boundary
                       └─ maximum-duration boundary
                              │ completed segments only
                              ▼
                       bounded segment queue
                              │
                              ▼
                       STT worker
                       ├─ PCM → in-memory WAV bytes
                       └─ existing stt::transcribe
                              │
                              ▼
                       TranscriptStore
                       ├─ Discord speaker ID
                       ├─ start/end timestamp
                       └─ bounded transcript text
                              │
                              ▼ /voice summary
                       existing ACP SessionPool
                              │
                              ▼
                       pinned Discord text channel/thread
```

### 7.1 Session Boundary

The Voice session controls capture and transcript state; it is not itself an ACP process. Its identity is the guild and joined Voice Channel, while its control/output destination is fixed at `/voice join` time.

The ACP session follows OpenAB's normal Discord text channel/thread session rules. This keeps agent history and replies where users can inspect them and avoids deriving an ACP session solely from a transient Voice Channel connection.

The implementation permits at most one active Voice session per guild. A second
join request returns an actionable error instead of replacing it silently.

### 7.2 Receive Path and Backpressure

Songbird callbacks do not call STT, ACP, or perform file I/O. The implemented receive callback:

1. updates speaker mappings;
2. appends decoded frames or scheduled silence to a per-user PCM buffer;
3. closes segments using processed sample counts and maximum duration; and
4. attempts to enqueue only completed segments into a bounded channel.

The worker consumes completed segments outside the callback, encodes WAV bytes in
memory, calls STT, and appends text to bounded transcript storage. If the segment
queue is full, the new segment is dropped and `/voice status` increments the dropped
segment counter. The callback never waits for queue capacity.

Segment boundaries are based on processed audio samples plus a maximum segment size. Wall-clock callback delay is not used as a measure of silence under load.

### 7.3 Speaker Identity

Discord voice RTP identifies streams by SSRC, while Discord users are identified separately. `SpeakingStateUpdate` maintains the relationship. Audio received before a mapping exists is skipped; it is not attributed to an arbitrary user.

Transcript segments retain a stable Discord user ID and render it as a mention. The implementation does not perform voice-print diarization or depend on mutable display names.

### 7.4 STT and Transcript Handling

Voice Channel STT reuses the existing `[stt]` provider configuration and
`stt::transcribe` API. Startup rejects voice enablement when STT is disabled, and
the join path also retains an actionable readiness guard.

An example rendered segment is:

```text
[00:02:14.200] <@123456789>:
The deployment is blocked by ImagePullBackOff, not MemoryPressure.
```

Only `/voice summary` starts an ACP turn in this implementation. Individual STT segments accumulate without independently invoking the coding agent.

The retained transcript is bounded by bytes and entry count. Status exposes retained
entries/bytes, evicted entries, and rejected entries. It remains in process memory
after stop so an operator can explicitly request a final summary; a new session for
the guild replaces it, and process exit removes it.

## 8. Command Lifecycle

The committed command interface is:

| Command | Intended behavior |
|---|---|
| `/voice join` | Verify voice/STT config and authorization, resolve the caller's current Voice Channel, pin the invocation text channel, install receive handlers, join, and publish the required start notice before entering `Listening`. |
| `/voice status` | Report state, Voice/control channel IDs, elapsed time, tracked speakers, retained transcript entries/bytes and eviction/rejection counts, pending STT, STT failures, and dropped segments without exposing transcript content. |
| `/voice transcript` | Wait up to five seconds for queued STT, then attach the retained raw transcript to the command's ephemeral response for accuracy and attribution checks. It does not invoke ACP. |
| `/voice summary` | Wait up to five seconds for queued STT, then submit the retained transcript through the pinned text ACP session. It can run during or after capture and is the only operation that invokes ACP. |
| `/voice stop` | Stop accepting frames, flush current per-user buffers, leave, wait up to 30 seconds for queued STT, and post a best-effort stop notice. A timeout is reported, remaining bounded STT may continue, and no summary is automatic. |

Implemented state transitions:

```text
disabled ── enabled=true + restart ──► idle
idle ── /voice join ──► connecting
connecting ── join + public notice succeeds ──► listening
connecting ── join/notice fails ──► stopping ── leave succeeds ──► session removed
listening ── terminal driver disconnect ──► failed
connecting/listening/failed ── /voice stop ──► stopping ── leave succeeds ──► stopped
connecting/listening ── hard timeout ──► stopping ── leave succeeds ──► expired
listening/stopped/expired/failed ── /voice transcript ──► same state + ephemeral file
listening/stopped/expired/failed ── /voice summary ──► same state + ACP turn
```

The public start notice is a gate: if it cannot be posted after join, the manager
discards the session and leaves instead of entering `Listening`. A manual stop notice
is best effort. The hard session timeout changes state to `Expired`, leaves, and logs,
but currently posts no public timeout notice and requests no summary.

Songbird emits `DriverDisconnect` only after recovery attempts are exhausted, so the
session becomes terminal `Failed`; OpenAB does not mislabel it as reconnecting.
Successful `DriverReconnect` events must still name the session's pinned, allowed
Voice Channel. A reconnect or administrative move to another channel fails capture
and removes the call. Transient retry progress is not exposed, and a state that says
`Listening` while no decryptable audio arrives remains a possible undetected failure.

## 9. Configuration and Backward Compatibility

The branch exposes these experimental controls:

```toml
[discord.voice]
enabled = false
allowed_channels = []
silence_ms = 2000
max_segment_seconds = 30
max_session_minutes = 120
max_pending_segments = 8
max_transcript_bytes = 80000
```

When the section is absent or `enabled = false`:

- OpenAB must preserve existing Discord behavior;
- Voice Channel commands must not initiate capture;
- the bot must not join a Voice Channel;
- no live audio is decrypted or sent to STT; and
- voice-specific gateway intents and tasks are not activated.

Voice Channel capture additionally requires existing STT configuration:

```toml
[stt]
enabled = true
api_key = "${GROQ_API_KEY}"
model = "whisper-large-v3-turbo"
base_url = "https://api.groq.com/openai/v1"
```

`allowed_channels` restricts joined Voice Channel IDs. It is independent of
`[discord].allowed_channels`, which gates the text channel/thread where lifecycle
commands run. `[discord].allowed_users` authorizes operators only: the receive path
captures every audible participant that Discord maps to a user. It does not implement
per-speaker consent or an audio exclusion list.

These values are implemented but remain experimental pending real measurements.
Capture is additionally capped at 25 mapped speakers and STT requests have a
120-second timeout. With the defaults, the completed-segment queue retains at most
about 46 MB of raw PCM, excluding active speaker buffers and one WAV request body.
Every future change must preserve the prior behavior by default or be documented as
a migration.

## 10. Security, Privacy, and Consent

### 10.1 Explicit and Visible Capture

- OpenAB must never auto-join or auto-record in the first version.
- `/voice join` must be an authorized, explicit action.
- The control channel must receive a visible notice that transcription started, who started it, which Voice Channel is involved, and how to stop it before the manager enters `Listening`.
- A visible status must remain discoverable for the duration of capture.
- Joining a Voice Channel captures all audible participants, not only the command invoker. `allowed_users` controls who can operate OpenAB; it is not participant consent.

Operators must obtain the consent required by their laws, server rules, employment policies, and STT provider terms. OpenAB should make consent visible but cannot determine legal sufficiency.

### 10.2 Audio and Transcript Retention

- Raw PCM and encoded WAV remain in bounded process memory. The worker writes no temporary audio files and releases bytes after each STT request.
- The Voice subsystem does not persist full recordings.
- Transcript text remains in a bounded in-memory store after stop until a new guild session replaces it or the process exits. Only explicit `/voice summary` sends it to ACP and the configured model provider; that downstream retention must be disclosed.
- Full transcript text must not be written to normal logs. Diagnostics should contain IDs, durations, byte counts, states, and a bounded/redacted preview only when explicitly enabled.
- Memory and queue limits must prevent a stalled STT endpoint from retaining an unbounded meeting.

### 10.3 Transcript Prompt Injection

Spoken and transcribed text is untrusted data. A participant can say content that
looks like an instruction to read files, reveal secrets, or invoke tools. The branch
escapes Discord mention delimiters and prompts the agent not to follow instructions
or call tools based on transcript content. However, summary dispatch still uses the
normal tool-capable ACP session; the restriction is prompt-only, not a technical
sandbox. This unresolved risk is another reason the feature is not production-ready.

### 10.4 Credential and Process Boundary

- `DISCORD_BOT_TOKEN` and STT credentials remain in the OpenAB process.
- They must not be added to `[agent].env` or exposed to the ACP child process.
- Existing `env_clear()` child-process isolation remains unchanged.
- A cloud `base_url` sends audio to that provider; a local OpenAI-compatible endpoint can keep audio on the operator's network.

### 10.5 DAVE Is Transport Security, Not No-Access

Discord DAVE encrypts the voice connection between participants. The bot is an authorized participant and decrypts audio locally to perform STT. Enabling DAVE does not mean the OpenAB process or configured STT provider cannot access meeting audio.

## 11. Failure Handling and Observability

The implemented `/voice status` reports:

- connection state;
- Voice Channel and pinned control channel IDs;
- elapsed session time, tracked-speaker count, and ignored-speaker count after the 25-speaker cap;
- retained transcript entries and bytes, plus eviction/rejection counts;
- pending STT work and STT failure count; and
- dropped completed-segment count.

Connection, reconnect, leave, session timeout, queue loss, and STT errors are
also logged without transcript content. The branch does not yet expose decoded-frame
rate, last-decodable-audio age, unmapped-frame count, STT latency/retries, or explicit
DAVE/decrypt health in status. Therefore a connection can still appear `Listening`
while receiving no useful audio. Live validation must treat that as a known
observability gap rather than inferring health from connection state.

## 12. Verification and Acceptance Checklist

### Code and Unit Verification

- [x] Targeted Discord-feature `cargo check` passes.
- [x] 78 targeted voice/config/runtime tests pass.
- [ ] `cargo fmt`
- [x] `cargo clippy -- -D warnings`
- [ ] `cargo test --workspace` (the core library reaches 740/741; one unrelated macOS `/bin/false` assertion still fails locally while the targeted new suites pass)
- [ ] `cargo check --target x86_64-pc-windows-gnu`
- [x] Default/omitted `[discord.voice]` leaves voice disabled in config/runtime tests.
- [ ] Disabled voice does not register active receive work or join channels.
- [ ] Command authorization follows Discord allowlists.
- [ ] A caller outside a Voice Channel receives an actionable `/voice join` error.
- [ ] A disabled/misconfigured STT provider produces an actionable error.
- [ ] SSRC mappings cannot attribute audio to the wrong user.
- [x] PCM-to-WAV output has valid headers, channel count, sample rate, and duration in synthetic tests.
- [x] Segment boundaries and maximum sizes are deterministic with synthetic PCM.
- [x] Transcript byte/entry bounds and segment drop counters have unit coverage.
- [ ] `/voice stop` removes the call and capture sender; queued STT drain/continuation behavior is validated under load.
- [x] Transcript rendering order is covered explicitly when STT completes out of order.

### Real Discord / DAVE Validation

- [x] One-speaker join, decoded receive, attribution, timestamps, and transcript were validated in a normal Discord Voice Channel.
- [ ] Explicitly validate current DAVE behavior across stop, leave, reconnect, and rejoin.
- [ ] Two human accounts produce separate, correctly attributed audio.
- [ ] Simultaneous speech remains in separate speaker streams.
- [ ] The first spoken syllable after silence is not consistently lost.
- [ ] A real STT provider receives valid audio and returns usable text.
- [ ] `/voice summary` posts to the text channel/thread that invoked `/voice join`.
- [ ] Moving or disconnecting a participant updates mappings correctly.
- [ ] A brief bot network interruption either recovers or ends visibly as `Failed` after Songbird exhausts retries.
- [ ] Leave/rejoin creates a healthy new DAVE receive session.
- [ ] A minimum 30-minute two-speaker soak has no silent receive failure.
- [ ] Backpressure is exercised by slowing or failing the STT endpoint.
- [ ] Consent/start/stop notices are visible and accurate.
- [ ] The deployed build writes no audio files and releases in-memory WAV payloads after STT completion.

### Packaging Validation

- [ ] The default unified image still builds for all variants affected by common layers.
- [ ] The Discord-enabled image contains any native runtime libraries required by `opus2`.
- [ ] If the Helm chart changes, both required `helm template` commands pass and the new value is documented.

## 13. Rollout

1. Land the receive-only implementation behind `enabled = false`.
2. Complete full-repository verification beyond the passing targeted check and 78 tests.
3. Run the real Discord/DAVE checklist in a private test server.
4. Record measured loss, STT latency, memory growth, and reconnect behavior.
5. Keep the feature experimental until the soak criteria pass.
6. Only then update this ADR to `Accepted` or `Implemented` and describe the exact supported configuration in the user guide.

If real DAVE receive is unstable, retain the branch as a documented feasibility result rather than shipping an interface that can appear connected while silently missing audio.
