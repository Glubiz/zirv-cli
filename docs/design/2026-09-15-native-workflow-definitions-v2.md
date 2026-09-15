# Native workflow definitions v2: declarative schema, layered registry, state compatibility, selection, and first-wave packs (#542, all chunks)

**Date:** 2026-09-15 · **Issue:** #542 (chunks 1+2+3a+3b+4+5, complete) ·
**Worktree:** `native/542`, based on `release/native-harness`

Chunks 1+2 (the schema, the registry, state compatibility) were accepted
with one required follow-up: chunk 3a below replaces the deferral in
"§4. Deliberately NOT done in this chunk", making v2 materialization THE
production execution path. Chunk 3a was accepted ("the parity proof over
960 combinations is exactly what was needed"). Chunks 3b (selection,
`adaptive-work`, native tools/slash commands), 4 (the ten first-wave
professional packs) and 5 (the engine fix for a genuinely artifact-less
approval gate, plus the remaining sixteen packs) are covered in their own
sections below, appended in the same sessions that implemented them.

## Context

#542 wants to grow the native harness from a five-kind software-development
workflow engine into an extensible work engine, without hand-coding a new
Rust match arm per recurring work pattern. This chunk builds the foundation
the later chunks (selection/`adaptive-work`/native tools -- chunk 3;
professional packs -- chunk 4) sit on: a versioned declarative
`WorkflowDefinition v2` schema, a layered/trust-checked registry over it, and
state-schema compatibility so existing in-flight workflows are unaffected.

A parallel worker on `native/541` is adding `src/commands/workflow/
{profile,team}.rs` and `zirv workflow team` in the same base; this chunk's
`engine.rs` edits were kept as localised as possible (three call sites plus
two struct/field additions) to keep that rebase clean, and `agents.rs`/
`classify.rs` were not touched beyond what already existed.

## Decisions

### 1. `WorkflowDefinitionV2` (`src/commands/workflow/definition.rs`)

A `#[serde(deny_unknown_fields)]` struct with `schema_version` (format
version, `1`, independent of `engine::WORKFLOW_SCHEMA_VERSION`, the *state*
schema), `id`/`version`/`title`/`description`, free-form `domains` tags,
example `triggers`, typed `inputs`/`outputs`, `steps: Vec<StepV2>`, `gates`
(approval/validation/independent-review step-id sets), `limits`, a `failure`
policy, a top-level `effects` ceiling (`none|repository|external`), an
optional `idempotency` note, a `completion` contract, and an operator-only
`override` flag (see registry, below). `StepV2` carries `id`, `title`,
`phase` (reused `skill::WorkflowPhase`), `skills`, an optional `agent_role`
(a string role name, resolved against the live `AgentRegistry` at
MATERIALISE time -- not here, kept separate from the dispatch-time
writable-seat refusal per the issue), `capabilities` (reused
`capability::CapabilityId`), `depends_on`, an optional `parallel_group`,
`condition` (reused `engine::StepCondition`), `approval`, `artifact` (reused
`engine::ArtifactStage`), `max_attempts`, `effect`, and a human-readable
`reason`.

`validate(&self, known_skill_ids: &BTreeSet<&str>)` is purely structural: id
shape (`[a-z0-9][a-z0-9._-]*`), version/title/description non-empty, size
(32 KB cap), step id uniqueness, unknown-skill detection (against the
caller-supplied set -- see below), unknown/self dependency, a DFS cycle
check, an unreachable-step check, and gate-reference resolution. It
deliberately takes the known skill ids as a parameter rather than a live
`SkillRegistry`: `definition.rs` stays testable standalone, and the actual
registry cross-check is `registry.rs`'s job at load time.

**Unreachable steps, precisely.** Given a cycle-free graph with every
`depends_on` already resolved, forward reachability from "every step with an
empty `depends_on`" can never actually fail -- finite + acyclic means walking
any step's `depends_on` backward always terminates at such a root. So
"unreachable" is defined as: treating `depends_on` as UNDIRECTED, every step
must share a connected component with `steps[0]` (the definition's anchor).
This catches a genuinely stray, forgotten step (no `depends_on`, and nothing
else names it as one) while still allowing several genuine parallel entry
points, as long as something downstream eventually depends on each of them.
Documented as a deliberate interpretation in the function's own doc comment,
since the issue text ("not root and not reachable from a root") is provably
unsatisfiable by construction under the more literal directed reading.

`hash()` is sha256-hex of `serde_json::to_value(self)` re-serialized to a
string: this crate never enables `serde_json`'s `preserve_order` feature, so
`serde_json::Map` is `BTreeMap`-backed and keys are always emitted sorted --
the hash is a function of the deserialized VALUE, never of whatever key order
the source TOML/YAML happened to use. Proven by a test that parses the same
logical definition from two TOML documents with completely different root
key orders and asserts equal hashes.

### 2. Layered `WorkflowRegistry` (`src/commands/workflow/registry.rs`)

Mirrors `skill::SkillRegistry`/`agents::AgentRegistry` exactly: built-ins
(compiled in via `include_str!` from `packs/*.toml`, never touch disk) →
`~/.zirv/workflows/*.{toml,yaml,yml}` (operator-global, trusted; may replace
a built-in id ONLY when the file itself sets `override = true`, otherwise
dropped with a warning) → `<repo>/.zirv/workflows/*.{toml,yaml,yml}`
(repository, untrusted, gated by the new `workflow.repo_workflows_enabled`,
default `false`). Same symlink/path-escape/size(32 KB)/entry-count(512)
defenses as `skill::load_dir`.

The repository layer additionally cannot WIDEN authority beyond what some
already-registered built-in pack exercises (`widening_violation`):

- `effects = "external"` is always refused (no built-in ever uses it);
- a step declaring `repo.write`/`shell.exec`/`network.access`/`agent.spawn`
  that no built-in step anywhere declares is refused;
- for a same-domain built-in (`domains` intersects), dropping an
  approval/validation/independent-review gate CATEGORY that built-in
  establishes is refused (a coarse "does this domain still have the gate
  kind at all", not an exact per-step match).

A widening violation, like an id collision, is dropped with a warning rather
than hard-failing the whole untrusted layer -- one hostile file must not take
down every other legitimate repository pack. Symlink/escape/size/schema
failures remain hard errors, matching `skill.rs`'s own asymmetry between
"this file is malicious/broken" (fail the load) and "this file collides/
widens" (warn and skip).

### 3. Five built-in packs (`src/commands/workflow/packs/*.toml`)

`feature`, `bugfix`, `refactor`, `spike`, `review` reproduce the exact step
ids/phases/primary-skills/conditions/approvals/artifacts the pre-#542
`engine::definitions()` literal vec encodes, plus new v2-only metadata
(`capabilities` mirrored from each step's primary skill's own
`required_capabilities`, a linear `depends_on` chain matching the legacy
sequential order, `gates`, `limits`, `failure`, `effects`, `completion`).
Every pack round-trips through `WorkflowRegistry::load` and its own
`validate()` (test: `every_builtin_pack_parses_and_validates`).

### 4. `materialize()` is now THE execution path (chunk 3a)

Chunk 1+2 originally deferred replacing `materialize()`, judging the
93-call-site blast radius of touching `WorkflowState::start` too risky to
rush. The coordinator overruled that deferral as not optional ("acceptance
bullet 2 ... is only true when a v2 definition actually drives
`materialize()`"); see the "Chunk 3a" section below for the completed
design and how the blast radius was actually contained.

### 5. State compatibility (`engine.rs`)

`WORKFLOW_SCHEMA_VERSION` 4 → 5. `WorkflowState` gains `definition:
Option<DefinitionRef>` (`#[serde(default)]`), where `DefinitionRef` carries
`id`/`version`/`hash`/`source_layer`, plus `inline: Option<WorkflowDefinitionV2>`
populated only for a non-built-in pack (a repository/operator pack file can
be edited or deleted out from under a running workflow; a built-in is
versioned with the binary itself and therefore stable for the run's life,
so only the non-built-in case needs its own copy). `load()` now accepts
schema `4` (upgrades in place -- `#[serde(default)]` already gives it
`definition: None`, kind-only v1 semantics, unchanged) alongside the current
`5`, and still hard-errors on anything else. `WorkflowState::start` is
UNCHANGED (still takes a `WorkflowKind`, still used by every existing
caller/test); the CLI `Start` handler best-effort populates `state.definition`
AFTER construction by resolving the matching built-in pack from the registry
-- a registry load failure there never blocks starting the workflow itself.
`zirv workflow status` prints the pin (`id@version (hash-prefix) [layer]`)
and, on a best-effort re-resolution against the CURRENT registry, a
`definition drifted from registry` note when the hash no longer matches (or
the id no longer resolves at all).

### 6. CLI (`engine.rs`)

`ShowArgs`/`StartArgs`'s positional changed from a closed `WorkflowKind`
`ValueEnum` to a plain `String` id (`OutputArgs`/`ShowArgs` also gained
`--built-in-only`/`--repo`, matching `skill`/`agents` list/show). `workflow
list`/`show` now resolve through `WorkflowRegistry` unconditionally -- the
five kind spellings are registry ids like any other, so no special-casing is
needed. `workflow start <id>` resolves `id` via `WorkflowKind::from_pack_id`
first (the five executable kinds); for any other id it distinguishes
"registered but not yet executable" from "unknown workflow '<id>'" (the
latter phrasing matches `engine::load`'s existing convention for an unknown
STATE id, since clap can no longer reject an unknown value at parse time now
that the positional is a plain string).

## What is verified (chunks 1+2, still true)

- `definition.rs`: round trip (hand-authored TOML fixture, matching the real
  pack shape, plus a `serde_yaml_ng` round trip), cycle rejection, unreachable-
  step rejection, unknown skill/dependency rejection (plus a closed-enum
  capability parse failure), and hash stability across differently-ordered
  TOML source documents.
- `registry.rs`: every built-in pack parses/validates; an operator pack only
  overrides a built-in with `override = true`; a repository pack can neither
  shadow a built-in id nor widen `effects`; repository packs are inert unless
  `repo_workflows_enabled`; symlink/oversized manifests are hard refusals; a
  hostile-repo fixture proves both the capability-widening and gate-dropping
  refusals independently.
- `ZCHK-FORBIDDEN-WIDENING`, `ZCHK-RUNTIME-INVENTORY`, `ZCHK-NATIVE-PARITY`
  (`cargo run -- verify --builtin`) all pass with the new config key and CLI
  shapes.

## Chunk 3a: v2 materialization is THE execution path

### 1. `materialize_from_definition` replaces the literal pipeline

New pure function: prunes `WorkflowDefinitionV2` steps by `StepCondition`
(via a shared `condition_applies` helper, so `WorkflowStep::applies` and
`StepV2` pruning can never drift), resolves each surviving step's data via
`select_step_data` (below), orders the result by `depends_on` with Kahn's
algorithm (ties broken by declaration order in the pack -- a stable sort),
then applies `apply_brainstorm_selection`/`apply_deploy_tier` exactly as
before (both were ALREADY keyed on `WorkflowPhase`, never on `WorkflowKind`,
so they needed no change at all -- only `apply_profile` was kind-keyed).
`WorkflowStep` gained `parallel_group: Option<String>` and `effect:
EffectClass` (`#[serde(default)]`, backward compatible with persisted
state), carried straight from the selected `StepV2`.

### 2. Domain variants replace the hardcoded profile match table

`StepV2` gained `domains: Vec<String>` and `overrides_step: Option<String>`.
A step with `overrides_step = Some(id)` is a DATA VARIANT of the step named
`id`, not its own DAG node (`WorkflowDefinitionV2::validate` excludes it
from cycle/unreachable/depends_on checks, but still requires it to name a
real, non-variant sibling and to declare no `depends_on` of its own).
`select_step_data(definition, primary, profile)` picks the variant whose
`domains` contains `"frontend"` when `profile == Frontend`, else `primary`
itself -- `WorkflowProfile::Standard` always resolves to `primary`, matching
the old table's implicit "no override" default. The materialized step keeps
`primary`'s `id`/`phase`/`depends_on`/`parallel_group`/`condition` always
(so a profile switch never changes a step's id -- pinned by the pre-existing
`reclassified.current().unwrap().id == "plan"` assertion, still green), and
takes `skills`/`agent_role`/`capabilities`/`approval`/`artifact`/
`max_attempts`/`effect` from whichever of {primary, variant} was selected.
Every phase-bearing step across all five built-in packs (roughly 19 step
slots) got a `-frontend` variant reproducing the old table's skill mapping
(`design → frontend-design`, etc.) with IDENTICAL `approval`/`artifact`/
`effect`, proven exactly by decision 3's parity test.

`apply_profile` (used by `set_profile`/`workflow reclassify` and
`reclassify_at_gate`'s automatic mid-run Frontend detection) was rewritten
to re-run `select_step_data` per already-materialized step id against a
resolved `WorkflowDefinitionV2`, in place -- `resolve_definition_for_state`
supplies that definition from the pinned inline copy, else a fresh built-in
lookup by pinned id, else (a v1/schema-4 state with no pin) a built-in
lookup by `state.kind`, so this always has something valid to consult. A
review finding on the OLD `apply_profile` (Design-phase approval not
restored leaving Frontend) turned out to be a no-op for all five real
packs once traced through (feature's own Design step is artifact-gated and
was never touched by that branch; spike's is `approval = false` either
way) -- the new implementation reproduces this by having each domain
variant simply declare the SAME `approval` its primary does, verified by
the existing `apply_profile_restores_the_kind_default_design_approval_when_
leaving_frontend` test, ported to the pack-driven API.

### 3. The parity oracle, and containing the ~90-call-site blast radius

`converted_packs_materialise_identically_to_the_legacy_literals` compares
`materialize_from_definition` (the five built-in packs) against
`legacy_materialize` -- an EXACT, `#[cfg(test)]`-only copy of the deleted
pre-#542 `apply_profile`/`materialize` bodies, kept purely as the oracle --
across 5 kinds × 2 profiles × 4 complexities × 4 risk bands × 3 deploy tiers
× 2 brainstorm settings (960 comparisons), asserting `(id, phase, skill,
agent, artifact, approval, max_attempts)` AND order match exactly.
`definitions()`/`definition()`/`WorkflowDefinition` (the v1 struct) are now
`#[cfg(test)]`-gated -- absent from the production binary, per the issue's
own instruction.

`WorkflowState::start(repo, task, kind: WorkflowKind, ...)` keeps its EXACT
pre-#542 signature and stays infallible -- the ~90 call sites across
`agent.rs`, `compile.rs`, `handoff.rs`, `hook.rs`, `prompt.rs`, `runtime/
{context,native}.rs`, `runtime/tools/mod.rs`, `workflow/{maintain,mod,
review}.rs` (mostly unrelated-module test fixtures) needed ZERO changes.
Internally it now resolves a `WorkflowDefinitionV2` via a best-effort,
registry-aware lookup (`resolve_builtin_or_registry`: tries the live
registry so an operator's `override = true` pack still wins, falls back to
the embedded built-in text on any failure) and calls a new private
`start_with_definition` core. A second, new constructor,
`WorkflowState::start_from_pack(repo, task, pack: &RegisteredWorkflow, ...)`,
starts from an already-resolved registry entry for ANY id (used by the CLI
`Start` handler); it maps the pack's id back to a legacy `WorkflowKind` via
`WorkflowKind::from_pack_id` for the vestigial `kind`/brainstorm-default
fields, defaulting to `Feature` for a pack with no legacy counterpart --
`state.definition` (not `state.kind`) is the authoritative record of what
actually ran, per the issue's own "`WorkflowKind` remains the legacy id set
... nothing else keys on it".

### 4. `workflow start <id>` executes any registry id

The CLI `Start` handler now: resolves `id` through `WorkflowRegistry` (the
existing `"unknown workflow '<id>'"` phrasing, unconditionally -- no more
"registered but not yet executable" carve-out), materializes once for
preflight (skill/capability checks, gated on `--agent`, unchanged), then
checks EVERY step's `agent_role` resolves in the (registry-aware)
`AgentRegistry` -- unconditionally, independent of `--agent` -- before
calling `WorkflowState::start_from_pack`, so an unknown role fails before
`save` ever runs (`an_unknown_agent_role_fails_at_materialise_before_state_
is_written` proves no active-workflow pointer exists afterward). A v2-only
pack with two independent steps sharing a `parallel_group` tag, plus a
third step depending on both (so `WorkflowDefinitionV2::validate`'s
connectivity rule is satisfied -- true concurrent execution needs a state-
machine rewrite this chunk does not attempt; `parallel_group` is carried as
informational metadata only, per decision 1), starts and advances through
this exact path end to end
(`a_v2_pack_with_parallel_steps_starts_and_advances_through_the_engine`).

### 5. Drift and the native completion gate, reconfirmed

`a_pinned_definition_survives_registry_drift` and
`a_schema_four_state_file_still_loads_resumes_and_advances` (both already
designed in chunk 1+2, now actually written) prove the pin persists through
save/reload and that a genuine pre-#542 v4 state file still loads, resumes
and advances end to end. `native_completion_gate_and_advance_with_evidence_
still_agree` proves `native_completion_gate` (the native loop's own
completion check) and `advance_with_evidence`'s Test-phase evidence gate
still agree, both before and after fresh passing evidence exists, on a
workflow materialized through the new pipeline -- neither function's own
logic changed in this chunk, but both consume `WorkflowState.steps`, which
now comes from a different construction path.

## What was deferred out of chunks 1+2+3a (now addressed below)

- Chunk 3b: selection, the `adaptive-work` generic fallback, `/workflows`/
  `/workflow` native UI, native tools beyond the existing
  `workflow_status|context|advance|approve` wrappers. **Done -- see below.**
- Chunk 4: the project-management/data/architecture/devops/SRE professional
  packs. **Done -- see below.**
- True concurrent execution of `parallel_group` steps -- the state machine
  is still a single `current_step` sequence; the tag is carried as
  informational metadata for a future scheduler (chunk 3a decision 1). Still
  deferred; no chunk in #542 asked for it.
- Typed Linear/Kibana/cloud-ops tools (#539) -- still deferred, and now the
  reason a handful of chunk-4 steps stop at an explicit gate instead of
  acting (see chunk 4, below).

## Chunk 3b: selection and native surfaces

### 1. Deterministic selection, no model call

`selection::select_definition(classification, registry, objective) ->
Selection` scores every registry pack against `objective` (the `--task`
text) and the resolved `Classification`: +3 per `triggers` phrase that
appears in the task text, +2 per `domains` tag that appears in the task
text, +1 when the classified work-domain matches a `domains` entry. Scores
below `SELECTION_FLOOR` (2) are dropped; the highest scorer wins; a tie is
broken toward the lower `EffectClass` (an `Ord` on `none < repository <
external`) and then alphabetically by id -- both the winning score's
rationale and, when a tie was actually broken, the discarded alternatives
are recorded in `Selection::reasons`/`alternatives` so the choice is always
explainable after the fact, not just reproducible. A `Classification.intent`
that already maps onto one of the five legacy kinds (`WorkflowKind::
from_intent`, the exact reverse of the existing `intent()` method) selects
that kind's pack outright with confidence `1.0` and skips scoring entirely
-- the pre-#542 selection behavior for `feature`/`bugfix`/`refactor`/
`spike`/`review` is unchanged bit-for-bit, not merely "usually still
picked". Nothing scoring above the floor falls back to `adaptive-work` at
confidence `0.0`, itself explained in `reasons`.

**Why deterministic is sufficient, and why a model tie-break stays
deferred.** Every one of the six required-test scenarios (legacy intent,
clean win, floor-miss fallback, an explicit tie, a trivial task pruning
`adaptive-work`, and an explicit id override) is *already* fully explained
by `reasons` alone: the scoring is bounded, small (16 packs), and cheap
enough to reference in test assertions verbatim. A model call would only
plausibly change the outcome in the narrow band right at `SELECTION_FLOOR`
-- exactly the region a human operator can least afford an unreviewable
"the model decided" outcome for, since it is by construction the least
clear-cut case. Until there is a concrete instance of the deterministic
algorithm choosing wrong in a way a bounded model call would catch (and an
operator who wants to spend a call on it), adding one is speculative
machinery for a problem not yet observed; a bounded tie-break stays a
documented future option, not a chunk-3b requirement.

### 2. `adaptive-work`, the generic fallback

Five steps -- `understand` (intent, `write-intent`) -> `plan` (condition
`complexity-at-least bounded`) -> `execute` (depends on both) -> `validate`
(condition `complexity-or-risk{bounded,medium}`) -> `present` -- with
`domains = []` and `triggers = []` so it never competes for another pack's
score (only the floor-miss fallback path ever selects it), `effects =
"none"`, and no approval gates: an unmatched task should be picked up
immediately, not blocked on a gate a never-scored pack never earned.
`a_trivial_adaptive_task_prunes_to_three_steps` proves the two conditional
steps drop out for a trivial/low-risk classification, leaving exactly
understand/execute/present.

### 3. Wiring `workflow start`/`classify` and durable override

`StartArgs.id` is now `Option<String>`. The entire CLI `Start` handler body
(registry load, selection when `id` is `None`, classification, materialize,
preflight, `WorkflowState::start_from_pack`, artifact template, save,
telemetry) was extracted into a new `pub fn start_workflow(state_dir,
args) -> CtxResult<StartOutcome>` -- `WorkflowState::start`'s own signature
is untouched (per chunk 3a's ~90-call-site containment), so this is a
refactor, not a new code path grafted alongside the old one; the CLI
handler and the new native tool/slash command are now three callers of the
exact same function. `StartOutcome{state, selection, work_dir_gitignored}`
carries `selection: Option<Selection>` -- `Some` only when no id was given.
`--json` output gets a `selection` object spliced in (never a separate top-
level field the schema would need to reserve); text output gets a `selected:
<id> (<reasons>)` line. `workflow classify --json`/text gained the same
`selection` field/line as a side-effect-free preview (best-effort: an
unreadable registry silently omits it rather than failing `classify`
outright) -- issue #542's own field, additive alongside whatever `profile`
field the parallel `native/541` branch adds, per the brief's instruction to
add only `selection` here and let the two merge mechanically. An explicit id
always wins outright (no selection performed at all) and is pinned on
`state.definition` exactly as chunk 3a already established, so it survives
`resume`/`reclassify` unchanged.

### 4. Native tools `workflow_list`/`workflow_start`

Two new entries in `runtime::tools`, mirroring the four pre-existing
workflow tools' registration shape exactly (constant, `parse!` arm,
`ParsedTool` variant, `validate`, `action`, `retry_policy`, dispatch,
catalog `definition`). `workflow_list` is `Knowledge{write: false, scope:
"shared"}`/`RetryPolicy::Safe`, read-only like `workflow_status`.
`workflow_start` is `Knowledge{write: true, scope: "shared"}`/
`RetryPolicy::Reconcile` -- a shared-scope write, so the broker's existing
writer-permit check (unchanged, not reimplemented) refuses it without a
live permit (`workflow_start_tool_requires_a_writer_permit`). Both call
`engine::start_workflow`/`WorkflowRegistry::load_for_repo` directly, so
`workflow_list_and_start_tools_match_the_headless_json` can assert their
JSON output is byte-identical to the same calls made directly -- there is
no separate native-tool-only formatting to drift.

### 5. Native slash commands `/workflows`, `/workflow <id>`, `/workflow status`

Three of `workflow::engine`'s plain-text renderers (the `workflow list`/
`show`/`status`/`start` CLI branches' non-JSON output) were extracted into
shared `pub(crate)` functions -- `write_registry_list`, `write_registry_
entry`, `write_state` (already existed, now `pub(crate)`), `write_
definition_status` (ditto), and a new `write_start_outcome` -- so the CLI
and the native pane call the literal same code, not two implementations
that happen to agree today. `NativePaneRuntime::handle_composer_action`
recognises `/workflows` and `/workflow ...` the same way it already
special-cases `/status` (needs live `self.repo`/`self.state`, so it cannot
live in the pure `apply_slash_command` helper): `/workflows` lists the
registry; `/workflow status [id]` shows a running workflow (defaulting to
this repo's active one); `/workflow <id>` shows that pack's definition when
given alone (read-only, mirrors `workflow show`) and STARTS it when trailing
text supplies a task (mirrors `workflow start <id> --task ...`) -- "start or
show" per the brief, disambiguated by whether a task was actually typed, so
a bare id keystroke never has a side effect. `SLASH_COMMANDS` gained two
entries, appended after the existing four, kept additive so the parallel
`native/541` worker's `/agents`/`/agent`/`/team` rows rebase mechanically.
`workflow_views_render_the_headless_structs` proves all three notices equal
the same writer functions called directly over the same registry/state.

## Chunk 4: first-wave professional packs

Nine (plus `adaptive-work`'s five from chunk 3b, sixteen total) built-in
packs under `src/commands/workflow/packs/`, one per professional group named
in the brief: `pm-requirements`, `pm-status-report`, `data-question-to-
report`, `data-quality-investigation`, `architecture-decision-record`,
`architecture-design-review`, `sre-incident-triage`, `devops-ci-cd-change`,
`dependency-upgrade`, and `security-remediation` (fit the same effect
bound, so shipped too). Every step references only skill ids that already
exist (`write-intent`, `write-plan`, `implement`, `testing`, `review`,
`verify`, `design`, `finish-branch`) and agent roles from the #541 roster
(`architect`, `data-analyst`, `devops-sre`, `planner`, `researcher`,
`tester`, `reviewer`, `security-scanner`) -- `no_builtin_pack_references_an_
unknown_skill_or_role` cross-checks every built-in pack against a `KNOWN_
ROLES` allowlist (the #541 `AgentRegistry` roster has not merged into this
worktree yet, so the allowlist is this chunk's own fixed list rather than a
live import; it will need reconciling against the real roster once #541
lands -- a follow-up, not a chunk-4 gap, since every role used here is one
the brief itself names as available). No pack's `effects` exceeds
`repository`: `devops-ci-cd-change`, `dependency-upgrade`, and
`security-remediation` are `repository` (they edit files in this checkout);
the rest are `none`. `sre-incident-triage` is read-only diagnosis by design
-- its `mitigation-gate` step stops at an explicit approval rather than
acting, both because any real mitigation is a production mutation and
because live log/deploy-history investigation needs the Kibana integration
#539 has not shipped yet; `a_pack_needing_a_missing_integration_stops_at_an_
explicit_gate` proves the step's `reason` names the missing integration
rather than silently proceeding on partial evidence.

### A genuinely gate-only, non-artifact approval step needed a real engine fix (chunk 4 workaround, fixed in chunk 5)

Building the end-to-end fixtures surfaced a real bug, not a fixture-only
quirk: `refresh_deploy_tier` (called at the START of both `approve` and
`advance_with_evidence`) unconditionally recomputes `state.status` from the
CURRENT step's own `approval` field. For an artifact-gated approval step,
`approve`'s artifact branch advances `current_step` before this matters, so
the recompute sees the NEXT step. For a plain gate-only approval step (no
`artifact`), `approve`'s fallback branch sets `status = Running` without
advancing `current_step` -- the same still-current step is still `approval =
true`, so the very next call re-derives `AwaitingApproval` from it and
`advance_with_evidence`'s own guard immediately refuses. `Deploy`-phase
steps never hit this: `apply_deploy_tier` unconditionally overrides a
Deploy-phase step's `approval` from the resolved deploy tier regardless of
what the pack authored, which happens to sidestep the bug rather than fix
it. Chunk 4's fix was narrow and pack-level, not an engine change: both
packs that had a plain gate-only approval step (`pm-requirements`'s
`brief-approval`, `sre-incident-triage`'s `mitigation-gate`) declared
`artifact = "plan"` as a workaround, routing them through the already-proven
artifact-approval mechanism instead. **Chunk 5 replaces that workaround with
the real engine fix** -- see "Chunk 5" below; both packs have since dropped
the workaround `artifact = "plan"` field, and a genuinely artifact-less
approval gate is now a fully supported step shape used throughout the
chunk-5 packs (e.g. `pm-cycle-planning`'s `committed-plan`,
`devops-infrastructure-change`'s `apply-gate`).

### End-to-end fixtures

Each of the ten chunk-3b/4 packs (`adaptive-work` plus the nine listed
above) gets its own fixture test in `engine.rs`: start it, walk it to
completion by feeding synthetic passing evidence (`walk_to_completion`,
seeding `VerificationReport`s for Test/Verify steps and `ReviewRunEvidence`
for Review steps via the SAME `super::review::required_independent_
reviews_for` gate `advance_with_evidence` itself consults -- this chunk adds
no new evidence-shape bypass), approving artifact-gated steps with
substantive (non-template) content, and asserting the pack's completion
contract's required output was actually produced and its artifact stages
accepted. `every_builtin_pack_parses_validates_and_selects_on_its_own_
triggers` and a registry-wide unknown-skill/role cross-check round out chunk
4's coverage alongside the ten fixtures.

## Chunk 5: the engine fix, and the remaining sixteen packs

### 1. The real fix for a genuinely artifact-less approval gate

`WorkflowState` gains `current_step_approved: Option<String>` -- the id of
the step whose GATE-ONLY (no `artifact`) approval has already been granted.
`approve`'s fallback branch (the one a plain `approval = true` step with no
`artifact` takes, which does not itself advance `current_step`) now records
`Some(step.id)` there before returning. A new private
`WorkflowState::step_requires_approval(&self, step)` reads `step.approval &&
self.current_step_approved.as_deref() != Some(step.id.as_str())`, and every
site that previously recomputed `status` from a bare `step.approval` --
`apply_effective_deploy_tier`, both step-advance branches of
`advance_with_evidence` and `approve`, and `reclassify` -- now calls it
instead. Comparing by id rather than clearing the field on every
`current_step` move is deliberate: a step id is unique within one
materialization (`WorkflowDefinitionV2::validate` already enforces this), so
a stale `current_step_approved` value can never falsely match a later,
different step -- it simply stops mattering the moment `current_step` moves
on, without needing an explicit reset anywhere. This is a small, targeted
patch (one new field, one new private method, four call sites), not the
larger "rework how approve/advance are paired" the chunk-4 note flagged as
the alternative -- trying the narrow fix first, per the brief, was the right
call.

`an_approval_gate_without_an_artifact_can_be_approved_and_advanced` proves
the exact previously-failing sequence directly against `pm-requirements`'s
`brief-approval` step: reach the gate, `approve` it (status unblocks to
`Running`, `current_step` does NOT move, `current_step_approved` records the
step's id), then `advance_with_evidence` successfully moves past it. Both
packs that carried the chunk-4 workaround (`pm-requirements`'s
`brief-approval`, `sre-incident-triage`'s `mitigation-gate`) had their
workaround `artifact = "plan"` field removed; their existing end-to-end
fixtures (`pm_requirements_end_to_end_walks_to_completion_with_its_artifact`,
`sre_incident_triage_end_to_end_stays_read_only_until_the_mitigation_gate`)
were updated to assert the Plan artifact stage is now genuinely absent
(`completed.artifacts.get(ArtifactStage::Plan.key()).is_none()`) and still
pass unmodified otherwise -- both packs still walk to completion end to end
with the real fix in place, not just the direct unit test.

### 2. The remaining sixteen packs

Sixteen more built-in packs (`builtin_sources()` grows from 16 to 32
entries), one group at a time:

- **Project management** (domain `pm`): `pm-backlog-triage` (intake -->
  dedupe/clarify --> priority/risk/dependency --> backlog update),
  `pm-cycle-planning` (capacity/evidence --> candidate scope -->
  dependencies/risks --> a gate-only `committed-plan` approval, proving the
  chunk-5 fix in a second, independently-authored pack), `pm-risk-review`
  (identify --> assess likelihood/impact --> mitigation options -->
  risk-scaled independent review --> register), `pm-retrospective` (gather
  signals --> findings --> action items --> summary). `pm-backlog-triage`
  and `pm-retrospective` both stop their intake step at an explicit
  missing-Linear-integration gate, matching `pm-requirements`/
  `pm-status-report`'s existing pattern.
- **Data** (domain `data`): `data-anomaly-investigation` (scope -->
  reproduce/gather evidence --> root cause (`systematic-debugging`) -->
  independent validation --> findings), `data-recurring-kpi-review` (a
  routine, recurring check: collect --> compare to baseline --> flag
  deviations only when risk warrants it --> summary; collect-metrics stops
  at a missing-BI-integration gate).
- **Architecture** (domain `architecture`): `architecture-discovery`
  (read-only inventory --> constraints/boundaries --> discovery report, no
  independent-review gate -- a discovery pass is itself lower-consequence
  than the decision it feeds), `architecture-migration-roadmap` (current
  state --> materially different phasing options, both approval-gated -->
  phased plan --> independent review --> the roadmap; same "an architecture
  workflow does not mutate production merely because its recommendation
  names commands" posture as the ADR pack), `architecture-threat-scale-
  cost-review` (intake --> two INDEPENDENT assessments sharing
  `parallel_group = "assessment"`, `threat-assessment` on the
  `security-scanner` seat and `scale-cost-assessment` on `architect` -->
  independent review --> disposition -- the first built-in pack to actually
  exercise `parallel_group` on two sibling steps, proven by asserting Kahn's
  algorithm's declaration-order tie-break puts them in authoring order).
- **DevOps and SRE**: `devops-infrastructure-change` (domain `devops`) and
  `sre-deploy-or-rollback` (domain `sre`) are the two packs the brief singled
  out for `effects = "external"` -- both start read-only (scope/plan/
  precondition or health-assessment steps), put every mutating step behind
  an explicit approval, and stop at a gate whose `reason` names the missing
  #539 cloud/deployment-ops tooling rather than acting through raw/untyped
  access; every step's OWN `effect` field stays `effect = "none"` (the
  default) today, since neither pack actually performs a mutation yet --
  `external_effects_packs_stay_read_only_until_their_gate` proves this for
  both together, mirroring chunk 4's
  `sre_incident_triage_end_to_end_stays_read_only_until_the_mitigation_gate`.
  `sre-postmortem` (domain `sre`, `effects = "none"`) never mutates, per the
  brief's explicit "an incident or postmortem pack never mutates": timeline
  --> root cause --> contributing factors --> corrective actions -->
  independent review --> the postmortem.
  `sre-capacity-reliability-review` is the read-only, recurring counterpart
  to `data-recurring-kpi-review` -- collect --> assess risk -->
  recommendations only when risk warrants it --> summary; collect-metrics
  stops at a missing-observability-integration gate.
- **Software engineering** (domain `software`, `effects = "repository"`):
  `schema-data-migration` extends `dependency-upgrade`/`security-remediation`'s
  established shape (intent --> plan --> implement --> test --> review -->
  verify --> deploy) but, like `security-remediation`, never proportionally
  skips ANY of its intent/plan/deploy approval gates or its independent
  review -- irreversible data loss is not proportional to complexity alone.
  `performance-investigation` is proportional instead (`dependency-upgrade`'s
  shape: a `complexity-or-risk` gate on the initial profile/reproduce step, a
  risk-scaled review), inserting a `systematic-debugging`-skilled
  `root-cause` step (agent role `debugger`) between reproduction and the fix
  itself. `documentation-runbook-change` is the lightest of the three: a
  small wording fix skips its own intent gate entirely, but the independent
  review is risk-scaled rather than complexity-scaled -- "a runbook error
  can mislead an on-call responder during a real incident" is a risk
  property, not a size property.

Every step across all sixteen packs references only a skill id that already
exists and an agent role from the #541 roster; none invents a new skill or
role. Consistent with chunk 4's own precedent (the nine chunk-4 professional
packs never set a step's `capabilities` field either, unlike the five
converted legacy packs), none of the sixteen sets `capabilities`.

### 3. Reconciling with #541

`native/541`'s `agents.rs` (inspected directly in the `zirv-541` worktree at
chunk-5 time) registers exactly twelve built-in manifests: `implementer`,
`reviewer`, `doc-keeper`, `security-scanner`, `explorer`, `researcher`,
`planner`, `architect`, `debugger`, `tester`, `data-analyst`, `devops-sre` --
each manifest's `id` is the literal string a `StepV2.agent_role` names (role
resolution is by manifest id, not by the separate `TeamRole` enum #541 also
introduces for write-authority classification). #541 still had not merged
into this worktree as of chunk 5 (`agents.rs` here carries no `team_role`
field), so `registry.rs`'s `KNOWN_ROLES` test allowlist -- introduced in
chunk 4 as a stand-in for a live `AgentRegistry` lookup -- is extended from
nine to the full twelve rather than replaced: every role chunk 4 and chunk 5
together actually use (`architect`, `data-analyst`, `debugger`,
`devops-sre`, `doc-keeper`, `implementer`, `planner`, `researcher`,
`reviewer`, `security-scanner`, `tester`) is a real #541 manifest id, so this
remains a rebase step, not a functional gap: once #541 merges into a shared
base, `no_builtin_pack_references_an_unknown_skill_or_role` should resolve
roles through the live `AgentRegistry` (as `registry.rs`'s own validate
already does for skill ids against a live `SkillRegistry`) instead of the
fixed allowlist. `explorer` and `tester` are the two #541 roles neither
chunk 4 nor chunk 5 happens to assign to any pack step (every `test`/
`testing`-skilled step across the whole catalogue leaves `agent_role` unset,
matching chunk 4's own precedent); both remain available for a future pack.

### 4. Selection: extending, not just re-running, the chunk-4 tests

`every_builtin_pack_parses_validates_and_selects_on_its_own_triggers`'s
threshold grows from 10 to 26 (every non-legacy, non-`adaptive-work` pack
across chunks 4 and 5) -- all twenty-six select correctly on their own first
declared trigger with no code changes to `selection::select_definition`
itself, confirming the sixteen new packs' `triggers`/`domains` vocabularies
do not collide with the existing ten (or each other) badly enough to steal
another pack's selection. A NEW test,
`no_two_packs_claim_the_same_trigger_ambiguously`, guards the input side of
that claim directly: no two built-in packs may literally share a trigger
phrase (case-insensitive) at all. This is a stronger, complementary
guarantee to the per-pack selection proof above -- an accidental substring
collision that changed a WINNER would already fail the per-pack test, but a
literal duplicate trigger between two packs that happened not to change any
single test's outcome (e.g. because the other pack was never itself probed
with that exact phrase) would not be caught there. Two packs are still free
to score a genuine TIE on some other, non-identical phrasing --
`a_tie_is_broken_toward_fewer_external_effects_and_recorded` (chunk 3b,
unchanged) already proves that case is resolved by the documented
effects-then-alphabetical rule, not by accident; this test only forecloses
the more insidious case of a literal, silent duplicate.

### 5. Docs

The README's `### Built-in packs` table (renamed `chunks 4-5`) grows to all
thirty-two ids with group/effects/output; its explanatory paragraph is
corrected for the two new `effects = "external"` packs (previously "none
reaches external" was true and is no longer). No new CLI arg, config key, or
model-calling call site was added in this chunk, so
`docs/design/native-runtime-inventory.md` needed no new rows --
`ZCHK-RUNTIME-INVENTORY` and `scripts/check-readme-features.sh` both still
pass unchanged.

### What remains deferred (unchanged from chunk 4)

Same three items chunk 4 already named: true concurrent execution of
`parallel_group` steps (still informational-only metadata --
`architecture-threat-scale-cost-review` exercises the tag but still executes
sequentially), typed Linear/Kibana/cloud-ops tools from #539 (the reason
eight packs across chunks 4-5 now stop at an explicit missing-integration
gate -- `pm-status-report` and `sre-incident-triage` from chunk 4, plus
`pm-backlog-triage`, `pm-retrospective`, `data-recurring-kpi-review`,
`sre-capacity-reliability-review`, `devops-infrastructure-change` and
`sre-deploy-or-rollback` from chunk 5), and a bounded model tie-break for
selection (still unnecessary -- no chunk-5 pack landed near the selection
floor in a way the deterministic algorithm got wrong).
