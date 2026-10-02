//! Provider-neutral development workflows.
//!
//! This subsystem intentionally lives outside `ctx`: `ctx` supervises agent
//! processes, while this module owns reusable methodology and durable
//! workflow state. The two meet through small data interfaces instead of
//! provider-specific prompt/tool names.

use clap::{Parser, Subcommand};
use std::process::Child;
use std::time::Duration;

use super::ctx::CtxResult;

pub mod adoption;
pub mod agents;
pub mod artifact;
pub mod capability;
pub mod checks;
pub mod classify;
pub mod definition;
pub mod deploy;
pub mod engine;
pub mod frontend;
pub mod frontend_detector;
pub mod frontend_render;
pub mod maintain;
pub mod outcomes;
pub mod profile;
pub mod registry;
pub mod research;
pub mod review;
pub mod selection;
pub mod skill;
pub mod skill_activation;
pub mod skill_render;
pub mod skill_tools;
pub mod team;
pub mod telemetry;
pub mod verification;

/// Small workflow status view for the dashboard cache; it does not expose artifacts, findings or classification. (#209)
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActiveWorkflowSummary {
    pub kind: &'static str,
    pub step: String,
    pub awaiting_approval: bool,
    /// The pinned pack id, else the kind; shown by the agent tree's "back to seat" box.
    pub pack: String,
    /// Every step in order with how far the run is.
    pub steps: Vec<(String, StepMark)>,
    /// The task the workflow was started for, first line; shown beside the pack in the orchestrator dashboard's stepper.
    pub title: String,
    /// When the run started (unix seconds).
    pub started_at: u64,
    /// The first step after the current one that needs a person or a check: an approval, an external effect, a test, review or verify phase.
    pub next_gate: Option<String>,
}

/// How far a workflow run is past one step.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StepMark {
    Done,
    Current,
    Pending,
}

/// The same read `zirv workflow status` uses when no explicit `--id` is
/// given (`engine::load_active`): the repo's active-workflow pointer file,
/// then that workflow's own state file -- both plain file reads, no
/// subprocess, safe to call on a ~1s render-loop throttle.
///
/// `None` covers every reason there is nothing to show: no active workflow
/// for this repo, or one that failed to load (a torn write mid-save, an
/// unsupported schema version left behind by an older binary). The
/// dashboard's footer renders the same dim `▸ –` placeholder either way --
/// surfacing a parse error where an operator expects a status glyph would
/// be a worse failure mode than just not showing one.
pub fn active_workflow_summary(
    state: &crate::commands::ctx::state::StateDir,
    repo: &std::path::Path,
) -> Option<ActiveWorkflowSummary> {
    let wf = engine::load_active(state, repo).ok().flatten()?;
    let (pack, step) = pack_and_step(&wf);
    let next_gate = wf
        .steps
        .iter()
        .skip(wf.current_step + 1)
        .find(|s| {
            s.approval
                || s.effect == definition::EffectClass::External
                || matches!(
                    s.phase,
                    skill::WorkflowPhase::Test
                        | skill::WorkflowPhase::Review
                        | skill::WorkflowPhase::Verify
                )
        })
        .map(|s| s.id.clone());
    Some(ActiveWorkflowSummary {
        kind: wf.kind.as_str(),
        step,
        awaiting_approval: wf.status == engine::WorkflowStatus::AwaitingApproval,
        pack,
        steps: wf
            .steps
            .iter()
            .enumerate()
            .map(|(index, s)| {
                let mark = if wf.completed_steps.contains(&s.id) {
                    StepMark::Done
                } else if index == wf.current_step {
                    StepMark::Current
                } else {
                    StepMark::Pending
                };
                (s.id.clone(), mark)
            })
            .collect(),
        title: wf
            .task
            .lines()
            .next()
            .unwrap_or_default()
            .trim()
            .to_string(),
        started_at: wf.created_at,
        next_gate,
    })
}

/// The pinned pack id (else the kind) and the current step id of a workflow run.
fn pack_and_step(wf: &engine::WorkflowState) -> (String, String) {
    let pack = wf
        .definition
        .as_ref()
        .map_or_else(|| wf.kind.as_str().to_string(), |d| d.id.clone());
    (pack, wf.current().map(|s| s.id.clone()).unwrap_or_default())
}

/// `(workflow id, pack, step)` of the repo's active workflow, read as [`active_workflow_summary`]
/// reads it; `None` when there is none.
pub fn active_workflow_stamp(
    state: &crate::commands::ctx::state::StateDir,
    repo: &std::path::Path,
) -> Option<(String, String, String)> {
    let wf = engine::load_active(state, repo).ok().flatten()?;
    let (pack, step) = pack_and_step(&wf);
    Some((wf.id.clone(), pack, step))
}

/// Reserve the full command surface so repository scripts cannot claim a name between releases.
pub const TOP_LEVEL_COMMANDS: &[&str] = &[
    "skill", "workflow", "test", "verify", "artifact", "frontend",
];

/// Operator gates over repository-provided workflow input. Each is resolved
/// from operator-controlled config and fails closed when that config cannot be
/// trusted.
pub(crate) struct RepoGates {
    pub checks: bool,
    pub skills: bool,
    pub agents: bool,
    /// Repository packs are off until the operator enables them; they cannot replace trusted ids or widen authority. (#542)
    pub workflows: bool,
    /// Operator-owned `[workflow] check_env_passthrough` (REPO_FORBIDDEN,
    /// `~/.zirv/ctx.toml`/`ZIRV_CTX_*` only) -- extra environment variable
    /// names ADDED to `verification::DEFAULT_CHECK_ENV_PASSTHROUGH` when a
    /// check child is spawned. Empty (never widened) when the config could
    /// not even be read, same fail-closed posture as `checks`/`skills`/
    /// `agents` above.
    pub check_env_passthrough: Vec<String>,
    /// Only operator configuration may allow an empty verify pass; unreadable config defaults to inconclusive. (#268)
    pub allow_empty_verify: bool,
    /// Only operator configuration may exclude built-in checks; unreadable config runs them all. (#276)
    pub builtin_checks_exclude: Vec<String>,
}

/// Resolves both gates, failing **closed** when the configuration cannot be
/// read.
///
/// This is the one answer both gates need, and it has to be the same answer.
/// Reading the config per-gate produced two different wrong behaviors on an
/// unparseable `.zirv/ctx.toml` -- which a repository checkout controls: the
/// skill gate defaulted to *enabled* (so a malformed repo config was a way to
/// force the untrusted skill layer back on) while verification hard-errored
/// (so the same file bricked `zirv test`/`zirv verify` in that checkout).
/// Neither is acceptable, and they disagree: an unreadable config means the
/// operator's intent is unknown, so the security decision goes to "no
/// repository-provided input" while everything zirv owns itself -- built-in
/// skills, discovered toolchain checks -- keeps working.
pub(crate) fn repo_gates(repo: &std::path::Path) -> RepoGates {
    match crate::commands::ctx::config::CtxConfig::load(repo, &|key| std::env::var(key).ok()) {
        Ok(cfg) => RepoGates {
            checks: cfg.workflow.repo_checks_enabled,
            skills: cfg.workflow.repo_skills_enabled,
            agents: cfg.workflow.repo_agents_enabled,
            workflows: cfg.workflow.repo_workflows_enabled,
            check_env_passthrough: cfg.workflow.check_env_passthrough,
            allow_empty_verify: cfg.workflow.allow_empty_verify,
            builtin_checks_exclude: cfg.workflow.builtin_checks_exclude,
        },
        Err(error) => {
            announce_unreadable_config(&error.to_string());
            RepoGates {
                checks: false,
                skills: false,
                agents: false,
                workflows: false,
                check_env_passthrough: Vec::new(),
                allow_empty_verify: false,
                builtin_checks_exclude: Vec::new(),
            }
        }
    }
}

/// The degradation notice for a configuration that would not load. `chrome.
/// events` is the switch this channel normally reads, and it lives in the very
/// file that just failed to parse, so the operator's own `--quiet`/
/// `ZIRV_CTX_QUIET` is consulted directly instead of assuming silence.
fn announce_unreadable_config(reason: &str) {
    let quiet = std::env::var("ZIRV_CTX_QUIET")
        .map(|value| matches!(value.trim(), "true" | "1"))
        .unwrap_or(false);
    crate::commands::ctx::announce::Announcer::new(!quiet, false).emit(
        &crate::commands::ctx::announce::Event::WorkflowGatesClosed {
            reason: reason.to_string(),
        },
    );
}

pub(crate) use crate::commands::ctx::supervise::isolate_process_tree;

/// Terminate a child and every process it spawned, then reap the direct child.
pub(crate) fn terminate_process_tree(child: &mut Child) -> CtxResult<()> {
    crate::commands::ctx::supervise::terminate_group(child, Duration::from_secs(5))
}

#[derive(Debug, Parser)]
#[command(name = "zirv", disable_help_subcommand = true)]
struct WorkflowCli {
    #[command(subcommand)]
    command: WorkflowCommand,
}

/// Expose this command tree to shared help discovery without exposing the parsed CLI value. (#355)
pub(crate) fn command() -> clap::Command {
    use clap::CommandFactory;
    WorkflowCli::command()
}

#[derive(Debug, Subcommand)]
enum WorkflowCommand {
    /// Inspect model-agnostic engineering skills.
    Skill(skill::SkillArgs),
    /// Run and inspect durable development workflows.
    Workflow(engine::WorkflowArgs),
    /// Run repository-aware checks.
    Test(verification::TestArgs),
    /// Run final verification for the current change set.
    Verify(verification::VerifyArgs),
    /// Register and present workflow artifacts.
    Artifact(artifact::ArtifactArgs),
    /// Infer and inspect autonomous frontend quality state.
    Frontend(frontend::FrontendArgs),
}

fn run(cli: &WorkflowCli, writer: &mut impl std::io::Write) -> CtxResult<i32> {
    match &cli.command {
        WorkflowCommand::Skill(args) => skill::run(args, writer),
        WorkflowCommand::Workflow(args) => engine::run(args, writer),
        WorkflowCommand::Test(args) => verification::run_test(args, writer),
        WorkflowCommand::Verify(args) => verification::run_verify(args, writer),
        WorkflowCommand::Artifact(args) => artifact::run(args, writer),
        WorkflowCommand::Frontend(args) => frontend::run(args, writer),
    }
}

fn normalized_args(args: &[String]) -> Vec<String> {
    let mut args = args.to_vec();
    if let Some(command) = args.get_mut(1)
        && TOP_LEVEL_COMMANDS
            .iter()
            .any(|candidate| command.eq_ignore_ascii_case(candidate))
    {
        command.make_ascii_lowercase();
    }
    args
}

pub fn dispatch(args: &[String]) -> i32 {
    let cli = match WorkflowCli::try_parse_from(normalized_args(args)) {
        Ok(cli) => cli,
        Err(err) => {
            let code = if matches!(
                err.kind(),
                clap::error::ErrorKind::DisplayHelp | clap::error::ErrorKind::DisplayVersion
            ) {
                0
            } else {
                2
            };
            let _ = err.print();
            return code;
        }
    };

    match run(&cli, &mut std::io::stdout()) {
        Ok(code) => code,
        Err(err) => {
            // Preserve clap’s exit code 2 for unknown start/show workflow ids; other unknown ids remain runtime errors with exit code 1. (#542)
            let is_registry_id_lookup = matches!(
                &cli.command,
                WorkflowCommand::Workflow(args)
                    if matches!(
                        args.command,
                        engine::WorkflowSubcommand::Start(_) | engine::WorkflowSubcommand::Show(_)
                    )
            );
            let text = err.to_string();
            crate::output::error(err);
            if is_registry_id_lookup && text.starts_with("unknown workflow '") {
                2
            } else {
                1
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn skill_command_parses_in_its_own_tree() {
        let cli = WorkflowCli::try_parse_from(["zirv", "skill", "list"])
            .expect("skill list should parse");
        assert!(matches!(cli.command, WorkflowCommand::Skill(_)));
    }

    #[test]
    fn top_level_workflow_command_is_case_insensitive() {
        let args = ["zirv", "SKILL", "list"]
            .into_iter()
            .map(str::to_string)
            .collect::<Vec<_>>();
        let cli = WorkflowCli::try_parse_from(normalized_args(&args))
            .expect("uppercase reserved workflow command should parse");
        assert!(matches!(cli.command, WorkflowCommand::Skill(_)));
    }

    #[test]
    fn frontend_profile_command_parses_without_an_init_verb() {
        let cli = WorkflowCli::try_parse_from(["zirv", "frontend", "profile"])
            .expect("frontend profile should parse");
        assert!(matches!(cli.command, WorkflowCommand::Frontend(_)));
    }

    /// Issue #542 review finding 17: `StartArgs.id`/`ShowArgs.id` used to be
    /// a closed `WorkflowKind` `ValueEnum`, so an unrecognized value failed
    /// AT CLAP PARSE TIME (`dispatch`'s own `Err` branch, exit 2) before
    /// #542 ever changed it to a plain registry id string. Losing that exit
    /// code for the exact same "you typed an id that does not exist"
    /// condition would be a user-facing regression a script relying on
    /// `$? == 2` for a usage error would silently stop seeing -- `dispatch`
    /// now restores it specifically for `start`/`show`'s own registry
    /// lookup failure, at the full CLI entry point (not just `run`'s
    /// return value), so this is the exact path a real invocation takes.
    #[test]
    fn an_unknown_registry_id_exits_2_like_the_old_closed_enum_did() {
        let repo = tempfile::tempdir().unwrap();

        let start_args = [
            "zirv",
            "workflow",
            "start",
            "totally-unknown-workflow-id",
            "--task",
            "do something",
            "--repo",
            repo.path().to_str().unwrap(),
            "--changed-lines",
            "5",
        ]
        .into_iter()
        .map(str::to_string)
        .collect::<Vec<_>>();
        assert_eq!(
            dispatch(&start_args),
            2,
            "an unknown `workflow start` id must exit 2, matching the old closed-enum behavior"
        );

        let show_args = [
            "zirv",
            "workflow",
            "show",
            "totally-unknown-workflow-id",
            "--repo",
            repo.path().to_str().unwrap(),
        ]
        .into_iter()
        .map(str::to_string)
        .collect::<Vec<_>>();
        assert_eq!(
            dispatch(&show_args),
            2,
            "an unknown `workflow show` id must exit 2, matching the old closed-enum behavior"
        );
    }

    // Issue #209/v3 §D: `active_workflow_summary`, the dashboard footer's
    // own read of the same active-workflow state `zirv workflow status`
    // resolves.

    fn test_classification() -> classify::Classification {
        classify::Classification {
            intent: classify::Intent::Feature,
            complexity: classify::Complexity::Bounded,
            risk: classify::RiskBand::Low,
            risk_score: 0,
            changed_files: 1,
            changed_lines: 10,
            changed_paths: Vec::new(),
            declared_scope: true,
            work_domain: classify::DomainClassification::default(),
            risk_measurement: classify::RiskMeasurement::default(),
            reasons: Vec::new(),
        }
    }

    #[test]
    fn active_workflow_summary_is_none_with_nothing_active() {
        let root = tempfile::tempdir().unwrap();
        let repo = tempfile::tempdir().unwrap();
        let state = crate::commands::ctx::state::StateDir::from_root(root.path().to_path_buf());
        assert_eq!(active_workflow_summary(&state, repo.path()), None);
    }

    #[test]
    fn active_workflow_summary_carries_the_kind_and_current_step() {
        let root = tempfile::tempdir().unwrap();
        let repo = tempfile::tempdir().unwrap();
        let state_dir = crate::commands::ctx::state::StateDir::from_root(root.path().to_path_buf());
        let wf = engine::WorkflowState::start(
            repo.path().to_path_buf(),
            "small feature".into(),
            engine::WorkflowKind::Feature,
            None,
            true,
            test_classification(),
        );
        engine::save(&state_dir, &wf, true).expect("save active workflow");

        let summary = active_workflow_summary(&state_dir, repo.path())
            .expect("an active workflow was just saved");
        assert_eq!(summary.kind, "feature");
        assert_eq!(summary.step, wf.current().unwrap().id);
    }

    #[test]
    fn active_workflow_stamp_names_the_run_its_pack_and_its_current_step() {
        let root = tempfile::tempdir().unwrap();
        let repo = tempfile::tempdir().unwrap();
        let state_dir = crate::commands::ctx::state::StateDir::from_root(root.path().to_path_buf());
        assert_eq!(active_workflow_stamp(&state_dir, repo.path()), None);
        let wf = engine::WorkflowState::start(
            repo.path().to_path_buf(),
            "small feature".into(),
            engine::WorkflowKind::Feature,
            None,
            true,
            test_classification(),
        );
        engine::save(&state_dir, &wf, true).expect("save active workflow");

        let (id, pack, step) = active_workflow_stamp(&state_dir, repo.path()).expect("active");
        let summary = active_workflow_summary(&state_dir, repo.path()).expect("active");
        assert_eq!(
            (id, pack, step),
            (wf.id.clone(), summary.pack, summary.step)
        );
    }

    #[test]
    fn active_workflow_summary_lists_every_step_with_how_far_the_run_is() {
        let root = tempfile::tempdir().unwrap();
        let repo = tempfile::tempdir().unwrap();
        let state_dir = crate::commands::ctx::state::StateDir::from_root(root.path().to_path_buf());
        let mut wf = engine::WorkflowState::start(
            repo.path().to_path_buf(),
            "small feature".into(),
            engine::WorkflowKind::Feature,
            None,
            true,
            test_classification(),
        );
        let first = wf.steps[0].id.clone();
        wf.completed_steps.push(first.clone());
        wf.current_step = 1;
        engine::save(&state_dir, &wf, true).expect("save active workflow");

        let summary = active_workflow_summary(&state_dir, repo.path()).expect("active");
        assert_eq!(summary.steps.len(), wf.steps.len());
        assert_eq!(summary.steps[0], (first, StepMark::Done));
        assert_eq!(
            summary.steps[1],
            (wf.steps[1].id.clone(), StepMark::Current)
        );
        assert!(
            summary.steps[2..]
                .iter()
                .all(|(_, mark)| *mark == StepMark::Pending),
            "{:?}",
            summary.steps
        );
        assert!(!summary.pack.is_empty(), "a pack id, or the kind");
    }

    #[test]
    fn active_workflow_summary_reports_awaiting_approval() {
        let root = tempfile::tempdir().unwrap();
        let repo = tempfile::tempdir().unwrap();
        let state_dir = crate::commands::ctx::state::StateDir::from_root(root.path().to_path_buf());
        let mut wf = engine::WorkflowState::start(
            repo.path().to_path_buf(),
            "small feature".into(),
            engine::WorkflowKind::Feature,
            None,
            true,
            test_classification(),
        );
        wf.status = engine::WorkflowStatus::AwaitingApproval;
        engine::save(&state_dir, &wf, true).expect("save active workflow");

        let summary = active_workflow_summary(&state_dir, repo.path()).expect("saved active");
        assert!(summary.awaiting_approval);
    }

    #[test]
    fn active_workflow_summary_is_none_for_a_different_repo() {
        let root = tempfile::tempdir().unwrap();
        let repo = tempfile::tempdir().unwrap();
        let other_repo = tempfile::tempdir().unwrap();
        let state_dir = crate::commands::ctx::state::StateDir::from_root(root.path().to_path_buf());
        let wf = engine::WorkflowState::start(
            repo.path().to_path_buf(),
            "small feature".into(),
            engine::WorkflowKind::Feature,
            None,
            true,
            test_classification(),
        );
        engine::save(&state_dir, &wf, true).expect("save active workflow");

        assert_eq!(active_workflow_summary(&state_dir, other_repo.path()), None);
    }

    /// Issue #242 follow-up: `engine::auto_spawn_decision`'s argv is built by
    /// hand, so a renamed flag (`--repo`, `--agent`, the review-run
    /// positional id) would fail only at runtime, silently, unless something
    /// feeds it through the real parser. This does: every argv the pure
    /// decision produces for Review/Test/Verify must parse through the same
    /// `WorkflowCli` `main.rs` itself dispatches into, with the fields
    /// landing where the decision meant them to.
    #[test]
    fn auto_spawn_argv_parses_through_the_real_top_level_cli() {
        let repo = tempfile::tempdir().unwrap();
        let mut classification = test_classification();
        classification.complexity = classify::Complexity::Substantial;
        classification.risk = classify::RiskBand::High;
        let mut state = engine::WorkflowState::start(
            repo.path().to_path_buf(),
            "ship it".into(),
            engine::WorkflowKind::Feature,
            Some("claude".to_string()),
            true,
            classification,
        );
        state.status = engine::WorkflowStatus::Running;

        for phase in [
            skill::WorkflowPhase::Review,
            skill::WorkflowPhase::Test,
            skill::WorkflowPhase::Verify,
        ] {
            state.current_step = state
                .steps
                .iter()
                .position(|step| step.phase == phase)
                .unwrap_or_else(|| panic!("{phase:?} step must be present in this workflow"));
            let spawn = engine::auto_spawn_decision(&state, true, true, None)
                .unwrap_or_else(|skip| panic!("{phase:?} must be eligible to fire: {skip:?}"));

            let mut argv = vec!["zirv".to_string()];
            argv.extend(spawn.argv.clone());
            let cli = WorkflowCli::try_parse_from(&argv)
                .unwrap_or_else(|err| panic!("auto-spawn argv {argv:?} must parse: {err}"));

            match phase {
                skill::WorkflowPhase::Review => {
                    let WorkflowCommand::Workflow(wf) = &cli.command else {
                        panic!("expected a `workflow` subcommand, got {:?}", cli.command)
                    };
                    let engine::WorkflowSubcommand::Review(review_args) = &wf.command else {
                        panic!("expected `workflow review`, got {:?}", wf.command)
                    };
                    let review::ReviewCommand::Run(run_args) = &review_args.command else {
                        panic!("expected `review run`, got {:?}", review_args.command)
                    };
                    assert_eq!(run_args.id, state.id);
                    assert_eq!(run_args.agent, "claude");
                    assert_eq!(run_args.repo.as_deref(), Some(state.repo.as_path()));
                }
                skill::WorkflowPhase::Test => {
                    let WorkflowCommand::Test(test_args) = &cli.command else {
                        panic!("expected a `test` subcommand, got {:?}", cli.command)
                    };
                    let verification::TestCommand::Changed(run_args) = &test_args.command else {
                        panic!("expected `test changed`, got {:?}", test_args.command)
                    };
                    assert_eq!(run_args.repo.as_deref(), Some(state.repo.as_path()));
                }
                skill::WorkflowPhase::Verify => {
                    let WorkflowCommand::Verify(verify_args) = &cli.command else {
                        panic!("expected a `verify` subcommand, got {:?}", cli.command)
                    };
                    assert_eq!(verify_args.run.repo.as_deref(), Some(state.repo.as_path()));
                }
                _ => unreachable!("only the three eligible phases are iterated"),
            }
        }
    }
}
