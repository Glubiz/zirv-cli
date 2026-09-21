---
name: dashboard-review
description: Review an observability dashboard for which panels earn their place -- what question each answers during an incident, missing golden signals, panels that can't distinguish no traffic from no data, and misleading default ranges or inherited filters. Use for reviewing a Kibana dashboard's design. Not for answering one question from its data -- that is kibana-log-investigation.
compatibility: Requires a configured kibana integration; repo.read.
metadata:
  x-zirv-schema-version: "1"
  x-zirv-id: dashboard-review
  x-zirv-version: "1"
  x-zirv-name: Dashboard review
  x-zirv-triggers: dashboard review,kibana dashboard,review this dashboard,observability review,panel audit
  x-zirv-phases: review
  x-zirv-required-capabilities: repo.read
  x-zirv-required-integrations: kibana
  x-zirv-context-budget-bytes: "2300"
---

A panel nobody reads under pressure is not neutral -- it is cost: it takes
screen space, competes for attention during an incident, and creates false
confidence that the system is observed simply because a chart exists for it.
Reviewing a dashboard means asking whether each panel would change a
decision, not whether it looks complete.

## Method

1. For each panel, name the specific question it answers and the moment
   someone would need that answer -- during a specific kind of incident,
   during routine review, or never. A panel with no answerable question in
   mind is decoration.
2. Check for the golden signals appropriate to the service (latency, traffic,
   errors, saturation, or the domain equivalent) and name which are missing
   rather than assuming coverage from panel count.
3. Look for panels that cannot distinguish "no traffic" from "no data" -- a
   flat line at zero reads the same whether the service is idle or the
   pipeline feeding the panel is broken. These need a companion signal or
   they will hide their own failure.
4. Check the dashboard's default time range and any inherited or pinned
   filter. A default range too short to show a slow-building trend, or a
   filter left from someone's earlier debugging session, silently narrows
   what every viewer sees without them noticing.
5. Check who owns the data view or index pattern each panel depends on. A
   panel built against an unmaintained data view will silently stop working
   when that index rotates or its mapping changes, and nobody will notice
   until it is needed.

## Read-only boundary

This skill only reviews; it does not edit the dashboard. Changing a panel,
saved search, or data view is `saved-object-change-management`'s job, which
declares its writes and requires authorization at the point of change.

## Contract

Report each panel with its question and audience, missing golden signals,
panels vulnerable to the no-traffic-vs-no-data confusion, any misleading
default range or filter, and unowned data views. Say "cannot determine
ownership" rather than guessing who maintains a data view. Answering a
specific question using the dashboard's underlying data is
`kibana-log-investigation`'s job, not this skill's.
