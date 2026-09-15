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

use serde::Deserialize;

use super::{ToolError, ToolErrorCode};

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
        non_empty(&self.objective, "objective")?;
        if let Some(seat) = &self.seat {
            non_empty(seat, "seat")?;
        }
        if let Some(task) = &self.task {
            non_empty(task, "task")?;
        }
        Ok(())
    }
}

fn non_empty(value: &str, field: &str) -> Result<(), ToolError> {
    if value.trim().is_empty() {
        return Err(ToolError::new(
            ToolErrorCode::InvalidArguments,
            format!("{field} must not be empty"),
        ));
    }
    Ok(())
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
        non_empty(&self.title, "title")?;
        non_empty(&self.brief, "brief")?;
        for parent in &self.parents {
            validate_id(parent, "parents[]")?;
        }
        if let Some(group) = &self.group {
            validate_id(group, "group")?;
        }
        if let Some(role) = &self.role {
            non_empty(role, "role")?;
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
        non_empty(&self.scope, "scope")?;
        if self.child_limit == Some(0) {
            return Err(ToolError::new(
                ToolErrorCode::InvalidArguments,
                "child_limit must be at least 1; a group that admits nobody is not a group",
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
