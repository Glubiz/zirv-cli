//! Which nodes the tree shows and how the selection moves through them.
//!
//! The seat is the virtual root. Its direct children, and any other node with no
//! visible parent, are the agent boxes; everything below an agent is listed
//! inside that agent's box as a child line. `group` nodes are bookkeeping and
//! are folded away.

use std::collections::{HashMap, HashSet};

use super::super::super::graph::Node;
use super::super::super::sessions;
use super::content::{Mark, node_job, node_title};
use super::{TreeData, TreeFacts};

/// How much of the machine the tree shows.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(in super::super) enum Scope {
    /// This dashboard's panes and their descendants.
    #[default]
    Dashboard,
    /// Every session registered for this repository.
    Repo,
    All,
}

impl Scope {
    pub(super) fn next(self) -> Self {
        match self {
            Self::Dashboard => Self::Repo,
            Self::Repo => Self::All,
            Self::All => Self::Dashboard,
        }
    }

    pub(super) fn label(self) -> &'static str {
        match self {
            Self::Dashboard => "this dashboard",
            Self::Repo => "this repo",
            Self::All => "all sessions",
        }
    }
}

/// The one selected node, by graph id.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(in super::super) enum Sel {
    #[default]
    Seat,
    /// The Jev box between the seat and the agents; it has no harness.
    Jev,
    Agent(String),
    /// A line inside an agent's box.
    Child(String),
}

pub(super) struct Agent<'a> {
    pub(super) node: &'a Node,
    /// Every descendant, depth first.
    pub(super) kids: Vec<&'a Node>,
}

/// The session an agent without a pane of its own runs inside.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Host {
    /// The host session's short id.
    pub(super) short: String,
    /// `seat`, or the host node's title.
    pub(super) name: String,
    /// The host has a pane on this dashboard.
    pub(super) pane: bool,
    pub(super) harness: Option<String>,
}

pub(super) struct Model<'a> {
    pub(super) data: &'a TreeData,
    pub(super) facts: &'a TreeFacts<'a>,
    pub(super) scope: Scope,
    pub(super) seat: Option<&'a Node>,
    pub(super) agents: Vec<Agent<'a>>,
}

/// The parent a node hangs under once `group` nodes are folded away.
fn effective_parent<'a>(by_id: &HashMap<&str, &'a Node>, node: &'a Node) -> Option<&'a str> {
    let mut parent = node.parent.as_deref()?;
    for _ in 0..16 {
        match by_id.get(parent) {
            Some(p) if p.kind == "group" => parent = p.parent.as_deref()?,
            _ => return Some(parent),
        }
    }
    None
}

impl<'a> Model<'a> {
    pub(super) fn build(data: &'a TreeData, facts: &'a TreeFacts<'a>, scope: Scope) -> Self {
        let by_id: HashMap<&str, &Node> = data.nodes.iter().map(|n| (n.id.as_str(), n)).collect();
        let is_seat = |n: &Node| facts.seat_session.is_some_and(|s| n.id == s);
        let mut children: HashMap<&str, Vec<&Node>> = HashMap::new();
        for node in data.nodes.iter().filter(|n| n.kind != "group") {
            if let Some(parent) = effective_parent(&by_id, node) {
                children.entry(parent).or_default().push(node);
            }
        }
        let in_scope = |n: &Node| match scope {
            Scope::All => true,
            Scope::Dashboard => {
                is_seat(n) || facts.pane_shorts.contains(&sessions::short_id(&n.id))
            }
            Scope::Repo => is_seat(n) || data.repo_ids.contains(&n.id) || n.kind == "delegation",
        };
        let mut visible: HashSet<&str> = HashSet::new();
        let mut stack: Vec<&Node> = data
            .nodes
            .iter()
            .filter(|n| n.kind != "group" && in_scope(n))
            .collect();
        while let Some(node) = stack.pop() {
            if visible.insert(node.id.as_str()) {
                stack.extend(children.get(node.id.as_str()).into_iter().flatten());
            }
        }
        let seat = data.nodes.iter().find(|n| is_seat(n));
        let seat_id = seat.map(|n| n.id.as_str());
        let top_level = |n: &Node| match effective_parent(&by_id, n) {
            None => true,
            Some(parent) => {
                Some(parent) == seat_id || !visible.contains(parent) || !by_id.contains_key(parent)
            }
        };
        let mut placed: HashSet<&str> = HashSet::new();
        let mut agents = Vec::new();
        let tops: Vec<&Node> = data
            .nodes
            .iter()
            .filter(|n| {
                n.kind != "group" && visible.contains(n.id.as_str()) && !is_seat(n) && top_level(n)
            })
            .collect();
        for node in tops {
            let mut kids = Vec::new();
            let mut walk: Vec<&Node> = children
                .get(node.id.as_str())
                .map(|c| c.iter().rev().copied().collect())
                .unwrap_or_default();
            placed.insert(node.id.as_str());
            while let Some(kid) = walk.pop() {
                if !visible.contains(kid.id.as_str()) || !placed.insert(kid.id.as_str()) {
                    continue;
                }
                kids.push(kid);
                walk.extend(
                    children
                        .get(kid.id.as_str())
                        .into_iter()
                        .flatten()
                        .rev()
                        .copied(),
                );
            }
            agents.push(Agent { node, kids });
        }
        // Members of a pure parent cycle have no top level above them: show each once.
        for node in data
            .nodes
            .iter()
            .filter(|n| n.kind != "group" && !is_seat(n))
        {
            if visible.contains(node.id.as_str()) && !placed.contains(node.id.as_str()) {
                placed.insert(node.id.as_str());
                agents.push(Agent {
                    node,
                    kids: Vec::new(),
                });
            }
        }
        let mut model = Self {
            data,
            facts,
            scope,
            seat,
            agents,
        };
        // Live agents first (running, waiting, failed, queued), then the finished, newest first.
        let rank = |model: &Self, node: &Node| match (model.waiting(node), Mark::of(&node.status)) {
            (true, _) => 1,
            (_, Mark::Running) => 0,
            (_, Mark::Failed) => 2,
            (_, Mark::Idle) => 4,
            (_, Mark::Done) => 5,
            _ => 3,
        };
        let mut agents = std::mem::take(&mut model.agents);
        agents.sort_by_key(|a| {
            let rank = rank(&model, a.node);
            let newest = if rank >= 4 {
                a.node.ended_at.or(a.node.started_at).unwrap_or(0)
            } else {
                0
            };
            (rank, std::cmp::Reverse(newest))
        });
        model.agents = agents;
        model
    }

    /// Agents plus every child line: what the header counts.
    pub(super) fn total(&self) -> usize {
        self.agents.iter().map(|a| 1 + a.kids.len()).sum()
    }

    pub(super) fn agent_index(&self, id: &str) -> Option<usize> {
        self.agents.iter().position(|a| a.node.id == id)
    }

    /// The agent whose box lists this child.
    pub(super) fn parent_agent(&self, child: &str) -> Option<usize> {
        self.agents
            .iter()
            .position(|a| a.kids.iter().any(|k| k.id == child))
    }

    pub(super) fn node(&self, sel: &Sel) -> Option<&'a Node> {
        match sel {
            Sel::Seat => self.seat,
            Sel::Jev => None,
            Sel::Agent(id) => self.agent_index(id).map(|i| self.agents[i].node),
            Sel::Child(id) => self
                .parent_agent(id)
                .and_then(|i| self.agents[i].kids.iter().find(|k| k.id == *id).copied()),
        }
    }

    /// The selection if its node is still shown, else the seat.
    pub(super) fn resolve(&self, sel: &Sel) -> Sel {
        match sel {
            Sel::Seat => Sel::Seat,
            Sel::Jev => Sel::Jev,
            Sel::Agent(id) if self.agent_index(id).is_some() => sel.clone(),
            Sel::Child(id) if self.parent_agent(id).is_some() => sel.clone(),
            _ => Sel::Seat,
        }
    }

    /// Seat, then each agent followed by its children: the Tab order.
    pub(super) fn flat(&self) -> Vec<Sel> {
        std::iter::once(Sel::Seat)
            .chain(self.agents.iter().flat_map(|a| {
                std::iter::once(Sel::Agent(a.node.id.clone()))
                    .chain(a.kids.iter().map(|k| Sel::Child(k.id.clone())))
            }))
            .collect()
    }

    /// Next (`1`) or previous (`-1`) in Tab order, wrapping.
    pub(super) fn cycle(&self, sel: &Sel, delta: isize) -> Sel {
        let flat = self.flat();
        let at = flat.iter().position(|s| s == sel).unwrap_or(0) as isize;
        flat[(at + delta).rem_euclid(flat.len() as isize) as usize].clone()
    }

    /// Left and right: the sibling on the same level, wrapping. The seat has none.
    pub(super) fn sibling(&self, sel: &Sel, delta: isize) -> Sel {
        let step = |len: usize, at: usize| (at as isize + delta).rem_euclid(len as isize) as usize;
        match sel {
            Sel::Seat | Sel::Jev => sel.clone(),
            Sel::Agent(id) => match self.agent_index(id) {
                Some(at) => Sel::Agent(self.agents[step(self.agents.len(), at)].node.id.clone()),
                None => Sel::Seat,
            },
            Sel::Child(id) => {
                let Some(parent) = self.parent_agent(id) else {
                    return Sel::Seat;
                };
                let kids = &self.agents[parent].kids;
                let at = kids.iter().position(|k| k.id == *id).unwrap_or(0);
                Sel::Child(kids[step(kids.len(), at)].id.clone())
            }
        }
    }

    /// Down one level: seat to its first agent, agent to its first child.
    pub(super) fn down(&self, sel: &Sel) -> Sel {
        match sel {
            Sel::Seat => self
                .agents
                .first()
                .map_or(Sel::Seat, |a| Sel::Agent(a.node.id.clone())),
            Sel::Agent(id) => self
                .agent_index(id)
                .and_then(|i| self.agents[i].kids.first())
                .map_or_else(|| sel.clone(), |k| Sel::Child(k.id.clone())),
            Sel::Child(_) | Sel::Jev => sel.clone(),
        }
    }

    /// Up one level: child to its agent, agent to the seat.
    pub(super) fn up(&self, sel: &Sel) -> Sel {
        match sel {
            Sel::Seat => Sel::Seat,
            Sel::Agent(_) | Sel::Jev => Sel::Seat,
            Sel::Child(id) => self
                .parent_agent(id)
                .map_or(Sel::Seat, |i| Sel::Agent(self.agents[i].node.id.clone())),
        }
    }

    /// The dashboard pane that runs this node, if any.
    pub(super) fn pane_short(&self, node: &Node) -> Option<String> {
        let short = sessions::short_id(&node.id);
        self.facts.pane_shorts.contains(&short).then_some(short)
    }

    /// The pane of the selection (the seat's own pane for the seat).
    pub(super) fn selected_pane(&self, sel: &Sel) -> Option<String> {
        match sel {
            Sel::Seat => self
                .facts
                .seat_session
                .map(sessions::short_id)
                .filter(|short| self.facts.pane_shorts.contains(short)),
            _ => self.node(sel).and_then(|n| self.pane_short(n)),
        }
    }

    /// The selection that names the node a session short id belongs to, the seat included.
    pub(super) fn sel_for_short(&self, short: &str) -> Option<Sel> {
        if self
            .facts
            .seat_session
            .is_some_and(|seat| sessions::short_id(seat) == short)
        {
            return Some(Sel::Seat);
        }
        self.agents.iter().find_map(|a| {
            if sessions::short_id(&a.node.id) == short {
                return Some(Sel::Agent(a.node.id.clone()));
            }
            a.kids
                .iter()
                .find(|k| sessions::short_id(&k.id) == short)
                .map(|k| Sel::Child(k.id.clone()))
        })
    }

    /// The session a node without a pane runs inside: its nearest ancestor with a pane, else its
    /// root session. A native Claude subagent has no pane; it runs inside its host's Claude Code.
    pub(super) fn host(&self, node: &Node) -> Option<Host> {
        let seat_short = self.facts.seat_session.map(sessions::short_id);
        let host_of = |n: &Node| {
            let short = sessions::short_id(&n.id);
            let name = if seat_short.as_deref() == Some(short.as_str()) {
                "seat".to_string()
            } else {
                node_title(n)
            };
            Host {
                pane: self.facts.pane_shorts.contains(&short),
                short,
                name,
                harness: n.harness.clone(),
            }
        };
        let by_id = |id: &str| self.data.nodes.iter().find(|n| n.id == id);
        let mut parent = node.parent.clone();
        for _ in 0..16 {
            let Some(p) = parent.take().and_then(|id| by_id(&id)) else {
                break;
            };
            parent = p.parent.clone();
            if p.kind != "group" && self.pane_short(p).is_some() {
                return Some(host_of(p));
            }
        }
        let root = by_id(node.session.as_deref()?)?;
        (root.id != node.id).then(|| host_of(root))
    }

    /// What the card and the panels call the node: the job it was given, else the brief its pane
    /// was launched with, else its type.
    pub(super) fn job_of(&self, node: &Node) -> String {
        let short = sessions::short_id(&node.id);
        node.name
            .clone()
            .or_else(|| node_job(node))
            .or_else(|| {
                self.facts
                    .pane_meta
                    .iter()
                    .find(|m| m.short == short)
                    .map(|m| m.brief.clone())
                    .filter(|b| !b.is_empty())
            })
            .unwrap_or_else(|| node_title(node))
    }

    /// The operator has a request open for this node.
    pub(super) fn waiting(&self, node: &Node) -> bool {
        self.facts
            .approval_shorts
            .contains(&sessions::short_id(&node.id))
    }
}

#[cfg(test)]
mod tests {
    use super::super::testkit::*;
    use super::*;

    #[test]
    fn live_agents_come_first_and_the_finished_follow_newest_first() {
        let mut old = node_k(
            "old",
            "seat-1",
            "subagent",
            "claude",
            "sonnet",
            "Old",
            "completed",
        );
        old.ended_at = Some(100);
        let mut new = node_k(
            "new",
            "seat-1",
            "subagent",
            "claude",
            "sonnet",
            "New",
            "completed",
        );
        new.ended_at = Some(900);
        let queued = node_k(
            "q", "seat-1", "subagent", "claude", "sonnet", "Queued", "queued",
        );
        let failed = node_k(
            "f", "seat-1", "subagent", "claude", "sonnet", "Failed", "failed",
        );
        let running = node_k(
            "r", "seat-1", "subagent", "claude", "sonnet", "Running", "running",
        );
        let waiting = node_k(
            "w", "seat-1", "subagent", "claude", "sonnet", "Waiting", "running",
        );
        let mut seat = node("seat-1", None, "orchestrator", "fable", "live");
        seat.kind = "session".into();
        let data = TreeData {
            loaded: true,
            nodes: vec![seat, old, new, queued, failed, waiting, running],
            ..TreeData::default()
        };
        let mut f = facts(None);
        f.approval_shorts = vec!["w".into()];
        let model = Model::build(&data, &f, Scope::Dashboard);
        let order: Vec<&str> = model.agents.iter().map(|a| a.node.id.as_str()).collect();
        assert_eq!(order, ["r", "w", "f", "q", "new", "old"]);
    }

    fn deep() -> TreeData {
        let mut data = fixture();
        data.nodes
            .push(node("w4", Some("w3"), "grand", "m", "running"));
        data.nodes
            .push(node("w5", Some("w1"), "second", "m", "running"));
        data.nodes
            .push(node("lonely", None, "manual", "m", "running"));
        data
    }

    fn all_facts() -> TreeFacts<'static> {
        facts(None)
    }

    #[test]
    fn the_dashboard_scope_is_its_panes_and_their_descendants() {
        let data = deep();
        let f = all_facts();
        let model = Model::build(&data, &f, Scope::Dashboard);
        let tops: Vec<&str> = model.agents.iter().map(|a| a.node.id.as_str()).collect();
        assert_eq!(tops, ["w1", "w2"], "w1 and w2 are panes; `lonely` is not");
        let w1: Vec<&str> = model.agents[0].kids.iter().map(|k| k.id.as_str()).collect();
        assert_eq!(w1, ["w3", "w4", "w5"], "every descendant, depth first");
        assert_eq!(model.total(), 2 + 3);
        assert_eq!(model.seat.map(|n| n.id.as_str()), Some("seat-1"));
    }

    #[test]
    fn the_repo_scope_adds_registered_sessions_and_all_adds_the_rest() {
        let mut data = deep();
        data.nodes[3].kind = "subagent".into();
        let f = all_facts();
        let repo = Model::build(&data, &f, Scope::Repo);
        assert!(
            repo.agents.iter().any(|a| a.node.id == "w1"),
            "delegations are repo scoped"
        );
        assert!(!repo.agents.iter().any(|a| a.node.id == "lonely") || data.repo_ids.is_empty(),);
        data.repo_ids.insert("lonely".into());
        data.nodes
            .iter_mut()
            .find(|n| n.id == "lonely")
            .expect("lonely")
            .kind = "session".into();
        let repo = Model::build(&data, &f, Scope::Repo);
        assert!(repo.agents.iter().any(|a| a.node.id == "lonely"));
        let mut other = deep();
        other.nodes.push({
            let mut n = node("far", None, "elsewhere", "m", "live");
            n.kind = "session".into();
            n
        });
        let all = Model::build(&other, &f, Scope::All);
        assert!(all.agents.iter().any(|a| a.node.id == "far"));
        let dash = Model::build(&other, &f, Scope::Dashboard);
        assert!(!dash.agents.iter().any(|a| a.node.id == "far"));
    }

    #[test]
    fn group_nodes_are_folded_away() {
        let mut data = fixture();
        let mut group = node("group:g1", Some("seat-1"), "scope", "-", "open");
        group.kind = "group".into();
        data.nodes.push(group);
        data.nodes[1].parent = Some("group:g1".into());
        let f = all_facts();
        let model = Model::build(&data, &f, Scope::Dashboard);
        assert!(
            model.agents.iter().any(|a| a.node.id == "w1"),
            "w1 hangs under the seat through the group"
        );
        assert!(!model.agents.iter().any(|a| a.node.kind == "group"));
    }

    #[test]
    fn selection_moves_between_levels_and_siblings_and_wraps() {
        let data = deep();
        let f = all_facts();
        let model = Model::build(&data, &f, Scope::Dashboard);
        let agent = |id: &str| Sel::Agent(id.into());
        let child = |id: &str| Sel::Child(id.into());
        assert_eq!(model.down(&Sel::Seat), agent("w1"));
        assert_eq!(model.down(&agent("w1")), child("w3"));
        assert_eq!(model.down(&child("w3")), child("w3"), "no level below");
        assert_eq!(model.up(&child("w4")), agent("w1"));
        assert_eq!(model.up(&agent("w2")), Sel::Seat);
        assert_eq!(model.sibling(&agent("w1"), 1), agent("w2"));
        assert_eq!(model.sibling(&agent("w2"), 1), agent("w1"), "wraps right");
        assert_eq!(model.sibling(&agent("w1"), -1), agent("w2"), "wraps left");
        assert_eq!(
            model.sibling(&child("w5"), 1),
            child("w3"),
            "children wrap too"
        );
        assert_eq!(model.sibling(&Sel::Seat, 1), Sel::Seat);
        let order = model.flat();
        assert_eq!(order.first(), Some(&Sel::Seat));
        assert_eq!(model.cycle(order.last().expect("last"), 1), Sel::Seat);
        assert_eq!(
            model.cycle(&Sel::Seat, -1),
            *order.last().expect("last"),
            "Tab wraps backwards from the seat"
        );
        assert_eq!(model.cycle(&Sel::Seat, 1), agent("w1"));
    }

    /// A parent cycle must neither hang the build nor hide a node.
    #[test]
    fn a_parent_cycle_shows_each_node_once() {
        let a = node("a", Some("b"), "x", "m", "running");
        let b = node("b", Some("a"), "y", "m", "running");
        let data = TreeData {
            loaded: true,
            nodes: vec![a, b],
            ..TreeData::default()
        };
        let f = all_facts();
        let model = Model::build(&data, &f, Scope::All);
        let mut ids: Vec<&str> = model
            .agents
            .iter()
            .flat_map(|a| {
                std::iter::once(a.node.id.as_str()).chain(a.kids.iter().map(|k| k.id.as_str()))
            })
            .collect();
        ids.sort();
        assert_eq!(ids, ["a", "b"]);
    }

    #[test]
    fn a_vanished_selection_falls_back_to_the_seat_and_panes_resolve_by_short_id() {
        let data = deep();
        let f = all_facts();
        let model = Model::build(&data, &f, Scope::Dashboard);
        assert_eq!(model.resolve(&Sel::Agent("gone".into())), Sel::Seat);
        assert_eq!(
            model.selected_pane(&Sel::Agent("w2".into())).as_deref(),
            Some("w2")
        );
        assert_eq!(
            model.selected_pane(&Sel::Child("w3".into())),
            None,
            "a child without a pane has none to open"
        );
        assert_eq!(model.selected_pane(&Sel::Seat).as_deref(), Some("seat1"));
    }
}
