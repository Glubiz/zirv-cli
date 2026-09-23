# Rubric: t08_usage_doc

The task asked the agent to write `docs/USAGE.md` documenting every CLI
command, its flags, and one example invocation per command. The diff you
are given contains only the new documentation file(s); judge it for
accuracy against the argparse definitions below (this is the ground truth
for what the CLI actually accepts -- do not assume any other flags or
commands exist), not against the diff of `cli.py` (which did not change).

Ground truth: `DEFAULT_STORE = "ledger.json"` and `ledgerlite/cli.py`'s
argument parser is built as follows:

```python
def _build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(
        prog="ledgerlite", description="A tiny personal-finance ledger."
    )
    sub = parser.add_subparsers(dest="command", required=True)

    p_import = sub.add_parser("import", help="import transactions from a CSV file")
    p_import.add_argument("csv_path", help="path to a bank-export CSV file")
    p_import.add_argument(
        "--store", default=DEFAULT_STORE, help="ledger JSON file to write (default: %(default)s)"
    )

    p_list = sub.add_parser("list", help="list stored transactions")
    p_list.add_argument(
        "--store", default=DEFAULT_STORE, help="ledger JSON file to read (default: %(default)s)"
    )
    p_list.add_argument("--page", type=int, default=1, help="page number, 1-indexed (default: %(default)s)")
    p_list.add_argument(
        "--page-size", type=int, default=10, dest="page_size", help="rows per page (default: %(default)s)"
    )

    p_summary = sub.add_parser("summary", help="show per-category totals")
    p_summary.add_argument(
        "--store", default=DEFAULT_STORE, help="ledger JSON file to read (default: %(default)s)"
    )

    p_categorize = sub.add_parser(
        "categorize", help="(re)apply the default rules to stored transactions"
    )
    p_categorize.add_argument(
        "--store", default=DEFAULT_STORE, help="ledger JSON file to update (default: %(default)s)"
    )

    return parser
```

So the CLI has exactly four commands, invoked as `python -m ledgerlite <command> ...`:

- `import <csv_path> [--store PATH]` (`csv_path` is positional/required; `--store` defaults to `ledger.json`)
- `list [--store PATH] [--page N] [--page-size N]` (`--page` defaults to `1`, `--page-size` defaults to `10`)
- `summary [--store PATH]`
- `categorize [--store PATH]`

Score 0-10. Start from 10 and deduct:

- **-10 (score 0)**: a command is missing entirely, or a documented command
  does not exist in the ground truth above (hallucinated command/flag).
- **-6 per command with a wrong or missing flag**: e.g. wrong default
  value, a flag listed as required when it is optional (or vice versa), a
  missing flag (such as `--page-size` under `list`), or an extra flag that
  does not exist.
- **-3 per command missing a working example invocation**: each of the
  four commands must have at least one concrete example command line that
  is syntactically valid given the flags above.
- **-3 per example that would not actually work**: e.g. an example that
  uses a flag or argument that does not exist, or omits a required
  positional argument.
- **-2, poor concision**: excessive repetition, filler, or restating
  argparse help text verbatim in a way that adds no value beyond the
  grounds already given here.
- **-2, missing defaults**: does not mention default values for `--store`,
  `--page`, or `--page-size`.

A score of 10 requires all four commands documented, each with its exact
flags (including defaults) and a correct, runnable example.

Answer with only a JSON object: `{"score": 0-10, "reasoning": "..."}`.
