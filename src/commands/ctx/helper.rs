//! The one native helper-model call (issue #484, roadmap N15).
//!
//! Zirv makes model calls that are not the main chat loop: handoff
//! distillation, `ctx ask`, `ctx optimize`'s judgment, the loop's objective
//! judge, memory harvest and consolidation, the workflow reviewer and the
//! built-in agent seats. Before this step every one of them reached a vendor
//! CLI, so a "native" session still could not run without Claude or Codex on
//! PATH.
//!
//! This module is the single native replacement for all of them. It is
//! deliberately ONE service rather than per-call-site provider code: a helper
//! call is always the same shape -- one bounded conversation, a read-only tool
//! set, a text answer -- and the interesting decisions (which route, what
//! budget, what a failure means) belong in one place where they can be
//! reviewed once.
//!
//! # Selection
//!
//! There is no new configuration key. A helper runs natively exactly when the
//! operator's own native provider configuration names a route for that
//! helper's ROLE (`[roles]` in `~/.zirv/native.toml`, or `--route`); otherwise
//! [`available`] reports `false` and the caller keeps its existing harness
//! path unchanged. That is the same `[roles]` selection `zirv ctx exec
//! --runtime native` and `zirv agent --runtime native` already use, so a
//! legacy session is never silently migrated and a native session never needs
//! a harness binary.
//!
//! # Read-only is the broker's decision, not this module's
//!
//! A helper session is constructed with NO writer permit. Every repository
//! write, every outside write, every process with write effects and every
//! shared-scope knowledge write is then refused by
//! [`super::runtime::enforcement::ExecutionBroker`] itself, at effect time,
//! with `BrokerError::WriterPermit` -- not by a convention this module could
//! forget to apply, and not by a prompt the model could be talked out of.
//! `ApprovalMode::Headless` means the refusal cannot be approved away either.
//! There is therefore no read-only enforcement code in this module at all:
//! the absent permit IS the mechanism.

use std::path::Path;

use super::config::EnvLookup;
use super::runtime::native::{self, NativeLimits, NativeStatus};

/// The role a helper call resolves its route from. These are the `[roles]`
/// keys an operator writes in the native provider configuration; a role with
/// no entry simply has no native path and the caller stays on its harness.
pub const ROLE_DISTILLER: &str = "distiller";
pub const ROLE_ASK: &str = "ask";
pub const ROLE_OPTIMIZE: &str = "optimize";
/// A built-in workflow agent seat (`zirv workflow agents dispatch --runtime
/// native`). The independent code reviewer is NOT here: it runs as a real
/// delegated worker through `zirv agent --runtime native`, so its route comes
/// from that command's own positional or the `worker` role.
pub const ROLE_SEAT: &str = "seat";

/// How much of a session a helper may spend. Deliberately far below
/// [`NativeLimits::default`]: a helper answers one question, and a helper that
/// can run for an hour is a helper that can eat a session's whole budget.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HelperBudget {
    pub max_turns: u32,
    pub max_tool_calls: u32,
    pub max_wall_ms: u64,
    pub max_output_tokens: u64,
}

impl Default for HelperBudget {
    fn default() -> Self {
        Self {
            max_turns: 8,
            max_tool_calls: 24,
            max_wall_ms: 5 * 60 * 1000,
            max_output_tokens: 8_192,
        }
    }
}

impl HelperBudget {
    /// A distiller/judge call: one question, one answer, no tools at all. The
    /// prompt already carries everything, so a tool call here would only be a
    /// way to spend time.
    pub fn one_shot(timeout_ms: u64) -> Self {
        Self {
            max_turns: 2,
            max_tool_calls: 0,
            max_wall_ms: timeout_ms,
            max_output_tokens: 8_192,
        }
    }

    fn into_limits(self) -> NativeLimits {
        NativeLimits {
            max_turns: self.max_turns,
            max_tool_calls: self.max_tool_calls,
            max_wall_ms: self.max_wall_ms,
            max_output_tokens: self.max_output_tokens,
            ..NativeLimits::default()
        }
    }
}

/// One helper call.
#[derive(Debug)]
pub struct HelperRequest<'a> {
    pub repo: &'a Path,
    pub prompt: &'a str,
    /// The `[roles]` key the route is resolved from when `route` is `None`.
    pub role: &'a str,
    /// An explicit route, overriding the role lookup.
    pub route: Option<&'a str>,
    pub budget: HelperBudget,
    /// Operator-only transport override, the same `fixture:<path>` shape
    /// `zirv ctx exec --runtime native --provider` accepts. Production callers
    /// pass `None`; it exists so this service can be driven deterministically.
    pub provider: Option<&'a str>,
}

/// What a helper call produced.
#[derive(Clone, Debug, PartialEq)]
pub struct HelperAnswer {
    pub text: String,
    pub route: String,
    pub model: String,
    pub status: NativeStatus,
}

/// Why a helper call could not produce an answer. Typed because the callers
/// react differently: [`HelperError::Unconfigured`] means "this operator has
/// no native route for this role", which is a routine fall back to the harness
/// path, while the rest are real failures worth reporting.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HelperError {
    /// No native provider configuration, or none naming a route for this role.
    /// The caller keeps its existing behaviour; this is not an error to print.
    Unconfigured(String),
    /// The session ran but was refused, blocked or cut short by a limit.
    Refused(String),
    /// The session failed outright (transport, credentials, journal).
    Failed(String),
    /// The session completed and said nothing usable.
    Empty,
}

impl std::fmt::Display for HelperError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unconfigured(detail) => write!(f, "no native helper route: {detail}"),
            Self::Refused(detail) => write!(f, "native helper was refused: {detail}"),
            Self::Failed(detail) => write!(f, "native helper failed: {detail}"),
            Self::Empty => write!(f, "native helper returned no answer"),
        }
    }
}

impl std::error::Error for HelperError {}

/// Whether a helper for `role` has a native route on this machine.
///
/// Answers from operator configuration alone -- no credential store, no
/// network, no spawned process -- so a caller can decide which path to take
/// before doing any work, and a missing credential still fails where it
/// should: at the actual request.
pub fn available(repo: &Path, role: &str, route: Option<&str>, env: EnvLookup<'_>) -> bool {
    native::route_provider(repo, route, role, env).is_ok()
}

/// Runs one bounded, read-only native helper call and returns its text.
///
/// The session holds no writer permit, so every mutating effect is refused by
/// the execution broker (see this module's own documentation). It also holds
/// no task card: a helper is not a delegation and must not take ownership of
/// one.
pub fn run(request: &HelperRequest<'_>, env: EnvLookup<'_>) -> Result<HelperAnswer, HelperError> {
    if request.prompt.trim().is_empty() {
        return Err(HelperError::Failed(
            "a helper call needs a non-empty prompt".to_string(),
        ));
    }
    // Route resolution first, and its failure is `Unconfigured`, not `Failed`:
    // "this operator has no native route for this role" is the ordinary case
    // on a machine that has not configured one, and must not be reported as a
    // helper malfunction.
    //
    // Skipped for an operator transport override, exactly as `native::
    // build_transport` skips the provider configuration for one: a
    // `fixture:<path>` spec already names the whole transport, and demanding a
    // configured route for it would make the deterministic path unreachable on
    // a machine with no provider configuration at all.
    if request.provider.is_none() {
        native::route_provider(request.repo, request.route, request.role, env)
            .map_err(|error| HelperError::Unconfigured(error.to_string()))?;
    }

    let mut notices: Vec<u8> = Vec::new();
    let status = native::run_session(
        &mut native::HeadlessRequest {
            repo: request.repo,
            prompt: request.prompt,
            route: request.route,
            role: request.role,
            limits: request.budget.into_limits(),
            resume: None,
            provider: request.provider,
            fixture_tools: None,
            // A helper is not a delegation: it takes no task card, and
            // therefore never competes for one with a real worker.
            task: None,
            // The read-only mechanism. See the module documentation.
            writer: None,
        },
        &mut notices,
        env,
    )
    .map_err(|error| HelperError::Failed(error.to_string()))?;

    let text = status.final_text.clone().unwrap_or_default();
    match status.status {
        NativeStatus::Completed if !text.trim().is_empty() => Ok(HelperAnswer {
            text,
            route: status.route.clone(),
            model: status
                .served_model
                .clone()
                .unwrap_or(status.configured_model),
            status: status.status,
        }),
        NativeStatus::Completed => Err(HelperError::Empty),
        NativeStatus::Failed => {
            Err(HelperError::Failed(status.failure.clone().unwrap_or_else(
                || "the native helper session failed".to_string(),
            )))
        }
        // Incomplete/limit/interrupted with usable text is still an answer
        // worth returning -- a distiller that hit its turn ceiling after
        // writing a complete handoff is more useful than no handoff -- but the
        // status travels with it so a caller can tell.
        other if !text.trim().is_empty() => Ok(HelperAnswer {
            text,
            route: status.route.clone(),
            model: status
                .served_model
                .clone()
                .unwrap_or(status.configured_model),
            status: other,
        }),
        other => Err(HelperError::Refused(
            status
                .blocked_reason
                .clone()
                .or_else(|| status.limit.map(|limit| limit.as_str().to_string()))
                .unwrap_or_else(|| other.as_str().to_string()),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::ctx::state::STATE_ENV;
    use crate::commands::ctx::testenv::VarGuard;

    fn fixture(name: &str) -> std::path::PathBuf {
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests")
            .join("fixtures")
            .join("runtime")
            .join("native")
            .join(name)
    }

    fn env_for(state: &std::path::Path) -> impl Fn(&str) -> Option<String> + use<> {
        let state = state.to_string_lossy().into_owned();
        move |key: &str| (key == STATE_ENV).then(|| state.clone())
    }

    #[test]
    fn an_unconfigured_role_is_reported_as_unconfigured_not_as_a_failure() {
        let repo = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        let state = tempfile::tempdir().unwrap();
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());
        let env = env_for(state.path());
        let error = run(
            &HelperRequest {
                repo: repo.path(),
                prompt: "summarize",
                role: ROLE_DISTILLER,
                route: None,
                budget: HelperBudget::one_shot(1_000),
                provider: None,
            },
            &env,
        )
        .unwrap_err();
        assert!(
            matches!(error, HelperError::Unconfigured(_)),
            "unexpected error: {error:?}"
        );
        assert!(!available(repo.path(), ROLE_DISTILLER, None, &env));
    }

    #[test]
    fn an_empty_prompt_is_refused_before_any_route_is_resolved() {
        let repo = tempfile::tempdir().unwrap();
        let error = run(
            &HelperRequest {
                repo: repo.path(),
                prompt: "   ",
                role: ROLE_ASK,
                route: None,
                budget: HelperBudget::default(),
                provider: None,
            },
            &|_| None,
        )
        .unwrap_err();
        assert!(matches!(error, HelperError::Failed(_)));
    }

    /// The whole point of the native helper: no coding harness on PATH at all.
    /// The fixture transport stands in for the provider, so this exercises
    /// session construction, the loop and the answer extraction without a
    /// network call -- with `PATH` scrubbed, so a regression that reintroduced
    /// a vendor-CLI dependency on this path would fail here.
    #[test]
    fn a_helper_answers_with_every_coding_harness_removed_from_path() {
        let repo = crate::commands::ctx::testenv::repo();
        let home = tempfile::tempdir().unwrap();
        let state = tempfile::tempdir().unwrap();
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());
        let _path = VarGuard::set(&[("PATH", Some(""))]);
        let script = fixture("helper-answer.json");
        let answer = run(
            &HelperRequest {
                repo: repo.path(),
                prompt: "distill this",
                role: ROLE_DISTILLER,
                route: None,
                budget: HelperBudget::one_shot(30_000),
                provider: Some(&format!("fixture:{}", script.display())),
            },
            &env_for(state.path()),
        )
        .unwrap();
        assert_eq!(answer.status, NativeStatus::Completed);
        assert!(answer.text.contains("## Task"), "got: {}", answer.text);
    }

    /// Issue #484 item 6: a read-only helper is read-only because the BROKER
    /// says so, not because the helper is asked nicely.
    ///
    /// The broker here is built by `native::session_broker` -- the exact
    /// function a real session's tool executor is built by -- with the `None`
    /// writer lease `run` above passes. A helper that tries to write is
    /// refused at effect time; the same helper reading the same file is not.
    #[test]
    fn a_helper_that_tries_to_write_is_refused_by_the_broker() {
        use crate::commands::ctx::runtime::RuntimeKind;
        use crate::commands::ctx::runtime::enforcement::ExecutionIdentity;
        use crate::commands::ctx::runtime::native::session_broker;
        use crate::commands::ctx::runtime::tools::{NativeToolClient, ToolErrorCode, ToolLimits};
        use crate::commands::ctx::seat;
        use crate::commands::ctx::state::StateDir;

        let root = tempfile::tempdir().unwrap();
        let repo = root.path().join("repo");
        let home = root.path().join("home");
        std::fs::create_dir_all(&repo).unwrap();
        std::fs::create_dir_all(&home).unwrap();
        let repo = std::fs::canonicalize(&repo).unwrap();
        // The broker's resource claims discover the checkout's git metadata,
        // so the helper's own repository has to be a real one.
        assert!(
            std::process::Command::new("git")
                .args(["init", "-q"])
                .current_dir(&repo)
                .status()
                .map(|status| status.success())
                .unwrap_or(false),
            "git init"
        );
        std::fs::write(repo.join("README.md"), "evidence\n").unwrap();
        let state = StateDir::from_root(root.path().join("state"));

        seat::store(
            &state,
            &seat::Seat {
                short: "helper01".to_string(),
                session: "native-helper-1".to_string(),
                generation: 1,
                agent: "native".to_string(),
                model: None,
                provider: "fixture".to_string(),
                role: ROLE_SEAT.to_string(),
                pinned: false,
                phase: Default::default(),
                visited: Vec::new(),
                last_rollover_at: None,
                pending: None,
                displaced: None,
                created_at: 1,
                updated_at: 1,
                runtime: RuntimeKind::Native,
            },
        )
        .unwrap();

        let broker = session_broker(
            &repo,
            &state,
            &home,
            &Default::default(),
            ExecutionIdentity {
                session: "native-helper-1".to_string(),
                short: "helper01".to_string(),
                generation: 1,
                role: ROLE_SEAT.to_string(),
                task: None,
            },
            // The read-only mechanism, verbatim from `run`.
            None,
            // No operator dialog: a helper session stays headless, so the
            // refusal cannot be approved away either (issue #490, N21 item B).
            None,
        )
        .unwrap();
        let mut client =
            NativeToolClient::new(broker, state.clone(), repo.clone(), ToolLimits::testing());

        let refused = client.execute(
            "file_write",
            serde_json::json!({
                "path": "README.md",
                "content": "rewritten",
                "idempotency_key": "helper-write-1"
            }),
            None,
            None,
        );
        assert_eq!(
            refused.error.as_ref().map(|error| error.code.clone()),
            Some(ToolErrorCode::ResourceBusy),
            "a helper write must be refused for want of a writer permit: {refused:?}"
        );
        assert_eq!(
            std::fs::read_to_string(repo.join("README.md")).unwrap(),
            "evidence\n",
            "the refusal must be before the effect, not after it"
        );

        let allowed = client.execute(
            "file_read",
            serde_json::json!({"path": "README.md"}),
            None,
            None,
        );
        assert!(
            allowed.error.is_none(),
            "read-only does not mean blind: {allowed:?}"
        );
    }
}
