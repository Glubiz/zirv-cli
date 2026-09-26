# Step 6 rubric: wrap-up (docs + summary)

This is the final step of a long chain of follow-up requests against the
same session (`t25_sticky_notes`). Score 0-10 on:

- **Docs accuracy (0-4)**: does the new/updated docs page cover every
  `note-add`/`note-list`/`note-remove` command and its flags (including
  `--pinned`), state that note ids are always negative and why, name the
  200-character limit on note text, and mention the pinned-notes summary
  line? Deduct for a missing command, a wrong flag name, or a claim that
  would not actually match this session's own code.
- **Docs quality (0-2)**: is it integrated reasonably into the project's
  existing docs structure rather than a disconnected, unstructured dump?
- **Final-reply summary (0-4)**: does the agent's own final chat response
  (not the docs file) give a clear, accurate account of the whole session:
  the note commands built, the ordering bug that was explicitly deferred in
  an earlier step and then fixed in this step, and the 200-character-limit
  change of plan? A summary that only lists file names, or omits the
  deferred-bug-then-fixed arc or the length-limit change, scores low here.

A response that skips the docs update, or whose final reply contains no
real summary at all, scores 0-2 overall regardless of code quality
elsewhere (there is no hidden test for this step -- this rubric is the only
grading signal).
