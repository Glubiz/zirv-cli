//! The tier and the geometry. A [`Plan`] is a pure function of the terminal size, the
//! model and the view state; the painter draws it and the mouse hit-tests it, so they
//! can never disagree about where a box is.

use ratatui::layout::Rect;

use super::super::ui::JevSectionFact;
use super::content::{self, BoxSpec, Footer, Row};
use super::model::{Model, Sel};

/// Mockup A in full, the same without the supervisor sidecar, or a plain list.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Tier {
    Full,
    Compact,
    List,
}

/// The smallest terminal that shows the whole page (header, legend, log, statusline) in each tier.
pub(super) const FULL_MIN: (u16, u16) = (100, 34);
pub(super) const COMPACT_MIN: (u16, u16) = (80, 24);
/// The same for the flow alone, drawn into a rect with none of that chrome around it.
pub(super) const PANEL_FULL_MIN: (u16, u16) = (100, 30);
pub(super) const PANEL_COMPACT_MIN: (u16, u16) = (60, 18);

/// Where the tree draws: the whole page, or just the flow inside a rect an enclosing view gave it.
#[derive(Debug, Clone, Copy)]
pub(in super::super) struct Surface {
    pub(super) area: Rect,
    pub(super) panel: bool,
}

impl Surface {
    /// The whole terminal: header, legend, flow, session log and statusline.
    pub(in super::super) fn page(area: Rect) -> Self {
        Self { area, panel: false }
    }

    /// The flow only (sidecar, seat, Jev, agents, back to seat) inside `area`.
    #[cfg(test)]
    pub(in super::super) fn panel(area: Rect) -> Self {
        Self { area, panel: true }
    }
}

pub(super) fn tier_for(width: u16, height: u16, panel: bool) -> Tier {
    let (full, compact) = if panel {
        (PANEL_FULL_MIN, PANEL_COMPACT_MIN)
    } else {
        (FULL_MIN, COMPACT_MIN)
    };
    if width >= full.0 && height >= full.1 {
        Tier::Full
    } else if width >= compact.0 && height >= compact.1 {
        Tier::Compact
    } else {
        Tier::List
    }
}

/// What the plan needs from the view beyond the data.
pub(super) struct ViewState<'v> {
    pub(super) selected: &'v Sel,
    pub(super) scroll: usize,
    pub(super) error: Option<&'v str>,
}

pub(super) struct Cell {
    pub(super) rect: Rect,
    pub(super) agent: usize,
    pub(super) rows: Vec<Row>,
}

pub(super) struct BoxRow {
    pub(super) bus_y: u16,
    pub(super) arrow_y: u16,
    pub(super) cells: Vec<Cell>,
}

pub(super) struct ListRow {
    pub(super) y: u16,
    pub(super) sel: Sel,
    pub(super) depth: usize,
}

/// A card height chosen from the ladder of profiles.
#[derive(Debug, Clone, Copy)]
struct Prof {
    rule: bool,
    seat_lines: usize,
    legend: bool,
    sites: usize,
    spec: BoxSpec,
    back_lines: usize,
    log_lines: usize,
}

pub(super) struct Plan {
    pub(super) tier: Tier,
    pub(super) panel: bool,
    pub(super) area: Rect,
    pub(super) header_y: u16,
    pub(super) rule_y: Option<u16>,
    pub(super) legend_y: u16,
    pub(super) side: Option<Rect>,
    pub(super) seat: Rect,
    pub(super) seat_lines: usize,
    pub(super) jev: Rect,
    pub(super) jev_sites: usize,
    /// The orchestrator's flow leaves the Jev box out while Jev is off.
    pub(super) jev_hidden: bool,
    pub(super) jev_legend: bool,
    pub(super) cx: u16,
    pub(super) spawn_y: u16,
    pub(super) flow: Rect,
    pub(super) rows: Vec<BoxRow>,
    pub(super) above: usize,
    pub(super) below: usize,
    pub(super) per_row: usize,
    pub(super) vis_rows: usize,
    pub(super) total_rows: usize,
    pub(super) first_row: usize,
    /// Where the empty or loading message goes.
    pub(super) empty: Option<Rect>,
    pub(super) back: Option<Rect>,
    pub(super) back_lines: usize,
    pub(super) conv_y: Option<u16>,
    pub(super) log: Option<Rect>,
    pub(super) log_lines: usize,
    pub(super) status_y: u16,
    /// Rows of the sidecar's three moments and where each arrow points.
    pub(super) moments: [Option<u16>; 3],
    pub(super) moment_to: [Option<u16>; 3],
    pub(super) list: Vec<ListRow>,
    pub(super) hits: Vec<(Rect, Sel)>,
    pub(super) footer: Footer,
}

impl Plan {
    /// The first hit under a cell: children before their box, boxes before nothing.
    pub(super) fn hit(&self, x: u16, y: u16) -> Option<&Sel> {
        self.hits
            .iter()
            .find(|(rect, _)| {
                x >= rect.x && x < rect.x + rect.width && y >= rect.y && y < rect.y + rect.height
            })
            .map(|(_, sel)| sel)
    }

    /// The footer hint under a cell.
    pub(super) fn hint_at(&self, x: u16, y: u16) -> Option<content::Hint> {
        if y != self.status_y {
            return None;
        }
        self.footer
            .hints
            .iter()
            .find(|p| x >= p.x && (x as usize) < p.x as usize + p.hint.width())
            .map(|p| p.hint)
    }

    /// Compact tier: the columns where the seat and Jev cards each hang a line down to the first
    /// bus, left to right. Empty in the other tiers and while there is no bus.
    pub(super) fn hanging_lines(&self) -> Vec<u16> {
        if self.tier != Tier::Compact || self.rows.is_empty() {
            return Vec::new();
        }
        let mut xs = vec![self.seat.x + self.seat.width / 2];
        if !self.jev_hidden {
            xs.push(self.jev.x + self.jev.width / 2);
        }
        xs.sort_unstable();
        xs
    }

    /// The columns a vertical line crosses the spawn row at, whatever the tier: the label never
    /// sits on one.
    pub(super) fn spawn_verticals(&self) -> Vec<u16> {
        match self.tier {
            Tier::Full if !self.rows.is_empty() => vec![self.cx],
            _ => self.hanging_lines(),
        }
    }

    /// True over the area the wheel scrolls.
    pub(super) fn over_agents(&self, y: u16) -> bool {
        match self.tier {
            Tier::List => true,
            _ => y >= self.spawn_y && y < self.flow.y + self.flow.height,
        }
    }

    /// The box row an agent index sits in.
    pub(super) fn row_of(&self, agent: usize) -> usize {
        agent / self.per_row.max(1)
    }
}

fn full_ladder() -> Vec<Prof> {
    let mut p = Prof {
        rule: true,
        seat_lines: 3,
        legend: true,
        sites: 3,
        spec: BoxSpec {
            core: 4,
            detail: 2,
            kid_cap: 3,
        },
        back_lines: 2,
        log_lines: 4,
    };
    let mut out = vec![p];
    let mut step = |p: &mut Prof, apply: &dyn Fn(&mut Prof)| {
        apply(p);
        out.push(*p);
    };
    step(&mut p, &|p| p.rule = false);
    step(&mut p, &|p| p.log_lines = 3);
    step(&mut p, &|p| p.legend = false);
    step(&mut p, &|p| p.log_lines = 2);
    step(&mut p, &|p| p.spec.detail = 1);
    step(&mut p, &|p| p.seat_lines = 2);
    step(&mut p, &|p| {
        p.spec.kid_cap = 2;
        p.back_lines = 1;
    });
    step(&mut p, &|p| {
        p.spec.detail = 0;
        p.log_lines = 1;
    });
    step(&mut p, &|p| {
        p.spec.kid_cap = 1;
        p.sites = 2;
    });
    step(&mut p, &|p| {
        p.sites = 1;
        p.spec.core = 3;
    });
    out
}

fn compact_ladder() -> Vec<Prof> {
    let mut p = Prof {
        rule: false,
        seat_lines: 3,
        legend: false,
        sites: 3,
        spec: BoxSpec {
            core: 4,
            detail: 1,
            kid_cap: 2,
        },
        back_lines: 2,
        log_lines: 2,
    };
    let mut out = vec![p];
    let mut step = |p: &mut Prof, apply: &dyn Fn(&mut Prof)| {
        apply(p);
        out.push(*p);
    };
    step(&mut p, &|p| p.log_lines = 1);
    step(&mut p, &|p| p.back_lines = 1);
    step(&mut p, &|p| p.spec.kid_cap = 1);
    step(&mut p, &|p| {
        p.seat_lines = 2;
        p.sites = 2;
    });
    step(&mut p, &|p| p.spec.detail = 0);
    step(&mut p, &|p| p.spec.core = 3);
    step(&mut p, &|p| {
        p.sites = 1;
        p.log_lines = 0;
    });
    out
}

const EMPTY_ROWS: usize = 4;
const MAX_LOG_LINES: usize = 6;

pub(super) fn build(surf: Surface, m: &Model, v: &ViewState) -> Plan {
    let Surface { area, panel } = surf;
    match tier_for(area.width, area.height, panel) {
        Tier::List => list_plan(area, m, v, panel),
        tier => stacked(area, m, v, tier == Tier::Full, panel),
    }
}

fn rect(x: u16, y: u16, width: u16, height: u16) -> Rect {
    Rect {
        x,
        y,
        width,
        height,
    }
}

fn stacked(area: Rect, m: &Model, v: &ViewState, full: bool, panel: bool) -> Plan {
    let (w, h) = (area.width, area.height as usize);
    let n = m.agents.len();
    let has_back = m.facts.workflow.is_some();
    let real_sites = content::jev_site_count(m.facts);
    let sites_len = real_sites.max(1);
    // Rows the seat card has to show; a card never reserves a blank row.
    let seat_cap = content::seat_rows(m, 3, 60).len();
    // The orchestrator's flow draws no box for what is off, and the cards get its room.
    let jev_hidden = panel && !matches!(m.facts.jev, Some(JevSectionFact::Active { .. }));
    let arch_hidden = panel && m.data.supervisor.is_none();
    let (min_w, gap) = if full { (19u16, 2u16) } else { (16, 1) };
    let side_w = if full && !arch_hidden {
        (w * 11 / 50).clamp(22, 32)
    } else {
        0
    };
    let flow_x = area.x + 1 + if side_w > 0 { side_w + 2 } else { 0 };
    let flow_w = (area.x + w).saturating_sub(flow_x + 1);
    let cx = flow_x + flow_w / 2;
    let per_row = (((flow_w + gap) / (min_w + gap)).max(1)) as usize;
    let box_w =
        ((flow_w.saturating_sub(gap * (per_row as u16 - 1))) / per_row as u16).clamp(10, 26);
    let total_rows = n.div_ceil(per_row);

    let box_h_for = |spec: BoxSpec| -> usize {
        2 + m
            .agents
            .iter()
            .map(|a| content::agent_rows(m, a, spec, &Sel::Seat, box_w as usize - 2).len())
            .max()
            .unwrap_or(1)
            .max(1)
    };
    let need = |p: &Prof| -> usize {
        let region = if n == 0 {
            EMPTY_ROWS
        } else {
            2 + box_h_for(p.spec)
        };
        let jev_rows = p.sites.min(sites_len);
        let seat_lines = p.seat_lines.min(seat_cap);
        let legend = usize::from(p.legend && real_sites > 0);
        let back = if has_back { 4 + p.back_lines } else { 0 };
        // The page adds a header, legend, session log and statusline around the flow.
        let (top, log, status) = match (panel, full) {
            (true, _) => (0, 0, 0),
            (false, true) => (if p.rule { 4 } else { 2 }, 2 + p.log_lines, 1),
            (false, false) => (2, if p.log_lines > 0 { 2 + p.log_lines } else { 0 }, 1),
        };
        if full {
            let jev = if jev_hidden {
                0
            } else {
                (2 + jev_rows + legend) + 1
            };
            let flow = (2 + seat_lines) + 1 + jev + 1 + region;
            top + flow + back + log + status
        } else {
            let cards = 2 + seat_lines.max(if jev_hidden { 0 } else { jev_rows });
            top + cards + 1 + region + back + log + status
        }
    };
    let ladder = if full {
        full_ladder()
    } else {
        compact_ladder()
    };
    let fitting = ladder.iter().copied().filter(|p| need(p) <= h);
    let prof = if panel && n > 0 {
        // Show every agent when some profile can; else the one that shows the most rows.
        let rows_of = |p: &Prof| {
            let room = h - need(p);
            (1 + room / (box_h_for(p.spec) + 2)).min(total_rows)
        };
        fitting
            .clone()
            .find(|p| rows_of(p) == total_rows)
            .or_else(|| {
                fitting
                    .clone()
                    .fold(None, |best: Option<Prof>, p| match best {
                        Some(b) if rows_of(&b) >= rows_of(&p) => Some(b),
                        _ => Some(p),
                    })
            })
    } else {
        fitting.clone().next()
    }
    .unwrap_or_else(|| *ladder.last().expect("ladder is never empty"));
    let box_h = box_h_for(prof.spec);
    let mut spare = h.saturating_sub(need(&prof));
    let mut vis_rows = usize::from(n > 0);
    if n > 0 {
        let extra = (spare / (box_h + 2)).min(total_rows.saturating_sub(1));
        vis_rows += extra;
        spare -= extra * (box_h + 2);
    }
    let mut log_lines = prof.log_lines;
    if !panel && (full || log_lines > 0) {
        log_lines += spare.min(MAX_LOG_LINES.saturating_sub(log_lines));
    }
    let jev_sites = prof.sites.min(sites_len);
    let seat_lines = prof.seat_lines.min(seat_cap);
    let jev_legend = prof.legend && real_sites > 0;

    let mut y = area.y;
    let header_y = y;
    let (rule_y, legend_y);
    if panel {
        (rule_y, legend_y) = (None, y);
    } else {
        y += 1;
        rule_y = prof.rule.then_some(y);
        y += u16::from(prof.rule);
        legend_y = y;
        y += 1 + u16::from(prof.rule);
    }
    let body_top = y;

    let (seat, jev);
    if full {
        let seat_w = flow_w.min(46);
        let jev_w = flow_w.min(58);
        let seat_h = 2 + seat_lines as u16;
        seat = rect(cx.saturating_sub(seat_w / 2), y, seat_w, seat_h);
        y += seat_h + 1;
        if jev_hidden {
            jev = rect(cx, y, 0, 0);
        } else {
            let jev_h = 2 + jev_sites as u16 + u16::from(jev_legend);
            jev = rect(cx.saturating_sub(jev_w / 2), y, jev_w, jev_h);
            y += jev_h + 1;
        }
    } else {
        let seat_w = (flow_w * 2 / 5).max(30).min(flow_w / 2);
        let cards_h = 2 + seat_lines.max(if jev_hidden { 0 } else { jev_sites }) as u16;
        if jev_hidden {
            seat = rect(cx.saturating_sub(seat_w / 2), y, seat_w, cards_h);
            jev = rect(cx, y, 0, 0);
        } else {
            seat = rect(flow_x, y, seat_w, cards_h);
            jev = rect(
                flow_x + seat_w + 1,
                y,
                flow_w.saturating_sub(seat_w + 1),
                cards_h,
            );
        }
        y += cards_h;
    }
    let spawn_y = y;
    y += 1;
    let flow_top = y;

    let first_row = v.scroll.min(total_rows.saturating_sub(vis_rows));
    let mut rows = Vec::new();
    let mut empty = None;
    if n == 0 {
        empty = Some(rect(flow_x, y, flow_w, EMPTY_ROWS as u16));
        y += EMPTY_ROWS as u16;
    }
    for r in first_row..first_row + vis_rows.min(total_rows) {
        let from = r * per_row;
        let to = (from + per_row).min(n);
        let k = (to - from) as u16;
        let group_w = k * box_w + gap * k.saturating_sub(1);
        let start = cx
            .saturating_sub(group_w / 2)
            .max(flow_x)
            .min((flow_x + flow_w).saturating_sub(group_w));
        let (bus_y, top) = (y, y + 2);
        let cells = (from..to)
            .map(|agent| {
                let col = (agent - from) as u16;
                Cell {
                    rect: rect(start + col * (box_w + gap), top, box_w, box_h as u16),
                    agent,
                    rows: content::agent_rows(
                        m,
                        &m.agents[agent],
                        prof.spec,
                        v.selected,
                        box_w as usize - 2,
                    ),
                }
            })
            .collect();
        rows.push(BoxRow {
            bus_y,
            arrow_y: y + 1,
            cells,
        });
        y = top + box_h as u16;
    }
    let flow = rect(flow_x, flow_top, flow_w, y - flow_top);
    let shown_to = (first_row + rows.len()) * per_row;
    let (above, below) = (first_row * per_row, n.saturating_sub(shown_to.min(n)));

    let mut back = None;
    let mut conv_y = None;
    if has_back {
        let back_h = 2 + prof.back_lines as u16;
        let back_w = flow_w.min(50);
        conv_y = Some(y);
        back = Some(rect(cx.saturating_sub(back_w / 2), y + 2, back_w, back_h));
        y += 2 + back_h;
    }
    let status_y = if panel {
        u16::MAX
    } else {
        area.y + area.height - 1
    };
    let bottom_edge = if panel {
        area.y + area.height
    } else {
        status_y
    };
    let log = (!panel && (full || log_lines > 0)).then(|| {
        // A quiet session needs one line to say so, not a box of blanks.
        let wanted = log_lines.min(m.data.events.len().max(1));
        let log_h = (2 + wanted as u16).min(status_y.saturating_sub(y));
        rect(area.x + 1, status_y - log_h, w.saturating_sub(2), log_h)
    });
    let log = log.filter(|l| l.height >= 3);
    let log_lines = log.map_or(0, |l| l.height as usize - 2);

    let side = (full && !arch_hidden).then(|| {
        let bottom = log.map_or(bottom_edge, |l| l.y);
        let height = if m.data.supervisor.is_some() {
            bottom.saturating_sub(body_top)
        } else {
            3
        };
        rect(area.x + 1, body_top, side_w, height)
    });
    let moments = if full && m.data.supervisor.is_some() {
        let last_row = log.map_or(bottom_edge, |l| l.y).saturating_sub(2);
        [
            Some(seat.y + 2),
            Some(rows.first().and_then(|r| r.cells.first()).map_or_else(
                || (seat.y + 2 + last_row) / 2,
                |c| c.rect.y + c.rect.height / 2,
            )),
            Some(back.map_or(last_row, |b| b.y + 1)),
        ]
    } else {
        [None; 3]
    };
    let moment_to = [
        Some(seat.x),
        rows.first().and_then(|r| r.cells.first()).map(|c| c.rect.x),
        back.map(|b| b.x),
    ];

    let mut hits = vec![(seat, Sel::Seat)];
    for row in &rows {
        for cell in &row.cells {
            let id = &m.agents[cell.agent].node.id;
            for (i, line) in cell.rows.iter().enumerate() {
                if let Some(sel) = &line.sel {
                    hits.push((
                        rect(
                            cell.rect.x + 1,
                            cell.rect.y + 1 + i as u16,
                            cell.rect.width - 2,
                            1,
                        ),
                        sel.clone(),
                    ));
                }
            }
            hits.push((cell.rect, Sel::Agent(id.clone())));
        }
    }
    Plan {
        tier: if full { Tier::Full } else { Tier::Compact },
        panel,
        area,
        header_y,
        rule_y,
        legend_y,
        side,
        seat,
        seat_lines,
        jev,
        jev_sites,
        jev_hidden,
        jev_legend,
        cx,
        spawn_y,
        flow,
        rows,
        above,
        below,
        per_row,
        vis_rows,
        total_rows,
        first_row,
        empty,
        back,
        back_lines: prof.back_lines,
        conv_y,
        log,
        log_lines,
        status_y,
        moments,
        moment_to,
        list: Vec::new(),
        hits,
        footer: page_footer(m, v, w, panel),
    }
}

/// Panels have no statusline of their own.
fn page_footer(m: &Model, v: &ViewState, width: u16, panel: bool) -> Footer {
    if panel {
        return Footer {
            left: String::new(),
            hints: Vec::new(),
        };
    }
    content::footer(m, v.selected, width)
}

fn list_plan(area: Rect, m: &Model, v: &ViewState, panel: bool) -> Plan {
    let (w, h) = (area.width, area.height as usize);
    let status_y = if panel {
        u16::MAX
    } else {
        area.y + area.height.saturating_sub(1)
    };
    let budget = if panel { h } else { h.saturating_sub(1) };
    let head = if panel { 0 } else { 3usize.min(budget) };
    let log_lines = if panel {
        0
    } else {
        m.data.events.len().min(3).min(budget.saturating_sub(head))
    };
    let node_rows = budget.saturating_sub(head + log_lines);
    let items: Vec<(Sel, usize)> = m
        .flat()
        .into_iter()
        .filter(|s| *s != Sel::Seat)
        .map(|s| {
            let depth = usize::from(matches!(s, Sel::Child(_)));
            (s, depth)
        })
        .collect();
    let at = items.iter().position(|(s, _)| s == v.selected).unwrap_or(0);
    let start = if at >= node_rows && node_rows > 0 {
        at + 1 - node_rows
    } else {
        0
    };
    let list: Vec<ListRow> = items
        .into_iter()
        .skip(start)
        .take(node_rows)
        .enumerate()
        .map(|(i, (sel, depth))| ListRow {
            y: area.y + (head + i) as u16,
            sel,
            depth,
        })
        .collect();
    let mut hits = vec![(rect(area.x, area.y, w, 1), Sel::Seat)];
    hits.extend(
        list.iter()
            .map(|row| (rect(area.x, row.y, w, 1), row.sel.clone())),
    );
    let none = rect(area.x, area.y, 0, 0);
    Plan {
        tier: Tier::List,
        panel,
        area,
        header_y: area.y,
        rule_y: None,
        legend_y: area.y,
        side: None,
        seat: none,
        seat_lines: 0,
        jev: none,
        jev_sites: 0,
        jev_hidden: false,
        jev_legend: false,
        cx: area.x,
        spawn_y: area.y + 1,
        flow: none,
        rows: Vec::new(),
        above: 0,
        below: 0,
        per_row: 1,
        vis_rows: 0,
        total_rows: 0,
        first_row: 0,
        empty: None,
        back: None,
        back_lines: 0,
        conv_y: None,
        log: None,
        log_lines,
        status_y,
        moments: [None; 3],
        moment_to: [None; 3],
        list,
        hits,
        footer: page_footer(m, v, w, panel),
    }
}

#[cfg(test)]
mod tests {
    use super::super::model::Scope;
    use super::super::testkit::*;
    use super::*;

    fn plan_for(w: u16, h: u16) -> (Plan, usize) {
        let data = fixture();
        let f = facts(None);
        let model = Model::build(&data, &f, Scope::Dashboard);
        let v = ViewState {
            selected: &Sel::Seat,
            scroll: 0,
            error: None,
        };
        let plan = build(Surface::page(rect(0, 0, w, h)), &model, &v);
        (plan, model.agents.len())
    }

    #[test]
    fn the_tier_follows_the_terminal_size() {
        assert_eq!(
            tier_for(120, 36, false),
            Tier::Full,
            "ordinary terminals show A"
        );
        assert_eq!(tier_for(100, 34, false), Tier::Full);
        assert_eq!(tier_for(160, 45, false), Tier::Full);
        assert_eq!(tier_for(99, 40, false), Tier::Compact);
        assert_eq!(tier_for(120, 33, false), Tier::Compact);
        assert_eq!(tier_for(80, 24, false), Tier::Compact);
        assert_eq!(tier_for(79, 24, false), Tier::List);
        assert_eq!(tier_for(70, 20, false), Tier::List);
        assert_eq!(tier_for(200, 23, false), Tier::List);
        // A panel has no header, log or statusline around it, so it needs less.
        assert_eq!(tier_for(100, 30, true), Tier::Full);
        assert_eq!(tier_for(112, 29, true), Tier::Compact);
        assert_eq!(tier_for(60, 18, true), Tier::Compact);
        assert_eq!(tier_for(59, 18, true), Tier::List);
        assert_eq!(tier_for(60, 17, true), Tier::List);
    }

    #[test]
    fn every_tier_keeps_its_cards_inside_the_terminal() {
        for (w, h) in [
            (100, 34),
            (120, 36),
            (160, 45),
            (80, 24),
            (90, 30),
            (99, 33),
        ] {
            let (plan, _) = plan_for(w, h);
            let inside = |r: Rect| r.x + r.width <= w && r.y + r.height <= h;
            assert!(inside(plan.seat), "{w}x{h} seat {:?}", plan.seat);
            assert!(inside(plan.jev), "{w}x{h} jev {:?}", plan.jev);
            for row in &plan.rows {
                for cell in &row.cells {
                    assert!(inside(cell.rect), "{w}x{h} box {:?}", cell.rect);
                }
            }
            assert!(plan.back.is_none_or(inside), "{w}x{h} back");
            assert!(plan.log.is_none_or(inside), "{w}x{h} log");
            assert!(plan.status_y < h);
            let last_flow = plan.flow.y + plan.flow.height;
            assert!(
                plan.log.is_none_or(|l| last_flow <= l.y) || plan.back.is_some(),
                "{w}x{h}: boxes overlap the log"
            );
        }
    }

    #[test]
    fn a_click_maps_to_the_box_or_the_child_line_under_it() {
        let data = fixture();
        let f = facts(None);
        let model = Model::build(&data, &f, Scope::Dashboard);
        let v = ViewState {
            selected: &Sel::Seat,
            scroll: 0,
            error: None,
        };
        let plan = build(Surface::page(rect(0, 0, 120, 36)), &model, &v);
        let worker = plan.rows[0].cells[0].rect;
        assert_eq!(
            plan.hit(worker.x + 2, worker.y),
            Some(&Sel::Agent("w1".into())),
            "the border of the first box is the worker"
        );
        let child_line = plan.rows[0].cells[0]
            .rows
            .iter()
            .position(|r| r.sel.is_some())
            .expect("the worker lists its reviewer");
        assert_eq!(
            plan.hit(worker.x + 2, worker.y + 1 + child_line as u16),
            Some(&Sel::Child("w3".into())),
            "the child line wins over its box"
        );
        let second = plan.rows[0].cells[1].rect;
        assert_eq!(
            plan.hit(second.x + 1, second.y + 1),
            Some(&Sel::Agent("w2".into()))
        );
        assert_eq!(plan.hit(plan.seat.x + 1, plan.seat.y + 1), Some(&Sel::Seat));
        assert_eq!(
            plan.hit(0, plan.area.height - 2),
            None,
            "empty space hits nothing"
        );
    }
}
