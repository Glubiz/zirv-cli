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

/// Where a model of a family the ladder does not know stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum NewFamilyStatus {
    /// Held off dispatch and probed until it has enough synthetic samples.
    Probation,
    /// Enough samples: no longer held, so the router may pick it on merit.
    Eligible,
    /// Every probe row failed without output; probed again once `unavailable_until` passes.
    Unavailable,
}

/// A model of a brand-new family: a vendor-level candidate with no incumbent and no rung.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NewFamilyState {
    pub vendor: String,
    pub family: String,
    pub first_seen: u64,
    pub status: NewFamilyStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unavailable_until: Option<u64>,
}

/// An unavailable candidate is probed again after this long.
const UNAVAILABLE_BACKOFF_SECS: u64 = 7 * 24 * 3600;
/// A models.dev release date older than this at first sight marks the model as old catalogue.
const MAX_RELEASE_AGE_SECS: u64 = 90 * 24 * 3600;

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Promotions {
    pub version: u32,
    pub updated_at: u64,
    /// Keyed `vendor.family`, for example `anthropic.opus`.
    pub families: BTreeMap<String, FamilyState>,
    /// Models of unknown families, keyed by normalized id.
    pub new_families: BTreeMap<String, NewFamilyState>,
    /// Unknown-family ids that are not candidates: those already known when new-family
    /// tracking began, and those released long before they were first seen. `None` until the
    /// first run, which is what makes that run a bootstrap.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub new_family_baseline: Option<BTreeSet<String>>,
    /// Known-family versions newer than the incumbent that only models.dev lists, by id.
    pub unverified: BTreeMap<String, UnverifiedState>,
}

/// What new-family tracking needs beyond the discovered models.
struct NewFamilyInput<'a> {
    /// models.dev release dates (unix seconds) by normalized id.
    pub released: &'a BTreeMap<String, u64>,
    /// False while the existence source has not been read yet, so the bootstrap waits for it
    /// instead of baselining only the local models.
    pub ready: bool,
    /// Normalized ids whose only registry source is models.dev (never seen on this account).
    pub models_dev_only: &'a BTreeSet<String>,
}

impl NewFamilyInput<'_> {
    /// Released more than 90 days before `now`: old catalogue, not a newcomer.
    fn is_old(&self, id: &str, now: u64) -> bool {
        self.released
            .get(id)
            .is_some_and(|released| now.saturating_sub(*released) > MAX_RELEASE_AGE_SECS)
    }
}

/// A version of a known family that exists only on models.dev and is newer than the family's
/// incumbent: probed to learn whether this account can run it. Not a candidate yet, since it
/// is not available; once a probe proves it runs it enters the ordinary family gate.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UnverifiedState {
    pub vendor: String,
    pub first_seen: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unavailable_until: Option<u64>,
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
fn update_with(
    prev: &Promotions,
    discovered: &[DiscoveredModel],
    evidence: &Evidence,
    cfg: &RoutingConfig,
    now: u64,
    input: &NewFamilyInput<'_>,
) -> Promotions {
    let gate = cfg.enabled && cfg.hold_new_models;
    let (new_families, new_family_baseline) =
        update_new_families(prev, discovered, evidence, gate, input, now);
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
    let unverified = update_unverified(prev, discovered, &families, gate, input, now);
    Promotions {
        version: PROMOTIONS_VERSION,
        updated_at: now,
        families,
        new_families,
        new_family_baseline,
        unverified,
    }
}

/// Known-family ids only models.dev lists, newer than their family's incumbent (else its
/// newest available id), released within the 90-day window. Bootstrap needs nothing special:
/// only versions above the incumbent qualify, and a family with nothing available has none.
fn update_unverified(
    prev: &Promotions,
    discovered: &[DiscoveredModel],
    families: &BTreeMap<String, FamilyState>,
    gate: bool,
    input: &NewFamilyInput<'_>,
    now: u64,
) -> BTreeMap<String, UnverifiedState> {
    if !gate {
        return BTreeMap::new();
    }
    if !input.ready {
        return prev.unverified.clone();
    }
    let available = available_by_family(discovered);
    let mut out = BTreeMap::new();
    for model in discovered.iter().filter(|model| !model.available) {
        let id = catalogue::normalize_id(&model.id).to_lowercase();
        if harness_of(&model.vendor).is_none() || !input.models_dev_only.contains(&id) {
            continue;
        }
        let Some((family, version)) = catalogue::family_and_version(&model.vendor, &id) else {
            continue;
        };
        let key = format!("{}.{family}", model.vendor);
        let incumbent = families
            .get(&key)
            .and_then(|state| version_of(&model.vendor, &state.incumbent))
            .or_else(|| available.get(&key)?.first().map(|(version, _)| *version));
        if incumbent.is_none_or(|incumbent| version <= incumbent) {
            continue;
        }
        if input.is_old(&id, now) {
            continue;
        }
        let mut state = prev
            .unverified
            .get(&id)
            .cloned()
            .unwrap_or(UnverifiedState {
                vendor: model.vendor.clone(),
                first_seen: now,
                unavailable_until: None,
            });
        if state.unavailable_until.is_some_and(|until| until <= now) {
            state.unavailable_until = None;
        }
        out.insert(id, state);
    }
    out
}

/// Unverified versions of `vendor` due a probe (not backed off), newest first.
pub fn unverified_candidates(promotions: &Promotions, vendor: &str, now: u64) -> Vec<String> {
    let mut out: Vec<(catalogue::ModelVersion, String)> = promotions
        .unverified
        .iter()
        .filter(|(_, state)| {
            state.vendor == vendor && state.unavailable_until.is_none_or(|until| until <= now)
        })
        .filter_map(|(id, _)| version_of(vendor, id).map(|version| (version, id.clone())))
        .collect();
    out.sort_by(|a, b| b.cmp(a));
    out.into_iter().map(|(_, id)| id).collect()
}

/// The unknown-family models in `discovered` that belong to a gated vendor, keyed by
/// normalized id, with whether any source marks them available.
fn unknown_family_models(
    discovered: &[DiscoveredModel],
) -> BTreeMap<String, (String, String, bool)> {
    let mut out: BTreeMap<String, (String, String, bool)> = BTreeMap::new();
    for model in discovered {
        if harness_of(&model.vendor).is_none() {
            continue;
        }
        let Some((family, _)) = catalogue::generic_family_and_version(&model.vendor, &model.id)
        else {
            continue;
        };
        if catalogue::is_known_family(&model.vendor, &family) {
            continue;
        }
        let id = catalogue::normalize_id(&model.id).to_lowercase();
        let entry = out
            .entry(id)
            .or_insert_with(|| (model.vendor.clone(), family, false));
        entry.2 |= model.available;
    }
    out
}

/// Advance the new-family candidates: bootstrap the baseline, admit newcomers, lift an expired
/// backoff and mark candidates with enough synthetic samples eligible.
fn update_new_families(
    prev: &Promotions,
    discovered: &[DiscoveredModel],
    evidence: &Evidence,
    gate: bool,
    input: &NewFamilyInput<'_>,
    now: u64,
) -> (BTreeMap<String, NewFamilyState>, Option<BTreeSet<String>>) {
    if !gate {
        return (BTreeMap::new(), prev.new_family_baseline.clone());
    }
    if !input.ready {
        return (prev.new_families.clone(), prev.new_family_baseline.clone());
    }
    let seen = unknown_family_models(discovered);
    let Some(mut baseline) = prev.new_family_baseline.clone() else {
        // Bootstrap: everything known now is the existing catalogue, not a newcomer.
        return (BTreeMap::new(), Some(seen.keys().cloned().collect()));
    };
    let mut candidates = prev.new_families.clone();
    candidates.retain(|id, _| seen.contains_key(id));
    for (id, (vendor, family, _)) in &seen {
        if candidates.contains_key(id) || baseline.contains(id) {
            continue;
        }
        if input.is_old(id, now) {
            baseline.insert(id.clone());
            continue;
        }
        candidates.insert(
            id.clone(),
            NewFamilyState {
                vendor: vendor.clone(),
                family: family.clone(),
                first_seen: now,
                status: NewFamilyStatus::Probation,
                unavailable_until: None,
            },
        );
    }
    for (id, candidate) in &mut candidates {
        if candidate.status == NewFamilyStatus::Unavailable
            && candidate.unavailable_until.is_none_or(|until| until <= now)
        {
            candidate.status = NewFamilyStatus::Probation;
            candidate.unavailable_until = None;
        }
        let sampled = harness_of(&candidate.vendor).is_some_and(|harness| {
            evidence.cells.iter().any(|cell| {
                cell.harness == harness
                    && cell.model == *id
                    && cell.synthetic.n >= evidence::MIN_SYNTH
            })
        });
        if candidate.status == NewFamilyStatus::Probation && sampled {
            candidate.status = NewFamilyStatus::Eligible;
        }
    }
    (candidates, Some(baseline))
}

/// Apply one probe run's availability findings: `(model id, ran)`, where a model that never
/// ran (every row failed without output) is unavailable for a week and one that did run is
/// back on probation if it was unavailable. Eligible candidates are left alone.
fn record_probe_outcomes(prev: &Promotions, outcomes: &[(String, bool)], now: u64) -> Promotions {
    let mut next = prev.clone();
    for (id, ran) in outcomes {
        let key = catalogue::normalize_id(id).to_lowercase();
        if let Some(unverified) = next.unverified.get_mut(&key) {
            // A version that ran is registered as available and leaves the map on the next
            // update, entering the ordinary family gate; one that did not is backed off.
            unverified.unavailable_until = (!ran).then_some(now + UNAVAILABLE_BACKOFF_SECS);
            continue;
        }
        let Some(candidate) = next.new_families.get_mut(&key) else {
            continue;
        };
        match (ran, candidate.status) {
            (_, NewFamilyStatus::Eligible) => {}
            (false, _) => {
                candidate.status = NewFamilyStatus::Unavailable;
                candidate.unavailable_until = Some(now + UNAVAILABLE_BACKOFF_SECS);
            }
            (true, _) => {
                candidate.status = NewFamilyStatus::Probation;
                candidate.unavailable_until = None;
            }
        }
    }
    next.updated_at = now;
    next
}

/// Persist [`record_probe_outcomes`].
pub(crate) fn save_probe_outcomes(
    state: &StateDir,
    outcomes: &[(String, bool)],
    now: u64,
) -> CtxResult<()> {
    let prev = load(state).unwrap_or_default();
    let next = record_probe_outcomes(&prev, outcomes, now);
    if next.new_families != prev.new_families || next.unverified != prev.unverified {
        write_json(&state.root().join(PROMOTIONS_FILE), &next)?;
    }
    Ok(())
}

/// New-family candidates on probation (not unavailable, not eligible) for `vendor`, newest
/// version first.
pub fn new_family_probation(promotions: &Promotions, vendor: &str) -> Vec<String> {
    let mut out: Vec<(catalogue::ModelVersion, String)> = promotions
        .new_families
        .iter()
        .filter(|(_, c)| c.vendor == vendor && c.status == NewFamilyStatus::Probation)
        .filter_map(|(id, _)| {
            catalogue::generic_family_and_version(vendor, id).map(|(_, v)| (v, id.clone()))
        })
        .collect();
    out.sort_by(|a, b| b.cmp(a));
    out.into_iter().map(|(_, id)| id).collect()
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
        .chain(
            promotions
                .new_families
                .iter()
                .filter(|(_, candidate)| candidate.status != NewFamilyStatus::Eligible)
                .map(|(id, _)| id.clone()),
        )
        .chain(promotions.unverified.keys().cloned())
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
    refresh_with(state, cfg, &load_registry(state), now)
}

/// [`refresh`] against a registry not yet on disk, so the gate can hold a new model before
/// `registry.json` exposes it.
pub(crate) fn refresh_with(
    state: &StateDir,
    cfg: &CtxConfig,
    registry: &super::Registry,
    now: u64,
) -> CtxResult<Promotions> {
    let prev = load(state).unwrap_or_default();
    if !cfg.models.discovery {
        return Ok(prev);
    }
    let discovered = super::discovered_models(registry);
    let evidence = evidence::load(state).unwrap_or_default();
    let released: BTreeMap<String, u64> = registry
        .models
        .values()
        .filter_map(|model| Some((model.id.clone(), model.released_at?)))
        .collect();
    let models_dev_only = super::models_dev_only_ids(registry);
    let input = NewFamilyInput {
        released: &released,
        models_dev_only: &models_dev_only,
        // The first bootstrap waits for the existence source, so the whole catalogue is baseline.
        ready: !cfg.models.price_fetch
            || registry
                .models
                .values()
                .any(|model| model.sources.iter().any(|s| s == super::MODELS_DEV_SOURCE)),
    };
    let next = update_with(&prev, &discovered, &evidence, &cfg.routing, now, &input);
    if next.families != prev.families
        || next.new_families != prev.new_families
        || next.new_family_baseline != prev.new_family_baseline
        || next.unverified != prev.unverified
    {
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
    if promotions.families.is_empty()
        && promotions.new_families.is_empty()
        && promotions.unverified.is_empty()
    {
        return writeln!(w, "\nPROMOTIONS\tno model families tracked yet");
    }
    writeln!(
        w,
        "\nPROMOTIONS (a new model is held until probe evidence says it is not worse)"
    )?;
    if !promotions.new_families.is_empty() {
        writeln!(
            w,
            "NEW FAMILY\tMODEL\tSTATUS\tFIRST SEEN\tUNAVAILABLE UNTIL"
        )?;
        for (id, candidate) in &promotions.new_families {
            writeln!(
                w,
                "{}.{}\t{id}\t{}\t{}\t{}",
                candidate.vendor,
                candidate.family,
                match candidate.status {
                    NewFamilyStatus::Probation => "probation",
                    NewFamilyStatus::Eligible => "eligible",
                    NewFamilyStatus::Unavailable => "unavailable",
                },
                super::format_epoch_date(candidate.first_seen),
                candidate
                    .unavailable_until
                    .map_or_else(|| "-".to_string(), super::format_epoch_date),
            )?;
        }
    }
    for (id, state) in &promotions.unverified {
        writeln!(
            w,
            "unverified\t{id}\tprobe pending\t{}\t{}",
            super::format_epoch_date(state.first_seen),
            state
                .unavailable_until
                .map_or_else(|| "-".to_string(), super::format_epoch_date),
        )?;
    }
    if promotions.families.is_empty() {
        return Ok(());
    }
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

    /// [`update_with`] with no new-family inputs.
    fn update(
        prev: &Promotions,
        discovered: &[DiscoveredModel],
        evidence: &Evidence,
        cfg: &RoutingConfig,
        now: u64,
    ) -> Promotions {
        let input = NewFamilyInput {
            released: &BTreeMap::new(),
            ready: true,
            models_dev_only: &BTreeSet::new(),
        };
        update_with(prev, discovered, evidence, cfg, now, &input)
    }
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

    const BEL: &str = "claude-bel-1";
    const DAY: u64 = 24 * 3600;

    fn step_new(
        prev: &Promotions,
        ids: &[&str],
        released: &BTreeMap<String, u64>,
        evidence: &Evidence,
        at: u64,
    ) -> Promotions {
        let input = NewFamilyInput {
            released,
            ready: true,
            models_dev_only: &BTreeSet::new(),
        };
        update_with(prev, &found(ids), evidence, &cfg(), at, &input)
    }

    fn bel_evidence(model: &str, n: usize) -> Evidence {
        evidence(vec![cell(model, n, 0.9)])
    }

    #[test]
    fn bootstrap_baselines_existing_ids_and_a_later_newcomer_is_a_candidate() {
        let none = BTreeMap::new();
        let boot = step_new(
            &Promotions::default(),
            &[OLD, BEL],
            &none,
            &Evidence::default(),
            NOW,
        );
        assert!(boot.new_families.is_empty());
        assert!(held_ids(&boot).is_empty());
        let later = step_new(
            &boot,
            &[OLD, BEL, "claude-bel-2"],
            &none,
            &Evidence::default(),
            NOW + 1,
        );
        assert_eq!(
            held_ids(&later),
            BTreeSet::from(["claude-bel-2".to_string()])
        );
        let candidate = &later.new_families["claude-bel-2"];
        assert_eq!(
            (
                candidate.status,
                candidate.first_seen,
                candidate.family.as_str()
            ),
            (NewFamilyStatus::Probation, NOW + 1, "bel")
        );
        assert_eq!(new_family_probation(&later, "anthropic"), ["claude-bel-2"]);
        // Known-family ids never become new-family candidates.
        assert!(!later.new_families.contains_key(OLD));
    }

    #[test]
    fn bootstrap_waits_for_the_existence_source() {
        let none = BTreeMap::new();
        let input = NewFamilyInput {
            released: &none,
            ready: false,
            models_dev_only: &BTreeSet::new(),
        };
        let waiting = update_with(
            &Promotions::default(),
            &found(&[BEL]),
            &Evidence::default(),
            &cfg(),
            NOW,
            &input,
        );
        assert_eq!(waiting.new_family_baseline, None);
        assert!(waiting.new_families.is_empty());
    }

    #[test]
    fn a_model_released_over_90_days_before_first_seen_is_not_a_candidate() {
        let boot = step_new(
            &Promotions::default(),
            &[OLD],
            &BTreeMap::new(),
            &Evidence::default(),
            NOW,
        );
        let at = 200 * DAY;
        let released = BTreeMap::from([
            ("claude-bel-1".to_string(), at - 91 * DAY),
            ("claude-bel-2".to_string(), at - 89 * DAY),
        ]);
        let next = step_new(
            &boot,
            &[OLD, "claude-bel-1", "claude-bel-2"],
            &released,
            &Evidence::default(),
            at,
        );
        assert_eq!(
            held_ids(&next),
            BTreeSet::from(["claude-bel-2".to_string()])
        );
        let again = step_new(
            &next,
            &[OLD, "claude-bel-1", "claude-bel-2"],
            &released,
            &Evidence::default(),
            at + DAY,
        );
        assert_eq!(
            held_ids(&again),
            BTreeSet::from(["claude-bel-2".to_string()])
        );
    }

    #[test]
    fn a_new_family_is_held_then_eligible_at_min_synth_and_the_router_can_pick_it() {
        use crate::commands::ctx::routing::{Candidate, RouteQuery, choose};
        use crate::commands::workflow::classify::RiskBand;
        let none = BTreeMap::new();
        let boot = step_new(
            &Promotions::default(),
            &[OLD],
            &none,
            &Evidence::default(),
            NOW,
        );
        let held = step_new(&boot, &[OLD, BEL], &none, &bel_evidence(BEL, 4), NOW + 1);
        assert_eq!(held.new_families[BEL].status, NewFamilyStatus::Probation);
        assert!(held_ids(&held).contains(BEL));
        let eligible = step_new(
            &held,
            &[OLD, BEL],
            &none,
            &bel_evidence(BEL, evidence::MIN_SYNTH),
            NOW + 2,
        );
        assert_eq!(eligible.new_families[BEL].status, NewFamilyStatus::Eligible);
        assert!(held_ids(&eligible).is_empty());
        // Eligible is sticky and, with no incumbent, the router compares on merit alone.
        let still = step_new(&eligible, &[OLD, BEL], &none, &Evidence::default(), NOW + 3);
        assert_eq!(still.new_families[BEL].status, NewFamilyStatus::Eligible);
        let candidates = [Candidate {
            harness: "claude".into(),
            model: BEL.into(),
            strength: None,
        }];
        let query = RouteQuery {
            role: RouteRole::Worker,
            complexity: None,
            risk: RiskBand::Low,
        };
        let pick = choose(
            &bel_evidence(BEL, evidence::MIN_SYNTH),
            &candidates,
            &query,
            0.05,
        );
        assert_eq!(pick.map(|p| p.model), Some(BEL.to_string()));
    }

    #[test]
    fn no_output_probe_rows_mark_a_candidate_unavailable_for_seven_days() {
        let none = BTreeMap::new();
        let boot = step_new(
            &Promotions::default(),
            &[OLD],
            &none,
            &Evidence::default(),
            NOW,
        );
        let held = step_new(&boot, &[OLD, BEL], &none, &Evidence::default(), NOW + 1);
        let down = record_probe_outcomes(&held, &[(BEL.to_string(), false)], NOW + 10);
        let candidate = &down.new_families[BEL];
        assert_eq!(candidate.status, NewFamilyStatus::Unavailable);
        assert_eq!(candidate.unavailable_until, Some(NOW + 10 + 7 * DAY));
        assert!(new_family_probation(&down, "anthropic").is_empty());
        assert!(held_ids(&down).contains(BEL));
        let early = step_new(
            &down,
            &[OLD, BEL],
            &none,
            &Evidence::default(),
            NOW + 7 * DAY,
        );
        assert_eq!(early.new_families[BEL].status, NewFamilyStatus::Unavailable);
        let back = step_new(
            &early,
            &[OLD, BEL],
            &none,
            &Evidence::default(),
            NOW + 10 + 7 * DAY,
        );
        assert_eq!(back.new_families[BEL].status, NewFamilyStatus::Probation);
        assert_eq!(back.new_families[BEL].unavailable_until, None);
        let ran = record_probe_outcomes(&down, &[(BEL.to_string(), true)], NOW + 20);
        assert_eq!(ran.new_families[BEL].status, NewFamilyStatus::Probation);
    }

    const MYTHOS5: &str = "claude-mythos-5";
    const MYTHOS6: &str = "claude-mythos-6";

    /// `MYTHOS5` is available; `listed` are models.dev-only unavailable ids.
    fn step_unverified(prev: &Promotions, listed: &[&str], at: u64) -> Promotions {
        let mut discovered = found(&[MYTHOS5]);
        discovered.extend(
            listed
                .iter()
                .map(|id| DiscoveredModel::new("anthropic", *id, false)),
        );
        let only: BTreeSet<String> = listed.iter().map(|id| (*id).to_string()).collect();
        let input = NewFamilyInput {
            released: &BTreeMap::new(),
            ready: true,
            models_dev_only: &only,
        };
        update_with(prev, &discovered, &Evidence::default(), &cfg(), at, &input)
    }

    #[test]
    fn a_newer_models_dev_only_version_is_unverified_and_an_older_one_is_not() {
        let next = step_unverified(&Promotions::default(), &[MYTHOS6, "claude-mythos-4"], NOW);
        assert_eq!(next.unverified.keys().collect::<Vec<_>>(), [MYTHOS6]);
        assert_eq!(unverified_candidates(&next, "anthropic", NOW), [MYTHOS6]);
        assert!(held_ids(&next).contains(MYTHOS6));
        // A family with nothing available has no incumbent, so nothing floods in.
        let input = NewFamilyInput {
            released: &BTreeMap::new(),
            ready: true,
            models_dev_only: &BTreeSet::from([MYTHOS6.to_string()]),
        };
        let none = update_with(
            &Promotions::default(),
            &[DiscoveredModel::new("anthropic", MYTHOS6, false)],
            &Evidence::default(),
            &cfg(),
            NOW,
            &input,
        );
        assert!(none.unverified.is_empty());
    }

    #[test]
    fn a_probed_unverified_version_enters_the_family_gate_and_a_failed_one_backs_off() {
        let listed = step_unverified(&Promotions::default(), &[MYTHOS6], NOW);
        let down = record_probe_outcomes(&listed, &[(MYTHOS6.to_string(), false)], NOW + 5);
        assert_eq!(
            down.unverified[MYTHOS6].unavailable_until,
            Some(NOW + 5 + 7 * DAY)
        );
        assert!(unverified_candidates(&down, "anthropic", NOW + 6).is_empty());
        let later = step_unverified(&down, &[MYTHOS6], NOW + 5 + 7 * DAY);
        assert_eq!(
            unverified_candidates(&later, "anthropic", NOW + 5 + 7 * DAY),
            [MYTHOS6]
        );

        // The probe ran it: the registry marks it available, so the next update sees an
        // ordinary newer version and holds it on probation.
        let next = update(
            &later,
            &found(&[MYTHOS5, MYTHOS6]),
            &Evidence::default(),
            &cfg(),
            NOW + 9 * DAY,
        );
        assert!(next.unverified.is_empty());
        assert_eq!(probation_candidates(&next, "anthropic"), [MYTHOS6]);
    }

    #[test]
    fn an_old_promotions_file_without_new_family_fields_still_loads() {
        let old: Promotions = serde_json::from_str(r#"{"version":1,"updated_at":5,"families":{}}"#)
            .expect("old file");
        assert!(old.new_families.is_empty());
        assert_eq!(old.new_family_baseline, None);
    }
}
