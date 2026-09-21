---
name: project-cycle-planning
description: Plan a cycle of work for a team using observed throughput rather than optimism, sequence to retire risk and unblock dependents first, and leave explicit slack for interrupts. Use for team-level cycle or sprint planning. Not for decomposing one engineering change into steps -- that is plan or write-plan.
metadata:
  x-zirv-schema-version: "1"
  x-zirv-id: project-cycle-planning
  x-zirv-version: "1"
  x-zirv-name: Project cycle planning
  x-zirv-triggers: sprint planning,cycle planning,plan the sprint,capacity planning,plan next cycle
  x-zirv-phases: plan
  x-zirv-required-capabilities: repo.read
  x-zirv-context-budget-bytes: "2000"
---

A cycle plan built from optimism or from headcount times days fails on
schedule almost every time, because it prices in no interrupts, no unknowns,
and no dependency stalls. The plan that survives contact with the week is the
one built from what the team actually delivered last time.

## Method

1. Derive capacity from observed throughput over recent cycles, not from
   multiplying people by working days. Actual delivered output already nets
   out meetings, review time, on-call, and the ordinary friction a capacity
   formula ignores.
2. Sequence work to retire the highest-uncertainty items and unblock the most
   dependents first, not by priority label alone. An item ranked highest that
   blocks nothing can wait behind a lower one that three others depend on.
3. Reserve explicit slack for interrupts -- production issues, support
   escalations, urgent requests -- sized from how often they actually
   occurred last cycle. A plan with none fails on its first ordinary week and
   looks like poor execution when it was poor planning.
4. State plainly what will not be done this cycle, not just what will. An
   unstated exclusion becomes a surprised stakeholder later; a stated one is
   a decision someone can push back on now, while there is still time to
   trade something else out.
5. Flag items whose estimate depends on an unresolved unknown (an unreviewed
   design, an external dependency's availability) separately from ordinary
   work, since their risk to the plan is different in kind, not just size.

## Contract

Report the capacity figure and its basis, the sequenced work with what
depends on what, the slack reserved and its basis, and the explicit exclusion
list. Say "insufficient history" rather than inventing a throughput number
when recent cycles are not comparable. Decomposing one engineering change
into implementation steps belongs to `plan` and `write-plan`, not here.
