//! Supervises one headless run, restarting it on rot with a distilled
//! Restart and park reuse launch context and mail; a nudge recomposes and
//! re-lists mail for its session.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use super::config::{CtxConfig, EnvLookup, env_from_process};
use super::event::{NormalizedEvent, SessionId, SessionRef, TranscriptUsage, input_hash};
use super::pace;
use super::rot::Verdict;
use super::signal::{self, TurnSignal};
use super::state::{StateDir, now_secs};
use super::supervise::{self, Outcome, Tick};
#[cfg(test)]
use super::testenv;
use super::{CtxResult, adapters, agent, handoff, jev, jev_relay, log, objective, score};
use super::{
    announce, attention, config, dash, health, mail, obfuscate_store, prompt, provider, proxy,
    runtime, screen, sessions, stall, state,
};
use crate::commands::workflow::classify::Complexity;

mod accounting;
mod argv;
mod command;
mod compact;
mod effort;
mod entry;
mod restart;
mod supervision;

pub use self::accounting::ExecutionReport;
#[cfg(test)]
pub use self::accounting::ExecutionSegment;
use self::accounting::{
    evaluate_worker_budget, harvest_spend, objective_layer_for_restart, record_execution_segment,
};
pub(crate) use self::argv::pins_an_existing_conversation;
pub use self::argv::{extra_launch_flags, extract_prompt};
use self::argv::{locate_prompt, resume_pin};
use self::command::{build_command, headless_argv_len, headless_prompt_via_stdin};
pub(crate) use self::command::{headless_resume_launch, prompt_delivery_via_stdin};
use self::compact::protect_compaction_continuation;
pub(crate) use self::compact::{
    CompactBudget, action_for_verdict, compact_in_place, should_attempt_compact,
};
pub use self::compact::{SignalAction, action_for_signal};
use self::effort::apply_headless_cost_levers;
pub(crate) use self::effort::{
    LAUNCH_EFFORT_DEFAULT_FLOOR, launch_effort_action, launch_effort_question,
};
#[cfg(test)]
pub(crate) use self::entry::run_with_clock;
pub use self::entry::{ExecArgs, run, run_with, run_with_report};
pub(crate) use self::restart::EXIT_CODES;
use self::restart::capacity_backoff_secs;
pub use self::restart::{
    EXIT_ACCOUNT_EXHAUSTED, EXIT_BUDGET_EXHAUSTED, EXIT_CAPACITY_EXHAUSTED, EXIT_CONTRACT_FAILED,
    EXIT_ROT_EXHAUSTED, EXIT_STALLED, EXIT_TIMEOUT, EXIT_WRITER_BUSY, describe_exit, nudges_after,
};
use self::supervision::supervise_run;

#[allow(clippy::too_many_arguments)]
fn run_with_clock_inner<W: Write>(
    args: &ExecArgs,
    w: &mut W,
    repo: &Path,
    env: EnvLookup<'_>,
    now_fn: &dyn Fn() -> u64,
    sleep_fn: &dyn Fn(Duration),
    stable_short: Option<&str>,
    // Only the whole delegation's first launch skips the pacing wait; a
    // provider-switch recursive entry is a continuation. (#358)
    initial_launch_allowed: bool,
    report: &mut ExecutionReport,
    // Reuse one presence oracle across recursive provider handover. (#690)
    present: &dyn Fn(&str, &str) -> adapters::Liveness,
) -> CtxResult<i32> {
    let cfg = CtxConfig::load_for_launch(repo, env)?;
    // Headless event announcements follow config even without a terminal.
    let announcer =
        super::announce::Announcer::new(cfg.chrome.events, console::colors_enabled_stderr());
    let agent_name = args.agent.as_deref().or(cfg.agent.as_deref());
    // Resolve launch ownership before pacing and usage I/O: preflight must
    // reject an absent adapter program before those side effects. (#690)
    let adapter_builds_launch = args
        .command
        .first()
        .is_none_or(|first| first.starts_with('-'));
    // Flags-only argv is adapter-built in exec; selection and preflight
    // must use the same presence oracle and ownership decision. (#690)
    let adapter = adapters::select_with_presence(
        agent_name,
        &args.command,
        &cfg,
        adapter_builds_launch,
        present,
    )?;
    // Preflight only zirv-built adapter launches; an operator-supplied
    // command is outside this check and remains fail-open. (#690)
    if adapter_builds_launch {
        adapters::refuse_if_program_absent_with_presence(adapter.as_ref(), &cfg, present)?;
    }
    let execution_started = Instant::now();
    let execution_model = adapters::last_model_flag(&args.command).map(str::to_string);
    // Reject unenforceable tool-call limits before spawning. (#155)
    if args.max_tool_calls.is_some() && !adapter.counts_tool_calls() {
        return Err(format!(
            "--max-tool-calls is not supported with the '{}' adapter: it has no verified way \
             to count tool calls in its transcript, so the ceiling would never be enforced",
            adapter.name()
        )
        .into());
    }
    // Keep this adapter's distiller choice across same-harness restarts.
    let distiller_model =
        handoff::resolve_distiller_model(cfg.handoff.model.as_deref(), adapter.as_ref());
    let state = StateDir::resolve(env)?;
    // Mail listing still needs this repository slug. (#44)
    let mail_slug = super::state::repo_slug(repo);
    // Resolve parent lineage once from authority-bearing launch context,
    // never from a message. (#249)
    let parent_short = agent::parent_identity(env);

    // Set the durable objective once before prompt composition; restarts
    // reload it rather than resetting progress. (#285)
    if let Some(text) = &args.objective {
        let key = super::state::repo_slug(repo);
        let record = objective::Objective {
            schema_version: objective::SCHEMA_VERSION,
            objective: text.clone(),
            budget_tokens: cfg.pace.run_budget_tokens,
            deadline_secs: None,
            spent_tokens: 0,
            started_at: now_fn(),
            status: objective::Status::Active,
            pending_note: None,
            evidence: Vec::new(),
        };
        objective::store(&state, &key, &record)?;
    }

    // Do not inject adapter flags into an unmatched operator command.
    let skip_injection = args.simple
        || !adapters::command_matches_adapter(
            adapter.as_ref(),
            agent_name.is_some(),
            &args.command,
        );
    let mut compiled = super::compile::compile(
        crate::utils::home_dir().ok().as_deref(),
        repo,
        skip_injection,
        &cfg,
        adapter.as_ref(),
        super::prompt::PromptRole::Worker,
        &state,
        now_secs(),
        super::adapters::LaunchMode::Headless,
        true,
    );
    // Match the known prompt by value before interpreting argv shape.
    let prompt = args
        .prompt
        .clone()
        .or_else(|| extract_prompt(&args.command));
    if let Some(task) = prompt.as_deref() {
        super::compile::select_skill_descriptions_for_task(
            &mut compiled,
            &cfg,
            &state,
            repo,
            crate::utils::home_dir().ok().as_deref(),
            task,
        );
    }
    let composed = compiled.composed;
    // Flags-only argv belongs to the adapter-built launch. (#690)
    let prefix = if adapter_builds_launch {
        0
    } else {
        adapter.launch_prefix_len()
    };
    // A prompt is data even if it resembles a flag; never promote it into
    // operator instructions.
    let prompt_value_at = locate_prompt(&args.command, prefix, prompt.as_deref())
        .and_then(|(index, value)| value.map(|_| index + 1));

    // Honor an operator's existing conversation pin on the first launch
    // and track that same id; later restarts escape it. (#778)
    let (resume_pin_tokens, resumed_session_id) = resume_pin(&args.command, adapter.name());
    // Establish the session id before scoped mail and private prompt paths;
    // an explicit session id wins over a resume pin.
    let session_raw = args
        .session_id
        .clone()
        .or(resumed_session_id)
        .unwrap_or_else(|| SessionId::new_v4().to_string());
    let mut session = SessionId::parse(&session_raw);

    // Deliver only to this stable registry address. Consume mail after a
    // successful spawn, never during pacing or before an attempted launch.
    // Ordinary restarts reuse the listing; nudge re-lists it.
    let registry_short = stable_short
        .map(str::to_string)
        .unwrap_or_else(|| super::sessions::short_id(session.as_str()));
    // Mail needs an actual delivery channel: composed system context for
    // capable adapters, task text for zirv-built injection-less launches.
    // Explicit initial argv cannot carry fallback text, but relaunches can.
    let system_prompt_supported = adapter.system_prompt_supported(&args.command);
    // An explicit initial command has fixed argv and cannot receive task-text
    // mail; leave it unread. Every relaunch is zirv-built and can deliver it.
    let mail_deliverable = adapter_builds_launch || system_prompt_supported;
    // Simple mode still delivers mail through task text on adapters without
    // system-prompt injection; composed context is irrelevant there.
    let mut mail_entries: Vec<(PathBuf, super::mail::Message)> =
        if cfg.mail.enabled && mail_deliverable && (composed.is_some() || adapter_builds_launch) {
            super::mail::list(
                &state,
                &mail_slug,
                Some(adapter.name()),
                super::sessions::delivery_filter(None, &registry_short),
            )
            .unwrap_or_default()
        } else {
            Vec::new()
        };
    // If this launch cannot deliver mail, report that unread mail exists
    // without consuming it.
    if cfg.mail.enabled && !mail_deliverable {
        let withheld = super::mail::list(
            &state,
            &mail_slug,
            Some(adapter.name()),
            super::sessions::delivery_filter(None, &registry_short),
        )
        .unwrap_or_default();
        if !withheld.is_empty() {
            announcer.emit(&super::announce::Event::MailWithheld {
                count: withheld.len(),
            });
        }
    }
    // Keep rendered mail in lockstep with entries across nudge and restart.
    let mut mail_messages: Vec<super::mail::Message> = mail_entries
        .iter()
        .map(|(path, msg)| {
            super::mail::message_with_delivery_envelope(
                &cfg,
                &state,
                path,
                msg,
                parent_short.as_deref(),
                &cfg.screen.thresholds(),
            )
        })
        .collect();
    if !mail_messages.is_empty() {
        announcer.emit(&super::announce::Event::MailDelivered {
            count: mail_messages.len(),
        });
    }
    let composed = if system_prompt_supported {
        super::prompt::with_mail_layer(
            composed,
            &mail_messages,
            cfg.mail.max_delivered_bytes,
            parent_short.as_deref(),
        )
    } else {
        composed
    };

    // Capture the operator's prompt flag before cleaning argv so nudge
    // recomposition can restore that instruction. Merge rather than override.
    let operator_prompt_text: Option<String> = if composed.is_some() {
        super::prompt::extract_user_prompt_flag(adapter.as_ref(), &args.command, prompt_value_at)
            .ok()
            .and_then(|(_, text)| text)
    } else {
        None
    };
    let (launch_command, mut composed) = super::prompt::merge_command_line_prompt(
        adapter.as_ref(),
        &args.command,
        composed,
        prompt_value_at,
        super::prompt::PromptRole::Worker,
        &cfg.prompt,
    );
    composed = super::obfuscate_store::protect_composed(
        &state,
        repo,
        &cfg,
        composed,
        "exec_system_prompt",
    )?;

    // Preserve operator flags across relaunch; only regenerated flags and
    // conversation pins are removed.
    let user_extra = extra_launch_flags(&launch_command, prefix, prompt.as_deref(), adapter.name());
    // Apply default and configured policy at every launch; explicit operator
    // flags win. Simple mode still applies safety flags, while an unmatched
    // command must not receive adapter flags.
    let policy_skip = !adapter_builds_launch
        && !adapters::command_matches_adapter(
            adapter.as_ref(),
            agent_name.is_some(),
            &args.command,
        );
    let mut policy_extra = if policy_skip {
        Vec::new()
    } else {
        adapters::policy_launch_args(
            &cfg,
            adapter.as_ref(),
            &user_extra,
            adapters::LaunchMode::Headless,
            super::prompt::PromptRole::Worker,
        )
    };
    // Announce effective policy once at session start.
    announcer.emit(&super::announce::Event::SandboxPosture {
        detail: if policy_extra.is_empty() {
            "not applied (operator flags, --simple/command mismatch is irrelevant here, or \
             [sandbox] enabled = false)"
                .to_string()
        } else {
            super::announce::posture_detail(&policy_extra)
        },
    });
    if !policy_skip {
        let mcp_args = super::mcp::launch::arguments(
            adapter.name(),
            repo,
            &state,
            &registry_short,
            &user_extra,
        );
        super::mcp::launch::append(&mut policy_extra, mcp_args);
    }
    // Heal hook drift best-effort; no home state cannot fail launch. (#420)
    if let Ok(home) = crate::utils::home_dir() {
        let _ = super::hook_integrity::heal_outdated(&state, &home);
        if let Some(summary) = super::hook_integrity::drift_warning_if_due(&state, &home) {
            announcer.emit(&super::announce::Event::HookIntegrity { summary });
        }
    }

    // Probe the program actually spawned, not flags-only argv.
    let probe_target: &[String] = if adapter_builds_launch {
        &[]
    } else {
        &launch_command
    };
    let mut prompt_args = super::prompt::injection_args_for_session(
        adapter.as_ref(),
        probe_target,
        composed.as_ref(),
        &state,
        session.as_str(),
    )?;
    super::prompt::log_injection(
        &state,
        "exec",
        session.as_str(),
        composed.as_ref(),
        system_prompt_supported,
    );
    announcer.emit(&super::prompt::injection_event(
        composed.as_ref(),
        system_prompt_supported,
    ));

    let derive_transcript = |session: &SessionId| {
        adapter.transcript_path(&SessionRef {
            id: session.clone(),
            cwd: repo.to_path_buf(),
        })
    };

    // An explicit transcript belongs to the first child; restarts derive
    // their own paths.
    let mut transcript = args
        .transcript
        .clone()
        .unwrap_or_else(|| derive_transcript(&session));
    // Self-heal only zirv-derived transcript paths, never an operator path.
    let mut transcript_derived = args.transcript.is_none();

    // Warn about an unavailable restart path before rot occurs.
    if prompt.is_none() {
        writeln!(
            w,
            "zirv ctx exec: no prompt could be found in the command; restarts and usage-limit \
             parking will be unavailable for this run. Pass --prompt to enable them."
        )?;
    }
    let max_restarts = args.max_restarts.unwrap_or(cfg.supervise.max_restarts);
    let timeout = Duration::from_secs(args.timeout_secs.unwrap_or(cfg.supervise.max_cycle_secs));
    let poll = Duration::from_millis(cfg.supervise.poll_ms);
    // Fix the ceiling for the whole run and accumulate outgoing child spend
    // before each remint, so restarts cannot reset the budget. (#155/#169) (#169.2)
    let worker_budget = agent::WorkerBudget {
        tokens: args.budget_tokens,
        tool_calls: args.max_tool_calls,
    };
    // Prior child spend joins the current transcript in every budget check. (#169.2)
    let mut prior_usage = TranscriptUsage::default();
    let mut prior_tool_calls: u32 = 0;

    let socket_path = state.socket_for(session.as_str());
    let server = match signal::SignalServer::bind(&socket_path) {
        Ok(server) => Some(server),
        Err(e) => {
            // Turn signals only accelerate detection; polling is the floor.
            let _ = log::append(
                &state,
                &log::Decision {
                    ts: now_secs(),
                    session: session.as_str(),
                    verb: "exec",
                    verdict: "n/a",
                    score: 0,
                    action: "no-socket",
                    detail: &e.to_string(),
                    observed_at: None,
                },
            );
            None
        }
    };
    // Rebuild session identity for every child; export harness identity even
    // without a turn-signal socket.
    let turn_env_for = |session: &SessionId| {
        let mut turn_env: Vec<(String, String)> = server
            .as_ref()
            .map(|server| {
                adapter
                    .register_turn_signal(
                        &SessionRef {
                            id: session.clone(),
                            cwd: repo.to_path_buf(),
                        },
                        server.path(),
                    )
                    .env
            })
            .unwrap_or_default();
        turn_env.push((adapters::AGENT_ENV.to_string(), adapter.name().to_string()));
        // Record the actual route for later accounting, not a config guess. (#800)
        let route_tier = execution_model
            .as_deref()
            .and_then(|model| super::handover::tier_for_model(adapter.name(), model, &cfg));
        turn_env.extend(super::attribution::route_env(
            Some(adapter.name()),
            execution_model.as_deref(),
            route_tier,
            None,
        ));
        // Pass work-group lineage to headless children so their own
        // delegations remain under group admission and ceilings.
        if let Some(group) = env(super::agent::WORK_GROUP_ENV).filter(|id| !id.is_empty()) {
            turn_env.push((super::agent::WORK_GROUP_ENV.to_string(), group));
        }
        // Export verified parent lineage to the child that sends and reads
        // mail; this supervisor's in-process copy is insufficient. (#249)
        if let Some(parent) = env(super::agent::PARENT_SESSION_ENV) {
            turn_env.push((super::agent::PARENT_SESSION_ENV.to_string(), parent));
        }
        // Export this run's result contract to the child for consistent
        // report validation. (#318)
        if let Some(schema) = env(super::agent::RESULT_SCHEMA_ENV).filter(|s| !s.is_empty()) {
            turn_env.push((super::agent::RESULT_SCHEMA_ENV.to_string(), schema));
            turn_env.push((
                super::agent::RESULT_WORKDIR_ENV.to_string(),
                repo.display().to_string(),
            ));
        }
        turn_env
    };

    // Scrub inherited supervision identity before setting this child's;
    // failed socket bind must mean unsupervised, never another session.
    let mut jev_relay_handle: Option<jev_relay::Handle> = None;
    let mut jev_relay_session: Option<String> = None;
    let mut apply_session_env = |command: &mut Command, session: &SessionId| {
        if jev_relay_session.as_deref() != Some(session.as_str()) {
            jev_relay_handle = jev_relay::start(
                &cfg.proxy.typesafe,
                jev::any_gate_enabled(&cfg.jev),
                &state,
                session.as_str(),
            );
            jev_relay_session = Some(session.as_str().to_string());
        }
        super::sessions::scrub_supervision_env_cmd(command);
        for (key, value) in turn_env_for(session) {
            command.env(key, value);
        }
        // Headless launches cannot answer permission prompts; export their
        // mode after scrubbing inherited identity. (#236)
        if let Some((key, value)) = adapters::headless_marker_env(adapters::LaunchMode::Headless) {
            command.env(key, value);
        }
    };

    // Windows cmd and PowerShell launchers reparse downstream argv; deliver
    // task text on stdin for every adapter-built launch or relaunch.
    let prompt_via_stdin = prompt_delivery_via_stdin(adapter.as_ref(), &session);
    let relaunch_system_prompt_supported = adapter.system_prompt_supported(&[]);
    // Measure complete argv on each relaunch because prompt and context
    // lengths vary; send oversized commands through stdin. (#220, #213)
    let build_headless = |prompt_text: &str,
                          session: &SessionId,
                          extra: &[String]|
     -> CtxResult<(Command, Option<String>)> {
        let prompt_text = super::obfuscate_store::protect_text(
            &state,
            repo,
            &cfg,
            prompt_text,
            "exec_task_prompt",
        )?
        .0;
        let probe = adapter.headless_cmd(&prompt_text, session, extra);
        let argv_total_len = headless_argv_len(&probe);
        if headless_prompt_via_stdin(prompt_via_stdin, argv_total_len)
            && let Some(mut command) = adapter.headless_cmd_stdin(session, extra)
        {
            apply_headless_cost_levers(
                &mut command,
                &cfg,
                adapter.name(),
                Some(&prompt_text),
                &state,
                session,
            );
            return Ok((command, Some(prompt_text)));
        }
        let mut probe = probe;
        apply_headless_cost_levers(
            &mut probe,
            &cfg,
            adapter.name(),
            Some(&prompt_text),
            &state,
            session,
        );
        Ok((probe, None))
    };

    // Treat caller prompt text as data when building the first launch.
    let (mut command, mut stdin_prompt) = if adapter_builds_launch {
        let prompt_text = prompt.as_deref().ok_or(
            "no command to supervise; pass the agent command after --, \
             or --prompt to have zirv build the launch itself",
        )?;
        let prompt_text = super::prompt::task_prompt_with_composed_fallback(
            prompt_text,
            system_prompt_supported,
            composed.as_ref(),
        );
        let mail_in_composed = composed
            .as_ref()
            .is_some_and(|prompt| prompt.sources.contains(&super::prompt::PromptSource::Mail));
        let prompt_text = super::prompt::task_prompt_with_mail_fallback(
            &prompt_text,
            (system_prompt_supported && composed.is_some()) || mail_in_composed,
            &mail_messages,
            cfg.mail.max_delivered_bytes,
            parent_short.as_deref(),
        );
        // Restore the operator's resume pin only for the initial launch;
        // adapter launch must not mint a conflicting id. (#778)
        let extra: Vec<String> = policy_extra
            .iter()
            .cloned()
            .chain(user_extra.iter().cloned())
            .chain(resume_pin_tokens.iter().cloned())
            .chain(prompt_args.iter().cloned())
            .collect();
        let (mut command, stdin_prompt) = build_headless(&prompt_text, &session, &extra)?;
        command.current_dir(repo);
        (command, stdin_prompt)
    } else {
        // Explicit command argv is fixed by the operator; append zirv flags
        // while respecting policy pins in that argv.
        let mut argv = launch_command.clone();
        super::mcp::launch::append(
            &mut argv,
            policy_extra
                .iter()
                .chain(prompt_args.iter())
                .cloned()
                .collect(),
        );
        let mut command = build_command(&argv, repo)?;
        apply_headless_cost_levers(
            &mut command,
            &cfg,
            adapter.name(),
            prompt.as_deref(),
            &state,
            &session,
        );
        (command, None)
    };
    apply_session_env(&mut command, &session);
    let mut restarts = 0;
    // Nudges have a separate consecutive cap and require a known prompt.
    let mut nudge_restarts = 0u32;
    let can_restart = prompt.is_some();

    // Register best-effort across the run and release explicitly on each
    // exit path; panic = "abort" makes Drop unreliable for cleanup.
    // Record launch policy for later drift checks. (#139, #155)
    let safety_policy_sha256 = super::safety::policy_fingerprint(&cfg.safety).ok();
    let mut session_guard = super::sessions::SessionGuard::register(
        &state,
        super::sessions::Record::new(
            session.as_str(),
            adapter.name(),
            repo,
            super::sessions::Verb::Exec,
        )
        .with_stable_short(&registry_short)
        .with_safety_policy_sha256(safety_policy_sha256)
        // Headless delegations use the Worker prompt role. (#169)
        .with_role(super::prompt::PromptRole::Worker.label()),
    );

    // Keep pacing and screening deduplication across restarts.
    let mut pace_flags = pace::PaceGateFlags::default();
    let http_poller = super::poll::HttpPoller::new(cfg.chrome.events);
    let mut screening_announced: Option<String> = None;
    let mut compact_budget = CompactBudget::default();
    let compact_window = Duration::from_secs(cfg.supervise.interval_secs);
    // Only the whole delegation's first launch skips pacing; recursive
    // provider handover and all restarts are continuations. (#358)
    let mut initial_launch = initial_launch_allowed;

    loop {
        pace::wait_for_window(
            w,
            &state,
            &cfg.pace,
            "exec",
            session.as_str(),
            now_fn,
            sleep_fn,
            Some(&announcer),
            adapter.provider_for_model(execution_model.as_deref()),
            pace::PaceGate {
                use_credits: cfg
                    .pace
                    .use_credits
                    .for_provider(adapter.provider_for_model(execution_model.as_deref())),
                poller: cfg
                    .pace
                    .poll_enabled
                    .then_some(&http_poller as &dyn super::poll::UsagePoller),
                initial_launch,
            },
            &mut pace_flags,
        );
        initial_launch = false;

        // Hold this child in the console-close registry and kill-on-close
        // job for exactly its lifetime.
        let (mut child, tap, _child_guard) = supervise::spawn_tapped(command, stdin_prompt.clone())
            .map_err(|error| {
                adapters::format_launch_error(error.as_ref(), adapter.name(), adapter.program())
            })?;
        // Mark work in flight at spawn until this session reports a turn. (#281)
        session_guard.stamp_in_flight(super::sessions::Verb::Exec.as_str(), 0);
        // Consume delivered mail only after successful spawn; failure is
        // best-effort and cannot fail the launch.
        for (path, _) in mail_entries.drain(..) {
            let _ = super::mail::consume_and_log(
                &state,
                &mail_slug,
                &path,
                &registry_short,
                "exec",
                "exec:launch-prompt",
            );
        }
        // Fresh scorer per iteration, over the current session's transcript.
        let mut scorer = score::IncrementalScorer::new(transcript.clone());
        let mut rotted = false;
        let mut compact_requested = false;
        let mut limit_hit = false;
        let mut limit_confirmation_detail = None;
        let mut nudged_by: Option<String> = None;
        // Per-child budget warning state starts with its fresh transcript. (#155)
        let mut budget_soft_warned = false;
        let mut budget_exhausted = false;

        // C3: reset below whenever this run reported a turn of its own.
        let mut progressed = false;
        // A new child gets a fresh stall clock. (#310)
        let mut stalled = false;
        let mut capacity_pattern = None;
        let mut account_pattern = None;
        // Pass a resolver only for zirv-derived transcript paths.
        let derive = || derive_transcript(&session);
        let outcome = supervise_run(
            &mut child,
            Instant::now() + timeout,
            poll,
            &mut scorer,
            adapter.as_ref(),
            &cfg.score,
            &cfg.pace,
            &cfg.screen.thresholds(),
            &cfg.fallback.effective_health(),
            &state,
            server.as_ref(),
            session.as_str(),
            &registry_short,
            &mut session_guard,
            &announcer,
            &mut screening_announced,
            &mut rotted,
            &mut compact_requested,
            &mut compact_budget,
            compact_window,
            &mut progressed,
            &tap,
            &mut limit_hit,
            &mut capacity_pattern,
            &mut account_pattern,
            &mut limit_confirmation_detail,
            &mut nudged_by,
            nudge_restarts,
            cfg.supervise.max_nudges,
            can_restart,
            &mut transcript,
            transcript_derived.then_some(&derive as &dyn Fn() -> PathBuf),
            worker_budget,
            &prior_usage,
            prior_tool_calls,
            &mut budget_soft_warned,
            &mut budget_exhausted,
            repo,
            &cfg,
            Duration::from_secs(cfg.supervise.idle_no_tool_secs),
            Duration::from_secs(cfg.supervise.in_tool_secs),
            Duration::from_secs(cfg.supervise.stall_grace_secs),
            &mut stalled,
            args.cancellation.as_deref(),
        )?;

        if args
            .cancellation
            .as_ref()
            .is_some_and(|flag| super::provider::adapter::Cancellation::is_cancelled(flag.as_ref()))
        {
            record_execution_segment(
                report,
                adapter.as_ref(),
                &session,
                &transcript,
                &prior_usage,
                execution_model.as_deref(),
                execution_started,
            );
            session_guard.release();
            return Ok(130);
        }

        if budget_exhausted {
            let _ = log::append(
                &state,
                &log::Decision {
                    ts: now_secs(),
                    session: session.as_str(),
                    verb: "exec",
                    verdict: "budget",
                    score: 0,
                    action: "kill",
                    detail: &transcript.display().to_string(),
                    observed_at: None,
                },
            );
            writeln!(
                w,
                "zirv ctx exec: token/tool-call budget exhausted, stopping (exit \
                 {EXIT_BUDGET_EXHAUSTED})"
            )?;
            record_execution_segment(
                report,
                adapter.as_ref(),
                &session,
                &transcript,
                &prior_usage,
                execution_model.as_deref(),
                execution_started,
            );
            session_guard.release();
            return Ok(EXIT_BUDGET_EXHAUSTED);
        }

        // A turn boundary resets the consecutive-nudge budget.
        nudge_restarts = nudges_after(nudge_restarts, progressed);

        // Drain final output after child exit; live ticks can miss a fast
        // limit or capacity notice. Account exhaustion outranks capacity
        // retry, and a confirmed usage limit outranks both. (#227)
        if limit_hit {
            capacity_pattern = None;
            account_pattern = None;
        } else {
            // `drain_to_eof`, not `try_lines`: the latter is just as
            // non-blocking as an ordinary tick and can still lose the same
            // race against a fast exit that this drain exists to close.
            let final_lines = tap.drain_to_eof(supervise::FINAL_DRAIN_BUDGET);
            // Prefer the observed model context window over a catalogue
            // estimate when structured output supplies one.
            if let Some((model_id, window)) =
                super::model_window::parse_observed_window(&final_lines.join("\n"))
                && let Ok(home) = crate::utils::home_dir()
            {
                super::model_window::record(
                    &home,
                    &[model_id.as_str(), execution_model.as_deref().unwrap_or("")],
                    window,
                );
            }
            let limit_text_seen = pace::scan_for_limit(
                &final_lines,
                &state,
                session.as_str(),
                "exec",
                &mut std::io::stderr(),
            );
            if limit_text_seen {
                let now = now_fn();
                match pace::confirm_limit_hit(
                    &state,
                    &cfg.pace,
                    now,
                    adapter.provider_for_model(execution_model.as_deref()),
                ) {
                    pace::LimitConfirmation::Confirmed { detail } => {
                        limit_hit = true;
                        limit_confirmation_detail = Some(detail);
                    }
                    pace::LimitConfirmation::Unconfirmed { detail } => {
                        pace::note_unconfirmed_limit_text(
                            &state,
                            now,
                            session.as_str(),
                            "exec",
                            &detail,
                            &mut std::io::stderr(),
                        );
                    }
                }
            }
            if !limit_hit {
                account_pattern =
                    account_pattern.or_else(|| pace::scan_for_account_exhausted(&final_lines));
                if account_pattern.is_none() {
                    capacity_pattern =
                        capacity_pattern.or_else(|| pace::scan_for_capacity_error(&final_lines));
                }
            }
        }

        // Account exhaustion on nonzero exit cannot be fixed by restart;
        // preserve a clean exit even if its text matches. (#227)
        if let Some(label) = account_pattern
            && matches!(outcome, Outcome::Exited(code) if code != 0)
        {
            let _ = log::append(
                &state,
                &log::Decision {
                    ts: now_secs(),
                    session: session.as_str(),
                    verb: "exec",
                    verdict: "account",
                    score: 0,
                    action: "account-exhausted",
                    detail: label,
                    observed_at: None,
                },
            );
            writeln!(
                w,
                "zirv ctx exec: {} account exhausted ({label}); this is not retryable -- \
                 check billing before restarting (exit {EXIT_ACCOUNT_EXHAUSTED})",
                adapter.name()
            )?;
            record_execution_segment(
                report,
                adapter.as_ref(),
                &session,
                &transcript,
                &prior_usage,
                execution_model.as_deref(),
                execution_started,
            );
            session_guard.release();
            return Ok(EXIT_ACCOUNT_EXHAUSTED);
        }

        if should_attempt_compact(compact_requested, limit_hit) {
            let extra: Vec<String> = policy_extra
                .iter()
                .cloned()
                .chain(user_extra.iter().cloned())
                .chain(prompt_args.iter().cloned())
                .collect();
            // With this gate off, do not read the transcript in the live
            // supervision path. (#798)
            let compact_focus = handoff::compaction_focus_for_transcript(
                &cfg,
                &state,
                adapter.as_ref(),
                Some(&transcript),
                cfg.handoff.tail_items,
                supervise::COMPACT_FOCUS,
            );
            let compact_result = compact_in_place(
                adapter.as_ref(),
                Some(&transcript),
                Duration::from_millis(cfg.supervise.compact_timeout_ms),
                poll,
                &compact_focus,
                |compact_prompt| {
                    let session_ref = SessionRef {
                        id: session.clone(),
                        cwd: repo.to_path_buf(),
                    };
                    let (mut compact, stdin_prompt) = headless_resume_launch(
                        adapter.as_ref(),
                        compact_prompt,
                        &session_ref,
                        &extra,
                        prompt_via_stdin,
                    )?;
                    compact.current_dir(repo);
                    apply_headless_cost_levers(
                        &mut compact,
                        &cfg,
                        adapter.name(),
                        Some(compact_prompt),
                        &state,
                        &session,
                    );
                    apply_session_env(&mut compact, &session);
                    Some((compact, stdin_prompt))
                },
            )
            .and_then(|()| {
                let prompt_text = prompt
                    .as_deref()
                    .ok_or_else(|| "no prompt available for continuation".to_string())?;
                let continuation = protect_compaction_continuation(&state, repo, &cfg, prompt_text)
                    .map_err(|error| format!("sensitive-data masking failed: {error}"))?;
                let session_ref = SessionRef {
                    id: session.clone(),
                    cwd: repo.to_path_buf(),
                };
                let (mut command, stdin_prompt) = headless_resume_launch(
                    adapter.as_ref(),
                    &continuation,
                    &session_ref,
                    &extra,
                    prompt_via_stdin,
                )
                .ok_or_else(|| {
                    format!(
                        "adapter '{}' cannot resume a headless session in place",
                        adapter.name()
                    )
                })?;
                command.current_dir(repo);
                apply_headless_cost_levers(
                    &mut command,
                    &cfg,
                    adapter.name(),
                    Some(&continuation),
                    &state,
                    &session,
                );
                apply_session_env(&mut command, &session);
                Ok((command, stdin_prompt))
            });

            let verified = compact_result.is_ok();
            announcer.emit(&super::announce::Event::Compact { verified });
            match compact_result {
                Ok((continued, continued_stdin)) => {
                    let _ = log::append(
                        &state,
                        &log::Decision {
                            ts: now_secs(),
                            session: session.as_str(),
                            verb: "exec",
                            verdict: "compact",
                            score: 0,
                            action: "compact",
                            detail: &transcript.display().to_string(),
                            observed_at: None,
                        },
                    );
                    command = continued;
                    stdin_prompt = continued_stdin;
                    continue;
                }
                Err(reason) => {
                    let _ = log::append(
                        &state,
                        &log::Decision {
                            ts: now_secs(),
                            session: session.as_str(),
                            verb: "exec",
                            verdict: "compact",
                            score: 0,
                            action: "compact-failed",
                            detail: &reason,
                            observed_at: None,
                        },
                    );
                    writeln!(
                        w,
                        "zirv ctx exec: {reason}; falling back to restart with handoff"
                    )?;
                    rotted = true;
                }
            }
        }

        match outcome {
            Outcome::Exited(code) if !(limit_hit || capacity_pattern.is_some() && code != 0) => {
                // Harvest clean exits only when enabled; failure cannot
                // change a successful exit. (#37)
                if cfg.memory.enabled && cfg.memory.harvest {
                    let jsonl = std::fs::read_to_string(&transcript).unwrap_or_default();
                    let ctx = adapter.structural_context(&jsonl, cfg.handoff.tail_items);
                    let _ = super::memory::harvest_at_session_end(
                        adapter.as_ref(),
                        &distiller_model,
                        &ctx,
                        Duration::from_secs(cfg.handoff.timeout_secs),
                        repo,
                        &state,
                        &mail_slug,
                        &cfg,
                    );
                }
                record_execution_segment(
                    report,
                    adapter.as_ref(),
                    &session,
                    &transcript,
                    &prior_usage,
                    execution_model.as_deref(),
                    execution_started,
                );
                // Supervisor authority clears attention when its child is
                // gone, regardless of older hook observations. (#349)
                let _ = super::attention::record(
                    &state,
                    &registry_short,
                    super::attention::Observation::new(
                        super::attention::Authority::Supervisor,
                        format!("exited with code {code}"),
                        100,
                        now_secs(),
                    )
                    .with_lifecycle(super::attention::Lifecycle::Exited),
                    now_secs(),
                );
                session_guard.release();
                return Ok(code);
            }
            Outcome::Exited(_) | Outcome::TimedOut | Outcome::StoppedByTick(_) => {}
        }

        // Nudge relaunch is eligible only with a known prompt and available
        // consecutive budget.
        if let Some(nudged_from) = nudged_by.take() {
            // No expect in a hot restart path: panic = "abort" would take
            // down the supervised session.
            let prompt_text = prompt
                .clone()
                .ok_or_else(|| "cannot relaunch after a nudge: no prompt is known".to_string())?;

            let jsonl = std::fs::read_to_string(&transcript).unwrap_or_default();
            let ctx = adapter.structural_context(&jsonl, cfg.handoff.tail_items);
            let previous = handoff::latest_for_repo(&state, repo)
                .ok()
                .flatten()
                .map(|(_, h)| h);
            let (note, source) = handoff::distill_or_structural(
                adapter.as_ref(),
                &distiller_model,
                &ctx,
                Duration::from_secs(cfg.handoff.timeout_secs),
                cfg.chrome.events,
                previous.as_ref(),
            );
            let stored = handoff::store(&state, repo, session.as_str(), &note)?;

            // Harvest outgoing spend even without a budget; delegation
            // accounting also needs it.
            harvest_spend(
                adapter.as_ref(),
                &transcript,
                &mut prior_usage,
                &mut prior_tool_calls,
            );
            session = SessionId::new_v4();
            session_guard.refresh_session(session.as_str());
            transcript = derive_transcript(&session);
            transcript_derived = true;

            // Persist objective progress before recomposing the nudge prompt. (#285)
            let _ = objective_layer_for_restart(
                &state,
                repo,
                now_fn(),
                agent::token_spend(&prior_usage),
            );

            // A nudge recomposes and re-lists session mail, so its guidance
            // reaches the new child; ordinary restarts reuse launch context. (#44)
            let mut fresh_compiled = super::compile::compile(
                crate::utils::home_dir().ok().as_deref(),
                repo,
                skip_injection,
                &cfg,
                adapter.as_ref(),
                super::prompt::PromptRole::Worker,
                &state,
                now_secs(),
                super::adapters::LaunchMode::Headless,
                true,
            );
            if let Some(task) = prompt.as_deref() {
                super::compile::select_skill_descriptions_for_task(
                    &mut fresh_compiled,
                    &cfg,
                    &state,
                    repo,
                    crate::utils::home_dir().ok().as_deref(),
                    task,
                );
            }
            let mut fresh = fresh_compiled.composed;
            // Use stable registry address after session id rotation. Listing
            // needs a real delivery channel: composed context or task text.
            // Relaunches always have a zirv-built task-text channel.
            let nudge_mail: Vec<(PathBuf, super::mail::Message)> = if cfg.mail.enabled {
                // Registry short id survives session remint.
                super::mail::list(
                    &state,
                    &mail_slug,
                    Some(adapter.name()),
                    super::sessions::delivery_filter(Some(session_guard.short()), &registry_short),
                )
                .unwrap_or_default()
            } else {
                Vec::new()
            };
            let nudge_mail_msgs: Vec<super::mail::Message> =
                nudge_mail.iter().map(|(_, msg)| msg.clone()).collect();
            if !nudge_mail_msgs.is_empty() {
                announcer.emit(&super::announce::Event::MailDelivered {
                    count: nudge_mail_msgs.len(),
                });
            }
            // Compose mail only for adapters that inject system context;
            // others deliver it through task text.
            fresh = if relaunch_system_prompt_supported {
                super::prompt::with_mail_layer(
                    fresh,
                    &nudge_mail_msgs,
                    cfg.mail.max_delivered_bytes,
                    parent_short.as_deref(),
                )
            } else {
                fresh
            };
            // Reapply the adapter layer and captured operator instruction;
            // cleaned argv no longer contains that prompt flag.
            let fresh = super::prompt::relayer_recomposed(
                adapter.as_ref(),
                fresh,
                operator_prompt_text.as_deref(),
                super::prompt::PromptRole::Worker,
                &cfg.prompt,
            );
            composed = super::obfuscate_store::protect_composed(
                &state,
                repo,
                &cfg,
                fresh,
                "exec_system_prompt",
            )?;
            prompt_args = super::prompt::injection_args_for_session(
                adapter.as_ref(),
                &[],
                composed.as_ref(),
                &state,
                session.as_str(),
            )?;
            // Mark mail read only after the relaunch carrying it spawns.
            mail_entries = nudge_mail;
            // Keep rendered mail synchronized with entries for later park
            // or rot restart.
            mail_messages = nudge_mail_msgs.clone();

            super::prompt::log_injection(
                &state,
                "exec",
                session.as_str(),
                composed.as_ref(),
                relaunch_system_prompt_supported,
            );
            announcer.emit(&super::prompt::injection_event(
                composed.as_ref(),
                relaunch_system_prompt_supported,
            ));
            announcer.emit(&super::announce::Event::Nudge {
                from: nudged_from,
                disposition: super::announce::NudgeDisposition::Relaunching,
            });

            nudge_restarts += 1;
            let _ = log::append(
                &state,
                &log::Decision {
                    ts: now_secs(),
                    session: session.as_str(),
                    verb: "exec",
                    verdict: "n/a",
                    score: 0,
                    action: "nudge-restart",
                    detail: &format!("{source} handoff at {}", stored.display()),
                    observed_at: None,
                },
            );
            writeln!(
                w,
                "zirv ctx exec: nudged ({nudge_restarts}/{}), restarting with a {source} handoff",
                cfg.supervise.max_nudges
            )?;

            let combined = format!(
                "{prompt_text}\n\n{}",
                handoff::labeled_for_injection(&note, &cfg.screen.thresholds())
            );
            let combined = super::prompt::task_prompt_with_composed_fallback(
                &combined,
                relaunch_system_prompt_supported,
                composed.as_ref(),
            );
            let mail_in_composed = composed
                .as_ref()
                .is_some_and(|prompt| prompt.sources.contains(&super::prompt::PromptSource::Mail));
            // Use the nudge's fresh listing for task-text fallback.
            let combined = super::prompt::task_prompt_with_mail_fallback(
                &combined,
                (relaunch_system_prompt_supported && composed.is_some()) || mail_in_composed,
                &nudge_mail_msgs,
                cfg.mail.max_delivered_bytes,
                parent_short.as_deref(),
            );
            let extra: Vec<String> = policy_extra
                .iter()
                .cloned()
                .chain(user_extra.iter().cloned())
                .chain(prompt_args.iter().cloned())
                .collect();
            let (mut rebuilt, sp) = build_headless(&combined, &session, &extra)?;
            rebuilt.current_dir(repo);
            apply_session_env(&mut rebuilt, &session);
            command = rebuilt;
            stdin_prompt = sp;
            continue;
        }

        if limit_hit {
            // Move harness only after vendor-confirmed block and child stop;
            // explicit operator commands remain on their original harness. (#186)
            let visited: Vec<String> = env(super::fallback::VISITED_ENV)
                .map(|raw| super::config::split_csv_list(&raw))
                .unwrap_or_default();
            let source_model = adapters::last_model_flag(&args.command);
            let delegation = env(super::fallback::DELEGATION_ENV);
            let is_delegation = delegation.is_some();
            let route_request = super::fallback::RouteRequest {
                requested: adapter.name(),
                source_model,
                source_model_explicit: if is_delegation {
                    delegation.as_deref() == Some("explicit-model")
                } else {
                    source_model.is_some()
                },
                delegation: is_delegation,
                bounds: super::fallback::TaskBounds {
                    tokens: worker_budget.tokens,
                    tool_calls: worker_budget.tool_calls,
                },
                now: now_fn(),
                // This worker's reroute is not a new orchestrator
                // delegation; its own registry row is not competing capacity. (#328)
                exclude: &[],
                // This very session is the one being rerouted, so its own
                // registry row is not competing capacity.
                requester: Some(session.as_str()),
            };
            let route = (adapter_builds_launch && prompt.is_some())
                .then(|| {
                    super::fallback::route_blocked_session(&state, &cfg, route_request, &visited)
                })
                .flatten();
            let deferred_reset = (route.is_none() && adapter_builds_launch && prompt.is_some())
                .then(|| {
                    super::fallback::earliest_reset_choice(&state, &cfg, route_request, &visited)
                })
                .flatten();
            let alternate = route
                .as_ref()
                .map(|route| {
                    (
                        route.selected.clone(),
                        route.model.clone(),
                        route.detail(super::pace::Seat::Cli),
                    )
                })
                .or_else(|| {
                    deferred_reset.as_ref().and_then(|choice| {
                        if !choice.is_cross_harness() {
                            return None;
                        }
                        Some((
                            choice.selected.clone(),
                            choice.model.clone()?,
                            choice.detail(),
                        ))
                    })
                });
            let route_observed_at = route.as_ref().and_then(|route| route.requested_observed_at);

            if let Some((selected_agent, selected_model, selection_detail)) = alternate {
                let jsonl = std::fs::read_to_string(&transcript).unwrap_or_default();
                let ctx = adapter.structural_context(&jsonl, cfg.handoff.tail_items);
                let previous = handoff::latest_for_repo(&state, repo)
                    .ok()
                    .flatten()
                    .map(|(_, h)| h);
                let (note, source) = handoff::distill_or_structural(
                    adapter.as_ref(),
                    &distiller_model,
                    &ctx,
                    Duration::from_secs(cfg.handoff.timeout_secs),
                    cfg.chrome.events,
                    previous.as_ref(),
                );
                let stored = handoff::store(&state, repo, session.as_str(), &note)?;

                // Record source-harness spend before adding it to the
                // cumulative budget, or it would be counted twice.
                record_execution_segment(
                    report,
                    adapter.as_ref(),
                    &session,
                    &transcript,
                    &prior_usage,
                    execution_model.as_deref(),
                    execution_started,
                );

                // Carry prior and just-stopped spend across the provider
                // boundary for accounting and budget enforcement.
                harvest_spend(
                    adapter.as_ref(),
                    &transcript,
                    &mut prior_usage,
                    &mut prior_tool_calls,
                );
                let spent_tokens = prior_usage
                    .context_total()
                    .saturating_add(prior_usage.output_tokens);
                let remaining_tokens = worker_budget
                    .tokens
                    .map(|limit| limit.saturating_sub(spent_tokens));
                let remaining_tool_calls = worker_budget
                    .tool_calls
                    .map(|limit| limit.saturating_sub(prior_tool_calls));
                if worker_budget
                    .tokens
                    .is_some_and(|_| remaining_tokens == Some(0))
                    || worker_budget
                        .tool_calls
                        .is_some_and(|_| remaining_tool_calls == Some(0))
                {
                    let _ = log::append(
                        &state,
                        &log::Decision {
                            ts: now_secs(),
                            session: session.as_str(),
                            verb: "exec",
                            verdict: "budget",
                            score: 0,
                            action: "fallback-budget-exhausted",
                            detail: "usage limit coincided with the delegation budget ceiling",
                            observed_at: None,
                        },
                    );
                    session_guard.release();
                    return Ok(EXIT_BUDGET_EXHAUSTED);
                }

                // The nested launch reloads this repo's durable objective. (#285)
                let _ = objective_layer_for_restart(&state, repo, now_fn(), spent_tokens);

                let confirmation = limit_confirmation_detail
                    .as_deref()
                    .map(|detail| format!("; structured confirmation: {detail}"))
                    .unwrap_or_default();
                let detail = format!(
                    "{}{confirmation}; {} handoff at {}",
                    selection_detail,
                    source,
                    stored.display()
                );
                let _ = log::append(
                    &state,
                    &log::Decision {
                        ts: now_secs(),
                        session: session.as_str(),
                        verb: "exec",
                        verdict: "limit",
                        score: 100,
                        action: "harness-handover",
                        detail: &detail,
                        observed_at: route_observed_at,
                    },
                );
                writeln!(
                    w,
                    "zirv ctx exec: usage limit hit; continuing on another harness ({detail})"
                )?;

                // No expect on this restart path: a broken prompt assumption
                // must end honestly, not abort the process.
                let prompt_text = prompt.clone().ok_or_else(|| {
                    "cannot continue on another harness: no prompt is known".to_string()
                })?;
                let continuation = format!(
                    "{prompt_text}\n\nThe previous harness exhausted its usage window. Continue from this handoff without redoing completed work:\n\n{}",
                    handoff::labeled_for_injection(&note, &cfg.screen.thresholds())
                );
                let target = adapters::select(Some(&selected_agent), &[], &cfg)?;
                // Move this delegation's reservation to the successor
                // provider for remaining budget; ledger failure cannot block
                // an otherwise valid handover. (#358)
                let mut reservation_id = None;
                if let Some(old_id) = args.reservation_id.as_deref() {
                    let _ = super::reservation::release(
                        &state,
                        adapter.provider_for_model(execution_model.as_deref()),
                        old_id,
                    );
                    let target_provider = target.provider_for_model(Some(selected_model.as_str()));
                    reservation_id = match super::reservation::reserve(
                        &state,
                        target_provider,
                        session.as_str(),
                        remaining_tokens.unwrap_or(0),
                        now_fn(),
                    ) {
                        Ok(reservation) => Some(reservation.id),
                        Err(e) => {
                            eprintln!(
                                "zirv ctx exec: failed to record a token reservation for \
                                 provider '{target_provider}': {e}"
                            );
                            None
                        }
                    };
                    // Tell the caller which provider ledger now owns the
                    // reservation for final settlement. (#358)
                    report.final_reservation =
                        reservation_id.clone().map(|id| (id, target_provider));
                }
                let nested_args = ExecArgs {
                    agent: Some(selected_agent.clone()),
                    session_id: None,
                    transcript: None,
                    prompt: Some(continuation),
                    max_restarts: args.max_restarts,
                    timeout_secs: args.timeout_secs,
                    budget_tokens: remaining_tokens,
                    max_tool_calls: remaining_tool_calls,
                    // Do not reset a repo-keyed objective on recursive
                    // handover; that would erase accumulated spend.
                    objective: None,
                    command: target.model_args(&selected_model),
                    simple: args.simple,
                    reservation_id,
                    ..Default::default()
                };

                let mut next_visited = visited;
                if !next_visited.iter().any(|name| name == adapter.name()) {
                    next_visited.push(adapter.name().to_string());
                }
                let visited_csv = next_visited.join(",");
                let nested_env = |key: &str| {
                    if key == super::fallback::VISITED_ENV {
                        Some(visited_csv.clone())
                    } else {
                        env(key)
                    }
                };

                // Release the old registry entry before registering the
                // continuation, avoiding two live claims for one worker.
                session_guard.release();
                return run_with_clock_inner(
                    &nested_args,
                    w,
                    repo,
                    &nested_env,
                    now_fn,
                    sleep_fn,
                    Some(&registry_short),
                    false,
                    report,
                    present,
                );
            }

            let mut wait_detail = deferred_reset
                .as_ref()
                .map(super::fallback::ResetChoice::detail)
                .unwrap_or_else(|| {
                    "agent reported a usage limit; parking until the current window resets"
                        .to_string()
                });
            if let Some(detail) = &limit_confirmation_detail {
                wait_detail.push_str(&format!("; structured confirmation: {detail}"));
            }
            let _ = log::append(
                &state,
                &log::Decision {
                    ts: now_secs(),
                    session: session.as_str(),
                    verb: "exec",
                    verdict: "limit",
                    score: 100,
                    action: "limit-park",
                    detail: &wait_detail,
                    observed_at: None,
                },
            );
            writeln!(w, "zirv ctx exec: {wait_detail}")?;

            // Vendor refusal is authoritative even with proactive pacing
            // disabled; park before retrying that provider.
            let mut confirmed_limit_pace = cfg.pace.clone();
            confirmed_limit_pace.enabled = true;
            pace::wait_for_window(
                w,
                &state,
                &confirmed_limit_pace,
                "exec",
                session.as_str(),
                now_fn,
                sleep_fn,
                Some(&announcer),
                adapter.provider_for_model(execution_model.as_deref()),
                pace::PaceGate {
                    // Vendor-reported limit means immediate credit-backed
                    // relaunch may hit the same refusal.
                    use_credits: false,
                    poller: cfg
                        .pace
                        .poll_enabled
                        .then_some(&http_poller as &dyn super::poll::UsagePoller),
                    initial_launch: false,
                },
                &mut pace_flags,
            );

            let Some(prompt_text) = prompt.clone() else {
                writeln!(
                    w,
                    "zirv ctx exec: usage limit hit and the original prompt is unknown, so it cannot relaunch. Pass --prompt to enable parking."
                )?;
                record_execution_segment(
                    report,
                    adapter.as_ref(),
                    &session,
                    &transcript,
                    &prior_usage,
                    execution_model.as_deref(),
                    execution_started,
                );
                session_guard.release();
                return Ok(EXIT_ROT_EXHAUSTED);
            };

            // Harvest the outgoing child before a park remints transcript.
            harvest_spend(
                adapter.as_ref(),
                &transcript,
                &mut prior_usage,
                &mut prior_tool_calls,
            );
            session = SessionId::new_v4();
            session_guard.refresh_session(session.as_str());
            transcript = derive_transcript(&session);
            transcript_derived = true;
            prompt_args = super::prompt::injection_args_for_session(
                adapter.as_ref(),
                &[],
                composed.as_ref(),
                &state,
                session.as_str(),
            )?;
            // Attribute injection under each newly minted session id.
            super::prompt::log_injection(
                &state,
                "exec",
                session.as_str(),
                composed.as_ref(),
                relaunch_system_prompt_supported,
            );
            announcer.emit(&super::prompt::injection_event(
                composed.as_ref(),
                relaunch_system_prompt_supported,
            ));
            // Preserve operator flags through the park relaunch.
            let extra: Vec<String> = policy_extra
                .iter()
                .cloned()
                .chain(user_extra.iter().cloned())
                .chain(prompt_args.iter().cloned())
                .collect();
            let prompt_text = super::prompt::task_prompt_with_composed_fallback(
                &prompt_text,
                relaunch_system_prompt_supported,
                composed.as_ref(),
            );
            let mail_in_composed = composed
                .as_ref()
                .is_some_and(|prompt| prompt.sources.contains(&super::prompt::PromptSource::Mail));
            // Park reuses the latest mail listing, including any nudge's
            // refresh; it does not re-list.
            let prompt_text = super::prompt::task_prompt_with_mail_fallback(
                &prompt_text,
                (relaunch_system_prompt_supported && composed.is_some()) || mail_in_composed,
                &mail_messages,
                cfg.mail.max_delivered_bytes,
                parent_short.as_deref(),
            );
            let (mut rebuilt, sp) = build_headless(&prompt_text, &session, &extra)?;
            rebuilt.current_dir(repo);
            apply_session_env(&mut rebuilt, &session);
            command = rebuilt;
            stdin_prompt = sp;
            continue;
        }

        // Only a nonzero child exit can count as capacity exhaustion. (#227)
        let capacity_exit =
            capacity_pattern.is_some() && matches!(outcome, Outcome::Exited(code) if code != 0);

        let reason = if capacity_exit {
            "capacity"
        } else if stalled {
            "stalled"
        } else if rotted {
            "rot"
        } else {
            "timeout"
        };
        let exhausted_code = if capacity_exit {
            EXIT_CAPACITY_EXHAUSTED
        } else if stalled {
            EXIT_STALLED
        } else if rotted {
            EXIT_ROT_EXHAUSTED
        } else {
            EXIT_TIMEOUT
        };

        let _ = log::append(
            &state,
            &log::Decision {
                ts: now_secs(),
                session: session.as_str(),
                verb: "exec",
                verdict: reason,
                score: 0,
                action: "kill",
                detail: &transcript.display().to_string(),
                observed_at: None,
            },
        );

        let Some(prompt_text) = prompt.clone() else {
            writeln!(
                w,
                "zirv ctx exec: {reason} detected but the original prompt is unknown, so it cannot restart. Pass --prompt to enable restarts."
            )?;
            let _ = log::append(
                &state,
                &log::Decision {
                    ts: now_secs(),
                    session: session.as_str(),
                    verb: "exec",
                    verdict: reason,
                    score: 0,
                    action: "stand-down",
                    detail: "no prompt available for restart",
                    observed_at: None,
                },
            );
            record_execution_segment(
                report,
                adapter.as_ref(),
                &session,
                &transcript,
                &prior_usage,
                execution_model.as_deref(),
                execution_started,
            );
            session_guard.release();
            return Ok(exhausted_code);
        };

        // Record this boot in the cross-process restart chain before checking
        // local budget; capacity, stall, and crash use separate classes. (#310, #227)
        let failure_class = match reason {
            "capacity" => super::chain::FailureClass::UsageLimit,
            "stalled" => super::chain::FailureClass::Stalled,
            _ => super::chain::FailureClass::Crash,
        };
        let chain_key = super::state::repo_slug(repo);
        if let super::chain::ChainVerdict::Tripped { boots } =
            super::chain::record_boot_and_evaluate(
                &state,
                &chain_key,
                failure_class,
                false,
                now_secs(),
                cfg.supervise.chain_max_restarts,
                cfg.supervise.chain_max_gap_secs,
            )
        {
            let _ = log::append(
                &state,
                &log::Decision {
                    ts: now_secs(),
                    session: session.as_str(),
                    verb: "exec",
                    verdict: reason,
                    score: 0,
                    action: "chain-tripped",
                    detail: &format!(
                        "{boots} unplanned {reason} respawns within the configured gap; not \
                         auto-resuming"
                    ),
                    observed_at: None,
                },
            );
            writeln!(
                w,
                "zirv ctx exec: restart-chain breaker tripped ({boots} {reason} respawns within \
                 the configured gap); not auto-resuming -- run `zirv ctx status` (exit \
                 {exhausted_code})"
            )?;
            record_execution_segment(
                report,
                adapter.as_ref(),
                &session,
                &transcript,
                &prior_usage,
                execution_model.as_deref(),
                execution_started,
            );
            session_guard.release();
            return Ok(exhausted_code);
        }

        if restarts >= max_restarts {
            let _ = log::append(
                &state,
                &log::Decision {
                    ts: now_secs(),
                    session: session.as_str(),
                    verb: "exec",
                    verdict: reason,
                    score: 0,
                    action: "give-up",
                    detail: "restart budget exhausted",
                    observed_at: None,
                },
            );
            if capacity_exit {
                let label = capacity_pattern.unwrap_or("provider capacity error");
                writeln!(
                    w,
                    "zirv ctx exec: {} finished: provider capacity limit ({label}) after \
                     {restarts} restarts; workspace changes are uncommitted (exit \
                     {exhausted_code})",
                    adapter.name()
                )?;
            } else {
                writeln!(
                    w,
                    "zirv ctx exec: {reason} after {restarts} restarts, giving up with exit {exhausted_code}"
                )?;
            }
            record_execution_segment(
                report,
                adapter.as_ref(),
                &session,
                &transcript,
                &prior_usage,
                execution_model.as_deref(),
                execution_started,
            );
            session_guard.release();
            return Ok(exhausted_code);
        }

        let jsonl = std::fs::read_to_string(&transcript).unwrap_or_default();
        let ctx = adapter.structural_context(&jsonl, cfg.handoff.tail_items);
        let previous = handoff::latest_for_repo(&state, repo)
            .ok()
            .flatten()
            .map(|(_, h)| h);
        let (note, source) = handoff::distill_or_structural(
            adapter.as_ref(),
            &distiller_model,
            &ctx,
            Duration::from_secs(cfg.handoff.timeout_secs),
            cfg.chrome.events,
            previous.as_ref(),
        );
        let stored = handoff::store(&state, repo, session.as_str(), &note)?;
        // Harvest enabled, genuinely distilled handoffs best-effort; failure
        // must not fail a restart.
        if source == "distilled" {
            let _ = super::memory::harvest_durable(
                adapter.as_ref(),
                &distiller_model,
                &note,
                repo,
                &state,
                &mail_slug,
                &cfg,
            );
        }
        announcer.emit(&super::announce::Event::Restart {
            style: source.to_string(),
            stored: stored.display().to_string(),
        });

        restarts += 1;
        let _ = log::append(
            &state,
            &log::Decision {
                ts: now_secs(),
                session: session.as_str(),
                verb: "exec",
                verdict: reason,
                score: 0,
                action: "restart",
                detail: &format!("{source} handoff at {}", stored.display()),
                observed_at: None,
            },
        );
        writeln!(
            w,
            "zirv ctx exec: {reason} detected, restarting ({restarts}/{max_restarts}) with a {source} handoff"
        )?;

        // Delay capacity retries only; rot and timeout need a fresh child
        // without extra backoff. (#227)
        if capacity_exit {
            let backoff = capacity_backoff_secs(restarts);
            if backoff > 0 {
                writeln!(w, "zirv ctx exec: backing off {backoff}s before retrying")?;
                sleep_fn(Duration::from_secs(backoff));
            }
        }

        // Harvest before replacing transcript for whole-run accounting.
        harvest_spend(
            adapter.as_ref(),
            &transcript,
            &mut prior_usage,
            &mut prior_tool_calls,
        );
        // Refresh objective spend beside the handoff because ordinary
        // restarts reuse the old composed context. (#285)
        let objective_block =
            objective_layer_for_restart(&state, repo, now_fn(), agent::token_spend(&prior_usage));
        session = SessionId::new_v4();
        session_guard.refresh_session(session.as_str());
        // A fresh child needs its own transcript watcher.
        transcript = derive_transcript(&session);
        transcript_derived = true;
        prompt_args = super::prompt::injection_args_for_session(
            adapter.as_ref(),
            &[],
            composed.as_ref(),
            &state,
            session.as_str(),
        )?;
        // Attribute injected context at every session start.
        super::prompt::log_injection(
            &state,
            "exec",
            session.as_str(),
            composed.as_ref(),
            relaunch_system_prompt_supported,
        );
        announcer.emit(&super::prompt::injection_event(
            composed.as_ref(),
            relaunch_system_prompt_supported,
        ));
        let combined = format!(
            "{prompt_text}\n\n{}",
            handoff::labeled_for_injection(&note, &cfg.screen.thresholds())
        );
        let combined = match &objective_block {
            Some(text) => format!("{combined}{text}"),
            None => combined,
        };
        let combined = super::prompt::task_prompt_with_composed_fallback(
            &combined,
            relaunch_system_prompt_supported,
            composed.as_ref(),
        );
        let mail_in_composed = composed
            .as_ref()
            .is_some_and(|prompt| prompt.sources.contains(&super::prompt::PromptSource::Mail));
        // Ordinary restart reuses the latest mail listing for task-text
        // fallback without re-listing.
        let combined = super::prompt::task_prompt_with_mail_fallback(
            &combined,
            (relaunch_system_prompt_supported && composed.is_some()) || mail_in_composed,
            &mail_messages,
            cfg.mail.max_delivered_bytes,
            parent_short.as_deref(),
        );
        // Preserve operator flags through restart.
        let extra: Vec<String> = policy_extra
            .iter()
            .cloned()
            .chain(user_extra.iter().cloned())
            .chain(prompt_args.iter().cloned())
            .collect();
        let (mut rebuilt, sp) = build_headless(&combined, &session, &extra)?;
        rebuilt.current_dir(repo);
        apply_session_env(&mut rebuilt, &session);
        command = rebuilt;
        stdin_prompt = sp;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::ctx::window::{self, UsageWindows, Window};
    use std::collections::HashMap;

    pub(super) fn fixture(name: &str) -> PathBuf {
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests")
            .join("fixtures")
            .join(name)
    }

    /// Runs the fake agent directly, so `exec` supervises a real child whose
    /// transcript path we control through `--transcript`.
    pub(super) fn fake_agent_command(session: &str) -> Vec<String> {
        vec![
            "sh".to_string(),
            fixture("fake-agent.sh").display().to_string(),
            "-p".to_string(),
            "do the work".to_string(),
            "--session-id".to_string(),
            session.to_string(),
        ]
    }

    pub(super) fn base_env(state: &std::path::Path) -> HashMap<String, String> {
        [
            (
                crate::commands::ctx::state::STATE_ENV.to_string(),
                state.display().to_string(),
            ),
            (
                "ZIRV_CTX_AGENT_BIN".to_string(),
                format!("sh {}", fixture("fake-agent.sh").display()),
            ),
            // T8: `run_with`'s `sleep_fn` is real `std::thread::sleep` (not
            // test-injectable -- see its own call site), and a fresh temp
            // state dir has no usage source by construction, so every test
            // built on this helper would otherwise pay the real, wall-clock
            // fail-safe delay (default 60s) on every call into `wait_for_
            // window`. Zeroed here, not by lowering the production default:
            // pace.rs's own unit tests already cover the delay's correctness
            // with a `FakeClock`, so exec.rs's tests (which are not testing
            // pacing) should not pay it in real time.
            (
                "ZIRV_CTX_PACE_BLIND_DELAY_SECS".to_string(),
                "0".to_string(),
            ),
            // Fix round (inline-argv budget regression): these fixtures
            // deliver the composed prompt as one argv element through
            // Git-for-Windows `sh.exe`, which silently truncates a single
            // argument at ~8186 bytes -- the index has its own tests in
            // prompt.rs. A test asserting the index IS present sets this
            // back to "true" explicitly.
            (
                "ZIRV_CTX_PROMPT_SKILL_INDEX".to_string(),
                "false".to_string(),
            ),
        ]
        .into()
    }

    pub(super) fn transcript_for(
        home: &std::path::Path,
        repo: &std::path::Path,
        session: &str,
    ) -> PathBuf {
        home.join(".claude/projects")
            .join(crate::commands::ctx::adapters::claude::project_slug(repo))
            .join(format!("{session}.jsonl"))
    }

    #[test]
    fn a_healthy_run_exits_with_the_childs_own_code() {
        let tmp = crate::commands::ctx::testenv::repo();
        let home = tmp.path().join("home");
        let session = "11111111-2222-4333-8444-555555555555";
        let env = base_env(&tmp.path().join("state"));

        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);
        // SAFETY: CI runs tests single-threaded.
        unsafe {
            std::env::set_var("FAKE_AGENT_MODE", "healthy");
        }
        let args = ExecArgs {
            agent: Some("claude".to_string()),
            session_id: Some(session.to_string()),
            transcript: Some(transcript_for(&home, tmp.path(), session)),
            prompt: Some("do the work".to_string()),
            max_restarts: Some(2),
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
        let code = run_with(&args, &mut out, tmp.path(), &|k| env.get(k).cloned());
        unsafe {
            std::env::remove_var("FAKE_AGENT_MODE");
        }

        assert_eq!(code.expect("runs"), 0);
    }

    /// T11: the fail-safe blind delay (T8) actually reaches the injected
    /// `sleep_fn` with the right duration -- proof this path is real, not
    /// just claimed by `pace.rs`'s own unit tests, which never touch this
    /// integration seam at all. `base_env` zeros `ZIRV_CTX_PACE_BLIND_DELAY_
    /// SECS` for every other test in this file (see its own doc comment);
    /// this test overrides it back to a small nonzero value specifically so
    /// there is something real to observe, then verifies the observation
    /// through a recording `sleep_fn` rather than actually blocking --
    /// exactly the seam `pace.rs`'s own `FakeClock` tests already use, now
    /// available one layer up.
    #[test]
    fn the_blind_delay_reaches_the_injected_sleep_fn_with_the_right_duration() {
        let tmp = crate::commands::ctx::testenv::repo();
        let home = tmp.path().join("home");
        let session = "33333333-2222-4333-8444-555555555555";
        let mut env = base_env(&tmp.path().join("state"));
        env.insert(
            "ZIRV_CTX_PACE_BLIND_DELAY_SECS".to_string(),
            "2".to_string(),
        );

        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);
        unsafe {
            std::env::set_var("FAKE_AGENT_MODE", "healthy");
        }
        let args = ExecArgs {
            agent: Some("claude".to_string()),
            session_id: Some(session.to_string()),
            transcript: Some(transcript_for(&home, tmp.path(), session)),
            prompt: Some("do the work".to_string()),
            max_restarts: Some(2),
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
        let slept: std::cell::RefCell<Vec<u64>> = std::cell::RefCell::new(Vec::new());
        let code = run_with_clock(
            &args,
            &mut out,
            tmp.path(),
            &|k| env.get(k).cloned(),
            &crate::commands::ctx::state::now_secs,
            &|d: Duration| slept.borrow_mut().push(d.as_secs()),
        );
        unsafe {
            std::env::remove_var("FAKE_AGENT_MODE");
        }

        assert_eq!(code.expect("runs"), 0);
        assert_eq!(
            slept.borrow().first().copied(),
            Some(2),
            "the blind-mode delay must actually be slept via the injected sleep_fn, got {:?}",
            slept.borrow()
        );
    }

    #[test]
    fn a_failing_child_propagates_its_exit_code() {
        let tmp = crate::commands::ctx::testenv::repo();
        let home = tmp.path().join("home");
        let session = "22222222-2222-4333-8444-555555555555";
        let env = base_env(&tmp.path().join("state"));

        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);
        unsafe {
            std::env::set_var("FAKE_AGENT_MODE", "fail");
        }
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
        let code = run_with(&args, &mut out, tmp.path(), &|k| env.get(k).cloned());
        unsafe {
            std::env::remove_var("FAKE_AGENT_MODE");
        }
        assert_eq!(code.expect("runs"), 3);
    }

    pub(super) fn transcripts_in(home: &std::path::Path) -> Vec<PathBuf> {
        let mut found = Vec::new();
        let Ok(dirs) = std::fs::read_dir(home.join(".claude/projects")) else {
            return found;
        };
        for dir in dirs.flatten() {
            let Ok(files) = std::fs::read_dir(dir.path()) else {
                continue;
            };
            for file in files.flatten() {
                if file.path().extension().and_then(|e| e.to_str()) == Some("jsonl") {
                    found.push(file.path());
                }
            }
        }
        found
    }

    pub(super) fn store_provider_collector(
        state_dir: &std::path::Path,
        provider: &str,
        percent: f64,
        limit_reached: bool,
    ) {
        let state = crate::commands::ctx::state::StateDir::from_root(state_dir.to_path_buf());
        let now = crate::commands::ctx::state::now_secs();
        window::store_for(
            &state,
            provider,
            &UsageWindows {
                five_hour: Some(Window {
                    used_percentage: percent,
                    resets_at: now + 60,
                    observed_at: now,
                    overage_covered: false,
                    limit_reached,
                }),
                seven_day: None,
            },
        )
        .expect("store provider collector state");
    }

    // N4: `zirv ctx nudge` restarting a headless worker.
    //
    // Every test here drives a real `run_with` call whose first agent
    // invocation hangs (`FAKE_AGENT_MODE_FILE` starting with "hang") and is
    // nudged from a background thread once its transcript is up. Like every
    // other test in this module that spawns `sh`/`fake-agent.sh`, these are
    // blocked on Windows by the pre-existing os-193 spawn issue (see this
    // module's other `sh`-spawning tests); written to the same standard the
    // rest of this suite holds regardless.

    /// Polls `path` until it has at least `n` lines or `timeout` elapses,
    /// returning whatever was there either way -- the same "best effort,
    /// bounded wait" shape `run_loop.rs`'s own synchronized tests use via a
    /// marker file, adapted here to a growing log instead of a touch-once
    /// marker since more than one invocation is expected.
    pub(super) fn wait_for_lines(
        path: &std::path::Path,
        n: usize,
        timeout: Duration,
    ) -> Vec<String> {
        let deadline = Instant::now() + timeout;
        loop {
            if let Ok(text) = std::fs::read_to_string(path) {
                let lines: Vec<String> = text.lines().map(|l| l.to_string()).collect();
                if lines.len() >= n {
                    return lines;
                }
            }
            if Instant::now() >= deadline {
                return std::fs::read_to_string(path)
                    .map(|t| t.lines().map(|l| l.to_string()).collect())
                    .unwrap_or_default();
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// Nudges whichever session is currently live. `exec` (like `loop`)
    /// keeps exactly one registry record at a time, refreshed on every
    /// restart or park, so resolving the registry is how this test finds the
    /// run it is driving without knowing that session's id up front.
    ///
    /// This used to pass an empty prefix and lean on `starts_with("")`
    /// matching everything. F6 made `zirv ctx nudge` refuse any prefix
    /// shorter than four characters -- a unique-but-mistyped prefix could
    /// otherwise wake, and in `exec`'s case restart, a session the operator
    /// never named -- and an empty prefix is the extreme case of exactly
    /// that. The helper now resolves the live short id and passes it whole,
    /// which is what an operator reading `zirv ctx status` would type.
    pub(super) fn nudge_live_session(
        state_dir: &std::path::Path,
        repo: &std::path::Path,
        message: &str,
    ) {
        let state = crate::commands::ctx::state::StateDir::from_root(state_dir.to_path_buf());
        let prefix = crate::commands::ctx::sessions::list(&state)
            .into_iter()
            .find(|(_, liveness)| *liveness == crate::commands::ctx::sessions::Liveness::Live)
            .map(|(record, _)| record.short)
            .expect("exactly one live session to nudge");
        let env: HashMap<String, String> = [(
            crate::commands::ctx::state::STATE_ENV.to_string(),
            state_dir.display().to_string(),
        )]
        .into();
        let args = crate::commands::ctx::sessions::NudgeArgs {
            prefix,
            message: Some(message.to_string()),
            message_file: None,
        };
        let mut out = Vec::new();
        let mut stdin = std::io::Cursor::new(Vec::<u8>::new());
        crate::commands::ctx::sessions::run_nudge_with(
            &args,
            &mut out,
            repo,
            &|k| env.get(k).cloned(),
            &mut stdin,
        )
        .expect("nudge the live session");
    }

    /// `wait_for_lines`, but a give-up (never reaching `n` lines within
    /// `budget`) panics with a clear message instead of silently returning
    /// whatever partial result it has. Every writer-thread test in this
    /// nudge family used to swallow that case (`if lines.is_empty() {
    /// return; }`) and just never nudge -- the only visible symptom, tens of
    /// seconds later, was the launch's own exec timeout (a bare exit 76),
    /// indistinguishable from a real regression. `budget` is sized honestly
    /// against each test's own exec timeout (not the old, uniformly tight
    /// 5s this whole family shared regardless of how much headroom its own
    /// launch actually had), leaving real margin for the rest of the test's
    /// work after the wait. A panic here is caught by the caller's own
    /// `writer.join().expect(...)`, which is what actually fails the test --
    /// this only makes the *reason* legible.
    pub(super) fn wait_for_lines_or_panic(
        path: &std::path::Path,
        n: usize,
        budget: Duration,
    ) -> Vec<String> {
        let lines = wait_for_lines(path, n, budget);
        assert!(
            lines.len() >= n,
            "never saw {n} line(s) in {} within {budget:?} -- the hang-mode agent likely never \
             started, or this machine is starved badly enough that it could not be observed in \
             time (check for CPU contention before assuming a real regression): got {lines:?}",
            path.display()
        );
        lines
    }
}
