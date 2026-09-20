---
name: evidence-visualization
description: Turn an analytical result into a chart or figure a reader can act on and check -- answering the actual question, honest axes, and a carried source, query, and time range. Use to present a data finding. Not for product or application interfaces -- that is the frontend-craft family.
compatibility: artifact.render makes the output directly viewable; not required.
metadata:
  x-zirv-schema-version: "1"
  x-zirv-id: evidence-visualization
  x-zirv-version: "1"
  x-zirv-name: Evidence visualization
  x-zirv-triggers: chart this,visualize the result,make a graph,plot the finding,present this data
  x-zirv-phases: present
  x-zirv-required-capabilities: repo.read
  x-zirv-optional-capabilities: artifact.render
  x-zirv-context-budget-bytes: "2400"
---

A chart that is technically accurate but answers a different question than
the one asked forces the reader to do the translation themselves, and most
readers will not notice that translation is needed -- they will just draw the
wrong conclusion with full confidence.

## Method

1. Restate the question the chart must answer before picking a chart type. A
   trend question needs a line over time; a comparison question needs bars or
   a ranked list; picking the type first and fitting the question to it
   produces a chart that looks like an answer without being one.
2. Set axes to represent the data honestly -- start bars at zero, state when
   an axis is log-scaled, and do not truncate a range specifically to
   exaggerate a difference. An axis chosen to flatter the story is a claim
   about the axis, not just a style choice.
3. Recognize when there is no chart to make. Three numbers are a sentence,
   not a chart; forcing them into a visualization adds decoding effort without
   adding information.
4. Carry the source, the query, and the time range on the artifact itself, not
   only in surrounding prose that can get separated from it. A chart nobody
   can trace back to its query cannot be checked when someone doubts it later,
   and someone eventually will.
5. Check the visualization still answers the question after styling is
   applied. Color, sorting, and annotation choices can each independently
   shift what a reader takes away, even when the underlying numbers are
   untouched.

Failure modes: defaulting to a chart type because it is available in the
plotting library rather than because it fits the question; omitting the time
range because "it's obviously the latest data," which stops being obvious the
moment the artifact is reused later; using color to encode a category that a
legend never actually explains.

Boundary: this covers a chart or figure built to carry evidence for a
specific finding; a product or application surface is the frontend-craft
family; `stakeholder-summary` is the surrounding narrative a chart supports,
not a substitute for one.

## Contract

State the question the chart answers, confirm axes are not misleading, and
report the source, query, and time range attached to the artifact. If three
numbers or fewer are involved, say so and give the sentence instead of forcing
a chart.
