# Step 20 rubric: decision log (design note)

This step is the paper trail midway through a long chain of follow-up
requests against the same session (`t27_tax_season`). There is no hidden
test for this step -- this rubric is the only grading signal. Score 0-10 on:

- **Coverage (0-3)**: does `docs/design-notes/ledger-hygiene.md` exist and
  cover all six requested topics (rule resolution, clean merchant names,
  `merge` duplicates, split cents and `amend`, the tax year, backup/restore
  integrity), each with a decision AND a rationale? Deduct for each missing
  topic or any topic that states a decision without saying why.
- **Accuracy of recalled decisions (0-5)**: these are the facts the session
  actually established; check each against the diff so far and deduct for
  any that is wrong or stale:
  - user rules always beat the built-in rules; among user rules the highest
    priority wins and, on equal priority, the MOST RECENTLY ADDED rule
    (highest id) wins -- this REPLACED an earlier "longer pattern wins, then
    lowest id" tie-break (a note that still says longer-pattern-wins, or
    never mentions the reversal, loses points);
  - the clean merchant name drops `#123` tokens and a trailing digit-only
    token of 3+ digits, collapses whitespace, keeps letter case, and is
    applied on `import`, `merge` and `normalize` (original kept in
    `raw_payee`);
  - a `merge` duplicate is same clean payee (case-insensitive), same signed
    amount, within the window (default 2 days); after the bug fix each
    existing transaction absorbs at most ONE incoming row (closest date,
    then lowest id), and rows within the same file never match each other;
  - split parts are whole cents, each part gets the floor of its share and
    the leftover cents go one each to the FIRST parts in the order given;
    `amend` re-divides a split with the same percentages using that same
    rule;
  - `tax summary`/`tax export` default to the fiscal year 1 April YEAR to 31
    March YEAR+1, replacing the original calendar-year behaviour, which is
    still available via `--calendar`;
  - backup writes a `.sha256` sidecar and restore verifies it BEFORE
    touching the store.
- **Known gaps and quality (0-2)**: a short, sensible "Known gaps" list that
  follows from the actual state of the code (no invented features that
  already exist), in a concise, readable note.

A response that skips the file entirely scores 0-2 regardless of other
quality.
