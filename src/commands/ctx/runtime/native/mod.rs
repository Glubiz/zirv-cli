//! The native agent loop (issue #478, roadmap N09).
//!
//! This is the module that makes `RuntimeKind::Native` a real backend. It
//! conducts the provider request -> tool execution -> provider continuation
//! cycle itself, from an explicit session/turn/request/tool state machine,
//! over the contracts the earlier steps shipped:
//!
//! - N02/N07/N08 [`ProviderAdapter`] transports ONE already-compiled request.
//!   It never owns the loop, never executes a tool and never persists
//!   anything. No provider agent SDK is involved at any point.
//! - N03 [`Journal`] is the authoritative conversation. Every request this
//!   loop sends is rebuilt from the journal's own replay, so a request is a
//!   projection of durable facts rather than of in-memory bookkeeping that a
//!   crash would take with it.
//! - N04/N05 authorization and tools sit behind [`ToolExecutor`]. The
//!   production implementation is `NativeToolClient`, which brokers every
//!   action at effect time; the loop adds the durable barrier and the
//!   ordering contract on top and never reaches a filesystem itself.
//! - N09's own shared lifecycle services (`ctx::lifecycle`) decide tool
//!   admission and whether a session may stop. Those calls are direct: a
//!   native session never shells out to a hook and never probes for an
//!   installed coding harness.
//!
//! # The barrier
//!
//! A tool cannot execute until (1) its arguments parsed as a complete JSON
//! object, (2) the shared before-tool service admitted it, and (3) the
//! assistant message that requested it, plus the tool-call record itself, are
//! durably committed. A truncated argument stream therefore cannot become an
//! effect, and a crash between "committed" and "executed" leaves a record the
//! next open can reconcile rather than a silent gap.
//!
//! # Ordering
//!
//! Providers require tool results in the order their tool-use blocks were
//! emitted, keyed by call id. The scheduler here is free to run independent
//! (read-only) calls before mutating ones, so results can COMPLETE out of
//! order; [`TurnOutcome::results`] is always rebuilt in the provider's own
//! declared order before it goes back over the wire.
//!
//! # Input, steering and interruption
//!
//! Every accepted input is acknowledged durably (`InputAcknowledged`) the
//! moment it is taken, before anything can be lost. Delivery boundaries are
//! explicit: an acknowledged input joins the conversation at the next request
//! built for this session -- between requests inside a turn, or between turns
//! -- never mid-stream and never mid-tool. An input that has not reached a
//! boundary before the loop stops stays visibly queued in the final status,
//! so "delivered once, or still queued" is always decidable from durable
//! state.
//!
//! An interrupt cancels the in-flight provider stream, every tool that has
//! not started, and every remaining turn. It deliberately does NOT cancel an
//! effect that already started: that execution becomes `OutcomeUnknown` and
//! must be reconciled before any retry. Interrupted work is never reported
//! complete.
//!
//! Split (issue split/native) into submodules by responsibility, re-exported
//! here so every existing `native::Item` path outside this directory still
//! resolves:
//! - `types`: the state machine, tool seam and result/status types shared by
//!   every backend below.
//! - `turn`: [`NativeLoop`] itself -- the turn loop and the journal/resume
//!   mechanics it shares with the other entry points.
//! - `backend`: [`NativeBackend`], the [`RuntimeBackend`] impl.
//! - `headless`: `zirv ctx exec --runtime native` (`run_headless`/
//!   `run_session`) and the transport/broker construction the other entry
//!   points below share.
//! - `interactive`: [`InteractiveSession`], a dashboard native pane's
//!   multi-turn worker.
//! - `hosted`: the persistent runtime's already-open native conversation
//!   (`run_hosted_turns`).

mod backend;
mod headless;
mod hosted;
mod interactive;
mod turn;
mod types;

pub use backend::NativeBackend;
pub(crate) use headless::session_broker;
pub use headless::{
    Accounting, HeadlessRequest, journal_route_identity, route_pool, route_provider, run_headless,
    run_session,
};
pub use hosted::{HostedTurn, run_hosted_turns};
pub use interactive::{
    InteractiveProgress, InteractiveRequest, InteractiveSession, spawn_interactive,
};
pub use turn::resume_journal;
pub use types::{
    AbortedRun, NativeFinalStatus, NativeLimits, NativeStatus, NativeToolCall, SessionState,
    ToolExecutor, TurnState,
};

#[cfg(test)]
mod tests {
    use super::super::super::config::OrchestratorWrites;
    use super::super::super::provider::{
        AccountId, BillingPoolId, EndpointId, ModelId, Protocol, ProviderId, RouteId,
    };
    use super::super::journal::{
        Journal, JournalSessionId, RouteIdentity, SeatId, SessionIdentity,
    };
    use super::super::{RuntimeKind, SessionSpec, UiSurface};
    use super::types::{CompactionSettings, NativeLimits, NativeSessionConfig};

    pub(super) fn no_env(_: &str) -> Option<String> {
        None
    }

    pub(super) fn route_for(protocol: Protocol, model: &str) -> RouteIdentity {
        RouteIdentity {
            route: RouteId::new("fixture").unwrap(),
            provider: ProviderId::new(match protocol {
                Protocol::AnthropicMessages => "anthropic",
                Protocol::GoogleGenerativeAi | Protocol::GoogleVertex => "google",
                _ => "openai",
            })
            .unwrap(),
            endpoint: EndpointId::new("fixture").unwrap(),
            account: AccountId::new("fixture").unwrap(),
            billing_pool: BillingPoolId::new("fixture").unwrap(),
            protocol,
            model: ModelId {
                vendor: "fixture".into(),
                id: model.into(),
            },
        }
    }

    pub(super) fn journal_for(
        route: &RouteIdentity,
    ) -> (tempfile::TempDir, Journal, JournalSessionId) {
        let dir = tempfile::tempdir().unwrap();
        let mut journal = Journal::open_path(dir.path().join("journal.sqlite")).unwrap();
        let session = JournalSessionId::new("native-session-1").unwrap();
        journal
            .create_session(&SessionIdentity {
                session: session.clone(),
                seat: SeatId::new("seat-1").unwrap(),
                generation: 1,
                task: None,
                route: route.clone(),
                repo: std::path::PathBuf::from("/native-test-repo"),
                created_at: 1,
                completed_at: None,
            })
            .unwrap();
        (dir, journal, session)
    }

    pub(super) fn config_for(
        session: JournalSessionId,
        route: RouteIdentity,
    ) -> NativeSessionConfig {
        NativeSessionConfig {
            session,
            generation: 1,
            route,
            role: "worker".to_string(),
            seat_model: None,
            write_posture: OrchestratorWrites::Allow,
            limits: NativeLimits::default(),
            task: None,
            workflow_gate: None,
            compaction: CompactionSettings::default(),
            workflow_repo: None,
            system: Vec::new(),
            preamble: Vec::new(),
            prompt_cache: Default::default(),
        }
    }

    pub(super) fn spec(runtime: RuntimeKind) -> SessionSpec {
        SessionSpec {
            runtime,
            role: "worker".into(),
            agent: None,
            provider_route: None,
            model: None,
            surface: UiSurface::Headless,
            cwd: std::path::PathBuf::from("."),
            prompt: "go".into(),
            extra_args: Vec::new(),
        }
    }

    /// Shared setup for the two `spawn_interactive` shutdown tests below:
    /// a `StateDir` rooted at a fresh temp dir, an `env` that resolves it
    /// via `ZIRV_CTX_STATE_DIR` (the same pattern `a_resume_with_a_
    /// reconcile_notice_writes_exactly_one_json_object_to_stdout` above
    /// uses for `run_headless`), a real-but-empty repo tree for the writer
    /// permit to claim, and an `InteractiveRequest` pointed at the
    /// `helper-answer.json` fixture -- one short text-only turn, so a test
    /// never depends on tool-call machinery to exercise shutdown itself.
    pub(super) fn interactive_shutdown_fixture() -> (
        tempfile::TempDir,
        super::super::super::state::StateDir,
        std::path::PathBuf,
        std::collections::HashMap<String, String>,
    ) {
        let tmp = tempfile::tempdir().expect("tempdir");
        let repo = tempfile::tempdir().expect("repo");
        let state_dir = tmp.path().join("state");
        let state = super::super::super::state::StateDir::from_root(state_dir.clone());
        let tree = std::fs::canonicalize(repo.path()).expect("canonicalize repo");
        let env: std::collections::HashMap<String, String> = [(
            crate::commands::ctx::state::STATE_ENV.to_string(),
            state_dir.to_str().expect("utf8").to_string(),
        )]
        .into();
        (repo, state, tree, env)
    }
}
