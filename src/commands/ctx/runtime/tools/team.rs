//! Typed arguments for the native coordinator's team tools (issue #485,
//! roadmap N16).
//!
//! A native coordinator runs the SAME task, group and objective services a
//! `zirv ctx task|group|objective` command runs, over the same durable state.
//! These are thin argument shapes in front of those services -- exactly the
//! shape N10's delegation tools and N15's workflow tools already have. None
//! of them contains a state machine of its own: a second definition of "this
//! card is claimed" would be a second answer to the question the claim exists
//! to settle.
//!
//! Everything that MUTATES shared state (`task_create`, `task_claim`,
//! `group_create`) crosses the broker as a shared-scope knowledge WRITE, so a
//! session with no writer permit for its own worktree -- a read-only helper,
//! a reviewer seat -- is refused at effect time rather than by a prompt it
//! could be talked out of. The three reads (`task_list`, `group_status`,
//! `objective_status`) and the coordinator's own view (`team_status`) are
//! inert.

use std::path::PathBuf;

use serde::Deserialize;

use super::{ToolError, ToolErrorCode};
use crate::commands::ctx::state;
use serde_json::{Value, json};

pub const TASK_CREATE: &str = "task_create";
pub const TASK_CLAIM: &str = "task_claim";
pub const TASK_LIST: &str = "task_list";
pub const GROUP_CREATE: &str = "group_create";
pub const GROUP_STATUS: &str = "group_status";
pub const OBJECTIVE_STATUS: &str = "objective_status";
pub const TEAM_STATUS: &str = "team_status";
/// Issue #541 chunk C, decision 1: compile the proportional team for an
/// objective and persist it (coordinator/sub-orchestrator seats only).
pub const TEAM_PLAN: &str = "team_plan";

/// Every team tool name, in registry order. One list, so the registry, the
/// parser and the dispatcher cannot drift apart.
pub const ALL: [&str; 8] = [
    TASK_CREATE,
    TASK_CLAIM,
    TASK_LIST,
    GROUP_CREATE,
    GROUP_STATUS,
    OBJECTIVE_STATUS,
    TEAM_STATUS,
    TEAM_PLAN,
];

/// How many cards or graph nodes one listing may return. A coordinator that
/// asks for everything gets the newest slice plus a stated total, never a
/// reply that fills its own context with a card index.
pub const DEFAULT_LIST_LIMIT: usize = 50;
pub const MAX_LIST_LIMIT: usize = 200;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct TaskCreateArgs {
    pub title: String,
    /// What a worker claiming this card is told to do.
    pub brief: String,
    /// The team role the coordinator intends to hand this card to. Recorded
    /// on the coordinator's own graph node; it is not an authority grant --
    /// the role a worker actually runs under is the one its seat record
    /// carries.
    #[serde(default)]
    pub role: Option<String>,
    /// Parent card ids that must be `Done` before this card can be claimed.
    #[serde(default)]
    pub parents: Vec<String>,
    #[serde(default)]
    pub group: Option<String>,
    #[serde(default)]
    pub workdir: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct TaskIdArgs {
    pub task: String,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(super) enum CardFilter {
    #[default]
    Open,
    All,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct TaskListArgs {
    #[serde(default)]
    pub filter: CardFilter,
    #[serde(default)]
    pub limit: Option<usize>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct GroupCreateArgs {
    pub scope: String,
    #[serde(default)]
    pub child_limit: Option<u32>,
    #[serde(default)]
    pub token_budget: Option<u64>,
    #[serde(default)]
    pub deadline_secs: Option<u64>,
    #[serde(default)]
    pub completion_contract: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct GroupStatusArgs {
    /// Absent lists every open group in this state directory.
    #[serde(default)]
    pub group: Option<String>,
}

/// `team_plan`: issue #541 chunk C, decision 1. `seat`, when given, bypasses
/// the proportional selection rules and compiles a single explicit seat for
/// this manifest id -- the same `compile_explicit` path `zirv workflow team
/// plan --seat` uses, through the identical capability/team-role/route
/// checks. `task`, only meaningful together with `seat`, overrides that
/// one seat's task text with something more specific than `objective` --
/// ignored when `seat` is absent, since a proportional plan's seats each
/// already get their own task text from the compiler.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct TeamPlanArgs {
    pub objective: String,
    #[serde(default)]
    pub seat: Option<String>,
    #[serde(default)]
    pub task: Option<String>,
}

impl TeamPlanArgs {
    pub(super) fn validate(&self) -> Result<(), ToolError> {
        super::non_empty(self.objective.trim(), "objective")?;
        if let Some(seat) = &self.seat {
            super::non_empty(seat.trim(), "seat")?;
        }
        if let Some(task) = &self.task {
            super::non_empty(task.trim(), "task")?;
        }
        Ok(())
    }
}

/// A work group id names a FILE under the state directory
/// (`<state>/groups/<id>.json`), so it is validated at the argument boundary
/// with the same rule the delegation handle uses: provider output can never
/// name a path outside the store it is addressing.
pub(super) fn validate_id(value: &str, field: &str) -> Result<(), ToolError> {
    if value.is_empty()
        || value.len() > 128
        || !value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return Err(ToolError::new(
            ToolErrorCode::InvalidArguments,
            format!("{field} must be 1-128 characters of [A-Za-z0-9_-]"),
        ));
    }
    Ok(())
}

impl TaskCreateArgs {
    pub(super) fn validate(&self) -> Result<(), ToolError> {
        super::non_empty(self.title.trim(), "title")?;
        super::non_empty(self.brief.trim(), "brief")?;
        for parent in &self.parents {
            validate_id(parent, "parents[]")?;
        }
        if let Some(group) = &self.group {
            validate_id(group, "group")?;
        }
        if let Some(role) = &self.role {
            super::non_empty(role.trim(), "role")?;
        }
        Ok(())
    }
}

impl TaskListArgs {
    pub(super) fn bounded_limit(&self) -> usize {
        self.limit
            .unwrap_or(DEFAULT_LIST_LIMIT)
            .clamp(1, MAX_LIST_LIMIT)
    }
}

impl GroupCreateArgs {
    pub(super) fn validate(&self) -> Result<(), ToolError> {
        super::non_empty(self.scope.trim(), "scope")?;
        if self.child_limit == Some(0) {
            return Err(ToolError::new(
                ToolErrorCode::InvalidArguments,
                "child_limit must be at least 1; a group that admits nobody is not a group",
            ));
        }
        Ok(())
    }
}

impl super::NativeToolClient {
    pub(super) fn task_create(&mut self, args: &TaskCreateArgs) -> Result<Value, ToolError> {
        use crate::commands::ctx::{coordinator, task};

        let slug = state::repo_slug(&self.repo);
        let id = task::create_card(
            &self.state,
            &slug,
            &task::CreateArgs {
                title: args.title.clone(),
                brief: args.brief.clone(),
                parents: args.parents.clone(),
                group: args.group.clone(),
                workdir: args.workdir.as_ref().map(PathBuf::from),
            },
            state::now_secs(),
        )
        .map_err(ToolError::external)?;

        let role = args
            .role
            .clone()
            .unwrap_or_else(|| crate::commands::ctx::team::DEFAULT_ROLE.to_string());
        let now = state::now_secs();
        // Review finding on issue #485: the task card minted above is the
        // authoritative record, so a coordinator-graph store failure must
        // never block it -- but it must not vanish silently either, so it
        // gets the same decision-log line `delegation::delegate` and
        // `objective::run_set` write for their own best-effort graph writes.
        if let Err(error) = coordinator::update(&self.state, &self.repo, |graph| {
            graph.plan(&id, &role, &args.parents, now);
            graph.decide(
                &format!("planned {id} for role {role}: {}", args.title),
                now,
            );
        }) {
            let detail = format!("task {id}: {error}");
            let _ = crate::commands::ctx::log::append(
                &self.state,
                &crate::commands::ctx::log::Decision {
                    ts: now,
                    session: &self.broker.identity().short,
                    verb: "task",
                    verdict: "error",
                    score: 0,
                    action: "coordinator-store-failed",
                    detail: &detail,
                    observed_at: None,
                },
            );
        }
        Ok(json!({"task": id, "role": role, "parents": args.parents}))
    }

    pub(super) fn task_claim(&mut self, args: &TaskIdArgs) -> Result<Value, ToolError> {
        use crate::commands::ctx::{sessions, task};

        let slug = state::repo_slug(&self.repo);
        let identity = self.broker.identity().clone();
        let pid = std::process::id();
        let outcome = task::claim_locked(
            &self.state,
            &slug,
            &args.task,
            &identity.session,
            pid,
            sessions::process_start_secs(pid),
            &task::local_host(),
            state::now_secs(),
            task::DEFAULT_CLAIM_TTL_SECS,
        )
        .map_err(ToolError::external)?;
        match outcome {
            None => Err(ToolError::new(
                ToolErrorCode::PreconditionFailed,
                format!("no task card {:?} in this repository", args.task),
            )),
            // A refusal is an ANSWER, not a malfunction: "somebody else holds
            // this" is exactly what the coordinator needs to hear, and the
            // reason is the refusal's own.
            Some(Err(refusal)) => Ok(json!({
                "claimed": false,
                "task": args.task,
                "reason": refusal.to_string(),
            })),
            Some(Ok(card)) => Ok(json!({
                "claimed": true,
                "task": card.id,
                "attempts": card.attempts,
                "state": card.state.to_string(),
            })),
        }
    }

    pub(super) fn task_list(&self, args: &TaskListArgs) -> Result<Value, ToolError> {
        use crate::commands::ctx::task;

        let slug = state::repo_slug(&self.repo);
        let cards = task::load_cards(&self.state, &slug);
        let total = cards.len();
        let mut rows: Vec<&task::Card> = cards
            .values()
            .filter(|card| match args.filter {
                CardFilter::All => true,
                CardFilter::Open => {
                    !matches!(card.state, task::State::Done | task::State::Archived)
                }
            })
            .collect();
        rows.sort_by_key(|card| std::cmp::Reverse(card.updated_at));
        let shown = args.bounded_limit().min(rows.len());
        let listed: Vec<Value> = rows[..shown]
            .iter()
            .map(|card| {
                json!({
                    "task": card.id,
                    "title": card.title,
                    "state": card.state.to_string(),
                    "parents": card.parents,
                    "group": card.group_id,
                    "claimed_by": card.claim.as_ref().map(|claim| claim.session.clone()),
                })
            })
            .collect();
        Ok(json!({"total": total, "matched": rows.len(), "tasks": listed}))
    }

    pub(super) fn group_create(&self, args: &GroupCreateArgs) -> Result<Value, ToolError> {
        use crate::commands::ctx::group;

        let identity = self.broker.identity().clone();
        let mut sink: Vec<u8> = Vec::new();
        let id = group::run_create(
            &self.state,
            &mut sink,
            &group::CreateArgs {
                scope: args.scope.clone(),
                child_limit: args.child_limit.unwrap_or(group::DEFAULT_CHILD_LIMIT),
                token_budget: args.token_budget,
                deadline_secs: args.deadline_secs,
                completion_contract: args
                    .completion_contract
                    .clone()
                    .unwrap_or_else(|| group::DEFAULT_COMPLETION_CONTRACT.to_string()),
                parent_session: Some(identity.session.clone()),
            },
            state::now_secs(),
        )
        .map_err(ToolError::external)?;
        Ok(json!({
            "group": id,
            "scope": args.scope,
            "child_limit": args.child_limit.unwrap_or(group::DEFAULT_CHILD_LIMIT),
        }))
    }

    pub(super) fn group_status(&self, id: Option<&str>) -> Result<Value, ToolError> {
        use crate::commands::ctx::group;

        let render = |g: &group::WorkGroup| {
            let cards: Vec<Value> = group::cards_for_group(&self.state, &g.work_group_id)
                .into_iter()
                .map(|card| json!({"task": card.id, "state": card.state.to_string()}))
                .collect();
            json!({
                "group": g.work_group_id,
                "scope": g.scope,
                "status": if g.closed_at.is_some() { "closed" } else { "open" },
                "child_limit": g.child_limit,
                "admitted_children": g.admitted_children,
                "token_budget": g.token_budget,
                "spent_tokens": g.spent_tokens,
                "reserved_tokens": g.reserved_tokens,
                "overdue": group::is_overdue(g, state::now_secs()),
                "completion_contract": g.completion_contract,
                "tasks": cards,
            })
        };
        match id {
            Some(id) => match group::load(&self.state, id).map_err(ToolError::external)? {
                Some(group) => Ok(render(&group)),
                None => Err(ToolError::new(
                    ToolErrorCode::PreconditionFailed,
                    format!("no work group {id:?}"),
                )),
            },
            None => Ok(json!({
                "groups": group::list(&self.state).iter().map(render).collect::<Vec<Value>>(),
            })),
        }
    }

    pub(super) fn objective_status(&self) -> Result<Value, ToolError> {
        use crate::commands::ctx::{coordinator, objective};

        let graph = coordinator::load(&self.state, &self.repo);
        let record = objective::load(&self.state, &state::repo_slug(&self.repo))
            .map_err(ToolError::external)?;
        Ok(match record {
            Some(record) => json!({
                "objective": record.objective,
                "status": format!("{:?}", record.status).to_lowercase(),
                "budget_tokens": record.budget_tokens,
                "spent_tokens": record.spent_tokens,
                "deadline_secs": record.deadline_secs,
                "constraints": graph.constraints,
                "stopped": graph.cancelled,
            }),
            None => json!({
                "objective": Value::Null,
                "constraints": graph.constraints,
                "stopped": graph.cancelled,
                "note": "no objective is set for this repository",
            }),
        })
    }

    pub(super) fn team_status(&self) -> Result<Value, ToolError> {
        use crate::commands::ctx::coordinator;

        let graph = coordinator::load(&self.state, &self.repo);
        let pending = coordinator::pending(&self.state, &self.repo, &graph);
        let nodes: Vec<Value> = graph
            .nodes
            .values()
            .map(|node| {
                json!({
                    "task": node.task,
                    "role": node.role,
                    "runtime": node.runtime,
                    "delegation": node.delegation,
                    "parents": node.parents,
                    "state": node.state.as_str(),
                    "evidence": node.evidence,
                })
            })
            .collect();
        let decisions: Vec<&str> = graph
            .decisions
            .iter()
            .rev()
            .take(20)
            .map(|decision| decision.what.as_str())
            .collect();
        let outstanding: Vec<&str> = graph
            .outstanding()
            .iter()
            .map(|node| node.task.as_str())
            .collect();
        // Which roles this machine can actually staff. A coordinator that
        // plans around a role with no configured route is planning work
        // nothing can take.
        let roster: Vec<Value> = self
            .native_config()
            .map(|native| {
                crate::commands::ctx::team::roster(&native)
                    .into_iter()
                    .map(|(role, route)| {
                        json!({"role": role.as_str(), "route": route.map(|id| id.to_string())})
                    })
                    .collect()
            })
            .unwrap_or_default();
        Ok(json!({
            "objective": graph.objective,
            "constraints": graph.constraints,
            "stopped": graph.cancelled,
            "nodes": nodes,
            "outstanding": outstanding,
            "roster": roster,
            "pending_completions": pending,
            "recent_decisions": decisions,
            "note": "a node stays `delegated` until its worker's receipt is consumed; an \
                     unconsumed outcome is listed under pending_completions rather than assumed",
        }))
    }

    /// `team_plan` (issue #541 chunk C, decision 1): runs the SAME chunk B
    /// compiler `zirv workflow team plan` runs -- classification from the
    /// repo plus the objective text, the minimal execution profile, then
    /// `compile`/`compile_explicit` -- and persists it: the active workflow
    /// OWNS the plan when one exists for this repository, and the
    /// coordinator record keeps only a reference to it; with no active
    /// workflow, the coordinator record holds the plan itself.
    pub(super) fn team_plan(&mut self, args: &TeamPlanArgs) -> Result<Value, ToolError> {
        use crate::commands::workflow::team;

        // Issue #541 chunk C review finding: `team_plan` writes the SAME
        // plan `coordinator::check` later enforces every delegation
        // against, so any writable seat that could call it could silently
        // replace a coordinator's compiled plan -- dropping independent
        // review/test seats and hollowing out that whole enforcement.
        // Restricted to the two roles `team::Authority::may_delegate` is
        // true for (`Coordinator`/`SubOrchestrator`) -- the SAME authority
        // table `coordinator::check` itself reads, so this gate cannot
        // silently drift from the delegation bounds it protects. Gates the
        // WHOLE call, `--seat` explicit compiles included: they reach the
        // identical `store_plan`.
        let role = self.broker.identity().role.clone();
        if !crate::commands::ctx::team::authority(&role).may_delegate {
            return Err(ToolError::new(
                ToolErrorCode::AuthorizationDenied,
                format!(
                    "a `{role}` seat may not compile or store a team plan: team_plan is \
                     restricted to coordinator/sub-orchestrator seats"
                ),
            ));
        }

        // Matches `zirv workflow team plan`'s own resolution
        // (`workflow::team::run_plan`) so the native tool and the wrapped-
        // harness CLI agree on which operator/repository manifests apply.
        let home = dirs::home_dir();
        let mut plan = team::compile_for_objective(
            &self.repo,
            home.as_deref(),
            &args.objective,
            args.seat.as_deref(),
        )
        .map_err(ToolError::external)?;
        if args.seat.is_some()
            && let Some(task) = &args.task
            && let Some(seat) = plan.seats.first_mut()
        {
            seat.task = task.clone();
        }
        team::store_plan(&self.state, &self.repo, &plan).map_err(ToolError::external)?;
        serde_json::to_value(&plan).map_err(ToolError::external)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::ctx::runtime::tools::delegation::{DELEGATE, RESULT};
    use crate::commands::ctx::runtime::tools::tests::*;
    use crate::commands::ctx::runtime::tools::*;

    #[test]
    fn team_plan_needs_a_non_empty_objective_and_nothing_else_is_accepted() {
        let ok: TeamPlanArgs =
            serde_json::from_str(r#"{"objective":"ship the export feature"}"#).expect("parse");
        ok.validate().expect("valid");
        assert_eq!(ok.seat, None);

        let with_seat: TeamPlanArgs =
            serde_json::from_str(r#"{"objective":"x","seat":"reviewer","task":"review the diff"}"#)
                .expect("parse");
        with_seat.validate().expect("valid");

        let blank: TeamPlanArgs = serde_json::from_str(r#"{"objective":"  "}"#).expect("parse");
        assert!(blank.validate().is_err());

        assert!(
            serde_json::from_str::<TeamPlanArgs>(r#"{"objective":"x","surprise":true}"#).is_err()
        );
    }

    #[test]
    fn the_tool_name_list_has_no_duplicates() {
        let unique: std::collections::BTreeSet<&str> = ALL.into_iter().collect();
        assert_eq!(unique.len(), ALL.len());
    }

    #[test]
    fn a_card_needs_a_title_and_a_brief_and_nothing_else_is_accepted() {
        let ok: TaskCreateArgs =
            serde_json::from_str(r#"{"title":"port N16","brief":"do it","role":"implementer"}"#)
                .expect("parse");
        ok.validate().expect("valid");
        assert_eq!(ok.role.as_deref(), Some("implementer"));
        assert!(ok.parents.is_empty());

        let blank: TaskCreateArgs =
            serde_json::from_str(r#"{"title":"  ","brief":"x"}"#).expect("parse");
        assert!(blank.validate().is_err());

        // A key the schema does not declare is refused rather than ignored:
        // a model must not be able to smuggle a `state` or `claim` field past
        // the task service.
        assert!(
            serde_json::from_str::<TaskCreateArgs>(r#"{"title":"a","brief":"b","state":"done"}"#)
                .is_err()
        );
    }

    #[test]
    fn an_id_that_could_name_a_file_is_refused_at_the_boundary() {
        for bad in ["../escape", "a/b", "", "with space"] {
            assert!(validate_id(bad, "group").is_err(), "{bad:?}");
        }
        validate_id("wg-1234", "group").expect("plain ids are fine");
    }

    #[test]
    fn a_listing_is_always_bounded() {
        let huge: TaskListArgs = serde_json::from_str(r#"{"limit":100000}"#).expect("parse");
        assert_eq!(huge.bounded_limit(), MAX_LIST_LIMIT);
        let zero: TaskListArgs = serde_json::from_str(r#"{"limit":0}"#).expect("parse");
        assert_eq!(zero.bounded_limit(), 1);
        let default: TaskListArgs = serde_json::from_str("{}").expect("parse");
        assert_eq!(default.bounded_limit(), DEFAULT_LIST_LIMIT);
        assert_eq!(default.filter, CardFilter::Open);
    }

    #[test]
    fn a_group_that_admits_nobody_is_refused() {
        let zero: GroupCreateArgs =
            serde_json::from_str(r#"{"scope":"x","child_limit":0}"#).expect("parse");
        assert!(zero.validate().is_err());
        let ok: GroupCreateArgs = serde_json::from_str(r#"{"scope":"x"}"#).expect("parse");
        ok.validate().expect("valid");
        assert_eq!(ok.child_limit, None);
    }

    #[test]
    fn every_team_tool_is_registered_with_a_closed_schema_and_a_typed_scope() {
        let registry = ToolRegistry::native();
        for name in ALL {
            let definition = registry
                .get(name)
                .unwrap_or_else(|| panic!("missing {name}"));
            assert_eq!(definition.input_schema["type"], "object");
            assert_eq!(definition.input_schema["additionalProperties"], false);
            assert!(!definition.capabilities.is_empty());
        }
        // The three that MUTATE shared state declare the write capability;
        // the four reads do not, so a read-only seat still gets them.
        for name in [TASK_CREATE, TASK_CLAIM, GROUP_CREATE] {
            assert!(
                registry
                    .get(name)
                    .expect(name)
                    .capabilities
                    .iter()
                    .any(|capability| capability == "repo_fs_write"),
                "{name} mutates shared state and has to declare a write"
            );
        }
        for name in [TASK_LIST, GROUP_STATUS, OBJECTIVE_STATUS, TEAM_STATUS] {
            assert!(
                !registry
                    .get(name)
                    .expect(name)
                    .capabilities
                    .iter()
                    .any(|capability| capability == "repo_fs_write"),
                "{name} only reads"
            );
        }
    }

    /// Acceptance criterion 1 and 2, driven through the REAL registry, broker
    /// and services with only the worker launch replaced: a native
    /// coordinator plans a feature, dispatches a native implementer and a
    /// wrapped (harness) reviewer against the same shared cards and group,
    /// and reads one consistent board back.
    #[test]
    fn a_native_coordinator_runs_a_mixed_team_through_the_shared_services() {
        let mut fixture = fixture_with(0, "coordinator", true);

        let group = result_of(&call(
            &mut fixture.client,
            GROUP_CREATE,
            json!({"scope":"ship N16","child_limit":4}),
        ))["group"]
            .as_str()
            .expect("group id")
            .to_string();

        let implement = result_of(&call(
            &mut fixture.client,
            TASK_CREATE,
            json!({
                "title":"implement the coordinator",
                "brief":"write ctx::coordinator",
                "role":"implementer",
                "group": group,
            }),
        ))["task"]
            .as_str()
            .expect("task id")
            .to_string();
        let review = result_of(&call(
            &mut fixture.client,
            TASK_CREATE,
            json!({
                "title":"review the coordinator",
                "brief":"read the diff",
                "role":"reviewer",
                "parents":[implement.clone()],
                "group": group,
            }),
        ))["task"]
            .as_str()
            .expect("task id")
            .to_string();

        // One native worker and one wrapped worker, on the same cards.
        let native = handle_from(call(
            &mut fixture.client,
            DELEGATE,
            json!({"brief":"implement","role":"implementer","task":implement,"group":group}),
        ));
        let wrapped = handle_from(call(
            &mut fixture.client,
            DELEGATE,
            json!({
                "brief":"review","role":"reviewer","task":review,"group":group,
                "runtime":"harness","target":"claude"
            }),
        ));
        assert_ne!(native, wrapped);

        let launches = fixture.launches.lock().expect("lock");
        assert_eq!(launches.len(), 2);
        assert_eq!(
            launches[0].runtime,
            super::super::super::RuntimeKind::Native
        );
        assert_eq!(
            launches[1].runtime,
            super::super::super::RuntimeKind::Harness
        );
        assert!(
            !launches[0].read_only && launches[1].read_only,
            "the implementer writes and the reviewer does not, whichever runtime each ran on"
        );
        drop(launches);

        // One board, both runtimes on it.
        let board = call(&mut fixture.client, TEAM_STATUS, json!({}));
        let board = result_of(&board);
        // The graph is keyed by card id, and a card id is a uuid, so the
        // assertion is about the SET of runtimes on one board, not an order.
        let runtimes: std::collections::BTreeSet<&str> = board["nodes"]
            .as_array()
            .expect("nodes")
            .iter()
            .filter_map(|node| node["runtime"].as_str())
            .collect();
        assert_eq!(
            runtimes,
            ["harness", "native"].into_iter().collect(),
            "one board carries both runtimes: {board}"
        );
        assert_eq!(
            board["pending_completions"]
                .as_array()
                .expect("pending")
                .len(),
            2,
            "both outcomes are published and neither is consumed yet -- unknown stays unknown"
        );

        let cards = call(&mut fixture.client, TASK_LIST, json!({}));
        assert_eq!(result_of(&cards)["total"], 2);
        let status = call(&mut fixture.client, GROUP_STATUS, json!({"group": group}));
        assert_eq!(
            result_of(&status)["tasks"].as_array().expect("tasks").len(),
            2
        );
        assert_eq!(result_of(&status)["scope"], "ship N16");
    }

    /// Issue #541 chunk C, decision 1: `team_plan` runs the same compiler
    /// `zirv workflow team plan` runs and persists the result -- on the
    /// coordinator record when (as here) no workflow is active -- and the
    /// stored plan agrees byte-for-byte with what the tool returned.
    #[test]
    fn team_plan_tool_stores_the_plan_and_returns_it() {
        let home = tempfile::tempdir().expect("home");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());
        let mut fixture = fixture_with(0, "coordinator", true);
        // `classify::from_args` measures the repository's own git history
        // when no explicit `--path`/`--changed-lines` is given -- which is
        // exactly how the native tool calls it -- so the fixture repo needs
        // at least one commit to measure against.
        std::fs::write(fixture.repo.join("README.md"), "hello\n").expect("seed file");
        let git = |args: &[&str]| {
            let status = std::process::Command::new("git")
                .args([
                    "-c",
                    "user.email=t@example.com",
                    "-c",
                    "user.name=t",
                    "-c",
                    "commit.gpgsign=false",
                ])
                .args(args)
                .current_dir(&fixture.repo)
                .status()
                .expect("git");
            assert!(status.success(), "git {args:?}");
        };
        git(&["init", "--quiet"]);
        git(&["add", "."]);
        git(&["commit", "-q", "-m", "init"]);

        let receipt = call(
            &mut fixture.client,
            TEAM_PLAN,
            json!({"objective": "fix the null pointer crash in the parser"}),
        );
        assert_eq!(receipt.state, ToolReceiptState::Completed, "{receipt:?}");
        let plan = receipt.result.expect("plan");
        assert_eq!(
            plan["objective"],
            "fix the null pointer crash in the parser"
        );
        assert!(plan["seats"].is_array());

        let graph = crate::commands::ctx::coordinator::load(&fixture.state, &fixture.repo);
        assert!(
            graph.team_plan.is_some(),
            "the plan is stored on the coordinator record (no active workflow in this fixture)"
        );
        let resolved = crate::commands::ctx::coordinator::resolve_team_plan(
            &fixture.state,
            &fixture.repo,
            &graph,
        )
        .expect("resolved plan");
        assert_eq!(
            serde_json::to_value(&resolved).expect("json"),
            plan,
            "the stored plan is exactly what the tool returned"
        );
    }

    /// Builds a two-seat plan (an implementer and an independent reviewer)
    /// straight from the real `compile_explicit` path -- so every field
    /// (authority, route tier, required capabilities) is what the compiler
    /// itself would produce -- rather than depending on a real git diff to
    /// drive the proportional compiler to a particular shape.
    fn two_seat_mixed_plan(repo: &Path) -> crate::commands::workflow::team::TeamPlan {
        use crate::commands::workflow::agents::AgentRegistry;
        use crate::commands::workflow::classify::{
            Classification, Complexity, DomainClassification, Intent, RiskBand, RiskMeasurement,
        };
        use crate::commands::workflow::profile::ExecutionProfile;
        use crate::commands::workflow::skill::SkillRegistry;
        use crate::commands::workflow::team;

        let classification = Classification {
            intent: Intent::Feature,
            complexity: Complexity::Trivial,
            risk: RiskBand::Low,
            risk_score: 0,
            changed_files: 1,
            changed_lines: 5,
            changed_paths: Vec::new(),
            declared_scope: false,
            work_domain: DomainClassification::default(),
            risk_measurement: RiskMeasurement::Measured,
            reasons: vec!["test fixture".to_string()],
        };
        let profile = ExecutionProfile::derive("mixed-runtime dispatch", &classification);
        let registry = AgentRegistry::load(repo, None, false, false).expect("registry");
        let skills = SkillRegistry::load(repo, None, false, false).expect("skills");
        let always_eligible = |_role: crate::commands::ctx::team::TeamRole| Ok(());

        let mut plan = team::compile_explicit(
            "implement the change",
            &profile,
            &registry,
            &skills,
            &always_eligible,
            "implementer",
        )
        .expect("implementer seat compiles");
        let mut implementer_seat = plan.seats.remove(0);
        implementer_seat.id = "implementer-1".to_string();

        let reviewer_plan = team::compile_explicit(
            "review the change",
            &profile,
            &registry,
            &skills,
            &always_eligible,
            "reviewer",
        )
        .expect("reviewer seat compiles");
        let mut reviewer_seat = reviewer_plan
            .seats
            .into_iter()
            .next()
            .expect("reviewer seat");
        reviewer_seat.id = "reviewer-1".to_string();
        // `compile_explicit` gives every explicit seat the same "primary"
        // claim regardless of role; an independent reviewer holds none, the
        // same invariant the proportional compiler enforces.
        reviewer_seat.claim.paths.clear();
        reviewer_seat.claim.worktree = false;

        plan.seats = vec![implementer_seat, reviewer_seat];
        plan
    }

    /// Issue #541 chunk C, decision 5: a wrapped-harness worker seat
    /// dispatched from a native coordinator goes through the SAME
    /// manifest/team-plan checks a native seat does -- the mixed-team test
    /// pattern, now against a stored plan.
    #[test]
    fn a_mixed_team_still_dispatches_through_the_plan_checks() {
        let mut fixture = fixture_with(0, "coordinator", true);
        let plan = two_seat_mixed_plan(&fixture.repo);
        crate::commands::ctx::coordinator::update(&fixture.state, &fixture.repo, |graph| {
            graph.store_team_plan_inline(plan, state::now_secs());
        })
        .expect("store plan");

        let native = handle_from(call(
            &mut fixture.client,
            DELEGATE,
            json!({
                "brief": "implement",
                "role": "implementer",
                "task": "implementer-1",
                "runtime": "native",
            }),
        ));
        let wrapped = handle_from(call(
            &mut fixture.client,
            DELEGATE,
            json!({
                "brief": "review",
                "role": "reviewer",
                "task": "reviewer-1",
                "runtime": "harness",
                "target": "claude",
            }),
        ));
        assert_ne!(native, wrapped);

        let launches = fixture.launches.lock().expect("lock");
        assert_eq!(launches.len(), 2);
        assert_eq!(
            launches[0].runtime,
            super::super::super::RuntimeKind::Native
        );
        assert_eq!(
            launches[1].runtime,
            super::super::super::RuntimeKind::Harness
        );
        drop(launches);

        // A THIRD delegation for the SAME seat, while it is still filled, is
        // refused -- the plan match rule, not a legacy claim rule.
        let refused = call(
            &mut fixture.client,
            DELEGATE,
            json!({
                "brief": "implement again",
                "role": "implementer",
                "task": "implementer-1",
                "runtime": "native",
            }),
        );
        assert_eq!(refused.state, ToolReceiptState::Failed, "{refused:?}");
    }

    /// Acceptance criterion 3, the ownership half: the claim the tool takes is
    /// the SHARED one, so a second claimant is refused with the reason rather
    /// than paid to redo the first one's work.
    #[test]
    fn two_workers_can_never_claim_one_card() {
        let mut fixture = fixture_with(0, "coordinator", true);
        let task = result_of(&call(
            &mut fixture.client,
            TASK_CREATE,
            json!({"title":"one card","brief":"do it"}),
        ))["task"]
            .as_str()
            .expect("task id")
            .to_string();

        let first = call(&mut fixture.client, TASK_CLAIM, json!({"task": task}));
        assert_eq!(result_of(&first)["claimed"], true);

        // A different session, same card, through the shared task service.
        let other = crate::commands::ctx::task::claim_locked(
            &fixture.state,
            &state::repo_slug(&fixture.repo),
            &task,
            "some-other-session",
            std::process::id(),
            crate::commands::ctx::sessions::process_start_secs(std::process::id()),
            "host",
            state::now_secs(),
            crate::commands::ctx::task::DEFAULT_CLAIM_TTL_SECS,
        )
        .expect("claim")
        .expect("card exists");
        assert!(other.is_err(), "a live claim is exclusive");

        let unknown = call(
            &mut fixture.client,
            TASK_CLAIM,
            json!({"task":"task-does-not-exist"}),
        );
        assert_eq!(
            unknown.error.as_ref().map(|error| error.code.clone()),
            Some(ToolErrorCode::PreconditionFailed)
        );
    }

    /// Acceptance criterion 3, the authority half: a reviewer seat may not
    /// delegate at all, and the refusal reaches the model as an authorization
    /// denial rather than as a launch that quietly did nothing.
    #[test]
    fn a_seat_whose_role_grants_no_delegation_authority_is_refused_at_the_tool() {
        let mut fixture = fixture_with(0, "reviewer", true);
        let refused = call(
            &mut fixture.client,
            DELEGATE,
            json!({"brief":"do it","role":"implementer"}),
        );
        assert_eq!(refused.state, ToolReceiptState::Failed);
        assert!(
            refused
                .error
                .as_ref()
                .is_some_and(|error| error.message.contains("may not delegate")),
            "{refused:?}"
        );
        assert!(fixture.launches.lock().expect("lock").is_empty());
    }

    /// Issue #485 item 1, the enforcement half: reading the board needs no
    /// permit, moving a piece on it does -- decided by the broker at effect
    /// time, exactly as the workflow tools are.
    #[test]
    fn a_session_with_no_writer_permit_can_read_the_board_but_never_move_it() {
        let mut fixture = delegation_fixture(0);
        for (name, arguments) in [
            (TASK_LIST, json!({})),
            (GROUP_STATUS, json!({})),
            (OBJECTIVE_STATUS, json!({})),
            (TEAM_STATUS, json!({})),
        ] {
            let receipt = call(&mut fixture.client, name, arguments);
            assert_eq!(
                receipt.state,
                ToolReceiptState::Completed,
                "{name}: {receipt:?}"
            );
        }
        for (name, arguments) in [
            (TASK_CREATE, json!({"title":"t","brief":"b"})),
            (TASK_CLAIM, json!({"task":"task-1"})),
            (GROUP_CREATE, json!({"scope":"s"})),
        ] {
            let receipt = call(&mut fixture.client, name, arguments);
            assert_eq!(
                receipt.error.as_ref().map(|error| error.code.clone()),
                Some(ToolErrorCode::ResourceBusy),
                "{name} must be refused before the service is reached: {receipt:?}"
            );
        }
    }

    /// Item 5: the coordinator is handed a bounded manifest and a reference,
    /// never a replay -- and its own board says plainly what it does not yet
    /// know.
    #[test]
    fn a_coordinator_reads_a_bounded_result_and_keeps_unknown_honest() {
        let mut fixture = fixture_with(0, "coordinator", true);
        let handle = handle_from(call(
            &mut fixture.client,
            DELEGATE,
            json!({"brief":"implement","role":"implementer","task":"task-a"}),
        ));

        let before = call(&mut fixture.client, TEAM_STATUS, json!({}));
        let before = result_of(&before);
        assert_eq!(before["nodes"][0]["state"], "delegated");
        assert_eq!(before["pending_completions"][0]["delegation"], handle);

        let manifest = call(
            &mut fixture.client,
            RESULT,
            json!({"delegation": handle, "max_bytes": 512}),
        );
        let manifest = result_of(&manifest);
        assert_eq!(manifest["delegation"], handle);
        assert!(
            manifest["summary"].as_str().unwrap_or_default().len() <= 512,
            "a manifest is bounded, not a transcript"
        );
    }

    /// Acceptance criterion 5: the whole coordinator surface -- the shared
    /// task, group, objective, workflow and delegation services, the real
    /// broker, the real registry -- with `PATH` scrubbed EMPTY, so there is
    /// no `claude`, no `codex` and no other vendor CLI anywhere on it.
    ///
    /// The worker launch is the one seam that is stubbed, for the same reason
    /// every N10 delegation test stubs it: starting a real worker needs a
    /// provider endpoint, and this test is about whether zirv needs a coding
    /// harness, not about whether a vendor answers.
    #[test]
    fn an_all_native_team_runs_a_workflow_with_every_coding_harness_absent() {
        use crate::commands::ctx::testenv::VarGuard;
        use crate::commands::workflow::engine;

        let mut fixture = fixture_with(0, "coordinator", true);
        let _path = VarGuard::set(&[("PATH", Some(""))]);

        // A real workflow in the real store, for the repository this session
        // is seated in.
        let workflow = engine::WorkflowState::start(
            fixture.repo.clone(),
            "ship the native meta-orchestrator".into(),
            engine::WorkflowKind::Feature,
            None,
            true,
            crate::commands::workflow::classify::Classification {
                intent: crate::commands::workflow::classify::Intent::Feature,
                complexity: crate::commands::workflow::classify::Complexity::Trivial,
                risk: crate::commands::workflow::classify::RiskBand::Low,
                risk_score: 0,
                changed_files: 1,
                changed_lines: 5,
                changed_paths: Vec::new(),
                declared_scope: false,
                work_domain: Default::default(),
                risk_measurement: crate::commands::workflow::classify::RiskMeasurement::Measured,
                reasons: vec!["small".into()],
            },
        );
        engine::save(&fixture.state, &workflow, true).expect("save the workflow");

        // Plan, staff and dispatch: three roles, all native.
        let group = result_of(&call(
            &mut fixture.client,
            GROUP_CREATE,
            json!({"scope":"N16","child_limit":3}),
        ))["group"]
            .as_str()
            .expect("group")
            .to_string();
        let mut handles = Vec::new();
        for role in ["implementer", "tester", "reviewer"] {
            let task = result_of(&call(
                &mut fixture.client,
                TASK_CREATE,
                json!({"title": role, "brief":"do the thing", "role": role, "group": group}),
            ))["task"]
                .as_str()
                .expect("task")
                .to_string();
            handles.push(handle_from(call(
                &mut fixture.client,
                DELEGATE,
                json!({"brief":"do the thing","role":role,"task":task,"group":group}),
            )));
        }
        assert_eq!(handles.len(), 3);
        assert!(
            fixture
                .launches
                .lock()
                .expect("lock")
                .iter()
                .all(|launch| launch.runtime == super::super::super::RuntimeKind::Native),
            "every worker on this team is native"
        );

        // The workflow is read live through the same engine the CLI verb
        // uses -- with no harness on PATH at all.
        let status = call(&mut fixture.client, WORKFLOW_STATUS, json!({}));
        let status = result_of(&status);
        assert_eq!(status["task"], "ship the native meta-orchestrator");

        // The coordinator restarts: every receipt is consumed exactly once
        // and the board settles without anything being dispatched twice.
        let mut graph = crate::commands::ctx::coordinator::load(&fixture.state, &fixture.repo);
        let consumed = crate::commands::ctx::coordinator::consume_pending(
            &fixture.state,
            &fixture.repo,
            &mut graph,
            state::now_secs(),
        )
        .expect("consume");
        assert_eq!(consumed.len(), 3);
        crate::commands::ctx::coordinator::store(&fixture.state, &fixture.repo, &graph)
            .expect("store");

        let board = call(&mut fixture.client, TEAM_STATUS, json!({}));
        let board = result_of(&board);
        assert!(
            board["pending_completions"]
                .as_array()
                .expect("pending")
                .is_empty(),
            "every receipt has been read: {board}"
        );
        assert!(
            board["outstanding"]
                .as_array()
                .expect("outstanding")
                .is_empty(),
            "and nothing is waiting to be dispatched a second time: {board}"
        );
        let states: Vec<&str> = board["nodes"]
            .as_array()
            .expect("nodes")
            .iter()
            .filter_map(|node| node["state"].as_str())
            .collect();
        assert_eq!(states, ["completed", "completed", "completed"]);
    }

    /// Issue #609 (roadmap N22, review of #493): the fresh-install proof
    /// widened past the test above. That test proves the COORDINATOR needs
    /// no harness by substituting `RecordingLauncher` for the one seam that
    /// cannot be fixture-driven from a model-facing tool call (the delegate
    /// tool's `LaunchRequest` is deliberately never given an operator-only
    /// transport override -- see `HeadlessRequest::provider`'s own doc
    /// comment -- so a real dispatched worker cannot be driven through it
    /// deterministically without live credentials). This test closes the gap
    /// the review found: it drives the REAL production worker entry point
    /// (`runtime::native::run_session`, the exact function `zirv ctx exec
    /// --runtime native --provider fixture:<path>` calls) and the REAL
    /// helper entry point (`helper::run`) to completion, and it does so with
    /// every name `ctx::adapters::ADAPTERS` registers -- not a hard-coded
    /// `claude`/`codex` pair -- replaced by a CANARY executable that leaves
    /// evidence if the OS ever actually runs it. `PATH` naming eight
    /// look-alike executables and still completing with no canary firing is
    /// strictly stronger evidence than an empty `PATH`, which only proves a
    /// lookup would have failed, not that nothing tried to look.
    #[test]
    fn a_real_native_worker_and_every_helper_role_complete_with_every_registered_harness_canaried_and_uninvoked()
     {
        use crate::commands::ctx::helper::{self, HelperBudget, HelperRequest};
        use crate::commands::ctx::runtime::native::{HeadlessRequest, NativeLimits, NativeStatus};
        use crate::commands::ctx::state::STATE_ENV;
        use crate::commands::ctx::testenv;

        let canary_root = tempfile::tempdir().expect("canary root");
        let invoked_log = canary_root.path().join("invoked.log");
        let (_canary_dir, _path) = testenv::canary_path_for_every_registered_harness(&invoked_log);

        let repo = testenv::repo();
        let home = tempfile::tempdir().expect("home");
        let state_dir = tempfile::tempdir().expect("state");
        let _home = testenv::HomeGuard::set(home.path());
        let state_path = state_dir.path().to_string_lossy().into_owned();
        let env = move |key: &str| (key == STATE_ENV).then(|| state_path.clone());

        let script = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests")
            .join("fixtures")
            .join("runtime")
            .join("native")
            .join("helper-answer.json");
        let provider_spec = format!("fixture:{}", script.display());

        // A REAL worker: the same function a delegated native worker and a
        // plain `zirv ctx exec --runtime native` both run, not a stub.
        let mut notices: Vec<u8> = Vec::new();
        let status = crate::commands::ctx::runtime::native::run_session(
            &mut HeadlessRequest {
                repo: repo.path(),
                prompt: "implement the thing",
                route: None,
                role: "worker",
                limits: NativeLimits::default(),
                session_id: None,
                cancellation: None,
                resume: None,
                provider: Some(provider_spec.as_str()),
                fixture_tools: None,
                task: None,
                writer: None,
                accounting: crate::commands::ctx::runtime::native::Accounting::Seat,
            },
            &mut notices,
            &env,
        )
        .expect("a real native worker completes with no harness on PATH");
        assert_eq!(status.status, NativeStatus::Completed);
        assert_eq!(status.runtime, "native");

        // A helper call, widened to every role an installed team needs
        // (issue #492 item 3's own widening, reused here rather than
        // duplicated).
        for role in [
            helper::ROLE_DISTILLER,
            helper::ROLE_ASK,
            helper::ROLE_OPTIMIZE,
            helper::ROLE_SEAT,
        ] {
            let answer = helper::run(
                &HelperRequest {
                    repo: repo.path(),
                    prompt: "distill this",
                    role,
                    route: None,
                    budget: HelperBudget::one_shot(30_000),
                    provider: Some(provider_spec.as_str()),
                },
                &env,
            )
            .unwrap_or_else(|err| {
                panic!("role {role} must answer with no harness on PATH: {err:?}")
            });
            assert_eq!(answer.status, NativeStatus::Completed, "role {role}");
        }

        assert!(
            !invoked_log.exists(),
            "a registered harness executable was invoked: {}",
            std::fs::read_to_string(&invoked_log).unwrap_or_default()
        );
    }

    /// Acceptance criterion 7: the operator steers and stops the objective
    /// through the command they already have, and the coordinator sees it.
    #[test]
    fn operator_steering_and_stopping_reach_the_coordinator() {
        use crate::commands::ctx::{coordinator, objective};

        let mut fixture = fixture_with(0, "coordinator", true);
        let cfg = CtxConfig::default();
        let mut sink: Vec<u8> = Vec::new();
        objective::run_set(
            &fixture.state,
            &mut sink,
            &fixture.repo,
            &cfg,
            &objective::SetArgs {
                objective: "ship N16 without touching the release branch".to_string(),
                budget_tokens: None,
                deadline_secs: None,
            },
            10,
        )
        .expect("set");

        let seen = call(&mut fixture.client, OBJECTIVE_STATUS, json!({}));
        let seen = result_of(&seen);
        assert_eq!(
            seen["objective"],
            "ship N16 without touching the release branch"
        );
        assert_eq!(seen["stopped"], false);
        assert_eq!(
            seen["constraints"][0],
            "ship N16 without touching the release branch"
        );

        // Stopping it refuses further delegation, with the reason.
        let mut graph = coordinator::load(&fixture.state, &fixture.repo);
        graph.cancel(20);
        coordinator::store(&fixture.state, &fixture.repo, &graph).expect("store");
        let refused = call(
            &mut fixture.client,
            DELEGATE,
            json!({"brief":"more work","role":"implementer"}),
        );
        assert!(
            refused
                .error
                .as_ref()
                .is_some_and(|error| error.message.contains("cancelled")),
            "{refused:?}"
        );
        assert!(fixture.launches.lock().expect("lock").is_empty());

        // And setting a new objective lifts it, which is what "steering"
        // means: the operator redirects rather than restarts.
        objective::run_set(
            &fixture.state,
            &mut sink,
            &fixture.repo,
            &cfg,
            &objective::SetArgs {
                objective: "ship N16, release branch is fine now".to_string(),
                budget_tokens: None,
                deadline_secs: None,
            },
            30,
        )
        .expect("set again");
        let resumed = call(
            &mut fixture.client,
            DELEGATE,
            json!({"brief":"more work","role":"implementer"}),
        );
        assert_eq!(resumed.state, ToolReceiptState::Completed, "{resumed:?}");
        let seen = call(&mut fixture.client, OBJECTIVE_STATUS, json!({}));
        assert_eq!(
            result_of(&seen)["constraints"].as_array().expect("c").len(),
            2
        );
    }
}
