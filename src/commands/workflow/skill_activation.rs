//! Issue #539's deterministic, model-independent skill activation.
//!
//! The catalogue a repository/operator/built-in layer resolves is only
//! useful if something decides which of its skills apply to the task at
//! hand -- and the issue is explicit that this decision must not depend on
//! a particular host or model: the same task and phase must always score
//! the same way, in this binary, with no hook into whichever model happens
//! to be driving the session. [`score_skills`] is that pure function.
//!
//! Modelled closely on `selection::score_pack` (workflow-pack selection,
//! issue #542 chunk 3b): same shape of floor-then-sort scoring, same
//! deterministic tie-break. `selection::word_tokens` is a private helper of
//! that module (workflow-pack triggers are matched by plain substring
//! containment, not tokenized), so it is not reusable here; this module
//! mirrors its tokenization approach instead, because skill triggers *are*
//! meant to match whole words/phrases only -- "cat" must not activate a
//! skill whose trigger is "category".
//!
//! Issue #539 chunk F: the operator's own design decision on that chunk is
//! that zirv may make a skill's MATCH deterministic but never the choice to
//! use one -- so [`score_skills`] backs only the agent's own search
//! (`skill_tools::skill_list`'s `query`, `zirv skill list --match`), never a
//! standing prompt layer that pre-selects a skill for a task. A session's
//! own standing skill index (`ctx::prompt::skill_index_text`) is a plain,
//! unscored listing of every implicit-activation skill instead -- see that
//! function's own doc comment.

use std::collections::BTreeSet;

use super::skill::{RegisteredSkill, SkillRegistry, WorkflowPhase};

/// A trigger phrase match is the strongest, most deliberate signal a task
/// can give -- an operator or skill author chose this exact wording, as
/// opposed to a bare phase match ([`PHASE_MATCH_SCORE`] alone, which many
/// skills in the same phase would share regardless of what the task says).
const TRIGGER_MATCH_SCORE: u32 = 3;
/// The active workflow phase alone is a weaker signal than an explicit
/// trigger match: many skills declare a phase, but only some of those are
/// actually relevant to what the task says.
const PHASE_MATCH_SCORE: u32 = 2;
/// A skill needs at least one substantive signal to activate -- exactly one
/// phase match alone still clears this (a phase-scoped skill activating for
/// every task in that phase is intended), but a skill with no trigger and
/// no phase declared at all, or matching neither, contributes nothing.
const ACTIVATION_FLOOR: u32 = 2;
/// Each word beyond the first makes a trigger more specific: "terraform
/// plan" says far more about a task than "plan" does, and without this a
/// generic one-word trigger ties with the phrase that actually describes the
/// work and the tie falls to alphabetical order.
const PHRASE_WORD_BONUS: u32 = 1;

/// A phrase matches only as a contiguous run of whole words, so "log search"
/// does not fire inside "blog searching".
fn phrase_matches(task_lower: &str, phrase: &str) -> bool {
    task_lower.match_indices(phrase).any(|(start, matched)| {
        let boundary = |c: Option<char>| c.is_none_or(|c| !c.is_alphanumeric());
        boundary(task_lower[..start].chars().next_back())
            && boundary(task_lower[start + matched.len()..].chars().next())
    })
}

/// Mirrors `selection::word_tokens`: any non-alphanumeric byte is a
/// separator, so a single-word trigger only matches a WHOLE word in the
/// task text, never a substring inside a longer word.
fn word_tokens(text: &str) -> BTreeSet<&str> {
    text.split(|c: char| !c.is_alphanumeric())
        .filter(|word| !word.is_empty())
        .collect()
}

/// One skill's deterministic score against `task`/`phase`, and why it
/// scored that way -- issue #539's requirement that an activation is
/// reviewable, not just a number.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillMatch<'a> {
    pub skill: &'a RegisteredSkill,
    pub score: u32,
    pub reasons: Vec<String>,
    /// How many of the skill's own triggers the task hit. Equal scores are
    /// ordered by the share of a skill's triggers that matched, so a skill
    /// the task fully describes outranks one it merely brushes against.
    pub matched_triggers: usize,
}

/// Deterministic, model-independent skill activation. Scores every skill
/// the registry resolved against the task text and the active phase and
/// returns the best matches above [`ACTIVATION_FLOOR`], highest score
/// first, ties broken by matched-trigger share then id -- never by map
/// iteration order, and never by
/// anything a model decided.
///
/// Issue #539 chunk F: the agent's own search, not a standing prompt layer
/// -- wired into `skill_tools::skill_list`'s `query` (and `zirv skill list
/// --match`), both initiated by the agent or operator, never by zirv
/// pre-selecting a skill for a task on its own.
pub fn score_skills<'a>(
    registry: &'a SkillRegistry,
    task: &str,
    phase: Option<WorkflowPhase>,
    limit: usize,
) -> Vec<SkillMatch<'a>> {
    let task_lower = task.to_lowercase();
    let task_words = word_tokens(&task_lower);

    let mut scored: Vec<SkillMatch<'a>> = registry
        .list()
        // Issue #539: `implicit_activation == false` means explicit
        // invocation only -- this scorer exists for automatic activation,
        // so such a skill must never appear here no matter how well its
        // triggers match.
        .filter(|skill| skill.manifest.implicit_activation)
        .filter_map(|skill| {
            let mut score = 0u32;
            let mut reasons = Vec::new();
            let mut matched_triggers = 0usize;

            for trigger in &skill.manifest.triggers {
                let trigger_lower = trigger.trim().to_lowercase();
                if trigger_lower.is_empty() {
                    continue;
                }
                let extra_words = trigger_lower.split_whitespace().count().saturating_sub(1);
                let matched = if extra_words > 0 {
                    // A multi-word trigger is a phrase: matched as a
                    // contiguous whole-word run rather than tokenized
                    // membership, which would accept the same words in any
                    // order or position.
                    phrase_matches(&task_lower, &trigger_lower)
                } else {
                    task_words.contains(trigger_lower.as_str())
                };
                if matched {
                    score += TRIGGER_MATCH_SCORE + PHRASE_WORD_BONUS * extra_words as u32;
                    matched_triggers += 1;
                    reasons.push(format!("trigger '{trigger}' matched the task"));
                }
            }

            if let Some(phase) = phase
                && skill.manifest.phases.contains(&phase)
            {
                score += PHASE_MATCH_SCORE;
                reasons.push(format!("declares the active phase '{phase}'"));
            }

            (score >= ACTIVATION_FLOOR).then_some(SkillMatch {
                skill,
                score,
                reasons,
                matched_triggers,
            })
        })
        .collect();

    scored.sort_by(|a, b| {
        // Match share compared by cross-multiplication: no floats, so the
        // order is exact and identical on every platform.
        let total = |m: &SkillMatch<'_>| m.skill.manifest.triggers.len().max(1);
        b.score
            .cmp(&a.score)
            .then_with(|| (b.matched_triggers * total(a)).cmp(&(a.matched_triggers * total(b))))
            .then_with(|| a.skill.manifest.id.cmp(&b.skill.manifest.id))
    });
    scored.truncate(limit);
    scored
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn write_skill(repo: &std::path::Path, text: &str) {
        let dir = repo.join(".zirv/skills");
        std::fs::create_dir_all(&dir).expect("mkdir");
        // Filename is irrelevant to the registry; one file per test keeps
        // fixtures independent.
        std::fs::write(dir.join(format!("{}.yaml", uuid_like())), text)
            .expect("write fixture skill");
    }

    // A tiny, dependency-free unique-enough suffix so repeated calls in one
    // test don't collide on the same filename.
    fn uuid_like() -> String {
        use std::sync::atomic::{AtomicU32, Ordering};
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        format!("fixture-{}", COUNTER.fetch_add(1, Ordering::Relaxed))
    }

    fn registry_with(repo: &std::path::Path) -> SkillRegistry {
        SkillRegistry::load(repo, None, true, true).expect("registry loads")
    }

    #[test]
    fn a_trigger_match_activates() {
        let repo = tempdir().unwrap();
        write_skill(
            repo.path(),
            "schema_version: 1\nid: incident-probe\nversion: 1\nname: Incident probe\ndescription: test\ntriggers: [\"production outage\"]\ncontext_budget_bytes: 64\nphases: [debug]\ninstructions: investigate\n",
        );
        let registry = registry_with(repo.path());
        let matches = score_skills(
            &registry,
            "there is a production outage right now",
            None,
            10,
        );
        assert!(
            matches
                .iter()
                .any(|m| m.skill.manifest.id == "incident-probe")
        );
        let hit = matches
            .iter()
            .find(|m| m.skill.manifest.id == "incident-probe")
            .unwrap();
        assert_eq!(hit.score, TRIGGER_MATCH_SCORE + PHRASE_WORD_BONUS);
        assert!(hit.reasons.iter().any(|r| r.contains("production outage")));
    }

    #[test]
    fn a_phase_only_match_scores_below_a_trigger_match() {
        let repo = tempdir().unwrap();
        write_skill(
            repo.path(),
            "schema_version: 1\nid: phase-only\nversion: 1\nname: Phase only\ndescription: test\ncontext_budget_bytes: 64\nphases: [debug]\ninstructions: investigate\n",
        );
        write_skill(
            repo.path(),
            "schema_version: 1\nid: trigger-hit\nversion: 1\nname: Trigger hit\ndescription: test\ntriggers: [\"widget frobnication\"]\ncontext_budget_bytes: 64\nphases: [debug]\ninstructions: investigate\n",
        );
        let registry = registry_with(repo.path());
        let matches = score_skills(
            &registry,
            "please handle the widget frobnication",
            Some(WorkflowPhase::Debug),
            10,
        );
        let phase_only = matches
            .iter()
            .find(|m| m.skill.manifest.id == "phase-only")
            .expect("phase-only skill activates on the phase alone");
        let trigger_hit = matches
            .iter()
            .find(|m| m.skill.manifest.id == "trigger-hit")
            .expect("trigger-hit skill activates");
        assert!(phase_only.score < trigger_hit.score);
        assert_eq!(phase_only.score, PHASE_MATCH_SCORE);
    }

    #[test]
    fn implicit_activation_false_never_activates_even_on_an_exact_trigger() {
        let repo = tempdir().unwrap();
        write_skill(
            repo.path(),
            "schema_version: 1\nid: explicit-only\nversion: 1\nname: Explicit only\ndescription: test\ntriggers: [\"summon me exactly\"]\nimplicit_activation: false\ncontext_budget_bytes: 64\nphases: [debug]\ninstructions: investigate\n",
        );
        let registry = registry_with(repo.path());
        let matches = score_skills(&registry, "summon me exactly please", None, 10);
        assert!(
            !matches
                .iter()
                .any(|m| m.skill.manifest.id == "explicit-only")
        );
    }

    #[test]
    fn ties_break_by_id() {
        let repo = tempdir().unwrap();
        for id in ["zeta-skill", "alpha-skill"] {
            write_skill(
                repo.path(),
                &format!(
                    "schema_version: 1\nid: {id}\nversion: 1\nname: {id}\ndescription: test\ntriggers: [\"shared trigger phrase\"]\ncontext_budget_bytes: 64\nphases: [debug]\ninstructions: investigate\n"
                ),
            );
        }
        let registry = registry_with(repo.path());
        let matches = score_skills(
            &registry,
            "the shared trigger phrase appears here",
            None,
            10,
        );
        let ids: Vec<&str> = matches
            .iter()
            .filter(|m| m.skill.manifest.id.ends_with("-skill"))
            .map(|m| m.skill.manifest.id.as_str())
            .collect();
        assert_eq!(ids, vec!["alpha-skill", "zeta-skill"]);
    }

    #[test]
    fn the_limit_is_honoured() {
        let repo = tempdir().unwrap();
        for id in ["one-skill", "two-skill", "three-skill"] {
            write_skill(
                repo.path(),
                &format!(
                    "schema_version: 1\nid: {id}\nversion: 1\nname: {id}\ndescription: test\ntriggers: [\"cap trigger phrase\"]\ncontext_budget_bytes: 64\nphases: [debug]\ninstructions: investigate\n"
                ),
            );
        }
        let registry = registry_with(repo.path());
        let matches = score_skills(&registry, "the cap trigger phrase appears here", None, 2);
        assert_eq!(matches.len(), 2);
    }

    #[test]
    fn an_empty_task_activates_nothing() {
        let repo = tempdir().unwrap();
        let registry = registry_with(repo.path());
        let matches = score_skills(&registry, "", None, 10);
        assert!(matches.is_empty());
    }

    #[test]
    fn a_phrase_outranks_a_generic_one_word_trigger() {
        let repo = tempdir().unwrap();
        let registry = SkillRegistry::load(repo.path(), None, false, false).expect("registry");
        let matches = score_skills(
            &registry,
            "Look over this terraform plan before I apply it.",
            None,
            3,
        );
        assert_eq!(matches[0].skill.manifest.id, "infrastructure-review");
        assert_eq!(matches[0].score, TRIGGER_MATCH_SCORE + PHRASE_WORD_BONUS);
    }

    #[test]
    fn equal_scores_order_by_the_share_of_a_skills_triggers_that_matched() {
        let repo = tempdir().unwrap();
        let registry = SkillRegistry::load(repo.path(), None, false, false).expect("registry");
        // `verify` and `frontend-verify` both match "complete" and "verify",
        // but those are all of `verify`'s triggers and half of the other's.
        let matches = score_skills(
            &registry,
            "Confirm the work is complete and verify it.",
            Some(WorkflowPhase::Verify),
            3,
        );
        assert_eq!(matches[0].skill.manifest.id, "verify");
        assert_eq!(matches[0].score, matches[1].score);
    }

    #[test]
    fn a_phrase_only_matches_on_whole_word_boundaries() {
        assert!(phrase_matches("run a log search now", "log search"));
        assert!(phrase_matches("log search", "log search"));
        assert!(!phrase_matches("my blog searching habit", "log search"));
    }

    /// The fixture is the catalogue's activation contract: one natural
    /// request per built-in skill, which must rank that skill first, plus
    /// requests nothing should claim. A new skill earns its place by adding
    /// a row here that passes without displacing another.
    #[test]
    fn every_fixture_task_ranks_its_expected_skill_first() {
        let repo = tempdir().unwrap();
        let registry = SkillRegistry::load(repo.path(), None, false, false).expect("registry");
        let fixture = include_str!("../../../tests/fixtures/skill-activation/tasks.tsv");
        let mut covered = BTreeSet::new();
        for line in fixture
            .lines()
            .filter(|l| !l.starts_with('#') && !l.trim().is_empty())
        {
            let mut columns = line.splitn(3, '\t');
            let (expected, phase, task) = (
                columns.next().expect("skill column"),
                columns.next().expect("phase column"),
                columns.next().expect("task column"),
            );
            let phase = (phase != "-").then(|| WorkflowPhase::parse(phase).expect("known phase"));
            let matches = score_skills(&registry, task, phase, 3);
            if expected == "-" {
                assert!(
                    matches.is_empty(),
                    "nothing should claim {task:?}: {matches:?}"
                );
                continue;
            }
            let ranked: Vec<&str> = matches
                .iter()
                .map(|m| m.skill.manifest.id.as_str())
                .collect();
            assert_eq!(
                ranked.first(),
                Some(&expected),
                "{task:?} ranked {ranked:?}"
            );
            covered.insert(expected.to_string());
        }
        let all: BTreeSet<String> = registry.list().map(|s| s.manifest.id.clone()).collect();
        assert_eq!(covered, all, "every built-in skill needs a fixture row");
    }
}
