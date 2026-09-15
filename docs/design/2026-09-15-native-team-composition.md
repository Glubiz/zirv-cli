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

- Chunk C: native `/agents`, `/agent`, `/team` slash commands and
  coordinator enforcement (the issue's implementation item 4) -- this chunk
  only ships the registry/compiler they will call.
- #537's full execution profile (intent/domain/complexity/risk/validation
  tuned against real outcome evidence) replaces `profile.rs` wholesale.
- #539's portable skill bundles; this chunk uses the existing
  `SkillRegistry`/`SkillRef` id+version composition only.
- Benchmark evidence (issue "Evaluation" section: completion, review
  findings, duplicate work, latency, cost, route failures, unnecessary
  spawns) -- no route/spend telemetry integration in this chunk; the task
  battery pins EXPECTED composition, not measured outcomes.
- Real per-path claim splitting and a bounded model tie-break, both noted
  above.
- Coordinator-side enforcement that a retry/restart cannot create a second
  writer for the same claim (issue implementation item 7) -- this chunk's
  `Claim` is plan DATA describing intended ownership; wiring it into the
  existing `task::claim_locked`/`permit::acquire_writer`/`group::admit_
  child`/`reservation::reserve_within` machinery is chunk C's coordinator
  work.
