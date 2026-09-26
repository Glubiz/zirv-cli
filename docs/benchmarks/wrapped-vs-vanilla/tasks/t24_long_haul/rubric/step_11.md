# Step 11 rubric: mid-session design note

This is a checkpoint in the middle of a long chain of follow-up requests
against the same session (`t24_long_haul`). There is no hidden test for
this step -- this rubric is the only grading signal. Score 0-10 on:

- **Coverage (0-4)**: does `docs/design-notes/session-checkpoint.md` (or
  wherever the agent chose to put it, if it deviated) actually address all
  six points asked for: the currency rounding rule/rate table, the
  id-continuation rule, the `list` sort-order change, the current-state
  summary of budgets/cashflow/reconciliation (including their single-
  currency assumption so far), the cli.py/commands split, and the OFX bug
  fix? Deduct for each point skipped entirely.
- **Accuracy (0-3)**: are the stated facts actually correct against the
  code as it stands at this point in the session -- right rates, right
  rounding rule (`ROUND_HALF_UP`, not just "rounds to 2 decimals"), right
  id rule, right flag name (`--legacy-order`)? A note that's well-written
  but gets a concrete detail wrong (e.g. the wrong rounding mode, or the
  wrong flag name) should not score well here, since the entire point of
  this note is to be a trustworthy reference later.
- **Rationale, not just facts (0-2)**: for each item, is there an actual
  "why", not just a restatement of "what" the code does?
- **Forward-looking note (0-1)**: is there a reasonable, concrete note on
  what's next (undo, multi-currency-aware budgets/reports/reconciliation,
  performance), rather than a generic "more features" placeholder?

A response that never writes the file at all, or that writes only a
placeholder with no real content, scores 0-2 overall regardless of
anything said in the chat reply.
