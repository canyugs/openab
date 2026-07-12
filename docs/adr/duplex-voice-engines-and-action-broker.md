# ADR: Hands-Free Device Voice Delegation through Discord

- **Status:** Proposed — daily-operation UX and two integration tracks selected; implementation pending
- **Date:** 2026-07-12
- **Implementation:** Not started — Track A is the first delivery target
- **Related:** [Discord multi-agent guide](../multi-agent.md), [Discord guide](../discord.md), [Discord Voice receive ADR](discord-voice.md), [Issue #1364](https://github.com/openabdev/openab/issues/1364), [Issue #1368](https://github.com/openabdev/openab/issues/1368)

---

## 1. Decision

OpenAB's first useful voice-to-agent workflow will be a **device-bound,
hands-free voice delegation broker**, not a new action runtime inside OpenAB.

The operator talks to a Siri-like assistant on their phone. The assistant turns
the request into a semantic delegation, paraphrases the intended task aloud,
and waits for one spoken confirmation. After the operator confirms, a dedicated
Discord bot such as `b0-voice-broker` posts the delegation to an existing OAB
agent. The existing Discord bot-to-bot and ACP paths perform the work unchanged.

The broker then observes the Discord thread in the background and speaks short,
useful progress and completion updates back to the operator.

This UX has two supported integration tracks:

1. **Discord-native, no OpenAB code changes.** The broker infers job state from
   the existing Discord root message, thread, reactions, and replies.
2. **OpenAB-assisted.** The same Discord delegation remains visible, while an
   optional OpenAB event bridge emits structured lifecycle and completion data
   to the broker.

The operator-facing command flow is the same in both tracks. Status wording may
be more precise in the OpenAB-assisted track, but the operator does not learn a
different command vocabulary.

```text
Phone voice client
    │
    │ "Ask B0 to run a group review on PR #12345"
    ▼
Voice Delegation Broker
    │
    │ "You want B0 to start a group review of PR #12345, right?"
    │
    ◄── "Yes"
    │
    │ confirmed semantic intent
    ▼
Discord as b0-voice-broker
    │
    │ "<@B0> Can asked via voice:
    │  start a group review of PR #12345."
    ▼
Existing OpenAB multi-agent flow
    │
    │ B0 creates a thread, runs ACP, and coordinates other bots
    ▼
Voice Delegation Broker observes the thread
    │
    └── "B0 has started coordinating the review. I'll keep tracking it."
```

For ordinary daily work, **there is no Discord text approval step**. Discord
text is the dispatch and audit surface, not a UI the operator must watch.

The one required gate is a spoken **intent confirmation** before a new
delegation is posted. It confirms what the assistant understood; it is not a
permission prompt and it does not require reviewing the exact Discord wording.

## 2. Product Goal

The goal is a daily operating interface for times when looking at a screen is
inconvenient, especially while driving or walking.

The operator should be able to:

- delegate a new task to B0 or another named OAB agent;
- correct a misunderstood task before it is sent;
- continue talking while one or more jobs run in the background;
- ask for status without triggering another confirmation;
- add a new task while existing tasks are still running;
- hear concise progress only when something meaningful changes;
- receive a short completion summary and ask for more detail; and
- return later and resume tracking jobs created by an earlier voice session.

This is not intended as a demo-only path. The broker owns a persistent daily job
ledger and treats Discord/OAB work as asynchronous background activity.

## 3. Scope and Non-Goals

### In scope

- a native or device-appropriate voice client;
- a low-latency conversational voice provider, initially OpenAI Realtime;
- semantic intent drafting and spoken paraphrase-back;
- exactly one spoken confirmation for each new or materially changed
  delegation;
- a dedicated Discord broker bot identity;
- existing OpenAB bot-to-bot dispatch;
- background Discord thread observation;
- spoken progress, completion, and failure summaries;
- multiple concurrent delegation jobs;
- restart-safe job correlation for ordinary daily use;
- a zero-core-change Discord-native integration track; and
- an optional OpenAB-assisted track for structured progress, completion,
  cancellation, and follow-up.

### Explicitly not part of this ADR

- a new OpenAB permission resolver;
- a per-action Discord button or text confirmation;
- speaker biometrics or Discord Voice Channel SSRC attribution;
- a new `ActionBroker` or `AcpActionExecutor` inside OpenAB;
- direct ACP execution by the voice model;
- moving the Discord bot token into the phone client;
- Discord Voice Channel capture, Songbird playback, or meeting transcription;
- defining a new policy for high-risk or irreversible operations; and
- requiring typed ACP completion before the voice workflow can ship.

The optional OpenAB-assisted track adds an observation/control seam around the
existing dispatch path. It does not move intent confirmation into OpenAB and it
does not replace the dedicated broker identity in Discord.

Existing agent capabilities, permissions, and operational policy remain as
they are. This ADR changes the interaction path, not the authority model of the
receiving OAB agents.

## 4. Daily User Experience

### 4.1 New delegation

```text
Operator: "Ask B0 to review PR 12345 with the group."

Broker:   "You want B0 to start a group review of PR #12345, right?"

Operator: "Yes."

Broker:   "Sent. I'll let you know when B0 starts."

Broker:   "B0 has started coordinating the review. Please keep driving;
           I'll follow up when there is a useful update."
```

The confirmation is semantic. The operator confirms the target and objective,
not an exact transcript or generated Discord sentence.

### 4.2 Correction before dispatch

```text
Broker:   "You want B0 to review and merge PR #12345, right?"

Operator: "No, review only. Don't merge it."

Broker:   "Review PR #12345 with the group, but do not merge it. Right?"

Operator: "Right."
```

Nothing is posted until the corrected intent is confirmed.

### 4.3 Status and follow-up

```text
Operator: "How is the PR review going?"

Broker:   "B0 has started three reviewers. Two have replied; one is still
           running. No blocker has been reported yet."
```

Status questions do not require confirmation because they do not create a new
delegation. If the operator materially changes a job, the broker paraphrases
the new intent once before posting the follow-up.

### 4.4 Answering an agent question

When B0 or another agent asks a blocking question, the broker speaks a concise
version and binds the operator's answer to that job:

```text
Broker:   "B0 needs to know whether the review should include the draft PR."

Operator: "Yes, include drafts but don't merge anything."

Broker:   "Tell B0 to include draft PRs and not merge anything. Right?"

Operator: "Right."
```

The confirmed answer is posted into the existing job thread. This uses the same
single intent-confirmation turn as a new delegation; it is not a second approval
system.

### 4.5 Multiple jobs

The foreground voice conversation is not bound to one OAB turn. Every confirmed
delegation becomes an independent background job. The operator can say:

```text
"While that runs, ask B3 to check the staging error from this morning."
```

The broker confirms that second intent, dispatches it, and tracks both jobs.
Short references such as "the PR review" or "the staging check" resolve against
the broker's job ledger rather than raw Discord history.

### 4.6 Interruption and notification style

The voice client should support natural barge-in. The operator can interrupt a
long spoken update, ask for the conclusion first, or say "later".

Background updates are coalesced and spoken only when one of these happens:

- the target agent accepts or starts the delegation;
- the target agent asks a question that blocks progress;
- a material blocker or failure appears;
- the job completes; or
- the operator explicitly asks for status.

Routine reactions, repeated thinking messages, and every individual agent post
are not read aloud.

If there is no active voice session, the broker records blockers and completions
in the job ledger. On the next wake, it gives one compact summary. A conventional
push notification may be added later, but the design does not assume a phone can
start unsolicited background speech.

## 5. Interaction State

There are two independent state machines: one for the foreground conversation
and one for each delegated job.

### 5.1 Foreground intent state

```text
Listening
    │
    ▼
DraftingIntent
    │
    ├─ ambiguous ─────────► Clarifying ───────┐
    │                                         │
    ▼                                         │
ConfirmingIntent ◄────────────────────────────┘
    │
    ├─ correction ────────► DraftingIntent
    ├─ no / timeout ──────► Abandoned
    └─ yes ───────────────► Dispatching
```

Only an explicit affirmative response while one intent is pending advances to
`Dispatching`. An acknowledgement such as "okay" after a progress report must
not accidentally confirm a different future task.

Any command that creates or changes Discord work follows this same rule: a new
task, correction, answer to an agent question, follow-up, reassignment, or
cancellation. Status queries and passive progress reports do not require
confirmation.

If two drafts could be pending, the broker names the task in its question. The
preferred UX is still one active confirmation at a time.

### 5.2 Background job state

```text
Dispatching
    ├─ Discord post failed ─► DeliveryFailed
    └─ root message posted ─► Sent
                                 │
                                 ├─ thread/reaction observed ─► Running
                                 ├─ no response ──────────────► Waiting
                                 └─ final thread result ──────► Completed | Failed
```

The voice session may disconnect while a job is `Sent`, `Waiting`, or `Running`.
Tracking continues server-side. A later session can ask for the current state.

## 6. Components

### 6.1 Device Voice Client

The phone client owns microphone input, audio playback, interruption, and the
device-native activation experience. It should feel like a voice assistant, not
like a mobile Discord client.

For an OpenAI Realtime implementation, use WebRTC from the client. The backend
creates the Realtime session or returns a short-lived client credential; the
normal OpenAI API key remains server-side.

The client does not hold the Discord bot token and does not talk to OAB agents
directly.

### 6.2 Realtime Conversation Engine

The first engine provides low-latency speech-to-speech conversation and emits a
structured proposal such as:

```json
{
  "target": "B0",
  "objective": "Start a group review of PR #12345",
  "references": ["PR #12345"]
}
```

The model-facing tool is named as a proposal, for example
`propose_delegation`. It does not expose a raw `send_discord_message` tool.

A proposal creates `ConfirmingIntent`; it does not dispatch. Application state,
not prompt wording alone, decides when the confirmed intent is posted.

The provider is replaceable. A later STT + dialogue model + TTS pipeline may
emit the same proposal contract without changing Discord or OpenAB.

### 6.3 Voice Delegation Broker

The broker is a small server-side service that owns:

- the foreground session state;
- pending semantic intents;
- confirmation matching;
- Discord posting;
- the persistent delegation job ledger;
- Discord Gateway observation with REST polling as recovery;
- progress coalescing and summarization; and
- spoken event delivery back to the active device session.

It does **not** execute ACP work. Its executable action is only "post this
confirmed delegation to this configured Discord target and track the result."

Suggested records:

```rust
struct PendingIntent {
    intent_id: IntentId,
    voice_session_id: VoiceSessionId,
    target_bot_id: DiscordUserId,
    objective: String,
    references: Vec<String>,
    paraphrase: String,
    state: IntentState,
}

struct DelegationJob {
    intent_id: IntentId,
    target_bot_id: DiscordUserId,
    channel_id: DiscordChannelId,
    root_message_id: DiscordMessageId,
    thread_id: Option<DiscordChannelId>,
    state: DelegationState,
    last_observed_message_id: Option<DiscordMessageId>,
    last_spoken_summary: Option<String>,
}
```

### 6.4 Discord Broker Bot

The broker posts under its own visible identity, for example
`b0-voice-broker`. It never impersonates the human operator.

A delegation message should be concise and explicit:

```text
<@TARGET_BOT_ID> Voice delegation from Can:
Start a group review of PR #12345. Report blockers and the final result here.
```

This gives the channel a natural audit marker: everyone can see that the task
was delegated through the voice broker.

### 6.5 Existing OAB Agents

The receiving agent follows the existing Discord message flow:

```text
broker bot message
  → trusted bot mention admission
  → normal thread creation/detection
  → normal OpenAB dispatch
  → existing ACP coding agent
  → Discord thread result
```

No OpenAB core change is required for the Discord-native track. The deployment
configuration of each directly targetable OAB bot must trust the broker bot ID.

### 6.6 Optional OpenAB Delegation Event Bridge

The OpenAB-assisted track adds a narrow event/control interface to the existing
dispatch lifecycle. It does not become another agent runtime.

The bridge correlates events by the originating Discord message ID, which is
already retained as `ChannelRef.origin_event_id`, and may emit:

```rust
enum DelegationEvent {
    Accepted,
    Queued,
    Running,
    ToolProgress { title: String, status: String },
    Completed { display_text: String },
    Failed { display_text: String },
    Cancelled,
}
```

The transport may be a broker WebSocket connection, a configured webhook, or a
small local event stream. The contract matters more than the initial transport:
events are ordered per origin message, replayable after reconnect, and end in
one terminal event.

The existing Discord message and thread remain the human-visible record. The
event bridge improves correlation and spoken status; it does not make the
handoff invisible.

## 7. Integration Tracks

### 7.1 Shared UX contract

Both tracks use the same sequence:

```text
spoken request
  → spoken paraphrase
  → one spoken confirmation
  → broker-authored Discord delegation
  → background tracking
  → concise spoken updates
```

They also share the same `PendingIntent` and `DelegationJob` records. A job may
start in Discord-native mode and later attach to structured OpenAB events
without being re-dispatched.

### 7.2 Track A — Discord-native, no OpenAB code changes

This is the first delivery track and the compatibility fallback. It changes
OpenAB configuration, not OpenAB Rust code.

#### Receiver configuration

The receiving OAB bot can keep bot messages disabled generally. An explicit
mention from a bot listed in `trusted_bot_ids` already bypasses the normal bot
message mode in the current Discord adapter.

```toml
[discord]
allowed_channels = ["<VOICE_DELEGATION_CHANNEL_ID>"]
allow_bot_messages = "off"
trusted_bot_ids = ["<VOICE_BROKER_BOT_USER_ID>"]
```

The broker must send a real Discord mention token, `<@TARGET_BOT_USER_ID>`, not
plain display text such as `@B0`.

The recommended first topology is to target one orchestrator, B0, per root
message. B0 can use the existing multi-agent flow to coordinate B1-B15. This
keeps one clear thread and one responsibility chain per voice delegation.

#### Thread correlation

After posting the root message, the broker stores `(channel_id, message_id)`.
OpenAB normally creates a thread for the task. The broker observes or refetches
the root message until its thread ID appears, then tracks messages and reactions
inside that thread.

Primary observation uses Discord Gateway events. REST polling fills gaps after
disconnects and supports restart recovery.

#### Observable progress

Without a new OpenAB API, the broker uses Discord-visible evidence:

- successful root-message creation means "sent";
- thread creation or the queued/working reaction means "accepted/started";
- thread messages provide questions, progress, and final text; and
- completion/error reactions plus final thread output provide a terminal UX
  signal.

The broker should describe only what it observed. For example, say "B0 reports
the review is complete" rather than claiming an independent ACP-level proof.
For a cancellation, Track A says "I asked B0 to stop" until Discord shows the
result; it does not claim that the underlying turn has already stopped.

### 7.3 Track B — OpenAB-assisted structured lifecycle

This track keeps the exact same Discord message path, then supplements it with
structured events from the receiving OpenAB instance.

```text
b0-voice-broker ── Discord message ──► B0 / normal ACP path
        ▲                                  │
        │                                  ├─ Discord thread remains visible
        │                                  │
        └──── DelegationEvent stream ◄─────┘
```

The broker subscribes using the root Discord message ID. OpenAB reports queue,
turn, tool-progress, terminal, and delivery states without requiring the broker
to infer all of them from reactions.

This track may additionally expose exact controls keyed by the same origin:

- cancel the currently active turn;
- send a confirmed follow-up into the same OAB session/thread; and
- request the latest structured status after reconnect.

Intent confirmation still happens on the phone before Discord dispatch. A
material job-changing command such as cancel or follow-up uses the same single
spoken paraphrase/confirmation rule; it never adds a text approval step.

Track B may say that an exact turn cancellation was requested or confirmed only
when the structured lifecycle reports that state.

### 7.4 Comparison and upgrade path

| Concern | Track A: no OpenAB code change | Track B: OpenAB-assisted |
|---|---|---|
| Dispatch | Trusted Discord bot mention | Same trusted Discord bot mention |
| Audit | Root message + OAB thread | Same root message + OAB thread |
| Progress source | Reactions and thread messages | Structured events plus Discord |
| Completion wording | "B0 reports complete" | Can report typed terminal state |
| Cancellation | Natural-language Discord follow-up | Exact active-turn control |
| OpenAB work | Configuration only | Event/control API and lifecycle wiring |
| Rollout role | First daily-driver and fallback | Reliability/precision upgrade |

Track A is not throwaway code. The broker always retains Discord observation so
it can continue operating when the structured event bridge is unavailable.

## 8. Smooth-UX Rules

1. **One confirmation, once.** A new or materially changed delegation gets one
   short paraphrase-back. Status queries and spoken reports do not.
2. **Confirm meaning, not wording.** Read back target, objective, and key
   constraints; never read a full generated Discord message unless asked.
3. **Dispatch immediately after yes.** Do not introduce a second text or button
   approval.
4. **Keep listening.** Dispatching a job must not block the foreground voice
   conversation.
5. **Track multiple jobs by name.** Resolve "the PR review" and "the staging
   check" against a persistent ledger.
6. **Speak deltas, not logs.** Coalesce repeated Discord activity into one short
   update.
7. **Be truthful about handoff state.** Distinguish "sent", "B0 started", and
   "B0 reported completion".
8. **Support correction naturally.** "No, review only" edits the pending intent
   and triggers one new paraphrase.
9. **Allow barge-in.** The operator can interrupt speech, ask for the conclusion,
   defer an update, or start another task.
10. **Recover without re-dispatching.** Reconnect and restart resume observation
    from the stored Discord IDs.
11. **Use one confirmation rule everywhere.** New tasks, corrections, answers,
    follow-ups, reassignments, and cancellations each receive one semantic
    paraphrase; status queries and passive reports receive none.

## 9. Relationship to Other Voice Proposals

### Issue #1364

#1364 proposes a voice bridge around OpenAB/GPT-Live-style interaction. This ADR
keeps the useful foreground/background split but makes the handoff explicit:
the voice assistant first creates and confirms a semantic intent, then delegates
through a distinct Discord broker identity. It does not forward a raw transcript
or pretend that the operator typed the message.

### Issue #1368 and the Discord Voice receive ADR

#1368 and [the Discord Voice receive ADR](discord-voice.md) are channel-bound:
OpenAB joins a Discord Voice Channel, receives several participants, associates
audio with Discord speakers, and may summarize that meeting.

This ADR is device-bound: the voice conversation happens on the operator's
phone, outside Discord Voice. The broker enters Discord only after the operator
confirms the semantic delegation.

The two paths are independent. Discord Voice receive remains useful for meeting
transcription; it is not a prerequisite for the hands-free daily assistant.

### Typed ACP turn work

The experimental typed ACP turn/completion refactor on this branch is also
orthogonal to Track A and is not a release prerequisite. It can supply the
authoritative completion boundary for Track B, where OpenAB emits structured
execution and delivery events. Neither track creates a direct ACP executor in
the voice broker.

## 10. Prior Art Applied

### OpenAI Realtime

The Realtime API supports stateful speech-to-speech sessions and function calls.
For client applications such as mobile devices, the official guide recommends
WebRTC and supports server-minted short-lived client credentials. In this
design, Realtime produces a `propose_delegation` event; the broker application
stores it, performs the spoken intent-confirmation turn, and only then calls
Discord.

### Voice assistant delegation

The UX follows the familiar assistant pattern:

```text
request → concise paraphrase → yes/correction → background execution → update
```

The assistant is responsible for translating the operator's intent into a clear
delegation. The operator is not expected to edit or approve its prose.

### Existing OpenAB multi-agent routing

OpenAB already supports trusted bot mentions, multi-bot threads, turn limits,
thread creation, normal ACP execution, and final Discord delivery. The broker
uses that public interaction surface instead of adding a second agent runtime.

## 11. Current State

Confirmed on 2026-07-12:

- OpenAB's Discord adapter admits an explicit mention from a bot in
  `trusted_bot_ids`, even when `allow_bot_messages = "off"`;
- a top-level admitted message enters the normal thread and ACP path;
- sender context retains the broker Discord ID and `is_bot = true`;
- reactions and thread messages are observable progress signals;
- existing Discord/OpenAB metadata provides a deterministic root-message to
  thread correlation seam, with broker end-to-end validation pending; and
- the current local Kubernetes environment can host another small broker
  service.

For Track B, the branch contains an in-progress typed ACP turn/completion seam,
but no delegation event transport or origin-keyed control API has been wired to
the voice broker.

Not yet implemented:

- the mobile voice client;
- Realtime session setup;
- the intent-confirmation state machine;
- the Discord broker bot service;
- the persistent job ledger; and
- spoken background progress/reporting.

## 12. Implementation Plan

### Track A1 — Text-driven broker and Discord handoff

Build the broker service and exercise it with a small local text/fake-voice
client before connecting audio.

- persist pending intents and delegation jobs;
- implement `DraftingIntent → ConfirmingIntent → Dispatching`;
- post as the dedicated broker bot;
- target B0 with a real mention;
- correlate the created thread;
- observe progress through Gateway events with polling recovery; and
- deploy the broker in `docker-desktop/openab-local`.

This phase proves that OpenAB itself needs no code change.

### Track A2 — Daily mobile Realtime client

- add the device voice client;
- create OpenAI Realtime WebRTC sessions from the broker backend;
- expose only a structured `propose_delegation` function to the voice model;
- implement concise spoken paraphrase-back and correction;
- dispatch immediately after the single spoken confirmation; and
- keep the session conversational while jobs run.

### Track A3 — Background job experience

- support several concurrent jobs;
- add natural job references and status questions;
- coalesce Discord activity into useful spoken deltas;
- speak blockers, questions, completion, and concise failure summaries;
- support barge-in, "tell me later", and "give me the short version"; and
- resume tracking after app, network, or broker restart.

### Track A4 — Daily-use soak and refinement

- use the assistant for normal PR review, test, investigation, and coordination
  flows;
- validate Bluetooth/headset and driving interaction;
- tune paraphrase length and update frequency;
- measure false dispatches, duplicate dispatches, missed updates, and unwanted
  interruptions; and
- add an OpenAB status protocol only if Discord-visible correlation proves
  insufficient.

### Track B1 — Structured OpenAB delegation events

- define an origin-message-keyed `DelegationEvent` contract;
- emit queue/start/tool/terminal/delivery events from the existing dispatch and
  typed turn seams;
- expose an ordered, reconnectable stream to the broker;
- preserve the existing legacy Discord behavior when no subscriber exists;
- let the broker attach structured events to an already-created Track A job;
  and
- deploy and validate the enhanced target agent in local Kubernetes.

### Track B2 — Exact control and follow-up

- expose current-turn status by origin message ID;
- connect broker cancellation to the exact active turn ticket;
- allow a confirmed voice follow-up to enter the same Discord/OAB thread;
- return a typed result while continuing to post the normal Discord result; and
- fall back to Track A behavior whenever the native control channel is absent.

## 13. Acceptance Scenarios

1. The operator delegates a group PR review, hears one concise paraphrase, says
   yes, and the broker posts to B0 without requiring the phone screen.
2. The operator corrects "review and merge" to "review only"; only the corrected
   intent reaches Discord.
3. The operator asks for status and receives an answer without another
   confirmation turn.
4. A second task is delegated while the first is running; both continue and can
   be referenced naturally.
5. After posting, the broker says "sent"; only after thread/reaction evidence
   does it say B0 started.
6. The broker does not read every Discord message aloud. It reports only a
   blocker, question, meaningful milestone, completion, or requested status.
7. The operator interrupts a spoken update and immediately starts another
   request.
8. A Discord post fails; the broker says it was not sent and can retry without
   creating a duplicate job.
9. The device disconnects after dispatch; server-side tracking continues and a
   later voice session can ask for the result.
10. A broker restart resumes from stored root/thread/message IDs without
    re-posting the delegation.
11. The Discord audit clearly shows `b0-voice-broker` as the author and the
    operator as the stated delegator.
12. Existing B0-B15 coordination and ACP execution work without an OpenAB core
    patch; only receiver configuration changes.
13. The same voice script passes against the OpenAB-assisted track without any
    additional operator confirmation or different spoken command.
14. If the structured event stream disconnects, the broker continues tracking
    from Discord without re-dispatching the job.
15. In the assisted track, a spoken cancellation maps to the exact active turn
    and the terminal spoken report comes from the structured result.

## 14. Consequences

### Positive

- The operator gets a genuinely hands-free daily workflow.
- Confirmation fixes misunderstanding without forcing screen interaction.
- OpenAB agents retain all existing coding and program-execution capabilities.
- Discord remains the shared coordination and audit surface.
- The broker's distinct bot identity makes voice-originated delegation visible.
- Long-running work does not freeze the foreground conversation.
- The first implementation is much smaller than an in-core voice action
  subsystem.
- Realtime, STT, and TTS providers remain replaceable behind the broker.
- OpenAB-assisted status can be added later without replacing the working
  Discord-native daily UX.

### Trade-offs

- The broker infers progress from Discord presentation-level events rather than
  a structured OpenAB job API in Track A.
- Receiver bot configuration must trust the broker and allow the target channel.
- Good daily UX requires persistent job state, reconnection, and careful spoken
  notification design.
- The phone client and broker are additional deployable components even though
  OpenAB core remains unchanged in Track A.
- Track B adds an event/control surface that must remain compatible with the
  existing message path and legacy callers.

## References

- [OpenAI Realtime API with WebRTC](https://developers.openai.com/api/docs/guides/realtime-webrtc)
- [OpenAI Realtime conversations and function calling](https://developers.openai.com/api/docs/guides/realtime-conversations)
- [OpenAB Discord multi-agent guide](../multi-agent.md)
- [OpenAB Discord bot admission](../discord.md#trusted_bot_ids)
- [Issue #1364](https://github.com/openabdev/openab/issues/1364)
- [Issue #1368](https://github.com/openabdev/openab/issues/1368)
