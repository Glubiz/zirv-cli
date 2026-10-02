//! `zirv ctx chat`: an interactive session launched through the same `wrap`
//! supervision, with argv built by the resolved adapter. The human-facing
//! orchestrator role receives cross-harness delegation guidance.

use std::io::{self, IsTerminal, Write};
use std::path::Path;

use super::adapters::{self, AgentAdapter, DefaultOrigin};
use super::chrome::{self, BannerFacts, ChromeCaps, HarnessRule};
use super::config::{CtxConfig, EnvLookup, env_from_process};
use super::dash;
use super::dash::pane::PaneSpec;
use super::event::SessionId;
use super::prompt::PromptRole;
use super::proxy::{
    self,
    decision::{ProxyDecision, SeatRole},
};
use super::runtime::{self as runtime_kind, RuntimeKind};
use super::state::StateDir;
use super::term;
use super::wrap::{self, WrapArgs};
use super::{CtxResult, handoff, resume};

mod intake;

#[derive(Debug, clap::Args)]
pub struct ChatArgs {
    /// Adapter name: claude or codex. Falls back to the configured default,
    /// then to the registry's own fallback rule.
    #[arg(long)]
    pub agent: Option<String>,
    /// Fold the latest stored handoff into the first prompt.
    #[arg(long, default_value_t = false)]
    pub resume: bool,
    /// Simple run: skip every zirv-injected instruction, including the shipped
    /// default. Supervision, pacing and hooks still apply.
    #[arg(long, default_value_t = false)]
    pub simple: bool,
    /// Suppress the `zirv ▸` announcement channel. Errors and warnings are
    /// never suppressed; the launch banner and status bar have their own
    /// `[chrome]` toggles.
    #[arg(long, default_value_t = false)]
    pub quiet: bool,
    /// Start even though this process looks like it is already inside an
    /// agent session. Off by default: a nested interactive supervisor can
    /// take the outer session down.
    #[arg(long, default_value_t = false)]
    pub allow_nested: bool,
    /// T10: see `WrapArgs::force_pace` -- threaded straight through, since a
    /// `chat` launch becomes a `wrap` launch (`wrap_args_for`).
    #[arg(long, default_value_t = false)]
    pub force_pace: bool,
    /// Issue #358 (task 4): keep this orchestrator seat off the automatic
    /// cross-harness rollover path entirely (`fallback.auto_orchestrator_
    /// rollover`), regardless of headroom. Folded into the same `EnvLookup`
    /// `--quiet` already rides (`pin_env`, mirroring `quiet_env`) as
    /// `ZIRV_CTX_SEAT_PIN=true`, so `seat::pin_from_env` reads it correctly
    /// wherever a seat is registered downstream, without this flag having to
    /// be threaded through `WrapArgs`/`PaneSpec` by hand.
    #[arg(long, default_value_t = false)]
    pub pin_harness: bool,
    /// Issue #352: never use the persistent runtime, even when the operator
    /// has turned it on. The compatibility and debugging escape hatch -- this
    /// process owns the pty, and the session ends when it does, exactly as
    /// every `zirv chat` did before the runtime existed.
    #[arg(long, default_value_t = false)]
    pub no_session: bool,
    /// Issue #480 (roadmap N11): `native` opens a structured native
    /// conversation pane (N09's in-process agent loop, no coding harness
    /// installed, no PTY) instead of a wrapped-harness session. Every other
    /// value, and the default (unset), is today's wrapped-harness dashboard.
    /// A native pane never accepts `--agent`, `--simple`, `--resume` or
    /// `extra` -- see `run_with`'s own refusal for a value combined with any
    /// of those.
    #[arg(long)]
    pub runtime: Option<String>,
    /// Issue #537: force this launch through the harness proxy's intake
    /// view, overriding `[proxy] enabled` for this one launch. Mutually
    /// exclusive with `--no-proxy`; still skipped outright under `--simple`
    /// or `--resume` (see `run_with`'s own intake step).
    #[arg(long, conflicts_with = "no_proxy")]
    pub proxy: bool,
    /// The inverse of `--proxy`: never take over this launch through the
    /// intake view, even when `[proxy] enabled = true`.
    #[arg(long, default_value_t = false)]
    pub no_proxy: bool,
    /// Extra arguments passed through to the agent, after `--`.
    #[arg(allow_hyphen_values = true, last = true)]
    pub extra: Vec<String>,
}

/// Resolved adapter, argv and role let wrap launch without guessing a command.
#[derive(Debug, Clone, PartialEq)]
pub struct ChatLaunch {
    pub agent_name: String,
    pub argv: Vec<String>,
    pub role: PromptRole,
    /// Always register chat as Verb::Chat, even when resumed or passed extra flags.
    pub verb: super::sessions::Verb,
}

/// Pure adapter-built interactive launch defaults to Orchestrator so the human-facing seat receives delegation guidance.
pub fn build_launch(
    adapter: &dyn AgentAdapter,
    initial_prompt: Option<&str>,
    extra: &[String],
) -> ChatLaunch {
    let command = adapter.interactive_cmd(initial_prompt, extra);
    let mut argv = vec![command.get_program().to_string_lossy().to_string()];
    argv.extend(command.get_args().map(|a| a.to_string_lossy().to_string()));
    ChatLaunch {
        agent_name: adapter.name().to_string(),
        argv,
        role: PromptRole::Orchestrator,
        verb: super::sessions::Verb::Chat,
    }
}

/// Use positional prompt fallback when injection is unsupported; probe the adapter with empty argv before launch exists.
/// Mask prompts on both paths; compile handles simple/disabled injection, and the resolved role must match env/hooks (#537).
#[allow(clippy::too_many_arguments)]
fn orchestrator_initial_prompt(
    adapter: &dyn AgentAdapter,
    initial_prompt: Option<String>,
    cfg: &CtxConfig,
    home: Option<&Path>,
    repo: &Path,
    simple: bool,
    state: &StateDir,
    proxy_layer: Option<&str>,
    role: PromptRole,
) -> Option<String> {
    let text = if adapter.system_prompt_supported(&[]) {
        initial_prompt.unwrap_or_default()
    } else {
        let mut compiled = super::compile::compile(
            home,
            repo,
            simple,
            cfg,
            adapter,
            role,
            state,
            super::state::now_secs(),
            super::adapters::LaunchMode::Interactive,
            false,
        );
        if let Some(task) = initial_prompt
            .as_deref()
            .filter(|task| !task.trim().is_empty())
        {
            super::compile::select_skill_descriptions_for_task(
                &mut compiled,
                cfg,
                state,
                repo,
                home,
                task,
            );
        }
        // Unsupported injection needs the same proxy layer that verified launch paths receive (#537).
        let compiled = super::compile::with_proxy_layer(compiled, proxy_layer);
        let base = initial_prompt.unwrap_or_default();
        super::prompt::task_prompt_with_composed_fallback(&base, false, compiled.composed.as_ref())
    };
    if text.is_empty() {
        None
    } else {
        match super::obfuscate_store::protect_text(state, repo, cfg, &text, "chat_initial_prompt") {
            Ok(protected) => Some(protected.0),
            Err(error) => {
                // Fail closed: never send unprotected text; warn when masking drops the initial task.
                crate::output::warn(format!(
                    "sensitive-data masking failed ({error}); starting without the initial task prompt"
                ));
                None
            }
        }
    }
}

/// Resolve and refuse disabled/unready adapters before touching the terminal; wrap repeats the guard independently.
pub(crate) fn resolve_adapter(
    cfg: &CtxConfig,
    requested: Option<&str>,
) -> CtxResult<(Box<dyn AgentAdapter>, HarnessRule)> {
    resolve_adapter_with_presence(cfg, requested, &adapters::liveness_probe)
}

/// Inject presence for every resolution arm so tests never depend on the developer's installed PATH tools (#690).
pub(crate) fn resolve_adapter_with_presence(
    cfg: &CtxConfig,
    requested: Option<&str>,
    present: &dyn Fn(&str, &str) -> adapters::Liveness,
) -> CtxResult<(Box<dyn AgentAdapter>, HarnessRule)> {
    // Empty chat argv means the adapter builds its own launch.
    if requested.is_some() {
        let adapter = adapters::select_with_presence(requested, &[], cfg, true, present)?;
        return Ok((adapter, HarnessRule::Explicit));
    }
    match cfg.agent.as_deref() {
        Some(name) => Ok((
            adapters::select_with_presence(Some(name), &[], cfg, true, present)?,
            HarnessRule::Configured,
        )),
        None => adapters::resolve_default_with_presence(cfg, present).map(|(adapter, origin)| {
            let rule = match origin {
                DefaultOrigin::Configured => HarnessRule::Configured,
                DefaultOrigin::FirstEnabledReady => HarnessRule::FirstEnabledReady,
                DefaultOrigin::FirstInstalledReady { not_found } => {
                    HarnessRule::FirstInstalledReady { not_found }
                }
            };
            (adapter, rule)
        }),
    }
}

/// Show disabled harnesses, omit only confirmed-absent enabled binaries, and retain uncertain/readiness-failed entries (#298).
fn harness_list(cfg: &CtxConfig) -> Vec<(String, bool)> {
    adapters::ADAPTERS
        .iter()
        .filter_map(|(name, _)| {
            let enabled = cfg.agents.is_enabled(name);
            if enabled {
                let present = match adapters::adapter_liveness(cfg, name, None) {
                    Ok((_, verdict)) => verdict.emits_line(),
                    Err(_) => true,
                };
                if !present {
                    return None;
                }
            }
            Some(((*name).to_string(), enabled))
        })
        .collect()
}

/// Resume opportunistically: missing stored handoff announces a fresh start instead of refusing the session.
pub fn resolve_initial_prompt<W: Write>(
    resume_requested: bool,
    state: &StateDir,
    repo: &Path,
    w: &mut W,
    screen_thresholds: &super::screen::Thresholds,
) -> CtxResult<Option<String>> {
    if !resume_requested {
        return Ok(None);
    }
    match handoff::latest_for_repo(state, repo)? {
        // No session id exists yet; working-set resume composition does not consume that argument (#281).
        Some((_path, found)) => Ok(Some(resume::resume_prompt(
            state,
            repo,
            "",
            &found,
            screen_thresholds,
        ))),
        None => {
            writeln!(
                w,
                "zirv ctx chat: --resume requested but no handoff is stored for this repo; \
                 starting a fresh session"
            )?;
            Ok(None)
        }
    }
}

/// Intake outcome resolved once before adapter selection (#537, #799).
#[derive(Debug, PartialEq)]
enum ProxyIntakeOutcome {
    /// Skipped/abandoned intake preserves typed request text; optional advice still honors quiet settings.
    Inactive {
        advisory: Option<String>,
        request: Option<String>,
    },
    /// Enabled intake without usable stdin must refuse rather than silently skip the requested interaction.
    Refuse { message: String },
    /// Confirmed decision with clarified request text; boxing keeps the large variant cheap to pass.
    Decided {
        decision: Box<ProxyDecision>,
        request: String,
    },
}

/// Run intake before adapter resolution or terminal takeover; simple/resume skip it (#537, #799).
/// Only explicit proxy requests announce those skips; active intake requires terminal input and VT-capable stderr.
fn proxy_intake(
    cfg: &CtxConfig,
    state: &StateDir,
    repo: &Path,
    args: &ChatArgs,
    stdin_is_tty: bool,
    vt_ok: bool,
) -> CtxResult<ProxyIntakeOutcome> {
    if args.simple || args.resume {
        let advisory = args.proxy.then(|| {
            let flag = if args.simple { "--simple" } else { "--resume" };
            format!("proxy: skipped ({flag}); starting the orchestrator harness")
        });
        return Ok(ProxyIntakeOutcome::Inactive {
            advisory,
            request: None,
        });
    }
    if let Err(reason) = proxy::activation(cfg) {
        // Disabled proxy stays silent; announce activation failure only when proxy use was requested (#537).
        let advisory = cfg.proxy.enabled.then_some(reason);
        return Ok(ProxyIntakeOutcome::Inactive {
            advisory,
            request: None,
        });
    }
    if !stdin_is_tty {
        return Ok(ProxyIntakeOutcome::Refuse {
            message: "zirv ctx chat: the harness proxy needs an interactive terminal on stdin to \
                      read the task description; pass --no-proxy (or disable [proxy]) to skip it"
                .to_string(),
        });
    }
    if !vt_ok || !io::stderr().is_terminal() {
        return Ok(ProxyIntakeOutcome::Inactive {
            advisory: Some(
                "proxy: this terminal cannot render the intake view; starting the orchestrator \
                 harness"
                    .to_string(),
            ),
            request: None,
        });
    }
    match intake::run(cfg, state, repo)? {
        intake::IntakeOutcome::Decided { decision, request } => {
            Ok(ProxyIntakeOutcome::Decided { decision, request })
        }
        intake::IntakeOutcome::Unplanned { request } => Ok(ProxyIntakeOutcome::Inactive {
            advisory: None,
            request,
        }),
    }
}

/// Count intake rows written under `minted` toward the runtime session the chat attached to, when
/// Jev is active at all (#827).
fn alias_when_jev_active(cfg: &CtxConfig, state: &StateDir, minted: &str, attached: &str) {
    if super::jev::any_gate_enabled(&cfg.jev) && super::jev::credential_present(cfg) {
        super::jev::record_session_alias(state, minted, attached);
    }
}

/// Purely apply the decided harness/model through shared launch fields so argv and disclosures agree.
fn apply_proxy_decision(cfg: &mut CtxConfig, decision: &ProxyDecision) -> String {
    cfg.chat.model = Some(decision.orchestrator.model.clone());
    decision.orchestrator.harness.clone()
}

/// Single decisions must omit orchestrator teaching and write guards; all other outcomes retain Orchestrator (#537).
fn proxy_prompt_role(intake: &ProxyIntakeOutcome) -> PromptRole {
    match intake {
        ProxyIntakeOutcome::Decided { decision, .. } if decision.seat_role == SeatRole::Single => {
            PromptRole::Single
        }
        _ => PromptRole::Orchestrator,
    }
}

/// Native decisions supply route candidates, validated against operator policy at spawn; invalid candidates use role defaults (#702, #703).
fn proxy_decided_model(intake: &ProxyIntakeOutcome) -> Option<String> {
    match intake {
        ProxyIntakeOutcome::Decided { decision, .. } => Some(decision.orchestrator.model.clone()),
        _ => None,
    }
}

/// Build one proxy layer for every launch shape, after workflow startup so it names the actual instance.
fn proxy_layer_text(
    intake: &ProxyIntakeOutcome,
    started_workflow_id: Option<&str>,
) -> Option<String> {
    match intake {
        ProxyIntakeOutcome::Decided { decision, .. } => {
            Some(proxy::prompt_layer(decision, started_workflow_id))
        }
        _ => None,
    }
}

/// Start the workflow once before building prompts; announce skipped/failed starts and never silently discard outcomes (#537).
fn start_proxy_workflow(
    outcome: &ProxyIntakeOutcome,
    state: &StateDir,
    repo: &Path,
    mut announce: impl FnMut(String),
) -> Option<String> {
    let ProxyIntakeOutcome::Decided { decision, request } = outcome else {
        return None;
    };
    match proxy::launch::start_workflow_for(decision, state.root(), repo, request, None) {
        Ok(proxy::launch::WorkflowStart::Started { id }) => Some(id),
        Ok(proxy::launch::WorkflowStart::Skipped { reason }) => {
            announce(format!("proxy: workflow not started; {reason}"));
            None
        }
        Err(err) => {
            announce(format!("proxy: workflow start failed; {err}"));
            None
        }
    }
}

/// Close started workflows only on final launch failure; runtime-attempt failures must leave them live for fallback.
fn close_proxy_workflow_on_failure<T>(
    started_id: Option<&str>,
    state: &StateDir,
    repo: &Path,
    mut announce: impl FnMut(String),
    spawn: impl FnOnce() -> CtxResult<T>,
) -> CtxResult<T> {
    let result = spawn();
    if result.is_err()
        && let Some(id) = started_id
        && let Err(close_error) =
            proxy::launch::close_started(state.root(), repo, id, "proxy launch failed")
    {
        announce(format!(
            "proxy: could not close workflow {id} after the failed launch; {close_error} -- \
             run `zirv workflow close {id}` yourself"
        ));
    }
    result
}

/// Wrap pane construction and dashboard startup in one cleanup boundary so either error closes the started workflow.
/// Explicit inputs make this composition testable without TTY-dependent eligibility.
#[allow(clippy::too_many_arguments)]
fn run_dash_branch(
    adapter: &dyn AgentAdapter,
    launch: ChatLaunch,
    cfg: &CtxConfig,
    state: &StateDir,
    repo: &Path,
    env: EnvLookup<'_>,
    session: &str,
    simple: bool,
    proxy_layer: Option<&str>,
    task: Option<&str>,
    started_workflow_id: Option<&str>,
    force_pace: bool,
    announce: impl FnMut(String),
) -> CtxResult<i32> {
    close_proxy_workflow_on_failure(started_workflow_id, state, repo, announce, || {
        let pane = dash_orchestrator_pane_with_task(
            adapter,
            launch,
            cfg,
            state,
            repo,
            session,
            simple,
            proxy_layer,
            task,
        )?;
        // Mark proxy-decided panes so downstream first-prompt handling sees the decision (#753).
        let proxied = |key: &str| {
            if proxy_layer.is_some() && key == super::adapters::PROXY_DECIDED_ENV {
                Some("1".to_string())
            } else {
                env(key)
            }
        };
        super::models::spawn_refresh_if_due_detached(cfg, state);
        dash::run_dashboard(
            cfg,
            repo,
            &proxied,
            state,
            pane,
            None,
            force_pace,
            started_workflow_id.map(str::to_string),
        )
    })
}

/// Keep the VT guard for the entire session; probe stdin and stdout independently.
/// Unix window-size probes use stdin and cannot prove redirected stdout is a terminal.
fn probe_terminal() -> (bool, bool, bool, (u16, u16), Option<term::VtGuard>) {
    let stdout_is_tty = std::io::stdout().is_terminal();
    let stdin_is_tty = std::io::stdin().is_terminal();
    let size = term::window_size(term::STDIN_FD).unwrap_or((0, 0));
    let vt_guard = term::enable_vt_output().ok();
    let vt_ok = vt_guard.is_some();
    (stdout_is_tty, stdin_is_tty, vt_ok, size, vt_guard)
}

/// One-shot alias marker shares native dispatch without adding a hidden clap surface (#540).
pub const NATIVE_ALIAS_ENV: &str = "ZIRV_CTX_NATIVE_ALIAS";

/// Alias-only stderr notice, emitted once and never injected into model context.
pub const NATIVE_ALIAS_BANNER: &str =
    "zirv native is experimental; `zirv chat` remains the stable harness.";

/// Share refusal text with native help so the documented limitation cannot drift.
pub const NATIVE_WRAPPED_ONLY_FLAGS_REFUSAL: &str = "--runtime native accepts no --agent, --simple, --resume, --pin-harness or trailing \
     arguments -- those are wrapped-harness-only";

/// Share the terminal requirement with native help.
pub const NATIVE_TTY_REFUSAL: &str =
    "zirv chat --runtime native needs an interactive terminal on both stdin and stdout";

/// Native alias help must remain separate from ordinary chat help.
pub fn native_help_text() -> String {
    format!("{}\n", runtime_kind::NATIVE_COMING_SOON)
}

/// Reject unknown runtimes and wrapped-only flags rather than silently accepting ineffective options.
#[allow(clippy::too_many_arguments)]
fn run_native_chat<E: Write>(
    runtime: &str,
    cfg: &CtxConfig,
    repo: &Path,
    env: EnvLookup<'_>,
    stderr: &mut E,
    args: &ChatArgs,
    stdout_is_tty: bool,
    stdin_is_tty: bool,
    vt_ok: bool,
) -> CtxResult<i32> {
    runtime_kind::require_native_available()?;
    // Emit the alias notice before launch validation so refusals still identify the experimental spelling (#540).
    if env(NATIVE_ALIAS_ENV).as_deref() == Some("true") {
        writeln!(stderr, "{NATIVE_ALIAS_BANNER}")?;
    }
    // Clear the one-shot alias marker after reading so children cannot inherit it (#540).
    // SAFETY: no threads have started here, so process-environment access is not concurrent.
    unsafe {
        std::env::remove_var(NATIVE_ALIAS_ENV);
    }
    // Use the shared runtime selector and error text so launch paths cannot drift (#531).
    match runtime_kind::selected(runtime) {
        Ok(RuntimeKind::Native) => {}
        Ok(_) => {
            writeln!(
                stderr,
                "--runtime '{runtime}': expected `native` (omit --runtime for a wrapped harness)"
            )?;
            return Ok(1);
        }
        Err(error) => {
            writeln!(stderr, "{error}")?;
            return Ok(1);
        }
    }
    if args.agent.is_some()
        || args.simple
        || args.resume
        || args.pin_harness
        || !args.extra.is_empty()
    {
        writeln!(stderr, "{NATIVE_WRAPPED_ONLY_FLAGS_REFUSAL}")?;
        return Ok(1);
    }
    if !(stdout_is_tty && stdin_is_tty && vt_ok) {
        writeln!(stderr, "{NATIVE_TTY_REFUSAL}")?;
        return Ok(1);
    }
    // Nesting was refused before config loading; that guard also covers native dispatch.
    let state = StateDir::resolve(env)?;
    // Run native intake after runtime/flag/TTY refusals and before pane creation, sharing the wrapped path's guards (#537).
    // Known before intake so its Jev call is attributed to the session this launch will use.
    let session = uuid::Uuid::new_v4().to_string();
    super::jev::adopt_session_id(&session);
    let intake = proxy_intake(cfg, &state, repo, args, stdin_is_tty, vt_ok)?;
    if let ProxyIntakeOutcome::Refuse { message } = &intake {
        writeln!(stderr, "{message}")?;
        return Ok(1);
    }
    // Share role mapping with wrapped launches so direct decisions stay Single (#537).
    let seat_role = proxy_prompt_role(&intake);
    // Use the ordinary dashboard so native conversations share roster, mail, attention and worker panes (#490).
    // Carry the decided model as a route candidate alongside its role; spawn must still validate it (#703).
    let model = proxy_decided_model(&intake);
    let (pane_spec, native_spec) = native_pane_spec(repo, session, seat_role, model);
    super::models::spawn_refresh_if_due_detached(cfg, &state);
    dash::run_dashboard(
        cfg,
        repo,
        env,
        &state,
        pane_spec,
        Some(native_spec),
        args.force_pace,
        // This branch starts no workflow to bind.
        None,
    )
}

/// Derive both native pane role fields from one resolved seat; never hardcode Orchestrator (#537).
/// The optional decided model is a route candidate; absent keeps the role's default (#703).
fn native_pane_spec(
    repo: &Path,
    session: String,
    seat_role: PromptRole,
    model: Option<String>,
) -> (dash::PaneSpec, dash::native_pane::NativeDashboardSpec) {
    (
        dash::PaneSpec {
            agent_name: super::runtime::RuntimeKind::Native.as_str().to_string(),
            argv: Vec::new(),
            role: seat_role,
            verb: super::sessions::Verb::Chat,
            session_id: session,
            title: "orch".to_string(),
        },
        dash::native_pane::NativeDashboardSpec {
            repo: repo.to_path_buf(),
            role: seat_role.label().to_string(),
            route: model,
            writing: true,
            provider: None,
            seat: None,
            initial_input: None,
        },
    )
}

/// Inject stderr separately so refusal diagnostics can be tested without capturing process streams.
pub fn run_with<W: Write, E: Write>(
    args: &ChatArgs,
    w: &mut W,
    stderr: &mut E,
    repo: &Path,
    env: EnvLookup<'_>,
) -> CtxResult<i32> {
    if args
        .runtime
        .as_deref()
        .is_some_and(|value| value.eq_ignore_ascii_case("native"))
    {
        runtime_kind::require_native_available()?;
    }
    // Refuse nesting before config, terminal probes or VT changes because shared-console changes can kill the outer session.
    // Print once and return exit 1; wrap independently rechecks and must receive the same override.
    if let Some(refusal) = super::sessions::nesting_refusal("chat", env, args.allow_nested) {
        writeln!(stderr, "{refusal}")?;
        return Ok(1);
    }

    let mut cfg = CtxConfig::load_for_launch(repo, env)?;
    // Proxy flags override activation only; they never bypass configured decider readiness checks (#537).
    if args.proxy {
        cfg.proxy.enabled = true;
    } else if args.no_proxy {
        cfg.proxy.enabled = false;
    }
    // Retain the VT guard until the session ends; early Drop would disable VT before raw mode uses it.
    let (stdout_is_tty, stdin_is_tty, vt_ok, size, _vt_guard) = probe_terminal();

    // Resolve runtime before harness-only setup while keeping the VT guard alive across either branch (#480).
    // Absent runtime uses operator role/default settings and falls back to harness; native intake runs its own guards (#491, #537).
    let configured = runtime_kind::resolve(
        args.runtime.as_deref().unwrap_or(runtime_kind::CONFIGURED),
        &cfg.runtime,
        "orchestrator",
    );
    if !runtime_kind::native_available() {
        configured.as_ref().map_err(|error| error.to_string())?;
    }
    if let Ok(choice) = &configured
        && let Some(note) = &choice.note
    {
        writeln!(stderr, "zirv chat: {note}")?;
    }
    // Explicit runtime always wins, including harness over a native default; unknown values use shared validation (#593).
    let native = match args.runtime.as_deref() {
        Some(flag) => !flag.eq_ignore_ascii_case(RuntimeKind::Harness.as_str()),
        None => configured.is_ok_and(|choice| choice.kind == RuntimeKind::Native),
    };
    if native {
        return run_native_chat(
            args.runtime
                .as_deref()
                .unwrap_or(RuntimeKind::Native.as_str()),
            &cfg,
            repo,
            env,
            stderr,
            args,
            stdout_is_tty,
            stdin_is_tty,
            vt_ok,
        );
    }

    let chrome = ChromeCaps::probe(stdout_is_tty, vt_ok, size, &cfg.chrome, args.simple, false);
    let state = StateDir::resolve(env)?;

    // Resolve intake before adapter selection or dashboard/wrap launch (#537).
    // Known before intake so its Jev call is attributed to the session this launch will use.
    let session = SessionId::new_v4();
    super::jev::adopt_session_id(session.as_str());
    let intake = proxy_intake(&cfg, &state, repo, args, stdin_is_tty, vt_ok)?;
    if let ProxyIntakeOutcome::Refuse { message } = &intake {
        writeln!(stderr, "{message}")?;
        return Ok(1);
    }
    let proxy_announcer = super::announce::Announcer::new(
        cfg.chrome.events && !args.quiet,
        console::colors_enabled_stderr(),
    );
    // An accepted proxy decision overrides the requested harness and model; inactive intake retains normal resolution (#537).
    let mut requested_agent = args.agent.clone();
    match &intake {
        ProxyIntakeOutcome::Inactive {
            advisory: Some(reason),
            ..
        } => {
            proxy_announcer.emit_to(
                stderr,
                &super::announce::Event::ProxyAdvisory {
                    text: reason.clone(),
                },
            );
        }
        ProxyIntakeOutcome::Decided { decision, .. } => {
            // The confirmed plan already discloses the decision; its scrollback summary is the receipt (#799).
            requested_agent = Some(apply_proxy_decision(&mut cfg, decision));
        }
        ProxyIntakeOutcome::Inactive { advisory: None, .. } | ProxyIntakeOutcome::Refuse { .. } => {
        }
    }

    let (adapter, rule) = match resolve_adapter(&cfg, requested_agent.as_deref()) {
        Ok(found) => found,
        Err(err) => {
            // Print refusals once on stderr and return exit 1 to avoid top-level duplicate errors.
            // Never gate them on the banner: redirected stdout must still leave a visible refusal.
            writeln!(
                stderr,
                "{}",
                chrome::style_no_adapter_error(&err.to_string(), chrome.colour)
            )?;
            return Ok(1);
        }
    };
    // Proxy request and resume handoff cannot compete for the initial prompt because resume always skips intake (#537).
    let initial_prompt = match &intake {
        ProxyIntakeOutcome::Decided { request, .. } => Some(request.clone()),
        // Abandoning the plan preserves typed task text.
        ProxyIntakeOutcome::Inactive {
            request: Some(text),
            ..
        } => Some(text.clone()),
        _ => resolve_initial_prompt(args.resume, &state, repo, w, &cfg.screen.thresholds())?,
    };
    let resuming = args.resume && initial_prompt.is_some();

    // Start the workflow once before prompt/argv construction so all launch shapes name the same live instance.
    // Compose unsupported-injection fallback before positional argv is fixed; Windows Codex shell shims need this channel.
    let started_workflow_id = start_proxy_workflow(&intake, &state, repo, |text| {
        proxy_announcer.emit_to(stderr, &super::announce::Event::ProxyAdvisory { text });
    });
    // Print after workflow startup so the receipt names its instance; quiet cannot suppress a confirmed-plan receipt (#799).
    if let ProxyIntakeOutcome::Decided { decision, .. } = &intake {
        writeln!(
            stderr,
            "{}",
            intake::summary_line(decision, started_workflow_id.as_deref())
        )?;
    }
    // Share one bounded proxy layer across every launch shape (#537).
    let proxy_layer = proxy_layer_text(&intake, started_workflow_id.as_deref());
    // Resolve the role once so all prompt, env and launch consumers agree (#537).
    let seat_role = proxy_prompt_role(&intake);
    let initial_prompt = orchestrator_initial_prompt(
        adapter.as_ref(),
        initial_prompt,
        &cfg,
        crate::utils::home_dir().ok().as_deref(),
        repo,
        args.simple,
        &state,
        proxy_layer.as_deref(),
        seat_role,
    );

    // Resolve model extras before the launch fork so dashboard and wrap use the same argv.
    let extra = extra_with_model(&cfg, adapter.as_ref(), &args.extra);
    let mut launch = build_launch(adapter.as_ref(), initial_prompt.as_deref(), &extra);
    // Override the default role once so every downstream consumer honors Single decisions (#537).
    launch.role = seat_role;

    if chrome.banner {
        let facts = BannerFacts {
            harness: adapter.name().to_string(),
            rule,
            session: session.as_str().to_string(),
            harnesses: harness_list(&cfg),
            resuming: resuming.then(|| "the last stored handoff for this repo".to_string()),
            model: cfg.chat.model.clone(),
        };
        // Zero width means a failed probe; use the compact banner instead of a zero-width box.
        let banner_cols = (size.0 > 0).then_some(size.0);
        writeln!(
            w,
            "{}",
            chrome::banner(&facts, chrome.colour, vt_ok, banner_cols)
        )?;
    }

    let env = quiet_env(env, args.quiet);
    let env = pin_env(&env, args.pin_harness);

    // Disclose after quiet resolution and before dispatch, independently of the banner.
    announce_model_choice(stderr, &cfg, args.quiet);
    announce_harness_choice(stderr, &cfg, args.quiet, adapter.name(), rule);

    // Try persistence before both local paths; experimental-runtime failures must fall back to a usable session (#352).
    if super::session::chat_route(
        cfg.session.persistent,
        args.no_session,
        stdin_is_tty,
        stdout_is_tty,
    ) == super::session::ChatRoute::Runtime
    {
        // Runtime failure is not final: fallback prompts already name this workflow, so leave it live until final launch failure (#537).
        match super::session::chat_via_runtime(
            &state,
            adapter.name(),
            initial_prompt.as_deref(),
            &extra,
            repo,
            w,
            launch.role,
            |attached| alias_when_jev_active(&cfg, &state, session.as_str(), attached),
        ) {
            Ok(code) => return Ok(code),
            Err(error) => writeln!(
                stderr,
                "zirv chat: the persistent runtime is unavailable ({error}); \
                 starting a session in this process instead"
            )?,
        }
    }

    if chrome::dash_eligible(
        stdout_is_tty,
        stdin_is_tty,
        vt_ok,
        size,
        &cfg.dash,
        args.simple,
    ) {
        return run_dash_branch(
            adapter.as_ref(),
            launch,
            &cfg,
            &state,
            repo,
            &env,
            session.as_str(),
            args.simple,
            proxy_layer.as_deref(),
            initial_prompt.as_deref(),
            started_workflow_id.as_deref(),
            args.force_pace,
            |text| {
                proxy_announcer.emit_to(stderr, &super::announce::Event::ProxyAdvisory { text });
            },
        );
    }

    // Announce only size-based dashboard ineligibility; simple mode never intended to use that layout.
    if cfg.dash.enabled
        && !args.simple
        && stdout_is_tty
        && stdin_is_tty
        && vt_ok
        && (size.0 < chrome::MIN_DASH_COLS || size.1 < chrome::MIN_DASH_ROWS)
    {
        crate::output::error(format!(
            "the terminal is too small for the dashboard (need at least {}x{}, got {}x{}); \
             falling back to a plain session. Pass --simple to silence this.",
            chrome::MIN_DASH_COLS,
            chrome::MIN_DASH_ROWS,
            size.0,
            size.1
        ));
    }

    let wrap_args = wrap_args_for(args, launch.clone(), proxy_layer.clone());
    close_proxy_workflow_on_failure(
        started_workflow_id.as_deref(),
        &state,
        repo,
        |text| {
            proxy_announcer.emit_to(stderr, &super::announce::Event::ProxyAdvisory { text });
        },
        || {
            wrap::run_with(
                &wrap_args,
                repo,
                &env,
                launch.role,
                Some(session),
                launch.verb,
            )
        },
    )
}

/// Repo-settable models require disclosure through repo-unsilenceable events; a hideable banner is insufficient.
/// Honor operator quiet separately because config was loaded before the flag's env fold.
fn announce_model_choice<E: Write>(stderr: &mut E, cfg: &CtxConfig, quiet: bool) {
    let Some(model) = &cfg.chat.model else {
        return;
    };
    super::announce::Announcer::new(
        cfg.chrome.events && !quiet,
        console::colors_enabled_stderr(),
    )
    .emit_to(
        stderr,
        &super::announce::Event::ChatModel {
            model: model.clone(),
        },
    );
}

/// Disclose installation-based provider fallback through repo-unsilenceable events, with operator quiet still honored (#690).
fn announce_harness_choice<E: Write>(
    stderr: &mut E,
    cfg: &CtxConfig,
    quiet: bool,
    chosen: &str,
    rule: HarnessRule,
) {
    let HarnessRule::FirstInstalledReady { not_found } = rule else {
        return;
    };
    super::announce::Announcer::new(
        cfg.chrome.events && !quiet,
        console::colors_enabled_stderr(),
    )
    .emit_to(
        stderr,
        &super::announce::Event::HarnessAutoSelected {
            chosen: chosen.to_string(),
            not_found: not_found.to_string(),
        },
    );
}

/// Match wrap's order: compile, merge explicit prompt, inject, then log (#44).
/// Interactive orchestrators must never receive mail bodies, only unread counts; workers receive bodies.
#[allow(clippy::too_many_arguments)]
pub(crate) fn dash_orchestrator_pane(
    adapter: &dyn AgentAdapter,
    launch: ChatLaunch,
    cfg: &CtxConfig,
    state: &StateDir,
    repo: &Path,
    session: &str,
    simple: bool,
    proxy_layer: Option<&str>,
) -> CtxResult<PaneSpec> {
    dash_orchestrator_pane_with_task(
        adapter,
        launch,
        cfg,
        state,
        repo,
        session,
        simple,
        proxy_layer,
        None,
    )
}

#[allow(clippy::too_many_arguments)]
fn dash_orchestrator_pane_with_task(
    adapter: &dyn AgentAdapter,
    launch: ChatLaunch,
    cfg: &CtxConfig,
    state: &StateDir,
    repo: &Path,
    session: &str,
    simple: bool,
    proxy_layer: Option<&str>,
    task: Option<&str>,
) -> CtxResult<PaneSpec> {
    // Compile context and its policy report together (#44).
    let home = crate::utils::home_dir().ok();
    let mut compiled = super::compile::compile(
        home.as_deref(),
        repo,
        simple,
        cfg,
        adapter,
        launch.role,
        state,
        super::state::now_secs(),
        super::adapters::LaunchMode::Interactive,
        true,
    );
    if let Some(task) = task.filter(|task| !task.trim().is_empty()) {
        super::compile::select_skill_descriptions_for_task(
            &mut compiled,
            cfg,
            state,
            repo,
            home.as_deref(),
            task,
        );
    }
    // Apply the bounded proxy layer consistently with other launch paths (#537).
    let compiled = super::compile::with_proxy_layer(compiled, proxy_layer);
    let (mut argv, mut composed) = super::prompt::merge_command_line_prompt(
        adapter,
        &launch.argv,
        compiled.composed,
        None,
        launch.role,
        &cfg.prompt,
    );
    composed = super::obfuscate_store::protect_composed(
        state,
        repo,
        cfg,
        composed,
        "chat_orchestrator_prompt",
    )?;
    let prompt_args = super::prompt::injection_args_for_session(
        adapter,
        &argv,
        composed.as_ref(),
        state,
        session,
    )?;
    super::prompt::log_injection(
        state,
        "chat",
        session,
        composed.as_ref(),
        adapter.system_prompt_supported(&argv),
    );
    // Apply the shared sandbox/policy posture while honoring explicit operator flags.
    // Use the resolved role so Single seats skip the orchestrator-only skill plugin.
    let sandbox_extra = adapters::with_workload_writable_roots(
        adapters::policy_launch_args(
            cfg,
            adapter,
            &argv,
            adapters::LaunchMode::Interactive,
            launch.role,
        ),
        adapter,
        repo,
        state,
    );
    // Announce policy degradation on the same operator-controlled channel as other launches.
    let announcer =
        super::announce::Announcer::new(cfg.chrome.events, console::colors_enabled_stderr());
    announcer.emit(&super::announce::Event::SandboxPosture {
        detail: if sandbox_extra.is_empty() {
            "not applied (operator flags or [sandbox] enabled = false)".to_string()
        } else {
            super::announce::posture_detail(&sandbox_extra)
        },
    });
    // Best-effort heal outdated hooks, then warn at most daily; missing home must not prevent launch (#420).
    if let Ok(home) = crate::utils::home_dir() {
        let _ = super::hook_integrity::heal_outdated(state, &home);
        if let Some(summary) = super::hook_integrity::drift_warning_if_due(state, &home) {
            announcer.emit(&super::announce::Event::HookIntegrity { summary });
        }
    }
    argv.extend(sandbox_extra);
    argv.extend(prompt_args);
    // Pin dashboard conversations for roster resume, but preserve explicit operator conversation ids; wrap mints fresh conversations.
    // Unpinned orchestrator registry ids may differ safely only because restore excludes that role; workers must always pin.
    if !super::exec::pins_an_existing_conversation(&argv, adapter.name()) {
        argv.extend(adapter.session_pin_args(session));
    }

    Ok(PaneSpec {
        agent_name: launch.agent_name,
        argv,
        role: launch.role,
        verb: launch.verb,
        session_id: session.to_string(),
        title: "orch".to_string(),
    })
}

/// Add model flags as trailing extras so they cannot land inside Windows cmd.exe /c launcher prefixes.
fn extra_with_model(cfg: &CtxConfig, adapter: &dyn AgentAdapter, extra: &[String]) -> Vec<String> {
    let Some(model) = cfg.chat.model.as_deref() else {
        return extra.to_vec();
    };
    let mut out = adapter.model_args(model);
    out.extend_from_slice(extra);
    out
}

/// Pure wrap conversion must preserve allow_nested because wrap independently rechecks the same environment.
pub fn wrap_args_for(args: &ChatArgs, launch: ChatLaunch, proxy_layer: Option<String>) -> WrapArgs {
    WrapArgs {
        agent: Some(launch.agent_name),
        no_supervise: false,
        command: launch.argv,
        simple: args.simple,
        allow_nested: args.allow_nested,
        force_pace: args.force_pace,
        proxy_layer,
    }
}

/// Fold quiet into shared env so downstream config loads retain operator-over-repo announcement control.
pub(crate) fn quiet_env<'a>(
    env: EnvLookup<'a>,
    quiet: bool,
) -> impl Fn(&str) -> Option<String> + 'a {
    move |key: &str| {
        if quiet && key == "ZIRV_CTX_QUIET" {
            Some("true".to_string())
        } else {
            env(key)
        }
    }
}

/// Fold pin-harness into shared env so both wrap and dashboard seat registration honor the override (#358).
pub(crate) fn pin_env<'a>(env: EnvLookup<'a>, pin: bool) -> impl Fn(&str) -> Option<String> + 'a {
    move |key: &str| {
        if pin && key == super::seat::PIN_ENV {
            Some("true".to_string())
        } else {
            env(key)
        }
    }
}

pub fn run<W: Write>(args: &ChatArgs, w: &mut W) -> CtxResult<i32> {
    let repo = std::env::current_dir()?;
    let env = env_from_process();
    run_with(args, w, &mut std::io::stderr(), &repo, &env)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::ctx::adapters::claude::ClaudeAdapter;
    use crate::commands::ctx::adapters::codex::CodexAdapter;
    use crate::commands::ctx::catalogue::Tier;
    use crate::commands::ctx::handoff::Handoff;
    use crate::commands::ctx::proxy::decision::{Decider, Seat, SeatTier};
    use crate::commands::ctx::state::StateDir;
    use crate::commands::workflow::classify::{Complexity, Intent, RiskBand};
    use crate::commands::workflow::profile::{ExecutionMode, ValidationProfile};
    use std::collections::BTreeMap;

    fn handoff() -> Handoff {
        Handoff {
            task: "Wire the payments webhook".to_string(),
            done: vec!["Added the route".to_string()],
            remaining: vec!["Signature verification".to_string()],
            next_step: "Add a failing test for an invalid signature".to_string(),
            files_modified: vec!["src/routes/webhook.rs".to_string()],
            gotchas: vec![],
            ..Handoff::default()
        }
    }

    #[test]
    fn chat_builds_the_launch_from_the_adapter_rather_than_a_user_argv() {
        // Deliberately does not recompute `expected_argv` by calling
        // `adapter.interactive_cmd` a second time and extracting it the same
        // way `build_launch` does: that would just be `build_launch`'s own
        // extraction logic compared against itself, so a bug in it would
        // never show up here. Asserting on fixed, independently-known
        // content (the adapter binary is the given constant; the prompt is
        // the given constant; extra flags land after both) is what actually
        // pins the behavior.
        let adapter = ClaudeAdapter::new(Some("/tmp/fake-claude"));
        let launch = build_launch(
            &adapter,
            Some("hello"),
            &["--model".to_string(), "opus".to_string()],
        );

        assert_eq!(
            launch.argv.first().map(String::as_str),
            Some("/tmp/fake-claude"),
            "the argv's own program is the adapter's own binary: {:?}",
            launch.argv
        );
        assert!(
            launch.argv.contains(&"hello".to_string()),
            "the initial prompt reaches argv: {:?}",
            launch.argv
        );
        assert_eq!(
            &launch.argv[launch.argv.len() - 2..],
            &["--model".to_string(), "opus".to_string()],
            "extra flags land last: {:?}",
            launch.argv
        );
    }

    #[test]
    fn chat_passes_the_resolved_agent_explicitly_so_wrap_never_has_to_guess() {
        let adapter = ClaudeAdapter::new(None);
        let launch = build_launch(&adapter, None, &[]);
        assert_eq!(launch.agent_name, "claude");
    }

    #[test]
    fn extra_flags_after_the_separator_reach_the_agent_untouched() {
        let adapter = ClaudeAdapter::new(Some("/tmp/fake-claude"));
        let extra = vec!["--model".to_string(), "opus".to_string()];
        let launch = build_launch(&adapter, None, &extra);
        assert_eq!(
            &launch.argv[launch.argv.len() - 2..],
            &extra[..],
            "extra flags must survive to the end of argv untouched: {:?}",
            launch.argv
        );
    }

    /// Bug B: claude always reports `system_prompt_supported`, so the
    /// composed fallback must be a no-op for it. With masking disabled (the
    /// default), the positional prompt stays byte-for-byte unchanged too.
    #[test]
    fn orchestrator_initial_prompt_is_a_no_op_for_a_supported_adapter() {
        let tmp = crate::commands::ctx::testenv::repo();
        let home = tmp.path().join("home");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);
        let state = StateDir::from_root(tmp.path().join("state"));
        let cfg = CtxConfig::default();
        let adapter = ClaudeAdapter::new(Some("/nonexistent/fake-claude"));

        assert_eq!(
            orchestrator_initial_prompt(
                &adapter,
                None,
                &cfg,
                Some(&home),
                tmp.path(),
                false,
                &state,
                None,
                PromptRole::Orchestrator,
            ),
            None
        );
        assert_eq!(
            orchestrator_initial_prompt(
                &adapter,
                Some("resume this".to_string()),
                &cfg,
                Some(&home),
                tmp.path(),
                false,
                &state,
                None,
                PromptRole::Orchestrator,
            ),
            Some("resume this".to_string())
        );
    }

    #[test]
    fn orchestrator_initial_prompt_masks_a_supported_adapters_positional_prompt() {
        let tmp = crate::commands::ctx::testenv::repo();
        let home = tmp.path().join("home");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);
        let state = StateDir::from_root(tmp.path().join("state"));
        let mut cfg = CtxConfig::default();
        cfg.obfuscate.mode = super::super::config::ObfuscateMode::Obfuscate;
        let adapter = ClaudeAdapter::new(Some("/nonexistent/fake-claude"));
        let secret = "ghp_abcdefghijklmnopqrstuvwxyz123456";

        let prompt = orchestrator_initial_prompt(
            &adapter,
            Some(format!("use {secret}")),
            &cfg,
            Some(&home),
            tmp.path(),
            false,
            &state,
            None,
            PromptRole::Orchestrator,
        )
        .expect("the masked prompt is preserved");

        assert!(!prompt.contains(secret), "{prompt}");
        assert!(prompt.contains("ZIRV_SECRET_GITHUB_TOKEN_1"), "{prompt}");
    }

    /// Bug B, the actual fix: on a Windows npm-installed `codex.cmd` shim --
    /// the shape `CodexAdapter::system_prompt_supported` narrows to
    /// unsupported -- the composed session context (the shipped default
    /// layer and, because this is an Orchestrator launch, the harness
    /// meta-teaching layer) must land on the positional initial-prompt slot,
    /// since `injection_args_for_session` never reaches this adapter at all.
    /// Before this fix a codex orchestrator on this launch shape started
    /// with no zirv context whatsoever, while a claude orchestrator always
    /// got one (see the previous test).
    #[cfg(windows)]
    #[test]
    fn orchestrator_initial_prompt_folds_composed_context_for_an_unsupported_codex_shim() {
        let tmp = crate::commands::ctx::testenv::repo();
        let home = tmp.path().join("home");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);
        let state = StateDir::from_root(tmp.path().join("state"));
        let cfg = CtxConfig::default();
        let shim_dir = tempfile::tempdir().expect("tempdir");
        let shim = shim_dir.path().join("codex.cmd");
        std::fs::write(&shim, "@echo off\r\n").expect("write shim");
        let adapter = CodexAdapter::new(Some(&shim.display().to_string()));
        assert!(
            !adapter.system_prompt_supported(&[]),
            "a .cmd shim must be the unsupported shape this test exercises"
        );

        let text = orchestrator_initial_prompt(
            &adapter,
            None,
            &cfg,
            Some(&home),
            tmp.path(),
            false,
            &state,
            None,
            PromptRole::Orchestrator,
        )
        .expect("an unsupported adapter still gets a fallback prompt");
        assert!(
            text.contains("zirv engineering standard"),
            "the shipped default layer must reach the fallback: {text}"
        );
        assert!(
            text.contains("zirv meta-harness"),
            "an Orchestrator launch must still get the harness delegation layer: {text}"
        );

        // A real resume prompt is preserved as the leading text, with the
        // composed context appended after it -- the same order `exec.rs`'s
        // own `task_prompt_with_composed_fallback` call keeps for a headless
        // worker's own task text.
        let with_resume = orchestrator_initial_prompt(
            &adapter,
            Some("continue the payments webhook".to_string()),
            &cfg,
            Some(&home),
            tmp.path(),
            false,
            &state,
            None,
            PromptRole::Orchestrator,
        )
        .expect("still folds a fallback in on top of a real prompt");
        assert!(
            with_resume.starts_with("continue the payments webhook"),
            "the caller's own prompt text must lead: {with_resume}"
        );
        assert!(
            with_resume.contains("zirv engineering standard"),
            "and the composed context must still follow it: {with_resume}"
        );
    }

    /// Issue #537 (T2a): the same unsupported-adapter fallback carries the
    /// harness proxy's own bounded layer when one is given, on top of the
    /// composed context this launch shape already folds in.
    #[cfg(windows)]
    #[test]
    fn orchestrator_initial_prompt_folds_the_proxy_layer_for_an_unsupported_codex_shim() {
        let tmp = crate::commands::ctx::testenv::repo();
        let home = tmp.path().join("home");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);
        let state = StateDir::from_root(tmp.path().join("state"));
        let cfg = CtxConfig::default();
        let shim_dir = tempfile::tempdir().expect("tempdir");
        let shim = shim_dir.path().join("codex.cmd");
        std::fs::write(&shim, "@echo off\r\n").expect("write shim");
        let adapter = CodexAdapter::new(Some(&shim.display().to_string()));

        let text = orchestrator_initial_prompt(
            &adapter,
            None,
            &cfg,
            Some(&home),
            tmp.path(),
            false,
            &state,
            Some("[zirv proxy]\nexecution: bounded"),
            PromptRole::Orchestrator,
        )
        .expect("an unsupported adapter still gets a fallback prompt");
        assert!(
            text.contains("[zirv proxy]"),
            "a given decision must reach the fallback prompt: {text}"
        );
    }

    /// `--simple` disables prompt composition entirely (`compile::compile`
    /// returns `composed: None`, mirroring `prompt::compose`'s own gate), so
    /// even an unsupported adapter must fall back to the caller's own
    /// `initial_prompt` unchanged rather than injecting anything.
    #[cfg(windows)]
    #[test]
    fn orchestrator_initial_prompt_is_a_no_op_under_simple_even_for_an_unsupported_adapter() {
        let tmp = crate::commands::ctx::testenv::repo();
        let home = tmp.path().join("home");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);
        let state = StateDir::from_root(tmp.path().join("state"));
        let cfg = CtxConfig::default();
        let shim_dir = tempfile::tempdir().expect("tempdir");
        let shim = shim_dir.path().join("codex.cmd");
        std::fs::write(&shim, "@echo off\r\n").expect("write shim");
        let adapter = CodexAdapter::new(Some(&shim.display().to_string()));

        assert_eq!(
            orchestrator_initial_prompt(
                &adapter,
                None,
                &cfg,
                Some(&home),
                tmp.path(),
                true,
                &state,
                None,
                PromptRole::Orchestrator,
            ),
            None,
            "--simple must still suppress every zirv-injected layer, fallback included"
        );
    }

    #[test]
    fn chat_is_an_orchestrator_session() {
        let adapter = ClaudeAdapter::new(None);
        assert_eq!(
            build_launch(&adapter, None, &[]).role,
            PromptRole::Orchestrator,
            "a chat session is the one a human talks to directly"
        );
        // Resuming or adding extra flags must not change that.
        assert_eq!(
            build_launch(&adapter, Some("resume this"), &["--model".to_string()]).role,
            PromptRole::Orchestrator
        );
    }

    /// N1: a chat session's registry record must say "chat", not fall back to
    /// `wrap`'s own default verb -- `wrap::run_with` takes a `verb` parameter
    /// precisely so this can be threaded through explicitly rather than
    /// guessed from `role` (the two are independent: role governs prompt
    /// injection permissions, verb only names the calling verb for the
    /// registry). Unit-tested here, on the same pure `build_launch` the role
    /// assertions above already exercise, rather than through a real pty.
    #[test]
    fn chat_registers_as_chat_rather_than_wrap() {
        let adapter = ClaudeAdapter::new(None);
        assert_eq!(
            build_launch(&adapter, None, &[]).verb,
            crate::commands::ctx::sessions::Verb::Chat,
        );
        // Resuming or adding extra flags must not change that either.
        assert_eq!(
            build_launch(&adapter, Some("resume this"), &["--model".to_string()]).verb,
            crate::commands::ctx::sessions::Verb::Chat,
        );
    }

    /// F3: the dashboard's orchestrator pane must carry the same composed
    /// prompt the `wrap` fallback builds -- the shipped default layer proves
    /// injection happened at all, and the harness meta-teaching layer proves
    /// it happened as an *Orchestrator*. Before this fix the dashboard
    /// branch handed `run_dashboard` the bare adapter argv, so the one
    /// session a human talks to was the only unprompted one in the codebase.
    #[test]
    fn the_dash_orchestrator_pane_carries_the_composed_prompt() {
        let tmp = crate::commands::ctx::testenv::repo();
        let home = tmp.path().join("home");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);
        let state = StateDir::from_root(tmp.path().join("state"));
        let cfg = CtxConfig::default();

        // A binary that does not exist and is not a cmd shim: the file-flag
        // capability probe fails and the launch is not the reparsed `cmd.exe /c`
        // form, so `injection_args_for_session` uses the inline
        // `system_prompt_args` form and the prompt text is visible in argv --
        // which is what makes this assertable without a real agent. (The shim
        // form, which forces the file form, is covered in `prompt.rs`.)
        let adapter = ClaudeAdapter::new(Some("/nonexistent/fake-claude"));
        let launch = build_launch(&adapter, None, &[]);
        let pane = dash_orchestrator_pane(
            &adapter,
            launch,
            &cfg,
            &state,
            tmp.path(),
            "11111111-2222-4333-8444-555555555555",
            false,
            None,
        )
        .expect("pane");

        let argv = pane.argv.join(" ");
        assert!(
            argv.contains("zirv engineering standard"),
            "the shipped default layer proves injection happened: {argv}"
        );
        assert!(
            argv.contains("zirv meta-harness"),
            "an orchestrator session gets the harness delegation layer: {argv}"
        );
        assert_eq!(pane.role, PromptRole::Orchestrator);
        assert_eq!(pane.verb, crate::commands::ctx::sessions::Verb::Chat);
        assert_eq!(pane.title, "orch");
        assert_eq!(
            pane.argv.first().map(String::as_str),
            Some("/nonexistent/fake-claude"),
            "the launch program is still the adapter's own binary: {argv}"
        );
    }

    #[test]
    fn task_selected_skill_descriptions_change_late_launch_bytes_but_not_discovery() {
        let tmp = crate::commands::ctx::testenv::repo();
        let home = tmp.path().join("home");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);
        let skills = tmp.path().join(".zirv/skills");
        std::fs::create_dir_all(&skills).expect("skills");
        let descriptions = [
            (
                "aa-database-helper",
                "database migration schema alpha ".repeat(16),
            ),
            (
                "ab-security-helper",
                "security credential audit beta ".repeat(16),
            ),
            (
                "ac-database-helper",
                "database migration schema gamma ".repeat(16),
            ),
            (
                "ad-security-helper",
                "security credential audit delta ".repeat(16),
            ),
        ];
        for (id, description) in &descriptions {
            std::fs::write(
                skills.join(format!("{id}.yaml")),
                format!(
                    "schema_version: 1\nid: {id}\nversion: 1\nname: {id}\n\
                     description: {description}\nimplicit_activation: true\n\
                     context_budget_bytes: 64\nphases: [implement]\ninstructions: use safely\n"
                ),
            )
            .expect("skill fixture");
        }
        let entries = super::super::prompt::skill_index_entries(tmp.path(), Some(&home), false)
            .expect("skill entries");
        let adapter = ClaudeAdapter::new(Some("/nonexistent/fake-claude"));
        let task = "Fix the CSS frontend layout";
        let credential_env = "CHAT_TEST_JEV_SKILL_DESCRIPTIONS_737";
        // SAFETY (test-only): this test owns a unique environment variable.
        unsafe { std::env::set_var(credential_env, "secret") };
        let mut delivered = Vec::new();
        for (case, kept_skill) in [
            ("data", "aa-database-helper"),
            ("security", "ab-security-helper"),
        ] {
            let answers = descriptions
                .iter()
                .map(|(id, _)| {
                    let index = entries
                        .iter()
                        .position(|(entry_id, _, _)| entry_id == id)
                        .expect("fixture skill");
                    (
                        format!("s{index}"),
                        serde_json::json!({"type":"noul","noul": if *id == kept_skill {0.98} else {0.02}}),
                    )
                })
                .collect::<serde_json::Map<_, _>>();
            let body = serde_json::json!({"model":"jev-latest","answers":answers,"usage":{"input_tokens":8,"output_tokens":1}}).to_string();
            let (url, handle) =
                super::super::jev::tests::one_shot_server(200, Box::leak(body.into_boxed_str()));
            let mut cfg = CtxConfig::default();
            cfg.jev.context = true;
            // Issue #755: this test's own `entries` above are computed
            // unfiltered (`false`) and asserted against verbatim below, so
            // the real launch must see the identical, unfiltered entry
            // list -- unrelated to what this test is about (Jev-driven
            // optional-description selection, not the repo-signal family
            // filter).
            cfg.prompt.skill_index_repo_filter = false;
            cfg.proxy.typesafe.base_url = url;
            cfg.proxy.typesafe.credential_env = credential_env.into();
            let state = StateDir::from_root(tmp.path().join(format!("state-{case}")));
            let pane = dash_orchestrator_pane_with_task(
                &adapter,
                build_launch(&adapter, Some(task), &[]),
                &cfg,
                &state,
                tmp.path(),
                "11111111-2222-4333-8444-555555555555",
                false,
                None,
                Some(task),
            )
            .expect("pane");
            handle.join().expect("Jev fixture");
            delivered.push(pane.argv.join(" "));
        }
        let header = super::super::prompt::SKILL_DESCRIPTIONS_HEADER;
        let (first_prefix, first_late) = delivered[0].split_once(header).expect("late layer");
        let (second_prefix, second_late) = delivered[1].split_once(header).expect("late layer");
        assert_eq!(
            first_prefix, second_prefix,
            "task-independent launch prefix"
        );
        let index = first_prefix
            .split_once(super::super::prompt::SKILL_INDEX_HEADER)
            .expect("skill index")
            .1
            .split("\n\n---")
            .next()
            .expect("index body");
        for (id, _, _) in &entries {
            assert!(index.contains(&format!("- {id}")), "missing skill ID {id}");
        }
        assert!(first_prefix.contains("zirv skill load <id>"));
        for (_, description) in &descriptions {
            assert!(!index.contains(description));
        }
        assert!(first_late.contains(&descriptions[0].1));
        assert!(!first_late.contains(&descriptions[1].1));
        assert!(!second_late.contains(&descriptions[0].1));
        assert!(second_late.contains(&descriptions[1].1));

        let mut baseline_cfg = CtxConfig::default();
        // Issue #755: keep the family filter off here too, so this baseline
        // (jev disabled) differs from `delivered` only in the jev-driven
        // optional-description trim this test is actually about, not also
        // in family-filtered skill-index byte count.
        baseline_cfg.prompt.skill_index_repo_filter = false;
        baseline_cfg.proxy.typesafe.credential_env = credential_env.into();
        let baseline_state = StateDir::from_root(tmp.path().join("state-baseline"));
        let baseline = dash_orchestrator_pane_with_task(
            &adapter,
            build_launch(&adapter, Some(task), &[]),
            &baseline_cfg,
            &baseline_state,
            tmp.path(),
            "11111111-2222-4333-8444-555555555555",
            false,
            None,
            Some(task),
        )
        .expect("baseline pane")
        .argv
        .join(" ");
        assert!(
            baseline.contains(super::super::prompt::SKILL_INDEX_HEADER),
            "the baseline launch must fit the inline argv budget untruncated, or the comparison \
             below measures the budget's layer strip instead of the Jev trim"
        );
        assert!(delivered.iter().all(|prompt| prompt.len() < baseline.len()));
        assert!(
            !baseline.contains(header),
            "inactive gate keeps old prompt bytes"
        );

        let mut explicit_cfg = baseline_cfg;
        explicit_cfg.jev.context = true;
        explicit_cfg.proxy.typesafe.base_url = "http://127.0.0.1:0".into();
        let explicit_state = StateDir::from_root(tmp.path().join("state-explicit"));
        let explicit_task = "Use aa-database-helper for the frontend layout";
        let explicit = dash_orchestrator_pane_with_task(
            &adapter,
            build_launch(&adapter, Some(explicit_task), &[]),
            &explicit_cfg,
            &explicit_state,
            tmp.path(),
            "11111111-2222-4333-8444-555555555555",
            false,
            None,
            Some(explicit_task),
        )
        .expect("explicit pane")
        .argv
        .join(" ");
        unsafe { std::env::remove_var(credential_env) };
        assert!(explicit.contains(&descriptions[0].1));
        assert!(!explicit_state.root().join("jev-decisions.jsonl").exists());
    }

    /// Issue #537 (T2a): the dashboard orchestrator pane folds the harness
    /// proxy's own bounded layer onto its compiled context when it is given
    /// one, and (the companion assertion) never does when it is not --
    /// same shape as the test above, but with `proxy_layer` set.
    #[test]
    fn the_dash_orchestrator_pane_carries_the_proxy_layer_only_when_given_one() {
        let tmp = crate::commands::ctx::testenv::repo();
        let home = tmp.path().join("home");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);
        let state = StateDir::from_root(tmp.path().join("state"));
        let cfg = CtxConfig::default();
        let adapter = ClaudeAdapter::new(Some("/nonexistent/fake-claude"));

        let without = dash_orchestrator_pane(
            &adapter,
            build_launch(&adapter, None, &[]),
            &cfg,
            &state,
            tmp.path(),
            "11111111-2222-4333-8444-555555555555",
            false,
            None,
        )
        .expect("pane");
        assert!(
            !without.argv.join(" ").contains("[zirv proxy]"),
            "no decision given, no proxy layer: {:?}",
            without.argv
        );

        let with = dash_orchestrator_pane(
            &adapter,
            build_launch(&adapter, None, &[]),
            &cfg,
            &state,
            tmp.path(),
            "11111111-2222-4333-8444-555555555555",
            false,
            Some("[zirv proxy]\nexecution: bounded"),
        )
        .expect("pane");
        assert!(
            with.argv.join(" ").contains("[zirv proxy]"),
            "a given decision must reach the pane's own argv: {:?}",
            with.argv
        );
    }

    /// Bug B (harness/model parity, 2026-08-22): the dashboard's own
    /// orchestrator pane is the interactive session a human actually
    /// watches -- previously the one path a codex operator saw zero
    /// zirv-applied argv restriction on. It now carries the shipped-default
    /// sandbox posture too, and an operator's own explicit pin still wins.
    #[test]
    fn the_dash_orchestrator_pane_carries_the_shipped_sandbox_posture_by_default() {
        let tmp = crate::commands::ctx::testenv::repo();
        let home = tmp.path().join("home");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);
        let state = StateDir::from_root(tmp.path().join("state"));
        let cfg = CtxConfig::default();

        let adapter = CodexAdapter::new(Some("/nonexistent/fake-codex"));
        let launch = build_launch(&adapter, None, &[]);
        let pane = dash_orchestrator_pane(
            &adapter,
            launch,
            &cfg,
            &state,
            tmp.path(),
            "11111111-2222-4333-8444-555555555555",
            false,
            None,
        )
        .expect("pane");
        assert!(
            pane.argv
                .windows(2)
                .any(|w| w == ["--sandbox", "workspace-write"]),
            "got {:?}",
            pane.argv
        );
        assert!(
            pane.argv
                .windows(2)
                .any(|w| w == ["--ask-for-approval", "never"]),
            "got {:?}",
            pane.argv
        );
    }

    /// An operator's own explicit `--sandbox`/`--ask-for-approval` (passed
    /// after `--` on `zirv chat`) suppresses the zirv-computed prefix
    /// entirely, the same `flags_pin_policy` contract every other seam
    /// honours.
    #[test]
    fn the_dash_orchestrator_pane_lets_an_operators_own_sandbox_flag_win() {
        let tmp = crate::commands::ctx::testenv::repo();
        let home = tmp.path().join("home");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);
        let state = StateDir::from_root(tmp.path().join("state"));
        let cfg = CtxConfig::default();

        let adapter = CodexAdapter::new(Some("/nonexistent/fake-codex"));
        let extra = vec!["--sandbox".to_string(), "danger-full-access".to_string()];
        let launch = build_launch(&adapter, None, &extra);
        let pane = dash_orchestrator_pane(
            &adapter,
            launch,
            &cfg,
            &state,
            tmp.path(),
            "11111111-2222-4333-8444-555555555555",
            false,
            None,
        )
        .expect("pane");
        assert_eq!(
            pane.argv
                .iter()
                .filter(|a| a.as_str() == "--sandbox")
                .count(),
            1,
            "the operator's own --sandbox must appear exactly once, not augmented: {:?}",
            pane.argv
        );
        assert!(pane.argv.contains(&"danger-full-access".to_string()));
    }

    /// `[sandbox] enabled = false` restores the pre-2026-08-22 behaviour: no
    /// posture argv from this seam at all.
    #[test]
    fn the_dash_orchestrator_pane_carries_nothing_when_the_sandbox_posture_is_opted_out() {
        let tmp = crate::commands::ctx::testenv::repo();
        let home = tmp.path().join("home");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);
        let state = StateDir::from_root(tmp.path().join("state"));
        let cfg = CtxConfig {
            sandbox: crate::commands::ctx::config::SandboxConfig {
                enabled: false,
                ..Default::default()
            },
            ..CtxConfig::default()
        };

        let adapter = CodexAdapter::new(Some("/nonexistent/fake-codex"));
        let launch = build_launch(&adapter, None, &[]);
        let pane = dash_orchestrator_pane(
            &adapter,
            launch,
            &cfg,
            &state,
            tmp.path(),
            "11111111-2222-4333-8444-555555555555",
            false,
            None,
        )
        .expect("pane");
        assert!(
            !pane.argv.contains(&"--sandbox".to_string()),
            "got {:?}",
            pane.argv
        );
    }

    /// Issue #34 seam coverage (memory review, fix round): the dashboard
    /// orchestrator pane's composed prompt must actually carry the memory
    /// core layer, bounded by the CONFIGURED `cfg.memory.core_max_bytes` --
    /// not a hardcoded default. A tiny cap forces `prompt::with_memory_layer`
    /// to truncate, which only happens if the seam really threads the
    /// configured value through (see `with_memory_layer`'s own truncation
    /// note).
    #[test]
    fn the_dash_orchestrator_pane_carries_the_memory_layer_under_its_configured_cap() {
        let tmp = crate::commands::ctx::testenv::repo();
        let home = tmp.path().join("home");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);
        let state = StateDir::from_root(tmp.path().join("state"));
        let mut cfg = CtxConfig::default();
        cfg.memory.core_max_bytes = 40;
        // Issue #155: the merged memory layer is capped by the SUM of the two
        // budgets now, not `core_max_bytes` alone -- zero the retrieval half
        // out so this test's tiny budget still actually bounds what gets
        // delivered.
        cfg.memory.retrieval_max_bytes = 0;
        let slug = crate::commands::ctx::state::repo_slug(tmp.path());

        crate::commands::ctx::memory::remember(
            &state,
            &slug,
            &crate::commands::ctx::memory::Entry {
                key: "seam-fact".to_string(),
                written_by: "test".to_string(),
                written: 1,
                verified: 1,
                source: "explicit".to_string(),
                body: format!("{}TAIL_MARKER_NOT_TRUNCATED", "z".repeat(200)),
                importance: None,
                confidence: None,
                tags: Vec::new(),
                paths: Vec::new(),
            },
            &cfg,
        )
        .expect("remember");

        let adapter = ClaudeAdapter::new(Some("/nonexistent/fake-claude"));
        let launch = build_launch(&adapter, None, &[]);
        let pane = dash_orchestrator_pane(
            &adapter,
            launch,
            &cfg,
            &state,
            tmp.path(),
            "11111111-2222-4333-8444-555555555555",
            false,
            None,
        )
        .expect("pane");

        let argv = pane.argv.join(" ");
        assert!(
            argv.contains("seam-fact"),
            "the memory core layer must reach the composed prompt: {argv}"
        );
        assert!(
            !argv.contains("TAIL_MARKER_NOT_TRUNCATED"),
            "a tiny core_max_bytes must actually bound the delivered memory layer: {argv}"
        );
        assert!(
            argv.contains("[memory truncated:"),
            "the truncation must be visible, not silent: {argv}"
        );
    }

    /// The orchestrator is never body-delivered mail -- it gets the header's
    /// one-line unread-count advisory instead. Same trust split `wrap`'s own
    /// orchestrator path holds (it never calls `with_mail_layer` either);
    /// only a headless Worker session is handed message bodies.
    #[test]
    fn the_dash_orchestrator_pane_is_never_given_mail_bodies() {
        let tmp = crate::commands::ctx::testenv::repo();
        let home = tmp.path().join("home");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);
        let state = StateDir::from_root(tmp.path().join("state"));
        let cfg = CtxConfig::default();
        let slug = crate::commands::ctx::state::repo_slug(tmp.path());
        crate::commands::ctx::mail::store(
            &state,
            &slug,
            &crate::commands::ctx::mail::Message {
                from_session: "s1".to_string(),
                from_agent: "claude".to_string(),
                to: "any".to_string(),
                to_session: None,
                sent: 1,
                body: "SECRET-MAIL-BODY-MARKER".to_string(),
            },
            &cfg,
        )
        .expect("store");

        let adapter = ClaudeAdapter::new(Some("/nonexistent/fake-claude"));
        let launch = build_launch(&adapter, None, &[]);
        let pane = dash_orchestrator_pane(
            &adapter,
            launch,
            &cfg,
            &state,
            tmp.path(),
            "11111111-2222-4333-8444-555555555555",
            false,
            None,
        )
        .expect("pane");

        let argv = pane.argv.join(" ");
        assert!(
            !argv.contains("SECRET-MAIL-BODY-MARKER"),
            "an interactive orchestrator never receives message bodies: {argv}"
        );
        assert!(
            crate::commands::ctx::mail::list(&state, &slug, None, None)
                .expect("list")
                .len()
                == 1,
            "and nothing was consumed on its behalf either"
        );
    }

    /// `--simple` promises no zirv-*injected instruction* -- the composed
    /// prompt layer -- at all. It also makes the terminal dashboard-
    /// ineligible, so this path is unreachable in practice today -- pinned
    /// anyway, because the flag's meaning must not depend on which launch
    /// path happens to be taken.
    ///
    /// 2026-08-22 revision: the shipped-default sandbox posture
    /// (`adapters::policy_launch_args`) is a *safety* flag layer, not
    /// injected instruction text, so `--simple` does not withhold it --
    /// otherwise `--simple` would double as an accidental way to disable
    /// the default sandboxing, which is not what "skip zirv's injected
    /// text" asks for. This test now pins that the session pin *and* the
    /// sandbox prefix survive `--simple`, and nothing else does.
    #[test]
    fn a_simple_dash_orchestrator_pane_still_carries_the_sandbox_posture_but_no_injected_prompt() {
        let tmp = crate::commands::ctx::testenv::repo();
        let home = tmp.path().join("home");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);
        let state = StateDir::from_root(tmp.path().join("state"));
        let cfg = CtxConfig::default();

        let adapter = ClaudeAdapter::new(Some("/nonexistent/fake-claude"));
        let launch = build_launch(&adapter, None, &[]);
        let mut expected = launch.argv.clone();
        let pane = dash_orchestrator_pane(
            &adapter,
            launch,
            &cfg,
            &state,
            tmp.path(),
            "11111111-2222-4333-8444-555555555555",
            true,
            None,
        )
        .expect("pane");
        // R1: the session pin is launch plumbing, not injected instruction --
        // `--simple` promises the agent no zirv-authored text, and a pane that
        // cannot be resumed after a quit is not what it is asking for.
        expected.extend(adapter.default_sandbox_args(
            &Default::default(),
            &Default::default(),
            &[],
            super::super::adapters::LaunchMode::Interactive,
        ));
        expected.extend(adapter.session_pin_args("11111111-2222-4333-8444-555555555555"));
        assert_eq!(
            pane.argv, expected,
            "--simple leaves the adapter's own argv untouched apart from the sandbox posture \
             and the session pin"
        );
    }

    /// R1: the roster stores zirv's own uuid, so a dashboard pane has to make
    /// the harness adopt it as the conversation id -- otherwise the next
    /// launch's restore runs `claude --resume <uuid zirv invented>` and the
    /// restored pane dies with "no conversation found" before it draws a
    /// frame.
    #[test]
    fn the_dash_orchestrator_pane_pins_the_harness_session_to_zirvs_own_uuid() {
        let tmp = crate::commands::ctx::testenv::repo();
        let home = tmp.path().join("home");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);
        let state = StateDir::from_root(tmp.path().join("state"));
        let cfg = CtxConfig::default();

        let session = "11111111-2222-4333-8444-555555555555";
        let adapter = ClaudeAdapter::new(Some("/nonexistent/fake-claude"));
        let launch = build_launch(&adapter, None, &[]);
        let pane = dash_orchestrator_pane(
            &adapter,
            launch,
            &cfg,
            &state,
            tmp.path(),
            session,
            false,
            None,
        )
        .expect("pane");

        let pin = pane
            .argv
            .iter()
            .position(|a| a == "--session-id")
            .unwrap_or_else(|| panic!("no --session-id in {:?}", pane.argv));
        assert_eq!(
            pane.argv.get(pin + 1).map(String::as_str),
            Some(session),
            "the pinned id is the pane's own registry session id: {:?}",
            pane.argv
        );
    }

    /// D3: an operator who passed their own resume flag has already said which
    /// conversation this seat is. Appending a fresh `--session-id` on top of it
    /// hands the harness two contradictory ids and gets the launch refused
    /// outright -- and inside a dashboard the pane then died on the spot and was
    /// reaped, so the failure was invisible.
    #[test]
    fn an_operators_own_resume_flag_suppresses_the_session_pin() {
        let tmp = crate::commands::ctx::testenv::repo();
        let home = tmp.path().join("home");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);
        let state = StateDir::from_root(tmp.path().join("state"));
        let cfg = CtxConfig::default();

        let session = "11111111-2222-4333-8444-555555555555";
        let existing = "99999999-8888-4777-8666-555555555555";
        for extra in [
            vec!["--resume".to_string(), existing.to_string()],
            vec![format!("--resume={existing}")],
            vec!["--session-id".to_string(), existing.to_string()],
            vec![format!("--session-id={existing}")],
            vec!["-c".to_string()],
            vec!["--continue".to_string()],
            vec!["--fork-session".to_string()],
        ] {
            let adapter = ClaudeAdapter::new(Some("/nonexistent/fake-claude"));
            let launch = build_launch(&adapter, None, &extra);
            let pane = dash_orchestrator_pane(
                &adapter,
                launch,
                &cfg,
                &state,
                tmp.path(),
                session,
                false,
                None,
            )
            .expect("pane");

            assert!(
                !pane.argv.iter().any(|a| a == session),
                "no fresh pin may be appended alongside {extra:?}: {:?}",
                pane.argv
            );
            assert!(
                pane.argv.iter().any(|a| a.contains(existing)
                    || a == "-c"
                    || a == "--continue"
                    || a == "--fork-session"),
                "and the operator's own flag still reaches the harness: {:?}",
                pane.argv
            );
        }
    }

    /// F6: what the roster actually records when the pin is suppressed. The
    /// `PaneSpec` keeps zirv's own uuid whatever the operator pinned, so the
    /// stored id and the harness's real conversation id differ -- and the only
    /// thing that makes that inert is the orchestrator being excluded from
    /// restore (`dash::restorable_candidates`, pinned by its own test). This
    /// test is the other end of that pair: it states the mismatch plainly, so
    /// a future change that starts restoring orchestrators has to face it.
    #[test]
    fn a_pin_suppressed_orchestrator_pane_still_carries_zirvs_own_session_id() {
        let tmp = crate::commands::ctx::testenv::repo();
        let home = tmp.path().join("home");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);
        let state = StateDir::from_root(tmp.path().join("state"));
        let cfg = CtxConfig::default();

        let session = "11111111-2222-4333-8444-555555555555";
        let existing = "99999999-8888-4777-8666-555555555555";
        let adapter = ClaudeAdapter::new(Some("/nonexistent/fake-claude"));
        let launch = build_launch(
            &adapter,
            None,
            &["--resume".to_string(), existing.to_string()],
        );
        let pane = dash_orchestrator_pane(
            &adapter,
            launch,
            &cfg,
            &state,
            tmp.path(),
            session,
            false,
            None,
        )
        .expect("pane");

        assert_eq!(
            pane.session_id, session,
            "the pane -- and so the roster entry built from it -- keeps zirv's own uuid"
        );
        assert!(
            pane.argv.iter().any(|a| a == existing),
            "while the harness is actually resuming the operator's conversation: {:?}",
            pane.argv
        );
        assert_eq!(
            pane.verb,
            crate::commands::ctx::sessions::Verb::Chat,
            "and it is the verb `on_quit` stamps ROLE_ORCHESTRATOR from, which is what keeps \
             the mismatch out of any restore"
        );
    }

    // The `chat.model` disclosure. `chat.model` is repo-settable because the
    // choice is supposed to be visible; `chrome.banner` is not
    // `REPO_FORBIDDEN`, so the banner alone could be turned off by the same
    // repo that chose the model. `chrome.events` is.

    // `cfg_with_model` is the shared helper defined further down with the
    // Task 6 model-splice tests.

    #[test]
    fn a_configured_chat_model_is_disclosed_on_the_events_channel() {
        let mut err = Vec::new();
        announce_model_choice(&mut err, &cfg_with_model(Some("fable")), false);
        let text = String::from_utf8(err).expect("utf8");
        assert!(
            text.contains("chat model 'fable' (from config)"),
            "got {text:?}"
        );
    }

    #[test]
    fn no_configured_model_discloses_nothing() {
        let mut err = Vec::new();
        announce_model_choice(&mut err, &cfg_with_model(None), false);
        assert!(err.is_empty(), "got {err:?}");
    }

    /// The operator may silence it; a repo may not. `--quiet` reaches this as
    /// the flag (config was loaded before it was folded into the environment),
    /// `ZIRV_CTX_QUIET`/`[chrome] events = false` reach it as
    /// `cfg.chrome.events` -- both are operator-controlled surfaces.
    #[test]
    fn the_operator_can_silence_the_model_disclosure_but_a_repo_cannot() {
        let mut err = Vec::new();
        announce_model_choice(&mut err, &cfg_with_model(Some("fable")), true);
        assert!(err.is_empty(), "--quiet silences it: {err:?}");

        let mut quiet_cfg = cfg_with_model(Some("fable"));
        quiet_cfg.chrome.events = false;
        let mut err = Vec::new();
        announce_model_choice(&mut err, &quiet_cfg, false);
        assert!(
            err.is_empty(),
            "ZIRV_CTX_QUIET / [chrome] events = false silences it too: {err:?}"
        );

        // And the repo's own lever does not: `chrome.banner` is not
        // `REPO_FORBIDDEN`, so a repo can turn the banner off -- the events
        // line is emitted regardless of it.
        let mut bannerless = cfg_with_model(Some("fable"));
        bannerless.chrome.banner = false;
        let mut err = Vec::new();
        announce_model_choice(&mut err, &bannerless, false);
        assert!(
            String::from_utf8(err).expect("utf8").contains("fable"),
            "a repo-disabled banner must not take the disclosure with it"
        );
    }

    /// The exact scenario the finding describes, end to end through the real
    /// config loader: a repo turns its banner off and picks a model. It may do
    /// both -- neither key is repo-forbidden -- and the events line is what
    /// discloses the choice anyway. A repo that tries to silence *that* channel
    /// does not get a quiet session, it gets a refusal.
    #[test]
    fn a_repo_can_hide_the_banner_and_pick_a_model_but_cannot_hide_the_disclosure() {
        let repo = crate::commands::ctx::testenv::repo();
        let dir = repo.path().join(".zirv");
        std::fs::create_dir_all(&dir).expect("mkdir .zirv");
        std::fs::write(
            dir.join("ctx.toml"),
            "[chrome]\nbanner = false\n\n[chat]\nmodel = \"sneaky\"\n",
        )
        .expect("write repo ctx.toml");
        let env: std::collections::HashMap<String, String> = Default::default();
        let cfg = CtxConfig::load(repo.path(), &|k| env.get(k).cloned()).expect("load");

        assert!(!cfg.chrome.banner, "a repo may turn the banner off");
        assert_eq!(
            cfg.chat.model.as_deref(),
            Some("sneaky"),
            "and it may still choose the model"
        );
        assert!(
            cfg.chrome.events,
            "but the announcement channel is still on"
        );

        let mut err = Vec::new();
        announce_model_choice(&mut err, &cfg, false);
        assert!(
            String::from_utf8(err).expect("utf8").contains("sneaky"),
            "so the choice is disclosed anyway"
        );

        // And the channel itself is `REPO_FORBIDDEN`: a repo reaching for it
        // fails the load outright rather than quietly winning.
        std::fs::write(
            dir.join("ctx.toml"),
            "[chrome]\nevents = false\n\n[chat]\nmodel = \"sneaky\"\n",
        )
        .expect("write repo ctx.toml");
        let err = CtxConfig::load(repo.path(), &|k| env.get(k).cloned())
            .expect_err("a repo may not set chrome.events");
        assert!(
            err.to_string().contains("chrome.events"),
            "the refusal names the key: {err}"
        );
    }

    /// End to end through `run_with`'s own stderr writer, on the `wrap`
    /// fallback path (the only one reachable under `cargo test`'s piped
    /// stdio). The dashboard branch cannot be driven from a test, which is
    /// precisely why the emit sits *before* the branch: one call site, both
    /// paths.
    #[test]
    fn run_with_discloses_the_model_before_it_picks_a_launch_path() {
        let repo = crate::commands::ctx::testenv::repo();
        let home = tempfile::tempdir().expect("tempdir");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());

        let state_tmp = tempfile::tempdir().expect("tempdir");
        let env: std::collections::HashMap<String, String> = [
            (
                "ZIRV_CTX_AGENT_BIN".to_string(),
                "Z:/nonexistent/agent-bin".to_string(),
            ),
            ("ZIRV_CTX_CHAT_MODEL".to_string(), "fable".to_string()),
            (
                crate::commands::ctx::state::STATE_ENV.to_string(),
                state_tmp.path().display().to_string(),
            ),
        ]
        .into();

        let mut out = Vec::new();
        let mut err_out = Vec::new();
        // The launch itself fails (the configured binary does not exist),
        // which is fine and is what the neighbouring tests already pin: the
        // disclosure happens before the launch either way.
        let _ = run_with(
            &chat_args(false),
            &mut out,
            &mut err_out,
            repo.path(),
            &|k| env.get(k).cloned(),
        );

        let text = String::from_utf8(err_out).expect("utf8");
        assert!(
            text.contains("chat model 'fable' (from config)"),
            "the disclosure must reach stderr on the wrap fallback path: {text:?}"
        );
    }

    /// R1, the other half: `wrap` is untouched. Its relaunch path expects the
    /// harness to mint a fresh conversation on every restart, so the pin lives
    /// at the dashboard-pane seam and never inside `interactive_cmd`/
    /// `build_launch`.
    #[test]
    fn the_wrap_fallback_launch_is_never_session_pinned() {
        let adapter = ClaudeAdapter::new(None);
        let launch = build_launch(&adapter, Some("do the thing"), &["--model".to_string()]);
        assert!(
            !launch.argv.iter().any(|a| a == "--session-id"),
            "the plain chat/wrap launch carries no pin: {:?}",
            launch.argv
        );
    }

    #[test]
    fn resume_folds_the_latest_handoff_into_the_first_prompt() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().join("state"));
        crate::commands::ctx::handoff::store(&state, tmp.path(), "sess", &handoff())
            .expect("store");

        let mut out = Vec::new();
        let prompt = resolve_initial_prompt(
            true,
            &state,
            tmp.path(),
            &mut out,
            &super::super::screen::Thresholds::default(),
        )
        .expect("resolves")
        .expect("a handoff was stored");

        assert!(prompt.contains("Wire the payments webhook"), "got {prompt}");
        assert_eq!(
            prompt,
            resume::resume_prompt(
                &state,
                tmp.path(),
                "",
                &handoff(),
                &super::super::screen::Thresholds::default(),
            ),
            "chat must fold the handoff the same way `zirv ctx resume` does"
        );
        assert!(
            out.is_empty(),
            "no note needed when a handoff was actually found"
        );
    }

    #[test]
    fn resume_without_a_stored_handoff_starts_a_fresh_session_and_says_so() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().join("state"));

        let mut out = Vec::new();
        let prompt = resolve_initial_prompt(
            true,
            &state,
            tmp.path(),
            &mut out,
            &super::super::screen::Thresholds::default(),
        )
        .expect("resolves");

        assert_eq!(prompt, None, "nothing to fold in, so a fresh session");
        let printed = String::from_utf8(out).expect("utf8");
        assert!(
            printed.contains("--resume") && printed.to_lowercase().contains("fresh"),
            "must say why it started fresh: {printed}"
        );
    }

    #[test]
    fn no_resume_requested_never_touches_the_handoff_store() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().join("state"));
        let mut out = Vec::new();
        let prompt = resolve_initial_prompt(
            false,
            &state,
            tmp.path(),
            &mut out,
            &super::super::screen::Thresholds::default(),
        )
        .expect("resolves");
        assert_eq!(prompt, None);
        assert!(out.is_empty());
    }

    /// Issue #690: the banner's rule has to carry what the origin carries,
    /// or the one surface an operator reads at launch says "auto" where the
    /// truth is "the harness you configured nothing about is the only one
    /// you have". Injected rather than read off `PATH`: on a runner with no
    /// harness installed at all, an ambient probe would make this assert
    /// about the runner instead of about the mapping.
    #[test]
    fn the_harness_rule_carries_the_missing_harness_the_origin_named() {
        let cfg = CtxConfig::default();

        let (adapter, rule) =
            resolve_adapter_with_presence(&cfg, None, &adapters::only_installed(&["codex"]))
                .expect("codex is installed, so there is an answer");
        assert_eq!(adapter.name(), "codex");
        assert_eq!(
            rule,
            HarnessRule::FirstInstalledReady {
                not_found: "claude"
            }
        );

        let (adapter, rule) =
            resolve_adapter_with_presence(&cfg, None, &adapters::everything_installed())
                .expect("a default exists");
        assert_eq!(adapter.name(), "claude");
        assert_eq!(
            rule,
            HarnessRule::FirstEnabledReady,
            "with nothing missing the banner reads exactly as it always did"
        );

        // An explicitly requested harness bypasses presence entirely, even
        // when this machine is the one that does not have it.
        let (adapter, rule) = resolve_adapter_with_presence(
            &cfg,
            Some("claude"),
            &adapters::only_installed(&["codex"]),
        )
        .expect("an explicit --agent is never second-guessed");
        assert_eq!(adapter.name(), "claude");
        assert_eq!(rule, HarnessRule::Explicit);
    }

    /// The registry's own aggregated error (naming every candidate and why it
    /// was skipped) is the message shown when nothing is both enabled and
    /// ready -- the same one `adapters::resolve_default` produces on its own.
    /// Printed to `stderr` (not `w`/stdout: `zirv chat > log` must still show
    /// the operator something on the terminal, matching `output::error`'s own
    /// stream) and reported via exit code 1 rather than a returned `Err`:
    /// propagating it would have `zirv ctx`'s own dispatch print the same
    /// text a second time, unstyled, through `output::error`.
    #[test]
    fn chat_with_no_enabled_and_ready_adapter_names_each_candidate_and_its_reason() {
        let repo = crate::commands::ctx::testenv::repo();
        std::fs::create_dir_all(repo.path().join(".zirv")).expect("mkdir");
        std::fs::write(
            repo.path().join(".zirv/.settings.toml"),
            crate::commands::ctx::adapters::ADAPTERS
                .iter()
                .map(|(name, _)| format!("[agents.{name}]\nenabled = false\n"))
                .collect::<String>(),
        )
        .expect("write");
        let home = tempfile::tempdir().expect("tempdir");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());

        let empty: std::collections::HashMap<String, String> = std::collections::HashMap::new();
        let args = ChatArgs {
            agent: None,
            resume: false,
            simple: false,
            quiet: false,
            allow_nested: false,
            force_pace: false,
            pin_harness: false,
            no_session: false,
            runtime: None,
            proxy: false,
            no_proxy: false,
            extra: Vec::new(),
        };
        let mut out = Vec::new();
        let mut err_out = Vec::new();
        let code = run_with(&args, &mut out, &mut err_out, repo.path(), &|k| {
            empty.get(k).cloned()
        })
        .expect("prints and exits 1 rather than propagating an Err");
        assert_eq!(code, 1, "nothing is both enabled and ready");
        assert!(out.is_empty(), "nothing prints to stdout on this path");
        let msg = String::from_utf8(err_out).expect("utf8");
        assert!(msg.contains("claude"), "must name claude: {msg}");
        assert!(msg.contains("codex"), "must name codex: {msg}");
        assert!(msg.contains("opencode"), "must name opencode: {msg}");
        assert!(msg.contains("disabled"), "must say why: {msg}");
    }

    /// The gate is checked, and refuses, before any terminal or pty work:
    /// this runs synchronously, with no pty ever opened, to a printed
    /// message and exit code 1 (not a returned `Err` -- see the comment on
    /// `chat_with_no_enabled_and_ready_adapter_names_each_candidate_and_its_
    /// reason` for why). `wrap` performs the identical gate check on its
    /// own before touching a terminal, so the refusal holds even by that
    /// second, independent path.
    #[test]
    fn an_explicitly_named_disabled_agent_is_refused_before_the_terminal_is_touched() {
        let repo = crate::commands::ctx::testenv::repo();
        std::fs::create_dir_all(repo.path().join(".zirv")).expect("mkdir");
        std::fs::write(
            repo.path().join(".zirv/.settings.toml"),
            "[agents.claude]\nenabled = false\n",
        )
        .expect("write");
        let home = tempfile::tempdir().expect("tempdir");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());

        let empty: std::collections::HashMap<String, String> = std::collections::HashMap::new();
        let args = ChatArgs {
            agent: Some("claude".to_string()),
            resume: false,
            simple: false,
            quiet: false,
            allow_nested: false,
            force_pace: false,
            pin_harness: false,
            no_session: false,
            runtime: None,
            proxy: false,
            no_proxy: false,
            extra: Vec::new(),
        };
        let mut out = Vec::new();
        let mut err_out = Vec::new();
        let code = run_with(&args, &mut out, &mut err_out, repo.path(), &|k| {
            empty.get(k).cloned()
        })
        .expect("prints and exits 1");
        assert_eq!(code, 1, "claude is disabled");
        let msg = String::from_utf8(err_out).expect("utf8");
        assert!(msg.contains("claude"), "got {msg}");
        assert!(msg.contains("disabled"), "got {msg}");
    }

    /// PR #531 review finding 3: `--runtime` used to reimplement the
    /// harness/native decision inline instead of calling
    /// `runtime::selected()`, the one place that decision is supposed to be
    /// made. An unrecognised value must be refused with THAT function's own
    /// wording, not a bespoke message this module drifted from it.
    #[test]
    fn an_unknown_runtime_value_is_refused_with_runtime_selected_s_own_error() {
        let repo = crate::commands::ctx::testenv::repo();
        let home = tempfile::tempdir().expect("tempdir");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());

        let empty: std::collections::HashMap<String, String> = std::collections::HashMap::new();
        let args = ChatArgs {
            agent: None,
            resume: false,
            simple: false,
            quiet: false,
            allow_nested: false,
            force_pace: false,
            pin_harness: false,
            no_session: false,
            runtime: Some("bogus".to_string()),
            proxy: false,
            no_proxy: false,
            extra: Vec::new(),
        };
        let mut out = Vec::new();
        let mut err_out = Vec::new();
        let code = run_with(&args, &mut out, &mut err_out, repo.path(), &|k| {
            empty.get(k).cloned()
        })
        .expect("prints and exits 1 rather than propagating an Err");
        assert_eq!(code, 1);
        assert!(out.is_empty());
        let msg = String::from_utf8(err_out).expect("utf8");
        let expected = runtime_kind::selected("bogus")
            .expect_err("bogus is not a known runtime")
            .to_string();
        assert_eq!(msg.trim_end(), expected);
    }

    /// Issue #593 (roadmap N22): an explicit `--runtime harness` must
    /// override a configured `[runtime] default = "native"` and launch the
    /// normal wrapped chat -- not the native dashboard pane. Every agent is
    /// disabled so `resolve_adapter` fails deterministically, the same setup
    /// `chat_with_no_enabled_and_ready_adapter_names_each_candidate_and_its_
    /// reason` uses: reaching THAT message (naming every harness candidate)
    /// rather than `run_native_chat`'s own refusal text is the proof the
    /// wrapped path, not the native one, was taken.
    #[test]
    fn chat_runtime_harness_overrides_configured_native_default() {
        let repo = crate::commands::ctx::testenv::repo();
        std::fs::create_dir_all(repo.path().join(".zirv")).expect("mkdir");
        std::fs::write(
            repo.path().join(".zirv/.settings.toml"),
            crate::commands::ctx::adapters::ADAPTERS
                .iter()
                .map(|(name, _)| format!("[agents.{name}]\nenabled = false\n"))
                .collect::<String>(),
        )
        .expect("write");
        let home = tempfile::tempdir().expect("tempdir");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());
        std::fs::create_dir_all(home.path().join(".zirv")).expect("mkdir");
        std::fs::write(
            home.path().join(".zirv").join("ctx.toml"),
            "[runtime]\ndefault = 'native'\n",
        )
        .expect("write ctx.toml");

        let empty: std::collections::HashMap<String, String> = std::collections::HashMap::new();
        let args = ChatArgs {
            agent: None,
            resume: false,
            simple: false,
            quiet: false,
            allow_nested: false,
            force_pace: false,
            pin_harness: false,
            no_session: false,
            runtime: Some("harness".to_string()),
            proxy: false,
            no_proxy: false,
            extra: Vec::new(),
        };
        let mut out = Vec::new();
        let mut err_out = Vec::new();
        let code = run_with(&args, &mut out, &mut err_out, repo.path(), &|k| {
            empty.get(k).cloned()
        })
        .expect("prints and exits 1 rather than propagating an Err");
        assert_eq!(code, 1, "nothing is both enabled and ready");
        assert!(out.is_empty(), "no dashboard/banner is ever built here");
        let msg = String::from_utf8(err_out).expect("utf8");
        assert!(
            msg.contains("claude") && msg.contains("codex") && msg.contains("opencode"),
            "reaching resolve_adapter's every-candidate message proves the wrapped harness \
             path was taken, not run_native_chat: {msg}"
        );
        assert!(
            !msg.contains("needs an interactive terminal"),
            "run_native_chat's own refusal text must never appear: {msg}"
        );
    }

    /// Issue #540: `run_native_chat` prints the one-time experimental banner
    /// when launched through the `zirv native` alias -- signalled by
    /// `NATIVE_ALIAS_ENV`, exactly the flag `main.rs`'s alias rewrite sets --
    /// and prints it exactly once, before any of its own refusals (proven
    /// here by asserting it appears even though a non-terminal test process
    /// makes this call reach the TTY refusal too).
    #[test]
    fn native_alias_env_prints_the_banner_once() {
        let repo = crate::commands::ctx::testenv::repo();
        let cfg = CtxConfig::default();
        let env_map: std::collections::HashMap<String, String> =
            [(NATIVE_ALIAS_ENV.to_string(), "true".to_string())]
                .into_iter()
                .collect();
        let args = ChatArgs {
            agent: None,
            resume: false,
            simple: false,
            quiet: false,
            allow_nested: false,
            force_pace: false,
            pin_harness: false,
            no_session: false,
            runtime: Some("native".to_string()),
            proxy: false,
            no_proxy: false,
            extra: Vec::new(),
        };
        let mut err_out = Vec::new();
        let _ = run_native_chat(
            "native",
            &cfg,
            repo.path(),
            &|k| env_map.get(k).cloned(),
            &mut err_out,
            &args,
            false,
            false,
            false,
        );
        let msg = String::from_utf8(err_out).expect("utf8");
        assert_eq!(
            msg.matches(NATIVE_ALIAS_BANNER).count(),
            1,
            "the banner must print exactly once: {msg}"
        );
    }

    /// The mirror of the test above: an explicit `zirv chat --runtime
    /// native` never sets `NATIVE_ALIAS_ENV`, so it must never print the
    /// alias banner even though it launches through this exact same
    /// function.
    #[test]
    fn plain_runtime_native_never_prints_the_alias_banner() {
        let repo = crate::commands::ctx::testenv::repo();
        let cfg = CtxConfig::default();
        let empty: std::collections::HashMap<String, String> = std::collections::HashMap::new();
        let args = ChatArgs {
            agent: None,
            resume: false,
            simple: false,
            quiet: false,
            allow_nested: false,
            force_pace: false,
            pin_harness: false,
            no_session: false,
            runtime: Some("native".to_string()),
            proxy: false,
            no_proxy: false,
            extra: Vec::new(),
        };
        let mut err_out = Vec::new();
        let _ = run_native_chat(
            "native",
            &cfg,
            repo.path(),
            &|k| empty.get(k).cloned(),
            &mut err_out,
            &args,
            false,
            false,
            false,
        );
        let msg = String::from_utf8(err_out).expect("utf8");
        assert!(
            !msg.contains(NATIVE_ALIAS_BANNER),
            "an explicit `--runtime native` (no alias env) must never print the alias banner: \
             {msg}"
        );
    }

    /// Review finding (issue #540): `NATIVE_ALIAS_ENV` is main.rs's own
    /// internal signal, meant to be read exactly once. Left set, every child
    /// process this session later spawns (`wrap.rs`'s harness PTY,
    /// `dash/pane.rs`'s worker panes) would inherit it, since neither
    /// clears the environment before spawning. This proves `run_native_chat`
    /// clears the REAL process environment (not just its own local `env`
    /// closure argument) immediately after its one read, so a "nested" read
    /// afterward -- standing in for such a child reading its own inherited
    /// environment -- sees it unset.
    #[test]
    fn native_alias_env_is_cleared_from_the_real_process_environment_after_one_read() {
        let repo = crate::commands::ctx::testenv::repo();
        let cfg = CtxConfig::default();
        // SAFETY: nextest isolates each test in its own process (this
        // repo's own convention, documented in CLAUDE.md), so no other test
        // can be reading or writing this key concurrently.
        unsafe {
            std::env::set_var(NATIVE_ALIAS_ENV, "true");
        }
        let args = ChatArgs {
            agent: None,
            resume: false,
            simple: false,
            quiet: false,
            allow_nested: false,
            force_pace: false,
            pin_harness: false,
            no_session: false,
            runtime: Some("native".to_string()),
            proxy: false,
            no_proxy: false,
            extra: Vec::new(),
        };
        let real_env = env_from_process();
        let mut err_out = Vec::new();
        let _ = run_native_chat(
            "native",
            &cfg,
            repo.path(),
            &real_env,
            &mut err_out,
            &args,
            false,
            false,
            false,
        );
        // The one read happened -- the banner proves it.
        let msg = String::from_utf8(err_out).expect("utf8");
        assert!(msg.contains(NATIVE_ALIAS_BANNER), "got: {msg}");
        // A nested read afterward, through the identical closure, must see
        // it unset -- proving the real process environment was cleared, not
        // just some local copy.
        assert_eq!(
            real_env(NATIVE_ALIAS_ENV),
            None,
            "NATIVE_ALIAS_ENV must be cleared from the real process environment \
             immediately after run_native_chat's one read"
        );
        assert!(std::env::var(NATIVE_ALIAS_ENV).is_err());
    }

    #[test]
    fn native_help_text_reports_coming_soon_without_setup_instructions() {
        let text = native_help_text();
        assert!(text.contains("coming soon"), "{text}");
        assert!(text.contains("cannot be enabled"), "{text}");
        assert!(!text.contains("provider init"), "{text}");
    }

    #[test]
    fn codex_is_a_valid_launch_target_regardless_of_readiness() {
        // Sanity: build_launch itself does not care about readiness, only
        // resolve_adapter (exercised above) does -- true whether or not
        // codex's own ready() happens to succeed on the machine running this.
        let adapter = CodexAdapter::new(None);
        let launch = build_launch(&adapter, None, &[]);
        assert_eq!(launch.agent_name, "codex");
        assert_eq!(launch.role, PromptRole::Orchestrator);
    }

    /// The chrome probe (`std::io::stdout().is_terminal()`, `term::window_
    /// size`, `term::enable_vt_output`) runs unconditionally at the top of
    /// `run_with`, before adapter resolution -- under cargo test's own piped
    /// stdio (never a terminal) it must degrade cleanly rather than panic,
    /// and a disabled agent must still be refused, with no banner printed
    /// (there is nothing to show a banner for once resolution has failed).
    #[test]
    fn resolving_a_disabled_agent_under_non_terminal_stdio_does_not_panic_and_prints_no_banner() {
        // The agent is disabled explicitly (the same setup `chat_with_no_
        // enabled_and_ready_adapter_names_each_candidate_and_its_reason`
        // uses) so this test never depends on whatever agent binaries
        // happen to be on this machine's PATH: `resolve_adapter` fails
        // deterministically before `wrap::run_with` -- and therefore before
        // any pty or subprocess -- is ever reached.
        let repo = crate::commands::ctx::testenv::repo();
        std::fs::create_dir_all(repo.path().join(".zirv")).expect("mkdir");
        std::fs::write(
            repo.path().join(".zirv/.settings.toml"),
            "[agents.claude]\nenabled = false\n",
        )
        .expect("write");
        let home = tempfile::tempdir().expect("tempdir");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());

        let empty: std::collections::HashMap<String, String> = std::collections::HashMap::new();
        let args = ChatArgs {
            agent: Some("claude".to_string()),
            resume: false,
            simple: false,
            quiet: false,
            allow_nested: false,
            force_pace: false,
            pin_harness: false,
            no_session: false,
            runtime: None,
            proxy: false,
            no_proxy: false,
            extra: Vec::new(),
        };
        let mut out = Vec::new();
        let mut err_out = Vec::new();
        // Reaching this line at all -- rather than a panic from the probe --
        // is the main thing this test pins.
        let code = run_with(&args, &mut out, &mut err_out, repo.path(), &|k| {
            empty.get(k).cloned()
        })
        .expect("prints and exits 1, no panic");
        assert_eq!(code, 1);
        assert!(
            String::from_utf8(out).expect("utf8").is_empty(),
            "no terminal means no banner, and resolution failed before the banner code anyway"
        );
        let printed = String::from_utf8(err_out).expect("utf8");
        assert!(printed.contains("disabled"), "got {printed}");
    }

    /// Issue #352: the escape hatch exists on the command line and is OFF
    /// unless it is typed. A `zirv chat` that quietly opted into an
    /// experimental runtime would be the opposite of staging it behind a
    /// flag.
    #[test]
    fn no_session_is_an_explicit_opt_out_that_defaults_to_off() {
        use clap::Parser;
        let cli = crate::commands::ctx::CtxCli::try_parse_from(["zirv ctx", "chat"])
            .expect("plain chat parses");
        let crate::commands::ctx::CtxVerb::Chat(args) = cli.verb else {
            panic!("expected chat");
        };
        assert!(!args.no_session);

        let cli =
            crate::commands::ctx::CtxCli::try_parse_from(["zirv ctx", "chat", "--no-session"])
                .expect("--no-session parses");
        let crate::commands::ctx::CtxVerb::Chat(args) = cli.verb else {
            panic!("expected chat");
        };
        assert!(args.no_session);
    }

    // F2: the nesting guard, checked before anything touches the terminal.

    fn chat_args(allow_nested: bool) -> ChatArgs {
        ChatArgs {
            agent: None,
            resume: false,
            simple: false,
            quiet: false,
            allow_nested,
            force_pace: false,
            pin_harness: false,
            no_session: false,
            runtime: None,
            proxy: false,
            no_proxy: false,
            extra: Vec::new(),
        }
    }

    /// The refusal comes out on `stderr` as exit code 1, the same shape every
    /// other `chat` refusal uses (a returned `Err` would be printed a second
    /// time by `ctx`'s own dispatch), and it names the outer session so the
    /// operator can see *which* one they were about to endanger.
    #[test]
    fn chat_refuses_to_start_inside_a_supervised_session_and_names_the_evidence() {
        let repo = crate::commands::ctx::testenv::repo();
        // The `ZIRV_CTX_AGENT_BIN` entry is a safety belt, not a fixture
        // detail: `adapters::select`/`resolve_default` call `ready()`, so an
        // agent_bin that cannot exist makes a launch structurally
        // impossible. If the guard under test ever regresses, this fails on
        // a missing binary rather than spawning a real nested agent.
        let env: std::collections::HashMap<String, String> = [
            (
                crate::commands::ctx::adapters::SESSION_ENV.to_string(),
                "abcdef12-3456-4789-8abc-def012345678".to_string(),
            ),
            (
                "ZIRV_CTX_AGENT_BIN".to_string(),
                "/nonexistent/agent-must-never-launch".to_string(),
            ),
        ]
        .into();

        let mut out = Vec::new();
        let mut err_out = Vec::new();
        let code = run_with(
            &chat_args(false),
            &mut out,
            &mut err_out,
            repo.path(),
            &|k| env.get(k).cloned(),
        )
        .expect("refuses by printing and exiting 1, not by propagating an Err");

        assert_eq!(code, 1);
        assert!(
            String::from_utf8(out).expect("utf8").is_empty(),
            "nothing goes to stdout on this path -- not even a banner"
        );
        let msg = String::from_utf8(err_out).expect("utf8");
        assert!(
            msg.contains("refusing to start inside an existing agent session"),
            "got {msg}"
        );
        assert!(msg.contains("abcdef12"), "names the outer session: {msg}");
        assert!(
            msg.contains("--allow-nested"),
            "says how to override: {msg}"
        );
    }

    /// With the override on, the guard is out of the way and resolution
    /// proceeds -- reaching the disabled-agent refusal instead. That specific
    /// later message is the evidence the guard was passed, without this test
    /// ever launching an agent.
    #[test]
    fn allow_nested_overrides_the_guard() {
        let repo = crate::commands::ctx::testenv::repo();
        std::fs::create_dir_all(repo.path().join(".zirv")).expect("mkdir");
        std::fs::write(
            repo.path().join(".zirv/.settings.toml"),
            "[agents.claude]\nenabled = false\n[agents.codex]\nenabled = false\n",
        )
        .expect("write");
        let home = tempfile::tempdir().expect("tempdir");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());

        let env: std::collections::HashMap<String, String> = [(
            crate::commands::ctx::adapters::SESSION_ENV.to_string(),
            "abcdef12-3456-4789-8abc-def012345678".to_string(),
        )]
        .into();

        let mut out = Vec::new();
        let mut err_out = Vec::new();
        let code = run_with(
            &chat_args(true),
            &mut out,
            &mut err_out,
            repo.path(),
            &|k| env.get(k).cloned(),
        )
        .expect("past the guard, onto adapter resolution");
        assert_eq!(code, 1);
        let msg = String::from_utf8(err_out).expect("utf8");
        assert!(
            !msg.contains("refusing to start inside"),
            "the guard was overridden: {msg}"
        );
        assert!(msg.contains("disabled"), "got {msg}");
    }

    /// `--allow-nested` has to reach `wrap` too: `wrap::run_with` runs the
    /// identical guard against the identical environment, so an override that
    /// stopped here would simply be refused one layer down.
    #[test]
    fn the_override_is_threaded_through_to_the_wrap_arguments() {
        let adapter = ClaudeAdapter::new(Some("/tmp/fake-claude"));
        let launch = build_launch(&adapter, None, &[]);
        for allow_nested in [false, true] {
            let wrap_args = wrap_args_for(&chat_args(allow_nested), launch.clone(), None);
            assert_eq!(
                wrap_args.allow_nested, allow_nested,
                "chat's own override has to reach wrap's identical guard"
            );
            assert_eq!(wrap_args.agent.as_deref(), Some("claude"));
            assert!(
                !wrap_args.no_supervise,
                "a chat session is always supervised"
            );
        }
    }

    #[test]
    fn quiet_folds_into_the_env_lookup_as_zirv_ctx_quiet() {
        let empty: std::collections::HashMap<String, String> = std::collections::HashMap::new();
        let base = |k: &str| empty.get(k).cloned();
        let looked_up = quiet_env(&base, true);
        assert_eq!(looked_up("ZIRV_CTX_QUIET"), Some("true".to_string()));
        assert_eq!(looked_up("ZIRV_CTX_AGENT"), None, "other keys pass through");

        let not_quiet = quiet_env(&base, false);
        assert_eq!(
            not_quiet("ZIRV_CTX_QUIET"),
            None,
            "without --quiet the underlying lookup is untouched"
        );
    }

    #[test]
    fn an_interactive_quiet_flag_overrides_the_operators_stored_zirv_ctx_quiet_false() {
        // An operator who explicitly set ZIRV_CTX_QUIET=false is still
        // overridden by an interactive --quiet flag: the flag is this
        // invocation's own request, layered on top like any other override.
        let set: std::collections::HashMap<String, String> =
            [("ZIRV_CTX_QUIET".to_string(), "false".to_string())].into();
        let base = |k: &str| set.get(k).cloned();
        let looked_up = quiet_env(&base, true);
        assert_eq!(looked_up("ZIRV_CTX_QUIET"), Some("true".to_string()));
    }

    // Task 6: dashboard wiring -- model splice and the wrap fallback.

    fn cfg_with_model(model: Option<&str>) -> CtxConfig {
        let mut cfg = CtxConfig::default();
        cfg.chat.model = model.map(str::to_string);
        cfg
    }

    /// The configured model's flags land after the positional prompt and
    /// ahead of the operator's own `--` extras; no configured model leaves the
    /// argv byte-for-byte unchanged.
    #[test]
    fn orchestrator_argv_carries_the_configured_model() {
        let adapter = ClaudeAdapter::new(Some("/tmp/fake-claude"));

        let extra = extra_with_model(
            &cfg_with_model(Some("opus")),
            &adapter,
            &["--continue".to_string()],
        );
        let with_model = build_launch(&adapter, Some("hello"), &extra);
        assert_eq!(
            with_model.argv,
            vec![
                "/tmp/fake-claude".to_string(),
                "hello".to_string(),
                "--model".to_string(),
                "opus".to_string(),
                "--continue".to_string(),
            ],
            "model flags follow the prompt, the operator's extras still land last"
        );

        let plain = extra_with_model(&cfg_with_model(None), &adapter, &["--continue".to_string()]);
        let without_model = build_launch(&adapter, Some("hello"), &plain);
        assert_eq!(
            without_model.argv,
            build_launch(&adapter, Some("hello"), &["--continue".to_string()]).argv,
            "no configured model means the argv is untouched"
        );
    }

    /// R1, the shape the old splice broke on: a program whose real argv
    /// prefix is more than one token. `launch_prefix_len()` counts only what
    /// the operator wrote, so splicing at it dropped the model flags *inside*
    /// the launcher's own arguments. Appending them as trailing extras cannot:
    /// whatever the prefix turns out to be, the flags land after the prompt.
    #[test]
    fn model_flags_never_land_inside_a_multi_token_launch_prefix() {
        // `bin_args`: "sh /tmp/stub.sh" is program + one leading argument.
        let adapter = ClaudeAdapter::new(Some("sh /tmp/stub.sh"));
        let extra = extra_with_model(&cfg_with_model(Some("fable")), &adapter, &[]);
        let launch = build_launch(&adapter, Some("do the work"), &extra);
        assert_eq!(
            launch.argv,
            vec![
                "sh".to_string(),
                "/tmp/stub.sh".to_string(),
                "do the work".to_string(),
                "--model".to_string(),
                "fable".to_string(),
            ]
        );
    }

    /// The Windows launcher rewrite specifically: an npm-installed
    /// `claude.cmd` is spawned as `cmd.exe /c <shim> ...`, a three-token
    /// prefix against a `launch_prefix_len()` of 1. The old splice put
    /// `--model fable` between `cmd.exe` and `/c`, so `cmd.exe` was handed the
    /// model flags and the agent never started.
    #[cfg(windows)]
    #[test]
    fn model_flags_land_after_the_prompt_behind_the_windows_cmd_launcher() {
        let dir = tempfile::tempdir().expect("tempdir");
        let shim = dir.path().join("claude.cmd");
        std::fs::write(&shim, "@echo off\r\n").expect("write shim");

        let adapter = ClaudeAdapter::new(Some(&shim.display().to_string()));
        let extra = extra_with_model(&cfg_with_model(Some("fable")), &adapter, &[]);
        let launch = build_launch(&adapter, Some("do the work"), &extra);

        assert_eq!(
            launch.argv,
            vec![
                launch.argv[0].clone(),
                "/c".to_string(),
                shim.display().to_string(),
                "do the work".to_string(),
                "--model".to_string(),
                "fable".to_string(),
            ],
            "the launcher prefix stays intact and the model flags trail the prompt"
        );
        assert!(
            launch.argv[0].to_lowercase().contains("cmd"),
            "the shim is routed through cmd.exe: {:?}",
            launch.argv[0]
        );
    }

    /// The dashboard eligibility gate is real terminal I/O
    /// (`std::io::stdout()`/`stdin().is_terminal()`), and `cargo test`'s own
    /// stdio is never a real terminal either way -- so under test, `zirv
    /// chat` always falls through to the `wrap` path regardless of
    /// `--simple`. This pins that the fallback is actually reached (not
    /// short-circuited by some other refusal) by letting adapter resolution
    /// succeed and following it all the way into `wrap::run_with`'s own
    /// spawn attempt, which fails fast because the binary does not exist --
    /// the fake-agent-bin pattern, never a real agent.
    #[test]
    fn simple_flag_still_reaches_the_wrap_fallback_path() {
        let repo = crate::commands::ctx::testenv::repo();
        let home = tempfile::tempdir().expect("tempdir");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());

        let state_tmp = tempfile::tempdir().expect("tempdir");
        let env: std::collections::HashMap<String, String> = [
            (
                "ZIRV_CTX_AGENT_BIN".to_string(),
                "Z:/nonexistent/agent-bin".to_string(),
            ),
            (
                crate::commands::ctx::state::STATE_ENV.to_string(),
                state_tmp.path().display().to_string(),
            ),
        ]
        .into();

        let mut out = Vec::new();
        let mut err_out = Vec::new();
        let result = run_with(
            &chat_args(false),
            &mut out,
            &mut err_out,
            repo.path(),
            &|k| env.get(k).cloned(),
        );
        // The dashboard is never reachable under `cargo test`'s own piped
        // stdio (`dash_eligible` requires a real terminal on both streams),
        // so this pins the wrap path specifically: it got far enough to
        // actually attempt the configured, nonexistent binary -- proof the
        // model splice and the dashboard branch above it did not divert or
        // corrupt the launch -- and failed there rather than anywhere
        // earlier (a disabled agent, the nesting guard, or a config error).
        let failure =
            result.expect_err("the configured binary does not exist, so the spawn must fail");
        let msg = failure.to_string();
        assert!(
            msg.contains("agent-bin") || msg.contains("Z:"),
            "expected the failure to name the configured (nonexistent) binary: {msg}"
        );
    }

    /// `chat_args(false).simple` is `false` above deliberately: the point is
    /// that even the default (non-`--simple`) path reaches `wrap` under
    /// non-terminal stdio. This companion pins that `--simple` explicitly
    /// set behaves the same way -- neither flag value changes which path a
    /// non-terminal `cargo test` run reaches.
    #[test]
    fn explicit_simple_also_reaches_the_wrap_fallback_path() {
        let repo = crate::commands::ctx::testenv::repo();
        let home = tempfile::tempdir().expect("tempdir");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());

        let mut simple_args = chat_args(false);
        simple_args.simple = true;

        let state_tmp = tempfile::tempdir().expect("tempdir");
        let env: std::collections::HashMap<String, String> = [
            (
                "ZIRV_CTX_AGENT_BIN".to_string(),
                "Z:/nonexistent/agent-bin".to_string(),
            ),
            (
                crate::commands::ctx::state::STATE_ENV.to_string(),
                state_tmp.path().display().to_string(),
            ),
        ]
        .into();

        let mut out = Vec::new();
        let mut err_out = Vec::new();
        let result = run_with(&simple_args, &mut out, &mut err_out, repo.path(), &|k| {
            env.get(k).cloned()
        });
        let failure =
            result.expect_err("the configured binary does not exist, so the spawn must fail");
        let msg = failure.to_string();
        assert!(
            msg.contains("agent-bin") || msg.contains("Z:"),
            "expected the failure to name the configured (nonexistent) binary: {msg}"
        );
    }

    /// Bug: the welcome banner used to mark every registered adapter live
    /// off `cfg.agents.is_enabled` alone, with no check that the binary
    /// exists -- on a machine with only a couple of harnesses installed,
    /// every other adapter still rendered a green `\u{25cf}`. `harness_list`
    /// now reuses `adapters::adapter_liveness` (the same issue #298 probe
    /// the injected roster gates on), so an enabled adapter confirmed absent
    /// is omitted entirely, while a disabled adapter still gets its
    /// `(name, false)` entry -- the banner keeps showing operators what they
    /// turned off, just not what they never installed.
    #[test]
    fn harness_list_omits_a_confirmed_absent_adapter_but_keeps_a_disabled_one() {
        let repo = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(repo.path().join(".zirv")).expect("mkdir");
        std::fs::write(
            repo.path().join(".zirv/.settings.toml"),
            "[agents.droid]\nenabled = false\n",
        )
        .expect("write settings");
        let settings_home = tempfile::tempdir().expect("tempdir");
        let cfg = {
            let _home = crate::commands::ctx::testenv::HomeGuard::set(settings_home.path());
            let empty: std::collections::HashMap<String, String> = std::collections::HashMap::new();
            CtxConfig {
                agents: crate::settings::AgentGate::load(repo.path(), &|k| empty.get(k).cloned())
                    .expect("load"),
                ..CtxConfig::default()
            }
        };

        // Every adapter except codex gets a stub on `PATH`, and `HOME` moves
        // to a fresh temp dir so `adapters::known_install_roots`'s widened
        // codex search (see that function's own doc comment) cannot find a
        // real binary either -- codex is confirmed absent by construction.
        let path_dir = tempfile::tempdir().expect("tempdir");
        for (name, _) in adapters::ADAPTERS {
            if *name != "codex" {
                std::fs::write(path_dir.path().join(name), "").expect("write stub");
            }
        }
        let _path_guard = crate::commands::ctx::testenv::VarGuard::set(&[(
            "PATH",
            Some(path_dir.path().to_str().expect("utf8 tempdir path")),
        )]);
        let home = tempfile::tempdir().expect("tempdir");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());

        let harnesses = harness_list(&cfg);

        assert!(
            !harnesses.iter().any(|(name, _)| name == "codex"),
            "codex is confirmed absent, so it must be omitted entirely: {harnesses:?}"
        );
        assert!(
            harnesses.contains(&("droid".to_string(), false)),
            "droid is disabled (not absent), so it must still be listed as off: {harnesses:?}"
        );
        assert!(
            harnesses.contains(&("claude".to_string(), true)),
            "claude is enabled and present, so it must still be listed as live: {harnesses:?}"
        );
    }

    // Issue #537 (T2a): the harness proxy's launch wiring.

    fn sample_decision(
        repo: &Path,
        harness: &str,
        model: &str,
        workflow: Option<&str>,
    ) -> ProxyDecision {
        ProxyDecision {
            request_sha256: "deadbeef".to_string(),
            repo: repo.to_path_buf(),
            intent: Intent::Feature,
            complexity: Complexity::Bounded,
            risk: RiskBand::Medium,
            execution: ExecutionMode::Bounded,
            // `Bounded` -> `SeatRole::Single`/`SeatTier::Standard`, mirroring
            // `decision::SeatRole::from_execution`/
            // `SeatTier::from_execution_complexity_risk` (both private to
            // that module) rather than re-deriving them.
            seat_role: SeatRole::Single,
            seat_tier: SeatTier::Standard,
            validation: ValidationProfile::default(),
            workflow: workflow.map(str::to_string),
            orchestrator: Seat {
                harness: harness.to_string(),
                model: model.to_string(),
            },
            worker_tier: Tier::Standard,
            needs_clarification: 0.0,
            needs_clarification_decisive: false,
            clarification_category: None,
            domains: Vec::new(),
            decider: Decider::Deterministic,
            confidence: BTreeMap::new(),
            reasons: Vec::new(),
            fallbacks: Vec::new(),
            elapsed_ms: 0,
            usage: None,
            created_at: 0,
            headless: false,
        }
    }

    /// The overwhelmingly common case (the proxy never configured at all):
    /// `activation` refuses on `[proxy] enabled = false`, and `proxy_intake`
    /// folds that reason into `Inactive` rather than reading stdin at all.
    #[test]
    fn proxy_disabled_by_default_is_silently_inactive() {
        let cfg = CtxConfig::default();
        let repo = crate::commands::ctx::testenv::repo();
        let state_tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(state_tmp.path().to_path_buf());
        let args = chat_args(false);

        let outcome =
            proxy_intake(&cfg, &state, repo.path(), &args, false, false).expect("never errors");

        assert_eq!(
            outcome,
            ProxyIntakeOutcome::Inactive {
                advisory: None,
                request: None
            },
            "the disabled default must be byte-identical to today, announcements included"
        );
    }

    /// The mirror of the test above: `[proxy] enabled = true` (config or
    /// `--proxy`) but not actually usable (here, the deterministic decider,
    /// which `activation` always refuses) still gets the advisory -- an
    /// operator who turned the proxy on deserves to know why it never took
    /// over, unlike the silent, never-asked-for default.
    #[test]
    fn proxy_enabled_but_unusable_still_names_why() {
        let mut cfg = CtxConfig::default();
        cfg.proxy.enabled = true;
        cfg.proxy.decider = crate::commands::ctx::config::ProxyDecider::Deterministic;
        let repo = crate::commands::ctx::testenv::repo();
        let state_tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(state_tmp.path().to_path_buf());
        let args = chat_args(false);

        let outcome =
            proxy_intake(&cfg, &state, repo.path(), &args, false, false).expect("never errors");

        match outcome {
            ProxyIntakeOutcome::Inactive {
                advisory: Some(reason),
                ..
            } => assert!(reason.contains("deterministic"), "got {reason}"),
            other => panic!("expected Inactive with a reason, got {other:?}"),
        }
    }

    /// `--resume`'s own first prompt is the stored handoff; the proxy must
    /// never insert its own intake step ahead of it. Naming the reason is
    /// conditional on `--proxy` -- proven by the companion test below.
    #[test]
    fn resume_with_an_explicit_proxy_request_is_skipped_with_a_named_reason() {
        let cfg = CtxConfig::default();
        let repo = crate::commands::ctx::testenv::repo();
        let state_tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(state_tmp.path().to_path_buf());
        let mut args = chat_args(false);
        args.resume = true;
        args.proxy = true;

        let outcome =
            proxy_intake(&cfg, &state, repo.path(), &args, false, false).expect("never errors");

        match outcome {
            ProxyIntakeOutcome::Inactive {
                advisory: Some(reason),
                ..
            } => assert!(reason.contains("--resume"), "got {reason}"),
            other => panic!("expected Inactive naming --resume, got {other:?}"),
        }
    }

    /// The mirror of the test above: `--simple` alone (no explicit
    /// `--proxy`) must skip silently -- an operator who never asked for the
    /// proxy should not see it mentioned at all.
    #[test]
    fn simple_alone_skips_silently_with_no_explicit_proxy_request() {
        let cfg = CtxConfig::default();
        let repo = crate::commands::ctx::testenv::repo();
        let state_tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(state_tmp.path().to_path_buf());
        let mut args = chat_args(false);
        args.simple = true;

        let outcome =
            proxy_intake(&cfg, &state, repo.path(), &args, false, false).expect("never errors");

        assert_eq!(
            outcome,
            ProxyIntakeOutcome::Inactive {
                advisory: None,
                request: None
            },
            "no explicit --proxy means no advisory at all"
        );
    }

    /// `--proxy` and `--no-proxy` name the same underlying `enabled`
    /// override in opposite directions; clap must refuse both together
    /// rather than silently letting one win.
    #[test]
    fn proxy_and_no_proxy_together_is_a_clap_conflict() {
        use clap::Parser;
        let result = crate::commands::ctx::CtxCli::try_parse_from([
            "zirv ctx",
            "chat",
            "--proxy",
            "--no-proxy",
        ]);
        assert!(
            result.is_err(),
            "clap must refuse --proxy together with --no-proxy"
        );
    }

    /// Activation succeeding (a usable Typesafe model configured, credential
    /// present) but stdin not a terminal leaves nowhere to read the task
    /// description from: `run_with` must refuse the whole launch rather than
    /// silently falling back, which would look like the proxy was never
    /// asked for at all.
    #[test]
    fn activation_ok_but_non_tty_stdin_refuses_the_launch() {
        let mut cfg = CtxConfig::default();
        cfg.proxy.enabled = true;
        let _cred = crate::commands::ctx::testenv::VarGuard::set(&[(
            cfg.proxy.typesafe.credential_env.as_str(),
            Some("a-test-key"),
        )]);
        let repo = crate::commands::ctx::testenv::repo();
        let state_tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(state_tmp.path().to_path_buf());
        let args = chat_args(false);

        let outcome =
            proxy_intake(&cfg, &state, repo.path(), &args, false, false).expect("never errors");

        match outcome {
            ProxyIntakeOutcome::Refuse { message } => {
                assert!(message.contains("interactive terminal"), "got {message}");
            }
            other => panic!("expected Refuse, got {other:?}"),
        }
    }

    /// A stdin that IS a terminal but a console with no VT output support
    /// (or a redirected stderr) cannot render the ratatui inline region --
    /// its cursor-movement escapes need the same VT processing the old line
    /// editor's `redraw_edit_line` did. This degrades to a routine, advised
    /// skip rather than `Refuse`: the operator still gets a harness, just
    /// without a plan.
    #[test]
    fn a_tty_stdin_with_no_vt_support_skips_with_an_advisory_instead_of_refusing() {
        let mut cfg = CtxConfig::default();
        cfg.proxy.enabled = true;
        let _cred = crate::commands::ctx::testenv::VarGuard::set(&[(
            cfg.proxy.typesafe.credential_env.as_str(),
            Some("a-test-key"),
        )]);
        let repo = crate::commands::ctx::testenv::repo();
        let state_tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(state_tmp.path().to_path_buf());
        let args = chat_args(false);

        let outcome =
            proxy_intake(&cfg, &state, repo.path(), &args, true, false).expect("never errors");

        match outcome {
            ProxyIntakeOutcome::Inactive {
                advisory: Some(reason),
                request: None,
            } => assert!(reason.contains("cannot render"), "got {reason}"),
            other => panic!("expected an advised Inactive skip, got {other:?}"),
        }
    }

    /// The wiring `run_with` applies once the proxy actually decided this
    /// launch: the decided model replaces `cfg.chat.model` and the decided
    /// harness is returned for `resolve_adapter`, so `extra_with_model`/
    /// `build_launch` (already covered by their own tests) put the decided
    /// `--model` in argv and the request text becomes the initial prompt.
    #[test]
    fn an_injected_proxy_decision_lands_the_decided_model_and_request_in_the_built_launch() {
        let repo = crate::commands::ctx::testenv::repo();
        let decision = sample_decision(repo.path(), "claude", "fable", Some("bugfix"));
        let mut cfg = CtxConfig::default();

        let requested_agent = apply_proxy_decision(&mut cfg, &decision);

        assert_eq!(requested_agent, "claude");
        assert_eq!(cfg.chat.model.as_deref(), Some("fable"));

        let adapter = ClaudeAdapter::new(Some("/tmp/fake-claude"));
        let extra = extra_with_model(&cfg, &adapter, &[]);
        let launch = build_launch(&adapter, Some("fix the flaky retry test"), &extra);

        assert!(
            launch
                .argv
                .windows(2)
                .any(|pair| pair == ["--model", "fable"]),
            "the decided model must land in argv: {:?}",
            launch.argv
        );
        assert!(
            launch
                .argv
                .contains(&"fix the flaky retry test".to_string()),
            "the request text must become the initial prompt: {:?}",
            launch.argv
        );
    }

    /// `apply_proxy_decision` is a no-op on `cfg.chat.model` for any field
    /// it does not touch: the proxy only ever replaces the model, never
    /// anything else on `cfg`.
    #[test]
    fn apply_proxy_decision_only_touches_the_chat_model() {
        let repo = crate::commands::ctx::testenv::repo();
        let decision = sample_decision(repo.path(), "codex", "o-fast", None);
        let mut cfg = CtxConfig::default();
        cfg.chat.model = Some("stale".to_string());

        let requested_agent = apply_proxy_decision(&mut cfg, &decision);

        assert_eq!(requested_agent, "codex");
        assert_eq!(cfg.chat.model.as_deref(), Some("o-fast"));
    }

    /// Issue #537 (T3): `run_with` reads the seat this launch runs as
    /// straight off `proxy_prompt_role` -- `SeatRole::Single` maps to
    /// `PromptRole::Single`, `SeatRole::Orchestrator` keeps today's
    /// `Orchestrator`, and an intake that never decided this launch at all
    /// (the disabled default, or every other `Inactive`/`Refuse` outcome)
    /// also keeps `Orchestrator`, unchanged.
    #[test]
    fn proxy_prompt_role_maps_seat_role_and_leaves_an_undecided_launch_alone() {
        let repo = crate::commands::ctx::testenv::repo();
        let mut decision = sample_decision(repo.path(), "claude", "fable", None);
        assert_eq!(
            decision.seat_role,
            SeatRole::Single,
            "sample_decision's Bounded execution is a Single seat"
        );

        let single = ProxyIntakeOutcome::Decided {
            decision: Box::new(decision.clone()),
            request: "fix a typo".to_string(),
        };
        assert_eq!(proxy_prompt_role(&single), PromptRole::Single);

        decision.seat_role = SeatRole::Orchestrator;
        let orchestrated = ProxyIntakeOutcome::Decided {
            decision: Box::new(decision),
            request: "redesign the billing pipeline".to_string(),
        };
        assert_eq!(proxy_prompt_role(&orchestrated), PromptRole::Orchestrator);

        assert_eq!(
            proxy_prompt_role(&ProxyIntakeOutcome::Inactive {
                advisory: None,
                request: None
            }),
            PromptRole::Orchestrator
        );
    }

    /// Issue #537 (T3, operator field report): the actual bug -- a direct/
    /// bounded decision started "the full orchestrator setup" -- reproduced
    /// and fixed at the launch level. A `SeatRole::Single` decision must
    /// launch with no harness meta-teaching (`HARNESS_PROMPT`, "zirv meta-
    /// harness"), no adapter orchestrator-conventions layer (`ORCHESTRATOR_
    /// PROMPT`, "it does not implement"), and its role must reach
    /// `adapters::seat_role_env` as `"single"` -- the same env pair the
    /// write guard and the subagent guard key off (see `hook.rs`'s `run_
    /// pretool_stays_silent_for_a_single_seat_editing_a_repo_file`). Same
    /// recipe `the_dash_orchestrator_pane_carries_the_composed_prompt`
    /// already proves for an `Orchestrator` decision, which this test's
    /// companion assertions confirm is still unaffected.
    #[test]
    fn a_decided_single_seat_launches_with_no_orchestrator_conventions() {
        let tmp = crate::commands::ctx::testenv::repo();
        let home = tmp.path().join("home");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);
        let state = StateDir::from_root(tmp.path().join("state"));
        let cfg = CtxConfig::default();
        let adapter = ClaudeAdapter::new(Some("/nonexistent/fake-claude"));

        let decision = sample_decision(tmp.path(), "claude", "fable", None);
        let outcome = ProxyIntakeOutcome::Decided {
            decision: Box::new(decision),
            request: "fix a typo in README".to_string(),
        };
        let role = proxy_prompt_role(&outcome);
        assert_eq!(role, PromptRole::Single);

        let mut launch = build_launch(&adapter, None, &[]);
        launch.role = role;
        let pane = dash_orchestrator_pane(
            &adapter,
            launch,
            &cfg,
            &state,
            tmp.path(),
            "11111111-2222-4333-8444-555555555555",
            false,
            None,
        )
        .expect("pane");

        let argv = pane.argv.join(" ");
        assert!(
            argv.contains("zirv engineering standard"),
            "the shipped default layer must still apply to a single seat: {argv}"
        );
        assert!(
            !argv.contains("zirv meta-harness"),
            "a single seat must not get the harness delegation layer: {argv}"
        );
        assert!(
            !argv.contains("it does not implement"),
            "a single seat must not get the orchestrator's own conventions layer: {argv}"
        );
        assert_eq!(pane.role, PromptRole::Single);
        assert_eq!(
            adapters::seat_role_env(pane.role),
            vec![(adapters::SEAT_ROLE_ENV.to_string(), "single".to_string())],
            "the hook write guard and the subagent guard both key off this env pair"
        );
    }

    /// Issue #537 (T3, native seam): the actual bug this fixes -- a native
    /// launch's `PaneSpec`/`NativeDashboardSpec` used to hardcode
    /// `Orchestrator` no matter what the proxy decided, because `proxy_
    /// intake` never even ran on that path. Reproduced at the seam that
    /// actually builds those two structs (`native_pane_spec`), fed the SAME
    /// `proxy_prompt_role(&Decided{SeatRole::Single, ..})` the wrapped
    /// path's own `a_decided_single_seat_launches_with_no_orchestrator_
    /// conventions` test above proves for `dash_orchestrator_pane`.
    #[test]
    fn native_pane_spec_uses_the_single_seat_role_for_a_decided_single_seat() {
        let repo = crate::commands::ctx::testenv::repo();
        let decision = sample_decision(repo.path(), "claude", "fable", None);
        let outcome = ProxyIntakeOutcome::Decided {
            decision: Box::new(decision),
            request: "fix a typo in README".to_string(),
        };
        let seat_role = proxy_prompt_role(&outcome);
        assert_eq!(seat_role, PromptRole::Single);

        let (pane, native) =
            native_pane_spec(repo.path(), "session-1".to_string(), seat_role, None);

        assert_eq!(pane.role, PromptRole::Single);
        assert_eq!(
            native.role, "single",
            "NativeDashboardSpec.role must carry PromptRole::Single's own \
             label, not a hardcoded string"
        );
    }

    /// The mirror of the test above: an intake that never decided this
    /// launch at all (the disabled-by-default case, exercised end to end by
    /// `proxy_disabled_by_default_is_silently_inactive`) must still produce
    /// today's `Orchestrator` seat on the native pane -- not just leave
    /// `proxy_prompt_role` unchanged (already proven by `proxy_prompt_role_
    /// maps_seat_role_and_leaves_an_undecided_launch_alone`), but actually
    /// carry that role through into both fields `run_native_chat` builds.
    #[test]
    fn native_pane_spec_keeps_the_orchestrator_role_when_the_proxy_never_decided() {
        let repo = crate::commands::ctx::testenv::repo();
        let seat_role = proxy_prompt_role(&ProxyIntakeOutcome::Inactive {
            advisory: None,
            request: None,
        });
        assert_eq!(seat_role, PromptRole::Orchestrator);

        let (pane, native) =
            native_pane_spec(repo.path(), "session-2".to_string(), seat_role, None);

        assert_eq!(pane.role, PromptRole::Orchestrator);
        assert_eq!(native.role, "orchestrator");
    }

    /// Issue #703 (follow-up to #702): the actual bug this fixes -- a native
    /// launch's `NativeDashboardSpec` used to hardcode `route: None` no
    /// matter what model the proxy decided, because `native_pane_spec` never
    /// read `ProxyDecision.orchestrator.model` at all. `proxy_decided_model`
    /// is the seam `run_native_chat` now feeds `native_pane_spec` from, the
    /// same way `proxy_prompt_role` already feeds it the seat role.
    #[test]
    fn proxy_decided_model_reaches_the_native_pane_spec_as_a_route_candidate() {
        let repo = crate::commands::ctx::testenv::repo();
        let decision = sample_decision(repo.path(), "claude", "fable", None);
        let outcome = ProxyIntakeOutcome::Decided {
            decision: Box::new(decision),
            request: "fix a typo in README".to_string(),
        };

        let model = proxy_decided_model(&outcome);
        assert_eq!(model.as_deref(), Some("fable"));

        let (_, native) = native_pane_spec(
            repo.path(),
            "session-3".to_string(),
            PromptRole::Single,
            model,
        );

        assert_eq!(
            native.route.as_deref(),
            Some("fable"),
            "the decided model must reach NativeDashboardSpec::route as the \
             candidate NativePaneRuntime::spawn validates"
        );
    }

    /// The mirror of the test above: an intake that never decided this
    /// launch leaves `proxy_decided_model` (and therefore the pane's route)
    /// `None` -- the role's own configured default route, unchanged.
    #[test]
    fn proxy_decided_model_is_none_when_the_proxy_never_decided() {
        assert_eq!(
            proxy_decided_model(&ProxyIntakeOutcome::Inactive {
                advisory: None,
                request: None
            }),
            None
        );
    }

    fn git_init_with_commit(repo: &Path) {
        let git = |cmd_args: &[&str]| {
            let status = std::process::Command::new("git")
                .args([
                    "-c",
                    "user.email=t@example.com",
                    "-c",
                    "user.name=t",
                    "-c",
                    "commit.gpgsign=false",
                ])
                .args(cmd_args)
                .current_dir(repo)
                .status()
                .expect("run git");
            assert!(status.success(), "git {cmd_args:?} failed");
        };
        git(&["init", "-q"]);
        std::fs::write(repo.join("README.md"), "hello\n").expect("write");
        git(&["add", "."]);
        git(&["commit", "-q", "-m", "base"]);
    }

    /// `start_proxy_workflow` is a plain no-op when the intake never decided
    /// this launch: no workflow is ever touched, and `close_proxy_workflow_
    /// on_failure` (given the resulting `None`) is a plain pass-through to
    /// `spawn`.
    #[test]
    fn start_proxy_workflow_is_a_no_op_when_inactive() {
        let repo = tempfile::tempdir().expect("tempdir");
        let state_tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(state_tmp.path().to_path_buf());
        let outcome = ProxyIntakeOutcome::Inactive {
            advisory: None,
            request: None,
        };
        let mut announced: Vec<String> = Vec::new();

        let started_id =
            start_proxy_workflow(&outcome, &state, repo.path(), |text| announced.push(text));
        assert_eq!(started_id, None);

        let result = close_proxy_workflow_on_failure(
            started_id.as_deref(),
            &state,
            repo.path(),
            |text| announced.push(text),
            || Ok(42),
        );

        assert_eq!(result.expect("passthrough"), 42);
        assert!(
            announced.is_empty(),
            "inactive never announces anything: {announced:?}"
        );
    }

    /// A failed spawn must close the workflow this launch started -- an
    /// orphaned "active" workflow left behind by a launch that never
    /// actually started would otherwise block every later `zirv chat`
    /// (proxy or not) that reaches the same repo, since `engine::
    /// start_workflow` never overwrites an existing active pointer.
    #[test]
    fn close_proxy_workflow_on_failure_closes_a_workflow_it_started_when_the_spawn_fails() {
        let repo = tempfile::tempdir().expect("tempdir");
        git_init_with_commit(repo.path());
        let state_tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(state_tmp.path().to_path_buf());
        // Trivial/Low keeps the built-in `bugfix` pack's conditional,
        // approval-gated `intent` step out of the materialized plan, so
        // this starts `Running` -- `engine::close` refuses to close a
        // workflow still `AwaitingApproval`.
        let mut decision = sample_decision(repo.path(), "claude", "fable", Some("bugfix"));
        decision.complexity = Complexity::Trivial;
        decision.risk = RiskBand::Low;
        let outcome = ProxyIntakeOutcome::Decided {
            decision: Box::new(decision),
            request: "fix a database retry bug".to_string(),
        };

        let mut announced: Vec<String> = Vec::new();
        let started_id =
            start_proxy_workflow(&outcome, &state, repo.path(), |text| announced.push(text));
        assert!(started_id.is_some(), "expected a started workflow id");

        let result: CtxResult<i32> = close_proxy_workflow_on_failure(
            started_id.as_deref(),
            &state,
            repo.path(),
            |text| announced.push(text),
            || Err("spawn failed".into()),
        );
        assert!(result.is_err());

        let active =
            crate::commands::workflow::engine::load_active(&state, repo.path()).expect("readable");
        assert!(
            active.is_none(),
            "a failed spawn must clear the active pointer via close_started"
        );
        assert!(
            announced.is_empty(),
            "close_started succeeded, so nothing needs announcing: {announced:?}"
        );
    }

    /// The mirror of the test above: a successful spawn leaves the started
    /// workflow running -- `close_proxy_workflow_on_failure` must never
    /// close a launch that actually succeeded.
    #[test]
    fn close_proxy_workflow_on_failure_leaves_a_successful_launch_s_workflow_running() {
        let repo = tempfile::tempdir().expect("tempdir");
        git_init_with_commit(repo.path());
        let state_tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(state_tmp.path().to_path_buf());
        let mut decision = sample_decision(repo.path(), "claude", "fable", Some("bugfix"));
        decision.complexity = Complexity::Trivial;
        decision.risk = RiskBand::Low;
        let outcome = ProxyIntakeOutcome::Decided {
            decision: Box::new(decision),
            request: "fix a database retry bug".to_string(),
        };

        let mut announced: Vec<String> = Vec::new();
        let started_id =
            start_proxy_workflow(&outcome, &state, repo.path(), |text| announced.push(text));
        assert!(started_id.is_some(), "expected a started workflow id");

        let result: CtxResult<i32> = close_proxy_workflow_on_failure(
            started_id.as_deref(),
            &state,
            repo.path(),
            |text| announced.push(text),
            || Ok(0),
        );
        assert_eq!(result.expect("spawn succeeded"), 0);

        let active = crate::commands::workflow::engine::load_active(&state, repo.path())
            .expect("readable")
            .expect("the started workflow is still active");
        assert_eq!(
            active.status,
            crate::commands::workflow::engine::WorkflowStatus::Running
        );
        assert!(
            announced.is_empty(),
            "started + successful spawn must stay silent: {announced:?}"
        );
    }

    /// Review fix (findings 1 & 2): `run_with`'s persistent-runtime attempt
    /// is never the last launch shape tried, so its own failure must not
    /// close the workflow -- proven here by simulating that failure as a
    /// plain no-op (exactly what the fixed `run_with` does: it calls
    /// `chat_via_runtime` directly, with no `close_proxy_workflow_on_failure`
    /// wrapper) and asserting the workflow is still `Running` afterwards.
    /// The dashboard branch IS the last shape once it is reached, so building
    /// its pane is folded into the SAME `close_proxy_workflow_on_failure`
    /// call as `dash::run_dashboard`, via [`run_dash_branch`] -- the exact
    /// function `run_with` itself calls -- proven by forcing `dash_
    /// orchestrator_pane` itself to fail (`protect_composed` refuses when the
    /// operator's own obfuscation config failed to load, `cfg.obfuscate.
    /// operator_load_failed`, a deterministic failure with no filesystem
    /// trickery needed) and asserting that THIS closes the still-running
    /// workflow. Before the fix, a `dash_orchestrator_pane` failure sat
    /// outside the wrapper's own `?` and left the workflow orphaned forever
    /// (the engine never overwrites an active pointer).
    ///
    /// `run_with` itself is not driven end to end here: `chat_route`/
    /// `dash_eligible` both require a real interactive terminal on both
    /// streams, which `cargo test`'s piped stdio never provides (see
    /// `run_with_discloses_the_model_before_it_picks_a_launch_path`'s own
    /// doc comment for the same limitation) -- but [`run_dash_branch`] is a
    /// free function with no TTY dependency of its own, so it is driven
    /// directly, exercising the real composition rather than a re-
    /// implementation of it.
    #[test]
    fn a_failed_runtime_attempt_leaves_the_workflow_live_for_the_dash_branch_that_actually_runs() {
        let repo = crate::commands::ctx::testenv::repo();
        git_init_with_commit(repo.path());
        let home = repo.path().join("home");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);
        let state = StateDir::from_root(repo.path().join("state"));
        let mut decision = sample_decision(repo.path(), "claude", "fable", Some("bugfix"));
        decision.complexity = Complexity::Trivial;
        decision.risk = RiskBand::Low;
        let outcome = ProxyIntakeOutcome::Decided {
            decision: Box::new(decision),
            request: "fix a database retry bug".to_string(),
        };

        let mut announced: Vec<String> = Vec::new();
        let started_id =
            start_proxy_workflow(&outcome, &state, repo.path(), |text| announced.push(text));
        assert!(started_id.is_some(), "expected a started workflow id");

        // The runtime attempt "fails" -- nothing here closes anything, which
        // is the fix: `close_proxy_workflow_on_failure` is never called
        // around it any more.
        let running_after_runtime_failure =
            crate::commands::workflow::engine::load_active(&state, repo.path())
                .expect("readable")
                .expect("still active: the runtime attempt's own failure must not touch it");
        assert_eq!(
            running_after_runtime_failure.status,
            crate::commands::workflow::engine::WorkflowStatus::Running
        );

        // The dash branch, driven through the real `run_dash_branch` -- pane
        // build folded into the same wrapped closure as `dash::run_
        // dashboard`, exactly as `run_with` now composes it.
        let adapter = ClaudeAdapter::new(Some("/nonexistent/fake-claude"));
        let launch = build_launch(&adapter, None, &[]);
        let mut cfg = CtxConfig::default();
        cfg.obfuscate.mode = super::super::config::ObfuscateMode::Flag;
        cfg.obfuscate.operator_load_failed = true;
        let env: std::collections::HashMap<String, String> = Default::default();

        let result = run_dash_branch(
            &adapter,
            launch,
            &cfg,
            &state,
            repo.path(),
            &|k| env.get(k).cloned(),
            "11111111-2222-4333-8444-555555555555",
            false,
            None,
            None,
            started_id.as_deref(),
            false,
            |text| announced.push(text),
        );
        assert!(
            result.is_err(),
            "the forced pane-build failure must propagate: {result:?}"
        );

        let active =
            crate::commands::workflow::engine::load_active(&state, repo.path()).expect("readable");
        assert!(
            active.is_none(),
            "a pane-build failure in the launch shape that actually runs must close the \
             workflow, not orphan it"
        );
    }

    /// Issue #537 review: a `Skipped` workflow start (a decision that names no
    /// workflow) must not vanish silently -- the operator has no other way to
    /// learn the proxy's own decision never actually started a workflow,
    /// unlike `runtime/native.rs`, which already announces this case.
    #[test]
    fn start_proxy_workflow_announces_a_skipped_start() {
        let repo = tempfile::tempdir().expect("tempdir");
        git_init_with_commit(repo.path());
        let state_tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(state_tmp.path().to_path_buf());

        let mut decision = sample_decision(repo.path(), "claude", "fable", None);
        decision.complexity = Complexity::Trivial;
        decision.risk = RiskBand::Low;
        let outcome = ProxyIntakeOutcome::Decided {
            decision: Box::new(decision),
            request: "do more work".to_string(),
        };
        let mut announced: Vec<String> = Vec::new();

        let started_id =
            start_proxy_workflow(&outcome, &state, repo.path(), |text| announced.push(text));
        assert_eq!(started_id, None, "a skipped start never names an id");

        let result: CtxResult<i32> = close_proxy_workflow_on_failure(
            started_id.as_deref(),
            &state,
            repo.path(),
            |text| announced.push(text),
            || Ok(0),
        );
        assert_eq!(result.expect("spawn still runs"), 0);

        assert_eq!(announced.len(), 1, "got {announced:?}");
        assert!(
            announced[0].contains("proxy: workflow not started")
                && announced[0].contains("no workflow named"),
            "must name why nothing was started: {}",
            announced[0]
        );
    }
    /// #827: with every `[jev]` gate off the runtime route must not create the alias file.
    #[test]
    fn gates_off_the_runtime_route_writes_no_session_alias() {
        let cfg = CtxConfig::default();
        let dir = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(dir.path().to_path_buf());
        alias_when_jev_active(&cfg, &state, "minted", "runtime");
        assert!(!dir.path().join("jev-session-aliases.jsonl").exists());
    }
}
