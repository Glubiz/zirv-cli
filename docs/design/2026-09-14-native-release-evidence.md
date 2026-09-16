# Native runtime: evidence, limitations and the release decision (N23)

**Date:** 2026-09-14 · **Issue:** #492 · **Roadmap:** #469

This is the evidence half of the last roadmap step. The parity record itself
is [`native-parity.md`](native-parity.md), which is machine-checked on every
`zirv verify --builtin` run. This document says what that record *is worth*:
which claims are backed by which test or CI job, which are not backed at all,
what an operator has to do to close the gaps, and what the release decision
actually is.

Two rules govern every sentence below, and both are inherited rather than
invented here:

- **No unmeasured cost or performance claim.** A number appears only if
  `docs/benchmarks/` contains a recorded observation of it. That is
  `docs/benchmarks/native-runtime-baseline.md`'s own non-negotiable rule and
  `docs/benchmarks/token-cost.md`'s before it.
- **No rung a row has not earned.** `live-validated` means a recorded run
  against a real endpoint. Nothing in this release claims it.

## 1. Support matrix

Rungs are the ones [`native-parity.md`](native-parity.md) defines. "Route" is
a `RouteProfile` id from `src/commands/ctx/provider/profiles.rs`; the check
that keeps these two lists from drifting is the profile table's own
`Support` field, which `zirv ctx provider list` prints.

### Direct provider transports

| Route profile | Protocol | OS | Rung | Evidence |
|---|---|---|---|---|
| `anthropic-messages` | Anthropic Messages | Linux, macOS, Windows | `integration` | `anthropic::tests` (stream reassembly, retry-after, mid-stream overload, first-event deadline, in-flight cancellation) |
| `openai-responses` | OpenAI Responses | Linux, macOS, Windows | `integration` | `openai::tests` (stream reassembly, truncation, incomplete responses, rate-limit retry-after) |
| `google-developer`, `google-vertex` | Google Generative AI / Vertex | Linux, macOS, Windows | `integration` | `google::tests` (thought signatures, parallel calls, safety refusal, quota classification) |
| `azure-openai-chat`, `deepseek-chat`, `xai-chat`, `qwen-dashscope-chat`, `moonshot-chat`, `mistral-chat`, `zhipu-chat`, `minimax-chat`, `meta-llama-chat`, `ollama-openai`, `lmstudio-openai`, `vllm-openai`, `openai-chat-generic` | OpenAI chat-completions | Linux, macOS, Windows | `integration` | `openai_chat::tests` (one transport, every compatible vendor and local runtime) |
| `aws-bedrock-anthropic`, `aws-bedrock-converse` | Bedrock Converse (SigV4) | Linux, macOS, Windows | `integration` | `bedrock::tests` (event-stream frames, signed reasoning, max-token stop) |
| `copilot-broker` | -- | -- | `legacy-only` | `Support::LegacyOnly` in `profiles.rs`: a Copilot subscription is brokered through the Copilot editor/CLI identity and has no documented direct API a third-party client may authenticate against |
| `factory-droid-broker` | -- | -- | `legacy-only` | `Support::LegacyOnly` in `profiles.rs`: Factory/Droid resells upstream models under its own subscription identity, with no documented direct API |

Every row above is **protocol** evidence, not **live integration** evidence:
the transport is driven against a fixture that replays real wire shapes, not
against the vendor's endpoint. See §3.

### Runtime surfaces, per OS

| Surface | Linux | macOS | Windows | Evidence |
|---|---|---|---|---|
| Setup, provider template, inventory, doctor, capabilities with **no coding harness on PATH** | yes | yes | yes | `Native Install` job, `Native Setup And Doctor With No Harness Installed` (the job asserts the absence rather than assuming it) |
| Config migrate forward / downgrade round trip | yes | yes | yes | `Native Install` job, `Config Migration Round Trip` + `Verify Config Migration Contracts` |
| Every helper role (distiller, ask, optimize, seat) with an empty `PATH` | yes | yes | yes | `Native Install` job, `Verify Every Helper Path Runs Without A Harness`; `helper::tests::every_helper_role_answers_with_every_coding_harness_removed_from_path` |
| A whole coordinating team with every coding harness absent | yes | yes | yes | same job/step; `tools::tests::an_all_native_team_runs_a_workflow_with_every_coding_harness_absent` |
| A REAL native worker and every helper role, with a CANARY executable standing in for every registered harness name (not just `claude`/`codex`) | yes | yes | yes | `Native Install` job, `Verify The Harness-Free Install Proof`; `tools::tests::a_real_native_worker_and_every_helper_role_complete_with_every_registered_harness_canaried_and_uninvoked` (issue #609, roadmap N22) |
| Mixed board (wrapped + native) and return to the harness default | yes | yes | yes | `Native Install` job, `Verify Mixed Runtime And Fault Invariants`; `runtime::tests::a_mixed_board_exchanges_mail_and_survives_a_return_to_the_harness_default` |
| Fault invariants (seat fence, approval channel, queued mail, every rollover direction) | yes | yes | yes | same step; see [`native-parity.md`](native-parity.md) "The four invariants" |
| Native coding tools (closed registry, file tools, bounded output, process lifecycle) | yes | yes | yes | `Native Tools` job |
| Execution enforcement / broker contract | yes | yes | yes | `Native Enforcement` job |
| Verified process isolation for a sandboxed invocation | yes | yes | **no** | `runtime::enforcement::PlatformIsolation::detect`; on Windows the broker REFUSES a sandboxed invocation rather than running it unconfined (`enforcement::tests::unavailable_process_isolation_never_falls_back_to_a_plain_spawn`, which hand-feeds an `Unavailable` value, and `enforcement::tests::a_process_action_is_refused_by_this_machines_own_real_platform_isolation_detection`, added by issue #610, which calls `detect()` for real on this machine and pins the exact Windows verdict). Tracked as an implementation gap, N04 (#473) |
| Interactive PTY supervision of a native session | n/a | n/a | n/a | there is no vendor TUI under a native seat; `zirv chat --runtime native` is the interactive surface |

## 2. What is verified, and by what

Grouped by the acceptance criterion it serves. Every name here exists in the
tree -- `ZCHK-NATIVE-PARITY` fails the build if one stops existing.

- **Nothing hidden behind a harness dependency.** 158 capabilities (121 clap
  verbs, 37 model-calling call sites) each carry a parity row;
  `checks::parity::tests::the_real_repo_parity_matrix_passes` runs the check
  against this repository on every test run, and `Verify Native Parity
  Matrix` runs it on all three OSes.
- **An all-native team completes real work.**
  `tools::tests::an_all_native_team_runs_a_workflow_with_every_coding_harness_absent`
  drives a coordinator, a group, shared cards, delegation and the workflow
  tools through the real registry and broker with no harness present;
  `helper::tests::every_helper_role_answers_with_every_coding_harness_removed_from_path`
  covers the distiller, ask, optimize and seat helper roles with an empty
  `PATH`; `review::tests::a_native_reviewer_argv_pins_read_only_with_no_harness_flags`
  covers the independent reviewer. Read the helper test precisely: it proves
  that every helper role plumbs through session construction, the loop and
  answer extraction with `PATH` emptied, so none of them shells out to a
  vendor CLI on that path. It supplies the provider as a fixture transport,
  which *bypasses route resolution* — so it is not, by itself, evidence that
  route resolution never consults `PATH`. What covers that is
  `provider::inventory`'s own tests together with the `Native Install` job,
  which runs `zirv ctx provider list` and `zirv ctx doctor --json` on a
  machine the job has already asserted has no coding harness installed.
  Issue #609 (roadmap N22) closed the remaining gap in the team test above:
  the coordinator's own delegate tool call cannot be driven through a real
  dispatched worker deterministically (`HeadlessRequest::provider`, the
  operator-only fixture override, is deliberately never threaded onto a
  model-facing `LaunchRequest`), so `an_all_native_team_runs_a_workflow_with_
  every_coding_harness_absent` still substitutes `RecordingLauncher` for that
  one seam. `tools::tests::a_real_native_worker_and_every_helper_role_
  complete_with_every_registered_harness_canaried_and_uninvoked` closes it
  from the other side: it drives the REAL production worker entry point
  (`runtime::native::run_session`) and every helper role to completion, with
  a CANARY executable -- not an empty `PATH` -- standing in for every name
  `ctx::adapters::ADAPTERS` registers, and asserts none of the eight ever
  ran. `Verify The Harness-Free Install Proof` runs it on all three OSes.
- **Mixed runtime and the way back.**
  `tools::tests::a_native_coordinator_runs_a_mixed_team_through_the_shared_services`
  (one board, both runtimes, one graph) and
  `runtime::tests::a_mixed_board_exchanges_mail_and_survives_a_return_to_the_harness_default`
  (mail both ways, then the default returned to the harness with every seat
  record, conversation reference and unread message intact).
- **Original harness-session resumption.**
  `sessions::tests::native_conversation_does_not_answer_for_a_different_runtime`
  and the mixed-board test's own assertion: a conversation reference recorded
  under one runtime is never handed to the other, so a harness session
  resumes as itself.
- **The four fault invariants.** Listed row by row, with the fault each
  injects, in [`native-parity.md`](native-parity.md) under "The four
  invariants, and what pins each".
- **Installation, diagnostics and packaging.** The `Native Install` matrix
  job on ubuntu/macOS/Windows; `ctx::doctor`'s six failure classes derived
  from a real `Inventory::build`; the schema-2 sidecar migration and its
  downgrade.

## 3. What is NOT verified

Stated plainly, because the value of the record above depends on it.

1. **No live provider route.** Every transport test replays fixtures. No
   Anthropic, OpenAI, Google, Azure, Bedrock or compatible endpoint has been
   contacted by a test in this repository. What that leaves unproven is
   specifically: real authentication against a real account, real
   rate-limit/quota behaviour under real load, real model availability, and
   any latency or token number for a native run. The `live-validated` rung
   exists for exactly this and is unreached; it is the one release blocker
   named in [`native-parity.md`](native-parity.md).
   **Update 2026-09-16 (issue #592):** the tooling to collect this evidence
   is now committed -- `docs/evidence/provider-live-contract-manifest.json`
   is a redacted, schema-versioned manifest with one row per production
   route (Anthropic Messages, OpenAI Responses, Google Generative AI, and
   the OpenAI-compatible chat family's representative route), each row
   `collection_status: "not_collected"` until an operator runs its named
   `live_*` test with real credentials
   (`commands::ctx::provider::evidence::record_stream_result` then rewrites
   that row in place; see `docs/evidence/README.md`). No key exists in this
   environment, so no row has been collected by this update -- the manifest
   states that plainly rather than being mistaken for evidence.
2. **No comparative quality evaluation against real models.**
   `docs/benchmarks/native-runtime-baseline.jsonl` holds two rows today,
   both `"runtime": "harness"` (#470 and #471). There is no `"runtime":
   "native"` row, so there is nothing to compare and **no quality, cost,
   latency or token claim is made anywhere in this release**. The targets a
   future comparison must clear are declared in §4 -- before the data
   exists, deliberately.
3. **No hand-run fresh install on a real desktop OS.** The `Native Install`
   job proves the first-run path on GitHub-hosted ubuntu, macOS and Windows
   runners. It does not prove an install from a released artefact
   (Homebrew/Chocolatey/`install.sh`) on an operator's own machine, nor
   anything about a machine with an existing older zirv configuration that
   CI does not construct.
4. **No verified process isolation on Windows.** There is no
   restricted-token/AppContainer backend. The broker refuses a sandboxed
   invocation there rather than running it unconfined, which is safe but is
   not the same as having the feature.
5. **No live MCP/browser integration.** `zirv ctx capabilities --probe`
   contacts configured MCP servers, and the CI job runs it with none
   configured. Browser-backed frontend rendering is configuration-gated and
   unverified here.
   **Update 2026-09-16 (issue #610 scenario 2):** the frontend
   run-inspect-capture-review acceptance test now exists --
   `frontend_render::tests::
   a_frontend_run_inspect_capture_review_scenario_runs_live_or_names_exactly_what_is_missing`
   -- and checks this live rather than assuming it: it is present, named,
   and prints exactly why it did no work whenever no Chromium-family
   browser is discovered (still the case on this machine), or drives a
   real capture through `render()` when one is. The review half of that
   same scenario still needs a real, credentialed reviewer even once a
   browser exists (see the #592 update above), and is reported the same
   honest way rather than faked.
6. **`#[cfg(unix)]` PTY paths are not compiled on Windows.** The real-PTY
   wrap tests are Linux/macOS-only by construction. This affects the legacy
   surface, not the native one -- a native session has no PTY -- but it is
   listed so the matrix is not read as claiming more than CI runs.

## 4. Pre-declared non-regression targets

Declared **now**, before any native comparison run exists, precisely so they
cannot be tuned to whatever the first run happens to produce. They refine
`docs/benchmarks/native-runtime-baseline.md` §3 rather than replace it; that
document's field definitions and its "observed or absent" rule still govern.

A `native` row in `native-runtime-baseline.jsonl` is compared against the
nearest `harness` row **of the same task class** (roadmap step vs. roadmap
step, release batch vs. release batch). The targets:

| # | Target | Rule |
|---|---|---|
| T1 | Gate quality | `gates_first_pass` must not go `true` -> `false`. |
| T2 | Review burden | `confirmed_findings` must not increase for comparable scope, and `review_rounds` must not exceed the harness comparison by more than 1. |
| T3 | Task completion | the task must reach merge without a runtime-caused fallback to the harness. A fallback that happened because a role had no native route configured is a configuration finding, not a regression; a fallback caused by the native path failing is a regression. |
| T4 | Cost and wall time | reported honestly in both directions; **no ceiling**. A native run costing more is not disqualifying -- hiding it is. `tokens` may be `null` only with a `notes` field saying why. |
| T5 | Sample size before any default change | at least **three** `native` rows across at least **two** distinct task classes, each cleared against T1-T3, before `runtime.default = "native"` may be proposed as a shipped default for anyone but the operator who set it themselves. |

T5 is the one that binds the release decision in §6. T1-T4 are per-run.

## 5. Migration and rollback

The operator-facing guide is README's **Native setup, diagnosis and
rollback** section; the design rationale is
[`2026-09-14-native-release-readiness.md`](2026-09-14-native-release-readiness.md)
§4. The short form, and the order matters:

1. **Forward.** `zirv ctx provider init`, then `zirv ctx provider credential set` for
   the routes you want, then `zirv ctx doctor` until it reports no blockers.
   Turning it on is `zirv ctx config migrate --to native`, which writes
   `[runtime]`, backs the previous document up beside it, and records the
   schema in the `~/.zirv/ctx.migration.toml` **sidecar** -- never as a key
   inside `ctx.toml`, because `CtxConfig` is `deny_unknown_fields` and an
   older binary would reject the whole file over one key it had not heard of.
2. **Back.** `zirv ctx config migrate --downgrade` restores the
   pre-migration backup byte for byte. **Downgrade first, install the older
   binary second** -- an older binary cannot parse `[runtime]` either, so
   "remove the key" and "restore the document that predates us" have to be
   the same operation, and only the second is exact.
3. **Untouched either way.** `~/.zirv/native.toml`, the journals, and the
   harness conversation references those journals carry are outside the
   transaction; `config_cmd::tests` pins that they survive a round trip in
   both directions.
4. **Per-seat, without migrating anything.** `--runtime native` on `zirv ctx
   exec`, `zirv ctx agent`, `zirv chat`, `zirv workflow review run` and a
   script `agent:` step is always available and outranks every configured
   default. An operator who wants to try one seat natively never has to
   change a machine-wide file at all.

## 6. The release decision

**The default stays the harness. Native is opt-in. A default change is a
separate operator decision, not part of this release.**

The reasoning, stated so it can be argued with:

- The code is done and it is honestly scored: 158 capabilities, no missing
  parity row, no hidden harness dependency, the four fault invariants pinned
  by name, an all-native team and a mixed team both proven on three OSes.
- The *evidence* is not done, and the missing piece is not something this
  repository can produce: `live-validated` needs an operator's own API auth
  material and spends that operator's money (§3.1), and the comparative
  evaluation needs at least three native baseline rows that do not exist yet
  (§4, T5).
- Flipping a default on unmeasured evidence is exactly the failure
  `docs/benchmarks/native-runtime-baseline.md` was written in N01 to
  prevent: "shipping a native default that is quietly worse and nobody can
  tell, because nothing honest was ever recorded to compare it against."

`[runtime]` is `REPO_FORBIDDEN` in both directions, so no checkout can move
an operator's unflagged sessions onto metered native routes -- or off them.
Only `~/.zirv/ctx.toml`, `ZIRV_CTX_*` or an explicit flag decides.

### What an operator must do to close the remaining gaps

Each of these is a step only the operator can take; none is blocked on code.

| Gap | What closes it |
|---|---|
| Live provider evidence (§3.1) | configure one route per vendor you care about (`zirv ctx provider init`, `zirv ctx provider credential set`), run `zirv ctx doctor --live` to confirm the model list, then drive one real task with `zirv ctx exec --runtime native --route <id>`. Record the run under `docs/benchmarks/` and raise the affected rows to `live-validated` -- `ZCHK-NATIVE-PARITY` will not accept that rung without the committed recording. |
| Comparative quality (§3.2) | run at least three roadmap steps or release batches with `--runtime native`, append one `"runtime": "native"` row per run to `docs/benchmarks/native-runtime-baseline.jsonl` with observed fields only, and check them against T1-T3. |
| Real-OS fresh install (§3.3) | install the released artefact on a clean Windows, macOS and Linux machine (Chocolatey, Homebrew, `install.sh`), then run `zirv ctx provider init && zirv ctx doctor --json` and confirm it matches what the CI job reports. |
| Windows process isolation (§3.4) | tracked as N04 (#473); needs a restricted-token/AppContainer helper, not an operator action. |
| Default change | only after T5 is satisfied. Then it is `zirv ctx config migrate --to native` on that operator's own machine -- still not a shipped default. |
