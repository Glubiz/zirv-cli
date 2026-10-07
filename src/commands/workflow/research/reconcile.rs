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
                            "orch" => Split::Orch,
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
                // Requeue an owed attempt from the ledger; no trial file exists for an attempt that never ran.
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

#[cfg(test)]
mod tests {
    use super::*;

    fn minimal_manifest() -> Manifest {
        Manifest::parse(
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
calls_per_trial = 2
timeout_secs = 30

[route]
harness = "claude"
model = "sonnet"

[budgets]
max_spend_usd = 10.0
max_wall_secs = 3600
max_calls = 100
max_trials = 20
max_retries = 1
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
        .expect("minimal manifest fixture must parse")
    }

    fn scheduled(
        trial_id: &str,
        seq: u64,
        reserved_spend_usd: f64,
        reserved_calls: u64,
    ) -> LedgerEvent {
        LedgerEvent::TrialScheduled {
            seq,
            ts: 0,
            trial_id: trial_id.to_string(),
            candidate: "baseline".to_string(),
            arm: "baseline".to_string(),
            stage: "screen".to_string(),
            task: "t1".to_string(),
            rep: 0,
            split: "dev".to_string(),
            attempt: 0,
            reserved_spend_usd,
            reserved_calls,
        }
    }

    fn finished(trial_id: &str, seq: u64, cost_usd: Option<f64>, overhead_usd: f64) -> LedgerEvent {
        LedgerEvent::TrialFinished {
            seq,
            ts: 0,
            trial_id: trial_id.to_string(),
            status: "ok".to_string(),
            correctness: Some(1.0),
            quality: Some(1.0),
            cost_usd,
            cost_complete: true,
            overhead_usd,
            wall_ms: 10,
            receipts: BTreeMap::new(),
        }
    }

    fn failed(
        trial_id: &str,
        seq: u64,
        charged_usd: f64,
        attempt: u32,
        retryable: bool,
    ) -> LedgerEvent {
        LedgerEvent::TrialFailed {
            seq,
            ts: 0,
            trial_id: trial_id.to_string(),
            reason: "boom".to_string(),
            charged_usd,
            attempt,
            retryable,
        }
    }

    #[test]
    fn reconstruct_tracker_rebuilds_spend_and_calls_from_scheduled_finished_and_failed_events() {
        let events = vec![
            scheduled("trial-a", 0, 1.0, 2),
            finished("trial-a", 1, Some(0.4), 0.1),
            scheduled("trial-b", 2, 2.0, 3),
            failed("trial-b", 3, 2.0, 0, false),
        ];

        let tracker = reconstruct_tracker(0, &events);

        assert_eq!(tracker.reserved_usd, 0.0);
        assert_eq!(tracker.reserved_calls, 0);
        // trial-a settles its own cost_usd + overhead_usd (0.4 + 0.1); trial-b
        // is charged its full reservation ceiling (2.0) on failure.
        assert!(
            (tracker.spent_usd - 2.5).abs() < 1e-9,
            "expected 2.5, got {}",
            tracker.spent_usd
        );
        // trial-a's finish charges its reserved_calls (2); a failure charges
        // zero actual calls.
        assert_eq!(tracker.calls_used, 2);
    }

    #[test]
    fn reconstruct_tracker_leaves_an_unresolved_scheduled_trial_reserved() {
        let events = vec![scheduled("trial-a", 0, 1.5, 4)];

        let tracker = reconstruct_tracker(0, &events);

        assert_eq!(tracker.reserved_usd, 1.5);
        assert_eq!(tracker.reserved_calls, 4);
        assert_eq!(tracker.spent_usd, 0.0);
        assert_eq!(tracker.calls_used, 0);
    }

    #[test]
    fn terminal_trial_ids_includes_finished_and_permanently_failed_but_excludes_started_only() {
        let campaign_dir = tempfile::tempdir().unwrap();
        let (ledger, _) = Ledger::open(campaign_dir.path()).unwrap();

        ledger
            .append(&scheduled("trial-finished", 0, 1.0, 1))
            .unwrap();
        ledger
            .append(&finished("trial-finished", 1, Some(0.1), 0.0))
            .unwrap();

        ledger.append(&scheduled("trial-dead", 2, 1.0, 1)).unwrap();
        ledger
            .append(&failed("trial-dead", 3, 1.0, 1, false))
            .unwrap();

        ledger
            .append(&scheduled("trial-retrying", 4, 1.0, 1))
            .unwrap();
        ledger
            .append(&failed("trial-retrying", 5, 1.0, 0, true))
            .unwrap();

        ledger
            .append(&scheduled("trial-started-only", 6, 1.0, 1))
            .unwrap();

        let terminal = terminal_trial_ids(campaign_dir.path()).unwrap();

        assert!(terminal.contains("trial-finished"));
        assert!(terminal.contains("trial-dead"));
        assert!(!terminal.contains("trial-retrying"));
        assert!(!terminal.contains("trial-started-only"));
    }

    fn write_lock(campaign_dir: &Path, manifest_sha256: &str) {
        let lock = Lock {
            manifest: minimal_manifest(),
            manifest_path: PathBuf::from("manifest.toml"),
            repo: PathBuf::from("."),
            manifest_sha256: manifest_sha256.to_string(),
            baseline_sha: "deadbeef".to_string(),
            corpus_version: String::new(),
            corpus_families: Vec::new(),
            evaluator_version: None,
            evaluator_files: BTreeMap::new(),
            evaluator_fingerprint: String::new(),
            zirv_version: "0.0.0".to_string(),
            price_as_of: None,
            started_at: 0,
        };
        lock.write(campaign_dir).unwrap();
    }

    #[test]
    fn load_or_create_lock_refuses_resume_when_the_manifest_changed() {
        let campaign_dir = tempfile::tempdir().unwrap();
        write_lock(campaign_dir.path(), "original-sha");
        let manifest = minimal_manifest();

        let err = load_or_create_lock(
            Path::new("."),
            &manifest,
            Path::new("manifest.toml"),
            campaign_dir.path(),
            "a-different-sha",
            true,
        )
        .expect_err("a changed manifest sha must be refused under --resume");

        assert!(
            err.to_string().contains("manifest has changed"),
            "got: {err}"
        );
    }

    #[test]
    fn load_or_create_lock_refuses_a_fresh_run_when_a_lock_already_exists() {
        let campaign_dir = tempfile::tempdir().unwrap();
        write_lock(campaign_dir.path(), "some-sha");
        let manifest = minimal_manifest();

        let err = load_or_create_lock(
            Path::new("."),
            &manifest,
            Path::new("manifest.toml"),
            campaign_dir.path(),
            "some-sha",
            false,
        )
        .expect_err("an existing lock.json without --resume must be refused");

        assert!(err.to_string().contains("--resume"), "got: {err}");
    }

    #[test]
    fn load_or_create_lock_refuses_resume_when_no_lock_exists() {
        let campaign_dir = tempfile::tempdir().unwrap();
        let manifest = minimal_manifest();

        let err = load_or_create_lock(
            Path::new("."),
            &manifest,
            Path::new("manifest.toml"),
            campaign_dir.path(),
            "any-sha",
            true,
        )
        .expect_err("--resume with no lock.json must be refused");

        assert!(err.to_string().contains("no lock.json"), "got: {err}");
    }
}
