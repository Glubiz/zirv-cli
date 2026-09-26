//! Trial-list construction, cohort/observation building, and the
//! concurrency-bounded dispatch loop -- issue #802's scheduling seam,
//! split out of `run.rs` (which keeps the core types and the top-level
//! `execute` orchestration). No behaviour change from the code that used
//! to live here; see `run.rs`'s own module doc comment for the campaign
//! loop this feeds.

use std::collections::{BTreeMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::mpsc;

use super::backend::{self, RouteSpec, TrialOutcome, TrialSpec};
use super::corpus::Corpus;
use super::ledger::LedgerEvent;
use super::manifest::{self, Manifest, Split};
use super::promote::{self, Observation};
use super::reconcile::{ensure_no_drift, terminal_trial_ids};
use super::run::{Arm, CandidateRuntime, PendingTrial, RunState, Stage, TRIALS_DIR, TrialRecord};
use crate::commands::ctx::CtxResult;
use crate::commands::ctx::state::now_secs;

pub(crate) fn sanitize(text: &str) -> String {
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

pub(crate) fn seed_for(campaign_id: &str, candidate_id: &str, stage: &str) -> u64 {
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
pub(crate) fn shuffled(tasks: &[String], seed: u64) -> Vec<String> {
    let mut items = tasks.to_vec();
    let mut state = seed;
    for i in (1..items.len()).rev() {
        let r = splitmix64(&mut state);
        let j = (r % (i as u64 + 1)) as usize;
        items.swap(i, j);
    }
    items
}

pub(crate) fn task_ids(corpus: &Corpus, split: Split, classes: &[String]) -> Vec<String> {
    corpus
        .tasks_for_split_and_classes(split, classes)
        .into_iter()
        .map(|task| task.id.clone())
        .collect()
}

pub(crate) fn trials_for(
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

/// One uncounted `Stage::Warmup` trial for `(candidate, arm)`, dispatched
/// once before any of that arm's real trials under `cache_mode = "warm"`.
/// `rep = 0` and `attempt = 0` like any other first attempt; its trial id
/// is stable across a campaign (and resume), so it is dispatched exactly
/// once per campaign, same as every other trial id.
pub(crate) fn warmup_trial(candidate: &str, arm: Arm, task: &str, split: Split) -> PendingTrial {
    PendingTrial {
        trial_id: sanitize(&format!("warmup-{candidate}-{}", arm.as_str())),
        candidate: candidate.to_string(),
        arm,
        stage: Stage::Warmup,
        task: task.to_string(),
        rep: 0,
        split,
        attempt: 0,
    }
}

/// The base cohort key, plus (issue #804) the observation's own corpus task
/// `class` appended when `manifest.stratify = "class"` -- cohorts are never
/// pooled, so a routing/gate change that helps `bounded` work while hurting
/// `architecture` work is reported as two separate verdicts, not averaged
/// into one misleading one. `task_class` is `None` only when `stratify =
/// "class"` and the task is missing from the loaded corpus (should not
/// happen in practice; falls back to an explicit `unknown` bucket rather
/// than silently reusing another class's cohort).
pub(crate) fn cohort_key(manifest: &Manifest, task_class: Option<&str>) -> String {
    let base = format!(
        "{}:{}:{}:{}",
        manifest.runtime.as_str(),
        manifest.route.model,
        manifest.cache_mode.as_str(),
        manifest.cohort.pressure.as_str()
    );
    match manifest.stratify {
        manifest::Stratify::None => base,
        manifest::Stratify::Class => format!("{base}:{}", task_class.unwrap_or("unknown")),
    }
}

pub(crate) fn build_task_classes(corpus: &Corpus) -> BTreeMap<String, String> {
    corpus
        .tasks
        .iter()
        .map(|task| (task.id.clone(), task.class.clone()))
        .collect()
}

pub(crate) fn trial_dir_for(campaign_dir: &Path, trial_id: &str, attempt: u32) -> PathBuf {
    campaign_dir
        .join(TRIALS_DIR)
        .join(trial_id)
        .join(format!("attempt-{attempt}"))
}

pub(crate) fn strategy_json(strategy: &manifest::Strategy) -> serde_json::Value {
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

pub(crate) fn to_observation(
    manifest: &Manifest,
    task_classes: &BTreeMap<String, String>,
    required_receipts: &[String],
    pair_fingerprint: Option<&str>,
    record: &TrialRecord,
) -> Observation {
    let cohort = cohort_key(manifest, task_classes.get(&record.task).map(String::as_str));
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
        cohort,
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

pub(crate) fn pair_observations(
    manifest: &Manifest,
    task_classes: &BTreeMap<String, String>,
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
        .map(|r| to_observation(manifest, task_classes, &[], None, r))
        .collect();
    for record in candidate {
        let pair_fp = baseline_by_key
            .get(&(record.task.clone(), record.rep))
            .and_then(|b| b.env_fingerprint.as_deref());
        obs.push(to_observation(
            manifest,
            task_classes,
            required_receipts,
            pair_fp,
            record,
        ));
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
    // The highest attempt ever actually scheduled for each trial id -- used
    // below to tell a genuinely exhausted/terminal failure (`retryable:
    // false`) apart from a *retryable* failure whose promised retry was
    // never dispatched (a budget stop right after the crash orphaned it in
    // the scheduler's in-memory queue; see `reconcile_unfinished`'s own
    // fix). Both must count as a terminal crash here, or the trial silently
    // vanishes from every downstream pair count instead of being charged
    // and recorded.
    let mut max_scheduled_attempt: BTreeMap<String, u32> = BTreeMap::new();
    for event in events {
        if let LedgerEvent::TrialScheduled {
            trial_id, attempt, ..
        } = event
        {
            let entry = max_scheduled_attempt
                .entry(trial_id.clone())
                .or_insert(*attempt);
            if *attempt > *entry {
                *entry = *attempt;
            }
        }
    }
    for event in events {
        if let LedgerEvent::TrialFailed {
            trial_id,
            retryable,
            attempt,
            ..
        } = event
            && !records.contains_key(trial_id)
            && let Some(m) = meta.get(trial_id)
            && m.stage == stage.as_str()
            && m.candidate == candidate
        {
            // Orphaned iff this failed attempt is the last one ever
            // scheduled for the trial id -- nothing came after it to
            // supersede it (a later, higher-attempt schedule means this
            // older failure was already retried for real, and that newer
            // attempt's own outcome is what should count instead).
            let orphaned_retry = *retryable && max_scheduled_attempt.get(trial_id) == Some(attempt);
            if !*retryable || orphaned_retry {
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
    }
    records.into_values().collect()
}

/// Runs at most `budgets.concurrency` trials at once. A worker thread never
/// touches `state`; it only runs the backend and reports back over a
/// channel, so the scheduler loop (the only place that reserves budget and
/// writes the ledger) stays single-threaded even though trials run in
/// parallel. Returns the campaign-stop reason, if scheduling a trial would
/// have exceeded a budget cap.
pub(crate) fn dispatch_batch(
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
                // Re-hash protected files before every single trial dispatch
                // (issue-review finding R1), not just at stage boundaries:
                // a concurrent batch's trials can run for a long time, and
                // tampering mid-stage must be caught before the *next*
                // trial goes out, not only after the whole stage finishes
                // and a verdict has already been computed. On drift this
                // appends `campaign_stopped { evaluator_tampered }` and
                // returns `Err`, which aborts this batch (and, via `?` at
                // every call site, the whole campaign) before this trial --
                // or any trial after it -- is ever spawned.
                if let Err(err) = ensure_no_drift(&repo, &state.lock, &mut state.ledger) {
                    dispatch_err = Some(err);
                    break 'outer;
                }
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
