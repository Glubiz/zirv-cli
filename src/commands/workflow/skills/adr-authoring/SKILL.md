---
name: adr-authoring
description: Write an architecture decision record that captures the forcing constraint, the rejected alternatives, and what evidence would reverse the call. Use once a decision has actually been made. Not for producing the implementation steps -- that is `write-plan`.
metadata:
  x-zirv-schema-version: "1"
  x-zirv-id: adr-authoring
  x-zirv-version: "1"
  x-zirv-name: ADR authoring
  x-zirv-triggers: adr,decision record,architecture decision,record the decision,design decision
  x-zirv-phases: design
  x-zirv-required-capabilities: repo.read
  x-zirv-optional-capabilities: repo.write
  x-zirv-context-budget-bytes: "2200"
---

A decision record with no rejected alternative is a description, not a
decision -- it cannot tell a future reader why the obvious other option was
wrong, so the same debate reopens the moment someone new arrives.

## Method

1. State the constraint that forced the decision, not just the choice made. A
   record that says what was picked but not what made every other option
   unacceptable gives a future reader nothing to check their own situation
   against.
2. List the alternatives seriously considered and why each lost. An
   alternative dismissed in one clause was probably not evaluated -- if you
   cannot state its real failure mode, go back and evaluate it before writing
   it down.
3. Name the evidence that would reverse this decision. A decision with no
   stated reversal condition is treated as permanent by default, which is
   rarely what was intended and rarely true.
4. Record consequences honestly, including the ones that cut against the
   choice made. An ADR that reads as pure advocacy will be discounted the
   first time it is checked against reality.
5. Set status deliberately: proposed, accepted, superseded. A superseded
   record is never rewritten in place -- edit history destroys the exact
   record of what was believed when the original call was made. Write a new
   record and link it both ways.

Failure modes: writing the ADR to justify a decision already implemented
rather than to record the reasoning that led there; omitting the constraint
because it seemed obvious at the time, which is exactly when it stops being
obvious later; conflating "no one objected" with "alternatives were
evaluated."

Boundary: this produces the durable record of why; `write-plan` produces the
steps for how; `design-review` is the check applied before a design is built,
not the record of what was decided afterward.

## Contract

Produce a record with a stated status, the forcing constraint, each rejected
alternative with its specific failure mode, the consequences accepted, and the
condition that would trigger superseding it. If the decision cannot be traced
to an actual constraint, say so rather than inventing one that reads well.
