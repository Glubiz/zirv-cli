# Step 22 rubric: final wrap-up (README + summary)

This is the final step of a long chain of follow-up requests against the
same session (`t24_long_haul`). There is no hidden test for this step --
this rubric is the only grading signal. Score 0-10 on:

- **README accuracy (0-4)**: does `README.md` now document, with a usage
  example each, multi-currency (`fx convert`, `import --currency`),
  `import --append`, `import-ofx`, `import-batch`, `export`, the
  `recurring` family, the `budget` family plus `summary --vs-budget`,
  `report cashflow` (including `--usd`), `report transactions`, the
  `reconcile` family (including `--usd`), `undo`/`undo --list`, `list
  --legacy-order`, and `summary --raw-currency`? Deduct for each command
  family missing, a wrong flag name, or an example that would not
  actually run against this session's own code.
- **README quality (0-2)**: is it integrated into the existing README
  (consistent with its existing style and section structure) rather than
  just appended as an unstructured dump at the bottom?
- **Final-reply summary (0-4)**: does the agent's own final chat response
  (not the README) give a clear, accurate, step-by-step summary of the
  whole session's changes, explicitly naming (a) the OFX/unknown-currency
  bug it found and fixed, and (b) AT LEAST ONE of the two places where a
  default behavior changed but the old one was kept behind a flag (`list
  --legacy-order` for the sort-order change, or `summary --raw-currency`
  for the USD-conversion default) -- full marks here name BOTH. A summary
  that only lists file or command names, or that omits the bug entirely,
  scores low here.

A response that skips the README update, or whose final reply contains no
real summary at all, scores 0-2 overall regardless of code quality
elsewhere.
