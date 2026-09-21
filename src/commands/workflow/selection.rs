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

use super::classify::{Classification, Intent, WorkDomain};
use super::definition::EffectClass;
use super::engine::WorkflowKind;
use super::registry::{WorkflowRegistry, WorkflowSource};

/// A pack clears the floor only with at least one substantive signal (a
/// matched trigger phrase, or a matching `domains` tag) -- not merely an
/// incidental one-point work-domain alignment, which alone would otherwise
/// let every general-purpose pack "match" any undifferentiated task.
const SELECTION_FLOOR: u32 = 2;
const TRIGGER_MATCH_SCORE: u32 = 3;
const DOMAIN_TAG_IN_OBJECTIVE_SCORE: u32 = 2;
const WORK_DOMAIN_ALIGNMENT_SCORE: u32 = 1;
/// The score a legacy intent's direct pack mapping would carry if it were
/// run through [`score_pack`] like everything else -- it isn't (see
/// [`select_definition`]), so a specialised pack that displaces it needs
/// SOME score to record it by in `Selection::alternatives`. `10` is exactly
/// the value that yields the same `confidence: 1.0` the direct mapping
/// itself reports.
const LEGACY_DIRECT_SCORE: u32 = 10;

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

/// Whole-word tokens in **appearance order**, duplicates kept -- shared with
/// `classify.rs`'s `infer_intent`, which needs position (the leading word,
/// windows of a few following tokens) rather than only set membership.
/// `word_tokens` below is this module's own deduplicated, sorted view of the
/// same split rule.
pub(crate) fn word_tokens_ordered(text: &str) -> Vec<&str> {
    text.split(|c: char| !c.is_alphanumeric())
        .filter(|word| !word.is_empty())
        .collect()
}

/// Whether `objective_word` is `trigger_word`, or `trigger_word` with a
/// trailing plural `s`/`es` -- workflow-trigger-determinism: the objective's
/// token aligned with a multi-word trigger's LAST word may be a plural
/// mention of it ("outages" hits the trigger word "outage").
fn objective_word_matches_trigger_word(objective_word: &str, trigger_word: &str) -> bool {
    objective_word == trigger_word
        || matches!(
            objective_word.strip_prefix(trigger_word),
            Some("s") | Some("es")
        )
}

/// Whether `trigger`'s own whole-word token sequence appears CONTIGUOUSLY in
/// `objective_tokens` -- workflow-trigger-determinism: a plain `contains`
/// substring check let a short trigger like "retro" false-positive inside an
/// unrelated word ("Retrofit"). Every token but the sequence's last must
/// match exactly; the last may also match a trailing-plural objective token
/// (see [`objective_word_matches_trigger_word`]).
fn trigger_matches(trigger: &str, objective_tokens: &[&str]) -> bool {
    let trigger_lower = trigger.to_lowercase();
    let trigger_tokens = word_tokens_ordered(&trigger_lower);
    let Some(last) = trigger_tokens.len().checked_sub(1) else {
        return false;
    };
    if trigger_tokens.len() > objective_tokens.len() {
        return false;
    }
    objective_tokens
        .windows(trigger_tokens.len())
        .any(|window| {
            window.iter().zip(trigger_tokens.iter()).enumerate().all(
                |(i, (objective_word, trigger_word))| {
                    if i == last {
                        objective_word_matches_trigger_word(objective_word, trigger_word)
                    } else {
                        objective_word == trigger_word
                    }
                },
            )
        })
}

fn work_domain_tag(domain: WorkDomain) -> &'static str {
    match domain {
        WorkDomain::Frontend => "frontend",
        WorkDomain::General => "software",
    }
}

/// One candidate pack's deterministic score against `classification` and
/// `objective`, the human-readable reasons behind it, and whether at least
/// one of its own trigger phrases actually hit (`0`/empty reasons when
/// nothing matched at all). The trigger-hit flag is a substantive signal
/// distinct from the score itself -- a domain-tag or work-domain point alone
/// can produce a positive score with no trigger hit at all, and
/// [`refine_legacy_selection`] cares specifically about the latter.
fn score_pack(
    domains: &[String],
    triggers: &[String],
    classification: &Classification,
    objective_lower: &str,
) -> (u32, Vec<String>, bool) {
    let mut score = 0u32;
    let mut reasons = Vec::new();
    let objective_tokens = word_tokens_ordered(objective_lower);

    let mut trigger_hits = 0u32;
    for trigger in triggers {
        if !trigger.trim().is_empty() && trigger_matches(trigger, &objective_tokens) {
            trigger_hits += 1;
        }
    }
    if trigger_hits > 0 {
        score += trigger_hits * TRIGGER_MATCH_SCORE;
        reasons.push(format!(
            "{trigger_hits} trigger phrase(s) matched the objective"
        ));
    }

    let objective_words: BTreeSet<&str> = objective_tokens.iter().copied().collect();
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

    (score, reasons, trigger_hits > 0)
}

/// Whether a specialised pack's declared `effects` may replace `intent`'s
/// own legacy pack in [`refine_legacy_selection`]. `Refactor` is handled by
/// that function's own early return and never reaches here; `Other` has no
/// legacy pack to refine and never calls this either.
fn effects_compatible_with_legacy_intent(intent: Intent, effects: EffectClass) -> bool {
    match intent {
        Intent::Feature | Intent::Bugfix => {
            matches!(effects, EffectClass::Repository | EffectClass::External)
        }
        Intent::Review => matches!(effects, EffectClass::None),
        Intent::Spike => true,
        Intent::Refactor | Intent::Other => false,
    }
}

/// A legacy-mapped intent (`Feature`/`Bugfix`/`Spike`/`Review`; `Refactor`
/// never reaches here) still selects its own kind pack by default, but a
/// more specialised registered pack may take over when it BOTH actually
/// matched one of its own trigger phrases in the objective (a domain-tag or
/// work-domain-alignment point alone never qualifies) AND its declared
/// `effects` fits what that intent is allowed to touch -- see
/// [`effects_compatible_with_legacy_intent`]. Scoring and the tie-break
/// mirror `select_definition`'s own step 2/4 exactly, restricted to this
/// smaller, gated candidate pool. `None` when no specialised pack qualifies,
/// so the caller falls back to the plain direct mapping.
///
/// Trust boundary (review finding F1): a repository-provided pack
/// (`WorkflowSource::Repository`) is untrusted and may only ADD a
/// non-colliding id (see `registry.rs`'s own widening refusal) -- it must
/// never REFINE a legacy intent's own built-in pack out from under it, since
/// that would let an untrusted trigger/`effects` pairing silently drop the
/// gates a trusted built-in bugfix/feature pack enforces. Only `BuiltIn` and
/// `OperatorGlobal` packs are eligible here; a repository pack still wins
/// outright for `Intent::Other` via `select_definition`'s ordinary scoring
/// (step 2), which this function is never involved in.
fn refine_legacy_selection(
    classification: &Classification,
    registry: &WorkflowRegistry,
    objective_lower: &str,
    legacy_id: &str,
) -> Option<Selection> {
    if classification.intent == Intent::Refactor {
        return None;
    }

    let mut candidates: Vec<(String, u32, Vec<String>, EffectClass)> = registry
        .list()
        // Neither the fallback pack nor any of the five legacy kind packs
        // (this intent's own included) ever compete here -- only a
        // genuinely SPECIALISED pack may displace a legacy default. A
        // repository-layer pack is untrusted and never competes here either
        // (see this function's own doc comment) -- only `BuiltIn`/
        // `OperatorGlobal` packs are.
        .filter(|pack| {
            pack.definition.id != ADAPTIVE_WORK_ID
                && WorkflowKind::from_pack_id(&pack.definition.id).is_none()
                && pack.source != WorkflowSource::Repository
        })
        .filter_map(|pack| {
            let (score, reasons, trigger_hit) = score_pack(
                &pack.definition.domains,
                &pack.definition.triggers,
                classification,
                objective_lower,
            );
            if !trigger_hit
                || !effects_compatible_with_legacy_intent(
                    classification.intent,
                    pack.definition.effects,
                )
            {
                return None;
            }
            Some((
                pack.definition.id.clone(),
                score,
                reasons,
                pack.definition.effects,
            ))
        })
        .collect();

    if candidates.is_empty() {
        return None;
    }
    candidates.sort_by(|a, b| b.1.cmp(&a.1).then(a.3.cmp(&b.3)).then(a.0.cmp(&b.0)));

    let (winner_id, winner_score, winner_reasons, _) = candidates[0].clone();
    let tied = candidates
        .iter()
        .take_while(|(_, score, _, _)| *score == winner_score)
        .count();

    let mut reasons = winner_reasons;
    reasons.push(format!(
        "specialised pack '{winner_id}' replaces the classified {:?} intent's own '{legacy_id}' \
         pack: its trigger phrase matched the objective",
        classification.intent
    ));
    if tied > 1 {
        reasons.push(format!(
            "tied with {} other specialised pack(s) at score {winner_score}; broken toward fewer \
             external effects, then alphabetically",
            tied - 1
        ));
    }

    let mut alternatives: Vec<(String, u32)> = candidates
        .iter()
        .skip(1)
        .map(|(id, score, _, _)| (id.clone(), *score))
        .collect();
    alternatives.push((legacy_id.to_string(), LEGACY_DIRECT_SCORE));

    Some(Selection {
        definition_id: winner_id,
        confidence: (f64::from(winner_score) / 10.0).min(1.0),
        reasons,
        alternatives,
    })
}

/// Selects which registered pack should run for `classification`/
/// `objective` (the raw task text) -- issue #542 chunk 3b, refined for
/// workflow-trigger-determinism.
///
/// 1. A classified software-development intent (`feature`/`bugfix`/
///    `refactor`/`spike`/`review`) selects its own legacy kind pack by
///    DEFAULT when that id is registered. A more SPECIALISED pack may
///    replace it (see [`refine_legacy_selection`]) when it actually matched
///    one of its own trigger phrases in the objective -- a domain-tag or
///    work-domain-alignment point alone never qualifies -- and its declared
///    `effects` fits the intent: `Feature`/`Bugfix` need `Repository` or
///    `External`, `Review` needs `None`, `Spike` accepts any effects, and
///    `Refactor` is never displaced. When no specialised pack qualifies,
///    behavior is exactly the old direct mapping (confidence `1.0`).
///    `Intent::Other` has no legacy kind counterpart, so it always reaches
///    step 2 below (no existing intent value describes a project-
///    management/data/architecture/devops task, so this is also the only
///    path those new packs are ever chosen through when their objective
///    doesn't also match one of the five kinds above).
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
    let objective_lower = objective.to_lowercase();

    if let Some(kind) = WorkflowKind::from_intent(classification.intent)
        && registry.get(kind.as_str()).is_ok()
    {
        let legacy_id = kind.as_str().to_string();
        if let Some(refined) =
            refine_legacy_selection(classification, registry, &objective_lower, &legacy_id)
        {
            return refined;
        }
        return Selection {
            definition_id: legacy_id,
            confidence: 1.0,
            reasons: vec![format!(
                "classified intent {:?} maps directly to the '{}' pack",
                classification.intent,
                kind.as_str()
            )],
            alternatives: Vec::new(),
        };
    }

    let mut scored: Vec<(String, u32, Vec<String>, EffectClass)> = registry
        .list()
        .filter(|pack| pack.definition.id != ADAPTIVE_WORK_ID)
        .map(|pack| {
            let (score, reasons, _trigger_hit) = score_pack(
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

    /// Workflow-trigger-determinism: a trigger phrase matches on WHOLE-WORD
    /// token sequences, never `contains` -- "retro" (a `pm-retrospective`
    /// trigger) must not fire inside "Retrofit". With no pack qualifying,
    /// this `Other`-intent objective falls all the way back to
    /// `adaptive-work`, exactly as the acceptance matrix expects.
    #[test]
    fn a_whole_word_trigger_never_matches_inside_an_unrelated_word() {
        let registry = registry();
        let selection = select_definition(
            &classification(Intent::Other),
            &registry,
            "Retrofit the old importer docs",
        );
        assert_eq!(
            selection.definition_id, ADAPTIVE_WORK_ID,
            "{:?}",
            selection.reasons
        );
    }

    /// Workflow-trigger-determinism: the objective's token aligned with a
    /// trigger's LAST word may carry a trailing plural `s`/`es` -- "outages"
    /// still hits the single-word trigger "outage".
    #[test]
    fn a_trigger_s_last_word_matches_a_trailing_plural_in_the_objective() {
        assert!(trigger_matches(
            "outage",
            &["the", "outages", "piled", "up"]
        ));
        assert!(!trigger_matches("outage", &["outaged"]));
    }

    /// Effect-compatibility arm: `Bugfix` needs `Repository`/`External`,
    /// which `security-remediation` (effects = repository) satisfies.
    #[test]
    fn bugfix_with_a_security_trigger_selects_security_remediation() {
        let registry = registry();
        let selection = select_definition(
            &classification(Intent::Bugfix),
            &registry,
            "Fix the security vulnerability in the auth module",
        );
        assert_eq!(
            selection.definition_id, "security-remediation",
            "{:?}",
            selection.reasons
        );
        assert!(
            selection.alternatives.iter().any(|(id, _)| id == "bugfix"),
            "the displaced legacy pack must still be recorded: {:?}",
            selection.alternatives
        );
    }

    /// Effect-compatibility arm: `Feature` also needs `Repository`/
    /// `External`, which `pm-status-report` (effects = none) does NOT
    /// satisfy -- its trigger still matches, but the legacy `feature` pack
    /// stands.
    #[test]
    fn feature_with_a_status_report_trigger_stays_feature() {
        let registry = registry();
        let selection = select_definition(
            &classification(Intent::Feature),
            &registry,
            "Add a status report page to the dashboard",
        );
        assert_eq!(
            selection.definition_id, "feature",
            "{:?}",
            selection.reasons
        );
        assert_eq!(selection.confidence, 1.0);
    }

    /// Effect-compatibility arm: `Review` needs `None`, which
    /// `architecture-design-review` (effects = none) satisfies.
    #[test]
    fn review_with_a_design_review_trigger_selects_architecture_design_review() {
        let registry = registry();
        let selection = select_definition(
            &classification(Intent::Review),
            &registry,
            "Review the design of the new queue",
        );
        assert_eq!(
            selection.definition_id, "architecture-design-review",
            "{:?}",
            selection.reasons
        );
    }

    /// `Refactor` is never displaced, even though `devops-ci-cd-change`'s
    /// own "build pipeline" trigger matches and its effects (repository)
    /// would otherwise qualify under the Feature/Bugfix rule.
    #[test]
    fn refactor_is_never_displaced_by_a_specialised_pack() {
        let registry = registry();
        let selection = select_definition(
            &classification(Intent::Refactor),
            &registry,
            "Refactor the build pipeline scripts",
        );
        assert_eq!(
            selection.definition_id, "refactor",
            "{:?}",
            selection.reasons
        );
        assert_eq!(selection.confidence, 1.0);
        assert!(selection.alternatives.is_empty());
    }

    /// Effect-compatibility arm: `Spike` accepts ANY effects, so
    /// `sre-incident-triage` (effects = none) still qualifies.
    #[test]
    fn spike_with_an_outage_trigger_selects_sre_incident_triage() {
        let registry = registry();
        let selection = select_definition(
            &classification(Intent::Spike),
            &registry,
            "Investigate the outage in checkout",
        );
        assert_eq!(
            selection.definition_id, "sre-incident-triage",
            "{:?}",
            selection.reasons
        );
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

    fn write_fixture_with_trigger(dir: &std::path::Path, id: &str, effects: &str, trigger: &str) {
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
triggers = ["{trigger}"]
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

    /// Review finding F1 (trust boundary): a repository-provided pack is
    /// untrusted and may only ADD a non-colliding id -- it must never
    /// refine a legacy intent's own built-in pack out from under it, even
    /// with a broad trigger ("fix") and a compatible `effects`. The
    /// IDENTICAL pack at a trusted layer (operator-global) DOES refine,
    /// proving the gate is about provenance, not the pack's own content.
    #[test]
    fn a_repository_layer_pack_never_refines_a_legacy_intent_but_a_trusted_layer_pack_does() {
        let skills_repo = tempdir().unwrap();
        let skills = SkillRegistry::load(skills_repo.path(), None, false, false).unwrap();

        let untrusted_repo = tempdir().unwrap();
        let untrusted_dir = untrusted_repo.path().join(".zirv/workflows");
        std::fs::create_dir_all(&untrusted_dir).unwrap();
        write_fixture_with_trigger(&untrusted_dir, "repo-bugfix-like", "repository", "fix");
        let untrusted_registry =
            WorkflowRegistry::load(untrusted_repo.path(), None, true, true, &skills).unwrap();
        let untrusted_selection = select_definition(
            &classification(Intent::Bugfix),
            &untrusted_registry,
            "fix the crash",
        );
        assert_eq!(
            untrusted_selection.definition_id, "bugfix",
            "an untrusted repository pack must never refine a legacy intent: {:?}",
            untrusted_selection.reasons
        );

        let home = tempdir().unwrap();
        let home_dir = home.path().join(".zirv/workflows");
        std::fs::create_dir_all(&home_dir).unwrap();
        write_fixture_with_trigger(&home_dir, "repo-bugfix-like", "repository", "fix");
        let trusted_repo = tempdir().unwrap();
        let trusted_registry =
            WorkflowRegistry::load(trusted_repo.path(), Some(home.path()), true, false, &skills)
                .unwrap();
        let trusted_selection = select_definition(
            &classification(Intent::Bugfix),
            &trusted_registry,
            "fix the crash",
        );
        assert_eq!(
            trusted_selection.definition_id, "repo-bugfix-like",
            "an operator-global (trusted) pack must still refine: {:?}",
            trusted_selection.reasons
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
