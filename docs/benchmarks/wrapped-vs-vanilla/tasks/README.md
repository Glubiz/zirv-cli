# ledgerlite benchmark tasks

Eleven tasks plus the orchestration task `o01_ledger_suite` against the `ledgerlite` template (see `../template/`,
which is FROZEN). The short tasks t01-t12, t14 and t16 were dropped from the benchmark (their recorded
results stay under `../results/`); what remains is t13/t15 (LARGE), t17-t21 (XL) and t22 (EPIC), which the
corpus puts in the `jev` lane (Jev-proxy measurement only, see `../README.md` "Lanes"), the chain tasks t23, t24,
t24b and t25, and the orch task. They only target behaviour the frozen template already has, or spec new
features with an exact hidden-test contract; they never modify template/. The template ships with one
known-red visible test, `tests/test_rules.py::test_regex_rule_case_insensitive`, caused by D4 below; every
grader that checks `visible_ok` treats that single failure as baseline.

Latent defects planted in the template: D1 `parse_money` mishandles
parenthesized negatives and thousands separators (models.py); D2
`report.page` drops the first item of page 1 and empties an exact-multiple
final page (report.py); D3 categorize()'s priority/tie-break logic is
correct but non-obvious (rules.py, sort key over (priority, -index)); D4
regex rules are case-sensitive despite the docstring promising
case-insensitive matching (rules.py); D5 `cli.py`'s `summary` command
duplicates `report.category_totals()`/`summarize()` instead of calling
them. D6 (found while building t14, not originally planted but genuinely
present): `cli.py`'s `categorize` command unconditionally overwrites each
transaction's category with whatever `categorize()` returns, including
`None` when no rule matches -- so re-running `categorize` on an already
(manually or CSV-column) categorized ledger silently erases any category
that doesn't happen to match a current rule.

| id | kind | measures | ground truth / notes |
|---|---|---|---|
| t13_recurring | tests (LARGE) | greenfield feature spanning a new module + persistence + two CLI surfaces: `ledgerlite/recurring.py` (`Recurrence`, `expand()` with exact weekly/monthly/yearly clamping rules and a specified negative-id scheme), persistence under a `"recurring"` store key, `recurring add`/`recurring list`, and `list --include-recurring --since/--until` merging expanded rows sorted by `(date, id)` | hidden: 20 cases -- weekly/monthly/yearly expansion incl. day-31 and leap-day clamping, `until` and range clipping, id determinism/uniqueness, CLI persistence + list formatting, and the merged-list `--include-recurring` behaviour incl. its exit-2 argument-validation path |
| t15_reports | tests (LARGE) | reporting feature spanning report.py/cli.py/store.py: `report.monthly_breakdown(txns, year)` (month -> category -> net, gap months omitted) and `report.trend(txns, category, months)` (zero-filled, anchored on the latest transaction overall), plus a `report` CLI command with exactly specified `--year [--format table\|csv]` and `--trend/--months` output shapes | hidden: 17 cases -- both functions (incl. a gap month and empty input), both CLI output formats byte-exact, the empty-year header-only case, and the mutually-exclusive/missing-argument exit-2 paths (a returned 2 or a `SystemExit(2)` from `parser.error` both count; the missing-`--months` error must name `months`) |
| t17_schema_migration | tests (XL) | a versioned-store-format upgrade with backwards compatibility: new `ledgerlite/schema.py` (`migrate_payload`, pure), a `source` field on `Transaction` defaulting differently for CSV import vs. a new manual `add` CLI command, transparent in-memory migration on `load()`, and an explicit `migrate` CLI command with an optional `--backup` | hidden: 23 cases -- pure-migration purity/idempotency, store round-trip and backward-compat load, `add`'s id-assignment and empty-store path, `migrate`'s already-current no-op (incl. ignoring `--backup`) vs. actual-migration-with-backup paths, and existing commands still working untouched against an old-format file |
| t18_ledger_layer | tests (XL) | behaviour-preserving refactor: extracting a `Ledger` service layer (`ledgerlite/ledger.py`: `load`/`save`/`import_csv`/`categorize_all`) out of `cli.py`'s duplicated `import`/`categorize` logic, required to preserve the existing (buggy) D6 category-clearing quirk exactly | hidden: 15 cases split evenly between the new `Ledger` API in isolation and byte-exact CLI-message/regression checks on `import`/`categorize`/`list`/`summary` |
| t19_goals_saga | tests (XL, longest) | a 7-phase savings/spending-goals feature in one prompt (model -> persistence+add/list -> progress -> milestones -> a sorted report -> an `--overall` total -> validation+`remove`), each phase specified with an exact CLI/format contract so phases can be scored independently | hidden: 26 cases, grouped by phase for partial credit -- `Goal`/`progress()` unit cases, add/list persistence and formatting, `goal progress` incl. unknown-name exit-2, milestone dedup/sort/boundary-exact reached-set, the sorted report incl. the empty-header-only case, `--overall`'s combined total incl. zero-goals, and add's positive-target/duplicate-name rejection plus `goal remove` |
| t20_audit_log | tests (XL) | a cross-cutting audit trail: new `ledgerlite/audit.py` (sequence-numbered, not wall-clock, entries under an `"audit"` store key), wired into every existing mutating command (`import`, `categorize`), plus a `store.save()` fix so it stops clobbering unrelated top-level keys across saves, and two new `audit log`/`audit summary` CLI commands | hidden: 15 cases -- append/read semantics and persistence-of-other-keys, CLI wiring on both mutating commands (and non-wiring on the read-only ones), cross-command sequence continuity, and both new commands' output/empty-state |
| t21_search | tests (XL) | an ambiguous-but-specified product request ("a way to search my transactions") pinned down by 12 explicit acceptance criteria in the prompt: a pure `report.search()` filter function (AND-combined payee/amount/category/date criteria) plus a `search` CLI command adding sort/limit/formatting on top | hidden: 20 cases -- the filter function's criteria individually and combined, and the CLI's mutual-exclusion and range-sanity exit-2 paths, three sort orders (incl. case-insensitive payee), `--limit`, the no-match message, and row-format parity with `list` |
| t22_envelopes | tests (EPIC) | a 10-phase envelope-budgeting subsystem in one prompt (model -> persistence+add/list -> status -> transfers -> a largest-remainder allocation helper -> overspend alerts -> a permanent rollover close-month checkpoint -> CSV export -> CSV import -> validation+remove), spanning 6 new/touched modules (`envelopes.py`, `allocation.py`, `envelope_store.py`, `envelope_cli.py`, plus `cli.py`/`__init__.py` wiring) | hidden: 41 cases, grouped by phase for partial credit -- the rollover balance walk incl. its `closed_through` restart and before-start/before-closed `ValueError`s, persistence/add/list, status incl. its two exit-2 paths, transfers incl. accumulation and same-name rejection, the allocation helper's exact-sum/tie-break/all-zero cases, alerts' sort-and-skip behaviour, close-month's fold+adjustment-pruning+idempotency, CSV export/import round-tripping (incl. import's replace-by-name state reset), and add's validation + remove |

## Jev-lane grid (t13, t15, t17-t22)

`t17_schema_migration` .. `t21_search` are XL: each is sized for roughly 10-30 minutes of
work by a strong model (t19_goals_saga, a 7-phase saga, is the longest of
the XL tasks at ~30+ minutes), spanning several files per task. `t22_envelopes`
is EPIC: 10 ordered phases, a reference diff of 512 lines across 6 files, and
41 hidden cases -- sized for 30-60+ minutes. Every one of t17-t22 is
`kind=tests` (t17-t21: 15-23 hidden cases; t22: 41) with `score =
passed/total`, ships a `reference.patch` (a verified, from-scratch solution
scoring 1.0 with `visible_ok: true`, proven against a pristine copy of
`template/` scoring 0 first), and -- like every other task -- is never
copied into the agent's own working copy of the repo by
`run.py`/`regrade.py` (only `template/` is copied into `<run>/repo`;
graders copy only `hidden/*.py`), so `reference.patch` can't leak into a
run.

Full Jev-lane grid, combining t13/t15 (LARGE), t17-t21 (XL), and t22
(EPIC) -- `--timeout-min 60` to give t22 enough room:

```
python run.py --tasks t13_recurring,t15_reports,t17_schema_migration,t18_ledger_layer,t19_goals_saga,t20_audit_log,t21_search,t22_envelopes \
  --conds vanilla,zirv,zirv-proxy --reps 3 --model sonnet --parallel 2 --stagger-s 90 --timeout-min 60 \
  --runs-subdir runs-large \
  --noninteractive --vanilla-plugin-dir <path to obra/superpowers plugin> --zirv-dir <path to zirv.exe under test>
python aggregate.py --runs runs-large --out report-large-sonnet.md
```

## t09 ground truth: groceries rows in examples/sample.csv

Computed by running `ledgerlite.rules.categorize` (with `DEFAULT_RULES`)
against every row's payee/memo in `examples/sample.csv` (48 data rows).
11 rows resolve to `groceries`:

1. `WHOLE FOODS MARKET #221` (row 1) -- matches `whole foods` (priority 8)
2. `TRADER JOES #58` (row 6) -- matches `trader joe` (priority 8)
3. `CENTRAL MARKET SEATTLE` (row 10) -- matches only the `MARKET` regex
   (priority 5); no substring rule applies
4. `WHOLE FOODS CAFE DOWNTOWN` (row 14) -- matches both `whole foods`
   (priority 8, groceries) and the `CAFE` regex (priority 4, dining);
   priority decides groceries
5. `SAFEWAY SUPERMARKET` (row 15) -- `MARKET` regex matches inside
   "SUPERMARKET" (priority 5)
6. `KROGER MARKETPLACE` (row 18) -- `MARKET` regex matches inside
   "MARKETPLACE" (priority 5)
7. `TRADER JOES #12` (row 27) -- matches `trader joe` (priority 8)
8. `AMAZON.COM MARKETPLACE` (row 28) -- `MARKET` regex matches inside
   "MARKETPLACE" (priority 5); a false-positive quirk of the rule set, not
   an actual grocery purchase, but it is what the code produces
9. `WHOLE FOODS MARKET #221` (row 35, second visit) -- matches `whole
   foods` (priority 8)
10. `TRADER JOES #58` (row 43, second visit) -- matches `trader joe`
    (priority 8)
11. `WINCO FOODS MARKET` (row 47) -- `MARKET` regex matches (priority 5)

Notably NOT counted, on purpose:

- `corner market` (row 12) -- lowercase, so it does **not** match the
  case-sensitive `MARKET` regex (D4); a naive human skim would likely
  count this as groceries too.
- `ALDI 2044` (row 19) and `COSTCO WHOLESALE` (row 17) -- no rule matches
  these payees at all (`DEFAULT_RULES` has no generic "grocery store"
  catch-all), so they are uncategorized, not groceries.

## t12 ground truth: dead functions in ledgerlite/

Computed with an AST walk over every `.py` file in `ledgerlite/` (tests/
excluded): collect every `def` (module-level function or method) as a
qualified name, then collect every `Name` load and `Attribute.attr` use
anywhere in `ledgerlite/*.py`. A def is "dead" iff its bare identifier
never appears as a `Name`/`Attribute` reference anywhere else in the
package. This is a literal has-any-caller check, not full
reachability-from-`main` analysis -- see the `category_totals` note below.

Script (run against the frozen `template/ledgerlite/`):

```python
import ast, os

pkg_dir = ".../template/ledgerlite"
files = {}
for fname in sorted(os.listdir(pkg_dir)):
    if fname.endswith(".py"):
        src = open(os.path.join(pkg_dir, fname), encoding="utf-8").read()
        files[fname] = (src, ast.parse(src, filename=fname))

defs = []
for fname, (src, tree) in files.items():
    class ClassCtx(ast.NodeVisitor):
        def __init__(self): self.stack = []
        def visit_ClassDef(self, node):
            self.stack.append(node.name); self.generic_visit(node); self.stack.pop()
        def visit_FunctionDef(self, node):
            cls = self.stack[-1] if self.stack else None
            qualname = f"{fname[:-3]}." + (f"{cls}.{node.name}" if cls else node.name)
            defs.append((qualname, node.name))
            self.generic_visit(node)
        visit_AsyncFunctionDef = visit_FunctionDef
    ClassCtx().visit(tree)

name_uses, attr_uses = {}, {}
for fname, (src, tree) in files.items():
    for node in ast.walk(tree):
        if isinstance(node, ast.Name):
            name_uses.setdefault(node.id, []).append(fname)
        if isinstance(node, ast.Attribute):
            attr_uses.setdefault(node.attr, []).append(fname)

for qualname, simple in defs:
    uses = name_uses.get(simple, []) + attr_uses.get(simple, [])
    if not uses:
        print("UNUSED:", qualname)
```

Output on the frozen template (19 functions/methods defined in total --
the package has no classes with methods, only dataclasses, so every def is
module-level):

```
UNUSED: report.monthly_totals
UNUSED: report.summarize
```

Ground truth: **`report.monthly_totals`** and **`report.summarize`**.

Note on `report.category_totals`: it has exactly one reference in the
whole package -- from inside `report.summarize()`, which is itself dead.
By the literal "has at least one caller anywhere in `ledgerlite/`" rule
used here (and specified by the task), `category_totals` therefore does
NOT count as dead, even though that one caller is unreachable from the
CLI. `report.page`, `rules.categorize`, `store.save`/`load`, and all of
`cli.py`'s `_cmd_*` handlers (referenced via the `_HANDLERS` dict, not a
direct call site) each have real references and are excluded correctly.

## t23_afternoon: the one long-session chain task

`t23_afternoon` is a different shape entirely (`kind=chain`, see the main
README's "Long-session chain" section): 9 sequential follow-up prompts sent
to the SAME agent session, simulating a developer's afternoon of merchant/
category-alias features built on the `ledgerlite` template. It measures
session management (rot scoring, compaction, memory across a long
conversation) that a single 1-3 minute prompt task never exercises, so it is
not part of the "Jev-lane grid" table above and is run separately -- see
README.md for the exact command.

| step | shape | hidden tests | reference score |
|---|---|---|---|
| 01 | greenfield: `aliases.py` + `alias add`/`list` | 7 | 1.0 |
| 02 | feature: `list --canonical` | 4 | 1.0 |
| 03 | bug report: case-insensitive alias matching | 5 | 1.0 |
| 04 | refactor: extract `alias_cli.py` (behaviour-preserving) | 5 | 1.0 |
| 05 | feature: `alias remove` | 2 | 1.0 |
| 06 | "like you did for X": `category-alias` + unconditional canonical `summary` | 5 | 1.0 |
| 07 | change of mind: gate step 6's default behind `--canonical-category` | 3 | 1.0 |
| 08 | feature: `report.top_merchants` + `merchants` CLI | 8 | 1.0 |
| 09 | wrap-up: README + final-reply summary (judge only, no hidden tests) | - | rubric-graded |

Every reference score above was verified by applying that step's cumulative
`reference/step_NN.patch` to a fresh, pristine copy of `template/` and
running that step's own hidden tests against it (score 1.0, `visible_ok`
true, the template's one known baseline-red visible test excepted); the same
hidden tests against an unpatched pristine copy score ~0.

## t24b_long_haul: corrected copy of t24_long_haul

`t24b_long_haul` is a byte-identical copy of `t24_long_haul` (a 22-step
`kind=chain` task) except for its step 09 and step 14 prompts, which state
two requirements their hidden tests already checked but the original
wording never named: step 09's `read_ofx` needs the same `start_id`
parameter `read_csv` already has, and step 14's `report.cashflow()` needs
its own `to_usd` keyword parameter (not just the CLI's `--usd` flag). Use
t24b in place of t24 for any run where those two steps should be
solvable from the prompt alone.

## t25_sticky_notes: autoresearch's context/handoff long-session task (issue #805)

`t25_sticky_notes` is a 6-step `kind=chain` task (a lightweight sticky-notes
feature added to `ledgerlite`, separate from the transaction ledger),
purpose-built for #805's context/compaction/handoff/review-reuse
candidates rather than as a general afternoon-of-work story like t23/t24:

| step | shape | tests the axis... | hidden tests | reference score |
|---|---|---|---|---|
| 01 | greenfield: `notes.py` + `note-add`/`note-list`, note ids permanently negative | -- (constraint origin) | 8 | 1.0 |
| 02 | feature: `--pinned` add/list filter + `summary` pinned count (multi-module: notes.py + cli.py); plants a deferred ordering bug | multi-module edit; unresolved-failure origin | 6 | 1.0 |
| 03 | bug report: `note-remove` | earlier-decision recall (ids still negative after a removal) | 6 | 1.0 |
| 04 | change of plan: cap note text at 200 chars | changed requirement mid-chain | 4 | 1.0 |
| 05 | "picking this back up after a handoff": fix the deferred ordering bug + `report.notes_summary()` (multi-module: report.py + notes.py + cli.py) | resume-after-handoff phrasing; unresolved-failure fixed; multi-module edit; recall regression check | 7 | 1.0 |
| 06 | wrap-up: `docs/NOTES.md` + final summary naming the deferred-bug-then-fixed arc | -- (judge only, no hidden tests) | - | rubric-graded |

Every hidden-test step's reference score was verified with `python run.py
--check-graders --tasks t25_sticky_notes`: each cumulative
`reference/step_NN.patch` applied to a fresh pristine template copy scores
1.0 on that step's own hidden tests (the template's one known baseline-red
visible test excepted), and an unpatched pristine copy scores well below
that on every step.

## t27_tax_season: ledger hygiene and tax-season long-session chain

`t27_tax_season` is a 21-step `kind=chain` task on the `ledgerlite` template,
built to the same shape as `t24b_long_haul` (about 15 minutes for a Sonnet
agent, context growing past ~150k tokens) but with a different story arc: getting
the ledger ready for tax season and data hygiene. It deliberately shares no
features with t24/t24b (currencies, OFX, recurring, budgets, cashflow,
reconciliation, undo, `commands/` split). Every requirement a hidden test
checks is stated in that step's prompt or an earlier one (the lesson of t24's
steps 9/14); hidden tests drive the CLI and only name modules/functions the
prompt names (`merchants.normalize_payee`, `errors.CliError`).

| step | shape | axis | hidden tests | reference score |
|---|---|---|---|---|
| 01 | greenfield: user categorisation rules (`rules add/list`), priority + longer-pattern tie-break; store keeps other top-level keys (multi-module: rulebook, store, cli) | multi-module edit; constraint origin (store never clobbers other keys) | 15 | 1.0 |
| 02 | feature: `merchants.normalize_payee`, `raw_payee`, `normalize` (multi-module) | multi-module edit | 11 | 1.0 |
| 03 | feature: `merge` with duplicate detection and `--window` (multi-module: dedupe, cli); plants the deferred bug | multi-module edit; deferred-bug origin | 9 | 1.0 |
| 04 | feature: `split` / `split-show`, integer-cent remainder rule (multi-module) | multi-module edit; decision origin (remainder rule) | 9 | 1.0 |
| 05 | feature: tax flags `tax mark/unmark/list` | -- | 8 | 1.0 |
| 06 | feature: `tax summary YEAR` (calendar year, per split part) | recall (split parts) | 7 | 1.0 (superseded from step 9) |
| 07 | feature: `doctor` with three checks | -- | 7 | 1.0 |
| 08 | feature: `receipt attach/show/verify` (path + SHA-256, no binaries; multi-module) | multi-module edit | 7 | 1.0 |
| 09 | scope trap: reorder only `tax summary` biggest-first; wording mentions `summary`/`rules list` | scope discipline (untouched orderings are tested) | 6 | 1.0 |
| 10 | bug report: `merge` skips both identical rows when the ledger holds one; match one-to-one | deferred bug surfaced | 8 | 1.0 |
| 11 | behaviour-preserving refactor: `errors.CliError`, one catch in `main()` | refactor with regression tests | 5 | 1.0 |
| 12 | feature: `amend ID --amount` re-divides a split "the way splitting did originally" | recall (step 4 remainder rule, not restated) | 6 | 1.0 |
| 13 | change of plan: equal-priority tie-break flips to newest rule wins | changed requirement (reverses step 1) | 6 | 1.0 |
| 14 | feature: `search` query language (`payee:`, `category:`, `amount>`, `year:`, `tax:`, bare words) | recall (clean payee, split-part categories) | 10 | 1.0 |
| 15 | "picking this back up after a handoff": `backup` / `restore` with `.sha256` integrity check | resume-after-handoff phrasing | 8 | 1.0 |
| 16 | change of plan: `tax summary` becomes April-March fiscal year, `--calendar` keeps the old | changed requirement (alters step 6) | 5 | 1.0 |
| 17 | feature: `doctor` `missing-receipt` check "reported exactly like the other checks" | recall (step 7 format, step 5 flag, step 8 receipts) | 5 | 1.0 |
| 18 | feature: `tax export` json/markdown fed by the same code as `tax summary` (multi-module) | recall (fiscal year, order, split parts); multi-module edit | 7 | 1.0 |
| 19 | "picking this back up after a handoff": `rules explain` | recall (step 13 tie-break, not restated); handoff phrasing | 6 | 1.0 |
| 20 | decision log `docs/design-notes/ledger-hygiene.md` (judge only) | recall of every decision incl. both reversals | - | rubric-graded |
| 21 | wrap-up: README + final-reply summary naming the `merge` bug and both changes of plan (judge only) | -- | - | rubric-graded |

Two earlier test files are deliberately superseded by a later change of plan
(`verify_reference.py`'s `SUPERSEDED` map): `step_01/test_step1_tiebreak.py`
from step 13, and `step_06/test_step6_tax_summary.py` from step 9 (ordering)
and 16 (fiscal year). Each chain step is graded on its OWN hidden tests only
(`grade_step_tests`), so these never conflict with the live run.

Every reference score above was verified two ways: `python
tasks/t27_tax_season/verify_reference.py` (FAIL-check against the previous
step's patch, PASS-check of every still-applicable earlier step against the
cumulative patch) and `python run.py --check-graders --tasks t27_tax_season`
(each cumulative `reference/step_NN.patch` on a fresh pristine copy scores 1.0
on that step's own hidden tests, the template's one known baseline-red
visible test excepted, and the unpatched pristine copy scores below 1.0).
The patches are CRLF to match the Windows checkout of `template/`, like t24b's.

## t26_household: a second long-haul chain, on a different story

`t26_household` is a 22-step `kind=chain` task of the same shape as
`t24b_long_haul` (sequential follow-ups to ONE agent session on the
`ledgerlite` template, roughly 15 minutes for a Sonnet agent, context past
~150k tokens) but with a story that shares nothing with t24's: **a household
sharing the ledger** (members, shared expenses in integer cents, transfers as
paired entries, settle-up, an append-only history with `undo`, a store-format
version bump with migration, archiving and renaming members). None of t24's
features (currencies, imports, recurring rules, pagination, budgets, cashflow,
reconciliation, the CLI split, OFX, undo of imports) are reused. It sits in the
`validation` split as its own group.

Hidden tests drive the CLI through `python -m ledgerlite ...` and only check
names, formats, messages and storage keys the step's prompt (or an earlier
prompt) states. Tests that a later change of plan makes obsolete carry
`superseded_by_NN` in their name; `verify_reference.py` expects exactly those
to fail from step NN on (and to pass before it), and `run.py` only ever grades
a step against its own tests, so graders are unaffected.

| step | shape | axis exercised | hidden tests | reference score | pristine score |
|---|---|---|---|---|---|
| 01 | greenfield: `household.py`, `member add/list`, `store.save` keeps other keys | multi-module (household/store/cli); constraint origin (error convention, storage layout) | 9 | 1.0 | 0.0 |
| 02 | feature: `expense add/list/show`, equal split in cents, leftover-cent rule | rule origin; **planted deferred bug** (amounts with >2 decimals are silently truncated) | 16 | 1.0 | 0.0 |
| 03 | feature: `--split NAME=W,...` weighted shares | recall: "same leftover rule", error conventions, not restated | 11 | 1.0 | 0.0 |
| 04 | feature: `balance` (sign convention) | decision origin (balance sign) | 7 | 1.0 | 0.0 |
| 05 | feature: `settle suggest` (greedy, deterministic ties) | recall: balance numbers reused | 9 | 1.0 | 0.0 |
| 06 | feature: `transfer add/list`, `account` (paired entries) | multi-module (household/balances/cli); recall of sign convention | 10 | 1.0 | 0.0 |
| 07 | feature: `report member` (`Net` must equal `balance`) | recall: balance convention incl. transfers | 6 | 1.0 | 0.0 |
| 08 | **change of plan**: leftover cents rotate by expense id | changed requirement; old expenses keep stored shares | 8 | 1.0 | 0.0 |
| 09 | feature: append-only `history` + `undo` | multi-module (retro-fitting 3 commands); standing rule "record future changes too" | 12 | 1.0 | 0.0 |
| 10 | feature: `expense edit` ("picking this back up after a handoff") | recall: weights, rotating rule, history rule not restated | 10 | 1.0 | 0.0 |
| 11 | **scope trap**: `expense list --payer` while the old CSV/paging/rules oddities tempt a cleanup | untouched behaviour must stay unchanged (including the old buggy `report.page`) | 9 | 1.0 | 0.556 |
| 12 | wrap-up checkpoint: `docs/household-decisions.md` (judge only) | accuracy of recorded decisions incl. the rule change | - | rubric-graded | - |
| 13 | **change of plan**: only `--settles` transfers move balances; old stored transfers count as settling | changed requirement across balance/suggest/report | 8 | 1.0 | 0.0 |
| 14 | **behaviour-preserving refactor**: `cli.py` -> `household_cli.py` (`register(sub)`) | refactor with regression net | 6 | 1.0 | 0.167 |
| 15 | **bug report** ("picking this up after a handoff"): reject >2 decimals everywhere | planted bug surfaces; find every amount entry point | 7 | 1.0 | 0.0 |
| 16 | feature: store format v2 + migration of v1 files, newer versions refused | multi-module (store/household/cli); read paths must not rewrite | 11 | 1.0 | 0.091 |
| 17 | feature: `member archive`, `list --all`, archived rules | recall: "everyone" now means active members, uniqueness, balance, history rule | 13 | 1.0 | 0.0 |
| 18 | feature: `report month YYYY-MM` | recall: expense dates, archived handling, member order | 10 | 1.0 | 0.0 |
| 19 | feature: `settle apply` (one history entry for the whole run) | recall: suggest order, `--settles`, history rule, archived members | 8 | 1.0 | 0.0 |
| 20 | feature: `member rename OLD NEW` everywhere | multi-module; recall: name rules, append-only history, v2 members | 10 | 1.0 | 0.0 |
| 21 | feature: read-only `check` (invariants over everything built so far) | recall: cent sums, paired entries, v1 files untouched | 11 | 1.0 | 0.0 |
| 22 | wrap-up: README + final-reply summary (judge only) | summary must name both changes of plan, the bug, the migration, the refactor | - | rubric-graded | - |

Reference scores were verified with `python verify_reference.py` (from this
task's directory; every cumulative `reference/step_NN.patch` applied to a fresh
pristine template copy passes the hidden tests of steps 1..N, modulo the
`superseded_by_NN` tests, and the visible suite stays green apart from the
template's one baseline-red test; the previous step's patch fails step N's own
tests) and `python run.py --check-graders --tasks t26_household` (pristine
scores in the last column). Steps 12 and 22 are judge-graded through
`rubric/step_NN.md`.
