# The native meta-orchestrator and mixed-runtime teams (N16)

**Date:** 2026-09-13 · **Issue:** #485 · **Roadmap:** #469

## Context

N09-N15 made a native session a real Zirv session: it conducts a
conversation, delegates workers on either runtime (#479), reaches MCP servers
and browsers (#482-#483), drives the workflow engine and answers every helper
model call without a vendor CLI (#484). What was still hosted somewhere else
was the thing *coordinating* all of it.

That mattered in four concrete ways, none of them cosmetic:

- **Role was a free-text string.** `--role` reached exactly two decisions --
  which methodology layer the context compiler injected, and which `[roles]`
  entry the route came from -- and both were open-coded, twice. `native.rs`
  carried two copies of `roles.get(role)`, each producing an untyped error
  string, and `prompt_role` recognised `orchestrator` but not `coordinator`,
  the name the roadmap itself uses for the seat.
- **A delegation's bounds were decided after its receipt was written.**
  `agent::run_with` refuses a depth-0 envelope, but `delegation::delegate`
  wrote the durable launch receipt *first*. A refused delegation therefore
  left a `Failed` record naming work nobody started.
- **Capability was inherited, not identified.** A child's `--mode` was
  whatever the caller asked for, narrowed against the parent's envelope.
  Nothing said "a reviewer does not write", so a reviewer asked for as a
  writer got a checkout; and nothing said "a coordinator may delegate a
  writer even though it is not one itself".
- **The plan lived in a transcript.** Task cards, delegation records, groups
  and the objective were all durable, but *which role took which task, which
  delegation is answering for it, and what the operator has asked for since*
  existed only in the coordinating model's context window. A restart lost it,
  and "restart the coordinator without duplicating tasks" was therefore a
  property of a transcript rather than of a record.

## Decision

### Two small modules, no new state machine

`ctx::team` answers "who is this seat". `ctx::coordinator` answers "what is
the plan, and may this delegation start". Everything else is reused.

The rule applied throughout: **a fact that some service already owns is read
from that service, never copied.** The coordinator record holds no claim, no
permit, no reservation, no health verdict and no admission count. It holds the
graph, the decisions, the evidence *references* and the user's constraints --
the four things nothing else held.

### The team is a closed set, and its authority comes from the seat record

Seven roles: `coordinator`, `sub-orchestrator`, `researcher`, `planner`,
`implementer`, `reviewer`, `tester`. `orchestrator` is accepted as a spelling
of `coordinator`, because that is the name the prompt layer and every seat
record written before this step already use.

`team::authority(role)` is a pure table of two bits: may it delegate, may it
write. It is read from the role zirv itself minted into the persisted **seat
record** (`seat::Seat::role`, reachable at effect time as
`ExecutionIdentity::role`), which model output cannot reach. That single
source is what makes item 6's two directions both true at once:

- **no inherited over-restriction** -- a coordinator running read-only still
  delegates a *writing* implementer, because the child's posture is the
  child's role's, not a copy of the parent's;
- **no escalation** -- a reviewer, tester, researcher or planner is clamped to
  read-only however the request was spelled, and a reviewer seat may not
  delegate at all.

A role **outside** the table keeps exactly what it had: `Authority::worker()`
is `may_delegate: true, may_write: true`, bounded as it always was by the
envelope's `delegation_depth` and by the writer permit. This table narrows the
roles it knows; it does not silently take an entitlement away from a role
nobody has told it about, which is the difference between a policy and a
regression.

### Role-to-route selection is the operator's table and nothing else

`team::route_for_role` is one lookup in `[roles]`, with a typed
`RouteRefusal::Unconfigured` naming the roles that *do* have an entry.
Deliberately no fallback chain: answering "no route for `reviewer`" with the
coordinator's own expensive route would be inferring an entitlement nobody
granted. Both open-coded copies in `native.rs` now call it.

A route a delegating **model** names for a role goes through
`team::authorize_route`, which reuses N18 wholesale: `route::offers_from_config`
(previously `#[allow(dead_code)]`, now a production caller) plus
`route::eligible` with the role's own configured billing posture as the
authorization. A policy refusal, a missing capability or a *billing change* is
therefore refused with N18's own reason text. An operator typing `--route` on a
CLI delegation is the operator speaking and is untouched.

`team::roster` reports which roles this machine can actually staff, and
`team_status` returns it: a coordinator planning around a role with no
configured route is planning work nothing can take.

### One bounds decision, before the receipt

`coordinator::check` is pure -- no clock, no filesystem, no config -- and
takes the parent's role, the child's role, the envelope depth and whether the
objective is stopped. It returns the child's *granted* write posture and
depth, or one of three typed refusals. `delegation::delegate` calls it as its
first act, so a refusal starts nothing and writes nothing.

What it deliberately does **not** check is as important as what it does. Each
of these is already enforced centrally, on the path both runtimes go through,
and a second counter would be a second answer:

| Bound | Where it is enforced | Reused unchanged |
| --- | --- | --- |
| Ownership of a task | `task::claim_locked` | yes |
| Ownership of a checkout | `permit::acquire_writer` + the broker's lease check | yes |
| Machine-wide writer concurrency | `supervise.max_writers` | yes |
| Batch concurrency and token budget | `group::admit_child` (`child_limit`, `token_budget`, `reserved_tokens`) | yes |
| Per-provider token ceiling | `reservation::reserve_within` | yes |
| Delegation depth | `envelope::WorkerEnvelope::delegation_depth` | yes, now also read at the tool seam |

**No new configuration key.** Every knob this needed already existed.

### The graph is written by the services, not asserted by the model

`coordinator::dispatched` is called by `delegation::delegate` beside the launch
receipt; `coordinator::plan` by the `task_create` tool; `coordinator::settled`
only by `consume_pending`. The model chooses what to do next; it does not get
to say what happened. Nodes, decisions, constraints and per-node evidence are
all bounded rings, because the record is read back into a coordinator's
context on every resume.

A node stays `delegated` until its receipt is **consumed**, not merely
published. `team_status` reports unconsumed outcomes under
`pending_completions` rather than folding them into the node, which is item
5's "known/unknown stays honest until receipts arrive" expressed as a data
shape rather than as prompt text.

### Restart is a read

A coordinating session (`PromptRole::Orchestrator`/`SubOrchestrator`) calls
`coordinator::consume_pending` at the top of `run_session`, before the
transport is built. It consumes each terminal outcome exactly once through
`delegation::consume_delivery` -- the mechanism N10 already ships, not a
second one -- and folds it into the graph. A settled node is never re-settled
and `outstanding()` excludes anything already dispatched, so a coordinator
that restarts twice neither loses a receipt nor restarts finished work. A
graph that cannot be read never stops a session from starting: the delegation
records are still authoritative.

### Steering is the command the operator already has

`zirv ctx objective set` records the new target as a constraint on the
coordinator record and lifts a stop -- redirecting rather than restarting.
`zirv ctx objective close` (and `record_completion`, the shared close path)
stops further dispatch: planned nodes are cancelled, *delegated* ones keep
their node so their receipts are still consumed. A live worker is stopped
through `delegation::interrupt`, which owns that.

There is deliberately no `objective_steer` tool. Steering is something the
**user** does to the coordinator; a tool that let the coordinator write its
own constraints would be a coordinator marking its own homework.

### Seven tools, zero new logic

`task_create`, `task_claim`, `task_list`, `group_create`, `group_status`,
`objective_status`, `team_status` join the native registry (40 tools to 47).
Each is a validated argument shape in front of the same `ctx::task` /
`ctx::group` / `ctx::objective` function the CLI verb calls -- N10's
delegation tools and N15's workflow tools exactly. `task::create_card` was
split out of `run_create` so the tool and the verb share one append rather
than two; nothing else needed splitting.

The three that mutate shared state cross the broker as
`ExecutionAction::Knowledge` with `scope: "shared"` and `write: true`, so a
session with no writer permit for its own worktree -- a read-only helper, a
reviewer seat -- reads the board and cannot move a piece on it, refused at
effect time rather than by a prompt. A refused *claim* is an answer, not a
failure: "somebody else holds this" is exactly what a coordinator needs to
hear, and it comes back as `claimed: false` with the refusal's own reason.

## What is verified

Deterministic; no provider call, no spawned harness, no network. The worker
LAUNCH is the one stubbed seam, for the reason every N10 delegation test stubs
it: starting a real worker needs a provider endpoint, and these tests are about
whether zirv needs a coding harness.

- **All-native, harness-absent** --
  `tools::tests::an_all_native_team_runs_a_workflow_with_every_coding_harness_absent`
  plans, staffs and dispatches three native roles against real cards and a real
  work group, reads the live workflow through the real engine, restarts and
  settles all three receipts, all with `PATH` scrubbed empty.
- **Mixed runtimes, one board** --
  `tools::tests::a_native_coordinator_runs_a_mixed_team_through_the_shared_services`
  dispatches a native implementer and a wrapped (harness) reviewer against the
  same cards and group, and reads one consistent board back.
- **Ownership** --
  `tools::tests::two_workers_can_never_claim_one_card` (the tool's claim is the
  shared one), plus N10's unchanged writer-permit and task-card exclusions.
- **Authority, both directions** --
  `team::tests::authority_comes_from_the_role_and_nothing_else`,
  `coordinator::tests::a_childs_write_posture_comes_from_its_own_role`,
  `coordinator::tests::a_read_only_role_is_clamped_however_the_request_was_spelled`,
  `delegation::tests::the_childs_mode_is_decided_by_its_role_not_by_the_request`,
  `tools::tests::a_seat_whose_role_grants_no_delegation_authority_is_refused_at_the_tool`.
- **Bounds before the receipt** --
  `delegation::tests::a_refused_delegation_starts_nothing_and_writes_no_receipt`.
- **Restart** --
  `coordinator::tests::a_restarted_coordinator_consumes_pending_receipts_exactly_once`
  and `re_planning_a_settled_task_never_resurrects_it`.
- **Steering and stopping** --
  `tools::tests::operator_steering_and_stopping_reach_the_coordinator`,
  `coordinator::tests::cancelling_stops_planned_work_and_keeps_delegated_work_answerable`,
  `delegation::tests::a_cancelled_objective_admits_no_further_delegations`.
- **Routes** --
  `team::tests::a_role_with_no_entry_is_a_typed_refusal_naming_the_roles_that_have_one`,
  `a_requested_route_may_not_move_a_role_onto_different_billing`,
  `operator_policy_outranks_the_role_table`,
  `the_roster_says_which_roles_this_machine_can_actually_staff`.
- **Read-only enforcement** --
  `tools::tests::a_session_with_no_writer_permit_can_read_the_board_but_never_move_it`.

## What is deferred

- **Live provider coverage.** Every test here is fixture- or service-level; no
  route was exercised against a real endpoint, and the parity table records
  implementation and tests, never validated end-to-end vendor behaviour.
- **A real stub-harness process in the mixed-team test.** The mixed test drives
  both runtimes through the real delegation service with the launcher stubbed.
  The repository's stub-harness fixtures are shell scripts and do not run on
  the Windows development machine this was written on; the wrapped half is
  pinned by the `LaunchRequest` the service produced, not by a spawned CLI.
- **The coordinator's own envelope depth at the tool seam.** The `delegate`
  tool resolves depth the same way `agent::run_with` does for the same session
  (`agent::resolve_parent_envelope`), so the two always agree. Threading a
  native worker's *own* narrowed child envelope into its in-process tool client
  is a separate change; the narrowing itself is still enforced where it always
  was, inside `agent::run_with`.
- **Per-turn refresh of the graph into the model's context.** The coordinator
  record is consumed at session start and read on demand through `team_status`;
  it is not recompiled into the standing context every turn, for the same
  prompt-caching reason N15 gives for the workflow context.
- **Overlap advisories.** Exact ownership is enforced; "these two planned
  tasks look like the same work" is not attempted, unchanged from N10.
