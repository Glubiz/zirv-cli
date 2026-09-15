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
use crate::commands::workflow::team::TeamPlan;

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
    /// Where the [`TeamPlan`] for this objective lives (issue #541 chunk C,
    /// decision 1). `None` until `team_plan` is compiled at least once.
    #[serde(default)]
    pub team_plan: Option<TeamPlanLocation>,
    #[serde(default)]
    pub updated_at: u64,
}

/// One source of truth for a compiled [`TeamPlan`]: a workflow OWNS it when
/// one is active for this objective (the SAME `WorkflowState::team_plan`
/// `zirv workflow team` itself reads and writes), and this record carries
/// only the workflow's id -- never a second copy that could drift from the
/// first. `Inline` is the fallback for a coordinator with no active
/// workflow at all.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TeamPlanLocation {
    Workflow { workflow_id: String },
    Inline { plan: Box<TeamPlan> },
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
            team_plan: None,
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

    /// Issue #541 chunk C, decision 1: the `team_plan` tool's storage rule --
    /// a workflow that is active for this objective OWNS the plan, and this
    /// record keeps only its id.
    pub fn store_team_plan_workflow(&mut self, workflow_id: &str, now: u64) {
        self.team_plan = Some(TeamPlanLocation::Workflow {
            workflow_id: workflow_id.to_string(),
        });
        self.updated_at = now;
    }

    /// The fallback for a coordinator running with no active workflow.
    pub fn store_team_plan_inline(&mut self, plan: TeamPlan, now: u64) {
        self.team_plan = Some(TeamPlanLocation::Inline {
            plan: Box::new(plan),
        });
        self.updated_at = now;
    }

    /// Issue #541 chunk C, decision 2: whether `seat_id` in this coordinator's
    /// team plan already has an active answerer. `Filled` covers `Delegated`
    /// (a live delegation is answering for it) and `Completed` (the seat's
    /// work is done, so the plan is satisfied and must not be re-dispatched);
    /// anything else -- no node at all, `Planned`, `Failed`, `Cancelled` -- is
    /// `Empty` and free to (re)fill, which is what lets a retry after a
    /// failure re-fill the SAME seat rather than being permanently refused.
    pub fn seat_filled(&self, seat_id: &str) -> bool {
        self.nodes
            .get(seat_id)
            .is_some_and(|node| matches!(node.state, NodeState::Delegated | NodeState::Completed))
    }

    /// Issue #541 chunk C review finding: whether `seat_id`'s CLAIM is
    /// currently active -- true only while a delegation is IN FLIGHT for it
    /// (`Delegated`). Deliberately narrower than [`Self::seat_filled`],
    /// which answers a different question (may this seat be matched again)
    /// and treats `Completed` as filled forever: a settled seat --
    /// `Completed`, `Failed`, `Cancelled` -- has released its claim, so a
    /// bug-fix plan's `debugger-1` and `implementer-1` sharing a claim can
    /// hand off sequentially (the debugger settles, THEN the implementer is
    /// admitted) without either treating the other as a permanent conflict.
    pub fn seat_claim_active(&self, seat_id: &str) -> bool {
        self.nodes
            .get(seat_id)
            .is_some_and(|node| node.state == NodeState::Delegated)
    }
}

/// Issue #541 chunk C, decision 1: resolves the [`TeamPlan`] a coordinator
/// record points at, following [`TeamPlanLocation::Workflow`] through the
/// workflow engine's own store when the plan lives there. `None` when no
/// plan has been compiled yet, or when a `Workflow` reference names a
/// workflow that no longer exists (deleted, or a state directory an
/// operator pruned) -- read failure here is "no plan", never an error a
/// bounds check has no way to surface.
pub fn resolve_team_plan(state: &StateDir, repo: &Path, record: &Coordinator) -> Option<TeamPlan> {
    match record.team_plan.as_ref()? {
        TeamPlanLocation::Inline { plan } => Some(plan.as_ref().clone()),
        TeamPlanLocation::Workflow { workflow_id } => {
            crate::commands::workflow::engine::load(state, repo, workflow_id)
                .ok()
                .and_then(|workflow| workflow.team_plan)
        }
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
    /// Issue #541 chunk C, decision 2: identity facts about the manifest this
    /// delegation named (or the role's own default). `None` when the child
    /// role is outside the closed team (`worker`, `seat`, an operator's own
    /// label) -- the manifest/team-plan system applies only to a recognised
    /// [`team::TeamRole`], exactly like `team::authority` itself.
    pub manifest: Option<ManifestBounds<'a>>,
    /// Issue #541 chunk C, decision 2/3: what the caller has already resolved
    /// about the team plan for this objective, gathered from the
    /// coordinator's own graph and the plan before `check` is called (a
    /// registry/plan lookup is I/O, so it cannot happen inside this pure
    /// function). `None` when there is no plan concept to check against --
    /// same rule as `manifest`.
    pub plan: Option<PlanBounds<'a>>,
}

/// Issue #541 chunk C, decision 2: what the caller resolved about the
/// manifest a delegation named.
#[derive(Clone, Copy, Debug)]
pub struct ManifestBounds<'a> {
    pub requested_id: &'a str,
    /// `None` when `requested_id` names no manifest the registry knows.
    pub known: Option<ManifestFacts>,
}

#[derive(Clone, Copy, Debug)]
pub struct ManifestFacts {
    pub team_role: team::TeamRole,
    pub may_write: bool,
}

/// Issue #541 chunk C, decision 2/3: what the caller resolved about the team
/// plan for this objective and this delegation's place in it.
#[derive(Clone, Copy, Debug)]
pub struct PlanBounds<'a> {
    /// A plan exists for this objective -- even a zero-seat one (a Direct
    /// execution objective) counts, so an empty plan still refuses a
    /// delegation that does not match anything in it.
    pub exists: bool,
    /// The seat id this delegation's (manifest, role) resolves to in the
    /// plan, when it names one that is not already filled
    /// ([`Coordinator::seat_filled`]).
    pub matching_unfilled_seat: Option<&'a str>,
    /// The matched seat's own claim paths (empty for an independent,
    /// no-claim seat, or when there is no match at all).
    pub matched_claim_paths: &'a [String],
    /// Claim paths of every OTHER seat this coordinator's graph currently
    /// shows as filled, gathered by the caller from the plan and the graph --
    /// so two writers whose claims overlap are refused before the second
    /// one's receipt exists, not after.
    pub active_claim_paths: &'a [&'a [String]],
    /// Whether the DELEGATING seat is itself the coordinator -- only the
    /// coordinator seat may pass `override_requested`.
    pub is_coordinator: bool,
    /// The caller asked to bypass the "must match an unfilled seat" rule.
    pub override_requested: bool,
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
    /// Issue #541 chunk C, decision 2: whether this grant used the
    /// coordinator's `override: true` escape from the team-plan match rule --
    /// recorded so the launch receipt can carry it rather than leaving an
    /// override invisible after the fact.
    pub plan_override: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Refusal {
    /// The user cancelled the objective; no further work is dispatched.
    Cancelled,
    /// The delegating seat's own role may not delegate at all.
    ParentMayNotDelegate { role: String },
    /// The delegating session's envelope is out of delegation hops.
    DepthExhausted,
    /// Issue #541 chunk C, decision 2: the named manifest is not one the
    /// registry knows.
    UnknownManifest { manifest_id: String },
    /// The manifest's own team role does not match the role this delegation
    /// requested for it.
    ManifestRoleMismatch {
        manifest_id: String,
        manifest_team_role: String,
        requested_role: String,
    },
    /// The manifest may write but the requested role is read-only by
    /// identity: the manifest would grant authority wider than the role's.
    ManifestWiderThanRole { manifest_id: String, role: String },
    /// A team plan exists for this objective and this delegation matches no
    /// unfilled seat in it.
    NotInTeamPlan,
    /// The matched seat's claim overlaps a claim another currently-filled
    /// seat already holds; two overlapping writers may not both be
    /// dispatched.
    ClaimConflict { seat_id: String },
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
            Self::UnknownManifest { manifest_id } => {
                write!(f, "unknown manifest '{manifest_id}'")
            }
            Self::ManifestRoleMismatch {
                manifest_id,
                manifest_team_role,
                requested_role,
            } => write!(
                f,
                "manifest '{manifest_id}' maps to team role '{manifest_team_role}', not the \
                 requested '{requested_role}'"
            ),
            Self::ManifestWiderThanRole { manifest_id, role } => write!(
                f,
                "manifest '{manifest_id}' may write, which is wider than role '{role}''s own \
                 authority"
            ),
            Self::NotInTeamPlan => {
                f.write_str("not in the team plan; run team_plan again or pass override: true")
            }
            Self::ClaimConflict { seat_id } => write!(
                f,
                "seat '{seat_id}' claims paths another currently-dispatched seat already holds"
            ),
        }
    }
}

impl std::error::Error for Refusal {}

/// The pure bounds decision. No clock, no filesystem, no config: identical
/// inputs give an identical verdict, the same discipline `rot.rs` keeps.
///
/// Issue #541 chunk C: the manifest identity check and the team-plan match
/// happen AFTER role/depth/cancellation but still entirely before any
/// durable launch receipt exists -- a refused delegation here leaves no
/// receipt naming work nobody started, exactly like the pre-existing three
/// checks above them.
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

    if let Some(manifest) = &bounds.manifest {
        match manifest.known {
            None => {
                return Err(Refusal::UnknownManifest {
                    manifest_id: manifest.requested_id.to_string(),
                });
            }
            Some(facts) => {
                if team::TeamRole::parse(bounds.child_role) != Some(facts.team_role) {
                    return Err(Refusal::ManifestRoleMismatch {
                        manifest_id: manifest.requested_id.to_string(),
                        manifest_team_role: facts.team_role.as_str().to_string(),
                        requested_role: bounds.child_role.to_string(),
                    });
                }
                if facts.may_write && !team::authority(bounds.child_role).may_write {
                    return Err(Refusal::ManifestWiderThanRole {
                        manifest_id: manifest.requested_id.to_string(),
                        role: bounds.child_role.to_string(),
                    });
                }
            }
        }
    }

    let mut plan_override = false;
    if let Some(plan) = &bounds.plan
        && plan.exists
    {
        match plan.matching_unfilled_seat {
            Some(seat_id) => {
                if plan
                    .active_claim_paths
                    .iter()
                    .any(|other| claim_paths_overlap(plan.matched_claim_paths, other))
                {
                    return Err(Refusal::ClaimConflict {
                        seat_id: seat_id.to_string(),
                    });
                }
            }
            None => {
                if plan.is_coordinator && plan.override_requested {
                    plan_override = true;
                } else {
                    return Err(Refusal::NotInTeamPlan);
                }
            }
        }
    }

    Ok(Grant {
        write: bounds.requested_write && team::authority(bounds.child_role).may_write,
        depth: bounds.depth.saturating_sub(1),
        plan_override,
    })
}

/// Two claims overlap when they share a path AND neither is the "no claim"
/// empty set -- an independent (read-only, `Claim::none()`) seat never
/// conflicts with anything.
fn claim_paths_overlap(a: &[String], b: &[String]) -> bool {
    !a.is_empty() && !b.is_empty() && a.iter().any(|path| b.contains(path))
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
            manifest: None,
            plan: None,
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

    /// Issue #541 chunk C, decision 2: an unknown manifest is refused before
    /// any receipt -- the caller resolves `known: None` from a failed
    /// registry lookup, and `check` never invents a manifest to admit.
    #[test]
    fn a_delegation_with_an_unknown_manifest_is_refused_before_the_receipt() {
        let refusal = check(&Bounds {
            manifest: Some(ManifestBounds {
                requested_id: "does-not-exist",
                known: None,
            }),
            ..bounds(team::COORDINATOR, team::IMPLEMENTER)
        })
        .expect_err("unknown manifest refused");
        assert_eq!(
            refusal,
            Refusal::UnknownManifest {
                manifest_id: "does-not-exist".to_string()
            }
        );
    }

    /// Issue #541 chunk C, decision 2: the manifest's OWN team role has to
    /// agree with the role this delegation requested for it, and a writable
    /// manifest may never be admitted for a role whose own authority is
    /// read-only -- both directions of "a manifest is a routing hint, never
    /// an authorization grant".
    #[test]
    fn a_manifest_whose_team_role_does_not_match_is_refused() {
        let refusal = check(&Bounds {
            manifest: Some(ManifestBounds {
                requested_id: "reviewer",
                known: Some(ManifestFacts {
                    team_role: team::TeamRole::Reviewer,
                    may_write: false,
                }),
            }),
            ..bounds(team::COORDINATOR, team::IMPLEMENTER)
        })
        .expect_err("role mismatch refused");
        assert!(
            matches!(refusal, Refusal::ManifestRoleMismatch { .. }),
            "{refusal:?}"
        );

        let refusal = check(&Bounds {
            manifest: Some(ManifestBounds {
                requested_id: "implementer",
                known: Some(ManifestFacts {
                    team_role: team::TeamRole::Reviewer,
                    may_write: true,
                }),
            }),
            ..bounds(team::COORDINATOR, team::REVIEWER)
        })
        .expect_err("wider-than-role refused");
        assert!(
            matches!(refusal, Refusal::ManifestWiderThanRole { .. }),
            "{refusal:?}"
        );

        // A matching, correctly-scoped manifest is admitted.
        let grant = check(&Bounds {
            manifest: Some(ManifestBounds {
                requested_id: "implementer",
                known: Some(ManifestFacts {
                    team_role: team::TeamRole::Implementer,
                    may_write: true,
                }),
            }),
            ..bounds(team::COORDINATOR, team::IMPLEMENTER)
        })
        .expect("matching manifest admitted");
        assert!(grant.write);
    }

    /// Issue #541 chunk C, decision 2: once a plan exists for the objective,
    /// a delegation must match an unfilled seat in it -- refused with
    /// `NotInTeamPlan` otherwise, unless the COORDINATOR (never any other
    /// role) passes `override_requested`, which is recorded on the grant.
    #[test]
    fn a_delegation_outside_the_team_plan_is_refused_unless_overridden_by_the_coordinator() {
        let no_match = PlanBounds {
            exists: true,
            matching_unfilled_seat: None,
            matched_claim_paths: &[],
            active_claim_paths: &[],
            is_coordinator: false,
            override_requested: false,
        };
        assert_eq!(
            check(&Bounds {
                plan: Some(no_match),
                ..bounds(team::COORDINATOR, team::IMPLEMENTER)
            }),
            Err(Refusal::NotInTeamPlan)
        );

        // A non-coordinator's override is ignored: only the coordinator seat
        // may bypass the team-plan match.
        assert_eq!(
            check(&Bounds {
                plan: Some(PlanBounds {
                    override_requested: true,
                    is_coordinator: false,
                    ..no_match
                }),
                ..bounds(team::SUB_ORCHESTRATOR, team::IMPLEMENTER)
            }),
            Err(Refusal::NotInTeamPlan)
        );

        // The coordinator's own override is admitted and recorded.
        let grant = check(&Bounds {
            plan: Some(PlanBounds {
                override_requested: true,
                is_coordinator: true,
                ..no_match
            }),
            ..bounds(team::COORDINATOR, team::IMPLEMENTER)
        })
        .expect("coordinator override admitted");
        assert!(grant.plan_override);

        // A matching unfilled seat needs no override at all.
        let grant = check(&Bounds {
            plan: Some(PlanBounds {
                matching_unfilled_seat: Some("implementer-1"),
                ..no_match
            }),
            ..bounds(team::COORDINATOR, team::IMPLEMENTER)
        })
        .expect("matching seat admitted");
        assert!(!grant.plan_override);
    }

    /// Issue #541 chunk C, decision 3: two seats whose claims overlap may
    /// never both be dispatched -- the caller gathers every OTHER currently
    /// filled seat's claim from the graph, and `check` refuses a match
    /// against any of them.
    #[test]
    fn overlapping_seat_claims_cannot_both_be_dispatched() {
        let seat_a_claim = vec!["src/a.rs".to_string(), "src/b.rs".to_string()];
        let active: &[&[String]] = &[&seat_a_claim];

        let seat_b_claim = vec!["src/b.rs".to_string()];
        let refusal = check(&Bounds {
            plan: Some(PlanBounds {
                exists: true,
                matching_unfilled_seat: Some("implementer-2"),
                matched_claim_paths: &seat_b_claim,
                active_claim_paths: active,
                is_coordinator: false,
                override_requested: false,
            }),
            ..bounds(team::COORDINATOR, team::IMPLEMENTER)
        })
        .expect_err("overlapping claim refused");
        assert_eq!(
            refusal,
            Refusal::ClaimConflict {
                seat_id: "implementer-2".to_string()
            }
        );

        // A disjoint claim is admitted.
        let seat_c_claim = vec!["docs/readme.md".to_string()];
        let grant = check(&Bounds {
            plan: Some(PlanBounds {
                exists: true,
                matching_unfilled_seat: Some("implementer-3"),
                matched_claim_paths: &seat_c_claim,
                active_claim_paths: active,
                is_coordinator: false,
                override_requested: false,
            }),
            ..bounds(team::COORDINATOR, team::IMPLEMENTER)
        })
        .expect("disjoint claim admitted");
        assert!(grant.write);
    }

    /// Issue #541 chunk C, decision 2/3: a seat that is currently filled
    /// cannot be matched again (refused, exactly like being outside the
    /// plan) -- but once it settles to `Failed`, a retry names the SAME seat
    /// id and `Coordinator::dispatched` reuses the identical graph node,
    /// never creating a second, competing writer for it.
    #[test]
    fn a_retry_refills_the_same_seat_and_never_creates_a_second_writer() {
        let mut record = Coordinator::default();
        record.plan("implementer-1", team::IMPLEMENTER, &[], 1);
        assert!(!record.seat_filled("implementer-1"));

        record.dispatched("implementer-1", team::IMPLEMENTER, "native", "deleg-1", 2);
        assert!(record.seat_filled("implementer-1"));

        let no_match = PlanBounds {
            exists: true,
            matching_unfilled_seat: None,
            matched_claim_paths: &[],
            active_claim_paths: &[],
            is_coordinator: false,
            override_requested: false,
        };
        assert_eq!(
            check(&Bounds {
                plan: Some(no_match),
                ..bounds(team::COORDINATOR, team::IMPLEMENTER)
            }),
            Err(Refusal::NotInTeamPlan),
            "a filled seat cannot be matched a second time while it is still filled"
        );

        record.settled("implementer-1", NodeState::Failed, None, 3);
        assert!(
            !record.seat_filled("implementer-1"),
            "a failed seat is free to retry"
        );

        let grant = check(&Bounds {
            plan: Some(PlanBounds {
                matching_unfilled_seat: Some("implementer-1"),
                ..no_match
            }),
            ..bounds(team::COORDINATOR, team::IMPLEMENTER)
        })
        .expect("retry admitted");
        assert!(grant.write);
        record.dispatched("implementer-1", team::IMPLEMENTER, "native", "deleg-2", 4);
        assert_eq!(
            record.nodes.len(),
            1,
            "the retry reused the same node, never a second one"
        );
        assert_eq!(
            record.nodes["implementer-1"].delegation.as_deref(),
            Some("deleg-2")
        );
    }

    /// Issue #541 chunk C review finding 1, half 1: a seat's claim is
    /// active only while a delegation is IN FLIGHT for it -- `seat_filled`
    /// (matching) and `seat_claim_active` (conflict) diverge exactly on
    /// `Completed`: a finished seat may never be re-matched, but it no
    /// longer excludes anything from a claim-conflict check.
    #[test]
    fn a_completed_seats_claim_is_released_but_a_delegated_seats_is_not() {
        let mut record = Coordinator::default();
        assert!(!record.seat_claim_active("debugger-1"), "no node at all");

        record.dispatched("debugger-1", team::IMPLEMENTER, "native", "deleg-1", 1);
        assert!(record.seat_claim_active("debugger-1"), "in flight");
        assert!(record.seat_filled("debugger-1"));

        record.settled("debugger-1", NodeState::Completed, None, 2);
        assert!(
            !record.seat_claim_active("debugger-1"),
            "a completed seat's claim is released"
        );
        assert!(
            record.seat_filled("debugger-1"),
            "but it is still filled -- it must never be re-matched"
        );

        record.settled("debugger-1", NodeState::Failed, None, 3);
        assert!(!record.seat_claim_active("debugger-1"), "a failed seat too");
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

    /// Issue #541 chunk C, decision 1: a `Workflow` reference to a workflow
    /// that no longer exists resolves to "no plan" rather than an error --
    /// read failure here is a fact a bounds check has no way to surface, not
    /// a caller-visible failure. `seat_filled` on a seat with no node at all
    /// is `false`, the same "free to (re)fill" answer an absent node and a
    /// settled-failed one both give.
    #[test]
    fn a_dangling_workflow_reference_resolves_to_no_plan() {
        let (_dir, state, repo) = fixture();
        let mut record = Coordinator::default();
        assert!(resolve_team_plan(&state, &repo, &record).is_none());
        assert!(!record.seat_filled("implementer-1"));

        record.store_team_plan_workflow("no-such-workflow", 1);
        assert!(resolve_team_plan(&state, &repo, &record).is_none());
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
                manifest: None,
                plan_override: false,
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
                manifest: None,
                plan: None,
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
