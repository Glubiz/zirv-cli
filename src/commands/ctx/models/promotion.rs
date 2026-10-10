//! The new-model promotion gate.
//!
//! A family version above the incumbent is held on probation: dispatch keeps resolving the
//! incumbent until probe evidence says the candidate is not worse. [`update`] is the pure
//! state machine; [`refresh`] feeds it the registry and the evidence store.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use super::super::catalogue::{self, DiscoveredModel};
use super::super::config::{CtxConfig, RoutingConfig};
use super::super::state::StateDir;
use super::evidence::{self, Evidence, Verdict, compare};
use super::{CtxResult, load_registry, read_json, write_json};

pub(crate) const PROMOTIONS_FILE: &str = "promotions.json";
const PROMOTIONS_VERSION: u32 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CandidateStatus {
    Probation,
    Rejected,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CandidateState {
    pub first_seen: u64,
    pub status: CandidateStatus,
    pub decided_at: Option<u64>,
    pub last_verdict: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FamilyState {
    pub incumbent: String,
    pub previous: Option<String>,
    pub promoted_at: Option<u64>,
    /// Model key to its state; every entry is held off the dispatch ladder.
    pub candidates: BTreeMap<String, CandidateState>,
    /// Why the incumbent last changed without a verdict (it left the account, or the gate
    /// was off); absent after an evidence-driven promotion.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Promotions {
    pub version: u32,
    pub updated_at: u64,
    /// Keyed `vendor.family`, for example `anthropic.opus`.
    pub families: BTreeMap<String, FamilyState>,
}

/// The harness whose evidence compares a vendor's models.
fn harness_of(vendor: &str) -> Option<&'static str> {
    match vendor {
        "anthropic" => Some("claude"),
        "openai" => Some("codex"),
        _ => None,
    }
}

/// Available ids of each gated family, keyed `vendor.family`, newest first.
fn available_by_family(
    discovered: &[DiscoveredModel],
) -> BTreeMap<String, Vec<(catalogue::ModelVersion, String)>> {
    let mut out: BTreeMap<String, Vec<(catalogue::ModelVersion, String)>> = BTreeMap::new();
    for model in discovered.iter().filter(|model| model.available) {
        if harness_of(&model.vendor).is_none() {
            continue;
        }
        let Some((family, version)) = catalogue::family_and_version(&model.vendor, &model.id)
        else {
            continue;
        };
        let id = catalogue::normalize_id(&model.id).to_lowercase();
        let entry = out.entry(format!("{}.{family}", model.vendor)).or_default();
        if !entry.iter().any(|(_, known)| *known == id) {
            entry.push((version, id));
        }
    }
    for ids in out.values_mut() {
        ids.sort_by(|a, b| b.cmp(a));
    }
    out
}

fn version_of(vendor: &str, id: &str) -> Option<catalogue::ModelVersion> {
    catalogue::family_and_version(vendor, id).map(|(_, version)| version)
}

/// Advance the gate one step. Pure: identical inputs give an identical result.
///
/// With `enabled` or `hold_new_models` off every family's incumbent is simply the newest
/// available id and nothing is held, which is exactly today's resolution.
pub fn update(
    prev: &Promotions,
    discovered: &[DiscoveredModel],
    evidence: &Evidence,
    cfg: &RoutingConfig,
    now: u64,
) -> Promotions {
    let gate = cfg.enabled && cfg.hold_new_models;
    let mut families = prev.families.clone();
    for (key, available) in available_by_family(discovered) {
        let Some((vendor, _)) = key.split_once('.') else {
            continue;
        };
        let Some(harness) = harness_of(vendor) else {
            continue;
        };
        let Some((_, newest)) = available.first() else {
            continue;
        };
        let state = if gate {
            gate_family(
                families.get(&key),
                vendor,
                harness,
                &available,
                evidence,
                cfg.tolerance,
                now,
            )
        } else {
            let unchanged = families
                .get(&key)
                .is_some_and(|old| old.incumbent == *newest && old.candidates.is_empty());
            if unchanged {
                families[&key].clone()
            } else {
                FamilyState {
                    incumbent: newest.clone(),
                    previous: None,
                    promoted_at: None,
                    candidates: BTreeMap::new(),
                    reason: Some(if cfg.enabled {
                        "new models are adopted at once (hold_new_models is off)".to_string()
                    } else {
                        "routing is disabled".to_string()
                    }),
                }
            }
        };
        families.insert(key, state);
    }
    Promotions {
        version: PROMOTIONS_VERSION,
        updated_at: now,
        families,
    }
}

fn gate_family(
    old: Option<&FamilyState>,
    vendor: &str,
    harness: &str,
    available: &[(catalogue::ModelVersion, String)],
    evidence: &Evidence,
    tolerance: f64,
    now: u64,
) -> FamilyState {
    let is_available = |id: &str| available.iter().any(|(_, known)| known == id);
    let Some(old) = old else {
        // Bootstrap: nothing changes on upgrade, the newest id the account has is the incumbent.
        return FamilyState {
            incumbent: available[0].1.clone(),
            previous: None,
            promoted_at: None,
            candidates: BTreeMap::new(),
            reason: None,
        };
    };
    let mut state = old.clone();
    // Only ids the account still offers can be probed or promoted.
    state.candidates.retain(|id, _| is_available(id));
    if state
        .previous
        .as_deref()
        .is_some_and(|id| !is_available(id))
    {
        state.previous = None;
    }

    let pooled = |id: &str| evidence.pooled(harness, id);

    if !is_available(&state.incumbent) {
        let qualifying = state
            .candidates
            .iter()
            .filter(|(_, candidate)| candidate.status == CandidateStatus::Probation)
            .filter_map(|(id, _)| {
                let cell = pooled(id)?;
                let mean = cell
                    .synthetic
                    .mean
                    .filter(|_| cell.synthetic.n >= evidence::MIN_SYNTH)?;
                Some((mean, version_of(vendor, id)?, id.clone()))
            })
            .max_by(|a, b| a.0.total_cmp(&b.0).then_with(|| a.1.cmp(&b.1)))
            .map(|(_, _, id)| id);
        let replacement = qualifying.unwrap_or_else(|| available[0].1.clone());
        let reason = format!("incumbent {} left the account", state.incumbent);
        state.candidates.remove(&replacement);
        state.incumbent = replacement;
        state.previous = None;
        state.promoted_at = Some(now);
        state.reason = Some(reason);
    }

    let incumbent_version = version_of(vendor, &state.incumbent);
    // A version above the incumbent that is not yet tracked starts on probation.
    for (version, id) in available {
        if Some(*version) > incumbent_version && !state.candidates.contains_key(id) {
            state.candidates.insert(
                id.clone(),
                CandidateState {
                    first_seen: now,
                    status: CandidateStatus::Probation,
                    decided_at: None,
                    last_verdict: None,
                },
            );
        }
    }
    state
        .candidates
        .retain(|id, _| version_of(vendor, id) > incumbent_version);

    // Evaluate every candidate; a rejected one keeps being re-evaluated as evidence arrives.
    let incumbent_cell = pooled(&state.incumbent);
    let mut eligible: Vec<(catalogue::ModelVersion, String)> = Vec::new();
    for (id, candidate) in &mut state.candidates {
        let verdict = match (pooled(id), &incumbent_cell) {
            (Some(cell), Some(incumbent)) => compare(&cell, incumbent, tolerance),
            _ => Verdict::Insufficient,
        };
        candidate.last_verdict = Some(verdict.label().to_string());
        match verdict {
            Verdict::Better | Verdict::NonInferior => {
                if let Some(version) = version_of(vendor, id) {
                    eligible.push((version, id.clone()));
                }
            }
            Verdict::Inferior => {
                if candidate.status != CandidateStatus::Rejected {
                    candidate.status = CandidateStatus::Rejected;
                    candidate.decided_at = Some(now);
                }
            }
            Verdict::Insufficient => {}
        }
    }
    if let Some((_, winner)) = eligible.into_iter().max() {
        let old_incumbent = std::mem::replace(&mut state.incumbent, winner.clone());
        state.previous = Some(old_incumbent);
        state.promoted_at = Some(now);
        state.reason = None;
        state.candidates.remove(&winner);
        let version = version_of(vendor, &winner);
        state
            .candidates
            .retain(|id, _| version_of(vendor, id) > version);
    }

    // A promoted model that turns out worse than the one it replaced is swapped back out.
    if let Some(previous) = state.previous.clone()
        && let (Some(current), Some(before)) = (pooled(&state.incumbent), pooled(&previous))
        && compare(&current, &before, tolerance) == Verdict::Inferior
    {
        let demoted = std::mem::replace(&mut state.incumbent, previous);
        state.previous = None;
        state.promoted_at = Some(now);
        state.reason = None;
        state.candidates.insert(
            demoted,
            CandidateState {
                first_seen: now,
                status: CandidateStatus::Rejected,
                decided_at: Some(now),
                last_verdict: Some(Verdict::Inferior.label().to_string()),
            },
        );
    }
    state
}

/// Every candidate in probation or rejected: the ids dispatch must not resolve.
pub fn held_ids(promotions: &Promotions) -> BTreeSet<String> {
    promotions
        .families
        .values()
        .flat_map(|family| family.candidates.keys().cloned())
        .collect()
}

/// Candidates on probation for `vendor`, newest version first.
pub fn probation_candidates(promotions: &Promotions, vendor: &str) -> Vec<String> {
    let prefix = format!("{vendor}.");
    let mut out: Vec<(catalogue::ModelVersion, String)> = promotions
        .families
        .iter()
        .filter(|(key, _)| key.starts_with(&prefix))
        .flat_map(|(_, family)| &family.candidates)
        .filter(|(_, candidate)| candidate.status == CandidateStatus::Probation)
        .filter_map(|(id, _)| Some((version_of(vendor, id)?, id.clone())))
        .collect();
    out.sort_by(|a, b| b.cmp(a));
    out.into_iter().map(|(_, id)| id).collect()
}

/// Advance the stored gate with the current registry and evidence. A no-op while
/// `[models] discovery` is off, since nothing is then discovered to gate.
pub(crate) fn refresh(state: &StateDir, cfg: &CtxConfig, now: u64) -> CtxResult<Promotions> {
    let prev = load(state).unwrap_or_default();
    if !cfg.models.discovery {
        return Ok(prev);
    }
    let discovered = super::discovered_models(&load_registry(state));
    let evidence = evidence::load(state).unwrap_or_default();
    let next = update(&prev, &discovered, &evidence, &cfg.routing, now);
    if next.families != prev.families {
        write_json(&state.root().join(PROMOTIONS_FILE), &next)?;
    }
    Ok(next)
}

pub(crate) fn load(state: &StateDir) -> Option<Promotions> {
    read_json(&state.root().join(PROMOTIONS_FILE))
}

/// The PROMOTIONS section of `zirv ctx models`: each family's incumbent and previous model,
/// then one line per held candidate with its status and last verdict.
pub(crate) fn render(
    promotions: &Promotions,
    routing: &RoutingConfig,
    w: &mut dyn std::io::Write,
) -> std::io::Result<()> {
    if !routing.enabled {
        return writeln!(w, "\nPROMOTIONS\tdisabled ([routing] enabled = false)");
    }
    if !routing.hold_new_models {
        return writeln!(
            w,
            "\nPROMOTIONS\tnew models are adopted at once ([routing] hold_new_models = false)"
        );
    }
    if promotions.families.is_empty() {
        return writeln!(w, "\nPROMOTIONS\tno model families tracked yet");
    }
    writeln!(
        w,
        "\nPROMOTIONS (a new model is held until probe evidence says it is not worse)"
    )?;
    writeln!(w, "FAMILY\tINCUMBENT\tPREVIOUS\tCANDIDATE\tSTATUS\tVERDICT")?;
    for (key, family) in &promotions.families {
        let previous = family.previous.as_deref().unwrap_or("-");
        if family.candidates.is_empty() {
            writeln!(
                w,
                "{key}\t{}\t{previous}\t-\t-\t{}",
                family.incumbent,
                family.reason.as_deref().unwrap_or("-")
            )?;
            continue;
        }
        for (id, candidate) in &family.candidates {
            writeln!(
                w,
                "{key}\t{}\t{previous}\t{id}\t{}\t{}",
                family.incumbent,
                match candidate.status {
                    CandidateStatus::Probation => "probation",
                    CandidateStatus::Rejected => "rejected",
                },
                candidate.last_verdict.as_deref().unwrap_or("-")
            )?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::ctx::models::evidence::{
        Cell, CellComplexity, RealStats, RouteRole, SyntheticStats,
    };

    const OLD: &str = "claude-opus-5";
    const NEW: &str = "claude-opus-5-5";
    const KEY: &str = "anthropic.opus";
    const NOW: u64 = 1_000;

    fn found(ids: &[&str]) -> Vec<DiscoveredModel> {
        ids.iter()
            .map(|id| DiscoveredModel::new("anthropic", *id, true))
            .collect()
    }

    fn cell(model: &str, n: usize, mean: f64) -> Cell {
        Cell {
            harness: "claude".into(),
            model: model.into(),
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

    fn evidence(cells: Vec<Cell>) -> Evidence {
        Evidence {
            cells,
            ..Evidence::default()
        }
    }

    fn cfg() -> RoutingConfig {
        RoutingConfig::default()
    }

    fn step(prev: &Promotions, ids: &[&str], evidence: &Evidence, at: u64) -> Promotions {
        update(prev, &found(ids), evidence, &cfg(), at)
    }

    fn bootstrapped() -> Promotions {
        step(&Promotions::default(), &[OLD], &Evidence::default(), NOW)
    }

    fn on_probation() -> Promotions {
        step(&bootstrapped(), &[OLD, NEW], &Evidence::default(), NOW + 1)
    }

    fn family(promotions: &Promotions) -> &FamilyState {
        &promotions.families[KEY]
    }

    #[test]
    fn bootstrap_adopts_the_newest_available_id_and_holds_nothing() {
        let promotions = step(
            &Promotions::default(),
            &[OLD, NEW],
            &Evidence::default(),
            NOW,
        );
        assert_eq!(family(&promotions).incumbent, NEW);
        assert!(family(&promotions).candidates.is_empty());
        assert!(held_ids(&promotions).is_empty());
    }

    #[test]
    fn a_new_version_above_the_incumbent_is_held_on_probation() {
        let promotions = on_probation();
        assert_eq!(family(&promotions).incumbent, OLD);
        assert_eq!(
            family(&promotions).candidates[NEW].status,
            CandidateStatus::Probation
        );
        assert_eq!(held_ids(&promotions), BTreeSet::from([NEW.to_string()]));
        assert_eq!(probation_candidates(&promotions, "anthropic"), [NEW]);
        assert!(probation_candidates(&promotions, "openai").is_empty());
    }

    #[test]
    fn a_non_inferior_candidate_is_promoted_and_leaves_the_candidates() {
        let proof = evidence(vec![cell(OLD, 10, 0.8), cell(NEW, 10, 0.78)]);
        let promotions = step(&on_probation(), &[OLD, NEW], &proof, NOW + 2);
        let family = family(&promotions);
        assert_eq!(family.incumbent, NEW);
        assert_eq!(family.previous.as_deref(), Some(OLD));
        assert_eq!(family.promoted_at, Some(NOW + 2));
        assert!(family.candidates.is_empty());
        assert!(held_ids(&promotions).is_empty());
    }

    #[test]
    fn an_inferior_candidate_is_rejected_and_stays_held() {
        let proof = evidence(vec![cell(OLD, 10, 0.9), cell(NEW, 10, 0.4)]);
        let promotions = step(&on_probation(), &[OLD, NEW], &proof, NOW + 2);
        let family = family(&promotions);
        assert_eq!(family.incumbent, OLD);
        let candidate = &family.candidates[NEW];
        assert_eq!(candidate.status, CandidateStatus::Rejected);
        assert_eq!(candidate.decided_at, Some(NOW + 2));
        assert_eq!(held_ids(&promotions), BTreeSet::from([NEW.to_string()]));
        assert!(probation_candidates(&promotions, "anthropic").is_empty());
    }

    #[test]
    fn insufficient_evidence_leaves_a_candidate_on_probation() {
        let proof = evidence(vec![cell(OLD, 10, 0.9), cell(NEW, 2, 0.1)]);
        let promotions = step(&on_probation(), &[OLD, NEW], &proof, NOW + 2);
        assert_eq!(family(&promotions).incumbent, OLD);
        let candidate = &family(&promotions).candidates[NEW];
        assert_eq!(candidate.status, CandidateStatus::Probation);
        assert_eq!(candidate.last_verdict.as_deref(), Some("insufficient"));
    }

    #[test]
    fn a_rejected_candidate_is_promoted_once_later_evidence_clears_it() {
        let worse = evidence(vec![cell(OLD, 10, 0.9), cell(NEW, 10, 0.4)]);
        let rejected = step(&on_probation(), &[OLD, NEW], &worse, NOW + 2);
        assert_eq!(
            family(&rejected).candidates[NEW].status,
            CandidateStatus::Rejected
        );
        let better = evidence(vec![cell(OLD, 10, 0.9), cell(NEW, 20, 0.95)]);
        let promoted = step(&rejected, &[OLD, NEW], &better, NOW + 3);
        assert_eq!(family(&promoted).incumbent, NEW);
        assert_eq!(family(&promoted).previous.as_deref(), Some(OLD));
        assert!(held_ids(&promoted).is_empty());
    }

    #[test]
    fn the_highest_version_among_eligible_candidates_wins() {
        let newest = "claude-opus-5-6";
        let mut prev = on_probation();
        prev = step(&prev, &[OLD, NEW, newest], &Evidence::default(), NOW + 2);
        let proof = evidence(vec![
            cell(OLD, 10, 0.8),
            cell(NEW, 10, 0.8),
            cell(newest, 10, 0.8),
        ]);
        let promoted = step(&prev, &[OLD, NEW, newest], &proof, NOW + 3);
        assert_eq!(family(&promoted).incumbent, newest);
        assert!(family(&promoted).candidates.is_empty());
    }

    #[test]
    fn a_promoted_model_that_turns_out_worse_is_demoted_and_rejected() {
        let proof = evidence(vec![cell(OLD, 10, 0.8), cell(NEW, 10, 0.8)]);
        let promoted = step(&on_probation(), &[OLD, NEW], &proof, NOW + 2);
        assert_eq!(family(&promoted).incumbent, NEW);
        let regress = evidence(vec![cell(OLD, 10, 0.9), cell(NEW, 30, 0.5)]);
        let demoted = step(&promoted, &[OLD, NEW], &regress, NOW + 3);
        let family = family(&demoted);
        assert_eq!(family.incumbent, OLD);
        assert_eq!(family.previous, None);
        assert_eq!(family.candidates[NEW].status, CandidateStatus::Rejected);
        assert_eq!(held_ids(&demoted), BTreeSet::from([NEW.to_string()]));
    }

    #[test]
    fn an_incumbent_that_leaves_the_account_is_replaced_and_the_reason_recorded() {
        let promotions = step(&on_probation(), &[NEW], &Evidence::default(), NOW + 2);
        let family = family(&promotions);
        assert_eq!(family.incumbent, NEW);
        assert!(family.candidates.is_empty());
        assert!(
            family
                .reason
                .as_deref()
                .is_some_and(|reason| reason.contains(OLD))
        );
    }

    #[test]
    fn a_departing_incumbent_prefers_a_candidate_with_evidence() {
        let newer = "claude-opus-5-6";
        let mut prev = step(
            &bootstrapped(),
            &[OLD, NEW, newer],
            &Evidence::default(),
            NOW + 1,
        );
        assert_eq!(family(&prev).candidates.len(), 2);
        let proof = evidence(vec![cell(NEW, 10, 0.9), cell(newer, 10, 0.5)]);
        prev = step(&prev, &[NEW, newer], &proof, NOW + 2);
        assert_eq!(family(&prev).incumbent, NEW);
    }

    #[test]
    fn hold_new_models_off_adopts_the_newest_at_once_and_holds_nothing() {
        let off = RoutingConfig {
            hold_new_models: false,
            ..RoutingConfig::default()
        };
        let promotions = update(
            &bootstrapped(),
            &found(&[OLD, NEW]),
            &Evidence::default(),
            &off,
            NOW + 1,
        );
        assert_eq!(family(&promotions).incumbent, NEW);
        assert!(held_ids(&promotions).is_empty());
    }

    #[test]
    fn routing_disabled_releases_every_held_candidate() {
        let off = RoutingConfig {
            enabled: false,
            ..RoutingConfig::default()
        };
        let promotions = update(
            &on_probation(),
            &found(&[OLD, NEW]),
            &Evidence::default(),
            &off,
            NOW + 2,
        );
        assert_eq!(family(&promotions).incumbent, NEW);
        assert!(held_ids(&promotions).is_empty());
    }

    #[test]
    fn update_is_idempotent_for_unchanged_inputs() {
        let again = step(&on_probation(), &[OLD, NEW], &Evidence::default(), NOW + 1);
        assert_eq!(again.families, on_probation().families);
    }

    #[test]
    fn refresh_reads_the_registry_and_persists_the_gate() {
        use crate::commands::ctx::models::{REGISTRY_FILE, Registry, RegistryModel};
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::resolve(&|_| Some(tmp.path().display().to_string())).expect("state");
        let registry = |ids: &[&str]| Registry {
            models: ids
                .iter()
                .map(|id| {
                    (
                        format!("anthropic:{id}"),
                        RegistryModel {
                            vendor: "anthropic".into(),
                            id: (*id).into(),
                            available: true,
                            ..RegistryModel::default()
                        },
                    )
                })
                .collect(),
            ..Registry::default()
        };
        let cfg = CtxConfig::default();
        write_json(&state.root().join(REGISTRY_FILE), &registry(&[OLD])).expect("registry");
        refresh(&state, &cfg, NOW).expect("bootstrap");
        assert!(held_ids(&load(&state).expect("promotions")).is_empty());
        write_json(&state.root().join(REGISTRY_FILE), &registry(&[OLD, NEW])).expect("registry");
        refresh(&state, &cfg, NOW + 1).expect("hold");
        let stored = load(&state).expect("promotions");
        assert_eq!(held_ids(&stored), BTreeSet::from([NEW.to_string()]));
        assert_eq!(family(&stored).incumbent, OLD);
    }

    #[test]
    fn families_without_a_parseable_version_are_not_gated() {
        let promotions = update(
            &Promotions::default(),
            &[DiscoveredModel::new("google", "gemini-3", true)],
            &Evidence::default(),
            &cfg(),
            NOW,
        );
        assert!(promotions.families.is_empty());
    }
}
