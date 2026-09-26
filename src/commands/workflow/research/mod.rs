//! `zirv workflow research plan|run|status|report` (issue #802): a bounded,
//! resumable autoresearch campaign runner. See
//! `docs/superpowers/specs/2026-09-26-autoresearch-design.md` for the full
//! design; this module implements its "#802 Runner" section.

pub mod backend;
pub mod budget;
pub mod corpus;
pub mod guard;
pub mod ledger;
pub mod manifest;
pub mod plan;
pub mod promote;
pub mod report;
pub mod run;
pub mod stats;
pub mod status;

use std::io::Write;
use std::path::{Path, PathBuf};

use clap::{Args, Subcommand};

use crate::commands::ctx::CtxResult;
use crate::commands::ctx::state::StateDir;

#[derive(Debug, Args)]
pub struct ResearchArgs {
    #[command(subcommand)]
    pub command: ResearchCommand,
}

#[derive(Debug, Subcommand)]
pub enum ResearchCommand {
    /// Validate and resolve a campaign manifest with zero backend/provider
    /// calls: baseline sha, corpus splits, candidate scope, evaluator
    /// fingerprint, and a schedule preview against its own budgets.
    Plan(PlanArgs),
    /// Run (or resume) a bounded campaign: baseline, then screen -> validate
    /// -> holdout per candidate, in budget.
    Run(RunArgs),
    /// Show a campaign's stage, trial counts, and spend used vs caps.
    Status(CampaignRefArgs),
    /// Regenerate a campaign's report/results/proposal outputs from its
    /// ledger.
    Report(CampaignRefArgs),
}

#[derive(Debug, Args)]
pub struct PlanArgs {
    pub manifest: PathBuf,
    #[arg(long)]
    pub repo: Option<PathBuf>,
    #[arg(long)]
    pub dir: Option<PathBuf>,
    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, Args)]
pub struct RunArgs {
    pub manifest: PathBuf,
    #[arg(long)]
    pub repo: Option<PathBuf>,
    #[arg(long)]
    pub dir: Option<PathBuf>,
    #[arg(long)]
    pub resume: bool,
    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, Args)]
pub struct CampaignRefArgs {
    /// Either a campaign id (resolved under `<ctx state dir>/research/<id>/`)
    /// or an explicit campaign directory.
    pub id_or_dir: String,
    #[arg(long)]
    pub json: bool,
}

fn resolve_campaign_dir(id_or_dir: &str, state_dir: &StateDir) -> PathBuf {
    let candidate = Path::new(id_or_dir);
    if candidate.is_dir() {
        candidate.to_path_buf()
    } else {
        state_dir.root().join("research").join(id_or_dir)
    }
}

pub fn run(args: &ResearchArgs, writer: &mut impl Write) -> CtxResult<i32> {
    match &args.command {
        ResearchCommand::Plan(args) => {
            let repo = super::engine::resolve_repo(args.repo.as_deref())?;
            let report = plan::plan(&args.manifest, &repo)?;
            if args.json {
                serde_json::to_writer_pretty(&mut *writer, &report)?;
                writeln!(writer)?;
            } else {
                writeln!(writer, "campaign: {}", report.id)?;
                writeln!(writer, "baseline: {}", report.baseline_sha)?;
                writeln!(
                    writer,
                    "corpus: dev={} validation={} holdout={}",
                    report.splits.dev, report.splits.validation, report.splits.holdout
                )?;
                writeln!(
                    writer,
                    "worst case: {} trials, ${:.2}, {} calls, {}s wall, fits_budgets={}",
                    report.schedule.worst_case_trials,
                    report.schedule.worst_case_spend_usd,
                    report.schedule.worst_case_calls,
                    report.schedule.worst_case_wall_secs,
                    report.schedule.fits_budgets
                )?;
                for note in &report.coverage {
                    writeln!(writer, "coverage: {note}")?;
                }
                for candidate in &report.candidates {
                    writeln!(
                        writer,
                        "candidate {}: env_keys={} patch={}",
                        candidate.id, candidate.env_keys, candidate.has_patch
                    )?;
                    for problem in &candidate.problems {
                        writeln!(writer, "  problem: {problem}")?;
                    }
                }
                if !report.valid {
                    for error in &report.errors {
                        writeln!(writer, "error: {error}")?;
                    }
                }
                writeln!(writer, "valid: {}", report.valid)?;
            }
            Ok(if report.valid { 0 } else { 2 })
        }
        ResearchCommand::Run(args) => {
            let repo = super::engine::resolve_repo(args.repo.as_deref())?;
            let state_dir = super::engine::resolve_state()?;
            let summary = run::execute(
                &args.manifest,
                &repo,
                args.dir.as_deref(),
                args.resume,
                &state_dir,
            )?;
            if args.json {
                serde_json::to_writer_pretty(
                    &mut *writer,
                    &serde_json::json!({
                        "campaign_dir": summary.campaign_dir,
                        "verdict": summary.verdict,
                        "promoted": summary.promoted,
                        "stopped_reason": summary.stopped_reason,
                    }),
                )?;
                writeln!(writer)?;
            } else {
                writeln!(writer, "campaign dir: {}", summary.campaign_dir.display())?;
                writeln!(writer, "verdict: {:?}", summary.verdict)?;
                match &summary.promoted {
                    Some(id) => writeln!(writer, "promoted: {id}")?,
                    None => writeln!(writer, "promoted: (none)")?,
                }
                if let Some(reason) = &summary.stopped_reason {
                    writeln!(writer, "stopped: {reason}")?;
                }
            }
            Ok(if summary.stopped_reason.is_some() {
                1
            } else {
                0
            })
        }
        ResearchCommand::Status(args) => {
            let state_dir = super::engine::resolve_state()?;
            let campaign_dir = resolve_campaign_dir(&args.id_or_dir, &state_dir);
            let report = status::status(&campaign_dir)?;
            if args.json {
                serde_json::to_writer_pretty(&mut *writer, &report)?;
                writeln!(writer)?;
            } else {
                writeln!(writer, "campaign: {}", report.campaign_id)?;
                writeln!(
                    writer,
                    "trials: finished={} failed={} in_flight_or_orphaned={}",
                    report.trials_finished,
                    report.trials_failed_permanently,
                    report.trials_in_flight_or_orphaned
                )?;
                writeln!(
                    writer,
                    "spend: ${:.2} / ${:.2}",
                    report.spend_used_usd, report.spend_cap_usd
                )?;
                writeln!(
                    writer,
                    "calls: {} / {}",
                    report.calls_used, report.calls_cap
                )?;
                for (candidate, stage, count) in &report.stage_counts {
                    writeln!(writer, "stage: {candidate} {stage} -> {count} trial(s)")?;
                }
                for (candidate, stage, verdict) in &report.last_stage_decisions {
                    writeln!(writer, "decision: {candidate} {stage} -> {verdict}")?;
                }
                if let Some(reason) = &report.stopped_reason {
                    writeln!(writer, "stopped: {reason}")?;
                }
                writeln!(writer, "finished: {}", report.finished)?;
            }
            Ok(0)
        }
        ResearchCommand::Report(args) => {
            let state_dir = super::engine::resolve_state()?;
            let campaign_dir = resolve_campaign_dir(&args.id_or_dir, &state_dir);
            let summary = report::generate(&campaign_dir)?;
            if args.json {
                serde_json::to_writer_pretty(&mut *writer, &summary)?;
                writeln!(writer)?;
            } else {
                writeln!(writer, "report written to {}", campaign_dir.display())?;
                writeln!(writer, "verdict: {}", summary.verdict)?;
                match &summary.promoted {
                    Some(id) => writeln!(writer, "promoted: {id}")?,
                    None => writeln!(writer, "promoted: (none)")?,
                }
            }
            Ok(0)
        }
    }
}
