# Specification

## #717 setup progress

`workspace::materialize` receives the state and effective checkout identity.
It reads `<state>/worktrees/<repo-slug>/<checkout>-setup.jsonl`, tolerates
invalid JSONL rows, and treats only matching `{ step_index, step_digest }`
success rows as complete. Each zero-exit setup command appends a record after
it exits; a non-zero exit or timeout returns immediately. The digest is SHA-256
of the command text. The identity includes a canonical root-derived component,
so unrelated roots cannot reuse state, and is stable over the same worktree's
supervisor restart.

## #719 goal bootstrap

Add `AgentArgs.goal: Option<String>`, accepted only by the harness path.
After workspace materialization and before the normal worker dispatch, run a
synchronous bootstrap with `exec::run_with_report` in the effective root. Its
fixed prompt permits environment preparation only and forbids business-logic
edits and `zirv ctx agent`. It receives operator goal text, constrained
permissions/envelope/cancellation, timeout default 600 seconds, and at most
one restart. Resolve `ModelTier::Fast` via a crate-visible tier resolver; if
unmapped, do not add a model flag. A bootstrap must produce a valid explicit
Done completion; process exit 0 alone does not pass. Bootstrap segments write
their own delegation accounting row before the main worker may launch.

## #716 audit

Audit each stated criterion against #729 and add a regression only for a
concrete deficit. Complete the manifest-skill criterion through the existing
`--manifest` delegation file: add its optional `agent: <AgentManifest id>`
reference, resolve it through `AgentRegistry` before allocation, and attach
its `skills` with the same shared `SkillRegistry` renderer as workspace skills.
When `--workspace` is selected its explicit skill list replaces those manifest
defaults. This adds no new manifest runtime, model routing, role mapping, or
authority grant; the ID supplies only labelled skill instructions already
validated by the normal registries.
