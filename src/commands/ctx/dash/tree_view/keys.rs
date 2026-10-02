//! Keys and the mouse. Plain keys are free while the tree shows (every `^A` chord keeps
//! working), so the tree reads them here and hands the event loop an [`Outcome`] for the
//! few that act outside it.
//!
//! The orchestrator dashboard (100 columns and up) is driven through the [`Scene`] it drew: a
//! click, a hover and the keys that answer a request all read the same cells the operator saw.
//! Below that the phase-1 page keeps its own plan, keys and double-click.

use std::time::Instant;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};

use super::super::super::approvals::Decision;
use super::super::pane_rollover::DOUBLE_CLICK;
use super::content::{self, Click};
use super::model::{Model, Sel};
use super::orch;
use super::plan::{self, Surface, ViewState};
use super::scene::{Act, Scene, ShownApproval};
use super::theme::c;
use super::{TreeFacts, TreeView};

/// What the event loop does after the tree has handled an event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(in super::super) enum Outcome {
    None,
    /// Back to the dashboard.
    Leave,
    /// Leave the tree and focus this pane (by session short id).
    OpenPane(String),
    /// Answer the pending approval of this pane through the approvals inbox.
    Answer {
        short: String,
        decision: Decision,
    },
    /// Open the mail compose addressed to this agent.
    Mail {
        to: String,
    },
    /// Open the mail compose to a subagent's host session, the text started for the subagent.
    MailSubagent {
        to: String,
        body: String,
    },
    Notice(String),
    /// Answer the approval the orchestrator dashboard showed in full: only a request that was
    /// drawn with its answer keys is ever answered.
    AnswerShown {
        short: String,
        conn: u64,
        decision: Decision,
    },
    /// Override this open supervisor ruling (the operator's lever; the dashboard calls it
    /// directly, never the CLI). `short` is the session it was made for.
    OverrideRuling {
        id: String,
        short: String,
    },
    /// Open the nudge dialog for this pane.
    Nudge {
        short: String,
    },
    /// Ask this pane's harness to quit; `x` already asked the operator.
    Stop {
        short: String,
    },
    /// Relaunch this ended pane from its original request.
    Retry {
        short: String,
    },
    /// Open the spawn dialog.
    Spawn,
    /// From an open chat back to the flow.
    BackToFlow,
}

fn name_of(model: &Model, sel: &Sel) -> String {
    if *sel == Sel::Jev {
        return "Jev".to_string();
    }
    model
        .node(sel)
        .map_or("the seat".to_string(), content::node_title)
}

/// Which way an arrow key moves through the cards.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Step {
    Prev,
    Next,
    Up,
    Down,
}

/// The selection after an arrow. `order` is the seat, Jev, then the cards: left and right walk it,
/// up and down follow the page (seat, Jev, then a row of the card grid at a time).
fn step(order: &[Sel], per_row: usize, cur: &Sel, dir: Step) -> Sel {
    const CARDS: usize = 2;
    if order.is_empty() {
        return cur.clone();
    }
    let at = order.iter().position(|s| s == cur).unwrap_or(0);
    let wrap = |to: isize| order[to.rem_euclid(order.len() as isize) as usize].clone();
    let stay = || cur.clone();
    match dir {
        Step::Prev => wrap(at as isize - 1),
        Step::Next => wrap(at as isize + 1),
        Step::Down if at < CARDS => order.get(at + 1).cloned().unwrap_or_else(stay),
        Step::Down => order.get(at + per_row).cloned().unwrap_or_else(stay),
        Step::Up if at == 0 => stay(),
        Step::Up if at < CARDS + per_row => order[if at <= CARDS { at - 1 } else { 1 }].clone(),
        Step::Up => order[at - per_row].clone(),
    }
}

impl TreeView {
    fn plan_for<R>(
        &self,
        surf: Surface,
        facts: &TreeFacts,
        f: impl FnOnce(&Model, &plan::Plan) -> R,
    ) -> R {
        let model = Model::build(&self.data, facts, self.scope);
        let selected = model.resolve(&self.selected);
        let vs = ViewState {
            selected: &selected,
            scroll: self.scroll,
            error: self.error.as_deref(),
        };
        let plan = plan::build(surf, &model, &vs);
        f(&model, &plan)
    }

    /// The orchestrator dashboard's scene, when this surface gets it.
    fn orch_for<R>(
        &self,
        surf: Surface,
        facts: &TreeFacts,
        f: impl FnOnce(&Model, &Scene) -> R,
    ) -> Option<R> {
        if surf.panel {
            return None;
        }
        let model = Model::build(&self.data, facts, self.scope);
        let selected = model.resolve(&self.selected);
        let scene = orch::build(surf.area, &model, self, &selected)?;
        Some(f(&model, &scene))
    }

    /// The selection moved by the operator.
    fn picked(&mut self, sel: Sel) {
        self.selected = sel;
        self.act_scroll = 0;
    }

    /// Scroll just far enough that the selected agent's row shows.
    fn reveal(&mut self, surf: Surface, facts: &TreeFacts) {
        if let Some(scroll) = self.orch_for(surf, facts, |model, scene| {
            let m = &scene.metrics;
            let sel = model.resolve(&self.selected);
            let id = match &sel {
                Sel::Seat | Sel::Jev => return m.first_row,
                Sel::Agent(id) => id.clone(),
                Sel::Child(id) => match model.parent_agent(id) {
                    Some(i) => model.agents[i].node.id.clone(),
                    None => return m.first_row,
                },
            };
            let Some(row) = m
                .order
                .iter()
                .position(|o| *o == id)
                .map(|i| i / m.per_row.max(1))
            else {
                return m.first_row;
            };
            if row < m.first_row {
                row
            } else if m.vis_rows > 0 && row >= m.first_row + m.vis_rows {
                row + 1 - m.vis_rows
            } else {
                m.first_row
            }
        }) {
            self.scroll = scroll;
            return;
        }
        let scroll = self.plan_for(surf, facts, |model, plan| {
            let sel = model.resolve(&self.selected);
            let agent = match &sel {
                Sel::Seat | Sel::Jev => return plan.first_row,
                Sel::Agent(id) => model.agent_index(id),
                Sel::Child(id) => model.parent_agent(id),
            };
            let Some(row) = agent.map(|a| plan.row_of(a)) else {
                return plan.first_row;
            };
            if row < plan.first_row {
                row
            } else if plan.vis_rows > 0 && row >= plan.first_row + plan.vis_rows {
                row + 1 - plan.vis_rows
            } else {
                plan.first_row
            }
        });
        self.scroll = scroll;
    }

    fn scroll_by(&mut self, delta: isize, surf: Surface, facts: &TreeFacts) {
        let (base, max) = match self.orch_for(surf, facts, |_, scene| {
            let m = &scene.metrics;
            (m.first_row, m.total_rows.saturating_sub(m.vis_rows.max(1)))
        }) {
            Some(found) => found,
            None => self.plan_for(surf, facts, |_, plan| {
                (
                    plan.first_row,
                    plan.total_rows.saturating_sub(plan.vis_rows.max(1)),
                )
            }),
        };
        self.scroll = base.saturating_add_signed(delta).min(max);
    }

    /// Show a notice as a toast on the flow border, and a request's answer as one.
    fn toasted(&mut self, outcome: Outcome) -> Outcome {
        if let Outcome::Notice(text) = &outcome {
            self.motion.toast(text.clone(), c::WARN, self.now_ms);
        }
        outcome
    }

    /// One plain key while the tree shows.
    pub(in super::super) fn key(
        &mut self,
        key: KeyEvent,
        surf: Surface,
        facts: &TreeFacts,
    ) -> Outcome {
        let outcome = self.key_inner(key, surf, facts);
        self.toasted(outcome)
    }

    fn key_inner(&mut self, key: KeyEvent, surf: Surface, facts: &TreeFacts) -> Outcome {
        if key
            .modifiers
            .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
        {
            return Outcome::None;
        }
        let in_orch = !surf.panel && orch::fits(surf.area.width, surf.area.height);
        if self.help {
            // The dashboard's overlay closes on any key; the phase-1 one on its own keys.
            if in_orch || matches!(key.code, KeyCode::Esc | KeyCode::Char('?' | 'q')) {
                self.help = false;
            }
            return Outcome::None;
        }
        if let Some(target) = self.confirm_stop.take() {
            // `y` stops it; anything else, `n` and Esc included, leaves it running.
            if key.code != KeyCode::Char('y') {
                return Outcome::None;
            }
            let model = Model::build(&self.data, facts, self.scope);
            return match model.selected_pane(&target) {
                Some(short) => Outcome::Stop { short },
                None => Outcome::Notice(format!(
                    "{} is not running in a pane",
                    name_of(&model, &target)
                )),
            };
        }
        if in_orch {
            return self.orch_key(key, surf, facts);
        }
        let (current, next, outcome) = {
            let model = Model::build(&self.data, facts, self.scope);
            let current = model.resolve(&self.selected);
            let mut next = current.clone();
            let mut outcome = Outcome::None;
            match key.code {
                KeyCode::Esc => outcome = Outcome::Leave,
                KeyCode::Left | KeyCode::Char('h') => next = model.sibling(&current, -1),
                KeyCode::Right | KeyCode::Char('l') => next = model.sibling(&current, 1),
                KeyCode::Up | KeyCode::Char('k') => next = model.up(&current),
                KeyCode::Down | KeyCode::Char('j') => next = model.down(&current),
                KeyCode::Tab => next = model.cycle(&current, 1),
                KeyCode::BackTab => next = model.cycle(&current, -1),
                KeyCode::Enter => outcome = open(&model, &current),
                KeyCode::Char(c @ ('y' | 'd')) => {
                    outcome = answer(
                        &model,
                        &current,
                        if c == 'y' {
                            Decision::Allow
                        } else {
                            Decision::Deny
                        },
                    )
                }
                KeyCode::Char('m') => outcome = mail(&model, &current),
                _ => {}
            }
            (current, next, outcome)
        };
        match key.code {
            KeyCode::Char('?') => self.help = true,
            KeyCode::Char('s') => {
                self.scope = self.scope.next();
                self.scroll = 0;
                self.selected = Sel::Seat;
            }
            KeyCode::PageUp => self.scroll_by(-1, surf, facts),
            KeyCode::PageDown => self.scroll_by(1, surf, facts),
            _ => {}
        }
        if next != current {
            self.picked(next);
            self.reveal(surf, facts);
        }
        outcome
    }

    /// A key on the orchestrator dashboard.
    fn orch_key(&mut self, key: KeyEvent, surf: Surface, facts: &TreeFacts) -> Outcome {
        let (order, per_row, shown, rulings) = self
            .orch_for(surf, facts, |_, scene| {
                let mut order = vec![Sel::Seat, Sel::Jev];
                order.extend(
                    scene
                        .metrics
                        .order
                        .iter()
                        .chain(&scene.metrics.folded)
                        .cloned()
                        .map(Sel::Agent),
                );
                (
                    order,
                    scene.metrics.per_row.max(1),
                    scene.shown_approvals.clone(),
                    scene.shown_rulings.clone(),
                )
            })
            .unwrap_or_default();
        let mut opened = None;
        let mut answered = None;
        let mut page = 0isize;
        let (current, next, outcome) = {
            let model = Model::build(&self.data, facts, self.scope);
            let current = model.resolve(&self.selected);
            // A child line selected by Tab belongs to its agent's card for the arrows.
            let anchor = match &current {
                Sel::Child(id) => model
                    .parent_agent(id)
                    .map_or(Sel::Seat, |i| Sel::Agent(model.agents[i].node.id.clone())),
                other => other.clone(),
            };
            let mut next = current.clone();
            let mut outcome = Outcome::None;
            match key.code {
                KeyCode::Char(code @ ('y' | 'a' | 'd')) => {
                    let decision = match code {
                        'y' => Decision::Allow,
                        'a' => Decision::AllowAlways,
                        _ => Decision::Deny,
                    };
                    outcome = answer_shown(&model, &shown, &current, decision);
                    if let Outcome::AnswerShown {
                        short, decision, ..
                    } = &outcome
                    {
                        answered = Some((answer_target(&model, short), *decision));
                    }
                }
                KeyCode::Char('o') => {
                    outcome = match rulings.first() {
                        Some(r) => Outcome::OverrideRuling {
                            id: r.id.clone(),
                            short: r.short.clone(),
                        },
                        None => Outcome::Notice("no supervisor ruling is waiting".to_string()),
                    };
                }
                KeyCode::Char('n') => outcome = nudge(&model, &current),
                KeyCode::Char('x') => match model.selected_pane(&current) {
                    Some(_) => self.confirm_stop = Some(current.clone()),
                    None => {
                        outcome = Outcome::Notice(format!(
                            "{} is not running in a pane here",
                            name_of(&model, &current)
                        ));
                    }
                },
                KeyCode::Char('+') => outcome = Outcome::Spawn,
                KeyCode::Char('r') => outcome = retry(&model, &current),
                KeyCode::Esc => outcome = Outcome::Leave,
                KeyCode::Left | KeyCode::Char('h') => {
                    next = step(&order, per_row, &anchor, Step::Prev);
                }
                KeyCode::Right | KeyCode::Char('l') => {
                    next = step(&order, per_row, &anchor, Step::Next);
                }
                KeyCode::Up | KeyCode::Char('k') => next = step(&order, per_row, &anchor, Step::Up),
                KeyCode::Down | KeyCode::Char('j') => {
                    next = step(&order, per_row, &anchor, Step::Down);
                }
                KeyCode::Tab => next = model.cycle(&current, 1),
                KeyCode::BackTab => next = model.cycle(&current, -1),
                KeyCode::Enter => (outcome, opened) = open_sel(&model, &current),
                KeyCode::Char('m') => outcome = mail_orch(&model, &current),
                KeyCode::Char('?') => self.help = true,
                KeyCode::Char('s') => {
                    self.scope = self.scope.next();
                    self.scroll = 0;
                    self.selected = Sel::Seat;
                }
                KeyCode::Char('A') => {
                    self.filter_selected = !self.filter_selected;
                    self.act_scroll = 0;
                }
                KeyCode::PageUp => page = -1,
                KeyCode::PageDown => page = 1,
                _ => {}
            }
            (current, next, outcome)
        };
        if let Some(opened) = opened {
            self.opened = opened;
        }
        if let Some((Some(id), decision)) = answered {
            self.answered(&id, decision);
        }
        if page != 0 {
            self.scroll_by(page, surf, facts);
        }
        if next != current {
            self.picked(next);
            self.reveal(surf, facts);
        }
        outcome
    }

    /// The request was answered: a pulse back down the bus to its agent and a toast.
    fn answered(&mut self, id: &str, decision: Decision) {
        let (glyph, col) = if decision == Decision::Deny {
            ('\u{2717}', c::ERR)
        } else {
            ('\u{2713}', c::OK)
        };
        self.motion.push(id, false, glyph, col, self.now_ms, 900);
        let msg = match decision {
            Decision::Deny => "Denied. The agent is told no and carries on.",
            Decision::AllowAlways => "Allowed, and the rule is applied.",
            _ => "Allowed once.",
        };
        self.motion.toast(msg, col, self.now_ms);
    }

    /// The dashboard overrode a ruling: it leaves NEEDS YOU at once, ACTIVITY gets a row from
    /// `you`, and a toast says so.
    pub(in super::super) fn override_done(&mut self, id: &str, short: &str) {
        self.data.rulings.retain(|r| r.id != id);
        let row = super::LocalRow {
            ts: super::super::super::state::now_secs(),
            from: "you".into(),
            to_short: short.to_string(),
            text: "overrode the supervisor's ruling".into(),
        };
        self.motion.rows.insert(row.key(), self.now_ms);
        self.local_rows.push(row);
        self.motion.toast(
            "Override recorded. The seat can carry on.",
            c::OK,
            self.now_ms,
        );
    }

    /// One mouse event while the tree shows.
    pub(in super::super) fn mouse(
        &mut self,
        event: MouseEvent,
        surf: Surface,
        facts: &TreeFacts,
        now: Instant,
    ) -> Outcome {
        let outcome = self.mouse_inner(event, surf, facts, now);
        self.toasted(outcome)
    }

    fn mouse_inner(
        &mut self,
        event: MouseEvent,
        surf: Surface,
        facts: &TreeFacts,
        now: Instant,
    ) -> Outcome {
        let (x, y) = (event.column, event.row);
        let in_orch = !surf.panel && orch::fits(surf.area.width, surf.area.height);
        match event.kind {
            MouseEventKind::Moved if in_orch => {
                let (hover, key) = self
                    .orch_for(surf, facts, |_, scene| {
                        scene
                            .hit(i32::from(x), i32::from(y))
                            .map_or((None, None), |r| (r.node.clone(), r.hk.clone()))
                    })
                    .unwrap_or_default();
                (self.hover, self.hover_key) = (hover, key);
                Outcome::None
            }
            MouseEventKind::ScrollUp | MouseEventKind::ScrollDown => {
                let up = event.kind == MouseEventKind::ScrollUp;
                if in_orch {
                    return self.orch_wheel(up, (x, y), surf, facts);
                }
                if self.plan_for(surf, facts, |_, plan| plan.over_agents(y)) {
                    self.scroll_by(if up { -1 } else { 1 }, surf, facts);
                }
                Outcome::None
            }
            MouseEventKind::Down(MouseButton::Left) => {
                // The help overlay covers the layout: a press closes it and acts on nothing under it.
                if self.help {
                    self.help = false;
                    return Outcome::None;
                }
                // Any click answers a pending stop question with "no", unless it is the chip that asks.
                self.confirm_stop = None;
                if in_orch {
                    return self.orch_click((x, y), surf, facts);
                }
                let (hint, hit) = self.plan_for(surf, facts, |_, plan| {
                    (plan.hint_at(x, y), plan.hit(x, y).cloned())
                });
                if let Some(hint) = hint {
                    return match hint.click {
                        Click::Leave => Outcome::Leave,
                        Click::Key(code) => {
                            self.key(KeyEvent::new(code, KeyModifiers::NONE), surf, facts)
                        }
                        Click::None => Outcome::None,
                    };
                }
                let Some(sel) = hit else {
                    return Outcome::None;
                };
                let double = self.last_click.as_ref().is_some_and(|(last, at)| {
                    *last == sel && now.saturating_duration_since(*at) <= DOUBLE_CLICK
                });
                self.picked(sel.clone());
                self.reveal(surf, facts);
                if !double {
                    self.last_click = Some((sel, now));
                    return Outcome::None;
                }
                self.last_click = None;
                let model = Model::build(&self.data, facts, self.scope);
                open(&model, &sel)
            }
            _ => Outcome::None,
        }
    }

    /// The wheel over the dashboard: the activity rows scroll back, the flow's rows scroll.
    fn orch_wheel(
        &mut self,
        up: bool,
        (x, y): (u16, u16),
        surf: Surface,
        facts: &TreeFacts,
    ) -> Outcome {
        let (over_activity, over_flow) = self
            .orch_for(surf, facts, |_, scene| {
                let inside = |r: (i32, i32, i32, i32)| {
                    let (x, y) = (i32::from(x), i32::from(y));
                    x >= r.0 && x < r.0 + r.2 && y >= r.1 && y < r.1 + r.3
                };
                // The box, not just its rows: the border is the wheel's too.
                let a = scene.activity;
                (
                    inside((a.0 - 1, a.1 - 1, a.2 + 2, a.3 + 2)),
                    inside(scene.flow),
                )
            })
            .unwrap_or_default();
        if over_activity {
            let model = Model::build(&self.data, facts, self.scope);
            let max = orch::activity_max_scroll(&model, self);
            self.act_scroll = if up {
                self.act_scroll.saturating_add(1).min(max)
            } else {
                self.act_scroll.saturating_sub(1)
            };
        } else if over_flow {
            self.scroll_by(if up { -1 } else { 1 }, surf, facts);
        }
        Outcome::None
    }

    /// A press on the dashboard: whatever region is under it acts.
    fn orch_click(&mut self, (x, y): (u16, u16), surf: Surface, facts: &TreeFacts) -> Outcome {
        let Some((act, node)) = self
            .orch_for(surf, facts, |_, scene| {
                scene
                    .hit(i32::from(x), i32::from(y))
                    .and_then(|r| r.act.clone().map(|a| (a, r.node.clone())))
            })
            .flatten()
        else {
            return Outcome::None;
        };
        self.perform(act, node, surf, facts)
    }

    fn perform(
        &mut self,
        act: Act,
        node: Option<Sel>,
        surf: Surface,
        facts: &TreeFacts,
    ) -> Outcome {
        match act {
            Act::Classic => Outcome::Leave,
            Act::BackToFlow => Outcome::BackToFlow,
            Act::Help => {
                self.help = true;
                Outcome::None
            }
            Act::Select(sel) => {
                self.picked(sel);
                self.reveal(surf, facts);
                Outcome::None
            }
            Act::Open(sel) => {
                self.picked(sel.clone());
                self.reveal(surf, facts);
                let (outcome, opened) =
                    open_sel(&Model::build(&self.data, facts, self.scope), &sel);
                if let Some(opened) = opened {
                    self.opened = opened;
                }
                outcome
            }
            Act::Press(code) => {
                if let Some(sel) = node {
                    self.picked(sel);
                    self.reveal(surf, facts);
                }
                self.key(KeyEvent::new(code, KeyModifiers::NONE), surf, facts)
            }
        }
    }

    /// A click on the open chat's chrome (the bar and the others strip); `None` when it was
    /// somewhere else, which belongs to the pane.
    pub(in super::super) fn chat_mouse(
        &mut self,
        event: MouseEvent,
        facts: &TreeFacts,
    ) -> Option<Outcome> {
        if !self.in_chat() {
            return None;
        }
        let (x, y) = (i32::from(event.column), i32::from(event.row));
        let area = chat_area(facts);
        let model = Model::build(&self.data, facts, self.scope);
        let scene = orch::build_chat(area, &model, self)?;
        let reg = scene.hit(x, y);
        match event.kind {
            MouseEventKind::Moved => {
                (self.hover, self.hover_key) =
                    reg.map_or((None, None), |r| (r.node.clone(), r.hk.clone()));
                return reg.map(|_| Outcome::None);
            }
            MouseEventKind::Down(MouseButton::Left) => {}
            _ => return None,
        }
        let act = reg?.act.clone()?;
        let (outcome, opened) = match act {
            Act::BackToFlow => (Outcome::BackToFlow, None),
            Act::Open(sel) => open_sel(&model, &sel),
            _ => (Outcome::None, None),
        };
        drop(scene);
        drop(model);
        if let Some(opened) = opened {
            self.opened = opened;
        }
        Some(self.toasted(outcome))
    }

    /// `^A <-` and `^A ->` in a chat: the previous or next agent's chat.
    pub(in super::super) fn chat_step(&self, facts: &TreeFacts, delta: isize) -> Outcome {
        let model = Model::build(&self.data, facts, self.scope);
        match orch::neighbor_pane(&model, delta) {
            Some(short) => Outcome::OpenPane(short),
            None => Outcome::Notice("no other agent has a chat to switch to".to_string()),
        }
    }
}

/// The area the open chat's chrome draws into: the terminal without the approvals strip.
pub(in super::super) fn chat_area(facts: &TreeFacts) -> ratatui::layout::Rect {
    ratatui::layout::Rect::new(
        0,
        0,
        facts.term_size.0,
        facts.term_size.1.saturating_sub(facts.strip_rows),
    )
}

/// The node id an answered request belongs to, for the pulse back to its card.
fn answer_target(model: &Model, short: &str) -> Option<String> {
    match model.sel_for_short(short)? {
        Sel::Seat => Some("seat".to_string()),
        Sel::Agent(id) | Sel::Child(id) => Some(id),
        Sel::Jev => None,
    }
}

fn answer_shown(model: &Model, shown: &[ShownApproval], sel: &Sel, decision: Decision) -> Outcome {
    let pane = model.selected_pane(sel);
    let own = pane
        .as_deref()
        .filter(|short| model.facts.approval_items.iter().any(|a| a.short == *short));
    let target = match own {
        Some(short) => shown.iter().find(|a| a.short == short),
        None => shown.first(),
    };
    match target {
        None if own.is_some() => Outcome::Notice(format!(
            "{}'s request is not shown here; open its harness to answer",
            name_of(model, sel)
        )),
        None => Outcome::Notice("no approval is waiting".to_string()),
        Some(a) if !a.answerable => Outcome::Notice(
            "that command is not shown in full; open its harness to answer".to_string(),
        ),
        Some(a) if decision == Decision::AllowAlways && !a.always => {
            Outcome::Notice("this request does not offer always allow".to_string())
        }
        Some(a) => Outcome::AnswerShown {
            short: a.short.clone(),
            conn: a.conn,
            decision,
        },
    }
}

/// Open a node's harness: its own pane's chat, or for an agent without a pane the chat of the
/// session it runs inside. When even that has no pane here, say which session it is. The second
/// value is the new `opened` node, when this changes it.
fn open_sel(model: &Model, sel: &Sel) -> (Outcome, Option<Option<Sel>>) {
    if *sel == Sel::Jev {
        let notice = "Jev has no harness to open; it answers typed questions in code";
        return (Outcome::Notice(notice.to_string()), None);
    }
    if let Some(short) = model.selected_pane(sel) {
        return (Outcome::OpenPane(short), Some(None));
    }
    let Some(node) = model.node(sel) else {
        let notice = Outcome::Notice(format!("{} has no pane to open", name_of(model, sel)));
        return (notice, None);
    };
    match model.host(node) {
        Some(host) if host.pane => (Outcome::OpenPane(host.short), Some(Some(sel.clone()))),
        Some(host) => (
            Outcome::Notice(format!(
                "{} runs inside {}, which has no pane on this dashboard",
                content::fit(&model.job_of(node), 40),
                host.name
            )),
            None,
        ),
        None => (
            Outcome::Notice(format!("{} has no pane to open", name_of(model, sel))),
            None,
        ),
    }
}

fn nudge(model: &Model, sel: &Sel) -> Outcome {
    match model.selected_pane(sel) {
        Some(short) => Outcome::Nudge { short },
        None => Outcome::Notice(format!("{} has no pane to nudge", name_of(model, sel))),
    }
}

fn retry(model: &Model, sel: &Sel) -> Outcome {
    match model.selected_pane(sel) {
        Some(short) if model.facts.retryable.contains(&short) => Outcome::Retry { short },
        _ => Outcome::Notice(format!("{} has nothing to retry", name_of(model, sel))),
    }
}

fn open(model: &Model, sel: &Sel) -> Outcome {
    match model.selected_pane(sel) {
        Some(short) => Outcome::OpenPane(short),
        None => Outcome::Notice(format!("{} has no pane to open", name_of(model, sel))),
    }
}

fn answer(model: &Model, sel: &Sel, decision: Decision) -> Outcome {
    match model.selected_pane(sel) {
        Some(short) if model.facts.approval_shorts.contains(&short) => {
            Outcome::Answer { short, decision }
        }
        _ => Outcome::Notice(format!("{} has no pending approval", name_of(model, sel))),
    }
}

fn mail(model: &Model, sel: &Sel) -> Outcome {
    let harness = match sel {
        Sel::Seat => model.facts.seat_harness,
        _ => model.node(sel).and_then(|n| n.harness.as_deref()),
    };
    match harness {
        Some(to) => Outcome::Mail { to: to.to_string() },
        None => Outcome::Notice(format!("{} has no mail address", name_of(model, sel))),
    }
}

/// `m` on the dashboard: an agent with a pane is mailed itself; a subagent without one is mailed
/// through its host session, the text started "For your subagent <job>: ".
fn mail_orch(model: &Model, sel: &Sel) -> Outcome {
    match orch::mail_target(model, sel) {
        Some((to, Some(job))) => Outcome::MailSubagent {
            to,
            body: format!("For your subagent {job}: "),
        },
        Some((to, None)) => Outcome::Mail { to },
        None => Outcome::Notice(format!("{} has no mail address", name_of(model, sel))),
    }
}

#[cfg(test)]
mod tests {
    use super::super::model::Scope;
    use super::super::testkit::*;
    use super::*;
    use crossterm::event::KeyEventKind;
    use ratatui::layout::Rect;
    use std::time::Duration;

    // Below 100 columns: the phase-1 page, where the boxes and the footer hints sit at the
    // plan's own coordinates.
    const AREA: Rect = Rect {
        x: 0,
        y: 0,
        width: 90,
        height: 30,
    };

    fn press(view: &mut TreeView, facts: &TreeFacts, code: KeyCode) -> Outcome {
        view.key(
            KeyEvent::new(code, KeyModifiers::NONE),
            Surface::page(AREA),
            facts,
        )
    }

    fn click(view: &mut TreeView, facts: &TreeFacts, x: u16, y: u16, now: Instant) -> Outcome {
        let down = MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: x,
            row: y,
            modifiers: KeyModifiers::NONE,
        };
        view.mouse(down, Surface::page(AREA), facts, now)
    }

    #[test]
    fn arrows_and_tab_move_the_selection_and_wrap_around() {
        let mut view = view(fixture());
        let f = facts(None);
        assert_eq!(view.selected, Sel::Seat);
        press(&mut view, &f, KeyCode::Down);
        assert_eq!(view.selected, Sel::Agent("w1".into()), "seat to the agents");
        press(&mut view, &f, KeyCode::Right);
        assert_eq!(view.selected, Sel::Agent("w2".into()));
        press(&mut view, &f, KeyCode::Right);
        assert_eq!(
            view.selected,
            Sel::Agent("w1".into()),
            "wraps past the last box"
        );
        press(&mut view, &f, KeyCode::Left);
        assert_eq!(
            view.selected,
            Sel::Agent("w2".into()),
            "wraps before the first"
        );
        press(&mut view, &f, KeyCode::Char('h'));
        press(&mut view, &f, KeyCode::Char('j'));
        assert_eq!(
            view.selected,
            Sel::Child("w3".into()),
            "down into the child lines"
        );
        press(&mut view, &f, KeyCode::Char('k'));
        assert_eq!(view.selected, Sel::Agent("w1".into()));
        // Tab walks every node and comes back to the seat.
        let mut seen = vec![view.selected.clone()];
        for _ in 0..4 {
            press(&mut view, &f, KeyCode::Tab);
            seen.push(view.selected.clone());
        }
        assert_eq!(
            seen,
            [
                Sel::Agent("w1".into()),
                Sel::Child("w3".into()),
                Sel::Agent("w2".into()),
                Sel::Seat,
                Sel::Agent("w1".into()),
            ]
        );
        press(&mut view, &f, KeyCode::BackTab);
        assert_eq!(view.selected, Sel::Seat, "Shift-Tab wraps backwards");
    }

    #[test]
    fn enter_opens_the_selected_agents_pane_and_says_when_it_has_none() {
        let mut view = view(fixture());
        let f = facts(None);
        assert_eq!(
            press(&mut view, &f, KeyCode::Enter),
            Outcome::OpenPane("seat1".into()),
            "the seat opens its own pane"
        );
        view.selected = Sel::Agent("w2".into());
        assert_eq!(
            press(&mut view, &f, KeyCode::Enter),
            Outcome::OpenPane("w2".into())
        );
        view.selected = Sel::Child("w3".into());
        assert_eq!(
            press(&mut view, &f, KeyCode::Enter),
            Outcome::Notice("reviewer has no pane to open".into())
        );
    }

    #[test]
    fn y_and_d_answer_only_a_selection_with_a_pending_approval() {
        let mut view = view(fixture());
        let mut f = facts(None);
        view.selected = Sel::Agent("w1".into());
        assert_eq!(
            press(&mut view, &f, KeyCode::Char('y')),
            Outcome::Notice("worker has no pending approval".into())
        );
        f.approval_shorts = vec!["w1".into()];
        f.approvals = 1;
        assert_eq!(
            press(&mut view, &f, KeyCode::Char('y')),
            Outcome::Answer {
                short: "w1".into(),
                decision: Decision::Allow
            }
        );
        assert_eq!(
            press(&mut view, &f, KeyCode::Char('d')),
            Outcome::Answer {
                short: "w1".into(),
                decision: Decision::Deny
            }
        );
    }

    #[test]
    fn m_composes_mail_to_the_agents_harness() {
        let mut view = view(fixture());
        let f = facts(None);
        view.selected = Sel::Agent("w1".into());
        assert_eq!(
            press(&mut view, &f, KeyCode::Char('m')),
            Outcome::Mail { to: "codex".into() }
        );
        view.selected = Sel::Seat;
        assert_eq!(
            press(&mut view, &f, KeyCode::Char('m')),
            Outcome::Mail {
                to: "claude".into()
            }
        );
    }

    #[test]
    fn s_cycles_the_scope_esc_leaves_and_question_mark_toggles_the_key_list() {
        let mut view = view(fixture());
        let f = facts(None);
        assert_eq!(view.scope, Scope::Dashboard);
        press(&mut view, &f, KeyCode::Char('s'));
        assert_eq!(view.scope, Scope::Repo);
        press(&mut view, &f, KeyCode::Char('s'));
        assert_eq!(view.scope, Scope::All);
        press(&mut view, &f, KeyCode::Char('s'));
        assert_eq!(view.scope, Scope::Dashboard);
        press(&mut view, &f, KeyCode::Char('?'));
        assert!(view.help);
        assert_eq!(
            press(&mut view, &f, KeyCode::Esc),
            Outcome::None,
            "Esc closes the key list first"
        );
        assert!(!view.help);
        assert_eq!(press(&mut view, &f, KeyCode::Esc), Outcome::Leave);
    }

    #[test]
    fn a_control_chord_is_never_read_as_a_tree_key() {
        let mut view = view(fixture());
        let f = facts(None);
        let out = view.key(
            KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL),
            Surface::page(AREA),
            &f,
        );
        assert_eq!(out, Outcome::None);
        assert_eq!(view.scope, Scope::Dashboard);
        let _ = KeyEventKind::Press;
    }

    #[test]
    fn a_click_selects_the_box_and_a_second_click_opens_its_pane() {
        let mut view = view(fixture());
        let f = facts(None);
        let (second, child_line) = {
            let model = Model::build(&view.data, &f, view.scope);
            let vs = ViewState {
                selected: &Sel::Seat,
                scroll: 0,
                error: None,
            };
            let plan = plan::build(Surface::page(AREA), &model, &vs);
            (plan.rows[0].cells[1].rect, plan.rows[0].cells[0].rect)
        };
        let t0 = Instant::now();
        assert_eq!(
            click(&mut view, &f, second.x + 1, second.y + 1, t0),
            Outcome::None
        );
        assert_eq!(view.selected, Sel::Agent("w2".into()), "one click selects");
        assert_eq!(
            click(
                &mut view,
                &f,
                second.x + 1,
                second.y + 1,
                t0 + Duration::from_millis(100)
            ),
            Outcome::OpenPane("w2".into()),
            "a quick second click opens the pane"
        );
        click(
            &mut view,
            &f,
            child_line.x + 1,
            child_line.y,
            t0 + Duration::from_secs(5),
        );
        assert_eq!(view.selected, Sel::Agent("w1".into()));
        assert_eq!(
            click(
                &mut view,
                &f,
                child_line.x + 1,
                child_line.y,
                t0 + Duration::from_secs(9)
            ),
            Outcome::None,
            "a slow second click is just another select"
        );
    }

    #[test]
    fn a_click_on_a_footer_hint_does_what_the_key_does() {
        let mut view = view(fixture());
        let f = facts(None);
        let (scope, leave, row) = {
            let model = Model::build(&view.data, &f, view.scope);
            let vs = ViewState {
                selected: &Sel::Seat,
                scroll: 0,
                error: None,
            };
            let plan = plan::build(Surface::page(AREA), &model, &vs);
            let find = |key: &str| {
                plan.footer
                    .hints
                    .iter()
                    .find(|p| p.hint.key == key)
                    .map(|p| p.x)
                    .expect("hint is shown at 120 columns")
            };
            (find("s"), find("^A t"), plan.status_y)
        };
        let now = Instant::now();
        click(&mut view, &f, scope, row, now);
        assert_eq!(view.scope, Scope::Repo, "clicking `s scope` cycles it");
        assert_eq!(click(&mut view, &f, leave, row, now), Outcome::Leave);
    }

    #[test]
    fn the_wheel_scrolls_agent_rows_and_the_selection_scrolls_itself_into_view() {
        let mut data = fixture();
        for i in 0..30 {
            data.nodes.push(node(
                &format!("x{i:02}"),
                Some("seat-1"),
                "filler",
                "m",
                "running",
            ));
        }
        let mut f = facts(None);
        f.pane_shorts = data.nodes.iter().map(|n| n.id.clone()).collect();
        let mut view = view(data);
        // Anywhere over the agent rows: just under the spawn line.
        let over_agents = {
            let model = Model::build(&view.data, &f, view.scope);
            let vs = ViewState {
                selected: &Sel::Seat,
                scroll: 0,
                error: None,
            };
            plan::build(Surface::page(AREA), &model, &vs).spawn_y + 2
        };
        let wheel = |view: &mut TreeView, kind| {
            let ev = MouseEvent {
                kind,
                column: 60,
                row: over_agents,
                modifiers: KeyModifiers::NONE,
            };
            view.mouse(ev, Surface::page(AREA), &f, Instant::now())
        };
        assert_eq!(view.scroll, 0);
        wheel(&mut view, MouseEventKind::ScrollDown);
        assert_eq!(view.scroll, 1);
        wheel(&mut view, MouseEventKind::ScrollUp);
        wheel(&mut view, MouseEventKind::ScrollUp);
        assert_eq!(view.scroll, 0, "never above the first row");
        view.selected = Sel::Agent("x29".into());
        press(&mut view, &f, KeyCode::Tab);
        press(&mut view, &f, KeyCode::BackTab);
        view.selected = Sel::Agent("x28".into());
        press(&mut view, &f, KeyCode::Right);
        assert_eq!(view.selected, Sel::Agent("x29".into()));
        assert!(view.scroll > 0, "the last agent's row was scrolled in");
        let text = draw_classic(90, 30, &view, &f);
        assert!(
            text.contains("above"),
            "the hint says rows are hidden:\n{text}"
        );
    }

    #[test]
    fn the_arrows_walk_the_seat_jev_and_the_cards_and_up_down_follow_the_page() {
        let agent = |id: &str| Sel::Agent(id.into());
        let order = vec![
            Sel::Seat,
            Sel::Jev,
            agent("a"),
            agent("b"),
            agent("c"),
            agent("d"),
            agent("e"),
        ];
        // Two cards a row: a b / c d / e.
        let at = |cur: &Sel, dir| step(&order, 2, cur, dir);
        assert_eq!(at(&Sel::Seat, Step::Next), Sel::Jev);
        assert_eq!(at(&agent("e"), Step::Next), Sel::Seat, "wraps");
        assert_eq!(at(&Sel::Seat, Step::Prev), agent("e"));
        assert_eq!(at(&Sel::Seat, Step::Down), Sel::Jev);
        assert_eq!(at(&Sel::Jev, Step::Down), agent("a"));
        assert_eq!(at(&agent("b"), Step::Down), agent("d"));
        assert_eq!(
            at(&agent("d"), Step::Down),
            agent("d"),
            "no row of two below the last"
        );
        assert_eq!(at(&agent("e"), Step::Up), agent("c"));
        assert_eq!(at(&agent("a"), Step::Up), Sel::Jev);
        assert_eq!(at(&agent("b"), Step::Up), Sel::Jev);
        assert_eq!(at(&Sel::Jev, Step::Up), Sel::Seat);
        assert_eq!(at(&Sel::Seat, Step::Up), Sel::Seat);
        assert_eq!(
            step(&[], 3, &Sel::Seat, Step::Next),
            Sel::Seat,
            "nothing drawn, nowhere to go"
        );
    }
}
