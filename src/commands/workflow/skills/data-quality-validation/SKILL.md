---
name: data-quality-validation
description: Validate a dataset or pipeline with assertions that fail loudly -- row counts, referential integrity, duplicate keys, distribution drift, late or out-of-order arrivals. Use to check a pipeline's ongoing correctness. Not for judging whether a dataset can be trusted for a new use -- that is `data-source-audit`.
compatibility: repo.read; shell.exec and a test runner make executing the assertions materially faster.
metadata:
  x-zirv-schema-version: "1"
  x-zirv-id: data-quality-validation
  x-zirv-version: "1"
  x-zirv-name: Data quality validation
  x-zirv-triggers: data quality,pipeline validation,data assertions,row count check,schema drift,duplicate keys
  x-zirv-phases: test
  x-zirv-required-capabilities: repo.read
  x-zirv-optional-capabilities: shell.exec,test.run
  x-zirv-context-budget-bytes: "2600"
---

A schema check passing tells you the shape arrived intact; it says nothing
about whether the values inside it still mean what they meant last week.
Treating the two as one check is how a semantically broken pipeline keeps
reporting green.

## Method

1. Assert row counts against an expected range, not an exact number -- pick
   the range from historical variance, not from the last run's count, or the
   check degrades into confirming the pipeline repeated whatever it did
   before, including a repeated failure.
2. Check referential integrity explicitly: does every foreign key resolve to
   a real row on the other side. A silent orphaned reference produces correct-
   looking joins that quietly drop rows.
3. Check for duplicate keys where the schema promises uniqueness. Duplicates
   introduced upstream inflate every downstream count that joins against the
   key, and the inflation is proportional to the join fan-out, so a small
   duplicate rate can produce a large visible error further down.
4. Watch distribution drift on the fields that matter for downstream
   decisions -- a category's share, a numeric field's range -- not just
   whether values are technically valid. A field can stay perfectly valid
   while its distribution shifts enough to invalidate a model or a report
   built on the old shape.
5. Handle late-arriving and out-of-order data as an expected case with a
   defined grace window, not as a bug to route around ad hoc. A validation
   pass that runs before the grace window closes will flag correct data as
   missing.
6. Make every assertion fail loudly and specifically -- name which row, which
   key, which threshold -- because a check that only logs a warning gets
   ignored at exactly the volume where it matters most.

Failure modes: writing a schema check and treating it as a semantic
guarantee; setting thresholds once and never revisiting them as real volume
changes; validating only the happy path and never simulating a late or
missing batch.

Boundary: this checks an existing pipeline's ongoing health; whether a
dataset is fit to trust for a new use in the first place is `data-source-
audit`; `statistical-sanity` is for auditing a numeric claim's reasoning, not
a pipeline's mechanics.

## Contract

Report each assertion run, its threshold and rationale, pass/fail per check,
and for any failure the specific rows or keys implicated. State which checks
are schema-level versus semantic, and mark anything not covered by an
assertion as unvalidated.
