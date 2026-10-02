//! Paints a [`Plan`] cell by cell, the way mockup A places every character.

use ratatui::Frame;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Style};
use ratatui::text::Span;
use unicode_width::UnicodeWidthStr;

use super::content::{self, Row, pal, span, spans_width};
use super::model::{Model, Sel};
use super::orch;
use super::plan::{self, Plan, Surface, Tier, ViewState};
use super::{Moment, TreeFacts, TreeView};

pub(super) struct Canvas<'a> {
    pub(super) buf: &'a mut Buffer,
    pub(super) bounds: Rect,
}

impl Canvas<'_> {
    pub(super) fn inside(&self, x: u16, y: u16) -> bool {
        x >= self.bounds.x
            && x < self.bounds.x + self.bounds.width
            && y >= self.bounds.y
            && y < self.bounds.y + self.bounds.height
    }

    pub(super) fn put(&mut self, x: u16, y: u16, symbol: &str, style: Style) {
        if !self.inside(x, y) {
            return;
        }
        if let Some(cell) = self.buf.cell_mut((x, y)) {
            cell.reset();
            cell.set_symbol(symbol);
            cell.set_style(style);
        }
    }

    /// Spans from `x`, never past `limit` columns. Returns the column after the text.
    pub(super) fn text(&mut self, x: u16, y: u16, spans: &[Span<'_>], limit: u16) -> u16 {
        let mut x = x;
        let end = x.saturating_add(limit);
        for s in spans {
            if x >= end || !self.inside(x, y) {
                break;
            }
            let (nx, _) = self
                .buf
                .set_stringn(x, y, &s.content, usize::from(end - x), s.style);
            x = nx;
        }
        x
    }

    pub(super) fn center(&mut self, x: u16, width: u16, y: u16, spans: &[Span<'_>]) {
        let w = spans_width(spans).min(usize::from(width)) as u16;
        self.text(x + (width - w) / 2, y, spans, w);
    }

    pub(super) fn right(&mut self, end: u16, y: u16, spans: &[Span<'_>]) {
        let w = spans_width(spans) as u16;
        self.text(end.saturating_sub(w), y, spans, w);
    }

    pub(super) fn hline(&mut self, x0: u16, x1: u16, y: u16, symbol: &str, style: Style) {
        for x in x0..=x1 {
            self.put(x, y, symbol, style);
        }
    }

    /// A single-line border with an optional title set into the top edge.
    pub(super) fn boxed(&mut self, r: Rect, style: Style, title: &[Span<'_>]) {
        if r.width < 2 || r.height < 2 {
            return;
        }
        let (right, bottom) = (r.x + r.width - 1, r.y + r.height - 1);
        self.hline(r.x + 1, right - 1, r.y, "\u{2500}", style);
        self.hline(r.x + 1, right - 1, bottom, "\u{2500}", style);
        for y in r.y + 1..bottom {
            self.put(r.x, y, "\u{2502}", style);
            self.put(right, y, "\u{2502}", style);
        }
        self.put(r.x, r.y, "\u{250c}", style);
        self.put(right, r.y, "\u{2510}", style);
        self.put(r.x, bottom, "\u{2514}", style);
        self.put(right, bottom, "\u{2518}", style);
        if !title.is_empty() && r.width > 6 {
            let room = r.width - 4;
            let mut padded = vec![span(" ", style)];
            padded.extend(title.iter().cloned());
            padded.push(span(" ", style));
            self.text(r.x + 2, r.y, &padded, room);
        }
    }

    /// The selected-row background on a box border.
    pub(super) fn tint_ring(&mut self, r: Rect, bg: Color) {
        for x in r.x..r.x + r.width {
            self.tint(x, r.y, bg);
            self.tint(x, r.y + r.height - 1, bg);
        }
        for y in r.y..r.y + r.height {
            self.tint(r.x, y, bg);
            self.tint(r.x + r.width - 1, y, bg);
        }
    }

    pub(super) fn tint_row(&mut self, x0: u16, x1: u16, y: u16, bg: Color) {
        for x in x0..=x1 {
            self.tint(x, y, bg);
        }
    }

    pub(super) fn tint(&mut self, x: u16, y: u16, bg: Color) {
        if !self.inside(x, y) {
            return;
        }
        if let Some(cell) = self.buf.cell_mut((x, y)) {
            cell.set_bg(bg);
        }
    }
}

fn draw_rows(c: &mut Canvas, r: Rect, first: u16, rows: &[Row]) {
    let inner = r.width.saturating_sub(2);
    for (i, row) in rows.iter().enumerate() {
        let y = r.y + first + i as u16;
        if y + 1 >= r.y + r.height {
            break;
        }
        if row.left {
            c.text(r.x + 1, y, &row.spans, inner);
        } else {
            c.center(r.x + 1, inner, y, &row.spans);
        }
    }
}

/// Draw the whole page into `area` (normally the frame): the orchestrator dashboard from 100
/// columns up, else header, legend, flow, log, statusline.
pub(in super::super) fn render(f: &mut Frame, area: Rect, view: &TreeView, facts: &TreeFacts) {
    paint(
        f,
        Surface::page(area),
        view,
        facts,
        super::theme::truecolor(),
    );
}

/// [`render`] with the colour depth chosen, so a test does not depend on the environment.
#[cfg(test)]
pub(super) fn render_with(
    f: &mut Frame,
    area: Rect,
    view: &TreeView,
    facts: &TreeFacts,
    truecolor: bool,
) {
    paint(f, Surface::page(area), view, facts, truecolor);
}

/// Just the flow inside `area`, the way the orchestrator dashboard composes it.
#[cfg(test)]
fn render_flow(f: &mut Frame, area: Rect, view: &TreeView, facts: &TreeFacts) {
    paint(f, Surface::panel(area), view, facts, true);
}

/// The phase-1 page at any size, for the tests that pin its tiers.
#[cfg(test)]
pub(super) fn render_classic(f: &mut Frame, area: Rect, view: &TreeView, facts: &TreeFacts) {
    paint_with(f, Surface::page(area), view, facts, false, true);
}

fn paint(f: &mut Frame, surf: Surface, view: &TreeView, facts: &TreeFacts, truecolor: bool) {
    paint_with(f, surf, view, facts, true, truecolor);
}

fn paint_with(
    f: &mut Frame,
    surf: Surface,
    view: &TreeView,
    facts: &TreeFacts,
    orchestrator: bool,
    truecolor: bool,
) {
    let area = surf.area;
    if area.width < 4 || area.height < 1 || (area.height < 2 && !view.in_chat()) {
        return;
    }
    let model = Model::build(&view.data, facts, view.scope);
    if view.in_chat() {
        if let Some(scene) = orch::build_chat(area, &model, view) {
            orch::paint(f.buffer_mut(), area, &scene, truecolor);
            return;
        }
        let mut c = Canvas {
            buf: f.buffer_mut(),
            bounds: area,
        };
        let bar = content::chat_bar(&model, usize::from(area.width).saturating_sub(2));
        c.text(area.x + 1, area.y, &bar, area.width.saturating_sub(2));
        return;
    }
    let selected = model.resolve(&view.selected);
    if orchestrator
        && !surf.panel
        && let Some(scene) = orch::build(area, &model, view, &selected)
    {
        orch::paint(f.buffer_mut(), area, &scene, truecolor);
        return;
    }
    let vs = ViewState {
        selected: &selected,
        scroll: view.scroll,
        error: view.error.as_deref(),
    };
    let plan = plan::build(surf, &model, &vs);
    let mut c = Canvas {
        buf: f.buffer_mut(),
        bounds: area,
    };
    match plan.tier {
        Tier::List => draw_list(&mut c, &model, &plan, &vs),
        _ => draw_stacked(&mut c, &model, &plan, &vs),
    }
    if !plan.panel {
        draw_footer(&mut c, &plan);
        if view.help {
            draw_help(&mut c, area);
        }
    }
}

fn header(c: &mut Canvas, m: &Model, plan: &Plan) {
    let w = plan.area.width;
    let x0 = plan.area.x;
    let full = plan.tier == Tier::Full;
    let sep = if full { "  \u{b7}  " } else { " \u{b7} " };
    let mut left = vec![
        span("ZIRV AGENT TREE", pal::bold()),
        span(sep, pal::dim()),
        span(
            content::seat_title(m.facts).to_uppercase(),
            pal::strong(pal::SEAT),
        ),
        span(" SEAT", Style::default()),
    ];
    let mut arch_part = Vec::new();
    match (&m.data.supervisor, full) {
        (Some(_), true) => {
            arch_part.push(span(sep, pal::dim()));
            arch_part.push(span(
                content::header_supervisor(m.data).unwrap_or_default(),
                pal::strong(pal::ARCH),
            ));
            arch_part.push(span(" ON CALL", Style::default()));
        }
        (Some(a), false) => {
            arch_part.push(span(sep, pal::dim()));
            arch_part.push(span(
                format!("supervisor on \u{b7} {}/{}", a.calls, a.max_calls),
                pal::fg(pal::ARCH),
            ));
        }
        (None, false) => {
            arch_part.push(span(sep, pal::dim()));
            arch_part.push(span("supervisor off", pal::dim()));
        }
        (None, true) => {}
    }
    let scope = vec![
        span(m.scope.label(), pal::dim()),
        span(" \u{b7} ", pal::dim()),
        span(m.total().to_string(), pal::bold()),
    ];
    let hint = vec![
        span("   ", pal::dim()),
        span("^A t", pal::bold()),
        span(" \u{2192} dashboard", pal::dim()),
    ];
    let width = usize::from(w);
    let fits = |l: &[Span], r: &[Span]| spans_width(l) + spans_width(r) + 3 <= width;
    let with_arch: Vec<Span> = left.iter().chain(&arch_part).cloned().collect();
    let mut right: Vec<Span> = scope.iter().chain(&hint).cloned().collect();
    if fits(&with_arch, &right) {
        left = with_arch;
    } else {
        right = scope.clone();
        if fits(&with_arch, &right) {
            left = with_arch;
        } else if !fits(&left, &right) {
            // Last resort: the scope alone, then nothing on the right.
            right = Vec::new();
        }
    }
    c.text(x0 + 1, plan.header_y, &left, w.saturating_sub(2));
    c.right(x0 + w - 1, plan.header_y, &right);
}

/// Mockup A's legend: what each colour of box means.
pub(super) fn legend_spans(m: &Model) -> Vec<Span<'static>> {
    let jev_off = if matches!(
        m.facts.jev,
        Some(super::super::ui::JevSectionFact::Active { .. })
    ) {
        ""
    } else {
        " off"
    };
    let mut spans = vec![
        span("\u{25a0} ", pal::fg(pal::SEAT)),
        span("seat", pal::dim()),
        span("     ", pal::dim()),
        span("\u{25a0} ", pal::fg(pal::AGENT)),
        span("agents", pal::dim()),
        span("     ", pal::dim()),
        span("\u{25a0} ", pal::fg(pal::JEV)),
        span("jev", pal::dim()),
        span(jev_off, pal::dim()),
        span("     ", pal::dim()),
        span("\u{25a0} ", pal::fg(pal::ARCH)),
        span("supervisor", pal::dim()),
    ];
    if m.data.supervisor.is_some() {
        spans.push(span(" \u{b7} on call", pal::dim()));
    } else {
        spans.push(span(" off", pal::dim()));
    }
    spans
}

fn legend(c: &mut Canvas, m: &Model, plan: &Plan, error: Option<&str>) {
    let spans = legend_spans(m);
    let x = plan.area.x + 2;
    c.text(x, plan.legend_y, &spans, plan.area.width.saturating_sub(3));
    if let Some(error) = error {
        let line = [span(content::fit(error, 30), pal::dim())];
        let start = x + spans_width(&spans) as u16 + 2;
        let w = spans_width(&line) as u16;
        if start + w < plan.area.x + plan.area.width {
            c.right(plan.area.x + plan.area.width - 1, plan.legend_y, &line);
        }
    }
}

fn select_ring(c: &mut Canvas, r: Rect, selected: bool, first_row: Option<u16>) {
    if !selected {
        return;
    }
    c.tint_ring(r, pal::SELECTED_BG);
    if let Some(y) = first_row {
        c.tint_row(r.x + 1, r.x + r.width - 2, y, pal::SELECTED_BG);
    }
}

fn draw_stacked(c: &mut Canvas, m: &Model, plan: &Plan, vs: &ViewState) {
    let full = plan.tier == Tier::Full;
    if !plan.panel {
        header(c, m, plan);
        if let Some(y) = plan.rule_y {
            let w = plan.area.width.saturating_sub(2);
            c.hline(plan.area.x + 1, plan.area.x + w, y, "\u{2500}", pal::dim());
        }
        legend(c, m, plan, vs.error);
    }

    // Seat card.
    let seat_style = pal::fg(pal::SEAT);
    c.boxed(plan.seat, seat_style, &[]);
    let inner = usize::from(plan.seat.width.saturating_sub(4));
    draw_rows(
        c,
        plan.seat,
        1,
        &content::seat_rows(m, plan.seat_lines, inner),
    );
    select_ring(
        c,
        plan.seat,
        *vs.selected == Sel::Seat,
        Some(plan.seat.y + 1),
    );

    // Jev card.
    let jev_style = pal::fg(pal::JEV);
    if !plan.jev_hidden {
        let title = [span(content::jev_title(m.facts), pal::strong(pal::JEV))];
        c.boxed(plan.jev, jev_style, &title);
        if let Some(calls) = content::jev_calls(m.facts) {
            let tail = [
                span(" calls ", pal::dim()),
                span(calls.to_string(), pal::bold()),
                span(" ", pal::dim()),
            ];
            let room = spans_width(&title) as u16 + 6;
            if plan.jev.width > room + spans_width(&tail) as u16 + 2 {
                c.right(plan.jev.x + plan.jev.width - 2, plan.jev.y, &tail);
            }
        }
        let jev_inner = usize::from(plan.jev.width.saturating_sub(4));
        let mut rows = content::jev_rows(m, plan.jev_sites, jev_inner);
        if plan.jev_legend {
            rows.push(content::jev_legend());
        }
        for (i, row) in rows.iter().enumerate() {
            let y = plan.jev.y + 1 + i as u16;
            if y + 1 >= plan.jev.y + plan.jev.height {
                break;
            }
            if row.left {
                c.text(
                    plan.jev.x + 2,
                    y,
                    &row.spans,
                    plan.jev.width.saturating_sub(4),
                );
            } else {
                c.center(plan.jev.x + 1, plan.jev.width - 2, y, &row.spans);
            }
        }
    }
    if full {
        let bottom = plan.seat.y + plan.seat.height - 1;
        c.put(plan.cx, bottom, "\u{252c}", seat_style);
        c.put(plan.cx, bottom + 1, "\u{2502}", pal::dim());
        if !plan.rows.is_empty() {
            c.put(plan.cx, plan.spawn_y, "\u{2502}", pal::dim());
        }
        if !plan.jev_hidden {
            c.put(plan.cx, plan.jev.y, "\u{2534}", jev_style);
            let jev_bottom = plan.jev.y + plan.jev.height - 1;
            c.put(plan.cx, jev_bottom, "\u{252c}", jev_style);
            c.put(plan.cx, jev_bottom + 1, "\u{2502}", pal::dim());
        }
    }

    // Compact: each card hangs its line from the middle of its bottom edge down to the first bus.
    if !plan.hanging_lines().is_empty()
        && let Some(bus_y) = plan.rows.first().map(|row| row.bus_y)
    {
        let cards = [(plan.seat, seat_style), (plan.jev, jev_style)];
        for (card, style) in &cards[..if plan.jev_hidden { 1 } else { 2 }] {
            let (card, style) = (*card, *style);
            let x = card.x + card.width / 2;
            let bottom = card.y + card.height - 1;
            c.put(x, bottom, "\u{252c}", style);
            for y in bottom + 1..bus_y {
                c.put(x, y, "\u{2502}", pal::dim());
            }
        }
    }

    spawn_row(c, m, plan);
    draw_agents(c, m, plan, vs);

    if let Some(back) = plan.back {
        draw_back(c, m, plan, back);
    }
    if let Some(log) = plan.log {
        c.boxed(
            log,
            pal::dim(),
            &[span("session log", pal::fg(Color::Reset))],
        );
        let inner = log.width.saturating_sub(4);
        let rows = content::log_rows(m, usize::from(inner), plan.log_lines);
        if rows.is_empty() {
            let text = if m.data.loaded {
                "no events yet"
            } else {
                "gathering\u{2026}"
            };
            c.text(log.x + 2, log.y + 1, &[span(text, pal::dim())], inner);
        }
        for (i, spans) in rows.iter().enumerate() {
            c.text(log.x + 2, log.y + 1 + i as u16, spans, inner);
        }
    }
    if let Some(side) = plan.side {
        draw_sidecar(c, m, plan, side);
    }
}

fn spawn_row(c: &mut Canvas, m: &Model, plan: &Plan) {
    let facts = m.facts;
    let head = vec![
        span("spawn agents", Style::default()),
        span(" \u{b7} ", pal::dim()),
        span(
            format!("{} of {}", facts.panes_used, facts.max_panes),
            pal::strong(pal::AGENT),
        ),
        span(" slots", pal::dim()),
    ];
    let mut label = head.clone();
    if facts.max_writers > 0 {
        label.push(span(
            format!(" \u{b7} writers cap {}", facts.max_writers),
            pal::dim(),
        ));
    }
    let hint = |wording: bool| -> Vec<Span<'static>> {
        let mut parts = Vec::new();
        let (up, down) = if wording {
            (
                format!("\u{25b2} {} above", plan.above),
                format!("\u{25bc} {} below", plan.below),
            )
        } else {
            (
                format!("\u{25b2}{}", plan.above),
                format!("\u{25bc}{}", plan.below),
            )
        };
        if plan.above > 0 {
            parts.push(span(up, pal::fg(pal::AGENT)));
        }
        if plan.above > 0 && plan.below > 0 {
            parts.push(span(" \u{b7} ", pal::dim()));
        }
        if plan.below > 0 {
            parts.push(span(down, pal::fg(pal::AGENT)));
        }
        parts
    };
    let flow = plan.flow;
    let scrolled = plan.above > 0 || plan.below > 0;
    // The widest pairing that does not collide: full wording first, the short forms after.
    let pairs: Vec<(Vec<Span<'static>>, Vec<Span<'static>>)> = if scrolled {
        vec![
            (label.clone(), hint(true)),
            (label.clone(), hint(false)),
            (head.clone(), hint(true)),
            (head.clone(), hint(false)),
        ]
    } else {
        vec![(label.clone(), Vec::new())]
    };
    let fitting = pairs.iter().find(|(l, h)| {
        let label_w = spans_width(l) as u16;
        let label_x = flow.x + flow.width.saturating_sub(label_w) / 2;
        h.is_empty() || label_x + label_w + 2 + spans_width(h) as u16 <= flow.x + flow.width
    });
    let (label, hint) = fitting
        .cloned()
        .unwrap_or_else(|| (head.clone(), Vec::new()));
    if !hint.is_empty() {
        c.right(flow.x + flow.width, plan.spawn_y, &hint);
    }
    // Compact: the label shares its row with the lines the seat and Jev cards hang, so it sits
    // centred when it clears them and in the widest gap between them when it does not.
    let lines = plan.spawn_verticals();
    let w = spans_width(&label) as u16;
    let centred = flow.x + flow.width.saturating_sub(w) / 2;
    if lines.iter().all(|&l| l + 1 < centred || l > centred + w) {
        c.text(centred, plan.spawn_y, &label, w);
        return;
    }
    let mut edges = vec![flow.x.saturating_sub(2)];
    edges.extend(&lines);
    let hint_w = if hint.is_empty() {
        0
    } else {
        spans_width(&hint) as u16 + 2
    };
    edges.push((flow.x + flow.width + 1).saturating_sub(hint_w));
    let Some((from, to)) = edges
        .windows(2)
        .map(|pair| (pair[0] + 2, pair[1].saturating_sub(2)))
        .max_by_key(|(a, b)| b.saturating_sub(*a))
    else {
        return;
    };
    let room = to.saturating_sub(from);
    for candidate in [&label, &head] {
        let cw = spans_width(candidate) as u16;
        if cw <= room {
            c.text(from + (room - cw) / 2, plan.spawn_y, candidate, cw);
            return;
        }
    }
}

fn centers(row: &plan::BoxRow) -> Vec<u16> {
    row.cells
        .iter()
        .map(|cell| cell.rect.x + cell.rect.width / 2)
        .collect()
}

/// The box-drawing glyph with exactly these connections.
fn junction(up: bool, down: bool, left: bool, right: bool) -> &'static str {
    match (up, down, left, right) {
        (true, true, true, true) => "\u{253c}",
        (true, true, true, false) => "\u{2524}",
        (true, true, false, true) => "\u{251c}",
        (true, false, true, true) => "\u{2534}",
        (false, true, true, true) => "\u{252c}",
        (true, true, false, false) => "\u{2502}",
        (true, false, false, true) => "\u{2514}",
        (true, false, true, false) => "\u{2518}",
        (false, true, false, true) => "\u{250c}",
        (false, true, true, false) => "\u{2510}",
        _ => "\u{2500}",
    }
}

/// A bus between box centres `xs` and the lines `tops` that meet it from the other side: going
/// down, the `tops` come from above and `xs` leave below (`┌─●─┬─┴─┬─●─┐`); going up it is the
/// reverse. Every cell's glyph is picked from the four connections it really has. A dot only
/// replaces a plain `─`.
fn bus(c: &mut Canvas, y: u16, xs: &[u16], tops: &[u16], down: bool, node: Style) {
    let dim = pal::dim();
    let (Some(&first), Some(&last)) = (xs.first(), xs.last()) else {
        return;
    };
    let lo = tops.iter().copied().fold(first, u16::min);
    let hi = tops.iter().copied().fold(last, u16::max);
    let (above, below) = if down { (tops, xs) } else { (xs, tops) };
    for x in lo..=hi {
        c.put(
            x,
            y,
            junction(above.contains(&x), below.contains(&x), x > lo, x < hi),
            dim,
        );
    }
    for pair in xs.windows(2) {
        let mid = (pair[0] + pair[1]) / 2;
        // Next to a junction a dot would only crowd it.
        if tops.iter().all(|t| t.abs_diff(mid) > 1) && mid > lo && mid < hi {
            let plain = c
                .buf
                .cell_mut((mid, y))
                .is_some_and(|cell| cell.symbol() == "\u{2500}");
            if plain {
                c.put(mid, y, "\u{25cf}", node);
            }
        }
    }
}

fn draw_agents(c: &mut Canvas, m: &Model, plan: &Plan, vs: &ViewState) {
    if let Some(empty) = plan.empty {
        draw_empty(c, m, empty);
    }
    let last = plan.rows.len().saturating_sub(1);
    for (index, row) in plan.rows.iter().enumerate() {
        let xs = centers(row);
        // Compact: the seat and Jev cards each drop their own line onto the first bus.
        // Only the first fan-out row is fed from the seat or Jev; a later row hangs off nothing above.
        let tops = match plan.hanging_lines() {
            _ if index > 0 => Vec::new(),
            lines if !lines.is_empty() => lines,
            _ => vec![plan.cx],
        };
        bus(c, row.bus_y, &xs, &tops, true, pal::fg(pal::AGENT));
        for &x in &xs {
            c.put(x, row.arrow_y, "\u{25bc}", pal::fg(pal::AGENT));
        }
        for cell in &row.cells {
            let id = &m.agents[cell.agent].node.id;
            let is_sel = matches!(vs.selected, Sel::Agent(sel) if sel == id);
            c.boxed(cell.rect, pal::fg(pal::AGENT), &[]);
            for (i, line) in cell.rows.iter().enumerate() {
                let y = cell.rect.y + 1 + i as u16;
                if y + 1 >= cell.rect.y + cell.rect.height {
                    break;
                }
                if line.left {
                    c.text(
                        cell.rect.x + 2,
                        y,
                        &line.spans,
                        cell.rect.width.saturating_sub(3),
                    );
                } else {
                    c.center(cell.rect.x + 1, cell.rect.width - 2, y, &line.spans);
                }
                if line.sel.as_ref().is_some_and(|s| s == vs.selected) {
                    c.tint_row(
                        cell.rect.x + 1,
                        cell.rect.x + cell.rect.width - 2,
                        y,
                        pal::SELECTED_BG,
                    );
                }
            }
            select_ring(c, cell.rect, is_sel, Some(cell.rect.y + 1));
            if plan.back.is_some() && index == last {
                c.put(
                    cell.rect.x + cell.rect.width / 2,
                    cell.rect.y + cell.rect.height - 1,
                    "\u{252c}",
                    pal::fg(pal::AGENT),
                );
            }
        }
    }
    if let (Some(conv), Some(back)) = (plan.conv_y, plan.back) {
        if let Some(row) = plan.rows.last() {
            bus(
                c,
                conv,
                &centers(row),
                &[plan.cx],
                false,
                pal::fg(pal::SEAT),
            );
        }
        c.put(plan.cx, back.y - 1, "\u{25bc}", pal::fg(pal::SEAT));
    }
}

fn draw_empty(c: &mut Canvas, m: &Model, area: Rect) {
    let lines: Vec<Vec<Span<'static>>> = if !m.data.loaded {
        vec![vec![span("gathering\u{2026}", pal::dim())]]
    } else {
        let mut lines = vec![
            vec![span(
                content::fit(
                    "no agents yet \u{b7} they appear here when the seat dispatches",
                    usize::from(area.width),
                ),
                pal::dim(),
            )],
            vec![
                span("^A s", pal::bold()),
                span(" spawns one now", pal::dim()),
            ],
        ];
        if m.scope != super::model::Scope::All {
            lines.push(vec![
                span("s", pal::bold()),
                span(
                    format!(" widens the view to {}", m.scope.next().label()),
                    pal::dim(),
                ),
            ]);
        }
        lines
    };
    for (i, line) in lines.iter().enumerate() {
        c.center(area.x, area.width, area.y + 1 + i as u16, line);
    }
}

fn draw_back(c: &mut Canvas, m: &Model, plan: &Plan, back: Rect) {
    let style = pal::fg(pal::SEAT);
    c.boxed(back, style, &[span("back to seat", pal::strong(pal::SEAT))]);
    let inner = usize::from(back.width.saturating_sub(4));
    draw_rows(c, back, 1, &content::back_rows(m, plan.back_lines, inner));
}

/// A line of dim text wrapped to `width`, at most `max` lines.
fn wrap(text: &str, width: usize, max: usize) -> Vec<String> {
    let mut lines: Vec<String> = Vec::new();
    for word in text.split_whitespace() {
        match lines.last_mut() {
            Some(line) if line.width() + 1 + word.width() <= width => {
                line.push(' ');
                line.push_str(word);
            }
            _ => lines.push(word.to_string()),
        }
    }
    if lines.len() > max {
        lines.truncate(max);
        if let Some(last) = lines.last_mut() {
            *last = content::fit(&format!("{last}\u{2026}"), width);
        }
    }
    lines.into_iter().map(|l| content::fit(&l, width)).collect()
}

fn draw_sidecar(c: &mut Canvas, m: &Model, plan: &Plan, side: Rect) {
    let arch_style = pal::fg(pal::ARCH);
    c.boxed(
        side,
        arch_style,
        &[span("SUPERVISOR", pal::strong(pal::ARCH))],
    );
    let inner = side.width.saturating_sub(4);
    let Some(a) = &m.data.supervisor else {
        c.center(
            side.x + 1,
            side.width - 2,
            side.y + 1,
            &[span("off", pal::dim())],
        );
        return;
    };
    let bottom = side.y + side.height - 1;
    c.center(
        side.x + 1,
        side.width - 2,
        side.y + 1,
        &[span(
            content::supervisor_model_line(a, usize::from(inner)),
            arch_style,
        )],
    );
    let moments = [Moment::BeforePlan, Moment::ErrorRepeats, Moment::BeforeDone];
    for (i, moment) in moments.into_iter().enumerate() {
        let Some(y) = plan.moments[i].filter(|y| *y < bottom) else {
            continue;
        };
        let fired = a.last == Some(moment);
        let label = content::moment_label(moment);
        if fired {
            c.tint_row(side.x + 1, side.x + side.width - 2, y, pal::FIRED_BG);
            c.text(
                side.x + 2,
                y,
                &[
                    span("\u{25c6} ", pal::strong(pal::ARCH)),
                    span(label, pal::strong(pal::ARCH)),
                ],
                inner,
            );
            if let Some(cell) = c.buf.cell_mut((side.x + 2, y)) {
                cell.set_bg(pal::FIRED_BG);
            }
        } else {
            c.text(
                side.x + 2,
                y,
                &[span("\u{25c7} ", arch_style), span(label, Style::default())],
                inner,
            );
        }
        // The dashed arrow into the flow.
        let from = side.x + side.width;
        if let Some(to) = plan.moment_to[i]
            && to >= from + 2
        {
            let style = if fired { arch_style } else { pal::dim() };
            c.hline(from, to - 2, y, "\u{254c}", style);
            if fired {
                c.put(from, y, "\u{25cf}", arch_style);
            }
            c.put(to - 1, y, "\u{25b6}", style);
        }
    }

    // Everything else in the gaps between the moments, most useful first.
    let regions = [
        (
            plan.moments[0].map_or(side.y + 3, |y| y + 1),
            plan.moments[1].unwrap_or(bottom),
        ),
        (
            plan.moments[1].map_or(bottom, |y| y + 1),
            plan.moments[2].unwrap_or(bottom),
        ),
        (plan.moments[2].map_or(bottom, |y| y + 1), bottom),
    ];
    let w = usize::from(inner);
    let advice: Vec<Vec<Span<'static>>> = {
        let mut rows = vec![vec![span("last advice:", pal::dim())]];
        if a.advice.is_empty() {
            rows.push(vec![span("\u{2014} none yet", pal::dim())]);
        } else {
            for (i, line) in wrap(&a.advice, w.saturating_sub(2), 2)
                .into_iter()
                .enumerate()
            {
                let lead = if i == 0 { "\u{bb} " } else { "  " };
                rows.push(vec![span(
                    format!("{lead}{line}"),
                    pal::warn().add_modifier(ratatui::style::Modifier::BOLD),
                )]);
            }
        }
        rows
    };
    let usage = vec![
        vec![
            span("calls", pal::dim()),
            span(
                format!(" {}/{}", a.calls, a.max_calls),
                pal::strong(pal::ARCH),
            ),
        ],
        vec![
            span("tokens read ", pal::dim()),
            span(content::supervisor_reads(a), Style::default()),
        ],
    ];
    let note = |lines: &[&str]| -> Vec<Vec<Span<'static>>> {
        lines
            .iter()
            .map(|l| vec![span((*l).to_string(), pal::dim())])
            .collect()
    };
    let blocks: [Vec<Vec<Vec<Span<'static>>>>; 3] = [
        vec![
            advice,
            usage,
            note(&[
                "fires at only three",
                "moments; reads the",
                "plan, diff + errors",
            ]),
        ],
        vec![note(&[
            "never writes code;",
            "the seat applies",
            "the advice",
        ])],
        vec![note(&["silent on every", "routine turn"])],
    ];
    for ((from, to), group) in regions.into_iter().zip(blocks) {
        let mut y = from;
        for block in group {
            let need = block.len() as u16;
            if y + need > to {
                continue;
            }
            for row in block {
                c.text(side.x + 2, y, &row, inner);
                y += 1;
            }
            y += 1;
        }
    }
}

fn draw_footer(c: &mut Canvas, plan: &Plan) {
    let y = plan.status_y;
    c.text(
        plan.area.x,
        y,
        &[span(plan.footer.left.clone(), pal::dim())],
        plan.area.width,
    );
    for placed in &plan.footer.hints {
        c.text(
            plan.area.x + placed.x,
            y,
            &[
                span(placed.hint.key, pal::bold()),
                span(format!(" {}", placed.hint.label), pal::dim()),
            ],
            placed.hint.width() as u16,
        );
    }
}

fn draw_list(c: &mut Canvas, m: &Model, plan: &Plan, vs: &ViewState) {
    let w = plan.area.width;
    let x = plan.area.x;
    let y0 = plan.area.y;
    if !plan.panel {
        let title = format!(
            "AGENT TREE \u{b7} {} \u{b7} {} \u{b7} {}",
            content::fit(
                &format!(
                    "{} \u{b7} {}",
                    content::seat_title(m.facts),
                    m.facts.seat_role.unwrap_or("seat")
                ),
                usize::from(w),
            ),
            m.scope.label(),
            m.total()
        );
        c.text(
            x,
            y0,
            &[span(content::fit(&title, usize::from(w)), pal::bold())],
            w,
        );
        if *vs.selected == Sel::Seat {
            c.tint_row(x, x + w - 1, y0, pal::SELECTED_BG);
        }
        c.text(
            x,
            y0 + 1,
            &[span(
                content::fit(&content::spawn_label(m.facts), usize::from(w)),
                pal::dim(),
            )],
            w,
        );
        let jev = content::jev_calls(m.facts).map_or("off".to_string(), |n| format!("{n} calls"));
        c.text(
            x,
            y0 + 2,
            &[span(
                content::fit(
                    &format!(
                        "supervisor: {}   jev: {jev}",
                        content::supervisor_status(m.data)
                    ),
                    usize::from(w),
                ),
                pal::dim(),
            )],
            w,
        );
    }
    if plan.list.is_empty() {
        let text = if m.data.loaded {
            "no agents yet \u{b7} they appear when the seat dispatches"
        } else {
            "gathering\u{2026}"
        };
        let top = if plan.panel { y0 } else { y0 + 3 };
        c.text(
            x,
            top,
            &[span(content::fit(text, usize::from(w)), pal::dim())],
            w,
        );
    }
    for row in &plan.list {
        let node = match &row.sel {
            Sel::Agent(id) | Sel::Child(id) => m.data.nodes.iter().find(|n| n.id == *id),
            Sel::Seat | Sel::Jev => None,
        };
        let Some(node) = node else { continue };
        let mark = if m.waiting(node) {
            content::Mark::Waiting
        } else {
            content::Mark::of(&node.status)
        };
        let mut parts = vec![
            content::node_title(node),
            content::node_model_badged(m.data, node),
        ];
        parts.extend(content::own_effort(node).map(str::to_string));
        parts.extend(content::node_elapsed(node, m.facts.now));
        parts.extend(node.tokens.map(content::tokens_label));
        let indent = "  ".repeat(row.depth);
        let room = usize::from(w).saturating_sub(indent.width() + 2);
        let line = [
            span(indent, pal::dim()),
            span(format!("{} ", mark.glyph()), mark.style()),
            span(content::fit(&parts.join(" "), room), Style::default()),
        ];
        c.text(x, row.y, &line, w);
        if &row.sel == vs.selected {
            c.tint_row(x, x + w - 1, row.y, pal::SELECTED_BG);
        }
    }
    let log_top = plan.status_y.saturating_sub(plan.log_lines as u16);
    for (i, spans) in content::log_rows(m, usize::from(w), plan.log_lines)
        .iter()
        .enumerate()
    {
        c.text(x, log_top + i as u16, spans, w);
    }
}

const KEYS: [(&str, &str); 18] = [
    ("\u{2190}\u{2192}  h l", "select a sibling"),
    (
        "\u{2191}\u{2193}  k j",
        "level: seat \u{203a} agents \u{203a} children",
    ),
    ("Tab  S-Tab", "every node in turn"),
    ("\u{23ce}", "open its chat here; ^A t comes back"),
    ("y  d", "allow / deny an approval shown in full"),
    ("m", "mail the agent"),
    ("n", "nudge the agent"),
    ("x", "stop the agent (asks y / n first)"),
    ("+", "spawn a new agent"),
    ("r", "retry a failed pane"),
    ("A", "activity: everything, or the selected node's"),
    ("s", "scope: dashboard \u{203a} repo \u{203a} all"),
    ("PgUp  PgDn", "scroll the agent rows"),
    ("?", "this list"),
    ("Esc", "back to the dashboard (a chat keeps Esc)"),
    ("^A t", "from a chat the flow, from the flow the dashboard"),
    (
        "^A \u{2190}  ^A \u{2192}",
        "in a chat: the previous / next agent's chat",
    ),
    (
        "mouse",
        "click selects \u{b7} double-click opens chat \u{b7} wheel scrolls",
    ),
];

fn draw_help(c: &mut Canvas, area: Rect) {
    let w = 76.min(area.width.saturating_sub(2));
    let h = (KEYS.len() as u16 + 4).min(area.height.saturating_sub(2));
    if w < 30 || h < 5 {
        return;
    }
    let r = Rect {
        x: area.x + (area.width - w) / 2,
        y: area.y + (area.height - h) / 2,
        width: w,
        height: h,
    };
    for y in r.y..r.y + r.height {
        c.hline(r.x, r.x + r.width - 1, y, " ", Style::default());
    }
    c.boxed(r, pal::fg(pal::AGENT), &[span("keys", pal::bold())]);
    for (i, (key, what)) in KEYS.iter().enumerate() {
        let y = r.y + 2 + i as u16;
        if y + 1 >= r.y + r.height {
            break;
        }
        c.text(r.x + 2, y, &[span(*key, pal::bold())], 14);
        c.text(
            r.x + 17,
            y,
            &[span(content::fit(what, usize::from(w) - 19), pal::dim())],
            w - 19,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::super::TreeData;
    use super::super::testkit::*;
    use super::*;
    use ratatui::buffer::Buffer;
    use ratatui::style::Modifier;

    // These pin the phase-1 page tiers, so they draw it at any size; the orchestrator dashboard
    // that takes the page from 100 columns up is tested in `orch`.
    fn draw(width: u16, height: u16, view: &TreeView, facts: &TreeFacts) -> String {
        draw_classic(width, height, view, facts)
    }

    fn draw_buffer(
        width: u16,
        height: u16,
        view: &TreeView,
        facts: &TreeFacts,
    ) -> ratatui::buffer::Buffer {
        draw_classic_buffer(width, height, view, facts)
    }

    /// The cell holding `needle`'s first character, found by scanning rows.
    fn find(buffer: &Buffer, needle: &str) -> Option<(u16, u16)> {
        let first = needle.chars().next()?.to_string();
        let area = buffer.area;
        for y in 0..area.height {
            let row: String = (0..area.width)
                .map(|x| buffer[(x, y)].symbol().to_string())
                .collect();
            if let Some(at) = row.find(needle) {
                let x = row[..at].chars().count() as u16;
                if buffer[(x, y)].symbol() == first {
                    return Some((x, y));
                }
            }
        }
        None
    }

    fn fg_at(buffer: &Buffer, needle: &str) -> Color {
        let (x, y) = find(buffer, needle).unwrap_or_else(|| panic!("{needle:?} not drawn"));
        buffer[(x, y)].fg
    }

    #[test]
    fn full_layout_snapshot() {
        let jev = jev_fact();
        let text = draw(120, 40, &view(fixture()), &facts(Some(&jev)));
        for expected in [
            "ZIRV AGENT TREE",
            "CLAUDE FABLE SEAT",
            "this dashboard \u{b7} 3",
            "^A t \u{2192} dashboard",
            "SUPERVISOR",
            "off",
            "claude fable \u{b7} orchestrator",
            "effort \u{25ae}\u{25ae}\u{25ae}\u{25af} high",
            "$15 / $75 per 1M",
            "rot 0.18",
            "JEV \u{b7} decisions",
            "calls 214",
            "dispatch tier",
            "0.87 sharp",
            "spawn agents \u{b7} 3 of 6 slots \u{b7} writers cap 1",
            "worker",
            "codex sol",
            "\u{25d0} running",
            "explorer",
            "\u{2713} done",
            "\u{2514} reviewer",
            "12k tok",
            "session log",
            "jev        dispatch tier -> claude sonnet  p=0.87",
            "explorer   mapped 4 call sites",
            "agents [3/6]",
        ] {
            assert!(text.contains(expected), "missing {expected:?} in:\n{text}");
        }
        assert!(!text.contains('\u{1b}'), "control characters are stripped");
        assert!(
            !text.contains("spend") && !text.contains("3.10"),
            "no spend in the tree view:\n{text}"
        );
        // The seat is a card, not an agent box.
        assert_eq!(text.matches("orchestrator").count(), 1, "{text}");
    }

    #[test]
    fn the_phase_one_page_shows_a_with_its_sidecar_a_compact_a_and_the_list_by_size() {
        let (data, wf, jev) = busy();
        let mut f = facts(Some(&jev));
        f.workflow = Some(&wf);
        let v = view(data);
        let full = draw(120, 36, &v, &f);
        assert!(full.contains("SUPERVISOR"), "the sidecar:\n{full}");
        assert!(
            full.contains("\u{25bc}") && full.contains("\u{250c}"),
            "bus and boxes:\n{full}"
        );
        assert!(full.contains("CODEX GPT-6-ASTRA ON CALL"), "{full}");

        let compact = draw(80, 24, &v, &f);
        assert!(!compact.contains("SUPERVISOR"), "no sidecar:\n{compact}");
        assert!(
            compact.contains("supervisor on \u{b7} 1/3"),
            "folded into the header:\n{compact}"
        );
        assert!(
            compact.contains("JEV") && compact.contains("claude fable"),
            "{compact}"
        );
        assert!(
            compact.contains("worker") && compact.contains("\u{25bc}"),
            "{compact}"
        );
        let rows: Vec<&str> = compact.lines().collect();
        let seat = rows
            .iter()
            .position(|r| r.contains("claude fable \u{b7} orchestrator"))
            .expect("seat");
        let jev_row = rows.iter().position(|r| r.contains("JEV")).expect("jev");
        assert!(
            seat.abs_diff(jev_row) <= 1,
            "seat and jev share a row:\n{compact}"
        );

        let list = draw(70, 20, &v, &f);
        assert!(
            !list.contains('\u{250c}') && !list.contains('\u{25bc}'),
            "a plain list:\n{list}"
        );
        assert!(
            list.contains("worker") && list.contains("agents [3/6]"),
            "{list}"
        );
    }

    #[test]
    fn narrow_terminal_falls_back_to_an_indented_list() {
        let text = draw(70, 20, &view(fixture()), &facts(None));
        assert!(
            !text.contains('\u{250c}') && !text.contains('\u{2500}'),
            "no boxes:\n{text}"
        );
        let lines: Vec<&str> = text.lines().collect();
        let worker = lines.iter().find(|l| l.contains("worker")).expect("worker");
        let reviewer = lines
            .iter()
            .find(|l| l.contains("reviewer"))
            .expect("reviewer");
        assert!(worker.starts_with("\u{25d0} worker codex sol"), "{worker}");
        assert!(
            reviewer.starts_with("  \u{25cc} reviewer"),
            "child is indented: {reviewer}"
        );
        assert!(text.contains("agents [3/6]"));
    }

    #[test]
    fn the_legend_swatches_match_the_colours_of_what_they_label() {
        let (data, wf, jev) = busy();
        let mut f = facts(Some(&jev));
        f.workflow = Some(&wf);
        f.approval_shorts = vec!["w1".into()];
        let buffer = draw_buffer(160, 45, &view(data), &f);
        // The legend row: seat cyan, agents blue, jev green, supervisor purple.
        let legend_y = find(&buffer, "\u{25a0} seat").expect("legend").1;
        let swatches: Vec<Color> = (0..160)
            .filter(|x| buffer[(*x, legend_y)].symbol() == "\u{25a0}")
            .map(|x| buffer[(x, legend_y)].fg)
            .collect();
        assert_eq!(swatches, [pal::SEAT, pal::AGENT, pal::JEV, pal::ARCH]);
        // And each card's border is drawn in the colour its swatch promises.
        let border_left_of = |needle: &str| -> Color {
            let (x, y) = find(&buffer, needle).unwrap_or_else(|| panic!("{needle}"));
            (0..x)
                .rev()
                .find(|x| buffer[(*x, y)].symbol() == "\u{2502}")
                .map(|x| buffer[(x, y)].fg)
                .expect("a border to the left")
        };
        assert_eq!(
            border_left_of("claude fable \u{b7} orchestrator"),
            pal::SEAT
        );
        assert_eq!(border_left_of("claude haiku"), pal::AGENT);
        assert_eq!(fg_at(&buffer, "\u{250c}\u{2500} JEV"), pal::JEV);
        assert_eq!(fg_at(&buffer, "\u{250c}\u{2500} SUPERVISOR"), pal::ARCH);
        assert_eq!(fg_at(&buffer, "\u{250c}\u{2500} back to seat"), pal::SEAT);
    }

    #[test]
    fn status_glyphs_carry_the_status_colours() {
        let (data, wf, jev) = busy();
        let mut f = facts(Some(&jev));
        f.workflow = Some(&wf);
        f.approval_shorts = vec!["w1".into()];
        let buffer = draw_buffer(160, 45, &view(data), &f);
        assert_eq!(
            fg_at(&buffer, "\u{25d0} running"),
            pal::AGENT,
            "running is the agent blue"
        );
        assert_eq!(fg_at(&buffer, "\u{2713} completed"), Color::Green);
        assert_eq!(fg_at(&buffer, "\u{2717} failed"), Color::Red);
        assert_eq!(
            fg_at(&buffer, "\u{2691} approval"),
            Color::Yellow,
            "waiting on the operator"
        );
        let (x, y) = find(&buffer, "\u{25cc}").expect("queued marker in the workflow box");
        assert!(
            buffer[(x, y)].modifier.contains(Modifier::DIM),
            "queued is dim"
        );
    }

    #[test]
    fn an_agent_shows_its_own_effort_or_a_dash_and_never_the_seats() {
        let (data, _, _) = busy();
        let f = facts(None);
        let text = draw(160, 45, &view(data.clone()), &f);
        assert!(
            text.contains("effort \u{25ae}\u{25ae}\u{25af}\u{25af} med"),
            "the worker's own:\n{text}"
        );
        assert!(
            text.contains("effort \u{2014}"),
            "unknown effort is a dash:\n{text}"
        );
        let seat_effort = "effort \u{25ae}\u{25ae}\u{25ae}\u{25af} high";
        assert_eq!(
            text.matches(seat_effort).count(),
            1,
            "only the seat card says high:\n{text}"
        );
        // A seat with no known effort has no effort row at all.
        let mut bare = data;
        bare.nodes[0].effort = None;
        bare.seat_price = None;
        let text = draw(160, 45, &view(bare), &f);
        let seat_rows: Vec<&str> = text.lines().filter(|l| l.contains("effort")).collect();
        assert!(
            seat_rows
                .iter()
                .all(|l| l.contains('\u{2014}') || l.contains("med")),
            "no blank effort row in the seat card: {seat_rows:?}"
        );
    }

    #[test]
    fn the_workflow_box_names_the_pack_the_step_and_the_marks() {
        let (data, wf, jev) = busy();
        let mut f = facts(Some(&jev));
        f.workflow = Some(&wf);
        let text = draw(120, 40, &view(data.clone()), &f);
        for expected in [
            "back to seat",
            "workflow feature \u{b7} implement",
            "plan \u{2713}",
            "implement \u{25d0}",
            "review \u{25cc}",
            "verify \u{25cc}",
        ] {
            assert!(text.contains(expected), "missing {expected:?}:\n{text}");
        }
        let none = draw(120, 40, &view(data), &facts(Some(&jev)));
        assert!(
            !none.contains("back to seat"),
            "no workflow, no box:\n{none}"
        );
    }

    #[test]
    fn the_supervisor_sidecar_shows_its_state_and_highlights_the_moment_that_fired() {
        let (data, _, _) = busy();
        let buffer = draw_buffer(160, 45, &view(data.clone()), &facts(None));
        let text = draw(160, 45, &view(data.clone()), &facts(None));
        for expected in [
            "codex gpt-6-astra \u{b7} on call",
            "\u{25c7} before a plan",
            "\u{25c6} error repeats",
            "\u{25c7} before done",
            "\u{bb} fixture path wrong",
            "calls 1/3",
            "tokens read 224k",
        ] {
            assert!(text.contains(expected), "missing {expected:?}:\n{text}");
        }
        let (x, y) = find(&buffer, "\u{25c6} error repeats").expect("fired moment");
        assert_eq!(
            buffer[(x, y)].bg,
            pal::FIRED_BG,
            "the fired moment is highlighted"
        );
        let (px, py) = find(&buffer, "\u{25c7} before a plan").expect("idle moment");
        assert_ne!(buffer[(px, py)].bg, pal::FIRED_BG);
        assert!(
            text.contains('\u{254c}') && text.contains('\u{25b6}'),
            "dashed arrows into the flow:\n{text}"
        );
        // Off: one dim line, and the compact tier hides the sidecar altogether.
        let mut off = data;
        off.supervisor = None;
        let text = draw(160, 45, &view(off), &facts(None));
        assert!(
            text.contains("SUPERVISOR") && text.contains("off"),
            "{text}"
        );
        assert!(!text.contains("last advice"), "{text}");
    }

    #[test]
    fn mail_between_nodes_reaches_the_log_by_name_and_marks_the_box() {
        let (data, _, _) = busy();
        let text = draw(160, 45, &view(data), &facts(None));
        assert!(
            text.contains(
                "worker     \u{2709} \u{2192} seat  run the migration tests before review"
            ),
            "mail edge named by node:\n{text}"
        );
        assert!(
            text.contains("worker \u{2709}2"),
            "the box carries its mail count:\n{text}"
        );
        assert!(
            !text.contains("seat-1  "),
            "no raw session ids in the log:\n{text}"
        );
    }

    #[test]
    fn the_footer_never_cuts_a_key_hint_at_any_width() {
        let (data, _, jev) = busy();
        let mut f = facts(Some(&jev));
        f.approvals = 2;
        f.approval_shorts = vec!["w1".into()];
        let mut v = view(data);
        v.selected = Sel::Agent("w1".into());
        let all = [
            "\u{2190}\u{2192} select",
            "\u{23ce} open",
            "y/d answer",
            "s scope",
            "? keys",
            "^A t dashboard",
        ];
        let mut smallest_with_all = None;
        for width in 20..=200u16 {
            let text = draw(width, 24.max(if width >= 100 { 34 } else { 24 }), &v, &f);
            let last = text.lines().last().expect("footer").to_string();
            assert!(
                last.contains("^A t dashboard"),
                "the way back is always shown ({width}): {last:?}"
            );
            for hint in all {
                let key = hint.split(' ').next().expect("key");
                let label = hint.rsplit(' ').next().expect("label");
                if last.contains(&format!("{key} ")) && last.contains(label) {
                    continue;
                }
                assert!(
                    !last.contains(&format!("{key} {}", &label[..1])),
                    "a hint is whole or absent ({width}): {last:?}"
                );
            }
            if all.iter().all(|h| last.contains(h)) && smallest_with_all.is_none() {
                smallest_with_all = Some(width);
            }
        }
        assert!(
            smallest_with_all.is_some_and(|w| w <= 120),
            "every hint fits at a normal width"
        );
    }

    #[test]
    fn the_empty_state_says_what_to_do_and_loading_says_gathering() {
        let empty = TreeData {
            loaded: true,
            ..TreeData::default()
        };
        let mut f = facts(None);
        f.rot = None;
        let text = draw(120, 36, &view(empty.clone()), &f);
        assert!(
            text.contains("no agents yet \u{b7} they appear here when the seat dispatches"),
            "{text}"
        );
        assert!(text.contains("^A s"), "the spawn chord:\n{text}");
        assert!(text.contains("widens the view to this repo"), "{text}");
        let seat_rows: Vec<&str> = text.lines().filter(|l| l.contains("effort")).collect();
        assert!(
            seat_rows.is_empty(),
            "no blank effort row in the seat card: {seat_rows:?}"
        );
        let loading = draw(120, 36, &view(TreeData::default()), &f);
        assert!(loading.contains("gathering\u{2026}"), "{loading}");
        assert!(!loading.contains("no agents yet"), "{loading}");
    }

    #[test]
    fn the_key_list_names_every_key_and_mouse_action() {
        let mut v = view(fixture());
        v.help = true;
        let text = draw(120, 36, &v, &facts(None));
        for expected in [
            "keys",
            "select a sibling",
            "open its chat here",
            "scope",
            "click selects",
            "wheel scrolls",
        ] {
            assert!(text.contains(expected), "missing {expected:?}:\n{text}");
        }
    }

    #[test]
    fn an_open_chat_is_one_bar_naming_the_focused_agent() {
        let (data, _, _) = busy();
        let mut f = facts(None);
        f.focused = Some(("w1".into(), "worker".into(), "codex".into()));
        let mut v = view(data);
        v.open_chat();
        let text = draw(120, 1, &v, &f);
        assert_eq!(
            text.trim(),
            "flow \u{203a} worker \u{b7} codex gpt-6-sol \u{b7} effort \u{25ae}\u{25ae}\u{25af}\u{25af} med \u{b7} ^A t back to flow"
        );
        let narrow = draw(50, 1, &v, &f);
        assert!(
            narrow.contains("^A t back to flow"),
            "the way back survives: {narrow}"
        );
    }

    /// Issue #840: the approvals count joins the footer only while something is pending.
    #[test]
    fn the_footer_shows_an_approvals_count_only_while_something_is_pending() {
        let idle = content::footer_label(&TreeData::default(), &facts(None));
        assert_eq!(idle, " agents [3/6]   supervisor [off]   jev [off]");
        let mut f = facts(None);
        f.approvals = 2;
        let pending = content::footer_label(&TreeData::default(), &f);
        assert_eq!(pending, format!("{idle}   \u{2691} approvals [2]"));
    }

    #[test]
    fn an_avoided_model_gets_a_badge_and_nothing_else_changes() {
        let plain = draw(160, 45, &view(fixture()), &facts(None));
        assert!(!plain.contains("[avoid]"), "{plain}");
        let mut data = fixture();
        data.avoided.insert("sol".into());
        let badged = draw(160, 45, &view(data.clone()), &facts(None));
        assert_eq!(badged.matches("[avoid]").count(), 1, "{badged}");
        let list = draw(70, 20, &view(data), &facts(None));
        assert!(list.contains("sol [avoid]"), "{list}");
    }

    #[test]
    fn disabled_features_say_off_never_an_empty_box() {
        let text = draw(120, 40, &view(fixture()), &facts(None));
        assert!(text.contains("JEV"), "{text}");
        assert!(text.contains("jev [off]"), "{text}");
        assert!(text.contains("supervisor [off]"), "{text}");
    }

    #[test]
    fn a_crowd_of_children_folds_into_the_first_two_and_a_count() {
        let mut data = fixture();
        for i in 0..5 {
            data.nodes.push(node(
                &format!("k{i}"),
                Some("w1"),
                &format!("kid-{i}"),
                "m",
                "running",
            ));
        }
        let text = draw(160, 45, &view(data.clone()), &facts(None));
        assert!(
            text.contains("\u{2514} reviewer") && text.contains("\u{2514} kid-0"),
            "{text}"
        );
        assert!(text.contains("+4 more"), "{text}");
        assert!(!text.contains("kid-4"), "{text}");
        // Selecting a hidden child scrolls the box's list so it stays visible.
        let mut v = view(data);
        v.selected = Sel::Child("k4".into());
        let text = draw(160, 45, &v, &facts(None));
        assert!(text.contains("kid-4"), "{text}");
    }

    #[test]
    fn many_agents_scroll_with_a_count_instead_of_a_plus_n_more() {
        let mut f = facts(None);
        f.pane_shorts = (0..24)
            .map(|i| format!("n{i:02}"))
            .chain(["seat1".into()])
            .collect();
        let mut v = view(huge());
        v.scroll = 2;
        let wide = draw(200, 40, &v, &f);
        let hint = wide
            .lines()
            .find(|l| l.contains("spawn agents"))
            .expect("spawn row");
        assert!(hint.contains(" above") && hint.contains(" below"), "{hint}");
        // A narrow flow keeps the counts in their short form rather than dropping them.
        let text = draw(120, 36, &v, &f);
        assert!(
            text.contains("\u{25b2}") && text.contains("\u{25bc}"),
            "{text}"
        );
        let top = draw(120, 36, &view(huge()), &f);
        assert!(
            !top.contains("\u{25b2}"),
            "nothing above the first row:\n{top}"
        );
        assert!(!top.contains("+13 more"), "the old fold is gone:\n{top}");
    }

    /// The orchestrator dashboard hands the flow a rect; the layout follows that rect's size and
    /// nothing is drawn outside it or as page chrome.
    #[test]
    fn the_flow_renders_into_a_given_rect_and_picks_its_layout_from_that_rect() {
        let (data, wf, jev) = busy();
        let mut f = facts(Some(&jev));
        f.workflow = Some(&wf);
        let v = view(data);
        let panel = |rect: Rect| -> Buffer {
            let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(160, 45))
                .expect("terminal");
            terminal
                .draw(|frame| render_flow(frame, rect, &v, &f))
                .expect("draw");
            terminal.backend().buffer().clone()
        };
        let text_of = |buffer: &Buffer| -> String {
            (0..45)
                .map(|y| {
                    (0..160)
                        .map(|x| buffer[(x, y)].symbol().to_string())
                        .collect::<String>()
                })
                .collect::<Vec<_>>()
                .join("\n")
        };
        let rect = Rect::new(24, 3, 112, 32);
        let buffer = panel(rect);
        let text = text_of(&buffer);
        assert!(
            text.contains("SUPERVISOR") && text.contains("spawn agents"),
            "full A:\n{text}"
        );
        for chrome in [
            "ZIRV AGENT TREE",
            "session log",
            "^A t dashboard",
            "\u{25a0} seat",
        ] {
            assert!(!text.contains(chrome), "{chrome:?} is page chrome:\n{text}");
        }
        for y in 0..45u16 {
            for x in 0..160u16 {
                let inside = x >= rect.x
                    && x < rect.x + rect.width
                    && y >= rect.y
                    && y < rect.y + rect.height;
                assert!(
                    inside || buffer[(x, y)].symbol() == " ",
                    "({x},{y}) is outside the rect"
                );
            }
        }
        let small = text_of(&panel(Rect::new(10, 2, 70, 20)));
        assert!(
            !small.contains("SUPERVISOR"),
            "compact A in a 70x20 rect:\n{small}"
        );
        assert!(
            small.contains("JEV") && small.contains("worker") && small.contains("back to seat"),
            "{small}"
        );
        let tiny = text_of(&panel(Rect::new(0, 0, 50, 10)));
        assert!(
            tiny.contains("worker") && !tiny.contains('\u{250c}'),
            "a list below 60x18:\n{tiny}"
        );
    }

    fn symbols(buffer: &Buffer, y: u16, x0: u16, x1: u16) -> String {
        (x0..=x1)
            .map(|x| buffer[(x, y)].symbol().to_string())
            .collect()
    }

    #[test]
    fn a_fan_out_bus_ends_the_line_from_above_in_a_tee_up_and_a_converging_bus_drops_a_tee_down() {
        let mut buffer = Buffer::empty(Rect::new(0, 0, 60, 3));
        let mut c = Canvas {
            bounds: buffer.area,
            buf: &mut buffer,
        };
        let style = pal::fg(pal::AGENT);
        let xs = [10, 25, 40, 55];
        bus(&mut c, 0, &xs, &[32], true, style);
        bus(&mut c, 2, &xs, &[32], false, style);
        let fan = symbols(&buffer, 0, 10, 55);
        let conv = symbols(&buffer, 2, 10, 55);
        let at = |s: &str, x: usize| s.chars().nth(x - 10);
        assert_eq!(
            at(&fan, 32),
            Some('\u{2534}'),
            "┴ where the line from above meets the bus"
        );
        assert_eq!(
            at(&conv, 32),
            Some('\u{252c}'),
            "┬ where the drop leaves it"
        );
        for x in [25, 40] {
            assert_eq!(at(&fan, x), Some('\u{252c}'), "┬ over an arrow: {fan}");
            assert_eq!(at(&conv, x), Some('\u{2534}'), "┴ under a card: {conv}");
        }
        assert!(
            fan.starts_with('\u{250c}') && fan.ends_with('\u{2510}'),
            "{fan}"
        );
        assert!(
            conv.starts_with('\u{2514}') && conv.ends_with('\u{2518}'),
            "{conv}"
        );
    }

    #[test]
    fn the_flow_draws_the_tees_the_right_way_round() {
        let (data, wf, jev) = busy();
        let mut f = facts(Some(&jev));
        f.workflow = Some(&wf);
        let text = draw_classic(120, 40, &view(data), &f);
        let rows: Vec<&str> = text.lines().collect();
        let fan = rows
            .iter()
            .position(|r| {
                r.contains('\u{250c}') && r.contains('\u{2534}') && r.contains('\u{25cf}')
            })
            .expect("a fan-out bus with a ┴");
        assert!(
            rows[fan + 1].contains('\u{25bc}'),
            "the arrows follow the bus:\n{text}"
        );
        let conv = rows
            .iter()
            .position(|r| {
                r.contains('\u{2514}')
                    && r.contains('\u{252c}')
                    && r.contains('\u{2518}')
                    && r.contains('\u{25cf}')
            })
            .expect("a converging bus with a ┬");
        assert!(
            rows[conv + 1].contains('\u{25bc}'),
            "the drop leads to back to seat:\n{text}"
        );
        assert!(
            !rows.iter().any(|r| r.contains('\u{253c}')),
            "the flow has no cross:\n{text}"
        );
    }

    #[test]
    fn compact_a_hangs_a_line_from_the_seat_and_from_jev_down_to_the_bus() {
        let (data, wf, jev) = busy();
        let mut f = facts(Some(&jev));
        f.workflow = Some(&wf);
        let buffer = draw_classic_buffer(90, 30, &view(data), &f);
        // The cards' bottom edges each carry a ┬; below it a line runs to the bus.
        let tee_row = (0..30u16)
            .find(|&y| symbols(&buffer, y, 0, 89).matches('\u{252c}').count() >= 2)
            .expect("both cards hang a line");
        let tees: Vec<u16> = (0..90u16)
            .filter(|&x| buffer[(x, tee_row)].symbol() == "\u{252c}")
            .collect();
        let bus_row = (tee_row + 1..30)
            .find(|&y| symbols(&buffer, y, 0, 89).contains('\u{25cf}'))
            .expect("the bus");
        assert!(bus_row > tee_row + 1, "the line has a row to run in");
        for &x in &tees[..2] {
            for y in tee_row + 1..bus_row {
                assert_eq!(
                    buffer[(x, y)].symbol(),
                    "\u{2502}",
                    "the vertical at {x},{y}"
                );
            }
            let junction = buffer[(x, bus_row)].symbol().to_string();
            assert!(
                ["\u{2534}", "\u{2514}", "\u{2518}"].contains(&junction.as_str()),
                "it meets the bus from above: {junction}"
            );
        }
    }

    /// No terminal size, page or panel, may panic or paint outside its rect.
    #[test]
    fn every_size_draws_without_panicking_or_leaving_its_rect() {
        let (busy_data, wf, jev) = busy();
        let mut rich = facts(Some(&jev));
        rich.workflow = Some(&wf);
        rich.approvals = 3;
        rich.approval_shorts = vec!["w1".into()];
        let mut crowded = facts(None);
        crowded.pane_shorts = (0..24)
            .map(|i| format!("n{i:02}"))
            .chain(["seat1".into()])
            .collect();
        let scenes: Vec<(TreeView, &TreeFacts)> = vec![
            (view(busy_data.clone()), &rich),
            (view(huge()), &crowded),
            (
                view(TreeData {
                    loaded: true,
                    ..TreeData::default()
                }),
                &crowded,
            ),
            (view(TreeData::default()), &crowded),
        ];
        for (v, f) in &scenes {
            for (w, h) in (4..=130u16)
                .step_by(7)
                .flat_map(|w| (1..=50u16).step_by(3).map(move |h| (w, h)))
            {
                for panel in [false, true] {
                    let rect = Rect::new(1.min(w - 1), 0, w - 1.min(w - 1), h);
                    let mut terminal =
                        ratatui::Terminal::new(ratatui::backend::TestBackend::new(w, h))
                            .expect("terminal");
                    terminal
                        .draw(|frame| {
                            if panel {
                                render_flow(frame, rect, v, f)
                            } else {
                                render(frame, rect, v, f)
                            }
                        })
                        .expect("draw");
                    let buffer = terminal.backend().buffer();
                    for x in 0..w {
                        if x < rect.x {
                            assert_eq!(buffer[(x, 0)].symbol(), " ", "{w}x{h} left of the rect");
                        }
                    }
                }
            }
        }
        let mut selected = view(huge());
        for sel in [
            Sel::Agent("n03".into()),
            Sel::Child("n03k0".into()),
            Sel::Agent("gone".into()),
        ] {
            selected.selected = sel;
            for (w, h) in [(160, 45), (120, 36), (100, 34), (80, 24), (70, 20), (30, 8)] {
                let _ = draw(w, h, &selected, &crowded);
            }
        }
    }
}
