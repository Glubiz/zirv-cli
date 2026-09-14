//! The native coordinator's durable state, and the one place a delegation's
//! bounds are decided (issue #485, roadmap N16).
//!
//! A coordinator model reasons about what to do next. It is emphatically not
//! the owner of the facts that decide whether the work is safe, finished or
//! already taken: those live in the services that own them --
//! `ctx::task` (claims), `ctx::permit` (checkouts), `ctx::group` (admission
//! and budget), `ctx::delegation` (launch receipts, terminal outcomes,
//! delivery identities), `ctx::health`/`ctx::allocator` (route health and
//! placement). What this module persists is the part nothing else holds: the
//! **shape of the plan** -- which task depends on which, which role took it,
//! which delegation is answering for it, which bounded evidence reference
//! came back, and what the user has asked for since.
//!
//! Two consequences of that split are load-bearing:
//!
//! - **Restart is a read, not a replay.** [`consume_pending`] asks the
//!   delegation service which terminal outcomes this coordinator has not yet
//!   consumed, consumes each exactly once through `delegation::
//!   consume_delivery`, and folds them into the graph. A node that is already
//!   `Completed` is never touched, so a resumed coordinator does not restart
//!   finished work and does not double-count a receipt it already read.
//! - **Bounds are checked before a launch record exists.** [`check`] is pure
//!   -- role authority, delegation depth, a cancelled objective -- and is
//!   applied at `delegation::delegate`, ahead of the durable launch receipt.
//!   Everything it does NOT check is already enforced centrally somewhere
//!   else and is deliberately not duplicated here: the task claim
//!   (`task::claim_locked`), the per-tree writer permit
//!   (`permit::acquire_writer`, plus `supervise.max_writers`), the group's
//!   `child_limit`/token budget (`group::admit_child`) and the per-provider
//!   token ledger (`reservation::reserve_within`). A second counter for any
//!   of those would be a second answer to a question that already has one.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::CtxResult;
use super::delegation;
use super::state::{StateDir, create_private_dir_all, repo_slug, write_private};
use super::team;

pub const SCHEMA_VERSION: u32 = 1;

/// Bounded on purpose. The record is read into a coordinator's context on
/// every resume, so it has to stay small enough to be worth reading; the
/// authoritative history is the decision log and the delegation records.
pub const MAX_DECISIONS: usize = 100;
pub const MAX_CONSTRAINTS: usize = 32;
pub const MAX_EVIDENCE_PER_NODE: usize = 8;
pub const MAX_TEXT_BYTES: usize = 512;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NodeState {
    /// Planned by the coordinator, nobody dispatched yet.
    #[default]
    Planned,
    /// A delegation is answering for it. Whether that worker is alive is the
    /// delegation record's fact, not this one's.
    Delegated,
    Completed,
    Failed,
    Cancelled,
}

impl NodeState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Planned => "planned",
            Self::Delegated => "delegated",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
        }
    }

    /// Whether the work behind this node is finished for good. A restart
    /// must not re-dispatch one of these.
    pub fn is_settled(self) -> bool {
        matches!(self, Self::Completed | Self::Failed | Self::Cancelled)
    }
}

/// One unit of the coordinator's plan, keyed by the shared task-card id it
/// is about (or an ad-hoc label when no card was minted).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Node {
    pub task: String,
    pub role: String,
    /// `native` or `harness`: which runtime took it. Recorded because a
    /// mixed team's whole point is that the answer varies, and a resumed
    /// coordinator should not have to guess.
    #[serde(default)]
    pub runtime: Option<String>,
    /// The stable delegation handle answering for this node, if one was
    /// started. Never a provider conversation id.
    #[serde(default)]
    pub delegation: Option<String>,
    #[serde(default)]
    pub parents: Vec<String>,
    #[serde(default)]
    pub state: NodeState,
    /// Bounded REFERENCES to evidence -- result-manifest paths, report ids,
    /// verification ids. Never a worker transcript and never a summary long
    /// enough to be one.
    #[serde(default)]
    pub evidence: Vec<String>,
    #[serde(default)]
    pub updated_at: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Decision {
    pub at: u64,
    pub what: String,
}

/// The coordinator's durable record for one repository.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Coordinator {
    #[serde(default = "default_schema_version")]
    pub schema_version: u32,
    /// The coordinating seat's session id, so a resume can tell "this is my
    /// graph" from "another seat is coordinating here".
    #[serde(default)]
    pub session: Option<String>,
    /// The objective this graph serves, mirrored from `ctx::objective` when
    /// the coordinator started. The objective record stays authoritative;
    /// this is the label the graph was planned against.
    #[serde(default)]
    pub objective: Option<String>,
    /// User steering, newest last. Item 7: the operator can change the
    /// target mid-flight and the coordinator has to see it after a restart.
    #[serde(default)]
    pub constraints: Vec<String>,
    /// Set by [`cancel`]. A cancelled objective admits no further
    /// delegations; work already running is stopped through
    /// `delegation::interrupt`, which owns that.
    #[serde(default)]
    pub cancelled: bool,
    #[serde(default)]
    pub nodes: BTreeMap<String, Node>,
    #[serde(default)]
    pub decisions: Vec<Decision>,
    #[serde(default)]
    pub updated_at: u64,
}

fn default_schema_version() -> u32 {
    SCHEMA_VERSION
}

impl Default for Coordinator {
    fn default() -> Self {
        Self {
            schema_version: SCHEMA_VERSION,
            session: None,
            objective: None,
            constraints: Vec::new(),
            cancelled: false,
            nodes: BTreeMap::new(),
            decisions: Vec::new(),
            updated_at: 0,
        }
    }
}

fn clip(text: &str) -> String {
    let trimmed = text.trim();
    if trimmed.len() <= MAX_TEXT_BYTES {
        return trimmed.to_string();
    }
    let mut end = MAX_TEXT_BYTES;
    while end > 0 && !trimmed.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}...", &trimmed[..end])
}

impl Coordinator {
    /// Records one planned unit. Idempotent by task id: re-planning a task
    /// updates its role/parents and leaves a settled state alone, so a
    /// coordinator that re-states its plan after a restart cannot resurrect
    /// finished work.
    pub fn plan(&mut self, task: &str, role: &str, parents: &[String], now: u64) -> &mut Node {
        let node = self.nodes.entry(task.to_string()).or_insert_with(|| Node {
            task: task.to_string(),
            ..Node::default()
        });
        node.role = role.to_string();
        if !parents.is_empty() {
            node.parents = parents.to_vec();
        }
        node.updated_at = now;
        self.updated_at = now;
        self.nodes.get_mut(task).expect("just inserted")
    }

    /// Binds a delegation handle to a node. The handle is what every later
    /// receipt is addressed by, so a node that has one is answerable even if
    /// this record is all a resumed coordinator has.
    pub fn dispatched(
        &mut self,
        task: &str,
        role: &str,
        runtime: &str,
        delegation: &str,
        now: u64,
    ) {
        let node = self.plan(task, role, &[], now);
        node.delegation = Some(delegation.to_string());
        node.runtime = Some(runtime.to_string());
        if !node.state.is_settled() {
            node.state = NodeState::Delegated;
        }
        node.updated_at = now;
        self.updated_at = now;
    }

    /// Settles a node from a receipt. `evidence` is a bounded reference, not
    /// a report.
    pub fn settled(&mut self, task: &str, state: NodeState, evidence: Option<&str>, now: u64) {
        let Some(node) = self.nodes.get_mut(task) else {
            return;
        };
        node.state = state;
        if let Some(reference) = evidence.map(clip).filter(|text| !text.is_empty())
            && !node.evidence.contains(&reference)
        {
            node.evidence.push(reference);
            if node.evidence.len() > MAX_EVIDENCE_PER_NODE {
                node.evidence.remove(0);
            }
        }
        node.updated_at = now;
        self.updated_at = now;
    }

    pub fn decide(&mut self, what: &str, now: u64) {
        self.decisions.push(Decision {
            at: now,
            what: clip(what),
        });
        if self.decisions.len() > MAX_DECISIONS {
            let excess = self.decisions.len() - MAX_DECISIONS;
            self.decisions.drain(..excess);
        }
        self.updated_at = now;
    }

    /// Item 7: the user narrows or redirects the objective. Constraints are
    /// additive and newest-last; a repeated constraint is not duplicated.
    pub fn steer(&mut self, constraint: &str, now: u64) {
        let constraint = clip(constraint);
        if constraint.is_empty() || self.constraints.iter().any(|seen| seen == &constraint) {
            return;
        }
        self.constraints.push(constraint);
        if self.constraints.len() > MAX_CONSTRAINTS {
            self.constraints.remove(0);
        }
        self.updated_at = now;
    }

    /// Item 7: the user cancels. Planned work is cancelled outright;
    /// delegated work keeps its node (the worker may still be running and its
    /// receipt still has to be consumed) and is stopped through
    /// `delegation::interrupt`, which owns cancellation of a live worker.
    pub fn cancel(&mut self, now: u64) {
        self.cancelled = true;
        for node in self.nodes.values_mut() {
            if node.state == NodeState::Planned {
                node.state = NodeState::Cancelled;
                node.updated_at = now;
            }
        }
        self.updated_at = now;
    }

    /// Nodes that still need somebody to do them, oldest first. A settled
    /// node and a node a delegation is already answering for are both
    /// excluded, which is the "no duplicate dispatch after a restart" rule.
    pub fn outstanding(&self) -> Vec<&Node> {
        let mut nodes: Vec<&Node> = self
            .nodes
            .values()
            .filter(|node| node.state == NodeState::Planned)
            .collect();
        nodes.sort_by_key(|node| node.updated_at);
        nodes
    }
}

// -- storage --------------------------------------------------------------

fn record_path(state: &StateDir, repo: &Path) -> PathBuf {
    state
        .coordinator()
        .join(format!("{}.json", repo_slug(repo)))
}

/// A missing or malformed record reads as an empty coordinator: a graph
/// nobody has written yet and a graph that cannot be parsed are the same
/// thing to a caller, and reading must never delete an operator's state to
/// make itself succeed (the idiom `objective::load`/`group::load` already
/// use).
pub fn load(state: &StateDir, repo: &Path) -> Coordinator {
    std::fs::read_to_string(record_path(state, repo))
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_default()
}

pub fn store(state: &StateDir, repo: &Path, record: &Coordinator) -> CtxResult<()> {
    create_private_dir_all(&state.coordinator())?;
    let json = serde_json::to_string_pretty(record)?;
    write_private(&record_path(state, repo), &json)?;
    Ok(())
}

/// Reads, mutates and writes in one call. Every caller in this codebase
/// wants exactly that, and doing it in one place keeps the read-modify-write
/// window as short as it can be without inventing a lock for a
/// single-coordinator record.
pub fn update<T>(
    state: &StateDir,
    repo: &Path,
    mutate: impl FnOnce(&mut Coordinator) -> T,
) -> CtxResult<T> {
    let mut record = load(state, repo);
    let out = mutate(&mut record);
    store(state, repo, &record)?;
    Ok(out)
}

/// [`update`], fenced on the caller's seat generation (issue #488, item 4).
///
/// The coordinator graph is the one durable thing a coordinator OWNS rather
/// than reads, so "exactly one write-capable coordinator generation at any
/// time" has to be enforced here or it is not enforced at all. A rollover
/// that is prepared but not committed leaves the seat with the SOURCE
/// generation, so the source keeps writing its graph and the successor
/// cannot -- and the instant `seat::commit` swaps them, under the seat lock,
/// the answer swaps with it. An injected crash on either side of that write
/// therefore leaves exactly one generation able to mutate the graph, never
/// two and never none.
pub fn update_fenced<T>(
    state: &StateDir,
    repo: &Path,
    seat_short: &str,
    generation: u64,
    mutate: impl FnOnce(&mut Coordinator) -> CtxResult<T>,
) -> CtxResult<T> {
    let _generation = super::seat::lock_generation(state, seat_short, generation)?;
    let mut record = load(state, repo);
    let out = mutate(&mut record)?;
    // A store that fails is a disk fault, not a fencing verdict: the fence
    // answered, and reporting a write failure as a stale generation would
    // tell an operator their seat moved when it did not.
    store(state, repo, &record)?;
    Ok(out)
}

// -- pending completions (item 4/5/7) -------------------------------------

/// A terminal worker outcome this coordinator has not consumed yet.
///
/// Deliberately a bounded manifest reference rather than a result: item 5's
/// rule is that a coordinator is handed where the evidence is, not a replay
/// of the transcript that produced it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Pending {
    pub task: String,
    pub delegation: String,
    pub identity: String,
    pub phase: &'static str,
    pub exit_code: Option<i32>,
    pub result_path: Option<PathBuf>,
}

/// What the coordinator still owes a read on.
///
/// Every fact here comes from the delegation service's own durable records;
/// nothing is inferred from the graph. A node whose delegation record has
/// gone (a pruned state directory) simply produces no pending entry, which
/// is honest: there is no receipt to consume.
pub fn pending(state: &StateDir, repo: &Path, record: &Coordinator) -> Vec<Pending> {
    let mut out = Vec::new();
    for node in record.nodes.values() {
        let Some(handle) = node.delegation.as_deref() else {
            continue;
        };
        let Some(delegated) = delegation::load(state, repo, handle) else {
            continue;
        };
        if !delegated.phase.is_terminal() {
            continue;
        }
        let identity = delegated.identity();
        if delegated.consumed.iter().any(|seen| seen == &identity) {
            continue;
        }
        out.push(Pending {
            task: node.task.clone(),
            delegation: handle.to_string(),
            identity,
            phase: delegated.phase.as_str(),
            exit_code: delegated.exit_code,
            result_path: delegated.result_path.clone(),
        });
    }
    out.sort_by(|a, b| a.task.cmp(&b.task));
    out
}

/// Item 7's restart contract: consume every pending receipt exactly once and
/// fold it into the graph.
///
/// `delegation::consume_delivery` is the exactly-once mechanism and is not
/// reimplemented here -- a receipt it reports as already consumed leaves its
/// node untouched. A node that is already settled is never re-settled, so a
/// coordinator that restarts twice does not restart the work either time.
pub fn consume_pending(
    state: &StateDir,
    repo: &Path,
    record: &mut Coordinator,
    now: u64,
) -> CtxResult<Vec<Pending>> {
    let entries = pending(state, repo, record);
    for entry in &entries {
        let settled = match (entry.phase, entry.exit_code) {
            ("completed", Some(0)) | ("completed", None) => NodeState::Completed,
            ("cancelled", _) => NodeState::Cancelled,
            _ => NodeState::Failed,
        };
        let evidence = entry
            .result_path
            .as_ref()
            .map(|path| path.display().to_string());
        record.settled(&entry.task, settled, evidence.as_deref(), now);
    }
    if !entries.is_empty() {
        store(state, repo, record)?;
    }

    let mut consumed = Vec::new();
    for entry in entries {
        if delegation::consume_delivery(state, repo, &entry.delegation, &entry.identity)? {
            consumed.push(entry);
        }
    }
    if !consumed.is_empty() {
        record.decide(
            &format!("consumed {} pending worker receipt(s)", consumed.len()),
            now,
        );
        store(state, repo, record)?;
    }
    Ok(consumed)
}

// -- delegation bounds (item 3/6) -----------------------------------------

/// Everything a delegation has to clear that is decidable from identity
/// alone, gathered as one value so the decision is a function rather than a
/// scattering of `if`s across the launch path.
#[derive(Clone, Copy, Debug)]
pub struct Bounds<'a> {
    /// The DELEGATING seat's role, read off its persisted seat record
    /// (`ExecutionIdentity::role`). Model output never supplies this.
    pub parent_role: &'a str,
    pub child_role: &'a str,
    /// The delegating session's own `envelope::WorkerEnvelope::
    /// delegation_depth`. `0` means it may not delegate at all -- the same
    /// rule `agent::run_with` applies, checked here as well so a refusal
    /// happens BEFORE a durable launch receipt names work nobody started.
    pub depth: u8,
    /// Whether the user has cancelled the objective.
    pub cancelled: bool,
    /// What the caller asked the child's mode to be. Only ever narrowed.
    pub requested_write: bool,
}

/// What the child is actually allowed, once its own role has had its say.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Grant {
    /// The child's write posture, derived from the CHILD's role rather than
    /// copied from the parent's posture (item 6's "no inherited-parent
    /// over-restriction"): a read-only coordinator still delegates a writing
    /// implementer. A role that is read-only by identity -- reviewer, tester,
    /// researcher, planner -- is clamped to read-only however the request was
    /// spelled (item 6's "no child privilege escalation").
    pub write: bool,
    /// The depth the child's envelope starts from.
    pub depth: u8,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Refusal {
    /// The user cancelled the objective; no further work is dispatched.
    Cancelled,
    /// The delegating seat's own role may not delegate at all.
    ParentMayNotDelegate { role: String },
    /// The delegating session's envelope is out of delegation hops.
    DepthExhausted,
}

impl std::fmt::Display for Refusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Cancelled => f.write_str(
                "the objective has been cancelled; no further work is dispatched (steer or set a \
                 new objective to continue)",
            ),
            Self::ParentMayNotDelegate { role } => write!(
                f,
                "a `{role}` seat may not delegate: its role grants no delegation authority"
            ),
            Self::DepthExhausted => f.write_str(
                "this session's delegation envelope has depth 0; it may not delegate further",
            ),
        }
    }
}

impl std::error::Error for Refusal {}

/// The pure bounds decision. No clock, no filesystem, no config: identical
/// inputs give an identical verdict, the same discipline `rot.rs` keeps.
pub fn check(bounds: &Bounds<'_>) -> Result<Grant, Refusal> {
    if bounds.cancelled {
        return Err(Refusal::Cancelled);
    }
    if !team::authority(bounds.parent_role).may_delegate {
        return Err(Refusal::ParentMayNotDelegate {
            role: bounds.parent_role.to_string(),
        });
    }
    if bounds.depth == 0 {
        return Err(Refusal::DepthExhausted);
    }
    Ok(Grant {
        write: bounds.requested_write && team::authority(bounds.child_role).may_write,
        depth: bounds.depth.saturating_sub(1),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::ctx::config::CtxConfig;
    use crate::commands::ctx::runtime::RuntimeKind;

    fn fixture() -> (tempfile::TempDir, StateDir, PathBuf) {
        let dir = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(dir.path().join("state"));
        let repo = dir.path().join("repo");
        std::fs::create_dir_all(&repo).expect("repo");
        (dir, state, repo)
    }

    fn bounds<'a>(parent: &'a str, child: &'a str) -> Bounds<'a> {
        Bounds {
            parent_role: parent,
            child_role: child,
            depth: 2,
            cancelled: false,
            requested_write: true,
        }
    }

    /// Issue #485 item 6, the "no over-restriction" direction: the child's
    /// write posture is its OWN role's, so a coordinator whose own request
    /// was shaped by a restricted parent still dispatches a writing
    /// implementer.
    #[test]
    fn a_childs_write_posture_comes_from_its_own_role() {
        let grant = check(&bounds(team::COORDINATOR, team::IMPLEMENTER)).expect("admitted");
        assert!(grant.write);
        assert_eq!(grant.depth, 1);
    }

    /// And the "no escalation" direction: a role that is read-only by
    /// identity is clamped however the request was spelled.
    #[test]
    fn a_read_only_role_is_clamped_however_the_request_was_spelled() {
        for role in [
            team::REVIEWER,
            team::TESTER,
            team::RESEARCHER,
            team::PLANNER,
        ] {
            let grant = check(&bounds(team::COORDINATOR, role)).expect("admitted");
            assert!(!grant.write, "{role} must never be granted a writing seat");
        }
    }

    #[test]
    fn a_seat_whose_role_grants_no_delegation_authority_is_refused() {
        let refusal = check(&bounds(team::REVIEWER, team::IMPLEMENTER)).expect_err("refused");
        assert_eq!(
            refusal,
            Refusal::ParentMayNotDelegate {
                role: team::REVIEWER.to_string()
            }
        );
        // A sub-orchestrator may; that is what the role is for.
        assert!(check(&bounds(team::SUB_ORCHESTRATOR, team::TESTER)).is_ok());
    }

    #[test]
    fn depth_and_cancellation_are_refused_before_anything_is_launched() {
        let mut exhausted = bounds(team::COORDINATOR, team::IMPLEMENTER);
        exhausted.depth = 0;
        assert_eq!(check(&exhausted), Err(Refusal::DepthExhausted));

        let mut cancelled = bounds(team::COORDINATOR, team::IMPLEMENTER);
        cancelled.cancelled = true;
        assert_eq!(check(&cancelled), Err(Refusal::Cancelled));
    }

    #[test]
    fn the_graph_survives_a_round_trip_and_is_bounded() {
        let (_dir, state, repo) = fixture();
        let mut record = Coordinator {
            session: Some("coord-1".to_string()),
            objective: Some("ship N16".to_string()),
            ..Coordinator::default()
        };
        record.plan(
            "task-impl",
            team::IMPLEMENTER,
            &["task-plan".to_string()],
            10,
        );
        record.dispatched("task-impl", team::IMPLEMENTER, "native", "deleg1", 11);
        record.steer("do not touch the release branch", 12);
        for n in 0..(MAX_DECISIONS + 20) {
            record.decide(&format!("decision {n}"), 13);
        }
        store(&state, &repo, &record).expect("store");

        let reread = load(&state, &repo);
        assert_eq!(reread.schema_version, SCHEMA_VERSION);
        assert_eq!(reread.objective.as_deref(), Some("ship N16"));
        assert_eq!(reread.constraints, ["do not touch the release branch"]);
        assert_eq!(reread.decisions.len(), MAX_DECISIONS);
        assert_eq!(
            reread.decisions.last().map(|d| d.what.as_str()),
            Some(format!("decision {}", MAX_DECISIONS + 19).as_str()),
            "the ring keeps the NEWEST decisions"
        );
        let node = reread.nodes.get("task-impl").expect("node");
        assert_eq!(node.state, NodeState::Delegated);
        assert_eq!(node.delegation.as_deref(), Some("deleg1"));
        assert_eq!(node.runtime.as_deref(), Some("native"));
        assert_eq!(node.parents, ["task-plan"]);
    }

    #[test]
    fn an_unreadable_record_reads_as_an_empty_graph_rather_than_failing() {
        let (_dir, state, repo) = fixture();
        create_private_dir_all(&state.coordinator()).expect("dir");
        write_private(&record_path(&state, &repo), "{ not json").expect("write");
        assert_eq!(load(&state, &repo), Coordinator::default());
        // And the malformed file is left where it is.
        assert!(record_path(&state, &repo).exists());
    }

    fn publish(state: &StateDir, repo: &Path, handle: &str, code: i32) {
        delegation::record_launch(
            state,
            repo,
            delegation::WorkerHandle {
                delegation: handle.to_string(),
                attempt: 1,
                runtime: RuntimeKind::Native,
                worker_session: format!("{handle}-session"),
                short: "short".to_string(),
                role: team::IMPLEMENTER.to_string(),
                task: Some(handle.to_string()),
                group: None,
                objective: None,
                workdir: repo.to_path_buf(),
            },
            Some("coord-1".to_string()),
            10,
        )
        .expect("launch");
        delegation::publish_terminal(
            state,
            repo,
            &CtxConfig::default(),
            handle,
            if code == 0 {
                delegation::Phase::Completed
            } else {
                delegation::Phase::Failed
            },
            Some(code),
            Some("done".to_string()),
            Some(PathBuf::from("results/r.json")),
            20,
        )
        .expect("publish");
    }

    /// Acceptance criterion 4: a restarted coordinator consumes what is
    /// waiting, exactly once, and does not restart completed work.
    #[test]
    fn a_restarted_coordinator_consumes_pending_receipts_exactly_once() {
        let (_dir, state, repo) = fixture();
        let mut record = Coordinator::default();
        record.plan("d-done", team::IMPLEMENTER, &[], 1);
        record.dispatched("d-done", team::IMPLEMENTER, "native", "d-done", 2);
        record.plan("d-fail", team::TESTER, &[], 1);
        record.dispatched("d-fail", team::TESTER, "harness", "d-fail", 2);
        record.plan("d-todo", team::REVIEWER, &[], 1);
        publish(&state, &repo, "d-done", 0);
        publish(&state, &repo, "d-fail", 1);
        store(&state, &repo, &record).expect("store");

        // The restart: read the graph back and drain what is waiting.
        let mut resumed = load(&state, &repo);
        assert_eq!(pending(&state, &repo, &resumed).len(), 2);
        let consumed = consume_pending(&state, &repo, &mut resumed, 30).expect("consume");
        assert_eq!(consumed.len(), 2);
        assert_eq!(
            resumed.nodes["d-done"].state,
            NodeState::Completed,
            "a zero-exit receipt settles the node"
        );
        assert_eq!(resumed.nodes["d-fail"].state, NodeState::Failed);
        assert_eq!(
            resumed.nodes["d-done"].evidence,
            [PathBuf::from("results/r.json").display().to_string()],
            "the node carries a REFERENCE to the result, never the report"
        );

        // A second restart consumes nothing and restarts nothing: the
        // completed node stays completed and the only outstanding work is the
        // one nobody ever dispatched.
        store(&state, &repo, &resumed).expect("store");
        let mut again = load(&state, &repo);
        assert!(pending(&state, &repo, &again).is_empty());
        assert!(
            consume_pending(&state, &repo, &mut again, 40)
                .expect("consume")
                .is_empty()
        );
        assert_eq!(again.nodes["d-done"].state, NodeState::Completed);
        let outstanding: Vec<&str> = again
            .outstanding()
            .iter()
            .map(|node| node.task.as_str())
            .collect();
        assert_eq!(outstanding, ["d-todo"]);
    }

    #[test]
    fn crash_between_receipt_consumption_and_graph_store_recovers_once() {
        let (_dir, state, repo) = fixture();
        let mut record = Coordinator::default();
        record.dispatched("issue574", team::IMPLEMENTER, "native", "issue574", 1);
        publish(&state, &repo, "issue574", 0);

        std::fs::create_dir_all(state.root()).expect("state root");
        std::fs::write(state.coordinator(), b"blocks coordinator directory")
            .expect("inject graph-store failure");
        assert!(consume_pending(&state, &repo, &mut record, 2).is_err());

        std::fs::remove_file(state.coordinator()).expect("remove fault");
        store(&state, &repo, &record).expect("store graph");
        let mut restarted = load(&state, &repo);
        let consumed = consume_pending(&state, &repo, &mut restarted, 3).expect("recover");
        assert_eq!(consumed.len(), 1);
        assert_eq!(restarted.nodes["issue574"].state, NodeState::Completed);
        assert!(
            consume_pending(&state, &repo, &mut restarted, 4)
                .expect("replay")
                .is_empty()
        );
    }

    /// Item 7: cancelling stops what has not started and leaves what has, so
    /// a live worker's receipt is still consumed rather than lost.
    #[test]
    fn cancelling_stops_planned_work_and_keeps_delegated_work_answerable() {
        let (_dir, state, repo) = fixture();
        let mut record = Coordinator::default();
        record.plan("planned", team::IMPLEMENTER, &[], 1);
        record.plan("running", team::TESTER, &[], 1);
        record.dispatched("running", team::TESTER, "native", "running", 2);
        publish(&state, &repo, "running", 0);
        record.cancel(50);

        assert_eq!(record.nodes["planned"].state, NodeState::Cancelled);
        assert_eq!(record.nodes["running"].state, NodeState::Delegated);
        assert!(record.outstanding().is_empty());
        assert_eq!(
            pending(&state, &repo, &record).len(),
            1,
            "a cancelled objective still owes a read on work that already ran"
        );
        assert_eq!(
            check(&Bounds {
                parent_role: team::COORDINATOR,
                child_role: team::IMPLEMENTER,
                depth: 2,
                cancelled: record.cancelled,
                requested_write: true,
            }),
            Err(Refusal::Cancelled)
        );
    }

    #[test]
    fn re_planning_a_settled_task_never_resurrects_it() {
        let mut record = Coordinator::default();
        record.plan("t", team::IMPLEMENTER, &[], 1);
        record.dispatched("t", team::IMPLEMENTER, "native", "d", 2);
        record.settled("t", NodeState::Completed, Some("results/r.json"), 3);
        record.dispatched("t", team::IMPLEMENTER, "native", "d2", 4);
        assert_eq!(
            record.nodes["t"].state,
            NodeState::Completed,
            "a second dispatch must not move a completed node back to delegated"
        );
        assert!(record.outstanding().is_empty());
    }

    #[test]
    fn steering_is_deduplicated_and_bounded() {
        let mut record = Coordinator::default();
        record.steer("keep the diff small", 1);
        record.steer("keep the diff small", 2);
        assert_eq!(record.constraints.len(), 1);
        for n in 0..(MAX_CONSTRAINTS + 5) {
            record.steer(&format!("constraint {n}"), 3);
        }
        assert_eq!(record.constraints.len(), MAX_CONSTRAINTS);
        assert_eq!(
            record.constraints.last().map(String::as_str),
            Some(format!("constraint {}", MAX_CONSTRAINTS + 4).as_str())
        );
    }

    /// Issue #488 criterion 3: at both injected crash points -- after
    /// `prepare` and after `commit` -- exactly one generation can mutate the
    /// coordinator graph. Before the commit only the source can; after it
    /// only the successor can. There is no instant at which both can, and
    /// none at which neither can.
    #[test]
    fn exactly_one_generation_can_write_the_graph_across_a_rollover() {
        use crate::commands::ctx::seat;
        let (_dir, state, repo) = fixture();
        let session = "9d8c7b6a-5555-4444-8333-222211110000";
        let short = crate::commands::ctx::sessions::short_id(session);
        seat::register(
            &state,
            &short,
            session,
            "claude",
            None,
            "anthropic",
            "orchestrator",
            false,
            1,
        )
        .expect("register");

        let write = |generation: u64, what: &str| {
            update_fenced(&state, &repo, &short, generation, |record| {
                record.decide(what, 1);
                Ok(())
            })
        };

        assert!(write(1, "source plans").is_ok());
        let prepared = seat::prepare_onto(
            &state,
            &short,
            "native",
            None,
            RuntimeKind::Native,
            seat::Cause::Manual,
            2,
        )
        .expect("prepare");

        // CRASH POINT 1: prepared, never committed.
        assert!(write(1, "source keeps the seat").is_ok());
        let refused = write(prepared, "successor jumps the gun").expect_err("fenced");
        assert_eq!(
            refused
                .downcast_ref::<seat::StaleGeneration>()
                .expect("typed stale refusal")
                .reason,
            seat::StaleReason::Uncommitted
        );

        seat::commit(&state, &short, prepared, "native-session", 3).expect("commit");

        // CRASH POINT 2: committed, and the swap is total.
        assert!(write(prepared, "successor owns the graph").is_ok());
        let refused = write(1, "source writes after being replaced").expect_err("fenced");
        assert_eq!(
            refused
                .downcast_ref::<seat::StaleGeneration>()
                .expect("typed stale refusal")
                .reason,
            seat::StaleReason::Superseded
        );

        let record = load(&state, &repo);
        let written: Vec<&str> = record
            .decisions
            .iter()
            .map(|decision| decision.what.as_str())
            .collect();
        assert_eq!(
            written,
            vec![
                "source plans",
                "source keeps the seat",
                "successor owns the graph"
            ],
            "no fenced write reached the graph"
        );
    }

    #[test]
    fn rollover_racing_graph_and_writer_effects_admits_only_new_generation() {
        use crate::commands::ctx::{permit, seat};
        let (_dir, state, repo) = fixture();
        let session = "55355355-5555-4555-8555-555555555555";
        let short = crate::commands::ctx::sessions::short_id(session);
        seat::register(
            &state,
            &short,
            session,
            "claude",
            None,
            "anthropic",
            team::COORDINATOR,
            false,
            1,
        )
        .expect("register");
        let prepared = seat::prepare_onto(
            &state,
            &short,
            "native",
            None,
            RuntimeKind::Native,
            seat::Cause::Manual,
            2,
        )
        .expect("prepare");

        seat::guard(&state, &short, 1).expect("old check observes authority");
        seat::commit(&state, &short, prepared, "successor", 3).expect("commit wins race");

        assert!(
            update_fenced(&state, &repo, &short, 1, |graph| {
                graph.decide("stale", 4);
                Ok(())
            })
            .is_err()
        );
        assert!(load(&state, &repo).decisions.is_empty());
        let tree = std::fs::canonicalize(&repo).expect("tree");
        assert!(matches!(
            permit::acquire_writer(
                &state,
                1,
                "stale",
                &tree,
                Some(permit::SeatFence {
                    short: &short,
                    generation: 1,
                }),
            ),
            Err(permit::WriterRefusal::StaleSeat { .. })
        ));
        let writer = permit::acquire_writer(
            &state,
            1,
            "successor",
            &tree,
            Some(permit::SeatFence {
                short: &short,
                generation: prepared,
            }),
        )
        .expect("new generation owns writer effect");
        drop(writer);
    }
}
