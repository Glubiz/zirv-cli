# Worktree-aware workflow state and gating

**Date:** 2026-09-12 · **Issue:** #467

## Context

The orchestrator convention puts worker changes in
`<repo>/.claude/worktrees/<name>`, a `git worktree add`-linked sibling of the
main checkout that shares its `.git` common dir. `zirv workflow start` runs in
the main checkout; a worker implements and runs `zirv test changed` in the
worktree. Two independent defects then blocked the gate:

1. `workflow::engine`'s per-repository state key (`repo_dir`/`state_path`/
   `active_path`) hashed the literal checkout path. A main checkout and its
   linked worktree are two different paths, so they keyed two unrelated state
   directories: `zirv workflow status|advance|review package <id> --repo
   <worktree>` said `unknown workflow '<id>'`.
2. Even once a lookup succeeds, the `Test`/`Verify` evidence gate
   (`verification::latest_is_fresh_and_passing`) read only the literal
   checkout's own report directory, never a sibling's -- `zirv test changed`
   run in the worktree recorded evidence the gate, evaluated from the main
   checkout, never looked for.

`zirv workflow classify`/`start`'s own diff (`classify::git_change_input`,
`git diff --numstat <base>` against whichever `repo` it is given) was already
correct for the checkout it is given: it already measures the literal repo
argument's branch content against its merge-base, not "the checkout's dirty
tree only". The issue's "empty diff in the main checkout" observation was
that diff correctly reporting the main checkout had no relation to the
feature branch, not a bug in how the diff itself is computed -- round 3 adds
`--branch` for the case where the given checkout is not even ON the branch
being classified at all (see below).

## Decision history

**Round 1 (rejected): a universal identity.** Made `state::repo_slug` itself
-- the one function backing EVERY piece of per-repository state (workflow
state, verification reports, telemetry, test baselines, crash witnesses,
handoffs, mail, ...) -- redirect a linked worktree to its main checkout's
identity before hashing. Review caught that verification reports are
one-per-repository, so merging every worktree's reports into one directory
let two workers running `zirv test changed` concurrently in sibling
worktrees clobber each other's evidence; and that crash-witness/handoff
lookup must stay keyed by the literal, process-scoped checkout, never merged.

**Round 2 (rejected): identity for exactly two call sites.** Reverted
`repo_slug`, added a narrowly-scoped `state::workflow_identity_slug`
(`repo_slug(pathutil::worktree_identity(path))`) used ONLY by workflow-state
lookup (`engine::{repo_dir, state_path, active_path}`) and the Test/Verify
gate's widened read. Review caught two further, more subtle defects:

- Workflow state is NOT actually one-per-repository the way a verification
  report bucket is: `workflow_identity_slug` on `repo_dir`/`active_path`
  meant every worktree of a repository shared ONE active-workflow pointer, so
  two DIFFERENT workers each running their own `zirv workflow start` in their
  own worktrees clobbered each other, and `load`'s repo-retarget could
  relabel a completely unrelated sibling's workflow as if it were the
  caller's own.
- The widened gate read accepted ANY sibling's fresh, passing evidence with
  no check that the sibling had anything to do with the workflow being
  gated -- reaching as far as `deploy.rs`'s production-tier gate. A
  completely unrelated worker's evidence on a different branch could satisfy
  a gate that was never about that work at all.

**Round 3 (this one, final): literal-checkout storage everywhere, two
narrow, rule-bounded exceptions for lookup.** `workflow_identity_slug` is
removed entirely -- nothing needs it. `repo_dir`/`state_path`/`active_path`
are back to plain, literal `repo_slug`, unconditionally: workflow state (and
the active pointer) live at the literal checkout that `start`/`save` wrote
them at, full stop.

### Finding a workflow (`engine::load`/`load_active`)

- **By explicit id** (`status|advance|review package <id> --repo <path>`,
  `engine::resolve_state_path_for_id`): the literal checkout's own state
  directory first, then every sibling checkout in turn
  (`pathutil::sibling_checkouts`, `git worktree list --porcelain`) for one
  holding `id`. An explicit id is never ambiguous the way "whichever pointer
  happens to be there" is, so widening to every sibling is safe here.
- **The active pointer, no id** (`engine::load_active`): the literal
  checkout's own pointer first; if it has none, falls back ONLY to the MAIN
  checkout's own pointer (`pathutil::worktree_identity`), never an arbitrary
  other sibling. This is the asymmetry round 2 got wrong: a worker worktree
  with no workflow of its own should inherit the orchestrator's (there is
  exactly one unambiguous "the main checkout" to fall back to), but two
  workers each with their OWN active workflow must never see or clobber each
  other's -- neither of their worktrees is ever mistaken for "the" fallback.

`load` still retargets the loaded `WorkflowState.repo` to the literal `repo`
argument the caller passed, regardless of which checkout `id`'s state file
was actually found in -- this is what makes `advance`'s Review gate,
`review package`'s diff/fingerprint, and frontend detection (all of which
read `state.repo` unchanged) measure wherever the caller actually is.

### The Test/Verify gate's relatedness check (Finding 2)

Round 2's widened read (`verification::latest_is_fresh_and_passing`) is kept
-- report storage stays keyed by the literal checkout via plain `repo_slug`,
so sibling worktrees still never clobber each other's evidence -- but the
widened half now requires proof of relatedness, not merely "fresh and
passing for someone's tree":

- **`WorkflowState.branch: String`** (new): the branch this workflow gates.
  Set at `start` from `--branch <name>` (new flag) when given, else the
  checkout's own current branch (`verification::current_branch`, empty when
  unresolvable -- detached HEAD, no commits, `git` unavailable).
  `#[serde(default)]` for workflows persisted before this field existed, so
  they default to an empty branch and simply never widen (see below), never
  a schema break.
- **`VerificationReport.branch: String`** (new): the branch a `zirv test
  changed`/`zirv verify` run was produced on, recorded the same way at
  `run_mode`. `#[serde(default)]` likewise for pre-existing reports.
- **`latest_is_fresh_and_passing(state, repo, final_only, branch: Option<&str>)`**
  (signature change, all ~10 call sites updated): the literal checkout's own
  evidence is trusted unconditionally regardless of `branch`, exactly as
  before -- only the widened, cross-checkout half requires `branch` to be
  `Some` (a caller with an actual workflow in view) AND the sibling's own
  recorded `VerificationReport::branch` to equal it exactly. An empty
  `branch` (no workflow context, e.g. `hook.rs`'s advisory nudge; or a
  workflow whose own branch could not be resolved) never widens at all --
  there is nothing safe to match against. Every production call site now
  passes `Some(&state.branch)` where a `WorkflowState` is in view
  (`engine.rs`'s gate and `--run-checks` path, `deploy.rs`) and `None` where
  it is not (`hook.rs`, `status.rs`, `objective.rs` -- advisory/presentation
  contexts with no specific workflow to key relatedness on).

### Classification against a branch the checkout isn't even on (`--branch`)

`classify::git_change_input_for_branch`/`review::default_base_for` (new):
like the existing, unchanged `git_change_input`/`default_base`, but resolve
the base and diff relative to a NAMED branch ref
(`git diff --numstat <base> <branch>`) rather than the checkout's working
tree -- because `--branch`'s whole purpose is letting an orchestrator's main
checkout (sitting on `main`) classify a worker's feature branch it does not
have checked out at all. No untracked-file scan in this path (there is no
working tree standing in for the named branch's content to sample), and a
currently-checked-out branch given via `--branch` will not see its own
uncommitted edits reflected -- both accepted: plain `git_change_input`
already covers "the checkout's own current branch, uncommitted edits
included", which is unaffected. `ClassifyArgs`/`StartArgs` both gained
`--branch`; `from_args` threads it to `git_change_input_for_branch` when
given.

## What is verified

- `pathutil::worktree_identity`/`sibling_checkouts`: worktree-to-main-
  checkout identity resolution and the sibling-checkout listing, each in
  isolation, with real `git worktree add` fixtures.
- Acceptance 1: `advance_accepts_test_changed_evidence_recorded_in_a_linked_worktree`
  (`engine.rs`) -- evidence recorded in a worktree, loaded through
  `--repo <worktree>`, satisfies the `Test` gate (a direct, literal-checkout
  hit -- no widening needed since evidence and gate check reach through the
  SAME path).
- Acceptance 2: `workflow_started_in_the_main_checkout_is_found_from_a_linked_worktree`
  (`engine.rs`) -- `load`/`load_active` resolve a workflow started in the
  main checkout when given the linked worktree's path.
- Acceptance 3: `git_change_input_sees_a_linked_worktrees_branch_diff_against_its_base`
  (`classify.rs`) -- unchanged from round 1: the main checkout's own diff is
  empty, the worktree's diff against the shared base sees its real changes.
- Finding 1 regression: `sibling_worktrees_each_resolve_their_own_active_workflow`
  (`engine.rs`) -- two workers each start their own workflow in their own
  worktree; each `status` resolves its own, never the other's; a third
  sibling with no workflow of its own resolves the MAIN checkout's.
- Finding 2 regression, positive:
  `latest_is_fresh_and_passing_widens_to_a_sibling_worktrees_evidence_on_the_same_branch`
  -- a sibling's fresh, passing evidence on the workflow's own branch
  satisfies a gate with none of its own.
- Finding 2 regression, negative:
  `latest_is_fresh_and_passing_does_not_widen_to_an_unrelated_branch` -- the
  same sibling's evidence, recorded on a DIFFERENT branch than the one the
  gate is asked about, never satisfies it; neither does any sibling at all
  when the caller has no branch to check against.
- `sibling_worktrees_record_and_gate_pass_independently` (round 2, kept): two
  sibling worktrees each record their own passing evidence; neither clobbers
  the other's `report_dir`/`latest` pointer.
- `a_sibling_worktrees_dead_in_flight_record_is_not_reported` (`sessions.rs`,
  round 2, kept): a crash witness never leaks across a real worktree pair --
  `repo_slug` being unconditionally literal now makes this the ONLY behavior,
  not one of two paths.
- The full `workflow::`/`verification::`/`review::`/`testrun::`/`pathutil::`/
  `sessions::`/`handoff::` suite passes (671 tests), confirming the ordinary
  single-checkout path is unchanged throughout all three rounds.

## What is deferred

- A main checkout whose `.git` was relocated with `git init
  --separate-git-dir=...` breaks the "common dir's parent is the main
  checkout" assumption used by `pathutil::worktree_identity` (and therefore
  `load_active`'s fallback). Not attempted -- no evidence this convention is
  in use anywhere in this repository's own tooling.
- The gate widening in `latest_is_fresh_and_passing` also reaches
  `deploy.rs`'s production-tier gate and a few advisory reads (`hook.rs`,
  `status.rs`, `objective.rs`) that share the same function; the advisory
  callers pass `None` (never widen) since they have no specific workflow
  branch to key relatedness on, and `deploy.rs` passes the workflow's own
  branch like every other workflow-aware caller -- intentional, not
  independently re-verified per call site beyond the shared function's own
  tests.
- `--branch`'s classification path (`git_change_input_for_branch`) does not
  see a currently-checked-out branch's own uncommitted edits when that
  branch happens to be given explicitly via `--branch`; documented as
  accepted in the README, since `--branch`'s purpose is inspecting a branch
  the given checkout is NOT sitting on.
