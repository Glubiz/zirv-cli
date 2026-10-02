//! `zirv workflow` CLI arguments, subcommand dispatch (`run`), starting a
//! new workflow, and auto-spawning the next phase's worker
//! (issue #542-split).

use std::io::Write;
use std::path::{Path, PathBuf};

use clap::{Args, Subcommand};
use serde::Serialize;

use crate::commands::ctx::CtxResult;
use crate::commands::ctx::state::{StateDir, now_secs};
use crate::commands::workflow::agents::AgentRegistry;
use crate::commands::workflow::classify::{self, Classification, Complexity, RiskBand, WorkDomain};
use crate::commands::workflow::skill::{SkillRegistry, WorkflowPhase};

use super::definitions::*;
use super::lifecycle::*;
use super::state::*;
use super::transition::*;
#[derive(Debug, Args)]
pub struct WorkflowArgs {
    #[command(subcommand)]
    pub command: WorkflowSubcommand,
}

#[derive(Debug, Subcommand)]
pub enum WorkflowSubcommand {
    /// List built-in workflow definitions.
    List(OutputArgs),
    /// Show a workflow definition.
    Show(ShowArgs),
    /// Classify a task without starting a workflow.
    Classify(classify::ClassifyArgs),
    /// Start and persist a workflow.
    Start(StartArgs),
    /// Show one workflow instance, or the active one.
    Status(StatusArgs),
    /// Restore a running workflow as this repository's active workflow.
    Resume(StateIdArgs),
    /// Force a persisted workflow's methodology overlay, preserving
    /// completed steps and accepted artifacts (#255 recovery path).
    Reclassify(ReclassifyArgs),
    /// Print only the current step's resolved skill context.
    Context(StatusArgs),
    /// Inspect committed workflow work-product artifacts and acceptance state.
    Artifacts(ArtifactsArgs),
    /// Inspect provider-neutral workflow seats and their trust provenance.
    Agents(crate::commands::workflow::agents::AgentArgs),
    /// Compile, show, and brief the proportional team for a request (issue
    /// #541).
    Team(crate::commands::workflow::team::TeamArgs),
    /// Approve the current gated step.
    Approve(StateIdArgs),
    /// Record a step result and transition the state machine.
    Advance(AdvanceArgs),
    /// Close a workflow that will not reach `Completed` (for example one
    /// whose review/fix loop hit `MAX_FIX_REVIEW_ROUNDS`), recording residual
    /// dispositions first. Refuses while any review finding is `Open` or the
    /// workflow is `AwaitingApproval`; clears it as this repository's active
    /// workflow.
    Close(CloseArgs),
    /// Build compact review packages and persist finding dispositions,
    /// including `review dispose --apply-recommended` -- see
    /// [`apply_recommended_dispositions`]'s own doc comment.
    Review(crate::commands::workflow::review::ReviewArgs),
    /// Run deterministic operator-configured maintenance detectors.
    Maintain(crate::commands::workflow::maintain::MaintainArgs),
    /// Aggregate privacy-conscious local workflow telemetry.
    Stats(crate::commands::workflow::telemetry::StatsArgs),
    /// Read-only: aggregate recorded workflow outcomes and propose one-step
    /// heavier/lighter routing per bucket (issue #757). Never changes config.
    Calibrate(crate::commands::workflow::outcomes::CalibrateArgs),
    /// Read-only: reconciles `delegations.jsonl`/`jev-decisions.jsonl`/
    /// `jev-effects.jsonl`/`proxy-decisions.jsonl` under `--state-dir` into
    /// one attributable `SpendReport` (issue #800). Never a new ledger.
    Spend(crate::commands::ctx::attribution::SpendArgs),
    /// Plan, run, inspect, and report a bounded, resumable autoresearch
    /// campaign (issue #802): baseline first, paired screen/validate/
    /// holdout stages, explicit budgets, never auto-applied.
    Research(crate::commands::workflow::research::ResearchArgs),
}

#[derive(Debug, Args)]
pub struct OutputArgs {
    #[arg(long)]
    pub json: bool,
    /// Ignore operator-global and repository-provided workflow packs.
    #[arg(long)]
    pub built_in_only: bool,
    /// Repository root; defaults to the current directory.
    #[arg(long)]
    pub repo: Option<PathBuf>,
}

#[derive(Debug, Args)]
pub struct ShowArgs {
    /// A registry id -- one of the five built-in kind ids (`feature`,
    /// `bugfix`, `refactor`, `spike`, `review`) or any operator-global/
    /// repository pack id (issue #542; was a closed `WorkflowKind` value
    /// before, so the five kind spellings keep working unchanged).
    pub id: String,
    #[arg(long)]
    pub json: bool,
    /// Ignore operator-global and repository-provided workflow packs.
    #[arg(long)]
    pub built_in_only: bool,
    /// Repository root; defaults to the current directory.
    #[arg(long)]
    pub repo: Option<PathBuf>,
}

#[derive(Debug, Args)]
pub struct StartArgs {
    /// A registry id. Omit it and `workflow start` deterministically SELECTS
    /// one via `selection::select_definition` against `--task` and the
    /// resolved classification (issue #542 chunk 3b); an explicit id always
    /// wins outright, with no selection performed at all.
    pub id: Option<String>,
    #[arg(long)]
    pub task: String,
    /// Harness adapter used for capability preflight (for example claude/codex).
    #[arg(long)]
    pub agent: Option<String>,
    /// Ignore operator-global and repository-provided skill/agent overrides.
    #[arg(long)]
    pub built_in_only: bool,
    #[arg(long)]
    pub repo: Option<PathBuf>,
    #[arg(long = "path")]
    pub paths: Vec<PathBuf>,
    #[arg(long)]
    pub changed_lines: Option<usize>,
    #[arg(long)]
    pub tests_changed: bool,
    #[arg(long, value_enum)]
    pub complexity: Option<Complexity>,
    #[arg(long, value_enum)]
    pub risk: Option<RiskBand>,
    /// The branch this workflow gates, when it differs from `--repo`'s own
    /// checked-out branch (issue #467: an orchestrator in the main checkout
    /// starting a workflow for a worker's feature branch it does not have
    /// checked out here). Classification diffs this branch against its own
    /// base as refs, not `--repo`'s working tree. Recorded on the workflow
    /// and matched against a linked worktree's own recorded branch when the
    /// Test/Verify gate widens its read to that worktree's evidence.
    #[arg(long)]
    pub branch: Option<String>,
    /// Repository whose frontend the auto-run detector/render evidence
    /// should scan for a Frontend-profile workflow, when it differs from
    /// `--repo` (for example a workflow tracked in this repo whose frontend
    /// lives in a sibling checkout).
    #[arg(long)]
    pub frontend_root: Option<PathBuf>,
    /// Force the interactive `brainstorm` skill at the intent step.
    #[arg(long, conflicts_with = "no_brainstorm")]
    pub brainstorm: bool,
    /// Force the autonomous `write-intent` skill at the intent step.
    #[arg(long)]
    pub no_brainstorm: bool,
    /// Force this workflow's methodology overlay instead of trusting
    /// automatic classification (#255 recovery path: a misclassified
    /// profile can otherwise only be fixed by abandoning the workflow).
    #[arg(long, value_enum)]
    pub profile: Option<WorkflowProfile>,
    #[arg(long)]
    pub json: bool,
}

impl StartArgs {
    pub(crate) fn brainstorm_override(&self) -> Option<bool> {
        if self.brainstorm {
            Some(true)
        } else if self.no_brainstorm {
            Some(false)
        } else {
            None
        }
    }
}

#[derive(Debug, Args)]
pub struct StatusArgs {
    pub id: Option<String>,
    #[arg(long)]
    pub repo: Option<PathBuf>,
    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, Args)]
pub struct StateIdArgs {
    pub id: String,
    #[arg(long)]
    pub repo: Option<PathBuf>,
}

#[derive(Debug, Args)]
pub struct CloseArgs {
    pub id: String,
    #[arg(long)]
    pub repo: Option<PathBuf>,
    /// Operator-supplied reason for closing without reaching `Completed`,
    /// recorded on the workflow state.
    #[arg(long)]
    pub reason: Option<String>,
    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, Args)]
pub struct ReclassifyArgs {
    pub id: String,
    #[arg(long)]
    pub repo: Option<PathBuf>,
    #[arg(long, value_enum)]
    pub profile: WorkflowProfile,
    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, Args)]
pub struct ArtifactsArgs {
    pub id: String,
    #[arg(long)]
    pub repo: Option<PathBuf>,
    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, Clone, Serialize)]
pub(super) struct WorkflowArtifactStatus {
    stage: ArtifactStage,
    rel_path: String,
    exists: bool,
    accepted: bool,
    drifted: bool,
    accepted_at: Option<String>,
}

pub(super) fn workflow_artifact_statuses(
    state: &WorkflowState,
) -> CtxResult<Vec<WorkflowArtifactStatus>> {
    let mut statuses = Vec::new();
    for stage in [
        ArtifactStage::Intent,
        ArtifactStage::Spec,
        ArtifactStage::Plan,
    ] {
        let Some(record) = state.artifacts.get(stage.key()) else {
            continue;
        };
        let path = workflow_artifact_path(state, stage)?;
        let exists = path.exists();
        let accepted = record.accepted_hash.is_some();
        let drifted = match record.accepted_hash.as_deref() {
            Some(hash) => !exists || artifact_hash(&path)? != hash,
            None => false,
        };
        statuses.push(WorkflowArtifactStatus {
            stage,
            rel_path: record.rel_path.clone(),
            exists,
            accepted,
            drifted,
            accepted_at: record.accepted_at.clone(),
        });
    }
    Ok(statuses)
}

#[derive(Debug, Args)]
pub struct AdvanceArgs {
    pub id: String,
    /// Required unless `--run-checks` is set, which determines the outcome
    /// itself from the evidence command's own result.
    #[arg(long, value_enum, required_unless_present = "run_checks")]
    pub outcome: Option<StepOutcome>,
    /// For a `Test`/`Verify` step, run the step's own required evidence
    /// command in-process (`zirv test changed` for `Test`, `zirv verify` for
    /// `Verify`) and advance on success, printing the evidence summary; on
    /// failure, print it and do not advance. Collapses "run the gate, then
    /// advance" into one call. Conflicts with `--outcome`, which the checks'
    /// own result determines instead.
    #[arg(long, conflicts_with = "outcome")]
    pub run_checks: bool,
    #[arg(long)]
    pub repo: Option<PathBuf>,
    #[arg(long)]
    pub json: bool,
    #[arg(long)]
    pub duration_ms: Option<u64>,
    #[arg(long)]
    pub agent: Option<String>,
    #[arg(long)]
    pub model: Option<String>,
    #[arg(long)]
    pub role: Option<String>,
    #[arg(long)]
    pub input_tokens: Option<u64>,
    #[arg(long)]
    pub output_tokens: Option<u64>,
    #[arg(long, default_value_t = 0)]
    pub workers: u32,
    /// Set (or update) the sibling repository whose frontend the auto-run
    /// detector/render evidence should scan for this workflow, for example
    /// once it becomes clear the tracked repo isn't the one under test.
    #[arg(long)]
    pub frontend_root: Option<PathBuf>,
    /// Accept the frontend detector's pre-existing (not newly introduced)
    /// blocking findings so this advance can proceed; introduced blocking
    /// findings always still fail. Recorded on the workflow and applies for
    /// the rest of it once accepted.
    #[arg(long)]
    pub accept_preexisting_findings: bool,
}

pub(crate) fn resolve_repo(repo: Option<&Path>) -> CtxResult<PathBuf> {
    Ok(match repo {
        Some(path) => path.canonicalize().unwrap_or_else(|_| path.to_path_buf()),
        None => std::env::current_dir()?,
    })
}

pub(super) fn report_registry_warnings(
    registry: &crate::commands::workflow::registry::WorkflowRegistry,
) {
    for warning in registry.warnings() {
        crate::output::warn(warning);
    }
}

/// Load the layered registry for this repository, with optional built-in-only mode. (#542)
pub(crate) fn load_workflow_registry(
    repo: &Path,
    built_in_only: bool,
) -> CtxResult<crate::commands::workflow::registry::WorkflowRegistry> {
    let skills = SkillRegistry::load_for_repo(repo, dirs::home_dir().as_deref(), !built_in_only)?;
    crate::commands::workflow::registry::WorkflowRegistry::load_for_repo(
        repo,
        dirs::home_dir().as_deref(),
        !built_in_only,
        &skills,
    )
}

/// Share one plain-text workflow list between the CLI and native slash command. (#542)
pub(crate) fn write_registry_list(
    writer: &mut impl Write,
    entries: &[&crate::commands::workflow::registry::RegisteredWorkflow],
) -> CtxResult<()> {
    writeln!(writer, "ID\tLAYER\tVERSION\tHASH\tDOMAINS")?;
    for workflow in entries {
        writeln!(
            writer,
            "{}\t{}\t{}\t{}\t{}",
            workflow.definition.id,
            workflow.source,
            workflow.definition.version,
            &workflow.hash[..workflow.hash.len().min(12)],
            workflow.definition.domains.join(",")
        )?;
    }
    Ok(())
}

/// The plain-text rendering `workflow show <id>` prints without `--json` --
/// shared verbatim with the native `/workflow <id>` slash command (issue
/// #542 chunk 3b, decision 5) so the two surfaces can never drift apart.
pub(crate) fn write_registry_entry(
    writer: &mut impl Write,
    workflow: &crate::commands::workflow::registry::RegisteredWorkflow,
) -> CtxResult<()> {
    writeln!(
        writer,
        "{}@{} ({}): {}",
        workflow.definition.id,
        workflow.definition.version,
        workflow.source,
        workflow.definition.description
    )?;
    for step in &workflow.definition.steps {
        writeln!(
            writer,
            "  {}\t{}\tskills={}\tagent_role={}\tartifact={}\tdepends_on={}\twhen={:?}\tapproval={}",
            step.id,
            step.phase,
            step.skills.join(","),
            step.agent_role.as_deref().unwrap_or("-"),
            step.artifact
                .map(|stage| stage.to_string())
                .unwrap_or_else(|| "-".into()),
            if step.depends_on.is_empty() {
                "-".to_string()
            } else {
                step.depends_on.join(",")
            },
            step.condition,
            step.approval
        )?;
    }
    Ok(())
}

/// Share one start-result renderer between the CLI and native slash command. (#542)
pub(crate) fn write_start_outcome(
    writer: &mut impl Write,
    outcome: &StartOutcome,
    json: bool,
) -> CtxResult<()> {
    if outcome.work_dir_gitignored {
        writeln!(
            writer,
            "warning: .zirv/work is ignored by this repository's .gitignore -- workflow artifacts will not be tracked by git"
        )?;
    }
    match (&outcome.selection, json) {
        (Some(selection), true) => {
            let mut value = serde_json::to_value(&outcome.state)?;
            if let serde_json::Value::Object(map) = &mut value {
                map.insert("selection".into(), serde_json::to_value(selection)?);
            }
            serde_json::to_writer_pretty(&mut *writer, &value)?;
            writeln!(writer)?;
        }
        // `write_state` already prints persisted selection, so avoid a duplicate line. (#542)
        (Some(_), false) => write_state(writer, &outcome.state, false)?,
        (None, json) => write_state(writer, &outcome.state, json)?,
    }
    Ok(())
}

/// Show the pinned definition and any registry drift; registry read failures do not break status. (#542)
pub(crate) fn write_definition_status(
    writer: &mut impl Write,
    state: &WorkflowState,
) -> CtxResult<()> {
    let Some(reference) = &state.definition else {
        return Ok(());
    };
    writeln!(
        writer,
        "definition: {}@{} ({}) [{}]",
        reference.id,
        reference.version,
        &reference.hash[..reference.hash.len().min(12)],
        reference.source_layer
    )?;
    let current_hash = load_workflow_registry(&state.repo, false)
        .ok()
        .and_then(|registry| registry.get(&reference.id).ok().map(|w| w.hash.clone()));
    match current_hash {
        Some(hash) if hash == reference.hash => {}
        Some(_) => writeln!(writer, "definition drifted from registry")?,
        None => writeln!(
            writer,
            "definition drifted from registry (id no longer resolves)"
        )?,
    }
    Ok(())
}

/// Resolves and validates `--frontend-root`: absolutized against the current
/// directory, then required to exist and be a directory so a typo fails
/// loudly at parse time instead of surfacing later as "0 files scanned".
pub(super) fn resolve_frontend_root(path: &Path) -> CtxResult<PathBuf> {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()?.join(path)
    };
    let canonical = absolute.canonicalize().map_err(|err| {
        format!(
            "frontend root '{}' does not exist: {err}",
            absolute.display()
        )
    })?;
    if !canonical.is_dir() {
        return Err(format!("frontend root '{}' is not a directory", canonical.display()).into());
    }
    Ok(canonical)
}

pub(crate) fn resolve_state() -> CtxResult<StateDir> {
    StateDir::resolve(&|key| std::env::var(key).ok())
}

/// Run Test/Verify checks in process, skip unchanged failed worktrees, and gate only on a newly persisted report evaluated with the operator baseline. (#287)
pub(super) fn run_required_checks(
    state_dir: &StateDir,
    repo: &Path,
    phase: WorkflowPhase,
    step_id: &str,
    attempts_so_far: u8,
    branch: &str,
    writer: &mut impl Write,
) -> CtxResult<crate::commands::workflow::verification::GateOutcome> {
    if !matches!(phase, WorkflowPhase::Test | WorkflowPhase::Verify) {
        return Err(format!(
            "--run-checks only applies to Test/Verify steps; the current step is {phase:?} -- \
             pass --outcome instead"
        )
        .into());
    }
    let fingerprint = crate::commands::workflow::verification::change_fingerprint(repo)?;
    if let Some(last_failure) =
        crate::commands::workflow::verification::last_failure_fingerprint(state_dir, repo, step_id)?
        && last_failure == fingerprint
    {
        return Ok(
            crate::commands::workflow::verification::GateOutcome::Unchanged {
                fingerprint,
                since_attempt: attempts_so_far.saturating_add(1),
            },
        );
    }
    let before = crate::commands::workflow::verification::latest_report_id(state_dir, repo)?;
    let run_args = crate::commands::workflow::verification::RunArgs {
        repo: Some(repo.to_path_buf()),
        checks: Vec::new(),
        dry_run: false,
        json: false,
    };
    let final_only = match phase {
        WorkflowPhase::Test => {
            crate::commands::workflow::verification::run_test(
                &crate::commands::workflow::verification::TestArgs {
                    command: crate::commands::workflow::verification::TestCommand::Changed(
                        run_args,
                    ),
                },
                writer,
            )?;
            false
        }
        WorkflowPhase::Verify => {
            crate::commands::workflow::verification::run_verify(
                &crate::commands::workflow::verification::VerifyArgs {
                    run: run_args,
                    builtin: false,
                },
                writer,
            )?;
            true
        }
        _ => unreachable!("non-Test/Verify phases returned above"),
    };
    let after = crate::commands::workflow::verification::latest_report_id(state_dir, repo)?;
    // run_and_persist turns a persist failure into a warning, so an unchanged report id means no fresh report exists; never gate on the stale one. (#287)
    if after == before {
        writeln!(
            writer,
            "checks ran but no fresh report was persisted; step '{step_id}' was not advanced"
        )?;
        return Ok(crate::commands::workflow::verification::GateOutcome::Fail);
    }
    // Gate on the baseline-aware freshness rule, not run_test/run_verify's raw exit code: baselined failures exit non-zero yet satisfy the gate. (#215)
    if crate::commands::workflow::verification::latest_is_fresh_and_passing(
        state_dir,
        repo,
        final_only,
        Some(branch),
    )? {
        Ok(crate::commands::workflow::verification::GateOutcome::Pass)
    } else {
        Ok(crate::commands::workflow::verification::GateOutcome::Fail)
    }
}

/// Measure each attempt since the step became current or the previous attempt reset its phase clock. (#699)
pub(super) fn phase_elapsed_ms(state: &WorkflowState) -> u64 {
    now_secs()
        .saturating_sub(state.phase_started_at)
        .saturating_mul(1000)
}

/// Record duration before completing the step so telemetry reuses the same elapsed measurement. (#699)
pub(super) fn record_step_duration_ms(state: &mut WorkflowState, step_id: &str) -> u64 {
    let elapsed_ms = phase_elapsed_ms(state);
    state
        .step_durations_ms
        .insert(step_id.to_string(), elapsed_ms);
    elapsed_ms
}

/// `<minutes>m<seconds>s`, e.g. `2m10s`.
pub(super) fn format_wall_clock(ms: u64) -> String {
    let total_secs = ms / 1000;
    format!("{}m{}s", total_secs / 60, total_secs % 60)
}

/// A bounded worker to auto-spawn after a gate transition.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct AutoSpawn {
    pub phase: WorkflowPhase,
    pub argv: Vec<String>,
    /// Review is read-only; Test/Verify need writes for build artifacts and caches.
    pub mode: crate::commands::ctx::permit::WorkerMode,
    /// Task class for a future delegated auto-spawn; this subprocess path does not consume it. (#264)
    pub task_class: crate::commands::ctx::log::TaskClass,
}

/// Stay quiet when auto-spawn is disabled or ineligible; report missing permits or agents when the operator enabled it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AutoSpawnSkip {
    Quiet,
    NoPermit,
    NoAgent,
}

/// Pure: whether a gate transition should auto-spawn a worker, the argv for
/// it, or the reason it did not. `default_agent` is the operator's
/// configured chat harness (`adapters::resolve_default`, the same one `zirv
/// ctx chat` launches by default), consulted only when the workflow itself
/// has no recorded adapter -- `state.adapter` always wins when set.
pub(crate) fn auto_spawn_decision(
    state: &WorkflowState,
    enabled: bool,
    permit_available: bool,
    default_agent: Option<&str>,
) -> Result<AutoSpawn, AutoSpawnSkip> {
    if !enabled || state.status != WorkflowStatus::Running {
        return Err(AutoSpawnSkip::Quiet);
    }
    let phase = state.current().ok_or(AutoSpawnSkip::Quiet)?.phase;
    if !matches!(
        phase,
        WorkflowPhase::Review | WorkflowPhase::Test | WorkflowPhase::Verify
    ) {
        return Err(AutoSpawnSkip::Quiet);
    }
    if !permit_available {
        return Err(AutoSpawnSkip::NoPermit);
    }
    let repo = state.repo.display().to_string();
    let argv = match phase {
        WorkflowPhase::Review => {
            let agent = state
                .adapter
                .clone()
                .or_else(|| default_agent.map(str::to_string))
                .ok_or(AutoSpawnSkip::NoAgent)?;
            vec![
                "workflow".to_string(),
                "review".to_string(),
                "run".to_string(),
                state.id.clone(),
                "--agent".to_string(),
                agent,
                "--repo".to_string(),
                repo,
            ]
        }
        WorkflowPhase::Test => vec![
            "test".to_string(),
            "changed".to_string(),
            "--repo".to_string(),
            repo,
        ],
        WorkflowPhase::Verify => vec!["verify".to_string(), "--repo".to_string(), repo],
        _ => unreachable!("filtered above"),
    };
    Ok(AutoSpawn {
        phase,
        argv,
        mode: if phase == WorkflowPhase::Review {
            crate::commands::ctx::permit::WorkerMode::ReadOnly
        } else {
            crate::commands::ctx::permit::WorkerMode::Writing
        },
        // Review has its own task class; Test and Verify share `TaskClass::Test`. (#264)
        task_class: match phase {
            WorkflowPhase::Review => crate::commands::ctx::log::TaskClass::Review,
            WorkflowPhase::Test | WorkflowPhase::Verify => {
                crate::commands::ctx::log::TaskClass::Test
            }
            _ => unreachable!("filtered above"),
        },
    })
}

#[cfg(unix)]
pub(crate) fn detach(command: &mut std::process::Command) {
    use std::os::unix::process::CommandExt;
    command.process_group(0);
}

#[cfg(windows)]
pub(crate) fn detach(command: &mut std::process::Command) {
    use std::os::windows::process::CommandExt;
    const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
    const DETACHED_PROCESS: u32 = 0x0000_0008;
    command.creation_flags(CREATE_NEW_PROCESS_GROUP | DETACHED_PROCESS);
}

/// The operator explicitly enabled `auto_spawn_on_gate`, so a skip that
/// reaches this point (as opposed to `AutoSpawnSkip::Quiet`) is reported,
/// not silent.
pub(super) fn announce_auto_spawn_skip(
    cfg: &crate::commands::ctx::config::CtxConfig,
    phase: WorkflowPhase,
    reason: &str,
) {
    crate::commands::ctx::announce::Announcer::new(cfg.chrome.events, false).emit(
        &crate::commands::ctx::announce::Event::AutoSpawnSkipped {
            phase: phase.to_string(),
            reason: reason.to_string(),
        },
    );
}

/// Detach the child without failing advance; retain its heavy permit until the live-record sweep observes child exit. (#242)
pub(super) fn spawn_auto_worker(
    state_dir: &StateDir,
    state: &WorkflowState,
    cfg: &crate::commands::ctx::config::CtxConfig,
    spawn: AutoSpawn,
) {
    use crate::commands::ctx::permit;

    // A race against `try_auto_spawn`'s own peek: the peek said a slot was
    // free, but another caller took it before this real acquire ran.
    let Some(permit) = permit::acquire(
        state_dir,
        cfg.supervise.max_heavy_operations,
        &format!("auto-spawn: {}", spawn.argv.join(" ")),
    ) else {
        announce_auto_spawn_skip(cfg, spawn.phase, "no heavy-operation permit was free");
        return;
    };
    let Ok(exe) = std::env::current_exe() else {
        announce_auto_spawn_skip(
            cfg,
            spawn.phase,
            "could not resolve the zirv executable path",
        );
        return;
    };
    let mut command = std::process::Command::new(exe);
    command
        .args(&spawn.argv)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    detach(&mut command);
    let Ok(child) = command.spawn() else {
        announce_auto_spawn_skip(cfg, spawn.phase, "failed to spawn the worker process");
        return;
    };
    permit.set_child_pid(child.id());
    std::mem::forget(permit);

    let mut event = crate::commands::workflow::telemetry::TelemetryEvent::new(
        crate::commands::workflow::telemetry::TelemetryKind::AgentDispatched,
    );
    event.workflow_id = Some(state.id.clone());
    event.phase = Some(spawn.phase);
    event.intent = Some(state.classification.intent);
    event.complexity = Some(state.classification.complexity);
    event.risk = Some(state.classification.risk);
    event.work_domain = Some(state.classification.work_domain.domain);
    event.agent_id = Some(format!("auto-spawn:{}", spawn.phase));
    let _ = crate::commands::workflow::telemetry::record(
        state_dir,
        &state.repo,
        &event,
        &crate::commands::workflow::telemetry::TelemetryConfig::for_repo(&state.repo),
    );

    crate::commands::ctx::announce::Announcer::new(cfg.chrome.events, false).emit(
        &crate::commands::ctx::announce::Event::AutoSpawned {
            phase: spawn.phase.to_string(),
            command: spawn.argv.join(" "),
        },
    );
}

/// Thin I/O wrapper around [`auto_spawn_decision`]: resolves config, a
/// permit peek, and (only when the workflow itself has no adapter) the
/// operator's default chat harness, then hands off to [`spawn_auto_worker`].
/// Any failure along the way (config, permit, spawn) is silently degraded --
/// never propagated to `advance`'s own result -- but a skip the operator's
/// own `auto_spawn_on_gate = true` made eligible is announced, not silent.
pub(super) fn try_auto_spawn(state_dir: &StateDir, state: &WorkflowState) {
    let cfg = match crate::commands::ctx::config::CtxConfig::load(&state.repo, &|key| {
        std::env::var(key).ok()
    }) {
        Ok(cfg) => cfg,
        Err(_) => return,
    };
    if !cfg.workflow.auto_spawn_on_gate {
        return;
    }
    let permit_available =
        crate::commands::ctx::permit::live_count(state_dir) < cfg.supervise.max_heavy_operations;
    // `state.adapter` always wins; the operator's configured chat harness
    // (the same one `adapters::resolve_default` picks for `zirv ctx chat`)
    // is only worth resolving -- readiness probes and all -- when the
    // workflow itself named none.
    let default_agent = state
        .adapter
        .is_none()
        .then(|| {
            crate::commands::ctx::adapters::resolve_default(&cfg)
                .ok()
                .map(|(adapter, _)| adapter.name().to_string())
        })
        .flatten();
    match auto_spawn_decision(state, true, permit_available, default_agent.as_deref()) {
        Ok(spawn) => spawn_auto_worker(state_dir, state, &cfg, spawn),
        Err(skip) => {
            if let Some(reason) = auto_spawn_skip_reason(skip)
                && let Some(phase) = state.current().map(|step| step.phase)
            {
                announce_auto_spawn_skip(&cfg, phase, reason);
            }
        }
    }
}

/// The advisory text for a skip the operator's own `auto_spawn_on_gate =
/// true` made eligible, or `None` for `Quiet` -- the ordinary, never-
/// announced case (disabled, wrong phase, `AwaitingApproval`).
pub(super) fn auto_spawn_skip_reason(skip: AutoSpawnSkip) -> Option<&'static str> {
    match skip {
        AutoSpawnSkip::Quiet => None,
        AutoSpawnSkip::NoPermit => Some("no heavy-operation permit was free"),
        AutoSpawnSkip::NoAgent => Some(
            "no adapter to run the reviewer as (the workflow has none, and no operator \
             default chat harness could be resolved)",
        ),
    }
}

pub(crate) fn write_state(
    writer: &mut impl Write,
    state: &WorkflowState,
    json: bool,
) -> CtxResult<()> {
    if json {
        serde_json::to_writer_pretty(&mut *writer, state)?;
        writeln!(writer)?;
    } else {
        writeln!(writer, "workflow: {}", state.id)?;
        writeln!(writer, "kind: {}", state.kind.as_str())?;
        // Persisted selection explains the chosen pack in status. (#542)
        if let Some(selection) = &state.selection {
            writeln!(
                writer,
                "selected: {} ({})",
                selection.definition_id,
                selection.reasons.join("; ")
            )?;
        }
        writeln!(
            writer,
            "profile: {:?} ({})",
            state.profile,
            match state.profile_source {
                ProfileSource::Classified => "classified",
                ProfileSource::OperatorOverride => "operator override",
            }
        )?;
        if let Some(frontend_root) = &state.frontend_target_root {
            writeln!(writer, "frontend target root: {}", frontend_root.display())?;
        }
        if let Some(accepted) = &state.accepted_preexisting_findings {
            writeln!(
                writer,
                "accepted pre-existing frontend findings: {} blocking / {} total at {} ({})",
                accepted.blocking, accepted.total, accepted.step, accepted.at
            )?;
        }
        writeln!(writer, "deploy tier: {}", state.deploy_tier)?;
        if !state.jev_tags.is_empty() {
            writeln!(writer, "jev tags: {}", state.jev_tags.join(", "))?;
        }
        writeln!(writer, "status: {:?}", state.status)?;
        if let Some(reason) = &state.closed_reason {
            writeln!(writer, "closed reason: {reason}")?;
        }
        writeln!(
            writer,
            "classification: {:?}/{:?} risk={} ({:?})",
            state.classification.intent,
            state.classification.complexity,
            state.classification.risk_score,
            state.classification.risk
        )?;
        if let classify::RiskMeasurement::Unavailable { reason } =
            &state.classification.risk_measurement
        {
            writeln!(writer, "risk measurement: unavailable ({reason})")?;
        }
        // Show gate-time classification reasons so escalated risk is visible in text status. (#685)
        for reason in &state.classification.reasons {
            writeln!(writer, "- {reason}")?;
        }
        // The intent-step flag matters only when this workflow has an intent step. (#236)
        if state
            .steps
            .iter()
            .any(|step| step.phase == WorkflowPhase::Intent)
        {
            writeln!(
                writer,
                "brainstorm: {}",
                if state.brainstorm { "on" } else { "off" }
            )?;
        }
        if let Some(step) = state.current() {
            writeln!(
                writer,
                "current: {} ({}, skill {}, agent {}{})",
                step.id,
                step.phase,
                step.skill,
                step.agent.as_deref().unwrap_or("-"),
                step.artifact
                    .map(|stage| format!(", artifact {stage}"))
                    .unwrap_or_default()
            )?;
        } else {
            writeln!(writer, "current: none")?;
        }
        let completed_rendered = state
            .completed_steps
            .iter()
            .map(|id| match state.step_durations_ms.get(id) {
                Some(&ms) => format!("{id} ({})", format_wall_clock(ms)),
                None => id.clone(),
            })
            .collect::<Vec<_>>()
            .join(", ");
        writeln!(writer, "completed: {completed_rendered}")?;
    }
    Ok(())
}

/// The result of [`start_workflow`]: the newly persisted [`WorkflowState`],
/// the [`crate::commands::workflow::selection::Selection`] that chose it (`None` when the
/// caller gave an explicit id), and whether `.zirv/work` is gitignored in
/// the target repository (a caller-formatted warning, not printed here).
pub struct StartOutcome {
    pub state: WorkflowState,
    pub selection: Option<crate::commands::workflow::selection::Selection>,
    pub work_dir_gitignored: bool,
}

/// The one-line note [`start_workflow`] prints to STDERR when starting a new
/// workflow silently changes which workflow this repository's active
/// pointer names. Multiple workflows per repository are legitimate
/// (`zirv workflow resume` restores any of them), so `start_workflow` never
/// refuses the start; it only says what moved and how to get it back. A
/// pure fn so its exact wording is unit-testable without capturing real
/// process stderr.
pub(super) fn active_workflow_displaced_note(
    old_instance_id: &str,
    old_definition_id: &str,
) -> String {
    format!(
        "note: workflow {old_instance_id} ({old_definition_id}) is no longer this repository's \
         active workflow; restore it with: zirv workflow resume {old_instance_id}"
    )
}

/// A status-note write failure must not turn an already-saved workflow start into a failure.
pub(super) fn best_effort_write_displacement_note(mut writer: impl std::io::Write, note: &str) {
    let _ = writeln!(writer, "{note}");
}

/// Starts and persists a workflow from `args` -- the SAME logic `zirv
/// workflow start` and the native `workflow_start` tool both run, so
/// "what starting a workflow means" has exactly one implementation (issue
/// #542 chunk 3b), matching this crate's own "a second implementation of
/// what advances a step is a second definition of done" rule for the
/// native workflow tools. Pure of `writer`/output formatting: the caller
/// decides how to render [`StartOutcome`] (CLI text/JSON, or a tool's JSON
/// result) -- except for one STDERR note when the start displaces a
/// different, still-running workflow as this repository's active one (see
/// [`active_workflow_displaced_note`]); stdout/`--json` output is unaffected
/// either way.
pub fn start_workflow(state_dir: &StateDir, args: &StartArgs) -> CtxResult<StartOutcome> {
    let repo = resolve_repo(args.repo.as_deref())?;
    // Resolve any registry id through the same start path; use the standard unknown-workflow wording. (#542)
    let registry = load_workflow_registry(&repo, args.built_in_only)?;
    report_registry_warnings(&registry);
    let inherited_agent = session_identity().map(|(_, adapter)| adapter);
    let selected_agent = args.agent.clone().or(inherited_agent);

    // Registry ids are validated lowercase at load (`definition::valid_id`),
    // so an explicit id is matched case-insensitively by lowercasing it
    // here once, before it feeds `WorkflowKind::from_pack_id` or
    // `registry.get` -- `zirv workflow start Bugfix` must resolve exactly
    // like `zirv workflow start bugfix`.
    let requested_id = args.id.as_deref().map(str::to_ascii_lowercase);

    // An explicit id wins; otherwise classify then select from the task objective. (#542)
    let explicit_kind_hint = requested_id.as_deref().and_then(WorkflowKind::from_pack_id);
    let classify_args = classify::ClassifyArgs {
        task: args.task.clone(),
        paths: args.paths.clone(),
        changed_lines: args.changed_lines,
        tests_changed: args.tests_changed,
        intent: explicit_kind_hint.map(|kind| kind.intent()),
        complexity: args.complexity,
        risk: args.risk,
        repo: Some(repo.clone()),
        branch: args.branch.clone(),
        json: false,
    };
    let mut classification = classify::from_start_args(&classify_args)?;
    // Refine intent before pack selection when enabled; start has no profile domain-tag surface. (#782)
    crate::commands::workflow::profile::refine_intent_via_jev(
        &repo,
        &args.task,
        &mut classification,
    );
    let selection = if requested_id.is_none() {
        Some(crate::commands::workflow::selection::select_definition(
            &classification,
            &registry,
            &args.task,
        ))
    } else {
        None
    };
    let resolved_id = match &requested_id {
        Some(id) => id.clone(),
        None => selection
            .as_ref()
            .expect("selection ran when id is None")
            .definition_id
            .clone(),
    };
    let pack = registry.get(&resolved_id)?;
    let kind_hint = WorkflowKind::from_pack_id(&pack.definition.id);
    if classification.work_domain.domain == WorkDomain::Frontend {
        // Eager zero-touch bootstrap. Prompt rendering refreshes this
        // derived profile as repository evidence evolves.
        crate::commands::workflow::frontend::ensure_profile(state_dir, &repo)?;
    }
    let profile = WorkflowProfile::for_classification(&classification);
    let deploy_tier = crate::commands::workflow::deploy::effective_tier(&repo)?;
    let brainstorm = args
        .brainstorm_override()
        .unwrap_or_else(|| kind_hint.map(default_brainstorm_for_kind).unwrap_or(false));
    let materialized = materialize_from_definition(
        &pack.definition,
        &classification,
        profile,
        deploy_tier,
        brainstorm,
    );
    if let Some(agent) = &selected_agent {
        let skills =
            SkillRegistry::load_for_repo(&repo, dirs::home_dir().as_deref(), !args.built_in_only)?;
        let report =
            crate::commands::workflow::capability::CapabilityReport::for_repo(agent, &repo)?;
        for step in &materialized {
            for skill in step_skill_ids(step, &classification) {
                skills.ensure_supported(&skill, &report)?;
            }
            // Refuse unavailable required integrations before entering a step, naming what is missing. (#483)
            let frontend = classification.work_domain.domain == WorkDomain::Frontend;
            report
                .admit(
                    &crate::commands::workflow::capability::required_integrations(
                        step.phase, frontend,
                    ),
                )
                .map_err(|why| format!("step '{}': {why}", step.id))?;
        }
    }
    // Resolve every agent role before writing state, even when no execution adapter is configured. (#542)
    let agent_registry =
        AgentRegistry::load_for_repo(&repo, dirs::home_dir().as_deref(), !args.built_in_only)?;
    for step in &materialized {
        if let Some(role) = step.agent.as_deref() {
            agent_registry
                .get(role)
                .map_err(|_| format!("step '{}': unknown agent role '{role}'", step.id))?;
        }
    }
    // Read the CURRENT active pointer before it gets overwritten below, so
    // a start that silently displaces a still-running workflow can be
    // reported after the fact -- multiple workflows per repository are
    // legitimate (`zirv workflow resume` restores any of them), so this
    // never refuses the start itself.
    let previously_active = load_active(state_dir, &repo).ok().flatten();
    let mut state = WorkflowState::start_from_pack(
        repo,
        args.task.clone(),
        pack,
        selected_agent,
        !args.built_in_only,
        classification,
    );
    state.selection = selection.clone();
    state.branch = args
        .branch
        .clone()
        .unwrap_or_else(|| crate::commands::workflow::verification::current_branch(&state.repo));
    if brainstorm != state.brainstorm {
        state.brainstorm = brainstorm;
        apply_brainstorm_selection(
            brainstorm,
            WorkflowKind::from_pack_id(&pack.definition.id).is_some(),
            &mut state.steps,
        );
    }
    if let Some(forced_profile) = args.profile {
        state.set_profile(forced_profile);
    }
    apply_effective_deploy_tier(&mut state, deploy_tier);
    state.usage_checkpoint = usage_checkpoint(&state.repo);
    if let Some(frontend_root) = &args.frontend_root {
        state.frontend_target_root = Some(resolve_frontend_root(frontend_root)?);
    }
    // Starting a workflow must never pre-create an unfilled artifact or mutate the worktree/index; context still exposes its path and template.
    let work_dir_gitignored = work_dir_is_gitignored(&state.repo);
    save(state_dir, &state, true)?;
    // Bind the workflow to this session for pane-specific status; absent session context is a quiet best-effort skip.
    bind_started_workflow_to_calling_session(state_dir, &state.id);
    if let Some(old) = previously_active
        && old.id != state.id
        && matches!(
            old.status,
            WorkflowStatus::Running | WorkflowStatus::AwaitingApproval
        )
    {
        let old_definition_id = old
            .definition
            .as_ref()
            .map(|definition| definition.id.clone())
            .unwrap_or_else(|| old.kind.as_str().to_string());
        best_effort_write_displacement_note(
            std::io::stderr(),
            &active_workflow_displaced_note(&old.id, &old_definition_id),
        );
    }
    let mut event = crate::commands::workflow::telemetry::TelemetryEvent::new(
        crate::commands::workflow::telemetry::TelemetryKind::WorkflowStarted,
    );
    event.workflow_id = Some(state.id.clone());
    event.intent = Some(state.classification.intent);
    event.complexity = Some(state.classification.complexity);
    event.risk = Some(state.classification.risk);
    event.work_domain = Some(state.classification.work_domain.domain);
    event.deploy_tier = Some(state.deploy_tier.to_string());
    let _ = crate::commands::workflow::telemetry::record(
        state_dir,
        &state.repo,
        &event,
        &crate::commands::workflow::telemetry::TelemetryConfig::for_repo(&state.repo),
    );
    Ok(StartOutcome {
        state,
        selection,
        work_dir_gitignored,
    })
}

pub fn run(args: &WorkflowArgs, writer: &mut impl Write) -> CtxResult<i32> {
    match &args.command {
        WorkflowSubcommand::List(args) => {
            let repo = resolve_repo(args.repo.as_deref())?;
            let registry = load_workflow_registry(&repo, args.built_in_only)?;
            report_registry_warnings(&registry);
            let entries: Vec<&crate::commands::workflow::registry::RegisteredWorkflow> =
                registry.list().collect();
            if args.json {
                serde_json::to_writer_pretty(&mut *writer, &entries)?;
                writeln!(writer)?;
            } else {
                write_registry_list(writer, &entries)?;
            }
        }
        WorkflowSubcommand::Show(args) => {
            let repo = resolve_repo(args.repo.as_deref())?;
            let registry = load_workflow_registry(&repo, args.built_in_only)?;
            report_registry_warnings(&registry);
            // Same case-insensitive id match as `workflow start` --
            // registry ids are validated lowercase at load, so `zirv
            // workflow show Bugfix` must resolve like `zirv workflow show
            // bugfix`.
            let workflow = registry.get(&args.id.to_ascii_lowercase())?;
            if args.json {
                serde_json::to_writer_pretty(&mut *writer, workflow)?;
                writeln!(writer)?;
            } else {
                write_registry_entry(writer, workflow)?;
            }
        }
        WorkflowSubcommand::Classify(args) => {
            let classification = classify::from_args(args)?;
            // Registry selection is best-effort; an unreadable registry must not break classify. (#542)
            let repo = resolve_repo(args.repo.as_deref())?;
            // Derive profile from the same classification; optional Jev refinement precedes pack selection and leaves defaults intact when unavailable. (#541, #782)
            let mut profile = crate::commands::workflow::profile::ExecutionProfile::derive(
                &args.task,
                &classification,
            );
            crate::commands::workflow::profile::refine_via_jev(&repo, &args.task, &mut profile);
            let selection = load_workflow_registry(&repo, false).ok().map(|registry| {
                crate::commands::workflow::selection::select_definition(
                    &profile.classification,
                    &registry,
                    &args.task,
                )
            });
            if args.json {
                #[derive(Serialize)]
                struct ClassifyOutput<'a> {
                    #[serde(flatten)]
                    classification: &'a Classification,
                    /// Embed the minimal execution profile from the same classification. (#541)
                    profile: &'a crate::commands::workflow::profile::ExecutionProfile,
                    /// Omit best-effort selection if the registry cannot load. (#542)
                    #[serde(skip_serializing_if = "Option::is_none")]
                    selection: Option<&'a crate::commands::workflow::selection::Selection>,
                }
                serde_json::to_writer_pretty(
                    &mut *writer,
                    &ClassifyOutput {
                        classification: &profile.classification,
                        profile: &profile,
                        selection: selection.as_ref(),
                    },
                )?;
                writeln!(writer)?;
            } else {
                writeln!(
                    writer,
                    "intent={:?} domain={:?} complexity={:?} risk={:?} score={}",
                    profile.classification.intent,
                    profile.classification.work_domain.domain,
                    profile.classification.complexity,
                    profile.classification.risk,
                    profile.classification.risk_score
                )?;
                for reason in &profile.classification.reasons {
                    writeln!(writer, "- {reason}")?;
                }
                if let Some(selection) = &selection {
                    writeln!(
                        writer,
                        "selection: {} ({})",
                        selection.definition_id,
                        selection.reasons.join("; ")
                    )?;
                }
            }
        }
        WorkflowSubcommand::Start(args) => {
            let state_dir = resolve_state()?;
            let outcome = start_workflow(&state_dir, args)?;
            write_start_outcome(writer, &outcome, args.json)?;
        }
        WorkflowSubcommand::Status(args) => {
            let repo = resolve_repo(args.repo.as_deref())?;
            let state_dir = resolve_state()?;
            let state = match &args.id {
                Some(id) => load(&state_dir, &repo, id)?,
                None => load_active(&state_dir, &repo)?.ok_or("no active workflow")?,
            };
            write_state(writer, &state, args.json)?;
            if !args.json {
                write_definition_status(writer, &state)?;
            }
        }
        WorkflowSubcommand::Resume(args) => {
            let repo = resolve_repo(args.repo.as_deref())?;
            let state_dir = resolve_state()?;
            let mut state = load(&state_dir, &repo, &args.id)?;
            // Checked against the as-loaded status, before `refresh_deploy_tier`:
            // `apply_effective_deploy_tier` unconditionally recomputes `status`
            // from the current step's position, which would otherwise silently
            // revive a terminal `Failed`/`Completed`/`Closed` workflow back to
            // `Running`/`AwaitingApproval`.
            if !matches!(
                state.status,
                WorkflowStatus::Running | WorkflowStatus::AwaitingApproval
            ) {
                return Err(format!("cannot resume workflow in {:?} state", state.status).into());
            }
            refresh_deploy_tier(&mut state)?;
            save(&state_dir, &state, true)?;
            write_state(writer, &state, false)?;
        }
        WorkflowSubcommand::Reclassify(args) => {
            let repo = resolve_repo(args.repo.as_deref())?;
            let state_dir = resolve_state()?;
            let state = reclassify(&state_dir, load(&state_dir, &repo, &args.id)?, args.profile)?;
            write_state(writer, &state, args.json)?;
        }
        WorkflowSubcommand::Context(args) => {
            let repo = resolve_repo(args.repo.as_deref())?;
            let state_dir = resolve_state()?;
            let state = match &args.id {
                Some(id) => load(&state_dir, &repo, id)?,
                None => load_active(&state_dir, &repo)?.ok_or("no active workflow")?,
            };
            match render_current_context(&state, &repo, dirs::home_dir().as_deref())? {
                Some(context) => write!(writer, "{context}")?,
                None => writeln!(writer, "workflow has no active step context")?,
            }
        }
        WorkflowSubcommand::Agents(args) => {
            return crate::commands::workflow::agents::run(args, writer);
        }
        WorkflowSubcommand::Team(args) => {
            return crate::commands::workflow::team::run(args, writer);
        }
        WorkflowSubcommand::Artifacts(args) => {
            let repo = resolve_repo(args.repo.as_deref())?;
            let state_dir = resolve_state()?;
            let state = load(&state_dir, &repo, &args.id)?;
            let statuses = workflow_artifact_statuses(&state)?;
            if args.json {
                serde_json::to_writer_pretty(&mut *writer, &statuses)?;
                writeln!(writer)?;
            } else if statuses.is_empty() {
                writeln!(writer, "workflow has no committed work-product artifacts")?;
            } else {
                writeln!(writer, "STAGE\tPATH\tSTATE")?;
                for status in statuses {
                    let state = if status.drifted {
                        "drifted"
                    } else if status.accepted {
                        "accepted"
                    } else if status.exists {
                        "pending"
                    } else {
                        "missing"
                    };
                    writeln!(
                        writer,
                        "{}\t{}\t{}{}",
                        status.stage,
                        status.rel_path,
                        state,
                        status
                            .accepted_at
                            .as_deref()
                            .map(|at| format!(" ({at})"))
                            .unwrap_or_default()
                    )?;
                }
            }
        }
        WorkflowSubcommand::Approve(args) => {
            let repo = resolve_repo(args.repo.as_deref())?;
            let state_dir = resolve_state()?;
            let state = approve(&state_dir, load(&state_dir, &repo, &args.id)?)?;
            write_state(writer, &state, false)?;
        }
        WorkflowSubcommand::Advance(args) => {
            let repo = resolve_repo(args.repo.as_deref())?;
            let state_dir = resolve_state()?;
            let mut state = load(&state_dir, &repo, &args.id)?;
            if let Some(frontend_root) = &args.frontend_root {
                state.frontend_target_root = Some(resolve_frontend_root(frontend_root)?);
                // Persisted before the gate runs: a fail-closed advance below
                // must not force the operator to pass `--frontend-root` again
                // on retry.
                let active = matches!(
                    state.status,
                    WorkflowStatus::Running | WorkflowStatus::AwaitingApproval
                );
                save(&state_dir, &state, active)?;
            }
            let outcome = if args.run_checks {
                let current = state
                    .current()
                    .cloned()
                    .ok_or("workflow has no current step")?;
                let attempts_so_far = state.attempts.get(&current.id).copied().unwrap_or(0);
                match run_required_checks(
                    &state_dir,
                    &repo,
                    current.phase,
                    &current.id,
                    attempts_so_far,
                    &state.branch,
                    writer,
                )? {
                    crate::commands::workflow::verification::GateOutcome::Pass => {
                        StepOutcome::Success
                    }
                    crate::commands::workflow::verification::GateOutcome::Unchanged {
                        since_attempt,
                        ..
                    } => {
                        writeln!(
                            writer,
                            "verification not re-run: the worktree is byte-identical to the \
                             previous failed attempt (attempt {since_attempt}/{}). Edit source, \
                             tests, or record a blocker artifact before verifying again.",
                            current.max_attempts
                        )?;
                        let evidence = enrich_transition_evidence(
                            &mut state,
                            TransitionEvidence {
                                verification_unchanged: true,
                                ..Default::default()
                            },
                        );
                        let state = advance_with_evidence(
                            &state_dir,
                            state,
                            StepOutcome::Failure,
                            Some(&evidence),
                            args.accept_preexisting_findings,
                        )?;
                        write_state(writer, &state, args.json)?;
                        return Ok(1);
                    }
                    crate::commands::workflow::verification::GateOutcome::Fail
                    | crate::commands::workflow::verification::GateOutcome::Inconclusive(_) => {
                        writeln!(
                            writer,
                            "checks failed; step '{}' was not advanced",
                            current.id
                        )?;
                        return Ok(1);
                    }
                }
            } else {
                args.outcome
                    .ok_or("--outcome is required unless --run-checks is set")?
            };
            let evidence = enrich_transition_evidence(
                &mut state,
                TransitionEvidence {
                    duration_ms: args.duration_ms,
                    adapter: args.agent.clone(),
                    model: args.model.clone(),
                    role: args.role.clone(),
                    input_tokens: args.input_tokens,
                    output_tokens: args.output_tokens,
                    token_usage_source: (args.input_tokens.is_some()
                        || args.output_tokens.is_some())
                    .then(|| "operator-reported".into()),
                    worker_count: args.workers,
                    ..Default::default()
                },
            );
            let state = advance_with_evidence(
                &state_dir,
                state,
                outcome,
                Some(&evidence),
                args.accept_preexisting_findings,
            )?;
            write_state(writer, &state, args.json)?;
        }
        WorkflowSubcommand::Close(args) => {
            let repo = resolve_repo(args.repo.as_deref())?;
            let state_dir = resolve_state()?;
            let state = close(
                &state_dir,
                load(&state_dir, &repo, &args.id)?,
                args.reason.clone(),
            )?;
            write_state(writer, &state, args.json)?;
        }
        WorkflowSubcommand::Review(args) => {
            return crate::commands::workflow::review::run(args, writer);
        }
        WorkflowSubcommand::Maintain(args) => {
            return crate::commands::workflow::maintain::run(args, writer);
        }
        WorkflowSubcommand::Stats(args) => {
            return crate::commands::workflow::telemetry::run_stats(args, writer);
        }
        WorkflowSubcommand::Calibrate(args) => {
            return crate::commands::workflow::outcomes::run_calibrate(args, writer);
        }
        WorkflowSubcommand::Spend(args) => {
            return crate::commands::ctx::attribution::run_spend(args, writer);
        }
        WorkflowSubcommand::Research(args) => {
            return crate::commands::workflow::research::run(args, writer);
        }
    }
    Ok(0)
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use std::path::{Path, PathBuf};

    use tempfile::tempdir;

    use crate::commands::ctx::state::{StateDir, repo_slug};

    use crate::commands::workflow::classify::{Complexity, Intent, RiskBand, WorkDomain};
    use crate::commands::workflow::deploy::DeployTier;
    use crate::commands::workflow::skill::WorkflowPhase;

    use super::*;

    use super::super::tests::{
        git_init_with_commit, low_classification, skip_leading_artifact_steps,
    };
    fn at_phase(mut state: WorkflowState, phase: WorkflowPhase) -> WorkflowState {
        state.current_step = state
            .steps
            .iter()
            .position(|step| step.phase == phase)
            .expect("phase present in this workflow's steps");
        state.status = WorkflowStatus::Running;
        state
    }

    fn production_feature_state(repo: &Path) -> WorkflowState {
        let mut classification = low_classification();
        classification.complexity = Complexity::Substantial;
        classification.risk = RiskBand::High;
        let mut state = WorkflowState::start(
            repo.to_path_buf(),
            "ship it".into(),
            WorkflowKind::Feature,
            Some("claude".to_string()),
            true,
            classification,
        );
        apply_effective_deploy_tier(&mut state, DeployTier::Production);
        state
    }

    #[test]
    fn auto_spawn_decision_truth_table() {
        let repo = tempdir().unwrap();
        let state = production_feature_state(repo.path());

        assert_eq!(
            auto_spawn_decision(
                &at_phase(state.clone(), WorkflowPhase::Review),
                false,
                true,
                None
            ),
            Err(AutoSpawnSkip::Quiet),
            "disabled must never fire"
        );
        assert_eq!(
            auto_spawn_decision(
                &at_phase(state.clone(), WorkflowPhase::Review),
                true,
                false,
                None
            ),
            Err(AutoSpawnSkip::NoPermit),
            "no permit must never fire"
        );
        assert_eq!(
            auto_spawn_decision(
                &at_phase(state.clone(), WorkflowPhase::Implement),
                true,
                true,
                None
            ),
            Err(AutoSpawnSkip::Quiet),
            "Implement must never fire"
        );

        let mut awaiting = at_phase(state.clone(), WorkflowPhase::Review);
        awaiting.status = WorkflowStatus::AwaitingApproval;
        assert_eq!(
            auto_spawn_decision(&awaiting, true, true, None),
            Err(AutoSpawnSkip::Quiet),
            "AwaitingApproval must never fire"
        );

        let review = auto_spawn_decision(
            &at_phase(state.clone(), WorkflowPhase::Review),
            true,
            true,
            None,
        )
        .expect("Review with a workflow adapter fires");
        assert_eq!(review.phase, WorkflowPhase::Review);
        assert_eq!(
            review.mode,
            crate::commands::ctx::permit::WorkerMode::ReadOnly,
            "issue #267: a review spawn is read-only"
        );
        assert_eq!(
            review.task_class,
            crate::commands::ctx::log::TaskClass::Review,
            "issue #264: a review-phase auto-spawn is classified as review"
        );
        assert_eq!(
            review.argv,
            vec![
                "workflow",
                "review",
                "run",
                &state.id,
                "--agent",
                "claude",
                "--repo",
                &state.repo.display().to_string(),
            ]
        );

        // `state.adapter` always wins over a configured default, even when
        // both resolve.
        let with_both = auto_spawn_decision(
            &at_phase(state.clone(), WorkflowPhase::Review),
            true,
            true,
            Some("codex"),
        )
        .expect("Review fires");
        assert_eq!(
            with_both.argv[5], "claude",
            "the workflow's own adapter wins"
        );

        let mut no_adapter = at_phase(state.clone(), WorkflowPhase::Review);
        no_adapter.adapter = None;
        assert_eq!(
            auto_spawn_decision(&no_adapter, true, true, None),
            Err(AutoSpawnSkip::NoAgent),
            "Review with no workflow adapter and no default must not fire"
        );

        let with_default = auto_spawn_decision(&no_adapter, true, true, Some("codex"))
            .expect("Review with no workflow adapter falls back to the operator default");
        assert_eq!(with_default.argv[5], "codex");

        let test = auto_spawn_decision(
            &at_phase(state.clone(), WorkflowPhase::Test),
            true,
            true,
            None,
        )
        .expect("Test fires");
        assert_eq!(test.phase, WorkflowPhase::Test);
        assert_eq!(
            test.mode,
            crate::commands::ctx::permit::WorkerMode::Writing,
            "issue #371: a test spawn needs to write build artifacts and caches"
        );
        assert_eq!(
            test.task_class,
            crate::commands::ctx::log::TaskClass::Test,
            "issue #264: a test-phase auto-spawn is classified as test"
        );
        assert_eq!(
            test.argv,
            vec![
                "test",
                "changed",
                "--repo",
                &state.repo.display().to_string()
            ]
        );

        let verify = auto_spawn_decision(
            &at_phase(state.clone(), WorkflowPhase::Verify),
            true,
            true,
            None,
        )
        .expect("Verify fires");
        assert_eq!(verify.phase, WorkflowPhase::Verify);
        assert_eq!(
            verify.mode,
            crate::commands::ctx::permit::WorkerMode::Writing,
            "issue #371: a verify spawn needs to write build artifacts and caches"
        );
        assert_eq!(
            verify.task_class,
            crate::commands::ctx::log::TaskClass::Test,
            "issue #264: a verify-phase auto-spawn is classified as test"
        );
        assert_eq!(
            verify.argv,
            vec!["verify", "--repo", &state.repo.display().to_string()]
        );
    }

    /// `Quiet` is the only skip that stays silent -- the config-disabled
    /// path, or a phase auto-spawn was never meant to touch. `NoPermit`/
    /// `NoAgent` are eligible-but-skipped, so an operator who turned the
    /// key on must see why.
    #[test]
    fn auto_spawn_skip_reason_is_silent_only_for_quiet() {
        assert_eq!(auto_spawn_skip_reason(AutoSpawnSkip::Quiet), None);
        assert!(auto_spawn_skip_reason(AutoSpawnSkip::NoPermit).is_some());
        assert!(auto_spawn_skip_reason(AutoSpawnSkip::NoAgent).is_some());
    }

    #[test]
    fn brainstorm_flags_are_mutually_exclusive_and_resolve_to_an_override() {
        use clap::Parser;
        #[derive(clap::Parser)]
        struct Cli {
            #[command(flatten)]
            args: StartArgs,
        }
        let plain =
            Cli::try_parse_from(["zirv", "feature", "--task", "x"]).expect("no flags parses");
        assert_eq!(plain.args.brainstorm_override(), None);

        let on = Cli::try_parse_from(["zirv", "feature", "--task", "x", "--brainstorm"])
            .expect("--brainstorm parses");
        assert_eq!(on.args.brainstorm_override(), Some(true));

        let off = Cli::try_parse_from(["zirv", "feature", "--task", "x", "--no-brainstorm"])
            .expect("--no-brainstorm parses");
        assert_eq!(off.args.brainstorm_override(), Some(false));

        assert!(
            Cli::try_parse_from([
                "zirv",
                "feature",
                "--task",
                "x",
                "--brainstorm",
                "--no-brainstorm",
            ])
            .is_err(),
            "both flags together must be refused"
        );
    }

    /// #260-adjacent: `zirv workflow advance --run-checks` collapses "run
    /// the gate, then advance" into one call -- a passing check must both
    /// print the evidence summary and advance past the `Test` step.
    #[test]
    fn advance_run_checks_runs_the_test_gate_and_advances_on_success() {
        let repo = tempdir().unwrap();
        let root = tempdir().unwrap();
        let state_dir = StateDir::from_root(root.path().to_path_buf());

        let git = |args: &[&str]| {
            let status = std::process::Command::new("git")
                .args([
                    "-c",
                    "user.email=t@example.com",
                    "-c",
                    "user.name=t",
                    "-c",
                    "commit.gpgsign=false",
                ])
                .args(args)
                .current_dir(repo.path())
                .status()
                .expect("run git");
            assert!(status.success(), "git {args:?} failed");
        };
        git(&["init", "-q"]);
        std::fs::write(repo.path().join("README.md"), "readme\n").unwrap();
        git(&["add", "."]);
        git(&["commit", "-q", "-m", "base"]);

        std::fs::create_dir_all(repo.path().join(".zirv")).unwrap();
        let passing = if cfg!(windows) { "exit /b 0" } else { "exit 0" };
        std::fs::write(
            repo.path().join(".zirv/verify.toml"),
            format!("schema_version=1\n[[checks]]\nid='unit'\nkind='unit'\ncommand='{passing}'\n"),
        )
        .unwrap();

        let mut state = WorkflowState::start(
            repo.path().to_path_buf(),
            "small feature".into(),
            WorkflowKind::Feature,
            None,
            true,
            low_classification(),
        );
        let test_index = state
            .steps
            .iter()
            .position(|step| step.phase == WorkflowPhase::Test)
            .unwrap();
        state.completed_steps = state.steps[..test_index]
            .iter()
            .map(|step| step.id.clone())
            .collect();
        state.current_step = test_index;
        state.status = WorkflowStatus::Running;
        let id = state.id.clone();
        save(&state_dir, &state, true).unwrap();

        let _state_dir_env = crate::commands::ctx::testenv::VarGuard::set(&[(
            "ZIRV_CTX_STATE_DIR",
            Some(root.path().to_str().expect("utf-8 tempdir path")),
        )]);
        let args = WorkflowArgs {
            command: WorkflowSubcommand::Advance(AdvanceArgs {
                id: id.clone(),
                outcome: None,
                run_checks: true,
                repo: Some(repo.path().to_path_buf()),
                json: false,
                duration_ms: None,
                agent: None,
                model: None,
                role: None,
                input_tokens: None,
                output_tokens: None,
                workers: 0,
                frontend_root: None,
                accept_preexisting_findings: false,
            }),
        };
        let mut out = Vec::new();
        let code = run(&args, &mut out).unwrap();
        assert_eq!(code, 0, "a passing check must advance");
        let text = String::from_utf8(out).unwrap();
        assert!(
            text.contains("verification"),
            "expected the evidence summary to be printed, got {text}"
        );

        let reloaded = load(&state_dir, repo.path(), &id).unwrap();
        assert_eq!(
            reloaded.current().unwrap().phase,
            WorkflowPhase::Verify,
            "the test step must have advanced"
        );
    }

    /// Issue #610 scenario 3 (roadmap N05/N14/N15, review of #493): a real
    /// multi-file change, driven through BOTH gates a Feature workflow has
    /// -- Test (a real `--run-checks` execution) and Review (a real
    /// unresolved finding, blocking, then resolved) and Verify (a second
    /// real `--run-checks` execution) -- rather than exercising either gate
    /// in isolation the way the surrounding tests in this module do. Every
    /// step is the REAL production entry point (`run(&args, ...)`,
    /// `advance_with_evidence`), never a stand-in for what the gate would
    /// decide.
    #[test]
    fn a_real_multi_file_change_advances_only_once_test_review_and_verify_each_genuinely_pass() {
        use crate::commands::workflow::review::{
            FindingDisposition, FindingSeverity, ReviewFinding,
        };

        let repo = tempdir().unwrap();
        let root = tempdir().unwrap();
        let state_dir = StateDir::from_root(root.path().to_path_buf());

        let git = |args: &[&str]| {
            let status = std::process::Command::new("git")
                .args([
                    "-c",
                    "user.email=t@example.com",
                    "-c",
                    "user.name=t",
                    "-c",
                    "commit.gpgsign=false",
                ])
                .args(args)
                .current_dir(repo.path())
                .status()
                .expect("run git");
            assert!(status.success(), "git {args:?} failed");
        };
        git(&["init", "-q"]);
        std::fs::write(repo.path().join("a.rs"), "fn a() {}\n").unwrap();
        std::fs::write(repo.path().join("b.rs"), "fn b() {}\n").unwrap();
        git(&["add", "."]);
        git(&["commit", "-q", "-m", "base"]);
        // The real multi-file change this workflow is actually about.
        std::fs::write(repo.path().join("a.rs"), "fn a() { println!(\"a\"); }\n").unwrap();
        std::fs::write(repo.path().join("b.rs"), "fn b() { println!(\"b\"); }\n").unwrap();
        git(&["add", "."]);
        git(&["commit", "-q", "-m", "touch two files"]);

        std::fs::create_dir_all(repo.path().join(".zirv")).unwrap();
        let passing = if cfg!(windows) { "exit /b 0" } else { "exit 0" };
        let failing = if cfg!(windows) { "exit /b 1" } else { "exit 1" };
        let write_check = |command: &str| {
            std::fs::write(
                repo.path().join(".zirv/verify.toml"),
                format!(
                    "schema_version=1\n[[checks]]\nid='unit'\nkind='unit'\ncommand='{command}'\n"
                ),
            )
            .unwrap();
        };
        write_check(passing);

        let mut classification = low_classification();
        classification.risk = RiskBand::Medium;
        let mut state = WorkflowState::start(
            repo.path().to_path_buf(),
            "touch two files".into(),
            WorkflowKind::Feature,
            None,
            true,
            classification,
        );
        let review_index = state
            .steps
            .iter()
            .position(|step| step.phase == WorkflowPhase::Review)
            .expect("Medium risk must materialize a review step");
        let test_index = state
            .steps
            .iter()
            .position(|step| step.phase == WorkflowPhase::Test)
            .unwrap();
        let verify_index = state
            .steps
            .iter()
            .position(|step| step.phase == WorkflowPhase::Verify)
            .unwrap();
        assert!(
            test_index < review_index && review_index < verify_index,
            "test, then review, then verify: {:?}",
            state.steps.iter().map(|s| s.phase).collect::<Vec<_>>()
        );
        state.completed_steps = state.steps[..test_index]
            .iter()
            .map(|step| step.id.clone())
            .collect();
        state.current_step = test_index;
        state.status = WorkflowStatus::Running;
        let id = state.id.clone();
        save(&state_dir, &state, true).unwrap();

        let _state_dir_env = crate::commands::ctx::testenv::VarGuard::set(&[(
            "ZIRV_CTX_STATE_DIR",
            Some(root.path().to_str().expect("utf-8 tempdir path")),
        )]);
        let advance_args = || WorkflowArgs {
            command: WorkflowSubcommand::Advance(AdvanceArgs {
                id: id.clone(),
                outcome: None,
                run_checks: true,
                repo: Some(repo.path().to_path_buf()),
                json: false,
                duration_ms: None,
                agent: None,
                model: None,
                role: None,
                input_tokens: None,
                output_tokens: None,
                workers: 0,
                frontend_root: None,
                accept_preexisting_findings: false,
            }),
        };

        // Gate 1 (Test): a real passing check over the real two-file diff.
        let mut out = Vec::new();
        let code = run(&advance_args(), &mut out).unwrap();
        assert_eq!(code, 0, "a passing test check must advance past Test");
        let after_test = load(&state_dir, repo.path(), &id).unwrap();
        assert_eq!(
            after_test.current().unwrap().phase,
            WorkflowPhase::Implement
        );
        assert_eq!(after_test.current().unwrap().id, "simplify");

        // The `simplify` step paired with this review round: `Implement`
        // phase, no run-checks gate of its own -- a plain successful advance
        // moves it straight into Review.
        let after_simplify =
            advance_with_evidence(&state_dir, after_test, StepOutcome::Success, None, false)
                .expect("the simplify step must advance");
        assert_eq!(
            after_simplify.current().unwrap().phase,
            WorkflowPhase::Review
        );
        save(&state_dir, &after_simplify, true).unwrap();

        // Gate 2 (Review): a real, unresolved finding blocks -- the same
        // gate `a_finding_recorded_while_the_reviewer_ran_survives_the_
        // evidence_write` proves records for real; this proves what the
        // engine does with it.
        let mut with_finding = after_simplify;
        with_finding.review_findings.push(ReviewFinding {
            id: "finding-1".into(),
            severity: FindingSeverity::Major,
            summary: "both files need a second look".into(),
            path: Some("a.rs".into()),
            line: None,
            disposition: FindingDisposition::Open,
            recommended_disposition: None,
            advisory_disposition: None,
            advisory_confidence: None,
            duplicate_of: None,
            created_at: 0,
        });
        save(&state_dir, &with_finding, true).unwrap();
        let blocked = advance_with_evidence(
            &state_dir,
            with_finding.clone(),
            StepOutcome::Success,
            None,
            false,
        )
        .unwrap_err()
        .to_string();
        assert!(
            blocked.contains("final disposition"),
            "an open finding must block review: {blocked}"
        );

        // Resolved for real: the review gate now passes, into Verify. Also
        // needs one fresh independent review run recorded against the
        // CURRENT diff's own fingerprint -- the same freshness check
        // `fix_review_rounds_advance_only_for_a_changed_fingerprint` pins,
        // computed here with the real production function rather than a
        // guessed value.
        let mut resolved = with_finding;
        resolved.review_findings[0].disposition = FindingDisposition::Fixed;
        let fingerprint =
            crate::commands::workflow::verification::change_fingerprint(&resolved.repo).unwrap();
        resolved
            .review_evidence
            .push(crate::commands::workflow::review::ReviewRunEvidence {
                id: "review-1".into(),
                change_fingerprint: fingerprint,
                adapter: "claude".into(),
                review_round: 1,
                completed_at: 0,
                head_sha: None,
                reviewed_tree_sha: None,
                finding_dispositions: std::collections::BTreeMap::new(),
                jev_dedup_converged_for: None,
            });
        let after_review =
            advance_with_evidence(&state_dir, resolved, StepOutcome::Success, None, false)
                .expect("a resolved finding must let review pass");
        assert_eq!(after_review.current().unwrap().phase, WorkflowPhase::Verify);
        save(&state_dir, &after_review, true).unwrap();

        // Gate 3 (Verify): a real failing check refuses this same diff...
        write_check(failing);
        let mut out = Vec::new();
        let code = run(&advance_args(), &mut out).unwrap();
        assert_eq!(code, 1, "a failing verify check must not advance");
        let still_verify = load(&state_dir, repo.path(), &id).unwrap();
        assert_eq!(
            still_verify.current().unwrap().phase,
            WorkflowPhase::Verify,
            "a failing check must not advance the workflow"
        );

        // ...and a real passing check over the SAME multi-file diff finally
        // clears it.
        write_check(passing);
        let mut out = Vec::new();
        let code = run(&advance_args(), &mut out).unwrap();
        assert_eq!(code, 0, "a passing verify check must advance past Verify");
        let final_state = load(&state_dir, repo.path(), &id).unwrap();
        assert_ne!(
            final_state.current().map(|step| step.phase),
            Some(WorkflowPhase::Verify),
            "the workflow must have moved past verify: {:?}",
            final_state.current()
        );
    }

    /// The mirror of the above: a failing check must print the failure and
    /// leave the workflow exactly where it was, rather than advancing on
    /// bad evidence.
    #[test]
    fn advance_run_checks_does_not_advance_on_failure() {
        let repo = tempdir().unwrap();
        let root = tempdir().unwrap();
        let state_dir = StateDir::from_root(root.path().to_path_buf());

        let git = |args: &[&str]| {
            let status = std::process::Command::new("git")
                .args([
                    "-c",
                    "user.email=t@example.com",
                    "-c",
                    "user.name=t",
                    "-c",
                    "commit.gpgsign=false",
                ])
                .args(args)
                .current_dir(repo.path())
                .status()
                .expect("run git");
            assert!(status.success(), "git {args:?} failed");
        };
        git(&["init", "-q"]);
        std::fs::write(repo.path().join("README.md"), "readme\n").unwrap();
        git(&["add", "."]);
        git(&["commit", "-q", "-m", "base"]);

        std::fs::create_dir_all(repo.path().join(".zirv")).unwrap();
        let failing = if cfg!(windows) { "exit /b 1" } else { "exit 1" };
        std::fs::write(
            repo.path().join(".zirv/verify.toml"),
            format!("schema_version=1\n[[checks]]\nid='unit'\nkind='unit'\ncommand='{failing}'\n"),
        )
        .unwrap();

        let mut state = WorkflowState::start(
            repo.path().to_path_buf(),
            "small feature".into(),
            WorkflowKind::Feature,
            None,
            true,
            low_classification(),
        );
        let test_index = state
            .steps
            .iter()
            .position(|step| step.phase == WorkflowPhase::Test)
            .unwrap();
        state.completed_steps = state.steps[..test_index]
            .iter()
            .map(|step| step.id.clone())
            .collect();
        state.current_step = test_index;
        state.status = WorkflowStatus::Running;
        let id = state.id.clone();
        save(&state_dir, &state, true).unwrap();

        let _state_dir_env = crate::commands::ctx::testenv::VarGuard::set(&[(
            "ZIRV_CTX_STATE_DIR",
            Some(root.path().to_str().expect("utf-8 tempdir path")),
        )]);
        let args = WorkflowArgs {
            command: WorkflowSubcommand::Advance(AdvanceArgs {
                id: id.clone(),
                outcome: None,
                run_checks: true,
                repo: Some(repo.path().to_path_buf()),
                json: false,
                duration_ms: None,
                agent: None,
                model: None,
                role: None,
                input_tokens: None,
                output_tokens: None,
                workers: 0,
                frontend_root: None,
                accept_preexisting_findings: false,
            }),
        };
        let mut out = Vec::new();
        let code = run(&args, &mut out).unwrap();
        assert_eq!(code, 1, "a failing check must not advance");
        let text = String::from_utf8(out).unwrap();
        assert!(
            text.contains("was not advanced"),
            "expected the failure to be reported, got {text}"
        );

        let reloaded = load(&state_dir, repo.path(), &id).unwrap();
        assert_eq!(
            reloaded.current().unwrap().phase,
            WorkflowPhase::Test,
            "a failing check must leave the workflow on the test step"
        );
    }

    /// Dogfooding regression: on a repository with a recorded test baseline
    /// (`zirv test baseline`), `--run-checks` used to decide pass/fail from
    /// `run_test`/`run_verify`'s own raw exit code, which reflects the
    /// unwaived result -- non-zero even when the only failure is already
    /// covered by the baseline. The very next `--outcome success` against the
    /// identical persisted report advanced fine, because that path (and the
    /// gate `--run-checks` now shares) reads the report back through
    /// `latest_is_fresh_and_passing`, which is baseline-aware. This proves
    /// `--run-checks` advances in that exact situation instead of reporting
    /// "checks failed".
    #[test]
    fn advance_run_checks_advances_when_the_only_failure_is_baselined() {
        let repo = tempdir().unwrap();
        let root = tempdir().unwrap();
        let home = tempdir().unwrap();
        let _home_guard = crate::commands::ctx::testenv::HomeGuard::set(home.path());
        let state_dir = StateDir::from_root(root.path().to_path_buf());

        let git = |args: &[&str]| {
            let status = std::process::Command::new("git")
                .args([
                    "-c",
                    "user.email=t@example.com",
                    "-c",
                    "user.name=t",
                    "-c",
                    "commit.gpgsign=false",
                ])
                .args(args)
                .current_dir(repo.path())
                .status()
                .expect("run git");
            assert!(status.success(), "git {args:?} failed");
        };
        git(&["init", "-q"]);
        std::fs::write(repo.path().join("README.md"), "readme\n").unwrap();
        git(&["add", "."]);
        git(&["commit", "-q", "-m", "base"]);

        crate::commands::workflow::verification::save_baseline(
            repo.path(),
            BTreeSet::from(["wrap::tests::a".to_string()]),
        )
        .unwrap();

        std::fs::create_dir_all(repo.path().join(".zirv")).unwrap();
        // A `unit`-kind check whose output has the exact shape
        // `parse_cargo_test_failure_names`/`FailureNameScanner` recognize --
        // a `failures:` header naming `wrap::tests::a`, immediately followed
        // by a `test result: FAILED` line -- and a non-zero exit, so the
        // check itself is genuinely `Failed`; only the recorded baseline
        // makes the gate pass.
        let baselined_failure = if cfg!(windows) {
            "echo failures: & echo wrap::tests::a & echo test result: FAILED. 0 passed; 1 failed; \
             0 ignored; 0 measured; 0 filtered out; finished in 0.00s & exit /b 101"
        } else {
            "printf \"failures:\\nwrap::tests::a\\ntest result: FAILED. 0 passed; 1 failed; 0 \
             ignored; 0 measured; 0 filtered out; finished in 0.00s\\n\"; exit 101"
        };
        std::fs::write(
            repo.path().join(".zirv/verify.toml"),
            format!("schema_version=1\n[[checks]]\nid='unit'\nkind='unit'\ncommand='{baselined_failure}'\n"),
        )
        .unwrap();

        let mut state = WorkflowState::start(
            repo.path().to_path_buf(),
            "small feature".into(),
            WorkflowKind::Feature,
            None,
            true,
            low_classification(),
        );
        let test_index = state
            .steps
            .iter()
            .position(|step| step.phase == WorkflowPhase::Test)
            .unwrap();
        state.completed_steps = state.steps[..test_index]
            .iter()
            .map(|step| step.id.clone())
            .collect();
        state.current_step = test_index;
        state.status = WorkflowStatus::Running;
        let id = state.id.clone();
        save(&state_dir, &state, true).unwrap();

        let _state_dir_env = crate::commands::ctx::testenv::VarGuard::set(&[(
            "ZIRV_CTX_STATE_DIR",
            Some(root.path().to_str().expect("utf-8 tempdir path")),
        )]);
        let args = WorkflowArgs {
            command: WorkflowSubcommand::Advance(AdvanceArgs {
                id: id.clone(),
                outcome: None,
                run_checks: true,
                repo: Some(repo.path().to_path_buf()),
                json: false,
                duration_ms: None,
                agent: None,
                model: None,
                role: None,
                input_tokens: None,
                output_tokens: None,
                workers: 0,
                frontend_root: None,
                accept_preexisting_findings: false,
            }),
        };
        let mut out = Vec::new();
        let code = run(&args, &mut out).unwrap();
        assert_eq!(
            code, 0,
            "a failure fully covered by the recorded baseline must still advance"
        );

        let reloaded = load(&state_dir, repo.path(), &id).unwrap();
        assert_eq!(
            reloaded.current().unwrap().phase,
            WorkflowPhase::Verify,
            "the test step must have advanced on the baselined report"
        );

        // H-2: the just-persisted Test-phase report is itself gate-passing
        // (baseline-covered), but `run_required_checks`'s own
        // `last_failure_fingerprint` guard used to treat any `passed():
        // false` report as "the previous failed attempt" regardless of the
        // baseline -- with the worktree still byte-identical, that made the
        // Verify step's own `--run-checks` return `Unchanged` and never
        // actually run, instead of running (and passing, via the same
        // baseline) as it must here.
        let code_again = run(&args, &mut out).unwrap();
        assert_eq!(
            code_again, 0,
            "the Verify step's own baselined run must advance, not report Unchanged"
        );
        let reloaded_again = load(&state_dir, repo.path(), &id).unwrap();
        assert_eq!(
            reloaded_again.current().unwrap().phase,
            WorkflowPhase::Deploy,
            "the Verify step must have advanced on its own baselined report"
        );
    }

    /// Fail-open regression: a stale, still-fingerprint-fresh PASSING report
    /// already sits at `latest` (fingerprint unchanged since -- nothing in
    /// the tree moved). The run this `--run-checks` call actually performs
    /// FAILS, but persisting its report is made to fail too (`latest`'s
    /// pointer file is read-only, so `run_and_persist`'s `persist` call hits
    /// a genuine IO error -- swallowed into a warning, never an error, so
    /// the run's own printed results survive). Before the identity check,
    /// `latest_is_fresh_and_passing` would still read the untouched stale
    /// PASSING report and the gate would incorrectly advance. It must not:
    /// the identity of `latest` is unchanged, so no fresh report exists to
    /// gate on, and the step must stay put.
    #[test]
    fn advance_run_checks_does_not_advance_when_persistence_silently_fails() {
        let repo = tempdir().unwrap();
        let root = tempdir().unwrap();
        let state_dir = StateDir::from_root(root.path().to_path_buf());

        let git = |args: &[&str]| {
            let status = std::process::Command::new("git")
                .args([
                    "-c",
                    "user.email=t@example.com",
                    "-c",
                    "user.name=t",
                    "-c",
                    "commit.gpgsign=false",
                ])
                .args(args)
                .current_dir(repo.path())
                .status()
                .expect("run git");
            assert!(status.success(), "git {args:?} failed");
        };
        git(&["init", "-q"]);
        std::fs::write(repo.path().join("README.md"), "readme\n").unwrap();
        git(&["add", "."]);
        git(&["commit", "-q", "-m", "base"]);

        std::fs::create_dir_all(repo.path().join(".zirv")).unwrap();
        let failing = if cfg!(windows) { "exit /b 1" } else { "exit 1" };
        std::fs::write(
            repo.path().join(".zirv/verify.toml"),
            format!("schema_version=1\n[[checks]]\nid='unit'\nkind='unit'\ncommand='{failing}'\n"),
        )
        .unwrap();

        // Seed a stale but still fingerprint-fresh PASSING report at
        // `latest`, matching the exact tree state above.
        let fingerprint =
            crate::commands::workflow::verification::change_fingerprint(repo.path()).unwrap();
        let stale_passing_report = crate::commands::workflow::verification::VerificationReport {
            schema_version: crate::commands::workflow::verification::VERIFY_REPORT_SCHEMA_VERSION,
            id: "stale-pass".into(),
            mode: crate::commands::workflow::verification::VerificationMode::Changed,
            source: "configured".into(),
            repo: repo.path().to_path_buf(),
            branch: String::new(),
            head_sha: String::new(),
            change_fingerprint: fingerprint,
            changed_paths: vec![],
            fallback_to_full: false,
            narrowed_to: vec![],
            notes: vec![],
            started_at: 0,
            finished_at: 0,
            checks: vec![crate::commands::workflow::verification::CheckResult {
                id: "unit".into(),
                kind: crate::commands::workflow::verification::CheckKind::Unit,
                command: "true".into(),
                source: crate::commands::workflow::verification::CheckSource::DiscoveredToolchain,
                status: crate::commands::workflow::verification::CheckStatus::Passed,
                exit_code: Some(0),
                duration_ms: 1,
                failure_output: None,
                failure_test_names: Vec::new(),
                inconclusive_reason: None,
            }],
        };
        crate::commands::workflow::verification::save_report(&state_dir, &stale_passing_report)
            .unwrap();

        // Make the next persist's `latest`-pointer write fail: `write_private`
        // writes a temp sibling then `rename`s it over `latest`.
        let latest_pointer = state_dir
            .verification()
            .join(repo_slug(repo.path()))
            .join("latest");
        // On Unix a rename over a read-only FILE succeeds (the directory's
        // write bit governs), so there the directory holding `latest` is
        // what gets locked; on Windows the read-only destination file itself
        // makes the rename fail with ERROR_ACCESS_DENIED.
        let lock_target = if cfg!(unix) {
            latest_pointer.parent().unwrap().to_path_buf()
        } else {
            latest_pointer.clone()
        };
        let original_perms = std::fs::metadata(&lock_target).unwrap().permissions();
        let mut readonly_perms = original_perms.clone();
        readonly_perms.set_readonly(true);
        std::fs::set_permissions(&lock_target, readonly_perms).unwrap();

        let mut state = WorkflowState::start(
            repo.path().to_path_buf(),
            "small feature".into(),
            WorkflowKind::Feature,
            None,
            true,
            low_classification(),
        );
        let test_index = state
            .steps
            .iter()
            .position(|step| step.phase == WorkflowPhase::Test)
            .unwrap();
        state.completed_steps = state.steps[..test_index]
            .iter()
            .map(|step| step.id.clone())
            .collect();
        state.current_step = test_index;
        state.status = WorkflowStatus::Running;
        let id = state.id.clone();
        save(&state_dir, &state, true).unwrap();

        let _state_dir_env = crate::commands::ctx::testenv::VarGuard::set(&[(
            "ZIRV_CTX_STATE_DIR",
            Some(root.path().to_str().expect("utf-8 tempdir path")),
        )]);
        let args = WorkflowArgs {
            command: WorkflowSubcommand::Advance(AdvanceArgs {
                id: id.clone(),
                outcome: None,
                run_checks: true,
                repo: Some(repo.path().to_path_buf()),
                json: false,
                duration_ms: None,
                agent: None,
                model: None,
                role: None,
                input_tokens: None,
                output_tokens: None,
                workers: 0,
                frontend_root: None,
                accept_preexisting_findings: false,
            }),
        };
        let mut out = Vec::new();
        let result = run(&args, &mut out);

        // Cleanup before any assertion panics, so the tempdir can still be
        // removed on drop even if an assertion below fails. Restores the
        // exact original permissions rather than `set_readonly(false)`,
        // which clippy flags as leaving the file world-writable on Unix.
        std::fs::set_permissions(&lock_target, original_perms).unwrap();

        let code = result.unwrap();
        assert_eq!(
            code, 1,
            "a genuinely failing run whose report could not be persisted must not advance on a \
             stale prior report"
        );
        let text = String::from_utf8(out).unwrap();
        assert!(
            text.contains("no fresh report was persisted"),
            "expected the persistence-failure reason to be reported, got {text}"
        );

        let reloaded = load(&state_dir, repo.path(), &id).unwrap();
        assert_eq!(
            reloaded.current().unwrap().phase,
            WorkflowPhase::Test,
            "the step must not advance on stale evidence when the fresh report never persisted"
        );
    }

    /// `--run-checks` only knows how to satisfy a `Test`/`Verify` step's own
    /// evidence gate; any other current step must fail loudly rather than
    /// silently treating itself as satisfied.
    #[test]
    fn advance_run_checks_rejects_a_non_test_verify_step() {
        let repo = tempdir().unwrap();
        let root = tempdir().unwrap();
        let state_dir = StateDir::from_root(root.path().to_path_buf());
        let state = skip_leading_artifact_steps(WorkflowState::start(
            repo.path().to_path_buf(),
            "small feature".into(),
            WorkflowKind::Feature,
            None,
            true,
            low_classification(),
        ));
        assert_eq!(state.current().unwrap().phase, WorkflowPhase::Implement);
        let id = state.id.clone();
        save(&state_dir, &state, true).unwrap();

        let _state_dir_env = crate::commands::ctx::testenv::VarGuard::set(&[(
            "ZIRV_CTX_STATE_DIR",
            Some(root.path().to_str().expect("utf-8 tempdir path")),
        )]);
        let args = WorkflowArgs {
            command: WorkflowSubcommand::Advance(AdvanceArgs {
                id: id.clone(),
                outcome: None,
                run_checks: true,
                repo: Some(repo.path().to_path_buf()),
                json: false,
                duration_ms: None,
                agent: None,
                model: None,
                role: None,
                input_tokens: None,
                output_tokens: None,
                workers: 0,
                frontend_root: None,
                accept_preexisting_findings: false,
            }),
        };
        let mut out = Vec::new();
        let error = run(&args, &mut out).unwrap_err().to_string();
        assert!(
            error.contains("--run-checks") && error.contains("--outcome instead"),
            "{error}"
        );
    }

    #[test]
    fn advance_frontend_root_flag_persists_into_state() {
        let repo = tempdir().unwrap();
        let target_repo = tempdir().unwrap();
        let root = tempdir().unwrap();
        let state_dir = StateDir::from_root(root.path().to_path_buf());
        let state = skip_leading_artifact_steps(WorkflowState::start(
            repo.path().to_path_buf(),
            "small feature".into(),
            WorkflowKind::Feature,
            None,
            true,
            low_classification(),
        ));
        let id = state.id.clone();
        save(&state_dir, &state, true).unwrap();

        let _state_dir_env = crate::commands::ctx::testenv::VarGuard::set(&[(
            "ZIRV_CTX_STATE_DIR",
            Some(root.path().to_str().expect("utf-8 tempdir path")),
        )]);
        let args = WorkflowArgs {
            command: WorkflowSubcommand::Advance(AdvanceArgs {
                id: id.clone(),
                outcome: Some(StepOutcome::Failure),
                run_checks: false,
                repo: Some(repo.path().to_path_buf()),
                json: false,
                duration_ms: None,
                agent: None,
                model: None,
                role: None,
                input_tokens: None,
                output_tokens: None,
                workers: 0,
                frontend_root: Some(target_repo.path().to_path_buf()),
                accept_preexisting_findings: false,
            }),
        };
        let mut out = Vec::new();
        run(&args, &mut out).unwrap();

        let reloaded = load(&state_dir, repo.path(), &id).unwrap();
        assert_eq!(
            reloaded.frontend_target_root,
            Some(target_repo.path().canonicalize().unwrap())
        );
    }

    #[test]
    fn advance_persists_frontend_root_before_the_gate_even_when_it_still_fails_closed() {
        // #214 follow-up: `--frontend-root` must be saved before the gate
        // runs, so a fail-closed advance (the target root has no fresh
        // evidence yet) still records the root -- the operator should not
        // have to pass the flag again on retry.
        let workflow_repo = tempdir().unwrap();
        let target_repo = tempdir().unwrap();
        let root = tempdir().unwrap();
        let state_dir = StateDir::from_root(root.path().to_path_buf());

        let mut classification = low_classification();
        classification.work_domain.domain = WorkDomain::Frontend;
        classification.work_domain.score = 55;
        let mut state = WorkflowState::start(
            workflow_repo.path().to_path_buf(),
            "build a frontend component".into(),
            WorkflowKind::Feature,
            None,
            true,
            classification,
        );
        let test_index = state
            .steps
            .iter()
            .position(|step| step.phase == WorkflowPhase::Test)
            .unwrap();
        state.completed_steps = state.steps[..test_index]
            .iter()
            .map(|step| step.id.clone())
            .collect();
        state.current_step = test_index;
        state.status = WorkflowStatus::Running;
        let id = state.id.clone();
        save(&state_dir, &state, true).unwrap();

        let _state_dir_env = crate::commands::ctx::testenv::VarGuard::set(&[(
            "ZIRV_CTX_STATE_DIR",
            Some(root.path().to_str().expect("utf-8 tempdir path")),
        )]);
        let args = WorkflowArgs {
            command: WorkflowSubcommand::Advance(AdvanceArgs {
                id: id.clone(),
                outcome: Some(StepOutcome::Success),
                run_checks: false,
                repo: Some(workflow_repo.path().to_path_buf()),
                json: false,
                duration_ms: None,
                agent: None,
                model: None,
                role: None,
                input_tokens: None,
                output_tokens: None,
                workers: 0,
                // `target_repo` is empty and has no detector evidence of its
                // own, so the gate must still fail closed against it.
                frontend_root: Some(target_repo.path().to_path_buf()),
                accept_preexisting_findings: false,
            }),
        };
        let mut out = Vec::new();
        let result = run(&args, &mut out);
        assert!(
            result.is_err(),
            "expected the gate to still fail closed against an empty target root"
        );

        let reloaded = load(&state_dir, workflow_repo.path(), &id).unwrap();
        assert_eq!(
            reloaded.frontend_target_root,
            Some(target_repo.path().canonicalize().unwrap())
        );
    }

    /// Dash refresh PR1: `zirv workflow start` binds the workflow it just
    /// started onto whatever `ZIRV_CTX_SESSION` names, so the dashboard can
    /// resolve THIS pane's own workflow step from its own session record
    /// rather than the repo-wide active pointer.
    #[test]
    fn start_binds_the_new_workflow_onto_the_calling_session() {
        let repo = tempdir().unwrap();
        let root = tempdir().unwrap();
        let session_id = "11111111-2222-4333-8444-555555555555";
        let _env = crate::commands::ctx::testenv::VarGuard::set(&[
            (
                "ZIRV_CTX_STATE_DIR",
                Some(root.path().to_str().expect("utf-8 tempdir path")),
            ),
            ("ZIRV_CTX_SESSION", Some(session_id)),
        ]);
        let state_dir = resolve_state().unwrap();
        // The calling session must already be registered -- `bind_workflow_
        // id` is a patch onto an existing record, never a fresh one.
        let short = crate::commands::ctx::sessions::short_id(session_id);
        let record = crate::commands::ctx::sessions::Record::new(
            session_id,
            "claude",
            repo.path(),
            crate::commands::ctx::sessions::Verb::Chat,
        );
        let _guard = crate::commands::ctx::sessions::SessionGuard::register(&state_dir, record);

        let args = WorkflowArgs {
            command: WorkflowSubcommand::Start(StartArgs {
                id: Some("bugfix".into()),
                task: "fix a database retry bug".into(),
                agent: None,
                built_in_only: true,
                repo: Some(repo.path().to_path_buf()),
                paths: vec![PathBuf::from("src/commands/ctx/safety.rs")],
                changed_lines: Some(40),
                tests_changed: true,
                complexity: None,
                risk: None,
                branch: None,
                frontend_root: None,
                brainstorm: false,
                no_brainstorm: false,
                profile: None,
                json: false,
            }),
        };
        let mut out = Vec::new();
        run(&args, &mut out).unwrap();

        let started = load_active(&state_dir, repo.path()).unwrap().unwrap();
        assert!(
            crate::commands::ctx::sessions::load_record(&state_dir, &short).is_some(),
            "the calling session's record is still there"
        );
        // Round 2 coordinator review: the binding lives in its own sibling
        // file now, never a `Record` field -- see `bind_workflow_id`'s own
        // doc comment.
        assert_eq!(
            crate::commands::ctx::sessions::workflow_id_for(&state_dir, &short).as_deref(),
            Some(started.id.as_str())
        );
    }

    /// #255 recovery path (i): a task classified General/Standard (no
    /// frontend text or path signal) can still be forced onto the Frontend
    /// methodology overlay with `--profile`, applied after classification
    /// materializes the default steps.
    #[test]
    fn start_profile_flag_overrides_automatic_classification() {
        let repo = tempdir().unwrap();
        let root = tempdir().unwrap();
        let _state_dir_env = crate::commands::ctx::testenv::VarGuard::set(&[(
            "ZIRV_CTX_STATE_DIR",
            Some(root.path().to_str().expect("utf-8 tempdir path")),
        )]);
        let args = WorkflowArgs {
            command: WorkflowSubcommand::Start(StartArgs {
                id: Some("bugfix".into()),
                task: "fix a database retry bug".into(),
                agent: None,
                built_in_only: true,
                repo: Some(repo.path().to_path_buf()),
                // Declared, so classification never needs a real git
                // repository: this test is about `--profile`, not about
                // git-measured risk.
                paths: vec![PathBuf::from("src/commands/ctx/safety.rs")],
                changed_lines: Some(40),
                tests_changed: true,
                complexity: None,
                risk: None,
                branch: None,
                frontend_root: None,
                brainstorm: false,
                no_brainstorm: false,
                profile: Some(WorkflowProfile::Frontend),
                json: false,
            }),
        };
        let mut out = Vec::new();
        run(&args, &mut out).unwrap();

        let state_dir = resolve_state().unwrap();
        let state = load_active(&state_dir, repo.path()).unwrap().unwrap();
        assert_eq!(
            state.classification.work_domain.domain,
            WorkDomain::General,
            "the task/paths alone must not have classified this as Frontend"
        );
        assert_eq!(state.profile, WorkflowProfile::Frontend);
        assert_eq!(state.profile_source, ProfileSource::OperatorOverride);
        assert!(
            state
                .steps
                .iter()
                .any(|step| step.skill == "frontend-implement")
        );
    }

    /// An explicit id is matched case-insensitively -- `zirv workflow start
    /// Bugfix` must resolve exactly like `zirv workflow start bugfix`
    /// rather than exiting 2 "unknown workflow 'Bugfix'".
    #[test]
    fn start_resolves_an_explicit_id_case_insensitively() {
        let repo = tempdir().unwrap();
        let root = tempdir().unwrap();
        let _state_dir_env = crate::commands::ctx::testenv::VarGuard::set(&[(
            "ZIRV_CTX_STATE_DIR",
            Some(root.path().to_str().expect("utf-8 tempdir path")),
        )]);
        let args = WorkflowArgs {
            command: WorkflowSubcommand::Start(StartArgs {
                id: Some("Bugfix".into()),
                task: "fix a database retry bug".into(),
                agent: None,
                built_in_only: true,
                repo: Some(repo.path().to_path_buf()),
                paths: vec![PathBuf::from("src/commands/ctx/safety.rs")],
                changed_lines: Some(40),
                tests_changed: true,
                complexity: None,
                risk: None,
                branch: None,
                frontend_root: None,
                brainstorm: false,
                no_brainstorm: false,
                profile: None,
                json: false,
            }),
        };
        let mut out = Vec::new();
        run(&args, &mut out).expect("'Bugfix' must resolve like 'bugfix'");

        let state_dir = resolve_state().unwrap();
        let state = load_active(&state_dir, repo.path()).unwrap().unwrap();
        assert_eq!(state.kind, WorkflowKind::Bugfix);
        assert_eq!(
            state.definition.as_ref().map(|d| d.id.as_str()),
            Some("bugfix")
        );
    }

    /// F6 (blind-review finding, 2026-09-24): `zirv workflow start` must
    /// never pre-create the first step's artifact file in the worktree, or
    /// touch the git index -- a blind reviewer flagged the untouched
    /// `.zirv/work/<id>/intent.md` as a stray addition in 16/20 benchmark
    /// runs. `architecture-discovery`'s first step ("scope") carries
    /// `ArtifactStage::Intent`, so this exercises the exact shape the
    /// finding reported. The path and template text stay discoverable
    /// through `render_current_context` (what `zirv workflow context`
    /// prints) without the file existing at all.
    #[test]
    fn start_never_pre_creates_the_first_steps_artifact_or_touches_the_index() {
        let repo = tempdir().unwrap();
        let root = tempdir().unwrap();
        git_init_with_commit(repo.path());
        let _state_dir_env = crate::commands::ctx::testenv::VarGuard::set(&[(
            "ZIRV_CTX_STATE_DIR",
            Some(root.path().to_str().expect("utf-8 tempdir path")),
        )]);
        let args = WorkflowArgs {
            command: WorkflowSubcommand::Start(StartArgs {
                id: Some("architecture-discovery".into()),
                task: "map the architecture of the billing subsystem".into(),
                agent: None,
                built_in_only: true,
                repo: Some(repo.path().to_path_buf()),
                paths: Vec::new(),
                changed_lines: None,
                tests_changed: false,
                complexity: None,
                risk: None,
                branch: None,
                frontend_root: None,
                brainstorm: false,
                no_brainstorm: false,
                profile: None,
                json: false,
            }),
        };
        let mut out = Vec::new();
        run(&args, &mut out).expect("start");

        let state_dir = resolve_state().unwrap();
        let state = load_active(&state_dir, repo.path()).unwrap().unwrap();
        let stage = state
            .current()
            .and_then(|step| step.artifact)
            .expect("architecture-discovery's first step carries an artifact");
        assert_eq!(stage, ArtifactStage::Intent);
        let path = workflow_artifact_path(&state, stage).unwrap();
        assert!(
            !path.exists(),
            "start must not pre-create the artifact file: {}",
            path.display()
        );

        // No git-visible change: the worktree and the index are both
        // exactly as `git_init_with_commit` left them.
        let status = std::process::Command::new("git")
            .arg("-C")
            .arg(repo.path())
            .args(["status", "--porcelain"])
            .output()
            .expect("git status");
        assert!(
            status.stdout.is_empty(),
            "start must leave the worktree and index untouched: {}",
            String::from_utf8_lossy(&status.stdout)
        );

        // The path and the template text are still discoverable without the
        // file existing.
        let rendered = render_current_context(&state, repo.path(), None)
            .unwrap()
            .expect("a running workflow has step context");
        assert!(
            rendered.contains("not yet created"),
            "context must say the artifact is not yet created: {rendered}"
        );
        assert!(
            rendered.contains(stage.template()),
            "context must carry the template text so the agent knows what to write: {rendered}"
        );
    }

    /// Issue #782: `start_workflow` now runs the same off-by-default Jev
    /// intent refinement `zirv workflow classify` does, right before
    /// `selection::select_definition` -- with the gate at its default (off),
    /// this must leave the deterministic classification (and the pack
    /// `selection` it drives) completely untouched.
    #[test]
    fn start_leaves_intent_and_selection_untouched_when_the_jev_classify_gate_is_off() {
        let repo = tempdir().unwrap();
        let root = tempdir().unwrap();
        let _state_dir_env = crate::commands::ctx::testenv::VarGuard::set(&[(
            "ZIRV_CTX_STATE_DIR",
            Some(root.path().to_str().expect("utf-8 tempdir path")),
        )]);
        let args = WorkflowArgs {
            command: WorkflowSubcommand::Start(StartArgs {
                id: None,
                task: "fix a database retry bug".into(),
                agent: None,
                built_in_only: true,
                repo: Some(repo.path().to_path_buf()),
                paths: vec![PathBuf::from("src/commands/ctx/safety.rs")],
                changed_lines: Some(40),
                tests_changed: true,
                complexity: None,
                risk: None,
                branch: None,
                frontend_root: None,
                brainstorm: false,
                no_brainstorm: false,
                profile: None,
                json: false,
            }),
        };
        let mut out = Vec::new();
        run(&args, &mut out).expect("start");

        let state_dir = resolve_state().unwrap();
        let state = load_active(&state_dir, repo.path()).unwrap().unwrap();
        assert_eq!(state.classification.intent, Intent::Bugfix);
        assert!(
            !state
                .classification
                .reasons
                .iter()
                .any(|reason| reason.starts_with("jev:")),
            "{:?}",
            state.classification.reasons
        );
        assert_eq!(
            state.kind,
            WorkflowKind::Bugfix,
            "selection must still pick the bugfix pack from the untouched intent"
        );
    }

    /// `zirv workflow show` resolves its id the same case-insensitive way
    /// as `start`.
    #[test]
    fn show_resolves_an_explicit_id_case_insensitively() {
        let repo = tempdir().unwrap();
        let registry = load_workflow_registry(repo.path(), true).unwrap();
        let workflow = registry.get("bugfix").unwrap();
        let args = ShowArgs {
            id: "BUGFIX".into(),
            json: false,
            built_in_only: true,
            repo: Some(repo.path().to_path_buf()),
        };
        let full_args = WorkflowArgs {
            command: WorkflowSubcommand::Show(args),
        };
        let mut out = Vec::new();
        run(&full_args, &mut out).expect("'BUGFIX' must resolve like 'bugfix'");
        assert_eq!(workflow.definition.id, "bugfix");
    }

    /// The exact wording of the note `start_workflow` prints to STDERR when
    /// a start silently displaces a different, still-running workflow as
    /// this repository's active one. A pure fn, so the wording is checked
    /// directly rather than by capturing real process STDERR.
    #[test]
    fn active_workflow_displaced_note_names_both_ids_and_the_resume_command() {
        let note = active_workflow_displaced_note("abc-123", "feature");
        assert!(note.starts_with("note: "), "{note}");
        assert!(note.contains("abc-123"), "{note}");
        assert!(note.contains("(feature)"), "{note}");
        assert!(
            note.contains("zirv workflow resume abc-123"),
            "must point at the exact resume command: {note}"
        );
    }

    /// Review finding: a write failure (a closed stderr, say) must never
    /// panic -- the new workflow this note is ABOUT is already saved by the
    /// time it's printed, so a failure here must degrade silently rather
    /// than turning an already-successful start into a reported failure.
    #[test]
    fn best_effort_write_displacement_note_never_panics_on_a_failing_writer() {
        struct AlwaysErrors;
        impl std::io::Write for AlwaysErrors {
            fn write(&mut self, _buf: &[u8]) -> std::io::Result<usize> {
                Err(std::io::Error::other("closed"))
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Err(std::io::Error::other("closed"))
            }
        }
        // Must not panic; a `writeln!`/`eprintln!`-shaped implementation
        // that propagated the error with `.unwrap()`/`.expect()` would.
        best_effort_write_displacement_note(AlwaysErrors, "note: irrelevant");
    }

    /// Starting a second workflow for the same repository while an earlier
    /// one is still `Running` must NOT refuse -- multiple workflows per
    /// repository are legitimate, and `zirv workflow resume` restores the
    /// displaced one. This only proves
    /// the non-refusal and that the active pointer now names the new run;
    /// the note text itself is covered by
    /// `active_workflow_displaced_note_names_both_ids_and_the_resume_command`
    /// since real process STDERR isn't capturable through this seam.
    #[test]
    fn starting_a_second_workflow_never_refuses_and_moves_the_active_pointer() {
        let repo = tempdir().unwrap();
        let root = tempdir().unwrap();
        let _state_dir_env = crate::commands::ctx::testenv::VarGuard::set(&[(
            "ZIRV_CTX_STATE_DIR",
            Some(root.path().to_str().expect("utf-8 tempdir path")),
        )]);
        let start_args = |id: &str, task: &str| StartArgs {
            id: Some(id.to_string()),
            task: task.to_string(),
            agent: None,
            built_in_only: true,
            repo: Some(repo.path().to_path_buf()),
            paths: vec![PathBuf::from("src/commands/ctx/safety.rs")],
            changed_lines: Some(40),
            tests_changed: true,
            complexity: None,
            risk: None,
            branch: None,
            frontend_root: None,
            brainstorm: false,
            no_brainstorm: false,
            profile: None,
            json: false,
        };

        let mut out = Vec::new();
        run(
            &WorkflowArgs {
                command: WorkflowSubcommand::Start(start_args("bugfix", "fix the first thing")),
            },
            &mut out,
        )
        .unwrap();
        let state_dir = resolve_state().unwrap();
        let first = load_active(&state_dir, repo.path()).unwrap().unwrap();
        assert!(
            matches!(
                first.status,
                WorkflowStatus::Running | WorkflowStatus::AwaitingApproval
            ),
            "the first workflow must still be non-terminal: {:?}",
            first.status
        );

        let mut out2 = Vec::new();
        let result = run(
            &WorkflowArgs {
                command: WorkflowSubcommand::Start(start_args("feature", "add the second thing")),
            },
            &mut out2,
        );
        assert!(
            result.is_ok(),
            "a second workflow for the same repo must never be refused: {result:?}"
        );

        let second = load_active(&state_dir, repo.path()).unwrap().unwrap();
        assert_ne!(second.id, first.id);
        assert_eq!(second.kind, WorkflowKind::Feature);

        // The first workflow's own state is untouched -- still non-terminal,
        // still loadable, `resume`-able exactly as the note says.
        let reloaded_first = load(&state_dir, repo.path(), &first.id).unwrap();
        assert_eq!(reloaded_first.status, first.status);
    }

    #[test]
    fn workflow_artifact_status_reports_pending_accepted_and_drifted() {
        let repo = tempdir().unwrap();
        let root = tempdir().unwrap();
        let state_dir = StateDir::from_root(root.path().to_path_buf());
        // `approve` refreshes the deploy tier, which re-materializes `steps`
        // straight from `state.classification` -- so this needs a
        // classification that naturally keeps exactly one artifact step
        // (intent) through that regeneration. Bugfix's plan gate is
        // `ComplexityOrRisk{Substantial, High}` (unlike Feature's, which
        // shares intent's own `Bounded` threshold), so Bounded complexity
        // here gates intent in without also gating plan in.
        let mut classification = low_classification();
        classification.complexity = Complexity::Bounded;
        let state = WorkflowState::start(
            repo.path().to_path_buf(),
            "small bugfix".into(),
            WorkflowKind::Bugfix,
            None,
            true,
            classification,
        );
        ensure_current_artifact_template(&state).unwrap();
        let pending = workflow_artifact_statuses(&state).unwrap();
        assert_eq!(pending.len(), 1);
        assert!(pending[0].exists);
        assert!(!pending[0].accepted);

        let intent = workflow_artifact_path(&state, ArtifactStage::Intent).unwrap();
        std::fs::write(
            &intent,
            "# Intent\n\n## Problem\nA\n\n## Desired outcome\nB\n",
        )
        .unwrap();
        let accepted = approve(&state_dir, state).unwrap();
        let statuses = workflow_artifact_statuses(&accepted).unwrap();
        assert!(statuses[0].accepted);
        assert!(!statuses[0].drifted);

        std::fs::write(&intent, "# Intent\nchanged\n").unwrap();
        let statuses = workflow_artifact_statuses(&accepted).unwrap();
        assert!(statuses[0].drifted);
    }

    /// A step with no recorded duration (an older saved state) renders its
    /// bare id, never a bogus "0m0s".
    #[test]
    fn write_state_renders_completed_step_wall_clock_only_when_known() {
        let repo = tempdir().unwrap();
        let mut state = WorkflowState::start(
            repo.path().to_path_buf(),
            "small feature".into(),
            WorkflowKind::Feature,
            None,
            true,
            low_classification(),
        );
        state.completed_steps = vec!["intent".to_string(), "spec".to_string()];
        state
            .step_durations_ms
            .insert("intent".to_string(), 130_000);
        state.step_durations_ms.insert("spec".to_string(), 40_000);
        let mut out = Vec::new();
        write_state(&mut out, &state, false).unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(
            text.contains("completed: intent (2m10s), spec (0m40s)"),
            "got: {text}"
        );

        // No recorded duration for a step (an older schema, or a test
        // fixture that only sets `completed_steps` directly): the bare id,
        // not a fabricated duration.
        let mut legacy = state.clone();
        legacy.step_durations_ms.clear();
        let mut out = Vec::new();
        write_state(&mut out, &legacy, false).unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("completed: intent, spec"), "got: {text}");
    }

    #[test]
    fn write_state_renders_brainstorm_only_when_the_workflow_has_an_intent_step() {
        let repo = tempdir().unwrap();
        // Feature's intent step is now conditional (`ComplexityOrRisk{Bounded,
        // Medium}`, same as Bugfix), so this needs a classification that
        // still gates one in.
        let mut classification = low_classification();
        classification.complexity = Complexity::Bounded;
        let feature = WorkflowState::start(
            repo.path().to_path_buf(),
            "small feature".into(),
            WorkflowKind::Feature,
            None,
            true,
            classification,
        );
        let mut out = Vec::new();
        write_state(&mut out, &feature, false).unwrap();
        assert!(String::from_utf8(out).unwrap().contains("brainstorm: on"));

        let review = WorkflowState::start(
            repo.path().to_path_buf(),
            "independent review".into(),
            WorkflowKind::Review,
            None,
            true,
            low_classification(),
        );
        let mut out = Vec::new();
        write_state(&mut out, &review, false).unwrap();
        assert!(!String::from_utf8(out).unwrap().contains("brainstorm:"));
    }

    /// Issue #685: `reclassify_at_gate` appends a "reclassified at step
    /// ...: measured risk ..." reason to `classification.reasons`, but
    /// `write_state`'s text render never printed it -- an operator's
    /// `--complexity`/`--risk` override at `workflow start` could be
    /// escalated by a later gate with no visible explanation short of
    /// `--json`. The text render must print every recorded reason.
    #[test]
    fn write_state_renders_classification_reasons() {
        let repo = tempdir().unwrap();
        let mut classification = low_classification();
        classification.reasons = vec![
            "small".into(),
            "reclassified at step 'review': measured risk High".into(),
        ];
        let state = WorkflowState::start(
            repo.path().to_path_buf(),
            "small feature".into(),
            WorkflowKind::Feature,
            None,
            true,
            classification,
        );
        let mut out = Vec::new();
        write_state(&mut out, &state, false).unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("- small"), "got: {text}");
        assert!(
            text.contains("- reclassified at step 'review': measured risk High"),
            "got: {text}"
        );
    }

    #[test]
    fn resume_refuses_a_closed_workflow() {
        let repo = tempdir().unwrap();
        let root = tempdir().unwrap();
        let state_dir = StateDir::from_root(root.path().to_path_buf());
        let state = WorkflowState::start(
            repo.path().to_path_buf(),
            "small feature".into(),
            WorkflowKind::Feature,
            None,
            true,
            low_classification(),
        );
        save(&state_dir, &state, true).unwrap();
        let closed = close(&state_dir, state, None).unwrap();

        let _state_dir_env = crate::commands::ctx::testenv::VarGuard::set(&[(
            "ZIRV_CTX_STATE_DIR",
            Some(root.path().to_str().expect("utf-8 tempdir path")),
        )]);
        let args = WorkflowArgs {
            command: WorkflowSubcommand::Resume(StateIdArgs {
                id: closed.id.clone(),
                repo: Some(repo.path().to_path_buf()),
            }),
        };
        let mut out = Vec::new();
        let error = run(&args, &mut out).unwrap_err().to_string();
        assert!(
            error.contains("cannot resume") && error.contains("Closed"),
            "{error}"
        );
    }

    /// A committed repository with a few pending (untracked) files, so
    /// `zirv workflow team plan`'s own undeclared classification has a real
    /// measured diff to size a Bounded team against.
    fn git_repo_with_pending_files(count: usize) -> tempfile::TempDir {
        let repo = tempdir().unwrap();
        let git = |args: &[&str]| {
            let status = std::process::Command::new("git")
                .args([
                    "-c",
                    "user.email=t@example.com",
                    "-c",
                    "user.name=t",
                    "-c",
                    "commit.gpgsign=false",
                ])
                .args(args)
                .current_dir(repo.path())
                .status()
                .expect("run git");
            assert!(status.success(), "git {args:?} failed");
        };
        git(&["init", "-q"]);
        std::fs::write(repo.path().join("README.md"), "readme\n").unwrap();
        git(&["add", "."]);
        git(&["commit", "-q", "-m", "base"]);
        // All under one `src/` prefix (not scattered at the repository
        // root), so `classify`'s cross-module signal never fires here and
        // this stays a plain Bounded, Low-risk change regardless of
        // `count` -- the point of this fixture is a real measured diff
        // sized as Bounded, not an incidental risk escalation.
        std::fs::create_dir_all(repo.path().join("src")).unwrap();
        for index in 0..count {
            std::fs::write(
                repo.path().join(format!("src/pending-{index}.rs")),
                "fn work() {}\n".repeat(15),
            )
            .unwrap();
        }
        repo
    }

    /// Issue #541: `zirv workflow team plan --json`'s printed plan is
    /// exactly what got persisted onto the active workflow -- the CLI never
    /// prints a plan different from the one a later `team show`/`team
    /// brief` would read back.
    #[test]
    fn workflow_team_plan_json_matches_the_stored_plan() {
        let repo = git_repo_with_pending_files(0);
        let home = tempdir().unwrap();
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());
        let root = tempdir().unwrap();
        let state_dir = StateDir::from_root(root.path().to_path_buf());
        let state = WorkflowState::start(
            repo.path().to_path_buf(),
            "small feature".into(),
            WorkflowKind::Feature,
            None,
            true,
            low_classification(),
        );
        save(&state_dir, &state, true).unwrap();

        let _state_dir_env = crate::commands::ctx::testenv::VarGuard::set(&[(
            "ZIRV_CTX_STATE_DIR",
            Some(root.path().to_str().expect("utf-8 tempdir path")),
        )]);
        let args = WorkflowArgs {
            command: WorkflowSubcommand::Team(crate::commands::workflow::team::TeamArgs {
                command: crate::commands::workflow::team::TeamCommand::Plan(
                    crate::commands::workflow::team::TeamPlanArgs {
                        objective: "add a small feature".into(),
                        workflow: None,
                        dry_run: false,
                        seat: None,
                        built_in_only: true,
                        repo: Some(repo.path().to_path_buf()),
                        json: true,
                    },
                ),
            }),
        };
        let mut out = Vec::new();
        let code = run(&args, &mut out).unwrap();
        assert_eq!(code, 0);
        let printed: crate::commands::workflow::team::TeamPlan =
            serde_json::from_slice(&out).unwrap();

        let stored = load_active(&state_dir, repo.path())
            .unwrap()
            .expect("workflow still active");
        assert_eq!(stored.team_plan, Some(printed));
    }

    /// Issue #541: `zirv workflow team brief <seat>` attaches only the
    /// skills that SEAT's own manifest references (the debugger's
    /// `systematic-debugging`), never the whole skill catalogue and never
    /// another seat's skills.
    #[test]
    fn workflow_team_brief_attaches_only_the_seats_skills() {
        let repo = git_repo_with_pending_files(3);
        let home = tempdir().unwrap();
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());
        let root = tempdir().unwrap();
        let state_dir = StateDir::from_root(root.path().to_path_buf());
        let state = WorkflowState::start(
            repo.path().to_path_buf(),
            "fix the crash".into(),
            WorkflowKind::Bugfix,
            None,
            true,
            low_classification(),
        );
        save(&state_dir, &state, true).unwrap();

        let _state_dir_env = crate::commands::ctx::testenv::VarGuard::set(&[(
            "ZIRV_CTX_STATE_DIR",
            Some(root.path().to_str().expect("utf-8 tempdir path")),
        )]);
        let plan_args = WorkflowArgs {
            command: WorkflowSubcommand::Team(crate::commands::workflow::team::TeamArgs {
                command: crate::commands::workflow::team::TeamCommand::Plan(
                    crate::commands::workflow::team::TeamPlanArgs {
                        objective: "fix the crash".into(),
                        workflow: None,
                        dry_run: false,
                        seat: None,
                        built_in_only: true,
                        repo: Some(repo.path().to_path_buf()),
                        json: true,
                    },
                ),
            }),
        };
        let mut plan_out = Vec::new();
        run(&plan_args, &mut plan_out).unwrap();
        let plan: crate::commands::workflow::team::TeamPlan =
            serde_json::from_slice(&plan_out).unwrap();
        assert!(
            plan.seats.iter().any(|seat| seat.id == "debugger-1"),
            "{plan:?}"
        );
        assert!(
            plan.seats.iter().any(|seat| seat.id == "implementer-1"),
            "{plan:?}"
        );

        let brief = |seat_id: &str| -> serde_json::Value {
            let args = WorkflowArgs {
                command: WorkflowSubcommand::Team(crate::commands::workflow::team::TeamArgs {
                    command: crate::commands::workflow::team::TeamCommand::Brief(
                        crate::commands::workflow::team::TeamBriefArgs {
                            seat_id: seat_id.to_string(),
                            workflow: None,
                            built_in_only: true,
                            repo: Some(repo.path().to_path_buf()),
                            json: true,
                        },
                    ),
                }),
            };
            let mut out = Vec::new();
            run(&args, &mut out).unwrap();
            serde_json::from_slice(&out).unwrap()
        };

        let debugger_brief = brief("debugger-1");
        let skills = debugger_brief["skills"].as_array().expect("skills array");
        assert_eq!(skills.len(), 1, "{debugger_brief}");
        assert_eq!(skills[0]["id"], "systematic-debugging");

        let implementer_brief = brief("implementer-1");
        assert_eq!(
            implementer_brief["skills"].as_array().unwrap().len(),
            0,
            "{implementer_brief}"
        );
    }

    /// Issue #542 chunk 3a decision 1: every referenced agent role must
    /// resolve before any state is written, independent of whether an
    /// execution adapter was even given at `workflow start`.
    #[test]
    fn an_unknown_agent_role_fails_at_materialise_before_state_is_written() {
        let repo = tempdir().unwrap();
        let root = tempdir().unwrap();
        let _env = crate::commands::ctx::testenv::VarGuard::set(&[
            (
                "ZIRV_CTX_STATE_DIR",
                Some(root.path().to_str().expect("utf-8 tempdir path")),
            ),
            ("ZIRV_CTX_WORKFLOW_REPO_WORKFLOWS", Some("true")),
        ]);
        let dir = repo.path().join(".zirv/workflows");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("broken-agent.toml"),
            r#"
schema_version = 1
id = "broken-agent"
version = 1
title = "Broken agent"
description = "References an agent role nothing provides."
domains = ["testing"]
effects = "repository"

[[steps]]
id = "work"
title = "Work"
phase = "implement"
skills = ["implement"]
agent_role = "no-such-role"
condition = "always"

[failure]
escalate_to = "human"

[completion]
present_as = "summary"
"#,
        )
        .unwrap();

        let args = WorkflowArgs {
            command: WorkflowSubcommand::Start(StartArgs {
                id: Some("broken-agent".into()),
                task: "do work".into(),
                agent: None,
                built_in_only: false,
                repo: Some(repo.path().to_path_buf()),
                paths: vec![],
                // Declared, so classification never needs a real git
                // repository -- this test is about agent-role validation.
                changed_lines: Some(10),
                tests_changed: false,
                complexity: None,
                risk: None,
                branch: None,
                frontend_root: None,
                brainstorm: false,
                no_brainstorm: false,
                profile: None,
                json: false,
            }),
        };
        let mut out = Vec::new();
        let error = run(&args, &mut out).unwrap_err().to_string();
        assert!(error.contains("unknown agent role"), "{error}");
        assert!(error.contains("no-such-role"), "{error}");

        let state_dir = resolve_state().unwrap();
        assert!(
            load_active(&state_dir, repo.path()).unwrap().is_none(),
            "no state may be written when agent-role validation fails"
        );
    }

    /// Issue #542 chunk 3b decision 3: an explicit id at `workflow start`
    /// always wins outright (no selection line printed, no `selection` in
    /// state), and that choice is durable across resume because it is
    /// simply what got pinned on `state.definition` -- selection never runs
    /// again on reload.
    #[test]
    fn an_explicit_id_overrides_selection_and_survives_resume() {
        let repo = tempdir().unwrap();
        let root = tempdir().unwrap();
        let _env = crate::commands::ctx::testenv::VarGuard::set(&[(
            "ZIRV_CTX_STATE_DIR",
            Some(root.path().to_str().expect("utf-8 tempdir path")),
        )]);
        let args = WorkflowArgs {
            command: WorkflowSubcommand::Start(StartArgs {
                id: Some("review".into()),
                task: "totally unrelated free-text objective".into(),
                agent: None,
                built_in_only: true,
                repo: Some(repo.path().to_path_buf()),
                paths: vec![],
                changed_lines: Some(5),
                tests_changed: false,
                complexity: None,
                risk: None,
                branch: None,
                frontend_root: None,
                brainstorm: false,
                no_brainstorm: false,
                profile: None,
                json: false,
            }),
        };
        let mut out = Vec::new();
        run(&args, &mut out).unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(
            !text.contains("selected:"),
            "an explicit id must never print a selection line: {text}"
        );

        let state_dir = resolve_state().unwrap();
        let state = load_active(&state_dir, repo.path()).unwrap().unwrap();
        assert_eq!(state.definition.as_ref().unwrap().id, "review");

        let resumed = load(&state_dir, repo.path(), &state.id).unwrap();
        assert_eq!(
            resumed.definition.as_ref().unwrap().id,
            "review",
            "the explicit override survives resume"
        );
    }
}
