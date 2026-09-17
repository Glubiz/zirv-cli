# Implementation plan

Branch `feat/harness-proxy` off `main` @f1f618a2. Sequential T1, then T2 and
T3 in parallel (disjoint files), then gates, one review round, one fix round
at most, PR. Workers are native Claude subagents (sonnet); this seat
integrates and commits. Design of record: `spec.md` in this directory.

## Ordered tasks

- [x] T1: Core proxy module, configuration, catalogue vendor, helper role, verb
  - Files: `src/commands/ctx/proxy/{mod.rs, decision.rs, typesafe.rs, llm.rs,
    native.rs}` (new); `src/commands/ctx/mod.rs` (`pub mod proxy`, `CtxVerb::Proxy`);
    `src/commands/ctx/config.rs` (`ProxyConfig`, `TypesafeConfig`, defaults,
    `REPO_FORBIDDEN` rows with env pairs, `ZIRV_CTX_PROXY_*` overrides);
    `src/commands/ctx/catalogue.rs` (vendor `typesafe`, rung `jev` /
    `jev-latest`, input 42_000 µUSD per MTok, output 0, tier Cheap);
    `src/commands/ctx/helper.rs` (`ROLE_PROXY`); `tests/fixtures/proxy/
    {jev-response.json, battery.json}` (new); `tests/fixtures/fake-model.sh`
    (modes `proxy`, `proxy_garbage`).
  - Public surface T2 depends on: `proxy::decide(cfg, state_dir, repo, request)
    -> ProxyDecision`; `proxy::announce_line(&ProxyDecision) -> String`;
    `proxy::prompt_layer(&ProxyDecision) -> String` (≤ 6 lines);
    `proxy::persist(state_dir, &ProxyDecision) -> CtxResult<()>` (also writes
    the `log::Delegation` spend row when `usage` is present);
    `proxy::latest_for_repo(state_dir, repo) -> Option<ProxyDecision>`;
    `proxy::read_request(&mut impl BufRead) -> Option<String>`;
    `proxy::native::route_for_decision(&ProxyDecision, &NativeConfig) ->
    Option<RouteId>`; `ProxyDecision` fields as listed in the spec, all
    `Serialize + Deserialize`.
  - Verify: `cargo build`; `cargo nextest run proxy:: config:: catalogue::
    helper::`; `cargo clippy --all-targets -- -D warnings`; `cargo fmt -- --check`;
    `./target/debug/zirv ctx proxy --json "fix the typo in README"` prints a
    `deterministic` decision when `TYPESAFE_API_KEY` is unset.

- [x] T2: Launch wiring — wrapped harness, native gate, status
  - Files: `src/commands/ctx/chat.rs` (`--proxy` / `--no-proxy`, intake before
    `resolve_adapter`, decided model in `extra_with_model`, request as
    `initial_prompt`, `engine::start_workflow` immediately before spawn with
    close-on-spawn-failure, proxy layer into the compiled context, skipped with
    reason under `--resume` / `--simple`); `src/commands/ctx/status.rs`
    (`proxy:` line); `src/commands/ctx/runtime/native.rs` (one guarded call in
    `spawn_interactive`'s submit loop on the first turn: decide → workflow
    start → `route_for_decision` override or recorded fallback);
    `src/commands/ctx/compile.rs` or `prompt.rs` only if a new layer hook is
    required. No change to `runtime/mod.rs`.
  - Verify: `cargo build`; `cargo nextest run chat:: status:: runtime::
    proxy::`; `cargo clippy --all-targets -- -D warnings`; `cargo fmt -- --check`;
    `./target/debug/zirv chat --runtime native` still prints the coming-soon
    refusal; `./target/debug/zirv ctx status | grep '^proxy:'`.

- [x] T3: Documentation, command schema, version
  - Files: `README.md` ("### Harness proxy" beside the `zirv chat` section;
    `[proxy]` in the configuration example and the trust-boundary table;
    `zirv ctx proxy` in the verbs table; `typesafe` row in the model catalogue
    table); `src/commands/command_schema.rs` / `src/commands/help.rs` only
    where an existing test enumerates verbs; `Cargo.toml` + `Cargo.lock`
    (minor bump; T3 is the sole manifest editor).
  - Verify: `cargo nextest run command_schema:: help::`; `cargo fmt -- --check`;
    `git diff --stat` shows only the listed files.

- [x] T4: Gates and baseline diff (this seat)
  - Files: none.
  - Verify: `cargo build`; `cargo nextest run --no-fail-fast`; `cargo fmt --
    check`; `cargo clippy --all-targets -- -D warnings`; sorted failure-name
    list diffed against the same-environment `main` run captured before T1
    (scratchpad `baseline-main-nextest.log`); commit the work products and the
    implementation.

- [x] T5: Independent review and one fix round
  - Files: as findings require; fixes go through a worker.
  - Verify: one sonnet review of `git diff main...feat/harness-proxy` reporting
    confirmed, concrete findings; fix only confirmed defects; re-review only the
    touched hunks; stop when a round yields nothing new, hard stop after two
    fix rounds; residuals reported in the PR.

- [x] T6: Pull request
  - Files: none.
  - Verify: `gh pr create` on `Glubiz/zirv-cli` with a short title and body,
    assignee set, linking issue #537 as the profile seam this implements in
    part; the four gates re-run green on the final commit.

## Execution ledger

| Task | Started | Finished | Evidence |
| --- | --- | --- | --- |
| T1 | 2026-09-17 05:58Z | 2026-09-17 07:04Z | core module `src/commands/ctx/proxy/{mod,decision,typesafe,llm,native}.rs`, `[proxy]` config + REPO_FORBIDDEN, `typesafe` catalogue vendor, `ROLE_PROXY`, `zirv ctx proxy`; 45 tests; addenda: risk≥High⇒Bounded floor, session id on the spend row, review fixes (validation OR-merge, helper deadline, config bounds), text-only intake baseline, security-text⇒risk High floor |
| T2 | 2026-09-17 06:19Z | 2026-09-17 07:58Z | T2a `chat.rs` intake/apply, `proxy/launch.rs`, `--proxy/--no-proxy`, proxy layer hook (`prompt.rs`, `compile.rs`, `wrap.rs`), `Event::ProxyAdvisory`, `chat_via_runtime` wrapped, `engine::close_unstarted`; T2b `status.rs` proxy line, guarded first-turn branch in `runtime/native.rs`; `runtime/mod.rs` untouched |
| T3 | 2026-09-17 06:19Z | 2026-09-17 06:40Z | README "Harness proxy" section + activation rule, verbs/config/trust-boundary/catalogue rows; Cargo 4.4.0→4.5.0 |
| T4 | 2026-09-17 07:35Z | 2026-09-17 08:43Z | three full gate runs on the branch; final (f5bfccef): build/fmt/clippy exit 0, nextest 7430/7432 passed; failures by name vs the main baseline: `run_loop::…pacing_gate` (pre-existing) and one flaky fake-server test fixed afterwards in `typesafe.rs` tests only; two new doc-check rows (`native-runtime-inventory.md`, `native-parity.md`) and a `LegacyOnly` profile row for `typesafe` |
| T5 | 2026-09-17 07:36Z | 2026-09-17 08:50Z | review round 1 (two sonnet reviewers, disjoint halves): 5 confirmed findings (validation flags lowered on merge; helper 2× timeout; config bounds; test gap; orphaned AwaitingApproval workflow on failed spawn) all fixed; re-review of round 1: OK; re-review of round 2 (text-only baseline, security floor, parity rows): OK; residuals: string-matched engine refusal guarded by a test, persistent-runtime path has no wire field for the proxy layer |
| T6 | 2026-09-17 | 2026-09-17 | PR from `feat/harness-proxy` on Glubiz/zirv-cli linking #537; release 4.5.0 built from the branch and installed over the Homebrew binary for operator testing |
