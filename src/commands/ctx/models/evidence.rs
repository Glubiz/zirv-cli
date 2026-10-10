//! Per-(harness, model, role, complexity) evidence for model routing.
//!
//! A pure fold over two sources: synthetic benchmark probe rows and the operator's own
//! recorded work (delegations and workflow outcomes). Real-world evidence overrules
//! synthetic evidence in [`compare`]. No model call, no network; [`refresh`] only reads the
//! logs and writes `evidence.json`.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use super::super::catalogue;
use super::super::config::CtxConfig;
use super::super::event::TranscriptUsage;
use super::super::log::{DelegationRow, TaskClass};
use super::super::price::{self, PriceTable};
use super::super::state::StateDir;
use super::scorecard::{self, Class, MIN_SAMPLES, Rate};
use super::{CtxResult, read_json, write_json};
use crate::commands::benchmark::{self, Row, Status};
use crate::commands::workflow::classify::Complexity;
use crate::commands::workflow::engine::WorkflowStatus;
use crate::commands::workflow::outcomes::OutcomeRow;

pub(crate) const EVIDENCE_FILE: &str = "evidence.json";
/// Rows and recorded work older than this are ignored.
pub(crate) const WINDOW_DAYS: u32 = 30;
/// Fewer synthetic probe rows than this make a cell's synthetic numbers inconclusive.
pub(crate) const MIN_SYNTH: usize = 5;
/// Fewer real outcomes than this make a cell's real success rate inconclusive.
pub(crate) const MIN_REAL: usize = MIN_SAMPLES;
const EVIDENCE_VERSION: u32 = 1;
const Z: f64 = 1.96;

/// What a model is being chosen for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RouteRole {
    /// A chat or proxy seat, or a workflow-level seat.
    Orchestrator,
    /// A `zirv agent` delegation or a workflow worker seat.
    Worker,
    Reviewer,
}

/// Task difficulty; `Any` is the rollup across all of them.
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize,
)]
#[serde(rename_all = "lowercase")]
pub enum CellComplexity {
    Trivial,
    Bounded,
    Substantial,
    Architectural,
    #[default]
    Any,
}

impl From<Complexity> for CellComplexity {
    fn from(complexity: Complexity) -> Self {
        match complexity {
            Complexity::Trivial => Self::Trivial,
            Complexity::Bounded => Self::Bounded,
            Complexity::Substantial => Self::Substantial,
            Complexity::Architectural => Self::Architectural,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct SyntheticStats {
    pub n: usize,
    pub mean: Option<f64>,
    /// `mean` -/+ 1.96 sd / sqrt(n), clamped to [0, 1]; `None` below [`MIN_SYNTH`].
    pub low: Option<f64>,
    pub high: Option<f64>,
    pub cost_micros_mean: Option<u64>,
    pub wall_ms_median: Option<u64>,
    pub last_at: Option<u64>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct RealStats {
    pub n: usize,
    /// Successes among the `n` scored outcomes; kept so cells can be pooled exactly.
    pub k: usize,
    /// Wilson interval; the rate fields are `None` below [`MIN_REAL`].
    pub success: Rate,
    pub cost_micros_mean: Option<u64>,
    pub last_at: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Cell {
    pub harness: String,
    pub model: String,
    pub role: RouteRole,
    pub complexity: CellComplexity,
    pub synthetic: SyntheticStats,
    pub real: RealStats,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Evidence {
    pub version: u32,
    pub generated_at: u64,
    pub window_days: u32,
    pub cells: Vec<Cell>,
}

impl Evidence {
    /// The cell for exactly this key, else the complexity-`any` rollup of the same
    /// (harness, model, role).
    #[allow(dead_code)] // read by the router, which lands after the evidence store
    pub fn cell(
        &self,
        harness: &str,
        model: &str,
        role: RouteRole,
        complexity: CellComplexity,
    ) -> Option<&Cell> {
        let find = |complexity: CellComplexity| {
            self.cells.iter().find(|cell| {
                cell.harness == harness
                    && cell.model == model
                    && cell.role == role
                    && cell.complexity == complexity
            })
        };
        find(complexity).or_else(|| {
            (complexity != CellComplexity::Any)
                .then(|| find(CellComplexity::Any))
                .flatten()
        })
    }

    /// The complexity-`any` cells of one (harness, model) pooled across roles, for decisions
    /// that are not about one role (the promotion gate). `None` when no cell exists. The
    /// synthetic interval is not pooled (`low`/`high` stay `None`); `n`, `mean` and the real
    /// success rate are exact.
    pub fn pooled(&self, harness: &str, model: &str) -> Option<Cell> {
        let cells: Vec<&Cell> = self
            .cells
            .iter()
            .filter(|cell| {
                cell.harness == harness
                    && cell.model == model
                    && cell.complexity == CellComplexity::Any
            })
            .collect();
        if cells.is_empty() {
            return None;
        }
        let synthetic_n: usize = cells.iter().map(|cell| cell.synthetic.n).sum();
        let weighted = |value: fn(&Cell) -> Option<f64>, weight: fn(&Cell) -> usize| {
            let total: usize = cells
                .iter()
                .filter(|cell| value(cell).is_some())
                .map(|cell| weight(cell))
                .sum();
            (total > 0).then(|| {
                cells
                    .iter()
                    .filter_map(|cell| value(cell).map(|v| v * weight(cell) as f64))
                    .sum::<f64>()
                    / total as f64
            })
        };
        let real_n: usize = cells.iter().map(|cell| cell.real.n).sum();
        let real_k: usize = cells.iter().map(|cell| cell.real.k).sum();
        Some(Cell {
            harness: harness.to_string(),
            model: model.to_string(),
            role: cells[0].role,
            complexity: CellComplexity::Any,
            synthetic: SyntheticStats {
                n: synthetic_n,
                mean: weighted(|cell| cell.synthetic.mean, |cell| cell.synthetic.n),
                cost_micros_mean: weighted(
                    |cell| cell.synthetic.cost_micros_mean.map(|c| c as f64),
                    |cell| cell.synthetic.n,
                )
                .map(|cost| cost.round() as u64),
                last_at: cells.iter().filter_map(|cell| cell.synthetic.last_at).max(),
                ..SyntheticStats::default()
            },
            real: RealStats {
                n: real_n,
                k: real_k,
                success: Rate::new(real_k, real_n),
                cost_micros_mean: weighted(
                    |cell| cell.real.cost_micros_mean.map(|c| c as f64),
                    |cell| cell.real.n,
                )
                .map(|cost| cost.round() as u64),
                last_at: cells.iter().filter_map(|cell| cell.real.last_at).max(),
            },
        })
    }
}

/// How a candidate model compares with an incumbent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Verdict {
    Better,
    NonInferior,
    Inferior,
    Insufficient,
}

impl Verdict {
    pub fn label(self) -> &'static str {
        match self {
            Self::Better => "better",
            Self::NonInferior => "non-inferior",
            Self::Inferior => "inferior",
            Self::Insufficient => "insufficient",
        }
    }
}

/// The first decisive rule wins; real-world evidence overrules synthetic.
///
/// 1. With [`MIN_REAL`] real outcomes on both sides, a candidate interval wholly below the
///    incumbent's is `Inferior` and wholly above it is `Better`.
/// 2. With [`MIN_SYNTH`] synthetic rows on both sides, a mean more than `tolerance` above the
///    incumbent's is `Better`, within `tolerance` below it is `NonInferior`, else `Inferior`.
/// 3. Otherwise `Insufficient`.
pub fn compare(candidate: &Cell, incumbent: &Cell, tolerance: f64) -> Verdict {
    if candidate.real.n >= MIN_REAL
        && incumbent.real.n >= MIN_REAL
        && let (Some(c_low), Some(c_high), Some(i_low), Some(i_high)) = (
            candidate.real.success.low,
            candidate.real.success.high,
            incumbent.real.success.low,
            incumbent.real.success.high,
        )
    {
        if c_high < i_low {
            return Verdict::Inferior;
        }
        if c_low > i_high {
            return Verdict::Better;
        }
    }
    if candidate.synthetic.n >= MIN_SYNTH
        && incumbent.synthetic.n >= MIN_SYNTH
        && let (Some(c), Some(i)) = (candidate.synthetic.mean, incumbent.synthetic.mean)
    {
        if c > i + tolerance {
            return Verdict::Better;
        }
        if c >= i - tolerance {
            return Verdict::NonInferior;
        }
        return Verdict::Inferior;
    }
    Verdict::Insufficient
}

type Key = (String, String, RouteRole, CellComplexity);

#[derive(Default)]
struct SynthAcc {
    qualities: Vec<f64>,
    costs: Vec<u64>,
    walls: Vec<u64>,
    last_at: Option<u64>,
}

#[derive(Default)]
struct RealAcc {
    k: usize,
    n: usize,
    costs: Vec<u64>,
    last_at: Option<u64>,
}

#[derive(Default)]
struct Acc {
    synthetic: SynthAcc,
    real: RealAcc,
}

fn model_key(model: &str) -> String {
    catalogue::normalize_id(model).to_lowercase()
}

/// The keys one observation feeds: its own complexity and the `any` rollup.
fn keys(harness: &str, model: &str, role: RouteRole, complexity: CellComplexity) -> Vec<Key> {
    let mut keys = vec![(harness.to_string(), model.to_string(), role, complexity)];
    if complexity != CellComplexity::Any {
        keys.push((
            harness.to_string(),
            model.to_string(),
            role,
            CellComplexity::Any,
        ));
    }
    keys
}

fn max_at(slot: &mut Option<u64>, at: u64) {
    *slot = Some(slot.map_or(at, |old| old.max(at)));
}

/// The existing benchmark composite: correctness, or the mean of correctness and judge/10. A
/// failed run scores 0; a run with nothing graded has no quality.
fn quality(row: &Row) -> Option<f64> {
    if row.status == Status::Failed {
        return Some(0.0);
    }
    let correctness = row.correctness?;
    Some(match row.judge_score {
        Some(judge) => (correctness + judge / 10.0) / 2.0,
        None => correctness,
    })
}

fn synthetic_stats(acc: &SynthAcc) -> SyntheticStats {
    let n = acc.qualities.len();
    if n == 0 {
        return SyntheticStats::default();
    }
    let mean = acc.qualities.iter().sum::<f64>() / n as f64;
    let (low, high) = if n >= MIN_SYNTH {
        let variance = acc
            .qualities
            .iter()
            .map(|q| (q - mean).powi(2))
            .sum::<f64>()
            / (n - 1) as f64;
        let half = Z * variance.sqrt() / (n as f64).sqrt();
        (
            Some((mean - half).clamp(0.0, 1.0)),
            Some((mean + half).clamp(0.0, 1.0)),
        )
    } else {
        (None, None)
    };
    let mut walls = acc.walls.clone();
    walls.sort_unstable();
    SyntheticStats {
        n,
        mean: Some(mean),
        low,
        high,
        cost_micros_mean: mean_u64(&acc.costs),
        wall_ms_median: walls.get(walls.len().saturating_sub(1) / 2).copied(),
        last_at: acc.last_at,
    }
}

fn mean_u64(values: &[u64]) -> Option<u64> {
    (!values.is_empty()).then(|| values.iter().sum::<u64>() / values.len() as u64)
}

fn real_stats(acc: &RealAcc) -> RealStats {
    RealStats {
        n: acc.n,
        k: acc.k,
        success: Rate::new(acc.k, acc.n),
        cost_micros_mean: mean_u64(&acc.costs),
        last_at: acc.last_at,
    }
}

/// Fold probe rows and recorded work into cells. Pure: identical inputs give an identical
/// result. Inputs are expected to be inside the window already; recorded work older than
/// [`WINDOW_DAYS`] before `now` is dropped here.
pub fn compute(
    bench: &[Row],
    delegations: &[DelegationRow],
    outcomes: &[OutcomeRow],
    table: &PriceTable,
    now: u64,
) -> Evidence {
    let since = now.saturating_sub(u64::from(WINDOW_DAYS) * 86_400);
    let mut accs: BTreeMap<Key, Acc> = BTreeMap::new();

    for row in bench.iter().filter(|row| row.status != Status::Skipped) {
        let Some(quality) = quality(row) else {
            continue;
        };
        if row.model.is_empty() {
            continue;
        }
        let model = model_key(&row.model);
        for key in keys(&row.harness, &model, row.routing_role(), row.complexity) {
            let acc = &mut accs.entry(key).or_default().synthetic;
            acc.qualities.push(quality);
            acc.costs.extend(row.cost_micros);
            acc.walls.extend(row.wall_ms);
            if row.ts > 0 {
                max_at(&mut acc.last_at, row.ts);
            }
        }
    }

    for row in delegations.iter().filter(|row| row.ts >= since) {
        let Some(model) = row.model.as_deref().filter(|model| !model.is_empty()) else {
            continue;
        };
        if row.cached || scorecard::NON_TASK_AGENTS.contains(&row.agent.as_str()) {
            continue;
        }
        let class = scorecard::classify_delegation(row);
        if class == Class::Infrastructure {
            continue;
        }
        let role = if row.task_class == Some(TaskClass::Review) {
            RouteRole::Reviewer
        } else {
            RouteRole::Worker
        };
        let cost = (class == Class::Success)
            .then(|| {
                price::price(
                    model,
                    &TranscriptUsage {
                        input_tokens: row.input_tokens,
                        cache_creation_input_tokens: row.cache_creation_input_tokens,
                        cache_read_input_tokens: row.cache_read_input_tokens,
                        output_tokens: row.output_tokens,
                    },
                    table,
                )
            })
            .flatten();
        for key in keys(&row.agent, &model_key(model), role, CellComplexity::Any) {
            let acc = &mut accs.entry(key).or_default().real;
            acc.n += 1;
            acc.k += usize::from(class == Class::Success);
            acc.costs.extend(cost);
            max_at(&mut acc.last_at, row.ts);
        }
    }

    for row in outcomes.iter().filter(|row| row.ts >= since) {
        let (Some(harness), Some(model)) = (
            row.harness.as_deref().filter(|harness| !harness.is_empty()),
            row.model.as_deref().filter(|model| !model.is_empty()),
        ) else {
            continue;
        };
        // A direct session or an infrastructure stop says nothing about the model.
        if !matches!(
            scorecard::classify_outcome(row),
            Some(Class::Success | Class::TaskFailure)
        ) {
            continue;
        }
        let success =
            row.terminal == WorkflowStatus::Completed && row.verification_passed != Some(false);
        for key in keys(
            harness,
            &model_key(model),
            RouteRole::Orchestrator,
            row.complexity.into(),
        ) {
            let acc = &mut accs.entry(key).or_default().real;
            acc.n += 1;
            acc.k += usize::from(success);
            max_at(&mut acc.last_at, row.ts);
        }
    }

    let cells = accs
        .into_iter()
        .filter(|(_, acc)| !acc.synthetic.qualities.is_empty() || acc.real.n > 0)
        .map(|((harness, model, role, complexity), acc)| Cell {
            harness,
            model,
            role,
            complexity,
            synthetic: synthetic_stats(&acc.synthetic),
            real: real_stats(&acc.real),
        })
        .collect();
    Evidence {
        version: EVIDENCE_VERSION,
        generated_at: now,
        window_days: WINDOW_DAYS,
        cells,
    }
}

/// Read the probe store and the logs, fold them, and cache the result as `evidence.json`.
pub(crate) fn refresh(state: &StateDir, cfg: &CtxConfig, now: u64) -> CtxResult<Evidence> {
    let since = now.saturating_sub(u64::from(WINDOW_DAYS) * 86_400);
    let bench = benchmark::read_rows_since(&state.root().join("benchmark"), since);
    let delegations = super::super::log::read_delegations(state, usize::MAX);
    let outcomes = crate::commands::workflow::outcomes::read_all(state);
    let table = super::effective_prices(cfg, state).table;
    let evidence = compute(&bench, &delegations, &outcomes, &table, now);
    write_json(&state.root().join(EVIDENCE_FILE), &evidence)?;
    Ok(evidence)
}

pub(crate) fn load(state: &StateDir) -> Option<Evidence> {
    read_json(&state.root().join(EVIDENCE_FILE))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const NOW: u64 = 100 * 86_400;

    fn row(harness: &str, model: &str, complexity: CellComplexity, correctness: f64) -> Row {
        let mut row = Row::new(
            harness,
            model,
            None,
            "t",
            crate::commands::benchmark::Role::Worker,
            0,
            Status::Ok,
        );
        row.complexity = complexity;
        row.correctness = Some(correctness);
        row.ts = NOW - 10;
        row
    }

    fn rows(n: usize, complexity: CellComplexity, correctness: f64) -> Vec<Row> {
        (0..n)
            .map(|_| row("claude", "claude-opus-5-5", complexity, correctness))
            .collect()
    }

    fn delegation(model: &str, outcome: &str, exit_code: i32, output: u64) -> DelegationRow {
        serde_json::from_value(json!({
            "ts": NOW - 5, "session": "s", "parent_session": "p", "agent": "claude",
            "model": model, "input_tokens": 1000, "cache_creation_input_tokens": 0,
            "cache_read_input_tokens": 0, "output_tokens": output, "wall_ms": 1000,
            "exit_code": exit_code, "outcome": outcome,
        }))
        .expect("delegation row")
    }

    fn synth(n: usize, mean: f64) -> Cell {
        Cell {
            harness: "claude".into(),
            model: "m".into(),
            role: RouteRole::Worker,
            complexity: CellComplexity::Any,
            synthetic: SyntheticStats {
                n,
                mean: Some(mean),
                ..SyntheticStats::default()
            },
            real: RealStats::default(),
        }
    }

    fn real(k: usize, n: usize) -> Cell {
        let mut cell = synth(0, 0.0);
        cell.synthetic = SyntheticStats::default();
        cell.real = RealStats {
            n,
            k,
            success: Rate::new(k, n),
            ..RealStats::default()
        };
        cell
    }

    #[test]
    fn real_evidence_overrules_synthetic_when_both_sides_have_enough() {
        let mut candidate = real(5, 40);
        candidate.synthetic = synth(10, 0.99).synthetic;
        let mut incumbent = real(38, 40);
        incumbent.synthetic = synth(10, 0.5).synthetic;
        assert_eq!(compare(&candidate, &incumbent, 0.05), Verdict::Inferior);
        assert_eq!(compare(&incumbent, &candidate, 0.05), Verdict::Better);
    }

    #[test]
    fn overlapping_real_intervals_fall_through_to_the_synthetic_rule() {
        let mut candidate = real(20, 40);
        candidate.synthetic = synth(10, 0.9).synthetic;
        let mut incumbent = real(22, 40);
        incumbent.synthetic = synth(10, 0.5).synthetic;
        assert_eq!(compare(&candidate, &incumbent, 0.05), Verdict::Better);
    }

    #[test]
    fn synthetic_means_are_compared_within_the_tolerance() {
        let incumbent = synth(10, 0.8);
        assert_eq!(compare(&synth(10, 0.9), &incumbent, 0.05), Verdict::Better);
        assert_eq!(
            compare(&synth(10, 0.76), &incumbent, 0.05),
            Verdict::NonInferior
        );
        assert_eq!(
            compare(&synth(10, 0.84), &incumbent, 0.05),
            Verdict::NonInferior
        );
        assert_eq!(
            compare(&synth(10, 0.7), &incumbent, 0.05),
            Verdict::Inferior
        );
    }

    #[test]
    fn thin_evidence_on_either_side_is_insufficient() {
        assert_eq!(
            compare(&synth(4, 1.0), &synth(10, 0.1), 0.05),
            Verdict::Insufficient
        );
        assert_eq!(
            compare(&synth(10, 1.0), &synth(4, 0.1), 0.05),
            Verdict::Insufficient
        );
        assert_eq!(
            compare(&real(0, 19), &real(19, 19), 0.05),
            Verdict::Insufficient
        );
    }

    #[test]
    fn compute_keys_cells_by_harness_normalised_model_role_and_complexity_with_an_any_rollup() {
        let mut bench = rows(3, CellComplexity::Trivial, 1.0);
        bench.extend(rows(2, CellComplexity::Bounded, 0.0));
        bench.push(row("codex", "gpt-6.1-sol", CellComplexity::Trivial, 1.0));
        let evidence = compute(&bench, &[], &[], &price::built_in_table(), NOW);
        let cell = |h: &str, m: &str, c| {
            evidence
                .cells
                .iter()
                .find(|cell| {
                    cell.harness == h
                        && cell.model == m
                        && cell.role == RouteRole::Worker
                        && cell.complexity == c
                })
                .map(|cell| cell.synthetic.clone())
        };
        let trivial = cell("claude", "claude-opus-5-5", CellComplexity::Trivial).expect("trivial");
        assert_eq!((trivial.n, trivial.mean), (3, Some(1.0)));
        let any = cell("claude", "claude-opus-5-5", CellComplexity::Any).expect("rollup");
        assert_eq!(any.n, 5);
        assert!((any.mean.expect("mean") - 0.6).abs() < 1e-9);
        assert!(any.low.is_some(), "five rows are enough for an interval");
        assert_eq!(trivial.low, None, "three rows are not");
        assert!(cell("codex", "gpt-6.1-sol", CellComplexity::Trivial).is_some());
    }

    #[test]
    fn an_exact_cell_wins_and_a_missing_one_falls_back_to_the_any_rollup() {
        let evidence = compute(
            &rows(5, CellComplexity::Trivial, 1.0),
            &[],
            &[],
            &price::built_in_table(),
            NOW,
        );
        let model = "claude-opus-5-5";
        let exact = evidence.cell("claude", model, RouteRole::Worker, CellComplexity::Trivial);
        assert_eq!(exact.map(|c| c.complexity), Some(CellComplexity::Trivial));
        let rollup = evidence.cell("claude", model, RouteRole::Worker, CellComplexity::Bounded);
        assert_eq!(rollup.map(|c| c.complexity), Some(CellComplexity::Any));
        assert!(
            evidence
                .cell(
                    "claude",
                    model,
                    RouteRole::Reviewer,
                    CellComplexity::Bounded
                )
                .is_none()
        );
    }

    #[test]
    fn a_failed_row_scores_zero_and_a_skipped_row_is_excluded() {
        let mut failed = row("claude", "m", CellComplexity::Any, 1.0);
        failed.status = Status::Failed;
        failed.correctness = None;
        let mut skipped = row("claude", "m", CellComplexity::Any, 1.0);
        skipped.status = Status::Skipped;
        let ok = row("claude", "m", CellComplexity::Any, 1.0);
        let evidence = compute(
            &[failed, skipped, ok],
            &[],
            &[],
            &price::built_in_table(),
            NOW,
        );
        let stats = &evidence.cells[0].synthetic;
        assert_eq!(stats.n, 2);
        assert_eq!(stats.mean, Some(0.5));
    }

    #[test]
    fn a_judge_score_is_averaged_with_correctness() {
        let mut judged = row("claude", "m", CellComplexity::Any, 1.0);
        judged.judge_score = Some(5.0);
        let evidence = compute(&[judged], &[], &[], &price::built_in_table(), NOW);
        assert_eq!(evidence.cells[0].synthetic.mean, Some(0.75));
    }

    #[test]
    fn real_delegations_exclude_infrastructure_failures_and_split_review_from_work() {
        let mut delegations = Vec::new();
        for _ in 0..MIN_REAL {
            delegations.push(delegation("claude-opus-5-5", "ok", 0, 10));
        }
        // A timeout (143) and a crash before any output are infrastructure, never counted.
        delegations.push(delegation("claude-opus-5-5", "failed", 143, 10));
        delegations.push(delegation("claude-opus-5-5", "failed", 1, 0));
        // A real task failure counts against the model.
        delegations.push(delegation("claude-opus-5-5", "failed", 1, 10));
        let mut review = delegation("claude-opus-5-5", "ok", 0, 10);
        review.task_class = Some(TaskClass::Review);
        delegations.push(review);
        let evidence = compute(&[], &delegations, &[], &price::built_in_table(), NOW);
        let work = evidence
            .cell(
                "claude",
                "claude-opus-5-5",
                RouteRole::Worker,
                CellComplexity::Any,
            )
            .expect("worker cell");
        assert_eq!((work.real.n, work.real.k), (MIN_REAL + 1, MIN_REAL));
        assert!(work.real.success.rate.is_some());
        let review = evidence
            .cell(
                "claude",
                "claude-opus-5-5",
                RouteRole::Reviewer,
                CellComplexity::Any,
            )
            .expect("reviewer cell");
        assert_eq!(review.real.n, 1);
        assert_eq!(review.real.success.rate, None, "one outcome is too few");
    }

    #[test]
    fn recorded_work_outside_the_window_is_ignored() {
        let mut old = delegation("claude-opus-5-5", "ok", 0, 10);
        old.ts = NOW - u64::from(WINDOW_DAYS) * 86_400 - 1;
        let evidence = compute(&[], &[old], &[], &price::built_in_table(), NOW);
        assert!(evidence.cells.is_empty());
    }

    #[test]
    fn pooled_cells_add_roles_up_exactly() {
        let mut bench = rows(5, CellComplexity::Any, 1.0);
        let mut reviewer = row("claude", "claude-opus-5-5", CellComplexity::Any, 0.0);
        reviewer.route_role = Some(RouteRole::Reviewer);
        bench.extend(std::iter::repeat_n(reviewer, 5));
        let evidence = compute(&bench, &[], &[], &price::built_in_table(), NOW);
        let pooled = evidence
            .pooled("claude", "claude-opus-5-5")
            .expect("pooled cell");
        assert_eq!(pooled.synthetic.n, 10);
        assert_eq!(pooled.synthetic.mean, Some(0.5));
        assert!(evidence.pooled("claude", "other").is_none());
    }

    #[test]
    fn compute_is_deterministic() {
        let bench = rows(6, CellComplexity::Bounded, 0.7);
        let a = compute(&bench, &[], &[], &price::built_in_table(), NOW);
        let b = compute(&bench, &[], &[], &price::built_in_table(), NOW);
        assert_eq!(a, b);
    }
}
