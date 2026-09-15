# `zirv native`: an explicit experimental top-level command (issue #540)

**Date:** 2026-09-15 · **Issue:** #540 · **Roadmap:** #469 (builds on N11 #480)

## Context

`zirv chat --runtime native` (N11, #480) already opens the structured native
conversation pane -- no coding harness installed, no PTY -- as a flag on the
existing `chat` verb. It works, but it is not discoverable: an operator has
to already know the flag exists, `zirv help` gives it no billing of its own,
and nothing in the generated command surface (`zirv commands --json`) marks
it as anything other than an ordinary flag on a stable command. Issue #540
asks for one obvious top-level command, `zirv native`, while making sure the
native runtime -- still under development -- is never presented as a stable
default: hidden from ordinary `zirv help` output, or shown only in a
clearly-labelled experimental section, with `zirv native --help` and
`zirv commands --json` staying honest about what it is.

The investigated baseline (`main.rs`'s `chat`/`agent` alias interception,
`help.rs`'s curated top-level help, `command_schema.rs`'s generated surface,
`utils::RESERVED_COMMANDS`, and `chat.rs`'s own native-chat validator)
already solves the adjacent problem for `chat`/`agent`: a raw-argv
interception in `main.rs`, before clap ever sees `Input`, rewrites the alias
into the `ctx` verb tree. `native` reuses that exact mechanism rather than
adding a second implementation.

## Decision

### One rewrite, one launch path

`main.rs` gains `is_top_level_native_alias`/`rewrite_native_alias_args`,
matched case-insensitively like every other reserved built-in. The rewrite
turns `zirv native [args...]` into `["ctx", "chat", "--runtime", "native",
...args]` -- literally the argv `chat.rs`'s own `run_with`/`run_native_chat`
already parses and validates for `zirv chat --runtime native`. There is no
second native-launch implementation anywhere: adapter-free launch, TTY
probing, the wrapped-only-flag refusal, nesting protection, journal and pane
startup are all the one code path both spellings reach. `native` is added to
`utils::RESERVED_COMMANDS` (case-insensitive, like `chat`/`agent`), so a
`.zirv/native.yaml` script or a shortcut keyed `native` can never shadow it
-- `zirv help`'s existing shadowed-script marker covers that automatically,
with no code change of its own.

Direct `zirv native` deliberately skips `maybe_run_first_run_wizard`
entirely (unlike the `chat` alias, which runs it for a bare `zirv chat`):
native prerequisites are diagnosed by `zirv ctx doctor` (#491's path), not
the wrapped-harness setup wizard, and the issue is explicit that this alias
must never silently select a legacy runtime. The rewritten argv always
names `--runtime native` outright, so there is nothing left for a wizard or
a runtime-selection default to decide.

### `zirv native --help`: reused prose, not a second description

`zirv native --help`/`-h` is intercepted as its own raw-argv shape
(`is_top_level_native_help`: exactly `native` followed by one `--help`/`-h`
token) before the ordinary rewrite, so it never falls through to clap's
generated help for the `chat` verb tree -- which says nothing about
prerequisites, limitations, or the experimental status. `chat.rs` gains
`native_help_text()`, printed verbatim by `main.rs` and exiting 0. Its
limitations section is built from two new `pub const` strings,
`NATIVE_WRAPPED_ONLY_FLAGS_REFUSAL` and `NATIVE_TTY_REFUSAL`, which
`run_native_chat`'s own refusals now print via `{CONST}` interpolation
instead of a second hand-typed copy of the same sentence -- the help text
and the actual refusal can never drift apart, because they are now the same
string.

### The one-time banner: environment, not a new clap flag

The orchestrator's brief asked for a single, low-noise experimental banner
on stderr, printed once per launch, keyed on the alias having been used --
`zirv chat --runtime native` itself must never print it, since the runtime
choice is not what is experimental-and-alias-specific here, the *spelling*
is. Two ways to carry that signal from `main.rs`'s rewrite into
`run_native_chat` were considered:

- **A hidden clap flag on `ChatArgs`.** Rejected: it would add a flag to a
  struct an operator's own `zirv chat --help` renders and `command_schema.rs`
  reflects into `zirv commands --json`'s flag list for `zirv chat` (and,
  by the alias-clone construction `command_entries` already uses for
  `chat`/`agent`, `zirv native` too) -- internal plumbing leaking into a
  documented, machine-readable surface for no operator-facing reason.
- **A process environment variable, `ZIRV_CTX_NATIVE_ALIAS`.** Chosen.
  `main.rs` sets it (`std::env::set_var`, the same unguarded call already
  used elsewhere in this codebase, e.g. `script_runner/command_types.rs`)
  immediately before calling `ctx::dispatch`, and `run_native_chat` reads it
  back through the exact `EnvLookup` closure parameter every other
  environment signal in that function already goes through (mirroring
  `quiet_env`'s `ZIRV_CTX_QUIET` and `pin_env`'s `seat::PIN_ENV`) --
  fully testable with a stub closure, no real process environment or pty
  required.

The banner print itself is the first statement in `run_native_chat`, ahead
of the runtime/flag/TTY checks, so an operator sees "zirv native is
experimental; `zirv chat` remains the stable harness." even when the launch
goes on to refuse for an unrelated reason (no TTY, a bad `--runtime` value).
It runs exactly once because `run_native_chat` itself runs exactly once per
process invocation -- no separate de-duplication state was needed.

### `zirv help`: a new section, never a conditional inside the existing one

`help.rs`'s `write_builtins` gains one more `write_table` call after the
existing `init only:` section: a `Experimental / work in progress:` header
with a single `native` row. It is never added to the `Commands:` table
above it, so `native` can never appear beside a stable command without
qualification -- the acceptance criterion is enforced by *where the row is
written*, not by a runtime flag checked inside a shared table renderer.

### `zirv commands --json`: a `Stability` enum, not a one-off conditional

`command_schema.rs` gains a `Stability` enum (`Stable`/`Experimental`/
`Hidden`, kebab-case serialised) and two new `CommandEntry` fields:
`stability` (always populated, `Stable` by default) and `runtime` (`Option
<String>`, `Some("native")` only for the `zirv native` entry). Every
existing construction site -- the clap-derived `walk()` push, and the
`synthetic()` helper used for `help`/`version`/`init`/`create`/`skill`/
`commands` -- sets `stability: Stability::Stable, runtime: None` explicitly,
so the field is a real classification on every entry, not an
absent-means-stable convention a future command could accidentally violate.

`zirv native`'s own entry is cloned from the already-built `zirv chat` entry
(the same alias-clone pattern `command_entries` already uses for `chat`/
`agent` themselves), so its `args`/`flags`/`mutating`/`availability` can
never drift from the command it actually rewrites into -- forwarding every
native-compatible `chat` option is a property of the clone, not something
that has to be kept in sync by hand. Only `about`, `stability` and
`runtime` are overridden on top of the clone. The human-readable
`zirv commands` renderer (`render_human`) also tags a non-stable entry with
`[EXPERIMENTAL]`/`[HIDDEN]`, so the same fact is visible without `--json`
too.

## What is verified

- Cargo unit tests (Windows): argv rewrite (`native --foo` -> `chat
  --runtime native --foo`, case-insensitive `NATIVE`), reserved-name
  protection (`utils::is_reserved_command("native")` and `"Native"`),
  `is_top_level_native_help`'s exact-shape detection, `zirv commands --json`
  carrying `stability`/`runtime` (and every other entry defaulting to
  `stable`/`None`), `zirv help` placing `native` only inside the
  experimental section (never inside the stable `Commands:` table), the
  one-time banner printing for the alias env signal and never for a plain
  `--runtime native` call, and `native_help_text()` reusing the shared
  refusal constants verbatim.
- A process-level test (`native_help_exits_0_and_prints_the_experimental_
  notice`, mirroring the existing `memory_is_intercepted_before_script_
  lookup` pattern) spawns the real built binary for `zirv native --help`
  and asserts exit code 0 and the experimental notice in stdout.
- `docs/design/native-runtime-inventory.md` gained a `native` row (owner
  `N11 (#480)`, matching `ctx chat`'s own) so `ZCHK-RUNTIME-INVENTORY`
  keeps passing -- `command_entries()` now discovers `zirv native` as a
  depth-1 verb, and the check fails any real verb missing a row.
- README: the reserved-command-names anchor list (`ZCHK-DOC-RESERVED`), a
  Features bullet (`scripts/check-readme-features.sh`), and a prose
  paragraph under "The native conversation pane" all name `native` and the
  `zirv chat --runtime native` equivalence.

## What is deferred

- Promotion out of the experimental section (dropping the `Stability::
  Experimental` tag, or defaulting the operator's runtime to native) is
  explicitly out of scope here -- #492 (N23, parity proof) owns that
  decision, per the issue's own "does not authorize making native mode the
  default."
- The native setup/doctor path direct `zirv native` may eventually offer
  (the issue's implementation note 6) is #491's (N22) own scope; this
  change only guarantees `zirv native` never substitutes the
  wrapped-harness first-run wizard in its place.
