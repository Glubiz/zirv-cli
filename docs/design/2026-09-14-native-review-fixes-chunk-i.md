# Native-harness review fixes, chunk I (#554, #552, #580, #620, #627)

**Date:** 2026-09-14 · **Issues:** #554 (N18), #552 (N19), #580 (N17), #620,
#627 · **Branch:** `native/fix-i` onto `release/native-harness`

## Context

Four findings from the PR #493 review, two of them release blockers. Each is
the same shape: a mechanism that exists and is tested, with no production path
actually reaching it.

## Decisions

### #554 — native requests go through the shared scheduling machinery

The rule taken here is the one #487's own design note states: *extend the
existing model, do not stand a second one beside it.*

- **One allocator.** `route::offers_from_config` (previously `#[allow(dead_code)]`)
  now feeds `fallback::capacity_snapshot_with_native`, which appends a real
  `allocator::HarnessCapacity` row per configured native route — capacity
  looked up by **billing pool**, health from the route's three scoped
  breakers, and the route's own `RouteOffer` so `place`'s eligibility gate
  runs before any ranking. `native_worker::native_placement` calls the shared
  `allocator::place`, and its answer is recorded in the decision log.
  `capacity_snapshot_with_native` is deliberately a **separate** entry point:
  `capacity_snapshot` is on the dashboard's once-a-second path and a native
  row costs a provider-config load, so the cost lands where the answer is
  used.
- **Only health refuses.** A native pool has no per-minute reading of its own
  yet, so a native row classifies `Unknown`, which for a harness means "prefer
  someone else" and for a native route would mean "never run". Admission is
  therefore gated on `Exclusion::Unhealthy` alone — an endpoint the provider
  just refused, a credential it rejected, a model this account may not use.
  Every other exclusion is reported, not enforced.
- **Health is durable and keyed per route.** The loop stays pure about health
  (`rot.rs`'s rule): it decides the scope and carries it out on
  `NativeFinalStatus::failure_routing`; `health_store::record_native_outcome`
  is the only place that becomes a record on disk. A rate limit, a context
  overflow, a refusal and a cancellation have no `breaker_key` and so reach no
  breaker at all. `health_store::native_admission` is the read side: the
  strictest of the endpoint, credential and credential/model verdicts.
- **Reserve and settle against the pool, on the total.**
  `runtime::native::route_pool` returns the billing pool beside the provider,
  the reservation is keyed by the pool, an absent `--budget-tokens` now
  reserves the loop's own output ceiling rather than zero, and
  `settle_native_run` settles every token the provider metered (prompt, cache
  write, cache read, completion) rather than the completion alone.
- **Spend.** `settle_native_run` appends one `log::Delegation` row, which is
  the ledger `zirv ctx spend` reads. Without it a native worker's usage
  existed only inside its own JSON status.

### #552 — every rollover direction launches a successor

`rollover_runtime::launch_successor` is now the one production seam a live
swap starts its successor through. It settles this seat's subagents
(`settle_subagents`, previously with no in-tree caller) **before** anything
takes the seat, then starts exactly one successor through the seam's own
`SuccessorLauncher` backend. `plan_successor` is pure and decides all four
directions in one table: a harness successor is named by an agent and a native
one by a route, a provider envelope never crosses runtimes, and a boundary
carrying an ambiguous effect produces `SuccessorPlan::halted_for` so the
successor is admitted **halted** rather than unaware.

`HandoverRequest` gained `target_runtime`/`target_route`, and `rollover.rs`
fills both from the runtime it already resolved off the snapshot
(`runtime_of`), so what a rollover decided is what the swap seam starts.
`dash::handover_pane` — the dashboard's live swap driver — now goes through
`launch_successor` with `PaneSuccessorLauncher`, which carries two backends:

- a **harness** successor is the in-place pty swap `Pane::handover` has always
  performed;
- a **native** successor cannot be an in-place child replacement, because a
  native pane has no child — it is an in-process session with its own journal.
  So the successor pane is **opened first** (`Pane::spawn_native`) and the
  source is retired only once it exists. Exactly one of the two is ever live,
  and a failure to build the successor leaves the source untouched and still
  holding the seat.

Three small seams make that honest, rather than a new session wearing the old
one's name:

- `NativeBackend::start_on_seat` starts a brand-new conversation **on an
  existing seat**: it keeps the seat's stable short id (the address mail,
  nudge and `zirv ctx status` resolve, which by design does not move across a
  rollover) and runs under the generation `seat::commit` promoted. The logical
  session id is still fresh — this is a new conversation, not a resumed one.
  `RuntimeBackend::start` is now a call to it with no seat.
- `NativeDashboardSpec::initial_input` carries the handoff packet, every
  acknowledged input the source never delivered, and the reconciliation the
  plan is halted on, submitted as the successor's first turn. Acknowledged
  input is spelled out separately from the packet on purpose: the packet is a
  summary, and criterion 2 forbids a summary losing an operator's instruction.
  `NativeDashboardSpec::seat` also forces the in-process spawn — a successor
  must never attach to whatever a persistent runtime already holds for this
  repository, which is the conversation the rollover just moved away from.
- `Pane::retire_for_successor` ends the source without releasing the registry
  record or forgetting the seat, both of which the successor has adopted under
  the same short id. `SessionGuard::disown` is what lets the source's guard
  stop speaking for an address without deleting the file the successor wrote.

Nothing in this half is `#[cfg(unix)]`; both backends compile and run on every
platform CI covers.

### #580 — no acknowledged input is ever dropped

`checkpoint::build` no longer truncates `acknowledged_input`. Length is bounded
by BYTES instead: past a total budget the oldest already-**delivered** turns
give up their text, marked `text_elided` so a reader resolves it from the
journal by `message_id` rather than believing the turn said nothing. A
`pending` turn is never elided — it is still owed.

### #620 / #627 — a dashboard-hosted seat's own delegations

Three behaviours, one per cause:

1. `join_targets_in_order` prefers the dashboard **hosting the caller**, read
   off `owner_pid` on the caller's own registry record. That is not an
   inference about repositories: it names the dashboard this process is inside.
2. `dash_shorts_for_repo` accepts `Verb::Chat` as well as `Verb::Dash`. A
   dashboard's token directory is named by the orchestrator seat's short id,
   and that seat's row is `Verb::Chat` from the moment it re-registers after a
   restart with handoff — which is exactly how the live dashboard stopped
   being a repo match.
3. `try_join_dashboard` walks the ordered candidate list: a `retryable`
   refusal costs one round-trip and the next live dashboard, not the whole
   delegation.

For #627, the parent-claim gate now distinguishes what the channel proved.
A claim forged on a pane's own private channel stays a `policy` refusal; a
claim on the shared channel (which proves no identity, the ordinary shape of a
dashboard-hosted seat that re-registered) is a `channel` refusal, so the
delegation falls back to the inline supervised run instead of exiting 1 with no
fallback at all. The claim itself is refused either way.

## What is verified

- `checkpoint::tests::portable_checkpoint_preserves_more_than_64_acknowledged_inputs`
- `native_worker::tests::native_requests_allocate_record_health_and_reconcile_pool_spend`
- `rollover_runtime::tests::every_rollover_direction_launches_one_successor`
- `dash::tests::a_native_successor_actually_opens_as_a_live_pane_on_the_same_seat`
- `dash::tests::a_harness_successor_takes_the_seat_from_a_native_source`
- `agent::tests::the_dashboard_hosting_this_caller_is_preferred_over_a_newer_foreign_one`
- `agent::tests::a_restarted_hosted_seats_chat_record_still_names_its_dashboard_for_this_repo`
- `agent::tests::a_foreign_repo_refusal_tries_the_next_live_dashboard_before_running_inline`
- `dash::tests::an_unprovable_parent_claim_is_a_retryable_channel_refusal`

## What is deferred

- **Nothing about the four directions.** All four launch a live successor at
  the dashboard seam, so `SuccessorRefusal` is down to `LaunchFailed` — and
  every backend builds its successor completely before taking anything away,
  so that always leaves the source holding the seat (item 7).
- **`wrap.rs`'s own swap seam is untouched.** Its ~30 `#[cfg(unix)]` PTY tests
  cannot be compiled on the Windows machine this was developed on.
- **Native rows still rank on `Unknown`.** A native route reports no
  per-minute readings, so its four non-window dimensions remain labelled
  estimates. Ranking native routes against harnesses needs those readings
  first, which is why admission is gated on health alone.
- **The loop's billing posture is still `Api`.** Unchanged from #487's own
  deferral: over-reporting billable usage is visible to an operator,
  under-reporting is not.
