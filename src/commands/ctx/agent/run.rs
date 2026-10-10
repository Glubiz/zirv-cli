//! `run_with`: the full lifecycle of one `zirv ctx agent` delegation, from
//! task-card claim and worktree allocation through dispatch (dashboard pane
//! or inline supervised run) to report-back, plus its goal-bootstrap helpers.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::Deserialize;

use super::super::CtxResult;
use super::super::adapters::{self, AgentAdapter};
use super::super::announce::{Announcer, Event};
use super::super::chat::quiet_env;
use super::super::config::{CtxConfig, EnvLookup, env_from_process};
use super::super::envelope;
use super::super::event::{SessionId, SessionRef, TranscriptUsage};
use super::super::exec::{self, ExecArgs};
use super::super::pace;
use super::super::permit::{self, WorkerMode};
use super::super::policy;
use super::super::result_schema::{self, Schema};
use super::super::routing;
use super::super::state::StateDir;
use super::super::worktree;
use super::dashboard::*;
use super::worktree_lifecycle::*;
use super::*;

#[derive(Deserialize)]
struct BootstrapCompletion {
    status: String,
    evidence: String,
}

struct GoalBootstrapError {
    error: Box<dyn std::error::Error>,
    usage: Option<TranscriptUsage>,
    exit_code: Option<i32>,
}

impl std::fmt::Display for GoalBootstrapError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.error.fmt(f)
    }
}

impl std::fmt::Debug for GoalBootstrapError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.error.fmt(f)
    }
}

impl std::error::Error for GoalBootstrapError {}

impl GoalBootstrapError {
    fn before_launch(error: impl Into<Box<dyn std::error::Error>>) -> Self {
        Self {
            error: error.into(),
            usage: None,
            exit_code: None,
        }
    }

    fn after_launch(
        error: impl Into<Box<dyn std::error::Error>>,
        usage: TranscriptUsage,
        exit_code: i32,
    ) -> Self {
        Self {
            error: error.into(),
            usage: Some(usage),
            exit_code: Some(exit_code),
        }
    }
}

const BOOTSTRAP_SYSTEM_PROMPT: &str = "Prepare only the local development environment needed for the operator goal. Do not edit business logic, delegate, or run `zirv ctx agent`. Report only the environment preparation performed. Your final response must contain JSON: {\"status\":\"Done\",\"evidence\":\"...\"}.";

fn goal_bootstrap_envelope(
    args: &AgentArgs,
    parent: &envelope::WorkerEnvelope,
    session: &str,
    budget_tokens: Option<u64>,
) -> Result<(envelope::WorkerEnvelope, String), envelope::CannotGrow> {
    let short = super::super::sessions::short_id(session);
    let principal = format!("{}/{}", parent.principal, short);
    let mut bootstrap_args = args.clone();
    bootstrap_args.depth = Some(0);
    let mut requested =
        requested_envelope_from_args(&bootstrap_args, parent, principal.clone(), budget_tokens);
    requested.tools.delegate = false;
    envelope::WorkerEnvelope::narrow(parent, &requested).map(|child| (child, principal))
}

fn run_goal_bootstrap(
    args: &AgentArgs,
    cfg: &CtxConfig,
    state: &StateDir,
    repo: &Path,
    parent_envelope: &envelope::WorkerEnvelope,
    budget_tokens: Option<u64>,
    env: EnvLookup<'_>,
) -> Result<TranscriptUsage, GoalBootstrapError> {
    let goal = args
        .goal
        .as_deref()
        .ok_or_else(|| GoalBootstrapError::before_launch("goal bootstrap called without --goal"))?;
    let adapter =
        adapters::select(Some(&args.name), &[], cfg).map_err(GoalBootstrapError::before_launch)?;
    let mut command = adapters::with_workload_writable_roots(
        adapters::policy_launch_args(
            cfg,
            adapter.as_ref(),
            &[],
            adapters::LaunchMode::Headless,
            super::super::prompt::PromptRole::Worker,
        ),
        adapter.as_ref(),
        repo,
        state,
    );
    if let Some(model) = adapters::resolve_tiered_model(
        cfg,
        adapter.name(),
        crate::commands::workflow::agents::ModelTier::Fast,
    ) {
        command.extend(adapter.model_args(model));
    }
    let system_prompt_args = adapter.system_prompt_args(BOOTSTRAP_SYSTEM_PROMPT);
    if system_prompt_args.is_empty() {
        return Err(GoalBootstrapError::before_launch(format!(
            "goal bootstrap cannot run on adapter '{}': it has no verified system-prompt channel",
            adapter.name()
        )));
    }
    command.extend(system_prompt_args);

    let session = SessionId::new_v4().to_string();
    let (bootstrap_envelope, principal) =
        goal_bootstrap_envelope(args, parent_envelope, &session, budget_tokens).map_err(
            |error| {
                GoalBootstrapError::before_launch(format!(
                    "goal bootstrap envelope refused: {error}"
                ))
            },
        )?;
    let envelope_json = envelope::canonical_json(&bootstrap_envelope).ok();
    let envelope_sha256 = envelope::digest(&bootstrap_envelope).ok();
    let parent_session = super::super::mail::session_identity(env).unwrap_or_default();
    let bootstrap_env = envelope_env(env, envelope_json, Some(principal.clone()));
    let bootstrap_env = result_schema_env(&bootstrap_env, None);
    let bootstrap_env = |key: &str| {
        if key == "ZIRV_CTX_FALLBACK" {
            Some("false".to_string())
        } else {
            bootstrap_env(key)
        }
    };
    let exec_args = ExecArgs {
        agent: Some(args.name.clone()),
        session_id: Some(session.clone()),
        prompt: Some(goal.to_string()),
        max_restarts: Some(1),
        timeout_secs: Some(cfg.worker.bootstrap_timeout_secs),
        budget_tokens,
        command,
        cancellation: args.cancellation.clone(),
        simple: true,
        ..Default::default()
    };
    let mut output = Vec::new();
    let (code, report) = match exec::run_with_report(&exec_args, &mut output, repo, &bootstrap_env)
    {
        Ok(result) => result,
        Err(error) => {
            let path = write_delegation_result(
                state,
                repo,
                &session,
                &args.name,
                "launch_failed",
                &None,
                &[vec![error.to_string()]],
                &[],
                None,
                false,
            );
            let _ = super::super::log::append_delegation(
                state,
                &super::super::log::Delegation {
                    ts: super::super::state::now_secs(),
                    session: &session,
                    parent_session: &parent_session,
                    work_group_id: args.group.as_deref(),
                    agent: &args.name,
                    model: adapters::last_model_flag(&exec_args.command),
                    input_tokens: 0,
                    cache_creation_input_tokens: 0,
                    cache_read_input_tokens: 0,
                    output_tokens: 0,
                    wall_ms: 0,
                    exit_code: 1,
                    outcome: "bootstrap-launch-failed",
                    mode: Some(WorkerMode::Writing),
                    task_class: Some(super::super::log::TaskClass::Other),
                    principal: &principal,
                    envelope_sha256: envelope_sha256.as_deref(),
                },
            );
            return Err(GoalBootstrapError::before_launch(format!(
                "goal bootstrap failed to launch: {error}; diagnostic result: {}",
                path.display()
            )));
        }
    };
    let final_session = report
        .segments
        .last()
        .map(|segment| segment.session.as_str())
        .unwrap_or(session.as_str());
    let transcript = adapter.transcript_path(&SessionRef {
        id: SessionId::parse(final_session),
        cwd: repo.to_path_buf(),
    });
    let text = std::fs::read_to_string(&transcript).ok().and_then(|jsonl| {
        adapter
            .structural_context(&jsonl, 1)
            .assistant_texts
            .last()
            .cloned()
    });
    let completion = text
        .as_deref()
        .and_then(result_schema::extract_json_candidate)
        .and_then(|json| serde_json::from_str::<BootstrapCompletion>(&json).ok());
    let valid = completion.is_some_and(|completion| {
        completion.status == "Done" && !completion.evidence.trim().is_empty()
    });
    let (stored, truncated) = cap_report(text.as_deref());
    let path = if let Some(report_text) = stored.as_deref() {
        store_report_only(
            state,
            repo,
            final_session,
            &args.name,
            report_text,
            truncated,
        )
    } else {
        write_delegation_result(
            state,
            repo,
            final_session,
            &args.name,
            "exited_no_report",
            &None,
            &[vec!["bootstrap produced no assistant report".to_string()]],
            &[],
            None,
            false,
        )
    };
    let outcome = if code == 0 && valid {
        "bootstrap-ok"
    } else {
        "bootstrap-failed"
    };
    let usage = append_execution_segments(
        state,
        &report,
        &parent_session,
        args.group.as_deref(),
        code,
        outcome,
        WorkerMode::Writing,
        Some(super::super::log::TaskClass::Other),
        &principal,
        envelope_sha256.as_deref(),
    );
    if code != 0 || !valid {
        return Err(GoalBootstrapError::after_launch(
            format!(
                "goal bootstrap refused main worker launch (exit {code}; current explicit Done report with non-empty evidence required); diagnostic result: {}",
                path.display()
            ),
            usage,
            code,
        ));
    }
    Ok(usage)
}

/// `agent_bin` reaches only the adapter it names; any other candidate keeps its own program.
fn fallback_candidate(
    name: &str,
    ctor: fn(Option<&str>) -> Box<dyn AgentAdapter>,
    bin: Option<&str>,
) -> Box<dyn AgentAdapter> {
    let foreign = adapters::agent_bin_names_a_different_adapter(bin, name).is_some();
    ctor(if foreign { None } else { bin })
}

/// `auto` with no evidence pick: the default harness, unless that is this seat's own harness
/// (which a seat may not delegate to) and another enabled, live, not usage-refused harness
/// exists. Candidates are built the way `resolve_default` builds them: `agent_bin` reaches only
/// the adapter it names, and presence is skipped only for that adapter.
fn auto_fallback(
    args: &mut AgentArgs,
    cfg: &CtxConfig,
    env: EnvLookup<'_>,
    present: &dyn Fn(&str, &str) -> adapters::Liveness,
    pace_refused: &dyn Fn(&str) -> bool,
) -> CtxResult<()> {
    args.name = adapters::resolve_default_with_presence(cfg, present)?
        .0
        .name()
        .to_string();
    let Some(message) = same_harness_refusal(args, env) else {
        return Ok(());
    };
    let own = args.name.clone();
    let bin = cfg.agent_bin.as_deref();
    let other = adapters::ADAPTERS
        .iter()
        .filter(|(name, _)| !name.eq_ignore_ascii_case(&own) && cfg.agents.is_enabled(name))
        .find_map(|(name, ctor)| {
            let adapter = fallback_candidate(name, *ctor, bin);
            let uses_bin = bin.is_some_and(|b| adapter.program() == b);
            let live = adapter.ready().is_ok()
                && (uses_bin
                    || !matches!(
                        present(name, adapter.program()),
                        adapters::Liveness::Absent(_)
                    ))
                && !pace_refused(name);
            live.then_some(*name)
        });
    match other {
        Some(name) => {
            args.name = name.to_string();
            Ok(())
        }
        None => Err(message.into()),
    }
}

/// Resolve `zirv agent auto` to a harness and give an unpinned delegation the model evidence
/// picks, announcing the choice. Returns whether a model was added to `args.flags`.
fn apply_worker_routing(
    args: &mut AgentArgs,
    cfg: &CtxConfig,
    state: &StateDir,
    env: EnvLookup<'_>,
    canary: &mut Option<super::super::attribution::CandidateGuard>,
) -> CtxResult<bool> {
    let auto = args.name == routing::AUTO;
    let own_seat = (env(adapters::SEAT_ROLE_ENV).as_deref() == Some("orchestrator") && !args.force)
        .then(|| env(adapters::AGENT_ENV))
        .flatten();
    let session = super::super::mail::session_identity(env).unwrap_or_default();
    let pick = routing::route_worker(
        cfg,
        state,
        &routing::WorkerRequest {
            name: &args.name,
            prompt: &args.prompt,
            review: args.task_class == Some(super::super::log::TaskClass::Review),
            model_pinned: flags_pin_model(&args.flags),
            session: &session,
            exclude: own_seat.as_deref(),
        },
    );
    let Some(pick) = pick else {
        if auto {
            // No evidence yet: `auto` is the harness a plain run would use.
            auto_fallback(args, cfg, env, &adapters::liveness_probe, &|harness| {
                routing::pace_refuses(cfg, state, harness)
            })?;
        }
        return Ok(false);
    };
    args.name = pick.harness.clone();
    if auto && let Some(message) = same_harness_refusal(args, env) {
        return Err(message.into());
    }
    let mut routed = false;
    if let Some(model) = &pick.model {
        let adapter = adapters::select(Some(&args.name), &[], cfg)?;
        let mut flags = adapter.model_args(model);
        flags.append(&mut args.flags);
        args.flags = flags;
        routed = true;
    }
    if pick.canary {
        *canary = super::super::attribution::scope_candidate("canary");
    }
    if !args.quiet {
        eprintln!(
            "zirv ctx agent: routed to {}{} ({})",
            pick.harness,
            pick.model
                .as_deref()
                .map(|model| format!(" on {model}"))
                .unwrap_or_default(),
            pick.reason
        );
    }
    Ok(routed)
}

pub fn run_with<W: Write>(
    args: &AgentArgs,
    w: &mut W,
    repo: &Path,
    env: EnvLookup<'_>,
) -> CtxResult<i32> {
    // Resolve untrusted manifest fields before any consumer reads them (#725).
    let mut owned_args = args.clone();
    super::super::agent_manifest::apply(&mut owned_args)?;
    let args = &mut owned_args;
    validate_flags(&args.flags)?;
    validate_role(&args.role)?;
    // Resolve runtime first and reject unknown values; native names are provider routes, not adapters (#479).
    let native = resolve_runtime(args)? == super::super::runtime::RuntimeKind::Native;
    if native && args.goal.is_some() {
        return Err("--goal is available only for the harness runtime".into());
    }
    if native && args.workspace.is_some() {
        return Err("--workspace is available only for the harness runtime".into());
    }
    if !native && let Some(message) = same_harness_refusal(args, env) {
        return Err(message.into());
    }
    // Validate the result contract before spending any worker run (#318).
    let result_schema = resolve_result_schema(args)?;
    // Refuse ambiguous existing-versus-new workdir requests rather than silently choosing one (#267).
    if args.worktree && args.workdir.is_some() {
        return Err("--worktree and --workdir are mutually exclusive".into());
    }
    // Resolve state before routing, startup GC and ownership recording (#186, #319).
    let state = super::super::state::StateDir::resolve(env)?;
    // Fence superseded seats before they can coordinate another delegation (#358).
    super::super::seat::fence(&state)?;
    // Resolve workspace names and skills before allocation so invalid references cannot leave temporary trees.
    let cfg = CtxConfig::load_for_launch(repo, env)?;
    // Evidence may choose the harness (`auto`) or the model, only where nothing was pinned.
    // The canary stamp lives only as long as this run.
    let mut _canary_scope = None;
    let routed_model =
        !native && apply_worker_routing(args, &cfg, &state, env, &mut _canary_scope)?;
    let manifest_skills = if let Some(id) = args.manifest_agent.as_deref() {
        let home = env("HOME")
            .or_else(|| env("USERPROFILE"))
            .map(PathBuf::from);
        let registry = crate::commands::workflow::agents::AgentRegistry::load_for_repo(
            repo,
            home.as_deref(),
            true,
        )?;
        let manifest = registry.get(id)?;
        if manifest.manifest.read_only {
            args.mode = WorkerMode::ReadOnly;
        }
        let adapter = adapters::select(Some(&args.name), &[], &cfg)?;
        let report = crate::commands::workflow::capability::CapabilityReport::for_policy(
            adapter.name(),
            &cfg.policy,
        );
        for capability in &manifest.manifest.required_capabilities {
            if !report.support(*capability).satisfies_requirement() {
                return Err(format!(
                    "agent manifest '{id}' requires capability '{capability}' which is unavailable under the effective policy for adapter '{}'",
                    adapter.name()
                )
                .into());
            }
        }
        manifest.manifest.skills.clone()
    } else {
        Vec::new()
    };
    if args.goal.is_some() && args.mode == WorkerMode::ReadOnly {
        return Err("--goal requires a writing worker because environment preparation changes the checkout; refusing before workspace setup".into());
    }
    // Refuse before workspace setup, worktree allocation or any permit when the harness cannot enforce read-only.
    if !native && args.mode == WorkerMode::ReadOnly {
        let adapter = adapters::select(Some(&args.name), &[], &cfg)?;
        adapters::require_read_only_floor(adapter.as_ref(), adapters::LaunchMode::Headless)?;
    }
    let selected_workspace = args
        .workspace
        .as_deref()
        .map(|name| super::super::workspace::resolve(&cfg.workspace, name))
        .transpose()?;
    let workspace_requires_mcp =
        selected_workspace.is_some_and(|workspace| !workspace.mcp_servers.is_empty());
    if let Some(workspace) = selected_workspace {
        super::super::workspace::validate_skills(workspace, repo, env)?;
    }
    // Workspace setup runs before the harness sandbox; check parent authority before allocation or writer admission.
    let parent_envelope = match resolve_parent_envelope(&cfg, env) {
        Ok(envelope) => envelope,
        Err(reason) => {
            if args.json {
                let receipt =
                    launch_failure_receipt(args, None, None, None, Some(2), reason.clone(), &[]);
                print_receipt(w, &receipt)?;
            } else {
                writeln!(w, "agent: {reason}")?;
            }
            return Ok(2);
        }
    };
    if parent_envelope.delegation_depth == 0 {
        let reason =
            "this session's delegation envelope has depth 0; it may not run `zirv agent` itself";
        if args.json {
            let receipt =
                launch_failure_receipt(args, None, None, None, Some(2), reason.to_string(), &[]);
            print_receipt(w, &receipt)?;
        } else {
            writeln!(w, "agent: {reason}")?;
        }
        return Ok(2);
    }
    if let Some(workspace) = selected_workspace {
        workspace_execution_allowed(
            workspace,
            args,
            &parent_envelope,
            super::super::state::now_secs(),
        )?;
    }
    // Canonicalize once before dispatch so both paths share the validated workdir (#228, #267).
    // Startup GC remains best-effort; pool-config reread failure uses defaults instead of blocking allocation (#319, #718).
    let worktree_pool = if args.worktree {
        CtxConfig::load(repo, env)
            .map(|c| c.worktree)
            .unwrap_or_default()
    } else {
        super::super::config::WorktreeConfig::default()
    };
    let canonical_workdir = if args.worktree {
        let _ = worktree::gc(
            &state,
            repo,
            &super::super::sessions::is_alive,
            worktree_pool.idle_ttl_secs,
        );
        Some(allocate_worktree(
            &state,
            repo,
            env(adapters::SESSION_ENV).as_deref(),
            args.worktree_reuse,
            selected_workspace
                .map(|workspace| workspace.setup.as_slice())
                .unwrap_or_default(),
        )?)
    } else {
        args.workdir.as_deref().map(validate_workdir).transpose()?
    };
    // Arm immediately after allocation to cover every early return; explicit workdirs are never ours to reclaim.
    let mut worktree_guard = WorktreeReclaimGuard::new(
        &state,
        repo,
        if args.worktree {
            canonical_workdir.clone()
        } else {
            None
        },
        worktree_pool.idle_pool_max,
    );
    let prompt = resolve_prompt(&args.prompt, &mut std::io::stdin())?;
    let prompt = match selected_workspace {
        Some(workspace) => super::super::workspace::attach_skills(workspace, repo, prompt, env)?,
        None => super::super::workspace::attach_skill_refs(
            args.manifest_agent.as_deref().unwrap_or("agent manifest"),
            &manifest_skills,
            repo,
            prompt,
            env,
        )?,
    };

    // Warn about external paths before wasting a sandboxed run; this diagnostic never affects dispatch (#250).
    if canonical_workdir.is_none() {
        let home = env("HOME")
            .or_else(|| env("USERPROFILE"))
            .map(PathBuf::from);
        warn_about_paths_outside_launch_repo(&prompt, repo, home.as_deref());
    }
    // Expose the hint on both dispatch paths, except JSON stdout must remain a single object (#328, #452).
    if !args.json
        && let Some(hint) = same_harness_hint(args, env)
    {
        writeln!(w, "{hint}")?;
    }

    // Resolve requested artifacts before routing and share one prompt across both paths; missing context must fail early.
    let prompt = attach_artifact_to_prompt(args, &state, repo, prompt)?;
    // Both dispatch paths must receive the same output contract (#318).
    let prompt = attach_result_contract_to_prompt(result_schema.as_ref(), prompt);
    // Attach shared task context before dispatch and fail early for unknown cards (#317).
    let prompt = attach_task_context_to_prompt(args, &state, repo, prompt, &cfg)?;

    // Enforce adoption before any route selection or dashboard join (#223).
    if let Some(message) = adoption_enforcement_refusal(&state, repo, &cfg, env) {
        return Err(message.into());
    }

    let now = super::super::state::now_secs();
    // Claim before routing/spawn; unmet dependencies or live claimants refuse (#317).
    // Use this process PID so a failed launch is reapable, never falsely Running forever or silently Done.
    if let Some(task_id) = &args.task
        && let Err(refusal) = claim_task_for_delegation(&state, repo, &cfg, task_id, env, now)
    {
        if args.json {
            let receipt = launch_failure_receipt(
                args,
                None,
                None,
                canonical_workdir.as_deref(),
                Some(2),
                refusal.clone(),
                &[],
            );
            print_receipt(w, &receipt)?;
        } else {
            writeln!(w, "agent: {refusal}")?;
        }
        return Ok(2);
    }

    // Shared validation/allocation/claim happens once before the native fork; transfer reclaim ownership to it (#479).
    if native {
        worktree_guard.disarm();
        let launch_repo = effective_launch_repo(canonical_workdir.as_deref(), repo);
        let code = super::super::native_worker::run(
            super::super::native_worker::Request {
                args,
                prompt,
                repo,
                launch_repo,
                state: &state,
                cfg: &cfg,
                parent_envelope: &parent_envelope,
                result_schema: result_schema.as_ref(),
                provider_override: None,
            },
            w,
            env,
        );
        if args.worktree
            && let Some(path) = canonical_workdir.as_deref()
        {
            reclaim_worktree_and_report(&state, repo, path, worktree_pool.idle_pool_max);
        }
        return code;
    }

    let requested_adapter = adapters::select(Some(&args.name), &[], &cfg)?;
    let live_inherited_dashboard = env(spawnreq::DASH_REQUESTS_ENV)
        .map(PathBuf::from)
        .and_then(|path| inherited_dashboard_liveness(&path))
        .is_some_and(|liveness| matches!(liveness, super::super::sessions::OwnerLiveness::Live));
    // Report pane placement whenever any live dashboard can host it; this extra read does not decide routing.
    let seat = if live_inherited_dashboard
        || super::super::dash::select_live_dash_dir(&super::super::dash::discover_live_dash_dirs(
            &state,
        ))
        .is_some()
    {
        pace::Seat::Pane
    } else {
        pace::Seat::Cli
    };
    // Resolve pure requested-model flags before usage refresh so provider selection uses the actual model pin.
    let requested_command = headless_worker_flags(&cfg, args, requested_adapter.as_ref());
    let requested_model = adapters::last_model_flag(&requested_command);
    let mut refresh_flags = pace::PaceGateFlags::default();
    pace::refresh_sources(
        &state,
        &cfg.pace,
        now,
        requested_adapter.provider_for_model(requested_model),
        &pace::PaceGate {
            use_credits: false,
            poller: None,
            initial_launch: false,
        },
        &mut refresh_flags,
    );
    // A routed model is a default, not a pin: a reroute may still use the target's own policy.
    let source_model_explicit = flags_pin_model(&args.flags) && !routed_model;
    let bounds = super::super::fallback::TaskBounds {
        tokens: args.budget_tokens,
        tool_calls: args.max_tool_calls,
    };

    // Fallback must not bypass the orchestrator's same-harness refusal; authorized force remains exempt (#328).
    let same_harness_exclude = (env(adapters::SEAT_ROLE_ENV).as_deref() == Some("orchestrator")
        && !args.force)
        .then(|| env(adapters::AGENT_ENV))
        .flatten();

    // Exclude the requester from capacity competition so a max-active of one does not permanently drain its own dispatches.
    let requester = super::super::mail::session_identity(env);
    let mut base_excludes: Vec<&str> = same_harness_exclude.as_deref().into_iter().collect();
    // Auto-routing must never move read-only work onto a harness with no read-only floor.
    if args.mode == WorkerMode::ReadOnly {
        base_excludes.extend(adapters::floorless_adapter_names(
            adapters::LaunchMode::Headless,
        ));
    }
    let route_request = super::super::fallback::RouteRequest {
        requested: &args.name,
        source_model: requested_model,
        source_model_explicit,
        delegation: true,
        bounds,
        now,
        exclude: &base_excludes,
        requester: requester.as_deref(),
    };
    let route =
        super::super::fallback::route_new_delegation(&state, &cfg, route_request, args.force);
    // Claim the half-open route before applying it: only one probe may launch, and losing callers must replan or refuse (#455).
    let claimant = requester
        .clone()
        .unwrap_or_else(|| format!("pid-{}", std::process::id()));
    let route = match super::super::fallback::claim_route_trial(
        &state,
        &cfg,
        route_request,
        route,
        &claimant,
        args.force,
    ) {
        super::super::fallback::TrialClaim::Cleared(route) => route,
        super::super::fallback::TrialClaim::Refused(reason) => {
            return Err(format!(
                "zirv ctx agent: {reason}. Nothing else can take this work right now; retry \
                 once the trial above frees itself, or pass --force to spend on '{}' anyway.",
                args.name
            )
            .into());
        }
    };
    let mut routed_args = args.clone();
    // Every downstream path must use the canonical workdir, never the raw spelling (#228).
    routed_args.workdir = canonical_workdir.clone();
    let mut route_applied = None;
    if let Some(route) = route
        && let Ok(target_adapter) = adapters::select(Some(&route.selected), &[], &cfg)
        && let Some(flags) =
            translated_route_flags(&args.flags, target_adapter.as_ref(), &route.model)
    {
        routed_args.name = route.selected.clone();
        routed_args.flags = flags;
        let parent_session =
            super::super::mail::session_identity(env).unwrap_or_else(|| "delegation".to_string());
        let detail = route.detail(seat);
        let _ = super::super::log::append(
            &state,
            &super::super::log::Decision {
                ts: now,
                session: &parent_session,
                verb: "agent",
                verdict: "reroute",
                score: 0,
                action: "harness-reroute",
                detail: &detail,
                observed_at: route.requested_observed_at,
            },
        );
        // Use the shared capacity vocabulary for delegation and orchestrator routing records (#358).
        super::super::rollover::record_route(
            &state,
            &parent_session,
            "agent",
            now,
            &route,
            bounds.tokens,
        );
        eprintln!("zirv ctx agent: {}", automatic_route_message(&route, seat));
        route_applied = Some(route);
    }

    // Re-check the rerouted adapter with its real args: availability is only a side-effect-free pre-check.
    if !native && routed_args.mode == WorkerMode::ReadOnly {
        let adapter = adapters::select(Some(&routed_args.name), &[], &cfg)?;
        adapters::require_read_only_floor(adapter.as_ref(), adapters::LaunchMode::Headless)?;
    }

    // Validate MCP requirements against the final routed adapter and materialize before either dispatch path can spawn.
    // Existing-checkout setup takes a writer permit; newly allocated worktrees are already exclusive.
    let _workspace_ready = if let Some(workspace) = selected_workspace {
        let adapter = adapters::select(Some(&routed_args.name), &[], &cfg)?;
        let root = effective_launch_repo(routed_args.workdir.as_deref(), repo);
        let flags = headless_worker_flags(&cfg, &routed_args, adapter.as_ref());
        let materialization_permit = if workspace.requires_write() {
            let tree = std::fs::canonicalize(&root).unwrap_or_else(|_| root.clone());
            let identity = super::super::seat::env_seat_identity();
            let fence = identity
                .as_ref()
                .map(|(short, generation)| permit::SeatFence {
                    short,
                    generation: *generation,
                });
            Some(
                permit::acquire_writer(
                    &state,
                    cfg.supervise.max_writers,
                    &format!("workspace {}: {}", workspace.name, routed_args.name),
                    &tree,
                    fence,
                )
                .map_err(|refusal| {
                    permit::describe_writer_refusal(
                        &refusal,
                        &state,
                        cfg.supervise.max_writers,
                        &tree,
                    )
                })?,
            )
        } else {
            None
        };
        let ready = super::super::workspace::materialize(
            workspace,
            &state,
            &root,
            adapter.as_ref(),
            &flags,
            env,
        )?;
        drop(materialization_permit);
        Some(ready)
    } else {
        None
    };

    // Headroom ranks new work; it must not refuse or delay an unstarted delegation (#358).
    // Resolve the effective routed model once so usage, reservation and settlement share the same provider.
    let effective_model = effective_delegation_model(route_applied.as_ref(), requested_model);
    let provider = adapters::provider_for_agent_and_model(Some(&routed_args.name), effective_model);
    let (collector, estimator) = pace::current_windows(&state, &cfg.pace, now, provider);
    let gate = pace::spawn_gate(&collector, estimator.as_ref(), now, &cfg.pace);
    let reading_age = pace::spawn_headroom(&collector, estimator.as_ref(), now, &cfg.pace)
        .map(|reading| reading.age_secs);
    let gate_note = pace::describe_spawn_gate(&gate, reading_age);
    if let Some(note) = gate_note.as_deref() {
        eprintln!("zirv ctx agent: {note}");
    }
    if matches!(gate, pace::SpawnGate::Refuse { .. }) {
        // Quota attention belongs to the requester and is informational; it does not prevent this spawn (#349).
        if let Some(short) = super::super::mail::session_identity(env) {
            let _ = super::super::attention::record(
                &state,
                &short,
                super::super::attention::Observation::new(
                    super::super::attention::Authority::Supervisor,
                    gate_note
                        .clone()
                        .unwrap_or_else(|| "usage at the spawn ceiling".to_string()),
                    80,
                    super::super::state::now_secs(),
                )
                .with_attention(super::super::attention::Attention::Quota),
                super::super::state::now_secs(),
            );
        }
        // Only announce requested-harness pressure when no healthier reroute was applied.
        if route_applied.is_none() {
            eprintln!(
                "zirv ctx agent: usage at the ceiling on {}; launching anyway -- the provider \
                 may refuse, in which case the worker is rerouted or parked",
                routed_args.name
            );
        }
    } else if let Some(short) = super::super::mail::session_identity(env) {
        // Clear stale requester quota attention when this reading no longer reaches the ceiling (#349).
        let _ = super::super::attention::record(
            &state,
            &short,
            super::super::attention::Observation::new(
                super::super::attention::Authority::Supervisor,
                "usage pacing allowed new work",
                80,
                super::super::state::now_secs(),
            )
            .with_attention(super::super::attention::Attention::None),
            super::super::state::now_secs(),
        );
    }

    // Resolve one group binding before dispatch; only newly minted groups may be unwound on non-start paths (#170).
    let mut args = routed_args;
    let minted_group = resolve_group_binding(&mut args, &state, env)?;
    let args = &args;
    let discard_minted_group = || {
        if let Some(id) = &minted_group {
            super::super::group::discard_if_unused(&state, id);
        }
    };

    // Warn after routing but before either launch path so the warning names the actual harness.
    if let Some(warning) = codex_read_only_build_warning(&args.name, args.mode) {
        eprintln!("zirv ctx agent: {warning}");
    } else if cfg!(windows)
        && args.name.eq_ignore_ascii_case("codex")
        && let Some(warning) = codex_worktree_sandbox_warning(
            &args.name,
            adapters::git_dirs(&effective_launch_repo(args.workdir.as_deref(), repo)),
            cfg!(windows),
        )
    {
        eprintln!("{warning}");
    }

    // Synchronous internal callers and goal preparation stay inline to consume completion before proceeding (#733).
    // JSON suppresses human stdout; receipts use structured AnswerFacts, never captured text (#452).
    let mut dash_buf: Vec<u8> = Vec::new();
    let dispatch = if args.inline || args.goal.is_some() {
        if args.goal.is_some() {
            eprintln!(
                "zirv ctx agent: --goal preparation runs synchronously; running this delegation inline"
            );
        }
        Dispatch::Inline {
            no_dashboard: false,
        }
    } else if workspace_requires_mcp {
        eprintln!(
            "zirv ctx agent: workspace MCP requirements bind this launch to the validated harness; \
             running inline with cross-harness fallback disabled"
        );
        Dispatch::Inline {
            no_dashboard: false,
        }
    } else if args.json {
        try_join_dashboard(
            args,
            &prompt,
            &mut dash_buf,
            repo,
            env,
            DASH_ACK_TIMEOUT,
            DASH_CLAIM_EXTENSION,
            result_schema.as_ref(),
        )
    } else {
        try_join_dashboard(
            args,
            &prompt,
            w,
            repo,
            env,
            DASH_ACK_TIMEOUT,
            DASH_CLAIM_EXTENSION,
            result_schema.as_ref(),
        )
    };
    match dispatch {
        Dispatch::Inline { no_dashboard } => {
            if no_dashboard {
                eprintln!("{}", inline_notice(&args.name));
            }
        }
        Dispatch::Answered(result, facts) => {
            // Discard only unused minted groups on non-success; a late admission may safely refuse if cleanup wins the race.
            if matches!(result, Ok(0) | Ok(EXIT_DASH_UNCONFIRMED)) {
                // Disarm on confirmed or unconfirmed admission: the dashboard may still spawn into this tree.
                // Leaving a clean tree is safer than deleting it under a live spawn; dashboard exit owns reclamation.
                worktree_guard.disarm();
            }
            if !matches!(result, Ok(0)) {
                discard_minted_group();
            }
            if args.json
                && let Ok(code) = &result
            {
                let receipt = dashboard_answer_receipt(args, effective_model, *code, &facts);
                print_receipt(w, &receipt)?;
            }
            return result;
        }
    }

    let announcer = Announcer::new(
        cfg.chrome.events && !args.quiet,
        console::colors_enabled_stderr(),
    );
    // Read parent identity from the original session env, never an inherited grandparent id (#249).
    let worker_parent = super::super::mail::session_identity(env)
        .filter(|id| super::super::prompt::is_addressable_short(id));
    // Export the resolved group into the child launch env so lineage survives inline delegation.
    let quieted = quiet_env(env, args.quiet);
    let grouped = group_env(&quieted, args.group.clone());
    let parented = parent_session_env(&grouped, worker_parent);
    let delegated = |key: &str| {
        if workspace_requires_mcp && key == "ZIRV_CTX_FALLBACK" {
            Some("false".to_string())
        } else if key == super::super::fallback::DELEGATION_ENV {
            Some(
                if source_model_explicit {
                    "explicit-model"
                } else {
                    "implicit-model"
                }
                .to_string(),
            )
        } else {
            parented(key)
        }
    };
    // Export this delegation's result contract to its child (#318).
    let env = result_schema_env(
        &delegated,
        result_schema.as_ref().map(Schema::to_canonical_json),
    );

    // Select the explicit target to compute launch flags; empty command input cannot change a named adapter.
    let adapter = adapters::select(Some(&args.name), &[], &cfg)?;
    // Compute worker cwd before writable-root flags so permissions follow the worker's target, not the delegator (#228).
    let launch_repo = effective_launch_repo(args.workdir.as_deref(), repo);
    let command = with_headless_extra_writable_roots(
        headless_worker_flags(&cfg, args, adapter.as_ref()),
        adapter.as_ref(),
        &launch_repo,
        &state,
    );
    // Read the effective argv so the winning explicit or default model is recorded accurately.
    let model = adapters::last_model_flag(&command).map(str::to_string);
    // Evaluate the same pure launch policy once for result and mail warnings (#230).
    // Print with synchronous stdout results so callers neither miss stderr-only warnings nor receive duplicates.
    let capability_warnings = policy::evaluate(
        &cfg.policy,
        adapter.as_ref(),
        adapters::LaunchMode::Headless,
    )
    .degraded_capabilities();
    let worker_session = args
        .session_id
        .clone()
        .unwrap_or_else(|| SessionId::new_v4().to_string());
    // First claimant alone may auto-close a group; best-effort ownership never replaces budget admission checks (#170).
    if args.role.as_deref() == Some("sub-orchestrator")
        && let Some(id) = &args.group
    {
        let _ = super::super::group::claim_sub_orchestrator(
            &state,
            id,
            &super::super::sessions::short_id(&worker_session),
        );
    }
    // Resolve before launch so unknown or closed groups cannot run unbounded (#155).
    let (worker_budget, reserved_ceiling) = match resolve_worker_budget(&env, args) {
        Ok(result) => result,
        Err(e) => {
            // No launch means newly minted groups must be unwound.
            discard_minted_group();
            if super::super::group::is_admission_exhausted(e.as_ref()) {
                let code = exec::EXIT_BUDGET_EXHAUSTED;
                if args.json {
                    let receipt = launch_failure_receipt(
                        args,
                        model.as_deref(),
                        Some(&worker_session),
                        Some(&launch_repo),
                        Some(code),
                        e.to_string(),
                        &capability_warnings,
                    );
                    print_receipt(w, &receipt)?;
                } else {
                    writeln!(w, "{}: {e}", delegation_outcome(code))?;
                }
                return Ok(code);
            }
            if args.json {
                let receipt = launch_failure_receipt(
                    args,
                    model.as_deref(),
                    Some(&worker_session),
                    Some(&launch_repo),
                    None,
                    e.to_string(),
                    &capability_warnings,
                );
                print_receipt(w, &receipt)?;
            }
            return Err(e);
        }
    };
    // Atomically check/reserve provider headroom under one lock; stale placement snapshots can overcommit (#358).
    // Release every failed launch and settle completion; ledger errors remain best-effort rather than aborting committed work.
    let limit_tokens = pace::headroom_limit_tokens(&collector, estimator.as_ref(), now, &cfg.pace);
    let reservation_id = match super::super::reservation::reserve_within(
        &state,
        provider,
        &worker_session,
        worker_budget.tokens.unwrap_or(0),
        limit_tokens,
        super::super::state::now_secs(),
    ) {
        Ok(Ok(reservation)) => Some(reservation.id),
        Ok(Err(outstanding)) => {
            // Run unreserved on accounting limits: rerouting here would mismatch the adapter and argv already committed to.
            eprintln!(
                "zirv ctx agent: provider '{provider}' is at its projected headroom limit \
                 ({outstanding} tokens already outstanding); running unreserved rather than \
                 refusing"
            );
            None
        }
        Err(e) => {
            eprintln!(
                "zirv ctx agent: failed to record a token reservation for provider \
                 '{provider}': {e}"
            );
            None
        }
    };
    // Release on every pre-launch failure; idempotent best-effort cleanup must precede fallible reporting.
    let release_reservation = || {
        if let Some(id) = &reservation_id {
            let _ = super::super::reservation::release(&state, provider, id);
        }
    };
    let settle_initial_reservation = |actual| {
        if let Some(id) = args.group.as_deref() {
            let _ = super::super::group::settle_reservation(
                &state,
                id,
                reserved_ceiling.unwrap_or(0),
                actual,
            );
        }
        if let Some(id) = &reservation_id {
            let _ = super::super::reservation::settle(&state, provider, id, actual);
        }
    };

    // Narrow the child grant before spawning; any requested authority growth hard-errors, never silently clamps (#262).
    let child_short = super::super::sessions::short_id(&worker_session);
    let principal = format!("{}/{}", parent_envelope.principal, child_short);
    let requested_envelope = requested_envelope_from_args(
        args,
        &parent_envelope,
        principal.clone(),
        worker_budget.tokens,
    );
    let mut child_envelope =
        match envelope::WorkerEnvelope::narrow(&parent_envelope, &requested_envelope) {
            Ok(envelope) => envelope,
            Err(err) => {
                discard_minted_group();
                release_reservation();
                if args.json {
                    let receipt = launch_failure_receipt(
                        args,
                        model.as_deref(),
                        Some(&worker_session),
                        Some(&launch_repo),
                        Some(2),
                        format!("delegation envelope refused: {err}"),
                        &capability_warnings,
                    );
                    print_receipt(w, &receipt)?;
                } else {
                    writeln!(w, "failed: delegation envelope refused: {err}")?;
                }
                return Ok(2);
            }
        };
    // Writing workers require an exclusive checkout permit; read-only workers never take one (#267).
    // Retain it through execution, contract audit and retry.
    let writer_permit = if args.mode == WorkerMode::Writing {
        let tree = std::fs::canonicalize(&launch_repo).unwrap_or_else(|_| launch_repo.clone());
        // Use strict seat fencing: an uncommitted successor must not grant a writer lease before rollover commits (#543).
        let identity = super::super::seat::env_seat_identity();
        let fence = identity
            .as_ref()
            .map(|(short, generation)| permit::SeatFence {
                short,
                generation: *generation,
            });
        match permit::acquire_writer(
            &state,
            cfg.supervise.max_writers,
            &format!(
                "session {}: {}",
                super::super::sessions::short_id(&worker_session),
                args.name
            ),
            &tree,
            fence,
        ) {
            Ok(writer_permit) => Some(writer_permit),
            Err(refusal) => {
                // No worker launched, so unwind only this invocation's minted group and reservation.
                discard_minted_group();
                release_reservation();
                let reason = permit::describe_writer_refusal(
                    &refusal,
                    &state,
                    cfg.supervise.max_writers,
                    &tree,
                );
                // Record refusal on the requester; the prevented worker has no session attention row (#349).
                if let Some(short) = super::super::mail::session_identity(&env) {
                    let _ = super::super::attention::record(
                        &state,
                        &short,
                        super::super::attention::Observation::new(
                            super::super::attention::Authority::Supervisor,
                            reason.clone(),
                            80,
                            super::super::state::now_secs(),
                        )
                        .with_attention(super::super::attention::Attention::WriterConflict),
                        super::super::state::now_secs(),
                    );
                }
                let code = exec::EXIT_WRITER_BUSY;
                if args.json {
                    let receipt = launch_failure_receipt(
                        args,
                        model.as_deref(),
                        Some(&worker_session),
                        Some(&launch_repo),
                        Some(code),
                        reason,
                        &capability_warnings,
                    );
                    print_receipt(w, &receipt)?;
                } else {
                    writeln!(w, "{reason}")?;
                }
                return Ok(code);
            }
        }
    } else {
        None
    };

    // Worker cwd and sandbox use launch_repo; routing config and requester identity retain the delegator repo (#228).

    let bootstrap_usage = if args.goal.is_some() {
        match run_goal_bootstrap(
            args,
            &cfg,
            &state,
            &launch_repo,
            &parent_envelope,
            child_envelope.token_budget,
            &env,
        ) {
            Ok(usage) => usage,
            Err(error) => {
                drop(writer_permit);
                if let Some(usage) = &error.usage {
                    settle_initial_reservation(token_spend(usage));
                } else {
                    if let Some(id) = &args.group {
                        super::super::group::rollback_admission(
                            &state,
                            id,
                            reserved_ceiling.unwrap_or(0),
                        );
                    }
                    release_reservation();
                }
                discard_minted_group();
                finish_task_card(
                    &state,
                    repo,
                    &cfg,
                    args,
                    super::super::task::ExitKind::Crash,
                    ("goal bootstrap failed", None),
                    super::super::state::now_secs(),
                );
                if error.exit_code == Some(exec::EXIT_BUDGET_EXHAUSTED) {
                    let code = exec::EXIT_BUDGET_EXHAUSTED;
                    if args.json {
                        let receipt = launch_failure_receipt(
                            args,
                            model.as_deref(),
                            Some(&worker_session),
                            Some(&launch_repo),
                            Some(code),
                            error.to_string(),
                            &capability_warnings,
                        );
                        print_receipt(w, &receipt)?;
                    } else {
                        writeln!(w, "{}: {error}", delegation_outcome(code))?;
                    }
                    return Ok(code);
                }
                if args.json {
                    let receipt = launch_failure_receipt(
                        args,
                        model.as_deref(),
                        Some(&worker_session),
                        Some(&launch_repo),
                        Some(2),
                        error.to_string(),
                        &capability_warnings,
                    );
                    print_receipt(w, &receipt)?;
                    return Ok(2);
                }
                return Err(error.into());
            }
        }
    } else {
        TranscriptUsage::default()
    };
    let bootstrap_spend = token_spend(&bootstrap_usage);
    let remaining_budget = child_envelope
        .token_budget
        .map(|limit| limit.saturating_sub(bootstrap_spend));
    if child_envelope.token_budget.is_some() && remaining_budget == Some(0) {
        drop(writer_permit);
        settle_initial_reservation(bootstrap_spend);
        discard_minted_group();
        let code = exec::EXIT_BUDGET_EXHAUSTED;
        let reason =
            "goal bootstrap spent the delegation token budget; main worker was not launched";
        finish_task_card(
            &state,
            repo,
            &cfg,
            args,
            super::super::task::ExitKind::Crash,
            (reason, None),
            super::super::state::now_secs(),
        );
        if args.json {
            let receipt = launch_failure_receipt(
                args,
                model.as_deref(),
                Some(&worker_session),
                Some(&launch_repo),
                Some(code),
                reason.to_string(),
                &capability_warnings,
            );
            print_receipt(w, &receipt)?;
        } else {
            writeln!(w, "{}: {reason}", delegation_outcome(code))?;
        }
        return Ok(code);
    }
    child_envelope.token_budget = remaining_budget;
    let envelope_json = envelope::canonical_json(&child_envelope)
        .ok()
        .filter(|s| !s.is_empty());
    let env = envelope_env(&env, envelope_json, Some(principal.clone()));

    let exec_args = ExecArgs {
        agent: Some(args.name.clone()),
        session_id: Some(worker_session.clone()),
        transcript: None,
        // Keep prompts as data, never command argv where flag-shaped text could change launch options.
        prompt: Some(prompt),
        max_restarts: args.max_restarts,
        timeout_secs: args.timeout_secs,
        budget_tokens: remaining_budget,
        max_tool_calls: worker_budget.tool_calls,
        // Workers inherit durable repository objectives through compilation; agent exposes no objective flag (#285).
        objective: None,
        // Retain identical policy/model/writable-root flags for the bounded contract resume (#318).
        command: command.clone(),
        simple: false,
        // Carry the reservation id so mid-run handover can move it to the correct provider (#358).
        reservation_id: reservation_id.clone(),
        cancellation: args.cancellation.clone(),
        ..Default::default()
    };

    announcer.emit(&Event::DelegatedStart {
        agent: args.name.clone(),
    });
    let started = std::time::Instant::now();
    // A failed spawn must roll back prior group admission so work that never ran consumes no slot.
    // JSON stdout stays one receipt; supervisor pacing/restart notices belong on stderr.
    let execution = if args.json {
        let mut stderr = std::io::stderr();
        exec::run_with_report(&exec_args, &mut stderr, &launch_repo, &env)
    } else {
        exec::run_with_report(&exec_args, w, &launch_repo, &env)
    };
    let (mut code, execution_report) = match execution {
        Ok(result) => result,
        Err(e) => {
            if args.goal.is_some() {
                settle_initial_reservation(bootstrap_spend);
            } else {
                if let Some(id) = &args.group {
                    super::super::group::rollback_admission(
                        &state,
                        id,
                        reserved_ceiling.unwrap_or(0),
                    );
                }
                release_reservation();
            }
            // After admission rollback, remove only groups this invocation minted for an unstarted launch.
            discard_minted_group();
            // An unstarted worker follows crash/respawn policy, never silent Done (#317).
            finish_task_card(
                &state,
                repo,
                &cfg,
                args,
                super::super::task::ExitKind::Crash,
                ("launch failed", None),
                super::super::state::now_secs(),
            );
            if args.json {
                let receipt = launch_failure_receipt(
                    args,
                    model.as_deref(),
                    Some(&worker_session),
                    Some(&launch_repo),
                    None,
                    e.to_string(),
                    &capability_warnings,
                );
                print_receipt(w, &receipt)?;
            }
            return Err(e);
        }
    };
    // Close only the group this coordinator actually claimed; concurrent users of a shared group may still have work (#170).
    if args.role.as_deref() == Some("sub-orchestrator")
        && let Some(id) = &args.group
        && let Ok(Some(group)) = super::super::group::load(&state, id)
        && group.sub_orchestrator_session.as_deref()
            == Some(super::super::sessions::short_id(&worker_session).as_str())
    {
        let _ = super::super::group::close(&state, id, super::super::state::now_secs());
    }
    let wall_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);

    // Accounting must never fail completed work; record each vendor-backed segment to avoid mischarging continuation (#155, #186).
    // Default receipt state carries no report until extraction succeeds (#452).
    let mut delegation_state = DelegationState::ExitedNoReport;
    let mut result_path: Option<PathBuf> = None;
    let mut report_truncated = false;
    let mut mail_delivered = false;
    let mut contract_errors: Vec<String> = Vec::new();
    // Without a state directory, there is no safety log from which to report blocks.
    let mut blocked_families: Vec<String> = Vec::new();

    if let Ok(state_dir) = super::super::state::StateDir::resolve(&env) {
        // Read blocks for the original worker identity so a later cross-harness segment cannot hide earlier denials.
        blocked_families = blocked_family_lines(&state_dir, &worker_session);
        let parent_session = super::super::mail::session_identity(&env).unwrap_or_default();
        // Derive task completion from actual report outcomes; nonzero exits crash and clean exits without reports are SilentZero (#317, #722).
        let task_exit_kind: Option<super::super::task::ExitKind>;

        // Use one final-text extraction path for contracted and plain delegations (#452).
        let repo_slug = super::super::state::repo_slug(repo);
        let final_session = execution_report
            .segments
            .last()
            .map(|segment| segment.session.clone())
            .unwrap_or_else(|| worker_session.clone());
        let final_agent_name = execution_report
            .segments
            .last()
            .map(|segment| segment.agent.clone())
            .unwrap_or_else(|| args.name.clone());
        // Reselect only after a harness change so extraction uses the adapter that actually finished.
        let reselected = if final_agent_name == args.name {
            None
        } else {
            adapters::select(Some(&final_agent_name), &[], &cfg).ok()
        };
        let result_adapter: &dyn AgentAdapter =
            reselected.as_deref().unwrap_or_else(|| adapter.as_ref());
        let session_ref = SessionId::parse(&final_session);
        let session_ref_full = SessionRef {
            id: session_ref.clone(),
            cwd: launch_repo.clone(),
        };
        let transcript_path = result_adapter.transcript_path(&session_ref_full);
        let read_last_text = |path: &Path| -> Option<String> {
            let jsonl = std::fs::read_to_string(path).unwrap_or_default();
            result_adapter
                .structural_context(&jsonl, 1)
                .assistant_texts
                .last()
                .cloned()
        };
        let first_text = read_last_text(&transcript_path);

        // Declared contracts require extracted, validated text, never synthetic success from exit zero (#318).
        // Always mail the result or failure so asynchronous requesters receive the same completion evidence.
        if let Some(schema) = &result_schema {
            let mut attempts: Vec<Vec<String>> = Vec::new();
            let mut last_candidate = String::new();
            let mut validated: Option<serde_json::Value> = None;
            let mut undeclared = Vec::new();

            if let Some(text) = first_text.as_deref() {
                last_candidate = result_schema::extract_json_candidate(text).unwrap_or_default();
            }
            match first_text.as_deref() {
                Some(text) => match evaluate_report(schema, text, &launch_repo, &mut undeclared) {
                    Ok(value) => validated = Some(value),
                    Err(errors) => attempts.push(errors),
                },
                None => attempts.push(vec![
                    "no JSON object found in the worker's final message".to_string(),
                ]),
            }

            // Retry only with resume support and a recoverable real session; an argv builder alone cannot prove resumability (#303).
            if validated.is_none()
                && let Some(first_errors) = attempts.first()
                && result_adapter
                    .headless_resume_cmd(Some("probe"), &final_session, &[])
                    .is_some()
                && result_adapter.resume_target(&session_ref_full).is_some()
            {
                let retry_prompt = result_schema::build_retry_message(first_errors);
                let timeout = Duration::from_secs(args.timeout_secs.unwrap_or(300));
                let mut retry_env: Vec<(String, String)> = vec![(
                    adapters::AGENT_ENV.to_string(),
                    result_adapter.name().to_string(),
                )];
                if let Some(group) = env(WORK_GROUP_ENV).filter(|id| !id.is_empty()) {
                    retry_env.push((WORK_GROUP_ENV.to_string(), group));
                }
                if let Some(parent) = env(PARENT_SESSION_ENV) {
                    retry_env.push((PARENT_SESSION_ENV.to_string(), parent));
                }
                match run_contract_retry(
                    result_adapter,
                    &session_ref,
                    &command,
                    &retry_prompt,
                    &launch_repo,
                    timeout,
                    &retry_env,
                ) {
                    Ok(()) => {
                        let second_text = read_last_text(&transcript_path);
                        if let Some(text) = second_text.as_deref() {
                            last_candidate = result_schema::extract_json_candidate(text)
                                .unwrap_or(last_candidate);
                        }
                        match second_text.as_deref() {
                            Some(text) => {
                                match evaluate_report(schema, text, &launch_repo, &mut undeclared) {
                                    Ok(value) => validated = Some(value),
                                    Err(errors) => attempts.push(errors),
                                }
                            }
                            None => attempts.push(vec![
                                "no JSON object found in the worker's final message".to_string(),
                            ]),
                        }
                    }
                    Err(e) => attempts.push(vec![format!("retry could not run: {e}")]),
                }
            }

            if validated.is_none() {
                code = exec::EXIT_CONTRACT_FAILED;
            }
            task_exit_kind = Some(if validated.is_some() {
                super::super::task::ExitKind::Reported
            } else {
                super::super::task::ExitKind::SilentZero
            });
            delegation_state = if validated.is_some() {
                DelegationState::ReportedValidated
            } else {
                DelegationState::ReportedContractFailed
            };
            contract_errors = attempts.last().cloned().unwrap_or_default();

            let (stored_report, stored_truncated) = cap_report(first_text.as_deref());
            report_truncated = stored_truncated;
            let path = store_result(
                &state_dir,
                repo,
                &worker_session,
                &args.name,
                &validated,
                &attempts,
                &undeclared,
                stored_report.as_deref(),
                stored_truncated,
            );
            result_path = Some(path.clone());

            if !args.json {
                // Keep this line stable for exact-match consumers; report paths belong on a separate following line (#452).
                writeln!(
                    w,
                    "result: {}",
                    if validated.is_some() {
                        "validated".to_string()
                    } else {
                        format!(
                            "contract_failed ({} errors)",
                            attempts.last().map(Vec::len).unwrap_or(0)
                        )
                    }
                )?;
                writeln!(w, "full report: {}", path.display())?;
            }

            let mut body = format!(
                "zirv ctx agent: {} finished: {} (exit {code})",
                args.name,
                exec::describe_exit(code)
            );
            if let Some(value) = &validated {
                let pretty = serde_json::to_string_pretty(value).unwrap_or_default();
                body.push_str(&format!("\nresult:\n```json\n{pretty}\n```"));
            } else {
                body.push_str("\ncontract_failed:");
                for (i, errors) in attempts.iter().enumerate() {
                    for error in errors {
                        body.push_str(&format!("\n- attempt {}: {error}", i + 1));
                    }
                }
                body.push_str(&format!(
                    "\n\nraw candidate:\n{}",
                    cap_bytes(&last_candidate, 2048)
                ));
            }
            if !undeclared.is_empty() {
                body.push_str(&format!("\nundeclared changes: {}", undeclared.join(", ")));
            }
            // Link the full stored report from mail without embedding its unbounded raw text (#452).
            body.push_str(&format!("\nfull report: {}", path.display()));
            let to_session = super::super::mail::session_identity(&env)
                .filter(|id| super::super::prompt::is_addressable_short(id));
            let msg = super::super::mail::Message {
                from_session: worker_session.clone(),
                from_agent: args.name.clone(),
                to: "any".to_string(),
                to_session,
                sent: super::super::state::now_secs(),
                body,
            };
            mail_delivered =
                super::super::mail::store_to(&state_dir, &repo_slug, &repo_slug, &msg, &cfg)
                    .is_ok();
        } else {
            // Persist uncontracted final text so mail/JSON-only callers still have a durable report (#452).
            delegation_state = match first_text.as_deref() {
                Some(_) => DelegationState::Reported,
                None => DelegationState::ExitedNoReport,
            };
            // Nonzero exit always means Crash, even with text; clean exit without usable text is SilentZero, never Reported (#722).
            task_exit_kind = Some(if code != 0 {
                super::super::task::ExitKind::Crash
            } else if first_text.is_some() {
                super::super::task::ExitKind::Reported
            } else {
                super::super::task::ExitKind::SilentZero
            });
            if let Some(text) = first_text.as_deref() {
                let (stored_report, stored_truncated) = cap_report(Some(text));
                report_truncated = stored_truncated;
                let path = store_report_only(
                    &state_dir,
                    repo,
                    &worker_session,
                    &args.name,
                    stored_report.as_deref().unwrap_or_default(),
                    stored_truncated,
                );
                if !args.json {
                    writeln!(w, "{}", no_contract_result_line(Some(&path), code))?;
                }
                result_path = Some(path);
            } else {
                // Persist even an absent report so silent exits retain a durable post-mortem and result path (#722).
                let path = write_delegation_result(
                    &state_dir,
                    repo,
                    &worker_session,
                    &args.name,
                    "exited_no_report",
                    &None,
                    &[],
                    &[],
                    None,
                    false,
                );
                if !args.json {
                    writeln!(w, "{}", no_contract_result_line(None, code))?;
                }
                result_path = Some(path);
            }

            // A supervisor consult reports through its own ruling and fallback path, never as worker mail.
            if code != 0
                && env(super::super::supervisor::CONSULT_ENV).is_none()
                && let Some(parent_short) = super::super::mail::session_identity(&env)
                && super::super::prompt::is_addressable_short(&parent_short)
            {
                // Supervisor failure mail covers children unable to self-report; match stderr reasons and never fail completed work on mail errors (#227).
                let msg = report_back_message(
                    code,
                    &worker_session,
                    &args.name,
                    &parent_short,
                    &capability_warnings,
                );
                mail_delivered =
                    super::super::mail::store_to(&state_dir, &repo_slug, &repo_slug, &msg, &cfg)
                        .is_ok();
            }
        }
        let outcome = delegation_outcome(code);
        let envelope_sha256 = envelope::digest(&child_envelope).ok();
        // Finish the task only after the real report outcome is known; never silently mark it Done (#317).
        if let Some(exit_kind) = task_exit_kind {
            let failure_signals = (exit_kind == super::super::task::ExitKind::Crash)
                .then(|| {
                    first_text
                        .as_deref()
                        .or_else(|| contract_errors.first().map(String::as_str))
                        .map(super::super::task::CrashSignals::from_text)
                })
                .flatten();
            finish_task_card(
                &state_dir,
                repo,
                &cfg,
                args,
                exit_kind,
                (outcome, failure_signals),
                super::super::state::now_secs(),
            );
        }
        let total = append_execution_segments(
            &state_dir,
            &execution_report,
            &parent_session,
            args.group.as_deref(),
            code,
            outcome,
            args.mode,
            args.task_class,
            &principal,
            envelope_sha256.as_deref(),
        );
        let aggregate_spend = bootstrap_spend.saturating_add(token_spend(&total));
        if let Some(id) = args.group.as_deref() {
            let _ = super::super::group::settle_reservation(
                &state_dir,
                id,
                reserved_ceiling.unwrap_or(0),
                aggregate_spend,
            );
        }
        // Settle the final reservation after provider handover; the original pair may name a ledger already left behind (#358).
        let settle_reservation = execution_report
            .final_reservation
            .as_ref()
            .map(|(id, provider)| (id.as_str(), *provider))
            .or_else(|| reservation_id.as_deref().map(|id| (id, provider)));
        if let Some((id, provider)) = settle_reservation {
            let _ = super::super::reservation::settle(&state_dir, provider, id, aggregate_spend);
        }
        let route: Vec<String> = execution_report
            .segments
            .iter()
            .map(|segment| match segment.model.as_deref() {
                Some(model) => format!("{} ({model})", segment.agent),
                None => format!("{} (default model)", segment.agent),
            })
            .collect();

        let route = if route.is_empty() {
            format!(
                "{} ({})",
                args.name,
                model.as_deref().unwrap_or("default worker model")
            )
        } else {
            route.join(" -> ")
        };
        let detail = format!(
            "{route}: {} in / {} cache-creation / {} cache-read / {} out in {}ms -- {}",
            total.input_tokens,
            total.cache_creation_input_tokens,
            total.cache_read_input_tokens,
            total.output_tokens,
            wall_ms,
            outcome,
        );
        let _ = super::super::log::append(
            &state_dir,
            &super::super::log::Decision {
                ts: super::super::state::now_secs(),
                session: &worker_session,
                verb: "agent",
                verdict: "n/a",
                score: 0,
                action: super::super::log::DELEGATION_ACTION,
                detail: &detail,
                observed_at: None,
            },
        );
    }

    announcer.emit(&Event::DelegatedFinish {
        agent: args.name.clone(),
        meaning: exec::describe_exit(code),
    });
    if let Some(note) = exit_note(code) {
        eprintln!("zirv ctx agent: {note}");
    }

    // Keep the checkout and writer permit through contract auditing and retry.
    drop(writer_permit);
    if args.worktree
        && let Some(path) = canonical_workdir.as_deref()
    {
        reclaim_worktree_and_report(&state, repo, path, worktree_pool.idle_pool_max);
    }
    worktree_guard.disarm();

    if args.json {
        let receipt = DelegationReceipt {
            schema_version: 1,
            harness: args.name.clone(),
            runtime: super::super::runtime::RuntimeKind::Harness.as_str(),
            delegation: None,
            model,
            mode: DelegationMode::Inline,
            state: delegation_state,
            exit_code: Some(code),
            session: Some(worker_session.clone()),
            task: args.task.clone(),
            workdir: Some(launch_repo.clone()),
            result_path,
            report_truncated,
            mail_delivered,
            errors: contract_errors,
            capability_warnings: capability_warning_lines(&capability_warnings),
            blocked_families,
            reason: None,
            note: receipt_note(delegation_state),
        };
        print_receipt(w, &receipt)?;
    } else {
        // Expose full capability warnings with synchronous stdout results (#230).
        for warning in &capability_warnings {
            writeln!(
                w,
                "capability warning: {} -- {}: {}",
                warning.capability, warning.mechanism, warning.detail
            )?;
        }
        // Expose blocked families with the result so the requester needs no separate status query.
        for line in &blocked_families {
            writeln!(w, "blocked: {line}")?;
        }
    }

    Ok(code)
}

// Keep independent completion-record inputs explicit; a wrapper would obscure the call sites (#264).
#[allow(clippy::too_many_arguments)]
fn append_execution_segments(
    state: &super::super::state::StateDir,
    report: &exec::ExecutionReport,
    parent_session: &str,
    work_group_id: Option<&str>,
    exit_code: i32,
    outcome: &'static str,
    mode: WorkerMode,
    task_class: Option<super::super::log::TaskClass>,
    principal: &str,
    envelope_sha256: Option<&str>,
) -> TranscriptUsage {
    let mut total = TranscriptUsage::default();
    for segment in &report.segments {
        total.input_tokens = total
            .input_tokens
            .saturating_add(segment.usage.input_tokens);
        total.cache_creation_input_tokens = total
            .cache_creation_input_tokens
            .saturating_add(segment.usage.cache_creation_input_tokens);
        total.cache_read_input_tokens = total
            .cache_read_input_tokens
            .saturating_add(segment.usage.cache_read_input_tokens);
        total.output_tokens = total
            .output_tokens
            .saturating_add(segment.usage.output_tokens);
        let _ = super::super::log::append_delegation(
            state,
            &super::super::log::Delegation {
                ts: super::super::state::now_secs(),
                session: &segment.session,
                parent_session,
                work_group_id,
                agent: &segment.agent,
                model: segment.model.as_deref(),
                input_tokens: segment.usage.input_tokens,
                cache_creation_input_tokens: segment.usage.cache_creation_input_tokens,
                cache_read_input_tokens: segment.usage.cache_read_input_tokens,
                output_tokens: segment.usage.output_tokens,
                wall_ms: segment.wall_ms,
                exit_code,
                outcome,
                mode: Some(mode),
                task_class,
                principal,
                envelope_sha256,
            },
        );
    }
    total
}

pub fn run<W: Write>(args: &AgentArgs, w: &mut W) -> CtxResult<i32> {
    let repo = std::env::current_dir()?;
    let env = env_from_process();
    // Resolve operator runtime defaults at CLI entry so downstream code sees an explicit backend for this role (#491).
    let choice = super::super::runtime::resolve_for_cli(
        &args.runtime,
        &repo,
        &env,
        args.role.as_deref().unwrap_or("worker"),
    )?;
    if let Some(note) = &choice.note {
        eprintln!("zirv ctx agent: {note}");
    }
    let mut args = args.clone();
    args.runtime = choice.kind.as_str().to_string();
    run_with(&args, w, &repo, &env)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::ctx::state::StateDir;
    use crate::commands::ctx::{fallback, window};
    #[cfg(windows)]
    use std::collections::HashMap;
    use std::path::PathBuf;

    use super::super::tests::*;

    #[test]
    fn auto_without_evidence_avoids_the_seats_own_harness_when_another_is_live() {
        let env = env_map(&[
            (adapters::SEAT_ROLE_ENV, "orchestrator"),
            (adapters::AGENT_ENV, "claude"),
        ]);
        let cfg = CtxConfig::default();
        let mut args = args_for(routing::AUTO, "go");
        auto_fallback(
            &mut args,
            &cfg,
            &|k| env.get(k).cloned(),
            &adapters::everything_installed(),
            &|_| false,
        )
        .expect("another harness is live");
        assert_eq!(args.name, "codex");

        let mut args = args_for(routing::AUTO, "go");
        let err = auto_fallback(
            &mut args,
            &cfg,
            &|k| env.get(k).cloned(),
            &adapters::only_installed(&["claude"]),
            &|_| false,
        )
        .expect_err("no other harness is installed");
        assert!(err.to_string().contains("own harness"), "{err}");
    }

    #[test]
    fn auto_fallback_skips_a_usage_refused_harness() {
        let env = env_map(&[
            (adapters::SEAT_ROLE_ENV, "orchestrator"),
            (adapters::AGENT_ENV, "claude"),
        ]);
        let mut args = args_for(routing::AUTO, "go");
        auto_fallback(
            &mut args,
            &CtxConfig::default(),
            &|k| env.get(k).cloned(),
            &adapters::everything_installed(),
            &|harness| harness == "codex",
        )
        .expect("another harness is live");
        assert_ne!(args.name, "codex");
        assert_ne!(args.name, "claude");

        let mut args = args_for(routing::AUTO, "go");
        let err = auto_fallback(
            &mut args,
            &CtxConfig::default(),
            &|k| env.get(k).cloned(),
            &adapters::everything_installed(),
            &|harness| harness != "claude",
        )
        .expect_err("every other harness is usage-refused");
        assert!(err.to_string().contains("own harness"), "{err}");
    }

    #[test]
    fn auto_fallback_does_not_launch_another_adapters_agent_bin() {
        let env = env_map(&[
            (adapters::SEAT_ROLE_ENV, "orchestrator"),
            (adapters::AGENT_ENV, "claude"),
        ]);
        let bin = Some("/opt/claude/claude");
        let build = |name: &str| {
            let (_, ctor) = adapters::ADAPTERS
                .iter()
                .find(|(n, _)| *n == name)
                .expect("adapter");
            fallback_candidate(name, *ctor, bin)
        };
        assert_eq!(build("claude").program(), "/opt/claude/claude");
        assert_ne!(build("codex").program(), "/opt/claude/claude");

        // A foreign candidate keeps its own program, so it must be present to be chosen.
        let cfg = CtxConfig {
            agent_bin: bin.map(str::to_string),
            ..CtxConfig::default()
        };
        let mut args = args_for(routing::AUTO, "go");
        auto_fallback(
            &mut args,
            &cfg,
            &|k| env.get(k).cloned(),
            &adapters::everything_installed(),
            &|_| false,
        )
        .expect("codex is built with its own program and is installed");
        assert_eq!(args.name, "codex");
    }

    #[test]
    fn auto_fallback_with_agent_bin_still_requires_a_foreign_candidate_present() {
        let env = env_map(&[
            (adapters::SEAT_ROLE_ENV, "orchestrator"),
            (adapters::AGENT_ENV, "claude"),
        ]);
        let cfg = CtxConfig {
            agent_bin: Some("/opt/claude/claude".to_string()),
            ..CtxConfig::default()
        };
        let mut args = args_for(routing::AUTO, "go");
        let err = auto_fallback(
            &mut args,
            &cfg,
            &|k| env.get(k).cloned(),
            &adapters::only_installed(&["claude"]),
            &|_| false,
        )
        .expect_err("codex is absent and agent_bin does not reach it");
        assert!(err.to_string().contains("own harness"), "{err}");
    }

    /// Issue #358 (T9): usage headroom never blocks a spawn -- renamed from
    /// `only_a_refusal_is_overridable_and_only_by_force`, which used to pin
    /// `spawn_blocked` (now deleted: nothing in `run_with` gates on
    /// `SpawnGate` any more) as the single place a `Refuse` stopped a
    /// delegation, overridable only by `--force`. Now `Refuse` carries
    /// exactly the same operational weight as `Warn` and `Proceed` -- none
    /// of them stop anything -- with or without `--force`, which is why this
    /// test no longer has a `force` parameter to pin either.
    #[test]
    fn a_refuse_gate_never_blocks_a_spawn() {
        let refuse = pace::SpawnGate::Refuse {
            window: "five_hour",
            percent: 97.0,
            source: pace::Source::Collector,
        };
        // `describe_spawn_gate` still names the ceiling for the operator,
        // but the note is informational: nothing downstream reads `matches!
        // (gate, SpawnGate::Refuse { .. })` as a reason to return `Err`.
        let note = pace::describe_spawn_gate(&refuse, None).expect("a note for the ceiling");
        assert!(note.contains("at the spawn ceiling"), "got {note}");
        assert!(!note.contains("refusing"), "got {note}");
    }

    /// Issue #358 (T9): renamed from `a_stale_dashboard_env_yields_the_cli_
    /// override_hint`, which used to pin `run_with`'s hard refusal (and its
    /// `--force`-specific wording) on exactly this 96%-usage setup. Now the
    /// same setup must reach the launch path regardless -- with or without
    /// `--force`, which is why `args.force` is exercised both ways here
    /// rather than pinning one value the way the old override-hint assertion
    /// implicitly did.
    #[test]
    fn a_spawn_at_the_ceiling_launches_anyway_with_or_without_force() {
        let tmp = crate::commands::ctx::testenv::repo();
        let home = tmp.path().join("home");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);
        let state_dir = tmp.path().join("state");
        let state = StateDir::from_root(state_dir.clone());
        let now = crate::commands::ctx::state::now_secs();
        window::store_for(
            &state,
            "anthropic",
            &window::UsageWindows {
                five_hour: Some(window::Window {
                    used_percentage: 96.0,
                    resets_at: now + 600,
                    observed_at: now,
                    overage_covered: false,
                    limit_reached: false,
                }),
                seven_day: None,
            },
        )
        .expect("store source usage");
        let mut env = base_env(&state_dir);
        env.insert("ZIRV_CTX_FALLBACK".to_string(), "false".to_string());
        env.insert(
            crate::commands::ctx::dash::spawnreq::DASH_REQUESTS_ENV.to_string(),
            tmp.path()
                .join("missing-dashboard-requests")
                .display()
                .to_string(),
        );

        for force in [false, true] {
            let mut args = joinable_args("claude", "go");
            args.force = force;
            let result = run_with(&args, &mut Vec::new(), tmp.path(), &|key| {
                env.get(key).cloned()
            });
            assert!(
                result.is_ok(),
                "usage at the ceiling must never refuse the spawn (force={force}): {result:?}"
            );
        }
    }

    #[test]
    fn delegation_refreshes_requested_codex_usage_before_gating_and_routing() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let home = tempfile::tempdir().expect("home tempdir");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());
        let rollout_dir = home.path().join(".codex/sessions/2026/09/04");
        std::fs::create_dir_all(&rollout_dir).expect("rollout dir");
        std::fs::write(
            rollout_dir.join(
                "rollout-2026-09-04T07-56-26-01a06afd-63c2-7061-8bdf-2798fe10b9e2.jsonl",
            ),
            include_str!(
                "../../../../tests/fixtures/codex-rollouts/2026/09/04/rollout-2026-09-04T07-56-26-01a06afd-63c2-7061-8bdf-2798fe10b9e2.jsonl"
            ),
        )
        .expect("rollout fixture");

        let state_dir = tmp.path().join("state");
        let state = StateDir::from_root(state_dir.clone());
        window::store_for(
            &state,
            window::CODEX_USAGE_PROVIDER,
            &window::UsageWindows {
                five_hour: None,
                seven_day: Some(window::Window {
                    used_percentage: 99.0,
                    resets_at: 1_788_758_370,
                    observed_at: 1_788_423_353,
                    overage_covered: false,
                    limit_reached: false,
                }),
            },
        )
        .expect("stale reading");
        let mut env = base_env(&state_dir);
        env.insert("ZIRV_CTX_FALLBACK".to_string(), "false".to_string());
        let args = args_for("codex", "go");
        let result = run_with(&args, &mut Vec::new(), tmp.path(), &|key| {
            env.get(key).cloned()
        });
        assert!(
            result.is_ok(),
            "the refreshed reading must let delegation reach its launch path: {result:?}"
        );

        let mut cfg = CtxConfig {
            agent_bin: Some(
                std::env::current_exe()
                    .expect("current test executable")
                    .display()
                    .to_string(),
            ),
            ..CtxConfig::default()
        };
        cfg.pace.estimator = false;
        let now = 1_788_501_500;
        let (collector, estimator) =
            pace::current_windows(&state, &cfg.pace, now, window::CODEX_USAGE_PROVIDER);
        assert_eq!(
            pace::spawn_gate(&collector, estimator.as_ref(), now, &cfg.pace),
            pace::SpawnGate::Proceed
        );
        assert_eq!(
            fallback::route_new_delegation(
                &state,
                &cfg,
                fallback::RouteRequest {
                    requested: "codex",
                    source_model: Some("gpt-5.6-terra"),
                    source_model_explicit: false,
                    delegation: true,
                    bounds: fallback::TaskBounds {
                        tokens: None,
                        tool_calls: None,
                    },
                    now,
                    exclude: &[],
                    requester: None,
                },
                false,
            ),
            None
        );
    }

    #[test]
    fn delegation_ignores_an_expired_stale_codex_reading_when_refresh_finds_no_rollout() {
        let tmp = crate::commands::ctx::testenv::repo();
        let home = tempfile::tempdir().expect("home tempdir");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());
        let state_dir = tmp.path().join("state");
        let state = StateDir::from_root(state_dir.clone());
        let now = crate::commands::ctx::state::now_secs();
        window::store_for(
            &state,
            window::CODEX_USAGE_PROVIDER,
            &window::UsageWindows {
                five_hour: None,
                seven_day: Some(window::Window {
                    used_percentage: 99.0,
                    resets_at: now,
                    observed_at: now.saturating_sub(901),
                    overage_covered: false,
                    limit_reached: false,
                }),
            },
        )
        .expect("store expired reading");
        window::store_for(
            &state,
            window::LEGACY_USAGE_PROVIDER,
            &window::UsageWindows {
                five_hour: Some(window::Window {
                    used_percentage: 10.0,
                    resets_at: now + 3_600,
                    observed_at: now,
                    overage_covered: false,
                    limit_reached: false,
                }),
                seven_day: None,
            },
        )
        .expect("store alternate reading");
        let mut env = base_env(&state_dir);
        env.insert(
            "ZIRV_CTX_AGENT_BIN".to_string(),
            format!("sh {}", fixture("fake-codex-agent.sh").display()),
        );
        env.insert("ZIRV_CTX_PACE_ESTIMATOR".to_string(), "false".to_string());
        let args = args_for("codex", "go");
        let mut out = Vec::new();
        let result = run_with(&args, &mut out, tmp.path(), &|key| env.get(key).cloned());

        assert_eq!(result.expect("delegation runs"), 0);
        let output = String::from_utf8(out).expect("utf8");
        assert!(!output.contains("automatically routed"), "got {output}");
        let decisions = crate::commands::ctx::log::tail(&state, 20).expect("decisions");
        assert!(
            !decisions
                .iter()
                .any(|line| line.contains("\"action\":\"harness-reroute\"")),
            "no reroute decision expected: {decisions:?}"
        );
        let delegations = crate::commands::ctx::log::read_delegations(&state, 10);
        assert_eq!(delegations.len(), 1, "one worker ran: {delegations:?}");
        assert_eq!(delegations[0].agent, "codex");
    }

    #[test]
    fn exhausted_group_admission_returns_the_budget_exit_and_outcome() {
        let tmp = crate::commands::ctx::testenv::repo();
        let home = tmp.path().join("home");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);
        let state_path = tmp.path().join("state");
        let state = crate::commands::ctx::state::StateDir::from_root(state_path.clone());
        create_work_group_with_spend(&state, "wg-spent", 400_000, 400_000);
        let mut env = base_env(&state_path);
        env.insert("ZIRV_CTX_PACE".to_string(), "false".to_string());
        let mut args = args_for("claude", "go");
        args.group = Some("wg-spent".to_string());
        let mut out = Vec::new();

        let code = run_with(&args, &mut out, tmp.path(), &|k| env.get(k).cloned())
            .expect("budget exhaustion is a structured exit");

        assert_eq!(code, exec::EXIT_BUDGET_EXHAUSTED);
        assert_eq!(delegation_outcome(code), "budget-exhausted");
    }

    /// Re-review (2026-08-27) finding 1: a delegation that admits into a
    /// group and then fails before a child is genuinely launched must not
    /// permanently burn that admission slot. `--max-tool-calls` with the
    /// codex adapter is refused by `exec::run_with` itself (issue #155
    /// review finding C2) strictly AFTER `resolve_worker_budget` has already
    /// admitted the child into its group -- exactly this finding's failure
    /// window.
    #[test]
    fn a_failed_delegation_after_admission_rolls_back_the_group_slot() {
        let tmp = crate::commands::ctx::testenv::repo();
        let home = tmp.path().join("home");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);
        let state_path = tmp.path().join("state");
        let state = crate::commands::ctx::state::StateDir::from_root(state_path.clone());
        crate::commands::ctx::group::create(&state, &sample_work_group("wg-1", 3, 0))
            .expect("create group");

        let env = base_env(&state_path);
        let mut args = args_for("codex", "go");
        args.group = Some("wg-1".to_string());
        args.max_tool_calls = Some(5);

        let mut out = Vec::new();
        let err = run_with(&args, &mut out, tmp.path(), &|k| env.get(k).cloned())
            .expect_err("codex + --max-tool-calls is refused by exec::run_with");
        assert!(err.to_string().contains("--max-tool-calls"), "got {err}");

        assert_eq!(
            crate::commands::ctx::group::load(&state, "wg-1")
                .expect("load")
                .expect("present")
                .admitted_children,
            0,
            "the failed delegation must not have permanently burned the group slot"
        );
    }

    /// Issue #358 (task T3): the same failed launch above must also release
    /// this delegation's own provider-level token reservation -- the pane-
    /// less, headless-only mirror of `group::rollback_admission`'s own
    /// group-slot rollback, written by `run_with` right after `resolve_
    /// worker_budget` succeeds and released on the identical `exec::
    /// run_with_report` failure path.
    #[test]
    fn a_failed_delegation_after_admission_releases_the_provider_reservation() {
        let tmp = crate::commands::ctx::testenv::repo();
        let home = tmp.path().join("home");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);
        let state_path = tmp.path().join("state");
        let state = crate::commands::ctx::state::StateDir::from_root(state_path.clone());
        crate::commands::ctx::group::create(&state, &sample_work_group("wg-1", 3, 0))
            .expect("create group");

        let env = base_env(&state_path);
        let mut args = args_for("codex", "go");
        args.group = Some("wg-1".to_string());
        args.max_tool_calls = Some(5);

        let mut out = Vec::new();
        let err = run_with(&args, &mut out, tmp.path(), &|k| env.get(k).cloned())
            .expect_err("codex + --max-tool-calls is refused by exec::run_with");
        assert!(err.to_string().contains("--max-tool-calls"), "got {err}");

        let provider = crate::commands::ctx::adapters::provider_for_agent_name(Some("codex"));
        assert_eq!(
            crate::commands::ctx::reservation::outstanding(&state, provider, 1_700_000_000),
            0,
            "the failed launch must have released its own provider reservation"
        );
        assert!(
            crate::commands::ctx::reservation::entries(&state, provider).is_empty(),
            "the released reservation must be gone from the ledger, not merely excluded"
        );
    }

    /// A successful delegation still counts exactly once against its group --
    /// the rollback added for the failure path above must never also undo a
    /// genuine admission for a child that actually ran.
    #[test]
    fn a_successful_delegation_still_counts_exactly_one_admission() {
        let tmp = crate::commands::ctx::testenv::repo();
        let home = tmp.path().join("home");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);
        let state_path = tmp.path().join("state");
        let state = crate::commands::ctx::state::StateDir::from_root(state_path.clone());
        crate::commands::ctx::group::create(&state, &sample_work_group("wg-1", 3, 0))
            .expect("create group");

        let env = base_env(&state_path);
        let mut args = args_for("claude", "go");
        args.group = Some("wg-1".to_string());

        let mut out = Vec::new();
        let code = run_with(&args, &mut out, tmp.path(), &|k| env.get(k).cloned())
            .expect("a plain claude delegation with no --max-tool-calls runs cleanly");
        assert_eq!(code, 0);

        assert_eq!(
            crate::commands::ctx::group::load(&state, "wg-1")
                .expect("load")
                .expect("present")
                .admitted_children,
            1,
            "a successful spawn must still count exactly one admission"
        );
    }

    /// Finding #11 (issue #358 review): admitting this delegation's own
    /// token ceiling would push the provider's ledger past its projected
    /// headroom (a comfortable 1000-token budget, 95% already used, this
    /// delegation asking for 600 more) -- `reserve_within` must refuse to
    /// write that reservation, but the delegation itself must still run:
    /// usage headroom ranks and ceiling-checks a delegation, it never
    /// refuses one outright (issue #358 T9's own rule, one layer up).
    #[test]
    fn a_delegation_over_the_ledger_limit_runs_unreserved_rather_than_refusing() {
        let tmp = crate::commands::ctx::testenv::repo();
        let home = tmp.path().join("home");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);
        let state_dir = tmp.path().join("state");
        let state = StateDir::from_root(state_dir.clone());
        let now = crate::commands::ctx::state::now_secs();
        window::store_for(
            &state,
            "anthropic",
            &window::UsageWindows {
                five_hour: Some(window::Window {
                    used_percentage: 95.0,
                    resets_at: now + 600,
                    observed_at: now,
                    overage_covered: false,
                    limit_reached: false,
                }),
                seven_day: None,
            },
        )
        .expect("store usage");

        let mut env = base_env(&state_dir);
        // Audit finding G3: this used to name `ZIRV_CTX_PACE_FIVE_HOUR_
        // BUDGET_TOKENS`, which `ENV_MAP` has never heard of -- so the budget
        // stayed 0, `pace::headroom_limit_tokens` returned `None`, the ceiling
        // check was disabled outright and this test passed without ever
        // reaching `reserve_within`'s refusal. `ZIRV_CTX_FIVE_HOUR_BUDGET` is
        // the real name (see `config::every_pace_env_name_referenced_in_the_
        // crate_exists_in_env_map`).
        env.insert("ZIRV_CTX_FIVE_HOUR_BUDGET".to_string(), "1000".to_string());
        // Only the collector reading above may decide this: an estimator
        // layer would be a second source for the same ceiling.
        env.insert("ZIRV_CTX_PACE_ESTIMATOR".to_string(), "false".to_string());
        // Rerouting is orthogonal to this test: with cross-harness fallback
        // on, claude's own low headroom here would otherwise steer this
        // delegation onto codex before reservation is ever reached.
        env.insert("ZIRV_CTX_FALLBACK".to_string(), "false".to_string());
        let mut args = args_for("claude", "go");
        // 5% headroom of a 1000-token budget is 50 tokens; this delegation's
        // own ceiling asks for far more than that -- large enough to also
        // clear the fake agent's own reported usage, so the run genuinely
        // completes rather than being stopped by an unrelated budget-
        // exhausted check.
        args.budget_tokens = Some(500_000);

        // The ceiling is real before the run: 5% headroom of a 1000-token
        // budget, which this delegation's own 500k ceiling cannot fit. Pinned
        // here so a future change that silently disables the check again
        // (a renamed variable, a zeroed budget) fails on this line rather
        // than passing vacuously the way this test did before G3.
        let pace_cfg =
            crate::commands::ctx::config::CtxConfig::load(tmp.path(), &|k| env.get(k).cloned())
                .expect("load")
                .pace;
        let (collector, estimator) =
            crate::commands::ctx::pace::current_windows(&state, &pace_cfg, now, "anthropic");
        assert_eq!(
            crate::commands::ctx::pace::headroom_limit_tokens(
                &collector,
                estimator.as_ref(),
                now,
                &pace_cfg,
            ),
            Some(50),
            "the reservation ceiling must actually be configured, not None"
        );

        let mut out = Vec::new();
        let code = run_with(&args, &mut out, tmp.path(), &|k| env.get(k).cloned())
            .expect("the delegation must still run, not be refused");
        assert_eq!(code, 0);

        assert!(
            crate::commands::ctx::reservation::entries(&state, "anthropic").is_empty(),
            "an over-limit admission must leave no reservation behind, not a wrong-amount one"
        );
    }

    /// Strips the `\\?\` extended-length prefix `std::fs::canonicalize` adds
    /// on Windows and normalises separators/case, so a path git-bash's own
    /// `pwd -W` reported (plain drive-letter form, forward slashes) can be
    /// compared against one this test built with `Path::join` without either
    /// side's own formatting quirks producing a false mismatch.
    fn normalize_path_for_compare(p: &std::path::Path) -> String {
        let s = p.display().to_string();
        let stripped = s.strip_prefix(r"\\?\").unwrap_or(&s);
        stripped.replace('\\', "/").to_lowercase()
    }

    /// Issue #328, end to end: the refusal fires inside `run_with` itself,
    /// before `--worktree` ever allocates anything.
    #[test]
    fn run_with_refuses_same_harness_delegation_before_allocating_a_worktree() {
        if !git_available() {
            eprintln!("skipping: git not found on PATH");
            return;
        }
        let tmp = tempfile::tempdir().expect("tempdir");
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(&repo).expect("mkdir");
        assert!(git_init(&repo), "git init");
        let home = tmp.path().join("home");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);

        let mut env = base_env(&tmp.path().join("state"));
        env.insert(
            adapters::SEAT_ROLE_ENV.to_string(),
            "orchestrator".to_string(),
        );
        env.insert(adapters::AGENT_ENV.to_string(), "claude".to_string());

        let mut args = args_for("claude", "go");
        args.worktree = true;
        let mut out = Vec::new();
        let err = run_with(&args, &mut out, &repo, &|k| env.get(k).cloned())
            .expect_err("same-harness delegation from an orchestrator seat must be refused");
        assert!(err.to_string().contains("own harness"), "got {err}");
        assert!(
            !repo.join(".zirv").join("worktrees").exists(),
            "the refusal must land before any worktree is allocated"
        );
    }

    /// Issue #267: allocating a fresh tree and being told to use a specific
    /// existing one are two different requests -- `run_with` must refuse
    /// the ambiguous combination up front rather than silently picking one.
    #[test]
    fn run_with_rejects_worktree_and_workdir_together() {
        let tmp = crate::commands::ctx::testenv::repo();
        let home = tmp.path().join("home");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);
        let env = base_env(&tmp.path().join("state"));

        let mut args = args_for("claude", "go");
        args.worktree = true;
        args.workdir = Some(tmp.path().to_path_buf());
        let mut out = Vec::new();
        let err = run_with(&args, &mut out, tmp.path(), &|k| env.get(k).cloned())
            .expect_err("--worktree and --workdir together must be refused");
        assert!(err.to_string().contains("mutually exclusive"), "got {err}");
    }

    /// Issue #228, decision 1: a bad `--workdir` fails loudly, up front,
    /// before any spawn decision -- not as a confusing sandbox error deep
    /// inside a harness's own child process.
    #[test]
    fn run_with_rejects_a_missing_workdir_up_front() {
        let tmp = crate::commands::ctx::testenv::repo();
        let home = tmp.path().join("home");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);
        let env = base_env(&tmp.path().join("state"));

        let mut args = args_for("claude", "go");
        args.workdir = Some(tmp.path().join("does-not-exist"));
        let mut out = Vec::new();
        let err = run_with(&args, &mut out, tmp.path(), &|k| env.get(k).cloned())
            .expect_err("a missing --workdir must fail up front");
        assert!(err.to_string().contains("--workdir"), "got {err}");
    }

    /// The exact user-facing wording issue #228 specifies, exercised through
    /// the full `run_with` entry point rather than only `validate_workdir`
    /// directly.
    #[test]
    fn run_with_rejects_a_workdir_with_no_git_ancestry_with_the_exact_wording() {
        if !git_available() {
            eprintln!("skipping: git not found on PATH");
            return;
        }
        let tmp = crate::commands::ctx::testenv::repo();
        let home = tmp.path().join("home");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);
        let env = base_env(&tmp.path().join("state"));
        let not_a_repo = tmp.path().join("plain-dir");
        std::fs::create_dir_all(&not_a_repo).expect("mkdir");

        let mut args = args_for("claude", "go");
        args.workdir = Some(not_a_repo);
        let mut out = Vec::new();
        let err = run_with(&args, &mut out, tmp.path(), &|k| env.get(k).cloned())
            .expect_err("a non-repo --workdir must be refused");
        let msg = err.to_string();
        assert!(
            msg.contains(
                "is not inside a git repository; zirv agent workers need a repository \
                          checkout"
            ),
            "got {msg}"
        );
    }

    /// Issue #267, acceptance criterion: a second `--mode writing` worker
    /// dispatched into a tree that already has a live writer permit is
    /// refused with a one-line, retryable reason -- before any adapter is
    /// ever launched (`code == exec::EXIT_WRITER_BUSY`, not an `Err`, the
    /// same structured-exit discipline `EXIT_BUDGET_EXHAUSTED` already
    /// uses).
    #[test]
    fn run_with_refuses_a_second_writing_worker_into_a_tree_with_a_live_writer() {
        let tmp = crate::commands::ctx::testenv::repo();
        let home = tmp.path().join("home");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);
        let state_path = tmp.path().join("state");
        let env = base_env(&state_path);
        let state = StateDir::from_root(state_path);

        // The SAME canonicalisation `run_with`'s own writer-permit check
        // applies to `launch_repo` (here, `repo` itself -- no `--workdir`).
        let tree = std::fs::canonicalize(tmp.path()).expect("canonicalize");
        let held = permit::acquire_writer(&state, 1, "worker-a", &tree, None)
            .expect("writer permit pre-held for the test");

        let args = args_for("claude", "go");
        let mut out = Vec::new();
        let code = run_with(&args, &mut out, tmp.path(), &|k| env.get(k).cloned())
            .expect("a writer-busy refusal is a structured exit, not an Err");
        assert_eq!(code, exec::EXIT_WRITER_BUSY);
        let text = String::from_utf8(out).expect("utf8");
        assert!(
            text.contains("writer-busy"),
            "the outcome must be structured, not just \"failed\": got {text}"
        );
        assert!(
            text.contains("worker-a"),
            "the busy holder's own label must be named: got {text}"
        );

        drop(held);
    }

    /// Issue #543: `run_with` fences its writer lease on this process's seat identity, so an
    /// uncommitted successor generation is refused before any adapter is launched.
    #[test]
    fn run_with_refuses_a_writer_lease_for_an_uncommitted_seat_generation() {
        use crate::commands::ctx::runtime::RuntimeKind;
        use crate::commands::ctx::seat;

        let tmp = crate::commands::ctx::testenv::repo();
        let home = tmp.path().join("home");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);
        let state_path = tmp.path().join("state");
        let env = base_env(&state_path);
        let state = StateDir::from_root(state_path);

        let session = "7b1a2c3d-9999-4000-8000-000000000543";
        let short = crate::commands::ctx::sessions::short_id(session);
        seat::register(
            &state,
            &short,
            session,
            "native",
            None,
            "anthropic",
            "orchestrator",
            false,
            1,
        )
        .expect("register");
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
        let _vars = crate::commands::ctx::testenv::VarGuard::set(&[
            (crate::commands::ctx::adapters::SESSION_ENV, Some(session)),
            (seat::GENERATION_ENV, Some(&prepared.to_string())),
        ]);

        let args = args_for("claude", "go");
        let mut out = Vec::new();
        let code = run_with(&args, &mut out, tmp.path(), &|k| env.get(k).cloned())
            .expect("a writer refusal is a structured exit, not an Err");
        assert_eq!(code, exec::EXIT_WRITER_BUSY);
        let text = String::from_utf8(out).expect("utf8");
        assert!(text.contains("uncommitted seat generation"), "got {text}");
        assert_eq!(permit::live_writer_records(&state).len(), 0);
    }

    /// Audit finding G4: the same refusal, one invariant deeper. `run_with`
    /// reserves this delegation's token ceiling against its PROVIDER before
    /// the writer permit is even asked for, promising (at the reservation
    /// itself) to release it "on every failure path between here and a
    /// genuinely running child". This path discarded the minted group and
    /// returned without ever releasing, so a refused delegation left its
    /// whole ceiling outstanding against the provider until the process
    /// exited -- `dash::fulfill_spawn_request` already handled the identical
    /// refusal through `rollback_admission`, which does release.
    #[test]
    fn a_writer_permit_refusal_releases_the_provider_reservation() {
        let tmp = crate::commands::ctx::testenv::repo();
        let home = tmp.path().join("home");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);
        let state_path = tmp.path().join("state");
        let env = base_env(&state_path);
        let state = StateDir::from_root(state_path);

        let tree = std::fs::canonicalize(tmp.path()).expect("canonicalize");
        let held = permit::acquire_writer(&state, 1, "worker-a", &tree, None)
            .expect("writer permit pre-held for the test");

        let args = args_for("claude", "go");
        let mut out = Vec::new();
        let code = run_with(&args, &mut out, tmp.path(), &|k| env.get(k).cloned())
            .expect("a writer-busy refusal is a structured exit, not an Err");
        assert_eq!(code, exec::EXIT_WRITER_BUSY);

        let outstanding = crate::commands::ctx::reservation::entries(&state, "anthropic");
        assert!(
            outstanding.is_empty(),
            "a refusal that never launched a child must leave nothing reserved: {outstanding:?}"
        );

        drop(held);
    }

    /// A read-only worker on a harness with no read-only floor is refused
    /// before any run state, worktree or permit exists.
    #[test]
    fn run_with_refuses_a_read_only_worker_on_an_empty_floor_harness_before_any_side_effect() {
        let tmp = crate::commands::ctx::testenv::repo();
        let home = tmp.path().join("home");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);
        let state_path = tmp.path().join("state");
        let env = base_env(&state_path);

        let mut args = args_for("cursor-agent", "go");
        args.mode = WorkerMode::ReadOnly;
        args.worktree = true;
        let mut out = Vec::new();
        let err = run_with(&args, &mut out, tmp.path(), &|k| env.get(k).cloned())
            .expect_err("an empty read-only floor must refuse the launch");
        let message = err.to_string();
        assert!(message.contains("'cursor-agent'"), "{message}");
        assert!(message.contains("claude"), "{message}");
        assert!(out.is_empty(), "nothing is reported as launched");
        assert!(
            !tmp.path().join(".zirv").join("worktrees").exists(),
            "no worktree may be allocated"
        );
    }

    /// The other half: a `--mode read-only` worker never takes a writer
    /// permit, so it must never be refused by another tree's live writer --
    /// exercised through `run_with` itself, not just `permit::
    /// acquire_writer` in isolation.
    #[test]
    fn run_with_never_refuses_a_read_only_worker_for_writer_contention() {
        let tmp = crate::commands::ctx::testenv::repo();
        let home = tmp.path().join("home");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);
        let state_path = tmp.path().join("state");
        let env = base_env(&state_path);
        let state = StateDir::from_root(state_path);

        let tree = std::fs::canonicalize(tmp.path()).expect("canonicalize");
        let held = permit::acquire_writer(&state, 1, "worker-a", &tree, None)
            .expect("writer permit pre-held for the test");

        let mut args = args_for("claude", "go");
        args.mode = WorkerMode::ReadOnly;
        let mut out = Vec::new();
        let code = run_with(&args, &mut out, tmp.path(), &|k| env.get(k).cloned())
            .expect("a read-only worker must never hit the writer-busy refusal");
        assert_ne!(
            code,
            exec::EXIT_WRITER_BUSY,
            "read-only must never take a writer permit"
        );

        drop(held);
    }

    /// Review finding (2026-09), finding 2b: `allocate_worktree` runs before
    /// admission, so a refusal that happens AFTER allocation -- here, the
    /// writer pool itself exhausted by some OTHER tree's live writer, never
    /// the freshly-allocated one -- must still reclaim the clean, unused
    /// worktree it allocated rather than leak it under `.zirv/worktrees/`
    /// forever.
    #[test]
    fn run_with_reclaims_a_worktree_left_behind_by_a_post_allocation_refusal() {
        if !git_available() {
            eprintln!("skipping: git not found on PATH");
            return;
        }
        let tmp = crate::commands::ctx::testenv::repo();
        let home = tmp.path().join("home");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);
        let state_path = tmp.path().join("state");
        let mut env = base_env(&state_path);
        env.insert(
            "ZIRV_CTX_SUPERVISE_MAX_WRITERS".to_string(),
            "1".to_string(),
        );
        let state = StateDir::from_root(state_path);

        assert!(git_init(tmp.path()), "git init");
        let run = |args: &[&str]| -> bool {
            std::process::Command::new("git")
                .arg("-C")
                .arg(tmp.path())
                .args(args)
                .output()
                .map(|o| o.status.success())
                .unwrap_or(false)
        };
        assert!(run(&["config", "user.email", "test@example.com"]));
        assert!(run(&["config", "user.name", "test"]));
        std::fs::write(tmp.path().join("README.md"), "hello\n").expect("write");
        assert!(run(&["add", "README.md"]));
        assert!(run(&["commit", "-q", "-m", "initial"]));

        // Exhausts the explicitly configured writer pool of 1 with a writer holding some
        // OTHER tree -- `--worktree` always allocates a fresh, never-before-
        // seen tree, so this is `WriterRefusal::PoolExhausted`, never
        // `TreeBusy`, proving the refusal is genuinely unrelated to the
        // worktree `run_with` is about to allocate.
        let elsewhere = tmp.path().join("elsewhere");
        std::fs::create_dir_all(&elsewhere).expect("mkdir");
        let elsewhere = std::fs::canonicalize(&elsewhere).expect("canonicalize");
        let held = permit::acquire_writer(&state, 1, "worker-a", &elsewhere, None)
            .expect("writer permit pre-held for the test");

        let mut args = args_for("claude", "go");
        args.worktree = true;
        let mut out = Vec::new();
        let code = run_with(&args, &mut out, tmp.path(), &|k| env.get(k).cloned())
            .expect("a writer-busy refusal is a structured exit, not an Err");
        assert_eq!(code, exec::EXIT_WRITER_BUSY);

        let worktrees_root = tmp
            .path()
            .join(crate::utils::SCRIPT_DIR_NAME)
            .join("worktrees");
        let leftover: Vec<_> = std::fs::read_dir(&worktrees_root)
            .map(|entries| entries.flatten().collect::<Vec<_>>())
            .unwrap_or_default();
        assert!(
            leftover.is_empty(),
            "the allocated worktree must be reclaimed on a post-allocation refusal, got \
             {leftover:?}"
        );

        drop(held);
    }

    /// Issue #716: a workspace's MCP list is a hard dependency checked after
    /// final adapter resolution and before either dashboard or headless spawn.
    /// The Claude adapter's readiness probe may invoke the fake binary with
    /// `--help`; a real worker invocation is the one carrying `--session-id`,
    /// and none may occur when the named server is absent.
    #[test]
    fn workspace_missing_mcp_server_refuses_before_worker_spawn() {
        let tmp = crate::commands::ctx::testenv::repo();
        let home = tmp.path().join("home");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);
        std::fs::create_dir_all(tmp.path().join(".zirv")).expect("mkdir");
        std::fs::write(
            tmp.path().join(".zirv/ctx.toml"),
            "[[workspace]]\nname = \"needs-linear\"\nmcp_servers = [\"linear\"]\n",
        )
        .expect("workspace config");
        let argv_log = tmp.path().join("argv.log");
        unsafe {
            std::env::set_var("FAKE_AGENT_ARGV_LOG", &argv_log);
        }

        let env = base_env(&tmp.path().join("state"));
        let args = AgentArgs {
            workspace: Some("needs-linear".into()),
            ..args_for("claude", "go")
        };
        let error = run_with(&args, &mut Vec::new(), tmp.path(), &|key| {
            env.get(key).cloned()
        })
        .expect_err("missing MCP server must refuse");
        unsafe {
            std::env::remove_var("FAKE_AGENT_ARGV_LOG");
        }

        assert!(
            error
                .to_string()
                .contains("no configured MCP server(s): linear"),
            "{error}"
        );
        let invocations = std::fs::read_to_string(&argv_log).unwrap_or_default();
        assert!(
            invocations
                .lines()
                .all(|line| !line.contains("--session-id")),
            "the worker launched despite the missing MCP dependency: {invocations}"
        );
    }

    #[test]
    fn restricted_envelopes_refuse_executable_workspace_before_worktree_or_setup() {
        if cfg!(windows) {
            return;
        }
        let tmp = crate::commands::ctx::testenv::repo();
        let home = tmp.path().join("home");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);
        std::fs::create_dir_all(home.join(".zirv")).expect("mkdir");
        let marker = tmp.path().join("workspace-ran");
        std::fs::write(
            home.join(".zirv/ctx.toml"),
            format!(
                "[[workspace]]\nname = \"setup\"\nsetup = [\"touch {}\"]\n",
                marker.display()
            ),
        )
        .expect("operator workspace config");
        let base_parent = root_envelope(&CtxConfig::default());
        let base_args = AgentArgs {
            workspace: Some("setup".into()),
            worktree: true,
            ..args_for("claude", "go")
        };
        let now = crate::commands::ctx::state::now_secs();
        let mut cases: Vec<(&str, envelope::WorkerEnvelope, AgentArgs, bool)> = Vec::new();

        let mut parent = base_parent.clone();
        parent.destructive = false;
        cases.push(("non-destructive", parent, base_args.clone(), false));
        let mut parent = base_parent.clone();
        parent.network = false;
        cases.push(("parent network", parent, base_args.clone(), false));
        let mut parent = base_parent.clone();
        parent.tools.shell = false;
        cases.push(("parent shell", parent, base_args.clone(), false));
        let mut parent = base_parent.clone();
        parent.paths = vec![envelope::PathScope::new("src")];
        cases.push(("parent path", parent, base_args.clone(), false));
        let mut parent = base_parent.clone();
        parent.expires_at = now.saturating_sub(1);
        cases.push(("expired", parent, base_args.clone(), false));
        let mut args = base_args.clone();
        args.no_network = true;
        cases.push(("--no-network", base_parent.clone(), args, false));
        let mut args = base_args.clone();
        args.mode = WorkerMode::ReadOnly;
        cases.push(("read-only", base_parent.clone(), args, false));
        let mut args = base_args.clone();
        args.path_scope = vec![PathBuf::from("src")];
        cases.push(("narrow path", base_parent.clone(), args, false));
        let mut args = base_args.clone();
        args.path_scope = vec![PathBuf::from("../outside")];
        cases.push(("path widening", base_parent.clone(), args, false));
        let mut parent = base_parent.clone();
        parent.delegation_depth = 1;
        let mut args = base_args.clone();
        args.depth = Some(1);
        cases.push(("depth widening", parent, args, false));
        let mut parent = base_parent;
        parent.delegation_depth = 0;
        cases.push(("depth zero", parent, base_args, true));

        for (name, parent, args, returns_exit_code) in cases {
            let mut env = base_env(&tmp.path().join(format!("state-{name}")));
            env.insert("HOME".into(), home.display().to_string());
            env.insert(
                ENVELOPE_ENV.into(),
                envelope::canonical_json(&parent).expect("envelope"),
            );
            let result = run_with(&args, &mut Vec::new(), tmp.path(), &|key| {
                env.get(key).cloned()
            });
            if returns_exit_code {
                assert_eq!(
                    result.expect("depth zero is a refusal receipt"),
                    2,
                    "{name}"
                );
            } else {
                let error = result.expect_err(name);
                assert!(
                    error.to_string().contains("envelope")
                        || error.to_string().contains("requires an unexpired writing"),
                    "{name}: {error}"
                );
            }
            assert!(
                !marker.exists(),
                "{name}: workspace setup ran despite refusal"
            );
            let worktrees = tmp.path().join(".zirv/worktrees");
            assert!(
                !worktrees.exists()
                    || std::fs::read_dir(&worktrees)
                        .expect("worktrees")
                        .next()
                        .is_none(),
                "{name}: worktree allocated before workspace refusal"
            );
        }
    }

    #[test]
    fn unrestricted_envelope_allows_workspace_setup() {
        if cfg!(windows) {
            return;
        }
        let tmp = crate::commands::ctx::testenv::repo();
        let home = tmp.path().join("home");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);
        std::fs::create_dir_all(home.join(".zirv")).expect("mkdir");
        let marker = tmp.path().join("workspace-ran");
        std::fs::write(
            home.join(".zirv/ctx.toml"),
            format!(
                "[[workspace]]\nname = \"setup\"\nsetup = [\"touch {}\"]\n",
                marker.display()
            ),
        )
        .expect("operator workspace config");
        let mut env = base_env(&tmp.path().join("state"));
        env.insert("HOME".into(), home.display().to_string());
        let args = AgentArgs {
            workspace: Some("setup".into()),
            ..args_for("claude", "go")
        };
        assert_eq!(
            run_with(&args, &mut Vec::new(), tmp.path(), &|key| env
                .get(key)
                .cloned())
            .expect("unrestricted workspace runs"),
            0
        );
        assert!(marker.exists(), "workspace setup did not run");
    }

    /// Issue #228, decision 2, the core of the feature: a headless
    /// delegation's child process cwd (and so its per-harness sandbox) comes
    /// from `--workdir`, not from the delegating session's own `repo` --
    /// exercised end to end through `run_with`, with the fake agent
    /// (`fake-agent.sh`'s `FAKE_AGENT_CWD_LOG`) reporting its own real `pwd`
    /// back rather than this test trusting the plumbing without ever
    /// spawning anything.
    #[test]
    fn a_headless_delegation_with_workdir_launches_its_child_process_there() {
        if !git_available() {
            eprintln!("skipping: git not found on PATH");
            return;
        }
        let tmp = crate::commands::ctx::testenv::repo();
        let home = tmp.path().join("home");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);
        let target = tmp.path().join("target-repo");
        std::fs::create_dir_all(&target).expect("mkdir");
        assert!(git_init(&target), "git init");
        let cwd_log = tmp.path().join("cwd.log");

        let env = base_env(&tmp.path().join("state"));
        // `FAKE_AGENT_CWD_LOG` (like `FAKE_AGENT_ARGV_LOG` elsewhere in this
        // module) is read by the fixture script from its own REAL process
        // environment, which it inherits from this test process -- not from
        // the `EnvLookup` closure `run_with` itself reads config through, so
        // it has to be a genuine `std::env::set_var`, not an entry in `env`.
        unsafe {
            std::env::set_var("FAKE_AGENT_CWD_LOG", &cwd_log);
        }

        let mut args = args_for("claude", "go");
        args.workdir = Some(target.clone());
        let mut out = Vec::new();
        let code = run_with(&args, &mut out, tmp.path(), &|k| env.get(k).cloned());
        unsafe {
            std::env::remove_var("FAKE_AGENT_CWD_LOG");
        }
        assert_eq!(
            code.expect("a headless delegation with an explicit workdir runs"),
            0
        );

        let logged = std::fs::read_to_string(&cwd_log).expect("cwd log written");
        let logged_cwd = logged.lines().next().expect("one logged cwd");
        assert_eq!(
            normalize_path_for_compare(std::path::Path::new(logged_cwd)),
            normalize_path_for_compare(&target),
            "the headless child's own process cwd must be the requested --workdir, not `repo`: \
             logged {logged_cwd:?}, target {target:?}"
        );
    }

    #[test]
    fn the_delegation_verb_refuses_an_agent_the_settings_file_disabled() {
        let tmp = crate::commands::ctx::testenv::repo();
        let home = tmp.path().join("home");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);
        std::fs::create_dir_all(tmp.path().join(".zirv")).expect("mkdir");
        std::fs::write(
            tmp.path().join(".zirv/.settings.toml"),
            "[agents.claude]\nenabled = false\n",
        )
        .expect("write");

        let env = base_env(&tmp.path().join("state"));
        let args = args_for("claude", "go");
        let mut out = Vec::new();
        let err = run_with(&args, &mut out, tmp.path(), &|k| env.get(k).cloned())
            .expect_err("claude is disabled");
        let msg = err.to_string();
        assert!(msg.contains("claude"), "got {msg}");
        assert!(msg.contains("disabled"), "got {msg}");
    }

    /// Codex is a supported delegation target now that its own `ready()`
    /// only checks that its program resolves (`CodexAdapter::ready`, mirrors
    /// `ClaudeAdapter::ready`), so a disabled-by-settings codex is refused
    /// for the gate reason, not "not implemented yet" -- the same shape as
    /// `the_delegation_verb_refuses_an_agent_the_settings_file_disabled`
    /// above, with the disabled name swapped.
    #[test]
    fn the_delegation_verb_refuses_an_agent_the_settings_file_disabled_for_codex() {
        let tmp = crate::commands::ctx::testenv::repo();
        let home = tmp.path().join("home");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);
        std::fs::create_dir_all(tmp.path().join(".zirv")).expect("mkdir");
        std::fs::write(
            tmp.path().join(".zirv/.settings.toml"),
            "[agents.codex]\nenabled = false\n",
        )
        .expect("write");

        let env = base_env(&tmp.path().join("state"));
        let args = args_for("codex", "go");
        let mut out = Vec::new();
        let err = run_with(&args, &mut out, tmp.path(), &|k| env.get(k).cloned())
            .expect_err("codex is disabled");
        let msg = err.to_string();
        assert!(msg.contains("codex"), "got {msg}");
        assert!(msg.contains("disabled"), "got {msg}");
    }

    /// H4: distinct from the gate-disabled tests above -- an *enabled* named
    /// agent whose own `ready()` fails must still have that error propagate
    /// all the way out of `agent::run_with`. Not reachable with `ZIRV_CTX_
    /// AGENT_BIN` (an explicit path is never a `ready()` failure -- only a
    /// bare name resolved via `PATH` to an unlaunchable extension is, see
    /// `adapters::mod::tests::an_unlaunchable_program_on_path_is_named_
    /// rather_than_left_to_error_193`), so this deliberately omits `ZIRV_
    /// CTX_AGENT_BIN` and rigs `PATH`/`PATHEXT` instead, the same seam
    /// `readiness_note_and_the_fallback_skip_both_stay_covered_when_an_
    /// adapter_is_genuinely_unready` uses.
    #[cfg(windows)]
    #[test]
    fn the_delegation_verb_propagates_a_genuine_ready_failure() {
        let tmp = crate::commands::ctx::testenv::repo();
        let home = tmp.path().join("home");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);

        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("claude.py"), "print('x')\n").expect("write");
        let path = std::env::var("PATH").unwrap_or_default();
        let _path_guard = crate::commands::ctx::testenv::VarGuard::set(&[
            (
                "PATH",
                Some(format!("{};{}", dir.path().display(), path).as_str()),
            ),
            ("PATHEXT", Some(".EXE;.CMD;.PY")),
        ]);

        let env: HashMap<String, String> = [(
            crate::commands::ctx::state::STATE_ENV.to_string(),
            tmp.path().join("state").display().to_string(),
        )]
        .into();
        let args = args_for("claude", "go");
        let mut out = Vec::new();
        let err = run_with(&args, &mut out, tmp.path(), &|k| env.get(k).cloned())
            .expect_err("claude's own ready() must fail under this rig");
        let msg = err.to_string();
        assert!(msg.contains("claude"), "got {msg}");
        assert!(
            !msg.to_lowercase().contains("disabled"),
            "a ready() failure is not a gate refusal, must not say 'disabled': {msg}"
        );
    }

    #[test]
    fn the_prompt_travels_as_data_and_is_never_encoded_into_argv() {
        let tmp = crate::commands::ctx::testenv::repo();
        let home = tmp.path().join("home");
        let argv_log = tmp.path().join("argv.log");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);
        let env = base_env(&tmp.path().join("state"));
        unsafe {
            std::env::set_var("FAKE_AGENT_MODE", "healthy");
            std::env::set_var("FAKE_AGENT_ARGV_LOG", &argv_log);
        }

        let mut args = args_for("claude", "--looks-like-a-flag but is not");
        args.flags = vec!["--model".to_string(), "opus".to_string()];
        let mut out = Vec::new();
        let code = run_with(&args, &mut out, tmp.path(), &|k| env.get(k).cloned());
        unsafe {
            std::env::remove_var("FAKE_AGENT_MODE");
            std::env::remove_var("FAKE_AGENT_ARGV_LOG");
        }
        assert_eq!(code.expect("runs"), 0);

        let argv = std::fs::read_to_string(&argv_log).expect("argv recorded");
        assert!(
            argv.contains("-p --looks-like-a-flag but is not"),
            "the prompt must land as the -p value verbatim, not be parsed as flags: {argv}"
        );
        assert!(
            argv.contains("--model opus"),
            "the operator's own flags must still reach the agent: {argv}"
        );
    }

    #[test]
    fn a_timed_out_run_keeps_its_exit_code_and_explains_it_in_words() {
        let tmp = crate::commands::ctx::testenv::repo();
        let home = tmp.path().join("home");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);
        let env = base_env(&tmp.path().join("state"));
        unsafe {
            std::env::set_var("FAKE_AGENT_MODE", "hang");
        }

        let mut args = args_for("claude", "do the work");
        args.max_restarts = Some(0);
        args.timeout_secs = Some(3);
        let mut out = Vec::new();
        let code = run_with(&args, &mut out, tmp.path(), &|k| env.get(k).cloned());
        unsafe {
            std::env::remove_var("FAKE_AGENT_MODE");
        }

        assert_eq!(
            code.expect("runs"),
            exec::EXIT_TIMEOUT,
            "the caller applies its own policy after the budget is spent"
        );
        assert!(
            exit_note(exec::EXIT_TIMEOUT)
                .expect("a note exists")
                .contains("wall-clock timeout")
        );
    }

    /// Issue #230 item 3: the delegator captures `zirv agent`'s synchronous
    /// stdout result, so a degraded capability must appear in the captured
    /// `out` with its full `detail`.
    #[test]
    fn a_headless_run_with_a_degraded_capability_reports_it_in_the_captured_result() {
        let tmp = crate::commands::ctx::testenv::repo();
        let home = tmp.path().join("home");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);
        std::fs::create_dir_all(tmp.path().join(".zirv")).expect("mkdir .zirv");
        // `approval = "deny"` on a headless claude launch resolves to
        // `Support::Unsupported` (`ClaudeAdapter::policy_support`'s own
        // `APPROVAL_UNSUPPORTED` arm) -- a real, non-`Enforced` degradation.
        std::fs::write(
            tmp.path().join(".zirv/ctx.toml"),
            "[policy]\napproval = \"deny\"\n",
        )
        .expect("write ctx.toml");
        let env = base_env(&tmp.path().join("state"));
        unsafe {
            std::env::set_var("FAKE_AGENT_MODE", "healthy");
        }
        let args = args_for("claude", "do the work");
        let mut out = Vec::new();
        let code = run_with(&args, &mut out, tmp.path(), &|k| env.get(k).cloned());
        unsafe {
            std::env::remove_var("FAKE_AGENT_MODE");
        }
        assert_eq!(code.expect("runs"), 0);

        let text = String::from_utf8(out).expect("utf8");
        assert!(
            text.contains("capability warning: approval/ask behavior --"),
            "got {text}"
        );
        assert!(
            text.contains("not enforced (advisory only)"),
            "the detail field must ride along, not just capability/mechanism: {text}"
        );
    }

    /// The other half: a clean run (no `[policy]` restriction at all) prints
    /// nothing about capabilities.
    #[test]
    fn a_clean_headless_run_prints_nothing_about_capabilities() {
        let tmp = crate::commands::ctx::testenv::repo();
        let home = tmp.path().join("home");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);
        let env = base_env(&tmp.path().join("state"));
        unsafe {
            std::env::set_var("FAKE_AGENT_MODE", "healthy");
        }
        let args = args_for("claude", "do the work");
        let mut out = Vec::new();
        let code = run_with(&args, &mut out, tmp.path(), &|k| env.get(k).cloned());
        unsafe {
            std::env::remove_var("FAKE_AGENT_MODE");
        }
        assert_eq!(code.expect("runs"), 0);

        let text = String::from_utf8(out).expect("utf8");
        assert!(!text.contains("capability warning"), "got {text}");
    }

    /// Issue #227: a headless delegation that FAILS (here, a timeout
    /// give-up with the restart budget at zero) sends a report-back mail to
    /// the spawning session -- the worker's own self-report only ever fires
    /// for a dashboard pane and only on success, so without this the
    /// requester previously learned nothing beyond a bare exit code for any
    /// plain headless failure, success or not.
    #[test]
    fn a_failed_delegation_reports_back_to_the_spawning_session() {
        let tmp = crate::commands::ctx::testenv::repo();
        let home = tmp.path().join("home");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);
        let state = tmp.path().join("state");
        let mut env = base_env(&state);
        // A short, addressable parent session id -- `short_id` takes the
        // first eight ASCII-alphanumeric characters.
        env.insert(
            crate::commands::ctx::adapters::SESSION_ENV.to_string(),
            "aaaaaaaa-1111-4222-8333-444444444444".to_string(),
        );
        unsafe {
            std::env::set_var("FAKE_AGENT_MODE", "hang");
        }

        let mut args = args_for("claude", "do the work");
        args.max_restarts = Some(0);
        args.timeout_secs = Some(3);
        let mut out = Vec::new();
        let code = run_with(&args, &mut out, tmp.path(), &|k| env.get(k).cloned());
        unsafe {
            std::env::remove_var("FAKE_AGENT_MODE");
        }
        assert_eq!(code.expect("runs"), exec::EXIT_TIMEOUT);

        let state_dir = crate::commands::ctx::state::StateDir::from_root(state);
        let repo_slug = crate::commands::ctx::state::repo_slug(tmp.path());
        let mailbox = state_dir.mail().join(&repo_slug);
        let entries: Vec<_> = std::fs::read_dir(&mailbox)
            .map(|dir| dir.flatten().collect())
            .unwrap_or_default();
        assert_eq!(
            entries.len(),
            1,
            "exactly one report-back message must land in the mailbox"
        );
        let body = std::fs::read_to_string(entries[0].path()).expect("read mail");
        assert!(
            body.contains("To-session: aaaaaaaa"),
            "addressed to the requester's short id: {body}"
        );
        assert!(
            body.contains("wall-clock timeout"),
            "carries the same structured reason as the stderr note: {body}"
        );
    }

    /// Issue #318: `--result-kind review` appends the OUTPUT CONTRACT block
    /// to the worker's prompt and exports the schema into its env; a worker
    /// whose final assistant text carries a fenced json block satisfying it
    /// (`FAKE_AGENT_MODE=contract-ok`, see `fake-agent.sh`'s own doc
    /// comment) is validated on the very first attempt -- no retry, and the
    /// report-back mail carries the parsed result verbatim rather than the
    /// bare exit-code summary a schema-less run gets.
    #[test]
    fn a_headless_run_with_a_valid_contract_reply_is_validated_on_the_first_attempt() {
        let tmp = crate::commands::ctx::testenv::repo();
        let home = tmp.path().join("home");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);
        let state = tmp.path().join("state");
        let mut env = base_env(&state);
        env.insert(
            crate::commands::ctx::adapters::SESSION_ENV.to_string(),
            "aaaaaaaa-1111-4222-8333-444444444444".to_string(),
        );
        unsafe {
            std::env::set_var("FAKE_AGENT_MODE", "contract-ok");
        }

        let mut args = args_for("claude", "do the work");
        args.result_kind = Some("review".to_string());
        let mut out = Vec::new();
        let code = run_with(&args, &mut out, tmp.path(), &|k| env.get(k).cloned());
        unsafe {
            std::env::remove_var("FAKE_AGENT_MODE");
        }
        assert_eq!(code.expect("runs"), 0);
        let printed = String::from_utf8_lossy(&out);
        assert!(printed.contains("result: validated"), "got {printed}");

        let state_dir = crate::commands::ctx::state::StateDir::from_root(state);
        let repo_slug = crate::commands::ctx::state::repo_slug(tmp.path());
        let mailbox = state_dir.mail().join(&repo_slug);
        let entries: Vec<_> = std::fs::read_dir(&mailbox)
            .map(|dir| dir.flatten().collect())
            .unwrap_or_default();
        assert_eq!(
            entries.len(),
            1,
            "a schema-declared run mails back even on success"
        );
        let body = std::fs::read_to_string(entries[0].path()).expect("read mail");
        assert!(body.contains("result:"), "got {body}");
        assert!(body.contains("\"status\": \"done\""), "got {body}");

        let results_dir = state_dir.logs().join("delegation-results");
        let files: Vec<_> = std::fs::read_dir(&results_dir)
            .expect("results dir exists")
            .flatten()
            .collect();
        assert_eq!(files.len(), 1, "exactly one results file");
        let record: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(files[0].path()).expect("read"))
                .expect("json");
        assert_eq!(record["outcome"], "validated");
        assert_eq!(record["result"]["status"], "done");
    }

    /// Issue #318: a worker whose final text never satisfies the declared
    /// contract (`FAKE_AGENT_MODE=contract-bad`: `status` is `"bogus"`, not
    /// one of `review`'s declared enum values) gets exactly one bounded
    /// retry on an adapter that supports resuming (claude does) -- and when
    /// the retry ALSO fails, the delegation reports `contract_failed` with
    /// both attempts' errors recorded, never a synthetic success.
    #[test]
    fn a_headless_run_whose_reply_never_satisfies_the_contract_retries_once_then_fails() {
        let tmp = crate::commands::ctx::testenv::repo();
        let home = tmp.path().join("home");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);
        let state = tmp.path().join("state");
        let mut env = base_env(&state);
        env.insert(
            crate::commands::ctx::adapters::SESSION_ENV.to_string(),
            "aaaaaaaa-1111-4222-8333-444444444444".to_string(),
        );
        unsafe {
            std::env::set_var("FAKE_AGENT_MODE", "contract-bad");
        }

        let mut args = args_for("claude", "do the work");
        args.result_kind = Some("review".to_string());
        let mut out = Vec::new();
        let code = run_with(&args, &mut out, tmp.path(), &|k| env.get(k).cloned());
        unsafe {
            std::env::remove_var("FAKE_AGENT_MODE");
        }
        assert_eq!(
            code.expect("runs"),
            exec::EXIT_CONTRACT_FAILED,
            "a clean child exit cannot hide a failed contract"
        );
        let printed = String::from_utf8_lossy(&out);
        assert!(printed.contains("result: contract_failed"), "got {printed}");

        let state_dir = crate::commands::ctx::state::StateDir::from_root(state);
        let repo_slug = crate::commands::ctx::state::repo_slug(tmp.path());
        let mailbox = state_dir.mail().join(&repo_slug);
        let entries: Vec<_> = std::fs::read_dir(&mailbox)
            .map(|dir| dir.flatten().collect())
            .unwrap_or_default();
        assert_eq!(entries.len(), 1);
        let body = std::fs::read_to_string(entries[0].path()).expect("read mail");
        assert!(body.contains("contract_failed:"), "got {body}");
        assert!(
            body.contains("not in [done, blocked, partial]"),
            "carries the enum error verbatim: {body}"
        );
        assert!(body.contains("raw candidate:"), "got {body}");

        let results_dir = state_dir.logs().join("delegation-results");
        let files: Vec<_> = std::fs::read_dir(&results_dir)
            .expect("results dir exists")
            .flatten()
            .collect();
        assert_eq!(files.len(), 1);
        let record: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(files[0].path()).expect("read"))
                .expect("json");
        assert_eq!(record["outcome"], "contract_failed");
        let rows = super::super::super::log::read_delegations(&state_dir, usize::MAX);
        assert!(!rows.is_empty());
        assert!(
            rows.iter().all(|row| row.outcome == "contract_failed"
                && row.exit_code == exec::EXIT_CONTRACT_FAILED)
        );

        assert!(record["result"].is_null());
        let errors = record["errors"].as_array().expect("errors array");
        assert_eq!(
            errors.len(),
            2,
            "one bounded retry means exactly two attempts: {errors:?}"
        );
    }

    /// Issue #722: a plain (no `--result-schema`/`--result-kind`) delegation
    /// that exits clean but never produces an extractable final report
    /// (`DelegationState::ExitedNoReport`) used to persist NOTHING at all --
    /// `store_report_only` was only ever reached when `first_text.is_some()`.
    /// It now always gets a durable `delegation-results/<session>.json`
    /// too, written through the same `write_delegation_result` writer every
    /// other outcome uses, with `outcome: "exited_no_report"` and an empty
    /// report -- so `result_path` in the `--json` receipt is never left
    /// empty for this state either.
    #[test]
    fn a_no_contract_run_with_no_extractable_report_still_persists_a_post_mortem() {
        let tmp = crate::commands::ctx::testenv::repo();
        let home = tmp.path().join("home");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);
        let state = tmp.path().join("state");
        let mut env = base_env(&state);
        env.insert("ZIRV_CTX_PACE".to_string(), "false".to_string());
        unsafe {
            std::env::set_var("FAKE_AGENT_MODE", "healthy");
            std::env::set_var("FAKE_AGENT_TURNS", "0");
        }

        let mut args = args_for("claude", "do the work");
        args.json = true;
        let mut out = Vec::new();
        let code = run_with(&args, &mut out, tmp.path(), &|k| env.get(k).cloned());
        unsafe {
            std::env::remove_var("FAKE_AGENT_MODE");
            std::env::remove_var("FAKE_AGENT_TURNS");
        }
        assert_eq!(
            code.expect("runs"),
            0,
            "the worker process itself exited clean"
        );

        let printed = String::from_utf8_lossy(&out);
        let receipt: serde_json::Value =
            serde_json::from_str(printed.trim()).expect("receipt json");
        assert_eq!(receipt["state"], "exited_no_report");
        let result_path = receipt["result_path"]
            .as_str()
            .expect("result_path is populated even with nothing to report");

        let record: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(result_path).expect("read post-mortem"))
                .expect("json");
        assert_eq!(record["outcome"], "exited_no_report");
        assert!(record["report"].is_null());

        let state_dir = crate::commands::ctx::state::StateDir::from_root(state);
        let results_dir = state_dir.logs().join("delegation-results");
        let files: Vec<_> = std::fs::read_dir(&results_dir)
            .expect("results dir exists")
            .flatten()
            .collect();
        assert_eq!(
            files.len(),
            1,
            "exactly one durable post-mortem record, even with no report text"
        );
    }

    /// #868: a failed delegation mails the delegating seat, except a supervisor consult's helper.
    #[test]
    fn a_failed_supervisor_consult_helper_sends_no_report_back_mail() {
        let mailed = |consult: bool| {
            let tmp = crate::commands::ctx::testenv::repo();
            let home = tmp.path().join("home");
            let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);
            let state_dir = tmp.path().join("state");
            let mut env = base_env(&state_dir);
            env.insert("ZIRV_CTX_PACE".to_string(), "false".to_string());
            env.insert(
                crate::commands::ctx::adapters::SESSION_ENV.to_string(),
                "seat0001-2222-4333-8444-555555555555".to_string(),
            );
            if consult {
                env.insert(
                    crate::commands::ctx::supervisor::CONSULT_ENV.to_string(),
                    "1".to_string(),
                );
            }
            // SAFETY: CI runs tests single-threaded.
            unsafe {
                std::env::set_var("FAKE_AGENT_MODE", "fail");
                std::env::set_var("FAKE_AGENT_TURNS", "0");
            }
            let mut args = args_for("claude", "do the work");
            args.json = true;
            args.max_restarts = Some(0);
            let mut out = Vec::new();
            let code = run_with(&args, &mut out, tmp.path(), &|k| env.get(k).cloned());
            unsafe {
                std::env::remove_var("FAKE_AGENT_MODE");
                std::env::remove_var("FAKE_AGENT_TURNS");
            }
            assert_eq!(code.expect("runs"), 3);
            crate::commands::ctx::mail::list(
                &StateDir::from_root(state_dir),
                &crate::commands::ctx::state::repo_slug(tmp.path()),
                None,
                Some("seat0001"),
            )
            .expect("mail")
            .len()
        };

        assert_eq!(
            mailed(false),
            1,
            "control: an ordinary failure mails the seat"
        );
        assert_eq!(mailed(true), 0);
    }

    /// Issue #722 (orchestrator follow-up): confirms what a reader actually
    /// sees for `ExitedNoReport` on a NONZERO exit -- `FAKE_AGENT_TURNS=0`
    /// (no text) plus `FAKE_AGENT_MODE=fail` (exits 3) reaches the identical
    /// `write_delegation_result` call the clean-exit case does, since
    /// `delegation_state`/the post-mortem write only ever branch on
    /// `first_text`, never on `code`. The `--json` receipt still carries the
    /// real exit code; `DelegationResultRecord` (the persisted file) has no
    /// `exit_code` field at all, for this outcome or any other -- that was
    /// already true before this issue, not something #722 introduced.
    #[test]
    fn a_no_contract_run_with_a_nonzero_exit_and_no_text_is_still_exited_no_report() {
        let tmp = crate::commands::ctx::testenv::repo();
        let home = tmp.path().join("home");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);
        let state = tmp.path().join("state");
        let mut env = base_env(&state);
        env.insert("ZIRV_CTX_PACE".to_string(), "false".to_string());
        unsafe {
            std::env::set_var("FAKE_AGENT_MODE", "fail");
            std::env::set_var("FAKE_AGENT_TURNS", "0");
        }

        let mut args = args_for("claude", "do the work");
        args.json = true;
        args.max_restarts = Some(0);
        let mut out = Vec::new();
        let code = run_with(&args, &mut out, tmp.path(), &|k| env.get(k).cloned());
        unsafe {
            std::env::remove_var("FAKE_AGENT_MODE");
            std::env::remove_var("FAKE_AGENT_TURNS");
        }
        assert_eq!(code.expect("runs"), 3, "the crash exit code propagates");

        let printed = String::from_utf8_lossy(&out);
        let receipt: serde_json::Value =
            serde_json::from_str(printed.trim()).expect("receipt json");
        assert_eq!(receipt["state"], "exited_no_report");
        assert_eq!(
            receipt["exit_code"], 3,
            "the receipt still carries the real exit code"
        );
        let result_path = receipt["result_path"].as_str().expect("result_path");

        let record: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(result_path).expect("read post-mortem"))
                .expect("json");
        assert_eq!(record["outcome"], "exited_no_report");
        assert!(record["report"].is_null());
        assert!(
            record.get("exit_code").is_none(),
            "the persisted record has no exit_code field at all, for any outcome: {record}"
        );
    }

    /// Issue #303 gave codex a real `headless_resume_cmd` (`codex exec
    /// resume`), but review round 1 found it targeted zirv's own session id
    /// instead of codex's own minted one -- a wasted resume against a
    /// conversation codex never created. The fix (`CodexAdapter::
    /// resume_target`) recovers codex's real id from its own rollout file
    /// and fails closed (`None`) whenever that rollout cannot be resolved,
    /// which is exactly what happens here: this test's `fake-agent.sh` never
    /// writes anything codex-shaped under `.codex/sessions/`, so no rollout
    /// exists to recover an id from. The structural capability
    /// (`headless_resume_cmd`) is real, but the gate now ALSO requires
    /// `resume_target` to resolve -- and it cannot, so this stays exactly
    /// ONE attempt, never a resume aimed at a session that was never
    /// verified to exist.
    #[test]
    fn a_headless_run_on_codex_stays_one_attempt_when_its_own_rollout_cannot_be_resolved() {
        let tmp = crate::commands::ctx::testenv::repo();
        let home = tmp.path().join("home");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);
        let state = tmp.path().join("state");
        let mut env = base_env(&state);
        env.insert(
            crate::commands::ctx::adapters::SESSION_ENV.to_string(),
            "aaaaaaaa-1111-4222-8333-444444444444".to_string(),
        );
        unsafe {
            std::env::set_var("FAKE_AGENT_MODE", "healthy");
        }

        let mut args = args_for("codex", "do the work");
        args.result_kind = Some("review".to_string());
        let mut out = Vec::new();
        let code = run_with(&args, &mut out, tmp.path(), &|k| env.get(k).cloned());
        unsafe {
            std::env::remove_var("FAKE_AGENT_MODE");
        }
        code.expect("runs");
        let printed = String::from_utf8_lossy(&out);
        assert!(
            printed.contains("result: contract_failed (1 errors)"),
            "got {printed}"
        );

        let state_dir = crate::commands::ctx::state::StateDir::from_root(state);
        let results_dir = state_dir.logs().join("delegation-results");
        let files: Vec<_> = std::fs::read_dir(&results_dir)
            .expect("results dir exists")
            .flatten()
            .collect();
        assert_eq!(files.len(), 1);
        let record: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(files[0].path()).expect("read"))
                .expect("json");
        assert_eq!(record["outcome"], "contract_failed");
        let errors = record["errors"].as_array().expect("errors array");
        assert_eq!(
            errors.len(),
            1,
            "an unresolvable rollout means exactly one attempt, never a resume aimed at an \
             unverified session: {errors:?}"
        );
    }

    /// Restores the original issue #318 guarantee: an adapter still on the
    /// trait's own honest-refusal `headless_resume_cmd` default (droid,
    /// confirmed by `droid.rs`'s own module doc -- "no verified headless
    /// compact-then-resume flow") gets exactly ONE contract attempt, never a
    /// synthetic retry it cannot structurally receive. Droid's own
    /// `headless_cmd` (`exec -o stream-json <prompt>`) matches none of
    /// `fake-agent.sh`'s recognized flags either, so this run's transcript is
    /// genuinely unreadable the same way codex's own no-`--session-id`
    /// launch is -- exactly the "no final text at all" case a real crashed
    /// worker would also produce.
    #[test]
    fn a_headless_run_on_an_adapter_with_no_resume_support_gets_exactly_one_attempt() {
        let tmp = crate::commands::ctx::testenv::repo();
        let home = tmp.path().join("home");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);
        let state = tmp.path().join("state");
        let mut env = base_env(&state);
        env.insert(
            crate::commands::ctx::adapters::SESSION_ENV.to_string(),
            "aaaaaaaa-1111-4222-8333-444444444444".to_string(),
        );
        unsafe {
            std::env::set_var("FAKE_AGENT_MODE", "healthy");
        }

        let mut args = args_for("droid", "do the work");
        args.result_kind = Some("review".to_string());
        let mut out = Vec::new();
        let code = run_with(&args, &mut out, tmp.path(), &|k| env.get(k).cloned());
        unsafe {
            std::env::remove_var("FAKE_AGENT_MODE");
        }
        code.expect("runs");
        let printed = String::from_utf8_lossy(&out);
        assert!(
            printed.contains("result: contract_failed (1 errors)"),
            "got {printed}"
        );

        let state_dir = crate::commands::ctx::state::StateDir::from_root(state);
        let results_dir = state_dir.logs().join("delegation-results");
        let files: Vec<_> = std::fs::read_dir(&results_dir)
            .expect("results dir exists")
            .flatten()
            .collect();
        assert_eq!(files.len(), 1);
        let record: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(files[0].path()).expect("read"))
                .expect("json");
        assert_eq!(record["outcome"], "contract_failed");
        let errors = record["errors"].as_array().expect("errors array");
        assert_eq!(
            errors.len(),
            1,
            "no resume support means exactly one attempt, never a synthetic retry: {errors:?}"
        );
    }

    /// A delegated run is a worker session, not an orchestrator one: it must
    /// carry zirv's own shipped default layer (proving injection happened at
    /// all) but never the harness meta-teaching layer, which only an
    /// orchestrator session gets. `exec::run_with` has no `PromptRole`
    /// parameter to get wrong -- it is hardcoded to `Worker` -- so this pins
    /// the observable behavior that fact is supposed to guarantee.
    #[test]
    fn a_delegated_run_is_a_worker_session_not_an_orchestrator_one() {
        let tmp = crate::commands::ctx::testenv::repo();
        let home = tmp.path().join("home");
        let argv_log = tmp.path().join("argv.log");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);
        let mut env = base_env(&tmp.path().join("state"));
        env.insert("ZIRV_CTX_PACE".to_string(), "false".to_string());
        unsafe {
            std::env::set_var("FAKE_AGENT_MODE", "healthy");
            std::env::set_var("FAKE_AGENT_ARGV_LOG", &argv_log);
        }

        let args = args_for("claude", "do the work");
        let mut out = Vec::new();
        let code = run_with(&args, &mut out, tmp.path(), &|k| env.get(k).cloned());
        unsafe {
            std::env::remove_var("FAKE_AGENT_MODE");
            std::env::remove_var("FAKE_AGENT_ARGV_LOG");
        }
        assert_eq!(code.expect("runs"), 0);

        let argv = std::fs::read_to_string(&argv_log).expect("argv recorded");
        assert!(
            argv.contains("zirv engineering standard"),
            "the shipped default layer proves injection happened: {argv}"
        );
        // Match the layer's version header, not the bare name: the adapter
        // layer legitimately references "the zirv meta-harness layer" by name,
        // and only the header marks the layer itself being present.
        assert!(
            !argv.contains("zirv meta-harness (v"),
            "a worker session must never get the harness delegation layer: {argv}"
        );
    }

    /// `--quiet` folds into `ZIRV_CTX_QUIET` for the delegated `exec::
    /// run_with` call the same way it does for `chat`; this pins that the
    /// flag does not otherwise disturb a delegated run (it still launches,
    /// still succeeds, still exits 0).
    #[test]
    fn quiet_still_lets_the_delegated_run_complete_normally() {
        let tmp = crate::commands::ctx::testenv::repo();
        let home = tmp.path().join("home");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);
        let mut env = base_env(&tmp.path().join("state"));
        env.insert("ZIRV_CTX_PACE".to_string(), "false".to_string());
        unsafe {
            std::env::set_var("FAKE_AGENT_MODE", "healthy");
        }

        let mut args = args_for("claude", "do the work");
        args.quiet = true;
        let mut out = Vec::new();
        let code = run_with(&args, &mut out, tmp.path(), &|k| env.get(k).cloned());
        unsafe {
            std::env::remove_var("FAKE_AGENT_MODE");
        }
        assert_eq!(
            code.expect("runs"),
            0,
            "--quiet must not change the outcome"
        );
    }

    #[test]
    fn goal_runs_setup_then_a_depth_zero_fast_bootstrap_then_the_main_worker() {
        let tmp = crate::commands::ctx::testenv::repo();
        let home = tmp.path().join("home");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);
        std::fs::create_dir_all(home.join(".zirv")).expect("config dir");
        let order = tmp.path().join("order.log");
        std::fs::write(
            home.join(".zirv/ctx.toml"),
            format!(
                "[model_tiers.claude]\nfast = 'haiku-cheap'\n\n[[workspace]]\nname = 'prepared'\nsetup = [\"printf 'setup\\n' >> '{}'\"]\n",
                order.display()
            ),
        )
        .expect("operator config");
        let modes = tmp.path().join("modes.txt");
        std::fs::write(&modes, "bootstrap-ok\nhealthy\n").expect("modes");
        let argv = tmp.path().join("argv.log");
        let _fake = crate::commands::ctx::testenv::VarGuard::set(&[
            ("FAKE_AGENT_MODE_FILE", modes.to_str()),
            ("FAKE_AGENT_MODE_LOG", order.to_str()),
            ("FAKE_AGENT_ARGV_LOG", argv.to_str()),
        ]);
        let mut env = base_env(&tmp.path().join("state"));
        env.insert("HOME".into(), home.display().to_string());
        env.insert("ZIRV_CTX_PACE".into(), "false".into());

        let mut args = args_for("claude", "implement the feature");
        args.goal = Some("install dependencies".into());
        args.workspace = Some("prepared".into());
        let code = run_with(&args, &mut Vec::new(), tmp.path(), &|key| {
            env.get(key).cloned()
        })
        .expect("goal delegation succeeds");

        assert_eq!(code, 0);
        assert_eq!(
            std::fs::read_to_string(&order).expect("order"),
            "setup\nbootstrap-ok\nhealthy\n"
        );
        let argv = std::fs::read_to_string(argv).expect("argv");
        let launches: Vec<_> = argv
            .lines()
            .filter(|line| line.starts_with("-p "))
            .collect();
        assert_eq!(launches.len(), 2, "bootstrap and main: {launches:?}");
        assert!(launches[0].contains("--model haiku-cheap"), "{launches:?}");
        assert!(
            argv.contains("--append-system-prompt Prepare only the local development environment"),
            "fixed bootstrap instructions must be a system prompt: {argv}"
        );
        assert!(launches[1].contains("--model sonnet"), "{launches:?}");
        assert!(!launches[1].contains("haiku-cheap"), "{launches:?}");
        let state = StateDir::from_root(tmp.path().join("state"));
        let rows = super::super::super::log::tail_delegations(&state, 10).expect("ledger");
        assert_eq!(rows.len(), 2, "bootstrap spend is separate: {rows:?}");
        assert!(rows.iter().any(|row| row.contains("bootstrap-ok")));
        assert!(rows.iter().any(|row| row.contains("haiku-cheap")));
    }

    #[test]
    fn goal_bootstrap_spend_reduces_the_main_request_budget_and_group_settlement() {
        let tmp = crate::commands::ctx::testenv::repo();
        let home = tmp.path().join("home");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);
        let state_path = tmp.path().join("state");
        let state = StateDir::from_root(state_path.clone());
        create_work_group_with_spend(&state, "wg-goal-cap", 1_000_000, 10);
        let modes = tmp.path().join("modes.txt");
        std::fs::write(&modes, "bootstrap-ok\nhealthy\n").expect("modes");
        let _fake = crate::commands::ctx::testenv::VarGuard::set(&[
            ("FAKE_AGENT_MODE_FILE", modes.to_str()),
            ("FAKE_AGENT_TURNS", Some("1")),
        ]);
        let mut env = base_env(&state_path);
        env.insert("ZIRV_CTX_PACE".into(), "false".into());
        let mut args = args_for("claude", "main uses only what remains");
        args.goal = Some("prepare it".into());
        args.group = Some("wg-goal-cap".into());
        args.budget_tokens = Some(70_000);

        let code = run_with(&args, &mut Vec::new(), tmp.path(), &|key| {
            env.get(key).cloned()
        })
        .expect("budget exhaustion is a structured exit");

        assert_eq!(code, exec::EXIT_BUDGET_EXHAUSTED);
        let rows = super::super::super::log::read_delegations(&state, 10);
        assert_eq!(rows.len(), 2, "bootstrap and main are accounted separately");
        let bootstrap_spend = rows
            .iter()
            .find(|row| row.outcome == "bootstrap-ok")
            .map(|row| {
                row.input_tokens
                    .saturating_add(row.cache_creation_input_tokens)
                    .saturating_add(row.cache_read_input_tokens)
                    .saturating_add(row.output_tokens)
            })
            .expect("bootstrap row");
        assert!(
            bootstrap_spend > 30_000 && bootstrap_spend < 70_000,
            "the main succeeds under the original request cap but exhausts only the reduced remainder"
        );
        let aggregate_spend = rows.iter().fold(0_u64, |total, row| {
            total
                .saturating_add(row.input_tokens)
                .saturating_add(row.cache_creation_input_tokens)
                .saturating_add(row.cache_read_input_tokens)
                .saturating_add(row.output_tokens)
        });
        assert_eq!(
            super::super::super::group::load(&state, "wg-goal-cap")
                .expect("load")
                .expect("group")
                .spent_tokens,
            10 + aggregate_spend,
            "one reservation settles bootstrap and main spend together"
        );
    }

    #[test]
    fn a_group_budget_spent_by_goal_bootstrap_never_admits_the_main_worker() {
        let tmp = crate::commands::ctx::testenv::repo();
        let home = tmp.path().join("home");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);
        let state_path = tmp.path().join("state");
        let state = StateDir::from_root(state_path.clone());
        create_work_group_with_spend(&state, "wg-goal-exhausted", 30_000, 0);
        let modes = tmp.path().join("modes.txt");
        std::fs::write(&modes, "bootstrap-ok\nhealthy\n").expect("modes");
        let _fake = crate::commands::ctx::testenv::VarGuard::set(&[
            ("FAKE_AGENT_MODE_FILE", modes.to_str()),
            ("FAKE_AGENT_TURNS", Some("1")),
        ]);
        let mut env = base_env(&state_path);
        env.insert("ZIRV_CTX_PACE".into(), "false".into());
        let mut args = args_for("claude", "main must not launch");
        args.goal = Some("prepare it".into());
        args.group = Some("wg-goal-exhausted".into());

        let code = run_with(&args, &mut Vec::new(), tmp.path(), &|key| {
            env.get(key).cloned()
        })
        .expect("budget exhaustion is a structured exit");

        assert_eq!(code, exec::EXIT_BUDGET_EXHAUSTED);
        assert_eq!(
            std::fs::read_to_string(modes).expect("modes"),
            "healthy\n",
            "the main worker must not consume its scripted launch"
        );
        let rows = super::super::super::log::read_delegations(&state, 10);
        assert_eq!(rows.len(), 1, "only bootstrap usage is recorded");
        let bootstrap_spend = rows[0]
            .input_tokens
            .saturating_add(rows[0].cache_creation_input_tokens)
            .saturating_add(rows[0].cache_read_input_tokens)
            .saturating_add(rows[0].output_tokens);
        let group = super::super::super::group::load(&state, "wg-goal-exhausted")
            .expect("load")
            .expect("group");
        assert_eq!(group.reserved_tokens, 0);
        assert_eq!(group.spent_tokens, bootstrap_spend);
    }

    #[test]
    fn goal_bootstrap_is_a_depth_zero_sibling_with_request_floors() {
        let mut parent = envelope::WorkerEnvelope {
            principal: "root/requester".into(),
            paths: vec![envelope::PathScope::new("src")],
            tools: envelope::ToolSet::all(),
            network: true,
            destructive: true,
            delegation_depth: 3,
            expires_at: 42,
            token_budget: Some(900),
        };
        parent.tools.network = false;
        let mut args = args_for("claude", "go");
        args.no_network = true;
        args.path_scope = vec![PathBuf::from("src/bin")];
        let session = "aaaaaaaa-1111-4111-8111-111111111111";

        let (bootstrap, principal) =
            goal_bootstrap_envelope(&args, &parent, session, Some(700)).expect("narrow");

        assert_eq!(principal, "root/requester/aaaaaaaa");
        assert_eq!(bootstrap.principal, principal);
        assert_eq!(bootstrap.delegation_depth, 0);
        assert!(!bootstrap.tools.delegate);
        assert!(!bootstrap.tools.network);
        assert!(!bootstrap.network);
        assert_eq!(bootstrap.paths, vec![envelope::PathScope::new("src/bin")]);
        assert_eq!(bootstrap.token_budget, Some(700));
        assert_eq!(bootstrap.expires_at, 42);
    }

    #[test]
    fn blocked_goal_report_preserves_diagnostics_and_never_launches_main() {
        let tmp = crate::commands::ctx::testenv::repo();
        let home = tmp.path().join("home");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);
        let modes = tmp.path().join("modes.txt");
        std::fs::write(&modes, "bootstrap-blocked\nhealthy\n").expect("modes");
        let argv = tmp.path().join("argv.log");
        let _fake = crate::commands::ctx::testenv::VarGuard::set(&[
            ("FAKE_AGENT_MODE_FILE", modes.to_str()),
            ("FAKE_AGENT_ARGV_LOG", argv.to_str()),
        ]);
        let state_path = tmp.path().join("state");
        let state = StateDir::from_root(state_path.clone());
        create_work_group_with_spend(&state, "wg-blocked-bootstrap", 1_000_000, 0);
        let mut env = base_env(&state_path);
        env.insert("ZIRV_CTX_PACE".into(), "false".into());
        let mut args = args_for("claude", "main must not run");
        args.goal = Some("prepare it".into());
        args.group = Some("wg-blocked-bootstrap".into());

        let error = run_with(&args, &mut Vec::new(), tmp.path(), &|key| {
            env.get(key).cloned()
        })
        .expect_err("Blocked bootstrap refuses delegation");

        assert!(
            error.to_string().contains("goal bootstrap refused"),
            "{error}"
        );
        assert!(error.to_string().contains("diagnostic result:"), "{error}");
        assert_eq!(std::fs::read_to_string(modes).expect("modes"), "healthy\n");
        let argv = std::fs::read_to_string(argv).expect("argv");
        let bootstrap = argv
            .lines()
            .find(|line| line.starts_with("-p "))
            .expect("bootstrap launch");
        assert!(
            !bootstrap.contains("--model"),
            "an unmapped Fast tier leaves model selection to the adapter: {bootstrap}"
        );
        let result_dir = tmp.path().join("state/logs/delegation-results");
        assert_eq!(
            std::fs::read_dir(result_dir).expect("result dir").count(),
            1,
            "the failed bootstrap keeps one diagnostic result"
        );
        let rows = super::super::super::log::tail_delegations(&state, 10).expect("ledger");
        assert_eq!(
            rows.len(),
            1,
            "main worker must have no ledger row: {rows:?}"
        );
        assert!(rows[0].contains("bootstrap-failed"));
        let records = super::super::super::log::read_delegations(&state, 10);
        let row = records.first().expect("delegation row");
        let bootstrap_spend = row
            .input_tokens
            .saturating_add(row.cache_creation_input_tokens)
            .saturating_add(row.cache_read_input_tokens)
            .saturating_add(row.output_tokens);
        let group = super::super::super::group::load(&state, "wg-blocked-bootstrap")
            .expect("load")
            .expect("group");
        assert_eq!(group.reserved_tokens, 0);
        assert_eq!(
            group.spent_tokens, bootstrap_spend,
            "rejected bootstrap usage must settle instead of rolling back"
        );
        let provider = super::super::super::adapters::provider_for_agent_name(Some("claude"));
        assert!(
            super::super::super::reservation::entries(&state, provider).is_empty(),
            "the rejected bootstrap must settle and remove its provider reservation"
        );
    }

    #[test]
    fn missing_goal_report_refuses_main_and_keeps_the_result() {
        let tmp = crate::commands::ctx::testenv::repo();
        let home = tmp.path().join("home");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);
        let modes = tmp.path().join("modes.txt");
        std::fs::write(&modes, "healthy\nhealthy\n").expect("modes");
        let _fake = crate::commands::ctx::testenv::VarGuard::set(&[(
            "FAKE_AGENT_MODE_FILE",
            modes.to_str(),
        )]);
        let mut env = base_env(&tmp.path().join("state"));
        env.insert("ZIRV_CTX_PACE".into(), "false".into());
        let mut args = args_for("claude", "main must not run");
        args.goal = Some("prepare it".into());

        let error = run_with(&args, &mut Vec::new(), tmp.path(), &|key| {
            env.get(key).cloned()
        })
        .expect_err("missing Done report refuses delegation");

        assert!(
            error.to_string().contains("explicit Done report"),
            "{error}"
        );
        assert!(error.to_string().contains("diagnostic result:"), "{error}");
        assert_eq!(std::fs::read_to_string(modes).expect("modes"), "healthy\n");
        assert_eq!(
            std::fs::read_dir(tmp.path().join("state/logs/delegation-results"))
                .expect("result dir")
                .count(),
            1
        );
    }

    #[test]
    fn nonzero_goal_exit_refuses_main_even_with_a_report() {
        let tmp = crate::commands::ctx::testenv::repo();
        let home = tmp.path().join("home");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);
        let modes = tmp.path().join("modes.txt");
        std::fs::write(&modes, "fail\nhealthy\n").expect("modes");
        let _fake = crate::commands::ctx::testenv::VarGuard::set(&[(
            "FAKE_AGENT_MODE_FILE",
            modes.to_str(),
        )]);
        let mut env = base_env(&tmp.path().join("state"));
        env.insert("ZIRV_CTX_PACE".into(), "false".into());
        let mut args = args_for("claude", "main must not run");
        args.goal = Some("prepare it".into());

        let error = run_with(&args, &mut Vec::new(), tmp.path(), &|key| {
            env.get(key).cloned()
        })
        .expect_err("nonzero bootstrap refuses delegation");

        assert!(error.to_string().contains("exit 3"), "{error}");
        assert!(error.to_string().contains("diagnostic result:"), "{error}");
        assert_eq!(std::fs::read_to_string(modes).expect("modes"), "healthy\n");
    }

    #[test]
    fn goal_failure_json_prints_one_delegation_receipt() {
        let tmp = crate::commands::ctx::testenv::repo();
        let home = tmp.path().join("home");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);
        let modes = tmp.path().join("modes.txt");
        std::fs::write(&modes, "bootstrap-blocked\nhealthy\n").expect("modes");
        let _fake = crate::commands::ctx::testenv::VarGuard::set(&[(
            "FAKE_AGENT_MODE_FILE",
            modes.to_str(),
        )]);
        let mut env = base_env(&tmp.path().join("state"));
        env.insert("ZIRV_CTX_PACE".into(), "false".into());
        let mut args = args_for("claude", "main must not run");
        args.goal = Some("prepare it".into());
        args.json = true;
        let mut out = Vec::new();

        let code = run_with(&args, &mut out, tmp.path(), &|key| env.get(key).cloned())
            .expect("JSON failure is a structured exit");

        assert_eq!(code, 2);
        let text = String::from_utf8(out).expect("utf8");
        let receipt: serde_json::Value = serde_json::from_str(text.trim()).expect("receipt");
        assert_eq!(receipt["state"], "launch_failed");
        assert!(
            receipt["reason"]
                .as_str()
                .is_some_and(|reason| reason.contains("diagnostic result:")),
            "{receipt}"
        );
        assert_eq!(std::fs::read_to_string(modes).expect("modes"), "healthy\n");
    }

    #[test]
    fn goal_timeout_refuses_main_with_one_whole_run_deadline() {
        let tmp = crate::commands::ctx::testenv::repo();
        let home = tmp.path().join("home");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);
        let modes = tmp.path().join("modes.txt");
        std::fs::write(&modes, "hang\nhang\nhealthy\n").expect("modes");
        let _fake = crate::commands::ctx::testenv::VarGuard::set(&[(
            "FAKE_AGENT_MODE_FILE",
            modes.to_str(),
        )]);
        let mut env = base_env(&tmp.path().join("state"));
        env.insert("ZIRV_CTX_PACE".into(), "false".into());
        env.insert("ZIRV_CTX_WORKER_BOOTSTRAP_TIMEOUT_SECS".into(), "1".into());
        let mut args = args_for("claude", "main must not run");
        args.goal = Some("prepare it".into());
        let started = std::time::Instant::now();

        let error = run_with(&args, &mut Vec::new(), tmp.path(), &|key| {
            env.get(key).cloned()
        })
        .expect_err("timed-out bootstrap refuses delegation");

        // A bound on "the deadline killed both hang-mode children", which
        // never exit on their own: the failure this guards is an unbounded
        // wait. A passing run is ~3-4s on a loaded Windows dev box -- two
        // msys shell spawns and two tree-kills around the 1s deadline -- so
        // 8s was a 2x margin that host load alone could eat (the kills used
        // to cost a WMI-backed `taskkill` each and took this to ~120s). 30s
        // keeps an order of magnitude between pass and fail.
        assert!(
            started.elapsed() < Duration::from_secs(30),
            "the whole-run deadline must kill the hanging bootstrap, took {:?}",
            started.elapsed()
        );
        assert!(
            error.to_string().contains("goal bootstrap refused"),
            "{error}"
        );
        assert!(error.to_string().contains("diagnostic result:"), "{error}");
        // Main's `healthy` line is never consumed. The restart launches after
        // the whole-run deadline has passed and is killed at once, so whether
        // it read its own `hang` line first is a race either outcome of which
        // is correct.
        let remaining = std::fs::read_to_string(modes).expect("modes");
        assert!(
            matches!(remaining.as_str(), "healthy\n" | "hang\nhealthy\n"),
            "main must not run: {remaining:?}"
        );
    }

    #[test]
    fn goal_forces_inline_even_when_a_dashboard_is_live() {
        let tmp = crate::commands::ctx::testenv::repo();
        let home = tmp.path().join("home");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);
        let modes = tmp.path().join("modes.txt");
        std::fs::write(&modes, "bootstrap-ok\nhealthy\n").expect("modes");
        let _fake = crate::commands::ctx::testenv::VarGuard::set(&[(
            "FAKE_AGENT_MODE_FILE",
            modes.to_str(),
        )]);
        let (requests, mut env) = live_dashboard_dir(tmp.path());
        env.insert("ZIRV_CTX_PACE".into(), "false".into());
        let mut args = joinable_args("claude", "main runs inline");
        args.goal = Some("prepare it".into());

        let code = run_with(&args, &mut Vec::new(), tmp.path(), &|key| {
            env.get(key).cloned()
        })
        .expect("goal delegation runs");

        assert_eq!(code, 0);
        assert_eq!(std::fs::read_to_string(modes).expect("modes"), "");
        let requests: Vec<_> = std::fs::read_dir(requests)
            .expect("requests")
            .filter_map(Result::ok)
            .filter(|entry| entry.path().extension().is_some_and(|ext| ext == "json"))
            .collect();
        assert!(
            requests.is_empty(),
            "--goal must bypass pane requests: {requests:?}"
        );
    }

    #[test]
    fn an_internal_inline_request_completes_without_spawning_a_dashboard_pane() {
        let tmp = crate::commands::ctx::testenv::repo();
        let home = tmp.path().join("home");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);
        let _fake =
            crate::commands::ctx::testenv::VarGuard::set(&[("FAKE_AGENT_MODE", Some("healthy"))]);
        let (requests, mut env) = live_dashboard_dir(tmp.path());
        env.insert("ZIRV_CTX_PACE".into(), "false".into());
        let mut args = joinable_args("claude", "review synchronously");
        args.inline = true;

        let code = run_with(&args, &mut Vec::new(), tmp.path(), &|key| {
            env.get(key).cloned()
        })
        .expect("inline delegation completes");

        assert_eq!(code, 0);
        let requests: Vec<_> = std::fs::read_dir(requests)
            .expect("requests")
            .filter_map(Result::ok)
            .filter(|entry| entry.path().extension().is_some_and(|ext| ext == "json"))
            .collect();
        assert!(requests.is_empty(), "internal inline request: {requests:?}");
    }

    #[test]
    fn a_main_worker_restart_does_not_repeat_goal_preparation() {
        let tmp = crate::commands::ctx::testenv::repo();
        let home = tmp.path().join("home");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);
        let modes = tmp.path().join("modes.txt");
        let order = tmp.path().join("order.log");
        std::fs::write(&modes, "bootstrap-ok\nhang\nhealthy\n").expect("modes");
        let _fake = crate::commands::ctx::testenv::VarGuard::set(&[
            ("FAKE_AGENT_MODE_FILE", modes.to_str()),
            ("FAKE_AGENT_MODE_LOG", order.to_str()),
        ]);
        let mut env = base_env(&tmp.path().join("state"));
        env.insert("ZIRV_CTX_PACE".into(), "false".into());
        let mut args = args_for("claude", "main restarts once");
        args.goal = Some("prepare once".into());
        args.max_restarts = Some(1);
        args.timeout_secs = Some(5);

        let code = run_with(&args, &mut Vec::new(), tmp.path(), &|key| {
            env.get(key).cloned()
        })
        .expect("delegation succeeds after restart");

        assert_eq!(code, 0);
        assert_eq!(
            std::fs::read_to_string(order).expect("order"),
            "bootstrap-ok\nhang\nhealthy\n",
            "the bootstrap must run once outside the main supervisor restart loop"
        );
    }

    #[test]
    fn read_only_goal_is_rejected_before_workspace_setup() {
        let tmp = crate::commands::ctx::testenv::repo();
        let home = tmp.path().join("home");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);
        std::fs::create_dir_all(home.join(".zirv")).expect("config dir");
        let touched = tmp.path().join("must-not-exist");
        std::fs::write(
            home.join(".zirv/ctx.toml"),
            format!(
                "[[workspace]]\nname = 'prepared'\nsetup = [\"touch '{}'\"]\n",
                touched.display()
            ),
        )
        .expect("operator config");
        let mut env = base_env(&tmp.path().join("state"));
        env.insert("HOME".into(), home.display().to_string());
        let mut args = args_for("claude", "go");
        args.goal = Some("prepare".into());
        args.workspace = Some("prepared".into());
        args.mode = WorkerMode::ReadOnly;

        let error = run_with(&args, &mut Vec::new(), tmp.path(), &|key| {
            env.get(key).cloned()
        })
        .expect_err("read-only prep is incompatible");
        assert!(
            error.to_string().contains("requires a writing worker"),
            "{error}"
        );
        assert!(!touched.exists(), "setup must not have run");
    }

    #[test]
    fn manifest_agent_skills_are_real_prompt_defaults() {
        let tmp = crate::commands::ctx::testenv::repo();
        let home = tmp.path().join("home");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);
        let manifest = tmp.path().join("delegation.yaml");
        std::fs::write(&manifest, "agent: debugger\nbrief: go\n").expect("manifest");
        let argv = tmp.path().join("argv.log");
        let _fake = crate::commands::ctx::testenv::VarGuard::set(&[
            ("FAKE_AGENT_MODE", Some("healthy")),
            ("FAKE_AGENT_ARGV_LOG", argv.to_str()),
        ]);
        let mut env = base_env(&tmp.path().join("state"));
        env.insert("HOME".into(), home.display().to_string());
        env.insert("ZIRV_CTX_PACE".into(), "false".into());
        let mut args = args_for("claude", "go");
        args.manifest = Some(manifest);

        assert_eq!(
            run_with(&args, &mut Vec::new(), tmp.path(), &|key| env
                .get(key)
                .cloned())
            .expect("manifest delegation"),
            0
        );
        let argv = std::fs::read_to_string(argv).expect("argv");
        assert!(
            argv.contains("[skill systematic-debugging@"),
            "debugger's default skill must reach the real worker prompt: {argv}"
        );
    }

    #[test]
    fn explicit_workspace_skills_replace_manifest_agent_defaults() {
        let tmp = crate::commands::ctx::testenv::repo();
        let home = tmp.path().join("home");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);
        let manifest = tmp.path().join("delegation.yaml");
        std::fs::write(&manifest, "agent: debugger\nbrief: go\n").expect("manifest");
        std::fs::create_dir_all(home.join(".zirv")).expect("config dir");
        std::fs::write(
            home.join(".zirv/ctx.toml"),
            "[[workspace]]\nname = 'plain'\nskills = []\n",
        )
        .expect("workspace config");
        let argv = tmp.path().join("argv.log");
        let _fake = crate::commands::ctx::testenv::VarGuard::set(&[
            ("FAKE_AGENT_MODE", Some("healthy")),
            ("FAKE_AGENT_ARGV_LOG", argv.to_str()),
        ]);
        let mut env = base_env(&tmp.path().join("state"));
        env.insert("HOME".into(), home.display().to_string());
        env.insert("ZIRV_CTX_PACE".into(), "false".into());
        let mut args = args_for("claude", "go");
        args.manifest = Some(manifest);
        args.workspace = Some("plain".into());

        assert_eq!(
            run_with(&args, &mut Vec::new(), tmp.path(), &|key| env
                .get(key)
                .cloned())
            .expect("workspace delegation"),
            0
        );
        let argv = std::fs::read_to_string(argv).expect("argv");
        assert!(
            !argv.contains("[skill systematic-debugging@"),
            "an explicit workspace skill list replaces manifest defaults: {argv}"
        );
    }

    #[test]
    fn cross_harness_execution_segments_are_both_persisted_and_summed() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = crate::commands::ctx::state::StateDir::from_root(tmp.path().join("state"));
        let report = exec::ExecutionReport {
            segments: vec![
                exec::ExecutionSegment {
                    session: "aaaaaaaa-1111-4111-8111-111111111111".to_string(),
                    agent: "claude".to_string(),
                    model: Some("sonnet".to_string()),
                    usage: TranscriptUsage {
                        input_tokens: 10,
                        cache_creation_input_tokens: 20,
                        cache_read_input_tokens: 30,
                        output_tokens: 40,
                    },
                    wall_ms: 100,
                },
                exec::ExecutionSegment {
                    session: "bbbbbbbb-2222-4222-8222-222222222222".to_string(),
                    agent: "codex".to_string(),
                    model: Some("gpt-5.6-terra".to_string()),
                    usage: TranscriptUsage {
                        input_tokens: 1,
                        cache_creation_input_tokens: 2,
                        cache_read_input_tokens: 3,
                        output_tokens: 4,
                    },
                    wall_ms: 200,
                },
            ],
            final_reservation: None,
        };

        let total = append_execution_segments(
            &state,
            &report,
            "parent",
            Some("wg-1"),
            0,
            "ok",
            WorkerMode::Writing,
            Some(crate::commands::ctx::log::TaskClass::Implement),
            "root",
            None,
        );
        assert_eq!(total.input_tokens, 11);
        assert_eq!(total.cache_creation_input_tokens, 22);
        assert_eq!(total.cache_read_input_tokens, 33);
        assert_eq!(total.output_tokens, 44);

        let rows = crate::commands::ctx::log::tail_delegations(&state, 10).expect("rows");
        assert_eq!(rows.len(), 2, "both vendor segments must be visible");
        assert!(rows.iter().any(|row| row.contains("\"agent\":\"claude\"")));
        assert!(rows.iter().any(|row| row.contains("\"agent\":\"codex\"")));
        assert!(rows.iter().any(|row| row.contains("gpt-5.6-terra")));
        assert!(
            rows.iter()
                .all(|row| row.contains("\"task_class\":\"implement\"")),
            "issue #264: every segment of one logical delegation carries the same task_class: {rows:?}"
        );
    }

    #[test]
    fn a_finished_child_rolls_its_spend_into_the_group_exactly_once() {
        let tmp = crate::commands::ctx::testenv::repo();
        let home = tmp.path().join("home");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);
        let state_path = tmp.path().join("state");
        let state = crate::commands::ctx::state::StateDir::from_root(state_path.clone());
        create_work_group_with_spend(&state, "wg-roll-up", 1_000_000, 10);
        let mut env = base_env(&state_path);
        env.insert("ZIRV_CTX_PACE".to_string(), "false".to_string());
        let mut args = args_for("claude", "do the work");
        args.group = Some("wg-roll-up".to_string());
        unsafe {
            std::env::set_var("FAKE_AGENT_MODE", "healthy");
        }

        let code = run_with(&args, &mut Vec::new(), tmp.path(), &|k| env.get(k).cloned());

        unsafe {
            std::env::remove_var("FAKE_AGENT_MODE");
        }
        assert_eq!(code.expect("runs"), 0);
        let rows = crate::commands::ctx::log::read_delegations(&state, 10);
        let child_spend = rows.iter().fold(0_u64, |total, row| {
            total
                .saturating_add(row.input_tokens)
                .saturating_add(row.cache_creation_input_tokens)
                .saturating_add(row.cache_read_input_tokens)
                .saturating_add(row.output_tokens)
        });
        assert!(child_spend > 0, "fixture must report real usage");
        assert_eq!(
            crate::commands::ctx::group::load(&state, "wg-roll-up")
                .expect("load")
                .expect("present")
                .spent_tokens,
            10 + child_spend,
            "the completion path must add the child's spend once"
        );
    }

    /// Issue #155, Phase 2: the end-to-end write, against a real
    /// `AgentAdapter` (`ClaudeAdapter`) and a real fake-agent transcript --
    /// not just `log.rs`'s own isolated `append_delegation`/`tail_
    /// delegations` round trip. A completed delegation must leave exactly
    /// one `Delegation` record with a real (non-zero) cache-read count read
    /// back off the worker's own transcript, and exactly one
    /// `delegation-complete` line in the main decision log naming the same
    /// verb.
    #[test]
    fn a_completed_delegation_writes_a_checkpoint_record() {
        let tmp = crate::commands::ctx::testenv::repo();
        let home = tmp.path().join("home");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);
        let state_dir = tmp.path().join("state");
        let mut env = base_env(&state_dir);
        env.insert("ZIRV_CTX_PACE".to_string(), "false".to_string());
        unsafe {
            std::env::set_var("FAKE_AGENT_MODE", "healthy");
        }

        let args = args_for("claude", "do the work");
        let mut out = Vec::new();
        let code = run_with(&args, &mut out, tmp.path(), &|k| env.get(k).cloned());
        unsafe {
            std::env::remove_var("FAKE_AGENT_MODE");
        }
        assert_eq!(code.expect("runs"), 0);

        let state = crate::commands::ctx::state::StateDir::resolve(&|k| env.get(k).cloned())
            .expect("state resolves");
        let delegations = crate::commands::ctx::log::tail_delegations(&state, 10).expect("tail");
        assert_eq!(
            delegations.len(),
            1,
            "exactly one checkpoint record per delegation: {delegations:?}"
        );
        let record: serde_json::Value = serde_json::from_str(&delegations[0]).expect("json");
        assert_eq!(record["agent"], "claude");
        // Pins the argv -> model field wiring end-to-end: no `--model` was
        // passed, so `worker_launch_flags` prepends claude's own configured-
        // or-default worker model (`ClaudeAdapter::default_worker_model`,
        // "sonnet" with nothing configured), and `adapters::last_model_flag`
        // must read that exact value back out of the effective argv.
        assert_eq!(record["model"], "sonnet");
        assert_eq!(record["exit_code"], 0);
        assert_eq!(record["outcome"], "ok");
        assert!(
            record["cache_read_input_tokens"].as_u64().unwrap_or(0) > 0,
            "must read real usage back off the worker's own transcript: {record}"
        );
        assert!(
            !record["session"].as_str().unwrap_or("").is_empty(),
            "must carry the worker's own session id: {record}"
        );

        let decisions = crate::commands::ctx::log::tail(&state, 10).expect("tail");
        assert!(
            decisions
                .iter()
                .any(|line| line.contains(crate::commands::ctx::log::DELEGATION_ACTION)),
            "the main decision log must also get a one-line delegation-complete marker: \
             {decisions:?}"
        );
    }

    /// Issue #317: `--task <id>` claims the card before any spawn, and a
    /// successful run marks it `Done` with the delegation's own outcome --
    /// never left `Running`.
    #[test]
    fn run_with_task_claims_before_spawn_and_completes_on_success() {
        let tmp = crate::commands::ctx::testenv::repo();
        let home = tmp.path().join("home");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);
        let state_dir = tmp.path().join("state");
        let mut env = base_env(&state_dir);
        env.insert("ZIRV_CTX_PACE".to_string(), "false".to_string());
        unsafe {
            std::env::set_var("FAKE_AGENT_MODE", "healthy");
        }

        let state = crate::commands::ctx::state::StateDir::resolve(&|k: &str| env.get(k).cloned())
            .expect("state resolves");
        let repo_slug = crate::commands::ctx::state::repo_slug(tmp.path());
        crate::commands::ctx::task::append_event(
            &state,
            &repo_slug,
            &crate::commands::ctx::task::Event::Created {
                id: "task-1".to_string(),
                repo_slug: repo_slug.clone(),
                title: "do the work".to_string(),
                brief: "do the work well".to_string(),
                parents: Vec::new(),
                group_id: None,
                workdir: None,
                at: 1,
            },
        )
        .expect("create task card");

        let mut args = args_for("claude", "do the work");
        args.task = Some("task-1".to_string());
        let mut out = Vec::new();
        let code = run_with(&args, &mut out, tmp.path(), &|k| env.get(k).cloned());
        unsafe {
            std::env::remove_var("FAKE_AGENT_MODE");
        }
        assert_eq!(code.expect("runs"), 0);

        let cards = crate::commands::ctx::task::load_cards(&state, &repo_slug);
        let card = &cards["task-1"];
        assert_eq!(
            card.state,
            crate::commands::ctx::task::State::Done,
            "a successful run closes the card, never leaves it Running"
        );
        assert!(card.outcome.is_some());
    }

    /// Issue #722: a clean-exit (`code == 0`), no-contract `--task`
    /// delegation whose worker produced no extractable final text
    /// (`DelegationState::ExitedNoReport`) must not close its card `Done`
    /// -- that used to happen because `task_exit_kind` was decided from
    /// `code == 0` alone, before `first_text` was known, and was only ever
    /// revised on the declared-`--result-schema` branch. `FAKE_AGENT_TURNS=0`
    /// makes fake-agent.sh exit 0 with an empty transcript, so `first_text`
    /// is `None` while `code` is still `0`. `respawn_decision`'s own
    /// `ExitKind::SilentZero` arm ("card already completed successfully" is
    /// the only path back to `Done`, and it is never reached here) sends
    /// the card back to `Ready` for a respawn instead.
    #[test]
    fn run_with_task_leaves_the_card_open_on_a_clean_exit_with_no_extractable_report() {
        let tmp = crate::commands::ctx::testenv::repo();
        let home = tmp.path().join("home");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);
        let state_dir = tmp.path().join("state");
        let mut env = base_env(&state_dir);
        env.insert("ZIRV_CTX_PACE".to_string(), "false".to_string());
        unsafe {
            std::env::set_var("FAKE_AGENT_MODE", "healthy");
            std::env::set_var("FAKE_AGENT_TURNS", "0");
        }

        let state = crate::commands::ctx::state::StateDir::resolve(&|k: &str| env.get(k).cloned())
            .expect("state resolves");
        let repo_slug = crate::commands::ctx::state::repo_slug(tmp.path());
        crate::commands::ctx::task::append_event(
            &state,
            &repo_slug,
            &crate::commands::ctx::task::Event::Created {
                id: "task-1".to_string(),
                repo_slug: repo_slug.clone(),
                title: "do the work".to_string(),
                brief: "do the work well".to_string(),
                parents: Vec::new(),
                group_id: None,
                workdir: None,
                at: 1,
            },
        )
        .expect("create task card");

        let mut args = args_for("claude", "do the work");
        args.task = Some("task-1".to_string());
        let mut out = Vec::new();
        let code = run_with(&args, &mut out, tmp.path(), &|k| env.get(k).cloned());
        unsafe {
            std::env::remove_var("FAKE_AGENT_MODE");
            std::env::remove_var("FAKE_AGENT_TURNS");
        }
        assert_eq!(
            code.expect("runs"),
            0,
            "the worker process itself exited clean"
        );

        let cards = crate::commands::ctx::task::load_cards(&state, &repo_slug);
        let card = &cards["task-1"];
        assert_eq!(
            card.state,
            crate::commands::ctx::task::State::Ready,
            "a clean exit with no extractable report is a SilentZero, respawn-guarded \
             back to Ready, never closed Done: {card:?}"
        );
        assert!(
            card.outcome.is_none(),
            "no real outcome was ever reported: {card:?}"
        );
    }

    /// Issue #722 regression (orchestrator ruling on deviation 3): a
    /// NONZERO exit with extractable final text must still be a `Crash`,
    /// exactly as it was before this issue's fix -- never `Reported`, which
    /// would close the card `Done` as if the worker had actually succeeded.
    /// `FAKE_AGENT_MODE=fail` writes a healthy transcript (so `first_text`
    /// is `Some`) and then exits 3, with `max_restarts: Some(0)` so the
    /// crash exit code propagates instead of being retried away.
    #[test]
    fn run_with_task_records_a_crash_not_done_on_a_nonzero_exit_with_text() {
        let tmp = crate::commands::ctx::testenv::repo();
        let home = tmp.path().join("home");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);
        let state_dir = tmp.path().join("state");
        let mut env = base_env(&state_dir);
        env.insert("ZIRV_CTX_PACE".to_string(), "false".to_string());
        unsafe {
            std::env::set_var("FAKE_AGENT_MODE", "fail");
        }

        let state = crate::commands::ctx::state::StateDir::resolve(&|k: &str| env.get(k).cloned())
            .expect("state resolves");
        let repo_slug = crate::commands::ctx::state::repo_slug(tmp.path());
        crate::commands::ctx::task::append_event(
            &state,
            &repo_slug,
            &crate::commands::ctx::task::Event::Created {
                id: "task-1".to_string(),
                repo_slug: repo_slug.clone(),
                title: "do the work".to_string(),
                brief: "do the work well".to_string(),
                parents: Vec::new(),
                group_id: None,
                workdir: None,
                at: 1,
            },
        )
        .expect("create task card");

        let mut args = args_for("claude", "do the work");
        args.task = Some("task-1".to_string());
        args.max_restarts = Some(0);
        let mut out = Vec::new();
        let code = run_with(&args, &mut out, tmp.path(), &|k| env.get(k).cloned());
        unsafe {
            std::env::remove_var("FAKE_AGENT_MODE");
        }
        assert_eq!(code.expect("runs"), 3, "the crash exit code propagates");

        let cards = crate::commands::ctx::task::load_cards(&state, &repo_slug);
        let card = &cards["task-1"];
        assert_eq!(
            card.state,
            crate::commands::ctx::task::State::Ready,
            "a nonzero exit with text is a Crash, respawn-guarded back to Ready, \
             never closed Done just because a report happened to be extractable: {card:?}"
        );
        assert!(
            card.outcome.is_none(),
            "no real outcome was ever reported: {card:?}"
        );
        let events = crate::commands::ctx::task::read_events(&state, &repo_slug);
        assert!(
            events
                .iter()
                .any(|e| matches!(e, crate::commands::ctx::task::Event::Crash { .. })),
            "recorded as a crash, not a protocol violation or a completion: {events:?}"
        );
    }

    /// Issue #317 acceptance: a card with an unmet parent cannot be claimed,
    /// so `--task` refuses BEFORE any worker spawns -- exit 2, nothing
    /// launched, and the card is left exactly as it was.
    #[test]
    fn run_with_task_refuses_before_any_spawn_when_a_parent_is_unmet() {
        let tmp = crate::commands::ctx::testenv::repo();
        let home = tmp.path().join("home");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);
        let state_dir = tmp.path().join("state");
        let mut env = base_env(&state_dir);
        env.insert("ZIRV_CTX_PACE".to_string(), "false".to_string());
        unsafe {
            std::env::set_var("FAKE_AGENT_MODE", "healthy");
        }

        let state = crate::commands::ctx::state::StateDir::resolve(&|k: &str| env.get(k).cloned())
            .expect("state resolves");
        let repo_slug = crate::commands::ctx::state::repo_slug(tmp.path());
        crate::commands::ctx::task::append_event(
            &state,
            &repo_slug,
            &crate::commands::ctx::task::Event::Created {
                id: "parent-1".to_string(),
                repo_slug: repo_slug.clone(),
                title: "parent".to_string(),
                brief: "b".to_string(),
                parents: Vec::new(),
                group_id: None,
                workdir: None,
                at: 1,
            },
        )
        .expect("create parent");
        crate::commands::ctx::task::append_event(
            &state,
            &repo_slug,
            &crate::commands::ctx::task::Event::Created {
                id: "child-1".to_string(),
                repo_slug: repo_slug.clone(),
                title: "child".to_string(),
                brief: "b".to_string(),
                parents: vec!["parent-1".to_string()],
                group_id: None,
                workdir: None,
                at: 1,
            },
        )
        .expect("create child");

        let mut args = args_for("claude", "do the work");
        args.task = Some("child-1".to_string());
        let mut out = Vec::new();
        let code = run_with(&args, &mut out, tmp.path(), &|k| env.get(k).cloned());
        unsafe {
            std::env::remove_var("FAKE_AGENT_MODE");
        }
        assert_eq!(code.expect("runs"), 2, "refused before any spawn");

        let cards = crate::commands::ctx::task::load_cards(&state, &repo_slug);
        assert_eq!(
            cards["child-1"].state,
            crate::commands::ctx::task::State::Todo,
            "the refused card is left exactly as it was"
        );
        assert!(
            crate::commands::ctx::log::tail_delegations(&state, 10)
                .expect("tail")
                .is_empty(),
            "nothing was ever launched"
        );
    }

    /// Issue #155 review finding D2: a completed headless delegation must
    /// record the group it actually ran under -- before this fix `agent.rs`
    /// hardcoded `work_group_id: None` on every completion log record, so
    /// `zirv ctx status`'s group tree rendered every delegation as
    /// ungrouped no matter what `--group` was passed.
    #[test]
    fn a_completed_delegation_records_its_own_work_group_id() {
        let tmp = crate::commands::ctx::testenv::repo();
        let home = tmp.path().join("home");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);
        let state_dir = tmp.path().join("state");
        let mut env = base_env(&state_dir);
        env.insert("ZIRV_CTX_PACE".to_string(), "false".to_string());
        let state = crate::commands::ctx::state::StateDir::from_root(state_dir.clone());
        let group_id = crate::commands::ctx::group::run_create(
            &state,
            &mut Vec::new(),
            &crate::commands::ctx::group::CreateArgs {
                scope: "test batch".to_string(),
                child_limit: 3,
                token_budget: None,
                deadline_secs: None,
                completion_contract: "report by mail".to_string(),
                parent_session: None,
            },
            1_700_000_000,
        )
        .expect("group create");

        unsafe {
            std::env::set_var("FAKE_AGENT_MODE", "healthy");
        }
        let mut args = args_for("claude", "do the work");
        args.group = Some(group_id.clone());
        let mut out = Vec::new();
        let code = run_with(&args, &mut out, tmp.path(), &|k| env.get(k).cloned());
        unsafe {
            std::env::remove_var("FAKE_AGENT_MODE");
        }
        assert_eq!(code.expect("runs"), 0);

        let delegations = crate::commands::ctx::log::tail_delegations(&state, 10).expect("tail");
        assert_eq!(delegations.len(), 1);
        let record: serde_json::Value = serde_json::from_str(&delegations[0]).expect("json");
        assert_eq!(
            record["work_group_id"].as_str(),
            Some(group_id.as_str()),
            "the completion record must name the real group: {record}"
        );
    }

    /// Issue #170: a SubOrchestrator's group closes automatically once its
    /// own supervised run ends -- "when a sub-orchestrator finishes its
    /// scope, its group closes" -- with no separate `zirv ctx group close`
    /// step required.
    #[test]
    fn a_completed_sub_orchestrator_delegation_closes_its_own_claimed_group() {
        let tmp = crate::commands::ctx::testenv::repo();
        let home = tmp.path().join("home");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);
        let state_dir = tmp.path().join("state");
        let mut env = base_env(&state_dir);
        env.insert("ZIRV_CTX_PACE".to_string(), "false".to_string());
        let state = crate::commands::ctx::state::StateDir::from_root(state_dir.clone());
        let group_id = crate::commands::ctx::group::run_create(
            &state,
            &mut Vec::new(),
            &crate::commands::ctx::group::CreateArgs {
                scope: "own the frontend rewrite".to_string(),
                child_limit: 3,
                token_budget: None,
                deadline_secs: None,
                completion_contract: "report by mail".to_string(),
                parent_session: None,
            },
            1_700_000_000,
        )
        .expect("group create");

        unsafe {
            std::env::set_var("FAKE_AGENT_MODE", "healthy");
        }
        let mut args = args_for("claude", "do the work");
        args.role = Some("sub-orchestrator".to_string());
        args.group = Some(group_id.clone());
        let mut out = Vec::new();
        let code = run_with(&args, &mut out, tmp.path(), &|k| env.get(k).cloned());
        unsafe {
            std::env::remove_var("FAKE_AGENT_MODE");
        }
        assert_eq!(code.expect("runs"), 0);

        let group = crate::commands::ctx::group::load(&state, &group_id)
            .expect("load")
            .expect("present");
        assert!(
            group.closed_at.is_some(),
            "the sub-orchestrator's own scope finished; the group must close itself"
        );
        assert!(
            group.sub_orchestrator_session.is_some(),
            "the group must name who claimed and closed it"
        );
    }

    /// Security review round 2 (Finding 3): the headless fork of a
    /// coordinator delegation resolved (and here mints) a group, but exported
    /// nothing -- only the dashboard fork pushed `WORK_GROUP_ENV` into its
    /// pane's environment. So a headless sub-orchestrator's own children
    /// resolved `group = None`: no admission, no child limit, no token
    /// ceiling, "ungrouped" in the status tree. The child's real environment
    /// is what proves the fix, read back through the fixture's own env log.
    #[test]
    fn a_headless_coordinators_child_inherits_the_group_it_was_bound_to() {
        let tmp = crate::commands::ctx::testenv::repo();
        let home = tmp.path().join("home");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);
        let state_dir = tmp.path().join("state");
        let mut env = base_env(&state_dir);
        env.insert("ZIRV_CTX_PACE".to_string(), "false".to_string());
        let state = crate::commands::ctx::state::StateDir::from_root(state_dir.clone());
        let group_env_log = tmp.path().join("group-env.log");

        unsafe {
            std::env::set_var("FAKE_AGENT_MODE", "healthy");
            std::env::set_var("FAKE_AGENT_GROUP_ENV_LOG", &group_env_log);
        }
        let mut args = args_for("claude", "own this scope");
        args.role = Some("sub-orchestrator".to_string());
        args.scope = Some("the frontend rewrite".to_string());
        let mut out = Vec::new();
        let code = run_with(&args, &mut out, tmp.path(), &|k| env.get(k).cloned());
        unsafe {
            std::env::remove_var("FAKE_AGENT_MODE");
            std::env::remove_var("FAKE_AGENT_GROUP_ENV_LOG");
        }
        assert_eq!(code.expect("runs"), 0);

        let groups = crate::commands::ctx::group::list(&state);
        assert_eq!(groups.len(), 1, "the scope minted exactly one group");
        let inherited = std::fs::read_to_string(&group_env_log).expect("the child logged its env");
        assert_eq!(
            inherited.trim(),
            groups[0].work_group_id,
            "the coordinator's own child must carry the group it was bound to"
        );
    }

    /// Issue #236: every headless delegation sets `ZIRV_CTX_HEADLESS` on the
    /// child, read by `engine::refusal_for` to refuse the `brainstorm` skill.
    #[test]
    fn a_headless_delegation_sets_the_headless_env_marker() {
        let tmp = crate::commands::ctx::testenv::repo();
        let home = tmp.path().join("home");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);
        let state_dir = tmp.path().join("state");
        let mut env = base_env(&state_dir);
        env.insert("ZIRV_CTX_PACE".to_string(), "false".to_string());
        let headless_env_log = tmp.path().join("headless-env.log");

        unsafe {
            std::env::set_var("FAKE_AGENT_MODE", "healthy");
            std::env::set_var("FAKE_AGENT_HEADLESS_ENV_LOG", &headless_env_log);
        }
        let args = args_for("claude", "do the work");
        let mut out = Vec::new();
        let code = run_with(&args, &mut out, tmp.path(), &|k| env.get(k).cloned());
        unsafe {
            std::env::remove_var("FAKE_AGENT_MODE");
            std::env::remove_var("FAKE_AGENT_HEADLESS_ENV_LOG");
        }
        assert_eq!(code.expect("runs"), 0);
        assert_eq!(
            std::fs::read_to_string(&headless_env_log)
                .expect("the child logged its env")
                .trim(),
            "1"
        );
    }

    /// The other half of Finding 3, end to end: a child that inherits that
    /// exact environment -- a `zirv ctx agent` call with no `--group` of its
    /// own, run from inside a coordinator's harness -- resolves the SAME
    /// group and is admitted against its child limit, which is what the
    /// missing export cost.
    #[test]
    fn a_child_that_inherits_the_group_env_is_admitted_into_that_same_group() {
        let tmp = crate::commands::ctx::testenv::repo();
        let home = tmp.path().join("home");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);
        let state_dir = tmp.path().join("state");
        let mut env = base_env(&state_dir);
        env.insert("ZIRV_CTX_PACE".to_string(), "false".to_string());
        let state = crate::commands::ctx::state::StateDir::from_root(state_dir.clone());
        let group_id = crate::commands::ctx::group::run_create(
            &state,
            &mut Vec::new(),
            &crate::commands::ctx::group::CreateArgs {
                scope: "the frontend rewrite".to_string(),
                child_limit: 3,
                token_budget: None,
                deadline_secs: None,
                completion_contract: "report by mail".to_string(),
                parent_session: None,
            },
            1_700_000_000,
        )
        .expect("group create");
        // Exactly what the coordinator's child process inherits, per the test
        // above: the binding in the environment, and no `--group` typed.
        env.insert(WORK_GROUP_ENV.to_string(), group_id.clone());

        unsafe {
            std::env::set_var("FAKE_AGENT_MODE", "healthy");
        }
        let args = args_for("claude", "do a slice of it");
        let mut out = Vec::new();
        let code = run_with(&args, &mut out, tmp.path(), &|k| env.get(k).cloned());
        unsafe {
            std::env::remove_var("FAKE_AGENT_MODE");
        }
        assert_eq!(code.expect("runs"), 0);

        let group = crate::commands::ctx::group::load(&state, &group_id)
            .expect("load")
            .expect("present");
        assert_eq!(
            group.admitted_children, 1,
            "the inherited binding is what makes `admit_child` fire at all"
        );
        assert!(
            group.closed_at.is_none(),
            "a plain worker child never closes its coordinator's group"
        );
        let delegations = crate::commands::ctx::log::tail_delegations(&state, 10).expect("tail");
        let record: serde_json::Value = serde_json::from_str(&delegations[0]).expect("json");
        assert_eq!(
            record["work_group_id"].as_str(),
            Some(group_id.as_str()),
            "and the delegation is recorded inside that group, not as ungrouped: {record}"
        );
    }

    /// Issue #249, design A: a headless `zirv agent` delegation's own child
    /// carries `PARENT_SESSION_ENV`, set from the DELEGATING session's own
    /// identity (`ZIRV_CTX_SESSION` on the process that ran `zirv agent`) --
    /// end to end, through the real child process's own environment, read
    /// back via the fixture's env log (mirrors `a_headless_coordinators_
    /// child_inherits_the_group_it_was_bound_to`'s own proof shape).
    #[test]
    fn a_headless_delegations_child_carries_the_delegating_sessions_identity_as_its_parent() {
        let tmp = crate::commands::ctx::testenv::repo();
        let home = tmp.path().join("home");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);
        let state_dir = tmp.path().join("state");
        let mut env = base_env(&state_dir);
        env.insert("ZIRV_CTX_PACE".to_string(), "false".to_string());
        env.insert(
            crate::commands::ctx::adapters::SESSION_ENV.to_string(),
            "orch0001-2222-4333-8444-555555555555".to_string(),
        );
        let parent_env_log = tmp.path().join("parent-env.log");

        unsafe {
            std::env::set_var("FAKE_AGENT_MODE", "healthy");
            std::env::set_var("FAKE_AGENT_PARENT_ENV_LOG", &parent_env_log);
        }
        let args = args_for("claude", "do the work");
        let mut out = Vec::new();
        let code = run_with(&args, &mut out, tmp.path(), &|k| env.get(k).cloned());
        unsafe {
            std::env::remove_var("FAKE_AGENT_MODE");
            std::env::remove_var("FAKE_AGENT_PARENT_ENV_LOG");
        }
        assert_eq!(code.expect("runs"), 0);

        let logged = std::fs::read_to_string(&parent_env_log).expect("the child logged its env");
        assert_eq!(
            logged.trim(),
            "orch0001",
            "the child must see the DELEGATING session's own short id as its parent"
        );
    }

    /// The "never rely on inheritance" half of design A: this delegation's
    /// OWN process env already carries a `PARENT_SESSION_ENV` (as if it were
    /// itself a worker calling `zirv agent` again to spawn a further child).
    /// The new child must see THIS session's own id, never the grandparent's
    /// -- `parent_session_env`'s whole reason for never falling through.
    #[test]
    fn a_headless_delegations_child_never_inherits_a_grandparents_parent_session() {
        let tmp = crate::commands::ctx::testenv::repo();
        let home = tmp.path().join("home");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);
        let state_dir = tmp.path().join("state");
        let mut env = base_env(&state_dir);
        env.insert("ZIRV_CTX_PACE".to_string(), "false".to_string());
        // This process's OWN identity -- what the new child's parent must
        // become.
        env.insert(
            crate::commands::ctx::adapters::SESSION_ENV.to_string(),
            "worker01-2222-4333-8444-555555555555".to_string(),
        );
        // A stray, already-inherited value from further up the chain -- must
        // never leak through to the new child.
        env.insert(PARENT_SESSION_ENV.to_string(), "grandpar".to_string());
        let parent_env_log = tmp.path().join("parent-env.log");

        unsafe {
            std::env::set_var("FAKE_AGENT_MODE", "healthy");
            std::env::set_var("FAKE_AGENT_PARENT_ENV_LOG", &parent_env_log);
        }
        let args = args_for("claude", "do the work");
        let mut out = Vec::new();
        let code = run_with(&args, &mut out, tmp.path(), &|k| env.get(k).cloned());
        unsafe {
            std::env::remove_var("FAKE_AGENT_MODE");
            std::env::remove_var("FAKE_AGENT_PARENT_ENV_LOG");
        }
        assert_eq!(code.expect("runs"), 0);

        let logged = std::fs::read_to_string(&parent_env_log).expect("the child logged its env");
        assert_eq!(
            logged.trim(),
            "worker01",
            "the child must see THIS session's own id, never the inherited grandparent's"
        );
    }

    /// No identified delegating session (an operator's own raw terminal, no
    /// `ZIRV_CTX_SESSION` at all) means no parent -- the child's env carries
    /// no `PARENT_SESSION_ENV` at all, not an empty or placeholder value.
    #[test]
    fn a_headless_delegation_with_no_identified_delegating_session_sets_no_parent_env() {
        let tmp = crate::commands::ctx::testenv::repo();
        let home = tmp.path().join("home");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);
        let state_dir = tmp.path().join("state");
        let mut env = base_env(&state_dir);
        env.insert("ZIRV_CTX_PACE".to_string(), "false".to_string());
        let parent_env_log = tmp.path().join("parent-env.log");

        unsafe {
            std::env::set_var("FAKE_AGENT_MODE", "healthy");
            std::env::set_var("FAKE_AGENT_PARENT_ENV_LOG", &parent_env_log);
        }
        let args = args_for("claude", "do the work");
        let mut out = Vec::new();
        let code = run_with(&args, &mut out, tmp.path(), &|k| env.get(k).cloned());
        unsafe {
            std::env::remove_var("FAKE_AGENT_MODE");
            std::env::remove_var("FAKE_AGENT_PARENT_ENV_LOG");
        }
        assert_eq!(code.expect("runs"), 0);

        let logged = std::fs::read_to_string(&parent_env_log).expect("the child logged its env");
        assert_eq!(
            logged.trim(),
            "",
            "with no identified delegating session, the child gets no parent at all"
        );
    }

    /// The other half: a plain WORKER delegation into the same group must
    /// never close it -- only the coordinator that owns a group's scope may
    /// finish it. Otherwise the very first worker to complete would close a
    /// batch its siblings are still working through.
    #[test]
    fn a_completed_plain_worker_delegation_never_closes_its_group() {
        let tmp = crate::commands::ctx::testenv::repo();
        let home = tmp.path().join("home");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);
        let state_dir = tmp.path().join("state");
        let mut env = base_env(&state_dir);
        env.insert("ZIRV_CTX_PACE".to_string(), "false".to_string());
        let state = crate::commands::ctx::state::StateDir::from_root(state_dir.clone());
        let group_id = crate::commands::ctx::group::run_create(
            &state,
            &mut Vec::new(),
            &crate::commands::ctx::group::CreateArgs {
                scope: "own the frontend rewrite".to_string(),
                child_limit: 3,
                token_budget: None,
                deadline_secs: None,
                completion_contract: "report by mail".to_string(),
                parent_session: None,
            },
            1_700_000_000,
        )
        .expect("group create");

        unsafe {
            std::env::set_var("FAKE_AGENT_MODE", "healthy");
        }
        let mut args = args_for("claude", "do the work");
        args.group = Some(group_id.clone());
        let mut out = Vec::new();
        let code = run_with(&args, &mut out, tmp.path(), &|k| env.get(k).cloned());
        unsafe {
            std::env::remove_var("FAKE_AGENT_MODE");
        }
        assert_eq!(code.expect("runs"), 0);

        let group = crate::commands::ctx::group::load(&state, &group_id)
            .expect("load")
            .expect("present");
        assert!(
            group.closed_at.is_none(),
            "a plain worker completing must never close the group it ran in"
        );
        assert!(group.sub_orchestrator_session.is_none());
    }
}
