//! Which harnesses are present, which models each could run, and who judges.

use serde::{Deserialize, Serialize};

use super::Filters;
use crate::commands::ctx::adapters::{self, AgentAdapter, Liveness};
use crate::commands::ctx::catalogue::{self, Tier};
use crate::commands::ctx::config::CtxConfig;

/// Model name for a harness whose vendor ladder is unknown: no model flag is pinned.
pub const DEFAULT_MODEL: &str = "default";

const ALL_TIERS: [Tier; 3] = [Tier::Cheap, Tier::Standard, Tier::Deep];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Presence {
    Live,
    Absent,
    Disabled,
}

#[derive(Debug, Clone, Serialize)]
pub struct HarnessInfo {
    pub name: String,
    pub presence: Presence,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Candidate {
    pub harness: String,
    pub model: String,
    pub tier: Option<Tier>,
    #[serde(skip)]
    strength: Option<u8>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Judge {
    pub harness: String,
    pub model: String,
    pub also_candidate: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct Discovery {
    pub harnesses: Vec<HarnessInfo>,
    pub candidates: Vec<Candidate>,
    pub judge: Option<Judge>,
}

pub fn discover(
    cfg: &CtxConfig,
    filters: &Filters,
    present: &dyn Fn(&str, &str) -> Liveness,
) -> Result<Discovery, String> {
    for name in filters
        .harnesses
        .iter()
        .chain(filters.models.iter().map(|(harness, _)| harness))
        .chain(filters.judge.iter().map(|(harness, _)| harness))
    {
        if !adapters::ADAPTERS.iter().any(|(known, _)| known == name) {
            return Err(format!("unknown harness '{name}'"));
        }
    }

    let mut harnesses = Vec::new();
    let mut candidates = Vec::new();
    for (name, _) in adapters::ADAPTERS {
        let (presence, adapter) = if !cfg.agents.is_enabled(name) {
            (Presence::Disabled, None)
        } else {
            match adapters::adapter_liveness_with(cfg, name, None, present) {
                Ok((adapter, Liveness::Live | Liveness::Unknown(_))) => {
                    (Presence::Live, Some(adapter))
                }
                _ => (Presence::Absent, None),
            }
        };
        harnesses.push(HarnessInfo {
            name: (*name).to_string(),
            presence,
        });
        let wanted = filters.harnesses.is_empty() || filters.harnesses.iter().any(|h| h == name);
        if let (Some(adapter), true) = (adapter, wanted) {
            candidates.extend(candidates_for(adapter.as_ref(), filters));
        }
    }

    let judge = match &filters.judge {
        _ if filters.no_judge => None,
        Some((harness, model)) => {
            let live = harnesses
                .iter()
                .any(|info| info.name == *harness && info.presence == Presence::Live);
            if !live {
                return Err(format!(
                    "--judge {harness}:{model}: harness is not available"
                ));
            }
            Some(judge_for(harness, model, &candidates))
        }
        None => strongest(&candidates, &cfg.fallback.order)
            .map(|best| judge_for(&best.harness, &best.model, &candidates)),
    };
    Ok(Discovery {
        harnesses,
        candidates,
        judge,
    })
}

fn judge_for(harness: &str, model: &str, candidates: &[Candidate]) -> Judge {
    Judge {
        harness: harness.to_string(),
        model: model.to_string(),
        also_candidate: candidates
            .iter()
            .any(|c| c.harness == harness && c.model == model),
    }
}

fn candidates_for(adapter: &dyn AgentAdapter, filters: &Filters) -> Vec<Candidate> {
    let name = adapter.name();
    let vendor = catalogue::vendor(adapter.provider());
    let candidate = |model: &str, tier: Option<Tier>| Candidate {
        harness: name.to_string(),
        model: model.to_string(),
        tier,
        strength: vendor.and_then(|v| catalogue::strength(v, model)),
    };

    if !filters.models.is_empty() {
        return filters
            .models
            .iter()
            .filter(|(harness, _)| harness == name)
            .map(|(_, model)| candidate(model, None))
            .collect();
    }

    let tiers = if filters.tiers.is_empty() {
        ALL_TIERS.as_slice()
    } else {
        filters.tiers.as_slice()
    };
    let Some(vendor) = vendor else {
        return vec![candidate(DEFAULT_MODEL, None)];
    };
    let mut out: Vec<Candidate> = Vec::new();
    for tier in tiers {
        let Some(model) = catalogue::tier_model(vendor, *tier) else {
            continue;
        };
        if out.iter().any(|existing| existing.model == model) {
            continue;
        }
        out.push(candidate(model, Some(*tier)));
    }
    // A harness that ignores model flags would benchmark one model under several names.
    if out
        .first()
        .is_some_and(|first| adapter.model_args(&first.model).is_empty())
    {
        return vec![candidate(DEFAULT_MODEL, None)];
    }
    out
}

/// Highest ladder strength; a tie goes to the harness earliest in `[fallback] order`.
fn strongest<'a>(candidates: &'a [Candidate], order: &[String]) -> Option<&'a Candidate> {
    let rank = |harness: &str| {
        order
            .iter()
            .position(|name| name == harness)
            .unwrap_or(usize::MAX)
    };
    let mut best: Option<&Candidate> = None;
    for candidate in candidates.iter().filter(|c| c.strength.is_some()) {
        let better = best.is_none_or(|current| {
            (
                candidate.strength,
                std::cmp::Reverse(rank(&candidate.harness)),
            ) > (current.strength, std::cmp::Reverse(rank(&current.harness)))
        });
        if better {
            best = Some(candidate);
        }
    }
    best
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn cfg(env: &[(&str, &str)]) -> CtxConfig {
        let tmp = tempfile::tempdir().unwrap();
        let env: HashMap<String, String> = env
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect();
        CtxConfig::load(tmp.path(), &|k| env.get(k).cloned()).expect("config loads")
    }

    fn filters() -> Filters {
        Filters::default()
    }

    fn names(discovery: &Discovery) -> Vec<&str> {
        let mut out: Vec<&str> = discovery
            .candidates
            .iter()
            .map(|c| c.harness.as_str())
            .collect();
        out.dedup();
        out
    }

    #[test]
    fn an_absent_harness_is_excluded() {
        let present = adapters::only_installed(&["claude"]);
        let d = discover(&cfg(&[]), &filters(), &present).unwrap();
        assert_eq!(names(&d), vec!["claude"]);
        let codex = d.harnesses.iter().find(|h| h.name == "codex").unwrap();
        assert_eq!(codex.presence, Presence::Absent);
    }

    #[test]
    fn a_disabled_harness_is_excluded() {
        let present = adapters::only_installed(&["claude", "codex"]);
        let config = cfg(&[("ZIRV_AGENT_CODEX_ENABLED", "false")]);
        let d = discover(&config, &filters(), &present).unwrap();
        assert_eq!(names(&d), vec!["claude"]);
        let codex = d.harnesses.iter().find(|h| h.name == "codex").unwrap();
        assert_eq!(codex.presence, Presence::Disabled);
    }

    #[test]
    fn tier_candidates_come_from_the_catalogue_without_duplicates() {
        let present = adapters::only_installed(&["claude"]);
        let d = discover(&cfg(&[]), &filters(), &present).unwrap();
        let vendor = catalogue::vendor("anthropic").unwrap();
        let expected: Vec<&str> = ALL_TIERS
            .iter()
            .filter_map(|t| catalogue::tier_model(vendor, *t))
            .collect();
        let got: Vec<&str> = d.candidates.iter().map(|c| c.model.as_str()).collect();
        assert_eq!(got, expected);
    }

    #[test]
    fn a_tier_filter_narrows_the_candidates() {
        let present = adapters::only_installed(&["claude"]);
        let f = Filters {
            tiers: vec![Tier::Cheap],
            ..filters()
        };
        let d = discover(&cfg(&[]), &f, &present).unwrap();
        assert_eq!(d.candidates.len(), 1);
        assert_eq!(d.candidates[0].tier, Some(Tier::Cheap));
    }

    #[test]
    fn an_unresolvable_vendor_gets_a_single_default_candidate() {
        let present = adapters::only_installed(&["goose"]);
        let d = discover(&cfg(&[]), &filters(), &present).unwrap();
        assert_eq!(d.candidates.len(), 1);
        assert_eq!(d.candidates[0].model, DEFAULT_MODEL);
        assert_eq!(d.candidates[0].tier, None);
    }

    #[test]
    fn explicit_models_replace_the_tier_candidates() {
        let present = adapters::only_installed(&["claude", "codex"]);
        let f = Filters {
            models: vec![("claude".to_string(), "sonnet".to_string())],
            ..filters()
        };
        let d = discover(&cfg(&[]), &f, &present).unwrap();
        assert_eq!(d.candidates.len(), 1);
        assert_eq!(
            (
                d.candidates[0].harness.as_str(),
                d.candidates[0].model.as_str()
            ),
            ("claude", "sonnet")
        );
    }

    #[test]
    fn the_harness_filter_restricts_candidates_but_not_the_listing() {
        let present = adapters::only_installed(&["claude", "codex"]);
        let f = Filters {
            harnesses: vec!["codex".to_string()],
            ..filters()
        };
        let d = discover(&cfg(&[]), &f, &present).unwrap();
        assert_eq!(names(&d), vec!["codex"]);
        assert_eq!(d.harnesses.len(), adapters::ADAPTERS.len());
    }

    #[test]
    fn unknown_harness_names_are_rejected() {
        let present = adapters::only_installed(&["claude"]);
        let f = Filters {
            harnesses: vec!["nope".to_string()],
            ..filters()
        };
        assert!(discover(&cfg(&[]), &f, &present).is_err());
    }

    #[test]
    fn the_judge_is_the_strongest_present_candidate() {
        let present = adapters::only_installed(&["claude"]);
        let d = discover(&cfg(&[]), &filters(), &present).unwrap();
        let strongest = d
            .candidates
            .iter()
            .max_by_key(|c| c.strength)
            .expect("a candidate");
        let judge = d.judge.expect("a judge");
        assert_eq!(judge.model, strongest.model);
        assert!(judge.also_candidate);
    }

    #[test]
    fn a_strength_tie_goes_to_the_first_harness_in_fallback_order() {
        let tied = |harness: &str| Candidate {
            harness: harness.to_string(),
            model: "m".to_string(),
            tier: None,
            strength: Some(5),
        };
        let candidates = vec![tied("claude"), tied("codex")];
        let order = vec!["codex".to_string(), "claude".to_string()];
        assert_eq!(strongest(&candidates, &order).unwrap().harness, "codex");
        let order = vec!["claude".to_string()];
        assert_eq!(strongest(&candidates, &order).unwrap().harness, "claude");
    }

    #[test]
    fn candidates_without_a_known_strength_never_judge() {
        let present = adapters::only_installed(&["goose"]);
        let d = discover(&cfg(&[]), &filters(), &present).unwrap();
        assert!(d.judge.is_none());
    }

    #[test]
    fn no_judge_disables_the_judge_and_an_explicit_judge_overrides_it() {
        let present = adapters::only_installed(&["claude", "codex"]);
        let f = Filters {
            no_judge: true,
            ..filters()
        };
        assert!(discover(&cfg(&[]), &f, &present).unwrap().judge.is_none());
        let f = Filters {
            judge: Some(("codex".to_string(), "x-model".to_string())),
            ..filters()
        };
        let judge = discover(&cfg(&[]), &f, &present).unwrap().judge.unwrap();
        assert_eq!(
            (judge.harness.as_str(), judge.model.as_str()),
            ("codex", "x-model")
        );
        assert!(!judge.also_candidate);
    }

    #[test]
    fn an_explicit_judge_on_an_absent_harness_is_rejected() {
        let present = adapters::only_installed(&["claude"]);
        let f = Filters {
            judge: Some(("codex".to_string(), "m".to_string())),
            ..filters()
        };
        assert!(discover(&cfg(&[]), &f, &present).is_err());
    }
}
