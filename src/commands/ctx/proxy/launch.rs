//! T2 (issue #537): starts and, on a failed spawn, closes the workflow a
//! harness-proxy decision names, immediately before a wrapped-harness or
//! dashboard-pane launch actually spawns. Shared by `chat.rs`'s wrapped-
//! harness apply path and the native runtime's own guarded call.
//!
//! Reuses `workflow::engine`'s own `start_workflow`/`close`/`load_active`
//! rather than reimplementing any state handling: "what starting a workflow
//! means" has exactly one implementation in this crate (see `engine::
//! start_workflow`'s own doc comment), and this module is one more caller of
//! it, not a second one.

use std::path::Path;

use crate::commands::ctx::state::StateDir;
use crate::commands::workflow::engine::{self, StartArgs};

use super::decision::ProxyDecision;

/// Whether `start_workflow_for` actually started a workflow for this launch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorkflowStart {
    Started { id: String },
    Skipped { reason: String },
}

/// Starts `decision.workflow` (when named) via `engine::start_workflow`,
/// immediately before a spawn. `Skipped` -- never an error -- when `decision.
/// workflow` is `None`, or when `repo` already has an active workflow (an
/// operator's own, or one an earlier launch started): the proxy only ever
/// fills an empty slot, never displaces one already running. Any other
/// failure (an unreadable state directory, a capability preflight refusal,
/// an unknown registry id the decision named) is `Err(String)`; this
/// function never panics.
pub fn start_workflow_for(
    decision: &ProxyDecision,
    state_dir: &Path,
    repo: &Path,
    request: &str,
) -> Result<WorkflowStart, String> {
    let Some(kind) = decision.workflow.clone() else {
        return Ok(WorkflowStart::Skipped {
            reason: "no workflow named by this decision".to_string(),
        });
    };
    let state = StateDir::from_path(state_dir.to_path_buf());
    if let Some(active) = engine::load_active(&state, repo).map_err(|error| error.to_string())? {
        return Ok(WorkflowStart::Skipped {
            reason: format!("repo already has an active workflow '{}'", active.id),
        });
    }
    let args = StartArgs {
        id: Some(kind),
        task: request.to_string(),
        agent: None,
        built_in_only: false,
        repo: Some(repo.to_path_buf()),
        paths: Vec::new(),
        changed_lines: None,
        tests_changed: false,
        complexity: Some(decision.complexity),
        risk: Some(decision.risk),
        branch: None,
        frontend_root: None,
        brainstorm: false,
        no_brainstorm: false,
        profile: None,
        json: false,
    };
    let outcome = engine::start_workflow(&state, &args).map_err(|error| error.to_string())?;
    Ok(WorkflowStart::Started {
        id: outcome.state.id,
    })
}

/// Closes a workflow this launch started, when the spawn that followed it
/// failed -- so a failed launch never leaves an orphaned workflow reported
/// as this repository's active one forever (`engine::close`'s own doc
/// comment names exactly this hazard). Tries `engine::close` first; when it
/// refuses specifically because the workflow is still `AwaitingApproval` --
/// the COMMON case here, not an edge one: `packs/feature.toml`/`packs/
/// bugfix.toml` gate their first step behind `approval = true` for anything
/// bounded-or-riskier, so a proxy-started workflow is `AwaitingApproval` the
/// instant it starts -- falls back to `engine::close_unstarted`, which only
/// succeeds while nothing has actually progressed yet. `Err` only when both
/// refuse (a human has since advanced or approved it, or it is already
/// terminal), so the caller still learns it needs `zirv workflow close`
/// run by hand.
pub fn close_started(state_dir: &Path, repo: &Path, id: &str, reason: &str) -> Result<(), String> {
    let state = StateDir::from_path(state_dir.to_path_buf());
    let workflow = engine::load(&state, repo, id).map_err(|error| error.to_string())?;
    let unstarted_fallback = workflow.clone();
    match engine::close(&state, workflow, Some(reason.to_string())) {
        Ok(_) => Ok(()),
        Err(error) if error.to_string().contains("awaiting approval") => {
            engine::close_unstarted(&state, unstarted_fallback, Some(reason.to_string()))
                .map(|_| ())
                .map_err(|error| error.to_string())
        }
        Err(error) => Err(error.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::path::{Path, PathBuf};

    use tempfile::tempdir;

    use super::*;
    use crate::commands::ctx::catalogue::Tier;
    use crate::commands::ctx::proxy::decision::{Decider, Seat, SeatRole, SeatTier};
    use crate::commands::ctx::state::StateDir as CtxStateDir;
    use crate::commands::workflow::classify::{Complexity, Intent, RiskBand};
    use crate::commands::workflow::profile::{ExecutionMode, ValidationProfile};

    fn git_init_with_commit(repo: &Path) {
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
                .current_dir(repo)
                .status()
                .expect("run git");
            assert!(status.success(), "git {args:?} failed");
        };
        git(&["init", "-q"]);
        std::fs::write(repo.join("README.md"), "hello\n").expect("write");
        git(&["add", "."]);
        git(&["commit", "-q", "-m", "base"]);
    }

    fn decision(
        repo: &Path,
        workflow: Option<&str>,
        complexity: Complexity,
        risk: RiskBand,
    ) -> ProxyDecision {
        ProxyDecision {
            request_sha256: "deadbeef".to_string(),
            repo: repo.to_path_buf(),
            intent: Intent::Feature,
            complexity,
            risk,
            execution: ExecutionMode::Bounded,
            seat_role: SeatRole::Single,
            validation: ValidationProfile::default(),
            workflow: workflow.map(str::to_string),
            orchestrator: Seat {
                harness: "claude".to_string(),
                model: "fable".to_string(),
            },
            seat_tier: SeatTier::Standard,
            worker_tier: Tier::Standard,
            needs_clarification: 0.0,
            needs_clarification_decisive: false,
            domains: Vec::new(),
            decider: Decider::Deterministic,
            confidence: BTreeMap::new(),
            reasons: Vec::new(),
            fallbacks: Vec::new(),
            elapsed_ms: 0,
            usage: None,
            created_at: 0,
        }
    }

    #[test]
    fn no_workflow_named_is_skipped_with_a_reason() {
        let repo = tempdir().unwrap();
        let state_dir = tempdir().unwrap();
        let decision = decision(repo.path(), None, Complexity::Bounded, RiskBand::Medium);

        let outcome = start_workflow_for(&decision, state_dir.path(), repo.path(), "fix the typo")
            .expect("never errors");

        assert_eq!(
            outcome,
            WorkflowStart::Skipped {
                reason: "no workflow named by this decision".to_string()
            }
        );
    }

    #[test]
    fn an_already_active_workflow_is_skipped_naming_its_id() {
        let repo = tempdir().unwrap();
        git_init_with_commit(repo.path());
        let state_dir = tempdir().unwrap();
        let state = CtxStateDir::from_path(state_dir.path().to_path_buf());

        let existing = engine::start_workflow(
            &state,
            &StartArgs {
                id: Some("bugfix".to_string()),
                task: "an earlier launch's workflow".to_string(),
                agent: None,
                built_in_only: true,
                repo: Some(repo.path().to_path_buf()),
                paths: vec![PathBuf::from("README.md")],
                changed_lines: Some(1),
                tests_changed: false,
                complexity: None,
                risk: None,
                branch: None,
                frontend_root: None,
                brainstorm: false,
                no_brainstorm: false,
                profile: None,
                json: false,
            },
        )
        .expect("seed an active workflow");

        let decision = decision(
            repo.path(),
            Some("feature"),
            Complexity::Bounded,
            RiskBand::Medium,
        );
        let outcome = start_workflow_for(&decision, state_dir.path(), repo.path(), "do more work")
            .expect("never errors");

        assert_eq!(
            outcome,
            WorkflowStart::Skipped {
                reason: format!(
                    "repo already has an active workflow '{}'",
                    existing.state.id
                )
            }
        );
    }

    #[test]
    fn happy_path_starts_and_persists_a_workflow() {
        let repo = tempdir().unwrap();
        git_init_with_commit(repo.path());
        let state_dir = tempdir().unwrap();
        let decision = decision(
            repo.path(),
            Some("bugfix"),
            Complexity::Bounded,
            RiskBand::Medium,
        );

        let outcome = start_workflow_for(
            &decision,
            state_dir.path(),
            repo.path(),
            "fix a database retry bug",
        )
        .expect("starts cleanly");

        let WorkflowStart::Started { id } = outcome else {
            panic!("expected Started, got {outcome:?}");
        };

        let state = CtxStateDir::from_path(state_dir.path().to_path_buf());
        let active = engine::load_active(&state, repo.path())
            .expect("readable")
            .expect("an active workflow is now on record");
        assert_eq!(
            active.id, id,
            "the started workflow is this repo's active one"
        );
        assert_eq!(active.classification.complexity, Complexity::Bounded);
        assert_eq!(active.classification.risk, RiskBand::Medium);
    }

    #[test]
    fn close_started_marks_the_workflow_closed() {
        let repo = tempdir().unwrap();
        git_init_with_commit(repo.path());
        let state_dir = tempdir().unwrap();
        let decision = decision(
            repo.path(),
            Some("bugfix"),
            Complexity::Trivial,
            RiskBand::Low,
        );

        let outcome = start_workflow_for(
            &decision,
            state_dir.path(),
            repo.path(),
            "fix a database retry bug",
        )
        .expect("starts cleanly");
        let WorkflowStart::Started { id } = outcome else {
            panic!("expected Started, got {outcome:?}");
        };

        close_started(state_dir.path(), repo.path(), &id, "proxy launch failed")
            .expect("closes cleanly");

        let state = CtxStateDir::from_path(state_dir.path().to_path_buf());
        let closed = engine::load(&state, repo.path(), &id).expect("still on disk");
        assert_eq!(closed.status, engine::WorkflowStatus::Closed);
        assert_eq!(closed.closed_reason.as_deref(), Some("proxy launch failed"));
        assert!(
            engine::load_active(&state, repo.path())
                .expect("readable")
                .is_none(),
            "closing the just-started workflow must clear the active pointer"
        );
    }

    /// Issue #537 review: the COMMON case, not an edge one -- `bugfix`'s own
    /// pack gates its first step behind `approval = true` at Bounded/Medium,
    /// so a proxy-started workflow at that classification is `AwaitingApproval`
    /// immediately. `engine::close` alone would refuse here; `close_started`
    /// must still succeed via `engine::close_unstarted`, since nothing has
    /// progressed past that first gate yet.
    #[test]
    fn close_started_falls_back_to_close_unstarted_when_awaiting_approval() {
        let repo = tempdir().unwrap();
        git_init_with_commit(repo.path());
        let state_dir = tempdir().unwrap();
        let decision = decision(
            repo.path(),
            Some("bugfix"),
            Complexity::Bounded,
            RiskBand::Medium,
        );

        let outcome = start_workflow_for(
            &decision,
            state_dir.path(),
            repo.path(),
            "fix a database retry bug",
        )
        .expect("starts cleanly");
        let WorkflowStart::Started { id } = outcome else {
            panic!("expected Started, got {outcome:?}");
        };

        let state = CtxStateDir::from_path(state_dir.path().to_path_buf());
        let started = engine::load(&state, repo.path(), &id).expect("on disk");
        assert_eq!(
            started.status,
            engine::WorkflowStatus::AwaitingApproval,
            "bounded/medium must gate the first step behind approval"
        );

        close_started(state_dir.path(), repo.path(), &id, "proxy launch failed")
            .expect("closes via the close_unstarted fallback");

        let closed = engine::load(&state, repo.path(), &id).expect("still on disk");
        assert_eq!(closed.status, engine::WorkflowStatus::Closed);
        assert_eq!(closed.closed_reason.as_deref(), Some("proxy launch failed"));
        assert!(
            engine::load_active(&state, repo.path())
                .expect("readable")
                .is_none(),
            "closing via the fallback must also clear the active pointer"
        );
    }
}
