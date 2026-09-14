//! Native conversations owned by the persistent runtime (issue #489, step
//! N20 of the native-runtime roadmap #469).
//!
//! This is `host.rs`'s counterpart for sessions that have a journal instead of
//! a pseudoterminal, and it is deliberately assembled from the same
//! primitives the rest of zirv already uses rather than from new ones:
//! `runtime::native::NativeBackend` (identity, durable acknowledgement,
//! generation fencing), `runtime::journal::Journal` (the durable barrier),
//! `sessions::SessionGuard` (the registry record), and
//! `runtime::native::run_hosted_turns` (the SAME transport, broker and agent
//! loop a headless `zirv ctx exec --runtime native` uses).
//!
//! Three rules are worth stating where they are implemented:
//!
//! - **The service holds the registry record.** Pacing, budgets, rot scoring,
//!   mail addressing, writer permits, `zirv ctx status` and workflow policy
//!   all read the session registry. A runtime-owned native session files one
//!   and the SERVICE holds the guard, so detaching every client changes
//!   nothing any of them can see -- exactly the mechanism §2.1 of the
//!   persistent-runtime design note records for a pty session.
//! - **Detach, cancel and stop are three things.** [`NativeHost::detach`] only
//!   moves entries in `clients`/`controller`; [`NativeHost::interrupt`] only
//!   sets a cancellation flag, ending the TURN; [`NativeHost::stop`] is the
//!   one method that completes the journal session and releases the registry
//!   guard.
//! - **A restart reconciles, it never replays.** [`NativeSessions::restore`]
//!   turns every execution that was durably `Started` into `OutcomeUnknown`
//!   and advances the generation, and reports what it could and could not
//!   bring back by name. Nothing is re-submitted, and no shell process is ever
//!   described as having survived.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};

use super::super::CtxResult;
use super::super::api::server::NativeHost;
use super::super::api::wire::{
    ApiError, ApprovalDecision, AttachMode, AttachRole, Attachment, ErrorCode, HistoryEntry,
    HistoryRole, InputAck, NativeEvent, NativeHistory, NativePage, SessionFacts, SessionState,
    TaskOutcome,
};
use super::super::provider::adapter::CancellationFlag;
use super::super::runtime::journal::{
    AssistantBlock, ConversationState, ExecutionState, Journal, JournalEvent, JournalSessionId,
    MessageRole, RouteIdentity, SeatId, SequenceId, SessionIdentity, TaskId, TaskReceiptState,
};
use super::super::runtime::native::{
    HostedTurn, NativeLimits, journal_route_identity, resume_journal, run_hosted_turns,
};
use super::super::runtime::{
    BackendConversationRef, RuntimeBackend, RuntimeKind, SessionHandle, SessionSpec, UiSurface,
    native::NativeBackend,
};
use super::super::state::{self, StateDir};
use super::super::{prompt::PromptRole, sessions};

/// How many ENDED native sessions the table keeps, for the same reason
/// `host::MAX_ENDED_SESSIONS` exists: a runtime up for a week must not list
/// every conversation it has ever run. Live sessions are never pruned.
pub const MAX_ENDED_NATIVE_SESSIONS: usize = 16;

/// One entry of the durable native topology.
///
/// Separate from `host::Topology` on purpose: the two restore differently. A
/// pty entry can at best be RELAUNCHED against a harness conversation; a
/// native entry's conversation is right there in the journal, so restoring it
/// is reading durable state rather than starting a process.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct NativeEntry {
    pub session_id: String,
    pub short: String,
    pub role: String,
    pub cwd: String,
    #[serde(default)]
    pub route: Option<String>,
    #[serde(default)]
    pub task: Option<String>,
    /// The service instance that owned this session. A restore under a
    /// different instance continues the same conversation under a new
    /// generation; it never claims to be the same run.
    #[serde(default)]
    pub instance: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct NativeTopology {
    pub written: u64,
    pub instance: String,
    pub sessions: Vec<NativeEntry>,
}

pub fn topology_path(state: &StateDir, namespace: &str) -> PathBuf {
    super::namespace::runtime_dir(state)
        .join(format!("{}-native.json", state::provider_slug(namespace)))
}

pub fn write_topology(
    state: &StateDir,
    namespace: &str,
    topology: &NativeTopology,
) -> CtxResult<()> {
    state::create_private_dir_all(&super::namespace::runtime_dir(state))?;
    let body = serde_json::to_string_pretty(topology)?;
    state::write_private(&topology_path(state, namespace), &body)?;
    Ok(())
}

pub fn read_topology(state: &StateDir, namespace: &str) -> Option<NativeTopology> {
    let body = std::fs::read_to_string(topology_path(state, namespace)).ok()?;
    serde_json::from_str(&body).ok()
}

/// What a native restore actually did. Kept apart from the doing so the
/// honesty rule is reportable rather than narrated.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct NativeRestoreReport {
    /// Conversations brought back under a new generation, ready to be driven.
    pub resumed: Vec<String>,
    /// Executions that were durably `Started` when the previous service
    /// stopped and are now `OutcomeUnknown`. Never retried, never assumed to
    /// have failed, and named so an operator can reconcile them for real.
    pub outcome_unknown: Vec<String>,
    /// Entries whose durable state could not be read back at all, with why.
    pub lost: Vec<(String, String)>,
}

/// The two things this table needs from the outside world: which route a new
/// conversation is filed under, and how a queued turn actually runs.
///
/// A trait so the runtime's own bookkeeping -- seats, durability, registry
/// records, reconciliation, cursors, the controller rule -- is testable
/// without a provider, a credential, a network or a model. The production
/// implementation is [`ProviderEnvironment`], which is two calls into the
/// shared native code and nothing else.
pub trait NativeEnvironment: Send + Sync + std::fmt::Debug {
    /// The durable route identity a new conversation is created with.
    fn route_identity(
        &self,
        repo: &Path,
        route: Option<&str>,
        role: &str,
    ) -> CtxResult<RouteIdentity>;
    /// Drives every turn already queued on one conversation to completion.
    fn run(&self, turn: &QueuedTurn) -> CtxResult<()>;
}

/// Everything a runner needs to drive the turns already queued on one
/// conversation. Owned values: it crosses a thread boundary.
#[derive(Debug, Clone)]
pub struct QueuedTurn {
    pub session: String,
    pub seat_short: String,
    pub generation: u64,
    pub role: String,
    pub cwd: PathBuf,
    pub route: Option<String>,
    pub task: Option<String>,
    pub cancel: Arc<CancellationFlag>,
    pub approvals: Arc<super::super::runtime::enforcement::InteractiveApprovals>,
    pub pending_wake: Arc<AtomicBool>,
}

/// The production environment: the shared native agent loop, with the writer
/// permit acquired for the duration of the turn and released with it.
///
/// The permit is per-turn rather than per-session deliberately. A lease is the
/// right to write one tree, and holding one for an idle conversation would
/// block every other worker on that checkout for as long as the operator left
/// the session open.
#[derive(Debug)]
pub struct ProviderEnvironment {
    limits: NativeLimits,
    max_writers: usize,
}

impl ProviderEnvironment {
    pub fn new(limits: NativeLimits, max_writers: usize) -> Self {
        Self {
            limits,
            max_writers,
        }
    }
}

impl NativeEnvironment for ProviderEnvironment {
    fn route_identity(
        &self,
        repo: &Path,
        route: Option<&str>,
        role: &str,
    ) -> CtxResult<RouteIdentity> {
        journal_route_identity(repo, route, role, &super::super::config::env_from_process())
    }

    fn run(&self, turn: &QueuedTurn) -> CtxResult<()> {
        let state = StateDir::resolve(&super::super::config::env_from_process())?;
        let session = JournalSessionId::new(turn.session.clone())?;
        let writer = super::super::permit::acquire_writer(
            &state,
            self.max_writers,
            &format!("session native {}: {}", turn.seat_short, turn.role),
            &turn.cwd,
            // Issue #488 (review finding 1): the runtime handed this turn its
            // seat short and generation, so this lease gets the STRICT
            // verdict -- an uncommitted successor is refused a writer lease
            // just as it is refused a delegation and a graph write.
            Some(super::super::permit::SeatFence {
                short: &turn.seat_short,
                generation: turn.generation,
            }),
        )
        .ok()
        .map(|permit| Box::new(permit) as Box<dyn super::super::runtime::enforcement::WriterLease>);
        let mut hosted = HostedTurn {
            repo: &turn.cwd,
            session: &session,
            seat_short: &turn.seat_short,
            generation: turn.generation,
            role: &turn.role,
            route: turn.route.as_deref(),
            limits: self.limits,
            provider: None,
            fixture_tools: None,
            task: turn.task.clone(),
            writer,
            approvals: Some(Arc::clone(&turn.approvals)),
            cancel: Arc::clone(&turn.cancel),
        };
        let mut notes = Vec::new();
        run_hosted_turns(
            &mut hosted,
            &mut notes,
            &super::super::config::env_from_process(),
        )?;
        Ok(())
    }
}

/// One native conversation this runtime owns.
#[derive(Debug)]
struct NativeSession {
    handle: SessionHandle,
    journal_session: JournalSessionId,
    role: String,
    cwd: PathBuf,
    route: Option<String>,
    task: Option<String>,
    started_at: u64,
    /// Held by the SERVICE. See this module's own doc comment.
    guard: sessions::SessionGuard,
    /// Shared with whichever turn is running, so `session.interrupt` cancels
    /// the turn in flight rather than the next one.
    cancel: Arc<CancellationFlag>,
    /// Whether a turn is in flight. An atomic rather than a field behind the
    /// table's mutex because the runner clears it from its own thread, without
    /// taking the table lock a protocol call may be holding.
    running: Arc<AtomicBool>,
    /// Set by every durable acknowledgement and consumed by the runner.
    pending_wake: Arc<AtomicBool>,
    approvals: Arc<super::super::runtime::enforcement::InteractiveApprovals>,
    approval_prompts: std::sync::mpsc::Receiver<super::super::runtime::enforcement::ApprovalPrompt>,
    pending_approvals: BTreeMap<String, super::super::runtime::enforcement::ApprovalPrompt>,
    clients: Vec<String>,
    controller: Option<String>,
    /// Set on a restore: the predecessor generation this one continues from.
    /// Recorded, never reused as an identity.
    restored_from: Option<u64>,
    ended: bool,
    ended_at: Option<u64>,
}

impl NativeSession {
    fn attachment_for(&self, caller: &str) -> Attachment {
        Attachment {
            controller: self.controller.clone(),
            clients: self.clients.clone(),
            // A native conversation has no terminal, so it has no size. Zero
            // is the honest answer; inventing 24x80 would tell a client to lay
            // out a screen that does not exist.
            rows: 0,
            cols: 0,
            role: if self.controller.as_deref() == Some(caller) {
                AttachRole::Controller
            } else if self.clients.iter().any(|id| id == caller) {
                AttachRole::Observer
            } else {
                AttachRole::Detached
            },
        }
    }

    fn facts(&self) -> SessionFacts {
        SessionFacts {
            session_id: self.handle.logical_id.clone(),
            short: self.handle.short.clone(),
            runtime: RuntimeKind::Native,
            generation: self.handle.generation,
            surface: if self.clients.is_empty() {
                UiSurface::Headless
            } else {
                UiSurface::Terminal
            },
            state: match (self.ended, self.running.load(Ordering::Acquire)) {
                (true, _) => SessionState::Ended,
                (false, true) => SessionState::Working,
                (false, false) => SessionState::Idle,
            },
            role: Some(self.role.clone()),
            agent: Some(RuntimeKind::Native.as_str().to_string()),
            repo_slug: Some(state::repo_slug(&self.cwd)),
            started_at: Some(self.started_at),
            reachable: !self.ended,
        }
    }

    fn entry(&self, instance: &str) -> NativeEntry {
        NativeEntry {
            session_id: self.handle.logical_id.clone(),
            short: self.handle.short.clone(),
            role: self.role.clone(),
            cwd: self.cwd.to_string_lossy().into_owned(),
            route: self.route.clone(),
            task: self.task.clone(),
            instance: instance.to_string(),
        }
    }

    fn queued_turn(&self) -> QueuedTurn {
        QueuedTurn {
            session: self.handle.logical_id.clone(),
            seat_short: self.handle.short.clone(),
            generation: self.handle.generation,
            role: self.role.clone(),
            cwd: self.cwd.clone(),
            route: self.route.clone(),
            task: self.task.clone(),
            cancel: Arc::clone(&self.cancel),
            approvals: Arc::clone(&self.approvals),
            pending_wake: Arc::clone(&self.pending_wake),
        }
    }
}

/// The runtime service's native session table.
pub struct NativeSessions {
    state: StateDir,
    namespace: String,
    instance: String,
    backend: Mutex<NativeBackend>,
    sessions: Mutex<BTreeMap<String, NativeSession>>,
    environment: Arc<dyn NativeEnvironment>,
    ended_cap: AtomicUsize,
    /// Run a queued turn on this thread instead of a spawned one. Test-only:
    /// a deterministic test must not race a background thread, and production
    /// must never block a protocol call on a model round trip.
    inline_turns: AtomicBool,
}

impl std::fmt::Debug for NativeSessions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NativeSessions")
            .field("namespace", &self.namespace)
            .field("instance", &self.instance)
            .finish()
    }
}

impl NativeSessions {
    pub fn new(
        state: StateDir,
        namespace: &str,
        instance: &str,
        environment: Arc<dyn NativeEnvironment>,
    ) -> CtxResult<Arc<Self>> {
        let mut backend = NativeBackend::new();
        backend.attach_journal(Journal::open(&state)?);
        Ok(Arc::new(Self {
            state,
            namespace: namespace.to_string(),
            instance: instance.to_string(),
            backend: Mutex::new(backend),
            sessions: Mutex::new(BTreeMap::new()),
            environment,
            ended_cap: AtomicUsize::new(MAX_ENDED_NATIVE_SESSIONS),
            inline_turns: AtomicBool::new(false),
        }))
    }

    /// Same poison tolerance, and same reason, as `ApiServer::lock` and
    /// `RuntimeHost::lock`: the state behind this mutex is a session table,
    /// not a half-written invariant, and refusing every later call would turn
    /// one panic into a runtime whose conversations can never be reached.
    fn lock(&self) -> std::sync::MutexGuard<'_, BTreeMap<String, NativeSession>> {
        match self.sessions.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        }
    }

    fn backend(&self) -> std::sync::MutexGuard<'_, NativeBackend> {
        match self.backend.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        }
    }

    /// Test seam: drive a queued turn on the calling thread.
    #[cfg(test)]
    pub fn run_turns_inline_for_test(&self) {
        self.inline_turns.store(true, Ordering::Relaxed);
    }

    #[cfg(test)]
    pub fn set_ended_cap_for_test(&self, cap: usize) {
        self.ended_cap.store(cap, Ordering::Relaxed);
    }

    /// The predecessor generation a restored conversation continues from, if
    /// any -- the only place it is read back, and never as an identity.
    pub fn restored_from(&self, session_id: &str) -> Option<u64> {
        self.lock()
            .get(session_id)
            .and_then(|session| session.restored_from)
    }

    /// Writes the durable native topology. Called on every structural change,
    /// so a crash loses at most the sessions opened since the last one.
    pub fn persist_topology(&self) {
        let sessions = self.lock();
        let topology = NativeTopology {
            written: state::now_secs(),
            instance: self.instance.clone(),
            sessions: sessions
                .values()
                .filter(|session| !session.ended)
                .map(|session| session.entry(&self.instance))
                .collect(),
        };
        drop(sessions);
        let _ = write_topology(&self.state, &self.namespace, &topology);
    }

    /// Issue #489 (issue #352's mail-injection residual), for conversations.
    ///
    /// A native session has no terminal to type into, so delivery here is
    /// what it should always have been: the message becomes an ordinary
    /// durable input, recorded in the journal and queued as a turn. Its
    /// idempotency identity is the delivered text itself, so a delivery
    /// re-attempted after a crash between the injection and the consume
    /// records nothing twice.
    ///
    /// Delivery goes through the dashboard's OWN sweep -- the same trust
    /// framing, the same budget cap, the same "consume only if the injection
    /// succeeded" rule -- rather than a second delivery path.
    pub fn deliver_mail(
        &self,
        cfg: &super::super::config::CtxConfig,
        errors: &mut super::super::dash::ErrorLog,
    ) {
        if !cfg.mail.enabled {
            return;
        }
        let targets: Vec<(String, String, String)> = self
            .lock()
            .values()
            .filter(|session| !session.ended && !session.running.load(Ordering::Acquire))
            .map(|session| {
                (
                    session.handle.logical_id.clone(),
                    session.handle.short.clone(),
                    state::repo_slug(&session.cwd),
                )
            })
            .collect();
        for (id, short, slug) in targets {
            let mut injector = ConversationInjector {
                host: self,
                session_id: id.clone(),
            };
            super::super::dash::sweep_one_pane(
                &mut injector,
                &id,
                &self.state,
                &slug,
                RuntimeKind::Native.as_str(),
                &short,
                cfg.mail.max_delivered_bytes,
                errors,
                None,
                &cfg.screen.thresholds(),
            );
        }
    }

    /// The operator's explicit shutdown: the topology is drained first, and
    /// only the conversations named are completed. `stop_all = false` is the
    /// ordinary case -- the service exits, the conversations do not.
    pub fn shutdown(&self, stop_all: bool) {
        self.persist_topology();
        if !stop_all {
            return;
        }
        let ids: Vec<String> = self.lock().keys().cloned().collect();
        for id in ids {
            let _ = self.stop(&id);
        }
    }

    /// Tier 2 for native conversations (issue #489, item 6).
    ///
    /// For every entry of THIS runtime's own durable topology -- never every
    /// session the journal happens to hold, which would fence a concurrent
    /// `zirv ctx exec` out of a conversation this service never owned --
    /// reconcile and adopt:
    ///
    /// 1. read the stored identity, which fails loudly for a session the
    ///    journal has never heard of rather than inventing one;
    /// 2. convert every execution whose last durable state is `Started` into
    ///    `OutcomeUnknown`, because an effect that began and never reported
    ///    cannot be assumed to have failed and must never be silently retried;
    /// 3. advance the generation, fencing any straggler still holding the old
    ///    one out of the journal and out of the execution broker.
    ///
    /// Nothing is re-submitted. A restored conversation is idle and drivable,
    /// and what could not be brought back is reported by name.
    pub fn restore(&self) -> NativeRestoreReport {
        let mut report = NativeRestoreReport::default();
        let Some(topology) = read_topology(&self.state, &self.namespace) else {
            return report;
        };
        for entry in topology.sessions {
            match self.resume_entry(&entry) {
                Ok(outcome) => {
                    report.resumed.push(outcome.0);
                    report.outcome_unknown.extend(outcome.1);
                }
                Err(error) => report.lost.push((entry.short.clone(), error.to_string())),
            }
        }
        self.persist_topology();
        report
    }

    fn resume_entry(&self, entry: &NativeEntry) -> CtxResult<(String, Vec<String>)> {
        let session = JournalSessionId::new(entry.session_id.clone())?;
        let resumed = {
            let mut backend = self.backend();
            let journal = backend
                .journal_mut()
                .ok_or("native runtime: the journal was not attached")?;
            resume_journal(journal, &session, state::now_secs().saturating_mul(1000))?
        };
        let handle = SessionHandle {
            runtime: RuntimeKind::Native,
            logical_id: entry.session_id.clone(),
            short: entry.short.clone(),
            generation: resumed.generation,
            role: entry.role.clone(),
            surface: UiSurface::Headless,
            conversation: Some(BackendConversationRef {
                agent: RuntimeKind::Native.as_str().to_string(),
                conversation: entry.session_id.clone(),
            }),
        };
        store_native_seat(
            &self.state,
            &handle,
            &entry.role,
            &resumed.identity.route,
            state::now_secs(),
        )?;
        self.backend().adopt(&handle, session.clone())?;
        let cwd = restore_cwd(entry, &std::env::current_dir()?);
        let record = self.register(&handle, &entry.role, &cwd);
        let unknown: Vec<String> = resumed.reconciled.iter().map(ToString::to_string).collect();
        let (approvals, approval_prompts) =
            super::super::runtime::enforcement::InteractiveApprovals::new(
                Arc::new(super::super::runtime::enforcement::ApprovalAuthority::new()),
                format!("protocol {}", handle.short),
            );
        self.lock().insert(
            entry.session_id.clone(),
            NativeSession {
                handle,
                journal_session: session,
                role: entry.role.clone(),
                cwd,
                route: entry.route.clone(),
                task: entry.task.clone(),
                started_at: state::now_secs(),
                guard: record,
                cancel: Arc::new(CancellationFlag::default()),
                running: Arc::new(AtomicBool::new(false)),
                pending_wake: Arc::new(AtomicBool::new(false)),
                approvals,
                approval_prompts,
                pending_approvals: BTreeMap::new(),
                clients: Vec::new(),
                controller: None,
                restored_from: Some(resumed.previous_generation),
                ended: false,
                ended_at: None,
            },
        );
        Ok((entry.session_id.clone(), unknown))
    }

    /// The registry record that makes pacing, budgets, rot, mail addressing,
    /// writer permits and workflow policy see a native session -- held by this
    /// process, which is the one actually running its turns.
    ///
    /// `unreachable()`: a native conversation binds no turn-signal socket,
    /// because there is no harness hook to post to one. Saying so is the
    /// honest answer; claiming reachability would make a wake-up look
    /// deliverable when nothing could ever act on it.
    fn register(&self, handle: &SessionHandle, role: &str, cwd: &Path) -> sessions::SessionGuard {
        let mut record = sessions::Record::new(
            &handle.logical_id,
            RuntimeKind::Native.as_str(),
            cwd,
            sessions::Verb::Chat,
        )
        .with_role(role)
        .unreachable();
        record.runtime = RuntimeKind::Native;
        sessions::SessionGuard::register(&self.state, record)
    }

    /// Starts (or resumes, when a turn is already queued) the runner for one
    /// session. Returns without waiting: a protocol call must never block on a
    /// model round trip.
    fn wake(&self, turn: QueuedTurn, running: Arc<AtomicBool>) {
        if running.swap(true, Ordering::AcqRel) {
            // Already running: the loop drains everything queued when it gets
            // to the next delivery boundary, so a second runner would be a
            // second conversation on one journal.
            return;
        }
        turn.approvals.resume();
        let environment = Arc::clone(&self.environment);
        if self.inline_turns.load(Ordering::Relaxed) {
            loop {
                turn.pending_wake.store(false, Ordering::Release);
                let _ = environment.run(&turn);
                if !turn.pending_wake.swap(false, Ordering::AcqRel) {
                    break;
                }
            }
            running.store(false, Ordering::Release);
            return;
        }
        std::thread::spawn(move || {
            loop {
                turn.pending_wake.store(false, Ordering::Release);
                let _ = environment.run(&turn);
                if turn.pending_wake.swap(false, Ordering::AcqRel) {
                    continue;
                }
                running.store(false, Ordering::Release);
                if turn.pending_wake.swap(false, Ordering::AcqRel)
                    && running
                        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                        .is_ok()
                {
                    continue;
                }
                break;
            }
        });
    }

    fn collect_pending_approvals(&self, session_id: &str) -> Result<(), ApiError> {
        let mut sessions = self.lock();
        let Some(session) = sessions.get_mut(session_id) else {
            return Err(ApiError::new(
                ErrorCode::UnknownSession,
                format!("no native session {session_id} on this runtime"),
            ));
        };
        let mut requests = Vec::new();
        while let Ok(prompt) = session.approval_prompts.try_recv() {
            let request = prompt.request().clone();
            let request_id = request.scope_digest.clone();
            session.pending_approvals.insert(request_id.clone(), prompt);
            requests.push((request_id, request));
        }
        if requests.is_empty() {
            return Ok(());
        }
        let journal_session = session.journal_session.clone();
        let generation = session.handle.generation;
        let mut backend = self.backend();
        let journal = backend
            .journal_mut()
            .ok_or_else(|| ApiError::new(ErrorCode::Internal, "the journal was not attached"))?;
        for (request_id, request) in requests {
            let task = TaskId::new(format!("approval-{request_id}"))
                .map_err(|error| ApiError::new(ErrorCode::Internal, error.to_string()))?;
            journal
                .record_task_receipt(
                    &journal_session,
                    generation,
                    &Default::default(),
                    task,
                    TaskReceiptState::Accepted,
                    serde_json::json!({
                        "kind": "approval_request",
                        "request_id": request_id,
                        "request": request,
                    }),
                    state::now_secs(),
                )
                .map_err(|error| ApiError::new(ErrorCode::Internal, error.to_string()))?;
        }
        Ok(())
    }

    fn with_session<T>(
        &self,
        session_id: &str,
        call: impl FnOnce(&mut NativeSession) -> Result<T, ApiError>,
    ) -> Result<T, ApiError> {
        let mut sessions = self.lock();
        let Some(session) = sessions.get_mut(session_id) else {
            return Err(ApiError::new(
                ErrorCode::UnknownSession,
                format!("no native session {session_id} on this runtime"),
            ));
        };
        call(session)
    }

    /// Caps the ENDED table, oldest first. Live conversations are never
    /// pruned: this bounds history, not concurrency -- the same rule, and the
    /// same reason, as `host::prune_ended`.
    fn prune_ended(&self) {
        let cap = self.ended_cap.load(Ordering::Relaxed);
        let mut sessions = self.lock();
        let mut ended: Vec<(u64, String)> = sessions
            .values()
            .filter(|session| session.ended)
            .map(|session| {
                (
                    session.ended_at.unwrap_or_default(),
                    session.handle.logical_id.clone(),
                )
            })
            .collect();
        if ended.len() <= cap {
            return;
        }
        ended.sort();
        let excess = ended.len() - cap;
        for (_, id) in ended.into_iter().take(excess) {
            sessions.remove(&id);
        }
    }
}

/// Where a restored conversation's working directory comes from. Kept separate
/// so an entry naming a directory that no longer exists degrades to the
/// operator's current one rather than failing the whole restore -- the same
/// rule, and the same shape, as `host::restore_cwd`.
pub fn restore_cwd(entry: &NativeEntry, fallback: &Path) -> PathBuf {
    let recorded = PathBuf::from(&entry.cwd);
    if recorded.is_dir() {
        recorded
    } else {
        fallback.to_path_buf()
    }
}

fn store_native_seat(
    state_dir: &StateDir,
    handle: &SessionHandle,
    role: &str,
    route: &RouteIdentity,
    now: u64,
) -> CtxResult<()> {
    super::super::seat::store(
        state_dir,
        &super::super::seat::Seat {
            short: handle.short.clone(),
            session: handle.logical_id.clone(),
            generation: handle.generation,
            agent: RuntimeKind::Native.as_str().to_string(),
            model: Some(route.model.id.clone()),
            provider: route.provider.to_string(),
            role: role.to_string(),
            pinned: false,
            phase: Default::default(),
            visited: Vec::new(),
            last_rollover_at: None,
            pending: None,
            displaced: None,
            created_at: now,
            updated_at: now,
            runtime: RuntimeKind::Native,
        },
    )?;
    sessions::record_conversation_on(
        state_dir,
        &handle.short,
        RuntimeKind::Native.as_str(),
        &handle.logical_id,
        &handle.logical_id,
        RuntimeKind::Native,
    );
    Ok(())
}

impl NativeHost for NativeSessions {
    fn sessions(&self) -> Vec<SessionFacts> {
        self.lock().values().map(NativeSession::facts).collect()
    }

    fn owns(&self, session_id: &str) -> bool {
        self.lock().contains_key(session_id)
    }

    fn start(&self, spec: &SessionSpec) -> Result<SessionFacts, ApiError> {
        if spec.runtime != RuntimeKind::Native {
            return Err(ApiError::new(
                ErrorCode::InvalidParams,
                "this host opens native conversations only",
            ));
        }
        let role = if spec.role.trim().is_empty() {
            PromptRole::Orchestrator.label().to_string()
        } else {
            PromptRole::from_label(&spec.role)
                .ok_or_else(|| {
                    ApiError::new(
                        ErrorCode::InvalidParams,
                        format!("unknown session role '{}'", spec.role),
                    )
                })?
                .label()
                .to_string()
        };
        let facts = self
            .open(spec, &role)
            .map_err(|error| ApiError::new(ErrorCode::Internal, error.to_string()))?;
        // The launch prompt is an ordinary first input: durable before the
        // caller is told the session exists, and queued as a turn like any
        // other. A separate "launch" path would be a second way to get text
        // into a conversation.
        if !spec.prompt.trim().is_empty() {
            self.submit(&facts.session_id, &spec.prompt, false, None)?;
        }
        Ok(facts)
    }

    fn submit(
        &self,
        session_id: &str,
        input: &str,
        steering: bool,
        idempotency: Option<&str>,
    ) -> Result<InputAck, ApiError> {
        let (handle, turn, running, pending_wake) = {
            let sessions = self.lock();
            let Some(session) = sessions.get(session_id) else {
                return Err(ApiError::new(
                    ErrorCode::UnknownSession,
                    format!("no native session {session_id} on this runtime"),
                ));
            };
            if session.ended {
                return Err(ApiError::new(
                    ErrorCode::UnknownSession,
                    "this session has ended",
                ));
            }
            (
                session.handle.clone(),
                session.queued_turn(),
                Arc::clone(&session.running),
                Arc::clone(&session.pending_wake),
            )
        };
        // Durable FIRST, under the caller's own idempotency identity, and only
        // then is a turn queued: a crash between the two costs a wake-up the
        // next submit re-triggers, never the input itself.
        let ack = self
            .backend()
            .accept_input(&handle, input, steering, idempotency)
            .map_err(|error| backend_error(error.as_ref()))?;
        if !ack.duplicate {
            pending_wake.store(true, Ordering::Release);
            self.wake(turn, running);
        }
        Ok(InputAck {
            message_id: ack.message_id.to_string(),
            duplicate: ack.duplicate,
        })
    }

    fn interrupt(&self, session_id: &str) -> Result<bool, ApiError> {
        self.with_session(session_id, |session| {
            if !session.running.load(Ordering::Acquire) {
                return Ok(false);
            }
            session.approvals.cancel();
            session.cancel.cancel();
            Ok(true)
        })
    }

    fn approve(
        &self,
        session_id: &str,
        request_id: &str,
        decision: ApprovalDecision,
        note: Option<&str>,
    ) -> Result<bool, ApiError> {
        self.collect_pending_approvals(session_id)?;
        let (journal_session, prompt) = self.with_session(session_id, |session| {
            Ok((
                session.journal_session.clone(),
                session.pending_approvals.remove(request_id),
            ))
        })?;
        let Some(prompt) = prompt else {
            return Ok(false);
        };
        let interactive_decision = match decision {
            ApprovalDecision::Allow => {
                super::super::runtime::enforcement::InteractiveDecision::Once
            }
            ApprovalDecision::Deny | ApprovalDecision::Unknown => {
                super::super::runtime::enforcement::InteractiveDecision::Deny {
                    guidance: note.unwrap_or("operator denied this action").to_string(),
                }
            }
        };
        // Durable as well as in memory: an approval is an authority decision,
        // and an authority decision that existed only in a process's memory
        // would be unauditable the moment that process went away.
        let mut backend = self.backend();
        let journal = backend
            .journal_mut()
            .ok_or_else(|| ApiError::new(ErrorCode::Internal, "the journal was not attached"))?;
        let task = TaskId::new(format!("approval-{request_id}"))
            .map_err(|error| ApiError::new(ErrorCode::InvalidParams, error.to_string()))?;
        let identity = journal
            .session(&journal_session)
            .map_err(|error| ApiError::new(ErrorCode::Internal, error.to_string()))?;
        journal
            .record_task_receipt(
                &journal_session,
                identity.generation,
                &Default::default(),
                task,
                match decision {
                    ApprovalDecision::Allow => TaskReceiptState::Completed,
                    _ => TaskReceiptState::Cancelled,
                },
                serde_json::json!({
                    "kind": "approval",
                    "request_id": request_id,
                    "decision": if decision == ApprovalDecision::Allow { "allow" } else { "deny" },
                    "note": note,
                }),
                state::now_secs(),
            )
            .map_err(|error| ApiError::new(ErrorCode::Internal, error.to_string()))?;
        Ok(prompt.decide(interactive_decision))
    }

    fn task_result(
        &self,
        session_id: &str,
        task_id: &str,
        outcome: TaskOutcome,
        receipt: &serde_json::Value,
    ) -> Result<bool, ApiError> {
        let journal_session =
            self.with_session(session_id, |session| Ok(session.journal_session.clone()))?;
        let mut backend = self.backend();
        let journal = backend
            .journal_mut()
            .ok_or_else(|| ApiError::new(ErrorCode::Internal, "the journal was not attached"))?;
        let identity = journal
            .session(&journal_session)
            .map_err(|error| ApiError::new(ErrorCode::Internal, error.to_string()))?;
        let task = TaskId::new(task_id.to_string())
            .map_err(|error| ApiError::new(ErrorCode::InvalidParams, error.to_string()))?;
        journal
            .record_task_receipt(
                &journal_session,
                identity.generation,
                &Default::default(),
                task,
                receipt_state(outcome),
                receipt.clone(),
                state::now_secs(),
            )
            .map_err(|error| ApiError::new(ErrorCode::Internal, error.to_string()))?;
        Ok(true)
    }

    fn history(
        &self,
        session_id: &str,
        after: u64,
        limit: usize,
    ) -> Result<NativeHistory, ApiError> {
        let journal_session =
            self.with_session(session_id, |session| Ok(session.journal_session.clone()))?;
        let backend = self.backend();
        let journal = journal_of(&backend)?;
        let state = journal
            .replay(&journal_session)
            .map_err(|error| ApiError::new(ErrorCode::Internal, error.to_string()))?;
        let (_, last) = journal
            .sequence_bounds(&journal_session)
            .map_err(|error| ApiError::new(ErrorCode::Internal, error.to_string()))?;
        let entries = history_entries(&state, after, limit);
        let cursor = entries.last().map(|entry| entry.sequence).unwrap_or(after);
        Ok(NativeHistory {
            session_id: session_id.to_string(),
            generation: state.identity.generation,
            cursor,
            last_sequence: last.0,
            entries,
        })
    }

    fn journal(&self, session_id: &str, after: u64, limit: usize) -> Result<NativePage, ApiError> {
        self.collect_pending_approvals(session_id)?;
        let journal_session =
            self.with_session(session_id, |session| Ok(session.journal_session.clone()))?;
        let backend = self.backend();
        let journal = journal_of(&backend)?;
        let identity = journal
            .session(&journal_session)
            .map_err(|error| ApiError::new(ErrorCode::Internal, error.to_string()))?;
        let (first, last) = journal
            .sequence_bounds(&journal_session)
            .map_err(|error| ApiError::new(ErrorCode::Internal, error.to_string()))?;
        let events = journal
            .events_after(&journal_session, SequenceId(after), limit)
            .map_err(|error| ApiError::new(ErrorCode::Internal, error.to_string()))?;
        let page = NativePage {
            session_id: session_id.to_string(),
            generation: identity.generation,
            after_sequence: after,
            cursor: events.last().map(|e| e.sequence.0).unwrap_or(after),
            last_sequence: last.0,
            gap: gap_at(after, first.0, last.0),
            events: events
                .iter()
                .map(|stored| NativeEvent {
                    sequence: stored.sequence.0,
                    generation: stored.generation,
                    kind: event_kind(&stored.event).to_string(),
                    detail: event_detail(&stored.event),
                })
                .collect(),
        };
        Ok(page)
    }

    fn attach(
        &self,
        session_id: &str,
        client_id: &str,
        mode: AttachMode,
    ) -> Result<Attachment, ApiError> {
        self.with_session(session_id, |session| {
            if !session.clients.iter().any(|id| id == client_id) {
                session.clients.push(client_id.to_string());
                session.clients.sort();
            }
            if mode == AttachMode::Controller {
                match session.controller.clone() {
                    Some(current) if current != client_id => {
                        return Err(ApiError::new(
                            ErrorCode::Busy,
                            format!(
                                "{current} already controls this session; `session.takeover` \
                                 takes the seat explicitly"
                            ),
                        ));
                    }
                    _ => session.controller = Some(client_id.to_string()),
                }
            }
            Ok(session.attachment_for(client_id))
        })
    }

    fn detach(&self, session_id: &str, client_id: &str) -> Result<Attachment, ApiError> {
        self.with_session(session_id, |session| {
            session.clients.retain(|id| id != client_id);
            if session.controller.as_deref() == Some(client_id) {
                session.controller = None;
            }
            // Nothing else. The conversation, its journal, its registry record
            // and any turn in flight are untouched: that is the acceptance
            // criterion this method exists to satisfy.
            Ok(session.attachment_for(client_id))
        })
    }

    fn takeover(&self, session_id: &str, client_id: &str) -> Result<Attachment, ApiError> {
        self.with_session(session_id, |session| {
            if !session.clients.iter().any(|id| id == client_id) {
                session.clients.push(client_id.to_string());
                session.clients.sort();
            }
            session.controller = Some(client_id.to_string());
            Ok(session.attachment_for(client_id))
        })
    }

    fn seat(&self, session_id: &str) -> Result<(bool, Option<String>), ApiError> {
        self.with_session(session_id, |session| {
            Ok((!session.clients.is_empty(), session.controller.clone()))
        })
    }

    fn stop(&self, session_id: &str) -> Result<bool, ApiError> {
        let (journal_session, generation) = {
            let mut sessions = self.lock();
            let Some(session) = sessions.get_mut(session_id) else {
                return Err(ApiError::new(
                    ErrorCode::UnknownSession,
                    format!("no native session {session_id} on this runtime"),
                ));
            };
            if session.ended {
                return Ok(false);
            }
            // The turn in flight is cancelled first: stopping a session whose
            // model call was still streaming would otherwise leave the runner
            // writing into a journal the operator has just ended.
            session.cancel.cancel();
            session.approvals.close();
            session.ended = true;
            session.ended_at = Some(state::now_secs());
            session.clients.clear();
            session.controller = None;
            // The registry entry goes with the conversation it described --
            // here, on the operator's explicit stop. `detach` never reaches
            // this, and neither does the service's own shutdown unless the
            // operator asked for `--stop-sessions`.
            session.guard.release();
            (session.journal_session.clone(), session.handle.generation)
        };
        {
            let mut backend = self.backend();
            if let Some(journal) = backend.journal_mut() {
                let _ = journal.complete_session(
                    &journal_session,
                    generation,
                    "stopped".to_string(),
                    state::now_secs(),
                );
            }
        }
        self.prune_ended();
        self.persist_topology();
        Ok(true)
    }
}

impl NativeSessions {
    /// Opens a new conversation: a fresh identity, a journal session, a
    /// registry record and a durable topology entry. Separate from
    /// [`NativeHost::start`] so the protocol-shaped error mapping stays in one
    /// place and this stays readable.
    fn open(&self, spec: &SessionSpec, role: &str) -> CtxResult<SessionFacts> {
        let route = spec.provider_route.as_ref().map(ToString::to_string);
        // Resolved BEFORE anything durable exists: a conversation pinned to a
        // route the operator never configured would be a session that can
        // never take a turn, and the honest place to say so is here.
        let route_identity = self
            .environment
            .route_identity(&spec.cwd, route.as_deref(), role)?;
        let mut backend = self.backend();
        let handle = backend.start(&SessionSpec {
            role: role.to_string(),
            ..spec.clone()
        })?;
        let journal_session = JournalSessionId::new(handle.logical_id.clone())?;
        let identity = SessionIdentity {
            session: journal_session.clone(),
            seat: SeatId::new(handle.short.clone())?,
            generation: handle.generation,
            task: None,
            route: route_identity,
            created_at: state::now_secs(),
            completed_at: None,
        };
        backend
            .journal_mut()
            .ok_or("native runtime: the journal was not attached")?
            .create_session(&identity)?;
        store_native_seat(
            &self.state,
            &handle,
            role,
            &identity.route,
            state::now_secs(),
        )?;
        backend.adopt(&handle, journal_session.clone())?;
        drop(backend);

        let guard = self.register(&handle, role, &spec.cwd);
        let (approvals, approval_prompts) =
            super::super::runtime::enforcement::InteractiveApprovals::new(
                Arc::new(super::super::runtime::enforcement::ApprovalAuthority::new()),
                format!("protocol {}", handle.short),
            );
        let session = NativeSession {
            handle: handle.clone(),
            journal_session,
            role: role.to_string(),
            cwd: spec.cwd.clone(),
            route,
            task: None,
            started_at: state::now_secs(),
            guard,
            cancel: Arc::new(CancellationFlag::default()),
            running: Arc::new(AtomicBool::new(false)),
            pending_wake: Arc::new(AtomicBool::new(false)),
            approvals,
            approval_prompts,
            pending_approvals: BTreeMap::new(),
            clients: Vec::new(),
            controller: None,
            restored_from: None,
            ended: false,
            ended_at: None,
        };
        let facts = session.facts();
        self.lock().insert(handle.logical_id.clone(), session);
        self.persist_topology();
        Ok(facts)
    }
}

/// Issue #489: the runtime's own [`dash::Injector`] for a conversation.
///
/// "Injecting" into a native session means recording a durable input, so the
/// message is in the journal before the mail file is consumed -- the ordering
/// the dashboard's `deliver_and_consume` already relies on, with a stronger
/// guarantee behind it than a pty write has.
struct ConversationInjector<'a> {
    host: &'a NativeSessions,
    session_id: String,
}

impl super::super::dash::Injector for ConversationInjector<'_> {
    fn try_inject(&mut self, label: &str, body: &str) -> CtxResult<()> {
        let text = format!("{label}\n{body}");
        // The delivered text IS the idempotency identity: a re-delivery of the
        // same message after a crash between the injection and the consume
        // hits the journal's uniqueness constraint instead of queueing a
        // second turn about the same mail.
        let key = format!("mail:{}:{}", self.session_id, text);
        self.host
            .submit(&self.session_id, &text, false, Some(&key))
            .map(|_| ())
            .map_err(|error| error.to_string().into())
    }
}

fn journal_of(backend: &NativeBackend) -> Result<&Journal, ApiError> {
    backend
        .journal()
        .ok_or_else(|| ApiError::new(ErrorCode::Internal, "the journal was not attached"))
}

fn receipt_state(outcome: TaskOutcome) -> TaskReceiptState {
    match outcome {
        TaskOutcome::Accepted => TaskReceiptState::Accepted,
        TaskOutcome::Started => TaskReceiptState::Started,
        TaskOutcome::Blocked => TaskReceiptState::Blocked,
        TaskOutcome::Failed => TaskReceiptState::Failed,
        TaskOutcome::Cancelled => TaskReceiptState::Cancelled,
        // `Unknown` never reaches here: the server refuses it as invalid
        // params before the host is called at all.
        TaskOutcome::Completed | TaskOutcome::Unknown => TaskReceiptState::Completed,
    }
}

/// Whether a caller's cursor can be continued from.
///
/// Pure, so the rule is provable without a database: a cursor of 0 always
/// works (start from the beginning), a cursor at or past the newest sequence
/// is simply "caught up", and anything below the OLDEST sequence this journal
/// still holds cannot be continued -- that caller has to resynchronize from a
/// history snapshot instead of applying a page it cannot place.
pub fn gap_at(after: u64, first: u64, last: u64) -> bool {
    if after == 0 || first == 0 {
        return false;
    }
    if after > last {
        // Ahead of the journal: whatever this cursor came from, it is not this
        // conversation as it now stands.
        return true;
    }
    after + 1 < first
}

fn event_kind(event: &JournalEvent) -> &'static str {
    match event {
        JournalEvent::InputAcknowledged { .. } => "input_acknowledged",
        JournalEvent::AssistantMessageCommitted { .. } => "assistant_message_committed",
        JournalEvent::UsageRecorded { .. } => "usage_recorded",
        JournalEvent::ToolCallPrepared { .. } => "tool_call_prepared",
        JournalEvent::ToolExecution { .. } => "tool_execution",
        JournalEvent::TaskReceipt { .. } => "task_receipt",
        JournalEvent::Checkpoint { .. } => "checkpoint",
        JournalEvent::GenerationAdvanced { .. } => "generation_advanced",
        JournalEvent::SessionEnded { .. } => "session_ended",
    }
}

/// The short, REDACTED descriptor one durable event publishes.
///
/// Deliberately never the payload: a tool call's arguments and a tool
/// execution's result routinely carry file contents and credentials, and this
/// stream is a "what happened" feed, not a transcript. Conversation text is
/// reachable only through `session.history`, which is seat-checked.
fn event_detail(event: &JournalEvent) -> Option<String> {
    match event {
        JournalEvent::InputAcknowledged { steering, .. } => {
            Some(if *steering { "steering" } else { "submit" }.to_string())
        }
        JournalEvent::ToolCallPrepared { name, .. } => Some(name.clone()),
        JournalEvent::ToolExecution { state, .. } => Some(
            match state {
                ExecutionState::Prepared => "prepared",
                ExecutionState::Started => "started",
                ExecutionState::Completed => "completed",
                ExecutionState::Failed => "failed",
                ExecutionState::Cancelled => "cancelled",
                ExecutionState::OutcomeUnknown => "outcome_unknown",
            }
            .to_string(),
        ),
        JournalEvent::TaskReceipt { receipt, .. }
            if receipt.get("kind").and_then(serde_json::Value::as_str)
                == Some("approval_request") =>
        {
            receipt
                .get("request_id")
                .and_then(serde_json::Value::as_str)
                .map(str::to_string)
        }
        JournalEvent::GenerationAdvanced { previous, current } => {
            Some(format!("{previous} -> {current}"))
        }
        JournalEvent::SessionEnded { reason } => Some(reason.clone()),
        _ => None,
    }
}

/// The conversation, reduced to what the protocol publishes. Tool entries name
/// the tool and nothing else -- see [`event_detail`] for why.
fn history_entries(state: &ConversationState, after: u64, limit: usize) -> Vec<HistoryEntry> {
    let mut entries: Vec<HistoryEntry> = Vec::new();
    for message in &state.messages {
        if message.sequence.0 <= after {
            continue;
        }
        let (role, text) = match message.role {
            MessageRole::User => (HistoryRole::User, message.text.clone().unwrap_or_default()),
            MessageRole::Assistant => (
                HistoryRole::Assistant,
                message
                    .blocks
                    .iter()
                    .filter_map(|block| match block {
                        AssistantBlock::Text { text } | AssistantBlock::Refusal { text } => {
                            Some(text.as_str())
                        }
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join("\n"),
            ),
        };
        entries.push(HistoryEntry {
            sequence: message.sequence.0,
            role,
            text,
            steering: message.steering,
        });
        for block in &message.blocks {
            if let AssistantBlock::ToolCall { tool_call } = block
                && let Some(call) = state.tool_calls.get(tool_call)
            {
                entries.push(HistoryEntry {
                    sequence: call.sequence.0,
                    role: HistoryRole::Tool,
                    text: call.name.clone(),
                    steering: false,
                });
            }
        }
    }
    entries.sort_by_key(|entry| entry.sequence);
    entries.truncate(limit);
    entries
}

/// The runtime-contract failures a backend reports, mapped onto the published
/// error codes -- the same four `runtime::protocol::dispatch` distinguishes,
/// so a native refusal reads identically wherever a caller meets it.
fn backend_error(error: &(dyn std::error::Error + 'static)) -> ApiError {
    use super::super::runtime::RuntimeError;

    match error.downcast_ref::<RuntimeError>() {
        Some(RuntimeError::Unsupported(what)) => ApiError::new(ErrorCode::Unsupported, what),
        Some(RuntimeError::UnknownSession(id)) => {
            ApiError::new(ErrorCode::UnknownSession, format!("unknown session: {id}"))
        }
        Some(RuntimeError::Busy(id)) => ApiError::new(
            ErrorCode::Busy,
            format!("a turn is already in flight for {id}"),
        ),
        Some(RuntimeError::StaleGeneration { expected, got }) => ApiError::new(
            ErrorCode::StaleGeneration,
            format!("session is at generation {expected}, not {got}"),
        ),
        None => ApiError::new(ErrorCode::Internal, error.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::ctx::provider::Protocol;
    use crate::commands::ctx::runtime::fixture::fixture_target;
    use crate::commands::ctx::runtime::journal::{
        EventScope, ExecutionId, PolicyProvenance, ToolCallId,
    };

    /// A native environment with no provider, no credential and no network: a
    /// fixed route identity, and a turn runner that only records that it was
    /// asked to run. Everything this module actually owns -- identity,
    /// durability, seats, cursors, reconciliation -- is then exercised for
    /// real, against a real journal and a real session registry.
    #[derive(Debug, Default)]
    struct TestEnvironment {
        runs: Mutex<Vec<QueuedTurn>>,
    }

    impl NativeEnvironment for TestEnvironment {
        fn route_identity(
            &self,
            _repo: &Path,
            _route: Option<&str>,
            _role: &str,
        ) -> CtxResult<RouteIdentity> {
            let target = fixture_target(Protocol::AnthropicMessages, "test-model");
            Ok(RouteIdentity {
                route: target.route.clone(),
                provider: target.provider.clone(),
                endpoint: target.endpoint.clone(),
                account: target.account.clone(),
                billing_pool: target.billing_pool.clone(),
                protocol: target.protocol,
                model: target.model.clone(),
            })
        }

        fn run(&self, turn: &QueuedTurn) -> CtxResult<()> {
            match self.runs.lock() {
                Ok(mut runs) => runs.push(turn.clone()),
                Err(poisoned) => poisoned.into_inner().push(turn.clone()),
            }
            Ok(())
        }
    }

    impl TestEnvironment {
        fn runs(&self) -> Vec<QueuedTurn> {
            match self.runs.lock() {
                Ok(runs) => runs.clone(),
                Err(poisoned) => poisoned.into_inner().clone(),
            }
        }
    }

    #[derive(Debug)]
    struct ApprovalEnvironment {
        state: StateDir,
        executions: AtomicUsize,
    }

    impl NativeEnvironment for ApprovalEnvironment {
        fn route_identity(
            &self,
            _repo: &Path,
            _route: Option<&str>,
            _role: &str,
        ) -> CtxResult<RouteIdentity> {
            TestEnvironment::default().route_identity(Path::new("."), None, "worker")
        }

        fn run(&self, turn: &QueuedTurn) -> CtxResult<()> {
            use crate::commands::ctx::runtime::enforcement::{
                ApprovalOutcome, ApprovalRequest, ExecutionAction, ExecutionIdentity,
                GenerationFence, StoredSeatFence,
            };

            let identity = ExecutionIdentity {
                session: turn.session.clone(),
                short: turn.seat_short.clone(),
                generation: turn.generation,
                role: turn.role.clone(),
                task: turn.task.clone(),
            };
            StoredSeatFence::new(self.state.clone()).verify(&identity)?;
            let action = ExecutionAction::ReadFile {
                path: turn.cwd.join("approved.txt"),
            };
            let policy_fingerprint = "policy-1".to_string();
            let claims_fingerprint = "claims-1".to_string();
            let resolved_paths = vec![turn.cwd.join("approved.txt")];
            let execution_scope_fingerprint = "scope-1".to_string();
            #[derive(Serialize)]
            struct Scope<'a> {
                identity: &'a ExecutionIdentity,
                action: &'a ExecutionAction,
                policy_fingerprint: &'a str,
                claims_fingerprint: &'a str,
                resolved_paths: &'a [PathBuf],
                execution_scope_fingerprint: &'a str,
            }
            use sha2::Digest;
            let digest = sha2::Sha256::digest(serde_json::to_vec(&Scope {
                identity: &identity,
                action: &action,
                policy_fingerprint: &policy_fingerprint,
                claims_fingerprint: &claims_fingerprint,
                resolved_paths: &resolved_paths,
                execution_scope_fingerprint: &execution_scope_fingerprint,
            })?);
            let request = ApprovalRequest {
                scope_digest: digest.iter().map(|byte| format!("{byte:02x}")).collect(),
                identity,
                action,
                policy_fingerprint,
                claims_fingerprint,
                resolved_paths,
                execution_scope_fingerprint,
                created_at: state::now_secs(),
            };
            if matches!(
                turn.approvals.request(&request, state::now_secs()),
                ApprovalOutcome::Granted(_)
            ) {
                self.executions.fetch_add(1, Ordering::SeqCst);
            }
            Ok(())
        }
    }

    fn approval_host(root: &Path) -> (Arc<NativeSessions>, Arc<ApprovalEnvironment>) {
        let state = StateDir::from_root(root.join("state"));
        let environment = Arc::new(ApprovalEnvironment {
            state: state.clone(),
            executions: AtomicUsize::new(0),
        });
        let host = NativeSessions::new(
            state,
            "default",
            "instance-approval",
            Arc::clone(&environment) as Arc<dyn NativeEnvironment>,
        )
        .expect("native host");
        (host, environment)
    }

    fn wait_for_approval(host: &NativeSessions, session_id: &str, after: u64) -> (String, u64) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            let page = host.journal(session_id, after, 64).expect("journal page");
            if let Some(event) = page
                .events
                .iter()
                .find(|event| event.kind == "task_receipt" && event.detail.is_some())
            {
                return (event.detail.clone().expect("request id"), page.cursor);
            }
            assert!(
                std::time::Instant::now() < deadline,
                "approval request was never published"
            );
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }

    fn wait_for_native_idle(host: &NativeSessions, session_id: &str) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            if host
                .sessions()
                .iter()
                .any(|facts| facts.session_id == session_id && facts.state == SessionState::Idle)
            {
                return;
            }
            assert!(std::time::Instant::now() < deadline, "turn stayed busy");
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }

    #[derive(Debug)]
    struct ShutdownBoundaryEnvironment {
        checked: std::sync::Barrier,
        release: std::sync::Barrier,
        runs: AtomicUsize,
    }

    impl NativeEnvironment for ShutdownBoundaryEnvironment {
        fn route_identity(
            &self,
            repo: &Path,
            route: Option<&str>,
            role: &str,
        ) -> CtxResult<RouteIdentity> {
            TestEnvironment::default().route_identity(repo, route, role)
        }

        fn run(&self, _turn: &QueuedTurn) -> CtxResult<()> {
            if self.runs.fetch_add(1, Ordering::SeqCst) == 0 {
                self.checked.wait();
                self.release.wait();
            }
            Ok(())
        }
    }

    fn host_for(root: &Path) -> (Arc<NativeSessions>, Arc<TestEnvironment>) {
        let state = StateDir::from_root(root.join("state"));
        let environment = Arc::new(TestEnvironment::default());
        let host = NativeSessions::new(
            state,
            "default",
            "instance-1",
            Arc::clone(&environment) as Arc<dyn NativeEnvironment>,
        )
        .expect("native host");
        host.run_turns_inline_for_test();
        (host, environment)
    }

    fn spec(cwd: &Path, prompt: &str) -> SessionSpec {
        SessionSpec {
            runtime: RuntimeKind::Native,
            role: "orchestrator".to_string(),
            agent: None,
            provider_route: None,
            model: None,
            surface: UiSurface::Headless,
            cwd: cwd.to_path_buf(),
            prompt: prompt.to_string(),
            extra_args: Vec::new(),
        }
    }

    /// Issue #489, criterion 1, for the half this module owns: a client going
    /// away is a CLIENT lifecycle event. The conversation keeps its registry
    /// record -- the mechanical reason pacing, budgets, rot scoring, mail
    /// addressing and writer permits keep seeing it -- and nothing is
    /// relaunched, because there was never a process to relaunch.
    #[test]
    fn detaching_every_client_leaves_the_conversation_and_its_registry_record() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let (host, _) = host_for(tmp.path());
        let facts = host.start(&spec(tmp.path(), "")).expect("start");

        host.attach(&facts.session_id, "client-1", AttachMode::Controller)
            .expect("attach");
        let state = StateDir::from_root(tmp.path().join("state"));
        assert!(
            sessions::list(&state)
                .iter()
                .any(|(record, _)| record.session == facts.session_id),
            "a runtime-owned native session files a registry record"
        );

        let attachment = host.detach(&facts.session_id, "client-1").expect("detach");
        assert_eq!(attachment.role, AttachRole::Detached);
        assert!(
            host.owns(&facts.session_id),
            "the conversation is still here"
        );
        assert!(
            sessions::list(&state)
                .iter()
                .any(|(record, _)| record.session == facts.session_id),
            "and so is its registry record: detach is not stop"
        );
        assert_eq!(
            NativeHost::sessions(host.as_ref())[0].state,
            SessionState::Idle
        );
    }

    /// Issue #489, criterion 2: a retried input carrying the same idempotency
    /// key is deduplicated by the JOURNAL, not by a cache -- so the guarantee
    /// survives the reconnect (and the service restart) that loses every cache
    /// there is. Proven by the durable message count, never by the reply.
    #[test]
    fn a_retried_input_with_the_same_key_is_recorded_once() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let (host, environment) = host_for(tmp.path());
        let facts = host.start(&spec(tmp.path(), "")).expect("start");

        let first = host
            .submit(&facts.session_id, "do the thing", false, Some("retry-1"))
            .expect("submit");
        let second = host
            .submit(&facts.session_id, "do the thing", false, Some("retry-1"))
            .expect("retry");

        assert!(!first.duplicate);
        assert!(
            second.duplicate,
            "the retry must be reported as a duplicate"
        );
        assert_eq!(
            first.message_id, second.message_id,
            "and under the same durable identity"
        );

        let history = host.history(&facts.session_id, 0, 64).expect("history");
        let inputs = history
            .entries
            .iter()
            .filter(|entry| entry.role == HistoryRole::User)
            .count();
        assert_eq!(
            inputs, 1,
            "one input on disk, not two: {:?}",
            history.entries
        );
        assert_eq!(
            environment.runs().len(),
            1,
            "and exactly one turn was queued"
        );
    }

    /// Issue #548: protocol approvals are observable, consumed once, and
    /// bound to the exact pending action.
    #[test]
    fn hosted_native_tools_require_and_consume_protocol_approval() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let (host, environment) = approval_host(tmp.path());
        let facts = host.start(&spec(tmp.path(), "")).expect("start");

        host.submit(&facts.session_id, "first", false, None)
            .expect("first submit");
        let (request_id, cursor) = wait_for_approval(&host, &facts.session_id, 0);
        assert_eq!(environment.executions.load(Ordering::SeqCst), 0);
        assert!(
            host.approve(
                &facts.session_id,
                &request_id,
                ApprovalDecision::Deny,
                Some("no")
            )
            .expect("deny")
        );
        wait_for_native_idle(&host, &facts.session_id);
        assert_eq!(environment.executions.load(Ordering::SeqCst), 0);

        host.submit(&facts.session_id, "second", false, None)
            .expect("second submit");
        let (request_id, cursor) = wait_for_approval(&host, &facts.session_id, cursor);
        assert!(
            host.approve(
                &facts.session_id,
                &request_id,
                ApprovalDecision::Allow,
                None
            )
            .expect("allow")
        );
        wait_for_native_idle(&host, &facts.session_id);
        assert_eq!(environment.executions.load(Ordering::SeqCst), 1);

        host.submit(&facts.session_id, "third", false, None)
            .expect("third submit");
        let (request_id, _) = wait_for_approval(&host, &facts.session_id, cursor);
        assert_eq!(environment.executions.load(Ordering::SeqCst), 1);
        host.approve(&facts.session_id, &request_id, ApprovalDecision::Deny, None)
            .expect("cleanup denial");
    }

    /// Issue #548: opening the hosted session installs the exact native seat
    /// before its first approved effect reaches the generation fence.
    #[test]
    fn fresh_hosted_native_session_installs_its_seat_before_tool_dispatch() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let (host, environment) = approval_host(tmp.path());
        let facts = host.start(&spec(tmp.path(), "")).expect("start");
        let state = StateDir::from_root(tmp.path().join("state"));
        let seat = super::super::super::seat::load(&state, &facts.short).expect("native seat");
        assert_eq!(seat.runtime, RuntimeKind::Native);
        assert_eq!(seat.generation, facts.generation);

        host.submit(&facts.session_id, "run", false, None)
            .expect("submit");
        let (request_id, _) = wait_for_approval(&host, &facts.session_id, 0);
        host.approve(
            &facts.session_id,
            &request_id,
            ApprovalDecision::Allow,
            None,
        )
        .expect("approval");
        wait_for_native_idle(&host, &facts.session_id);
        assert_eq!(environment.executions.load(Ordering::SeqCst), 1);
    }

    /// Issue #578: input acknowledged after the runner's last queue check
    /// starts a successor without requiring another external wake.
    #[test]
    fn input_acknowledged_during_runner_shutdown_is_driven() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().join("state"));
        let environment = Arc::new(ShutdownBoundaryEnvironment {
            checked: std::sync::Barrier::new(2),
            release: std::sync::Barrier::new(2),
            runs: AtomicUsize::new(0),
        });
        let host = NativeSessions::new(
            state,
            "default",
            "instance-boundary",
            Arc::clone(&environment) as Arc<dyn NativeEnvironment>,
        )
        .expect("host");
        let facts = host.start(&spec(tmp.path(), "")).expect("start");

        host.submit(&facts.session_id, "first", false, None)
            .expect("first submit");
        environment.checked.wait();
        host.submit(&facts.session_id, "second", false, None)
            .expect("boundary submit");
        environment.release.wait();
        wait_for_native_idle(&host, &facts.session_id);

        assert_eq!(
            environment.runs.load(Ordering::SeqCst),
            2,
            "the acknowledged boundary input must drive a successor turn"
        );
        let history = host.history(&facts.session_id, 0, 64).expect("history");
        assert_eq!(
            history
                .entries
                .iter()
                .filter(|entry| entry.role == HistoryRole::User)
                .count(),
            2
        );
    }

    /// Issue #489, item 5 and criterion 1: cancel and stop are different
    /// verbs. An interrupt ends the turn; the conversation, its journal and
    /// its registry record survive it, and only `stop` releases them.
    #[test]
    fn an_interrupt_cancels_the_turn_and_a_stop_ends_the_conversation() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let (host, _) = host_for(tmp.path());
        let facts = host.start(&spec(tmp.path(), "")).expect("start");

        // No turn in flight (the inline runner already returned), so there is
        // nothing to cancel -- reported honestly rather than as a success.
        assert!(!host.interrupt(&facts.session_id).expect("interrupt"));
        assert!(host.owns(&facts.session_id));

        assert!(host.stop(&facts.session_id).expect("stop"));
        let state = StateDir::from_root(tmp.path().join("state"));
        assert!(
            !sessions::list(&state)
                .iter()
                .any(|(record, _)| record.session == facts.session_id),
            "stop releases the registry record; detach never does"
        );
        assert!(
            !host.stop(&facts.session_id).expect("second stop"),
            "a second stop reports false rather than failing"
        );
    }

    /// Issue #489, criterion 4 and item 6: after a service restart the
    /// conversation comes back from durable state, every execution that was
    /// merely `Started` becomes outcome-unknown, and NOTHING is replayed.
    #[test]
    fn a_restart_reconciles_started_executions_instead_of_replaying_them() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let (host, environment) = host_for(tmp.path());
        let facts = host.start(&spec(tmp.path(), "")).expect("start");
        host.submit(&facts.session_id, "run a tool", false, None)
            .expect("submit");

        // An effect that began and never reported: exactly the shape a killed
        // runtime leaves behind.
        {
            let mut backend = host.backend();
            let journal = backend.journal_mut().expect("journal");
            let session = JournalSessionId::new(facts.session_id.clone()).expect("id");
            let call = ToolCallId::new("call-1").expect("id");
            let execution = ExecutionId::new("exec-1").expect("id");
            journal
                .prepare_tool_call(
                    &session,
                    1,
                    &EventScope::default(),
                    call.clone(),
                    "shell".to_string(),
                    serde_json::json!({"command": "echo hi"}),
                    PolicyProvenance {
                        fingerprint: "fp".to_string(),
                        source: "test".to_string(),
                        decision: "allow".to_string(),
                        scope: "repo".to_string(),
                    },
                    None,
                    1,
                )
                .expect("prepare call");
            journal
                .prepare_execution(
                    &session,
                    1,
                    &EventScope::default(),
                    execution.clone(),
                    call,
                    None,
                    1,
                )
                .expect("prepare execution");
            journal
                .transition_execution(
                    &session,
                    1,
                    &EventScope::default(),
                    &execution,
                    ExecutionState::Started,
                    None,
                    None,
                    None,
                    1,
                )
                .expect("start execution");
        }
        let before = environment.runs().len();

        // A successor service over the same state directory and the same
        // journal: a restart, without killing the test's own process.
        let state = StateDir::from_root(tmp.path().join("state"));
        let successor = NativeSessions::new(
            state,
            "default",
            "instance-2",
            Arc::clone(&environment) as Arc<dyn NativeEnvironment>,
        )
        .expect("successor");
        let report = successor.restore();

        assert_eq!(report.resumed, vec![facts.session_id.clone()]);
        assert_eq!(
            report.outcome_unknown,
            vec!["exec-1".to_string()],
            "a started execution is reported outcome-unknown, never retried"
        );
        assert!(report.lost.is_empty(), "{:?}", report.lost);
        assert_eq!(
            environment.runs().len(),
            before,
            "a restore submits nothing: it reconciles durable state, it does not replay commands"
        );
        assert_eq!(
            successor.restored_from(&facts.session_id),
            Some(1),
            "and the predecessor generation is recorded, never republished"
        );
    }

    /// Issue #489, item 3: the cursor contract. A page starts strictly after
    /// the caller's cursor, carries the next one, and never carries a payload.
    #[test]
    fn a_journal_page_pages_by_cursor_and_publishes_no_payloads() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let (host, _) = host_for(tmp.path());
        let facts = host.start(&spec(tmp.path(), "")).expect("start");
        for n in 0..4 {
            host.submit(&facts.session_id, &format!("input {n}"), false, None)
                .expect("submit");
        }

        let first = host.journal(&facts.session_id, 0, 2).expect("page");
        assert_eq!(first.events.len(), 2);
        assert!(!first.gap);
        assert_eq!(first.cursor, first.events[1].sequence);
        let second = host
            .journal(&facts.session_id, first.cursor, 2)
            .expect("page");
        assert!(
            second
                .events
                .iter()
                .all(|event| event.sequence > first.cursor),
            "a page starts strictly after the cursor"
        );
        assert_eq!(second.last_sequence, first.last_sequence);
        for event in first.events.iter().chain(second.events.iter()) {
            assert_eq!(event.kind, "input_acknowledged");
            assert_eq!(event.detail.as_deref(), Some("submit"));
        }
    }

    /// The gap rule, pure: a cursor the journal can no longer start from is a
    /// resynchronization signal, not a page.
    #[test]
    fn a_cursor_the_journal_cannot_continue_from_is_a_gap() {
        assert!(
            !gap_at(0, 5, 9),
            "starting from the beginning is never a gap"
        );
        assert!(
            !gap_at(5, 5, 9),
            "a cursor inside the retained range is fine"
        );
        assert!(!gap_at(4, 5, 9), "and so is the one immediately before it");
        assert!(
            gap_at(2, 5, 9),
            "a cursor below the oldest retained event is a gap"
        );
        assert!(
            gap_at(11, 5, 9),
            "and so is one ahead of the journal itself"
        );
    }

    /// Issue #489, item 4: many observers, one controller -- the same rule the
    /// pty host enforces, for a session that has no terminal.
    #[test]
    fn many_clients_may_observe_a_conversation_but_only_one_may_drive_it() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let (host, _) = host_for(tmp.path());
        let facts = host.start(&spec(tmp.path(), "")).expect("start");

        host.attach(&facts.session_id, "driver", AttachMode::Controller)
            .expect("controller");
        for observer in ["watch-1", "watch-2"] {
            let attachment = host
                .attach(&facts.session_id, observer, AttachMode::Observer)
                .expect("observer");
            assert_eq!(attachment.role, AttachRole::Observer);
        }
        let busy = host
            .attach(&facts.session_id, "usurper", AttachMode::Controller)
            .expect_err("a second controller is refused");
        assert_eq!(busy.code, ErrorCode::Busy);

        let (attached, controller) = host.seat(&facts.session_id).expect("seat");
        assert!(attached);
        assert_eq!(controller.as_deref(), Some("driver"));

        let taken = host
            .takeover(&facts.session_id, "usurper")
            .expect("takeover");
        assert_eq!(taken.controller.as_deref(), Some("usurper"));
    }

    /// A stopped conversation stays visible for a bounded while and then goes.
    /// Live ones are never pruned: this bounds history, not concurrency.
    #[test]
    fn stopped_conversations_are_kept_bounded_and_live_ones_are_never_pruned() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let (host, _) = host_for(tmp.path());
        host.set_ended_cap_for_test(1);
        for _ in 0..3 {
            let facts = host.start(&spec(tmp.path(), "")).expect("start");
            host.stop(&facts.session_id).expect("stop");
        }
        let live = host.start(&spec(tmp.path(), "")).expect("start");

        let held = NativeHost::sessions(host.as_ref());
        assert_eq!(
            held.iter()
                .filter(|facts| facts.state == SessionState::Ended)
                .count(),
            1,
            "the ended table is capped"
        );
        assert!(
            held.iter().any(|facts| facts.session_id == live.session_id),
            "a live conversation is never pruned"
        );
    }

    /// Issue #352's mail-injection residual, closed for conversations: mail
    /// addressed to a session NOBODY is attached to is delivered by the
    /// SERVICE, not left in the queue until a client shows up.
    ///
    /// Delivery is durable and idempotent: the message becomes a journalled
    /// input, the mail file is consumed only because that succeeded, and a
    /// second sweep of the same body records nothing twice.
    #[test]
    fn mail_for_a_detached_conversation_is_delivered_rather_than_queued() {
        use crate::commands::ctx::config::CtxConfig;
        use crate::commands::ctx::mail;

        let tmp = tempfile::tempdir().expect("tempdir");
        let (host, _) = host_for(tmp.path());
        let facts = host.start(&spec(tmp.path(), "")).expect("start");
        let state = StateDir::from_root(tmp.path().join("state"));
        let slug = state::repo_slug(tmp.path());
        let cfg = CtxConfig::default();

        mail::store(
            &state,
            &slug,
            &mail::Message {
                from_session: "11111111-2222-4333-8444-555555555555".to_string(),
                from_agent: "claude".to_string(),
                to: RuntimeKind::Native.as_str().to_string(),
                to_session: Some(facts.short.clone()),
                sent: state::now_secs(),
                body: "the build is red".to_string(),
            },
            &cfg,
        )
        .expect("store");
        assert_eq!(
            mail::list(&state, &slug, None, Some(&facts.short))
                .expect("list")
                .len(),
            1,
            "the message starts out queued, with nobody attached"
        );

        let mut errors = crate::commands::ctx::dash::ErrorLog::default();
        host.deliver_mail(&cfg, &mut errors);

        assert!(
            mail::list(&state, &slug, None, Some(&facts.short))
                .expect("list")
                .is_empty(),
            "the service delivered it; a message is consumed only when the delivery succeeded"
        );
        let history = host.history(&facts.session_id, 0, 64).expect("history");
        assert!(
            history
                .entries
                .iter()
                .any(|entry| entry.role == HistoryRole::User
                    && entry.text.contains("the build is red")),
            "and it is a durable input on the conversation: {:?}",
            history.entries
        );

        // A re-delivery of the same body -- what a crash between the injection
        // and the consume leaves behind -- records nothing twice.
        let before = history.entries.len();
        let injected = host
            .submit(
                &facts.session_id,
                &history
                    .entries
                    .iter()
                    .rev()
                    .find(|entry| entry.role == HistoryRole::User)
                    .map(|entry| entry.text.clone())
                    .expect("the delivered text"),
                false,
                Some(&format!(
                    "mail:{}:{}",
                    facts.session_id,
                    history
                        .entries
                        .iter()
                        .rev()
                        .find(|entry| entry.role == HistoryRole::User)
                        .map(|entry| entry.text.clone())
                        .expect("the delivered text")
                )),
            )
            .expect("redeliver");
        assert!(injected.duplicate, "a re-delivery is a duplicate");
        assert_eq!(
            host.history(&facts.session_id, 0, 64)
                .expect("history")
                .entries
                .len(),
            before
        );
    }
}
