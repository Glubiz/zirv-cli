# Native workflows, verification and helper-model calls (N15)

**Date:** 2026-09-13 · **Issue:** #484 · **Roadmap:** #469

## Context

N09-N14 made a native session real: it conducts a conversation, delegates
workers, and reaches MCP servers, browsers and artifacts without a vendor CLI.
What it still could not do was be a *Zirv* session. Every model call outside
the main chat loop -- handoff distillation, `ctx ask`, `ctx optimize`'s
judgment, the loop's objective judge, the memory harvest, the independent code
reviewer, the frontend visual reviewer, the built-in agent seats -- built a
harness subprocess. Migrating the chat loop alone would have left those as
hidden Claude/Codex dependencies inside a runtime that advertises needing
neither.

Two further gaps were structural rather than incidental:

- `NativeLoop::build_request` sent `system: Vec::new()`. The native context
  compiler shipped in N06 had **no production caller at all**, so a native
  session ran with no engineering standard, no role methodology and no
  workflow context unless a human pasted them into the prompt. That is the
  opposite of "workflow adoption is automatic".
- `NativeSessionConfig::workflow_gate` was `None` with a comment naming this
  step as the one that would populate it. Until then a native session could
  declare itself finished over a Test step with stale, missing or failing
  evidence, because nothing outranked the model's own finish token.

#467 supplies the fourth piece: evidence identity. A workflow started in the
main checkout has to accept a worker worktree's evidence for the same change
set, and reject a different one.

## Decision

### One helper service, not seven provider integrations

`ctx::helper` is the single native replacement for every non-chat model call.
A helper call is always the same shape -- one bounded conversation, a
read-only tool set, a text answer -- so the interesting decisions (which
route, what budget, what a failure means) live in one reviewable place.

`handoff::helper_answer` is the chokepoint every caller reaches it through:
native first, then the existing `run_model` harness child. `run_model` itself
is unchanged, and so is every test that pins its argv-shim guard,
supervision-env scrub and tree-kill-at-the-deadline behaviour.

**Selection introduces no configuration key.** A helper runs natively exactly
when the operator's own native provider configuration names a route for the
helper's ROLE -- `distiller`, `ask`, `optimize`, `seat` under `[roles]` -- the
same `[roles]` table `zirv ctx exec --runtime native` and `zirv agent
--runtime native` already use. A machine that configured none behaves exactly
as it did. This is why `HelperError::Unconfigured` is a distinct variant: "no
route for this role" is the ordinary case and must not be reported as a
malfunction.

A native attempt that *fails* also falls back to the harness, with a warning.
These calls are best-effort by construction -- `distill_or_structural` already
degrades to a mechanical extraction -- and a native route that made them
strictly worse than none would be a regression, not a feature.

### Read-only is the broker's decision

A helper session is constructed with **no writer permit**. Every repository
write, outside write, write-effect process and shared-scope knowledge write is
then refused by `ExecutionBroker` itself, at effect time, with
`BrokerError::WriterPermit`; `ApprovalMode::Headless` means the refusal cannot
be approved away either. There is no read-only enforcement code in `helper.rs`
at all: the absent permit *is* the mechanism.

`native::session_broker` was extracted from `brokered_tools` so this contract
is asserted against the construction a real session gets rather than a
test-local copy that could drift. Extracting it surfaced a latent bug:
`discover_linked_worktree_git()` refuses a MAIN checkout outright ("native
writers require a linked worktree"), which is right for a worker granted a
tree and wrong for anything that cannot write at all -- a read-only helper or
a plain `zirv ctx exec --runtime native` in an ordinary checkout could not
previously construct a broker. It is now claimed only when a lease exists.

The same mechanism carries the seats. `--mode read-only` on `zirv agent
--runtime native` means `native_worker` takes no writer permit, so the
reviewer's read-only pin stops being an argv flag on a vendor CLI and becomes
a broker refusal. A *writable* seat is consequently refused by the native seat
dispatcher rather than quietly downgraded: promising `implementer` write
access it does not have would be worse than saying so.

### The reviewer reuses `zirv agent`, it does not reimplement it

`reviewer_argv` gained a `RuntimeKind`. Under `Native` it emits `agent <route>
- --runtime native --mode read-only --system-prompt <seat>` plus the worker
budget flags, and nothing adapter-shaped: no `--` passthrough, no model flag,
no sandbox argv floor, because there is no external process to hand them to.
The seat instructions and the budget travel exactly as they already do. The
frontend visual reviewer inherits this by construction, since it builds on the
same function.

`CapabilityReport::for_adapter` had to learn the native runtime as a seat
host; without a row it mapped every capability to `Unsupported` and
`ensure_supported` refused every native seat before it ran.

`runtime::selected` is now the one place a `--runtime` FLAG becomes a
decision. `RuntimeKind::from_str` is infallible on purpose -- it decodes
persisted values, where `Unknown` is the only safe answer -- so each new call
site would otherwise have silently accepted a typo as `harness`.

### Four workflow tools, zero new workflow logic

`workflow_status`, `workflow_context`, `workflow_advance`, `workflow_approve`
join the native registry (36 tools to 40). Each is a validated argument shape
in front of the same `workflow::engine` function the CLI verb calls, over the
same durable state, through the same gates -- the N10 delegation tools' shape
exactly. A second implementation of "what advances a step" would be a second
definition of "done", and the two would drift.

They cross the broker as `ExecutionAction::Knowledge` with `scope: "shared"`,
so the two that mutate the workflow require a writer permit for the session's
own worktree. `workflow_status` returns the completion gate's own words
alongside the step, because "would this workflow let me finish" is the fact a
session most needs and can least infer.

### Adoption is compiled, not pasted

`runtime::context::compile` is now wired into `run_session`. Its instruction
messages become the provider system prompt and its data messages one leading
user message, ahead of the journal's replay. That is what makes methodology
and workflow adoption automatic: the engineering standard, the role
methodology, the model profile, the operator's and repository's instruction
files and the active workflow's current step reach every request without
anyone seeding a prompt.

Compiled **once**, at session start, deliberately: it is the cacheable stable
prefix (`stable_prefix_sha256` exists for exactly that), and rebuilding it
every turn would defeat prompt caching to refresh something the session can ask
for explicitly through `workflow_context`. A compilation failure degrades to
no standing context with a notice, rather than failing the session.

### The gate is read live

`engine::native_completion_gate` is consulted in `finalize`, at every
completion attempt, not snapshotted at session start -- a session that reaches
the Test step after it began is gated on the evidence that exists *then*. It
is deliberately the same predicate `advance_with_evidence`'s own Test/Verify
arm applies (`verification::latest_is_fresh_and_passing`, keyed by
`WorkflowState::branch`), so #467's relatedness rule governs a native session's
completion exactly as it governs an advance: the main checkout's workflow
accepts its worker worktree's evidence for the same change set and rejects
anyone else's. The shared stop service already outranked a model finish token
with `workflow_gate`; only the wiring was missing.

It never fails the session: an unreadable state directory, an absent workflow
or an unresolvable branch all mean "nothing to gate on". A gate that blocked a
session because it could not read a file would be worse than no gate.

### Scripts

`AgentCommand` gained an optional `runtime`. `None` is `harness`, so every
script written before the field existed runs byte for byte as it did. Under
`native` the `agent` value is read as the provider route (`native` deferring
to the `[roles]` entry), and `flags` are refused rather than silently dropped
-- a script whose flags do nothing is a script whose author believes they do
something. `${var}` substitution, secrets, `options`, `fallback`,
`proceed_on_failure` and the exit-code contract are resolved by the step's
caller and are identical on both runtimes.

## What is verified

Deterministic; no provider call, no spawned harness, no network.

- A helper answers end to end through the fixture transport with `PATH`
  scrubbed empty (`helper::tests::a_helper_answers_with_every_coding_harness_
  removed_from_path`), and an unconfigured role reports `Unconfigured` rather
  than a failure.
- A helper write is refused by the broker built by `native::session_broker`
  with the same `None` lease `helper::run` passes, before the effect, while
  the same helper's read succeeds (`helper::tests::a_helper_that_tries_to_
  write_is_refused_by_the_broker`).
- The chokepoint still reaches the harness on a machine with no native route
  (`handoff::tests::helper_answer_falls_back_to_the_harness_when_no_native_
  route_exists`).
- A native reviewer argv pins read-only, carries the seat instructions and the
  worker budget, and carries no `--`, no model flag and no sandbox floor
  (`review::tests::a_native_reviewer_argv_pins_read_only_with_no_harness_
  flags`); the harness argv assertions are unchanged.
- A writable seat is refused by the native dispatcher and a read-only one gets
  as far as the route (`agents::tests::a_writable_seat_is_refused_by_the_
  native_dispatcher`); an unknown `--runtime` is an error
  (`an_unknown_dispatch_runtime_is_refused`).
- The four workflow tools are registered with closed schemas, `advance`
  declares the write capability, a session with no writer permit reads the
  workflow and is refused the advance at effect time, and a traversal-shaped
  workflow id is rejected at the argument boundary
  (`tools::tests::a_session_with_no_writer_permit_can_read_a_workflow_but_
  never_advance_it`).
- A native session's standing context carries the model profile and the active
  workflow's own task with nothing seeded
  (`native::tests::a_native_session_adopts_the_active_workflow_without_being_
  seeded`).
- A workflow started in the main checkout is satisfied -- through
  `native_completion_gate` AND through `advance_with_evidence`, so the two
  cannot drift -- by its worker worktree's evidence on its own branch, and by
  neither when that evidence names a different branch
  (`engine::tests::a_main_checkout_workflow_accepts_only_its_own_worktrees_
  evidence`, with real `git worktree add` fixtures).
- An existing script step keeps its harness runtime and flags; a native step
  needs no adapter and refuses harness flags; an unknown runtime fails at load
  time (`script_runner::agent_command::tests`).

## What is deferred

- **Live provider coverage.** Every test here is fixture- or service-level. No
  helper, reviewer seat or native workflow session was run against a real
  endpoint, so the parity table records implementation and tests, never a
  claim of validated end-to-end behaviour against a vendor.
- **Per-turn context recompilation.** The standing context is compiled once.
  A workflow that advances mid-session refreshes through the
  `workflow_context` tool rather than automatically; the completion GATE is
  live, which is the load-bearing half.
- **Native `frontend_render`.** The render itself still starts a development
  server and a browser through the N14 frontend service; only the visual
  *reviewer* moved to the native runtime here.
- **A native seat in a dashboard pane.** Unchanged from N10: a pane hosts a
  harness TUI, so a native seat always runs inline.
- **Helper spend accounting parity.** A helper's usage is reported in its own
  final status; folding it into the per-segment cost ledger is N18 (#488).
