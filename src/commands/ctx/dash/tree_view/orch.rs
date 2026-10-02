//! The orchestrator dashboard: the agent tree's top level at 100 columns and up. It is a
//! dashboard about the session, not a chat window: the header, the workflow stepper, the flow,
//! what needs the operator, the selected (or hovered) node, how the agents talk to each other and
//! a key bar. No chat is visible until the operator opens an agent's harness.
//!
//! [`build`] turns the data, the facts, the view state and the injected clock into a [`Scene`]
//! (cells plus the regions the mouse hits); [`paint`] copies it into the terminal buffer. Both read
//! only what the background gather and the dashboard's cached facts already hold, so a frame never
//! touches a file.

use chrono::TimeZone;
use crossterm::event::KeyCode;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;

use super::super::super::graph::{Event, Node};
use super::super::super::sessions;
use super::content::{self, Mark};
use super::flow::{self, St};
use super::model::{Model, Sel};
use super::scene::{Act, Chip, Ctx, Scene, ShownApproval, ShownRuling, cut, short_name, wrap};
use super::theme::{self, Rgb, breathe, c, mix};
use super::{ApprovalFact, TreeFacts, TreeView, WaitFact, WaitKind};
use crate::commands::workflow::StepMark;

/// 35 rows: the tightest flow profile plus the FINISHED strip needs 21 rows of box, and below that the cards overrun it.
const MIN_SIZE: (u16, u16) = (100, 35);
/// Rows above the flow: header, stepper and a blank row.
const TOP: i32 = 3;
/// Activity box height: a border, six rows, a border.
const ACT_H: i32 = 8;
const ACT_LINES: usize = 6;
/// Rows under the activity box: the key bar and its hint.
const KEY_ROWS: i32 = 2;
/// Most lines the approval card spends on the command.
const CMD_LINES: usize = 4;
/// Rows SELECTED keeps whatever else is above it.
const SELECTED_MIN: i32 = 9;

/// Whether a terminal this size gets the orchestrator dashboard; smaller ones draw the phase-1
/// tiers as before.
pub(super) fn fits(width: u16, height: u16) -> bool {
    width >= MIN_SIZE.0 && height >= MIN_SIZE.1
}

/// Rows the open chat leaves to the dashboard: `(top, bottom)`. The chat keeps the header and
/// stepper above its bar, and the others strip and key bar below, only where the dashboard
/// itself would be drawn; smaller terminals keep the phase-1 one-line bar.
pub(super) fn chat_chrome(term: (u16, u16)) -> (u16, u16) {
    if fits(term.0, term.1) { (4, 3) } else { (1, 0) }
}

/// The flow's rectangle `(x, y, w, h)` and the right column's `(x, w)` in a `w` x `h` terminal.
fn layout(w: i32, h: i32) -> ((i32, i32, i32, i32), (i32, i32)) {
    let rw = (w * 31 / 100).clamp(36, 49);
    let rx = w - 1 - rw;
    let fh = h - TOP - 1 - ACT_H - KEY_ROWS;
    ((1, TOP, rx - 3, fh), (rx, rw))
}

// -- What needs the operator ----------------------------------------------------------------

fn name_of(model: &Model, sel: &Sel) -> String {
    match sel {
        Sel::Seat => "seat".to_string(),
        Sel::Jev => "jev".to_string(),
        _ => model
            .node(sel)
            .map_or_else(|| "agent".to_string(), content::node_title),
    }
}

/// The kind of thing that needs the operator.
#[derive(Debug, Clone, PartialEq, Eq)]
enum NeedKind {
    Approval(ApprovalFact),
    /// An open supervisor ruling the operator can override.
    Ruling(super::super::super::supervisor::Ruling),
    Failed {
        why: Option<String>,
    },
    Wait {
        kind: WaitKind,
        evidence: String,
    },
    Stalled,
}

#[derive(Debug, Clone)]
struct Need {
    kind: NeedKind,
    sel: Option<Sel>,
    name: String,
    age: u64,
}

fn wait_verb(kind: WaitKind) -> &'static str {
    match kind {
        WaitKind::Question => "asks you",
        WaitKind::Permission => "needs permission",
        WaitKind::Approval => "waits for approval",
        WaitKind::WorkflowGate => "waits at a gate",
    }
}

/// Everything that needs the operator, oldest first.
fn needs(model: &Model) -> Vec<Need> {
    let facts = model.facts;
    let now = facts.now;
    let mut out: Vec<Need> = Vec::new();
    let subject = |short: &str| -> (Option<Sel>, String) {
        let sel = model.sel_for_short(short);
        let name = sel
            .as_ref()
            .map_or_else(|| short.to_string(), |s| name_of(model, s));
        (sel, name)
    };
    for fact in &facts.approval_items {
        let (sel, name) = subject(&fact.short);
        out.push(Need {
            kind: NeedKind::Approval(fact.clone()),
            sel,
            name,
            age: fact.waited_secs,
        });
    }
    for WaitFact {
        short,
        kind,
        since,
        evidence,
    } in &facts.waits
    {
        if facts.approval_items.iter().any(|a| a.short == *short) {
            continue;
        }
        let (sel, name) = subject(short);
        out.push(Need {
            kind: NeedKind::Wait {
                kind: *kind,
                evidence: evidence.clone(),
            },
            sel,
            name,
            age: now.saturating_sub(*since),
        });
    }
    for (short, since) in &facts.stalled {
        let (sel, name) = subject(short);
        out.push(Need {
            kind: NeedKind::Stalled,
            sel,
            name,
            age: if *since == 0 {
                0
            } else {
                now.saturating_sub(*since)
            },
        });
    }
    for ruling in &model.data.rulings {
        let (sel, name) = subject(&sessions::short_id(&ruling.session));
        if sel.is_none() && model.scope != super::model::Scope::All {
            continue;
        }
        out.push(Need {
            kind: NeedKind::Ruling(ruling.clone()),
            sel,
            name,
            age: now.saturating_sub(ruling.ts),
        });
    }
    for agent in &model.agents {
        let nodes = std::iter::once((Sel::Agent(agent.node.id.clone()), agent.node))
            .chain(agent.kids.iter().map(|k| (Sel::Child(k.id.clone()), *k)));
        for (sel, node) in nodes {
            if Mark::of(&node.status) != Mark::Failed {
                continue;
            }
            out.push(Need {
                kind: NeedKind::Failed {
                    why: content::node_job(node),
                },
                name: content::node_title(node),
                age: node.ended_at.map_or(0, |end| now.saturating_sub(end)),
                sel: Some(sel),
            });
        }
    }
    out.sort_by_key(|n| std::cmp::Reverse(n.age));
    out
}

fn clock(secs: u64) -> String {
    format!("{}:{:02}", secs / 60, secs % 60)
}

/// Hard-wrap `text` to `width` columns, at most `max` lines. The flag says it all fitted. The
/// command of a request is wrapped by column, never by word, so what is shown is what would run.
fn hard_wrap(text: &str, width: usize, max: usize) -> (Vec<String>, bool) {
    let mut lines: Vec<String> = Vec::new();
    let mut line = String::new();
    let mut used = 0usize;
    let mut complete = true;
    for ch in content::clean(text).chars() {
        let w = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0);
        if used + w > width {
            if lines.len() + 1 >= max {
                complete = false;
                break;
            }
            lines.push(std::mem::take(&mut line));
            used = 0;
        }
        line.push(ch);
        used += w;
    }
    if !line.is_empty() || lines.is_empty() {
        lines.push(line);
    }
    if !complete && let Some(last) = lines.last_mut() {
        *last = content::fit(&format!("{last}\u{2026}"), width);
    }
    (lines, complete)
}

/// The prefix "always allow" would cover: `python3`, or the first two words.
fn prefix_of(cmd: &str) -> String {
    if cmd.starts_with("python3") {
        return "python3".to_string();
    }
    cmd.split_whitespace().take(2).collect::<Vec<_>>().join(" ")
}

// -- Header and stepper ---------------------------------------------------------------------

fn header(s: &mut Scene, ctx: &Ctx, w: i32, need_count: usize) {
    let facts = ctx.model.facts;
    let g = &mut s.grid;
    g.fill(0, 0, w, 1, c::PANEL);
    let mut x = g.text_on(1, 0, " zirv ", c::BG, Some(c::SEAT), true);
    x = g.text(x + 1, 0, &cut(&facts.repo_name, 24), c::FG);
    x = g.text(x, 0, "  \u{b7}  ", c::FAINT);
    x = g.text(x, 0, "seat ", c::DIM);
    let seat = content::seat_title(facts);
    if !seat.is_empty() {
        x = g.bold(x, 0, &cut(&seat, 28), c::SEAT);
    }
    x = g.text(
        x,
        0,
        &format!(" \u{b7} {}", cut(facts.seat_role.unwrap_or("seat"), 16)),
        c::DIM,
    );
    let help = " ? keys ";
    let hw = theme::width(help);
    let help_x = w - 1 - hw;
    g.text_on(help_x, 0, help, c::DIM, Some(c::CHIP), false);
    let r = s.reg(help_x, 0, hw, 1, Some(Act::Help));
    r.hk = Some("help".into());
    // Right to left: the badge, then the facts that fit.
    let badge = if need_count > 0 {
        let b = format!(
            " \u{2691} {need_count} need{} you ",
            if need_count == 1 { "s" } else { "" }
        );
        (
            b,
            c::BG,
            mix(c::WARN_DIM, c::WARN, 0.55 + 0.45 * breathe(ctx.now, 1.6)),
        )
    } else {
        (" \u{2713} all clear ".to_string(), c::OK, c::OK_BG)
    };
    let usage = facts
        .usage_5h
        .map(|pct| ("usage 5h ".to_string(), format!("{pct:.0}%")));
    // Drop the facts that matter least until what is left fits beside the badge.
    let sep = "  \u{b7}  ";
    let mut parts: Vec<(String, String)> = [usage].into_iter().flatten().collect();
    let parts_w = |parts: &[(String, String)]| -> i32 {
        parts
            .iter()
            .map(|(k, v)| theme::width(sep) + theme::width(k) + theme::width(v))
            .sum()
    };
    let badge_w = theme::width(&badge.0);
    while !parts.is_empty() && x + badge_w + parts_w(&parts) + hw + 6 > w {
        parts.remove(0);
    }
    let mut px = help_x - 2 - badge_w - parts_w(&parts);
    px = s
        .grid
        .text_on(px, 0, &badge.0, badge.1, Some(badge.2), true);
    for (k, v) in &parts {
        px = s.grid.text(px, 0, sep, c::FAINT);
        px = s.grid.text(px, 0, k, c::DIM);
        px = s.grid.bold(px, 0, v, c::FG);
    }
}

fn stepper(s: &mut Scene, ctx: &Ctx, w: i32) {
    let y = 1;
    let g = &mut s.grid;
    let Some(wf) = ctx.model.facts.workflow else {
        let x = g.text(1, y, "No workflow running", c::DIM);
        g.text(x + 3, y, "start one with  zirv workflow start", c::FAINT);
        return;
    };
    let now = ctx.now;
    let current = wf
        .steps
        .iter()
        .position(|(_, m)| *m == StepMark::Current)
        .unwrap_or_else(|| wf.steps.len().saturating_sub(1));
    let elapsed = content::elapsed_label(ctx.model.facts.now.saturating_sub(wf.started_at));
    let right_text = |with_gate: bool| -> String {
        let mut t = format!("step {}/{} \u{b7} {elapsed}", current + 1, wf.steps.len());
        match (with_gate, wf.next_gate.as_ref()) {
            (true, Some(gate)) => t.push_str(&format!(" \u{b7} next gate {}", cut(gate, 14))),
            (true, None) => {
                if let Some((next, _)) = wf.steps.get(current + 1) {
                    t.push_str(&format!(" \u{b7} next: {}", cut(next, 14)));
                }
            }
            _ => {}
        }
        t
    };
    let step_w = |from: usize| -> i32 {
        let names: i32 = wf
            .steps
            .iter()
            .skip(from)
            .map(|(id, _)| 2 + theme::width(&cut(id, 16)))
            .sum();
        let seps = 5 * (wf.steps.len() - from).saturating_sub(1) as i32;
        names + seps + if from > 0 { 2 } else { 0 }
    };
    let head = format!("WORKFLOW {}", cut(&wf.pack, 18));
    let head_w = theme::width(&head);
    let mut chosen: Option<(usize, bool, String)> = None;
    'fit: for from in 0..=current {
        for with_gate in [true, false] {
            let right = right_text(with_gate);
            let room = w - 2 - head_w - 4 - step_w(from) - theme::width(&right) - 2;
            let title = content::title_fit(&wf.title, (room - 3).max(0) as usize);
            let title_w = title.as_ref().map_or(0, |t| theme::width(t) + 3);
            let fits = head_w + title_w + 4 + step_w(from) + theme::width(&right) + 2 <= w;
            chosen = Some((from, with_gate, title.unwrap_or_default()));
            if fits {
                break 'fit;
            }
        }
    }
    let Some((from, with_gate, title)) = chosen else {
        return;
    };
    let right = right_text(with_gate);
    let mut x = g.bold(1, y, "WORKFLOW ", c::FAINT);
    x = g.bold(x, y, &cut(&wf.pack, 18), c::SEAT);
    if !title.is_empty() {
        x = g.text(x, y, " \u{b7} ", c::FAINT);
        x = g.text(x, y, &title, c::FG);
    }
    x += 4;
    if from > 0 {
        x = g.text(x, y, "\u{2026} ", c::DIM);
    }
    let shimmer = (now / 60 % 60) as f64;
    for (i, (id, mark)) in wf.steps.iter().enumerate().skip(from) {
        if i > from {
            x += 1;
            for k in 0..3 {
                let filled = i <= current;
                let glow = filled && ((((x + k) % 60) as f64) - shimmer).abs() < 2.0;
                let (ch, fg) = match (filled, glow) {
                    (true, true) => ('\u{2501}', c::HI),
                    (true, false) => ('\u{2501}', c::OK),
                    _ => ('\u{2500}', c::RULE),
                };
                g.put(x + k, y, ch, Some(fg), None, false);
            }
            x += 4;
        }
        let (glyph, gc) = match mark {
            StepMark::Done => ('\u{2713}', c::OK),
            StepMark::Current => (
                ['\u{25d0}', '\u{25d3}', '\u{25d1}', '\u{25d2}'][(now / 250 % 4) as usize],
                c::AGENT,
            ),
            StepMark::Pending => ('\u{25cb}', c::FAINT),
        };
        g.put(x, y, glyph, Some(gc), None, true);
        let (fg, bold) = match mark {
            StepMark::Current => (c::HI, true),
            StepMark::Done => (c::FG, false),
            StepMark::Pending => (c::FAINT, false),
        };
        x = g.text_on(x + 2, y, &cut(id, 16), fg, None, bold);
    }
    g.text(w - 1 - theme::width(&right), y, &right, c::DIM);
}

// -- NEEDS YOU ------------------------------------------------------------------------------

/// One request, as the prototype's approval card: who asks, the whole command, why, four answers.
/// Returns the card's height.
fn approval_card(
    s: &mut Scene,
    ctx: &Ctx,
    fact: &ApprovalFact,
    count: usize,
    (x, y, w): (i32, i32, i32),
    max_h: i32,
) -> i32 {
    let model = ctx.model;
    let iw = (w - 4) as usize;
    let sel = model.sel_for_short(&fact.short);
    let node = sel.as_ref().and_then(|sel| model.node(sel));
    let who = node.map_or_else(|| fact.short.clone(), |n| model.job_of(n));
    let view = &fact.view;
    let mut who_lines = wrap(&who, iw - 2, Some(2)).0;
    let (mut cmd, _) = hard_wrap(&view.command, iw - 2, CMD_LINES);
    let reason = if !view.reason.is_empty() {
        view.reason.clone()
    } else if fact.released {
        "This request is waiting in its harness; answer it there.".to_string()
    } else if !fact.fully_shown {
        "Only part of the command is shown here. Open its harness to answer.".to_string()
    } else {
        String::new()
    };
    let mut why = if reason.is_empty() {
        Vec::new()
    } else {
        wrap(&reason, iw, Some(4)).0
    };
    let height = |who: usize, cmd: usize, why: usize| -> i32 {
        (who + cmd + 8 + if why > 0 { why + 1 } else { 0 }) as i32
    };
    while height(who_lines.len(), cmd.len(), why.len()) > max_h && !why.is_empty() {
        why.pop();
    }
    while height(who_lines.len(), cmd.len(), why.len()) > max_h && who_lines.len() > 1 {
        who_lines.pop();
    }
    let room = (max_h - height(who_lines.len(), 0, why.len())).max(1) as usize;
    let (cut_cmd, complete) = hard_wrap(&view.command, iw - 2, CMD_LINES.min(room));
    if cut_cmd.len() < cmd.len() || !complete {
        cmd = cut_cmd;
    }
    let (_, complete) = hard_wrap(&view.command, iw - 2, cmd.len().max(1));
    let answerable = complete && fact.fully_shown && !fact.released;
    let h = height(who_lines.len(), cmd.len(), why.len());
    let breath = breathe(ctx.now, 1.6);
    s.grid.boxed(
        x,
        y,
        w,
        h,
        mix(c::WARN_DIM, c::WARN, breath),
        Some(c::WARN_BG),
    );
    s.grid.bold(x + 2, y, " \u{2691} NEEDS YOU ", c::WARN);
    if count > 1 {
        let tag = format!(" 1 of {count} ");
        s.grid.text(x + w - 2 - theme::width(&tag), y, &tag, c::DIM);
    }
    let mut yy = y + 1;
    for (i, line) in who_lines.iter().enumerate() {
        if i == 0 {
            s.grid
                .put(x + 2, yy, '\u{25cf}', Some(c::AGENT), None, true);
        }
        s.grid.bold(x + 4, yy, line, c::HI);
        yy += 1;
    }
    if let Some(sel) = sel.clone() {
        let r = s.reg(
            x,
            y + 1,
            w,
            who_lines.len() as i32,
            Some(Act::Select(sel.clone())),
        );
        r.node = Some(sel);
    }
    let age = format!(" \u{b7} {} ago", content::elapsed_label(fact.waited_secs));
    let asks = if view.cwd.is_empty() {
        format!("asks to run this{age}")
    } else {
        // A long directory loses its front, not its end: the end is the part that tells them apart.
        let room =
            ((w - 6) as usize).saturating_sub("asks to run this in ".len() + age.chars().count());
        let chars: Vec<char> = view.cwd.chars().collect();
        let cwd = if chars.len() <= room {
            view.cwd.clone()
        } else {
            let tail: String = chars[chars.len() - room.saturating_sub(1)..]
                .iter()
                .collect();
            format!("\u{2026}{tail}")
        };
        format!("asks to run this in {cwd}{age}")
    };
    s.grid.text(x + 4, yy, &cut(&asks, w - 6), c::DIM);
    yy += 2;
    for (i, line) in cmd.iter().enumerate() {
        s.grid
            .put(x + 2, yy, '\u{258e}', Some(c::WARN), None, false);
        color_command(&mut s.grid, x + 4, yy, line, i == 0);
        yy += 1;
    }
    yy += 1;
    for line in &why {
        s.grid.text(x + 2, yy, line, c::FG);
        yy += 1;
    }
    if !why.is_empty() {
        yy += 1;
    }
    let act = |code: char, on: bool| on.then_some(Act::Press(KeyCode::Char(code)));
    let always = view.always.is_some() && answerable;
    let mut bx = s.chip(
        ctx,
        x + 2,
        yy,
        Chip::new("y", "Allow once", act('y', answerable))
            .bg(c::OK_BG)
            .kc(c::OK)
            .id("ask-y")
            .node(sel.clone()),
    );
    s.chip(
        ctx,
        bx + 2,
        yy,
        Chip::new("a", "Always allow", act('a', always))
            .id("ask-a")
            .node(sel.clone()),
    );
    yy += 1;
    bx = s.chip(
        ctx,
        x + 2,
        yy,
        Chip::new("d", "Deny", act('d', answerable))
            .kc(c::ERR)
            .id("ask-d")
            .node(sel.clone()),
    );
    s.chip(
        ctx,
        bx + 2,
        yy,
        Chip::new("\u{23ce}", "Open its harness", sel.clone().map(Act::Open))
            .id("ask-o")
            .node(sel.clone()),
    );
    yy += 1;
    let scope = if let Some(label) = view.always.as_deref().filter(|_| answerable) {
        let covered = if label.is_empty() {
            prefix_of(&view.command)
        } else {
            label.to_string()
        };
        format!("Always allow stops asking for {covered}")
    } else if view.outside_sandbox {
        "This command runs outside the sandbox.".to_string()
    } else if answerable {
        "Allow once and deny answer this request only.".to_string()
    } else {
        "Not shown in full: answer it in its harness.".to_string()
    };
    s.grid.text(x + 2, yy, &cut(&scope, w - 4), c::FAINT);
    s.shown_approvals.push(ShownApproval {
        short: fact.short.clone(),
        conn: fact.conn,
        answerable,
        always,
    });
    h
}

/// A command line in colour: the program bold, flags dim, paths and names in the seat's cyan.
fn color_command(g: &mut theme::Grid, x: i32, y: i32, line: &str, first: bool) {
    let mut px = x;
    let mut word = 0;
    let mut rest = line;
    while !rest.is_empty() {
        let blank = rest.starts_with(char::is_whitespace);
        let end = rest
            .find(|ch: char| ch.is_whitespace() != blank)
            .unwrap_or(rest.len());
        let (tok, tail) = rest.split_at(end);
        rest = tail;
        if blank {
            px = g.text(px, y, tok, c::FG);
            continue;
        }
        let (fg, bold) = if first && word == 0 {
            (c::HI, true)
        } else if tok.starts_with('-') {
            (c::DIM, false)
        } else if tok.contains(['/', '<', '>'])
            || tok.contains("::")
            || tok.contains(".rs")
            || tok.contains(".log")
        {
            (c::SEAT, false)
        } else {
            (c::FG, false)
        };
        word += 1;
        px = g.text_on(px, y, tok, fg, None, bold);
    }
}

/// One wait that only its harness can answer, as a card: who, what it waits on, where, and a way in.
/// Returns its height.
fn wait_card(
    s: &mut Scene,
    ctx: &Ctx,
    (kind, evidence, need): (WaitKind, &str, &Need),
    count: usize,
    (x, y, w): (i32, i32, i32),
) -> i32 {
    let model = ctx.model;
    let inbox_hint =
        matches!(kind, WaitKind::Permission | WaitKind::Approval) && !model.facts.approvals_inbox;
    let h = if inbox_hint { 7 } else { 5 };
    s.grid.boxed(
        x,
        y,
        w,
        h,
        mix(c::WARN_DIM, c::WARN, breathe(ctx.now, 1.6)),
        Some(c::WARN_BG),
    );
    s.grid.bold(x + 2, y, " \u{2691} NEEDS YOU ", c::WARN);
    let tag = format!(
        " {}{} ",
        if count > 1 {
            format!("1 of {count} \u{b7} ")
        } else {
            String::new()
        },
        content::elapsed_label(need.age)
    );
    s.grid.text(x + w - 2 - theme::width(&tag), y, &tag, c::DIM);
    s.grid
        .put(x + 2, y + 1, '\u{25cf}', Some(c::AGENT), None, true);
    let name = cut(&need.name, 20);
    let after = s.grid.bold(x + 4, y + 1, &name, c::HI);
    let place = need
        .sel
        .as_ref()
        .and_then(|sel| model.node(sel))
        .map(|node| where_line(model, node))
        .unwrap_or_default();
    let line = if place.is_empty() {
        format!("\u{b7} {}", wait_verb(kind))
    } else {
        format!("\u{b7} {} \u{b7} {place}", wait_verb(kind))
    };
    s.grid
        .text(after + 1, y + 1, &cut(&line, x + w - 3 - after), c::DIM);
    s.grid.text(x + 2, y + 2, &cut(evidence, w - 4), c::FG);
    let mut yy = y + 3;
    if inbox_hint {
        for note in [
            "Answer it in its pane.",
            "To answer here: [approvals] inbox = true",
        ] {
            s.grid.text(x + 2, yy, &cut(note, w - 4), c::FAINT);
            yy += 1;
        }
    }
    s.chip(
        ctx,
        x + 2,
        yy,
        Chip::new(
            "\u{23ce}",
            "Open its pane",
            Some(Act::Open(need.sel.clone().unwrap_or(Sel::Seat))),
        )
        .id("wait-e")
        .node(need.sel.clone()),
    );
    h
}

/// One open supervisor ruling as a card: whose it is, its kind, why, and the two answers. Returns
/// its height.
fn ruling_card(
    s: &mut Scene,
    ctx: &Ctx,
    (ruling, need): (&super::super::super::supervisor::Ruling, &Need),
    count: usize,
    (x, y, w): (i32, i32, i32),
    max_reason: usize,
) -> i32 {
    let iw = (w - 4) as usize;
    let text = if ruling.reason.is_empty() {
        &ruling.verdict
    } else {
        &ruling.reason
    };
    let reason = wrap(text, iw, Some(max_reason.max(1))).0;
    let h = reason.len() as i32 + 4;
    s.grid.boxed(x, y, w, h, c::ARCH, Some(c::WARN_BG));
    let kind = ruling.kind.as_str();
    s.grid.bold(
        x + 2,
        y,
        &format!(" \u{bb} SUPERVISOR \u{b7} {kind} "),
        c::ARCH,
    );
    let tag = format!(
        " {}{} ",
        if count > 1 {
            format!("1 of {count} \u{b7} ")
        } else {
            String::new()
        },
        content::elapsed_label(need.age)
    );
    s.grid.text(x + w - 2 - theme::width(&tag), y, &tag, c::DIM);
    s.grid
        .put(x + 2, y + 1, '\u{25cf}', Some(c::AGENT), None, true);
    let verdict = ruling.verdict.replace('_', " ");
    s.grid.bold(x + 4, y + 1, &cut(&need.name, 20), c::HI);
    s.grid.text(
        x + 5 + theme::width(&cut(&need.name, 20)),
        y + 1,
        &cut(
            &format!("\u{b7} {verdict}"),
            w - 10 - theme::width(&cut(&need.name, 20)),
        ),
        c::DIM,
    );
    let mut yy = y + 2;
    for line in &reason {
        s.grid.text(x + 2, yy, line, c::FG);
        yy += 1;
    }
    let seat = need.sel.clone().unwrap_or(Sel::Seat);
    let label = if seat == Sel::Seat {
        "Open the seat"
    } else {
        "Open its harness"
    };
    let bx = s.chip(
        ctx,
        x + 2,
        yy,
        Chip::new("o", "Override", Some(Act::Press(KeyCode::Char('o'))))
            .id("rule-o")
            .node(need.sel.clone()),
    );
    s.chip(
        ctx,
        bx + 2,
        yy,
        Chip::new("\u{23ce}", label, Some(Act::Open(seat))).id("rule-e"),
    );
    s.shown_rulings.push(ShownRuling {
        id: ruling.id.clone(),
        short: sessions::short_id(&ruling.session),
    });
    h
}

/// `(glyph, colour, verb, verb colour)` of a need's line.
fn need_style(need: &Need) -> (char, Rgb, String, Rgb) {
    match &need.kind {
        NeedKind::Approval(fact) => (
            '\u{2691}',
            c::WARN,
            if fact.released {
                "waits in its harness"
            } else {
                "wants to run"
            }
            .to_string(),
            c::DIM,
        ),
        NeedKind::Ruling(r) => (
            '\u{bb}',
            c::ARCH,
            format!("ruled {}", r.kind.as_str()),
            c::DIM,
        ),
        NeedKind::Failed { .. } => ('\u{2717}', c::ERR, "failed".to_string(), c::ERR),
        NeedKind::Wait { kind, .. } => ('?', c::WARN, wait_verb(*kind).to_string(), c::DIM),
        NeedKind::Stalled => ('\u{25cc}', c::WARN, "stalled".to_string(), c::DIM),
    }
}

fn need_age(need: &Need) -> String {
    match &need.kind {
        NeedKind::Approval(_) => clock(need.age),
        _ if need.age == 0 => String::new(),
        _ => content::elapsed_label(need.age),
    }
}

/// The next workflow gate as the last line of the compact list.
fn gate_line(model: &Model) -> Option<String> {
    let wf = model.facts.workflow?;
    let gate = wf.next_gate.as_ref()?;
    Some(format!(
        "next gate {} \u{b7} after {}",
        cut(gate, 14),
        cut(&wf.step, 14)
    ))
}

/// The right column: the approval card, the compact list of everything else, then SELECTED.
fn right_col(s: &mut Scene, ctx: &Ctx, items: &[Need], (rx, rw): (i32, i32), fh: i32) {
    let mut y = TOP;
    let model = ctx.model;
    let approvals = model.facts.approval_items.len();
    let first = items
        .iter()
        .position(|n| matches!(n.kind, NeedKind::Approval(_)));
    if let Some(i) = first
        && let NeedKind::Approval(fact) = &items[i].kind
    {
        let max_h = (fh - 1 - SELECTED_MIN).max(10);
        y += approval_card(s, ctx, fact, approvals, (rx, y, rw), max_h) + 1;
    }
    let waits = items
        .iter()
        .filter(|n| matches!(n.kind, NeedKind::Wait { .. }))
        .count();
    let mut drawn_wait = None;
    if first.is_none()
        && let Some(i) = items
            .iter()
            .position(|n| matches!(n.kind, NeedKind::Wait { .. }))
        && let NeedKind::Wait { kind, evidence } = &items[i].kind
        && fh - 1 - SELECTED_MIN >= 8
    {
        y += wait_card(s, ctx, (*kind, evidence, &items[i]), waits, (rx, y, rw)) + 1;
        drawn_wait = Some(i);
    }
    let rulings = items
        .iter()
        .filter(|n| matches!(n.kind, NeedKind::Ruling(_)))
        .count();
    let ruled = items
        .iter()
        .position(|n| matches!(n.kind, NeedKind::Ruling(_)));
    let mut drawn_ruling = None;
    if let Some(i) = ruled
        && let NeedKind::Ruling(r) = &items[i].kind
    {
        let room = fh - (y - TOP) - 1 - SELECTED_MIN;
        if room >= 5 {
            let h = ruling_card(
                s,
                ctx,
                (r, &items[i]),
                rulings,
                (rx, y, rw),
                (room - 4).min(3) as usize,
            );
            y += h + 1;
            drawn_ruling = Some(i);
        }
    }
    let rest: Vec<&Need> = items
        .iter()
        .enumerate()
        .filter(|(i, _)| Some(*i) != first && Some(*i) != drawn_ruling && Some(*i) != drawn_wait)
        .map(|(_, n)| n)
        .collect();
    let gate = gate_line(model);
    let used = y - TOP;
    let room = fh - used - 1 - SELECTED_MIN;
    let carded = first.is_some() || drawn_ruling.is_some() || drawn_wait.is_some();
    if rest.is_empty() && !carded {
        let h = if gate.is_some() { 4 } else { 3 };
        s.grid.boxed(rx, y, rw, h, c::RULE, Some(c::BG));
        s.grid.bold(rx + 2, y, " NEEDS YOU ", c::DIM);
        s.grid
            .put(rx + 2, y + 1, '\u{2713}', Some(c::OK), None, true);
        s.grid
            .text(rx + 4, y + 1, "Nothing needs you right now", c::DIM);
        if let Some(gate) = &gate {
            s.grid.text(rx + 2, y + 2, &cut(gate, rw - 4), c::FAINT);
        }
        y += h + 1;
    } else if room >= 3 && (!rest.is_empty() || gate.is_some()) {
        let rows = (room - 2 - i32::from(gate.is_some())).max(0) as usize;
        let shown = rest.len().min(if rest.len() > rows {
            rows.saturating_sub(1)
        } else {
            rows
        });
        let h = 2 + shown as i32 + i32::from(rest.len() > shown) + i32::from(gate.is_some());
        let title = if carded {
            " ALSO NEEDS YOU "
        } else {
            " \u{2691} NEEDS YOU "
        };
        let col = if carded { c::DIM } else { c::WARN };
        s.grid.boxed(rx, y, rw, h, c::RULE, Some(c::BG));
        s.grid.bold(rx + 2, y, title, col);
        s.grid.text(
            rx + 2 + theme::width(title),
            y,
            &format!("\u{b7} {} ", rest.len()),
            c::DIM,
        );
        let mut yy = y + 1;
        for need in rest.iter().take(shown) {
            let (glyph, gc, verb, vc) = need_style(need);
            s.grid.put(rx + 2, yy, glyph, Some(gc), None, true);
            let age = need_age(need);
            let name = cut(&need.name, 16);
            let mut x = s.grid.bold(rx + 4, yy, &name, c::FG);
            x = s.grid.text(x, yy, &format!(" {verb}"), vc);
            let detail = match &need.kind {
                NeedKind::Failed { why: Some(w) } => w.clone(),
                NeedKind::Wait { evidence, .. } => evidence.clone(),
                NeedKind::Approval(f) => cut(&f.view.command, 40),
                NeedKind::Ruling(r) => cut(&r.reason, 40),
                _ => String::new(),
            };
            let avail = rx + rw - 3 - theme::width(&age) - 2 - x;
            if !detail.is_empty() && avail > 6 {
                s.grid.text(x + 2, yy, &cut(&detail, avail - 2), c::FAINT);
            }
            s.grid
                .text(rx + rw - 2 - theme::width(&age), yy, &age, c::DIM);
            if let Some(sel) = need.sel.clone() {
                let r = s.reg(rx + 1, yy, rw - 2, 1, Some(Act::Select(sel.clone())));
                r.node = Some(sel);
            }
            yy += 1;
        }
        if rest.len() > shown {
            s.grid
                .text(rx + 2, yy, &format!("+{} more", rest.len() - shown), c::DIM);
            yy += 1;
        }
        if let Some(gate) = &gate {
            s.grid.text(rx + 2, yy, &cut(gate, rw - 4), c::FAINT);
        }
        y += h + 1;
    }
    selected(s, ctx, (rx, y, rw, TOP + fh - y));
}

// -- SELECTED / PREVIEW ---------------------------------------------------------------------

struct Subject<'a> {
    sel: Sel,
    name: String,
    node: Option<&'a Node>,
    short: Option<String>,
}

fn subject<'a>(model: &Model<'a>, sel: &Sel) -> Subject<'a> {
    let node = model.node(sel);
    let short = match sel {
        Sel::Seat => model.facts.seat_session.map(sessions::short_id),
        Sel::Jev => None,
        _ => node.map(|n| sessions::short_id(&n.id)),
    };
    Subject {
        sel: sel.clone(),
        name: name_of(model, sel),
        node,
        short,
    }
}

fn kv(s: &mut Scene, x: i32, y: i32, k: &str, v: &str, iw: i32) {
    s.grid.text(x + 2, y, &format!("{k:<9}"), c::FAINT);
    s.grid.text(x + 11, y, &cut(v, iw - 9), c::FG);
}

/// Where a node runs, in words.
fn where_line(model: &Model, node: &Node) -> String {
    let short = sessions::short_id(&node.id);
    if let Some(m) = model.facts.pane_meta.iter().find(|m| m.short == short) {
        return format!("pane {} \u{b7} {}", m.number, m.worktree);
    }
    match model.host(node) {
        Some(h) => {
            let pane = model
                .facts
                .pane_meta
                .iter()
                .find(|m| m.short == h.short)
                .map_or(String::new(), |m| format!(" \u{b7} pane {}", m.number));
            format!("inside {}{pane}", h.name)
        }
        None => "no pane here".to_string(),
    }
}

/// What kind of agent a node is, in words.
fn kind_line(model: &Model, node: &Node) -> String {
    let harness = node.harness.as_deref().unwrap_or("agent");
    let cap = |h: &str| {
        let mut ch = h.chars();
        ch.next()
            .map_or(String::new(), |f| f.to_uppercase().chain(ch).collect())
    };
    if model.pane_short(node).is_some() {
        return format!("{} pane", cap(harness));
    }
    match model.host(node) {
        Some(h) => format!("{} subagent, inside {}", cap(harness), h.name),
        None => format!("{} {}", cap(harness), node.kind),
    }
}

/// Whether `m` can mail this node: it has a harness, or a host that does.
pub(super) fn mail_target(model: &Model, sel: &Sel) -> Option<(String, Option<String>)> {
    let node = model.node(sel);
    let own = match sel {
        Sel::Seat => model.facts.seat_harness.map(str::to_string),
        _ => node.and_then(|n| n.harness.clone()),
    };
    let Some(node) = node.filter(|n| model.pane_short(n).is_none() && *sel != Sel::Seat) else {
        return own.map(|to| (to, None));
    };
    match model.host(node) {
        Some(host) => host
            .harness
            .or_else(|| model.facts.seat_harness.map(str::to_string))
            .map(|to| (to, Some(model.job_of(node)))),
        None => own.map(|to| (to, None)),
    }
}

fn selected(s: &mut Scene, ctx: &Ctx, (x, y, w, h): (i32, i32, i32, i32)) {
    if h < 4 {
        return;
    }
    let model = ctx.model;
    let id = ctx.hover.unwrap_or(ctx.sel);
    let preview = ctx.hover.is_some_and(|hv| hv != ctx.sel);
    s.grid.boxed(x, y, w, h, c::RULE, Some(c::BG));
    s.grid.bold(
        x + 2,
        y,
        if preview { " PREVIEW " } else { " SELECTED " },
        c::DIM,
    );
    let iw = w - 4;
    let bottom = y + h - 3;
    // A short panel drops the blank rows between its sections.
    let gap = i32::from(h >= 20);
    let mut yy = y + 1;
    let subj = subject(model, id);
    if subj.sel == Sel::Jev {
        jev_panel(s, ctx, &subj, (x, y, w, h));
        return;
    }
    let Some(node) = subj.node.filter(|_| subj.sel != Sel::Seat) else {
        // The seat's own panel, or a node that is gone.
        s.grid.put(x + 2, yy, '\u{25cf}', Some(c::SEAT), None, true);
        s.grid.bold(x + 4, yy, "Your seat", c::HI);
        yy += 1;
        let seat = format!(
            "{} \u{b7} {}",
            content::seat_title(model.facts),
            model.facts.seat_role.unwrap_or("orchestrator")
        );
        s.grid.text(x + 4, yy, &cut(&seat, iw - 2), c::DIM);
        yy += 1 + gap;
        let wf = model
            .facts
            .workflow
            .map_or("none running".to_string(), |wf| {
                format!("{} \u{b7} {}", wf.pack, wf.step)
            });
        kv(s, x, yy, "workflow", &wf, iw);
        yy += 1;
        kv(s, x, yy, "agents", &flow::counts_line(ctx, iw - 9), iw);
        yy += 1;
        yy += gap;
        s.grid.bold(x + 2, yy, "RECENT", c::FAINT);
        yy += 1;
        recent_events(s, ctx, &subj, (x, yy, iw), bottom);
        chips(s, ctx, &subj, x, y + h - 3, w);
        return;
    };
    let st = flow::status(model, node);
    let (g, gc) = flow::glyph(st, ctx.now);
    s.grid.put(x + 2, yy, g, Some(gc), None, true);
    for line in wrap(&model.job_of(node), (iw - 2) as usize, Some(2)).0 {
        s.grid.bold(x + 4, yy, &line, c::HI);
        yy += 1;
    }
    let (word, wc) = match st {
        St::Running => (
            format!("working for {}", flow::age_word(node, st, ctx.wall)),
            c::AGENT,
        ),
        St::Waiting => ("waiting for you".to_string(), c::WARN),
        St::Done | St::Failed => (
            flow::age_word(node, st, ctx.wall),
            if st == St::Done { c::OK } else { c::ERR },
        ),
        St::Idle => ("not started".to_string(), c::DIM),
    };
    s.grid.text(x + 4, yy, &word, wc);
    yy += 1 + gap;
    let mut fields: Vec<(&str, String)> = vec![
        ("model", content::node_model_badged(model.data, node)),
        ("kind", kind_line(model, node)),
        ("where", where_line(model, node)),
    ];
    if let Some(step) = content::node_step(node) {
        fields.push(("step", step));
    }
    if !model
        .agents
        .iter()
        .find(|a| a.node.id == node.id)
        .is_none_or(|a| a.kids.is_empty())
    {
        let kids = model
            .agents
            .iter()
            .find(|a| a.node.id == node.id)
            .map_or(0, |a| a.kids.len());
        fields.push(("children", format!("{kids}")));
    }
    for (k, v) in &fields {
        if yy >= bottom {
            break;
        }
        kv(s, x, yy, k, v, iw);
        yy += 1;
    }
    yy += gap;
    if yy < bottom {
        s.grid.bold(x + 2, yy, "NOW", c::FAINT);
        yy += 1;
        let (now_text, nc) = flow::now_line(node, st);
        s.grid.text(x + 2, yy, &cut(&now_text, iw), nc);
        yy += 1 + gap;
    }
    if yy < bottom {
        s.grid.bold(x + 2, yy, "RECENT", c::FAINT);
        yy += 1;
        let room = (bottom - yy).max(0) as usize;
        let steps = flow::recent(node, st, ctx.wall, room);
        if steps.is_empty() {
            recent_events(s, ctx, &subj, (x, yy, iw), bottom);
        }
        for (age, label) in steps {
            s.grid.text(
                x + 2,
                yy,
                &format!("{:>5}", content::elapsed_label(age)),
                c::FAINT,
            );
            s.grid.text(x + 9, yy, &cut(&label, iw - 7), c::DIM);
            yy += 1;
        }
    }
    chips(s, ctx, &subj, x, y + h - 3, w);
}

/// SELECTED for Jev: what it decided today, which sites are on, how it works, its recent decisions.
fn jev_panel(s: &mut Scene, ctx: &Ctx, subj: &Subject, (x, y, w, h): (i32, i32, i32, i32)) {
    let feed = content::jev_feed(ctx.model.data);
    let iw = w - 4;
    let bottom = y + h - 3;
    let mut yy = y + 1;
    s.grid.put(x + 2, yy, '\u{25c6}', Some(c::JEV), None, true);
    s.grid.bold(x + 4, yy, "Jev", c::HI);
    yy += 1;
    if !feed.on() {
        s.grid.text(x + 4, yy, "off", c::DIM);
        yy += 2;
        let why = "No [jev] site is on in the operator config and the harness proxy does not use TypeSafe, so every decision is made by zirv's own rules.";
        for line in wrap(why, iw as usize, Some(5)).0 {
            s.grid.text(x + 2, yy, &line, c::FAINT);
            yy += 1;
        }
        chips(s, ctx, subj, x, y + h - 3, w);
        return;
    }
    s.grid.text(x + 4, yy, "TypeSafe decision model", c::DIM);
    yy += 2;
    let today = flow::jev_today(ctx, &feed);
    let unsure = today.iter().filter(|j| !j.sure).count();
    let line = format!("{} calls \u{b7} {unsure} unsure", today.len());
    kv(s, x, yy, "today", &line, iw);
    yy += 1;
    let sites = if feed.sites.is_empty() {
        "the harness proxy".to_string()
    } else {
        feed.sites.join(", ")
    };
    for (i, l) in wrap(&sites, (iw - 9) as usize, Some(3))
        .0
        .iter()
        .enumerate()
    {
        kv(s, x, yy, if i == 0 { "sites on" } else { "" }, l, iw);
        yy += 1;
    }
    yy += 1;
    // The explanation goes first when there is room for it and the decisions below it.
    if bottom - yy >= 9 {
        let how = "It answers typed questions about the work. Sure answers are applied in code; unsure ones fall back to zirv's rules, or to the supervisor when it is on.";
        for l in wrap(how, iw as usize, Some(3)).0 {
            s.grid.text(x + 2, yy, &l, c::FAINT);
            yy += 1;
        }
        yy += 1;
    }
    if yy < bottom {
        s.grid.bold(x + 2, yy, "RECENT", c::FAINT);
        yy += 1;
    }
    let room = (bottom - yy).max(0) as usize;
    for j in feed.rows.iter().rev().take(room) {
        let age = content::elapsed_label(ctx.wall.saturating_sub(j.ts));
        s.grid.text(x + 2, yy, &format!("{age:>5}"), c::FAINT);
        s.grid.text(x + 8, yy, &cut(&j.site, 8), c::JEV);
        s.grid.text(x + 17, yy, &cut(&j.text, iw - 21), c::DIM);
        s.grid.text(
            x + w - 7,
            yy,
            &format!("{:.2}", j.confidence),
            if j.sure { c::OK } else { c::WARN },
        );
        yy += 1;
    }
    chips(s, ctx, subj, x, y + h - 3, w);
}

/// The node's newest mail and decisions, standing in for its steps until the graph carries them.
fn recent_events(
    s: &mut Scene,
    ctx: &Ctx,
    subj: &Subject,
    (x, y, iw): (i32, i32, i32),
    bottom: i32,
) {
    let room = (bottom - y).max(0) as usize;
    let rows = activity_rows(ctx, Some(subj));
    for (i, row) in rows.iter().rev().take(room).rev().enumerate() {
        let yy = y + i as i32;
        s.grid.text(x + 2, yy, &hm(ctx.model, row.ts), c::FAINT);
        let line = format!("{}  {}", row.counterpart(), row.text);
        s.grid.text(x + 8, yy, &cut(&line, iw - 6), c::DIM);
    }
}

fn hm(model: &Model, ts: u64) -> String {
    model
        .facts
        .utc_offset
        .timestamp_opt(ts as i64, 0)
        .single()
        .map_or("--:--".to_string(), |t| t.format("%H:%M").to_string())
}

/// The action chips under SELECTED: whole chips on the last two rows. A chip whose action does
/// not apply is faint and does nothing.
fn chips(s: &mut Scene, ctx: &Ctx, subj: &Subject, x: i32, y: i32, w: i32) {
    let model = ctx.model;
    let pane = model.selected_pane(&subj.sel).is_some();
    let sel = subj.sel.clone();
    let mailable = mail_target(model, &sel).is_some();
    let on = |applies: bool, code: char| applies.then_some(Act::Press(KeyCode::Char(code)));
    let specs = [
        Chip::new(
            "\u{23ce}",
            "Open",
            (sel != Sel::Jev).then(|| Act::Open(sel.clone())),
        )
        .id("sel-o"),
        // A click on a chip acts on the node it is under, whatever else is selected.
        Chip::new("m", "Message", on(mailable, 'm'))
            .id("sel-m")
            .node(Some(sel.clone())),
        Chip::new("n", "Nudge", on(pane, 'n'))
            .id("sel-n")
            .node(Some(sel.clone())),
        Chip::new("x", "Stop", on(pane, 'x'))
            .id("sel-x")
            .node(Some(sel.clone())),
    ];
    let (mut cx, mut cy) = (x + 2, y);
    for spec in specs {
        if cx + spec.width() > x + w - 1 {
            cx = x + 2;
            cy += 1;
        }
        if cy >= y + 2 {
            break;
        }
        cx = s.chip(ctx, cx, cy, spec) + 1;
    }
    // Stop's confirmation takes the place of the chips.
    if ctx.view.confirm_stop.as_ref() == Some(&sel) {
        s.grid.fill(x + 1, y, w - 2, 2, c::BG);
        let q = format!("stop {}? ", cut(&subj.name, 20));
        let after = s.grid.text_on(x + 2, y + 1, &q, c::WARN, None, true);
        let after = s.grid.bold(after, y + 1, "y", c::HI);
        let after = s.grid.text(after, y + 1, " / ", c::DIM);
        s.grid.bold(after, y + 1, "n", c::HI);
    }
}

// -- ACTIVITY -------------------------------------------------------------------------------

struct ActRow {
    ts: u64,
    key: String,
    from: String,
    from_c: Rgb,
    glyph: char,
    glyph_c: Rgb,
    to: String,
    to_c: Rgb,
    text: String,
    text_c: Rgb,
    sel: Option<Sel>,
}

impl ActRow {
    /// The other end of the row from the node it is listed under.
    fn counterpart(&self) -> String {
        format!("{} \u{2192} {}", self.from, self.to)
    }
}

fn role_rgb(name: &str, kind: &str) -> Rgb {
    match (name, kind) {
        ("jev" | "proxy", _) | (_, "jev") => c::JEV,
        ("supervisor", _) => c::ARCH,
        ("seat", _) => c::SEAT,
        ("you", _) => c::YOU,
        _ => c::AGENT,
    }
}

/// A name for whoever a log row names: the seat, else the agent's job cut at a word, else its
/// label.
fn who(model: &Model, raw: &str) -> String {
    let short = sessions::short_id(raw);
    if model
        .facts
        .seat_session
        .is_some_and(|seat| sessions::short_id(seat) == short)
    {
        return "seat".to_string();
    }
    let node = model
        .data
        .nodes
        .iter()
        .find(|n| n.id == raw || (!short.is_empty() && sessions::short_id(&n.id) == short));
    match node {
        Some(n) => short_name(&model.job_of(n), 22),
        None => content::actor_name(model, raw),
    }
}

/// The agent a dispatch or subagent event is about: a subagent event names it as the second word
/// of its summary, a dispatch event carries the role and the very second it started.
fn event_agent<'a>(model: &Model<'a>, event: &Event) -> Option<&'a Node> {
    let nodes = &model.data.nodes;
    if event.kind.starts_with("subagent") {
        let id = event.summary.split_whitespace().nth(1)?;
        return nodes.iter().find(|n| n.id == id);
    }
    if event.kind == "dispatch" {
        let role = event.to.as_deref()?;
        return nodes.iter().find(|n| {
            n.started_at == Some(event.ts)
                && n.role
                    .as_deref()
                    .is_some_and(|r| r.eq_ignore_ascii_case(role))
        });
    }
    None
}

fn act_row(model: &Model, event: &Event) -> ActRow {
    let actor = who(model, &event.actor);
    let summary = content::clean(&event.summary);
    let mut row = ActRow {
        ts: event.ts,
        key: super::fx::event_key(event),
        from_c: role_rgb(&actor, &event.kind),
        from: actor,
        glyph: '\u{2500}',
        glyph_c: c::DIM,
        to: String::new(),
        to_c: c::AGENT,
        text: summary.clone(),
        text_c: c::FG,
        sel: row_sel(model, event),
    };
    // Jev's own plain words from the feed (`site: verdict · confidence`), never a raw margin.
    let verdict = |event: &Event| -> String {
        let feed = model
            .data
            .jev
            .rows
            .iter()
            .find(|r| r.ts == event.ts && event.summary.starts_with(r.site.as_str()));
        match feed {
            Some(r) => format!(
                "{}: {} \u{b7} {:.2} {}",
                r.site,
                r.text,
                r.confidence,
                if r.sure { "sure" } else { "unsure" }
            ),
            None => summary.clone(),
        }
    };
    match event.kind.as_str() {
        "mail" => {
            row.glyph = '\u{2709}';
            row.glyph_c = c::SEAT;
            let to = event.to.as_deref().unwrap_or("?");
            row.to = who(model, to);
            row.to_c = role_rgb(&row.to, "mail");
        }
        "jev" | "proxy" => {
            let session = who(model, &event.actor);
            row.from = if event.kind == "proxy" {
                "proxy"
            } else {
                "jev"
            }
            .into();
            row.from_c = c::JEV;
            row.glyph = '\u{25c6}';
            row.glyph_c = c::JEV;
            row.to = if event.kind == "proxy" {
                "seat".into()
            } else {
                session
            };
            row.to_c = c::SEAT;
            row.text = verdict(event);
        }
        k if k.contains("supervisor") || event.actor == "supervisor" => {
            row.from = "supervisor".into();
            row.from_c = c::ARCH;
            row.glyph = '\u{bb}';
            row.glyph_c = c::WARN;
            row.to = event
                .to
                .as_deref()
                .map_or_else(|| "seat".into(), |to| who(model, to));
            row.to_c = c::SEAT;
            row.text_c = c::WARN;
        }
        k if k.starts_with("subagent") => {
            let mut words = summary.splitn(2, ' ');
            let kind = words.next().unwrap_or_default().to_string();
            let rest = words.next().unwrap_or_default();
            let rest = rest.split_once(' ').map_or("", |(_, tail)| tail);
            row.to = event_agent(model, event).map_or(kind, |n| short_name(&model.job_of(n), 22));
            row.glyph = if k == "subagent_start" {
                '\u{25cf}'
            } else {
                '\u{2713}'
            };
            row.glyph_c = if k == "subagent_start" {
                c::AGENT
            } else {
                c::OK
            };
            row.text = if k == "subagent_start" {
                "started".to_string()
            } else {
                rest.to_string()
            };
        }
        "dispatch" => {
            row.glyph = '\u{25cf}';
            row.glyph_c = c::AGENT;
            match event_agent(model, event) {
                Some(n) => {
                    row.to = short_name(&model.job_of(n), 22);
                    row.text = format!("dispatched \u{b7} {}", content::node_model(n));
                }
                None => row.to = event.to.clone().unwrap_or_default(),
            }
        }
        "delegation" => {
            let mut words = summary.splitn(2, ' ');
            row.to = words.next().unwrap_or_default().to_string();
            row.text = words.next().unwrap_or_default().to_string();
            row.glyph = '\u{25cf}';
            row.glyph_c = c::AGENT;
        }
        _ => {}
    }
    row
}

/// An approval that waits is the agent asking: it is activity too, newest at its wait.
fn ask_row(model: &Model, fact: &ApprovalFact) -> ActRow {
    let sel = model.sel_for_short(&fact.short);
    let who = match sel.as_ref().and_then(|s| model.node(s)) {
        Some(n) => short_name(&model.job_of(n), 22),
        None => sel
            .as_ref()
            .map_or_else(|| fact.short.clone(), |s| name_of(model, s)),
    };
    ActRow {
        ts: model.facts.now.saturating_sub(fact.waited_secs),
        key: super::fx::ask_key(fact.conn),
        from: who,
        from_c: c::AGENT,
        glyph: '\u{2691}',
        glyph_c: c::WARN,
        to: "you".into(),
        to_c: c::YOU,
        text: format!("asks to run {}", fact.view.command),
        text_c: c::WARN,
        sel: sel.filter(|s| *s != Sel::Seat),
    }
}

/// The agent a row is about, for a click: the actor if it is one, else the recipient.
fn row_sel(model: &Model, event: &Event) -> Option<Sel> {
    let known = |who: &str| {
        model
            .sel_for_short(&sessions::short_id(who))
            .or_else(|| {
                model
                    .agents
                    .iter()
                    .find(|a| {
                        a.node
                            .role
                            .as_deref()
                            .is_some_and(|r| r.eq_ignore_ascii_case(who))
                    })
                    .map(|a| Sel::Agent(a.node.id.clone()))
            })
            .filter(|s| *s != Sel::Seat)
    };
    // A subagent event names its agent as the second word of the summary: "Explore a1".
    let agent = event
        .kind
        .starts_with("subagent")
        .then(|| event.summary.split_whitespace().nth(1))
        .flatten();
    known(&event.actor)
        .or_else(|| event.to.as_deref().and_then(known))
        .or_else(|| agent.and_then(known))
}

/// Whether an event is between the node and anyone.
fn involves(event: &Event, s: &Subject) -> bool {
    if s.sel == Sel::Jev {
        return matches!(event.kind.as_str(), "jev" | "proxy");
    }
    let short = s.short.as_deref().unwrap_or_default();
    if !short.is_empty() && sessions::short_id(&event.actor) == short {
        return true;
    }
    let titles = [
        Some(s.name.as_str()),
        s.node.and_then(|n| n.role.as_deref()),
    ];
    event.to.as_deref().is_some_and(|to| {
        (!short.is_empty() && sessions::short_id(to) == short)
            || titles.iter().flatten().any(|t| t.eq_ignore_ascii_case(to))
    }) || (event.kind.starts_with("subagent")
        && event
            .summary
            .split_whitespace()
            .next()
            .zip(s.node.and_then(|n| n.role.as_deref()))
            .is_some_and(|(kind, role)| kind == role))
}

/// Whether the event happened in a session this view shows; an event of an unknown session is
/// not (the `All` scope shows everything).
fn in_scope(model: &Model, event: &Event) -> bool {
    if model.scope == super::model::Scope::All {
        return true;
    }
    let known = |who: &str| model.sel_for_short(&sessions::short_id(who)).is_some();
    known(&event.actor) || event.to.as_deref().is_some_and(known)
}

fn activity_rows(ctx: &Ctx, filter: Option<&Subject>) -> Vec<ActRow> {
    let model = ctx.model;
    let mut rows: Vec<ActRow> = model
        .data
        .events
        .iter()
        .filter(|e| in_scope(model, e) && filter.is_none_or(|s| involves(e, s)))
        .map(|e| act_row(model, e))
        .collect();
    rows.extend(
        model
            .facts
            .approval_items
            .iter()
            .filter(|a| filter.is_none_or(|s| s.short.as_deref() == Some(a.short.as_str())))
            .map(|a| ask_row(model, a)),
    );
    rows.extend(
        ctx.view
            .local_rows
            .iter()
            .filter(|l| filter.is_none_or(|s| s.short.as_deref() == Some(l.to_short.as_str())))
            .map(|l| ActRow {
                ts: l.ts,
                key: l.key(),
                from: l.from.clone(),
                from_c: c::YOU,
                glyph: '\u{2713}',
                glyph_c: c::OK,
                to: who(model, &l.to_short),
                to_c: c::SEAT,
                text: l.text.clone(),
                text_c: c::FG,
                sel: model.sel_for_short(&l.to_short).filter(|s| *s != Sel::Seat),
            }),
    );
    rows.sort_by_key(|r| r.ts);
    rows
}

/// How far the activity can scroll back: until its oldest row is the first one shown.
pub(super) fn activity_max_scroll(model: &Model, view: &TreeView) -> usize {
    let sel = model.resolve(&view.selected);
    let ctx = Ctx {
        model,
        view,
        now: view.now_ms,
        wall: model.facts.now,
        sel: &sel,
        hover: None,
        hover_key: None,
    };
    let subj = subject(model, &sel);
    let filter = view.filter_selected.then_some(&subj);
    activity_rows(&ctx, filter).len().saturating_sub(ACT_LINES)
}

fn activity(s: &mut Scene, ctx: &Ctx, y: i32, w: i32) {
    let model = ctx.model;
    let subj = subject(model, ctx.sel);
    let filter = ctx.view.filter_selected.then_some(&subj);
    s.grid.boxed(1, y, w - 2, ACT_H, c::RULE, Some(c::BG));
    let tx = s.grid.bold(3, y, " ACTIVITY ", c::DIM);
    let sub = if filter.is_some() {
        format!("\u{b7} {} only ", cut(&subj.name, 24))
    } else {
        "\u{b7} how they work together ".to_string()
    };
    s.grid.text(tx, y, &sub, c::FAINT);
    let toggle = if filter.is_some() {
        " A show everyone "
    } else {
        " A only the selected agent "
    };
    let tw = theme::width(toggle);
    s.grid.text(w - 3 - tw, y, toggle, c::FAINT);
    s.reg(w - 3 - tw, y, tw, 1, Some(Act::Press(KeyCode::Char('A'))));
    s.activity = (2, y + 1, w - 4, ACT_LINES as i32);
    let rows = activity_rows(ctx, filter);
    if rows.is_empty() {
        let text = if !model.data.loaded {
            "gathering\u{2026}".to_string()
        } else if filter.is_some() {
            format!("no activity for {} yet", subj.name)
        } else {
            "no activity yet".to_string()
        };
        s.grid.text(3, y + 1, &text, c::FAINT);
        return;
    }
    let end = rows.len().saturating_sub(
        ctx.view
            .act_scroll
            .min(rows.len().saturating_sub(ACT_LINES)),
    );
    let from = end.saturating_sub(ACT_LINES);
    for (i, row) in rows[from..end].iter().enumerate() {
        let yy = y + 1 + i as i32;
        let age = ctx
            .view
            .motion
            .rows
            .get(&row.key)
            .map_or(u64::MAX, |b| ctx.now.saturating_sub(*b));
        if age < super::fx::ROW_FADE_MS {
            s.grid.fill(
                2,
                yy,
                w - 4,
                1,
                mix(c::SEL, c::BG, age as f64 / super::fx::ROW_FADE_MS as f64),
            );
        }
        let mut x = s.grid.text(3, yy, &hm(model, row.ts), c::FAINT) + 2;
        s.grid.text_on(
            x,
            yy,
            &cut(&row.from, 22),
            row.from_c,
            None,
            row.from == "you",
        );
        x += 24;
        s.grid.text(x, yy, "\u{2500}\u{2500}", c::RULE);
        s.grid
            .put(x + 2, yy, row.glyph, Some(row.glyph_c), None, true);
        s.grid.text(x + 3, yy, "\u{2500}\u{2500}\u{25b6}", c::RULE);
        x += 8;
        s.grid.text(x, yy, &cut(&row.to, 22), row.to_c);
        x += 24;
        s.grid
            .text(x, yy, &cut(&row.text, (w - 4 - x).max(0)), row.text_c);
        if let Some(sel) = row.sel.clone() {
            let r = s.reg(2, yy, w - 4, 1, Some(Act::Select(sel.clone())));
            r.node = None;
        }
    }
    if end < rows.len() {
        let t = format!(" \u{2193} {} newer ", rows.len() - end);
        s.grid
            .text(w - 3 - theme::width(&t), y + ACT_H - 1, &t, c::DIM);
    }
}

// -- Key bar, help and toast ----------------------------------------------------------------

fn key_bar(s: &mut Scene, ctx: &Ctx, w: i32, y: i32) {
    let model = ctx.model;
    // An armed stop is shown here whatever the SELECTED panel previews, so `y` is never a surprise.
    if let Some(target) = &ctx.view.confirm_stop {
        let q = format!("stop {}? ", cut(&name_of(model, target), 24));
        let after = s.grid.text_on(2, y, &q, c::WARN, None, true);
        let after = s.grid.bold(after, y, "y", c::HI);
        let after = s.grid.text(after, y, " stop \u{b7} ", c::DIM);
        let after = s.grid.bold(after, y, "n", c::HI);
        s.grid.text(after, y, " keep", c::DIM);
        return;
    }
    let sel = ctx.sel.clone();
    let pane = model.selected_pane(&sel).is_some();
    let pending = s.shown_approvals.first().cloned();
    let press = |code: KeyCode| Some(Act::Press(code));
    // (priority, gap before, chip): a lower priority is kept longer; a chip is whole or absent.
    let mut all: Vec<(u8, i32, Chip)> = Vec::new();
    // Jev has no harness: there is nothing for Enter to open.
    if sel != Sel::Jev {
        all.push((
            1,
            0,
            Chip::new("\u{23ce}", "Open", Some(Act::Open(sel.clone()))),
        ));
    }
    all.push((
        2,
        2,
        Chip::new(
            "\u{2190} \u{2192} \u{2191} \u{2193}",
            "Select",
            press(KeyCode::Right),
        ),
    ));
    if mail_target(model, &sel).is_some() {
        all.push((4, 2, Chip::new("m", "Message", press(KeyCode::Char('m')))));
    }
    if pane {
        all.push((5, 2, Chip::new("n", "Nudge", press(KeyCode::Char('n')))));
        all.push((5, 2, Chip::new("x", "Stop", press(KeyCode::Char('x')))));
    }
    all.push((
        6,
        2,
        Chip::new("A", "Activity filter", press(KeyCode::Char('A'))),
    ));
    all.push((7, 2, Chip::new("s", "Scope", press(KeyCode::Char('s')))));
    all.push((7, 2, Chip::new("+", "New agent", press(KeyCode::Char('+')))));
    if let Some(p) = &pending {
        let on = |ok: bool, code: char| ok.then_some(Act::Press(KeyCode::Char(code)));
        // The chips answer the request that is drawn, whoever is selected.
        let asker = model.sel_for_short(&p.short);
        all.push((
            1,
            4,
            Chip::new("y", "Allow", on(p.answerable, 'y'))
                .kc(c::OK)
                .id("bar-y")
                .node(asker.clone()),
        ));
        if p.always {
            all.push((
                1,
                2,
                Chip::new("a", "Always", on(true, 'a'))
                    .id("bar-a")
                    .node(asker.clone()),
            ));
        }
        all.push((
            1,
            2,
            Chip::new("d", "Deny", on(p.answerable, 'd'))
                .kc(c::ERR)
                .id("bar-d")
                .node(asker),
        ));
    }
    if let Some(r) = s.shown_rulings.first() {
        all.push((
            1,
            4,
            Chip::new("o", "Override", Some(Act::Press(KeyCode::Char('o'))))
                .id("bar-o")
                .node(model.sel_for_short(&r.short)),
        ));
    }
    all.push((
        2,
        4,
        Chip::new("?", "All keys", Some(Act::Help)).id("bar-help"),
    ));
    all.push((
        0,
        2,
        Chip::new("^A t", "Classic dashboard", Some(Act::Classic)),
    ));
    let budget = w - 2;
    let mut keep = vec![true; all.len()];
    let used = |keep: &[bool], all: &[(u8, i32, Chip)]| -> i32 {
        all.iter()
            .zip(keep)
            .filter(|(_, k)| **k)
            .map(|((_, gap, chip), _)| gap + chip.width())
            .sum()
    };
    while used(&keep, &all) > budget {
        let worst = all
            .iter()
            .zip(&keep)
            .filter(|(_, k)| **k)
            .map(|((p, _, _), _)| *p)
            .max()
            .unwrap_or(0);
        if worst == 0 {
            break;
        }
        match all
            .iter()
            .enumerate()
            .rev()
            .find(|(i, (p, _, _))| keep[*i] && *p == worst)
        {
            Some((at, _)) => keep[at] = false,
            None => break,
        }
    }
    let mut x = 1;
    let mut first = true;
    for ((_, gap, chip), keep) in all.into_iter().zip(keep) {
        if !keep {
            continue;
        }
        if !first {
            x += gap;
        }
        first = false;
        if x + chip.width() > w {
            break;
        }
        x = s.chip(ctx, x, y, chip);
    }
    let hint = "hover an agent to preview it \u{b7} click one to open its harness \u{b7} click a request's buttons to answer it";
    s.grid.text(2, y + 1, &cut(hint, w - 3), c::FAINT);
}

const HELP: [(&str, &str); 13] = [
    (
        "click or \u{23ce}",
        "open the agent's own harness (its pane chat)",
    ),
    ("hover", "preview an agent in the right panel"),
    (
        "\u{2190} \u{2192} \u{2191} \u{2193}",
        "move between the agents",
    ),
    (
        "y  a  d",
        "allow once, always allow, or deny the request shown",
    ),
    ("o", "override the supervisor ruling NEEDS YOU draws"),
    ("m  n  x", "message, nudge or stop the selected agent"),
    ("A", "activity for the selected agent only, or for everyone"),
    ("s", "scope: this dashboard, this repo, or every session"),
    ("+", "spawn a new agent"),
    ("esc", "back to the classic dashboard"),
    (
        "^A t",
        "in a harness: back to the flow; in the flow: classic",
    ),
    (
        "^A \u{2190} \u{2192}",
        "in a harness: the previous or next agent's chat",
    ),
    (
        "wheel",
        "scroll the activity, or the agent rows over the flow",
    ),
];

fn help_box(s: &mut Scene, w: i32, h: i32) {
    let bw = 76.min(w - 2);
    let bh = (HELP.len() as i32 + 4).min(h - 2);
    let (x, y) = ((w - bw) / 2, ((h - bh) / 2).max(1));
    s.grid.boxed(x, y, bw, bh, c::SEAT, Some(c::PANEL));
    s.grid.bold(x + 2, y, " KEYS AND MOUSE ", c::SEAT);
    for (i, (k, v)) in HELP.iter().enumerate() {
        let yy = y + 2 + i as i32;
        if yy >= y + bh - 2 {
            break;
        }
        s.grid.bold(x + 3, yy, k, c::HI);
        s.grid.text(x + 18, yy, &cut(v, bw - 21), c::FG);
    }
    s.grid.text(
        x + 3,
        y + bh - 2,
        "Press any key or click to close",
        c::FAINT,
    );
}

fn toast(s: &mut Scene, ctx: &Ctx, (fx, fy, fw, fh): (i32, i32, i32, i32)) {
    let Some(t) = ctx.view.motion.toast.as_ref() else {
        return;
    };
    if ctx.now.saturating_sub(t.born) >= super::fx::TOAST_MS {
        return;
    }
    let msg = format!(" {} ", cut(&t.msg, fw - 8));
    let x = fx + fw - 3 - theme::width(&msg);
    s.grid
        .text_on(x, fy + fh - 1, &msg, t.col, Some(c::RAISE), true);
}

// -- Entry points ---------------------------------------------------------------------------

/// The dashboard's scene for `area`, or `None` below the orchestrator tier.
pub(super) fn build(area: Rect, model: &Model, view: &TreeView, selected: &Sel) -> Option<Scene> {
    if !fits(area.width, area.height) {
        return None;
    }
    let (w, h) = (i32::from(area.width), i32::from(area.height));
    // An armed stop names the selected node, so the panel must show that node, not a hover preview.
    let hover = view
        .hover
        .as_ref()
        .filter(|hv| model.resolve(hv) == **hv && view.confirm_stop.is_none());
    let ctx = Ctx {
        model,
        view,
        now: view.now_ms,
        wall: model.facts.now,
        sel: selected,
        hover,
        hover_key: view.hover_key.as_deref(),
    };
    let mut s = Scene::new(area.width, area.height);
    s.grid.clear_page();
    let items = needs(model);
    let (flow_rect, right) = layout(w, h);
    header(&mut s, &ctx, w, items.len());
    stepper(&mut s, &ctx, w);
    flow::draw(&mut s, &ctx, flow_rect);
    right_col(&mut s, &ctx, &items, right, flow_rect.3);
    activity(&mut s, &ctx, TOP + flow_rect.3 + 1, w);
    key_bar(&mut s, &ctx, w, h - KEY_ROWS);
    toast(&mut s, &ctx, flow_rect);
    if let Some(error) = view.error.as_deref() {
        let line = cut(error, 30);
        let x = flow_rect.0 + flow_rect.2 - 3 - theme::width(&line);
        s.grid.text(x, TOP + 1, &line, c::DIM);
    }
    if view.help {
        help_box(&mut s, w, h);
    }
    Some(s)
}

/// Copy a scene into the terminal buffer.
pub(super) fn paint(buf: &mut Buffer, area: Rect, scene: &Scene, truecolor: bool) {
    scene.grid.blit(buf, (area.x, area.y), truecolor);
}

// -- The open chat --------------------------------------------------------------------------

/// A node the chat view can move between: the seat, then each top-level agent.
fn chat_nodes<'a>(model: &Model<'a>) -> Vec<Sel> {
    std::iter::once(Sel::Seat)
        .chain(model.agents.iter().map(|a| Sel::Agent(a.node.id.clone())))
        .collect()
}

/// The pane short id of the chat that is open.
pub(super) fn focused_short<'a>(facts: &'a TreeFacts<'_>) -> Option<&'a str> {
    facts.focused.as_ref().map(|(short, _, _)| short.as_str())
}

/// The chat before or after the open one among the nodes that have a pane, wrapping.
pub(super) fn neighbor_pane(model: &Model, delta: isize) -> Option<String> {
    let panes: Vec<(String, Sel)> = chat_nodes(model)
        .into_iter()
        .filter_map(|sel| model.selected_pane(&sel).map(|short| (short, sel)))
        .collect();
    if panes.len() < 2 {
        return None;
    }
    let open = focused_short(model.facts)?;
    let at = panes.iter().position(|(short, _)| short == open)? as isize;
    let next = (at + delta).rem_euclid(panes.len() as isize) as usize;
    Some(panes[next].0.clone())
}

/// Which node the open chat belongs to: the subagent the operator clicked when its host's chat
/// is the one showing, else whatever owns the focused pane.
fn open_node<'a>(model: &Model<'a>, view: &TreeView) -> Option<Sel> {
    let focused = focused_short(model.facts)?;
    if let Some(opened) = view.opened.as_ref()
        && let Some(node) = model.node(opened)
        && model.host(node).is_some_and(|h| h.short == focused)
        && model.pane_short(node).is_none()
    {
        return Some(opened.clone());
    }
    model.sel_for_short(focused)
}

/// The chat view's chrome: the header and stepper, the bar naming the open agent, and below the
/// pane the others strip and the key bar. Only these rows are painted; the pane owns the rest.
pub(super) fn build_chat(area: Rect, model: &Model, view: &TreeView) -> Option<Scene> {
    let (top, bottom) = chat_chrome((area.width, area.height));
    if bottom == 0 {
        return None;
    }
    let (w, h) = (i32::from(area.width), i32::from(area.height));
    let facts = model.facts;
    let sel = Sel::Seat;
    let hover = view.hover.as_ref();
    let ctx = Ctx {
        model,
        view,
        now: view.now_ms,
        wall: facts.now,
        sel: &sel,
        hover,
        hover_key: view.hover_key.as_deref(),
    };
    let mut s = Scene::new(area.width, area.height);
    let items = needs(model);
    for y in 0..i32::from(top) {
        s.grid.fill(0, y, w, 1, c::BG);
    }
    header(&mut s, &ctx, w, items.len());
    stepper(&mut s, &ctx, w);
    // The bar.
    let bar_y = i32::from(top) - 1;
    s.grid.fill(0, bar_y, w, 1, c::PANEL);
    let open = open_node(model, view);
    let subj = open.as_ref().map(|sel| subject(model, sel));
    let node = subj.as_ref().and_then(|s| s.node);
    let is_seat = subj.as_ref().is_some_and(|s| s.sel == Sel::Seat);
    let (name, color, st, model_name, place) = match (&subj, node) {
        (Some(_), Some(n)) => {
            let place = match model.host(n).filter(|_| model.pane_short(n).is_none()) {
                Some(h) => format!("runs inside {}", h.name),
                None => where_line(model, n),
            };
            (
                model.job_of(n),
                if is_seat { c::SEAT } else { c::AGENT },
                flow::status(model, n),
                content::node_model(n),
                place,
            )
        }
        (Some(_), None) => {
            let pane = facts
                .pane_meta
                .iter()
                .find(|m| Some(m.short.as_str()) == focused_short(facts))
                .map_or(String::new(), |m| {
                    format!("pane {} \u{b7} {}", m.number, m.worktree)
                });
            (
                "seat".to_string(),
                c::SEAT,
                St::Running,
                content::seat_title(facts),
                pane,
            )
        }
        _ => (
            facts
                .focused
                .as_ref()
                .map_or("agent".to_string(), |(_, t, _)| content::clean(t)),
            c::AGENT,
            St::Running,
            facts
                .focused
                .as_ref()
                .map_or(String::new(), |(_, _, a)| content::clean(a)),
            String::new(),
        ),
    };
    let back = " \u{2039} Flow ";
    let bw = theme::width(back);
    let hov = ctx.hover_key == Some("back");
    s.grid.text_on(
        2,
        bar_y,
        back,
        c::HI,
        Some(if hov { c::CHIP_HI } else { c::CHIP }),
        true,
    );
    let r = s.reg(2, bar_y, bw, 1, Some(Act::BackToFlow));
    r.hk = Some("back".into());
    let mut x = s.grid.text(2 + bw, bar_y, "/ ", c::DIM);
    let (g, gc) = flow::glyph(st, ctx.now);
    s.grid.put(x, bar_y, g, Some(gc), None, true);
    x = s.grid.bold(x + 2, bar_y, &short_name(&name, 34), color);
    for part in [place.as_str(), model_name.as_str()] {
        if part.is_empty() {
            continue;
        }
        x = s.grid.text(x, bar_y, " \u{b7} ", c::FAINT);
        x = s.grid.text(
            x,
            bar_y,
            &cut(part, 30),
            if part == place { c::DIM } else { color },
        );
    }
    let tail = "^A t back to the flow";
    s.grid.text(w - 2 - theme::width(tail), bar_y, tail, c::DIM);

    // The others strip.
    let strip_y = h - 3;
    s.grid.fill(0, strip_y, w, 3, c::BG);
    let mut x = s.grid.text(2, strip_y, "others ", c::FAINT);
    let end = w - 2;
    let count = items.len();
    let reserve = if count > 0 {
        theme::width(&format!("\u{2691} {count} need you")) + 3
    } else {
        0
    };
    let mut entries: Vec<(Sel, String, Rgb, char, Rgb, String)> = Vec::new();
    for other in chat_nodes(model) {
        if Some(&other) == open.as_ref() {
            continue;
        }
        let o = subject(model, &other);
        let asks = items.iter().any(|n| {
            n.sel.as_ref() == Some(&other)
                && matches!(n.kind, NeedKind::Approval(_) | NeedKind::Wait { .. })
        });
        let (st, note) = match o.node {
            Some(n) if asks || model.waiting(n) => (St::Waiting, " asks you".to_string()),
            None if asks => (St::Waiting, " asks you".to_string()),
            Some(n) => {
                let st = flow::status(model, n);
                let note = match st {
                    St::Failed => " failed".to_string(),
                    St::Running => content::node_elapsed(n, facts.now)
                        .map(|e| format!(" {e}"))
                        .unwrap_or_default(),
                    _ => String::new(),
                };
                (st, note)
            }
            None => (St::Running, String::new()),
        };
        let (g, gc) = if other == Sel::Seat {
            ('\u{25cf}', c::SEAT)
        } else {
            flow::glyph(st, ctx.now)
        };
        let nc = match st {
            St::Failed => c::ERR,
            St::Waiting => c::WARN,
            _ => c::DIM,
        };
        entries.push((other, o.name, gc, g, nc, note));
    }
    let mut shown = 0;
    for (other, name, gc, g, nc, note) in &entries {
        let label = format!(" {} {}{} ", g, cut(name, 16), note);
        let lw = theme::width(&label);
        if x + lw + 1 + reserve > end {
            break;
        }
        let hov = ctx.hover == Some(other);
        s.grid.text_on(
            x,
            strip_y,
            &label,
            c::FG,
            Some(if hov { c::RAISE } else { c::PANEL }),
            false,
        );
        s.grid.put(x + 1, strip_y, *g, Some(*gc), None, true);
        let note_x = x + 3 + theme::width(&cut(name, 16));
        s.grid.text(note_x, strip_y, note, *nc);
        let r = s.reg(x, strip_y, lw, 1, Some(Act::Open(other.clone())));
        r.node = Some(other.clone());
        x += lw + 1;
        shown += 1;
    }
    if shown < entries.len() {
        s.grid
            .text(x, strip_y, &format!("+{}", entries.len() - shown), c::DIM);
    }
    if count > 0 {
        let t = format!(
            "\u{2691} {count} need{} you",
            if count == 1 { "s" } else { "" }
        );
        s.grid.bold(end - theme::width(&t), strip_y, &t, c::WARN);
    }
    // The key bar.
    let mut cx = 1;
    let mut chips = vec![
        Chip::new("^A t", "Back to the flow", Some(Act::BackToFlow)).id("chat-back"),
        Chip::new("^A \u{2190} \u{2192}", "Previous / next chat", None).hint(),
    ];
    if facts.approvals > 0 {
        chips.push(Chip::new("^A y", "Allow", None).hint());
        chips.push(Chip::new("^A d", "Deny", None).hint());
    }
    for chip in chips {
        if cx + chip.width() > w {
            break;
        }
        cx = s.chip(&ctx, cx, h - 2, chip) + 2;
    }
    let hint = "click a name in the others strip to switch to its chat \u{b7} click \u{2039} Flow to go back";
    s.grid.text(2, h - 1, &cut(hint, w - 3), c::FAINT);
    Some(s)
}

#[cfg(test)]
mod tests {
    use super::super::super::super::approvals::Decision;
    use super::super::super::super::jev_feed;
    use super::super::keys::Outcome;
    use super::super::plan::Surface;
    use super::super::testkit::*;
    use super::super::*;
    use super::*;
    use crossterm::event::{KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
    use ratatui::style::{Color, Modifier};
    use std::time::Instant;

    fn area(w: u16, h: u16) -> Surface {
        Surface::page(Rect::new(0, 0, w, h))
    }

    fn press(view: &mut TreeView, f: &TreeFacts, w: u16, h: u16, code: KeyCode) -> Outcome {
        view.key(KeyEvent::new(code, KeyModifiers::NONE), area(w, h), f)
    }

    fn mouse(
        view: &mut TreeView,
        f: &TreeFacts,
        (w, h): (u16, u16),
        kind: MouseEventKind,
        (x, y): (u16, u16),
    ) -> Outcome {
        let event = MouseEvent {
            kind,
            column: x,
            row: y,
            modifiers: KeyModifiers::NONE,
        };
        view.mouse(event, area(w, h), f, Instant::now())
    }

    fn click(view: &mut TreeView, f: &TreeFacts, size: (u16, u16), at: (u16, u16)) -> Outcome {
        mouse(view, f, size, MouseEventKind::Down(MouseButton::Left), at)
    }

    fn row_of(text: &str, needle: &str) -> usize {
        text.lines()
            .position(|l| l.contains(needle))
            .unwrap_or_else(|| panic!("{needle:?} not drawn in:\n{text}"))
    }

    /// The `(column, row)` of the first character of `needle`.
    fn at(text: &str, needle: &str) -> (u16, u16) {
        let y = row_of(text, needle);
        let line = text.lines().nth(y).expect("row");
        let x = line[..line.find(needle).expect("col")].chars().count();
        (x as u16, y as u16)
    }

    /// Like [`at`], but only in the flow (left of the right column).
    fn at_flow(text: &str, needle: &str) -> (u16, u16) {
        for (y, line) in text.lines().enumerate() {
            let flow: String = line.chars().take(108).collect();
            if let Some(b) = flow.find(needle) {
                return (flow[..b].chars().count() as u16, y as u16);
            }
        }
        panic!("{needle:?} not drawn in the flow of:\n{text}")
    }

    /// The rows of the ACTIVITY box and what is under it.
    fn activity_part(text: &str) -> String {
        text.lines()
            .skip_while(|l| !l.contains("ACTIVITY"))
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn model_of<'a>(data: &'a TreeData, f: &'a TreeFacts<'a>) -> Model<'a> {
        Model::build(data, f, super::super::Scope::Dashboard)
    }

    fn scene_of(w: u16, h: u16, v: &TreeView, f: &TreeFacts) -> Scene {
        let model = model_of(&v.data, f);
        let sel = model.resolve(&v.selected);
        build(Rect::new(0, 0, w, h), &model, v, &sel).expect("the orchestrator tier")
    }

    fn fg_of(buffer: &ratatui::buffer::Buffer, text: &str, needle: &str) -> Color {
        let (x, y) = at(text, needle);
        buffer[(x, y)].fg
    }

    fn rgb(c: Rgb) -> Color {
        Color::Rgb(c.0, c.1, c.2)
    }

    #[test]
    fn the_orchestrator_dashboard_takes_the_page_from_100_by_35_and_smaller_sizes_keep_phase_one() {
        assert!(fits(160, 45) && fits(100, 35) && fits(120, 36));
        assert!(
            !fits(100, 34)
                && !fits(100, 33)
                && !fits(100, 32)
                && !fits(100, 31)
                && !fits(99, 45)
                && !fits(80, 24)
        );
        let (data, wf, jev) = busy();
        let f = orch_facts(&wf, &jev);
        let v = view(data);
        for (w, h) in [(160, 45), (120, 36), (100, 35)] {
            let text = draw(w, h, &v, &f);
            for part in ["NEEDS YOU", "SELECTED", "ACTIVITY", "FLOW \u{b7}", "JEV"] {
                assert!(text.contains(part), "{part} at {w}x{h}:\n{text}");
            }
        }
        let small = draw(80, 24, &v, &f);
        assert!(
            small.contains("ZIRV AGENT TREE") && !small.contains("ACTIVITY"),
            "below 100 columns the phase-1 compact page draws:\n{small}"
        );
    }

    #[test]
    fn the_minimum_height_leaves_the_flow_a_row_of_cards_that_fits_its_box() {
        let tightest = |h: i32, strip: bool| {
            let fh = layout(100, h).0.3;
            super::flow::ladder(true)
                .into_iter()
                .map(|p| p.rows_fit(fh, strip))
                .max()
                .expect("profiles")
        };
        assert_eq!(
            tightest(34, true),
            0,
            "100x34 with the strip overruns the FLOW border"
        );
        assert!(!fits(100, 34), "so it draws the phase-1 tiers");
        assert!(
            tightest(35, true) >= 1 && tightest(35, false) >= 1 && fits(100, 35),
            "100x35 fits a card row and the strip inside the border"
        );
    }

    #[test]
    fn the_view_never_uses_the_dim_modifier_and_paints_real_backgrounds() {
        let (data, wf, jev) = busy();
        let f = orch_facts(&wf, &jev);
        let buffer = draw_buffer(160, 45, &view(data), &f);
        let mut panels = 0;
        for y in 0..45 {
            for x in 0..160 {
                let cell = &buffer[(x, y)];
                assert!(!cell.modifier.contains(Modifier::DIM), "DIM at {x},{y}");
                assert!(
                    matches!(cell.bg, Color::Rgb(..)),
                    "{x},{y} has a real background"
                );
                panels += usize::from(cell.bg == rgb(c::PANEL));
            }
        }
        assert!(panels > 200, "the cards and the seat are panels");
        // Rounded corners all round.
        let text = draw(160, 45, &view(busy().0), &f);
        assert!(
            text.contains('\u{256d}') && text.contains('\u{256f}') && !text.contains('\u{250c}')
        );
    }

    #[test]
    fn the_stepper_shows_the_run_and_its_gate_or_says_there_is_no_workflow() {
        let (data, wf, jev) = busy();
        let f = orch_facts(&wf, &jev);
        let v = view(data.clone());
        let buffer = draw_buffer(160, 45, &v, &f);
        let text = draw(160, 45, &v, &f);
        let line = text.lines().nth(1).expect("the stepper row");
        assert!(
            line.contains("WORKFLOW feature \u{b7} Agent tree polish"),
            "{line}"
        );
        assert!(
            line.contains(
                "\u{2713} intent \u{2501}\u{2501}\u{2501} \u{2713} plan \u{2501}\u{2501}\u{2501} \u{25d0} implement \u{2500}\u{2500}\u{2500} \u{25cb} review \u{2500}\u{2500}\u{2500} \u{25cb} verify"
            ),
            "done steps join with a heavy bar, the rest with a light one: {line}"
        );
        assert!(
            line.contains("step 3/5 \u{b7} 11m \u{b7} next gate review"),
            "{line}"
        );
        let fg = |needle: &str| fg_of(&buffer, &text, needle);
        assert_eq!(fg("\u{2713} intent"), rgb(c::OK));
        assert_eq!(fg("\u{25d0} implement"), rgb(c::AGENT));
        assert_eq!(fg("\u{25cb} review"), rgb(c::FAINT));
        let (x, y) = at(&text, "implement");
        assert!(buffer[(x, y)].modifier.contains(Modifier::BOLD));
        // The spinner on the current step turns and the bars shimmer, on the injected clock.
        let mut later = view(data);
        later.now_ms = 250;
        let turned = draw(160, 45, &later, &f);
        assert!(
            turned
                .lines()
                .nth(1)
                .expect("row")
                .contains("\u{25d3} implement"),
            "{turned}"
        );
        let glow = |now: u64| {
            let mut v = view(busy().0);
            v.now_ms = now;
            let b = draw_buffer(160, 45, &v, &f);
            (0..160)
                .filter(|&x| b[(x, 1)].fg == rgb(c::HI) && b[(x, 1)].symbol() == "\u{2501}")
                .count()
        };
        assert!(
            glow(0) + glow(1500) + glow(3000) > 0,
            "a bar glows somewhere along the run"
        );
        let none = draw(160, 45, &view(busy().0), &facts(Some(&jev)));
        assert!(
            none.lines()
                .nth(1)
                .expect("row")
                .contains("No workflow running"),
            "{none}"
        );
    }

    #[test]
    fn a_long_title_gives_way_to_the_steps_and_is_cut_at_a_word() {
        let (data, mut wf, jev) = busy();
        wf.steps = ["understand", "plan", "execute", "validate", "present"]
            .iter()
            .enumerate()
            .map(|(i, s)| {
                (
                    (*s).to_string(),
                    match i {
                        0 | 1 => StepMark::Done,
                        2 => StepMark::Current,
                        _ => StepMark::Pending,
                    },
                )
            })
            .collect();
        wf.pack = "adaptive-work".into();
        wf.next_gate = Some("validate".into());
        wf.title = "Orchestrator dashboard (PR #843): rebuild the agent-tree view as an orchestrator dashboard".into();
        let f = orch_facts(&wf, &jev);
        let text = draw(160, 45, &view(data), &f);
        let line = text.lines().nth(1).expect("the stepper row");
        for step in ["understand", "plan", "execute", "validate", "present"] {
            assert!(line.contains(step), "{step} at 160 columns: {line}");
        }
        let title = line
            .split("adaptive-work \u{b7} ")
            .nth(1)
            .and_then(|t| t.split('\u{2026}').next())
            .expect("a cut title");
        assert!(
            wf.title.starts_with(title.trim_end()) && wf.title[title.len()..].starts_with(' '),
            "cut at a word: {title:?} of {line}"
        );
        let narrow = draw(100, 35, &view(busy().0), &f);
        let line = narrow.lines().nth(1).expect("row");
        assert!(
            line.contains("execute") && line.contains("present"),
            "{line}"
        );
    }

    #[test]
    fn the_header_carries_the_badge_the_cached_usage_and_the_keys_chip_but_no_spend() {
        let (data, wf, jev) = busy();
        let f = orch_facts(&wf, &jev);
        let text = draw(160, 45, &view(data.clone()), &f);
        let head = text.lines().next().expect("header");
        assert!(
            head.contains("zirv-cli  \u{b7}  seat claude fable \u{b7} orchestrator"),
            "{head}"
        );
        for part in ["\u{2691} 4 need you", "usage 5h 34%", "? keys"] {
            assert!(head.contains(part), "{part} in {head}");
        }
        assert!(
            head.find("need you") < head.find("usage") && head.find("usage") < head.find("? keys"),
            "badge, usage, keys: {head}"
        );
        assert!(
            !head.contains("spend") && !head.contains('$'),
            "no cost in the header: {head}"
        );
        // The badge breathes.
        let at_ms = |ms: u64| {
            let mut v = view(data.clone());
            v.now_ms = ms;
            let b = draw_buffer(160, 45, &v, &f);
            let (x, y) = at(&text, "\u{2691} 4");
            b[(x, y)].bg
        };
        assert_ne!(at_ms(0), at_ms(400), "the badge breathes on the clock");
        let mut quiet = orch_facts(&wf, &jev);
        quiet.usage_5h = None;
        quiet.approval_items.clear();
        quiet.waits.clear();
        quiet.stalled.clear();
        let mut calm = data;
        calm.nodes.retain(|n| n.id != "a3");
        let head = draw(160, 45, &view(calm), &quiet);
        let head = head.lines().next().expect("header").trim_end().to_string();
        assert!(head.contains("\u{2713} all clear"), "{head}");
        assert!(
            !head.contains("usage") && !head.contains("need"),
            "an unknown reading is left out, not made up: {head}"
        );
        assert!(head.ends_with("? keys"), "{head}");
    }

    #[test]
    fn a_card_shows_its_status_glyph_job_model_age_and_what_it_is_doing() {
        let (data, wf, jev) = busy();
        let f = orch_facts(&wf, &jev);
        let v = view(data);
        let text = draw(160, 45, &v, &f);
        for part in [
            "\u{280b} reads the code",
            "claude haiku \u{b7} 1m",
            "\u{25b8} working \u{b7} 12k tok",
            "\u{2691} edits + tests for",
            "gpt-6-sol \u{b7} 2m",
            "waiting for you",
            "\u{2717} Plan",
            "failed",
            "\u{2713} general-purpose",
            "finished",
        ] {
            assert!(text.contains(part), "{part} in:\n{text}");
        }
        // Eleven rows tall, with a faint rule under the model line.
        let scene = scene_of(160, 45, &v, &f);
        let card = scene
            .regs
            .iter()
            .find(|r| r.node == Some(Sel::Agent("a1".into())))
            .expect("the card");
        assert_eq!(card.h, 11);
        // A claude model id from a meta file reads as its family.
        let mut data = v.data.clone();
        data.nodes
            .iter_mut()
            .find(|n| n.id == "a1")
            .expect("a1")
            .model = Some("claude-sonnet-5-5".into());
        let text = draw(160, 45, &view(data), &f);
        assert!(
            text.contains("claude sonnet") && !text.contains("claude-so"),
            "{text}"
        );
        // The title is cut at a word, two lines at most.
        let mut data = v.data.clone();
        data.nodes
            .iter_mut()
            .find(|n| n.id == "a2")
            .expect("a2")
            .job = Some("Research mockup source and harness APIs for the tree".into());
        let text = draw(160, 45, &view(data), &f);
        assert!(
            text.contains("Research mockup") && text.contains('\u{2026}'),
            "{text}"
        );
        // The step words.
        let step = |tool: &str, arg: &str| {
            content::step_label(&content::StepView {
                ts: 0,
                tool: tool.into(),
                arg: arg.into(),
            })
        };
        assert_eq!(step("Bash", "cargo build"), "run cargo build");
        assert_eq!(step("Edit", "src/a.rs"), "edit src/a.rs");
        assert_eq!(step("SubagentHandback", ""), "reported back to the seat");
        assert_eq!(step("WebFetch", "x.dev"), "webfetch x.dev");
    }

    #[test]
    fn the_flow_geometry_is_the_prototypes_with_jev_between_the_seat_and_the_bus() {
        let (data, wf, jev) = busy();
        let f = orch_facts(&wf, &jev);
        let v = view(data);
        let text = draw(160, 45, &v, &f);
        let grid: Vec<Vec<char>> = text.lines().map(|l| l.chars().collect()).collect();
        let flow_top = 3usize;
        let seat_top = row_of(&text, "\u{25cf} claude fable") - 1;
        assert_eq!(seat_top, flow_top + 2, "the seat card at FY+2");
        let jev_top = row_of(&text, "\u{25c6} JEV");
        assert_eq!(jev_top, flow_top + 7, "Jev at FY+7");
        let cx = grid[jev_top]
            .iter()
            .position(|&c| c == '\u{2534}')
            .expect("the seat's line meets Jev's top border in a \u{2534}");
        assert_eq!(
            grid[jev_top - 1][cx],
            '\u{2502}',
            "the seat's line comes down into it"
        );
        let jev_bottom = jev_top + 5;
        assert_eq!(
            grid[jev_bottom][cx], '\u{252c}',
            "and leaves its bottom border in a \u{252c}"
        );
        assert_eq!(grid[jev_bottom + 1][cx], '\u{2502}');
        let bus = flow_top + 14;
        assert_eq!(bus, jev_bottom + 2, "the bus at FY+14");
        assert!(
            grid[bus].contains(&'\u{252c}') && grid[bus][cx] == '\u{2534}',
            "{}",
            grid[bus].iter().collect::<String>()
        );
        let cards = row_of(&text, "reads the code") - 1;
        assert_eq!(cards, flow_top + 16, "the cards at FY+16");
        // The box says what it decided and how sure it was.
        let jev_text = grid[jev_top..=jev_bottom]
            .iter()
            .map(|r| r.iter().collect::<String>())
            .collect::<Vec<_>>()
            .join("\n");
        for part in [
            "\u{25c6} JEV \u{b7} decisions",
            "4 sites on \u{b7} 2 calls today",
            "dispatch",
            "claude sonnet",
            "0.87",
            "sure",
            "0.12",
            "unsure",
            "ago",
            "sure \u{2192} zirv applies it",
            "unsure \u{2192} zirv's own rule, or the supervisor",
        ] {
            assert!(jev_text.contains(part), "{part} in:\n{jev_text}");
        }
        let bar = jev_text
            .lines()
            .find(|l| l.contains("0.87"))
            .expect("a row");
        assert!(
            bar.contains(
                "\u{2588}\u{2588}\u{2588}\u{2588}\u{2588}\u{2588}\u{2588}\u{2588}\u{2588}\u{2591}"
            ),
            "a 10-cell bar: {bar}"
        );
        // Rounded ends on the bus: the first and last cards hang from a \u{256d} and a \u{256e}.
        assert!(grid[bus].contains(&'\u{256d}') && grid[bus].contains(&'\u{256e}'));
    }

    #[test]
    fn a_click_on_jev_selects_it_and_opens_nothing_and_enter_says_it_has_no_harness() {
        let (data, wf, jev) = busy();
        let f = orch_facts(&wf, &jev);
        let mut v = view(data);
        let text = draw(160, 45, &v, &f);
        let (x, y) = at(&text, "\u{25c6} JEV");
        assert_eq!(click(&mut v, &f, (160, 45), (x + 4, y + 2)), Outcome::None);
        assert_eq!(v.selected, Sel::Jev);
        let panel = draw(160, 45, &v, &f);
        for part in [
            "SELECTED",
            "TypeSafe decision model",
            "today",
            "2 calls",
            "1 unsure",
            "sites on",
            "memory, dispatch, review, gates",
            "RECENT",
        ] {
            assert!(panel.contains(part), "{part} in:\n{panel}");
        }
        assert!(
            matches!(press(&mut v, &f, 160, 45, KeyCode::Enter), Outcome::Notice(n) if n.contains("no harness")),
            "Enter has nothing to open"
        );
        // Its Open chip is faint and there is no pane to nudge.
        let scene = scene_of(160, 45, &v, &f);
        assert!(!scene.regs.iter().any(|r| r.hk.as_deref() == Some("sel-o")));
        // Hovering Jev previews it; the arrows reach it from the seat.
        let mut w = view(busy().0);
        w.hover = Some(Sel::Jev);
        assert!(draw(160, 45, &w, &f).contains("PREVIEW"));
        let mut a = view(busy().0);
        press(&mut a, &f, 160, 45, KeyCode::Down);
        assert_eq!(a.selected, Sel::Jev);
        press(&mut a, &f, 160, 45, KeyCode::Down);
        assert_eq!(a.selected, Sel::Agent("a1".into()), "then the first card");
        press(&mut a, &f, 160, 45, KeyCode::Up);
        assert_eq!(a.selected, Sel::Jev);
    }

    #[test]
    fn jev_is_off_only_when_no_site_is_on_and_the_proxy_does_not_use_typesafe() {
        let feed = |sites: &[&str], proxy| content::JevFeed {
            rows: Vec::new(),
            sites: sites.iter().map(|s| (*s).to_string()).collect(),
            proxy,
        };
        assert!(!feed(&[], false).on());
        assert!(feed(&["memory"], false).on());
        assert!(feed(&[], true).on(), "the harness proxy alone turns it on");
        let (mut data, wf, jev) = busy();
        let f = orch_facts(&wf, &jev);
        // The facts' own reading is irrelevant: this operator has sites on and `jev` facts absent.
        let mut no_facts = orch_facts(&wf, &jev);
        no_facts.jev = None;
        let on = draw(160, 45, &view(data.clone()), &no_facts);
        assert!(
            on.contains("4 sites on") && !on.contains("JEV \u{b7} off"),
            "{on}"
        );
        data.jev.sites.clear();
        let off = draw(160, 45, &view(data.clone()), &f);
        assert!(
            off.contains("\u{25c6} JEV \u{b7} off") && off.contains("Jev is off"),
            "{off}"
        );
        data.jev.proxy = true;
        let proxy = draw(160, 45, &view(data), &f);
        assert!(
            proxy.contains("proxy on") && !proxy.contains("Jev is off"),
            "{proxy}"
        );
    }

    #[test]
    fn the_approval_card_shows_who_where_the_whole_command_why_and_the_answers() {
        let (data, wf, jev) = busy();
        let mut f = orch_facts(&wf, &jev);
        f.approval_items[0].view = content::ApprovalView {
            command: "cargo nextest run -E 'test(dash::)'".into(),
            cwd: "/work/wt-at-graph".into(),
            reason: "It writes outside the sandbox: the build cache is not in the allowed paths."
                .into(),
            outside_sandbox: true,
            always: None,
        };
        let v = view(data.clone());
        let text = draw(160, 45, &v, &f);
        let buffer = draw_buffer(160, 45, &v, &f);
        for part in [
            "\u{2691} NEEDS YOU",
            "\u{25cf} edits + tests for #839",
            "asks to run this in \u{2026}/wt-at-graph \u{b7} 42s ago",
            "\u{258e} cargo nextest run -E 'test(dash::)'",
            "It writes outside the sandbox:",
            "y Allow once",
            "a Always allow",
            "d Deny",
            "\u{23ce} Open its harness",
            "This command runs outside the sandbox.",
        ] {
            assert!(text.contains(part), "{part} in:\n{text}");
        }
        assert!(!text.contains("1 of"), "one request: no counter");
        let fg = |needle: &str| fg_of(&buffer, &text, needle);
        assert_eq!(fg("Allow once"), rgb(c::FG), "y is on offer");
        assert_eq!(
            fg("Always allow"),
            rgb(c::FAINT),
            "the request does not offer always"
        );
        assert_eq!(fg("cargo"), rgb(c::HI), "the program is bold and bright");
        assert_eq!(fg("-E"), rgb(c::DIM));
        // When it does offer always, a is live and the scope line says what it covers.
        f.approval_items[0].view.always = Some(String::new());
        let text = draw(160, 45, &v, &f);
        let buffer = draw_buffer(160, 45, &v, &f);
        assert_eq!(fg_of(&buffer, &text, "Always allow"), rgb(c::FG));
        assert!(
            text.contains("Always allow stops asking for cargo nextest"),
            "{text}"
        );
        // A second request shows the counter.
        let mut two = orch_facts(&wf, &jev);
        let mut other = two.approval_items[0].clone();
        other.short = "a1".into();
        other.conn = 2;
        two.approval_items.push(other);
        assert!(draw(160, 45, &v, &two).contains("1 of 2"));
    }

    #[test]
    fn a_long_command_wraps_by_column_to_four_lines_and_is_then_not_answerable() {
        let (data, wf, jev) = busy();
        let mut f = orch_facts(&wf, &jev);
        let long = format!("cargo test {}", "--some-flag value ".repeat(14));
        f.approval_items[0].view.command = long.clone();
        let mut v = view(data);
        v.selected = Sel::Agent("w1".into());
        let text = draw(160, 70, &v, &f);
        let lines: Vec<&str> = text.lines().filter(|l| l.contains('\u{258e}')).collect();
        assert_eq!(lines.len(), 4, "{text}");
        assert!(
            lines[3].contains('\u{2026}'),
            "the cut is marked: {}",
            lines[3]
        );
        let first = lines[0].split('\u{258e}').nth(1).expect("text").trim();
        assert!(
            long.starts_with(
                first.trim_end_matches(|c: char| c == '\u{2502}' || c.is_whitespace())
            ),
            "wrapped exactly"
        );
        assert!(text.contains("Not shown in full"), "{text}");
        let buffer = draw_buffer(160, 70, &v, &f);
        assert_eq!(fg_of(&buffer, &text, "Allow once"), rgb(c::FAINT));
        for code in ['y', 'd'] {
            assert!(
                matches!(
                    press(&mut v, &f, 160, 70, KeyCode::Char(code)),
                    Outcome::Notice(_)
                ),
                "{code} cannot answer a command that is cut"
            );
        }
        // Whole and drawn: it is answered, as the node's own request, and the toast says so.
        f.approval_items[0].view.command = "cargo test".into();
        assert_eq!(
            press(&mut v, &f, 160, 70, KeyCode::Char('y')),
            Outcome::AnswerShown {
                short: "w1".into(),
                conn: 1,
                decision: Decision::Allow
            }
        );
        assert!(draw(160, 70, &v, &f).contains("Allowed once."));
        assert_eq!(
            press(&mut v, &f, 160, 70, KeyCode::Char('d')),
            Outcome::AnswerShown {
                short: "w1".into(),
                conn: 1,
                decision: Decision::Deny
            }
        );
        // The answer sends a pulse back down to its agent.
        assert!(
            v.motion
                .pulses
                .iter()
                .any(|p| p.id == "w1" && !p.up && p.glyph == '\u{2717}')
        );
        // Not shown in full: the keys are inert even with a short command.
        f.approval_items[0].fully_shown = false;
        assert!(matches!(
            press(&mut v, &f, 160, 70, KeyCode::Char('y')),
            Outcome::Notice(_)
        ));
        f.approval_items.clear();
        assert_eq!(
            press(&mut v, &f, 160, 70, KeyCode::Char('y')),
            Outcome::Notice("no approval is waiting".into())
        );
        // Always is not on offer yet.
        assert!(matches!(
            press(&mut v, &f, 160, 70, KeyCode::Char('a')),
            Outcome::Notice(_)
        ));
    }

    #[test]
    fn a_wait_with_no_approval_card_draws_its_own_card_that_opens_its_pane() {
        let (data, wf, jev) = busy();
        let mut f = orch_facts(&wf, &jev);
        f.approval_items.clear();
        f.approval_shorts.clear();
        f.approvals = 0;
        f.stalled.clear();
        f.waits = vec![WaitFact {
            short: "a1".into(),
            kind: WaitKind::Permission,
            since: f.now - 70,
            evidence: "Bash: cargo test".into(),
        }];
        let text = draw(160, 45, &view(data), &f);
        let right: String = text
            .lines()
            .map(|l| l.chars().skip(110).collect::<String>() + "\n")
            .collect();
        for part in [
            "\u{2691} NEEDS YOU",
            "needs permission",
            "Bash: cargo test",
            "Open its pane",
            "[approvals] inbox = true",
        ] {
            assert!(right.contains(part), "{part} in:\n{right}");
        }
        f.approvals_inbox = true;
        let (data, ..) = busy();
        let text = draw(160, 45, &view(data), &f);
        assert!(!text.contains("[approvals] inbox = true"), "{text}");
    }

    #[test]
    fn the_rest_of_what_needs_you_is_a_compact_list_oldest_first_and_calm_says_so() {
        let (data, wf, jev) = busy();
        let mut f = orch_facts(&wf, &jev);
        f.retryable = vec!["a3".into()];
        let model = model_of(&data, &f);
        let order: Vec<(String, &str)> = needs(&model)
            .iter()
            .map(|n| {
                let kind = match n.kind {
                    NeedKind::Failed { .. } => "failed",
                    NeedKind::Wait { .. } => "wait",
                    NeedKind::Approval(_) => "approval",
                    NeedKind::Stalled => "stalled",
                    NeedKind::Ruling(_) => "ruling",
                };
                (n.name.clone(), kind)
            })
            .collect();
        assert_eq!(
            order,
            [
                ("Plan".to_string(), "failed"),
                ("seat".to_string(), "wait"),
                ("worker".to_string(), "approval"),
                ("Explore".to_string(), "stalled"),
            ],
            "200s, 70s, 42s, 30s old"
        );
        let text = draw(160, 45, &view(data), &f);
        let right: String = text
            .lines()
            .map(|l| l.chars().skip(110).collect::<String>() + "\n")
            .collect();
        for part in [
            "ALSO NEEDS YOU \u{b7} 3",
            "\u{2717} Plan failed",
            "? seat asks you",
            "pick a layout: outline",
            "\u{25cc} Explore stalled",
            "next gate review \u{b7} after implement",
        ] {
            assert!(right.contains(part), "{part} in:\n{right}");
        }
        let line = |needle: &str| row_of(&right, needle);
        assert!(
            line("Plan failed") < line("seat asks") && line("seat asks") < line("Explore stalled"),
            "oldest first:\n{right}"
        );
        // Nothing to do.
        let (_, wf, jev) = busy();
        let mut calm = facts(Some(&jev));
        calm.workflow = Some(&wf);
        let quiet = draw(160, 45, &view(fixture()), &calm);
        assert!(
            quiet.contains("\u{2713} Nothing needs you right now"),
            "{quiet}"
        );
        // A click on a row selects its agent.
        let mut v = view(busy().0);
        let (x, y) = at(&text, "Plan failed");
        click(&mut v, &f, (160, 45), (x + 2, y));
        assert_eq!(v.selected, Sel::Agent("a3".into()));
    }

    #[test]
    fn selected_shows_the_prototypes_fields_and_hovering_previews_another_node() {
        let (data, wf, jev) = busy();
        let f = orch_facts(&wf, &jev);
        let mut v = view(data);
        let panel = |text: &str| -> String {
            text.lines()
                .map(|l| l.chars().skip(110).collect::<String>() + "\n")
                .collect()
        };
        let seat = panel(&draw(160, 45, &v, &f));
        for part in [
            "SELECTED",
            "Your seat",
            "claude fable \u{b7} orchestrator",
            "workflow feature \u{b7} implement",
            "agents",
            "RECENT",
            "\u{23ce} Open",
            "m Message",
            "n Nudge",
            "x Stop",
        ] {
            assert!(seat.contains(part), "{part} in:\n{seat}");
        }
        v.selected = Sel::Agent("w1".into());
        let agent = panel(&draw(160, 60, &v, &f));
        for part in [
            "SELECTED",
            "\u{2691} edits + tests for #839",
            "waiting for you",
            "model    codex gpt-6-sol",
            "kind     Codex pane",
            "where    pane 5 \u{b7} wt-at-models",
            "step     feature \u{b7} implement",
            "children 3",
            "NOW",
            "RECENT",
        ] {
            assert!(agent.contains(part), "{part} in:\n{agent}");
        }
        // Hover: the panel previews the node under the pointer and the selection stays.
        let seat_card = {
            let text = draw(160, 45, &v, &f);
            at(&text, "\u{25cf} claude fable")
        };
        mouse(
            &mut v,
            &f,
            (160, 45),
            MouseEventKind::Moved,
            (seat_card.0 + 4, seat_card.1),
        );
        assert_eq!(v.hover, Some(Sel::Seat));
        let preview = panel(&draw(160, 45, &v, &f));
        assert!(
            preview.contains("PREVIEW") && preview.contains("Your seat"),
            "{preview}"
        );
        assert_eq!(
            v.selected,
            Sel::Agent("w1".into()),
            "hovering never selects"
        );
        mouse(&mut v, &f, (160, 45), MouseEventKind::Moved, (2, 40));
        assert_eq!(v.hover, None);
        assert!(panel(&draw(160, 45, &v, &f)).contains("SELECTED"));
        // A chip lights under the pointer.
        let text = draw(160, 45, &v, &f);
        let (x, y) = at(&text, "Message");
        mouse(&mut v, &f, (160, 45), MouseEventKind::Moved, (x, y));
        assert_eq!(v.hover_key.as_deref(), Some("sel-m"));
        let b = draw_buffer(160, 45, &v, &f);
        assert_eq!(b[(x, y)].bg, rgb(c::CHIP_HI));
    }

    #[test]
    fn a_chip_whose_action_does_not_apply_is_faint_and_does_nothing() {
        let (data, wf, jev) = busy();
        let mut f = orch_facts(&wf, &jev);
        // The Explore subagent has no pane of its own; it runs inside the seat.
        f.pane_shorts.retain(|s| s != "a1");
        let mut v = view(data);
        v.selected = Sel::Agent("a1".into());
        let text = draw(160, 45, &v, &f);
        let buffer = draw_buffer(160, 45, &v, &f);
        let scene = scene_of(160, 45, &v, &f);
        let hk = |k: &str| scene.regs.iter().any(|r| r.hk.as_deref() == Some(k));
        assert!(hk("sel-o") && hk("sel-m"), "open and message apply");
        assert!(!hk("sel-n") && !hk("sel-x"), "nudge and stop need a pane");
        let panel_row = |needle: &str| {
            let y = text
                .lines()
                .enumerate()
                .position(|(y, l)| {
                    y > 3 && l.chars().skip(110).collect::<String>().contains(needle)
                })
                .expect(needle);
            let line = text.lines().nth(y).expect("row");
            let x = line
                .chars()
                .skip(110)
                .collect::<String>()
                .find(needle)
                .expect("col");
            (110 + x as u16, y as u16)
        };
        let (nx, ny) = panel_row("Nudge");
        assert_eq!(buffer[(nx, ny)].fg, rgb(c::FAINT));
        assert_eq!(buffer[(nx, ny)].bg, rgb(c::PANEL));
        let (mx, my) = panel_row("Message");
        assert_eq!(buffer[(mx, my)].fg, rgb(c::FG));
        for (x, y) in [panel_row("Nudge"), panel_row("Stop")] {
            assert_eq!(click(&mut v, &f, (160, 45), (x + 1, y)), Outcome::None);
            assert_eq!(v.confirm_stop, None, "a faint Stop asks nothing");
        }
        assert_eq!(v.selected, Sel::Agent("a1".into()));
        // The keys give a reason instead.
        assert!(matches!(
            press(&mut v, &f, 160, 45, KeyCode::Char('x')),
            Outcome::Notice(_)
        ));
        assert!(matches!(
            press(&mut v, &f, 160, 45, KeyCode::Char('n')),
            Outcome::Notice(_)
        ));
        // The key bar leaves the keys out too.
        let bar = text.lines().nth(43).expect("key bar");
        assert!(
            !bar.contains("Nudge") && !bar.contains("Stop") && bar.contains("Message"),
            "{bar}"
        );
    }

    #[test]
    fn one_click_on_a_pane_agent_the_seat_or_a_finished_pill_opens_its_pane_chat() {
        let (mut data, wf, jev) = busy();
        // Make the general-purpose agent old enough to fold into the FINISHED strip.
        data.nodes
            .iter_mut()
            .find(|n| n.id == "a2")
            .expect("a2")
            .ended_at = Some(900);
        let f = orch_facts(&wf, &jev);
        let mut v = view(data);
        let text = draw(160, 45, &v, &f);
        let (x, y) = at_flow(&text, "edits + tests for");
        assert_eq!(
            click(&mut v, &f, (160, 45), (x + 3, y)),
            Outcome::OpenPane("w1".into()),
            "a Codex pane agent"
        );
        assert_eq!(v.selected, Sel::Agent("w1".into()));
        let (x, y) = at_flow(&text, "reads the code");
        assert_eq!(
            click(&mut v, &f, (160, 45), (x + 3, y)),
            Outcome::OpenPane("a1".into()),
            "a Claude pane agent"
        );
        let (x, y) = at_flow(&text, "\u{25cf} claude fable");
        assert_eq!(
            click(&mut v, &f, (160, 45), (x, y)),
            Outcome::OpenPane("seat1".into()),
            "the seat"
        );
        let (px, y) = at(&text, "\u{2713} general-purpose  ");
        assert!(
            text.lines()
                .nth(y as usize)
                .expect("row")
                .contains("FINISHED")
        );
        assert_eq!(
            click(&mut v, &f, (160, 45), (px + 3, y)),
            Outcome::OpenPane("a2".into()),
            "a pill"
        );
        assert_eq!(
            v.opened, None,
            "a pane agent opens its own chat, nothing else is named"
        );
        // Enter does the same for the selection.
        v.selected = Sel::Agent("w1".into());
        assert_eq!(
            press(&mut v, &f, 160, 45, KeyCode::Enter),
            Outcome::OpenPane("w1".into())
        );
    }

    #[test]
    fn a_native_subagent_opens_its_hosts_chat_and_the_bar_names_the_subagent() {
        let (mut data, wf, jev) = busy();
        data.nodes
            .iter_mut()
            .find(|n| n.id == "a1")
            .expect("a1")
            .session = Some("seat-1".into());
        let mut f = orch_facts(&wf, &jev);
        // Explore has no pane: it runs inside the seat's Claude Code.
        f.pane_shorts.retain(|s| s != "a1");
        let mut v = view(data);
        let text = draw(160, 45, &v, &f);
        let (x, y) = at_flow(&text, "reads the code");
        assert_eq!(
            click(&mut v, &f, (160, 45), (x + 3, y)),
            Outcome::OpenPane("seat1".into())
        );
        assert_eq!(v.opened, Some(Sel::Agent("a1".into())));
        // The host's chat is showing: the bar names the subagent and where it runs.
        v.chat = true;
        f.focused = Some(("seat1".into(), "seat".into(), "claude".into()));
        let chat = draw(160, 45, &v, &f);
        let bar = chat.lines().nth(3).expect("the bar");
        assert!(
            bar.contains("\u{2039} Flow / ")
                && bar.contains("reads the code \u{b7} runs inside seat \u{b7} claude haiku"),
            "{bar}"
        );
        // Another chat showing: the bar names that one instead.
        f.focused = Some(("w1".into(), "worker".into(), "codex".into()));
        let other = draw(160, 45, &v, &f);
        assert!(
            other.lines().nth(3).expect("bar").contains("pane 5"),
            "{}",
            other.lines().nth(3).expect("bar")
        );
        // Mail goes to the host, started for the subagent.
        v.chat = false;
        v.selected = Sel::Agent("a1".into());
        assert_eq!(
            press(&mut v, &f, 160, 45, KeyCode::Char('m')),
            Outcome::MailSubagent {
                to: "claude".into(),
                body: "For your subagent reads the code: ".into()
            }
        );
        // An agent with a pane is mailed itself.
        v.selected = Sel::Agent("w1".into());
        assert_eq!(
            press(&mut v, &f, 160, 45, KeyCode::Char('m')),
            Outcome::Mail { to: "codex".into() }
        );
        // ‹ Flow goes back.
        v.chat = true;
        v.selected = Sel::Seat;
        f.focused = Some(("seat1".into(), "seat".into(), "claude".into()));
        let back = at(&draw(160, 45, &v, &f), "\u{2039} Flow");
        let down = MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: back.0 + 2,
            row: back.1,
            modifiers: KeyModifiers::NONE,
        };
        assert_eq!(v.chat_mouse(down, &f), Some(Outcome::BackToFlow));
    }

    #[test]
    fn a_subagent_whose_host_has_no_pane_here_gets_a_notice_naming_the_host() {
        let (mut data, wf, jev) = busy();
        data.nodes
            .iter_mut()
            .find(|n| n.id == "a1")
            .expect("a1")
            .session = Some("seat-1".into());
        let mut f = orch_facts(&wf, &jev);
        f.pane_shorts.retain(|s| s != "a1" && s != "seat1");
        let mut v = view(data);
        let text = draw(160, 45, &v, &f);
        let (x, y) = at_flow(&text, "reads the code");
        let out = click(&mut v, &f, (160, 45), (x + 3, y));
        assert_eq!(
            out,
            Outcome::Notice(
                "reads the code runs inside seat, which has no pane on this dashboard".into()
            )
        );
        assert_eq!(v.opened, None);
        assert!(
            draw(160, 45, &v, &f).contains("runs inside seat, which has no pane"),
            "the toast shows it"
        );
    }

    #[test]
    fn each_wired_action_hands_the_existing_action_to_the_event_loop() {
        let (data, wf, jev) = busy();
        let mut f = orch_facts(&wf, &jev);
        f.retryable = vec!["a3".into()];
        let mut v = view(data);
        v.selected = Sel::Agent("w1".into());
        let key = |v: &mut TreeView, c: char| press(v, &f, 160, 45, KeyCode::Char(c));
        assert_eq!(key(&mut v, 'n'), Outcome::Nudge { short: "w1".into() });
        assert_eq!(key(&mut v, '+'), Outcome::Spawn);
        v.selected = Sel::Agent("a3".into());
        assert_eq!(key(&mut v, 'r'), Outcome::Retry { short: "a3".into() });
        v.selected = Sel::Agent("a2".into());
        assert!(
            matches!(key(&mut v, 'r'), Outcome::Notice(_)),
            "only a retryable pane relaunches"
        );
        v.selected = Sel::Child("c1".into());
        assert_eq!(
            key(&mut v, 'n'),
            Outcome::Notice("explorer has no pane to nudge".into())
        );
        assert!(matches!(key(&mut v, 'x'), Outcome::Notice(_)));
        assert_eq!(v.confirm_stop, None);
        // The chips and the key bar do what their keys do.
        v.selected = Sel::Seat;
        let text = draw(160, 45, &v, &f);
        let (nx, ny) = {
            let y = text
                .lines()
                .enumerate()
                .position(|(y, l)| y > 3 && y < 40 && l.contains("Nudge"))
                .expect("the chip");
            let line = text.lines().nth(y).expect("row");
            (
                line[..line.find("Nudge").expect("col")].chars().count() as u16,
                y as u16,
            )
        };
        assert_eq!(
            click(&mut v, &f, (160, 45), (nx, ny)),
            Outcome::Nudge {
                short: "seat1".into()
            }
        );
        let classic = at(&text, "Classic dashboard");
        assert_eq!(click(&mut v, &f, (160, 45), classic), Outcome::Leave);
        let help = at(&text, "? keys");
        click(&mut v, &f, (160, 45), help);
        assert!(v.help);
    }

    #[test]
    fn stop_asks_first_and_n_or_anything_else_cancels() {
        let (data, wf, jev) = busy();
        let f = orch_facts(&wf, &jev);
        let mut v = view(data);
        v.selected = Sel::Agent("w1".into());
        assert_eq!(
            press(&mut v, &f, 160, 45, KeyCode::Char('x')),
            Outcome::None
        );
        assert_eq!(v.confirm_stop, Some(Sel::Agent("w1".into())));
        assert!(draw(160, 45, &v, &f).contains("stop worker? y / n"));
        assert_eq!(
            press(&mut v, &f, 160, 45, KeyCode::Char('n')),
            Outcome::None
        );
        assert_eq!(v.confirm_stop, None, "n cancels");
        press(&mut v, &f, 160, 45, KeyCode::Char('x'));
        assert_eq!(press(&mut v, &f, 160, 45, KeyCode::Esc), Outcome::None);
        assert_eq!(v.confirm_stop, None, "Esc cancels and does not leave");
        press(&mut v, &f, 160, 45, KeyCode::Char('x'));
        assert_eq!(
            press(&mut v, &f, 160, 45, KeyCode::Char('y')),
            Outcome::Stop { short: "w1".into() }
        );
        press(&mut v, &f, 160, 45, KeyCode::Char('x'));
        click(&mut v, &f, (160, 45), (60, 30));
        assert_eq!(v.confirm_stop, None, "a click elsewhere cancels it too");
        // At 100 columns too.
        press(&mut v, &f, 100, 35, KeyCode::Char('x'));
        assert!(draw(100, 35, &v, &f).contains("stop worker? y / n"));
    }

    #[test]
    fn an_armed_stop_is_visible_even_while_the_pointer_hovers_another_card() {
        let (data, wf, jev) = busy();
        let f = orch_facts(&wf, &jev);
        let mut v = view(data);
        v.selected = Sel::Agent("w1".into());
        v.hover = Some(Sel::Jev);
        press(&mut v, &f, 160, 45, KeyCode::Char('x'));
        assert_eq!(v.confirm_stop, Some(Sel::Agent("w1".into())));
        let text = draw(160, 45, &v, &f);
        assert!(text.contains("stop worker? y / n"), "panel prompt:\n{text}");
        assert!(
            text.contains("stop worker? y stop"),
            "key bar prompt:\n{text}"
        );
    }

    #[test]
    fn a_filters_the_activity_to_the_selected_node_and_clicking_a_row_selects_its_agent() {
        let (data, wf, jev) = busy();
        let f = orch_facts(&wf, &jev);
        let mut v = view(data);
        v.selected = Sel::Agent("w1".into());
        let all = draw(160, 45, &v, &f);
        assert!(
            all.contains("ACTIVITY \u{b7} how they work together") && all.contains("retry or stop"),
            "{all}"
        );
        press(&mut v, &f, 160, 45, KeyCode::Char('A'));
        let only = draw(160, 45, &v, &f);
        assert!(only.contains("ACTIVITY \u{b7} worker only"), "{only}");
        let activity = only
            .lines()
            .skip_while(|l| !l.contains("ACTIVITY"))
            .take(8)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            activity.contains("run the migration tests before review")
                && activity.contains("asks to run cargo"),
            "the worker's mail and its ask:\n{activity}"
        );
        assert!(!activity.contains("retry or stop"), "{activity}");
        press(&mut v, &f, 160, 45, KeyCode::Char('A'));
        // A click on a row about an agent selects it.
        let mut v = view(busy().0);
        let text = draw(160, 45, &v, &f);
        let (x, y) = {
            let top = at(&text, "ACTIVITY").1;
            let (x, dy) = at(&activity_part(&text), "started");
            (x, top + dy)
        };
        click(&mut v, &f, (160, 45), (x, y));
        assert_eq!(
            v.selected,
            Sel::Agent("a1".into()),
            "the Explore dispatch row"
        );
        // The header toggle.
        let (x, y) = at(&text, "A only the selected agent");
        click(&mut v, &f, (160, 45), (x + 2, y));
        assert!(v.filter_selected);
    }

    #[test]
    fn activity_shows_connections_with_their_glyphs_and_a_new_row_fades_in() {
        let (mut data, wf, jev) = busy();
        let mut advice = event(1_236, "supervisor", "supervisor", "split the work", None);
        advice.to = Some("seat1".into());
        data.events.push(advice);
        data.events.push(event(
            1_237,
            "someone-elses-session",
            "jev",
            "hook restart forward",
            None,
        ));
        let f = orch_facts(&wf, &jev);
        let mut v = view(data);
        let text = draw(160, 45, &v, &f);
        let rows = text
            .lines()
            .skip_while(|l| !l.contains("ACTIVITY"))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            !rows.contains("hook restart forward"),
            "an unknown session is not in scope:\n{rows}"
        );
        assert!(
            rows.contains("\u{bb}") && rows.contains("split the work"),
            "supervisor advice:\n{rows}"
        );
        assert!(
            rows.contains("\u{25c6}") && rows.contains("retry or stop"),
            "{rows}"
        );
        assert!(
            rows.contains("\u{2500}\u{2500}\u{25c6}\u{2500}\u{2500}\u{25b6}"),
            "the connector: {rows}"
        );
        // A row born at 1000 ms is lit at 1200 and back to the page at 3000.
        let key = super::super::fx::event_key(&v.data.events[v.data.events.len() - 2]);
        v.motion.rows.insert(key, 1_000);
        let (x, y) = {
            let top = at(&text, "ACTIVITY").1;
            let below = activity_part(&text);
            let (x, dy) = at(&below, "split the work");
            (x, top + dy)
        };
        v.now_ms = 1_200;
        assert_ne!(draw_buffer(160, 45, &v, &f)[(x, y)].bg, rgb(c::BG));
        v.now_ms = 1_000 + super::super::fx::ROW_FADE_MS;
        assert_eq!(draw_buffer(160, 45, &v, &f)[(x, y)].bg, rgb(c::BG));
    }

    #[test]
    fn the_wheel_over_the_activity_box_scrolls_it_and_never_past_the_oldest_row() {
        let (mut data, wf, jev) = busy();
        for i in 0..10u64 {
            data.events.push(event(
                1_300 + i,
                "seat-1",
                "decision",
                &format!("older {i}"),
                None,
            ));
            data.events.last_mut().expect("event").kind = "dispatch".into();
        }
        let f = orch_facts(&wf, &jev);
        let mut v = view(data);
        let wheel = |v: &mut TreeView, kind, y| mouse(v, &f, (160, 45), kind, (60, y));
        assert!(activity_part(&draw(160, 45, &v, &f)).contains("older 9"));
        wheel(&mut v, MouseEventKind::ScrollUp, 38);
        wheel(&mut v, MouseEventKind::ScrollUp, 38);
        assert_eq!(v.act_scroll, 2);
        let text = activity_part(&draw(160, 45, &v, &f));
        assert!(
            !text.contains("older 9") && text.contains("older 7") && text.contains("2 newer"),
            "{text}"
        );
        wheel(&mut v, MouseEventKind::ScrollDown, 38);
        assert_eq!(v.act_scroll, 1);
        for _ in 0..500 {
            wheel(&mut v, MouseEventKind::ScrollUp, 38);
        }
        let max = activity_max_scroll(&model_of(&v.data, &f), &v);
        assert_eq!(v.act_scroll, max);
        // Over the flow the wheel scrolls the agent rows, not the log.
        let before = v.act_scroll;
        wheel(&mut v, MouseEventKind::ScrollDown, 20);
        assert_eq!(v.act_scroll, before);
    }

    #[test]
    fn a_click_with_the_help_overlay_open_only_closes_it_and_any_key_does_too() {
        let (data, wf, jev) = busy();
        let f = orch_facts(&wf, &jev);
        let mut v = view(data);
        v.selected = Sel::Agent("w1".into());
        press(&mut v, &f, 160, 45, KeyCode::Char('?'));
        assert!(v.help);
        assert!(draw(160, 45, &v, &f).contains("KEYS AND MOUSE"));
        assert_eq!(click(&mut v, &f, (160, 45), (60, 15)), Outcome::None);
        assert!(!v.help);
        assert_eq!(v.selected, Sel::Agent("w1".into()));
        press(&mut v, &f, 160, 45, KeyCode::Char('?'));
        assert_eq!(
            press(&mut v, &f, 160, 45, KeyCode::Char('z')),
            Outcome::None
        );
        assert!(!v.help, "any key closes it");
    }

    #[test]
    fn the_key_bar_shows_the_answer_keys_only_while_a_request_is_pending_and_never_cuts_a_chip() {
        let (data, wf, jev) = busy();
        let f = orch_facts(&wf, &jev);
        let v = view(data.clone());
        for width in [100u16, 110, 120, 139, 140, 160, 200] {
            let text = draw(width, 40, &v, &f);
            let bar = text.lines().rev().nth(1).expect("the key row").to_string();
            assert!(bar.contains("^A t Classic dashboard"), "{width}: {bar}");
            for chip in [
                "m Message",
                "n Nudge",
                "x Stop",
                "A Activity filter",
                "s Scope",
                "+ New agent",
                "y Allow",
                "d Deny",
            ] {
                let key = chip.split(' ').next().expect("key");
                let cut = bar.contains(&format!(" {key} ")) && !bar.contains(chip);
                assert!(!cut, "{width}: {chip:?} is cut in {bar:?}");
            }
            assert!(
                text.lines()
                    .last()
                    .expect("hint row")
                    .contains("hover an agent"),
                "a hint row under it"
            );
        }
        let mut calm = orch_facts(&wf, &jev);
        calm.approval_items.clear();
        let bar = draw(160, 45, &v, &calm)
            .lines()
            .rev()
            .nth(1)
            .expect("bar")
            .to_string();
        assert!(!bar.contains(" y ") && !bar.contains(" d "), "{bar}");
        let bar = draw(160, 45, &v, &f)
            .lines()
            .rev()
            .nth(1)
            .expect("bar")
            .to_string();
        assert!(
            bar.contains("y Allow") && bar.contains("d Deny") && !bar.contains(" a Always"),
            "{bar}"
        );
    }

    fn chat_view(data: TreeData) -> TreeView {
        let mut v = view(data);
        v.chat = true;
        v
    }

    #[test]
    fn the_open_chat_keeps_the_header_and_has_a_flow_button_the_agent_and_the_others() {
        let (data, wf, jev) = busy();
        let mut f = orch_facts(&wf, &jev);
        f.focused = Some(("w1".into(), "worker".into(), "codex".into()));
        let v = chat_view(data);
        assert_eq!(v.chat_rows((160, 45), 0), 4);
        assert_eq!(v.chat_bottom_rows((160, 45), 0), 3);
        assert_eq!(
            (v.chat_rows((80, 24), 0), v.chat_bottom_rows((80, 24), 0)),
            (1, 0)
        );
        let text = draw(160, 45, &v, &f);
        let lines: Vec<&str> = text.lines().collect();
        assert!(
            lines[0].contains("zirv") && lines[1].contains("WORKFLOW"),
            "{text}"
        );
        assert!(
            lines[3].contains("\u{2039} Flow / \u{2691} edits + tests for #839 \u{b7} pane 5 \u{b7} wt-at-models \u{b7} codex gpt-6-sol")
                && lines[3].contains("^A t back to the flow"),
            "{}",
            lines[3]
        );
        let others = lines[42];
        for part in [
            "others",
            "\u{25cf} seat",
            "Explore",
            "general-purpose",
            "Plan failed",
        ] {
            assert!(others.contains(part), "{part} in {others:?}");
        }
        assert!(
            !others.contains("worker"),
            "the open one is not among the others"
        );
        assert!(
            lines[43].contains("^A t Back to the flow")
                && lines[43].contains("^A \u{2190} \u{2192}")
        );
        assert!(lines[44].contains("click"));
        // Only the chrome is painted: the rows the pane owns are left alone.
        let buffer = draw_buffer(160, 45, &v, &f);
        assert_eq!(buffer[(60, 20)].symbol(), " ");
        assert_eq!(buffer[(60, 20)].bg, Color::Reset);
    }

    #[test]
    fn a_click_in_the_others_strip_opens_that_chat_and_flow_goes_back() {
        let (data, wf, jev) = busy();
        let mut f = orch_facts(&wf, &jev);
        f.focused = Some(("w1".into(), "worker".into(), "codex".into()));
        let mut v = chat_view(data);
        let text = draw(160, 45, &v, &f);
        let strip = text.lines().nth(42).expect("strip").to_string();
        let col = |name: &str| {
            strip
                .find(name)
                .map(|b| strip[..b].chars().count() as u16)
                .expect(name)
        };
        let mouse = |x: u16, y: u16| MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: x,
            row: y,
            modifiers: KeyModifiers::NONE,
        };
        assert_eq!(
            v.chat_mouse(mouse(col("Explore") + 2, 42), &f),
            Some(Outcome::OpenPane("a1".into()))
        );
        assert_eq!(
            v.chat_mouse(mouse(col("general-purpose") + 2, 42), &f),
            Some(Outcome::OpenPane("a2".into()))
        );
        assert_eq!(
            v.chat_mouse(mouse(col("seat") + 1, 42), &f),
            Some(Outcome::OpenPane("seat1".into()))
        );
        let back = at(&text, "\u{2039} Flow");
        assert_eq!(
            v.chat_mouse(mouse(back.0 + 3, back.1), &f),
            Some(Outcome::BackToFlow)
        );
        assert_eq!(
            v.chat_mouse(mouse(60, 20), &f),
            None,
            "the pane keeps its own clicks"
        );
        let scroll = MouseEvent {
            kind: MouseEventKind::ScrollUp,
            ..mouse(col("Explore"), 42)
        };
        assert_eq!(v.chat_mouse(scroll, &f), None);
        // Hovering the strip lights a name.
        let moved = MouseEvent {
            kind: MouseEventKind::Moved,
            ..mouse(col("Explore") + 2, 42)
        };
        assert_eq!(v.chat_mouse(moved, &f), Some(Outcome::None));
        assert_eq!(v.hover, Some(Sel::Agent("a1".into())));
    }

    #[test]
    fn caret_arrows_step_to_the_previous_and_next_agents_chat() {
        let (data, wf, jev) = busy();
        let mut f = orch_facts(&wf, &jev);
        f.focused = Some(("a1".into(), "Explore".into(), "claude".into()));
        let v = chat_view(data);
        assert_eq!(v.chat_step(&f, 1), Outcome::OpenPane("w1".into()));
        assert_eq!(v.chat_step(&f, -1), Outcome::OpenPane("seat1".into()));
        f.focused = Some(("seat1".into(), "seat".into(), "claude".into()));
        assert_eq!(
            v.chat_step(&f, -1),
            Outcome::OpenPane("a2".into()),
            "wraps from the seat to the last agent"
        );
        f.focused = Some(("a2".into(), "general-purpose".into(), "claude".into()));
        assert_eq!(v.chat_step(&f, 1), Outcome::OpenPane("seat1".into()));
        f.pane_shorts.retain(|s| s != "w1");
        f.focused = Some(("a1".into(), "Explore".into(), "claude".into()));
        assert_eq!(
            v.chat_step(&f, 1),
            Outcome::OpenPane("a3".into()),
            "an agent without a pane is skipped"
        );
    }

    #[test]
    fn the_approvals_strip_stays_out_of_the_dashboard_and_stays_in_everything_else() {
        let mut v = TreeView::default();
        assert!(
            !v.hides_approvals_strip((160, 45)),
            "hidden tree: classic dashboard"
        );
        v.toggle();
        assert!(v.hides_approvals_strip((160, 45)) && v.hides_approvals_strip((100, 35)));
        assert!(
            !v.hides_approvals_strip((99, 45)),
            "below 100 columns the strip stays"
        );
        assert!(
            !v.hides_approvals_strip((120, 24)),
            "and in a short terminal"
        );
        v.chat = true;
        assert!(!v.hides_approvals_strip((160, 45)), "an open chat keeps it");
    }

    #[test]
    fn many_agents_step_into_rows_that_scroll_and_only_the_first_row_is_fed_from_above() {
        let (_, wf, jev) = busy();
        let f = orch_facts(&wf, &jev);
        let mut v = view(huge());
        let text = draw(160, 45, &v, &f);
        let scene = scene_of(160, 45, &v, &f);
        assert!(
            scene.metrics.total_rows >= 3 && scene.metrics.vis_rows >= 1,
            "{:?}",
            scene.metrics
        );
        assert!(
            text.contains("below") && text.contains("0 above"),
            "the rest scroll:\n{text}"
        );
        let grid: Vec<Vec<char>> = text.lines().map(|l| l.chars().collect()).collect();
        let fans: Vec<usize> = (0..grid.len())
            .filter(|&y| grid[y].iter().filter(|&&c| c == '\u{25bc}').count() >= 4)
            .collect();
        if fans.len() >= 2 {
            let first_bus: String = grid[fans[0] - 1].iter().collect();
            assert!(
                first_bus.contains('\u{2534}') || first_bus.contains('\u{253c}'),
                "{first_bus}"
            );
            for &y in &fans[1..] {
                let bus: String = grid[y - 1].iter().collect();
                assert!(
                    !bus.contains(['\u{2534}', '\u{253c}', '\u{251c}', '\u{2524}']),
                    "a later bus claims no line from above: {bus}"
                );
                assert!(bus.contains('\u{252c}'), "{bus}");
            }
        }
        // The wheel scrolls the rows over the flow, and the arrows bring a hidden card into view.
        let before = scene.metrics.first_row;
        mouse(&mut v, &f, (160, 45), MouseEventKind::ScrollDown, (40, 20));
        assert_eq!(v.scroll, before + 1);
        v.scroll = 0;
        let order = scene.metrics.order.clone();
        v.selected = Sel::Agent(order.last().expect("a last card").clone());
        press(&mut v, &f, 160, 45, KeyCode::Left);
        let after = scene_of(160, 45, &v, &f);
        let Sel::Agent(now) = v.selected.clone() else {
            panic!("an agent is selected: {:?}", v.selected)
        };
        assert_eq!(now, order[order.len() - 2], "Left steps back one card");
        let row = after
            .metrics
            .order
            .iter()
            .position(|id| *id == now)
            .unwrap_or(0)
            / after.metrics.per_row;
        assert!(
            row >= after.metrics.first_row
                && row < after.metrics.first_row + after.metrics.vis_rows,
            "the selected card's row was scrolled in: {:?}",
            after.metrics
        );
    }

    #[test]
    fn a_pulse_is_at_the_same_cell_for_the_same_clock_and_walks_the_bus_down_to_its_card() {
        let (data, wf, jev) = busy();
        let f = orch_facts(&wf, &jev);
        let mut v = view(data);
        let lane = scene_of(160, 45, &v, &f)
            .metrics
            .lanes
            .iter()
            .find(|l| l.0 == "w1")
            .expect("w1 is on the first row")
            .1
            .clone();
        // The lane starts under the seat, skips the Jev rows, runs along the bus into the card.
        assert_eq!(lane[0].0, lane.iter().map(|p| p.0).next().expect("x"));
        let jev_rows = (10, 15);
        assert!(
            lane.iter().all(|&(_, y)| y < jev_rows.0 || y > jev_rows.1),
            "{lane:?}"
        );
        assert_eq!(
            *lane.last().expect("end"),
            (lane.last().expect("end").0, 18),
            "ends on the arrow row under the bus"
        );
        v.motion.pulses.push(super::super::fx::Pulse {
            id: "w1".into(),
            up: false,
            glyph: '\u{2709}',
            col: c::SEAT,
            born: 1_000,
            dur: 1_000,
        });
        for (now, expect_frac) in [(1_000u64, 0.0f64), (1_500, 0.5), (1_900, 0.9)] {
            v.now_ms = now;
            let head = flow::pulse_head(lane.len(), 1_000, 1_000, now);
            assert_eq!(
                head,
                ((expect_frac * lane.len() as f64).floor() as usize).min(lane.len() - 1)
            );
            let buffer = draw_buffer(160, 45, &v, &f);
            let (x, y) = lane[head];
            assert_eq!(
                buffer[(x as u16, y as u16)].symbol(),
                "\u{2709}",
                "at {now} ms the head is on {:?}",
                (x, y)
            );
            assert_eq!(buffer[(x as u16, y as u16)].fg, rgb(c::SEAT));
        }
        // Coming back up (a finish or an ask) runs the same cells the other way.
        v.motion.pulses[0].up = true;
        v.now_ms = 1_000;
        let buffer = draw_buffer(160, 45, &v, &f);
        let (x, y) = *lane.last().expect("end");
        assert_eq!(buffer[(x as u16, y as u16)].symbol(), "\u{2709}");
        // After its duration it is gone.
        v.now_ms = 2_000;
        let buffer = draw_buffer(160, 45, &v, &f);
        assert_ne!(buffer[(x as u16, y as u16)].symbol(), "\u{2709}");
    }

    #[test]
    fn the_orchestrator_view_redraws_about_15_times_a_second_only_while_it_shows() {
        let mut v = TreeView::default();
        assert_eq!(
            v.frame_interval(),
            None,
            "hidden: the classic dashboard's tick is untouched"
        );
        v.toggle();
        let every = v.frame_interval().expect("visible");
        assert!(
            every >= std::time::Duration::from_millis(60)
                && every <= std::time::Duration::from_millis(70)
        );
        v.open_chat();
        assert_eq!(v.frame_interval(), None, "an open chat draws with the pane");
        v.close_chat();
        v.toggle();
        assert_eq!(v.frame_interval(), None);
    }

    #[test]
    fn a_new_event_from_a_later_gather_starts_motion_that_the_frame_then_draws_from_the_clock() {
        let (data, wf, jev) = busy();
        let f = orch_facts(&wf, &jev);
        let mut v = view(data.clone());
        v.rev = 1;
        v.observe_at(&f, 100);
        assert!(v.motion.pulses.is_empty());
        let mut next = data;
        let mut decided = event(
            1_239,
            "seat-1",
            "jev",
            "dispatch tier -> claude sonnet",
            Some(0.93),
        );
        decided.to = None;
        next.events.push(decided);
        next.jev.rows.push(content::JevRow {
            id: "jev|1239|dispatch tier|claude sonnet".into(),
            ts: 1_239,
            site: "dispatch tier".into(),
            text: "claude sonnet".into(),
            confidence: 0.93,
            sure: true,
            cached: false,
        });
        v.data = next;
        v.rev = 2;
        v.observe_at(&f, 5_000);
        assert!(
            v.motion
                .pulses
                .iter()
                .any(|p| p.id == "jev" && p.glyph == '\u{25c6}'),
            "{:?}",
            v.motion.pulses
        );
        assert_eq!(v.motion.jev_flash, Some(5_000));
        // The Jev border flashes and settles back.
        let flashing = {
            v.now_ms = 5_100;
            draw_buffer(160, 45, &v, &f)
        };
        v.now_ms = 5_000 + 1_500;
        let settled = draw_buffer(160, 45, &v, &f);
        let text = draw(160, 45, &v, &f);
        let (x, y) = at(&text, "\u{25c6} JEV");
        assert_ne!(
            flashing[(x - 3, y)].fg,
            settled[(x - 3, y)].fg,
            "the border flashes"
        );
    }

    #[test]
    fn the_gathers_jev_feed_becomes_rows_oldest_first_and_activity_shows_its_plain_words() {
        let feed = jev_feed::JevFeed {
            decisions: vec![
                jev_feed::JevDecision {
                    ts: 1_238,
                    site: "memory".into(),
                    text: "reused a cached answer".into(),
                    confidence: 0.42,
                    sure: true,
                    cost_usd: 0.0,
                    cached: true,
                },
                jev_feed::JevDecision {
                    ts: 1_180,
                    site: "dispatch".into(),
                    text: "send it to a sonnet agent".into(),
                    confidence: 0.87,
                    sure: true,
                    cost_usd: 0.002,
                    cached: false,
                },
            ],
            enabled: vec!["memory", "dispatch"],
        };
        let converted = content::jev_feed_of(feed, false);
        let rows: Vec<(&str, u64, bool)> = converted
            .rows
            .iter()
            .map(|r| (r.site.as_str(), r.ts, r.cached))
            .collect();
        assert_eq!(rows, [("dispatch", 1_180, false), ("memory", 1_238, true)]);
        assert_eq!(converted.sites, ["memory", "dispatch"]);
        let (mut data, wf, jev) = busy();
        data.jev = converted;
        data.events
            .push(event(1_238, "seat-1", "jev", "memory (cached)", Some(0.42)));
        let f = orch_facts(&wf, &jev);
        let text = draw(160, 45, &view(data), &f);
        let activity = activity_part(&text);
        assert!(
            activity.contains("memory: reused a cached answer \u{b7} 0.42 sure"),
            "{activity}"
        );
        assert!(
            !activity.contains("p 0.42")
                && !activity.contains("sharp")
                && !activity.contains("(cached)"),
            "{activity}"
        );
        assert!(
            text.contains("reused a cached answer"),
            "the Jev box shows it too:\n{text}"
        );
    }

    fn ruling(id: &str) -> super::super::super::super::supervisor::Ruling {
        use super::super::super::super::supervisor::{Ruling, RulingKind, RulingStatus};
        Ruling {
            id: id.into(),
            session: "seat-1".into(),
            workflow: None,
            kind: RulingKind::Done,
            verdict: "not_done".into(),
            reason: "The tests for the changed module were never run, so the work is not done yet."
                .into(),
            ts: 1_200,
            status: RulingStatus::Open,
            blocks: 0,
            override_reason: None,
            lapse_reason: None,
            refusals: 0,
        }
    }

    #[test]
    fn an_open_supervisor_ruling_is_a_card_and_o_overrides_it_with_a_toast_and_an_activity_row() {
        let (mut data, wf, jev) = busy();
        data.rulings = vec![ruling("r1")];
        let mut f = orch_facts(&wf, &jev);
        f.approval_items.clear();
        let mut v = view(data);
        let text = draw(160, 60, &v, &f);
        for part in [
            "\u{bb} SUPERVISOR \u{b7} done",
            "\u{25cf} seat \u{b7} not done",
            "The tests for the changed module",
            "40s",
            "o Override",
            "\u{23ce} Open the seat",
        ] {
            assert!(text.contains(part), "{part} in:\n{text}");
        }
        assert!(
            text.lines().next().expect("header").contains("need"),
            "it counts as needing you"
        );
        let short = sessions::short_id("seat-1");
        assert_eq!(
            press(&mut v, &f, 160, 60, KeyCode::Char('o')),
            Outcome::OverrideRuling {
                id: "r1".into(),
                short: short.clone()
            }
        );
        // The chip does the same.
        let (x, y) = at(&text, "o Override");
        assert_eq!(
            click(&mut v, &f, (160, 60), (x + 4, y)),
            Outcome::OverrideRuling {
                id: "r1".into(),
                short: short.clone()
            }
        );
        let (x, y) = at(&text, "Open the seat");
        assert_eq!(
            click(&mut v, &f, (160, 60), (x, y)),
            Outcome::OpenPane("seat1".into())
        );
        // The dashboard recorded the override.
        v.override_done("r1", &short);
        let after = draw(160, 60, &v, &f);
        assert!(
            !after.contains("SUPERVISOR"),
            "the ruling is gone at once:\n{after}"
        );
        assert!(
            after.contains("Override recorded. The seat can carry on."),
            "{after}"
        );
        let activity = activity_part(&after);
        assert!(
            activity.contains("you") && activity.contains("overrode the supervisor's ruling"),
            "{activity}"
        );
        assert!(matches!(
            press(&mut v, &f, 160, 60, KeyCode::Char('o')),
            Outcome::Notice(_)
        ));
    }

    #[test]
    fn always_allow_is_offered_only_when_the_request_names_a_rule_and_carries_the_decision() {
        let (data, wf, jev) = busy();
        let mut f = orch_facts(&wf, &jev);
        let mut v = view(data);
        assert!(
            matches!(press(&mut v, &f, 160, 60, KeyCode::Char('a')), Outcome::Notice(n) if n.contains("does not offer")),
            "no rule, no always"
        );
        f.approval_items[0].view.always = Some("cargo nextest".into());
        let text = draw(160, 60, &v, &f);
        assert!(
            text.contains("Always allow stops asking for cargo nextest"),
            "{text}"
        );
        assert_eq!(
            press(&mut v, &f, 160, 60, KeyCode::Char('a')),
            Outcome::AnswerShown {
                short: "w1".into(),
                conn: 1,
                decision: Decision::AllowAlways
            }
        );
        assert!(draw(160, 60, &v, &f).contains("Allowed, and the rule is applied."));
        let (x, y) = at(&text, "Always allow");
        assert_eq!(
            click(&mut view(busy().0), &f, (160, 60), (x + 1, y)),
            Outcome::AnswerShown {
                short: "w1".into(),
                conn: 1,
                decision: Decision::AllowAlways
            },
            "the chip answers the drawn request, whoever is selected"
        );
    }

    #[test]
    fn activity_and_the_seats_recent_name_agents_by_their_job_not_their_type() {
        let (data, wf, jev) = busy();
        let f = orch_facts(&wf, &jev);
        let text = draw(160, 45, &view(data), &f);
        let activity = activity_part(&text);
        assert!(
            activity.contains("\u{25cf}\u{2500}\u{2500}\u{25b6}  reads the code"),
            "{activity}"
        );
        assert!(!activity.contains("Explore"), "{activity}");
        let tall = draw(160, 70, &view(busy().0), &f);
        let right: String = tall
            .lines()
            .map(|l| l.chars().skip(110).collect::<String>() + "\n")
            .collect();
        assert!(
            right.contains("\u{2192} reads the code") && !right.contains("Explore  "),
            "{right}"
        );
    }

    #[test]
    fn an_agents_real_steps_show_as_now_and_recent_on_its_card() {
        use super::super::super::super::graph::Step;
        let (mut data, wf, jev) = busy();
        let step = |ts, tool: &str, arg: &str| Step {
            ts,
            tool: tool.into(),
            arg: arg.into(),
        };
        data.nodes
            .iter_mut()
            .find(|n| n.id == "a1")
            .expect("a1")
            .steps = vec![
            step(1_190, "Read", "src/a.rs"),
            step(1_200, "Edit", "src/a.rs"),
            step(1_235, "Bash", "cargo build"),
        ];
        let f = orch_facts(&wf, &jev);
        let text = draw(160, 45, &view(data), &f);
        for part in [
            "\u{25b8} run cargo build",
            "40s edit src/a.rs",
            "50s read src/a.rs",
        ] {
            assert!(text.contains(part), "{part} in:\n{text}");
        }
        let request = super::super::super::super::approvals::Request {
            id: "i".into(),
            short: "w1".into(),
            tool: "Bash".into(),
            preview: "cargo build".into(),
            ts: 1,
            dash_pid: 1,
            released: false,
            nonce: 0,
            fully_shown: true,
            command: "cargo build --release".into(),
            cwd: Some("/work/wt".into()),
            reason: Some("Builds the release binary.".into()),
            outside_sandbox: true,
            always: Some("cargo build".into()),
        };
        let view = content::approval_view(&request);
        assert_eq!(
            (
                view.command.as_str(),
                view.cwd.as_str(),
                view.reason.as_str(),
                view.outside_sandbox,
                view.always.as_deref()
            ),
            (
                "cargo build --release",
                "/work/wt",
                "Builds the release binary.",
                true,
                Some("cargo build")
            )
        );
    }

    #[test]
    fn a_motion_event_changes_the_hover_signature_only_when_the_target_does() {
        let (data, wf, jev) = busy();
        let f = orch_facts(&wf, &jev);
        let mut v = view(data);
        let text = draw(160, 45, &v, &f);
        let (x, y) = at_flow(&text, "reads the code");
        let moved = |v: &mut TreeView, at: (u16, u16)| {
            let before = v.hover_signature();
            mouse(v, &f, (160, 45), MouseEventKind::Moved, at);
            v.hover_signature() != before
        };
        assert!(moved(&mut v, (x + 2, y)), "onto a card");
        assert!(
            !moved(&mut v, (x + 4, y + 1)),
            "inside the same card: nothing to redraw"
        );
        assert!(moved(&mut v, (2, 40)), "off it");
        assert!(!moved(&mut v, (3, 40)));
        assert!(v.wants_hover((160, 45)) && !v.wants_hover((99, 45)));
        v.open_chat();
        assert!(!v.wants_hover((160, 45)), "a chat does not ask for motion");
    }
}
