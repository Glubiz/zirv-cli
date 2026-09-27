//! The bounded campaign loop (issue #802): self-check -> baseline (dev) ->
//! per candidate screen (paired with the shared baseline) -> discard or
//! survive -> survivors validate (paired) -> gate -> simplest accepted
//! candidate -> one holdout confirmation -> promoted | not promoted. No
//! improvement is a valid, successful result.
//!
//! This file keeps the core types and the top-level [`execute`]
//! orchestration; three natural seams are split into sibling modules:
//! [`super::schedule`] (trial-list construction, cohort/observation
//! building, the concurrency-bounded dispatch loop), [`super::reconcile`]
//! (baseline/lock/evaluator-drift and resume reconciliation), and
//! [`super::proposer`] (the optional `[proposer]` round). See each for its
//! own doc comment.

use std::collections::BTreeMap;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde::{Deserialize, Serialize};

use super::backend;
use super::budget::Tracker;
use super::corpus::Corpus;
use super::guard;
use super::ledger::{self, Ledger, LedgerEvent, Lock, manifest_sha256};
use super::manifest::{self, Candidate, Manifest, SourcePatch, Split};
use super::promote::{self, Decision, Observation, ScreenVerdict, Verdict};
use super::proposer::{apply_proposal_outcome, dev_aggregate_summary, run_proposer_round};
use super::reconcile::{
    ensure_no_drift, load_or_create_lock, reconcile_unfinished, reconstruct_tracker,
    resolve_protected_paths,
};
pub(crate) use super::schedule::stage_records_from_ledger;
use super::schedule::{
    build_task_classes, dispatch_batch, pair_observations, seed_for, shuffled, strategy_json,
    task_ids, trials_for, warmup_trial,
};
use crate::commands::ctx::CtxResult;
use crate::commands::ctx::state::{StateDir, create_private_dir_all, now_secs};

// Used only by this module's own tests, which exercise `reconcile_unfinished`
// and the proposer spawn path directly rather than only through `execute`.
#[cfg(test)]
use super::backend::TrialResult;
#[cfg(test)]
use super::proposer::{proposer_prompt, spawn_and_validate_proposal};
#[cfg(test)]
use super::schedule::trial_dir_for;

pub const TRIALS_DIR: &str = "trials";
pub const WORKTREES_DIR: &str = "wt";
pub const TARGET_DIR: &str = "target";
pub const HOLDOUT_USES_FILE: &str = "research/holdout-uses.jsonl";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Arm {
    Baseline,
    Candidate,
}

impl Arm {
    pub fn as_str(self) -> &'static str {
        match self {
            Arm::Baseline => "baseline",
            Arm::Candidate => "candidate",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stage {
    Screen,
    Validate,
    Holdout,
    /// Cache-mode `warm` only: one uncounted trial per arm, dispatched once
    /// before any of that arm's real trials, sharing the same persistent
    /// state dir they will reuse. Charged for real (a real dispatch, real
    /// spend) but never fed into `promote::Observation` -- `stage_records_
    /// from_ledger` is always called with `Stage::Screen/Validate/Holdout`,
    /// so a `"warmup"`-staged trial is structurally excluded already.
    Warmup,
}

impl Stage {
    pub fn as_str(self) -> &'static str {
        match self {
            Stage::Screen => "screen",
            Stage::Validate => "validate",
            Stage::Holdout => "holdout",
            Stage::Warmup => "warmup",
        }
    }
}

#[derive(Debug, Clone)]
pub struct PendingTrial {
    pub trial_id: String,
    pub candidate: String,
    pub arm: Arm,
    pub stage: Stage,
    pub task: String,
    pub rep: u32,
    pub split: Split,
    pub attempt: u32,
}

#[derive(Debug, Clone)]
pub struct TrialRecord {
    pub task: String,
    pub rep: u32,
    pub arm: Arm,
    pub status: backend::TrialStatus,
    pub correctness: Option<f64>,
    pub quality: Option<f64>,
    pub cost_usd: Option<f64>,
    pub cost_complete: bool,
    pub wall_ms: u64,
    pub env_fingerprint: Option<String>,
    pub receipts: BTreeMap<String, u64>,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct CandidateRuntime {
    pub(crate) env: BTreeMap<String, String>,
    pub(crate) zirv_dir: Option<String>,
    pub(crate) strategy: Option<serde_json::Value>,
    /// Added + deleted lines from the candidate's own patch (0 for a plain
    /// env-overlay candidate) -- feeds `promote::simplest`'s complexity
    /// score alongside the overlay's own key count.
    pub(crate) patch_lines: usize,
}

pub struct RunState {
    pub(crate) manifest: Manifest,
    pub(crate) repo: PathBuf,
    pub(crate) manifest_dir: PathBuf,
    pub(crate) campaign_dir: PathBuf,
    pub(crate) ledger: Ledger,
    pub(crate) tracker: Tracker,
    /// Carried alongside `ledger` so `dispatch_batch` can re-hash protected
    /// files against it right before every trial spawn (issue-review
    /// finding R1): evaluator drift used to be checked only at campaign
    /// start and at each candidate's screen/validate stage decision, so
    /// tampering that happened mid-stage -- while a concurrent batch of
    /// trials was still running -- was not caught until the next stage
    /// boundary, by which point a verdict had already been computed from
    /// (potentially) tainted results.
    pub(crate) lock: Lock,
}

#[derive(Debug, Clone)]
pub struct CampaignSummary {
    pub campaign_dir: PathBuf,
    pub verdict: Verdict,
    pub promoted: Option<String>,
    pub stopped_reason: Option<String>,
}

fn build_candidate_runtimes(
    repo: &Path,
    campaign_dir: &Path,
    manifest_dir: &Path,
    baseline_sha: &str,
    manifest: &Manifest,
    candidates: &[Candidate],
    protected: &[PathBuf],
) -> CtxResult<BTreeMap<String, CandidateRuntime>> {
    let mut map = BTreeMap::new();
    map.insert("baseline".to_string(), CandidateRuntime::default());
    for candidate in candidates {
        let runtime = if candidate.patch.is_some() {
            let source_patch = manifest
                .candidate_space
                .source_patch
                .as_ref()
                .ok_or_else(|| {
                    format!(
                        "candidate '{}' has a patch but no [candidate_space.source_patch]",
                        candidate.id
                    )
                })?;
            prepare_source_patch_candidate(
                repo,
                campaign_dir,
                manifest_dir,
                baseline_sha,
                candidate,
                source_patch,
                protected,
            )?
        } else {
            CandidateRuntime {
                env: candidate.env.clone(),
                zirv_dir: None,
                strategy: candidate.strategy.as_ref().map(strategy_json),
                patch_lines: 0,
            }
        };
        map.insert(candidate.id.clone(), runtime);
    }
    Ok(map)
}

fn prepare_source_patch_candidate(
    repo: &Path,
    campaign_dir: &Path,
    manifest_dir: &Path,
    baseline_sha: &str,
    candidate: &Candidate,
    source_patch: &SourcePatch,
    protected: &[PathBuf],
) -> CtxResult<CandidateRuntime> {
    let patch_rel = candidate
        .patch
        .as_ref()
        .expect("caller only reaches here for a patch candidate");
    let patch_path = manifest_dir.join(patch_rel);
    if !patch_path.is_file() {
        return Err(format!(
            "candidate '{}': patch file '{}' not found",
            candidate.id,
            patch_path.display()
        )
        .into());
    }

    let numstat = Command::new("git")
        .arg("apply")
        .arg("--numstat")
        .arg(&patch_path)
        .current_dir(repo)
        .output()
        .map_err(|err| {
            format!(
                "candidate '{}': could not run `git apply --numstat`: {err}",
                candidate.id
            )
        })?;
    if !numstat.status.success() {
        return Err(format!(
            "candidate '{}': `git apply --numstat` failed: {}",
            candidate.id,
            String::from_utf8_lossy(&numstat.stderr)
        )
        .into());
    }
    let numstat_text = String::from_utf8_lossy(&numstat.stdout);
    let touched = guard::parse_numstat(&numstat_text);
    let patch_lines: usize = numstat_text
        .lines()
        .filter_map(|line| {
            let mut parts = line.splitn(3, '\t');
            let added: usize = parts.next()?.parse().unwrap_or(0);
            let deleted: usize = parts.next()?.parse().unwrap_or(0);
            Some(added + deleted)
        })
        .sum();
    let protected_strs: Vec<String> = protected
        .iter()
        .map(|p| p.to_string_lossy().replace('\\', "/"))
        .collect();
    let violations =
        guard::validate_patch_scope(&touched, &source_patch.allowed_paths, &protected_strs);
    if !violations.is_empty() {
        return Err(format!(
            "candidate '{}' patch scope violations: {violations:?}",
            candidate.id
        )
        .into());
    }

    let wt_dir = campaign_dir.join(WORKTREES_DIR).join(&candidate.id);
    if wt_dir.exists() {
        let _ = std::fs::remove_dir_all(&wt_dir);
    }
    let add = Command::new("git")
        .arg("worktree")
        .arg("add")
        .arg("--detach")
        .arg(&wt_dir)
        .arg(baseline_sha)
        .current_dir(repo)
        .status()
        .map_err(|err| {
            format!(
                "candidate '{}': could not run `git worktree add`: {err}",
                candidate.id
            )
        })?;
    if !add.success() {
        return Err(format!("candidate '{}': `git worktree add` failed", candidate.id).into());
    }
    let apply = Command::new("git")
        .arg("apply")
        .arg(&patch_path)
        .current_dir(&wt_dir)
        .status()
        .map_err(|err| {
            format!(
                "candidate '{}': could not run `git apply`: {err}",
                candidate.id
            )
        })?;
    if !apply.success() {
        return Err(format!(
            "candidate '{}': `git apply` failed in its worktree",
            candidate.id
        )
        .into());
    }

    let target_dir = campaign_dir.join(TARGET_DIR).join(&candidate.id);
    if source_patch.build.is_empty() {
        return Err(format!(
            "candidate '{}': candidate_space.source_patch.build is empty",
            candidate.id
        )
        .into());
    }
    let build_status = Command::new(&source_patch.build[0])
        .args(&source_patch.build[1..])
        .current_dir(&wt_dir)
        .env("CARGO_TARGET_DIR", &target_dir)
        .status()
        .map_err(|err| {
            format!(
                "candidate '{}': could not run its build: {err}",
                candidate.id
            )
        })?;
    if !build_status.success() {
        return Err(format!("candidate '{}': build failed", candidate.id).into());
    }

    Ok(CandidateRuntime {
        env: candidate.env.clone(),
        zirv_dir: Some(
            target_dir
                .join(&source_patch.bin_dir)
                .to_string_lossy()
                .to_string(),
        ),
        strategy: candidate.strategy.as_ref().map(strategy_json),
        patch_lines,
    })
}

fn cleanup_worktrees(repo: &Path, campaign_dir: &Path) {
    let root = campaign_dir.join(WORKTREES_DIR);
    let Ok(entries) = std::fs::read_dir(&root) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            let _ = Command::new("git")
                .arg("worktree")
                .arg("remove")
                .arg("--force")
                .arg(&path)
                .current_dir(repo)
                .status();
        }
    }
}

fn candidate_complexity(
    all_candidates: &[Candidate],
    candidates: &BTreeMap<String, CandidateRuntime>,
    candidate_id: &str,
) -> usize {
    let env_len = all_candidates
        .iter()
        .find(|c| c.id == candidate_id)
        .map(|c| c.env.len())
        .unwrap_or(0);
    let patch_lines = candidates
        .get(candidate_id)
        .map(|r| r.patch_lines)
        .unwrap_or(0);
    env_len + patch_lines
}

/// A validate/holdout `StageDecision`'s persisted `detail`: the `Decision`
/// itself (verdict/reasons/cohorts/confidence), plus two things it does not
/// carry on its own -- the per-reason exclusion breakdown (`Decision`'s own
/// `CohortDecision.excluded` is only ever a total count) and the bootstrap
/// seed actually used, so `report::generate` (which works purely from the
/// ledger, never re-deriving `obs` from the corpus) can show both without
/// guessing. `Decision` has no `#[serde(deny_unknown_fields)]`, so merging
/// extra keys alongside its own here does not break deserializing this same
/// value back into a `Decision`.
fn decision_detail(
    decision: &Decision,
    obs: &[Observation],
    seed: u64,
) -> CtxResult<serde_json::Value> {
    let mut detail = serde_json::to_value(decision)?;
    if let serde_json::Value::Object(map) = &mut detail {
        map.insert(
            "excluded_by_reason".to_string(),
            serde_json::to_value(promote::excluded_by_reason(obs))?,
        );
        map.insert("seed".to_string(), serde_json::json!(seed));
    }
    Ok(detail)
}

fn holdout_uses_path(state_dir: &StateDir) -> PathBuf {
    state_dir.root().join(HOLDOUT_USES_FILE)
}

#[derive(Debug, Serialize, Deserialize)]
struct HoldoutUseRow {
    corpus_file: String,
    corpus_version: String,
    campaign_id: String,
    ts: u64,
}

fn check_holdout_uses(
    state_dir: &StateDir,
    corpus_file: &Path,
    corpus_version: &str,
    max_uses: u32,
) -> Result<(), String> {
    let path = holdout_uses_path(state_dir);
    let Ok(text) = std::fs::read_to_string(&path) else {
        return Ok(());
    };
    let file_str = corpus_file.to_string_lossy();
    let uses = text
        .lines()
        .filter_map(|line| serde_json::from_str::<HoldoutUseRow>(line).ok())
        .filter(|row| row.corpus_file == file_str && row.corpus_version == corpus_version)
        .count() as u32;
    if uses >= max_uses {
        Err("refresh holdout: bump the corpus version".to_string())
    } else {
        Ok(())
    }
}

fn record_holdout_use(
    state_dir: &StateDir,
    corpus_file: &Path,
    corpus_version: &str,
    campaign_id: &str,
) -> CtxResult<()> {
    let path = holdout_uses_path(state_dir);
    if let Some(parent) = path.parent() {
        create_private_dir_all(parent)?;
    }
    let row = HoldoutUseRow {
        corpus_file: corpus_file.to_string_lossy().to_string(),
        corpus_version: corpus_version.to_string(),
        campaign_id: campaign_id.to_string(),
        ts: now_secs(),
    };
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)?;
    writeln!(file, "{}", serde_json::to_string(&row)?)?;
    Ok(())
}

fn unmeasured_summary(campaign_dir: PathBuf, reason: &str) -> CampaignSummary {
    CampaignSummary {
        campaign_dir,
        verdict: Verdict::Unmeasured,
        promoted: None,
        stopped_reason: Some(reason.to_string()),
    }
}

/// Runs (or resumes) the whole bounded campaign described by `manifest_path`
/// against `repo`, writing everything under `<state_dir>/research/<id>/`
/// unless `dir_override` names a different campaign directory.
pub fn execute(
    manifest_path: &Path,
    repo: &Path,
    dir_override: Option<&Path>,
    resume: bool,
    state_dir: &StateDir,
) -> CtxResult<CampaignSummary> {
    let manifest_text = std::fs::read_to_string(manifest_path).map_err(|err| {
        format!(
            "could not read manifest '{}': {err}",
            manifest_path.display()
        )
    })?;
    let manifest = Manifest::parse(&manifest_text)?;
    let manifest_dir = manifest_path
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));
    let repo = repo.to_path_buf();

    let campaign_dir = dir_override
        .map(Path::to_path_buf)
        .unwrap_or_else(|| state_dir.root().join("research").join(&manifest.id));
    create_private_dir_all(&campaign_dir)?;
    create_private_dir_all(&campaign_dir.join(TRIALS_DIR))?;

    let sha = manifest_sha256(&manifest_text);

    if matches!(manifest.runtime, manifest::Runtime::Native) {
        return Ok(unmeasured_summary(
            campaign_dir,
            "runtime = \"native\": zirv native is release-gated (issue #802)",
        ));
    }
    if matches!(manifest.seat_mode, manifest::SeatMode::Orchestration) {
        return Ok(unmeasured_summary(
            campaign_dir,
            "seat_mode = \"orchestration\": no orchestration suite exists yet",
        ));
    }

    let lock = load_or_create_lock(&repo, &manifest, manifest_path, &campaign_dir, &sha, resume)?;

    let (mut research_ledger, mut events) = Ledger::open(&campaign_dir)?;
    let tracker = reconstruct_tracker(lock.started_at, &events);

    if events.is_empty() {
        let seq = research_ledger.next_seq();
        research_ledger.append(&LedgerEvent::CampaignStarted {
            seq,
            ts: now_secs(),
            manifest_sha256: sha.clone(),
            baseline_sha: lock.baseline_sha.clone(),
        })?;
        events = ledger::replay(&Ledger::path(&campaign_dir))?;
    }

    ensure_no_drift(&repo, &lock, &mut research_ledger)?;

    let protected = resolve_protected_paths(&repo, &manifest)?;
    let mut candidates_map = build_candidate_runtimes(
        &repo,
        &campaign_dir,
        &manifest_dir,
        &lock.baseline_sha,
        &manifest,
        &manifest.candidates,
        &protected,
    )?;

    let mut state = RunState {
        manifest: manifest.clone(),
        repo: repo.clone(),
        manifest_dir: manifest_dir.clone(),
        campaign_dir: campaign_dir.clone(),
        ledger: research_ledger,
        tracker,
        lock: lock.clone(),
    };

    let retry_queue = reconcile_unfinished(
        &campaign_dir,
        manifest.budgets,
        &events,
        &mut state.ledger,
        &mut state.tracker,
    )?;
    let mut stop_reason = if retry_queue.is_empty() {
        None
    } else {
        dispatch_batch(&mut state, &candidates_map, retry_queue)?
    };

    let corpus = Corpus::load(&repo.join(&manifest.corpus.file))?;
    let task_classes = build_task_classes(&corpus);

    if stop_reason.is_none() && matches!(manifest.cache_mode, manifest::CacheMode::Warm) {
        let seed = seed_for(&manifest.id, "baseline", "screen");
        let screen_tasks = shuffled(
            &task_ids(
                &corpus,
                manifest.stages.screen.split,
                &manifest.stages.screen.classes,
            ),
            seed,
        );
        if let Some(task) = screen_tasks.first() {
            let mut warmups = vec![warmup_trial(
                "baseline",
                Arm::Baseline,
                task,
                manifest.stages.screen.split,
            )];
            for candidate in &manifest.candidates {
                warmups.push(warmup_trial(
                    &candidate.id,
                    Arm::Candidate,
                    task,
                    manifest.stages.screen.split,
                ));
            }
            stop_reason = dispatch_batch(&mut state, &candidates_map, warmups)?;
        }
    }

    if stop_reason.is_none() {
        let seed = seed_for(&manifest.id, "baseline", "screen");
        let screen_tasks = shuffled(
            &task_ids(
                &corpus,
                manifest.stages.screen.split,
                &manifest.stages.screen.classes,
            ),
            seed,
        );
        let baseline_screen = trials_for(
            "baseline",
            Arm::Baseline,
            Stage::Screen,
            manifest.stages.screen.split,
            &screen_tasks,
            manifest.stages.screen.reps,
        );
        stop_reason = dispatch_batch(&mut state, &candidates_map, baseline_screen)?;
    }

    let mut all_candidates = manifest.candidates.clone();
    if stop_reason.is_none()
        && !resume
        && let Some(proposer_cfg) = &manifest.proposer
    {
        let events_now = ledger::replay(&Ledger::path(&campaign_dir))?;
        let baseline_screen_records =
            stage_records_from_ledger(&events_now, Stage::Screen, "baseline");
        let dev_summary = dev_aggregate_summary(&baseline_screen_records);
        for round in 0..proposer_cfg.max_proposals {
            let outcome =
                run_proposer_round(&campaign_dir, &manifest, proposer_cfg, &dev_summary, round);
            apply_proposal_outcome(
                outcome,
                round,
                &mut state.ledger,
                &mut candidates_map,
                &mut all_candidates,
            )?;
        }
    }

    let mut survivors: Vec<String> = Vec::new();
    if stop_reason.is_none() {
        let events_now = ledger::replay(&Ledger::path(&campaign_dir))?;
        let baseline_screen_records =
            stage_records_from_ledger(&events_now, Stage::Screen, "baseline");
        let seed = seed_for(&manifest.id, "baseline", "screen");
        let screen_tasks = shuffled(
            &task_ids(
                &corpus,
                manifest.stages.screen.split,
                &manifest.stages.screen.classes,
            ),
            seed,
        );

        for candidate in &all_candidates {
            if stop_reason.is_some() {
                break;
            }
            let pending = trials_for(
                &candidate.id,
                Arm::Candidate,
                Stage::Screen,
                manifest.stages.screen.split,
                &screen_tasks,
                manifest.stages.screen.reps,
            );
            stop_reason = dispatch_batch(&mut state, &candidates_map, pending)?;
            if stop_reason.is_some() {
                break;
            }
            let events_now = ledger::replay(&Ledger::path(&campaign_dir))?;
            let candidate_records =
                stage_records_from_ledger(&events_now, Stage::Screen, &candidate.id);
            let obs = pair_observations(
                &manifest,
                &task_classes,
                &baseline_screen_records,
                &candidate_records,
                &candidate.requires_receipts,
            );
            let verdict = promote::screen(&obs, &manifest.criteria);
            let points = promote::screen_points(&obs);
            let excluded = promote::excluded_by_reason(&obs);
            let (verdict_str, reason) = match &verdict {
                ScreenVerdict::Survive => ("survive".to_string(), None),
                ScreenVerdict::Discard { reason } => ("discard".to_string(), Some(reason.clone())),
            };
            let seq = state.ledger.next_seq();
            state.ledger.append(&LedgerEvent::StageDecision {
                seq,
                ts: now_secs(),
                candidate: candidate.id.clone(),
                stage: "screen".to_string(),
                verdict: verdict_str,
                detail: serde_json::json!({
                    "reason": reason,
                    "points": points,
                    "excluded_by_reason": excluded,
                }),
            })?;
            match verdict {
                ScreenVerdict::Survive => survivors.push(candidate.id.clone()),
                ScreenVerdict::Discard { reason } => {
                    let seq = state.ledger.next_seq();
                    state.ledger.append(&LedgerEvent::CandidateRejected {
                        seq,
                        ts: now_secs(),
                        candidate: candidate.id.clone(),
                        reason,
                    })?;
                }
            }
            ensure_no_drift(&repo, &lock, &mut state.ledger)?;
        }
    }

    let mut decisions: Vec<(String, Decision)> = Vec::new();
    if stop_reason.is_none() && !survivors.is_empty() {
        let seed = seed_for(&manifest.id, "baseline", "validate");
        let validate_tasks = shuffled(
            &task_ids(
                &corpus,
                manifest.stages.validate.split,
                &manifest.stages.validate.classes,
            ),
            seed,
        );
        let baseline_validate = trials_for(
            "baseline",
            Arm::Baseline,
            Stage::Validate,
            manifest.stages.validate.split,
            &validate_tasks,
            manifest.stages.validate.reps,
        );
        stop_reason = dispatch_batch(&mut state, &candidates_map, baseline_validate)?;

        if stop_reason.is_none() {
            let events_now = ledger::replay(&Ledger::path(&campaign_dir))?;
            let baseline_validate_records =
                stage_records_from_ledger(&events_now, Stage::Validate, "baseline");
            let adjusted =
                promote::adjusted_confidence(manifest.criteria.confidence, survivors.len());

            for candidate_id in &survivors {
                if stop_reason.is_some() {
                    break;
                }
                let candidate = all_candidates
                    .iter()
                    .find(|c| &c.id == candidate_id)
                    .expect("a survivor always came from all_candidates");
                let pending = trials_for(
                    candidate_id,
                    Arm::Candidate,
                    Stage::Validate,
                    manifest.stages.validate.split,
                    &validate_tasks,
                    manifest.stages.validate.reps,
                );
                stop_reason = dispatch_batch(&mut state, &candidates_map, pending)?;
                if stop_reason.is_some() {
                    break;
                }
                let events_now = ledger::replay(&Ledger::path(&campaign_dir))?;
                let candidate_records =
                    stage_records_from_ledger(&events_now, Stage::Validate, candidate_id);
                let obs = pair_observations(
                    &manifest,
                    &task_classes,
                    &baseline_validate_records,
                    &candidate_records,
                    &candidate.requires_receipts,
                );
                let seed = seed_for(&manifest.id, candidate_id, "validate");
                let decision = promote::evaluate(&obs, &manifest.criteria, adjusted, seed);
                let seq = state.ledger.next_seq();
                state.ledger.append(&LedgerEvent::StageDecision {
                    seq,
                    ts: now_secs(),
                    candidate: candidate_id.clone(),
                    stage: "validate".to_string(),
                    verdict: format!("{:?}", decision.verdict).to_lowercase(),
                    detail: decision_detail(&decision, &obs, seed)?,
                })?;
                decisions.push((candidate_id.clone(), decision));
                ensure_no_drift(&repo, &lock, &mut state.ledger)?;
            }
        }
    }

    let accepted: Vec<(&str, usize, &Decision)> = decisions
        .iter()
        .filter(|(_, d)| d.verdict == Verdict::Accept)
        .map(|(id, d)| {
            (
                id.as_str(),
                candidate_complexity(&all_candidates, &candidates_map, id),
                d,
            )
        })
        .collect();
    let winner = promote::simplest(&accepted, manifest.criteria.objective).map(str::to_string);

    let mut promoted = None;
    let mut final_verdict = Verdict::Inconclusive;

    if stop_reason.is_none()
        && let Some(winner_id) = &winner
    {
        match check_holdout_uses(
            state_dir,
            &manifest.corpus.file,
            &corpus.version,
            manifest.stages.holdout.max_uses,
        ) {
            Ok(()) => {
                let holdout_tasks = task_ids(
                    &corpus,
                    manifest.stages.holdout.split,
                    &manifest.stages.holdout.classes,
                );
                let baseline_holdout = trials_for(
                    "baseline",
                    Arm::Baseline,
                    Stage::Holdout,
                    manifest.stages.holdout.split,
                    &holdout_tasks,
                    manifest.stages.holdout.reps,
                );
                stop_reason = dispatch_batch(&mut state, &candidates_map, baseline_holdout)?;
                if stop_reason.is_none() {
                    let candidate_holdout = trials_for(
                        winner_id,
                        Arm::Candidate,
                        Stage::Holdout,
                        manifest.stages.holdout.split,
                        &holdout_tasks,
                        manifest.stages.holdout.reps,
                    );
                    stop_reason = dispatch_batch(&mut state, &candidates_map, candidate_holdout)?;
                }
                if stop_reason.is_none() {
                    record_holdout_use(
                        state_dir,
                        &manifest.corpus.file,
                        &corpus.version,
                        &manifest.id,
                    )?;
                    let seq = state.ledger.next_seq();
                    state.ledger.append(&LedgerEvent::HoldoutUsed {
                        seq,
                        ts: now_secs(),
                        corpus_file: manifest.corpus.file.to_string_lossy().to_string(),
                        corpus_version: corpus.version.clone(),
                        uses: 0,
                    })?;

                    let candidate = all_candidates
                        .iter()
                        .find(|c| &c.id == winner_id)
                        .expect("winner came from all_candidates");
                    let events_now = ledger::replay(&Ledger::path(&campaign_dir))?;
                    let baseline_holdout_records =
                        stage_records_from_ledger(&events_now, Stage::Holdout, "baseline");
                    let candidate_holdout_records =
                        stage_records_from_ledger(&events_now, Stage::Holdout, winner_id);
                    let obs = pair_observations(
                        &manifest,
                        &task_classes,
                        &baseline_holdout_records,
                        &candidate_holdout_records,
                        &candidate.requires_receipts,
                    );
                    let seed = seed_for(&manifest.id, winner_id, "holdout");
                    let decision = promote::evaluate(
                        &obs,
                        &manifest.criteria,
                        manifest.criteria.confidence,
                        seed,
                    );
                    // One last re-hash immediately before a verdict is
                    // recorded and, if it accepts, a candidate is promoted:
                    // closes the gap between holdout's last trial finishing
                    // and this decision being finalized. On drift this
                    // propagates `Err`, so neither the holdout
                    // `StageDecision` nor `promoted` is ever written from
                    // results that may now be tainted.
                    ensure_no_drift(&repo, &lock, &mut state.ledger)?;
                    let seq = state.ledger.next_seq();
                    state.ledger.append(&LedgerEvent::StageDecision {
                        seq,
                        ts: now_secs(),
                        candidate: winner_id.clone(),
                        stage: "holdout".to_string(),
                        verdict: format!("{:?}", decision.verdict).to_lowercase(),
                        detail: decision_detail(&decision, &obs, seed)?,
                    })?;
                    final_verdict = decision.verdict;
                    if decision.verdict == Verdict::Accept {
                        promoted = Some(winner_id.clone());
                    }
                }
            }
            Err(reason) => {
                let seq = state.ledger.next_seq();
                state.ledger.append(&LedgerEvent::CandidateRejected {
                    seq,
                    ts: now_secs(),
                    candidate: winner_id.clone(),
                    reason,
                })?;
            }
        }
    }

    cleanup_worktrees(&repo, &campaign_dir);

    let seq = state.ledger.next_seq();
    if let Some(reason) = &stop_reason {
        state.ledger.append(&LedgerEvent::CampaignStopped {
            seq,
            ts: now_secs(),
            reason: reason.clone(),
        })?;
    } else {
        state.ledger.append(&LedgerEvent::CampaignFinished {
            seq,
            ts: now_secs(),
            promoted: promoted.clone(),
        })?;
    }

    let summary = CampaignSummary {
        campaign_dir: campaign_dir.clone(),
        verdict: final_verdict,
        promoted,
        stopped_reason: stop_reason,
    };
    super::report::generate(&campaign_dir)?;
    Ok(summary)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    /// Hand-authored TOML (like `manifest.rs`'s own tests) rather than
    /// `toml::to_string` on a `Manifest` value -- the `toml` crate cannot
    /// round-trip a struct whose `Option` fields are not
    /// `skip_serializing_if`, and every manifest section here has one.
    #[allow(clippy::too_many_arguments)]
    fn manifest_toml(
        id: &str,
        corpus_rel: &str,
        fixture_rel: &str,
        concurrency: usize,
        max_retries: u32,
        max_spend_usd: f64,
        screen_reps: u32,
        candidates_block: &str,
    ) -> String {
        format!(
            r#"
schema = 1
id = "{id}"
runtime = "meta"
seat_mode = "single"
cache_mode = "cold"
billing = "subscription"

[baseline]
commit = "HEAD"

[corpus]
file = "{corpus_rel}"

[backend]
kind = "fixture"
file = "{fixture_rel}"
per_trial_ceiling_usd = 1.0
calls_per_trial = 2
timeout_secs = 30

[route]
harness = "claude"
model = "sonnet"

[budgets]
max_spend_usd = {max_spend_usd}
max_wall_secs = 3600
max_calls = 200
max_trials = 200
max_retries = {max_retries}
concurrency = {concurrency}

[stages.screen]
split = "dev"
reps = {screen_reps}

[stages.validate]
split = "validation"
reps = 1

[stages.holdout]
split = "holdout"
reps = 1
max_uses = 1

{candidates_block}
"#
        )
    }

    const ONE_CANDIDATE: &str = r#"
[[candidates]]
id = "cand-a"
hypothesis = "faster"
"#;

    fn write_corpus(path: &Path) {
        std::fs::write(
            path,
            r#"
schema = 1
version = "1"

[[task]]
id = "t1"
family = "f"
class = "bounded"
split = "dev"

[[task]]
id = "t2"
family = "f"
class = "bounded"
split = "validation"

[[task]]
id = "t3"
family = "f"
class = "bounded"
split = "holdout"
"#,
        )
        .unwrap();
    }

    fn write_fixture(path: &Path, extra: &str) {
        std::fs::write(
            path,
            format!(
                r#"
[[result]]
arm = "baseline"
task = "*"
status = "ok"
correctness = 1.0
quality = 1.0
cost_usd = 0.01
wall_ms = 10
{extra}
"#
            ),
        )
        .unwrap();
    }

    fn init_git_repo(dir: &Path) {
        let run = |args: &[&str]| {
            let status = Command::new("git")
                .args(args)
                .current_dir(dir)
                .status()
                .unwrap();
            assert!(status.success(), "git {args:?} failed");
        };
        run(&["init", "-q"]);
        run(&["config", "user.email", "test@example.com"]);
        run(&["config", "user.name", "test"]);
        std::fs::write(dir.join("README.md"), "hello").unwrap();
        run(&["add", "."]);
        run(&["commit", "-q", "-m", "init"]);
    }

    #[test]
    fn a_fixture_campaign_runs_end_to_end_and_finishes() {
        let repo = tempfile::tempdir().unwrap();
        init_git_repo(repo.path());
        write_corpus(&repo.path().join("corpus.toml"));
        let manifest_dir = tempfile::tempdir().unwrap();
        let fixture_path = manifest_dir.path().join("fixture.toml");
        write_fixture(
            &fixture_path,
            r#"
[[result]]
arm = "cand-a"
task = "*"
status = "ok"
correctness = 1.0
quality = 1.0
cost_usd = 0.01
wall_ms = 10
"#,
        );
        let manifest_path = manifest_dir.path().join("manifest.toml");
        std::fs::write(
            &manifest_path,
            manifest_toml(
                "demo",
                "corpus.toml",
                "fixture.toml",
                2,
                0,
                100.0,
                1,
                ONE_CANDIDATE,
            ),
        )
        .unwrap();

        let state_root = tempfile::tempdir().unwrap();
        let state_dir = StateDir::from_root(state_root.path().to_path_buf());

        let summary = execute(&manifest_path, repo.path(), None, false, &state_dir).unwrap();
        assert_eq!(
            summary.promoted, None,
            "the promote stub never accepts a candidate"
        );
        assert!(summary.stopped_reason.is_none());

        let events = ledger::replay(&Ledger::path(&summary.campaign_dir)).unwrap();
        assert!(
            events
                .iter()
                .any(|e| matches!(e, LedgerEvent::CampaignStarted { .. }))
        );
        assert!(
            events
                .iter()
                .any(|e| matches!(e, LedgerEvent::CampaignFinished { .. }))
        );
        assert!(
            events
                .iter()
                .any(|e| matches!(e, LedgerEvent::TrialFinished { .. }))
        );
    }

    #[test]
    fn warm_cache_mode_dispatches_one_uncounted_charged_warmup_trial_per_arm() {
        let repo = tempfile::tempdir().unwrap();
        init_git_repo(repo.path());
        write_corpus(&repo.path().join("corpus.toml"));
        let manifest_dir = tempfile::tempdir().unwrap();
        let fixture_path = manifest_dir.path().join("fixture.toml");
        write_fixture(
            &fixture_path,
            r#"
[[result]]
arm = "cand-a"
task = "*"
status = "ok"
correctness = 1.0
quality = 1.0
cost_usd = 0.02
wall_ms = 10
"#,
        );
        let manifest_path = manifest_dir.path().join("manifest.toml");
        let cold_toml = manifest_toml(
            "warm-demo",
            "corpus.toml",
            "fixture.toml",
            1,
            0,
            100.0,
            1,
            ONE_CANDIDATE,
        );
        let warm_toml = cold_toml.replacen("cache_mode = \"cold\"", "cache_mode = \"warm\"", 1);
        assert!(
            warm_toml.contains("cache_mode = \"warm\""),
            "the replace must actually apply"
        );
        std::fs::write(&manifest_path, &warm_toml).unwrap();

        let state_root = tempfile::tempdir().unwrap();
        let state_dir = StateDir::from_root(state_root.path().to_path_buf());

        let summary = execute(&manifest_path, repo.path(), None, false, &state_dir).unwrap();
        assert!(summary.stopped_reason.is_none());

        let events = ledger::replay(&Ledger::path(&summary.campaign_dir)).unwrap();
        let warmup_finished: Vec<&str> = events
            .iter()
            .filter_map(|e| match e {
                LedgerEvent::TrialFinished {
                    trial_id, cost_usd, ..
                } if trial_id.starts_with("warmup-") => {
                    assert!(
                        cost_usd.is_some_and(|c| c > 0.0),
                        "the warmup trial's spend must be a real, nonzero charge"
                    );
                    Some(trial_id.as_str())
                }
                _ => None,
            })
            .collect();
        assert_eq!(
            warmup_finished.len(),
            2,
            "one warmup trial per arm (baseline + cand-a): {warmup_finished:?}"
        );

        // Screen's own observation-building must never see the warmup
        // trial: with 1 dev task * 1 rep, exactly one real screen trial per
        // arm, not two.
        let baseline_screen = stage_records_from_ledger(&events, Stage::Screen, "baseline");
        let candidate_screen = stage_records_from_ledger(&events, Stage::Screen, "cand-a");
        assert_eq!(
            baseline_screen.len(),
            1,
            "the warmup trial must not appear in screen observations"
        );
        assert_eq!(
            candidate_screen.len(),
            1,
            "the warmup trial must not appear in screen observations"
        );
    }

    #[test]
    fn a_spend_cap_stops_scheduling_with_in_flight_reservations_counted() {
        let repo = tempfile::tempdir().unwrap();
        init_git_repo(repo.path());
        write_corpus(&repo.path().join("corpus.toml"));
        let manifest_dir = tempfile::tempdir().unwrap();
        let fixture_path = manifest_dir.path().join("fixture.toml");
        write_fixture(
            &fixture_path,
            r#"
[[result]]
arm = "cand-a"
task = "*"
status = "ok"
correctness = 1.0
quality = 1.0
cost_usd = 0.01
wall_ms = 10
"#,
        );
        let manifest_path = manifest_dir.path().join("manifest.toml");
        // Only enough budget for the single baseline screen trial
        // (per_trial_ceiling_usd is 1.0 in `manifest_toml`).
        std::fs::write(
            &manifest_path,
            manifest_toml(
                "budget-demo",
                "corpus.toml",
                "fixture.toml",
                1,
                0,
                1.0,
                1,
                ONE_CANDIDATE,
            ),
        )
        .unwrap();

        let state_root = tempfile::tempdir().unwrap();
        let state_dir = StateDir::from_root(state_root.path().to_path_buf());

        let summary = execute(&manifest_path, repo.path(), None, false, &state_dir).unwrap();
        assert_eq!(
            summary.stopped_reason.as_deref(),
            Some("budget_exhausted:spend")
        );

        let events = ledger::replay(&Ledger::path(&summary.campaign_dir)).unwrap();
        let finished = events
            .iter()
            .filter(|e| matches!(e, LedgerEvent::TrialFinished { .. }))
            .count();
        assert_eq!(
            finished, 1,
            "only the single affordable baseline trial should have run"
        );
        assert!(events.iter().any(|e| matches!(e, LedgerEvent::CampaignStopped { reason, .. } if reason == "budget_exhausted:spend")));
    }

    #[test]
    fn resuming_an_already_finished_campaign_schedules_nothing_new() {
        let repo = tempfile::tempdir().unwrap();
        init_git_repo(repo.path());
        write_corpus(&repo.path().join("corpus.toml"));
        let manifest_dir = tempfile::tempdir().unwrap();
        let fixture_path = manifest_dir.path().join("fixture.toml");
        write_fixture(
            &fixture_path,
            r#"
[[result]]
arm = "cand-a"
task = "*"
status = "ok"
correctness = 1.0
quality = 1.0
cost_usd = 0.01
wall_ms = 10
"#,
        );
        let manifest_path = manifest_dir.path().join("manifest.toml");
        let text = manifest_toml(
            "resume-demo",
            "corpus.toml",
            "fixture.toml",
            2,
            0,
            100.0,
            1,
            ONE_CANDIDATE,
        );
        std::fs::write(&manifest_path, &text).unwrap();

        let state_root = tempfile::tempdir().unwrap();
        let state_dir = StateDir::from_root(state_root.path().to_path_buf());

        let summary = execute(&manifest_path, repo.path(), None, false, &state_dir).unwrap();
        assert!(summary.stopped_reason.is_none());
        let events = ledger::replay(&Ledger::path(&summary.campaign_dir)).unwrap();
        let scheduled_before = events
            .iter()
            .filter(|e| matches!(e, LedgerEvent::TrialScheduled { .. }))
            .count();

        // Same manifest, same campaign dir, `--resume`: a completed campaign
        // must not re-dispatch a single already-finished trial.
        let summary2 = execute(
            &manifest_path,
            repo.path(),
            Some(&summary.campaign_dir),
            true,
            &state_dir,
        )
        .unwrap();
        assert!(summary2.stopped_reason.is_none());
        let events2 = ledger::replay(&Ledger::path(&summary2.campaign_dir)).unwrap();
        let scheduled_after = events2
            .iter()
            .filter(|e| matches!(e, LedgerEvent::TrialScheduled { .. }))
            .count();
        assert_eq!(
            scheduled_before, scheduled_after,
            "resuming a finished campaign must schedule nothing new"
        );
    }

    #[test]
    fn resume_reconciles_a_finished_trial_and_retries_a_missing_one() {
        let campaign_dir = tempfile::tempdir().unwrap();
        let (mut research_ledger, _) = Ledger::open(campaign_dir.path()).unwrap();

        // Trial A: scheduled, and its `trial.json` IS present -- the
        // interrupted process dispatched it and the backend finished
        // writing its result, but the scheduler never got to append
        // `trial_finished` before it died.
        let seq = research_ledger.next_seq();
        research_ledger
            .append(&LedgerEvent::TrialScheduled {
                seq,
                ts: 1,
                trial_id: "trial-a".to_string(),
                candidate: "baseline".to_string(),
                arm: "baseline".to_string(),
                stage: "screen".to_string(),
                task: "t1".to_string(),
                rep: 0,
                split: "dev".to_string(),
                attempt: 0,
                reserved_spend_usd: 1.0,
                reserved_calls: 2,
            })
            .unwrap();
        let trial_a_dir = trial_dir_for(campaign_dir.path(), "trial-a", 0);
        std::fs::create_dir_all(&trial_a_dir).unwrap();
        let finished_result = TrialResult {
            schema: backend::TRIAL_SPEC_SCHEMA,
            trial_id: "trial-a".to_string(),
            status: backend::TrialStatus::Ok,
            correctness: Some(1.0),
            quality: Some(1.0),
            wall_ms: 10,
            spend: None,
            route: None,
            env_fingerprint: None,
            details: None,
        };
        finished_result.write(&trial_a_dir).unwrap();

        // Trial B: scheduled, but no `trial.json` at all -- it crashed
        // before writing anything.
        let seq = research_ledger.next_seq();
        research_ledger
            .append(&LedgerEvent::TrialScheduled {
                seq,
                ts: 1,
                trial_id: "trial-b".to_string(),
                candidate: "baseline".to_string(),
                arm: "baseline".to_string(),
                stage: "screen".to_string(),
                task: "t2".to_string(),
                rep: 0,
                split: "dev".to_string(),
                attempt: 0,
                reserved_spend_usd: 1.0,
                reserved_calls: 2,
            })
            .unwrap();

        let events = ledger::replay(&Ledger::path(campaign_dir.path())).unwrap();
        let budgets = manifest::Budgets {
            max_spend_usd: 100.0,
            max_wall_secs: 1000,
            max_calls: 100,
            max_trials: 100,
            max_retries: 1,
            concurrency: 1,
        };
        let mut tracker = reconstruct_tracker(0, &events);

        let retries = reconcile_unfinished(
            campaign_dir.path(),
            budgets,
            &events,
            &mut research_ledger,
            &mut tracker,
        )
        .unwrap();

        assert_eq!(
            retries.len(),
            1,
            "only the trial with no trial.json is retried"
        );
        assert_eq!(retries[0].trial_id, "trial-b");
        assert_eq!(retries[0].attempt, 1);

        let events_after = ledger::replay(&Ledger::path(campaign_dir.path())).unwrap();
        assert!(
            events_after
                .iter()
                .any(|e| matches!(e, LedgerEvent::TrialFinished { trial_id, .. } if trial_id == "trial-a")),
            "trial-a's own trial.json must be reconciled into a trial_finished event"
        );
        assert!(
            events_after
                .iter()
                .any(|e| matches!(e, LedgerEvent::TrialFailed { trial_id, retryable, .. } if trial_id == "trial-b" && *retryable)),
            "trial-b must be recorded as a retryable failure"
        );
    }

    /// Regression for issue-review finding R3: a crash recorded as
    /// `trial_failed { retryable: true }` whose promised retry never
    /// actually got dispatched (a budget stop hit right after the crash, in
    /// the same `dispatch_batch` call, orphaning the retry that was only
    /// ever queued in memory) must not silently disappear. It must (a)
    /// still be counted in this stage's denominators as a terminal crash,
    /// and (b) still be requeued for a real retry on resume, rather than
    /// `reconcile_unfinished` treating the lone `trial_scheduled` +
    /// `trial_failed` pair as already resolved.
    #[test]
    fn an_orphaned_retryable_failure_counts_toward_denominators_and_is_requeued_on_resume() {
        let campaign_dir = tempfile::tempdir().unwrap();
        let (mut research_ledger, _) = Ledger::open(campaign_dir.path()).unwrap();

        let seq = research_ledger.next_seq();
        research_ledger
            .append(&LedgerEvent::TrialScheduled {
                seq,
                ts: 1,
                trial_id: "trial-c".to_string(),
                candidate: "baseline".to_string(),
                arm: "baseline".to_string(),
                stage: "screen".to_string(),
                task: "t1".to_string(),
                rep: 0,
                split: "dev".to_string(),
                attempt: 0,
                reserved_spend_usd: 1.0,
                reserved_calls: 2,
            })
            .unwrap();
        let seq = research_ledger.next_seq();
        research_ledger
            .append(&LedgerEvent::TrialFailed {
                seq,
                ts: 1,
                trial_id: "trial-c".to_string(),
                reason: "crashed".to_string(),
                charged_usd: 1.0,
                attempt: 0,
                retryable: true,
            })
            .unwrap();

        // Nothing else was ever appended for trial-c: the retry that
        // `dispatch_batch` queued in memory never actually redispatched.

        let events = ledger::replay(&Ledger::path(campaign_dir.path())).unwrap();

        // (a) Without a real resolution, the stage's own records must still
        // carry a terminal crash for trial-c instead of it vanishing.
        let records = stage_records_from_ledger(&events, Stage::Screen, "baseline");
        assert_eq!(
            records.len(),
            1,
            "the orphaned retryable failure must still count toward this stage's denominators"
        );
        assert_eq!(records[0].status, backend::TrialStatus::Crash);
        assert_eq!(records[0].task, "t1");

        // (b) On resume, it must be requeued for a real attempt (attempt 1),
        // not treated as already resolved.
        let budgets = manifest::Budgets {
            max_spend_usd: 100.0,
            max_wall_secs: 1000,
            max_calls: 100,
            max_trials: 100,
            max_retries: 2,
            concurrency: 1,
        };
        let mut tracker = reconstruct_tracker(0, &events);
        let retries = reconcile_unfinished(
            campaign_dir.path(),
            budgets,
            &events,
            &mut research_ledger,
            &mut tracker,
        )
        .unwrap();
        assert_eq!(
            retries.len(),
            1,
            "the orphaned retry must be requeued for a fresh dispatch on resume"
        );
        assert_eq!(retries[0].trial_id, "trial-c");
        assert_eq!(retries[0].attempt, 1);

        // Requeueing for redispatch must not fabricate a second ledger
        // event for this trial: no `trial.json`-probe fallback ran, and no
        // charge was recorded twice.
        let events_after = ledger::replay(&Ledger::path(campaign_dir.path())).unwrap();
        let trial_c_events: Vec<&LedgerEvent> = events_after
            .iter()
            .filter(|e| match e {
                LedgerEvent::TrialScheduled { trial_id, .. }
                | LedgerEvent::TrialFinished { trial_id, .. }
                | LedgerEvent::TrialFailed { trial_id, .. } => trial_id == "trial-c",
                _ => false,
            })
            .collect();
        assert_eq!(
            trial_c_events.len(),
            2,
            "reconcile must not append a duplicate event for a trial it only requeued in memory: {trial_c_events:?}"
        );
    }

    #[test]
    fn concurrency_never_exceeds_the_configured_cap() {
        let repo = tempfile::tempdir().unwrap();
        init_git_repo(repo.path());
        std::fs::write(
            repo.path().join("corpus.toml"),
            r#"
schema = 1
version = "1"

[[task]]
id = "t1"
family = "f"
class = "bounded"
split = "dev"

[[task]]
id = "t2"
family = "f"
class = "bounded"
split = "dev"

[[task]]
id = "t3"
family = "f"
class = "bounded"
split = "dev"

[[task]]
id = "t4"
family = "f"
class = "bounded"
split = "dev"
"#,
        )
        .unwrap();
        let manifest_dir = tempfile::tempdir().unwrap();
        let fixture_path = manifest_dir.path().join("fixture.toml");
        std::fs::write(
            &fixture_path,
            r#"
[[result]]
arm = "baseline"
task = "*"
status = "ok"
correctness = 1.0
cost_usd = 0.0
delay_ms = 80
"#,
        )
        .unwrap();
        let manifest_path = manifest_dir.path().join("manifest.toml");
        std::fs::write(
            &manifest_path,
            manifest_toml(
                "concurrency-demo",
                "corpus.toml",
                "fixture.toml",
                2,
                0,
                100.0,
                1,
                "",
            ),
        )
        .unwrap();

        let state_root = tempfile::tempdir().unwrap();
        let state_dir = StateDir::from_root(state_root.path().to_path_buf());

        backend::reset_fixture_concurrency_tracking();
        let summary = execute(&manifest_path, repo.path(), None, false, &state_dir).unwrap();
        assert!(summary.stopped_reason.is_none());
        let events = ledger::replay(&Ledger::path(&summary.campaign_dir)).unwrap();
        let finished = events
            .iter()
            .filter(|e| matches!(e, LedgerEvent::TrialFinished { .. }))
            .count();
        assert_eq!(finished, 4);
        assert!(
            backend::fixture_max_in_flight() <= 2,
            "must never exceed budgets.concurrency = 2"
        );
        assert!(
            backend::fixture_max_in_flight() >= 2,
            "concurrency = 2 must actually be exercised by 4 delayed trials"
        );
    }

    #[test]
    fn a_crash_is_retried_then_recorded_when_retries_are_exhausted() {
        let repo = tempfile::tempdir().unwrap();
        init_git_repo(repo.path());
        write_corpus(&repo.path().join("corpus.toml"));
        let manifest_dir = tempfile::tempdir().unwrap();
        let fixture_path = manifest_dir.path().join("fixture.toml");
        std::fs::write(
            &fixture_path,
            r#"
[[result]]
arm = "baseline"
task = "*"
status = "ok"
correctness = 1.0
cost_usd = 0.01
crash_first = 5
"#,
        )
        .unwrap();
        let manifest_path = manifest_dir.path().join("manifest.toml");
        std::fs::write(
            &manifest_path,
            manifest_toml(
                "crash-demo",
                "corpus.toml",
                "fixture.toml",
                1,
                1,
                100.0,
                1,
                "",
            ),
        )
        .unwrap();

        let state_root = tempfile::tempdir().unwrap();
        let state_dir = StateDir::from_root(state_root.path().to_path_buf());

        let summary = execute(&manifest_path, repo.path(), None, false, &state_dir).unwrap();
        assert!(summary.stopped_reason.is_none());
        let events = ledger::replay(&Ledger::path(&summary.campaign_dir)).unwrap();
        let failures: Vec<(u32, bool)> = events
            .iter()
            .filter_map(|e| match e {
                LedgerEvent::TrialFailed {
                    attempt, retryable, ..
                } => Some((*attempt, *retryable)),
                _ => None,
            })
            .collect();
        assert_eq!(
            failures,
            vec![(0, true), (1, false)],
            "the original attempt and its one retry must both crash"
        );

        let records = stage_records_from_ledger(&events, Stage::Screen, "baseline");
        assert_eq!(records.len(), 1);
        assert_eq!(
            records[0].status,
            backend::TrialStatus::Crash,
            "an exhausted-retry trial is recorded as a crash"
        );
    }

    #[test]
    fn a_second_holdout_use_over_the_same_corpus_version_is_refused() {
        let state_root = tempfile::tempdir().unwrap();
        let state_dir = StateDir::from_root(state_root.path().to_path_buf());
        let corpus_file = Path::new("docs/benchmarks/wrapped-vs-vanilla/corpus.toml");
        assert!(check_holdout_uses(&state_dir, corpus_file, "1", 1).is_ok());
        record_holdout_use(&state_dir, corpus_file, "1", "campaign-a").unwrap();
        let err = check_holdout_uses(&state_dir, corpus_file, "1", 1)
            .expect_err("max_uses of 1 must now refuse");
        assert!(err.contains("refresh holdout"));
        // A different corpus version is unaffected.
        assert!(check_holdout_uses(&state_dir, corpus_file, "2", 1).is_ok());
    }

    /// Regression for issue-review finding R1: evaluator drift used to be
    /// checked only at campaign start and after each candidate's
    /// screen/validate stage decision -- so tampering that happened between
    /// two stage-boundary checks (e.g. right before a fresh `dispatch_batch`
    /// call for the next candidate) was not caught until that whole batch
    /// had already dispatched every one of its trials. `dispatch_batch`
    /// must now re-hash before every single trial, so not even the first
    /// trial in an already-drifted batch is ever dispatched.
    #[test]
    fn dispatch_batch_checks_evaluator_drift_before_every_trial_not_just_at_stage_boundaries() {
        let repo = tempfile::tempdir().unwrap();
        init_git_repo(repo.path());
        write_corpus(&repo.path().join("corpus.toml"));
        let manifest_dir = tempfile::tempdir().unwrap();
        let fixture_path = manifest_dir.path().join("fixture.toml");
        write_fixture(&fixture_path, "");
        let manifest_path = manifest_dir.path().join("manifest.toml");
        let text = manifest_toml(
            "drift-demo",
            "corpus.toml",
            "fixture.toml",
            1,
            0,
            100.0,
            1,
            ONE_CANDIDATE,
        );
        std::fs::write(&manifest_path, &text).unwrap();
        let manifest_text = std::fs::read_to_string(&manifest_path).unwrap();
        let manifest = Manifest::parse(&manifest_text).unwrap();

        let campaign_dir = tempfile::tempdir().unwrap();
        let sha = manifest_sha256(&manifest_text);
        let lock = load_or_create_lock(
            repo.path(),
            &manifest,
            &manifest_path,
            campaign_dir.path(),
            &sha,
            false,
        )
        .unwrap();

        // Tamper with the protected corpus file AFTER the lock hashed it --
        // simulating drift that happened between two stage-boundary checks,
        // before this (freshly started) batch has dispatched anything.
        std::fs::write(
            repo.path().join("corpus.toml"),
            "schema = 1\nversion = \"2\"\n",
        )
        .unwrap();

        let (research_ledger, _) = Ledger::open(campaign_dir.path()).unwrap();
        let tracker = reconstruct_tracker(lock.started_at, &[]);
        let mut state = RunState {
            manifest: manifest.clone(),
            repo: repo.path().to_path_buf(),
            manifest_dir: manifest_dir.path().to_path_buf(),
            campaign_dir: campaign_dir.path().to_path_buf(),
            ledger: research_ledger,
            tracker,
            lock,
        };
        let candidates_map = build_candidate_runtimes(
            repo.path(),
            campaign_dir.path(),
            manifest_dir.path(),
            &state.lock.baseline_sha,
            &manifest,
            &manifest.candidates,
            &[],
        )
        .unwrap();

        let pending = trials_for(
            "baseline",
            Arm::Baseline,
            Stage::Screen,
            Split::Dev,
            &["t1".to_string()],
            1,
        );
        let err = dispatch_batch(&mut state, &candidates_map, pending)
            .expect_err("drift must abort dispatch before any trial in the batch runs");
        assert!(err.to_string().contains("evaluator_tampered"), "got: {err}");

        let events = ledger::replay(&Ledger::path(campaign_dir.path())).unwrap();
        assert!(
            !events
                .iter()
                .any(|e| matches!(e, LedgerEvent::TrialScheduled { .. })),
            "not even the first trial in an already-drifted batch may be dispatched"
        );
        assert!(
            events.iter().any(
                |e| matches!(e, LedgerEvent::CampaignStopped { reason, .. } if reason == "evaluator_tampered")
            ),
            "drift must record campaign_stopped{{evaluator_tampered}}"
        );
    }

    /// The committed `docs/benchmarks/autoresearch/campaigns/fixture-demo.toml`
    /// run end to end against the REAL promotion gate (not a stub): one
    /// accepted candidate promoted through holdout, one rejected outright by
    /// the screen pre-filter, one inconclusive, one candidate whose crashed
    /// first attempt is retried and recorded, and one discarded for being
    /// untriggered -- every outcome the manifest's own header comment
    /// promises, verified against this repo's actual committed data. Fixed
    /// paths resolve from `CARGO_MANIFEST_DIR` (this worktree), never a
    /// synthetic repo, and the campaign directory is a fresh tempdir.
    #[test]
    fn the_committed_fixture_demo_campaign_produces_every_promised_outcome() {
        let repo = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let manifest_path = repo.join("docs/benchmarks/autoresearch/campaigns/fixture-demo.toml");
        let campaign_root = tempfile::tempdir().unwrap();
        let state_root = tempfile::tempdir().unwrap();
        let state_dir = StateDir::from_root(state_root.path().to_path_buf());

        let summary = execute(
            &manifest_path,
            repo,
            Some(campaign_root.path()),
            false,
            &state_dir,
        )
        .expect("the committed fixture-demo campaign must run to completion");
        assert!(
            summary.stopped_reason.is_none(),
            "must never hit a budget/drift stop: {:?}",
            summary.stopped_reason
        );
        assert_eq!(
            summary.promoted,
            Some("cheaper-equal".to_string()),
            "cheaper-equal is the only candidate that should clear screen, validate and holdout"
        );
        assert_eq!(summary.verdict, Verdict::Accept);

        let events = ledger::replay(&Ledger::path(&summary.campaign_dir)).unwrap();

        // cheaper-but-incorrect: correctness 0.5 is both below the
        // correctness_floor (0.8) and a large regression vs baseline's 1.0
        // -- the cheap screen pre-filter catches it before validate ever
        // spends anything on it, and records it as rejected.
        assert!(
            events.iter().any(
                |e| matches!(e, LedgerEvent::CandidateRejected { candidate, .. } if candidate == "cheaper-but-incorrect")
            ),
            "cheaper-but-incorrect must be rejected"
        );
        assert!(
            events.iter().any(|e| matches!(
                e,
                LedgerEvent::StageDecision { candidate, stage, verdict, .. }
                    if candidate == "cheaper-but-incorrect" && stage == "screen" && verdict == "discard"
            )),
            "the rejection must come from the screen stage, not a wasted validate run"
        );

        // no-clear-win: no axis clears min_effect -> inconclusive, reached
        // only after surviving screen (so validate actually ran for it).
        assert!(
            events.iter().any(|e| matches!(
                e,
                LedgerEvent::StageDecision { candidate, stage, verdict, .. }
                    if candidate == "no-clear-win" && stage == "validate" && verdict == "inconclusive"
            )),
            "no-clear-win must be inconclusive at validate"
        );

        // untriggered-gate: requires a jev:harvest_screen receipt the
        // fixture never produces for it -> discarded as untriggered before
        // ever reaching validate.
        assert!(
            events.iter().any(|e| matches!(
                e,
                LedgerEvent::StageDecision { candidate, stage, verdict, .. }
                    if candidate == "untriggered-gate" && stage == "screen" && verdict == "discard"
            )),
            "untriggered-gate must be discarded at screen"
        );
        assert!(
            events.iter().any(|e| matches!(
                e,
                LedgerEvent::CandidateRejected { candidate, reason, .. }
                    if candidate == "untriggered-gate" && reason.contains("untriggered")
            )),
            "untriggered-gate's discard reason must say untriggered"
        );

        // cheaper-equal: its rep-1 trial is scripted to crash once, then
        // recover on retry -- both must show up in the ledger.
        let cheaper_equal_failures: Vec<bool> = events
            .iter()
            .filter_map(|e| match e {
                LedgerEvent::TrialFailed {
                    trial_id,
                    retryable,
                    ..
                } if trial_id.contains("cheaper-equal") => Some(*retryable),
                _ => None,
            })
            .collect();
        assert!(
            !cheaper_equal_failures.is_empty(),
            "cheaper-equal's scripted crash must appear as a trial_failed event"
        );
        let cheaper_equal_finished_ok = events.iter().any(|e| {
            matches!(
                e,
                LedgerEvent::TrialFinished { trial_id, status, .. }
                    if trial_id.contains("cheaper-equal") && status == "ok"
            )
        });
        assert!(
            cheaper_equal_finished_ok,
            "the retried trial must eventually finish ok"
        );

        // Every promised output file.
        let dir = &summary.campaign_dir;
        assert!(dir.join("report.md").is_file());
        assert!(dir.join("report.json").is_file());
        assert!(dir.join("results.tsv").is_file());
        assert!(dir.join("proposal/overlay.toml").is_file());
        assert!(dir.join("proposal/ROLLBACK.md").is_file());

        let report_md = std::fs::read_to_string(dir.join("report.md")).unwrap();
        assert!(
            report_md.contains("## Spend"),
            "report.md must have its own spend section"
        );
        assert!(
            report_md.contains("execution:"),
            "report.md must separate execution spend"
        );
        assert!(
            report_md.contains("overhead:"),
            "report.md must separate overhead spend"
        );
        assert!(
            report_md.contains("completeness:"),
            "report.md must state data completeness"
        );

        // Report-completeness round: results.tsv must never leave
        // rel_cost/rel_wall/d_correctness blank -- a number (from the
        // Decision's own intervals at validate/holdout, or screen's point
        // estimates) or an explicit "-", never an empty cell.
        let results_tsv = std::fs::read_to_string(dir.join("results.tsv")).unwrap();
        let mut data_lines = results_tsv.lines();
        let header = data_lines.next().unwrap();
        assert_eq!(
            header,
            "candidate\tstage\tverdict\trel_cost\trel_wall\td_correctness\td_quality\thypothesis"
        );
        let mut saw_a_row = false;
        for line in data_lines {
            if line.is_empty() {
                continue;
            }
            saw_a_row = true;
            let fields: Vec<&str> = line.split('\t').collect();
            assert_eq!(fields.len(), 8, "row must have all 8 columns: {line}");
            for (name, value) in ["rel_cost", "rel_wall", "d_correctness"]
                .iter()
                .zip(&fields[3..6])
            {
                assert!(
                    !value.is_empty(),
                    "{name} must never be blank (use '-' when unusable): {line}"
                );
            }
        }
        assert!(saw_a_row, "results.tsv must have at least one data row");

        // report.md: verdict reasons, per-cohort arm summaries and delta
        // CIs with the confidence used, exclusions, retries, budgets vs
        // used, promotion criteria + seed, extended provenance, coverage,
        // and a concrete reproduction command.
        assert!(
            report_md.contains("- reasons:"),
            "report.md must print each stage decision's own verdict reasons"
        );
        assert!(
            report_md.contains("cost_per_success_usd=") && report_md.contains("wall_median_ms="),
            "report.md must print per-cohort arm summaries"
        );
        assert!(
            report_md.contains("(point [lo, hi])"),
            "report.md must print delta CIs as point [lo, hi]"
        );
        assert!(
            report_md.contains("confidence used:"),
            "report.md must state the confidence level used per decision"
        );
        assert!(
            report_md.contains("- exclusions:") || report_md.contains("exclusions: none"),
            "report.md must report exclusions per candidate/stage"
        );
        assert!(
            report_md.contains("untriggered"),
            "untriggered-gate's exclusions must be visible in the report"
        );
        assert!(
            report_md.contains("- retries:"),
            "report.md must report retries (cheaper-equal's scripted crash) per candidate"
        );
        assert!(
            report_md.contains("## Budgets")
                && report_md.contains("| spend (total) |")
                && report_md.contains("| trials |"),
            "report.md must have a budgets-vs-used table"
        );
        assert!(
            report_md.contains("## Promotion criteria") && report_md.contains("min_pairs"),
            "report.md must list the manifest's own criteria values"
        );
        assert!(
            report_md.contains("bootstrap seed:"),
            "report.md must report the bootstrap seed used per decision"
        );
        assert!(
            report_md.contains("manifest:") && report_md.contains("fixture-demo.toml"),
            "report.md provenance must name the manifest path"
        );
        assert!(
            report_md.contains("manifest sha256:"),
            "report.md provenance must include the manifest sha256"
        );
        assert!(
            report_md.contains("corpus:") && report_md.contains("corpus version:"),
            "report.md provenance must name the corpus file and version"
        );
        assert!(
            report_md.contains("billing:") && report_md.contains("route:"),
            "report.md provenance must include billing posture and route"
        );
        assert!(
            report_md.contains("cache mode:")
                && report_md.contains("pressure:")
                && report_md.contains("stratify:"),
            "report.md provenance must include cache mode, pressure and stratify"
        );
        assert!(
            report_md.contains("UTC"),
            "report.md provenance must include human-readable UTC timestamps"
        );
        assert!(
            report_md.contains("single project family: `ledgerlite`"),
            "report.md coverage must note the corpus's single project family"
        );
        assert!(
            !report_md.contains("no paid campaign evidence"),
            "report.md must never carry that internal process phrase as a product statement"
        );
        assert!(
            report_md.contains("zirv workflow research run")
                && report_md.contains("--repo")
                && report_md.contains("--resume")
                && report_md.contains("fixture-demo.toml"),
            "report.md reproduction must give the exact manifest path, --repo and --resume"
        );
        let lock = Lock::read(dir).unwrap();
        let baseline_checkout_line = format!("checkout {}", lock.baseline_sha);
        assert!(
            report_md.contains(&baseline_checkout_line),
            "report.md reproduction must name the exact baseline commit to check out: {report_md}"
        );

        // overlay.toml: cheaper-equal's env ZIRV_CTX_JEV_MEMORY=true must
        // render as the REAL ctx.toml key, not just an env-var comment.
        let overlay = std::fs::read_to_string(dir.join("proposal/overlay.toml")).unwrap();
        assert!(
            overlay.contains("[jev]") && overlay.contains("memory = true"),
            "overlay.toml must render the real ctx.toml key: {overlay}"
        );
        assert!(
            overlay.contains("ZIRV_CTX_JEV_MEMORY"),
            "overlay.toml must still note the source env var: {overlay}"
        );

        let rollback = std::fs::read_to_string(dir.join("proposal/ROLLBACK.md")).unwrap();
        assert!(
            rollback.contains("jev.memory"),
            "ROLLBACK.md must name the exact key to remove: {rollback}"
        );
        assert!(
            rollback.contains("report.json") && rollback.contains("ledger.jsonl"),
            "ROLLBACK.md must name the evidence files to keep: {rollback}"
        );
    }

    /// Issue #804: `stratify = "class"` gives every corpus task class its
    /// own cohort key (never pooled with another class), while the default
    /// `stratify = "none"` keeps today's single cohort per campaign
    /// regardless of how many classes the paired tasks span.
    #[test]
    fn stratify_class_splits_cohorts_by_task_class_stratify_none_keeps_one() {
        let base_toml = manifest_toml(
            "stratify-demo",
            "corpus.toml",
            "fixture.toml",
            1,
            0,
            100.0,
            1,
            "",
        );
        let unstratified = Manifest::parse(&base_toml).unwrap();
        // Inserted right after the top-level scalar keys, before any
        // `[table]` header -- appending it at the end of the file would
        // land inside `[stages.holdout]`, TOML's last-opened table.
        let stratified_toml = base_toml.replacen(
            "billing = \"subscription\"",
            "billing = \"subscription\"\nstratify = \"class\"",
            1,
        );
        let stratified = Manifest::parse(&stratified_toml).unwrap();

        let mut classes = BTreeMap::new();
        classes.insert("t1".to_string(), "bounded".to_string());
        classes.insert("t2".to_string(), "architecture".to_string());

        let record = |task: &str, arm: Arm| TrialRecord {
            task: task.to_string(),
            rep: 0,
            arm,
            status: backend::TrialStatus::Ok,
            correctness: Some(1.0),
            quality: Some(1.0),
            cost_usd: Some(1.0),
            cost_complete: true,
            wall_ms: 10,
            env_fingerprint: None,
            receipts: BTreeMap::new(),
        };
        let baseline = vec![record("t1", Arm::Baseline), record("t2", Arm::Baseline)];
        let candidate = vec![record("t1", Arm::Candidate), record("t2", Arm::Candidate)];

        let stratified_obs = pair_observations(&stratified, &classes, &baseline, &candidate, &[]);
        let stratified_cohorts: BTreeSet<&str> =
            stratified_obs.iter().map(|o| o.cohort.as_str()).collect();
        assert_eq!(
            stratified_cohorts.len(),
            2,
            "one cohort per task class: {stratified_cohorts:?}"
        );
        assert!(stratified_cohorts.iter().any(|c| c.ends_with(":bounded")));
        assert!(
            stratified_cohorts
                .iter()
                .any(|c| c.ends_with(":architecture"))
        );

        let plain_obs = pair_observations(&unstratified, &classes, &baseline, &candidate, &[]);
        let plain_cohorts: BTreeSet<&str> = plain_obs.iter().map(|o| o.cohort.as_str()).collect();
        assert_eq!(
            plain_cohorts.len(),
            1,
            "stratify = none must keep a single cohort: {plain_cohorts:?}"
        );
    }

    // -- proposer -------------------------------------------------------

    fn manifest_with_candidate_space(allow_env: &[&str], allowed_models: &[&str]) -> Manifest {
        let text = format!(
            r#"
schema = 1
id = "proposer-demo"
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
allow_env = {allow_env:?}
allowed_models = {allowed_models:?}
"#
        );
        Manifest::parse(&text).unwrap()
    }

    /// A portable stand-in for a proposer subprocess that just echoes fixed
    /// stdout, in place of shelling out to `python -c`: only `python3` ships
    /// on many Linux/macOS CI images (ubuntu/macos runners), so `python`
    /// alone is an ENOENT there ("could not run the proposer command: No
    /// such file or directory") -- no Rust test may depend on Python at
    /// all. `stdout` is written to a file inside `dir` and echoed back with
    /// a small platform-native command instead.
    fn stub_argv(dir: &Path, stdout: &str) -> Vec<String> {
        let file = dir.join("stub-stdout.txt");
        std::fs::write(&file, stdout).unwrap();
        let file = file.to_string_lossy().into_owned();
        if cfg!(windows) {
            vec![
                "cmd".to_string(),
                "/C".to_string(),
                "type".to_string(),
                file,
            ]
        } else {
            vec!["cat".to_string(), file]
        }
    }

    #[test]
    fn spawn_and_validate_proposal_accepts_a_valid_stub_proposal() {
        let manifest = manifest_with_candidate_space(&["ZIRV_CTX_JEV_MEMORY"], &["sonnet"]);
        let dir = tempfile::tempdir().unwrap();
        let argv = stub_argv(
            dir.path(),
            r#"{"id": "cand-x", "hypothesis": "h", "mechanism": "m", "env": {"ZIRV_CTX_JEV_MEMORY": "true"}}"#,
        );
        let candidate = spawn_and_validate_proposal(&argv, dir.path(), &[], &manifest)
            .expect("a valid proposal must not error")
            .expect("a valid proposal must be returned");
        assert_eq!(candidate.id, "cand-x");
        assert_eq!(
            candidate.env.get("ZIRV_CTX_JEV_MEMORY").map(String::as_str),
            Some("true")
        );
        assert!(candidate.patch.is_none());
    }

    #[test]
    fn spawn_and_validate_proposal_rejects_a_fixed_safety_key() {
        // Declared in allow_env deliberately, so this exercises the compiled
        // fixed-safety refusal specifically, not the allow_env membership
        // check.
        let manifest = manifest_with_candidate_space(&["ZIRV_CTX_JEV_APPROVE"], &["sonnet"]);
        let dir = tempfile::tempdir().unwrap();
        let argv = stub_argv(
            dir.path(),
            r#"{"id": "cand-x", "hypothesis": "h", "env": {"ZIRV_CTX_JEV_APPROVE": "false"}}"#,
        );
        let err = spawn_and_validate_proposal(&argv, dir.path(), &[], &manifest)
            .expect_err("a fixed safety gate must be refused");
        assert!(err.contains("fixed safety"), "got: {err}");
    }

    #[test]
    fn spawn_and_validate_proposal_rejects_a_patch_field() {
        let manifest = manifest_with_candidate_space(&["ZIRV_CTX_JEV_MEMORY"], &["sonnet"]);
        let dir = tempfile::tempdir().unwrap();
        let argv = stub_argv(
            dir.path(),
            r#"{"id": "cand-x", "hypothesis": "h", "env": {}, "patch": "x.patch"}"#,
        );
        let err = spawn_and_validate_proposal(&argv, dir.path(), &[], &manifest)
            .expect_err("a patch field must be refused");
        assert!(err.contains("patch"), "got: {err}");
    }

    #[test]
    fn proposer_prompt_never_contains_validation_or_holdout_task_ids() {
        let manifest = manifest_with_candidate_space(&["ZIRV_CTX_JEV_MEMORY"], &["sonnet"]);
        let corpus = Corpus::parse(
            r#"
schema = 1
version = "1"

[[task]]
id = "dev-task-visible"
family = "f"
class = "bounded"
split = "dev"

[[task]]
id = "validation-task-secret"
family = "f"
class = "bounded"
split = "validation"

[[task]]
id = "holdout-task-secret"
family = "f"
class = "bounded"
split = "holdout"
"#,
        )
        .unwrap();
        let dev_records = vec![TrialRecord {
            task: "dev-task-visible".to_string(),
            rep: 0,
            arm: Arm::Baseline,
            status: backend::TrialStatus::Ok,
            correctness: Some(1.0),
            quality: Some(1.0),
            cost_usd: Some(1.0),
            cost_complete: true,
            wall_ms: 10,
            env_fingerprint: None,
            receipts: BTreeMap::new(),
        }];
        let summary = dev_aggregate_summary(&dev_records);
        let prompt = proposer_prompt(&manifest, &summary);

        for task in &corpus.tasks {
            if task.split != Split::Dev {
                assert!(
                    !prompt.contains(&task.id),
                    "prompt must never mention the {:?}-split task '{}'",
                    task.split,
                    task.id
                );
            }
        }
    }

    #[test]
    fn apply_proposal_outcome_schedules_a_valid_proposal_and_records_a_rejection() {
        let dir = tempfile::tempdir().unwrap();
        let (mut ledger, _) = Ledger::open(dir.path()).unwrap();
        let mut candidates_map: BTreeMap<String, CandidateRuntime> = BTreeMap::new();
        let mut all_candidates: Vec<Candidate> = Vec::new();

        let candidate = Candidate {
            id: "cand-x".to_string(),
            hypothesis: "h".to_string(),
            mechanism: None,
            env: BTreeMap::new(),
            patch: None,
            requires_receipts: Vec::new(),
            strategy: None,
        };
        apply_proposal_outcome(
            Ok(Some(candidate.clone())),
            0,
            &mut ledger,
            &mut candidates_map,
            &mut all_candidates,
        )
        .unwrap();
        assert!(
            candidates_map.contains_key("cand-x"),
            "must be scheduled with a runtime"
        );
        assert!(
            all_candidates.iter().any(|c| c.id == "cand-x"),
            "must be scheduled into the candidate list"
        );

        apply_proposal_outcome(
            Err("a forbidden key".to_string()),
            1,
            &mut ledger,
            &mut candidates_map,
            &mut all_candidates,
        )
        .unwrap();

        let events = ledger::replay(&Ledger::path(dir.path())).unwrap();
        assert!(events.iter().any(
            |e| matches!(e, LedgerEvent::CandidateProposed { candidate, .. } if candidate == "cand-x")
        ));
        assert!(events.iter().any(|e| matches!(
            e,
            LedgerEvent::CandidateRejected { candidate, reason, .. }
                if candidate == "proposal-1" && reason == "a forbidden key"
        )));
    }

    /// Regression for issue-review finding R2: a proposal with `id =
    /// "baseline"` must never reach `apply_proposal_outcome`'s
    /// `candidates_map.insert` -- if it did, it would silently overwrite the
    /// baseline's own `CandidateRuntime`, corrupting every subsequent trial
    /// dispatched as "baseline" for the rest of the campaign.
    #[test]
    fn spawn_and_validate_proposal_rejects_the_reserved_baseline_id() {
        let manifest = manifest_with_candidate_space(&["ZIRV_CTX_JEV_MEMORY"], &["sonnet"]);
        let dir = tempfile::tempdir().unwrap();
        let argv = stub_argv(
            dir.path(),
            r#"{"id": "baseline", "hypothesis": "h", "env": {}}"#,
        );
        let err = spawn_and_validate_proposal(&argv, dir.path(), &[], &manifest)
            .expect_err("the reserved id 'baseline' must be refused");
        assert!(err.contains("baseline"), "got: {err}");
    }

    /// Regression for issue-review finding R2: a proposal's `id` must be
    /// validated the same way a declared candidate's `id` is (manifest's own
    /// `[A-Za-z0-9._-]{1,48}` rule) -- an empty or otherwise malformed id
    /// must never reach `candidates_map`.
    #[test]
    fn spawn_and_validate_proposal_rejects_a_malformed_id() {
        let manifest = manifest_with_candidate_space(&["ZIRV_CTX_JEV_MEMORY"], &["sonnet"]);
        let dir = tempfile::tempdir().unwrap();
        let argv = stub_argv(
            dir.path(),
            r#"{"id": "has a space", "hypothesis": "h", "env": {}}"#,
        );
        let err = spawn_and_validate_proposal(&argv, dir.path(), &[], &manifest)
            .expect_err("a malformed id must be refused");
        assert!(err.contains("must match"), "got: {err}");
    }

    /// Regression for issue-review finding R2: even once a proposal's id has
    /// passed format validation, `apply_proposal_outcome` must refuse to
    /// insert it into `candidates_map` when it collides with an id already
    /// there -- "baseline" (seeded before any proposer round ever runs) or a
    /// declared/previously-proposed candidate -- rather than silently
    /// overwriting that entry's runtime.
    #[test]
    fn apply_proposal_outcome_refuses_a_proposal_that_collides_with_an_existing_id() {
        let dir = tempfile::tempdir().unwrap();
        let (mut ledger, _) = Ledger::open(dir.path()).unwrap();
        let mut candidates_map: BTreeMap<String, CandidateRuntime> = BTreeMap::new();
        candidates_map.insert("baseline".to_string(), CandidateRuntime::default());
        candidates_map.insert(
            "declared-a".to_string(),
            CandidateRuntime {
                env: BTreeMap::from([("MARKER".to_string(), "original".to_string())]),
                zirv_dir: None,
                strategy: None,
                patch_lines: 0,
            },
        );
        let mut all_candidates: Vec<Candidate> = Vec::new();

        let overwrite_baseline = Candidate {
            id: "baseline".to_string(),
            hypothesis: "h".to_string(),
            mechanism: None,
            env: BTreeMap::from([("MARKER".to_string(), "hijacked".to_string())]),
            patch: None,
            requires_receipts: Vec::new(),
            strategy: None,
        };
        apply_proposal_outcome(
            Ok(Some(overwrite_baseline)),
            0,
            &mut ledger,
            &mut candidates_map,
            &mut all_candidates,
        )
        .unwrap();

        let overwrite_declared = Candidate {
            id: "declared-a".to_string(),
            hypothesis: "h".to_string(),
            mechanism: None,
            env: BTreeMap::from([("MARKER".to_string(), "hijacked".to_string())]),
            patch: None,
            requires_receipts: Vec::new(),
            strategy: None,
        };
        apply_proposal_outcome(
            Ok(Some(overwrite_declared)),
            1,
            &mut ledger,
            &mut candidates_map,
            &mut all_candidates,
        )
        .unwrap();

        assert!(
            candidates_map
                .get("baseline")
                .is_some_and(|r| r.env.is_empty()),
            "the baseline runtime must not be overwritten by a colliding proposal"
        );
        assert_eq!(
            candidates_map
                .get("declared-a")
                .and_then(|r| r.env.get("MARKER"))
                .map(String::as_str),
            Some("original"),
            "a declared candidate's runtime must not be overwritten by a colliding proposal"
        );
        assert!(
            !all_candidates.iter().any(|c| c.id == "baseline"),
            "baseline must never be pushed into all_candidates via the proposer"
        );
        assert!(
            all_candidates
                .iter()
                .filter(|c| c.id == "declared-a")
                .count()
                == 0,
            "declared-a must not be duplicated in all_candidates via the proposer"
        );

        let events = ledger::replay(&Ledger::path(dir.path())).unwrap();
        let rejections: Vec<&str> = events
            .iter()
            .filter_map(|e| match e {
                LedgerEvent::CandidateRejected {
                    candidate, reason, ..
                } if reason.contains("collides") => Some(candidate.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(
            rejections,
            vec!["baseline", "declared-a"],
            "both collisions must be recorded as rejections, in order"
        );
    }
}
