//! Approvals inbox UI (#840): the strip across the bottom of the dashboard, its keys, and the
//! cross-dashboard list. Everything here draws or routes; the hold and the decision live in
//! `ctx::approvals`, and a decision only ever leaves this process on the hook's own connection.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Modifier;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Paragraph};

use super::super::approvals::{self, Decision, Hub, Resolved};
use super::super::state::StateDir;
use super::input::ApprovalKey;
use super::{sessions, ui};
use crate::style;

/// Border, three content lines, border.
pub(super) const STRIP_ROWS: u16 = 5;
const MIN_TERM_ROWS: u16 = 16;
const MIN_TERM_COLS: u16 = 40;

/// Rows the strip takes from a terminal of this size: none while nothing is pending or when the
/// terminal is too small to give rows away (the header badge and sidebar marker still show).
pub(super) fn strip_rows(pending: usize, term_cols: u16, term_rows: u16) -> u16 {
    if pending == 0 || term_cols < MIN_TERM_COLS || term_rows < MIN_TERM_ROWS {
        return 0;
    }
    STRIP_ROWS
}

/// `strip_rows` for the live terminal; costs nothing while the inbox is off or empty.
pub(super) fn current_strip_rows(hub: Option<&Hub>) -> u16 {
    let pending = hub.map_or(0, Hub::count);
    if pending == 0 {
        return 0;
    }
    let (cols, rows) = crossterm::terminal::size().unwrap_or((0, 0));
    strip_rows(pending, cols, rows)
}

/// What the strip shows for the selected request.
pub(super) struct StripFacts {
    pub position: usize,
    pub total: usize,
    pub who: String,
    pub waited_secs: u64,
    pub released: bool,
    pub tool: String,
    pub preview: String,
    /// The whole input is visible: not redacted, not capped, not wider than the strip.
    pub answerable: bool,
    /// The harness's own always-allow rule, as a label; `None` when it offered none.
    pub always: Option<String>,
}

/// The whole input is visible: not redacted or capped, and "<tool>  <preview>" fits the strip's
/// display width (borders take two columns; wide characters take two cells each).
fn fits_strip(request: &approvals::Request, term_cols: u16) -> bool {
    use unicode_width::UnicodeWidthStr;
    let line_cols = request.tool.width() + 2 + request.preview.width();
    request.fully_shown && line_cols <= usize::from(term_cols.saturating_sub(2))
}

/// Facts for the selected request, resolved against this dashboard's own roster rows.
pub(super) fn strip_facts(
    hub: &Hub,
    rows: &[ui::SidebarRow],
    term_cols: u16,
) -> Option<StripFacts> {
    let Some(item) = hub.current() else {
        hub.mark_drawn(None);
        return None;
    };
    let request = &item.request;
    let row = rows.iter().position(|row| row.short == request.short);
    let who = match row.map(|index| (index, &rows[index])) {
        Some((index, row)) => {
            let model = row
                .model
                .as_deref()
                .map(|m| format!(" {m}"))
                .unwrap_or_default();
            format!(
                "{} \u{b7} {}{model} \u{b7} pane {}",
                row.role,
                row.harness,
                index + 1
            )
        }
        None => request.short.clone(),
    };
    // Border and one cell of margin each side, then "<tool>  <preview>" on one clipped line.
    let answerable = fits_strip(request, term_cols);
    hub.mark_drawn(Some((item.conn, answerable)));
    Some(StripFacts {
        answerable,
        position: hub.selected_index() + 1,
        total: hub.count(),
        who,
        waited_secs: item.since.elapsed().as_secs(),
        released: request.released,
        tool: request.tool.clone(),
        preview: request.preview.clone(),
        always: request.always.clone(),
    })
}

fn clock(secs: u64) -> String {
    format!("{}:{:02}", secs / 60, secs % 60)
}

pub(super) fn render_strip(f: &mut Frame, area: Rect, facts: &StripFacts) {
    if area.is_empty() {
        return;
    }
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(style::tui::warning())
        .title(Line::from(Span::styled(
            format!(" \u{2691} APPROVALS {} of {} ", facts.position, facts.total),
            style::tui::warning().add_modifier(Modifier::BOLD),
        )))
        .title_top(Line::from(Span::styled(" ^A a all ", style::tui::hint())).right_aligned());
    let inner = block.inner(area);
    f.render_widget(block, area);
    let waiting = if facts.released {
        format!("waiting in pane {}", clock(facts.waited_secs))
    } else {
        format!("waiting {}", clock(facts.waited_secs))
    };
    let keys = match (&facts.always, facts.released, facts.answerable) {
        (_, true, _) => {
            "answer in the pane   ^A g go to pane   ^A ] next   ^A a open all".to_string()
        }
        (_, _, false) => {
            "not shown in full: answer in pane (^A g)   ^A d deny   ^A ] next   ^A a open all"
                .to_string()
        }
        (Some(label), _, _) => format!(
            "^A y allow once   ^A Y always: {label}   ^A d deny   ^A ] next   ^A g go to pane"
        ),
        (None, _, _) => {
            "^A y allow once   ^A d deny   ^A ] next   ^A g go to pane   ^A a open all".to_string()
        }
    };
    let lines = vec![
        Line::from(format!("{} \u{b7} {waiting}", facts.who)),
        Line::from(vec![
            Span::styled(
                format!("{}  ", facts.tool),
                style::tui::warning().add_modifier(Modifier::BOLD),
            ),
            Span::raw(facts.preview.clone()),
        ]),
        Line::from(Span::styled(keys, style::tui::muted())),
    ];
    f.render_widget(Paragraph::new(lines), inner);
}

/// What a chord asks the event loop to do beyond what the inbox already did.
#[derive(Default)]
pub(super) struct KeyOutcome {
    pub notice: Option<String>,
    /// Focus this session's pane (it has just been released to its native dialog).
    pub goto: Option<String>,
    pub overlay: Option<ui::Overlay>,
}

/// One approvals chord. With the inbox off, or nothing pending, the chord does nothing, as it did before.
pub(super) fn handle_key(hub: Option<&mut Hub>, key: ApprovalKey, state: &StateDir) -> KeyOutcome {
    let Some(hub) = hub else {
        return KeyOutcome::default();
    };
    match key {
        ApprovalKey::List => KeyOutcome {
            overlay: Some(ui::Overlay::Approvals(list_view(
                state,
                &sessions::is_alive,
            ))),
            ..KeyOutcome::default()
        },
        ApprovalKey::Next => {
            hub.next();
            KeyOutcome::default()
        }
        ApprovalKey::Goto => {
            let Some(short) = hub.drawn_item().map(|item| item.request.short.clone()) else {
                return KeyOutcome::default();
            };
            hub.resolve_current(Decision::Release);
            KeyOutcome {
                goto: Some(short),
                ..KeyOutcome::default()
            }
        }
        ApprovalKey::Allow | ApprovalKey::AllowAlways | ApprovalKey::Deny => {
            let (decision, label) = match key {
                ApprovalKey::Allow => (Decision::Allow, "allowed once"),
                ApprovalKey::AllowAlways => (Decision::AllowAlways, "allowed, rule applied"),
                _ => (Decision::Deny, "denied"),
            };
            let notice = match hub.resolve_current(decision) {
                Resolved::Sent => Some(format!("approval {label}")),
                Resolved::NotFullyShown => {
                    Some("that command is not shown in full; answer in its pane (^A g)".to_string())
                }
                Resolved::NoAlways => {
                    Some("Claude offered no always-allow rule for that request".to_string())
                }
                Resolved::InPane => {
                    Some("that request is waiting in its pane; ^A g goes there".to_string())
                }
                Resolved::Nothing => None,
            };
            KeyOutcome {
                notice,
                ..KeyOutcome::default()
            }
        }
    }
}

/// The `^A a` list: every live dashboard's requests, read from the request records. A request another dashboard
/// holds is marked, because only that dashboard can answer it.
pub(super) fn list_view(state: &StateDir, pid_alive: &dyn Fn(u32) -> bool) -> ui::JevErrorsView {
    let own = std::process::id();
    let items = approvals::list_all(state, pid_alive)
        .into_iter()
        .map(|request| {
            let suffix = if request.dash_pid != own {
                format!(" \u{b7} answer in dashboard {}", request.dash_pid)
            } else if request.released {
                " \u{b7} waiting in pane".to_string()
            } else {
                String::new()
            };
            ui::ErrorItem {
                text: format!(
                    "{} \u{b7} {} \u{b7} {}{suffix}",
                    request.short, request.tool, request.preview
                ),
                count: 1,
                age_secs: request.waited_secs(),
                acked: false,
            }
        })
        .collect();
    ui::JevErrorsView {
        items,
        cursor: 0,
        offset: 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    fn facts(released: bool) -> StripFacts {
        StripFacts {
            position: 1,
            total: 4,
            who: "worker \u{b7} claude sonnet \u{b7} pane 2".to_string(),
            waited_secs: 42,
            released,
            tool: "Bash".to_string(),
            preview: "cargo nextest run --no-fail-fast".to_string(),
            answerable: true,
            always: None,
        }
    }

    #[test]
    fn width_is_counted_in_display_cells_and_multiline_input_is_never_fully_shown() {
        // 12 wide characters are 24 cells: 4 ("Bash") + 2 + 24 = 30 cells.
        let wide = "\u{4e2d}".repeat(12);
        let request = approvals::Request::new("abc123", "Bash", &wide, &wide, 1);
        assert!(request.fully_shown);
        assert!(fits_strip(&request, 32));
        assert!(
            !fits_strip(&request, 31),
            "chars().count() would say 18 and offer ^A y"
        );
        let multi = approvals::Request::new("abc123", "Bash", "ls\nrm x", "ls\nrm x", 1);
        assert!(!multi.fully_shown);
        let trailing = approvals::Request::new("abc123", "Bash", "ls\n", "ls\n", 1);
        assert!(trailing.fully_shown);
    }

    #[test]
    fn a_partly_shown_command_offers_no_allow_key() {
        let mut partial = facts(false);
        partial.answerable = false;
        let text = draw(100, 5, |f| render_strip(f, f.area(), &partial));
        assert!(!text.contains("^A y"), "{text}");
        assert!(text.contains("answer in pane"), "{text}");
    }

    fn draw(width: u16, height: u16, paint: impl FnOnce(&mut Frame)) -> String {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).expect("terminal");
        terminal.draw(paint).expect("draw");
        let buffer = terminal.backend().buffer().clone();
        (0..height)
            .map(|y| {
                (0..width)
                    .map(|x| buffer[(x, y)].symbol().to_string())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn the_strip_takes_no_rows_while_nothing_is_pending() {
        assert_eq!(strip_rows(0, 200, 50), 0);
        assert_eq!(strip_rows(1, 200, 50), STRIP_ROWS);
        assert_eq!(strip_rows(3, 30, 50), 0, "too narrow to give rows away");
        assert_eq!(strip_rows(3, 200, 12), 0, "too short to give rows away");
        assert_eq!(current_strip_rows(None), 0);
    }

    #[test]
    fn the_strip_shows_the_request_and_its_keys() {
        let screen = draw(90, STRIP_ROWS, |f| render_strip(f, f.area(), &facts(false)));
        for needle in [
            "APPROVALS 1 of 4",
            "^A a all",
            "worker",
            "pane 2",
            "waiting 0:42",
            "Bash",
            "cargo nextest run --no-fail-fast",
            "^A y allow once",
            "^A d deny",
            "^A g go to pane",
        ] {
            assert!(screen.contains(needle), "missing {needle:?} in\n{screen}");
        }
    }

    #[test]
    fn a_released_request_offers_only_the_pane() {
        let screen = draw(90, STRIP_ROWS, |f| render_strip(f, f.area(), &facts(true)));
        assert!(screen.contains("waiting in pane"), "{screen}");
        assert!(!screen.contains("allow once"), "{screen}");
        assert!(screen.contains("^A g go to pane"), "{screen}");
    }

    fn header_text(approvals: usize) -> String {
        let mut facts = super::super::sidebar_facts::assemble_header_facts(3, 1, 0, 0, None, None);
        facts.approvals = approvals;
        draw(140, 1, |f| ui::render_header(f, f.area(), &facts))
    }

    #[test]
    fn the_header_badge_appears_only_while_something_is_pending() {
        let none = header_text(0);
        assert!(
            !none.contains("approvals") && !none.contains('\u{2691}'),
            "{none}"
        );
        assert!(header_text(2).contains("\u{2691} 2 approvals"));
    }

    #[test]
    fn the_cross_dashboard_list_marks_a_request_another_dashboard_holds() {
        let tmp = tempfile::tempdir().expect("tmp");
        let state = StateDir::resolve(&|_| Some(tmp.path().display().to_string())).expect("state");
        let own = std::process::id();
        let foreign = own.wrapping_add(1);
        let mut mine = approvals::Request::new("aaaa1111", "Bash", "ls", "ls", own);
        mine.nonce = 1;
        let mut theirs = approvals::Request::new("bbbb2222", "Edit", "", "/tmp/x", foreign);
        theirs.nonce = 2;
        theirs.ts += 1;
        for request in [&mine, &theirs] {
            approvals::write_record(&state, request).expect("record");
        }
        let view = list_view(&state, &|_| true);
        let texts: Vec<&str> = view.items.iter().map(|item| item.text.as_str()).collect();
        let foreign_row = texts
            .iter()
            .find(|t| t.starts_with("bbbb2222"))
            .expect("foreign row");
        assert!(
            foreign_row.contains(&format!("answer in dashboard {foreign}")),
            "{texts:?}"
        );
        let own_row = texts
            .iter()
            .find(|t| t.starts_with("aaaa1111"))
            .expect("own row");
        assert!(!own_row.contains("answer in dashboard"), "{own_row}");
    }

    #[cfg(unix)]
    #[test]
    fn allow_and_deny_chords_answer_the_selected_request_and_are_inert_when_idle() {
        use std::time::{Duration, Instant};
        let tmp = tempfile::Builder::new()
            .prefix("ap")
            .tempdir_in(std::env::temp_dir())
            .expect("tmp");
        let state = StateDir::resolve(&|_| Some(tmp.path().display().to_string())).expect("state");
        let mut hub = Hub::bind(&state).expect("hub");
        let idle = handle_key(Some(&mut hub), ApprovalKey::Allow, &state);
        assert!(idle.notice.is_none() && idle.goto.is_none() && idle.overlay.is_none());
        assert!(
            handle_key(None, ApprovalKey::Allow, &state)
                .notice
                .is_none()
        );
        for (chord, expected) in [
            (ApprovalKey::Allow, Decision::Allow),
            (ApprovalKey::Deny, Decision::Deny),
        ] {
            let request = approvals::Request::new("abc123", "Bash", "ls", "ls", std::process::id());
            approvals::write_record(&state, &request).expect("record");
            let sock = approvals::socket_path(&state, std::process::id());
            let waiter = std::thread::spawn(move || {
                approvals::hold(&sock, std::process::id(), &request, Duration::from_secs(5))
            });
            let deadline = Instant::now() + Duration::from_secs(5);
            while hub.count() == 0 && Instant::now() < deadline {
                hub.poll(&|_| true);
                std::thread::sleep(Duration::from_millis(20));
            }
            let outcome = handle_key(Some(&mut hub), chord, &state);
            assert!(outcome.notice.is_some());
            assert_eq!(waiter.join().expect("hook"), Some(expected));
            assert_eq!(hub.count(), 0);
        }
    }
}
