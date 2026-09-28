//! CLI arguments, runtime selection, and execution entry points.

use super::*;

#[derive(Debug, Clone, clap::Args)]
pub struct ExecArgs {
    /// Adapter name: claude or codex. Detected from the command when omitted.
    #[arg(long)]
    pub agent: Option<String>,
    /// Session id of the supervised run, used to locate its transcript.
    #[arg(long)]
    pub session_id: Option<String>,
    /// Transcript path, when the agent writes somewhere the adapter cannot derive.
    #[arg(long)]
    pub transcript: Option<PathBuf>,
    /// Prompt to reuse on restart. Extracted from the command when omitted.
    #[arg(long)]
    pub prompt: Option<String>,
    /// Restart budget before giving up.
    #[arg(long)]
    pub max_restarts: Option<u32>,
    /// Wall-clock limit for the whole supervised run.
    #[arg(long)]
    pub timeout_secs: Option<u64>,
    /// Token ceiling for this run (issue #155, Phase 5(d)). Checkpoints at
    /// `agent::BUDGET_SOFT_FRACTION` of the ceiling and stops -- never
    /// restarted, and never a signal to change models. `None` is unbounded.
    #[arg(long)]
    pub budget_tokens: Option<u64>,
    /// Tool-call ceiling for this run, independent of `budget_tokens`.
    #[arg(long)]
    pub max_tool_calls: Option<u32>,
    /// Sets (or replaces) this repository's durable objective (issue #285)
    /// before the run starts -- a shorthand for `zirv ctx objective set`.
    /// Its own soft budget defaults from `[pace] run_budget_tokens`; use
    /// `zirv ctx objective set --budget-tokens`/`--deadline-secs` directly
    /// for a per-run ceiling.
    #[arg(long)]
    pub objective: Option<String>,
    /// Which runtime drives the conversation: `harness` (zirv supervises an
    /// external coding-agent process) or `native` (issue #478, roadmap N09 --
    /// zirv conducts the model/tool conversation itself over a direct
    /// provider route, with no coding harness installed at all). Native mode
    /// is explicit and opt-in: it is never selected by detection.
    ///
    /// The default, `configured` (issue #491), means "whatever `[runtime]` in
    /// `~/.zirv/ctx.toml` says, harness when it says nothing" -- so an
    /// operator config written before that key existed behaves exactly as it
    /// always did. See `runtime::resolve`.
    #[arg(long, default_value = super::runtime::CONFIGURED)]
    pub runtime: String,
    /// Native runtime only: which `[route]` from the operator's own native
    /// provider configuration to spend. Defaults to the `[roles]` entry for
    /// this run's seat role.
    #[arg(long)]
    pub route: Option<String>,
    /// Native runtime only: the session's role, which selects the default
    /// route and the repository-write posture applied to its tools.
    #[arg(long, default_value = "worker")]
    pub role: String,
    /// Native runtime only: continue an existing native journal session
    /// instead of starting a new one. Every execution that was still running
    /// when that session stopped is reconciled as outcome-unknown (never
    /// silently retried) and the generation is advanced, fencing out anything
    /// still holding the old one.
    #[arg(long)]
    pub resume: Option<String>,
    /// Issue #480 (roadmap N11): native runtime only. `json` (the default)
    /// prints exactly the structured final status this flag's absence
    /// always printed -- the JSON contract is unchanged. `plain` ADDITIONALLY
    /// renders the session's transcript through `dash::native_pane`'s own
    /// non-ratatui renderer (the same view model the dashboard pane draws,
    /// with no terminal required) after the JSON status line, so a native
    /// session is inspectable without ratatui at all -- piped output, a CI
    /// log, or a terminal too small for the dashboard.
    #[arg(long, default_value = "json")]
    pub view: String,
    /// Native runtime only, operator-only: replace the live provider with a
    /// deterministic fixture script. The only accepted value is
    /// `fixture:<path>`. No configuration layer can set this -- least of all
    /// a repository's -- because it is a command-line flag and nothing else.
    #[arg(long)]
    pub provider: Option<String>,
    /// Native runtime only: the fixture tool script a `--provider fixture:`
    /// run executes against. Without it every tool call reports a fixture
    /// failure rather than touching the machine.
    #[arg(long)]
    pub fixture_tools: Option<PathBuf>,
    /// The headless agent command, after `--`.
    #[arg(allow_hyphen_values = true, last = true)]
    pub command: Vec<String>,
    /// Simple run: skip every zirv-injected instruction, including the shipped
    /// default. Supervision, pacing and hooks still apply.
    #[arg(long, default_value_t = false)]
    pub simple: bool,
    /// NOT a CLI flag -- `#[arg(skip)]` always leaves this at its default
    /// (`None`) on `zirv ctx exec`'s own command line, the same reasoning
    /// `memory::RememberArgs::importance`'s own doc comment gives. Issue
    /// #358 (task T3): the id of the calling delegation's own entry in
    /// `reservation`'s per-provider ledger (`agent::run_with` sets it when
    /// building this struct for a headless worker's launch), carried
    /// through so a harness-handover restart, below, can release it against
    /// the OLD provider and reserve a fresh one against the NEW provider it
    /// is about to continue on -- `None` for a plain `zirv ctx exec` with no
    /// delegation reservation of its own, which this never creates one for.
    #[arg(skip)]
    pub reservation_id: Option<String>,
    /// Internal cancellation shared with a delegation record watcher.
    #[arg(skip)]
    pub cancellation: Option<std::sync::Arc<super::provider::adapter::CancellationFlag>>,
}

/// The same defaults clap itself applies, so a caller that builds this struct
/// in code (a delegation fold, an `agent:` script step, a test) gets the
/// harness runtime and the worker role without restating them -- and a field
/// added here later cannot silently become `""` at those call sites.
impl Default for ExecArgs {
    fn default() -> Self {
        Self {
            agent: None,
            session_id: None,
            transcript: None,
            prompt: None,
            max_restarts: None,
            timeout_secs: None,
            budget_tokens: None,
            max_tool_calls: None,
            objective: None,
            runtime: super::runtime::RuntimeKind::Harness.to_string(),
            route: None,
            role: "worker".to_string(),
            view: "json".to_string(),
            resume: None,
            provider: None,
            fixture_tools: None,
            command: Vec::new(),
            simple: false,
            reservation_id: None,
            cancellation: None,
        }
    }
}

/// T11: real-clock wrapper. `run_with_clock` (below) does the actual work;
/// this hands it the two real-world functions -- `state::now_secs` and a
/// genuine `std::thread::sleep` -- so every caller outside this module
/// (`script_runner::AgentCommand`, `agent.rs`, `dash`'s headless spawn
/// fallback) keeps calling `run_with` exactly as before, with no signature
/// change to ripple through.
pub fn run_with<W: Write>(
    args: &ExecArgs,
    w: &mut W,
    repo: &Path,
    env: EnvLookup<'_>,
) -> CtxResult<i32> {
    // Issue #478: `--runtime native` conducts the conversation in-process
    // instead of supervising a harness, and shares nothing with the spawn
    // path below -- no adapter, no argv, no PTY, no transcript to score. It
    // is matched here, before any of that work starts, and the branch is
    // explicit: an unrecognised value is an error, never a silent fall back
    // to the harness.
    match args.runtime.parse::<super::runtime::RuntimeKind>() {
        Ok(super::runtime::RuntimeKind::Native) => return run_native(args, w, repo, env),
        Ok(super::runtime::RuntimeKind::Harness) => {}
        _ => {
            return Err(format!(
                "--runtime '{}': expected `harness` or `native`",
                args.runtime
            )
            .into());
        }
    }
    run_with_clock(
        args,
        w,
        repo,
        env,
        &super::state::now_secs,
        &|d: Duration| std::thread::sleep(d),
    )
}

/// `zirv ctx exec --runtime native` (issue #478, roadmap N09). The prompt is
/// `--prompt`, or the trailing `-- <text>` words when no `--prompt` is given;
/// `--agent`, `--transcript`, `--session-id` and the restart/rot flags have no
/// meaning here and are refused rather than silently ignored, because a native
/// session has no external process to restart or transcript to score.
fn run_native<W: Write>(
    args: &ExecArgs,
    w: &mut W,
    repo: &Path,
    env: EnvLookup<'_>,
) -> CtxResult<i32> {
    for (name, present) in [
        ("--agent", args.agent.is_some()),
        ("--transcript", args.transcript.is_some()),
        ("--session-id", args.session_id.is_some()),
        ("--max-restarts", args.max_restarts.is_some()),
    ] {
        if present {
            return Err(format!(
                "{name} is a harness-runtime flag; a native session supervises no external \
                 process"
            )
            .into());
        }
    }
    let prompt = match args.prompt.as_deref() {
        Some(prompt) => prompt.to_string(),
        None => args.command.join(" "),
    };
    // A resume continues a conversation that already has everything it needs,
    // so a fresh prompt is optional there and mandatory everywhere else.
    if prompt.trim().is_empty() && args.resume.is_none() {
        return Err("native runtime: pass a prompt with --prompt or after `--`".into());
    }
    if args.fixture_tools.is_some() && args.provider.is_none() {
        return Err("--fixture-tools needs --provider fixture:<path>".into());
    }

    if !matches!(args.view.as_str(), "json" | "plain") {
        return Err(format!("--view '{}': expected `json` or `plain`", args.view).into());
    }

    let mut limits = super::runtime::native::NativeLimits::default();
    if let Some(max_tool_calls) = args.max_tool_calls {
        limits.max_tool_calls = max_tool_calls;
    }
    if let Some(timeout_secs) = args.timeout_secs {
        limits.max_wall_ms = timeout_secs.saturating_mul(1000);
    }
    // Issue #637: was accepted by clap but never read on the native path, so
    // `--budget-tokens` silently did nothing (`--max-tool-calls` on the same
    // request correctly stopped the loop).
    if let Some(budget_tokens) = args.budget_tokens {
        limits.max_budget_tokens = Some(budget_tokens);
    }
    let mut request = super::runtime::native::HeadlessRequest {
        repo,
        prompt: prompt.trim(),
        route: args.route.as_deref(),
        role: &args.role,
        limits,
        session_id: None,
        cancellation: None,
        resume: args.resume.as_deref(),
        provider: args.provider.as_deref(),
        fixture_tools: args.fixture_tools.as_deref(),
        // Issue #479: a plain `zirv ctx exec --runtime native` is not a
        // delegation. It holds no task card and no writer permit, so its
        // repository writes are refused rather than silently unbacked --
        // `zirv agent --runtime native` is the surface that grants both.
        task: None,
        writer: None,
        accounting: super::runtime::native::Accounting::Seat,
    };

    if args.view != "plain" {
        return super::runtime::native::run_headless(&mut request, w, env);
    }

    // `--view plain`: the exact same JSON status `run_headless` always
    // printed (the contract is unchanged), followed by the same view model
    // `dash::native_pane`'s ratatui renderer draws, rendered as plain text
    // -- no terminal, no ratatui, required to read it.
    let mut notices: Vec<u8> = Vec::new();
    let status = super::runtime::native::run_session(&mut request, &mut notices, env)?;
    if !notices.is_empty() {
        eprint!("{}", String::from_utf8_lossy(&notices));
    }
    writeln!(w, "{}", serde_json::to_string_pretty(&status)?)?;

    let state = super::state::StateDir::resolve(env)?;
    let plain = render_native_session_plain(&state, &status, repo);
    match plain {
        Ok(text) => {
            writeln!(w)?;
            write!(w, "{text}")?;
        }
        Err(error) => {
            eprintln!("--view plain: could not render the transcript: {error}");
        }
    }
    Ok(status.exit_code)
}

/// Replays `session`'s journal and renders it through `dash::native_pane`'s
/// own plain-text renderer -- exactly the view model a dashboard native pane
/// draws, over the SAME reducer (`build_transcript`) both surfaces share, so
/// this headless view and the dashboard's live one never diverge in what a
/// tool call, a diff or a test outcome looks like.
fn render_native_session_plain(
    state: &super::state::StateDir,
    status: &super::runtime::native::NativeFinalStatus,
    repo: &Path,
) -> CtxResult<String> {
    use super::dash::native_pane::{
        NativePresentation, StatusFacts, build_transcript, render_plain, resolve_billing,
    };
    use super::runtime::journal::{Journal, JournalSessionId};

    let journal = Journal::open(state)?;
    let session_id = JournalSessionId::new(status.session.clone())?;
    let identity = journal.session(&session_id)?;
    let conversation = journal.replay(&session_id)?;
    let view = build_transcript(&conversation);
    let (session_state, blocked, unread_result) = plain_status_projection(
        status.status,
        status.blocked_reason.is_some(),
        status.final_text.is_some(),
    );
    let facts = StatusFacts {
        model: format!(
            "{}/{}",
            identity.route.model.vendor, identity.route.model.id
        ),
        route: identity.route.route.to_string(),
        runtime: "native".to_string(),
        billing: resolve_billing(&identity.route, repo),
        session_state,
        turn_state: None,
        blocked,
        unread_result,
        notice: None,
        activity: None,
        cwd: repo.display().to_string(),
        git_branch: None,
        context_left_pct: None,
    };
    Ok(render_plain(
        &view,
        &NativePresentation::default(),
        &facts,
        100,
    ))
}

fn plain_status_projection(
    status: super::runtime::native::NativeStatus,
    blocked: bool,
    unread: bool,
) -> (super::runtime::native::SessionState, bool, bool) {
    use super::runtime::native::{NativeStatus, SessionState};
    let state = match status {
        NativeStatus::Completed => SessionState::Completed,
        NativeStatus::Interrupted => SessionState::Interrupted,
        NativeStatus::Incomplete | NativeStatus::LimitReached | NativeStatus::Failed => {
            SessionState::Failed
        }
    };
    (state, blocked, unread)
}

/// Same supervised execution as [run_with], plus the per-harness accounting
/// segments that make a cross-harness continuation observable to its caller.
pub fn run_with_report<W: Write>(
    args: &ExecArgs,
    w: &mut W,
    repo: &Path,
    env: EnvLookup<'_>,
) -> CtxResult<(i32, ExecutionReport)> {
    let mut report = ExecutionReport::default();
    let code = run_with_clock_inner(
        args,
        w,
        repo,
        env,
        &super::state::now_secs,
        &|d: Duration| std::thread::sleep(d),
        None,
        true,
        &mut report,
        &adapters::liveness_probe,
    )?;
    Ok((code, report))
}

/// T11 (sleep injection): identical to the former run_with in every way
/// except now_fn/sleep_fn are now parameters instead of a real-clock
/// closure hardcoded two calls deep in the body.
pub(crate) fn run_with_clock<W: Write>(
    args: &ExecArgs,
    w: &mut W,
    repo: &Path,
    env: EnvLookup<'_>,
    now_fn: &dyn Fn() -> u64,
    sleep_fn: &dyn Fn(Duration),
) -> CtxResult<i32> {
    run_with_clock_and_presence(
        args,
        w,
        repo,
        env,
        now_fn,
        sleep_fn,
        &adapters::liveness_probe,
    )
}

/// Issue #690 (remaining scope): [`run_with_clock`] with the launch
/// pre-flight's one machine-dependent input injected -- whether the resolved
/// adapter's program is actually installed -- in the same style
/// `adapters::resolve_default_with_presence`/`select_with_presence` already
/// expose. A test of the pre-flight at this entry point (the one `zirv ctx
/// agent` delegates to) states the machine it assumes rather than inheriting
/// the developer's own `PATH`, which is also the only way to assert the
/// *absence* of the pacing and Keychain lines without a real 60-second sleep
/// and a really uninstalled harness.
#[allow(clippy::too_many_arguments)]
pub(crate) fn run_with_clock_and_presence<W: Write>(
    args: &ExecArgs,
    w: &mut W,
    repo: &Path,
    env: EnvLookup<'_>,
    now_fn: &dyn Fn() -> u64,
    sleep_fn: &dyn Fn(Duration),
    present: &dyn Fn(&str, &str) -> adapters::Liveness,
) -> CtxResult<i32> {
    let mut report = ExecutionReport::default();
    run_with_clock_inner(
        args,
        w,
        repo,
        env,
        now_fn,
        sleep_fn,
        None,
        true,
        &mut report,
        present,
    )
}

pub fn run<W: Write>(args: &ExecArgs, w: &mut W) -> CtxResult<i32> {
    let repo = std::env::current_dir()?;
    let ambient = env_from_process();
    // Issue #249/#250 review: this is a DIRECT CLI entry, not a supervisor
    // spawn seam -- only `agent::run_with`'s fold (headless delegation) and
    // dash's `verified_parent` (pane spawn) may ever establish parent
    // lineage. A worker's own ambient process env may still carry a
    // `PARENT_SESSION_ENV` it inherited from whatever launched IT (e.g. a
    // worker with a real parent running this as a raw shell command rather
    // than through `zirv agent`); left alone, that stale value would render
    // its own parent's mail as steering for this brand new session, and
    // export it onward to this session's own child. Scrubbed here with
    // `agent::parent_session_env`'s own unconditional-substitute fold
    // (`parent: None`), so a direct launch always resolves to no parent --
    // fail-closed to peer trust.
    let env = agent::parent_session_env(&ambient, None);
    // Issue #491: the CLI entry is where the operator's opt-in `[runtime]`
    // default becomes an explicit backend, so everything below -- `run_with`,
    // the native branch, `script_runner`'s own direct callers -- keeps seeing
    // one of exactly two literal values and never has to resolve anything.
    let choice = resolved_runtime(args, &repo, &env)?;
    if let Some(note) = &choice.note {
        eprintln!("zirv ctx exec: {note}");
    }
    let mut args = args.clone();
    args.runtime = choice.kind.as_str().to_string();
    // Round 4B (stdout/stderr separation): a harness launch's child inherits
    // nothing of its own -- `supervise::forward` echoes the CHILD's stdout
    // line by line straight to this process's own real `std::io::stdout()`,
    // on its own thread, entirely independent of `w`. Every "zirv ctx exec:
    // ..." notice the harness path (`run_with_clock`/`run_with_clock_inner`)
    // writes to `w` used to go to that SAME real stdout too -- on this, the
    // one production call site (`mod.rs`'s `CtxVerb::Exec` dispatch), `w` IS
    // `std::io::Stdout` -- racing the forwarding thread and landing a notice
    // (`pace::wait_for_window`'s usage-limit line, a restart/nudge/backoff
    // line, and so on) in front of a child's own `--output-format json`
    // stream, which a machine consumer piping this process's stdout cannot
    // recover from. `--runtime native`'s `w` argument IS the contract
    // instead (`--view json/plain`'s structured result, `run_native`'s own
    // `writeln!(w, ...)` calls), so only the harness branch is redirected --
    // this mirrors `run_with`'s own branch exactly, just choosing the writer
    // per arm rather than sharing one across both. `run_with` itself (and
    // every other caller -- `script_runner::agent_command`, every test that
    // still passes its own buffer) is untouched: this function alone owns
    // the real-process CLI entry, so redirecting here cannot perturb a test
    // that asserts on a harness notice via its own `Vec<u8>` writer through
    // `run_with`/`run_with_clock` directly.
    match choice.kind {
        super::runtime::RuntimeKind::Native => run_native(&args, w, &repo, &env),
        super::runtime::RuntimeKind::Harness => {
            // Issue #800: `run_with_clock` itself discards its own
            // `ExecutionReport` (every OTHER caller of this arm's own
            // underlying `run_with_clock_and_presence` never needed the
            // segments it collects) -- this is the one call site that does,
            // to learn the actual session/harness/model a `Direct` outcome
            // row (below) would otherwise have to re-derive from env.
            let mut report = ExecutionReport::default();
            let code = run_with_clock_inner(
                &args,
                &mut std::io::stderr(),
                &repo,
                &env,
                &super::state::now_secs,
                &|d: Duration| std::thread::sleep(d),
                None,
                true,
                &mut report,
                &adapters::liveness_probe,
            );
            record_direct_outcome_if_needed(&repo, &env, &report);
            code
        }
        super::runtime::RuntimeKind::Unknown => Err(format!(
            "--runtime '{}': expected `harness` or `native`",
            args.runtime
        )
        .into()),
    }
}

/// Issue #800: best-effort `Direct`-outcome recording for a headless launch
/// that never ran a workflow -- gated by the SAME `[workflow]
/// telemetry_enabled` switch `outcomes::record_terminal` itself checks, and
/// never makes a provider call (`OutcomeRow::direct` only reads local config/
/// env). `let _ =`/early-return throughout: this must never affect the
/// launch's own exit code or block on a missing state directory/telemetry
/// opt-out.
fn record_direct_outcome_if_needed(repo: &Path, env: EnvLookup<'_>, report: &ExecutionReport) {
    let Some(segment) = report.segments.last() else {
        return;
    };
    if !crate::commands::workflow::telemetry::TelemetryConfig::for_repo(repo).enabled {
        return;
    }
    let Ok(state) = StateDir::resolve(env) else {
        return;
    };
    if crate::commands::workflow::outcomes::has_any_row_for_session(&state, &segment.session) {
        return;
    }
    let row = crate::commands::workflow::outcomes::OutcomeRow::direct(
        &segment.session,
        Some(segment.agent.as_str()),
        segment.model.as_deref(),
    );
    let _ = crate::commands::workflow::outcomes::append(&state, &row);
}

/// Issue #491: the `[runtime]` resolution `run` applies before anything
/// downstream sees a runtime value at all.
///
/// Split out of `run` for one reason: the ROLE it keys on is a real decision,
/// and hardcoding `"worker"` here would silently resolve `zirv ctx exec
/// --role reviewer` against the `worker` row of `[runtime.roles]`. `args.role`
/// is this run's own seat role -- the same key `agent::run` passes -- and a
/// test pins that rather than the resolution ladder underneath it (which
/// `runtime::tests` already owns).
fn resolved_runtime(
    args: &ExecArgs,
    repo: &Path,
    env: EnvLookup<'_>,
) -> CtxResult<super::runtime::RuntimeChoice> {
    super::runtime::resolve_for_cli(&args.runtime, repo, env, &args.role)
}

#[cfg(test)]
mod tests {
    use super::super::tests::*;
    use super::*;
    use std::collections::HashMap;

    #[test]
    fn plain_native_view_matches_structured_status() {
        use super::super::runtime::native::{NativeStatus, SessionState};

        for (status, expected) in [
            (NativeStatus::Completed, SessionState::Completed),
            (NativeStatus::Incomplete, SessionState::Failed),
            (NativeStatus::Interrupted, SessionState::Interrupted),
            (NativeStatus::LimitReached, SessionState::Failed),
            (NativeStatus::Failed, SessionState::Failed),
        ] {
            assert_eq!(plain_status_projection(status, false, false).0, expected);
        }
        assert_eq!(
            plain_status_projection(NativeStatus::Incomplete, true, false),
            (SessionState::Failed, true, false)
        );
        assert_eq!(
            plain_status_projection(NativeStatus::Completed, false, true),
            (SessionState::Completed, false, true)
        );
    }

    /// PR #546 review finding 1: `run` used to resolve the configured
    /// runtime default against a hardcoded `"worker"`, so an exec launched
    /// as some other seat read the wrong row of `[runtime.roles]`. The
    /// resolution has to key on THIS run's `--role`.
    #[test]
    fn exec_resolves_the_configured_default_against_its_own_role() {
        let home = tempfile::tempdir().expect("home");
        let _home = super::super::testenv::HomeGuard::set(home.path());
        let repo = super::super::testenv::repo();
        std::fs::create_dir_all(home.path().join(".zirv")).expect("mkdir");
        std::fs::write(
            home.path().join(".zirv").join("ctx.toml"),
            "[runtime.roles]\nreviewer = 'native'\nworker = 'harness'\n",
        )
        .expect("ctx.toml");

        let choice_for = |role: &str| {
            let args = ExecArgs {
                runtime: super::super::runtime::CONFIGURED.to_string(),
                role: role.to_string(),
                ..Default::default()
            };
            resolved_runtime(&args, repo.path(), &|_| None).expect("resolve")
        };

        let reviewer = choice_for("reviewer");
        assert_eq!(reviewer.kind, super::super::runtime::RuntimeKind::Native);
        assert_eq!(
            reviewer.source,
            super::super::runtime::RuntimeSource::RoleTable
        );
        assert_eq!(
            choice_for("worker").kind,
            super::super::runtime::RuntimeKind::Harness
        );
    }

    // -- issue #478: the native runtime through the shipped CLI path -------

    /// Issue #478 item 7: the deterministic fixtures must be reachable from
    /// the shipped command, not only from `#[cfg(test)]`. This drives a whole
    /// native session -- request, tool call, continuation, structured final
    /// status -- through `exec::run_with` with NO provider configured, no
    /// credential, and no coding harness installed.
    #[test]
    fn native_runtime_runs_a_whole_fixture_session_through_exec() {
        let state = tempfile::tempdir().expect("tempdir");
        let repo = tempfile::tempdir().expect("repo");
        let env: HashMap<String, String> = [(
            super::super::state::STATE_ENV.to_string(),
            state.path().display().to_string(),
        )]
        .into();
        let lookup = |key: &str| env.get(key).cloned();

        let fixtures = super::super::runtime::fixture::fixture_root();
        let args = ExecArgs {
            runtime: "native".to_string(),
            prompt: Some("fix the failing test".to_string()),
            provider: Some(format!(
                "fixture:{}",
                fixtures
                    .join("anthropic-investigate-edit-test.json")
                    .display()
            )),
            fixture_tools: Some(fixtures.join("tools-investigate-edit-test.json")),
            ..Default::default()
        };
        let mut out = Vec::new();
        let code = run_with(&args, &mut out, repo.path(), &lookup).expect("native run");
        let text = String::from_utf8(out).expect("utf8");
        let status: serde_json::Value =
            serde_json::from_str(&text).unwrap_or_else(|error| panic!("{error}: {text}"));

        assert_eq!(code, 0, "{text}");
        assert_eq!(status["runtime"], "native");
        assert_eq!(status["status"], "completed");
        assert_eq!(status["requests"], 4);
        assert_eq!(status["tool_calls"], 4);
        assert_eq!(status["served_model"], "fixture-anthropic-model");
    }

    /// The same command, resumed: `--resume` continues the stored session
    /// rather than starting a new one, and says so by reusing its id.
    #[test]
    fn native_runtime_resumes_a_stored_session_through_exec() {
        let state = tempfile::tempdir().expect("tempdir");
        let repo = tempfile::tempdir().expect("repo");
        let env: HashMap<String, String> = [(
            super::super::state::STATE_ENV.to_string(),
            state.path().display().to_string(),
        )]
        .into();
        let lookup = |key: &str| env.get(key).cloned();
        let fixtures = super::super::runtime::fixture::fixture_root();

        let first = ExecArgs {
            runtime: "native".to_string(),
            prompt: Some("start".to_string()),
            provider: Some(format!(
                "fixture:{}",
                fixtures
                    .join("anthropic-investigate-edit-test.json")
                    .display()
            )),
            fixture_tools: Some(fixtures.join("tools-investigate-edit-test.json")),
            ..Default::default()
        };
        let mut out = Vec::new();
        run_with(&first, &mut out, repo.path(), &lookup).expect("first run");
        let started: serde_json::Value =
            serde_json::from_slice(&out).expect("first status is json");
        let session = started["session"].as_str().expect("session id").to_string();

        // A different script for the continuation: a provider never reissues
        // a tool-call id it has already used, and the journal would refuse it
        // if one did.
        let resumed = ExecArgs {
            runtime: "native".to_string(),
            resume: Some(session.clone()),
            provider: Some(format!(
                "fixture:{}",
                fixtures.join("resume-continue.json").display()
            )),
            ..Default::default()
        };
        let mut out = Vec::new();
        // No prompt at all: a resume continues a conversation that already
        // has one.
        run_with(&resumed, &mut out, repo.path(), &lookup)
            .unwrap_or_else(|error| panic!("resumed run: {error}"));
        let status: serde_json::Value =
            serde_json::from_slice(&out).expect("resumed status is json");
        assert_eq!(status["session"], session);
        assert_eq!(status["runtime"], "native");
    }

    #[test]
    fn native_runtime_refuses_an_unsupported_provider_override() {
        let repo = tempfile::tempdir().expect("repo");
        let state = tempfile::tempdir().expect("tempdir");
        let env: HashMap<String, String> = [(
            super::super::state::STATE_ENV.to_string(),
            state.path().display().to_string(),
        )]
        .into();
        let lookup = |key: &str| env.get(key).cloned();
        let args = ExecArgs {
            runtime: "native".to_string(),
            prompt: Some("go".to_string()),
            provider: Some("https://example.invalid".to_string()),
            ..Default::default()
        };
        let error = run_with(&args, &mut Vec::new(), repo.path(), &lookup).expect_err("refused");
        assert!(error.to_string().contains("fixture:"), "{error}");
    }

    #[test]
    fn fixture_tools_without_a_fixture_provider_is_refused() {
        let repo = tempfile::tempdir().expect("repo");
        let args = ExecArgs {
            runtime: "native".to_string(),
            prompt: Some("go".to_string()),
            fixture_tools: Some(PathBuf::from("tools.json")),
            ..Default::default()
        };
        let error = run_with(&args, &mut Vec::new(), repo.path(), &|_| None).expect_err("refused");
        assert!(error.to_string().contains("--fixture-tools"), "{error}");
    }

    #[test]
    fn an_empty_command_is_rejected() {
        let tmp = crate::commands::ctx::testenv::repo();
        let env = base_env(&tmp.path().join("state"));
        let args = ExecArgs {
            agent: None,
            session_id: None,
            transcript: None,
            prompt: None,
            max_restarts: None,
            budget_tokens: None,
            max_tool_calls: None,
            objective: None,
            timeout_secs: None,
            simple: false,
            reservation_id: None,
            command: Vec::new(),
            ..Default::default()
        };
        let mut out = Vec::new();
        let err = run_with(&args, &mut out, tmp.path(), &|k| env.get(k).cloned())
            .expect_err("nothing to supervise");
        assert!(err.to_string().contains("command"), "got {err}");
    }

    /// Finding #1: `exec` launches/supervises a harness, so a syntax error in
    /// the operator's own HOME `ctx.toml` must refuse outright rather than
    /// silently falling back to permissive pacing/policy/sandbox defaults --
    /// unlike a repo-layer parse failure (still skipped, see `CtxConfig::
    /// load_for_launch`'s own doc comment) and unlike `status`, which stays
    /// on plain `load` and keeps reporting instead of refusing.
    #[test]
    fn a_home_layer_syntax_error_refuses_to_launch_naming_the_file() {
        let tmp = crate::commands::ctx::testenv::repo();
        let home = tmp.path().join("home");
        std::fs::create_dir_all(home.join(".zirv")).expect("mkdir home");
        std::fs::write(home.join(".zirv/ctx.toml"), "[score\n").expect("write broken home layer");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);

        let env = base_env(&tmp.path().join("state"));
        let args = ExecArgs {
            agent: None,
            session_id: None,
            transcript: None,
            prompt: None,
            max_restarts: None,
            budget_tokens: None,
            max_tool_calls: None,
            objective: None,
            timeout_secs: None,
            simple: false,
            reservation_id: None,
            command: vec!["true".to_string()],
            ..Default::default()
        };
        let mut out = Vec::new();
        let err = run_with(&args, &mut out, tmp.path(), &|k| env.get(k).cloned())
            .expect_err("a broken home layer must refuse to launch");
        let msg = err.to_string();
        assert!(
            msg.contains(&home.join(".zirv").join("ctx.toml").display().to_string()),
            "names the broken file: {msg}"
        );
    }

    /// C2 (issue #155 review finding): the codex adapter's own
    /// `parse_events` never emits `NormalizedEvent::ToolCall` (see its own
    /// doc comment), so `--max-tool-calls` would otherwise be accepted for
    /// a codex worker and then silently never fire. Refused up front,
    /// before anything is spawned -- no fake agent, no transcript, nothing
    /// to poll -- the same shape
    /// `the_delegation_verb_refuses_an_agent_the_settings_file_disabled`
    /// uses in `agent.rs` for a different early refusal.
    #[test]
    fn max_tool_calls_is_refused_up_front_for_the_codex_adapter() {
        let tmp = crate::commands::ctx::testenv::repo();
        let home = tmp.path().join("home");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);
        let env = base_env(&tmp.path().join("state"));
        let args = ExecArgs {
            agent: Some("codex".to_string()),
            session_id: Some("20202020-2222-4333-8444-555555555555".to_string()),
            transcript: None,
            prompt: Some("do the work".to_string()),
            max_restarts: Some(0),
            budget_tokens: None,
            max_tool_calls: Some(5),
            objective: None,
            timeout_secs: Some(30),
            simple: false,
            reservation_id: None,
            command: vec!["codex".to_string(), "exec".to_string()],
            ..Default::default()
        };
        let mut out = Vec::new();
        let err = run_with(&args, &mut out, tmp.path(), &|k| env.get(k).cloned())
            .expect_err("codex cannot count tool calls");
        let msg = err.to_string();
        assert!(msg.contains("--max-tool-calls"), "got {msg}");
        assert!(msg.contains("codex"), "got {msg}");
    }

    /// The other half of C2: an adapter that CAN count tool calls (claude,
    /// the default) must not be caught by the same refusal just because a
    /// budget was configured at all.
    #[test]
    fn max_tool_calls_is_accepted_for_the_claude_adapter() {
        let tmp = crate::commands::ctx::testenv::repo();
        let home = tmp.path().join("home");
        let session = "21212121-2222-4333-8444-555555555555";
        let env = base_env(&tmp.path().join("state"));

        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);
        unsafe {
            std::env::set_var("FAKE_AGENT_MODE", "healthy");
        }
        let args = ExecArgs {
            agent: Some("claude".to_string()),
            session_id: Some(session.to_string()),
            transcript: Some(transcript_for(&home, tmp.path(), session)),
            prompt: Some("do the work".to_string()),
            max_restarts: Some(0),
            budget_tokens: None,
            // Comfortably above what `healthy` mode's fixed 12-turn
            // transcript could ever produce (it has no tool calls to count
            // either, but the point here is that the flag is accepted at
            // all, not rejected before the child ever runs).
            max_tool_calls: Some(1_000),
            objective: None,
            timeout_secs: Some(30),
            simple: false,
            reservation_id: None,
            command: fake_agent_command(session),
            ..Default::default()
        };
        let mut out = Vec::new();
        let code = run_with(&args, &mut out, tmp.path(), &|k| env.get(k).cloned());
        unsafe {
            std::env::remove_var("FAKE_AGENT_MODE");
        }

        assert_eq!(code.expect("runs"), 0);
    }

    /// Issue #690 (remaining scope), the reported defect itself. On a machine
    /// with no harness installed, `zirv ctx agent claude "say hi"` -- which
    /// delegates straight to this entry point (see `agent.rs`'s module doc) --
    /// warned about macOS Keychain access for a harness the operator does not
    /// have, sat out the blind-mode safety delay because that harness has no
    /// usage source, and only then said `claude` is not a program. The
    /// pre-flight has to arrive first, and none of that machinery may run.
    ///
    /// Three observations, because the three channels differ:
    ///
    /// - `pacing degraded` is written to this call's own writer, so its
    ///   absence is asserted directly.
    /// - The 60-second wait is observed through the injected `sleep_fn`
    ///   rather than really slept. The delay is set to a nonzero value on
    ///   purpose (`base_env` zeroes it for every other test here), so there
    ///   genuinely is a wait for the pre-flight to be skipping.
    /// - The Keychain advisory goes to the announcer's own stderr channel,
    ///   which no writer here can capture. It is reachable only from `pace::
    ///   wait_for_window`'s usage refresh, and that call is exactly what logs
    ///   `pacing-blind` -- so an empty decision log is the structural proof
    ///   that the advisory could not have been emitted, asserted alongside
    ///   the literal string.
    ///
    /// Deliberately not `base_env`: that helper sets `ZIRV_CTX_AGENT_BIN`,
    /// which the pre-flight declines to probe at all. The machine is stated
    /// (`only_installed(&[])`), never inherited from this developer's `PATH`.
    #[test]
    fn an_absent_harness_fails_before_pacing_and_before_the_keychain_advisory() {
        let tmp = crate::commands::ctx::testenv::repo();
        let home = tmp.path().join("home");
        let state = tmp.path().join("state");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);
        let env: HashMap<String, String> = [
            (
                crate::commands::ctx::state::STATE_ENV.to_string(),
                state.display().to_string(),
            ),
            (
                "ZIRV_CTX_PACE_BLIND_DELAY_SECS".to_string(),
                "60".to_string(),
            ),
        ]
        .into();

        let args = ExecArgs {
            agent: Some("claude".to_string()),
            prompt: Some("say hi".to_string()),
            ..Default::default()
        };
        let mut out = Vec::new();
        let slept: std::cell::RefCell<Vec<u64>> = std::cell::RefCell::new(Vec::new());
        let err = run_with_clock_and_presence(
            &args,
            &mut out,
            tmp.path(),
            &|k| env.get(k).cloned(),
            &crate::commands::ctx::state::now_secs,
            &|d: Duration| slept.borrow_mut().push(d.as_secs()),
            &adapters::only_installed(&[]),
        )
        .expect_err("an absent harness must not launch");

        assert_eq!(
            err.to_string(),
            "adapter 'claude': program 'claude' not found. Install it so its program is on \
             PATH, or point `agent_bin` at it in ~/.zirv/ctx.toml, or name an installed one \
             with --agent.",
        );
        let printed = String::from_utf8_lossy(&out).to_string();
        assert!(
            !printed.contains("pacing degraded"),
            "a harness that cannot be launched must not be paced for: {printed}"
        );
        assert!(
            !printed.contains("Keychain"),
            "no usage token is worth reading for a harness that is not installed: {printed}"
        );
        assert!(
            slept.borrow().is_empty(),
            "the operator must not wait out a safety delay for a missing binary, slept {:?}",
            slept.borrow()
        );
        let decisions =
            std::fs::read_to_string(state.join("logs/decisions.jsonl")).unwrap_or_default();
        assert!(
            !decisions.contains("pacing-blind"),
            "reaching `pace::wait_for_window` at all is what could emit the Keychain \
             advisory: {decisions}"
        );
    }

    /// Issue #690 (remaining scope), rule 2 at the real entry point: an
    /// explicit `--agent` naming a harness this machine does not have fails
    /// under *that* harness's own name, and the installed one is never put in
    /// its place. The stated machine has claude and not codex, so a
    /// pre-flight that silently fell back would be plainly visible here.
    #[test]
    fn an_explicit_agent_that_is_absent_fails_under_its_own_name() {
        let tmp = crate::commands::ctx::testenv::repo();
        let home = tmp.path().join("home");
        let state = tmp.path().join("state");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);
        let env: HashMap<String, String> = [(
            crate::commands::ctx::state::STATE_ENV.to_string(),
            state.display().to_string(),
        )]
        .into();

        let args = ExecArgs {
            agent: Some("codex".to_string()),
            prompt: Some("say hi".to_string()),
            ..Default::default()
        };
        let mut out = Vec::new();
        let err = run_with_clock_and_presence(
            &args,
            &mut out,
            tmp.path(),
            &|k| env.get(k).cloned(),
            &crate::commands::ctx::state::now_secs,
            &|_d: Duration| panic!("an absent harness must never reach a pacing wait"),
            &adapters::only_installed(&["claude"]),
        )
        .expect_err("codex is not installed on this stated machine");

        let message = err.to_string();
        assert!(
            message.contains("adapter 'codex'"),
            "the harness the operator named is the one that must be reported: {message}"
        );
        assert!(
            !message.contains("claude"),
            "the installed harness must never be substituted: {message}"
        );
    }

    /// Issue #690 (remaining scope), the case selection and the pre-flight
    /// used to disagree about: `zirv ctx exec -- --model x` with no
    /// `--agent`. That argv names no program -- it is flags `exec` appends
    /// to `adapter.program()` -- so this run IS zirv choosing a harness to
    /// launch (`adapter_builds_launch`), and on a machine with codex and no
    /// claude it must choose codex. Until `exec` stated that for itself,
    /// `adapters::select` derived the answer from a non-empty `command`
    /// alone, kept claude, and let the pre-flight refuse a harness the
    /// operator never asked for while an installed one sat there.
    ///
    /// Observed through `--max-tool-calls`, which is the first check after
    /// the pre-flight that names the resolved adapter and the last one
    /// before this run would start pacing and spawning: codex has no
    /// verified way to count tool calls, so its refusal is reachable with
    /// nothing launched and nothing slept. The precondition that makes that
    /// observation honest is asserted below rather than assumed. The
    /// machine is stated (`only_installed(&["codex"])`), never this
    /// developer's own `PATH`, and no `ZIRV_CTX_AGENT_BIN` is set -- an
    /// `agent_bin` override switches presence off entirely
    /// (`resolve_default_with_presence`'s `consult_presence`), which would
    /// make the whole test vacuous.
    #[test]
    fn a_flags_only_command_launches_the_harness_this_machine_actually_has() {
        assert!(
            !adapters::select(Some("codex"), &[], &CtxConfig::default())
                .expect("codex adapter")
                .counts_tool_calls(),
            "this test reads the --max-tool-calls refusal as proof codex was selected; a codex \
             that can count tool calls would sail past it into a real spawn"
        );

        let tmp = crate::commands::ctx::testenv::repo();
        let home = tmp.path().join("home");
        let state = tmp.path().join("state");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);
        let env: HashMap<String, String> = [(
            crate::commands::ctx::state::STATE_ENV.to_string(),
            state.display().to_string(),
        )]
        .into();

        let args = ExecArgs {
            command: vec!["--model".to_string(), "x".to_string()],
            prompt: Some("say hi".to_string()),
            max_tool_calls: Some(1),
            ..Default::default()
        };
        let mut out = Vec::new();
        let err = run_with_clock_and_presence(
            &args,
            &mut out,
            tmp.path(),
            &|k| env.get(k).cloned(),
            &crate::commands::ctx::state::now_secs,
            &|_d: Duration| panic!("this run must stop before anything is paced for"),
            &adapters::only_installed(&["codex"]),
        )
        .expect_err("--max-tool-calls is the stop this observation uses");

        let message = err.to_string();
        assert!(
            message.contains("'codex' adapter"),
            "a flags-only argv is zirv's own launch, so the installed harness must be the one \
             selected: {message}"
        );
        assert!(
            !message.contains("not found"),
            "the harness this machine does not have must never have been selected: {message}"
        );
    }

    /// Fix 2 (issue #249/#250 review): a direct `zirv ctx exec` launch (the
    /// bare `run` entry, which reads `env_from_process()`) must not trust an
    /// inherited `PARENT_SESSION_ENV` off its own ambient process env -- only
    /// a supervisor spawn seam (`agent::run_with`'s fold, or dash's
    /// `verified_parent`) may establish parent lineage. Proves both halves
    /// end to end: the mail from the ambient "parent" renders as ordinary
    /// peer mail in this launch's own composed prompt (not steering), and
    /// the launched child's own real environment carries no `PARENT_SESSION_
    /// ENV` at all -- so it is not exported onward either.
    #[test]
    fn direct_exec_entry_ignores_an_inherited_parent_session_env() {
        let tmp = crate::commands::ctx::testenv::repo();
        let home = tmp.path().join("home");
        let state_dir = tmp.path().join("state");
        let argv_log = tmp.path().join("argv.log");
        let parent_env_log = tmp.path().join("parent-env.log");
        let session = "cececece-2222-4333-8444-555555555555";

        let state = crate::commands::ctx::state::StateDir::from_root(state_dir.clone());
        let slug = crate::commands::ctx::state::repo_slug(tmp.path());
        crate::commands::ctx::mail::store(
            &state,
            &slug,
            &crate::commands::ctx::mail::Message {
                from_session: "grandpar".to_string(),
                from_agent: "claude".to_string(),
                to: "any".to_string(),
                to_session: None,
                sent: 1,
                body: "scope now includes billing".to_string(),
            },
            &CtxConfig::default(),
        )
        .expect("store mail");

        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);
        let _cwd = crate::commands::ctx::testenv::CwdGuard::enter(tmp.path()).expect("enter repo");
        // `run` (unlike `run_with`) reads every one of these off the REAL
        // process environment, `PARENT_SESSION_ENV` included -- the whole
        // seam under test.
        let _vars = crate::commands::ctx::testenv::VarGuard::set(&[
            (crate::commands::ctx::state::STATE_ENV, state_dir.to_str()),
            ("ZIRV_CTX_PACE", Some("false")),
            (
                crate::commands::ctx::agent::PARENT_SESSION_ENV,
                Some("grandpar"),
            ),
            ("FAKE_AGENT_MODE", Some("healthy")),
            ("FAKE_AGENT_ARGV_LOG", argv_log.to_str()),
            ("FAKE_AGENT_PARENT_ENV_LOG", parent_env_log.to_str()),
            // Fix round (inline-argv budget regression): see `base_env`'s own
            // comment -- this test calls `run`, not `run_with`, so it is not
            // covered by that helper and needs the same override directly.
            ("ZIRV_CTX_PROMPT_SKILL_INDEX", Some("false")),
        ]);

        let args = ExecArgs {
            agent: Some("claude".to_string()),
            session_id: Some(session.to_string()),
            transcript: Some(transcript_for(&home, tmp.path(), session)),
            prompt: Some("do the work".to_string()),
            max_restarts: Some(0),
            budget_tokens: None,
            max_tool_calls: None,
            objective: None,
            timeout_secs: Some(60),
            simple: false,
            reservation_id: None,
            command: fake_agent_command(session),
            ..Default::default()
        };
        let mut out = Vec::new();
        let code = run(&args, &mut out);
        assert_eq!(code.expect("runs"), 0);

        let argv = std::fs::read_to_string(&argv_log).expect("argv recorded");
        assert!(
            argv.contains("scope now includes billing"),
            "the mail must still be delivered: {argv}"
        );
        assert!(
            argv.contains("another agent session"),
            "an inherited PARENT_SESSION_ENV from this process's own ambient env must render as \
             ordinary peer mail, never steering: {argv}"
        );
        assert!(
            !argv.contains("the session that spawned this one"),
            "must not be marked as steering: {argv}"
        );

        let logged_parent = std::fs::read_to_string(&parent_env_log).unwrap_or_default();
        assert_eq!(
            logged_parent.trim(),
            "",
            "must not export the inherited parent onward to the launched child: \
             {logged_parent:?}"
        );
    }

    /// Round 4B (stdout/stderr separation): a real harness launch's child
    /// forwards its OWN stdout independently, line by line, straight to this
    /// process's real `std::io::stdout()` (`supervise::forward`) -- entirely
    /// apart from whatever `w` this function was handed. `run()`'s one
    /// production caller (`mod.rs`'s `CtxVerb::Exec` dispatch) hands it that
    /// SAME real stdout, so any supervisor notice ("zirv ctx exec: ...")
    /// still written to `w` used to race the forwarding thread on the
    /// identical stream -- landing in front of, or inside, a child's own
    /// `--output-format json` output, which a downstream consumer piping
    /// this process's stdout could never recover from. Triggers the
    /// cheapest deterministic notice: `command` carries no `-p`/`--print`/
    /// `exec` token and `prompt` is unset, so `extract_prompt` finds nothing
    /// and the "no prompt could be found" notice fires unconditionally, with
    /// no pacing, timing or usage history involved.
    #[test]
    fn direct_exec_entry_never_leaks_a_harness_notice_into_its_own_writer() {
        let tmp = crate::commands::ctx::testenv::repo();
        let home = tmp.path().join("home");
        let state_dir = tmp.path().join("state");
        let session = "dedededd-2222-4333-8444-555555555555";

        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);
        let _cwd = crate::commands::ctx::testenv::CwdGuard::enter(tmp.path()).expect("enter repo");
        let _vars = crate::commands::ctx::testenv::VarGuard::set(&[
            (crate::commands::ctx::state::STATE_ENV, state_dir.to_str()),
            ("ZIRV_CTX_PACE", Some("false")),
            ("FAKE_AGENT_MODE", Some("healthy")),
            ("ZIRV_CTX_PROMPT_SKILL_INDEX", Some("false")),
        ]);

        let args = ExecArgs {
            agent: Some("claude".to_string()),
            session_id: Some(session.to_string()),
            transcript: Some(transcript_for(&home, tmp.path(), session)),
            max_restarts: Some(0),
            timeout_secs: Some(60),
            simple: false,
            // Deliberately no `--prompt`, and `command` carries no `-p`/
            // `--print`/`exec` token either -- fake-agent.sh's own argv
            // parser (a `case` loop with `*) shift ;;`) tolerates the
            // missing flag fine, but `extract_prompt` has nothing to find.
            command: vec![
                "sh".to_string(),
                fixture("fake-agent.sh").display().to_string(),
                "--session-id".to_string(),
                session.to_string(),
            ],
            ..Default::default()
        };
        let mut out = Vec::new();
        let code = run(&args, &mut out);
        assert_eq!(code.expect("runs"), 0);

        let rendered = String::from_utf8_lossy(&out);
        assert!(
            !rendered.contains("zirv ctx exec:"),
            "a supervisor notice reached the caller's own writer instead of stderr: {rendered}"
        );
    }
}
