# Native sessions on the persistent runtime and the public protocol (issue #489)

**Date:** 2026-09-13 · **Issue:** #489, step **N20 of 23** of the native-runtime roadmap (#469); closes the two residuals issue #352 left open · **Status:** implemented behind the same experimental operator-only gate as #352

## 1. Context

Two pieces already existed and were deliberately kept apart.

Protocol v1 (#353) published a narrow, versioned local API — redacted session
facts, a small method table, capability negotiation, a server-wide revision
with a gap rule, owner-only transport — and shipped with **no daemon** and no
in-tree consumer. The persistent runtime (#352) then made `zirv session serve`
the owner of the PTYs, with every UI a client of it over that protocol.

Neither of them knew anything about native sessions. A native conversation
(#478/#479, N09/N10) has a journal instead of a pseudoterminal: there is no
screen to repaint, no keystroke to forward and no child process to supervise,
but there *is* durable state, a generation fence, a turn that can be cancelled,
an approval that can be decided and a conversation that can be read. Reaching
any of that meant calling into the binary's own internals.

Issue #352 also left two residuals in writing: the dashboard was still its own
PTY owner, and mail into a detached session queued until a client attached.

## 2. Decision

One runtime, one endpoint, one protocol, **two hosts**.

```
ApiServer
├─ SessionHost   (#352)  session::host::RuntimeHost   — the terminals
└─ NativeHost    (#489)  session::native::NativeSessions — the conversations
```

### 2.1 A second trait, not a second daemon and not a fatter first trait

`NativeHost` is a separate trait from `SessionHost` because the two own
genuinely different things. A pty host can be resized, typed into and
screen-read; none of those verbs mean anything for a conversation. A native
host can be interrupted mid-turn, have an approval decided, and have its
journal paged by durable cursor; none of those mean anything for a supervised
harness process. One `ApiServer` holds both, `zirv session serve` attaches
both, and `HostSource` publishes both into **one** session list — which is what
makes "one versioned local runtime" true rather than two daemons sharing an
endpoint.

Routing is by ownership, never by a session's advertised `runtime` field: the
server asks `native.owns(session_id)` before it reaches for either host, so a
registry record that merely *says* `native` cannot redirect a call to a host
that never opened it.

### 2.2 Five new methods, and two verbs deliberately not added

`session.interrupt`, `session.approve`, `session.task_result`,
`session.history`, `session.journal`, all gated on one new capability,
`session.native`.

Submit and steer are **not** here. They are `session.send_input`'s existing
`submit`/`steer` modes, routed to whichever host owns the session. A second way
to say "here is input for this session" would be a second authorization path to
keep in step with the first, which is exactly how the controller rule gets
holes in it.

The capability is advertised only by a server with a native host attached, so:

- a client that predates native sessions negotiates the whole surface away and
  never calls it (`client-previous-minor.json`, unchanged);
- a native-capable client meeting an older server disables the surface in
  **itself** and reports it as `client_only` (`client-native.json`, new).

Both directions are tested. The frozen request/response/event fixtures under
`tests/fixtures/protocol/v1/` are untouched and still replay byte-for-byte —
which is the point of putting new methods behind a capability a fixture-era
server never advertises.

`session.send_input` gained two OPTIONAL result fields (`message_id`,
`duplicate`), present only for a native session, where the acknowledgement is
durable and therefore has an identity to report. A pty keystroke has neither.

### 2.3 Idempotency that survives the thing it exists for

Protocol v1's `idempotency_key` was a bounded in-memory cache (256 keys). That
is the right amount of machinery for a retry on a live connection and the wrong
amount for the case the criterion actually names: a reconnect after the service
restarted, which loses every cache there is.

So for a native session the key becomes the journal's own `MessageId`
(`idem-<sha256 of the key>`), and the journal already carries
`UNIQUE(session_id, message_id)`. A retry hits a constraint **on disk**, writes
nothing, queues no second turn, and is reported back as `duplicate: true` with
the same `message_id`. The digest is cryptographic on purpose: a cheap hash's
collision would silently drop a genuinely different input as a duplicate, which
is the one failure mode the whole mechanism exists to prevent.

`accept_input` also does *not* refuse a session with a turn in flight, unlike
`RuntimeBackend::submit`. The agent loop drains everything unconsumed at the
next delivery boundary, so refusing would reject a message it was about to
deliver anyway. Whether a turn is running is the HOST's fact — it is what
spawned the runner — not the backend table's.

### 2.4 Cursors and gaps: the durable counterpart of the revision rule

The server-wide `revision` is a live-connection signal and resets with the
process. A conversation's journal `sequence` does not. `session.journal` pages
by that sequence, bounded to 256 events a page, and answers `gap: true` when
the caller's cursor is one the journal can no longer be continued from —
either below the oldest retained sequence, or ahead of the newest, which means
whatever that cursor came from is not this conversation as it now stands. The
rule (`session::native::gap_at`) is pure and tested on its own.

Event payloads never travel: a page carries the sequence, the generation, the
event kind and a short redacted descriptor (a tool's name, an execution's
state). Tool arguments and results routinely carry file contents and
credentials.

### 2.5 Conversation text has exactly one door

`session.history` is the only method in v1 that publishes conversation text,
and a test pins that property over the whole method table. It is gated on the
native capability, it is seat-checked like a mutation, and tool entries carry
the tool's **name** and nothing else. `SessionFacts` is unchanged, so no
snapshot, list or event frame carries a word of it — the "no transcript bodies
in snapshots by default" rule stays enforced by the types rather than by a
filter someone has to remember.

### 2.6 The controller rule, and the hole it would otherwise have

"Only the current controller can submit/approve/resize; observers cannot
mutate" has two halves, and the obvious implementation only closes one.

A session **nobody has attached to** is driven by whoever can reach the
owner-only endpoint — which is exactly the rule that applied before this issue,
and the rule a headless `zirv ctx exec` needs. The moment any client attaches,
seats exist to arbitrate, and every native mutation must name a `client_id`
holding the controller seat. So an observer naming itself is refused because it
is not the controller, and an observer **omitting** the field is refused
because a session with clients requires one. Without the second half,
`client_id` would be optional and therefore optional to skip.

`session.screen` and `session.resize` on a conversation, and `mode=raw` input,
are refused by name rather than with "this server owns no terminals" — which
would be false of a runtime holding plenty, just not one for this session.

### 2.7 Four operations, four effects

| verb | what it touches | what it never touches |
|---|---|---|
| `session.detach` | this client's entry in `clients`/`controller` | the conversation, its journal, its registry record, the turn in flight |
| `session.interrupt` | the cancellation flag the running turn shares | the session, which stays alive and goes idle |
| `session.stop` | completes the journal session, releases the registry guard | other sessions |
| service shutdown | drains the topology | every session, unless `--stop-sessions` |

A detached native session keeps running under policy, pacing, health, mail,
workflow gates and writer claims for one mechanical reason: the service files
the ordinary **registry record** and holds the guard, exactly as `RuntimeHost`
does for a pty. Every one of those subsystems reads the registry.

The record is filed `unreachable()`: a native conversation binds no turn-signal
socket, because there is no harness hook to post to one. Saying so is honest;
claiming reachability would make a wake-up look deliverable when nothing could
ever act on it.

### 2.8 Restart: reconcile, never replay

`NativeSessions::restore` walks **this runtime's own durable topology**
(`<state>/runtime/<name>-native.json`), not every session the journal happens to
hold — advancing the generation of a conversation a concurrent `zirv ctx exec`
is driving would fence that process out of its own session. For each entry, in
this order:

1. read the stored identity, which fails loudly for a session the journal has
   never heard of rather than inventing one;
2. convert every execution whose last durable state is `Started` into
   `OutcomeUnknown` — an effect that began and never reported cannot be assumed
   to have failed, so it is never silently retried;
3. advance the generation, fencing any straggler still holding the old one out
   of the journal and out of the execution broker.

Reconciliation is written as the OLD generation, which is the generation those
executions actually belong to. Nothing is re-submitted: a restored conversation
is idle and drivable, `zirv session serve` names every outcome-unknown
execution on startup, and what could not be read back at all is reported as
lost with the reason. No shell process is ever described as having survived.

### 2.9 Turns run in the service, through the shared loop

`native::run_hosted_turns` is the one new entry point: it builds the transport,
the broker and the agent loop through the SAME `build_transport` /
`brokered_tools` a headless run uses, and differs only in what it must not do —
it neither creates the journal session, nor resumes it, nor completes it. The
service created it, holds its generation, and the conversation outlives the
turn. Advancing a generation per turn (what a resume does) would fence the
service out of its own session; completing it would end a conversation nobody
asked to end.

The writer permit is acquired **per turn** and released with it. A lease is the
right to write one tree, and holding one for an idle conversation would block
every other worker on that checkout for as long as the operator left the
session open.

Route resolution, the provider call and the permit all sit behind one injected
`NativeEnvironment` trait, so the runtime's own bookkeeping — identity,
durability, seats, cursors, reconciliation, the controller rule — is tested for
real against a real journal and a real registry without a provider, a
credential, a network or a model.

## 3. The two #352 residuals

### 3.1 The dashboard is no longer its own PTY owner where the runtime is one

`dash::link::RuntimeLink` is the dashboard's whole reach into a runtime-owned
session: facts, the attachment table, `session.screen`, `session.journal`,
raw input and native submit, over the ordinary protocol v1 client. It opens no
pty, spawns no child and files no registry record.

The ownership rule (`link::ownership`, pure) is enforced at startup: with the
gate on and a runtime listening, `run_dashboard` asks whether the runtime
already holds this repository's seat and **refuses to open a second terminal
over it**, naming `zirv session attach`. Two supervisors on one conversation is
what the runtime exists to prevent. A gate that is on with nothing listening —
a stopped, restarting or namespace-refused service — leaves the dashboard
owning its own terminals, because that is a working mode and being stranded is
not.

*Painting* a runtime-owned session inside a pane is step N11 (#480), which owns
pane rendering and is being built in parallel. The transport is finished and
tested here so that step is a pane change rather than a protocol change.

### 3.2 Mail reaches a detached session

Mail addressing already worked headless — the service files the registry
record, so a sender could always reach a detached session — but the dashboard
was still what typed a delivered message into a pane. The service owns the
terminal, so the service now types into it, on the heartbeat, through the
dashboard's OWN sweep (`dash::sweep_one_pane` for a worker's body delivery,
`dash::advise_one_pane` for an orchestrator seat's advisory) rather than a
second delivery path with its own trust framing, caps and consumption rules.

The two-phase injection a pane performs — the text, then a deferred carriage
return — moves into the host's pump loop, so delivering never blocks the caller
for the settle delay, and the in-flight witness is stamped exactly as an
operator's keystroke stamps one.

A native conversation has no terminal to type into, so delivery there is what
it should always have been: the message becomes a journalled input keyed on the
delivered text, so a re-delivery after a crash between the injection and the
consume records nothing twice.

## 4. What is verified

Tests are named in `api::{wire,client,server}`, `session::{native,host,service}`
and `dash::link`.

- **Ownership and supervision.**
  `detaching_every_client_leaves_the_conversation_and_its_registry_record`,
  `an_interrupt_cancels_the_turn_and_a_stop_ends_the_conversation`,
  `interrupt_cancels_a_turn_and_stop_is_the_only_verb_that_ends_one`.
- **Durable idempotency.** `a_retried_input_with_the_same_key_is_recorded_once`
  (proven by the journal's own message count, not by the reply) and
  `a_retried_native_input_carries_its_key_to_the_host`.
- **Cursors and gaps.** `a_journal_page_pages_by_cursor_and_publishes_no_payloads`,
  `a_cursor_the_journal_cannot_continue_from_is_a_gap`.
- **Seats.** `only_the_controller_drives_a_native_session_and_observers_cannot_mutate`
  (including the omitted-`client_id` half),
  `many_clients_may_observe_a_conversation_but_only_one_may_drive_it`.
- **Restart.** `a_restart_reconciles_started_executions_instead_of_replaying_them`
  — a successor over the same journal reports the started execution as
  outcome-unknown, submits nothing, and records the predecessor generation.
- **Negotiation, both directions.**
  `a_native_capable_client_and_a_previous_one_each_degrade_explicitly`, against
  two committed client fixtures.
- **Real transport** (a named pipe on Windows, a unix domain socket elsewhere):
  `a_served_runtime_serves_the_native_surface_over_the_real_transport` and
  `the_dashboard_drives_a_runtime_session_as_a_protocol_client`.
- **Privacy.** `conversation_text_is_reachable_only_through_session_history`,
  and the unchanged `session_facts_publish_no_bodies_secrets_or_absolute_paths`.
- **Mail.** `mail_reaches_a_session_no_client_is_attached_to` (a pty session
  with zero clients) and
  `mail_for_a_detached_conversation_is_delivered_rather_than_queued`.
- **Wire compatibility.** The frozen v1 fixtures replay byte-for-byte,
  unchanged.

## 5. What is deferred

- **Rendering a runtime-owned session inside a dashboard pane** — step N11
  (#480). The transport is here; the pane is not.
- **Worker seats on the runtime.** `session::host::launch_spec` still opens
  orchestrator seats only; a worker pane's task prompt, work group, budget and
  report address are assembled by `dash::fulfill_spawn_request`. Native
  conversations have no such restriction, because a native worker's ownership
  is already `ctx::delegation`'s (N10).
- **Interactive approvals end to end.** `session.approve` records the
  controller's decision durably and in memory, and the seat rule is enforced;
  the execution broker still runs hosted turns in `ApprovalMode::Headless`, so
  nothing yet *raises* an approval request over the protocol. Turning that on
  is a broker change (N04), not a protocol one.
- **`#[cfg(unix)]` paths.** Nothing in this change is unix-only, but the unix
  halves of `api::transport` and `session::host` it runs over could not be
  compiled on the Windows machine this was developed on; CI is the evidence.
- **Live event push for native sessions.** `events.subscribe` still carries the
  server-wide session-fact stream; a conversation's own events are pulled by
  cursor. A push stream is worth adding when a client is paying for the polling,
  and none is yet.
