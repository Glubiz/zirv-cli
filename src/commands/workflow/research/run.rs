//! The bounded campaign loop (issue #802): self-check -> baseline (dev) ->
//! per candidate screen (paired with the shared baseline) -> discard or
//! survive -> survivors validate (paired) -> gate -> simplest accepted
//! candidate -> one holdout confirmation -> promoted | not promoted. No
//! improvement is a valid, successful result.
//!
//! `promote::evaluate`/`screen` are the lane-`ar/promote` stub today (always
//! `Inconclusive`/`Survive`), so a real end-to-end run never promotes
//! anything yet -- this module only owns scheduling, budgets, the ledger,
//! and resume, which is exactly what phase 1 can verify without the real
//! statistics.

use std::collections::{BTreeMap, VecDeque};
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::mpsc;

use serde::{Deserialize, Serialize};

use super::backend::{self, RouteSpec, TrialOutcome, TrialResult, TrialSpec};
use super::budget::Tracker;
use super::corpus::Corpus;
use super::guard;
use super::ledger::{self, Ledger, LedgerEvent, Lock, manifest_sha256};
use super::manifest::{self, Candidate, Manifest, SourcePatch, Split};
use super::promote::{self, Decision, Observation, ScreenVerdict, Verdict};
use crate::commands::ctx::CtxResult;
use crate::commands::ctx::state::{StateDir, create_private_dir_all, now_secs};

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
}

impl Stage {
    pub fn as_str(self) -> &'static str {
        match self {
            Stage::Screen => "screen",
            Stage::Validate => "validate",
            Stage::Holdout => "holdout",
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
struct CandidateRuntime {
    env: BTreeMap<String, String>,
    zirv_dir: Option<String>,
    strategy: Option<serde_json::Value>,
    /// Added + deleted lines from the candidate's own patch (0 for a plain
    /// env-overlay candidate) -- feeds `promote::simplest`'s complexity
    /// score alongside the overlay's own key count.
    patch_lines: usize,
}

pub struct RunState {
    manifest: Manifest,
    repo: PathBuf,
    manifest_dir: PathBuf,
    campaign_dir: PathBuf,
    ledger: Ledger,
    tracker: Tracker,
}

#[derive(Debug, Clone)]
pub struct CampaignSummary {
    pub campaign_dir: PathBuf,
    pub verdict: Verdict,
    pub promoted: Option<String>,
    pub stopped_reason: Option<String>,
}

fn sanitize(text: &str) -> String {
    text.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

fn splitmix64(state: &mut u64) -> u64 {
    *state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
    let mut z = *state;
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

fn seed_for(campaign_id: &str, candidate_id: &str, stage: &str) -> u64 {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(campaign_id.as_bytes());
    hasher.update(candidate_id.as_bytes());
    hasher.update(stage.as_bytes());
    let digest = hasher.finalize();
    u64::from_le_bytes(
        digest[0..8]
            .try_into()
            .expect("sha256 digest is at least 8 bytes"),
    )
}

/// A stable Fisher-Yates shuffle over task ids, seeded so screen/validate
/// order is reproducible per campaign+stage -- not per candidate, so every
/// candidate is paired against the baseline in the same task order.
fn shuffled(tasks: &[String], seed: u64) -> Vec<String> {
    let mut items = tasks.to_vec();
    let mut state = seed;
    for i in (1..items.len()).rev() {
        let r = splitmix64(&mut state);
        let j = (r % (i as u64 + 1)) as usize;
        items.swap(i, j);
    }
    items
}

fn task_ids(corpus: &Corpus, split: Split) -> Vec<String> {
    corpus
        .tasks_for_split(split)
        .into_iter()
        .map(|task| task.id.clone())
        .collect()
}

fn trials_for(
    candidate: &str,
    arm: Arm,
    stage: Stage,
    split: Split,
    tasks: &[String],
    reps: u32,
) -> Vec<PendingTrial> {
    let mut out = Vec::new();
    for task in tasks {
        for rep in 0..reps {
            let trial_id = sanitize(&format!(
                "{}-{}-{}-{}-r{}",
                stage.as_str(),
                candidate,
                arm.as_str(),
                task,
                rep
            ));
            out.push(PendingTrial {
                trial_id,
                candidate: candidate.to_string(),
                arm,
                stage,
                task: task.clone(),
                rep,
                split,
                attempt: 0,
            });
        }
    }
    out
}

fn cohort_key(manifest: &Manifest) -> String {
    format!(
        "{}:{}:{}:{}",
        manifest.runtime.as_str(),
        manifest.route.model,
        manifest.cache_mode.as_str(),
        manifest.cohort.pressure.as_str()
    )
}

fn trial_dir_for(campaign_dir: &Path, trial_id: &str, attempt: u32) -> PathBuf {
    campaign_dir
        .join(TRIALS_DIR)
        .join(trial_id)
        .join(format!("attempt-{attempt}"))
}

fn strategy_json(strategy: &manifest::Strategy) -> serde_json::Value {
    serde_json::json!({ "kind": strategy.kind, "to_model": strategy.to_model })
}

fn map_status(status: backend::TrialStatus) -> promote::TrialStatus {
    match status {
        backend::TrialStatus::Ok => promote::TrialStatus::Ok,
        backend::TrialStatus::Error => promote::TrialStatus::Error,
        backend::TrialStatus::Timeout => promote::TrialStatus::Timeout,
        backend::TrialStatus::Crash => promote::TrialStatus::Crash,
    }
}

fn parse_status(text: &str) -> backend::TrialStatus {
    match text {
        "ok" => backend::TrialStatus::Ok,
        "error" => backend::TrialStatus::Error,
        "timeout" => backend::TrialStatus::Timeout,
        _ => backend::TrialStatus::Crash,
    }
}

fn to_observation(
    cohort: &str,
    required_receipts: &[String],
    pair_fingerprint: Option<&str>,
    record: &TrialRecord,
) -> Observation {
    let mut excluded = None;
    if let (Some(other), Some(mine)) = (pair_fingerprint, record.env_fingerprint.as_deref())
        && other != mine
    {
        excluded = Some("env_mismatch".to_string());
    }
    if excluded.is_none() {
        for receipt in required_receipts {
            if !record.receipts.contains_key(receipt) {
                excluded = Some("untriggered".to_string());
                break;
            }
        }
    }
    Observation {
        task: record.task.clone(),
        rep: record.rep,
        cohort: cohort.to_string(),
        arm: match record.arm {
            Arm::Baseline => promote::Arm::Baseline,
            Arm::Candidate => promote::Arm::Candidate,
        },
        status: map_status(record.status),
        correctness: record.correctness,
        quality: record.quality,
        cost_usd: record.cost_usd,
        cost_complete: record.cost_complete,
        wall_ms: record.wall_ms,
        excluded,
    }
}

fn pair_observations(
    cohort: &str,
    baseline: &[TrialRecord],
    candidate: &[TrialRecord],
    required_receipts: &[String],
) -> Vec<Observation> {
    let baseline_by_key: BTreeMap<(String, u32), &TrialRecord> = baseline
        .iter()
        .map(|r| ((r.task.clone(), r.rep), r))
        .collect();
    let mut obs: Vec<Observation> = baseline
        .iter()
        .map(|r| to_observation(cohort, &[], None, r))
        .collect();
    for record in candidate {
        let pair_fp = baseline_by_key
            .get(&(record.task.clone(), record.rep))
            .and_then(|b| b.env_fingerprint.as_deref());
        obs.push(to_observation(cohort, required_receipts, pair_fp, record));
    }
    obs
}

/// Reconstructs every finished/permanently-failed trial for `(stage,
/// candidate)` purely from the ledger -- the single source of truth both a
/// live run and a resumed one read from, so promotion never depends on
/// which process actually dispatched a trial.
pub(crate) fn stage_records_from_ledger(
    events: &[LedgerEvent],
    stage: Stage,
    candidate: &str,
) -> Vec<TrialRecord> {
    struct Meta {
        candidate: String,
        arm: String,
        stage: String,
        task: String,
        rep: u32,
    }
    let mut meta: BTreeMap<String, Meta> = BTreeMap::new();
    for event in events {
        if let LedgerEvent::TrialScheduled {
            trial_id,
            candidate,
            arm,
            stage,
            task,
            rep,
            ..
        } = event
        {
            meta.insert(
                trial_id.clone(),
                Meta {
                    candidate: candidate.clone(),
                    arm: arm.clone(),
                    stage: stage.clone(),
                    task: task.clone(),
                    rep: *rep,
                },
            );
        }
    }
    let mut records: BTreeMap<String, TrialRecord> = BTreeMap::new();
    for event in events {
        if let LedgerEvent::TrialFinished {
            trial_id,
            status,
            correctness,
            quality,
            cost_usd,
            cost_complete,
            wall_ms,
            receipts,
            ..
        } = event
            && let Some(m) = meta.get(trial_id)
            && m.stage == stage.as_str()
            && m.candidate == candidate
        {
            records.insert(
                trial_id.clone(),
                TrialRecord {
                    task: m.task.clone(),
                    rep: m.rep,
                    arm: if m.arm == "baseline" {
                        Arm::Baseline
                    } else {
                        Arm::Candidate
                    },
                    status: parse_status(status),
                    correctness: *correctness,
                    quality: *quality,
                    cost_usd: *cost_usd,
                    cost_complete: *cost_complete,
                    wall_ms: *wall_ms,
                    env_fingerprint: None,
                    receipts: receipts.clone(),
                },
            );
        }
    }
    for event in events {
        if let LedgerEvent::TrialFailed {
            trial_id,
            retryable,
            ..
        } = event
            && !*retryable
            && !records.contains_key(trial_id)
            && let Some(m) = meta.get(trial_id)
            && m.stage == stage.as_str()
            && m.candidate == candidate
        {
            records.insert(
                trial_id.clone(),
                TrialRecord {
                    task: m.task.clone(),
                    rep: m.rep,
                    arm: if m.arm == "baseline" {
                        Arm::Baseline
                    } else {
                        Arm::Candidate
                    },
                    status: backend::TrialStatus::Crash,
                    correctness: Some(0.0),
                    quality: Some(0.0),
                    cost_usd: None,
                    cost_complete: false,
                    wall_ms: 0,
                    env_fingerprint: None,
                    receipts: BTreeMap::new(),
                },
            );
        }
    }
    records.into_values().collect()
}

/// Runs at most `budgets.concurrency` trials at once. A worker thread never
/// touches `state`; it only runs the backend and reports back over a
/// channel, so the scheduler loop (the only place that reserves budget and
/// writes the ledger) stays single-threaded even though trials run in
/// parallel. Returns the campaign-stop reason, if scheduling a trial would
/// have exceeded a budget cap.
fn dispatch_batch(
    state: &mut RunState,
    candidates: &BTreeMap<String, CandidateRuntime>,
    pending: Vec<PendingTrial>,
) -> CtxResult<Option<String>> {
    let repo = state.repo.clone();
    let fixture_dir = state.manifest_dir.clone();
    let campaign_dir = state.campaign_dir.clone();
    let backend_cfg = state.manifest.backend.clone();
    let budgets = state.manifest.budgets;
    let cohort_env = state.manifest.cohort.env.clone();
    let cache_mode = state.manifest.cache_mode;
    let pressure = state.manifest.cohort.pressure;
    let route = state.manifest.route.clone();
    let campaign_id = state.manifest.id.clone();

    let ceiling = backend_cfg.per_trial_ceiling_usd;
    let calls_reserved = backend_cfg.calls_per_trial as u64;
    let timeout_secs = backend_cfg.timeout_secs;
    let concurrency = budgets.concurrency;

    // On a resumed run, every stage's trial list is rebuilt the same way it
    // was the first time -- so a trial id that already reached a terminal
    // event (finished, or permanently failed) must never be dispatched
    // again, whichever stage loop happens to regenerate it.
    let already_terminal = terminal_trial_ids(&campaign_dir)?;
    let mut queue: VecDeque<PendingTrial> = pending
        .into_iter()
        .filter(|trial| !already_terminal.contains(&trial.trial_id))
        .collect();
    let mut in_flight = 0usize;
    // The channel carries a plain `TrialOutcome`, never `CtxResult<_>`:
    // `CtxResult`'s `Box<dyn Error>` is not `Send`, so any dispatch error is
    // folded into `TrialOutcome::Crash` inside the worker closure itself,
    // before it ever crosses the channel.
    let (tx, rx) = mpsc::channel::<(PendingTrial, TrialOutcome)>();
    let mut stop_reason: Option<String> = None;
    let mut dispatch_err: Option<Box<dyn std::error::Error>> = None;

    std::thread::scope(|scope| {
        'outer: loop {
            while dispatch_err.is_none() && stop_reason.is_none() && in_flight < concurrency {
                let Some(pending_trial) = queue.pop_front() else {
                    break;
                };
                let now = now_secs();
                match state.tracker.check_reservation(
                    &budgets,
                    ceiling,
                    calls_reserved,
                    timeout_secs,
                    now,
                ) {
                    Err(exhausted) => {
                        stop_reason = Some(exhausted.reason().to_string());
                        queue.push_front(pending_trial);
                        break;
                    }
                    Ok(()) => {
                        state.tracker.reserve(ceiling, calls_reserved);
                        let seq = state.ledger.next_seq();
                        let scheduled = state.ledger.append(&LedgerEvent::TrialScheduled {
                            seq,
                            ts: now,
                            trial_id: pending_trial.trial_id.clone(),
                            candidate: pending_trial.candidate.clone(),
                            arm: pending_trial.arm.as_str().to_string(),
                            stage: pending_trial.stage.as_str().to_string(),
                            task: pending_trial.task.clone(),
                            rep: pending_trial.rep,
                            split: pending_trial.split.as_str().to_string(),
                            attempt: pending_trial.attempt,
                            reserved_spend_usd: ceiling,
                            reserved_calls: calls_reserved,
                        });
                        if let Err(err) = scheduled {
                            dispatch_err = Some(err);
                            break 'outer;
                        }
                        in_flight += 1;

                        let runtime = candidates
                            .get(&pending_trial.candidate)
                            .cloned()
                            .unwrap_or_default();
                        let trial_dir = trial_dir_for(
                            &campaign_dir,
                            &pending_trial.trial_id,
                            pending_trial.attempt,
                        );
                        let trial_state_dir = match cache_mode {
                            manifest::CacheMode::Cold => trial_dir.join("state"),
                            manifest::CacheMode::Warm => campaign_dir
                                .join("warm")
                                .join(&pending_trial.candidate)
                                .join(pending_trial.arm.as_str())
                                .join("state"),
                        };
                        let mut env = cohort_env.clone();
                        for (k, v) in &runtime.env {
                            env.insert(k.clone(), v.clone());
                        }
                        let spec = TrialSpec {
                            schema: backend::TRIAL_SPEC_SCHEMA,
                            campaign: campaign_id.clone(),
                            candidate: pending_trial.candidate.clone(),
                            arm: pending_trial.arm.as_str().to_string(),
                            trial_id: pending_trial.trial_id.clone(),
                            task: pending_trial.task.clone(),
                            rep: pending_trial.rep,
                            split: pending_trial.split.as_str().to_string(),
                            stage: pending_trial.stage.as_str().to_string(),
                            route: RouteSpec {
                                harness: route.harness.clone(),
                                model: route.model.clone(),
                            },
                            env,
                            state_dir: trial_state_dir.to_string_lossy().to_string(),
                            timeout_secs,
                            zirv_dir: runtime.zirv_dir.clone(),
                            strategy: runtime.strategy.clone(),
                            cache_mode: cache_mode.as_str().to_string(),
                            pressure: pressure.as_str().to_string(),
                        };
                        let backend_cfg = backend_cfg.clone();
                        let repo = repo.clone();
                        let fixture_dir = fixture_dir.clone();
                        let attempt = pending_trial.attempt;
                        let tx = tx.clone();
                        let reported = pending_trial.clone();
                        scope.spawn(move || {
                            let outcome = backend::run_trial(
                                &backend_cfg,
                                &spec,
                                &trial_dir,
                                &repo,
                                &fixture_dir,
                                attempt,
                            )
                            .unwrap_or_else(|err| {
                                TrialOutcome::Crash {
                                    reason: err.to_string(),
                                }
                            });
                            let _ = tx.send((reported, outcome));
                        });
                    }
                }
            }
            if dispatch_err.is_some() || in_flight == 0 {
                break;
            }
            let Ok((pending_trial, outcome)) = rx.recv() else {
                break;
            };
            in_flight -= 1;
            let now = now_secs();
            let result: CtxResult<()> = (|| match outcome {
                TrialOutcome::Finished(result) => {
                    let execution_actual = if result.cost_complete() {
                        result.cost_usd().unwrap_or(0.0)
                    } else {
                        ceiling.max(result.cost_usd().unwrap_or(0.0))
                    };
                    let overhead_actual = result.overhead_usd();
                    state.tracker.settle(
                        ceiling,
                        calls_reserved,
                        execution_actual + overhead_actual,
                        result.calls().max(1),
                    );
                    let seq = state.ledger.next_seq();
                    state.ledger.append(&LedgerEvent::TrialFinished {
                        seq,
                        ts: now,
                        trial_id: pending_trial.trial_id.clone(),
                        status: result.status.as_str().to_string(),
                        correctness: result.correctness,
                        quality: result.quality,
                        cost_usd: result.cost_usd(),
                        cost_complete: result.cost_complete(),
                        overhead_usd: overhead_actual,
                        wall_ms: result.wall_ms,
                        receipts: result.receipts(),
                    })
                }
                TrialOutcome::Timeout => {
                    state.tracker.settle(ceiling, calls_reserved, ceiling, 0);
                    let seq = state.ledger.next_seq();
                    state.ledger.append(&LedgerEvent::TrialFinished {
                        seq,
                        ts: now,
                        trial_id: pending_trial.trial_id.clone(),
                        status: "timeout".to_string(),
                        correctness: Some(0.0),
                        quality: Some(0.0),
                        cost_usd: None,
                        cost_complete: false,
                        overhead_usd: 0.0,
                        wall_ms: timeout_secs.saturating_mul(1000),
                        receipts: BTreeMap::new(),
                    })
                }
                TrialOutcome::Crash { reason } => {
                    state.tracker.settle(ceiling, calls_reserved, ceiling, 0);
                    let retryable = pending_trial.attempt < budgets.max_retries;
                    let seq = state.ledger.next_seq();
                    state.ledger.append(&LedgerEvent::TrialFailed {
                        seq,
                        ts: now,
                        trial_id: pending_trial.trial_id.clone(),
                        reason,
                        charged_usd: ceiling,
                        attempt: pending_trial.attempt,
                        retryable,
                    })?;
                    if retryable {
                        let mut retry = pending_trial.clone();
                        retry.attempt += 1;
                        queue.push_back(retry);
                    }
                    Ok(())
                }
            })();
            if let Err(err) = result {
                dispatch_err = Some(err);
                break;
            }
        }
    });

    if let Some(err) = dispatch_err {
        return Err(err);
    }
    Ok(stop_reason)
}

fn resolve_git_sha(repo: &Path, commit: &str) -> CtxResult<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(repo)
        .arg("rev-parse")
        .arg(commit)
        .output()
        .map_err(|err| format!("could not run `git rev-parse {commit}`: {err}"))?;
    if !output.status.success() {
        return Err(format!(
            "`git rev-parse {commit}` failed: {}",
            String::from_utf8_lossy(&output.stderr)
        )
        .into());
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

fn run_self_check(repo: &Path, manifest: &Manifest) -> CtxResult<()> {
    let Some(command) = &manifest.evaluator.self_check else {
        return Ok(());
    };
    if command.is_empty() {
        return Ok(());
    }
    let status = Command::new(&command[0])
        .args(&command[1..])
        .current_dir(repo)
        .status()
        .map_err(|err| format!("could not run evaluator self_check {command:?}: {err}"))?;
    if !status.success() {
        return Err(format!("evaluator self_check {command:?} failed with {status}").into());
    }
    Ok(())
}

pub fn resolve_protected_paths(repo: &Path, manifest: &Manifest) -> CtxResult<Vec<PathBuf>> {
    let mut paths = std::collections::BTreeSet::new();
    for pattern in &manifest.evaluator.protected {
        for path in guard::expand_glob(repo, pattern) {
            paths.insert(path);
        }
    }
    if repo.join(&manifest.corpus.file).is_file() {
        paths.insert(manifest.corpus.file.clone());
    }
    if let Some(parent) = manifest.corpus.file.parent() {
        for candidate in guard::compiled_protected_defaults(parent) {
            if repo.join(&candidate).is_file() {
                paths.insert(candidate);
            }
        }
    }
    Ok(paths.into_iter().collect())
}

fn resolve_price_as_of(repo: &Path) -> Option<String> {
    let cfg =
        crate::commands::ctx::config::CtxConfig::load(repo, &|key| std::env::var(key).ok()).ok()?;
    Some(crate::commands::ctx::price::resolve_table(&cfg).as_of)
}

fn ensure_no_drift(repo: &Path, lock: &Lock, ledger: &mut Ledger) -> CtxResult<()> {
    let drifted = guard::detect_drift(repo, &lock.evaluator_files);
    if drifted.is_empty() {
        return Ok(());
    }
    let seq = ledger.next_seq();
    ledger.append(&LedgerEvent::CampaignStopped {
        seq,
        ts: now_secs(),
        reason: "evaluator_tampered".to_string(),
    })?;
    Err(format!(
        "evaluator_tampered: protected file(s) changed since the campaign started: {drifted:?}"
    )
    .into())
}

fn load_or_create_lock(
    repo: &Path,
    manifest: &Manifest,
    campaign_dir: &Path,
    sha: &str,
    resume: bool,
) -> CtxResult<Lock> {
    if Lock::exists(campaign_dir) {
        if !resume {
            return Err(format!(
                "campaign directory '{}' already has a lock.json; pass --resume or use a different --dir",
                campaign_dir.display()
            )
            .into());
        }
        let existing = Lock::read(campaign_dir)?;
        if existing.manifest_sha256 != sha {
            return Err(
                "--resume refused: the manifest has changed since this campaign started".into(),
            );
        }
        Ok(existing)
    } else {
        if resume {
            return Err(format!("--resume: no lock.json in '{}'", campaign_dir.display()).into());
        }
        let baseline_sha = resolve_git_sha(repo, &manifest.baseline.commit)?;
        run_self_check(repo, manifest)?;
        let protected = resolve_protected_paths(repo, manifest)?;
        let (hashes, fingerprint) = guard::hash_protected_files(repo, &protected)?;
        let lock = Lock {
            manifest: manifest.clone(),
            manifest_sha256: sha.to_string(),
            baseline_sha,
            evaluator_version: manifest.evaluator.version.clone(),
            evaluator_files: hashes,
            evaluator_fingerprint: fingerprint,
            zirv_version: env!("CARGO_PKG_VERSION").to_string(),
            price_as_of: resolve_price_as_of(repo),
            started_at: now_secs(),
        };
        lock.write(campaign_dir)?;
        Ok(lock)
    }
}

/// Rebuilds spend/call/trial accounting from a replayed ledger. A scheduled
/// trial with no terminal event yet stays "reserved" here -- exactly as it
/// was mid-run -- until `reconcile_unfinished` resolves it.
/// Every trial id that has already reached a terminal state (finished, or
/// permanently failed past `max_retries`) anywhere in the campaign's
/// ledger -- read fresh each time so a resumed run's re-derived stage lists
/// never re-dispatch one, whichever stage rebuilt it.
fn terminal_trial_ids(campaign_dir: &Path) -> CtxResult<std::collections::BTreeSet<String>> {
    let events = ledger::replay(&Ledger::path(campaign_dir))?;
    let mut terminal = std::collections::BTreeSet::new();
    for event in &events {
        match event {
            LedgerEvent::TrialFinished { trial_id, .. } => {
                terminal.insert(trial_id.clone());
            }
            LedgerEvent::TrialFailed {
                trial_id,
                retryable: false,
                ..
            } => {
                terminal.insert(trial_id.clone());
            }
            _ => {}
        }
    }
    Ok(terminal)
}

fn reconstruct_tracker(started_at: u64, events: &[LedgerEvent]) -> Tracker {
    let mut tracker = Tracker::new(started_at);
    let mut outstanding: BTreeMap<String, VecDeque<(f64, u64)>> = BTreeMap::new();
    for event in events {
        match event {
            LedgerEvent::TrialScheduled {
                trial_id,
                reserved_spend_usd,
                reserved_calls,
                ..
            } => {
                tracker.reserve(*reserved_spend_usd, *reserved_calls);
                outstanding
                    .entry(trial_id.clone())
                    .or_default()
                    .push_back((*reserved_spend_usd, *reserved_calls));
            }
            LedgerEvent::TrialFinished {
                trial_id,
                cost_usd,
                cost_complete,
                overhead_usd,
                ..
            } => {
                if let Some((ceiling, calls)) =
                    outstanding.get_mut(trial_id).and_then(VecDeque::pop_front)
                {
                    let execution_actual = if *cost_complete {
                        cost_usd.unwrap_or(0.0)
                    } else {
                        ceiling.max(cost_usd.unwrap_or(0.0))
                    };
                    tracker.settle(
                        ceiling,
                        calls,
                        execution_actual + overhead_usd,
                        calls.max(1),
                    );
                }
            }
            LedgerEvent::TrialFailed {
                trial_id,
                charged_usd,
                ..
            } => {
                if let Some((ceiling, calls)) =
                    outstanding.get_mut(trial_id).and_then(VecDeque::pop_front)
                {
                    tracker.settle(ceiling, calls, *charged_usd, 0);
                    let _ = ceiling;
                }
            }
            _ => {}
        }
    }
    tracker
}

/// Resolves every `trial_scheduled` that never reached a terminal event
/// (the previous process died mid-dispatch): `trial.json` present and valid
/// -> `trial_finished`; otherwise -> `trial_failed` (charged at the
/// ceiling), re-queued for a fresh attempt only within `max_retries`.
fn reconcile_unfinished(
    campaign_dir: &Path,
    budgets: manifest::Budgets,
    events: &[LedgerEvent],
    ledger: &mut Ledger,
    tracker: &mut Tracker,
) -> CtxResult<Vec<PendingTrial>> {
    struct Outstanding {
        candidate: String,
        arm: String,
        stage: String,
        task: String,
        rep: u32,
        split: Split,
        attempt: u32,
        ceiling: f64,
        calls: u64,
    }
    let mut queues: BTreeMap<String, VecDeque<Outstanding>> = BTreeMap::new();
    for event in events {
        match event {
            LedgerEvent::TrialScheduled {
                trial_id,
                candidate,
                arm,
                stage,
                task,
                rep,
                split,
                attempt,
                reserved_spend_usd,
                reserved_calls,
                ..
            } => {
                queues
                    .entry(trial_id.clone())
                    .or_default()
                    .push_back(Outstanding {
                        candidate: candidate.clone(),
                        arm: arm.clone(),
                        stage: stage.clone(),
                        task: task.clone(),
                        rep: *rep,
                        split: match split.as_str() {
                            "dev" => Split::Dev,
                            "validation" => Split::Validation,
                            _ => Split::Holdout,
                        },
                        attempt: *attempt,
                        ceiling: *reserved_spend_usd,
                        calls: *reserved_calls,
                    });
            }
            LedgerEvent::TrialFinished { trial_id, .. }
            | LedgerEvent::TrialFailed { trial_id, .. } => {
                if let Some(queue) = queues.get_mut(trial_id) {
                    queue.pop_front();
                }
            }
            _ => {}
        }
    }

    let mut retries = Vec::new();
    for (trial_id, mut queue) in queues {
        while let Some(outstanding) = queue.pop_front() {
            let trial_dir = trial_dir_for(campaign_dir, &trial_id, outstanding.attempt);
            if let Some(result) = TrialResult::read(&trial_dir) {
                let execution_actual = if result.cost_complete() {
                    result.cost_usd().unwrap_or(0.0)
                } else {
                    outstanding.ceiling.max(result.cost_usd().unwrap_or(0.0))
                };
                tracker.settle(
                    outstanding.ceiling,
                    outstanding.calls,
                    execution_actual + result.overhead_usd(),
                    result.calls().max(1),
                );
                let seq = ledger.next_seq();
                ledger.append(&LedgerEvent::TrialFinished {
                    seq,
                    ts: now_secs(),
                    trial_id: trial_id.clone(),
                    status: result.status.as_str().to_string(),
                    correctness: result.correctness,
                    quality: result.quality,
                    cost_usd: result.cost_usd(),
                    cost_complete: result.cost_complete(),
                    overhead_usd: result.overhead_usd(),
                    wall_ms: result.wall_ms,
                    receipts: result.receipts(),
                })?;
            } else {
                tracker.settle(
                    outstanding.ceiling,
                    outstanding.calls,
                    outstanding.ceiling,
                    0,
                );
                let retryable = outstanding.attempt < budgets.max_retries;
                let seq = ledger.next_seq();
                ledger.append(&LedgerEvent::TrialFailed {
                    seq,
                    ts: now_secs(),
                    trial_id: trial_id.clone(),
                    reason: "resume: no valid trial.json for a scheduled trial".to_string(),
                    charged_usd: outstanding.ceiling,
                    attempt: outstanding.attempt,
                    retryable,
                })?;
                if retryable {
                    retries.push(PendingTrial {
                        trial_id: trial_id.clone(),
                        candidate: outstanding.candidate,
                        arm: if outstanding.arm == "baseline" {
                            Arm::Baseline
                        } else {
                            Arm::Candidate
                        },
                        stage: match outstanding.stage.as_str() {
                            "screen" => Stage::Screen,
                            "validate" => Stage::Validate,
                            _ => Stage::Holdout,
                        },
                        task: outstanding.task,
                        rep: outstanding.rep,
                        split: outstanding.split,
                        attempt: outstanding.attempt + 1,
                    });
                }
            }
        }
    }
    Ok(retries)
}

fn build_candidate_runtimes(
    repo: &Path,
    campaign_dir: &Path,
    manifest_dir: &Path,
    baseline_sha: &str,
    manifest: &Manifest,
    protected: &[PathBuf],
) -> CtxResult<BTreeMap<String, CandidateRuntime>> {
    let mut map = BTreeMap::new();
    map.insert("baseline".to_string(), CandidateRuntime::default());
    for candidate in &manifest.candidates {
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
    manifest: &Manifest,
    candidates: &BTreeMap<String, CandidateRuntime>,
    candidate_id: &str,
) -> usize {
    let env_len = manifest
        .candidates
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

    let lock = load_or_create_lock(&repo, &manifest, &campaign_dir, &sha, resume)?;

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
    let candidates_map = build_candidate_runtimes(
        &repo,
        &campaign_dir,
        &manifest_dir,
        &lock.baseline_sha,
        &manifest,
        &protected,
    )?;

    let mut state = RunState {
        manifest: manifest.clone(),
        repo: repo.clone(),
        manifest_dir: manifest_dir.clone(),
        campaign_dir: campaign_dir.clone(),
        ledger: research_ledger,
        tracker,
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
    let cohort = cohort_key(&manifest);

    if stop_reason.is_none() {
        let seed = seed_for(&manifest.id, "baseline", "screen");
        let screen_tasks = shuffled(&task_ids(&corpus, manifest.stages.screen.split), seed);
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

    let mut survivors: Vec<String> = Vec::new();
    if stop_reason.is_none() {
        let events_now = ledger::replay(&Ledger::path(&campaign_dir))?;
        let baseline_screen_records =
            stage_records_from_ledger(&events_now, Stage::Screen, "baseline");
        let seed = seed_for(&manifest.id, "baseline", "screen");
        let screen_tasks = shuffled(&task_ids(&corpus, manifest.stages.screen.split), seed);

        for candidate in &manifest.candidates {
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
                &cohort,
                &baseline_screen_records,
                &candidate_records,
                &candidate.requires_receipts,
            );
            let verdict = promote::screen(&obs, &manifest.criteria);
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
                detail: serde_json::json!({ "reason": reason }),
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
        let validate_tasks = shuffled(&task_ids(&corpus, manifest.stages.validate.split), seed);
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
                let candidate = manifest
                    .candidates
                    .iter()
                    .find(|c| &c.id == candidate_id)
                    .expect("a survivor always came from manifest.candidates");
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
                    &cohort,
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
                    detail: serde_json::to_value(&decision)?,
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
                candidate_complexity(&manifest, &candidates_map, id),
                d,
            )
        })
        .collect();
    let winner = promote::simplest(&accepted).map(str::to_string);

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
                let holdout_tasks = task_ids(&corpus, manifest.stages.holdout.split);
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

                    let candidate = manifest
                        .candidates
                        .iter()
                        .find(|c| &c.id == winner_id)
                        .expect("winner came from manifest.candidates");
                    let events_now = ledger::replay(&Ledger::path(&campaign_dir))?;
                    let baseline_holdout_records =
                        stage_records_from_ledger(&events_now, Stage::Holdout, "baseline");
                    let candidate_holdout_records =
                        stage_records_from_ledger(&events_now, Stage::Holdout, winner_id);
                    let obs = pair_observations(
                        &cohort,
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
                    let seq = state.ledger.next_seq();
                    state.ledger.append(&LedgerEvent::StageDecision {
                        seq,
                        ts: now_secs(),
                        candidate: winner_id.clone(),
                        stage: "holdout".to_string(),
                        verdict: format!("{:?}", decision.verdict).to_lowercase(),
                        detail: serde_json::to_value(&decision)?,
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
}
