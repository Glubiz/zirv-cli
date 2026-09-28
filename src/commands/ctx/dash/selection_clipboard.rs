//! Terminal text selection and OSC 52 clipboard copy.
use super::*;

/// Informational notices expire; sticky errors remain visible after a notice clears.
pub(super) struct Notice {
    pub(super) text: String,
    expires_at: Instant,
}

pub(super) fn push_notice(notices: &mut Vec<Notice>, now: Instant, text: String) {
    notices.push(Notice {
        text,
        expires_at: now + NOTICE_TTL,
    });
    if notices.len() > MAX_KEPT_ERRORS {
        let drop = notices.len() - MAX_KEPT_ERRORS;
        notices.drain(0..drop);
    }
}

/// Pure: what the header says about a scroll that just happened.
///
/// Every scroll gets one, including the ones that moved nothing: total silence
/// on a scroll that did not scroll is precisely how "the chat window is still
/// not scrollable" was reported twice with nothing to go on. A notice expires
/// on its own after [`NOTICE_TTL`], so this is the transient channel and never
/// the sticky `⚠` error line -- a scroll that stops at the top of the history
/// is not a failure.
pub(super) fn scroll_notice(outcome: ScrollOutcome) -> String {
    match outcome {
        ScrollOutcome::Scrolled(0) => "back to the live view".to_string(),
        ScrollOutcome::Scrolled(rows) => format!("scrolled back {rows} line(s)"),
        ScrollOutcome::AtOldest => "already at the oldest line".to_string(),
        ScrollOutcome::AtLive => "already at the live view".to_string(),
        ScrollOutcome::ForwardedMouse => {
            "pane is in full-screen mode -- scrolling is forwarded to the app".to_string()
        }
        ScrollOutcome::FullScreen => {
            "pane is in full-screen mode -- the app scrolls itself (wheel, or unprefixed PageUp)"
                .to_string()
        }
    }
}

/// Pure: crossterm's button enum as the xterm protocol's own button number.
/// Left/middle/right are 0/1/2 in every encoding, the same numbering the wheel
/// extends with 64/65.
pub(super) const fn mouse_button_code(button: MouseButton) -> u8 {
    match button {
        MouseButton::Left => 0,
        MouseButton::Middle => 1,
        MouseButton::Right => 2,
    }
}

/// Pure: a frame-relative mouse position translated into the pane-local,
/// 1-based coordinates a child's own mouse reports are written in.
///
/// The child believes it owns a terminal whose top-left is its own, so a frame
/// coordinate handed straight through would make it act on the wrong row --
/// worse than not scrolling at all, and `area.x` is genuinely non-zero
/// whenever the sidebar is drawn. Clamped into the pane as well as translated:
/// the wheel scrolls the focused pane wherever the pointer happens to be
/// (including over the sidebar), so a position outside the grid still has to
/// encode to something inside it.
pub(super) fn pane_local_mouse(area: Rect, column: u16, row: u16) -> (u16, u16) {
    if area.is_empty() {
        return (1, 1);
    }
    let col = column
        .saturating_sub(area.x)
        .min(area.width.saturating_sub(1))
        + 1;
    let row = row
        .saturating_sub(area.y)
        .min(area.height.saturating_sub(1))
        + 1;
    (col, row)
}

/// Return a zero-based visible-grid cell, clamped to both pane and area size to tolerate resize races.
pub(super) fn pane_local_cell(
    area: Rect,
    column: u16,
    row: u16,
    grid_rows: u16,
    grid_cols: u16,
) -> Option<(u16, u16)> {
    if area.is_empty() || grid_rows == 0 || grid_cols == 0 {
        return None;
    }
    let col = column
        .saturating_sub(area.x)
        .min(area.width.saturating_sub(1))
        .min(grid_cols - 1);
    let row = row
        .saturating_sub(area.y)
        .min(area.height.saturating_sub(1))
        .min(grid_rows - 1);
    Some((row, col))
}

/// Pure: a selection's anchor/end pair, ordered so `start <= end` in (row,
/// col) reading order -- tuple comparison is already lexicographic, which is
/// exactly row-major order. `vt100::Screen::contents_between` does not
/// normalize its own arguments (a `start_row` past `end_row` silently
/// returns an empty string), so the caller owns it; `ui::render_grid`'s
/// `cell_in_selection` expects the same already-ordered pair this returns.
pub(super) fn normalize_selection(a: (u16, u16), b: (u16, u16)) -> ((u16, u16), (u16, u16)) {
    if a <= b { (a, b) } else { (b, a) }
}

/// Selection coordinates track the visible grid with signed rows so scrolling can move a selection off-screen and back; pane identity uses a stable short ID (#697).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Selection {
    pub(super) pane_short: String,
    anchor: (i64, u16),
    pub(super) end: (i64, u16),
}

/// Defer a left press until movement distinguishes a drag from a click that must reach the child (#697).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct PendingPress {
    pub(super) pane_short: String,
    /// Frame-relative coordinates of the ORIGINAL press, exactly as
    /// crossterm reported them. Kept in frame space, not yet pane-local, so
    /// both the drag-distance check and the eventual forwarded click
    /// re-derive pane-local coordinates the same way every other mouse path
    /// here already does, rather than caching a second projection that
    /// could quietly drift from it.
    pub(super) column: u16,
    pub(super) row: u16,
    /// The pane-local grid cell the press landed on (`pane_local_cell`),
    /// captured once so a promoted `Selection`'s anchor is exactly where the
    /// button went down, not wherever the drag was first observed to have
    /// moved past the threshold.
    pub(super) anchor_cell: (u16, u16),
}

/// Allow small pointer jitter so an ordinary click still reaches the child (#697).
pub(super) const DRAG_THRESHOLD_CELLS: u16 = 1;

/// Use absolute axis distance so dragging backward cannot underflow; small
/// motion remains a child click rather than a selection.
pub(super) fn past_drag_threshold(from_col: u16, from_row: u16, to_col: u16, to_row: u16) -> bool {
    from_col.abs_diff(to_col) > DRAG_THRESHOLD_CELLS
        || from_row.abs_diff(to_row) > DRAG_THRESHOLD_CELLS
}

/// Anchor a promoted drag at the original press, not where it crossed the threshold.
pub(super) fn promote_pending_drag(pending: PendingPress, end_cell: (u16, u16)) -> Selection {
    Selection {
        pane_short: pending.pane_short,
        anchor: (i64::from(pending.anchor_cell.0), pending.anchor_cell.1),
        end: (i64::from(end_cell.0), end_cell.1),
    }
}

/// Pure: the two pane-local, 1-based coordinate pairs -- `(press, release)`
/// -- a `PendingPress` that reached `Up` without ever crossing
/// `DRAG_THRESHOLD_CELLS` replays as a forwarded click, in exactly the
/// shape `Pane::forward_mouse_button` needs (`pane_local_mouse`'s own doc
/// comment). `main` is the SAME rect both the original press and this
/// release must be interpreted against. Pulled out of the event loop for
/// the same reason `promote_pending_drag` is: a click that never crossed
/// the threshold is provably the coordinate pair this returns, with no live
/// `Pane` needed to check it -- `Pane::forward_mouse_button` is itself the
/// `wants_mouse` gate on what the event loop does with them.
pub(super) fn deferred_click_coords(
    pending: &PendingPress,
    main: Rect,
    release_column: u16,
    release_row: u16,
) -> ((u16, u16), (u16, u16)) {
    (
        pane_local_mouse(main, pending.column, pending.row),
        pane_local_mouse(main, release_column, release_row),
    )
}

/// Dragging beyond the pane's top or bottom edge scrolls history in that direction (#697).
pub(super) fn drag_autoscroll_direction(main: Rect, row: u16) -> Option<isize> {
    if main.is_empty() {
        return None;
    }
    if row < main.y {
        Some(1)
    } else if row >= main.y.saturating_add(main.height) {
        Some(-1)
    } else {
        None
    }
}

/// Shift selection rows by every scroll delta; leave off-screen rows unclamped so scrolling back restores the same cells (#697).
pub(super) fn translate_selection(
    selection: &mut Option<Selection>,
    scrolled_pane_short: &str,
    delta: i64,
) {
    let Some(sel) = selection.as_mut() else {
        return;
    };
    if delta == 0 || sel.pane_short != scrolled_pane_short {
        return;
    }
    sel.anchor.0 += delta;
    sel.end.0 += delta;
}

/// Pure: `sel`'s anchor/end resolved against a grid of `grid_rows` by
/// `grid_cols`, clamped into `0..grid_rows`/`0..grid_cols` and ordered so
/// `start <= end` in reading order (`normalize_selection`). This is the one
/// place a `Selection`'s translated (and possibly out-of-range) coordinates
/// are ever clamped -- both `ui::render_grid`'s highlighting and the
/// extraction on release go through here, so a row `translate_selection` has
/// pushed above the top or past the bottom of the CURRENT view degrades to
/// "clamped to that edge" for both, rather than indexing past the grid or
/// (worse, for extraction) silently reading the wrong row.
pub(super) fn resolve_selection_range(
    sel: &Selection,
    grid_rows: u16,
    grid_cols: u16,
) -> ((u16, u16), (u16, u16)) {
    let clamp_row = |r: i64| -> u16 {
        if grid_rows == 0 {
            0
        } else {
            r.clamp(0, i64::from(grid_rows) - 1) as u16
        }
    };
    let clamp_col = |c: u16| c.min(grid_cols.saturating_sub(1));
    let a = (clamp_row(sel.anchor.0), clamp_col(sel.anchor.1));
    let b = (clamp_row(sel.end.0), clamp_col(sel.end.1));
    normalize_selection(a, b)
}

/// Snapshot only fully visible selected text; a partial off-screen comparison could miss changed rows and copy stale content.
pub(super) fn selection_snapshot(screen: &vt100::Screen, sel: &Selection) -> Option<String> {
    let (rows, cols) = screen.size();
    let fully_visible = [sel.anchor.0, sel.end.0]
        .iter()
        .all(|row| *row >= 0 && *row < i64::from(rows));
    if !fully_visible {
        return None;
    }
    let (start, end) = resolve_selection_range(sel, rows, cols);
    Some(screen.contents_between(start.0, start.1, end.0, end.1))
}

/// Cancel only when output changes selected rows: stale coordinates would
/// otherwise copy text the operator never highlighted.
pub(super) fn output_cancels_selection(selection: &Selection, output_pane_short: &str) -> bool {
    selection.pane_short == output_pane_short
}

/// Pure: whether resizing `resized_pane_short`'s grid from `old_size` to
/// `new_size` (both `(rows, cols)`, `vt100::Screen::size`'s own order) must
/// cancel `selection`. The same invariant as `output_cancels_selection`
/// from the second angle: a resize does not move content, but it does mean
/// the pane's grid this selection's `(row, col)` cells index into is no
/// longer the one they were captured against -- `ui::cell_in_selection`'s
/// middle-row arm would highlight every remaining row of a shrunk grid, and
/// `contents_between` would copy the trailing row in full, if the
/// coordinates were left to point past the new bounds.
pub(super) fn resize_cancels_selection(
    selection: &Selection,
    resized_pane_short: &str,
    old_size: (u16, u16),
    new_size: (u16, u16),
) -> bool {
    old_size != new_size && selection.pane_short == resized_pane_short
}

/// Glue for [`resize_cancels_selection`], shared by every path that resizes
/// a pane's grid -- `apply_terminal_resize` (covering both `Event::Resize`
/// and the per-frame reconciliation) and the `Ctrl+A z` zoom toggle's own
/// inline resize -- so none of the three can independently forget the check.
/// `new_size` is `(rows, cols)`, the size every pane in `panes` is about to
/// be resized to (all three call sites resize every pane to the same
/// geometry); the selected pane's *current* size is read fresh out of its
/// own screen rather than threaded through as a parameter, since the caller
/// has not resized anything yet at the point this runs.
pub(super) fn cancel_selection_on_resize(
    selection: &mut Option<Selection>,
    panes: &[Pane],
    new_size: (u16, u16),
) {
    let Some(sel) = selection.as_ref() else {
        return;
    };
    let stale = panes
        .iter()
        .find(|pane| pane.short() == sel.pane_short)
        .is_some_and(|pane| {
            resize_cancels_selection(sel, pane.short(), pane.screen().size(), new_size)
        });
    if stale {
        *selection = None;
    }
}

/// A bare click has no selection and must not copy text.
pub(super) fn selection_on_release(sel: Selection) -> (Option<Selection>, bool) {
    if sel.end == sel.anchor {
        (None, false)
    } else {
        (Some(sel), true)
    }
}

/// The standard (padded) base64 alphabet, RFC 4648 §4. OSC 52 is the only
/// place this dashboard needs base64, and a ~15-line encoder was not worth a
/// new dependency (or reaching for a transitive one another crate happens to
/// pull in, which is not a contract this code can rely on staying true).
pub(super) const B64_ALPHABET: &[u8; 64] =
    b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// Pure: standard, padded base64 of `bytes`. See [`B64_ALPHABET`] for why
/// this exists instead of a dependency.
pub(super) fn b64_encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b0 = chunk[0];
        let b1 = chunk.get(1).copied().unwrap_or(0);
        let b2 = chunk.get(2).copied().unwrap_or(0);
        out.push(B64_ALPHABET[usize::from(b0 >> 2)] as char);
        out.push(B64_ALPHABET[usize::from(((b0 & 0x03) << 4) | (b1 >> 4))] as char);
        out.push(if chunk.len() > 1 {
            B64_ALPHABET[usize::from(((b1 & 0x0f) << 2) | (b2 >> 6))] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            B64_ALPHABET[usize::from(b2 & 0x3f)] as char
        } else {
            '='
        });
    }
    out
}

/// OSC 52's own practical ceiling here, in bytes of base64 *output*: some
/// terminals cap how much a single OSC 52 write will accept, and this
/// dashboard has no way to ask the host terminal what its own limit is. 64
/// KiB of base64 is about 48 KiB of source text -- generous for anything
/// selected by hand, and cheap insurance against writing something enormous
/// to the host terminal's stdout.
pub(super) const OSC52_MAX_BASE64_BYTES: usize = 64 * 1024;

/// Pure: `text`, truncated on a UTF-8 boundary so its base64 encoding never
/// exceeds [`OSC52_MAX_BASE64_BYTES`]. Base64 expands every 3 raw bytes into
/// 4 output bytes with no partial-group form that stays valid mid-group, so
/// the raw cap is derived from the output cap rather than truncating the
/// already-encoded string after the fact.
pub(super) fn cap_for_osc52(text: &str) -> &str {
    let max_raw = (OSC52_MAX_BASE64_BYTES / 4) * 3;
    if text.len() <= max_raw {
        return text;
    }
    let mut end = max_raw;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

/// Pure: the full OSC 52 "set clipboard" escape sequence for `text`, capped
/// by [`cap_for_osc52`]. `c` selects the system clipboard (as opposed to
/// `p`, the primary selection) -- what every terminal implementing OSC 52
/// treats as "the" clipboard a paste reads from.
pub(super) fn osc52_copy_sequence(text: &str) -> Vec<u8> {
    let capped = cap_for_osc52(text);
    let encoded = b64_encode(capped.as_bytes());
    let mut seq = Vec::with_capacity(encoded.len() + 8);
    seq.extend_from_slice(b"\x1b]52;c;");
    seq.extend_from_slice(encoded.as_bytes());
    seq.push(0x07);
    seq
}

/// Writes an OSC 52 clipboard-set sequence to the host terminal, the same
/// way `term::dash_mouse_on_bytes` is written at startup: raw to stdout,
/// flushed immediately. Best-effort like that write -- a terminal that
/// ignores or does not support OSC 52, or a write that simply fails, loses
/// the copy, not the session.
pub(super) fn copy_to_host_clipboard(text: &str) -> io::Result<()> {
    let mut stdout = io::stdout();
    stdout.write_all(&osc52_copy_sequence(text))?;
    stdout.flush()
}

/// Pure: `text`, with trailing horizontal whitespace trimmed from every
/// line. `vt100::Row::write_contents` (what `contents_between`/`rows` read
/// through) never pads a cell the child truly never wrote into, but a
/// harness that redraws by clearing to end-of-line with literal spaces (a
/// prompt box, a right-aligned status strip, ...) writes real space
/// *characters* there, which `has_contents()` then reports as real content
/// -- included verbatim in a copy. The operator never saw those as part of
/// what they selected, and pasting them elsewhere reflows or diffs oddly
/// against a source that trimmed on write. Only trailing whitespace on each
/// line is touched; `\n` stays the line separator and leading/interior
/// whitespace is left exactly as drawn.
pub(super) fn trim_trailing_whitespace_per_line(text: &str) -> String {
    text.lines()
        .map(str::trim_end)
        .collect::<Vec<_>>()
        .join("\n")
}

/// One external clipboard command to try, and the argv it needs.
type ClipboardCommand = (&'static str, &'static [&'static str]);

/// Run platform clipboard fallback even after a successful OSC 52 write, since some terminals silently ignore it; prefer Wayland's wl-copy before X11's xclip (#697).
pub(super) fn fallback_clipboard_commands() -> &'static [ClipboardCommand] {
    if cfg!(target_os = "macos") {
        &[("pbcopy", &[])]
    } else if cfg!(target_os = "windows") {
        &[("clip.exe", &[])]
    } else {
        &[("wl-copy", &[]), ("xclip", &["-selection", "clipboard"])]
    }
}

/// What [`copy_via_fallback`] needs from whatever launches one clipboard
/// command: given the program, its args and the text to copy, either it
/// landed (`Ok`) or it did not (`Err`, for any reason -- missing from
/// `$PATH`, refused the write, exited non-zero). A trait object rather than
/// a bare function pointer so a test can inject a closure that records what
/// it was asked to run and returns a canned result, never actually forking
/// `pbcopy`/`wl-copy`/`xclip`/`clip.exe`.
type ClipboardSpawner<'a> = dyn Fn(&str, &[&str], &str) -> io::Result<()> + 'a;

/// Spawns `program args`, writes `text` to its stdin, closes it (so the
/// command sees EOF and actually acts -- every command
/// `fallback_clipboard_commands` lists reads until end of input) and waits
/// for it to exit. The real [`ClipboardSpawner`]; tests inject a fake
/// instead (see `copy_via_fallback`'s own tests).
pub(super) fn spawn_and_write_clipboard(
    program: &str,
    args: &[&str],
    text: &str,
) -> io::Result<()> {
    use std::process::{Command, Stdio};
    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(text.as_bytes())?;
        // Dropped here, at the end of this block: closing the pipe is what
        // tells the command the input is complete, which is what lets
        // `wait` below return promptly instead of blocking on a child still
        // waiting for more.
    }
    let status = child.wait()?;
    if status.success() {
        Ok(())
    } else {
        Err(io::Error::other(format!("{program} exited with {status}")))
    }
}

/// Try clipboard commands in order and report failure only if every command fails (#697).
pub(super) fn copy_via_fallback(
    spawner: &ClipboardSpawner<'_>,
    commands: &[ClipboardCommand],
    text: &str,
) -> bool {
    commands
        .iter()
        .any(|(program, args)| spawner(program, args, text).is_ok())
}

/// What copying a selection settled on, once OSC 52's own write and the
/// background fallback attempt have both had their say. Confirmed and
/// unconfirmed both mean "do not alarm the operator" -- the difference is
/// only whether anything could actually verify the copy landed; `Failed` is
/// the one case whose notice must be an error, since it is the silent loss
/// the issue is about.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ClipboardOutcome {
    /// The fallback command actually ran and exited cleanly: the clipboard
    /// is confirmed set on this machine, whatever OSC 52's own write did.
    Confirmed,
    /// OSC 52's write to the host terminal succeeded and no fallback
    /// command could confirm or deny it (none is installed on this
    /// platform) -- the same best-effort standing `copy_to_host_clipboard`
    /// always had on its own.
    Unconfirmed,
    /// Neither avenue worked: OSC 52's own write failed AND every fallback
    /// command failed too.
    Failed,
}

/// Pure: folds an OSC 52 write's own result and the fallback attempt's own
/// result into one [`ClipboardOutcome`]. See that type's own doc comment for
/// what each arm means to the operator; this is the decision `copy_selection`
/// hands off to a background thread purely so it stays unit-testable without
/// spawning anything real.
pub(super) fn resolve_clipboard_outcome(osc52_ok: bool, fallback_ok: bool) -> ClipboardOutcome {
    if fallback_ok {
        ClipboardOutcome::Confirmed
    } else if osc52_ok {
        ClipboardOutcome::Unconfirmed
    } else {
        ClipboardOutcome::Failed
    }
}

/// Copies `text` to the clipboard: OSC 52 first (synchronous, and fast
/// enough to never be worth backgrounding -- a handful of bytes to the host
/// terminal's own stdout), then the platform fallback command on a detached
/// thread, the same way `FactsRefresher` backgrounds a slow disk read -- so
/// a hung or missing `pbcopy`/`wl-copy`/`xclip`/`clip.exe` costs the render
/// loop nothing. `result_tx` is where the outcome lands once the thread
/// finishes; the event loop drains it with `try_recv`, the same way it
/// drains `FactsRefresher`'s own channel.
///
/// A thread that fails to spawn at all runs the fallback inline instead --
/// the same "still correct, just not backgrounded" degradation
/// `FactsRefresher::spawn` uses for its own read -- rather than dropping the
/// fallback entirely.
pub(super) fn copy_selection(text: String, result_tx: &mpsc::Sender<ClipboardOutcome>) {
    let osc52_ok = copy_to_host_clipboard(&text).is_ok();
    let tx = result_tx.clone();
    let thread_text = text.clone();
    let spawned = std::thread::Builder::new()
        .name("zirv-dash-clipboard".to_string())
        .spawn(move || {
            let fallback_ok = copy_via_fallback(
                &spawn_and_write_clipboard,
                fallback_clipboard_commands(),
                &thread_text,
            );
            let _ = tx.send(resolve_clipboard_outcome(osc52_ok, fallback_ok));
        })
        .is_ok();
    if !spawned {
        let fallback_ok = copy_via_fallback(
            &spawn_and_write_clipboard,
            fallback_clipboard_commands(),
            &text,
        );
        let _ = result_tx.send(resolve_clipboard_outcome(osc52_ok, fallback_ok));
    }
}

/// Prefer a live transient notice over a sticky error, which reappears when the notice expires.
pub(super) fn live_notice(notices: &[Notice], now: Instant) -> Option<&str> {
    notices
        .iter()
        .rev()
        .find(|n| n.expires_at > now)
        .map(|n| n.text.as_str())
}

/// Best-effort: claims any `<short>.nudge` markers written for this
/// dashboard's own live panes and turns each into a header notice. The
/// nudger has already delivered the message body to the pane's inbox (mail);
/// this only surfaces the wake-up so the operator knows to look. Throttled by
/// the caller (once per `FACTS_THROTTLE`), the same as every other disk read
/// here.
pub(super) fn claim_pane_nudges(
    panes: &[Pane],
    state: &StateDir,
    notices: &mut Vec<Notice>,
    now: Instant,
) {
    for pane in panes {
        if sessions::claim_nudge_marker(state, pane.short()).is_some() {
            push_notice(
                notices,
                now,
                format!("nudge received for {} -- see inbox", pane.short()),
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every scroll says what it did. Silence on a scroll that moved nothing
    /// is what left two rounds of "it does not scroll" with nothing to go on,
    /// and the full-screen case has to name itself: it is the one where the
    /// pane genuinely has no scrollback to move.
    #[test]
    fn every_scroll_outcome_has_something_to_say_for_itself() {
        assert_eq!(
            scroll_notice(ScrollOutcome::Scrolled(12)),
            "scrolled back 12 line(s)"
        );
        assert_eq!(
            scroll_notice(ScrollOutcome::Scrolled(0)),
            "back to the live view"
        );
        assert_eq!(
            scroll_notice(ScrollOutcome::AtOldest),
            "already at the oldest line"
        );
        assert_eq!(
            scroll_notice(ScrollOutcome::AtLive),
            "already at the live view"
        );
        for full_screen in [ScrollOutcome::ForwardedMouse, ScrollOutcome::FullScreen] {
            assert!(
                scroll_notice(full_screen).contains("full-screen mode"),
                "{full_screen:?} must name the mode it is in: {}",
                scroll_notice(full_screen)
            );
        }
        assert!(
            scroll_notice(ScrollOutcome::ForwardedMouse).contains("forwarded to the app"),
            "a forwarded scroll says where it went"
        );
    }

    /// A forwarded wheel event has to arrive in the child's own coordinate
    /// space: pane-local and 1-based. An untranslated frame coordinate makes
    /// the child act on the wrong row, which is worse than not scrolling --
    /// and with the sidebar drawn the pane's origin is genuinely not `(0, 0)`.
    #[test]
    fn a_forwarded_wheel_lands_in_the_childs_own_coordinate_space() {
        // The real geometry: an 80x24 frame, one header row, one rule row
        // (issue #209/v3 §A4/§D), a 24-column sidebar and its separator,
        // then dash refresh PR1's own pane-header row and its rule below it
        // -- so the pane starts at column 25, row 4.
        let main = ui::layout(Rect::new(0, 0, 80, 24), 24).main;
        assert_eq!((main.x, main.y), (25, 4), "sanity: the pane is inset");

        assert_eq!(
            pane_local_mouse(main, main.x, main.y),
            (1, 1),
            "the pane's own top-left cell is its (1, 1), not the frame's"
        );
        assert_eq!(pane_local_mouse(main, 31, 8), (7, 5));
        // Bottom-right corner of the pane, and nothing past it.
        assert_eq!(
            pane_local_mouse(main, main.x + main.width - 1, main.y + main.height - 1),
            (main.width, main.height)
        );
        assert_eq!(
            pane_local_mouse(main, 500, 500),
            (main.width, main.height),
            "a position past the pane clamps into it rather than wrapping"
        );
        // The pointer over the sidebar still encodes to somewhere inside the
        // pane: the wheel scrolls the focused pane wherever it is pointing.
        assert_eq!(pane_local_mouse(main, 0, 0), (1, 1));
        // Degenerate rects a narrowed terminal really produces.
        assert_eq!(pane_local_mouse(Rect::new(24, 1, 0, 0), 30, 5), (1, 1));
        assert_eq!(pane_local_mouse(Rect::new(0, 0, 1, 1), 9, 9), (1, 1));
    }

    /// This loop never forwards a `Drag` to a child (`Pane::forward_mouse_button`
    /// is only ever called from the `Down`/`Up` arms), so the only button
    /// events that can reach one are still presses and releases even though
    /// `term::dash_mouse_on_bytes` now enables `?1002` alongside `?1000h`/
    /// `?1006h` -- and they carry the protocol's own button numbers, which
    /// the wheel's 64/65 extend.
    #[test]
    fn mouse_buttons_use_the_protocols_own_numbering() {
        assert_eq!(mouse_button_code(MouseButton::Left), 0);
        assert_eq!(mouse_button_code(MouseButton::Middle), 1);
        assert_eq!(mouse_button_code(MouseButton::Right), 2);
    }

    #[test]
    fn pane_local_cell_translates_and_clamps_into_the_grid_zero_based() {
        let main = Rect::new(24, 1, 76, 29);
        // The pane's own top-left cell is (0, 0), not the frame's, and not
        // `pane_local_mouse`'s 1-based (1, 1).
        assert_eq!(
            pane_local_cell(main, 24, 1, 29, 76),
            Some((0, 0)),
            "top-left of the grid is (row 0, col 0)"
        );
        assert_eq!(pane_local_cell(main, 31, 5, 29, 76), Some((4, 7)));
        // Past the pane clamps into its last row/col rather than wrapping or
        // indexing out of bounds.
        assert_eq!(
            pane_local_cell(main, 500, 500, 29, 76),
            Some((28, 75)),
            "clamped to the last cell of a 29x76 grid"
        );
        // A grid smaller than `area` (a resize race) clamps to the grid's own
        // size, not just the area's.
        assert_eq!(
            pane_local_cell(main, 500, 500, 3, 10),
            Some((2, 9)),
            "clamped to the smaller grid, not the larger area"
        );
        // Degenerate inputs never panic and never index a grid that has
        // nothing in it.
        assert_eq!(pane_local_cell(Rect::new(24, 1, 0, 0), 30, 5, 29, 76), None);
        assert_eq!(pane_local_cell(main, 30, 5, 0, 76), None);
        assert_eq!(pane_local_cell(main, 30, 5, 29, 0), None);
    }

    /// `contents_between` does not order its own arguments -- a `start_row`
    /// past `end_row` silently returns an empty string -- so this is the one
    /// invariant every caller depends on: whichever way the drag actually
    /// went, the pair that comes back reads in row-major order.
    #[test]
    fn normalize_selection_orders_start_before_end_in_reading_order() {
        assert_eq!(normalize_selection((1, 5), (3, 2)), ((1, 5), (3, 2)));
        assert_eq!(
            normalize_selection((3, 2), (1, 5)),
            ((1, 5), (3, 2)),
            "an upward drag is swapped back into reading order"
        );
        // Same row: ordered by column.
        assert_eq!(normalize_selection((2, 7), (2, 3)), ((2, 3), (2, 7)));
        assert_eq!(normalize_selection((2, 3), (2, 7)), ((2, 3), (2, 7)));
        // A degenerate click (anchor == end) is its own normalized form.
        assert_eq!(normalize_selection((4, 4), (4, 4)), ((4, 4), (4, 4)));
    }

    fn selection_at(anchor: (i64, u16), end: (i64, u16)) -> Selection {
        Selection {
            pane_short: "aaa11111".to_string(),
            anchor,
            end,
        }
    }

    /// Issue #697: scrolling no longer cancels a selection -- it translates
    /// it, by exactly the amount the pane's own scrollback offset just moved
    /// by, so the selection stays exact wherever the pane's own content
    /// happens to be currently drawn. Replaces the old
    /// `scroll_cancels_a_selection_on_the_same_pane_but_not_others`, which
    /// pinned the opposite (and now removed) behaviour; the invariant it
    /// protected -- a selection must never be evaluated against stale
    /// coordinates -- is now `translate_selection`'s job instead of a
    /// cancel's.
    #[test]
    fn translate_selection_shifts_anchor_and_end_by_the_scroll_delta_on_the_same_pane_only() {
        let mut sel = Some(selection_at((1, 0), (3, 5)));
        translate_selection(&mut sel, "aaa11111", 3); // offset 0 -> 3.
        assert_eq!(
            sel,
            Some(selection_at((4, 0), (6, 5))),
            "both anchor and end move by the same delta"
        );

        translate_selection(&mut sel, "aaa11111", -3); // offset 3 -> 0, back where it started.
        assert_eq!(sel, Some(selection_at((1, 0), (3, 5))));

        let before = sel.clone();
        translate_selection(&mut sel, "aaa11111", 0);
        assert_eq!(sel, before, "a delta of zero is a no-op");

        translate_selection(&mut sel, "bbb22222", 5);
        assert_eq!(
            sel, before,
            "a scroll on a different pane never touches this selection"
        );

        translate_selection(&mut sel, "aaa11111", -10);
        assert_eq!(
            sel,
            Some(selection_at((-9, 0), (-7, 5))),
            "a translation past row 0 keeps its true (negative) value rather than \
             clamping in storage -- only resolve_selection_range clamps"
        );
    }

    /// HIGH (review): the gap `translate_selection` alone cannot cover --
    /// live output at scrollback offset 0 rewrites the grid rows under a
    /// selection's stale coordinates with the offset never moving at all.
    #[test]
    fn processed_output_cancels_a_selection_on_the_same_pane_but_not_others() {
        let sel = selection_at((0, 0), (2, 4));
        assert!(
            output_cancels_selection(&sel, "aaa11111"),
            "output on the selected pane invalidates it"
        );
        assert!(
            !output_cancels_selection(&sel, "bbb22222"),
            "output on a different pane leaves this selection alone"
        );
    }

    /// The call site's own reason `output_cancels_selection` returning
    /// `true` no longer means an automatic cancel: a busy pane that keeps
    /// producing output far from the selected rows (a streaming response
    /// below it, a status line above it) must not wipe out a selection that
    /// spans more than one tick, which is exactly what happened before this
    /// -- the highlight only ever survived the tick it started in.
    #[test]
    fn selection_snapshot_is_unchanged_when_new_output_lands_outside_the_selected_rows() {
        let mut parser = vt100::Parser::new(5, 10, 0);
        parser.process(b"line1\r\nline2\r\nline3\r\nline4\r\nline5");
        let sel = selection_at((0, 0), (1, 4));
        let before = selection_snapshot(parser.screen(), &sel);
        assert_eq!(before.as_deref(), Some("line1\nline"));

        // New output lands on row 4, well below the selected rows 0..1 --
        // short enough not to overflow the 10-column grid and trigger a
        // genuine scroll, which would legitimately invalidate row 0 too.
        parser.process(b"\x1b[5;1Hlin5x");
        let after = selection_snapshot(parser.screen(), &sel);
        assert_eq!(
            before, after,
            "output outside the selection's own rows must not change its snapshot"
        );
    }

    /// The other half: output that actually rewrites a row the selection
    /// covers -- the case `output_cancels_selection`'s cancel exists for --
    /// still changes the snapshot, so the call site still cancels it.
    #[test]
    fn selection_snapshot_changes_when_new_output_overwrites_a_selected_row() {
        let mut parser = vt100::Parser::new(5, 10, 0);
        parser.process(b"line1\r\nline2\r\nline3\r\nline4\r\nline5");
        let sel = selection_at((0, 0), (1, 4));
        let before = selection_snapshot(parser.screen(), &sel);

        // New output overwrites row 1, inside the selected range.
        parser.process(b"\x1b[2;1Hchanged!!!");
        let after = selection_snapshot(parser.screen(), &sel);
        assert_ne!(
            before, after,
            "output that rewrites a selected row must change its snapshot"
        );
    }

    /// Review finding: a selection scrolled out of view still resolves --
    /// `resolve_selection_range` clamps it to the visible grid -- so
    /// comparing snapshots of it would compare only the sliver still on
    /// screen and keep a selection whose real rows had been overwritten.
    /// Off-screen must read as "cannot tell", which the call site turns back
    /// into the unconditional cancel.
    #[test]
    fn a_selection_scrolled_off_screen_has_no_snapshot_to_compare() {
        let mut parser = vt100::Parser::new(5, 10, 0);
        parser.process(b"line1\r\nline2\r\nline3\r\nline4\r\nline5");
        assert!(
            selection_snapshot(parser.screen(), &selection_at((-1, 0), (1, 4))).is_none(),
            "a selection reaching above the visible grid cannot be compared"
        );
        assert!(
            selection_snapshot(parser.screen(), &selection_at((0, 0), (5, 4))).is_none(),
            "a selection reaching below the visible grid cannot be compared"
        );
    }

    /// MEDIUM (review): none of the three resize paths (`Event::Resize`, the
    /// per-frame reconciliation, `Ctrl+A z`) used to touch a selection, so a
    /// shrink could leave its coordinates pointing past the new grid.
    #[test]
    fn resize_cancels_a_selection_on_the_same_pane_but_not_others() {
        let sel = selection_at((0, 0), (10, 20));
        assert!(
            resize_cancels_selection(&sel, "aaa11111", (24, 80), (20, 80)),
            "the selected pane's grid actually changed size"
        );
        assert!(
            !resize_cancels_selection(&sel, "aaa11111", (24, 80), (24, 80)),
            "an unchanged size (e.g. re-zooming to the same geometry) is a no-op"
        );
        assert!(
            !resize_cancels_selection(&sel, "bbb22222", (24, 80), (20, 80)),
            "a resize of a different pane never touches this selection"
        );
    }

    /// Issue #697: translating a selection by more than one screenful's
    /// worth of rows (the grid this fixture implies is nowhere near 10 rows
    /// tall) must still be exact, not clamp or lose precision partway --
    /// the case a naive "cancel past the edge" rule, or a translation that
    /// saturated instead of staying signed, would have broken. Complements
    /// `scroll_translates_a_selection_and_resolves_the_same_text_after_
    /// scrolling_back_into_view` below, which pins the same property against
    /// a real `vt100::Parser`.
    #[test]
    fn a_selection_survives_translating_more_than_one_screenful_away_and_back() {
        let mut sel = Some(selection_at((0, 0), (2, 4)));
        translate_selection(&mut sel, "aaa11111", 10);
        assert_eq!(
            sel,
            Some(selection_at((10, 0), (12, 4))),
            "translation is exact however far past a single screen the scroll went"
        );
        translate_selection(&mut sel, "aaa11111", -10);
        assert_eq!(
            sel,
            Some(selection_at((0, 0), (2, 4))),
            "and scrolling back the same amount restores it exactly"
        );
    }

    /// `resolve_selection_range` is the one place a translated selection's
    /// coordinates are ever clamped -- issue #697's own "clamp to the grid
    /// edge for highlight purposes but keep the true translated value"
    /// requirement, from the other side: given a selection already pushed
    /// out of range, this is what a renderer or an extraction actually gets.
    #[test]
    fn resolve_selection_range_clamps_out_of_range_rows_and_columns_to_the_grid_edge() {
        let sel = selection_at((-5, 3), (2, 999));
        assert_eq!(
            resolve_selection_range(&sel, 3, 10),
            ((0, 3), (2, 9)),
            "a negative row clamps to 0, an over-wide column clamps to the last one"
        );
        // Ordering still holds after clamping: the more-negative row is
        // still `start`, whatever it clamped to.
        let sel = selection_at((7, 0), (-3, 0));
        assert_eq!(resolve_selection_range(&sel, 3, 10), ((0, 0), (2, 0)));
        // A zero-sized grid never panics; everything clamps to row/col 0.
        let sel = selection_at((4, 4), (4, 4));
        assert_eq!(resolve_selection_range(&sel, 0, 0), ((0, 0), (0, 0)));
    }

    /// Issue #697's click-vs-drag deferral, the click half: a release within
    /// `DRAG_THRESHOLD_CELLS` of the press is a plain click, never a drag.
    /// `deferred_click_coords` is exactly the pane-local coordinate pair the
    /// event loop hands `Pane::forward_mouse_button` for it (press, then
    /// release) -- that function is itself the `wants_mouse` gate on
    /// whether those bytes actually reach the child (`pane.rs`'s own
    /// suite), so proving the dashboard computes and would forward the
    /// right pair, for a press that never crossed the threshold, is what is
    /// testable here without a live `Pane` or a spawned child.
    #[test]
    fn a_press_and_release_within_the_threshold_replays_as_a_deferred_click() {
        let pending = PendingPress {
            pane_short: "aaa11111".to_string(),
            column: 50,
            row: 6,
            anchor_cell: (2, 3),
        };
        // One cell of movement: still within the threshold.
        assert!(!past_drag_threshold(pending.column, pending.row, 51, 6));

        let main = Rect::new(45, 2, 35, 16);
        let (press, release) = deferred_click_coords(&pending, main, 51, 6);
        assert_eq!(
            press,
            pane_local_mouse(main, 50, 6),
            "the ORIGINAL press coordinates are replayed, not the release's"
        );
        assert_eq!(release, pane_local_mouse(main, 51, 6));
    }

    /// The drag half of the same deferral: once the release (or an
    /// intervening drag) has moved past the threshold, the press is
    /// promoted into a `Selection` instead -- and, structurally, the event
    /// loop's `Up` handler can then never reach the click-forwarding branch
    /// for it: `pending_press` was already consumed (`.take()`) by the
    /// `Drag` event that promoted it, so only the `Selection`-copy path
    /// remains, which never calls `Pane::forward_mouse_button` at all.
    #[test]
    fn a_press_moved_past_the_threshold_promotes_into_a_selection_instead_of_a_click() {
        let pending = PendingPress {
            pane_short: "aaa11111".to_string(),
            column: 50,
            row: 6,
            anchor_cell: (2, 3),
        };
        // Two cells of movement: past the threshold, a drag.
        assert!(past_drag_threshold(pending.column, pending.row, 52, 6));

        let sel = promote_pending_drag(pending, (4, 7));
        assert_eq!(
            sel,
            Selection {
                pane_short: "aaa11111".to_string(),
                anchor: (2, 3),
                end: (4, 7),
            },
            "the anchor is exactly where the button went down, not the current cell"
        );
    }

    /// F7-a: a click -- `Down` then `Up` with no `Drag` in between, so `end`
    /// never moved off `anchor` -- must never copy anything, and must not
    /// leave a zero-width "selection" highlighted either.
    #[test]
    fn a_click_without_a_drag_copies_nothing_and_clears_the_selection() {
        let sel = selection_at((2, 3), (2, 3));
        let (kept, copy) = selection_on_release(sel);
        assert_eq!(kept, None, "nothing stays highlighted after a bare click");
        assert!(!copy, "a click must never trigger a copy");
    }

    /// A genuine drag -- `end` differs from `anchor` by the time the button
    /// comes up -- is kept (so it stays highlighted) and is the one case that
    /// copies.
    #[test]
    fn a_real_drag_is_kept_and_copied_on_release() {
        let sel = selection_at((2, 3), (2, 9));
        let (kept, copy) = selection_on_release(sel.clone());
        assert_eq!(kept, Some(sel));
        assert!(copy);
    }

    /// The extraction path end to end: a small known screen, a normalized
    /// selection, and `vt100::Screen::contents_between` -- pinning that the
    /// coordinates this module feeds it are in the same space `pane.screen()`
    /// already renders from (row-major, 0-based, scrollback already applied
    /// by `vt100` itself).
    #[test]
    fn extraction_reads_the_right_text_out_of_a_known_screen() {
        let mut parser = vt100::Parser::new(3, 10, 0);
        parser.process(b"abcdefghij\r\nKLMNOPQRST\r\nzyxwvutsrq");
        let screen = parser.screen();

        // A drag from row 0 col 3 to row 1 col 4 (dragged downward): the
        // tail of row 0 from col 3, then the head of row 1 up to col 4.
        let (start, end) = normalize_selection((0, 3), (1, 4));
        let text = screen.contents_between(start.0, start.1, end.0, end.1);
        assert_eq!(text, "defghij\nKLMN");

        // A single-row drag is just that row's own half-open column range.
        let (start, end) = normalize_selection((2, 1), (2, 5));
        let text = screen.contents_between(start.0, start.1, end.0, end.1);
        assert_eq!(text, "yxwv");

        // An upward drag still reads correctly once normalized.
        let (start, end) = normalize_selection((1, 2), (0, 6));
        let text = screen.contents_between(start.0, start.1, end.0, end.1);
        assert_eq!(text, "ghij\nKL");
    }

    /// Issue #697's central claim, pinned against a real `vt100::Parser` the
    /// way `extraction_reads_the_right_text_out_of_a_known_screen` does: a
    /// scroll never cancels a selection any more, and translating it by the
    /// scroll's own delta means the SAME text extracts once the pane is
    /// scrolled back to where it was -- even though, mid-scroll, the
    /// translated coordinates briefly point past the bottom of the grid
    /// that is visible right then (exactly the "clamp for highlight
    /// purposes but keep the true value" case `resolve_selection_range`'s
    /// own doc comment describes).
    #[test]
    fn scroll_translates_a_selection_and_resolves_the_same_text_after_scrolling_back_into_view() {
        let mut parser = vt100::Parser::new(3, 10, 10);
        // No trailing `\r\n` after the last line: three lines scroll into
        // history (line1..line3), leaving line4..line6 as the live screen.
        parser.process(b"line1\r\nline2\r\nline3\r\nline4\r\nline5\r\nline6");
        assert_eq!(parser.screen().scrollback(), 0, "starts at the live view");

        // Select all of line4 through the first 4 columns of line6, while
        // still live.
        let mut sel = Some(selection_at((0, 0), (2, 4)));
        let (rows, cols) = parser.screen().size();
        let expected = {
            let (start, end) = resolve_selection_range(sel.as_ref().unwrap(), rows, cols);
            parser
                .screen()
                .contents_between(start.0, start.1, end.0, end.1)
        };
        assert_eq!(expected, "line4\nline5\nline");

        // Scroll all the way back into history: line1..line3 are now what
        // is drawn at rows 0..2, so the selection's own translated rows
        // (3 and 5) point past the bottom of what is CURRENTLY visible --
        // selecting nothing sensible if resolved right now, which is
        // exactly why nothing tries to extract or highlight it at this
        // point.
        parser.screen_mut().set_scrollback(3);
        translate_selection(&mut sel, "aaa11111", 3);
        assert_eq!(sel, Some(selection_at((3, 0), (5, 4))));

        // Scroll back to live: the selection translates back to its
        // original coordinates, and extracting it now reproduces the exact
        // same text as before the round trip.
        parser.screen_mut().set_scrollback(0);
        translate_selection(&mut sel, "aaa11111", -3);
        assert_eq!(sel, Some(selection_at((0, 0), (2, 4))));
        let (start, end) = resolve_selection_range(sel.as_ref().unwrap(), rows, cols);
        let actual = parser
            .screen()
            .contents_between(start.0, start.1, end.0, end.1);
        assert_eq!(actual, expected);
    }

    #[test]
    fn b64_encode_matches_known_vectors() {
        // RFC 4648 test vectors.
        assert_eq!(b64_encode(b""), "");
        assert_eq!(b64_encode(b"f"), "Zg==");
        assert_eq!(b64_encode(b"fo"), "Zm8=");
        assert_eq!(b64_encode(b"foo"), "Zm9v");
        assert_eq!(b64_encode(b"foob"), "Zm9vYg==");
        assert_eq!(b64_encode(b"fooba"), "Zm9vYmE=");
        assert_eq!(b64_encode(b"foobar"), "Zm9vYmFy");
    }

    /// The full OSC 52 wire format for a known string: `ESC ] 52 ; c ;
    /// <base64> BEL`.
    #[test]
    fn osc52_copy_sequence_wraps_the_base64_payload_correctly() {
        let seq = osc52_copy_sequence("hello");
        assert_eq!(seq, b"\x1b]52;c;aGVsbG8=\x07".to_vec());
    }

    /// A selection is plain UTF-8 text, so the cap has to fall on a char
    /// boundary even when that means giving up a couple of trailing bytes.
    #[test]
    fn cap_for_osc52_truncates_on_a_char_boundary() {
        // 4-byte UTF-8 char right at the boundary the raw cap would otherwise
        // land inside.
        let max_raw = (OSC52_MAX_BASE64_BYTES / 4) * 3;
        let mut text = "a".repeat(max_raw - 2);
        text.push('\u{1F600}'); // a 4-byte emoji straddling the cap
        let capped = cap_for_osc52(&text);
        assert!(capped.len() <= max_raw);
        assert!(text.is_char_boundary(capped.len()));
        assert!(
            capped.chars().all(|c| c == 'a'),
            "the split emoji is dropped, not corrupted"
        );

        // Short text is never touched.
        assert_eq!(cap_for_osc52("short"), "short");
    }

    /// The output cap in the brief's own terms: 64 KiB of base64, never more.
    #[test]
    fn osc52_copy_sequence_never_exceeds_the_base64_cap() {
        let huge = "x".repeat(OSC52_MAX_BASE64_BYTES * 2);
        let seq = osc52_copy_sequence(&huge);
        // seq is "\x1b]52;c;" (7 bytes) + base64 + "\x07" (1 byte).
        let payload_len = seq.len() - 8;
        assert!(
            payload_len <= OSC52_MAX_BASE64_BYTES,
            "base64 payload was {payload_len} bytes"
        );
    }

    /// Issue #697: `vt100::Screen::contents_between`/`rows` pad every line
    /// out to the pane's own column count, so a copied selection carries
    /// trailing spaces the operator never saw as meaningful. Only trailing
    /// whitespace is touched -- leading and interior spacing survive
    /// untouched, and `\n` stays the line separator.
    #[test]
    fn trim_trailing_whitespace_per_line_trims_only_trailing_runs() {
        assert_eq!(
            trim_trailing_whitespace_per_line("line4     \nline5     \nline6"),
            "line4\nline5\nline6"
        );
        assert_eq!(
            trim_trailing_whitespace_per_line("  indented  \nplain"),
            "  indented\nplain",
            "leading whitespace is left alone -- only trailing is trimmed"
        );
        assert_eq!(trim_trailing_whitespace_per_line(""), "");
        assert_eq!(
            trim_trailing_whitespace_per_line("no trailing space"),
            "no trailing space"
        );
    }

    /// Pure with respect to the injected spawner: `copy_via_fallback` stops
    /// at the first command that succeeds, and never calls a later one.
    #[test]
    fn copy_via_fallback_stops_at_the_first_command_that_succeeds() {
        let calls: std::sync::Mutex<Vec<&'static str>> = std::sync::Mutex::new(Vec::new());
        let spawner = |program: &str, _args: &[&str], _text: &str| -> io::Result<()> {
            calls.lock().unwrap().push(match program {
                "wl-copy" => "wl-copy",
                "xclip" => "xclip",
                other => panic!("unexpected program {other}"),
            });
            if program == "wl-copy" {
                Ok(())
            } else {
                Err(io::Error::other("should never be reached"))
            }
        };
        let commands: &[ClipboardCommand] = &[("wl-copy", &[]), ("xclip", &["-selection"])];
        assert!(copy_via_fallback(&spawner, commands, "hello"));
        assert_eq!(*calls.lock().unwrap(), vec!["wl-copy"]);
    }

    /// The other half: every command failing is a real `false`, not a panic
    /// or a false positive, and every one of them was actually tried.
    #[test]
    fn copy_via_fallback_tries_every_command_before_giving_up() {
        let calls: std::sync::Mutex<Vec<&'static str>> = std::sync::Mutex::new(Vec::new());
        let spawner = |program: &str, _args: &[&str], _text: &str| -> io::Result<()> {
            calls.lock().unwrap().push(match program {
                "wl-copy" => "wl-copy",
                "xclip" => "xclip",
                other => panic!("unexpected program {other}"),
            });
            Err(io::Error::other("not installed"))
        };
        let commands: &[ClipboardCommand] = &[("wl-copy", &[]), ("xclip", &["-selection"])];
        assert!(!copy_via_fallback(&spawner, commands, "hello"));
        assert_eq!(*calls.lock().unwrap(), vec!["wl-copy", "xclip"]);
    }

    /// An empty command list (should never happen -- every platform branch
    /// of `fallback_clipboard_commands` lists at least one -- but the
    /// function itself makes no such assumption) is simply `false`.
    #[test]
    fn copy_via_fallback_with_no_commands_is_false() {
        let spawner = |_: &str, _: &[&str], _: &str| -> io::Result<()> {
            panic!("must never be called with an empty command list")
        };
        assert!(!copy_via_fallback(&spawner, &[], "hello"));
    }

    /// `fallback_clipboard_commands` names a real, non-empty command list for
    /// whichever platform this test happens to run on.
    #[test]
    fn fallback_clipboard_commands_matches_this_platform() {
        let commands = fallback_clipboard_commands();
        assert!(!commands.is_empty());
        if cfg!(target_os = "macos") {
            assert_eq!(commands[0].0, "pbcopy");
        } else if cfg!(target_os = "windows") {
            assert_eq!(commands[0].0, "clip.exe");
        } else {
            assert_eq!(commands[0].0, "wl-copy");
            assert!(commands.iter().any(|(program, _)| *program == "xclip"));
        }
    }

    /// Issue #697: the clipboard fallback is what gets chosen -- and
    /// confirms the copy -- exactly when OSC 52 could not be trusted on its
    /// own; `resolve_clipboard_outcome` is the pure decision `copy_selection`
    /// hands to its background thread, so this is testable with no process
    /// ever spawned.
    #[test]
    fn resolve_clipboard_outcome_prefers_a_confirmed_fallback_and_only_fails_when_both_do() {
        assert_eq!(
            resolve_clipboard_outcome(true, true),
            ClipboardOutcome::Confirmed
        );
        assert_eq!(
            resolve_clipboard_outcome(false, true),
            ClipboardOutcome::Confirmed,
            "the fallback confirming the copy wins even if OSC 52's own write failed"
        );
        assert_eq!(
            resolve_clipboard_outcome(true, false),
            ClipboardOutcome::Unconfirmed,
            "OSC 52 unavailable to double check is not by itself a failure"
        );
        assert_eq!(
            resolve_clipboard_outcome(false, false),
            ClipboardOutcome::Failed,
            "neither avenue worked: the one case that must not be silent"
        );
    }

    /// L13: a notice shows while live and disappears once past its TTL; the
    /// header prefers the freshest live notice.
    #[test]
    fn notices_expire_after_their_ttl() {
        let now = Instant::now();
        let mut notices = Vec::new();
        push_notice(&mut notices, now, "spawned claude".to_string());
        assert_eq!(live_notice(&notices, now), Some("spawned claude"));
        assert_eq!(
            live_notice(&notices, now + NOTICE_TTL + Duration::from_millis(1)),
            None,
            "a notice past its TTL is gone"
        );
        push_notice(
            &mut notices,
            now + Duration::from_millis(10),
            "nudge received".to_string(),
        );
        assert_eq!(
            live_notice(&notices, now + Duration::from_millis(20)),
            Some("nudge received"),
            "the freshest live notice wins"
        );
    }

    /// MED (reassigned): a nudge marker written for a live pane is claimed and
    /// surfaced as a notice.
    #[test]
    fn a_claimed_pane_nudge_marker_becomes_a_notice() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().join("state"));
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(&repo).expect("mkdir repo");
        let spec = PaneSpec {
            agent_name: "test-agent".to_string(),
            argv: super::pane::tests::long_lived_argv(),
            role: prompt::PromptRole::Worker,
            verb: sessions::Verb::Dash,
            session_id: "dddddddd-2222-4333-8444-555555555555".to_string(),
            title: "wrk nudge".to_string(),
        };
        let mut panes = vec![
            Pane::spawn(
                spec,
                &state,
                &repo,
                &repo,
                (80, 24),
                &[],
                true,
                pane::DEFAULT_IDLE_QUIET,
            )
            .expect("spawn"),
        ];
        let short = panes[0].short().to_string();

        // The nudger writes `<short>.nudge` into the sessions dir; write it
        // directly here rather than driving a whole `zirv ctx nudge`.
        std::fs::create_dir_all(state.sessions()).expect("mkdir sessions");
        std::fs::write(state.sessions().join(format!("{short}.nudge")), b"operator")
            .expect("write marker");

        let mut notices = Vec::new();
        claim_pane_nudges(&panes, &state, &mut notices, Instant::now());
        assert!(
            notices
                .iter()
                .any(|n| n.text.contains("nudge received") && n.text.contains(&short)),
            "a claimed marker surfaces a notice naming the pane"
        );
        // Idempotent: the marker was claimed (removed), so a second sweep is
        // silent.
        let mut again = Vec::new();
        claim_pane_nudges(&panes, &state, &mut again, Instant::now());
        assert!(again.is_empty(), "a claimed marker is not claimed twice");

        // finish_shutdown: see the identical comment on
        // `a_nudge_aimed_at_a_reaped_pane_is_reported_and_delivered_nowhere`.
        for pane in panes.iter_mut() {
            let _ = pane.finish_shutdown();
        }
    }

    /// Issue #697's own click-vs-drag number: strictly more than one cell of
    /// movement in either axis is a drag, one cell of jitter (or none at
    /// all) is still a click.
    #[test]
    fn past_drag_threshold_requires_more_than_one_cell_of_movement_in_either_axis() {
        assert!(!past_drag_threshold(10, 5, 10, 5), "no movement at all");
        assert!(
            !past_drag_threshold(10, 5, 11, 5),
            "one cell of jitter, column"
        );
        assert!(
            !past_drag_threshold(10, 5, 10, 6),
            "one cell of jitter, row"
        );
        assert!(!past_drag_threshold(10, 5, 9, 5), "one cell the other way");
        assert!(past_drag_threshold(10, 5, 12, 5), "two cells, column");
        assert!(past_drag_threshold(10, 5, 10, 7), "two cells, row");
        assert!(past_drag_threshold(10, 5, 8, 5), "two cells, backward");
    }

    /// Issue #697: dragging the pointer past either edge of the focused
    /// pane's own rect auto-scrolls it -- back into history above the top
    /// edge, toward live at or past the bottom edge -- and does nothing
    /// anywhere inside it.
    #[test]
    fn drag_autoscroll_direction_only_fires_past_either_edge() {
        let main = Rect::new(45, 2, 35, 16); // rows 2..=17.
        assert_eq!(
            drag_autoscroll_direction(main, 1),
            Some(1),
            "above the top edge scrolls further into history"
        );
        assert_eq!(
            drag_autoscroll_direction(main, 18),
            Some(-1),
            "at or past the bottom edge scrolls toward live"
        );
        assert_eq!(
            drag_autoscroll_direction(main, 2),
            None,
            "the top row itself is still inside the grid"
        );
        assert_eq!(
            drag_autoscroll_direction(main, 17),
            None,
            "the bottom row itself is still inside the grid"
        );
        assert_eq!(
            drag_autoscroll_direction(main, 9),
            None,
            "well inside the grid"
        );
        assert_eq!(
            drag_autoscroll_direction(Rect::new(0, 0, 0, 0), 5),
            None,
            "an empty rect never auto-scrolls"
        );
    }
}
