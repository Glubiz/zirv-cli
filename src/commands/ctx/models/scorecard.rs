//! Per-model scorecard over the operator's own recorded work (#839).
//!
//! A pure fold over logs that already exist: `delegations.jsonl` rows and the
//! workflow outcome rows. No model call, no network. Failures are classified
//! first; only task failures count against a model, and anything uncertain is
//! infrastructure, so a model is never blamed for a pipe.
//!
//! Not joined: per-finding review results (the review evidence records the
//! adapter, never a model) and rot scores (keyed by transcript, not by model).

use std::collections::{BTreeMap, BTreeSet};
use std::io::Write;

use serde::{Deserialize, Serialize};

use super::super::catalogue;
use super::super::event::TranscriptUsage;
use super::super::log::DelegationRow;
use super::super::price::{self, PriceTable};
use super::super::state::{self, StateDir};
use crate::commands::workflow::engine::WorkflowStatus;
use crate::commands::workflow::outcomes::{OutcomeKind, OutcomeRow};

pub(crate) const SCORECARD_FILE: &str = "scorecard.json";
const AUTO_AVOID_LOG_FILE: &str = "model-auto-avoid.jsonl";
/// A stratum or metric with fewer samples than this shows "insufficient data".
pub const MIN_SAMPLES: usize = 20;
const WILSON_Z: f64 = 1.96;
/// Delegation rows by these agents are classifier calls, not tasks.
pub(crate) const NON_TASK_AGENTS: &[&str] = &["typesafe", "helper"];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Class {
    Success,
    /// A wrong result, failed verification or review rework: counts against the model.
    TaskFailure,
    /// Provider/auth/quota error, crash before the first turn, zirv-side error: never counted.
    Infrastructure,
}

/// `exit_code`, `outcome` and `output_tokens` drive this. A failure is a task failure only when
/// the run exited 1 after producing output; timeouts, signals (143), rot exhaustion, and an
/// exit before any output are infrastructure, as is anything unrecognised.
pub fn classify_delegation(row: &DelegationRow) -> Class {
    match (row.outcome.as_str(), row.exit_code) {
        ("ok", 0) => Class::Success,
        ("failed", 1) if row.output_tokens > 0 => Class::TaskFailure,
        _ => Class::Infrastructure,
    }
}

/// `terminal`, `verification_passed` and `review_rounds` drive this. `None` for a direct-session
/// row (it records only that a session ran) and for a row with no model to attribute.
pub fn classify_outcome(row: &OutcomeRow) -> Option<Class> {
    if row.kind == OutcomeKind::Direct || row.model.as_deref().is_none_or(str::is_empty) {
        return None;
    }
    let failed_verification = row.verification_passed == Some(false);
    Some(match row.terminal {
        WorkflowStatus::Completed => Class::Success,
        WorkflowStatus::Failed if failed_verification || row.review_rounds > 0 => {
            Class::TaskFailure
        }
        WorkflowStatus::Closed
            if failed_verification
                || row.review_rounds
                    >= crate::commands::workflow::review::MAX_FIX_REVIEW_ROUNDS =>
        {
            Class::TaskFailure
        }
        _ => Class::Infrastructure,
    })
}

/// A rate with its Wilson 95% interval; every number is `None` below [`MIN_SAMPLES`].
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Rate {
    pub n: usize,
    pub rate: Option<f64>,
    pub low: Option<f64>,
    pub high: Option<f64>,
}

impl Rate {
    pub(crate) fn new(k: usize, n: usize) -> Self {
        if n < MIN_SAMPLES {
            return Self {
                n,
                ..Self::default()
            };
        }
        let (low, high) = wilson(k, n);
        Self {
            n,
            rate: Some(k as f64 / n as f64),
            low: Some(low),
            high: Some(high),
        }
    }
}

/// Wilson score interval at 95%.
pub fn wilson(k: usize, n: usize) -> (f64, f64) {
    if n == 0 {
        return (0.0, 1.0);
    }
    let n = n as f64;
    let p = k as f64 / n;
    let z2 = WILSON_Z * WILSON_Z;
    let denom = 1.0 + z2 / n;
    let centre = (p + z2 / (2.0 * n)) / denom;
    let half = WILSON_Z * (p * (1.0 - p) / n + z2 / (4.0 * n * n)).sqrt() / denom;
    ((centre - half).max(0.0), (centre + half).min(1.0))
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct StratumScore {
    /// `all`, or `dimension=value` (`task_class`, `complexity`, `effort`).
    pub stratum: String,
    /// Task successes plus task failures; infrastructure failures are excluded.
    pub scored: usize,
    pub infrastructure_failures: usize,
    pub task_success: Rate,
    pub first_attempt_pass: Rate,
    pub rework: Rate,
    pub mean_review_rounds: Option<f64>,
    pub cost_per_task_micros: Option<u64>,
    pub median_wall_secs: Option<f64>,
    /// The best same-tier peer's task-success lower bound, when the peer has enough samples.
    pub peer: Option<String>,
    pub peer_low: Option<f64>,
    /// True when this model's task-success upper bound is below the peer's lower bound.
    pub worse_than_peer: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ModelScore {
    pub model: String,
    pub tier: Option<String>,
    pub strata: Vec<StratumScore>,
}

/// One automatic avoidance decision, with the evidence that justified it.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct AutoAvoid {
    pub model: String,
    pub stratum: String,
    pub low: f64,
    pub high: f64,
    pub peer: String,
    pub peer_low: f64,
    pub peer_high: f64,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Scorecard {
    pub generated_at: u64,
    pub models: Vec<ModelScore>,
    /// Written by the refresher; resolvers read it only when `[models] auto_avoid` is on.
    pub auto_avoid: Vec<AutoAvoid>,
}

impl Scorecard {
    pub fn auto_avoid_ids(&self) -> BTreeSet<String> {
        self.auto_avoid
            .iter()
            .map(|decision| decision.model.to_lowercase())
            .collect()
    }
}

#[derive(Default)]
struct Acc {
    success: usize,
    task_failure: usize,
    infra: usize,
    first_attempt: (usize, usize),
    review_rounds: (u64, usize),
    rework: (usize, usize),
    cost: (u64, usize),
    walls: Vec<f64>,
}

#[derive(Default)]
struct Event {
    model: String,
    strata: Vec<String>,
    class: Option<Class>,
    first_attempt: Option<bool>,
    review_rounds: Option<u8>,
    cost_micros: Option<u64>,
    wall_secs: Option<f64>,
}

fn label<T: Serialize>(value: &T) -> Option<String> {
    serde_json::to_value(value)
        .ok()
        .and_then(|v| v.as_str().map(str::to_string))
}

fn delegation_event(row: &DelegationRow, table: &PriceTable) -> Option<Event> {
    let model = row.model.as_deref().filter(|m| !m.is_empty())?;
    if row.cached || NON_TASK_AGENTS.contains(&row.agent.as_str()) {
        return None;
    }
    let class = classify_delegation(row);
    let usage = TranscriptUsage {
        input_tokens: row.input_tokens,
        cache_creation_input_tokens: row.cache_creation_input_tokens,
        cache_read_input_tokens: row.cache_read_input_tokens,
        output_tokens: row.output_tokens,
    };
    Some(Event {
        model: catalogue::normalize_id(model).to_lowercase(),
        strata: row
            .task_class
            .map(|c| format!("task_class={c}"))
            .into_iter()
            .collect(),
        class: Some(class),
        cost_micros: (class == Class::Success)
            .then(|| price::price(model, &usage, table))
            .flatten(),
        wall_secs: (class == Class::Success).then_some(row.wall_ms as f64 / 1000.0),
        ..Event::default()
    })
}

fn outcome_event(row: &OutcomeRow) -> Option<Event> {
    let class = classify_outcome(row)?;
    let model = row.model.as_deref()?;
    let mut strata = Vec::new();
    if let Some(complexity) = label(&row.complexity) {
        strata.push(format!("complexity={complexity}"));
    }
    if let Some(effort) = row.effort.as_deref().filter(|e| !e.is_empty()) {
        strata.push(format!("effort={effort}"));
    }
    Some(Event {
        model: catalogue::normalize_id(model).to_lowercase(),
        strata,
        class: Some(class),
        first_attempt: row.verification_first_attempt,
        review_rounds: (class != Class::Infrastructure).then_some(row.review_rounds),
        wall_secs: (class == Class::Success).then_some(row.duration_secs as f64),
        ..Event::default()
    })
}

fn tier_of(model: &str) -> Option<String> {
    let vendor = catalogue::vendor(catalogue::vendor_of(model)?)?;
    let tier = catalogue::rung_of(vendor, model)?.tier?;
    Some(super::tier_name(tier).to_string())
}

fn score(stratum: &str, acc: &Acc) -> StratumScore {
    let scored = acc.success + acc.task_failure;
    StratumScore {
        stratum: stratum.to_string(),
        scored,
        infrastructure_failures: acc.infra,
        task_success: Rate::new(acc.success, scored),
        first_attempt_pass: Rate::new(acc.first_attempt.0, acc.first_attempt.1),
        rework: Rate::new(acc.rework.0, acc.rework.1),
        mean_review_rounds: (acc.review_rounds.1 >= MIN_SAMPLES)
            .then(|| acc.review_rounds.0 as f64 / acc.review_rounds.1 as f64),
        cost_per_task_micros: (acc.cost.1 >= MIN_SAMPLES).then(|| acc.cost.0 / acc.cost.1 as u64),
        median_wall_secs: (acc.walls.len() >= MIN_SAMPLES)
            .then(|| super::super::measure::median(&acc.walls))
            .flatten(),
        ..StratumScore::default()
    }
}

/// Fold classified rows into the scorecard. Pure: identical inputs give an identical result.
pub fn compute(
    delegations: &[DelegationRow],
    outcomes: &[OutcomeRow],
    table: &PriceTable,
    now: u64,
) -> Scorecard {
    let events = delegations
        .iter()
        .filter_map(|row| delegation_event(row, table))
        .chain(outcomes.iter().filter_map(outcome_event));
    let mut accs: BTreeMap<(String, String), Acc> = BTreeMap::new();
    for event in events {
        let Some(class) = event.class else { continue };
        let strata = std::iter::once("all".to_string()).chain(event.strata.iter().cloned());
        for stratum in strata {
            let acc = accs.entry((event.model.clone(), stratum)).or_default();
            match class {
                Class::Success => acc.success += 1,
                Class::TaskFailure => acc.task_failure += 1,
                Class::Infrastructure => acc.infra += 1,
            }
            if let Some(passed) = event.first_attempt {
                acc.first_attempt.1 += 1;
                acc.first_attempt.0 += usize::from(passed);
            }
            if let Some(rounds) = event.review_rounds {
                acc.review_rounds.0 += u64::from(rounds);
                acc.review_rounds.1 += 1;
                acc.rework.1 += 1;
                acc.rework.0 += usize::from(rounds >= 2);
            }
            if let Some(cost) = event.cost_micros {
                acc.cost.0 += cost;
                acc.cost.1 += 1;
            }
            if let Some(wall) = event.wall_secs {
                acc.walls.push(wall);
            }
        }
    }

    let mut by_model: BTreeMap<String, Vec<StratumScore>> = BTreeMap::new();
    for ((model, stratum), acc) in &accs {
        by_model
            .entry(model.clone())
            .or_default()
            .push(score(stratum, acc));
    }
    let tiers: BTreeMap<String, Option<String>> = by_model
        .keys()
        .map(|model| (model.clone(), tier_of(model)))
        .collect();

    let snapshot = by_model.clone();
    let mut auto_avoid = Vec::new();
    for (model, strata) in &mut by_model {
        let Some(tier) = tiers.get(model).and_then(Option::as_ref) else {
            continue;
        };
        for stratum in strata.iter_mut() {
            let Some((peer, peer_row)) = snapshot
                .iter()
                .filter(|(other, _)| {
                    *other != model && tiers.get(*other) == Some(&Some(tier.clone()))
                })
                .filter_map(|(other, rows)| {
                    let row = rows.iter().find(|r| r.stratum == stratum.stratum)?;
                    row.task_success.low?;
                    Some((other, row))
                })
                .max_by(|a, b| {
                    a.1.task_success
                        .low
                        .partial_cmp(&b.1.task_success.low)
                        .unwrap_or(std::cmp::Ordering::Equal)
                        .then_with(|| b.0.cmp(a.0))
                })
            else {
                continue;
            };
            let (Some(high), Some(low), Some(peer_low), Some(peer_high)) = (
                stratum.task_success.high,
                stratum.task_success.low,
                peer_row.task_success.low,
                peer_row.task_success.high,
            ) else {
                continue;
            };
            stratum.peer = Some(peer.clone());
            stratum.peer_low = Some(peer_low);
            stratum.worse_than_peer = high < peer_low;
            // Only the all-tasks stratum decides: per-stratum comparisons are display-only.
            if stratum.worse_than_peer && stratum.stratum == "all" {
                auto_avoid.push(AutoAvoid {
                    model: model.clone(),
                    stratum: stratum.stratum.clone(),
                    low,
                    high,
                    peer: peer.clone(),
                    peer_low,
                    peer_high,
                });
            }
        }
    }
    Scorecard {
        generated_at: now,
        models: by_model
            .into_iter()
            .map(|(model, strata)| ModelScore {
                tier: tiers.get(&model).cloned().flatten(),
                model,
                strata,
            })
            .collect(),
        auto_avoid,
    }
}

/// Read the logs and fold them. Missing or unreadable logs give an empty scorecard.
pub(crate) fn build(state: &StateDir, table: &PriceTable, now: u64) -> Scorecard {
    let delegations = super::super::log::read_delegations(state, usize::MAX);
    let outcomes = crate::commands::workflow::outcomes::read_all(state);
    compute(&delegations, &outcomes, table, now)
}

pub(crate) fn load(state: &StateDir) -> Option<Scorecard> {
    super::read_json(&state.root().join(SCORECARD_FILE))
}

/// Compute, cache next to the registry, and log each newly made auto-avoid decision.
pub(crate) fn refresh(state: &StateDir, table: &PriceTable, now: u64) -> super::CtxResult<()> {
    let previous = load(state).map(|card| card.auto_avoid).unwrap_or_default();
    let card = build(state, table, now);
    super::write_json(&state.root().join(SCORECARD_FILE), &card)?;
    let fresh: Vec<&AutoAvoid> = card
        .auto_avoid
        .iter()
        .filter(|decision| {
            !previous
                .iter()
                .any(|old| old.model == decision.model && old.stratum == decision.stratum)
        })
        .collect();
    if fresh.is_empty() {
        return Ok(());
    }
    let path = state.logs().join(AUTO_AVOID_LOG_FILE);
    state::create_private_dir_all(state.logs().as_path())?;
    let mut file = state::open_private_append(&path)?;
    for decision in fresh {
        writeln!(
            file,
            "{}",
            serde_json::json!({"ts": now, "decision": decision})
        )?;
    }
    Ok(())
}

fn pct(value: Option<f64>) -> String {
    value.map_or_else(|| "-".into(), |v| format!("{:.0}%", v * 100.0))
}

fn rate_text(rate: &Rate) -> String {
    match (rate.rate, rate.low, rate.high) {
        (Some(r), Some(l), Some(h)) => format!(
            "{} [{}-{}] n={}",
            pct(Some(r)),
            pct(Some(l)),
            pct(Some(h)),
            rate.n
        ),
        _ => format!("insufficient data (n={})", rate.n),
    }
}

/// The scorecard section of `zirv ctx models`.
pub fn render(card: &Scorecard, w: &mut dyn Write) -> std::io::Result<()> {
    if card.models.is_empty() {
        return writeln!(w, "\nSCORECARD\tno recorded work yet");
    }
    writeln!(
        w,
        "\nSCORECARD (task failures only; infrastructure failures excluded; Wilson 95%, minimum {MIN_SAMPLES} samples)"
    )?;
    writeln!(
        w,
        "MODEL\tTIER\tSTRATUM\tTASK SUCCESS\tFIRST-ATTEMPT PASS\tREVIEW ROUNDS\tREWORK\tCOST/TASK\tMEDIAN WALL\tINFRA FAILURES"
    )?;
    for model in &card.models {
        for s in &model.strata {
            writeln!(
                w,
                "{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}",
                model.model,
                model.tier.as_deref().unwrap_or("-"),
                s.stratum,
                rate_text(&s.task_success),
                rate_text(&s.first_attempt_pass),
                s.mean_review_rounds
                    .map_or_else(|| "insufficient data".into(), |v| format!("{v:.2}")),
                rate_text(&s.rework),
                s.cost_per_task_micros.map_or_else(
                    || "insufficient data".into(),
                    |c| format!("${}.{:04}", c / 1_000_000, (c % 1_000_000) / 100),
                ),
                s.median_wall_secs
                    .map_or_else(|| "insufficient data".into(), |v| format!("{v:.0}s"),),
                s.infrastructure_failures,
            )?;
        }
    }
    for d in &card.auto_avoid {
        writeln!(
            w,
            "AUTO-AVOID\t{}\t{}\tsuccess [{}-{}] below peer {} lower bound {}",
            d.model,
            d.stratum,
            pct(Some(d.low)),
            pct(Some(d.high)),
            d.peer,
            pct(Some(d.peer_low)),
        )?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn delegation(model: &str, outcome: &str, exit_code: i32, output_tokens: u64) -> DelegationRow {
        serde_json::from_value(json!({
            "ts": 1, "session": "s", "parent_session": "p", "agent": "codex", "model": model,
            "input_tokens": 1000, "cache_creation_input_tokens": 0, "cache_read_input_tokens": 0,
            "output_tokens": output_tokens, "wall_ms": 60_000, "exit_code": exit_code,
            "outcome": outcome,
        }))
        .expect("synthetic row")
    }

    fn outcome(model: &str, terminal: &str, rounds: u8, first: Option<bool>) -> OutcomeRow {
        serde_json::from_value(json!({
            "schema_version": 2, "ts": 1, "workflow_id": "w", "pack": "p",
            "profile": "standard", "complexity": "bounded", "risk": "low", "seat_tier": null,
            "review_rounds": rounds, "verification_first_attempt": first,
            "verification_passed": first.map(|_| terminal == "completed"),
            "terminal": terminal, "duration_secs": 100, "harness": "codex", "model": model,
            "effort": "high",
        }))
        .expect("synthetic outcome")
    }

    fn table() -> PriceTable {
        price::built_in_table()
    }

    fn rows(model: &str, ok: usize, failed: usize) -> Vec<DelegationRow> {
        let mut out: Vec<_> = (0..ok).map(|_| delegation(model, "ok", 0, 10)).collect();
        out.extend((0..failed).map(|_| delegation(model, "failed", 1, 10)));
        out
    }

    fn all(card: &Scorecard, model: &str) -> StratumScore {
        card.models
            .iter()
            .find(|m| m.model == model)
            .and_then(|m| m.strata.iter().find(|s| s.stratum == "all"))
            .cloned()
            .expect("model scored")
    }

    #[test]
    fn infrastructure_failures_are_excluded_from_the_score() {
        let mut input = rows("gpt-5.6-sol", 20, 0);
        input.push(delegation("gpt-5.6-sol", "failed", 1, 0));
        input.push(delegation("gpt-5.6-sol", "failed", 143, 50));
        input.push(delegation("gpt-5.6-sol", "timeout", 76, 50));
        input.push(delegation("gpt-5.6-sol", "rot-exhausted", 75, 50));
        input.push(delegation("gpt-5.6-sol", "failed", 2, 50));
        let card = compute(&input, &[], &table(), 1);
        let s = all(&card, "gpt-5.6-sol");
        assert_eq!(s.scored, 20);
        assert_eq!(s.infrastructure_failures, 5);
        assert_eq!(s.task_success.rate, Some(1.0));

        input.push(delegation("gpt-5.6-sol", "failed", 1, 80));
        let s = all(&compute(&input, &[], &table(), 1), "gpt-5.6-sol");
        assert_eq!(s.scored, 21, "a failure after output is a task failure");
        assert!(s.task_success.rate.expect("rate") < 1.0);
    }

    #[test]
    fn workflow_outcomes_classify_by_terminal_state_and_evidence() {
        let class =
            |terminal, rounds, first| classify_outcome(&outcome("m", terminal, rounds, first));
        assert_eq!(class("completed", 0, Some(true)), Some(Class::Success));
        assert_eq!(class("failed", 0, Some(false)), Some(Class::TaskFailure));
        assert_eq!(class("failed", 0, None), Some(Class::Infrastructure));
        assert_eq!(class("closed", 0, None), Some(Class::Infrastructure));
        assert_eq!(class("closed", 3, None), Some(Class::TaskFailure));
        let mut direct = outcome("m", "completed", 0, None);
        direct.kind = OutcomeKind::Direct;
        assert_eq!(classify_outcome(&direct), None);
    }

    #[test]
    fn a_stratum_under_the_minimum_sample_shows_insufficient_data() {
        let card = compute(&rows("gpt-5.6-sol", 19, 0), &[], &table(), 1);
        let s = all(&card, "gpt-5.6-sol");
        assert_eq!(s.task_success.rate, None);
        assert_eq!(s.task_success.low, None);
        let mut text = Vec::new();
        render(&card, &mut text).expect("render");
        assert!(
            String::from_utf8(text)
                .expect("utf8")
                .contains("insufficient data (n=19)")
        );
        let s = all(
            &compute(&rows("gpt-5.6-sol", 20, 0), &[], &table(), 1),
            "gpt-5.6-sol",
        );
        assert_eq!(s.task_success.rate, Some(1.0));
    }

    #[test]
    fn wilson_interval_matches_known_values() {
        let (low, high) = wilson(10, 20);
        assert!((low - 0.299).abs() < 0.005 && (high - 0.701).abs() < 0.005);
        let (low, high) = wilson(20, 20);
        assert!((low - 0.839).abs() < 0.005 && high > 0.999);
    }

    #[test]
    fn outcome_metrics_cover_first_attempt_review_rounds_and_rework() {
        let mut input = Vec::new();
        for i in 0..20 {
            let rounds = if i < 5 { 2 } else { 1 };
            input.push(outcome("gpt-5.6-terra", "completed", rounds, Some(i >= 4)));
        }
        let card = compute(&[], &input, &table(), 1);
        let s = all(&card, "gpt-5.6-terra");
        assert_eq!(s.first_attempt_pass.rate, Some(0.8));
        assert_eq!(s.rework.rate, Some(0.25));
        assert_eq!(s.mean_review_rounds, Some(1.25));
        assert!(
            card.models[0]
                .strata
                .iter()
                .any(|s| s.stratum == "complexity=bounded")
        );
        assert!(
            card.models[0]
                .strata
                .iter()
                .any(|s| s.stratum == "effort=high")
        );
    }

    #[test]
    fn cost_and_wall_use_the_existing_price_resolution() {
        let card = compute(&rows("gpt-5.6-sol", 20, 0), &[], &table(), 1);
        let s = all(&card, "gpt-5.6-sol");
        assert!(s.cost_per_task_micros.is_some());
        assert_eq!(s.median_wall_secs, Some(60.0));
    }

    #[test]
    fn a_clearly_worse_model_is_marked_against_its_best_same_tier_peer() {
        // gpt-5.6-terra and sonnet are both Standard.
        let mut input = rows("sonnet", 40, 0);
        input.extend(rows("gpt-5.6-terra", 2, 38));
        let card = compute(&input, &[], &table(), 1);
        let worse = all(&card, "gpt-5.6-terra");
        assert!(worse.worse_than_peer);
        assert_eq!(worse.peer.as_deref(), Some("sonnet"));
        assert!(
            !all(&card, "sonnet").worse_than_peer,
            "a good model is never demoted"
        );
        assert_eq!(card.auto_avoid.len(), 1);
        assert_eq!(card.auto_avoid[0].model, "gpt-5.6-terra");
        assert_eq!(
            card.auto_avoid_ids(),
            BTreeSet::from(["gpt-5.6-terra".to_string()])
        );

        // Under the minimum on either side nothing is decided.
        let mut input = rows("sonnet", 19, 0);
        input.extend(rows("gpt-5.6-terra", 0, 40));
        assert!(compute(&input, &[], &table(), 1).auto_avoid.is_empty());
        // Overlapping intervals are not enough.
        let mut input = rows("sonnet", 24, 16);
        input.extend(rows("gpt-5.6-terra", 20, 20));
        assert!(compute(&input, &[], &table(), 1).auto_avoid.is_empty());
    }

    #[test]
    fn a_worse_single_stratum_is_displayed_but_never_auto_avoided() {
        let with_complexity = |model: &str, complexity: &str, terminal: &str, first: bool| {
            let mut row = outcome(model, terminal, 0, Some(first));
            row.complexity = serde_json::from_value(json!(complexity)).expect("complexity");
            row
        };
        let mut input = Vec::new();
        for _ in 0..40 {
            input.push(with_complexity("sonnet", "bounded", "completed", true));
            input.push(with_complexity("sonnet", "trivial", "failed", false));
            input.push(with_complexity("gpt-5.6-terra", "bounded", "failed", false));
        }
        for _ in 0..200 {
            input.push(with_complexity(
                "gpt-5.6-terra",
                "trivial",
                "completed",
                true,
            ));
        }
        let card = compute(&[], &input, &table(), 1);
        let bounded = card
            .models
            .iter()
            .find(|m| m.model == "gpt-5.6-terra")
            .and_then(|m| m.strata.iter().find(|s| s.stratum == "complexity=bounded"))
            .expect("bounded stratum");
        assert!(bounded.worse_than_peer, "still shown per stratum");
        assert!(!all(&card, "gpt-5.6-terra").worse_than_peer);
        assert!(
            !card.auto_avoid_ids().contains("gpt-5.6-terra"),
            "{:?}",
            card.auto_avoid
        );
    }

    #[test]
    fn classifier_and_cached_rows_are_not_tasks() {
        let mut jev = delegation("jev-latest", "ok", 0, 10);
        jev.agent = "typesafe".into();
        assert!(compute(&[jev], &[], &table(), 1).models.is_empty());
    }
}
