# PR #712 verification — 2026-09-21

The review fixed the three open PR items (shell-variable mutation, file-bearing query bodies, and Claude scratchpad roots), plus query-route/argument spoofing and pipeline/redirect false positives found during review and replay.

## Checks

- `cargo build`: exit 0.
- `cargo fmt -- --check`: exit 0.
- `cargo clippy --all-targets -- -D warnings`: exit 0.
- Affected-module run (`cargo nextest run --no-fail-fast -E 'test(commands::ctx::safety::) | test(commands::ctx::hook::) | test(commands::ctx::adapters::)'`): 1,145 passed, exit 0. A sandboxed attempt had local socket permission errors; the completed run used the same unrestricted environment as the full suites below.
- Final full suite: `cargo nextest run --no-fail-fast`, exit 100.
- Baseline full suite on unmodified `main` (`ab8aa389`): identical command and unrestricted environment, exit 100.

PR: `Summary [ 187.220s] 7834 tests run: 7831 passed (1 slow), 3 failed, 4 skipped`

Main: `Summary [ 207.510s] 7814 tests run: 7811 passed (1 slow), 3 failed, 4 skipped`

Sorted failure-name comparison (not failure counts):

- New failures in PR: none.
- Baseline-only failures: none.

Final PR failure names:

- `commands::ctx::proxy::tests::jev_live_battery_matches_recorded_rulings`
- `commands::ctx::run_loop::tests::the_first_cycle_passes_the_pacing_gate_without_waiting`
- `commands::ctx::runtime::native::tests::proxy_disabled_leaves_the_native_session_unaffected`

### Final PR error text

```text
thread 'commands::ctx::run_loop::tests::the_first_cycle_passes_the_pacing_gate_without_waiting' panicked at src/commands/ctx/run_loop.rs:2801:9:
    usage headroom must never delay the first cycle's launch
    note: run with `RUST_BACKTRACE=1` environment variable to display a backtrace
```

```text
thread 'commands::ctx::runtime::native::tests::proxy_disabled_leaves_the_native_session_unaffected' panicked at src/commands/ctx/runtime/native.rs:8583:9:
    a disabled proxy must never persist a decision
    note: run with `RUST_BACKTRACE=1` environment variable to display a backtrace
```

```text
thread 'commands::ctx::proxy::tests::jev_live_battery_matches_recorded_rulings' panicked at src/commands/ctx/proxy/mod.rs:1476:9:
    4 instability(ies) and 1 mismatch(es) out of 24 cases:
    instability: bug-backtrace: workflow flipped between two identical calls: none then bugfix
    instability: ambiguous: execution flipped between two identical calls: direct then bounded
    instability: ambiguous: complexity flipped between two identical calls: trivial then bounded
    instability: ambiguous: seat_tier flipped between two identical calls: cheap then standard
    bug-backtrace: workflow expected bugfix got none
    note: run with `RUST_BACKTRACE=1` environment variable to display a backtrace
```

### Main baseline error text

```text
thread 'commands::ctx::run_loop::tests::the_first_cycle_passes_the_pacing_gate_without_waiting' panicked at src/commands/ctx/run_loop.rs:2801:9:
    usage headroom must never delay the first cycle's launch
    note: run with `RUST_BACKTRACE=1` environment variable to display a backtrace
```

```text
thread 'commands::ctx::runtime::native::tests::proxy_disabled_leaves_the_native_session_unaffected' panicked at src/commands/ctx/runtime/native.rs:8583:9:
    a disabled proxy must never persist a decision
    note: run with `RUST_BACKTRACE=1` environment variable to display a backtrace
```

```text
thread 'commands::ctx::proxy::tests::jev_live_battery_matches_recorded_rulings' panicked at src/commands/ctx/proxy/mod.rs:1476:9:
    1 instability(ies) and 3 mismatch(es) out of 24 cases:
    instability: bug-backtrace: workflow flipped between two identical calls: none then bugfix
    bump-timeout: execution expected direct got bounded
    bump-timeout: seat_tier expected cheap got standard
    bug-backtrace: workflow expected bugfix got none
    note: run with `RUST_BACKTRACE=1` environment variable to display a backtrace
```

The proxy-disabled assertion also fails on main with this operator's `proxy.enabled = true` configuration. The pacing assertion measures a one-second wall-clock bound; its failure varies with load. The Jev battery calls a live model and reports differing classifications across identical requests. These are retained as failures, not reported as a green full suite.

## Replay of today's decisions

At the initial capture, the daily safety log contained 312 ask/deny decisions. Matching their command hashes against local Claude transcripts recovered 197 commands. The rebuilt hook classified those commands in `auto` mode with their recorded sandbox-retry flags; it did not execute them. The replay explicitly restored the shipped headless `ask` and interactive `allow` defaults instead of using the operator's temporary headless-allow workaround, and supplied the observed already-suffixed Claude temp directory.

| Original reason | Now allow | Still ask/deny |
| --- | ---: | ---: |
| Unmatched default | 134 | 0 |
| Remote request | 7 | 5 |
| Sandbox retry | 9 | 33 |
| Credential or destructive operation | 0 | 9 |
| Total | 150 | 47 |

All seven inline Elasticsearch query examples clear. Remaining network cases include mutations and file/dynamic uploads. Remaining retry cases include unresolved shell state, scripts and credential-related execution. This is classifier replay coverage of the recovered sample, not a promise that every future prompt disappears. Raw session commands, authentication material and transcript contents are not committed.

## Sources checked

Curl option/body/URL semantics were checked against the [curl manual](https://curl.se/docs/manpage.html); Elasticsearch query semantics against the [Elasticsearch API documentation](https://www.elastic.co/docs/api/doc/elasticsearch/operation/operation-search-template). The durable Claude temp-directory observation is recorded in the repository memory bank.
