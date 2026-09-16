//! Deterministic workflow-pack selection (issue #542 chunk 3b).
//!
//! `zirv workflow start` with no explicit id, and the native `workflow_list`/
//! slash-command surfaces, all resolve "what should run" through
//! [`select_definition`] -- a single, explainable, pure function of the
//! classification, the objective text, and the registry's currently
//! registered packs. No model call: the issue's own brief asks for a bounded
//! model tie-break to stay deferred, and the deterministic rule below is
//! sufficient while every registered pack still has a small, distinct
//! `domains`/`triggers` vocabulary -- see the design note for the exact
//! reasoning and what would justify revisiting it.

use std::collections::BTreeSet;

use super::classify::{Classification, WorkDomain};
use super::definition::EffectClass;
use super::engine::WorkflowKind;
use super::registry::WorkflowRegistry;

/// A pack clears the floor only with at least one substantive signal (a
/// matched trigger phrase, or a matching `domains` tag) -- not merely an
/// incidental one-point work-domain alignment, which alone would otherwise
/// let every general-purpose pack "match" any undifferentiated task.
const SELECTION_FLOOR: u32 = 2;
const TRIGGER_MATCH_SCORE: u32 = 3;
const DOMAIN_TAG_IN_OBJECTIVE_SCORE: u32 = 2;
const WORK_DOMAIN_ALIGNMENT_SCORE: u32 = 1;

/// The generic fallback pack's own id -- never a real competitor in the
/// scored pool (see [`select_definition`]'s doc comment on why), always the
/// answer when nothing else clears the floor.
pub const ADAPTIVE_WORK_ID: &str = "adaptive-work";

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Selection {
    pub definition_id: String,
    /// A simple, deterministic 0.0-1.0 readout of the winning score --
    /// `1.0` for a legacy-intent direct match, `0.0` for the adaptive-work
    /// fallback, otherwise `min(1.0, score / 10)`. Not a calibrated
    /// probability; a bounded model tie-break (deferred) would be the
    /// right place to produce one of those.
    pub confidence: f64,
    pub reasons: Vec<String>,
    /// Every OTHER pack that cleared the selection floor, most-relevant
    /// first, so a caller (`workflow classify --json`, the native `/workflow`
    /// surfaces) can show what else was considered without re-running
    /// selection itself.
    pub alternatives: Vec<(String, u32)>,
}

/// Issue #542 review finding 16: `objective_lower` split into whole word
/// tokens (any non-alphanumeric byte is a separator), so a domain tag only
/// matches a WHOLE word in the objective text -- plain substring containment
/// let short tags false-positive inside unrelated words ("data" inside
/// "database", "pm" inside "shipment").
fn word_tokens(text: &str) -> BTreeSet<&str> {
    text.split(|c: char| !c.is_alphanumeric())
        .filter(|word| !word.is_empty())
        .collect()
}

fn work_domain_tag(domain: WorkDomain) -> &'static str {
    match domain {
        WorkDomain::Frontend => "frontend",
        WorkDomain::General => "software",
    }
}

/// One candidate pack's deterministic score against `classification` and
/// `objective`, plus the human-readable reasons behind it. `0` (empty
/// reasons) when nothing matched at all.
fn score_pack(
    domains: &[String],
    triggers: &[String],
    classification: &Classification,
    objective_lower: &str,
) -> (u32, Vec<String>) {
    let mut score = 0u32;
    let mut reasons = Vec::new();

    let mut trigger_hits = 0u32;
    for trigger in triggers {
        if !trigger.trim().is_empty() && objective_lower.contains(&trigger.to_lowercase()) {
            trigger_hits += 1;
        }
    }
    if trigger_hits > 0 {
        score += trigger_hits * TRIGGER_MATCH_SCORE;
        reasons.push(format!(
            "{trigger_hits} trigger phrase(s) matched the objective"
        ));
    }

    let objective_words = word_tokens(objective_lower);
    let mut domain_hits = 0u32;
    for domain in domains {
        if !domain.trim().is_empty() && objective_words.contains(domain.as_str()) {
            domain_hits += 1;
        }
    }
    if domain_hits > 0 {
        score += domain_hits * DOMAIN_TAG_IN_OBJECTIVE_SCORE;
        reasons.push(format!(
            "objective text mentions {domain_hits} of the pack's domain tag(s)"
        ));
    }

    let tag = work_domain_tag(classification.work_domain.domain);
    if domains.iter().any(|domain| domain == tag) {
        score += WORK_DOMAIN_ALIGNMENT_SCORE;
        reasons.push(format!("classified work domain aligns with '{tag}'"));
    }

    (score, reasons)
}

/// Selects which registered pack should run for `classification`/
/// `objective` (the raw task text) -- issue #542 chunk 3b.
///
/// 1. A classified software-development intent (`feature`/`bugfix`/
///    `refactor`/`spike`/`review`) selects its own legacy kind pack
///    OUTRIGHT when that id is registered, with no scoring at all -- today's
///    behavior is exactly unchanged. `Intent::Other` is the only intent that
///    reaches step 2 (no existing intent value describes a project-
///    management/data/architecture/devops task, so this is also the only
///    path those new packs are ever chosen through).
/// 2. Every OTHER registered pack (excluding [`ADAPTIVE_WORK_ID`] itself,
///    which never competes) is scored via [`score_pack`]. A pack that
///    clears [`SELECTION_FLOOR`] is eligible.
/// 3. No eligible pack -> `adaptive-work`, confidence `0.0`.
/// 4. A single top-scoring pack wins outright. Two or more tied at the top
///    are broken toward fewer external effects (`EffectClass`'s own `Ord`:
///    `None < Repository < External`), then alphabetically by id -- both
///    tied ids are recorded in `alternatives` and the tie itself is named
///    in `reasons`.
pub fn select_definition(
    classification: &Classification,
    registry: &WorkflowRegistry,
    objective: &str,
) -> Selection {
    if let Some(kind) = WorkflowKind::from_intent(classification.intent)
        && registry.get(kind.as_str()).is_ok()
    {
        return Selection {
            definition_id: kind.as_str().to_string(),
            confidence: 1.0,
            reasons: vec![format!(
                "classified intent {:?} maps directly to the '{}' pack",
                classification.intent,
                kind.as_str()
            )],
            alternatives: Vec::new(),
        };
    }

    let objective_lower = objective.to_lowercase();
    let mut scored: Vec<(String, u32, Vec<String>, EffectClass)> = registry
        .list()
        .filter(|pack| pack.definition.id != ADAPTIVE_WORK_ID)
        .map(|pack| {
            let (score, reasons) = score_pack(
                &pack.definition.domains,
                &pack.definition.triggers,
                classification,
                &objective_lower,
            );
            (
                pack.definition.id.clone(),
                score,
                reasons,
                pack.definition.effects,
            )
        })
        .filter(|(_, score, _, _)| *score >= SELECTION_FLOOR)
        .collect();
    // Deterministic regardless of the registry's own (BTreeMap, so already
    // id-sorted) iteration order: sort explicitly by score desc, then
    // fewer effects, then id, so the winner/tie detection below never
    // depends on incidental ordering.
    scored.sort_by(|a, b| b.1.cmp(&a.1).then(a.3.cmp(&b.3)).then(a.0.cmp(&b.0)));

    let Some((winner_id, winner_score, winner_reasons, _)) = scored.first().cloned() else {
        return Selection {
            definition_id: ADAPTIVE_WORK_ID.to_string(),
            confidence: 0.0,
            reasons: vec!["no registered pack cleared the selection floor".into()],
            alternatives: Vec::new(),
        };
    };

    let tied: Vec<&(String, u32, Vec<String>, EffectClass)> = scored
        .iter()
        .take_while(|(_, score, _, _)| *score == winner_score)
        .collect();

    let mut reasons = winner_reasons;
    if tied.len() > 1 {
        reasons.push(format!(
            "tied with {} other pack(s) at score {winner_score}; broken toward fewer external \
             effects, then alphabetically",
            tied.len() - 1
        ));
    }

    let alternatives = scored
        .iter()
        .filter(|(id, _, _, _)| id != &winner_id)
        .map(|(id, score, _, _)| (id.clone(), *score))
        .collect();

    Selection {
        definition_id: winner_id,
        confidence: (f64::from(winner_score) / 10.0).min(1.0),
        reasons,
        alternatives,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::workflow::classify::{
        Complexity, DomainClassification, Intent, RiskBand, RiskMeasurement,
    };
    use crate::commands::workflow::skill::SkillRegistry;
    use tempfile::tempdir;

    fn classification(intent: Intent) -> Classification {
        Classification {
            intent,
            complexity: Complexity::Bounded,
            risk: RiskBand::Low,
            risk_score: 0,
            changed_files: 1,
            changed_lines: 10,
            changed_paths: Vec::new(),
            declared_scope: true,
            work_domain: DomainClassification::default(),
            risk_measurement: RiskMeasurement::default(),
            reasons: Vec::new(),
        }
    }

    fn registry() -> WorkflowRegistry {
        let repo = tempdir().unwrap();
        let skills = SkillRegistry::load(repo.path(), None, false, false).unwrap();
        WorkflowRegistry::load(repo.path(), None, false, false, &skills).unwrap()
    }

    #[test]
    fn a_legacy_intent_still_selects_its_kind_pack() {
        let registry = registry();
        let selection = select_definition(&classification(Intent::Bugfix), &registry, "fix it");
        assert_eq!(selection.definition_id, "bugfix");
        assert_eq!(selection.confidence, 1.0);
        assert!(selection.alternatives.is_empty());
    }

    #[test]
    fn selection_is_deterministic_and_explains_itself() {
        let registry = registry();
        let classification = classification(Intent::Other);
        let objective = "run a project management backlog triage for the sprint";
        let first = select_definition(&classification, &registry, objective);
        let second = select_definition(&classification, &registry, objective);
        assert_eq!(
            first, second,
            "selection must be a pure function of its inputs"
        );
        assert!(!first.reasons.is_empty(), "a selection must explain itself");
    }

    /// Issue #542 review finding 16: a domain tag must only match a WHOLE
    /// word in the objective text, never a substring inside an unrelated
    /// word. "database" contains "data" and "shipment" contains "pm", but
    /// neither objective actually mentions either pack's domain.
    #[test]
    fn a_domain_tag_only_matches_a_whole_word_not_a_substring() {
        assert_eq!(
            score_pack(
                &["data".into()],
                &[],
                &classification(Intent::Other),
                "investigate the database schema"
            )
            .0,
            0
        );
        assert_eq!(
            score_pack(
                &["pm".into()],
                &[],
                &classification(Intent::Other),
                "track the shipment status"
            )
            .0,
            0
        );
        // The genuine whole-word case still scores.
        assert!(
            score_pack(
                &["data".into()],
                &[],
                &classification(Intent::Other),
                "look at the data quality"
            )
            .0 > 0
        );
        assert!(
            score_pack(
                &["pm".into()],
                &[],
                &classification(Intent::Other),
                "update the pm backlog"
            )
            .0 > 0
        );
    }

    #[test]
    fn an_unmatched_task_falls_back_to_adaptive_work() {
        let registry = registry();
        let selection = select_definition(
            &classification(Intent::Other),
            &registry,
            "xyzzy plugh qux frobnicate",
        );
        assert_eq!(selection.definition_id, ADAPTIVE_WORK_ID);
        assert_eq!(selection.confidence, 0.0);
    }

    fn write_fixture(dir: &std::path::Path, id: &str, effects: &str) {
        std::fs::write(
            dir.join(format!("{id}.toml")),
            format!(
                r#"
schema_version = 1
id = "{id}"
version = 1
title = "{id}"
description = "fixture"
domains = ["testing"]
triggers = ["do the tied thing"]
effects = "{effects}"

[[steps]]
id = "only"
title = "Only"
phase = "implement"
skills = ["implement"]
condition = "always"

[failure]
escalate_to = "human"

[completion]
present_as = "summary"
"#
            ),
        )
        .unwrap();
    }

    #[test]
    fn a_tie_is_broken_toward_fewer_external_effects_and_recorded() {
        let repo = tempdir().unwrap();
        let dir = repo.path().join(".zirv/workflows");
        std::fs::create_dir_all(&dir).unwrap();
        // "a-more-effects" sorts alphabetically FIRST but declares the wider
        // "repository" effect; "z-fewer-effects" sorts alphabetically LAST
        // but declares "none". Both match the same trigger, so they tie on
        // score -- picking "z-fewer-effects" anyway proves the effects
        // ordering is checked before the alphabetical fallback, not that the
        // test coincidentally picked whichever sorts first.
        write_fixture(&dir, "a-more-effects", "repository");
        write_fixture(&dir, "z-fewer-effects", "none");

        let skills = SkillRegistry::load(repo.path(), None, false, false).unwrap();
        let registry = WorkflowRegistry::load(repo.path(), None, true, true, &skills).unwrap();

        let selection = select_definition(
            &classification(Intent::Other),
            &registry,
            "please do the tied thing today",
        );
        assert_eq!(selection.definition_id, "z-fewer-effects");
        assert!(
            selection
                .reasons
                .iter()
                .any(|reason| reason.contains("tied")),
            "{:?}",
            selection.reasons
        );
        assert!(
            selection
                .alternatives
                .iter()
                .any(|(id, _)| id == "a-more-effects"),
            "the losing tied pack must still be recorded as an alternative"
        );
    }

    /// Issue #542 chunk 4's own acceptance test, extended by chunk 5 to the
    /// full catalogue: every non-legacy, non-fallback built-in pack (ten
    /// chunk-4 first-wave professional packs plus sixteen chunk-5 packs,
    /// twenty-six total) must actually be reachable through
    /// `select_definition` via its own declared triggers -- a catalogue
    /// entry nothing ever selects is not useful coverage.
    #[test]
    fn every_builtin_pack_parses_validates_and_selects_on_its_own_triggers() {
        let registry = registry();
        let mut checked = 0usize;
        for pack in registry.list() {
            if pack.definition.id == ADAPTIVE_WORK_ID {
                continue;
            }
            if crate::commands::workflow::engine::WorkflowKind::from_pack_id(&pack.definition.id)
                .is_some()
            {
                // The five legacy kinds win via classified intent, not
                // scoring -- covered by a_legacy_intent_still_selects_its_kind_pack.
                continue;
            }
            let trigger =
                pack.definition.triggers.first().unwrap_or_else(|| {
                    panic!("{} has no triggers to select on", pack.definition.id)
                });
            let selection = select_definition(&classification(Intent::Other), &registry, trigger);
            assert_eq!(
                selection.definition_id, pack.definition.id,
                "objective {trigger:?} should have selected '{}', selected '{}' instead ({:?})",
                pack.definition.id, selection.definition_id, selection.reasons
            );
            checked += 1;
        }
        assert!(
            checked >= 26,
            "expected to check every chunk-4 and chunk-5 professional pack, checked {checked}"
        );
    }

    /// Issue #542 chunk 5: no two DIFFERENT built-in packs may literally
    /// share a trigger phrase. `every_builtin_pack_parses_validates_and_
    /// selects_on_its_own_triggers` already proves each pack's OWN trigger
    /// selects that exact pack (so an accidental substring collision that
    /// changed the winner would already fail there); this test additionally
    /// guards the input itself -- a genuine tie on identical objective text
    /// must only ever be resolved by the documented, deterministic
    /// effects-then-alphabetical tie-break (proven directly by
    /// `a_tie_is_broken_toward_fewer_external_effects_and_recorded`), never
    /// by two packs quietly claiming the same phrase.
    #[test]
    fn no_two_packs_claim_the_same_trigger_ambiguously() {
        let registry = registry();
        let mut owner_by_trigger: std::collections::BTreeMap<String, String> =
            std::collections::BTreeMap::new();
        for pack in registry.list() {
            if pack.definition.id == ADAPTIVE_WORK_ID {
                continue;
            }
            for trigger in &pack.definition.triggers {
                let key = trigger.to_lowercase();
                if let Some(owner) = owner_by_trigger.get(&key) {
                    assert_eq!(
                        owner, &pack.definition.id,
                        "trigger {trigger:?} is claimed by both '{owner}' and '{}' -- an \
                         ambiguous tie must go through the documented tie-break, not accidental \
                         duplication",
                        pack.definition.id
                    );
                } else {
                    owner_by_trigger.insert(key, pack.definition.id.clone());
                }
            }
        }
    }

    /// Issue #542 chunk 4, renamed per review finding 18: this checks the
    /// DEFINITION only (`step.approval`/`step.reason` as authored) -- it
    /// does not drive the engine through the gate. The engine-level proof
    /// that this gate actually blocks a real run lives with each pack's own
    /// end-to-end fixture in `engine.rs` (for these two:
    /// `pm_status_report_end_to_end_walks_to_completion_with_its_artifact`
    /// and `sre_incident_triage_end_to_end_stays_read_only_until_the_
    /// mitigation_gate`, which starts the pack and walks it to completion,
    /// necessarily passing through -- and being blocked by, until approved
    /// -- this same gate). A pack whose real completion would need a
    /// Linear/Kibana/cloud tool (#539, not yet available) must declare that
    /// gap in the relevant step's own `reason` and gate explicitly there,
    /// rather than silently proceeding as if it had live data.
    #[test]
    fn a_pack_needing_a_missing_integration_gates_in_its_own_definition() {
        for (pack_id, step_id) in [
            ("pm-status-report", "source-collection"),
            ("sre-incident-triage", "diagnosis"),
        ] {
            let definition = crate::commands::workflow::registry::builtin_definition(pack_id)
                .unwrap_or_else(|| panic!("{pack_id} pack"));
            let step = definition
                .steps
                .iter()
                .find(|step| step.id == step_id)
                .unwrap_or_else(|| panic!("{pack_id}: step '{step_id}'"));
            assert!(
                step.approval,
                "{pack_id}: '{step_id}' must gate rather than silently proceed without the integration"
            );
            assert!(
                step.reason
                    .as_deref()
                    .unwrap_or("")
                    .to_lowercase()
                    .contains("integration"),
                "{pack_id}: '{step_id}' reason must name the missing integration: {:?}",
                step.reason
            );
        }
    }
}
