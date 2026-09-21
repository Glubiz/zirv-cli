---
name: statistical-sanity
description: Audit a numeric claim, including your own, for base rate, denominator, survivorship and selection bias, multiple comparisons, and the gap between statistically significant and practically large. Use before accepting or presenting any quantitative claim. Not for checking the pipeline that produced the number -- that is `data-quality-validation`.
metadata:
  x-zirv-schema-version: "1"
  x-zirv-id: statistical-sanity
  x-zirv-version: "1"
  x-zirv-name: Statistical sanity check
  x-zirv-triggers: is this significant,sanity check this number,p-value,statistical significance,does this claim hold up
  x-zirv-phases: review
  x-zirv-required-capabilities: repo.read
  x-zirv-context-budget-bytes: "2500"
---

"Statistically significant" and "large enough to matter" answer different
questions, and a claim that quietly substitutes one for the other survives
review far more often than it should -- including when the claim is your own
and feels obviously correct because you just computed it.

## Method

1. Check the base rate before the claimed effect. A percentage change against
   an unstated base rate can describe either a meaningful shift or a jump from
   two events to three; the claim is uninterpretable until the base rate is
   named.
2. Check the denominator: what population was this computed over, and does
   that population match what the claim implies. A rate computed over a
   filtered or self-selected subset described as if it were the whole
   population overstates its own reach.
3. Check for survivorship: does the data only include cases that made it to
   where you could observe them. A claim about "successful" cases that never
   accounts for the ones that dropped out earlier attributes causes to the
   wrong side of a selection effect.
4. Check for multiple comparisons: how many things were tested before this
   one was reported. One notable result out of twenty tried is close to what
   chance alone predicts, and the claim should say how many comparisons were
   made, not just the one that looked interesting.
5. Separate statistical significance from practical size explicitly. A large
   enough sample makes a trivial difference statistically detectable; state
   the effect size in units a reader can act on, not just the significance
   test's verdict.

Failure modes: applying this rigor to someone else's claim but skipping it on
a number you produced yourself in the same session; treating a p-value as the
probability the claim is true, which it is not; reporting the single test
that confirmed a hypothesis while omitting the ones that did not.

Boundary: this audits the reasoning behind a number; whether the pipeline
that produced the underlying data is mechanically sound is
`data-quality-validation`; `data-analysis` is where the number itself gets
computed and reconciled in the first place.

## Contract

Name the claim's single weakest link -- base rate, denominator, survivorship,
multiple comparisons, or significance-versus-size -- rather than issuing a
blanket verdict. State plainly when the available evidence is insufficient to
judge the claim at all.
