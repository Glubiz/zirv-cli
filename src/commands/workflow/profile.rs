//! The minimal execution profile (issue #541, the #537 seam).
//!
//! #537 owns the full proportional execution profile (intent/domain/
//! complexity/risk/validation, tuned against real outcome evidence). This
//! module is deliberately the smallest slice #541's team compiler actually
//! needs today, kept in one file so #537 can replace it wholesale without
//! touching every caller: a deterministic, pure `derive` over the existing
//! [`Classification`] plus the request text, with no model call and no I/O.
//!
//! [`ExecutionProfile::derive`] must stay a pure function of its two inputs:
//! identical classification and request text always produce an identical
//! profile, the same guarantee [`super::classify::classify`] itself gives.

use std::collections::BTreeSet;
use std::path::Path;

use serde::{Deserialize, Serialize};

use super::agents::ModelTier;
use super::classify::{
    Classification, Complexity, FRONTEND_TASK_TERMS, Intent, RiskBand, RiskMeasurement, WorkDomain,
};
use crate::commands::ctx::config::CtxConfig;
use crate::commands::ctx::jev::{self, Answers, Question};
use crate::commands::ctx::proxy;
use crate::commands::ctx::proxy::decision::{DOMAIN_NOUL_QUESTIONS, DOMAIN_QUESTION_IDS};
use crate::commands::ctx::state::StateDir;

/// How much coordination a request needs, derived from complexity alone.
/// Trivial work stays on the calling seat; bounded work gets one or two
/// seats; substantial/architectural work gets a compiled team.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ExecutionMode {
    Direct,
    Bounded,
    Orchestrated,
}

/// Independent validation a plan must include. Each flag names a GATE, not a
/// seat -- the team compiler ([`super::team::compile`]) decides which
/// concrete manifest satisfies it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ValidationProfile {
    pub independent_review: bool,
    pub independent_test: bool,
    pub security_review: bool,
}

/// A domain signal beyond the base `general`/`frontend` split
/// [`classify::WorkDomain`] already carries. Additive rather than a
/// replacement: [`WorkDomain::Frontend`] still selects `frontend`, and a
/// request may carry several tags at once (a security fix to a deployment
/// script is both `security` and `dev-ops`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum WorkDomainTag {
    General,
    Frontend,
    Security,
    Data,
    Docs,
    DevOps,
    Architecture,
}

/// How much the classification underneath this profile can be trusted.
/// Mirrors [`RiskMeasurement`] and [`Classification::declared_scope`] rather
/// than inventing a new signal: an unmeasured risk band (outside a
/// repository, or one with no commits) can only ever produce `Low`
/// confidence, and an operator-declared (not Git-measured) scope caps at
/// `Medium`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Confidence {
    High,
    Medium,
    Low,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExecutionProfile {
    pub classification: Classification,
    pub execution: ExecutionMode,
    /// Routing hint for the team as a whole -- reuses [`agents::ModelTier`]
    /// rather than a parallel enum, since it means the same thing: a seat's
    /// tier is never an authorization grant, only a cost/capability hint.
    pub model_tier: ModelTier,
    pub validation: ValidationProfile,
    pub domains: Vec<WorkDomainTag>,
    pub confidence: Confidence,
    pub reasons: Vec<String>,
}

fn matches_any(text: &str, needles: &[&str]) -> bool {
    needles.iter().any(|needle| text.contains(needle))
}

/// The five keyword-domain signals [`ExecutionProfile::derive`] scans
/// `request_text` for, beyond the base `general`/`frontend` split
/// [`classify::WorkDomain`] already carries -- promoted out of `derive`
/// itself so the workflow module's own metadata-only Jev classify
/// refinement (issue #782, [`refine_via_jev`]) can count the same keyword
/// hits instead of carrying a second copy of these lists.
pub(crate) const DOMAIN_SIGNALS: [(&[&str], WorkDomainTag, &str); 5] = [
    (
        &["security", "auth", "permission", "credential", "secret"],
        WorkDomainTag::Security,
        "request names a security surface (security)",
    ),
    (
        &[
            "data",
            "dataset",
            "query",
            "sql",
            "analytics",
            "pipeline data",
        ],
        WorkDomainTag::Data,
        "request names a data surface (data)",
    ),
    (
        &["document", "docs", "readme", "changelog"],
        WorkDomainTag::Docs,
        "request names a documentation surface (docs)",
    ),
    (
        &[
            "deploy",
            "ci/cd",
            " ci ",
            "pipeline",
            "infrastructure",
            "devops",
            "docker",
            "terraform",
            "kubernetes",
            "incident",
        ],
        WorkDomainTag::DevOps,
        "request names a deployment/operations surface (dev-ops)",
    ),
    (
        &[
            "architecture",
            "migration",
            "redesign",
            "system design",
            "trade-off",
            "adr",
        ],
        WorkDomainTag::Architecture,
        "request names an architectural surface (architecture)",
    ),
];

impl ExecutionProfile {
    /// Pure and deterministic: identical `request_text`/`classification`
    /// inputs always produce an identical profile. No model call, no I/O --
    /// see the module doc for why this matters.
    pub fn derive(request_text: &str, classification: &Classification) -> Self {
        let text = request_text.to_ascii_lowercase();
        let mut reasons = Vec::new();

        let execution = match classification.complexity {
            Complexity::Trivial => ExecutionMode::Direct,
            Complexity::Bounded => ExecutionMode::Bounded,
            Complexity::Substantial | Complexity::Architectural => ExecutionMode::Orchestrated,
        };
        reasons.push(format!(
            "complexity {:?} selects {execution:?} execution",
            classification.complexity
        ));

        let model_tier = match execution {
            ExecutionMode::Direct => ModelTier::Fast,
            ExecutionMode::Bounded => ModelTier::Standard,
            ExecutionMode::Orchestrated => ModelTier::Deep,
        };

        let mut domains = BTreeSet::new();
        if classification.work_domain.domain == WorkDomain::Frontend {
            domains.insert(WorkDomainTag::Frontend);
            reasons.push("diff touches a frontend surface (frontend)".to_string());
        }
        for (needles, tag, reason) in DOMAIN_SIGNALS {
            if matches_any(&text, needles) {
                domains.insert(tag);
                reasons.push(reason.to_string());
            }
        }
        if domains.is_empty() {
            domains.insert(WorkDomainTag::General);
        }
        let security_domain = domains.contains(&WorkDomainTag::Security);

        let mut validation = ValidationProfile::default();
        if classification.risk >= RiskBand::High || security_domain {
            validation.independent_review = true;
            validation.security_review = true;
            reasons.push(
                "risk >= High or a security surface requires independent and security review"
                    .to_string(),
            );
        }
        if classification.complexity >= Complexity::Substantial
            || classification.risk >= RiskBand::Medium
        {
            validation.independent_test = true;
            reasons.push(
                "complexity >= Substantial or risk >= Medium requires independent test coverage"
                    .to_string(),
            );
        }

        let confidence = match &classification.risk_measurement {
            RiskMeasurement::Unavailable { .. } => Confidence::Low,
            RiskMeasurement::Measured if classification.declared_scope => Confidence::Medium,
            RiskMeasurement::Measured => Confidence::High,
        };
        reasons.sort();
        reasons.dedup();

        Self {
            classification: classification.clone(),
            execution,
            model_tier,
            validation,
            domains: domains.into_iter().collect(),
            confidence,
            reasons,
        }
    }
}

/// Issue #782 site string every `jev-decisions.jsonl`/`jev-effects.jsonl`
/// row and `cfg.jev.classify` gate share.
const JEV_CLASSIFY_SITE: &str = "classify";

/// Minimum additive-domain-tag probability for the classify site, the same
/// floor `workflow::engine::JEV_TAG_PROBABILITY` uses for the gate
/// reclassification site, from the same 2026-09-18 probe -- kept as its own
/// constant rather than importing `engine` here, since `profile` is the
/// lower-level module `engine` itself depends on.
const JEV_CLASSIFY_TAG_PROBABILITY: f64 = 0.7;

/// Maps one of [`DOMAIN_QUESTION_IDS`]'s six ids onto this module's own
/// [`WorkDomainTag`]. `None` is unreachable in practice ([`DOMAIN_QUESTION_IDS`]
/// is the single source of truth both this and `proxy::decision::merge`
/// read by the same ids) but kept total rather than panicking on drift.
fn domain_tag_for(id: &str) -> Option<WorkDomainTag> {
    match id {
        "security" => Some(WorkDomainTag::Security),
        "data" => Some(WorkDomainTag::Data),
        "docs_only" => Some(WorkDomainTag::Docs),
        "devops" => Some(WorkDomainTag::DevOps),
        "architecture" => Some(WorkDomainTag::Architecture),
        "frontend" => Some(WorkDomainTag::Frontend),
        _ => None,
    }
}

/// Local, bounded facts about `task`/`classification` -- the only signal
/// [`refine_via_jev`] sends Jev. Must satisfy `jev::safe_metadata_request`
/// (checked directly by this module's own tests). Reuses [`DOMAIN_SIGNALS`]
/// (this module's own five keyword-domain lists) and
/// [`super::classify::FRONTEND_TASK_TERMS`] (the deterministic classifier's
/// own frontend text signal) for the per-domain hit counts, and
/// `proxy::mod`'s own path-like-token/outcome/constraint predicates and
/// word-count bucket -- the exact same local text signals the harness
/// proxy's own metadata-only intake (`proxy::safe_intake_metadata`) already
/// computes, reused rather than re-derived.
fn classify_jev_facts(task: &str, classification: &Classification) -> serde_json::Value {
    let lower = task.to_ascii_lowercase();
    let path_like_tokens = task
        .split_whitespace()
        .filter(|word| proxy::is_path_like_token(word))
        .count() as u64;
    let mut domain_hits: Vec<u64> = DOMAIN_SIGNALS
        .iter()
        .map(|(needles, _, _)| {
            needles
                .iter()
                .filter(|needle| lower.contains(*needle))
                .count() as u64
        })
        .collect();
    domain_hits.push(
        FRONTEND_TASK_TERMS
            .iter()
            .filter(|needle| lower.contains(*needle))
            .count() as u64,
    );
    serde_json::json!({
        "_zirv_metadata_only": true,
        // [site=3, intent, complexity, risk, word-count bucket, path-like
        // token count, stated-outcome flag, stated-constraint flag,
        // per-domain keyword hit counts (security, data, docs, devops,
        // architecture, frontend)]. No request text, path, or repository
        // name is sent.
        "facts": [[
            3,
            classification.intent as u64,
            classification.complexity as u64,
            classification.risk as u64,
            proxy::word_count_bucket(task.split_whitespace().count()),
            path_like_tokens,
            proxy::text_has_outcome_terms(&lower) as u64,
            proxy::text_has_constraint_terms(&lower) as u64,
            domain_hits[0],
            domain_hits[1],
            domain_hits[2],
            domain_hits[3],
            domain_hits[4],
            domain_hits[5],
        ]],
    })
}

/// The intent Choice alone (the same six options `proxy::decision::
/// questions` asks, restated as static metadata-signed criteria) -- factored
/// out of [`classify_jev_questions`] so a caller with no [`ExecutionProfile`]
/// surface to add domain tags to (`workflow::engine::start_workflow`, via
/// [`refine_intent_via_jev`]) can ask only this one question instead of
/// carrying a second copy of the six options.
fn classify_intent_question() -> Question {
    Question::metadata_choice(
        "intent",
        "From facts [site=3, intent (0 feature, 1 bugfix, 2 refactor, 3 spike, 4 review, 5 \
         other), complexity (0 trivial to 3 architectural), risk (0 low to 3 critical), \
         word-count bucket (0 to 4), path-like token count, stated-outcome/stated-constraint \
         flags, per-domain keyword hit counts (security, data, docs, devops, architecture, \
         frontend)], what kind of work is this request? Pick the single best match.",
        &[
            (
                "feature",
                "Adds new capability or behavior that did not exist before.",
            ),
            (
                "bugfix",
                "Fixes a defect or regression -- something that should work but does not.",
            ),
            (
                "refactor",
                "Restructures existing code without changing its observable behavior.",
            ),
            (
                "spike",
                "Explores, prototypes, or researches an approach before committing to it.",
            ),
            (
                "review",
                "Reviews or audits existing work rather than changing it outright.",
            ),
            ("other", "Anything that does not fit the other five."),
        ],
    )
}

/// [`classify_intent_question`] plus one Noul per domain tag, reusing
/// [`DOMAIN_QUESTION_IDS`]/[`DOMAIN_NOUL_QUESTIONS`] (the harness proxy's own
/// six domain questions) rather than a second copy. No complexity question:
/// the 2026-09-18 replay found complexity answers stable-but-wrong at margin
/// 0.18-0.24, routing 1.7-2.2x more spend through Substantial with no
/// accuracy gain (see `jev::DEFAULT_MIN_MARGIN`'s own doc comment).
fn classify_jev_questions() -> Vec<Question> {
    let mut out = vec![classify_intent_question()];
    for (id, (what, when_true, when_false)) in
        DOMAIN_QUESTION_IDS.into_iter().zip(DOMAIN_NOUL_QUESTIONS)
    {
        out.push(Question::metadata_noul(id, what, when_true, when_false));
    }
    out
}

/// Applies a decisive `intent` answer onto `classification` in place: `intent`
/// is replaced outright only when decisive, the same rule
/// `proxy::decision::merge` applies to its own intent Choice. Shared by
/// [`apply_classify_answers`] (the `profile`-based site) and
/// [`refine_intent_via_jev`] (the `Classification`-only site), so the two
/// can never drift on what "decisive" means for this field.
fn apply_intent_answer(cfg: &CtxConfig, answers: &Answers, classification: &mut Classification) {
    if let Some(answer) = answers.get("intent")
        && answer.decisive(cfg.proxy.min_confidence, cfg.proxy.min_margin)
        && let Some(choice) = answer.as_choice()
        && let Ok(intent) =
            serde_json::from_value::<Intent>(serde_json::Value::String(choice.to_string()))
        && intent != classification.intent
    {
        classification.reasons.push(format!(
            "jev: intent refined from {:?} to {intent:?}",
            classification.intent
        ));
        classification.intent = intent;
        classification.reasons.sort();
    }
}

/// Applies a decisive `answers` set onto `profile` in place, monotonic the
/// same way `proxy::decision::merge` is: `intent` is replaced outright only
/// when decisive ([`apply_intent_answer`]); a domain tag is only ever ADDED
/// (never removed), and only when its Noul clears both the margin gate and
/// [`JEV_CLASSIFY_TAG_PROBABILITY`]; `risk` is never touched at all. A
/// `security` tag added this way raises validation the same way the
/// keyword-driven path already does.
fn apply_classify_answers(cfg: &CtxConfig, answers: &Answers, profile: &mut ExecutionProfile) {
    apply_intent_answer(cfg, answers, &mut profile.classification);

    let mut added_security = false;
    for id in DOMAIN_QUESTION_IDS {
        let Some(tag) = domain_tag_for(id) else {
            continue;
        };
        let Some(answer) = answers.get(id) else {
            continue;
        };
        if !answer.decisive(0.0, jev::DEFAULT_MIN_MARGIN) {
            continue;
        }
        let Some(probability) = answer.as_noul() else {
            continue;
        };
        if probability < JEV_CLASSIFY_TAG_PROBABILITY {
            continue;
        }
        if profile.domains.contains(&tag) {
            continue;
        }
        profile.domains.push(tag);
        profile.reasons.push(format!(
            "jev: added domain tag {tag:?} (p={probability:.2})"
        ));
        if tag == WorkDomainTag::Security {
            added_security = true;
        }
    }
    if added_security {
        profile.validation.independent_review = true;
        profile.validation.security_review = true;
    }
    profile.domains.sort();
    profile.domains.dedup();
    profile.reasons.sort();
    profile.reasons.dedup();
}

/// Issue #782: an off-by-default Jev refinement of an already-computed
/// `profile`, for `zirv workflow classify` -- gated on `cfg.jev.classify`
/// (`[jev] classify`/`ZIRV_CTX_JEV_CLASSIFY`). Key off, or no
/// `[proxy.typesafe]` credential, is a silent no-op: `profile` is left
/// exactly as [`ExecutionProfile::derive`] computed it, so the caller's own
/// output stays byte-identical to today (issue #782 acceptance). Sends only
/// the same bounded numeric metadata envelope every other `[jev]`-gated site
/// sends ([`classify_jev_facts`]), never the request text itself, and never
/// fails or blocks the caller: any Jev error, timeout, or low-margin answer
/// simply leaves `profile` untouched, recorded as a fallback by the shared
/// [`jev::advise`] client. For `zirv workflow start`, which has no
/// `ExecutionProfile` surface to add a domain tag to, see
/// [`refine_intent_via_jev`] instead.
pub(crate) fn refine_via_jev(repo: &Path, task: &str, profile: &mut ExecutionProfile) {
    let env = |key: &str| std::env::var(key).ok();
    let Ok(cfg) = CtxConfig::load(repo, &env) else {
        return;
    };
    if !cfg.jev.classify || !jev::available(&cfg.proxy.typesafe) {
        return;
    }
    let Ok(state) = StateDir::resolve(&env) else {
        return;
    };
    refine_profile_with_jev(&cfg, &state, task, profile);
}

/// [`refine_via_jev`]'s testable core, taking an already-loaded `cfg`/
/// `state` directly rather than loading them from a repository -- the same
/// split `workflow::team::advise_compiled_plan`/`maybe_advise_team_plan`
/// already uses for the same reason (its own tests build a `CtxConfig`
/// directly instead of a real `.zirv/ctx.toml`).
fn refine_profile_with_jev(
    cfg: &CtxConfig,
    state: &StateDir,
    task: &str,
    profile: &mut ExecutionProfile,
) {
    let facts = classify_jev_facts(task, &profile.classification);
    let questions = classify_jev_questions();
    let Some(answers) = jev::advise(
        cfg,
        state,
        JEV_CLASSIFY_SITE,
        cfg.jev.classify,
        &facts,
        &questions,
    ) else {
        return;
    };

    apply_classify_answers(cfg, &answers, profile);
}

/// Issue #782: the same off-by-default Jev intent refinement as
/// [`refine_via_jev`], for `zirv workflow start` (`workflow::engine::
/// start_workflow`), which classifies before any `ExecutionProfile` exists
/// and has no domain-tag surface to add to -- so, deliberately, this asks
/// and applies only the intent Choice, never the domain Nouls. Same gate
/// (`cfg.jev.classify`), same facts ([`classify_jev_facts`]), same
/// byte-identical-when-off/no-credential/failed-call guarantee as
/// `refine_via_jev`; a decisive answer replaces `classification.intent`
/// in place, before the caller's own `selection::select_definition` runs,
/// so a replaced intent steers pack selection at start too.
pub(crate) fn refine_intent_via_jev(repo: &Path, task: &str, classification: &mut Classification) {
    let env = |key: &str| std::env::var(key).ok();
    let Ok(cfg) = CtxConfig::load(repo, &env) else {
        return;
    };
    if !cfg.jev.classify || !jev::available(&cfg.proxy.typesafe) {
        return;
    }
    let Ok(state) = StateDir::resolve(&env) else {
        return;
    };
    refine_intent_with_jev(&cfg, &state, task, classification);
}

/// [`refine_intent_via_jev`]'s testable core -- see [`refine_profile_with_jev`]'s
/// own doc comment for why this split exists.
fn refine_intent_with_jev(
    cfg: &CtxConfig,
    state: &StateDir,
    task: &str,
    classification: &mut Classification,
) {
    let facts = classify_jev_facts(task, classification);
    let questions = vec![classify_intent_question()];
    let Some(answers) = jev::advise(
        cfg,
        state,
        JEV_CLASSIFY_SITE,
        cfg.jev.classify,
        &facts,
        &questions,
    ) else {
        return;
    };
    apply_intent_answer(cfg, &answers, classification);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::workflow::classify::DomainClassification;
    use std::path::PathBuf;

    fn classification(paths: &[&str], lines: usize) -> Classification {
        crate::commands::workflow::classify::classify(
            &crate::commands::workflow::classify::ClassificationInput {
                task: "implement feature".to_string(),
                paths: paths.iter().map(PathBuf::from).collect(),
                changed_lines: lines,
                tests_changed: true,
                intent_override: None,
                complexity_override: None,
                risk_override: None,
            },
        )
        .expect("classification")
    }

    #[test]
    fn a_trivial_change_is_direct_with_no_validation() {
        let classification = classification(&["src/util.rs"], 5);
        assert_eq!(classification.complexity, Complexity::Trivial);
        let profile = ExecutionProfile::derive("fix a typo in a comment", &classification);
        assert_eq!(profile.execution, ExecutionMode::Direct);
        assert_eq!(profile.validation, ValidationProfile::default());
        assert_eq!(profile.domains, vec![WorkDomainTag::General]);
    }

    #[test]
    fn high_risk_or_security_requires_independent_review() {
        // Risk-driven: a sensitive auth path floors risk at High regardless
        // of the request text.
        let auth_classification = classification(&["src/auth/session.rs"], 20);
        assert!(auth_classification.risk >= RiskBand::High);
        let profile = ExecutionProfile::derive("harden session handling", &auth_classification);
        assert!(profile.validation.independent_review);
        assert!(profile.validation.security_review);

        // Domain-driven: request text alone, over an otherwise low-risk diff.
        let mut low_risk = classification(&["README.md"], 5);
        low_risk.risk = RiskBand::Low;
        low_risk.reasons.clear();
        let profile = ExecutionProfile::derive("rotate the shared credential secret", &low_risk);
        assert!(profile.validation.independent_review);
        assert!(profile.validation.security_review);
    }

    #[test]
    fn domain_tags_come_from_the_request_and_the_diff() {
        let classification = classification(&["src/dashboard/Billing.tsx"], 40);
        assert_eq!(classification.work_domain.domain, WorkDomain::Frontend);
        let profile = ExecutionProfile::derive(
            "wire up the deploy pipeline for the billing dashboard",
            &classification,
        );
        assert!(
            profile.domains.contains(&WorkDomainTag::Frontend),
            "{:?}",
            profile.domains
        );
        assert!(
            profile.domains.contains(&WorkDomainTag::DevOps),
            "{:?}",
            profile.domains
        );
    }

    #[test]
    fn unmeasured_risk_caps_confidence_at_low() {
        let mut classification = classification(&["README.md"], 5);
        classification.risk_measurement = RiskMeasurement::Unavailable {
            reason: "no repository".to_string(),
        };
        let profile = ExecutionProfile::derive("small doc fix", &classification);
        assert_eq!(profile.confidence, Confidence::Low);
    }

    #[test]
    fn declared_not_measured_scope_caps_confidence_at_medium() {
        let mut classification = classification(&["README.md"], 5);
        classification.declared_scope = true;
        let profile = ExecutionProfile::derive("small doc fix", &classification);
        assert_eq!(profile.confidence, Confidence::Medium);
    }

    #[test]
    fn substantial_complexity_alone_requires_independent_test() {
        let classification = classification(
            &[
                "src/a.rs", "src/b.rs", "src/c.rs", "src/d.rs", "src/e.rs", "src/f.rs",
            ],
            300,
        );
        assert!(classification.complexity >= Complexity::Substantial);
        let profile = ExecutionProfile::derive("implement the feature", &classification);
        assert!(profile.validation.independent_test);
        assert!(!profile.validation.independent_review);
    }

    #[test]
    fn empty_domain_classification_defaults_to_general_without_paths() {
        let classification = Classification {
            intent: crate::commands::workflow::classify::Intent::Other,
            complexity: Complexity::Trivial,
            risk: RiskBand::Low,
            risk_score: 0,
            changed_files: 0,
            changed_lines: 0,
            changed_paths: Vec::new(),
            declared_scope: false,
            work_domain: DomainClassification::default(),
            risk_measurement: RiskMeasurement::Measured,
            reasons: vec!["small, isolated deterministic change".to_string()],
        };
        let profile = ExecutionProfile::derive("say hello", &classification);
        assert_eq!(profile.domains, vec![WorkDomainTag::General]);
    }

    fn classify_config(base_url: String, credential_env: &str) -> CtxConfig {
        let mut cfg = CtxConfig::default();
        cfg.jev.classify = true;
        cfg.proxy.typesafe.base_url = base_url;
        cfg.proxy.typesafe.credential_env = credential_env.to_string();
        cfg
    }

    fn temp_state() -> (tempfile::TempDir, StateDir) {
        let dir = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(dir.path().to_path_buf());
        (dir, state)
    }

    /// Issue #782 acceptance: key off, or on with no credential, leaves
    /// [`ExecutionProfile::derive`]'s own output byte-identical -- across a
    /// few prompts, including the "rotate the shared token" miss the
    /// keyword-only domain lists cannot catch (no "credential"/"secret"/
    /// "auth" substring).
    #[test]
    fn key_off_or_missing_credential_leaves_profile_byte_identical() {
        let (_dir, state) = temp_state();
        for task in [
            "fix the login bug",
            "rotate the shared token",
            "refactor the billing module",
            "explore a caching approach for search",
        ] {
            let classification = classification(&["src/lib.rs"], 10);
            let baseline = ExecutionProfile::derive(task, &classification);

            let mut off = baseline.clone();
            refine_profile_with_jev(&CtxConfig::default(), &state, task, &mut off);
            assert_eq!(off, baseline, "key off changed the profile for {task:?}");

            let cfg_no_credential = classify_config(
                "http://127.0.0.1:1".to_string(),
                "JEV_TEST_CLASSIFY_NO_CRED",
            );
            let mut on_no_cred = baseline.clone();
            refine_profile_with_jev(&cfg_no_credential, &state, task, &mut on_no_cred);
            assert_eq!(
                on_no_cred, baseline,
                "missing credential changed the profile for {task:?}"
            );
        }
    }

    /// A decisive Choice replaces `intent` outright, and a decisive Noul
    /// clearing [`JEV_CLASSIFY_TAG_PROBABILITY`] adds a domain tag the
    /// keyword path missed ("rotate the shared token" names no keyword in
    /// [`DOMAIN_SIGNALS`]'s own `security` list), raising validation the
    /// same way the keyword-driven path already does.
    #[test]
    fn decisive_answers_replace_intent_and_add_a_security_domain_tag() {
        let body = r#"{"model":"jev-latest","answers":{
            "intent":{"type":"choice","choice":"bugfix","probabilities":{"bugfix":0.9,"feature":0.05,"refactor":0.02,"spike":0.01,"review":0.01,"other":0.01},"confidence":0.9},
            "security":{"type":"noul","noul":0.95},
            "data":{"type":"noul","noul":0.1},
            "docs_only":{"type":"noul","noul":0.1},
            "devops":{"type":"noul","noul":0.1},
            "architecture":{"type":"noul","noul":0.1},
            "frontend":{"type":"noul","noul":0.1}
        },"usage":{"input_tokens":10,"output_tokens":2}}"#;
        let (url, handle) = crate::commands::ctx::jev::tests::one_shot_server(200, body);
        let cfg = classify_config(url, "JEV_TEST_CLASSIFY_ONPATH");
        let _credential = crate::commands::ctx::testenv::VarGuard::set(&[(
            "JEV_TEST_CLASSIFY_ONPATH",
            Some("secret"),
        )]);
        let (_dir, state) = temp_state();
        let classification = classification(&["src/lib.rs"], 10);
        let mut profile = ExecutionProfile::derive("rotate the shared token", &classification);
        assert_eq!(profile.classification.intent, Intent::Feature);
        assert_eq!(profile.domains, vec![WorkDomainTag::General]);

        refine_profile_with_jev(&cfg, &state, "rotate the shared token", &mut profile);
        handle.join().expect("server thread must not panic");

        assert_eq!(profile.classification.intent, Intent::Bugfix);
        assert!(
            profile.domains.contains(&WorkDomainTag::Security),
            "{:?}",
            profile.domains
        );
        assert!(!profile.domains.contains(&WorkDomainTag::Data));
        assert!(profile.validation.independent_review);
        assert!(profile.validation.security_review);
    }

    /// A 5xx (or, by the same `jev::ask` codepath, a timeout) leaves the
    /// profile exactly as the deterministic path computed it -- the shared
    /// client's own fallback, not anything this site adds.
    #[test]
    fn server_error_leaves_profile_unchanged() {
        let (url, handle) = crate::commands::ctx::jev::tests::one_shot_server(500, "{}");
        let cfg = classify_config(url, "JEV_TEST_CLASSIFY_5XX");
        let _credential = crate::commands::ctx::testenv::VarGuard::set(&[(
            "JEV_TEST_CLASSIFY_5XX",
            Some("secret"),
        )]);
        let (_dir, state) = temp_state();
        let classification = classification(&["src/lib.rs"], 10);
        let baseline = ExecutionProfile::derive("rotate the shared token", &classification);
        let mut profile = baseline.clone();

        refine_profile_with_jev(&cfg, &state, "rotate the shared token", &mut profile);
        handle.join().expect("server thread must not panic");

        assert_eq!(profile, baseline);
    }

    /// Direction rule: a domain tag the keyword path already found (here,
    /// `frontend` from a `.tsx` path) survives even a confident dissenting
    /// Jev answer, and `risk` is never touched by this site at all.
    #[test]
    fn keyword_found_domain_tag_survives_a_dissenting_jev_answer_and_risk_never_moves() {
        let body = r#"{"model":"jev-latest","answers":{
            "intent":{"type":"choice","choice":"other","probabilities":{"other":0.3,"feature":0.3,"bugfix":0.1,"refactor":0.1,"spike":0.1,"review":0.1},"confidence":0.3},
            "security":{"type":"noul","noul":0.1},
            "data":{"type":"noul","noul":0.1},
            "docs_only":{"type":"noul","noul":0.1},
            "devops":{"type":"noul","noul":0.1},
            "architecture":{"type":"noul","noul":0.1},
            "frontend":{"type":"noul","noul":0.02}
        },"usage":{"input_tokens":10,"output_tokens":2}}"#;
        let (url, handle) = crate::commands::ctx::jev::tests::one_shot_server(200, body);
        let cfg = classify_config(url, "JEV_TEST_CLASSIFY_DIRECTION");
        let _credential = crate::commands::ctx::testenv::VarGuard::set(&[(
            "JEV_TEST_CLASSIFY_DIRECTION",
            Some("secret"),
        )]);
        let (_dir, state) = temp_state();
        let classification = classification(&["src/dashboard/Billing.tsx"], 40);
        assert_eq!(classification.work_domain.domain, WorkDomain::Frontend);
        let baseline_risk = classification.risk;
        let mut profile = ExecutionProfile::derive("implement feature", &classification);
        assert!(profile.domains.contains(&WorkDomainTag::Frontend));

        refine_profile_with_jev(&cfg, &state, "implement feature", &mut profile);
        handle.join().expect("server thread must not panic");

        assert!(
            profile.domains.contains(&WorkDomainTag::Frontend),
            "a keyword-found domain tag must never be removed: {:?}",
            profile.domains
        );
        assert_eq!(
            profile.classification.risk, baseline_risk,
            "this site must never touch risk"
        );
    }

    /// Issue #746 egress boundary: the exact facts/questions
    /// [`refine_via_jev`] builds must pass the shared client's own
    /// metadata-only guard.
    #[test]
    fn classify_jev_facts_and_questions_pass_the_metadata_only_guard() {
        let classification = classification(&["src/lib.rs"], 10);
        let facts = classify_jev_facts("rotate the shared token", &classification);
        let questions = classify_jev_questions();
        assert!(jev::safe_metadata_request(&facts, &questions, "jev-latest"));
        let facts_text = facts.to_string();
        assert!(!facts_text.contains("rotate"));
        assert!(!facts_text.contains("token"));
    }
}
