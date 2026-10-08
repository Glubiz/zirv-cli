//! Per-candidate, per-role aggregation of stored rows and the stack recommendation.
//!
//! Everything here is pure: rows in, structs and strings out.

use serde::Serialize;

use super::corpus::Role;
use super::run::{Row, RunMeta, Status};
use crate::commands::ctx::price::format_usd;
use crate::commands::workflow::research::stats::{mean, median};

/// A worker may score this far below the best worker and still win on cost.
pub const WORKER_TOLERANCE: f64 = 0.10;
/// Orchestrator scores this close to the best count as tied.
const TIE_TOLERANCE: f64 = 0.005;
const LOW_SAMPLE: usize = 3;
const FLOAT_SLACK: f64 = 1e-9;

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Cell {
    pub harness: String,
    pub model: String,
    pub role: Role,
    pub n: usize,
    pub failed: usize,
    pub correct: Option<f64>,
    pub judge: Option<f64>,
    pub score: Option<f64>,
    pub median_wall_s: Option<f64>,
    /// `None` when any run's cost is unknown; an unknown cost is never zero.
    pub median_cost_micros: Option<u64>,
    pub total_cost_micros: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Pick {
    pub harness: String,
    pub model: String,
    pub score: f64,
    pub reason: String,
    /// Config to paste into `~/.zirv/ctx.toml`, or the plain `harness/model`.
    pub apply_with: String,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Recommendation {
    pub orchestrator: Option<Pick>,
    pub worker: Option<Pick>,
    pub caveat: String,
    pub warning: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Report {
    pub meta: RunMeta,
    pub total_runs: usize,
    pub skipped_runs: usize,
    /// Tasks left out of a role's comparison, and why.
    pub notes: Vec<RoleNote>,
    pub cells: Vec<Cell>,
    pub recommendation: Recommendation,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct RoleNote {
    pub role: Role,
    pub text: String,
    /// No task was completed by every candidate, so the role cannot be compared.
    pub empty: bool,
}

pub fn build(meta: RunMeta, rows: &[Row]) -> Report {
    let (comparable, notes) = comparable_rows(&meta, rows);
    let cells = cells(&comparable);
    let recommendation = recommend(&cells);
    Report {
        total_runs: rows.len(),
        skipped_runs: rows.iter().filter(|r| r.status == Status::Skipped).count(),
        meta,
        notes,
        cells,
        recommendation,
    }
}

/// A planned candidate: harness and planned model label.
type Key<'a> = (&'a str, &'a str);
/// One planned run: task id and repetition.
type Pair<'a> = (&'a str, u32);

/// Within a role, only the (task, rep) pairs every compared candidate completed (ok or failed,
/// not skipped) are compared, so a spend cap cannot give two candidates different run sets.
/// The planned candidates come from `meta`, keyed by their planned label, not the model a run
/// ended up using; a planned candidate with no completed run is named and left out.
fn comparable_rows(meta: &RunMeta, rows: &[Row]) -> (Vec<Row>, Vec<RoleNote>) {
    let planned: Vec<(&str, &str)> = if meta.candidates.is_empty() {
        let mut seen: Vec<(&str, &str)> = Vec::new();
        for row in rows {
            let key = (row.harness.as_str(), row.candidate_label());
            if !seen.contains(&key) {
                seen.push(key);
            }
        }
        seen
    } else {
        meta.candidates
            .iter()
            .map(|c| (c.harness.as_str(), c.model.as_str()))
            .collect()
    };
    let pair_label = |task: &str, rep: u32| {
        if meta.reps > 1 {
            format!("{task} rep {}", rep + 1)
        } else {
            task.to_string()
        }
    };

    let mut kept = Vec::new();
    let mut notes = Vec::new();
    for role in [Role::Worker, Role::Orchestrator] {
        let ran: Vec<&Row> = rows
            .iter()
            .filter(|r| r.role == role && r.status != Status::Skipped)
            .collect();
        let owned_by = |row: &Row, (harness, label): (&str, &str)| {
            row.harness == harness && row.candidate_label() == label
        };
        let (completers, idle): (Vec<Key>, Vec<Key>) = planned
            .iter()
            .copied()
            .partition(|key| ran.iter().any(|r| owned_by(r, *key)));
        if completers.is_empty() {
            continue;
        }
        let mut pairs: Vec<(&str, u32)> = Vec::new();
        for row in ran
            .iter()
            .filter(|r| completers.iter().any(|k| owned_by(r, *k)))
        {
            let pair = (row.task.as_str(), row.rep);
            if !pairs.contains(&pair) {
                pairs.push(pair);
            }
        }
        let (common, excluded): (Vec<Pair>, Vec<Pair>) =
            pairs.into_iter().partition(|(task, rep)| {
                completers.iter().all(|key| {
                    ran.iter()
                        .any(|r| owned_by(r, *key) && r.task == *task && r.rep == *rep)
                })
            });
        let labels = |pairs: &[(&str, u32)]| {
            pairs
                .iter()
                .map(|(task, rep)| pair_label(task, *rep))
                .collect::<Vec<_>>()
                .join(", ")
        };
        let mut parts = Vec::new();
        if !excluded.is_empty() && common.is_empty() {
            parts.push(
                "no task was completed by every candidate (spend cap), so the role is not compared"
                    .to_string(),
            );
        } else if !excluded.is_empty() {
            parts.push(format!(
                "compared over {} only; {} not completed by every candidate (spend cap)",
                labels(&common),
                labels(&excluded)
            ));
        }
        for (harness, label) in &idle {
            parts.push(format!("{harness}/{label} excluded: no completed runs"));
        }
        if !parts.is_empty() {
            notes.push(RoleNote {
                role,
                text: parts.join("; "),
                empty: common.is_empty(),
            });
        }
        kept.extend(
            ran.into_iter()
                .filter(|r| {
                    completers.iter().any(|k| owned_by(r, *k))
                        && common.contains(&(r.task.as_str(), r.rep))
                })
                .cloned(),
        );
    }
    (kept, notes)
}

/// One cell per candidate and role, in first-seen order; skipped rows are not runs.
pub fn cells(rows: &[Row]) -> Vec<Cell> {
    let mut keys: Vec<(&str, &str, Role)> = Vec::new();
    for row in rows.iter().filter(|r| r.status != Status::Skipped) {
        let key = (row.harness.as_str(), row.candidate_label(), row.role);
        if !keys.contains(&key) {
            keys.push(key);
        }
    }
    keys.into_iter()
        .map(|(harness, label, role)| {
            let group: Vec<&Row> = rows
                .iter()
                .filter(|r| {
                    r.status != Status::Skipped
                        && r.harness == harness
                        && r.candidate_label() == label
                        && r.role == role
                })
                .collect();
            // Show the model the transcript named when the candidate was only a label.
            let model = group
                .iter()
                .map(|r| r.model.as_str())
                .find(|m| *m != label)
                .unwrap_or(label);
            cell(harness, model, role, &group)
        })
        .collect()
}

fn cell(harness: &str, model: &str, role: Role, group: &[&Row]) -> Cell {
    let correctness: Vec<f64> = group.iter().filter_map(|r| r.correctness).collect();
    let judge: Vec<f64> = group.iter().filter_map(|r| r.judge_score).collect();
    let composites: Vec<f64> = group.iter().filter_map(|r| composite(r)).collect();
    let walls: Vec<f64> = group
        .iter()
        .filter_map(|r| r.wall_ms)
        .map(|ms| ms as f64 / 1000.0)
        .collect();
    let costs: Option<Vec<f64>> = group
        .iter()
        .map(|r| r.cost_micros.map(|c| c as f64))
        .collect();
    let has_value = |values: &[f64]| (!values.is_empty()).then(|| mean(values));
    Cell {
        harness: harness.to_string(),
        model: model.to_string(),
        role,
        n: group.len(),
        failed: group.iter().filter(|r| r.status == Status::Failed).count(),
        correct: has_value(&correctness),
        judge: has_value(&judge),
        score: has_value(&composites),
        median_wall_s: (!walls.is_empty()).then(|| median(&walls)),
        median_cost_micros: costs
            .as_ref()
            .filter(|c| !c.is_empty())
            .map(|c| median(c).round() as u64),
        total_cost_micros: costs.map(|c| c.iter().sum::<f64>().round() as u64),
    }
}

/// Mean of the components a run has; a run with neither is left out of quality aggregates.
fn composite(row: &Row) -> Option<f64> {
    let parts: Vec<f64> = [row.correctness, row.judge_score.map(|j| j / 10.0)]
        .into_iter()
        .flatten()
        .collect();
    (!parts.is_empty()).then(|| mean(&parts))
}

pub fn recommend(cells: &[Cell]) -> Recommendation {
    let scored = |role: Role| -> Vec<(&Cell, f64)> {
        cells
            .iter()
            .filter(|c| c.role == role)
            .filter_map(|c| c.score.map(|s| (c, s)))
            .collect()
    };
    let min_n = cells.iter().map(|c| c.n).min();
    let n_text = min_n.map_or_else(|| "0".to_string(), |n| n.to_string());
    Recommendation {
        orchestrator: pick_orchestrator(&scored(Role::Orchestrator)),
        worker: pick_worker(&scored(Role::Worker)),
        caveat: format!(
            "composed from per-role solo scores; delegation overhead not measured; n={n_text} per cell"
        ),
        warning: min_n.filter(|n| *n < LOW_SAMPLE).map(|n| {
            format!(
                "low sample: n={n} < {LOW_SAMPLE} in the smallest cell; differences may be noise"
            )
        }),
    }
}

/// Lower is better; an unknown value sorts after every known one.
fn unknown_last<T: PartialOrd + Copy>(a: Option<T>, b: Option<T>) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    match (a, b) {
        (Some(x), Some(y)) => x.partial_cmp(&y).unwrap_or(Ordering::Equal),
        (Some(_), None) => Ordering::Less,
        (None, Some(_)) => Ordering::Greater,
        (None, None) => Ordering::Equal,
    }
}

fn cost_then_wall(a: &Cell, b: &Cell) -> std::cmp::Ordering {
    unknown_last(a.median_cost_micros, b.median_cost_micros)
        .then_with(|| unknown_last(a.median_wall_s, b.median_wall_s))
}

fn best_score(scored: &[(&Cell, f64)]) -> f64 {
    scored
        .iter()
        .map(|(_, s)| *s)
        .fold(f64::NEG_INFINITY, f64::max)
}

fn pick_orchestrator(scored: &[(&Cell, f64)]) -> Option<Pick> {
    let best = best_score(scored);
    let (cell, score) = scored
        .iter()
        .filter(|(_, s)| *s + FLOAT_SLACK >= best - TIE_TOLERANCE)
        .min_by(|(a, _), (b, _)| cost_then_wall(a, b))?;
    Some(Pick {
        harness: cell.harness.clone(),
        model: cell.model.clone(),
        score: *score,
        reason: format!(
            "highest orchestrator score {score:.2}; ties within {TIE_TOLERANCE} go to the lower cost, then time ({})",
            figures(cell)
        ),
        apply_with: apply_with(Role::Orchestrator, &cell.harness, &cell.model),
    })
}

fn pick_worker(scored: &[(&Cell, f64)]) -> Option<Pick> {
    let best = best_score(scored);
    let eligible: Vec<&(&Cell, f64)> = scored
        .iter()
        .filter(|(_, s)| *s + FLOAT_SLACK >= best - WORKER_TOLERANCE)
        .collect();
    let any_cost = eligible.iter().any(|(c, _)| c.median_cost_micros.is_some());
    let (cell, score) = **eligible.iter().min_by(|(a, sa), (b, sb)| {
        let primary = if any_cost {
            unknown_last(a.median_cost_micros, b.median_cost_micros)
        } else {
            unknown_last(a.median_wall_s, b.median_wall_s)
        };
        primary.then_with(|| sb.partial_cmp(sa).unwrap_or(std::cmp::Ordering::Equal))
    })?;
    let criterion = if any_cost {
        "lowest known median cost"
    } else {
        "no cost known: lowest median time"
    };
    Some(Pick {
        harness: cell.harness.clone(),
        model: cell.model.clone(),
        score,
        reason: format!(
            "{criterion} among workers within {WORKER_TOLERANCE:.2} of the best score {best:.2} ({})",
            figures(cell)
        ),
        apply_with: apply_with(Role::Worker, &cell.harness, &cell.model),
    })
}

fn figures(cell: &Cell) -> String {
    format!(
        "median {}, median {}",
        seconds(cell.median_wall_s),
        usd(cell.median_cost_micros)
    )
}

/// Only keys that exist in `CtxConfig` are emitted; anything else is the plain `harness/model`.
pub fn apply_with(role: Role, harness: &str, model: &str) -> String {
    let plain = format!("{harness}/{model}");
    match (role, harness, model) {
        (Role::Orchestrator, _, "default") => format!("agent = \"{harness}\""),
        (Role::Orchestrator, "claude" | "codex", _) => {
            format!("agent = \"{harness}\"\n\n[chat]\nmodel = \"{model}\"")
        }
        (Role::Worker, "claude" | "codex", m) if m != "default" => {
            format!("[worker]\n{harness} = \"{model}\"")
        }
        _ => plain,
    }
}

fn usd(micros: Option<u64>) -> String {
    micros.map_or_else(|| "unknown".to_string(), |m| format_usd(m, false))
}

/// The configured spend cap as dollars, through the same formatter as measured costs.
pub fn usd_amount(usd: f64) -> String {
    format_usd((usd * 1_000_000.0).round() as u64, false)
}

fn seconds(value: Option<f64>) -> String {
    value.map_or_else(|| "-".to_string(), |s| format!("{s:.1}s"))
}

fn number(value: Option<f64>) -> String {
    value.map_or_else(|| "-".to_string(), |v| format!("{v:.2}"))
}

pub fn render_text(report: &Report) -> String {
    let meta = &report.meta;
    let mut out = format!(
        "benchmark {}  started {}  zirv {}\nprices as of {}  reps {}  spend cap {}\n",
        meta.run_id,
        meta.started_at,
        meta.zirv_version,
        meta.prices_as_of,
        meta.reps,
        usd_amount(meta.max_usd),
    );
    match &meta.judge {
        Some(judge) => {
            out.push_str(&format!("judge: {}/{}\n", judge.harness, judge.model));
            if judge.also_candidate {
                out.push_str(
                    "note: the judge is also a candidate; judges can favour their own output\n",
                );
            }
        }
        None if meta.judge_unpriced => out.push_str(
            "judge: none (no candidate had a known output price; deterministic graders only)\n",
        ),
        None => out.push_str("judge: none (deterministic graders only)\n"),
    }
    if report.skipped_runs > 0 {
        out.push_str(&format!(
            "warning: {} of {} runs skipped by the spend cap (--max-usd {}); raise it or narrow with --family/--model\n",
            report.skipped_runs, report.total_runs, meta.max_usd
        ));
    }
    out.push('\n');

    let header = [
        "harness", "model", "role", "n", "failed", "correct", "judge", "score", "median s",
        "median $", "total $",
    ];
    let mut table: Vec<Vec<String>> = vec![header.iter().map(|h| (*h).to_string()).collect()];
    for cell in &report.cells {
        table.push(vec![
            cell.harness.clone(),
            cell.model.clone(),
            cell.role.as_str().to_string(),
            cell.n.to_string(),
            cell.failed.to_string(),
            number(cell.correct),
            number(cell.judge),
            number(cell.score),
            seconds(cell.median_wall_s),
            usd(cell.median_cost_micros),
            usd(cell.total_cost_micros),
        ]);
    }
    let widths: Vec<usize> = (0..header.len())
        .map(|col| {
            table
                .iter()
                .map(|row| row[col].chars().count())
                .max()
                .unwrap_or(0)
        })
        .collect();
    for row in &table {
        let line: Vec<String> = row
            .iter()
            .zip(&widths)
            .map(|(value, width)| format!("{value:<width$}"))
            .collect();
        out.push_str(line.join("  ").trim_end());
        out.push('\n');
    }

    let rec = &report.recommendation;
    out.push_str("\nrecommendation\n");
    let note_for = |role: Role| report.notes.iter().find(|n| n.role == role);
    for (role, label, pick) in [
        (Role::Orchestrator, "orchestrator", &rec.orchestrator),
        (Role::Worker, "worker", &rec.worker),
    ] {
        if let Some(note) = note_for(role).filter(|n| !n.empty) {
            out.push_str(&format!("  note: {label}: {}\n", note.text));
        }
        match pick {
            None => match note_for(role).filter(|n| n.empty) {
                Some(note) => out.push_str(&format!("  {label}: none ({})\n", note.text)),
                None => out.push_str(&format!("  {label}: none\n")),
            },
            Some(pick) => {
                out.push_str(&format!(
                    "  {label}: {}/{}  score {:.2}\n    {}\n    apply with:\n",
                    pick.harness, pick.model, pick.score, pick.reason
                ));
                for line in pick.apply_with.lines() {
                    out.push_str(&format!("      {line}\n"));
                }
            }
        }
    }
    out.push_str(&format!("  caveat: {}\n", rec.caveat));
    if let Some(warning) = &rec.warning {
        out.push_str(&format!("  warning: {warning}\n"));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::ctx::config::CtxConfig;

    fn cell_with(
        harness: &str,
        role: Role,
        score: Option<f64>,
        cost: Option<u64>,
        wall: f64,
    ) -> Cell {
        Cell {
            harness: harness.to_string(),
            model: "m".to_string(),
            role,
            n: 3,
            failed: 0,
            correct: score,
            judge: None,
            score,
            median_wall_s: Some(wall),
            median_cost_micros: cost,
            total_cost_micros: cost,
        }
    }

    fn row(harness: &str, role: Role, status: Status) -> Row {
        let mut row = Row::new(harness, "m", None, "t", role, 0, status);
        row.wall_ms = Some(2000);
        row
    }

    #[test]
    fn a_worker_exactly_at_the_tolerance_boundary_qualifies() {
        let cells = vec![
            cell_with("best", Role::Worker, Some(0.9), Some(9_000_000), 10.0),
            cell_with("edge", Role::Worker, Some(0.8), Some(1_000_000), 10.0),
        ];
        let rec = recommend(&cells);
        assert_eq!(rec.worker.unwrap().harness, "edge");
    }

    #[test]
    fn a_worker_just_below_the_tolerance_does_not_qualify() {
        let cells = vec![
            cell_with("best", Role::Worker, Some(0.9), Some(9_000_000), 10.0),
            cell_with("low", Role::Worker, Some(0.79), Some(1_000_000), 10.0),
        ];
        assert_eq!(recommend(&cells).worker.unwrap().harness, "best");
    }

    #[test]
    fn unknown_cost_sorts_last_and_is_never_a_zero_cost_winner() {
        let cells = vec![
            cell_with("free?", Role::Worker, Some(0.9), None, 1.0),
            cell_with("paid", Role::Worker, Some(0.9), Some(5_000_000), 50.0),
        ];
        assert_eq!(recommend(&cells).worker.unwrap().harness, "paid");
    }

    #[test]
    fn with_no_known_cost_the_worker_is_the_fastest() {
        let cells = vec![
            cell_with("slow", Role::Worker, Some(0.9), None, 30.0),
            cell_with("fast", Role::Worker, Some(0.85), None, 5.0),
        ];
        let pick = recommend(&cells).worker.unwrap();
        assert_eq!(pick.harness, "fast");
        assert!(pick.reason.contains("no cost known"));
    }

    #[test]
    fn worker_cost_ties_go_to_the_higher_score() {
        let cells = vec![
            cell_with("a", Role::Worker, Some(0.85), Some(1_000_000), 10.0),
            cell_with("b", Role::Worker, Some(0.95), Some(1_000_000), 10.0),
        ];
        assert_eq!(recommend(&cells).worker.unwrap().harness, "b");
    }

    #[test]
    fn the_orchestrator_is_the_highest_score() {
        let cells = vec![
            cell_with("a", Role::Orchestrator, Some(0.7), Some(1), 1.0),
            cell_with("b", Role::Orchestrator, Some(0.9), Some(9_000_000), 99.0),
        ];
        assert_eq!(recommend(&cells).orchestrator.unwrap().harness, "b");
    }

    #[test]
    fn orchestrator_ties_within_half_a_point_go_to_the_cheaper_then_faster() {
        let cells = vec![
            cell_with(
                "dear",
                Role::Orchestrator,
                Some(0.900),
                Some(9_000_000),
                1.0,
            ),
            cell_with(
                "cheap",
                Role::Orchestrator,
                Some(0.896),
                Some(1_000_000),
                50.0,
            ),
        ];
        assert_eq!(recommend(&cells).orchestrator.unwrap().harness, "cheap");
        let cells = vec![
            cell_with("slow", Role::Orchestrator, Some(0.9), Some(1_000_000), 50.0),
            cell_with("fast", Role::Orchestrator, Some(0.9), Some(1_000_000), 5.0),
        ];
        assert_eq!(recommend(&cells).orchestrator.unwrap().harness, "fast");
    }

    #[test]
    fn orchestrator_scores_further_than_the_tie_band_do_not_tie() {
        let cells = vec![
            cell_with("best", Role::Orchestrator, Some(0.9), Some(9_000_000), 1.0),
            cell_with("cheap", Role::Orchestrator, Some(0.89), Some(1), 1.0),
        ];
        assert_eq!(recommend(&cells).orchestrator.unwrap().harness, "best");
    }

    #[test]
    fn a_role_with_no_scored_rows_is_none() {
        let cells = vec![cell_with("a", Role::Worker, Some(0.5), Some(1), 1.0)];
        let rec = recommend(&cells);
        assert!(rec.orchestrator.is_none());
        let unscored = vec![cell_with("a", Role::Worker, None, Some(1), 1.0)];
        assert!(recommend(&unscored).worker.is_none());
        assert!(recommend(&[]).worker.is_none());
    }

    #[test]
    fn the_caveat_is_always_present_and_names_the_smallest_cell() {
        let mut cells = vec![cell_with("a", Role::Worker, Some(0.5), Some(1), 1.0)];
        cells[0].n = 5;
        let rec = recommend(&cells);
        assert_eq!(
            rec.caveat,
            "composed from per-role solo scores; delegation overhead not measured; n=5 per cell"
        );
        assert!(rec.warning.is_none());
        assert!(
            recommend(&[])
                .caveat
                .contains("delegation overhead not measured")
        );
    }

    #[test]
    fn fewer_than_three_runs_in_a_cell_warns() {
        let mut cells = vec![cell_with("a", Role::Worker, Some(0.5), Some(1), 1.0)];
        cells[0].n = 2;
        assert!(recommend(&cells).warning.unwrap().contains("n=2"));
    }

    #[test]
    fn composite_is_the_mean_of_the_available_components() {
        let mut both = row("h", Role::Worker, Status::Ok);
        both.correctness = Some(1.0);
        both.judge_score = Some(5.0);
        assert_eq!(composite(&both), Some(0.75));
        let mut only_judge = row("h", Role::Worker, Status::Ok);
        only_judge.judge_score = Some(8.0);
        assert_eq!(composite(&only_judge), Some(0.8));
        assert_eq!(composite(&row("h", Role::Worker, Status::Ok)), None);
    }

    #[test]
    fn cells_exclude_skipped_rows_and_count_failures() {
        let mut ok = row("h", Role::Worker, Status::Ok);
        ok.correctness = Some(1.0);
        ok.cost_micros = Some(2_000_000);
        let mut failed = row("h", Role::Worker, Status::Failed);
        failed.correctness = Some(0.0);
        failed.cost_micros = Some(4_000_000);
        let skipped = row("h", Role::Worker, Status::Skipped);
        let cells = cells(&[ok, failed, skipped]);
        assert_eq!(cells.len(), 1);
        assert_eq!((cells[0].n, cells[0].failed), (2, 1));
        assert_eq!(cells[0].score, Some(0.5));
        assert_eq!(cells[0].total_cost_micros, Some(6_000_000));
        assert_eq!(cells[0].median_cost_micros, Some(2_000_000));
    }

    #[test]
    fn one_unknown_cost_makes_the_cells_costs_unknown() {
        let mut known = row("h", Role::Worker, Status::Ok);
        known.cost_micros = Some(1_000_000);
        let unknown = row("h", Role::Worker, Status::Ok);
        let cells = cells(&[known, unknown]);
        assert_eq!(cells[0].median_cost_micros, None);
        assert_eq!(cells[0].total_cost_micros, None);
    }

    #[test]
    fn unknown_cost_is_rendered_as_unknown_never_as_zero() {
        let unknown = row("h", Role::Worker, Status::Ok);
        let report = Report {
            meta: RunMeta::for_test(),
            total_runs: 1,
            skipped_runs: 0,
            notes: Vec::new(),
            cells: cells(&[unknown]),
            recommendation: recommend(&[]),
        };
        let text = render_text(&report);
        assert!(text.contains("unknown"));
        assert!(!text.contains("$0.00"));
    }

    #[test]
    fn the_text_report_carries_the_caveat_and_none_for_empty_roles() {
        let report = Report {
            meta: RunMeta::for_test(),
            total_runs: 0,
            skipped_runs: 0,
            notes: Vec::new(),
            cells: Vec::new(),
            recommendation: recommend(&[]),
        };
        let text = render_text(&report);
        assert!(text.contains("orchestrator: none"));
        assert!(text.contains("worker: none"));
        assert!(text.contains("delegation overhead not measured"));
    }

    fn done(harness: &str, task: &str, role: Role, correctness: f64) -> Row {
        let mut row = Row::new(harness, "m", None, task, role, 0, Status::Ok);
        row.correctness = Some(correctness);
        row
    }

    fn skipped(harness: &str, task: &str, role: Role) -> Row {
        let mut row = Row::new(harness, "m", None, task, role, 0, Status::Skipped);
        row.skip_reason = Some("spend cap".to_string());
        row
    }

    #[test]
    fn candidates_are_compared_only_over_tasks_every_candidate_completed() {
        let rows = [
            done("a", "t1", Role::Worker, 1.0),
            done("a", "t2", Role::Worker, 0.0),
            done("b", "t1", Role::Worker, 0.5),
            skipped("b", "t2", Role::Worker),
        ];
        let report = build(RunMeta::for_test(), &rows);
        let score = |harness: &str| {
            report
                .cells
                .iter()
                .find(|c| c.harness == harness)
                .and_then(|c| c.score)
        };
        assert_eq!((score("a"), score("b")), (Some(1.0), Some(0.5)));
        assert_eq!(report.notes.len(), 1);
        assert!(
            report.notes[0]
                .text
                .contains("compared over t1 only; t2 not completed")
        );
        assert!(!report.notes[0].empty);
    }

    #[test]
    fn a_role_with_no_commonly_completed_task_is_none_with_the_reason() {
        let rows = [
            done("a", "t1", Role::Orchestrator, 1.0),
            done("b", "t2", Role::Orchestrator, 1.0),
        ];
        let report = build(RunMeta::for_test(), &rows);
        assert!(report.recommendation.orchestrator.is_none());
        assert!(report.cells.is_empty());
        let text = render_text(&report);
        assert!(text.contains("orchestrator: none (no task was completed by every candidate"));
    }

    fn meta_with(candidates: &[(&str, &str)], reps: u32) -> RunMeta {
        let mut meta = RunMeta::for_test();
        meta.reps = reps;
        meta.candidates = candidates
            .iter()
            .map(|(harness, model)| {
                serde_json::from_value(
                    serde_json::json!({"harness": harness, "model": model, "family": null}),
                )
                .unwrap()
            })
            .collect();
        meta
    }

    #[test]
    fn a_default_candidate_is_one_candidate_whatever_model_its_rows_record() {
        let mut ok = done("a", "t1", Role::Worker, 1.0);
        ok.candidate = "default".to_string();
        ok.model = "vendor-model-2".to_string();
        let mut failed = Row::new("a", "default", None, "t2", Role::Worker, 0, Status::Failed);
        failed.correctness = Some(0.0);
        let mut other = done("a", "t2", Role::Worker, 1.0);
        other.candidate = "default".to_string();
        other.model = "vendor-model-2".to_string();
        let report = build(meta_with(&[("a", "default")], 1), &[ok, failed, other]);
        assert_eq!(report.cells.len(), 1);
        assert_eq!(report.cells[0].model, "vendor-model-2");
        assert_eq!(report.cells[0].n, 3);
        assert!(report.notes.is_empty());
    }

    #[test]
    fn a_planned_candidate_with_no_completed_run_is_named_in_the_role_note() {
        let rows = [
            done("a", "t1", Role::Worker, 1.0),
            skipped("b", "t1", Role::Worker),
        ];
        let report = build(meta_with(&[("a", "m"), ("b", "m")], 1), &rows);
        assert_eq!(report.cells.len(), 1);
        assert!(
            report.notes[0]
                .text
                .contains("b/m excluded: no completed runs")
        );
        assert!(!report.notes[0].empty);
    }

    #[test]
    fn commonality_is_per_task_and_rep_so_cells_have_equal_n() {
        let rep = |harness: &str, task: &str, rep: u32| {
            let mut row = done(harness, task, Role::Worker, 1.0);
            row.rep = rep;
            row
        };
        let rows = [
            rep("a", "t1", 0),
            rep("a", "t1", 1),
            rep("b", "t1", 0),
            skipped("b", "t1", Role::Worker),
        ];
        let report = build(meta_with(&[("a", "m"), ("b", "m")], 2), &rows);
        assert!(report.cells.iter().all(|c| c.n == 1), "{:?}", report.cells);
        assert!(report.notes[0].text.contains("t1 rep 2 not completed"));
    }

    #[test]
    fn an_unpriced_default_judge_is_worded_like_the_plan_in_the_report() {
        let mut meta = RunMeta::for_test();
        meta.judge_unpriced = true;
        let text = render_text(&build(meta, &[]));
        assert!(text.contains("judge: none (no candidate had a known output price"));
        let plain = render_text(&build(RunMeta::for_test(), &[]));
        assert!(plain.contains("judge: none (deterministic graders only)"));
    }

    #[test]
    fn a_spend_cap_skip_prints_the_warning_with_counts() {
        let rows = [
            done("a", "t1", Role::Worker, 1.0),
            skipped("a", "t2", Role::Worker),
            skipped("a", "t3", Role::Worker),
        ];
        let text = render_text(&build(RunMeta::for_test(), &rows));
        assert!(text.contains(
            "warning: 2 of 3 runs skipped by the spend cap (--max-usd 10); raise it or narrow with --family/--model"
        ));
        let clean = render_text(&build(RunMeta::for_test(), &rows[..1]));
        assert!(!clean.contains("skipped by the spend cap"));
    }

    #[test]
    fn apply_with_snippets_parse_as_ctx_config() {
        for (role, harness, model) in [
            (Role::Orchestrator, "claude", "opus"),
            (Role::Orchestrator, "codex", "gpt-5.6-sol"),
            (Role::Orchestrator, "claude", "default"),
            (Role::Worker, "claude", "sonnet"),
            (Role::Worker, "codex", "gpt-5.6-terra"),
        ] {
            let snippet = apply_with(role, harness, model);
            let parsed: CtxConfig = toml::from_str(&snippet)
                .unwrap_or_else(|e| panic!("{snippet:?} does not parse: {e}"));
            match role {
                Role::Orchestrator => {
                    assert_eq!(parsed.agent.as_deref(), Some(harness));
                    let expected = (model != "default").then(|| model.to_string());
                    assert_eq!(parsed.chat.model, expected);
                }
                Role::Worker if harness == "claude" => {
                    assert_eq!(parsed.worker.claude.as_deref(), Some(model));
                }
                Role::Worker => assert_eq!(parsed.worker.codex.as_deref(), Some(model)),
            }
        }
    }

    #[test]
    fn harnesses_without_config_keys_get_the_plain_pair() {
        assert_eq!(apply_with(Role::Worker, "gemini", "pro"), "gemini/pro");
        assert_eq!(
            apply_with(Role::Orchestrator, "gemini", "pro"),
            "gemini/pro"
        );
        assert_eq!(
            apply_with(Role::Worker, "claude", "default"),
            "claude/default"
        );
    }
}
