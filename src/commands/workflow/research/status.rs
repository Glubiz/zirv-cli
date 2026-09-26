//! `zirv workflow research status`: stage, trial counts, and spend used vs
//! caps, read straight from `lock.json` + `ledger.jsonl` -- no dispatch, no
//! provider call, safe to run against a live campaign from another process.

use std::collections::BTreeSet;
use std::path::Path;

use serde::Serialize;

use super::ledger::{self, Ledger, LedgerEvent, Lock};
use super::report::stage_summary;
use super::run::Stage;
use crate::commands::ctx::CtxResult;

#[derive(Debug, Clone, Serialize)]
pub struct StatusReport {
    pub campaign_id: String,
    pub trials_finished: usize,
    pub trials_failed_permanently: usize,
    pub trials_in_flight_or_orphaned: usize,
    pub spend_used_usd: f64,
    pub spend_cap_usd: f64,
    pub calls_used: u64,
    pub calls_cap: u64,
    /// `(candidate, stage, trials counted)`, per `stage_summary`.
    pub stage_counts: Vec<(String, String, usize)>,
    pub last_stage_decisions: Vec<(String, String, String)>,
    pub stopped_reason: Option<String>,
    pub finished: bool,
}

pub fn status(campaign_dir: &Path) -> CtxResult<StatusReport> {
    let lock = Lock::read(campaign_dir)?;
    let events = ledger::replay(&Ledger::path(campaign_dir))?;

    let mut scheduled: BTreeSet<String> = BTreeSet::new();
    let mut terminal: BTreeSet<String> = BTreeSet::new();
    let mut trials_finished = 0usize;
    let mut trials_failed_permanently = 0usize;
    let mut spend_used_usd = 0.0f64;
    let mut calls_used = 0u64;
    let mut stopped_reason = None;
    let mut finished = false;
    let mut decisions: Vec<(String, String, String)> = Vec::new();

    for event in &events {
        match event {
            LedgerEvent::TrialScheduled { trial_id, .. } => {
                scheduled.insert(trial_id.clone());
            }
            LedgerEvent::TrialFinished {
                trial_id,
                cost_usd,
                cost_complete,
                overhead_usd,
                ..
            } => {
                terminal.insert(trial_id.clone());
                trials_finished += 1;
                let ceiling = lock.manifest.backend.per_trial_ceiling_usd;
                let execution_actual = if *cost_complete {
                    cost_usd.unwrap_or(0.0)
                } else {
                    ceiling.max(cost_usd.unwrap_or(0.0))
                };
                spend_used_usd += execution_actual + overhead_usd;
                calls_used += lock.manifest.backend.calls_per_trial as u64;
            }
            LedgerEvent::TrialFailed {
                trial_id,
                retryable,
                charged_usd,
                ..
            } => {
                spend_used_usd += charged_usd;
                if !retryable {
                    terminal.insert(trial_id.clone());
                    trials_failed_permanently += 1;
                }
            }
            LedgerEvent::StageDecision {
                candidate,
                stage,
                verdict,
                ..
            } => {
                decisions.push((candidate.clone(), stage.clone(), verdict.clone()));
            }
            LedgerEvent::CampaignStopped { reason, .. } => stopped_reason = Some(reason.clone()),
            LedgerEvent::CampaignFinished { .. } => finished = true,
            _ => {}
        }
    }

    let in_flight_or_orphaned = scheduled.difference(&terminal).count();

    let mut stage_counts = Vec::new();
    let mut candidate_ids = vec!["baseline".to_string()];
    candidate_ids.extend(lock.manifest.candidates.iter().map(|c| c.id.clone()));
    for candidate_id in &candidate_ids {
        for stage in [Stage::Screen, Stage::Validate, Stage::Holdout] {
            let count = stage_summary(campaign_dir, stage, candidate_id)?;
            if count > 0 {
                stage_counts.push((candidate_id.clone(), stage.as_str().to_string(), count));
            }
        }
    }

    Ok(StatusReport {
        campaign_id: lock.manifest.id.clone(),
        trials_finished,
        trials_failed_permanently,
        trials_in_flight_or_orphaned: in_flight_or_orphaned,
        spend_used_usd,
        spend_cap_usd: lock.manifest.budgets.max_spend_usd,
        calls_used,
        calls_cap: lock.manifest.budgets.max_calls,
        stage_counts,
        last_stage_decisions: decisions,
        stopped_reason,
        finished,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::workflow::research::manifest::Manifest;

    fn minimal_manifest() -> Manifest {
        Manifest::parse(
            r#"
schema = 1
id = "status-demo"
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
calls_per_trial = 2
timeout_secs = 30

[route]
harness = "claude"
model = "sonnet"

[budgets]
max_spend_usd = 10.0
max_wall_secs = 600
max_calls = 20
max_trials = 20
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
"#,
        )
        .unwrap()
    }

    #[test]
    fn status_counts_finished_and_in_flight_trials() {
        let dir = tempfile::tempdir().unwrap();
        let lock = super::super::ledger::Lock {
            manifest: minimal_manifest(),
            manifest_sha256: "x".into(),
            baseline_sha: "y".into(),
            evaluator_version: None,
            evaluator_files: Default::default(),
            evaluator_fingerprint: "fp".into(),
            zirv_version: "test".into(),
            price_as_of: None,
            started_at: 0,
        };
        lock.write(dir.path()).unwrap();

        let (research_ledger, _) = Ledger::open(dir.path()).unwrap();
        research_ledger
            .append(&LedgerEvent::TrialScheduled {
                seq: 0,
                ts: 0,
                trial_id: "a".into(),
                candidate: "baseline".into(),
                arm: "baseline".into(),
                stage: "screen".into(),
                task: "t1".into(),
                rep: 0,
                split: "dev".into(),
                attempt: 0,
                reserved_spend_usd: 1.0,
                reserved_calls: 2,
            })
            .unwrap();
        research_ledger
            .append(&LedgerEvent::TrialFinished {
                seq: 1,
                ts: 0,
                trial_id: "a".into(),
                status: "ok".into(),
                correctness: Some(1.0),
                quality: Some(1.0),
                cost_usd: Some(0.5),
                cost_complete: true,
                overhead_usd: 0.0,
                wall_ms: 10,
                receipts: Default::default(),
            })
            .unwrap();
        research_ledger
            .append(&LedgerEvent::TrialScheduled {
                seq: 2,
                ts: 0,
                trial_id: "b".into(),
                candidate: "baseline".into(),
                arm: "baseline".into(),
                stage: "screen".into(),
                task: "t2".into(),
                rep: 0,
                split: "dev".into(),
                attempt: 0,
                reserved_spend_usd: 1.0,
                reserved_calls: 2,
            })
            .unwrap();

        let report = status(dir.path()).unwrap();
        assert_eq!(report.trials_finished, 1);
        assert_eq!(report.trials_in_flight_or_orphaned, 1);
        assert_eq!(report.spend_used_usd, 0.5);
        assert!(!report.finished);
    }
}
