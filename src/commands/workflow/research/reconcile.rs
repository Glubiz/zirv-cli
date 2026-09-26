//! Baseline sha/self-check/lock creation, evaluator drift detection, budget
//! reconstruction from a replayed ledger, and resume reconciliation of a
//! scheduled-but-unfinished trial -- issue #802's resume seam, split out of
//! `run.rs`. No behaviour change from the code that used to live here.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::path::{Path, PathBuf};
use std::process::Command;

use super::backend::TrialResult;
use super::budget::Tracker;
use super::corpus::Corpus;
use super::guard;
use super::ledger::{self, Ledger, LedgerEvent, Lock};
use super::manifest::{self, Manifest, Split};
use super::run::{Arm, PendingTrial, Stage};
use super::schedule::trial_dir_for;
use crate::commands::ctx::CtxResult;
use crate::commands::ctx::state::now_secs;

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

pub(crate) fn resolve_protected_paths(repo: &Path, manifest: &Manifest) -> CtxResult<Vec<PathBuf>> {
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

pub(crate) fn ensure_no_drift(repo: &Path, lock: &Lock, ledger: &mut Ledger) -> CtxResult<()> {
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

pub(crate) fn load_or_create_lock(
    repo: &Path,
    manifest: &Manifest,
    manifest_path: &Path,
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
        let corpus = Corpus::load(&repo.join(&manifest.corpus.file))?;
        let corpus_version = corpus.version;
        let mut corpus_families: Vec<String> =
            corpus.tasks.iter().map(|t| t.family.clone()).collect();
        corpus_families.sort();
        corpus_families.dedup();
        let lock = Lock {
            manifest: manifest.clone(),
            manifest_path: manifest_path.to_path_buf(),
            repo: repo.to_path_buf(),
            manifest_sha256: sha.to_string(),
            baseline_sha,
            corpus_version,
            corpus_families,
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

/// Every trial id that has already reached a terminal state (finished, or
/// permanently failed past `max_retries`) anywhere in the campaign's
/// ledger -- read fresh each time so a resumed run's re-derived stage lists
/// never re-dispatch one, whichever stage rebuilt it.
pub(crate) fn terminal_trial_ids(
    campaign_dir: &Path,
) -> CtxResult<std::collections::BTreeSet<String>> {
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

/// Rebuilds spend/call/trial accounting from a replayed ledger. A scheduled
/// trial with no terminal event yet stays "reserved" here -- exactly as it
/// was mid-run -- until `reconcile_unfinished` resolves it.
pub(crate) fn reconstruct_tracker(started_at: u64, events: &[LedgerEvent]) -> Tracker {
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
pub(crate) fn reconcile_unfinished(
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
    // Trial ids whose *most recent* event is a retryable `trial_failed` with
    // nothing scheduled after it: the scheduler queued a retry in memory
    // (`dispatch_batch`'s own `queue.push_back`) but a budget stop -- or
    // this whole process dying -- meant it was never actually redispatched,
    // so no `trial_scheduled` for the next attempt ever reached the ledger.
    // Left alone, the ordinary schedule/resolve accounting below would see
    // the original `trial_scheduled` matched by this `trial_failed` and
    // treat the trial as fully resolved, silently dropping the retry it is
    // still owed. These are queued for a real redispatch below instead of
    // being probed for a `trial.json` that was never written.
    let mut needs_redispatch: BTreeSet<String> = BTreeSet::new();
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
                needs_redispatch.remove(trial_id);
            }
            LedgerEvent::TrialFinished { trial_id, .. } => {
                if let Some(queue) = queues.get_mut(trial_id) {
                    queue.pop_front();
                }
                needs_redispatch.remove(trial_id);
            }
            LedgerEvent::TrialFailed {
                trial_id,
                retryable: false,
                ..
            } => {
                if let Some(queue) = queues.get_mut(trial_id) {
                    queue.pop_front();
                }
                needs_redispatch.remove(trial_id);
            }
            LedgerEvent::TrialFailed {
                trial_id,
                retryable: true,
                ..
            } => {
                // Bump the still-queued schedule entry to the attempt it is
                // now owed, and mark it as already known (from the ledger)
                // to need a real redispatch -- not a `trial.json` check, one
                // was never written for an attempt that never ran.
                if let Some(entry) = queues.get_mut(trial_id).and_then(VecDeque::back_mut) {
                    entry.attempt += 1;
                    needs_redispatch.insert(trial_id.clone());
                }
            }
            _ => {}
        }
    }

    let mut retries = Vec::new();
    for (trial_id, mut queue) in queues {
        if needs_redispatch.contains(&trial_id) {
            while let Some(outstanding) = queue.pop_front() {
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
                        "warmup" => Stage::Warmup,
                        _ => Stage::Holdout,
                    },
                    task: outstanding.task,
                    rep: outstanding.rep,
                    split: outstanding.split,
                    attempt: outstanding.attempt,
                });
            }
            continue;
        }
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
                            "warmup" => Stage::Warmup,
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
