# Dash refresh for wrapped sessions, motion, and intake

Approved 2026-09-26. Acceptance target: `docs/design/mocks/2026-09-26-dash-refresh.html`
(artifact https://claude.ai/artifact/FySY9EquyheXPnCBXfToqm, version 5). Where this
text and the mock disagree, the mock wins on look, this text wins on behaviour.

Operator decisions: session column 28 columns; every intake plan waits for Enter (no
countdown); motion fully on by default; dashboard panes are not auto-compacted or
restarted (the footer recommends instead); only enabled, available harnesses and an
enabled Jev appear anywhere.

Shipped as three PRs: PR1 dashboard layout, limits, workflow binding; PR2 rot track,
rollover, Jev and motion (stacked on PR1); PR3 intake (#806).

## PR1 -- dashboard layout, limits, workflow binding

### Session column

- `dash.sidebar_cols` default 44 -> 28 (`config.rs`, its doc comment, README).
- Row contract at 28 columns:
  `tree(1) glyph(1) ' ' name(10) ' ' harness(6) ' ' rot(3) ' ' badge(2) ' '`.
  Extra width from a larger `dash.sidebar_cols` widens `name`. `name` is `orch` for the
  orchestrator, otherwise the 8-char short id. `harness` is the adapter name, cut to 6.
  `rot` stays `✻NN` coloured by band, `✻ –` when unknown.
- Tree prefix is one column (`├`, `└`, `│` continuation) instead of two.
- Badge column (2 cols), highest priority first: `⚑` workflow gate awaiting approval,
  `✉N` unread mail (`✉+` above 9). PR2 adds lifecycle badges above mail.
- Group header: `▾ {scope}` left, rollup counts right-aligned to the badge column.
- Selected row: subtle background (`Color::Indexed(236)`), glyph colours kept.
  REVERSED is no longer used for the selection.
- Title row ` SESSIONS` with the count right-aligned, then a `─` rule. The rule meets the
  divider with `┼`, aligned with the pane header rule (see mock).

### Fact block under the selected row

Replaces the 8-line disclosure. At most 2 lines, indented under the tree:
1. `{state word} · {since}` (the existing reason/since facts).
2. `▸ {workflow} › {step} {i}/{n}` only when the session has a bound workflow.

Delete the group, model, budget, branch, writer and signal lines and any facts/caches
that only fed them.

### Pane header

One row plus a `─` rule at the top of the main area for the focused pane:
- left ` {harness} ▸ {role} · {model} · {cwd, ~-shortened}`
- right `▸ {workflow} › {step} {i}/{n}  {state glyph} {State word} {age}`; the workflow
  segment is absent without a bound workflow.
- Truncate left before right. The grid loses these two rows; the child PTY size must
  follow the new grid rect exactly (no off-by-one, no resize storm).

### Limits

A `LIMITS` section pinned to the bottom of the session column: per harness with usage
data, two rows per window (5h, wk):

```
 claude 5h ▰▰▱▱▱▱  41%
   resets 16:20 · in 2h13m
        wk ▰▱▱▱▱▱  12%
   resets Thu 09:00
```

- Only harnesses where `settings::AgentGate::is_enabled` is true appear.
- Reset text in local time: today `HH:MM · in XhYYm` (`in Nm` under an hour); within 7
  days `Ddd HH:MM`; later `D Mon`. `~` before the time when the value is zirv's estimate
  rather than vendor-reported.
- `pct >= 80` yellow; `limit_reached` red `back at HH:MM · Nm`; `overage_covered`
  magenta `credits until HH:MM`.
- Data: `window::Window` already stores `resets_at`, `limit_reached`, `overage_covered`;
  `dash/mod.rs` (~2603-2622) drops them when building `ui::HarnessUsage`. Carry them
  through. No new I/O.
- Session rows win vertical space: when rows would collide, drop the limits block from
  the bottom a whole window at a time.

### Footer

- Remove the spend segment (`render_footer_spend`) and the aggregate spend row
  (`render_aggregate_row`) and their facts if nothing else uses them. The `^A i`
  inspector and `zirv ctx status` keep spend.
- Remove the healthy `● supervised` segment; keep `▲ unsupervised` and `◆ stalled`.
- The workflow segment moves to the pane header; remove it from the footer.
- Footer keeps `✻ NN {band}` and `✉ N`. PR2 extends it.

### Narrow terminals

Below 100 columns the session column hides; the header shows sessions as tabs
(` glyph name badge `, focused tab on the selection background). The key that toggles
the column (reuse an existing sidebar toggle if one exists, else `^A b`) brings it back.
The footer then shows the focused harness usage: `5h 41% · resets 16:20`.

### Workflow per session

Today the dash shows `engine::load_active` (one repo-wide pointer) on every pane, and a
completed run can leave `current_step` out of range, which renders an empty step.

- Add `workflow_id: Option<String>` (serde default) to `sessions::Record`.
- Bind it when a workflow starts: in `zirv workflow start` when `ZIRV_CTX_SESSION`
  identifies the calling session, and in `chat.rs` when the intake starts one for the
  session it launches.
- The dash resolves each pane's workflow from its own record by id: kind, current step,
  1-based index, total, awaiting approval, completed.
- Completed runs show `▸ {kind} › ✓ done` for 10 minutes after completion, then nothing.
- No bound workflow: show nothing. Never fall back to the repo pointer.

## PR2 -- rot track, rollover, Jev, motion (stacked on PR1)

Operator decision (option B): dashboard panes are NOT compacted or restarted
automatically. The dash receives the hook's forwarded verdict on the pane's turn-signal
socket and only uses it as a turn-ended marker (`dash/pane.rs` `on_turn_signal`); that
stays. Only rollover acts automatically (`orchestrator_seat_rollover` ->
`rollover::evaluate`, dash/mod.rs ~4253). The indicator must never claim an action
zirv will not take.

### Footer rot track

`✻ NN {band}` + a 22-column track: 20 cells of 5 points, `┊` ticks before the cell at
`score.advise_at` (default 40, warming) and `score.compact_at` (default 60, rotting),
cells coloured by band. Words after the track only when useful:
- score >= `score.compact_at`: `/compact recommended` (yellow)
- score >= `score.restart_at` (default 80): `fresh session recommended` (red)
Score source is unchanged (`score::cached_score`). No context-token or verdict plumbing.

### Rollover (orchestrator seat only)

Right side of the footer when the focused pane is the dash's orchestrator seat:
- distance: `⤓ rollover at {floor}% left · now {headroom}%` (muted), floor =
  `fallback.rollover_headroom_pct()`, headroom = the projected headroom the dash already
  computes for `rollover::evaluate`
- soon (headroom within 10 points of the floor): yellow `⤓ rollover soon · N% left (at F%)`
- pending (`seat::Phase::Pending` in `<short>.seat.json`): bold yellow, breathing,
  `⤓ switching to {harness} when idle` (verify the wording against when the swap
  actually fires)
- parked (`rollover_runtime::Record` `Settlement::Parked{until}` in
  `<short>.rollover.json`): magenta `⏸ parked until {harness} resets at HH:MM · Nm`
  (local time, same formatter as LIMITS)
- hidden when `fallback.enabled` is false or auto rollover is off.
Sidebar badge on the seat's row: `⤓` pending, `⏸` parked, above `✉` in priority.
Read the two small JSON files per seat on the facts refresh (1 s), never per frame.

### Toasts

A toast slot in the pane header (between the left facts and the workflow segment),
shown 5 s then faded: rollover committed (settlement change in `<short>.rollover.json`)
`⏺ ⤓ rolled over from {harness} · gen N`; a worker session finished (existing
done-unread transition) `⏺ {short} finished`. At most one visible; newest wins.

### Jev section

Sidebar section between SESSIONS and LIMITS (mock section 03):
```
 JEV               24h · on
────────────────────────────
 calls   739 · 82% cached
 wait    p95 670 ms
 errors  0
 last    approve · 2s ago
 memory    417 ▰▰▰▰▰▰
 approve   240 ▰▰▰▱▱▱
 classify   38 ▰▱▱▱▱▱
```
- Source: `jev-decisions.jsonl` / `jev-effects.jsonl` via `jev::usage_rollup`, with the
  window as a parameter (24 h here; `zirv ctx jev status` keeps 7 d). Top three sites by
  calls; bars relative to the busiest. Refresh at most every 10 s, off the render path.
- Errors > 0: red `N · {latest reason in plain words}`.
- Hidden entirely when no Jev gate is enabled. Gates enabled but credential missing: one
  yellow line `JEV  no key` plus `set {credential env}`.
- No spend. Numbers are machine-wide (records carry no session id).
- Vertical priority: sessions, then LIMITS, then JEV; drop JEV's site rows first, then
  the section, when rows run out.

### Enabled harnesses only

Anything per harness (LIMITS, rollover target names, toasts) shows only harnesses for
which `settings::AgentGate::is_enabled` is true (PR1 already does this for LIMITS).

### Motion

- Spinners on a clock (80 ms per frame from elapsed time), not per redraw.
- Shimmer across the pane-header working word (Claude Code style, ~90 ms step).
- Gauges (rot track, limit bars, Jev bars) ease to new values over ~300 ms.
- Pending rollover breathes (bold/normal or colour ramp, ~1.6 s period).
- A row flashes once (~900 ms background fade) on new mail or a finished worker; the Jev
  `last` line flashes on a new call.
- Toasts fade out.
- `dash.motion = "full" | "reduced"`, default `full`; reduced keeps state changes and
  drops spinner movement, shimmer, easing, breathing, flashes and fades. README + config
  docs. The redraw cadence must not rise above today's idle rate just for motion when
  nothing animates.

## PR3 -- intake plan card

Replaces the stderr lines in `chat.rs::proxy_intake` with an inline ratatui region
(`Viewport::Inline`) on the normal screen, before the harness starts (keeps the #701
scrollback fix). Frames are in the mock, section 06.

1. Prompt: rounded box with `> `; hint line `⏎ plan and start · shift+⏎ new line ·
   esc start without a plan`. Alt+Enter and Ctrl+J also insert a newline.
2. Sizing: `✻ Sizing the task… Ns · esc to start without a plan` with a clock-driven
   spinner; `proxy::decide` runs off the UI thread; Esc abandons it.
3. Clarify (only when the decision asks): box titled `One question`, reworded canned
   question, `⏎ answer · tab skip and plan anyway`.
4. Plan card: one plain sentence (size + area + risk), then `Model` or `Lead`/`Helpers`,
   `Workflow {kind} · N steps, starting at {first step}` or `none`, `Why`; numbered
   choices `1. Start`, `2. Start with a different model`, `3. Start without a
   workflow`, `4. Edit the task`. `↑↓`/digits choose, Enter confirms, Esc starts
   without a plan. Every plan waits for Enter.
5. The workflow starts only after the user confirms 1 or 2 (today it starts before the
   user sees anything); choice 3 starts none.
6. When the configured decider failed or timed out, a yellow `⚠` line says so and names
   the local-rules fallback, with `zirv ctx proxy --json` for details.
7. On confirm the region clears and one line stays in scrollback:
   `✻ zirv planned in 3s · Sonnet 5 alone · bugfix workflow w-3f2a`.
8. No seat tiers, decider names, domain scores or confidence numbers on screen.
