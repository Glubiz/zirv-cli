---
name: design-review
description: Review a proposed design before anything is built -- the problem statement, reversibility, failure modes, cost of being wrong, and what the design forecloses. Use on a design doc, RFC, or proposal. Not for reviewing a diff against an existing design -- that is `review`.
metadata:
  x-zirv-schema-version: "1"
  x-zirv-id: design-review
  x-zirv-version: "1"
  x-zirv-name: Design review
  x-zirv-triggers: design review,rfc,rfc review,proposal review,review this design,architecture review
  x-zirv-phases: review
  x-zirv-required-capabilities: repo.read
  x-zirv-context-budget-bytes: "2300"
---

A well-executed solution to the wrong problem passes every downstream check a
reviewer runs, because none of those checks ever ask whether the problem
statement was right. Reviewing the solution before the problem wastes the
review.

## Method

1. Review the problem statement first, on its own, before reading the
   proposed solution. Ask what evidence supports it and who it was validated
   with. A design built on an unvalidated or stale problem cannot be saved by
   a good solution.
2. Check reversibility next: what does it cost to undo this if it is wrong,
   and does that cost grow with time, data volume, or number of dependents.
   A one-way door deserves scrutiny an easily-reversed choice does not.
3. Enumerate failure modes the design introduces, not just the ones it fixes.
   A design is often sold entirely on the problem it solves while its own new
   failure modes go unstated.
4. Weigh cost of being wrong against cost of the review itself -- a low-stakes,
   reversible design does not need the same scrutiny as one that commits
   months of build time or a public contract.
5. State what the design forecloses: what future option becomes harder or
   impossible once this ships. A design that is silent about what it gives up
   has not actually been evaluated, only advocated for.

Failure modes even a careful reviewer falls into: approving a design because
its prose is confident and well organized rather than because its claims were
checked; treating the absence of an objection as evidence the design is
sound; reviewing implementation detail while skipping the problem statement
because it was written by someone senior.

Boundary: this reviews an intention before code exists; `review` reviews a
diff against what was already decided; `adr-authoring` is where the decision
that survives this review gets recorded with its rejected alternatives.

## Contract

Report whether the problem statement holds, the reversibility class of the
decision, the failure modes introduced, what it forecloses, and a clear
verdict -- approve, revise, or reject -- with the specific reasoning for each.
Say plainly when there is not enough information to judge a point rather than
approving on an assumption.
