# ledgerlite

A small, dependency-free personal-finance ledger for tracking bank
transactions imported from CSV exports.

Features:

- Import a bank CSV (ISO or `DD-MM-YYYY` dates, money amounts with an
  optional `$` prefix) into a local JSON ledger file.
- Auto-categorize transactions with a small rule engine (substring or
  regex patterns, priority-ordered).
- List, paginate, and summarize transactions from the command line.

## Usage

```
python -m ledgerlite import statement.csv --store ledger.json
python -m ledgerlite list --store ledger.json --page 1 --page-size 20
python -m ledgerlite summary --store ledger.json
python -m ledgerlite categorize --store ledger.json
```

`examples/sample.csv` holds a small illustrative export for experimenting
with the tool.

## Running the tests

```
python -m unittest discover -s tests
```
