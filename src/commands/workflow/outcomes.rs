//! Issue #757: the outcome feedback loop.
//!
//! When a workflow reaches a terminal state (`Completed`, `Failed`, or
//! `Closed`), [`record_terminal`] appends ONE metadata-only row to a
//! daily-bucketed jsonl log under the state directory. `zirv workflow
//! calibrate` ([`run_calibrate`]) aggregates those rows by complexity x
//! profile x seat tier and PROPOSES a one-step heavier or lighter routing
//! from the documented rule table below. It is strictly read-only: it never
//! writes config and never changes routing -- a human applies (or ignores)
//! every proposal.
//!
//! Rows carry no task text, prompt, path, or repository name: only enums,
//! counts, the workflow's random id, and its pack id.

use std::collections::BTreeMap;
use std::io::Write;
use std::path::PathBuf;

use clap::Args;
use serde::{Deserialize, Serialize};

use super::classify::{Complexity, RiskBand};
use super::engine::{WorkflowProfile, WorkflowState, WorkflowStatus};
use super::skill::WorkflowPhase;
use crate::commands::ctx::CtxResult;
use crate::commands::ctx::proxy::decision::SeatTier;
use crate::commands::ctx::state::{StateDir, create_private_dir_all, now_secs};

pub const OUTCOME_SCHEMA_VERSION: u32 = 1;
/// `<state>/logs/workflow-outcomes/{day:010}.jsonl`, the same daily-bucket
/// layout (and pruner) the safety-decision log uses.
pub const OUTCOMES_DIR: &str = "workflow-outcomes";
/// Longer than the safety log's 30 days: calibration needs many samples per
/// bucket, and a row is ~300 bytes written once per workflow.
pub const OUTCOME_RETENTION_DAYS: u64 = 365;
const MAX_PACK_BYTES: usize = 128;

/// Default `--min-samples`: a bucket smaller than this never gets a proposal.
pub const DEFAULT_MIN_SAMPLES: usize = 10;
/// Heavier when the first-pass verification rate is BELOW this...
pub const HEAVIER_FIRST_PASS_BELOW: f64 = 0.60;
/// ...or the mean review rounds is AT LEAST this.
pub const HEAVIER_MEAN_REVIEW_ROUNDS_AT_LEAST: f64 = 2.0;
/// Lighter only when the first-pass rate is AT LEAST this...
pub const LIGHTER_FIRST_PASS_AT_LEAST: f64 = 0.95;
/// ...AND the mean review rounds is AT MOST this.
pub const LIGHTER_MEAN_REVIEW_ROUNDS_AT_MOST: f64 = 0.2;

/// One terminal workflow, metadata only.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OutcomeRow {
    pub schema_version: u32,
    pub ts: u64,
    pub workflow_id: String,
    /// The pinned definition pack id, or the legacy kind id without one.
    pub pack: String,
    pub profile: WorkflowProfile,
    pub complexity: Complexity,
    pub risk: RiskBand,
    /// The proxy's seat tier when the workflow knows it. Workflow state does
    /// not carry it today, so this is `None` until it does.
    pub seat_tier: Option<SeatTier>,
    /// The highest review round any recorded review run reached (0 = none).
    pub review_rounds: u8,
    /// `None` when no Test/Verify step was ever attempted.
    pub verification_first_attempt: Option<bool>,
    pub verification_passed: Option<bool>,
    /// `completed`, `failed`, or `closed` (the latter two are "abandoned").
    pub terminal: WorkflowStatus,
    pub duration_secs: u64,
}

impl OutcomeRow {
    pub fn from_state(state: &WorkflowState) -> Self {
        let pack = state
            .definition
            .as_ref()
            .map(|definition| definition.id.clone())
            .unwrap_or_else(|| state.kind.as_str().to_string());
        let (verification_first_attempt, verification_passed) = verification_outcome(state);
        Self {
            schema_version: OUTCOME_SCHEMA_VERSION,
            ts: now_secs(),
            workflow_id: state.id.clone(),
            pack: crate::utils::truncate_bytes(pack, Some(MAX_PACK_BYTES)),
            profile: state.profile,
            complexity: state.classification.complexity,
            risk: state.classification.risk,
            seat_tier: None,
            review_rounds: state
                .review_evidence
                .iter()
                .map(|evidence| evidence.review_round)
                .max()
                .unwrap_or(0),
            verification_first_attempt,
            verification_passed,
            terminal: state.status,
            duration_secs: state.updated_at.saturating_sub(state.created_at),
        }
    }
}

/// `(passed on first attempt, passed at all)` over the workflow's Test/Verify
/// steps. `attempts` counts failed attempts only, so a step with a non-zero
/// entry failed at least once.
fn verification_outcome(state: &WorkflowState) -> (Option<bool>, Option<bool>) {
    let steps: Vec<_> = state
        .steps
        .iter()
        .filter(|step| matches!(step.phase, WorkflowPhase::Test | WorkflowPhase::Verify))
        .collect();
    let failures = |id: &str| state.attempts.get(id).copied().unwrap_or(0);
    let completed = |id: &str| state.completed_steps.iter().any(|done| done == id);
    let attempted = steps
        .iter()
        .any(|step| completed(&step.id) || failures(&step.id) > 0);
    if !attempted {
        return (None, None);
    }
    let passed = steps.iter().all(|step| completed(&step.id));
    let any_failure = steps.iter().any(|step| failures(&step.id) > 0);
    (Some(passed && !any_failure), Some(passed))
}

fn outcomes_dir(state: &StateDir) -> PathBuf {
    state.logs().join(OUTCOMES_DIR)
}

pub fn append(state: &StateDir, row: &OutcomeRow) -> CtxResult<()> {
    let dir = outcomes_dir(state);
    create_private_dir_all(&dir)?;
    let day = row.ts / 86_400;
    let mut file =
        crate::commands::ctx::state::open_private_append(&dir.join(format!("{day:010}.jsonl")))?;
    writeln!(file, "{}", serde_json::to_string(row)?)?;
    drop(file);
    crate::commands::ctx::log::prune_day_buckets(&dir, day, OUTCOME_RETENTION_DAYS);
    Ok(())
}

/// Called right after a transition persisted `state`. A no-op unless the
/// status is terminal and the operator's workflow telemetry is enabled (the
/// same `[workflow] telemetry_enabled` switch). Best-effort: callers ignore
/// the error, so recording can never fail a transition.
pub fn record_terminal(state_dir: &StateDir, state: &WorkflowState) -> CtxResult<()> {
    if !matches!(
        state.status,
        WorkflowStatus::Completed | WorkflowStatus::Failed | WorkflowStatus::Closed
    ) {
        return Ok(());
    }
    if !super::telemetry::TelemetryConfig::for_repo(&state.repo).enabled {
        return Ok(());
    }
    append(state_dir, &OutcomeRow::from_state(state))
}

/// Every parseable current-schema row, oldest bucket first. A corrupt line or
/// an unreadable bucket is skipped, never fatal.
pub fn read_all(state: &StateDir) -> Vec<OutcomeRow> {
    let Ok(entries) = std::fs::read_dir(outcomes_dir(state)) else {
        return Vec::new();
    };
    let mut paths: Vec<PathBuf> = entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.extension().and_then(|ext| ext.to_str()) == Some("jsonl"))
        .collect();
    paths.sort();
    paths
        .iter()
        .filter_map(|path| std::fs::read_to_string(path).ok())
        .flat_map(|text| {
            text.lines()
                .filter_map(|line| serde_json::from_str::<OutcomeRow>(line).ok())
                .filter(|row| row.schema_version == OUTCOME_SCHEMA_VERSION)
                .collect::<Vec<_>>()
        })
        .collect()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Verdict {
    Heavier,
    Lighter,
    NoChange,
    InsufficientEvidence,
}

/// Which routing knob a proposal moves: the seat tier when the bucket knows
/// it, otherwise the complexity class (`classify.rs` thresholds).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Knob {
    SeatTier,
    Complexity,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Proposal {
    pub verdict: Verdict,
    pub knob: Knob,
    pub from: String,
    /// `None` for no-change/insufficient evidence, or when the bucket is
    /// already on the heaviest/lightest rung.
    pub to: Option<String>,
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct BucketReport {
    pub complexity: Complexity,
    pub profile: WorkflowProfile,
    pub seat_tier: Option<SeatTier>,
    pub samples: usize,
    pub completed: usize,
    pub abandoned: usize,
    /// Rows where verification was attempted -- the first-pass denominator.
    pub verification_samples: usize,
    pub first_pass_rate: Option<f64>,
    pub mean_review_rounds: f64,
    pub abandon_rate: f64,
    pub proposal: Proposal,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Thresholds {
    pub heavier_first_pass_below: f64,
    pub heavier_mean_review_rounds_at_least: f64,
    pub lighter_first_pass_at_least: f64,
    pub lighter_mean_review_rounds_at_most: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct CalibrationReport {
    pub schema_version: u32,
    pub min_samples: usize,
    pub total_samples: usize,
    pub thresholds: Thresholds,
    pub buckets: Vec<BucketReport>,
}

const SEAT_TIERS: [SeatTier; 4] = [
    SeatTier::Cheap,
    SeatTier::Standard,
    SeatTier::Deep,
    SeatTier::Frontier,
];
const COMPLEXITIES: [Complexity; 4] = [
    Complexity::Trivial,
    Complexity::Bounded,
    Complexity::Substantial,
    Complexity::Architectural,
];

fn label<T: Serialize>(value: &T) -> String {
    serde_json::to_value(value)
        .ok()
        .and_then(|value| value.as_str().map(str::to_string))
        .unwrap_or_else(|| "?".to_string())
}

/// The neighbouring rung, one step heavier (`+1`) or lighter (`-1`).
fn step<T: Copy + PartialEq>(ladder: &[T], current: T, heavier: bool) -> Option<T> {
    let index = ladder.iter().position(|rung| *rung == current)?;
    let next = if heavier {
        index.checked_add(1)?
    } else {
        index.checked_sub(1)?
    };
    ladder.get(next).copied()
}

fn pct(rate: f64) -> String {
    format!("{:.0}%", rate * 100.0)
}

/// The documented rule table, pure over one bucket's figures.
pub fn propose(
    complexity: Complexity,
    seat_tier: Option<SeatTier>,
    samples: usize,
    first_pass_rate: Option<f64>,
    mean_review_rounds: f64,
    min_samples: usize,
) -> Proposal {
    let (knob, from) = match seat_tier {
        Some(tier) => (Knob::SeatTier, tier.label().to_string()),
        None => (Knob::Complexity, label(&complexity)),
    };
    let proposal = |verdict, to, reason: String| Proposal {
        verdict,
        knob,
        from: from.clone(),
        to,
        reason,
    };
    if samples < min_samples {
        return proposal(
            Verdict::InsufficientEvidence,
            None,
            format!("{samples} sample(s) < {min_samples}"),
        );
    }
    let low_first_pass = first_pass_rate.filter(|rate| *rate < HEAVIER_FIRST_PASS_BELOW);
    let many_rounds = mean_review_rounds >= HEAVIER_MEAN_REVIEW_ROUNDS_AT_LEAST;
    let (verdict, reason) = if low_first_pass.is_some() || many_rounds {
        let mut reasons = Vec::new();
        if let Some(rate) = low_first_pass {
            reasons.push(format!(
                "first-pass {} < {}",
                pct(rate),
                pct(HEAVIER_FIRST_PASS_BELOW)
            ));
        }
        if many_rounds {
            reasons.push(format!(
                "mean review rounds {mean_review_rounds:.1} >= {HEAVIER_MEAN_REVIEW_ROUNDS_AT_LEAST:.1}"
            ));
        }
        (Verdict::Heavier, reasons.join(", "))
    } else if let Some(rate) = first_pass_rate.filter(|rate| {
        *rate >= LIGHTER_FIRST_PASS_AT_LEAST
            && mean_review_rounds <= LIGHTER_MEAN_REVIEW_ROUNDS_AT_MOST
    }) {
        (
            Verdict::Lighter,
            format!(
                "first-pass {} >= {} and mean review rounds {mean_review_rounds:.1} <= {LIGHTER_MEAN_REVIEW_ROUNDS_AT_MOST:.1}",
                pct(rate),
                pct(LIGHTER_FIRST_PASS_AT_LEAST)
            ),
        )
    } else {
        return proposal(Verdict::NoChange, None, "within thresholds".to_string());
    };
    let heavier = verdict == Verdict::Heavier;
    let to = match seat_tier {
        Some(tier) => step(&SEAT_TIERS, tier, heavier).map(|tier| tier.label().to_string()),
        None => step(&COMPLEXITIES, complexity, heavier).map(|next| label(&next)),
    };
    let reason = if to.is_none() {
        format!(
            "{reason}; already on the {} rung",
            if heavier { "heaviest" } else { "lightest" }
        )
    } else {
        reason
    };
    proposal(verdict, to, reason)
}

pub fn calibrate(rows: &[OutcomeRow], min_samples: usize) -> CalibrationReport {
    type Key = (Complexity, String, String);
    let mut groups: BTreeMap<Key, Vec<&OutcomeRow>> = BTreeMap::new();
    for row in rows {
        let tier = row.seat_tier.map(|tier| {
            SEAT_TIERS
                .iter()
                .position(|rung| *rung == tier)
                .unwrap_or(0)
                .to_string()
        });
        groups
            .entry((
                row.complexity,
                label(&row.profile),
                tier.unwrap_or_default(),
            ))
            .or_default()
            .push(row);
    }
    let buckets = groups
        .into_values()
        .map(|group| {
            let first = group[0];
            let samples = group.len();
            let completed = group
                .iter()
                .filter(|row| row.terminal == WorkflowStatus::Completed)
                .count();
            let verified: Vec<bool> = group
                .iter()
                .filter_map(|row| row.verification_first_attempt)
                .collect();
            let first_pass_rate = (!verified.is_empty()).then(|| {
                verified.iter().filter(|passed| **passed).count() as f64 / verified.len() as f64
            });
            let mean_review_rounds = group
                .iter()
                .map(|row| f64::from(row.review_rounds))
                .sum::<f64>()
                / samples as f64;
            BucketReport {
                complexity: first.complexity,
                profile: first.profile,
                seat_tier: first.seat_tier,
                samples,
                completed,
                abandoned: samples - completed,
                verification_samples: verified.len(),
                first_pass_rate,
                mean_review_rounds,
                abandon_rate: (samples - completed) as f64 / samples as f64,
                proposal: propose(
                    first.complexity,
                    first.seat_tier,
                    samples,
                    first_pass_rate,
                    mean_review_rounds,
                    min_samples,
                ),
            }
        })
        .collect();
    CalibrationReport {
        schema_version: OUTCOME_SCHEMA_VERSION,
        min_samples,
        total_samples: rows.len(),
        thresholds: Thresholds {
            heavier_first_pass_below: HEAVIER_FIRST_PASS_BELOW,
            heavier_mean_review_rounds_at_least: HEAVIER_MEAN_REVIEW_ROUNDS_AT_LEAST,
            lighter_first_pass_at_least: LIGHTER_FIRST_PASS_AT_LEAST,
            lighter_mean_review_rounds_at_most: LIGHTER_MEAN_REVIEW_ROUNDS_AT_MOST,
        },
        buckets,
    }
}

#[derive(Debug, Args)]
pub struct CalibrateArgs {
    #[arg(long)]
    pub json: bool,
    /// A bucket needs at least this many outcomes before any proposal.
    #[arg(long, default_value_t = DEFAULT_MIN_SAMPLES)]
    pub min_samples: usize,
}

fn render_text(report: &CalibrationReport, writer: &mut impl Write) -> CtxResult<()> {
    writeln!(
        writer,
        "workflow calibrate: {} outcome(s), min samples {} (read-only: proposals only, routing unchanged)",
        report.total_samples, report.min_samples
    )?;
    if report.buckets.is_empty() {
        writeln!(
            writer,
            "no outcomes recorded yet; rows are appended when a workflow completes, fails, or is closed"
        )?;
    }
    for bucket in &report.buckets {
        let proposal = &bucket.proposal;
        let action = match (proposal.verdict, proposal.to.as_deref()) {
            (Verdict::Heavier | Verdict::Lighter, Some(to)) => format!(
                "{}: {} {} -> {to}",
                label(&proposal.verdict),
                label(&proposal.knob),
                proposal.from
            ),
            (verdict, _) => label(&verdict),
        };
        writeln!(
            writer,
            "{}/{}/{}: n={} first-pass {} review rounds {:.1} abandon {} -> {action} ({})",
            label(&bucket.complexity),
            label(&bucket.profile),
            bucket.seat_tier.map_or("tier ?", |tier| tier.label()),
            bucket.samples,
            bucket
                .first_pass_rate
                .map_or_else(|| "n/a".to_string(), pct),
            bucket.mean_review_rounds,
            pct(bucket.abandon_rate),
            proposal.reason
        )?;
    }
    Ok(())
}

pub fn run_calibrate(args: &CalibrateArgs, writer: &mut impl Write) -> CtxResult<i32> {
    let state = StateDir::resolve(&|key| std::env::var(key).ok())?;
    let report = calibrate(&read_all(&state), args.min_samples.max(1));
    if args.json {
        writeln!(writer, "{}", serde_json::to_string_pretty(&report)?)?;
    } else {
        render_text(&report, writer)?;
    }
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(complexity: Complexity, first_pass: Option<bool>, rounds: u8) -> OutcomeRow {
        OutcomeRow {
            schema_version: OUTCOME_SCHEMA_VERSION,
            ts: 1_700_000_000,
            workflow_id: "wf".into(),
            pack: "feature".into(),
            profile: WorkflowProfile::Standard,
            complexity,
            risk: RiskBand::Low,
            seat_tier: None,
            review_rounds: rounds,
            verification_first_attempt: first_pass,
            verification_passed: first_pass.map(|_| true),
            terminal: WorkflowStatus::Completed,
            duration_secs: 60,
        }
    }

    fn many(n: usize, complexity: Complexity, first_pass: bool, rounds: u8) -> Vec<OutcomeRow> {
        (0..n)
            .map(|_| row(complexity, Some(first_pass), rounds))
            .collect()
    }

    #[test]
    fn rule_table_yields_heavier_lighter_and_no_change() {
        // 5/12 first-pass -> heavier.
        let mut rows = many(5, Complexity::Bounded, true, 0);
        rows.extend(many(7, Complexity::Bounded, false, 1));
        // All first-pass, zero review rounds -> lighter.
        rows.extend(many(10, Complexity::Substantial, true, 0));
        // 80% first-pass, 1 round -> no change.
        rows.extend(many(8, Complexity::Trivial, true, 1));
        rows.extend(many(2, Complexity::Trivial, false, 1));
        let report = calibrate(&rows, DEFAULT_MIN_SAMPLES);
        let by = |c: Complexity| {
            report
                .buckets
                .iter()
                .find(|b| b.complexity == c)
                .unwrap()
                .proposal
                .clone()
        };
        let heavier = by(Complexity::Bounded);
        assert_eq!(heavier.verdict, Verdict::Heavier);
        assert_eq!(heavier.knob, Knob::Complexity);
        assert_eq!(heavier.to.as_deref(), Some("substantial"));
        let lighter = by(Complexity::Substantial);
        assert_eq!(lighter.verdict, Verdict::Lighter);
        assert_eq!(lighter.to.as_deref(), Some("bounded"));
        assert_eq!(by(Complexity::Trivial).verdict, Verdict::NoChange);
    }

    #[test]
    fn many_review_rounds_alone_propose_heavier_seat_tier() {
        let proposal = propose(
            Complexity::Bounded,
            Some(SeatTier::Standard),
            10,
            Some(0.9),
            2.0,
            10,
        );
        assert_eq!(proposal.verdict, Verdict::Heavier);
        assert_eq!(proposal.knob, Knob::SeatTier);
        assert_eq!(proposal.to.as_deref(), Some("deep"));
        let top = propose(Complexity::Architectural, None, 10, Some(0.1), 0.0, 10);
        assert_eq!(top.verdict, Verdict::Heavier);
        assert_eq!(top.to, None, "already on the heaviest rung");
    }

    #[test]
    fn below_the_minimum_sample_count_no_proposal_is_made() {
        let rows = many(9, Complexity::Bounded, false, 3);
        let report = calibrate(&rows, 10);
        let proposal = &report.buckets[0].proposal;
        assert_eq!(proposal.verdict, Verdict::InsufficientEvidence);
        assert_eq!(proposal.to, None);
        // The same bucket at N = 9 does get one.
        assert_eq!(
            calibrate(&rows, 9).buckets[0].proposal.verdict,
            Verdict::Heavier
        );
    }

    #[test]
    fn json_shape_is_stable() {
        let mut rows = many(10, Complexity::Bounded, true, 0);
        rows[0].terminal = WorkflowStatus::Closed;
        let value = serde_json::to_value(calibrate(&rows, 10)).unwrap();
        let keys = |v: &serde_json::Value| {
            let mut keys = v.as_object().unwrap().keys().cloned().collect::<Vec<_>>();
            keys.sort();
            keys
        };
        assert_eq!(
            keys(&value),
            [
                "buckets",
                "min_samples",
                "schema_version",
                "thresholds",
                "total_samples"
            ]
        );
        let bucket = &value["buckets"][0];
        assert_eq!(
            keys(bucket),
            [
                "abandon_rate",
                "abandoned",
                "completed",
                "complexity",
                "first_pass_rate",
                "mean_review_rounds",
                "profile",
                "proposal",
                "samples",
                "seat_tier",
                "verification_samples"
            ]
        );
        assert_eq!(
            keys(&bucket["proposal"]),
            ["from", "knob", "reason", "to", "verdict"]
        );
        assert_eq!(bucket["complexity"], "bounded");
        assert_eq!(bucket["profile"], "standard");
        assert_eq!(bucket["abandoned"], 1);
        assert_eq!(bucket["proposal"]["verdict"], "lighter");
        assert_eq!(bucket["proposal"]["knob"], "complexity");
    }

    #[test]
    fn rows_round_trip_through_the_day_bucket_log() {
        let root = tempfile::tempdir().unwrap();
        let state = StateDir::from_root(root.path().to_path_buf());
        let first = row(Complexity::Bounded, Some(true), 1);
        append(&state, &first).unwrap();
        std::fs::write(outcomes_dir(&state).join("0000000001.jsonl"), "not json\n").unwrap();
        assert_eq!(read_all(&state), vec![first]);
    }
}
