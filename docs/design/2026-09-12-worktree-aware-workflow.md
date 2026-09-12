# Worktree-aware workflow state and gating

**Date:** 2026-09-12 · **Issue:** #467

## Context

The orchestrator convention puts worker changes in
`<repo>/.claude/worktrees/<name>`, a `git worktree add`-linked sibling of the
main checkout that shares its `.git` common dir. `zirv workflow start` runs in
the main checkout; a worker implements and runs `zirv test changed` in the
worktree. Two independent defects then blocked the gate:

1. `commands::ctx::state::repo_slug` (every per-repository state key --
   workflow state, verification reports, telemetry, test baselines) hashed
   the literal checkout path. A main checkout and its linked worktree are two
   different paths, so they keyed two unrelated state directories:
   `zirv workflow status|advance|review package <id> --repo <worktree>` said
   `unknown workflow '<id>'`, and `zirv test changed` run in the worktree
   recorded evidence nothing ever read back.
2. Even once a lookup succeeds, every evidence/change-set check downstream
   (`advance`'s `Test`/`Verify` gate, `review package`'s diff and
   fingerprint, frontend detection) read `state.repo` -- the path persisted
   at `zirv workflow start`, immutable thereafter -- rather than whichever
   path the current command was actually invoked with. `zirv workflow advance
   <id> --outcome success` run against a workflow started in the main
   checkout always fingerprinted the (clean) main checkout, never the
   worktree the real work happened in, regardless of `--repo`.

`zirv workflow classify`/`start`'s own diff (`classify::git_change_input`,
`git diff --numstat <base>` against whichever `repo` it is given) was already
correct: it already measures the literal repo argument's branch content
against its merge-base, not a "the checkout's dirty tree only" comparison, so
pointing `--repo` at a worktree already produced the real diff before this
change. The issue's "empty diff in the main checkout" observation was that
diff correctly reporting the main checkout had no relation to the feature
branch, not a bug in how the diff itself is computed.

## Decision

Chose the simpler of the issue's two proposed designs: resolve `--repo
<worktree>` to the shared identity, and compute every change set from the
literal path given, rather than adding a `zirv workflow start --worktree
<path>` flag. No new flag was needed -- `--repo` (or cwd) already reaches
every verb this issue names.

**Identity (`pathutil::worktree_identity`, new in
`src/commands/ctx/pathutil.rs`):** for a linked worktree (its own `git
--git-dir` differs from `--git-common-dir`), returns the common dir's own
parent -- the main checkout's working-tree root, since every worktree's
common dir *is* that checkout's `.git` directory. For everything else (a main
checkout, a bare repo, or anywhere `git` does not resolve) it returns the
canonicalized path unchanged, which is exactly `repo_slug`'s pre-#467
behavior. `repo_slug` now redirects through this before hashing, so a main
checkout and any of its linked worktrees hash to one identity; every other
repository's slug is unchanged. Memoized per canonical path (a `Mutex<HashMap>`,
the same pattern `repo_slug`'s own legacy-migration cache already uses)
because `repo_slug` is called from hot paths -- a hook fires on every tool
call -- and resolving this shells out to `git`.

Reused `commands::ctx::adapters::{git_dirs, git_common_dir}` rather than
adding a second common-dir prober: it already strips
`GIT_DIR`/`GIT_COMMON_DIR`/`GIT_WORK_TREE`/`GIT_INDEX_FILE` from the
environment before shelling out (issue #119) and already resolves a linked
worktree's common dir to the main checkout's `.git`, documented at its own
definition.

**Change-set measurement (`workflow::engine::load`):** after a successful
lookup, retargets the loaded `WorkflowState.repo` to the literal `repo`
argument the caller passed. Reaching that point already proves `repo` and the
persisted `value.repo` are the same repository (the lookup above resolved
both through the same identity) -- so this is always safe, and a no-op in the
ordinary single-checkout case, where they were already equal. Every verb this
issue names (`status`, `resume`, `context`, `artifacts`, `approve`,
`advance`, `close`, `reclassify`, and `review`'s own `state_and_repo`) already
funnels through this one function (see its own doc comment), so no other call
site needed to change: `advance_with_evidence`, `review::package`, frontend
detection, and every one of their ~40 existing tests keep reading
`state.repo` exactly as before, and now correctly see whichever checkout the
operator pointed `--repo` at for *this* call, not wherever the workflow was
started. `zirv workflow advance <id> --outcome success --repo
<repo>/.claude/worktrees/<name>` now measures the worktree's own change set,
matching the evidence `zirv test changed` recorded there.

## What is verified

- `pathutil::worktree_identity`: a linked worktree resolves to its main
  checkout's identity; a plain checkout resolves to itself; a non-git path
  falls back to itself unchanged (three tests in `pathutil.rs`).
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
- The full pre-#467 `workflow::`/`verification::`/`review::`/`testrun::`
  suite (500 tests via `cargo nextest run workflow:: testrun:: worktree::
  pathutil::`) still passes unchanged, confirming the ordinary
  single-checkout path is untouched.

## What is deferred

- A main checkout whose `.git` was relocated with `git init
  --separate-git-dir=...` breaks the "common dir's parent is the main
  checkout" assumption; its worktree siblings will not resolve to it. Not
  attempted -- no evidence this convention is in use anywhere in this
  repository's own tooling.
- `advance --run-checks` already ran checks against its own CLI-resolved
  `repo` before this change (it never read `state.repo` for that); this fix
  is what makes its *second*, internal freshness re-check (inside
  `advance_with_evidence`) agree with it once a worktree is involved. Not
  re-verified as a fourth acceptance test since the existing
  `advance_run_checks_*` suite already covers that path end-to-end for the
  single-checkout case, unchanged by this diff.
