//! Regenerates `report.md`, `report.json`, `results.tsv`, and (for a
//! promoted candidate) `proposal/{overlay.toml,ROLLBACK.md}` purely from
//! `lock.json` + `ledger.jsonl` -- `run` calls this once at the end of a
//! campaign, and `zirv workflow research report` calls it again any time,
//! independent of whether the process that ran the campaign is still alive.

use std::io::Write as _;
use std::path::Path;

use serde::Serialize;

use super::ledger::{self, Ledger, LedgerEvent, Lock};
use super::manifest::Candidate;
use super::run::{Stage, stage_records_from_ledger};
use crate::commands::ctx::CtxResult;

#[derive(Debug, Clone, Serialize)]
pub struct CandidateReportRow {
    pub candidate: String,
    pub stage: String,
    pub verdict: String,
    pub hypothesis: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct ReportSummary {
    pub campaign_id: String,
    pub zirv_version: String,
    pub baseline_sha: String,
    pub started_at: u64,
    pub promoted: Option<String>,
    pub verdict: String,
    pub rows: Vec<CandidateReportRow>,
    pub stopped_reason: Option<String>,
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

fn candidate_rows(events: &[LedgerEvent], candidates: &[Candidate]) -> Vec<CandidateReportRow> {
    let mut rows = Vec::new();
    for candidate in candidates {
        for stage in ["screen", "validate", "holdout"] {
            if let Some(LedgerEvent::StageDecision { verdict, .. }) =
                events.iter().rev().find(|e| matches!(e, LedgerEvent::StageDecision { candidate: c, stage: s, .. } if c == &candidate.id && s == stage))
            {
                rows.push(CandidateReportRow {
                    candidate: candidate.id.clone(),
                    stage: stage.to_string(),
                    verdict: verdict.clone(),
                    hypothesis: candidate.hypothesis.clone(),
                });
            }
        }
    }
    rows
}

fn write_results_tsv(path: &Path, rows: &[CandidateReportRow]) -> CtxResult<()> {
    let mut file = std::fs::File::create(path)?;
    writeln!(
        file,
        "candidate\tstage\tverdict\trel_cost\trel_wall\td_correctness\thypothesis"
    )?;
    for row in rows {
        writeln!(
            file,
            "{}\t{}\t{}\t\t\t\t{}",
            row.candidate, row.stage, row.verdict, row.hypothesis
        )?;
    }
    Ok(())
}

fn write_report_md(path: &Path, summary: &ReportSummary, lock: &Lock) -> CtxResult<()> {
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
    writeln!(file, "## Provenance")?;
    writeln!(file, "- zirv version: {}", summary.zirv_version)?;
    writeln!(file, "- baseline commit: {}", summary.baseline_sha)?;
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
    writeln!(file, "- started at: {}", summary.started_at)?;
    writeln!(file)?;
    writeln!(file, "## Coverage and limitations")?;
    writeln!(
        file,
        "- seat_mode = `{:?}`: single-seat results are not orchestration evidence.",
        lock.manifest.seat_mode
    )?;
    writeln!(file, "- runtime = `{:?}`.", lock.manifest.runtime)?;
    writeln!(file)?;
    writeln!(file, "## Candidates")?;
    writeln!(file, "| candidate | stage | verdict |")?;
    writeln!(file, "|---|---|---|")?;
    for row in &summary.rows {
        writeln!(
            file,
            "| {} | {} | {} |",
            row.candidate, row.stage, row.verdict
        )?;
    }
    writeln!(file)?;
    writeln!(file, "## Reproduction")?;
    writeln!(file, "```")?;
    writeln!(file, "zirv workflow research run <manifest> --resume")?;
    writeln!(file, "```")?;
    Ok(())
}

fn write_rollback(path: &Path, candidate_id: &str) -> CtxResult<()> {
    let mut file = std::fs::File::create(path)?;
    writeln!(file, "# Rollback: {candidate_id}")?;
    writeln!(file)?;
    writeln!(
        file,
        "This candidate was PROMOTED as evidence, not applied. Nothing in the"
    )?;
    writeln!(
        file,
        "repository or the operator's own `~/.zirv/ctx.toml` was changed by this"
    )?;
    writeln!(
        file,
        "campaign. To adopt it, apply `proposal/overlay.toml` or"
    )?;
    writeln!(
        file,
        "`proposal/candidate.patch` yourself; to roll back after adopting it,"
    )?;
    writeln!(file, "remove those keys (or revert the patch).")?;
    Ok(())
}

fn write_overlay(path: &Path, candidate: &Candidate) -> CtxResult<()> {
    let mut file = std::fs::File::create(path)?;
    writeln!(
        file,
        "# Candidate '{}': {}",
        candidate.id, candidate.hypothesis
    )?;
    writeln!(file, "#")?;
    writeln!(
        file,
        "# No `ctx.toml` key mapping is available from this lane (the reverse"
    )?;
    writeln!(
        file,
        "# env -> config-key table is private to `src/commands/ctx/config.rs`,"
    )?;
    writeln!(
        file,
        "# which this lane does not modify) -- apply these as environment"
    )?;
    writeln!(file, "# variables instead:")?;
    for (key, value) in &candidate.env {
        writeln!(file, "# export {key}={value}")?;
    }
    Ok(())
}

/// Regenerates every report artifact for `campaign_dir` from its
/// `lock.json`/`ledger.jsonl` alone.
pub fn generate(campaign_dir: &Path) -> CtxResult<ReportSummary> {
    let lock = Lock::read(campaign_dir)?;
    let events = ledger::replay(&Ledger::path(campaign_dir))?;
    let (promoted, verdict, stopped_reason) = campaign_verdict(&events);
    let rows = candidate_rows(&events, &lock.manifest.candidates);

    let summary = ReportSummary {
        campaign_id: lock.manifest.id.clone(),
        zirv_version: lock.zirv_version.clone(),
        baseline_sha: lock.baseline_sha.clone(),
        started_at: lock.started_at,
        promoted: promoted.clone(),
        verdict,
        rows,
        stopped_reason,
    };

    write_report_md(&campaign_dir.join("report.md"), &summary, &lock)?;
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
            if candidate.patch.is_some() {
                let source = lock
                    .manifest
                    .candidate_space
                    .source_patch
                    .as_ref()
                    .map(|_| candidate.patch.clone().unwrap_or_default())
                    .unwrap_or_default();
                let _ = source; // The patch file itself lives beside the manifest; nothing to copy here.
                let mut note = std::fs::File::create(proposal_dir.join("candidate.patch.txt"))?;
                writeln!(
                    note,
                    "See the candidate's own `patch` file next to the manifest: {source}"
                )?;
            } else {
                write_overlay(&proposal_dir.join("overlay.toml"), candidate)?;
            }
        }
        write_rollback(&proposal_dir.join("ROLLBACK.md"), candidate_id)?;
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
    use std::collections::BTreeMap;

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
            manifest_sha256: "abc".to_string(),
            baseline_sha: "deadbeef".to_string(),
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
    }
}
