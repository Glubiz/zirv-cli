## Memory
- Key: jev-metadata-complexity-unstable
- Written-by: claude
- Written: 1791197600
- Verified: 1791197600
- Source: explicit

Verified 2026-10-05 with jev-1.13.0, 3 live passes over tests/fixtures/proxy/jev-battery.json plus 4 trivial prompts. A metadata-only complexity Score question (26 int facts) is too unstable to ship. Jev puts 0.56-0.83 on 'trivial' for every case, and its confidence FALLS as complexity rises: trivial cases score 0.51-0.75, larger ones 0.22-0.55. A 0.5 floor therefore drops exactly the answers that raise complexity. At a 0.2 floor, agreement rose from the baseline's 6/24 to 14-15/24. But trivial and bounded flip between calls on a rounded mean near 0.5: 'fix typo in README' went bounded in one pass, and 0/5 substantial or architectural cases matched. The change was reverted in the 4.52.0 work. Do not retry without richer facts and a stability check across at least 3 passes.
