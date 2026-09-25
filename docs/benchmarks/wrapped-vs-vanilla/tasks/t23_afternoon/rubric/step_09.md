# Step 9 rubric: wrap-up (README + summary)

This is the final step of a long chain of follow-up requests against the
same session (`t23_afternoon`). Score 0-10 on:

- **README accuracy (0-4)**: does `README.md` now document, with a usage
  example each, the merchant alias commands (`alias add`/`list`/`remove`),
  the category alias commands (`category-alias add`/`list`), `list
  --canonical`, `summary --canonical-category`, and `merchants`? Deduct for
  a missing command, a wrong flag name, or an example that would not
  actually run against this session's own code.
- **README quality (0-2)**: is it integrated into the existing README
  (consistent with its existing style/section structure) rather than just
  appended as an unstructured dump?
- **Final-reply summary (0-4)**: does the agent's own final chat response
  (not the README) give a clear, accurate, step-by-step summary of the
  whole session's changes, explicitly naming the case-insensitivity bug fix
  (step 3) and the canonical-category default it reverted (step 7)? A
  summary that only lists file names, or that omits the bug/revert, scores
  low here.

A response that skips the README update, or whose final reply contains no
real summary at all, scores 0-2 overall regardless of code quality
elsewhere (there is no hidden test for this step -- this rubric is the only
grading signal).
