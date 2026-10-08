//! Which harnesses are present, which models each could run, and who judges.
//!
//! Candidate models are the ones the runtime registry offers on this machine
//! (available, listed, not retired), never a built-in table.

use serde::{Deserialize, Serialize};

use super::Filters;
use crate::commands::ctx::adapters::{self, AgentAdapter, Liveness};
use crate::commands::ctx::config::CtxConfig;
use crate::commands::ctx::models::{self, Listing};

/// Model name for a harness with no available registry rows: no model flag is pinned.
pub const DEFAULT_MODEL: &str = "default";

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
    pub family: Option<String>,
    /// Output price per million tokens; the judge default's proxy for capability.
    #[serde(skip)]
    output_price: Option<u64>,
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
    listing: &Listing,
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
            candidates.extend(candidates_for(adapter.as_ref(), filters, listing));
        }
    }

    for (harness, model) in &filters.models {
        let excluded = !filters.harnesses.is_empty() && !filters.harnesses.contains(harness);
        let state = match harnesses.iter().find(|info| info.name == *harness) {
            _ if excluded => "excluded by --harness",
            Some(info) if info.presence == Presence::Live => continue,
            Some(info) if info.presence == Presence::Disabled => "disabled",
            _ => "not installed",
        };
        return Err(format!(
            "--model {harness}:{model}: harness '{harness}' is {state}"
        ));
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
        None => highest_priced(&candidates, &cfg.fallback.order)
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

fn candidates_for(
    adapter: &dyn AgentAdapter,
    filters: &Filters,
    listing: &Listing,
) -> Vec<Candidate> {
    let rows = &listing.rows;
    let name = adapter.name();
    let vendor = adapter.provider();
    let candidate = |model: &str, family: Option<&str>, output_price: Option<u64>| Candidate {
        harness: name.to_string(),
        model: model.to_string(),
        family: family.map(str::to_string),
        output_price,
    };

    if !filters.models.is_empty() {
        return filters
            .models
            .iter()
            .filter(|(harness, _)| harness == name)
            .map(|(_, model)| {
                let row = rows.iter().find(|r| r.vendor == vendor && r.id == *model);
                candidate(
                    model,
                    row.and_then(|r| r.family.as_deref()),
                    row.and_then(|r| r.output_micros_per_million),
                )
            })
            .collect();
    }

    let available = models::candidates(&listing.registry, vendor, listing.now);
    if available.is_empty() {
        // Without a family to match, a filter leaves nothing for a default launch to satisfy.
        return if filters.families.is_empty() {
            vec![candidate(DEFAULT_MODEL, None, None)]
        } else {
            Vec::new()
        };
    }

    available
        .into_iter()
        .filter_map(|model| {
            let row = rows.iter().find(|r| r.vendor == vendor && r.id == model.id);
            let family = row.and_then(|r| r.family.as_deref());
            let wanted = filters.families.is_empty()
                || family.is_some_and(|f| filters.families.iter().any(|w| w == f));
            wanted.then(|| {
                candidate(
                    &model.id,
                    family,
                    row.and_then(|r| r.output_micros_per_million),
                )
            })
        })
        .collect()
}

/// Highest output price; a tie goes to the harness earliest in `[fallback] order`, then by name.
fn highest_priced<'a>(candidates: &'a [Candidate], order: &[String]) -> Option<&'a Candidate> {
    let rank = |harness: &str| {
        order
            .iter()
            .position(|name| name == harness)
            .unwrap_or(usize::MAX)
    };
    candidates
        .iter()
        .filter(|c| c.output_price.is_some())
        .max_by(|a, b| {
            a.output_price
                .cmp(&b.output_price)
                .then_with(|| rank(&b.harness).cmp(&rank(&a.harness)))
                .then_with(|| b.harness.cmp(&a.harness))
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::ctx::models::{AVAILABLE, ModelRow, Registry, RegistryModel};
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

    fn row(vendor: &str, id: &str, family: Option<&str>, price: Option<u64>) -> ModelRow {
        ModelRow::for_test(vendor, id, family, AVAILABLE, price)
    }

    /// A listing whose registry mirrors the rows: `available` rows are available models.
    fn listing(rows: &[ModelRow]) -> Listing {
        let models = rows
            .iter()
            .map(|r| {
                let model = RegistryModel {
                    vendor: r.vendor.clone(),
                    id: r.id.clone(),
                    available: r.availability == AVAILABLE,
                    ..RegistryModel::default()
                };
                (format!("{}:{}", r.vendor, r.id), model)
            })
            .collect();
        Listing {
            registry: Registry {
                updated_at: 0,
                models,
            },
            rows: rows.to_vec(),
            now: 100,
        }
    }

    fn models(candidates: &[Candidate]) -> Vec<&str> {
        candidates.iter().map(|c| c.model.as_str()).collect()
    }

    /// Resolves the vendor slug the claude adapter runs, so tests name no vendor.
    fn claude_vendor() -> &'static str {
        adapters::ADAPTERS
            .iter()
            .find(|(n, _)| *n == "claude")
            .map(|(_, ctor)| ctor(None).provider())
            .unwrap()
    }

    fn only_claude(rows: &[ModelRow], f: &Filters) -> Discovery {
        let present = adapters::only_installed(&["claude"]);
        discover(&cfg(&[]), f, &present, &listing(rows)).unwrap()
    }

    #[test]
    fn every_available_id_is_a_candidate_with_its_family() {
        let v = claude_vendor();
        let rows = [
            row(v, "vendor-alpha-1", Some("alpha"), None),
            row(v, "vendor-alpha-2-1", Some("alpha"), None),
            row(v, "vendor-beta-10", Some("beta"), None),
            row(v, "loner-1", None, None),
        ];
        let d = only_claude(&rows, &filters());
        assert_eq!(
            models(&d.candidates),
            vec![
                "loner-1",
                "vendor-alpha-1",
                "vendor-alpha-2-1",
                "vendor-beta-10"
            ]
        );
        assert_eq!(d.candidates[2].family.as_deref(), Some("alpha"));
    }

    #[test]
    fn a_retired_registry_id_is_not_planned() {
        let v = claude_vendor();
        let rows = [
            row(v, "vendor-alpha-1", Some("alpha"), None),
            row(v, "vendor-alpha-2", Some("alpha"), None),
        ];
        let mut with = listing(&rows);
        with.registry
            .models
            .get_mut(&format!("{v}:vendor-alpha-2"))
            .unwrap()
            .retirement_at = Some(with.now);
        let present = adapters::only_installed(&["claude"]);
        let d = discover(&cfg(&[]), &filters(), &present, &with).unwrap();
        assert_eq!(models(&d.candidates), vec!["vendor-alpha-1"]);
    }

    #[test]
    fn unavailable_hidden_and_snapshot_rows_are_excluded() {
        let v = claude_vendor();
        let rows = [
            row(v, "vendor-alpha-1", Some("alpha"), None),
            ModelRow::for_test(v, "vendor-alpha-9", Some("alpha"), "hidden", None),
            ModelRow::for_test(v, "vendor-alpha-8", Some("alpha"), "snapshot", None),
        ];
        let d = only_claude(&rows, &filters());
        assert_eq!(models(&d.candidates), vec!["vendor-alpha-1"]);
    }

    #[test]
    fn placeholders_are_not_models() {
        let v = claude_vendor();
        let rows = [
            row(v, "<placeholder>", None, None),
            row(v, "vendor-alpha-1", Some("alpha"), None),
        ];
        let d = only_claude(&rows, &filters());
        assert_eq!(models(&d.candidates), vec!["vendor-alpha-1"]);
    }

    #[test]
    fn rows_without_a_family_each_stand_alone() {
        let v = claude_vendor();
        let rows = [row(v, "loner-1", None, None), row(v, "loner-2", None, None)];
        let d = only_claude(&rows, &filters());
        assert_eq!(models(&d.candidates), vec!["loner-1", "loner-2"]);
    }

    #[test]
    fn rows_of_another_vendor_are_not_candidates() {
        let rows = [row(
            "some-other-vendor",
            "vendor-alpha-1",
            Some("alpha"),
            None,
        )];
        let d = only_claude(&rows, &filters());
        assert_eq!(models(&d.candidates), vec![DEFAULT_MODEL]);
    }

    #[test]
    fn a_harness_with_no_available_rows_gets_one_default_candidate() {
        let d = only_claude(&[], &filters());
        assert_eq!(d.candidates.len(), 1);
        assert_eq!(d.candidates[0].model, DEFAULT_MODEL);
        assert_eq!(d.candidates[0].family, None);
    }

    #[test]
    fn the_family_filter_keeps_only_those_families() {
        let v = claude_vendor();
        let rows = [
            row(v, "vendor-alpha-1", Some("alpha"), None),
            row(v, "vendor-beta-1", Some("beta"), None),
            row(v, "loner-1", None, None),
        ];
        let f = Filters {
            families: vec!["beta".to_string()],
            ..filters()
        };
        assert_eq!(
            models(&only_claude(&rows, &f).candidates),
            vec!["vendor-beta-1"]
        );
        assert!(only_claude(&[], &f).candidates.is_empty());
    }

    #[test]
    fn a_model_pin_replaces_the_registry_candidates_with_exact_ids() {
        let v = claude_vendor();
        let rows = [row(v, "vendor-alpha-3", Some("alpha"), Some(7))];
        let present = adapters::only_installed(&["claude", "codex"]);
        let f = Filters {
            models: vec![
                ("claude".to_string(), "vendor-alpha-1".to_string()),
                ("claude".to_string(), "vendor-alpha-3".to_string()),
            ],
            ..filters()
        };
        let d = discover(&cfg(&[]), &f, &present, &listing(&rows)).unwrap();
        assert_eq!(
            models(&d.candidates),
            vec!["vendor-alpha-1", "vendor-alpha-3"]
        );
        assert_eq!(d.candidates[0].family, None);
        assert_eq!(d.candidates[1].family.as_deref(), Some("alpha"));
    }

    #[test]
    fn a_model_pin_on_an_unavailable_harness_is_an_error_naming_its_state() {
        let present = adapters::only_installed(&["claude", "codex"]);
        let pin = |harness: &str, only: &[&str]| Filters {
            models: vec![(harness.to_string(), "vendor-alpha-1".to_string())],
            harnesses: only.iter().map(|h| (*h).to_string()).collect(),
            ..filters()
        };
        let err =
            discover(&cfg(&[]), &pin("goose", &[]), &present, &Listing::default()).unwrap_err();
        assert!(err.contains("harness 'goose' is not installed"), "{err}");
        let config = cfg(&[("ZIRV_AGENT_CODEX_ENABLED", "false")]);
        let err = discover(&config, &pin("codex", &[]), &present, &Listing::default()).unwrap_err();
        assert!(err.contains("harness 'codex' is disabled"), "{err}");
        let err = discover(
            &cfg(&[]),
            &pin("codex", &["claude"]),
            &present,
            &Listing::default(),
        )
        .unwrap_err();
        assert!(err.contains("excluded by --harness"), "{err}");
    }

    #[test]
    fn pins_are_exact_and_ignore_the_family_filter() {
        let v = claude_vendor();
        let rows = [row(v, "vendor-alpha-1", Some("alpha"), None)];
        let present = adapters::only_installed(&["claude"]);
        let f = Filters {
            models: vec![("claude".to_string(), "vendor-alpha-1".to_string())],
            families: vec!["beta".to_string()],
            ..filters()
        };
        let d = discover(&cfg(&[]), &f, &present, &listing(&rows)).unwrap();
        assert_eq!(models(&d.candidates), vec!["vendor-alpha-1"]);
    }

    #[test]
    fn an_absent_harness_is_excluded_and_a_disabled_one_is_marked() {
        let present = adapters::only_installed(&["claude", "codex"]);
        let config = cfg(&[("ZIRV_AGENT_CODEX_ENABLED", "false")]);
        let d = discover(&config, &filters(), &present, &Listing::default()).unwrap();
        let harnesses: Vec<&str> = d.candidates.iter().map(|c| c.harness.as_str()).collect();
        assert_eq!(harnesses, vec!["claude"]);
        let presence = |name: &str| {
            d.harnesses
                .iter()
                .find(|h| h.name == name)
                .unwrap()
                .presence
        };
        assert_eq!(presence("codex"), Presence::Disabled);
        assert_eq!(presence("copilot"), Presence::Absent);
    }

    #[test]
    fn the_harness_filter_restricts_candidates_but_not_the_listing() {
        let present = adapters::only_installed(&["claude", "codex"]);
        let f = Filters {
            harnesses: vec!["codex".to_string()],
            ..filters()
        };
        let d = discover(&cfg(&[]), &f, &present, &Listing::default()).unwrap();
        assert!(d.candidates.iter().all(|c| c.harness == "codex"));
        assert_eq!(d.harnesses.len(), adapters::ADAPTERS.len());
    }

    #[test]
    fn unknown_harness_names_are_rejected() {
        let present = adapters::only_installed(&["claude"]);
        let f = Filters {
            harnesses: vec!["nope".to_string()],
            ..filters()
        };
        assert!(discover(&cfg(&[]), &f, &present, &Listing::default()).is_err());
    }

    fn priced(harness: &str, model: &str, price: Option<u64>) -> Candidate {
        Candidate {
            harness: harness.to_string(),
            model: model.to_string(),
            family: None,
            output_price: price,
        }
    }

    #[test]
    fn the_judge_is_the_candidate_with_the_highest_output_price() {
        let v = claude_vendor();
        let rows = [
            row(v, "vendor-alpha-1", Some("alpha"), Some(10)),
            row(v, "vendor-beta-1", Some("beta"), Some(50)),
            row(v, "vendor-gamma-1", Some("gamma"), None),
        ];
        let d = only_claude(&rows, &filters());
        let judge = d.judge.expect("a judge");
        assert_eq!(judge.model, "vendor-beta-1");
        assert!(judge.also_candidate);
    }

    #[test]
    fn a_price_tie_goes_to_fallback_order_then_harness_name() {
        let candidates = vec![
            priced("claude", "m", Some(5)),
            priced("codex", "m", Some(5)),
        ];
        let order = vec!["codex".to_string(), "claude".to_string()];
        assert_eq!(
            highest_priced(&candidates, &order).unwrap().harness,
            "codex"
        );
        let order = vec!["claude".to_string()];
        assert_eq!(
            highest_priced(&candidates, &order).unwrap().harness,
            "claude"
        );
        assert_eq!(highest_priced(&candidates, &[]).unwrap().harness, "claude");
    }

    #[test]
    fn candidates_without_a_known_price_never_judge() {
        let d = only_claude(&[], &filters());
        assert!(d.judge.is_none());
        assert_eq!(highest_priced(&[priced("claude", "m", None)], &[]), None);
    }

    #[test]
    fn no_judge_disables_the_judge_and_an_explicit_judge_overrides_it() {
        let present = adapters::only_installed(&["claude", "codex"]);
        let f = Filters {
            no_judge: true,
            ..filters()
        };
        assert!(
            discover(&cfg(&[]), &f, &present, &Listing::default())
                .unwrap()
                .judge
                .is_none()
        );
        let f = Filters {
            judge: Some(("codex".to_string(), "x-model".to_string())),
            ..filters()
        };
        let judge = discover(&cfg(&[]), &f, &present, &Listing::default())
            .unwrap()
            .judge
            .unwrap();
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
        assert!(discover(&cfg(&[]), &f, &present, &Listing::default()).is_err());
    }
}
