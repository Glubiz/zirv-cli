# Native team composition, chunks A+B (#541)

**Date:** 2026-09-15 · **Issue:** #541 · **Roadmap:** #469, N16 (#485) ·
**Worktree:** `native/541`, based on `release/native-harness`

## Context

#541 asks the native coordinator to assemble the smallest capable team for a
request instead of requiring the operator to name agents, and to keep that
assembly auditable (a persisted `TeamPlan`, never free-form narration). It
explicitly consumes #537's execution profile and #539's skill catalogue,
neither of which is shipped yet. This chunk (A+B) builds the minimal seam
#537 promised, the twelve-manifest roster, the manifest/team-role
unification, and the deterministic team compiler with its wrapped-harness
CLI (`zirv workflow team plan|show|brief`) -- everything that does not
require the native `/agents`/`/agent`/`/team` slash commands or coordinator
enforcement, which chunk C owns.

## Decisions

### 1. The minimal execution profile (`src/commands/workflow/profile.rs`)

`ExecutionProfile::derive(request_text, &Classification) -> ExecutionProfile`
is pure and deterministic: `execution` (`Direct`/`Bounded`/`Orchestrated`)
from complexity alone; `validation` (independent review/test/security) from
risk and domain signals; `domains` (`Security`/`Data`/`Docs`/`DevOps`/
`Architecture`, additive to the existing `Frontend`/`General` split) from
keyword signals in the request text and the diff; `confidence` from
`RiskMeasurement`/`declared_scope`. One module, so #537 can replace it
wholesale without touching any caller. `zirv workflow classify --json` now
embeds the derived `profile` object alongside the existing classification
fields.

### 2. The roster (`src/commands/workflow/agents.rs`)

Seven new built-ins: `researcher`, `planner`, `architect`, `debugger`
(carries the `systematic-debugging` skill reference), `tester`,
`data-analyst`, `devops-sre`. The five existing ids (`implementer`,
`reviewer`, `doc-keeper`, `security-scanner`, `explorer`) are unchanged.

**Deviation from the issue's own roster prose:** the issue describes
`tester` as writable ("writes tests only"). `AgentManifest::validate` (new
this chunk) requires a manifest's `team_role` authority
(`ctx::team::TeamRole::authority().may_write`) to agree with its own
`read_only` flag, and `TeamRole::Tester`'s authority is hard-fixed read-only
(`ctx::team.rs`: "a tester's evidence has to describe a tree it did not
change") -- foundational, tested behavior this chunk does not reopen. The
`tester` manifest is `read_only: true` instead, matching the issue's own
acceptance criterion ("Independent gates use independent read-only seats...")
more than its roster-table paraphrase. Every other roster id's write posture
already agreed with its mapped team role without needing this resolution.

### 3. Manifest <-> team role unification

`AgentManifest` gains `team_role: Option<TeamRole>` and `skills:
Vec<SkillRef>` (both `#[serde(default)]`, no schema bump -- existing
manifest files keep loading). `team_role_for(&AgentManifest) -> TeamRole`:
explicit field wins, else derived from `read_only` (read-only ->
`Researcher`, writable -> `Implementer`). `validate()` rejects a manifest
whose `team_role` authority disagrees with its own `read_only`. `TeamRole`
and `Authority` (`ctx::team.rs`) gained `Serialize`/`Deserialize` (kebab-case
matches `TeamRole::as_str`/`parse` exactly, pinned by a new test) so a
`TeamPlan::Seat` can carry a role and persist it.

### 4. Skill references

`AgentManifest.skills: Vec<SkillRef { id, version }>` composes rather than
duplicates skill instruction text. `AgentRegistry::validate_against(&
SkillRegistry)` refuses an unknown id or a version mismatch.

### 5. The team compiler (`src/commands/workflow/team.rs`)

`compile(objective, &ExecutionProfile, &AgentRegistry, &SkillRegistry,
route_eligibility) -> CtxResult<TeamPlan>` is pure and deterministic. Rules,
in the order applied: `Direct` -> zero seats unconditionally (even if a risk
floor would otherwise require validation -- the ordinary workflow engine's
own review/test gates already cover a mechanically-small but risky change;
the team compiler's job is proportional TEAM size, not re-implementing those
gates). `Bugfix` -> `debugger` then `implementer` (depends on the debugger),
regardless of execution mode. Non-bugfix `Bounded` -> one `implementer`.
Non-bugfix `Orchestrated` -> `planner` at complexity >= Substantial,
`architect` additionally at Architectural, then one `implementer` per claim
group (`changed_files` bucketed by 4, capped at the profile's fan-out).
Domain signals (`Data`/`Docs`/`DevOps`) each add their specialist once, with
a concrete deliverable; `Security` is deliberately NOT a second path here --
it is already covered by `validation.security_review`, which the profile
sets from the same signal. Independent seats (`reviewer`/`security-scanner`/
`tester`) are added last, hold an empty claim (`Claim::none()`: no paths, no
worktree), and `depends_on` every writer seat collected so far -- never a
shared claim, never a transcript dependency. An unknown manifest or a team
role with no eligible route is never invented into the plan: the seat is
omitted (`TeamPlan.omitted`) with a stated reason. Fan-out (`Bounded`: 2,
`Orchestrated`: 6) and dependency-depth limits are computed from the profile
and enforced by refusing the whole plan (`Err`) when exceeded -- not by
silently truncating seats. `compile_explicit` builds a one-seat plan for
`--seat <manifest-id>`, through the identical `resolve_seat` capability/
team-role/route path, so explicit selection never bypasses policy.

**Deferred: real per-path claim splitting.** The issue asks for "one
[implementer] per top-level path group from the classification's changed
paths." `Classification` (from `#537`'s predecessor, `classify.rs`) only
retains `changed_files: usize` (a count), not the actual path list, so this
chunk buckets by count (4 files per claim group) instead. Extending
`Classification` (or `compile`'s inputs) with the real changed-path list to
do a genuine path-boundary split is left to a follow-up -- it changes a type
`#537`/`#539` also touch, and this chunk's own signature is already fixed by
the brief.

**Deferred: bounded model tie-break.** The issue allows "a bounded
structured model decision may break genuine ties." This chunk's `compile`
is 100% deterministic with no model call; a tie-break is unneeded for any
rule implemented so far (every rule resolves without ambiguity) and is left
for when one actually arises.

### 6. Persistence and CLI

`WorkflowState.team_plan: Option<TeamPlan>` (additive, `#[serde(default)]`).
`zirv workflow team plan "<objective>" [--workflow <id>|active] [--dry-run]
[--seat <id>] [--json]`, `team show`, `team brief <seat-id>`. A new
`engine::save_preserving_active` (state-file write only, no active-pointer
mutation) backs `team plan`'s persistence -- `engine::save`'s `active: bool`
parameter can clear a DIFFERENT workflow's active pointer when `false` (see
its own doc comment), which `team plan` must never do as a side effect of
annotating a (possibly explicitly `--workflow`-named, possibly non-active)
workflow with a plan.

### 7. Orchestrator conventions

One bullet added to both `claude::ORCHESTRATOR_PROMPT` and
`codex::ORCHESTRATOR_PROMPT` (and their shared `..._TAIL_AFTER_WRITE_GUARD_
BULLET` copies): run `zirv workflow team plan "<objective>" --json` before
delegating substantial work, spawn only the seats it returns, brief each via
`zirv workflow team brief <seat>`, and honor authority/independence/
omissions; `--seat` remains for a deliberate explicit override. Both prompts
stay under their existing 3,600-byte ship cap.

## Verified

- `cargo build`, the full gate list in the PR/report.
- Every acceptance-criterion bullet not explicitly deferred to chunk C: see
  the PR/report's own CRITERIA section for the line-by-line mapping.
- `tests/fixtures/team/battery.json` (9 prompts spanning mechanical,
  bug-fix, substantial/architectural feature, security, data, and dev-ops)
  driven by `team::tests::the_team_battery_holds`.

## Deferred (explicitly out of scope for this chunk)

- Chunk C (now shipped -- see its own section below): native `/agents`,
  `/agent`, `/team` slash commands and coordinator enforcement (the issue's
  implementation item 4).
- #537's full execution profile (intent/domain/complexity/risk/validation
  tuned against real outcome evidence) replaces `profile.rs` wholesale.
- #539's portable skill bundles; this chunk uses the existing
  `SkillRegistry`/`SkillRef` id+version composition only.
- Benchmark evidence (issue "Evaluation" section: completion, review
  findings, duplicate work, latency, cost, route failures, unnecessary
  spawns) -- no route/spend telemetry integration in this chunk; the task
  battery pins EXPECTED composition, not measured outcomes.
- Real per-path claim splitting (now shipped -- see chunk C below) and a
  bounded model tie-break (still deferred: every rule implemented so far
  resolves without ambiguity).
- Coordinator-side enforcement that a retry/restart cannot create a second
  writer for the same claim (issue implementation item 7) -- now shipped at
  the PLAN layer (chunk C below); literal `task::claim_locked`/`permit::
  acquire_writer` wiring from a compiled seat remains deferred, see chunk
  C's own "Deferred".

## Chunk C (#541): coordinator enforcement, claims, native slash commands

**Worktree:** `native/541`, same branch, chunk A+B head `1228bb5a`.

### Decisions

1. **Native tool `team_plan`** (coordinator/sub-orchestrator seats only;
   `runtime::tools::team::{TEAM_PLAN, TeamPlanArgs}`). Args `{ objective,
   seat?: manifest id, task? }`; `task` overrides the one compiled seat's
   task text and is ignored without `seat`. Runs `workflow::team::
   compile_for_objective` -- the SAME classification/profile/registry/
   compile pipeline `zirv workflow team plan` runs, factored out so the
   native tool and the CLI cannot drift -- then `workflow::team::store_plan`:
   the active workflow owns the plan when one exists for the repository
   (`WorkflowState.team_plan`, chunk B's field); the coordinator record
   (`Coordinator.team_plan: Option<TeamPlanLocation>`, new this chunk) keeps
   only a `Workflow { workflow_id }` reference in that case, or the plan
   itself (`Inline { plan }`) with no active workflow. `coordinator::
   resolve_team_plan` follows the reference through `workflow::engine::load`
   when needed; a dangling reference (a deleted workflow) resolves to "no
   plan" rather than an error -- read failure here is a fact a bounds check
   has no way to surface.

2. **Enforcement in `coordinator::check`.** `Bounds` gained two optional,
   caller-resolved fields -- `manifest: Option<ManifestBounds>` and `plan:
   Option<PlanBounds>` -- so `check` stays pure (no registry read, no plan
   read inside it; `delegation::delegate` gathers both from trusted state
   before calling it, exactly like the pre-existing three checks). `check`
   refuses, in order: an unknown manifest (`Refusal::UnknownManifest`); a
   manifest whose own `team_role` disagrees with the requested role
   (`ManifestRoleMismatch`); a manifest that may write mapped to a role that
   is read-only by identity (`ManifestWiderThanRole`); a delegation that
   matches no unfilled seat once a plan exists (`NotInTeamPlan`), unless the
   COORDINATOR seat passed `override_requested` (recorded on the `Grant` as
   `plan_override`, and from there onto `WorkerHandle.plan_override`); two
   seats whose claim paths overlap (`ClaimConflict`). `LaunchRequest` and
   `WorkerHandle` both gained `manifest: Option<String>`; `WorkerHandle` also
   gained `plan_override: bool`. `delegate()` resolves the requested
   manifest id from `request.manifest` or, absent that, `team::
   default_manifest_for_role` (new: `TeamRole -> &'static str` for the five
   workable roles; `None` for `Coordinator`/`SubOrchestrator`, which have no
   single natural built-in manifest and so skip the check entirely, same as
   any role outside the closed team). Manifest facts are resolved against
   BUILT-IN manifests only (`AgentRegistry::load(repo, None, false, false)`)
   -- a deliberate scope cut, see Deferred. A delegation names its plan seat
   by passing that seat's id as `task` (the SAME field `Coordinator::
   dispatched` already keys its graph node by), so "seat filled" is exactly
   `Coordinator::seat_filled(seat_id)` -- `Delegated` or `Completed` is
   filled; absent, `Planned`, `Failed` or `Cancelled` is free to (re)fill,
   which is what lets a retry after a failure re-fill the identical seat.
   `runtime::tools::delegation::DelegateArgs` gained `manifest: Option
   <String>` and `override_` (`#[serde(rename = "override")]`, since
   `override` is a keyword).

3. **Claims.** Scope cut from the issue's literal "Claim.paths become the
   task card's claim (`task::claim_locked` scope)": `task::claim_locked`
   claims by TASK ID already (one worker per card, pre-existing and
   untouched), not by path -- there is no path-keyed lock to plug a claim's
   `paths` into. What this chunk actually enforces is the PLAN-level
   invariant the issue's acceptance criteria ask for: `PlanBounds` carries
   the matched seat's own claim paths and every OTHER currently-filled
   seat's claim paths (gathered by `delegate()` from the plan + the
   coordinator graph); `check` refuses a match whose claim overlaps any of
   them (`ClaimConflict`), so two overlapping writers can never both be
   dispatched at once. Real per-path splitting (chunk B's own deferred
   item): `Classification` gained `changed_paths: Vec<String>` (bounded to
   `MAX_CHANGED_PATHS = 200`; `changed_files` keeps the true total even when
   truncated), populated by `classify()` from its own `input.paths`. The
   Orchestrated implementer split (`workflow::team::claim_groups_for`)
   buckets by TOP-LEVEL path component when `changed_paths` is non-empty,
   merging the smallest groups down to `max_fan_out`; falls back to the old
   four-files-per-group count bucket only when it is empty (older
   classification, or one measured with none).

4. **Slash commands** in `dash::native_ux.rs`/`dash::native_pane.rs`:
   `/agents` (the roster, through `workflow::agents::write_agent_table` --
   the SAME function `zirv workflow agent list`'s text output calls, not a
   duplicate table); `/team` (the plan stored for this repository, through
   `workflow::team::print_plan_text` -- the SAME function `zirv workflow
   team show` calls) and `/team plan <objective>` (compiles and persists a
   new one, through the SAME `compile_for_objective`/`store_plan` the native
   `team_plan` tool uses); `/agent <manifest-id> <task>` (an explicit
   one-seat DRY-RUN preview via `compile_for_objective(..., seat: Some(id))`
   -- through the identical capability/team-role/route checks
   `compile_explicit` always applies, refusals rendered inline -- never
   persisted, so a preview can never clobber a coordinator's own compiled
   plan). `SLASH_COMMANDS`/`COMPLETION_ROWS` (6 -> 7) updated so a bare `/`
   still shows all seven commands at once; `native_pane.rs`'s own
   `"/agents" absent from help` test updated to the opposite assertion, per
   the brief.

5. **Mixed-runtime.** No new code was needed: `delegate()`'s manifest/plan
   resolution runs identically regardless of `request.runtime`, so a
   wrapped-harness worker seat goes through the exact same `coordinator::
   check` a native one does. Verified with a plan built directly from two
   real `compile_explicit` calls (an implementer and a reviewer seat, the
   reviewer's claim cleared to match the proportional compiler's own
   invariant) rather than depending on a real git diff to drive the
   proportional compiler to a particular shape.

### Verified

- `cargo build`; the full gate list in the PR/report.
- `zirv verify --builtin` (`ZCHK-RUNTIME-INVENTORY`, `ZCHK-NATIVE-PARITY`)
  clean with no new rows needed: `team_plan` is a `runtime::tools` registry
  entry, not a clap verb or a model-calling call site -- the same treatment
  N14's/N15's own native tools already get (see the inventory doc's own
  added paragraph).
- Coordinator enforcement: `coordinator::tests::
  a_delegation_with_an_unknown_manifest_is_refused_before_the_receipt`,
  `a_manifest_whose_team_role_does_not_match_is_refused`,
  `a_delegation_outside_the_team_plan_is_refused_unless_overridden_by_the_
  coordinator`, `a_retry_refills_the_same_seat_and_never_creates_a_second_
  writer`, `overlapping_seat_claims_cannot_both_be_dispatched`.
- Native tool + mixed-runtime: `tools::tests::
  team_plan_tool_stores_the_plan_and_returns_it`, `tools::tests::
  a_mixed_team_still_dispatches_through_the_plan_checks`.
- Slash surface agreement: `native_ux::tests::
  agents_agent_and_team_views_render_the_headless_structs` (byte-for-byte
  against the headless functions, not just similar-looking output),
  `native_ux::tests::every_advertised_native_control_has_a_dispatch_path`,
  `native_pane::tests::a_slash_draft_lists_commands_above_the_box`.
- Real path splitting: `classify::tests::
  classification_keeps_the_changed_paths`, `classify::tests::
  changed_paths_is_bounded_but_changed_files_keeps_the_true_total`,
  `workflow::team::tests::
  a_real_changed_path_list_splits_implementers_by_top_level_boundary`.

### Deferred

- **Literal `task::claim_locked`/worktree wiring from a compiled seat.** A
  coordinator dispatching a plan does not yet AUTOMATICALLY mint one task
  card and (when `claim.worktree`) one worktree per seat -- it still calls
  `task_create`/`delegate` itself, naming the seat id as `task`. The
  PLAN-level claim-overlap check (decision 3 above) is real enforcement, but
  it is not the same as the issue's literal "the task card's claim" wiring.
  A follow-up that has the coordinator (or a new helper) walk a stored
  plan's seats and mint their cards/worktrees automatically would close
  this the rest of the way.
- **Operator/repository manifest identity in the delegation bounds check.**
  `delegate()`'s manifest-facts resolution reads BUILT-IN manifests only, to
  avoid a real, un-isolated home-directory read on every delegation (the
  registry loader's operator-global layer reads `dirs::home_dir()`, which a
  unit test cannot safely point at a fixture without env-var isolation the
  existing delegation test suite does not carry). The native `team_plan`
  tool and the `/agents`/`/agent`/`/team` slash commands DO honor operator/
  repository manifests (same resolution `zirv workflow team plan` uses);
  only the per-delegation bounds check in `coordinator::check`'s caller is
  narrowed. Widening it needs either a registry passed through the seat's
  own resolved config (avoiding a fresh disk read per delegation) or a
  `HomeGuard`-equivalent seam `delegate()`'s own test fixtures adopt.
- A bounded model tie-break for the team compiler (still unneeded: every
  rule resolves without ambiguity).
- #537's full execution profile, #539's portable skill bundles, and
  benchmark/telemetry evidence -- unchanged from chunk A+B's own deferred
  list above.
