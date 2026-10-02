//! `zirv ctx exec --runtime native` (`run_headless`/`run_session`), plus the
//! transport/broker construction `interactive` and `hosted` share.

use std::sync::Arc;

use super::super::super::CtxResult;
use super::super::super::config::EnvLookup;
use super::super::super::lifecycle;
use super::super::super::provider::adapter::{CancellationFlag, ProviderAdapter};
use super::super::super::state::now_ms;
use super::super::compaction::{DistillBudget, NativeBudget, RETAIN_RECENT_MESSAGES};
use super::super::journal::{Journal, JournalSessionId, RouteIdentity};
use super::super::tools::NativeToolClient;
use super::super::{
    BackendConversationRef, RuntimeBackend, RuntimeKind, SessionHandle, SessionSpec, UiSurface,
};
use super::backend::NativeBackend;
use super::interactive::record_seat_conversation;
use super::turn::{NativeLoop, TurnDriver, resume_journal};
use super::types::{
    ClientToolExecutor, CompactionSettings, NativeFinalStatus, NativeLimits, NativeSessionConfig,
    RecompileContext, ToolExecutor,
};

// -- headless execution ---------------------------------------------------

/// The `--provider` prefix that swaps the live transport for a deterministic
/// fixture script. Operator-only by construction: it is a command-line flag,
/// and no configuration layer -- least of all a repository's -- can set it.
pub const FIXTURE_PROVIDER_PREFIX: &str = "fixture:";

/// Everything `zirv ctx exec --runtime native -- <prompt>` needs.
///
/// `route` is a `[route]` name from the operator's own native provider
/// configuration; omitting it uses the `[roles]` entry for `role`. There is no
/// adapter, no agent binary and no PATH probe anywhere on this path.
#[derive(Debug)]
pub struct HeadlessRequest<'a> {
    pub repo: &'a std::path::Path,
    pub prompt: &'a str,
    pub route: Option<&'a str>,
    pub role: &'a str,
    pub limits: NativeLimits,
    /// Conversation identity assigned by a delegation service for a new session.
    pub session_id: Option<&'a str>,
    /// Cancellation shared with the delegation record watcher, when delegated.
    pub cancellation: Option<Arc<CancellationFlag>>,
    /// An existing native journal session to continue instead of starting a
    /// new one. See [`resume_journal`] for what a resume owes first.
    pub resume: Option<&'a str>,
    /// Operator-only transport override. The only accepted shape today is
    /// `fixture:<path>`, which replays the deterministic provider script at
    /// that path instead of calling a provider.
    pub provider: Option<&'a str>,
    /// The fixture tool script a `fixture:` provider executes against. With
    /// none, every tool call reports a fixture failure rather than touching
    /// the machine.
    pub fixture_tools: Option<&'a std::path::Path>,
    /// Shared task card follows the session, loop, and tool execution identity. (#479)
    pub task: Option<String>,
    /// Repository writes require this live permit at the broker effect boundary. (#479)
    pub writer: Option<Box<dyn super::super::enforcement::WriterLease>>,
    /// Account owner for this run. (#554)
    pub accounting: Accounting,
}

/// Who places, reserves and settles one native run (issue #554).
///
/// Exactly one owner, always. Two owners is not "belt and braces": it
/// double-reserves the same billing pool and writes the same spend twice,
/// which is how `zirv ctx spend` starts reporting an account spending double
/// what it did.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Accounting {
    /// A SEAT: this session is the thing spending, and nothing above it has a
    /// delegation identity. [`run_session`] admits it through the shared
    /// allocator, holds an estimate against its billing pool and settles it.
    /// Every operator-facing entry point.
    #[default]
    Seat,
    /// The CALLER owns it. `native_worker` already placed this route,
    /// reserved against its pool and will settle it with a delegation
    /// identity `run_session` does not have -- the principal, the envelope
    /// digest, the work group and the worker mode -- so `run_session`
    /// must not do any of it a second time.
    CallerOwned,
}

/// The route a `--route`/`--role` pair names, from operator configuration
/// alone.
///
/// Issue #485 (roadmap N16) item 2: role-to-route selection is
/// `ctx::team`'s one lookup in the operator's `[roles]` table, with a typed
/// refusal when the role has no entry -- never a fallback onto another
/// role's route, which would be inferring an entitlement nobody granted.
/// An explicit `--route` is the OPERATOR naming a route and is taken as
/// given; a route a delegating MODEL names goes through
/// `team::authorize_route` instead, at the delegation seam.
///
/// Shared by [`route_provider`] (which reserves before the loop resolves a
/// transport) and by `build_transport` itself, so the two can never disagree
/// about which route a role spends.
fn resolve_role_route(
    native: &super::super::super::provider::config::NativeConfig,
    route: Option<&str>,
    role: &str,
) -> CtxResult<super::super::super::provider::RouteId> {
    use super::super::super::provider::RouteId;

    match route {
        Some(name) => Ok(RouteId::new(name)?),
        None => super::super::super::team::route_for_role(native, role)
            .map_err(|refusal| format!("native runtime: {refusal}; or pass --route").into()),
    }
}

/// The route this request will spend, the PROVIDER it belongs to, and the
/// BILLING POOL whose reservation ledger it spends against -- resolved from
/// operator configuration alone.
///
/// Issue #479 (roadmap N10): a delegated native worker has to reserve its
/// token ceiling against the same ledger a legacy delegation reserves against
/// (`ctx::reservation`), and that reservation is taken BEFORE the run, so it
/// cannot wait for the route resolution [`build_transport`] performs. This
/// deliberately touches no credential store and no network: it reads
/// `[route]`/`[account]` and answers, so a missing or expired credential
/// fails where it should -- at the actual request -- and not at accounting
/// time.
///
/// Issue #554: the pool is returned ALONGSIDE the provider, not instead of
/// it, because they answer different questions. The provider names the vendor
/// (which usage window a reading belongs to); the pool names the balance the
/// work is actually drawn from (`NativeConfig::account_pool`), which is what
/// a reservation must be keyed by -- two routes on one account share one
/// balance, and two accounts at one vendor do not.
pub fn route_provider(
    repo: &std::path::Path,
    route: Option<&str>,
    role: &str,
    env: EnvLookup<'_>,
) -> CtxResult<(super::super::super::provider::RouteId, String)> {
    route_pool(repo, route, role, env).map(|(route_id, provider, _pool)| (route_id, provider))
}

/// The route id one request resolves to, from operator configuration alone.
/// The admission gate's own lookup: it needs the route BEFORE a transport
/// (and therefore a credential) exists, which is the whole point of keeping
/// `resolve_role_route` free of the credential store.
fn resolve_role_route_for(
    repo: &std::path::Path,
    route: Option<&str>,
    role: &str,
) -> CtxResult<super::super::super::provider::RouteId> {
    let home = crate::utils::home_dir()?;
    let native = super::super::super::provider::config::NativeConfig::load(&home, repo)?
        .ok_or("native runtime: no provider configuration")?;
    resolve_role_route(&native, route, role)
}

/// [`route_provider`], plus the billing pool the work is drawn from.
pub fn route_pool(
    repo: &std::path::Path,
    route: Option<&str>,
    role: &str,
    env: EnvLookup<'_>,
) -> CtxResult<(super::super::super::provider::RouteId, String, String)> {
    use super::super::super::provider::config::NativeConfig;

    let home = crate::utils::home_dir()?;
    let native = NativeConfig::load(&home, repo)?.ok_or_else(|| {
        format!(
            "native runtime: no provider configuration at {}. Run `zirv ctx provider` to set up \
             an account, endpoint and route first.",
            NativeConfig::operator_path(&home).display()
        )
    })?;
    let _ = env;
    let route_id = resolve_role_route(&native, route, role)?;
    let account_id = native
        .routes
        .get(&route_id)
        .map(|route| route.account.clone())
        .ok_or_else(|| format!("native runtime: route `{route_id}` names no configured account"))?;
    let provider = native
        .accounts
        .get(&account_id)
        .map(|account| account.provider.to_string())
        .ok_or_else(|| format!("native runtime: route `{route_id}` names no configured account"))?;
    let pool = native.account_pool(&account_id).as_ref().to_string();
    Ok((route_id, provider, pool))
}

/// The durable route identity a new native conversation is filed under
/// (issue #489).
///
/// The persistent runtime has to create the journal session when the client
/// asks for it, and a journal session carries the route it will spend. This
/// resolves that route through the SAME `resolve_target` the transport uses,
/// so the identity written at creation is the identity the first request
/// spends -- rather than a second, hopeful derivation that could disagree with
/// it. A route the operator has not configured fails here, loudly, instead of
/// producing a conversation pinned to a route that does not exist.
pub fn journal_route_identity(
    repo: &std::path::Path,
    route: Option<&str>,
    role: &str,
    env: EnvLookup<'_>,
) -> CtxResult<RouteIdentity> {
    use super::super::super::provider::adapter::resolve_target;
    use super::super::super::provider::config::NativeConfig;
    use super::super::super::provider::credential::OsStore;
    use super::super::super::state::now_secs;

    let home = crate::utils::home_dir()?;
    let native = NativeConfig::load(&home, repo)?.ok_or_else(|| {
        format!(
            "native runtime: no provider configuration at {}. Run `zirv ctx provider` to set up \
             an account, endpoint and route first.",
            NativeConfig::operator_path(&home).display()
        )
    })?;
    let route_id = resolve_role_route(&native, route, role)?;
    if native
        .routes
        .get(&route_id)
        .is_some_and(|route| route.execution.is_some())
    {
        return super::super::execution::route_identity(&native, &route_id);
    }
    let (target, _) = resolve_target(&native, &route_id, env, &OsStore::default(), now_secs())?;
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

/// Runs one headless native session end to end and prints its structured
/// final status as JSON, returning the exit code a `zirv ctx exec` consumer
/// expects.
///
/// This is the native equivalent of `exec::run_with_clock_inner`'s harness
/// spawn: same command, same structured outcome, an entirely different
/// mechanism underneath. Everything it needs comes from operator
/// configuration and the state directory; nothing is inherited from a harness
/// process, because there is none.
pub fn run_headless<W: std::io::Write>(
    request: &mut HeadlessRequest<'_>,
    w: &mut W,
    env: EnvLookup<'_>,
) -> CtxResult<i32> {
    // `run_session`'s own doc comment: `w` can carry one human line before
    // the status exists (a resume's outcome-unknown reconcile notice), which
    // a `--json` caller has to route somewhere other than its own
    // single-object stdout. This is that caller -- it always prints exactly
    // one JSON status object to `w` below, so the notice is captured here
    // and re-emitted on stderr instead, mirroring `native_worker::launch_
    // native`'s identical treatment of the same notice.
    let mut notices: Vec<u8> = Vec::new();
    let status = run_session(request, &mut notices, env)?;
    if !notices.is_empty() {
        eprint!("{}", String::from_utf8_lossy(&notices));
    }
    writeln!(w, "{}", serde_json::to_string_pretty(&status)?)?;
    Ok(status.exit_code)
}

/// The session itself, without the final JSON print. Split out of
/// [`run_headless`] for issue #479 (roadmap N10): a delegated native worker
/// needs the structured status back as a VALUE -- to hold to a `--result-
/// schema` contract, to store as its result, to publish as its terminal
/// outcome and to fold into a delegation receipt -- not written to a stream.
///
/// `w` still carries the one human line a run can owe before its status
/// exists (a resume's outcome-unknown reconcile notice), which a `--json`
/// caller routes somewhere other than its own single-object stdout.
pub fn run_session<W: std::io::Write>(
    request: &mut HeadlessRequest<'_>,
    w: &mut W,
    env: EnvLookup<'_>,
) -> CtxResult<NativeFinalStatus> {
    super::super::require_native_available()?;
    use super::super::super::state::{StateDir, now_secs};
    use super::super::journal::{SeatId, SessionIdentity, TaskId};

    let state = StateDir::resolve(env)?;
    let home = crate::utils::home_dir()?;
    let cfg = super::super::super::config::CtxConfig::load(request.repo, env)?;
    let now = now_secs();
    // Validate the shared card before creating a seat, journal entry, or effect. (#479)
    let task = request.task.clone().map(TaskId::new).transpose()?;

    // Resume the coordinator graph before other work; consume terminal worker outcomes once. (#485)
    if matches!(
        super::super::super::team::prompt_role(request.role),
        super::super::super::prompt::PromptRole::Orchestrator
            | super::super::super::prompt::PromptRole::SubOrchestrator
    ) {
        let mut graph = super::super::super::coordinator::load(&state, request.repo);
        match super::super::super::coordinator::consume_pending(
            &state,
            request.repo,
            &mut graph,
            now,
        ) {
            Ok(consumed) if !consumed.is_empty() => {
                let _ = super::super::super::coordinator::store(&state, request.repo, &graph);
                let _ = writeln!(
                    w,
                    "zirv ctx: resumed the coordinator graph -- consumed {} pending worker \
                     receipt(s)",
                    consumed.len()
                );
            }
            Ok(_) => {}
            // A graph that cannot be read must never stop a session from
            // starting: the delegation records are still authoritative and
            // `team_status` will say what it can see.
            Err(error) => {
                let _ = writeln!(
                    w,
                    "zirv ctx: could not resume the coordinator graph: {error}"
                );
            }
        }
    }

    // Admit through the shared allocator before building a transport, so a
    // headless run obeys the account breaker. (#554)
    if request.accounting == Accounting::Seat
        && let Ok(route_id) = resolve_role_route_for(request.repo, request.route, request.role)
        && let Some(refusal) = super::super::super::native_account::native_placement(
            &state,
            &cfg,
            request.repo,
            &route_id,
            now,
        )
        .and_then(|placement| placement.refusal)
    {
        return Err(refusal.into());
    }

    let (provider, mut tools, route, brokered) =
        build_transport(request, &state, &home, &cfg, env)?;

    let mut journal = Journal::open(&state)?;
    let mut backend = NativeBackend::new();

    // Record the canonical root so relative or symlinked paths to this worktree match on resume. (#639)
    let canonical_repo =
        std::fs::canonicalize(request.repo).unwrap_or_else(|_| request.repo.to_path_buf());

    // Session identity first: the seat record is what the effect-time
    // generation fence reads, so it has to exist before any tool can run.
    let (handle, session) = match request.resume {
        Some(resume) => {
            let session = JournalSessionId::new(resume)?;
            // Check root affinity before `resume_journal` mutates generation or execution state; a refusal must be pure. (#639)
            let identity = journal.session(&session)?;
            if !identity.repo.as_os_str().is_empty() && identity.repo != canonical_repo {
                return Err(format!(
                    "--resume {resume}: this native session started in {}; refusing to \
                     continue it from {} -- resume it from its own repository, or start a new \
                     session here",
                    identity.repo.display(),
                    canonical_repo.display(),
                )
                .into());
            }
            let resumed = resume_journal(&mut journal, &session, now_ms())?;
            let handle = SessionHandle {
                runtime: RuntimeKind::Native,
                logical_id: session.to_string(),
                short: resumed.identity.seat.to_string(),
                generation: resumed.generation,
                role: request.role.to_string(),
                surface: UiSurface::Headless,
                conversation: Some(BackendConversationRef {
                    agent: RuntimeKind::Native.as_str().to_string(),
                    conversation: session.to_string(),
                }),
            };
            if !resumed.reconciled.is_empty() {
                writeln!(
                    w,
                    "native runtime: {} execution(s) were in flight when this session stopped and \
                     are now outcome-unknown; reconcile before retrying their effects",
                    resumed.reconciled.len()
                )?;
            }
            (handle, session)
        }
        None => {
            let handle = match request.session_id {
                Some(logical_id) => SessionHandle {
                    runtime: RuntimeKind::Native,
                    logical_id: logical_id.to_string(),
                    short: super::super::super::sessions::short_id(logical_id),
                    generation: 1,
                    role: request.role.to_string(),
                    surface: UiSurface::Headless,
                    conversation: Some(BackendConversationRef {
                        agent: RuntimeKind::Native.as_str().to_string(),
                        conversation: logical_id.to_string(),
                    }),
                },
                None => backend.start(&SessionSpec {
                    runtime: RuntimeKind::Native,
                    role: request.role.to_string(),
                    agent: None,
                    provider_route: Some(route.route.clone()),
                    model: Some(route.model.id.clone()),
                    surface: UiSurface::Headless,
                    cwd: request.repo.to_path_buf(),
                    prompt: request.prompt.to_string(),
                    extra_args: Vec::new(),
                })?,
            };
            let session = JournalSessionId::new(handle.logical_id.clone())?;
            journal.create_session(&SessionIdentity {
                session: session.clone(),
                seat: SeatId::new(handle.short.clone())?,
                generation: handle.generation,
                // Native journal and delegated task card identify the same shared task. (#479)
                task: task.clone(),
                route: route.clone(),
                // Record the canonical origin once for later resume affinity checks. (#639)
                repo: canonical_repo.clone(),
                created_at: now,
                completed_at: None,
            })?;
            (handle, session)
        }
    };

    super::super::super::seat::store(
        &state,
        &super::super::super::seat::Seat {
            short: handle.short.clone(),
            session: handle.logical_id.clone(),
            generation: handle.generation,
            agent: RuntimeKind::Native.as_str().to_string(),
            model: Some(route.model.id.clone()),
            provider: route.provider.to_string(),
            role: request.role.to_string(),
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

    // Record the conversation reference under its owning runtime. (#488)
    record_seat_conversation(&state, &handle, &session);

    // Register seat-owned native runs for status and control; delegated runs are caller-owned. Keep the guard through every exit path and leave in-flight stamps after aborts. (#645)
    let mut registry_guard = (request.accounting == Accounting::Seat).then(|| {
        let mut record = super::super::super::sessions::Record::new(
            &handle.logical_id,
            RuntimeKind::Native.as_str(),
            request.repo,
            super::super::super::sessions::Verb::Exec,
        )
        .with_role(request.role)
        .unreachable();
        record.runtime = RuntimeKind::Native;
        super::super::super::sessions::SessionGuard::register(&state, record)
    });

    if brokered {
        // The broker is built here, after the seat record exists, because its
        // own fence reads that record at every effect.
        let executor = brokered_tools(request, &state, &home, &cfg, &handle, None, env)?;
        tools = executor;
    }

    // Compile standing context once; failure degrades to an empty layer so the session can still run. (#484)
    let (system, preamble) = match compile_standing_context(
        &state,
        &home,
        &cfg,
        request,
        &route,
        &session,
        now,
        &[],
    ) {
        Ok(compiled) => compiled,
        Err(error) => {
            writeln!(
                w,
                "native runtime: standing context could not be compiled ({error}); continuing with the conversation alone"
            )?;
            (Vec::new(), Vec::new())
        }
    };

    // The backend owns the durable acknowledgement, so the input is on disk
    // before anything is told it was accepted -- the same `acknowledge_input`
    // the loop's own `acknowledge` uses.
    backend.attach_journal(journal);
    backend.adopt(&handle, session.clone())?;
    if !request.prompt.is_empty() {
        backend.submit(&handle, request.prompt)?;
    }
    let cancel = request
        .cancellation
        .clone()
        .or_else(|| backend.cancellation(&handle))
        .unwrap_or_else(|| std::sync::Arc::new(CancellationFlag::default()));

    // Use the declared model window minus reserved output; unknown capacity remains unknown to the rot gate. (#486)
    let compaction = CompactionSettings {
        enabled: true,
        policy: super::super::super::provider::config::NativeConfig::load(&home, request.repo)?
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

    // Guard the estimate until settlement so an early error releases the pool
    // reservation. (#554)
    let mut reservation = SeatReservation {
        state: &state,
        held: (request.accounting == Accounting::Seat)
            .then(|| {
                super::super::super::native_account::reserve_seat_turn(
                    &state,
                    route.billing_pool.as_ref(),
                    &session.to_string(),
                    request.limits.max_output_tokens,
                    now,
                )
            })
            .flatten(),
    };

    let status = {
        let prompt_cache = super::types::prompt_cache_for(&cfg, route.provider.as_ref());
        let journal = backend
            .journal_mut()
            .ok_or("native runtime: the journal was not attached")?;
        let mut driver = NativeLoop::new_driver(
            NativeSessionConfig {
                session: session.clone(),
                generation: handle.generation,
                route: route.clone(),
                role: request.role.to_string(),
                seat_model: env(super::super::super::adapters::SEAT_MODEL_ENV),
                write_posture: lifecycle::orchestrator_write_posture(&cfg),
                limits: request.limits,
                // Scope tool calls and task receipts to the shared card. (#479)
                task: task.clone(),
                workflow_gate: None,
                compaction,
                // Read the active workflow at every real completion attempt; fixtures perform no effects. (#484)
                workflow_repo: brokered.then(|| request.repo.to_path_buf()),
                system,
                preamble,
                prompt_cache,
            },
            &provider,
            tools.as_mut(),
            journal,
            cancel,
            &now_ms,
            env,
        );
        // Recheck standing instructions at each turn boundary. (#538)
        driver.set_recompile_context(RecompileContext {
            state: state.clone(),
            home: home.clone(),
            cfg: cfg.clone(),
            repo: request.repo.to_path_buf(),
        });
        // Stamp in-flight immediately before running; retain it after abort as a crash witness, clearing only at a clean boundary. (#645)
        if let Some(guard) = registry_guard.as_mut() {
            guard.stamp_in_flight(super::super::super::sessions::Verb::Exec.as_str(), 0);
        }
        // Settle billed work before propagating an abort; caller-owned runs pass
        // the billed status to their caller for settlement. (#554)
        match driver.run_to_completion() {
            Ok(status) => {
                if let Some(guard) = registry_guard.as_mut() {
                    guard.clear_in_flight();
                }
                status
            }
            Err(aborted) => {
                if request.accounting == Accounting::Seat {
                    super::super::super::native_account::settle_seat_turn(
                        &state,
                        &cfg,
                        &aborted.status,
                        reservation.take().as_ref(),
                        super::super::super::mail::session_identity(env).as_deref(),
                    );
                }
                return Err(Box::new(aborted));
            }
        }
    };

    if let Some(journal) = backend.journal_mut() {
        journal.complete_session(
            &session,
            handle.generation,
            status.status.as_str().to_string(),
            now_secs(),
        )?;
    }
    // Settle the seat here; caller-owned runs settle under their delegation
    // identity and must not be charged twice. (#554)
    if request.accounting == Accounting::Seat {
        super::super::super::native_account::settle_seat_turn(
            &state,
            &cfg,
            &status,
            reservation.take().as_ref(),
            super::super::super::mail::session_identity(env).as_deref(),
        );
    }
    Ok(status)
}

/// Releases an unsettled estimate on drop, including early error paths. (#554)
struct SeatReservation<'a> {
    state: &'a super::super::super::state::StateDir,
    held: Option<(String, String)>,
}

impl SeatReservation<'_> {
    /// Hands the reservation to the settlement, so the drop below is a no-op.
    fn take(&mut self) -> Option<(String, String)> {
        self.held.take()
    }
}

impl Drop for SeatReservation<'_> {
    fn drop(&mut self) {
        if let Some((pool, id)) = self.held.take() {
            let _ = super::super::super::reservation::release(self.state, &pool, &id);
        }
    }
}

/// The standing instruction and data context one native session runs under
/// (issue #484, roadmap N15), split the way the provider request wants it:
/// instructions become the system prompt, data becomes a leading user message.
///
/// Everything here comes from `runtime::context::compile`, which is the only
/// place that decides what a native session is told and in what order. This
/// function's whole job is handing it the session's own identity and budget.
#[allow(clippy::type_complexity, clippy::too_many_arguments)]
pub(super) fn compile_standing_context(
    state: &super::super::super::state::StateDir,
    home: &std::path::Path,
    cfg: &super::super::super::config::CtxConfig,
    request: &HeadlessRequest<'_>,
    route: &RouteIdentity,
    session: &JournalSessionId,
    now: u64,
    scope_paths: &[std::path::PathBuf],
) -> CtxResult<(Vec<String>, Vec<String>)> {
    use super::super::context::{CompileRequest, TokenBudget};

    let capabilities =
        super::super::super::provider::capability::declared(route.protocol, &route.model, None);
    // The window the route's own model declares, or a conservative floor when
    // the catalogue has nothing for it. Reserving the session's own output
    // ceiling is what keeps the compiled prefix from crowding out the answer.
    let context_window_tokens = capabilities.context_window.unwrap_or(128_000);
    let provider = route.provider.to_string();
    let session_id = session.to_string();
    let compiled = super::super::context::compile(&CompileRequest {
        home: Some(home),
        repo: request.repo,
        cwd: request.repo,
        state,
        config: cfg,
        role: prompt_role(request.role),
        session_id: &session_id,
        task: None,
        constraints: &[],
        pending_actions: &[],
        scope_paths,
        provider: &provider,
        model: &route.model.id,
        capabilities: &capabilities,
        budget: TokenBudget {
            context_window_tokens,
            output_reserve_tokens: request.limits.max_output_tokens,
            max_inline_evidence_bytes: cfg.output.max_summary_bytes,
        },
        evidence: &[],
        token_counter: None,
        now,
    })?;
    Ok(split_standing_context(&compiled))
}

pub(super) fn split_standing_context(
    compiled: &super::super::context::CompiledNativeContext,
) -> (Vec<String>, Vec<String>) {
    use super::super::context::MessageRole;

    let mut system = Vec::new();
    let mut preamble = Vec::new();
    for message in &compiled.messages {
        match message.role {
            MessageRole::Instruction => system.push(message.content.clone()),
            MessageRole::Data => preamble.push(message.content.clone()),
        }
    }
    (system, preamble)
}

/// The prompt role a native session's `--role` names. Unknown values are
/// workers: the least-privileged methodology is the safe default, and an
/// orchestrator layer handed to a worker would tell it to delegate work
/// nobody asked it to delegate.
/// Issue #485 (roadmap N16): the mapping itself moved to `ctx::team`, which
/// is where the closed team-role set lives, so `coordinator` (the roadmap's
/// own name for the seat) and `orchestrator` (the name the prompt layer and
/// every seat record already use) resolve to one methodology rather than two.
pub(super) fn prompt_role(role: &str) -> super::super::super::prompt::PromptRole {
    super::super::super::team::prompt_role(role)
}

/// Resolves the provider transport, the tool executor and the route identity
/// for one headless run. The fourth value says whether the returned executor
/// is a placeholder that must be replaced by a brokered one once the seat
/// record exists -- a fixture run never brokers, because it performs no
/// effects at all.
#[allow(clippy::type_complexity)]
pub(super) fn build_transport(
    request: &HeadlessRequest<'_>,
    state: &super::super::super::state::StateDir,
    home: &std::path::Path,
    cfg: &super::super::super::config::CtxConfig,
    env: EnvLookup<'_>,
) -> CtxResult<(TurnDriver, Box<dyn ToolExecutor>, RouteIdentity, bool)> {
    super::super::require_native_available()?;
    use std::time::Duration;

    use super::super::super::provider::anthropic::AnthropicMessagesAdapter;
    use super::super::super::provider::bedrock::BedrockAdapter;
    use super::super::super::provider::config::NativeConfig;
    use super::super::super::provider::credential::OsStore;
    use super::super::super::provider::google::GoogleAdapter;
    use super::super::super::provider::openai::OpenAiResponsesAdapter;
    use super::super::super::provider::openai_chat::OpenAiChatAdapter;
    use super::super::super::provider::transport::StreamTimeouts;
    use super::super::super::provider::{Protocol, adapter::resolve_target};
    use super::super::super::state::now_secs;
    use super::super::fixture::{
        FixtureProvider, FixtureScript, FixtureToolExecutor, FixtureToolScript, fixture_target,
    };

    let _ = (state, cfg);

    if let Some(spec) = request.provider {
        let Some(path) = spec.strip_prefix(FIXTURE_PROVIDER_PREFIX) else {
            return Err(format!(
                "--provider '{spec}': the only supported value is \
                 `{FIXTURE_PROVIDER_PREFIX}<path to a provider script>`"
            )
            .into());
        };
        let script = FixtureScript::load(std::path::Path::new(path))?;
        let protocol = if script.shape == "openai" {
            Protocol::OpenAiResponses
        } else {
            Protocol::AnthropicMessages
        };
        let model = if script.model.is_empty() {
            "fixture-model".to_string()
        } else {
            script.model.clone()
        };
        let target = fixture_target(protocol, &model);
        let route = RouteIdentity {
            route: target.route.clone(),
            provider: target.provider.clone(),
            endpoint: target.endpoint.clone(),
            account: target.account.clone(),
            billing_pool: target.billing_pool.clone(),
            protocol: target.protocol,
            model: target.model.clone(),
        };
        let tool_script = match request.fixture_tools {
            Some(path) => FixtureToolScript::load(path)?,
            None => FixtureToolScript::default(),
        };
        return Ok((
            TurnDriver::Direct(Box::new(FixtureProvider::new(target, script))),
            Box::new(FixtureToolExecutor::new(tool_script)),
            route,
            false,
        ));
    }

    let native = NativeConfig::load(home, request.repo)?.ok_or_else(|| {
        format!(
            "native runtime: no provider configuration at {}. Run `zirv ctx provider` to \
             set up an account, endpoint and route first.",
            NativeConfig::operator_path(home).display()
        )
    })?;
    let route_id = resolve_role_route(&native, request.route, request.role)?;

    if let Some(execution) = native
        .routes
        .get(&route_id)
        .and_then(|route| route.execution.as_ref())
    {
        let route = super::super::execution::route_identity(&native, &route_id)?;
        let adapter = super::super::execution::create(execution, request.repo, home, env)?;
        return Ok((
            TurnDriver::Execution(adapter),
            Box::new(FixtureToolExecutor::new(FixtureToolScript::default())),
            route,
            true,
        ));
    }
    let store = OsStore::default();
    let now = now_secs();
    let timeouts = StreamTimeouts {
        connect: Duration::from_secs(10),
        first_event: Duration::from_millis(request.limits.first_event_ms.max(1)),
        idle: Duration::from_millis(request.limits.idle_ms.max(1)),
    };
    let (target, _) = resolve_target(&native, &route_id, env, &store, now)?;
    let provider: Box<dyn ProviderAdapter> = match target.protocol {
        Protocol::AnthropicMessages => Box::new(AnthropicMessagesAdapter::from_config(
            &native, &route_id, env, &store, now, timeouts,
        )?),
        Protocol::OpenAiResponses => Box::new(OpenAiResponsesAdapter::from_config(
            &native, &route_id, env, &store, now, timeouts,
        )?),
        Protocol::GoogleGenerativeAi | Protocol::GoogleVertex => Box::new(
            GoogleAdapter::from_config(&native, &route_id, env, &store, now, timeouts)?,
        ),
        // One transport serves every chat-completions-compatible vendor,
        // every local runtime and Azure; the bound route profile is what
        // decides the address, the auth header and the caveats (N13).
        Protocol::OpenAiChatCompatible | Protocol::AzureOpenAiChat => Box::new(
            OpenAiChatAdapter::from_config(&native, &route_id, env, &store, now, timeouts)?,
        ),
        Protocol::AwsBedrock => Box::new(BedrockAdapter::from_config(
            &native, &route_id, env, &store, now, timeouts,
        )?),
    };
    let route = RouteIdentity {
        route: target.route.clone(),
        provider: target.provider.clone(),
        endpoint: target.endpoint.clone(),
        account: target.account.clone(),
        billing_pool: target.billing_pool.clone(),
        protocol: target.protocol,
        model: target.model.clone(),
    };
    // A placeholder: the real executor needs the seat record that only exists
    // once session identity is settled, so `run_headless` swaps it in there.
    Ok((
        TurnDriver::Direct(provider),
        Box::new(FixtureToolExecutor::new(FixtureToolScript::default())),
        route,
        true,
    ))
}

/// The production tool executor: N05's client behind N04's broker, fenced on
/// the persisted native seat record this run just wrote.
/// Issue #479: takes `request` by `&mut` so the live writer permit can be
/// MOVED into the broker rather than cloned -- a lease is the right to write
/// one tree, and duplicating it would be exactly the thing the per-tree claim
/// exists to prevent.
pub(super) fn brokered_tools(
    request: &mut HeadlessRequest<'_>,
    state: &super::super::super::state::StateDir,
    home: &std::path::Path,
    cfg: &super::super::super::config::CtxConfig,
    handle: &SessionHandle,
    approvals: Option<Arc<super::super::enforcement::InteractiveApprovals>>,
    env: EnvLookup<'_>,
) -> CtxResult<Box<dyn ToolExecutor>> {
    use super::super::enforcement::ExecutionIdentity;
    use super::super::tools::ToolLimits;

    let broker = session_broker(
        request.repo,
        state,
        home,
        cfg,
        ExecutionIdentity::from_handle(handle, request.task.clone())?,
        request.writer.take(),
        approvals,
    )?;
    let services = super::super::capabilities::CapabilityServices::from_config(
        cfg,
        request.repo,
        &super::super::super::config::env_from_process(),
        super::super::super::state::now_secs(),
    );
    let mut client = NativeToolClient::new(
        broker,
        state.clone(),
        request.repo.to_path_buf(),
        ToolLimits::from_config(cfg),
    );
    client.install_launch_env(env);
    Ok(Box::new(ClientToolExecutor::new(
        client.with_capabilities(services),
    )))
}

/// The execution broker one native session runs behind.
///
/// Extracted from [`brokered_tools`] for issue #484 (roadmap N15) so the
/// read-only helper contract can be asserted against the SAME construction a
/// real session gets, rather than against a test-local copy of it that could
/// drift. `writer` is the whole of that contract: a `None` lease means every
/// repository write, outside write, write-effect process and shared-scope
/// knowledge write is refused here, at effect time, with
/// `BrokerError::WriterPermit` -- and a session with no `approvals` gate runs
/// in `ApprovalMode::Headless`, which means the refusal cannot be approved
/// away either.
///
/// Issue #490 (roadmap N21 item B): `approvals` is the operator's own dialog,
/// and the approval MODE is derived from it rather than passed separately --
/// a session is interactive exactly when there is a live channel to ask on.
/// That makes the invariant structural: there is no way to build a broker
/// that says it will ask and then has nobody to ask, and no way to build one
/// that has a dialog it never consults. Every headless caller passes `None`
/// and gets precisely the pre-#490 construction.
pub(crate) fn session_broker(
    repo: &std::path::Path,
    state: &super::super::super::state::StateDir,
    home: &std::path::Path,
    cfg: &super::super::super::config::CtxConfig,
    identity: super::super::enforcement::ExecutionIdentity,
    writer: Option<Box<dyn super::super::enforcement::WriterLease>>,
    approvals: Option<Arc<super::super::enforcement::InteractiveApprovals>>,
) -> Result<super::super::enforcement::ExecutionBroker, super::super::enforcement::BrokerError> {
    use super::super::super::provider::config::NativeConfig;
    use super::super::super::provider::credential::CredentialRef;
    use super::super::enforcement::{
        ApprovalAuthority, ApprovalMode, ConfigPolicySource, ExecutionBroker, PlatformIsolation,
        ResourceClaims, StoredSeatFence,
    };

    let claims = ResourceClaims::new(
        repo,
        repo,
        state.root(),
        home,
        configured_network_scope(cfg),
    )?;
    // Only a session that actually holds a writer lease claims git metadata
    // roots. `discover_linked_worktree_git` refuses a MAIN checkout outright
    // ("native writers require a linked worktree"), which is the right answer
    // for a worker that was granted a tree -- and the wrong one for a
    // read-only helper or a plain `zirv ctx exec --runtime native`, neither of
    // which can write anything at all: without this, an inspection session in
    // an ordinary checkout could not even construct its broker.
    let claims = match writer {
        Some(_) => claims.discover_linked_worktree_git()?,
        None => claims,
    };

    let mode = match approvals {
        Some(_) => ApprovalMode::Interactive,
        None => ApprovalMode::Headless,
    };
    // The gate and the broker must share one signer, or every grant the
    // dialog mints fails verification on the way back in.
    let authority = approvals
        .as_ref()
        .map(|approvals| approvals.authority())
        .unwrap_or_else(|| std::sync::Arc::new(ApprovalAuthority::new()));
    let protected_env_names = NativeConfig::load(home, repo)
        .map_err(|error| {
            super::super::enforcement::BrokerError::PolicyUnavailable(error.to_string())
        })?
        .into_iter()
        .flat_map(|native| native.accounts.into_values())
        .filter_map(|account| match account.credential {
            Some(CredentialRef::Env(name)) => Some(name),
            _ => None,
        })
        .collect();
    let broker = ExecutionBroker::new(
        identity,
        claims,
        mode,
        std::sync::Arc::new(ConfigPolicySource::new(repo.to_path_buf())),
        std::sync::Arc::new(StoredSeatFence::new(state.clone())),
        authority,
        writer,
        PlatformIsolation::detect(),
        protected_env_names,
    )?;
    Ok(match approvals {
        Some(approvals) => broker.with_interactive_approvals(approvals),
        None => broker,
    })
}

/// The task's network claim, built from the operator's own capability
/// allowlist (issue #483). Nothing is reachable by default: a session gets a
/// host-scoped claim only for the hosts an operator wrote down, and
/// `NetworkScope::Only` is deliberately never `Any` -- an arbitrary process
/// still cannot take network under it, which is exactly the asymmetry N04
/// documents between brokered HTTP tools and shells.
fn configured_network_scope(
    cfg: &super::super::super::config::CtxConfig,
) -> super::super::enforcement::NetworkScope {
    use super::super::enforcement::{NetworkScope, NetworkTarget};

    if !cfg.capabilities.enabled {
        return NetworkScope::Denied;
    }
    let mut targets = std::collections::BTreeSet::new();
    for host in &cfg.capabilities.web.allow_hosts {
        let host = host.trim().trim_start_matches('.');
        for scheme in ["https", "http"] {
            if let Ok(target) = NetworkTarget::new(scheme, host, None) {
                targets.insert(target);
            }
        }
    }
    if targets.is_empty() {
        NetworkScope::Denied
    } else {
        NetworkScope::Only { targets }
    }
}
#[cfg(test)]
mod tests {
    use super::super::super::super::provider::Protocol;
    use super::super::super::super::provider::adapter::ProviderContent;
    use super::super::super::fixture::{
        FixtureProvider, FixtureScript, FixtureToolExecutor, FixtureToolScript, fixture_root,
        fixture_target,
    };
    use super::super::super::journal::{
        EventScope, ExecutionId, ExecutionState, SeatId, SessionIdentity, ToolCallId,
    };
    use super::super::tests::{
        config_for, interactive_shutdown_fixture, journal_for, no_env, route_for,
    };
    use super::super::turn::ABORT_AFTER_TURNS_ENV;
    use super::super::types::{AbortedRun, NativeStatus};
    use super::*;

    /// Issue #484 (roadmap N15) item 3: workflow and methodology adoption is
    /// AUTOMATIC. Nobody hand-seeds a native session with a methodology
    /// prompt; the context compiler puts the engineering standard and the
    /// active workflow's current step into every request the session makes,
    /// on the strength of the workflow store alone.
    #[test]
    fn a_native_session_adopts_the_active_workflow_without_being_seeded() {
        use crate::commands::workflow::engine;

        let repo = crate::commands::ctx::testenv::repo();
        let home = tempfile::tempdir().expect("home");
        let state = crate::commands::ctx::state::StateDir::from_root(
            tempfile::tempdir().expect("state").keep(),
        );
        let workflow = engine::WorkflowState::start(
            repo.path().to_path_buf(),
            "wire the native workflow tools".into(),
            engine::WorkflowKind::Feature,
            None,
            true,
            crate::commands::workflow::classify::Classification {
                intent: crate::commands::workflow::classify::Intent::Feature,
                complexity: crate::commands::workflow::classify::Complexity::Trivial,
                risk: crate::commands::workflow::classify::RiskBand::Low,
                risk_score: 0,
                changed_files: 1,
                changed_lines: 5,
                changed_paths: Vec::new(),
                declared_scope: false,
                work_domain: Default::default(),
                risk_measurement: crate::commands::workflow::classify::RiskMeasurement::Measured,
                reasons: vec!["small".into()],
            },
        );
        engine::save(&state, &workflow, true).expect("save");

        let route = route_for(Protocol::AnthropicMessages, "claude-fixture");
        let session = JournalSessionId::new("native-adoption-1").expect("session id");
        let request = HeadlessRequest {
            repo: repo.path(),
            prompt: "continue the workflow",
            route: None,
            role: "orchestrator",
            limits: NativeLimits::default(),
            session_id: None,
            cancellation: None,
            resume: None,
            provider: None,
            fixture_tools: None,
            task: None,
            writer: None,
            accounting: Accounting::Seat,
        };
        let (system, preamble) = compile_standing_context(
            &state,
            home.path(),
            &Default::default(),
            &request,
            &route,
            &session,
            1,
            &[],
        )
        .expect("the standing context compiles");

        let system_text = system.join(
            "
",
        );
        assert!(
            system_text.contains("zirv native model profile"),
            "the model profile is part of every native session's instructions"
        );
        assert!(
            !system.is_empty() && system_text.len() > 200,
            "the engineering standard and role methodology must be present: {system_text:?}"
        );
        let all = format!(
            "{system_text}
{}",
            preamble.join(
                "
"
            )
        );
        assert!(
            all.contains("wire the native workflow tools"),
            "the active workflow's own task must reach the session unseeded: {all}"
        );
    }

    #[test]
    fn a_fresh_native_turn_sends_the_operator_prompt_exactly_once() {
        let repo = crate::commands::ctx::testenv::repo();
        let home = tempfile::tempdir().expect("home");
        let state = crate::commands::ctx::state::StateDir::from_root(
            tempfile::tempdir().expect("state").keep(),
        );
        let model = "fixture-anthropic-model";
        let route = route_for(Protocol::AnthropicMessages, model);
        let (_dir, mut journal, session) = journal_for(&route);
        let prompt = "issue-649-unique-native-prompt-7d3c9f";
        let request = HeadlessRequest {
            repo: repo.path(),
            prompt,
            route: None,
            role: "worker",
            limits: NativeLimits::default(),
            session_id: None,
            cancellation: None,
            resume: None,
            provider: None,
            fixture_tools: None,
            task: None,
            writer: None,
            accounting: Accounting::Seat,
        };
        let (system, preamble) = compile_standing_context(
            &state,
            home.path(),
            &Default::default(),
            &request,
            &route,
            &session,
            1,
            &[],
        )
        .expect("the standing context compiles");
        let provider = FixtureProvider::new(
            fixture_target(Protocol::AnthropicMessages, model),
            FixtureScript::from_json(
                r#"{"model":"fixture-anthropic-model","turns":[{"message_id":"msg_1","blocks":[{"type":"text","text":"done"}],"finish_reason":"end_turn"}]}"#,
            )
            .expect("fixture"),
        );
        let mut tools = FixtureToolExecutor::new(
            FixtureToolScript::from_json(r#"{"tools":{}}"#).expect("tools"),
        );
        let mut config = config_for(session, route);
        config.system = system;
        config.preamble = preamble;
        {
            let mut driver = NativeLoop::new(
                config,
                &provider,
                &mut tools,
                &mut journal,
                Arc::new(CancellationFlag::default()),
                &|| 1_000,
                &no_env,
            );
            driver.acknowledge(prompt, false).expect("acknowledged");
            driver.run_to_completion().expect("turn completes");
        }

        let sent = provider.sent();
        assert_eq!(sent.len(), 1);
        let outbound = &sent[0];
        let occurrences = outbound
            .system
            .iter()
            .map(|text| text.matches(prompt).count())
            .sum::<usize>()
            + outbound
                .messages
                .iter()
                .flat_map(|message| &message.content)
                .filter_map(|content| match content {
                    ProviderContent::Text { text } => Some(text.matches(prompt).count()),
                    _ => None,
                })
                .sum::<usize>();
        assert_eq!(occurrences, 1, "outbound request: {outbound:?}");
    }

    #[test]
    fn a_native_preamble_does_not_charge_the_journaled_task_against_optional_context() {
        let repo = tempfile::tempdir().expect("repo");
        std::fs::create_dir(repo.path().join(".zirv")).expect("context directory");
        std::fs::write(
            repo.path().join(".zirv/system-prompt.md"),
            "optional repository context survives the task budget",
        )
        .expect("repository context");
        let home = tempfile::tempdir().expect("home");
        let state = crate::commands::ctx::state::StateDir::from_root(
            tempfile::tempdir().expect("state").keep(),
        );
        let route = route_for(Protocol::AnthropicMessages, "fixture-anthropic-model");
        let session = JournalSessionId::new("native-budget-invariant").expect("session");
        let compile = |prompt: &str| {
            compile_standing_context(
                &state,
                home.path(),
                &Default::default(),
                &HeadlessRequest {
                    repo: repo.path(),
                    prompt,
                    route: None,
                    role: "worker",
                    limits: NativeLimits::default(),
                    session_id: None,
                    cancellation: None,
                    resume: None,
                    provider: None,
                    fixture_tools: None,
                    task: None,
                    writer: None,
                    accounting: Accounting::Seat,
                },
                &route,
                &session,
                1,
                &[],
            )
        };

        let (_, without_task) = compile("").expect("empty task compiles");
        let large_task = "journaled task text ".repeat(100_000);
        let (_, with_large_task) = compile(&large_task).expect("large task compiles");
        assert_eq!(with_large_task, without_task);
        assert!(
            with_large_task
                .iter()
                .any(|text| text.contains("optional repository context survives")),
            "optional context must retain the budget the journaled task no longer consumes"
        );
    }

    /// Review finding on `run_headless` (issue #479 follow-up): `run_
    /// session`'s own doc comment promises its one human line -- a resume's
    /// outcome-unknown reconcile notice -- is routed "somewhere other than
    /// its own single-object stdout" for a `--json` caller. `run_headless`
    /// broke that promise by writing the notice to the SAME writer as the
    /// final status JSON. Seeds a journal session with an execution stuck
    /// `Started` (mid-effect, as if the process had crashed there, exactly
    /// like `a_resume_reconciles_a_started_execution_and_fences_the_old_
    /// generation` above), resumes it end to end through `run_headless`
    /// itself, and asserts the writer it was given holds exactly one
    /// parseable JSON object -- which a leaked notice line ahead of it would
    /// break entirely, since `serde_json::from_slice` accepts no other
    /// content before or after the one value it parses.
    #[test]
    fn a_resume_with_a_reconcile_notice_writes_exactly_one_json_object_to_stdout() {
        use super::super::super::super::state::StateDir;
        use super::super::super::journal::PolicyProvenance;

        let tmp = tempfile::tempdir().expect("tempdir");
        let repo = tempfile::tempdir().expect("repo");
        let state_dir = tmp.path().join("state");
        let state = StateDir::from_root(state_dir.clone());
        let route = route_for(Protocol::AnthropicMessages, "fixture-anthropic-model");
        let session = JournalSessionId::new("native-session-crashed").unwrap();
        {
            let mut journal = Journal::open(&state).expect("open journal");
            journal
                .create_session(&SessionIdentity {
                    session: session.clone(),
                    seat: SeatId::new("seat-crashed").unwrap(),
                    generation: 1,
                    task: None,
                    route: route.clone(),
                    // Issue #639: must match the `repo` the `HeadlessRequest`
                    // below resumes from (canonicalized, the same way
                    // `run_session` canonicalizes it), or the new affinity
                    // check refuses this resume.
                    repo: std::fs::canonicalize(repo.path()).expect("canonicalize repo"),
                    created_at: 1,
                    completed_at: None,
                })
                .unwrap();
            let scope = EventScope::default();
            let call = ToolCallId::new("call_crashed").unwrap();
            let execution = ExecutionId::new("exec_crashed").unwrap();
            journal
                .prepare_tool_call(
                    &session,
                    1,
                    &scope,
                    call.clone(),
                    "apply_patch".into(),
                    serde_json::json!({"path": "src/lib.rs", "patch": "x"}),
                    PolicyProvenance {
                        fingerprint: String::new(),
                        source: "native-loop".into(),
                        decision: "allowed".into(),
                        scope: "worker".into(),
                    },
                    Some(1),
                    1,
                )
                .unwrap();
            journal
                .prepare_execution(
                    &session,
                    1,
                    &scope,
                    execution.clone(),
                    call.clone(),
                    Some(1),
                    1,
                )
                .unwrap();
            journal
                .transition_execution(
                    &session,
                    1,
                    &scope,
                    &execution,
                    ExecutionState::Started,
                    None,
                    None,
                    Some(1),
                    1,
                )
                .unwrap();
            // ... and the process dies here, mid-effect. The journal is
            // closed (end of this block) with the execution still `Started`.
        }

        let env: std::collections::HashMap<String, String> = [(
            crate::commands::ctx::state::STATE_ENV.to_string(),
            state_dir.to_str().expect("utf8").to_string(),
        )]
        .into();
        let lookup = |k: &str| env.get(k).cloned();

        let fixtures = fixture_root();
        let provider = format!(
            "fixture:{}",
            fixtures.join("resume-continue.json").display()
        );
        let mut request = HeadlessRequest {
            repo: repo.path(),
            prompt: "",
            route: None,
            role: "worker",
            limits: NativeLimits::default(),
            session_id: None,
            cancellation: None,
            resume: Some("native-session-crashed"),
            provider: Some(&provider),
            fixture_tools: None,
            task: None,
            writer: None,
            accounting: Accounting::Seat,
        };
        let mut out: Vec<u8> = Vec::new();
        run_headless(&mut request, &mut out, &lookup).expect("resumed run");

        let value: serde_json::Value = serde_json::from_slice(&out).unwrap_or_else(|error| {
            panic!(
                "stdout must be exactly one JSON object, never mixed with the reconcile \
                 notice: {error}: {}",
                String::from_utf8_lossy(&out)
            )
        });
        assert!(value.get("status").is_some(), "{value}");

        // The reconcile really happened -- this is not a vacuous pass.
        let journal = Journal::open(&state).expect("reopen journal");
        let replayed = journal.replay(&session).expect("replay");
        let execution = ExecutionId::new("exec_crashed").unwrap();
        assert_eq!(
            replayed
                .executions
                .get(&execution)
                .map(|record| record.state),
            Some(ExecutionState::OutcomeUnknown),
            "the resume must have reconciled the started execution"
        );
    }

    /// Issue #639: a native session is bound to the repository it started
    /// in. A fresh session is started (through `run_session`, the real
    /// entry point, so `repo` is recorded exactly the way production does
    /// it) in repo A, then `--resume`d from an unrelated repo B -- the
    /// refusal must name repo A (the recorded origin), and must happen
    /// BEFORE the resume mutates anything: the generation the crashed-
    /// execution reconcile test above proves advances must NOT have
    /// advanced here.
    #[test]
    fn resume_from_a_different_repository_is_refused_and_names_the_recorded_origin() {
        let (repo_a, state, _tree, env) = interactive_shutdown_fixture();
        let repo_b = tempfile::tempdir().expect("repo b");
        let lookup = |k: &str| env.get(k).cloned();
        let provider = format!(
            "fixture:{}",
            fixture_root().join("helper-answer.json").display()
        );

        let status = run_session(
            &mut HeadlessRequest {
                repo: repo_a.path(),
                prompt: "do the thing",
                route: None,
                role: "worker",
                limits: NativeLimits::default(),
                session_id: None,
                cancellation: None,
                resume: None,
                provider: Some(&provider),
                fixture_tools: None,
                task: None,
                writer: None,
                accounting: Accounting::Seat,
            },
            &mut Vec::new(),
            &lookup,
        )
        .expect("a fresh native run in repo A completes");

        let resume_provider = format!(
            "fixture:{}",
            fixture_root().join("resume-continue.json").display()
        );
        let error = run_session(
            &mut HeadlessRequest {
                repo: repo_b.path(),
                prompt: "carry on",
                route: None,
                role: "worker",
                limits: NativeLimits::default(),
                session_id: None,
                cancellation: None,
                resume: Some(&status.session),
                provider: Some(&resume_provider),
                fixture_tools: None,
                task: None,
                writer: None,
                accounting: Accounting::Seat,
            },
            &mut Vec::new(),
            &lookup,
        )
        .expect_err("a resume from a different repository must be refused");
        let message = error.to_string();
        assert!(
            message.contains(
                &std::fs::canonicalize(repo_a.path())
                    .unwrap()
                    .display()
                    .to_string()
            ),
            "the refusal must name the recorded origin: {message}"
        );

        // A pure refusal: the generation the resume WOULD have advanced (and
        // the reconcile it would have run) must not have happened.
        let journal = Journal::open(&state).expect("reopen journal");
        let identity = journal
            .session(&JournalSessionId::new(status.session.clone()).unwrap())
            .expect("session still exists");
        assert_eq!(
            identity.generation, 1,
            "a refused resume must not advance the generation"
        );
    }

    /// Issue #639: the companion acceptance criterion -- resume from the
    /// SAME repository the session started in is unaffected by the new
    /// affinity check.
    #[test]
    fn resume_from_the_same_repository_is_unaffected() {
        let (repo, _state, _tree, env) = interactive_shutdown_fixture();
        let lookup = |k: &str| env.get(k).cloned();
        let provider = format!(
            "fixture:{}",
            fixture_root().join("helper-answer.json").display()
        );

        let status = run_session(
            &mut HeadlessRequest {
                repo: repo.path(),
                prompt: "do the thing",
                route: None,
                role: "worker",
                limits: NativeLimits::default(),
                session_id: None,
                cancellation: None,
                resume: None,
                provider: Some(&provider),
                fixture_tools: None,
                task: None,
                writer: None,
                accounting: Accounting::Seat,
            },
            &mut Vec::new(),
            &lookup,
        )
        .expect("a fresh native run completes");

        let resume_provider = format!(
            "fixture:{}",
            fixture_root().join("resume-continue.json").display()
        );
        let resumed = run_session(
            &mut HeadlessRequest {
                repo: repo.path(),
                prompt: "carry on",
                route: None,
                role: "worker",
                limits: NativeLimits::default(),
                session_id: None,
                cancellation: None,
                resume: Some(&status.session),
                provider: Some(&resume_provider),
                fixture_tools: None,
                task: None,
                writer: None,
                accounting: Accounting::Seat,
            },
            &mut Vec::new(),
            &lookup,
        )
        .expect("resume from the same repository is unaffected by the affinity check");
        assert_eq!(resumed.status, NativeStatus::Completed);
        assert_eq!(resumed.session, status.session);
    }

    // -- PR #531 review finding 1 / finding 7: `spawn_interactive` +
    // `InteractiveSession::shutdown` -------------------------------------

    /// Issue #645: a live headless native run (`zirv ctx exec --runtime
    /// native`, driven through `run_session`, the real entry point) must
    /// appear in the session registry -- `explain-status`/`ask`/`nudge`/
    /// `kill`/`session.list` all read it, and previously saw nothing for
    /// this path even while it was running. `run_session` exposes no delay
    /// hook, so this drives it on a real background thread against a
    /// multi-turn, multi-tool-call fixture (real synchronous journal disk
    /// writes per turn), and polls the registry from the main thread for a
    /// bounded window -- long enough to reliably observe it mid-flight
    /// without making the test depend on exact timing. The test's own state
    /// dir is otherwise empty, so ANY record appearing is this run's own.
    #[test]
    fn a_live_headless_native_run_appears_in_the_registry_and_disappears_after() {
        use crate::commands::ctx::sessions;

        let (repo, state, _tree, env) = interactive_shutdown_fixture();
        let repo_path = repo.path().to_path_buf();
        let provider = format!(
            "fixture:{}",
            fixture_root()
                .join("compaction-long-session.json")
                .display()
        );
        let fixture_tools = fixture_root().join("tools-investigate-edit-test.json");

        let worker = std::thread::spawn(move || {
            let lookup = |k: &str| env.get(k).cloned();
            // `CtxResult`'s error side (`Box<dyn Error>`) is not `Send`, so
            // it cannot cross the `JoinHandle` boundary as-is -- flattened
            // to its `Display` text here, which is all this test needs.
            run_session(
                &mut HeadlessRequest {
                    repo: &repo_path,
                    prompt: "fix the failing test",
                    route: None,
                    role: "worker",
                    limits: NativeLimits::default(),
                    session_id: None,
                    cancellation: None,
                    resume: None,
                    provider: Some(&provider),
                    fixture_tools: Some(&fixture_tools),
                    task: None,
                    writer: None,
                    accounting: Accounting::Seat,
                },
                &mut Vec::new(),
                &lookup,
            )
            .map_err(|error| error.to_string())
        });

        let mut seen_live: Option<sessions::Record> = None;
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while std::time::Instant::now() < deadline {
            if let Some((record, _)) = sessions::list(&state)
                .into_iter()
                .find(|(_, liveness)| *liveness == sessions::Liveness::Live)
            {
                seen_live = Some(record);
                break;
            }
            if worker.is_finished() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(1));
        }

        let status = worker
            .join()
            .expect("the run_session thread must not panic")
            .expect("a headless native run completes");

        let record = seen_live.expect("the live run must have appeared in the registry");
        assert_eq!(record.agent, "native");
        assert_eq!(record.verb, sessions::Verb::Exec);
        assert_eq!(record.runtime, RuntimeKind::Native);
        assert_eq!(record.role.as_deref(), Some("worker"));
        assert!(
            !record.reachable,
            "a native session binds no turn-signal socket"
        );

        let after = sessions::list(&state);
        assert!(
            after.is_empty(),
            "the registry record must be gone once the run finished: {after:?}"
        );
        assert_eq!(status.status, NativeStatus::Completed);
    }

    /// Issue #645 review round: the companion negative case. `CallerOwned`
    /// (a delegated `native_worker` run) is deliberately excluded from the
    /// new registration -- the caller that placed it already owns its own
    /// visibility and settlement, and registering a second record here
    /// would risk a conflicting entry over whatever the caller (a dashboard
    /// pane, a coordinator) already filed for the same process. Polls the
    /// registry for the WHOLE run, not just once, so a registration that
    /// only happened to land outside a single check could not slip past.
    #[test]
    fn a_caller_owned_delegated_run_never_appears_in_the_registry_while_live() {
        use crate::commands::ctx::sessions;

        let (repo, state, _tree, env) = interactive_shutdown_fixture();
        let repo_path = repo.path().to_path_buf();
        let provider = format!(
            "fixture:{}",
            fixture_root()
                .join("compaction-long-session.json")
                .display()
        );
        let fixture_tools = fixture_root().join("tools-investigate-edit-test.json");

        let worker = std::thread::spawn(move || {
            let lookup = |k: &str| env.get(k).cloned();
            run_session(
                &mut HeadlessRequest {
                    repo: &repo_path,
                    prompt: "fix the failing test",
                    route: None,
                    role: "worker",
                    limits: NativeLimits::default(),
                    session_id: None,
                    cancellation: None,
                    resume: None,
                    provider: Some(&provider),
                    fixture_tools: Some(&fixture_tools),
                    task: None,
                    writer: None,
                    accounting: Accounting::CallerOwned,
                },
                &mut Vec::new(),
                &lookup,
            )
            .map_err(|error| error.to_string())
        });

        let mut seen: Option<Vec<sessions::Record>> = None;
        loop {
            let records: Vec<sessions::Record> = sessions::list(&state)
                .into_iter()
                .map(|(record, _)| record)
                .collect();
            if !records.is_empty() {
                seen = Some(records);
                break;
            }
            if worker.is_finished() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(1));
        }

        let status = worker
            .join()
            .expect("the run_session thread must not panic")
            .expect("a caller-owned headless native run completes");

        assert_eq!(
            seen, None,
            "a CallerOwned run must never register a registry record"
        );
        assert!(sessions::list(&state).is_empty());
        assert_eq!(status.status, NativeStatus::Completed);
    }

    /// Issue #554 (review round 1): the same four obligations for a headless
    /// `zirv ctx exec --runtime native` run, driven through `run_session`.
    #[test]
    fn headless_native_exec_records_health_and_settles_pool_spend() {
        use crate::commands::ctx::health::{Observed, RouteKey, RouteScope};
        use crate::commands::ctx::{health_store, log};

        let (repo, state, _tree, env) = interactive_shutdown_fixture();
        let cfg = crate::commands::ctx::config::CtxConfig::default();
        let policy = cfg.fallback.effective_health();
        let lookup = |k: &str| env.get(k).cloned();
        let provider = format!(
            "fixture:{}",
            fixture_root().join("helper-answer.json").display()
        );

        let status = run_session(
            &mut HeadlessRequest {
                repo: repo.path(),
                prompt: "do the thing",
                route: None,
                role: "worker",
                limits: NativeLimits::default(),
                session_id: None,
                cancellation: None,
                resume: None,
                provider: Some(&provider),
                fixture_tools: None,
                task: None,
                writer: None,
                accounting: Accounting::Seat,
            },
            &mut Vec::new(),
            &lookup,
        )
        .expect("a headless native run completes");

        let endpoint = RouteKey::scoped(RouteScope::Endpoint, &status.endpoint);
        // Same shape as the interactive test: a record must exist for the
        // success to fold into, so one is seeded and the CHANGE is asserted.
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
        crate::commands::ctx::native_account::record_route_health(&state, &cfg, &status, 3);
        assert_ne!(
            health_store::load(&state, &endpoint, 3),
            before,
            "a headless run's outcome reaches the persistent breaker for its endpoint"
        );

        assert_eq!(
            crate::commands::ctx::reservation::outstanding(&state, &status.billing_pool, 0),
            0,
            "the run's estimate is settled against the pool"
        );
        let rows = log::read_delegations(&state, 20);
        let row = rows
            .iter()
            .find(|row| row.session == status.session)
            .expect("a headless native run appears in the ledger zirv ctx spend reads");
        assert_eq!(row.agent, "native");
        assert!(
            row.input_tokens + row.output_tokens > 0,
            "with the tokens the provider actually metered"
        );
    }

    /// Issue #554 (integration review): a HARD loop error still settles the
    /// usage earlier turns of the same loop were already billed for.
    ///
    /// `run_to_completion` used to return straight out of the loop on a hard
    /// `Err` -- not a captured `TurnState::Failed`, which it always handled
    /// -- so the estimate was released and the real spend vanished from
    /// `zirv ctx spend`. Driven through `run_session`, the production entry,
    /// with the loop's own `#[cfg(test)]` abort seam standing in for the two
    /// real hard-error paths (a journal write and a transport rebuild),
    /// neither of which the fixture provider can produce.
    #[test]
    fn a_hard_turn_error_still_settles_usage_from_earlier_turns() {
        use crate::commands::ctx::log;

        let (repo, state, _tree, mut env) = interactive_shutdown_fixture();
        // One good turn is billed, then the next one aborts hard.
        env.insert(ABORT_AFTER_TURNS_ENV.to_string(), "1".to_string());
        let lookup = |k: &str| env.get(k).cloned();
        let provider = format!(
            "fixture:{}",
            fixture_root().join("helper-answer.json").display()
        );

        let failed = run_session(
            &mut HeadlessRequest {
                repo: repo.path(),
                prompt: "do the thing",
                route: None,
                role: "worker",
                limits: NativeLimits::default(),
                session_id: None,
                cancellation: None,
                resume: None,
                provider: Some(&provider),
                fixture_tools: None,
                task: None,
                writer: None,
                accounting: Accounting::Seat,
            },
            &mut Vec::new(),
            &lookup,
        );
        let error = failed.expect_err("the injected hard error must propagate");
        let aborted = error
            .downcast_ref::<AbortedRun>()
            .expect("a hard abort carries what the loop already billed");
        let billed = aborted.status.usage.input_tokens + aborted.status.usage.output_tokens;
        assert!(
            billed > 0,
            "the fixture must actually bill a turn before the abort, or this asserts nothing"
        );

        let rows = log::read_delegations(&state, 20);
        let row = rows
            .iter()
            .find(|row| row.session == aborted.status.session)
            .expect("an aborted run still reaches the ledger zirv ctx spend reads");
        assert_eq!(
            row.input_tokens + row.output_tokens,
            billed,
            "carrying the tokens the earlier turn was billed for, not zero"
        );
        assert_eq!(
            crate::commands::ctx::reservation::outstanding(&state, &aborted.status.billing_pool, 0),
            0,
            "and the estimate is settled, not merely released"
        );
    }

    /// Issue #554 (review round 2): a seat run's estimate is released when
    /// the run does not reach its settlement.
    ///
    /// `run_session` has two `?`s between taking the estimate and settling it
    /// -- the loop's own failure and a journal that cannot be completed --
    /// and before this guard either of them left the pool short for the rest
    /// of the state directory's life. Neither `?` is reachable with the
    /// fixture provider (an exhausted or unusable script is a typed PROVIDER
    /// failure, which is a journaled `Failed` status that settles normally,
    /// not an `Err`), so the guard itself is what is driven here: the two
    /// outcomes it exists to tell apart.
    #[test]
    fn headless_native_exec_releases_its_reservation_on_failure() {
        let (_repo, state, _tree, _env) = interactive_shutdown_fixture();
        let reserve = || {
            crate::commands::ctx::native_account::reserve_seat_turn(
                &state, "work", "sess-1", 4_096, 0,
            )
        };

        // Dropped without settling -- the run never got there.
        {
            let _guard = SeatReservation {
                state: &state,
                held: reserve(),
            };
            assert_eq!(
                crate::commands::ctx::reservation::outstanding(&state, "work", 0),
                4_096,
                "the estimate is genuinely held while the run is in flight"
            );
        }
        assert_eq!(
            crate::commands::ctx::reservation::outstanding(&state, "work", 0),
            0,
            "a run that never reached its settlement leaves nothing outstanding"
        );

        // Taken by the settlement -- the drop must NOT release it a second
        // time, or a settled reservation would be resolved twice.
        let taken = {
            let mut guard = SeatReservation {
                state: &state,
                held: reserve(),
            };
            guard.take()
        };
        let (pool, id) = taken.expect("the settlement receives the reservation");
        assert_eq!(
            crate::commands::ctx::reservation::settle(&state, &pool, &id, 10).expect("settle"),
            Some(4_096),
            "the settlement still finds it: the guard handed it over rather than releasing it"
        );
    }

    /// Drives the same fixture through the real CLI entry point with the
    /// quality-driven retained-tail default, proving overflow recovery does
    /// not depend on lowering that default.
    #[test]
    fn headless_native_exec_recovers_a_first_turn_overflow_with_the_cli_defaults() {
        let (repo, _state, _tree, env) = interactive_shutdown_fixture();
        let lookup = |k: &str| env.get(k).cloned();
        let provider = format!(
            "fixture:{}",
            fixture_root()
                .join("compaction-overflow-recovery.json")
                .display()
        );
        let fixture_tools = fixture_root().join("tools-investigate-edit-test.json");

        let status = run_session(
            &mut HeadlessRequest {
                repo: repo.path(),
                prompt: "fix the failing test",
                route: None,
                role: "worker",
                limits: NativeLimits::default(),
                session_id: None,
                cancellation: None,
                resume: None,
                provider: Some(&provider),
                fixture_tools: Some(&fixture_tools),
                task: None,
                writer: None,
                accounting: Accounting::Seat,
            },
            &mut Vec::new(),
            &lookup,
        )
        .expect("a headless native run completes");

        assert_eq!(
            status.status,
            NativeStatus::Completed,
            "evidence: {:?}",
            status.evidence
        );
        assert_eq!(status.compactions.len(), 1);
        assert!(
            status.compactions[0].reason.contains("context_overflow"),
            "reason was {:?}",
            status.compactions[0].reason
        );
    }

    /// Review finding on issue #485: `run_session`'s coordinator auto-resume
    /// block (right after `now` is minted, ahead of `build_transport`) has
    /// never been driven end to end -- every existing coordinator-resume
    /// test calls `coordinator::consume_pending` directly. Pre-seeds a graph
    /// with one node dispatched to a delegation, publishes that delegation's
    /// terminal receipt, then runs a real `role: "coordinator"` session
    /// through `run_session` and asserts (a) the writer carries the "resumed
    /// the coordinator graph" notice and (b) the node settles exactly once:
    /// a second session against the same repository finds nothing pending
    /// and never re-settles it.
    #[test]
    fn a_coordinator_session_resumes_its_graph_and_settles_a_node_exactly_once() {
        use crate::commands::ctx::config::CtxConfig;
        use crate::commands::ctx::state::StateDir;
        use crate::commands::ctx::{coordinator, delegation, team};

        let tmp = tempfile::tempdir().expect("tempdir");
        let repo = tempfile::tempdir().expect("repo");
        let state_dir = tmp.path().join("state");
        let state = StateDir::from_root(state_dir.clone());

        // Seed the plan: one node dispatched to a delegation that already
        // finished -- exactly the shape a coordinator that restarted
        // mid-flight would find.
        let mut graph = coordinator::Coordinator::default();
        graph.plan("task-1", team::IMPLEMENTER, &[], 1);
        graph.dispatched("task-1", team::IMPLEMENTER, "native", "deleg-1", 2);
        coordinator::store(&state, repo.path(), &graph).expect("seed the graph");

        delegation::record_launch(
            &state,
            repo.path(),
            delegation::WorkerHandle {
                delegation: "deleg-1".to_string(),
                attempt: 1,
                runtime: RuntimeKind::Native,
                worker_session: "deleg-1-session".to_string(),
                short: "short1".to_string(),
                role: team::IMPLEMENTER.to_string(),
                task: Some("task-1".to_string()),
                group: None,
                objective: None,
                workdir: repo.path().to_path_buf(),
                manifest: None,
                plan_override: false,
            },
            Some("coord-session".to_string()),
            10,
        )
        .expect("launch receipt");
        delegation::publish_terminal(
            &state,
            repo.path(),
            &CtxConfig::default(),
            "deleg-1",
            delegation::Phase::Completed,
            Some(0),
            Some("done".to_string()),
            Some(std::path::PathBuf::from("results/task-1.json")),
            20,
        )
        .expect("publish the receipt");

        let env: std::collections::HashMap<String, String> = [(
            crate::commands::ctx::state::STATE_ENV.to_string(),
            state_dir.to_str().expect("utf8").to_string(),
        )]
        .into();
        let lookup = |k: &str| env.get(k).cloned();

        let fixtures = fixture_root();
        let provider = format!(
            "fixture:{}",
            fixtures.join("resume-continue.json").display()
        );

        // First run: the coordinator picks its graph back up.
        let mut request = HeadlessRequest {
            repo: repo.path(),
            prompt: "carry on",
            route: None,
            role: team::COORDINATOR,
            limits: NativeLimits::default(),
            session_id: None,
            cancellation: None,
            resume: None,
            provider: Some(&provider),
            fixture_tools: None,
            task: None,
            writer: None,
            accounting: Accounting::Seat,
        };
        let mut out: Vec<u8> = Vec::new();
        run_session(&mut request, &mut out, &lookup).expect("first coordinator session");
        assert!(
            String::from_utf8_lossy(&out)
                .contains("resumed the coordinator graph -- consumed 1 pending worker receipt(s)"),
            "{}",
            String::from_utf8_lossy(&out)
        );

        let resumed = coordinator::load(&state, repo.path());
        assert_eq!(
            resumed.nodes["task-1"].state,
            coordinator::NodeState::Completed,
            "the pending receipt must settle the node"
        );

        // Second run, a fresh session against the same repository: nothing
        // is pending anymore, so the node is never re-settled and no notice
        // is written.
        let mut request2 = HeadlessRequest {
            repo: repo.path(),
            prompt: "carry on again",
            route: None,
            role: team::COORDINATOR,
            limits: NativeLimits::default(),
            session_id: None,
            cancellation: None,
            resume: None,
            provider: Some(&provider),
            fixture_tools: None,
            task: None,
            writer: None,
            accounting: Accounting::Seat,
        };
        let mut out2: Vec<u8> = Vec::new();
        run_session(&mut request2, &mut out2, &lookup).expect("second coordinator session");
        assert!(
            !String::from_utf8_lossy(&out2).contains("resumed the coordinator graph"),
            "a second run must not find anything pending: {}",
            String::from_utf8_lossy(&out2)
        );

        let again = coordinator::load(&state, repo.path());
        assert_eq!(
            again.nodes["task-1"].state,
            coordinator::NodeState::Completed,
            "a second run must not re-settle the already-settled node"
        );
    }

    // -- issue #538 (chunk B): scoped nested loading, recompile-on-change --
}
