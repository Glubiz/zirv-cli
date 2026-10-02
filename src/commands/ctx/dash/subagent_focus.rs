//! Landing on one native Claude subagent inside its host pane. Claude Code lists its running
//! subagents as rows under the prompt (`⏺ main`, `◯ general-purpose  <description>`); `↓` on an
//! empty prompt selects the first row, `↑`/`↓` move, and Enter on a subagent opens its transcript
//! (verified on Claude Code 2.1.287, `tests/fixtures/claude-subagent-panel/`). Every key is chosen
//! from the pane's own screen, and Enter is only sent while the screen shows the wanted row
//! selected, so a mismatch never opens the wrong agent.

use std::time::{Duration, Instant};

use super::pane::Pane;

const RULE_MIN: usize = 20;
const STEP_TIMEOUT: Duration = Duration::from_millis(2500);
const TOTAL_TIMEOUT: Duration = Duration::from_secs(12);
const MAX_PRESSES: usize = 24;
const DOWN: &[u8] = b"\x1b[B";
const UP: &[u8] = b"\x1b[A";
const ENTER: &[u8] = b"\r";
const ESC: &[u8] = b"\x1b";

/// The subagent to land on, as Claude lists it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Target {
    pub(super) agent_type: Option<String>,
    pub(super) description: String,
    /// The descriptions of the host's other known subagents.
    pub(super) siblings: Vec<String>,
}

#[derive(Debug, PartialEq, Eq)]
struct PanelRow {
    text: String,
    selected: bool,
}

/// The rows Claude lists under the prompt.
#[derive(Debug, PartialEq, Eq)]
struct Panel {
    rows: Vec<PanelRow>,
    /// The footer says Enter views the selected row.
    enter_hint: bool,
}

impl Panel {
    fn selected(&self) -> Option<usize> {
        self.rows.iter().position(|row| row.selected)
    }
}

#[derive(Debug, PartialEq, Eq)]
enum Step {
    Press(&'static [u8]),
    Enter,
    Wait,
}

fn is_rule(row: &str) -> bool {
    let row = row.trim();
    row.chars().count() >= RULE_MIN && row.chars().all(|c| c == '\u{2500}')
}

/// The prompt box between the last two rules holds nothing, so an arrow key cannot move a cursor
/// or recall history.
fn prompt_is_empty(rows: &[String]) -> bool {
    let rules: Vec<usize> = (0..rows.len()).filter(|&i| is_rule(&rows[i])).collect();
    let [.., open, close] = rules[..] else {
        return false;
    };
    let mut body = rows[open + 1..close].iter();
    body.next().is_some_and(|first| first.trim() == "\u{276f}") && body.all(|r| r.trim().is_empty())
}

/// The rows of the list under the status lines: the last block of non-blank rows, set off by a
/// blank row, each starting with a glyph (and `❯` while selected).
fn read_panel(rows: &[String]) -> Option<Panel> {
    let rule = rows.iter().rposition(|row| is_rule(row))?;
    let below = &rows[rule + 1..];
    let end = below.iter().rposition(|row| !row.trim().is_empty())? + 1;
    let start = below[..end].iter().rposition(|row| row.trim().is_empty())? + 1;
    let mut panel_rows = Vec::new();
    for row in &below[start..end] {
        let trimmed = row.trim_start();
        let (selected, rest) = match trimmed.strip_prefix("\u{276f} ") {
            Some(rest) => (true, rest),
            None => (false, trimmed),
        };
        let mut chars = rest.chars();
        let glyph = chars.next()?;
        if glyph.is_alphanumeric() || glyph.is_whitespace() || chars.next() != Some(' ') {
            return None;
        }
        panel_rows.push(PanelRow {
            text: rest[glyph.len_utf8() + 1..].trim().to_string(),
            selected,
        });
    }
    Some(Panel {
        rows: panel_rows,
        enter_hint: below.iter().any(|row| row.contains("Enter to view")),
    })
}

/// The description Claude shows in a row: what follows the agent type, up to the stats column, with
/// a truncation ellipsis dropped. `None` when the row is not of the target's type.
fn row_description(row: &str, agent_type: &str) -> Option<String> {
    let rest = row.to_lowercase();
    let rest = rest.strip_prefix(&agent_type.to_lowercase())?;
    if !rest.starts_with(char::is_whitespace) {
        return None;
    }
    let shown = rest.trim_start().split("  ").next()?;
    let shown = shown.trim_end_matches(['\u{2026}', '.']).trim();
    (!shown.is_empty()).then(|| shown.to_string())
}

/// The one row that can only be the target: of its agent type, showing a prefix of its full
/// description, which no other subagent of the same host shares. Anything less is a mismatch.
fn find_target(panel: &Panel, target: &Target) -> Option<usize> {
    let kind = target
        .agent_type
        .as_deref()
        .filter(|k| !k.trim().is_empty())?;
    let full = target.description.trim().to_lowercase();
    let mut hits = (0..panel.rows.len()).filter(|&i| {
        row_description(&panel.rows[i].text, kind).is_some_and(|shown| full.starts_with(&shown))
    });
    let (row, extra) = (hits.next()?, hits.next());
    if extra.is_some() {
        return None;
    }
    let shown = row_description(&panel.rows[row].text, kind)?;
    let shared = target
        .siblings
        .iter()
        .any(|other| other.trim().to_lowercase().starts_with(&shown));
    (!shared).then_some(row)
}

fn next_step(panel: &Panel, target: usize) -> Step {
    match panel.selected() {
        None => Step::Press(DOWN),
        Some(at) if at < target => Step::Press(DOWN),
        Some(at) if at > target => Step::Press(UP),
        Some(_) if panel.enter_hint => Step::Enter,
        Some(_) => Step::Wait,
    }
}

fn screen_rows(pane: &Pane) -> Vec<String> {
    let screen = pane.screen();
    let cols = screen.size().1;
    screen.rows(0, cols).collect()
}

/// Whether a host pane can be driven to the target right now: idle, at the live screen, an empty
/// prompt, and exactly one listed row naming the subagent.
pub(super) fn drivable(pane: &Pane, target: &Target) -> bool {
    if !pane.injectable() || pane.scrollback() != 0 {
        return false;
    }
    let rows = screen_rows(pane);
    prompt_is_empty(&rows)
        && read_panel(&rows).is_some_and(|panel| find_target(&panel, target).is_some())
}

pub(super) enum Tick {
    Pending,
    Done,
    Failed(&'static str),
}

/// One drive in progress: the keys pressed so far and what the screen showed before the last.
pub(super) struct Focus {
    pub(super) short: String,
    target: Target,
    started: Instant,
    pressed: usize,
    entered: bool,
    awaiting: Option<(Instant, Option<usize>)>,
}

impl Focus {
    pub(super) fn new(short: String, target: Target, now: Instant) -> Self {
        Self {
            short,
            target,
            started: now,
            pressed: 0,
            entered: false,
            awaiting: None,
        }
    }

    /// Advance one step from the pane's screen. Any surprise leaves the panel with Esc (only while
    /// the host is still idle, never over the operator's own typing) and reports it.
    pub(super) fn tick(&mut self, pane: &mut Pane, now: Instant) -> Tick {
        let fail = |pane: &mut Pane, why: &'static str, entered: bool| {
            if entered && pane.injectable() {
                let _ = pane.write_input(ESC);
            }
            Tick::Failed(why)
        };
        if now.duration_since(self.started) > TOTAL_TIMEOUT {
            return fail(pane, "timed out", self.entered);
        }
        if !pane.injectable() || pane.scrollback() != 0 {
            return Tick::Failed("the host is busy");
        }
        let rows = screen_rows(pane);
        let Some(panel) = read_panel(&rows) else {
            return fail(pane, "the subagent list is not on screen", self.entered);
        };
        let Some(target) = find_target(&panel, &self.target) else {
            return fail(pane, "the subagent is not in the list", self.entered);
        };
        let selected = panel.selected();
        if let Some((at, before)) = self.awaiting {
            if selected == before {
                if now.duration_since(at) > STEP_TIMEOUT {
                    return fail(pane, "the list did not respond", self.entered);
                }
                return Tick::Pending;
            }
            self.awaiting = None;
        }
        match next_step(&panel, target) {
            Step::Wait => Tick::Pending,
            Step::Enter => match pane.write_input(ENTER) {
                Ok(()) => Tick::Done,
                Err(_) => Tick::Failed("could not write to the host"),
            },
            Step::Press(key) => {
                if self.pressed >= MAX_PRESSES || pane.write_input(key).is_err() {
                    return fail(pane, "could not move to the subagent", self.entered);
                }
                self.pressed += 1;
                self.entered = true;
                self.awaiting = Some((now, selected));
                Tick::Pending
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rows(text: &str) -> Vec<String> {
        text.lines().map(str::to_string).collect()
    }

    fn target() -> Target {
        Target {
            agent_type: Some("general-purpose".into()),
            description: "sleepy probe".into(),
            siblings: Vec::new(),
        }
    }

    const LAUNCHED: &str =
        include_str!("../../../../tests/fixtures/claude-subagent-panel/launched.txt");
    const DOWN1: &str = include_str!("../../../../tests/fixtures/claude-subagent-panel/down1.txt");
    const DOWN2: &str = include_str!("../../../../tests/fixtures/claude-subagent-panel/down2.txt");

    #[test]
    fn claude_2_1_287_lists_main_then_the_subagent_and_the_selection_walks_with_the_arrows() {
        let launched = read_panel(&rows(LAUNCHED)).expect("panel");
        assert_eq!(launched.rows.len(), 2);
        assert_eq!(launched.rows[0].text, "main");
        assert!(
            launched.rows[1]
                .text
                .starts_with("general-purpose  sleepy probe")
        );
        assert_eq!(launched.selected(), None);
        assert!(prompt_is_empty(&rows(LAUNCHED)));
        let at_main = read_panel(&rows(DOWN1)).expect("panel");
        assert_eq!(at_main.selected(), Some(0));
        assert!(!at_main.enter_hint);
        let at_agent = read_panel(&rows(DOWN2)).expect("panel");
        assert_eq!(at_agent.selected(), Some(1));
        assert!(at_agent.enter_hint);
    }

    #[test]
    fn the_keys_follow_the_screen_and_enter_waits_for_the_verified_row() {
        let want = find_target(&read_panel(&rows(LAUNCHED)).expect("panel"), &target());
        assert_eq!(want, Some(1));
        let step = |text: &str| next_step(&read_panel(&rows(text)).expect("panel"), 1);
        assert_eq!(step(LAUNCHED), Step::Press(DOWN));
        assert_eq!(step(DOWN1), Step::Press(DOWN));
        assert_eq!(step(DOWN2), Step::Enter);
        let from_below = Panel {
            rows: vec![
                PanelRow {
                    text: "main".into(),
                    selected: false,
                },
                PanelRow {
                    text: "a".into(),
                    selected: false,
                },
                PanelRow {
                    text: "b".into(),
                    selected: true,
                },
            ],
            enter_hint: true,
        };
        assert_eq!(next_step(&from_below, 1), Step::Press(UP));
        let unlabelled = Panel {
            enter_hint: false,
            ..read_panel(&rows(DOWN2)).expect("panel")
        };
        assert_eq!(
            next_step(&unlabelled, 1),
            Step::Wait,
            "no Enter before the hint shows"
        );
    }

    #[test]
    fn a_row_is_only_accepted_when_it_can_only_be_that_subagent() {
        let panel = |rows: &[&str]| Panel {
            rows: rows
                .iter()
                .map(|text| PanelRow {
                    text: text.to_string(),
                    selected: false,
                })
                .collect(),
            enter_hint: false,
        };
        let target = |kind: Option<&str>, description: &str, siblings: &[&str]| Target {
            agent_type: kind.map(str::to_string),
            description: description.into(),
            siblings: siblings.iter().map(|s| s.to_string()).collect(),
        };
        let real = read_panel(&rows(LAUNCHED)).expect("panel");
        assert_eq!(
            find_target(&real, &target(Some("general-purpose"), "sleepy probe", &[])),
            Some(1)
        );
        assert_eq!(
            find_target(
                &real,
                &target(Some("general-purpose"), "something else", &[])
            ),
            None
        );
        assert_eq!(
            find_target(&real, &target(None, "sleepy probe", &[])),
            None,
            "the type is required"
        );
        // The target is gone; a lone listed sibling of another type shares its first 24 characters.
        let lone = panel(&["main", "Plan  map the call sites of the focus drive  5s"]);
        let gone = target(
            Some("Explore"),
            "map the call sites of the focus drive and report",
            &[],
        );
        assert_eq!(find_target(&lone, &gone), None);
        // A listed row of the right type is still not enough when another subagent of the host
        // shares the prefix it shows (here a truncated row).
        let same_type = panel(&["main", "Explore  map the call sites of\u{2026}  5s"]);
        let twin = target(
            Some("Explore"),
            "map the call sites of the focus drive and report",
            &["map the call sites of the sink"],
        );
        assert_eq!(find_target(&same_type, &twin), None);
        let alone = target(
            Some("Explore"),
            "map the call sites of the focus drive and report",
            &[],
        );
        assert_eq!(find_target(&same_type, &alone), Some(1));
        // The type must end at a word boundary, and a different description is no match.
        let longer = panel(&["main", "Planner  map it  5s"]);
        assert_eq!(
            find_target(&longer, &target(Some("Plan"), "map it", &[])),
            None
        );
    }

    #[test]
    fn a_screen_without_the_list_or_with_typing_in_the_prompt_is_not_driven() {
        let mut busy = rows(LAUNCHED);
        let prompt = busy
            .iter()
            .position(|r| r.trim() == "\u{276f}")
            .expect("prompt");
        busy[prompt] = "\u{276f} half a sentence".into();
        assert!(!prompt_is_empty(&busy));
        let no_list: Vec<String> = rows(LAUNCHED).into_iter().take(36).collect();
        assert!(read_panel(&no_list).is_none());
    }
}
