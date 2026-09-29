//! The typed native delegation tools (issue #479, roadmap N10).
//!
//! Seven tools -- `delegate`, `send`, `wait`, `result`, `follow_up`,
//! `interrupt`, `close` -- that let a NATIVE orchestrator run the same fleet
//! a legacy one runs. Each is a thin, validated argument shape in front of
//! the SAME `ctx::delegation` service method the CLI verb calls; none of them
//! contains delegation logic of its own, which is the only way the two
//! surfaces can be guaranteed to agree.
//!
//! Provider output supplies a tool name and JSON arguments and nothing else:
//! every field below is validated here, the delegation id is checked against
//! the service's own id rule before it can name a file, and the action still
//! crosses `ExecutionAction::Delegate` at the broker for effect-time seat and
//! policy validation -- a native session cannot delegate its way around the
//! fence its own tools run behind.

use std::path::Path;

use serde::Deserialize;

use super::{ToolError, ToolErrorCode};
use crate::commands::ctx::runtime::RuntimeKind;
use crate::commands::ctx::state;
use serde_json::{Value, json};

pub const DELEGATE: &str = "delegate";
pub const SEND: &str = "send";
pub const WAIT: &str = "wait";
pub const RESULT: &str = "result";
pub const FOLLOW_UP: &str = "follow_up";
pub const INTERRUPT: &str = "interrupt";
pub const CLOSE: &str = "close";

/// Every delegation tool name, in registry order. One list, so the registry,
/// the parser and the dispatcher cannot drift apart.
pub const ALL: [&str; 7] = [DELEGATE, SEND, WAIT, RESULT, FOLLOW_UP, INTERRUPT, CLOSE];

/// Default byte budget for a `result` manifest's own summary. A manifest is
/// never the full report -- it names where the report is and says whether it
/// cut the summary.
pub const DEFAULT_RESULT_BYTES: usize = 4096;

/// Upper bound on a bounded `wait`. A native orchestrator waiting on its
/// fleet must not be able to park itself indefinitely inside one tool call.
pub const MAX_WAIT_SECS: u64 = 900;

#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ToolRuntime {
    #[default]
    Native,
    Harness,
}

impl ToolRuntime {
    pub fn kind(self) -> RuntimeKind {
        match self {
            ToolRuntime::Native => RuntimeKind::Native,
            ToolRuntime::Harness => RuntimeKind::Harness,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ToolMode {
    #[default]
    Writing,
    ReadOnly,
}

/// `delegate`: start one worker on a shared task, on either runtime.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DelegateArgs {
    pub brief: String,
    /// Harness name (harness runtime) or provider route (native). Defaults to
    /// `native`, i.e. the `[roles]` entry for `role`.
    #[serde(default)]
    pub target: Option<String>,
    #[serde(default)]
    pub runtime: ToolRuntime,
    #[serde(default)]
    pub role: Option<String>,
    /// The SHARED task-card id (`zirv ctx task`) this worker fulfils.
    #[serde(default)]
    pub task: Option<String>,
    #[serde(default)]
    pub group: Option<String>,
    #[serde(default)]
    pub workdir: Option<String>,
    #[serde(default)]
    pub mode: ToolMode,
    #[serde(default)]
    pub budget_tokens: Option<u64>,
    #[serde(default)]
    pub max_tool_calls: Option<u32>,
    /// Workflow manifest ID this worker fills; absent uses the role default. (#541)
    #[serde(default)]
    pub manifest: Option<String>,
    /// Bypass the "must match an unfilled team-plan seat" rule. Honoured
    /// only when the delegating seat is itself the coordinator
    /// (`coordinator::check`); anyone else's is silently ignored.
    #[serde(default, rename = "override")]
    pub override_: bool,
}

/// The four tools that only ever name an existing delegation.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HandleArgs {
    pub delegation: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MessageArgs {
    pub delegation: String,
    pub message: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WaitArgs {
    pub delegation: String,
    #[serde(default)]
    pub timeout_secs: Option<u64>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResultArgs {
    pub delegation: String,
    #[serde(default)]
    pub max_bytes: Option<usize>,
}

/// The id rule the delegation store itself enforces, applied at the argument
/// boundary so a malformed id from provider output is an `InvalidArguments`
/// error rather than a path the service has to reject later.
pub fn validate_handle(delegation: &str) -> Result<(), ToolError> {
    if delegation.is_empty()
        || delegation.len() > 128
        || !delegation
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return Err(ToolError::new(
            ToolErrorCode::InvalidArguments,
            "delegation must be 1-128 characters of [A-Za-z0-9_-]",
        ));
    }
    Ok(())
}

impl DelegateArgs {
    pub fn validate(&self) -> Result<(), ToolError> {
        super::non_empty(self.brief.trim(), "brief")?;
        if let Some(target) = &self.target {
            super::non_empty(target.trim(), "target")?;
        }
        if let Some(role) = &self.role {
            super::non_empty(role.trim(), "role")?;
        }
        if let Some(task) = &self.task {
            super::non_empty(task.trim(), "task")?;
        }
        if let Some(manifest) = &self.manifest {
            super::non_empty(manifest.trim(), "manifest")?;
        }
        Ok(())
    }

    pub fn target_or_default(&self) -> String {
        self.target
            .clone()
            .unwrap_or_else(|| RuntimeKind::Native.as_str().to_string())
    }

    pub fn role_or_default(&self) -> String {
        self.role.clone().unwrap_or_else(|| "worker".to_string())
    }

    /// The non-empty `task` the broker's own `ExecutionAction::Delegate`
    /// validation requires. A delegation with no shared card still names what
    /// it is, rather than borrowing an empty string.
    pub fn action_task(&self) -> String {
        self.task.clone().unwrap_or_else(|| "ad-hoc".to_string())
    }
}

impl WaitArgs {
    /// Clamped, never refused: a model that asks to wait an hour gets the
    /// bounded wait, plus a manifest that says the deadline it actually got.
    pub fn bounded_secs(&self) -> u64 {
        self.timeout_secs.unwrap_or(60).min(MAX_WAIT_SECS)
    }
}

impl ResultArgs {
    pub fn bounded_bytes(&self) -> usize {
        self.max_bytes
            .unwrap_or(DEFAULT_RESULT_BYTES)
            .clamp(256, DEFAULT_RESULT_BYTES * 4)
    }
}

impl super::NativeToolClient {
    pub(super) fn delegate(&mut self, args: DelegateArgs) -> Result<Value, ToolError> {
        use crate::commands::ctx::delegation as service;
        use crate::commands::ctx::team;

        let cfg = self.ctx_config()?;
        // Model-named routes must satisfy operator policy and role billing; explicit operator routes retain their authority. (#485)
        let target = args.target_or_default();
        if args.runtime == ToolRuntime::Native
            && target != crate::commands::ctx::runtime::RuntimeKind::Native.as_str()
            && let Some(native) = self.native_config()
        {
            team::authorize_route(&native, &args.role_or_default(), &target).map_err(
                |refusal| ToolError::new(ToolErrorCode::AuthorizationDenied, refusal.to_string()),
            )?;
        }
        let workdir = match args.workdir.as_deref() {
            Some(workdir) => {
                let roots = crate::commands::ctx::dash::workdir_roots(&cfg, &self.repo);
                Some(
                    crate::commands::ctx::dash::resolved_spawn_cwd(
                        self.repo.clone(),
                        Some(Path::new(workdir)),
                        &roots,
                    )
                    .map_err(|error| {
                        ToolError::new(ToolErrorCode::AuthorizationDenied, error.to_string())
                    })?,
                )
            }
            None => None,
        };
        let request = service::LaunchRequest {
            runtime: args.runtime.kind(),
            target: args.target_or_default(),
            brief: args.brief.clone(),
            role: args.role_or_default(),
            task: args.task.clone(),
            group: args.group.clone(),
            workdir,
            manifest: args.manifest.clone(),
            plan_override_requested: args.override_,
            manifest_registry: self.manifest_registry().map(std::sync::Arc::new),
            read_only: args.mode == ToolMode::ReadOnly,
            budget_tokens: args.budget_tokens,
            max_tool_calls: args.max_tool_calls,
            worker_session: None,
            delegated_depth: None,
            parent_envelope: None,
            parent_principal: None,
            cancellation: std::sync::Arc::new(
                crate::commands::ctx::provider::adapter::CancellationFlag::default(),
            ),
        };
        let identity = self.broker.identity().clone();
        // Read role and remaining depth from the persisted, fenced seat before delegating;
        // neither is reachable from model output. (#485)
        let parent_envelope = crate::commands::ctx::agent::resolve_parent_envelope(&cfg, &|key| {
            self.launch_env.get(key).cloned()
        })
        .map_err(|reason| ToolError::new(ToolErrorCode::AuthorizationDenied, reason))?;
        let mut request = request;
        request.parent_envelope = Some(
            crate::commands::ctx::envelope::canonical_json(&parent_envelope)
                .map_err(ToolError::external)?,
        );
        request.parent_principal = Some(parent_envelope.principal.clone());
        let depth = parent_envelope.delegation_depth;
        let (record, publication) = service::delegate(
            &self.state,
            &self.repo,
            &cfg,
            self.launcher.as_mut(),
            &request,
            &service::Parent {
                session: Some(identity.session.as_str()),
                short: &identity.short,
                role: &identity.role,
                depth,
                // Refuse a superseded generation before creating a durable launch receipt. (#488)
                generation: Some(identity.generation),
                generation_locked: true,
            },
            state::now_secs(),
        )
        .map_err(ToolError::external)?;
        Ok(json!({
            "delegation": record.handle.delegation,
            "attempt": record.handle.attempt,
            "runtime": record.handle.runtime.as_str(),
            "phase": record.phase.as_str(),
            "task": record.handle.task,
            "manifest": record.handle.manifest,
            "plan_override": record.handle.plan_override,
            "exit_code": record.exit_code,
            "delivery": publication.as_ref().map(|publication| &publication.identity),
            "mailed": publication.is_some_and(|publication| publication.mailed),
        }))
    }

    pub(super) fn send_to_worker(&mut self, args: MessageArgs) -> Result<Value, ToolError> {
        use crate::commands::ctx::delegation as service;

        let cfg = self.ctx_config()?;
        let dispatch = service::send(
            &self.state,
            &self.repo,
            &cfg,
            &args.delegation,
            &args.message,
            state::now_secs(),
        )
        .map_err(ToolError::external)?;
        Ok(match dispatch {
            service::Dispatch::Delivered { .. } => json!({"delivered": true}),
            service::Dispatch::Queued { id, reason } => json!({
                "delivered": false,
                "queued": id,
                "reason": reason,
                "retry": "the message is durable and is delivered at the worker's next idle \
                          boundary; it is never typed into an open dialog",
            }),
        })
    }

    pub(super) fn wait_for_worker(&mut self, args: WaitArgs) -> Result<Value, ToolError> {
        use crate::commands::ctx::delegation as service;

        let now = state::now_secs();
        let deadline = now.saturating_add(args.bounded_secs());
        let outcome = service::wait(&self.state, &self.repo, &args.delegation, now, deadline)
            .map_err(ToolError::external)?;
        Ok(match outcome {
            service::WaitOutcome::Ready(record) => json!({
                "ready": true,
                "phase": record.phase.as_str(),
                "exit_code": record.exit_code,
            }),
            service::WaitOutcome::Pending => {
                json!({"ready": false, "deadline_secs": args.bounded_secs()})
            }
            service::WaitOutcome::TimedOut => json!({"ready": false, "timed_out": true}),
        })
    }

    pub(super) fn worker_result(&mut self, args: ResultArgs) -> Result<Value, ToolError> {
        use crate::commands::ctx::delegation as service;

        let manifest = service::result(
            &self.state,
            &self.repo,
            &args.delegation,
            args.bounded_bytes(),
        )
        .map_err(ToolError::external)?;
        serde_json::to_value(manifest).map_err(ToolError::external)
    }

    pub(super) fn follow_up(&mut self, args: MessageArgs) -> Result<Value, ToolError> {
        use crate::commands::ctx::delegation as service;

        let cfg = self.ctx_config()?;
        let continuation = service::follow_up(
            &self.state,
            &self.repo,
            &cfg,
            &args.delegation,
            &args.message,
            state::now_secs(),
        )
        .map_err(ToolError::external)?;
        Ok(match continuation {
            service::Continuation::Directed { dispatch } => json!({
                "route": "directed",
                "delivered": matches!(dispatch, service::Dispatch::Delivered { .. }),
            }),
            service::Continuation::Resume {
                journal_session,
                attempt,
            } => json!({
                "route": "resume",
                "session": journal_session,
                "attempt": attempt,
                // The journal IS the conversation, so a native continuation
                // is a real resume of the original session rather than a
                // replacement: `--resume` takes this exact id, reconciles
                // anything that was still in flight as outcome-unknown and
                // advances the generation.
                "resume_with": "zirv ctx exec --runtime native --resume <session>",
            }),
            service::Continuation::Checkpoint { handoff } => json!({
                "route": "checkpoint",
                "handoff": handoff,
                "note": "no verified resume path; this is a replacement worker with none of the \
                         original's hidden context",
            }),
        })
    }

    pub(super) fn interrupt_worker(&mut self, args: HandleArgs) -> Result<Value, ToolError> {
        use crate::commands::ctx::delegation as service;

        let record =
            service::interrupt(&self.state, &self.repo, &args.delegation, state::now_secs())
                .map_err(ToolError::external)?;
        Ok(json!({
            "phase": record.phase.as_str(),
            "unknown_tool_outcomes": record.unknown_tool_outcomes,
        }))
    }

    pub(super) fn close_worker(&mut self, args: HandleArgs) -> Result<Value, ToolError> {
        use crate::commands::ctx::delegation as service;

        let record = service::close(&self.state, &self.repo, &args.delegation, state::now_secs())
            .map_err(ToolError::external)?;
        Ok(json!({
            "phase": record.phase.as_str(),
            "receipts": record.published,
            "unknown_tool_outcomes": record.unknown_tool_outcomes,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::ctx::attention;
    use crate::commands::ctx::delegation as service;
    use crate::commands::ctx::runtime::tools::tests::*;
    use crate::commands::ctx::runtime::tools::*;

    #[test]
    fn a_brief_is_required_and_defaults_are_explicit() {
        let args: DelegateArgs =
            serde_json::from_value(serde_json::json!({"brief": "read src/x.rs"})).expect("parse");
        args.validate().expect("valid");
        assert_eq!(args.runtime, ToolRuntime::Native);
        assert_eq!(args.target_or_default(), "native");
        assert_eq!(args.role_or_default(), "worker");
        assert_eq!(args.action_task(), "ad-hoc");
        assert_eq!(args.mode, ToolMode::Writing);
        assert_eq!(args.manifest, None);
        assert!(!args.override_);

        let empty: DelegateArgs =
            serde_json::from_value(serde_json::json!({"brief": "  "})).expect("parse");
        assert!(empty.validate().is_err());
    }

    #[test]
    fn unknown_fields_from_provider_output_are_rejected() {
        assert!(
            serde_json::from_value::<DelegateArgs>(
                serde_json::json!({"brief":"x","surprise":true})
            )
            .is_err()
        );
        assert!(
            serde_json::from_value::<HandleArgs>(serde_json::json!({"delegation":"a","x":1}))
                .is_err()
        );
    }

    #[test]
    fn a_delegation_id_that_could_name_a_file_is_refused_at_the_boundary() {
        for bad in ["../escape", "a/b", "", "with space"] {
            assert!(validate_handle(bad).is_err(), "{bad:?}");
        }
        validate_handle("a1b2c3").expect("plain ids are fine");
    }

    #[test]
    fn wait_and_result_budgets_are_bounded() {
        let long: WaitArgs =
            serde_json::from_value(serde_json::json!({"delegation":"a","timeout_secs":99999}))
                .expect("parse");
        assert_eq!(long.bounded_secs(), MAX_WAIT_SECS);
        let default: WaitArgs =
            serde_json::from_value(serde_json::json!({"delegation":"a"})).expect("parse");
        assert_eq!(default.bounded_secs(), 60);

        let huge: ResultArgs =
            serde_json::from_value(serde_json::json!({"delegation":"a","max_bytes":10_000_000}))
                .expect("parse");
        assert_eq!(huge.bounded_bytes(), DEFAULT_RESULT_BYTES * 4);
        let tiny: ResultArgs =
            serde_json::from_value(serde_json::json!({"delegation":"a","max_bytes":1}))
                .expect("parse");
        assert_eq!(tiny.bounded_bytes(), 256);
    }

    /// Issue #541 chunk C, decision 2: the JSON key is the reserved word
    /// `override` (a Rust keyword, so the field itself is named
    /// `override_`), and `manifest` round-trips as an ordinary optional
    /// string.
    #[test]
    fn manifest_and_override_parse_from_their_json_keys() {
        let args: DelegateArgs = serde_json::from_value(serde_json::json!({
            "brief": "implement the thing",
            "manifest": "implementer",
            "override": true,
        }))
        .expect("parse");
        args.validate().expect("valid");
        assert_eq!(args.manifest.as_deref(), Some("implementer"));
        assert!(args.override_);

        let blank_manifest: DelegateArgs = serde_json::from_value(serde_json::json!({
            "brief": "x",
            "manifest": "  ",
        }))
        .expect("parse");
        assert!(blank_manifest.validate().is_err());
    }

    #[test]
    fn the_tool_name_list_has_no_duplicates() {
        let unique: std::collections::BTreeSet<&str> = ALL.into_iter().collect();
        assert_eq!(unique.len(), ALL.len());
    }

    #[test]
    fn every_delegation_tool_is_registered_with_a_closed_schema_and_a_delegation_claim() {
        let registry = ToolRegistry::native();
        for name in ALL {
            let definition = registry
                .get(name)
                .unwrap_or_else(|| panic!("missing {name}"));
            assert_eq!(definition.input_schema["additionalProperties"], false);
            assert!(
                definition
                    .resource_claims
                    .contains(&ResourceClaimKind::DelegationStore),
                "{name} must declare the delegation store it touches"
            );
        }
        assert_eq!(
            registry.get(DELEGATE).map(|definition| definition.retry),
            Some(RetryPolicy::NeverAfterStart),
            "a dispatched worker must never be re-dispatched by a blind retry"
        );
    }

    #[test]
    fn a_native_orchestrator_delegates_to_either_runtime_through_one_service() {
        // Acceptance criteria (a) and (f): the same tool, the same durable
        // record and the same bounded manifest whichever runtime ran the
        // worker -- a mixed fleet is one ownership view, not two.
        for runtime in ["native", "harness"] {
            let mut fixture = delegation_fixture(0);
            let receipt = call(
                &mut fixture.client,
                DELEGATE,
                json!({
                    "brief": "read src/main.rs and report the entry point",
                    "runtime": runtime,
                    "target": "claude",
                    "task": "task-7",
                }),
            );
            assert_eq!(receipt.state, ToolReceiptState::Completed, "{receipt:?}");
            let result = receipt.result.clone().expect("result");
            assert_eq!(result["runtime"], runtime);
            assert_eq!(result["task"], "task-7");
            assert_eq!(result["phase"], "completed");
            let handle = handle_from(receipt);
            assert!(
                result["delivery"]
                    .as_str()
                    .is_some_and(|identity| identity.starts_with(&handle)),
                "the terminal outcome carries a delivery identity"
            );
            assert_eq!(
                fixture.launches.lock().expect("launches").len(),
                1,
                "exactly one worker was started"
            );

            // The bounded manifest an unchanged orchestrator consumes.
            let manifest = call(
                &mut fixture.client,
                RESULT,
                json!({ "delegation": handle.clone() }),
            )
            .result
            .expect("manifest");
            assert_eq!(manifest["phase"], "completed");
            assert_eq!(manifest["runtime"], runtime);
            assert_eq!(manifest["deliveries"].as_array().map(Vec::len), Some(1));

            // And the durable record behind it says the same thing.
            let record = service::load(&fixture.state, &fixture.repo, &handle).expect("record");
            assert_eq!(record.handle.task.as_deref(), Some("task-7"));
            assert_eq!(record.attempts.len(), 1);
        }
    }

    #[test]
    fn nested_native_delegation_uses_the_narrowed_child_environment() {
        let mut fixture = delegation_fixture(0);
        let mut wide = crate::commands::ctx::agent::root_envelope(&CtxConfig::default());
        wide.delegation_depth = 3;
        wide.network = true;
        let mut child = wide.clone();
        child.principal = "root/child".to_string();
        child.paths = vec![crate::commands::ctx::envelope::PathScope::new("src")];
        child.network = false;
        child.delegation_depth = 1;
        let wide_json = crate::commands::ctx::envelope::canonical_json(&wide).expect("wide");
        let child_json = crate::commands::ctx::envelope::canonical_json(&child).expect("child");
        let parent_env = |key: &str| {
            (key == crate::commands::ctx::agent::ENVELOPE_ENV).then(|| wide_json.clone())
        };
        let child_env = crate::commands::ctx::agent::envelope_env(
            &parent_env,
            Some(child_json),
            Some(child.principal.clone()),
        );
        fixture.client.install_launch_env(&child_env);

        let receipt = call(
            &mut fixture.client,
            DELEGATE,
            json!({"brief":"nested work","runtime":"native"}),
        );
        assert_eq!(receipt.state, ToolReceiptState::Completed, "{receipt:?}");
        let launches = fixture.launches.lock().expect("launches");
        assert_eq!(launches.len(), 1);
        assert_eq!(launches[0].delegated_depth, Some(0));
        let installed: crate::commands::ctx::envelope::WorkerEnvelope = serde_json::from_str(
            launches[0]
                .parent_envelope
                .as_deref()
                .expect("parent envelope"),
        )
        .expect("installed envelope");
        assert_eq!(installed, child);
    }

    #[test]
    // Issue #551: model-selected delegation workdirs stay inside operator roots.
    fn native_delegate_refuses_workdir_outside_configured_roots() {
        let mut fixture = delegation_fixture(0);
        let outside = tempfile::tempdir().expect("outside checkout");
        for checkout in [&fixture.repo, outside.path()] {
            let status = std::process::Command::new("git")
                .args(["init", "--quiet"])
                .current_dir(checkout)
                .status()
                .expect("git init");
            assert!(status.success());
        }
        let allowed = fixture.repo.to_string_lossy().to_string();
        let _env = crate::commands::ctx::testenv::VarGuard::set(&[(
            "ZIRV_CTX_DASH_WORKDIR_ROOTS",
            Some(&allowed),
        )]);

        let receipt = call(
            &mut fixture.client,
            DELEGATE,
            json!({
                "brief": "work in the unrelated checkout",
                "workdir": outside.path(),
            }),
        );

        assert_eq!(receipt.state, ToolReceiptState::Failed, "{receipt:?}");
        assert!(
            receipt
                .error
                .as_ref()
                .is_some_and(|error| error.message.contains("workdir roots")),
            "{receipt:?}"
        );
        assert!(fixture.launches.lock().expect("launches").is_empty());
    }

    #[test]
    fn a_failed_worker_is_never_reported_as_a_completed_delegation() {
        let mut fixture = delegation_fixture(2);
        let result = call(
            &mut fixture.client,
            DELEGATE,
            json!({"brief": "do the thing"}),
        )
        .result
        .expect("result");
        assert_eq!(result["phase"], "failed");
        assert_eq!(result["exit_code"], 2);
    }

    #[test]
    fn a_message_to_a_worker_with_an_approval_open_is_queued_not_typed() {
        // Acceptance criterion (f): #468's rule, reached through the native
        // tool surface rather than the dashboard pane sweep.
        let mut fixture = delegation_fixture(0);
        let handle = handle_from(call(
            &mut fixture.client,
            DELEGATE,
            json!({"brief": "investigate"}),
        ));

        let record = service::load(&fixture.state, &fixture.repo, &handle).expect("record");
        attention::record(
            &fixture.state,
            &record.handle.short,
            attention::Observation::new(
                attention::Authority::AdapterHook,
                "permission prompt open",
                90,
                1,
            )
            .with_attention(attention::Attention::Approval),
            1,
        );

        let queued = call(
            &mut fixture.client,
            SEND,
            json!({"delegation": handle, "message": "status?"}),
        )
        .result
        .expect("result");
        assert_eq!(queued["delivered"], false);
        assert_eq!(queued["reason"], "approval-open");
    }

    #[test]
    fn follow_up_interrupt_and_close_all_address_the_original_delegation() {
        // Acceptance criteria (d) and (e), through the tools.
        let mut fixture = delegation_fixture(0);
        let handle = handle_from(call(
            &mut fixture.client,
            DELEGATE,
            json!({"brief": "look"}),
        ));

        let follow_up = call(
            &mut fixture.client,
            FOLLOW_UP,
            json!({"delegation": handle.clone(), "message": "and the tests?"}),
        )
        .result
        .expect("result");
        assert_eq!(follow_up["route"], "resume");
        assert_eq!(follow_up["attempt"], 2);

        let unknown = call(
            &mut fixture.client,
            FOLLOW_UP,
            json!({"delegation": "nosuchdelegation", "message": "hi"}),
        );
        assert_eq!(
            unknown.state,
            ToolReceiptState::Failed,
            "an unknown handle must fail, never fall back to a recent session"
        );

        let interrupted = call(
            &mut fixture.client,
            INTERRUPT,
            json!({ "delegation": handle.clone() }),
        )
        .result
        .expect("result");
        assert_eq!(interrupted["phase"], "launched");

        let premature = call(
            &mut fixture.client,
            CLOSE,
            json!({ "delegation": handle.clone() }),
        );
        assert_eq!(premature.state, ToolReceiptState::Failed);

        service::publish_terminal(
            &fixture.state,
            &fixture.repo,
            &Default::default(),
            &handle,
            service::Phase::Cancelled,
            Some(130),
            Some("cancelled".to_string()),
            None,
            state::now_secs(),
        )
        .expect("terminal cancellation");

        let closed = call(&mut fixture.client, CLOSE, json!({ "delegation": handle }))
            .result
            .expect("result");
        assert_eq!(closed["phase"], "closed");
        assert!(
            closed["receipts"]
                .as_array()
                .is_some_and(|receipts| !receipts.is_empty()),
            "closing preserves the receipts already published"
        );
    }

    #[test]
    fn a_bounded_wait_on_a_live_delegation_reports_pending_without_waking_anything() {
        let mut fixture = delegation_fixture(0);
        let handle = service::record_launch(
            &fixture.state,
            &fixture.repo,
            service::WorkerHandle {
                delegation: "livedelegation".to_string(),
                attempt: 1,
                runtime: super::super::super::RuntimeKind::Native,
                worker_session: "w".to_string(),
                short: "wshort".to_string(),
                role: "worker".to_string(),
                task: None,
                group: None,
                objective: None,
                workdir: fixture.repo.clone(),
                manifest: None,
                plan_override: false,
            },
            None,
            1,
        )
        .expect("launch")
        .handle
        .delegation;
        let waited = call(
            &mut fixture.client,
            WAIT,
            json!({"delegation": handle, "timeout_secs": 99999}),
        )
        .result
        .expect("result");
        assert_eq!(waited["ready"], false);
        assert_eq!(waited["deadline_secs"], MAX_WAIT_SECS);
    }

    #[test]
    fn a_malformed_handle_from_provider_output_never_reaches_the_store() {
        let mut fixture = delegation_fixture(0);
        let receipt = call(
            &mut fixture.client,
            RESULT,
            json!({"delegation": "../../etc/passwd"}),
        );
        assert_eq!(receipt.state, ToolReceiptState::Failed);
        assert_eq!(
            receipt.error.map(|error| error.code),
            Some(ToolErrorCode::InvalidArguments)
        );
    }
}
