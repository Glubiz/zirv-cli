---
name: alert-rule-diagnosis
description: Work out why an alerting rule fired or failed to fire -- separate threshold, evaluation window, and evaluation delay, and check what the rule does when data is simply missing. Use for diagnosing one alert's behavior. Not for reviewing a dashboard's panels as a whole -- that is dashboard-review.
compatibility: Requires a configured kibana integration; repo.read.
metadata:
  x-zirv-schema-version: "1"
  x-zirv-id: alert-rule-diagnosis
  x-zirv-version: "1"
  x-zirv-name: Alert rule diagnosis
  x-zirv-triggers: alert didn't fire,why did this alert fire,flapping alert,alert rule debug,silent alert
  x-zirv-phases: debug
  x-zirv-required-capabilities: repo.read
  x-zirv-required-integrations: kibana
  x-zirv-context-budget-bytes: "2300"
---

Silent non-firing hides in the gap between "no data met the condition" and
"no data arrived to be evaluated," and most rule configurations do not
distinguish them. A rule can look correctly configured and still never fire
in exactly the failure mode it exists to catch.

## Method

1. Separate the three timing components explicitly: the threshold value, the
   evaluation window it aggregates over, and the delay between an event
   occurring and the rule evaluating it. Confusing window length with delay
   produces a wrong diagnosis of why a fire was late or missed.
2. Check the rule's configured behavior when the underlying query returns no
   data at all, not just a value below threshold -- some rule types treat
   missing data as "condition not met" (never fires) and others as
   "condition met" (fires as a false positive); this is where silent
   non-firing usually lives.
3. Check for flapping: a threshold sitting near a naturally noisy metric will
   fire and resolve repeatedly. This is a rule-design problem (missing
   hysteresis, too short a window) rather than evidence the underlying
   condition is actually unstable.
4. Check whether the rule alerts on a cause (a queue depth, a CPU percentage)
   or a symptom (user-facing error rate, failed checkouts). A cause-based
   alert can fire on a benign fluctuation that never reaches users, or stay
   silent on a symptom-causing combination it was not built to catch.
5. Apply the page-worthiness test to any rule under review: if firing it
   would not change what an on-call person does that night, it belongs on a
   dashboard as a signal to review later, not as a page.

## Read-only boundary

This skill only diagnoses; it does not edit the rule. Changing a rule's
threshold, window, or actions is `saved-object-change-management`'s job,
which requires authorization at the point of change.

## Contract

Report the threshold, window, and delay as separate figures, the rule's
missing-data behavior, whether flapping is present and why, whether the rule
targets cause or symptom, and a page-worthiness verdict. Say "behavior
undetermined" rather than assuming a rule's missing-data handling from its
type name alone. Reviewing the dashboard the rule's data feeds is
`dashboard-review`'s job.
