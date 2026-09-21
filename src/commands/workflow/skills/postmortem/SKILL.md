---
name: postmortem
description: Write the record after an incident is over -- reconstruct what each person knew at the time, prefer contributing conditions over a single root cause, and give every action item an owner. Use once an incident has resolved. Not the live investigation itself, which is incident-investigation.
metadata:
  x-zirv-schema-version: "1"
  x-zirv-id: postmortem
  x-zirv-version: "1"
  x-zirv-name: Postmortem
  x-zirv-triggers: postmortem,post-mortem,incident writeup,retro on the outage,write the incident report
  x-zirv-phases: present
  x-zirv-required-capabilities: repo.read
  x-zirv-context-budget-bytes: "2000"
---

A postmortem written to assign blame teaches people to hide information next
time; one written as if the cause were obvious in hindsight teaches nothing,
because it erases the decision points where a different action was possible.
Blameless does not mean vague about mechanism.

## Method

1. Reconstruct a timeline from what each person actually knew and saw at each
   moment, not what is obvious now with the outcome known. "Should have
   noticed" is almost always hindsight bias; ask what signal was available and
   whether it was distinguishable from normal noise at the time.
2. Name the mechanism precisely -- the sequence of conditions that let the
   failure happen -- rather than stopping at the first plausible cause. Most
   incidents have several contributing conditions (a missing alert, an
   ambiguous runbook, a recent change, a monitoring gap); a single "root
   cause" usually means the analysis stopped early.
3. Separate what limited impact from what failed outright. A system that
   degraded instead of crashing did something right that deserves naming
   alongside what went wrong.
4. Write each action item with an owner, a way to verify it was done, and a
   place it lives (a tracked issue, not just the document). An item with only
   a description is a wish, not a commitment.
5. Distinguish action items that would have prevented this incident from ones
   that only make the next one easier to detect or contain; conflating them
   overstates how fixed the underlying risk is.

## Contract

Produce a timeline anchored to evidence, the contributing conditions (not a
single cause unless one factor is genuinely sufficient and necessary), what
limited impact, and an action-item list each with owner, verification, and
tracking location. Say "contributing conditions unclear" rather than naming
one that fits the narrative better than the evidence. The live investigation
that supplies this evidence is `incident-investigation`'s job, not this
skill's.
