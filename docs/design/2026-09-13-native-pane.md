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

## Round 2: a real, driven, launchable pane

The first round shipped the view model with nothing driving it. The
roadmap's own acceptance criteria are explicit that this issue owes more
than that -- "a native session can complete a real read/edit/test
conversation from the TUI" needs an actual session behind the view, opened
from an actual command. Round 2 adds that, while keeping the change
reviewable against a codebase whose existing wrapped-harness dashboard
(`dash::mod::run_dashboard`) is a single ~3,000-line function threading
`Vec<Pane>` through roughly a hundred call sites (spawn, mail sweep, budget
accounting, attention projection, restore roster).

### `runtime::native::spawn_interactive`: one submit, many turns

Every existing native entry point (`run_headless`/`run_session`,
`native_worker::run`) runs ONE submitted prompt to completion and exits --
right for a headless run or a delegated worker, wrong for a pane an
operator keeps typing into. `spawn_interactive` resolves transport/journal/
seat/writer exactly like `run_session` (same `build_transport`, same
`NativeBackend`/`NativeLoop`, same writer-permit acquisition as
`native_worker.rs`'s `WorkerMode::Writing`), then hands the whole session to
a background OS thread that constructs a **fresh** `NativeLoop` and calls
`run_to_completion` once per item received on an `mpsc::Sender<String>`,
instead of once total. `ToolExecutor` gained a `Send` bound (`ProviderAdapter`
already had one) so a `Box<dyn ToolExecutor>` can move into that thread --
every real implementor already was `Send`; this is a bound addition, not a
behaviour change.

Three separate paths, not one multiplexed command channel:

- **Submit** (idle -> a fresh turn) goes through the channel -- it is the
  only thing that starts a new `run_to_completion` call.
- **Steer** (input during a turn already in flight) does **not** go through
  the channel at all. The worker thread is synchronously blocked inside
  `run_to_completion` while a turn runs, so a channel message would just
  queue until that call returns -- too late to matter as "steering". Instead
  the caller (`NativePaneRuntime::write_steering`) commits the input
  straight to the journal, using its own separate `Journal` handle on the
  same SQLite file (WAL mode; `journal.rs`'s own doc comment already
  documents concurrent readers, and a second writer's committed transaction
  is visible the same way). `NativeLoop::queued_input` already re-reads the
  journal for exactly this between requests inside a turn and between
  turns, so the running loop picks up the steer without either side
  coordinating directly.
- **Interrupt** bypasses the channel too, via the `Arc<CancellationFlag>`
  `NativeBackend::cancellation` already hands out for "a caller that drives
  a `NativeLoop` itself" -- `InteractiveSession::interrupt` just calls
  `.cancel()` on the shared flag from whichever thread the operator's
  keypress landed on.

`InteractiveProgress` (the worker -> caller direction) is deliberately
coarse (`Busy`/`Idle`/`Failed`/`Ended`): the pane never learns a turn's
*content* from this channel, only when a re-read of the journal is worth
doing. Today's granularity maps `Busy` to a blanket
`PresentationStatus::Generating` -- the finer `Requesting` vs
`ExecutingTools` distinction item 4 names needs a live protocol-event
stream this round does not add (see "still deferred" below).

### `dash::native_pane::run_native_dashboard`: its own dashboard mode

Rather than adding a `PaneKind` to `dash::mod`'s existing `Vec<Pane>` --
which would mean touching every one of those ~100 call sites, none of which
this issue's own acceptance criteria require changing, and doing so
concurrently with N20's own in-flight rewrite of the dashboard's ownership
seam -- `zirv chat --runtime native` opens a **separate**, additional,
single-pane dashboard entry point. It reuses `dash::mod`'s existing
terminal-setup/teardown helpers **verbatim**
(`install_panic_hook`/`enable_raw_mode`/`EnterAlternateScreen`/
`push_keyboard_enhancement`/`teardown_terminal`/`restore_panic_hook` --
copied nowhere, called directly via `super::`) so there is exactly one
place in this codebase that enters or leaves raw mode and the alternate
screen, and the wrapped-harness dashboard's existing loop, and everything
it threads `Vec<Pane>` through, is completely untouched.

`NativePaneRuntime` owns the one live thing this module now has: an
`InteractiveSession`, a second `Journal` handle for reads and steering
writes, and the already-tested presentation state. `tick()` drains progress
and re-replays the journal every dashboard frame (cheap: a
`try_recv` loop plus one SQLite read); `handle_composer_action` is the one
path a keypress reaches the session through, applying the composer action
and then, only on a `Submit`, consulting `classify_submit_intent` to decide
Immediate/Steer/Queue -- so a blocked submission is held in
`composer.queued`, never sent as an approval answer, exactly as round 1's
own `submit_intent_never_queues_as_an_approval_and_is_queue_while_blocked`
already proved for the classifier in isolation.

### `--view plain` on the existing headless path

`zirv ctx exec --runtime native --view json` (the default) is byte-for-byte
what `run_headless` always printed -- the flag changes nothing about that
contract. `--view plain` calls the lower-level `run_session` directly (the
same function `run_headless` itself calls), prints the identical JSON, then
opens the session's own journal fresh and renders it through
`dash::native_pane::render_plain` -- the exact reducer
(`build_transcript`) and renderer the dashboard pane draws through, so a
headless transcript and a live pane's transcript can never disagree about
what a tool call, a diff or a test outcome looks like.

## Round 3: Claude Code-style restyle (operator direction, PR #531 follow-up)

The operator asked for the pane to look and behave like Claude Code's own
interactive UI, on the reasoning that it is the harness UX this project's
users already know. This round restyles the renderers and extends the key
contract; it deliberately keeps round 1's reducer/composer/presentation
model untouched (no `TranscriptItem`/`ConversationState` shape changed
except one new, purely presentational `TranscriptItem::Elided` variant from
the review-fix round earlier on this branch) -- only how things are drawn
and which keys reach the pane changed.

### Shipped

- **Bullet/tree transcript.** `render_item`/`render_tool_call` now render
  assistant text and tool calls as `⏺` bullets (`with_marker`, shared by
  both), user turns as `>` lines, and a tool call's result as an indented
  `⎿` tree line carrying a one-line summary plus `(ctrl+r to expand)` while
  collapsed (dropped once actually expanded, and never shown at all for a
  pending/running/cancelled outcome, which has nothing more to reveal --
  `outcome_is_expandable`). A per-line shaded background for user turns is
  **not** implemented: the shared `StyledSpan`/`Tone` vocabulary both
  renderers (ratatui and plain-text) use has no per-line background concept
  today, and adding one is a crate-wide change to `style::Tone` (used well
  outside this module) rather than a native-pane-scoped one.
- **Diffs with a line-number gutter.** `render_diff_lines` parses each
  `@@ -a,b +c,d @@` hunk header and threads running old/new counters through
  context/added/removed rows, still colouring +/- rows the way round 1
  already did.
- **The activity line.** `activity_line_text(elapsed, tokens)` is a pure
  function producing a spinner frame, a rotating (decorative) verb, real
  elapsed seconds and a running token count, ending with "esc to interrupt".
  `NativePaneRuntime` tracks `turn_started_at` (set on the first `Busy`
  progress tick of a turn, cleared on `Idle`/`Failed`/`Ended`) and appends
  the line to the transcript's own content via `render_lines_with_activity`
  (not a separately reserved row, so it scrolls/wraps like everything else
  and follow-mode keeps it in view for free) rather than a fixed status-bar
  slot. The token count is the conversation's own total recorded usage so
  far, not a per-turn count -- the journal has no "usage recorded since this
  turn started" read, so it only grows across turns within one pane's
  lifetime rather than resetting each turn; a truer per-turn reading would
  need that journal capability.
- **The bottom status line.** `StatusFacts` gained `context_left_pct`,
  `cwd` and `git_branch`, appended after the existing model/route/runtime/
  billing/state segment. `context_left_pct` is an ESTIMATE --
  `context_left_pct` divides the conversation's own recorded
  input+output token usage by the route's declared context window
  (`provider::capability::declared`); it is not the compaction budget's own
  accounting, which also weighs distillation and lives inside the worker
  thread's `NativeSessionConfig`, never read back by this pane. `git_branch`
  reads `.git/HEAD` directly (resolving a linked worktree's `gitdir:`
  redirect) rather than shelling out to `git` -- this pane polls on a
  ~150ms tick, and spawning a process that often is not acceptable -- read
  once at spawn time, never per-tick, since a session's checked-out branch
  essentially never changes across its own lifetime.
- **The composer's hint line.** Extended (not replaced) to lead with
  "? for shortcuts", show the `Shift+Tab`-cycled `ComposerMode` label and,
  when non-zero, a queued-input count (`composer_hint_line`). The `>`
  prompt marker on the composer's first line already existed from round 1
  and is unchanged.
- **`ComposerMode` (`Shift+Tab`).** `Default`/`AcceptEdits`/`Plan`, cycled
  by `Shift+Tab` (or `BackTab`, which is what most terminals actually report
  for it) and shown on the hint line. **Decorative only**: no submit path
  reads it back to change approval or tool-write behaviour. An
  `AcceptEdits`/`Plan` mode that actually gated the execution broker would
  be a policy change at the enforcement layer -- out of scope for a
  rendering/key-contract pass, and a materially larger, separately
  reviewable change.
- **Key contract.** `Esc` now interrupts (Claude Code's own convention);
  `Ctrl+C` no longer interrupts by itself -- a single press only arms a
  quit confirmation (`ctrl_c_confirms_quit`), and the pane quits on a
  SECOND `Ctrl+C` within `CTRL_C_QUIT_WINDOW` (2s) of the first. `Ctrl+Q`
  still quits immediately, kept for backward compatibility with round 2's
  own key contract. `Ctrl+R` toggles the most recently rendered tool call's
  expanded state regardless of which region has focus (the composer's own
  `e`/`Enter`-while-`Transcript`-focused binding still works too, factored
  into the same `toggle_most_recent_tool_call` helper so the two never
  drift).
- **`/` slash commands -- real, not a mock.** `apply_slash_command`
  intercepts a `/`-prefixed submission before it ever reaches
  `classify_submit_intent`. `/clear` has a real effect (drops the queued
  backlog); `/help` reports the key contract; `/status` (handled directly
  by `NativePaneRuntime::handle_composer_action`, since it needs live
  `StatusFacts` the pure helper cannot produce) reports model/state/
  billing; `/compact` is an honest inert stub -- its own notice says so --
  rather than a command that looks wired but does nothing. An unrecognised
  `/`-prefixed line, or ordinary text, falls through to the normal submit
  path unchanged.

### Deferred (this round)

- **A live approval dialog.** `StatusFacts.blocked` is still hardcoded
  `false` (unchanged from round 2's own deferred item) -- nothing here reads
  the enforcement broker's own approval-gate state, and no numbered dialog
  was added. Rendering one without a real decision to route it to would be
  UI that looks live but is not; wiring the actual decision needs the
  broker/approval-authority seam this pane does not touch anywhere else
  either.
- **An interactive `@` fuzzy file picker.** `resolve_file_refs` (tested,
  containment-safe since the PR #531 review-finding-2 fix earlier on this
  branch) is still not called from `run_native_dashboard`'s own key
  handling -- unchanged from round 2's own deferred item. A live `@`-hint
  overlay needs a picker widget (selection state, filtering, rendering) this
  round did not build.
- **A `!`-prefixed shell line.** The interactive session has no direct-exec
  path that bypasses a model turn -- every submission today becomes a turn
  `NativeLoop` drives. Adding one is an architecture change (a new command
  on `InteractiveSession`, threaded through the worker thread and the
  broker's own process tool) rather than a rendering/key-contract change,
  and risks the exact kind of un-brokered write path issue #480's own
  enforcement model exists to prevent if done hastily.
- **A real bordered composer box.** The composer is still `>`-marker text
  plus a hint line (round 1's own layout), not a `ratatui::widgets::Block`
  with drawn borders. Adding one changes `composer_height`'s own row
  accounting, which several existing layout tests pin; doing that safely
  needs its own reviewable pass rather than folding it into an already
  large rendering change.
- **The visual mock (`docs/design/mocks/2026-09-13-native-pane.html`) was
  not regenerated for this round.** It still shows round 1/2's own look. A
  faithful three-size, current-vs-proposed redraw matching everything above
  is real design work in its own right; shipping the CODE unreviewed
  against a stale mock was judged the lesser risk than either skipping the
  code or rushing a mock that misrepresents what actually renders. Whoever
  picks this back up should regenerate the mock from the shipped renderer
  behaviour (or, better, from a screenshot of the actual pane) rather than
  hand-drawing it again from the operator's prose brief.

## What is still deferred

- **Mixing a native pane into the wrapped-harness dashboard.** No
  `PaneKind` was added to `dash::pane::Pane` or `dash::mod`'s `Vec<Pane>`.
  `zirv chat --runtime native` is a genuinely separate, working, launchable
  dashboard mode -- not a stand-in -- but an operator cannot today open one
  native pane and one wrapped pane side by side in the same dashboard
  process the way the issue's own mock (`docs/design/mocks/2026-09-13-
  native-pane.html`) shows. That needs the `Vec<Pane>` retrofit round 1's
  own note already scoped out, now additionally coordinated with N20's
  concurrent ownership-seam rewrite (`session::client` becoming the
  dashboard's own transport when the persistent gate is on) rather than
  raced against it.
- **The persistent-runtime (`session::client`) path.** The brief asks for
  `session::client.rs` to drive the pane when `[session] persistent` is on,
  falling back to the in-process `NativeLoop` worker thread otherwise. Only
  the fallback is implemented; `run_native_dashboard` always uses
  `spawn_interactive`'s in-process thread regardless of the persistent
  gate. `session::client`'s own attach surface
  (`client::attach_terminal`) is built around a single PTY-shaped
  session, not a structured event stream, and reconciling that with this
  view model is exactly the kind of ownership-seam question N20 owns;
  wiring it here first risked the two conflicting rather than merging
  cleanly, which the brief asked to avoid.
- **Fine-grained turn state.** `StatusFacts.turn_state` is `Some(Requesting)`
  for any `Busy` progress tick and `None` otherwise -- there is no live
  distinction between "waiting on the provider" and "running a tool" without
  a finer event stream than `InteractiveProgress` carries.
- **Live approval-pending / a real approval control.** `StatusFacts.blocked`
  is hardcoded `false`; nothing here reads the enforcement broker's own
  approval-gate state, and there is no approval dialog in
  `run_native_dashboard`'s minimal loop. A session whose tools need an
  approval this build cannot yet grant will simply stall.
- **`@path` hints, text selection/copy, explicit history keys, paste
  coalescing.** `resolve_file_refs`, `NativePresentation::set_selection`/
  `clear_selection`, and `ComposerAction::HistoryUp`/`HistoryDown` are
  implemented and unit-tested (round 1) but not called from
  `run_native_dashboard`'s own key handling -- showing a live `@`-hint line
  needs `composer_lines` to take a workdir; a copy mechanism needs
  something to copy into; explicit history keys are redundant with
  `MoveUp`/`MoveDown`'s own cursor-position rule today.
  `coalesce_paste_chunks` is unused because `Event::Paste` (bracketed
  paste) already covers the one paste path this loop exercises.
- **Task cards, group ownership, mail-to-a-native-pane.** A dashboard-
  opened native pane is a plain orchestrator session: no `task`, no
  `--writer` distinction beyond "always writing", and nothing here teaches
  the existing mail sweep (which types into a wrapped pane's PTY) to reach
  a native pane's composer instead -- there is exactly one native pane per
  process today, opened directly, never through a spawn request.
- No `#[cfg(unix)]` code was added or changed in either round; nothing here
  touches `wrap.rs`, raw-mode handling, or a wrapped pane's PTY input
  routing.
