# Discord Voice Channels

> **Experimental status snapshot (2026-07-13):** The implementation on
> `feat/discord-voice-receive` can register and run the `/voice` lifecycle, join
> through Songbird 0.6, segment per-user decoded PCM, transcribe it, and explicitly
> send a retained transcript to ACP for summary. The opt-in intent broker now
> implements hybrid routing with spoken or text confirmation: with
> `default_to_local = true`, every non-empty final operator transcript received
> while no confirmation is pending becomes a local raw-task candidate unless it
> explicitly addresses exactly one configured target. Confirmed local text is interpreted by Aragorn's existing ACP agent;
> explicit configured targets use deterministic Discord delegation. Optional OpenAI-compatible TTS
> plays the paraphrase and short local/delegated acknowledgements through Songbird
> with half-duplex capture suppression. The new spoken/TTS path is implemented but
> has not yet been deployed and validated in a live Voice Channel; text remains the
> fallback.
> Real Discord/DAVE, two-speaker,
> DAVE-specific behavior, two-speaker attribution, reconnect, and long-running
> validation have not passed yet. A live local deployment
> now connects Discord, receives and attributes decoded audio, transcribes with Groq,
> and runs an authenticated Claude ACP backend. The first raw transcript proved the
> receive pipeline but failed the accuracy bar: four attributed segments included an
> ellipsis-only result and an incorrect language detection. Artifact
> `sha256:423cdf29675a4d8666cf19a686626dee2a75217312789b1fb057deda806fae88`
> is deployed for a controlled retry with `whisper-large-v3` and `language = "zh"`.
> The summary uses the normal tool-capable ACP path with only a prompt-level
> injection guard. This is an experimental test build, not production
> meeting-recording support.

This guide records the intended setup, command contract, test environment, and current completion state. For design details, see [ADR: Discord Voice Channel Receive and Transcription](adr/discord-voice.md).

## Purpose

The feature lets an OpenAB bot join the Voice Channel that a user is currently in,
transcribe participants as separate Discord speakers, and post an ACP-generated
summary to the text channel or thread that started the session. An additional
opt-in intent broker can turn an operator's attributed speech into a proposed task,
confirm the interpreted intent once by voice or text, and either execute it through
this OpenAB bot's existing ACP path or delegate it to another OpenAB bot.

```text
Discord Voice Channel
  → Songbird receive + DAVE
  → per-user decoded audio
  → bounded audio segments
  → configured STT provider
  → speaker-attributed transcript
  → /voice summary
  → existing ACP session
  → invoking Discord text channel/thread
```

```text
operator speech
  → non-empty final STT transcript
  → exactly one explicit configured target, or local raw-task fallback
  → text paraphrase + optional spoken paraphrase
  → spoken or text yes/no/correction
  → local candidate: this bot's existing Dispatcher/ACP interprets the raw task
  → named target: nonce-enforced Discord mention and target bot's ACP flow
  → optional spoken local/delegated acknowledgement
```

Slices 2 and 3 add the no-screen path on top of the Slice 1 text scaffold. TTS is
independently opt-in, and the existing pinned text prompt remains available when
TTS is disabled or playback fails.

## Current State

This table records code state separately from runtime evidence. A compiling receive
path is not proof that Discord is delivering complete, correctly attributed audio.

| Area | State on 2026-07-13 |
|---|---|
| Uploaded Discord voice messages | **Implemented and available** through the existing [STT feature](stt.md). |
| Opt-in startup | **Implemented on branch.** `enabled = false` is the default. When enabled, OpenAB requires STT, registers Songbird with receive decoding, and adds `GUILD_VOICE_STATES`; disabled deployments retain the prior intent/runtime behavior. |
| `/voice` lifecycle | **Implemented on branch.** Global `join`, `status`, `transcript`, `summary`, and `stop` subcommands are registered only when voice is enabled. Global command propagation may take time. |
| Receive and segmentation | **Implemented on branch.** Songbird `SpeakingStateUpdate` maps SSRCs to Discord user IDs. `VoiceTick` feeds per-user 48 kHz stereo buffers; the callback closes segments on sample-count silence or maximum duration and only enqueues completed segments. |
| STT pipeline | **Implemented on branch.** A bounded queue feeds workers that encode WAV in memory and call the existing STT client. No temporary audio files are written. Queue loss and STT failures are counted. |
| Transcript | **Implemented on branch.** Text is retained in bounded process memory with user ID and timing metadata; evictions and rejected entries are counted. |
| ACP summary | **Implemented with a security limitation.** Only explicit `/voice summary` submits the transcript. It uses the normal tool-capable ACP path; instructions in the prompt say the transcript is untrusted, but tools are not technically disabled. |
| Hybrid intent Slices 1-3 | **Experimental and opt-in on branch; live spoken/TTS validation pending.** With `default_to_local = true`, every non-empty final transcript from the session operator received while no confirmation is pending creates a local raw-task candidate unless it explicitly addresses exactly one configured target. Spoken or text yes/no/correction resolves the pending intent. Confirmed local text enters this bot's existing Dispatcher/ACP path for LLM interpretation; one explicit configured target receives one nonce-enforced real Discord mention. Optional TTS plays bounded proposal and acknowledgement audio through Songbird while capture is suppressed. Intent routing and TTS default to disabled. |
| Result observation | **Later slice.** The broker does not yet poll the local or target bot's task thread for progress and completion. Initial local/delegated acknowledgements mean accepted or sent, not observed completion. |
| Automated branch verification | **Partial pass.** `cargo clippy -p openab-core --lib -- -D warnings`, 46 focused hybrid intent/runtime tests, and the 16 binary tests pass. The core library run reaches 740/741 with one pre-existing macOS `/bin/false` assertion failure. Repository-wide fmt still reports pre-existing drift; Windows and complete live Discord validation remain pending. |
| Local Kubernetes runtime smoke | **LLM-first local fallback startup passed.** Helm revision 10 runs `localhost:5555/openab:claude-voice-llm-first-0f8511f` at digest `sha256:eae5434841f875b592583f03b100fb9f365566c9990d99760cffa9bcfbd051c7`, `1/1` ready with zero restarts and `default_to_local = true`. Startup reports `voice_tts_ready=false` because the local Secret does not yet contain `TTS_API_KEY`; the duplicate Zeabur Aragorn service is suspended. Local Sam revision 2 remains ready, accepts the Voice text destination, and trusts Aragorn. Real spoken broad-fallback proposal/confirmation/handoff and TTS playback validation remain pending. |
| Discord, Groq, and Claude readiness | **Passed locally.** The bot connects, global commands register, Groq accepts the configured STT model, Claude OAuth persists on PVC, and a Discord-to-Claude ACP turn completes without an authentication error. |
| Live Discord receive evidence | **Passed for one speaker.** After commit `7b8f90f` fixed the unified Rustls provider ambiguity, `/voice join`, decoded receive, speaker attribution, timestamping, and transcript download worked in a real Voice Channel. The subsequent rollout ended the session; explicit `/voice stop` and DAVE behavior remain separate checks. |
| First STT accuracy sample | **Failed the reliability bar.** Four segments retained the correct single-speaker identity and plausible timestamps, but one result was only `...` and another hallucinated a different language. The two Chinese results were also not reliable enough to treat as a meeting record. A controlled model/language A/B retry is pending. |
| Attribution, reconnect, and soak evidence | **Not verified.** Two-speaker attribution, reconnect health beyond a unit state transition, and a 30-minute soak remain required. |

Playback is intentionally narrow: it speaks only bounded intent proposals and
broker acknowledgements. It does not synthesize arbitrary ACP responses or observe
and narrate task completion.

## Local Kubernetes Smoke and Live Test Record

The purpose of this smoke is to prove that the branch's unified Linux image can be
pulled from the local registry, mounted with Helm configuration, and kept running
under the same local Kubernetes environment that will host the real Discord test.
It deliberately does not claim that Discord audio, DAVE, STT, attribution, or ACP
summary work end to end.

Recorded state on 2026-07-12:

| Item | Recorded value |
|---|---|
| Kubernetes context | `docker-desktop` (used explicitly; the global context was not changed) |
| Namespace | `openab-local` |
| Helm release | `openab-voice-smoke`, revision 2, status `deployed` |
| Deployment | `openab-voice-smoke-voice`, `1/1 Running`, zero restarts at verification time |
| Image | `localhost:5555/openab:agentcore-voice-dev-661d58c` |
| Runtime image ID | `sha256:661d58c5934332ccb4222b7b18e4063cb0d8f9949aec110938bf1928bb1250b9` |
| Runtime architecture | Linux/arm64 |

The smoke configuration uses the harmless `/bin/echo` agent and a deliberately
unreachable loopback Custom Gateway. Repeated `connection refused` gateway logs are
expected and keep the process alive for runtime inspection. Discord and Voice
Channel capture are disabled in this smoke configuration.

That temporary smoke release was uninstalled after the following live test release
became healthy:

| Item | Current local test state on 2026-07-12 |
|---|---|
| Kubernetes context / namespace | `docker-desktop` / `openab-local` |
| Helm release / deployment | `openab-voice` revision 6 / `openab-voice-voice` |
| Runtime | `1/1 Running`, zero restarts at verification time |
| Image ID | `sha256:423cdf29675a4d8666cf19a686626dee2a75217312789b1fb057deda806fae88` |
| Rustls provider | Explicit `aws-lc-rs`; startup log confirms installation before Discord/Songbird TLS |
| ACP backend | `claude-agent-acp`; Claude OAuth authenticated and retained on a 1 GiB PVC |
| STT provider | Groq with `whisper-large-v3` and the optional ISO-639-1 hint `zh` |
| Authorization | Explicit control-channel allowlist and one human operator; identifiers are intentionally omitted here |
| Voice evidence | One-speaker join, decoded receive, attribution, timestamps, and raw transcript download passed; first-pass transcription accuracy failed; explicit stop remains to be observed |

The original runtime smoke did not contain a Discord bot token or STT API key. The
live release now receives those values from a Kubernetes Secret through the broker
process. They are not committed to the repository, placed in Helm values, or exposed
through `[agent].env`; the ACP child receives only the normal baseline environment
plus the non-secret Claude executable path.

Claude Code 2.1.179 currently uses an OAuth flow that asks the caller to paste an
authorization code back into stdin. OpenAB's Discord `/auth` relay can show the URL
but cannot feed a later Discord message into that stdin, so this specific flow gets
stuck behind the single-flight guard. The local test authenticated with a one-time
`kubectl exec ... env -i ... claude auth login` process instead, ensuring the Claude
login subprocess did not inherit Discord or STT credentials. Do not use Discord
`/auth` for this Claude version until the handler supports its paste-code flow.

## Voice Messages vs. Voice Channels

These are different Discord features:

| Input | Existing support | How it arrives |
|---|---|---|
| Voice message/audio attachment | Yes | Discord CDN file downloaded after a text-channel message event |
| Live Voice Channel | Experimental branch implementation | Bot joins a call and continuously receives per-user voice frames |

Enabling `[stt]` alone only enables attachment transcription. It does not make the bot join or listen to Voice Channels.

## Test Environment Requirements

Prepare:

- a private Discord test server;
- one normal Voice Channel (Stage Channels are out of scope);
- one text channel or thread for commands and results;
- the OpenAB bot;
- two human Discord accounts for speaker-attribution tests;
- a working OpenAI-compatible STT endpoint; and
- a non-sensitive, scripted test conversation.

The bot invitation needs these OAuth scopes:

- `bot`
- `applications.commands`

The bot needs these permissions in the test channels:

- View Channel
- Connect
- Send Messages
- Send Messages in Threads, if a thread is used for control/output
- Read Message History

`Speak` is required when TTS playback is enabled; receive/transcription alone does
not need it. Existing Discord text operation may require additional permissions
described in the [Discord guide](discord.md).

## Configuration

Voice Channel support is explicitly opt-in:

```toml
[discord]
bot_token = "${DISCORD_BOT_TOKEN}"
allowed_channels = ["TEXT_CHANNEL_ID"]
allowed_users = ["OPERATOR_USER_ID"]

[discord.voice]
enabled = false

[stt]
enabled = true
api_key = "${GROQ_API_KEY}"
model = "whisper-large-v3-turbo"
base_url = "https://api.groq.com/openai/v1"
language = "zh" # optional; use only when the meeting language is known
```

Use the full experimental settings in a private test environment:

```toml
[discord.voice]
enabled = true
allowed_channels = ["VOICE_CHANNEL_ID"]
silence_ms = 2000
max_segment_seconds = 30
max_session_minutes = 120
max_pending_segments = 8
max_transcript_bytes = 80000
```

Hybrid intent routing is independently opt-in. Enable local fallback when every
non-empty final operator transcript should be confirmed and then interpreted by
this bot, and optionally register target bots under stable spoken names:

```toml
[discord.voice.intent]
enabled = true
confirmation_timeout_seconds = 30
default_to_local = true

[discord.voice.intent.targets.sam]
discord_user_id = "<SAM_DISCORD_BOT_USER_ID>"
aliases = ["Samuel", "山姆"]

[tts]
enabled = true
api_key = "${TTS_API_KEY}"
model = "gpt-4o-mini-tts"
voice = "marin"
base_url = "https://api.openai.com/v1"
instructions = "Speak briefly in Traditional Chinese."
request_timeout_seconds = 30
```

The default is `false`. Omitting `[discord.voice]` must preserve all existing Discord behavior and must not cause the bot to join or listen to a Voice Channel.

`[discord.voice.intent].enabled` also defaults to `false`. Omitting it preserves
the existing Voice receive, transcript, and explicit summary behavior. Enabling
it requires Voice Channel receive, STT, a positive confirmation timeout, and at
least one available route. `default_to_local` also defaults to `false`; set it to
`true` to make any non-empty final session-operator transcript received while no
confirmation is pending a local raw-task candidate unless it explicitly addresses
exactly one configured target. The raw task reaches this bot's existing
Dispatcher/ACP path only after confirmation, and the ACP/LLM interprets it there;
no request-prefix or known-action allowlist is applied. A local-only configuration
may omit all target tables. If
targets are present, the target table name is its canonical spoken name and is
recognized alongside its `aliases`; `discord_user_id` must be the target bot's
real Discord user ID so OpenAB can emit a valid mention token. Because the
canonical table name is already an alias, do not repeat it in `aliases`, including
variants that differ only by surrounding whitespace or letter case. Normalized
aliases must be unique across all targets.

`[tts]` is independently opt-in and defaults to `enabled = false`. It calls an
OpenAI-compatible `/audio/speech` endpoint and explicitly requests WAV. The model
and voice shown above are defaults, not provider requirements; replace the base
URL, model, and voice together when using another compatible provider. The client
bounds each response to 10 MiB. If TTS is disabled, lacks a key, or fails during a
session, the pinned text prompt and text confirmation path remain available.

`allowed_channels` under `[discord.voice]` restricts which **Voice Channels** may
be joined. `[discord].allowed_channels` independently gates the text channel or
thread used to issue commands. `[discord].allowed_users` controls who may operate
the lifecycle; it does not filter speakers. All audible participants that Discord
maps to a user are captured after the bot joins.

For local-only audio processing, point `[stt].base_url` to a local OpenAI-compatible transcription server. See [STT deployment options](stt.md#deployment-options). A cloud endpoint receives meeting audio and may retain it under that provider's policy.

## Command Lifecycle

The text channel or thread where `/voice join` is invoked becomes the session's control and output channel. The bot joins the caller's current Voice Channel.

| Command | Behavior |
|---|---|
| `/voice join` | Validate configuration and authorization, join the caller's current Voice Channel, pin the current text destination, and announce that transcription started. |
| `/voice status` | Show `Connecting`, `Listening`, `Stopping`, `Stopped`, `Expired`, or `Failed`, plus Voice/control channel IDs, tracked/ignored speakers, elapsed time, retained transcript entries/bytes and eviction/rejection counts, pending STT work, STT failures, and dropped segments. It does not expose transcript text. |
| `/voice transcript` | Download the retained raw transcript as an ephemeral text attachment for direct STT accuracy and speaker-attribution checks. It waits up to five seconds for pending STT and never invokes ACP. |
| `/voice summary` | Wait up to five seconds for queued STT, then ask the existing ACP agent to summarize the retained transcript and post to the pinned text destination. If work is still pending, a later explicit summary can include it. |
| `/voice stop` | Stop capture, flush current speaker buffers, leave, wait up to 30 seconds for queued STT, and post a best-effort public stop notice. It does **not** automatically request an ACP summary. |

`/voice join` only enters `Listening` after the public start notice is posted. If the
notice cannot be posted, capture does not become active. The maximum-session timer
automatically leaves the call and logs expiration, but currently posts no public
timeout notice and requests no summary.

After `/voice stop`, STT work that did not drain within 30 seconds may continue.
The transcript remains in process memory for later explicit summary/status use until
a new session for that guild replaces it or the OpenAB process exits.

## Hybrid Intent Routing (Slices 1-3)

The text channel or thread that runs `/voice join` remains the intent prompt,
audit, and dispatch destination. The user who starts that session is the operator
allowed to resolve its pending intent in speech or text.

A named-target interaction delegates to the configured bot:

```text
Can, in the Voice Channel:
  "叫 Sam 看 canyugs/openab issue 20"

OpenAB, in the pinned text channel and, when TTS is enabled, in Voice:
  "我理解為：要請 Sam 看 canyugs/openab#20 嗎？
   請回覆「對」、「不是」，或用「更正：...」修正。"

Can, in Voice or text:
  "對"

OpenAB:
  <@SAM_DISCORD_BOT_USER_ID> Can asked via voice:
  請看 canyugs/openab#20

OpenAB, in Voice when TTS is enabled:
  "已送出。"
```

The confirmation approves the semantic target and task, not the exact wording of
the final Discord message. The broker owns the destination, mention token, and
dispatch idempotency. The initial STT path uses deterministic configured-target
resolution and a local raw-task fallback; any future Realtime/Live backend may
only propose the intent.

With `default_to_local = true`, any other non-empty finalized operator speech
received while no confirmation is pending becomes a raw task for the same OpenAB
bot (Aragorn in this example):

```text
Can, in the Voice Channel:
  "請先查看 Open App取得issue 1368整理目前行作方向"

Aragorn, in the pinned text destination and, when TTS is enabled, in Voice:
  "我理解為：由我直接處理『先查看 Open App取得issue 1368整理目前行作方向』，對嗎？
   請回覆「對」、「不是」，或用「更正：...」修正。"

Can, in Voice or text:
  "對"

Aragorn:
  "已確認語音意圖；即將由我直接處理：
   先查看 Open App取得issue 1368整理目前行作方向"

Aragorn:
  → queues the task through the existing Dispatcher and ACP agent

Aragorn, in Voice when TTS is enabled:
  "好的，我開始處理。"
```

When the pinned destination is an ordinary text/news channel, the audit message
anchors a dedicated task thread and ACP replies there. If `/voice join` was run
from an existing thread or from a Voice Channel's text chat, Aragorn uses that
destination directly because Discord cannot create a nested or Voice-channel
thread.

Intent rules:

1. One guild and active Voice-session generation can have only one pending intent.
2. A spoken or text yes selects the proposed route. Local work is queued through the existing Dispatcher/ACP path; named-target work uses one nonce-enforced Discord mention.
3. A spoken or text no cancels it without dispatch.
4. A spoken or text correction replaces the pending destination/task and asks for confirmation again; an explicit target or explicit self-directed phrase can switch routes.
5. Timeout, `/voice stop`, or replacing the Voice session abandons the pending intent.
6. Explicitly addressing exactly one configured target selects delegation. Unknown or multiple configured-target matches never create a guessed delegation; with local default enabled, the full utterance remains a raw local candidate.
7. If local thread creation or Dispatcher submission definitely fails, the broker reopens confirmation instead of silently losing the task.
8. Only finalized STT from the session operator can resolve a spoken confirmation. Unrelated, duplicate, wrong-speaker, and stale-session segments cannot dispatch.
9. TTS is half-duplex. Capture is suppressed and partial buffers are discarded while the bot speaks so playback cannot become its own affirmative confirmation.

The initial deterministic router recognizes an explicitly addressed,
single-configured-target request such as `叫 Sam review openab issue 20`. With
`default_to_local = true`, every other non-empty final operator transcript
received while no confirmation is pending is a local candidate, whether or not
it has a request prefix or begins with a known action. These two real STT outputs
remain regression examples and resolve to local execution in opt-in mode:

```text
先查看 OpenAPI 171368整理目前施作方向
請先查看 Open App取得issue 1368整理目前行作方向
```

An empty or whitespace-only final transcript remains a no-op. If another
configured target alias appears later in a targeted task, OpenAB does not guess
the addressee. A name absent from the target registry likewise never creates a
Discord delegation. In local-default mode, either case keeps the entire utterance
as raw local task text for ACP/LLM interpretation after confirmation. During
validation, use `/voice transcript` to distinguish an STT mismatch from a routing
decision.

This behavior is intentionally broad: ordinary non-empty operator narration also
opens a confirmation while the broker is idle. Enable local-default intent mode
for command-style voice sessions; passive meeting capture can leave it disabled.

The receiving target does not need new Rust code. It handles the broker's message
through the existing bot-to-bot Discord and ACP path. On every target bot, allow
the pinned text channel and trust the voice-dispatching bot identity:

```toml
[discord]
allowed_channels = ["<VOICE_CONTROL_CHANNEL_ID>"]
allow_bot_messages = "off"
trusted_bot_ids = ["<VOICE_DISPATCH_BOT_USER_ID>"]
```

The final dispatch contains a real `<@TARGET_BOT_USER_ID>` mention, so the existing
trusted-bot mention admission works without changing `allow_bot_messages` to
`"all"`.

The pinned text flow remains an operational fallback. With TTS configured, the
implemented Slice 2+3 path speaks the proposal and bounded sent/cancelled/error
feedback, accepts spoken yes/no/correction, and resumes capture after playback.
A later observation slice will follow the task thread and report meaningful
progress and the final result.

Intended flow:

1. All participants are told that the bot will transcribe the call and consent according to the applicable policy.
2. The operator enters the test Voice Channel.
3. In the chosen text channel or thread, the operator runs `/voice join`.
4. Participants read the start notice and speak the scripted test conversation.
5. The operator runs `/voice status` and checks pending work, STT failures, and dropped segments.
6. The operator runs `/voice transcript` and scores the raw STT text and speaker IDs against the script.
7. The operator runs `/voice summary` and separately verifies facts, decisions, and action items.
8. The operator runs `/voice stop` and verifies that the bot leaves and the public stop notice appears.
9. If a final post-stop summary is wanted, the operator runs `/voice summary` explicitly; stop never submits one automatically.

## Scripted Test Conversation

Use text that exposes common STT mistakes and has an objective expected result:

```text
Alice: OpenAB 的 ingress-controller 出現 ImagePullBackOff，不是 MemoryPressure。
Bob: Project ID 是 698f255e02b4effb0e85ba56。Can 要在星期五前確認 deployment。
Alice: 決策是先修 image pull secret，不要先擴大 memory limit。
```

Expected summary facts:

- the failure is `ImagePullBackOff`, not `MemoryPressure`;
- the full project ID is preserved;
- the action owner is Can and the deadline is Friday; and
- the decision is to fix the image pull secret before changing memory.

Test each speaker separately, then overlap speech briefly to ensure Discord user streams are not mixed.

## Real Discord Validation Checklist

Code compiling is not sufficient. Before reporting the feature as usable, record evidence for all of these:

- [ ] A bot joins a normal current Discord Voice Channel with DAVE.
- [ ] Two human accounts are mapped to the correct Discord user IDs.
- [ ] Each speaker produces separate, intelligible audio/transcript segments.
- [ ] `/voice transcript` returns an ephemeral raw transcript that can be compared with the scripted ground truth.
- [ ] Simultaneous speech does not cross-contaminate speaker streams.
- [ ] The first syllable after silence is retained.
- [ ] `/voice summary` posts to the text channel/thread that ran `/voice join`.
- [ ] `/voice status` exposes disconnect/reconnect state and dropping/STT counters; logs provide enough evidence to detect a stalled receive path.
- [ ] Brief network loss either recovers or becomes visibly `Failed` after retries are exhausted.
- [ ] Leave/rejoin restores a healthy DAVE session.
- [ ] A two-speaker session runs for at least 30 minutes without silent audio loss.
- [ ] Slow/failing STT does not cause unbounded memory growth.
- [ ] `/voice stop` removes the bot, stops capture, and accurately reports whether the 30-second STT drain timed out.
- [ ] No PCM/WAV files are written; in-memory WAV buffers are released after STT work.
- [ ] Consent/start/stop notices are visible in the control channel.
- [ ] Session timeout causes a leave and an observable log; the lack of a public timeout notice is recorded as a known limitation.

For the optional intent broker, also verify:

- [ ] A configured spoken target produces one paraphrased confirmation in the pinned text channel and, with TTS enabled, one audible paraphrase.
- [ ] Spoken or text yes sends exactly one real target-bot mention; duplicate confirmation does not send twice.
- [ ] With `default_to_local = true`, each documented real STT example and a non-command-shaped non-empty sample produces a local paraphrase and queues the confirmed raw task through this bot's Dispatcher/ACP path.
- [ ] A local task creates one task thread from a normal text control channel, while an existing thread or Voice Channel text chat is used directly.
- [ ] With `default_to_local = false`, the same unaddressed speech remains a no-op and existing explicit-target behavior is unchanged.
- [ ] Spoken and text no/correction, timeout, `/voice stop`, and session replacement follow the documented pending-intent rules.
- [ ] Unknown or multiple configured target names never produce a Discord target mention and remain raw local text in local-default mode.
- [ ] The receiving bot admits the trusted voice bot's mention and starts its normal Discord/ACP flow.
- [ ] A local acceptance speaks the local acknowledgement; delegation speaks the sent acknowledgement; cancelled and failed routes speak their bounded feedback.
- [ ] The bot does not transcribe its own playback into a confirmation, and capture resumes after track end, playback error, and watchdog release.
- [ ] Disabling TTS preserves the pinned text prompt/confirmation path and makes no TTS provider request.

Keep the feature marked experimental until these checks pass on the same build artifact intended for deployment.

## Privacy and Consent

Starting a Voice session affects everyone audible in the channel, not only the operator. `allowed_users` restricts who controls OpenAB; it does not establish consent for other participants.

Before `/voice join`:

- tell every participant that a bot will receive and transcribe their audio;
- state whether STT and optional TTS are local or sent to cloud providers;
- state where the transcript/summary will be posted;
- define the raw-audio and transcript retention policy; and
- provide an easy way to decline or stop capture.

OpenAB encodes WAV payloads in memory and writes no temporary audio files. Those
bytes are released after the STT request completes, but queued work can remain in
memory under the configured bound. Transcript text remains in a bounded in-memory
store until the guild starts another session or the process exits. An explicit
`/voice summary` sends retained text to the ACP/model provider, where it may become
part of agent session history.

When TTS is enabled, OpenAB sends only the bounded intent-broker prompt or
acknowledgement text to the configured TTS provider and keeps the returned WAV in
bounded process memory for playback. Check that provider's retention policy just
as you would for STT.

DAVE protects Discord voice transport. Because the bot is a participant, it decrypts audio locally before STT. DAVE does not prevent OpenAB or the configured STT provider from accessing that audio.

Do not place `DISCORD_BOT_TOKEN`, STT keys, or TTS keys in `[agent].env`.
OpenAB's child-process environment isolation must continue to keep platform
credentials out of the ACP agent process.

## ACP Summary and Prompt-Injection Risk

Meeting speech is untrusted input. A participant can say text that transcribes as an
instruction to read files, reveal secrets, or invoke tools. OpenAB escapes Discord
mention delimiters and adds a prompt telling the agent to treat the transcript only
as data, but `/voice summary` still uses the normal ACP session and its normal tool
capabilities. There is no enforced tool-free summary sandbox in this branch.

Use only synthetic or non-sensitive conversations in the test environment. Do not
enable this feature in production until summary execution can be constrained by a
technical policy appropriate to the deployed agent, rather than relying only on the
prompt.

## Troubleshooting During the Spike

### `/voice` is unavailable

- Confirm the build includes the branch implementation.
- Confirm `[discord.voice].enabled = true`.
- Confirm `[stt].enabled = true` and its API key is configured; voice-enabled startup rejects an unavailable STT configuration.
- Confirm the bot was invited with `applications.commands`.
- Restart OpenAB after changing the flag, then wait for Discord global command registration to propagate.

### The bot cannot join

- Run the command from a guild text channel or thread, not a DM.
- Join a normal Voice Channel before running `/voice join`.
- Confirm the bot has View Channel and Connect permissions for that Voice Channel.
- Check the ephemeral command response and OpenAB logs for join or required start-notice errors.

### The bot joins but receives no transcript

- Confirm `[stt].enabled = true` and the STT key/endpoint is valid.
- Run `/voice status` and confirm at least one tracked speaker, no STT failures, and no dropped segments.
- This spike does not expose decoded-frame rate or last-audio age. A `Listening` state alone cannot prove that useful audio is arriving.
- Treat repeated DAVE/decrypt errors or a connected session with no decoded frames as a failed receive path.
- Leave and rejoin once, then record whether recovery is repeatable; do not call the test successful based only on a connected status.

### Speaker attribution is wrong

- Compare the stable Discord user ID shown in the transcript mention; the branch does not use voice-print diarization.
- Verify that audio received before an SSRC mapping was not assigned to another user.
- Test joins, moves, and reconnects because each can change voice-state mappings.

### STT is slow or inaccurate

- Test short single-speaker segments before a long meeting.
- Verify generated WAV sample rate/channel metadata and actual duration.
- Compare a local and cloud OpenAI-compatible provider with the same scripted audio.
- Confirm `/voice status` does not show growing pending work, STT failures, or dropped segments while STT is slow.

### The text prompt appears but the bot does not speak

- Confirm `[tts].enabled = true` and the TTS API key is present in the OpenAB
  process environment/configuration.
- Confirm the provider exposes an OpenAI-compatible `/audio/speech` endpoint and
  that the configured model and voice are valid for that provider.
- Confirm the bot has Discord `Speak` permission in the joined Voice Channel.
- Check logs for TTS HTTP, WAV validation, decoding, Songbird playback, or track
  errors. The text prompt remains the fallback when speech fails.
- Do not treat code completion as live playback evidence until the operator has
  heard a proposal and local/delegated acknowledgement from the deployed artifact.

### The bot left without a public stop message

- Check whether `max_session_minutes` expired. Timeout currently leaves and logs but does not post a public timeout notice.
- If `/voice stop` was used, check logs for a failed best-effort stop notice.
- Run `/voice status` to distinguish `Expired`, `Stopped`, `Stopping`, `Failed`, `Connecting`, and `Listening` states.

## Updating This Document

When implementation or validation advances, update the **Current State** table first. Keep these states separate:

1. code implemented;
2. automated tests passing;
3. real Discord/DAVE validation passing;
4. soak/reconnect validation passing; and
5. production support declared.

This prevents a compiling Songbird integration from being mistaken for a reliable meeting transcription system.
