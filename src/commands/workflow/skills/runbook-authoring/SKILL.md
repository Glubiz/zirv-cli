---
name: runbook-authoring
description: Write a procedure for a tired stranger to follow under pressure with no context -- verifiable checkpoints, an abort and rollback path, and a named escalation point. Use for on-call or operational procedures. Not for reference material meant to be understood at leisure -- that is `technical-documentation`.
metadata:
  x-zirv-schema-version: "1"
  x-zirv-id: runbook-authoring
  x-zirv-version: "1"
  x-zirv-name: Runbook authoring
  x-zirv-triggers: runbook,on-call procedure,write a playbook,operational procedure,escalation steps
  x-zirv-phases: present
  x-zirv-required-capabilities: repo.read
  x-zirv-optional-capabilities: repo.write
  x-zirv-context-budget-bytes: "2500"
---

A runbook is read by someone paged at three in the morning who does not have
the context the author had while writing it -- any step that quietly requires
judgment the document does not supply becomes the step where the incident
gets worse instead of better.

## Method

1. Write every step with a stated, checkable outcome -- "you should see X" --
   not just an action to perform. A step with no verification leaves the
   follower unable to tell whether it worked before moving to the next one,
   which is exactly when a cascading mistake starts.
2. Put the abort and rollback path before it is needed, not appended at the
   end. Someone mid-procedure who realizes it is going wrong needs the exit
   already in front of them, not buried past the steps they are trying to
   escape.
3. Name the escalation point explicitly: who to page, at what trigger, with
   what information already gathered. A runbook that says "escalate if
   needed" without a name and a trigger guarantees the decision gets made late
   by someone unsure it is their call to make.
4. Remove every step that requires judgment the runbook itself does not
   supply. If a step says "assess whether traffic looks normal," either give
   the specific threshold and where to read it, or admit the runbook cannot
   cover this branch and route to escalation instead.
5. State the preconditions and assumed access up front -- credentials,
   permissions, tools already installed -- so the follower discovers a
   blocker before they are mid-procedure, not during the step that needs it.

Failure modes: writing steps that were accurate when the system looked one
way and never revisiting them after it changed shape; assuming the follower
has the same mental model as the author, which is precisely the assumption
that fails at three in the morning; describing the happy path in detail and
compressing the failure path into a single vague sentence.

Boundary: this is a procedure to be followed under pressure with the judgment
already removed; `technical-documentation` is reference material meant to be
understood, not executed verbatim; `incident-investigation` is what happens
when there is no runbook that covers what is actually occurring.

## Contract

Report the procedure with a checkable outcome per step, the abort and
rollback path, the named escalation point and trigger, and the preconditions
assumed. Flag any step that still requires judgment the document does not
supply, rather than leaving it implicit.
