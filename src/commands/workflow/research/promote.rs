//! The statistical promotion gate for autoresearch candidates (#801): turns
//! paired baseline/candidate trial observations into a per-cohort, then
//! overall, promotion verdict. Pure: no fs, clock, env or network -- the
//! caller supplies the observations, the criteria, the (already
//! multiple-comparisons-adjusted) confidence level, and a seed.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use super::stats::{self, Interval};

/// One trial's outcome for one arm of one (task, rep, cohort) pair.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Observation {
    pub task: String,
    pub rep: u32,
    /// Opaque cohort key. Cohorts are never pooled: every comparison in
    /// [`evaluate`] happens within one cohort's own observations.
    pub cohort: String,
    pub arm: Arm,
    pub status: TrialStatus,
    /// `[0, 1]`.
    pub correctness: Option<f64>,
    /// `[0, 1]`; `None` when the task has no quality judge.
    pub quality: Option<f64>,
    /// Execution cost only; `None` means unknown.
    pub cost_usd: Option<f64>,
    pub cost_complete: bool,
    pub wall_ms: u64,
    /// The reason this observation is dropped from stats (`untriggered`,
    /// `env_mismatch`, ...) while still being counted in the report; `None`
    /// when the observation is used normally.
    pub excluded: Option<String>,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Arm {
    Baseline,
    Candidate,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TrialStatus {
    Ok,
    Error,
    Timeout,
    Crash,
}

/// A manifest's `[criteria]` TOML table.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct Criteria {
    pub min_pairs: usize,
    pub correctness_floor: f64,
    pub quality_floor: Option<f64>,
    pub max_correctness_regression: f64,
    pub max_quality_regression: f64,
    /// Relative improvement required to count as a material benefit.
    pub min_effect: f64,
    pub confidence: f64,
    pub bootstrap_resamples: usize,
}

impl Default for Criteria {
    fn default() -> Self {
        Self {
            min_pairs: 6,
            correctness_floor: 0.0,
            quality_floor: None,
            max_correctness_regression: 0.02,
            max_quality_regression: 0.05,
            min_effect: 0.05,
            confidence: 0.95,
            bootstrap_resamples: 4000,
        }
    }
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Verdict {
    Accept,
    Reject,
    Inconclusive,
    Tradeoff,
    Unmeasured,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct ArmSummary {
    pub n: usize,
    pub success_rate: f64,
    pub timeout_rate: f64,
    pub error_rate: f64,
    pub correctness_mean: Option<f64>,
    pub quality_mean: Option<f64>,
    pub cost_per_success_usd: Option<f64>,
    pub cost_complete: bool,
    pub wall_median_ms: Option<u64>,
    /// Only computed when `n >= 10`; a p90 over fewer trials is not a
    /// tail statistic worth reporting.
    pub wall_p90_ms: Option<u64>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct CohortDecision {
    pub cohort: String,
    pub verdict: Verdict,
    pub reasons: Vec<String>,
    pub n_pairs: usize,
    pub excluded: usize,
    pub baseline: ArmSummary,
    pub candidate: ArmSummary,
    pub d_correctness: Option<Interval>,
    pub d_quality: Option<Interval>,
    pub rel_cost: Option<Interval>,
    pub rel_wall: Option<Interval>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Decision {
    pub verdict: Verdict,
    pub reasons: Vec<String>,
    pub cohorts: Vec<CohortDecision>,
    pub confidence: f64,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(tag = "verdict", rename_all = "snake_case")]
pub enum ScreenVerdict {
    Survive,
    Discard { reason: String },
}

/// A pair is the same `(task, rep, cohort)` with both arms present and
/// neither `excluded`. Matches baseline observations to candidate
/// observations in the order they appear in `obs`, so the result is
/// independent of any hash-map iteration order (bootstrap determinism
/// depends on this: the pairing itself must not vary run to run).
fn build_pairs<'a>(obs: &[&'a Observation]) -> Vec<(&'a Observation, &'a Observation)> {
    let baselines: Vec<&'a Observation> = obs
        .iter()
        .filter(|o| o.arm == Arm::Baseline && o.excluded.is_none())
        .copied()
        .collect();
    let candidates: Vec<&'a Observation> = obs
        .iter()
        .filter(|o| o.arm == Arm::Candidate && o.excluded.is_none())
        .copied()
        .collect();
    let mut used = vec![false; candidates.len()];
    let mut pairs = Vec::new();
    for b in baselines {
        if let Some((idx, c)) = candidates
            .iter()
            .enumerate()
            .find(|(i, c)| !used[*i] && c.task == b.task && c.rep == b.rep && c.cohort == b.cohort)
        {
            used[idx] = true;
            pairs.push((b, *c));
        }
    }
    pairs
}

/// A failed trial (`status != Ok`) counts as correctness `0.0` in every
/// mean and every bootstrap -- it never simply drops out of the
/// denominator.
fn trial_correctness(o: &Observation) -> f64 {
    if o.status == TrialStatus::Ok {
        o.correctness.unwrap_or(0.0)
    } else {
        0.0
    }
}

/// A failed trial counts as quality `0.0` when a quality judge exists for
/// this task (i.e. either side of the pair recorded a real quality value),
/// and stays unmeasured (`None`) when the task has no quality judge at all.
fn trial_quality(o: &Observation, judge_present: bool) -> Option<f64> {
    if o.status == TrialStatus::Ok {
        o.quality
    } else if judge_present {
        Some(0.0)
    } else {
        None
    }
}

fn pair_cost_complete(o: &Observation) -> bool {
    o.cost_complete && o.cost_usd.is_some()
}

/// A small mix of the seed, the cohort name and an axis index so that
/// different bootstraps within one `evaluate` call draw independent (but
/// still fully deterministic-for-`seed`) resample sequences, rather than
/// resampling every axis of every cohort in lockstep.
fn seed_for(seed: u64, cohort: &str, axis: u64) -> u64 {
    let mut h = seed ^ axis.wrapping_mul(0x9E37_79B9_7F4A_7C15);
    for b in cohort.bytes() {
        h = (h ^ b as u64).wrapping_mul(0x0000_0100_0000_01B3);
    }
    h
}

#[allow(clippy::too_many_arguments)]
fn cohort_decision(
    cohort: &str,
    verdict: Verdict,
    reasons: Vec<String>,
    n_pairs: usize,
    excluded: usize,
    baseline: ArmSummary,
    candidate: ArmSummary,
    d_correctness: Option<Interval>,
    d_quality: Option<Interval>,
    rel_cost: Option<Interval>,
    rel_wall: Option<Interval>,
) -> CohortDecision {
    CohortDecision {
        cohort: cohort.to_string(),
        verdict,
        reasons,
        n_pairs,
        excluded,
        baseline,
        candidate,
        d_correctness,
        d_quality,
        rel_cost,
        rel_wall,
    }
}

fn wall_percentile(vals: &[u64], p: f64) -> u64 {
    let floats: Vec<f64> = vals.iter().map(|v| *v as f64).collect();
    stats::percentile(&floats, p).round() as u64
}

/// Summarizes one arm's non-excluded observations within a cohort: rates,
/// correctness/quality means (with the failed-trial rules from
/// [`trial_correctness`]/[`trial_quality`] applied), cost per success, and
/// wall-time percentiles.
fn summarize_arm(obs: &[&Observation]) -> ArmSummary {
    let n = obs.len();
    if n == 0 {
        return ArmSummary {
            n: 0,
            success_rate: 0.0,
            timeout_rate: 0.0,
            error_rate: 0.0,
            correctness_mean: None,
            quality_mean: None,
            cost_per_success_usd: None,
            cost_complete: true,
            wall_median_ms: None,
            wall_p90_ms: None,
        };
    }

    let ok = obs.iter().filter(|o| o.status == TrialStatus::Ok).count();
    let timeout = obs
        .iter()
        .filter(|o| o.status == TrialStatus::Timeout)
        .count();
    let error = obs
        .iter()
        .filter(|o| matches!(o.status, TrialStatus::Error | TrialStatus::Crash))
        .count();

    let correctness_vals: Vec<f64> = obs.iter().map(|o| trial_correctness(o)).collect();
    let correctness_mean = Some(stats::mean(&correctness_vals));

    let judge_present = obs.iter().any(|o| o.quality.is_some());
    let quality_mean = if judge_present {
        let vals: Vec<f64> = obs
            .iter()
            .filter_map(|o| trial_quality(o, judge_present))
            .collect();
        if vals.is_empty() {
            None
        } else {
            Some(stats::mean(&vals))
        }
    } else {
        None
    };

    let cost_complete = obs.iter().all(|o| o.cost_complete);
    let total_cost: Option<f64> = if obs.iter().all(|o| o.cost_usd.is_some()) {
        Some(obs.iter().filter_map(|o| o.cost_usd).sum())
    } else {
        None
    };
    let cost_per_success_usd = if ok > 0 {
        total_cost.map(|c| c / ok as f64)
    } else {
        None
    };

    let wall: Vec<u64> = obs.iter().map(|o| o.wall_ms).collect();
    let wall_median_ms = Some(wall_percentile(&wall, 50.0));
    let wall_p90_ms = if wall.len() >= 10 {
        Some(wall_percentile(&wall, 90.0))
    } else {
        None
    };

    ArmSummary {
        n,
        success_rate: ok as f64 / n as f64,
        timeout_rate: timeout as f64 / n as f64,
        error_rate: error as f64 / n as f64,
        correctness_mean,
        quality_mean,
        cost_per_success_usd,
        cost_complete,
        wall_median_ms,
        wall_p90_ms,
    }
}

/// Evaluates one cohort's observations against `criteria`, in the fixed
/// order the module doc describes:
///
/// 1. no observations, or all excluded, or zero pairs -> [`Verdict::Unmeasured`].
/// 2. fewer than `min_pairs` pairs -> [`Verdict::Inconclusive`].
/// 3. candidate correctness/quality below its floor -> [`Verdict::Reject`].
/// 4. non-inferiority on correctness, then quality: a delta-CI whose upper
///    bound is still below `-margin` -> [`Verdict::Reject`]; whose lower
///    bound dips below `-margin` (but not the upper) -> [`Verdict::Inconclusive`].
/// 5. benefit on cost (only when every pair's cost is complete) and wall
///    time: a material win (CI upper below `-min_effect`) on one axis with a
///    material loss (CI lower above `+min_effect`) on the other ->
///    [`Verdict::Tradeoff`]; a win with no loss -> [`Verdict::Accept`]; no
///    win at all -> [`Verdict::Inconclusive`], or [`Verdict::Reject`] when
///    both usable point estimates are worse than baseline.
fn evaluate_cohort(
    all_obs: &[Observation],
    cohort: &str,
    criteria: &Criteria,
    confidence: f64,
    seed: u64,
) -> CohortDecision {
    let cohort_obs: Vec<&Observation> = all_obs.iter().filter(|o| o.cohort == cohort).collect();
    let excluded = cohort_obs.iter().filter(|o| o.excluded.is_some()).count();

    let baseline_active: Vec<&Observation> = cohort_obs
        .iter()
        .filter(|o| o.arm == Arm::Baseline && o.excluded.is_none())
        .copied()
        .collect();
    let candidate_active: Vec<&Observation> = cohort_obs
        .iter()
        .filter(|o| o.arm == Arm::Candidate && o.excluded.is_none())
        .copied()
        .collect();
    let baseline_summary = summarize_arm(&baseline_active);
    let candidate_summary = summarize_arm(&candidate_active);

    let pairs = build_pairs(&cohort_obs);

    if !cohort_obs.is_empty() && cohort_obs.iter().all(|o| o.excluded.is_some()) {
        return cohort_decision(
            cohort,
            Verdict::Unmeasured,
            vec![format!("{cohort}: untriggered (all observations excluded)")],
            0,
            excluded,
            baseline_summary,
            candidate_summary,
            None,
            None,
            None,
            None,
        );
    }
    if pairs.is_empty() {
        return cohort_decision(
            cohort,
            Verdict::Unmeasured,
            vec![format!("{cohort}: no pairs")],
            0,
            excluded,
            baseline_summary,
            candidate_summary,
            None,
            None,
            None,
            None,
        );
    }

    let n_pairs = pairs.len();
    if n_pairs < criteria.min_pairs {
        return cohort_decision(
            cohort,
            Verdict::Inconclusive,
            vec![format!(
                "{cohort}: only {n_pairs} pairs (min {})",
                criteria.min_pairs
            )],
            n_pairs,
            excluded,
            baseline_summary,
            candidate_summary,
            None,
            None,
            None,
            None,
        );
    }

    if let Some(mean) = candidate_summary.correctness_mean
        && mean < criteria.correctness_floor
    {
        return cohort_decision(
            cohort,
            Verdict::Reject,
            vec![format!(
                "{cohort}: candidate correctness {mean:.3} below floor {:.3}",
                criteria.correctness_floor
            )],
            n_pairs,
            excluded,
            baseline_summary,
            candidate_summary,
            None,
            None,
            None,
            None,
        );
    }
    if let Some(floor) = criteria.quality_floor
        && let Some(mean) = candidate_summary.quality_mean
        && mean < floor
    {
        return cohort_decision(
            cohort,
            Verdict::Reject,
            vec![format!(
                "{cohort}: candidate quality {mean:.3} below floor {floor:.3}"
            )],
            n_pairs,
            excluded,
            baseline_summary,
            candidate_summary,
            None,
            None,
            None,
            None,
        );
    }

    let correctness_pairs: Vec<(f64, f64)> = pairs
        .iter()
        .map(|(b, c)| (trial_correctness(b), trial_correctness(c)))
        .collect();
    let d_correctness = stats::paired_bootstrap(
        &correctness_pairs,
        stats::mean_diff,
        criteria.bootstrap_resamples,
        confidence,
        seed_for(seed, cohort, 0),
    );
    if let Some(iv) = &d_correctness {
        if iv.hi < -criteria.max_correctness_regression {
            return cohort_decision(
                cohort,
                Verdict::Reject,
                vec![format!(
                    "{cohort}: correctness regression CI [{:.3}, {:.3}] exceeds margin {:.3}",
                    iv.lo, iv.hi, criteria.max_correctness_regression
                )],
                n_pairs,
                excluded,
                baseline_summary,
                candidate_summary,
                d_correctness,
                None,
                None,
                None,
            );
        }
        if iv.lo < -criteria.max_correctness_regression {
            return cohort_decision(
                cohort,
                Verdict::Inconclusive,
                vec![format!(
                    "{cohort}: correctness regression CI lower {:.3} below margin {:.3}",
                    iv.lo, criteria.max_correctness_regression
                )],
                n_pairs,
                excluded,
                baseline_summary,
                candidate_summary,
                d_correctness,
                None,
                None,
                None,
            );
        }
    }

    let quality_pairs: Vec<(f64, f64)> = pairs
        .iter()
        .filter_map(|(b, c)| {
            let judge_present = b.quality.is_some() || c.quality.is_some();
            if !judge_present {
                return None;
            }
            match (
                trial_quality(b, judge_present),
                trial_quality(c, judge_present),
            ) {
                (Some(qb), Some(qc)) => Some((qb, qc)),
                _ => None,
            }
        })
        .collect();
    let d_quality = if quality_pairs.is_empty() {
        None
    } else {
        stats::paired_bootstrap(
            &quality_pairs,
            stats::mean_diff,
            criteria.bootstrap_resamples,
            confidence,
            seed_for(seed, cohort, 1),
        )
    };
    if let Some(iv) = &d_quality {
        if iv.hi < -criteria.max_quality_regression {
            return cohort_decision(
                cohort,
                Verdict::Reject,
                vec![format!(
                    "{cohort}: quality regression CI [{:.3}, {:.3}] exceeds margin {:.3}",
                    iv.lo, iv.hi, criteria.max_quality_regression
                )],
                n_pairs,
                excluded,
                baseline_summary,
                candidate_summary,
                d_correctness,
                d_quality,
                None,
                None,
            );
        }
        if iv.lo < -criteria.max_quality_regression {
            return cohort_decision(
                cohort,
                Verdict::Inconclusive,
                vec![format!(
                    "{cohort}: quality regression CI lower {:.3} below margin {:.3}",
                    iv.lo, criteria.max_quality_regression
                )],
                n_pairs,
                excluded,
                baseline_summary,
                candidate_summary,
                d_correctness,
                d_quality,
                None,
                None,
            );
        }
    }

    let mut reasons: Vec<String> = Vec::new();

    let cost_usable = pairs
        .iter()
        .all(|(b, c)| pair_cost_complete(b) && pair_cost_complete(c));
    let rel_cost = if cost_usable {
        let cost_pairs: Vec<(f64, f64)> = pairs
            .iter()
            .map(|(b, c)| (b.cost_usd.unwrap_or(0.0), c.cost_usd.unwrap_or(0.0)))
            .collect();
        stats::paired_bootstrap(
            &cost_pairs,
            stats::relative_diff,
            criteria.bootstrap_resamples,
            confidence,
            seed_for(seed, cohort, 2),
        )
    } else {
        reasons.push(format!("{cohort}: cost incomplete"));
        None
    };

    let wall_pairs: Vec<(f64, f64)> = pairs
        .iter()
        .map(|(b, c)| (b.wall_ms as f64, c.wall_ms as f64))
        .collect();
    let rel_wall = stats::paired_bootstrap(
        &wall_pairs,
        stats::relative_diff,
        criteria.bootstrap_resamples,
        confidence,
        seed_for(seed, cohort, 3),
    );

    let cost_win = rel_cost
        .as_ref()
        .is_some_and(|iv| iv.hi < -criteria.min_effect);
    let cost_loss = rel_cost
        .as_ref()
        .is_some_and(|iv| iv.lo > criteria.min_effect);
    let wall_win = rel_wall
        .as_ref()
        .is_some_and(|iv| iv.hi < -criteria.min_effect);
    let wall_loss = rel_wall
        .as_ref()
        .is_some_and(|iv| iv.lo > criteria.min_effect);

    let verdict = if (cost_win && wall_loss) || (wall_win && cost_loss) {
        reasons.push(format!(
            "{cohort}: material win on one axis offset by a material loss on the other"
        ));
        Verdict::Tradeoff
    } else if cost_win || wall_win {
        reasons.push(format!(
            "{cohort}: material improvement with no offsetting loss"
        ));
        Verdict::Accept
    } else {
        let cost_worse = rel_cost.as_ref().is_some_and(|iv| iv.point > 0.0);
        let wall_worse = rel_wall.as_ref().is_some_and(|iv| iv.point > 0.0);
        if cost_usable && rel_wall.is_some() && cost_worse && wall_worse {
            reasons.push(format!(
                "{cohort}: no improvement on cost or wall, both worse than baseline"
            ));
            Verdict::Reject
        } else {
            reasons.push(format!("{cohort}: no material win on cost or wall"));
            Verdict::Inconclusive
        }
    };

    cohort_decision(
        cohort,
        verdict,
        reasons,
        n_pairs,
        excluded,
        baseline_summary,
        candidate_summary,
        d_correctness,
        d_quality,
        rel_cost,
        rel_wall,
    )
}

fn overall_verdict(cohorts: &[CohortDecision]) -> Verdict {
    if cohorts.iter().any(|c| c.verdict == Verdict::Reject) {
        Verdict::Reject
    } else if cohorts.iter().any(|c| c.verdict == Verdict::Tradeoff) {
        Verdict::Tradeoff
    } else if cohorts.iter().any(|c| c.verdict == Verdict::Inconclusive) {
        Verdict::Inconclusive
    } else if cohorts.iter().any(|c| c.verdict == Verdict::Accept) {
        Verdict::Accept
    } else {
        Verdict::Unmeasured
    }
}

/// The full promotion gate: evaluates every cohort present in `obs`
/// independently (cohorts are never pooled), then combines their verdicts
/// with the fixed precedence any Reject > any Tradeoff > any Inconclusive >
/// any Accept > Unmeasured.
///
/// `confidence` is the level to use for every bootstrap CI in this call --
/// typically the caller's own [`adjusted_confidence`] result, not
/// `criteria.confidence` directly, since the Bonferroni adjustment depends
/// on how many candidates are being validated together and `evaluate` has
/// no way to know that count itself.
pub fn evaluate(obs: &[Observation], criteria: &Criteria, confidence: f64, seed: u64) -> Decision {
    let mut cohort_names: Vec<&str> = Vec::new();
    for o in obs {
        if !cohort_names.contains(&o.cohort.as_str()) {
            cohort_names.push(o.cohort.as_str());
        }
    }

    let cohorts: Vec<CohortDecision> = cohort_names
        .into_iter()
        .map(|name| evaluate_cohort(obs, name, criteria, confidence, seed))
        .collect();

    let reasons: Vec<String> = cohorts.iter().flat_map(|c| c.reasons.clone()).collect();
    let verdict = overall_verdict(&cohorts);

    Decision {
        verdict,
        reasons,
        cohorts,
        confidence,
    }
}

/// Bonferroni adjustment: `1 - (1 - base) / max(k, 1)`. Used to tighten the
/// confidence level (raise it) when `k` candidates are being validated
/// together, so the chance of any one of them clearing the bar by luck
/// alone stays bounded at roughly `1 - base` overall.
pub fn adjusted_confidence(base: f64, k: usize) -> f64 {
    1.0 - (1.0 - base) / (k.max(1) as f64)
}

/// A cheap, non-bootstrapped pre-filter on the dev split: point estimates
/// only, no confidence intervals, so it never itself promotes a candidate --
/// it can only discard one before the more expensive validated run.
/// Discards when every candidate observation is excluded ("untriggered"),
/// when candidate correctness is below its floor or has regressed past
/// `max_correctness_regression` against baseline, or when neither cost (if
/// every pair's cost is complete) nor wall time improves by at least half
/// of `min_effect`.
pub fn screen(obs: &[Observation], criteria: &Criteria) -> ScreenVerdict {
    let candidate_obs: Vec<&Observation> = obs.iter().filter(|o| o.arm == Arm::Candidate).collect();
    if candidate_obs.is_empty() || candidate_obs.iter().all(|o| o.excluded.is_some()) {
        return ScreenVerdict::Discard {
            reason: "untriggered".to_string(),
        };
    }

    let candidate_active: Vec<&Observation> = candidate_obs
        .iter()
        .filter(|o| o.excluded.is_none())
        .copied()
        .collect();
    let candidate_correctness: Vec<f64> = candidate_active
        .iter()
        .map(|o| trial_correctness(o))
        .collect();
    let candidate_mean = stats::mean(&candidate_correctness);
    if candidate_mean < criteria.correctness_floor {
        return ScreenVerdict::Discard {
            reason: "correctness below floor".to_string(),
        };
    }

    let baseline_active: Vec<&Observation> = obs
        .iter()
        .filter(|o| o.arm == Arm::Baseline && o.excluded.is_none())
        .collect();
    if !baseline_active.is_empty() {
        let baseline_correctness: Vec<f64> = baseline_active
            .iter()
            .map(|o| trial_correctness(o))
            .collect();
        let baseline_mean = stats::mean(&baseline_correctness);
        if candidate_mean < baseline_mean - criteria.max_correctness_regression {
            return ScreenVerdict::Discard {
                reason: "correctness regression vs baseline".to_string(),
            };
        }
    }

    let all_refs: Vec<&Observation> = obs.iter().collect();
    let pairs = build_pairs(&all_refs);
    if pairs.is_empty() {
        return ScreenVerdict::Discard {
            reason: "no pairs".to_string(),
        };
    }

    let cost_usable = pairs
        .iter()
        .all(|(b, c)| pair_cost_complete(b) && pair_cost_complete(c));
    let cost_rel = if cost_usable {
        let cost_pairs: Vec<(f64, f64)> = pairs
            .iter()
            .map(|(b, c)| (b.cost_usd.unwrap_or(0.0), c.cost_usd.unwrap_or(0.0)))
            .collect();
        stats::relative_diff(&cost_pairs)
    } else {
        None
    };
    let wall_pairs: Vec<(f64, f64)> = pairs
        .iter()
        .map(|(b, c)| (b.wall_ms as f64, c.wall_ms as f64))
        .collect();
    let wall_rel = stats::relative_diff(&wall_pairs);

    let half_effect = criteria.min_effect / 2.0;
    let cost_improves = cost_rel.is_some_and(|r| r < -half_effect);
    let wall_improves = wall_rel.is_some_and(|r| r < -half_effect);

    if !cost_improves && !wall_improves {
        return ScreenVerdict::Discard {
            reason: "no material point-estimate improvement".to_string(),
        };
    }

    ScreenVerdict::Survive
}

/// Point-only estimates (no confidence interval -- screen never bootstraps)
/// for the same paired baseline/candidate observations `screen` itself
/// judged, recorded alongside a screen `StageDecision` so a report generated
/// later (purely from the ledger) can show a number instead of leaving the
/// screen row's rel_cost/rel_wall/d_correctness columns blank. Reuses
/// exactly the same pairing and cost-usability rule `screen` uses, so the
/// numbers shown are the ones the gate actually looked at, not a
/// re-derivation that could disagree with it.
#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct ScreenPoints {
    pub d_correctness: Option<f64>,
    pub rel_cost: Option<f64>,
    pub rel_wall: Option<f64>,
}

pub fn screen_points(obs: &[Observation]) -> ScreenPoints {
    let all_refs: Vec<&Observation> = obs.iter().collect();
    let pairs = build_pairs(&all_refs);
    if pairs.is_empty() {
        return ScreenPoints::default();
    }

    let cost_usable = pairs
        .iter()
        .all(|(b, c)| pair_cost_complete(b) && pair_cost_complete(c));
    let rel_cost = if cost_usable {
        let cost_pairs: Vec<(f64, f64)> = pairs
            .iter()
            .map(|(b, c)| (b.cost_usd.unwrap_or(0.0), c.cost_usd.unwrap_or(0.0)))
            .collect();
        stats::relative_diff(&cost_pairs)
    } else {
        None
    };
    let wall_pairs: Vec<(f64, f64)> = pairs
        .iter()
        .map(|(b, c)| (b.wall_ms as f64, c.wall_ms as f64))
        .collect();
    let rel_wall = stats::relative_diff(&wall_pairs);

    let d_correctness_pairs: Vec<f64> = pairs
        .iter()
        .map(|(b, c)| trial_correctness(c) - trial_correctness(b))
        .collect();
    let d_correctness = if d_correctness_pairs.is_empty() {
        None
    } else {
        Some(stats::mean(&d_correctness_pairs))
    };

    ScreenPoints {
        d_correctness,
        rel_cost,
        rel_wall,
    }
}

/// Every observation's own exclusion reason (`untriggered`, `env_mismatch`,
/// ...), counted -- report generation (issue #802) needs this breakdown per
/// candidate/stage, and `CohortDecision.excluded` is only ever a total
/// count, not broken down by reason.
pub fn excluded_by_reason(obs: &[Observation]) -> BTreeMap<String, usize> {
    let mut counts = BTreeMap::new();
    for o in obs {
        if let Some(reason) = &o.excluded {
            *counts.entry(reason.clone()).or_insert(0) += 1;
        }
    }
    counts
}

/// The representative benefit axis and interval for a Decision, used by
/// [`simplest`]: the first cohort with a usable `rel_cost`, else the first
/// cohort with a usable `rel_wall`. A `Decision` in practice represents one
/// candidate's result for the promotion gate it was built from, which
/// commonly holds a single reported cohort; when it holds several, the
/// first cohort's primary axis stands in for the whole decision rather than
/// attempting to merge distinct cohorts' intervals (cohorts are never
/// pooled elsewhere in this module either).
fn primary_interval(decision: &Decision) -> Option<(&'static str, &Interval)> {
    for c in &decision.cohorts {
        if let Some(iv) = &c.rel_cost {
            return Some(("cost", iv));
        }
    }
    for c in &decision.cohorts {
        if let Some(iv) = &c.rel_wall {
            return Some(("wall", iv));
        }
    }
    None
}

fn intervals_overlap(a: &Interval, b: &Interval) -> bool {
    a.lo <= b.hi && b.lo <= a.hi
}

/// Among the `Accept` decisions, finds the best primary benefit (the lowest
/// `rel_cost` point estimate, falling back to `rel_wall` when a decision has
/// no usable cost axis -- see [`primary_interval`]), then returns the
/// lowest-complexity id among every winner whose same-axis interval
/// overlaps the best one's (an "equivalent" win, per the design's
/// simplicity criterion -- a marginally better number is not worth extra
/// complexity when the intervals cannot actually be told apart). Ties in
/// complexity keep the first winner in input order. `None` when there are
/// no `Accept` decisions.
pub fn simplest<'a>(winners: &[(&'a str, usize, &Decision)]) -> Option<&'a str> {
    let accepted: Vec<(&'a str, usize, &Interval, &'static str)> = winners
        .iter()
        .filter(|(_, _, d)| d.verdict == Verdict::Accept)
        .filter_map(|(id, complexity, decision)| {
            primary_interval(decision).map(|(axis, interval)| (*id, *complexity, interval, axis))
        })
        .collect();
    if accepted.is_empty() {
        return None;
    }

    let best = accepted
        .iter()
        .min_by(|a, b| a.2.point.partial_cmp(&b.2.point).unwrap())
        .unwrap();
    let best_axis = best.3;
    let best_interval = best.2;

    let mut equivalents: Vec<&(&'a str, usize, &Interval, &'static str)> = accepted
        .iter()
        .filter(|(_, _, interval, axis)| {
            *axis == best_axis && intervals_overlap(interval, best_interval)
        })
        .collect();
    equivalents.sort_by_key(|(_, complexity, _, _)| *complexity);

    equivalents.first().map(|(id, _, _, _)| *id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[allow(clippy::too_many_arguments)]
    fn obs(
        task: &str,
        rep: u32,
        cohort: &str,
        arm: Arm,
        status: TrialStatus,
        correctness: Option<f64>,
        quality: Option<f64>,
        cost_usd: Option<f64>,
        cost_complete: bool,
        wall_ms: u64,
    ) -> Observation {
        Observation {
            task: task.to_string(),
            rep,
            cohort: cohort.to_string(),
            arm,
            status,
            correctness,
            quality,
            cost_usd,
            cost_complete,
            wall_ms,
            excluded: None,
        }
    }

    fn fast_criteria() -> Criteria {
        Criteria {
            bootstrap_resamples: 500,
            ..Criteria::default()
        }
    }

    /// Builds `n` matched pairs for one cohort with the given per-pair
    /// baseline/candidate correctness, cost and wall time, all trials `Ok`.
    #[allow(clippy::too_many_arguments)]
    fn matched_pairs(
        cohort: &str,
        n: u32,
        base_correctness: f64,
        cand_correctness: f64,
        base_cost: f64,
        cand_cost: f64,
        base_wall: u64,
        cand_wall: u64,
    ) -> Vec<Observation> {
        let mut out = Vec::new();
        for rep in 0..n {
            let task = format!("task-{rep}");
            out.push(obs(
                &task,
                rep,
                cohort,
                Arm::Baseline,
                TrialStatus::Ok,
                Some(base_correctness),
                None,
                Some(base_cost),
                true,
                base_wall,
            ));
            out.push(obs(
                &task,
                rep,
                cohort,
                Arm::Candidate,
                TrialStatus::Ok,
                Some(cand_correctness),
                None,
                Some(cand_cost),
                true,
                cand_wall,
            ));
        }
        out
    }

    #[test]
    fn cheaper_but_incorrect_candidate_is_rejected() {
        let obs = matched_pairs("cohort-a", 6, 1.0, 0.5, 1.0, 0.2, 1000, 900);
        let decision = evaluate(&obs, &fast_criteria(), 0.95, 1);
        assert_eq!(decision.verdict, Verdict::Reject);
        assert_eq!(decision.cohorts[0].verdict, Verdict::Reject);
    }

    #[test]
    fn lucky_one_run_winner_is_inconclusive() {
        let obs = matched_pairs("cohort-a", 1, 1.0, 1.0, 1.0, 0.05, 1000, 1000);
        let decision = evaluate(&obs, &fast_criteria(), 0.95, 1);
        assert_eq!(decision.verdict, Verdict::Inconclusive);
        assert_eq!(decision.cohorts[0].n_pairs, 1);
    }

    #[test]
    fn missing_cost_in_any_pair_blocks_cost_based_accept() {
        let mut obs = matched_pairs("cohort-a", 6, 0.9, 0.9, 1.0, 0.2, 1000, 1000);
        // Blank out the cost on one candidate observation only.
        let victim = obs
            .iter_mut()
            .find(|o| o.arm == Arm::Candidate && o.task == "task-0")
            .unwrap();
        victim.cost_usd = None;
        victim.cost_complete = false;

        let decision = evaluate(&obs, &fast_criteria(), 0.95, 1);
        assert_eq!(decision.verdict, Verdict::Inconclusive);
        assert!(decision.cohorts[0].rel_cost.is_none());
        assert!(
            decision
                .reasons
                .iter()
                .any(|r| r.contains("cost incomplete"))
        );
    }

    #[test]
    fn wall_time_win_with_complete_correctness_still_accepts() {
        // Cost is present but flat (no win); wall time improves by 50%.
        let obs = matched_pairs("cohort-a", 8, 0.9, 0.9, 1.0, 1.0, 2000, 1000);
        let decision = evaluate(&obs, &fast_criteria(), 0.95, 1);
        assert_eq!(decision.verdict, Verdict::Accept);
        assert_eq!(decision.cohorts[0].verdict, Verdict::Accept);
    }

    #[test]
    fn clear_win_with_enough_pairs_is_accepted() {
        let obs = matched_pairs("cohort-a", 8, 0.9, 0.9, 1.0, 0.2, 2000, 1000);
        let decision = evaluate(&obs, &fast_criteria(), 0.95, 1);
        assert_eq!(decision.verdict, Verdict::Accept);
    }

    #[test]
    fn cost_win_and_wall_loss_is_a_tradeoff() {
        let obs = matched_pairs("cohort-a", 8, 0.9, 0.9, 1.0, 0.2, 1000, 2000);
        let decision = evaluate(&obs, &fast_criteria(), 0.95, 1);
        assert_eq!(decision.verdict, Verdict::Tradeoff);
    }

    #[test]
    fn failed_trials_count_as_zero_correctness_and_keep_their_cost() {
        let mut observations = matched_pairs("cohort-a", 6, 1.0, 1.0, 1.0, 0.5, 1000, 1000);
        let failed = observations
            .iter_mut()
            .find(|o| o.arm == Arm::Candidate && o.task == "task-0")
            .unwrap();
        failed.status = TrialStatus::Error;
        failed.correctness = None;
        // Cost is still recorded for the failed trial.
        failed.cost_usd = Some(0.5);
        failed.cost_complete = true;

        let decision = evaluate(&observations, &fast_criteria(), 0.95, 1);
        let candidate = &decision.cohorts[0].candidate;
        assert_eq!(candidate.n, 6);
        assert_eq!(candidate.success_rate, 5.0 / 6.0);
        // (1+1+1+1+1+0) / 6
        assert!((candidate.correctness_mean.unwrap() - 5.0 / 6.0).abs() < 1e-9);
        // total cost 6 * 0.5 = 3.0, over 5 successes.
        assert!((candidate.cost_per_success_usd.unwrap() - 3.0 / 5.0).abs() < 1e-9);
    }

    #[test]
    fn all_excluded_observations_are_unmeasured() {
        let mut observations = matched_pairs("cohort-a", 6, 1.0, 1.0, 1.0, 0.2, 1000, 1000);
        for o in &mut observations {
            o.excluded = Some("untriggered".to_string());
        }
        let decision = evaluate(&observations, &fast_criteria(), 0.95, 1);
        assert_eq!(decision.verdict, Verdict::Unmeasured);
        assert_eq!(decision.cohorts[0].verdict, Verdict::Unmeasured);
        assert!(decision.reasons.iter().any(|r| r.contains("untriggered")));
    }

    #[test]
    fn two_cohorts_are_never_pooled() {
        let mut observations = matched_pairs("cohort-accept", 8, 0.9, 0.9, 1.0, 0.2, 2000, 1000);
        observations.extend(matched_pairs(
            "cohort-reject",
            6,
            1.0,
            0.5,
            1.0,
            0.2,
            1000,
            900,
        ));
        let decision = evaluate(&observations, &fast_criteria(), 0.95, 1);
        assert_eq!(decision.verdict, Verdict::Reject);
        assert_eq!(decision.cohorts.len(), 2);
        let accept_cohort = decision
            .cohorts
            .iter()
            .find(|c| c.cohort == "cohort-accept")
            .unwrap();
        let reject_cohort = decision
            .cohorts
            .iter()
            .find(|c| c.cohort == "cohort-reject")
            .unwrap();
        assert_eq!(accept_cohort.verdict, Verdict::Accept);
        assert_eq!(reject_cohort.verdict, Verdict::Reject);
    }

    #[test]
    fn adjusted_confidence_matches_bonferroni_example() {
        assert!((adjusted_confidence(0.95, 5) - 0.99).abs() < 1e-12);
        assert_eq!(adjusted_confidence(0.95, 0), adjusted_confidence(0.95, 1));
    }

    #[test]
    fn screen_discards_when_candidate_untriggered() {
        let mut observations = matched_pairs("cohort-a", 6, 0.9, 0.9, 1.0, 0.2, 2000, 1000);
        for o in observations.iter_mut().filter(|o| o.arm == Arm::Candidate) {
            o.excluded = Some("untriggered".to_string());
        }
        let verdict = screen(&observations, &Criteria::default());
        assert_eq!(
            verdict,
            ScreenVerdict::Discard {
                reason: "untriggered".to_string()
            }
        );
    }

    #[test]
    fn screen_discards_below_correctness_floor() {
        let observations = matched_pairs("cohort-a", 6, 0.9, 0.1, 1.0, 0.2, 2000, 1000);
        let criteria = Criteria {
            correctness_floor: 0.5,
            ..Criteria::default()
        };
        let verdict = screen(&observations, &criteria);
        assert_eq!(
            verdict,
            ScreenVerdict::Discard {
                reason: "correctness below floor".to_string()
            }
        );
    }

    #[test]
    fn screen_discards_on_regression_vs_baseline() {
        let observations = matched_pairs("cohort-a", 6, 0.9, 0.8, 1.0, 0.2, 2000, 1000);
        let criteria = Criteria {
            max_correctness_regression: 0.02,
            ..Criteria::default()
        };
        let verdict = screen(&observations, &criteria);
        assert_eq!(
            verdict,
            ScreenVerdict::Discard {
                reason: "correctness regression vs baseline".to_string()
            }
        );
    }

    #[test]
    fn screen_discards_when_no_material_point_estimate_improvement() {
        let observations = matched_pairs("cohort-a", 6, 0.9, 0.9, 1.0, 1.0, 1000, 1000);
        let verdict = screen(&observations, &Criteria::default());
        assert_eq!(
            verdict,
            ScreenVerdict::Discard {
                reason: "no material point-estimate improvement".to_string()
            }
        );
    }

    #[test]
    fn screen_survives_material_point_estimate_improvement() {
        let observations = matched_pairs("cohort-a", 6, 0.9, 0.9, 1.0, 0.2, 1000, 1000);
        let verdict = screen(&observations, &Criteria::default());
        assert_eq!(verdict, ScreenVerdict::Survive);
    }

    /// Regression for report-completeness item 1: a screen `StageDecision`
    /// used to record only its verdict/reason, leaving a report's
    /// rel_cost/rel_wall/d_correctness columns blank for every screen row.
    /// `screen_points` must report the same point estimates `screen` itself
    /// judged (an 80% cheaper candidate with a correctness deficit).
    #[test]
    fn screen_points_reports_the_same_point_estimates_screen_itself_used() {
        let observations = matched_pairs("cohort-a", 6, 0.9, 0.8, 1.0, 0.2, 1000, 1100);
        let points = screen_points(&observations);
        assert!(
            (points.d_correctness.unwrap() - (-0.1)).abs() < 1e-9,
            "got {:?}",
            points.d_correctness
        );
        assert!(
            (points.rel_cost.unwrap() - (-0.8)).abs() < 1e-9,
            "got {:?}",
            points.rel_cost
        );
        assert!(
            (points.rel_wall.unwrap() - 0.1).abs() < 1e-9,
            "got {:?}",
            points.rel_wall
        );
    }

    /// An incomplete cost on any pair must leave `rel_cost` unset, the same
    /// "cost incomplete" rule `screen` itself enforces before ever calling
    /// it a cost-based improvement.
    #[test]
    fn screen_points_leaves_rel_cost_unset_when_any_pair_cost_is_incomplete() {
        let mut observations = matched_pairs("cohort-a", 6, 0.9, 0.9, 1.0, 0.2, 1000, 1000);
        let victim = observations
            .iter_mut()
            .find(|o| o.arm == Arm::Candidate && o.task == "task-0")
            .unwrap();
        victim.cost_usd = None;
        victim.cost_complete = false;

        let points = screen_points(&observations);
        assert!(points.rel_cost.is_none());
        assert!(points.rel_wall.is_some());
    }

    #[test]
    fn excluded_by_reason_counts_each_reason_separately() {
        let mut observations = matched_pairs("cohort-a", 3, 0.9, 0.9, 1.0, 0.9, 1000, 1000);
        observations[0].excluded = Some("untriggered".to_string());
        observations[1].excluded = Some("env_mismatch".to_string());
        observations[3].excluded = Some("untriggered".to_string());

        let counts = excluded_by_reason(&observations);
        assert_eq!(counts.get("untriggered"), Some(&2));
        assert_eq!(counts.get("env_mismatch"), Some(&1));
        assert_eq!(counts.len(), 2);
    }

    fn accept_decision(rel_cost_point: f64) -> Decision {
        let cohort = CohortDecision {
            cohort: "cohort-a".to_string(),
            verdict: Verdict::Accept,
            reasons: vec![],
            n_pairs: 8,
            excluded: 0,
            baseline: ArmSummary {
                n: 8,
                success_rate: 1.0,
                timeout_rate: 0.0,
                error_rate: 0.0,
                correctness_mean: Some(0.9),
                quality_mean: None,
                cost_per_success_usd: Some(1.0),
                cost_complete: true,
                wall_median_ms: Some(1000),
                wall_p90_ms: None,
            },
            candidate: ArmSummary {
                n: 8,
                success_rate: 1.0,
                timeout_rate: 0.0,
                error_rate: 0.0,
                correctness_mean: Some(0.9),
                quality_mean: None,
                cost_per_success_usd: Some(1.0 + rel_cost_point),
                cost_complete: true,
                wall_median_ms: Some(1000),
                wall_p90_ms: None,
            },
            d_correctness: None,
            d_quality: None,
            rel_cost: Some(Interval {
                point: rel_cost_point,
                lo: rel_cost_point - 0.02,
                hi: rel_cost_point + 0.02,
            }),
            rel_wall: None,
        };
        Decision {
            verdict: Verdict::Accept,
            reasons: vec![],
            cohorts: vec![cohort],
            confidence: 0.95,
        }
    }

    #[test]
    fn simplest_prefers_lowest_complexity_among_overlapping_winners() {
        // All three have overlapping cost intervals around -0.20; the
        // cheapest is complexity 3, but a simpler complexity-1 candidate
        // is statistically indistinguishable from it.
        let a = accept_decision(-0.21);
        let b = accept_decision(-0.20);
        let c = accept_decision(-0.19);
        let winners: Vec<(&str, usize, &Decision)> =
            vec![("complex", 3, &a), ("simple", 1, &b), ("medium", 2, &c)];
        assert_eq!(simplest(&winners), Some("simple"));
    }

    #[test]
    fn simplest_is_none_without_any_accept() {
        let winners: Vec<(&str, usize, &Decision)> = vec![];
        assert_eq!(simplest(&winners), None);
    }
}
