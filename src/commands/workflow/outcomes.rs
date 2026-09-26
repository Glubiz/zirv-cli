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
use crate::commands::ctx::attribution::{self, Attribution};
use crate::commands::ctx::proxy::decision::SeatTier;
use crate::commands::ctx::state::{StateDir, create_private_dir_all, now_secs};

/// Issue #800: bumped 1 -> 2 to add `kind`/`session`/`attribution`/
/// `harness`/`model`/`effort`/`policy` -- every one `#[serde(default)]` so a
/// v1 row (which never wrote them) still deserializes, as the empty/`None`
/// values that are the only honest reading for a row that predates them.
pub const OUTCOME_SCHEMA_VERSION: u32 = 2;
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

/// Issue #800: whether this row is a completed/failed/closed WORKFLOW, or a
/// headless `zirv ctx exec` session that ran no workflow at all (a "direct"
/// task). `#[default]` is `Workflow` -- the only kind a v1 row (which never
/// wrote this field) could have been.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum OutcomeKind {
    #[default]
    Workflow,
    Direct,
}

/// One terminal workflow, or (issue #800) one direct headless session,
/// metadata only.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OutcomeRow {
    pub schema_version: u32,
    pub ts: u64,
    /// Empty for a `Direct` row -- there is no workflow id to carry.
    pub workflow_id: String,
    /// The pinned definition pack id, or the legacy kind id without one.
    /// Empty for a `Direct` row.
    pub pack: String,
    pub profile: WorkflowProfile,
    pub complexity: Complexity,
    pub risk: RiskBand,
    /// The proxy's ACTUAL seat tier when known -- see [`resolve_seat_tier`].
    /// `None` when neither `ZIRV_ROUTE_TIER` nor the handover ladder (from
    /// `harness`/`model`) can place it.
    pub seat_tier: Option<SeatTier>,
    /// The highest review round any recorded review run reached (0 = none).
    /// Always 0 for a `Direct` row.
    pub review_rounds: u8,
    /// `None` when no Test/Verify step was ever attempted. Always `None` for
    /// a `Direct` row (there is no workflow step to have attempted one).
    pub verification_first_attempt: Option<bool>,
    pub verification_passed: Option<bool>,
    /// `completed`, `failed`, or `closed` (the latter two are "abandoned").
    /// A `Direct` row is always `Completed` -- it exists purely to record
    /// that the session ran, never a workflow verdict.
    pub terminal: WorkflowStatus,
    pub duration_secs: u64,
    /// Issue #800.
    #[serde(default)]
    pub kind: OutcomeKind,
    /// Issue #800: `ZIRV_CTX_SESSION`, when this row's own process had one --
    /// lets `zirv ctx exec`'s own direct-row append at exit check "did THIS
    /// session already write a workflow row" before adding a redundant one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session: Option<String>,
    /// Issue #800: this process's own campaign/candidate/trial/task ids.
    #[serde(default, skip_serializing_if = "Attribution::is_empty")]
    pub attribution: Attribution,
    /// Issue #800: the actual harness this session ran (`ZIRV_CTX_AGENT`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub harness: Option<String>,
    /// Issue #800: the actual configured model (`ZIRV_ROUTE_MODEL`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Issue #800: the actual headless effort level (`ZIRV_ROUTE_EFFORT`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
    /// Issue #800: `attribution::policy_fingerprint`'s own output, so two
    /// outcomes can be compared knowing whether the SAME policy produced
    /// them.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub policy: Option<String>,
}

/// `ZIRV_ROUTE_TIER`'s own labels (`SeatTier::label`), parsed back.
fn parse_seat_tier(label: &str) -> Option<SeatTier> {
    match label {
        "cheap" => Some(SeatTier::Cheap),
        "standard" => Some(SeatTier::Standard),
        "deep" => Some(SeatTier::Deep),
        "frontier" => Some(SeatTier::Frontier),
        _ => None,
    }
}

/// Issue #800: this session's ACTUAL seat tier -- `ZIRV_ROUTE_TIER` when the
/// launch seam already resolved one, else derived from `harness`/`model`
/// through the handover ladder (`handover::tier_for_model`), else `None`.
/// Best-effort throughout: a config load failure or an unplaceable model is
/// `None`, never a guess.
fn resolve_seat_tier(cfg: Option<&crate::commands::ctx::config::CtxConfig>) -> Option<SeatTier> {
    if let Ok(raw) = std::env::var(attribution::ROUTE_TIER_ENV)
        && let Some(tier) = parse_seat_tier(raw.trim())
    {
        return Some(tier);
    }
    let cfg = cfg?;
    let harness = std::env::var(crate::commands::ctx::adapters::AGENT_ENV).ok()?;
    let model = std::env::var(attribution::ROUTE_MODEL_ENV).ok()?;
    let label = crate::commands::ctx::handover::tier_for_model(&harness, &model, cfg)?;
    parse_seat_tier(label)
}

/// Best-effort: loads the live `CtxConfig` for the current repository, for
/// [`resolve_seat_tier`]'s handover-ladder fallback and the policy
/// fingerprint. `None` on any load failure -- recording an outcome must
/// never fail a transition over a bad config layer elsewhere.
fn load_cfg_best_effort() -> Option<crate::commands::ctx::config::CtxConfig> {
    let repo = std::env::current_dir().ok()?;
    crate::commands::ctx::config::CtxConfig::load_for_launch(&repo, &|key| std::env::var(key).ok())
        .ok()
}

fn current_session() -> Option<String> {
    std::env::var(crate::commands::ctx::adapters::SESSION_ENV)
        .ok()
        .filter(|value| !value.trim().is_empty())
}

impl OutcomeRow {
    pub fn from_state(state: &WorkflowState) -> Self {
        let pack = state
            .definition
            .as_ref()
            .map(|definition| definition.id.clone())
            .unwrap_or_else(|| state.kind.as_str().to_string());
        let (verification_first_attempt, verification_passed) = verification_outcome(state);
        let cfg = load_cfg_best_effort();
        Self {
            schema_version: OUTCOME_SCHEMA_VERSION,
            ts: now_secs(),
            workflow_id: state.id.clone(),
            pack: crate::utils::truncate_bytes(pack, Some(MAX_PACK_BYTES)),
            profile: state.profile,
            complexity: state.classification.complexity,
            risk: state.classification.risk,
            seat_tier: resolve_seat_tier(cfg.as_ref()),
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
            kind: OutcomeKind::Workflow,
            session: current_session(),
            attribution: Attribution::from_env(),
            harness: std::env::var(crate::commands::ctx::adapters::AGENT_ENV).ok(),
            model: std::env::var(attribution::ROUTE_MODEL_ENV).ok(),
            effort: std::env::var(attribution::ROUTE_EFFORT_ENV).ok(),
            policy: cfg.as_ref().map(attribution::policy_fingerprint),
        }
    }

    /// Issue #800: a `Direct` row for a headless session that ran no
    /// workflow at all -- `zirv ctx exec`'s own best-effort append at exit.
    /// `harness`/`model` are the actual values the launch resolved (from its
    /// own [`super::super::ctx::exec::ExecutionReport`] segment), not
    /// re-derived from env, since a supervised run's own env may already have
    /// moved on to a later turn by the time this is called.
    pub fn direct(session: &str, harness: Option<&str>, model: Option<&str>) -> Self {
        let cfg = load_cfg_best_effort();
        let seat_tier = std::env::var(attribution::ROUTE_TIER_ENV)
            .ok()
            .and_then(|raw| parse_seat_tier(raw.trim()))
            .or_else(|| {
                let cfg = cfg.as_ref()?;
                let harness = harness?;
                let model = model?;
                parse_seat_tier(crate::commands::ctx::handover::tier_for_model(
                    harness, model, cfg,
                )?)
            });
        Self {
            schema_version: OUTCOME_SCHEMA_VERSION,
            ts: now_secs(),
            workflow_id: String::new(),
            pack: String::new(),
            profile: WorkflowProfile::default(),
            complexity: Complexity::Trivial,
            risk: RiskBand::Low,
            seat_tier,
            review_rounds: 0,
            verification_first_attempt: None,
            verification_passed: None,
            terminal: WorkflowStatus::Completed,
            duration_secs: 0,
            kind: OutcomeKind::Direct,
            session: Some(session.to_string()),
            attribution: Attribution::from_env(),
            harness: harness.map(str::to_string),
            model: model.map(str::to_string),
            effort: std::env::var(attribution::ROUTE_EFFORT_ENV).ok(),
            policy: cfg.as_ref().map(attribution::policy_fingerprint),
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

/// Every parseable row at schema 1 or 2, oldest bucket first -- issue #800
/// bumped the schema to 2, but every new field is `#[serde(default)]`, so a
/// v1 row still deserializes cleanly and is kept, not dropped. A corrupt line
/// or an unreadable bucket is skipped, never fatal.
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
                .filter(|row| matches!(row.schema_version, 1 | 2))
                .collect::<Vec<_>>()
        })
        .collect()
}

/// Whether `read_all` already holds a `Workflow` row for `session` -- `zirv
/// ctx exec`'s own best-effort exit check for whether to append a `Direct`
/// row (issue #800): a session that already ran a workflow to completion
/// must never also get a redundant direct row.
pub fn has_workflow_row_for_session(state: &StateDir, session: &str) -> bool {
    read_all(state)
        .iter()
        .any(|row| row.kind == OutcomeKind::Workflow && row.session.as_deref() == Some(session))
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
    // Issue #800: a `Direct` row carries no workflow complexity/profile/tier
    // routing decision to calibrate against -- only ever a placeholder
    // (`Complexity::Trivial`/`WorkflowProfile::default()`), so folding it in
    // here would silently contaminate the `trivial`/`standard` bucket with
    // rows this proposal table was never meant to see.
    let rows: Vec<&OutcomeRow> = rows
        .iter()
        .filter(|row| row.kind == OutcomeKind::Workflow)
        .collect();
    for row in rows.iter().copied() {
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
            kind: OutcomeKind::Workflow,
            session: None,
            attribution: Attribution::default(),
            harness: None,
            model: None,
            effort: None,
            policy: None,
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

    /// Issue #757: pins the exact `<`/`<=`/`>=` choices `propose`'s threshold
    /// comparisons use, evaluated AT the boundary value itself -- a value
    /// comfortably inside or outside a threshold would still pass even if a
    /// `<` here ever silently became a `<=` (or vice versa).
    #[test]
    fn propose_pins_exact_threshold_boundaries() {
        // Exactly at HEAVIER_FIRST_PASS_BELOW (0.60): the comparison is a
        // strict `<`, so a rate exactly on the floor must NOT by itself
        // count as "low" -- with zero review rounds this must land on
        // NoChange, never Heavier.
        let at_heavier_floor = propose(
            Complexity::Bounded,
            None,
            10,
            Some(HEAVIER_FIRST_PASS_BELOW),
            0.0,
            10,
        );
        assert_eq!(
            at_heavier_floor.verdict,
            Verdict::NoChange,
            "first-pass rate exactly at HEAVIER_FIRST_PASS_BELOW must not trigger Heavier"
        );

        // Exactly at BOTH LIGHTER_FIRST_PASS_AT_LEAST (0.95, `>=`) and
        // LIGHTER_MEAN_REVIEW_ROUNDS_AT_MOST (0.2, `<=`): both boundaries
        // are inclusive, so this combination must be Lighter.
        let at_lighter_boundary = propose(
            Complexity::Bounded,
            None,
            10,
            Some(LIGHTER_FIRST_PASS_AT_LEAST),
            LIGHTER_MEAN_REVIEW_ROUNDS_AT_MOST,
            10,
        );
        assert_eq!(at_lighter_boundary.verdict, Verdict::Lighter);

        // Exactly at HEAVIER_MEAN_REVIEW_ROUNDS_AT_LEAST (2.0, `>=`): the
        // comparison is inclusive, so this alone -- with a first-pass rate
        // far above the heavier floor, so only the rounds figure can be
        // driving it -- must be Heavier.
        let at_rounds_floor = propose(
            Complexity::Bounded,
            None,
            10,
            Some(1.0),
            HEAVIER_MEAN_REVIEW_ROUNDS_AT_LEAST,
            10,
        );
        assert_eq!(at_rounds_floor.verdict, Verdict::Heavier);
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

    /// Issue #800: a v1 row (written before `kind`/`session`/`attribution`/
    /// `harness`/`model`/`effort`/`policy` existed) must still deserialize --
    /// every new field is `#[serde(default)]`.
    #[test]
    fn a_v1_json_line_still_reads() {
        let root = tempfile::tempdir().unwrap();
        let state = StateDir::from_root(root.path().to_path_buf());
        let v1_line = r#"{"schema_version":1,"ts":1700000000,"workflow_id":"wf","pack":"feature","profile":"standard","complexity":"bounded","risk":"low","seat_tier":null,"review_rounds":1,"verification_first_attempt":true,"verification_passed":true,"terminal":"completed","duration_secs":60}"#;
        std::fs::create_dir_all(outcomes_dir(&state)).unwrap();
        std::fs::write(
            outcomes_dir(&state).join("0000000001.jsonl"),
            format!("{v1_line}\n"),
        )
        .unwrap();
        let rows = read_all(&state);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].kind, OutcomeKind::Workflow);
        assert_eq!(rows[0].session, None);
        assert!(rows[0].attribution.is_empty());
        assert_eq!(rows[0].harness, None);
        assert_eq!(rows[0].policy, None);
    }

    /// Issue #800: a v2 row with every new field populated round-trips
    /// through JSON exactly.
    #[test]
    fn a_v2_row_round_trips() {
        let mut r = row(Complexity::Bounded, Some(true), 1);
        r.kind = OutcomeKind::Workflow;
        r.session = Some("sess-1".to_string());
        r.attribution = Attribution {
            campaign: Some("camp-1".to_string()),
            ..Attribution::default()
        };
        r.harness = Some("claude".to_string());
        r.model = Some("sonnet".to_string());
        r.effort = Some("high".to_string());
        r.policy = Some("deadbeefcafef00d".to_string());
        let json = serde_json::to_string(&r).unwrap();
        let back: OutcomeRow = serde_json::from_str(&json).unwrap();
        assert_eq!(back, r);
    }

    /// Issue #800: `OutcomeRow::direct` produces a `Direct` row, and
    /// `has_workflow_row_for_session` never mistakes it for a `Workflow` one.
    #[test]
    fn direct_row_is_written_and_never_counted_as_a_workflow_row() {
        let root = tempfile::tempdir().unwrap();
        let state = StateDir::from_root(root.path().to_path_buf());
        let direct = OutcomeRow::direct("sess-direct", Some("claude"), Some("sonnet"));
        assert_eq!(direct.kind, OutcomeKind::Direct);
        assert_eq!(direct.session.as_deref(), Some("sess-direct"));
        append(&state, &direct).unwrap();
        assert!(!has_workflow_row_for_session(&state, "sess-direct"));

        let workflow_row = {
            let mut r = row(Complexity::Bounded, Some(true), 0);
            r.session = Some("sess-workflow".to_string());
            r
        };
        append(&state, &workflow_row).unwrap();
        assert!(has_workflow_row_for_session(&state, "sess-workflow"));
    }

    /// Issue #800: a `Direct` row must never pollute a workflow calibration
    /// bucket -- `calibrate` only ever sees `Workflow` rows.
    #[test]
    fn calibrate_excludes_direct_rows() {
        let direct = OutcomeRow::direct("sess-direct", None, None);
        let report = calibrate(&[direct], 1);
        assert_eq!(report.total_samples, 0);
        assert!(report.buckets.is_empty());
    }
}
