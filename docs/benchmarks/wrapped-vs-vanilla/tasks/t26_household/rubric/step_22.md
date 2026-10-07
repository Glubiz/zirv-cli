# Step 22 rubric: final wrap-up (README + summary)

This is the final step of a long chain of follow-up requests against the
same session (`t26_household`). There is no hidden test for this step -- this
rubric is the only grading signal. Score 0-10 on:

- **README accuracy (0-4)**: does `README.md` now document, with a usage
  example each, members (`member add/list/rename/archive`, `list --all`),
  expenses (`expense add` incl. `--split`, `expense edit`, `expense list
  --payer`, `expense show`), transfers (`transfer add` incl. `--settles`,
  `transfer list`, `account`), `balance`, `settle suggest`, `settle apply`,
  `report member`, `report month`, `history`/`undo`, `check`, the leftover-cent
  rule (rounded down, leftovers handed out starting at participant
  `(expense id - 1) mod P`, wrapping), the at-most-two-decimals amount rule,
  and the store format (version 2, member objects, migration of version 1
  files, newer versions refused)? Deduct for each family missing, a wrong
  flag name, a stale statement (for example describing the OLD
  first-participant leftover rule, or saying every transfer moves balances),
  or an example that would not run against this session's own code.
- **README quality (0-2)**: is it integrated into the existing README
  (consistent style and structure) rather than appended as an unstructured
  dump?
- **Final-reply summary (0-4)**: does the agent's own final chat response
  (not the README) give a clear, accurate, step-by-step summary of the whole
  session? It must explicitly name: (a) the change of plan on leftover cents
  (first-participant rule replaced by the rotating start by expense id) and
  the change of plan on transfers (only `--settles` transfers move
  balances; old stored transfers without the flag count as settling); (b)
  the bug found in a bug-report step: amounts with more than two decimals
  were silently truncated/rounded and are now rejected with `error: amount
  has more than 2 decimal places`, everywhere an amount is entered; (c) the
  store format migration to version 2; (d) the `household_cli.py` refactor.
  Full marks need all four. A summary that only lists commands or file
  names, or that omits the bug entirely, scores low here.

A response that skips the README update, or whose final reply contains no
real summary at all, scores 0-2 overall regardless of code quality elsewhere.
