//! Key/mouse input routing: filters raw terminal events into dashboard actions.
use super::*;

/// What a filtered keystroke means for the dashboard's own loop to do next.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InputVerdict {
    /// Bytes to write into the active pane's pty as-is.
    ToChild(Vec<u8>),
    /// A dashboard command, already fully decoded.
    Dash(DashAction),
    /// The prefix key just armed; nothing to do until the next keystroke.
    Pending,
}

/// Every command the dashboard itself understands once the prefix is armed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DashAction {
    /// Right-click on a sidebar row: open the context menu for *that* row,
    /// which is not necessarily the selected one.
    ContextMenu(hit::RowId),
    /// `Ctrl+A c`, or the header's `actions` hint: the same menu for the
    /// selected row.
    ContextActions,
    /// Open the selected row's inspector with Ctrl+A i (#354).
    Inspect,
    /// Relaunch a retained ended row from its saved request, or show why it cannot be restored (#354).
    RestoreRow,
    /// `Ctrl+A Left`/`Ctrl+A Right`, or a click on a group header's
    /// disclosure triangle: fold a work group shut, or open it again.
    CollapseGroup,
    ExpandGroup,
    Switch(usize),
    NextPane,
    SelectUp,
    SelectDown,
    Spawn,
    Nudge,
    Mail,
    Memory,
    /// Ctrl+A o opens the focused pane's handover picker (#84).
    Handover,
    /// Ctrl+A e opens the retained errors overlay (#202).
    ShowErrors,
    /// Click affordance follow-up: a left click on the JEV sidebar's own
    /// `errors N \u{b7} <reason>` line -- opens `Overlay::JevErrors` over the
    /// cached rollup's own recent-errors list. Mouse-only: it is a
    /// dashboard-level action with no natural per-row `MenuAction`, so
    /// giving it a global chord would mean a new letter, a `filter_key` arm
    /// AND an `ACTIONS` row just to make the palette's copy of it do
    /// anything -- not the "line or two" that would earn its own binding, so
    /// it names no entry in `dash::actions::ACTIONS` and stays reachable by
    /// mouse only.
    ShowJevErrors,
    Zoom,
    /// Let the operator override automatic narrow-terminal sidebar hiding.
    ToggleSidebar,
    Quit,
    /// `Ctrl+A t` switches between the dashboard and the agent tree (#833).
    ToggleTree,
    /// `Ctrl+A y/d/]/g/a`: answer or browse the approvals inbox; inert while nothing is pending (#840).
    Approvals(ApprovalKey),
    /// `Ctrl+A ?` or `Ctrl+A h`/`H` -- opens the help overlay listing every
    /// binding below.
    Help,
    /// Ctrl+A p opens the searchable action palette (#354).
    Palette,
    /// Scroll the focused pane a half-screen back into its history
    /// (`Ctrl+A PageUp`) or toward the live view (`Ctrl+A PageDown`).
    ScrollPageUp,
    ScrollPageDown,
    /// Jump the focused pane to the oldest row it still has (`Ctrl+A Home`)
    /// or straight back to the live view (`Ctrl+A End`).
    ScrollTop,
    ScrollLive,
    /// The prefix key pressed again while armed: the operator meant to send
    /// the child a literal `Ctrl+A`, not invoke a dashboard command.
    LiteralPrefix,
}

/// The five approvals-inbox chords behind the prefix (#840).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApprovalKey {
    /// `y`: allow the shown request once.
    Allow,
    /// `Y`: allow it and apply the always-allow rule the harness offered, when it offered one.
    AllowAlways,
    /// `d`: deny it (`n` is already Nudge).
    Deny,
    /// `]`: show the next pending request.
    Next,
    /// `g`: release it to its pane and go there.
    Goto,
    /// `a`: the list across all dashboards.
    List,
}

/// Resolve pointer actions from the last drawn frame; only Grid routes reach the child (#354).
#[derive(Debug, PartialEq, Eq)]
pub(super) enum MouseRoute {
    /// Hand the event on to the existing grid path unchanged: child mouse
    /// forwarding, wheel scrollback and in-dashboard text selection.
    Grid,
    /// Swallowed: an overlay owns the pointer, mouse capture is off, or the
    /// gesture means nothing where it landed.
    Consume,
    /// Left click on a sidebar session row (by session short id).
    Select(String),
    /// Left click on the roster's summary line.
    Summary,
    /// Left click on a group header's disclosure triangle (by work-group id).
    Toggle(String),
    /// Wheel over the sidebar: scroll the roster viewport by this many
    /// entries, without moving the selection.
    ScrollRoster(isize),
    /// A chrome hit that maps onto a keyboard action the dash already has.
    Action(DashAction),
    /// Select an overlay row by index; the event loop decides whether a second click activates it (#354).
    OverlayRow(usize),
    /// Feed a dialog hint click to the same reducer key as its keyboard binding (#354).
    OverlayKey(KeyCode),
    /// Scroll the open dialog's list, not the underlying pane (#354).
    ScrollOverlay(isize),
}

/// Routing order is capture guard, modal overlay, captured gesture, chrome, then grid; only Grid may forward input to the child (#354).
pub(super) fn route_mouse(
    snap: &hit::FrameSnapshot,
    mouse: event::MouseEvent,
    capture: bool,
    overlay_open: bool,
    selecting: bool,
) -> MouseRoute {
    if !capture {
        return MouseRoute::Consume;
    }
    let hit = hit::hit_test(snap, mouse.column, mouse.row);
    let inside_dialog = matches!(hit, Hit::Overlay | Hit::OverlayRow(_) | Hit::OverlayHint(_));
    if overlay_open || inside_dialog || matches!(hit, Hit::ModalBackdrop) {
        return match (hit, mouse.kind) {
            (
                Hit::OverlayHint(HintId::DialogKey(code)),
                MouseEventKind::Down(MouseButton::Left),
            ) => MouseRoute::OverlayKey(code),
            (Hit::OverlayRow(index), MouseEventKind::Down(MouseButton::Left)) => {
                MouseRoute::OverlayRow(index)
            }
            // The wheel scrolls the dialog's own list, but only with the
            // pointer actually inside it: a notch on the backdrop is
            // consumed, exactly like a click there.
            (_, MouseEventKind::ScrollUp) if inside_dialog => MouseRoute::ScrollOverlay(-1),
            (_, MouseEventKind::ScrollDown) if inside_dialog => MouseRoute::ScrollOverlay(1),
            _ => MouseRoute::Consume,
        };
    }
    if selecting
        && matches!(
            mouse.kind,
            MouseEventKind::Drag(MouseButton::Left) | MouseEventKind::Up(MouseButton::Left)
        )
    {
        return MouseRoute::Grid;
    }
    // The wheel belongs to the whole sidebar column, not just its rows: a
    // notch over the summary line, a group header or the padding below the
    // last row scrolls the roster exactly the same way.
    if !snap.zoomed
        && snap
            .sidebar
            .contains(Position::new(mouse.column, mouse.row))
    {
        match mouse.kind {
            MouseEventKind::ScrollUp => return MouseRoute::ScrollRoster(-1),
            MouseEventKind::ScrollDown => return MouseRoute::ScrollRoster(1),
            _ => {}
        }
    }
    match (hit, mouse.kind) {
        (Hit::Grid, _) => MouseRoute::Grid,
        (Hit::SidebarRow(id), MouseEventKind::Down(MouseButton::Left)) => MouseRoute::Select(id),
        (Hit::SidebarRow(id), MouseEventKind::Down(MouseButton::Right)) => {
            MouseRoute::Action(DashAction::ContextMenu(id))
        }
        (Hit::SidebarSummary, MouseEventKind::Down(MouseButton::Left)) => MouseRoute::Summary,
        (Hit::GroupToggle(id), MouseEventKind::Down(MouseButton::Left)) => MouseRoute::Toggle(id),
        // Click affordance follow-up: only ever hit at all when the section
        // drew `errors > 0` (see `Hit::JevErrors`'s own doc comment), so
        // there is no zero-errors case to branch on here.
        (Hit::JevErrors, MouseEventKind::Down(MouseButton::Left)) => {
            MouseRoute::Action(DashAction::ShowJevErrors)
        }
        (Hit::HeaderHint(id), MouseEventKind::Down(MouseButton::Left)) => {
            MouseRoute::Action(match id {
                HintId::Actions => DashAction::ContextActions,
                HintId::Nudge => DashAction::Nudge,
                HintId::Mail => DashAction::Mail,
                HintId::Errors => DashAction::ShowErrors,
                HintId::Inspect => DashAction::Inspect,
                HintId::Restore => DashAction::RestoreRow,
                // A dialog hint is never in the header cluster; a
                // stray one is the always-safe action rather than a panic.
                HintId::Help | HintId::DialogKey(_) => DashAction::Help,
            })
        }
        _ => MouseRoute::Consume,
    }
}

/// Pure: clicking a row by session short id has exactly the effect the
/// keyboard's own select-and-focus has ([`apply_navigation`]'s `Switch`) --
/// for a row this dashboard actually owns. A view-only registry row or an
/// ended pane moves the cursor only, leaving input focus where it was, which
/// is the same rule arrow navigation follows (F7). An id that named a row
/// which has since been reaped is a no-op rather than a guess.
pub(super) fn select_row(
    id: &str,
    rows: &[ui::SidebarRow],
    selected: usize,
    focused: usize,
) -> (usize, usize) {
    let Some(index) = rows.iter().position(|r| r.short == id) else {
        return (selected, focused);
    };
    if rows[index].attached && rows[index].state != ui::RowState::Dead {
        apply_navigation(
            DashAction::Switch(index),
            selected,
            focused,
            rows.iter().filter(|r| r.attached).count(),
            rows.len(),
        )
    } else {
        (index, focused)
    }
}

/// Resolve session actions against the row cursor, never a stale pane index.
pub(super) fn session_target(
    chrome: Option<&Hit>,
    rows: &[ui::SidebarRow],
    selected: usize,
) -> Option<String> {
    if matches!(
        chrome,
        Some(Hit::SidebarSummary) | Some(Hit::GroupToggle(_))
    ) {
        return None;
    }
    rows.get(selected).map(|row| row.short.clone())
}

/// Allow a pending selection press in every pane while preserving ordinary clicks for child TUIs (#697).
pub(super) fn press_starts_selection(main: Rect, column: u16, row: u16) -> bool {
    main.contains(Position::new(column, row))
}

/// Pure: `SelectUp`/`SelectDown` over the *tree* the roster actually drew
/// (`order`, plus the summary line that always heads it), not over the flat
/// pane vector -- so the cursor walks onto group headers and the summary the
/// same way a click can reach them, and skips the children of a collapsed
/// group because they are not in `order` at all.
///
/// `chrome` holds the cursor whenever it is on a non-session entry; a session
/// entry clears it and moves the real `(selected, focused)` pair. Every other
/// navigation action (`Switch`, `NextPane`) is the pre-#354 behaviour
/// untouched.
pub(super) fn navigate_roster(
    action: DashAction,
    rows: &[ui::SidebarRow],
    order: &[Hit],
    selected: usize,
    focused: usize,
    chrome: &mut Option<Hit>,
) -> (usize, usize) {
    // Before the first draw populates the tree, navigate the flat row list so the first Down does not land on a phantom summary.
    if order.is_empty() || !matches!(action, DashAction::SelectUp | DashAction::SelectDown) {
        *chrome = None;
        return apply_navigation(
            action,
            selected,
            focused,
            rows.iter().filter(|r| r.attached).count(),
            rows.len(),
        );
    }
    let mut order = order.to_vec();
    order.insert(0, Hit::SidebarSummary);
    let current = chrome
        .clone()
        .or_else(|| rows.get(selected).map(|r| Hit::SidebarRow(r.short.clone())));
    let index = current
        .and_then(|id| order.iter().position(|r| *r == id))
        .unwrap_or(0);
    let index = if action == DashAction::SelectUp {
        index.saturating_sub(1)
    } else {
        index.saturating_add(1).min(order.len().saturating_sub(1))
    };
    match order.get(index) {
        Some(Hit::SidebarRow(id)) => {
            *chrome = None;
            select_row(id, rows, selected, focused)
        }
        Some(id) => {
            *chrome = Some(id.clone());
            (selected, focused)
        }
        None => (selected, focused),
    }
}

/// Folds the throttled [`FactsCache`] reads into the rows `assemble_sidebar`
/// built from the panes alone. Everything here comes from a value already
/// cached on the facts cadence -- the work group's scope, when this pane last
/// changed state, the harness's own usage window -- so a frame never reads
/// the disk and never shells out. Keys whose fact has no cached value keep
/// the placeholder `assemble_sidebar` put there.
pub(super) fn enrich_sidebar(rows: &mut [ui::SidebarRow], disk: &DiskFacts, now: u64) {
    for row in rows {
        // Use cached attention status for glyph, rollups and reason; render must not read it from disk (#354).
        row.status = disk.attention.get(&row.short).cloned();
        if let Some(group) = row.group.as_mut()
            && let Some(cached) = disk.groups.get(&group.id)
        {
            group.scope = cached.scope.clone();
            if let Some((_, value)) = row.disclosure.iter_mut().find(|(key, _)| key == "group") {
                *value = value.replacen(style::PLACEHOLDER, &group.scope, 1);
            }
        }
        // An Unknown projection preserves the row's lifecycle word; never
        // fabricate an attention state from a missing status file.
        if let Some(status) = row.status.as_ref() {
            let projection = super::attention::project(status);
            if projection != super::attention::Projection::Unknown {
                row.fact_state = projection_word(projection);
            }
        }
        // This row's own bound workflow and unread mail, from the same
        // throttled per-session reads `FactsCache::refresh_if_due` already
        // did this tick -- never a fallback to the repo-wide pointer.
        row.workflow = disk.workflow_by_session.get(&row.short).cloned();
        row.unread_mail = disk
            .mail_by_session
            .get(&row.short)
            .map(|(broadcast, direct)| broadcast + direct)
            .unwrap_or(0);
        // A retained ended row's `since`/fact are already final (`exited
        // <age> · exit <code>`, built by `assemble_sidebar` from facts
        // frozen at the reap); nothing cached may overwrite them.
        if row.exit_code.is_some() {
            continue;
        }
        // `since` prefers the attention model's own last transition -- the
        // moment the projection actually changed, which is what "waiting 1m"
        // in the approved frame means -- and falls back to the dashboard's own
        // observed `RowState` clock when no status has ever been recorded.
        let transition = row
            .status
            .as_ref()
            .filter(|s| s.last_transition > 0)
            .map(|s| (lifecycle_word(s.lifecycle), s.last_transition))
            .or_else(|| {
                disk.state_since
                    .get(&row.short)
                    .map(|(_, at)| (row_state_label(row.state), *at))
            });
        if let Some((word, at)) = transition {
            row.fact_since_secs = Some(now.saturating_sub(at));
            if let Some((_, value)) = row.disclosure.iter_mut().find(|(key, _)| key == "since") {
                *value = format!(
                    "{word} {} \u{b7} started {} ago",
                    style::format_age(now.saturating_sub(at)),
                    row.age_secs
                        .map(style::format_age)
                        .unwrap_or_else(|| style::PLACEHOLDER.into())
                );
            }
        }
        if let Some(usage) = disk
            .usage
            .iter()
            .find(|u| u.name == row.harness)
            .and_then(|u| u.five_hour)
            && let Some((_, value)) = row.disclosure.iter_mut().find(|(key, _)| key == "budget")
        {
            value.push_str(&format!(" \u{b7} 5h {usage:.0}%"));
        }
    }
}

/// Match both modifier-bearing Ctrl+A and the raw C0 byte Windows VT input can send.
pub(super) fn is_prefix_key(key: &KeyEvent) -> bool {
    match key.code {
        KeyCode::Char('a') | KeyCode::Char('A') => key.modifiers.contains(KeyModifiers::CONTROL),
        KeyCode::Char('\u{01}') => true,
        _ => false,
    }
}

/// Return the next prefix state with every verdict; every path disarms except
/// the prefix key itself, so an unrelated key cannot leave the prefix armed.
pub fn filter_key(prefix_armed: bool, key: KeyEvent) -> (bool, InputVerdict) {
    if !prefix_armed {
        if is_prefix_key(&key) {
            return (true, InputVerdict::Pending);
        }
        return (false, InputVerdict::ToChild(encode_key(key)));
    }

    if is_prefix_key(&key) {
        return (false, InputVerdict::Dash(DashAction::LiteralPrefix));
    }

    // A chord is an UNMODIFIED key. The table below matches `key.code` alone,
    // so without this `^A` followed by `Ctrl+Q` quit the dashboard -- and every
    // pane's child with it -- when the operator only meant to send a control
    // byte; `Ctrl+C`/`Ctrl+S`/`Ctrl+Z` fired their chords the same way. Only
    // the literal-prefix case above carries a modifier and still means
    // something here.
    if key
        .modifiers
        .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
    {
        return (false, InputVerdict::ToChild(Vec::new()));
    }

    let action = match key.code {
        KeyCode::Tab => Some(DashAction::NextPane),
        KeyCode::Up => Some(DashAction::SelectUp),
        KeyCode::Down => Some(DashAction::SelectDown),
        // Left and Right fold or unfold the selected work group (#354).
        KeyCode::Left => Some(DashAction::CollapseGroup),
        KeyCode::Right => Some(DashAction::ExpandGroup),
        // Scrollback, behind the prefix only. The bare keys deliberately keep
        // passing through to the child (`encode_key` sends them as `CSI 5~`/
        // `CSI 6~`): a harness has its own paging, and stealing PageUp from it
        // would be a regression traded for a feature.
        KeyCode::PageUp => Some(DashAction::ScrollPageUp),
        KeyCode::PageDown => Some(DashAction::ScrollPageDown),
        KeyCode::Home => Some(DashAction::ScrollTop),
        KeyCode::End => Some(DashAction::ScrollLive),
        KeyCode::Char(c) if c.is_ascii_digit() && c != '0' => {
            Some(DashAction::Switch((c as u8 - b'1') as usize))
        }
        KeyCode::Char('s') => Some(DashAction::Spawn),
        // Open the selected row's action menu (#354).
        KeyCode::Char('c') => Some(DashAction::ContextActions),
        KeyCode::Char('n') => Some(DashAction::Nudge),
        KeyCode::Char('m') => Some(DashAction::Mail),
        KeyCode::Char('M') => Some(DashAction::Memory),
        KeyCode::Char('o') => Some(DashAction::Handover),
        KeyCode::Char('e') => Some(DashAction::ShowErrors),
        // Open the selected row's inspector (#354).
        KeyCode::Char('i') => Some(DashAction::Inspect),
        // Restore the selected retained ended row (#354).
        KeyCode::Char('r') => Some(DashAction::RestoreRow),
        // Open the searchable action palette (#354).
        KeyCode::Char('p') => Some(DashAction::Palette),
        KeyCode::Char('z') => Some(DashAction::Zoom),
        // Toggle the session sidebar even below its automatic width threshold.
        KeyCode::Char('b') => Some(DashAction::ToggleSidebar),
        KeyCode::Char('q') => Some(DashAction::Quit),
        KeyCode::Char('t') => Some(DashAction::ToggleTree),
        KeyCode::Char('y') => Some(DashAction::Approvals(ApprovalKey::Allow)),
        KeyCode::Char('Y') => Some(DashAction::Approvals(ApprovalKey::AllowAlways)),
        KeyCode::Char('d') => Some(DashAction::Approvals(ApprovalKey::Deny)),
        KeyCode::Char(']') => Some(DashAction::Approvals(ApprovalKey::Next)),
        KeyCode::Char('g') => Some(DashAction::Approvals(ApprovalKey::Goto)),
        KeyCode::Char('a') => Some(DashAction::Approvals(ApprovalKey::List)),
        KeyCode::Char('?') | KeyCode::Char('h') | KeyCode::Char('H') => Some(DashAction::Help),
        _ => None,
    };

    match action {
        Some(action) => (false, InputVerdict::Dash(action)),
        // An armed prefix followed by a key with no dashboard meaning
        // disarms and forwards nothing -- never leaks a stray keystroke to
        // the child that the operator only meant as a (failed) command.
        None => (false, InputVerdict::ToChild(Vec::new())),
    }
}

/// Pure: the xterm modifier parameter for a modified special key --
/// `1 + Shift + 2*Alt + 4*Ctrl` -- or `None` when no modifier of interest is
/// set, so the caller emits the bare, unmodified escape. This is the standard
/// `CSI 1 ; <mod> <final>` / `CSI <n> ; <mod> ~` encoding every terminal and
/// harness reads (M7).
pub(super) fn xterm_modifier(mods: KeyModifiers) -> Option<u8> {
    let mut bits = 0u8;
    if mods.contains(KeyModifiers::SHIFT) {
        bits |= 1;
    }
    if mods.contains(KeyModifiers::ALT) {
        bits |= 2;
    }
    if mods.contains(KeyModifiers::CONTROL) {
        bits |= 4;
    }
    if bits == 0 { None } else { Some(1 + bits) }
}

/// A cursor/navigation key that ends in a letter final (`A`/`B`/`C`/`D` for the
/// arrows, `H`/`F` for Home/End): bare `CSI <final>` when unmodified, the
/// modified `CSI 1 ; <mod> <final>` form otherwise (M7).
pub(super) fn csi_letter_final(final_byte: u8, mods: KeyModifiers) -> Vec<u8> {
    match xterm_modifier(mods) {
        Some(m) => format!("\x1b[1;{m}{}", final_byte as char).into_bytes(),
        None => vec![0x1b, b'[', final_byte],
    }
}

/// A navigation key that ends in a tilde (`CSI <n> ~`, e.g. PageUp `5`,
/// PageDown `6`, Delete `3`, Insert `2`): the modified `CSI <n> ; <mod> ~`
/// form when a modifier is held, the bare `CSI <n> ~` otherwise (M7).
pub(super) fn csi_tilde(n: u8, mods: KeyModifiers) -> Vec<u8> {
    match xterm_modifier(mods) {
        Some(m) => format!("\x1b[{n};{m}~").into_bytes(),
        None => format!("\x1b[{n}~").into_bytes(),
    }
}

/// Map nonalphabetic Ctrl keys to C0 bytes, including the Ctrl+Alt path.
pub(super) fn control_byte(c: char) -> Vec<u8> {
    match c {
        ' ' => vec![0x00],  // Ctrl+Space -> NUL
        '4' => vec![0x1c],  // Ctrl+\  (legacy alias, delivered as Char('4'))
        '5' => vec![0x1d],  // Ctrl+]  (legacy alias, delivered as Char('5'))
        '6' => vec![0x1e],  // Ctrl+^  (legacy alias, delivered as Char('6'))
        '7' => vec![0x1f],  // Ctrl+_  (legacy alias, delivered as Char('7'))
        '\\' => vec![0x1c], // Ctrl+\  (literal, delivered once kitty is negotiated)
        ']' => vec![0x1d],  // Ctrl+]  (literal, delivered once kitty is negotiated)
        '^' => vec![0x1e],  // Ctrl+^  (literal, delivered once kitty is negotiated)
        '_' => vec![0x1f],  // Ctrl+_  (literal, delivered once kitty is negotiated)
        '/' => vec![0x1f],  // Ctrl+/  (kitty delivers the literal; same C0 as Ctrl+_)
        '@' => vec![0x00],  // Ctrl+@  (kitty delivers the literal; NUL)
        _ => {
            let upper = c.to_ascii_uppercase();
            if upper.is_ascii_alphabetic() {
                vec![(upper as u8) & 0x1f]
            } else {
                let mut buf = [0u8; 4];
                c.encode_utf8(&mut buf).as_bytes().to_vec()
            }
        }
    }
}

/// Encode modified navigation keys with xterm CSI forms and control combinations as C0 bytes; preserve legacy digit aliases from unnegotiated terminals.
pub fn encode_key(key: KeyEvent) -> Vec<u8> {
    if key.modifiers.contains(KeyModifiers::ALT)
        && let KeyCode::Char(c) = key.code
    {
        // Meta is a prefix ESC ON TOP of whatever the key already encodes to,
        // so `Ctrl+Alt+<x>` is `ESC` plus the C0 byte -- not `ESC` plus the
        // literal character, which dropped the control bit entirely.
        let mut bytes = vec![0x1b];
        bytes.extend_from_slice(&if key.modifiers.contains(KeyModifiers::CONTROL) {
            control_byte(c)
        } else {
            let mut buf = [0u8; 4];
            c.encode_utf8(&mut buf).as_bytes().to_vec()
        });
        return bytes;
    }
    match key.code {
        // Encode Shift+Enter as ESC CR for newline, while bare Enter submits; Windows Terminal may present its configured Shift+Enter as Alt+Enter.
        KeyCode::Enter => {
            if key
                .modifiers
                .intersects(KeyModifiers::SHIFT | KeyModifiers::ALT)
            {
                b"\x1b\r".to_vec()
            } else {
                b"\r".to_vec()
            }
        }
        KeyCode::Backspace => vec![0x7f],
        KeyCode::Tab => b"\t".to_vec(),
        KeyCode::BackTab => b"\x1b[Z".to_vec(),
        KeyCode::Left => csi_letter_final(b'D', key.modifiers),
        KeyCode::Right => csi_letter_final(b'C', key.modifiers),
        KeyCode::Up => csi_letter_final(b'A', key.modifiers),
        KeyCode::Down => csi_letter_final(b'B', key.modifiers),
        KeyCode::Home => csi_letter_final(b'H', key.modifiers),
        KeyCode::End => csi_letter_final(b'F', key.modifiers),
        KeyCode::PageUp => csi_tilde(5, key.modifiers),
        KeyCode::PageDown => csi_tilde(6, key.modifiers),
        KeyCode::Delete => csi_tilde(3, key.modifiers),
        KeyCode::Insert => csi_tilde(2, key.modifiers),
        KeyCode::Esc => vec![0x1b],
        KeyCode::F(n) => match n {
            1 => b"\x1bOP".to_vec(),
            2 => b"\x1bOQ".to_vec(),
            3 => b"\x1bOR".to_vec(),
            4 => b"\x1bOS".to_vec(),
            5 => b"\x1b[15~".to_vec(),
            6 => b"\x1b[17~".to_vec(),
            7 => b"\x1b[18~".to_vec(),
            8 => b"\x1b[19~".to_vec(),
            9 => b"\x1b[20~".to_vec(),
            10 => b"\x1b[21~".to_vec(),
            11 => b"\x1b[23~".to_vec(),
            12 => b"\x1b[24~".to_vec(),
            _ => Vec::new(),
        },
        KeyCode::Char(c) if key.modifiers.contains(KeyModifiers::CONTROL) => control_byte(c),
        KeyCode::Char(c) => {
            let mut buf = [0u8; 4];
            c.encode_utf8(&mut buf).as_bytes().to_vec()
        }
        _ => Vec::new(),
    }
}

/// The literal bytes `Ctrl+A` itself encodes to, reused for
/// `DashAction::LiteralPrefix` rather than hard-coded a second time: single
/// source of truth with `PREFIX`.
pub(super) fn literal_prefix_bytes() -> Vec<u8> {
    encode_key(KeyEvent::new(PREFIX.1, PREFIX.0))
}

/// How many rows one wheel notch scrolls a pane. Three is the near-universal
/// terminal/pager default, and a wheel that moves a single row feels broken.
///
/// Applies to the dashboard's *own* scrollback only. When the event is
/// forwarded to a child that asked for mouse reporting
/// (`Pane::scroll_wheel`), one notch in is one notch out -- how many rows that
/// moves is the child's decision to make, exactly as it would be in a real
/// terminal.
pub(super) const WHEEL_STEP: isize = 3;

/// Names an open overlay for the key diagnostic. Diagnostic-only: an overlay
/// consumes a keystroke instead of `filter_key`, and "which one" is the whole
/// answer to "why did `Ctrl+A q` do nothing".
pub(super) fn overlay_name(overlay: &ui::Overlay) -> &'static str {
    match overlay {
        ui::Overlay::None => "none",
        ui::Overlay::QuitConfirm(_) => "quit-confirm",
        ui::Overlay::Spawn(_) => "spawn",
        ui::Overlay::Nudge(_) => "nudge",
        ui::Overlay::Handover(_) => "handover",
        ui::Overlay::Mail(_) => "mail",
        ui::Overlay::Memory(_) => "memory",
        ui::Overlay::Restore(_) => "restore",
        ui::Overlay::Palette(view) => view.mode.title(),
        ui::Overlay::Errors(_) => "errors",
        ui::Overlay::JevErrors(_) => "jev errors",
        ui::Overlay::Approvals(_) => "approvals",
        ui::Overlay::Menu(_) => "actions",
        ui::Overlay::Inspector(_) => "inspect",
    }
}

/// Tag a pending double-click with overlay name and target so another dialog or row cannot inherit it.
pub(super) fn overlay_identity(overlay: &ui::Overlay) -> (&'static str, String) {
    let subject = match overlay {
        ui::Overlay::Menu(view) => view.target.clone(),
        ui::Overlay::Inspector(view) => view.target.clone(),
        ui::Overlay::Handover(draft) => draft.target_short.clone(),
        // Include palette query in dialog identity so filtering between clicks cannot activate a different row.
        ui::Overlay::Palette(view) => view.query.clone(),
        _ => String::new(),
    };
    (overlay_name(overlay), subject)
}

/// Trust overlay hit routes only while the live dialog identity matches the frame snapshot; queued events can outlive the overlay they hit (#354).
pub(super) fn overlay_route_is_current(
    route: &MouseRoute,
    live_overlay: &ui::Overlay,
    snapshot_overlay_ident: &(&'static str, String),
) -> bool {
    if !matches!(
        route,
        MouseRoute::OverlayRow(_) | MouseRoute::OverlayKey(_) | MouseRoute::ScrollOverlay(_)
    ) {
        return true;
    }
    overlay_identity(live_overlay) == *snapshot_overlay_ident
}

/// The first-run tip's own flag file: `<state>/dash/tip-seen`. Operator-level
/// and repo-independent (`StateDir::dash()` is the dashboard's own state
/// root), deliberately NOT a config key -- it is a fact about this operator's
/// history, not something to configure.
pub(super) fn first_run_tip_flag(state: &StateDir) -> PathBuf {
    state.dash().join("tip-seen")
}

/// Impure, best effort: has this operator seen the first-run tip already?
/// A missing or unreadable state directory answers "no", which shows the tip
/// again -- the harmless direction.
pub(super) fn first_run_tip_seen(state: &StateDir) -> bool {
    first_run_tip_flag(state).exists()
}

/// Impure, best effort: record that the tip has been shown. Every error is
/// deliberately swallowed -- a read-only state directory must cost the
/// operator a repeated tip, never an error line, and certainly never a panic
/// on the dashboard's own launch path.
pub(super) fn mark_first_run_tip_seen(state: &StateDir) {
    let path = first_run_tip_flag(state);
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(&path, b"1\n");
}

/// Set `ZIRV_CTX_DASH_KEYLOG` to a path and the dashboard appends one line per
/// input event it reads. Unset -- the normal case -- there is no file handle,
/// no formatting and no branch worth the name: [`KeyLog::from_env`] returns
/// `None` and every call site is an `if let Some(..)` over it.
///
/// Deliberately an environment variable read straight from the process env
/// rather than a `ctx.toml` key: it is a diagnostic an operator turns on for
/// one run to answer "what is my terminal actually delivering", not a
/// configuration surface with a trust story to get right. Nothing reads the
/// file back, and nothing behaves differently because it is on.
pub(super) const KEYLOG_ENV: &str = "ZIRV_CTX_DASH_KEYLOG";

/// A tick this long or longer is worth a `TICK` line of its own even when
/// nothing about the loop state changed -- it is a tick the operator felt.
/// The loop's own input poll is 10ms hot / 50ms idle, so anything at this
/// scale is maintenance or drawing, not waiting for a keystroke.
pub(super) const KEYLOG_SLOW_TICK: Duration = Duration::from_millis(100);

/// Watch prefix, overlay, pane focus and alternate-screen state only when they change, keeping per-tick logging cheap.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct LoopState {
    pub(super) prefix_armed: bool,
    pub(super) overlay: &'static str,
    pub(super) panes: usize,
    pub(super) focused: usize,
    pub(super) focused_alt: bool,
}

/// Append diagnostic events best-effort so logging failures cannot break a session. Event, dispatch, overlay and tick records distinguish lost input from downstream effects.
pub(super) struct KeyLog {
    file: std::fs::File,
    /// Monotonic, so the timestamps are readable deltas rather than wall
    /// clock -- what matters is the gap between two keystrokes, and whether a
    /// keystroke arrived at all.
    start: Instant,
    /// Bumped once per event-loop iteration and stamped on every line, so an
    /// event and the state around it can be placed on the same tick -- the
    /// difference between "armed was cleared by the next keystroke" and
    /// "armed was cleared by the loop with no keystroke at all".
    tick: u64,
    /// The last state a `TICK` line reported. The loop polls at 50ms, so an
    /// unconditional line per iteration would be twenty a second of nothing;
    /// only a change is worth a line.
    last: Option<LoopState>,
}

impl KeyLog {
    pub(super) fn from_env() -> Option<KeyLog> {
        let path = std::env::var_os(KEYLOG_ENV)?;
        if path.is_empty() {
            return None;
        }
        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .ok()?;
        Some(KeyLog {
            file,
            start: Instant::now(),
            tick: 0,
            last: None,
        })
    }

    pub(super) fn line(&mut self, body: &str) {
        let ms = self.start.elapsed().as_millis();
        let tick = self.tick;
        let _ = writeln!(self.file, "{ms:>9}ms t{tick:<7} {body}");
        // Flushed every line: the session this is diagnosing is one that may
        // well be killed from outside, and a buffered tail helps nobody.
        let _ = self.file.flush();
    }

    /// Record input reachability at event-loop startup.
    pub(super) fn startup(
        &mut self,
        cfg: &CtxConfig,
        size: (u16, u16),
        stdin_tty: bool,
        stdout_tty: bool,
    ) {
        self.line(&format!(
            "START loop=dash size={}x{} stdin_tty={stdin_tty} stdout_tty={stdout_tty} \
             dash.enabled={} dash.mouse={} pid={}",
            size.0,
            size.1,
            cfg.dash.enabled,
            cfg.dash.mouse,
            std::process::id()
        ));
    }

    /// Write a tick record only when watched state changes, avoiding a log entry for every poll.
    pub(super) fn tick(&mut self, state: LoopState, previous_tick: Duration) {
        self.tick = self.tick.saturating_add(1);
        let ms = previous_tick.as_millis();
        let slow = previous_tick >= KEYLOG_SLOW_TICK;
        if self.last == Some(state) && !slow {
            return;
        }
        let previous = self.last;
        self.last = Some(state);
        match previous {
            None => self.line(&format!(
                "TICK armed={} overlay={} panes={} focused={} alt_screen={} dur={ms}ms (first)",
                state.prefix_armed, state.overlay, state.panes, state.focused, state.focused_alt
            )),
            Some(prev) if prev == state => self.line(&format!(
                "TICK armed={} overlay={} panes={} focused={} alt_screen={} dur={ms}ms (slow)",
                state.prefix_armed, state.overlay, state.panes, state.focused, state.focused_alt
            )),
            Some(prev) => self.line(&format!(
                "TICK armed={}->{} overlay={}->{} panes={}->{} focused={}->{} \
                 alt_screen={}->{} dur={ms}ms",
                prev.prefix_armed,
                state.prefix_armed,
                prev.overlay,
                state.overlay,
                prev.panes,
                state.panes,
                prev.focused,
                state.focused,
                prev.focused_alt,
                state.focused_alt
            )),
        }
    }

    /// One line per scroll request, whatever it did. The scrolling bug was
    /// reported twice with nothing but "it does not scroll" to work from, so
    /// this records every fact that separates the branches: whether the
    /// focused pane was on the alternate screen (where vt100 keeps no
    /// scrollback at all), whether its child had asked to be sent mouse events
    /// (in which case the wheel is *its* event, not ours), the scrollback
    /// offset either side of the request, and which branch was taken -- so a
    /// capture showing `alt_screen=true mouse=true branch=forwarded-mouse`
    /// settles it without another guess.
    pub(super) fn scroll(
        &mut self,
        action: &str,
        alt_screen: bool,
        wants_mouse: bool,
        before: usize,
        after: usize,
        outcome: ScrollOutcome,
    ) {
        let branch = match outcome {
            ScrollOutcome::ForwardedMouse => "forwarded-mouse",
            ScrollOutcome::FullScreen => "none (alternate screen has no scrollback)",
            _ => "scrollback",
        };
        self.line(&format!(
            "SCROLL {action} alt_screen={alt_screen} mouse={wants_mouse} \
             scrollback {before}->{after} branch={branch} outcome={outcome:?}"
        ));
    }

    /// Record event, prior prefix, overlay and verdict so lost input can be distinguished from swallowed or ineffectual input.
    pub(super) fn observe<E: std::fmt::Display>(
        &mut self,
        read: &Result<Event, E>,
        prefix_armed: bool,
        overlay: &ui::Overlay,
    ) {
        match read {
            Ok(Event::Key(key)) if key.kind == KeyEventKind::Press => {
                let outcome = if matches!(overlay, ui::Overlay::None) {
                    format!(
                        "predicted filter_key -> {:?}",
                        filter_key(prefix_armed, *key)
                    )
                } else {
                    "will be consumed by the overlay".to_string()
                };
                self.line(&format!(
                    "EVENT {:?} | armed_before={prefix_armed} | overlay={} | {outcome}",
                    Event::Key(*key),
                    overlay_name(overlay)
                ));
            }
            // Non-`Press` key events (Windows delivers a Release for every
            // key, plus a stray one at startup left over from the shell that
            // launched the process) land here too, deliberately: "the loop saw
            // it and dropped it" is a different fact from "it never arrived".
            Ok(event) => self.line(&format!("EVENT {event:?}")),
            Err(e) => self.line(&format!("READ-ERR {e}")),
        }
    }

    /// Record every key verdict and stored prefix state, including actions with no visible effect.
    pub(super) fn dispatch(
        &mut self,
        armed_before: bool,
        armed_after: bool,
        verdict: &InputVerdict,
    ) {
        let rendered = match verdict {
            // A `ToChild` payload is the operator's own typing; log its length
            // rather than its bytes, which is enough to tell "forwarded" from
            // "swallowed" without writing what they typed into a file.
            InputVerdict::ToChild(bytes) => format!("ToChild({} bytes)", bytes.len()),
            other => format!("{other:?}"),
        };
        self.line(&format!(
            "DISPATCH armed {armed_before}->{armed_after} | verdict={rendered}"
        ));
    }

    /// Take and restore the overlay slot once per reduction so an opened dialog is not immediately cleared.
    pub(super) fn overlay_swap(&mut self, took: &'static str, now: &ui::Overlay) {
        self.line(&format!("OVERLAY took={took} now={}", overlay_name(now)));
    }
}

#[cfg(test)]
mod tests {

    /// Issue #840: the five approvals chords sit behind the prefix and do not shadow Nudge (`n`).
    #[test]
    fn the_approvals_chords_are_prefixed_and_leave_nudge_alone() {
        for (code, expected) in [
            ('y', ApprovalKey::Allow),
            ('Y', ApprovalKey::AllowAlways),
            ('d', ApprovalKey::Deny),
            (']', ApprovalKey::Next),
            ('g', ApprovalKey::Goto),
            ('a', ApprovalKey::List),
        ] {
            assert_eq!(
                filter_key(true, key(KeyCode::Char(code), KeyModifiers::NONE)).1,
                InputVerdict::Dash(DashAction::Approvals(expected)),
                "^A {code}"
            );
            assert_eq!(
                filter_key(false, key(KeyCode::Char(code), KeyModifiers::NONE)).1,
                InputVerdict::ToChild(encode_key(key(KeyCode::Char(code), KeyModifiers::NONE))),
                "unprefixed {code} still belongs to the pane"
            );
        }
        assert_eq!(
            filter_key(true, key(KeyCode::Char('n'), KeyModifiers::NONE)).1,
            InputVerdict::Dash(DashAction::Nudge)
        );
    }
    use super::super::tests::*;
    use super::*;

    #[test]
    fn plain_keys_pass_to_the_child() {
        let (armed, v) = filter_key(false, key(KeyCode::Char('x'), KeyModifiers::NONE));
        assert!(!armed);
        assert!(matches!(v, InputVerdict::ToChild(b) if b == b"x"));
    }

    #[test]
    fn tab_passes_to_the_child_unprefixed() {
        let (armed, v) = filter_key(false, key(KeyCode::Tab, KeyModifiers::NONE));
        assert!(!armed);
        assert!(matches!(v, InputVerdict::ToChild(b) if b == b"\t"));
    }

    #[test]
    fn prefix_arms_and_swallows() {
        let (armed, v) = filter_key(false, key(KeyCode::Char('a'), KeyModifiers::CONTROL));
        assert!(armed);
        assert!(matches!(v, InputVerdict::Pending));
    }

    #[test]
    fn armed_tab_switches_and_disarms() {
        let (armed, v) = filter_key(true, key(KeyCode::Tab, KeyModifiers::NONE));
        assert!(!armed);
        assert!(matches!(v, InputVerdict::Dash(DashAction::NextPane)));
    }

    #[test]
    fn armed_digit_switches_to_that_pane() {
        let (_, v) = filter_key(true, key(KeyCode::Char('1'), KeyModifiers::NONE));
        assert!(matches!(v, InputVerdict::Dash(DashAction::Switch(0))));
        let (_, v) = filter_key(true, key(KeyCode::Char('9'), KeyModifiers::NONE));
        assert!(matches!(v, InputVerdict::Dash(DashAction::Switch(8))));
    }

    #[test]
    fn armed_ctrl_a_sends_a_literal_ctrl_a() {
        let (armed, v) = filter_key(true, key(KeyCode::Char('a'), KeyModifiers::CONTROL));
        assert!(!armed);
        assert!(matches!(v, InputVerdict::Dash(DashAction::LiteralPrefix)));
        assert_eq!(literal_prefix_bytes(), vec![0x01]);
    }

    #[test]
    fn armed_arrows_move_the_pane_selection() {
        let (armed, v) = filter_key(true, key(KeyCode::Up, KeyModifiers::NONE));
        assert!(!armed);
        assert!(matches!(v, InputVerdict::Dash(DashAction::SelectUp)));

        let (armed, v) = filter_key(true, key(KeyCode::Down, KeyModifiers::NONE));
        assert!(!armed);
        assert!(matches!(v, InputVerdict::Dash(DashAction::SelectDown)));

        // Unarmed arrows still pass to the child (claude uses them).
        let (armed, v) = filter_key(false, key(KeyCode::Up, KeyModifiers::NONE));
        assert!(!armed);
        assert!(matches!(v, InputVerdict::ToChild(b) if b == b"\x1b[A"));
    }

    #[test]
    fn prefix_matches_the_raw_control_byte_shape_too() {
        // Windows can deliver Ctrl+A as the raw SOH byte with no modifier
        // flag (docs/superpowers/notes/2026-08-13-vt100-spike.md).
        let (armed, v) = filter_key(false, key(KeyCode::Char('\u{01}'), KeyModifiers::NONE));
        assert!(armed);
        assert!(matches!(v, InputVerdict::Pending));

        let (armed, v) = filter_key(true, key(KeyCode::Char('\u{01}'), KeyModifiers::NONE));
        assert!(!armed);
        assert!(matches!(v, InputVerdict::Dash(DashAction::LiteralPrefix)));
    }

    #[test]
    fn armed_unknown_key_disarms_and_forwards_nothing() {
        let (armed, v) = filter_key(true, key(KeyCode::Char('x'), KeyModifiers::NONE));
        assert!(!armed);
        assert!(matches!(v, InputVerdict::ToChild(b) if b.is_empty()));
    }

    /// The chord table matched `KeyCode` alone, so `^A` then `Ctrl+Q` quit the
    /// whole dashboard -- every pane's child with it -- when the operator was
    /// only sending a control byte. A chord is an unmodified key.
    #[test]
    fn an_armed_prefix_followed_by_a_modified_key_is_not_a_chord() {
        let (armed, v) = filter_key(true, key(KeyCode::Char('q'), KeyModifiers::CONTROL));
        assert!(!armed);
        assert!(
            matches!(v, InputVerdict::ToChild(ref b) if b.is_empty()),
            "got {v:?}"
        );

        let (armed, v) = filter_key(true, key(KeyCode::Char('q'), KeyModifiers::NONE));
        assert!(!armed);
        assert!(matches!(v, InputVerdict::Dash(DashAction::Quit)));
    }

    /// The ALT fast-path runs before the CONTROL arm, so `Ctrl+Alt+<x>` used
    /// to encode as plain `Alt+<x>` -- the control bit was dropped and the
    /// child saw a literal letter. Meta is a prefix ESC on top of the C0 byte,
    /// not instead of it.
    #[test]
    fn alt_carries_the_control_bit_through_to_the_c0_byte() {
        let both = KeyModifiers::CONTROL | KeyModifiers::ALT;
        assert_eq!(encode_key(key(KeyCode::Char('m'), both)), b"\x1b\r");
        assert_eq!(encode_key(key(KeyCode::Char(' '), both)), b"\x1b\0");
        assert_eq!(encode_key(key(KeyCode::Char(']'), both)), b"\x1b\x1d");
        assert_eq!(
            encode_key(key(KeyCode::Char('m'), KeyModifiers::ALT)),
            b"\x1bm",
            "plain Alt is unchanged"
        );
    }

    #[test]
    fn encode_key_covers_the_terminal_basics() {
        assert_eq!(encode_key(key(KeyCode::Enter, KeyModifiers::NONE)), b"\r");
        assert_eq!(encode_key(key(KeyCode::Up, KeyModifiers::NONE)), b"\x1b[A");
        assert_eq!(
            encode_key(key(KeyCode::Char('c'), KeyModifiers::CONTROL)),
            vec![0x03]
        );
        assert_eq!(
            encode_key(key(KeyCode::BackTab, KeyModifiers::SHIFT)),
            b"\x1b[Z"
        );
    }

    #[test]
    fn encode_key_passes_a_raw_control_byte_through_unchanged() {
        // No CONTROL modifier flag -- the raw-control-byte shape from the
        // spike note -- still round-trips to the same single byte, since
        // ASCII control characters are their own UTF-8 encoding.
        assert_eq!(
            encode_key(key(KeyCode::Char('\u{01}'), KeyModifiers::NONE)),
            vec![0x01]
        );
    }

    #[test]
    fn armed_spawn_nudge_mail_memory_keys_are_recognised() {
        assert!(matches!(
            filter_key(true, key(KeyCode::Char('s'), KeyModifiers::NONE)).1,
            InputVerdict::Dash(DashAction::Spawn)
        ));
        assert!(matches!(
            filter_key(true, key(KeyCode::Char('n'), KeyModifiers::NONE)).1,
            InputVerdict::Dash(DashAction::Nudge)
        ));
        assert!(matches!(
            filter_key(true, key(KeyCode::Char('m'), KeyModifiers::NONE)).1,
            InputVerdict::Dash(DashAction::Mail)
        ));
        assert!(matches!(
            filter_key(true, key(KeyCode::Char('M'), KeyModifiers::NONE)).1,
            InputVerdict::Dash(DashAction::Memory)
        ));
        assert!(matches!(
            filter_key(true, key(KeyCode::Char('e'), KeyModifiers::NONE)).1,
            InputVerdict::Dash(DashAction::ShowErrors)
        ));
        assert!(matches!(
            filter_key(true, key(KeyCode::Char('z'), KeyModifiers::NONE)).1,
            InputVerdict::Dash(DashAction::Zoom)
        ));
        // Issue #697: `Ctrl+A v` (select mode) is gone -- `v` is simply
        // unbound now, the same as any other key with no dashboard meaning.
        assert!(matches!(
            filter_key(true, key(KeyCode::Char('v'), KeyModifiers::NONE)).1,
            InputVerdict::ToChild(ref bytes) if bytes.is_empty()
        ));
        assert!(matches!(
            filter_key(true, key(KeyCode::Char('q'), KeyModifiers::NONE)).1,
            InputVerdict::Dash(DashAction::Quit)
        ));
        for c in ['?', 'h', 'H'] {
            assert!(matches!(
                filter_key(true, key(KeyCode::Char(c), KeyModifiers::NONE)).1,
                InputVerdict::Dash(DashAction::Help)
            ));
        }
        // A real terminal delivers SHIFT alongside '?' and 'H' (`?` is
        // shift-slash on most layouts, and 'H' is itself the shifted key);
        // the match is on `key.code` alone, so the modifier must not matter.
        for c in ['?', 'H'] {
            assert!(matches!(
                filter_key(true, key(KeyCode::Char(c), KeyModifiers::SHIFT)).1,
                InputVerdict::Dash(DashAction::Help)
            ));
        }
    }

    /// The keyboard half of scrollback: a half-screen step and the two jumps,
    /// all behind the prefix.
    #[test]
    fn armed_paging_keys_scroll_the_focused_pane() {
        assert_eq!(
            filter_key(true, key(KeyCode::PageUp, KeyModifiers::NONE)).1,
            InputVerdict::Dash(DashAction::ScrollPageUp)
        );
        assert_eq!(
            filter_key(true, key(KeyCode::PageDown, KeyModifiers::NONE)).1,
            InputVerdict::Dash(DashAction::ScrollPageDown)
        );
        assert_eq!(
            filter_key(true, key(KeyCode::Home, KeyModifiers::NONE)).1,
            InputVerdict::Dash(DashAction::ScrollTop)
        );
        assert_eq!(
            filter_key(true, key(KeyCode::End, KeyModifiers::NONE)).1,
            InputVerdict::Dash(DashAction::ScrollLive)
        );
        // Every prefix action disarms, scrolling included: `Ctrl+A PageUp
        // PageUp` must page the child, not scroll twice.
        assert!(!filter_key(true, key(KeyCode::PageUp, KeyModifiers::NONE)).0);
        assert!(!filter_key(true, key(KeyCode::End, KeyModifiers::NONE)).0);
    }

    /// The bare keys are NOT bound: a harness has its own paging and its own
    /// Home/End, and stealing them from the child would trade a regression for
    /// a feature. Unprefixed, all four still encode to the escape sequence the
    /// child expects.
    #[test]
    fn unprefixed_paging_keys_still_reach_the_child() {
        for (code, expected) in [
            (KeyCode::PageUp, &b"\x1b[5~"[..]),
            (KeyCode::PageDown, &b"\x1b[6~"[..]),
            (KeyCode::Home, &b"\x1b[H"[..]),
            (KeyCode::End, &b"\x1b[F"[..]),
        ] {
            let (armed, verdict) = filter_key(false, key(code, KeyModifiers::NONE));
            assert!(!armed);
            assert_eq!(
                verdict,
                InputVerdict::ToChild(expected.to_vec()),
                "{code:?} must pass through to the child unprefixed"
            );
        }
    }

    /// The diagnostic names whichever overlay is standing between a keystroke
    /// and `filter_key` -- the whole point of the log line, since an open
    /// overlay is one of the two ways `Ctrl+A <key>` can appear to do nothing.
    #[test]
    fn overlay_name_covers_every_variant() {
        assert_eq!(overlay_name(&ui::Overlay::None), "none");
        assert_eq!(
            overlay_name(&ui::Overlay::QuitConfirm(Vec::new())),
            "quit-confirm"
        );
        assert_eq!(
            overlay_name(&ui::Overlay::Spawn(ui::SpawnDraft::default())),
            "spawn"
        );
        assert_eq!(
            overlay_name(&ui::Overlay::Nudge(ui::NudgeDraft::default())),
            "nudge"
        );
        assert_eq!(
            overlay_name(&ui::Overlay::Mail(ui::MailView::default())),
            "mail"
        );
        assert_eq!(
            overlay_name(&ui::Overlay::Memory(ui::MemoryView::default())),
            "memory"
        );
        assert_eq!(
            overlay_name(&ui::Overlay::Restore(ui::RestoreView::default())),
            "restore"
        );
        assert_eq!(
            overlay_name(&ui::Overlay::Palette(ui::PaletteView {
                mode: ui::PaletteMode::Help,
                ..ui::PaletteView::default()
            })),
            "help"
        );
        assert_eq!(
            overlay_name(&ui::Overlay::Palette(ui::PaletteView::default())),
            "palette"
        );
    }

    /// The diagnostic must be completely inert unless the env var names a
    /// path, and must never be able to fail a session: an unwritable path is
    /// `None`, not an error.
    #[test]
    fn the_keylog_is_inert_without_its_env_var_and_never_fails_a_session() {
        // Serialised against nothing: this test owns the variable for its own
        // duration and restores it, the same shape `testenv`'s own guards use.
        struct EnvGuard(Option<std::ffi::OsString>);
        impl Drop for EnvGuard {
            fn drop(&mut self) {
                match self.0.take() {
                    Some(v) => unsafe { std::env::set_var(KEYLOG_ENV, v) },
                    None => unsafe { std::env::remove_var(KEYLOG_ENV) },
                }
            }
        }
        let _guard = EnvGuard(std::env::var_os(KEYLOG_ENV));

        unsafe { std::env::remove_var(KEYLOG_ENV) };
        assert!(
            KeyLog::from_env().is_none(),
            "no env var, no file handle at all"
        );

        unsafe { std::env::set_var(KEYLOG_ENV, "") };
        assert!(
            KeyLog::from_env().is_none(),
            "an empty value is 'off', not a path to the current directory"
        );

        // A path whose parent does not exist: open fails, and the failure is
        // swallowed rather than propagated.
        let missing = std::path::Path::new("no-such-dir-1a2b3c").join("keys.log");
        unsafe { std::env::set_var(KEYLOG_ENV, &missing) };
        assert!(
            KeyLog::from_env().is_none(),
            "an unopenable path degrades to no logging"
        );

        // And a real one appends, with the event and the verdict on one line.
        let tmp = tempfile::tempdir().expect("tempdir");
        let path = tmp.path().join("keys.log");
        unsafe { std::env::set_var(KEYLOG_ENV, &path) };
        let mut log = KeyLog::from_env().expect("a writable path logs");
        log.observe::<std::io::Error>(
            &Ok(Event::Key(key(KeyCode::Char('a'), KeyModifiers::CONTROL))),
            false,
            &ui::Overlay::None,
        );
        log.observe::<std::io::Error>(
            &Ok(Event::Key(key(KeyCode::Char('q'), KeyModifiers::NONE))),
            true,
            &ui::Overlay::None,
        );
        log.observe::<std::io::Error>(
            &Ok(Event::Key(key(KeyCode::Char('q'), KeyModifiers::NONE))),
            true,
            &ui::Overlay::Mail(ui::MailView::default()),
        );
        let text = std::fs::read_to_string(&path).expect("read back");
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 3, "one line per event: {text}");
        assert!(lines[0].contains("armed_before=false"), "{}", lines[0]);
        assert!(lines[0].contains("Pending"), "{}", lines[0]);
        assert!(
            lines[1].contains("armed_before=true") && lines[1].contains("Quit"),
            "{}",
            lines[1]
        );
        assert!(lines[2].contains("overlay=mail"), "{}", lines[2]);
        assert!(
            lines.iter().all(|l| l.contains("overlay=")),
            "the overlay is recorded on EVERY key line, `none` included: {text}"
        );
        assert!(
            lines.iter().all(|l| l.contains("ms ")),
            "every line is timestamped: {text}"
        );
    }

    /// Hypothesis (b): a `prefix_armed` that is stored and then lost before the
    /// next keystroke. The instrumentation has to make that shape readable --
    /// `DISPATCH` records what the loop actually stored, and `TICK` reports a
    /// later change to it *with no event in between*, which is the whole
    /// signature.
    #[test]
    fn the_keylog_makes_a_lost_prefix_arming_visible_between_ticks() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let path = tmp.path().join("keys.log");
        let mut log = KeyLog {
            file: std::fs::File::create(&path).expect("create"),
            start: Instant::now(),
            tick: 0,
            last: None,
        };

        let live = |armed: bool| LoopState {
            prefix_armed: armed,
            overlay: "none",
            panes: 1,
            focused: 0,
            focused_alt: false,
        };

        // Tick 1: nothing armed. Tick 2: identical, so it writes nothing.
        log.tick(live(false), Duration::ZERO);
        log.tick(live(false), Duration::ZERO);
        // The operator presses Ctrl+A and the loop stores the arming.
        log.dispatch(false, true, &InputVerdict::Pending);
        log.tick(live(true), Duration::ZERO);
        // ... and then it is gone, with no keystroke to explain it.
        log.tick(live(false), Duration::ZERO);

        let text = std::fs::read_to_string(&path).expect("read back");
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(
            lines.len(),
            4,
            "an unchanged tick writes nothing; at a 50ms poll silence is the point: {text}"
        );
        assert!(lines[0].contains("TICK armed=false"), "{}", lines[0]);
        assert!(
            lines[1].contains("DISPATCH armed false->true"),
            "the arming the loop actually stored: {}",
            lines[1]
        );
        assert!(
            lines[2].contains("armed=false->true"),
            "the tick that observed it: {}",
            lines[2]
        );
        assert!(
            lines[3].contains("armed=true->false"),
            "and the tick that lost it again, with no EVENT between: {}",
            lines[3]
        );
        // Tick numbers make "which iteration" answerable rather than inferred.
        assert!(lines[3].contains("t4"), "{}", lines[3]);
    }

    /// Issue #330: keystroke latency IS the tick duration -- the loop reaches
    /// `event::poll` only after a tick's maintenance and draw -- so every
    /// `TICK` line carries the previous iteration's wall time, and a tick slow
    /// enough for the operator to feel writes a line even when nothing else
    /// about the loop state moved.
    #[test]
    fn the_keylog_tick_line_carries_the_previous_tick_duration() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let path = tmp.path().join("keys.log");
        let mut log = KeyLog {
            file: std::fs::File::create(&path).expect("create"),
            start: Instant::now(),
            tick: 0,
            last: None,
        };
        let state = LoopState {
            prefix_armed: false,
            overlay: "none",
            panes: 3,
            focused: 0,
            focused_alt: false,
        };

        log.tick(state, Duration::from_millis(7));
        log.tick(state, Duration::from_millis(3));
        log.tick(state, KEYLOG_SLOW_TICK);

        let text = std::fs::read_to_string(&path).expect("read back");
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(
            lines.len(),
            2,
            "an unchanged, fast tick still writes nothing: {text}"
        );
        assert!(
            lines[0].contains("dur=7ms"),
            "the first line carries the duration too: {}",
            lines[0]
        );
        assert!(
            lines[1].contains(&format!("dur={}ms", KEYLOG_SLOW_TICK.as_millis()))
                && lines[1].contains("(slow)"),
            "a slow tick is reported even with the loop state unchanged: {}",
            lines[1]
        );
    }

    /// Hypothesis (c): the action fires but nothing visible follows. Every
    /// `DashAction` is logged, and each take/assign of the overlay slot is
    /// recorded -- so an overlay opened and immediately closed again reads
    /// differently from one that stayed open swallowing keys.
    #[test]
    fn the_keylog_records_every_action_and_overlay_swap() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let path = tmp.path().join("keys.log");
        let mut log = KeyLog {
            file: std::fs::File::create(&path).expect("create"),
            start: Instant::now(),
            tick: 0,
            last: None,
        };

        log.dispatch(true, false, &InputVerdict::Dash(DashAction::Mail));
        log.overlay_swap("none", &ui::Overlay::Mail(ui::MailView::default()));
        // The reducer closed it again on the very next key.
        log.overlay_swap("mail", &ui::Overlay::None);
        // A forwarded keystroke logs its length, never the operator's bytes.
        log.dispatch(false, false, &InputVerdict::ToChild(b"secret".to_vec()));

        let text = std::fs::read_to_string(&path).expect("read back");
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 4, "{text}");
        assert!(lines[0].contains("verdict=Dash(Mail)"), "{}", lines[0]);
        assert!(
            lines[1].contains("OVERLAY took=none now=mail"),
            "{}",
            lines[1]
        );
        assert!(
            lines[2].contains("OVERLAY took=mail now=none"),
            "{}",
            lines[2]
        );
        assert!(
            lines[3].contains("ToChild(6 bytes)"),
            "a forwarded keystroke logs its length: {}",
            lines[3]
        );
        assert!(
            !text.contains("secret"),
            "what the operator typed never lands in the log: {text}"
        );
    }

    /// One `ZIRV_CTX_DASH_KEYLOG` capture has to settle "why did nothing
    /// scroll" without another guess: the alternate-screen flag, the offset
    /// either side, and the branch taken.
    #[test]
    fn the_keylog_records_which_branch_each_scroll_took() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let path = tmp.path().join("keys.log");
        let mut log = KeyLog {
            file: std::fs::File::create(&path).expect("create"),
            start: Instant::now(),
            tick: 0,
            last: None,
        };

        log.scroll("wheel", false, false, 0, 3, ScrollOutcome::Scrolled(3));
        log.scroll("wheel", true, true, 0, 0, ScrollOutcome::ForwardedMouse);
        log.scroll("top", true, false, 0, 0, ScrollOutcome::FullScreen);
        // And the per-tick state line carries the flag even with no scroll.
        log.tick(
            LoopState {
                prefix_armed: false,
                overlay: "none",
                panes: 1,
                focused: 0,
                focused_alt: true,
            },
            Duration::ZERO,
        );

        let text = std::fs::read_to_string(&path).expect("read back");
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 4, "{text}");
        assert!(
            lines[0].contains(
                "SCROLL wheel alt_screen=false mouse=false scrollback 0->3 branch=scrollback"
            ),
            "{}",
            lines[0]
        );
        assert!(
            lines[1].contains("alt_screen=true")
                && lines[1].contains("mouse=true")
                && lines[1].contains("scrollback 0->0")
                && lines[1].contains("branch=forwarded-mouse"),
            "the whole diagnosis on one line: {}",
            lines[1]
        );
        assert!(lines[2].contains("outcome=FullScreen"), "{}", lines[2]);
        assert!(
            lines[3].contains("alt_screen=true"),
            "the tick line carries the focused pane's mode too: {}",
            lines[3]
        );
    }

    // F7: focus (the pane on screen and under the keyboard) versus selection
    // (the sidebar cursor, which may sit on a view-only session).

    #[test]
    fn digits_and_tab_move_both_the_selection_and_the_focus() {
        // Three panes, five combined rows (two view-only sessions).
        assert_eq!(apply_navigation(DashAction::Switch(2), 0, 0, 3, 5), (2, 2));
        assert_eq!(apply_navigation(DashAction::NextPane, 4, 2, 3, 5), (0, 0));
        assert_eq!(apply_navigation(DashAction::NextPane, 0, 0, 3, 5), (1, 1));
    }

    /// N2: `Ctrl+A 9` on a three-pane dashboard used to clamp to the last
    /// pane, which moved the keyboard somewhere the operator never asked for
    /// -- a mistyped digit is far more likely than a request for "whatever is
    /// last". An out-of-range digit now changes nothing at all.
    #[test]
    fn a_digit_beyond_the_pane_count_is_a_noop() {
        assert_eq!(
            apply_navigation(DashAction::Switch(8), 1, 1, 3, 5),
            (1, 1),
            "an out-of-range digit leaves both indices exactly where they were"
        );
        assert_eq!(apply_navigation(DashAction::Switch(3), 0, 0, 3, 5), (0, 0));
        // The last addressable pane is still addressable.
        assert_eq!(apply_navigation(DashAction::Switch(2), 0, 0, 3, 5), (2, 2));
    }

    /// The reported bug: `Ctrl+A Up`/`Down` highlighted the other session but
    /// could not switch to it, so `Ctrl+A Tab` was the only way to change
    /// panes. An arrow that lands on a pane row now moves the keyboard there
    /// too -- the F7 split stays, it just no longer strands the arrows.
    #[test]
    fn arrow_navigation_switches_panes_when_it_lands_on_one() {
        // Three panes, five combined rows (two view-only sessions).
        assert_eq!(
            apply_navigation(DashAction::SelectDown, 0, 0, 3, 5),
            (1, 1),
            "down onto pane 1 moves the keyboard onto pane 1"
        );
        assert_eq!(apply_navigation(DashAction::SelectDown, 1, 1, 3, 5), (2, 2));
        assert_eq!(
            apply_navigation(DashAction::SelectUp, 2, 2, 3, 5),
            (1, 1),
            "and back up again"
        );
        // Onto the first view-only row: the cursor moves, the keyboard does
        // not -- that session is not attached to this dashboard.
        assert_eq!(
            apply_navigation(DashAction::SelectDown, 2, 2, 3, 5),
            (3, 2),
            "a view-only row cannot take the keyboard"
        );
        assert_eq!(apply_navigation(DashAction::SelectDown, 3, 2, 3, 5), (4, 2));
        // Walking back out of the view-only rows re-focuses the pane the
        // cursor lands on, which is the whole point of the fix.
        assert_eq!(apply_navigation(DashAction::SelectUp, 4, 2, 3, 5), (3, 2));
        assert_eq!(
            apply_navigation(DashAction::SelectUp, 3, 2, 3, 5),
            (2, 2),
            "back onto a pane row, so focus follows again"
        );
        // Focus follows even when it was somewhere else entirely.
        assert_eq!(apply_navigation(DashAction::SelectUp, 1, 2, 3, 5), (0, 0));
    }

    #[test]
    fn focus_stays_on_a_pane_when_selection_walks_into_view_only_rows() {
        // One pane, three combined rows: rows 1 and 2 are view-only
        // sessions this dashboard does not own.
        let (mut selected, mut focused) = (0usize, 0usize);
        for _ in 0..5 {
            (selected, focused) = apply_navigation(DashAction::SelectDown, selected, focused, 1, 3);
        }
        assert_eq!(selected, 2, "the sidebar cursor reaches the last row");
        assert_eq!(
            focused, 0,
            "but the focused pane -- the one being drawn and typed into -- never moves"
        );

        // And walking back up leaves focus alone too.
        (selected, focused) = apply_navigation(DashAction::SelectUp, selected, focused, 1, 3);
        assert_eq!((selected, focused), (1, 0));
    }

    #[test]
    fn navigation_on_an_empty_dashboard_moves_nothing() {
        assert_eq!(apply_navigation(DashAction::Switch(3), 0, 0, 0, 0), (0, 0));
        assert_eq!(apply_navigation(DashAction::NextPane, 0, 0, 0, 0), (0, 0));
        assert_eq!(apply_navigation(DashAction::SelectDown, 0, 0, 0, 0), (0, 0));
        assert_eq!(apply_navigation(DashAction::SelectUp, 0, 0, 0, 0), (0, 0));
    }

    /// `enrich_sidebar` folds the cached status onto the row: the glyph's own
    /// source, the fact block's own state word (dash refresh PR1 -- the
    /// composed projection's word, replacing the old `reason` disclosure
    /// line) and the `since` disclosure line's `<lifecycle word> <age since
    /// last_transition> · started <age>` shape (kept for the `^A i`
    /// inspector).
    #[test]
    fn enrich_sidebar_folds_the_cached_attention_status_onto_the_row() {
        let panes = vec![pane_row("aaa11111", "claude")];
        let mut record = registry_record("aaa11111", "claude", Some(DASHBOARD_PID));
        record.started_at = 100;
        let registry = vec![(record, sessions::Liveness::Live)];
        let mut rows =
            assemble_sidebar(&panes, &registry, &HashMap::new(), 0, 0, DASHBOARD_PID, 740);
        let mut disk = DiskFacts::default();
        disk.attention.insert("aaa11111".into(), blocked_status(3));
        enrich_sidebar(&mut rows, &disk, 740);

        assert_eq!(ui::glyph_for(&rows[0]), ui::Glyph::NeedsAction);
        assert_eq!(rows[0].fact_state, "approval");
        assert_eq!(rows[0].fact_since_secs, Some(740 - 200));
        let value = |key: &str| {
            rows[0]
                .disclosure
                .iter()
                .find(|(k, _)| k == key)
                .map(|(_, v)| v.clone())
                .unwrap()
        };
        // 740 - 200 = 9m in state; 740 - 100 = 10m since it started.
        assert_eq!(value("since"), "waiting 9m \u{b7} started 10m ago");
    }

    /// With nothing cached, every disclosure line and the glyph stay exactly
    /// what phase 1 produced -- a dashboard with no issue #349 writers sees no
    /// change at all.
    #[test]
    fn enrich_sidebar_leaves_a_row_with_no_status_exactly_as_phase_one_drew_it() {
        let panes = vec![pane_row("aaa11111", "claude")];
        let mut rows = assemble_sidebar(&panes, &[], &HashMap::new(), 0, 0, DASHBOARD_PID, 740);
        let before = rows[0].disclosure.clone();
        enrich_sidebar(&mut rows, &DiskFacts::default(), 740);
        assert_eq!(rows[0].disclosure, before);
        assert!(rows[0].status.is_none());
        assert_eq!(ui::glyph_for(&rows[0]), ui::Glyph::Idle);
    }

    /// A retained ended row is selectable but never focusable, keeps its role
    /// and model, freezes its age, and says how it ended.
    #[test]
    fn a_retained_ended_row_is_selectable_but_never_focusable() {
        let panes = vec![
            pane_row("aaa11111", "claude"),
            ended_pane_row("bbb22222", 2, 600),
        ];
        let rows = assemble_sidebar(&panes, &[], &HashMap::new(), 1, 0, DASHBOARD_PID, 900);
        assert_eq!(rows.len(), 2);
        assert!(rows[1].selected, "the cursor may sit on it");
        assert!(!rows[1].attached, "but the keyboard can never follow");
        assert!(!rows[1].focused);
        assert_eq!(rows[1].role, "worker", "role is retained");
        assert_eq!(rows[1].age_secs, Some(300), "age is frozen at the exit");
        assert_eq!(rows[1].exit_code, Some(2));
        assert_eq!(ui::glyph_for(&rows[1]), ui::Glyph::Failed);
        let since = rows[1]
            .disclosure
            .iter()
            .find(|(k, _)| k == "since")
            .map(|(_, v)| v.clone())
            .unwrap();
        assert_eq!(since, "exited 5m \u{b7} exit 2");

        // Clicking it selects only -- `focused` never moves off the live pane.
        let (selected, focused) = select_row("bbb22222", &rows, 0, 0);
        assert_eq!((selected, focused), (1, 0));
    }

    #[test]
    fn ctrl_a_left_and_right_are_bound_to_the_fold_actions() {
        assert_eq!(
            filter_key(true, KeyEvent::new(KeyCode::Left, KeyModifiers::NONE)).1,
            InputVerdict::Dash(DashAction::CollapseGroup)
        );
        assert_eq!(
            filter_key(true, KeyEvent::new(KeyCode::Right, KeyModifiers::NONE)).1,
            InputVerdict::Dash(DashAction::ExpandGroup)
        );
        // Issue #354 phase 3: `^A i` is the real inspector, `^A r` restores
        // an ended row, and `^A e` still opens the kept-errors overlay.
        assert_eq!(
            filter_key(true, KeyEvent::new(KeyCode::Char('i'), KeyModifiers::NONE)).1,
            InputVerdict::Dash(DashAction::Inspect)
        );
        assert_eq!(
            filter_key(true, KeyEvent::new(KeyCode::Char('r'), KeyModifiers::NONE)).1,
            InputVerdict::Dash(DashAction::RestoreRow)
        );
        assert_eq!(
            filter_key(true, KeyEvent::new(KeyCode::Char('e'), KeyModifiers::NONE)).1,
            InputVerdict::Dash(DashAction::ShowErrors)
        );
    }

    /// A1-6: `frame_snapshot.roster` is empty until the first successful
    /// draw, so the very first `Ctrl+A ↓` used to park on the summary line
    /// instead of moving. With no tree to walk, navigation falls back to the
    /// flat row list.
    #[test]
    fn the_first_select_down_moves_before_any_frame_has_been_drawn() {
        let panes = vec![
            pane_row("worker01", "claude"),
            pane_row("worker02", "claude"),
            pane_row("lead0001", "codex"),
        ];
        let rows = assemble_sidebar(&panes, &[], &HashMap::new(), 0, 0, DASHBOARD_PID, 0);
        let mut chrome = None;
        assert_eq!(
            navigate_roster(DashAction::SelectDown, &rows, &[], 0, 0, &mut chrome),
            (1, 1),
            "an empty roster order must not swallow the first move"
        );
        assert_eq!(chrome, None, "and must not park the cursor on the summary");
    }

    /// A1-7: chrome owns the cursor or nothing does. With the cursor on the
    /// summary line or a group header there is no session row under it, so a
    /// session-scoped action must not fall back to the stale `selected`
    /// index.
    #[test]
    fn a_session_action_never_targets_a_row_the_cursor_left_for_chrome() {
        let panes = vec![
            pane_row("worker01", "claude"),
            pane_row("lead0001", "codex"),
        ];
        let rows = assemble_sidebar(&panes, &[], &HashMap::new(), 1, 1, DASHBOARD_PID, 0);
        assert_eq!(
            session_target(None, &rows, 1).as_deref(),
            Some("lead0001"),
            "with no chrome the selected row is the target"
        );
        assert_eq!(
            session_target(Some(&Hit::SidebarSummary), &rows, 1),
            None,
            "the summary line is not a session"
        );
        assert_eq!(
            session_target(Some(&Hit::GroupToggle("wg".to_string())), &rows, 1),
            None,
            "neither is a group header"
        );
    }

    /// M7: a modified special key carries its modifiers through the standard
    /// xterm `CSI 1 ; <mod> <final>` / `CSI <n> ; <mod> ~` forms, so `Ctrl+Left`
    /// is a word-left rather than a bare one-character move.
    #[test]
    fn encode_key_carries_modifiers_on_special_keys() {
        assert_eq!(
            encode_key(key(KeyCode::Left, KeyModifiers::CONTROL)),
            b"\x1b[1;5D"
        );
        assert_eq!(
            encode_key(key(KeyCode::Left, KeyModifiers::NONE)),
            b"\x1b[D",
            "an unmodified arrow is still the bare CSI form"
        );
        assert_eq!(
            encode_key(key(KeyCode::PageUp, KeyModifiers::SHIFT)),
            b"\x1b[5;2~"
        );
        assert_eq!(
            encode_key(key(KeyCode::Home, KeyModifiers::ALT)),
            b"\x1b[1;3H"
        );
    }

    /// M8: control combinations crossterm delivers as a plain char map to their
    /// real C0 bytes instead of typing a literal `4`/`7`/space.
    #[test]
    fn encode_key_maps_non_alphabetic_control_combinations() {
        assert_eq!(
            encode_key(key(KeyCode::Char(' '), KeyModifiers::CONTROL)),
            vec![0x00],
            "Ctrl+Space is NUL"
        );
        assert_eq!(
            encode_key(key(KeyCode::Char('7'), KeyModifiers::CONTROL)),
            vec![0x1f],
            "Ctrl+_ delivered as the legacy Char('7') alias is 0x1f"
        );
        assert_eq!(
            encode_key(key(KeyCode::Char('4'), KeyModifiers::CONTROL)),
            vec![0x1c]
        );
        // Under the kitty keyboard protocol (negotiated by
        // `push_keyboard_enhancement`) these same keys arrive with their
        // literal character instead of the legacy '4'..'7' aliases above --
        // a bare '_' used to be passed through unchanged here, typing a
        // literal underscore instead of Ctrl+_.
        assert_eq!(
            encode_key(key(KeyCode::Char('\\'), KeyModifiers::CONTROL)),
            vec![0x1c],
            "Ctrl+\\ delivered literally under kitty"
        );
        assert_eq!(
            encode_key(key(KeyCode::Char(']'), KeyModifiers::CONTROL)),
            vec![0x1d],
            "Ctrl+] delivered literally under kitty"
        );
        assert_eq!(
            encode_key(key(KeyCode::Char('^'), KeyModifiers::CONTROL)),
            vec![0x1e],
            "Ctrl+^ delivered literally under kitty"
        );
        assert_eq!(
            encode_key(key(KeyCode::Char('_'), KeyModifiers::CONTROL)),
            vec![0x1f],
            "Ctrl+_ delivered literally under kitty"
        );
        assert_eq!(
            encode_key(key(KeyCode::Char('/'), KeyModifiers::CONTROL)),
            vec![0x1f],
            "Ctrl+/ delivered literally under kitty shares Ctrl+_'s C0 byte"
        );
        assert_eq!(
            encode_key(key(KeyCode::Char('@'), KeyModifiers::CONTROL)),
            vec![0x00],
            "Ctrl+@ delivered literally under kitty is NUL"
        );
        // The alphabetic branch is untouched.
        assert_eq!(
            encode_key(key(KeyCode::Char('c'), KeyModifiers::CONTROL)),
            vec![0x03]
        );
    }

    /// M7: bare Enter still submits (`\r`); Shift+Enter sends `ESC CR`, which
    /// does not. `ESC CR` rather than the CSI-u form on purpose -- CSI-u is
    /// only legal once the child has negotiated the kitty keyboard protocol,
    /// so an un-negotiated harness would type a literal `[13;2u`, while
    /// `ESC CR` is the Meta+Enter convention it already reads as a newline.
    #[test]
    fn plain_enter_submits_but_shift_enter_does_not() {
        assert_eq!(encode_key(key(KeyCode::Enter, KeyModifiers::NONE)), b"\r");
        let shift_enter = encode_key(key(KeyCode::Enter, KeyModifiers::SHIFT));
        assert_ne!(shift_enter, b"\r", "Shift+Enter must not submit");
        assert_eq!(shift_enter, b"\x1b\r");
        // Never the CSI-u form: it would be typed literally by any harness
        // that has not enabled the protocol.
        assert!(!shift_enter.starts_with(b"\x1b["));
    }

    /// A real-CLI probe under ConPTY established that Windows Terminal, once
    /// an operator has run claude's own `/terminal-setup`, rewrites Shift+
    /// Enter into `ESC CR` -- and zirv's own console layer folds that into a
    /// single Enter keydown carrying ALT rather than SHIFT. Before this fix
    /// `encode_key` only checked SHIFT on `KeyCode::Enter`, so that keydown
    /// fell through to the bare-`\r` branch and silently submitted instead of
    /// inserting a newline. Ctrl+Enter (no SHIFT, no ALT) is unaffected and
    /// still submits.
    #[test]
    fn alt_enter_is_treated_the_same_as_shift_enter() {
        let alt_enter = encode_key(key(KeyCode::Enter, KeyModifiers::ALT));
        assert_eq!(
            alt_enter, b"\x1b\r",
            "ALT alone on Enter must degrade to newline, not submit"
        );
        let shift_alt_enter =
            encode_key(key(KeyCode::Enter, KeyModifiers::SHIFT | KeyModifiers::ALT));
        assert_eq!(shift_alt_enter, b"\x1b\r");
        // Ctrl+Enter carries neither SHIFT nor ALT, so it is untouched by
        // this fix and still submits.
        assert_eq!(
            encode_key(key(KeyCode::Enter, KeyModifiers::CONTROL)),
            b"\r"
        );
    }

    #[test]
    fn chrome_mouse_dispatch_never_reaches_child_forward_or_scroll() {
        let snap = hit::FrameSnapshot {
            frame: Rect::new(0, 0, 120, 40),
            sidebar: Rect::new(0, 2, 44, 36),
            grid: Rect::new(45, 2, 75, 36),
            divider: Rect::new(44, 2, 1, 36),
            rows: vec![
                (Rect::new(0, 2, 44, 1), Hit::SidebarSummary),
                (Rect::new(0, 3, 44, 9), Hit::SidebarRow("worker".into())),
            ],
            ..Default::default()
        };
        let mut child_calls = Vec::new();
        for (x, y) in [(0, 0), (5, 2), (5, 3), (5, 11), (44, 20), (60, 39)] {
            for kind in [
                MouseEventKind::ScrollUp,
                MouseEventKind::ScrollDown,
                MouseEventKind::Down(MouseButton::Left),
                MouseEventKind::Up(MouseButton::Right),
            ] {
                let mouse = event::MouseEvent {
                    kind,
                    column: x,
                    row: y,
                    modifiers: KeyModifiers::NONE,
                };
                if route_mouse(&snap, mouse, true, false, false) == MouseRoute::Grid {
                    child_calls.push(((x, y), kind));
                }
            }
        }
        assert!(
            child_calls.is_empty(),
            "chrome dispatched to child: {child_calls:?}"
        );
        let click = event::MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 5,
            row: 10,
            modifiers: KeyModifiers::NONE,
        };
        assert_eq!(
            route_mouse(&snap, click, true, false, false),
            MouseRoute::Select("worker".into())
        );
        assert_eq!(
            route_mouse(
                &snap,
                event::MouseEvent {
                    kind: MouseEventKind::Down(MouseButton::Right),
                    ..click
                },
                true,
                false,
                false
            ),
            MouseRoute::Action(DashAction::ContextMenu("worker".into()))
        );
        assert_eq!(
            route_mouse(
                &snap,
                event::MouseEvent {
                    kind: MouseEventKind::ScrollDown,
                    ..click
                },
                true,
                false,
                false
            ),
            MouseRoute::ScrollRoster(1)
        );
    }

    /// Click affordance follow-up: a left click on the JEV errors line opens
    /// the dialog; `Hit::JevErrors` is only ever in a frame's own `rows` at
    /// all when the section drew `errors > 0` (`ui::jev_errors_hit_rect`),
    /// so the zero-errors "no-op" case is that the hit never reaches
    /// `route_mouse` in the first place -- a click at that same screen
    /// position instead falls through to whatever chrome (or nothing) is
    /// really there, which this covers by asserting the same click is
    /// `MouseRoute::Consume` once the row is gone.
    #[test]
    fn a_left_click_on_the_jev_errors_line_opens_its_dialog() {
        let jev_errors_rect = Rect::new(0, 20, 44, 1);
        let snap = hit::FrameSnapshot {
            frame: Rect::new(0, 0, 120, 40),
            sidebar: Rect::new(0, 2, 44, 36),
            grid: Rect::new(45, 2, 75, 36),
            divider: Rect::new(44, 2, 1, 36),
            rows: vec![(jev_errors_rect, Hit::JevErrors)],
            ..Default::default()
        };
        let click = event::MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: jev_errors_rect.x,
            row: jev_errors_rect.y,
            modifiers: KeyModifiers::NONE,
        };
        assert_eq!(
            route_mouse(&snap, click, true, false, false),
            MouseRoute::Action(DashAction::ShowJevErrors)
        );

        // Zero errors: `frame_snapshot` never adds the row at all (see
        // `ui::jev_errors_hit_rect`), so the same click lands on nothing
        // rather than on a hit that does nothing.
        let no_errors_snap = hit::FrameSnapshot {
            rows: Vec::new(),
            ..snap
        };
        assert_eq!(
            route_mouse(&no_errors_snap, click, true, false, false),
            MouseRoute::Consume,
            "no hit region means the click is a no-op"
        );
    }

    // -- issue #354 phase 3: overlay pointer dispatch -----------------------

    /// A frame with a scrolled list dialog over it: three visible rows
    /// (entries 30-32), a pinned hint row with `⏎`/`esc`, and a roster
    /// underneath that must stay unreachable.
    fn dialog_frame() -> hit::FrameSnapshot {
        hit::FrameSnapshot {
            frame: Rect::new(0, 0, 120, 40),
            sidebar: Rect::new(0, 2, 44, 36),
            grid: Rect::new(45, 2, 75, 36),
            divider: Rect::new(44, 2, 1, 36),
            rows: vec![
                (Rect::new(0, 2, 44, 1), Hit::SidebarSummary),
                (Rect::new(0, 3, 44, 9), Hit::SidebarRow("worker".into())),
            ],
            header_hints: vec![(Rect::new(100, 0, 20, 1), HintId::Actions)],
            overlay: Some(Rect::new(50, 10, 60, 20)),
            overlay_rows: vec![
                (Rect::new(52, 11, 56, 1), 30),
                (Rect::new(52, 12, 56, 1), 31),
                (Rect::new(52, 13, 56, 1), 32),
            ],
            overlay_hints: vec![
                (Rect::new(52, 28, 6, 1), HintId::DialogKey(KeyCode::Enter)),
                (Rect::new(61, 28, 8, 1), HintId::DialogKey(KeyCode::Esc)),
            ],
            overlay_capacity: 3,
            ..Default::default()
        }
    }

    /// A click on one of the dialog's visible rows names that row's index
    /// into the FULL list (so a scrolled dialog addresses the right entry),
    /// a click on a hint is fed back as exactly that key, the wheel inside
    /// scrolls the list, and the backdrop is consumed without closing
    /// anything. Nothing underneath is reachable at any point.
    #[test]
    fn an_open_dialog_owns_the_pointer_and_addresses_its_own_rows_and_hints() {
        let snap = dialog_frame();
        assert_eq!(
            route_mouse(
                &snap,
                at(MouseEventKind::Down(MouseButton::Left), 60, 12),
                true,
                true,
                false
            ),
            MouseRoute::OverlayRow(31)
        );
        assert_eq!(
            route_mouse(
                &snap,
                at(MouseEventKind::Down(MouseButton::Left), 53, 28),
                true,
                true,
                false
            ),
            MouseRoute::OverlayKey(KeyCode::Enter)
        );
        assert_eq!(
            route_mouse(
                &snap,
                at(MouseEventKind::Down(MouseButton::Left), 63, 28),
                true,
                true,
                false
            ),
            MouseRoute::OverlayKey(KeyCode::Esc)
        );
        assert_eq!(
            route_mouse(
                &snap,
                at(MouseEventKind::ScrollDown, 60, 15),
                true,
                true,
                false
            ),
            MouseRoute::ScrollOverlay(1)
        );
        assert_eq!(
            route_mouse(
                &snap,
                at(MouseEventKind::ScrollUp, 60, 15),
                true,
                true,
                false
            ),
            MouseRoute::ScrollOverlay(-1)
        );
        // The backdrop -- over the sidebar, over the grid, over the header's
        // own `actions` hint -- is consumed, never a click on what is under
        // it and never a dismissal of the dialog.
        for (x, y) in [(5u16, 3u16), (80, 35), (105, 0), (44, 20)] {
            for kind in [
                MouseEventKind::Down(MouseButton::Left),
                MouseEventKind::Down(MouseButton::Right),
                MouseEventKind::ScrollUp,
                MouseEventKind::ScrollDown,
                MouseEventKind::Up(MouseButton::Left),
            ] {
                assert_eq!(
                    route_mouse(&snap, at(kind, x, y), true, true, false),
                    MouseRoute::Consume,
                    "({x}, {y}) {kind:?} escaped the modal"
                );
            }
        }
        // And a drag that belongs to an in-progress selection does not get to
        // fall through to the grid while a dialog is up either.
        assert_eq!(
            route_mouse(
                &snap,
                at(MouseEventKind::Drag(MouseButton::Left), 80, 30),
                true,
                true,
                true
            ),
            MouseRoute::Consume
        );
    }

    /// Review of #354 (defect 1, HIGH): several queued events are drained
    /// against ONE `frame_snapshot` (`HIGH-2`, above the drain loop). If an
    /// earlier event in that drain closes the overlay the snapshot was drawn
    /// with -- an `Esc` that closes the palette -- a later queued click that
    /// still lands on the snapshot's own pinned hint (here, the dialog's
    /// `Enter` hint, standing in for a palette's "run" hint) must be
    /// consumed, never synthesized into the key it names: with the overlay
    /// closed, that key would otherwise reach the child pane.
    ///
    /// `route_mouse` alone cannot see this -- it only knows the frozen
    /// geometry -- so `overlay_route_is_current` is the guard that actually
    /// stops it, by comparing the LIVE overlay's identity against the one the
    /// snapshot was drawn from.
    #[test]
    fn a_queued_click_on_a_closed_overlays_hint_is_consumed_not_synthesized() {
        let snap = dialog_frame();
        // The frame was drawn with a palette (mode `Run`) open -- this is
        // `frame_snapshot_overlay_ident` at the top of the drain.
        let snapshot_ident =
            overlay_identity(&ui::Overlay::Palette(open_palette(ui::PaletteMode::Run)));
        // First event in the drain: `Esc` closed the palette. The LIVE
        // overlay, from this point on, is `None`.
        let live_overlay = ui::Overlay::None;
        // Second event in the same drain: a click at the snapshot's own
        // coordinates for the pinned `Enter` hint.
        let route = route_mouse(
            &snap,
            at(MouseEventKind::Down(MouseButton::Left), 53, 28),
            true,
            !matches!(live_overlay, ui::Overlay::None),
            false,
        );
        // `route_mouse` still names the hint: it only has the stale
        // snapshot, and has no way to know the dialog it names already
        // closed.
        assert_eq!(route, MouseRoute::OverlayKey(KeyCode::Enter));
        // The identity guard is what actually stops it: the live overlay no
        // longer matches what the snapshot was drawn against, so the event
        // loop must downgrade this to `Consume` -- never synthesizing
        // `Enter`, never reaching `filter_key`, never reaching the child.
        assert!(
            !overlay_route_is_current(&route, &live_overlay, &snapshot_ident),
            "a stale overlay hit must not be trusted once the overlay it named has closed"
        );
        // The same dialog, still open, is still trusted.
        let still_open = ui::Overlay::Palette(open_palette(ui::PaletteMode::Run));
        assert!(overlay_route_is_current(
            &route,
            &still_open,
            &snapshot_ident
        ));
        // A DIFFERENT dialog opening at the very same coordinates within the
        // same drain is stale too -- it is the identity being checked, not
        // merely "some overlay is open".
        let different_dialog = ui::Overlay::Errors(ui::ErrorsView::default());
        assert!(!overlay_route_is_current(
            &route,
            &different_dialog,
            &snapshot_ident
        ));
        // Non-overlay routes never depend on the overlay's identity at all --
        // there is nothing here for them to go stale against.
        assert!(overlay_route_is_current(
            &MouseRoute::Grid,
            &live_overlay,
            &snapshot_ident
        ));
    }

    #[test]
    fn overlay_swallows_wheel_and_grid_keeps_both_mouse_modes() {
        let snap = hit::FrameSnapshot {
            frame: Rect::new(0, 0, 80, 20),
            grid: Rect::new(45, 2, 35, 16),
            ..Default::default()
        };
        let mut calls = Vec::new();
        for kind in [
            MouseEventKind::ScrollUp,
            MouseEventKind::Down(MouseButton::Left),
            MouseEventKind::Up(MouseButton::Left),
        ] {
            let mouse = event::MouseEvent {
                kind,
                column: 50,
                row: 5,
                modifiers: KeyModifiers::NONE,
            };
            assert_eq!(
                route_mouse(&snap, mouse, true, true, true),
                MouseRoute::Consume
            );
            assert_eq!(
                route_mouse(&snap, mouse, false, false, true),
                MouseRoute::Consume
            );
            if route_mouse(&snap, mouse, true, false, false) == MouseRoute::Grid {
                calls.push(kind);
            }
        }
        assert_eq!(calls.len(), 3);
    }

    /// Issue #697: a mouse-owning pane no longer suppresses zirv's own
    /// selection at all -- the dashboard owns click-drag inside every pane
    /// now, including one whose child wants mouse reporting, so
    /// `press_starts_selection` dropped the `wants_mouse` gate this test
    /// used to pin (and the once-per-session capture-hint notice it also
    /// covered, `drag_needs_capture_hint`, is gone with it: there is nothing
    /// left to explain once a mouse-owning pane is no longer a dead end).
    /// What still tells a click for such a pane's child apart from a drag
    /// meant for zirv's own selection is the separate click-vs-drag
    /// threshold (`past_drag_threshold`), pinned by its own test below.
    #[test]
    fn a_press_inside_the_grid_is_eligible_regardless_of_whether_the_child_wants_mouse() {
        let main = Rect::new(45, 2, 35, 16);
        assert!(
            press_starts_selection(main, 50, 5),
            "a press inside the grid over a plain pane is eligible"
        );
        assert!(
            !press_starts_selection(main, 5, 5),
            "a press outside the grid never starts one"
        );
    }

    #[test]
    fn grouped_keyboard_and_pointer_selection_use_identity_and_leave_external_focus() {
        let panes = vec![
            pane_row("worker01", "claude"),
            pane_row("lead0001", "codex"),
        ];
        let mut rows = assemble_sidebar(&panes, &[], &HashMap::new(), 0, 0, DASHBOARD_PID, 0);
        let mut external = rows[0].clone();
        external.short = "external".into();
        external.attached = false;
        external.focused = false;
        rows.push(external);
        let order = vec![
            Hit::GroupToggle("g".into()),
            Hit::SidebarRow("lead0001".into()),
            Hit::SidebarRow("worker01".into()),
            Hit::SidebarRow("external".into()),
        ];
        let mut chrome = Some(Hit::GroupToggle("g".into()));
        assert_eq!(
            navigate_roster(DashAction::SelectDown, &rows, &order, 0, 0, &mut chrome),
            select_row("lead0001", &rows, 0, 0)
        );
        assert_eq!(chrome, None);
        assert_eq!(select_row("external", &rows, 1, 1), (2, 1));
        assert_eq!(select_row("reaped", &rows, 1, 1), (1, 1));
        rows[0].state = ui::RowState::Dead;
        assert_eq!(select_row("worker01", &rows, 1, 1), (0, 1));
    }

    #[test]
    fn sidebar_disclosure_uses_cached_scope_usage_and_state_time() {
        let mut pane = pane_row("lead0001", "codex");
        pane.group_id = Some("group".into());
        pane.role = "sub-orchestrator".into();
        pane.model = Some("resolved-model".into());
        pane.budget = "12000 / 80000".into();
        let mut rows = assemble_sidebar(&[pane], &[], &HashMap::new(), 0, 0, DASHBOARD_PID, 0);
        let mut disk = DiskFacts::default();
        disk.state_since
            .insert("lead0001".into(), (ui::RowState::Working, 60));
        disk.usage.push(ui::HarnessUsage {
            name: "codex",
            five_hour: Some(61.0),
            seven_day: None,
            five_hour_detail: None,
            seven_day_detail: None,
        });
        enrich_sidebar(&mut rows, &disk, 240);
        assert_eq!(rows[0].model.as_deref(), Some("resolved-model"));
        // Dash refresh PR1: "model" and "reason" dropped from this vec --
        // they only ever fed the old 8-line sidebar disclosure block, which
        // the 2-line fact block replaced (see `SidebarRow::disclosure`'s own
        // doc comment). "group"/"budget"/"branch"/"writer"/"since"/"signal"
        // remain: the `^A i` per-row inspector still reads them.
        assert_eq!(rows[0].disclosure.len(), 6);
        assert!(
            rows[0]
                .disclosure
                .iter()
                .any(|(k, v)| k == "budget" && v == "12000 / 80000 · 5h 61%")
        );
        assert!(
            rows[0]
                .disclosure
                .iter()
                .any(|(k, v)| k == "since" && v.contains("3m"))
        );
        assert!(
            rows[0]
                .disclosure
                .iter()
                .any(|(k, v)| k == "branch" && v == style::PLACEHOLDER)
        );
    }

    /// `^A p` and `^A ?` are real bindings, and `p` did not used to be one.
    #[test]
    fn ctrl_a_p_opens_the_palette_and_ctrl_a_question_opens_help() {
        assert_eq!(
            filter_key(true, key(KeyCode::Char('p'), KeyModifiers::NONE)).1,
            InputVerdict::Dash(DashAction::Palette)
        );
        for code in [KeyCode::Char('?'), KeyCode::Char('h'), KeyCode::Char('H')] {
            assert_eq!(
                filter_key(true, key(code, KeyModifiers::NONE)).1,
                InputVerdict::Dash(DashAction::Help)
            );
        }
    }

    /// Typing filters; Enter runs the caret's own descriptor, and the effect
    /// resolves to exactly the `DashAction` the chord would have produced.
    #[test]
    fn enter_runs_the_selected_palette_row_as_its_own_chord_would() {
        let view = typed(open_palette(ui::PaletteMode::Run), "spawn");
        let rows = view.rows();
        assert!(
            rows.iter().all(|r| match r {
                actions::PaletteRow::Action { label, .. } => *label == "spawn",
                actions::PaletteRow::Note { .. } | actions::PaletteRow::Section(_) => false,
            }),
            "the query must filter the list: {rows:?}"
        );
        let (next, effect) = palette_overlay_reduce(view, key(KeyCode::Enter, KeyModifiers::NONE));
        assert!(next.is_none(), "running an action closes the palette");
        let id = effect.expect("an effect").0;
        let descriptor = actions::descriptor(id).expect("descriptor");
        assert_eq!(descriptor.dash_action(), Some(DashAction::Spawn));
        // And that action is exactly what the keyboard produces, so the
        // palette can reach no code path a chord could not.
        assert_eq!(
            filter_key(true, key(KeyCode::Char('s'), KeyModifiers::NONE)).1,
            InputVerdict::Dash(DashAction::Spawn)
        );
    }

    /// Finding F06's own rule, extended to the palette: while it is open the
    /// pointer reaches nothing underneath it, so a query typed into it
    /// cannot possibly be routed to a child. (The keyboard half is
    /// structural -- the event loop's overlay branch never calls
    /// `write_operator_input` -- and every reducer above returns the query
    /// in its own view rather than any bytes.)
    #[test]
    fn nothing_reaches_the_child_while_the_palette_is_open() {
        let snap = hit::FrameSnapshot {
            frame: Rect::new(0, 0, 120, 40),
            sidebar: Rect::new(0, 2, 44, 36),
            grid: Rect::new(45, 2, 75, 36),
            divider: Rect::new(44, 2, 1, 36),
            overlay: Some(Rect::new(30, 5, 60, 30)),
            ..Default::default()
        };
        for (x, y) in [(0u16, 0u16), (5, 3), (50, 10), (119, 39), (44, 20)] {
            for kind in [
                MouseEventKind::ScrollUp,
                MouseEventKind::ScrollDown,
                MouseEventKind::Down(MouseButton::Left),
                MouseEventKind::Down(MouseButton::Right),
            ] {
                let route = route_mouse(
                    &snap,
                    event::MouseEvent {
                        kind,
                        column: x,
                        row: y,
                        modifiers: KeyModifiers::NONE,
                    },
                    true,
                    true,
                    false,
                );
                assert_ne!(
                    route,
                    MouseRoute::Grid,
                    "({x}, {y}) {kind:?} reached the child"
                );
            }
        }
        // And every printable key the palette is given comes back inside its
        // own view -- there is no byte path out of the reducer at all.
        let view = typed(open_palette(ui::PaletteMode::Run), "rm -rf /");
        assert_eq!(view.query, "rm -rf /");
    }

    /// Issue #354 phase 5 (phase-4 residual): the flag records DISMISSAL, not
    /// display. A launch that never dismissed the tip must leave the flag
    /// unset, so the same operator is shown it again.
    #[test]
    fn the_first_run_tip_flag_is_written_only_when_the_tip_is_dismissed() {
        let dir = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(dir.path().to_path_buf());
        // The launch decision alone -- exactly what `run_dashboard` does now.
        let mut first_run_tip = !first_run_tip_seen(&state);
        assert!(first_run_tip, "a fresh operator is shown the tip");
        assert!(
            !first_run_tip_seen(&state),
            "showing the tip must not flag it as seen"
        );
        // The dismissal rule, applied exactly as the event loop applies it --
        // including defect 2's fix (review of #354, MEDIUM): the `Esc` that
        // dismisses the tip is consumed, never also forwarded, since it
        // happens once per operator, ever; an `Esc` that does not dismiss the
        // tip is unaffected.
        let dismiss =
            |armed: bool, k: KeyEvent, shown: &mut bool, state: &StateDir| -> InputVerdict {
                let (_, verdict) = filter_key(armed, k);
                let dismissed_by_esc =
                    *shown && k.code == KeyCode::Esc && !matches!(verdict, InputVerdict::Dash(_));
                if *shown && (matches!(verdict, InputVerdict::Dash(_)) || k.code == KeyCode::Esc) {
                    *shown = false;
                    mark_first_run_tip_seen(state);
                }
                if dismissed_by_esc {
                    InputVerdict::ToChild(Vec::new())
                } else {
                    verdict
                }
            };
        // Ordinary typing to the child dismisses nothing, writes nothing, and
        // still reaches the child.
        let verdict = dismiss(
            false,
            key(KeyCode::Char('x'), KeyModifiers::NONE),
            &mut first_run_tip,
            &state,
        );
        assert!(first_run_tip && !first_run_tip_seen(&state));
        assert!(matches!(verdict, InputVerdict::ToChild(bytes) if !bytes.is_empty()));
        // The dismissing Esc flags the tip seen, but this exact keystroke
        // must never also reach the child -- it used to (additively); that
        // regression is defect 2.
        let verdict = dismiss(
            false,
            key(KeyCode::Esc, KeyModifiers::NONE),
            &mut first_run_tip,
            &state,
        );
        assert!(!first_run_tip, "Esc dismisses it");
        assert!(first_run_tip_seen(&state), "and only then is it flagged");
        assert!(
            matches!(&verdict, InputVerdict::ToChild(bytes) if bytes.is_empty()),
            "the dismissing Esc must be consumed, not forwarded: {verdict:?}"
        );
        // The next launch never shows it again, and an ordinary Esc -- the
        // tip already gone -- reaches the child exactly as before.
        assert!(!(!first_run_tip_seen(&state)));
        let verdict = dismiss(
            false,
            key(KeyCode::Esc, KeyModifiers::NONE),
            &mut first_run_tip,
            &state,
        );
        assert!(
            matches!(&verdict, InputVerdict::ToChild(bytes) if !bytes.is_empty()),
            "Esc reaches the child once the tip is no longer showing: {verdict:?}"
        );
    }

    /// The first-run tip: absent flag shows it, and the flag is written once
    /// so the next launch never shows it again.
    #[test]
    fn the_first_run_tip_is_shown_once_and_then_flagged() {
        let dir = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(dir.path().to_path_buf());
        assert!(
            !first_run_tip_seen(&state),
            "a fresh operator has not seen it"
        );
        mark_first_run_tip_seen(&state);
        assert!(first_run_tip_seen(&state));
        let flag = first_run_tip_flag(&state);
        assert!(flag.starts_with(state.dash()), "{flag:?}");
        assert_eq!(flag.file_name().and_then(|n| n.to_str()), Some("tip-seen"));
        // Writing it twice is idempotent, never an error.
        mark_first_run_tip_seen(&state);
        assert!(first_run_tip_seen(&state));
    }

    /// A state directory that cannot be created (a plain FILE where the
    /// state root should be) must cost nothing: no panic, no error, the tip
    /// simply shows again next launch.
    #[test]
    fn an_unwritable_state_dir_never_fails_the_first_run_tip() {
        let dir = tempfile::tempdir().expect("tempdir");
        let blocked = dir.path().join("not-a-dir");
        std::fs::write(&blocked, b"i am a file").expect("write");
        let state = StateDir::from_root(blocked);
        assert!(!first_run_tip_seen(&state));
        mark_first_run_tip_seen(&state);
        assert!(
            !first_run_tip_seen(&state),
            "an unwritable state dir must leave the flag unset, not error"
        );
    }

    /// The dismissal rule, as the event loop applies it: any prefixed key
    /// (anything `filter_key` turns into a `DashAction`) or a bare `Esc`
    /// clears the tip; ordinary typing to the child does not.
    #[test]
    fn the_first_run_tip_is_dismissed_by_a_prefixed_key_or_esc() {
        let dismisses = |armed: bool, k: KeyEvent| {
            let (_, verdict) = filter_key(armed, k);
            matches!(verdict, InputVerdict::Dash(_)) || k.code == KeyCode::Esc
        };
        assert!(dismisses(true, key(KeyCode::Char('?'), KeyModifiers::NONE)));
        assert!(dismisses(true, key(KeyCode::Char('p'), KeyModifiers::NONE)));
        assert!(dismisses(false, key(KeyCode::Esc, KeyModifiers::NONE)));
        assert!(!dismisses(
            false,
            key(KeyCode::Char('x'), KeyModifiers::NONE)
        ));
        assert!(!dismisses(
            false,
            key(KeyCode::Char('a'), KeyModifiers::CONTROL)
        ));
    }

    /// Review of 9314156, finding 1: opening the inspector on a LIVE
    /// attached pane that is merely selected must not clear its `◆`. Only
    /// focus plus an unoccluded render does that, and a row that can never
    /// be focused (ended, or not attached) is the only one the inspector may
    /// acknowledge on its own.
    #[test]
    fn the_inspector_never_acknowledges_a_live_attached_pane() {
        use super::super::attention::{Authority, Lifecycle, Observation, compose};
        let done = compose(
            Some(&compose(
                None,
                &[Observation::new(Authority::QuietHeuristic, "busy", 40, 100)
                    .with_lifecycle(Lifecycle::Working)],
                100,
            )),
            &[
                Observation::new(Authority::QuietHeuristic, "quiet", 40, 200)
                    .with_lifecycle(Lifecycle::Settled),
            ],
            200,
        );

        // A live, attached, selected-but-unfocused pane showing `◆`.
        let panes = vec![pane_row("aaa11111", "claude")];
        let mut rows = assemble_sidebar(&panes, &[], &HashMap::new(), 0, 1, DASHBOARD_PID, 900);
        let mut disk = DiskFacts::default();
        disk.attention.insert("aaa11111".into(), done.clone());
        enrich_sidebar(&mut rows, &disk, 900);
        assert_eq!(ui::glyph_for(&rows[0]), ui::Glyph::DoneUnread);
        assert!(rows[0].attached && rows[0].exit_code.is_none());
        assert_eq!(
            inspect_ack_candidate(&rows[0]),
            None,
            "a live attached pane is acknowledged by viewing it, not by the inspector"
        );

        // The same status on a RETAINED ENDED row -- one that can never be
        // focused, so no render of it can ever acknowledge it. That row is
        // the inspector's to clear, and still is.
        let ended = vec![ended_pane_row("bbb22222", 0, 190)];
        let mut rows = assemble_sidebar(&ended, &[], &HashMap::new(), 0, 0, DASHBOARD_PID, 900);
        disk.attention.insert("bbb22222".into(), done.clone());
        enrich_sidebar(&mut rows, &disk, 900);
        assert_eq!(ui::glyph_for(&rows[0]), ui::Glyph::DoneUnread);
        assert_eq!(
            inspect_ack_candidate(&rows[0]),
            Some(("bbb22222".to_string(), done.revision)),
            "a row this dashboard cannot focus is the inspector's to acknowledge"
        );
    }

    /// Review of 9314156, finding 2: a pending single click belongs to the
    /// dialog it was made in. Two different dialogs -- and the same dialog
    /// reopened -- never share an identity, so the loop's identity check
    /// drops the stale click before a second click anywhere else can
    /// complete a double.
    #[test]
    fn a_pending_overlay_click_never_survives_into_another_dialog() {
        let menu = |target: &str| {
            ui::Overlay::Menu(ui::MenuView {
                target: target.into(),
                subject: format!("{target} \u{b7} worker"),
                entries: Vec::new(),
                cursor: 0,
                offset: 0,
                confirm: None,
            })
        };
        let errors = ui::Overlay::Errors(ui::ErrorsView {
            items: vec![err_item("boom")],
            cursor: 0,
            offset: 0,
            mark: 0,
        });
        let none = ui::Overlay::None;

        // Different dialogs, the same dialog on a different row, and the
        // closed state in between are each their own identity.
        assert_ne!(overlay_identity(&menu("aaa")), overlay_identity(&errors));
        assert_ne!(
            overlay_identity(&menu("aaa")),
            overlay_identity(&menu("bbb"))
        );
        assert_ne!(overlay_identity(&menu("aaa")), overlay_identity(&none));
        assert_eq!(
            overlay_identity(&menu("aaa")),
            overlay_identity(&menu("aaa"))
        );

        // Review of cc92a56 (finding 2): the palette's rows are a function of
        // its query, so a keystroke that re-filters the list is a new
        // identity -- otherwise a click on row 3, one keystroke, and a second
        // click on row 3 activated whatever had moved under the pointer.
        let palette = |query: &str| {
            ui::Overlay::Palette(ui::PaletteView {
                mode: ui::PaletteMode::Run,
                query: query.to_string(),
                ctx: actions::ActionContext::default(),
                cursor: 0,
                offset: 0,
            })
        };
        assert_ne!(
            overlay_identity(&palette("")),
            overlay_identity(&palette("n"))
        );
        assert_ne!(
            overlay_identity(&palette("n")),
            overlay_identity(&palette("nu"))
        );
        assert_eq!(
            overlay_identity(&palette("nu")),
            overlay_identity(&palette("nu"))
        );

        // The loop's own rule, replayed: a click made in one dialog is
        // dropped the moment the identity changes -- which includes the
        // `Overlay::None` a close leaves behind, so close-then-reopen never
        // completes a double-click either.
        let mut last_click: Option<(usize, Instant)> = None;
        let mut last_ident = overlay_identity(&none);
        let mut step = |overlay: &ui::Overlay,
                        click: Option<usize>,
                        last_click: &mut Option<(usize, Instant)>|
         -> bool {
            let ident = overlay_identity(overlay);
            if ident != last_ident {
                *last_click = None;
                last_ident = ident;
            }
            match click {
                Some(index) => {
                    let double = last_click.is_some_and(|(last, at)| {
                        last == index
                            && Instant::now().saturating_duration_since(at) <= DOUBLE_CLICK
                    });
                    // Exactly the loop's own bookkeeping: a completed
                    // double-click consumes the pending one.
                    *last_click = if double {
                        None
                    } else {
                        Some((index, Instant::now()))
                    };
                    double
                }
                None => false,
            }
        };
        assert!(!step(&menu("aaa"), Some(3), &mut last_click));
        assert!(
            step(&menu("aaa"), Some(3), &mut last_click),
            "same dialog doubles"
        );
        assert!(!step(&menu("aaa"), Some(3), &mut last_click));
        // Close it, then open a different dialog within the double-click
        // window and click the same index once: not a double.
        assert!(!step(&none, None, &mut last_click));
        assert!(
            !step(&errors, Some(3), &mut last_click),
            "a click from another dialog must not complete a double here"
        );
        // Same dialog, reopened: still not a double.
        assert!(!step(&none, None, &mut last_click));
        assert!(!step(&errors, Some(3), &mut last_click));
    }
}
