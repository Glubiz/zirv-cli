//! launch support for the interactive supervisor.

use super::*;

#[derive(Debug, clap::Args)]
pub struct WrapArgs {
    /// Adapter name: claude or codex. Detected from the command when omitted.
    #[arg(long)]
    pub agent: Option<String>,
    /// Pure passthrough: no scoring, no injection.
    #[arg(long, default_value_t = false)]
    pub no_supervise: bool,
    /// The interactive agent command, after `--`.
    #[arg(allow_hyphen_values = true, last = true)]
    pub command: Vec<String>,
    /// Simple run: skip every zirv-injected instruction, including the shipped
    /// default. Supervision, pacing and hooks still apply.
    #[arg(long, default_value_t = false)]
    pub simple: bool,
    /// Start even though this process looks like it is already inside an
    /// agent session. Off by default: a nested interactive supervisor can
    /// take the outer session down.
    #[arg(long, default_value_t = false)]
    pub allow_nested: bool,
    /// T10: skip the interactive usage-pacing prompt (the soft-band pause
    /// and the at-the-ceiling refusal shown before launch) and go straight
    /// to launching. Named in the prompt's own text as the flag alternative
    /// to a keypress, for a scripted or CI launch with nobody to answer it.
    /// Never silent about *why* it launched anyway -- see `run_with`'s own
    /// interactive-gate call site.
    #[arg(long, default_value_t = false)]
    pub force_pace: bool,
    /// Issue #537 (T2a): the harness proxy's own bounded `[zirv proxy]`
    /// layer text, already rendered (`proxy::prompt_layer`), when an active
    /// decision took over this launch. Never a CLI flag (`#[arg(skip)]`) --
    /// there is no sane way for an operator to type this; only `chat::
    /// wrap_args_for` ever sets it to `Some`. Folded onto this launch's own
    /// compiled context via `compile::with_proxy_layer`, right after the
    /// `compile::compile` call below.
    #[arg(skip)]
    pub proxy_layer: Option<String>,
}

pub fn run_with(
    args: &WrapArgs,
    repo: &Path,
    env: EnvLookup<'_>,
    role: PromptRole,
    session: Option<super::event::SessionId>,
    verb: super::sessions::Verb,
) -> CtxResult<i32> {
    if args.command.is_empty() {
        return Err("no command to wrap; pass it after --".into());
    }

    // F2, before anything reads config, resolves an adapter, or touches the
    // terminal: an interactive supervisor started *inside* another agent's
    // session can take that outer session down (see
    // `sessions::nested_session_evidence`). Returned as an `Err` rather than
    // printed here, because `run_with` deliberately has no writer of its own
    // (see this function's doc comment); `ctx`'s dispatch prints it on
    // stderr through `output::error`.
    if let Some(refusal) = super::sessions::nesting_refusal("wrap", env, args.allow_nested) {
        return Err(refusal.into());
    }

    let cfg = CtxConfig::load_for_launch(repo, env)?;
    // Announcements are gated by `cfg.chrome.events` (which already folds in
    // `--quiet`, `ZIRV_CTX_QUIET` and `[chrome] events`), never by whether
    // the terminal is big enough or colour-capable for the banner and bar: a
    // piped stderr in CI still wants these lines. `--no-supervise` is the one
    // exception: it promises pure passthrough ("no scoring, no injection"),
    // so nothing about supervision has anything to narrate either.
    let announcer = if args.no_supervise {
        Announcer::silent()
    } else {
        Announcer::new(cfg.chrome.events, console::colors_enabled_stderr())
    };
    let agent_name = args.agent.as_deref().or(cfg.agent.as_deref());
    // Selection happens here so an unknown or unverified agent fails before the
    // terminal is touched.
    // T84: `adapter` is `mut` so a live `zirv ctx handover` swap (see the
    // handover request check inside `pump`) can replace the boxed trait
    // object in place -- every existing `adapter.<method>()` call site below
    // and inside `pump` keeps working unchanged, since method resolution
    // auto-derefs through `&mut Box<dyn AgentAdapter>` exactly as it does
    // through `&dyn AgentAdapter`.
    let mut adapter = adapters::select(agent_name, &args.command, &cfg)?;
    // The pinned model this wrapped launch actually spawns with, if the
    // wrapped command names one -- the same `last_model_flag` scan `exec`'s
    // own `execution_model` and `seat_model_env` use, read directly off
    // `args.command` rather than `cfg.chat.model`: unlike the seat-model env
    // guard (see `seat_cfg_model` below), a launch's provider bucketing may
    // honestly use the operator's configured chat model too, but here the
    // wrapped argv is already in hand and is the more direct source. Recomputed
    // wherever the pinned model can change (a harness handover) rather than
    // reused across the whole session.
    let launch_model = adapters::last_model_flag(&args.command);

    // `select` defaults to claude when detection finds nothing to back it,
    // which is fine for a caller (like `exec`) that already gates every
    // claude-specific behavior on `command_matches_adapter`. `wrap` must not
    // spawn a command it can only guess is claude and then start typing
    // claude-only escape sequences (`/exit\r`, `/compact ...`) into it: an
    // undetected command with no explicit `--agent` fails loudly here,
    // before the terminal is ever touched, instead of running silently
    // unsupervised.
    //
    // `--no-supervise` and `--simple` are exempt: both promise pure
    // passthrough, neither injects anything or types into the child, so there
    // is nothing left for a wrong guess to get wrong.
    let passthrough_only = args.no_supervise || args.simple;
    if !passthrough_only
        && agent_name.is_none()
        && !adapters::command_matches_adapter(adapter.as_ref(), false, &args.command)
    {
        let program = args.command.first().map(String::as_str).unwrap_or("");
        // Named generically rather than hardcoding one adapter: the actual
        // options come from the registry (gate-enabled and `ready()` right
        // now), so a second working adapter shows up here without an edit.
        let available = adapters::available_adapter_names(&cfg);
        let agent_hint = if available.is_empty() {
            "pass --agent <name>".to_string()
        } else {
            format!("pass --agent {}", available.join("/"))
        };
        return Err(format!(
            "zirv ctx wrap: could not tell which agent '{program}' is; {agent_hint} \
             (or your agent's name), run it with --no-supervise for pure passthrough, \
             or run this command unwrapped"
        )
        .into());
    }

    let state_dir = super::state::StateDir::resolve(env)?;
    let session = session.unwrap_or_else(super::event::SessionId::new_v4);

    // Issue jev-relay: unlike `exec`, `wrap`'s own `session` never gets
    // reminted mid-run (a harness handover keeps the same id, see
    // `relaunch`'s own doc comment), so the relay is started exactly once
    // here and held for this whole interactive supervisor's lifetime --
    // `_jev_relay_handle`'s drop (at `run_with`'s return, whichever arm)
    // stops it. `start` itself is a no-op `None` (no bind at all) whenever
    // no `[jev]` gate is on or there is no credential, and never blocks this
    // function's own startup: binding is fast local filesystem/pipe setup,
    // and the accept loop moves to its own thread before `start` returns.
    let _jev_relay_handle = jev_relay::start(
        &cfg.proxy.typesafe,
        jev::any_gate_enabled(&cfg.jev),
        &state_dir,
        session.as_str(),
    );

    // T10: the launch-time pacing gate -- before this fix, `wrap` (and, by
    // extension, `zirv ctx chat`'s orchestrator and every dashboard pane,
    // which launch through this same function) never consulted `pace` at
    // all, so an operator's dashboard-heavy workload had no proactive
    // protection whatsoever, only the reactive `scan_for_limit` catching a
    // vendor-imposed limit after the fact. Deliberately placed before any
    // pty/terminal work below (never on the redraw path, which stays
    // network-free per CLAUDE.md) and skipped outright for `--no-supervise`,
    // whose whole promise is "nothing supervisory happens" -- `--simple`
    // does NOT skip it (its own doc comment already promises "supervision,
    // pacing and hooks still apply").
    //
    // Also gated on both stdin *and* stdout being real terminals, the same
    // double-check `chrome::dash_eligible` already makes for the same
    // reason: this is the interactive-session launch path, and a
    // non-interactive `wrap` invocation is out of scope for it -- `exec`/
    // `loop` are the supervisors for headless work, and already gate
    // correctly. Issue #358 (T9): `apply_interactive_gate` itself no longer
    // blocks or prompts for either `Pause` or `Refuse` -- it prints the note
    // and launches -- so this check is no longer load-bearing for avoiding a
    // hang under piped test stdio, but the interactive/headless distinction
    // it draws is still the right one to gate on.
    //
    // `interactive_launch` is also the real signal `compile`/`policy_
    // launch_args` below need (2026-08-24 hardening): before this, both
    // hardcoded `LaunchMode::Interactive` regardless of whether stdio was
    // actually a terminal, so a non-tty `wrap` invocation (piped stdio, a
    // CI runner, a script) got the permissive interactive posture instead
    // of failing closed to `Headless`.
    let interactive_launch = std::io::stdin().is_terminal() && std::io::stdout().is_terminal();
    if !args.no_supervise && interactive_launch {
        let gate = pace::interactive_gate(
            &state_dir,
            &cfg,
            adapter.provider_for_model(launch_model),
            true,
        );
        apply_interactive_gate(gate, args.force_pace)?;
    }

    // `--no-supervise` promises pure passthrough (its own help text says so),
    // and so does a wrapped command that matches no adapter: injecting this
    // adapter's flags into a program that may not be it would leak them into
    // its output.
    let skip_injection = passthrough_only
        || !adapters::command_matches_adapter(
            adapter.as_ref(),
            agent_name.is_some(),
            &args.command,
        );
    // Still needed standalone below: `pump`'s own best-effort memory-harvest
    // call on a restart (unrelated to prompt composition, which `compile`
    // now owns) reads this same slug.
    let memory_slug = super::state::repo_slug(repo);
    // Issue #44: gathers memory, the derived harness roster and the
    // canonical `.zirv/context/` layer, and attaches the policy report;
    // issue #537 (T2a) folds the harness proxy's own bounded layer on top
    // when `chat::wrap_args_for` set one -- see `compiled_context_for_
    // launch`'s own doc comment.
    let compiled = compiled_context_for_launch(
        repo,
        skip_injection,
        &cfg,
        adapter.as_ref(),
        role,
        &state_dir,
        launch_mode_from_interactive(interactive_launch),
        args.proxy_layer.as_deref(),
    );
    // The wrapped command's own argv may already carry the adapter's
    // system-prompt flag; merge it in rather than letting `prompt_args` below
    // silently override it with a second occurrence.
    let (launch_command, mut composed) = super::prompt::merge_command_line_prompt(
        adapter.as_ref(),
        &args.command,
        compiled.composed,
        None,
        role,
        &cfg.prompt,
    );
    composed = super::obfuscate_store::protect_composed(
        &state_dir,
        repo,
        &cfg,
        composed,
        "wrap_orchestrator_prompt",
    )?;
    let prompt_args = super::prompt::injection_args_for_session(
        adapter.as_ref(),
        &launch_command,
        composed.as_ref(),
        &state_dir,
        session.as_str(),
    )?;
    super::prompt::log_injection(
        &state_dir,
        "wrap",
        session.as_str(),
        composed.as_ref(),
        adapter.system_prompt_supported(&launch_command),
    );
    announcer.emit(&super::prompt::injection_event(
        composed.as_ref(),
        adapter.system_prompt_supported(&launch_command),
    ));
    // Stripping the user's own --append-system-prompt (see
    // merge_command_line_prompt) can empty the argv even though args.command
    // itself was not empty at the top of this function, e.g. `wrap -- --
    // append-system-prompt foo` with nothing else. That must be an error, not
    // a panic on a hot path where release is panic = "abort".
    let (program, rest) = launch_command
        .split_first()
        .ok_or("no command to wrap; pass it after --")?;
    // Bug B (harness/model parity, 2026-08-22): the same seam every real
    // launch now calls (`adapters::policy_launch_args`) -- the shipped-
    // default "sandboxed, no prompts" posture plus any explicit `[policy]`
    // restriction. Computed once, from `rest` (the wrapped command's own
    // trailing argv, whether zirv-built via `chat.rs::build_launch` or a
    // hand-typed `zirv ctx wrap -- <command>`), and reused for both the
    // first launch and every restart below (`relaunch_extra`), exactly like
    // `prompt_args` already is. `flags_pin_policy` reads `rest`, so an
    // operator's own explicit `--sandbox`/`--ask-for-approval`/
    // `--permission-mode`/`--disallowedTools` still wins.
    //
    // Deliberately **not** gated on `skip_injection` (which also folds in
    // `args.simple`): `--simple` promises no *injected instruction text*,
    // and the sandbox posture is a safety flag layer, not instruction text
    // (see `chat.rs`'s own `--simple` test for the identical call). It is
    // gated on the two reasons `skip_injection` exists for that *do* apply
    // here: `--no-supervise`'s own contract is pure passthrough (nothing
    // zirv-added at all), and a wrapped command that does not actually
    // match this adapter must never receive this adapter's flags -- the
    // same leakage risk `skip_injection` exists to prevent for `prompt_
    // args`.
    let policy_skip = args.no_supervise
        || !adapters::command_matches_adapter(
            adapter.as_ref(),
            agent_name.is_some(),
            &args.command,
        );
    let policy_extra = if policy_skip {
        Vec::new()
    } else {
        adapters::policy_launch_args(
            &cfg,
            adapter.as_ref(),
            rest,
            launch_mode_from_interactive(interactive_launch),
            role,
        )
    };
    // Visible, not silent: the shipped-default posture (or the operator's
    // own opt-out/override) is announced once, here, at session start -- not
    // re-announced on a restart, since `policy_extra` is computed once above
    // and simply reused by `relaunch_extra`. A no-op under `--no-supervise`
    // (`announcer` is `Announcer::silent()` there already).
    announcer.emit(&super::announce::Event::SandboxPosture {
        detail: if policy_extra.is_empty() {
            "not applied (operator flags, an unmatched wrapped command, --no-supervise, or \
             [sandbox] enabled = false)"
                .to_string()
        } else {
            super::announce::posture_detail(&policy_extra)
        },
    });
    // Issue #420: heal any self-healable (`Outdated`) hook entry, then warn
    // at most once per 24h if something still drifted. Best-effort: no home
    // directory is not a reason to fail the launch. A no-op under
    // `--no-supervise` in effect too (`announcer` is `Announcer::silent()`
    // there), though the heal itself still runs -- fixing a drifted hook
    // entry is not "supervision".
    if let Ok(home) = crate::utils::home_dir() {
        let _ = super::hook_integrity::heal_outdated(&state_dir, &home);
        if let Some(summary) = super::hook_integrity::drift_warning_if_due(&state_dir, &home) {
            announcer.emit(&super::announce::Event::HookIntegrity { summary });
        }
    }
    // Issue #222: codex has no per-command approval mechanism zirv can
    // pre-clear the way #224 pre-approves reserved claude built-ins, so an
    // interactive launch under a prompting posture gets a one-time advisory
    // naming the config fix instead.
    if !policy_skip && interactive_launch && adapter.name() == "codex" {
        let posture = adapters::codex::resolve_codex_approval_posture(
            &crate::utils::home_dir().unwrap_or_else(|_| std::path::PathBuf::from(".")),
        );
        if let Some(advisory) = adapters::codex::codex_approval_advisory(posture) {
            announcer.emit(&super::announce::Event::CodexApprovalAdvisory { advisory });
        }
    }

    let mut supervision = InjectionState::new();
    supervision.degraded = args.no_supervise;

    let server = if args.no_supervise {
        None
    } else {
        match super::signal::SignalServer::bind(&state_dir.socket_for(session.as_str())) {
            Ok(server) => {
                // Published per session (F5): the pre-F5 global file meant a
                // second supervisor overwrote the first one's entry, and a
                // reader then found somebody else's socket under it.
                publish_socket_path(&state_dir, session.as_str(), server.path());
                Some(server)
            }
            Err(_) => {
                note_failure(
                    &mut supervision,
                    Some((&state_dir, session.as_str())),
                    "socket unavailable",
                    &announcer,
                );
                None
            }
        }
    };

    // Registered here, after the bind, rather than earlier: the record has to
    // say whether this session can act on a wake-up, and only the bind
    // result knows.
    //
    // N6/NEW-3: `wrap` claims nudge markers exclusively from its turn-signal
    // arm (`if let Some(server) = server && ...`), so with no socket --
    // `--no-supervise`, or a bind that failed -- a marker written for this
    // session is never claimed, and the nudge silently does nothing forever.
    // The first fix for that dropped such sessions from the registry
    // entirely, which cured the silent nudge by making the session
    // *invisible*: it disappeared from `zirv ctx status` too, so an operator
    // whose `wrap` had failed to bind could not see it running at all.
    // Recorded as `reachable: false` instead -- `status` shows it as
    // `unreachable`, and `nudge` refuses it with a reason.
    //
    // Keyed on the socket rather than on `--no-supervise`/`--simple` as
    // such: `--simple` only skips prompt injection, still binds a socket and
    // still claims markers, so it stays a legitimate (advisory) target; and
    // a bind failure under a plain `wrap` is exactly as unreachable as
    // `--no-supervise` is.
    //
    // Best-effort, released right after `pump` returns below -- `wrap`'s own
    // control flow always funnels through that one point, unlike `exec`'s
    // scattered early returns, so a single release suffices here.
    // Issue #139: recorded so `zirv ctx status` can compare this launch's
    // pinned policy against whatever the repo/operator layers resolve to
    // right now and surface a "policy snapshot stale" line when they
    // diverge -- see `sessions::Record::safety_policy_sha256`'s own doc
    // comment. `policy_fingerprint` is pure and deterministic, so this is
    // guaranteed to match whatever fingerprint `ClaudeAdapter::launch_
    // settings_path` embedded in this same launch's own settings file,
    // since both are computed from the identical `cfg.safety` value.
    let safety_policy_sha256 = super::safety::policy_fingerprint(&cfg.safety).ok();
    let record = super::sessions::Record::new(session.as_str(), adapter.name(), repo, verb)
        .with_safety_policy_sha256(safety_policy_sha256)
        .with_role(role.label());
    let record = if server.is_some() {
        record
    } else {
        record.unreachable()
    };
    let mut session_guard = super::sessions::SessionGuard::register(&state_dir, record);

    // Deliberately not derived from `session`: that id belongs to wrap, not to
    // the agent it spawns, so a derived path names a file nobody ever writes.
    let mut transcript = TranscriptSource::new(env(TRANSCRIPT_ENV).map(PathBuf::from));

    let (cols, rows) = window_size(STDIN_FD).unwrap_or(DEFAULT_SIZE);
    // Probed (and, on success, held) ahead of `RawGuard::enter` below: the
    // chrome eligibility decision (in particular whether the bar may draw at
    // all) needs to know this before the pty is even sized, and the bar's
    // own escape sequences need VT on regardless of whether the wrapped
    // command is itself claude or codex. Restored explicitly alongside
    // `raw`, at the one place this function ever leaves the pump.
    let mut vt_guard = super::term::enable_vt_output().ok();
    let vt_ok = vt_guard.is_some();
    // `IsTerminal` on stdout specifically, not `window_size(STDIN_FD)`'s own
    // success: on unix that probes stdin's own fd, so `zirv chat > log` (or
    // `wrap`) left stdin attached to a real terminal still banered straight
    // into the redirected file. The size itself still comes from
    // `window_size`, which is the only source `wrap` has for it.
    let stdout_is_tty = std::io::stdout().is_terminal();
    let chrome = super::chrome::ChromeCaps::probe(
        stdout_is_tty,
        vt_ok,
        (cols, rows),
        &cfg.chrome,
        args.simple,
        args.no_supervise,
    );
    let (pty_cols, pty_rows) = super::chrome::reserved_pty_size((cols, rows), chrome.bar);
    let mut pair = native_pty_system().openpty(PtySize {
        rows: pty_rows,
        cols: pty_cols,
        pixel_width: 0,
        pixel_height: 0,
    })?;

    // FIX 2a (command-injection defense): the first-launch pty assembly does
    // not pass through supervise::spawn_tapped's guard either, so apply the
    // same cmd.exe argv-reparse policy over the full downstream argv -- the
    // wrapped command's own args plus zirv's injected prompt args, which carry
    // repo-sourced text. A no-op off Windows and for any non-shim program.
    let mut child_args: Vec<String> = rest.to_vec();
    super::mcp::launch::append(
        &mut child_args,
        policy_extra
            .iter()
            .chain(prompt_args.iter())
            .cloned()
            .collect(),
    );
    if !policy_skip {
        let mcp_args = super::mcp::launch::arguments(
            adapter.name(),
            repo,
            &state_dir,
            session_guard.short(),
            &child_args,
        );
        super::mcp::launch::append(&mut child_args, mcp_args);
    }
    adapters::guard_cmd_shim_reparse(program, &child_args)?;
    let mut command = CommandBuilder::new(program);
    for arg in &child_args {
        command.arg(arg);
    }
    command.cwd(repo);

    // Kept for the relaunch too: a fresh session with no socket to report on
    // would leave the rest of the run unsupervised.
    // `AGENT_ENV` is exported unconditionally, unlike the turn-signal env
    // (which needs a bound socket): it names the same fact `ctx.toml`'s own
    // `agent` config key would, so a nested `zirv ctx ...` call inside this
    // session's own children defaults to this session's own harness. Kept in
    // `turn_env` (despite the name) because a relaunch reuses this exact
    // vector, and the freshly relaunched session needs it too.
    let mut turn_env: Vec<(String, String)> = server
        .as_ref()
        .map(|server| {
            adapter
                .register_turn_signal(
                    &super::event::SessionRef {
                        id: session.clone(),
                        cwd: repo.to_path_buf(),
                    },
                    server.path(),
                )
                .env
        })
        .unwrap_or_default();
    turn_env.push((adapters::AGENT_ENV.to_string(), adapter.name().to_string()));
    // Issue #147 amendment: the durable interactive-launch pin
    // (`adapters::LAUNCH_MODE_ENV`), set from the identical `interactive_
    // launch` signal `policy_extra` above already used to pick this
    // launch's `LaunchMode` -- never re-derived, so the two can never
    // disagree about whether this session is interactive. `None` for a
    // headless wrap: nothing is added, matching every other absent-signal
    // case the hook already fails closed on.
    if let Some((key, value)) =
        adapters::launch_mode_pin_env(launch_mode_from_interactive(interactive_launch))
    {
        turn_env.push((key, value));
    }
    // The seat this session sits in, for the `zirv ctx hook pretool` guard
    // running inside it. Orchestrator or (issue #537 T3) Single only, and
    // preferring an operator's own `--model`/`--model=` passthrough in `rest`
    // (the same flag vector this launch actually spawns with) over
    // `cfg.chat.model`; see `adapters::seat_model_env`. Kept in `turn_env`
    // for the same reason `AGENT_ENV` is: a relaunch reuses this exact
    // vector, and the fresh session sits in the same seat.
    //
    // Only a `chat` launch may fall back to `cfg.chat.model`: that is `chat`'s
    // own knob, spliced into the argv it hands us (`chat::extra_with_model`),
    // so for that caller the fallback and `rest` agree anyway. The bare `wrap`
    // verb never applies it, and became an Orchestrator (so it reaches this at
    // all) only once the role also picked its prompt layers -- claiming a
    // configured model this launch did not spawn with would have the guard
    // refuse dispatches at a tier the session is not actually on.
    let seat_cfg_model = match verb {
        super::sessions::Verb::Chat => cfg.chat.model.as_deref(),
        _ => None,
    };
    turn_env.extend(adapters::seat_model_env(role, rest, seat_cfg_model));
    // Issues #328/#334: which seat role this session runs as, for the same
    // guard -- unlike `seat_model_env`, unconditional for every role.
    turn_env.extend(adapters::seat_role_env(role));
    // Issue #753: a proxy-decided launch tells its hook not to re-run intake.
    if args.proxy_layer.is_some() {
        turn_env.push((adapters::PROXY_DECIDED_ENV.to_string(), "1".to_string()));
    }
    // Issue #358 (task 5): the logical orchestrator seat this session sits
    // in. Registered here rather than beside `SessionGuard::register` above
    // (where `seat::register`'s own wiring note points) for one reason: the
    // seat records which MODEL is answering at this address, and that is not
    // resolved until `seat_model_env` just above has run. Registration still
    // happens before the child exists, which is all the fencing generation
    // riding in `turn_env` below actually needs. Worker sessions register no
    // seat at all: nothing ever rolls one over.
    if role == PromptRole::Orchestrator {
        let seat_model = turn_env
            .iter()
            .find(|(key, _)| key == adapters::SEAT_MODEL_ENV)
            .map(|(_, value)| value.clone());
        match super::seat::register(
            &state_dir,
            &super::sessions::short_id(session.as_str()),
            session.as_str(),
            adapter.name(),
            seat_model.as_deref(),
            adapter.provider_for_model(seat_model.as_deref()),
            role.label(),
            super::seat::pin_from_env(env),
            super::state::now_secs(),
        ) {
            Ok(seat) => {
                // A supervisor that died mid-swap leaves the seat stuck in
                // `Prepared`, refusing every future rollover; recovery runs
                // before this launch's own generation is exported so the env
                // below cannot name a generation the recovery just moved.
                // `None` unconditionally: a supervisor is only running this
                // line because the previous one is gone, and a successor it
                // had prepared died with it. Nothing that outlived the crash
                // could still be answering at this address.
                let recovered = super::rollover::on_startup(&state_dir, &seat.short, &|_| None);
                turn_env.push(super::seat::generation_env(
                    recovered.as_ref().unwrap_or(&seat),
                ));
            }
            // Logged, never `note_failure`: a seat that could not be
            // registered costs this session automatic rollover and nothing
            // else, whereas degrading the supervisor would also silence
            // compaction, restarts and mail for a feature the operator may
            // not even have turned on.
            Err(e) => {
                let _ = super::log::append(
                    &state_dir,
                    &super::log::Decision {
                        ts: super::state::now_secs(),
                        session: session.as_str(),
                        verb: "wrap",
                        verdict: "rollover",
                        score: 0,
                        action: "orchestrator-seat-unregistered",
                        detail: &format!(
                            "seat registration failed, so this session cannot roll over \
                             automatically: {e}"
                        ),
                        observed_at: None,
                    },
                );
            }
        }
    }
    // Scrubbed before any of it is applied -- see `apply_session_env`. When
    // the bind above failed, `turn_env` carries only `AGENT_ENV`, and the
    // scrub is the only thing standing between this child and the outer
    // session's identity.
    apply_session_env(&mut command, &turn_env);

    // One writer, shared: the stdin pump and (from Task C4) the injector both
    // need it, and `take_writer` can only be called once. Its contents (not
    // the Arc itself) get swapped out on a restart, so every holder of this
    // Arc transparently starts writing to the fresh pty.
    //
    // Taken before the spawn rather than after it, because on Windows the
    // console host has to be answered before it will service the child at all
    // -- see `CURSOR_POSITION_REPORT`.
    let mut first_writer = pair.master.take_writer()?;
    answer_inherit_cursor_probe(&mut *first_writer);
    let writer = std::sync::Arc::new(std::sync::Mutex::new(first_writer));

    // Issue #330, and the last statement before the child exists: a Windows
    // priority class is inherited AT CREATION, so this is the only point at
    // which one call can still reach the whole tree this launch is about to
    // become. `role` is the same one that picked this session's prompt layers
    // -- both `wrap` and `chat` are Orchestrators, so in practice this raises
    // this supervisor's own threads and deliberately leaves the process class
    // (which the child, and any build the operator starts from this seat,
    // would inherit) exactly where it was. See `priority::Posture`.
    super::priority::apply_process(super::priority::posture_for(role));

    let mut child = pair.slave.spawn_command(command).map_err(|error| {
        format!(
            "adapter '{}': program '{}' failed to start: {}",
            adapter.name(),
            adapter.program(),
            error
        )
    })?;
    // P2/P3: adopted the instant the child exists -- registered for the
    // console-close sweep and put in a kill-on-close job, so neither closing
    // the window nor killing zirv outright can orphan the agent.
    let mut child_guard = super::supervise::ChildGuard::adopt(child.process_id());
    // P5: the registry record was filed above with `std::process::id()` --
    // zirv's own pid, which stays alive exactly as long as the wrapper rather
    // than as long as the agent. Point it at the child, the same override
    // `dash::pane::Pane::spawn` makes. `pump` re-points it after every
    // relaunch.
    if let Some(child_pid) = child.process_id() {
        session_guard.adopt_child_pid(child_pid);
    }
    // Issue #281: the launch prompt is baked into this spawn's own argv, not
    // typed as pty input, so it never reaches the `PumpEvent::Input` arm
    // `pump` stamps a turn's start from -- this is the only edge that
    // observes the FIRST turn beginning. Turn 1: the very first thing this
    // session does.
    session_guard.stamp_in_flight(verb.as_str(), 1);

    let reader = pair.master.try_clone_reader()?;
    let (tx, rx) = mpsc::channel::<PumpEvent>();
    // Bumped on every restart so a stale reader thread from an abandoned pty
    // never reports a false PtyClosed for the pty that replaced it.
    let generation = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));

    // Guards every write to the real stdout: the output thread's own
    // child-byte writes, and (T12b) the bar's assembled redraw buffer. One
    // `Mutex<()>` rather than wrapping `Stdout` itself, since both writers
    // already hold their own handle and only need to serialize *when* they
    // write, not share the handle.
    let stdout_lock = std::sync::Arc::new(std::sync::Mutex::new(()));

    // PTY to stdout.
    spawn_output_thread(
        reader,
        tx.clone(),
        generation.clone(),
        0,
        stdout_lock.clone(),
    );

    // Armed for the pty just opened, and re-armed by `pump` for every pty a
    // restart opens after it.
    let cpr_filter = std::sync::Arc::new(std::sync::Mutex::new(CprFilter::default()));
    cpr_filter
        .lock()
        .map_err(|_| "cpr filter poisoned")?
        .arm(Instant::now());

    // stdin to PTY. Accepted race (issue #118 follow-up): this drain has no
    // owed-CR flush of its own, unlike `Pane::write_operator_input`, so an
    // operator keystroke landing inside a deferred injection's narrow
    // pending window can merge into that not-yet-submitted advisory line.
    // Deliberately not fixed -- a deferred injection only ever starts once
    // the transcript is idle-quiet, which keeps the window narrow, and the
    // failure mode is a garbled advisory line, never a lost or misdirected
    // submit. This input hot path must stay failure-free, so no
    // cross-thread state is added here to guard against it.
    let input_tx = tx.clone();
    let input_writer = std::sync::Arc::clone(&writer);
    let input_filter = std::sync::Arc::clone(&cpr_filter);
    let input_stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let input_stop_for_thread = std::sync::Arc::clone(&input_stop);
    let input_thread = std::thread::spawn(move || {
        // Issue #330: the operator's own keystrokes travel on this thread and
        // nothing else does. Raised for the same reason (and with the same
        // per-thread caveat) as the output thread's own raise.
        super::priority::raise_current_thread();
        let mut buf = [0u8; 4096];
        let mut stdin = std::io::stdin();
        // #206. Owned by this thread rather than shared: a relaunch swaps the
        // pty behind `input_writer` but never restarts this pump, so a paste
        // being accumulated when the child is replaced is still delivered
        // whole, to the new pty.
        let mut paste = PasteGuard::default();
        loop {
            if input_stop_for_thread.load(std::sync::atomic::Ordering::Acquire) {
                return;
            }
            #[cfg(unix)]
            match stdin_ready() {
                Ok(true) => {}
                Ok(false) => continue,
                Err(_) => return,
            }
            if input_stop_for_thread.load(std::sync::atomic::Ordering::Acquire) {
                return;
            }
            match stdin.read(&mut buf) {
                Ok(0) | Err(_) => return,
                Ok(n) => {
                    let now = Instant::now();
                    // The console host's own probe was already answered
                    // synthetically; the terminal's duplicate answer must not
                    // reach the agent as keystrokes.
                    let filtered = {
                        let Ok(mut filter) = input_filter.lock() else {
                            return;
                        };
                        filter.filter(&buf[..n], now)
                    };
                    // #206: and a bracketed paste reaches the agent's composer
                    // as one write, so its newlines stay newlines instead of
                    // submitting a turn per line.
                    let bytes = paste.filter(filtered.as_ref(), now);
                    if bytes.is_empty() {
                        continue;
                    }
                    let Ok(mut sink) = input_writer.lock() else {
                        return;
                    };
                    if sink.write_all(&bytes).is_err() || sink.flush().is_err() {
                        return;
                    }
                    drop(sink);
                    if input_tx.send(PumpEvent::Input(bytes.len())).is_err() {
                        return;
                    }
                }
            }
        }
    });

    // Raw mode is best-effort: without a terminal (a pipe, or CI) the wrapper
    // still passes bytes through.
    let mut raw = RawGuard::enter(STDIN_FD).ok();

    let mut bar = BarRuntime::new(
        chrome,
        adapter.name().to_string(),
        adapter
            .provider_for_model(seat_model_from_turn_env(&turn_env))
            .to_string(),
        super::sessions::short_id(session.as_str()),
        cfg.mail.enabled,
        stdout_lock.clone(),
        (cols, rows),
        cfg.pace.collector_max_age_secs,
    );
    // C5: `raw.is_some()` as well as `bar.chrome.bar`. `RawGuard::enter` is
    // what stashes the console modes and installs the emergency restore
    // handler (F4), so when it failed there is nothing armed to undo a
    // scroll region -- writing one anyway would fence off the terminal's
    // last row with no handler able to put it back, which is strictly worse
    // than having no bar. A failed `enter` also means this is not a real
    // terminal in the first place, so the bar has nothing to draw on.
    if bar.chrome.bar && raw.is_some() {
        let region = super::chrome::scroll_region_sequence(bar.rows);
        let region_ok = match stdout_lock.lock() {
            Ok(_guard) => {
                let mut stdout = std::io::stdout();
                stdout
                    .write_all(region.as_bytes())
                    .and_then(|()| stdout.flush())
                    .is_ok()
            }
            Err(_) => false,
        };
        // Same degrade-on-failure as every other bar write: a scroll region
        // that never actually got set must not leave the bar thinking it is
        // still safely confining the child.
        bar.disabled = super::chrome::after_redraw_attempt(bar.disabled, region_ok);
        // F4: from here the real console has a scroll region fencing off its
        // last row, so an external kill owes it a `CSI r` on the way out.
        // Only when the write actually landed -- a region that was never set
        // needs no reset.
        super::term::set_bar_active(region_ok);
    }

    let debounce = Duration::from_millis(cfg.wrap.debounce_ms);
    let inject_timeout = Duration::from_millis(cfg.wrap.inject_timeout_ms);

    // Carried into `relaunch_command` too, so a restart does not silently drop
    // the injected prompt the first command already carries.
    //
    // Not the raw argv: a restart is a deliberate escape from the conversation
    // that rotted, so anything pinning the launch to it (`--continue`,
    // `--resume <id>`, `--session-id <id>`, `--fork-session`, and their
    // `=`-bound spellings) has to go, or `wrap -- claude --continue` relaunches
    // straight back into the session it was leaving and burns the restart
    // budget doing it. `exec` already worked this out; this is that same
    // function, and the positional prompt it also strips is one `relaunch`
    // regenerates from the handoff anyway.
    let relaunch_extra: Vec<String> = policy_extra
        .iter()
        .cloned()
        .chain(restart_launch_flags(adapter.as_ref(), &launch_command))
        .chain(prompt_args.iter().cloned())
        .collect();

    // T84: owned rather than borrowed from `cfg`/`adapter` at the call site,
    // so a live handover swap inside `pump` can update it in place for
    // whatever the *new* adapter's own distiller default is -- otherwise a
    // rot-triggered restart after a handover would keep quoting the
    // predecessor's model name to a distiller that may not even recognise it.
    let mut distiller_model =
        handoff::resolve_distiller_model(cfg.handoff.model.as_deref(), adapter.as_ref());
    // Issue #249: this session's own supervising session, if any -- resolved
    // once, from `env` alone, and passed straight through to every poll.
    let parent_short = super::agent::parent_identity(env);
    let mut native_successor = None;
    let exit = pump(
        &mut child,
        &mut child_guard,
        &mut session_guard,
        &rx,
        &mut pair,
        &mut supervision,
        server.as_ref(),
        &mut adapter,
        &writer,
        &mut transcript,
        &state_dir,
        &session,
        debounce,
        inject_timeout,
        repo,
        env,
        cfg.handoff.tail_items,
        &mut distiller_model,
        Duration::from_secs(cfg.handoff.timeout_secs),
        &cfg,
        &memory_slug,
        QUIT_GRACE,
        tx,
        generation,
        &relaunch_extra,
        &mut turn_env,
        &cpr_filter,
        &announcer,
        &mut bar,
        role,
        parent_short.as_deref(),
        &mut native_successor,
    );
    input_stop.store(true, std::sync::atomic::Ordering::Release);
    #[cfg(unix)]
    let _ = input_thread.join();
    #[cfg(not(unix))]
    drop(input_thread);
    // P2/P3: `pump` only ever returns once the child has exited (every arm
    // waits on it), so the pid leaves the console-close registry and the job
    // handle closes here, explicitly -- `panic = "abort"` means `Drop` is no
    // safety net. Before `session_guard.release()` purely for symmetry with
    // the order they were taken in.
    child_guard.release();
    if native_successor.is_some() {
        session_guard.disown();
    } else {
        session_guard.release();
    }
    // Paired with `publish_socket_path` above, at the same single point every
    // other per-session artifact is released: a dead supervisor's file must
    // not linger to be picked as "the newest" by a later `read_socket_path`
    // that has no session id of its own.
    unpublish_socket_path(&state_dir, session.as_str());
    // Issue #358: the seat is an address for a live session, and this one is
    // over. Released at the same single point as every other per-session
    // artifact so a dead seat record can never be read as a live one.
    if role == PromptRole::Orchestrator && native_successor.is_none() {
        super::rollover::forget(&state_dir, &super::sessions::short_id(session.as_str()));
    }

    reset_bar(&bar);
    if let Some(guard) = raw.as_mut() {
        let _ = guard.restore();
    }

    if let Some(successor) = native_successor {
        let dashboard = super::dash::run_dashboard_with_first_pane(
            &cfg, repo, env, &state_dir, successor, false,
        );
        if let Some(guard) = vt_guard.as_mut() {
            let _ = guard.restore();
        }
        return dashboard;
    }
    if let Some(guard) = vt_guard.as_mut() {
        let _ = guard.restore();
    }

    match exit {
        Ok(code) => Ok(code),
        Err(e) => {
            // Item 6 audit: `w` is stdout in production, the same stream the
            // wrapped session's own pty bytes are already occupying -- a
            // rare internal pump failure (a `try_wait`/`wait` I/O error) used
            // to print its only diagnostic there, where it could be scrolled
            // off, overwritten by the child's own next redraw, or -- for
            // `zirv chat > log` -- land only in a redirected file instead of
            // the operator's own terminal. `output::error` matches
            // `output::error`'s own stream and styling, the same fix as
            // chat.rs's no-adapter diagnostic (item 1).
            crate::output::error(format!("zirv ctx wrap: {e}"));
            Ok(1)
        }
    }
}

#[cfg(test)]
mod tests {
    #[cfg(unix)]
    use super::super::tests::*;
    use super::*;
    // F2: the nesting guard.

    /// An env map for the guard tests, always pinning `ZIRV_CTX_AGENT_BIN`
    /// at a path that cannot exist.
    ///
    /// Safety belt, not a fixture detail. `adapters::select` calls `ready()`,
    /// so an unusable agent_bin makes a launch structurally impossible: if
    /// the guard under test ever regresses, these tests fail on a missing
    /// binary instead of opening a pty and spawning a real nested agent into
    /// whatever session the suite is running in.
    fn nested_env(pairs: &[(&str, &str)]) -> std::collections::HashMap<String, String> {
        let mut env: std::collections::HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect();
        env.insert(
            "ZIRV_CTX_AGENT_BIN".to_string(),
            "/nonexistent/agent-must-never-launch".to_string(),
        );
        env
    }

    fn wrap_args_in(command: &[&str], allow_nested: bool) -> WrapArgs {
        WrapArgs {
            agent: Some("claude".to_string()),
            no_supervise: false,
            command: command.iter().map(|s| (*s).to_string()).collect(),
            simple: false,
            allow_nested,
            force_pace: false,
            proxy_layer: None,
        }
    }

    #[test]
    fn wrap_refuses_to_start_inside_a_supervised_session_and_names_the_evidence() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let env = nested_env(&[(
            adapters::SESSION_ENV,
            "abcdef12-3456-4789-8abc-def012345678",
        )]);

        let err = run_with(
            &wrap_args_in(&["claude"], false),
            tmp.path(),
            &|k| env.get(k).cloned(),
            PromptRole::Worker,
            None,
            super::super::sessions::Verb::Wrap,
        )
        .expect_err("a wrap inside a supervised session must refuse");

        let msg = err.to_string();
        assert!(
            msg.contains("refusing to start inside an existing agent session"),
            "got {msg}"
        );
        assert!(
            msg.contains("abcdef12"),
            "names the outer session it found: {msg}"
        );
        assert!(
            msg.contains("--allow-nested"),
            "says how to override: {msg}"
        );
    }

    /// The Claude Code marker pair is the second, independent source of
    /// evidence: a `zirv chat` typed into a Claude Code session's own shell
    /// exports no `ZIRV_CTX_*` at all when that session was started outside
    /// zirv, but it is still nested.
    #[test]
    fn wrap_refuses_inside_a_claude_code_session_too() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let env = nested_env(&[("CLAUDE_PID", "4242"), ("CLAUDECODE", "1")]);

        let err = run_with(
            &wrap_args_in(&["claude"], false),
            tmp.path(),
            &|k| env.get(k).cloned(),
            PromptRole::Worker,
            None,
            super::super::sessions::Verb::Wrap,
        )
        .expect_err("refuses");
        assert!(err.to_string().contains("Claude Code"), "got {err}");
    }

    /// With the override on, the guard is out of the way and the run reaches
    /// the *next* refusal in `run_with` -- the undetected-command one. That
    /// specific later error is the evidence the guard was passed, without
    /// this test ever having to spawn a pty or an agent.
    #[test]
    fn allow_nested_overrides_the_guard() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(tmp.path());
        let env: std::collections::HashMap<String, String> = [(
            adapters::SESSION_ENV.to_string(),
            "abcdef12-3456-4789-8abc-def012345678".to_string(),
        )]
        .into();
        let mut args = wrap_args_in(&["echo", "hello"], true);
        args.agent = None;

        let err = run_with(
            &args,
            tmp.path(),
            &|k| env.get(k).cloned(),
            PromptRole::Worker,
            None,
            super::super::sessions::Verb::Wrap,
        )
        .expect_err("gets past the nesting guard, then fails on the undetected command");
        let msg = err.to_string();
        assert!(
            !msg.contains("refusing to start inside"),
            "the guard was overridden: {msg}"
        );
        assert!(msg.contains("could not tell which agent"), "got {msg}");
    }

    /// And the environment variable is the second, equivalent override, for
    /// an operator who cannot reach the command line (a wrapper script, a
    /// CI job).
    #[test]
    fn the_allow_nested_environment_variable_overrides_the_guard_too() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(tmp.path());
        let env: std::collections::HashMap<String, String> = [
            (
                adapters::SESSION_ENV.to_string(),
                "abcdef12-3456-4789-8abc-def012345678".to_string(),
            ),
            (
                super::super::sessions::ALLOW_NESTED_ENV.to_string(),
                "true".to_string(),
            ),
        ]
        .into();
        let mut args = wrap_args_in(&["echo", "hello"], false);
        args.agent = None;

        let err = run_with(
            &args,
            tmp.path(),
            &|k| env.get(k).cloned(),
            PromptRole::Worker,
            None,
            super::super::sessions::Verb::Wrap,
        )
        .expect_err("past the guard, onto the undetected command");
        assert!(
            !err.to_string().contains("refusing to start inside"),
            "got {err}"
        );
    }

    /// A plain terminal is not nested, and nothing about the guard changes
    /// what a normal run does: the same undetected-command refusal, reached
    /// the same way.
    #[test]
    fn a_wrap_outside_any_session_is_not_gated() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(tmp.path());
        let mut args = wrap_args_in(&["echo", "hello"], false);
        args.agent = None;
        let err = run_with(
            &args,
            tmp.path(),
            &|_| None,
            PromptRole::Worker,
            None,
            super::super::sessions::Verb::Wrap,
        )
        .expect_err("echo matches no adapter");
        assert!(
            !err.to_string().contains("refusing to start inside"),
            "got {err}"
        );
    }

    /// Finding #1: `wrap` launches/supervises a harness, so a syntax error
    /// in the operator's own HOME `ctx.toml` must refuse outright (via
    /// `CtxConfig::load_for_launch`) rather than silently degrading to
    /// permissive pacing/policy/sandbox defaults right before a harness
    /// spawns. Reached before the (later, weaker) undetected-command
    /// refusal `a_wrap_outside_any_session_is_not_gated` exercises, proving
    /// the config load itself is what fails here.
    #[test]
    fn a_home_layer_syntax_error_refuses_to_launch_naming_the_file() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let home = tmp.path().join("home");
        std::fs::create_dir_all(home.join(".zirv")).expect("mkdir home");
        std::fs::write(home.join(".zirv/ctx.toml"), "[score\n").expect("write broken home layer");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);

        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(&repo).expect("mkdir repo");
        let mut args = wrap_args_in(&["echo", "hello"], false);
        args.agent = None;

        let err = run_with(
            &args,
            &repo,
            &|_| None,
            PromptRole::Worker,
            None,
            super::super::sessions::Verb::Wrap,
        )
        .expect_err("a broken home layer must refuse to launch");
        let msg = err.to_string();
        assert!(
            msg.contains(&home.join(".zirv").join("ctx.toml").display().to_string()),
            "names the broken file: {msg}"
        );
    }

    // F3: a child never inherits another session's identity.

    /// The bind-failure case, which is the one that actually bit: `wrap`
    /// binds no socket, so it has no turn-signal env of its own to set, and
    /// every `ZIRV_CTX_*` the child sees would otherwise be the *outer*
    /// session's -- inherited straight out of this process's environment,
    /// because `CommandBuilder::new` seeds itself from `std::env::vars_os`.
    #[test]
    fn a_child_never_inherits_the_outer_sessions_socket_or_id() {
        let _outer = crate::commands::ctx::testenv::VarGuard::set(&[
            (
                adapters::SESSION_ENV,
                Some("outer111-2222-4333-8444-555555555555"),
            ),
            (adapters::SOCKET_ENV, Some("/tmp/outer.sock")),
            (TRANSCRIPT_ENV, Some("/tmp/outer.jsonl")),
            // F4: a headless worker under a nested interactive relaunch (or
            // an operator shell that inherited it) must never leave this
            // interactive child looking headless too.
            (adapters::HEADLESS_ENV, Some("1")),
        ]);

        // Sanity, and the reason the scrub has to be explicit: an untouched
        // builder really does carry the outer session's values.
        let inherited = CommandBuilder::new("echo");
        assert_eq!(
            inherited
                .get_env(adapters::SESSION_ENV)
                .and_then(|v| v.to_str()),
            Some("outer111-2222-4333-8444-555555555555"),
            "sanity: portable-pty seeds a builder from the process environment"
        );

        // No socket of its own: every supervision variable must be *absent*,
        // not inherited.
        let mut no_socket = CommandBuilder::new("echo");
        apply_session_env(&mut no_socket, &[]);
        for key in super::super::sessions::SUPERVISION_ENV {
            assert_eq!(
                no_socket.get_env(key),
                None,
                "{key} must not reach a child of an unsupervised wrap"
            );
        }

        // And with a socket of its own, only its own values reach the child.
        let mut supervised = CommandBuilder::new("echo");
        apply_session_env(
            &mut supervised,
            &[(
                adapters::SESSION_ENV.to_string(),
                "inner999-2222-4333-8444-555555555555".to_string(),
            )],
        );
        assert_eq!(
            supervised
                .get_env(adapters::SESSION_ENV)
                .and_then(|v| v.to_str()),
            Some("inner999-2222-4333-8444-555555555555"),
        );
        assert_eq!(
            supervised.get_env(adapters::SOCKET_ENV),
            None,
            "the outer socket must not survive alongside the inner id"
        );
        assert_eq!(supervised.get_env(TRANSCRIPT_ENV), None);
    }

    // F5: one published socket path per session.

    #[test]
    fn two_concurrent_wraps_do_not_clobber_each_others_socket_path() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().join("state"));
        let one = "aaaa1111-2222-4333-8444-555555555555";
        let two = "bbbb2222-2222-4333-8444-555555555555";

        publish_socket_path(&state, one, Path::new("/tmp/one.sock"));
        publish_socket_path(&state, two, Path::new("/tmp/two.sock"));

        assert_eq!(
            read_socket_path(&state, Some(one)).as_deref(),
            Some("/tmp/one.sock"),
            "the second launch must not have overwritten the first"
        );
        assert_eq!(
            read_socket_path(&state, Some(two)).as_deref(),
            Some("/tmp/two.sock")
        );
        assert!(
            !state.root().join(SOCKET_PATH_FILE).exists(),
            "the clobber-prone global file is never written any more"
        );

        // And releasing one leaves the other reachable.
        unpublish_socket_path(&state, one);
        assert_eq!(read_socket_path(&state, Some(one)), None);
        assert_eq!(
            read_socket_path(&state, Some(two)).as_deref(),
            Some("/tmp/two.sock")
        );
    }

    #[test]
    fn a_reader_resolves_its_own_sessions_socket_path() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().join("state"));
        let mine = "cccc3333-2222-4333-8444-555555555555";
        let theirs = "dddd4444-2222-4333-8444-555555555555";

        publish_socket_path(&state, mine, Path::new("/tmp/mine.sock"));
        publish_socket_path(&state, theirs, Path::new("/tmp/theirs.sock"));

        assert_eq!(
            read_socket_path(&state, Some(mine)).as_deref(),
            Some("/tmp/mine.sock"),
            "a reader that knows its own session id never gets somebody else's socket"
        );
        // A short id resolves the same file the full session id does.
        assert_eq!(
            read_socket_path(&state, Some("cccc3333")).as_deref(),
            Some("/tmp/mine.sock")
        );
        // With no session to go on, *some* published socket is the best
        // honest answer -- but only one of the two real ones, never a mix.
        let anonymous = read_socket_path(&state, None).expect("a published socket");
        assert!(
            anonymous == "/tmp/mine.sock" || anonymous == "/tmp/theirs.sock",
            "got {anonymous}"
        );
    }

    /// Backward tolerance: a supervisor from a pre-F5 build published the
    /// global file and nothing else. A reader must still find it.
    #[test]
    fn a_reader_still_falls_back_to_the_legacy_global_socket_path_file() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().join("state"));
        super::super::state::create_private_dir_all(state.root()).expect("mkdir");
        std::fs::write(state.root().join(SOCKET_PATH_FILE), "/tmp/legacy.sock").expect("write");

        assert_eq!(
            read_socket_path(&state, None).as_deref(),
            Some("/tmp/legacy.sock")
        );
        assert_eq!(
            read_socket_path(&state, Some("cccc3333")).as_deref(),
            Some("/tmp/legacy.sock"),
            "a session with no file of its own still falls back"
        );

        // A per-session file wins over the legacy one for its own session.
        publish_socket_path(
            &state,
            "cccc3333-2222-4333-8444-555555555555",
            Path::new("/tmp/mine.sock"),
        );
        assert_eq!(
            read_socket_path(&state, Some("cccc3333")).as_deref(),
            Some("/tmp/mine.sock")
        );
    }

    #[test]
    fn reading_a_state_dir_with_nothing_published_is_none_rather_than_an_error() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().join("state"));
        assert_eq!(read_socket_path(&state, None), None);
        assert_eq!(read_socket_path(&state, Some("cccc3333")), None);
    }

    /// Issue #358 (T9): `apply_interactive_gate` never waits or confirms any
    /// more, regardless of `force_pace` -- a `Pause` with an astronomical
    /// `seconds` and a `Refuse` both return `Ok(())` immediately whether or
    /// not `--force-pace` was passed, since usage headroom no longer blocks
    /// or delays a launch. Renamed from `force_pace_skips_the_wait_or_
    /// confirmation_for_both_pause_and_refuse`, which used to pin `--force-
    /// pace` as the one thing standing between an operator and an ~11-day
    /// block; there is no block left to skip.
    #[test]
    fn neither_pause_nor_refuse_ever_waits_or_confirms_with_or_without_force_pace() {
        assert!(apply_interactive_gate(pace::InteractiveGate::Launch, false).is_ok());
        assert!(apply_interactive_gate(pace::InteractiveGate::Launch, true).is_ok());

        for force_pace in [false, true] {
            assert!(
                apply_interactive_gate(
                    pace::InteractiveGate::Pause {
                        message: "usage 85.0% of the five_hour window".to_string(),
                        seconds: 999_999,
                    },
                    force_pace,
                )
                .is_ok(),
                "a pause that would otherwise wait ~11 days must never block, force_pace={force_pace}"
            );

            assert!(
                apply_interactive_gate(
                    pace::InteractiveGate::Refuse {
                        message: "usage 99.9% of the five_hour window".to_string(),
                    },
                    force_pace,
                )
                .is_ok(),
                "the hard ceiling must launch anyway, force_pace={force_pace}"
            );
        }
    }

    #[test]
    fn wrap_needs_a_command() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let args = WrapArgs {
            agent: None,
            no_supervise: false,
            command: Vec::new(),
            simple: false,
            allow_nested: false,
            force_pace: false,
            proxy_layer: None,
        };
        let err = run_with(
            &args,
            tmp.path(),
            &|_| None,
            PromptRole::Worker,
            None,
            super::super::sessions::Verb::Wrap,
        )
        .expect_err("nothing to wrap");
        assert!(err.to_string().contains("command"), "got {err}");
    }

    /// M5's undetected-command refusal used to hardcode "pass --agent
    /// claude", which only ever named one adapter no matter how many the
    /// registry actually holds. The error must instead name whatever the
    /// registry currently reports as available (gate-enabled and `ready()`),
    /// so a second working adapter shows up here without an edit to this
    /// string.
    #[test]
    fn the_undetected_command_error_names_the_registry_rather_than_claude() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(tmp.path());
        let args = WrapArgs {
            agent: None,
            no_supervise: false,
            command: vec!["echo".to_string(), "hello".to_string()],
            simple: false,
            allow_nested: false,
            force_pace: false,
            proxy_layer: None,
        };
        let err = run_with(
            &args,
            tmp.path(),
            &|_| None,
            PromptRole::Worker,
            None,
            super::super::sessions::Verb::Wrap,
        )
        .expect_err("echo matches no adapter");
        let msg = err.to_string();
        assert!(
            msg.contains("--agent"),
            "still tells the user how to fix it: {msg}"
        );
        for name in adapters::available_adapter_names(&CtxConfig::default()) {
            assert!(
                msg.contains(name),
                "must name available adapter '{name}': {msg}"
            );
        }
    }

    /// N1: `merge_command_line_prompt` strips the user's own
    /// `--append-system-prompt` and its value out of the passthrough argv.
    /// When that flag pair was the *entire* wrapped command, the argv is
    /// empty after the merge even though `args.command` itself was not empty
    /// at the top of `run_with`. This must be a returned error, not a panic:
    /// release is `panic = "abort"` and this is a supervisor hot path.
    /// `--agent claude` is explicit here so the adapter-match gate does not
    /// also suppress composition: the wrapped "command" is a bare flag pair,
    /// which detection would never recognize as any adapter's own binary.
    #[test]
    fn a_prompt_flag_that_empties_the_argv_after_merging_is_an_error_not_a_panic() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let args = WrapArgs {
            agent: Some("claude".to_string()),
            no_supervise: false,
            command: vec!["--append-system-prompt".to_string(), "foo".to_string()],
            simple: false,
            allow_nested: false,
            force_pace: false,
            proxy_layer: None,
        };
        let err = run_with(
            &args,
            tmp.path(),
            &|_| None,
            PromptRole::Worker,
            None,
            super::super::sessions::Verb::Wrap,
        )
        .expect_err("nothing left to wrap");
        assert!(err.to_string().contains("command"), "got {err}");
    }

    #[cfg(unix)]
    #[test]
    fn the_wrapped_program_output_reaches_the_terminal() {
        let script = fixture("stub-tui.sh").display().to_string();
        let mut h = spawn_wrap(&[], &["sh", &script]);

        let seen = read_until(&mut h.reader, "stub-tui ready", Duration::from_secs(10));
        assert!(seen.contains("stub-tui ready"), "got: {seen:?}");

        h.writer.write_all(b"/exit\r").expect("write");
        h.writer.flush().expect("flush");
        let _ = read_until(&mut h.reader, "bye", Duration::from_secs(10));
        let status = h.child.wait().expect("wait");
        assert_eq!(status.exit_code(), 0);
    }

    /// I2: a user's own --append-system-prompt inside the wrapped command must
    /// not be silently discarded by zirv's own occurrence of the same flag.
    #[cfg(unix)]
    #[test]
    fn a_users_own_append_system_prompt_is_merged_not_dropped() {
        // A real state dir (with no override, this test used to run against
        // one) is not test-isolated, and on macOS its default path contains
        // a space ("Application Support"), which breaks whitespace-based
        // parsing of a prompt-file path out of the stub's echoed argv below.
        // A tempdir sidesteps both.
        let state = tempfile::tempdir().expect("tempdir");
        // I2 regression (2026-08-23): this test asserts the exact composed
        // prompt text, so it cannot run with the wrapped child's cwd pinned
        // to this checkout -- the checkout now carries its own committed
        // `.zirv/context/*.md` and `.zirv/memory/*.md` (the zirv-managed
        // context migration), which would merge into the composed prompt on
        // top of the built-in default and the user's own flag under test.
        // No mail is involved here, so there is no need for the spawned
        // child's cwd to match this process's cwd for slug agreement --
        // point it at an isolated, unrelated temp dir instead.
        let repo = tempfile::tempdir().expect("tempdir");
        let script = fixture("stub-tui.sh").display().to_string();
        let mut h = spawn_wrap_with_flags_in(
            repo.path(),
            &[("ZIRV_CTX_STATE_DIR", state.path().display().to_string())],
            &["--agent", "claude"],
            &[
                "sh",
                &script,
                "--append-system-prompt",
                "always answer in Danish",
            ],
        );

        let seen = read_until(&mut h.reader, "stub-tui ready", Duration::from_secs(10));
        assert_eq!(
            seen.matches("--append-system-prompt").count(),
            1,
            "exactly one flag must reach the wrapped agent: {seen:?}"
        );

        // M7: the composed prompt travels either on argv
        // (--append-system-prompt <text>) or, when the configured agent
        // binary supports it, in a private file referenced by
        // --append-system-prompt-file <path>. The invariant under test is
        // that the user's own instruction survives the merge, not which
        // mechanism carried it, so read whichever one actually fired.
        let carried_text = match flag_value(&seen, "--append-system-prompt-file") {
            Some(path) => {
                std::fs::read_to_string(path).expect("prompt file referenced on argv is readable")
            }
            None => seen.clone(),
        };
        assert!(
            carried_text.contains("always answer in Danish"),
            "the user's own instruction must survive: {carried_text:?}"
        );
        assert!(
            carried_text.contains("zirv engineering standard"),
            "zirv's own layer is still present: {carried_text:?}"
        );

        h.writer.write_all(b"/exit\r").expect("write");
        h.writer.flush().expect("flush");
        let _ = read_until(&mut h.reader, "bye", Duration::from_secs(10));
        let status = h.child.wait().expect("wait");
        assert_eq!(status.exit_code(), 0);
    }

    /// Bug B (harness/model parity, 2026-08-22): the shipped-default
    /// sandbox posture reaches a plain (non-dashboard) `wrap` launch too,
    /// not only the seams a human never watches. Not
    /// runnable on this Windows dev machine (`#[cfg(unix)]`, mirroring
    /// every neighbouring live-argv test in this module); intended for CI.
    #[cfg(unix)]
    #[test]
    fn a_supervised_wrap_carries_the_shipped_sandbox_posture_for_codex() {
        let script = fixture("stub-tui.sh").display().to_string();
        // This is the one test in the suite the shipped sandbox posture must
        // actually reach, so it opts back in over the harness's own default
        // (see `spawn_wrap_with_flags`'s `ZIRV_CTX_SANDBOX=false`) rather than
        // relying on whatever `[sandbox]` happens to default to.
        let mut h = spawn_wrap_with_flags(
            &[("ZIRV_CTX_SANDBOX", "true".to_string())],
            &["--agent", "codex"],
            &["sh", &script],
        );

        let seen = read_until(&mut h.reader, "stub-tui ready", Duration::from_secs(10));
        // Inspect actual leading argv, excluding the developer prompt, which
        // can itself mention policy flags. The automatic-review preset is
        // mutually exclusive with explicit sandbox and approval flags (#710).
        let argv = seen.split_once("argv: ").expect("stub argv").1;
        let posture = argv.split(" -c ").next().unwrap_or(argv);
        if posture.contains("--approve-for-me") {
            assert!(!posture.contains("--sandbox"), "{posture}");
            assert!(!posture.contains("--ask-for-approval"), "{posture}");
        } else {
            assert!(posture.contains("--sandbox workspace-write"), "{posture}");
            assert!(
                posture.contains("--ask-for-approval never")
                    || posture.contains("--ask-for-approval on-request"),
                "{posture}"
            );
        }

        h.writer.write_all(b"/exit\r").expect("write");
        h.writer.flush().expect("flush");
        let _ = read_until(&mut h.reader, "bye", Duration::from_secs(10));
        let status = h.child.wait().expect("wait");
        assert_eq!(status.exit_code(), 0);
    }

    /// Bug (2026-08-02 validation of 2.5.0): `--no-supervise`'s own help text
    /// promises "no scoring, no injection", but it only turned off scoring;
    /// the system prompt was still composed and injected. `--no-supervise`
    /// must skip injection exactly like `--simple` does, including leaving a
    /// user's own `--append-system-prompt` untouched (nothing left to merge
    /// it into once nothing is composed).
    #[cfg(unix)]
    #[test]
    fn no_supervise_injects_nothing_and_leaves_the_users_own_flag_untouched() {
        let script = fixture("stub-tui.sh").display().to_string();
        let mut h = spawn_wrap_with_flags(
            &[],
            &["--agent", "claude", "--no-supervise"],
            &[
                "sh",
                &script,
                "--append-system-prompt",
                "always answer in Danish",
            ],
        );

        let seen = read_until(&mut h.reader, "stub-tui ready", Duration::from_secs(10));
        assert_eq!(
            seen.matches("--append-system-prompt").count(),
            1,
            "the user's own flag must pass through untouched: {seen:?}"
        );
        assert!(
            seen.contains("always answer in Danish"),
            "the user's own instruction is not stripped: {seen:?}"
        );
        assert!(
            !seen.contains("zirv engineering standard"),
            "no-supervise is pure passthrough, no injection: {seen:?}"
        );

        h.writer.write_all(b"/exit\r").expect("write");
        h.writer.flush().expect("flush");
        let _ = read_until(&mut h.reader, "bye", Duration::from_secs(10));
        let _ = h.child.wait();
    }

    /// M5: with no `--agent` given, `adapters::select` falls back to
    /// `ClaudeAdapter` when detection finds no match at all. Silently running
    /// the wrapped command anyway (even with injection suppressed) means
    /// `wrap` guesses an agent it cannot back up and can still start typing
    /// claude-only escape sequences into a foreign program. `echo` is not
    /// claude or codex, so wrap must refuse with an actionable error instead
    /// of running it unsupervised.
    #[cfg(unix)]
    #[test]
    fn wrapping_an_undetected_command_with_no_explicit_agent_is_a_clear_error() {
        let mut h = spawn_wrap_with_flags(&[], &[], &["echo", "hello"]);

        // Read before waiting: once the child (the pty's only other side) has
        // exited and every slave-side reference is closed, this platform can
        // drop whatever was still buffered in the master's queue rather than
        // let it be read afterwards (the same teardown quirk documented on
        // `wait_or_kill` above).
        let seen = read_until(&mut h.reader, "--agent", Duration::from_secs(5));
        assert!(
            seen.contains("--agent"),
            "the error must say how to fix it: {seen:?}"
        );
        assert!(
            !seen.contains("hello"),
            "the wrapped command must never have run: {seen:?}"
        );

        let status = h.child.wait().expect("wait");
        assert_ne!(
            status.exit_code(),
            0,
            "an unresolvable agent must fail, not run unsupervised"
        );
    }

    /// `--no-supervise` promises pure passthrough in its own help text: no
    /// scoring, no injection, nothing ever typed into the child. The M5 gate
    /// ran ahead of that decision, so it refused invocations where there was
    /// nothing left for a wrong guess to get wrong -- including the wrapper
    /// scripts around claude that the README's alias recipe encourages.
    #[cfg(unix)]
    #[test]
    fn no_supervise_passes_an_undetected_command_through_instead_of_refusing() {
        let mut h = spawn_wrap_with_flags(&[], &["--no-supervise"], &["echo", "hello"]);

        let seen = read_until(&mut h.reader, "hello", Duration::from_secs(5));
        assert!(
            seen.contains("hello"),
            "pure passthrough must actually run the command: {seen:?}"
        );
        assert!(
            !seen.contains("--agent"),
            "and must not refuse it: {seen:?}"
        );

        let status = h.child.wait().expect("wait");
        assert_eq!(status.exit_code(), 0);
    }

    #[cfg(unix)]
    #[test]
    fn keystrokes_pass_through_byte_for_byte() {
        let script = fixture("stub-tui.sh").display().to_string();
        let mut h = spawn_wrap(&[], &["sh", &script]);
        let _ = read_until(&mut h.reader, "stub-tui ready", Duration::from_secs(10));

        h.writer
            .write_all("hello wrap\r".as_bytes())
            .expect("write");
        h.writer.flush().expect("flush");
        let seen = read_until(&mut h.reader, "echo: hello wrap", Duration::from_secs(10));
        assert!(seen.contains("echo: hello wrap"), "got: {seen:?}");

        h.writer.write_all(b"/exit\r").expect("write");
        let _ = h.child.wait();
    }

    #[cfg(unix)]
    #[test]
    fn the_wrapped_exit_code_is_propagated() {
        let script = fixture("stub-tui.sh").display().to_string();
        let mut h = spawn_wrap(&[], &["sh", &script]);
        let _ = read_until(&mut h.reader, "stub-tui ready", Duration::from_secs(10));

        h.writer.write_all(b"/fail\r").expect("write");
        h.writer.flush().expect("flush");
        let status = h.child.wait().expect("wait");
        assert_eq!(
            status.exit_code(),
            5,
            "wrap must not swallow the agent's code"
        );
    }

    #[cfg(unix)]
    #[test]
    fn wrap_exits_when_the_wrapped_program_exits_on_its_own() {
        let mut h = spawn_wrap(&[], &["sh", "-c", "printf done\\n; exit 0"]);
        let seen = read_until(&mut h.reader, "done", Duration::from_secs(10));
        assert!(seen.contains("done"), "got {seen:?}");
        let status = h.child.wait().expect("wait");
        assert_eq!(status.exit_code(), 0);
    }

    #[cfg(unix)]
    #[test]
    fn a_missing_wrapped_binary_fails_without_wrecking_the_terminal() {
        let mut h = spawn_wrap(&[], &["/nonexistent/agent-binary"]);
        let status = h.child.wait().expect("wait");
        assert_ne!(status.exit_code(), 0);
        let seen = read_until(&mut h.reader, "", Duration::from_millis(300));
        assert!(
            !seen.contains("panicked"),
            "no panic on the hot path: {seen:?}"
        );
    }
}
