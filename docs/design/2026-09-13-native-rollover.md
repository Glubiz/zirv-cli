# Safe native and cross-runtime orchestrator rollover (issue #488)

**Date:** 2026-09-13 · **Issue:** #488, step **N19 of 23** of the native-runtime
roadmap (#469) · **Status:** implemented; the seat/record/fence halves are
wired into the live rollover driver, two decision halves await the supervisor
that carries a repository root (see §6)

## 1. Context

The orchestrator seat already had a real transaction. `seat.rs` reserves a
generation (`prepare`), swaps identity under a lock (`commit`) or releases it
(`abort`); `rollover.rs` decides from capacity evidence and opens it;
`handover.rs` is the live-swap seam; #464 added park-and-return so a displaced
harness keeps its own conversation instead of being closed.

Every part of that was built when a seat could only ever move between
supervised harness **processes**. That made "prepare, launch, wait for a turn
signal, commit" a complete story, because the successor's own harness owned the
continuation and all zirv had to carry across was a handoff packet.

A native session (N09/N10) breaks all three assumptions at once:

- its state is a **journal**, not a screen — in-flight tool effects are durable
  facts, and one of them may have begun and never reported;
- its continuation is either the provider's opaque **envelope** (same route) or
  a **semantic rebuild** (any other route) — N17's `plan_continuation`;
- its subagents are **zirv's own delegations**, not children of a harness
  process that dies with it.

So the gap this step closes is not "add native to the candidate list". It is:
what does a *safe boundary* mean when the thing being left has durable state,
and how is exactly one write-capable generation guaranteed while that state
moves?

## 2. Decision

One transaction, extended along one new axis, plus one module for the parts
that are genuinely different.

### 2.1 The runtime is a field of the transaction, not a second transaction

`Phase::Prepared` gains `successor_runtime` and `seat::commit` adopts it into
`Seat::runtime`, inside the same locked write that already adopts the
successor's agent, model and session. So a reader can never observe a seat
whose runtime and generation disagree about which session is sitting in it.

`prepare` becomes `prepare_onto(.., RuntimeKind::Harness, ..)`. That spelling
is deliberate: it is what every wrapped→wrapped caller already means, so those
seams, their acks and their tests are untouched by the runtime dimension
existing. `rollover.rs` reads the successor's runtime off the snapshot's own
route identity (`runtime_of`) rather than guessing from the name — a candidate
the snapshot has never heard of is a harness, because that is the only kind of
row `fallback.order` can name, and guessing `Native` would open a transaction
no backend could complete.

`Displaced` gains the runtime its conversation reference belongs to, and
`sessions::record_conversation_on` lets a native session record its own journal
session id. `native_conversation` already refused a marker whose runtime did
not match the reader's; recording the runtime honestly is what makes that
refusal mean anything for a session that is not a harness. That is criterion 5:
an inaccessible successor cannot cause a resume of the wrong conversation id,
because the id is only ever handed back under the runtime that produced it.

### 2.2 The safe boundary is reached before the successor exists

`rollover_runtime::reach_boundary` runs **before** `seat::prepare_onto`, and a
failure there is a failure to prepare — the source keeps the seat and nothing
has been given up. In order:

1. every execution whose effect **began** and never reported becomes
   `OutcomeUnknown` (the journal's existing `reconcile_started_as_unknown`);
2. on a forced boundary, every execution that was merely **prepared** —
   admitted, recorded, never begun — is explicitly `Cancelled`. Nothing that
   started is ever called cancelled: that would be a claim about an effect
   nobody observed;
3. one `PortableCheckpoint` is committed as a `CheckpointKind::Handoff`,
   carrying acknowledged input, claims, receipts, outstanding tools and
   evidence — under the **source** generation, which is the generation those
   facts belong to.

`Drain::Quiesced` vs `Drain::Forced` is the caller's verified-idle answer, never
guessed here. A harness source has no journal and therefore no native boundary:
it yields `None`, which is honest. Its boundary is the verified-idle turn
boundary the supervisor already observes and its carried state is `handoff.rs`'s
structural packet, neither of which this issue changes.

**What "delivered" means.** The journal does not record which provider request
an input was folded into. The one thing it records honestly is whether the
model ever answered after it, so `delivered_through` is the last assistant
message: acknowledged input with no assistant turn after it is still owed.
Over-reporting costs the successor a repeated instruction; under-reporting
loses an operator's instruction, which criterion 2 forbids outright.

**Halting is a state, not a note.** `Boundary::halts_successor` is criterion 4.
Carrying an ambiguous effect is not enough — the successor has to be blocked on
it, or the first thing a fresh model does is call the tool again.

### 2.3 Validation is pure, and the fence is what enforces "no writes yet"

`validate` gates policy → capability → context room → billing authority (all
four through N18's own `route::eligible`, not re-derived) → authentication →
budget → startup → a legal continuation payload. Every one of those is decided
**before** the source is given up, which is what makes item 7's restore
possible: nothing is discovered after the fact.

The issue asks for validation "without allowing successor writes before the
atomic generation commit". Asking the caller to remember that would be a rule
with no enforcement, so it is a **fence** instead. `seat::authority` is one pure
function: only the seat's *current* generation is write-capable. A generation
below it was superseded; a generation above it — during a prepared window, that
is exactly the successor — has not taken the seat. Both are refused, with a
typed `StaleGeneration` carrying `Superseded` or `Uncommitted`, because the
operator-facing answer differs.

That fence is applied at the four places a stale generation could still act:

| surface | mechanism |
|---|---|
| native tool effects | the journal's existing `ensure_generation` on every append |
| wrapped tool effects | the existing env-derived `seat::fence` in `hook::run_pretool` |
| delegation | `delegation::Parent::generation`, guarded first in `delegate` — ahead of the bounds check and far ahead of the durable launch receipt |
| the coordinator graph | `coordinator::update_fenced`, which `delegate` now writes through |
| writer leases | `permit::acquire_writer` maps `seat::fence` to a new, non-retryable `WriterRefusal::StaleSeat` |

Criterion 3 falls out of this rather than being bolted on: at the prepare crash
point only the source passes, at the commit crash point only the successor
does, and `seat::commit` swaps which is which under the seat lock. There is no
instant at which two generations can write and none at which neither can —
`exactly_one_generation_can_write_the_graph_across_a_rollover` pins it at both
crash points.

### 2.4 Continuation: one rule, stated once

A provider's opaque envelope belongs to one conversation, on one route, with
one vendor. So:

| direction | continuation |
|---|---|
| native → native, identical route | the provider envelope is kept |
| native → native, any route change | semantic rebuild (N17) |
| harness → native | semantic rebuild — the source ran no native route, so there is no envelope to keep whatever the plan says |
| * → harness | the structural packet; a coding harness never sees another vendor's envelope |

Portable checkpoints therefore stay strictly separate from provider envelopes,
which the journal already enforces at the storage layer: `store_continuation` /
`load_continuation` refuse a `ContinuationIdentity` mismatch outright.

### 2.5 Native subagents: three answers, and deliberately not a fourth

`Disposition` is `Finished`, `Stopped` or `Retained { owner }`. There is no
`Migrated`, because zirv has **no observed mechanism** that moves a running
worker from one seat generation to another. A live worker at a quiesced
boundary is retained under the seat's short id — a stable address that outlives
the session id rotating underneath it, which is exactly why retention works
across a runtime change and a fresh conversation would not. A live worker at a
forced boundary is stopped through `delegation::interrupt`, which owns
cancellation and preserves `unknown_tool_outcomes`: an unresolved effect is
what a cancel must not erase.

### 2.6 The record, and the return

`rollover_runtime::Record` lives at `<state>/sessions/<short>.rollover.json`,
beside the seat record rather than on it — for the reason `seat.rs` keeps the
seat out of `sessions::Record`: a seat is rewritten on every ordinary
supervision tick and this is not. It carries the trigger, the direction, every
route tried with its own refusal, the decision, the boundary summary and one of
three settlements: `Committed`, `Restored` (preparation failed; the ORIGINAL
session still holds the seat) or `Parked` (nothing could take it, and the seat
waits honestly with its durable state intact). `rollover.rs` opens it at
preparation and settles it at every exit — commit, fail, park — and
`rollover::forget` drops it with the seat.

`Record::status_line` is criterion 6's text: the same logical seat (short and
generation), the backend now answering at it, where it came from, why, and any
reconciliation the successor is halted on. `zirv ctx status`'s pool view now
renders the seat's runtime inline, the harness or route it was displaced from
(with whether a conversation reference was retained) and that status line.

The return (`plan_return`, item 8) applies four gates in the order an operator
would ask them: is there a home to return to → hysteresis (idle boundary,
cooldown, and a measured reading at least `min_candidate_headroom_pct` above
the seat's own; an unknown reading never moves a seat) → **authority** → go.
The authority gate runs through the same `validate` a forward rollover does, so
a return cannot take a shortcut a forward move would be refused for.

**Billing authority has an explicit default.** `Demand::authorizes` treats an
empty set as "no billing constraint was stated", which is right for callers
that never thought about billing and exactly wrong for a rollover, whose whole
job is to move work somewhere else. So
`default_authorized_billing(current) = {current, Local}`: whatever the seat
already spends, plus local runtimes (no credential, no invoice, no ceiling). An
operator who wants a subscription seat to fail over onto metered API credit
widens that at the call site; nothing widens it on their behalf. This is a
behaviour, not a config key — no new key, no new verb, no new crate.

### 2.7 Composing with N20's controller rule

N20 (#489, merged on the release head but not on this branch's base) makes the
persistent runtime the owner of native conversations, with the rule that a
session with attached clients requires the controller's `client_id` on every
mutation, and that `NativeSessions::restore` advances the generation only for
its **own** durable topology.

These compose without contradiction because they fence different questions:
N20's controller rule decides *which client may speak for a session*; this
step's generation fence decides *which seat generation may act at all*. The one
ordering rule that matters is stated here: rollover advances the seat
generation through `seat::commit`, and the runtime service advances a
conversation's journal generation through `advance_generation` when it restores
or resumes. A rollover must therefore never be driven for a conversation the
service owns from outside the service — which is already true, because
`reach_boundary` writes under the generation the journal reports
(`journal.session(..).generation`), so a service that has since advanced it
fences the rollover out rather than the other way round.

## 3. What is verified

All names are in `commands::ctx::{rollover_runtime, seat, coordinator,
delegation, pool}`.

- **Criterion 1 — all four directions × four triggers.**
  `every_direction_and_trigger_commits_without_losing_acknowledged_state`
  (usage exhaustion, endpoint failure, manual handover) and
  `an_inaccessible_successor_never_takes_the_seat_or_the_wrong_conversation`
  (successor startup failure), each over all four runtime pairs, against a real
  journal and a real seat record.
- **Criterion 2 — nothing acknowledged is lost.** The same matrix asserts the
  acknowledged input, the held task claim and the completion receipt all reach
  the committed checkpoint; `the_boundary_reconciles_started_effects_and_
  cancels_ones_that_never_began` re-reads the checkpoint back off the journal.
- **Criterion 3 — injected crashes at prepare/commit.**
  `exactly_one_generation_can_write_the_graph_across_a_rollover` (coordinator,
  both crash points) and `only_the_committed_generation_is_write_capable`
  (seat). `a_stale_or_uncommitted_generation_may_not_delegate` shows the
  delegation service refusing both, with no launch receipt written.
- **Criterion 4 — ambiguous effects halt.** `halts_successor` in the boundary
  tests and in the matrix; the reconciliation note names the calls.
- **Criterion 5 — wrong conversation id.**
  `an_inaccessible_successor_never_takes_the_seat_or_the_wrong_conversation`
  and `a_native_source_is_parked_with_its_own_conversation_and_never_a_harness_one`.
- **Criterion 6 — UI and status.**
  `render_full_names_a_pending_rollover_and_its_successor` pins the rendered
  seat line, the displaced-from line and the rollover status line;
  `the_rollover_record_carries_the_trigger_tried_routes_and_outcome` pins the
  line's own content.
- **Continuation and return.**
  `only_a_same_route_native_successor_keeps_the_provider_envelope`,
  `a_successor_is_refused_for_auth_capability_budget_startup_or_billing`,
  `a_return_respects_hysteresis_and_refuses_an_unauthorized_billing_change`,
  `the_default_billing_authority_is_what_the_seat_already_spends_plus_local`.
- **Subagents.**
  `native_subagents_are_finished_stopped_or_retained_but_never_migrated`.
- **Unchanged wrapped→wrapped.**
  `a_wrapped_to_wrapped_rollover_keeps_the_seat_on_the_harness_runtime`, plus
  every pre-existing `rollover::`/`seat::`/`dash::` rollover test passing
  untouched.

## 4. Trust boundary

Nothing here adds a config key, a CLI verb or a crate, so the trust-boundary
table is unchanged. The two policy-shaped defaults are stated in code and in
README: the seat's own billing posture (plus local) is what a rollover is
authorized to move onto, and `fallback.*` remains the only operator surface
that tunes when a rollover fires — repo-forbidden exactly as before.

## 5. What is NOT claimed

- **Cross-runtime candidate *ranking*.** `rollover::evaluate` still ranks the
  harness pool. Native routes get per-minute usage readings in a later step
  (`route::offers_from_config` is still not fed into the snapshot), so today the
  directions that actually arise in production are the two with a harness
  target plus `native->harness`. The transaction, boundary, validation, fence
  and record are direction-agnostic and tested on all four.
- **Transparent subagent migration.** Explicitly refused, see §2.5.
- **A distilled rollover summary.** The boundary commits the structural
  checkpoint only; spending a distiller call on a route that just failed is
  exactly what `structural_only` exists to avoid.

## 6. What is deferred

- **`settle_subagents` / `disposition` have no in-tree caller yet.** They need
  the repository root, and the seat-level rollover driver works from a seat
  short id and a state directory. The supervisors that carry a repo are
  `wrap.rs` and `dash/mod.rs`, which this step deliberately does not touch
  (`wrap.rs`'s ~30 `#[cfg(unix)]` PTY tests cannot be compiled on the Windows
  machine this was developed on). Both are pure/durable and tested here so that
  wiring is a call, not a redesign — the same `#[allow(dead_code)]` posture
  `runtime/mod.rs` and `route::offers_from_config` already document.
- **Pane rendering of a rollover.** The status view carries the line; the
  dashboard pane's own presentation of it is N21's UX step.
- **`#[cfg(unix)]`.** Nothing in this change is unix-only and no `#[cfg(unix)]`
  block was added or modified; CI is the evidence for the unix halves it runs
  over.
