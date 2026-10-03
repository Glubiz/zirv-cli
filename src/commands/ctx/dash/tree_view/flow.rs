//! The FLOW panel of the orchestrator dashboard: the seat card, the bus, one card per agent and
//! the FINISHED strip, with the light that runs down the bus to working agents and the pulses for
//! what just happened. Ported from the approved prototype; the agent order and the scrolling of
//! many rows are the tree's own (see [`super::model`]).

use super::super::super::graph::Node;
use super::content::{self, Mark, node_steps, step_label};
use super::fx::{DISPATCH_FLASH_MS, DONE_FLASH_MS};
use super::model::{Agent, Model, Sel};
use super::scene::{Act, Ctx, Scene, cut, short_name, wrap};
use super::theme::{Rgb, breathe, c, mix, spin};

const MIN_W: i32 = 18;
const MAX_W: i32 = 30;
const GAP: i32 = 2;
/// A finished agent stays a card for this long, then folds into the FINISHED strip.
const FOLD_SECS: u64 = 240;
/// Idle agents past this many (newest first) fold into the strip instead of taking a card each.
const IDLE_CARDS: usize = 4;
const FULL_CARD: i32 = 11;
const MIN_CARD: i32 = 7;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum St {
    Running,
    Waiting,
    Done,
    Failed,
    Idle,
}

pub(super) fn status(model: &Model, node: &Node) -> St {
    if model.waiting(node) {
        return St::Waiting;
    }
    match Mark::of(&node.status) {
        Mark::Running => St::Running,
        Mark::Done => St::Done,
        Mark::Failed => St::Failed,
        _ => St::Idle,
    }
}

/// A finished agent whose host session is still live: idle, as opposed to not started.
fn parked(node: &Node) -> bool {
    Mark::of(&node.status) == Mark::Idle
}

pub(super) fn glyph(st: St, now: u64) -> (char, Rgb) {
    match st {
        St::Running => (spin(now), c::AGENT),
        St::Waiting => ('⚑', c::WARN),
        St::Done => ('✓', c::OK),
        St::Failed => ('✗', c::ERR),
        St::Idle => ('◌', c::DIM),
    }
}

/// The word under a card's title: how long it has been working, or how long ago it ended.
pub(super) fn age_word(node: &Node, st: St, wall: u64) -> String {
    let since = |t: Option<u64>| content::elapsed_label(wall.saturating_sub(t.unwrap_or(wall)));
    match st {
        St::Done => format!("done {} ago", since(node.ended_at)),
        St::Failed => format!("failed {} ago", since(node.ended_at)),
        St::Idle if node.kind == "supervisor" && node.ended_at.is_none() => "idle".to_string(),
        St::Idle if parked(node) => format!("idle {}", since(node.ended_at)),
        St::Idle => "not started".to_string(),
        _ => content::node_elapsed(node, wall).unwrap_or_default(),
    }
}

/// The NOW line of a card or of SELECTED: what the agent is doing, with its colour.
pub(super) fn now_line(node: &Node, st: St) -> (String, Rgb) {
    // The supervisor's label is its consult budget (`1/3`), shown on every state.
    if node.kind == "supervisor" {
        let budget = node.label.as_deref().unwrap_or_default();
        return match (st, node_steps(node).last()) {
            // A running consult's step carries the moment that fired it as its argument.
            (St::Running, Some(step)) => (
                format!(
                    "\u{25b8} {} \u{b7} {budget}",
                    Some(step.arg.as_str())
                        .filter(|arg| !arg.is_empty())
                        .unwrap_or("consulting")
                ),
                c::FG,
            ),
            (_, Some(_)) => (format!("ruled \u{b7} {budget}"), c::FG),
            _ => (format!("idle \u{b7} {budget}"), c::DIM),
        };
    }
    match st {
        St::Waiting => ("waiting for you".into(), c::WARN),
        St::Done => ("finished".into(), c::OK),
        St::Failed => ("failed".into(), c::ERR),
        St::Idle if parked(node) => ("idle".into(), c::DIM),
        St::Idle => ("not started".into(), c::DIM),
        St::Running => {
            let text = match node_steps(node).last() {
                Some(step) => step_label(step),
                None => node.tokens.map_or_else(
                    || "working".to_string(),
                    |t| format!("working \u{b7} {}", content::tokens_label(t)),
                ),
            };
            (format!("\u{25b8} {text}"), c::FG)
        }
    }
}

/// The last steps before the current one, newest first, with their age in seconds.
pub(super) fn recent(node: &Node, st: St, wall: u64, n: usize) -> Vec<(u64, String)> {
    let steps = node_steps(node);
    let past = if st == St::Running && !steps.is_empty() {
        &steps[..steps.len() - 1]
    } else {
        &steps[..]
    };
    past.iter()
        .rev()
        .take(n)
        .map(|s| (wall.saturating_sub(s.ts), step_label(s)))
        .collect()
}

/// Which agents are cards and which fold into pills: indices into `model.agents`, in their order.
pub(super) fn split(ctx: &Ctx) -> (Vec<usize>, Vec<usize>) {
    let mut cards = Vec::new();
    let mut folded = Vec::new();
    let mut idle_cards = 0;
    for (i, agent) in ctx.model.agents.iter().enumerate() {
        let node = agent.node;
        let old = match status(ctx.model, node) {
            St::Done => node
                .ended_at
                .is_none_or(|end| ctx.wall.saturating_sub(end) >= FOLD_SECS),
            St::Idle if parked(node) => {
                idle_cards += 1;
                idle_cards > IDLE_CARDS
            }
            _ => false,
        };
        if old {
            folded.push(i);
        } else {
            cards.push(i);
        }
    }
    (cards, folded)
}

/// `(working, waiting, finished, failed, idle)` over every top-level agent.
pub(super) fn counts(ctx: &Ctx) -> (usize, usize, usize, usize, usize) {
    let mut n = (0, 0, 0, 0, 0);
    for agent in &ctx.model.agents {
        match status(ctx.model, agent.node) {
            St::Running => n.0 += 1,
            St::Waiting => n.1 += 1,
            St::Done => n.2 += 1,
            St::Failed => n.3 += 1,
            St::Idle if parked(agent.node) => n.4 += 1,
            St::Idle => {}
        }
    }
    n
}

/// `1 working · 1 waiting for you · 1 failed · 3 finished`, in the longest wording that fits
/// `room` columns (`waiting for you` shortens to `waiting`, then `failed` goes, then it is cut).
pub(super) fn counts_line(ctx: &Ctx, room: i32) -> String {
    let (working, waiting, done, failed, idle) = counts(ctx);
    let line = |wait: &str, with_failed: bool| {
        [
            Some(format!("{working} working")),
            (waiting > 0).then(|| format!("{waiting} {wait}")),
            (with_failed && failed > 0).then(|| format!("{failed} failed")),
            (idle > 0).then(|| format!("{idle} idle")),
            Some(format!("{done} finished")),
        ]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>()
        .join(" \u{b7} ")
    };
    [
        line("waiting for you", true),
        line("waiting", true),
        line("waiting", false),
    ]
    .into_iter()
    .find(|l| l.chars().count() as i32 <= room)
    .unwrap_or_else(|| cut(&line("waiting", false), room))
}

/// Where a card's profile puts things, from the top of the flow box.
#[derive(Debug, Clone, Copy)]
pub(super) struct Profile {
    /// Blank rows between the box edge and the seat card.
    gap: i32,
    /// The Jev box's height: 6 shows three decisions and its footer, 3 one line.
    jev: i32,
    card: i32,
}

/// The profiles from roomy to tight. A Jev that is off never needs more than 3 rows.
pub(super) fn ladder(jev_on: bool) -> Vec<Profile> {
    let cap = if jev_on { JEV_FULL } else { JEV_OFF };
    let mut out = vec![
        Profile {
            gap: 1,
            jev: cap,
            card: FULL_CARD,
        },
        Profile {
            gap: 0,
            jev: cap,
            card: FULL_CARD,
        },
    ];
    out.extend((JEV_OFF..cap).rev().map(|jev| Profile {
        gap: 0,
        jev,
        card: FULL_CARD,
    }));
    out.extend((MIN_CARD..FULL_CARD).rev().map(|card| Profile {
        gap: 0,
        jev: JEV_OFF,
        card,
    }));
    out
}

const SEAT_H: i32 = 4;
pub(super) const JEV_FULL: i32 = 6;
const JEV_OFF: i32 = 3;
const JEV_W: i32 = 76;

impl Profile {
    /// Rows from the box's top edge to the first card: the border, the seat, a stem, Jev, a stem,
    /// the bus and its arrows.
    fn head(self) -> i32 {
        1 + self.gap + SEAT_H + 1 + self.jev + 1 + 2
    }

    /// Whole rows of cards that fit under it in a box `fh` tall, with a strip row if `strip`.
    pub(super) fn rows_fit(self, fh: i32, strip: bool) -> i32 {
        let need = self.head() + self.card + i32::from(strip) + 1;
        if fh < need {
            return 0;
        }
        1 + (fh - need) / (self.card + 2)
    }
}

/// The columns and rows a bus pulse walks from the seat to one card: down the stem (behind the
/// Jev box, whose rows it skips), along the bus, then into the card.
pub(super) fn path_points(
    seat_x: i32,
    seat_bottom: i32,
    jev: (i32, i32),
    bus_y: i32,
    cx: i32,
) -> Vec<(i32, i32)> {
    let mut pts: Vec<(i32, i32)> = (seat_bottom..bus_y)
        .filter(|y| *y < jev.0 || *y > jev.1)
        .map(|y| (seat_x, y))
        .collect();
    let step = if cx >= seat_x { 1 } else { -1 };
    let mut x = seat_x;
    while x != cx {
        pts.push((x, bus_y));
        x += step;
    }
    pts.push((cx, bus_y));
    pts.push((cx, bus_y + 1));
    pts
}

/// The index of the pulse head along a path of `len` points.
pub(super) fn pulse_head(len: usize, born: u64, dur: u64, now: u64) -> usize {
    let t = now.saturating_sub(born) as f64 / dur.max(1) as f64;
    ((t * len as f64).floor() as usize).min(len.saturating_sub(1))
}

/// Draw the flow into `r = (x, y, w, h)`.
pub(super) fn draw(s: &mut Scene, ctx: &Ctx, r: (i32, i32, i32, i32)) {
    let (fx, fy, fw, fh) = r;
    s.flow = r;
    let (cards, folded) = split(ctx);
    let model = ctx.model;
    s.metrics.order = cards
        .iter()
        .map(|&i| model.agents[i].node.id.clone())
        .collect();
    s.metrics.folded = folded
        .iter()
        .map(|&i| model.agents[i].node.id.clone())
        .collect();
    s.grid.boxed(fx, fy, fw, fh, c::RULE, Some(c::BG));
    let (working, waiting, done, failed, idle) = counts(ctx);
    let mut tx = s.grid.bold(fx + 2, fy, " FLOW ", c::DIM);
    tx = s.grid.text(
        tx,
        fy,
        &format!("\u{b7} {} ", model.scope.label()),
        c::FAINT,
    );
    let head = [
        Some(format!("{working} working")),
        (waiting > 0).then(|| format!("{waiting} waiting")),
        (failed > 0).then(|| format!("{failed} failed")),
        (idle > 0).then(|| format!("{idle} idle")),
        Some(format!("{done} finished")),
    ]
    .into_iter()
    .flatten()
    .collect::<Vec<_>>()
    .join(" \u{b7} ");
    // The counts live on the seat card too, so they are the first thing to give way.
    if tx + (head.chars().count() as i32) + 16 <= fx + fw {
        s.grid.text(tx, fy, &format!("\u{b7} {head} "), c::DIM);
    }
    let scope = " s scope ";
    let sx = fx + fw - 12;
    s.grid.text(sx, fy, scope, c::FAINT);
    s.reg(
        sx,
        fy,
        9,
        1,
        Some(Act::Press(crossterm::event::KeyCode::Char('s'))),
    );

    let cx0 = fx + fw / 2;
    let sw = 50.min(fw - 4);
    let sxl = cx0 - sw / 2;
    let n = cards.len();
    let cap = ((fw - 6 + GAP) / (MIN_W + GAP)).max(1) as usize;
    let total_rows = n.div_ceil(cap);
    let strip = !folded.is_empty();
    let feed = content::jev_feed(model.data);
    let profiles = ladder(feed.on());
    let prof = if n == 0 {
        profiles[0]
    } else {
        profiles
            .iter()
            .copied()
            .find(|p| p.rows_fit(fh, strip) as usize >= total_rows)
            .unwrap_or_else(|| {
                profiles
                    .iter()
                    .copied()
                    .fold(profiles[profiles.len() - 1], |best, p| {
                        if p.rows_fit(fh, strip) > best.rows_fit(fh, strip) {
                            p
                        } else {
                            best
                        }
                    })
            })
    };
    let vis_rows = if n == 0 {
        0
    } else {
        (prof.rows_fit(fh, strip).max(1) as usize).min(total_rows)
    };
    let first_row = ctx.view.scroll.min(total_rows.saturating_sub(vis_rows));
    s.metrics.per_row = cap;
    s.metrics.total_rows = total_rows;
    s.metrics.vis_rows = vis_rows;
    s.metrics.first_row = first_row;

    // The seat card.
    let sy = fy + 1 + prof.gap;
    let sel = ctx.sel == &Sel::Seat;
    let hov = ctx.hover == Some(&Sel::Seat);
    let border = if sel {
        c::SEAT
    } else {
        mix(c::RULE, c::SEAT, if hov { 0.7 } else { 0.4 })
    };
    let bg = if hov || sel { c::RAISE } else { c::PANEL };
    s.grid.boxed(sxl, sy, sw, SEAT_H, border, Some(bg));
    let seat_name = content::seat_title(model.facts);
    let role = model.facts.seat_role.unwrap_or("orchestrator");
    s.grid.center(
        sxl,
        sw,
        sy + 1,
        &[
            ("\u{25cf} ", c::SEAT, false),
            (&cut(&seat_name, sw - 24), c::HI, true),
            (&format!("  \u{b7}  {role}"), c::DIM, false),
        ],
    );
    let line = counts_line(ctx, sw - 4);
    s.grid.center(
        sxl,
        sw,
        sy + 2,
        &[(&line, if waiting > 0 { c::WARN } else { c::DIM }, false)],
    );
    let reg = s.reg(sxl, sy, sw, SEAT_H, Some(Act::Open(Sel::Seat)));
    reg.node = Some(Sel::Seat);
    let seat_bottom = sy + SEAT_H;
    let jy = seat_bottom + 1;
    s.grid
        .put(cx0, seat_bottom, '│', Some(c::RULE), None, false);
    jev_box(s, ctx, &feed, (cx0, jy, prof.jev), fw);
    s.grid
        .put(cx0, jy + prof.jev, '│', Some(c::RULE), None, false);
    let jev_rows = (jy, jy + prof.jev - 1);
    s.metrics
        .lanes
        .push(("jev".to_string(), vec![(cx0, seat_bottom)]));

    if n == 0 {
        let msg = if model.data.loaded {
            "No agents are working. Dispatched agents appear here."
        } else {
            "gathering\u{2026}"
        };
        s.grid
            .center(fx, fw, jy + prof.jev + 3, &[(msg, c::FAINT, false)]);
        finished(s, ctx, r, &folded);
        pulses(s, ctx);
        return;
    }

    let cw = MAX_W.min((fw - 6 - (n.min(cap) as i32 - 1) * GAP) / n.min(cap) as i32);
    let card_y0 = fy + prof.head();
    let bus0 = card_y0 - 2;
    for j in 0..vis_rows {
        let row = first_row + j;
        let idxs: Vec<usize> = cards.iter().copied().skip(row * cap).take(cap).collect();
        let k = idxs.len() as i32;
        let total_w = k * cw + (k - 1) * GAP;
        let x0 = fx + (fw - total_w) / 2;
        let cy = card_y0 + j as i32 * (prof.card + 2);
        let centers: Vec<i32> = (0..k).map(|i| x0 + i * (cw + GAP) + cw / 2).collect();
        bus(s, cy - 2, &centers, (j == 0).then_some(cx0));
        for &cx in &centers {
            s.grid.put(cx, cy - 1, '▼', Some(c::FAINT), None, false);
        }
        for (i, &idx) in idxs.iter().enumerate() {
            let x = x0 + i as i32 * (cw + GAP);
            let agent = &model.agents[idx];
            card(s, ctx, agent, (x, cy, cw, prof.card));
            if j == 0 {
                let path = path_points(cx0, seat_bottom, jev_rows, bus0, centers[i]);
                s.metrics.lanes.push((agent.node.id.clone(), path));
            }
        }
    }
    if total_rows > vis_rows {
        let hint = format!(
            " \u{25b2} {} above \u{b7} \u{25bc} {} below ",
            first_row * cap,
            n.saturating_sub((first_row + vis_rows) * cap)
        );
        s.grid.text(fx + 2, fy + fh - 1, &hint, c::DIM);
    }
    finished(s, ctx, r, &folded);
    pulses(s, ctx);
}

/// The Jev box between the seat and the bus: its newest decisions, how sure it was, and what
/// that means. The seat's line runs through it (`┴` on its top edge, `┬` on its bottom edge).
fn jev_box(
    s: &mut Scene,
    ctx: &Ctx,
    feed: &content::JevFeed,
    (cx, y, h): (i32, i32, i32),
    fw: i32,
) {
    let w = JEV_W.min(fw - 4);
    let x = cx - w / 2;
    let now = ctx.now;
    let sel = ctx.sel == &Sel::Jev;
    let hov = ctx.hover == Some(&Sel::Jev);
    let flash = ctx
        .view
        .motion
        .jev_flash
        .filter(|b| now.saturating_sub(*b) < 1400)
        .map_or(0.0, |b| 1.0 - now.saturating_sub(b) as f64 / 1400.0);
    let base = mix(c::RULE, c::JEV, if hov { 0.75 } else { 0.45 });
    let border = if sel {
        c::JEV
    } else {
        mix(base, c::JEV, flash)
    };
    let bg = if sel || hov { c::RAISE } else { c::PANEL };
    s.grid.boxed(x, y, w, h, border, Some(bg));
    let tie = if sel {
        c::JEV
    } else {
        mix(c::RULE, c::JEV, 0.45)
    };
    s.grid.put(cx, y, '┴', Some(tie), None, false);
    s.grid.put(cx, y + h - 1, '┬', Some(tie), None, false);
    let tx = s.grid.bold(x + 2, y, " \u{25c6} JEV ", c::JEV);
    let reg = s.reg(x, y, w, h, Some(Act::Select(Sel::Jev)));
    reg.node = Some(Sel::Jev);
    if !feed.on() {
        s.grid.text(tx, y, "\u{b7} off ", c::FAINT);
        let why = "Jev is off: no [jev] site is on and the harness proxy does not use TypeSafe.";
        s.grid.text(x + 2, y + 1, &cut(why, w - 4), c::FAINT);
        return;
    }
    s.grid.text(tx, y, "\u{b7} decisions ", c::FAINT);
    let today = jev_today(ctx, feed);
    let sites = if feed.sites.is_empty() {
        "proxy on".to_string()
    } else {
        format!("{} sites on", feed.sites.len())
    };
    // The seat's line meets the top border at `cx`: the summary gives up its "today" before it
    // would run over it.
    let calls = today.len();
    let candidates = [
        Some(format!(" {sites} \u{b7} {calls} calls today ")),
        Some(format!(" {sites} \u{b7} {calls} calls ")),
    ];
    let right = candidates
        .into_iter()
        .flatten()
        .find(|r| x + w - 2 - r.chars().count() as i32 > cx + 1);
    if let Some(right) = right {
        s.grid
            .text(x + w - 2 - right.chars().count() as i32, y, &right, c::DIM);
    }
    let inner = h - 2;
    let footer = inner >= 2;
    let shown = (inner - i32::from(footer)).max(0) as usize;
    let last: Vec<&content::JevRow> = feed.rows.iter().rev().take(shown).collect();
    let bar_x = x + w - 36;
    for (i, j) in last.iter().enumerate() {
        let yy = y + 1 + i as i32;
        let born = ctx.view.motion.rows.get(&j.id);
        let age = born.map_or(u64::MAX, |b| now.saturating_sub(*b));
        if age < super::fx::ROW_FADE_MS {
            s.grid.fill(
                x + 1,
                yy,
                w - 2,
                1,
                mix(c::SEL, bg, age as f64 / super::fx::ROW_FADE_MS as f64),
            );
        }
        s.grid.bold(x + 2, yy, &cut(&j.site, 10), c::JEV);
        s.grid
            .text(x + 13, yy, &cut(&j.text, bar_x - x - 14), c::FG);
        let filled = (j.confidence * 10.0).round().clamp(0.0, 10.0) as i32;
        for k in 0..10 {
            let (ch, fg) = if k < filled {
                ('█', mix(c::JEV, c::FG, 0.1))
            } else {
                ('░', c::RULE)
            };
            s.grid.put(bar_x + k, yy, ch, Some(fg), None, false);
        }
        s.grid
            .text(bar_x + 11, yy, &format!("{:.2}", j.confidence), c::HI);
        let (word, col) = if j.sure {
            ("sure", c::OK)
        } else {
            ("unsure", c::WARN)
        };
        s.grid.text(bar_x + 16, yy, word, col);
        let ago = format!(
            "{} ago",
            content::elapsed_label(ctx.wall.saturating_sub(j.ts))
        );
        s.grid
            .text(x + w - 2 - ago.chars().count() as i32, yy, &ago, c::FAINT);
    }
    if last.is_empty() {
        s.grid.text(x + 2, y + 1, "No decisions yet", c::FAINT);
    }
    if footer {
        let text = "sure \u{2192} zirv applies it  \u{b7}  unsure \u{2192} zirv's own rule, or the supervisor";
        s.grid.text(x + 2, y + h - 2, &cut(text, w - 4), c::FAINT);
    }
}

/// The decisions made today, in the operator's local day.
pub(super) fn jev_today<'a>(ctx: &Ctx, feed: &'a content::JevFeed) -> Vec<&'a content::JevRow> {
    use chrono::TimeZone;
    let offset = ctx.model.facts.utc_offset;
    let day = |ts: u64| {
        offset
            .timestamp_opt(ts as i64, 0)
            .single()
            .map(|t| t.date_naive())
    };
    let today = day(ctx.wall);
    feed.rows.iter().filter(|j| day(j.ts) == today).collect()
}

/// The bus above one row of cards: every junction from the lines that really meet there. `from`
/// is the seat's line coming down into the first row; later rows hang off nothing above.
fn bus(s: &mut Scene, y: i32, centers: &[i32], from: Option<i32>) {
    let lo = centers.iter().copied().chain(from).min().unwrap_or(0);
    let hi = centers.iter().copied().chain(from).max().unwrap_or(0);
    for x in lo..=hi {
        let g = junction(from == Some(x), centers.contains(&x), x > lo, x < hi);
        s.grid.put(x, y, g, Some(c::RULE), None, false);
    }
}

/// The box-drawing glyph with exactly these connections, ends rounded.
pub(super) fn junction(up: bool, down: bool, left: bool, right: bool) -> char {
    match (up, down, left, right) {
        (true, true, true, true) => '┼',
        (true, true, true, false) => '┤',
        (true, true, false, true) => '├',
        (true, false, true, true) => '┴',
        (false, true, true, true) => '┬',
        (true, true, false, false) => '│',
        (true, false, false, true) => '╰',
        (true, false, true, false) => '╯',
        (false, true, false, true) => '╭',
        (false, true, true, false) => '╮',
        (false, false, _, _) => '─',
        (true, false, false, false) | (false, true, false, false) => '│',
    }
}

/// One agent's card.
fn card(s: &mut Scene, ctx: &Ctx, agent: &Agent, (x, y, w, h): (i32, i32, i32, i32)) {
    let node = agent.node;
    let id = &node.id;
    let st = status(ctx.model, node);
    let now = ctx.now;
    let sel = Sel::Agent(id.clone());
    let hov = ctx.hover == Some(&sel);
    let is_sel = ctx.sel == &sel;
    let motion = &ctx.view.motion;
    let mut bg = if hov || is_sel { c::RAISE } else { c::PANEL };
    let phase = node.started_at.unwrap_or(0).wrapping_mul(37);
    let rest = mix(c::RULE, c::OK, 0.3);
    let mut border = match st {
        St::Running => mix(
            c::AGENT_DIM,
            c::AGENT,
            breathe(now.wrapping_add(phase), 2.4),
        ),
        St::Waiting => mix(c::WARN_DIM, c::WARN, breathe(now, 1.4)),
        St::Done => rest,
        St::Failed => mix(c::RULE, c::ERR, 0.6),
        St::Idle => c::RULE,
    };
    if st == St::Done
        && let Some(born) = motion
            .finished
            .get(id)
            .filter(|b| now.saturating_sub(**b) < DONE_FLASH_MS)
    {
        border = mix(
            c::OK,
            rest,
            now.saturating_sub(*born) as f64 / DONE_FLASH_MS as f64,
        );
    }
    if let Some(born) = motion
        .dispatched
        .get(id)
        .filter(|b| now.saturating_sub(**b) < DISPATCH_FLASH_MS)
    {
        bg = mix(
            c::AGENT_DIM,
            bg,
            now.saturating_sub(*born) as f64 / DISPATCH_FLASH_MS as f64,
        );
    }
    if is_sel {
        border = if st == St::Waiting { c::WARN } else { c::HI };
    }
    s.grid.boxed(x, y, w, h, border, Some(bg));
    let reg = s.reg(x, y, w, h, Some(Act::Open(sel.clone())));
    reg.node = Some(sel);
    if w < 8 {
        return;
    }
    let iw = w - 4;
    let (g, gc) = glyph(st, now);
    s.grid.put(x + 2, y + 1, g, Some(gc), None, true);
    let job = ctx.model.job_of(node);
    let (title, _) = wrap(&job, (iw - 2) as usize, Some(2));
    let tc = if st == St::Done { c::DIM } else { c::HI };
    for (i, line) in title.iter().enumerate() {
        s.grid.bold(x + 4, y + 1 + i as i32, line, tc);
    }
    let model_name = content::node_model(node);
    let family = node.model.as_deref().map_or(String::new(), |m| {
        super::super::super::catalogue::model_family("anthropic", m)
            .unwrap_or(m)
            .to_string()
    });
    let age = age_word(node, st, ctx.wall);
    let head = format!("{model_name} \u{b7} {age}");
    let line = if head.chars().count() as i32 <= iw - 2 || family.is_empty() {
        head
    } else {
        format!("{family} \u{b7} {age}")
    };
    s.grid.text(x + 4, y + 3, &cut(&line, iw - 2), c::DIM);
    let rule = mix(c::RULE, bg, 0.3);
    for i in x + 2..x + w - 2 {
        s.grid.put(i, y + 4, '─', Some(rule), None, false);
    }
    let (now_text, nc) = now_line(node, st);
    s.grid.text(x + 2, y + 5, &cut(&now_text, iw), nc);
    for (k, (age, label)) in recent(node, st, ctx.wall, 4).into_iter().enumerate() {
        let row = y + 6 + k as i32;
        if row >= y + h - 1 {
            break;
        }
        let text = format!("{:>4} {label}", content::elapsed_label(age));
        s.grid.text(
            x + 2,
            row,
            &cut(&text, iw),
            if k == 0 { c::DIM } else { c::FAINT },
        );
    }
}

/// The FINISHED strip: agents that ended a while ago, as pills.
fn finished(s: &mut Scene, ctx: &Ctx, (fx, fy, fw, fh): (i32, i32, i32, i32), folded: &[usize]) {
    if folded.is_empty() {
        return;
    }
    let y = fy + fh - 2;
    let idle = |idx: usize| parked(ctx.model.agents[idx].node);
    let title = match (
        folded.iter().any(|&i| idle(i)),
        folded.iter().any(|&i| !idle(i)),
    ) {
        (true, true) => "IDLE \u{b7} FINISHED  ",
        (true, false) => "IDLE  ",
        _ => "FINISHED  ",
    };
    let mut x = s.grid.bold(fx + 3, y, title, c::FAINT);
    for (shown, &idx) in folded.iter().enumerate() {
        let node = ctx.model.agents[idx].node;
        let job = ctx.model.job_of(node);
        let ago =
            content::elapsed_label(ctx.wall.saturating_sub(node.ended_at.unwrap_or(ctx.wall)));
        let (glyph, glyph_color) = if parked(node) {
            ('\u{25cc}', c::DIM)
        } else {
            ('\u{2713}', c::OK)
        };
        let label = format!(" {glyph} {}  {ago} ago ", short_name(&job, 30));
        let lw = label.chars().count() as i32;
        if x + lw > fx + fw - 3 {
            let more = format!("+{} more", folded.len() - shown);
            s.grid.text(x, y, &more, c::FAINT);
            break;
        }
        let sel = Sel::Agent(node.id.clone());
        let hov = ctx.hover == Some(&sel);
        let (fg, bg) = if hov {
            (c::FG, c::RAISE)
        } else {
            (c::DIM, c::PANEL)
        };
        let end = s.grid.text_on(x, y, &label, fg, Some(bg), false);
        s.grid.put(x + 1, y, glyph, Some(glyph_color), None, true);
        let reg = s.reg(x, y, end - x, 1, Some(Act::Open(sel.clone())));
        reg.node = Some(sel);
        x = end + 1;
    }
}

/// The light that runs down the bus to working agents, and the pulses of what just happened.
fn pulses(s: &mut Scene, ctx: &Ctx) {
    let now = ctx.now;
    let lanes = s.metrics.lanes.clone();
    let path_of = |id: &str| lanes.iter().find(|l| l.0 == id).map(|l| l.1.clone());
    for (id, _) in &lanes {
        let Some(node) = ctx
            .model
            .agents
            .iter()
            .map(|a| a.node)
            .find(|n| &n.id == id)
        else {
            continue;
        };
        if status(ctx.model, node) != St::Running {
            continue;
        }
        let per = 2600u64;
        let ph = (now.wrapping_add(node.started_at.unwrap_or(0).wrapping_mul(13)) % per) as f64
            / per as f64;
        if ph > 0.6 {
            continue;
        }
        let Some(pts) = path_of(id) else { continue };
        let i = ((ph / 0.6 * pts.len() as f64).floor() as usize).min(pts.len() - 1);
        for k in 0..=3usize {
            let Some(j) = i.checked_sub(k) else { break };
            let (x, y) = pts[j];
            s.grid
                .set_fg(x, y, mix(c::RULE, c::AGENT, 0.85 - k as f64 * 0.22));
        }
    }
    for p in &ctx.view.motion.pulses {
        if now < p.born || now >= p.born + p.dur {
            continue;
        }
        let Some(mut pts) = path_of(&p.id) else {
            continue;
        };
        if p.up {
            pts.reverse();
        }
        let i = pulse_head(pts.len(), p.born, p.dur, now);
        for k in 1..=5usize {
            let Some(j) = i.checked_sub(k) else { break };
            let (x, y) = pts[j];
            s.grid.set_fg(x, y, mix(p.col, c::RULE, k as f64 / 6.0));
        }
        let (hx, hy) = pts[i];
        s.grid.put(hx, hy, p.glyph, Some(p.col), None, true);
    }
}

#[cfg(test)]
mod tests {
    use super::super::testkit::*;
    use super::*;
    use crate::commands::ctx::graph_steps::Step;

    #[test]
    fn junction_picks_the_glyph_with_exactly_those_connections() {
        assert_eq!(junction(true, true, true, true), '┼');
        assert_eq!(junction(true, false, true, true), '┴');
        assert_eq!(junction(false, true, true, true), '┬');
        assert_eq!(junction(true, true, false, true), '├');
        assert_eq!(junction(true, true, true, false), '┤');
        assert_eq!(junction(true, false, false, true), '╰');
        assert_eq!(junction(false, true, true, false), '╮');
        assert_eq!(junction(true, false, false, false), '│');
        assert_eq!(junction(false, false, true, true), '─');
    }

    #[test]
    fn the_pulse_head_walks_the_path_and_stops_on_its_last_point() {
        assert_eq!(pulse_head(10, 1_000, 1_000, 1_000), 0);
        assert_eq!(pulse_head(10, 1_000, 1_000, 1_500), 5);
        assert_eq!(pulse_head(10, 1_000, 1_000, 9_000), 9);
        assert_eq!(pulse_head(10, 1_000, 1_000, 500), 0);
        assert_eq!(pulse_head(0, 0, 0, 5), 0);
    }

    #[test]
    fn a_pulse_path_skips_the_jev_rows_then_runs_along_the_bus_into_the_card() {
        let right = path_points(10, 4, (6, 8), 12, 13);
        assert_eq!(&right[..3], [(10, 4), (10, 5), (10, 9)]);
        assert_eq!(right.last(), Some(&(13, 13)));
        assert!(right.contains(&(11, 12)) && right.contains(&(13, 12)));
        assert!(right.iter().all(|&(_, y)| !(6..=8).contains(&y)));
        let left = path_points(10, 4, (6, 8), 12, 8);
        assert!(left.contains(&(9, 12)) && left.contains(&(8, 12)));
        assert_eq!(left.last(), Some(&(8, 13)));
    }

    #[test]
    fn rows_fit_counts_whole_card_rows_and_a_too_short_box_fits_none() {
        let profiles = ladder(true);
        assert_eq!(profiles[0].jev, JEV_FULL);
        assert!(ladder(false).iter().all(|p| p.jev <= JEV_OFF));
        let tight = profiles.last().expect("profiles");
        assert_eq!(tight.card, MIN_CARD);
        assert_eq!(tight.rows_fit(tight.head(), false), 0);
        let one = tight.head() + tight.card + 1;
        assert_eq!(tight.rows_fit(one, false), 1);
        assert_eq!(tight.rows_fit(one + 1, true), 1);
        assert_eq!(tight.rows_fit(one + tight.card + 2, false), 2);
        assert!(tight.rows_fit(60, false) >= profiles[0].rows_fit(60, false));
    }

    fn running_step(ts: u64, tool: &str, arg: &str) -> Step {
        Step {
            ts,
            tool: tool.into(),
            arg: arg.into(),
        }
    }

    #[test]
    fn status_and_glyph_name_what_an_agent_is_doing() {
        let mut f = facts(None);
        f.approval_shorts = vec!["w1".into()];
        let v = view(fixture());
        let data = &v.data;
        let model = Model::build(data, &f, v.scope);
        let by = |id: &str| data.nodes.iter().find(|n| n.id == id).expect("node");
        assert_eq!(status(&model, by("w1")), St::Waiting);
        assert_eq!(status(&model, by("w2")), St::Done);
        assert_eq!(status(&model, by("w3")), St::Idle);
        assert_eq!(glyph(St::Waiting, 0).0, '⚑');
        assert_eq!(glyph(St::Done, 0).0, '✓');
        assert_eq!(glyph(St::Failed, 0).0, '✗');
        assert_eq!(glyph(St::Idle, 0).0, '◌');
        assert_eq!(glyph(St::Running, 0).1, c::AGENT);
    }

    #[test]
    fn now_line_and_recent_show_the_current_step_and_the_ones_before_it() {
        let mut node = node("w1", Some("seat-1"), "worker", "sol", "running");
        node.tokens = None;
        assert_eq!(now_line(&node, St::Running).0, "\u{25b8} working");
        assert_eq!(now_line(&node, St::Failed).0, "failed");
        node.steps = vec![
            running_step(100, "Read", "src/a.rs"),
            running_step(110, "Bash", "cargo build"),
        ];
        assert_eq!(now_line(&node, St::Running).0, "\u{25b8} run cargo build");
        assert_eq!(
            recent(&node, St::Running, 120, 5),
            [(20, "read src/a.rs".to_string())]
        );
        assert_eq!(
            recent(&node, St::Done, 120, 1),
            [(10, "run cargo build".to_string())]
        );
    }

    #[test]
    fn idle_agents_stay_cards_newest_first_and_only_the_overflow_folds_labelled_idle() {
        let mut data = fixture();
        data.nodes.retain(|n| n.id == "seat-1");
        for i in 0..6u64 {
            let mut n = node(&format!("i{i}"), Some("seat-1"), "subagent", "sol", "idle");
            n.ended_at = Some(100 + i);
            data.nodes.push(n);
        }
        let mut old = node("d", Some("seat-1"), "subagent", "sol", "completed");
        old.ended_at = Some(1);
        data.nodes.push(old);
        let f = facts(None);
        let v = view(data);
        let model = Model::build(&v.data, &f, v.scope);
        let sel = Sel::default();
        let ctx = Ctx {
            model: &model,
            view: &v,
            now: 0,
            wall: 100_000,
            sel: &sel,
            hover: None,
            hover_key: None,
        };
        let id = |i: &usize| model.agents[*i].node.id.clone();
        let (cards, folded) = split(&ctx);
        assert_eq!(
            cards.iter().map(id).collect::<Vec<_>>(),
            ["i5", "i4", "i3", "i2"]
        );
        assert_eq!(folded.iter().map(id).collect::<Vec<_>>(), ["i1", "i0", "d"]);
        let by = |id: &str| v.data.nodes.iter().find(|n| n.id == id).expect("node");
        assert_eq!(status(&model, by("i5")), St::Idle);
        assert_eq!(now_line(by("i5"), St::Idle).0, "idle");
        assert_eq!(age_word(by("i5"), St::Idle, 160), "idle 55s");
        assert_eq!(counts(&ctx).4, 6);
        assert!(counts_line(&ctx, 80).contains("6 idle"));
    }

    #[test]
    fn the_counts_line_shortens_its_wording_to_the_room_it_has() {
        let mut f = facts(None);
        f.approval_shorts = vec!["w1".into()];
        let mut data = fixture();
        data.nodes
            .push(node("wx", Some("seat-1"), "worker", "sol", "failed"));
        data.nodes
            .push(node("wy", Some("seat-1"), "worker", "sol", "running"));
        let v = view(data);
        let model = Model::build(&v.data, &f, v.scope);
        let sel = Sel::default();
        let ctx = Ctx {
            model: &model,
            view: &v,
            now: 0,
            wall: 2_000,
            sel: &sel,
            hover: None,
            hover_key: None,
        };
        assert_eq!(counts(&ctx), (1, 1, 1, 1, 0));
        assert_eq!(
            counts_line(&ctx, 60),
            "1 working \u{b7} 1 waiting for you \u{b7} 1 failed \u{b7} 1 finished"
        );
        assert_eq!(
            counts_line(&ctx, 50),
            "1 working \u{b7} 1 waiting \u{b7} 1 failed \u{b7} 1 finished"
        );
        assert_eq!(
            counts_line(&ctx, 40),
            "1 working \u{b7} 1 waiting \u{b7} 1 finished"
        );
    }
}
