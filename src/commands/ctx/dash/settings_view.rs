//! The `/settings` modal (issue #536, Design 1): one searchable list, a detail footer for the
//! focused row and an editor that replaces that footer. The reducer and the renderer are pure;
//! the pane performs the I/O a returned [`SettingsAction`] asks for.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use super::super::config::settings::{APPLIES, Scope, SettingRow};
use crate::style::display_width;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SettingsAction {
    None,
    Close,
    Save {
        key: String,
        raw: String,
        scope: Scope,
    },
    Reset {
        key: String,
        scope: Scope,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum Mode {
    List,
    Edit { buffer: String, scope: Scope },
}

#[derive(Clone, Debug)]
pub struct SettingsState {
    rows: Vec<SettingRow>,
    query: String,
    selected: usize,
    mode: Mode,
    message: Option<String>,
}

/// The text the editor starts from: the current value without its quotes, flipped when boolean.
fn initial_buffer(row: &SettingRow) -> String {
    match (row.kind, row.value.as_str()) {
        (_, "(default)" | "(redacted)") if row.kind != "bool" => String::new(),
        ("bool", "true") => "false".to_string(),
        ("bool", _) => "true".to_string(),
        (_, value) => value.trim_matches('"').to_string(),
    }
}

fn wrap(text: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut lines = Vec::new();
    let mut line = String::new();
    for word in text.split_whitespace() {
        if !line.is_empty() && display_width(&line) + 1 + display_width(word) > width {
            lines.push(std::mem::take(&mut line));
        }
        if !line.is_empty() {
            line.push(' ');
        }
        line.push_str(word);
    }
    if !line.is_empty() {
        lines.push(line);
    }
    lines
}

/// Truncate with an ellipsis, then pad with spaces to exactly `width` columns.
fn fit(text: &str, width: usize) -> String {
    let mut out = String::new();
    let mut used = 0;
    let overflow = display_width(text) > width;
    let limit = if overflow {
        width.saturating_sub(1)
    } else {
        width
    };
    for ch in text.chars() {
        let w = display_width(ch.encode_utf8(&mut [0; 4]));
        if used + w > limit {
            break;
        }
        out.push(ch);
        used += w;
    }
    if overflow && width > 0 {
        out.push('\u{2026}');
        used += 1;
    }
    out.push_str(&" ".repeat(width.saturating_sub(used)));
    out
}

impl SettingsState {
    pub fn new(rows: Vec<SettingRow>, query: &str) -> Self {
        Self {
            rows,
            query: query.to_string(),
            selected: 0,
            mode: Mode::List,
            message: None,
        }
    }

    /// Replace the rows after a write, keeping the focused key.
    pub fn set_rows(&mut self, rows: Vec<SettingRow>) {
        let key = self.selected_row().map(|row| row.key.clone());
        self.rows = rows;
        if let Some(index) =
            key.and_then(|key| self.filtered().iter().position(|row| row.key == key))
        {
            self.selected = index;
        }
    }

    pub fn set_message(&mut self, message: String) {
        self.message = Some(message);
    }

    /// Leave the editor after a successful write.
    pub fn finish_edit(&mut self) {
        self.mode = Mode::List;
    }

    fn filtered(&self) -> Vec<&SettingRow> {
        let query = self.query.to_lowercase();
        self.rows
            .iter()
            .filter(|row| {
                query.is_empty()
                    || row.key.to_lowercase().contains(&query)
                    || row.description.to_lowercase().contains(&query)
            })
            .collect()
    }

    pub fn selected_row(&self) -> Option<&SettingRow> {
        let rows = self.filtered();
        rows.get(self.selected.min(rows.len().saturating_sub(1)))
            .copied()
    }

    fn move_selection(&mut self, delta: isize) {
        let count = self.filtered().len();
        self.selected = self
            .selected
            .saturating_add_signed(delta)
            .min(count.saturating_sub(1));
    }

    pub fn key(&mut self, key: KeyEvent) -> SettingsAction {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match self.mode.clone() {
            Mode::List => self.list_key(key.code, ctrl),
            Mode::Edit { buffer, scope } => self.edit_key(key.code, ctrl, buffer, scope),
        }
    }

    fn list_key(&mut self, code: KeyCode, ctrl: bool) -> SettingsAction {
        self.message = None;
        match code {
            KeyCode::Esc => return SettingsAction::Close,
            KeyCode::Up => self.move_selection(-1),
            KeyCode::Down => self.move_selection(1),
            KeyCode::Enter => {
                let Some(row) = self.selected_row() else {
                    return SettingsAction::None;
                };
                let Some(scope) = row.scopes.first().copied() else {
                    self.message = Some(format!("{} cannot be edited here", row.key));
                    return SettingsAction::None;
                };
                self.mode = Mode::Edit {
                    buffer: initial_buffer(row),
                    scope,
                };
            }
            KeyCode::Char('r') if ctrl => {
                let Some(row) = self.selected_row() else {
                    return SettingsAction::None;
                };
                if row.scopes.is_empty() {
                    self.message = Some(format!("{} cannot be edited here", row.key));
                    return SettingsAction::None;
                }
                return SettingsAction::Reset {
                    key: row.key.clone(),
                    scope: Scope::User,
                };
            }
            KeyCode::Char(ch) if !ctrl => {
                self.query.push(ch);
                self.selected = 0;
            }
            KeyCode::Backspace => {
                self.query.pop();
                self.selected = 0;
            }
            _ => {}
        }
        SettingsAction::None
    }

    fn edit_key(
        &mut self,
        code: KeyCode,
        ctrl: bool,
        mut buffer: String,
        scope: Scope,
    ) -> SettingsAction {
        let Some(row) = self.selected_row().cloned() else {
            self.mode = Mode::List;
            return SettingsAction::None;
        };
        match code {
            KeyCode::Esc => {
                self.message = None;
                self.mode = Mode::List;
                return SettingsAction::None;
            }
            KeyCode::Enter => {
                return SettingsAction::Save {
                    key: row.key,
                    raw: buffer,
                    scope,
                };
            }
            KeyCode::Char('r') if ctrl => {
                return SettingsAction::Reset {
                    key: row.key,
                    scope,
                };
            }
            KeyCode::Tab => {
                let at = row.scopes.iter().position(|s| *s == scope).unwrap_or(0);
                let next = row.scopes[(at + 1) % row.scopes.len()];
                self.mode = Mode::Edit {
                    buffer,
                    scope: next,
                };
                return SettingsAction::None;
            }
            KeyCode::Char(' ') if row.kind == "bool" => {
                buffer = if buffer == "true" { "false" } else { "true" }.to_string();
            }
            KeyCode::Char(ch) if !ctrl => buffer.push(ch),
            KeyCode::Backspace => {
                buffer.pop();
            }
            _ => return SettingsAction::None,
        }
        self.mode = Mode::Edit { buffer, scope };
        SettingsAction::None
    }

    /// The whole modal as plain lines, each exactly `width` columns wide.
    pub fn lines(&self, width: usize, height: usize) -> Vec<String> {
        let width = width.max(24);
        let inner = width - 4;
        let boxed = |text: &str| format!("| {} |", fit(text, inner));
        let rule = format!("|{}|", "-".repeat(width - 2));
        let rows = self.filtered();
        let selected = self.selected.min(rows.len().saturating_sub(1));

        let title = format!("+-- /settings {}", "");
        let count = format!(
            " {} setting{} --+",
            rows.len(),
            if rows.len() == 1 { "" } else { "s" }
        );
        let dashes = width.saturating_sub(display_width(&title) + display_width(&count));
        let mut out = vec![fit(&format!("{title}{}{count}", "-".repeat(dashes)), width)];
        out.push(boxed(&format!("search: {}_", self.query)));
        out.push(boxed(""));

        let wide = inner >= 77;
        let cell =
            |marker: &str, key: &str, value: &str, source: &str, scopes: &str, applies: &str| {
                let key_w = if wide { 30 } else { 24 };
                let mut line = format!(
                    "{marker} {} {} {}",
                    fit(key, key_w),
                    fit(value, 12),
                    fit(source, 10)
                );
                if wide {
                    line.push_str(&format!(" {} {}", fit(scopes, 7), applies));
                }
                line
            };
        out.push(boxed(&cell(
            " ", "KEY", "VALUE", "SOURCE", "SCOPES", "APPLIES",
        )));

        let detail = self.detail_lines(&rows, selected, inner);
        let fixed = out.len() + 1 + detail.len() + 3;
        let visible = height.saturating_sub(fixed).max(1);
        let start = (selected + 1).saturating_sub(visible);
        if rows.is_empty() {
            out.push(boxed("(no setting matches)"));
        }
        for (index, row) in rows.iter().enumerate().skip(start).take(visible) {
            let scopes = if row.scopes.is_empty() {
                "-".to_string()
            } else {
                row.scopes
                    .iter()
                    .map(|s| s.letter().to_string())
                    .collect::<Vec<_>>()
                    .join(" ")
            };
            let marker = if index == selected { ">" } else { " " };
            out.push(boxed(&cell(
                marker,
                &row.key,
                &row.value,
                row.source,
                &scopes,
                row.applies,
            )));
        }
        out.push(rule.clone());
        out.extend(detail.iter().map(|line| boxed(line)));
        out.push(rule);
        let hint = match self.mode {
            Mode::List => "type to filter  Up/Down move  Enter edit  Ctrl+R reset  Esc close",
            Mode::Edit { .. } => "Enter save  Tab scope  Ctrl+R reset to inherited  Esc cancel",
        };
        out.extend(wrap(hint, inner).iter().map(|line| boxed(line)));
        out.push(format!("+{}+", "-".repeat(width - 2)));
        out
    }

    fn detail_lines(&self, rows: &[&SettingRow], selected: usize, inner: usize) -> Vec<String> {
        let Some(row) = rows.get(selected) else {
            return vec![String::new()];
        };
        let mut out = vec![format!("{}  ({}, default: built-in)", row.key, row.kind)];
        match &self.mode {
            Mode::List => {
                out.extend(wrap(&row.description, inner));
                out.extend(wrap(
                    &format!(
                        "Winning layer: {}.  Shadowed: {}.  Applies: {}.  U=user P=project",
                        row.source, row.shadowed, row.applies
                    ),
                    inner,
                ));
            }
            Mode::Edit { buffer, scope } => {
                out.push(format!("new value: {buffer}_"));
                let radio = |candidate: Scope, label: &str| {
                    let mark = if !row.scopes.contains(&candidate) {
                        "[n/a]"
                    } else if candidate == *scope {
                        "(o)"
                    } else {
                        "( )"
                    };
                    format!("{mark} {label}")
                };
                out.extend(wrap(
                    &format!(
                        "scope:  {}   {}",
                        radio(Scope::User, "user ~/.zirv/ctx.toml"),
                        radio(Scope::Project, "project (narrows only)"),
                    ),
                    inner,
                ));
                out.extend(wrap(&format!("applies: {APPLIES}"), inner));
            }
        }
        if let Some(locked) = &row.locked {
            out.extend(wrap(locked, inner));
        }
        if let Some(message) = &self.message {
            out.extend(wrap(message, inner));
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(key: &str, kind: &'static str, value: &str, source: &'static str) -> SettingRow {
        SettingRow {
            key: key.to_string(),
            kind,
            value: value.to_string(),
            source,
            scopes: vec![Scope::User, Scope::Project],
            applies: APPLIES,
            description: format!("{key} description"),
            shadowed: "none".to_string(),
            locked: None,
            sensitive: false,
        }
    }

    fn rows() -> Vec<SettingRow> {
        vec![
            row("pace.enabled", "bool", "true", "default"),
            row("pace.max_wait_secs", "integer", "900", "project"),
            row("memory.enabled", "bool", "(default)", "default"),
        ]
    }

    fn press(state: &mut SettingsState, code: KeyCode) -> SettingsAction {
        state.key(KeyEvent::new(code, KeyModifiers::NONE))
    }

    fn ctrl(state: &mut SettingsState, ch: char) -> SettingsAction {
        state.key(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::CONTROL))
    }

    #[test]
    fn typing_filters_the_list_and_backspace_widens_it_again() {
        let mut state = SettingsState::new(rows(), "");
        for ch in "pace_".chars() {
            press(&mut state, KeyCode::Char(ch));
        }
        assert_eq!(state.filtered().len(), 0);
        press(&mut state, KeyCode::Backspace);
        press(&mut state, KeyCode::Backspace);
        assert_eq!(state.filtered().len(), 2);
        assert_eq!(state.selected_row().unwrap().key, "pace.enabled");
        press(&mut state, KeyCode::Down);
        assert_eq!(state.selected_row().unwrap().key, "pace.max_wait_secs");
        press(&mut state, KeyCode::Down);
        assert_eq!(state.selected_row().unwrap().key, "pace.max_wait_secs");
    }

    #[test]
    fn enter_edits_and_saves_at_the_default_user_scope() {
        let mut state = SettingsState::new(rows(), "max_wait");
        press(&mut state, KeyCode::Enter);
        for _ in 0..3 {
            press(&mut state, KeyCode::Backspace);
        }
        for ch in "600".chars() {
            press(&mut state, KeyCode::Char(ch));
        }
        assert_eq!(
            press(&mut state, KeyCode::Enter),
            SettingsAction::Save {
                key: "pace.max_wait_secs".into(),
                raw: "600".into(),
                scope: Scope::User
            }
        );
    }

    #[test]
    fn tab_cycles_scopes_and_skips_the_ones_a_row_does_not_allow() {
        let mut only = row("agent", "string", "\"claude\"", "user");
        only.scopes = vec![Scope::User, Scope::Project];
        let mut state = SettingsState::new(vec![only], "");
        press(&mut state, KeyCode::Enter);
        press(&mut state, KeyCode::Tab);
        assert_eq!(
            press(&mut state, KeyCode::Enter),
            SettingsAction::Save {
                key: "agent".into(),
                raw: "claude".into(),
                scope: Scope::Project
            }
        );
    }

    #[test]
    fn space_toggles_a_boolean_and_esc_cancels_before_closing() {
        let mut state = SettingsState::new(rows(), "pace.enabled");
        press(&mut state, KeyCode::Enter);
        assert!(matches!(&state.mode, Mode::Edit { buffer, .. } if buffer == "false"));
        press(&mut state, KeyCode::Char(' '));
        assert!(matches!(&state.mode, Mode::Edit { buffer, .. } if buffer == "true"));
        assert_eq!(press(&mut state, KeyCode::Esc), SettingsAction::None);
        assert_eq!(state.mode, Mode::List);
        assert_eq!(press(&mut state, KeyCode::Esc), SettingsAction::Close);
    }

    #[test]
    fn ctrl_r_resets_in_both_modes_and_a_locked_row_cannot_be_edited() {
        let mut state = SettingsState::new(rows(), "pace.enabled");
        assert_eq!(
            ctrl(&mut state, 'r'),
            SettingsAction::Reset {
                key: "pace.enabled".into(),
                scope: Scope::User
            }
        );
        let mut secret = row(
            "proxy.typesafe.credential_env",
            "string",
            "(redacted)",
            "user",
        );
        secret.scopes.clear();
        secret.sensitive = true;
        let mut state = SettingsState::new(vec![secret], "");
        assert_eq!(press(&mut state, KeyCode::Enter), SettingsAction::None);
        assert_eq!(state.mode, Mode::List);
        assert!(
            state
                .message
                .as_deref()
                .unwrap()
                .contains("cannot be edited")
        );
    }

    fn assert_boxed(lines: &[String], width: usize) {
        for line in lines {
            assert_eq!(display_width(line), width, "ragged line: {line:?}");
        }
    }

    #[test]
    fn narrow_list_snapshot_drops_the_right_hand_columns() {
        let state = SettingsState::new(rows(), "pace");
        let lines = state.lines(60, 20);
        assert_boxed(&lines, 60);
        assert_eq!(
            lines.join("\n"),
            include_str!("../../../../tests/fixtures/settings_list_60.txt").trim_end_matches('\n')
        );
    }

    #[test]
    fn wide_list_snapshot_shows_scopes_and_applies() {
        let state = SettingsState::new(rows(), "pace");
        let lines = state.lines(120, 20);
        assert_boxed(&lines, 120);
        assert_eq!(
            lines.join("\n"),
            include_str!("../../../../tests/fixtures/settings_list_120.txt").trim_end_matches('\n')
        );
    }

    #[test]
    fn edit_state_snapshots_replace_the_footer_at_both_widths() {
        let mut state = SettingsState::new(rows(), "max_wait");
        press(&mut state, KeyCode::Enter);
        state.set_message("pace.max_wait_secs: expected an integer".to_string());
        for (width, fixture) in [
            (
                60,
                include_str!("../../../../tests/fixtures/settings_edit_60.txt"),
            ),
            (
                120,
                include_str!("../../../../tests/fixtures/settings_edit_120.txt"),
            ),
        ] {
            let lines = state.lines(width, 20);
            assert_boxed(&lines, width);
            assert_eq!(lines.join("\n"), fixture.trim_end_matches('\n'));
        }
    }
}
