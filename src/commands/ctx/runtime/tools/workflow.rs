//! Typed arguments for the native workflow tools (issue #484, roadmap N15).
//!
//! The CLI and native tools use the same workflow engine and gates.
//! Advance and approval require a live writer permit at the broker effect
//! boundary, where a request cannot grant itself authority.

use serde::Deserialize;
use serde_json::{Value, json};

use super::{ToolError, ToolErrorCode};

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

/// Omitted workflow ID uses the CLI's deterministic selection from task and classification. (#542)
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct WorkflowStartArgs {
    #[serde(default)]
    pub id: Option<String>,
    pub task: String,
}

impl super::NativeToolClient {
    fn workflow_state(
        &self,
        id: Option<&str>,
    ) -> Result<crate::commands::workflow::engine::WorkflowState, ToolError> {
        use crate::commands::workflow::engine;

        match id {
            Some(id) => engine::load(&self.state, &self.repo, id).map_err(ToolError::external),
            None => engine::load_active(&self.state, &self.repo)
                .map_err(ToolError::external)?
                .ok_or_else(|| {
                    ToolError::new(
                        ToolErrorCode::PreconditionFailed,
                        "no active workflow in this repository; start one with `zirv workflow start` or name an id",
                    )
                }),
        }
    }

    pub(super) fn workflow_status(&self, id: Option<&str>) -> Result<Value, ToolError> {
        let state = self.workflow_state(id)?;
        let step = state.current();
        Ok(json!({
            "id": state.id,
            "status": format!("{:?}", state.status),
            "branch": state.branch,
            "task": state.task,
            "step": step.map(|step| json!({
                "id": step.id,
                "phase": format!("{:?}", step.phase),
            })),
            "completed_steps": state.completed_steps,
            // The one fact a session most needs and can least infer: whether
            // the workflow would let it finish right now, in the engine's own
            // words. `None` means nothing blocks it.
            "completion_gate": crate::commands::workflow::engine::native_completion_gate(
                &self.state,
                &self.repo,
            ),
        }))
    }

    pub(super) fn workflow_context(&self, id: Option<&str>) -> Result<Value, ToolError> {
        let state = self.workflow_state(id)?;
        let home = crate::utils::home_dir().ok();
        let text = crate::commands::workflow::engine::render_current_context(
            &state,
            &self.repo,
            home.as_deref(),
        )
        .map_err(ToolError::external)?;
        Ok(json!({ "id": state.id, "context": text }))
    }

    pub(super) fn workflow_advance(&self, args: &WorkflowAdvanceArgs) -> Result<Value, ToolError> {
        use crate::commands::workflow::engine;

        let state = self.workflow_state(args.id.as_deref())?;
        let advanced =
            engine::advance_with_evidence(&self.state, state, args.outcome.outcome(), None, false)
                .map_err(ToolError::external)?;
        Ok(json!({
            "id": advanced.id,
            "status": format!("{:?}", advanced.status),
            "step": advanced.current().map(|step| step.id.clone()),
            "note": args.note,
        }))
    }

    pub(super) fn workflow_approve(&self, id: Option<&str>) -> Result<Value, ToolError> {
        use crate::commands::workflow::engine;

        let state = self.workflow_state(id)?;
        let approved = engine::approve(&self.state, state).map_err(ToolError::external)?;
        Ok(json!({
            "id": approved.id,
            "status": format!("{:?}", approved.status),
            "step": approved.current().map(|step| step.id.clone()),
        }))
    }

    /// Workflow tools share the CLI's trust-checked registry for this repository. (#542)
    fn workflow_registry(
        &self,
    ) -> Result<crate::commands::workflow::registry::WorkflowRegistry, ToolError> {
        use crate::commands::workflow::{registry::WorkflowRegistry, skill::SkillRegistry};

        let home = crate::utils::home_dir().ok();
        let skills = SkillRegistry::load_for_repo(&self.repo, home.as_deref(), true)
            .map_err(ToolError::external)?;
        WorkflowRegistry::load_for_repo(&self.repo, home.as_deref(), true, &skills)
            .map_err(ToolError::external)
    }

    /// Returns the EXACT same JSON shape `zirv workflow list --json` prints
    /// (a JSON array of the registry's `RegisteredWorkflow` entries) -- a
    /// native session and a headless caller must see one registry, not two
    /// independently-shaped views of it (issue #542 chunk 3b decision 4).
    pub(super) fn workflow_list(&self) -> Result<Value, ToolError> {
        let registry = self.workflow_registry()?;
        serde_json::to_value(registry.list().collect::<Vec<_>>()).map_err(ToolError::external)
    }

    /// Starts a workflow through the EXACT same `workflow::engine::
    /// start_workflow` the CLI's `Start` handler calls -- omitting `id`
    /// selects one deterministically (`selection::select_definition`)
    /// against `task`, exactly like `zirv workflow start` with no id.
    /// Shared-scope WRITE: the broker requires a live writer permit for
    /// this session's own worktree (`action()`, above), the same posture
    /// `workflow_advance`/`workflow_approve` already have.
    pub(super) fn workflow_start(&self, args: &WorkflowStartArgs) -> Result<Value, ToolError> {
        use crate::commands::workflow::engine;

        let start_args = engine::StartArgs {
            id: args.id.clone(),
            task: args.task.clone(),
            agent: None,
            built_in_only: false,
            repo: Some(self.repo.clone()),
            paths: Vec::new(),
            changed_lines: None,
            tests_changed: false,
            complexity: None,
            risk: None,
            branch: None,
            frontend_root: None,
            brainstorm: false,
            no_brainstorm: false,
            profile: None,
            json: true,
        };
        let outcome =
            engine::start_workflow(&self.state, &start_args).map_err(ToolError::external)?;
        let mut value = serde_json::to_value(&outcome.state).map_err(ToolError::external)?;
        if let (Some(selection), Value::Object(map)) = (&outcome.selection, &mut value) {
            map.insert(
                "selection".into(),
                serde_json::to_value(selection).map_err(ToolError::external)?,
            );
        }
        Ok(value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::ctx::runtime::tools::tests::*;
    use crate::commands::ctx::runtime::tools::*;

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

    #[test]
    fn start_arguments_accept_an_optional_id_and_are_closed() {
        let selected: WorkflowStartArgs =
            serde_json::from_str(r#"{"task":"investigate the KPI drop"}"#)
                .expect("id is optional, selection resolves it");
        assert_eq!(selected.id, None);
        assert_eq!(selected.task, "investigate the KPI drop");

        let explicit: WorkflowStartArgs =
            serde_json::from_str(r#"{"id":"bugfix","task":"fix the retry loop"}"#)
                .expect("an explicit id parses too");
        assert_eq!(explicit.id.as_deref(), Some("bugfix"));

        assert!(
            serde_json::from_str::<WorkflowStartArgs>(r#"{}"#).is_err(),
            "task is required"
        );
        assert!(
            serde_json::from_str::<WorkflowStartArgs>(r#"{"task":"x","agent":"claude"}"#).is_err(),
            "an undeclared field must be refused, not silently ignored"
        );
    }

    /// Issue #484 (roadmap N15): the workflow tools are registered like every
    /// other native tool, and the read/write split is the BROKER's, not the
    /// prompt's. The fixture's session holds no writer permit -- the same
    /// shape a read-only helper or reviewer seat runs in -- so reading the
    /// workflow works and moving it is refused at effect time.
    #[test]
    fn a_session_with_no_writer_permit_can_read_a_workflow_but_never_advance_it() {
        let registry = ToolRegistry::native();
        for name in [
            WORKFLOW_STATUS,
            WORKFLOW_CONTEXT,
            WORKFLOW_ADVANCE,
            WORKFLOW_APPROVE,
        ] {
            let definition = registry
                .get(name)
                .unwrap_or_else(|| panic!("missing {name}"));
            assert_eq!(definition.input_schema["additionalProperties"], false);
            assert!(!definition.capabilities.is_empty());
        }
        assert!(
            registry
                .get(WORKFLOW_ADVANCE)
                .expect("advance")
                .capabilities
                .iter()
                .any(|capability| capability == "repo_fs_write"),
            "moving a workflow is a write and has to declare one"
        );

        let mut fixture = delegation_fixture(0);
        // No workflow at all: the read reaches the engine and says so, which
        // is what proves this is the real engine and not a stub.
        let missing = call(&mut fixture.client, WORKFLOW_STATUS, json!({}));
        assert_eq!(
            missing.error.as_ref().map(|error| error.code.clone()),
            Some(ToolErrorCode::PreconditionFailed),
            "{missing:?}"
        );

        let refused = call(
            &mut fixture.client,
            WORKFLOW_ADVANCE,
            json!({"outcome":"success"}),
        );
        assert_eq!(
            refused.error.as_ref().map(|error| error.code.clone()),
            Some(ToolErrorCode::ResourceBusy),
            "an advance without a writer permit must be refused BEFORE the engine is reached, not after: {refused:?}"
        );

        let bad_id = registry
            .parse(WORKFLOW_STATUS, json!({"id":"../other"}))
            .expect_err("a workflow id may not escape the store");
        assert_eq!(bad_id.code, ToolErrorCode::InvalidArguments);
    }

    /// Issue #542 chunk 3b: `workflow_start` mints a new durable workflow --
    /// the same shared-scope WRITE posture as `workflow_advance`/
    /// `workflow_approve` above, refused before the engine is ever reached
    /// when this session holds no writer permit for its own worktree.
    #[test]
    fn workflow_start_tool_requires_a_writer_permit() {
        let mut fixture = delegation_fixture(0);
        let refused = call(
            &mut fixture.client,
            WORKFLOW_START,
            json!({"task": "fix a bug in the retry loop"}),
        );
        assert_eq!(
            refused.error.as_ref().map(|error| error.code.clone()),
            Some(ToolErrorCode::ResourceBusy),
            "starting a workflow without a writer permit must be refused BEFORE the engine is reached: {refused:?}"
        );
    }

    /// Issue #542 chunk 3b decision 4: a native session and a headless
    /// caller must see one registry and one workflow-start result, not two
    /// independently-shaped views of them.
    #[test]
    fn workflow_list_and_start_tools_match_the_headless_json() {
        let mut fixture = fixture_with(0, "implementer", true);
        // workflow_start classifies through git when the args declare no
        // explicit change surface, so the fixture repo needs a real commit.
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
                .expect("run git");
            assert!(status.success(), "git {args:?} failed");
        };
        git(&["init", "-q"]);
        std::fs::write(fixture.repo.join("README.md"), "hello\n").expect("write");
        git(&["add", "."]);
        git(&["commit", "-q", "-m", "base"]);

        let listed = call(&mut fixture.client, WORKFLOW_LIST, json!({}));
        assert!(listed.error.is_none(), "{listed:?}");
        let tool_list = result_of(&listed).clone();

        let skills = crate::commands::workflow::skill::SkillRegistry::load_for_repo(
            &fixture.repo,
            None,
            true,
        )
        .expect("skills");
        let headless_registry =
            crate::commands::workflow::registry::WorkflowRegistry::load_for_repo(
                &fixture.repo,
                None,
                true,
                &skills,
            )
            .expect("registry");
        let headless_list =
            serde_json::to_value(headless_registry.list().collect::<Vec<_>>()).expect("json");
        assert_eq!(tool_list, headless_list);

        let started = call(
            &mut fixture.client,
            WORKFLOW_START,
            json!({"id": "bugfix", "task": "fix the retry loop"}),
        );
        assert!(started.error.is_none(), "{started:?}");
        let tool_state = result_of(&started).clone();
        let id = tool_state["id"].as_str().expect("workflow id").to_string();

        let headless_state =
            crate::commands::workflow::engine::load(&fixture.state, &fixture.repo, &id)
                .expect("headless load");
        let headless_value = serde_json::to_value(&headless_state).expect("json");
        assert_eq!(tool_state, headless_value);
    }
}
