//! Typed arguments for the native workflow tools (issue #484, roadmap N15).
//!
//! A native session drives the SAME workflow engine a `zirv workflow ...`
//! command does, over the same durable state, through the same gates. These
//! are thin argument shapes in front of `workflow::engine`; none of them
//! contains workflow logic of its own, exactly the way N10's delegation tools
//! are thin over `ctx::delegation`. A second implementation of "what advances
//! a step" is a second definition of "done", and the two would drift.
//!
//! `workflow_advance` and `workflow_approve` are shared-scope knowledge WRITES,
//! so the execution broker requires a live writer permit for the session's own
//! worktree. A read-only helper or reviewer therefore cannot advance the
//! workflow it is reviewing -- enforced at effect time, not by the prompt.

use serde::Deserialize;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct WorkflowLookupArgs {
    /// The workflow id. Absent means the repository's active workflow, the
    /// same resolution `zirv workflow status` performs.
    #[serde(default)]
    pub id: Option<String>,
}

#[derive(Debug, Deserialize, PartialEq, Eq, Clone, Copy)]
#[serde(rename_all = "snake_case")]
pub(super) enum StepOutcomeArg {
    Success,
    Failure,
}

impl StepOutcomeArg {
    pub(super) fn outcome(self) -> crate::commands::workflow::engine::StepOutcome {
        match self {
            Self::Success => crate::commands::workflow::engine::StepOutcome::Success,
            Self::Failure => crate::commands::workflow::engine::StepOutcome::Failure,
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct WorkflowAdvanceArgs {
    #[serde(default)]
    pub id: Option<String>,
    pub outcome: StepOutcomeArg,
    /// Optional free-text note recorded with the transition. Never used as a
    /// gate: evidence is what the verification store holds, not what a model
    /// says about it.
    #[serde(default)]
    pub note: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::workflow::engine::StepOutcome;

    #[test]
    fn advance_arguments_are_closed_and_map_onto_the_engines_outcomes() {
        let ok: WorkflowAdvanceArgs =
            serde_json::from_str(r#"{"outcome":"success","note":"tests green"}"#)
                .expect("a minimal advance parses");
        assert_eq!(ok.id, None);
        assert_eq!(ok.outcome.outcome(), StepOutcome::Success);
        assert_eq!(ok.note.as_deref(), Some("tests green"));

        let failed: WorkflowAdvanceArgs =
            serde_json::from_str(r#"{"id":"wf-1","outcome":"failure"}"#).expect("failure parses");
        assert_eq!(failed.outcome.outcome(), StepOutcome::Failure);

        // A key the schema does not declare is refused rather than ignored, so
        // a model cannot smuggle an "evidence" field past the verification store.
        assert!(
            serde_json::from_str::<WorkflowAdvanceArgs>(
                r#"{"outcome":"success","evidence":"trust me"}"#
            )
            .is_err()
        );
        assert!(serde_json::from_str::<WorkflowLookupArgs>(r#"{"branch":"main"}"#).is_err());
    }
}
