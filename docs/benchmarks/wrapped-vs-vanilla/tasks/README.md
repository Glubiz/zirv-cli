# ledgerlite benchmark tasks

Twenty-two tasks against the `ledgerlite` template (see `../template/`, which
is FROZEN as of t01-t09 -- t10/t11/t12 were added afterward as harder
tasks, t13/t14/t15 after that as LARGE tasks (10-25 minutes, 30-80 tool
calls, several modules touched, for a strong model), t16-t21 after that
as XL tasks (10-30+ minutes, many edits across several modules each, for a
strong model), and t22 after that as the one EPIC task (10 ordered phases,
30-60+ minutes, 500+ reference-diff lines across 6+ modules -- see
"Large-task grid" below); all of t10-t22 only
target behaviour the frozen template already has, or spec new
features/questions with an exact hidden-test or scoring contract; they
never modify template/). The
template ships with one known-red visible test,
`tests/test_rules.py::test_regex_rule_case_insensitive`, caused by D4 below;
every grader that checks `visible_ok` treats that single failure as
baseline and does not penalize it except in t07 and t14, whose job includes
fixing it.

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
| t01_tiebreak | answer | whether the agent can explain categorize()'s priority + earliest-wins tie-break (D3) by reading the code | required regexes: mentions rules.py/categorize, "priorit*", and earliest/first/order |
| t02_pagination | tests | fixing D2 (report.page: page 1 skips row 0; an exact-multiple final page returns empty) without regressing normal/partial pages or the empty-list case | hidden: 7 cases; 3 fail on the template (page1, exact-multiple last page, single-exact-page), 4 already pass |
| t03_money | tests | fixing D1 (parse_money: parens should negate, not just strip; thousands separators should parse, not crash) end-to-end through read_csv | hidden: parse_money cases + one read_csv case combining both formats with a normal row |
| t04_budget | tests | greenfield feature: new `ledgerlite/budget.py` (`Budget`, `overspend`) + a `budget` CLI subcommand (`--set`, `--report`) persisted under a `budgets` key in the store JSON | hidden: overspend() unit cases + CLI set/report round trip + budgets persist alongside transactions |
| t05_dedupe | judge + visible | refactoring quality: removing D5's duplicated category-total/percent logic from `cli.py` in favor of `report.py`, with byte-identical output | rubric.md scores the diff directly; grade.py only reports visible_ok (baseline-tolerant) |
| t06_currency | tests | greenfield multi-file feature: `currency` field (default "DKK") on `Transaction`, store round-trip, optional CSV column, `report.totals_by_currency`, and `list` showing currency | hidden: model default/override, CSV column present/absent/blank, store round-trip, totals_by_currency, and a list-output check that reads the *second* row specifically to stay independent of the unrelated D2 pagination bug |
| t07_redtest | tests | root-causing and fixing D4 (missing `re.IGNORECASE` on regex rule matching) without touching the visible test file | hidden: the visible test's exact case + 2 more (uppercase pattern/lowercase payee, and a match via memo); grade.py forces score 0 if `tests/test_rules.py` is in `git diff --name-only HEAD` |
| t08_usage_doc | judge | technical writing: docs/USAGE.md covering all 4 CLI commands, their flags/defaults, and a working example each | rubric.md embeds the actual `_build_parser()` source as ground truth so the judge doesn't need the (unchanged) cli.py diff |
| t09_count | answer | careful code reading / running the categorizer over real data, including noticing where D4's case-sensitivity changes the result | ground truth: **11** (see breakdown below); required regex `\b11\b` |
| t10_shares | tests | greenfield spec compliance: `report.category_shares(txns)` implementing the largest-remainder method so percentage shares always sum to exactly 100.00, described behaviourally in the prompt (never says "largest remainder") | hidden: 7 cases -- two distinct naive-rounding-fails scenarios (99.99 and 100.01), a dedicated 2-category alphabetical tie-break, zero spending, single category, positive-only-category exclusion, and uncategorised grouping; all verified against a Fraction-exact reference implementation |
| t11_export | tests | greenfield CLI feature: `export --format csv\|json [--since] [--until]` with RFC-4180 CSV quoting, inclusive date bounds, (date, id) ordering, JSON string amounts + null category vs CSV blank category, empty-selection output, and an exit-code-2 error path | hidden: 9 cases covering quoting of a comma/quote/newline in memo, an unescaped unicode payee, inclusive bounds on both ends (with rows just outside excluded), empty selection in both formats, `--since` after `--until` exiting 2 with nothing on stdout, id-tiebreak ordering on a shared date, and JSON amount-is-a-string / null-category checks |
| t12_deadcode | answer | precise code reading: which functions in the FROZEN template's `ledgerlite/` package have zero callers anywhere in the package/CLI (tests excluded) | ground truth: **`monthly_totals`, `summarize`** (see script + output below); scored as (correct/2) minus 0.25 per hallucinated live-function claim, floored at 0 |
| t13_recurring | tests (LARGE) | greenfield feature spanning a new module + persistence + two CLI surfaces: `ledgerlite/recurring.py` (`Recurrence`, `expand()` with exact weekly/monthly/yearly clamping rules and a specified negative-id scheme), persistence under a `"recurring"` store key, `recurring add`/`recurring list`, and `list --include-recurring --since/--until` merging expanded rows sorted by `(date, id)` | hidden: 20 cases -- weekly/monthly/yearly expansion incl. day-31 and leap-day clamping, `until` and range clipping, id determinism/uniqueness, CLI persistence + list formatting, and the merged-list `--include-recurring` behaviour incl. its exit-2 argument-validation path |
| t14_bugsweep | tests (LARGE) | one multi-issue bug report combining D2 (pagination), D1 (money parsing), D4 (case-sensitive regex rules), and D6 (`categorize` erasing an existing category when no rule matches) -- all reported symptom-first in a single prompt, no file names | hidden: 20 cases -- the full t02/t03/t07 hidden suites (7+6+3) plus 4 new cases for D6; grade.py forces score 0 if `tests/test_rules.py` is touched, same as t07 |
| t15_reports | tests (LARGE) | reporting feature spanning report.py/cli.py/store.py: `report.monthly_breakdown(txns, year)` (month -> category -> net, gap months omitted) and `report.trend(txns, category, months)` (zero-filled, anchored on the latest transaction overall), plus a `report` CLI command with exactly specified `--year [--format table\|csv]` and `--trend/--months` output shapes | hidden: 17 cases -- both functions (incl. a gap month and empty input), both CLI output formats byte-exact, the empty-year header-only case, and the mutually-exclusive/missing-argument exit-2 paths |
| t16_tags | tests (XL) | greenfield feature spanning 5 files: `ledgerlite/tagging.py` (`normalize_tags`/`parse_tag_list`), a `tags` field on `Transaction` + store round-trip + optional CSV column, a `list` row-format suffix, a `tag add`/`tag remove`/`tag list` CLI family, and a `--tag` filter on `list` that ignores paging | hidden: 25 cases -- normalization edge cases, CSV column present/blank/absent, store backward-compat (missing `"tags"` key), list-output suffix incl. the D2-pagination-avoidance pattern, tag add/remove incl. unknown-id and all-blank-tags exit-2 paths, tag list counts, and the `--tag` filter incl. its no-match message |
| t17_schema_migration | tests (XL) | a versioned-store-format upgrade with backwards compatibility: new `ledgerlite/schema.py` (`migrate_payload`, pure), a `source` field on `Transaction` defaulting differently for CSV import vs. a new manual `add` CLI command, transparent in-memory migration on `load()`, and an explicit `migrate` CLI command with an optional `--backup` | hidden: 23 cases -- pure-migration purity/idempotency, store round-trip and backward-compat load, `add`'s id-assignment and empty-store path, `migrate`'s already-current no-op (incl. ignoring `--backup`) vs. actual-migration-with-backup paths, and existing commands still working untouched against an old-format file |
| t18_ledger_layer | tests (XL) | behaviour-preserving refactor: extracting a `Ledger` service layer (`ledgerlite/ledger.py`: `load`/`save`/`import_csv`/`categorize_all`) out of `cli.py`'s duplicated `import`/`categorize` logic, required to preserve the existing (buggy) D6 category-clearing quirk exactly | hidden: 15 cases split evenly between the new `Ledger` API in isolation and byte-exact CLI-message/regression checks on `import`/`categorize`/`list`/`summary` |
| t19_goals_saga | tests (XL, longest) | a 7-phase savings/spending-goals feature in one prompt (model -> persistence+add/list -> progress -> milestones -> a sorted report -> an `--overall` total -> validation+`remove`), each phase specified with an exact CLI/format contract so phases can be scored independently | hidden: 26 cases, grouped by phase for partial credit -- `Goal`/`progress()` unit cases, add/list persistence and formatting, `goal progress` incl. unknown-name exit-2, milestone dedup/sort/boundary-exact reached-set, the sorted report incl. the empty-header-only case, `--overall`'s combined total incl. zero-goals, and add's positive-target/duplicate-name rejection plus `goal remove` |
| t20_audit_log | tests (XL) | a cross-cutting audit trail: new `ledgerlite/audit.py` (sequence-numbered, not wall-clock, entries under an `"audit"` store key), wired into every existing mutating command (`import`, `categorize`), plus a `store.save()` fix so it stops clobbering unrelated top-level keys across saves, and two new `audit log`/`audit summary` CLI commands | hidden: 15 cases -- append/read semantics and persistence-of-other-keys, CLI wiring on both mutating commands (and non-wiring on the read-only ones), cross-command sequence continuity, and both new commands' output/empty-state |
| t21_search | tests (XL) | an ambiguous-but-specified product request ("a way to search my transactions") pinned down by 12 explicit acceptance criteria in the prompt: a pure `report.search()` filter function (AND-combined payee/amount/category/date criteria) plus a `search` CLI command adding sort/limit/formatting on top | hidden: 20 cases -- the filter function's criteria individually and combined, and the CLI's mutual-exclusion and range-sanity exit-2 paths, three sort orders (incl. case-insensitive payee), `--limit`, the no-match message, and row-format parity with `list` |
| t22_envelopes | tests (EPIC) | a 10-phase envelope-budgeting subsystem in one prompt (model -> persistence+add/list -> status -> transfers -> a largest-remainder allocation helper -> overspend alerts -> a permanent rollover close-month checkpoint -> CSV export -> CSV import -> validation+remove), spanning 6 new/touched modules (`envelopes.py`, `allocation.py`, `envelope_store.py`, `envelope_cli.py`, plus `cli.py`/`__init__.py` wiring) | hidden: 41 cases, grouped by phase for partial credit -- the rollover balance walk incl. its `closed_through` restart and before-start/before-closed `ValueError`s, persistence/add/list, status incl. its two exit-2 paths, transfers incl. accumulation and same-name rejection, the allocation helper's exact-sum/tie-break/all-zero cases, alerts' sort-and-skip behaviour, close-month's fold+adjustment-pruning+idempotency, CSV export/import round-tripping (incl. import's replace-by-name state reset), and add's validation + remove |

## Large-task grid (t13-t22)

`t16_tags` .. `t21_search` are XL: each is sized for roughly 10-30 minutes of
work by a strong model (t19_goals_saga, a 7-phase saga, is the longest of
the XL tasks at ~30+ minutes), spanning several files per task. `t22_envelopes`
is EPIC: 10 ordered phases, a reference diff of 512 lines across 6 files, and
41 hidden cases -- sized for 30-60+ minutes. Every one of t16-t22 is
`kind=tests` (t16-t21: 15-26 hidden cases; t22: 41) with `score =
passed/total`, ships a `reference.patch` (a verified, from-scratch solution
scoring 1.0 with `visible_ok: true`, proven against a pristine copy of
`template/` scoring 0 first), and -- like every other task -- is never
copied into the agent's own working copy of the repo by
`run.py`/`regrade.py` (only `template/` is copied into `<run>/repo`;
graders copy only `hidden/*.py`), so `reference.patch` can't leak into a
run.

Full large-task grid, combining t13-t15 (LARGE), t16-t21 (XL), and t22
(EPIC) -- `--timeout-min 60` to give t22 enough room:

```
python run.py --tasks t13_recurring,t14_bugsweep,t15_reports,t16_tags,t17_schema_migration,t18_ledger_layer,t19_goals_saga,t20_audit_log,t21_search,t22_envelopes \
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
not part of the "Large-task grid" table above and is run separately -- see
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
