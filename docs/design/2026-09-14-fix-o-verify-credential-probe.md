# Chunk O: verify --check filtering, a Windows credential fix, and an optional-credential probe (#640, #642, #643)

**Date:** 2026-09-14 · **Issues:** #640, #642, #643

## Context

The 4.0.0 release-build feature test on `release/native-harness` @ 59e9b77a
found three independent bugs plus two release-test follow-ups: `zirv verify
--check` did nothing on the `--builtin` path, `store:` credential refs never
worked on Windows, and a live `provider check`/`doctor` skipped the network
probe for a keyless `openai-compatible` route whose vendor profile marks the
credential optional (`vllm`). This note records the fix for each, test-first.

## Decision

### `zirv verify --builtin --check <id>` (#640)

`run_verify` (`src/commands/workflow/verification.rs`) ran `checks::run_all`
unconditionally on the `--builtin` path and printed every result, ignoring
`args.run.checks` and `args.run.dry_run` entirely -- an unknown id like
`ZCHK-DOES-NOT-EXIST` silently ran (and passed) the whole registry. The
`--builtin` branch now resolves the filter first, via a new
`resolve_builtin_check_ids`: empty means every id; a case-insensitive exact
match against `checks::ALL_IDS` narrows to those ids; anything unmatched is a
hard `Err` naming every known id, before a single check runs. `--dry-run`
returns a new `write_builtin_dry_run` listing (JSON array or `id\twould-run`
lines) and returns without calling `checks::run_all` at all -- a preview,
never evidence, mirroring `CheckStatus::DryRun`'s existing contract on the
repo-supplied path. Otherwise the (still unfiltered) `run_all` output is
retained-and-filtered to the selected ids before the pass/fail verdict and
report are computed, so `builtin_checks_exclude` and the filter compose
correctly.

### Windows `store:` credential refs (#642)

`windows_get_command`/`windows_set_command`
(`src/commands/ctx/provider/credential.rs`) appended the store item id as a
positional argv entry after `-Command <script>`. PowerShell's `-Command`
(a bare string, not a `{ scriptblock }`) never populates `$args` from
trailing argv -- it appends every remaining entry to the script text, so the
script failed to parse (`Unexpected token 'test-item' in expression or
statement`) and every `store:` ref was non-functional on Windows. Fixed by
adding `CommandSpec::envs: Vec<(String, String)>`, applied by `ProcessRunner`
via `Command::envs`; the Windows builders now set `ZIRV_CRED_ITEM` there
instead of a positional arg, and `WINDOWS_READ_SCRIPT`/`WINDOWS_WRITE_SCRIPT`
read `$env:ZIRV_CRED_ITEM`. The secret keeps going over stdin, unchanged.
`Command`'s default behaviour (inherit the parent's environment unless
`env_clear` is called, which `ProcessRunner` never does) is what already
makes the store dir honour a `LOCALAPPDATA` override -- both scripts read
`$env:LOCALAPPDATA` directly, with no Rust-side resolution to fix.

A second, independent bug surfaced only once the argv fix let the scripts
actually reach their DPAPI call: a bare `-NoProfile -NonInteractive`
PowerShell host does not have `System.Security.dll` loaded, so
`[Security.Cryptography.ProtectedData]` raised "Unable to find type" --
verified directly on this machine (Windows 11 Pro, PowerShell 5.1, both the
WOW64 and native `System32` hosts). Both scripts now start with
`Add-Type -AssemblyName System.Security;`.

### Optional-credential routes now probe (#643)

`Inventory::build` (`src/commands/ctx/provider/inventory.rs`) treated a
missing credential as a hard `Err` that skipped the probe entirely --
correct for a route that genuinely requires one, wrong for
`CredentialClass::LocalOptional` (e.g. `vllm`, which declares
`credential_env: &["VLLM_API_KEY"]` but does not require it). The `Err` arm
now has a guard: when `profile.credential.is_optional()`, the missing-
credential message is pushed to `report.problems` as an advisory and the
route proceeds to `apply_probe` with `credential = None`, exactly like a
credential-less (`LocalNone`, e.g. `ollama`) profile already does -- reaching
`Reachable` on a real connection attempt instead of stopping at `Configured`
with a 0s "probe".

### README one-liners

- `objective close` refuses (exit 1) without a fresh, passing final
  verification -- documented next to the existing worked example
  (`src/commands/ctx/objective.rs::run_close`, `--help` on
  `ObjectiveVerb::Close`).
- `ZIRV_CTX_PACE` (`pace.enabled`) was checked against
  `config.rs::REPO_FORBIDDEN` and is **not** present there: `pace.enabled` is
  repo-narrowable (`narrow_pace_bool`), not repo-forbidden, so it does not
  belong in the "Forbidden repo key | Set instead via" table alongside
  `ZIRV_CTX_MAIL`/`ZIRV_CTX_PROMPT` -- adding it there would misstate the
  trust boundary. No README change made for it; see the report for the full
  reasoning.
- The "native spend aggregates by pool" sentence conflated two subsystems:
  `spend.rs`'s `DelegationRow`/`SpendRow` have no `pool` field or dimension
  at all (`--by` is `harness|model|task-class|worker`, grouped by the
  model's own id, not by account/pool); only the usage windows
  (`zirv ctx usage`, `pace.rs`/`pool.rs`) aggregate per pool. The paragraph
  now attributes pool aggregation to the usage windows only and states
  `zirv ctx spend --by`'s real dimensions.

### `surface.rs` release-mode test gate

`into_nested_and_into_local_private_refuse_a_global_surface` asserts a
`debug_assert!` panics, so it fails under `cargo nextest run --release`
(`[profile.release]` in `Cargo.toml` sets `debug-assertions = false`). Gated
with `#[cfg(debug_assertions)]` and a one-line comment.

## What is verified

- `builtin_check_filter_runs_only_the_named_check`,
  `builtin_check_filter_rejects_an_unknown_id`,
  `builtin_dry_run_lists_without_executing` (`verification.rs`): filtering to
  one id, an unknown id erroring with every known id named, and dry-run
  listing without executing.
- `os_store_builders_keep_platform_specific_secret_handling`
  (`credential.rs`, extended): no positional argv entry follows `-Command`,
  and `envs` carries `ZIRV_CRED_ITEM`.
- End-to-end on this machine, worktree debug binary, `LOCALAPPDATA`/`HOME`/
  `USERPROFILE` pointed at a scratch dir and a scratch `~/.zirv/native.toml`
  (`vllm` vendor, `credential = "store:test-item"`): `echo s | zirv ctx
  provider credential set work` exits 0 and writes
  `<scratch>/zirv/native-credentials/test-item.dpapi`;
  `zirv ctx provider check` then reports the route `credentialed`, and
  `--live` reaches the network probe (`endpoint unreachable: connection
  refused` against the unreachable loopback fixture, i.e. it actually tried).
- `optional_credential_profile_still_probes_with_no_credential_configured`
  (`inventory.rs`): a `vllm`-vendor route with no configured credential
  reaches `Reachable` on a live probe (not `Configured`), with the missing
  credential kept as an advisory `problems` entry.
- `builtin_self_checks_do_not_fail_verify_in_a_repository_that_is_not_zirv`
  and the full `workflow::`/`ctx::surface::`/`ctx::provider::` nextest
  filters still pass; `cargo run -q -- verify --builtin` prints all 10
  checks; `cargo run -q -- verify --builtin --check ZCHK-DOC-EXIT-CODES`
  prints only that one.

## What is deferred

- No fix for the `Add-Type -AssemblyName System.Security` gap was filed as
  its own issue; it is folded into #642 since the argv fix is what exposed
  it end-to-end on this machine.
- `--check` validation was added only to the `--builtin` path; the
  non-`--builtin` path already hard-errors on an unmatched id via
  `run_mode`'s existing "no verification checks matched the requested
  selection", which does not silently pass, so it was left unchanged.
