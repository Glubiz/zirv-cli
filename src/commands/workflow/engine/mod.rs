//! Versioned workflow definitions and durable execution state.

pub(crate) mod cli;
mod definitions;
mod lifecycle;
mod session_context;
mod state;
mod transition;

pub use cli::{StartArgs, WorkflowArgs, WorkflowSubcommand, run, start_workflow};
pub(crate) use cli::{
    detach, load_workflow_registry, resolve_repo, resolve_state, write_definition_status,
    write_registry_entry, write_registry_list, write_start_outcome, write_state,
};
pub use definitions::{
    AcceptedPreexistingFindings, ArtifactStage, StepCondition, WorkflowKind, WorkflowProfile,
    WorkflowStatus,
};
pub use lifecycle::{
    SKILL_HEADER_SENTINEL, active_skill_context, apply_recommended_dispositions, approve, close,
    close_unstarted, native_completion_gate, render_current_context, waive_first_gate,
};
pub use session_context::{
    AbandonedWorkflow, abandoned_workflows, changed_step_context, step_context_note,
};
pub(crate) use state::{
    ARTIFACT_SUBSTANCE_DEFAULT_FLOOR, ARTIFACT_SUBSTANCE_LABEL, artifact_substance_action,
    artifact_substance_questions, hash_bytes, load_active_read_only, read_accepted_artifact, save,
    save_preserving_active,
};
pub use state::{WorkflowState, load, load_active, load_active_for_session, state_mtime_secs};
pub(crate) use transition::{
    GATE_RECLASS_LABEL, GATE_RECLASS_NOUL_DEFAULT_FLOOR, GATE_RECLASS_WORK_DOMAIN_DEFAULT_FLOOR,
    gate_reclass_questions, gate_sensitive_surface_action, gate_tag_action,
    gate_work_domain_action,
};
pub use transition::{StepOutcome, advance_with_evidence};

// Reached only from other modules' own `#[cfg(test)]` code, never from this
// crate's non-test build.
#[cfg(test)]
pub(crate) use cli::auto_spawn_decision;
#[cfg(test)]
pub use definitions::WorkflowArtifactRecord;
#[cfg(test)]
pub(crate) use state::artifact_hash;

/// Test fixtures used by more than one submodule's tests.
#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::path::Path;

    use crate::commands::ctx::jev::{self, AnswerValue};
    use crate::commands::workflow::classify::{self, Classification, Complexity, Intent, RiskBand};

    use super::definitions::WorkflowStatus;
    use super::state::WorkflowState;

    pub(super) fn low_classification() -> Classification {
        Classification {
            intent: Intent::Feature,
            complexity: Complexity::Trivial,
            risk: RiskBand::Low,
            risk_score: 0,
            changed_files: 1,
            changed_lines: 5,
            changed_paths: Vec::new(),
            declared_scope: false,
            work_domain: Default::default(),
            risk_measurement: classify::RiskMeasurement::Measured,
            reasons: vec!["small".into()],
        }
    }

    pub(super) fn skip_leading_artifact_steps(mut state: WorkflowState) -> WorkflowState {
        while state.current().is_some_and(|step| step.artifact.is_some()) {
            let id = state.current().unwrap().id.clone();
            state.completed_steps.push(id);
            state.current_step += 1;
        }
        state.status = if state.current().is_some() {
            WorkflowStatus::Running
        } else {
            WorkflowStatus::Completed
        };
        state
    }

    pub(super) fn review_finding(
        id: &str,
        disposition: crate::commands::workflow::review::FindingDisposition,
        recommended: Option<crate::commands::workflow::review::FindingDisposition>,
    ) -> crate::commands::workflow::review::ReviewFinding {
        crate::commands::workflow::review::ReviewFinding {
            id: id.into(),
            severity: crate::commands::workflow::review::FindingSeverity::Major,
            summary: "summary".into(),
            path: None,
            line: None,
            disposition,
            recommended_disposition: recommended,
            advisory_disposition: None,
            advisory_confidence: None,
            duplicate_of: None,
            created_at: 0,
        }
    }

    pub(super) fn jev_gate_config(
        base_url: String,
        credential_env: &str,
    ) -> crate::commands::ctx::config::CtxConfig {
        let mut cfg = crate::commands::ctx::config::CtxConfig::default();
        cfg.jev.gates = true;
        cfg.proxy.typesafe.base_url = base_url;
        cfg.proxy.typesafe.credential_env = credential_env.to_string();
        cfg
    }

    /// Shared by `definitions::tests` (its many end-to-end fixtures) and
    /// `cli::tests` (an F6 blind-review regression on `start_workflow`):
    /// initializes a bare git repo with one commit, so callers can assert on
    /// a clean `git status --porcelain` afterward.
    pub(super) fn git_init_with_commit(repo: &Path) {
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

    /// Shared by `state::tests` (`artifact_substance_action`'s own edge test)
    /// and `transition::tests` (`gate_work_domain_action`'s own edge test):
    /// a decisive `Choice` answer at the given confidence.
    pub(super) fn choice_answer(choice: &str, confidence: f32) -> jev::Answer {
        jev::Answer {
            value: AnswerValue::Choice(choice.to_string()),
            confidence,
            probabilities: BTreeMap::from([
                (choice.to_string(), 0.95_f32),
                ("other".to_string(), 0.05_f32),
            ]),
        }
    }
}
