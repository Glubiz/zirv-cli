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
correct: it already measures the literal repo argument's branch content
against its merge-base, not "the checkout's dirty tree only", so pointing
`--repo` at a worktree already produced the real diff before this change. The
issue's "empty diff in the main checkout" observation was that diff correctly
reporting the main checkout had no relation to the feature branch, not a bug
in how the diff itself is computed.

## Decision

Chose the simpler of the issue's two proposed designs: resolve `--repo
<worktree>` to a shared identity for the two places that need it, rather than
adding a `zirv workflow start --worktree <path>` flag. No new flag was needed.

**Round 1 (rejected on review): a universal identity.** The first pass made
`state::repo_slug` itself -- the one function backing EVERY piece of
per-repository state (workflow state, verification reports, telemetry, test
baselines, crash witnesses, handoffs, mail, ...) -- redirect a linked
worktree to its main checkout's identity before hashing. Review caught two
real defects this introduced:

- Verification reports are one-per-repository (`report_dir` = one directory,
  one `latest` pointer). Merging every worktree's reports into that one main-
  checkout-keyed directory meant two workers running `zirv test changed`
  concurrently in sibling worktrees would clobber each other's evidence, and
  a worktree's own fresh, passing evidence could be evicted by an older
  report from a sibling.
- Crash-witness (`sessions::interrupted_record`/`take_interrupted_in_flight`)
  and handoff (`handoff::store`/`latest_for_repo`) lookup must stay keyed by
  the literal checkout a session or process is actually running in. A crash
  witness or handoff is process/session-scoped, not repository-scoped:
  merging them across worktrees would let a session starting in one checkout
  consume a crash witness (or pick up a handoff) left by a completely
  different session that died in a sibling checkout.

**Round 2 (this one): identity for exactly two call sites, nowhere else.**
`state::repo_slug` is reverted to its pre-#467, literal-path-only behavior --
every one of its consumers except the two below is completely unaffected by
this issue.

- **`pathutil::worktree_identity`** (new, `src/commands/ctx/pathutil.rs`):
  for a linked worktree (its own `git --git-dir` differs from
  `--git-common-dir`), returns the common dir's own parent -- the main
  checkout's working-tree root, since every worktree's common dir *is* that
  checkout's `.git` directory. For everything else (a main checkout, a bare
  repo, or anywhere `git` does not resolve) it returns the canonicalized path
  unchanged. Memoized per canonical path (a `Mutex<HashMap>`, no
  invalidation) because its one caller below is invoked from a hook that
  fires once per agent turn and resolving this shells out to `git`; safe
  only because that caller's question ("is this the same repository")
  cannot change out from under a single process, and it is not a long-lived
  daemon that would accumulate stale entries.
- **`state::workflow_identity_slug`** (new): `repo_slug(worktree_identity(path))`.
  The ONLY two consumers, both explicitly reserved in its doc comment so a
  future change does not repeat round 1's mistake:
  - `workflow::engine::{repo_dir, state_path, active_path}` (and therefore
    `load`/`load_active`) -- workflow state itself is genuinely
    one-per-repository (a workflow tracks a piece of work, not a checkout),
    so this is exactly where a shared identity belongs. `load` additionally
    retargets the loaded `WorkflowState.repo` to the literal `repo` argument
    the caller passed (safe: reaching that point already proves `repo` and
    the persisted `value.repo` are the same repository) -- this is what
    makes `review package`'s diff/fingerprint and frontend detection, both
    of which read `state.repo` unchanged, measure wherever the caller
    actually is.
  - `verification::latest_is_fresh_and_passing`'s widened READ path (see
    below) -- report *storage* (`report_dir`/`save_report`) stays keyed by
    plain, literal `repo_slug`, exactly as before #467.

**The Test/Verify gate's widened read (`verification::
latest_is_fresh_and_passing`):** split into a private
`latest_is_fresh_and_passing_at` (the original single-checkout check,
unchanged) and a public wrapper that, only when the literal checkout has no
fresh passing evidence of its own, loops over
`pathutil::sibling_checkouts(repo)` (new: `git worktree list --porcelain`,
same environment isolation as `adapters::git_dirs`) and re-runs the exact
same check against each sibling -- that sibling's own fresh fingerprint
against that sibling's own latest report, never `repo`'s fingerprint compared
against a report recorded somewhere else. Report storage itself is
untouched: two sibling worktrees still write to two different directories,
so neither can clobber the other.

## What is verified

- `pathutil::worktree_identity`: a linked worktree resolves to its main
  checkout's identity; a plain checkout resolves to itself; a non-git path
  falls back to itself unchanged.
- `pathutil::sibling_checkouts`: lists both the main checkout and its linked
  worktree, from either side; empty for a non-git path.
- Acceptance 1: `advance_accepts_test_changed_evidence_recorded_in_a_linked_worktree`
  (`engine.rs`) -- a real `git worktree add` sibling with its own committed
  feature-branch change; evidence recorded against the worktree's fingerprint
  satisfies the `Test` gate once loaded through `--repo <worktree>`.
- Acceptance 2: `workflow_started_in_the_main_checkout_is_found_from_a_linked_worktree`
  (`engine.rs`) -- both `load` (by id) and `load_active` (the bare-status
  pointer) resolve a workflow started in the main checkout when given the
  linked worktree's path.
- Acceptance 3: `git_change_input_sees_a_linked_worktrees_branch_diff_against_its_base`
  (`classify.rs`) -- the main checkout's own diff (never touched past "base")
  is empty; the worktree's diff against the shared base sees its real,
  committed changes, confirming classification was already, and remains,
  worktree-correct once pointed at one.
- Round-2 regression coverage: `sibling_worktrees_record_and_gate_pass_independently`
  and `latest_is_fresh_and_passing_widens_to_a_sibling_worktrees_evidence`
  (`verification.rs`) prove neither sibling's evidence clobbers the other's
  AND that the gate still finds a sibling's evidence when evaluated from a
  checkout with none of its own; `a_sibling_worktrees_dead_in_flight_record_is_not_reported`
  (`sessions.rs`) proves a crash witness never leaks across a real worktree
  pair.
- The full pre-#467 `workflow::`/`verification::`/`review::`/`testrun::`/
  `pathutil::`/`sessions::` suite still passes, confirming the ordinary
  single-checkout path, and every `repo_slug` consumer other than the two
  named above, is completely unchanged.

## What is deferred

- A main checkout whose `.git` was relocated with `git init
  --separate-git-dir=...` breaks the "common dir's parent is the main
  checkout" assumption; its worktree siblings will not resolve to it. Not
  attempted -- no evidence this convention is in use anywhere in this
  repository's own tooling.
- `advance --run-checks` already ran checks against its own CLI-resolved
  `repo` before this change; the widened gate read is what makes its
  internal freshness re-check (inside `advance_with_evidence`) agree with it
  once a worktree is involved. Not re-verified as a separate acceptance test
  since the existing `advance_run_checks_*` suite already covers that path
  end-to-end for the single-checkout case, unchanged by this diff.
- The gate widening in `latest_is_fresh_and_passing` also reaches
  `deploy.rs`'s production-tier gate and a few advisory reads (`hook.rs`,
  `status.rs`, `objective.rs`) that share the same function; this is
  intentional (the same "Verify" evidence, the same repository) rather than
  forked into a gate-specific copy, but is worth naming since it was not
  independently re-verified per call site.
