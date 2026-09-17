# Intent

## Problem

Today `zirv chat` launches the configured harness with the configured model
(`agent = "claude"`, `chat.model = "fable"`) for every request, and workflow
adoption happens after the fact. `zirv workflow classify` is deterministic: it
keyword-matches the task text and measures the working-tree diff, so a prompt
for a substantial design task with nothing changed yet classifies as
`other/bounded/low` (measured on this very task: risk 18, evidence `.DS_Store`
and `.mcp.json`). The operator has to know up front which workflow kind, model
tier and topology (single seat vs orchestrator + workers) to ask for, and the
most expensive seat model is spent on trivial asks.

Issue #537 specifies the missing piece: a durable, explainable execution
profile derived from the user request at intake, before the first provider
turn. Nothing implements it yet; `workflow/team.rs` explicitly deferred "a
bounded structured model decision".

TypeSafe AI's Jev (`jev-latest`, `POST https://api.typesafe.ai/v1/systemone`,
`Authorization: Bearer $TYPESAFE_API_KEY`) is a decision-only model: it
evaluates typed questions (Choice / Score / Noul) against a `state` and
returns the chosen option with calibrated probabilities and a confidence, in
70–500 ms, at $0.042 per MTok input with output free. It is the right shape for
an intake decision: no free text to parse, output space fixed in advance,
confidence to gate on.

## Desired outcome

A "harness proxy" intake step in front of a session launch. With
`[proxy] enabled = true`, `zirv chat` (and bare `zirv`) opens a plain prompt
window, takes the user's prompt, wraps it with bounded metadata (enabled and
ready harnesses with headroom, the model catalogue per vendor with tier and
price, the workflow kinds with one-line descriptions, repository signals) and
asks a decision model for an execution profile: execution mode
(mechanical-direct / bounded-direct / orchestrated), complexity, risk, intent,
workflow kind, and a harness + model for the orchestrator seat plus a worker
tier. Zirv validates the decision against what is actually enabled and
configured, applies it (adapter, `--model`, seat role, orchestrator vs
single-seat prompt layer), starts the chosen workflow with the profile's
complexity and risk, and launches the session with the user's prompt as its
first prompt. The profile, the decider that produced it, its confidence and the
fallbacks taken are persisted, shown by `zirv ctx status`, and printable with
`zirv ctx proxy --dry-run "<prompt>"` without launching anything.

Deciders form a chain: TypeSafe Jev (when its credential env is set) → the
existing helper-model chokepoint (`helper_answer`, a small harness model, the
same questions rendered as a JSON contract, one repair) → deterministic
(`classify.rs` + `team.rs`). A field below `proxy.min_confidence` falls to the
next decider for that field. The deterministic result is also the floor: a
model decision may raise complexity, risk or validation depth, never lower them
below what deterministic signals require (#537, "reclassify monotonically").

The same intake applies to both runtimes: `RuntimeKind::Harness` (wrapped
claude/codex, what runs today) and `RuntimeKind::Native` (a route from
`native.toml` roles), behind the existing gate.

## Constraints

- The native harness stays disabled. `runtime::native_available()` remains
  `cfg!(test)`; no config key, env var, flag or Cargo feature introduced here
  may enable native execution. The proxy's native branch is exercised only by
  unit tests. Operator instruction 2026-09-17, verbatim: "DO NOT ACTIVE THE
  NATIVE HARNESS YET, IT IS DISABLED FOR A REASON".
- The TypeSafe adapter calls Jev over HTTP directly with the crate's existing
  `ureq` client: no Python/JS SDK, no new HTTP dependency. The credential is
  read from an env var named in config (`credential_env`, default
  `TYPESAFE_API_KEY`) and never stored in config, state or logs. The request
  `state` never includes secrets, transcript text or file contents beyond
  bounded repository signals.
- Model ids are operator configuration: the decider picks among
  `catalogue.rs` rungs and harnesses enabled by `.settings.toml` and
  `ready()`; an unavailable choice falls back with a recorded reason and never
  fails the launch. The proxy may not change provider, account or billing
  class (#537).
- Every `[proxy]` key is operator-only (`REPO_FORBIDDEN`): it selects models
  and spends money on a background call, so a repository checkout may not
  enable, disable or steer it. Repository text inside the state is untrusted
  evidence.
- Reuse before adding: `helper_answer` and the `ROLE_*` chokepoint,
  `catalogue`, `classify`, `team.rs` (`TeamPlan`, `Seat`, `ModelTier`),
  `StartArgs` complexity/risk overrides, `build_launch(initial_prompt)`,
  `extra_with_model`, the `FallbackConfig` headroom router. No second planner.
- `rot.rs` untouched; no `wrap` hot-path `unwrap`. Tests inline in
  `#[cfg(test)]`; fixtures under `tests/fixtures/`. The Jev call has a hard
  timeout (default 10 s) and on any failure the chain continues: the proxy can
  never make `zirv chat` fail to launch.
- README: a "Harness proxy" section, `[proxy]` in the configuration and
  trust-boundary tables, `zirv ctx proxy` in the verbs table.
- Delegation: native Claude subagents only, no codex this session (operator
  instruction 2026-09-16).

## Open questions

None. Routine choices resolved: feature name "harness proxy", config table
`[proxy]`, verb `zirv ctx proxy`; harness and model are chosen by the decider
from the enabled set and validated by zirv; confidence floor 0.5 by default
(TypeSafe documents 0.5–0.6 as the baseline); Jev output is free, so spend is
recorded on input tokens only.

## Acceptance criteria

- [ ] `zirv ctx proxy --dry-run "<prompt>"` prints one ExecutionProfile JSON
      (mode, complexity, risk, intent, workflow, orchestrator harness/model,
      worker tier, decider, confidence, reasons, fallbacks) without launching
      anything.
- [ ] With `TYPESAFE_API_KEY` set and `[proxy] decider = "typesafe"`, the
      profile comes from one `POST /v1/systemone` whose body carries the
      prompt and metadata as `state` and the Choice/Score/Noul questions; a
      401/422/429/529 or timeout falls through to the next decider and is named
      in `fallbacks`.
- [ ] Without any key, the helper-model decider produces the same profile
      shape via `helper_answer`; with the helper unavailable, the deterministic
      decider does. The launch never fails because of the proxy.
- [ ] A model decision can raise but never lower complexity, risk or
      validation depth below the deterministic classification.
- [ ] `zirv chat` with `[proxy] enabled = true` reads a prompt from the
      terminal, launches the chosen harness with the chosen `--model` and seat
      role, starts the chosen workflow with the profile's complexity and risk,
      and passes the prompt as the session's first prompt. With the proxy
      disabled, `zirv chat` behaves exactly as today.
- [ ] A decided harness that is not enabled or ready, or a model absent from
      the vendor's catalogue, is rejected with a recorded reason and the
      deterministic choice is used instead.
- [ ] The native branch resolves a route from `native.toml` roles and is
      covered by unit tests only; `native_available()` is unchanged and
      `zirv chat --runtime native` still refuses with the "coming soon" text.
- [ ] `zirv ctx status` shows a `proxy:` line with mode, seats, workflow,
      decider and confidence; a `[proxy]` key in a repository `ctx.toml`
      hard-errors as `REPO_FORBIDDEN`.
- [ ] README documents the feature, its config keys and the trust boundary;
      the four gates (build, `nextest --no-fail-fast`, `fmt --check`,
      `clippy -D warnings`) pass, with any failures diffed by name against a
      same-environment run on `main`.
