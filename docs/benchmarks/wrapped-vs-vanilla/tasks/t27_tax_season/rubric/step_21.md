# Step 21 rubric: final wrap-up (README + summary)

This is the final step of a long chain of follow-up requests against the
same session (`t27_tax_season`). There is no hidden test for this step --
this rubric is the only grading signal. Score 0-10 on:

- **README accuracy (0-4)**: does `README.md` now document, with a usage
  example each, `rules add/list/explain` (and the resolution order: user
  rules beat built-ins, then priority, then newest rule), `normalize` and
  merchant-name cleaning, `merge` with `--window`, `split` / `split-show`
  with the remainder-cents rule, `amend`, `tax mark/unmark/list`, `tax
  summary` and `tax export` (including the April-March fiscal year default
  and `--calendar`), `receipt attach/show/verify`, `doctor` and its four
  checks (`duplicate-id`, `split-mismatch`, `uncategorized-deductible`,
  `missing-receipt`), `search` with its terms, and `backup` / `restore`?
  Deduct for each command family missing, a wrong flag name, a stale claim
  (e.g. longer-pattern-wins, or a calendar-year default), or an example that
  would not actually run against this session's own code.
- **README quality (0-2)**: is it integrated into the existing README
  (consistent with its existing style and section structure) rather than
  just appended as an unstructured dump at the bottom?
- **Final-reply summary (0-4)**: does the agent's own final chat response
  (not the README) give a clear, accurate, step-by-step summary of the
  whole session's changes, explicitly naming (a) the `merge` bug found and
  fixed (two identical purchases: both incoming rows were skipped against a
  single existing one; the fix matches incoming rows to existing
  transactions one-to-one), and (b) the two places the earlier behaviour was
  changed on purpose -- the rule tie-break (longer pattern wins -> newest
  rule wins) and the tax year (calendar year -> April fiscal year, old
  behaviour kept behind `--calendar`)? Full marks need (a) and both halves
  of (b). A summary that only lists file or command names, or that omits
  the bug entirely, scores low here.

A response that skips the README update, or whose final reply contains no
real summary at all, scores 0-2 overall regardless of code quality
elsewhere.
