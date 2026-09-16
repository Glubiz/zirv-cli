# Native compaction, rot recovery and portable checkpoints (N17)

**Date:** 2026-09-13 · **Issue:** #486 · **Roadmap:** #469

## Context

N09 gave a native session a loop, N03 gave it a durable journal, and N06 gave
it a budgeted context compiler. None of them gave it a way to survive its own
length. Compaction and resume were still adapter capabilities:
`exec::action_for_signal` compacts a session only when
`adapter.supports_headless_compact()`, and rot scoring only ever saw an
external harness's transcript. A native session has no transcript, no
`/compact`, and no harness process to restart -- so a long native task ended
at the context window.

It also had two facts no transcript-scored session has ever had: the model's
declared context window, and the output reservation the run actually asked
for. Copying the shipped absolute token thresholds (100K/160K) into the
native path would have thrown both away.

## Decision

Two new modules, one loop hook, one status section, one narrowing config key.
`rot.rs` is not touched.

### Observation: a projection, not a second engine

`runtime::compaction::observe` reads `Journal::normalized_events` -- the lossy
scoring projection N03 already shipped -- and scores it with the existing pure
`rot::signals` / `rot::score_from`. The projection is an adapter at exactly
the same boundary as every harness transcript parser, so identical projected
events give identical verdicts and every existing scoring regression keeps
passing unchanged.

Two things are native-specific, and both are inputs to the pure engine rather
than changes to it:

- **Capacity.** `NativeBudget::capabilities()` hands the engine
  `context_window_tokens = window - output_reserve`. The existing
  `score.token_floor_ratio` / `token_ceiling_ratio` then derive the gate from
  the capacity that is actually available for input. An unknown window stays
  `None`, which `rot::token_gates` already reads as "use the absolute
  fallbacks" -- never as a guess.
- **Measurement.** The figure is the newest committed *assistant message*'s
  usage, summing fresh input, cache writes and cache reads. Cache reads are
  included because a cached prefix still occupies the window; keying off the
  assistant message rather than the newest usage row is what keeps a
  distillation's own (deliberately small) request from looking like the
  context suddenly shrinking.

Provider context-overflow refusals are not journal facts -- a refused request
commits nothing -- so the loop counts them and the projection appends them as
`NormalizedEvent::ProviderError { class: Overflow }`. Inventing a journal
event for something that never reached the conversation would have corrupted
replay to buy a scoring signal.

### Decision: typed triggers, narrowing policy

`compaction::evaluate` is pure and returns a `CompactionDecision` carrying
explicitly typed `CompactionTrigger`s -- `context_overflow`,
`token_pressure`, `repeated_identical_errors`, `loss_of_progress` -- not one
opaque verdict. Overflow and token pressure are capacity facts and force a
compaction; the two behavioural triggers and the weighted score alone are
advice until the rot engine's own gate escalates.

`native.toml`'s `[policy].compaction` is `automatic` (the default: compaction
is how a native session survives a long task) or `advisory` (zirv reports and
does nothing). It is the second key a repository layer may set, and like
`allowed_routes` it may only narrow -- `CompactionPolicy::narrow` lets
`advisory` from either layer win. An advisory policy really does stop the
recovery, including after an overflow; a narrowing that changed nothing would
be decorative.

### Checkpoints: versioned, portable, atomic

`runtime::checkpoint::PortableCheckpoint` (schema v1) carries the objective
(the first acknowledged input), the operator's hard constraints, the
task/workflow refs, **every** acknowledged input verbatim with a `pending`
flag, task claims from the journal's own receipts, completed-action receipts,
outstanding tool calls with their real state, and evidence by SHA-256. `build`
is pure; the same conversation always produces the same checkpoint.

`commit` writes the portable export first (temp sibling + rename, via
`state::write_private`), then appends the journal `Checkpoint` event. **The
journal event is the commit point.** A crash in between leaves an orphan
export that nothing reads -- never a half-compacted session. The export
failing is not a reason to refuse the commit, because the event carries the
whole checkpoint.

`latest_valid` walks checkpoints newest-first and SKIPS anything unusable: an
unknown schema, a payload that no longer deserialises, one for another
session, one claiming to cover events the journal does not have. A bad
checkpoint costs a session its newest summary, never its ability to resume.

### The boundary: why compaction cannot lie

`checkpoint::boundary` never crosses an unsettled tool call. Any call whose
latest execution is `Prepared`, `Started` or `OutcomeUnknown` -- and every
message from its assistant message onward -- stays verbatim. That single rule
is what makes "compaction never silently marks a pending action complete"
structural rather than a promise. The newest `retain_recent_messages` are
also kept verbatim.

Acknowledged input is safe twice over: undelivered input is after the
boundary by construction, and the checkpoint carries every acknowledged input
regardless, with the pending ones repeated in the summary message.

### The compacted request

`build_request` prepends `compaction::summary_message` and skips messages at
or before `covers_through`. `ProviderRequest::system` -- the stable cacheable
prefix -- is never rewritten, which is what keeps provider prompt caching
valid across a compaction. The summary is explicitly labelled `[zirv
compaction]`, states that the verbatim record is retained, and names
outstanding tool calls with their real state including `outcome_unknown`.

### Distillation: bounded, read-only, always available

`compaction::distill` goes through the session's own native route with a
bounded output budget and **no tool schemas at all** -- read-only is enforced
by giving the model nothing to call, and a reply that contains a tool-use
block anyway is refused outright. Any failure, refusal, empty reply or
attempted tool call falls back to `structural_summary`, a deterministic
rendering that needs no credential, no capacity and no network. That fallback
is why every helper here works with the external harness binaries absent.

A real distillation is a real cost: its usage is recorded in the journal and
summed into the session's own usage, so compaction can never be a spend a
reader cannot see.

### Continuation: keep the envelope, or rebuild legally

`compaction::plan_continuation` compares the target route field-by-field with
the session's. An exact match is `SameRoute`: the provider's opaque
continuation envelope (N03's `native_continuations` table, which already
refuses a non-matching identity) stays valid. Anything else is `Rebuilt`:
`semantic_history` reconstructs text and refusals only. `Thinking`,
`RedactedThinking` and every provider signature are deliberately absent --
they are one vendor's internal state, they are not transferable, and
fabricating them would be exactly the synthesis this step forbids. Tool calls
become plain statements of what was requested and what actually happened; an
unknown outcome is carried as unknown.

### Status

`zirv ctx status` grows one line per native session that has compacted or
resumed, with the newest compaction's reason and summary source, read
straight off the journal. It is omitted entirely when no native journal
exists, and looking never creates one. `NativeFinalStatus` (schema v2)
carries the same facts as `compactions` plus the newest
`compaction_decision`, including one that was only advice.

## What is verified

Deterministic, no network, no paid call, no filesystem effect:

- The usable window is the model window less the output reservation; an
  unknown window and an over-large reservation both stay `None`.
- Token pressure measured against the *usable* window forces a compaction; a
  provider overflow forces one at any token count; repeated identical tool
  calls and repeated identical error texts are each their own trigger.
- An advisory policy downgrades a forced compaction to advice, at the pure
  level and through a whole fixture session, which writes no checkpoint event
  at all. The narrowing fold never widens.
- A long fixture session crosses the window, compacts once on
  `token_pressure`, distils through the route, and the request the model then
  sees opens with the briefing carrying the objective, the operator's hard
  constraint and the completed calls' identities -- while the original
  acknowledged input and every original event are still in the journal.
- The distillation request carries no tool schemas.
- A context-overflow refusal recovers through a compaction and completes,
  with every tool executed exactly once.
- A newer checkpoint this build cannot read falls back to the last valid one;
  a portable export with no journal event is not a compaction.
- A route change discards the envelope and rebuilds history with no hidden
  reasoning and no synthesized outcome; an outcome-unknown call is carried as
  unknown.
- The boundary never crosses an unsettled tool call; an outcome-unknown
  execution is an outstanding tool, never a receipt; every acknowledged input
  is carried and undelivered input stays pending.
- Two observations of the same journal give identical decisions.
- A repository may narrow `policy.compaction` and cannot widen it, and gains
  no other policy key.

## What is deferred

- **Memory promotion from a checkpoint.** Compaction deliberately touches no
  memory bank: `memory.rs`'s locked store, its journal and its
  promotion/rollback remain the only writer, and a compaction neither harvests
  nor clears it. Harvesting durable facts out of a checkpoint the way
  `memory::harvest_durable_*` does from a handoff is a separate change.
- **Cross-provider continuation on a live route.** `plan_continuation` is
  fixture- and journal-verified; actually rebinding a running session to
  another provider mid-task belongs with N19's rollover.
- **`ctx handoff` from a checkpoint.** `CheckpointKind::Handoff` exists and is
  written by nothing here; the legacy transcript-distilled handoff is
  unchanged.
- **Workflow refs.** `CheckpointContext::workflow` is carried through every
  layer but nothing populates it yet -- wiring the workflow engine into a
  native session is N15's.
- **Live provider runs.** No live native session has been compacted against a
  paid account from this step.
