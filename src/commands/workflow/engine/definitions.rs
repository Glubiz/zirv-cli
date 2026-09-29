//! `WorkflowKind`/`WorkflowStep`/`WorkflowProfile` definitions and pack
//! materialization (issue #542-split).

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use clap::ValueEnum;
use serde::{Deserialize, Serialize};

use crate::commands::workflow::classify::{
    Classification, Complexity, Intent, RiskBand, WorkDomain,
};
use crate::commands::workflow::deploy::DeployTier;
use crate::commands::workflow::skill::WorkflowPhase;

use super::cli::*;
use super::state::*;
pub(super) const MAX_STEP_ATTEMPTS: u8 = 3;

const INTENT_TEMPLATE: &str = r#"# Intent

## Problem

<!-- What problem are we solving, for whom, and why now? -->

## Desired outcome

<!-- Describe the observable end state. -->

## Constraints

<!-- Technical, product, policy, compatibility, time, or scope constraints. -->

## Open questions

<!-- Keep only questions that materially affect correctness. Use "None" when resolved. -->

## Acceptance criteria

- [ ] <!-- Observable outcome -->
"#;

const SPEC_TEMPLATE: &str = r#"# Specification

## Context

<!-- Existing behavior, architecture, and evidence that constrain the design. -->

## Goals

- <!-- Goal -->

## Non-goals

- <!-- Explicitly out of scope -->

## Design

<!-- Chosen approach, affected boundaries, data/control flow, compatibility, and tradeoffs. -->

## Testing strategy

<!-- Deterministic checks and evidence required before completion. -->

## Risks

<!-- Material risks and mitigations. -->
"#;

const PLAN_TEMPLATE: &str = r#"# Implementation plan

## Ordered tasks

- [ ] T1: <!-- concrete task -->
  - Files: <!-- exact paths or bounded areas -->
  - Verify: <!-- exact command/check -->

## Execution ledger

| Task | Started | Finished | Evidence |
| --- | --- | --- | --- |
| T1 |  |  |  |
"#;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ValueEnum)]
#[serde(rename_all = "kebab-case")]
pub enum WorkflowKind {
    Feature,
    Bugfix,
    Refactor,
    Spike,
    Review,
}

impl WorkflowKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Feature => "feature",
            Self::Bugfix => "bugfix",
            Self::Refactor => "refactor",
            Self::Spike => "spike",
            Self::Review => "review",
        }
    }

    pub(crate) fn intent(self) -> Intent {
        match self {
            Self::Feature => Intent::Feature,
            Self::Bugfix => Intent::Bugfix,
            Self::Refactor => Intent::Refactor,
            Self::Spike => Intent::Spike,
            Self::Review => Intent::Review,
        }
    }

    /// Map classified intent to a legacy kind when one exists; `Other` has none. (#542)
    pub(crate) fn from_intent(intent: Intent) -> Option<Self> {
        match intent {
            Intent::Feature => Some(Self::Feature),
            Intent::Bugfix => Some(Self::Bugfix),
            Intent::Refactor => Some(Self::Refactor),
            Intent::Spike => Some(Self::Spike),
            Intent::Review => Some(Self::Review),
            Intent::Other => None,
        }
    }

    /// Map only the five legacy pack ids to `WorkflowKind`; other registry ids have no legacy counterpart. (#542)
    pub fn from_pack_id(id: &str) -> Option<Self> {
        match id {
            "feature" => Some(Self::Feature),
            "bugfix" => Some(Self::Bugfix),
            "refactor" => Some(Self::Refactor),
            "spike" => Some(Self::Spike),
            "review" => Some(Self::Review),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ArtifactStage {
    Intent,
    Spec,
    Plan,
}

impl ArtifactStage {
    pub(super) fn key(self) -> &'static str {
        match self {
            Self::Intent => "intent",
            Self::Spec => "spec",
            Self::Plan => "plan",
        }
    }

    pub(super) fn file_name(self) -> &'static str {
        match self {
            Self::Intent => "intent.md",
            Self::Spec => "spec.md",
            Self::Plan => "plan.md",
        }
    }

    pub(super) fn template(self) -> &'static str {
        match self {
            Self::Intent => INTENT_TEMPLATE,
            Self::Spec => SPEC_TEMPLATE,
            Self::Plan => PLAN_TEMPLATE,
        }
    }
}

impl std::fmt::Display for ArtifactStage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.key())
    }
}

/// Pin the pack id, version and hash so updates cannot silently change a running workflow; status reports drift. (#542)
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DefinitionRef {
    pub id: String,
    pub version: u32,
    pub hash: String,
    pub source_layer: crate::commands::workflow::registry::WorkflowSource,
    /// The full pinned definition, stored inline ONLY when `source_layer`
    /// is not `BuiltIn` -- a repository or operator-global pack file can be
    /// edited or deleted out from under a running workflow, so anything but
    /// a built-in (versioned with the zirv binary itself, and therefore
    /// stable for the life of the run) must carry its own copy rather than
    /// trust the registry to still resolve `id` the same way later.
    #[serde(default)]
    pub inline: Option<crate::commands::workflow::definition::WorkflowDefinitionV2>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkflowArtifactRecord {
    pub stage: ArtifactStage,
    pub rel_path: String,
    pub accepted_hash: Option<String>,
    pub accepted_at: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum StepCondition {
    Always,
    ComplexityAtLeast(Complexity),
    RiskAtLeast(RiskBand),
    ComplexityOrRisk {
        complexity: Complexity,
        risk: RiskBand,
    },
}

/// Share condition evaluation between materialization and persisted-step readers so they cannot diverge. (#542)
pub(super) fn condition_applies(condition: StepCondition, classification: &Classification) -> bool {
    match condition {
        StepCondition::Always => true,
        StepCondition::ComplexityAtLeast(minimum) => classification.complexity >= minimum,
        StepCondition::RiskAtLeast(minimum) => classification.risk >= minimum,
        StepCondition::ComplexityOrRisk { complexity, risk } => {
            classification.complexity >= complexity || classification.risk >= risk
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkflowStep {
    pub id: String,
    pub phase: WorkflowPhase,
    pub skill: String,
    /// Provider-neutral workflow seat. This is an address, not authority:
    /// dispatch still resolves the seat through the trusted agent registry and
    /// narrows it through the effective policy.
    #[serde(default)]
    pub agent: Option<String>,
    #[serde(default)]
    pub artifact: Option<ArtifactStage>,
    pub condition: StepCondition,
    pub approval: bool,
    pub max_attempts: u8,
    /// Persist the parallel group as metadata; the state machine still advances through one current step. (#542)
    #[serde(default)]
    pub parallel_group: Option<String>,
    #[serde(default)]
    pub effect: crate::commands::workflow::definition::EffectClass,
}

#[cfg(test)]
impl WorkflowStep {
    /// Only the legacy oracle (`WorkflowDefinition::materialize`) still uses
    /// this -- the production path (`materialize_from_definition`) prunes
    /// directly from `StepV2::condition` via `condition_applies`.
    fn applies(&self, classification: &Classification) -> bool {
        condition_applies(self.condition, classification)
    }
}

/// The legacy (pre-#542) per-kind literal step list, kept ONLY as the
/// oracle `converted_packs_materialise_identically_to_the_legacy_literals`
/// compares the new `packs/*.toml`-driven pipeline against -- never built
/// into the production binary. See `definitions()`'s own doc comment.
#[cfg(test)]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct WorkflowDefinition {
    schema_version: u32,
    kind: WorkflowKind,
    description: String,
    steps: Vec<WorkflowStep>,
}

#[cfg(test)]
impl WorkflowDefinition {
    fn materialize(&self, classification: &Classification) -> Vec<WorkflowStep> {
        self.steps
            .iter()
            .filter(|step| step.applies(classification))
            .cloned()
            .collect()
    }
}

pub(super) fn seat_for_phase(phase: WorkflowPhase) -> Option<String> {
    match phase {
        WorkflowPhase::Implement | WorkflowPhase::Debug => Some("implementer".into()),
        WorkflowPhase::Review => Some("reviewer".into()),
        _ => None,
    }
}

pub(super) fn step(
    id: &str,
    phase: WorkflowPhase,
    skill: &str,
    condition: StepCondition,
    approval: bool,
) -> WorkflowStep {
    WorkflowStep {
        id: id.to_string(),
        phase,
        skill: skill.to_string(),
        agent: seat_for_phase(phase),
        artifact: None,
        condition,
        approval,
        max_attempts: MAX_STEP_ATTEMPTS,
        parallel_group: None,
        effect: crate::commands::workflow::definition::EffectClass::None,
    }
}

/// Only the legacy oracle (`definitions()`) uses this now -- a real pack
/// authors an artifact-gated step directly as `StepV2` TOML/YAML data.
#[cfg(test)]
pub(super) fn artifact_step(
    id: &str,
    phase: WorkflowPhase,
    skill: &str,
    stage: ArtifactStage,
    condition: StepCondition,
) -> WorkflowStep {
    WorkflowStep {
        id: id.to_string(),
        phase,
        skill: skill.to_string(),
        agent: None,
        artifact: Some(stage),
        condition,
        approval: true,
        max_attempts: MAX_STEP_ATTEMPTS,
        parallel_group: None,
        effect: crate::commands::workflow::definition::EffectClass::Repository,
    }
}

/// The pre-#542 hardcoded, per-`WorkflowKind` step literals. Kept ONLY as
/// the oracle for `converted_packs_materialise_identically_to_the_legacy_
/// literals` (`#[cfg(test)]`, never compiled into the production binary):
/// `packs/*.toml` -- loaded through `WorkflowDefinitionV2` and
/// `materialize_from_definition` -- is now the ONLY step-list construction
/// path a real `zirv workflow start` ever runs (issue #542 chunk 3a,
/// decision 5's deferred back half).
#[cfg(test)]
pub(super) fn definitions() -> Vec<WorkflowDefinition> {
    use ArtifactStage as Artifact;
    use Complexity as C;
    use RiskBand as R;
    use StepCondition as When;
    use WorkflowPhase as Phase;
    vec![
        WorkflowDefinition {
            schema_version: WORKFLOW_SCHEMA_VERSION,
            kind: WorkflowKind::Feature,
            description: "Capture intent, design and plan proportionally, then implement, test and review.".into(),
            steps: vec![
                artifact_step(
                    "intent",
                    Phase::Intent,
                    "brainstorm",
                    Artifact::Intent,
                    When::ComplexityOrRisk {
                        complexity: C::Bounded,
                        risk: R::Medium,
                    },
                ),
                artifact_step(
                    "spec",
                    Phase::Design,
                    "design",
                    Artifact::Spec,
                    When::ComplexityOrRisk {
                        complexity: C::Substantial,
                        risk: R::High,
                    },
                ),
                artifact_step(
                    "plan",
                    Phase::Plan,
                    "plan",
                    Artifact::Plan,
                    When::ComplexityOrRisk {
                        complexity: C::Bounded,
                        risk: R::High,
                    },
                ),
                step("implement", Phase::Implement, "implement", When::Always, false),
                step("test", Phase::Test, "testing", When::Always, false),
                step(
                    "simplify",
                    Phase::Implement,
                    "simplify",
                    When::RiskAtLeast(R::Medium),
                    false,
                ),
                step(
                    "review",
                    Phase::Review,
                    "review",
                    When::RiskAtLeast(R::Medium),
                    false,
                ),
                step("verify", Phase::Verify, "verify", When::Always, false),
                step("deploy", Phase::Deploy, "finish-branch", When::Always, false),
            ],
        },
        WorkflowDefinition {
            schema_version: WORKFLOW_SCHEMA_VERSION,
            kind: WorkflowKind::Bugfix,
            description: "Capture intent when warranted, reproduce, plan larger fixes, test and verify.".into(),
            steps: vec![
                artifact_step(
                    "intent",
                    Phase::Intent,
                    "brainstorm",
                    Artifact::Intent,
                    When::ComplexityOrRisk {
                        complexity: C::Bounded,
                        risk: R::Medium,
                    },
                ),
                step("debug", Phase::Debug, "systematic-debugging", When::Always, false),
                artifact_step(
                    "plan",
                    Phase::Plan,
                    "plan",
                    Artifact::Plan,
                    When::ComplexityOrRisk {
                        complexity: C::Substantial,
                        risk: R::High,
                    },
                ),
                step("implement", Phase::Implement, "implement", When::Always, false),
                step("test", Phase::Test, "testing", When::Always, false),
                step(
                    "simplify",
                    Phase::Implement,
                    "simplify",
                    When::RiskAtLeast(R::Medium),
                    false,
                ),
                step(
                    "review",
                    Phase::Review,
                    "review",
                    When::RiskAtLeast(R::Medium),
                    false,
                ),
                step("verify", Phase::Verify, "verify", When::Always, false),
                step("deploy", Phase::Deploy, "finish-branch", When::Always, false),
            ],
        },
        WorkflowDefinition {
            schema_version: WORKFLOW_SCHEMA_VERSION,
            kind: WorkflowKind::Refactor,
            description: "Plan proportional behavior-preserving changes with intent capture for substantial or high-risk work.".into(),
            steps: vec![
                artifact_step(
                    "intent",
                    Phase::Intent,
                    "brainstorm",
                    Artifact::Intent,
                    When::ComplexityOrRisk {
                        complexity: C::Substantial,
                        risk: R::High,
                    },
                ),
                artifact_step(
                    "plan",
                    Phase::Plan,
                    "plan",
                    Artifact::Plan,
                    When::ComplexityOrRisk {
                        complexity: C::Bounded,
                        risk: R::Medium,
                    },
                ),
                step("implement", Phase::Implement, "implement", When::Always, false),
                step("test", Phase::Test, "testing", When::Always, false),
                step(
                    "simplify",
                    Phase::Implement,
                    "simplify",
                    When::RiskAtLeast(R::Medium),
                    false,
                ),
                step(
                    "review",
                    Phase::Review,
                    "review",
                    When::RiskAtLeast(R::Medium),
                    false,
                ),
                step("verify", Phase::Verify, "verify", When::Always, false),
                step("deploy", Phase::Deploy, "finish-branch", When::Always, false),
            ],
        },
        WorkflowDefinition {
            schema_version: WORKFLOW_SCHEMA_VERSION,
            kind: WorkflowKind::Spike,
            description: "Capture intent, run time-bounded exploration and record explicit findings.".into(),
            steps: vec![
                artifact_step("intent", Phase::Intent, "brainstorm", Artifact::Intent, When::Always),
                step("design", Phase::Design, "design", When::Always, false),
                step("implement", Phase::Implement, "implement", When::Always, false),
                step(
                    "verify",
                    Phase::Verify,
                    "verify",
                    When::RiskAtLeast(R::Medium),
                    false,
                ),
            ],
        },
        WorkflowDefinition {
            schema_version: WORKFLOW_SCHEMA_VERSION,
            kind: WorkflowKind::Review,
            description: "Independent review with inspectable disposition.".into(),
            steps: vec![
                step("review", Phase::Review, "review", When::Always, false),
                step(
                    "verify",
                    Phase::Verify,
                    "verify",
                    When::RiskAtLeast(R::High),
                    false,
                ),
            ],
        },
    ]
}

#[cfg(test)]
pub(super) fn definition(kind: WorkflowKind) -> WorkflowDefinition {
    definitions()
        .into_iter()
        .find(|definition| definition.kind == kind)
        .expect("every WorkflowKind has a built-in definition")
}

/// The EXACT pre-#542 `apply_profile` body, preserved only as the oracle
/// `converted_packs_materialise_identically_to_the_legacy_literals` compares
/// the pack-driven pipeline against -- never called by production code
/// (that now goes through the new `apply_profile`, keyed on
/// `WorkflowDefinitionV2` domain variants, defined earlier in this file).
#[cfg(test)]
pub(super) fn legacy_apply_profile(
    kind: WorkflowKind,
    profile: WorkflowProfile,
    steps: &mut [WorkflowStep],
) {
    let defaults = definition(kind).steps;
    for step in steps {
        // The `simplify` step (issue: simplify-paired-with-review) shares
        // `Phase::Implement` with `implement` itself but, like `implement`'s
        // OWN pre-#542 phase-keyed selection below, has no frontend variant
        // -- the real pack-driven `select_step_data` therefore always falls
        // back to its own primary skill regardless of profile. This oracle
        // is otherwise keyed purely on phase (a safe simplification when
        // exactly one step ever occupied each phase); `simplify` breaks that
        // one-step-per-phase assumption, so it is special-cased here rather
        // than widening the match below to carry id-awareness for every
        // phase.
        if step.id == "simplify" {
            continue;
        }
        step.skill = match (profile, step.phase) {
            (_, WorkflowPhase::Intent) => continue,
            (WorkflowProfile::Frontend, WorkflowPhase::Design) => "frontend-design",
            (WorkflowProfile::Standard, WorkflowPhase::Design) => "design",
            (WorkflowProfile::Frontend, WorkflowPhase::Plan) => "frontend-plan",
            (WorkflowProfile::Standard, WorkflowPhase::Plan) => "plan",
            (WorkflowProfile::Frontend, WorkflowPhase::Implement) => "frontend-implement",
            (WorkflowProfile::Standard, WorkflowPhase::Implement) => "implement",
            (WorkflowProfile::Frontend, WorkflowPhase::Debug) => "frontend-debug",
            (WorkflowProfile::Standard, WorkflowPhase::Debug) => "systematic-debugging",
            (WorkflowProfile::Frontend, WorkflowPhase::Test) => "frontend-test",
            (WorkflowProfile::Standard, WorkflowPhase::Test) => "testing",
            (WorkflowProfile::Frontend, WorkflowPhase::Review) => "frontend-review",
            (WorkflowProfile::Standard, WorkflowPhase::Review) => "review",
            (WorkflowProfile::Frontend, WorkflowPhase::Verify) => "frontend-verify",
            (WorkflowProfile::Standard, WorkflowPhase::Verify) => "verify",
            (_, WorkflowPhase::Deploy | WorkflowPhase::Delegate | WorkflowPhase::Present) => {
                continue;
            }
        }
        .into();
        if step.phase == WorkflowPhase::Design && step.artifact.is_none() {
            if profile == WorkflowProfile::Frontend {
                step.approval = false;
            } else if let Some(default) = defaults.iter().find(|candidate| candidate.id == step.id)
            {
                step.approval = default.approval;
            }
        }
    }
}

/// The EXACT pre-#542 `materialize` body (same oracle role as
/// `legacy_apply_profile`, above).
#[cfg(test)]
pub(super) fn legacy_materialize(
    kind: WorkflowKind,
    classification: &Classification,
    profile: WorkflowProfile,
    deploy_tier: DeployTier,
    brainstorm: bool,
) -> Vec<WorkflowStep> {
    let mut steps = definition(kind).materialize(classification);
    legacy_apply_profile(kind, profile, &mut steps);
    // Always one of the five legacy kinds by construction (`kind` is a
    // `WorkflowKind`, not an arbitrary pack id).
    apply_brainstorm_selection(brainstorm, true, &mut steps);
    apply_deploy_tier(deploy_tier, &mut steps);
    steps
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum WorkflowStatus {
    Running,
    AwaitingApproval,
    Failed,
    Completed,
    /// Explicitly closed via `zirv workflow close` -- typically a workflow
    /// whose review/fix loop hit `MAX_FIX_REVIEW_ROUNDS` (review.rs) and
    /// would otherwise stay `Running` forever, still reported as this
    /// repository's active workflow by `load_active`. Terminal, like
    /// `Failed`/`Completed`: `resume` refuses to re-activate it.
    Closed,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, ValueEnum)]
#[serde(rename_all = "kebab-case")]
pub enum WorkflowProfile {
    #[default]
    Standard,
    Frontend,
}

impl WorkflowProfile {
    pub(super) fn for_classification(classification: &Classification) -> Self {
        match classification.work_domain.domain {
            WorkDomain::Frontend => Self::Frontend,
            WorkDomain::General => Self::Standard,
        }
    }
}

/// Whether a workflow's `profile` came from automatic classification or was
/// later forced by an operator.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ProfileSource {
    #[default]
    Classified,
    OperatorOverride,
}

/// Recorded once an operator accepts a workflow's pre-existing blocking
/// frontend findings with `--accept-preexisting-findings` (#251): pre-dating
/// findings the detector's full-surface scan turned up that were not
/// introduced by this change. `blocking`/`total` are the pre-existing counts
/// at the moment of acceptance, not a live re-count.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AcceptedPreexistingFindings {
    pub step: String,
    pub at: String,
    pub blocking: usize,
    pub total: usize,
}

/// Select profile-specific step data while keeping canonical id, phase, dependencies and order; profile-invariant phases use the primary step. (#542)
// Shared by materialize_from_definition and apply_profile so initial build and later reclassify can never disagree on which phases are profile-invariant. (#542)
pub(super) const PROFILE_INVARIANT_PHASES: [WorkflowPhase; 4] = [
    WorkflowPhase::Intent,
    WorkflowPhase::Deploy,
    WorkflowPhase::Delegate,
    WorkflowPhase::Present,
];

pub(super) fn select_step_data<'a>(
    definition: &'a crate::commands::workflow::definition::WorkflowDefinitionV2,
    primary: &'a crate::commands::workflow::definition::StepV2,
    profile: WorkflowProfile,
) -> &'a crate::commands::workflow::definition::StepV2 {
    let domain = match profile {
        WorkflowProfile::Frontend => "frontend",
        WorkflowProfile::Standard => return primary,
    };
    definition
        .steps
        .iter()
        .find(|candidate| {
            candidate.overrides_step.as_deref() == Some(primary.id.as_str())
                && candidate.domains.iter().any(|tag| tag == domain)
        })
        .unwrap_or(primary)
}

/// Re-select profile data in place without changing step identity or completion; unknown primary ids are left untouched. (#542)
pub(super) fn apply_profile(
    definition: &crate::commands::workflow::definition::WorkflowDefinitionV2,
    profile: WorkflowProfile,
    steps: &mut [WorkflowStep],
) {
    for step in steps {
        if PROFILE_INVARIANT_PHASES.contains(&step.phase) {
            continue;
        }
        let Some(primary) = definition
            .steps
            .iter()
            .find(|candidate| candidate.overrides_step.is_none() && candidate.id == step.id)
        else {
            continue;
        };
        let effective = select_step_data(definition, primary, profile);
        step.skill = effective
            .skills
            .first()
            .cloned()
            .unwrap_or_else(|| primary.skills[0].clone());
        step.agent = effective.agent_role.clone();
        step.approval = effective.approval;
        step.artifact = effective.artifact;
        step.max_attempts = effective.max_attempts;
        step.effect = effective.effect;
    }
}

/// Legacy intent defaults use brainstorming for Feature/Spike and autonomous writing for Bugfix/Refactor; other packs use their authored skill. (#542)
pub(super) fn default_brainstorm_for_kind(kind: WorkflowKind) -> bool {
    matches!(kind, WorkflowKind::Feature | WorkflowKind::Spike)
}

/// Apply brainstorm/write-intent substitution only to legacy packs; non-legacy packs retain their authored intent skill. (#542)
pub(super) fn apply_brainstorm_selection(
    brainstorm: bool,
    legacy_eligible: bool,
    steps: &mut [WorkflowStep],
) {
    if !legacy_eligible {
        return;
    }
    for step in steps {
        if step.phase == WorkflowPhase::Intent {
            step.skill = if brainstorm {
                "brainstorm"
            } else {
                "write-intent"
            }
            .into();
        }
    }
}

/// Apply deploy readiness by phase for any pack; an inserted Review step is a fixed safety gate. (#542)
pub(super) fn apply_deploy_tier(tier: DeployTier, steps: &mut Vec<WorkflowStep>) {
    if tier == DeployTier::Production
        && !steps.iter().any(|step| step.phase == WorkflowPhase::Review)
        && let Some(verify_index) = steps
            .iter()
            .position(|step| step.phase == WorkflowPhase::Verify)
    {
        steps.insert(
            verify_index,
            // Use reserved `__review` so a synthetic safety step cannot collide with authored step ids. (#542)
            step(
                "__review",
                WorkflowPhase::Review,
                "review",
                StepCondition::Always,
                false,
            ),
        );
    }
    for step in steps {
        if step.phase == WorkflowPhase::Deploy {
            // A deploy-tier overlay may widen approval but must never clear a gate authored by the pack. (#542)
            step.approval = step.approval || tier >= DeployTier::Staging;
        }
    }
}

/// Materialize one validated definition by pruning, profile selection and stable dependency order, then apply intent and deploy overlays. (#542)
pub(super) fn materialize_from_definition(
    definition: &crate::commands::workflow::definition::WorkflowDefinitionV2,
    classification: &Classification,
    profile: WorkflowProfile,
    deploy_tier: DeployTier,
    brainstorm: bool,
) -> Vec<WorkflowStep> {
    let primaries: Vec<&crate::commands::workflow::definition::StepV2> = definition
        .steps
        .iter()
        .filter(|step| step.overrides_step.is_none())
        .filter(|step| condition_applies(step.condition, classification))
        .collect();
    let surviving_ids: BTreeSet<&str> = primaries.iter().map(|step| step.id.as_str()).collect();

    // Kahn's algorithm. A dependency on a step this classification pruned
    // is vacuously satisfied -- the same "a filtered-out step is simply
    // absent from the ordering" the pre-#542 flat Vec model already relied
    // on for its own `StepCondition` pruning.
    let mut indegree: BTreeMap<&str, usize> =
        primaries.iter().map(|step| (step.id.as_str(), 0)).collect();
    let mut dependents: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    for step in &primaries {
        for dependency in &step.depends_on {
            if surviving_ids.contains(dependency.as_str()) {
                *indegree.get_mut(step.id.as_str()).expect("primary id") += 1;
                dependents
                    .entry(dependency.as_str())
                    .or_default()
                    .push(step.id.as_str());
            }
        }
    }
    let declaration_order: BTreeMap<&str, usize> = primaries
        .iter()
        .enumerate()
        .map(|(index, step)| (step.id.as_str(), index))
        .collect();
    let mut ready: Vec<&str> = primaries
        .iter()
        .map(|step| step.id.as_str())
        .filter(|id| indegree[id] == 0)
        .collect();
    let mut ordered_ids: Vec<&str> = Vec::with_capacity(primaries.len());
    while !ready.is_empty() {
        ready.sort_by_key(|id| declaration_order[id]);
        let next = ready.remove(0);
        ordered_ids.push(next);
        if let Some(nexts) = dependents.get(next) {
            for &dependent in nexts {
                let entry = indegree.get_mut(dependent).expect("dependent id");
                *entry -= 1;
                if *entry == 0 {
                    ready.push(dependent);
                }
            }
        }
    }
    // A cycle among surviving steps cannot happen: `WorkflowDefinitionV2::
    // validate` already rejects any cycle in the full (unpruned) graph, and
    // pruning only ever removes nodes/edges, which cannot introduce one.
    debug_assert_eq!(ordered_ids.len(), primaries.len());

    let primaries_by_id: BTreeMap<&str, &crate::commands::workflow::definition::StepV2> = primaries
        .iter()
        .map(|step| (step.id.as_str(), *step))
        .collect();
    let mut steps: Vec<WorkflowStep> = ordered_ids
        .into_iter()
        .map(|id| {
            let primary = primaries_by_id[id];
            // Enforce profile-invariant phases in initial materialization as well as later reclassification. (#542)
            let effective = if PROFILE_INVARIANT_PHASES.contains(&primary.phase) {
                primary
            } else {
                select_step_data(definition, primary, profile)
            };
            WorkflowStep {
                id: primary.id.clone(),
                phase: primary.phase,
                skill: effective
                    .skills
                    .first()
                    .cloned()
                    .unwrap_or_else(|| primary.skills[0].clone()),
                agent: effective.agent_role.clone(),
                artifact: effective.artifact,
                condition: primary.condition,
                approval: effective.approval,
                max_attempts: effective.max_attempts,
                parallel_group: primary.parallel_group.clone(),
                effect: effective.effect,
            }
        })
        .collect();

    apply_brainstorm_selection(
        brainstorm,
        WorkflowKind::from_pack_id(&definition.id).is_some(),
        &mut steps,
    );
    apply_deploy_tier(deploy_tier, &mut steps);
    steps
}

/// Resolve the pinned inline definition, matching built-in, or legacy kind fallback; built-in parsing is a tested invariant. (#542)
pub(super) fn resolve_definition_for_state(
    state: &WorkflowState,
) -> crate::commands::workflow::definition::WorkflowDefinitionV2 {
    if let Some(reference) = &state.definition {
        if let Some(inline) = &reference.inline {
            return inline.clone();
        }
        if let Some(builtin) =
            crate::commands::workflow::registry::builtin_definition(&reference.id)
        {
            return builtin;
        }
    }
    crate::commands::workflow::registry::builtin_definition(state.kind.as_str())
        .expect("every WorkflowKind maps to a built-in pack")
}

/// Prefer a live registry override; unreadable registry falls back to embedded built-in data so state start remains infallible. (#542)
pub(super) fn resolve_builtin_or_registry(
    repo: &Path,
    kind: WorkflowKind,
    include_custom_skills: bool,
) -> (
    crate::commands::workflow::definition::WorkflowDefinitionV2,
    String,
    crate::commands::workflow::registry::WorkflowSource,
) {
    if let Ok(registry) = load_workflow_registry(repo, !include_custom_skills)
        && let Ok(pack) = registry.get(kind.as_str())
    {
        return (pack.definition.clone(), pack.hash.clone(), pack.source);
    }
    let definition = crate::commands::workflow::registry::builtin_definition(kind.as_str())
        .expect("every WorkflowKind maps to a built-in pack");
    let hash = definition.hash().expect("built-in pack hashes");
    (
        definition,
        hash,
        crate::commands::workflow::registry::WorkflowSource::BuiltIn,
    )
}

/// Compose step skills from the primary skill and accepted-plan executor when needed; this helper is private to context rendering. (#539)
pub(super) fn step_skill_ids(step: &WorkflowStep, classification: &Classification) -> Vec<String> {
    let mut ids = Vec::new();
    // `simplify` shares the Implement phase but is a reuse pass over a
    // finished diff, not a plan to execute.
    if step.phase == WorkflowPhase::Implement
        && step.skill != "simplify"
        && classification.complexity >= Complexity::Substantial
    {
        ids.push("execute-plan".to_string());
    }
    ids.push(step.skill.clone());
    ids
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use std::path::{Path, PathBuf};

    use tempfile::tempdir;

    use crate::commands::ctx::state::StateDir;
    use crate::commands::workflow::agents::AgentRegistry;
    use crate::commands::workflow::classify::{Classification, Complexity, RiskBand, WorkDomain};
    use crate::commands::workflow::deploy::DeployTier;
    use crate::commands::workflow::skill::{SkillRegistry, WorkflowPhase};

    use super::*;

    use super::super::lifecycle::*;

    use super::super::tests::{git_init_with_commit, low_classification};
    use super::super::transition::*;
    #[test]
    fn trivial_feature_skips_intent_spec_plan_and_review() {
        // Feature's intent step now shares Bugfix's ComplexityOrRisk{Bounded,
        // Medium} condition instead of `Always`, so a trivial/low-risk
        // classification no longer stops at an approval-gated intent
        // artifact.
        let steps = definition(WorkflowKind::Feature).materialize(&low_classification());
        assert_eq!(
            steps
                .iter()
                .map(|step| step.id.as_str())
                .collect::<Vec<_>>(),
            ["implement", "test", "verify", "deploy"]
        );
    }

    #[test]
    fn bounded_feature_keeps_the_intent_step() {
        let mut classification = low_classification();
        classification.complexity = Complexity::Bounded;
        let steps = definition(WorkflowKind::Feature).materialize(&classification);
        assert_eq!(steps.first().unwrap().id, "intent");
        assert_eq!(steps[0].artifact, Some(ArtifactStage::Intent));
        assert!(steps[0].approval);
    }

    #[test]
    fn high_risk_substantial_feature_keeps_design_gate_and_review() {
        let mut classification = low_classification();
        classification.complexity = Complexity::Substantial;
        classification.risk = RiskBand::High;
        let steps = definition(WorkflowKind::Feature).materialize(&classification);
        assert!(steps.first().unwrap().approval);
        assert!(steps.iter().any(|step| step.id == "review"));
    }

    #[test]
    fn high_risk_bounded_feature_keeps_design_gate() {
        let mut classification = low_classification();
        classification.complexity = Complexity::Bounded;
        classification.risk = RiskBand::High;
        let steps = definition(WorkflowKind::Feature).materialize(&classification);
        assert_eq!(
            steps
                .iter()
                .map(|step| step.id.as_str())
                .collect::<Vec<_>>(),
            [
                "intent",
                "spec",
                "plan",
                "implement",
                "test",
                "simplify",
                "review",
                "verify",
                "deploy"
            ]
        );
        assert!(steps[1].approval);
        assert_eq!(steps[1].artifact, Some(ArtifactStage::Spec));
    }

    #[test]
    fn deploy_tier_matrix_adds_structural_gates() {
        let classification = low_classification();
        let profile = WorkflowProfile::Standard;
        let definition = crate::commands::workflow::registry::builtin_definition("feature")
            .expect("feature pack");

        let development = materialize_from_definition(
            &definition,
            &classification,
            profile,
            DeployTier::Development,
            true,
        );
        let development_deploy = development
            .iter()
            .find(|step| step.phase == WorkflowPhase::Deploy)
            .unwrap();
        assert!(!development_deploy.approval);
        assert!(
            !development
                .iter()
                .any(|step| step.phase == WorkflowPhase::Review)
        );

        let staging = materialize_from_definition(
            &definition,
            &classification,
            profile,
            DeployTier::Staging,
            true,
        );
        assert!(
            staging
                .iter()
                .find(|step| step.phase == WorkflowPhase::Deploy)
                .unwrap()
                .approval
        );
        assert!(
            !staging
                .iter()
                .any(|step| step.phase == WorkflowPhase::Review)
        );

        let production = materialize_from_definition(
            &definition,
            &classification,
            profile,
            DeployTier::Production,
            true,
        );
        let review = production
            .iter()
            .position(|step| step.phase == WorkflowPhase::Review)
            .unwrap();
        let verify = production
            .iter()
            .position(|step| step.phase == WorkflowPhase::Verify)
            .unwrap();
        let deploy = production
            .iter()
            .position(|step| step.phase == WorkflowPhase::Deploy)
            .unwrap();
        assert!(review < verify && verify < deploy);
        assert!(production[review].agent.as_deref() == Some("reviewer"));
        assert!(production[deploy].approval);
        // Issue #542 review nit: the synthetic step uses the reserved id
        // `__review` (never a valid AUTHORED id -- `definition::valid_id`
        // rejects a leading `_`), so it can never collide with a pack-
        // authored step that names itself "review" for some other phase.
        assert_eq!(production[review].id, "__review");
    }

    /// Issue #542 review finding 7: `apply_deploy_tier` must only ever WIDEN
    /// a Deploy-phase step's `approval`, never clear one the pack itself
    /// authored. `devops-ci-cd-change`'s `deploy` step declares `approval =
    /// true` unconditionally ("a pipeline change reaching production
    /// infrastructure is always an explicit approval, regardless of the
    /// operator's default deploy tier") -- at `DeployTier::Development`, the
    /// tier-derived condition (`tier >= DeployTier::Staging`) alone would be
    /// `false`, so the old `step.approval = tier >= DeployTier::Staging`
    /// unconditional assignment silently dropped the pack-authored gate.
    #[test]
    fn a_pack_authored_approval_gate_survives_a_lower_deploy_tier() {
        let classification = low_classification();
        let definition =
            crate::commands::workflow::registry::builtin_definition("devops-ci-cd-change")
                .expect("devops-ci-cd-change pack");

        let development = materialize_from_definition(
            &definition,
            &classification,
            WorkflowProfile::Standard,
            DeployTier::Development,
            true,
        );
        let deploy = development
            .iter()
            .find(|step| step.phase == WorkflowPhase::Deploy)
            .expect("deploy step present");
        assert!(
            deploy.approval,
            "a pack-authored Deploy-phase approval gate must survive a lower deploy tier"
        );
    }

    #[test]
    fn frontend_classification_selects_the_frontend_profile_automatically() {
        let repo = tempdir().unwrap();
        let mut classification = low_classification();
        // Feature's intent step is now conditional (`ComplexityOrRisk{Bounded,
        // Medium}`, same as Bugfix), so this needs a classification that
        // still gates one in.
        classification.complexity = Complexity::Bounded;
        classification.work_domain.domain = WorkDomain::Frontend;
        classification.work_domain.score = 55;

        let state = WorkflowState::start(
            repo.path().to_path_buf(),
            "build a responsive dashboard UI".into(),
            WorkflowKind::Feature,
            None,
            true,
            classification,
        );

        assert_eq!(state.profile, WorkflowProfile::Frontend);
        assert_eq!(state.current().unwrap().skill, "brainstorm");
        assert_eq!(
            state.current().unwrap().artifact,
            Some(ArtifactStage::Intent)
        );
        assert!(
            state
                .steps
                .iter()
                .find(|step| step.phase == WorkflowPhase::Implement)
                .is_some_and(|step| step.skill == "frontend-implement")
        );
    }

    #[test]
    fn brainstorm_defaults_per_kind() {
        assert!(default_brainstorm_for_kind(WorkflowKind::Feature));
        assert!(default_brainstorm_for_kind(WorkflowKind::Spike));
        assert!(!default_brainstorm_for_kind(WorkflowKind::Bugfix));
        assert!(!default_brainstorm_for_kind(WorkflowKind::Refactor));
        assert!(!default_brainstorm_for_kind(WorkflowKind::Review));
    }

    #[test]
    fn brainstorm_selects_the_intent_step_skill_and_survives_an_explicit_override() {
        let repo = tempdir().unwrap();
        // Feature's intent step is now conditional (`ComplexityOrRisk{Bounded,
        // Medium}`, same as Bugfix), so this needs a classification that
        // still gates one in.
        let mut feature_classification = low_classification();
        feature_classification.complexity = Complexity::Bounded;
        let feature = WorkflowState::start(
            repo.path().to_path_buf(),
            "small feature".into(),
            WorkflowKind::Feature,
            None,
            true,
            feature_classification,
        );
        assert_eq!(feature.current().unwrap().skill, "brainstorm");
        assert!(feature.brainstorm);

        let mut classification = low_classification();
        classification.complexity = Complexity::Bounded;
        classification.risk = RiskBand::Medium;
        let bugfix = WorkflowState::start(
            repo.path().to_path_buf(),
            "small bugfix".into(),
            WorkflowKind::Bugfix,
            None,
            true,
            classification,
        );
        assert_eq!(bugfix.current().unwrap().skill, "write-intent");
        assert!(!bugfix.brainstorm);

        let mut overridden = bugfix;
        apply_brainstorm_selection(true, true, &mut overridden.steps);
        assert_eq!(overridden.current().unwrap().skill, "brainstorm");
    }

    #[test]
    fn frontend_design_is_autonomous_but_keeps_the_evidence_phases() {
        let mut classification = low_classification();
        classification.complexity = Complexity::Substantial;
        classification.risk = RiskBand::High;
        classification.work_domain.domain = WorkDomain::Frontend;
        classification.work_domain.score = 55;

        let state = WorkflowState::start(
            PathBuf::from("repo"),
            "build a frontend design system".into(),
            WorkflowKind::Feature,
            None,
            true,
            classification,
        );

        assert_eq!(state.status, WorkflowStatus::AwaitingApproval);
        assert_eq!(state.steps[0].skill, "brainstorm");
        let design = state
            .steps
            .iter()
            .find(|step| step.phase == WorkflowPhase::Design)
            .expect("substantial frontend has spec/design");
        assert_eq!(design.skill, "frontend-design");
        assert!(
            design.approval,
            "spec acceptance remains a hard artifact gate"
        );
        assert!(
            state
                .steps
                .iter()
                .any(|step| step.skill == "frontend-review")
        );
        assert!(
            state
                .steps
                .iter()
                .any(|step| step.skill == "frontend-verify")
        );
    }

    /// Reviewer finding: `apply_profile` forced a Design step's approval off
    /// going *to* Frontend but never restored it going back to Standard, so
    /// a `reclassify`/`set_profile` revert could leave the workflow with
    /// Frontend's autonomous-design approval semantics while reporting
    /// Standard. The restore must come from the pack's own authored
    /// default, not merely "leave whatever value is currently set" --
    /// issue #542 chunk 3a: `apply_profile` now always copies `approval`
    /// from whichever `StepV2` (primary or domain variant) is selected,
    /// rather than special-casing Design, so this proves the same
    /// guarantee holds through the pack-driven path.
    #[test]
    fn apply_profile_restores_the_kind_default_design_approval_when_leaving_frontend() {
        let definition =
            crate::commands::workflow::registry::builtin_definition("spike").expect("spike pack");
        let mut steps = materialize_from_definition(
            &definition,
            &low_classification(),
            WorkflowProfile::Standard,
            DeployTier::Development,
            true,
        );
        apply_profile(&definition, WorkflowProfile::Frontend, &mut steps);
        let design = steps
            .iter()
            .find(|step| step.phase == WorkflowPhase::Design)
            .expect("spike has a design step");
        assert!(!design.approval, "Frontend forces design approval off");

        // Simulate approval having drifted from the kind's own default for
        // any reason, so the assertion below proves the Standard branch
        // actively restores it rather than coincidentally leaving it alone.
        for step in &mut steps {
            if step.phase == WorkflowPhase::Design {
                step.approval = true;
            }
        }
        apply_profile(&definition, WorkflowProfile::Standard, &mut steps);
        let design = steps
            .iter()
            .find(|step| step.phase == WorkflowPhase::Design)
            .expect("spike has a design step");
        assert!(
            !design.approval,
            "leaving Frontend must restore the kind's own authored approval default"
        );
    }

    /// Issue #542 review finding 12: `materialize_from_definition` and
    /// `apply_profile` must agree on which phases a domain variant can
    /// never override (`PROFILE_INVARIANT_PHASES`) -- proven directly with a
    /// hand-built fixture carrying a Deploy-phase frontend variant (no real
    /// built-in pack authors one today, which is exactly why the two code
    /// paths could previously drift apart without any existing fixture
    /// catching it: one skipped Intent/Deploy/Delegate/Present explicitly,
    /// the other only "skipped" them by accident, because nothing had ever
    /// authored a variant for them). Both the initial materialize AND a
    /// later reclassify must ignore the variant identically.
    #[test]
    fn a_frontend_variant_deploy_step_is_ignored_by_both_materialize_and_reclassify() {
        use crate::commands::workflow::definition::{
            CompletionContract, EffectClass, EscalateTo, FailurePolicy, GateSpec, Limits,
            PresentAs, StepV2, WorkflowDefinitionV2,
        };
        let definition = WorkflowDefinitionV2 {
            schema_version: crate::commands::workflow::definition::DEFINITION_SCHEMA_VERSION,
            id: "deploy-variant-fixture".into(),
            version: 1,
            title: "Deploy variant fixture".into(),
            description: "Proves a Deploy-phase frontend variant is ignored by both \
                           materialize and reclassify."
                .into(),
            domains: vec![],
            triggers: vec![],
            inputs: vec![],
            outputs: vec![],
            steps: vec![
                StepV2 {
                    id: "only".into(),
                    title: "Deploy".into(),
                    phase: WorkflowPhase::Deploy,
                    skills: vec!["finish-branch".into()],
                    agent_role: None,
                    capabilities: vec![],
                    depends_on: vec![],
                    parallel_group: None,
                    condition: StepCondition::Always,
                    approval: false,
                    artifact: None,
                    max_attempts: 3,
                    effect: EffectClass::None,
                    reason: None,
                    domains: vec![],
                    overrides_step: None,
                },
                StepV2 {
                    id: "only-frontend".into(),
                    title: "Deploy (frontend)".into(),
                    phase: WorkflowPhase::Deploy,
                    skills: vec!["frontend-finish".into()],
                    agent_role: None,
                    capabilities: vec![],
                    depends_on: vec![],
                    parallel_group: None,
                    condition: StepCondition::Always,
                    approval: true,
                    artifact: None,
                    max_attempts: 3,
                    effect: EffectClass::None,
                    reason: None,
                    domains: vec!["frontend".into()],
                    overrides_step: Some("only".into()),
                },
            ],
            gates: GateSpec::default(),
            limits: Limits::default(),
            failure: FailurePolicy {
                escalate_to: EscalateTo::Human,
                retry: false,
            },
            effects: EffectClass::None,
            idempotency: None,
            completion: CompletionContract {
                required_outputs: vec![],
                present_as: PresentAs::Summary,
            },
            presentation: None,
            override_builtin: false,
        };

        // Materializing directly under Frontend must never pick up the
        // Deploy-phase variant.
        let frontend_steps = materialize_from_definition(
            &definition,
            &low_classification(),
            WorkflowProfile::Frontend,
            DeployTier::Development,
            true,
        );
        let deploy = frontend_steps
            .iter()
            .find(|step| step.phase == WorkflowPhase::Deploy)
            .unwrap();
        assert_eq!(deploy.skill, "finish-branch");
        assert!(!deploy.approval);

        // Reclassifying an already-materialized Standard run to Frontend
        // must agree with the initial materialize above, never diverge.
        let mut steps = materialize_from_definition(
            &definition,
            &low_classification(),
            WorkflowProfile::Standard,
            DeployTier::Development,
            true,
        );
        apply_profile(&definition, WorkflowProfile::Frontend, &mut steps);
        let deploy = steps
            .iter()
            .find(|step| step.phase == WorkflowPhase::Deploy)
            .unwrap();
        assert_eq!(
            deploy.skill, "finish-branch",
            "reclassify must also ignore the Deploy-phase variant"
        );
        assert!(!deploy.approval);
    }

    #[test]
    fn gate_off_preserves_general_classification_after_operator_frontend_profile_override() {
        let repo = tempdir().unwrap();
        let state_root = tempdir().unwrap();
        let state_dir = StateDir::from_root(state_root.path().to_path_buf());
        let git = |args: &[&str]| {
            let status = std::process::Command::new("git")
                .args([
                    "-c",
                    "user.email=t@example.com",
                    "-c",
                    "user.name=t",
                    "-c",
                    "commit.gpgsign=false",
                ])
                .args(args)
                .current_dir(repo.path())
                .status()
                .unwrap();
            assert!(status.success());
        };
        git(&["init", "-q"]);
        std::fs::create_dir_all(repo.path().join("src")).unwrap();
        std::fs::write(repo.path().join("src/App.tsx"), "export const App = 1;\n").unwrap();
        git(&["add", "."]);
        git(&["commit", "-q", "-m", "base"]);
        std::fs::write(repo.path().join("src/App.tsx"), "export const App = 2;\n").unwrap();

        let mut state = WorkflowState::start(
            repo.path().to_path_buf(),
            "small change".into(),
            WorkflowKind::Feature,
            None,
            true,
            low_classification(),
        );
        state.set_profile(WorkflowProfile::Frontend);
        state.current_step = state
            .steps
            .iter()
            .position(|step| step.phase == WorkflowPhase::Verify)
            .unwrap();
        let reasons = state.classification.reasons.clone();

        reclassify_at_gate(&state_dir, &mut state, None);

        assert_eq!(state.profile, WorkflowProfile::Frontend);
        assert_eq!(state.classification.work_domain.domain, WorkDomain::General);
        assert_eq!(state.classification.reasons, reasons);
    }

    // -- Issue #542 chunk 3a: v2 materialisation is THE execution path -----

    /// Decision 5's deferred back half: proves the pack-driven pipeline
    /// (`materialize_from_definition` over `packs/*.toml`) produces the
    /// IDENTICAL observable step list -- id, phase, skill, agent, artifact,
    /// approval, max_attempts, and ORDER -- as the deleted-from-production
    /// literal pipeline (`legacy_materialize`, preserved as a `#[cfg(test)]`
    /// oracle), across every kind x profile x complexity x risk x deploy-
    /// tier x brainstorm combination.
    #[test]
    fn converted_packs_materialise_identically_to_the_legacy_literals() {
        use Complexity as C;
        use RiskBand as R;

        let kinds = [
            WorkflowKind::Feature,
            WorkflowKind::Bugfix,
            WorkflowKind::Refactor,
            WorkflowKind::Spike,
            WorkflowKind::Review,
        ];
        let profiles = [WorkflowProfile::Standard, WorkflowProfile::Frontend];
        let complexities = [C::Trivial, C::Bounded, C::Substantial, C::Architectural];
        let risks = [R::Low, R::Medium, R::High, R::Critical];
        let deploy_tiers = [
            DeployTier::Development,
            DeployTier::Staging,
            DeployTier::Production,
        ];
        let brainstorms = [true, false];

        let mut compared = 0usize;
        for kind in kinds {
            let pack = crate::commands::workflow::registry::builtin_definition(kind.as_str())
                .unwrap_or_else(|| panic!("{} has a built-in pack", kind.as_str()));
            for profile in profiles {
                for complexity in complexities {
                    for risk in risks {
                        for deploy_tier in deploy_tiers {
                            for brainstorm in brainstorms {
                                let mut classification = low_classification();
                                classification.complexity = complexity;
                                classification.risk = risk;

                                let legacy = legacy_materialize(
                                    kind,
                                    &classification,
                                    profile,
                                    deploy_tier,
                                    brainstorm,
                                );
                                let converted = materialize_from_definition(
                                    &pack,
                                    &classification,
                                    profile,
                                    deploy_tier,
                                    brainstorm,
                                );

                                // Issue #542 review nit: `condition` joined
                                // the compared tuple too -- the oracle
                                // previously proved the two paths agree on
                                // every OUTPUT field but never on the
                                // condition a later reclassify/resume re-
                                // evaluates a step against.
                                let shape = |steps: &[WorkflowStep]| {
                                    steps
                                        .iter()
                                        .map(|step| {
                                            (
                                                step.id.clone(),
                                                step.phase,
                                                step.skill.clone(),
                                                step.agent.clone(),
                                                step.artifact,
                                                step.approval,
                                                step.max_attempts,
                                                step.condition,
                                            )
                                        })
                                        .collect::<Vec<_>>()
                                };
                                assert_eq!(
                                    shape(&converted),
                                    shape(&legacy),
                                    "{kind:?} profile={profile:?} complexity={complexity:?} \
                                     risk={risk:?} tier={deploy_tier:?} brainstorm={brainstorm}"
                                );
                                compared += 1;
                            }
                        }
                    }
                }
            }
        }
        assert_eq!(compared, 5 * 2 * 4 * 4 * 3 * 2);
    }

    /// Issue #542 chunk 3a decision 2: domain-based skill selection is pack
    /// DATA (`StepV2::domains`/`overrides_step`), not a Rust match keyed on
    /// `WorkflowKind` -- proven with a definition that has no legacy kind
    /// counterpart at all.
    #[test]
    fn frontend_steps_are_selected_by_domain_condition_not_kind() {
        const FIXTURE: &str = r#"
schema_version = 1
id = "custom-fixture"
version = 1
title = "Custom fixture"
description = "A definition with no legacy WorkflowKind counterpart."
effects = "repository"

[[steps]]
id = "work"
title = "Work"
phase = "implement"
skills = ["implement"]
condition = "always"

[[steps]]
id = "work-frontend"
title = "Work (frontend)"
phase = "implement"
skills = ["frontend-implement"]
domains = ["frontend"]
overrides_step = "work"

[failure]
escalate_to = "human"

[completion]
present_as = "summary"
"#;
        let definition: crate::commands::workflow::definition::WorkflowDefinitionV2 =
            toml::from_str(FIXTURE).expect("fixture parses");
        assert!(
            WorkflowKind::from_pack_id(&definition.id).is_none(),
            "the fixture must have no legacy kind counterpart"
        );

        let standard = materialize_from_definition(
            &definition,
            &low_classification(),
            WorkflowProfile::Standard,
            DeployTier::Development,
            false,
        );
        assert_eq!(standard.len(), 1);
        assert_eq!(standard[0].id, "work");
        assert_eq!(standard[0].skill, "implement");

        let frontend = materialize_from_definition(
            &definition,
            &low_classification(),
            WorkflowProfile::Frontend,
            DeployTier::Development,
            false,
        );
        assert_eq!(frontend.len(), 1);
        assert_eq!(
            frontend[0].id, "work",
            "the canonical id must not change with profile"
        );
        assert_eq!(frontend[0].skill, "frontend-implement");
    }

    // -- Issue #542 chunk 3b: selection and adaptive-work ------------------

    /// Architecture decision 2's own acceptance test, restated: a trivial
    /// classification must not receive the plan/validate steps a larger
    /// task would get.
    #[test]
    fn a_trivial_adaptive_task_prunes_to_three_steps() {
        let definition = crate::commands::workflow::registry::builtin_definition("adaptive-work")
            .expect("adaptive-work pack");
        let steps = materialize_from_definition(
            &definition,
            &low_classification(),
            WorkflowProfile::Standard,
            DeployTier::Development,
            false,
        );
        assert_eq!(
            steps
                .iter()
                .map(|step| step.id.as_str())
                .collect::<Vec<_>>(),
            ["understand", "execute", "present"]
        );
    }

    // -- Issue #542 chunk 4: end-to-end fixtures for the first-wave --------
    // -- professional packs -------------------------------------------------

    /// Substantial/High so every conditional step (`devops-ci-cd-change`'s
    /// and `dependency-upgrade`'s intent/plan/review gates) survives
    /// pruning -- a genuinely end-to-end walk, not a lucky trivial one.
    fn full_classification() -> Classification {
        let mut classification = low_classification();
        classification.complexity = Complexity::Substantial;
        classification.risk = RiskBand::High;
        classification
    }

    /// Starts `id` directly from the registry, bypassing the CLI/native-tool
    /// agent-role preflight entirely -- issue #542 chunk 4's packs forward-
    /// reference the #541 agent-role roster (see `registry::tests::
    /// no_builtin_pack_references_an_unknown_skill_or_role`'s own doc
    /// comment), which `WorkflowState::start_from_pack` itself never
    /// validates (only the CLI `Start` handler's explicit, separate check
    /// does), so this is a safe and representative way to exercise the pack
    /// data + materialize/advance pipeline today.
    fn start_pack_fixture(
        repo: &Path,
        id: &str,
        task: &str,
        classification: Classification,
    ) -> WorkflowState {
        let skills = SkillRegistry::load(repo, None, false, false).expect("skills");
        let registry = crate::commands::workflow::registry::WorkflowRegistry::load(
            repo, None, false, false, &skills,
        )
        .expect("registry");
        let pack = registry.get(id).unwrap_or_else(|_| panic!("{id} pack"));
        WorkflowState::start_from_pack(
            repo.to_path_buf(),
            task.to_string(),
            pack,
            None,
            true,
            classification,
        )
    }

    /// Issue #542 review findings 5+6: `registry::tests::no_builtin_pack_
    /// references_an_unknown_skill_or_role` now proves every built-in pack's
    /// `agent_role` resolves against the live `AgentRegistry`, but that is
    /// still only a static cross-check of the id strings. This proves
    /// `workflow start` actually WORKS for every one of them: each pack
    /// starts through the exact same `start_from_pack` path the CLI/native
    /// tool use, and its first materialized step is a real, present step
    /// (never an empty step list, and never a step whose own `agent_role`
    /// -- if any -- fails to resolve through the registry, mirroring the
    /// CLI `Start` handler's own preflight).
    #[test]
    fn every_builtin_pack_starts_and_materialises() {
        let repo = tempdir().unwrap();
        git_init_with_commit(repo.path());
        let skills = SkillRegistry::load(repo.path(), None, false, false).expect("skills");
        let registry = crate::commands::workflow::registry::WorkflowRegistry::load(
            repo.path(),
            None,
            false,
            false,
            &skills,
        )
        .expect("registry");
        let agents = AgentRegistry::load(repo.path(), None, false, false)
            .expect("every built-in agent manifest must load");
        let mut checked = 0usize;
        for pack in registry.list() {
            let state = WorkflowState::start_from_pack(
                repo.path().to_path_buf(),
                format!("exercise {}", pack.definition.id),
                pack,
                None,
                true,
                full_classification(),
            );
            let first = state.current().unwrap_or_else(|| {
                panic!("{}: materialized with no first step", pack.definition.id)
            });
            if let Some(role) = &first.agent {
                assert!(
                    agents.get(role).is_ok(),
                    "{}: first step '{}' references unknown agent role '{}'",
                    pack.definition.id,
                    first.id,
                    role
                );
            }
            checked += 1;
        }
        assert_eq!(
            checked,
            registry.list().count(),
            "every registered built-in pack must have started and materialised"
        );
        assert!(
            checked >= 32,
            "expected the full built-in catalogue, checked {checked}"
        );
    }

    /// Issue #542 review finding 11: `sre-postmortem` has no legacy
    /// `WorkflowKind` counterpart, so `start_from_pack` resolves `kind` to
    /// the harmless `WorkflowKind::Feature` placeholder -- which, before
    /// this fix, fed straight into `default_brainstorm_for_kind` (`true` for
    /// `Feature`) and silently swapped `timeline`'s authored `write-intent`
    /// skill to `brainstorm`, a skill this pack never declares and
    /// `validate()` never required to exist for it. `apply_brainstorm_
    /// selection` now only ever touches a pack whose id maps to a real
    /// legacy `WorkflowKind`.
    #[test]
    fn a_non_legacy_pack_never_gets_the_brainstorm_skill_swap() {
        let repo = tempdir().unwrap();
        git_init_with_commit(repo.path());
        let state = start_pack_fixture(
            repo.path(),
            "sre-postmortem",
            "write up the postmortem for the outage",
            full_classification(),
        );
        assert_eq!(state.current().unwrap().id, "timeline");
        assert_eq!(
            state.current().unwrap().skill,
            "write-intent",
            "a non-legacy pack's intent step must never be swapped to \"brainstorm\""
        );
    }

    fn seed_passing_verification(state_dir: &StateDir, state: &WorkflowState, final_only: bool) {
        let fingerprint = crate::commands::workflow::verification::change_fingerprint(&state.repo)
            .expect("fingerprint");
        let report = crate::commands::workflow::verification::VerificationReport {
            schema_version: crate::commands::workflow::verification::VERIFY_REPORT_SCHEMA_VERSION,
            id: "seeded".into(),
            mode: if final_only {
                crate::commands::workflow::verification::VerificationMode::Final
            } else {
                crate::commands::workflow::verification::VerificationMode::Changed
            },
            source: "configured".into(),
            repo: state.repo.clone(),
            branch: state.branch.clone(),
            head_sha: String::new(),
            change_fingerprint: fingerprint,
            changed_paths: vec![],
            fallback_to_full: false,
            narrowed_to: vec![],
            notes: vec![],
            started_at: 0,
            finished_at: 0,
            checks: vec![crate::commands::workflow::verification::CheckResult {
                id: "unit".into(),
                kind: crate::commands::workflow::verification::CheckKind::Unit,
                command: "true".into(),
                source: crate::commands::workflow::verification::CheckSource::DiscoveredToolchain,
                status: crate::commands::workflow::verification::CheckStatus::Passed,
                exit_code: Some(0),
                duration_ms: 1,
                failure_output: None,
                failure_test_names: Vec::new(),
                inconclusive_reason: None,
            }],
        };
        crate::commands::workflow::verification::save_report(state_dir, &report)
            .expect("save report");
    }

    /// Walks `state` to completion with synthetic evidence: an artifact-
    /// gated step gets real (non-template) content written and approved; a
    /// plain approval gate is approved then advanced past (`approve` only
    /// unblocks a non-artifact gate to `Running`, it does not itself move
    /// past it); a Test/Verify-phase step gets a freshly seeded passing
    /// verification report first; everything else just advances on
    /// `StepOutcome::Success`.
    fn walk_to_completion(state_dir: &StateDir, mut state: WorkflowState) -> WorkflowState {
        loop {
            match state.status {
                WorkflowStatus::Completed | WorkflowStatus::Failed | WorkflowStatus::Closed => {
                    return state;
                }
                WorkflowStatus::AwaitingApproval => {
                    // `approve` either advances PAST an artifact-gated step
                    // (pinning it) or, for a plain `approval = true` gate
                    // with no artifact, only unblocks the CURRENT step to
                    // `Running` without moving past it -- either way, the
                    // next loop iteration re-reads `state.status` fresh and
                    // the `Running` arm below advances past it from there,
                    // so no special-casing is needed here.
                    if let Some(stage) = state.current().and_then(|step| step.artifact) {
                        ensure_current_artifact_template(&state).expect("template");
                        let path = workflow_artifact_path(&state, stage).expect("artifact path");
                        std::fs::write(
                            &path,
                            format!(
                                "# Fixture\n\nSubstantive content for {stage}, not the template.\n"
                            ),
                        )
                        .expect("write artifact");
                    }
                    state = approve(state_dir, state).expect("approve");
                }
                WorkflowStatus::Running => {
                    let phase = state.current().map(|step| step.phase);
                    if matches!(
                        phase,
                        Some(WorkflowPhase::Test) | Some(WorkflowPhase::Verify)
                    ) {
                        seed_passing_verification(
                            state_dir,
                            &state,
                            phase == Some(WorkflowPhase::Verify),
                        );
                    }
                    if phase == Some(WorkflowPhase::Review) {
                        // Issue #187/#484's pre-existing risk-scaled
                        // independent-review requirement -- separate from,
                        // and additional to, this pack's own `gates.
                        // independent_review` metadata. Seeds exactly the
                        // evidence `advance_with_evidence`'s Review-phase
                        // gate demands, against the CURRENT change
                        // fingerprint, so the walk proves the gate is
                        // satisfiable rather than bypassing it.
                        let required =
                            crate::commands::workflow::review::required_independent_reviews_for(
                                &state,
                            );
                        if required > 0 {
                            let fingerprint =
                                crate::commands::workflow::verification::change_fingerprint(
                                    &state.repo,
                                )
                                .expect("fingerprint");
                            for round in 0..required {
                                state.review_evidence.push(
                                    crate::commands::workflow::review::ReviewRunEvidence {
                                        id: format!("fixture-review-{round}"),
                                        change_fingerprint: fingerprint,
                                        adapter: "claude".into(),
                                        review_round: 1,
                                        completed_at: 0,
                                        head_sha: None,
                                        reviewed_tree_sha: None,
                                        finding_dispositions: std::collections::BTreeMap::new(),
                                        jev_dedup_converged_for: None,
                                    },
                                );
                            }
                        }
                    }
                    state =
                        advance_with_evidence(state_dir, state, StepOutcome::Success, None, false)
                            .expect("advance");
                }
            }
        }
    }

    fn assert_artifact_accepted(state: &WorkflowState, stage: ArtifactStage) {
        let record = state
            .artifacts
            .get(stage.key())
            .unwrap_or_else(|| panic!("{stage} has no artifact record"));
        assert!(record.accepted_hash.is_some(), "{stage} was never accepted");
    }

    /// Issue #542 chunk 5: the engine-level fix for a genuinely artifact-less
    /// approval gate, proven directly (not just via the full pack walk)
    /// against `pm-requirements`'s `brief-approval` step. Before the fix,
    /// `approve` set `status = Running` but never moved `current_step` off
    /// the gate (there is no artifact to pin), so the very next
    /// `refresh_deploy_tier` (called at the top of `advance_with_evidence`)
    /// unconditionally re-derived `AwaitingApproval` from that SAME
    /// still-current step's declarative `approval = true`, and
    /// `advance_with_evidence`'s own guard then refused with "current
    /// workflow step is awaiting approval" -- an approval gate with no
    /// artifact could never actually be advanced past. Low-risk
    /// classification keeps the pre-existing independent-review gate out of
    /// the way (0 reviews required), isolating this test to the approval
    /// mechanism itself.
    #[test]
    fn an_approval_gate_without_an_artifact_can_be_approved_and_advanced() {
        let repo = tempdir().unwrap();
        let root = tempdir().unwrap();
        let state_dir = StateDir::from_root(root.path().to_path_buf());
        git_init_with_commit(repo.path());
        let mut state = start_pack_fixture(
            repo.path(),
            "pm-requirements",
            "gather requirements for the export feature",
            low_classification(),
        );

        // Walk past `intake` (artifact-gated) and the two plain steps to
        // reach `brief-approval`, the gate-only approval step under test.
        assert_eq!(state.current().unwrap().id, "intake");
        ensure_current_artifact_template(&state).expect("template");
        let path = workflow_artifact_path(&state, ArtifactStage::Intent).expect("artifact path");
        std::fs::write(&path, "# Fixture\n\nSubstantive intake content.\n").expect("write");
        state = approve(&state_dir, state).expect("approve intake");
        assert_eq!(state.current().unwrap().id, "constraints");
        state = advance_with_evidence(&state_dir, state, StepOutcome::Success, None, false)
            .expect("advance past constraints");
        assert_eq!(state.current().unwrap().id, "acceptance-criteria");
        state = advance_with_evidence(&state_dir, state, StepOutcome::Success, None, false)
            .expect("advance past acceptance-criteria");
        assert_eq!(state.current().unwrap().id, "brief-approval");
        assert_eq!(state.status, WorkflowStatus::AwaitingApproval);
        assert!(state.current().unwrap().artifact.is_none());

        // The gate itself: `approve` must unblock the step without moving
        // past it (no artifact to pin), and the FOLLOWING `advance_with_
        // evidence` must actually be able to advance past it -- this is the
        // exact sequence that used to fail.
        state = approve(&state_dir, state).expect("approve the gate-only step");
        assert_eq!(state.status, WorkflowStatus::Running);
        assert_eq!(state.current().unwrap().id, "brief-approval");
        assert_eq!(
            state.current_step_approved.as_deref(),
            Some("brief-approval")
        );

        state = advance_with_evidence(&state_dir, state, StepOutcome::Success, None, false)
            .expect("an approved gate-only step must be advanceable");
        assert_eq!(state.status, WorkflowStatus::Completed);
        assert!(
            state
                .completed_steps
                .iter()
                .any(|id| id == "brief-approval")
        );
    }

    /// Issue #542 review nit: a gate-only approval must emit the same
    /// telemetry event the artifact-gated approval path already does (with
    /// no `artifact_stage`, since there is none), so an operator/dashboard
    /// reading the event stream sees every approval grant, not only the
    /// artifact-gated ones.
    #[test]
    fn a_gate_only_approval_emits_an_artifact_accepted_telemetry_event() {
        let repo = tempdir().unwrap();
        let root = tempdir().unwrap();
        let state_dir = StateDir::from_root(root.path().to_path_buf());
        git_init_with_commit(repo.path());
        let mut state = start_pack_fixture(
            repo.path(),
            "pm-requirements",
            "gather requirements for the export feature",
            low_classification(),
        );
        ensure_current_artifact_template(&state).expect("template");
        let path = workflow_artifact_path(&state, ArtifactStage::Intent).expect("artifact path");
        std::fs::write(&path, "# Fixture\n\nSubstantive intake content.\n").expect("write");
        state = approve(&state_dir, state).expect("approve intake");
        state = advance_with_evidence(&state_dir, state, StepOutcome::Success, None, false)
            .expect("advance past constraints");
        state = advance_with_evidence(&state_dir, state, StepOutcome::Success, None, false)
            .expect("advance past acceptance-criteria");
        assert_eq!(state.current().unwrap().id, "brief-approval");

        state = approve(&state_dir, state).expect("approve the gate-only step");

        let events =
            crate::commands::workflow::telemetry::list(&state_dir, &state.repo).unwrap_or_default();
        assert!(
            events.iter().any(|event| {
                event.kind == crate::commands::workflow::telemetry::TelemetryKind::ArtifactAccepted
                    && event.workflow_id.as_deref() == Some(state.id.as_str())
                    && event.artifact_stage.is_none()
            }),
            "expected an ArtifactAccepted event with no artifact_stage for the gate-only \
             approval: {events:?}"
        );
    }

    /// Issue #542 review finding 14: when an earlier artifact drifts after a
    /// LATER gate-only step has already been approved, `reopen_artifact_
    /// gate`'s rewind must clear `current_step_approved` -- otherwise, once
    /// the run walks forward again, `step_requires_approval` would see the
    /// stale id still recorded and silently treat the gate-only step as
    /// already approved a second time, without a fresh operator decision.
    #[test]
    fn a_stale_gate_only_approval_is_cleared_when_an_earlier_artifact_reopens() {
        let repo = tempdir().unwrap();
        let root = tempdir().unwrap();
        let state_dir = StateDir::from_root(root.path().to_path_buf());
        git_init_with_commit(repo.path());
        let mut state = start_pack_fixture(
            repo.path(),
            "pm-requirements",
            "gather requirements for the export feature",
            low_classification(),
        );

        assert_eq!(state.current().unwrap().id, "intake");
        ensure_current_artifact_template(&state).expect("template");
        let intent_path =
            workflow_artifact_path(&state, ArtifactStage::Intent).expect("artifact path");
        std::fs::write(&intent_path, "# Fixture\n\nSubstantive intake content.\n").expect("write");
        state = approve(&state_dir, state).expect("approve intake");
        state = advance_with_evidence(&state_dir, state, StepOutcome::Success, None, false)
            .expect("advance past constraints");
        state = advance_with_evidence(&state_dir, state, StepOutcome::Success, None, false)
            .expect("advance past acceptance-criteria");
        assert_eq!(state.current().unwrap().id, "brief-approval");

        state = approve(&state_dir, state).expect("approve the gate-only step");
        assert_eq!(
            state.current_step_approved.as_deref(),
            Some("brief-approval")
        );

        // The `intake` artifact drifts after the LATER `brief-approval` gate
        // was already granted.
        std::fs::write(
            &intent_path,
            "# Fixture\n\nChanged after intake was accepted.\n",
        )
        .expect("rewrite");
        let error = advance_with_evidence(&state_dir, state, StepOutcome::Success, None, false)
            .expect_err("a drifted earlier artifact must reopen its gate");
        assert!(
            error.to_string().contains("intent artifact changed"),
            "{error}"
        );

        let reopened = load_active(&state_dir, repo.path()).unwrap().unwrap();
        assert_eq!(reopened.current().unwrap().id, "intake");
        assert_eq!(
            reopened.current_step_approved, None,
            "the stale brief-approval grant must not survive the rewind"
        );
    }

    /// Issue #542 review finding 2: "external effects are metadata only" --
    /// before this fix, a step's own `effect` field was purely descriptive
    /// and the engine never consulted it. This fixture's `second` step is
    /// gated only through `gates.approval` (the definition-level summary),
    /// deliberately leaving the step's own `approval` field `false`, to
    /// prove the ENGINE itself -- not just a pack author remembering to set
    /// `approval = true` -- refuses to enter an unapproved `External`-effect
    /// step.
    #[test]
    fn the_engine_refuses_to_enter_an_unapproved_external_step() {
        use crate::commands::workflow::definition::{
            CompletionContract, EffectClass, EscalateTo, FailurePolicy, GateSpec, Limits,
            PresentAs, StepV2, WorkflowDefinitionV2,
        };
        let repo = tempdir().unwrap();
        let root = tempdir().unwrap();
        let state_dir = StateDir::from_root(root.path().to_path_buf());

        let definition = WorkflowDefinitionV2 {
            schema_version: crate::commands::workflow::definition::DEFINITION_SCHEMA_VERSION,
            id: "external-gate-fixture".into(),
            version: 1,
            title: "External gate fixture".into(),
            description: "Proves the engine gates an External-effect step even without its own \
                           approval field."
                .into(),
            domains: vec![],
            triggers: vec![],
            inputs: vec![],
            outputs: vec![],
            steps: vec![
                StepV2 {
                    id: "first".into(),
                    title: "First".into(),
                    phase: WorkflowPhase::Intent,
                    skills: vec!["write-intent".into()],
                    agent_role: None,
                    capabilities: vec![],
                    depends_on: vec![],
                    parallel_group: None,
                    condition: StepCondition::Always,
                    approval: false,
                    artifact: None,
                    max_attempts: 3,
                    effect: EffectClass::None,
                    reason: None,
                    domains: vec![],
                    overrides_step: None,
                },
                StepV2 {
                    id: "second".into(),
                    title: "Second".into(),
                    phase: WorkflowPhase::Implement,
                    skills: vec!["implement".into()],
                    agent_role: None,
                    capabilities: vec![],
                    depends_on: vec!["first".into()],
                    parallel_group: None,
                    condition: StepCondition::Always,
                    approval: false,
                    artifact: None,
                    max_attempts: 3,
                    effect: EffectClass::External,
                    reason: None,
                    domains: vec![],
                    overrides_step: None,
                },
            ],
            gates: GateSpec {
                approval: vec!["second".into()],
                validation: vec![],
                independent_review: vec![],
            },
            limits: Limits::default(),
            failure: FailurePolicy {
                escalate_to: EscalateTo::Human,
                retry: false,
            },
            effects: EffectClass::External,
            idempotency: None,
            completion: CompletionContract {
                required_outputs: vec![],
                present_as: PresentAs::Summary,
            },
            presentation: None,
            override_builtin: false,
        };
        let known_skills: BTreeSet<&str> = ["write-intent", "implement"].into_iter().collect();
        definition
            .validate(&known_skills)
            .expect("the fixture itself is gated via gates.approval, so validate must accept it");

        let pack = crate::commands::workflow::registry::RegisteredWorkflow {
            definition: definition.clone(),
            source: crate::commands::workflow::registry::WorkflowSource::Repository,
            source_path: None,
            hash: definition.hash().unwrap(),
        };
        let mut state = WorkflowState::start_from_pack(
            repo.path().to_path_buf(),
            "task".into(),
            &pack,
            None,
            true,
            low_classification(),
        );
        assert_eq!(state.current().unwrap().id, "first");
        assert_eq!(state.status, WorkflowStatus::Running);

        state = advance_with_evidence(&state_dir, state, StepOutcome::Success, None, false)
            .expect("advance past the ungated first step");
        assert_eq!(state.current().unwrap().id, "second");
        assert!(
            !state.current().unwrap().approval,
            "the step's own approval field stays false"
        );
        assert_eq!(
            state.status,
            WorkflowStatus::AwaitingApproval,
            "an External-effect step must gate even though its own `approval` field is false"
        );

        // The existing gate-only approval path still grants it.
        state = approve(&state_dir, state).expect("approve the gate-only external step");
        assert_eq!(state.status, WorkflowStatus::Running);
        assert_eq!(state.current().unwrap().id, "second");
        assert_eq!(state.current_step_approved.as_deref(), Some("second"));

        state = advance_with_evidence(&state_dir, state, StepOutcome::Success, None, false)
            .expect("an approved external step must be advanceable");
        assert_eq!(state.status, WorkflowStatus::Completed);
        assert!(state.completed_steps.iter().any(|id| id == "second"));
    }

    #[test]
    fn pm_requirements_end_to_end_walks_to_completion_with_its_artifact() {
        let repo = tempdir().unwrap();
        let root = tempdir().unwrap();
        let state_dir = StateDir::from_root(root.path().to_path_buf());
        git_init_with_commit(repo.path());
        let state = start_pack_fixture(
            repo.path(),
            "pm-requirements",
            "gather requirements for the export feature",
            full_classification(),
        );
        let completed = walk_to_completion(&state_dir, state);
        assert_eq!(completed.status, WorkflowStatus::Completed);
        assert_eq!(
            completed.completed_steps,
            vec![
                "intake",
                "constraints",
                "acceptance-criteria",
                "brief-approval"
            ]
        );
        assert_artifact_accepted(&completed, ArtifactStage::Intent);
        // Issue #542 chunk 5: `brief-approval` is a gate-only approval (no
        // `artifact`) since the engine fix -- see
        // `an_approval_gate_without_an_artifact_can_be_approved_and_advanced`.
        assert!(!completed.artifacts.contains_key(ArtifactStage::Plan.key()));
    }

    #[test]
    fn pm_status_report_end_to_end_walks_to_completion_with_its_artifact() {
        let repo = tempdir().unwrap();
        let root = tempdir().unwrap();
        let state_dir = StateDir::from_root(root.path().to_path_buf());
        let state = start_pack_fixture(
            repo.path(),
            "pm-status-report",
            "sprint status update for leadership",
            full_classification(),
        );
        let completed = walk_to_completion(&state_dir, state);
        assert_eq!(completed.status, WorkflowStatus::Completed);
        assert_eq!(
            completed.completed_steps,
            vec!["source-collection", "variance-analysis", "audience-update"]
        );
        assert_artifact_accepted(&completed, ArtifactStage::Intent);
    }

    #[test]
    fn data_question_to_report_end_to_end_walks_to_completion_with_its_artifact() {
        let repo = tempdir().unwrap();
        let root = tempdir().unwrap();
        let state_dir = StateDir::from_root(root.path().to_path_buf());
        git_init_with_commit(repo.path());
        let state = start_pack_fixture(
            repo.path(),
            "data-question-to-report",
            "what does the data say about signup drop-off",
            full_classification(),
        );
        let completed = walk_to_completion(&state_dir, state);
        assert_eq!(completed.status, WorkflowStatus::Completed);
        assert_eq!(
            completed.completed_steps,
            vec![
                "question-definition",
                "source-audit",
                "analysis",
                "independent-validation",
                "findings-report",
            ]
        );
        assert_artifact_accepted(&completed, ArtifactStage::Intent);
    }

    #[test]
    fn data_quality_investigation_end_to_end_walks_to_completion_with_its_artifact() {
        let repo = tempdir().unwrap();
        let root = tempdir().unwrap();
        let state_dir = StateDir::from_root(root.path().to_path_buf());
        git_init_with_commit(repo.path());
        let state = start_pack_fixture(
            repo.path(),
            "data-quality-investigation",
            "investigate suspicious data quality in the export",
            full_classification(),
        );
        let completed = walk_to_completion(&state_dir, state);
        assert_eq!(completed.status, WorkflowStatus::Completed);
        assert_eq!(
            completed.completed_steps,
            vec!["scope", "investigation", "validation", "findings"]
        );
        assert_artifact_accepted(&completed, ArtifactStage::Intent);
    }

    #[test]
    fn architecture_decision_record_end_to_end_walks_to_completion_with_its_artifacts() {
        let repo = tempdir().unwrap();
        let root = tempdir().unwrap();
        let state_dir = StateDir::from_root(root.path().to_path_buf());
        git_init_with_commit(repo.path());
        let state = start_pack_fixture(
            repo.path(),
            "architecture-decision-record",
            "decide between two architectural options for the queue",
            full_classification(),
        );
        let completed = walk_to_completion(&state_dir, state);
        assert_eq!(completed.status, WorkflowStatus::Completed);
        assert_eq!(
            completed.completed_steps,
            vec!["context", "options", "review", "record"]
        );
        assert_artifact_accepted(&completed, ArtifactStage::Intent);
        assert_artifact_accepted(&completed, ArtifactStage::Spec);
    }

    #[test]
    fn architecture_design_review_end_to_end_walks_to_completion() {
        let repo = tempdir().unwrap();
        let root = tempdir().unwrap();
        let state_dir = StateDir::from_root(root.path().to_path_buf());
        git_init_with_commit(repo.path());
        let state = start_pack_fixture(
            repo.path(),
            "architecture-design-review",
            "review this architecture for the new subsystem",
            full_classification(),
        );
        let completed = walk_to_completion(&state_dir, state);
        assert_eq!(completed.status, WorkflowStatus::Completed);
        assert_eq!(
            completed.completed_steps,
            vec!["intake", "review", "disposition"]
        );
    }

    #[test]
    fn sre_incident_triage_end_to_end_stays_read_only_until_the_mitigation_gate() {
        let repo = tempdir().unwrap();
        let root = tempdir().unwrap();
        let state_dir = StateDir::from_root(root.path().to_path_buf());
        git_init_with_commit(repo.path());
        let state = start_pack_fixture(
            repo.path(),
            "sre-incident-triage",
            "the checkout service is down",
            full_classification(),
        );
        // Every step up to and including the mitigation gate declares
        // `effect = "none"` (the pack's own top-level ceiling) -- read-only
        // diagnosis, never a production mutation, until an operator
        // explicitly approves past the gate.
        for step in &state.steps {
            assert_eq!(
                step.effect,
                crate::commands::workflow::definition::EffectClass::None,
                "step '{}' must stay read-only",
                step.id
            );
        }
        let completed = walk_to_completion(&state_dir, state);
        assert_eq!(completed.status, WorkflowStatus::Completed);
        assert_eq!(
            completed.completed_steps,
            vec!["diagnosis", "root-cause", "mitigation-gate", "report"]
        );
        assert_artifact_accepted(&completed, ArtifactStage::Intent);
        // Issue #542 chunk 5: `mitigation-gate` is a gate-only approval (no
        // `artifact`) since the engine fix -- see
        // `an_approval_gate_without_an_artifact_can_be_approved_and_advanced`.
        assert!(!completed.artifacts.contains_key(ArtifactStage::Plan.key()));
    }

    #[test]
    fn devops_ci_cd_change_end_to_end_walks_to_completion_with_its_artifacts() {
        let repo = tempdir().unwrap();
        let root = tempdir().unwrap();
        let state_dir = StateDir::from_root(root.path().to_path_buf());
        git_init_with_commit(repo.path());
        let state = start_pack_fixture(
            repo.path(),
            "devops-ci-cd-change",
            "change the deployment pipeline stages",
            full_classification(),
        );
        let completed = walk_to_completion(&state_dir, state);
        assert_eq!(completed.status, WorkflowStatus::Completed);
        assert_eq!(
            completed.completed_steps,
            vec![
                "intent",
                "plan",
                "implement",
                "test",
                "simplify",
                "review",
                "verify",
                "deploy"
            ]
        );
        assert_artifact_accepted(&completed, ArtifactStage::Intent);
        assert_artifact_accepted(&completed, ArtifactStage::Plan);
    }

    #[test]
    fn dependency_upgrade_end_to_end_walks_to_completion_with_its_artifact() {
        let repo = tempdir().unwrap();
        let root = tempdir().unwrap();
        let state_dir = StateDir::from_root(root.path().to_path_buf());
        git_init_with_commit(repo.path());
        let state = start_pack_fixture(
            repo.path(),
            "dependency-upgrade",
            "bump the major version of the http client",
            full_classification(),
        );
        let completed = walk_to_completion(&state_dir, state);
        assert_eq!(completed.status, WorkflowStatus::Completed);
        assert_eq!(
            completed.completed_steps,
            vec![
                "scope",
                "implement",
                "test",
                "simplify",
                "review",
                "verify",
                "deploy"
            ]
        );
        assert_artifact_accepted(&completed, ArtifactStage::Intent);
    }

    #[test]
    fn security_remediation_end_to_end_never_skips_review_or_deploy_approval() {
        let repo = tempdir().unwrap();
        let root = tempdir().unwrap();
        let state_dir = StateDir::from_root(root.path().to_path_buf());
        git_init_with_commit(repo.path());
        // Trivial/Low -- unlike every other conditional pack, security
        // remediation's intent/review/deploy gates are all `condition =
        // "always"`, proportionality never drops them.
        let state = start_pack_fixture(
            repo.path(),
            "security-remediation",
            "patch the reported CVE in the parser",
            low_classification(),
        );
        assert!(
            state.steps.iter().any(
                |step| step.id == "review" && step.agent.as_deref() == Some("security-scanner")
            ),
            "the review step must carry the security-scanner seat"
        );
        let completed = walk_to_completion(&state_dir, state);
        assert_eq!(completed.status, WorkflowStatus::Completed);
        assert_eq!(
            completed.completed_steps,
            vec![
                "intent",
                "implement",
                "test",
                "simplify",
                "review",
                "verify",
                "deploy"
            ]
        );
        assert_artifact_accepted(&completed, ArtifactStage::Intent);
    }

    // -- issue #542 chunk 5: the remaining catalogue --------------------

    #[test]
    fn pm_backlog_triage_end_to_end_walks_to_completion_with_its_artifact() {
        let repo = tempdir().unwrap();
        let root = tempdir().unwrap();
        let state_dir = StateDir::from_root(root.path().to_path_buf());
        git_init_with_commit(repo.path());
        let state = start_pack_fixture(
            repo.path(),
            "pm-backlog-triage",
            "triage the backlog before the next cycle",
            full_classification(),
        );
        let completed = walk_to_completion(&state_dir, state);
        assert_eq!(completed.status, WorkflowStatus::Completed);
        assert_eq!(
            completed.completed_steps,
            vec![
                "intake",
                "dedupe-clarify",
                "priority-risk-dependency",
                "backlog-update"
            ]
        );
        assert_artifact_accepted(&completed, ArtifactStage::Intent);
    }

    #[test]
    fn pm_cycle_planning_end_to_end_walks_to_completion_with_its_artifact() {
        let repo = tempdir().unwrap();
        let root = tempdir().unwrap();
        let state_dir = StateDir::from_root(root.path().to_path_buf());
        git_init_with_commit(repo.path());
        let state = start_pack_fixture(
            repo.path(),
            "pm-cycle-planning",
            "plan the next cycle capacity",
            full_classification(),
        );
        let completed = walk_to_completion(&state_dir, state);
        assert_eq!(completed.status, WorkflowStatus::Completed);
        assert_eq!(
            completed.completed_steps,
            vec![
                "capacity-evidence",
                "candidate-scope",
                "dependencies-risks",
                "committed-plan"
            ]
        );
        assert_artifact_accepted(&completed, ArtifactStage::Intent);
        // `committed-plan` is a gate-only approval (no `artifact`) -- see
        // `an_approval_gate_without_an_artifact_can_be_approved_and_advanced`.
        assert!(!completed.artifacts.contains_key(ArtifactStage::Plan.key()));
    }

    #[test]
    fn pm_risk_review_end_to_end_walks_to_completion_with_its_artifact() {
        let repo = tempdir().unwrap();
        let root = tempdir().unwrap();
        let state_dir = StateDir::from_root(root.path().to_path_buf());
        git_init_with_commit(repo.path());
        let state = start_pack_fixture(
            repo.path(),
            "pm-risk-review",
            "review project risks before the release",
            full_classification(),
        );
        let completed = walk_to_completion(&state_dir, state);
        assert_eq!(completed.status, WorkflowStatus::Completed);
        assert_eq!(
            completed.completed_steps,
            vec![
                "identify-risks",
                "assess-risks",
                "mitigation-options",
                "independent-review",
                "register"
            ]
        );
        assert_artifact_accepted(&completed, ArtifactStage::Intent);
    }

    #[test]
    fn pm_retrospective_end_to_end_walks_to_completion_with_its_artifact() {
        let repo = tempdir().unwrap();
        let root = tempdir().unwrap();
        let state_dir = StateDir::from_root(root.path().to_path_buf());
        git_init_with_commit(repo.path());
        let state = start_pack_fixture(
            repo.path(),
            "pm-retrospective",
            "run a retro on the last sprint",
            full_classification(),
        );
        let completed = walk_to_completion(&state_dir, state);
        assert_eq!(completed.status, WorkflowStatus::Completed);
        assert_eq!(
            completed.completed_steps,
            vec![
                "gather-signals",
                "findings",
                "action-items",
                "retro-summary"
            ]
        );
        assert_artifact_accepted(&completed, ArtifactStage::Intent);
    }

    #[test]
    fn data_anomaly_investigation_end_to_end_walks_to_completion_with_its_artifact() {
        let repo = tempdir().unwrap();
        let root = tempdir().unwrap();
        let state_dir = StateDir::from_root(root.path().to_path_buf());
        git_init_with_commit(repo.path());
        let state = start_pack_fixture(
            repo.path(),
            "data-anomaly-investigation",
            "investigate anomaly in the signup metric",
            full_classification(),
        );
        let completed = walk_to_completion(&state_dir, state);
        assert_eq!(completed.status, WorkflowStatus::Completed);
        assert_eq!(
            completed.completed_steps,
            vec![
                "scope",
                "reproduce-evidence",
                "root-cause",
                "independent-validation",
                "findings"
            ]
        );
        assert_artifact_accepted(&completed, ArtifactStage::Intent);
    }

    #[test]
    fn data_recurring_kpi_review_end_to_end_walks_to_completion_with_its_artifact() {
        let repo = tempdir().unwrap();
        let root = tempdir().unwrap();
        let state_dir = StateDir::from_root(root.path().to_path_buf());
        git_init_with_commit(repo.path());
        let state = start_pack_fixture(
            repo.path(),
            "data-recurring-kpi-review",
            "weekly kpi review for the growth team",
            full_classification(),
        );
        let completed = walk_to_completion(&state_dir, state);
        assert_eq!(completed.status, WorkflowStatus::Completed);
        assert_eq!(
            completed.completed_steps,
            vec![
                "collect-metrics",
                "compare-baseline",
                "flag-deviations",
                "kpi-summary"
            ]
        );
        assert_artifact_accepted(&completed, ArtifactStage::Intent);
    }

    #[test]
    fn architecture_discovery_end_to_end_walks_to_completion_with_its_artifact() {
        let repo = tempdir().unwrap();
        let root = tempdir().unwrap();
        let state_dir = StateDir::from_root(root.path().to_path_buf());
        git_init_with_commit(repo.path());
        let state = start_pack_fixture(
            repo.path(),
            "architecture-discovery",
            "map the architecture of the billing subsystem",
            full_classification(),
        );
        let completed = walk_to_completion(&state_dir, state);
        assert_eq!(completed.status, WorkflowStatus::Completed);
        assert_eq!(
            completed.completed_steps,
            vec![
                "scope",
                "inventory",
                "constraints-boundaries",
                "discovery-report"
            ]
        );
        assert_artifact_accepted(&completed, ArtifactStage::Intent);
    }

    #[test]
    fn architecture_migration_roadmap_end_to_end_walks_to_completion_with_its_artifacts() {
        let repo = tempdir().unwrap();
        let root = tempdir().unwrap();
        let state_dir = StateDir::from_root(root.path().to_path_buf());
        git_init_with_commit(repo.path());
        let state = start_pack_fixture(
            repo.path(),
            "architecture-migration-roadmap",
            "plan the migration off the legacy queue",
            full_classification(),
        );
        let completed = walk_to_completion(&state_dir, state);
        assert_eq!(completed.status, WorkflowStatus::Completed);
        assert_eq!(
            completed.completed_steps,
            vec![
                "current-state",
                "target-and-options",
                "phased-plan",
                "review",
                "roadmap"
            ]
        );
        assert_artifact_accepted(&completed, ArtifactStage::Intent);
        assert_artifact_accepted(&completed, ArtifactStage::Spec);
    }

    #[test]
    fn architecture_threat_scale_cost_review_end_to_end_walks_to_completion() {
        let repo = tempdir().unwrap();
        let root = tempdir().unwrap();
        let state_dir = StateDir::from_root(root.path().to_path_buf());
        git_init_with_commit(repo.path());
        let state = start_pack_fixture(
            repo.path(),
            "architecture-threat-scale-cost-review",
            "threat model the new subsystem",
            full_classification(),
        );
        // Both assessments share `parallel_group = "assessment"` and depend
        // only on `intake` -- Kahn's algorithm breaks ties by declaration
        // order, so `threat-assessment` (declared first) precedes
        // `scale-cost-assessment` even though execution is still
        // sequential (issue #542 chunk 3a decision 1: `parallel_group` is
        // informational metadata only).
        assert_eq!(
            state
                .steps
                .iter()
                .map(|s| s.id.as_str())
                .collect::<Vec<_>>(),
            vec![
                "intake",
                "threat-assessment",
                "scale-cost-assessment",
                "review",
                "disposition"
            ]
        );
        let completed = walk_to_completion(&state_dir, state);
        assert_eq!(completed.status, WorkflowStatus::Completed);
        assert_eq!(
            completed.completed_steps,
            vec![
                "intake",
                "threat-assessment",
                "scale-cost-assessment",
                "review",
                "disposition"
            ]
        );
    }

    #[test]
    fn devops_infrastructure_change_end_to_end_walks_to_completion_with_its_artifacts() {
        let repo = tempdir().unwrap();
        let root = tempdir().unwrap();
        let state_dir = StateDir::from_root(root.path().to_path_buf());
        git_init_with_commit(repo.path());
        let state = start_pack_fixture(
            repo.path(),
            "devops-infrastructure-change",
            "provision infrastructure for the new queue",
            full_classification(),
        );
        let completed = walk_to_completion(&state_dir, state);
        assert_eq!(completed.status, WorkflowStatus::Completed);
        assert_eq!(
            completed.completed_steps,
            vec![
                "scope",
                "plan",
                "preconditions",
                "apply-gate",
                "apply-change",
                "receipt"
            ]
        );
        assert_artifact_accepted(&completed, ArtifactStage::Intent);
        assert_artifact_accepted(&completed, ArtifactStage::Plan);
    }

    #[test]
    fn sre_deploy_or_rollback_end_to_end_walks_to_completion_with_its_artifact() {
        let repo = tempdir().unwrap();
        let root = tempdir().unwrap();
        let state_dir = StateDir::from_root(root.path().to_path_buf());
        git_init_with_commit(repo.path());
        let state = start_pack_fixture(
            repo.path(),
            "sre-deploy-or-rollback",
            "should we roll back the checkout service",
            full_classification(),
        );
        let completed = walk_to_completion(&state_dir, state);
        assert_eq!(completed.status, WorkflowStatus::Completed);
        assert_eq!(
            completed.completed_steps,
            vec![
                "assessment",
                "options",
                "decision-gate",
                "execute-decision",
                "receipt"
            ]
        );
        assert_artifact_accepted(&completed, ArtifactStage::Intent);
    }

    /// Issue #542 review finding 13: before this fix, neither
    /// `sre-deploy-or-rollback` nor `devops-infrastructure-change` had ANY
    /// `phase = "deploy"` step, so `apply_deploy_tier` (which only ever acts
    /// on `WorkflowPhase::Deploy` steps) had nothing to widen for either
    /// pack regardless of the operator's deploy-tier policy -- the two packs
    /// the brief specifically singled out for `effects = "external"` were
    /// exactly the two the deploy-tier mechanism could never reach. Each
    /// pack now has an explicit Deploy-phase mutating step
    /// (`execute-decision`/`apply-change`) behind its existing approval
    /// gate, itself also `approval = true` (so it stays gated at every
    /// tier -- `a_pack_authored_approval_gate_survives_a_lower_deploy_tier`
    /// already proves that kind of gate survives `apply_deploy_tier`'s
    /// tier-derived OR), and `apply_deploy_tier` now has a real step to act
    /// on for both.
    #[test]
    fn external_effects_packs_now_carry_a_deploy_phase_step() {
        for (pack_id, deploy_step_id, gate_step_id) in [
            ("devops-infrastructure-change", "apply-change", "apply-gate"),
            (
                "sre-deploy-or-rollback",
                "execute-decision",
                "decision-gate",
            ),
        ] {
            let definition = crate::commands::workflow::registry::builtin_definition(pack_id)
                .unwrap_or_else(|| panic!("{pack_id} pack"));
            let deploy_steps: Vec<_> = definition
                .steps
                .iter()
                .filter(|step| step.phase == WorkflowPhase::Deploy)
                .collect();
            assert_eq!(
                deploy_steps.len(),
                1,
                "{pack_id}: expected exactly one Deploy-phase step, found {deploy_steps:?}"
            );
            let deploy = deploy_steps[0];
            assert_eq!(deploy.id, deploy_step_id);
            assert_eq!(deploy.depends_on, vec![gate_step_id.to_string()]);
            assert!(
                deploy.approval,
                "{pack_id}: the Deploy-phase step must itself stay gated"
            );

            for tier in [
                DeployTier::Development,
                DeployTier::Staging,
                DeployTier::Production,
            ] {
                let materialized = materialize_from_definition(
                    &definition,
                    &full_classification(),
                    WorkflowProfile::Standard,
                    tier,
                    true,
                );
                let materialized_deploy = materialized
                    .iter()
                    .find(|step| step.phase == WorkflowPhase::Deploy)
                    .unwrap_or_else(|| {
                        panic!("{pack_id}: no materialized Deploy step at {tier:?}")
                    });
                assert!(
                    materialized_deploy.approval,
                    "{pack_id}: the Deploy-phase gate must survive every tier ({tier:?})"
                );
            }
        }
    }

    /// Issue #542 chunk 5: `devops-infrastructure-change` and
    /// `sre-deploy-or-rollback` are the two packs that declare `effects =
    /// "external"` (their ceiling for the day #539's typed cloud/deployment-
    /// ops tooling lands). Neither actually mutates anything TODAY -- every
    /// step stays `effect = "none"` (the default), and the workflow stops at
    /// an explicit operator approval whose `reason` names the missing
    /// integration, mirroring `sre-incident-triage`'s own read-only-until-
    /// gate proof from chunk 4.
    #[test]
    fn external_effects_packs_stay_read_only_until_their_gate() {
        for (pack_id, task, gate_step_id) in [
            (
                "devops-infrastructure-change",
                "change the infrastructure config",
                "apply-gate",
            ),
            (
                "sre-deploy-or-rollback",
                "deploy or rollback the checkout service",
                "decision-gate",
            ),
        ] {
            let repo = tempdir().unwrap();
            git_init_with_commit(repo.path());
            let state = start_pack_fixture(repo.path(), pack_id, task, full_classification());
            for step in &state.steps {
                assert_eq!(
                    step.effect,
                    crate::commands::workflow::definition::EffectClass::None,
                    "{pack_id}: step '{}' must stay read-only today",
                    step.id
                );
            }
            let definition = crate::commands::workflow::registry::builtin_definition(pack_id)
                .unwrap_or_else(|| panic!("{pack_id} pack"));
            assert_eq!(
                definition.effects,
                crate::commands::workflow::definition::EffectClass::External
            );
            let gate = definition
                .steps
                .iter()
                .find(|step| step.id == gate_step_id)
                .unwrap_or_else(|| panic!("{pack_id}: step '{gate_step_id}'"));
            assert!(gate.approval, "{pack_id}: '{gate_step_id}' must gate");
            assert!(
                gate.reason
                    .as_deref()
                    .unwrap_or("")
                    .to_lowercase()
                    .contains("#539"),
                "{pack_id}: '{gate_step_id}' reason must name the missing integration: {:?}",
                gate.reason
            );
        }
    }

    #[test]
    fn sre_postmortem_end_to_end_walks_to_completion_with_its_artifact() {
        let repo = tempdir().unwrap();
        let root = tempdir().unwrap();
        let state_dir = StateDir::from_root(root.path().to_path_buf());
        git_init_with_commit(repo.path());
        let state = start_pack_fixture(
            repo.path(),
            "sre-postmortem",
            "write a postmortem for the checkout outage",
            full_classification(),
        );
        let completed = walk_to_completion(&state_dir, state);
        assert_eq!(completed.status, WorkflowStatus::Completed);
        assert_eq!(
            completed.completed_steps,
            vec![
                "timeline",
                "root-cause",
                "contributing-factors",
                "corrective-actions",
                "independent-review",
                "postmortem"
            ]
        );
        assert_artifact_accepted(&completed, ArtifactStage::Intent);
    }

    #[test]
    fn sre_capacity_reliability_review_end_to_end_walks_to_completion_with_its_artifact() {
        let repo = tempdir().unwrap();
        let root = tempdir().unwrap();
        let state_dir = StateDir::from_root(root.path().to_path_buf());
        git_init_with_commit(repo.path());
        let state = start_pack_fixture(
            repo.path(),
            "sre-capacity-reliability-review",
            "capacity review for the checkout service",
            full_classification(),
        );
        let completed = walk_to_completion(&state_dir, state);
        assert_eq!(completed.status, WorkflowStatus::Completed);
        assert_eq!(
            completed.completed_steps,
            vec![
                "collect-metrics",
                "assess-risk",
                "recommendations",
                "review-summary"
            ]
        );
        assert_artifact_accepted(&completed, ArtifactStage::Intent);
    }

    #[test]
    fn schema_data_migration_end_to_end_walks_to_completion_with_its_artifacts() {
        let repo = tempdir().unwrap();
        let root = tempdir().unwrap();
        let state_dir = StateDir::from_root(root.path().to_path_buf());
        git_init_with_commit(repo.path());
        let state = start_pack_fixture(
            repo.path(),
            "schema-data-migration",
            "migrate the schema for the orders table",
            full_classification(),
        );
        let completed = walk_to_completion(&state_dir, state);
        assert_eq!(completed.status, WorkflowStatus::Completed);
        assert_eq!(
            completed.completed_steps,
            vec![
                "intent",
                "migration-plan",
                "implement",
                "test",
                "simplify",
                "review",
                "verify",
                "deploy"
            ]
        );
        assert_artifact_accepted(&completed, ArtifactStage::Intent);
        assert_artifact_accepted(&completed, ArtifactStage::Plan);
    }

    #[test]
    fn performance_investigation_end_to_end_walks_to_completion_with_its_artifact() {
        let repo = tempdir().unwrap();
        let root = tempdir().unwrap();
        let state_dir = StateDir::from_root(root.path().to_path_buf());
        git_init_with_commit(repo.path());
        let state = start_pack_fixture(
            repo.path(),
            "performance-investigation",
            "investigate performance regression in the checkout path",
            full_classification(),
        );
        let completed = walk_to_completion(&state_dir, state);
        assert_eq!(completed.status, WorkflowStatus::Completed);
        assert_eq!(
            completed.completed_steps,
            vec![
                "profile",
                "root-cause",
                "implement",
                "test",
                "simplify",
                "review",
                "verify",
                "deploy"
            ]
        );
        assert_artifact_accepted(&completed, ArtifactStage::Intent);
    }

    #[test]
    fn documentation_runbook_change_end_to_end_walks_to_completion_with_its_artifact() {
        let repo = tempdir().unwrap();
        let root = tempdir().unwrap();
        let state_dir = StateDir::from_root(root.path().to_path_buf());
        git_init_with_commit(repo.path());
        let state = start_pack_fixture(
            repo.path(),
            "documentation-runbook-change",
            "update the runbook for the checkout on-call",
            full_classification(),
        );
        let completed = walk_to_completion(&state_dir, state);
        assert_eq!(completed.status, WorkflowStatus::Completed);
        assert_eq!(
            completed.completed_steps,
            vec!["intent", "implement", "review", "verify", "deploy"]
        );
        assert_artifact_accepted(&completed, ArtifactStage::Intent);
    }
}
