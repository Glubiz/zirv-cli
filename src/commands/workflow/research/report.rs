//! Regenerates `report.md`, `report.json`, `results.tsv`, and (for a
//! promoted candidate) `proposal/{overlay.toml|candidate.patch,ROLLBACK.md}`
//! purely from `lock.json` + `ledger.jsonl` -- `run` calls this once at the
//! end of a campaign, and `zirv workflow research report` calls it again any
//! time, independent of whether the process that ran the campaign is still
//! alive. `Lock` carries everything a repo-independent regeneration needs
//! (the manifest path/repo it ran against, the corpus version/families) so
//! this module never re-reads the corpus or the manifest file itself.

use std::collections::BTreeMap;
use std::io::Write as _;
use std::path::Path;

use serde::Serialize;

use super::ledger::{self, Ledger, LedgerEvent, Lock};
use super::manifest::Candidate;
use super::promote::CohortDecision;
use super::reconcile::reconstruct_tracker;
use super::run::{Stage, stage_records_from_ledger};
use crate::commands::ctx::CtxResult;
use crate::commands::ctx::state::display_path;

/// One candidate/stage's full result, parsed back from that `StageDecision`
/// event's own `detail` -- `screen`'s detail is `{reason, points,
/// excluded_by_reason}` (point estimates only, no cohorts, no bootstrap);
/// `validate`/`holdout`'s detail is a `promote::Decision` (verdict/reasons/
/// cohorts/confidence) plus `excluded_by_reason` and `seed` merged in
/// alongside it.
#[derive(Debug, Clone, Serialize)]
pub struct CandidateReportRow {
    pub candidate: String,
    pub stage: String,
    pub verdict: String,
    pub hypothesis: String,
    /// Empty when unusable (e.g. no pairs at all); one entry per cohort
    /// otherwise -- cohorts are never pooled into one number.
    pub d_correctness: Vec<f64>,
    /// Same shape as `d_correctness`; always present (not only under
    /// `[criteria] objective = "quality"`) since a campaign's own objective
    /// can differ from what a reader is judging it by.
    pub d_quality: Vec<f64>,
    pub rel_cost: Vec<f64>,
    pub rel_wall: Vec<f64>,
    pub reasons: Vec<String>,
    /// The confidence level actually applied (Bonferroni-adjusted at
    /// validate, the manifest's own `criteria.confidence` at holdout);
    /// `None` for screen, which never bootstraps.
    pub confidence: Option<f64>,
    /// The bootstrap seed actually used; `None` for screen.
    pub seed: Option<u64>,
    pub excluded_by_reason: BTreeMap<String, usize>,
    /// Full per-cohort arm summaries and delta intervals, for `report.md`'s
    /// detailed breakdown; empty for screen (which has no cohort concept of
    /// its own -- see `promote::screen`'s own doc comment).
    pub cohorts: Vec<CohortDecision>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ReportSummary {
    pub campaign_id: String,
    pub zirv_version: String,
    pub baseline_sha: String,
    pub started_at: u64,
    pub finished_at: Option<u64>,
    pub promoted: Option<String>,
    pub verdict: String,
    /// The campaign's `[criteria] objective` that decided `verdict`
    /// -- `"efficiency"` (cost/wall win) or `"quality"` (answer-stability
    /// win).
    pub objective: String,
    pub rows: Vec<CandidateReportRow>,
    pub stopped_reason: Option<String>,
    pub spend: SpendSummary,
}

/// Execution spend (what a trial's own arm cost) kept strictly separate
/// from experiment overhead (judges, a proposer's own calls -- never
/// counted toward a candidate's own cost axis, but still real campaign
/// spend). `completeness` is `"complete"` only when every finished trial's
/// own cost was fully known; a crash/timeout charged at its declared
/// ceiling, or any trial reporting `cost_complete = false`, makes it
/// `"partial"`; `"unknown"` when no trial finished at all.
#[derive(Debug, Clone, Serialize)]
pub struct SpendSummary {
    pub execution_usd: f64,
    pub overhead_usd: f64,
    pub completeness: String,
}

fn spend_summary(events: &[LedgerEvent]) -> SpendSummary {
    let mut execution_usd = 0.0;
    let mut overhead_usd = 0.0;
    let mut saw_a_trial = false;
    let mut all_complete = true;

    for event in events {
        match event {
            LedgerEvent::TrialFinished {
                cost_usd,
                cost_complete,
                overhead_usd: overhead,
                ..
            } => {
                saw_a_trial = true;
                execution_usd += cost_usd.unwrap_or(0.0);
                overhead_usd += overhead;
                if !cost_complete {
                    all_complete = false;
                }
            }
            LedgerEvent::TrialFailed { charged_usd, .. } => {
                // A crash/timeout's actual spend is unknown; `charged_usd`
                // is the declared ceiling substituted for budget accounting,
                // not a known real cost, so it also marks the total partial.
                saw_a_trial = true;
                execution_usd += charged_usd;
                all_complete = false;
            }
            _ => {}
        }
    }

    let completeness = if !saw_a_trial {
        "unknown"
    } else if all_complete {
        "complete"
    } else {
        "partial"
    };

    SpendSummary {
        execution_usd,
        overhead_usd,
        completeness: completeness.to_string(),
    }
}

fn campaign_verdict(events: &[LedgerEvent]) -> (Option<String>, String, Option<String>) {
    for event in events.iter().rev() {
        match event {
            LedgerEvent::CampaignFinished { promoted, .. } => {
                let verdict = if promoted.is_some() {
                    "accept".to_string()
                } else {
                    "no_improvement".to_string()
                };
                return (promoted.clone(), verdict, None);
            }
            LedgerEvent::CampaignStopped { reason, .. } => {
                return (None, "stopped".to_string(), Some(reason.clone()));
            }
            _ => {}
        }
    }
    (None, "unmeasured".to_string(), None)
}

/// The `ts` of the campaign's own closing event (`campaign_finished` or
/// `campaign_stopped`), or `None` while it is still in progress.
fn finished_at(events: &[LedgerEvent]) -> Option<u64> {
    events.iter().rev().find_map(|event| match event {
        LedgerEvent::CampaignFinished { ts, .. } | LedgerEvent::CampaignStopped { ts, .. } => {
            Some(*ts)
        }
        _ => None,
    })
}

/// The latest `ts` seen anywhere in the ledger, or `started_at` when the
/// ledger is empty -- used for "wall used" even on an in-progress campaign
/// that has not (yet) written a closing event.
fn last_event_ts(events: &[LedgerEvent], started_at: u64) -> u64 {
    let last = match events {
        [.., last] => match last {
            LedgerEvent::CampaignStarted { ts, .. }
            | LedgerEvent::TrialScheduled { ts, .. }
            | LedgerEvent::TrialFinished { ts, .. }
            | LedgerEvent::TrialFailed { ts, .. }
            | LedgerEvent::CandidateProposed { ts, .. }
            | LedgerEvent::CandidateRejected { ts, .. }
            | LedgerEvent::StageDecision { ts, .. }
            | LedgerEvent::HoldoutUsed { ts, .. }
            | LedgerEvent::CampaignStopped { ts, .. }
            | LedgerEvent::CampaignFinished { ts, .. } => *ts,
        },
        [] => started_at,
    };
    last.max(started_at)
}

fn parse_screen_detail(
    candidate: &Candidate,
    verdict: &str,
    detail: &serde_json::Value,
) -> CandidateReportRow {
    let reason = detail
        .get("reason")
        .and_then(|v| v.as_str())
        .map(str::to_string);
    let points = detail.get("points");
    let metric = |key: &str| -> Vec<f64> {
        points
            .and_then(|p| p.get(key))
            .and_then(|v| v.as_f64())
            .into_iter()
            .collect()
    };
    let excluded_by_reason = detail
        .get("excluded_by_reason")
        .and_then(|v| serde_json::from_value(v.clone()).ok())
        .unwrap_or_default();

    CandidateReportRow {
        candidate: candidate.id.clone(),
        stage: "screen".to_string(),
        verdict: verdict.to_string(),
        hypothesis: candidate.hypothesis.clone(),
        d_correctness: metric("d_correctness"),
        d_quality: metric("d_quality"),
        rel_cost: metric("rel_cost"),
        rel_wall: metric("rel_wall"),
        reasons: reason.into_iter().collect(),
        confidence: None,
        seed: None,
        excluded_by_reason,
        cohorts: Vec::new(),
    }
}

fn parse_decision_detail(
    candidate: &Candidate,
    stage: &str,
    verdict: &str,
    detail: &serde_json::Value,
) -> Option<CandidateReportRow> {
    let decision: super::promote::Decision = serde_json::from_value(detail.clone()).ok()?;
    let seed = detail.get("seed").and_then(|v| v.as_u64());
    let excluded_by_reason = detail
        .get("excluded_by_reason")
        .and_then(|v| serde_json::from_value(v.clone()).ok())
        .unwrap_or_default();
    let d_correctness = decision
        .cohorts
        .iter()
        .filter_map(|c| c.d_correctness.as_ref())
        .map(|iv| iv.point)
        .collect();
    let d_quality = decision
        .cohorts
        .iter()
        .filter_map(|c| c.d_quality.as_ref())
        .map(|iv| iv.point)
        .collect();
    let rel_cost = decision
        .cohorts
        .iter()
        .filter_map(|c| c.rel_cost.as_ref())
        .map(|iv| iv.point)
        .collect();
    let rel_wall = decision
        .cohorts
        .iter()
        .filter_map(|c| c.rel_wall.as_ref())
        .map(|iv| iv.point)
        .collect();

    Some(CandidateReportRow {
        candidate: candidate.id.clone(),
        stage: stage.to_string(),
        verdict: verdict.to_string(),
        hypothesis: candidate.hypothesis.clone(),
        d_correctness,
        d_quality,
        rel_cost,
        rel_wall,
        reasons: decision.reasons.clone(),
        confidence: Some(decision.confidence),
        seed,
        excluded_by_reason,
        cohorts: decision.cohorts,
    })
}

fn candidate_rows(events: &[LedgerEvent], candidates: &[Candidate]) -> Vec<CandidateReportRow> {
    let mut rows = Vec::new();
    for candidate in candidates {
        for stage in ["screen", "validate", "holdout"] {
            let found = events.iter().rev().find_map(|e| match e {
                LedgerEvent::StageDecision {
                    candidate: c,
                    stage: s,
                    verdict,
                    detail,
                    ..
                } if c == &candidate.id && s == stage => Some((verdict.as_str(), detail)),
                _ => None,
            });
            let Some((verdict, detail)) = found else {
                continue;
            };
            let row = if stage == "screen" {
                Some(parse_screen_detail(candidate, verdict, detail))
            } else {
                parse_decision_detail(candidate, stage, verdict, detail)
            };
            if let Some(row) = row {
                rows.push(row);
            }
        }
    }
    rows
}

/// One retry (an attempt beyond the first) dispatched, per candidate --
/// `trial_scheduled.attempt > 0` is the only place a retry shows up in the
/// ledger; `trial_failed.retryable` says a retry is OWED, not that one
/// actually ran.
fn retries_by_candidate(events: &[LedgerEvent]) -> BTreeMap<String, usize> {
    let mut counts = BTreeMap::new();
    for event in events {
        if let LedgerEvent::TrialScheduled {
            candidate, attempt, ..
        } = event
            && *attempt > 0
        {
            *counts.entry(candidate.clone()).or_insert(0) += 1;
        }
    }
    counts
}

fn format_metric(values: &[f64]) -> String {
    if values.is_empty() {
        "-".to_string()
    } else {
        values
            .iter()
            .map(|v| format!("{v:.4}"))
            .collect::<Vec<_>>()
            .join("/")
    }
}

fn write_results_tsv(path: &Path, rows: &[CandidateReportRow]) -> CtxResult<()> {
    let mut file = std::fs::File::create(path)?;
    writeln!(
        file,
        "candidate\tstage\tverdict\trel_cost\trel_wall\td_correctness\td_quality\thypothesis"
    )?;
    for row in rows {
        writeln!(
            file,
            "{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}",
            row.candidate,
            row.stage,
            row.verdict,
            format_metric(&row.rel_cost),
            format_metric(&row.rel_wall),
            format_metric(&row.d_correctness),
            format_metric(&row.d_quality),
            row.hypothesis
        )?;
    }
    Ok(())
}

/// `YYYY-MM-DD HH:MM:SS UTC` from a unix timestamp, using pure civil-
/// calendar arithmetic (Howard Hinnant's well-known `civil_from_days`) --
/// the same "no timezone-crate dependency just to print a clock" approach
/// `ctx::attention::utc_hhmm` already uses for a bare `HH:MM`, extended to a
/// full date since a report's provenance needs more than a clock face.
fn format_utc(ts: u64) -> String {
    let days = (ts / 86_400) as i64;
    let secs_of_day = ts % 86_400;
    let (year, month, day) = civil_from_days(days);
    format!(
        "{year:04}-{month:02}-{day:02} {:02}:{:02}:{:02} UTC",
        secs_of_day / 3600,
        (secs_of_day / 60) % 60,
        secs_of_day % 60
    )
}

fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = if m <= 2 { y + 1 } else { y };
    (y, m, d)
}

fn write_provenance(
    file: &mut std::fs::File,
    summary: &ReportSummary,
    lock: &Lock,
) -> CtxResult<()> {
    writeln!(file, "## Provenance")?;
    writeln!(file, "- zirv version: {}", summary.zirv_version)?;
    writeln!(file, "- manifest: `{}`", display_path(&lock.manifest_path))?;
    writeln!(file, "- manifest sha256: {}", lock.manifest_sha256)?;
    writeln!(file, "- repo: `{}`", display_path(&lock.repo))?;
    writeln!(file, "- baseline commit: {}", summary.baseline_sha)?;
    writeln!(file, "- corpus: `{}`", lock.manifest.corpus.file.display())?;
    writeln!(file, "- corpus version: {}", lock.corpus_version)?;
    writeln!(file, "- billing: `{:?}`", lock.manifest.billing)?;
    writeln!(
        file,
        "- route: harness `{}`, model `{}`",
        lock.manifest.route.harness, lock.manifest.route.model
    )?;
    writeln!(
        file,
        "- cache mode: `{}`",
        lock.manifest.cache_mode.as_str()
    )?;
    writeln!(
        file,
        "- pressure: `{}`",
        lock.manifest.cohort.pressure.as_str()
    )?;
    writeln!(file, "- stratify: `{:?}`", lock.manifest.stratify)?;
    writeln!(
        file,
        "- evaluator version: {}",
        lock.evaluator_version
            .clone()
            .unwrap_or_else(|| "(none declared)".to_string())
    )?;
    writeln!(
        file,
        "- evaluator fingerprint: {}",
        lock.evaluator_fingerprint
    )?;
    if let Some(price_as_of) = &lock.price_as_of {
        writeln!(file, "- price table as_of: {price_as_of}")?;
    }
    writeln!(
        file,
        "- started at: {} ({})",
        summary.started_at,
        format_utc(summary.started_at)
    )?;
    match summary.finished_at {
        Some(ts) => writeln!(file, "- finished at: {} ({})", ts, format_utc(ts))?,
        None => writeln!(file, "- finished at: (still in progress)")?,
    }
    writeln!(file)?;
    Ok(())
}

fn write_criteria(file: &mut std::fs::File, lock: &Lock) -> CtxResult<()> {
    let c = &lock.manifest.criteria;
    writeln!(file, "## Promotion criteria")?;
    writeln!(
        file,
        "Values actually used, from this manifest's own `[criteria]` table (never a hardcoded default once a manifest sets one):"
    )?;
    writeln!(file, "| key | value |")?;
    writeln!(file, "|---|---|")?;
    writeln!(file, "| min_pairs | {} |", c.min_pairs)?;
    writeln!(file, "| correctness_floor | {:.3} |", c.correctness_floor)?;
    writeln!(
        file,
        "| quality_floor | {} |",
        c.quality_floor
            .map(|v| format!("{v:.3}"))
            .unwrap_or_else(|| "(none)".to_string())
    )?;
    writeln!(
        file,
        "| max_correctness_regression | {:.3} |",
        c.max_correctness_regression
    )?;
    writeln!(
        file,
        "| max_quality_regression | {:.3} |",
        c.max_quality_regression
    )?;
    writeln!(file, "| objective | {} |", c.objective.as_str())?;
    writeln!(file, "| min_effect | {:.3} |", c.min_effect)?;
    writeln!(file, "| confidence (base) | {:.3} |", c.confidence)?;
    writeln!(file, "| bootstrap_resamples | {} |", c.bootstrap_resamples)?;
    writeln!(file)?;
    Ok(())
}

fn write_budgets(
    file: &mut std::fs::File,
    lock: &Lock,
    events: &[LedgerEvent],
    summary: &ReportSummary,
) -> CtxResult<()> {
    let budgets = lock.manifest.budgets;
    let tracker = reconstruct_tracker(lock.started_at, events);
    let wall_used = last_event_ts(events, lock.started_at).saturating_sub(lock.started_at);
    let max_attempt = events
        .iter()
        .filter_map(|e| match e {
            LedgerEvent::TrialScheduled { attempt, .. } => Some(*attempt),
            _ => None,
        })
        .max()
        .unwrap_or(0);
    let total_retries: usize = retries_by_candidate(events).values().sum();

    writeln!(file, "## Budgets")?;
    writeln!(file, "| cap | limit | used |")?;
    writeln!(file, "|---|---|---|")?;
    writeln!(
        file,
        "| spend (execution) | (part of total spend, below) | ${:.4} |",
        summary.spend.execution_usd
    )?;
    writeln!(
        file,
        "| spend (overhead) | (part of total spend, below) | ${:.4} |",
        summary.spend.overhead_usd
    )?;
    writeln!(
        file,
        "| spend (total) | ${:.2} | ${:.4} |",
        budgets.max_spend_usd,
        summary.spend.execution_usd + summary.spend.overhead_usd
    )?;
    writeln!(
        file,
        "| calls | {} | {} |",
        budgets.max_calls, tracker.calls_used
    )?;
    writeln!(
        file,
        "| trials | {} | {} |",
        budgets.max_trials, tracker.trials_dispatched
    )?;
    writeln!(
        file,
        "| retries (per-trial cap: {}) | max attempt seen: {} | {} retry dispatches total |",
        budgets.max_retries, max_attempt, total_retries
    )?;
    writeln!(
        file,
        "| wall | {}s | {}s |",
        budgets.max_wall_secs, wall_used
    )?;
    writeln!(file)?;
    if let Some(reason) = &summary.stopped_reason {
        writeln!(file, "**Stopped early:** `{reason}`")?;
        writeln!(file)?;
    }
    Ok(())
}

fn write_coverage(file: &mut std::fs::File, lock: &Lock) -> CtxResult<()> {
    writeln!(file, "## Coverage and limitations")?;
    writeln!(
        file,
        "- seat_mode = `{:?}`: single-seat results are not orchestration evidence.",
        lock.manifest.seat_mode
    )?;
    writeln!(file, "- runtime = `{:?}`.", lock.manifest.runtime)?;
    if matches!(lock.manifest.runtime, super::manifest::Runtime::Native) {
        writeln!(
            file,
            "- native runtime: unmeasured -- `zirv native` is release-gated (issue #802)."
        )?;
    }
    if matches!(
        lock.manifest.seat_mode,
        super::manifest::SeatMode::Orchestration
    ) {
        writeln!(
            file,
            "- orchestration: unmeasured -- no orchestration suite exists yet."
        )?;
    }
    if lock.corpus_families.len() == 1 {
        writeln!(
            file,
            "- single project family: `{}` -- this result does not generalize across project families.",
            lock.corpus_families[0]
        )?;
    }
    writeln!(file)?;
    Ok(())
}

fn write_reproduction(file: &mut std::fs::File, lock: &Lock, campaign_dir: &Path) -> CtxResult<()> {
    writeln!(file, "## Reproduction")?;
    writeln!(
        file,
        "1. Check out the exact baseline this campaign ran against:"
    )?;
    writeln!(file, "```")?;
    writeln!(
        file,
        "git -C \"{}\" checkout {}",
        display_path(&lock.repo),
        lock.baseline_sha
    )?;
    writeln!(file, "```")?;
    writeln!(
        file,
        "2. Run the same manifest, resuming this campaign directory if it stopped early (a fresh run without `--resume` starts a new campaign instead):"
    )?;
    writeln!(file, "```")?;
    writeln!(
        file,
        "zirv workflow research run \"{}\" --repo \"{}\" --dir \"{}\" --resume",
        display_path(&lock.manifest_path),
        display_path(&lock.repo),
        display_path(campaign_dir)
    )?;
    writeln!(file, "```")?;
    Ok(())
}

fn plural(reason: &str) -> &'static str {
    match reason {
        "untriggered" => " (a required receipt never fired)",
        "env_mismatch" => {
            " (the candidate's env fingerprint did not match the paired baseline trial)"
        }
        _ => "",
    }
}

fn write_candidates(
    file: &mut std::fs::File,
    summary: &ReportSummary,
    lock: &Lock,
    retries: &BTreeMap<String, usize>,
) -> CtxResult<()> {
    writeln!(file, "## Candidates")?;
    writeln!(
        file,
        "| candidate | stage | verdict | rel_cost | rel_wall | d_correctness | d_quality |"
    )?;
    writeln!(file, "|---|---|---|---|---|---|---|")?;
    for row in &summary.rows {
        writeln!(
            file,
            "| {} | {} | {} | {} | {} | {} | {} |",
            row.candidate,
            row.stage,
            row.verdict,
            format_metric(&row.rel_cost),
            format_metric(&row.rel_wall),
            format_metric(&row.d_correctness),
            format_metric(&row.d_quality)
        )?;
    }
    writeln!(file)?;

    for candidate in &lock.manifest.candidates {
        let rows: Vec<&CandidateReportRow> = summary
            .rows
            .iter()
            .filter(|r| r.candidate == candidate.id)
            .collect();
        if rows.is_empty() {
            continue;
        }
        writeln!(file, "### `{}`", candidate.id)?;
        writeln!(file, "{}", candidate.hypothesis)?;
        writeln!(file)?;
        for row in &rows {
            writeln!(file, "**{}**: {}", row.stage, row.verdict)?;
            if let Some(confidence) = row.confidence {
                writeln!(file, "- confidence used: {confidence:.4}")?;
            }
            if let Some(seed) = row.seed {
                writeln!(file, "- bootstrap seed: {seed}")?;
            }
            if !row.reasons.is_empty() {
                writeln!(file, "- reasons:")?;
                for reason in &row.reasons {
                    writeln!(file, "  - {reason}")?;
                }
            }
            if row.excluded_by_reason.is_empty() {
                writeln!(file, "- exclusions: none")?;
            } else {
                writeln!(file, "- exclusions:")?;
                for (reason, count) in &row.excluded_by_reason {
                    writeln!(file, "  - {reason}: {count}{}", plural(reason))?;
                }
            }
            for cohort in &row.cohorts {
                writeln!(file, "- cohort `{}`:", cohort.cohort)?;
                writeln!(
                    file,
                    "  - baseline: n={}, success_rate={:.3}, timeout_rate={:.3}, error_rate={:.3}, correctness_mean={}, quality_mean={}, cost_per_success_usd={}, wall_median_ms={}, wall_p90_ms={}",
                    cohort.baseline.n,
                    cohort.baseline.success_rate,
                    cohort.baseline.timeout_rate,
                    cohort.baseline.error_rate,
                    opt(cohort.baseline.correctness_mean),
                    opt(cohort.baseline.quality_mean),
                    opt(cohort.baseline.cost_per_success_usd),
                    opt_u64(cohort.baseline.wall_median_ms),
                    opt_u64(cohort.baseline.wall_p90_ms),
                )?;
                writeln!(
                    file,
                    "  - candidate: n={}, success_rate={:.3}, timeout_rate={:.3}, error_rate={:.3}, correctness_mean={}, quality_mean={}, cost_per_success_usd={}, wall_median_ms={}, wall_p90_ms={}",
                    cohort.candidate.n,
                    cohort.candidate.success_rate,
                    cohort.candidate.timeout_rate,
                    cohort.candidate.error_rate,
                    opt(cohort.candidate.correctness_mean),
                    opt(cohort.candidate.quality_mean),
                    opt(cohort.candidate.cost_per_success_usd),
                    opt_u64(cohort.candidate.wall_median_ms),
                    opt_u64(cohort.candidate.wall_p90_ms),
                )?;
                writeln!(
                    file,
                    "  - d_correctness: {}, d_quality: {}, rel_cost: {}, rel_wall: {} (point [lo, hi])",
                    interval(cohort.d_correctness.as_ref()),
                    interval(cohort.d_quality.as_ref()),
                    interval(cohort.rel_cost.as_ref()),
                    interval(cohort.rel_wall.as_ref()),
                )?;
            }
        }
        let retries_used = retries.get(&candidate.id).copied().unwrap_or(0);
        writeln!(file, "- retries: {retries_used} (across all stages)")?;
        if candidate
            .strategy
            .as_ref()
            .is_some_and(|s| s.kind == "escalate")
        {
            writeln!(
                file,
                "- escalation frequency: not recorded (per-trial details are not persisted to the ledger)"
            )?;
        }
        writeln!(file)?;
    }
    Ok(())
}

fn opt(v: Option<f64>) -> String {
    v.map(|v| format!("{v:.3}"))
        .unwrap_or_else(|| "-".to_string())
}

fn opt_u64(v: Option<u64>) -> String {
    v.map(|v| v.to_string()).unwrap_or_else(|| "-".to_string())
}

fn interval(iv: Option<&super::stats::Interval>) -> String {
    match iv {
        Some(iv) => format!("{:.4} [{:.4}, {:.4}]", iv.point, iv.lo, iv.hi),
        None => "-".to_string(),
    }
}

fn write_report_md(
    path: &Path,
    summary: &ReportSummary,
    lock: &Lock,
    events: &[LedgerEvent],
) -> CtxResult<()> {
    let mut file = std::fs::File::create(path)?;
    writeln!(file, "# Autoresearch campaign: {}", summary.campaign_id)?;
    writeln!(file)?;
    match &summary.promoted {
        Some(id) => writeln!(file, "**Verdict:** promoted candidate `{id}`")?,
        None => writeln!(file, "**Verdict:** no improvement (nothing promoted)")?,
    }
    if let Some(reason) = &summary.stopped_reason {
        writeln!(file, "**Stopped:** {reason}")?;
    }
    writeln!(file)?;
    write_provenance(&mut file, summary, lock)?;
    write_criteria(&mut file, lock)?;
    write_budgets(&mut file, lock, events, summary)?;
    writeln!(file, "## Spend")?;
    writeln!(
        file,
        "- execution: ${:.4} (what the trials' own arms cost -- what a candidate's cost axis is judged on)",
        summary.spend.execution_usd
    )?;
    writeln!(
        file,
        "- overhead: ${:.4} (judges, proposer -- counted against the campaign budget, never against a candidate's own cost)",
        summary.spend.overhead_usd
    )?;
    writeln!(
        file,
        "- completeness: {} (a crash/timeout charged at its declared ceiling, or any trial with an unknown cost, makes this `partial`)",
        summary.spend.completeness
    )?;
    writeln!(file)?;
    write_coverage(&mut file, lock)?;
    let retries = retries_by_candidate(events);
    write_candidates(&mut file, summary, lock, &retries)?;
    write_reproduction(&mut file, lock, path.parent().unwrap_or(Path::new(".")))?;
    Ok(())
}

fn write_rollback(
    path: &Path,
    candidate: &Candidate,
    keys: &[Vec<&'static str>],
    has_patch: bool,
) -> CtxResult<()> {
    let mut file = std::fs::File::create(path)?;
    writeln!(file, "# Rollback: {}", candidate.id)?;
    writeln!(file)?;
    writeln!(
        file,
        "This candidate was PROMOTED as evidence, not applied. Nothing in the"
    )?;
    writeln!(
        file,
        "repository or the operator's own `~/.zirv/ctx.toml` was changed by this"
    )?;
    writeln!(file, "campaign.")?;
    writeln!(file)?;
    if has_patch {
        writeln!(
            file,
            "To adopt it: apply `proposal/candidate.patch` in the repo."
        )?;
        writeln!(file, "To roll back after adopting it:")?;
        writeln!(file, "```")?;
        writeln!(file, "git apply -R proposal/candidate.patch")?;
        writeln!(file, "```")?;
    } else if keys.is_empty() {
        writeln!(
            file,
            "To adopt it: apply the environment variables in `proposal/overlay.toml`'s comments."
        )?;
        writeln!(file, "To roll back: unset those environment variables.")?;
    } else {
        writeln!(
            file,
            "To adopt it: merge `proposal/overlay.toml` into `~/.zirv/ctx.toml`."
        )?;
        writeln!(
            file,
            "To roll back after adopting it, remove exactly these keys:"
        )?;
        for key in keys {
            writeln!(file, "- `{}`", key.join("."))?;
        }
    }
    writeln!(file)?;
    writeln!(
        file,
        "Keep `report.json` and `ledger.jsonl` from this campaign directory as the"
    )?;
    writeln!(
        file,
        "evidence record for this change, whichever way you decide."
    )?;
    Ok(())
}

/// `raw` typed as a TOML scalar: `"true"`/`"false"` as a boolean, an
/// integer- or float-parseable string as a number, anything else as a
/// string. A best-effort rendering from the candidate's own `env` string
/// map -- `ctx::config`'s real `EnvKind` (which would type this exactly) is
/// private to that module; `toml_path_for_env` exposes only the key path.
fn typed_toml_value(raw: &str) -> toml::Value {
    if raw == "true" {
        return toml::Value::Boolean(true);
    }
    if raw == "false" {
        return toml::Value::Boolean(false);
    }
    if let Ok(i) = raw.parse::<i64>() {
        return toml::Value::Integer(i);
    }
    if let Ok(f) = raw.parse::<f64>() {
        return toml::Value::Float(f);
    }
    toml::Value::String(raw.to_string())
}

fn insert_nested(table: &mut toml::value::Table, path: &[&str], value: toml::Value) {
    let Some((head, rest)) = path.split_first() else {
        return;
    };
    if rest.is_empty() {
        table.insert((*head).to_string(), value);
        return;
    }
    let entry = table
        .entry((*head).to_string())
        .or_insert_with(|| toml::Value::Table(toml::value::Table::new()));
    if !entry.is_table() {
        *entry = toml::Value::Table(toml::value::Table::new());
    }
    if let Some(child) = entry.as_table_mut() {
        insert_nested(child, rest, value);
    }
}

/// Renders the candidate's env overlay as real `~/.zirv/ctx.toml` keys
/// (`ctx::config::toml_path_for_env`), with the source env var kept as a
/// comment next to each key -- an env var with no known mapping falls back
/// to a top-level key named after itself, clearly marked unmapped. Returns
/// the resolved key paths too, so `ROLLBACK.md` can name exactly what to
/// remove.
fn write_overlay(path: &Path, candidate: &Candidate) -> CtxResult<Vec<Vec<&'static str>>> {
    let mut root = toml::value::Table::new();
    let mut resolved: Vec<Vec<&'static str>> = Vec::new();
    let mut unmapped: Vec<&str> = Vec::new();
    for (env_key, raw_value) in &candidate.env {
        let value = typed_toml_value(raw_value);
        match crate::commands::ctx::config::toml_path_for_env(env_key) {
            Some(key_path) => {
                insert_nested(&mut root, key_path, value);
                resolved.push(key_path.to_vec());
            }
            None => {
                root.insert(env_key.clone(), value);
                unmapped.push(env_key.as_str());
            }
        }
    }

    let mut file = std::fs::File::create(path)?;
    writeln!(
        file,
        "# Candidate '{}': {}",
        candidate.id, candidate.hypothesis
    )?;
    writeln!(file, "#")?;
    writeln!(
        file,
        "# Merge these keys into ~/.zirv/ctx.toml to adopt this candidate."
    )?;
    writeln!(file, "# Each source env var is noted next to its key.")?;
    for env_key in candidate.env.keys() {
        if let Some(key_path) = crate::commands::ctx::config::toml_path_for_env(env_key) {
            writeln!(file, "# {env_key} -> {}", key_path.join("."))?;
        }
    }
    if !unmapped.is_empty() {
        writeln!(file, "#")?;
        writeln!(
            file,
            "# No ctx.toml key mapping is known for: {}. Set these as environment",
            unmapped.join(", ")
        )?;
        writeln!(file, "# variables instead:")?;
        for env_key in &unmapped {
            writeln!(file, "# export {env_key}={}", candidate.env[*env_key])?;
        }
    }
    writeln!(file)?;
    if !root.is_empty() {
        write!(
            file,
            "{}",
            toml::to_string_pretty(&toml::Value::Table(root))
                .map_err(|err| format!("could not render overlay.toml: {err}"))?
        )?;
    }
    Ok(resolved)
}

/// Regenerates every report artifact for `campaign_dir` from its
/// `lock.json`/`ledger.jsonl` alone.
pub fn generate(campaign_dir: &Path) -> CtxResult<ReportSummary> {
    let lock = Lock::read(campaign_dir)?;
    let events = ledger::replay(&Ledger::path(campaign_dir))?;
    let (promoted, verdict, stopped_reason) = campaign_verdict(&events);
    let rows = candidate_rows(&events, &lock.manifest.candidates);
    let spend = spend_summary(&events);

    let summary = ReportSummary {
        campaign_id: lock.manifest.id.clone(),
        zirv_version: lock.zirv_version.clone(),
        baseline_sha: lock.baseline_sha.clone(),
        started_at: lock.started_at,
        finished_at: finished_at(&events),
        promoted: promoted.clone(),
        verdict,
        objective: lock.manifest.criteria.objective.as_str().to_string(),
        rows,
        stopped_reason,
        spend,
    };

    write_report_md(&campaign_dir.join("report.md"), &summary, &lock, &events)?;
    std::fs::write(
        campaign_dir.join("report.json"),
        serde_json::to_string_pretty(&summary)?,
    )?;
    write_results_tsv(&campaign_dir.join("results.tsv"), &summary.rows)?;

    if let Some(candidate_id) = &promoted {
        let proposal_dir = campaign_dir.join("proposal");
        std::fs::create_dir_all(&proposal_dir)?;
        if let Some(candidate) = lock
            .manifest
            .candidates
            .iter()
            .find(|c| &c.id == candidate_id)
        {
            if let Some(patch_rel) = &candidate.patch {
                let manifest_dir = lock
                    .manifest_path
                    .parent()
                    .map(Path::to_path_buf)
                    .unwrap_or_else(|| Path::new(".").to_path_buf());
                let source = manifest_dir.join(patch_rel);
                if source.is_file() {
                    std::fs::copy(&source, proposal_dir.join("candidate.patch"))?;
                }
                write_rollback(&proposal_dir.join("ROLLBACK.md"), candidate, &[], true)?;
            } else {
                let keys = write_overlay(&proposal_dir.join("overlay.toml"), candidate)?;
                write_rollback(&proposal_dir.join("ROLLBACK.md"), candidate, &keys, false)?;
            }
        }
    }

    Ok(summary)
}

/// Re-derives the exact records a promotion decision was built from, for a
/// completed or in-progress campaign -- used by `status` to show trial
/// counts without re-deriving the ledger-walking logic twice.
pub fn stage_summary(campaign_dir: &Path, stage: Stage, candidate: &str) -> CtxResult<usize> {
    let events = ledger::replay(&Ledger::path(campaign_dir))?;
    Ok(stage_records_from_ledger(&events, stage, candidate).len())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::workflow::research::manifest::Manifest;

    fn write_minimal_lock(campaign_dir: &Path, candidate_id: &str, patch: bool) {
        let candidates_toml = if patch {
            format!(
                r#"
[[candidates]]
id = "{candidate_id}"
hypothesis = "h"
patch = "x.patch"
"#
            )
        } else {
            format!(
                r#"
[[candidates]]
id = "{candidate_id}"
hypothesis = "h"
env = {{ ZIRV_CTX_JEV_MEMORY = "true" }}
"#
            )
        };
        let manifest_text = format!(
            r#"
schema = 1
id = "demo"
runtime = "meta"
seat_mode = "single"
cache_mode = "cold"
billing = "subscription"

[baseline]
commit = "HEAD"

[corpus]
file = "corpus.toml"

[backend]
kind = "fixture"
file = "fixture.toml"
per_trial_ceiling_usd = 1.0
calls_per_trial = 1
timeout_secs = 30

[route]
harness = "claude"
model = "sonnet"

[budgets]
max_spend_usd = 10.0
max_wall_secs = 600
max_calls = 10
max_trials = 10
max_retries = 0
concurrency = 1

[stages.screen]
split = "dev"
reps = 1

[stages.validate]
split = "validation"
reps = 1

[stages.holdout]
split = "holdout"
reps = 1
max_uses = 1

[candidate_space]
allow_env = ["ZIRV_CTX_JEV_MEMORY"]

{candidates_toml}
"#
        );
        let manifest = Manifest::parse(&manifest_text).unwrap();
        let lock = Lock {
            manifest,
            manifest_path: campaign_dir.join("manifest.toml"),
            repo: campaign_dir.join("repo"),
            manifest_sha256: "abc".to_string(),
            baseline_sha: "deadbeef".to_string(),
            corpus_version: "1".to_string(),
            corpus_families: vec!["ledgerlite".to_string()],
            evaluator_version: Some("v1".to_string()),
            evaluator_files: BTreeMap::new(),
            evaluator_fingerprint: "fp".to_string(),
            zirv_version: "test".to_string(),
            price_as_of: None,
            started_at: 1,
        };
        lock.write(campaign_dir).unwrap();
    }

    #[test]
    fn generate_writes_every_output_and_labels_no_promotion() {
        let dir = tempfile::tempdir().unwrap();
        write_minimal_lock(dir.path(), "cand-a", false);
        let (ledger_handle, _) = Ledger::open(dir.path()).unwrap();
        let seq = 0;
        ledger_handle
            .append(&LedgerEvent::CampaignFinished {
                seq,
                ts: 1,
                promoted: None,
            })
            .unwrap();

        let summary = generate(dir.path()).unwrap();
        assert_eq!(summary.promoted, None);
        assert_eq!(summary.verdict, "no_improvement");
        assert!(dir.path().join("report.md").is_file());
        assert!(dir.path().join("report.json").is_file());
        assert!(dir.path().join("results.tsv").is_file());
        assert!(!dir.path().join("proposal").exists());
    }

    #[test]
    fn a_promoted_env_candidate_gets_an_overlay_and_rollback_note() {
        let dir = tempfile::tempdir().unwrap();
        write_minimal_lock(dir.path(), "cand-a", false);
        let (ledger_handle, _) = Ledger::open(dir.path()).unwrap();
        ledger_handle
            .append(&LedgerEvent::CampaignFinished {
                seq: 0,
                ts: 1,
                promoted: Some("cand-a".to_string()),
            })
            .unwrap();

        let summary = generate(dir.path()).unwrap();
        assert_eq!(summary.promoted, Some("cand-a".to_string()));
        assert_eq!(summary.verdict, "accept");
        assert!(dir.path().join("proposal/overlay.toml").is_file());
        assert!(dir.path().join("proposal/ROLLBACK.md").is_file());
        let overlay = std::fs::read_to_string(dir.path().join("proposal/overlay.toml")).unwrap();
        assert!(
            overlay.contains("[jev]") && overlay.contains("memory = true"),
            "must render the real ctx.toml key, not just an env-var comment: {overlay}"
        );
        let rollback = std::fs::read_to_string(dir.path().join("proposal/ROLLBACK.md")).unwrap();
        assert!(
            rollback.contains("jev.memory"),
            "must name the exact key to remove: {rollback}"
        );
    }

    #[test]
    fn format_utc_matches_known_epoch_values() {
        assert_eq!(format_utc(0), "1970-01-01 00:00:00 UTC");
        // The "Unix billennium" -- a well-known round-number timestamp.
        assert_eq!(format_utc(1_000_000_000), "2001-09-09 01:46:40 UTC");
    }

    /// Same shape as `write_minimal_lock`, but with `[criteria] objective =
    /// "quality"` set -- `d_quality`, not `d_correctness`, is the axis that
    /// decides promotion for this campaign, so the report must still carry
    /// it even though the column is unconditional for every objective.
    fn write_quality_objective_lock(campaign_dir: &Path, candidate_id: &str) {
        let manifest_text = format!(
            r#"
schema = 1
id = "demo"
runtime = "meta"
seat_mode = "single"
cache_mode = "cold"
billing = "subscription"

[baseline]
commit = "HEAD"

[corpus]
file = "corpus.toml"

[backend]
kind = "fixture"
file = "fixture.toml"
per_trial_ceiling_usd = 1.0
calls_per_trial = 1
timeout_secs = 30

[route]
harness = "claude"
model = "sonnet"

[budgets]
max_spend_usd = 10.0
max_wall_secs = 600
max_calls = 10
max_trials = 10
max_retries = 0
concurrency = 1

[criteria]
objective = "quality"

[stages.screen]
split = "dev"
reps = 1

[stages.validate]
split = "validation"
reps = 1

[stages.holdout]
split = "holdout"
reps = 1
max_uses = 1

[candidate_space]
allow_env = ["ZIRV_CTX_JEV_MEMORY"]

[[candidates]]
id = "{candidate_id}"
hypothesis = "h"
env = {{ ZIRV_CTX_JEV_MEMORY = "true" }}
"#
        );
        let manifest = Manifest::parse(&manifest_text).unwrap();
        let lock = Lock {
            manifest,
            manifest_path: campaign_dir.join("manifest.toml"),
            repo: campaign_dir.join("repo"),
            manifest_sha256: "abc".to_string(),
            baseline_sha: "deadbeef".to_string(),
            corpus_version: "1".to_string(),
            corpus_families: vec!["ledgerlite".to_string()],
            evaluator_version: Some("v1".to_string()),
            evaluator_files: BTreeMap::new(),
            evaluator_fingerprint: "fp".to_string(),
            zirv_version: "test".to_string(),
            price_as_of: None,
            started_at: 1,
        };
        lock.write(campaign_dir).unwrap();
    }

    /// A `validate` `StageDecision` detail is a `promote::Decision` --
    /// `parse_decision_detail` must pull `d_quality` out of its cohorts the
    /// same way it already pulls `d_correctness`, and `results.tsv` must
    /// carry that value through, not just leave the column blank because
    /// the campaign's own objective happens to be quality rather than
    /// efficiency.
    #[test]
    fn results_tsv_carries_d_quality_for_a_quality_objective_campaign() {
        use crate::commands::workflow::research::promote::{ArmSummary, Decision, Verdict};
        use crate::commands::workflow::research::stats::Interval;

        let dir = tempfile::tempdir().unwrap();
        write_quality_objective_lock(dir.path(), "cand-a");
        let (ledger_handle, _) = Ledger::open(dir.path()).unwrap();

        let arm = ArmSummary {
            n: 8,
            success_rate: 1.0,
            timeout_rate: 0.0,
            error_rate: 0.0,
            correctness_mean: Some(0.9),
            quality_mean: Some(0.7),
            cost_per_success_usd: Some(1.0),
            cost_complete: true,
            wall_median_ms: Some(1000),
            wall_p90_ms: None,
        };
        let decision = Decision {
            verdict: Verdict::Accept,
            reasons: vec!["material d_quality win".to_string()],
            cohorts: vec![CohortDecision {
                cohort: "cohort-a".to_string(),
                verdict: Verdict::Accept,
                reasons: vec![],
                n_pairs: 8,
                excluded: 0,
                baseline: arm.clone(),
                candidate: arm,
                d_correctness: None,
                d_quality: Some(Interval {
                    point: 0.1234,
                    lo: 0.05,
                    hi: 0.2,
                }),
                rel_cost: None,
                rel_wall: None,
            }],
            confidence: 0.95,
        };

        ledger_handle
            .append(&LedgerEvent::StageDecision {
                seq: 0,
                ts: 1,
                candidate: "cand-a".to_string(),
                stage: "validate".to_string(),
                verdict: "accept".to_string(),
                detail: serde_json::to_value(&decision).unwrap(),
            })
            .unwrap();
        ledger_handle
            .append(&LedgerEvent::CampaignFinished {
                seq: 1,
                ts: 2,
                promoted: Some("cand-a".to_string()),
            })
            .unwrap();

        let summary = generate(dir.path()).unwrap();
        assert_eq!(summary.objective, "quality");
        let row = summary
            .rows
            .iter()
            .find(|r| r.candidate == "cand-a" && r.stage == "validate")
            .expect("a validate row for cand-a");
        assert_eq!(row.d_quality, vec![0.1234]);

        let results_tsv = std::fs::read_to_string(dir.path().join("results.tsv")).unwrap();
        let validate_line = results_tsv
            .lines()
            .find(|l| l.starts_with("cand-a\tvalidate\t"))
            .expect("results.tsv must have a validate row for cand-a");
        let fields: Vec<&str> = validate_line.split('\t').collect();
        assert_eq!(
            fields.len(),
            8,
            "row must have all 8 columns: {validate_line}"
        );
        assert_eq!(
            fields[6], "0.1234",
            "the d_quality column must carry the cohort's d_quality point estimate: {validate_line}"
        );
    }
}
