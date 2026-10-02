//! The switchable agent-tree view (`^A t`, issue #833): a full-screen drawing
//! of the agent graph (`graph::snapshot`), the supervisor, the active workflow
//! and the merged session log (`graph::merged_events`) in place of the
//! dashboard chrome, laid out like the operator's mockup A.
//!
//! Nothing here is computed while the dashboard view is showing: the data is
//! gathered on a one-shot background thread started only while the tree is
//! visible, at the existing `FACTS_THROTTLE` cadence, so the render loop never
//! waits on a file read and the dashboard view pays nothing. Every frame is a
//! pure function of that data, the dashboard facts and the view state.
//!
//! - `model`: which nodes are in scope, the seat/agent/child structure and the
//!   selection moves.
//! - `content`: the text of every card, with its colours.
//! - `plan`: the tier and the geometry, shared by drawing and hit-testing.
//! - `orch`: the orchestrator dashboard (header, stepper, flow, requests, preview, activity, key
//!   bar) at 100 columns and up, built as a [`scene::Scene`]: cells plus the regions the mouse hits.
//! - `flow`: the FLOW panel of it: seat, bus, cards, FINISHED strip and the light along the bus.
//! - `theme`, `scene`, `fx`: the palette and cell grid, the frame and its regions, and the motion
//!   (pulses, flashes, fades) that starts when a gather delivers something new.
//! - `draw`: paints a plan, or the orchestrator's scene.
//! - `keys`: keys, the mouse and the actions they hand back to the event loop.

mod content;
mod draw;
mod flow;
mod fx;
mod keys;
mod model;
mod orch;
mod plan;
mod scene;
mod theme;

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use chrono::FixedOffset;

use super::super::config::CtxConfig;
use super::super::graph::{self, Event, Node};
use super::super::mail;
use super::super::price;
use super::super::sessions;
use super::super::state::{StateDir, now_secs, repo_slug_read_only};
use super::super::supervisor;
use super::sidebar_facts::FACTS_THROTTLE;
use super::ui::JevSectionFact;
use crate::commands::workflow::ActiveWorkflowSummary;

pub(super) use content::approval_view;
pub(super) use draw::render;
pub(super) use keys::Outcome;
pub(super) use model::{Scope, Sel};
pub(super) use plan::Surface;

/// Newest events kept from the merged log.
const EVENTS_KEPT: usize = 50;
/// A Jev call just before the seat registered still belongs to its first request.
const JEV_SINCE_GRACE_SECS: u64 = 120;
/// With no seat session the Jev box shows this much of the repository's recent past.
const JEV_REPO_WINDOW_SECS: u64 = 12 * 3600;
const ADVICE_CHARS: usize = 60;

/// One of the supervisor's three moments.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Moment {
    BeforePlan,
    ErrorRepeats,
    BeforeDone,
}

/// The on-call supervisor as the sidecar shows it; absent when the supervisor is off.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct SupervisorFact {
    pub(super) harness: String,
    pub(super) model: String,
    pub(super) calls: u32,
    pub(super) max_calls: u32,
    pub(super) tokens_read: u64,
    /// First line of the last advice, capped.
    pub(super) advice: String,
    pub(super) advising: bool,
    /// The moment that fired most recently.
    pub(super) last: Option<Moment>,
}

/// Recent mail touching one node, by session short id.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(super) struct MailCount {
    pub(super) recent: usize,
    pub(super) unread: usize,
}

/// How often the orchestrator dashboard redraws while it shows: about 15 frames a second.
pub(super) const FRAME_INTERVAL: Duration = Duration::from_millis(66);

/// What one background gather produces.
#[derive(Debug, Default, Clone)]
pub(super) struct TreeData {
    /// False until the first gather has landed.
    pub(super) loaded: bool,
    pub(super) nodes: Vec<Node>,
    pub(super) events: Vec<Event>,
    /// The seat model's `(input, output)` micro-USD per 1M tokens, when priced.
    pub(super) seat_price: Option<(u64, u64)>,
    /// Newest Jev decision per site: `(margin, sharp)`.
    pub(super) jev_verdicts: BTreeMap<String, (f64, bool)>,
    /// Lowercased model ids the operator or the scorecard avoids; empty unless opted in.
    pub(super) avoided: BTreeSet<String>,
    pub(super) supervisor: Option<SupervisorFact>,
    /// Session ids registered for this repository.
    pub(super) repo_ids: BTreeSet<String>,
    pub(super) mail_counts: BTreeMap<String, MailCount>,
    /// Jev's newest decisions and the sites that are on, read by the gather.
    pub(super) jev: content::JevFeed,
    /// The supervisor's open rulings, read by the gather (oldest first).
    pub(super) rulings: Vec<supervisor::Ruling>,
}

pub(super) fn capped_first_line(text: &str, cap: usize) -> String {
    let line: String = text
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or_default()
        .chars()
        .filter(|c| !c.is_control())
        .collect();
    if line.chars().count() <= cap {
        return line;
    }
    let mut cut: String = line.chars().take(cap.saturating_sub(1)).collect();
    cut.push('\u{2026}');
    cut
}

fn supervisor_fact(
    state: &StateDir,
    cfg: &CtxConfig,
    seat_short: Option<&str>,
) -> Option<SupervisorFact> {
    if !cfg.supervisor.enabled {
        return None;
    }
    let snap = seat_short
        .map(|short| supervisor::snapshot(state, short))
        .unwrap_or_default();
    Some(SupervisorFact {
        harness: cfg.supervisor.harness.clone(),
        model: cfg.supervisor.model.clone(),
        calls: snap.calls,
        max_calls: cfg.supervisor.max_calls,
        tokens_read: snap.tokens_read,
        advice: capped_first_line(&snap.last_advice, ADVICE_CHARS),
        advising: snap.advising,
        last: snap.last_trigger.map(|trigger| match trigger {
            supervisor::Trigger::BeforePlan => Moment::BeforePlan,
            supervisor::Trigger::ErrorRepeats => Moment::ErrorRepeats,
            supervisor::Trigger::BeforeDone => Moment::BeforeDone,
        }),
    })
}

fn mail_counts(edges: &[mail::MailEdge]) -> BTreeMap<String, MailCount> {
    let mut counts: BTreeMap<String, MailCount> = BTreeMap::new();
    for edge in edges {
        counts
            .entry(sessions::short_id(&edge.from_session))
            .or_default()
            .recent += 1;
        if let Some(to) = edge.to_session.as_deref() {
            let entry = counts.entry(sessions::short_id(to)).or_default();
            entry.recent += 1;
            entry.unread += usize::from(edge.unread);
        }
    }
    counts
}

/// The parent id with `group` nodes folded away.
fn ungrouped_parent(nodes: &[Node], node: &Node) -> Option<String> {
    let mut parent = node.parent.clone()?;
    for _ in 0..16 {
        match nodes.iter().find(|n| n.id == parent) {
            Some(p) if p.kind == "group" => parent = p.parent.clone()?,
            _ => break,
        }
    }
    Some(parent)
}

/// One `dispatch` event, parent to agent with the job as its text, for each agent no hook saw start:
/// those found from a meta file, a pane or a delegation record have no `subagent_start`.
fn dispatch_events(nodes: &[Node], events: &[Event]) -> Vec<Event> {
    let hooked = |node: &Node| {
        events
            .iter()
            .any(|e| e.kind == "subagent_start" && e.to.as_deref() == Some(node.id.as_str()))
    };
    nodes
        .iter()
        .filter(|n| n.kind != "group" && !hooked(n))
        .filter_map(|n| {
            Some(Event {
                ts: n.started_at?,
                actor: ungrouped_parent(nodes, n)?,
                kind: "dispatch".to_string(),
                summary: n.job.clone().unwrap_or_else(|| "dispatched".to_string()),
                p: None,
                to: Some(
                    n.role
                        .clone()
                        .or_else(|| n.label.clone())
                        .unwrap_or_else(|| n.kind.clone()),
                ),
            })
        })
        .collect()
}

/// The events that are one agent reaching another: mail, a Jev decision, the supervisor's advice
/// and a dispatch. Hook and safety decisions are not, and kept they would fill the whole cap.
fn connection_events(mut all: Vec<Event>) -> Vec<Event> {
    const CONNECTIONS: [&str; 7] = [
        "dispatch",
        "mail",
        "jev",
        "proxy",
        "supervisor",
        "delegation",
        "subagent_start",
    ];
    all.retain(|e| CONNECTIONS.contains(&e.kind.as_str()));
    all.sort_by_key(|e| e.ts);
    all[all.len().saturating_sub(EVENTS_KEPT)..].to_vec()
}

/// Gather the tree's data. Runs on the background thread only.
pub(super) fn compute(
    state: &StateDir,
    repo: &Path,
    codex_root: Option<&Path>,
    seat_model: Option<&str>,
    seat_session: Option<&str>,
    cfg: &CtxConfig,
) -> TreeData {
    let seat_short = seat_session.map(sessions::short_id);
    let seat_short = seat_short.as_deref();
    let nodes = graph::snapshot(state, repo, codex_root, now_secs());
    let edges = mail::recent_edges(state, now_secs(), EVENTS_KEPT);
    let mut all = graph::merged_events_with_mail(state, &edges);
    all.extend(dispatch_events(&nodes, &all));
    let events = connection_events(all);
    let seat_price = seat_model.and_then(|model| {
        let table = price::resolve_table(cfg);
        let p = table.models.get(model)?;
        Some((p.input_micros, p.output_micros))
    });
    let slug = repo_slug_read_only(repo);
    // The Jev box shows this seat's work: its descendants, and nothing from before it started (a proxy
    // row names no session). Without a seat the box falls back to the repository's last half day.
    let mut jev_children: Vec<String> = Vec::new();
    while let Some(seat) = seat_session {
        let before = jev_children.len();
        let kids = nodes.iter().filter(|n| {
            n.parent
                .as_deref()
                .is_some_and(|p| p == seat || jev_children.iter().any(|c| c == p))
        });
        let fresh: Vec<String> = kids
            .map(|n| n.id.clone())
            .filter(|id| !jev_children.contains(id))
            .collect();
        jev_children.extend(fresh);
        if jev_children.len() == before {
            break;
        }
    }
    let jev_since = seat_session
        .and_then(|seat| {
            let node = nodes.iter().find(|n| n.id == seat)?.started_at;
            node.or_else(|| {
                graph::read_session_records(state)
                    .into_iter()
                    .find(|(record, _)| record.session == seat)
                    .map(|(record, _)| record.started_at)
            })
        })
        .map_or(now_secs().saturating_sub(JEV_REPO_WINDOW_SECS), |start| {
            start.saturating_sub(JEV_SINCE_GRACE_SECS)
        });
    TreeData {
        loaded: true,
        nodes,
        events,
        seat_price,
        jev_verdicts: graph::jev_site_verdicts(state),
        avoided: super::super::models::avoid_for_state(cfg, state),
        supervisor: supervisor_fact(state, cfg, seat_short),
        repo_ids: graph::read_session_records(state)
            .into_iter()
            .filter(|(record, _)| record.repo_slug == slug)
            .map(|(record, _)| record.session)
            .collect(),
        mail_counts: mail_counts(&edges),
        jev: content::jev_feed_of(
            super::super::jev_feed::jev_feed(
                state,
                cfg,
                seat_session.map_or(super::super::jev_feed::JevScope::Repo(repo), |session| {
                    super::super::jev_feed::JevScope::Session {
                        session,
                        children: &jev_children,
                        repo,
                    }
                }),
                jev_since,
            ),
            cfg.proxy.enabled && cfg.proxy.decider == super::super::config::ProxyDecider::Typesafe,
        ),
        rulings: supervisor::open_rulings(state, None),
    }
}

/// Visibility, the latest gathered data, the in-flight gather and what the operator has selected.
#[derive(Default)]
pub(super) struct TreeView {
    visible: bool,
    last_refresh: Option<Instant>,
    inflight: Option<mpsc::Receiver<TreeData>>,
    data: TreeData,
    error: Option<String>,
    scope: Scope,
    selected: Sel,
    /// First visible row of agent boxes.
    scroll: usize,
    help: bool,
    /// The phase-1 page's double-click; the orchestrator dashboard opens on one click.
    last_click: Option<(Sel, Instant)>,
    /// An agent's live chat is open in place of the flow.
    chat: bool,
    /// The activity box shows only the selected node's events.
    filter_selected: bool,
    /// Events scrolled back from the newest.
    act_scroll: usize,
    /// The node `x` asked to stop, until `y` or anything else answers.
    confirm_stop: Option<Sel>,
    /// The node under the pointer, previewed in SELECTED's place.
    hover: Option<Sel>,
    /// The chip under the pointer.
    hover_key: Option<String>,
    /// The agent whose harness the operator opened, when it has no pane of its own and the chat
    /// showing is its host's: the bar names it.
    opened: Option<Sel>,
    /// Pulses, flashes, fades and the toast.
    motion: fx::Motion,
    /// Bumped each time a gather lands, so `observe` compares only new data.
    rev: u64,
    /// The animation clock in milliseconds since the view was first drawn: injected, so a frame
    /// is a pure function of it.
    now_ms: u64,
    shown_at: Option<Instant>,
    /// Activity rows the operator made on this dashboard (an override), until the log has them.
    local_rows: Vec<LocalRow>,
}

/// An activity row the dashboard itself recorded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct LocalRow {
    pub(super) ts: u64,
    pub(super) from: String,
    pub(super) to_short: String,
    pub(super) text: String,
}

impl LocalRow {
    /// The key its fade-in is tracked under.
    pub(super) fn key(&self) -> String {
        format!("local|{}|{}|{}", self.ts, self.from, self.text)
    }
}

impl TreeView {
    pub(super) fn is_visible(&self) -> bool {
        self.visible
    }

    /// Flip the view. Showing refreshes at once; hiding drops everything but the scope so a
    /// hidden tree holds no data and no pending work.
    pub(super) fn toggle(&mut self) {
        self.visible = !self.visible;
        *self = Self {
            visible: self.visible,
            scope: self.scope,
            ..Self::default()
        };
    }

    /// The chat of the focused pane is open under the one-line bar.
    pub(super) fn in_chat(&self) -> bool {
        self.visible && self.chat
    }

    /// Rows the chat chrome takes from the top of a terminal this size, under an approvals strip
    /// of `strip` rows: the chrome is sized from the surface it draws into, as `build_chat` is.
    pub(super) fn chat_rows(&self, term: (u16, u16), strip: u16) -> u16 {
        if self.in_chat() {
            orch::chat_chrome((term.0, term.1.saturating_sub(strip))).0
        } else {
            0
        }
    }

    /// Rows the chat chrome takes from the bottom (the others strip and the key bar).
    pub(super) fn chat_bottom_rows(&self, term: (u16, u16), strip: u16) -> u16 {
        if self.in_chat() {
            orch::chat_chrome((term.0, term.1.saturating_sub(strip))).1
        } else {
            0
        }
    }

    /// The orchestrator dashboard answers approvals itself, so the dashboard's strip stays out of it.
    pub(super) fn hides_approvals_strip(&self, term: (u16, u16)) -> bool {
        self.captures_plain_keys() && orch::fits(term.0, term.1)
    }

    /// Plain keys belong to the tree only while the flow shows; in a chat they go to the pane,
    /// Esc included, because the harnesses use it to interrupt.
    pub(super) fn captures_plain_keys(&self) -> bool {
        self.visible && !self.chat
    }

    pub(super) fn open_chat(&mut self) {
        if self.visible {
            self.chat = true;
            self.help = false;
        }
    }

    /// From an open chat back to the flow.
    pub(super) fn close_chat(&mut self) {
        self.chat = false;
    }

    /// `^A t`: from a chat back to the flow, from the flow back to the dashboard.
    pub(super) fn chord_toggle(&mut self) {
        if self.in_chat() {
            self.chat = false;
        } else {
            self.toggle();
        }
    }

    /// The redraw cadence while the flow shows; `None` while hidden or in a chat, where the loop's
    /// own tick is unchanged.
    pub(super) fn frame_interval(&self) -> Option<Duration> {
        self.captures_plain_keys().then_some(FRAME_INTERVAL)
    }

    /// The orchestrator FLOW is on screen at this terminal size, so the pointer's plain motion is
    /// worth asking the terminal for (the dashboard turns `?1003` on only then).
    pub(super) fn wants_hover(&self, term: (u16, u16)) -> bool {
        self.hides_approvals_strip(term)
    }

    /// What the pointer is over; a motion event redraws only when this changes.
    pub(super) fn hover_signature(&self) -> (Option<Sel>, Option<String>) {
        (self.hover.clone(), self.hover_key.clone())
    }

    /// Advance the animation clock to `now` and start the motion the data earned since the last
    /// look. Called once per frame, before drawing, while the view shows.
    pub(super) fn observe(&mut self, facts: &TreeFacts, now: Instant) {
        let shown = *self.shown_at.get_or_insert(now);
        let ms = now.saturating_duration_since(shown).as_millis() as u64;
        self.observe_at(facts, ms);
    }

    fn observe_at(&mut self, facts: &TreeFacts, ms: u64) {
        self.now_ms = ms;
        self.motion.observe(&self.data, self.rev, facts, ms);
    }

    /// Whether a gather should start now. Always false while hidden.
    pub(super) fn due(&self, now: Instant) -> bool {
        self.visible
            && self.inflight.is_none()
            && self
                .last_refresh
                .is_none_or(|last| now.duration_since(last) >= FACTS_THROTTLE)
    }

    /// Start one background gather; the result lands through [`Self::poll`].
    pub(super) fn start<F>(&mut self, now: Instant, gather: F)
    where
        F: FnOnce() -> TreeData + Send + 'static,
    {
        let (tx, rx) = mpsc::channel();
        self.last_refresh = Some(now);
        if std::thread::Builder::new()
            .name("zirv-tree".into())
            .spawn(move || {
                let _ = tx.send(gather());
            })
            .is_ok()
        {
            self.inflight = Some(rx);
        } else {
            self.error = Some("gather could not start".into());
        }
    }

    /// Take a finished gather, if any. Never blocks.
    pub(super) fn poll(&mut self) {
        let Some(rx) = self.inflight.as_ref() else {
            return;
        };
        match rx.try_recv() {
            Ok(data) => {
                self.data = data;
                self.rev += 1;
                self.error = None;
                self.inflight = None;
            }
            Err(mpsc::TryRecvError::Empty) => {}
            Err(mpsc::TryRecvError::Disconnected) => {
                self.error = Some("gather failed, retrying".into());
                self.inflight = None;
            }
        }
    }
}

/// One pending approval as the NEEDS YOU list shows it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(super) struct ApprovalFact {
    pub(super) short: String,
    /// The hook connection of this very request, so an answer reaches it and not a later one.
    pub(super) conn: u64,
    pub(super) tool: String,
    pub(super) preview: String,
    pub(super) waited_secs: u64,
    /// The preview is the whole input: nothing was redacted or capped.
    pub(super) fully_shown: bool,
    /// The hold ended: only the pane's own dialog can answer.
    pub(super) released: bool,
    /// What the approval card shows of it: the whole command, where, why, and the answers on offer.
    pub(super) view: content::ApprovalView,
}

/// Why a session waits for the operator, from the kinds the attention latch already holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum WaitKind {
    Question,
    Permission,
    Approval,
    WorkflowGate,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct WaitFact {
    pub(super) short: String,
    pub(super) kind: WaitKind,
    /// When the wait began (unix seconds).
    pub(super) since: u64,
    pub(super) evidence: String,
}

/// What the dashboard knows of one pane that the graph does not.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct PaneMeta {
    pub(super) short: String,
    /// Position in the dashboard's pane list, from 1.
    pub(super) number: usize,
    /// The checkout directory's name.
    pub(super) worktree: String,
    /// First line of the dispatch brief, when the dashboard kept the spawn request.
    pub(super) brief: String,
}

/// The dashboard facts the tree view reuses, already cached by the event loop.
pub(super) struct TreeFacts<'a> {
    pub(super) repo_name: String,
    /// The seat harness's 5h usage reading, from the sidebar's cached usage.
    pub(super) usage_5h: Option<f64>,
    pub(super) approval_items: Vec<ApprovalFact>,
    pub(super) waits: Vec<WaitFact>,
    /// Sessions with an armed stall latch, with when they last changed state (0 when unknown).
    pub(super) stalled: Vec<(String, u64)>,
    /// Panes whose Retry menu action is available right now.
    pub(super) retryable: Vec<String>,
    pub(super) pane_meta: Vec<PaneMeta>,
    /// The whole terminal, strip and chat chrome included; the tier follows it.
    pub(super) term_size: (u16, u16),
    /// Rows the approvals strip takes under an open chat (0 while none shows).
    pub(super) strip_rows: u16,
    pub(super) seat_harness: Option<&'a str>,
    pub(super) seat_model: Option<&'a str>,
    pub(super) seat_role: Option<&'a str>,
    pub(super) seat_session: Option<&'a str>,
    pub(super) rot: Option<u32>,
    pub(super) jev: Option<&'a JevSectionFact>,
    /// The repository's active workflow, from the dashboard's cache.
    pub(super) workflow: Option<&'a ActiveWorkflowSummary>,
    /// Pending approvals on this dashboard; the footer count draws only while above zero (#840).
    pub(super) approvals: usize,
    /// Short ids of the sessions whose request the approvals inbox holds.
    pub(super) approval_shorts: Vec<String>,
    /// The approvals inbox is bound, so a permission request can be answered here instead of in its pane.
    pub(super) approvals_inbox: bool,
    /// Short ids of this dashboard's panes.
    pub(super) pane_shorts: Vec<String>,
    /// The focused pane: `(short id, title, agent)`; the chat bar names it.
    pub(super) focused: Option<(String, String, String)>,
    pub(super) panes_used: usize,
    pub(super) max_panes: usize,
    pub(super) max_writers: usize,
    pub(super) now: u64,
    pub(super) utc_offset: FixedOffset,
}

#[cfg(test)]
mod testkit {
    use super::*;
    use crate::commands::ctx::dash::ui::JevSiteBar;
    use crate::commands::workflow::StepMark;

    pub(super) fn node(
        id: &str,
        parent: Option<&str>,
        role: &str,
        model: &str,
        status: &str,
    ) -> Node {
        Node {
            id: id.into(),
            parent: parent.map(str::to_string),
            kind: "delegation".into(),
            harness: Some("codex".into()),
            model: Some(model.into()),
            effort: None,
            role: Some(role.into()),
            status: status.into(),
            started_at: Some(1_000),
            ended_at: None,
            tokens: Some(12_000),
            label: None,
            job: None,
            workflow: None,
            session: None,
            steps: Vec::new(),
        }
    }

    pub(super) fn event(ts: u64, actor: &str, kind: &str, summary: &str, p: Option<f64>) -> Event {
        Event {
            ts,
            actor: actor.into(),
            kind: kind.into(),
            summary: summary.into(),
            p,
            to: None,
        }
    }

    pub(super) fn fixture() -> TreeData {
        let mut seat = node("seat-1", None, "orchestrator", "fable", "running");
        seat.harness = Some("claude".into());
        seat.effort = Some("high".into());
        let mut explorer = node("w2", Some("seat-1"), "explorer", "sonnet", "done");
        explorer.harness = Some("claude".into());
        TreeData {
            loaded: true,
            nodes: vec![
                seat,
                node("w1", Some("seat-1"), "worker", "sol", "running"),
                explorer,
                node("w3", Some("w1"), "reviewer", "terra", "queued"),
            ],
            events: vec![
                event(
                    1_700,
                    "jev",
                    "decision",
                    "dispatch tier -> claude sonnet",
                    Some(0.87),
                ),
                event(
                    1_710,
                    "explorer",
                    "delegation",
                    "mapped 4 call sites\u{1b}[31m",
                    None,
                ),
            ],
            seat_price: Some((15_000_000, 75_000_000)),
            jev_verdicts: [("dispatch tier".to_string(), (0.87, true))].into(),
            ..TreeData::default()
        }
    }

    pub(super) fn node_k(
        id: &str,
        parent: &str,
        kind: &str,
        harness: &str,
        model: &str,
        role: &str,
        status: &str,
    ) -> Node {
        let mut n = node(id, Some(parent), role, model, status);
        n.kind = kind.into();
        n.harness = Some(harness.into());
        n
    }

    pub(super) fn busy() -> (TreeData, ActiveWorkflowSummary, JevSectionFact) {
        let mut seat = node("seat-1", None, "orchestrator", "fable", "live");
        seat.kind = "session".into();
        seat.harness = Some("claude".into());
        seat.effort = Some("high".into());
        seat.tokens = Some(412_000);
        let mut explore = node_k(
            "a1", "seat-1", "subagent", "claude", "haiku", "Explore", "running",
        );
        explore.job = Some("reads the code".into());
        explore.started_at = Some(1_180);
        let mut general = node_k(
            "a2",
            "seat-1",
            "subagent",
            "claude",
            "sonnet",
            "general-purpose",
            "completed",
        );
        general.tokens = Some(23_400);
        general.ended_at = Some(1_123);
        let mut plan = node_k(
            "a3", "seat-1", "subagent", "claude", "opus", "Plan", "failed",
        );
        plan.tokens = Some(9_000);
        plan.ended_at = Some(1_040);
        let mut worker = node_k(
            "w1",
            "seat-1",
            "delegation",
            "codex",
            "gpt-6-sol",
            "worker",
            "running",
        );
        worker.effort = Some("medium".into());
        worker.workflow = Some(crate::commands::ctx::graph::WorkflowStamp {
            id: "wf1".into(),
            pack: "feature".into(),
            step: "implement".into(),
        });
        worker.tokens = Some(88_000);
        worker.started_at = Some(1_120);
        let mut child1 = node_k(
            "c1",
            "w1",
            "codex_child",
            "codex",
            "gpt-6-terra",
            "explorer",
            "running",
        );
        child1.effort = Some("low".into());
        child1.started_at = Some(1_200);
        let mut child2 = node_k(
            "c2",
            "w1",
            "codex_child",
            "codex",
            "gpt-6-terra",
            "reviewer",
            "completed",
        );
        child2.tokens = Some(5_000);
        let mut child3 = node_k(
            "c3",
            "w1",
            "codex_child",
            "codex",
            "gpt-6-terra",
            "linter",
            "completed",
        );
        child3.tokens = Some(1_000);
        let nodes = vec![seat, explore, general, plan, worker, child1, child2, child3];
        let mut mail_to = event(
            1_230,
            "w1",
            "mail",
            "run the migration tests before review",
            None,
        );
        mail_to.to = Some("seat1".into());
        let data = TreeData {
            loaded: true,
            nodes,
            events: vec![
                event(
                    1_140,
                    "proxy",
                    "proxy",
                    "intent=investigate complexity=bounded execution=native",
                    Some(0.71),
                ),
                event(
                    1_180,
                    "seat-1",
                    "jev",
                    "dispatch tier -> claude sonnet",
                    Some(0.87),
                ),
                event(1_190, "seat-1", "subagent_start", "Explore a1", None),
                event(
                    1_200,
                    "seat-1",
                    "jev",
                    "retry or stop -> seat decides",
                    Some(0.12),
                ),
                mail_to,
                event(1_235, "seat-1", "decision", "allow cargo", None),
            ],
            seat_price: Some((15_000_000, 75_000_000)),
            jev_verdicts: [
                ("dispatch tier".to_string(), (0.87, true)),
                ("review triage".to_string(), (0.97, true)),
                ("retry or stop".to_string(), (0.58, false)),
            ]
            .into(),
            supervisor: Some(SupervisorFact {
                harness: "codex".into(),
                model: "gpt-6-astra".into(),
                calls: 1,
                max_calls: 3,
                tokens_read: 224_000,
                advice: "fixture path wrong, check tests/fixtures before the next run".into(),
                advising: false,
                last: Some(Moment::ErrorRepeats),
            }),
            mail_counts: [(
                "w1".to_string(),
                MailCount {
                    recent: 2,
                    unread: 1,
                },
            )]
            .into(),
            jev: content::JevFeed {
                rows: vec![
                    content::JevRow {
                        id: "jev|1180|dispatch tier|claude sonnet".into(),
                        ts: 1_180,
                        site: "dispatch tier".into(),
                        text: "claude sonnet".into(),
                        confidence: 0.87,
                        sure: true,
                        cached: false,
                    },
                    content::JevRow {
                        id: "jev|1200|retry or stop|seat decides".into(),
                        ts: 1_200,
                        site: "retry or stop".into(),
                        text: "seat decides".into(),
                        confidence: 0.12,
                        sure: false,
                        cached: true,
                    },
                ],
                sites: ["memory", "dispatch", "review", "gates"]
                    .map(String::from)
                    .to_vec(),
                proxy: false,
            },
            ..TreeData::default()
        };
        let wf = ActiveWorkflowSummary {
            kind: "feature",
            step: "implement".into(),
            awaiting_approval: false,
            pack: "feature".into(),
            steps: ["intent", "plan", "implement", "review", "verify"]
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
                .collect(),
            title: "Agent tree polish".into(),
            started_at: 560,
            next_gate: Some("review".into()),
        };
        let jev = JevSectionFact::Active {
            calls: 214,
            cache_hit_rate: None,
            wait_p95_ms: None,
            errors: 0,
            latest_error_reason: None,
            last: None,
            sites: ["dispatch tier", "review triage", "retry or stop"]
                .iter()
                .map(|n| JevSiteBar {
                    name: (*n).into(),
                    calls: 70,
                    filled: 4,
                    eased_filled: 4.0,
                })
                .collect(),
            errors_detail: Vec::new(),
        };
        (data, wf, jev)
    }

    pub(super) fn huge() -> TreeData {
        let mut seat = node("seat-1", None, "orchestrator", "fable", "live");
        seat.kind = "session".into();
        seat.harness = Some("claude".into());
        let mut nodes = vec![seat];
        for i in 0..24usize {
            let status = ["running", "completed", "failed", "queued"][i % 4];
            let mut n = node(
                &format!("n{i:02}"),
                Some("seat-1"),
                &format!("agent-with-a-rather-long-role-name-{i}"),
                "sonnet",
                status,
            );
            n.harness = Some(if i % 2 == 0 { "codex" } else { "claude" }.into());
            n.tokens = Some(i as u64 * 1_000);
            nodes.push(n);
            if i % 3 == 0 {
                for k in 0..(i % 5) {
                    nodes.push(node(
                        &format!("n{i:02}k{k}"),
                        Some(&format!("n{i:02}")),
                        &format!("sub-{k}"),
                        "haiku",
                        "running",
                    ));
                }
            }
        }
        TreeData {
            loaded: true,
            nodes,
            events: (0..8)
                .map(|i| {
                    event(
                        1_100 + i * 10,
                        "seat-1",
                        "decision",
                        &format!("allow command {i}"),
                        None,
                    )
                })
                .collect(),
            ..TreeData::default()
        }
    }

    pub(super) fn jev_fact() -> JevSectionFact {
        JevSectionFact::Active {
            calls: 214,
            cache_hit_rate: None,
            wait_p95_ms: None,
            errors: 0,
            latest_error_reason: None,
            last: None,
            sites: vec![JevSiteBar {
                name: "dispatch tier".into(),
                calls: 150,
                filled: 6,
                eased_filled: 6.0,
            }],
            errors_detail: Vec::new(),
        }
    }

    pub(super) fn facts<'a>(jev: Option<&'a JevSectionFact>) -> TreeFacts<'a> {
        TreeFacts {
            repo_name: "zirv-cli".into(),
            usage_5h: Some(34.0),
            approval_items: Vec::new(),
            waits: Vec::new(),
            stalled: Vec::new(),
            retryable: Vec::new(),
            pane_meta: ["seat1", "w1", "w2"]
                .iter()
                .enumerate()
                .map(|(i, short)| PaneMeta {
                    short: (*short).into(),
                    number: i + 1,
                    worktree: format!("wt-{short}"),
                    brief: String::new(),
                })
                .collect(),
            term_size: (160, 45),
            strip_rows: 0,
            seat_harness: Some("claude"),
            seat_model: Some("fable"),
            seat_role: Some("orchestrator"),
            seat_session: Some("seat-1"),
            rot: Some(18),
            jev,
            workflow: None,
            approvals: 0,
            approval_shorts: Vec::new(),
            approvals_inbox: false,
            pane_shorts: ["seat1", "w1", "w2"].map(String::from).to_vec(),
            focused: None,
            panes_used: 3,
            max_panes: 6,
            max_writers: 1,
            now: 1_240,
            utc_offset: FixedOffset::east_opt(0).expect("utc"),
        }
    }

    /// The busy world as the orchestrator dashboard sees it: every node has a pane, the worker
    /// waits on an approval shown in full, the seat has asked a question, the Explorer has
    /// stalled and the Plan agent has failed.
    pub(super) fn orch_facts<'a>(
        wf: &'a ActiveWorkflowSummary,
        jev: &'a JevSectionFact,
    ) -> TreeFacts<'a> {
        let mut f = facts(Some(jev));
        f.workflow = Some(wf);
        f.pane_shorts = ["seat1", "a1", "a2", "a3", "w1"].map(String::from).to_vec();
        f.pane_meta = f
            .pane_shorts
            .iter()
            .enumerate()
            .map(|(i, short)| PaneMeta {
                short: short.clone(),
                number: i + 1,
                worktree: "wt-at-models".into(),
                brief: if short == "w1" {
                    "edits + tests for #839".into()
                } else {
                    String::new()
                },
            })
            .collect();
        f.approval_items = vec![ApprovalFact {
            short: "w1".into(),
            conn: 1,
            tool: "Bash".into(),
            preview: "cargo nextest run --no-fail-fast".into(),
            waited_secs: 42,
            fully_shown: true,
            released: false,
            view: content::ApprovalView {
                command: "cargo nextest run --no-fail-fast".into(),
                ..content::ApprovalView::default()
            },
        }];
        f.approval_shorts = vec!["w1".into()];
        f.approvals = 1;
        f.waits = vec![WaitFact {
            short: "seat1".into(),
            kind: WaitKind::Question,
            since: f.now - 70,
            evidence: "pick a layout: outline or graph".into(),
        }];
        f.stalled = vec![("a1".into(), f.now - 30)];
        f
    }

    pub(super) fn view(data: TreeData) -> TreeView {
        TreeView {
            visible: true,
            data,
            ..TreeView::default()
        }
    }

    /// Render into a test terminal and return the text rows.
    pub(super) fn draw(width: u16, height: u16, view: &TreeView, facts: &TreeFacts) -> String {
        let buffer = draw_buffer(width, height, view, facts);
        (0..height)
            .map(|y| {
                (0..width)
                    .map(|x| buffer[(x, y)].symbol().to_string())
                    .collect::<String>()
                    .trim_end()
                    .to_string()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// The phase-1 page at any size, as text.
    pub(super) fn draw_classic(
        width: u16,
        height: u16,
        view: &TreeView,
        facts: &TreeFacts,
    ) -> String {
        let buffer = draw_classic_buffer(width, height, view, facts);
        (0..height)
            .map(|y| {
                (0..width)
                    .map(|x| buffer[(x, y)].symbol().to_string())
                    .collect::<String>()
                    .trim_end()
                    .to_string()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    pub(super) fn draw_classic_buffer(
        width: u16,
        height: u16,
        view: &TreeView,
        facts: &TreeFacts,
    ) -> ratatui::buffer::Buffer {
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(width, height))
                .expect("terminal");
        terminal
            .draw(|f| super::draw::render_classic(f, f.area(), view, facts))
            .expect("draw");
        terminal.backend().buffer().clone()
    }

    pub(super) fn draw_buffer(
        width: u16,
        height: u16,
        view: &TreeView,
        facts: &TreeFacts,
    ) -> ratatui::buffer::Buffer {
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(width, height))
                .expect("terminal");
        terminal
            .draw(|f| super::draw::render_with(f, f.area(), view, facts, true))
            .expect("draw");
        terminal.backend().buffer().clone()
    }
}

#[cfg(test)]
mod tests {
    use super::testkit::*;
    use super::*;

    #[test]
    fn a_meta_discovered_agent_gets_one_dispatch_row_and_a_hooked_one_none() {
        let mut meta = node_k(
            "m1",
            "seat-1",
            "subagent",
            "claude",
            "sonnet",
            "Explore",
            "completed",
        );
        meta.job = Some("Map the tree".into());
        meta.started_at = Some(500);
        let mut hooked = node_k(
            "h1",
            "seat-1",
            "subagent",
            "claude",
            "sonnet",
            "Plan",
            "completed",
        );
        hooked.started_at = Some(600);
        let nodes = vec![meta, hooked];
        let mut start = event(600, "seat-1", "subagent_start", "Plan h1", None);
        start.to = Some("h1".into());
        let events = vec![start];
        let made = dispatch_events(&nodes, &events);
        assert_eq!(made.len(), 1);
        assert_eq!(
            (
                made[0].kind.as_str(),
                made[0].actor.as_str(),
                made[0].summary.as_str(),
                made[0].ts
            ),
            ("dispatch", "seat-1", "Map the tree", 500)
        );
        assert_eq!(made[0].to.as_deref(), Some("Explore"));
    }

    #[test]
    fn a_hooked_agent_without_a_type_gets_no_dispatch_row() {
        let mut hooked = node_k(
            "h1",
            "seat-1",
            "subagent",
            "claude",
            "sonnet",
            "",
            "completed",
        );
        hooked.started_at = Some(600);
        let mut start = event(600, "seat-1", "subagent_start", " h1", None);
        start.to = Some("h1".into());
        assert!(dispatch_events(&[hooked], &[start]).is_empty());
    }

    #[test]
    fn late_synthetic_dispatch_rows_never_push_newer_real_events_out_of_the_cap() {
        let mut all = vec![event(1_000, "seat-1", "mail", "newest", None)];
        for ts in 0..(EVENTS_KEPT as u64 + 10) {
            all.push(event(ts, "seat-1", "dispatch", "job", None));
        }
        let kept = connection_events(all);
        assert_eq!(kept.len(), EVENTS_KEPT);
        assert!(kept.iter().any(|e| e.kind == "mail"));
    }

    #[test]
    fn only_connections_between_agents_survive_and_hook_decisions_never_take_the_cap() {
        let mut all = vec![
            event(10, "seat-1", "mail", "hello", None),
            event(11, "seat-1", "subagent_start", "Explore a1", None),
            event(12, "seat-1", "jev", "dispatch tier", Some(0.9)),
        ];
        for ts in 100..300 {
            all.push(event(
                ts,
                "seat-1",
                "decision",
                "hook healthy forward: /x",
                None,
            ));
            all.push(event(ts, "seat-1", "safety", "allow cargo", None));
            all.push(event(
                ts,
                "seat-1",
                "subagent_stop",
                "Explore a1 done",
                None,
            ));
        }
        let kept = connection_events(all);
        let kinds: Vec<&str> = kept.iter().map(|e| e.kind.as_str()).collect();
        assert_eq!(kinds, ["mail", "subagent_start", "jev"]);
    }
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    fn wait_for_data(view: &mut TreeView) {
        for _ in 0..200 {
            view.poll();
            if view.inflight.is_none() {
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        panic!("gather never finished");
    }

    #[test]
    fn hidden_view_never_gathers() {
        let calls = Arc::new(AtomicUsize::new(0));
        let mut view = TreeView::default();
        let now = Instant::now();
        // The event loop only starts a gather when `due` says so.
        for step in 0..5 {
            let at = now + Duration::from_secs(step * 3);
            if view.due(at) {
                let calls = Arc::clone(&calls);
                view.start(at, move || {
                    calls.fetch_add(1, Ordering::SeqCst);
                    TreeData::default()
                });
            }
            view.poll();
        }
        assert_eq!(
            calls.load(Ordering::SeqCst),
            0,
            "dashboard view gathers nothing"
        );
        assert!(view.inflight.is_none());
    }

    #[test]
    fn toggle_shows_gathers_at_the_facts_cadence_and_hides_cleanly() {
        let calls = Arc::new(AtomicUsize::new(0));
        let mut view = TreeView::default();
        view.toggle();
        assert!(view.is_visible());
        let now = Instant::now();
        assert!(view.due(now), "showing refreshes at once");
        let counter = Arc::clone(&calls);
        view.start(now, move || {
            counter.fetch_add(1, Ordering::SeqCst);
            fixture()
        });
        assert!(!view.due(now), "one gather in flight at a time");
        wait_for_data(&mut view);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(view.data.nodes.len(), 4);
        assert!(
            !view.due(now + FACTS_THROTTLE / 2),
            "no faster than the facts throttle"
        );
        assert!(view.due(now + FACTS_THROTTLE));

        view.toggle();
        assert!(!view.is_visible());
        assert!(view.data.nodes.is_empty(), "a hidden tree holds no data");
        assert!(!view.due(now + FACTS_THROTTLE * 5));
    }

    #[test]
    fn a_dead_gather_is_one_dim_error_line_and_the_next_gather_clears_it() {
        let mut view = TreeView::default();
        view.toggle();
        let (tx, rx) = mpsc::channel::<TreeData>();
        drop(tx);
        view.inflight = Some(rx);
        view.poll();
        assert_eq!(view.error.as_deref(), Some("gather failed, retrying"));
        let text = draw(100, 34, &view, &facts(None));
        assert!(text.contains("gather failed, retrying"), "{text}");
        view.start(Instant::now(), fixture);
        wait_for_data(&mut view);
        assert_eq!(view.error, None);
    }

    #[test]
    fn the_scope_survives_hiding_the_tree() {
        let mut view = TreeView::default();
        view.toggle();
        view.scope = Scope::All;
        view.toggle();
        view.toggle();
        assert_eq!(view.scope, Scope::All);
    }

    #[test]
    fn mail_counts_credit_both_ends_and_unread_only_to_the_recipient() {
        let edge = |from: &str, to: Option<&str>, unread| mail::MailEdge {
            ts: 1,
            from_session: from.into(),
            to_session: to.map(String::from),
            to_label: "x".into(),
            unread,
            first_line: String::new(),
            topic: None,
        };
        let counts = mail_counts(&[
            edge("aaaaaaaa11", Some("bbbbbbbb22"), true),
            edge("bbbbbbbb22", Some("aaaaaaaa11"), false),
            edge("aaaaaaaa11", None, false),
        ]);
        assert_eq!(
            counts["aaaaaaaa"],
            MailCount {
                recent: 3,
                unread: 0
            }
        );
        assert_eq!(
            counts["bbbbbbbb"],
            MailCount {
                recent: 2,
                unread: 1
            }
        );
    }

    #[test]
    fn the_supervisor_fact_reads_its_state_and_caps_the_advice_to_one_line() {
        let dir = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_path(dir.path().join("state"));
        let mut cfg = CtxConfig::default();
        assert_eq!(supervisor_fact(&state, &cfg, Some("seat0001")), None, "off");
        cfg.supervisor.enabled = true;
        cfg.supervisor.harness = "codex".into();
        cfg.supervisor.model = "gpt-6-astra".into();
        let idle = supervisor_fact(&state, &cfg, Some("seat0001")).expect("enabled");
        assert_eq!((idle.calls, idle.max_calls, idle.last), (0, 3, None));
        let long = format!("fixture path wrong\nsecond line\n{}", "x".repeat(200));
        assert_eq!(capped_first_line(&long, 60), "fixture path wrong");
        assert_eq!(capped_first_line(&"y".repeat(100), 10).chars().count(), 10);
    }

    /// Enter opens a box's chat inside the tree, `^A t` comes back to the flow, and Esc is the
    /// pane's: the harnesses use it to interrupt.
    #[test]
    fn a_chat_opens_in_place_and_hands_every_key_including_esc_to_its_pane() {
        use crate::commands::ctx::dash::input::{DashAction, InputVerdict, filter_key};
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        let mut view = view(fixture());
        let f = facts(None);
        let area = Surface::page(ratatui::layout::Rect::new(0, 0, 120, 36));
        view.selected = Sel::Agent("w1".into());
        assert!(view.captures_plain_keys(), "the flow reads plain keys");
        assert_eq!(
            view.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), area, &f),
            Outcome::OpenPane("w1".into())
        );
        view.open_chat();
        assert!(
            view.in_chat() && view.is_visible(),
            "the chat opens inside the tree"
        );
        assert_eq!(view.chat_rows((80, 24), 0), 1, "under a one-line bar");
        // 120x35 minus a 5-row approvals strip is too short for the chrome `build_chat` would draw.
        assert_eq!(view.chat_rows((120, 35), 0), 4);
        assert_eq!(
            (
                view.chat_rows((120, 35), 5),
                view.chat_bottom_rows((120, 35), 5)
            ),
            (1, 0)
        );
        assert_eq!(
            view.chat_rows((160, 45), 0),
            4,
            "under the header, stepper and bar"
        );
        assert!(
            !view.captures_plain_keys(),
            "the pane gets every key, so Esc reaches it"
        );
        let esc = KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE);
        assert!(matches!(
            filter_key(false, esc),
            (false, InputVerdict::ToChild(bytes)) if bytes == b"\x1b"
        ));
        // `^A t`: the chat closes to the flow, the tree stays up, then the next one leaves it.
        let (armed, _) = filter_key(
            false,
            KeyEvent::new(KeyCode::Char('a'), KeyModifiers::CONTROL),
        );
        assert!(armed);
        assert_eq!(
            filter_key(true, KeyEvent::new(KeyCode::Char('t'), KeyModifiers::NONE)).1,
            InputVerdict::Dash(DashAction::ToggleTree)
        );
        view.chord_toggle();
        assert!(!view.in_chat() && view.is_visible() && view.captures_plain_keys());
        view.chord_toggle();
        assert!(
            !view.is_visible(),
            "from the flow the chord goes back to the dashboard"
        );
    }
}
