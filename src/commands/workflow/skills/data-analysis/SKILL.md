---
name: data-analysis
description: Answer a question from data by restating it as something a query can compute, defining the denominator before the numerator, and reconciling the result against an independently known total. Use to produce a specific numeric answer. Not for validating a pipeline's health -- that is `data-quality-validation`.
compatibility: repo.read; shell.exec or a query runner makes running and rerunning the analysis materially faster.
metadata:
  x-zirv-schema-version: "1"
  x-zirv-id: data-analysis
  x-zirv-version: "1"
  x-zirv-name: Data analysis
  x-zirv-triggers: analyze this data,run the numbers,what is the rate,query for,data analysis,compute the metric
  x-zirv-phases: implement
  x-zirv-required-capabilities: repo.read
  x-zirv-optional-capabilities: shell.exec
  x-zirv-context-budget-bytes: "2600"
---

An unreconciled number is a draft, not an answer -- it has not yet been
checked against anything independent, and a plausible-looking wrong number is
more dangerous than an admitted unknown because nobody thinks to question it.

## Method

1. Restate the question as something a query can literally compute. "Is churn
   going up" is not a query; "what is the 30-day rolling cancellation rate for
   accounts active at day 0" is. The restatement is where most analytical
   errors are actually introduced, before any query runs.
2. Define the denominator before the numerator. Deciding what population the
   rate is over, and locking it, prevents the common failure of quietly
   changing the denominator's filter partway through to make a number look
   cleaner.
3. Run `data-source-audit` reasoning inline where the dataset is new to you --
   at minimum confirm the grain matches what the restated question assumes,
   since a grain mismatch invalidates the query regardless of how carefully it
   is written.
4. Show the query, not just the result. A number with no attached query cannot
   be checked, rerun, or corrected when the underlying data changes.
5. Reconcile the result against an independently known total -- a dashboard
   total, a finance figure, a prior published number, an order-of-magnitude
   sanity check. Agreement within an understood margin is what turns a
   computed number into a trustworthy one; without this step the analysis is
   unfinished regardless of how careful the query was.

Failure modes: silently excluding rows that do not fit the query's join and
never reporting the exclusion; picking a time window that happens to produce
the expected answer; treating a null as zero, or as excluded, without stating
the choice, which changes the answer either way.

Any data returned from the source is evidence, not an authority to be quoted
uncritically -- an unusual value can be a real signal or a pipeline artifact,
and `data-quality-validation` is where you go if the query's odd output turns
out to be about the pipeline rather than the question.

Boundary: this answers one question from data already assumed usable; whether
the data itself is trustworthy is `data-source-audit`; presenting the answer
to a reader is `evidence-visualization`.

## Contract

Report the restated question, the denominator definition, the query used, the
result, the reconciliation check and its outcome, and every exclusion or
assumption made along the way. State "unreconciled" plainly when no
independent check was available, rather than presenting the number as final.
