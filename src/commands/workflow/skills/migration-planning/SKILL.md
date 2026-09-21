---
name: migration-planning
description: Plan a transition from one system or schema to another across multiple releases -- dual-write, backfill, verification, cutover, rollback -- without a big-bang switch. Use for a transition spanning many deploys. Not for a single release's rollback plan -- that is `deployment-rollback-planning`.
metadata:
  x-zirv-schema-version: "1"
  x-zirv-id: migration-planning
  x-zirv-version: "1"
  x-zirv-name: Migration planning
  x-zirv-triggers: migration,cutover,dual write,backfill,schema migration,data migration
  x-zirv-phases: plan
  x-zirv-required-capabilities: repo.read
  x-zirv-optional-capabilities: repo.write
  x-zirv-context-budget-bytes: "2600"
---

A migration planned as a single cutover treats the moment of switching as the
risk, when the real risk accumulates in everything that ran differently in
the two systems beforehand -- and a plan that skips ordering the stages hides
exactly when rollback stops being an option.

## Method

1. Order the stages explicitly: dual-write, backfill, verification, cutover,
   cleanup. Each stage has a different failure mode, and running one before
   its prerequisite is stable turns a contained problem into a compounded one
   -- backfilling before dual-write is reliable just means the backfill goes
   stale as fast as it completes.
2. Define how equivalence between old and new is actually verified, not
   assumed from the migration logic reading correctly. A comparison job that
   checks a sample, a checksum, or a reconciled count is evidence; "the code
   looks right" is not.
3. Name the point after which rollback stops being possible -- usually when
   something downstream has consumed or acted on data that only the new
   system holds -- and state what shrinks that window. A migration that never
   names this point is assumed reversible right up until it is not.
4. Plan cutover as a flag flip on already-verified, already-dual-written data,
   not as the step that starts the transition. If cutover is the first time
   the new path sees real traffic, the plan has no actual migration in it,
   only a deferred rewrite.
5. Keep the old path alive and correct until the new path has run at full
   volume through at least one full cycle of whatever makes the data
   time-sensitive -- a billing period, a reporting cycle -- because bugs at
   the seams often only appear at the boundary of that cycle.

Failure modes: treating dual-write as done once both paths compile, without
verifying they agree under real load; declaring victory at cutover instead of
after cleanup, leaving a permanent dual-write cost; conflating "we can revert
the deploy" with "we can revert the data," which is usually the harder and
slower half.

Boundary: this covers a transition spanning many releases; a single release's
rollback plan is `deployment-rollback-planning`; the record of why this
approach was chosen over the alternatives belongs in `adr-authoring`.

## Contract

Report the staged plan in order, the verification method and what evidence it
produces, the point past which rollback is no longer possible, and the
cleanup criteria that mark the migration actually finished. Flag any stage
with no defined verification as unverified rather than assumed safe.
