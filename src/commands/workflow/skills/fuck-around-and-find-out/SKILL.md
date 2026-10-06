---
name: fuck-around-and-find-out
description: Experiment toward a shared goal when the path is uncertain. Use when asked to fuck around and find out, FAFO, run a discovery spike, or try competing approaches. Run small, bounded experiments, share evidence across agents, and keep only validated progress.
compatibility: repo.read; repo.write, test.run and shell.exec enable executable experiments when available and authorized.
metadata:
  x-zirv-schema-version: "1"
  x-zirv-id: fuck-around-and-find-out
  x-zirv-version: "1"
  x-zirv-name: Fuck around and find out
  x-zirv-triggers: fuck around and find out,fafo,discovery spike,try competing approaches,experiment toward a shared goal
  x-zirv-phases: design,implement,debug
  x-zirv-required-capabilities: repo.read
  x-zirv-optional-capabilities: repo.write,test.run,shell.exec
  x-zirv-context-budget-bytes: "4800"
---

Turn uncertainty into evidence by trying the smallest useful thing. Prefer
an executable probe over another round of speculation. A failed experiment
is progress when it rules out an approach and the team can reuse the finding.

## Establish the common goal

State the desired outcome, current baseline, observable success criteria,
constraints, and the most important unknown. Keep the goal stable while
changing tactics; do not quietly lower correctness or quality to claim a win.
Use the user's existing scope and authorization. Ask only when a missing
answer materially changes the goal or permits an otherwise blocked action.

Set an experiment budget within the enclosing task's time, cost, and attempt
limits. If none is given, start with at most three small experiments, then
reassess from the evidence; this checkpoint does not authorize endless rounds.
Do not experiment when a known, cheap solution already satisfies the goal.

## Run the loop

1. Read prior findings. Choose one uncertainty whose resolution changes the
   next decision. State a falsifiable hypothesis and what would disprove it.
2. Define the cheapest discriminating experiment: one primary variable,
   inputs, expected observation, baseline comparison, time/attempt limit,
   and a stop condition. Prefer a tiny prototype, fixture, or local probe
   over a broad implementation. Keep acceptance checks independent of the
   candidate; never weaken them to make an experiment pass.
3. Execute within available capabilities and permissions. Use an isolated
   worktree, temporary artifact, or reversible change when mutation is
   needed. Preserve unrelated work. The skill grants no extra access,
   spending, deployment, or destructive-action authority. If execution is
   blocked, report the missing capability and an unrun probe, not a result.
4. Compare observed evidence with the baseline and success criteria. Record
   inputs, commands or method, result, artifact location, and limitations.
   Distinguish measured facts from guesses; classify inconclusive runs as
   inconclusive. Repeat noisy measurements before claiming an improvement.
5. Keep a supported improvement, discard a disproved approach, or refine an
   inconclusive probe. Roll back only this experiment's disposable changes.
   Select the next probe from what was learned, not from sunk effort. Never
   retry an unchanged failure without new evidence or changed conditions.

## Coordinate agents

The orchestrator owns the shared goal, budget, experiment queue, and decision
record. Give each worker the same goal and baseline, one distinct hypothesis,
a bounded scope, an isolated mutation area, and a concrete evidence contract.
Parallelize only independent probes when delegation is available and allowed;
otherwise run the same loop sequentially. Avoid overlapping edits and duplicate
experiments. Workers return findings to the orchestrator instead of silently
changing the shared plan or integrating their own preferred candidate.

Keep one compact record in the existing task/workflow artifacts:

`ID | owner | hypothesis | probe/baseline | evidence | outcome | next decision`

Share negative results and changed assumptions before assigning more work.
Compare competing candidates against the same criteria and inputs. Reconcile
conflicting evidence with a targeted probe. The orchestrator selects what to
integrate and stops redundant work once the uncertainty is resolved. Preserve
the goal, best validated state, rejected approaches, and next probe for handoff.

## Converge and finish

Stop exploring when the goal is met, the budget is exhausted, the next probe
needs unavailable authority or capabilities, or two consecutive experiments
produce neither useful evidence nor improvement. At a checkpoint, continue
only with a materially different, evidence-backed probe within the remaining
budget; otherwise report the blocker or unresolved uncertainty.

Validate the selected candidate against the original acceptance criteria and
relevant regression checks after integration. Label prototypes as prototypes
until that validation succeeds; promising measurements alone are not delivery.
Clean up disposable artifacts while retaining reproducible evidence and useful
negative findings. Report the outcome against the goal, what was tried, what
was learned, what was kept or reverted, checks actually run, and remaining
uncertainties. If nothing worked, say so and identify the best next experiment.
