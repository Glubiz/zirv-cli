# ZIRV.md discovery, same-directory precedence, compatibility dedup

**Date:** 2026-09-15 · **Issue:** #538 (chunk A of 3) · **Roadmap:** #469

## Context

The wrapped-harness context collector (`src/commands/ctx/optimize.rs`) and
the shared trust vocabulary (`src/commands/ctx/surface.rs`) already treat
`CLAUDE.md` and `AGENTS.md` as generic, trust-classed context surfaces
(issues #40/#41). Issue #538 gives the native harness its own instruction
file, `ZIRV.md`, while requiring `AGENTS.md` to keep working as a first-class
portable source and `CLAUDE.md`/singular `AGENT.md` to be read without
duplicating the same rules twice. This chunk (A) is scoped to the collector
half: discovery, same-directory precedence, and dedup, feeding `zirv context
status`/`zirv context lint`/`drift.rs`. Chunk B wires the native context
compiler (`runtime/context.rs`) and journal; chunk C covers migration
tooling and the native `/instructions` view.

## Decisions

**File and precedence contract.** Five new `Layer` variants
(`optimize.rs`): `GlobalZirvMd` (`~/.zirv/ZIRV.md`), `RepoZirvMd`
(`<repo>/ZIRV.md`, or `<repo>/.zirv/ZIRV.md` when the root file is absent —
both may be collected as separate surfaces when both exist), `NestedZirvMd`,
plus `RepoAgentMd`/`NestedAgentMd` for the singular `AGENT.md` compatibility
alias. `ZIRV.md` names `Provider::Zirv`; the `AGENT.md` alias keeps
`AGENTS.md`'s own `Provider::Codex` (issue #40's "portable" convention
marker), per the brief. Trust is derived from `Scope` exactly as before —
`Layer::trust()` for these five variants is `RepoUntrusted` at every
repo-owned scope, `Operator` only for the operator-global file — proven the
same way issue #39 proved it for every earlier layer
(`repo_owned_layers_can_never_carry_operator_trust`,
`a_repo_zirv_md_can_never_be_operator_trusted` in `surface.rs`).

**Same-directory precedence** (`context.rs`, next to `precedence_tier`):
`within_tier_rank(Layer) -> u8` gives `ZIRV.md` 0, `AGENTS.md` 1, `CLAUDE.md`
2, singular `AGENT.md` 3. `resolve_instruction_winners(surfaces, exclusions,
repo, home)` groups every same-directory-precedence candidate by (scope,
logical directory) — `<repo>/.zirv/ZIRV.md` is normalized to the repo root's
own directory bucket so it competes with `<repo>/ZIRV.md` rather than living
in a separate, never-compared group — and marks the lowest-ranked member
`Included`, every other member `Shadowed { by: <winner path> }` unless dedup
(below) applies. Every surface handed in gets exactly one `Resolved` entry:
a non-candidate (canonical `.zirv/context/`, settings) is always `Included`,
since nothing in its own directory competes with it. `AGENT.md` becomes a
real (`Included`) candidate only when no `AGENTS.md` sibling exists — this
falls out of the general rank ordering rather than needing a special case —
and unconditionally carries a `migration: Some("rename ... to AGENTS.md")`
diagnostic regardless of whether it won.

**Deviation from the brief's literal signature.** The brief describes
`resolve_instruction_winners` as a function over one `&[ContextSurface-like]`
slice. The implementation instead takes `(surfaces: &[optimize::Surface],
exclusions: &[optimize::Exclusion], repo: &Path, home: Option<&Path>)`:
`repo`/`home` are needed to enforce the import-chain trust-root-escape rule,
and `exclusions` is a second collection because a symlinked candidate is
never read into `surfaces` at all (see below) — folding both into one input
type was judged more complex than justified for this chunk. No filesystem
access happens inside the function; every read it needs is already in the
two slices the caller collected.

**Dedup** (`context.rs`). A shadowed candidate is `Duplicate { of: winner }`
instead of `Shadowed` when: its text is byte-identical to the winner's; or
its entire non-blank content is a single recognized import line resolving
(after a bounded, cycle-checked chase — `MAX_IMPORT_DEPTH = 3`, purely
against the already-collected `surfaces` slice, no extra filesystem reads) to
the winner. Import syntax recognized: the Claude Code `@file`/`@./file`
form, `include <path>`, and a lone markdown link line. A chase that revisits
a path it has already seen is `Excluded { reason: "import cycle" }`; one that
leaves its own trust root (`repo` for every repo-owned scope, `home` for
`Global`) is `Excluded { reason: "escapes trust root" }`.

**Symlinked instruction files are refused, not read through** — mirroring
`runtime/context.rs::push_file`'s posture, extended to `optimize.rs`'s own
collector for the first time. `push_surface_tracking` (renamed from
`push_surface`, all ~30 call sites updated) checks `Kind::Instructions`
candidates for a symlink before reading; a hit is never turned into a
`Surface` (so its target's content can never reach analysis or a future
prompt) and is instead recorded as an `Exclusion { layer, path, reason,
symlink_target }`. `symlink_target` records where the link points
(`fs::read_link`, never the target's content) so `resolve_instruction_
winners` can report a symlink that resolves to its directory's winner as
`Duplicate` rather than a blanket `Excluded` — a symlink IS the same content
by construction. `collect_surfaces`/`collect_instruction_exclusions` are now
thin `.0`/`.1` projections of one shared walk
(`collect_surfaces_and_exclusions`), so the two views can never disagree
about what was actually found on disk. A symlinked directory in the nested
walk gets one `Exclusion` too (previously silently skipped).

**Push order and the surface cap.** `ZIRV.md` candidates are pushed ahead of
every compatibility file at both repo and nested scope, so `MAX_SURFACES`
can never drop a `ZIRV.md` candidate in favour of a `CLAUDE.md`/`AGENTS.md`/
`AGENT.md` one in a large monorepo
(`zirv_md_survives_the_surface_cap_ahead_of_compatibility_files`). The
relative order of pre-existing pairs (`CLAUDE.md` before `AGENTS.md`) is left
untouched to minimize blast radius on existing behavior.

**`zirv context status`** (`context_status.rs`) gains, per instruction
surface: trust (`operator`/`repo-untrusted`), scope
(`global`/`repo`/`nested`/`local-private`), the first 12 hex characters of a
SHA-256 of the surface's text (reusing `memory::sha256_hex`, already
`pub(crate)`), and the decision (`included` / `shadowed by <path>` /
`duplicate of <path>` / `excluded: <reason>`, with `, oversized` appended
when the existing `[OVERSIZED]` budget flag also fires). A symlink-refused
candidate gets a synthetic zero-byte `Surface` so it renders through the same
code path rather than a second, parallel rendering branch. `drift.rs`'s own
`precedence-shadowing` finding needed no change: it already keys off
`context::precedence_tier`, which this chunk extended with arms for the five
new layers, so it picks up `ZIRV.md`/`AGENT.md` pairs automatically without a
second competing "which layer wins" computation.

## Verified

- `cargo build`, `cargo fmt -- --check`, `cargo clippy --all-targets -- -D
  warnings` all clean.
- `cargo nextest run --no-fail-fast -E 'test(ctx::optimize) or
  test(ctx::context) or test(ctx::context_status) or test(ctx::context_cli)
  or test(ctx::drift) or test(ctx::surface)'`: 240/240 passed.
- `cargo run -q -- verify --builtin`: all 10 built-in checks pass, including
  `ZCHK-RUNTIME-INVENTORY` (no new command verb this chunk) and
  `ZCHK-NATIVE-PARITY`.
- `cargo nextest run every_repo_slug_consumer`, `scripts/check-test-presence.sh
  --base release/native-harness`, `scripts/check-readme-features.sh`: all
  pass.
- The `#[cfg(unix)]`-gated `symlinked_instruction_files_are_excluded_not_
  skipped` test (`optimize.rs`) could not be exercised on this Windows
  development box; it follows the exact pattern of the pre-existing
  `the_nested_scan_does_not_follow_a_symlink_out_of_the_repo` test in the
  same file, which carries the same platform gate.

## Deferred

- Native context compiler (`runtime/context.rs`) and journal wiring — chunk B.
- The native `/instructions` (or `/context instructions`) TUI view — chunk B.
- Migration tooling (`zirv setup`-style idempotent `ZIRV.md` generation from
  existing sources, never overwriting) — chunk C.
- A `--json` output mode for `zirv context status` — no such flag exists
  today; out of scope for this issue's acceptance bullets 3/5/6/9.
