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

    // Reject nesting before config or terminal work: an inner interactive
    // supervisor can take down its outer session.
    if let Some(refusal) = super::sessions::nesting_refusal("wrap", env, args.allow_nested) {
        return Err(refusal.into());
    }

    let cfg = CtxConfig::load_for_launch(repo, env)?;
    // Event announcements follow config even with piped stderr; pure
    // passthrough suppresses them.
    let announcer = if args.no_supervise {
        Announcer::silent()
    } else {
        Announcer::new(cfg.chrome.events, console::colors_enabled_stderr())
    };
    let agent_name = args.agent.as_deref().or(cfg.agent.as_deref());
    // Resolve a verified adapter before touching the terminal; a live swap
    // may replace this adapter in place.
    let mut adapter = adapters::select(agent_name, &args.command, &cfg)?;
    // Read the model from the wrapped argv and recompute it on handover;
    // provider accounting must follow the model actually spawned.
    let launch_model = adapters::last_model_flag(&args.command);

    // Do not guess an adapter for a command zirv may type into; its control
    // sequences could reach the wrong program. Passthrough modes are exempt.
    let passthrough_only = args.no_supervise || args.simple;
    if !passthrough_only
        && agent_name.is_none()
        && !adapters::command_matches_adapter(adapter.as_ref(), false, &args.command)
    {
        let program = args.command.first().map(String::as_str).unwrap_or("");
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

    // This interactive session retains its id across swaps, so one relay
    // lasts its lifetime; relay startup never blocks launch.
    let _jev_relay_handle = jev_relay::start(
        &cfg.proxy.typesafe,
        jev::any_gate_enabled(&cfg.jev),
        &state_dir,
        session.as_str(),
    );

    // Pace before pty work only for supervised interactive terminals. Use
    // the same terminal signal for prompt policy; piped launches fail closed
    // to headless policy. (#358)
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

    // Passthrough and unrecognized commands must not receive adapter flags.
    let skip_injection = passthrough_only
        || !adapters::command_matches_adapter(
            adapter.as_ref(),
            agent_name.is_some(),
            &args.command,
        );
    // Pump needs the same repository slug for restart harvest.
    let memory_slug = super::state::repo_slug(repo);
    let compiled = compiled_context_for_launch(
        repo,
        skip_injection,
        &cfg,
        adapter.as_ref(),
        role,
        &state_dir,
        launch_mode_from_interactive(interactive_launch),
        args.proxy_layer.as_deref(),
        &args.command,
    );
    // Merge an existing operator-supplied system prompt flag before adding
    // another one.
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
    // Removing the operator prompt flag may empty argv; reject that case
    // without panicking in this abort-on-panic launch path.
    let (program, rest) = launch_command
        .split_first()
        .ok_or("no command to wrap; pass it after --")?;
    // Reuse policy flags on restarts; explicit operator policy wins. Simple
    // still applies safety flags, while passthrough and mismatched commands
    // must receive no adapter flags.
    let policy_skip = args.no_supervise
        || !adapters::command_matches_adapter(
            adapter.as_ref(),
            agent_name.is_some(),
            &args.command,
        );
    let policy_extra = if policy_skip {
        Vec::new()
    } else {
        adapters::with_workload_writable_roots(
            adapters::policy_launch_args(
                &cfg,
                adapter.as_ref(),
                rest,
                launch_mode_from_interactive(interactive_launch),
                role,
            ),
            adapter.as_ref(),
            repo,
            &state_dir,
        )
    };
    // Announce the effective policy once per session.
    announcer.emit(&super::announce::Event::SandboxPosture {
        detail: if policy_extra.is_empty() {
            "not applied (operator flags, an unmatched wrapped command, --no-supervise, or \
             [sandbox] enabled = false)"
                .to_string()
        } else {
            super::announce::posture_detail(&policy_extra)
        },
    });
    // Heal hooks best-effort and warn at most daily; unavailable home state
    // must not fail launch. (#420)
    if let Ok(home) = crate::utils::home_dir() {
        let _ = super::hook_integrity::heal_outdated(&state_dir, &home);
        if let Some(summary) = super::hook_integrity::drift_warning_if_due(&state_dir, &home) {
            announcer.emit(&super::announce::Event::HookIntegrity { summary });
        }
    }
    // Codex cannot pre-clear individual commands, so a prompting posture
    // needs one advisory naming the configuration path. (#222, #224)
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
                // Publish per session so concurrent supervisors retain their
                // own socket addresses.
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

    // Register after bind: only the socket result proves this session can
    // claim nudges. Preserve unreachable sessions in status, and release the
    // record after pump exits. (#139)
    // Record launch policy so status can detect later drift.
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

    // Wrap owns a different session id from the agent, so deriving the
    // transcript path from it would name an unwritten file.
    let mut transcript = TranscriptSource::new(env(TRANSCRIPT_ENV).map(PathBuf::from));

    let (cols, rows) = window_size(STDIN_FD).unwrap_or(DEFAULT_SIZE);
    // Probe VT before pty sizing and restore it explicitly with raw mode;
    // bar escape sequences require it regardless of adapter.
    let mut vt_guard = super::term::enable_vt_output().ok();
    let vt_ok = vt_guard.is_some();
    // Check stdout itself: stdin can still be a terminal when stdout is
    // redirected, and banner escapes must not enter the file.
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

    // Command-injection defense: the first-launch pty assembly does not pass
    // through `supervise::spawn_tapped`'s guard either, so apply the same
    // cmd.exe argv-reparse policy here, over the full downstream argv
    // including repository-sourced prompt text.
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

    // Export this session's harness on every launch, even without a socket,
    // so nested commands inherit the correct default.
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
    // Pin the same launch mode used by policy flags; absent headless signals
    // must fail closed in the hook. (#147)
    if let Some((key, value)) =
        adapters::launch_mode_pin_env(launch_mode_from_interactive(interactive_launch))
    {
        turn_env.push((key, value));
    }
    // The hook seat model must match the spawned argv; bare wrap cannot use
    // chat's configured model when it did not launch with it. (#537)
    let seat_cfg_model = match verb {
        super::sessions::Verb::Chat => cfg.chat.model.as_deref(),
        _ => None,
    };
    turn_env.extend(adapters::seat_model_env(role, rest, seat_cfg_model));
    turn_env.extend(adapters::seat_role_env(role));
    if args.proxy_layer.is_some() {
        turn_env.push((adapters::PROXY_DECIDED_ENV.to_string(), "1".to_string()));
    }
    // Register the logical seat after resolving its actual model and before
    // spawning the child, so its fencing generation is current. (#358)
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
                // Recover a prepared seat before exporting its generation;
                // nothing from the crashed supervisor remains live here.
                let recovered = super::rollover::on_startup(&state_dir, &seat.short, &|_| None);
                turn_env.push(super::seat::generation_env(
                    recovered.as_ref().unwrap_or(&seat),
                ));
            }
            // Seat registration failure only disables automatic rollover;
            // do not degrade unrelated supervision.
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
    // Scrub inherited session identity even when socket bind failed.
    apply_session_env(&mut command, &turn_env);

    // Share the one pty writer across input and injection, and take it
    // before spawn so Windows console-host probes can be answered.
    let mut first_writer = pair.master.take_writer()?;
    answer_inherit_cursor_probe(&mut *first_writer);
    let writer = std::sync::Arc::new(std::sync::Mutex::new(first_writer));

    // Windows child priority is inherited at creation; set the supervisor
    // posture here without raising the child process class. (#330)
    super::priority::apply_process(super::priority::posture_for(role));

    let mut child = pair.slave.spawn_command(command).map_err(|error| {
        format!(
            "adapter '{}': program '{}' failed to start: {}",
            adapter.name(),
            adapter.program(),
            error
        )
    })?;
    // Adopt the child immediately so console close or supervisor death
    // cannot orphan it.
    let mut child_guard = super::supervise::ChildGuard::adopt(child.process_id());
    // The registry initially names zirv; point it at the actual child and
    // repeat after every relaunch.
    if let Some(child_pid) = child.process_id() {
        session_guard.adopt_child_pid(child_pid);
    }
    // The launch prompt enters through argv, so this is the only edge that
    // can mark the first turn in flight. (#281)
    session_guard.stamp_in_flight(verb.as_str(), 1);

    let reader = pair.master.try_clone_reader()?;
    let (tx, rx) = mpsc::channel::<PumpEvent>();
    // Bumped on every restart so a stale reader thread from an abandoned pty
    // never reports a false PtyClosed for the pty that replaced it.
    let generation = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));

    // Serialize child output with bar redraws on real stdout.
    let stdout_lock = std::sync::Arc::new(std::sync::Mutex::new(()));

    spawn_output_thread(
        reader,
        tx.clone(),
        generation.clone(),
        0,
        stdout_lock.clone(),
    );

    // Re-arm the console probe filter for every new pty.
    let cpr_filter = std::sync::Arc::new(std::sync::Mutex::new(CprFilter::default()));
    cpr_filter
        .lock()
        .map_err(|_| "cpr filter poisoned")?
        .arm(Instant::now());

    // Operator input can meet a deferred advisory before its submit; the
    // idle gate keeps this window narrow and the input path failure-free.
    // The worst result is a garbled advisory, so no cross-thread lock is added. (#118)
    let input_tx = tx.clone();
    let input_writer = std::sync::Arc::clone(&writer);
    let input_filter = std::sync::Arc::clone(&cpr_filter);
    let input_stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let input_stop_for_thread = std::sync::Arc::clone(&input_stop);
    let input_thread = std::thread::spawn(move || {
        // Raise only this keystroke thread, leaving child process priority. (#330)
        super::priority::raise_current_thread();
        let mut buf = [0u8; 4096];
        let mut stdin = std::io::stdin();
        // Keep paste state across pty replacement so a pending paste stays whole. (#206)
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
                    // The synthetic console-host reply already handled this probe.
                    let filtered = {
                        let Ok(mut filter) = input_filter.lock() else {
                            return;
                        };
                        filter.filter(&buf[..n], now)
                    };
                    // Forward a bracketed paste in one write so its newlines
                    // cannot submit separate turns. (#206)
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
    // A bar requires RawGuard's emergency restore handler; without it, a
    // scroll region could strand the terminal in a fenced state.
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
        // If the scroll region write failed, the bar cannot assume it is active.
        bar.disabled = super::chrome::after_redraw_attempt(bar.disabled, region_ok);
        // Only an installed scroll region owes an emergency reset.
        super::term::set_bar_active(region_ok);
    }

    let debounce = Duration::from_millis(cfg.wrap.debounce_ms);
    let inject_timeout = Duration::from_millis(cfg.wrap.inject_timeout_ms);

    // Own this model so a later swap can use the successor's distiller.
    let mut distiller_model =
        handoff::resolve_distiller_model(cfg.handoff.model.as_deref(), adapter.as_ref());
    // Resolve the supervising session once from this launch environment. (#249)
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
    // Release the child job explicitly after exit; panic = "abort" means
    // Drop cannot be relied on for cleanup.
    child_guard.release();
    if native_successor.is_some() {
        session_guard.disown();
    } else {
        session_guard.release();
    }
    // Unpublish the socket at exit so a reader cannot select a dead endpoint.
    unpublish_socket_path(&state_dir, session.as_str());
    // Release the seat address when its session ends. (#358)
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
            // Report pump I/O errors on stderr; stdout carries child pty
            // bytes and may be redirected away from the operator.
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
