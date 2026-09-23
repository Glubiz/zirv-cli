# ledgerlite benchmark tasks

Fifteen tasks against the `ledgerlite` template (see `../template/`, which
is FROZEN as of t01-t09 -- t10/t11/t12 were added afterward as harder
tasks and t13/t14/t15 after that as LARGE tasks (10-25 minutes, 30-80 tool
calls, several modules touched, for a strong model); all of t10-t15 only
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
