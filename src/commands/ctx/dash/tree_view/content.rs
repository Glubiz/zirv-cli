//! The text and colour of every card in the tree. Pure: nothing here knows a
//! coordinate, so the plan can size a card from its content and the painter can
//! draw it without a second opinion.

use chrono::TimeZone;
use crossterm::event::KeyCode;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::Span;
use unicode_width::UnicodeWidthStr;

use super::super::super::graph::{Event, Node};
use super::super::super::sessions;
use super::super::ui::JevSectionFact;
use super::model::{Agent, Model, Sel};
use super::{Moment, SupervisorFact, TreeData, TreeFacts};
use crate::commands::workflow::StepMark;
use crate::style::tui;

/// Mockup A's legend. Named colours, like the rest of the dashboard.
pub(super) mod pal {
    use super::{Color, Style, tui};

    pub(in super::super) const SEAT: Color = Color::Cyan;
    pub(in super::super) const AGENT: Color = Color::LightBlue;
    pub(in super::super) const JEV: Color = Color::Green;
    pub(in super::super) const ARCH: Color = Color::Magenta;
    /// A's `b-sel`.
    pub(in super::super) const SELECTED_BG: Color = Color::Rgb(0x26, 0x32, 0x4A);
    /// A's `b-archbg`.
    pub(in super::super) const FIRED_BG: Color = Color::Rgb(0x2E, 0x29, 0x44);

    pub(in super::super) fn fg(color: Color) -> Style {
        Style::default().fg(color)
    }

    pub(in super::super) fn strong(color: Color) -> Style {
        fg(color).add_modifier(super::Modifier::BOLD)
    }

    pub(in super::super) fn dim() -> Style {
        tui::muted()
    }

    pub(in super::super) fn bold() -> Style {
        tui::title()
    }

    pub(in super::super) fn ok() -> Style {
        tui::ok()
    }

    pub(in super::super) fn warn() -> Style {
        tui::warning()
    }

    pub(in super::super) fn fail() -> Style {
        tui::error()
    }
}

use pal::{dim, ok};

pub(super) fn span(text: impl Into<String>, style: Style) -> Span<'static> {
    Span::styled(text.into(), style)
}

pub(super) fn clean(text: &str) -> String {
    text.chars().filter(|c| !c.is_control()).collect()
}

pub(super) fn fit(text: &str, width: usize) -> String {
    let text = clean(text);
    if text.chars().count() <= width {
        return text;
    }
    let mut cut: String = text.chars().take(width.saturating_sub(1)).collect();
    cut.push('\u{2026}');
    cut
}

pub(super) fn spans_width(spans: &[Span<'_>]) -> usize {
    spans.iter().map(|s| s.content.width()).sum()
}

/// Where a status sits on the tree's colour scale.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Mark {
    Running,
    Done,
    Failed,
    Waiting,
    Queued,
    Idle,
    Unknown,
}

impl Mark {
    pub(super) fn of(status: &str) -> Self {
        let s = status.to_ascii_lowercase();
        let has = |words: &[&str]| words.iter().any(|w| s.contains(w));
        if s == "idle" {
            Self::Idle
        } else if has(&["fail", "error", "block", "crash", "abort", "cancel"]) {
            Self::Failed
        } else if has(&["queue", "pend", "wait"]) {
            Self::Queued
        } else if has(&["done", "complete", "end", "closed"]) || s == "stopped" {
            Self::Done
        } else if has(&["run", "work", "live", "active", "open"]) {
            Self::Running
        } else {
            Self::Unknown
        }
    }

    pub(super) fn glyph(self) -> &'static str {
        match self {
            Self::Running => "\u{25d0}",
            Self::Done => "\u{2713}",
            Self::Failed => "\u{2717}",
            Self::Waiting => "\u{2691}",
            Self::Queued | Self::Idle => "\u{25cc}",
            Self::Unknown => "\u{25cb}",
        }
    }

    pub(super) fn style(self) -> Style {
        match self {
            Self::Running => pal::fg(pal::AGENT),
            Self::Done => ok(),
            Self::Failed => pal::fail(),
            Self::Waiting => pal::warn(),
            Self::Queued | Self::Idle | Self::Unknown => dim(),
        }
    }
}

fn node_mark(model: &Model, node: &Node) -> Mark {
    if model.waiting(node) {
        Mark::Waiting
    } else {
        Mark::of(&node.status)
    }
}

fn effort_pips(effort: &str) -> usize {
    match effort.to_ascii_lowercase().as_str() {
        "minimal" | "none" | "low" => 1,
        "high" => 3,
        "xhigh" | "max" => 4,
        _ => 2,
    }
}

/// `▮▮▯▯ med` in `color`, or a dim `—` when zirv does not know it.
pub(super) fn effort_value(effort: Option<&str>, color: Color) -> Vec<Span<'static>> {
    let Some(effort) = effort.filter(|e| !e.is_empty()) else {
        return vec![span("\u{2014}", dim())];
    };
    let filled = effort_pips(effort);
    let short = if effort.eq_ignore_ascii_case("medium") {
        "med"
    } else {
        effort
    };
    vec![
        span("\u{25ae}".repeat(filled), pal::fg(color)),
        span("\u{25af}".repeat(4 - filled), dim()),
        span(format!(" {}", clean(short)), pal::strong(color)),
    ]
}

/// `effort ▮▮▯▯ med` in `color`, or a dim `effort —` when zirv does not know it.
pub(super) fn effort_spans(effort: Option<&str>, color: Color) -> Vec<Span<'static>> {
    let mut spans = vec![span("effort ", dim())];
    spans.extend(effort_value(effort, color));
    spans
}

/// The effort a node itself was launched with; never an ancestor's.
pub(super) fn own_effort(node: &Node) -> Option<&str> {
    node.effort.as_deref().filter(|e| !e.is_empty())
}

pub(super) fn tokens_label(tokens: u64) -> String {
    if tokens >= 1_000_000 {
        format!("{:.1}M tok", tokens as f64 / 1_000_000.0)
    } else if tokens >= 1_000 {
        format!("{}k tok", tokens / 1_000)
    } else {
        format!("{tokens} tok")
    }
}

fn read_label(tokens: u64) -> String {
    tokens_label(tokens).trim_end_matches(" tok").to_string()
}

pub(super) fn elapsed_label(secs: u64) -> String {
    match secs {
        0..=59 => format!("{secs}s"),
        60..=3599 => format!("{}m", secs / 60),
        _ => format!("{}h{}m", secs / 3600, secs % 3600 / 60),
    }
}

pub(super) fn node_elapsed(node: &Node, now: u64) -> Option<String> {
    let start = node.started_at?;
    let end = node.ended_at.unwrap_or(now);
    Some(elapsed_label(end.saturating_sub(start)))
}

pub(super) fn node_title(node: &Node) -> String {
    node.name
        .as_deref()
        .or(node.role.as_deref())
        .or(node.label.as_deref())
        .unwrap_or(node.kind.as_str())
        .to_string()
}

/// What the agent was asked to do, when zirv recorded it. The one place a job is read from a node.
pub(super) fn node_job(node: &Node) -> Option<String> {
    node.job.clone().filter(|j| !j.is_empty())
}

/// The workflow step the agent was dispatched under (`<pack> · <step>`). The one place a step
/// is read from a node.
pub(super) fn node_step(node: &Node) -> Option<String> {
    node.workflow
        .as_ref()
        .map(|w| format!("{} \u{b7} {}", w.pack, w.step))
}

/// One thing an agent did: when, which tool and its argument.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct StepView {
    pub(super) ts: u64,
    pub(super) tool: String,
    pub(super) arg: String,
}

/// What the agent has done so far, oldest first: the graph's own `Node.steps`, read here and
/// nowhere else.
pub(super) fn node_steps(node: &Node) -> Vec<StepView> {
    node.steps
        .iter()
        .map(|s| StepView {
            ts: s.ts,
            tool: s.tool.clone(),
            arg: s.arg.clone(),
        })
        .collect()
}

/// A step in words: `run cargo build`, `read src/x.rs`.
pub(super) fn step_label(step: &StepView) -> String {
    let verb = match step.tool.as_str() {
        "Bash" => "run",
        "Read" => "read",
        "Edit" => "edit",
        "Write" => "write",
        "Grep" => "search",
        "Glob" => "find",
        "SubagentHandback" => return "reported back to the seat".to_string(),
        other => {
            return format!("{} {}", other.to_lowercase(), step.arg)
                .trim()
                .to_string();
        }
    };
    format!("{verb} {}", step.arg)
}

/// One Jev decision as the Jev box and its panel show it.
#[derive(Debug, Clone, PartialEq)]
pub(in super::super) struct JevRow {
    /// Identity of the decision, for noticing a new one.
    pub(in super::super) id: String,
    pub(in super::super) ts: u64,
    pub(in super::super) site: String,
    /// What it decided, in plain words.
    pub(in super::super) text: String,
    pub(in super::super) confidence: f64,
    pub(in super::super) sure: bool,
    pub(in super::super) cached: bool,
}

/// What Jev has been doing, and whether it is on.
#[derive(Debug, Clone, Default, PartialEq)]
pub(in super::super) struct JevFeed {
    /// Oldest first.
    pub(in super::super) rows: Vec<JevRow>,
    /// The enabled `[jev]` sites.
    pub(in super::super) sites: Vec<String>,
    /// The harness proxy is on and asks TypeSafe.
    pub(in super::super) proxy: bool,
}

impl JevFeed {
    /// Jev is off only when no `[jev]` site is on and the harness proxy does not use TypeSafe.
    pub(in super::super) fn on(&self) -> bool {
        !self.sites.is_empty() || self.proxy
    }
}

/// The feed the background gather read (`jev_feed::jev_feed`), as the view holds it.
pub(super) fn jev_feed(data: &TreeData) -> JevFeed {
    data.jev.clone()
}

/// The gather's own feed, turned into the view's rows (oldest first).
pub(super) fn jev_feed_of(feed: super::super::super::jev_feed::JevFeed, proxy: bool) -> JevFeed {
    let rows = feed
        .decisions
        .into_iter()
        .rev()
        .map(|d| JevRow {
            id: format!("jev|{}|{}|{}", d.ts, d.site, d.text),
            ts: d.ts,
            site: clean(&d.site),
            text: clean(&d.text),
            confidence: d.confidence,
            sure: d.sure,
            cached: d.cached,
        })
        .collect();
    JevFeed {
        rows,
        sites: feed.enabled.into_iter().map(str::to_string).collect(),
        proxy,
    }
}

/// A pending request as the approval card shows it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(in super::super) struct ApprovalView {
    /// The whole command, tool included when it is not a shell command.
    pub(in super::super) command: String,
    /// Where it would run; empty when not known.
    pub(in super::super) cwd: String,
    /// Why it needs the operator; empty when not known.
    pub(in super::super) reason: String,
    pub(in super::super) outside_sandbox: bool,
    /// The request offers "always allow", with the label of the rule it would add; empty when it
    /// does not name one.
    pub(in super::super) always: Option<String>,
}

/// What the approval card shows of a request, read here and nowhere else.
pub(in super::super) fn approval_view(
    request: &crate::commands::ctx::approvals::Request,
) -> ApprovalView {
    let command = if request.command.is_empty() {
        format!("{} {}", request.tool, request.preview)
    } else {
        request.command.clone()
    };
    ApprovalView {
        command: command.trim().to_string(),
        cwd: request.cwd.clone().unwrap_or_default(),
        reason: request.reason.clone().unwrap_or_default(),
        outside_sandbox: request.outside_sandbox,
        always: request.always.clone(),
    }
}

pub(super) fn node_model(node: &Node) -> String {
    // A Claude model id from a transcript (`claude-sonnet-5-5`) reads as its family, like a hook's `sonnet`.
    let model = node
        .model
        .as_deref()
        .map(|m| super::super::super::catalogue::model_family("anthropic", m).unwrap_or(m));
    [node.harness.as_deref(), model]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>()
        .join(" ")
}

/// The node's model, plus a short badge when that model is avoided.
pub(super) fn node_model_badged(data: &TreeData, node: &Node) -> String {
    let label = node_model(node);
    let avoided = node.model.as_deref().is_some_and(|model| {
        data.avoided
            .contains(&super::super::super::catalogue::normalize_id(model).to_lowercase())
    });
    if avoided {
        format!("{label} [avoid]")
    } else {
        label
    }
}

pub(super) fn seat_title(facts: &TreeFacts) -> String {
    [facts.seat_harness, facts.seat_model]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>()
        .join(" ")
}

pub(super) fn jev_calls(facts: &TreeFacts) -> Option<u64> {
    match facts.jev {
        Some(JevSectionFact::Active { calls, .. }) => Some(*calls),
        _ => None,
    }
}

pub(super) fn spawn_label(facts: &TreeFacts) -> String {
    let mut label = format!(
        "spawn agents \u{b7} {} of {} slots",
        facts.panes_used, facts.max_panes
    );
    if facts.max_writers > 0 {
        label.push_str(&format!(" \u{b7} writers cap {}", facts.max_writers));
    }
    label
}

/// `supervisor on 1/3`, or `off`.
pub(super) fn supervisor_status(data: &TreeData) -> String {
    data.supervisor.as_ref().map_or("off".to_string(), |a| {
        format!("on {}/{}", a.calls, a.max_calls)
    })
}

// -- Footer -----------------------------------------------------------------

/// The statusline's left side, in display order with the priority that keeps it.
pub(super) fn footer_segments(data: &TreeData, facts: &TreeFacts) -> Vec<(u8, String)> {
    let jev = jev_calls(facts).map_or("off".to_string(), |c| c.to_string());
    let mut segments = vec![
        (
            1,
            format!("agents [{}/{}]", facts.panes_used, facts.max_panes),
        ),
        (2, format!("supervisor [{}]", supervisor_status(data))),
        (3, format!("jev [{jev}]")),
    ];
    if facts.approvals > 0 {
        segments.push((0, format!("\u{2691} approvals [{}]", facts.approvals)));
    }
    segments
}

#[cfg(test)]
pub(super) fn footer_label(data: &TreeData, facts: &TreeFacts) -> String {
    let joined = footer_segments(data, facts)
        .into_iter()
        .map(|(_, text)| text)
        .collect::<Vec<_>>()
        .join("   ");
    format!(" {joined}")
}

/// What clicking a footer hint does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Click {
    None,
    Key(KeyCode),
    Leave,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Hint {
    /// Lower is kept longer when the footer is short.
    pub(super) prio: u8,
    pub(super) key: &'static str,
    pub(super) label: &'static str,
    pub(super) click: Click,
}

impl Hint {
    pub(super) fn width(&self) -> usize {
        self.key.width() + 1 + self.label.width()
    }
}

/// The keys that apply to the selection, in display order.
pub(super) fn hints(model: &Model, sel: &Sel) -> Vec<Hint> {
    let node = model.node(sel);
    let pane = model.selected_pane(sel);
    let waiting = pane
        .as_deref()
        .is_some_and(|short| model.facts.approval_shorts.iter().any(|s| s == short));
    let mailable = node.is_some_and(|n| n.harness.is_some() && n.kind != "supervisor");
    let mut out = vec![Hint {
        prio: 4,
        key: "\u{2190}\u{2192}",
        label: "select",
        click: Click::None,
    }];
    out.push(Hint {
        prio: 8,
        key: "\u{2191}\u{2193}",
        label: "level",
        click: Click::None,
    });
    if pane.is_some() {
        out.push(Hint {
            prio: 2,
            key: "\u{23ce}",
            label: "open",
            click: Click::Key(KeyCode::Enter),
        });
    }
    if waiting {
        out.push(Hint {
            prio: 3,
            key: "y/d",
            label: "answer",
            click: Click::None,
        });
    }
    if mailable {
        out.push(Hint {
            prio: 7,
            key: "m",
            label: "mail",
            click: Click::Key(KeyCode::Char('m')),
        });
    }
    out.push(Hint {
        prio: 6,
        key: "s",
        label: "scope",
        click: Click::Key(KeyCode::Char('s')),
    });
    out.push(Hint {
        prio: 1,
        key: "?",
        label: "keys",
        click: Click::Key(KeyCode::Char('?')),
    });
    out.push(Hint {
        prio: 0,
        key: "^A t",
        label: "dashboard",
        click: Click::Leave,
    });
    out
}

const HINT_GAP: usize = 2;

pub(super) fn hints_width(hints: &[Hint]) -> usize {
    hints.iter().map(Hint::width).sum::<usize>() + HINT_GAP * hints.len().saturating_sub(1)
}

/// The most important hints that fit `budget` whole, still in display order. A hint is
/// either drawn complete or not at all.
pub(super) fn fit_hints(all: &[Hint], budget: usize) -> Vec<Hint> {
    let mut by_prio: Vec<&Hint> = all.iter().collect();
    by_prio.sort_by_key(|h| h.prio);
    let mut kept: Vec<Hint> = Vec::new();
    for hint in by_prio {
        let mut trial: Vec<Hint> = kept.clone();
        trial.push(*hint);
        if hints_width(&trial) <= budget {
            kept = trial;
        }
    }
    all.iter().filter(|h| kept.contains(h)).copied().collect()
}

/// A footer hint with its column in the row.
#[derive(Debug, Clone, Copy)]
pub(super) struct PlacedHint {
    pub(super) x: u16,
    pub(super) hint: Hint,
}

#[derive(Debug, Clone)]
pub(super) struct Footer {
    pub(super) left: String,
    pub(super) hints: Vec<PlacedHint>,
}

/// Lay the statusline out for `width` columns: hints are never cut, the left status gives way.
pub(super) fn footer(model: &Model, sel: &Sel, width: u16) -> Footer {
    let width = width as usize;
    let segments = footer_segments(model.data, model.facts);
    let keep_left = segments
        .iter()
        .filter(|(prio, _)| *prio <= 1)
        .map(|(_, text)| text.width() + 3)
        .sum::<usize>()
        + 1;
    let all = hints(model, sel);
    let essential = all.iter().find(|h| h.prio == 0).map_or(0, Hint::width);
    let budget = width
        .saturating_sub(2 + keep_left)
        .max(essential)
        .min(width.saturating_sub(1));
    let shown = fit_hints(&all, budget);
    let used = hints_width(&shown);
    let mut x = width.saturating_sub(used + 1);
    let mut placed = Vec::new();
    for hint in shown {
        placed.push(PlacedHint { x: x as u16, hint });
        x += hint.width() + HINT_GAP;
    }
    let left_room = width.saturating_sub(used + 3);
    let mut kept: Vec<(u8, String)> = Vec::new();
    let mut ordered: Vec<(u8, String)> = segments.clone();
    ordered.sort_by_key(|(prio, _)| *prio);
    for (prio, text) in ordered {
        let mut trial = kept.clone();
        trial.push((prio, text));
        let len: usize = 1
            + trial.iter().map(|(_, t)| t.width()).sum::<usize>()
            + 3 * trial.len().saturating_sub(1);
        if len <= left_room {
            kept = trial;
        }
    }
    let left = segments
        .iter()
        .filter(|seg| kept.contains(seg))
        .map(|(_, text)| text.clone())
        .collect::<Vec<_>>()
        .join("   ");
    Footer {
        left: if left.is_empty() {
            String::new()
        } else {
            format!(" {left}")
        },
        hints: placed,
    }
}

// -- Cards ------------------------------------------------------------------

/// One row inside a card, centred unless `left`.
#[derive(Debug, Clone)]
pub(super) struct Row {
    pub(super) spans: Vec<Span<'static>>,
    pub(super) left: bool,
    pub(super) sel: Option<Sel>,
}

impl Row {
    fn centered(spans: Vec<Span<'static>>) -> Self {
        Self {
            spans,
            left: false,
            sel: None,
        }
    }
}

pub(super) fn seat_rows(model: &Model, count: usize, inner: usize) -> Vec<Row> {
    let facts = model.facts;
    let role = facts.seat_role.unwrap_or("seat");
    let title = vec![
        span(fit(&seat_title(facts), inner), pal::strong(pal::SEAT)),
        span(" \u{b7} ", dim()),
        span(clean(role), pal::strong(pal::SEAT)),
    ];
    let effort = model.seat.and_then(own_effort);
    let mut priced: Vec<Span<'static>> = Vec::new();
    if effort.is_some() {
        priced.extend(effort_spans(effort, pal::SEAT));
    }
    let tokens = model.seat.and_then(|n| n.tokens).map(tokens_label);
    let rot = facts
        .rot
        .map(|r| ("rot ", format!("{:.2}", r as f64 / 100.0), ""));
    let tokens = tokens.map(|t| ("", t, ""));
    // The widest wording that fits the card; the separator shrinks before a number is dropped.
    let options = [vec![rot.clone(), tokens], vec![rot]];
    let mut usage: Vec<Span<'static>> = Vec::new();
    'fit: for sep in ["  \u{b7}  ", " \u{b7} "] {
        for option in &options {
            let mut trial: Vec<Span<'static>> = Vec::new();
            for (label, value, tail) in option.iter().flatten() {
                if !trial.is_empty() {
                    trial.push(span(sep, dim()));
                }
                trial.push(span(*label, dim()));
                trial.push(span(value.clone(), Style::default()));
                trial.push(span(*tail, dim()));
            }
            if trial.is_empty() {
                continue;
            }
            let fits = spans_width(&trial) <= inner;
            if usage.is_empty() || fits {
                usage = trial;
            }
            if fits {
                break 'fit;
            }
        }
    }
    // Title first; with room for one more line the watched numbers beat the price line.
    let mut rows = vec![Row::centered(title)];
    let room = count.saturating_sub(1);
    let show_usage = !usage.is_empty() && room >= 1;
    let show_priced = !priced.is_empty() && room > usize::from(show_usage);
    if show_priced {
        rows.push(Row::centered(priced));
    }
    if show_usage {
        rows.push(Row::centered(usage));
    }
    rows
}

/// How much of a box to draw: `core` 4 shows effort, 3 drops it; `detail` adds the job and
/// token lines; `kid_cap` is the most child lines.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct BoxSpec {
    pub(super) core: usize,
    pub(super) detail: u8,
    pub(super) kid_cap: usize,
}

/// Which child lines show: `(first, shown)`; the rest fold into `+N more`. The cap counts the
/// `+N more` line, and a selected child is always among the shown ones.
fn kid_window(len: usize, cap: usize, selected: Option<usize>) -> (usize, usize) {
    if len <= cap {
        return (0, len);
    }
    let shown = cap.saturating_sub(1);
    match (shown, selected) {
        (0, Some(at)) => (at, 1),
        (0, None) => (0, 0),
        (_, at) => {
            let first = at.map_or(0, |at| (at + 1).saturating_sub(shown));
            (first.min(len - shown), shown)
        }
    }
}

fn kid_row(kid: &Node, inner: usize, selected: bool, model: &Model) -> Row {
    let mark = node_mark(model, kid);
    let glyph = format!(" {}", mark.glyph());
    let room = inner.saturating_sub(2 + glyph.width());
    let title = fit(&node_title(kid), room);
    let spare = room.saturating_sub(title.width());
    let mut spans = vec![span("\u{2514} ", dim()), span(title, Style::default())];
    let model_name = kid.model.clone().unwrap_or_default();
    if !model_name.is_empty() && spare > model_name.width() {
        spans.push(span(format!(" {model_name}"), dim()));
    }
    spans.push(span(glyph, mark.style()));
    Row {
        spans,
        left: true,
        sel: Some(Sel::Child(kid.id.clone())),
    }
    .selected(selected)
}

impl Row {
    fn selected(mut self, selected: bool) -> Self {
        if selected {
            for s in &mut self.spans {
                s.style = s.style.bg(pal::SELECTED_BG);
            }
        }
        self
    }
}

/// The title cut at a word to fit `room` columns, ending in `…`; `None` when not even one word fits.
pub(super) fn title_fit(title: &str, room: usize) -> Option<String> {
    if title.is_empty() || room == 0 {
        return None;
    }
    if title.width() <= room {
        return Some(title.to_string());
    }
    let words: Vec<&str> = title.split_whitespace().collect();
    (1..=words.len()).rev().find_map(|k| {
        let head = words[..k]
            .join(" ")
            .trim_end_matches([',', ';', ':', '-', '(', '\u{2013}'])
            .to_string();
        (head.width() < room).then(|| format!("{head}\u{2026}"))
    })
}

pub(super) fn agent_rows(
    model: &Model,
    agent: &Agent,
    spec: BoxSpec,
    selected: &Sel,
    inner: usize,
) -> Vec<Row> {
    let node = agent.node;
    let data = model.data;
    let counts = data.mail_counts.get(&sessions::short_id(&node.id));
    // The job names the card; the agent type, which most native agents share, goes under it.
    let job = node_job(node);
    let badge = if counts.is_some_and(|c| c.recent > 0) {
        4
    } else {
        0
    };
    let room = inner.saturating_sub(badge);
    let heading = job
        .as_deref()
        .map(|j| title_fit(j, room).unwrap_or_else(|| fit(j, room)));
    let mut title = vec![span(
        fit(heading.as_deref().unwrap_or(&node_title(node)), room),
        pal::bold(),
    )];
    if let Some(count) = counts.filter(|c| c.recent > 0) {
        let style = if count.unread > 0 { pal::warn() } else { dim() };
        title.push(span(format!(" \u{2709}{}", count.recent), style));
    }
    let mark = node_mark(model, node);
    let mut state = if mark == Mark::Waiting {
        "approval".to_string()
    } else {
        clean(&node.status)
    };
    if let Some(elapsed) = node_elapsed(node, model.facts.now) {
        state.push(' ');
        state.push_str(&elapsed);
    }
    let tokens = node.tokens.map(tokens_label);
    let has_kids = !agent.kids.is_empty();
    let mut rows = vec![
        Row::centered(title),
        Row::centered(vec![span(
            fit(&node_model_badged(data, node), inner),
            pal::fg(pal::AGENT),
        )]),
    ];
    if spec.core >= 4 {
        rows.push(Row::centered(effort_spans(own_effort(node), pal::AGENT)));
    }
    if spec.detail >= 1 {
        rows.push(Row::centered(vec![match heading {
            Some(_) => span(fit(&node_title(node), inner), dim()),
            None => span("\u{2014}", dim()),
        }]));
    }
    rows.push(Row::centered(vec![
        span(format!("{} ", mark.glyph()), mark.style()),
        span(fit(&state, inner.saturating_sub(2)), mark.style()),
    ]));
    let wants_tokens = spec.detail >= 2;
    if wants_tokens
        && !has_kids
        && let Some(tokens) = tokens
    {
        rows.push(Row::centered(vec![span(fit(&tokens, inner), dim())]));
    }
    if has_kids && spec.kid_cap > 0 {
        let at = match selected {
            Sel::Child(id) => agent.kids.iter().position(|k| k.id == *id),
            _ => None,
        };
        let (first, shown) = kid_window(agent.kids.len(), spec.kid_cap, at);
        for (offset, kid) in agent.kids.iter().skip(first).take(shown).enumerate() {
            let is_selected = at == Some(first + offset);
            rows.push(kid_row(kid, inner, is_selected, model));
        }
        let more = agent.kids.len() - shown;
        if more > 0 {
            let text = if shown == 0 {
                format!("\u{2514} {more} more")
            } else {
                format!("+{more} more")
            };
            rows.push(Row {
                spans: vec![span(text, dim())],
                left: true,
                sel: None,
            });
        }
    }
    rows
}

pub(super) fn jev_title(facts: &TreeFacts) -> String {
    match jev_calls(facts) {
        Some(_) => "JEV \u{b7} decisions".to_string(),
        None => "JEV".to_string(),
    }
}

pub(super) fn jev_site_count(facts: &TreeFacts) -> usize {
    match facts.jev {
        Some(JevSectionFact::Active { sites, .. }) => sites.len(),
        _ => 0,
    }
}

/// One row per site (at most `cap`), or the single dim line when Jev is off or silent.
pub(super) fn jev_rows(model: &Model, cap: usize, inner: usize) -> Vec<Row> {
    let Some(JevSectionFact::Active { sites, .. }) = model.facts.jev else {
        return vec![Row::centered(vec![span("off", dim())])];
    };
    if sites.is_empty() {
        return vec![Row::centered(vec![span("no calls yet", dim())])];
    }
    let shown = &sites[..sites.len().min(cap.max(1))];
    let widest = shown.iter().map(|s| s.name.width()).max().unwrap_or(0);
    let name_w = widest.min(inner.saturating_sub(12 + 4)).clamp(4, 18);
    let bar_w = inner.saturating_sub(name_w + 12).clamp(4, 14);
    shown
        .iter()
        .map(|site| {
            let name = fit(&site.name, name_w);
            let pad = " ".repeat(name_w.saturating_sub(name.width()));
            let mut spans = vec![span(format!("{name}{pad} "), Style::default())];
            match model.data.jev_verdicts.get(&site.name) {
                Some((p, sharp)) => {
                    let color = if *sharp { pal::JEV } else { Color::Yellow };
                    let filled = ((p.clamp(0.0, 1.0)) * bar_w as f64).round() as usize;
                    spans.push(span("\u{2588}".repeat(filled), pal::fg(color)));
                    spans.push(span("\u{2591}".repeat(bar_w - filled), dim()));
                    spans.push(span(
                        format!(" {p:.2} "),
                        Style::default().add_modifier(Modifier::BOLD),
                    ));
                    spans.push(span(
                        if *sharp { "sharp" } else { "split" },
                        pal::strong(color),
                    ));
                }
                None => {
                    spans.push(span("\u{2591}".repeat(bar_w), dim()));
                    spans.push(span(format!(" {:>4} ", "\u{2014}"), dim()));
                    spans.push(span(format!("{} calls", site.calls), dim()));
                }
            }
            Row {
                spans,
                left: true,
                sel: None,
            }
        })
        .collect()
}

pub(super) fn jev_legend() -> Row {
    Row::centered(vec![
        span("sharp", pal::strong(pal::JEV)),
        span(" \u{2192} runs in code        ", dim()),
        span("split", pal::strong(Color::Yellow)),
        span(" \u{2192} seat decides", dim()),
    ])
}

/// The "back to seat" box: the active workflow and where it stands.
pub(super) fn back_rows(model: &Model, count: usize, inner: usize) -> Vec<Row> {
    let Some(wf) = model.facts.workflow else {
        return Vec::new();
    };
    let mut head = vec![
        span("workflow ", dim()),
        span(
            fit(&wf.pack, inner.saturating_sub(12) / 2),
            Style::default(),
        ),
        span(" \u{b7} ", dim()),
        span(fit(&wf.step, inner.saturating_sub(12) / 2), pal::bold()),
    ];
    if wf.awaiting_approval {
        head.push(span(" \u{2691} approval", pal::warn()));
    }
    let mut rows = vec![Row::centered(head)];
    if count < 2 {
        return rows;
    }
    // Show the run from just before the current step, as many marks as fit.
    let current = wf
        .steps
        .iter()
        .position(|(_, m)| *m == StepMark::Current)
        .unwrap_or(0);
    let mark = |m: StepMark| match m {
        StepMark::Done => ("\u{2713}", ok()),
        StepMark::Current => ("\u{25d0}", pal::fg(pal::SEAT)),
        StepMark::Pending => ("\u{25cc}", dim()),
    };
    let mut start = current.saturating_sub(1);
    let render = |from: usize| -> Vec<Span<'static>> {
        let mut spans = Vec::new();
        if from > 0 {
            spans.push(span("\u{2026} ", dim()));
        }
        for (i, (id, m)) in wf.steps.iter().enumerate().skip(from) {
            let (glyph, style) = mark(*m);
            if i > from {
                spans.push(span("  ", dim()));
            }
            spans.push(span(format!("{id} "), dim()));
            spans.push(span(glyph, style));
        }
        spans
    };
    let mut line = render(start);
    while spans_width(&line) > inner && start < current {
        start += 1;
        line = render(start);
    }
    rows.push(Row::centered(line));
    rows
}

// -- Session log ------------------------------------------------------------

/// A readable name for a session id the log carries.
pub(super) fn actor_name(model: &Model, raw: &str) -> String {
    let short = sessions::short_id(raw);
    if model
        .facts
        .seat_session
        .is_some_and(|seat| sessions::short_id(seat) == short)
    {
        return "seat".to_string();
    }
    if let Some(node) = model
        .data
        .nodes
        .iter()
        .find(|n| n.id == raw || (!short.is_empty() && sessions::short_id(&n.id) == short))
    {
        return node_title(node);
    }
    if raw.len() >= 8 && raw.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') {
        return short;
    }
    clean(raw)
}

pub(super) fn actor_color(name: &str, kind: &str) -> Color {
    match (name, kind) {
        ("jev" | "proxy", _) | (_, "jev") => pal::JEV,
        ("supervisor", _) => pal::ARCH,
        ("seat", _) => pal::SEAT,
        _ => pal::AGENT,
    }
}

/// `(actor, summary)` of an event as the log shows it.
fn event_text(model: &Model, event: &Event) -> (String, String) {
    if event.kind.starts_with("subagent") {
        let mut words = event.summary.splitn(2, ' ');
        let kind = words.next().unwrap_or_default().to_string();
        let rest = words.next().unwrap_or_default();
        // "Explore agent-id [status]": the agent id is noise next to the type.
        let rest = rest.split_once(' ').map_or("", |(_, tail)| tail);
        let verb = if event.kind == "subagent_start" {
            "started"
        } else {
            rest
        };
        return (kind, verb.to_string());
    }
    let actor = actor_name(model, &event.actor);
    match (&event.kind[..], &event.to) {
        ("mail", Some(to)) => {
            let to = actor_name(model, to);
            (actor, format!("\u{2709} \u{2192} {to}  {}", event.summary))
        }
        _ => (actor, event.summary.clone()),
    }
}

pub(super) fn log_rows(model: &Model, width: usize, count: usize) -> Vec<Vec<Span<'static>>> {
    let events = &model.data.events;
    events[events.len().saturating_sub(count)..]
        .iter()
        .map(|event| {
            let time = model
                .facts
                .utc_offset
                .timestamp_opt(event.ts as i64, 0)
                .single()
                .map_or("--:--:--".to_string(), |t| t.format("%H:%M:%S").to_string());
            let (actor, summary) = event_text(model, event);
            let color = actor_color(&actor, &event.kind);
            let mut tail = String::new();
            let mut verdict: Option<(&str, Color)> = None;
            if let Some(p) = event.p {
                tail = format!("  p={p:.2}");
                if event.kind == "jev" || event.kind == "decision" {
                    verdict = Some(
                        if p >= f64::from(super::super::super::jev::DEFAULT_MIN_MARGIN) {
                            ("sharp", pal::JEV)
                        } else {
                            ("split", Color::Yellow)
                        },
                    );
                }
            }
            let head = format!("{time}  ");
            let actor_cell = format!("{:<10} ", fit(&actor, 10));
            let verdict_w = verdict.map_or(0, |(w, _)| w.len() + 2);
            let room =
                width.saturating_sub(head.width() + actor_cell.width() + tail.width() + verdict_w);
            let mut spans = vec![
                span(head, dim()),
                span(actor_cell, pal::strong(color)),
                span(fit(&summary, room), Style::default()),
                span(tail, Style::default()),
            ];
            if let Some((word, color)) = verdict {
                spans.push(span(format!("  {word}"), pal::strong(color)));
            }
            spans
        })
        .collect()
}

// -- Supervisor --------------------------------------------------------------

pub(super) fn moment_label(moment: Moment) -> &'static str {
    match moment {
        Moment::BeforePlan => "before a plan",
        Moment::ErrorRepeats => "error repeats",
        Moment::BeforeDone => "before done",
    }
}

/// `codex gpt-6-astra · on call`, without the harness when the sidecar is narrow.
pub(super) fn supervisor_model_line(a: &SupervisorFact, width: usize) -> String {
    let state = if a.advising { "advising" } else { "on call" };
    let full = format!("{} {} \u{b7} {state}", a.harness, a.model);
    if full.width() <= width {
        full
    } else {
        fit(&format!("{} \u{b7} {state}", a.model), width)
    }
}

pub(super) fn supervisor_reads(a: &SupervisorFact) -> String {
    read_label(a.tokens_read)
}

pub(super) fn header_supervisor(data: &TreeData) -> Option<String> {
    data.supervisor
        .as_ref()
        .map(|a| format!("{} {}", a.harness, a.model).to_uppercase())
}

// -- Chat bar ---------------------------------------------------------------

/// `flow › worker · codex gpt-6-sol · effort ▮▮▯▯ med · ^A t back to flow`: the line above an
/// open chat, naming the focused pane. Pieces drop from the middle when the line is short.
pub(super) fn chat_bar(model: &Model, width: usize) -> Vec<Span<'static>> {
    let facts = model.facts;
    let focused = facts.focused.as_ref();
    let node = focused.and_then(|(short, _, _)| {
        model
            .data
            .nodes
            .iter()
            .find(|n| sessions::short_id(&n.id) == *short)
    });
    let is_seat = focused.is_some_and(|(short, _, _)| {
        facts
            .seat_session
            .is_some_and(|seat| sessions::short_id(seat) == *short)
    });
    let color = if is_seat { pal::SEAT } else { pal::AGENT };
    let title = node
        .map(node_title)
        .unwrap_or_else(|| focused.map_or("agent".to_string(), |(_, title, _)| clean(title)));
    let model_name = match (node, focused) {
        (Some(n), _) if !node_model(n).is_empty() => node_model(n),
        (_, Some((_, _, agent))) if is_seat => {
            let seat = seat_title(facts);
            if seat.is_empty() { clean(agent) } else { seat }
        }
        (_, Some((_, _, agent))) => clean(agent),
        _ => String::new(),
    };
    let effort = node
        .and_then(own_effort)
        .or_else(|| is_seat.then(|| model.seat.and_then(own_effort)).flatten());
    let head = vec![
        span("flow", dim()),
        span(" \u{203a} ", dim()),
        span(title, pal::strong(color)),
    ];
    let tail = vec![
        span(" \u{b7} ", dim()),
        span("^A t", pal::bold()),
        span(" back to flow", dim()),
    ];
    let model_part = vec![span(" \u{b7} ", dim()), span(model_name, pal::fg(color))];
    let mut effort_part = vec![span(" \u{b7} ", dim())];
    effort_part.extend(effort_spans(effort, color));
    let join = |parts: &[&Vec<Span<'static>>]| -> Vec<Span<'static>> {
        parts.iter().flat_map(|p| p.iter().cloned()).collect()
    };
    for candidate in [
        join(&[&head, &model_part, &effort_part, &tail]),
        join(&[&head, &model_part, &tail]),
        join(&[&head, &tail]),
    ] {
        if spans_width(&candidate) < width {
            return candidate;
        }
    }
    join(&[&head])
}
