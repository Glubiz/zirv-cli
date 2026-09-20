---
name: data-source-audit
description: Establish whether a dataset can be trusted before using it -- producer, grain, freshness, nullability, known gaps, producer-down behavior, and personal data. Use before building on an unfamiliar dataset. Not for validating a pipeline's ongoing correctness -- that is `data-quality-validation`.
metadata:
  x-zirv-schema-version: "1"
  x-zirv-id: data-source-audit
  x-zirv-version: "1"
  x-zirv-name: Data source audit
  x-zirv-triggers: data source,dataset audit,what does this table mean,data lineage,grain,data dictionary
  x-zirv-phases: design
  x-zirv-required-capabilities: repo.read
  x-zirv-context-budget-bytes: "2500"
---

Most misuse of a dataset starts one question earlier than people check: what
does one row actually mean. Getting the grain wrong makes every aggregate
built on top of it wrong in a way that still looks plausible.

## Method

1. Identify the producer and how the data reaches its current location --
   direct write, event stream, batch ETL, manual upload. Each path has a
   different failure mode, and the failure mode is what eventually explains
   the anomaly you find later.
2. Answer the grain question explicitly: what does one row represent, and is
   that true for every row or only most of them. A table that is one-row-per-
   customer except for a legacy bulk-import batch will silently corrupt any
   count or join that assumes uniformity.
3. Establish freshness and lag: how current is the data, and does lag vary by
   segment. A dataset that is real-time for one source system and batched
   nightly for another produces numbers that are internally inconsistent at
   any given moment, not just stale.
4. Check nullability and known gaps against the schema's stated intent, not
   just its declared type. A column marked required at the database level can
   still be functionally empty for an entire era before a feature existed.
5. Determine what happens downstream when the producer is unavailable --
   does the pipeline fill zeros, skip the interval, or replay later. Each
   produces a different, easily misread shape in the data.
6. Note whether the dataset carries personal data, directly or by
   reconstruction from a combination of fields, and what handling obligations
   that creates for anyone who queries it next.

Failure modes: trusting a column name over what values it actually contains;
assuming grain is stable because it was checked once, when a producer change
silently altered it; treating a data dictionary as current when nobody
updates it alongside schema changes.

Boundary: this establishes trust in a dataset before use; ongoing pipeline
correctness after that trust is established is `data-quality-validation`;
answering a specific question from the data is `data-analysis`. Read-only.

## Contract

Report the producer and path, the grain with any exceptions found, freshness
and lag characteristics, nullability against real values, known gaps, the
pipeline's behavior when the producer is down, and whether personal data is
present. State plainly which of these could not be established from available
evidence.
