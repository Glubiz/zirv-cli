# Native conversation view (N11)

**Date:** 2026-09-13 · **Issue:** #480 · **Roadmap:** #469

## Context

N09 (#478) made a native session real (`zirv ctx exec --runtime native`),
N10 (#479) made it a worker the fleet can delegate to. Both talk through the
protocol/journal facts N03's `journal.rs` already commits: acknowledged
input, committed assistant messages (with interleaved tool-call blocks),
tool executions and their outcomes. Nothing renders any of that yet except a
final JSON status line (`run_headless`) -- there is no everyday view of a
native conversation the way a wrapped harness pane already has one, through
its own `vt100`/`render_grid` PTY rendering.

The existing dashboard (`dash::mod`, `dash::pane`, `dash::ui`) is built
entirely around that PTY model: `Pane` owns a `vt100::Parser` over a real
child process's bytes, and `dash::mod`'s event loop threads `Vec<Pane>`
through roughly a hundred call sites (spawn, drain, mail sweep, budget
accounting, attention, rollover, restore roster). A native session has no
PTY and no child process to simulate one from -- it is structured events,
not a byte stream -- so it needs its own renderer, not a fake terminal to
feed through the existing one.

The roadmap issue itself scopes this: item 6 asks for native and wrapped
panes to coexist "without imposing native controls that a wrapped adapter
cannot implement", and the acceptance criteria close with "this issue
provides the initial usable view; integrated multi-agent attention and
rollover UX is completed in N21." N11 is the view and its presentation
contract; wiring a `PaneKind` into `dash::mod`'s live pane list -- spawn,
mail, budget, attention, rollover, restore -- is deliberately left to a
later, separately reviewable step (see "What is deferred" below), rather
than attempted as one large, harder-to-review change against a 30k-line
event loop this issue did not need to touch to satisfy its own criteria.

## Decision

### One pure reducer, one set of presentation state, two renderers

`dash::native_pane::build_transcript(&journal::ConversationState) ->
TranscriptView` is the whole reducer: total, deterministic, and free of I/O,
the clock or randomness. `ConversationState` is itself already a pure
reduction of committed journal events (`Journal::replay`), so replaying the
same events through the journal and then through this reducer always
produces a byte-identical `TranscriptView` -- proven directly in
`replaying_the_same_journal_events_twice_yields_an_identical_transcript`,
which feeds one realistic event sequence into two independent SQLite
journals and asserts the resulting transcripts are equal.

`NativePresentation` (scroll/follow, selection, focus, expanded tool calls,
the composer) is a separate struct with no reference to a session, a
journal or a socket. A future pane driver owns the actual `SessionHandle`/
`session::client::Client` and feeds this module two things every render:
a freshly replayed `ConversationState` and a small `StatusFacts` snapshot
(model/route/runtime/billing plus the runtime's own `SessionState`/
`TurnState` and an approval-pending flag). Everything else -- what the
transcript looks like, what Enter does right now, whether a tool update is
allowed to yank the viewport -- is computable from those two inputs alone,
which is what makes the whole surface testable without a terminal, a
runtime or a filesystem (the one filesystem-touching piece,
draft/queued-input persistence, is isolated to `load_draft`/`persist_draft`
and exercised against a temp `StateDir`).

`render_native_pane` (ratatui) and `render_plain` (a plain string) are both
thin walks over the same `render_lines`/`wrap_all`/`composer_lines`
building blocks, so item 7's requirement -- native chat usable without
ratatui -- is not a second implementation to keep in sync, only a second
`Frame`-free entry point.

### Classification lives in the presentation layer, not the journal

The journal stores an `ExecutionState` and a `ContentRef` (inline text or a
content-addressed artifact); it never records "this was a diff" or "this was
a test run". Teaching it to would mean a schema change and a second opinion
about semantics N03 deliberately kept out of the durable record. Instead
`classify_outcome`/`classify_completed` apply a presentation-only heuristic:
a `ContentRef::Artifact` is always an artifact link; inline text with a
`---`/`+++` header pair AND a `@@` hunk marker is a diff; inline text from a
tool whose name contains `"test"` is parsed for a `"<N> passed"`/
`"<N> failed"` pair (kept alongside the raw text either way, since the parse
is best-effort); everything else is plain text. A tool this build has never
heard of still renders sensibly -- classification never depends on a fixed
tool registry.

### Scroll position is item-based, not line-based

`ScrollState.items_back` counts whole transcript items back from the newest,
not wrapped display lines. A resize changes how many lines an item wraps to
at the new width, but never which items exist -- so keeping the "how far
back" measurement in item units makes a resize a pure re-slice of the same
underlying content (`render_lines` -> `wrap_all` -> `viewport_slice`, all
three cheap and re-run every frame) instead of a scroll-position
recomputation that has to guess how the old wrapped-line offset maps onto
new wrapping.

`ScrollState::on_items_appended` is the one rule the "tool updates never
force-scroll a user who scrolled up" acceptance criterion asks for:
appending items while `follow` is off grows `items_back` by the same
amount, so the exact same absolute window of items stays on screen; while
`follow` is on it is a no-op, since the renderer always shows the tail
regardless of `items_back`. Both directions are covered by
`a_tool_update_never_force_scrolls_an_operator_who_scrolled_up` and
`appending_items_while_following_does_not_change_items_back`.

### Queued input can never become an approval answer

`classify_submit_intent(&StatusFacts) -> SubmitIntent` (`Immediate` /
`Steer` / `Queue`) is a function of `StatusFacts` alone, never of the
composer's own state -- "what does Enter do right now" is answerable
without inspecting the draft. `blocked` (an approval or other policy gate
outstanding) always yields `Queue`, unconditionally, before any
`SessionState`/`TurnState` branch runs. Queued input is persisted
(`PersistedDraft`, under `StateDir::native_panes()`) so it survives a
reconnect/resume, but nothing in this module ever turns queued composer
text into an approval decision -- an approval has its own explicit control
(the dashboard's Approval dialog) that this module does not implement and
does not read from. `submit_intent_never_queues_as_an_approval_and_is_queue_
while_blocked` pins this.

### Markdown, wrapping, paste: no new crate

`markdown_lines` is a line classifier (headings, `-`/`*`/numbered lists,
fenced code blocks, inline code spans), not a CommonMark parser -- no
Markdown crate is a dependency today and this issue's own diff budget does
not justify adding one for four line shapes. `wrap_line` is a hand-written
greedy word wrap using `style::display_width` (already how this codebase
measures CJK/emoji-correct column width) with a hard-split fallback for a
single token wider than the pane. An early version of that fallback looped
forever (and OOMed) at `width == 1` against a double-width character, since
neither branch made forward progress when even a fresh line could not fit
one character; the fix (`wrap_line_never_splits_an_emoji`, a regression
test at every width 1..20) forces exactly one character through in that one
case, accepting a bounded one-glyph overflow rather than an unbounded loop
-- there is no legal way to split a codepoint, so *some* overflow at
degenerate widths is unavoidable, but it must be bounded and it must
terminate.

Paste has two paths: a terminal that sends a real bracketed-paste event
hands its text straight to `ComposerAction::InsertText` (crossterm's own
`Event::Paste`, not reimplemented here); a terminal that does not is
covered by `coalesce_paste_chunks`, a pure function over recorded
(text, gap) pairs that a live input loop would drive from real keystroke
timing. `InsertText` normalizes `\r\n`/`\r` to `\n` before inserting --
Windows clipboard/terminal paste routinely carries CRLF, and every other
multiline convention in this module (`resolve_file_refs`, `wrap_line`,
`render_item`) assumes `\n`-only line endings.

## What is verified

- `build_transcript` is a pure, deterministic reduction of
  `journal::ConversationState`, proven against both hand-built fixtures and
  a real two-journal replay (`replaying_the_same_journal_events_twice_
  yields_an_identical_transcript`).
- Tool-outcome classification: every `ExecutionState`, diff detection
  (positive and a header-without-hunk negative), test-count parsing, and
  artifact recognition independent of tool name.
- All seven named states (`generating`/`executing`/`waiting`/`blocked`/
  `cancelled`/`failed`/`completed-with-unread-result`) plus the natural
  eighth (`completed`, read), with `blocked` proven to outrank every other
  runtime state.
- The composer's documented key contract (Enter submits; Shift+Enter/
  Alt+Enter newline; Up/Down browse history only at the first/last logical
  line, otherwise move the cursor), history stash/restore, a 10,000-byte
  paste inserted as one block, CRLF normalization, and `@path` resolution
  against a real temp directory (existing, missing, and a Unicode path).
- Follow-mode scroll: disengage on scroll-up, re-engage at the bottom, and
  the no-force-scroll property itself.
- Draft/queued-input persistence: round-trip, tolerance of a missing or
  corrupt file, and that scroll/selection/focus/expanded state is never
  written to disk.
- Rendering: CJK double-width columns, an emoji never split across a wrap
  boundary (with the width=1 pathological case bounded rather than
  crashing), a 40-column narrow pane, a hard-split of a 100-character
  unbroken token, and `render_plain`'s output being stable across repeated
  calls with the same inputs (the replay-determinism property, restated at
  the rendering layer).

53 tests, all under `dash::native_pane::tests`, none requiring a real
terminal, a live runtime, or a network call.

## What is deferred

- **Wiring into `dash::mod`'s live event loop.** No `PaneKind` enum exists
  on `dash::pane::Pane` yet, and `Vec<Pane>` is not touched. A native pane
  is not spawnable from the dashboard today, does not appear in the
  sidebar, and does not participate in the mail sweep, budget accounting,
  attention projection or restore roster. This is the largest deferred
  piece, and deliberately so: those systems are deeply PTY-shaped (the mail
  sweep types text into a child's stdin; budget accounting reads transcript
  usage off a `vt100::Screen`; the restore roster relaunches a child
  process), and retrofitting all of them in the same change as the view
  model itself would make this diff both far larger and far harder to
  review independently of the rendering work. The roadmap's own acceptance
  criteria agree: "this issue provides the initial usable view; integrated
  multi-agent attention and rollover UX is completed in N21."
- **A live keyboard/paste event loop.** `key_to_action` and
  `coalesce_paste_chunks` are pure functions a real `crossterm::event::read`
  loop would drive; no such loop exists yet for a native pane specifically
  (the wrapped-pane loop in `dash::mod` is untouched, per scope).
- **`zirv ctx exec --runtime native`'s own output.** It still prints only
  the final JSON status (`run_headless`); this issue does not change that
  contract. `render_plain` is available for a future interactive
  non-ratatui surface and for tests, but nothing wires it into the exec
  path today.
- **Real approval-pending / billing-class facts.** `StatusFacts` is a plain
  struct a driver assembles; this issue does not add the plumbing that
  reads a live approval gate or resolves a route's `provider::BillingClass`
  at render time (that resolution already exists at session-start,
  `provider::inventory::RouteReport`, but nothing here calls it).
- **Text selection/copy.** `NativePresentation::selection` is a
  `(start, end)` item-index pair with `set_selection`/`clear_selection`,
  proven to survive a render call at a different width, but no copy
  mechanism reads it.
- No `#[cfg(unix)]` code was added or changed; nothing here touches
  `wrap.rs`, raw-mode handling, or PTY input routing.
