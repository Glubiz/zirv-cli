# The native agent loop (N09)

**Date:** 2026-09-13 · **Issue:** #478 · **Roadmap:** #469

## Context

N02-N08 shipped every piece a native session needs except the thing that
puts them together: provider routes and credentials (#471), an authoritative
journal (#472), an execution broker (#473), coding tools (#474), a context
compiler (#475), and two direct provider transports (#476, #477). Nothing
drove them. `runtime::select(RuntimeKind::Native, ..)` still returned
`RuntimeError::Unsupported`, and every lifecycle decision zirv makes about a
running session -- may this tool run, should this result be replaced, may
this session stop -- was reachable only by feeding harness-shaped JSON to
`hook.rs`, which a native session has no way to produce.

## Decision

Two new modules, one extraction, and a small set of operator-only flags on an
existing verb.

### `ctx::lifecycle` -- the shared decision services

Pure, payload-neutral, no fs/clock/env/net of its own (the harness-home
exemption takes an injected `EnvLookup`, exactly as the hook path already
did). It owns the before-tool admission (the expensive-seat subagent guard
and the orchestrator-write posture), the after-tool disposition, the prompt
notes, the stop decision, the verification decision and the notification
classifier.

`hook.rs` keeps every behaviour, envelope, state write and test it had; it is
now literally the payload translator its own comments already claimed it was.
`runtime::native` calls the same functions directly. That is what makes
acceptance criterion (f) -- native operation needs no installed coding
harness, lifecycle decisions included -- provable rather than asserted: the
test that runs a whole native session under an env lookup answering PATH as
empty and everything else as absent would fail if any decision still needed a
harness binary or a hook process.

### `runtime::native` -- the loop

An explicit four-level state machine (`SessionState` / `TurnState` /
`RequestState` / `ToolState`), with `ToolState` mirroring the journal's own
`ExecutionState` one-for-one so the in-memory view and the durable record can
never disagree about what happened.

**The request is a projection of the journal, not of memory.** Every request
is rebuilt from `Journal::replay`, so a crash takes no conversation state with
it and "what did we send" is answerable from disk. Tool results follow their
assistant message in the provider's own declared block order.

**The barrier.** A tool executes only after its arguments parsed as a complete
JSON object, the shared before-tool service admitted it, and the assistant
message plus the tool-call record are durably committed. A truncated argument
stream therefore cannot become an effect -- the loop drops a `tool_use` block
whose `input` is not an object rather than repairing it -- and a crash between
"committed" and "executed" leaves a `Prepared`/`Started` record the next open
reconciles instead of a silent gap.

**Scheduling vs. ordering.** A call is independent when it claims nothing but
read roots, the output store and the search index, and is neither a background
process nor process control; an unknown tool is dependent by default (fail
closed -- never reorder something this build cannot classify). Independent
calls run first, in declared order, then the rest in declared order. Results
are always rebuilt in the provider's declared order keyed by call id, so
completing out of order is invisible on the wire.

**Input, steering, interruption.** Every accepted input is durably
acknowledged before anything else can happen to it, through one
`acknowledge_input` shared by the loop's own `acknowledge` and by
`NativeBackend`'s `submit`/`steer`/`resume` -- written *before* the caller is
told the input was accepted, so a crash immediately after `Ok` still finds it.
Delivery boundaries are
explicit: an acknowledged input joins the conversation at the next request
built for the session -- between requests inside a turn, or between turns --
never mid-stream and never mid-tool. `delivered_through` is a journal sequence,
so "delivered once, or still queued" is decidable from durable state alone and
survives a restart. An interrupt cancels the in-flight stream, every unstarted
tool and every remaining turn; it deliberately does not cancel an effect
already in progress, which becomes `OutcomeUnknown`.

**Two different retry budgets.** A response retry re-sends a request that
committed nothing, and is therefore free; it applies only to failure classes
that cannot have produced an effect (transport, overload, rate limit, the two
timeouts) plus anything the provider itself marked retryable, and never to a
cancellation, a refusal, an authentication problem or a context overflow. A
tool-effect retry is not free: only a tool whose own contract says `Safe` is
ever re-run, each attempt gets its own execution record (reusing the id would
both be rejected by the journal's state machine and hide that a second effect
happened), and an `OutcomeUnknown` result is never replayed at any budget.

**A turn is a real unit.** One turn is one acknowledged input driven to the
point where the model stops asking for tools, so `max_turns` bounds how many
separate things a session was told to do. The bound is enforced in `run_turn`
itself rather than in `run_to_completion`, so it holds for any driver -- an
interactive surface stepping turns by hand is bounded exactly as the headless
loop is -- and `run_to_completion` runs another turn when input was queued
during the last one, which is the delivery boundary that makes the count
meaningful.

**The finish token does not decide.** `NativeFinalStatus::status` is
`Completed` only when the model finished AND no execution is non-terminal AND
none is outcome-unknown AND no acknowledged input is undelivered AND nothing
was interrupted or limited AND the shared stop service does not block. A
blocking stop decision -- today reachable through
`NativeSessionConfig::workflow_gate`, which N15 will populate -- outranks the
token outright, so that wiring cannot land without taking effect. It
carries the actual route, the configured *and* served model (a route alias and
the model that answered are two different facts), usage, and evidence rows a
reader can go and check.

**Bounded output.** A tool result past `max_tool_result_bytes` is stored whole
as a journal artifact and replaced with a bounded head/tail extract naming its
retrieval id -- the same "never let the summary be the only copy" rule the
PostToolUse compaction hook already follows.

### `zirv ctx exec --runtime native`

Explicit and opt-in. An unrecognised `--runtime` is an error, never a silent
fallback. The harness-only flags (`--agent`, `--transcript`, `--session-id`,
`--max-restarts`) are refused rather than ignored, because a native session
supervises no process, has no transcript to score and nothing to restart. The
output is one structured JSON final status and the same supervisor exit codes.

`--resume <session>` continues a stored session through `resume_journal`,
which is the only correct order and the reason N03 shipped
`reconcile_started_as_unknown` and `advance_generation`: read the stored
identity, reconcile every durably-`started` execution as `outcome_unknown`
(written as the OLD generation, which is the one those effects belong to), then
advance the generation so the previous one is fenced out of the journal and the
broker. A resumed loop's minted ids are namespaced by generation -- its own
counter restarts at zero, and the journal rejects a duplicate usage or
execution id, which the first real resume surfaced immediately.

`--provider fixture:<path>` and `--fixture-tools <path>` make the deterministic
fixtures reachable from the shipped command rather than only from
`#[cfg(test)]`, so a whole native session runs end to end with no provider
configured, no credential and no installed harness. Operator-only by
construction: command-line flags, settable by no configuration layer.

### `runtime::fixture`

Production-compiled for the same reason `fake.rs` is. A `FixtureProvider`
replays scripted turns in either primary protocol's streaming shape (the
committed response is identical across shapes -- that is the point of the
provider-neutral contract -- but the observable event sequence is not), and a
`FixtureToolExecutor` replays scripted receipts while recording the order it
was actually called in. Scripts live under `tests/fixtures/runtime/native/`,
pinned to LF like the other runtime fixture sets.

## What is verified

Deterministic, no network, no paid call, no filesystem effect:

- A multi-turn investigate/edit/test session per primary provider shape, with
  the expected request count, tool order, served model and summed usage.
- A truncated tool-argument stream that never executes; an empty response, a
  refusal and interleaved text/tool blocks each settling to an explicit state.
- Two disconnects retried inside the response budget, and the same script
  failing explicitly with the budget at zero.
- An acknowledged input reaching the conversation exactly once; steering
  accepted after the last request staying visibly queued.
- Journal event order proving the assistant message precedes the tool-call
  record, which precedes the first `Started` execution.
- A policy-denied tool that is never executed and whose reason reaches the
  model.
- A `Safe` tool failure retried once and succeeding; a `Reconcile` tool
  reporting an unknown outcome executed exactly once and forcing an
  `Incomplete` final status over the model's own `end_turn`.
- A whole session running under an env lookup that answers PATH as empty and
  everything else as absent.
- A continuation request replaying each call's LATEST execution, so a
  successful retry's result is what the model sees.
- A crash fixture: an execution left durably `started`, resumed, reconciled as
  outcome-unknown, never re-run, with the old generation fenced out.
- Backend-accepted input (`submit`/`steer`/`resume`) landing in the journal in
  order, with the resume's own input against the new generation.
- A blocking workflow gate producing `Incomplete` over an `end_turn` token.
- An unclassifiable tool never retried.
- The whole fixture path driven through `exec::run_with` -- `--provider
  fixture:` with `--fixture-tools`, and the same command resumed.
- The tool-call and wall-clock ceilings each stopping the loop and naming
  themselves.

## What is deferred

- **Concurrency.** Independent calls are *scheduled* first but still run one
  at a time. The ordering contract and the result-reassembly are what a
  parallel executor would need, and are in place; actually running two effects
  at once is not part of this step.
- **Live provider runs.** Both transports are fixture-verified here, exactly
  as N07/N08 left them. No live native session has been run against a paid
  account from this step.
- **Steering while a turn is in flight from another thread.** `NativeBackend`
  hands out the session's `Arc<CancellationFlag>` and accepts steering at any
  time, and the loop drains at its delivery boundaries, but the headless entry
  point drives one loop synchronously on one thread; the interactive surface
  that would exercise cross-thread steering is N11's.
- **Workflow gates in the stop decision.** `StopSignals::workflow_gate` exists,
  is carried on `NativeSessionConfig` and outranks the finish token in the
  status ladder, but nothing populates it yet -- wiring the workflow engine
  into a native session's stop is N15's.
- **Tasks, mail and delegation.** `NativeSessionConfig::task` is carried into
  every journal scope, but a native session does not yet accept or dispatch
  work; that is N10.
