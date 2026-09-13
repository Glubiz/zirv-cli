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

use serde::Deserialize;

use super::{ToolError, ToolErrorCode};
use crate::commands::ctx::runtime::RuntimeKind;

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

fn require(value: &str, field: &str) -> Result<(), ToolError> {
    if value.trim().is_empty() {
        return Err(ToolError::new(
            ToolErrorCode::InvalidArguments,
            format!("{field} must not be empty"),
        ));
    }
    Ok(())
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
        require(&self.brief, "brief")?;
        if let Some(target) = &self.target {
            require(target, "target")?;
        }
        if let Some(role) = &self.role {
            require(role, "role")?;
        }
        if let Some(task) = &self.task {
            require(task, "task")?;
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

#[cfg(test)]
mod tests {
    use super::*;

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

    #[test]
    fn the_tool_name_list_has_no_duplicates() {
        let unique: std::collections::BTreeSet<&str> = ALL.into_iter().collect();
        assert_eq!(unique.len(), ALL.len());
    }
}
