# Step 12 rubric: mid-session decisions note

This is a checkpoint in the middle of a long chain of follow-up requests
against the same session (`t26_household`). There is no hidden test for this
step -- this rubric is the only grading signal. Score 0-10 on:

- **Coverage (0-4)**: does `docs/household-decisions.md` (or wherever the
  agent put it, if it deviated) address all five asked-for points: the
  leftover-cent rule and its change, the balance sign convention and the
  commands sharing it, the `settle suggest` procedure and tie-breaking, why
  transfers are paired entries, and the history/undo behaviour and why it is
  append-only? Deduct for each point skipped. Is there a "still open" list?
- **Accuracy (0-4)**: are the facts correct against the code as it stands
  at this step? The things that must be right: the OLD rule gave leftover
  cents to the first participant onwards, the NEW rule starts at participant
  `(N - 1) mod P` for expense id N and P participants and wraps around,
  already-recorded expenses keep their stored shares; positive balance means
  the household owes the member; transfers raise the sender's balance and
  lower the receiver's; ties in `settle suggest` go to the earlier-added
  member; undo appends an `undo #SEQ` entry instead of deleting entries and
  undo entries cannot be undone. A note that is well written but gets one of
  these wrong (for example describing the old first-participant rule as
  current) should not score well here -- the note is meant to be trusted.
- **Rationale, not just facts (0-2)**: for each point, is there an actual
  "why" rather than a restatement of what the code does?

A response that never writes the file, or writes only a placeholder, scores
0-2 overall regardless of what the chat reply says.
