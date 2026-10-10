//! Evidence-driven model routing.
//!
//! [`choose`] is the pure decision: among candidate (harness, model) pairs it keeps those with
//! enough synthetic probe evidence for the role and complexity, drops any that recorded work
//! says is worse, and picks the cheapest one within `tolerance` of the best quality. [`route`]
//! gathers the candidates from the machine and asks it. Every seam calls this only when the
//! operator made no explicit choice, and a `None` always means "keep today's logic".

use std::collections::BTreeSet;
use std::sync::Arc;

use sha2::{Digest, Sha256};

use super::adapters::{self, Liveness};
use super::catalogue;
use super::config::{CtxConfig, env_from_process};
use super::models::evidence::{Cell, CellComplexity, Evidence, MIN_REAL, MIN_SYNTH, RouteRole};
use super::models::{self, promotion};
use super::state::StateDir;
use crate::commands::workflow::agents::ModelTier;
use crate::commands::workflow::classify::{Complexity, RiskBand};

/// The reserved agent name that lets the router pick the harness as well as the model.
pub(crate) const AUTO: &str = "auto";

/// Strength a high-risk task needs at least, on the catalogue's rung scale.
const HIGH_RISK_MIN_STRENGTH: u8 = 3;
/// Strength an orchestrator seat needs at least.
const ORCHESTRATOR_MIN_STRENGTH: u8 = 2;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RouteQuery {
    pub role: RouteRole,
    /// `None` reads the complexity-`any` rollup.
    pub complexity: Option<Complexity>,
    pub risk: RiskBand,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    pub harness: String,
    pub model: String,
    pub strength: Option<u8>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct RoutePick {
    pub harness: String,
    pub model: String,
    pub quality: f64,
    pub cost_micros: Option<u64>,
    pub reason: String,
}

struct Scored<'a> {
    candidate: &'a Candidate,
    cell: &'a Cell,
    mean: f64,
}

fn strength_floor(q: &RouteQuery) -> u8 {
    let mut floor = 0;
    if q.risk >= RiskBand::High {
        floor = floor.max(HIGH_RISK_MIN_STRENGTH);
    }
    if q.role == RouteRole::Orchestrator {
        floor = floor.max(ORCHESTRATOR_MIN_STRENGTH);
    }
    floor
}

fn cost_of(cell: &Cell) -> Option<u64> {
    cell.synthetic
        .cost_micros_mean
        .or(cell.real.cost_micros_mean)
}

/// The best model for `q` among `candidates`, which the caller orders by tie preference
/// (`[fallback] order`, then registry order). `None` when nothing has enough evidence.
pub fn choose(
    evidence: &Evidence,
    candidates: &[Candidate],
    q: &RouteQuery,
    tolerance: f64,
) -> Option<RoutePick> {
    let complexity = q
        .complexity
        .map_or(CellComplexity::Any, CellComplexity::from);
    let floor = strength_floor(q);
    let scored: Vec<Scored<'_>> = candidates
        .iter()
        .filter(|candidate| candidate.strength.is_none_or(|strength| strength >= floor))
        .filter_map(|candidate| {
            let cell = evidence.cell(&candidate.harness, &candidate.model, q.role, complexity)?;
            if cell.synthetic.n < MIN_SYNTH {
                return None;
            }
            Some(Scored {
                candidate,
                cell,
                mean: cell.synthetic.mean?,
            })
        })
        .collect();

    // Recorded work overrules probes: a candidate whose real success interval lies wholly
    // below the best peer's is out, however well it probed.
    let best_peer_low = scored
        .iter()
        .filter(|s| s.cell.real.n >= MIN_REAL)
        .filter_map(|s| s.cell.real.success.low)
        .fold(None, |best: Option<f64>, low| {
            Some(best.map_or(low, |b| b.max(low)))
        });
    let kept: Vec<&Scored<'_>> = scored
        .iter()
        .filter(|s| {
            let vetoed = s.cell.real.n >= MIN_REAL
                && matches!(
                    (s.cell.real.success.high, best_peer_low),
                    (Some(high), Some(low)) if high < low
                );
            !vetoed
        })
        .collect();

    let best = kept.iter().map(|s| s.mean).fold(f64::MIN, f64::max);
    // `min_by_key` keeps the first of equal keys, which is the caller's tie order.
    let pick = kept
        .iter()
        .filter(|s| s.mean >= best - tolerance)
        .min_by_key(|s| cost_of(s.cell).unwrap_or(u64::MAX))?;
    let cost = cost_of(pick.cell);
    Some(RoutePick {
        harness: pick.candidate.harness.clone(),
        model: pick.candidate.model.clone(),
        quality: pick.mean,
        cost_micros: cost,
        reason: format!(
            "{}/{} evidence: n={}, quality {:.2}, cost {}",
            role_label(pick.cell.role),
            complexity_label(pick.cell.complexity),
            pick.cell.synthetic.n,
            pick.mean,
            cost.map_or_else(|| "unknown".to_string(), dollars),
        ),
    })
}

fn dollars(micros: u64) -> String {
    format!("${:.4}", micros as f64 / 1_000_000.0)
}

fn complexity_label(complexity: CellComplexity) -> &'static str {
    match complexity {
        CellComplexity::Trivial => "trivial",
        CellComplexity::Bounded => "bounded",
        CellComplexity::Substantial => "substantial",
        CellComplexity::Architectural => "architectural",
        CellComplexity::Any => "any",
    }
}

fn role_label(role: RouteRole) -> &'static str {
    match role {
        RouteRole::Orchestrator => "orchestrator",
        RouteRole::Worker => "worker",
        RouteRole::Reviewer => "reviewer",
    }
}

// -- Evidence cache ---------------------------------------------------------

#[cfg(test)]
thread_local! {
    static TEST_EVIDENCE: std::cell::RefCell<Option<Arc<Evidence>>> =
        const { std::cell::RefCell::new(None) };
}

/// Installs `evidence` as the only evidence the router sees on this thread until dropped, so
/// no test reads the machine's real state.
#[cfg(test)]
pub(crate) struct TestEvidence;

#[cfg(test)]
impl TestEvidence {
    pub(crate) fn set(evidence: Evidence) -> Self {
        TEST_EVIDENCE.with(|slot| *slot.borrow_mut() = Some(Arc::new(evidence)));
        Self
    }
}

#[cfg(test)]
impl Drop for TestEvidence {
    fn drop(&mut self) {
        TEST_EVIDENCE.with(|slot| *slot.borrow_mut() = None);
    }
}

#[cfg(not(test))]
static EVIDENCE_CACHE: std::sync::Mutex<Option<(std::path::PathBuf, Option<Arc<Evidence>>)>> =
    std::sync::Mutex::new(None);

/// The evidence store, read once per process so a session's routes and roster line cannot
/// change underneath it.
fn cached_evidence(state: &StateDir) -> Option<Arc<Evidence>> {
    #[cfg(test)]
    {
        let _ = state;
        TEST_EVIDENCE.with(|slot| slot.borrow().clone())
    }
    #[cfg(not(test))]
    {
        let mut cache = EVIDENCE_CACHE.lock().ok()?;
        let root = state.root().to_path_buf();
        if let Some((cached_root, evidence)) = cache.as_ref()
            && *cached_root == root
        {
            return evidence.clone();
        }
        let loaded = models::evidence::load(state).map(Arc::new);
        *cache = Some((root, loaded.clone()));
        loaded
    }
}

// -- Routing ----------------------------------------------------------------

/// Which candidates a route considers, beyond the query.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct Scope<'a> {
    /// Route the model inside this harness only; the harness itself was already chosen.
    pub only: Option<&'a str>,
    /// Never pick this harness.
    pub exclude: Option<&'a str>,
    /// Never pick a model stronger than this rung strength.
    pub max_strength: Option<u8>,
    /// Ignore provider pacing, for output that must not change within a session.
    pub skip_pace: bool,
}

#[cfg(not(test))]
pub(crate) fn machine_presence(name: &str, program: &str) -> Liveness {
    adapters::liveness_probe(name, program)
}

#[cfg(test)]
pub(crate) fn machine_presence(_name: &str, _program: &str) -> Liveness {
    Liveness::Live
}

#[cfg(not(test))]
fn pace_refuses(cfg: &CtxConfig, state: &StateDir, harness: &str) -> bool {
    let now = super::state::now_secs();
    let provider = adapters::provider_for_agent_name(Some(harness));
    let (collector, estimator) = super::pace::current_windows(state, &cfg.pace, now, provider);
    matches!(
        super::pace::spawn_gate(&collector, estimator.as_ref(), now, &cfg.pace),
        super::pace::SpawnGate::Refuse { .. }
    )
}

#[cfg(test)]
fn pace_refuses(_cfg: &CtxConfig, _state: &StateDir, _harness: &str) -> bool {
    false
}

fn candidates_for(
    cfg: &CtxConfig,
    evidence: &Evidence,
    harness: &str,
    role: RouteRole,
) -> Vec<Candidate> {
    let vendor = catalogue::vendor(adapters::provider_for_agent_name(Some(harness)));
    let ladder = vendor
        .map(|vendor| models::ladder_for(cfg, vendor))
        .unwrap_or_default();
    let mut seen = BTreeSet::new();
    let mut names: Vec<String> = Vec::new();
    for rung in &ladder {
        for name in [&rung.alias, &rung.id] {
            let key = catalogue::normalize_id(name).to_lowercase();
            if !key.is_empty() && seen.insert(key.clone()) {
                names.push(key);
            }
        }
    }
    for cell in evidence
        .cells
        .iter()
        .filter(|cell| cell.harness == harness && cell.role == role)
    {
        if seen.insert(cell.model.clone()) {
            names.push(cell.model.clone());
        }
    }
    names
        .into_iter()
        .map(|model| {
            let strength = vendor
                .and_then(|vendor| catalogue::rung_of_in(vendor, &model, &ladder))
                .map(|rung| rung.strength);
            Candidate {
                harness: harness.to_string(),
                model,
                strength,
            }
        })
        .collect()
}

/// [`route`] with its scope and presence oracle spelled out.
pub(crate) fn route_scoped(
    cfg: &CtxConfig,
    state: &StateDir,
    q: &RouteQuery,
    scope: Scope<'_>,
    present: &dyn Fn(&str, &str) -> Liveness,
) -> Option<RoutePick> {
    if !cfg.routing.enabled {
        return None;
    }
    let evidence = cached_evidence(state)?;
    let mut harnesses: Vec<&'static str> = adapters::ADAPTERS
        .iter()
        .map(|(name, _)| *name)
        .filter(|name| scope.only.is_none_or(|only| only == *name))
        .filter(|name| scope.exclude != Some(*name))
        .filter(|name| cfg.agents.is_enabled(name))
        // Harnesses with no evidence for this role cannot win; skip probing them at all.
        .filter(|name| {
            evidence
                .cells
                .iter()
                .any(|cell| cell.harness == *name && cell.role == q.role)
        })
        .collect();
    let order = &cfg.fallback.order;
    harnesses.sort_by_key(|name| {
        order
            .iter()
            .position(|preferred| preferred == name)
            .unwrap_or(usize::MAX)
    });
    let mut candidates = Vec::new();
    for harness in harnesses {
        if scope.only.is_none() {
            let live = matches!(
                adapters::adapter_liveness_with(cfg, harness, None, present),
                Ok((_, Liveness::Live | Liveness::Unknown(_)))
            );
            if !live || (!scope.skip_pace && pace_refuses(cfg, state, harness)) {
                continue;
            }
        }
        candidates.extend(candidates_for(cfg, &evidence, harness, q.role));
    }
    if let Some(cap) = scope.max_strength {
        candidates.retain(|candidate| candidate.strength.is_none_or(|strength| strength <= cap));
    }
    choose(&evidence, &candidates, q, cfg.routing.tolerance)
}

pub fn route(
    cfg: &CtxConfig,
    state: &StateDir,
    q: &RouteQuery,
    only_harness: Option<&str>,
) -> Option<RoutePick> {
    route_scoped(
        cfg,
        state,
        q,
        Scope {
            only: only_harness,
            ..Scope::default()
        },
        &machine_presence,
    )
}

fn machine_state() -> Option<StateDir> {
    StateDir::resolve(&env_from_process()).ok()
}

/// Seats an orchestrator: the chat seat or the proxy's seat. `None` while `chat.model` is
/// set, since that is an explicit choice.
pub(crate) fn route_seat(
    cfg: &CtxConfig,
    complexity: Option<Complexity>,
    risk: RiskBand,
    only_harness: Option<&str>,
    present: &dyn Fn(&str, &str) -> Liveness,
) -> Option<RoutePick> {
    if cfg.chat.model.is_some() || !cfg.routing.enabled {
        return None;
    }
    route_scoped(
        cfg,
        &machine_state()?,
        &RouteQuery {
            role: RouteRole::Orchestrator,
            complexity,
            risk,
        },
        Scope {
            only: only_harness,
            ..Scope::default()
        },
        present,
    )
}

/// The `[model_tiers]`-less model for a workflow seat of `tier` inside `harness`.
pub(crate) fn route_tier(cfg: &CtxConfig, harness: &str, tier: ModelTier) -> Option<String> {
    if !cfg.routing.enabled {
        return None;
    }
    let complexity = match tier {
        ModelTier::Fast => Complexity::Trivial,
        ModelTier::Standard => Complexity::Bounded,
        ModelTier::Deep => Complexity::Substantial,
    };
    route(
        cfg,
        &machine_state()?,
        &RouteQuery {
            role: RouteRole::Worker,
            complexity: Some(complexity),
            risk: RiskBand::Low,
        },
        Some(harness),
    )
    .map(|pick| pick.model)
}

/// The review model for `harness` when `review.<h>` is unset: the best reviewer evidence at
/// or below the rung one under the seat.
pub(crate) fn route_reviewer(cfg: &CtxConfig, harness: &str) -> Option<String> {
    if !cfg.routing.enabled {
        return None;
    }
    let vendor = catalogue::vendor(adapters::provider_for_agent_name(Some(harness)))?;
    let below = catalogue::rung_below(vendor, cfg.chat.model.as_deref());
    let cap = catalogue::rung_of(vendor, below).map(|rung| rung.strength);
    route_scoped(
        cfg,
        &machine_state()?,
        &RouteQuery {
            role: RouteRole::Reviewer,
            complexity: None,
            risk: RiskBand::Medium,
        },
        Scope {
            only: Some(harness),
            max_strength: cap,
            ..Scope::default()
        },
        &machine_presence,
    )
    .map(|pick| pick.model)
}

// -- Workers ----------------------------------------------------------------

/// Where one `zirv agent` delegation goes.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct WorkerPick {
    pub harness: String,
    /// `None` leaves the model to today's worker policy.
    pub model: Option<String>,
    pub reason: String,
    /// The model is a probation candidate sent a share of low-risk work.
    pub canary: bool,
}

/// The inputs of a delegation that routing reads.
pub(crate) struct WorkerRequest<'a> {
    /// The agent as named: a harness, or [`AUTO`].
    pub name: &'a str,
    pub prompt: &'a str,
    pub review: bool,
    /// A model was passed in the flags.
    pub model_pinned: bool,
    pub session: &'a str,
    /// A harness the router must not pick (the seat's own).
    pub exclude: Option<&'a str>,
}

fn worker_model_configured(cfg: &CtxConfig, harness: &str) -> bool {
    match harness {
        "claude" => cfg.worker.claude.is_some(),
        "codex" => cfg.worker.codex.is_some(),
        _ => false,
    }
}

/// Deterministic share of delegations sent to a probation candidate.
pub(crate) fn canary_roll(session: &str, prompt: &str, pct: u8) -> bool {
    if pct == 0 {
        return false;
    }
    let mut hasher = Sha256::new();
    hasher.update(session.as_bytes());
    hasher.update(prompt.as_bytes());
    let digest = hasher.finalize();
    let mut head = [0u8; 8];
    head.copy_from_slice(&digest[..8]);
    u64::from_be_bytes(head) % 100 < u64::from(pct)
}

fn canary_model(cfg: &CtxConfig, state: &StateDir, harness: &str) -> Option<String> {
    let vendor = adapters::provider_for_agent_name(Some(harness));
    let promotions = promotion::load(state)?;
    promotion::probation_candidates(&promotions, vendor)
        .into_iter()
        .next()
        .filter(|_| cfg.routing.enabled)
}

pub(crate) fn route_worker(
    cfg: &CtxConfig,
    state: &StateDir,
    request: &WorkerRequest<'_>,
) -> Option<WorkerPick> {
    if !cfg.routing.enabled || request.model_pinned {
        return None;
    }
    let auto = request.name == AUTO;
    if !auto && worker_model_configured(cfg, request.name) {
        return None;
    }
    let classification = super::proxy::decision::try_classify_request(request.prompt)?;
    let q = RouteQuery {
        role: if request.review {
            RouteRole::Reviewer
        } else {
            RouteRole::Worker
        },
        complexity: Some(classification.complexity),
        risk: classification.risk,
    };
    let pick = route_scoped(
        cfg,
        state,
        &q,
        Scope {
            only: (!auto).then_some(request.name),
            exclude: request.exclude,
            ..Scope::default()
        },
        &machine_presence,
    )
    .filter(|pick| !worker_model_configured(cfg, &pick.harness));
    let harness = pick
        .as_ref()
        .map(|pick| pick.harness.as_str())
        .or((!auto).then_some(request.name))?;
    if q.role == RouteRole::Worker
        && q.risk <= RiskBand::Medium
        && !worker_model_configured(cfg, harness)
        && canary_roll(request.session, request.prompt, cfg.routing.canary_pct)
        && let Some(model) = canary_model(cfg, state, harness)
    {
        return Some(WorkerPick {
            harness: harness.to_string(),
            model: Some(model),
            reason: "canary: a probation model gets a small share of low-risk work".to_string(),
            canary: true,
        });
    }
    pick.map(|pick| WorkerPick {
        harness: pick.harness,
        model: Some(pick.model),
        reason: pick.reason,
        canary: false,
    })
}

// -- Display ----------------------------------------------------------------

const WORKER_COMPLEXITIES: [Complexity; 3] = [
    Complexity::Trivial,
    Complexity::Bounded,
    Complexity::Substantial,
];

fn pick_label(pick: &RoutePick) -> String {
    format!("{}/{}", pick.harness, pick.model)
}

/// The roster's routing line, or `None` while no route has evidence. Uses `present` and no
/// pacing so the line is the same for the life of a session.
pub(crate) fn roster_line(
    cfg: &CtxConfig,
    present: &dyn Fn(&str, &str) -> Liveness,
) -> Option<String> {
    if !cfg.routing.enabled {
        return None;
    }
    let state = machine_state()?;
    let scope = Scope {
        skip_pace: true,
        ..Scope::default()
    };
    let query = |role, complexity| RouteQuery {
        role,
        complexity,
        risk: RiskBand::Low,
    };
    let workers: Vec<String> = WORKER_COMPLEXITIES
        .iter()
        .filter_map(|complexity| {
            let pick = route_scoped(
                cfg,
                &state,
                &query(RouteRole::Worker, Some(*complexity)),
                scope,
                present,
            )?;
            Some(format!(
                "{} -> {}",
                complexity_label((*complexity).into()),
                pick_label(&pick)
            ))
        })
        .collect();
    let reviewer = route_scoped(
        cfg,
        &state,
        &query(RouteRole::Reviewer, None),
        scope,
        present,
    )
    .map(|pick| format!("reviewer -> {}", pick_label(&pick)));
    if workers.is_empty() && reviewer.is_none() {
        return None;
    }
    let mut parts = Vec::new();
    if !workers.is_empty() {
        parts.push(format!("worker {}", workers.join(", ")));
    }
    parts.extend(reviewer);
    Some(format!(
        "- routing: {} -- `zirv agent {AUTO} \"<prompt>\"` picks per task",
        parts.join("; ")
    ))
}

/// The ROUTES section of `zirv ctx models`.
pub(crate) fn render_routes(cfg: &CtxConfig, state: &StateDir) -> String {
    if !cfg.routing.enabled {
        return "\nROUTES\tdisabled ([routing] enabled = false)\n".to_string();
    }
    let rows: [(RouteRole, Option<Complexity>); 6] = [
        (RouteRole::Orchestrator, None),
        (RouteRole::Worker, Some(Complexity::Trivial)),
        (RouteRole::Worker, Some(Complexity::Bounded)),
        (RouteRole::Worker, Some(Complexity::Substantial)),
        (RouteRole::Worker, Some(Complexity::Architectural)),
        (RouteRole::Reviewer, None),
    ];
    let mut out = String::new();
    for (role, complexity) in rows {
        let q = RouteQuery {
            role,
            complexity,
            risk: RiskBand::Low,
        };
        let Some(pick) = route(cfg, state, &q, None) else {
            continue;
        };
        out.push_str(&format!(
            "{}\t{}\t{}\t{}\n",
            role_label(role),
            complexity_label(complexity.map_or(CellComplexity::Any, CellComplexity::from)),
            pick_label(&pick),
            pick.reason
        ));
    }
    if out.is_empty() {
        return "\nROUTES\tno route yet: not enough probe evidence\n".to_string();
    }
    format!(
        "\nROUTES (the best model per role and complexity, from probe and recorded evidence)\nROLE\tCOMPLEXITY\tPICK\tREASON\n{out}"
    )
}

#[cfg(test)]
pub(crate) mod fixtures {
    use super::super::models::evidence::{
        Cell, CellComplexity, RealStats, RouteRole, SyntheticStats,
    };

    /// A cell with `n` probe rows at `mean` quality, costing `cost` micros each, and no real work.
    pub(crate) fn cell(
        harness: &str,
        model: &str,
        role: RouteRole,
        n: usize,
        mean: f64,
        cost: Option<u64>,
    ) -> Cell {
        Cell {
            harness: harness.to_string(),
            model: model.to_string(),
            role,
            complexity: CellComplexity::Any,
            synthetic: SyntheticStats {
                n,
                mean: Some(mean),
                cost_micros_mean: cost,
                ..SyntheticStats::default()
            },
            real: RealStats::default(),
        }
    }

    pub(crate) fn evidence(cells: Vec<Cell>) -> super::Evidence {
        super::Evidence {
            version: 1,
            cells,
            ..super::Evidence::default()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::fixtures::{cell, evidence};
    use super::*;
    use crate::commands::ctx::models::evidence::RealStats;
    use crate::commands::ctx::models::promotion::{
        CandidateState, CandidateStatus, FamilyState, Promotions,
    };
    use crate::commands::ctx::models::scorecard::Rate;

    const TOL: f64 = 0.05;

    fn query(role: RouteRole, risk: RiskBand) -> RouteQuery {
        RouteQuery {
            role,
            complexity: None,
            risk,
        }
    }

    fn candidate(harness: &str, model: &str, strength: Option<u8>) -> Candidate {
        Candidate {
            harness: harness.to_string(),
            model: model.to_string(),
            strength,
        }
    }

    fn worker(harness: &str, model: &str, mean: f64, cost: Option<u64>) -> Cell {
        cell(harness, model, RouteRole::Worker, MIN_SYNTH, mean, cost)
    }

    fn pick_model(evidence: &Evidence, candidates: &[Candidate], q: &RouteQuery) -> Option<String> {
        choose(evidence, candidates, q, TOL).map(|pick| pick.model)
    }

    #[test]
    fn the_cheaper_candidate_within_tolerance_wins() {
        let ev = evidence(vec![
            worker("claude", "big", 0.90, Some(900)),
            worker("claude", "small", 0.87, Some(300)),
        ]);
        let candidates = [
            candidate("claude", "big", None),
            candidate("claude", "small", None),
        ];
        assert_eq!(
            pick_model(&ev, &candidates, &query(RouteRole::Worker, RiskBand::Low)).as_deref(),
            Some("small")
        );
    }

    #[test]
    fn a_cheaper_candidate_beyond_tolerance_loses() {
        let ev = evidence(vec![
            worker("claude", "big", 0.90, Some(900)),
            worker("claude", "small", 0.70, Some(300)),
        ]);
        let candidates = [
            candidate("claude", "big", None),
            candidate("claude", "small", None),
        ];
        assert_eq!(
            pick_model(&ev, &candidates, &query(RouteRole::Worker, RiskBand::Low)).as_deref(),
            Some("big")
        );
    }

    #[test]
    fn unknown_cost_ranks_last_and_ties_keep_candidate_order() {
        let ev = evidence(vec![
            worker("claude", "unpriced", 0.90, None),
            worker("codex", "first", 0.90, Some(500)),
            worker("claude", "second", 0.90, Some(500)),
        ]);
        let candidates = [
            candidate("claude", "unpriced", None),
            candidate("codex", "first", None),
            candidate("claude", "second", None),
        ];
        let pick = choose(
            &ev,
            &candidates,
            &query(RouteRole::Worker, RiskBand::Low),
            TOL,
        )
        .expect("a pick");
        assert_eq!(
            (pick.harness.as_str(), pick.model.as_str()),
            ("codex", "first")
        );
    }

    #[test]
    fn real_evidence_vetoes_the_probe_leader() {
        let mut leader = worker("claude", "probe-star", 0.95, Some(100));
        leader.real = RealStats {
            n: 40,
            k: 8,
            success: Rate::new(8, 40),
            ..RealStats::default()
        };
        let mut steady = worker("codex", "steady", 0.80, Some(200));
        steady.real = RealStats {
            n: 40,
            k: 36,
            success: Rate::new(36, 40),
            ..RealStats::default()
        };
        let ev = evidence(vec![leader, steady]);
        let candidates = [
            candidate("claude", "probe-star", None),
            candidate("codex", "steady", None),
        ];
        assert_eq!(
            pick_model(&ev, &candidates, &query(RouteRole::Worker, RiskBand::Low)).as_deref(),
            Some("steady")
        );
    }

    #[test]
    fn high_risk_work_needs_a_strong_model() {
        let ev = evidence(vec![
            worker("claude", "weak", 0.95, Some(100)),
            worker("claude", "strong", 0.90, Some(900)),
        ]);
        let candidates = [
            candidate("claude", "weak", Some(2)),
            candidate("claude", "strong", Some(4)),
        ];
        assert_eq!(
            pick_model(&ev, &candidates, &query(RouteRole::Worker, RiskBand::Low)).as_deref(),
            Some("weak")
        );
        assert_eq!(
            pick_model(&ev, &candidates, &query(RouteRole::Worker, RiskBand::High)).as_deref(),
            Some("strong")
        );
    }

    #[test]
    fn an_orchestrator_seat_needs_a_model_of_at_least_the_orchestrator_floor() {
        let ev = evidence(vec![
            cell(
                "claude",
                "tiny",
                RouteRole::Orchestrator,
                MIN_SYNTH,
                0.95,
                Some(10),
            ),
            cell(
                "claude",
                "mid",
                RouteRole::Orchestrator,
                MIN_SYNTH,
                0.90,
                Some(500),
            ),
        ]);
        let candidates = [
            candidate("claude", "tiny", Some(1)),
            candidate("claude", "mid", Some(2)),
        ];
        assert_eq!(
            pick_model(
                &ev,
                &candidates,
                &query(RouteRole::Orchestrator, RiskBand::Low)
            )
            .as_deref(),
            Some("mid")
        );
    }

    #[test]
    fn thin_evidence_yields_no_route() {
        let thin = evidence(vec![cell(
            "claude",
            "new",
            RouteRole::Worker,
            MIN_SYNTH - 1,
            0.99,
            Some(1),
        )]);
        let candidates = [candidate("claude", "new", None)];
        let q = query(RouteRole::Worker, RiskBand::Low);
        assert_eq!(pick_model(&thin, &candidates, &q), None);
        assert_eq!(pick_model(&Evidence::default(), &candidates, &q), None);
    }

    #[test]
    fn the_roles_evidence_is_not_borrowed_across_roles() {
        let ev = evidence(vec![worker("claude", "only-worker", 0.9, Some(1))]);
        let candidates = [candidate("claude", "only-worker", None)];
        assert_eq!(
            pick_model(&ev, &candidates, &query(RouteRole::Reviewer, RiskBand::Low)),
            None
        );
    }

    fn state() -> (tempfile::TempDir, StateDir) {
        let dir = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(dir.path().to_path_buf());
        (dir, state)
    }

    fn request<'a>(name: &'a str, prompt: &'a str) -> WorkerRequest<'a> {
        WorkerRequest {
            name,
            prompt,
            review: false,
            model_pinned: false,
            session: "session-1",
            exclude: None,
        }
    }

    fn two_harness_evidence() -> Evidence {
        evidence(vec![
            worker("claude", "claude-pick", 0.90, Some(900)),
            worker("codex", "codex-pick", 0.89, Some(100)),
        ])
    }

    #[test]
    fn zirv_agent_auto_picks_the_harness_and_model_evidence_prefers() {
        let _evidence = TestEvidence::set(two_harness_evidence());
        let (_dir, state) = state();
        let mut cfg = CtxConfig::default();
        cfg.routing.canary_pct = 0;
        let pick = route_worker(&cfg, &state, &request(AUTO, "fix a typo in the readme"))
            .expect("auto routes with evidence");
        assert_eq!(pick.harness, "codex");
        assert_eq!(pick.model.as_deref(), Some("codex-pick"));
        assert!(!pick.canary);
    }

    #[test]
    fn a_named_harness_routes_the_model_inside_it_only() {
        let _evidence = TestEvidence::set(two_harness_evidence());
        let (_dir, state) = state();
        let mut cfg = CtxConfig::default();
        cfg.routing.canary_pct = 0;
        let pick = route_worker(&cfg, &state, &request("claude", "fix a typo in the readme"))
            .expect("claude has evidence");
        assert_eq!(pick.harness, "claude");
        assert_eq!(pick.model.as_deref(), Some("claude-pick"));
    }

    #[test]
    fn auto_never_picks_the_excluded_seat_harness() {
        let _evidence = TestEvidence::set(two_harness_evidence());
        let (_dir, state) = state();
        let mut cfg = CtxConfig::default();
        cfg.routing.canary_pct = 0;
        let mut req = request(AUTO, "fix a typo in the readme");
        req.exclude = Some("codex");
        let pick = route_worker(&cfg, &state, &req).expect("claude remains");
        assert_eq!(pick.harness, "claude");
    }

    #[test]
    fn an_explicit_model_or_configured_worker_model_is_never_rerouted() {
        let _evidence = TestEvidence::set(two_harness_evidence());
        let (_dir, state) = state();
        let mut cfg = CtxConfig::default();
        cfg.routing.canary_pct = 100;

        let mut pinned = request("claude", "fix a typo in the readme");
        pinned.model_pinned = true;
        assert_eq!(route_worker(&cfg, &state, &pinned), None);

        cfg.worker.claude = Some("operator-choice".to_string());
        assert_eq!(
            route_worker(&cfg, &state, &request("claude", "fix a typo in the readme")),
            None
        );
    }

    #[test]
    fn routing_off_leaves_the_delegation_alone() {
        let _evidence = TestEvidence::set(two_harness_evidence());
        let (_dir, state) = state();
        let mut cfg = CtxConfig::default();
        cfg.routing.enabled = false;
        assert_eq!(
            route_worker(&cfg, &state, &request(AUTO, "fix a typo in the readme")),
            None
        );
    }

    #[test]
    fn the_canary_roll_is_deterministic_and_bounded_by_its_share() {
        let rolls: Vec<bool> = (0..200)
            .map(|i| canary_roll("session", &format!("prompt {i}"), 5))
            .collect();
        assert_eq!(
            rolls,
            (0..200)
                .map(|i| canary_roll("session", &format!("prompt {i}"), 5))
                .collect::<Vec<_>>(),
            "same session and prompt always roll the same"
        );
        assert!(rolls.iter().any(|hit| *hit), "5% of 200 prompts hits some");
        assert!(rolls.iter().filter(|hit| **hit).count() < 40);
        assert!((0..50).all(|i| !canary_roll("s", &format!("p{i}"), 0)));
        assert!((0..50).all(|i| canary_roll("s", &format!("p{i}"), 100)));
    }

    fn write_probation(state: &StateDir, vendor: &str, id: &str) {
        let mut family = FamilyState {
            incumbent: "old-model".to_string(),
            previous: None,
            promoted_at: None,
            candidates: Default::default(),
            reason: None,
        };
        family.candidates.insert(
            id.to_string(),
            CandidateState {
                first_seen: 1,
                status: CandidateStatus::Probation,
                decided_at: None,
                last_verdict: None,
            },
        );
        let mut promotions = Promotions::default();
        promotions.families.insert(format!("{vendor}.opus"), family);
        std::fs::create_dir_all(state.root()).expect("mkdir");
        std::fs::write(
            state.root().join(promotion::PROMOTIONS_FILE),
            serde_json::to_string(&promotions).expect("json"),
        )
        .expect("write promotions");
    }

    #[test]
    fn the_canary_swaps_in_a_probation_model_for_low_risk_workers() {
        let _evidence = TestEvidence::set(two_harness_evidence());
        let (_dir, state) = state();
        write_probation(&state, "anthropic", "claude-opus-9-9");
        let mut cfg = CtxConfig::default();
        cfg.routing.canary_pct = 100;
        let pick = route_worker(&cfg, &state, &request("claude", "fix a typo in the readme"))
            .expect("canary");
        assert!(pick.canary);
        assert_eq!(pick.harness, "claude");
        assert_eq!(pick.model.as_deref(), Some("claude-opus-9-9"));
    }

    #[test]
    fn the_canary_bypasses_reviewers_pins_a_zero_share_and_a_missing_candidate() {
        let _evidence = TestEvidence::set(two_harness_evidence());
        let (_dir, state) = state();
        let mut cfg = CtxConfig::default();
        cfg.routing.canary_pct = 100;

        // No probation candidate: the evidence pick stands.
        let pick = route_worker(&cfg, &state, &request("claude", "fix a typo in the readme"))
            .expect("pick");
        assert!(!pick.canary);

        write_probation(&state, "anthropic", "claude-opus-9-9");
        let mut review = request("claude", "fix a typo in the readme");
        review.review = true;
        assert!(
            route_worker(&cfg, &state, &review).is_none_or(|pick| !pick.canary),
            "a reviewer is never a canary"
        );

        cfg.routing.canary_pct = 0;
        let pick = route_worker(&cfg, &state, &request("claude", "fix a typo in the readme"))
            .expect("pick");
        assert!(!pick.canary, "a zero share never swaps");
    }

    #[test]
    fn the_roster_line_is_absent_without_picks_and_stable_with_them() {
        let listed = |_: &str, _: &str| Liveness::Live;
        let cfg = CtxConfig::default();
        assert_eq!(roster_line(&cfg, &listed), None);

        let _evidence = TestEvidence::set(two_harness_evidence());
        let first = roster_line(&cfg, &listed).expect("a worker pick exists");
        assert!(first.starts_with("- routing: worker "), "{first}");
        assert!(first.contains("codex/codex-pick"), "{first}");
        assert!(first.contains("zirv agent auto"), "{first}");
        assert_eq!(roster_line(&cfg, &listed), Some(first));
    }

    #[test]
    fn routes_render_the_picks_and_say_when_there_are_none() {
        let (_dir, state) = state();
        let cfg = CtxConfig::default();
        assert!(render_routes(&cfg, &state).contains("no route yet"));
        let _evidence = TestEvidence::set(two_harness_evidence());
        let text = render_routes(&cfg, &state);
        assert!(text.contains("ROUTES"), "{text}");
        assert!(text.contains("worker\ttrivial\tcodex/codex-pick"), "{text}");
    }
}
