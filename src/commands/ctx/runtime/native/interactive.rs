//! [`InteractiveSession`]: a dashboard native pane's multi-turn worker.

use std::sync::{Arc, mpsc};

use super::super::super::CtxResult;
use super::super::super::config::EnvLookup;
use super::super::super::lifecycle;
use super::super::super::provider::adapter::CancellationFlag;
use super::super::super::state::now_ms;
use super::super::compaction::{DistillBudget, NativeBudget, RETAIN_RECENT_MESSAGES};
use super::super::journal::{Journal, JournalSessionId, RouteIdentity};
use super::super::{RuntimeBackend, RuntimeKind, SessionHandle, SessionSpec, UiSurface};
use super::backend::NativeBackend;
use super::headless::{
    Accounting, HeadlessRequest, brokered_tools, build_transport, compile_standing_context,
};
use super::hosted::{apply_proxy_first_turn, task_for_config};
use super::turn::NativeLoop;
use super::types::{
    CompactionSettings, NativeLimits, NativeSessionConfig, RecompileContext, SessionState,
};

// -- an interactive, multi-turn session for a dashboard native pane --------
//
// Issue #480 (roadmap N11): everything above this point runs ONE submitted
// prompt to completion and exits (`run_headless`/`run_session`) -- exactly
// right for `zirv ctx exec --runtime native` and for a delegated worker
// (`native_worker.rs`), wrong for a dashboard pane an operator keeps typing
// into across many turns. [`spawn_interactive`] resolves transport/journal/
// seat/writer exactly like `run_session` does, then hands the session to a
// background OS thread that constructs a fresh [`NativeLoop`] and calls
// `run_to_completion` once per submitted turn, looping for the pane's whole
// lifetime instead of once. `dash::native_pane` never constructs a
// `NativeLoop` itself and never reads a provider/tool-executor directly --
// this is the one seam between the dashboard and this module.

/// What a dashboard native pane needs to open a session. Owned (no borrowed
/// lifetime) so it can be built on the caller's thread and then moved,
/// whole, into the worker thread this spawns.
pub struct InteractiveRequest {
    pub repo: std::path::PathBuf,
    pub role: String,
    pub route: Option<String>,
    pub limits: NativeLimits,
    /// The shared task card a delegated launch already carries (N10); a
    /// plain operator-opened pane has none.
    pub task: Option<String>,
    /// `true` acquires a writer permit for `repo` (issue #358's own
    /// per-tree ledger, `permit::acquire_writer`) so this session's tool
    /// calls can actually write -- the same ownership step
    /// `native_worker.rs`'s `WorkerMode::Writing` already takes. `false`
    /// mirrors a read-only session: every write this session's tools
    /// attempt is refused by the execution broker, on purpose.
    pub writing: bool,
    /// Optional provider override lets a fixture-backed session bypass the
    /// operator route; production callers use the configured provider. (#531)
    pub provider: Option<String>,
    /// Optional seat takeover retains the stable short ID and advances generation; ordinary panes mint a new seat. (#552)
    pub seat: Option<(String, u64)>,
}

/// Signals when to reread the journal and carries failures that occurred before any durable event was committed.
#[derive(Debug, Clone, PartialEq)]
pub enum InteractiveProgress {
    /// A submitted turn started running.
    Busy,
    /// A turn finished (however it finished -- completed, interrupted, hit
    /// a limit); the journal has whatever it is going to have.
    Idle,
    /// The turn could not even be started (a transport/journal error, not a
    /// provider failure -- a provider failure is a normal journaled
    /// `Failed` turn and reaches `Idle` instead).
    Failed(String),
    /// Reports a session-level context failure on the status line while the
    /// session continues. (#531)
    Notice(String),
    /// The worker thread's loop has exited; no more progress will ever
    /// follow. Sent once, always last.
    Ended,
}

/// A live, in-process native session a dashboard pane drives. Read-only
/// handles (`session`, `route`, `cancel`) are `Clone`/`Arc`-cheap to hand to
/// `dash::native_pane`'s own presentation code; `submit`/`interrupt`/
/// `shutdown` are the entire control surface -- there is no fourth way to
/// reach the worker thread.
pub struct InteractiveSession {
    pub handle: SessionHandle,
    pub session: JournalSessionId,
    pub route: RouteIdentity,
    /// Shared with the worker's own `NativeLoop`: calling `.cancel()` here
    /// reaches an in-flight turn without going through the channel at all,
    /// the same direct route `NativeBackend::interrupt` already documents
    /// for "a caller that drives a `NativeLoop` itself".
    pub cancel: Arc<CancellationFlag>,
    /// Approval gate for this session's broker; pane responses complete it, and interrupt cancels it. (#490)
    approvals: Arc<super::super::enforcement::InteractiveApprovals>,
    approval_prompts: mpsc::Receiver<super::super::enforcement::ApprovalPrompt>,
    submit_tx: mpsc::Sender<String>,
    progress_rx: mpsc::Receiver<InteractiveProgress>,
    worker: Option<std::thread::JoinHandle<()>>,
    writer_permit_held: Arc<std::sync::atomic::AtomicBool>,
}

#[derive(Debug)]
struct ObservedWriterLease {
    inner: Box<dyn super::super::enforcement::WriterLease>,
    held: Arc<std::sync::atomic::AtomicBool>,
}

impl super::super::enforcement::WriterLease for ObservedWriterLease {
    fn covers(&self, worktree: &std::path::Path) -> bool {
        self.inner.covers(worktree)
    }
}

impl Drop for ObservedWriterLease {
    fn drop(&mut self) {
        self.held.store(false, std::sync::atomic::Ordering::Release);
    }
}

impl std::fmt::Debug for InteractiveSession {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("InteractiveSession")
            .field("handle", &self.handle)
            .field("session", &self.session)
            .finish_non_exhaustive()
    }
}

impl InteractiveSession {
    /// Queues one turn's input. This is the ONLY path a fresh (idle) turn
    /// starts from; mid-turn steering does not go through this channel at
    /// all -- see `dash::native_pane`'s own steering note -- because the
    /// worker thread is synchronously blocked inside `run_to_completion`
    /// while a turn runs and cannot service it. `queued_input` is the loop's
    /// own re-poll of the journal between turns, which is how a `Steer`
    /// written directly to the journal by the caller is picked up without
    /// this channel's involvement.
    pub fn submit(&self, text: String) -> Result<(), mpsc::SendError<String>> {
        self.submit_tx.send(text)
    }

    /// Every progress update queued since the last call, oldest first.
    /// Never blocks.
    pub fn drain_progress(&self) -> Vec<InteractiveProgress> {
        let mut out = Vec::new();
        while let Ok(progress) = self.progress_rx.try_recv() {
            out.push(progress);
        }
        out
    }

    /// The next approval request this session's broker has raised, if any.
    /// Never blocks: the pane polls it once per tick, the same way it polls
    /// progress.
    pub fn next_approval(&self) -> Option<super::super::enforcement::ApprovalPrompt> {
        self.approval_prompts.try_recv().ok()
    }

    pub fn holds_writer_permit(&self) -> bool {
        self.writer_permit_held
            .load(std::sync::atomic::Ordering::Acquire)
    }

    #[cfg(test)]
    pub fn cancellation_flag(&self) -> Arc<CancellationFlag> {
        Arc::clone(&self.cancel)
    }

    /// Interrupt cancels an approval dialog with the tool call, so the worker cannot remain parked after the turn ends. (#490)
    pub fn interrupt(&self) {
        self.approvals.cancel();
        self.cancel.cancel();
    }

    /// Starts session shutdown without waiting for the worker thread. The
    /// dashboard uses this on its event loop, then polls
    /// [`Self::try_finish_shutdown`] on later ticks.
    pub fn request_shutdown(&mut self) {
        self.approvals.close();
        self.cancel.cancel();
        drop(std::mem::replace(&mut self.submit_tx, mpsc::channel().0));
    }

    /// Reaps a worker that has already terminated. Never blocks.
    pub fn try_finish_shutdown(&mut self) -> bool {
        let Some(worker) = self.worker.as_ref() else {
            return true;
        };
        if !worker.is_finished() {
            return false;
        }
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
        true
    }

    #[cfg(test)]
    pub fn replace_worker_for_test(&mut self, worker: std::thread::JoinHandle<()>) -> bool {
        self.request_shutdown();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while !self.try_finish_shutdown() && std::time::Instant::now() < deadline {
            std::thread::yield_now();
        }
        if self.worker.is_some() {
            return false;
        }
        self.worker = Some(worker);
        true
    }

    /// Ends the session: cancels any turn currently in flight, drops the
    /// submit channel (the worker's `for text in submit_rx` loop exits on
    /// its next iteration since a disconnected channel reads as "no more
    /// messages" rather than blocking forever), then joins the thread so a
    /// quitting dashboard never leaves an orphan running -- bounded, so a
    /// provider that never returns cannot hang the caller forever either.
    /// Consumes `self` -- there is nothing left to submit to afterward.
    ///
    /// PR #531 review finding 1 (blocker): this used to join unconditionally,
    /// with no cancellation at all -- quitting the pane mid-turn blocked on
    /// `worker.join()` for however long the in-flight turn's own provider
    /// call took, holding the writer permit and the seat record open the
    /// whole time. Two changes fix it:
    ///
    /// 1. `self.cancel.cancel()` runs FIRST, before the channel is even
    ///    dropped -- the exact mechanism [`Self::interrupt`] already uses,
    ///    so a turn that is mid-request winds down the same bounded way a
    ///    live `Ctrl+C` does, rather than running to its own natural
    ///    completion.
    /// 2. The join itself is bounded ([`SHUTDOWN_JOIN_TIMEOUT`]). A worker
    ///    that still has not exited after cancellation -- a provider bug
    ///    that never checks cancellation at all -- is detached rather than
    ///    waited on forever: a logged warning, and the `JoinHandle` is
    ///    simply dropped (which does not kill the OS thread, only stops
    ///    tracking it; it keeps running to whatever end it eventually
    ///    reaches, still holding its own writer permit and seat record
    ///    until then). The two ordinary paths this session ever actually
    ///    takes -- a turn already idle, or a turn cancelled and winding down
    ///    promptly -- both finish well inside the bound, so in practice this
    ///    always takes the fast path: the worker's own end-of-loop cleanup
    ///    (`journal.complete_session`, then dropping `tools`/the writer
    ///    permit as the closure returns) runs before `shutdown` returns.
    pub fn shutdown(mut self) {
        self.request_shutdown();
        if let Some(worker) = self.worker.take() {
            join_worker_with_timeout(worker, SHUTDOWN_JOIN_TIMEOUT);
        }
    }
}

/// How long [`InteractiveSession::shutdown`] waits for the worker thread to
/// exit, once cancelled, before giving up and detaching it. Generous enough
/// that a turn genuinely winding down (a provider finishing its current
/// chunk, a tool call being abandoned mid-flight) has time to, but bounded
/// so a caller tearing down a dashboard pane is never held hostage by a
/// provider bug that ignores cancellation outright.
const SHUTDOWN_JOIN_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// Joins `worker`, polling rather than blocking so the wait can be bounded
/// (`std::thread::JoinHandle` has no built-in timed join). A worker still
/// running once `timeout` elapses is left detached -- dropping the handle
/// stops tracking it without killing it, the only safe option in std Rust --
/// with a warning on stderr, the same "log to stderr" convention this
/// module's own callers already use for a degraded-but-not-fatal condition.
fn join_worker_with_timeout(worker: std::thread::JoinHandle<()>, timeout: std::time::Duration) {
    let start = std::time::Instant::now();
    loop {
        if worker.is_finished() {
            let _ = worker.join();
            return;
        }
        if start.elapsed() >= timeout {
            eprintln!(
                "native pane: the worker thread did not exit within {timeout:?} of shutdown \
                 (cancellation was requested); detaching it rather than waiting indefinitely"
            );
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
}

/// Opens a session and starts its worker thread. Returns once the session
/// exists and is ready to accept a first `submit` -- it does not wait for
/// any turn to run.
/// Records a native session's own conversation reference (issue #488, review
/// finding 4) -- the single implementation both native session start paths
/// use, so the headless and the pane session can never write a marker of a
/// different shape.
///
/// A native conversation IS resumable: `NativeBackend::adopt`/`resume` take
/// exactly this journal session id. Recording it is what lets a rollover that
/// later moves this seat elsewhere park it honestly -- `seat::commit` writes
/// `Displaced::conversation` from this marker -- instead of recording a
/// displacement with no way home and cold-launching on the return.
///
/// The marker is keyed `(short, agent, session, runtime)` and is looked up
/// with the seat's own three identity fields, which are the three the seat
/// record beside this call was just stored with: `RuntimeKind::Native`'s own
/// name is the agent every native seat and every native registry record
/// (`session::native`) already uses. `sessions::native_conversation` refuses a
/// marker whose runtime does not match the reader's, which keeps the other
/// direction safe: a harness successor asking for a resume id gets `None` and
/// cold-launches, never a journal session id it could not resume.
///
/// Best-effort, like every other marker in `sessions`: one that fails to write
/// costs a later return its resume, never this session.
pub(super) fn record_seat_conversation(
    state: &super::super::super::state::StateDir,
    handle: &SessionHandle,
    session: &JournalSessionId,
) {
    super::super::super::sessions::record_conversation_on(
        state,
        &handle.short,
        RuntimeKind::Native.as_str(),
        &handle.logical_id,
        session.as_str(),
        RuntimeKind::Native,
    );
}

/// Acquire the writer lease only after this session has a real seat to fence against. (#488)
fn acquire_pane_writer_permit(
    state: &super::super::super::state::StateDir,
    max_writers: usize,
    tree: &std::path::Path,
    handle: &SessionHandle,
) -> Result<super::super::super::permit::HeavyPermit, super::super::super::permit::WriterRefusal> {
    super::super::super::permit::acquire_writer(
        state,
        max_writers,
        "native pane",
        tree,
        Some(super::super::super::permit::SeatFence {
            short: &handle.short,
            generation: handle.generation,
        }),
    )
}

pub fn spawn_interactive(
    request: InteractiveRequest,
    env: EnvLookup<'_>,
) -> CtxResult<InteractiveSession> {
    super::super::require_native_available()?;
    use super::super::super::state::{StateDir, now_secs};
    use super::super::journal::{SeatId, SessionIdentity, TaskId};

    let state = StateDir::resolve(env)?;
    let home = crate::utils::home_dir()?;
    let cfg = super::super::super::config::CtxConfig::load(&request.repo, env)?;
    let now = now_secs();
    let task = request.task.clone().map(TaskId::new).transpose()?;

    let tree = std::fs::canonicalize(&request.repo).unwrap_or_else(|_| request.repo.clone());

    // Store the seat before acquiring its writer lease, so the strict seat-generation fence applies. (#488)
    let mut headless = HeadlessRequest {
        repo: &request.repo,
        prompt: "",
        route: request.route.as_deref(),
        role: &request.role,
        limits: request.limits,
        session_id: None,
        cancellation: None,
        resume: None,
        provider: request.provider.as_deref(),
        fixture_tools: None,
        task: request.task.clone(),
        writer: None,
        accounting: Accounting::Seat,
    };

    let (provider, mut tools, route, brokered) =
        build_transport(&headless, &state, &home, &cfg, env)?;

    let mut journal = Journal::open(&state)?;
    let mut backend = NativeBackend::new();

    // Issue #552: a rollover successor starts ON the seat it is taking over,
    // so the address and the committed generation are the seat's, not a
    // freshly minted pair nothing was fenced against.
    let handle = backend.start_on_seat(
        &SessionSpec {
            runtime: RuntimeKind::Native,
            role: request.role.clone(),
            agent: None,
            provider_route: Some(route.route.clone()),
            model: Some(route.model.id.clone()),
            surface: UiSurface::DashboardPane,
            cwd: request.repo.clone(),
            prompt: String::new(),
            extra_args: Vec::new(),
        },
        request
            .seat
            .as_ref()
            .map(|(short, generation)| (short.as_str(), *generation)),
    )?;
    let session = JournalSessionId::new(handle.logical_id.clone())?;
    journal.create_session(&SessionIdentity {
        session: session.clone(),
        seat: SeatId::new(handle.short.clone())?,
        generation: handle.generation,
        task,
        route: route.clone(),
        // Issue #639: same affinity record a headless `run_session` writes;
        // an interactive pane always mints a FRESH journal session here
        // (never a `--resume` of an existing one), so there is nothing to
        // check against yet, only an origin to record for a later one.
        repo: tree.clone(),
        created_at: now,
        completed_at: None,
    })?;

    super::super::super::seat::store(
        &state,
        &super::super::super::seat::Seat {
            short: handle.short.clone(),
            session: handle.logical_id.clone(),
            generation: handle.generation,
            agent: RuntimeKind::Native.as_str().to_string(),
            model: Some(route.model.id.clone()),
            provider: route.provider.to_string(),
            role: request.role.clone(),
            pinned: false,
            phase: Default::default(),
            visited: Vec::new(),
            last_rollover_at: None,
            rollover_failures: 0,
            failed_rollover_observed_at: None,
            pending: None,
            displaced: None,
            created_at: now,
            updated_at: now,
            runtime: RuntimeKind::Native,
        },
    )?;

    // Record this seat's conversation under its owning runtime. (#488)
    record_seat_conversation(&state, &handle, &session);

    // Fence the writer lease against this stored seat's actual generation. (#488)
    let writer_permit_held = Arc::new(std::sync::atomic::AtomicBool::new(false));
    if request.writing {
        match acquire_pane_writer_permit(&state, cfg.supervise.max_writers, &tree, &handle) {
            Ok(permit) => {
                writer_permit_held.store(true, std::sync::atomic::Ordering::Release);
                headless.writer = Some(Box::new(ObservedWriterLease {
                    inner: Box::new(permit),
                    held: Arc::clone(&writer_permit_held),
                }));
            }
            Err(refusal) => {
                let reason = super::super::super::permit::describe_writer_refusal(
                    &refusal,
                    &state,
                    cfg.supervise.max_writers,
                    &tree,
                );
                return Err(format!("native pane: {reason}").into());
            }
        }
    }

    // Issue #490 (N21 item B): an in-process pane HAS an operator, so its
    // broker runs interactive and raises its approval requests on this
    // channel. `approvals` is built before the executor because the executor's
    // broker is what installs it; the pane drains `approval_prompts`.
    let (approvals, approval_prompts) = super::super::enforcement::InteractiveApprovals::new(
        Arc::new(super::super::enforcement::ApprovalAuthority::new()),
        format!("pane {}", handle.short),
    );

    if brokered {
        let executor = brokered_tools(
            &mut headless,
            &state,
            &home,
            &cfg,
            &handle,
            Some(Arc::clone(&approvals)),
            env,
        )?;
        tools = executor;
    }
    let retained_writer = headless.writer.take();

    backend.attach_journal(journal);
    backend.adopt(&handle, session.clone())?;
    let cancel = backend
        .cancellation(&handle)
        .unwrap_or_else(|| Arc::new(CancellationFlag::default()));

    // Compile standing context as headless does; failure keeps the pane open and surfaces a notice. (#484, #531)
    let (system, preamble, standing_context_notice) = match compile_standing_context(
        &state,
        &home,
        &cfg,
        &headless,
        &route,
        &session,
        now,
        &[],
    ) {
        Ok((system, preamble)) => (system, preamble, None),
        Err(error) => (
            Vec::new(),
            Vec::new(),
            Some(format!(
                "standing context could not be compiled ({error}); continuing with the \
                     conversation alone"
            )),
        ),
    };

    // Admit the pane through the shared billing allocator and persistent breaker for its resolved route. (#554)
    if let Some(refusal) = super::super::super::native_account::native_placement(
        &state,
        &cfg,
        &request.repo,
        &route.route,
        now,
    )
    .and_then(|placement| placement.refusal)
    {
        return Err(refusal.into());
    }

    // Issue #486: the same compaction envelope `run_session` builds.
    let compaction = CompactionSettings {
        enabled: true,
        policy: super::super::super::provider::config::NativeConfig::load(&home, &request.repo)?
            .map(|native| native.compaction_policy())
            .unwrap_or_default(),
        budget: NativeBudget {
            context_window_tokens: super::super::super::provider::capability::declared(
                route.protocol,
                &route.model,
                None,
            )
            .context_window,
            output_reserve_tokens: request.limits.max_output_tokens,
        },
        score: cfg.score.clone(),
        distill: DistillBudget::default(),
        retain_recent_messages: RETAIN_RECENT_MESSAGES,
        constraints: Vec::new(),
        state: Some(state.clone()),
    };

    let prompt_cache = super::types::prompt_cache_for(&cfg, route.provider.as_ref());
    let config = NativeSessionConfig {
        session: session.clone(),
        generation: handle.generation,
        route: route.clone(),
        role: request.role.clone(),
        seat_model: env(super::super::super::adapters::SEAT_MODEL_ENV),
        write_posture: lifecycle::orchestrator_write_posture(&cfg),
        limits: request.limits,
        task: task_for_config(&handle, &route, request.task.as_deref())?,
        workflow_gate: None,
        compaction,
        workflow_repo: brokered.then(|| request.repo.clone()),
        system,
        preamble,
        prompt_cache,
    };

    let (submit_tx, submit_rx) = mpsc::channel::<String>();
    let (progress_tx, progress_rx) = mpsc::channel::<InteractiveProgress>();
    if let Some(notice) = standing_context_notice {
        // Queued before the worker thread even starts, so the FIRST
        // `drain_progress()` a caller makes already sees it -- never
        // dependent on the worker reaching its first turn.
        let _ = progress_tx.send(InteractiveProgress::Notice(notice));
    }
    let worker_cancel = Arc::clone(&cancel);
    let worker_handle = handle.clone();
    let worker_session = session.clone();
    // Pass the admitted billing pool to the worker thread; turns must settle against the pool that admitted them. (#554)
    let worker_state = state.clone();
    let worker_cfg = cfg.clone();
    // Issue #538 (chunk C): captured here (owned) so the spawned thread below
    // can opt its own `NativeLoop` into automatic per-turn recompile
    // checking -- `home`/`request.repo` themselves are not moved into it.
    let worker_home = home.clone();
    let worker_repo = request.repo.to_path_buf();
    let worker_pool = route.billing_pool.as_ref().to_string();
    let worker_output_reserve = request.limits.max_output_tokens;

    let worker_approvals = Arc::clone(&approvals);
    let worker = std::thread::spawn(move || {
        let _retained_writer = retained_writer;
        let mut backend = backend;
        let mut tools = tools;
        let mut config = config;
        let mut first_turn = true;
        let env_fn = super::super::super::config::env_from_process();
        for text in submit_rx.iter() {
            // Issue #537 (T2b): the harness proxy's decision applies once,
            // on this session's very first submitted text -- a native pane
            // always mints a FRESH journal session (see this function's own
            // doc comment above), so the first turn through this loop IS the
            // session's first turn, and a local flag is the honest signal
            // rather than an inferred one. A no-op whenever `proxy::
            // activation` finds no usable decider (disabled, or the
            // deterministic decider, which never takes over a launch), so a
            // disabled or unusable proxy leaves this session byte-identical
            // to today.
            if first_turn {
                first_turn = false;
                apply_proxy_first_turn(
                    &worker_cfg,
                    &worker_state,
                    &worker_repo,
                    &text,
                    &worker_home,
                    &mut config.route,
                    &progress_tx,
                );
            }
            // A new turn re-arms the dialog: an interrupt cancels the turn
            // that was running, never the session's ability to be asked again.
            worker_approvals.resume();
            let _ = progress_tx.send(InteractiveProgress::Busy);
            if let Err(error) = backend.submit(&worker_handle, &text) {
                let _ = progress_tx.send(InteractiveProgress::Failed(error.to_string()));
                continue;
            }
            let Some(journal) = backend.journal_mut() else {
                let _ = progress_tx.send(InteractiveProgress::Failed(
                    "native pane: the journal was not attached".to_string(),
                ));
                continue;
            };
            let env: EnvLookup<'_> = &env_fn;
            let mut driver = NativeLoop::new_driver(
                config.clone(),
                &provider,
                tools.as_mut(),
                journal,
                Arc::clone(&worker_cancel),
                &now_ms,
                env,
            );
            // Issue #538 (chunk C), decision 1: opts this loop into
            // automatic per-turn recompile checking -- see
            // `set_recompile_context`'s own doc.
            driver.set_recompile_context(RecompileContext {
                state: worker_state.clone(),
                home: worker_home.clone(),
                cfg: worker_cfg.clone(),
                repo: worker_repo.clone(),
            });
            // Settle each pane turn separately against its billing pool, breaker, and spend ledger. (#554)
            let turn_reservation = super::super::super::native_account::reserve_seat_turn(
                &worker_state,
                &worker_pool,
                &worker_session.to_string(),
                worker_output_reserve,
                super::super::super::state::now_secs(),
            );
            let result = driver.run_to_completion();
            drop(driver);
            match result {
                Ok(status) => {
                    super::super::super::native_account::settle_seat_turn(
                        &worker_state,
                        &worker_cfg,
                        &status,
                        turn_reservation.as_ref(),
                        Some(worker_handle.short.as_str()),
                    );
                    if let Some(entry) = backend.sessions.get_mut(&worker_handle.logical_id) {
                        entry.state = SessionState::Idle;
                        entry.push(
                            &worker_handle.logical_id,
                            super::super::protocol::RuntimeEvent::TurnCompleted {
                                final_text: status.final_text,
                            },
                        );
                    }
                    let _ = progress_tx.send(InteractiveProgress::Idle);
                }
                Err(aborted) => {
                    // Issue #554 (integration review): a hard abort is not an
                    // empty turn. Whatever the provider already billed inside
                    // it is spent, so it settles exactly as a completed turn
                    // does -- which also resolves the estimate, rather than
                    // releasing an estimate and dropping the real spend.
                    super::super::super::native_account::settle_seat_turn(
                        &worker_state,
                        &worker_cfg,
                        &aborted.status,
                        turn_reservation.as_ref(),
                        Some(worker_handle.short.as_str()),
                    );
                    let _ = progress_tx.send(InteractiveProgress::Failed(aborted.error));
                }
            }
        }
        if let Some(journal) = backend.journal_mut() {
            let _ = journal.complete_session(
                &worker_session,
                worker_handle.generation,
                "ended".to_string(),
                now_secs(),
            );
        }
        worker_approvals.close();
        let _ = progress_tx.send(InteractiveProgress::Ended);
    });

    Ok(InteractiveSession {
        handle,
        session,
        route,
        cancel,
        approvals,
        approval_prompts,
        submit_tx,
        progress_rx,
        worker: Some(worker),
        writer_permit_held,
    })
}
#[cfg(test)]
pub(super) use tests::{spawn_fixture_interactive_session, wait_for_idle};

#[cfg(test)]
mod tests {
    use super::super::super::fixture::fixture_root;
    use super::super::super::journal::MessageRole;
    use super::super::tests::interactive_shutdown_fixture;
    use super::*;

    pub(in super::super) fn spawn_fixture_interactive_session(
        repo: &std::path::Path,
        env: &std::collections::HashMap<String, String>,
    ) -> InteractiveSession {
        let provider = format!(
            "fixture:{}",
            fixture_root().join("helper-answer.json").display()
        );
        let lookup = |k: &str| env.get(k).cloned();
        spawn_interactive(
            InteractiveRequest {
                repo: repo.to_path_buf(),
                role: "worker".to_string(),
                route: None,
                limits: NativeLimits::default(),
                task: None,
                writing: true,
                provider: Some(provider),
                seat: None,
            },
            &lookup,
        )
        .expect("interactive session opens")
    }

    /// Blocks (bounded) until `session` has reported at least one
    /// `InteractiveProgress::Idle`, i.e. its one submitted turn finished.
    pub(in super::super) fn wait_for_idle(session: &InteractiveSession) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            if session
                .drain_progress()
                .iter()
                .any(|progress| matches!(progress, InteractiveProgress::Idle))
            {
                return;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "the one submitted turn never reported Idle"
            );
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }

    /// Issue #554 (review round 1): the operator's OWN dashboard-hosted pane
    /// accounts every turn -- it is a request on a real account exactly as a
    /// delegated worker's is.
    ///
    /// Drives the production entry (`spawn_interactive`) against the fixture
    /// provider and asserts what the turn owes: the persistent breaker saw
    /// the outcome, the pool's ledger has nothing left outstanding, and
    /// `zirv ctx spend` can see the row.
    #[test]
    fn interactive_native_turns_record_health_and_settle_pool_spend() {
        use crate::commands::ctx::health::{Observed, RouteKey, RouteScope};
        use crate::commands::ctx::{health_store, log, spend};

        let (repo, state, _tree, env) = interactive_shutdown_fixture();
        let cfg = crate::commands::ctx::config::CtxConfig::default();
        let policy = cfg.fallback.effective_health();
        let session = spawn_fixture_interactive_session(repo.path(), &env);
        let pool = session.route.billing_pool.as_ref().to_string();
        let endpoint = RouteKey::scoped(RouteScope::Endpoint, session.route.endpoint.as_ref());

        // A breaker record has to EXIST for a success to be folded into it --
        // that is `record_success_and_persist`'s own contract, and it is what
        // lets this assert the health write really happened rather than
        // asserting the absence of one.
        health_store::observe_and_persist(
            &state,
            &endpoint,
            &Observed::new(
                crate::commands::ctx::event::ProviderErrorClass::Transport,
                Some(1),
                None,
            ),
            None,
            1,
            &policy,
        );
        let before = health_store::load(&state, &endpoint, 2);

        session.submit("do the thing".to_string()).expect("submit");
        wait_for_idle(&session);

        assert_ne!(
            health_store::load(&state, &endpoint, 3),
            before,
            "the turn's outcome reaches the PERSISTENT breaker for its endpoint"
        );
        assert_eq!(
            crate::commands::ctx::reservation::outstanding(&state, &pool, 0),
            0,
            "the turn's estimate is settled against the pool, not left outstanding"
        );
        let rows = log::read_delegations(&state, 20);
        let row = rows
            .iter()
            .find(|row| row.session == session.session.to_string())
            .expect("the pane's own turn appears in the ledger zirv ctx spend reads");
        assert_eq!(row.agent, "native");
        let aggregated = spend::aggregate(
            &rows,
            spend::SpendDimension::Harness,
            &crate::commands::ctx::price::built_in_table(),
        );
        assert!(
            aggregated
                .iter()
                .any(|row| row.key == "native" && row.runs > 0),
            "`zirv ctx spend --by harness` reports the seat's own native spend"
        );
    }

    /// Issue #576: completing one dashboard turn returns the backend to idle,
    /// so the same native conversation can accept the next operator input.
    #[test]
    fn local_native_session_accepts_two_sequential_turns() {
        let (repo, state, _tree, env) = interactive_shutdown_fixture();
        let session = spawn_fixture_interactive_session(repo.path(), &env);
        let session_id = session.session.clone();

        session.submit("first".to_string()).expect("first submit");
        wait_for_idle(&session);
        session.submit("second".to_string()).expect("second submit");
        wait_for_idle(&session);
        session.shutdown();

        let replayed = Journal::open(&state)
            .expect("journal")
            .replay(&session_id)
            .expect("replay");
        let inputs: Vec<_> = replayed
            .messages
            .iter()
            .filter(|message| message.role == MessageRole::User)
            .filter_map(|message| message.text.as_deref())
            .collect();
        assert_eq!(inputs, ["first", "second"]);
    }

    /// PR #531 review finding 7's first case: submit a turn, let it finish
    /// on its own, then shut down. `shutdown`'s new cancel-first behaviour
    /// (finding 1) must not change the ordinary, already-idle outcome: the
    /// writer permit this session held for `repo` is gone, and the
    /// journal's own session record is finalised (`ended_reason` set),
    /// once `shutdown` returns.
    #[test]
    fn shutdown_after_a_completed_turn_releases_the_writer_permit_and_finalises_the_session() {
        let (repo, state, tree, env) = interactive_shutdown_fixture();
        let session = spawn_fixture_interactive_session(repo.path(), &env);
        let session_id = session.session.clone();
        session.submit("go".to_string()).expect("submit");
        wait_for_idle(&session);

        session.shutdown();

        let held = crate::commands::ctx::permit::live_writer_records(&state)
            .into_iter()
            .any(|record| record.tree.as_deref() == Some(tree.as_path()));
        assert!(
            !held,
            "the writer permit must be released once shutdown returns"
        );

        let journal = Journal::open(&state).expect("reopen journal");
        let replayed = journal.replay(&session_id).expect("replay");
        assert!(
            replayed.ended_reason.is_some(),
            "the journal session must be finalised (SessionEnded) by shutdown"
        );
    }

    /// Issue #488 (review finding 4): a native session records its own
    /// conversation reference, under its own runtime, beside the seat it just
    /// registered -- so a rollover that later moves this seat can park it
    /// honestly with a conversation a return can actually resume, instead of
    /// recording a displacement with no way home.
    ///
    /// The other half is the safety property: the same marker must be
    /// invisible to a reader asking as a HARNESS, so a harness successor can
    /// never be handed a journal session id it could not resume.
    #[test]
    fn a_native_session_records_its_own_conversation_under_its_own_runtime() {
        let (repo, state, _tree, env) = interactive_shutdown_fixture();
        let session = spawn_fixture_interactive_session(repo.path(), &env);
        let short = session.handle.short.clone();
        let logical = session.handle.logical_id.clone();
        let journal_session = session.session.to_string();
        session.shutdown();

        // The seat this session registered names the agent the marker is
        // keyed by, so the two genuinely resolve against each other rather
        // than merely both existing.
        let seat = crate::commands::ctx::seat::load(&state, &short).expect("a native seat");
        assert_eq!(seat.runtime, RuntimeKind::Native);
        assert_eq!(seat.agent, RuntimeKind::Native.as_str());

        assert_eq!(
            crate::commands::ctx::sessions::native_conversation(
                &state,
                &short,
                &seat.agent,
                &logical,
                RuntimeKind::Native,
            )
            .as_deref(),
            Some(journal_session.as_str()),
            "a native session's conversation must be resumable by a later return"
        );
        assert_eq!(
            crate::commands::ctx::sessions::native_conversation(
                &state,
                &short,
                &seat.agent,
                &logical,
                RuntimeKind::Harness,
            ),
            None,
            "a harness reader must never be handed a native journal session id"
        );
    }

    /// Issue #488 (review finding 1 follow-up, PR #535): `spawn_interactive`
    /// now fences its writer lease on `Some(SeatFence)` once its own seat is
    /// stored, so an uncommitted or superseded generation must be refused a
    /// lease and the committed one must be granted -- mirrors `permit::
    /// tests::a_stale_or_uncommitted_generation_may_not_take_a_writer_
    /// lease`, driven against `acquire_pane_writer_permit` directly: a real
    /// pane's short/logical id is random (`NativeBackend::start` mints it),
    /// so a `SessionHandle` this test controls stands in for the one a real
    /// session would carry at the exact point the lease is acquired.
    #[test]
    fn a_stale_or_uncommitted_generation_may_not_open_a_native_pane() {
        use crate::commands::ctx::seat;

        let tmp = tempfile::tempdir().expect("tempdir");
        let state = crate::commands::ctx::state::StateDir::from_root(tmp.path().to_path_buf());
        let tree = tmp.path().join("repo");
        std::fs::create_dir_all(&tree).expect("mkdir");

        let session_id = "8c7b6a5d-9999-4000-8000-000000000535";
        let short = crate::commands::ctx::sessions::short_id(session_id);
        seat::register(
            &state,
            &short,
            session_id,
            "native",
            None,
            "anthropic",
            "worker",
            false,
            1,
        )
        .expect("register");

        let handle_at = |generation: u64| SessionHandle {
            runtime: RuntimeKind::Native,
            logical_id: session_id.to_string(),
            short: short.clone(),
            generation,
            role: "worker".to_string(),
            surface: UiSurface::DashboardPane,
            conversation: None,
        };

        // The seat's own generation is granted.
        let held = acquire_pane_writer_permit(&state, 2, &tree, &handle_at(1))
            .expect("the committed generation holds the seat");
        drop(held);

        let prepared = seat::prepare_onto(
            &state,
            &short,
            "claude",
            None,
            RuntimeKind::Harness,
            seat::Cause::Manual,
            2,
        )
        .expect("prepare");

        // The successor of a prepared-but-uncommitted rollover may not write.
        let refusal = acquire_pane_writer_permit(&state, 2, &tree, &handle_at(prepared))
            .expect_err("an uncommitted successor may not take a writer lease");
        let crate::commands::ctx::permit::WriterRefusal::StaleSeat { stale } = &refusal else {
            panic!("expected a stale-seat refusal, got {refusal:?}");
        };
        assert_eq!(stale.reason, seat::StaleReason::Uncommitted);

        // ...and the source still holds the seat while the transaction is
        // open.
        let source = acquire_pane_writer_permit(&state, 2, &tree, &handle_at(1))
            .expect("the source keeps the seat until the commit");
        drop(source);

        seat::commit(&state, &short, prepared, "successor-session", 3).expect("commit");

        // After the commit the answer swaps: the predecessor is refused as
        // superseded rather than as uncommitted.
        let refusal = acquire_pane_writer_permit(&state, 2, &tree, &handle_at(1))
            .expect_err("a superseded predecessor may not take a writer lease");
        let crate::commands::ctx::permit::WriterRefusal::StaleSeat { stale } = &refusal else {
            panic!("expected a stale-seat refusal, got {refusal:?}");
        };
        assert_eq!(stale.reason, seat::StaleReason::Superseded);

        let successor = acquire_pane_writer_permit(&state, 2, &tree, &handle_at(prepared))
            .expect("the committed successor holds the seat");
        drop(successor);
    }

    /// PR #531 review finding 7's second case: interrupt a turn and shut
    /// down without ever waiting for it to finish on its own -- the "busy"
    /// case the blocker (finding 1) is actually about. Before that fix,
    /// `shutdown` never cancelled anything and simply joined, so this path
    /// was only ever as fast as the in-flight turn's own natural
    /// completion; now `shutdown` cancels first, so it must return quickly
    /// (well inside `SHUTDOWN_JOIN_TIMEOUT`, the bounded-wait fallback's own
    /// ceiling) and the writer permit and journal must still both be
    /// cleaned up -- "released regardless" of which of the two paths inside
    /// `shutdown` actually ran.
    #[test]
    fn shutdown_while_a_turn_is_in_flight_does_not_hang_and_still_releases_resources() {
        let (repo, state, tree, env) = interactive_shutdown_fixture();
        let session = spawn_fixture_interactive_session(repo.path(), &env);
        let session_id = session.session.clone();
        session.submit("go".to_string()).expect("submit");
        // Deliberately no wait: interrupt and shut down while the turn may
        // still be in flight (or, on a fast fixture, may have already
        // finished -- either way `shutdown` must behave the same).
        session.interrupt();

        let started = std::time::Instant::now();
        session.shutdown();
        let elapsed = started.elapsed();
        assert!(
            elapsed < SHUTDOWN_JOIN_TIMEOUT,
            "shutdown must not hang waiting on a cancelled/finished turn: took {elapsed:?}"
        );

        let held = crate::commands::ctx::permit::live_writer_records(&state)
            .into_iter()
            .any(|record| record.tree.as_deref() == Some(tree.as_path()));
        assert!(
            !held,
            "the writer permit must be released regardless of the interrupt race"
        );

        let journal = Journal::open(&state).expect("reopen journal");
        let replayed = journal.replay(&session_id).expect("replay");
        assert!(
            replayed.ended_reason.is_some(),
            "the journal session must still be finalised even when shutdown raced a busy turn"
        );
    }
}
