# Native setup, migration, diagnosis, docs and packaging (N22)

**Date:** 2026-09-14 · **Issue:** #491 · **Roadmap:** #469

## Context

N01–N21 built a native runtime that works. This step is about whether a
person who is not us can turn it on. Everything the roadmap shipped so far
assumed an operator who already had `~/.zirv/native.toml` in their head: the
routes, the role bindings, the credential reference syntax, the difference
between a route that is `configured` and one that is `authenticated`, and
which of the things that do not work are our fault. None of that was
discoverable from the binary.

The gap was also narrower than it first looked. Nearly every fact a setup or
doctor flow needs already had an owner:

- `provider::inventory::Inventory` — routes, accounts, pools, models,
  billing class, the evidence ladder, and (with `--live`) a model-list probe.
- `runtime::capabilities::discover` — MCP/web/browser/diagnostics/artifact/
  frontend integrations as available / unavailable / unverified, each with a
  diagnosis naming the missing binary, credential or key (N14, #483).
- `runtime::enforcement::PlatformIsolation::detect` — whether this platform
  has verified process containment (N04, #473).
- `adapters::ADAPTERS` + `ready()` — which coding harnesses are installed.
- `snapshot::redact_text` + `screen::screen` — the redaction pass `zirv ctx
  snapshot` already uses.

What was missing was not discovery. It was a *vocabulary*, a *default*, and a
*way back*.

## Decision

### 1. `zirv ctx doctor` classifies; it does not discover

The new verb gathers from the five owners above and adds exactly one thing:
each problem is sorted into one of six named classes.

    missing-auth-material · inaccessible-model · missing-tool
    unsupported-isolation · service-failure · upstream-entitlement

Five of those are the issue's acceptance criterion verbatim. The sixth,
`upstream-entitlement`, is deliberate and is how issue item 7 is satisfied
structurally rather than in prose: a subscription-billed account and a
vendor-CLI-only surface are genuinely not ours to fix, while a route profile
with no adapter yet is, and the two must never be reported in the same
bucket. `classify` puts the entitlement test *first*, before the credential
test, precisely because a subscription account's own message mentions a
credential — read naively it would have been filed as "get an API key",
which is advice that cannot be followed.

`classify`/`diagnose` are pure, following CLAUDE.md's `rot.rs`/`score.rs`
rule; `run_with` is the impure gatherer. The classifier reads message text
produced a few hundred lines away in `provider::inventory`, which is a drift
risk, so the test that pins it drives real configurations through
`Inventory::build` rather than asserting on hand-written strings: a reworded
message that lands in the wrong class fails the build.

Severity is separate from class. A route problem is `blocking` only when a
role actually names that route; an unavailable integration and an
unavailable sandbox are `advisory`, because the surfaces that need them
already refuse at the point of use with a typed error. That keeps the exit
code meaningful on Windows, where isolation is honestly unavailable and
always will be until the helper ships — a doctor that always exits 1 there
would be a doctor nobody runs.

One behaviour change fell out of the classifier: a probe's transport error
used to be pushed as the raw `ureq` string. "connection refused" names
neither what was being reached nor that reaching it is what failed, so it is
now prefixed `endpoint unreachable:`.

### 2. Redaction is two rules, not one

`snapshot::redact_text` (made `pub(super)` and reused, not reimplemented)
screens each line with `screen::screen` and replaces any flagged line with a
summary that never echoes the flagged text. That catches credential shapes
and long opaque runs — an API key and a continuation token both go.

It does not catch a pasted transcript. `ScreenFlag::RoleMarkerMidText` fires
only *past the start* of the text it is given, because it exists to catch a
forged turn boundary spliced into something else, and `redact_text` screens
line by line — so a transcript, whose every turn opens its own line, walks
straight through. The doctor adds the complementary rule, and it is
structural rather than heuristic: a doctor finding is a diagnosis, so a line
that opens like a conversation turn is not one, whatever it says, and is
replaced wholesale. Both JSON and text renderers go through the same helper
field by field; the JSON path deliberately does not serialize the report
struct, which would emit raw strings.

### 3. The default is a table, the flag is a sentinel

`[runtime]` in `~/.zirv/ctx.toml`:

```toml
[runtime]
default = "native"
[runtime.roles]
reviewer = "native"
```

`runtime::resolve` is the one pure ladder: explicit flag > `runtime.roles`
for this role > `runtime.default` > harness. It returns the authority that
decided alongside the decision, because "native because you asked" and
"native because your config says so" are the same outcome and very different
facts when a bill arrives — `zirv ctx doctor`'s role table prints that
column.

The mechanism is a sentinel rather than an `Option<String>`: `zirv ctx exec`
and `zirv ctx agent` default `--runtime` to the literal `configured`, and
their CLI entries (`run`, not `run_with`) normalise it to one of two literal
values before anything downstream sees it. That was chosen over changing the
field's type because ~80 `ExecArgs` literals and ~24 `AgentArgs` literals
construct these structs across the tree, every one of them with an explicit
runtime already; a type change would have been a large diff that says
nothing, while the sentinel leaves `run_with`, the native branch, the script
runner's direct callers and every receipt reading exactly what they read
before. It is also honest at the command line: `--runtime configured` is a
value an operator can type and understand.

`zirv chat` keeps `Option<String>` (its `None` already meant "wrapped
harness") and resolves `None` through the same ladder at the `orchestrator`
role.

An unrecognised value in the *config* degrades to the harness with a note,
never an error: a typo in a machine-wide file must not wedge every command on
that machine, and the doctor is where it gets reported. An unrecognised
value in the *flag* stays a hard error — that operator is at a prompt asking
for one specific thing.

The whole table is `REPO_FORBIDDEN`, in both directions. A checkout moving
an operator's unflagged sessions onto their metered native routes is
widening; moving them off is equally not a safety property a checkout gets
to assert, because the harness account is just as spendable.

### 4. The schema marker is a sidecar, and that is the whole downgrade story

`zirv ctx config migrate` brings `~/.zirv/ctx.toml` to schema 2, backs the
previous document up beside it, and records the schema in
`~/.zirv/ctx.migration.toml`.

The marker is not a key inside `ctx.toml`, and this is the load-bearing
decision of the whole migration design. `CtxConfig` is
`#[serde(deny_unknown_fields)]`. A `schema = 2` key in the document would
make *an older zirv binary reject the operator's entire configuration*
rather than ignore one key it has not heard of — turning a routine downgrade
into a machine that cannot run zirv at all. A sidecar file an older binary
never opens costs nothing and removes that failure mode.

The same property is why the downgrade restores a backup rather than editing
around the new table: an older binary cannot parse `[runtime]` either, so
"remove the key we added" and "restore the document that predates us" have
to be the same operation, and only the second one is exact. The documented
order is downgrade first, install the older binary second.

Both directions are idempotent, and the forward direction's idempotency is
not cosmetic: without it, a second `migrate` would overwrite the real
pre-migration backup with the already-migrated text, quietly destroying the
only thing the downgrade depends on.

Native state is deliberately outside the transaction. `~/.zirv/native.toml`,
the journals, and the harness conversation references those journals carry
are all in different files; a test pins that they survive a round trip in
both directions.

### 5. Packaging is a claim, so CI tests the claim

The native runtime is one binary. No component is downloaded, unpacked or
installed per platform, so `cd.yaml` needs no new artefact — and saying so is
worth nothing unless something checks it. The new `Native Install
(${{ matrix.os }})` job on ubuntu/macos/windows first *asserts* that no
coding harness is on PATH (rather than assuming the runner image has none, so
the job fails loudly if that ever changes), then walks the whole first-run
path on the built binary alone: provider template, inventory, doctor,
capabilities. A packaging regression that dropped a component shows up as a
failing command instead of as a broken install in the wild.

The migration round trip is the update/downgrade test on all three OSes. The
helper-path step runs the two existing tests that pin every model-calling
helper and a whole coordinating team with every coding harness absent
(`helper::tests::a_helper_answers_with_every_coding_harness_removed_from_
path`, `tools::tests::an_all_native_team_runs_a_workflow_with_every_coding_
harness_absent`), so a hidden subprocess dependency fails the matrix rather
than the user.

## What is verified

- Six failure classes are told apart from **real** `Inventory::build` output,
  not from hand-written strings
  (`doctor::tests::a_doctor_names_each_failure_class_from_a_real_inventory`).
- Present-but-rejected auth material is not mistaken for a service failure
  (`doctor::tests::a_rejected_secret_is_auth_material_not_a_service_failure`).
- An unavailable integration is an advisory missing tool, and does not block
  (`doctor::tests::an_unavailable_integration_is_an_advisory_missing_tool`).
- A dump carrying an API-key-shaped string, a transcript excerpt and a
  continuation token comes out with none of the three, in both renderers,
  while still naming the class
  (`doctor::tests::a_diagnostic_dump_carries_no_secret_transcript_or_
  continuation_data`).
- A machine with no native configuration still reports which backend each
  role would get, and an operator who set `default = "native"` with nothing
  configured is blocked rather than merely advised
  (`doctor::tests::an_unconfigured_machine_still_reports_the_backend_each_
  role_would_get`).
- The end-to-end CLI path exits 0 with no blockers and shows the deciding
  authority (`doctor::tests::the_command_reports_the_resolved_backend_and_
  exits_zero_without_blockers`).
- The resolution ladder, in all four rungs, including that an explicit flag
  outranks a configured native default and that an unconfigured table still
  answers `harness` (`runtime::tests`, four tests).
- Migration is idempotent forward and back, preserves comments, keeps the
  original backup on a re-run, and leaves native.toml and journals untouched
  (`config_cmd::tests`, four tests).

## What is deferred

- **Live-provider validation.** Every test here is fixture- or service-level.
  The `validated` rung of the evidence ladder still needs N19's real
  endpoint pass; nothing in this step claims it.
- **`zirv setup` integration.** The native setup path is `zirv ctx provider
  init` / `credential set` / `zirv ctx doctor`, documented as such.
  `zirv setup` remains the harness-hook installer it was; folding a native
  section into its 6.5k lines buys nothing this step needs.
- **Windows process isolation.** Still no verified restricted-token/
  AppContainer helper. The doctor reports it as `unsupported-isolation` and
  the README lists it under implementation gaps with the other tracked ones
  — explicitly *not* under entitlement limitations.
- **A migration that transforms anything.** Schema 2 adds a table; it does
  not rewrite existing keys. The backup/sidecar/downgrade machinery is built
  so the first migration that *does* transform something has somewhere to
  land.
- **Per-invocation runtime notes on stdout.** A degraded configured value is
  announced on stderr from the CLI entry, so a `--json` receipt on stdout
  stays parseable. A structured warning channel for it is not attempted.
