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

use serde::{Deserialize, Serialize};

use super::agents::ModelTier;
use super::classify::{Classification, Complexity, RiskBand, RiskMeasurement, WorkDomain};

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
        let domain_signals: [(&[&str], WorkDomainTag, &str); 5] = [
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
        for (needles, tag, reason) in domain_signals {
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
            declared_scope: false,
            work_domain: DomainClassification::default(),
            risk_measurement: RiskMeasurement::Measured,
            reasons: vec!["small, isolated deterministic change".to_string()],
        };
        let profile = ExecutionProfile::derive("say hello", &classification);
        assert_eq!(profile.domains, vec![WorkDomainTag::General]);
    }
}
