# Native feature parity

**Issues:** #484 (roadmap N15), #485 (roadmap N16) · **Roadmap:** #469 ·
**Last updated:** 2026-09-13

Every shipped Zirv command surface and helper-model call that touches a model
or a workflow, with the native implementation that serves it and the test that
pins it. Companion to
[`native-runtime-inventory.md`](native-runtime-inventory.md), which records
*who owns* each verb and call site: this table records *what actually runs*
natively today and what still requires a coding harness.

Every row is one of three states, and the vocabulary is deliberate:

- **native** — runs with no coding-harness binary on `PATH`. The test named is
  deterministic and fixture-backed.
- **shared** — the same zirv-owned code runs on both runtimes; there is no
  harness dependency to remove and nothing to port.
- **harness-only** — still requires a vendor CLI, with the reason. There are
  no bare "later" entries: every one names why, and the roadmap step or design
  note that owns it.

No row claims validated live-provider behaviour. Every test below is fixture-
or service-level; a real endpoint has not been exercised (see "What is
deferred" in
[`2026-09-13-native-workflows.md`](2026-09-13-native-workflows.md)).

## Model-calling helpers

| Helper | Native implementation | State | Test |
|---|---|---|---|
| Handoff distillation (`handoff::distill`) | `handoff::helper_answer` → `ctx::helper` at role `distiller` | native | `handoff::tests::helper_answer_falls_back_to_the_harness_when_no_native_route_exists`, `helper::tests::a_helper_answers_with_every_coding_harness_removed_from_path` |
| `zirv ctx ask` | `handoff::helper_answer` at role `ask` | native | as above (same chokepoint) |
| `zirv ctx optimize` judgment pass | `handoff::helper_answer` at role `optimize` | native | as above |
| Agent-loop objective judge (`run_loop::evaluate_objective_after_cycle`) | `handoff::helper_answer` at role `distiller` | native | as above |
| Memory durable harvest (`memory::harvest_durable_with_tool_errors`) | `handoff::helper_answer` at role `distiller` | native | as above |
| Memory consolidation (`memory_optimize::apply_consolidation`) | `handoff::helper_answer` at role `distiller` | native | as above |
| Independent code reviewer (`workflow review run`) | `review::reviewer_argv(RuntimeKind::Native, ..)` → `zirv agent --runtime native --mode read-only` | native | `review::tests::a_native_reviewer_argv_pins_read_only_with_no_harness_flags` |
| Frontend visual reviewer (`workflow frontend review`) | same argv builder, `--runtime native` | native | covered by the reviewer argv test it shares; the render half is harness/browser-configured (below) |
| Built-in agent seats (`workflow agents dispatch`) | `agents::dispatch_native_seat` → `ctx::helper` at role `seat` | native, read-only seats only | `agents::tests::a_writable_seat_is_refused_by_the_native_dispatcher` |
| Auto-spawn on a workflow gate (`engine::spawn_auto_worker`) | re-execs `zirv workflow review run` / `test` / `verify`; inherits whichever runtime those resolve | shared | covered by the reviewer and verification rows |
| Native agent loop (`NativeLoop::stream_once`) | itself | native | `runtime::native::tests` (N09, #478) |
| Delegated workers (`zirv agent --runtime native`) | `ctx::native_worker::run` | native | `runtime::tools::delegation` suite (N10, #479) |

## Workflow surface

| Command / step | Native implementation | State | Test |
|---|---|---|---|
| `workflow start` / `status` / `show` / `list` / `resume` / `close` | `workflow::engine`, runtime-independent | shared | existing `workflow::engine::tests` |
| `workflow advance` (intent, spec, plan, implement, test, review, verify, deploy) | `workflow::engine::advance_with_evidence`; natively addressable through the `workflow_advance` tool | shared + native tool | `tools::tests::a_session_with_no_writer_permit_can_read_a_workflow_but_never_advance_it` |
| `workflow approve` | `engine::approve`; `workflow_approve` tool | shared + native tool | as above |
| `workflow context` | `engine::render_current_context`; `workflow_context` tool | shared + native tool | as above |
| Workflow/skill adoption | `runtime::context::compile` wired into `run_session` | native | `native::tests::a_native_session_adopts_the_active_workflow_without_being_seeded` |
| Completion gating on verification freshness | `engine::native_completion_gate`, read live in `NativeLoop::finalize` | native | `engine::tests::a_main_checkout_workflow_accepts_only_its_own_worktrees_evidence` |
| Evidence identity across worktrees (#467) | `verification::latest_is_fresh_and_passing` keyed by `WorkflowState::branch` | shared | as above, plus the #467 suite in `verification::tests` |
| `workflow classify` / `reclassify` | `workflow::classify`, deterministic git measurement | shared | existing `workflow::classify::tests` |
| `workflow maintain scan` | `workflow::maintain`, deterministic detectors | shared | existing `workflow::maintain::tests` |
| `workflow artifacts` (registration and acceptance) | `workflow::artifact`; `artifact_register` / `artifact_present` tools (N14) | shared + native tools | `tools::tests::every_capability_tool_parses_through_the_same_closed_registry` |
| `workflow agents list` / `show` | `workflow::agents` registry, no model call | shared | existing `workflow::agents::tests` |
| `workflow frontend profile` / `check` / `benchmark` | `workflow::frontend*`, deterministic | shared | existing `frontend_detector::tests` |
| `workflow frontend render` | N14 frontend service (development server + configured browser) | harness-only when no browser capability is configured | `workflow::capability` integration rows; the render itself is configuration-gated, not harness-gated — see N14 |
| `workflow review package` / `add` / `dispose` / `list` / `ingest-pr-comments` | `workflow::review`, no model call | shared | existing `workflow::review::tests` |
| `workflow stats` | `workflow::telemetry` | shared | existing `workflow::telemetry::tests` |

## Team and coordination surface

Added by N16 (#485): the coordinating seat itself, and the shared services a
team runs on. Every row is reachable from a native session as a typed tool
over the same durable state the CLI verb writes.

| Surface | Native implementation | State | Test |
|---|---|---|---|
| The coordinating seat's role and methodology | `ctx::team::prompt_role` → `runtime::context::compile` | native | `team::tests::only_the_coordinating_roles_get_an_orchestrator_methodology` |
| Role-to-route selection (`coordinator`, `sub-orchestrator`, `researcher`, `planner`, `implementer`, `reviewer`, `tester`) | `ctx::team::route_for_role` over `[roles]`, typed refusal when unconfigured | native | `team::tests::a_role_with_no_entry_is_a_typed_refusal_naming_the_roles_that_have_one` |
| A route a delegating model names for a role | `ctx::team::authorize_route` → N18 `route::eligible` | native | `team::tests::a_requested_route_may_not_move_a_role_onto_different_billing`, `team::tests::operator_policy_outranks_the_role_table` |
| Delegation bounds (role authority, depth, stopped objective) | `ctx::coordinator::check`, applied in `delegation::delegate` before the receipt | shared | `delegation::tests::a_refused_delegation_starts_nothing_and_writes_no_receipt` |
| Child write posture from role identity | `ctx::coordinator::Grant` | shared | `delegation::tests::the_childs_mode_is_decided_by_its_role_not_by_the_request` |
| `zirv ctx task create` / `claim` / `list` | `ctx::task`; `task_create` / `task_claim` / `task_list` tools | shared + native tools | `tools::tests::two_workers_can_never_claim_one_card` |
| `zirv ctx group create` / `status` | `ctx::group`; `group_create` / `group_status` tools | shared + native tools | `tools::tests::a_native_coordinator_runs_a_mixed_team_through_the_shared_services` |
| `zirv ctx objective show` | `ctx::objective`; `objective_status` tool, with the operator's constraints | shared + native tool | `tools::tests::operator_steering_and_stopping_reach_the_coordinator` |
| The coordinator's task graph, decisions, evidence refs and pending completions | `ctx::coordinator` record; `team_status` tool | native | `coordinator::tests::a_restarted_coordinator_consumes_pending_receipts_exactly_once` |
| Coordinator restart | `coordinator::consume_pending` at the top of `native::run_session` | native | as above, plus `native::tests::a_coordinator_session_resumes_its_graph_and_settles_a_node_exactly_once` (drives `run_session` itself, not just `consume_pending`) |
| Operator steering / stopping (`zirv ctx objective set` / `close`) | writes the constraint and the stop onto the coordinator record | shared | `tools::tests::operator_steering_and_stopping_reach_the_coordinator` |
| A whole team with no coding harness on `PATH` | all of the above | native | `tools::tests::an_all_native_team_runs_a_workflow_with_every_coding_harness_absent` |
| A mixed native + wrapped team on one board | `ctx::delegation` on both runtimes, one coordinator record | native + harness | `tools::tests::a_native_coordinator_runs_a_mixed_team_through_the_shared_services` |

## Verification and scripts

| Surface | Native implementation | State | Test |
|---|---|---|---|
| `zirv test` / `zirv verify` / `test baseline` / `test changed` | `workflow::verification`, deterministic runner | shared | existing `workflow::verification::tests` |
| Script `agent:` step | `AgentCommand.runtime: native` → `exec::run_with` with `--runtime native` | native | `script_runner::agent_command::tests::a_native_step_needs_no_adapter_and_refuses_harness_flags` |
| Script `command:` steps, `${var}`, secrets, `options`, `fallback`, exit codes | unchanged; resolved by the step's caller on both runtimes | shared | `script_runner::agent_command::tests::an_existing_script_step_keeps_its_harness_runtime_and_flags` plus the unchanged `script_runner::` suite |
| `zirv ctx exec --runtime native` | `runtime::native::run_headless` | native | `runtime::native::tests` (N09, #478) |
| `zirv ctx loop` | spawns `zirv ctx exec` per cycle; a native cycle is a native exec | shared | `run_loop::tests` |

## Still harness-only

| Surface | Why | Owner |
|---|---|---|
| `zirv ctx wrap` (interactive PTY supervision) | wraps a vendor TUI process by definition; a native session has no TUI | N11 (#480) for the native interactive surface |
| Dashboard panes hosting a native session | a pane hosts a harness TUI | N11 (#480) |
| Transcript scoring / rot for native sessions | the native journal projects into the same scoring vocabulary; a *harness* transcript parser is still adapter-specific | N09 (#478), unchanged here |
| Cross-harness handover (`zirv ctx handover`) | swaps one vendor CLI for another, which a native session has none of; changing a native seat's model is `[roles]`/`--route` configuration, not a handover | roadmap #469, unassigned — N16 (#485) covers team orchestration, not vendor-CLI swapping |
| Writable built-in agent seats on the native seat dispatcher | the dispatcher is read-only by mechanism; a writable seat must be a delegated worker with a real permit (`zirv agent --runtime native --mode writing`) | by design, #484 |
| Live-provider validation of any row above | every test here is fixture- or service-level | N19 (#487) validation pass |
