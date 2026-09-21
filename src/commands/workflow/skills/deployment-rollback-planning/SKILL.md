---
name: deployment-rollback-planning
description: Plan a release together with its undo before deploying -- name irreversible steps, choose the health signal and abort threshold in advance. Use when preparing to ship a change to a running service. Not a multi-release transition, which is migration-planning, and not the branch handoff, which is finish-branch.
metadata:
  x-zirv-schema-version: "1"
  x-zirv-id: deployment-rollback-planning
  x-zirv-version: "1"
  x-zirv-name: Deployment rollback planning
  x-zirv-triggers: release plan,rollback plan,deploy this,ship to production,go live,rollout plan
  x-zirv-phases: deploy
  x-zirv-required-capabilities: repo.read
  x-zirv-context-budget-bytes: "1900"
---

A rollback designed after a bad release is designed under pressure, by people
who have already lost the context that would make it safe. The only reliable
rollback is the one written down before the deploy that needs it.

## Method

1. List every step in the release that cannot be undone by redeploying the
   previous version -- a schema migration, a data backfill, a message format
   a consumer has already read, a one-way feature flag, a third-party webhook
   already fired. Ordinary code rollback does not undo these.
2. For each irreversible step, decide how it degrades gracefully or is made
   reversible (a compatibility shim, a dual-write window, a flag instead of a
   hard cutover) before it ships, not after it causes an incident.
3. Choose the health signal and the abort threshold before starting the
   rollout -- the specific metric, its acceptable range, and how long to wait
   for it to move. A threshold picked mid-rollout is usually rationalized to
   match whatever is already happening.
4. Sequence the rollout to limit exposure -- canary, percentage ramp, region
   order -- and state the action at each stage: continue, hold, or abort, and
   who decides.
5. Write the rollback procedure as executable steps, not intent: exact
   commands, the previous version identifier, and what to check afterward to
   confirm the rollback actually restored the prior state rather than just
   reverting code.

## Contract

Report the release steps, which are irreversible and how each is handled, the
health signal and abort threshold, the rollout sequence with decision points,
and the rollback procedure as concrete steps. Say plainly when an irreversible
step has no safe mitigation rather than shipping it silently. A transition
spanning several releases belongs to `migration-planning`; the branch-level
handoff belongs to `finish-branch`.
