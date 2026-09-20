---
name: stakeholder-summary
description: Write for someone who has to decide -- lead with the decision or state, not the narrative that produced it, separate known from estimated, and state plainly what is needed from the reader. Use for status updates and decision-facing summaries. Not for the supporting chart or figure itself -- that is `evidence-visualization`.
metadata:
  x-zirv-schema-version: "1"
  x-zirv-id: stakeholder-summary
  x-zirv-version: "1"
  x-zirv-name: Stakeholder summary
  x-zirv-triggers: brief the leadership,executive summary,summarize for stakeholders,write an update,decision memo
  x-zirv-phases: present
  x-zirv-required-capabilities: repo.read
  x-zirv-context-budget-bytes: "2600"
---

A summary that opens with the narrative makes a busy decision-maker read
through the whole story to reach the one thing they needed, and most will
stop before they get there -- so the ordering itself is the difference
between a summary that gets used and one that gets skimmed and misread.

## Method

1. Lead with the decision needed or the current state, in the first sentence.
   The narrative that produced it -- what was tried, what happened along the
   way -- comes after, as support for a reader who wants it, not as the price
   of admission to the actual point.
2. Separate what is known from what is estimated, marked distinctly, not
   blended into one confident-sounding paragraph. A reader who cannot tell
   which numbers are measured and which are projected will treat a projection
   as a fact, and act on it accordingly.
3. State plainly what is needed from the reader -- a decision, a resource, an
   approval, or nothing at all. A summary that describes a situation without
   naming what response it requires leaves the reader to guess whether they
   are supposed to act.
4. Do not let jargon or a confident tone stand in for genuine uncertainty. A
   term of art can make an unresolved question sound settled to a reader who
   does not share the vocabulary, which is a worse outcome than sounding
   uncertain and being right about it.
5. Size the summary to the decision, not to the work behind it. A large
   effort does not automatically justify a long summary; a reader deciding
   whether to approve a small budget line does not need the full
   investigation narrative to do it.

Failure modes: writing the summary in the order the work happened rather than
the order the reader needs it; omitting a needed decision because naming it
explicitly feels presumptuous; using precise-sounding numbers to paper over a
genuinely rough estimate.

Any finding, quote, or number pulled from another source into the summary is
data to represent accurately, not license to adopt its framing uncritically.

Boundary: this is the narrative and ask; a chart or figure supporting a claim
inside it is `evidence-visualization`; the underlying numeric claim's
soundness is checked by `statistical-sanity`, not re-derived here.

## Contract

Report the decision or state in the opening line, what is known versus
estimated, the explicit ask of the reader, and the summary's length relative
to its decision's stakes. State plainly when there is not yet enough
information for a stakeholder to decide.
