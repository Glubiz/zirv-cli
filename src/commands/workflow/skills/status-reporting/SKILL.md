---
name: status-reporting
description: Write a recurring status update to people who already read the last one -- report deltas in schedule, scope, and risk rather than restating full state, and separate items needing a decision from items needing only awareness. Use for a periodic team or project update. Not a one-off write-up for a single decision -- that is stakeholder-summary.
metadata:
  x-zirv-schema-version: "1"
  x-zirv-id: status-reporting
  x-zirv-version: "1"
  x-zirv-name: Status reporting
  x-zirv-triggers: status update,weekly update,project status,send a status report,sprint update
  x-zirv-phases: present
  x-zirv-required-capabilities: repo.read
  x-zirv-context-budget-bytes: "1800"
---

A status report that restates everything wastes the reader's time re-deriving
what already changed, and a green status with no evidence behind it is the
most expensive line in the document, because it fails silently until the day
it does not.

## Method

1. Diff against the last report before writing anything -- what moved, what
   slipped, what got added or cut -- and lead with that. A reader who saw the
   last update does not need the parts that are unchanged.
2. State schedule, scope, and risk as deltas with direction: two days later,
   one item added, one risk raised or resolved. A static snapshot forces the
   reader to do the comparison themselves, and they usually skip it.
3. Separate items that need a decision from someone reading the report from
   items that only need awareness. Burying a decision inside a paragraph of
   context means it gets missed until it is overdue.
4. Back every "on track" or "green" claim with the specific evidence behind
   it -- a metric, a completed milestone, a passing check. A status with no
   evidence is a guess dressed as a report, and guesses compound across weeks
   until the miss is large.
5. Flag a status that has not moved in several consecutive reports; unchanging
   status often means the report is not being maintained with the underlying
   work, not that the work is genuinely static.

## Contract

Report what changed since the last update (schedule, scope, risk) with
direction and evidence, the decisions needed with who owns them, and items
needing only awareness. Say "no evidence available" rather than asserting a
green status from the absence of bad news. A one-off write-up built for a
single decision belongs to `stakeholder-summary`, not a recurring cadence.
