//! Versioned workflow definitions and durable execution state.

use std::collections::{BTreeMap, BTreeSet};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use clap::{Args, Subcommand, ValueEnum};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::agents::AgentRegistry;
use super::classify::{self, Classification, Complexity, Intent, RiskBand, WorkDomain};
use super::deploy::DeployTier;
use super::skill::{SkillRegistry, WorkflowPhase};
use crate::commands::ctx::jev::{self, AnswerValue, Question};
// Only `mod tests` below refers to this module by its bare name (as
// `super::team::X`, where `super` from inside `tests` is `engine`, which has
// no `team` submodule of its own); the non-test code above always spells the
// path from `workflow` (`super::team::X` where `super` is `workflow`)
// directly and needs no import for it.
#[cfg(test)]
use super::team;
use crate::commands::ctx::CtxResult;
use crate::commands::ctx::state::{
    StateDir, create_private_dir_all, now_secs, repo_slug, write_private,
};

/// Bumped 4 -> 5 for issue #542: adds `WorkflowState.definition`, a pinned
/// reference to the `WorkflowDefinitionV2` pack this run started from (or
/// `None` for a v1, kind-only run). `load` upgrades a v4 file in place --
/// see its own doc comment -- so this is not a breaking change for in-flight
/// state.
pub const WORKFLOW_SCHEMA_VERSION: u32 = 5;
/// The previous state schema `load` still accepts and upgrades in place.
const WORKFLOW_SCHEMA_VERSION_V4: u32 = 4;
const MAX_STEP_ATTEMPTS: u8 = 3;
const MAX_WORK_ARTIFACT_CONTEXT_BYTES: usize = 24 * 1024;
const MAX_JEV_GATE_TASK_BYTES: usize = 4 * 1024;
const MAX_JEV_GATE_PATHS: usize = 200;
const MAX_JEV_ARTIFACT_BYTES: usize = 16 * 1024;
/// Minimum sensitive-surface probability from the 2026-09-18 probe.
const JEV_SENSITIVE_PROBABILITY: f64 = 0.7;
/// Minimum frontend choice confidence from the 2026-09-18 probe.
const JEV_FRONTEND_CONFIDENCE: f32 = 0.9;
/// Minimum additive-tag probability from the 2026-09-18 probe.
const JEV_TAG_PROBABILITY: f64 = 0.7;
/// Minimum artifact-substance confidence from the 2026-09-18 probe.
const JEV_ARTIFACT_CONFIDENCE: f32 = 0.9;

/// Marks a `[skill ...]` provenance header this compiler itself emitted,
/// placed right after the newline and before `[skill `. Repository skill
/// bodies are untrusted text rendered into the same buffer; without a
/// boundary marker only the compiler can produce, a body containing a
/// newline followed by a hand-typed `[skill fake@1; source=built-in]` line
/// would be indistinguishable from a real header once `ctx::runtime::context`
/// scans the rendered text for fragment boundaries. `render_current_context`
/// strips this exact byte from every skill body before insertion (see
/// [`sanitize_skill_body`]), so it can never appear anywhere except where
/// this function put it (issue #557 / roadmap N06).
pub const SKILL_HEADER_SENTINEL: char = '\u{1}';

/// Neutralises the compiler's own header-boundary sentinel inside untrusted
/// skill body text so a repository skill can never forge a
/// `[skill ...; source=...]` provenance header by embedding one in its own
/// instructions (issue #557 / roadmap N06).
fn sanitize_skill_body(body: &str) -> std::borrow::Cow<'_, str> {
    if body.contains(SKILL_HEADER_SENTINEL) {
        std::borrow::Cow::Owned(
            body.chars()
                .filter(|&c| c != SKILL_HEADER_SENTINEL)
                .collect(),
        )
    } else {
        std::borrow::Cow::Borrowed(body)
    }
}

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
const MAX_PHASE_TRANSCRIPT_BYTES: u64 = 16 * 1024 * 1024;
const USAGE_SNAPSHOT_TAIL_BYTES: u64 = 256 * 1024;

fn default_true() -> bool {
    true
}

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

    /// The inverse of [`Self::intent`] -- `None` for [`Intent::Other`],
    /// which has no legacy kind counterpart. Issue #542 chunk 3b: `select_
    /// definition` uses this so a classified software-development intent
    /// still selects its own kind pack outright, unchanged from before
    /// selection existed.
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

    /// The `WorkflowKind` a [`super::registry::WorkflowRegistry`] pack id
    /// names, for the five kinds converted to `packs/*.toml` (issue #542).
    /// `None` for any other registry id -- a v2-only definition with no
    /// legacy kind counterpart, which `workflow start` cannot yet execute
    /// (selection/execution of an arbitrary v2 definition is chunk 3).
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
    fn key(self) -> &'static str {
        match self {
            Self::Intent => "intent",
            Self::Spec => "spec",
            Self::Plan => "plan",
        }
    }

    fn file_name(self) -> &'static str {
        match self {
            Self::Intent => "intent.md",
            Self::Spec => "spec.md",
            Self::Plan => "plan.md",
        }
    }

    fn template(self) -> &'static str {
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

/// A pinned reference to the [`super::definition::WorkflowDefinitionV2`]
/// pack a workflow run started from (issue #542, chunks 1+2). Persisted on
/// [`WorkflowState`] so update, resume and rollover cannot change the run's
/// meaning silently: `status` re-resolves `id` against the CURRENT registry
/// and reports drift when `hash` no longer matches, but the run itself keeps
/// executing against whatever this reference (and, for a non-built-in
/// definition, `inline` below) already pinned.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DefinitionRef {
    pub id: String,
    pub version: u32,
    pub hash: String,
    pub source_layer: super::registry::WorkflowSource,
    /// The full pinned definition, stored inline ONLY when `source_layer`
    /// is not `BuiltIn` -- a repository or operator-global pack file can be
    /// edited or deleted out from under a running workflow, so anything but
    /// a built-in (versioned with the zirv binary itself, and therefore
    /// stable for the life of the run) must carry its own copy rather than
    /// trust the registry to still resolve `id` the same way later.
    #[serde(default)]
    pub inline: Option<super::definition::WorkflowDefinitionV2>,
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

/// Whether `condition` admits `classification` -- shared by [`WorkflowStep::
/// applies`] (state already materialized, kept for callers reading a
/// persisted step) and [`super::definition::StepV2`]'s own pruning at
/// materialize time (issue #542 chunk 3a), so the two can never drift.
fn condition_applies(condition: StepCondition, classification: &Classification) -> bool {
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
    /// Issue #542 chunk 3a: carried straight from `StepV2::parallel_group`.
    /// Informational for now -- the state machine is still the single
    /// `current_step` index it always was; a future scheduler can use this
    /// tag to run same-group steps concurrently without a state-shape
    /// change, since it already round-trips through persisted state.
    #[serde(default)]
    pub parallel_group: Option<String>,
    /// Issue #542 chunk 3a: carried straight from `StepV2::effect`.
    #[serde(default)]
    pub effect: super::definition::EffectClass,
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
struct WorkflowDefinition {
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

fn seat_for_phase(phase: WorkflowPhase) -> Option<String> {
    match phase {
        WorkflowPhase::Implement | WorkflowPhase::Debug => Some("implementer".into()),
        WorkflowPhase::Review => Some("reviewer".into()),
        _ => None,
    }
}

fn step(
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
        effect: super::definition::EffectClass::None,
    }
}

/// Only the legacy oracle (`definitions()`) uses this now -- a real pack
/// authors an artifact-gated step directly as `StepV2` TOML/YAML data.
#[cfg(test)]
fn artifact_step(
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
        effect: super::definition::EffectClass::Repository,
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
fn definitions() -> Vec<WorkflowDefinition> {
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
fn definition(kind: WorkflowKind) -> WorkflowDefinition {
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
fn legacy_apply_profile(kind: WorkflowKind, profile: WorkflowProfile, steps: &mut [WorkflowStep]) {
    let defaults = definition(kind).steps;
    for step in steps {
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
fn legacy_materialize(
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
    fn for_classification(classification: &Classification) -> Self {
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

/// Resolves which `StepV2` supplies a step's non-structural data (`skills`,
/// `agent_role`, `capabilities`-derived `effect`, `approval`, `artifact`,
/// `max_attempts`) for `profile` -- issue #542 chunk 3a, replacing the old
/// hardcoded `WorkflowProfile`-keyed Rust match table with pack data. A
/// step's canonical `id`/`phase`/`depends_on`/`parallel_group`/`condition`
/// always come from `primary`, never from a variant: see
/// [`super::definition::StepV2::overrides_step`]'s own doc comment for why.
///
/// `WorkflowProfile::Standard` always resolves to `primary` itself (there is
/// no "standard" domain tag to match); `WorkflowProfile::Frontend` prefers a
/// step with `overrides_step == Some(primary.id)` and `domains` containing
/// `"frontend"`, falling back to `primary` when no such variant exists (a
/// pack with no frontend variant for this step -- for example `WorkflowPhase
/// ::Intent`/`Deploy` -- behaves identically under either profile, matching
/// the old table's explicit `continue` for those phases).
/// Issue #542 review finding 12: the phases a domain variant can never
/// override -- shared by `materialize_from_definition` (the initial build)
/// and `apply_profile` (a later `workflow reclassify`/mid-run Frontend
/// detection re-selection), so the two can never drift apart on which
/// phases are profile-invariant. Before this fix, `materialize_from_
/// definition` skipped no phase at all (relying entirely on no built-in pack
/// happening to author a variant for these phases, an implicit invariant),
/// while `apply_profile` hardcoded the identical-looking list separately;
/// a future pack authoring e.g. a Deploy-phase frontend variant would then
/// have made the initial materialize and a later reclassify silently
/// disagree.
const PROFILE_INVARIANT_PHASES: [WorkflowPhase; 4] = [
    WorkflowPhase::Intent,
    WorkflowPhase::Deploy,
    WorkflowPhase::Delegate,
    WorkflowPhase::Present,
];

fn select_step_data<'a>(
    definition: &'a super::definition::WorkflowDefinitionV2,
    primary: &'a super::definition::StepV2,
    profile: WorkflowProfile,
) -> &'a super::definition::StepV2 {
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

/// Re-selects every already-materialized step's profile-specific data
/// in place, without disturbing `id`/`phase`/order/completed-step tracking
/// -- issue #542 chunk 3a. Used by `WorkflowState::set_profile`/`workflow
/// reclassify` and by automatic mid-run Frontend detection
/// (`reclassify_at_gate`) to relabel an IN-PROGRESS step list. A step whose
/// id no longer names a primary step in `definition` (should not happen for
/// a definition resolved by `resolve_definition_for_state`, but a defensive
/// no-op rather than a panic if it ever does) is left untouched.
fn apply_profile(
    definition: &super::definition::WorkflowDefinitionV2,
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

/// Default intent-step skill per kind, absent a `--brainstorm`/
/// `--no-brainstorm` override: on for exploratory Feature/Spike, off for
/// Bugfix/Refactor's autonomous default. `Review` has no intent step.
/// `WorkflowKind` remains the legacy id set (issue #542 chunk 3a decision
/// 3): a registry id with no legacy kind counterpart has no per-kind
/// default here, so its caller (`workflow start`'s CLI handler) falls back
/// to the autonomous default instead of calling this.
fn default_brainstorm_for_kind(kind: WorkflowKind) -> bool {
    matches!(kind, WorkflowKind::Feature | WorkflowKind::Spike)
}

/// Selects the intent step's skill, same shape as `apply_profile`. Keyed on
/// `WorkflowPhase`, but -- issue #542 review finding 11 -- gated on
/// `legacy_eligible` (whether this run's pack is one of the five legacy kind
/// ids: `WorkflowKind::from_pack_id` resolves it): the "brainstorm" vs.
/// "write-intent" toggle is a legacy Feature/Bugfix/Refactor/Spike/Review
/// concept, not a general one. Before this fix, `start_from_pack`'s harmless
/// `WorkflowKind::Feature` placeholder for a pack with no legacy counterpart
/// fed straight into `default_brainstorm_for_kind`, which returns `true` for
/// `Feature` -- so EVERY non-legacy pack (all thirty-plus chunk-4/5
/// professional packs, none of which ever declares a `brainstorm` skill)
/// silently had its authored `write-intent` intent step swapped to
/// `brainstorm` at start, a skill the pack author never chose and the pack's
/// own `validate()` never even required to exist for it. `legacy_eligible ==
/// false` now leaves every non-legacy pack's intent step exactly as its
/// definition authored it, regardless of the resolved `brainstorm` bool.
fn apply_brainstorm_selection(brainstorm: bool, legacy_eligible: bool, steps: &mut [WorkflowStep]) {
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

/// Already keyed on `WorkflowPhase`, never on `WorkflowKind` or any pack id
/// (issue #542 chunk 3a decision 2): any pack's Deploy/Verify-phase steps
/// work with this unchanged. The synthetic Review step it may insert is a
/// fixed production-readiness safety net, not pack-authored data.
fn apply_deploy_tier(tier: DeployTier, steps: &mut Vec<WorkflowStep>) {
    if tier == DeployTier::Production
        && !steps.iter().any(|step| step.phase == WorkflowPhase::Review)
        && let Some(verify_index) = steps
            .iter()
            .position(|step| step.phase == WorkflowPhase::Verify)
    {
        steps.insert(
            verify_index,
            // Issue #542 review nit: `__review` rather than `review` -- a
            // reserved id `valid_id` itself can never accept for an authored
            // step (it must start with a lowercase letter or digit, never
            // `_`), so this synthetic production-safety step can never
            // collide with a pack-authored step that happens to name itself
            // "review" for some OTHER phase (an authored Review-phase step
            // named "review", like several built-in packs have, is never a
            // collision risk in the first place: the `!steps.iter().any(...
            // WorkflowPhase::Review)` guard above already skips this
            // insertion whenever any Review-phase step already exists).
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
            // Issue #542 review finding 7: this must only ever WIDEN the
            // gate, never clear one the pack itself authored -- a lower
            // deploy tier is not license to silently drop an approval a
            // pack's own Deploy-phase step declared unconditionally.
            step.approval = step.approval || tier >= DeployTier::Staging;
        }
    }
}

/// Builds this run's step list directly from a `WorkflowDefinitionV2` --
/// issue #542 chunk 3a, decision 1: the ONLY step-list construction path a
/// real `zirv workflow start` runs. Prunes by `StepCondition` (unchanged
/// semantics), resolves each surviving step's profile-specific data via
/// [`select_step_data`], orders the result by dependency (`depends_on`, a
/// stable topological sort -- Kahn's algorithm, ties broken by declaration
/// order in the source pack), then applies the brainstorm and deploy-tier
/// overlays exactly as before. A new pack -- including one with a
/// `domains = ["frontend"]` variant step -- needs no Rust code here.
fn materialize_from_definition(
    definition: &super::definition::WorkflowDefinitionV2,
    classification: &Classification,
    profile: WorkflowProfile,
    deploy_tier: DeployTier,
    brainstorm: bool,
) -> Vec<WorkflowStep> {
    let primaries: Vec<&super::definition::StepV2> = definition
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

    let primaries_by_id: BTreeMap<&str, &super::definition::StepV2> = primaries
        .iter()
        .map(|step| (step.id.as_str(), *step))
        .collect();
    let mut steps: Vec<WorkflowStep> = ordered_ids
        .into_iter()
        .map(|id| {
            let primary = primaries_by_id[id];
            // Issue #542 review finding 12: explicit now, not merely
            // implicit in no built-in pack ever authoring a variant for
            // these phases -- see `PROFILE_INVARIANT_PHASES`'s own doc
            // comment.
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

/// The `WorkflowDefinitionV2` a live/persisted `WorkflowState` is actually
/// running against (issue #542 chunk 3a): the pinned inline copy (a
/// non-built-in pack), else the CURRENT built-in pack matching the pinned
/// id, else -- for a v1/schema-4 state with no pin at all -- the built-in
/// pack for `state.kind`. Always resolves to SOME definition: a built-in
/// pack id always parses (`every_builtin_pack_parses_and_validates`), so
/// this never needs to be fallible.
fn resolve_definition_for_state(state: &WorkflowState) -> super::definition::WorkflowDefinitionV2 {
    if let Some(reference) = &state.definition {
        if let Some(inline) = &reference.inline {
            return inline.clone();
        }
        if let Some(builtin) = super::registry::builtin_definition(&reference.id) {
            return builtin;
        }
    }
    super::registry::builtin_definition(state.kind.as_str())
        .expect("every WorkflowKind maps to a built-in pack")
}

/// Resolves the pack `kind` should start from: a live, registry-aware
/// lookup (so an operator's `override = true` global pack, or an enabled
/// repository pack, wins the same way `workflow start <id>` already
/// respects the registry) when one succeeds, else the pure embedded
/// built-in text -- issue #542 chunk 3a. Best-effort by design: an
/// unreadable registry must not block `WorkflowState::start`, which stays
/// infallible for its ~90 non-CLI callers across the codebase (test
/// fixtures in unrelated modules, none of which care about registry
/// overrides).
fn resolve_builtin_or_registry(
    repo: &Path,
    kind: WorkflowKind,
    include_custom_skills: bool,
) -> (
    super::definition::WorkflowDefinitionV2,
    String,
    super::registry::WorkflowSource,
) {
    if let Ok(registry) = load_workflow_registry(repo, !include_custom_skills)
        && let Ok(pack) = registry.get(kind.as_str())
    {
        return (pack.definition.clone(), pack.hash.clone(), pack.source);
    }
    let definition = super::registry::builtin_definition(kind.as_str())
        .expect("every WorkflowKind maps to a built-in pack");
    let hash = definition.hash().expect("built-in pack hashes");
    (definition, hash, super::registry::WorkflowSource::BuiltIn)
}

/// Skill ids that compose one materialized step. The primary step skill stays
/// stable for state/back-compat; substantial implementation additionally
/// receives the resume-safe accepted-plan executor, whose own dependency stack
/// includes worktree isolation and the general implementation discipline.
///
/// Private: `render_current_context` (this module) is its only caller. Issue
/// #539 chunk E2.2 briefly made this `pub(crate)` for a task-matched
/// suggestions layer in `ctx::prompt`; that layer was removed in chunk F
/// (the operator's own design decision: zirv only surfaces which skills
/// exist, via a stable session-wide index, and never pre-selects one for a
/// task), so the cross-module visibility is no longer needed.
fn step_skill_ids(step: &WorkflowStep, classification: &Classification) -> Vec<String> {
    let mut ids = Vec::new();
    if step.phase == WorkflowPhase::Implement
        && classification.complexity >= Complexity::Substantial
    {
        ids.push("execute-plan".to_string());
    }
    ids.push(step.skill.clone());
    ids
}

// `Eq` dropped (issue #542 review nit -- persisting `selection`): `Selection`
// carries an `f64` confidence score, which cannot implement `Eq`; nothing in
// this crate needs `WorkflowState: Eq` (`PartialEq`, used by every existing
// `assert_eq!`/`==` on a `WorkflowState`, is unaffected).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WorkflowState {
    pub schema_version: u32,
    pub id: String,
    pub repo: PathBuf,
    /// The branch this workflow gates -- `--branch` at `start`, or the
    /// checkout's own current branch when not given (empty when neither is
    /// resolvable: a detached HEAD, no commits, or `git` unavailable).
    /// Issue #467: the relatedness key `verification::
    /// latest_is_fresh_and_passing`'s widened sibling-worktree read matches
    /// against a candidate's own recorded `VerificationReport::branch` --
    /// an empty value never matches anything, so an unresolvable branch
    /// safely disables widening rather than matching too broadly.
    /// `#[serde(default)]` for state persisted before this field existed.
    #[serde(default)]
    pub branch: String,
    pub task: String,
    pub kind: WorkflowKind,
    /// Automatically selected methodology overlay. This is derived from the
    /// task and change surface; there is deliberately no initialization flag.
    #[serde(default)]
    pub profile: WorkflowProfile,
    #[serde(default)]
    pub adapter: Option<String>,
    /// Whether operator-global and repository skills may override built-ins.
    /// Persisted so resume/prompt composition cannot silently change the
    /// trust mode selected at workflow start.
    #[serde(default = "default_true")]
    pub include_custom_skills: bool,
    pub classification: Classification,
    #[serde(default)]
    pub deploy_tier: DeployTier,
    pub steps: Vec<WorkflowStep>,
    pub current_step: usize,
    pub completed_steps: Vec<String>,
    pub attempts: BTreeMap<String, u8>,
    /// Wall-clock milliseconds each completed step took, keyed by step id.
    /// Read by `zirv workflow status` to render `completed: intent (2m10s)`.
    #[serde(default)]
    pub step_durations_ms: BTreeMap<String, u64>,
    /// Version-controlled workflow work products. Acceptance authority remains
    /// in this private state: repository markdown is never trusted as config.
    #[serde(default)]
    pub artifacts: BTreeMap<String, WorkflowArtifactRecord>,
    /// Issue #542: the pinned `WorkflowDefinitionV2` pack this run started
    /// from, when the registry had a matching id at `start` time.
    /// `#[serde(default)]` so a v4 state file (pre-dating this field)
    /// deserializes as `None` -- v1, kind-only semantics, unchanged.
    #[serde(default)]
    pub definition: Option<DefinitionRef>,
    #[serde(default)]
    pub review_findings: Vec<super::review::ReviewFinding>,
    #[serde(default)]
    pub review_evidence: Vec<super::review::ReviewRunEvidence>,
    #[serde(default)]
    pub usage_checkpoint: Option<UsageCheckpoint>,
    /// Repository whose frontend the detector/render evidence should scan
    /// instead of `repo`, for workflows tracked in one repository while the
    /// actual frontend under test lives in a sibling checkout. `None` keeps
    /// the historical single-repo behavior of scanning `repo` itself.
    #[serde(default)]
    pub frontend_target_root: Option<PathBuf>,
    /// Whether `profile` came from automatic classification or was later
    /// forced by an operator (`--profile` at start, or `workflow
    /// reclassify`). A state saved before this key existed defaults to
    /// `Classified`, its historical-only behavior.
    #[serde(default)]
    pub profile_source: ProfileSource,
    /// Set once an operator has accepted a workflow's pre-existing (not
    /// newly introduced) blocking frontend findings via
    /// `--accept-preexisting-findings`. Once present, pre-existing blocking
    /// findings stop failing the frontend gate for the rest of this
    /// workflow; newly introduced blocking findings always still fail.
    #[serde(default)]
    pub accepted_preexisting_findings: Option<AcceptedPreexistingFindings>,
    #[serde(default)]
    pub phase_started_at: u64,
    /// Issue #542 chunk 5: the id of the step whose GATE-ONLY (no
    /// `artifact`) approval has already been satisfied, so a subsequent
    /// status recompute over the SAME still-current step does not re-derive
    /// `AwaitingApproval` from that step's declarative `approval = true` a
    /// second time. `approve`'s artifact branch never sets this -- it
    /// advances `current_step` atomically with acceptance instead, so the
    /// next recompute already sees a fresh step. Read only alongside
    /// `step.approval`, via `Self::step_requires_approval`; compared by id
    /// rather than cleared on every `current_step` change, since a step id
    /// is unique within one materialization (validated at registration) so
    /// a stale value can never falsely match a later, different step.
    #[serde(default)]
    pub current_step_approved: Option<String>,
    /// Whether the intent step (when present) uses `brainstorm` (interactive
    /// Q&A) or `write-intent` (autonomous). A state saved before this key
    /// existed defaults to interactive on load.
    #[serde(default = "default_true")]
    pub brainstorm: bool,
    pub status: WorkflowStatus,
    /// Operator-supplied reason recorded by `zirv workflow close --reason`.
    /// A state saved before `close` existed defaults to `None`.
    #[serde(default)]
    pub closed_reason: Option<String>,
    /// When this workflow was closed (`WorkflowStatus::Closed`), `now_secs()`
    /// at that moment. `None` for a workflow never closed.
    #[serde(default)]
    pub closed_at: Option<u64>,
    /// The most recently compiled `zirv workflow team plan` for this
    /// workflow (issue #541). `None` until `team plan` is run against it;
    /// state persisted before this field existed defaults safely to `None`,
    /// same as every other additive field on this struct.
    #[serde(default)]
    pub team_plan: Option<super::team::TeamPlan>,
    /// Issue #542 review nit: the [`super::selection::Selection`] that
    /// chose this run's pack, when `zirv workflow start` (or the native
    /// `workflow_start` tool) picked one deterministically rather than
    /// being given an explicit id -- persisted so `zirv workflow status`
    /// can explain why a pack was chosen without the caller having to
    /// separately re-run `workflow classify` against the same task text.
    /// `None` for an explicit-id start (no selection ever ran) and for
    /// state persisted before this field existed.
    #[serde(default)]
    pub selection: Option<super::selection::Selection>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub jev_tags: Vec<String>,
    pub created_at: u64,
    pub updated_at: u64,
}

impl WorkflowState {
    pub fn current(&self) -> Option<&WorkflowStep> {
        self.steps.get(self.current_step)
    }

    /// Whether `step` (assumed to be the current step) still needs an
    /// operator's approval -- `step.approval` is true, OR the step's own
    /// `effect` is `External` (issue #542 review finding 2: an external
    /// effect is never metadata-only -- the engine itself refuses to enter
    /// an unapproved external-effect step regardless of whether the pack
    /// author also remembered to set `approval = true`, so a definition-
    /// level authoring gap can never let one through) -- AND this exact step
    /// id has not already been approved via the gate-only path recorded in
    /// `current_step_approved` (issue #542 chunk 5). An artifact-gated step
    /// never sets that field, so this is equivalent to plain `step.approval`
    /// for it, unchanged from before this fix.
    fn step_requires_approval(&self, step: &WorkflowStep) -> bool {
        (step.approval || step.effect == super::definition::EffectClass::External)
            && self.current_step_approved.as_deref() != Some(step.id.as_str())
    }

    /// Starts a workflow for one of the five legacy kind ids. Signature
    /// unchanged since before issue #542 (deliberately -- ~90 call sites
    /// across the codebase, mostly unrelated-module test fixtures,
    /// construct a workflow this way); internally now resolves and
    /// materializes from a `WorkflowDefinitionV2` pack (registry-aware,
    /// falling back to the embedded built-in) instead of the deleted
    /// per-kind literal.
    pub(crate) fn start(
        repo: PathBuf,
        task: String,
        kind: WorkflowKind,
        adapter: Option<String>,
        include_custom_skills: bool,
        classification: Classification,
    ) -> Self {
        let (definition, hash, source) =
            resolve_builtin_or_registry(&repo, kind, include_custom_skills);
        Self::start_with_definition(
            repo,
            task,
            kind,
            &definition,
            hash,
            source,
            adapter,
            include_custom_skills,
            classification,
        )
    }

    /// Starts a workflow from an already-resolved registry pack (issue #542
    /// chunk 3a decision 4): any registry id, not just the five legacy
    /// kinds. `pack.definition.id` is mapped back to a legacy `WorkflowKind`
    /// when one exists (`WorkflowKind::from_pack_id`) purely for the
    /// vestigial `kind`/`brainstorm`-default fields old readers still
    /// expect; a pack with no legacy counterpart gets `WorkflowKind::
    /// Feature` as a harmless placeholder -- `state.definition` is the
    /// authoritative record of what is actually running (issue #542 chunk
    /// 3a decision 3: "WorkflowKind remains the legacy id set ... nothing
    /// else keys on it").
    pub(crate) fn start_from_pack(
        repo: PathBuf,
        task: String,
        pack: &super::registry::RegisteredWorkflow,
        adapter: Option<String>,
        include_custom_skills: bool,
        classification: Classification,
    ) -> Self {
        let kind = WorkflowKind::from_pack_id(&pack.definition.id).unwrap_or(WorkflowKind::Feature);
        Self::start_with_definition(
            repo,
            task,
            kind,
            &pack.definition,
            pack.hash.clone(),
            pack.source,
            adapter,
            include_custom_skills,
            classification,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn start_with_definition(
        repo: PathBuf,
        task: String,
        kind: WorkflowKind,
        definition: &super::definition::WorkflowDefinitionV2,
        hash: String,
        source: super::registry::WorkflowSource,
        adapter: Option<String>,
        include_custom_skills: bool,
        classification: Classification,
    ) -> Self {
        let profile = WorkflowProfile::for_classification(&classification);
        let deploy_tier = DeployTier::Development;
        let brainstorm = default_brainstorm_for_kind(kind);
        let steps = materialize_from_definition(
            definition,
            &classification,
            profile,
            deploy_tier,
            brainstorm,
        );
        // Issue #542 review finding 2: mirrors `step_requires_approval` --
        // an `External`-effect first step must gate even when the pack
        // author only listed it in `gates.approval` rather than setting its
        // own `approval = true` (no `current_step_approved` can exist yet
        // for a workflow that has not started).
        let status = if steps.first().is_some_and(|step| {
            step.approval || step.effect == super::definition::EffectClass::External
        }) {
            WorkflowStatus::AwaitingApproval
        } else {
            WorkflowStatus::Running
        };
        let now = now_secs();
        let id = uuid::Uuid::new_v4().to_string();
        let artifacts = initial_artifact_records(&id, &steps);
        Self {
            schema_version: WORKFLOW_SCHEMA_VERSION,
            id,
            repo,
            branch: String::new(),
            task,
            kind,
            profile,
            adapter,
            include_custom_skills,
            classification,
            deploy_tier,
            steps,
            current_step: 0,
            completed_steps: Vec::new(),
            attempts: BTreeMap::new(),
            step_durations_ms: BTreeMap::new(),
            artifacts,
            definition: Some(DefinitionRef {
                id: definition.id.clone(),
                version: definition.version,
                hash,
                source_layer: source,
                inline: (source != super::registry::WorkflowSource::BuiltIn)
                    .then(|| definition.clone()),
            }),
            review_findings: Vec::new(),
            review_evidence: Vec::new(),
            usage_checkpoint: None,
            frontend_target_root: None,
            profile_source: ProfileSource::Classified,
            accepted_preexisting_findings: None,
            phase_started_at: now,
            current_step_approved: None,
            brainstorm,
            status,
            closed_reason: None,
            closed_at: None,
            team_plan: None,
            selection: None,
            jev_tags: Vec::new(),
            created_at: now,
            updated_at: now,
        }
    }

    /// Forces this workflow's methodology overlay to `profile`, marks it an
    /// operator override, and re-runs `apply_profile` over the current step
    /// list so every not-yet-completed step picks up the new profile's
    /// skills. Used by `--profile` at `workflow start` (applied after
    /// classification has already materialized the default steps) and by
    /// `workflow reclassify`.
    pub(crate) fn set_profile(&mut self, profile: WorkflowProfile) {
        self.profile = profile;
        self.profile_source = ProfileSource::OperatorOverride;
        let definition = resolve_definition_for_state(self);
        apply_profile(&definition, profile, &mut self.steps);
    }
}

fn initial_artifact_records(
    workflow_id: &str,
    steps: &[WorkflowStep],
) -> BTreeMap<String, WorkflowArtifactRecord> {
    let mut records = BTreeMap::new();
    for step in steps {
        let Some(stage) = step.artifact else {
            continue;
        };
        records
            .entry(stage.key().to_string())
            .or_insert_with(|| WorkflowArtifactRecord {
                stage,
                rel_path: format!(".zirv/work/{workflow_id}/{}", stage.file_name()),
                accepted_hash: None,
                accepted_at: None,
            });
    }
    records
}

fn sync_artifact_records(state: &mut WorkflowState) {
    for step in &state.steps {
        let Some(stage) = step.artifact else {
            continue;
        };
        state
            .artifacts
            .entry(stage.key().to_string())
            .or_insert_with(|| WorkflowArtifactRecord {
                stage,
                rel_path: format!(".zirv/work/{}/{}", state.id, stage.file_name()),
                accepted_hash: None,
                accepted_at: None,
            });
    }
}

/// Refuses a repo-owned workflow artifact path routed through a symlinked
/// `.zirv/work` directory, workflow directory, or artifact file itself --
/// the same defense `agents::load_dir` and `artifact::register` apply to
/// their own repo-owned surfaces. Checked once at the single choke point
/// every reader/writer/hasher of a workflow artifact goes through
/// (`workflow_artifact_path`), before any create/read/hash touches disk.
///
/// A missing component is not a symlink, so `symlink_metadata` erroring with
/// `NotFound` is treated as "nothing to refuse yet" -- `ensure_current_
/// artifact_template` still needs to be able to create these paths fresh.
fn refuse_symlinked_artifact_path(repo: &Path, workflow_id: &str, path: &Path) -> CtxResult<()> {
    let work_root = repo.join(".zirv").join("work");
    let workflow_dir = work_root.join(workflow_id);
    for candidate in [work_root.as_path(), workflow_dir.as_path(), path] {
        if let Ok(metadata) = std::fs::symlink_metadata(candidate)
            && metadata.file_type().is_symlink()
        {
            return Err(format!(
                "refusing symlinked workflow artifact path '{}'",
                candidate.display()
            )
            .into());
        }
    }
    Ok(())
}

/// Design spec risk-section commitment: a repo that `.gitignore`s `.zirv/`
/// (or `.zirv/work/` specifically) silently loses every work-product
/// artifact a workflow produces -- nothing else in this crate would notice.
/// Best-effort only, mirroring `classify::git_change_input`'s own plain
/// `git -C <repo> ...` shell-out: a missing `git`, a non-repository `repo`,
/// or any other probe failure reads as "not ignored" rather than blocking
/// `workflow start` on an environment problem this warning is not
/// authoritative about anyway.
fn work_dir_is_gitignored(repo: &Path) -> bool {
    std::process::Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["check-ignore", "--quiet", ".zirv/work"])
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

fn workflow_artifact_path(state: &WorkflowState, stage: ArtifactStage) -> CtxResult<PathBuf> {
    let record = state
        .artifacts
        .get(stage.key())
        .ok_or_else(|| format!("workflow '{}' has no {stage} artifact record", state.id))?;
    let expected = format!(".zirv/work/{}/{}", state.id, stage.file_name());
    if record.rel_path != expected {
        return Err(format!(
            "workflow '{}' has invalid {stage} artifact path '{}'",
            state.id, record.rel_path
        )
        .into());
    }
    let path = state.repo.join(&record.rel_path);
    refuse_symlinked_artifact_path(&state.repo, &state.id, &path)?;
    Ok(path)
}

fn ensure_current_artifact_template(state: &WorkflowState) -> CtxResult<()> {
    let Some(stage) = state.current().and_then(|step| step.artifact) else {
        return Ok(());
    };
    let path = workflow_artifact_path(state, stage)?;
    if path.exists() {
        return Ok(());
    }
    let parent = path
        .parent()
        .ok_or("workflow artifact has no parent directory")?;
    std::fs::create_dir_all(parent)?;
    std::fs::write(path, stage.template())?;
    Ok(())
}

fn hash_bytes(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut out = String::with_capacity(digest.len() * 2);
    for byte in digest {
        use std::fmt::Write as _;
        let _ = write!(&mut out, "{byte:02x}");
    }
    out
}

pub(crate) fn artifact_hash(path: &Path) -> CtxResult<String> {
    Ok(hash_bytes(&std::fs::read(path)?))
}

fn rfc3339_now() -> String {
    // UTC conversion using Howard Hinnant's civil-from-days algorithm. Keeping
    // this tiny avoids a date/time dependency solely for an audit timestamp.
    let seconds = i64::try_from(now_secs()).unwrap_or(i64::MAX);
    let days = seconds.div_euclid(86_400);
    let sod = seconds.rem_euclid(86_400);
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 }.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe.div_euclid(1_460) + doe.div_euclid(36_524) - doe.div_euclid(146_096))
        .div_euclid(365);
    let mut year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe.div_euclid(4) - yoe.div_euclid(100));
    let mp = (5 * doy + 2).div_euclid(153);
    let day = doy - (153 * mp + 2).div_euclid(5) + 1;
    let month = mp + if mp < 10 { 3 } else { -9 };
    year += if month <= 2 { 1 } else { 0 };
    let hour = sod.div_euclid(3_600);
    let minute = sod.rem_euclid(3_600).div_euclid(60);
    let second = sod.rem_euclid(60);
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}Z")
}

#[derive(Serialize)]
struct JevArtifactState {
    artifact_kind: ArtifactStage,
    artifact_text: String,
}

fn pin_current_artifact_with_config(
    state_dir: &StateDir,
    state: &mut WorkflowState,
    cfg: Option<&crate::commands::ctx::config::CtxConfig>,
) -> CtxResult<(ArtifactStage, Option<String>)> {
    let stage = state
        .current()
        .and_then(|step| step.artifact)
        .ok_or("current workflow step has no artifact to approve")?;
    ensure_current_artifact_template(state)?;
    let path = workflow_artifact_path(state, stage)?;
    let body = std::fs::read_to_string(&path)?;
    if body.trim() == stage.template().trim() {
        return Err(format!(
            "{stage} artifact is still the untouched template: {}",
            path.display()
        )
        .into());
    }
    let mut warning = None;
    if let Some(cfg) = cfg {
        let advice_state = JevArtifactState {
            artifact_kind: stage,
            artifact_text: crate::utils::truncate_bytes(body.clone(), Some(MAX_JEV_ARTIFACT_BYTES)),
        };
        let questions = [Question::choice(
            "substance",
            "Assess whether this artifact has substantive content for its section headings.",
            &[
                ("template_copy", "the template with only trivial edits"),
                (
                    "thin",
                    "has content but no substance for its section headings",
                ),
                (
                    "substantive",
                    "substantive content for its section headings",
                ),
            ],
        )];
        if let Some(answers) = jev::advise(
            cfg,
            state_dir,
            "workflow-artifact-substance",
            cfg.jev.gates,
            &advice_state,
            &questions,
        ) && let Some(answer) = answers.get("substance")
            && answer.decisive(JEV_ARTIFACT_CONFIDENCE, jev::DEFAULT_MIN_MARGIN)
            && let AnswerValue::Choice(choice) = &answer.value
        {
            match choice.as_str() {
                "template_copy" => {
                    return Err(format!(
                        "{stage} artifact refused by the template_copy advisory at {:.2} confidence: {}",
                        answer.confidence,
                        path.display()
                    )
                    .into());
                }
                "thin" => {
                    warning = Some(format!(
                        "{stage} artifact substance advisory is thin at {:.2} confidence; pinning anyway",
                        answer.confidence
                    ));
                }
                _ => {}
            }
        }
    }
    let hash = hash_bytes(body.as_bytes());
    let record = state
        .artifacts
        .get_mut(stage.key())
        .ok_or("workflow artifact record disappeared")?;
    record.accepted_hash = Some(hash);
    record.accepted_at = Some(rfc3339_now());
    Ok((stage, warning))
}

fn load_workflow_jev_config(repo: &Path) -> Option<crate::commands::ctx::config::CtxConfig> {
    crate::commands::ctx::config::CtxConfig::load(repo, &|key| std::env::var(key).ok()).ok()
}

fn artifact_drift(state: &WorkflowState) -> CtxResult<Option<ArtifactStage>> {
    for stage in [
        ArtifactStage::Intent,
        ArtifactStage::Spec,
        ArtifactStage::Plan,
    ] {
        let Some(record) = state.artifacts.get(stage.key()) else {
            continue;
        };
        let Some(accepted) = record.accepted_hash.as_deref() else {
            continue;
        };
        let path = workflow_artifact_path(state, stage)?;
        if !path.exists() || artifact_hash(&path)? != accepted {
            return Ok(Some(stage));
        }
    }
    Ok(None)
}

fn reopen_artifact_gate(state: &mut WorkflowState, stage: ArtifactStage) -> CtxResult<()> {
    let index = state
        .steps
        .iter()
        .position(|step| step.artifact == Some(stage))
        .ok_or_else(|| {
            format!("accepted {stage} artifact no longer has an owning workflow step")
        })?;
    let invalid: Vec<String> = state.steps[index..]
        .iter()
        .map(|step| step.id.clone())
        .collect();
    state
        .completed_steps
        .retain(|completed| !invalid.contains(completed));
    // Issue #542 review finding 14: a gate-only approval recorded further
    // along the (now rewound) step list must not silently count as still
    // granted if/when this run walks forward past it again -- `invalid`
    // covers exactly the steps this rewind un-completes.
    if state
        .current_step_approved
        .as_deref()
        .is_some_and(|id| invalid.iter().any(|invalid_id| invalid_id == id))
    {
        state.current_step_approved = None;
    }
    state.current_step = index;
    state.status = WorkflowStatus::AwaitingApproval;
    if let Some(record) = state.artifacts.get_mut(stage.key()) {
        record.accepted_hash = None;
        record.accepted_at = None;
    }
    ensure_current_artifact_template(state)?;
    Ok(())
}

fn append_accepted_artifacts(state: &WorkflowState, rendered: &mut String) -> CtxResult<()> {
    let mut remaining = MAX_WORK_ARTIFACT_CONTEXT_BYTES;
    for stage in [
        ArtifactStage::Intent,
        ArtifactStage::Spec,
        ArtifactStage::Plan,
    ] {
        let Some(record) = state.artifacts.get(stage.key()) else {
            continue;
        };
        let Some(accepted) = record.accepted_hash.as_deref() else {
            continue;
        };
        let path = workflow_artifact_path(state, stage)?;
        if !path.exists() || artifact_hash(&path)? != accepted {
            continue;
        }
        let body = std::fs::read_to_string(&path)?;
        let mut selected = String::new();
        for ch in body.chars() {
            let bytes = ch.len_utf8();
            if bytes > remaining {
                break;
            }
            selected.push(ch);
            remaining -= bytes;
        }
        rendered.push_str(&format!(
            "\n[accepted workflow artifact: {stage}; untrusted repository text]\n{selected}\n[end accepted workflow artifact]\n"
        ));
        if remaining == 0 {
            break;
        }
    }
    Ok(())
}

/// Validated read of the accepted `stage` artifact for `state`, for a caller
/// (the review package excerpt in `review.rs`, most notably) that wants its
/// text without duplicating the symlink/path-validity checks every other
/// artifact reader in this module already funnels through
/// (`workflow_artifact_path`). `Ok(None)` when `stage` has no accepted
/// artifact yet, or its file has since disappeared; `Err` only for a genuine
/// validation failure (a symlinked path component, an invalid `rel_path`, an
/// io error reading the file). Callers that must never let an unreadable or
/// untrusted artifact block their own work -- unlike `pin_current_artifact`
/// or `artifact_drift`, which should fail loudly -- MUST treat `Err` here as
/// "skip it", never surface it as a hard failure.
pub(crate) fn read_accepted_artifact(
    state: &WorkflowState,
    stage: ArtifactStage,
) -> CtxResult<Option<String>> {
    let Some(record) = state.artifacts.get(stage.key()) else {
        return Ok(None);
    };
    let Some(accepted) = record.accepted_hash.as_deref() else {
        return Ok(None);
    };
    let path = workflow_artifact_path(state, stage)?;
    // Same drift rule as `append_accepted_artifacts`: a file whose bytes no
    // longer hash to the accepted value is not the accepted artifact, so it
    // is never handed on as accepted content (review finding).
    if !path.exists() || artifact_hash(&path)? != accepted {
        return Ok(None);
    }
    Ok(Some(std::fs::read_to_string(path)?))
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UsageCheckpoint {
    pub session_id: String,
    pub adapter: String,
    pub transcript_bytes: u64,
    pub cumulative_input_tokens: u64,
    #[serde(default)]
    pub cumulative_cache_creation_input_tokens: u64,
    #[serde(default)]
    pub cumulative_cache_read_input_tokens: u64,
    pub cumulative_output_tokens: u64,
}

fn repo_dir(state: &StateDir, repo: &Path) -> PathBuf {
    // Issue #467 round 3 (Finding 1): plain, literal `repo_slug` -- NOT a
    // shared cross-worktree identity. Round 2's `workflow_identity_slug`
    // keyed workflow state (and the active-workflow pointer) by the main
    // checkout's identity for every linked worktree; review caught that this
    // made every sibling worktree of one repository share ONE active
    // pointer, so two unrelated `zirv workflow start` runs in two different
    // worker worktrees clobbered each other. `load`/`load_active` below
    // instead search sibling checkouts explicitly, with fallback rules
    // narrow enough to stay safe (see their own doc comments), while
    // storage itself -- what this function decides -- stays exactly where
    // pre-#467 code put it.
    state.workflows().join(repo_slug(repo))
}

fn state_path(state: &StateDir, repo: &Path, id: &str) -> CtxResult<PathBuf> {
    state_path_in(&repo_dir(state, repo), id)
}

fn state_path_in(dir: &Path, id: &str) -> CtxResult<PathBuf> {
    if !id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') {
        return Err(format!("invalid workflow id '{id}'").into());
    }
    Ok(dir.join(format!("{id}.json")))
}

fn active_path(state: &StateDir, repo: &Path) -> PathBuf {
    repo_dir(state, repo).join("active")
}

fn write_state_file(state_dir: &StateDir, state: &WorkflowState) -> CtxResult<()> {
    let dir = repo_dir(state_dir, &state.repo);
    create_private_dir_all(&dir)?;
    let json = serde_json::to_string_pretty(state)?;
    write_private(&state_path(state_dir, &state.repo, &state.id)?, &json)?;
    Ok(())
}

pub(crate) fn save(state_dir: &StateDir, state: &WorkflowState, active: bool) -> CtxResult<()> {
    write_state_file(state_dir, state)?;
    if active {
        write_private(&active_path(state_dir, &state.repo), &state.id)?;
    } else if active_path(state_dir, &state.repo).exists() {
        std::fs::remove_file(active_path(state_dir, &state.repo))?;
    }
    Ok(())
}

/// Persists `state` without touching the active-workflow pointer either way
/// -- unlike [`save`], whose `active` flag can clear a DIFFERENT workflow's
/// pointer when `false`. Issue #541: `zirv workflow team plan` annotates a
/// (possibly non-active, explicitly `--workflow <id>`-named) workflow with a
/// compiled `TeamPlan` and must never change which workflow is active as a
/// side effect of doing so.
pub(crate) fn save_preserving_active(state_dir: &StateDir, state: &WorkflowState) -> CtxResult<()> {
    write_state_file(state_dir, state)
}

/// Persists `state` and clears this repository's active pointer only when it
/// currently names `state.id` -- unlike `save(state_dir, state, false)`,
/// which clears the pointer unconditionally regardless of which workflow it
/// names. Used by [`close`] so closing an older, non-active workflow never
/// deactivates a different, currently-running workflow for the same repo.
fn save_inactive_if_active(state_dir: &StateDir, state: &WorkflowState) -> CtxResult<()> {
    write_state_file(state_dir, state)?;
    let pointer = active_path(state_dir, &state.repo);
    if pointer.exists() {
        let current = std::fs::read_to_string(&pointer)?;
        if current.trim() == state.id {
            std::fs::remove_file(&pointer)?;
        }
    }
    Ok(())
}

/// Issue #467 round 3 (Finding 1): workflow state itself stays keyed by the
/// LITERAL checkout (see `repo_dir`'s doc comment), but `--repo <path>` on
/// `status|advance|review package <id>` must still find a workflow tracked
/// by a DIFFERENT checkout of the same repository. This checks the literal
/// `repo` first, then every sibling checkout (`pathutil::sibling_checkouts`,
/// in whatever order git reports them) for one holding `id` -- unlike the
/// active-pointer fallback in `load_active`, this is safe to widen to every
/// sibling: an explicit id is never ambiguous the way "whichever pointer
/// happens to be there" is.
fn resolve_state_path_for_id(state: &StateDir, repo: &Path, id: &str) -> CtxResult<PathBuf> {
    let primary = state_path(state, repo, id)?;
    if primary.exists() {
        return Ok(primary);
    }
    let canonical = repo.canonicalize().unwrap_or_else(|_| repo.to_path_buf());
    for sibling in crate::commands::ctx::pathutil::sibling_checkouts(repo) {
        if sibling == canonical {
            continue;
        }
        let candidate = state_path(state, &sibling, id)?;
        if candidate.exists() {
            return Ok(candidate);
        }
    }
    // Let the caller's own `!path.exists()` check produce the domain-shaped
    // "unknown workflow" error uniformly, whether `repo` never had `id` at
    // all or simply is not (and has no sibling that is) a git repository.
    Ok(primary)
}

pub fn load(state: &StateDir, repo: &Path, id: &str) -> CtxResult<WorkflowState> {
    let path = resolve_state_path_for_id(state, repo, id)?;
    let mut value = load_from_path(&path, id)?;
    // Checks must measure the checkout through which the workflow was requested.
    value.repo = repo.to_path_buf();
    Ok(value)
}

fn load_from_path(path: &Path, id: &str) -> CtxResult<WorkflowState> {
    // Every verb that resolves a workflow by id (`status`, `resume`,
    // `context`, `artifacts`, `approve`, `advance`, ...) goes through this
    // one function, so checking here once is enough to keep a bogus id from
    // leaking a raw OS error ("The system cannot find the path specified.
    // (os error 3)") instead of a domain-shaped message.
    if !path.exists() {
        return Err(format!("unknown workflow '{id}'").into());
    }
    let mut value: WorkflowState = serde_json::from_str(&std::fs::read_to_string(path)?)?;
    match value.schema_version {
        version if version == WORKFLOW_SCHEMA_VERSION => {}
        WORKFLOW_SCHEMA_VERSION_V4 => {
            // Issue #542: v4 has no `definition` pin at all -- `#[serde(
            // default)]` already deserialized it as `None` above, kind-only
            // v1 semantics unchanged. Only the version marker itself needs
            // upgrading so a subsequent `save` writes it back as current.
            value.schema_version = WORKFLOW_SCHEMA_VERSION;
        }
        other => {
            return Err(format!(
                "workflow '{}': unsupported state schema {}",
                value.id, other
            )
            .into());
        }
    }
    Ok(value)
}

fn read_active_pointer(state: &StateDir, repo: &Path) -> CtxResult<Option<String>> {
    let path = active_path(state, repo);
    if !path.exists() {
        return Ok(None);
    }
    Ok(Some(std::fs::read_to_string(path)?.trim().to_string()))
}

/// Issue #467 round 3 (Finding 1): the literal checkout's own active-
/// workflow pointer first; if it has none, falls back to the MAIN
/// checkout's own pointer ONLY (`pathutil::worktree_identity`) -- never an
/// arbitrary other sibling. A worker worktree with no workflow of its own
/// (bare `zirv workflow status` run there) inherits the orchestrator's, but
/// two workers each running their own `zirv workflow start` in their own
/// worktrees never collide: neither's pointer is ever mistaken for the
/// other's, since neither is the main checkout. The main checkout itself
/// has no further fallback (its own pointer, or nothing).
pub fn load_active(state: &StateDir, repo: &Path) -> CtxResult<Option<WorkflowState>> {
    if let Some(id) = read_active_pointer(state, repo)? {
        return load(state, repo, &id).map(Some);
    }
    let main = crate::commands::ctx::pathutil::worktree_identity(repo);
    let canonical_repo = repo.canonicalize().unwrap_or_else(|_| repo.to_path_buf());
    if main == canonical_repo {
        return Ok(None);
    }
    match read_active_pointer(state, &main)? {
        Some(id) => load(state, repo, &id).map(Some),
        None => Ok(None),
    }
}

/// Read the requested checkout without migrating state or consulting siblings.
pub(crate) fn load_active_read_only(
    state: &StateDir,
    repo: &Path,
) -> CtxResult<Option<WorkflowState>> {
    let dir = state
        .workflows()
        .join(crate::commands::ctx::state::repo_slug_read_only(repo));
    let pointer = dir.join("active");
    if !pointer.exists() {
        return Ok(None);
    }
    let id = std::fs::read_to_string(pointer)?;
    let id = id.trim();
    load_from_path(&state_path_in(&dir, id)?, id).map(Some)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum StepOutcome {
    Success,
    Failure,
}

#[derive(Debug, Clone, Default)]
pub struct TransitionEvidence {
    pub duration_ms: Option<u64>,
    pub adapter: Option<String>,
    pub model: Option<String>,
    pub role: Option<String>,
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub token_usage_source: Option<String>,
    pub worker_count: u32,
    /// Issue #155, Phase 2: the raw cache classes behind `input_tokens`
    /// (which keeps its existing "combined context size" meaning), and the
    /// same four classes read separately over `isSidechain` rows -- subagent
    /// spend that used to be dropped entirely. `parent_session_id` and
    /// `work_group_id` are NOT here: they stay on `TelemetryEvent` only,
    /// populated by Phase 5, never invented here.
    pub cache_creation_input_tokens: Option<u64>,
    pub cache_read_input_tokens: Option<u64>,
    pub sidechain_input_tokens: Option<u64>,
    pub sidechain_cache_creation_input_tokens: Option<u64>,
    pub sidechain_cache_read_input_tokens: Option<u64>,
    pub sidechain_output_tokens: Option<u64>,
    pub session_id: Option<String>,
    /// Issue #287: set when this `StepOutcome::Failure` is the no-progress
    /// guard's `GateOutcome::Unchanged` -- carried onto the resulting
    /// `TelemetryEvent` so the pathology is distinguishable from a genuinely
    /// re-evaluated failure. Never set for any other transition.
    pub verification_unchanged: bool,
}

fn session_identity() -> Option<(String, String)> {
    let session_id = std::env::var(crate::commands::ctx::adapters::SESSION_ENV).ok()?;
    let adapter = std::env::var(crate::commands::ctx::adapters::AGENT_ENV).ok()?;
    let valid_session = !session_id.is_empty()
        && session_id.len() <= 128
        && session_id
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || character == '-');
    let valid_adapter = !adapter.is_empty()
        && adapter.len() <= 64
        && adapter.chars().all(|character| {
            character.is_ascii_lowercase() || character.is_ascii_digit() || character == '-'
        });
    (valid_session && valid_adapter).then_some((session_id, adapter))
}

/// Issue #349: files one attention observation, under `Authority::Workflow`,
/// against whatever `ZIRV_CTX_SESSION` currently names -- skipped silently
/// when unset (a headless caller with no live session context, or a test),
/// matching every other best-effort write in this codebase. Deliberately
/// reads only `SESSION_ENV`, not the stricter [`session_identity`] (which
/// also requires a valid `AGENT_ENV`): a workflow transition's own attention
/// row must not go unrecorded just because the adapter-name env happens to
/// be unset or malformed.
fn record_workflow_attention(
    attention: crate::commands::ctx::attention::Attention,
    evidence: impl Into<String>,
) {
    let Ok(session_id) = std::env::var(crate::commands::ctx::adapters::SESSION_ENV) else {
        return;
    };
    if session_id.is_empty() {
        return;
    }
    let env = crate::commands::ctx::config::env_from_process();
    let Ok(state) = crate::commands::ctx::state::StateDir::resolve(&env) else {
        return;
    };
    let short = crate::commands::ctx::sessions::short_id(&session_id);
    let now = crate::commands::ctx::state::now_secs();
    let _ = crate::commands::ctx::attention::record(
        &state,
        &short,
        crate::commands::ctx::attention::Observation::new(
            crate::commands::ctx::attention::Authority::Workflow,
            evidence,
            90,
            now,
        )
        .with_attention(attention),
        now,
    );
}

fn adapter_by_name(name: &str) -> Option<Box<dyn crate::commands::ctx::adapters::AgentAdapter>> {
    crate::commands::ctx::adapters::ADAPTERS
        .iter()
        .find(|(adapter_name, _)| *adapter_name == name)
        .map(|(_, constructor)| constructor(None))
}

fn transcript_path(repo: &Path, session_id: &str, adapter: &str) -> Option<PathBuf> {
    let adapter = adapter_by_name(adapter)?;
    Some(
        adapter.transcript_path(&crate::commands::ctx::event::SessionRef {
            id: crate::commands::ctx::event::SessionId::parse(session_id),
            cwd: repo.to_path_buf(),
        }),
    )
}

fn read_transcript_range(path: &Path, start: u64, end: u64) -> Option<String> {
    let length = end.checked_sub(start)?;
    if length > MAX_PHASE_TRANSCRIPT_BYTES {
        return None;
    }
    let mut file = std::fs::File::open(path).ok()?;
    file.seek(SeekFrom::Start(start)).ok()?;
    let mut body = Vec::with_capacity(usize::try_from(length).ok()?);
    file.take(length).read_to_end(&mut body).ok()?;
    Some(String::from_utf8_lossy(&body).into_owned())
}

fn cumulative_snapshot(
    path: &Path,
    transcript_bytes: u64,
    adapter: &dyn crate::commands::ctx::adapters::AgentAdapter,
) -> crate::commands::ctx::event::TranscriptUsage {
    if !adapter.transcript_usage_is_cumulative() || transcript_bytes == 0 {
        return Default::default();
    }
    let start = transcript_bytes.saturating_sub(USAGE_SNAPSHOT_TAIL_BYTES);
    read_transcript_range(path, start, transcript_bytes)
        .as_deref()
        .and_then(|body| adapter.transcript_usage(body))
        .unwrap_or_default()
}

fn usage_checkpoint(repo: &Path) -> Option<UsageCheckpoint> {
    let (session_id, adapter_name) = session_identity()?;
    let adapter = adapter_by_name(&adapter_name)?;
    let path = transcript_path(repo, &session_id, &adapter_name)?;
    let transcript_bytes = std::fs::metadata(&path)
        .map(|metadata| metadata.len())
        .unwrap_or(0);
    let usage = cumulative_snapshot(&path, transcript_bytes, adapter.as_ref());
    Some(UsageCheckpoint {
        session_id,
        adapter: adapter_name,
        transcript_bytes,
        cumulative_input_tokens: usage.input_tokens,
        cumulative_cache_creation_input_tokens: usage.cache_creation_input_tokens,
        cumulative_cache_read_input_tokens: usage.cache_read_input_tokens,
        cumulative_output_tokens: usage.output_tokens,
    })
}

fn usage_since(
    repo: &Path,
    checkpoint: &UsageCheckpoint,
) -> Option<crate::commands::ctx::event::TranscriptUsage> {
    let adapter = adapter_by_name(&checkpoint.adapter)?;
    let path = transcript_path(repo, &checkpoint.session_id, &checkpoint.adapter)?;
    let end = std::fs::metadata(&path).ok()?.len();
    if end < checkpoint.transcript_bytes {
        return None;
    }
    let body = read_transcript_range(&path, checkpoint.transcript_bytes, end)?;
    let usage = adapter.transcript_usage(&body)?;
    if adapter.transcript_usage_is_cumulative() {
        Some(crate::commands::ctx::event::TranscriptUsage {
            input_tokens: usage
                .input_tokens
                .saturating_sub(checkpoint.cumulative_input_tokens),
            cache_creation_input_tokens: usage
                .cache_creation_input_tokens
                .saturating_sub(checkpoint.cumulative_cache_creation_input_tokens),
            cache_read_input_tokens: usage
                .cache_read_input_tokens
                .saturating_sub(checkpoint.cumulative_cache_read_input_tokens),
            output_tokens: usage
                .output_tokens
                .saturating_sub(checkpoint.cumulative_output_tokens),
        })
    } else {
        Some(usage)
    }
}

/// The sidechain-only counterpart to [`usage_since`]. Subagent (`isSidechain`)
/// spend is a Claude Code transcript concept, not a general adapter one, so
/// this reads the claude free function directly rather than growing the
/// `AgentAdapter` trait for a shape only one adapter has. Claude's
/// `transcript_usage` is not cumulative (`transcript_usage_is_cumulative` is
/// unset for it), so sidechain reads need none of `usage_since`'s
/// cumulative-checkpoint subtraction either -- summing the same byte range is
/// exact.
fn sidechain_usage_since(
    repo: &Path,
    checkpoint: &UsageCheckpoint,
) -> Option<crate::commands::ctx::event::TranscriptUsage> {
    if checkpoint.adapter != "claude" {
        return None;
    }
    let path = transcript_path(repo, &checkpoint.session_id, &checkpoint.adapter)?;
    let end = std::fs::metadata(&path).ok()?.len();
    if end < checkpoint.transcript_bytes {
        return None;
    }
    let body = read_transcript_range(&path, checkpoint.transcript_bytes, end)?;
    // The in-file branch is legacy: current Claude Code writes no
    // `isSidechain` rows into the main transcript at all, keeping subagent
    // turns in a sibling `subagents/` directory instead. Preferred when it
    // answers, so a transcript recorded by an older harness still reads
    // exactly as before.
    crate::commands::ctx::adapters::claude::sidechain_transcript_usage(&body)
        .or_else(|| crate::commands::ctx::adapters::claude::subagent_transcript_usage(&path, &body))
}

fn enrich_transition_evidence(
    state: &mut WorkflowState,
    mut evidence: TransitionEvidence,
) -> TransitionEvidence {
    if evidence.duration_ms.is_none() {
        let started = if state.phase_started_at == 0 {
            state.updated_at
        } else {
            state.phase_started_at
        };
        evidence.duration_ms = Some(now_secs().saturating_sub(started).saturating_mul(1000));
    }
    let previous = state.usage_checkpoint.clone();
    let current = usage_checkpoint(&state.repo);
    let mut observed = previous
        .as_ref()
        .and_then(|checkpoint| usage_since(&state.repo, checkpoint));
    let mut sidechain_observed = previous
        .as_ref()
        .and_then(|checkpoint| sidechain_usage_since(&state.repo, checkpoint));
    if let (Some(previous), Some(current)) = (&previous, &current)
        && (previous.session_id != current.session_id || previous.adapter != current.adapter)
    {
        let beginning = UsageCheckpoint {
            transcript_bytes: 0,
            cumulative_input_tokens: 0,
            cumulative_cache_creation_input_tokens: 0,
            cumulative_cache_read_input_tokens: 0,
            cumulative_output_tokens: 0,
            ..current.clone()
        };
        if let Some(next) = usage_since(&state.repo, &beginning) {
            let total = observed.get_or_insert_default();
            total.input_tokens = total.input_tokens.saturating_add(next.input_tokens);
            total.cache_creation_input_tokens = total
                .cache_creation_input_tokens
                .saturating_add(next.cache_creation_input_tokens);
            total.cache_read_input_tokens = total
                .cache_read_input_tokens
                .saturating_add(next.cache_read_input_tokens);
            total.output_tokens = total.output_tokens.saturating_add(next.output_tokens);
        }
        if let Some(next) = sidechain_usage_since(&state.repo, &beginning) {
            let total = sidechain_observed.get_or_insert_default();
            total.input_tokens = total.input_tokens.saturating_add(next.input_tokens);
            total.cache_creation_input_tokens = total
                .cache_creation_input_tokens
                .saturating_add(next.cache_creation_input_tokens);
            total.cache_read_input_tokens = total
                .cache_read_input_tokens
                .saturating_add(next.cache_read_input_tokens);
            total.output_tokens = total.output_tokens.saturating_add(next.output_tokens);
        }
    }
    if let Some(usage) = observed {
        if evidence.input_tokens.is_none() {
            // `context_total()`, not the raw `input_tokens` field: this is
            // the same combined "real context size" number this call site
            // always reported, back when `TranscriptUsage::input_tokens` was
            // the adapter's pre-summed figure. Existing telemetry consumers
            // must keep seeing that value unchanged (issue #155 Phase 2).
            evidence.input_tokens = Some(usage.context_total());
        }
        if evidence.output_tokens.is_none() {
            evidence.output_tokens = Some(usage.output_tokens);
        }
        if evidence.token_usage_source.is_none() {
            evidence.token_usage_source = Some("harness-transcript-delta".into());
        }
        if evidence.cache_creation_input_tokens.is_none() {
            evidence.cache_creation_input_tokens = Some(usage.cache_creation_input_tokens);
        }
        if evidence.cache_read_input_tokens.is_none() {
            evidence.cache_read_input_tokens = Some(usage.cache_read_input_tokens);
        }
    }
    if let Some(usage) = sidechain_observed {
        if evidence.sidechain_input_tokens.is_none() {
            evidence.sidechain_input_tokens = Some(usage.input_tokens);
        }
        if evidence.sidechain_cache_creation_input_tokens.is_none() {
            evidence.sidechain_cache_creation_input_tokens =
                Some(usage.cache_creation_input_tokens);
        }
        if evidence.sidechain_cache_read_input_tokens.is_none() {
            evidence.sidechain_cache_read_input_tokens = Some(usage.cache_read_input_tokens);
        }
        if evidence.sidechain_output_tokens.is_none() {
            evidence.sidechain_output_tokens = Some(usage.output_tokens);
        }
    }
    if evidence.adapter.is_none() {
        evidence.adapter = current
            .as_ref()
            .map(|checkpoint| checkpoint.adapter.clone())
            .or_else(|| state.adapter.clone());
    }
    if evidence.model.is_none() {
        evidence.model = std::env::var(crate::commands::ctx::adapters::SEAT_MODEL_ENV).ok();
    }
    if evidence.session_id.is_none() {
        evidence.session_id = session_identity().map(|(session_id, _)| session_id);
    }
    state.usage_checkpoint = current;
    evidence
}

#[derive(Serialize)]
struct JevGateState {
    task: String,
    changed_paths: Vec<String>,
    current_complexity: Complexity,
    current_risk: RiskBand,
    current_domain: WorkDomain,
}

fn apply_jev_gate_advice(
    cfg: &crate::commands::ctx::config::CtxConfig,
    state_dir: &StateDir,
    state: &mut WorkflowState,
    measured: &mut Classification,
) {
    let current_risk = measured.risk.max(state.classification.risk);
    let current_domain = if state.classification.work_domain.domain == WorkDomain::Frontend {
        WorkDomain::Frontend
    } else {
        measured.work_domain.domain
    };
    let advice_state = JevGateState {
        task: crate::utils::truncate_bytes(state.task.clone(), Some(MAX_JEV_GATE_TASK_BYTES)),
        changed_paths: measured
            .changed_paths
            .iter()
            .take(MAX_JEV_GATE_PATHS)
            .cloned()
            .collect(),
        current_complexity: measured.complexity,
        current_risk,
        current_domain,
    };
    let questions = vec![
        Question::noul(
            "sensitive_surface",
            "Do these paths or this task touch authentication, credentials, permissions, schema or data migration, deployment, or a public API contract?",
            "a sensitive surface is touched",
            "no sensitive surface is touched",
        ),
        Question::choice(
            "work_domain",
            "Which work domain best describes this change?",
            &[
                ("frontend", "frontend user interface work"),
                ("backend", "backend or service work"),
                ("mixed", "both frontend and backend work"),
                ("docs", "documentation-only work"),
            ],
        ),
        Question::noul(
            "security",
            "Is this security work?",
            "security",
            "not security",
        ),
        Question::noul("data", "Is this data work?", "data", "not data"),
        Question::noul(
            "docs_only",
            "Is this documentation-only work?",
            "documentation only",
            "not documentation only",
        ),
        Question::noul("devops", "Is this DevOps work?", "DevOps", "not DevOps"),
        Question::noul(
            "architecture",
            "Is this architecture work?",
            "architecture",
            "not architecture",
        ),
    ];
    let Some(answers) = jev::advise(
        cfg,
        state_dir,
        "workflow-gate-reclassification",
        cfg.jev.gates,
        &advice_state,
        &questions,
    ) else {
        return;
    };
    measured.risk = current_risk;
    if state.classification.work_domain.domain == WorkDomain::Frontend {
        measured.work_domain = state.classification.work_domain.clone();
    }
    // Jev determinism fix: each `is_some_and` below now also requires the
    // answer to be `decisive` (margin at or above `jev::DEFAULT_MIN_MARGIN`);
    // for a noul, that is a margin-only check (no separate confidence on the
    // wire), so `decisive(0.0, ..)` -- the additional condition only ever
    // narrows which answers apply, never widens. No "thin margin at the
    // floor" test exists for `sensitive_surface`/the five tags below: their
    // own floors (`JEV_SENSITIVE_PROBABILITY`/`JEV_TAG_PROBABILITY`, both
    // 0.7) sit far enough from 0.5 that every value clearing them already has
    // margin `>= |0.7 - 0.5| * 2 = 0.4`, well clear of the default -- the
    // gate is real (see `jev::Answer::decisive`'s own tests) but structurally
    // a no-op at this floor, the same reasoning `memory.rs`'s harvest gate
    // documents for its own floor.

    if answers.get("sensitive_surface").is_some_and(|answer| {
        answer.decisive(0.0, jev::DEFAULT_MIN_MARGIN)
            && answer
                .as_noul()
                .is_some_and(|probability| probability >= JEV_SENSITIVE_PROBABILITY)
    }) {
        measured.risk = measured.risk.max(RiskBand::High);
        measured.risk_score = measured.risk_score.max(45);
    }
    if measured.work_domain.domain == WorkDomain::General
        && answers.get("work_domain").is_some_and(|answer| {
            matches!(&answer.value, AnswerValue::Choice(choice) if choice == "frontend")
                && answer.decisive(JEV_FRONTEND_CONFIDENCE, jev::DEFAULT_MIN_MARGIN)
        })
    {
        measured.work_domain.domain = WorkDomain::Frontend;
        measured.work_domain.score = measured.work_domain.score.max(90);
    }
    for (question, tag) in [
        ("security", "security"),
        ("data", "data"),
        ("docs_only", "docs-only"),
        ("devops", "devops"),
        ("architecture", "architecture"),
    ] {
        if answers.get(question).is_some_and(|answer| {
            answer.decisive(0.0, jev::DEFAULT_MIN_MARGIN)
                && answer
                    .as_noul()
                    .is_some_and(|probability| probability >= JEV_TAG_PROBABILITY)
        }) && !state.jev_tags.iter().any(|existing| existing == tag)
        {
            state.jev_tags.push(tag.to_string());
        }
    }
    state.jev_tags.sort();
}

/// Re-measure risk when a workflow reaches a gated step, and never lower it.
///
/// Classification used to be frozen at `workflow start`, which for the common
/// case (start the workflow, then do the work) measured an empty tree: the
/// review step was decided before a single line existed. Re-measuring here
/// means the tree that actually got written is what decides whether review is
/// required. Only Review/Verify steps are added by this path -- a design gate
/// appearing after the implementation is finished would be ceremony, not
/// safety -- and completed steps are never re-run.
///
/// Fails safe, not silently, when Git cannot be measured (not a repository,
/// no commits): the band is escalated one step (`classify::mark_unavailable`)
/// rather than left standing unchallenged -- see the Decision Log entry
/// "Unmeasurable risk fails safe, not open".
fn reclassify_at_gate(
    state_dir: &StateDir,
    state: &mut WorkflowState,
    cfg: Option<&crate::commands::ctx::config::CtxConfig>,
) {
    let Some(step) = state.current().cloned() else {
        return;
    };
    if !matches!(step.phase, WorkflowPhase::Review | WorkflowPhase::Verify) {
        return;
    }
    let measured = classify::git_change_input(&state.repo, state.task.clone())
        .ok()
        .map(|mut input| {
            input.intent_override = Some(state.classification.intent);
            input
        })
        .and_then(|input| classify::classify(&input).ok());
    let Some(mut measured) = measured else {
        let raised = classify::mark_unavailable(
            &mut state.classification,
            format!(
                "git measurement unavailable at step '{}' (not a repository, or no commits)",
                step.id
            ),
        );
        if raised {
            rematerialize_after_risk_increase(state);
        }
        return;
    };
    if let Some(cfg) = cfg {
        apply_jev_gate_advice(cfg, state_dir, state, &mut measured);
    }
    if measured.work_domain.domain == WorkDomain::Frontend
        && state.profile == WorkflowProfile::Standard
    {
        state.profile = WorkflowProfile::Frontend;
        state.classification.work_domain = measured.work_domain.clone();
        let definition = resolve_definition_for_state(state);
        apply_profile(&definition, state.profile, &mut state.steps);
        state.classification.reasons.push(format!(
            "frontend workflow profile selected at step '{}'",
            step.id
        ));
    }
    if measured.risk <= state.classification.risk {
        state.classification.reasons.sort();
        return;
    }
    state.classification.risk = measured.risk;
    state.classification.risk_score = state.classification.risk_score.max(measured.risk_score);
    state.classification.changed_files = measured.changed_files;
    state.classification.changed_lines = measured.changed_lines;
    state.classification.reasons.push(format!(
        "reclassified at step '{}': measured risk {:?}",
        step.id, measured.risk
    ));
    state.classification.reasons.sort();
    rematerialize_after_risk_increase(state);
}

/// Adds any Review/Verify step the just-raised risk band newly requires,
/// without re-running or reordering completed steps. Shared by the measured
/// re-classification above and by the fail-safe escalation applied when Git
/// measurement is unavailable at a gate.
fn rematerialize_after_risk_increase(state: &mut WorkflowState) {
    let definition = resolve_definition_for_state(state);
    let desired = materialize_from_definition(
        &definition,
        &state.classification,
        state.profile,
        state.deploy_tier,
        state.brainstorm,
    );
    let known: Vec<String> = state.steps.iter().map(|step| step.id.clone()).collect();
    let earliest_new = desired.iter().position(|step| {
        !known.contains(&step.id)
            && (step.artifact.is_some()
                || matches!(step.phase, WorkflowPhase::Review | WorkflowPhase::Verify))
    });
    let Some(cutoff) = earliest_new else {
        return;
    };

    // A newly-required artifact can land before already-completed code work.
    // Preserve only evidence that precedes the new gate; later work must be
    // replayed against the newly accepted artifact instead of being blessed
    // retroactively.
    let safe_ids: Vec<String> = desired[..cutoff]
        .iter()
        .map(|step| step.id.clone())
        .collect();
    state
        .completed_steps
        .retain(|completed| safe_ids.contains(completed));
    state.steps = desired;
    sync_artifact_records(state);
    state.current_step = state
        .steps
        .iter()
        .position(|step| !state.completed_steps.contains(&step.id))
        .unwrap_or(state.steps.len());
}

fn apply_effective_deploy_tier(state: &mut WorkflowState, effective: DeployTier) {
    let target = state.deploy_tier.max(effective);
    let definition = resolve_definition_for_state(state);
    let desired = materialize_from_definition(
        &definition,
        &state.classification,
        state.profile,
        target,
        state.brainstorm,
    );

    let known: Vec<String> = state.steps.iter().map(|step| step.id.clone()).collect();
    let earliest_new = desired.iter().position(|step| !known.contains(&step.id));

    if target > state.deploy_tier
        && let Some(cutoff) = earliest_new
    {
        let safe_ids: Vec<String> = desired[..cutoff]
            .iter()
            .map(|step| step.id.clone())
            .collect();
        state
            .completed_steps
            .retain(|completed| safe_ids.contains(completed));
        // Issue #542 review finding 15: a deploy-tier escalation can
        // invalidate steps the same way `reopen_artifact_gate`'s rewind
        // does -- a gate-only approval recorded for a step this escalation
        // just un-completed must not be treated as still granted if this
        // run walks forward past it again.
        if state
            .current_step_approved
            .as_deref()
            .is_some_and(|id| !safe_ids.iter().any(|safe_id| safe_id == id))
        {
            state.current_step_approved = None;
        }
    }

    state.deploy_tier = target;
    state.steps = desired;
    sync_artifact_records(state);
    state.current_step = state
        .steps
        .iter()
        .position(|step| !state.completed_steps.contains(&step.id))
        .unwrap_or(state.steps.len());
    state.status = match state.current() {
        None => WorkflowStatus::Completed,
        Some(step) if state.step_requires_approval(step) => WorkflowStatus::AwaitingApproval,
        Some(_) => WorkflowStatus::Running,
    };
}

fn refresh_deploy_tier(state: &mut WorkflowState) -> CtxResult<()> {
    let effective = super::deploy::effective_tier(&state.repo)?;
    apply_effective_deploy_tier(state, effective);
    Ok(())
}
pub fn advance_with_evidence(
    state_dir: &StateDir,
    mut state: WorkflowState,
    outcome: StepOutcome,
    evidence: Option<&TransitionEvidence>,
    accept_preexisting_findings: bool,
) -> CtxResult<WorkflowState> {
    // Checked against the as-loaded status, before `refresh_deploy_tier`:
    // `apply_effective_deploy_tier` unconditionally recomputes `status` from
    // the current step's position, which would otherwise silently revive a
    // terminal `Closed`/`Failed`/`Completed` workflow back to
    // `Running`/`AwaitingApproval` (mirrors the `Resume` handler's guard).
    // `Running`/`AwaitingApproval` themselves still go through the refresh
    // and artifact-drift checks below, unchanged.
    if !matches!(
        state.status,
        WorkflowStatus::Running | WorkflowStatus::AwaitingApproval
    ) {
        return Err(format!("workflow is {:?}, not running", state.status).into());
    }
    refresh_deploy_tier(&mut state)?;
    if let Some(stage) = artifact_drift(&state)? {
        reopen_artifact_gate(&mut state, stage)?;
        state.updated_at = now_secs();
        save(state_dir, &state, true)?;
        return Err(format!(
            "accepted {stage} artifact changed after approval; review and run `zirv workflow approve {}` again",
            state.id
        )
        .into());
    }
    if state.status == WorkflowStatus::AwaitingApproval {
        return Err("current workflow step is awaiting approval".into());
    }
    if state.status != WorkflowStatus::Running {
        return Err(format!("workflow is {:?}, not running", state.status).into());
    }
    let current = state
        .current()
        .cloned()
        .ok_or("workflow has no current step")?;
    // Issue #699 Phase 0: this attempt's own machine elapsed time, captured
    // by the match arms below (`Some` on both outcomes -- each assigns
    // exactly once, on every path that does not return early) so the
    // `PhaseCompleted`/`PhaseFailed` telemetry event built after the match
    // can carry it automatically -- see `phase_elapsed_ms`'s own doc
    // comment for why the same value is valid for either outcome.
    let auto_duration_ms: Option<u64>;
    match outcome {
        StepOutcome::Success => {
            let frontend_root: PathBuf = state
                .frontend_target_root
                .clone()
                .unwrap_or_else(|| state.repo.clone());
            if state.profile == WorkflowProfile::Frontend
                && matches!(
                    current.phase,
                    WorkflowPhase::Test | WorkflowPhase::Review | WorkflowPhase::Verify
                )
                && !super::frontend_detector::latest_is_fresh_and_passing(
                    state_dir,
                    &frontend_root,
                    matches!(current.phase, WorkflowPhase::Review | WorkflowPhase::Verify),
                )?
            {
                let report = super::frontend_detector::detect_for_workflow(
                    state_dir,
                    &frontend_root,
                    matches!(current.phase, WorkflowPhase::Review | WorkflowPhase::Verify),
                )?;
                let introduced = report.introduced_blocking_count();
                let preexisting = report.preexisting_blocking_count();
                let preexisting_already_accepted = state.accepted_preexisting_findings.is_some();
                let preexisting_accepted =
                    accept_preexisting_findings || preexisting_already_accepted;
                if report.truncated || introduced > 0 || (preexisting > 0 && !preexisting_accepted)
                {
                    let accept_hint = if preexisting > 0 && !preexisting_accepted {
                        format!(
                            "; pass --accept-preexisting-findings to accept {preexisting} pre-existing blocking finding(s) and proceed"
                        )
                    } else {
                        String::new()
                    };
                    return Err(format!(
                        "frontend step '{}' automatically ran the detector against '{}', but evidence did not pass ({} introduced blocking, {} pre-existing blocking, {} files, truncated={}); inspect with `zirv frontend check --all --repo {}`{}, or set `--frontend-root` if the frontend lives in a different repository",
                        current.id,
                        frontend_root.display(),
                        introduced,
                        preexisting,
                        report.analyzed_files.len(),
                        report.truncated,
                        frontend_root.display(),
                        accept_hint
                    )
                    .into());
                }
                if preexisting > 0 && accept_preexisting_findings && !preexisting_already_accepted {
                    state.accepted_preexisting_findings = Some(AcceptedPreexistingFindings {
                        step: current.id.clone(),
                        at: rfc3339_now(),
                        blocking: preexisting,
                        total: report.preexisting_total_count(),
                    });
                    // Persisted immediately, mirroring `--frontend-root`
                    // below: a later gate in this same advance (render/
                    // visual-review, or the general test-evidence gate)
                    // can still fail closed, and the operator should not
                    // have to pass the flag again on retry.
                    state.updated_at = now_secs();
                    save(state_dir, &state, true)?;
                }
            }
            if state.profile == WorkflowProfile::Frontend
                && matches!(current.phase, WorkflowPhase::Review | WorkflowPhase::Verify)
                && !super::frontend_render::latest_visual_is_fresh_and_passing(
                    state_dir,
                    &frontend_root,
                )?
            {
                let render = super::frontend_render::render(state_dir, &frontend_root)?;
                if !render.passed() {
                    return Err(format!(
                        "frontend step '{}' could not collect automatic rendered evidence against '{}': {}; inspect with `zirv frontend render --repo {}`",
                        current.id,
                        frontend_root.display(),
                        render.notes.join("; "),
                        frontend_root.display()
                    )
                    .into());
                }
                let review = super::frontend_render::review(
                    state_dir,
                    &frontend_root,
                    &super::frontend_render::VisualReviewArgs {
                        repo: Some(frontend_root.to_path_buf()),
                        agent: None,
                        model: None,
                        runtime: crate::commands::ctx::runtime::RuntimeKind::Harness
                            .as_str()
                            .to_string(),
                        json: false,
                    },
                )?;
                if review.verdict != super::frontend_render::VisualVerdict::Pass {
                    return Err(format!(
                        "frontend step '{}' failed automatic visual review round {}: {}",
                        current.id,
                        review.review_round,
                        review.findings.join("; ")
                    )
                    .into());
                }
            }
            if current.phase == WorkflowPhase::Deploy {
                let gate = super::deploy::production_gate_satisfied(state_dir, &state);
                let mut event = super::telemetry::TelemetryEvent::new(
                    super::telemetry::TelemetryKind::DeployGateEvaluated,
                );
                event.workflow_id = Some(state.id.clone());
                event.phase = Some(current.phase);
                event.intent = Some(state.classification.intent);
                event.complexity = Some(state.classification.complexity);
                event.risk = Some(state.classification.risk);
                event.work_domain = Some(state.classification.work_domain.domain);
                event.deploy_tier = Some(state.deploy_tier.to_string());
                event.succeeded = Some(gate.is_ok());
                let _ = super::telemetry::record(
                    state_dir,
                    &state.repo,
                    &event,
                    &super::telemetry::TelemetryConfig::for_repo(&state.repo),
                );
                gate?;
            }
            if current.phase == WorkflowPhase::Review {
                if state
                    .review_findings
                    .iter()
                    .any(|finding| finding.disposition == super::review::FindingDisposition::Open)
                {
                    return Err(
                        "review findings must have a final disposition before the review step can pass"
                            .into(),
                    );
                }
                let required = super::review::required_independent_reviews_for(&state);
                if required > 0 {
                    if state.review_evidence.is_empty() {
                        return Err(format!(
                            "review step requires {required} fresh independent review run(s); found 0"
                        )
                        .into());
                    }
                    let fingerprint = super::verification::change_fingerprint(&state.repo)?;
                    let completed = state
                        .review_evidence
                        .iter()
                        .filter(|evidence| evidence.change_fingerprint == fingerprint)
                        .count();
                    if completed < required {
                        return Err(format!(
                            "review step requires {required} fresh independent review run(s); found {completed}"
                        )
                        .into());
                    }
                }
            }
            if matches!(current.phase, WorkflowPhase::Test | WorkflowPhase::Verify) {
                let final_only = current.phase == WorkflowPhase::Verify;
                if !super::verification::latest_is_fresh_and_passing(
                    state_dir,
                    &state.repo,
                    final_only,
                    Some(&state.branch),
                )? {
                    let command = if final_only {
                        "zirv verify"
                    } else {
                        "zirv test changed"
                    };
                    let announcement =
                        super::verification::gate_announcement(state_dir, &state.repo, final_only);
                    record_workflow_attention(
                        crate::commands::ctx::attention::Attention::VerificationFailure,
                        format!(
                            "step '{}' has no fresh passing evidence ({command})",
                            current.id
                        ),
                    );
                    return Err(format!(
                        "step '{}' requires fresh passing evidence for the current change set; run `{command}`\n{announcement}",
                        current.id
                    )
                    .into());
                }
            }
            auto_duration_ms = Some(record_step_duration_ms(&mut state, &current.id));
            state.completed_steps.push(current.id.clone());
            state.current_step += 1;
            let jev_cfg = if state.current().is_some_and(|step| {
                matches!(step.phase, WorkflowPhase::Review | WorkflowPhase::Verify)
            }) {
                load_workflow_jev_config(&state.repo)
            } else {
                None
            };
            reclassify_at_gate(state_dir, &mut state, jev_cfg.as_ref());
            sync_artifact_records(&mut state);
            ensure_current_artifact_template(&state)?;
            state.status = match state.current() {
                None => WorkflowStatus::Completed,
                Some(step) if state.step_requires_approval(step) => {
                    WorkflowStatus::AwaitingApproval
                }
                Some(_) => WorkflowStatus::Running,
            };
            // Issue #349: `state.status` just above is ALWAYS a fresh
            // transition off `Running` here -- `advance_with_evidence`
            // already refused (line ~1623) to reach this match at all while
            // the previous status was `AwaitingApproval`. `AwaitingApproval`
            // needs an operator; `Completed`/`Running` mean this advance
            // cleared whatever gate attention a prior failed attempt left.
            match state.status {
                WorkflowStatus::AwaitingApproval => record_workflow_attention(
                    crate::commands::ctx::attention::Attention::WorkflowGate,
                    format!(
                        "step '{}' awaiting approval",
                        state.current().map(|step| step.id.as_str()).unwrap_or("?")
                    ),
                ),
                WorkflowStatus::Running | WorkflowStatus::Completed => record_workflow_attention(
                    crate::commands::ctx::attention::Attention::None,
                    "step advanced successfully",
                ),
                WorkflowStatus::Failed | WorkflowStatus::Closed => {}
            }
        }
        StepOutcome::Failure => {
            // Issue #699 Phase 0: captured before `phase_started_at` resets
            // below -- this failed attempt's own elapsed time, for the
            // `PhaseFailed` event's `duration_ms` (an explicit
            // `--duration-ms` still overrides it, same as `Success`).
            auto_duration_ms = Some(phase_elapsed_ms(&state));
            let attempts = state.attempts.entry(current.id.clone()).or_default();
            *attempts = attempts.saturating_add(1);
            if *attempts >= current.max_attempts {
                state.status = WorkflowStatus::Failed;
            }
        }
    }
    state.updated_at = now_secs();
    state.phase_started_at = state.updated_at;
    let active = matches!(
        state.status,
        WorkflowStatus::Running | WorkflowStatus::AwaitingApproval
    );
    save(state_dir, &state, active)?;
    let evidence = evidence.cloned().unwrap_or_default();
    let (findings_total, findings_meaningful, findings_dismissed) =
        super::telemetry::finding_counts(&state.review_findings);
    let mut event = super::telemetry::TelemetryEvent::new(match outcome {
        StepOutcome::Success => super::telemetry::TelemetryKind::PhaseCompleted,
        StepOutcome::Failure => super::telemetry::TelemetryKind::PhaseFailed,
    });
    event.workflow_id = Some(state.id.clone());
    event.phase = Some(current.phase);
    event.intent = Some(state.classification.intent);
    event.complexity = Some(state.classification.complexity);
    event.risk = Some(state.classification.risk);
    event.work_domain = Some(state.classification.work_domain.domain);
    // Issue #699 Phase 0: `--duration-ms` remains an explicit override when
    // a caller passes one; otherwise this is the same elapsed span
    // `record_step_duration_ms`/`phase_elapsed_ms` already derived from
    // `phase_started_at` above, so `slowest phase`/the implement-vs-
    // validate split work for any ordinary run with no special caller
    // cooperation.
    event.duration_ms = evidence.duration_ms.or(auto_duration_ms);
    event.adapter = evidence.adapter;
    event.model = evidence.model;
    event.role = evidence.role;
    event.input_tokens = evidence.input_tokens;
    event.output_tokens = evidence.output_tokens;
    event.token_usage_source = evidence.token_usage_source;
    event.cache_creation_input_tokens = evidence.cache_creation_input_tokens;
    event.cache_read_input_tokens = evidence.cache_read_input_tokens;
    event.sidechain_input_tokens = evidence.sidechain_input_tokens;
    event.sidechain_cache_creation_input_tokens = evidence.sidechain_cache_creation_input_tokens;
    event.sidechain_cache_read_input_tokens = evidence.sidechain_cache_read_input_tokens;
    event.sidechain_output_tokens = evidence.sidechain_output_tokens;
    event.session_id = evidence.session_id;
    // Issue #264: this process's own supervising session, if the delegated
    // worker running this workflow step was told one (`agent::PARENT_
    // SESSION_ENV`, set by `agent::run_with`/`dash::mod::fulfill_spawn_
    // request` at spawn time) -- what makes a delegation tree's cost
    // attributable up the chain, not just down to the leaf that ran it.
    // `None` for an orchestrator running this step directly (no parent to
    // report), the same "read fresh from this process's own env, never
    // cached" discipline `agent::parent_identity`'s own doc comment holds.
    event.parent_session_id =
        crate::commands::ctx::agent::parent_identity(&|key| std::env::var(key).ok());
    event.verification_unchanged = evidence.verification_unchanged;
    event.succeeded = Some(outcome == StepOutcome::Success);
    event.findings_total = findings_total;
    event.findings_meaningful = findings_meaningful;
    event.findings_dismissed = findings_dismissed;
    event.fix_round = state.attempts.get(&current.id).copied().unwrap_or(0);
    // Issue #699 Phase 0: only a genuine failure is a fix round happening;
    // a `Success` never gets a cause, even if `attempts` above is nonzero
    // (a step that failed N times before finally passing). Best-effort --
    // `load_latest` reads the SAME persisted report the engine's own
    // Test/Verify gate above already required to be fresh, so this never
    // does speculative work the gate did not already justify; a load
    // failure degrades to `None` (unclassified) rather than failing
    // `advance` itself.
    event.fix_round_cause = (outcome == StepOutcome::Failure)
        .then(|| {
            let report = super::verification::load_latest(state_dir, &state.repo)
                .ok()
                .flatten();
            super::telemetry::classify_fix_round_cause(
                current.phase,
                evidence.verification_unchanged,
                report.as_ref(),
            )
        })
        .flatten();
    event.worker_count = evidence.worker_count;
    // Issue #264: best-effort -- a config load failure here must never fail
    // `advance` itself, so it degrades to the built-in price table (no
    // operator override) rather than propagating the error.
    let price_cfg =
        crate::commands::ctx::config::CtxConfig::load(&state.repo, &|key| std::env::var(key).ok())
            .unwrap_or_default();
    event.apply_price(&crate::commands::ctx::price::resolve_table(&price_cfg));
    let _ = super::telemetry::record(
        state_dir,
        &state.repo,
        &event,
        &super::telemetry::TelemetryConfig::for_repo(&state.repo),
    );
    if state.status == WorkflowStatus::Completed {
        let mut completed = super::telemetry::TelemetryEvent::new(
            super::telemetry::TelemetryKind::WorkflowCompleted,
        );
        completed.workflow_id = Some(state.id.clone());
        completed.intent = Some(state.classification.intent);
        completed.complexity = Some(state.classification.complexity);
        completed.risk = Some(state.classification.risk);
        completed.work_domain = Some(state.classification.work_domain.domain);
        completed.deploy_tier = Some(state.deploy_tier.to_string());
        completed.succeeded = Some(true);
        completed.findings_total = findings_total;
        completed.findings_meaningful = findings_meaningful;
        completed.findings_dismissed = findings_dismissed;
        let _ = super::telemetry::record(
            state_dir,
            &state.repo,
            &completed,
            &super::telemetry::TelemetryConfig::for_repo(&state.repo),
        );
    }
    if outcome == StepOutcome::Success {
        try_auto_spawn(state_dir, &state);
    }
    Ok(state)
}

pub fn approve(state_dir: &StateDir, mut state: WorkflowState) -> CtxResult<WorkflowState> {
    // Checked against the as-loaded status, before `refresh_deploy_tier`: see
    // `advance_with_evidence`'s identical guard for why.
    if state.status != WorkflowStatus::AwaitingApproval {
        return Err("workflow is not awaiting approval".into());
    }
    refresh_deploy_tier(&mut state)?;

    if let Some(stage) = state.current().and_then(|step| step.artifact) {
        // Accepted predecessor artifacts must still be the exact bytes that
        // were reviewed. The current stage itself is intentionally excluded
        // until pin_current_artifact replaces its acceptance record.
        if let Some(drifted) = artifact_drift(&state)?
            && drifted != stage
        {
            reopen_artifact_gate(&mut state, drifted)?;
            save(state_dir, &state, true)?;
            return Err(format!(
                "accepted {drifted} artifact changed after approval; re-approve it before {stage}"
            )
            .into());
        }
        let completed = state.current().expect("artifact step exists").clone();
        let jev_cfg = load_workflow_jev_config(&state.repo);
        let (accepted, warning) =
            pin_current_artifact_with_config(state_dir, &mut state, jev_cfg.as_ref())?;
        if let Some(warning) = warning {
            crate::output::warn(warning);
        }
        // Issue #699 Phase 0: `None` when this artifact was already
        // completed (the `!contains` guard above is false) -- re-approving
        // an artifact that drifted back into acceptance without a genuine
        // new `AwaitingApproval` span has no honest wait to report.
        let mut approval_wait_ms = None;
        if !state.completed_steps.contains(&completed.id) {
            approval_wait_ms = Some(record_step_duration_ms(&mut state, &completed.id));
            state.completed_steps.push(completed.id);
        }
        state.current_step += 1;
        reclassify_at_gate(state_dir, &mut state, jev_cfg.as_ref());
        sync_artifact_records(&mut state);
        ensure_current_artifact_template(&state)?;
        state.status = match state.current() {
            None => WorkflowStatus::Completed,
            Some(step) if state.step_requires_approval(step) => WorkflowStatus::AwaitingApproval,
            Some(_) => WorkflowStatus::Running,
        };
        state.updated_at = now_secs();
        state.phase_started_at = state.updated_at;
        let active = matches!(
            state.status,
            WorkflowStatus::Running | WorkflowStatus::AwaitingApproval
        );
        save(state_dir, &state, active)?;

        let mut event = super::telemetry::TelemetryEvent::new(
            super::telemetry::TelemetryKind::ArtifactAccepted,
        );
        event.workflow_id = Some(state.id.clone());
        event.phase = Some(completed.phase);
        event.intent = Some(state.classification.intent);
        event.complexity = Some(state.classification.complexity);
        event.risk = Some(state.classification.risk);
        event.work_domain = Some(state.classification.work_domain.domain);
        event.succeeded = Some(true);
        event.artifact_stage = Some(accepted.to_string());
        // Issue #699 Phase 0: the whole `AwaitingApproval` span this
        // artifact-gated step spent, agent-drafting time and operator
        // review time both -- see `TelemetryEvent::approval_wait_ms`'s own
        // doc comment for why this is never sub-divided further.
        event.approval_wait_ms = approval_wait_ms;
        let _ = super::telemetry::record(
            state_dir,
            &state.repo,
            &event,
            &super::telemetry::TelemetryConfig::for_repo(&state.repo),
        );
        // Issue #349: a chained approval-required step (`state.status` is
        // `AwaitingApproval` again) still needs an operator, so only a
        // genuine `Running`/`Completed` outcome clears the gate attention.
        match state.status {
            WorkflowStatus::AwaitingApproval => record_workflow_attention(
                crate::commands::ctx::attention::Attention::WorkflowGate,
                format!(
                    "step '{}' awaiting approval",
                    state.current().map(|step| step.id.as_str()).unwrap_or("?")
                ),
            ),
            WorkflowStatus::Running | WorkflowStatus::Completed => record_workflow_attention(
                crate::commands::ctx::attention::Attention::None,
                "artifact approved",
            ),
            WorkflowStatus::Failed | WorkflowStatus::Closed => {}
        }
        return Ok(state);
    }

    // Issue #542 chunk 5: a gate-only (no `artifact`) approval step does not
    // advance `current_step` here -- it stays current -- so record THIS
    // step's id as approved. Without it, the next `refresh_deploy_tier`
    // (called at the top of both `approve` and `advance_with_evidence`)
    // would recompute `status` from this same still-current step's
    // declarative `approval = true` and immediately re-derive
    // `AwaitingApproval`, making the approval just granted unobservable.
    let approved_phase = state.current().map(|step| step.phase);
    // Issue #699 Phase 0: this gate-only step's own `AwaitingApproval` span
    // -- captured before `phase_started_at` resets below. Unlike the
    // artifact branch above, a gate-only step does NOT complete here (it
    // stays current and still has to run), so this must never be written to
    // `step_durations_ms`/`record_step_duration_ms` (that would falsely
    // mark the step as finished). Resetting `phase_started_at` to now, here,
    // is what makes the two spans separable at all: without it, the step's
    // EVENTUAL completion would measure from before this approval, folding
    // wait and execution back together exactly like the state itself does
    // not otherwise distinguish them (see `TelemetryEvent::
    // approval_wait_ms`'s own doc comment).
    let gate_wait_ms = phase_elapsed_ms(&state);
    if let Some(step) = state.current() {
        state.current_step_approved = Some(step.id.clone());
    }
    state.status = WorkflowStatus::Running;
    state.updated_at = now_secs();
    state.phase_started_at = state.updated_at;
    save(state_dir, &state, true)?;

    // Issue #542 review nit: a gate-only approval is still an approval --
    // emit the same `ArtifactAccepted` telemetry event the artifact branch
    // above does (with no `artifact_stage`, since there is none), so an
    // operator/dashboard reading this event stream sees every approval
    // grant, not only the artifact-gated ones.
    let mut event =
        super::telemetry::TelemetryEvent::new(super::telemetry::TelemetryKind::ArtifactAccepted);
    event.workflow_id = Some(state.id.clone());
    event.phase = approved_phase;
    event.intent = Some(state.classification.intent);
    event.complexity = Some(state.classification.complexity);
    event.risk = Some(state.classification.risk);
    event.work_domain = Some(state.classification.work_domain.domain);
    event.succeeded = Some(true);
    // Issue #699 Phase 0: see the artifact branch's identical assignment.
    event.approval_wait_ms = Some(gate_wait_ms);
    let _ = super::telemetry::record(
        state_dir,
        &state.repo,
        &event,
        &super::telemetry::TelemetryConfig::for_repo(&state.repo),
    );

    record_workflow_attention(
        crate::commands::ctx::attention::Attention::None,
        "step approved",
    );
    Ok(state)
}

/// Closes a workflow that will not reach `Completed` -- typically one whose
/// review/fix loop hit `MAX_FIX_REVIEW_ROUNDS` (review.rs) and would
/// otherwise stay `Running` forever, still reported as this repository's
/// active workflow by `load_active`. Refuses (fail closed) when the
/// workflow's status is already terminal (`Completed`/`Failed`/`Closed`) --
/// `close` only applies to a workflow still in flight -- while any review
/// finding is still `Open` -- residual dispositions must be recorded first,
/// via `workflow review dispose` -- or while the workflow is
/// `AwaitingApproval`, since approval is itself a pending decision on the
/// current step. Otherwise sets `status: Closed`, records `closed_reason`/
/// `closed_at`, and persists via `save_inactive_if_active`, which clears
/// this repository's active pointer only when it currently names THIS
/// workflow -- closing an older, non-active workflow must never deactivate a
/// different, currently-running one for the same repo.
pub fn close(
    state_dir: &StateDir,
    state: WorkflowState,
    reason: Option<String>,
) -> CtxResult<WorkflowState> {
    if matches!(
        state.status,
        WorkflowStatus::Completed | WorkflowStatus::Failed | WorkflowStatus::Closed
    ) {
        return Err(format!(
            "cannot close workflow: already {:?}; close only applies to a workflow that will \
             not reach Completed on its own",
            state.status
        )
        .into());
    }
    let open_findings = state
        .review_findings
        .iter()
        .filter(|finding| finding.disposition == super::review::FindingDisposition::Open)
        .count();
    if open_findings > 0 {
        return Err(format!(
            "cannot close workflow: {open_findings} open review finding(s) remain; record \
             dispositions first (see `zirv workflow review dispose`)"
        )
        .into());
    }
    if state.status == WorkflowStatus::AwaitingApproval {
        return Err(
            "cannot close workflow while awaiting approval; approve or reject the current step \
             first"
                .into(),
        );
    }
    finish_close(state_dir, state, reason)
}

/// Issue #537 review: a workflow the proxy started immediately before a
/// spawn that then failed is `AwaitingApproval` the instant `packs/feature.
/// toml`/`packs/bugfix.toml` gate the first step behind `approval = true`
/// (any bounded-or-riskier classification does) -- `close`'s own approval
/// refusal above exists because approval is a pending human decision on the
/// CURRENT step, but nobody has made or seen that decision yet here. This
/// path is deliberately narrower than `close`: it only ever closes a
/// workflow sitting at its very first gate, before a human has advanced OR
/// approved anything at all -- zero `completed_steps` and zero accepted
/// artifacts. The moment either is non-empty, this refuses and the caller
/// must go through `close` instead, the same fail-closed shape `close`
/// itself already uses for every other state it will not touch.
pub fn close_unstarted(
    state_dir: &StateDir,
    state: WorkflowState,
    reason: Option<String>,
) -> CtxResult<WorkflowState> {
    if matches!(
        state.status,
        WorkflowStatus::Completed | WorkflowStatus::Failed | WorkflowStatus::Closed
    ) {
        return Err(format!(
            "cannot close workflow: already {:?}; close only applies to a workflow that will \
             not reach Completed on its own",
            state.status
        )
        .into());
    }
    if state.status != WorkflowStatus::AwaitingApproval {
        return Err(
            "close_unstarted only applies to a workflow awaiting approval at its first gate; \
             use `close`"
                .into(),
        );
    }
    if !state.completed_steps.is_empty() {
        return Err(
            "cannot close_unstarted: at least one step has already completed; use `close`".into(),
        );
    }
    if state.artifacts.values().any(|a| a.accepted_hash.is_some()) {
        return Err(
            "cannot close_unstarted: at least one artifact has already been accepted; use \
             `close`"
                .into(),
        );
    }
    finish_close(state_dir, state, reason)
}

/// The actual close: sets `status: Closed`, records `closed_reason`/
/// `closed_at`, persists via `save_inactive_if_active` (clears this
/// repository's active pointer only when it currently names THIS
/// workflow), and records the same `TelemetryKind::Closed` event either of
/// [`close`]/[`close_unstarted`] always did inline before this split.
fn finish_close(
    state_dir: &StateDir,
    mut state: WorkflowState,
    reason: Option<String>,
) -> CtxResult<WorkflowState> {
    let now = now_secs();
    state.status = WorkflowStatus::Closed;
    state.closed_reason = reason;
    state.closed_at = Some(now);
    state.updated_at = now;
    save_inactive_if_active(state_dir, &state)?;

    let mut event = super::telemetry::TelemetryEvent::new(super::telemetry::TelemetryKind::Closed);
    event.workflow_id = Some(state.id.clone());
    event.intent = Some(state.classification.intent);
    event.complexity = Some(state.classification.complexity);
    event.risk = Some(state.classification.risk);
    event.work_domain = Some(state.classification.work_domain.domain);
    let _ = super::telemetry::record(
        state_dir,
        &state.repo,
        &event,
        &super::telemetry::TelemetryConfig::for_repo(&state.repo),
    );
    Ok(state)
}

/// What became of one open review finding when its own
/// `recommended_disposition` was applied in bulk -- see
/// [`apply_recommended_dispositions`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppliedDisposition {
    pub finding_id: String,
    /// `Some` when the finding carried a recommendation and was moved to it;
    /// `None` when it had none or requires an explicit disposition.
    pub applied: Option<super::review::FindingDisposition>,
    /// A Critical or Major dismissal was withheld from bulk application.
    pub requires_explicit_disposition: bool,
}

/// Applies every *open* review finding's own `recommended_disposition` in one
/// call, collapsing the per-finding turn cost of `zirv workflow review
/// dispose` for the common case where a reviewer already recommended a
/// disposition (review.rs stores it as `ReviewFinding::recommended_
/// disposition`, read here through that existing field rather than any new
/// accessor). A finding that is not `Open` (already `Accepted`/`Dismissed`/
/// `Fixed`/`Residual`) is left untouched and not reported at all -- this is
/// additive over the single-finding dispose, never a way to revisit a
/// decision already made. An open finding with no recommendation is left
/// `Open` and still reported, so the caller can see it was considered and
/// skipped rather than silently missed. Critical and Major findings whose
/// recommendation is `Dismissed` also remain `Open` for explicit disposition.
///
/// Called directly by `zirv workflow review dispose --apply-recommended`
/// (`review::ReviewCommand::Dispose`'s handler): the flag lives on the same
/// `dispose` verb the single-finding form uses -- a standalone
/// `DisposeRecommended` subcommand was the stopgap spelling before that flag
/// was wired up. Mirrors `review.rs`'s own single-finding arm otherwise: the
/// same load/mutate/save/telemetry shape (its `record_finding_update` is
/// private to that module, so the telemetry recording is reimplemented here
/// rather than exposed solely for this one caller), just applied to every
/// eligible finding instead of one named by id.
pub fn apply_recommended_dispositions(
    state_dir: &StateDir,
    mut state: WorkflowState,
) -> CtxResult<(WorkflowState, Vec<AppliedDisposition>)> {
    let mut results = Vec::new();
    for finding in &mut state.review_findings {
        if finding.disposition != super::review::FindingDisposition::Open {
            continue;
        }
        let requires_explicit_disposition = finding.recommended_disposition
            == Some(super::review::FindingDisposition::Dismissed)
            && matches!(
                finding.severity,
                super::review::FindingSeverity::Critical | super::review::FindingSeverity::Major
            );
        let applied = finding
            .recommended_disposition
            .filter(|_| !requires_explicit_disposition);
        results.push(AppliedDisposition {
            finding_id: finding.id.clone(),
            applied,
            requires_explicit_disposition,
        });
        if let Some(recommended) = applied {
            finding.disposition = recommended;
        }
    }
    state.updated_at = now_secs();
    let active = matches!(
        state.status,
        WorkflowStatus::Running | WorkflowStatus::AwaitingApproval
    );
    save(state_dir, &state, active)?;

    let (findings_total, findings_meaningful, findings_dismissed) =
        super::telemetry::finding_counts(&state.review_findings);
    let mut event =
        super::telemetry::TelemetryEvent::new(super::telemetry::TelemetryKind::FindingUpdated);
    event.workflow_id = Some(state.id.clone());
    event.phase = Some(WorkflowPhase::Review);
    event.intent = Some(state.classification.intent);
    event.complexity = Some(state.classification.complexity);
    event.risk = Some(state.classification.risk);
    event.work_domain = Some(state.classification.work_domain.domain);
    event.findings_total = findings_total;
    event.findings_meaningful = findings_meaningful;
    event.findings_dismissed = findings_dismissed;
    let _ = super::telemetry::record(
        state_dir,
        &state.repo,
        &event,
        &super::telemetry::TelemetryConfig::for_repo(&state.repo),
    );

    Ok((state, results))
}

/// Forces a persisted workflow's methodology overlay (#255 recovery path: a
/// misclassified profile previously could only be fixed by abandoning the
/// workflow). A profile change never adds, removes, or reorders steps --
/// `WorkflowState::set_profile` relabels skills on the existing step list in
/// place -- so completed steps and accepted artifacts are structurally
/// untouched; the state machine is never reset. (Unlike a risk increase,
/// which can genuinely require a new gate, `rematerialize_after_risk_
/// increase`'s known-step-id trimming would be a no-op here anyway, since
/// the same classification always produces the same step ids.)
pub fn reclassify(
    state_dir: &StateDir,
    mut state: WorkflowState,
    profile: WorkflowProfile,
) -> CtxResult<WorkflowState> {
    if !matches!(
        state.status,
        WorkflowStatus::Running | WorkflowStatus::AwaitingApproval
    ) {
        return Err(format!("cannot reclassify workflow in {:?} state", state.status).into());
    }
    state.set_profile(profile);
    sync_artifact_records(&mut state);
    state.status = match state.current() {
        None => WorkflowStatus::Completed,
        Some(step) if state.step_requires_approval(step) => WorkflowStatus::AwaitingApproval,
        Some(_) => WorkflowStatus::Running,
    };
    state.updated_at = now_secs();
    let active = matches!(
        state.status,
        WorkflowStatus::Running | WorkflowStatus::AwaitingApproval
    );
    save(state_dir, &state, active)?;
    Ok(state)
}

/// A headless worker must not answer `brainstorm`'s clarifying questions or
/// write the intent artifact on the operator's behalf.
const BRAINSTORM_HEADLESS_REFUSAL: &str = "This step needs an interactive operator. Do not answer the clarifying questions on their behalf or write the intent artifact; stop and report that the workflow is waiting for the operator.";

fn refusal_for(skill_id: &str, headless: bool) -> Option<&'static str> {
    (headless && skill_id == "brainstorm").then_some(BRAINSTORM_HEADLESS_REFUSAL)
}

/// Only the exact value `"1"` means headless -- `ZIRV_CTX_HEADLESS=0`, an
/// empty string, or any other value must not trip the refusal. Split out of
/// the `std::env::var` call site so the value comparison is testable without
/// a real (racy) environment variable.
fn is_headless_env(raw: Option<&str>) -> bool {
    raw == Some("1")
}

/// Issue #326: caps `rendered`'s total bytes at `max_bytes`, appending a
/// visible marker naming how many bytes were cut rather than a silent
/// truncation -- the same "keep what came first, mark what is missing"
/// shape `memory::cap_body` already uses for a memory entry's own per-entry
/// cap. The head is kept, never the tail: `rendered`'s own workflow/profile/
/// task/step/phase/state header lines come first and matter far more than
/// whichever selected skill happened to render last.
fn cap_workflow_context(rendered: String, max_bytes: usize) -> String {
    if rendered.len() <= max_bytes {
        return rendered;
    }
    let omitted = rendered.len() - max_bytes;
    let marker = format!(
        "\n[workflow context truncated -- {omitted} bytes omitted, cap \
         workflow.max_context_bytes={max_bytes}]\n"
    );
    // Review finding: `max_bytes.saturating_sub(marker.len())` alone still
    // appended the FULL marker even when it alone was longer than
    // `max_bytes` (a tiny operator-set cap), so the "capped" output could
    // exceed its own budget. A cap too small to hold even the marker gets
    // the marker itself, truncated -- an honest "cannot show this" beats
    // output that overruns the ceiling it claims to enforce, the same rule
    // `snapshot::cap_head_tail` uses for its own too-small-a-budget case.
    if marker.len() >= max_bytes {
        return crate::utils::truncate_bytes(marker, Some(max_bytes));
    }
    let keep = max_bytes - marker.len();
    let mut truncated = crate::utils::truncate_bytes(rendered, Some(keep));
    truncated.push_str(&marker);
    truncated
}

/// Why the ACTIVE workflow refuses to let a session declare itself done, or
/// `None` when nothing blocks it (issue #484, roadmap N15).
///
/// The native loop consults this at every completion attempt, so the gate is
/// read live rather than snapshotted at session start: a session that reaches
/// the Test step after it began is gated on the evidence that exists THEN.
/// The shared stop service outranks any model finish token with it, which is
/// what stops a native session declaring success over a step whose evidence
/// is stale, missing or failing.
///
/// Deliberately the same predicate `advance_with_evidence`'s own Test/Verify
/// arm applies -- `verification::latest_is_fresh_and_passing` keyed by the
/// workflow's own recorded branch (issue #467), so a workflow started in the
/// main checkout is satisfied by a worker worktree's evidence for the same
/// change set and by nothing else. A duplicate rule here would be a second
/// definition of "done" that could drift from the real one.
///
/// Fails open only up to the point of deciding whether there is anything to
/// gate on at all: an unreadable state directory, an absent workflow, or an
/// unresolvable branch all mean "nothing to gate on", the same as a step
/// outside Test/Verify. Once that decision is made and this step's
/// completion genuinely depends on fresh verification evidence, a failure
/// reading THAT evidence fails closed instead (issue #599, roadmap N15):
/// silently treating an unreadable record as passing would defeat the gate
/// for exactly the sessions it exists to stop.
pub fn native_completion_gate(state_dir: &StateDir, repo: &Path) -> Option<String> {
    let state = load_active(state_dir, repo).ok().flatten()?;
    if !matches!(
        state.status,
        WorkflowStatus::Running | WorkflowStatus::AwaitingApproval
    ) {
        return None;
    }
    let step = state.current()?;
    if !matches!(
        step.phase,
        super::skill::WorkflowPhase::Test | super::skill::WorkflowPhase::Verify
    ) {
        return None;
    }
    let final_only = step.phase == super::skill::WorkflowPhase::Verify;
    let command = if final_only {
        "zirv verify"
    } else {
        "zirv test changed"
    };
    // Issue #599 (roadmap N15): this differs from the state-load fallback
    // above on purpose. By this point the gate has already committed to
    // needing fresh evidence for a Test/Verify step -- unlike an unreadable
    // state directory or an absent workflow, where there is nothing to gate
    // on at all, a read error HERE means the evidence this step's
    // completion depends on could not be evaluated. Treating that as
    // "assume it passed" (`.unwrap_or(true)`) let missing permissions,
    // corruption, or any other evidence-read failure silently satisfy the
    // gate; failing closed with the error surfaced is the only reading that
    // keeps "fresh passing evidence" meaning what it says.
    let fresh = match super::verification::latest_is_fresh_and_passing(
        state_dir,
        &state.repo,
        final_only,
        Some(&state.branch),
    ) {
        Ok(fresh) => fresh,
        Err(error) => {
            return Some(format!(
                "zirv workflow: step '{}' of workflow '{}' could not read its verification evidence ({error}); run `{command}` and record the result before finishing",
                step.id, state.id
            ));
        }
    };
    if fresh {
        return None;
    }
    Some(format!(
        "zirv workflow: step '{}' of workflow '{}' has no fresh passing evidence for the current change set; run `{command}` and record the result before finishing",
        step.id, state.id
    ))
}

/// Current ephemeral skill context for the context compiler/session prompt.
/// Completed steps are intentionally absent; the durable state remains in
/// [`WorkflowState`] and is never accumulated into model context.
pub fn render_current_context(
    state: &WorkflowState,
    repo: &Path,
    home: Option<&Path>,
) -> CtxResult<Option<String>> {
    let Some(step) = state.current() else {
        return Ok(None);
    };
    if !matches!(
        state.status,
        WorkflowStatus::Running | WorkflowStatus::AwaitingApproval
    ) {
        return Ok(None);
    }
    let registry = SkillRegistry::load_for_repo(repo, home, state.include_custom_skills)?;
    let task = state
        .task
        .chars()
        .take(1_024)
        .map(|character| {
            if character.is_control() {
                ' '
            } else {
                character
            }
        })
        .collect::<String>();
    let mut rendered = format!(
        "zirv workflow step\nworkflow: {}\nprofile: {:?}\ntask: {}\nstep: {}\nphase: {}\nstate: {:?}\n",
        state.kind.as_str(),
        state.profile,
        task,
        step.id,
        step.phase,
        state.status
    );
    if let Some(agent) = step.agent.as_deref() {
        rendered.push_str(&format!("agent-seat: {agent}\n"));
    }
    if let Some(stage) = step.artifact {
        ensure_current_artifact_template(state)?;
        let record = state
            .artifacts
            .get(stage.key())
            .ok_or("current workflow artifact record is missing")?;
        rendered.push_str(&format!(
            "artifact: {} ({stage}; fill this committed work product, then wait for acceptance)\n",
            record.rel_path
        ));
    }
    append_accepted_artifacts(state, &mut rendered)?;
    if state.profile == WorkflowProfile::Frontend {
        let state_dir = StateDir::resolve(&|key| std::env::var(key).ok())?;
        let profile = super::frontend::ensure_profile(&state_dir, repo)?;
        rendered.push('\n');
        rendered.push_str(&super::frontend::render_profile(&profile));
        rendered.push('\n');
    }
    let headless = is_headless_env(
        std::env::var(crate::commands::ctx::adapters::HEADLESS_ENV)
            .ok()
            .as_deref(),
    );
    let mut rendered_skill_ids = BTreeSet::new();
    for selected in step_skill_ids(step, &state.classification) {
        for skill in registry.resolve_stack(&selected)? {
            if !rendered_skill_ids.insert(skill.manifest.id.clone()) {
                continue;
            }
            let body = refusal_for(&skill.manifest.id, headless)
                .unwrap_or_else(|| skill.manifest.instructions.trim());
            let body = sanitize_skill_body(body);
            let hash_prefix = &skill.content_hash[..skill.content_hash.len().min(12)];
            rendered.push_str(&format!(
                "\n{SKILL_HEADER_SENTINEL}[skill {}@{}; source={}; hash={hash_prefix}]\n{}\n",
                skill.manifest.id, skill.manifest.version, skill.source, body
            ));
        }
    }
    // Issue #326: a step whose selected skills happen to be large must not
    // inject them unbounded into the session prompt (`prompt::with_workflow_
    // layer`) or print them unbounded from `zirv workflow context` -- both
    // consumers funnel through this one function, so capping here catches
    // both at the single source rather than needing its own cap at each
    // consumer. A config load failure degrades to the built-in default
    // rather than skipping the cap entirely: this function must never fail
    // just because config could not be read.
    let max_context_bytes =
        crate::commands::ctx::config::CtxConfig::load(repo, &|key| std::env::var(key).ok())
            .map_or_else(
                |_| crate::commands::ctx::config::WorkflowConfig::default().max_context_bytes,
                |cfg| cfg.workflow.max_context_bytes,
            );
    Ok(Some(cap_workflow_context(rendered, max_context_bytes)))
}

pub fn active_skill_context(repo: &Path) -> CtxResult<Option<String>> {
    let state_dir = StateDir::resolve(&|key| std::env::var(key).ok())?;
    let Some(state) = load_active(&state_dir, repo)? else {
        return Ok(None);
    };
    match render_current_context(&state, repo, dirs::home_dir().as_deref()) {
        Ok(context) => Ok(context),
        // The caller composes a prompt and cannot fail over this, but a
        // silently dropped workflow layer is a session running without the
        // methodology it thinks it has. Say so once, on the channel a repo
        // cannot silence (`chrome.events` is REPO_FORBIDDEN).
        Err(error) => {
            announce_degradation(repo, &error.to_string());
            Ok(None)
        }
    }
}

fn announce_degradation(repo: &Path, reason: &str) {
    let enabled =
        crate::commands::ctx::config::CtxConfig::load(repo, &|key| std::env::var(key).ok())
            .map_or(true, |cfg| cfg.chrome.events);
    crate::commands::ctx::announce::Announcer::new(enabled, false).emit(
        &crate::commands::ctx::announce::Event::WorkflowLayerSkipped {
            reason: reason.to_string(),
        },
    );
}

#[derive(Debug, Args)]
pub struct WorkflowArgs {
    #[command(subcommand)]
    pub command: WorkflowSubcommand,
}

#[derive(Debug, Subcommand)]
pub enum WorkflowSubcommand {
    /// List built-in workflow definitions.
    List(OutputArgs),
    /// Show a workflow definition.
    Show(ShowArgs),
    /// Classify a task without starting a workflow.
    Classify(classify::ClassifyArgs),
    /// Start and persist a workflow.
    Start(StartArgs),
    /// Show one workflow instance, or the active one.
    Status(StatusArgs),
    /// Restore a running workflow as this repository's active workflow.
    Resume(StateIdArgs),
    /// Force a persisted workflow's methodology overlay, preserving
    /// completed steps and accepted artifacts (#255 recovery path).
    Reclassify(ReclassifyArgs),
    /// Print only the current step's resolved skill context.
    Context(StatusArgs),
    /// Inspect committed workflow work-product artifacts and acceptance state.
    Artifacts(ArtifactsArgs),
    /// Inspect provider-neutral workflow seats and their trust provenance.
    Agents(super::agents::AgentArgs),
    /// Compile, show, and brief the proportional team for a request (issue
    /// #541).
    Team(super::team::TeamArgs),
    /// Approve the current gated step.
    Approve(StateIdArgs),
    /// Record a step result and transition the state machine.
    Advance(AdvanceArgs),
    /// Close a workflow that will not reach `Completed` (for example one
    /// whose review/fix loop hit `MAX_FIX_REVIEW_ROUNDS`), recording residual
    /// dispositions first. Refuses while any review finding is `Open` or the
    /// workflow is `AwaitingApproval`; clears it as this repository's active
    /// workflow.
    Close(CloseArgs),
    /// Build compact review packages and persist finding dispositions,
    /// including `review dispose --apply-recommended` -- see
    /// [`apply_recommended_dispositions`]'s own doc comment.
    Review(super::review::ReviewArgs),
    /// Run deterministic operator-configured maintenance detectors.
    Maintain(super::maintain::MaintainArgs),
    /// Aggregate privacy-conscious local workflow telemetry.
    Stats(super::telemetry::StatsArgs),
}

#[derive(Debug, Args)]
pub struct OutputArgs {
    #[arg(long)]
    pub json: bool,
    /// Ignore operator-global and repository-provided workflow packs.
    #[arg(long)]
    pub built_in_only: bool,
    /// Repository root; defaults to the current directory.
    #[arg(long)]
    pub repo: Option<PathBuf>,
}

#[derive(Debug, Args)]
pub struct ShowArgs {
    /// A registry id -- one of the five built-in kind ids (`feature`,
    /// `bugfix`, `refactor`, `spike`, `review`) or any operator-global/
    /// repository pack id (issue #542; was a closed `WorkflowKind` value
    /// before, so the five kind spellings keep working unchanged).
    pub id: String,
    #[arg(long)]
    pub json: bool,
    /// Ignore operator-global and repository-provided workflow packs.
    #[arg(long)]
    pub built_in_only: bool,
    /// Repository root; defaults to the current directory.
    #[arg(long)]
    pub repo: Option<PathBuf>,
}

#[derive(Debug, Args)]
pub struct StartArgs {
    /// A registry id. Omit it and `workflow start` deterministically SELECTS
    /// one via `selection::select_definition` against `--task` and the
    /// resolved classification (issue #542 chunk 3b); an explicit id always
    /// wins outright, with no selection performed at all.
    pub id: Option<String>,
    #[arg(long)]
    pub task: String,
    /// Harness adapter used for capability preflight (for example claude/codex).
    #[arg(long)]
    pub agent: Option<String>,
    /// Ignore operator-global and repository-provided skill/agent overrides.
    #[arg(long)]
    pub built_in_only: bool,
    #[arg(long)]
    pub repo: Option<PathBuf>,
    #[arg(long = "path")]
    pub paths: Vec<PathBuf>,
    #[arg(long)]
    pub changed_lines: Option<usize>,
    #[arg(long)]
    pub tests_changed: bool,
    #[arg(long, value_enum)]
    pub complexity: Option<Complexity>,
    #[arg(long, value_enum)]
    pub risk: Option<RiskBand>,
    /// The branch this workflow gates, when it differs from `--repo`'s own
    /// checked-out branch (issue #467: an orchestrator in the main checkout
    /// starting a workflow for a worker's feature branch it does not have
    /// checked out here). Classification diffs this branch against its own
    /// base as refs, not `--repo`'s working tree. Recorded on the workflow
    /// and matched against a linked worktree's own recorded branch when the
    /// Test/Verify gate widens its read to that worktree's evidence.
    #[arg(long)]
    pub branch: Option<String>,
    /// Repository whose frontend the auto-run detector/render evidence
    /// should scan for a Frontend-profile workflow, when it differs from
    /// `--repo` (for example a workflow tracked in this repo whose frontend
    /// lives in a sibling checkout).
    #[arg(long)]
    pub frontend_root: Option<PathBuf>,
    /// Force the interactive `brainstorm` skill at the intent step.
    #[arg(long, conflicts_with = "no_brainstorm")]
    pub brainstorm: bool,
    /// Force the autonomous `write-intent` skill at the intent step.
    #[arg(long)]
    pub no_brainstorm: bool,
    /// Force this workflow's methodology overlay instead of trusting
    /// automatic classification (#255 recovery path: a misclassified
    /// profile can otherwise only be fixed by abandoning the workflow).
    #[arg(long, value_enum)]
    pub profile: Option<WorkflowProfile>,
    #[arg(long)]
    pub json: bool,
}

impl StartArgs {
    pub(crate) fn brainstorm_override(&self) -> Option<bool> {
        if self.brainstorm {
            Some(true)
        } else if self.no_brainstorm {
            Some(false)
        } else {
            None
        }
    }
}

#[derive(Debug, Args)]
pub struct StatusArgs {
    pub id: Option<String>,
    #[arg(long)]
    pub repo: Option<PathBuf>,
    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, Args)]
pub struct StateIdArgs {
    pub id: String,
    #[arg(long)]
    pub repo: Option<PathBuf>,
}

#[derive(Debug, Args)]
pub struct CloseArgs {
    pub id: String,
    #[arg(long)]
    pub repo: Option<PathBuf>,
    /// Operator-supplied reason for closing without reaching `Completed`,
    /// recorded on the workflow state.
    #[arg(long)]
    pub reason: Option<String>,
    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, Args)]
pub struct ReclassifyArgs {
    pub id: String,
    #[arg(long)]
    pub repo: Option<PathBuf>,
    #[arg(long, value_enum)]
    pub profile: WorkflowProfile,
    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, Args)]
pub struct ArtifactsArgs {
    pub id: String,
    #[arg(long)]
    pub repo: Option<PathBuf>,
    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, Clone, Serialize)]
struct WorkflowArtifactStatus {
    stage: ArtifactStage,
    rel_path: String,
    exists: bool,
    accepted: bool,
    drifted: bool,
    accepted_at: Option<String>,
}

fn workflow_artifact_statuses(state: &WorkflowState) -> CtxResult<Vec<WorkflowArtifactStatus>> {
    let mut statuses = Vec::new();
    for stage in [
        ArtifactStage::Intent,
        ArtifactStage::Spec,
        ArtifactStage::Plan,
    ] {
        let Some(record) = state.artifacts.get(stage.key()) else {
            continue;
        };
        let path = workflow_artifact_path(state, stage)?;
        let exists = path.exists();
        let accepted = record.accepted_hash.is_some();
        let drifted = match record.accepted_hash.as_deref() {
            Some(hash) => !exists || artifact_hash(&path)? != hash,
            None => false,
        };
        statuses.push(WorkflowArtifactStatus {
            stage,
            rel_path: record.rel_path.clone(),
            exists,
            accepted,
            drifted,
            accepted_at: record.accepted_at.clone(),
        });
    }
    Ok(statuses)
}

#[derive(Debug, Args)]
pub struct AdvanceArgs {
    pub id: String,
    /// Required unless `--run-checks` is set, which determines the outcome
    /// itself from the evidence command's own result.
    #[arg(long, value_enum, required_unless_present = "run_checks")]
    pub outcome: Option<StepOutcome>,
    /// For a `Test`/`Verify` step, run the step's own required evidence
    /// command in-process (`zirv test changed` for `Test`, `zirv verify` for
    /// `Verify`) and advance on success, printing the evidence summary; on
    /// failure, print it and do not advance. Collapses "run the gate, then
    /// advance" into one call. Conflicts with `--outcome`, which the checks'
    /// own result determines instead.
    #[arg(long, conflicts_with = "outcome")]
    pub run_checks: bool,
    #[arg(long)]
    pub repo: Option<PathBuf>,
    #[arg(long)]
    pub json: bool,
    #[arg(long)]
    pub duration_ms: Option<u64>,
    #[arg(long)]
    pub agent: Option<String>,
    #[arg(long)]
    pub model: Option<String>,
    #[arg(long)]
    pub role: Option<String>,
    #[arg(long)]
    pub input_tokens: Option<u64>,
    #[arg(long)]
    pub output_tokens: Option<u64>,
    #[arg(long, default_value_t = 0)]
    pub workers: u32,
    /// Set (or update) the sibling repository whose frontend the auto-run
    /// detector/render evidence should scan for this workflow, for example
    /// once it becomes clear the tracked repo isn't the one under test.
    #[arg(long)]
    pub frontend_root: Option<PathBuf>,
    /// Accept the frontend detector's pre-existing (not newly introduced)
    /// blocking findings so this advance can proceed; introduced blocking
    /// findings always still fail. Recorded on the workflow and applies for
    /// the rest of it once accepted.
    #[arg(long)]
    pub accept_preexisting_findings: bool,
}

pub(crate) fn resolve_repo(repo: Option<&Path>) -> CtxResult<PathBuf> {
    Ok(match repo {
        Some(path) => path.canonicalize().unwrap_or_else(|_| path.to_path_buf()),
        None => std::env::current_dir()?,
    })
}

fn report_registry_warnings(registry: &super::registry::WorkflowRegistry) {
    for warning in registry.warnings() {
        crate::output::warn(warning);
    }
}

/// The layered [`super::registry::WorkflowRegistry`] for `repo`: built-ins,
/// plus operator-global/repository packs unless `built_in_only`. Issue #542.
pub(crate) fn load_workflow_registry(
    repo: &Path,
    built_in_only: bool,
) -> CtxResult<super::registry::WorkflowRegistry> {
    let skills = SkillRegistry::load_for_repo(repo, dirs::home_dir().as_deref(), !built_in_only)?;
    super::registry::WorkflowRegistry::load_for_repo(
        repo,
        dirs::home_dir().as_deref(),
        !built_in_only,
        &skills,
    )
}

/// The plain-text rendering `workflow list` prints without `--json` --
/// shared verbatim with the native `/workflows` slash command (issue #542
/// chunk 3b, decision 5) so the two surfaces can never drift apart.
pub(crate) fn write_registry_list(
    writer: &mut impl Write,
    entries: &[&super::registry::RegisteredWorkflow],
) -> CtxResult<()> {
    writeln!(writer, "ID\tLAYER\tVERSION\tHASH\tDOMAINS")?;
    for workflow in entries {
        writeln!(
            writer,
            "{}\t{}\t{}\t{}\t{}",
            workflow.definition.id,
            workflow.source,
            workflow.definition.version,
            &workflow.hash[..workflow.hash.len().min(12)],
            workflow.definition.domains.join(",")
        )?;
    }
    Ok(())
}

/// The plain-text rendering `workflow show <id>` prints without `--json` --
/// shared verbatim with the native `/workflow <id>` slash command (issue
/// #542 chunk 3b, decision 5) so the two surfaces can never drift apart.
pub(crate) fn write_registry_entry(
    writer: &mut impl Write,
    workflow: &super::registry::RegisteredWorkflow,
) -> CtxResult<()> {
    writeln!(
        writer,
        "{}@{} ({}): {}",
        workflow.definition.id,
        workflow.definition.version,
        workflow.source,
        workflow.definition.description
    )?;
    for step in &workflow.definition.steps {
        writeln!(
            writer,
            "  {}\t{}\tskills={}\tagent_role={}\tartifact={}\tdepends_on={}\twhen={:?}\tapproval={}",
            step.id,
            step.phase,
            step.skills.join(","),
            step.agent_role.as_deref().unwrap_or("-"),
            step.artifact
                .map(|stage| stage.to_string())
                .unwrap_or_else(|| "-".into()),
            if step.depends_on.is_empty() {
                "-".to_string()
            } else {
                step.depends_on.join(",")
            },
            step.condition,
            step.approval
        )?;
    }
    Ok(())
}

/// The rendering `workflow start` prints (JSON or text) for a [`StartOutcome`]
/// -- shared verbatim with the native `/workflow <id>` slash command's start
/// path (issue #542 chunk 3b, decision 5) so the two surfaces can never
/// drift apart.
pub(crate) fn write_start_outcome(
    writer: &mut impl Write,
    outcome: &StartOutcome,
    json: bool,
) -> CtxResult<()> {
    if outcome.work_dir_gitignored {
        writeln!(
            writer,
            "warning: .zirv/work is ignored by this repository's .gitignore -- workflow artifacts will not be tracked by git"
        )?;
    }
    match (&outcome.selection, json) {
        (Some(selection), true) => {
            let mut value = serde_json::to_value(&outcome.state)?;
            if let serde_json::Value::Object(map) = &mut value {
                map.insert("selection".into(), serde_json::to_value(selection)?);
            }
            serde_json::to_writer_pretty(&mut *writer, &value)?;
            writeln!(writer)?;
        }
        // Issue #542 review nit: `write_state` itself now prints "selected:
        // ..." from `state.selection` (persisted alongside it, above), so
        // this no longer needs its own separate print -- `outcome.state.
        // selection` is set from this same `outcome.selection` value.
        (Some(_), false) => write_state(writer, &outcome.state, false)?,
        (None, json) => write_state(writer, &outcome.state, json)?,
    }
    Ok(())
}

/// A pinned [`DefinitionRef`] resolved from `state`, one line for the
/// terminal reader plus, when the registry's current copy of that id no
/// longer hashes the same (or the id has vanished entirely), an explicit
/// drift note -- decision #6 of issue #542's chunks 1+2. Best-effort: a
/// registry that fails to load (an unusual environment problem, not the
/// workflow's own concern) is silently treated as "cannot check drift"
/// rather than failing `zirv workflow status` outright.
pub(crate) fn write_definition_status(
    writer: &mut impl Write,
    state: &WorkflowState,
) -> CtxResult<()> {
    let Some(reference) = &state.definition else {
        return Ok(());
    };
    writeln!(
        writer,
        "definition: {}@{} ({}) [{}]",
        reference.id,
        reference.version,
        &reference.hash[..reference.hash.len().min(12)],
        reference.source_layer
    )?;
    let current_hash = load_workflow_registry(&state.repo, false)
        .ok()
        .and_then(|registry| registry.get(&reference.id).ok().map(|w| w.hash.clone()));
    match current_hash {
        Some(hash) if hash == reference.hash => {}
        Some(_) => writeln!(writer, "definition drifted from registry")?,
        None => writeln!(
            writer,
            "definition drifted from registry (id no longer resolves)"
        )?,
    }
    Ok(())
}

/// Resolves and validates `--frontend-root`: absolutized against the current
/// directory, then required to exist and be a directory so a typo fails
/// loudly at parse time instead of surfacing later as "0 files scanned".
fn resolve_frontend_root(path: &Path) -> CtxResult<PathBuf> {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()?.join(path)
    };
    let canonical = absolute.canonicalize().map_err(|err| {
        format!(
            "frontend root '{}' does not exist: {err}",
            absolute.display()
        )
    })?;
    if !canonical.is_dir() {
        return Err(format!("frontend root '{}' is not a directory", canonical.display()).into());
    }
    Ok(canonical)
}

pub(crate) fn resolve_state() -> CtxResult<StateDir> {
    StateDir::resolve(&|key| std::env::var(key).ok())
}

/// Runs the evidence command a `Test`/`Verify` step itself requires --
/// `zirv test changed` for `Test`, `zirv verify` for `Verify` -- through the
/// same in-process function the CLI verb itself calls (never a subprocess),
/// printing its evidence summary to `writer`. Returns the resolved
/// [`super::verification::GateOutcome`]. Backs `zirv workflow advance
/// --run-checks`, which collapses "run the gate, then advance" into a single
/// call. Any other phase is not something `--run-checks` knows how to
/// satisfy, so it errors rather than silently treating the step as passed.
///
/// Issue #287: before running anything, checks whether the worktree is
/// byte-identical to the fingerprint recorded by this step's own previous
/// *failing* report ([`super::verification::last_failure_fingerprint`]) --
/// a no-op turn since that attempt can only reach the same verdict, so no
/// check is executed at all and `GateOutcome::Unchanged` is returned
/// directly.
///
/// Otherwise, pass/fail is decided by
/// [`super::verification::latest_is_fresh_and_passing`] against the report
/// the run just persisted -- the exact same baseline-aware gate the plain
/// `zirv workflow advance --outcome success` path applies to a report from
/// an out-of-process `zirv test changed`/`zirv verify` run (see the
/// `Test`/`Verify` arm of `advance_with_evidence`). `run_test`/`run_verify`'s
/// own raw exit code is deliberately not used here: it reflects the run's
/// unwaived pass/fail, so a report whose only failures are covered by the
/// operator's recorded baseline (`zirv test baseline`) exits non-zero even
/// though the same report satisfies the gate -- see the dogfooding bug where
/// `--run-checks` printed "checks failed" immediately before a follow-up
/// `--outcome success` against the identical report advanced with the
/// baseline warning. Collapsed to `GateOutcome::Fail` here regardless of
/// which of `Fail`/`Inconclusive` the report's own outcome would name -- the
/// caller only ever distinguished pass from fail before this issue, and
/// still only needs to distinguish `Unchanged` from everything else.
///
/// Before running, and again after, this snapshots
/// [`super::verification::latest_report_id`] -- the persisted report's own
/// identity (a fresh UUID every run). `run_and_persist` swallows a `persist`
/// failure into a warning so the run's printed results survive a transient
/// IO error, which means the report `latest_is_fresh_and_passing` would read
/// afterwards can still be whatever older report preceded this run. If the
/// identity did not change, no fresh report exists to gate on at all -- so
/// this fails the step outright rather than falling back to evaluating that
/// stale report (which could easily still be fresh-and-passing against the
/// unchanged fingerprint, silently advancing a step whose check just
/// genuinely failed).
fn run_required_checks(
    state_dir: &StateDir,
    repo: &Path,
    phase: WorkflowPhase,
    step_id: &str,
    attempts_so_far: u8,
    branch: &str,
    writer: &mut impl Write,
) -> CtxResult<super::verification::GateOutcome> {
    if !matches!(phase, WorkflowPhase::Test | WorkflowPhase::Verify) {
        return Err(format!(
            "--run-checks only applies to Test/Verify steps; the current step is {phase:?} -- \
             pass --outcome instead"
        )
        .into());
    }
    let fingerprint = super::verification::change_fingerprint(repo)?;
    if let Some(last_failure) =
        super::verification::last_failure_fingerprint(state_dir, repo, step_id)?
        && last_failure == fingerprint
    {
        return Ok(super::verification::GateOutcome::Unchanged {
            fingerprint,
            since_attempt: attempts_so_far.saturating_add(1),
        });
    }
    let before = super::verification::latest_report_id(state_dir, repo)?;
    let run_args = super::verification::RunArgs {
        repo: Some(repo.to_path_buf()),
        checks: Vec::new(),
        dry_run: false,
        json: false,
    };
    let final_only = match phase {
        WorkflowPhase::Test => {
            super::verification::run_test(
                &super::verification::TestArgs {
                    command: super::verification::TestCommand::Changed(run_args),
                },
                writer,
            )?;
            false
        }
        WorkflowPhase::Verify => {
            super::verification::run_verify(
                &super::verification::VerifyArgs {
                    run: run_args,
                    builtin: false,
                },
                writer,
            )?;
            true
        }
        _ => unreachable!("non-Test/Verify phases returned above"),
    };
    let after = super::verification::latest_report_id(state_dir, repo)?;
    if after == before {
        writeln!(
            writer,
            "checks ran but no fresh report was persisted; step '{step_id}' was not advanced"
        )?;
        return Ok(super::verification::GateOutcome::Fail);
    }
    if super::verification::latest_is_fresh_and_passing(state_dir, repo, final_only, Some(branch))?
    {
        Ok(super::verification::GateOutcome::Pass)
    } else {
        Ok(super::verification::GateOutcome::Fail)
    }
}

/// This attempt's elapsed wall-clock, in milliseconds, since
/// `state.phase_started_at` -- shared by `record_step_duration_ms` (a
/// completed step) and, issue #699 Phase 0, by `advance_with_evidence`'s
/// `Failure` arm (a failed attempt that will retry): `phase_started_at` is
/// reset unconditionally after EVERY `advance_with_evidence` call,
/// success or failure, so this is well-defined either way -- it names
/// "since the step became current, or since the previous attempt was
/// recorded", never a stale span.
fn phase_elapsed_ms(state: &WorkflowState) -> u64 {
    now_secs()
        .saturating_sub(state.phase_started_at)
        .saturating_mul(1000)
}

/// Call before pushing `step_id` onto `completed_steps`, while
/// `phase_started_at` still names its own start. Returns the elapsed
/// milliseconds recorded, so a caller can also feed it (issue #699 Phase 0)
/// to a telemetry event's `duration_ms` without a second, potentially
/// inconsistent `now_secs()` read.
fn record_step_duration_ms(state: &mut WorkflowState, step_id: &str) -> u64 {
    let elapsed_ms = phase_elapsed_ms(state);
    state
        .step_durations_ms
        .insert(step_id.to_string(), elapsed_ms);
    elapsed_ms
}

/// `<minutes>m<seconds>s`, e.g. `2m10s`.
fn format_wall_clock(ms: u64) -> String {
    let total_secs = ms / 1000;
    format!("{}m{}s", total_secs / 60, total_secs % 60)
}

/// A bounded worker to auto-spawn after a gate transition.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct AutoSpawn {
    pub phase: WorkflowPhase,
    pub argv: Vec<String>,
    /// Review is read-only; Test and Verify need writing mode for build
    /// artifacts and caches. Not yet consumed by `spawn_auto_worker`, which
    /// launches zirv subprocesses directly rather than delegating workers.
    pub mode: crate::commands::ctx::permit::WorkerMode,
    /// Issue #264: the task class this auto-spawn's own work is, alongside
    /// `mode` above -- for a future caller that threads it onto the
    /// delegation this ultimately becomes (`zirv ctx agent --task-class`).
    /// Not yet consumed by `spawn_auto_worker` itself (this call spawns a
    /// `zirv workflow review run`/`test changed`/`verify` subprocess
    /// directly, not `zirv ctx agent`), the same not-yet-wired parity `mode`
    /// already holds for this exact call.
    pub task_class: crate::commands::ctx::log::TaskClass,
}

/// Why a gate transition that WOULD otherwise be eligible (right phase,
/// `Running`, enabled) did not produce an [`AutoSpawn`]. `Quiet` covers
/// every case that is not worth an operator's attention: the config key is
/// off, the phase is not Review/Test/Verify, or the workflow is
/// `AwaitingApproval` -- an operator who never opted in, or a transition
/// this feature was never meant to touch, must see nothing new. `NoPermit`/
/// `NoAgent` are the opposite: the operator explicitly enabled this, so a
/// skip is reported, not silent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AutoSpawnSkip {
    Quiet,
    NoPermit,
    NoAgent,
}

/// Pure: whether a gate transition should auto-spawn a worker, the argv for
/// it, or the reason it did not. `default_agent` is the operator's
/// configured chat harness (`adapters::resolve_default`, the same one `zirv
/// ctx chat` launches by default), consulted only when the workflow itself
/// has no recorded adapter -- `state.adapter` always wins when set.
pub(crate) fn auto_spawn_decision(
    state: &WorkflowState,
    enabled: bool,
    permit_available: bool,
    default_agent: Option<&str>,
) -> Result<AutoSpawn, AutoSpawnSkip> {
    if !enabled || state.status != WorkflowStatus::Running {
        return Err(AutoSpawnSkip::Quiet);
    }
    let phase = state.current().ok_or(AutoSpawnSkip::Quiet)?.phase;
    if !matches!(
        phase,
        WorkflowPhase::Review | WorkflowPhase::Test | WorkflowPhase::Verify
    ) {
        return Err(AutoSpawnSkip::Quiet);
    }
    if !permit_available {
        return Err(AutoSpawnSkip::NoPermit);
    }
    let repo = state.repo.display().to_string();
    let argv = match phase {
        WorkflowPhase::Review => {
            let agent = state
                .adapter
                .clone()
                .or_else(|| default_agent.map(str::to_string))
                .ok_or(AutoSpawnSkip::NoAgent)?;
            vec![
                "workflow".to_string(),
                "review".to_string(),
                "run".to_string(),
                state.id.clone(),
                "--agent".to_string(),
                agent,
                "--repo".to_string(),
                repo,
            ]
        }
        WorkflowPhase::Test => vec![
            "test".to_string(),
            "changed".to_string(),
            "--repo".to_string(),
            repo,
        ],
        WorkflowPhase::Verify => vec!["verify".to_string(), "--repo".to_string(), repo],
        _ => unreachable!("filtered above"),
    };
    Ok(AutoSpawn {
        phase,
        argv,
        mode: if phase == WorkflowPhase::Review {
            crate::commands::ctx::permit::WorkerMode::ReadOnly
        } else {
            crate::commands::ctx::permit::WorkerMode::Writing
        },
        // Issue #264: Review is its own class; Test and Verify are both
        // "did the checkout pass" work, so both map to `TaskClass::Test`.
        task_class: match phase {
            WorkflowPhase::Review => crate::commands::ctx::log::TaskClass::Review,
            WorkflowPhase::Test | WorkflowPhase::Verify => {
                crate::commands::ctx::log::TaskClass::Test
            }
            _ => unreachable!("filtered above"),
        },
    })
}

#[cfg(unix)]
fn detach(command: &mut std::process::Command) {
    use std::os::unix::process::CommandExt;
    command.process_group(0);
}

#[cfg(windows)]
fn detach(command: &mut std::process::Command) {
    use std::os::windows::process::CommandExt;
    const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
    const DETACHED_PROCESS: u32 = 0x0000_0008;
    command.creation_flags(CREATE_NEW_PROCESS_GROUP | DETACHED_PROCESS);
}

/// The operator explicitly enabled `auto_spawn_on_gate`, so a skip that
/// reaches this point (as opposed to `AutoSpawnSkip::Quiet`) is reported,
/// not silent.
fn announce_auto_spawn_skip(
    cfg: &crate::commands::ctx::config::CtxConfig,
    phase: WorkflowPhase,
    reason: &str,
) {
    crate::commands::ctx::announce::Announcer::new(cfg.chrome.events, false).emit(
        &crate::commands::ctx::announce::Event::AutoSpawnSkipped {
            phase: phase.to_string(),
            reason: reason.to_string(),
        },
    );
}

/// Issue #242: spawns `spawn.argv` detached and never fails `advance` --
/// `test changed`/`verify`/`review run` govern no heavy-operation permit of
/// their own, so this acquires one on their behalf and leaks it (the child
/// outlives this call): `HeavyPermit::set_child_pid` plus `permit::live_
/// records`' own dead-owner sweep is exactly the mechanism that frees the
/// slot once the detached child exits, the same as a parent that dies while
/// its child keeps running.
fn spawn_auto_worker(
    state_dir: &StateDir,
    state: &WorkflowState,
    cfg: &crate::commands::ctx::config::CtxConfig,
    spawn: AutoSpawn,
) {
    use crate::commands::ctx::permit;

    // A race against `try_auto_spawn`'s own peek: the peek said a slot was
    // free, but another caller took it before this real acquire ran.
    let Some(permit) = permit::acquire(
        state_dir,
        cfg.supervise.max_heavy_operations,
        &format!("auto-spawn: {}", spawn.argv.join(" ")),
    ) else {
        announce_auto_spawn_skip(cfg, spawn.phase, "no heavy-operation permit was free");
        return;
    };
    let Ok(exe) = std::env::current_exe() else {
        announce_auto_spawn_skip(
            cfg,
            spawn.phase,
            "could not resolve the zirv executable path",
        );
        return;
    };
    let mut command = std::process::Command::new(exe);
    command
        .args(&spawn.argv)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    detach(&mut command);
    let Ok(child) = command.spawn() else {
        announce_auto_spawn_skip(cfg, spawn.phase, "failed to spawn the worker process");
        return;
    };
    permit.set_child_pid(child.id());
    std::mem::forget(permit);

    let mut event =
        super::telemetry::TelemetryEvent::new(super::telemetry::TelemetryKind::AgentDispatched);
    event.workflow_id = Some(state.id.clone());
    event.phase = Some(spawn.phase);
    event.intent = Some(state.classification.intent);
    event.complexity = Some(state.classification.complexity);
    event.risk = Some(state.classification.risk);
    event.work_domain = Some(state.classification.work_domain.domain);
    event.agent_id = Some(format!("auto-spawn:{}", spawn.phase));
    let _ = super::telemetry::record(
        state_dir,
        &state.repo,
        &event,
        &super::telemetry::TelemetryConfig::for_repo(&state.repo),
    );

    crate::commands::ctx::announce::Announcer::new(cfg.chrome.events, false).emit(
        &crate::commands::ctx::announce::Event::AutoSpawned {
            phase: spawn.phase.to_string(),
            command: spawn.argv.join(" "),
        },
    );
}

/// Thin I/O wrapper around [`auto_spawn_decision`]: resolves config, a
/// permit peek, and (only when the workflow itself has no adapter) the
/// operator's default chat harness, then hands off to [`spawn_auto_worker`].
/// Any failure along the way (config, permit, spawn) is silently degraded --
/// never propagated to `advance`'s own result -- but a skip the operator's
/// own `auto_spawn_on_gate = true` made eligible is announced, not silent.
fn try_auto_spawn(state_dir: &StateDir, state: &WorkflowState) {
    let cfg = match crate::commands::ctx::config::CtxConfig::load(&state.repo, &|key| {
        std::env::var(key).ok()
    }) {
        Ok(cfg) => cfg,
        Err(_) => return,
    };
    if !cfg.workflow.auto_spawn_on_gate {
        return;
    }
    let permit_available =
        crate::commands::ctx::permit::live_count(state_dir) < cfg.supervise.max_heavy_operations;
    // `state.adapter` always wins; the operator's configured chat harness
    // (the same one `adapters::resolve_default` picks for `zirv ctx chat`)
    // is only worth resolving -- readiness probes and all -- when the
    // workflow itself named none.
    let default_agent = state
        .adapter
        .is_none()
        .then(|| {
            crate::commands::ctx::adapters::resolve_default(&cfg)
                .ok()
                .map(|(adapter, _)| adapter.name().to_string())
        })
        .flatten();
    match auto_spawn_decision(state, true, permit_available, default_agent.as_deref()) {
        Ok(spawn) => spawn_auto_worker(state_dir, state, &cfg, spawn),
        Err(skip) => {
            if let Some(reason) = auto_spawn_skip_reason(skip)
                && let Some(phase) = state.current().map(|step| step.phase)
            {
                announce_auto_spawn_skip(&cfg, phase, reason);
            }
        }
    }
}

/// The advisory text for a skip the operator's own `auto_spawn_on_gate =
/// true` made eligible, or `None` for `Quiet` -- the ordinary, never-
/// announced case (disabled, wrong phase, `AwaitingApproval`).
fn auto_spawn_skip_reason(skip: AutoSpawnSkip) -> Option<&'static str> {
    match skip {
        AutoSpawnSkip::Quiet => None,
        AutoSpawnSkip::NoPermit => Some("no heavy-operation permit was free"),
        AutoSpawnSkip::NoAgent => Some(
            "no adapter to run the reviewer as (the workflow has none, and no operator \
             default chat harness could be resolved)",
        ),
    }
}

pub(crate) fn write_state(
    writer: &mut impl Write,
    state: &WorkflowState,
    json: bool,
) -> CtxResult<()> {
    if json {
        serde_json::to_writer_pretty(&mut *writer, state)?;
        writeln!(writer)?;
    } else {
        writeln!(writer, "workflow: {}", state.id)?;
        writeln!(writer, "kind: {}", state.kind.as_str())?;
        // Issue #542 review nit: `state.selection` persists the deterministic
        // selection that chose this run's pack (when one ran at all -- an
        // explicit id at start never populates it), so `status` can explain
        // why a pack was chosen without a separate `workflow classify` call
        // against the same task text.
        if let Some(selection) = &state.selection {
            writeln!(
                writer,
                "selected: {} ({})",
                selection.definition_id,
                selection.reasons.join("; ")
            )?;
        }
        writeln!(
            writer,
            "profile: {:?} ({})",
            state.profile,
            match state.profile_source {
                ProfileSource::Classified => "classified",
                ProfileSource::OperatorOverride => "operator override",
            }
        )?;
        if let Some(frontend_root) = &state.frontend_target_root {
            writeln!(writer, "frontend target root: {}", frontend_root.display())?;
        }
        if let Some(accepted) = &state.accepted_preexisting_findings {
            writeln!(
                writer,
                "accepted pre-existing frontend findings: {} blocking / {} total at {} ({})",
                accepted.blocking, accepted.total, accepted.step, accepted.at
            )?;
        }
        writeln!(writer, "deploy tier: {}", state.deploy_tier)?;
        if !state.jev_tags.is_empty() {
            writeln!(writer, "jev tags: {}", state.jev_tags.join(", "))?;
        }
        writeln!(writer, "status: {:?}", state.status)?;
        if let Some(reason) = &state.closed_reason {
            writeln!(writer, "closed reason: {reason}")?;
        }
        writeln!(
            writer,
            "classification: {:?}/{:?} risk={} ({:?})",
            state.classification.intent,
            state.classification.complexity,
            state.classification.risk_score,
            state.classification.risk
        )?;
        if let classify::RiskMeasurement::Unavailable { reason } =
            &state.classification.risk_measurement
        {
            writeln!(writer, "risk measurement: unavailable ({reason})")?;
        }
        // Issue #236: only meaningful when this workflow actually has an
        // intent step -- `Review` never does, and a Feature/Bugfix/Refactor
        // whose classification did not gate one in has nothing for the flag
        // to select between.
        if state
            .steps
            .iter()
            .any(|step| step.phase == WorkflowPhase::Intent)
        {
            writeln!(
                writer,
                "brainstorm: {}",
                if state.brainstorm { "on" } else { "off" }
            )?;
        }
        if let Some(step) = state.current() {
            writeln!(
                writer,
                "current: {} ({}, skill {}, agent {}{})",
                step.id,
                step.phase,
                step.skill,
                step.agent.as_deref().unwrap_or("-"),
                step.artifact
                    .map(|stage| format!(", artifact {stage}"))
                    .unwrap_or_default()
            )?;
        } else {
            writeln!(writer, "current: none")?;
        }
        let completed_rendered = state
            .completed_steps
            .iter()
            .map(|id| match state.step_durations_ms.get(id) {
                Some(&ms) => format!("{id} ({})", format_wall_clock(ms)),
                None => id.clone(),
            })
            .collect::<Vec<_>>()
            .join(", ");
        writeln!(writer, "completed: {completed_rendered}")?;
    }
    Ok(())
}

/// The result of [`start_workflow`]: the newly persisted [`WorkflowState`],
/// the [`super::selection::Selection`] that chose it (`None` when the
/// caller gave an explicit id), and whether `.zirv/work` is gitignored in
/// the target repository (a caller-formatted warning, not printed here).
pub struct StartOutcome {
    pub state: WorkflowState,
    pub selection: Option<super::selection::Selection>,
    pub work_dir_gitignored: bool,
}

/// The one-line note [`start_workflow`] prints to STDERR when starting a new
/// workflow silently changes which workflow this repository's active
/// pointer names -- workflow-trigger-determinism item 5. Multiple workflows
/// per repository are legitimate (`zirv workflow resume` restores any of
/// them), so `start_workflow` never refuses the start; it only says what
/// moved and how to get it back. A pure fn so its exact wording is
/// unit-testable without capturing real process stderr.
fn active_workflow_displaced_note(old_instance_id: &str, old_definition_id: &str) -> String {
    format!(
        "note: workflow {old_instance_id} ({old_definition_id}) is no longer this repository's \
         active workflow; restore it with: zirv workflow resume {old_instance_id}"
    )
}

/// Review finding F2: writes `note` to `writer` best-effort. By the time
/// [`start_workflow`] reaches this, the new workflow is already saved --
/// `eprintln!`/`crate::output::note` panic on a write error (a closed
/// stderr, say), which would surface as a spurious failure of an already-
/// successful start. Ignoring the `Result` here instead keeps this call
/// site pure passthrough, the same posture `wrap.rs` holds its own
/// supervision failures to.
fn best_effort_write_displacement_note(mut writer: impl std::io::Write, note: &str) {
    let _ = writeln!(writer, "{note}");
}

/// Starts and persists a workflow from `args` -- the SAME logic `zirv
/// workflow start` and the native `workflow_start` tool both run, so
/// "what starting a workflow means" has exactly one implementation (issue
/// #542 chunk 3b), matching this crate's own "a second implementation of
/// what advances a step is a second definition of done" rule for the
/// native workflow tools. Pure of `writer`/output formatting: the caller
/// decides how to render [`StartOutcome`] (CLI text/JSON, or a tool's JSON
/// result) -- except for one STDERR note when the start displaces a
/// different, still-running workflow as this repository's active one (see
/// [`active_workflow_displaced_note`]); stdout/`--json` output is unaffected
/// either way.
pub fn start_workflow(state_dir: &StateDir, args: &StartArgs) -> CtxResult<StartOutcome> {
    let repo = resolve_repo(args.repo.as_deref())?;
    // Issue #542 chunk 3a decision 4: any registry id executes through this
    // same path now, not just the five legacy kinds. `registry.get`'s own
    // "unknown workflow '<id>'" phrasing matches `load`'s convention for an
    // unknown STATE id.
    let registry = load_workflow_registry(&repo, args.built_in_only)?;
    report_registry_warnings(&registry);
    let inherited_agent = session_identity().map(|(_, adapter)| adapter);
    let selected_agent = args.agent.clone().or(inherited_agent);

    // Workflow-trigger-determinism: registry ids are validated lowercase at
    // load (`definition::valid_id`), so an explicit id is matched case-
    // insensitively by lowercasing it here once, before it feeds
    // `WorkflowKind::from_pack_id` or `registry.get` -- `zirv workflow start
    // Bugfix` must resolve exactly like `zirv workflow start bugfix`.
    let requested_id = args.id.as_deref().map(str::to_ascii_lowercase);

    // Issue #542 chunk 3b: an explicit id always wins outright, no
    // selection performed at all. Omitting it classifies first (intent
    // inferred naturally, never forced to an explicit id's kind) and runs
    // `select_definition` against that classification and the raw `--task`
    // objective text.
    let explicit_kind_hint = requested_id.as_deref().and_then(WorkflowKind::from_pack_id);
    let classify_args = classify::ClassifyArgs {
        task: args.task.clone(),
        paths: args.paths.clone(),
        changed_lines: args.changed_lines,
        tests_changed: args.tests_changed,
        intent: explicit_kind_hint.map(|kind| kind.intent()),
        complexity: args.complexity,
        risk: args.risk,
        repo: Some(repo.clone()),
        branch: args.branch.clone(),
        json: false,
    };
    let classification = classify::from_args(&classify_args)?;
    let selection = if requested_id.is_none() {
        Some(super::selection::select_definition(
            &classification,
            &registry,
            &args.task,
        ))
    } else {
        None
    };
    let resolved_id = match &requested_id {
        Some(id) => id.clone(),
        None => selection
            .as_ref()
            .expect("selection ran when id is None")
            .definition_id
            .clone(),
    };
    let pack = registry.get(&resolved_id)?;
    let kind_hint = WorkflowKind::from_pack_id(&pack.definition.id);
    if classification.work_domain.domain == WorkDomain::Frontend {
        // Eager zero-touch bootstrap. Prompt rendering refreshes this
        // derived profile as repository evidence evolves.
        super::frontend::ensure_profile(state_dir, &repo)?;
    }
    let profile = WorkflowProfile::for_classification(&classification);
    let deploy_tier = super::deploy::effective_tier(&repo)?;
    let brainstorm = args
        .brainstorm_override()
        .unwrap_or_else(|| kind_hint.map(default_brainstorm_for_kind).unwrap_or(false));
    let materialized = materialize_from_definition(
        &pack.definition,
        &classification,
        profile,
        deploy_tier,
        brainstorm,
    );
    if let Some(agent) = &selected_agent {
        let skills =
            SkillRegistry::load_for_repo(&repo, dirs::home_dir().as_deref(), !args.built_in_only)?;
        let report = super::capability::CapabilityReport::for_repo(agent, &repo)?;
        for step in &materialized {
            for skill in step_skill_ids(step, &classification) {
                skills.ensure_supported(&skill, &report)?;
            }
            // Issue #483: a workflow must not enter a step whose required
            // integration is unavailable. The refusal names the missing
            // binary or credential, here at start, rather than halfway
            // through the step.
            let frontend = classification.work_domain.domain == WorkDomain::Frontend;
            report
                .admit(&super::capability::required_integrations(
                    step.phase, frontend,
                ))
                .map_err(|why| format!("step '{}': {why}", step.id))?;
        }
    }
    // Issue #542 chunk 3a decision 1: every referenced agent role must
    // resolve before any state is written -- independent of whether an
    // execution adapter was even given (the capability-support loop above
    // only runs `if let Some(agent) = ...`, so a plain existence check runs
    // unconditionally here instead).
    let agent_registry =
        AgentRegistry::load_for_repo(&repo, dirs::home_dir().as_deref(), !args.built_in_only)?;
    for step in &materialized {
        if let Some(role) = step.agent.as_deref() {
            agent_registry
                .get(role)
                .map_err(|_| format!("step '{}': unknown agent role '{role}'", step.id))?;
        }
    }
    // Workflow-trigger-determinism item 5: read the CURRENT active pointer
    // before it gets overwritten below, so a start that silently displaces a
    // still-running workflow can be reported after the fact -- multiple
    // workflows per repository are legitimate (`zirv workflow resume`
    // restores any of them), so this never refuses the start itself.
    let previously_active = load_active(state_dir, &repo).ok().flatten();
    let mut state = WorkflowState::start_from_pack(
        repo,
        args.task.clone(),
        pack,
        selected_agent,
        !args.built_in_only,
        classification,
    );
    state.selection = selection.clone();
    state.branch = args
        .branch
        .clone()
        .unwrap_or_else(|| super::verification::current_branch(&state.repo));
    if brainstorm != state.brainstorm {
        state.brainstorm = brainstorm;
        apply_brainstorm_selection(
            brainstorm,
            WorkflowKind::from_pack_id(&pack.definition.id).is_some(),
            &mut state.steps,
        );
    }
    if let Some(forced_profile) = args.profile {
        state.set_profile(forced_profile);
    }
    apply_effective_deploy_tier(&mut state, deploy_tier);
    state.usage_checkpoint = usage_checkpoint(&state.repo);
    if let Some(frontend_root) = &args.frontend_root {
        state.frontend_target_root = Some(resolve_frontend_root(frontend_root)?);
    }
    ensure_current_artifact_template(&state)?;
    let work_dir_gitignored = work_dir_is_gitignored(&state.repo);
    save(state_dir, &state, true)?;
    if let Some(old) = previously_active
        && old.id != state.id
        && matches!(
            old.status,
            WorkflowStatus::Running | WorkflowStatus::AwaitingApproval
        )
    {
        let old_definition_id = old
            .definition
            .as_ref()
            .map(|definition| definition.id.clone())
            .unwrap_or_else(|| old.kind.as_str().to_string());
        best_effort_write_displacement_note(
            std::io::stderr(),
            &active_workflow_displaced_note(&old.id, &old_definition_id),
        );
    }
    let mut event =
        super::telemetry::TelemetryEvent::new(super::telemetry::TelemetryKind::WorkflowStarted);
    event.workflow_id = Some(state.id.clone());
    event.intent = Some(state.classification.intent);
    event.complexity = Some(state.classification.complexity);
    event.risk = Some(state.classification.risk);
    event.work_domain = Some(state.classification.work_domain.domain);
    event.deploy_tier = Some(state.deploy_tier.to_string());
    let _ = super::telemetry::record(
        state_dir,
        &state.repo,
        &event,
        &super::telemetry::TelemetryConfig::for_repo(&state.repo),
    );
    Ok(StartOutcome {
        state,
        selection,
        work_dir_gitignored,
    })
}

pub fn run(args: &WorkflowArgs, writer: &mut impl Write) -> CtxResult<i32> {
    match &args.command {
        WorkflowSubcommand::List(args) => {
            let repo = resolve_repo(args.repo.as_deref())?;
            let registry = load_workflow_registry(&repo, args.built_in_only)?;
            report_registry_warnings(&registry);
            let entries: Vec<&super::registry::RegisteredWorkflow> = registry.list().collect();
            if args.json {
                serde_json::to_writer_pretty(&mut *writer, &entries)?;
                writeln!(writer)?;
            } else {
                write_registry_list(writer, &entries)?;
            }
        }
        WorkflowSubcommand::Show(args) => {
            let repo = resolve_repo(args.repo.as_deref())?;
            let registry = load_workflow_registry(&repo, args.built_in_only)?;
            report_registry_warnings(&registry);
            // Workflow-trigger-determinism: same case-insensitive id match
            // as `workflow start` -- registry ids are validated lowercase
            // at load, so `zirv workflow show Bugfix` must resolve like
            // `zirv workflow show bugfix`.
            let workflow = registry.get(&args.id.to_ascii_lowercase())?;
            if args.json {
                serde_json::to_writer_pretty(&mut *writer, workflow)?;
                writeln!(writer)?;
            } else {
                write_registry_entry(writer, workflow)?;
            }
        }
        WorkflowSubcommand::Classify(args) => {
            let value = classify::from_args(args)?;
            // Issue #542 chunk 3b: best-effort, so an unreadable registry
            // (an unusual environment problem, not classify's own concern)
            // never breaks `workflow classify` -- it just omits `selection`.
            let repo = resolve_repo(args.repo.as_deref())?;
            let selection = load_workflow_registry(&repo, false)
                .ok()
                .map(|registry| super::selection::select_definition(&value, &registry, &args.task));
            if args.json {
                #[derive(Serialize)]
                struct ClassifyOutput<'a> {
                    #[serde(flatten)]
                    classification: &'a Classification,
                    /// Issue #541 decision 1: the minimal execution profile
                    /// derived from this same classification, embedded
                    /// alongside it rather than requiring a second call.
                    profile: super::profile::ExecutionProfile,
                    /// Issue #542 chunk 3b: best-effort registry selection,
                    /// omitted when the registry could not be loaded.
                    #[serde(skip_serializing_if = "Option::is_none")]
                    selection: Option<&'a super::selection::Selection>,
                }
                let profile = super::profile::ExecutionProfile::derive(&args.task, &value);
                serde_json::to_writer_pretty(
                    &mut *writer,
                    &ClassifyOutput {
                        classification: &value,
                        profile,
                        selection: selection.as_ref(),
                    },
                )?;
                writeln!(writer)?;
            } else {
                writeln!(
                    writer,
                    "intent={:?} domain={:?} complexity={:?} risk={:?} score={}",
                    value.intent,
                    value.work_domain.domain,
                    value.complexity,
                    value.risk,
                    value.risk_score
                )?;
                for reason in value.reasons {
                    writeln!(writer, "- {reason}")?;
                }
                if let Some(selection) = &selection {
                    writeln!(
                        writer,
                        "selection: {} ({})",
                        selection.definition_id,
                        selection.reasons.join("; ")
                    )?;
                }
            }
        }
        WorkflowSubcommand::Start(args) => {
            let state_dir = resolve_state()?;
            let outcome = start_workflow(&state_dir, args)?;
            write_start_outcome(writer, &outcome, args.json)?;
        }
        WorkflowSubcommand::Status(args) => {
            let repo = resolve_repo(args.repo.as_deref())?;
            let state_dir = resolve_state()?;
            let state = match &args.id {
                Some(id) => load(&state_dir, &repo, id)?,
                None => load_active(&state_dir, &repo)?.ok_or("no active workflow")?,
            };
            write_state(writer, &state, args.json)?;
            if !args.json {
                write_definition_status(writer, &state)?;
            }
        }
        WorkflowSubcommand::Resume(args) => {
            let repo = resolve_repo(args.repo.as_deref())?;
            let state_dir = resolve_state()?;
            let mut state = load(&state_dir, &repo, &args.id)?;
            // Checked against the as-loaded status, before `refresh_deploy_tier`:
            // `apply_effective_deploy_tier` unconditionally recomputes `status`
            // from the current step's position, which would otherwise silently
            // revive a terminal `Failed`/`Completed`/`Closed` workflow back to
            // `Running`/`AwaitingApproval`.
            if !matches!(
                state.status,
                WorkflowStatus::Running | WorkflowStatus::AwaitingApproval
            ) {
                return Err(format!("cannot resume workflow in {:?} state", state.status).into());
            }
            refresh_deploy_tier(&mut state)?;
            ensure_current_artifact_template(&state)?;
            save(&state_dir, &state, true)?;
            write_state(writer, &state, false)?;
        }
        WorkflowSubcommand::Reclassify(args) => {
            let repo = resolve_repo(args.repo.as_deref())?;
            let state_dir = resolve_state()?;
            let state = reclassify(&state_dir, load(&state_dir, &repo, &args.id)?, args.profile)?;
            write_state(writer, &state, args.json)?;
        }
        WorkflowSubcommand::Context(args) => {
            let repo = resolve_repo(args.repo.as_deref())?;
            let state_dir = resolve_state()?;
            let state = match &args.id {
                Some(id) => load(&state_dir, &repo, id)?,
                None => load_active(&state_dir, &repo)?.ok_or("no active workflow")?,
            };
            match render_current_context(&state, &repo, dirs::home_dir().as_deref())? {
                Some(context) => write!(writer, "{context}")?,
                None => writeln!(writer, "workflow has no active step context")?,
            }
        }
        WorkflowSubcommand::Agents(args) => {
            return super::agents::run(args, writer);
        }
        WorkflowSubcommand::Team(args) => {
            return super::team::run(args, writer);
        }
        WorkflowSubcommand::Artifacts(args) => {
            let repo = resolve_repo(args.repo.as_deref())?;
            let state_dir = resolve_state()?;
            let state = load(&state_dir, &repo, &args.id)?;
            let statuses = workflow_artifact_statuses(&state)?;
            if args.json {
                serde_json::to_writer_pretty(&mut *writer, &statuses)?;
                writeln!(writer)?;
            } else if statuses.is_empty() {
                writeln!(writer, "workflow has no committed work-product artifacts")?;
            } else {
                writeln!(writer, "STAGE\tPATH\tSTATE")?;
                for status in statuses {
                    let state = if status.drifted {
                        "drifted"
                    } else if status.accepted {
                        "accepted"
                    } else if status.exists {
                        "pending"
                    } else {
                        "missing"
                    };
                    writeln!(
                        writer,
                        "{}\t{}\t{}{}",
                        status.stage,
                        status.rel_path,
                        state,
                        status
                            .accepted_at
                            .as_deref()
                            .map(|at| format!(" ({at})"))
                            .unwrap_or_default()
                    )?;
                }
            }
        }
        WorkflowSubcommand::Approve(args) => {
            let repo = resolve_repo(args.repo.as_deref())?;
            let state_dir = resolve_state()?;
            let state = approve(&state_dir, load(&state_dir, &repo, &args.id)?)?;
            write_state(writer, &state, false)?;
        }
        WorkflowSubcommand::Advance(args) => {
            let repo = resolve_repo(args.repo.as_deref())?;
            let state_dir = resolve_state()?;
            let mut state = load(&state_dir, &repo, &args.id)?;
            if let Some(frontend_root) = &args.frontend_root {
                state.frontend_target_root = Some(resolve_frontend_root(frontend_root)?);
                // Persisted before the gate runs: a fail-closed advance below
                // must not force the operator to pass `--frontend-root` again
                // on retry.
                let active = matches!(
                    state.status,
                    WorkflowStatus::Running | WorkflowStatus::AwaitingApproval
                );
                save(&state_dir, &state, active)?;
            }
            let outcome = if args.run_checks {
                let current = state
                    .current()
                    .cloned()
                    .ok_or("workflow has no current step")?;
                let attempts_so_far = state.attempts.get(&current.id).copied().unwrap_or(0);
                match run_required_checks(
                    &state_dir,
                    &repo,
                    current.phase,
                    &current.id,
                    attempts_so_far,
                    &state.branch,
                    writer,
                )? {
                    super::verification::GateOutcome::Pass => StepOutcome::Success,
                    super::verification::GateOutcome::Unchanged { since_attempt, .. } => {
                        writeln!(
                            writer,
                            "verification not re-run: the worktree is byte-identical to the \
                             previous failed attempt (attempt {since_attempt}/{}). Edit source, \
                             tests, or record a blocker artifact before verifying again.",
                            current.max_attempts
                        )?;
                        let evidence = enrich_transition_evidence(
                            &mut state,
                            TransitionEvidence {
                                verification_unchanged: true,
                                ..Default::default()
                            },
                        );
                        let state = advance_with_evidence(
                            &state_dir,
                            state,
                            StepOutcome::Failure,
                            Some(&evidence),
                            args.accept_preexisting_findings,
                        )?;
                        write_state(writer, &state, args.json)?;
                        return Ok(1);
                    }
                    super::verification::GateOutcome::Fail
                    | super::verification::GateOutcome::Inconclusive(_) => {
                        writeln!(
                            writer,
                            "checks failed; step '{}' was not advanced",
                            current.id
                        )?;
                        return Ok(1);
                    }
                }
            } else {
                args.outcome
                    .ok_or("--outcome is required unless --run-checks is set")?
            };
            let evidence = enrich_transition_evidence(
                &mut state,
                TransitionEvidence {
                    duration_ms: args.duration_ms,
                    adapter: args.agent.clone(),
                    model: args.model.clone(),
                    role: args.role.clone(),
                    input_tokens: args.input_tokens,
                    output_tokens: args.output_tokens,
                    token_usage_source: (args.input_tokens.is_some()
                        || args.output_tokens.is_some())
                    .then(|| "operator-reported".into()),
                    worker_count: args.workers,
                    ..Default::default()
                },
            );
            let state = advance_with_evidence(
                &state_dir,
                state,
                outcome,
                Some(&evidence),
                args.accept_preexisting_findings,
            )?;
            write_state(writer, &state, args.json)?;
        }
        WorkflowSubcommand::Close(args) => {
            let repo = resolve_repo(args.repo.as_deref())?;
            let state_dir = resolve_state()?;
            let state = close(
                &state_dir,
                load(&state_dir, &repo, &args.id)?,
                args.reason.clone(),
            )?;
            write_state(writer, &state, args.json)?;
        }
        WorkflowSubcommand::Review(args) => {
            return super::review::run(args, writer);
        }
        WorkflowSubcommand::Maintain(args) => {
            return super::maintain::run(args, writer);
        }
        WorkflowSubcommand::Stats(args) => {
            return super::telemetry::run_stats(args, writer);
        }
    }
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn low_classification() -> Classification {
        Classification {
            intent: Intent::Feature,
            complexity: Complexity::Trivial,
            risk: RiskBand::Low,
            risk_score: 0,
            changed_files: 1,
            changed_lines: 5,
            changed_paths: Vec::new(),
            declared_scope: false,
            work_domain: Default::default(),
            risk_measurement: classify::RiskMeasurement::Measured,
            reasons: vec!["small".into()],
        }
    }

    /// Prepends a synthetic, always-present intent step, for engine-internals
    /// tests (resume, artifact status, deploy-tier tightening) that need a
    /// leading artifact step regardless of classification -- Feature's own
    /// intent step is conditional now (`ComplexityOrRisk{Bounded, Medium}`,
    /// same as Bugfix), and that threshold overlaps Feature's existing plan
    /// gate (`ComplexityOrRisk{Bounded, High}`) and review gate
    /// (`RiskAtLeast(Medium)`), so no classification gates intent in alone
    /// without also gating in plan or review, which these tests are not
    /// about. `current_step` is already `0` on a freshly started state, so
    /// inserting at the front needs no index adjustment.
    fn with_synthetic_intent_step(mut state: WorkflowState) -> WorkflowState {
        state.steps.insert(
            0,
            artifact_step(
                "intent",
                WorkflowPhase::Intent,
                "brainstorm",
                ArtifactStage::Intent,
                StepCondition::Always,
            ),
        );
        state
    }

    fn skip_leading_artifact_steps(mut state: WorkflowState) -> WorkflowState {
        while state.current().is_some_and(|step| step.artifact.is_some()) {
            let id = state.current().unwrap().id.clone();
            state.completed_steps.push(id);
            state.current_step += 1;
        }
        state.status = if state.current().is_some() {
            WorkflowStatus::Running
        } else {
            WorkflowStatus::Completed
        };
        state
    }

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

    fn at_phase(mut state: WorkflowState, phase: WorkflowPhase) -> WorkflowState {
        state.current_step = state
            .steps
            .iter()
            .position(|step| step.phase == phase)
            .expect("phase present in this workflow's steps");
        state.status = WorkflowStatus::Running;
        state
    }

    fn production_feature_state(repo: &Path) -> WorkflowState {
        let mut classification = low_classification();
        classification.complexity = Complexity::Substantial;
        classification.risk = RiskBand::High;
        let mut state = WorkflowState::start(
            repo.to_path_buf(),
            "ship it".into(),
            WorkflowKind::Feature,
            Some("claude".to_string()),
            true,
            classification,
        );
        apply_effective_deploy_tier(&mut state, DeployTier::Production);
        state
    }

    #[test]
    fn auto_spawn_decision_truth_table() {
        let repo = tempdir().unwrap();
        let state = production_feature_state(repo.path());

        assert_eq!(
            auto_spawn_decision(
                &at_phase(state.clone(), WorkflowPhase::Review),
                false,
                true,
                None
            ),
            Err(AutoSpawnSkip::Quiet),
            "disabled must never fire"
        );
        assert_eq!(
            auto_spawn_decision(
                &at_phase(state.clone(), WorkflowPhase::Review),
                true,
                false,
                None
            ),
            Err(AutoSpawnSkip::NoPermit),
            "no permit must never fire"
        );
        assert_eq!(
            auto_spawn_decision(
                &at_phase(state.clone(), WorkflowPhase::Implement),
                true,
                true,
                None
            ),
            Err(AutoSpawnSkip::Quiet),
            "Implement must never fire"
        );

        let mut awaiting = at_phase(state.clone(), WorkflowPhase::Review);
        awaiting.status = WorkflowStatus::AwaitingApproval;
        assert_eq!(
            auto_spawn_decision(&awaiting, true, true, None),
            Err(AutoSpawnSkip::Quiet),
            "AwaitingApproval must never fire"
        );

        let review = auto_spawn_decision(
            &at_phase(state.clone(), WorkflowPhase::Review),
            true,
            true,
            None,
        )
        .expect("Review with a workflow adapter fires");
        assert_eq!(review.phase, WorkflowPhase::Review);
        assert_eq!(
            review.mode,
            crate::commands::ctx::permit::WorkerMode::ReadOnly,
            "issue #267: a review spawn is read-only"
        );
        assert_eq!(
            review.task_class,
            crate::commands::ctx::log::TaskClass::Review,
            "issue #264: a review-phase auto-spawn is classified as review"
        );
        assert_eq!(
            review.argv,
            vec![
                "workflow",
                "review",
                "run",
                &state.id,
                "--agent",
                "claude",
                "--repo",
                &state.repo.display().to_string(),
            ]
        );

        // `state.adapter` always wins over a configured default, even when
        // both resolve.
        let with_both = auto_spawn_decision(
            &at_phase(state.clone(), WorkflowPhase::Review),
            true,
            true,
            Some("codex"),
        )
        .expect("Review fires");
        assert_eq!(
            with_both.argv[5], "claude",
            "the workflow's own adapter wins"
        );

        let mut no_adapter = at_phase(state.clone(), WorkflowPhase::Review);
        no_adapter.adapter = None;
        assert_eq!(
            auto_spawn_decision(&no_adapter, true, true, None),
            Err(AutoSpawnSkip::NoAgent),
            "Review with no workflow adapter and no default must not fire"
        );

        let with_default = auto_spawn_decision(&no_adapter, true, true, Some("codex"))
            .expect("Review with no workflow adapter falls back to the operator default");
        assert_eq!(with_default.argv[5], "codex");

        let test = auto_spawn_decision(
            &at_phase(state.clone(), WorkflowPhase::Test),
            true,
            true,
            None,
        )
        .expect("Test fires");
        assert_eq!(test.phase, WorkflowPhase::Test);
        assert_eq!(
            test.mode,
            crate::commands::ctx::permit::WorkerMode::Writing,
            "issue #371: a test spawn needs to write build artifacts and caches"
        );
        assert_eq!(
            test.task_class,
            crate::commands::ctx::log::TaskClass::Test,
            "issue #264: a test-phase auto-spawn is classified as test"
        );
        assert_eq!(
            test.argv,
            vec![
                "test",
                "changed",
                "--repo",
                &state.repo.display().to_string()
            ]
        );

        let verify = auto_spawn_decision(
            &at_phase(state.clone(), WorkflowPhase::Verify),
            true,
            true,
            None,
        )
        .expect("Verify fires");
        assert_eq!(verify.phase, WorkflowPhase::Verify);
        assert_eq!(
            verify.mode,
            crate::commands::ctx::permit::WorkerMode::Writing,
            "issue #371: a verify spawn needs to write build artifacts and caches"
        );
        assert_eq!(
            verify.task_class,
            crate::commands::ctx::log::TaskClass::Test,
            "issue #264: a verify-phase auto-spawn is classified as test"
        );
        assert_eq!(
            verify.argv,
            vec!["verify", "--repo", &state.repo.display().to_string()]
        );
    }

    /// `Quiet` is the only skip that stays silent -- the config-disabled
    /// path, or a phase auto-spawn was never meant to touch. `NoPermit`/
    /// `NoAgent` are eligible-but-skipped, so an operator who turned the
    /// key on must see why.
    #[test]
    fn auto_spawn_skip_reason_is_silent_only_for_quiet() {
        assert_eq!(auto_spawn_skip_reason(AutoSpawnSkip::Quiet), None);
        assert!(auto_spawn_skip_reason(AutoSpawnSkip::NoPermit).is_some());
        assert!(auto_spawn_skip_reason(AutoSpawnSkip::NoAgent).is_some());
    }

    /// Issue #264: `advance_with_evidence`'s own `PhaseCompleted` event
    /// carries `parent_session_id` (read fresh from `agent::PARENT_SESSION_
    /// ENV`, never invented) and `cost_micros`/`price_as_of` (priced from
    /// `evidence.model` plus its token counts) -- the two attribution fields
    /// the cost ledger needs to answer "who spent this, and how much".
    #[test]
    fn advance_with_evidence_records_parent_session_and_cost_on_its_telemetry_event() {
        let repo = tempdir().unwrap();
        let root = tempdir().unwrap();
        let state_dir = StateDir::from_root(root.path().to_path_buf());
        let _vars = crate::commands::ctx::testenv::VarGuard::set(&[(
            crate::commands::ctx::agent::PARENT_SESSION_ENV,
            Some("aaaa1111"),
        )]);
        let state = skip_leading_artifact_steps(WorkflowState::start(
            repo.path().to_path_buf(),
            "small feature".into(),
            WorkflowKind::Feature,
            None,
            true,
            low_classification(),
        ));
        save(&state_dir, &state, true).unwrap();

        let evidence = TransitionEvidence {
            model: Some("sonnet".to_string()),
            // Combined context total 1_000_000 = 1_000_000 raw input, no
            // cache -- 1_000_000 tokens @ $3/M (sonnet) = $3.00 = 3_000_000
            // micros.
            input_tokens: Some(1_000_000),
            output_tokens: Some(0),
            ..Default::default()
        };
        let advanced = advance_with_evidence(
            &state_dir,
            state,
            StepOutcome::Success,
            Some(&evidence),
            false,
        )
        .unwrap();

        let events = crate::commands::workflow::telemetry::list(&state_dir, &advanced.repo)
            .unwrap_or_default();
        let phase_completed = events
            .iter()
            .find(|event| {
                event.kind == crate::commands::workflow::telemetry::TelemetryKind::PhaseCompleted
            })
            .expect("a PhaseCompleted event was recorded");
        assert_eq!(
            phase_completed.parent_session_id.as_deref(),
            Some("aaaa1111"),
            "parent_session_id must be read from PARENT_SESSION_ENV, never left None when set"
        );
        assert_eq!(phase_completed.cost_micros, Some(3_000_000));
        assert!(phase_completed.price_as_of.is_some());
    }

    /// Issue #349: chained design-gate steps (`intent` -> `spec`, both
    /// approval-gated for a high-risk classification -- the same fixture
    /// `reclassify_preserves_completed_steps_and_accepted_artifacts`, above,
    /// already relies on) prove both halves of the `approve()` wiring in one
    /// pass: approving `intent` lands on `spec`, which is ALSO gated, so
    /// `WorkflowGate` must still be set; approving `spec` lands on `plan`,
    /// which is not, so that second approval must clear it to `None`.
    #[test]
    fn approve_wires_workflow_gate_attention_across_chained_design_gates() {
        let repo = tempdir().unwrap();
        let root = tempdir().unwrap();
        let state_dir = StateDir::from_root(root.path().to_path_buf());
        let session_id = "ffff6666aaaa4bbb8cccdddddddddddd";
        let _vars = crate::commands::ctx::testenv::VarGuard::set(&[
            (
                crate::commands::ctx::adapters::SESSION_ENV,
                Some(session_id),
            ),
            (
                "ZIRV_CTX_STATE_DIR",
                Some(root.path().to_str().expect("utf-8 tempdir path")),
            ),
        ]);
        let mut classification = low_classification();
        classification.complexity = Complexity::Bounded;
        classification.risk = RiskBand::High;
        let state = WorkflowState::start(
            repo.path().to_path_buf(),
            "bounded high-risk feature".into(),
            WorkflowKind::Feature,
            None,
            true,
            classification,
        );
        assert_eq!(state.status, WorkflowStatus::AwaitingApproval);
        assert_eq!(state.current().unwrap().id, "intent");
        let short = crate::commands::ctx::sessions::short_id(session_id);

        ensure_current_artifact_template(&state).unwrap();
        std::fs::write(
            workflow_artifact_path(&state, ArtifactStage::Intent).unwrap(),
            "# Intent\n\n## Problem\nConcrete problem\n\n## Desired outcome\nConcrete result\n",
        )
        .unwrap();
        let state = approve(&state_dir, state).expect("approve intent");
        assert_eq!(state.current().unwrap().id, "spec");
        assert_eq!(
            state.status,
            WorkflowStatus::AwaitingApproval,
            "spec is itself design-gated for a high-risk classification"
        );
        let status = crate::commands::ctx::attention::load(&state_dir, &short);
        assert_eq!(
            status.attention,
            crate::commands::ctx::attention::Attention::WorkflowGate,
            "approving intent must not clear the gate while spec is still pending approval"
        );

        ensure_current_artifact_template(&state).unwrap();
        std::fs::write(
            workflow_artifact_path(&state, ArtifactStage::Spec).unwrap(),
            "# Specification\n\n## Context\nReal context\n\n## Goals\n- ship it\n",
        )
        .unwrap();
        let state = approve(&state_dir, state).expect("approve spec");
        assert_eq!(state.current().unwrap().id, "plan");
        assert_eq!(
            state.status,
            WorkflowStatus::AwaitingApproval,
            "plan is itself design-gated too for this classification"
        );
        let status = crate::commands::ctx::attention::load(&state_dir, &short);
        assert_eq!(
            status.attention,
            crate::commands::ctx::attention::Attention::WorkflowGate,
            "approving spec must not clear the gate while plan is still pending approval"
        );

        ensure_current_artifact_template(&state).unwrap();
        std::fs::write(
            workflow_artifact_path(&state, ArtifactStage::Plan).unwrap(),
            "# Plan\n\n## Steps\n1. Ship it\n\n## Risks\nNone material\n",
        )
        .unwrap();
        let state = approve(&state_dir, state).expect("approve plan");
        assert_eq!(state.current().unwrap().id, "implement");
        assert_eq!(
            state.status,
            WorkflowStatus::Running,
            "implement is not gated, so this approval must finally clear it"
        );
        let status = crate::commands::ctx::attention::load(&state_dir, &short);
        assert_eq!(
            status.attention,
            crate::commands::ctx::attention::Attention::None,
            "implement is not gated, so this approval must clear the WorkflowGate attention"
        );
    }

    /// Issue #349: a Test-phase advance with no fresh passing verification
    /// evidence on disk at all is exactly the gate rejection this attention
    /// source targets.
    #[test]
    fn advance_with_evidence_records_verification_failure_attention_on_a_stale_test_gate() {
        let repo = tempdir().unwrap();
        let root = tempdir().unwrap();
        let state_dir = StateDir::from_root(root.path().to_path_buf());
        let session_id = "eeee5555aaaa4bbb8cccdddddddddddd";
        let _vars = crate::commands::ctx::testenv::VarGuard::set(&[
            (
                crate::commands::ctx::adapters::SESSION_ENV,
                Some(session_id),
            ),
            (
                "ZIRV_CTX_STATE_DIR",
                Some(root.path().to_str().expect("utf-8 tempdir path")),
            ),
        ]);
        let mut state = skip_leading_artifact_steps(WorkflowState::start(
            repo.path().to_path_buf(),
            "small feature".into(),
            WorkflowKind::Feature,
            None,
            true,
            low_classification(),
        ));
        let test_index = state
            .steps
            .iter()
            .position(|step| step.phase == WorkflowPhase::Test)
            .expect("fixture has a Test step");
        state.completed_steps = state.steps[..test_index]
            .iter()
            .map(|step| step.id.clone())
            .collect();
        state.current_step = test_index;
        state.status = WorkflowStatus::Running;
        save(&state_dir, &state, true).unwrap();

        let result = advance_with_evidence(&state_dir, state, StepOutcome::Success, None, false);
        assert!(
            result.is_err(),
            "no verification evidence has ever been stored, so the gate must reject"
        );

        let short = crate::commands::ctx::sessions::short_id(session_id);
        let status = crate::commands::ctx::attention::load(&state_dir, &short);
        assert_eq!(
            status.attention,
            crate::commands::ctx::attention::Attention::VerificationFailure
        );
    }

    #[test]
    fn advance_with_evidence_records_no_agent_dispatched_event_when_auto_spawn_is_disabled() {
        let repo = tempdir().unwrap();
        let root = tempdir().unwrap();
        let state_dir = StateDir::from_root(root.path().to_path_buf());
        let state = skip_leading_artifact_steps(WorkflowState::start(
            repo.path().to_path_buf(),
            "small feature".into(),
            WorkflowKind::Feature,
            None,
            true,
            low_classification(),
        ));
        save(&state_dir, &state, true).unwrap();

        let advanced =
            advance_with_evidence(&state_dir, state, StepOutcome::Success, None, false).unwrap();
        assert_eq!(advanced.current().unwrap().phase, WorkflowPhase::Test);

        let events = crate::commands::workflow::telemetry::list(&state_dir, &advanced.repo)
            .unwrap_or_default();
        assert!(
            !events.iter().any(|event| {
                event.kind == crate::commands::workflow::telemetry::TelemetryKind::AgentDispatched
            }),
            "auto_spawn_on_gate defaults to false; advance must never record a dispatch"
        );
    }

    #[test]
    fn tightening_to_production_rewinds_later_completed_evidence() {
        // `apply_effective_deploy_tier` re-materializes `steps` straight from
        // `state.classification`, so the leading artifact step below must
        // survive that regeneration rather than being injected by hand.
        // Bounded complexity gates Feature's own intent step in
        // (`ComplexityOrRisk{Bounded, Medium}`) but also gates its plan step
        // in (`ComplexityOrRisk{Bounded, High}`, the same `Bounded` bar), so
        // both now precede implement.
        let mut classification = low_classification();
        classification.complexity = Complexity::Bounded;
        let mut state = WorkflowState::start(
            PathBuf::from("repo"),
            "small feature".into(),
            WorkflowKind::Feature,
            None,
            true,
            classification,
        );
        state.completed_steps = vec!["intent", "plan", "implement", "test", "verify"]
            .into_iter()
            .map(str::to_string)
            .collect();
        state.current_step = state
            .steps
            .iter()
            .position(|step| step.phase == WorkflowPhase::Deploy)
            .unwrap();
        state.status = WorkflowStatus::Running;

        apply_effective_deploy_tier(&mut state, DeployTier::Production);

        assert_eq!(state.deploy_tier, DeployTier::Production);
        assert_eq!(state.current().unwrap().phase, WorkflowPhase::Review);
        assert_eq!(
            state.completed_steps,
            ["intent", "plan", "implement", "test"],
            "verify evidence after the inserted production review must be replayed"
        );
    }

    /// Issue #542 review finding 15: a deploy-tier escalation can invalidate
    /// (un-complete) a step the same way `reopen_artifact_gate`'s rewind
    /// does -- see `tightening_to_production_rewinds_later_completed_
    /// evidence`, which this test otherwise mirrors exactly. A stale
    /// `current_step_approved` recorded for a step this escalation just
    /// un-completed must not survive it, or `step_requires_approval` would
    /// treat that step as already granted the next time it is reached.
    #[test]
    fn tightening_to_production_clears_a_stale_gate_only_approval_for_a_rewound_step() {
        let mut classification = low_classification();
        classification.complexity = Complexity::Bounded;
        let mut state = WorkflowState::start(
            PathBuf::from("repo"),
            "small feature".into(),
            WorkflowKind::Feature,
            None,
            true,
            classification,
        );
        state.completed_steps = vec!["intent", "plan", "implement", "test", "verify"]
            .into_iter()
            .map(str::to_string)
            .collect();
        state.current_step = state
            .steps
            .iter()
            .position(|step| step.phase == WorkflowPhase::Deploy)
            .unwrap();
        state.status = WorkflowStatus::Running;
        state.current_step_approved = Some("verify".to_string());

        apply_effective_deploy_tier(&mut state, DeployTier::Production);

        assert_eq!(
            state.completed_steps,
            ["intent", "plan", "implement", "test"],
        );
        assert_eq!(
            state.current_step_approved, None,
            "a stale gate-only approval for a step this escalation un-completed must be cleared"
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
    fn brainstorm_flags_are_mutually_exclusive_and_resolve_to_an_override() {
        use clap::Parser;
        #[derive(clap::Parser)]
        struct Cli {
            #[command(flatten)]
            args: StartArgs,
        }
        let plain =
            Cli::try_parse_from(["zirv", "feature", "--task", "x"]).expect("no flags parses");
        assert_eq!(plain.args.brainstorm_override(), None);

        let on = Cli::try_parse_from(["zirv", "feature", "--task", "x", "--brainstorm"])
            .expect("--brainstorm parses");
        assert_eq!(on.args.brainstorm_override(), Some(true));

        let off = Cli::try_parse_from(["zirv", "feature", "--task", "x", "--no-brainstorm"])
            .expect("--no-brainstorm parses");
        assert_eq!(off.args.brainstorm_override(), Some(false));

        assert!(
            Cli::try_parse_from([
                "zirv",
                "feature",
                "--task",
                "x",
                "--brainstorm",
                "--no-brainstorm",
            ])
            .is_err(),
            "both flags together must be refused"
        );
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
        use super::super::definition::{
            CompletionContract, EffectClass, EscalateTo, FailurePolicy, GateSpec, Limits,
            PresentAs, StepV2, WorkflowDefinitionV2,
        };
        let definition = WorkflowDefinitionV2 {
            schema_version: super::super::definition::DEFINITION_SCHEMA_VERSION,
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
    fn frontend_test_step_fails_closed_without_detector_evidence() {
        let repo = tempdir().unwrap();
        let root = tempdir().unwrap();
        let state_dir = StateDir::from_root(root.path().to_path_buf());
        let mut classification = low_classification();
        classification.work_domain.domain = WorkDomain::Frontend;
        classification.work_domain.score = 55;
        let mut state = WorkflowState::start(
            repo.path().to_path_buf(),
            "build a frontend component".into(),
            WorkflowKind::Feature,
            None,
            true,
            classification,
        );
        let test_index = state
            .steps
            .iter()
            .position(|step| step.phase == WorkflowPhase::Test)
            .unwrap();
        state.completed_steps = state.steps[..test_index]
            .iter()
            .map(|step| step.id.clone())
            .collect();
        state.current_step = test_index;
        state.status = WorkflowStatus::Running;

        let error = advance_with_evidence(&state_dir, state, StepOutcome::Success, None, false)
            .unwrap_err()
            .to_string();

        assert!(
            error.contains("automatically ran the detector")
                || error.contains("cannot inspect changed paths"),
            "{error}"
        );
    }

    #[test]
    fn frontend_gate_uses_frontend_target_root_when_set() {
        // #214: the workflow is tracked in `workflow_repo`, but the real
        // frontend under test lives in a sibling `target_repo` -- the
        // detector must scan `frontend_target_root`, not `state.repo`, once
        // it is set.
        let workflow_repo = tempdir().unwrap();
        let target_repo = tempdir().unwrap();
        let root = tempdir().unwrap();
        let state_dir = StateDir::from_root(root.path().to_path_buf());

        let git = |dir: &std::path::Path, args: &[&str]| {
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
                .current_dir(dir)
                .status()
                .expect("run git");
            assert!(status.success(), "git {args:?} failed in {}", dir.display());
        };
        // Both repos are real git checkouts, so `changed_paths_since_base`
        // resolves cleanly instead of surfacing an unrelated "not a git
        // repository" error that would mask what this test is about.
        git(workflow_repo.path(), &["init", "-q"]);
        std::fs::write(workflow_repo.path().join("README.md"), "readme\n").unwrap();
        git(workflow_repo.path(), &["add", "."]);
        git(workflow_repo.path(), &["commit", "-q", "-m", "base"]);
        // #255: an empty scan is now a pass ("not applicable"), so this repo
        // needs a real, introduced blocking finding in scope -- not just an
        // absence of frontend files -- to still demonstrate the wrong repo
        // being scanned when `--frontend-root` is missing.
        std::fs::write(
            workflow_repo.path().join("Bad.tsx"),
            "export const Bad = () => <img src={avatar} />;\n",
        )
        .unwrap();

        git(target_repo.path(), &["init", "-q"]);
        std::fs::write(target_repo.path().join("README.md"), "readme\n").unwrap();
        git(target_repo.path(), &["add", "."]);
        git(target_repo.path(), &["commit", "-q", "-m", "base"]);
        // A minimal, clean stylesheet: no images, semantic-action targets,
        // gradients, motion, or viewport hazards, so the detector should
        // report zero blocking findings for it.
        std::fs::write(
            target_repo.path().join("style.css"),
            ".card { color: rebeccapurple; }\n",
        )
        .unwrap();

        let mut classification = low_classification();
        classification.work_domain.domain = WorkDomain::Frontend;
        classification.work_domain.score = 55;

        let build_state = |classification: Classification| {
            let mut state = WorkflowState::start(
                workflow_repo.path().to_path_buf(),
                "build a frontend component".into(),
                WorkflowKind::Feature,
                None,
                true,
                classification,
            );
            let test_index = state
                .steps
                .iter()
                .position(|step| step.phase == WorkflowPhase::Test)
                .unwrap();
            state.completed_steps = state.steps[..test_index]
                .iter()
                .map(|step| step.id.clone())
                .collect();
            state.current_step = test_index;
            state.status = WorkflowStatus::Running;
            state
        };

        // Without a frontend target root, the gate fails closed scanning the
        // frontend-less workflow repo -- same failure mode as #214.
        let without_root = advance_with_evidence(
            &state_dir,
            build_state(classification.clone()),
            StepOutcome::Success,
            None,
            false,
        )
        .unwrap_err()
        .to_string();
        assert!(
            without_root.contains("automatically ran the detector"),
            "{without_root}"
        );

        // With the frontend target root pointed at the sibling repo, the
        // detector gate must pass -- execution proceeds past it to the
        // unrelated general test-evidence gate, which `workflow_repo` has no
        // recorded evidence for.
        let mut state = build_state(classification);
        state.frontend_target_root = Some(target_repo.path().canonicalize().unwrap());
        let with_root = advance_with_evidence(&state_dir, state, StepOutcome::Success, None, false)
            .unwrap_err()
            .to_string();
        assert!(
            !with_root.contains("automatically ran the detector"),
            "{with_root}"
        );
        assert!(
            with_root.contains("requires fresh passing evidence"),
            "{with_root}"
        );
    }

    /// #255: a repository with zero frontend-extension files in scope is
    /// "frontend gate not applicable", not missing evidence to fail closed
    /// on -- the old `report.analyzed_files.is_empty()` check made a
    /// Frontend-profile workflow over a backend-only change unable to ever
    /// pass its Test step.
    #[test]
    fn frontend_test_step_passes_with_zero_frontend_files_in_the_change_surface() {
        let repo = tempdir().unwrap();
        let root = tempdir().unwrap();
        let state_dir = StateDir::from_root(root.path().to_path_buf());
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
                .expect("run git");
            assert!(status.success(), "git {args:?} failed");
        };
        git(&["init", "-q"]);
        std::fs::write(repo.path().join("README.md"), "hello\n").unwrap();
        git(&["add", "."]);
        git(&["commit", "-q", "-m", "base"]);

        let mut classification = low_classification();
        classification.work_domain.domain = WorkDomain::Frontend;
        classification.work_domain.score = 55;
        let mut state = WorkflowState::start(
            repo.path().to_path_buf(),
            "build a frontend component".into(),
            WorkflowKind::Feature,
            None,
            true,
            classification,
        );
        let test_index = state
            .steps
            .iter()
            .position(|step| step.phase == WorkflowPhase::Test)
            .unwrap();
        state.completed_steps = state.steps[..test_index]
            .iter()
            .map(|step| step.id.clone())
            .collect();
        state.current_step = test_index;
        state.status = WorkflowStatus::Running;

        // Seed passing general test evidence so the ONLY thing under test
        // here is the frontend gate's handling of an empty (0 frontend
        // files) scope, not the unrelated `zirv test changed` gate.
        let fingerprint = super::super::verification::change_fingerprint(repo.path()).unwrap();
        let evidence_report = super::super::verification::VerificationReport {
            schema_version: super::super::verification::VERIFY_REPORT_SCHEMA_VERSION,
            id: "seeded".into(),
            mode: super::super::verification::VerificationMode::Changed,
            source: "configured".into(),
            repo: repo.path().to_path_buf(),
            branch: String::new(),
            head_sha: String::new(),
            change_fingerprint: fingerprint,
            changed_paths: vec![],
            fallback_to_full: false,
            narrowed_to: vec![],
            notes: vec![],
            started_at: 0,
            finished_at: 0,
            checks: vec![super::super::verification::CheckResult {
                id: "unit".into(),
                kind: super::super::verification::CheckKind::Unit,
                command: "true".into(),
                source: super::super::verification::CheckSource::DiscoveredToolchain,
                status: super::super::verification::CheckStatus::Passed,
                exit_code: Some(0),
                duration_ms: 1,
                failure_output: None,
                failure_test_names: Vec::new(),
                inconclusive_reason: None,
            }],
        };
        super::super::verification::save_report(&state_dir, &evidence_report).unwrap();

        let advanced = advance_with_evidence(&state_dir, state, StepOutcome::Success, None, false)
            .expect("zero frontend files in scope must not fail the frontend gate");
        assert_eq!(advanced.current().unwrap().phase, WorkflowPhase::Verify);
    }

    /// Issue #467, acceptance 1: a workflow started in the main checkout
    /// must accept `zirv test changed` evidence recorded in a linked `git
    /// worktree add` sibling of it. The main checkout's own tree stays
    /// clean while the real work happens in the worktree, so the evidence's
    /// change fingerprint can only ever be computed from the worktree, never
    /// from `state.repo` as originally started -- before #467 this gate
    /// always rejected with "requires fresh passing evidence for the
    /// current change set" because it fingerprinted the clean main checkout.
    #[test]
    fn advance_accepts_test_changed_evidence_recorded_in_a_linked_worktree() {
        let main_repo = tempdir().unwrap();
        let root = tempdir().unwrap();
        let state_dir = StateDir::from_root(root.path().to_path_buf());
        let git = |dir: &std::path::Path, args: &[&str]| {
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
                .current_dir(dir)
                .status()
                .expect("run git");
            assert!(status.success(), "git {args:?} failed");
        };
        git(main_repo.path(), &["init", "-q"]);
        std::fs::write(main_repo.path().join("README.md"), "hello\n").unwrap();
        git(main_repo.path(), &["add", "."]);
        git(main_repo.path(), &["commit", "-q", "-m", "base"]);

        // A linked worktree on its own feature branch -- the main checkout
        // stays exactly at "base", untouched.
        let worktree_dir = tempdir().unwrap();
        let worktree_path = worktree_dir.path().to_path_buf();
        std::fs::remove_dir(&worktree_path).unwrap();
        git(
            main_repo.path(),
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                "feature",
                worktree_path.to_str().unwrap(),
            ],
        );
        std::fs::write(worktree_path.join("feature.rs"), "fn feature() {}\n").unwrap();
        git(&worktree_path, &["add", "."]);
        git(&worktree_path, &["commit", "-q", "-m", "feature work"]);

        // Workflow tracked from the main checkout, as `zirv workflow start`
        // ran there -- unchanged from every other test in this module.
        let mut state = WorkflowState::start(
            main_repo.path().to_path_buf(),
            "small feature".into(),
            WorkflowKind::Feature,
            None,
            true,
            low_classification(),
        );
        let test_index = state
            .steps
            .iter()
            .position(|step| step.phase == WorkflowPhase::Test)
            .expect("fixture has a Test step");
        state.completed_steps = state.steps[..test_index]
            .iter()
            .map(|step| step.id.clone())
            .collect();
        state.current_step = test_index;
        state.status = WorkflowStatus::Running;
        save(&state_dir, &state, true).unwrap();

        // `zirv test changed`, run inside the worktree: evidence
        // fingerprinted against the worktree's own tree, where the real
        // change lives.
        let fingerprint = super::super::verification::change_fingerprint(&worktree_path).unwrap();
        let evidence_report = super::super::verification::VerificationReport {
            schema_version: super::super::verification::VERIFY_REPORT_SCHEMA_VERSION,
            id: "worktree-evidence".into(),
            mode: super::super::verification::VerificationMode::Changed,
            source: "configured".into(),
            repo: worktree_path.clone(),
            branch: String::new(),
            head_sha: String::new(),
            change_fingerprint: fingerprint,
            changed_paths: vec![],
            fallback_to_full: false,
            narrowed_to: vec![],
            notes: vec![],
            started_at: 0,
            finished_at: 0,
            checks: vec![super::super::verification::CheckResult {
                id: "unit".into(),
                kind: super::super::verification::CheckKind::Unit,
                command: "true".into(),
                source: super::super::verification::CheckSource::DiscoveredToolchain,
                status: super::super::verification::CheckStatus::Passed,
                exit_code: Some(0),
                duration_ms: 1,
                failure_output: None,
                failure_test_names: Vec::new(),
                inconclusive_reason: None,
            }],
        };
        super::super::verification::save_report(&state_dir, &evidence_report).unwrap();

        // `zirv workflow advance <id> --outcome success --repo <worktree>`:
        // `load` (fixed for #467) resolves the SAME workflow through the
        // worktree's path and retargets `state.repo` to it, so the gate
        // below measures the worktree's own change set -- where the
        // evidence just persisted actually lives -- not the clean main
        // checkout `state.repo` was started with.
        let loaded = load(&state_dir, &worktree_path, &state.id)
            .expect("a linked worktree must resolve the workflow the main checkout started");
        assert_eq!(loaded.repo, worktree_path);

        let advanced = advance_with_evidence(&state_dir, loaded, StepOutcome::Success, None, false)
            .expect("evidence recorded in the linked worktree must satisfy the Test gate");
        assert_eq!(advanced.current().unwrap().phase, WorkflowPhase::Verify);
    }

    /// Issue #484 acceptance 2 (roadmap N15), standing on #467's mechanism:
    /// a workflow STARTED IN THE MAIN CHECKOUT accepts the worker worktree's
    /// evidence for its own change set and rejects evidence for a different
    /// one -- proven through the gate a native session is actually stopped by
    /// (`native_completion_gate`) as well as through `advance_with_evidence`,
    /// so the two cannot drift apart.
    ///
    /// The main checkout's own report directory stays empty throughout: the
    /// only way either assertion can pass is the widened, sibling-checkout
    /// read, and the only thing that makes that read safe is the branch
    /// relatedness key. Recording the same evidence against a DIFFERENT
    /// branch -- another worker's work, in another worktree -- must leave the
    /// gate shut.
    #[test]
    fn a_main_checkout_workflow_accepts_only_its_own_worktrees_evidence() {
        let git = |dir: &std::path::Path, args: &[&str]| {
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
                .current_dir(dir)
                .status()
                .expect("run git");
            assert!(status.success(), "git {args:?} failed");
        };

        // `evidence_branch` is what the worker's `zirv test changed` recorded
        // its report against; the workflow itself always gates "feature".
        let scenario = |evidence_branch: &str| {
            let main_repo = tempdir().unwrap();
            let root = tempdir().unwrap();
            let state_dir = StateDir::from_root(root.path().to_path_buf());
            git(main_repo.path(), &["init", "-q"]);
            std::fs::write(
                main_repo.path().join("README.md"),
                "hello
",
            )
            .unwrap();
            git(main_repo.path(), &["add", "."]);
            git(main_repo.path(), &["commit", "-q", "-m", "base"]);

            let worktree_dir = tempdir().unwrap();
            let worktree_path = worktree_dir.path().to_path_buf();
            std::fs::remove_dir(&worktree_path).unwrap();
            git(
                main_repo.path(),
                &[
                    "worktree",
                    "add",
                    "-q",
                    "-b",
                    "feature",
                    worktree_path.to_str().unwrap(),
                ],
            );
            std::fs::write(
                worktree_path.join("feature.rs"),
                "fn feature() {}
",
            )
            .unwrap();
            git(&worktree_path, &["add", "."]);
            git(&worktree_path, &["commit", "-q", "-m", "feature work"]);

            let mut state = WorkflowState::start(
                main_repo.path().to_path_buf(),
                "small feature".into(),
                WorkflowKind::Feature,
                None,
                true,
                low_classification(),
            );
            state.branch = "feature".into();
            let test_index = state
                .steps
                .iter()
                .position(|step| step.phase == WorkflowPhase::Test)
                .expect("fixture has a Test step");
            state.completed_steps = state.steps[..test_index]
                .iter()
                .map(|step| step.id.clone())
                .collect();
            state.current_step = test_index;
            state.status = WorkflowStatus::Running;
            // `save(.., active = true)` writes the main checkout's own
            // active-workflow pointer, which is what `native_completion_gate`
            // resolves through.
            save(&state_dir, &state, true).unwrap();

            let fingerprint =
                super::super::verification::change_fingerprint(&worktree_path).unwrap();
            let report = super::super::verification::VerificationReport {
                schema_version: super::super::verification::VERIFY_REPORT_SCHEMA_VERSION,
                id: "worktree-evidence".into(),
                mode: super::super::verification::VerificationMode::Changed,
                source: "configured".into(),
                repo: worktree_path.clone(),
                branch: evidence_branch.to_string(),
                head_sha: String::new(),
                change_fingerprint: fingerprint,
                changed_paths: vec![],
                fallback_to_full: false,
                narrowed_to: vec![],
                notes: vec![],
                started_at: 0,
                finished_at: 0,
                checks: vec![super::super::verification::CheckResult {
                    id: "unit".into(),
                    kind: super::super::verification::CheckKind::Unit,
                    command: "true".into(),
                    source: super::super::verification::CheckSource::DiscoveredToolchain,
                    status: super::super::verification::CheckStatus::Passed,
                    exit_code: Some(0),
                    duration_ms: 1,
                    failure_output: None,
                    failure_test_names: Vec::new(),
                    inconclusive_reason: None,
                }],
            };
            super::super::verification::save_report(&state_dir, &report).unwrap();
            // `root` travels with the rest: dropping it would delete the state
            // directory `state_dir` only holds a PATH to, and every assertion
            // below would then pass vacuously against an empty store.
            (main_repo, worktree_dir, root, state_dir, state)
        };

        // Accepted: the worker's evidence names the workflow's own branch.
        let (main_repo, _worktree, _root, state_dir, state) = scenario("feature");
        assert_eq!(
            native_completion_gate(&state_dir, main_repo.path()),
            None,
            "the worker worktree's evidence for this change set must open the gate evaluated from the main checkout"
        );
        let loaded = load(&state_dir, main_repo.path(), &state.id).unwrap();
        let advanced = advance_with_evidence(&state_dir, loaded, StepOutcome::Success, None, false)
            .expect("the same evidence must satisfy the advance gate");
        assert_eq!(advanced.current().unwrap().phase, WorkflowPhase::Verify);

        // Rejected: fresh, passing, and about somebody else's change set.
        let (main_repo, _worktree, _root, state_dir, state) = scenario("someone-elses-feature");
        let blocked = native_completion_gate(&state_dir, main_repo.path())
            .expect("unrelated evidence must leave the gate shut");
        assert!(
            blocked.contains("no fresh passing evidence"),
            "the gate must say why: {blocked}"
        );
        let loaded = load(&state_dir, main_repo.path(), &state.id).unwrap();
        assert!(
            advance_with_evidence(&state_dir, loaded, StepOutcome::Success, None, false).is_err(),
            "an unrelated worktree's evidence must never advance this workflow"
        );
    }

    /// Issue #599 (roadmap N15): `native_completion_gate` used to read as
    /// `latest_is_fresh_and_passing(..).unwrap_or(true)` -- any error reading
    /// the persisted verification record (missing permissions, corruption,
    /// any other read failure) was treated as "fresh and passing" and opened
    /// the gate. Corrupts the record directly (invalid JSON behind a valid
    /// `latest` pointer) rather than through `save_report`, so the gate hits
    /// a genuine read error rather than "no evidence yet" (which correctly
    /// stays a normal, worded "no fresh passing evidence" block, not this
    /// one).
    #[test]
    fn workflow_completion_refuses_unreadable_verification_evidence() {
        let repo = tempdir().unwrap();
        let root = tempdir().unwrap();
        let state_dir = StateDir::from_root(root.path().to_path_buf());

        let mut state = WorkflowState::start(
            repo.path().to_path_buf(),
            "small feature".into(),
            WorkflowKind::Feature,
            None,
            true,
            low_classification(),
        );
        let test_index = state
            .steps
            .iter()
            .position(|step| step.phase == WorkflowPhase::Test)
            .expect("fixture has a Test step");
        state.completed_steps = state.steps[..test_index]
            .iter()
            .map(|step| step.id.clone())
            .collect();
        state.current_step = test_index;
        state.status = WorkflowStatus::Running;
        save(&state_dir, &state, true).unwrap();

        let report_dir = state_dir.verification().join(repo_slug(repo.path()));
        create_private_dir_all(&report_dir).unwrap();
        write_private(&report_dir.join("corrupt.json"), "not valid json").unwrap();
        write_private(&report_dir.join("latest"), "corrupt.json").unwrap();

        let blocked = native_completion_gate(&state_dir, repo.path())
            .expect("an unreadable verification record must block completion, not silently pass");
        assert!(
            blocked.contains("could not read its verification evidence"),
            "the gate must surface the evidence read error, not just say evidence is missing or stale: {blocked}"
        );
    }

    /// Issue #467, acceptance 2: `zirv workflow status|advance|review
    /// package <id> --repo <worktree>` must find a workflow started (and
    /// tracked) from the main checkout -- both by id (`load`, what
    /// `status <id>`, `advance` and `review package` all go through) and via
    /// the active-workflow pointer (`load_active`, what bare `zirv workflow
    /// status` -- run from inside the worktree -- goes through). Before
    /// #467 both returned "unknown workflow"/"no active workflow": the main
    /// checkout and the worktree keyed two different, unrelated state
    /// directories under plain `repo_slug`.
    #[test]
    fn workflow_started_in_the_main_checkout_is_found_from_a_linked_worktree() {
        let main_repo = tempdir().unwrap();
        let root = tempdir().unwrap();
        let state_dir = StateDir::from_root(root.path().to_path_buf());
        let git = |dir: &std::path::Path, args: &[&str]| {
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
                .current_dir(dir)
                .status()
                .expect("run git");
            assert!(status.success(), "git {args:?} failed");
        };
        git(main_repo.path(), &["init", "-q"]);
        std::fs::write(main_repo.path().join("README.md"), "hello\n").unwrap();
        git(main_repo.path(), &["add", "."]);
        git(main_repo.path(), &["commit", "-q", "-m", "base"]);

        let worktree_dir = tempdir().unwrap();
        let worktree_path = worktree_dir.path().to_path_buf();
        std::fs::remove_dir(&worktree_path).unwrap();
        git(
            main_repo.path(),
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                "feature",
                worktree_path.to_str().unwrap(),
            ],
        );

        let state = WorkflowState::start(
            main_repo.path().to_path_buf(),
            "small feature".into(),
            WorkflowKind::Feature,
            None,
            true,
            low_classification(),
        );
        save(&state_dir, &state, true).unwrap();

        let by_id = load(&state_dir, &worktree_path, &state.id);
        assert!(
            by_id.is_ok(),
            "a linked worktree of the started repo must resolve the workflow by id: {:?}",
            by_id.err()
        );
        assert_eq!(by_id.unwrap().id, state.id);

        let active = load_active(&state_dir, &worktree_path).unwrap();
        assert!(
            active.is_some(),
            "a linked worktree must also see the started repo's active-workflow pointer"
        );
        assert_eq!(active.unwrap().id, state.id);
    }

    /// Issue #467 round 3 (Finding 1): workflow state and the active
    /// pointer are keyed by the LITERAL checkout, not a shared identity --
    /// two unrelated `zirv workflow start` runs in two DIFFERENT worker
    /// worktrees of the same repository must never collide. A third
    /// sibling with no active workflow of its own must still see the MAIN
    /// checkout's (never a's or b's), matching "a worker worktree with no
    /// workflow of its own inherits the orchestrator's".
    #[test]
    fn sibling_worktrees_each_resolve_their_own_active_workflow() {
        let main_repo = tempdir().unwrap();
        let state_dir = StateDir::from_root(tempdir().unwrap().path().to_path_buf());
        let git = |dir: &std::path::Path, args: &[&str]| {
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
                .current_dir(dir)
                .status()
                .expect("run git");
            assert!(status.success(), "git {args:?} failed");
        };
        git(main_repo.path(), &["init", "-q"]);
        std::fs::write(main_repo.path().join("README.md"), "hello\n").unwrap();
        git(main_repo.path(), &["add", "."]);
        git(main_repo.path(), &["commit", "-q", "-m", "base"]);

        let mut worktree_paths = Vec::new();
        for name in ["worker-a", "worker-b", "worker-c"] {
            let dir = tempdir().unwrap();
            let path = dir.path().to_path_buf();
            std::fs::remove_dir(&path).unwrap();
            git(
                main_repo.path(),
                &["worktree", "add", "-q", "-b", name, path.to_str().unwrap()],
            );
            worktree_paths.push((dir, path));
        }
        let worktree_a = &worktree_paths[0].1;
        let worktree_b = &worktree_paths[1].1;
        let worktree_c = &worktree_paths[2].1;

        // The orchestrator's own workflow, started in the main checkout.
        let main_state = WorkflowState::start(
            main_repo.path().to_path_buf(),
            "orchestrator work".into(),
            WorkflowKind::Feature,
            None,
            true,
            low_classification(),
        );
        save(&state_dir, &main_state, true).unwrap();

        // Two DIFFERENT workers, each starting their own workflow in their
        // own worktree -- the exact scenario that clobbered under a shared
        // identity.
        let state_a = WorkflowState::start(
            worktree_a.clone(),
            "worker a's task".into(),
            WorkflowKind::Feature,
            None,
            true,
            low_classification(),
        );
        save(&state_dir, &state_a, true).unwrap();
        let state_b = WorkflowState::start(
            worktree_b.clone(),
            "worker b's task".into(),
            WorkflowKind::Feature,
            None,
            true,
            low_classification(),
        );
        save(&state_dir, &state_b, true).unwrap();

        assert_eq!(
            load_active(&state_dir, worktree_a).unwrap().unwrap().id,
            state_a.id,
            "worktree a must resolve its OWN workflow, not b's or the main checkout's"
        );
        assert_eq!(
            load_active(&state_dir, worktree_b).unwrap().unwrap().id,
            state_b.id,
            "worktree b must resolve its OWN workflow, not a's or the main checkout's"
        );
        assert_eq!(
            load_active(&state_dir, worktree_c).unwrap().unwrap().id,
            main_state.id,
            "a worktree with no workflow of its own must inherit the MAIN checkout's, \
             never an arbitrary sibling's"
        );
    }

    /// #260-adjacent: `zirv workflow advance --run-checks` collapses "run
    /// the gate, then advance" into one call -- a passing check must both
    /// print the evidence summary and advance past the `Test` step.
    #[test]
    fn advance_run_checks_runs_the_test_gate_and_advances_on_success() {
        let repo = tempdir().unwrap();
        let root = tempdir().unwrap();
        let state_dir = StateDir::from_root(root.path().to_path_buf());

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
                .expect("run git");
            assert!(status.success(), "git {args:?} failed");
        };
        git(&["init", "-q"]);
        std::fs::write(repo.path().join("README.md"), "readme\n").unwrap();
        git(&["add", "."]);
        git(&["commit", "-q", "-m", "base"]);

        std::fs::create_dir_all(repo.path().join(".zirv")).unwrap();
        let passing = if cfg!(windows) { "exit /b 0" } else { "exit 0" };
        std::fs::write(
            repo.path().join(".zirv/verify.toml"),
            format!("schema_version=1\n[[checks]]\nid='unit'\nkind='unit'\ncommand='{passing}'\n"),
        )
        .unwrap();

        let mut state = WorkflowState::start(
            repo.path().to_path_buf(),
            "small feature".into(),
            WorkflowKind::Feature,
            None,
            true,
            low_classification(),
        );
        let test_index = state
            .steps
            .iter()
            .position(|step| step.phase == WorkflowPhase::Test)
            .unwrap();
        state.completed_steps = state.steps[..test_index]
            .iter()
            .map(|step| step.id.clone())
            .collect();
        state.current_step = test_index;
        state.status = WorkflowStatus::Running;
        let id = state.id.clone();
        save(&state_dir, &state, true).unwrap();

        let _state_dir_env = crate::commands::ctx::testenv::VarGuard::set(&[(
            "ZIRV_CTX_STATE_DIR",
            Some(root.path().to_str().expect("utf-8 tempdir path")),
        )]);
        let args = WorkflowArgs {
            command: WorkflowSubcommand::Advance(AdvanceArgs {
                id: id.clone(),
                outcome: None,
                run_checks: true,
                repo: Some(repo.path().to_path_buf()),
                json: false,
                duration_ms: None,
                agent: None,
                model: None,
                role: None,
                input_tokens: None,
                output_tokens: None,
                workers: 0,
                frontend_root: None,
                accept_preexisting_findings: false,
            }),
        };
        let mut out = Vec::new();
        let code = run(&args, &mut out).unwrap();
        assert_eq!(code, 0, "a passing check must advance");
        let text = String::from_utf8(out).unwrap();
        assert!(
            text.contains("verification"),
            "expected the evidence summary to be printed, got {text}"
        );

        let reloaded = load(&state_dir, repo.path(), &id).unwrap();
        assert_eq!(
            reloaded.current().unwrap().phase,
            WorkflowPhase::Verify,
            "the test step must have advanced"
        );
    }

    /// Issue #610 scenario 3 (roadmap N05/N14/N15, review of #493): a real
    /// multi-file change, driven through BOTH gates a Feature workflow has
    /// -- Test (a real `--run-checks` execution) and Review (a real
    /// unresolved finding, blocking, then resolved) and Verify (a second
    /// real `--run-checks` execution) -- rather than exercising either gate
    /// in isolation the way the surrounding tests in this module do. Every
    /// step is the REAL production entry point (`run(&args, ...)`,
    /// `advance_with_evidence`), never a stand-in for what the gate would
    /// decide.
    #[test]
    fn a_real_multi_file_change_advances_only_once_test_review_and_verify_each_genuinely_pass() {
        use super::super::review::{FindingDisposition, FindingSeverity, ReviewFinding};

        let repo = tempdir().unwrap();
        let root = tempdir().unwrap();
        let state_dir = StateDir::from_root(root.path().to_path_buf());

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
                .expect("run git");
            assert!(status.success(), "git {args:?} failed");
        };
        git(&["init", "-q"]);
        std::fs::write(repo.path().join("a.rs"), "fn a() {}\n").unwrap();
        std::fs::write(repo.path().join("b.rs"), "fn b() {}\n").unwrap();
        git(&["add", "."]);
        git(&["commit", "-q", "-m", "base"]);
        // The real multi-file change this workflow is actually about.
        std::fs::write(repo.path().join("a.rs"), "fn a() { println!(\"a\"); }\n").unwrap();
        std::fs::write(repo.path().join("b.rs"), "fn b() { println!(\"b\"); }\n").unwrap();
        git(&["add", "."]);
        git(&["commit", "-q", "-m", "touch two files"]);

        std::fs::create_dir_all(repo.path().join(".zirv")).unwrap();
        let passing = if cfg!(windows) { "exit /b 0" } else { "exit 0" };
        let failing = if cfg!(windows) { "exit /b 1" } else { "exit 1" };
        let write_check = |command: &str| {
            std::fs::write(
                repo.path().join(".zirv/verify.toml"),
                format!(
                    "schema_version=1\n[[checks]]\nid='unit'\nkind='unit'\ncommand='{command}'\n"
                ),
            )
            .unwrap();
        };
        write_check(passing);

        let mut classification = low_classification();
        classification.risk = RiskBand::Medium;
        let mut state = WorkflowState::start(
            repo.path().to_path_buf(),
            "touch two files".into(),
            WorkflowKind::Feature,
            None,
            true,
            classification,
        );
        let review_index = state
            .steps
            .iter()
            .position(|step| step.phase == WorkflowPhase::Review)
            .expect("Medium risk must materialize a review step");
        let test_index = state
            .steps
            .iter()
            .position(|step| step.phase == WorkflowPhase::Test)
            .unwrap();
        let verify_index = state
            .steps
            .iter()
            .position(|step| step.phase == WorkflowPhase::Verify)
            .unwrap();
        assert!(
            test_index < review_index && review_index < verify_index,
            "test, then review, then verify: {:?}",
            state.steps.iter().map(|s| s.phase).collect::<Vec<_>>()
        );
        state.completed_steps = state.steps[..test_index]
            .iter()
            .map(|step| step.id.clone())
            .collect();
        state.current_step = test_index;
        state.status = WorkflowStatus::Running;
        let id = state.id.clone();
        save(&state_dir, &state, true).unwrap();

        let _state_dir_env = crate::commands::ctx::testenv::VarGuard::set(&[(
            "ZIRV_CTX_STATE_DIR",
            Some(root.path().to_str().expect("utf-8 tempdir path")),
        )]);
        let advance_args = || WorkflowArgs {
            command: WorkflowSubcommand::Advance(AdvanceArgs {
                id: id.clone(),
                outcome: None,
                run_checks: true,
                repo: Some(repo.path().to_path_buf()),
                json: false,
                duration_ms: None,
                agent: None,
                model: None,
                role: None,
                input_tokens: None,
                output_tokens: None,
                workers: 0,
                frontend_root: None,
                accept_preexisting_findings: false,
            }),
        };

        // Gate 1 (Test): a real passing check over the real two-file diff.
        let mut out = Vec::new();
        let code = run(&advance_args(), &mut out).unwrap();
        assert_eq!(code, 0, "a passing test check must advance past Test");
        let after_test = load(&state_dir, repo.path(), &id).unwrap();
        assert_eq!(after_test.current().unwrap().phase, WorkflowPhase::Review);

        // Gate 2 (Review): a real, unresolved finding blocks -- the same
        // gate `a_finding_recorded_while_the_reviewer_ran_survives_the_
        // evidence_write` proves records for real; this proves what the
        // engine does with it.
        let mut with_finding = after_test;
        with_finding.review_findings.push(ReviewFinding {
            id: "finding-1".into(),
            severity: FindingSeverity::Major,
            summary: "both files need a second look".into(),
            path: Some("a.rs".into()),
            line: None,
            disposition: FindingDisposition::Open,
            recommended_disposition: None,
            advisory_disposition: None,
            advisory_confidence: None,
            duplicate_of: None,
            created_at: 0,
        });
        save(&state_dir, &with_finding, true).unwrap();
        let blocked = advance_with_evidence(
            &state_dir,
            with_finding.clone(),
            StepOutcome::Success,
            None,
            false,
        )
        .unwrap_err()
        .to_string();
        assert!(
            blocked.contains("final disposition"),
            "an open finding must block review: {blocked}"
        );

        // Resolved for real: the review gate now passes, into Verify. Also
        // needs one fresh independent review run recorded against the
        // CURRENT diff's own fingerprint -- the same freshness check
        // `fix_review_rounds_advance_only_for_a_changed_fingerprint` pins,
        // computed here with the real production function rather than a
        // guessed value.
        let mut resolved = with_finding;
        resolved.review_findings[0].disposition = FindingDisposition::Fixed;
        let fingerprint = super::super::verification::change_fingerprint(&resolved.repo).unwrap();
        resolved
            .review_evidence
            .push(super::super::review::ReviewRunEvidence {
                id: "review-1".into(),
                change_fingerprint: fingerprint,
                adapter: "claude".into(),
                review_round: 1,
                completed_at: 0,
                head_sha: None,
                reviewed_tree_sha: None,
                finding_dispositions: std::collections::BTreeMap::new(),
            });
        let after_review =
            advance_with_evidence(&state_dir, resolved, StepOutcome::Success, None, false)
                .expect("a resolved finding must let review pass");
        assert_eq!(after_review.current().unwrap().phase, WorkflowPhase::Verify);
        save(&state_dir, &after_review, true).unwrap();

        // Gate 3 (Verify): a real failing check refuses this same diff...
        write_check(failing);
        let mut out = Vec::new();
        let code = run(&advance_args(), &mut out).unwrap();
        assert_eq!(code, 1, "a failing verify check must not advance");
        let still_verify = load(&state_dir, repo.path(), &id).unwrap();
        assert_eq!(
            still_verify.current().unwrap().phase,
            WorkflowPhase::Verify,
            "a failing check must not advance the workflow"
        );

        // ...and a real passing check over the SAME multi-file diff finally
        // clears it.
        write_check(passing);
        let mut out = Vec::new();
        let code = run(&advance_args(), &mut out).unwrap();
        assert_eq!(code, 0, "a passing verify check must advance past Verify");
        let final_state = load(&state_dir, repo.path(), &id).unwrap();
        assert_ne!(
            final_state.current().map(|step| step.phase),
            Some(WorkflowPhase::Verify),
            "the workflow must have moved past verify: {:?}",
            final_state.current()
        );
    }

    /// The mirror of the above: a failing check must print the failure and
    /// leave the workflow exactly where it was, rather than advancing on
    /// bad evidence.
    #[test]
    fn advance_run_checks_does_not_advance_on_failure() {
        let repo = tempdir().unwrap();
        let root = tempdir().unwrap();
        let state_dir = StateDir::from_root(root.path().to_path_buf());

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
                .expect("run git");
            assert!(status.success(), "git {args:?} failed");
        };
        git(&["init", "-q"]);
        std::fs::write(repo.path().join("README.md"), "readme\n").unwrap();
        git(&["add", "."]);
        git(&["commit", "-q", "-m", "base"]);

        std::fs::create_dir_all(repo.path().join(".zirv")).unwrap();
        let failing = if cfg!(windows) { "exit /b 1" } else { "exit 1" };
        std::fs::write(
            repo.path().join(".zirv/verify.toml"),
            format!("schema_version=1\n[[checks]]\nid='unit'\nkind='unit'\ncommand='{failing}'\n"),
        )
        .unwrap();

        let mut state = WorkflowState::start(
            repo.path().to_path_buf(),
            "small feature".into(),
            WorkflowKind::Feature,
            None,
            true,
            low_classification(),
        );
        let test_index = state
            .steps
            .iter()
            .position(|step| step.phase == WorkflowPhase::Test)
            .unwrap();
        state.completed_steps = state.steps[..test_index]
            .iter()
            .map(|step| step.id.clone())
            .collect();
        state.current_step = test_index;
        state.status = WorkflowStatus::Running;
        let id = state.id.clone();
        save(&state_dir, &state, true).unwrap();

        let _state_dir_env = crate::commands::ctx::testenv::VarGuard::set(&[(
            "ZIRV_CTX_STATE_DIR",
            Some(root.path().to_str().expect("utf-8 tempdir path")),
        )]);
        let args = WorkflowArgs {
            command: WorkflowSubcommand::Advance(AdvanceArgs {
                id: id.clone(),
                outcome: None,
                run_checks: true,
                repo: Some(repo.path().to_path_buf()),
                json: false,
                duration_ms: None,
                agent: None,
                model: None,
                role: None,
                input_tokens: None,
                output_tokens: None,
                workers: 0,
                frontend_root: None,
                accept_preexisting_findings: false,
            }),
        };
        let mut out = Vec::new();
        let code = run(&args, &mut out).unwrap();
        assert_eq!(code, 1, "a failing check must not advance");
        let text = String::from_utf8(out).unwrap();
        assert!(
            text.contains("was not advanced"),
            "expected the failure to be reported, got {text}"
        );

        let reloaded = load(&state_dir, repo.path(), &id).unwrap();
        assert_eq!(
            reloaded.current().unwrap().phase,
            WorkflowPhase::Test,
            "a failing check must leave the workflow on the test step"
        );
    }

    /// #287: a second `--run-checks` verify of the same step, with the
    /// worktree byte-identical to the previous failed attempt, must not
    /// execute any check at all -- the model is told to make progress
    /// instead of burning wall-clock on a run that can only reach the same
    /// verdict. Three such no-op turns still terminate the step at
    /// `MAX_STEP_ATTEMPTS`. The check's own marker file lives outside the
    /// repository (`root`, not `repo`) so writing it never perturbs
    /// `change_fingerprint` itself.
    #[test]
    fn advance_run_checks_skips_a_re_verify_of_an_unchanged_worktree() {
        let repo = tempdir().unwrap();
        let root = tempdir().unwrap();
        let state_dir = StateDir::from_root(root.path().to_path_buf());

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
                .expect("run git");
            assert!(status.success(), "git {args:?} failed");
        };
        git(&["init", "-q"]);
        std::fs::write(repo.path().join("README.md"), "readme\n").unwrap();
        git(&["add", "."]);
        git(&["commit", "-q", "-m", "base"]);

        std::fs::create_dir_all(repo.path().join(".zirv")).unwrap();
        let ran_count = root.path().join("ran_count.txt");
        let failing = if cfg!(windows) {
            format!("echo x>>{} & exit /b 1", ran_count.display())
        } else {
            format!("echo x >> {}; exit 1", ran_count.display())
        };
        std::fs::write(
            repo.path().join(".zirv/verify.toml"),
            format!("schema_version=1\n[[checks]]\nid='unit'\nkind='unit'\ncommand='{failing}'\n"),
        )
        .unwrap();

        let mut state = WorkflowState::start(
            repo.path().to_path_buf(),
            "small feature".into(),
            WorkflowKind::Feature,
            None,
            true,
            low_classification(),
        );
        let test_index = state
            .steps
            .iter()
            .position(|step| step.phase == WorkflowPhase::Test)
            .unwrap();
        state.completed_steps = state.steps[..test_index]
            .iter()
            .map(|step| step.id.clone())
            .collect();
        state.current_step = test_index;
        state.status = WorkflowStatus::Running;
        let step_id = state.steps[test_index].id.clone();
        let id = state.id.clone();
        save(&state_dir, &state, true).unwrap();

        let _state_dir_env = crate::commands::ctx::testenv::VarGuard::set(&[(
            "ZIRV_CTX_STATE_DIR",
            Some(root.path().to_str().expect("utf-8 tempdir path")),
        )]);
        let run_advance = || {
            let args = WorkflowArgs {
                command: WorkflowSubcommand::Advance(AdvanceArgs {
                    id: id.clone(),
                    outcome: None,
                    run_checks: true,
                    repo: Some(repo.path().to_path_buf()),
                    json: false,
                    duration_ms: None,
                    agent: None,
                    model: None,
                    role: None,
                    input_tokens: None,
                    output_tokens: None,
                    workers: 0,
                    frontend_root: None,
                    accept_preexisting_findings: false,
                }),
            };
            let mut out = Vec::new();
            let code = run(&args, &mut out).unwrap();
            (code, String::from_utf8(out).unwrap())
        };
        let ran_executions = || {
            std::fs::read_to_string(&ran_count)
                .map(|body| body.lines().count())
                .unwrap_or(0)
        };
        let attempts_of = |id: &str| {
            load(&state_dir, repo.path(), id)
                .unwrap()
                .attempts
                .get(&step_id)
                .copied()
                .unwrap_or(0)
        };

        // First call: the worktree has never been verified before, so the
        // check actually executes and fails. An ordinary failed run-checks
        // call does not itself burn an attempt.
        let (code, text) = run_advance();
        assert_eq!(code, 1);
        assert!(text.contains("was not advanced"), "{text}");
        assert_eq!(
            ran_executions(),
            1,
            "the first verify must execute the check"
        );
        assert_eq!(attempts_of(&id), 0);

        // Second call: nothing moved -- the guard must skip execution
        // entirely and name the attempt.
        let (code, text) = run_advance();
        assert_eq!(code, 1);
        assert!(
            text.contains(
                "verification not re-run: the worktree is byte-identical to the previous \
                 failed attempt (attempt 1/3). Edit source, tests, or record a blocker \
                 artifact before verifying again."
            ),
            "{text}"
        );
        assert_eq!(
            ran_executions(),
            1,
            "an unchanged re-verify must not execute the check a second time"
        );
        assert_eq!(attempts_of(&id), 1);
        assert_eq!(
            load(&state_dir, repo.path(), &id).unwrap().status,
            WorkflowStatus::Running
        );
        let events = super::super::telemetry::list(&state_dir, repo.path()).unwrap();
        assert!(
            events.iter().any(|event| event.kind
                == super::super::telemetry::TelemetryKind::PhaseFailed
                && event.verification_unchanged),
            "expected a PhaseFailed event marked verification_unchanged, got {events:?}"
        );

        // Third call: still unchanged.
        let (code, _) = run_advance();
        assert_eq!(code, 1);
        assert_eq!(ran_executions(), 1);
        assert_eq!(attempts_of(&id), 2);
        assert_eq!(
            load(&state_dir, repo.path(), &id).unwrap().status,
            WorkflowStatus::Running
        );

        // Fourth call: the third unchanged attempt reaches
        // `MAX_STEP_ATTEMPTS` and fails the step outright.
        let (code, text) = run_advance();
        assert_eq!(code, 1);
        assert!(text.contains("attempt 3/3"), "{text}");
        assert_eq!(
            ran_executions(),
            1,
            "none of the unchanged re-verifies ever executed the check"
        );
        assert_eq!(attempts_of(&id), 3);
        assert_eq!(
            load(&state_dir, repo.path(), &id).unwrap().status,
            WorkflowStatus::Failed
        );
    }

    /// Dogfooding regression: on a repository with a recorded test baseline
    /// (`zirv test baseline`), `--run-checks` used to decide pass/fail from
    /// `run_test`/`run_verify`'s own raw exit code, which reflects the
    /// unwaived result -- non-zero even when the only failure is already
    /// covered by the baseline. The very next `--outcome success` against the
    /// identical persisted report advanced fine, because that path (and the
    /// gate `--run-checks` now shares) reads the report back through
    /// `latest_is_fresh_and_passing`, which is baseline-aware. This proves
    /// `--run-checks` advances in that exact situation instead of reporting
    /// "checks failed".
    #[test]
    fn advance_run_checks_advances_when_the_only_failure_is_baselined() {
        let repo = tempdir().unwrap();
        let root = tempdir().unwrap();
        let home = tempdir().unwrap();
        let _home_guard = crate::commands::ctx::testenv::HomeGuard::set(home.path());
        let state_dir = StateDir::from_root(root.path().to_path_buf());

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
                .expect("run git");
            assert!(status.success(), "git {args:?} failed");
        };
        git(&["init", "-q"]);
        std::fs::write(repo.path().join("README.md"), "readme\n").unwrap();
        git(&["add", "."]);
        git(&["commit", "-q", "-m", "base"]);

        super::super::verification::save_baseline(
            repo.path(),
            BTreeSet::from(["wrap::tests::a".to_string()]),
        )
        .unwrap();

        std::fs::create_dir_all(repo.path().join(".zirv")).unwrap();
        // A `unit`-kind check whose output has the exact shape
        // `parse_cargo_test_failure_names`/`FailureNameScanner` recognize --
        // a `failures:` header naming `wrap::tests::a`, immediately followed
        // by a `test result: FAILED` line -- and a non-zero exit, so the
        // check itself is genuinely `Failed`; only the recorded baseline
        // makes the gate pass.
        let baselined_failure = if cfg!(windows) {
            "echo failures: & echo wrap::tests::a & echo test result: FAILED. 0 passed; 1 failed; \
             0 ignored; 0 measured; 0 filtered out; finished in 0.00s & exit /b 101"
        } else {
            "printf \"failures:\\nwrap::tests::a\\ntest result: FAILED. 0 passed; 1 failed; 0 \
             ignored; 0 measured; 0 filtered out; finished in 0.00s\\n\"; exit 101"
        };
        std::fs::write(
            repo.path().join(".zirv/verify.toml"),
            format!("schema_version=1\n[[checks]]\nid='unit'\nkind='unit'\ncommand='{baselined_failure}'\n"),
        )
        .unwrap();

        let mut state = WorkflowState::start(
            repo.path().to_path_buf(),
            "small feature".into(),
            WorkflowKind::Feature,
            None,
            true,
            low_classification(),
        );
        let test_index = state
            .steps
            .iter()
            .position(|step| step.phase == WorkflowPhase::Test)
            .unwrap();
        state.completed_steps = state.steps[..test_index]
            .iter()
            .map(|step| step.id.clone())
            .collect();
        state.current_step = test_index;
        state.status = WorkflowStatus::Running;
        let id = state.id.clone();
        save(&state_dir, &state, true).unwrap();

        let _state_dir_env = crate::commands::ctx::testenv::VarGuard::set(&[(
            "ZIRV_CTX_STATE_DIR",
            Some(root.path().to_str().expect("utf-8 tempdir path")),
        )]);
        let args = WorkflowArgs {
            command: WorkflowSubcommand::Advance(AdvanceArgs {
                id: id.clone(),
                outcome: None,
                run_checks: true,
                repo: Some(repo.path().to_path_buf()),
                json: false,
                duration_ms: None,
                agent: None,
                model: None,
                role: None,
                input_tokens: None,
                output_tokens: None,
                workers: 0,
                frontend_root: None,
                accept_preexisting_findings: false,
            }),
        };
        let mut out = Vec::new();
        let code = run(&args, &mut out).unwrap();
        assert_eq!(
            code, 0,
            "a failure fully covered by the recorded baseline must still advance"
        );

        let reloaded = load(&state_dir, repo.path(), &id).unwrap();
        assert_eq!(
            reloaded.current().unwrap().phase,
            WorkflowPhase::Verify,
            "the test step must have advanced on the baselined report"
        );

        // H-2: the just-persisted Test-phase report is itself gate-passing
        // (baseline-covered), but `run_required_checks`'s own
        // `last_failure_fingerprint` guard used to treat any `passed():
        // false` report as "the previous failed attempt" regardless of the
        // baseline -- with the worktree still byte-identical, that made the
        // Verify step's own `--run-checks` return `Unchanged` and never
        // actually run, instead of running (and passing, via the same
        // baseline) as it must here.
        let code_again = run(&args, &mut out).unwrap();
        assert_eq!(
            code_again, 0,
            "the Verify step's own baselined run must advance, not report Unchanged"
        );
        let reloaded_again = load(&state_dir, repo.path(), &id).unwrap();
        assert_eq!(
            reloaded_again.current().unwrap().phase,
            WorkflowPhase::Deploy,
            "the Verify step must have advanced on its own baselined report"
        );
    }

    /// Fail-open regression: a stale, still-fingerprint-fresh PASSING report
    /// already sits at `latest` (fingerprint unchanged since -- nothing in
    /// the tree moved). The run this `--run-checks` call actually performs
    /// FAILS, but persisting its report is made to fail too (`latest`'s
    /// pointer file is read-only, so `run_and_persist`'s `persist` call hits
    /// a genuine IO error -- swallowed into a warning, never an error, so
    /// the run's own printed results survive). Before the identity check,
    /// `latest_is_fresh_and_passing` would still read the untouched stale
    /// PASSING report and the gate would incorrectly advance. It must not:
    /// the identity of `latest` is unchanged, so no fresh report exists to
    /// gate on, and the step must stay put.
    #[test]
    fn advance_run_checks_does_not_advance_when_persistence_silently_fails() {
        let repo = tempdir().unwrap();
        let root = tempdir().unwrap();
        let state_dir = StateDir::from_root(root.path().to_path_buf());

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
                .expect("run git");
            assert!(status.success(), "git {args:?} failed");
        };
        git(&["init", "-q"]);
        std::fs::write(repo.path().join("README.md"), "readme\n").unwrap();
        git(&["add", "."]);
        git(&["commit", "-q", "-m", "base"]);

        std::fs::create_dir_all(repo.path().join(".zirv")).unwrap();
        let failing = if cfg!(windows) { "exit /b 1" } else { "exit 1" };
        std::fs::write(
            repo.path().join(".zirv/verify.toml"),
            format!("schema_version=1\n[[checks]]\nid='unit'\nkind='unit'\ncommand='{failing}'\n"),
        )
        .unwrap();

        // Seed a stale but still fingerprint-fresh PASSING report at
        // `latest`, matching the exact tree state above.
        let fingerprint = super::super::verification::change_fingerprint(repo.path()).unwrap();
        let stale_passing_report = super::super::verification::VerificationReport {
            schema_version: super::super::verification::VERIFY_REPORT_SCHEMA_VERSION,
            id: "stale-pass".into(),
            mode: super::super::verification::VerificationMode::Changed,
            source: "configured".into(),
            repo: repo.path().to_path_buf(),
            branch: String::new(),
            head_sha: String::new(),
            change_fingerprint: fingerprint,
            changed_paths: vec![],
            fallback_to_full: false,
            narrowed_to: vec![],
            notes: vec![],
            started_at: 0,
            finished_at: 0,
            checks: vec![super::super::verification::CheckResult {
                id: "unit".into(),
                kind: super::super::verification::CheckKind::Unit,
                command: "true".into(),
                source: super::super::verification::CheckSource::DiscoveredToolchain,
                status: super::super::verification::CheckStatus::Passed,
                exit_code: Some(0),
                duration_ms: 1,
                failure_output: None,
                failure_test_names: Vec::new(),
                inconclusive_reason: None,
            }],
        };
        super::super::verification::save_report(&state_dir, &stale_passing_report).unwrap();

        // Make the next persist's `latest`-pointer write fail: `write_private`
        // writes a temp sibling then `rename`s it over `latest`.
        let latest_pointer = state_dir
            .verification()
            .join(repo_slug(repo.path()))
            .join("latest");
        // On Unix a rename over a read-only FILE succeeds (the directory's
        // write bit governs), so there the directory holding `latest` is
        // what gets locked; on Windows the read-only destination file itself
        // makes the rename fail with ERROR_ACCESS_DENIED.
        let lock_target = if cfg!(unix) {
            latest_pointer.parent().unwrap().to_path_buf()
        } else {
            latest_pointer.clone()
        };
        let original_perms = std::fs::metadata(&lock_target).unwrap().permissions();
        let mut readonly_perms = original_perms.clone();
        readonly_perms.set_readonly(true);
        std::fs::set_permissions(&lock_target, readonly_perms).unwrap();

        let mut state = WorkflowState::start(
            repo.path().to_path_buf(),
            "small feature".into(),
            WorkflowKind::Feature,
            None,
            true,
            low_classification(),
        );
        let test_index = state
            .steps
            .iter()
            .position(|step| step.phase == WorkflowPhase::Test)
            .unwrap();
        state.completed_steps = state.steps[..test_index]
            .iter()
            .map(|step| step.id.clone())
            .collect();
        state.current_step = test_index;
        state.status = WorkflowStatus::Running;
        let id = state.id.clone();
        save(&state_dir, &state, true).unwrap();

        let _state_dir_env = crate::commands::ctx::testenv::VarGuard::set(&[(
            "ZIRV_CTX_STATE_DIR",
            Some(root.path().to_str().expect("utf-8 tempdir path")),
        )]);
        let args = WorkflowArgs {
            command: WorkflowSubcommand::Advance(AdvanceArgs {
                id: id.clone(),
                outcome: None,
                run_checks: true,
                repo: Some(repo.path().to_path_buf()),
                json: false,
                duration_ms: None,
                agent: None,
                model: None,
                role: None,
                input_tokens: None,
                output_tokens: None,
                workers: 0,
                frontend_root: None,
                accept_preexisting_findings: false,
            }),
        };
        let mut out = Vec::new();
        let result = run(&args, &mut out);

        // Cleanup before any assertion panics, so the tempdir can still be
        // removed on drop even if an assertion below fails. Restores the
        // exact original permissions rather than `set_readonly(false)`,
        // which clippy flags as leaving the file world-writable on Unix.
        std::fs::set_permissions(&lock_target, original_perms).unwrap();

        let code = result.unwrap();
        assert_eq!(
            code, 1,
            "a genuinely failing run whose report could not be persisted must not advance on a \
             stale prior report"
        );
        let text = String::from_utf8(out).unwrap();
        assert!(
            text.contains("no fresh report was persisted"),
            "expected the persistence-failure reason to be reported, got {text}"
        );

        let reloaded = load(&state_dir, repo.path(), &id).unwrap();
        assert_eq!(
            reloaded.current().unwrap().phase,
            WorkflowPhase::Test,
            "the step must not advance on stale evidence when the fresh report never persisted"
        );
    }

    /// `--run-checks` only knows how to satisfy a `Test`/`Verify` step's own
    /// evidence gate; any other current step must fail loudly rather than
    /// silently treating itself as satisfied.
    #[test]
    fn advance_run_checks_rejects_a_non_test_verify_step() {
        let repo = tempdir().unwrap();
        let root = tempdir().unwrap();
        let state_dir = StateDir::from_root(root.path().to_path_buf());
        let state = skip_leading_artifact_steps(WorkflowState::start(
            repo.path().to_path_buf(),
            "small feature".into(),
            WorkflowKind::Feature,
            None,
            true,
            low_classification(),
        ));
        assert_eq!(state.current().unwrap().phase, WorkflowPhase::Implement);
        let id = state.id.clone();
        save(&state_dir, &state, true).unwrap();

        let _state_dir_env = crate::commands::ctx::testenv::VarGuard::set(&[(
            "ZIRV_CTX_STATE_DIR",
            Some(root.path().to_str().expect("utf-8 tempdir path")),
        )]);
        let args = WorkflowArgs {
            command: WorkflowSubcommand::Advance(AdvanceArgs {
                id: id.clone(),
                outcome: None,
                run_checks: true,
                repo: Some(repo.path().to_path_buf()),
                json: false,
                duration_ms: None,
                agent: None,
                model: None,
                role: None,
                input_tokens: None,
                output_tokens: None,
                workers: 0,
                frontend_root: None,
                accept_preexisting_findings: false,
            }),
        };
        let mut out = Vec::new();
        let error = run(&args, &mut out).unwrap_err().to_string();
        assert!(
            error.contains("--run-checks") && error.contains("--outcome instead"),
            "{error}"
        );
    }

    fn review_finding(
        id: &str,
        disposition: super::super::review::FindingDisposition,
        recommended: Option<super::super::review::FindingDisposition>,
    ) -> super::super::review::ReviewFinding {
        super::super::review::ReviewFinding {
            id: id.into(),
            severity: super::super::review::FindingSeverity::Major,
            summary: "summary".into(),
            path: None,
            line: None,
            disposition,
            recommended_disposition: recommended,
            advisory_disposition: None,
            advisory_confidence: None,
            duplicate_of: None,
            created_at: 0,
        }
    }

    /// #260-adjacent (T3, bulk dispose): three open findings with mixed
    /// recommendations must each land on their own recommended disposition
    /// in one call; a finding with no recommendation stays `Open` and is
    /// still reported (not silently dropped); an already-resolved finding is
    /// left alone and not reported at all.
    #[test]
    fn apply_recommended_dispositions_applies_each_open_findings_own_recommendation() {
        use super::super::review::FindingDisposition as Disposition;
        let repo = tempdir().unwrap();
        let root = tempdir().unwrap();
        let state_dir = StateDir::from_root(root.path().to_path_buf());
        let mut state = WorkflowState::start(
            repo.path().to_path_buf(),
            "small feature".into(),
            WorkflowKind::Feature,
            None,
            true,
            low_classification(),
        );
        let mut minor_dismissal =
            review_finding("b", Disposition::Open, Some(Disposition::Dismissed));
        minor_dismissal.severity = super::super::review::FindingSeverity::Minor;
        state.review_findings = vec![
            review_finding("a", Disposition::Open, Some(Disposition::Fixed)),
            minor_dismissal,
            review_finding("c", Disposition::Open, None),
            review_finding("d", Disposition::Accepted, Some(Disposition::Fixed)),
        ];
        let id = state.id.clone();
        save(&state_dir, &state, true).unwrap();

        let (state, results) = apply_recommended_dispositions(&state_dir, state).unwrap();

        assert_eq!(
            results.len(),
            3,
            "only the open findings are considered: {results:?}"
        );
        let by_id: std::collections::BTreeMap<_, _> = results
            .iter()
            .map(|result| (result.finding_id.clone(), result.applied))
            .collect();
        assert_eq!(by_id["a"], Some(Disposition::Fixed));
        assert_eq!(by_id["b"], Some(Disposition::Dismissed));
        assert_eq!(
            by_id["c"], None,
            "no recommendation must be reported as such"
        );

        let finding = |needle: &str| {
            state
                .review_findings
                .iter()
                .find(|finding| finding.id == needle)
                .unwrap()
        };
        assert_eq!(finding("a").disposition, Disposition::Fixed);
        assert_eq!(finding("b").disposition, Disposition::Dismissed);
        assert_eq!(
            finding("c").disposition,
            Disposition::Open,
            "no recommendation must leave the finding open"
        );
        assert_eq!(
            finding("d").disposition,
            Disposition::Accepted,
            "an already-resolved finding must never be revisited"
        );

        let reloaded = load(&state_dir, repo.path(), &id).unwrap();
        assert_eq!(
            reloaded
                .review_findings
                .iter()
                .find(|finding| finding.id == "a")
                .unwrap()
                .disposition,
            Disposition::Fixed,
            "the applied disposition must persist"
        );
    }

    #[test]
    fn apply_recommended_dispositions_requires_explicit_major_or_critical_dismissal() {
        use super::super::review::{
            FindingDisposition as Disposition, FindingSeverity as Severity,
        };
        let repo = tempdir().unwrap();
        let root = tempdir().unwrap();
        let state_dir = StateDir::from_root(root.path().to_path_buf());
        let mut state = WorkflowState::start(
            repo.path().to_path_buf(),
            "small feature".into(),
            WorkflowKind::Feature,
            None,
            true,
            low_classification(),
        );
        let mut critical =
            review_finding("critical", Disposition::Open, Some(Disposition::Dismissed));
        critical.severity = Severity::Critical;
        let major = review_finding("major", Disposition::Open, Some(Disposition::Dismissed));
        let mut minor = review_finding("minor", Disposition::Open, Some(Disposition::Dismissed));
        minor.severity = Severity::Minor;
        state.review_findings = vec![critical, major, minor];

        let (state, results) = apply_recommended_dispositions(&state_dir, state).unwrap();
        let finding = |needle: &str| {
            state
                .review_findings
                .iter()
                .find(|finding| finding.id == needle)
                .unwrap()
        };
        assert_eq!(finding("critical").disposition, Disposition::Open);
        assert_eq!(finding("major").disposition, Disposition::Open);
        assert_eq!(finding("minor").disposition, Disposition::Dismissed);
        for id in ["critical", "major"] {
            let result = results
                .iter()
                .find(|result| result.finding_id == id)
                .unwrap();
            assert_eq!(result.applied, None);
            assert!(result.requires_explicit_disposition);
        }
    }

    #[test]
    fn advance_frontend_root_flag_persists_into_state() {
        let repo = tempdir().unwrap();
        let target_repo = tempdir().unwrap();
        let root = tempdir().unwrap();
        let state_dir = StateDir::from_root(root.path().to_path_buf());
        let state = skip_leading_artifact_steps(WorkflowState::start(
            repo.path().to_path_buf(),
            "small feature".into(),
            WorkflowKind::Feature,
            None,
            true,
            low_classification(),
        ));
        let id = state.id.clone();
        save(&state_dir, &state, true).unwrap();

        let _state_dir_env = crate::commands::ctx::testenv::VarGuard::set(&[(
            "ZIRV_CTX_STATE_DIR",
            Some(root.path().to_str().expect("utf-8 tempdir path")),
        )]);
        let args = WorkflowArgs {
            command: WorkflowSubcommand::Advance(AdvanceArgs {
                id: id.clone(),
                outcome: Some(StepOutcome::Failure),
                run_checks: false,
                repo: Some(repo.path().to_path_buf()),
                json: false,
                duration_ms: None,
                agent: None,
                model: None,
                role: None,
                input_tokens: None,
                output_tokens: None,
                workers: 0,
                frontend_root: Some(target_repo.path().to_path_buf()),
                accept_preexisting_findings: false,
            }),
        };
        let mut out = Vec::new();
        run(&args, &mut out).unwrap();

        let reloaded = load(&state_dir, repo.path(), &id).unwrap();
        assert_eq!(
            reloaded.frontend_target_root,
            Some(target_repo.path().canonicalize().unwrap())
        );
    }

    #[test]
    fn advance_persists_frontend_root_before_the_gate_even_when_it_still_fails_closed() {
        // #214 follow-up: `--frontend-root` must be saved before the gate
        // runs, so a fail-closed advance (the target root has no fresh
        // evidence yet) still records the root -- the operator should not
        // have to pass the flag again on retry.
        let workflow_repo = tempdir().unwrap();
        let target_repo = tempdir().unwrap();
        let root = tempdir().unwrap();
        let state_dir = StateDir::from_root(root.path().to_path_buf());

        let mut classification = low_classification();
        classification.work_domain.domain = WorkDomain::Frontend;
        classification.work_domain.score = 55;
        let mut state = WorkflowState::start(
            workflow_repo.path().to_path_buf(),
            "build a frontend component".into(),
            WorkflowKind::Feature,
            None,
            true,
            classification,
        );
        let test_index = state
            .steps
            .iter()
            .position(|step| step.phase == WorkflowPhase::Test)
            .unwrap();
        state.completed_steps = state.steps[..test_index]
            .iter()
            .map(|step| step.id.clone())
            .collect();
        state.current_step = test_index;
        state.status = WorkflowStatus::Running;
        let id = state.id.clone();
        save(&state_dir, &state, true).unwrap();

        let _state_dir_env = crate::commands::ctx::testenv::VarGuard::set(&[(
            "ZIRV_CTX_STATE_DIR",
            Some(root.path().to_str().expect("utf-8 tempdir path")),
        )]);
        let args = WorkflowArgs {
            command: WorkflowSubcommand::Advance(AdvanceArgs {
                id: id.clone(),
                outcome: Some(StepOutcome::Success),
                run_checks: false,
                repo: Some(workflow_repo.path().to_path_buf()),
                json: false,
                duration_ms: None,
                agent: None,
                model: None,
                role: None,
                input_tokens: None,
                output_tokens: None,
                workers: 0,
                // `target_repo` is empty and has no detector evidence of its
                // own, so the gate must still fail closed against it.
                frontend_root: Some(target_repo.path().to_path_buf()),
                accept_preexisting_findings: false,
            }),
        };
        let mut out = Vec::new();
        let result = run(&args, &mut out);
        assert!(
            result.is_err(),
            "expected the gate to still fail closed against an empty target root"
        );

        let reloaded = load(&state_dir, workflow_repo.path(), &id).unwrap();
        assert_eq!(
            reloaded.frontend_target_root,
            Some(target_repo.path().canonicalize().unwrap())
        );
    }

    /// #255 recovery path (i): a task classified General/Standard (no
    /// frontend text or path signal) can still be forced onto the Frontend
    /// methodology overlay with `--profile`, applied after classification
    /// materializes the default steps.
    #[test]
    fn start_profile_flag_overrides_automatic_classification() {
        let repo = tempdir().unwrap();
        let root = tempdir().unwrap();
        let _state_dir_env = crate::commands::ctx::testenv::VarGuard::set(&[(
            "ZIRV_CTX_STATE_DIR",
            Some(root.path().to_str().expect("utf-8 tempdir path")),
        )]);
        let args = WorkflowArgs {
            command: WorkflowSubcommand::Start(StartArgs {
                id: Some("bugfix".into()),
                task: "fix a database retry bug".into(),
                agent: None,
                built_in_only: true,
                repo: Some(repo.path().to_path_buf()),
                // Declared, so classification never needs a real git
                // repository: this test is about `--profile`, not about
                // git-measured risk.
                paths: vec![PathBuf::from("src/commands/ctx/safety.rs")],
                changed_lines: Some(40),
                tests_changed: true,
                complexity: None,
                risk: None,
                branch: None,
                frontend_root: None,
                brainstorm: false,
                no_brainstorm: false,
                profile: Some(WorkflowProfile::Frontend),
                json: false,
            }),
        };
        let mut out = Vec::new();
        run(&args, &mut out).unwrap();

        let state_dir = resolve_state().unwrap();
        let state = load_active(&state_dir, repo.path()).unwrap().unwrap();
        assert_eq!(
            state.classification.work_domain.domain,
            WorkDomain::General,
            "the task/paths alone must not have classified this as Frontend"
        );
        assert_eq!(state.profile, WorkflowProfile::Frontend);
        assert_eq!(state.profile_source, ProfileSource::OperatorOverride);
        assert!(
            state
                .steps
                .iter()
                .any(|step| step.skill == "frontend-implement")
        );
    }

    /// Workflow-trigger-determinism item 4: an explicit id is matched
    /// case-insensitively -- `zirv workflow start Bugfix` must resolve
    /// exactly like `zirv workflow start bugfix` rather than exiting 2
    /// "unknown workflow 'Bugfix'".
    #[test]
    fn start_resolves_an_explicit_id_case_insensitively() {
        let repo = tempdir().unwrap();
        let root = tempdir().unwrap();
        let _state_dir_env = crate::commands::ctx::testenv::VarGuard::set(&[(
            "ZIRV_CTX_STATE_DIR",
            Some(root.path().to_str().expect("utf-8 tempdir path")),
        )]);
        let args = WorkflowArgs {
            command: WorkflowSubcommand::Start(StartArgs {
                id: Some("Bugfix".into()),
                task: "fix a database retry bug".into(),
                agent: None,
                built_in_only: true,
                repo: Some(repo.path().to_path_buf()),
                paths: vec![PathBuf::from("src/commands/ctx/safety.rs")],
                changed_lines: Some(40),
                tests_changed: true,
                complexity: None,
                risk: None,
                branch: None,
                frontend_root: None,
                brainstorm: false,
                no_brainstorm: false,
                profile: None,
                json: false,
            }),
        };
        let mut out = Vec::new();
        run(&args, &mut out).expect("'Bugfix' must resolve like 'bugfix'");

        let state_dir = resolve_state().unwrap();
        let state = load_active(&state_dir, repo.path()).unwrap().unwrap();
        assert_eq!(state.kind, WorkflowKind::Bugfix);
        assert_eq!(
            state.definition.as_ref().map(|d| d.id.as_str()),
            Some("bugfix")
        );
    }

    /// Workflow-trigger-determinism item 5: `zirv workflow show` resolves
    /// its id the same case-insensitive way as `start`.
    #[test]
    fn show_resolves_an_explicit_id_case_insensitively() {
        let repo = tempdir().unwrap();
        let registry = load_workflow_registry(repo.path(), true).unwrap();
        let workflow = registry.get("bugfix").unwrap();
        let args = ShowArgs {
            id: "BUGFIX".into(),
            json: false,
            built_in_only: true,
            repo: Some(repo.path().to_path_buf()),
        };
        let full_args = WorkflowArgs {
            command: WorkflowSubcommand::Show(args),
        };
        let mut out = Vec::new();
        run(&full_args, &mut out).expect("'BUGFIX' must resolve like 'bugfix'");
        assert_eq!(workflow.definition.id, "bugfix");
    }

    /// Workflow-trigger-determinism item 5: the exact wording of the note
    /// `start_workflow` prints to STDERR when a start silently displaces a
    /// different, still-running workflow as this repository's active one.
    /// A pure fn, so the wording is checked directly rather than by
    /// capturing real process STDERR.
    #[test]
    fn active_workflow_displaced_note_names_both_ids_and_the_resume_command() {
        let note = active_workflow_displaced_note("abc-123", "feature");
        assert!(note.starts_with("note: "), "{note}");
        assert!(note.contains("abc-123"), "{note}");
        assert!(note.contains("(feature)"), "{note}");
        assert!(
            note.contains("zirv workflow resume abc-123"),
            "must point at the exact resume command: {note}"
        );
    }

    /// Review finding F2: a write failure (a closed stderr, say) must never
    /// panic -- the new workflow this note is ABOUT is already saved by the
    /// time it's printed, so a failure here must degrade silently rather
    /// than turning an already-successful start into a reported failure.
    #[test]
    fn best_effort_write_displacement_note_never_panics_on_a_failing_writer() {
        struct AlwaysErrors;
        impl std::io::Write for AlwaysErrors {
            fn write(&mut self, _buf: &[u8]) -> std::io::Result<usize> {
                Err(std::io::Error::other("closed"))
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Err(std::io::Error::other("closed"))
            }
        }
        // Must not panic; a `writeln!`/`eprintln!`-shaped implementation
        // that propagated the error with `.unwrap()`/`.expect()` would.
        best_effort_write_displacement_note(AlwaysErrors, "note: irrelevant");
    }

    /// Workflow-trigger-determinism item 5: starting a second workflow for
    /// the same repository while an earlier one is still `Running` must
    /// NOT refuse -- multiple workflows per repository are legitimate, and
    /// `zirv workflow resume` restores the displaced one. This only proves
    /// the non-refusal and that the active pointer now names the new run;
    /// the note text itself is covered by
    /// `active_workflow_displaced_note_names_both_ids_and_the_resume_command`
    /// since real process STDERR isn't capturable through this seam.
    #[test]
    fn starting_a_second_workflow_never_refuses_and_moves_the_active_pointer() {
        let repo = tempdir().unwrap();
        let root = tempdir().unwrap();
        let _state_dir_env = crate::commands::ctx::testenv::VarGuard::set(&[(
            "ZIRV_CTX_STATE_DIR",
            Some(root.path().to_str().expect("utf-8 tempdir path")),
        )]);
        let start_args = |id: &str, task: &str| StartArgs {
            id: Some(id.to_string()),
            task: task.to_string(),
            agent: None,
            built_in_only: true,
            repo: Some(repo.path().to_path_buf()),
            paths: vec![PathBuf::from("src/commands/ctx/safety.rs")],
            changed_lines: Some(40),
            tests_changed: true,
            complexity: None,
            risk: None,
            branch: None,
            frontend_root: None,
            brainstorm: false,
            no_brainstorm: false,
            profile: None,
            json: false,
        };

        let mut out = Vec::new();
        run(
            &WorkflowArgs {
                command: WorkflowSubcommand::Start(start_args("bugfix", "fix the first thing")),
            },
            &mut out,
        )
        .unwrap();
        let state_dir = resolve_state().unwrap();
        let first = load_active(&state_dir, repo.path()).unwrap().unwrap();
        assert!(
            matches!(
                first.status,
                WorkflowStatus::Running | WorkflowStatus::AwaitingApproval
            ),
            "the first workflow must still be non-terminal: {:?}",
            first.status
        );

        let mut out2 = Vec::new();
        let result = run(
            &WorkflowArgs {
                command: WorkflowSubcommand::Start(start_args("feature", "add the second thing")),
            },
            &mut out2,
        );
        assert!(
            result.is_ok(),
            "a second workflow for the same repo must never be refused: {result:?}"
        );

        let second = load_active(&state_dir, repo.path()).unwrap().unwrap();
        assert_ne!(second.id, first.id);
        assert_eq!(second.kind, WorkflowKind::Feature);

        // The first workflow's own state is untouched -- still non-terminal,
        // still loadable, `resume`-able exactly as the note says.
        let reloaded_first = load(&state_dir, repo.path(), &first.id).unwrap();
        assert_eq!(reloaded_first.status, first.status);
    }

    /// #255 recovery path (ii): `workflow reclassify` forces a persisted
    /// workflow's profile without resetting the state machine -- completed
    /// steps and already-accepted artifacts survive the change.
    #[test]
    fn reclassify_preserves_completed_steps_and_accepted_artifacts() {
        let repo = tempdir().unwrap();
        let root = tempdir().unwrap();
        let state_dir = StateDir::from_root(root.path().to_path_buf());
        let mut classification = low_classification();
        classification.complexity = Complexity::Substantial;
        classification.risk = RiskBand::High;
        let state = WorkflowState::start(
            repo.path().to_path_buf(),
            "substantial feature".into(),
            WorkflowKind::Feature,
            None,
            true,
            classification,
        );
        assert_eq!(state.status, WorkflowStatus::AwaitingApproval);
        assert_eq!(state.current().unwrap().id, "intent");
        ensure_current_artifact_template(&state).unwrap();
        std::fs::write(
            workflow_artifact_path(&state, ArtifactStage::Intent).unwrap(),
            "# Intent\n\n## Problem\nConcrete problem\n\n## Desired outcome\nConcrete result\n",
        )
        .unwrap();
        let state = approve(&state_dir, state).unwrap();
        assert_eq!(state.current().unwrap().id, "spec");
        ensure_current_artifact_template(&state).unwrap();
        std::fs::write(
            workflow_artifact_path(&state, ArtifactStage::Spec).unwrap(),
            "# Specification\n\n## Context\nReal context\n\n## Goals\n- ship it\n",
        )
        .unwrap();
        let state = approve(&state_dir, state).unwrap();
        assert_eq!(state.current().unwrap().id, "plan");
        assert_eq!(state.profile, WorkflowProfile::Standard);
        let intent_hash_before = state.artifacts.get("intent").unwrap().accepted_hash.clone();
        let spec_hash_before = state.artifacts.get("spec").unwrap().accepted_hash.clone();
        assert!(intent_hash_before.is_some());
        assert!(spec_hash_before.is_some());

        let reclassified = reclassify(&state_dir, state, WorkflowProfile::Frontend).unwrap();

        assert_eq!(reclassified.profile, WorkflowProfile::Frontend);
        assert_eq!(reclassified.profile_source, ProfileSource::OperatorOverride);
        assert_eq!(
            reclassified.completed_steps,
            vec!["intent".to_string(), "spec".to_string()],
            "completed steps must survive reclassification"
        );
        assert_eq!(
            reclassified.artifacts.get("intent").unwrap().accepted_hash,
            intent_hash_before,
            "the accepted intent artifact must survive reclassification"
        );
        assert_eq!(
            reclassified.artifacts.get("spec").unwrap().accepted_hash,
            spec_hash_before,
            "the accepted spec artifact must survive reclassification"
        );
        assert_eq!(reclassified.current().unwrap().id, "plan");
        assert_eq!(reclassified.current().unwrap().skill, "frontend-plan");

        let reloaded = load(&state_dir, repo.path(), &reclassified.id).unwrap();
        assert_eq!(reloaded.profile, WorkflowProfile::Frontend);
    }

    /// #251: a full-surface (Review/Verify) detector scan tags a finding
    /// `preexisting` when its path was not part of the since-base change
    /// set. Without `--accept-preexisting-findings` such a finding still
    /// fails the gate and the error names the flag and the count; with the
    /// flag, the gate accepts it, records the acceptance, and execution
    /// reaches the next (render) gate instead.
    #[test]
    fn accept_preexisting_findings_flag_lets_the_review_gate_pass_pre_existing_blocking_findings() {
        let repo = tempdir().unwrap();
        let root = tempdir().unwrap();
        let state_dir = StateDir::from_root(root.path().to_path_buf());
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
                .expect("run git");
            assert!(status.success(), "git {args:?} failed");
        };
        git(&["init", "-q", "-b", "main"]);
        std::fs::write(
            repo.path().join("Old.tsx"),
            "export const Old = () => <img src={avatar} />;\n",
        )
        .unwrap();
        git(&["add", "."]);
        git(&["commit", "-q", "-m", "base"]);
        git(&["checkout", "-q", "-b", "feature"]);
        std::fs::write(
            repo.path().join("style.css"),
            ".card { color: rebeccapurple; }\n",
        )
        .unwrap();
        git(&["add", "."]);
        git(&["commit", "-q", "-m", "add clean style"]);

        let mut classification = low_classification();
        classification.risk = RiskBand::Medium;
        classification.work_domain.domain = WorkDomain::Frontend;
        classification.work_domain.score = 55;
        let mut state = WorkflowState::start(
            repo.path().to_path_buf(),
            "review a frontend component".into(),
            WorkflowKind::Review,
            None,
            true,
            classification,
        );
        assert_eq!(state.current().unwrap().phase, WorkflowPhase::Review);
        state.status = WorkflowStatus::Running;

        let without_flag =
            advance_with_evidence(&state_dir, state.clone(), StepOutcome::Success, None, false)
                .unwrap_err()
                .to_string();
        assert!(
            without_flag.contains("--accept-preexisting-findings"),
            "{without_flag}"
        );
        assert!(
            without_flag.contains("1 pre-existing blocking"),
            "{without_flag}"
        );
        assert!(
            without_flag.contains("0 introduced blocking"),
            "{without_flag}"
        );

        let with_flag = advance_with_evidence(&state_dir, state, StepOutcome::Success, None, true)
            .unwrap_err()
            .to_string();
        assert!(
            !with_flag.contains("automatically ran the detector"),
            "the flag must let the detector gate pass its pre-existing findings: {with_flag}"
        );
    }

    /// Reviewer finding: the acceptance was only mutated on the in-memory
    /// `WorkflowState`, so if a LATER gate in this same `advance` call (the
    /// render/visual-review gate, right after the detector gate) still
    /// fails closed, the acceptance was lost -- the operator would have to
    /// pass `--accept-preexisting-findings` again on the very next retry.
    #[test]
    fn accept_preexisting_findings_persists_even_when_a_later_gate_fails() {
        let repo = tempdir().unwrap();
        let root = tempdir().unwrap();
        let state_dir = StateDir::from_root(root.path().to_path_buf());
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
                .expect("run git");
            assert!(status.success(), "git {args:?} failed");
        };
        git(&["init", "-q", "-b", "main"]);
        std::fs::write(
            repo.path().join("Old.tsx"),
            "export const Old = () => <img src={avatar} />;\n",
        )
        .unwrap();
        git(&["add", "."]);
        git(&["commit", "-q", "-m", "base"]);
        git(&["checkout", "-q", "-b", "feature"]);
        std::fs::write(
            repo.path().join("style.css"),
            ".card { color: rebeccapurple; }\n",
        )
        .unwrap();
        git(&["add", "."]);
        git(&["commit", "-q", "-m", "add clean style"]);

        let mut classification = low_classification();
        classification.risk = RiskBand::Medium;
        classification.work_domain.domain = WorkDomain::Frontend;
        classification.work_domain.score = 55;
        let mut state = WorkflowState::start(
            repo.path().to_path_buf(),
            "review a frontend component".into(),
            WorkflowKind::Review,
            None,
            true,
            classification,
        );
        assert_eq!(state.current().unwrap().phase, WorkflowPhase::Review);
        state.status = WorkflowStatus::Running;
        let id = state.id.clone();
        save(&state_dir, &state, true).unwrap();

        let error = advance_with_evidence(&state_dir, state, StepOutcome::Success, None, true)
            .unwrap_err()
            .to_string();
        // The render gate fails closed in this test environment (no dev
        // server/browser); confirm we actually got past the detector gate
        // so this test is exercising the scenario it claims to.
        assert!(!error.contains("automatically ran the detector"), "{error}");

        let reloaded = load(&state_dir, repo.path(), &id).unwrap();
        assert!(
            reloaded.accepted_preexisting_findings.is_some(),
            "the acceptance must survive a later gate failing closed in the same advance"
        );
    }

    #[test]
    fn frontend_review_step_collects_visual_evidence_automatically_and_fails_closed() {
        let repo = tempdir().unwrap();
        let root = tempdir().unwrap();
        let state_dir = StateDir::from_root(root.path().to_path_buf());
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
        std::fs::write(
            repo.path().join("App.tsx"),
            "export const App = () => <main />;\n",
        )
        .unwrap();
        git(&["add", "App.tsx"]);
        git(&["commit", "-q", "-m", "base"]);
        std::fs::write(
            repo.path().join("App.tsx"),
            "export const App = () => <main><h1>Settings</h1></main>;\n",
        )
        .unwrap();
        let profile = super::super::frontend::ensure_profile(&state_dir, repo.path()).unwrap();
        let detector = super::super::frontend_detector::DetectorReport {
            schema_version: super::super::frontend_detector::DETECTOR_REPORT_SCHEMA_VERSION,
            id: uuid::Uuid::new_v4().to_string(),
            repo: repo.path().canonicalize().unwrap(),
            change_fingerprint: super::super::verification::change_fingerprint(repo.path())
                .unwrap(),
            profile_fingerprint: profile.source_fingerprint,
            scope: super::super::frontend_detector::DetectorScope::Changed,
            generated_at: now_secs(),
            analyzed_files: vec![PathBuf::from("App.tsx")],
            analyzed_bytes: 64,
            truncated: false,
            findings: Vec::new(),
            waivers_loaded: 0,
            waivers_rejected: 0,
            not_applicable: false,
        };
        super::super::frontend_detector::save_report(&state_dir, &detector).unwrap();
        let mut classification = low_classification();
        classification.risk = RiskBand::Medium;
        classification.work_domain.domain = WorkDomain::Frontend;
        classification.work_domain.score = 55;
        let mut state = WorkflowState::start(
            repo.path().to_path_buf(),
            "review a frontend component".into(),
            WorkflowKind::Review,
            None,
            true,
            classification,
        );
        state.current_step = state
            .steps
            .iter()
            .position(|step| step.phase == WorkflowPhase::Review)
            .unwrap();

        let error = advance_with_evidence(&state_dir, state, StepOutcome::Success, None, false)
            .unwrap_err()
            .to_string();

        assert!(error.contains("automatic rendered evidence"), "{error}");
        assert!(error.contains("zirv frontend render"), "{error}");
        assert!(!error.contains("frontend review --help"), "{error}");
    }

    #[test]
    fn frontend_render_gate_uses_frontend_target_root_when_set() {
        // #214 follow-up: once `frontend_target_root` is set, the render/
        // visual-review gate must scan it instead of `state.repo`, mirroring
        // `frontend_gate_uses_frontend_target_root_when_set` for the
        // detector gate.
        let workflow_repo = tempdir().unwrap();
        let target_repo = tempdir().unwrap();
        let root = tempdir().unwrap();
        let state_dir = StateDir::from_root(root.path().to_path_buf());
        let git = |dir: &std::path::Path, args: &[&str]| {
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
                .current_dir(dir)
                .status()
                .unwrap();
            assert!(status.success(), "git {args:?} failed in {}", dir.display());
        };
        git(workflow_repo.path(), &["init", "-q"]);
        std::fs::write(workflow_repo.path().join("README.md"), "readme\n").unwrap();
        git(workflow_repo.path(), &["add", "."]);
        git(workflow_repo.path(), &["commit", "-q", "-m", "base"]);

        git(target_repo.path(), &["init", "-q"]);
        std::fs::write(
            target_repo.path().join("App.tsx"),
            "export const App = () => <main />;\n",
        )
        .unwrap();
        git(target_repo.path(), &["add", "App.tsx"]);
        git(target_repo.path(), &["commit", "-q", "-m", "base"]);

        // Pre-seed a fresh, passing detector report for `target_repo` so the
        // detector gate above the render step is already satisfied and
        // execution reaches the render/visual-review gate under test here.
        let profile =
            super::super::frontend::ensure_profile(&state_dir, target_repo.path()).unwrap();
        let detector = super::super::frontend_detector::DetectorReport {
            schema_version: super::super::frontend_detector::DETECTOR_REPORT_SCHEMA_VERSION,
            id: uuid::Uuid::new_v4().to_string(),
            repo: target_repo.path().canonicalize().unwrap(),
            change_fingerprint: super::super::verification::change_fingerprint(target_repo.path())
                .unwrap(),
            profile_fingerprint: profile.source_fingerprint,
            scope: super::super::frontend_detector::DetectorScope::Changed,
            generated_at: now_secs(),
            analyzed_files: vec![PathBuf::from("App.tsx")],
            analyzed_bytes: 64,
            truncated: false,
            findings: Vec::new(),
            waivers_loaded: 0,
            waivers_rejected: 0,
            not_applicable: false,
        };
        super::super::frontend_detector::save_report(&state_dir, &detector).unwrap();

        let mut classification = low_classification();
        classification.risk = RiskBand::Medium;
        classification.work_domain.domain = WorkDomain::Frontend;
        classification.work_domain.score = 55;
        let mut state = WorkflowState::start(
            workflow_repo.path().to_path_buf(),
            "review a frontend component".into(),
            WorkflowKind::Review,
            None,
            true,
            classification,
        );
        state.current_step = state
            .steps
            .iter()
            .position(|step| step.phase == WorkflowPhase::Review)
            .unwrap();
        state.frontend_target_root = Some(target_repo.path().canonicalize().unwrap());

        let error = advance_with_evidence(&state_dir, state, StepOutcome::Success, None, false)
            .unwrap_err()
            .to_string();

        assert!(error.contains("automatic rendered evidence"), "{error}");
        let target_display = target_repo
            .path()
            .canonicalize()
            .unwrap()
            .display()
            .to_string();
        let workflow_display = workflow_repo
            .path()
            .canonicalize()
            .unwrap()
            .display()
            .to_string();
        assert!(
            error.contains(&target_display),
            "expected the render gate error to name the target root: {error}"
        );
        assert!(
            !error.contains(&workflow_display),
            "render gate must not scan the workflow repo once frontend_target_root is set: {error}"
        );
    }

    #[test]
    fn approval_gate_must_be_explicitly_released() {
        let repo = tempdir().unwrap();
        let root = tempdir().unwrap();
        let state_dir = StateDir::from_root(root.path().to_path_buf());
        let mut classification = low_classification();
        classification.complexity = Complexity::Substantial;
        classification.risk = RiskBand::High;
        let state = WorkflowState::start(
            repo.path().to_path_buf(),
            "substantial feature".into(),
            WorkflowKind::Feature,
            None,
            true,
            classification,
        );
        assert_eq!(state.status, WorkflowStatus::AwaitingApproval);
        ensure_current_artifact_template(&state).unwrap();
        assert!(
            advance_with_evidence(&state_dir, state.clone(), StepOutcome::Success, None, false)
                .is_err()
        );
        assert!(
            approve(&state_dir, state.clone()).is_err(),
            "an untouched template cannot be accepted"
        );
        let intent = workflow_artifact_path(&state, ArtifactStage::Intent).unwrap();
        std::fs::write(
            intent,
            "# Intent\n\n## Problem\nConcrete problem\n\n## Desired outcome\nConcrete result\n",
        )
        .unwrap();
        let approved = approve(&state_dir, state).unwrap();
        assert_eq!(approved.current().unwrap().id, "spec");
        assert_eq!(approved.status, WorkflowStatus::AwaitingApproval);
        assert!(
            approved
                .artifacts
                .get("intent")
                .and_then(|record| record.accepted_hash.as_ref())
                .is_some()
        );
    }

    fn artifact_gate_state(repo: &Path) -> WorkflowState {
        WorkflowState::start(
            repo.to_path_buf(),
            "document the intent".into(),
            WorkflowKind::Bugfix,
            None,
            true,
            Classification {
                complexity: Complexity::Bounded,
                ..low_classification()
            },
        )
    }

    #[test]
    fn jev_artifact_refuses_template_copy_at_high_confidence() {
        let body = r#"{"model":"jev-latest","answers":{"substance":{"type":"choice","choice":"template_copy","probabilities":{"template_copy":0.95,"other":0.05},"confidence":0.95}},"usage":{"input_tokens":10,"output_tokens":1}}"#;
        let (url, request) = crate::commands::ctx::provider::testhttp::one_shot_server(
            200,
            body,
            "application/json",
        );
        let cfg = jev_gate_config(url, "JEV_TEST_ARTIFACT_COPY");
        let _credential = crate::commands::ctx::testenv::VarGuard::set(&[(
            "JEV_TEST_ARTIFACT_COPY",
            Some("secret"),
        )]);
        let repo = tempdir().unwrap();
        let root = tempdir().unwrap();
        let state_dir = StateDir::from_root(root.path().to_path_buf());
        let mut state = artifact_gate_state(repo.path());
        ensure_current_artifact_template(&state).unwrap();
        let path = workflow_artifact_path(&state, ArtifactStage::Intent).unwrap();
        std::fs::write(&path, "# Intent\n\n## Problem\nChanged wording\n").unwrap();

        let error = pin_current_artifact_with_config(&state_dir, &mut state, Some(&cfg))
            .unwrap_err()
            .to_string();
        request.recv().unwrap();

        assert!(
            error.contains("template_copy") && error.contains("0.95"),
            "{error}"
        );
        assert!(state.artifacts["intent"].accepted_hash.is_none());
    }

    /// Jev determinism fix: a `template_copy` verdict with a confidence
    /// (0.95) above `JEV_ARTIFACT_CONFIDENCE`, but a thin margin (0.51/0.49)
    /// between its own top and runner-up probability, must NOT refuse the
    /// artifact -- it falls through and pins exactly like the gate-off path.
    #[test]
    fn jev_artifact_pins_a_thin_margin_template_copy_verdict() {
        let body = r#"{"model":"jev-latest","answers":{"substance":{"type":"choice","choice":"template_copy","probabilities":{"template_copy":0.51,"substantive":0.49},"confidence":0.95}},"usage":{"input_tokens":10,"output_tokens":1}}"#;
        let (url, request) = crate::commands::ctx::provider::testhttp::one_shot_server(
            200,
            body,
            "application/json",
        );
        let cfg = jev_gate_config(url, "JEV_TEST_ARTIFACT_THIN_MARGIN");
        let _credential = crate::commands::ctx::testenv::VarGuard::set(&[(
            "JEV_TEST_ARTIFACT_THIN_MARGIN",
            Some("secret"),
        )]);
        let repo = tempdir().unwrap();
        let root = tempdir().unwrap();
        let state_dir = StateDir::from_root(root.path().to_path_buf());
        let mut state = artifact_gate_state(repo.path());
        ensure_current_artifact_template(&state).unwrap();
        let path = workflow_artifact_path(&state, ArtifactStage::Intent).unwrap();
        let body_text = "# Intent\n\n## Problem\nChanged wording\n";
        std::fs::write(&path, body_text).unwrap();

        let (stage, warning) =
            pin_current_artifact_with_config(&state_dir, &mut state, Some(&cfg)).unwrap();
        request.recv().unwrap();

        assert_eq!(stage, ArtifactStage::Intent);
        assert!(warning.is_none(), "{warning:?}");
        assert_eq!(
            state.artifacts["intent"].accepted_hash.as_deref(),
            Some(hash_bytes(body_text.as_bytes()).as_str())
        );
    }

    #[test]
    fn jev_artifact_warns_and_pins_thin_content_at_high_confidence() {
        let body = r#"{"model":"jev-latest","answers":{"substance":{"type":"choice","choice":"thin","probabilities":{"thin":0.95,"other":0.05},"confidence":0.95}},"usage":{"input_tokens":10,"output_tokens":1}}"#;
        let (url, request) = crate::commands::ctx::provider::testhttp::one_shot_server(
            200,
            body,
            "application/json",
        );
        let cfg = jev_gate_config(url, "JEV_TEST_ARTIFACT_THIN");
        let _credential = crate::commands::ctx::testenv::VarGuard::set(&[(
            "JEV_TEST_ARTIFACT_THIN",
            Some("secret"),
        )]);
        let repo = tempdir().unwrap();
        let root = tempdir().unwrap();
        let state_dir = StateDir::from_root(root.path().to_path_buf());
        let mut state = artifact_gate_state(repo.path());
        ensure_current_artifact_template(&state).unwrap();
        let path = workflow_artifact_path(&state, ArtifactStage::Intent).unwrap();
        std::fs::write(&path, "# Intent\n\nA sentence.\n").unwrap();

        let (_, warning) =
            pin_current_artifact_with_config(&state_dir, &mut state, Some(&cfg)).unwrap();
        request.recv().unwrap();

        let warning = warning.expect("thin content warns");
        assert!(
            warning.contains("thin") && warning.contains("0.95"),
            "{warning}"
        );
        assert!(state.artifacts["intent"].accepted_hash.is_some());
    }

    #[test]
    fn deterministic_template_equality_refuses_before_substantive_advice() {
        let body = r#"{"model":"jev-latest","answers":{"substance":{"type":"choice","choice":"substantive","probabilities":{"substantive":0.99,"other":0.01},"confidence":0.99}},"usage":{"input_tokens":10,"output_tokens":1}}"#;
        let (url, request) = crate::commands::ctx::provider::testhttp::one_shot_server(
            200,
            body,
            "application/json",
        );
        let cfg = jev_gate_config(url, "JEV_TEST_ARTIFACT_EQUALITY");
        let _credential = crate::commands::ctx::testenv::VarGuard::set(&[(
            "JEV_TEST_ARTIFACT_EQUALITY",
            Some("secret"),
        )]);
        let repo = tempdir().unwrap();
        let root = tempdir().unwrap();
        let state_dir = StateDir::from_root(root.path().to_path_buf());
        let mut state = artifact_gate_state(repo.path());
        ensure_current_artifact_template(&state).unwrap();

        let error = pin_current_artifact_with_config(&state_dir, &mut state, Some(&cfg))
            .unwrap_err()
            .to_string();

        assert!(error.contains("untouched template"), "{error}");
        assert!(
            request
                .recv_timeout(std::time::Duration::from_millis(100))
                .is_err(),
            "deterministic equality must refuse before calling Jev"
        );
    }

    #[test]
    fn jev_artifact_500_pins_exactly_like_gate_off() {
        let (url, request) = crate::commands::ctx::provider::testhttp::one_shot_server(
            500,
            "{}",
            "application/json",
        );
        let cfg = jev_gate_config(url, "JEV_TEST_ARTIFACT_500");
        let _credential = crate::commands::ctx::testenv::VarGuard::set(&[(
            "JEV_TEST_ARTIFACT_500",
            Some("secret"),
        )]);
        let repo = tempdir().unwrap();
        let root = tempdir().unwrap();
        let state_dir = StateDir::from_root(root.path().to_path_buf());
        let mut state = artifact_gate_state(repo.path());
        ensure_current_artifact_template(&state).unwrap();
        let path = workflow_artifact_path(&state, ArtifactStage::Intent).unwrap();
        let body = "# Intent\n\n## Problem\nConcrete problem\n";
        std::fs::write(&path, body).unwrap();

        let (stage, warning) =
            pin_current_artifact_with_config(&state_dir, &mut state, Some(&cfg)).unwrap();
        request.recv().unwrap();

        assert_eq!(stage, ArtifactStage::Intent);
        assert!(warning.is_none());
        assert_eq!(
            state.artifacts["intent"].accepted_hash.as_deref(),
            Some(hash_bytes(body.as_bytes()).as_str())
        );
    }

    /// Mirrors `skill::symlinked_manifests_are_refused` / `agents::load_dir`'s
    /// own symlink defense: a symlinked `.zirv/work/<id>` workflow directory
    /// must be refused before `ensure_current_artifact_template` ever creates
    /// or writes through it. `#[cfg(unix)]` for the same reason those two
    /// tests are: creating a real symlink needs elevated privileges on
    /// Windows, so this is verified on Linux/Docker instead (see the crate's
    /// own working instructions on cross-platform symlink tests).
    #[cfg(unix)]
    #[test]
    fn ensure_current_artifact_template_refuses_a_symlinked_workflow_directory() {
        use std::os::unix::fs::symlink;
        let repo = tempdir().unwrap();
        let outside = tempdir().unwrap();
        std::fs::create_dir_all(repo.path().join(".zirv/work")).unwrap();
        // Bounded complexity gates Feature's (now conditional) intent step in,
        // so `start` materializes the artifact record the template path needs.
        let state = WorkflowState::start(
            repo.path().to_path_buf(),
            "small feature".into(),
            WorkflowKind::Feature,
            None,
            true,
            Classification {
                complexity: Complexity::Bounded,
                ..low_classification()
            },
        );
        let workflow_dir = repo.path().join(".zirv/work").join(&state.id);
        symlink(outside.path(), &workflow_dir).unwrap();

        let error = ensure_current_artifact_template(&state)
            .unwrap_err()
            .to_string();
        assert!(error.contains("symlinked"), "{error}");
        assert!(
            !outside
                .path()
                .join(ArtifactStage::Intent.file_name())
                .exists(),
            "must refuse before ever writing through the symlink"
        );
    }

    /// Same defense, but the `.zirv/work` root itself is symlinked rather
    /// than the per-workflow directory beneath it.
    #[cfg(unix)]
    #[test]
    fn ensure_current_artifact_template_refuses_a_symlinked_work_root() {
        use std::os::unix::fs::symlink;
        let repo = tempdir().unwrap();
        let outside = tempdir().unwrap();
        std::fs::create_dir_all(repo.path().join(".zirv")).unwrap();
        symlink(outside.path(), repo.path().join(".zirv/work")).unwrap();
        let state = WorkflowState::start(
            repo.path().to_path_buf(),
            "small feature".into(),
            WorkflowKind::Feature,
            None,
            true,
            Classification {
                complexity: Complexity::Bounded,
                ..low_classification()
            },
        );

        let error = ensure_current_artifact_template(&state)
            .unwrap_err()
            .to_string();
        assert!(error.contains("symlinked"), "{error}");
    }

    /// `read_accepted_artifact` is the validated read `review.rs`'s
    /// `accepted_artifact_excerpt` funnels through instead of opening a
    /// `WorkflowArtifactRecord.rel_path` directly -- after acceptance, a
    /// repository writer replacing the accepted artifact file with a symlink
    /// to an arbitrary local file must be refused, not have its target's
    /// contents read and handed to an external review worker. Same
    /// `#[cfg(unix)]` rationale as the two tests above: a real symlink needs
    /// elevated privileges on Windows.
    #[cfg(unix)]
    #[test]
    fn read_accepted_artifact_refuses_a_symlinked_artifact_file() {
        use std::os::unix::fs::symlink;
        let repo = tempdir().unwrap();
        let outside = tempdir().unwrap();
        let secret = outside.path().join("secret.txt");
        std::fs::write(&secret, "top secret, not for the review worker").unwrap();

        let mut state = WorkflowState::start(
            repo.path().to_path_buf(),
            "small feature".into(),
            WorkflowKind::Feature,
            None,
            true,
            Classification {
                complexity: Complexity::Bounded,
                ..low_classification()
            },
        );
        ensure_current_artifact_template(&state).unwrap();
        let intent_path = workflow_artifact_path(&state, ArtifactStage::Intent).unwrap();
        std::fs::remove_file(&intent_path).unwrap();
        symlink(&secret, &intent_path).unwrap();
        state
            .artifacts
            .get_mut(ArtifactStage::Intent.key())
            .unwrap()
            .accepted_hash = Some("deadbeef".to_string());

        let error = read_accepted_artifact(&state, ArtifactStage::Intent)
            .unwrap_err()
            .to_string();
        assert!(error.contains("symlinked"), "{error}");
    }

    /// Review finding: an accepted artifact whose bytes no longer hash to the
    /// accepted value is not the accepted artifact, so the validated read
    /// yields `None` for it -- the same drift rule `append_accepted_artifacts`
    /// applies -- while an unmodified one still reads back.
    #[test]
    fn read_accepted_artifact_rejects_hash_drift_and_accepts_the_pinned_bytes() {
        let repo = tempdir().unwrap();
        let mut state = WorkflowState::start(
            repo.path().to_path_buf(),
            "small feature".into(),
            WorkflowKind::Feature,
            None,
            true,
            Classification {
                complexity: Complexity::Bounded,
                ..low_classification()
            },
        );
        ensure_current_artifact_template(&state).unwrap();
        let intent_path = workflow_artifact_path(&state, ArtifactStage::Intent).unwrap();
        std::fs::write(&intent_path, "# Intent\n\naccepted body\n").unwrap();
        let pinned = artifact_hash(&intent_path).unwrap();
        state
            .artifacts
            .get_mut(ArtifactStage::Intent.key())
            .unwrap()
            .accepted_hash = Some(pinned);

        let body = read_accepted_artifact(&state, ArtifactStage::Intent).unwrap();
        assert_eq!(body.as_deref(), Some("# Intent\n\naccepted body\n"));

        std::fs::write(&intent_path, "# Intent\n\nedited after acceptance\n").unwrap();
        assert_eq!(
            read_accepted_artifact(&state, ArtifactStage::Intent).unwrap(),
            None,
            "drifted bytes must never be handed on as the accepted artifact"
        );
    }

    /// Design spec risk-section commitment: `workflow start` warns (does not
    /// block) when `.zirv/work` would be gitignored, since a repo that
    /// ignores it silently loses every work-product artifact a workflow
    /// produces. Uses a real `git` shell-out, same as the frontend tests
    /// above (`git` is on PATH in this crate's own test environment).
    #[test]
    fn work_dir_is_gitignored_detects_a_zirv_work_gitignore_rule() {
        let repo = tempdir().unwrap();
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
        assert!(
            !work_dir_is_gitignored(repo.path()),
            "no .gitignore rule yet"
        );

        std::fs::write(repo.path().join(".gitignore"), ".zirv/\n").unwrap();
        assert!(
            work_dir_is_gitignored(repo.path()),
            "a .zirv/ gitignore rule covers .zirv/work"
        );
    }

    /// The probe is best-effort, never authoritative: a path with no git
    /// repository at all must read as "not ignored" rather than erroring
    /// `workflow start` on an environment `git` cannot make sense of.
    #[test]
    fn work_dir_is_gitignored_fails_open_outside_a_git_repository() {
        let not_a_repo = tempdir().unwrap();
        assert!(!work_dir_is_gitignored(not_a_repo.path()));
    }

    #[test]
    fn artifact_drift_reopens_the_owning_gate_and_invalidates_later_work() {
        let repo = tempdir().unwrap();
        let root = tempdir().unwrap();
        let state_dir = StateDir::from_root(root.path().to_path_buf());
        let mut classification = low_classification();
        classification.complexity = Complexity::Substantial;
        classification.risk = RiskBand::High;
        let state = WorkflowState::start(
            repo.path().to_path_buf(),
            "substantial feature".into(),
            WorkflowKind::Feature,
            None,
            true,
            classification,
        );
        ensure_current_artifact_template(&state).unwrap();
        let intent = workflow_artifact_path(&state, ArtifactStage::Intent).unwrap();
        std::fs::write(
            &intent,
            "# Intent\n\n## Problem\nA\n\n## Desired outcome\nB\n",
        )
        .unwrap();
        let state = approve(&state_dir, state).unwrap();
        assert_eq!(state.current().unwrap().id, "spec");

        std::fs::write(
            &intent,
            "# Intent\n\n## Problem\nChanged after acceptance\n\n## Desired outcome\nB\n",
        )
        .unwrap();
        let error = advance_with_evidence(&state_dir, state, StepOutcome::Success, None, false)
            .unwrap_err();
        assert!(error.to_string().contains("intent artifact changed"));

        let reopened = load_active(&state_dir, repo.path()).unwrap().unwrap();
        assert_eq!(reopened.current().unwrap().id, "intent");
        assert_eq!(reopened.status, WorkflowStatus::AwaitingApproval);
        assert!(
            reopened
                .artifacts
                .get("intent")
                .unwrap()
                .accepted_hash
                .is_none()
        );
    }

    #[test]
    fn workflow_artifact_status_reports_pending_accepted_and_drifted() {
        let repo = tempdir().unwrap();
        let root = tempdir().unwrap();
        let state_dir = StateDir::from_root(root.path().to_path_buf());
        // `approve` refreshes the deploy tier, which re-materializes `steps`
        // straight from `state.classification` -- so this needs a
        // classification that naturally keeps exactly one artifact step
        // (intent) through that regeneration. Bugfix's plan gate is
        // `ComplexityOrRisk{Substantial, High}` (unlike Feature's, which
        // shares intent's own `Bounded` threshold), so Bounded complexity
        // here gates intent in without also gating plan in.
        let mut classification = low_classification();
        classification.complexity = Complexity::Bounded;
        let state = WorkflowState::start(
            repo.path().to_path_buf(),
            "small bugfix".into(),
            WorkflowKind::Bugfix,
            None,
            true,
            classification,
        );
        ensure_current_artifact_template(&state).unwrap();
        let pending = workflow_artifact_statuses(&state).unwrap();
        assert_eq!(pending.len(), 1);
        assert!(pending[0].exists);
        assert!(!pending[0].accepted);

        let intent = workflow_artifact_path(&state, ArtifactStage::Intent).unwrap();
        std::fs::write(
            &intent,
            "# Intent\n\n## Problem\nA\n\n## Desired outcome\nB\n",
        )
        .unwrap();
        let accepted = approve(&state_dir, state).unwrap();
        let statuses = workflow_artifact_statuses(&accepted).unwrap();
        assert!(statuses[0].accepted);
        assert!(!statuses[0].drifted);

        std::fs::write(&intent, "# Intent\nchanged\n").unwrap();
        let statuses = workflow_artifact_statuses(&accepted).unwrap();
        assert!(statuses[0].drifted);
    }

    /// A step with no recorded duration (an older saved state) renders its
    /// bare id, never a bogus "0m0s".
    #[test]
    fn write_state_renders_completed_step_wall_clock_only_when_known() {
        let repo = tempdir().unwrap();
        let mut state = WorkflowState::start(
            repo.path().to_path_buf(),
            "small feature".into(),
            WorkflowKind::Feature,
            None,
            true,
            low_classification(),
        );
        state.completed_steps = vec!["intent".to_string(), "spec".to_string()];
        state
            .step_durations_ms
            .insert("intent".to_string(), 130_000);
        state.step_durations_ms.insert("spec".to_string(), 40_000);
        let mut out = Vec::new();
        write_state(&mut out, &state, false).unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(
            text.contains("completed: intent (2m10s), spec (0m40s)"),
            "got: {text}"
        );

        // No recorded duration for a step (an older schema, or a test
        // fixture that only sets `completed_steps` directly): the bare id,
        // not a fabricated duration.
        let mut legacy = state.clone();
        legacy.step_durations_ms.clear();
        let mut out = Vec::new();
        write_state(&mut out, &legacy, false).unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("completed: intent, spec"), "got: {text}");
    }

    #[test]
    fn write_state_renders_brainstorm_only_when_the_workflow_has_an_intent_step() {
        let repo = tempdir().unwrap();
        // Feature's intent step is now conditional (`ComplexityOrRisk{Bounded,
        // Medium}`, same as Bugfix), so this needs a classification that
        // still gates one in.
        let mut classification = low_classification();
        classification.complexity = Complexity::Bounded;
        let feature = WorkflowState::start(
            repo.path().to_path_buf(),
            "small feature".into(),
            WorkflowKind::Feature,
            None,
            true,
            classification,
        );
        let mut out = Vec::new();
        write_state(&mut out, &feature, false).unwrap();
        assert!(String::from_utf8(out).unwrap().contains("brainstorm: on"));

        let review = WorkflowState::start(
            repo.path().to_path_buf(),
            "independent review".into(),
            WorkflowKind::Review,
            None,
            true,
            low_classification(),
        );
        let mut out = Vec::new();
        write_state(&mut out, &review, false).unwrap();
        assert!(!String::from_utf8(out).unwrap().contains("brainstorm:"));
    }

    /// Issue #542 review finding 3: a REAL pre-#542 state file, not one
    /// synthesized from the current (post-#542) serializer by stripping the
    /// `definition` key back out. `tests/fixtures/workflow/state-v4/
    /// feature.json` is the literal, unmodified JSON `WorkflowState::save`
    /// wrote at base commit `eabc14db` (the pre-#542 `zirv workflow start
    /// feature` path: `schema_version: 4`, no `definition` key ever
    /// existed, a plain `WorkflowKind`-keyed run) -- captured by checking
    /// out that commit into a scratch worktree, adding a throwaway test
    /// that called its own `WorkflowState::start`/`save`, and copying the
    /// resulting file out verbatim. Only the `"repo"` field is rewritten
    /// below, to point at THIS test's own fresh temp repo -- an
    /// environment-specific path, not part of the schema being proven --
    /// everything else (`steps`, `status`, `current_step`, field presence)
    /// is the base commit's own serializer output, unedited.
    ///
    /// `load` upgrades this in place rather than refusing it, and
    /// `#[serde(default)]` on `WorkflowState::definition` means a v4 file
    /// with no such key deserializes as `None`, kind-only v1 semantics,
    /// exactly as before this schema existed.
    #[test]
    fn a_schema_four_state_file_still_loads_resumes_and_advances() {
        const GENUINE_V4_FIXTURE: &str =
            include_str!("../../../tests/fixtures/workflow/state-v4/feature.json");

        let repo = tempdir().unwrap();
        let root = tempdir().unwrap();
        let state_dir = StateDir::from_root(root.path().to_path_buf());

        let mut value: serde_json::Value = serde_json::from_str(GENUINE_V4_FIXTURE).unwrap();
        assert_eq!(value["schema_version"], serde_json::json!(4));
        assert!(
            value.as_object().unwrap().get("definition").is_none(),
            "the fixture must genuinely have no `definition` key, not merely a null one"
        );
        let id = value["id"].as_str().unwrap().to_string();
        let object = value.as_object_mut().unwrap();
        object.insert(
            "repo".into(),
            serde_json::json!(repo.path().to_string_lossy()),
        );
        let path = state_path(&state_dir, repo.path(), &id).unwrap();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, serde_json::to_string_pretty(&value).unwrap()).unwrap();

        let loaded = load(&state_dir, repo.path(), &id).unwrap();
        assert_eq!(loaded.schema_version, WORKFLOW_SCHEMA_VERSION);
        assert!(
            loaded.definition.is_none(),
            "a v4 file has no pin -- kind-only v1 semantics"
        );
        assert_eq!(loaded.current().unwrap().phase, WorkflowPhase::Implement);
        assert_eq!(loaded.status, WorkflowStatus::Running);
        assert_eq!(loaded.kind, WorkflowKind::Feature);

        // Resume-and-advance still works against the migrated state.
        let advanced =
            advance_with_evidence(&state_dir, loaded, StepOutcome::Success, None, false).unwrap();
        assert_eq!(advanced.current().unwrap().phase, WorkflowPhase::Test);
        assert_eq!(advanced.schema_version, WORKFLOW_SCHEMA_VERSION);

        // The upgrade is durable: a fresh load sees schema 5 on disk, not a
        // one-time in-memory patch.
        let reloaded = load(&state_dir, repo.path(), &id).unwrap();
        assert_eq!(reloaded.schema_version, WORKFLOW_SCHEMA_VERSION);
    }

    /// `load` is the single choke point every id-resolving verb (`status`,
    /// `resume`, `context`, `artifacts`, `approve`, `advance`) goes through --
    /// pinned here so a bogus id gets the domain-shaped "unknown workflow"
    /// message rather than `std::fs::read_to_string`'s raw OS error ("The
    /// system cannot find the path specified. (os error 3)" on Windows).
    #[test]
    fn load_reports_a_domain_error_for_an_unknown_workflow_id() {
        let repo = tempdir().unwrap();
        let root = tempdir().unwrap();
        let state_dir = StateDir::from_root(root.path().to_path_buf());

        let error = load(&state_dir, repo.path(), "does-not-exist")
            .unwrap_err()
            .to_string();

        assert!(error.contains("unknown workflow"), "{error}");
        assert!(error.contains("does-not-exist"), "{error}");
        assert!(
            !error.to_ascii_lowercase().contains("os error"),
            "must not leak a raw OS error: {error}"
        );
    }

    #[test]
    fn resume_does_not_redispatch_completed_steps() {
        let repo = tempdir().unwrap();
        let root = tempdir().unwrap();
        let state_dir = StateDir::from_root(root.path().to_path_buf());
        let mut state =
            skip_leading_artifact_steps(with_synthetic_intent_step(WorkflowState::start(
                repo.path().to_path_buf(),
                "small feature".into(),
                WorkflowKind::Feature,
                None,
                true,
                low_classification(),
            )));
        save(&state_dir, &state, true).unwrap();
        state =
            advance_with_evidence(&state_dir, state, StepOutcome::Success, None, false).unwrap();
        let resumed = load(&state_dir, repo.path(), &state.id).unwrap();
        assert_eq!(resumed.completed_steps, vec!["intent", "implement"]);
        assert_eq!(resumed.current().unwrap().id, "test");
    }

    #[test]
    fn failed_steps_have_a_hard_retry_limit() {
        let repo = tempdir().unwrap();
        let root = tempdir().unwrap();
        let state_dir = StateDir::from_root(root.path().to_path_buf());
        let mut state = skip_leading_artifact_steps(WorkflowState::start(
            repo.path().to_path_buf(),
            "small feature".into(),
            WorkflowKind::Feature,
            None,
            true,
            low_classification(),
        ));
        save(&state_dir, &state, true).unwrap();
        for _ in 0..MAX_STEP_ATTEMPTS {
            state = advance_with_evidence(&state_dir, state, StepOutcome::Failure, None, false)
                .unwrap();
        }
        assert_eq!(state.status, WorkflowStatus::Failed);
        assert!(load_active(&state_dir, repo.path()).unwrap().is_none());
    }

    /// #88: outside a git repository, `reclassify_at_gate` used to silently
    /// leave the risk band exactly as declared/measured at `workflow start`
    /// -- the safety net that exists specifically to catch a mismatch was
    /// inert exactly where it mattered most. It must now report the
    /// unmeasured state and escalate the band one step, adding whatever
    /// Review/Verify step the escalated band newly requires.
    #[test]
    fn reclassify_at_gate_fails_safe_when_git_is_unavailable_outside_a_repository() {
        let repo = tempdir().unwrap();
        let state_root = tempdir().unwrap();
        let state_dir = StateDir::from_root(state_root.path().to_path_buf());
        let mut state = WorkflowState::start(
            repo.path().to_path_buf(),
            "small feature".into(),
            WorkflowKind::Feature,
            None,
            true,
            low_classification(),
        );
        assert!(
            !state.steps.iter().any(|step| step.id == "review"),
            "the Low-risk fast path starts with no review step: {:?}",
            state.steps
        );
        state.current_step = state
            .steps
            .iter()
            .position(|step| step.phase == WorkflowPhase::Verify)
            .unwrap();

        reclassify_at_gate(&state_dir, &mut state, None);

        assert!(
            matches!(
                state.classification.risk_measurement,
                classify::RiskMeasurement::Unavailable { .. }
            ),
            "{:?}",
            state.classification
        );
        assert_eq!(state.classification.risk, RiskBand::Medium);
        assert!(
            state
                .classification
                .reasons
                .iter()
                .any(|reason| reason.contains("risk escalated"))
        );
        assert!(
            state.steps.iter().any(|step| step.id == "review"),
            "the escalated band newly requires review: {:?}",
            state.steps
        );
    }

    /// #88: a repository that exists but has no commits fails the same Git
    /// calls a non-repository does, and must fail the same safe way.
    #[test]
    fn reclassify_at_gate_fails_safe_when_the_repository_has_no_commits() {
        let repo = tempdir().unwrap();
        let state_root = tempdir().unwrap();
        let state_dir = StateDir::from_root(state_root.path().to_path_buf());
        let status = std::process::Command::new("git")
            .args(["init", "-q"])
            .current_dir(repo.path())
            .status()
            .expect("git init");
        assert!(status.success());
        let mut state = WorkflowState::start(
            repo.path().to_path_buf(),
            "small feature".into(),
            WorkflowKind::Feature,
            None,
            true,
            low_classification(),
        );
        state.current_step = state
            .steps
            .iter()
            .position(|step| step.phase == WorkflowPhase::Verify)
            .unwrap();

        reclassify_at_gate(&state_dir, &mut state, None);

        assert!(
            matches!(
                state.classification.risk_measurement,
                classify::RiskMeasurement::Unavailable { .. }
            ),
            "{:?}",
            state.classification
        );
        assert_eq!(state.classification.risk, RiskBand::Medium);
    }

    /// No change to behavior when measurement succeeds: reclassification
    /// with a real Git history still reports `Measured`.
    #[test]
    fn reclassify_at_gate_stays_measured_when_git_succeeds() {
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
        std::fs::write(repo.path().join("README.md"), "hello\n").unwrap();
        git(&["add", "."]);
        git(&["commit", "-q", "-m", "base"]);
        let mut state = WorkflowState::start(
            repo.path().to_path_buf(),
            "small feature".into(),
            WorkflowKind::Feature,
            None,
            true,
            low_classification(),
        );
        state.current_step = state
            .steps
            .iter()
            .position(|step| step.phase == WorkflowPhase::Verify)
            .unwrap();

        reclassify_at_gate(&state_dir, &mut state, None);

        assert_eq!(
            state.classification.risk_measurement,
            classify::RiskMeasurement::Measured
        );
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

    fn jev_gate_config(
        base_url: String,
        credential_env: &str,
    ) -> crate::commands::ctx::config::CtxConfig {
        let mut cfg = crate::commands::ctx::config::CtxConfig::default();
        cfg.jev.gates = true;
        cfg.proxy.typesafe.base_url = base_url;
        cfg.proxy.typesafe.credential_env = credential_env.to_string();
        cfg
    }

    #[test]
    fn jev_gate_sensitive_surface_raises_medium_to_high() {
        let body = r#"{"model":"jev-latest","answers":{"sensitive_surface":{"type":"noul","noul":0.95}},"usage":{"input_tokens":10,"output_tokens":1}}"#;
        let (url, request) = crate::commands::ctx::provider::testhttp::one_shot_server(
            200,
            body,
            "application/json",
        );
        let cfg = jev_gate_config(url, "JEV_TEST_GATE_SENSITIVE");
        let _credential = crate::commands::ctx::testenv::VarGuard::set(&[(
            "JEV_TEST_GATE_SENSITIVE",
            Some("secret"),
        )]);
        let root = tempdir().unwrap();
        let state_dir = StateDir::from_root(root.path().to_path_buf());
        let repo = tempdir().unwrap();
        let mut state = WorkflowState::start(
            repo.path().to_path_buf(),
            "change credentials".into(),
            WorkflowKind::Feature,
            None,
            true,
            low_classification(),
        );
        state.classification.risk = RiskBand::Medium;
        let mut measured = state.classification.clone();

        apply_jev_gate_advice(&cfg, &state_dir, &mut state, &mut measured);
        request.recv().unwrap();

        assert_eq!(measured.risk, RiskBand::High);
    }

    #[test]
    fn jev_gate_frontend_choice_sets_an_unset_frontend_domain() {
        let body = r#"{"model":"jev-latest","answers":{"work_domain":{"type":"choice","choice":"frontend","probabilities":{"frontend":0.95,"other":0.05},"confidence":0.95}},"usage":{"input_tokens":10,"output_tokens":1}}"#;
        let (url, request) = crate::commands::ctx::provider::testhttp::one_shot_server(
            200,
            body,
            "application/json",
        );
        let cfg = jev_gate_config(url, "JEV_TEST_GATE_FRONTEND");
        let _credential = crate::commands::ctx::testenv::VarGuard::set(&[(
            "JEV_TEST_GATE_FRONTEND",
            Some("secret"),
        )]);
        let root = tempdir().unwrap();
        let state_dir = StateDir::from_root(root.path().to_path_buf());
        let repo = tempdir().unwrap();
        let mut state = WorkflowState::start(
            repo.path().to_path_buf(),
            "update the view".into(),
            WorkflowKind::Feature,
            None,
            true,
            low_classification(),
        );
        let mut measured = state.classification.clone();

        apply_jev_gate_advice(&cfg, &state_dir, &mut state, &mut measured);
        request.recv().unwrap();

        assert_eq!(measured.work_domain.domain, WorkDomain::Frontend);
    }

    /// Jev determinism fix: a `frontend` choice with a confidence (0.95)
    /// above `JEV_FRONTEND_CONFIDENCE`, but a thin margin (0.51/0.49)
    /// between its own top and runner-up probability, must NOT set the
    /// domain -- it falls through exactly like a low-confidence answer.
    #[test]
    fn jev_gate_frontend_choice_with_a_thin_margin_never_sets_the_domain() {
        let body = r#"{"model":"jev-latest","answers":{"work_domain":{"type":"choice","choice":"frontend","probabilities":{"frontend":0.51,"backend":0.49},"confidence":0.95}},"usage":{"input_tokens":10,"output_tokens":1}}"#;
        let (url, request) = crate::commands::ctx::provider::testhttp::one_shot_server(
            200,
            body,
            "application/json",
        );
        let cfg = jev_gate_config(url, "JEV_TEST_GATE_FRONTEND_THIN_MARGIN");
        let _credential = crate::commands::ctx::testenv::VarGuard::set(&[(
            "JEV_TEST_GATE_FRONTEND_THIN_MARGIN",
            Some("secret"),
        )]);
        let root = tempdir().unwrap();
        let state_dir = StateDir::from_root(root.path().to_path_buf());
        let repo = tempdir().unwrap();
        let mut state = WorkflowState::start(
            repo.path().to_path_buf(),
            "update the view".into(),
            WorkflowKind::Feature,
            None,
            true,
            low_classification(),
        );
        let mut measured = state.classification.clone();

        apply_jev_gate_advice(&cfg, &state_dir, &mut state, &mut measured);
        request.recv().unwrap();

        assert_ne!(measured.work_domain.domain, WorkDomain::Frontend);
    }

    #[test]
    fn jev_gate_permissive_answers_never_lower_risk_or_unset_frontend() {
        let body = r#"{"model":"jev-latest","answers":{"sensitive_surface":{"type":"noul","noul":0.05},"work_domain":{"type":"choice","choice":"backend","probabilities":{"backend":0.99,"other":0.01},"confidence":0.99},"security":{"type":"noul","noul":0.05},"data":{"type":"noul","noul":0.05},"docs_only":{"type":"noul","noul":0.05},"devops":{"type":"noul","noul":0.05},"architecture":{"type":"noul","noul":0.05}},"usage":{"input_tokens":20,"output_tokens":7}}"#;
        let (url, request) = crate::commands::ctx::provider::testhttp::one_shot_server(
            200,
            body,
            "application/json",
        );
        let cfg = jev_gate_config(url, "JEV_TEST_GATE_NARROW");
        let _credential = crate::commands::ctx::testenv::VarGuard::set(&[(
            "JEV_TEST_GATE_NARROW",
            Some("secret"),
        )]);
        let root = tempdir().unwrap();
        let state_dir = StateDir::from_root(root.path().to_path_buf());
        let repo = tempdir().unwrap();
        let mut classification = low_classification();
        classification.risk = RiskBand::High;
        classification.work_domain.domain = WorkDomain::Frontend;
        let mut state = WorkflowState::start(
            repo.path().to_path_buf(),
            "existing frontend change".into(),
            WorkflowKind::Feature,
            None,
            true,
            classification.clone(),
        );
        let mut measured = classification.clone();

        apply_jev_gate_advice(&cfg, &state_dir, &mut state, &mut measured);
        request.recv().unwrap();

        assert_eq!(measured.risk, RiskBand::High);
        assert_eq!(measured.work_domain.domain, WorkDomain::Frontend);
        assert!(state.jev_tags.is_empty());
    }

    #[test]
    fn jev_gate_tags_accumulate_and_render_in_status() {
        let body = r#"{"model":"jev-latest","answers":{"security":{"type":"noul","noul":0.95},"data":{"type":"noul","noul":0.95},"docs_only":{"type":"noul","noul":0.95},"devops":{"type":"noul","noul":0.95},"architecture":{"type":"noul","noul":0.95}},"usage":{"input_tokens":20,"output_tokens":5}}"#;
        let (url, request) = crate::commands::ctx::provider::testhttp::one_shot_server(
            200,
            body,
            "application/json",
        );
        let cfg = jev_gate_config(url, "JEV_TEST_GATE_TAGS");
        let _credential =
            crate::commands::ctx::testenv::VarGuard::set(&[("JEV_TEST_GATE_TAGS", Some("secret"))]);
        let root = tempdir().unwrap();
        let state_dir = StateDir::from_root(root.path().to_path_buf());
        let repo = tempdir().unwrap();
        let mut state = WorkflowState::start(
            repo.path().to_path_buf(),
            "cross-cutting change".into(),
            WorkflowKind::Feature,
            None,
            true,
            low_classification(),
        );
        let mut measured = state.classification.clone();

        apply_jev_gate_advice(&cfg, &state_dir, &mut state, &mut measured);
        request.recv().unwrap();

        assert_eq!(
            state.jev_tags,
            vec!["architecture", "data", "devops", "docs-only", "security"]
        );
        let mut output = Vec::new();
        write_state(&mut output, &state, false).unwrap();
        let output = String::from_utf8(output).unwrap();
        assert!(
            output.contains("jev tags: architecture, data, devops, docs-only, security"),
            "{output}"
        );
    }

    #[test]
    fn jev_gate_500_leaves_state_identical_to_gate_off() {
        let (url, request) = crate::commands::ctx::provider::testhttp::one_shot_server(
            500,
            "{}",
            "application/json",
        );
        let cfg = jev_gate_config(url, "JEV_TEST_GATE_500");
        let _credential =
            crate::commands::ctx::testenv::VarGuard::set(&[("JEV_TEST_GATE_500", Some("secret"))]);
        let root = tempdir().unwrap();
        let state_dir = StateDir::from_root(root.path().to_path_buf());
        let repo = tempdir().unwrap();
        let mut state = WorkflowState::start(
            repo.path().to_path_buf(),
            "small change".into(),
            WorkflowKind::Feature,
            None,
            true,
            low_classification(),
        );
        let mut measured = state.classification.clone();
        let expected_state = state.clone();
        let expected_measured = measured.clone();

        apply_jev_gate_advice(&cfg, &state_dir, &mut state, &mut measured);
        request.recv().unwrap();

        assert_eq!(state, expected_state);
        assert_eq!(measured, expected_measured);
    }

    #[test]
    fn phase_usage_reads_only_the_appended_claude_transcript() {
        let home = tempdir().unwrap();
        let repo = crate::commands::ctx::testenv::repo();
        let _home = crate::commands::ctx::testenv::EnvGuard::set(home.path(), None);
        let session_id = "11111111-2222-4333-8444-555555555555";
        let path = transcript_path(repo.path(), session_id, "claude").expect("path");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(
            &path,
            concat!(
                r#"{"type":"assistant","message":{"usage":{"input_tokens":100,"output_tokens":10}}}"#,
                "\n"
            ),
        )
        .unwrap();
        let checkpoint = UsageCheckpoint {
            session_id: session_id.into(),
            adapter: "claude".into(),
            transcript_bytes: std::fs::metadata(&path).unwrap().len(),
            cumulative_input_tokens: 0,
            cumulative_cache_creation_input_tokens: 0,
            cumulative_cache_read_input_tokens: 0,
            cumulative_output_tokens: 0,
        };
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        writeln!(
            file,
            r#"{{"type":"assistant","message":{{"usage":{{"input_tokens":7,"cache_read_input_tokens":5,"output_tokens":3}}}}}}"#
        )
        .unwrap();

        let usage = usage_since(repo.path(), &checkpoint).expect("usage");
        assert_eq!(
            usage,
            crate::commands::ctx::event::TranscriptUsage {
                input_tokens: 7,
                cache_creation_input_tokens: 0,
                cache_read_input_tokens: 5,
                output_tokens: 3,
            }
        );
        assert_eq!(usage.context_total(), 12, "the pre-2.34.0 combined number");
    }

    /// The sidechain counterpart of `phase_usage_reads_only_the_appended_
    /// claude_transcript`: a subagent turn appended in the same byte range
    /// must be counted, and only that range -- not the whole transcript.
    #[test]
    fn sidechain_usage_since_reads_only_the_appended_sidechain_rows() {
        let home = tempdir().unwrap();
        let repo = crate::commands::ctx::testenv::repo();
        let _home = crate::commands::ctx::testenv::EnvGuard::set(home.path(), None);
        let session_id = "11111111-2222-4333-8444-555555555555";
        let path = transcript_path(repo.path(), session_id, "claude").expect("path");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(
            &path,
            concat!(
                r#"{"type":"assistant","isSidechain":true,"message":{"usage":{"input_tokens":1000,"output_tokens":1000}}}"#,
                "\n"
            ),
        )
        .unwrap();
        let checkpoint = UsageCheckpoint {
            session_id: session_id.into(),
            adapter: "claude".into(),
            transcript_bytes: std::fs::metadata(&path).unwrap().len(),
            cumulative_input_tokens: 0,
            cumulative_cache_creation_input_tokens: 0,
            cumulative_cache_read_input_tokens: 0,
            cumulative_output_tokens: 0,
        };
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        writeln!(
            file,
            r#"{{"type":"assistant","isSidechain":true,"message":{{"usage":{{"input_tokens":40,"cache_read_input_tokens":12000,"output_tokens":90}}}}}}"#
        )
        .unwrap();

        let usage = sidechain_usage_since(repo.path(), &checkpoint).expect("sidechain usage");
        assert_eq!(
            usage,
            crate::commands::ctx::event::TranscriptUsage {
                input_tokens: 40,
                cache_creation_input_tokens: 0,
                cache_read_input_tokens: 12_000,
                output_tokens: 90,
            }
        );

        let codex_checkpoint = UsageCheckpoint {
            adapter: "codex".into(),
            ..checkpoint
        };
        assert_eq!(
            sidechain_usage_since(repo.path(), &codex_checkpoint),
            None,
            "sidechain rows are a claude transcript concept, not a general adapter one"
        );
    }

    /// Current Claude Code writes NO `isSidechain` rows into the main
    /// transcript at all (0 of 15,510 rows across 12 recorded real sessions);
    /// subagent turns live in `<transcript-dir>/<session-id>/subagents/
    /// agent-<id>.jsonl` instead. With only the in-file branch, phase
    /// telemetry's sidechain bucket was permanently `None`.
    #[test]
    fn sidechain_usage_reads_the_subagents_directory() {
        let home = tempdir().unwrap();
        let repo = crate::commands::ctx::testenv::repo();
        let _home = crate::commands::ctx::testenv::EnvGuard::set(home.path(), None);
        let session_id = "11111111-2222-4333-8444-555555555556";
        let path = transcript_path(repo.path(), session_id, "claude").expect("path");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(
            &path,
            concat!(
                r#"{"type":"assistant","timestamp":"2026-09-06T09:00:00.000Z","message":{"id":"m0","usage":{"input_tokens":1}}}"#,
                "\n"
            ),
        )
        .unwrap();
        let checkpoint = UsageCheckpoint {
            session_id: session_id.into(),
            adapter: "claude".into(),
            transcript_bytes: std::fs::metadata(&path).unwrap().len(),
            cumulative_input_tokens: 0,
            cumulative_cache_creation_input_tokens: 0,
            cumulative_cache_read_input_tokens: 0,
            cumulative_output_tokens: 0,
        };
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        writeln!(
            file,
            r#"{{"type":"assistant","timestamp":"2026-09-06T10:00:00.000Z","message":{{"id":"m1","usage":{{"input_tokens":5}}}}}}"#
        )
        .unwrap();

        let subagents = path.parent().unwrap().join(session_id).join("subagents");
        std::fs::create_dir_all(&subagents).unwrap();
        std::fs::write(
            subagents.join("agent-aaaa.jsonl"),
            concat!(
                // Before the phase boundary: another phase's subagent.
                r#"{"type":"assistant","isSidechain":true,"timestamp":"2026-09-06T08:00:00.000Z","message":{"id":"s0","usage":{"input_tokens":777}}}"#,
                "\n",
                // Inside the phase, split across three content-block rows.
                r#"{"type":"assistant","isSidechain":true,"timestamp":"2026-09-06T10:30:00.000Z","message":{"id":"s1","usage":{"input_tokens":40,"cache_read_input_tokens":12000,"output_tokens":90}}}"#,
                "\n",
                r#"{"type":"assistant","isSidechain":true,"timestamp":"2026-09-06T10:30:00.000Z","message":{"id":"s1","usage":{"input_tokens":40,"cache_read_input_tokens":12000,"output_tokens":90}}}"#,
                "\n"
            ),
        )
        .unwrap();
        std::fs::write(
            subagents.join("agent-bbbb.jsonl"),
            concat!(
                r#"{"type":"assistant","isSidechain":true,"timestamp":"2026-09-06T11:00:00.000Z","message":{"id":"s2","usage":{"input_tokens":3,"output_tokens":4}}}"#,
                "\n"
            ),
        )
        .unwrap();

        let usage = sidechain_usage_since(repo.path(), &checkpoint).expect("sidechain usage");
        assert_eq!(
            usage,
            crate::commands::ctx::event::TranscriptUsage {
                input_tokens: 43,
                cache_creation_input_tokens: 0,
                cache_read_input_tokens: 12_000,
                output_tokens: 94,
            }
        );
    }

    /// Wiring test for issue #155 Phase 2: a completed phase must attribute
    /// spend to the session that produced it, and bucket subagent spend
    /// separately from the main session's own numbers instead of dropping it.
    #[test]
    fn enrich_transition_evidence_buckets_sidechain_spend_and_records_session_lineage() {
        let home = tempdir().unwrap();
        let repo = crate::commands::ctx::testenv::repo();
        let _home = crate::commands::ctx::testenv::EnvGuard::set(home.path(), None);
        let session_id = "11111111-2222-4333-8444-555555555555";
        let _vars = crate::commands::ctx::testenv::VarGuard::set(&[
            (
                crate::commands::ctx::adapters::SESSION_ENV,
                Some(session_id),
            ),
            (crate::commands::ctx::adapters::AGENT_ENV, Some("claude")),
        ]);
        let path = transcript_path(repo.path(), session_id, "claude").expect("path");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(
            &path,
            concat!(
                r#"{"type":"assistant","message":{"usage":{"input_tokens":100,"output_tokens":10}}}"#,
                "\n"
            ),
        )
        .unwrap();
        let checkpoint = UsageCheckpoint {
            session_id: session_id.into(),
            adapter: "claude".into(),
            transcript_bytes: std::fs::metadata(&path).unwrap().len(),
            cumulative_input_tokens: 0,
            cumulative_cache_creation_input_tokens: 0,
            cumulative_cache_read_input_tokens: 0,
            cumulative_output_tokens: 0,
        };
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        writeln!(
            file,
            r#"{{"type":"assistant","message":{{"usage":{{"input_tokens":7,"cache_read_input_tokens":5,"output_tokens":3}}}}}}"#
        )
        .unwrap();
        writeln!(
            file,
            r#"{{"type":"assistant","isSidechain":true,"message":{{"usage":{{"input_tokens":40,"cache_read_input_tokens":12000,"output_tokens":90}}}}}}"#
        )
        .unwrap();
        drop(file);

        let mut state = WorkflowState::start(
            repo.path().to_path_buf(),
            "small feature".into(),
            WorkflowKind::Feature,
            None,
            true,
            low_classification(),
        );
        state.usage_checkpoint = Some(checkpoint);

        let evidence = enrich_transition_evidence(&mut state, TransitionEvidence::default());

        assert_eq!(evidence.cache_creation_input_tokens, Some(0));
        assert_eq!(evidence.cache_read_input_tokens, Some(5));
        assert_eq!(
            evidence.sidechain_input_tokens,
            Some(40),
            "subagent spend must be counted rather than dropped"
        );
        assert_eq!(evidence.sidechain_cache_creation_input_tokens, Some(0));
        assert_eq!(evidence.sidechain_cache_read_input_tokens, Some(12_000));
        assert_eq!(evidence.sidechain_output_tokens, Some(90));
        assert_eq!(evidence.session_id.as_deref(), Some(session_id));
    }

    /// Issue #326: `workflow.max_context_bytes` caps `render_current_
    /// context`'s own output -- the single source both `prompt::with_
    /// workflow_layer` and `zirv workflow context` render from -- rather
    /// than injecting a large step's resolved skill instructions unbounded.
    /// A substantial `Feature` classification composes several real skills
    /// (`worktree`/`implement`/`execute-plan`, per `substantial_
    /// implementation_composes_execute_plan_and_worktree` above), comfortably
    /// over the tiny cap this test forces, so the cut is real, not
    /// coincidental. The cut must still lead with the task/step header
    /// (`cap_workflow_context` keeps the head) and end with a visible
    /// marker naming both the omitted byte count and the config key, never
    /// silence.
    #[test]
    fn workflow_context_over_the_configured_cap_is_truncated_with_a_visible_marker() {
        let repo = tempdir().unwrap();
        let _vars = crate::commands::ctx::testenv::VarGuard::set(&[(
            "ZIRV_CTX_WORKFLOW_MAX_CONTEXT_BYTES",
            Some("300"),
        )]);
        let mut classification = low_classification();
        classification.complexity = Complexity::Substantial;
        let state = skip_leading_artifact_steps(WorkflowState::start(
            repo.path().to_path_buf(),
            "substantial feature".into(),
            WorkflowKind::Feature,
            None,
            true,
            classification,
        ));
        let context = render_current_context(&state, repo.path(), None)
            .unwrap()
            .unwrap();
        assert!(
            context.len() <= 300,
            "must not exceed the configured cap: {} bytes:\n{context}",
            context.len()
        );
        assert!(
            context.contains("workflow context truncated")
                && context.contains("workflow.max_context_bytes=300"),
            "a cut must leave a visible marker, not silence: {context}"
        );
        assert!(
            context.starts_with("zirv workflow step"),
            "the head (task/step header) must be kept, not dropped: {context}"
        );
    }

    /// Review finding on the test above: `cap_workflow_context` used to
    /// compute `keep = max_bytes.saturating_sub(marker.len())` and still
    /// append the FULL marker regardless, so a `max_context_bytes` small
    /// enough that the marker alone does not fit produced output LARGER
    /// than its own cap -- a "capped" render that was not actually capped.
    /// Every value here, including ones far smaller than the marker's own
    /// length, must yield output that never exceeds `max_bytes`.
    #[test]
    fn cap_workflow_context_never_exceeds_its_own_budget_even_when_the_marker_does_not_fit() {
        let rendered = "x".repeat(500);
        for cap in [0usize, 1, 10, 30, 60, 8192] {
            let capped = cap_workflow_context(rendered.clone(), cap);
            assert!(
                capped.len() <= cap,
                "cap {cap}: output must never exceed its own budget, got {} bytes: {capped:?}",
                capped.len()
            );
        }
    }

    #[test]
    fn switching_steps_replaces_ephemeral_skill_context() {
        let repo = tempdir().unwrap();
        let mut state = skip_leading_artifact_steps(WorkflowState::start(
            repo.path().to_path_buf(),
            "small feature".into(),
            WorkflowKind::Feature,
            None,
            true,
            low_classification(),
        ));
        let implement = render_current_context(&state, repo.path(), None)
            .unwrap()
            .unwrap();
        assert!(implement.contains("task: small feature"));
        state.completed_steps.push("implement".into());
        state.current_step += 1;
        let testing = render_current_context(&state, repo.path(), None)
            .unwrap()
            .unwrap();
        assert!(implement.contains("[skill implement@1"));
        assert!(!testing.contains("[skill implement@1"));
        assert!(testing.contains("[skill testing@1"));
    }

    /// Issue #539 (chunk C): a skill header must name a hash an operator or
    /// a resumed session can compare against the registry's own
    /// `content_hash`, so a skill that changed underneath a session is
    /// detectable rather than silently re-activated under the same id.
    #[test]
    fn rendered_step_context_names_the_skill_version_and_a_twelve_char_hash() {
        let repo = tempdir().unwrap();
        let state = skip_leading_artifact_steps(WorkflowState::start(
            repo.path().to_path_buf(),
            "small feature".into(),
            WorkflowKind::Feature,
            None,
            true,
            low_classification(),
        ));
        let context = render_current_context(&state, repo.path(), None)
            .unwrap()
            .unwrap();
        let registry = SkillRegistry::load_for_repo(repo.path(), None, true).unwrap();
        let skill = registry.get("implement").unwrap();
        let expected_hash = &skill.content_hash[..skill.content_hash.len().min(12)];
        assert_eq!(expected_hash.len(), 12);
        let expected_header = format!("[skill implement@1; source=built-in; hash={expected_hash}]");
        assert!(
            context.contains(&expected_header),
            "expected header {expected_header:?} in:\n{context}"
        );
    }

    #[test]
    fn substantial_implementation_composes_execute_plan_and_worktree() {
        let repo = tempdir().unwrap();
        let mut classification = low_classification();
        classification.complexity = Complexity::Substantial;
        let state = skip_leading_artifact_steps(WorkflowState::start(
            repo.path().to_path_buf(),
            "substantial feature".into(),
            WorkflowKind::Feature,
            None,
            true,
            classification,
        ));
        let context = render_current_context(&state, repo.path(), None)
            .unwrap()
            .unwrap();
        assert!(context.contains("[skill worktree@1"));
        assert!(context.contains("[skill implement@1"));
        assert!(context.contains("[skill execute-plan@1"));
    }

    #[test]
    fn trivial_implementation_does_not_pay_execute_plan_or_worktree_context() {
        let repo = tempdir().unwrap();
        let state = skip_leading_artifact_steps(WorkflowState::start(
            repo.path().to_path_buf(),
            "small feature".into(),
            WorkflowKind::Feature,
            None,
            true,
            low_classification(),
        ));
        let context = render_current_context(&state, repo.path(), None)
            .unwrap()
            .unwrap();
        assert!(context.contains("[skill implement@1"));
        assert!(!context.contains("[skill execute-plan@1"));
        assert!(!context.contains("[skill worktree@1"));
    }

    #[test]
    fn refusal_for_only_fires_for_brainstorm_when_headless() {
        assert_eq!(
            refusal_for("brainstorm", true),
            Some(BRAINSTORM_HEADLESS_REFUSAL)
        );
        assert_eq!(refusal_for("brainstorm", false), None);
        assert_eq!(refusal_for("write-intent", true), None);
    }

    /// Only the exact value `"1"` means headless -- an interactive launch
    /// that inherited the variable set to `"0"`, empty, or anything else
    /// from its own parent process must not be refused.
    #[test]
    fn is_headless_env_requires_the_exact_value_1() {
        assert!(is_headless_env(Some("1")));
        assert!(!is_headless_env(Some("0")));
        assert!(!is_headless_env(Some("")));
        assert!(!is_headless_env(Some("true")));
        assert!(!is_headless_env(None));
    }

    #[test]
    fn a_headless_worker_refuses_the_brainstorm_step() {
        let repo = tempdir().unwrap();
        // Feature's intent step is now conditional (`ComplexityOrRisk{Bounded,
        // Medium}`, same as Bugfix), so this needs a classification that
        // still gates one in to exercise the headless refusal at that step.
        let mut classification = low_classification();
        classification.complexity = Complexity::Bounded;
        let state = WorkflowState::start(
            repo.path().to_path_buf(),
            "small feature".into(),
            WorkflowKind::Feature,
            None,
            true,
            classification,
        );
        assert_eq!(state.current().unwrap().skill, "brainstorm");
        // SAFETY: nextest runs one test per process.
        unsafe {
            std::env::set_var(crate::commands::ctx::adapters::HEADLESS_ENV, "1");
        }
        let context = render_current_context(&state, repo.path(), None).unwrap();
        unsafe {
            std::env::remove_var(crate::commands::ctx::adapters::HEADLESS_ENV);
        }
        let context = context.unwrap();
        assert!(context.contains(BRAINSTORM_HEADLESS_REFUSAL));
        assert!(!context.contains("Explore the repository"));
    }

    #[test]
    fn built_in_only_state_survives_prompt_rendering() {
        let repo = tempdir().unwrap();
        let skills = repo.path().join(".zirv/skills");
        std::fs::create_dir_all(&skills).unwrap();
        std::fs::write(
            skills.join("implement.yaml"),
            "schema_version: 1\nid: implement\nversion: 2\nname: Override\ndescription: untrusted override\ncontext_budget_bytes: 64\nphases: [implement]\ninstructions: repository override\n",
        )
        .unwrap();
        let state = skip_leading_artifact_steps(WorkflowState::start(
            repo.path().to_path_buf(),
            "small feature".into(),
            WorkflowKind::Feature,
            None,
            false,
            low_classification(),
        ));
        let context = render_current_context(&state, repo.path(), None)
            .unwrap()
            .unwrap();
        assert!(context.contains("[skill implement@1; source=built-in; hash="));
        assert!(!context.contains("repository override"));
    }

    #[test]
    fn review_step_cannot_pass_with_open_findings() {
        let repo = tempdir().unwrap();
        let root = tempdir().unwrap();
        let state_dir = StateDir::from_root(root.path().to_path_buf());
        let mut state = WorkflowState::start(
            repo.path().to_path_buf(),
            "review change".into(),
            WorkflowKind::Review,
            None,
            true,
            low_classification(),
        );
        state
            .review_findings
            .push(super::super::review::ReviewFinding {
                id: "finding-1".into(),
                severity: super::super::review::FindingSeverity::Major,
                summary: "concrete defect".into(),
                path: None,
                line: None,
                disposition: super::super::review::FindingDisposition::Open,
                recommended_disposition: None,
                advisory_disposition: None,
                advisory_confidence: None,
                duplicate_of: None,
                created_at: now_secs(),
            });
        let error = advance_with_evidence(&state_dir, state, StepOutcome::Success, None, false)
            .unwrap_err();
        assert!(error.to_string().contains("final disposition"));
    }

    #[test]
    fn close_refuses_with_an_open_review_finding() {
        let repo = tempdir().unwrap();
        let root = tempdir().unwrap();
        let state_dir = StateDir::from_root(root.path().to_path_buf());
        let mut state = WorkflowState::start(
            repo.path().to_path_buf(),
            "small feature".into(),
            WorkflowKind::Feature,
            None,
            true,
            low_classification(),
        );
        state.review_findings.push(review_finding(
            "finding-1",
            super::super::review::FindingDisposition::Open,
            None,
        ));
        save(&state_dir, &state, true).unwrap();

        let error = close(&state_dir, state, None).unwrap_err();
        assert!(error.to_string().contains("open review finding"), "{error}");
    }

    #[test]
    fn close_refuses_while_awaiting_approval() {
        let repo = tempdir().unwrap();
        let root = tempdir().unwrap();
        let state_dir = StateDir::from_root(root.path().to_path_buf());
        let mut state = WorkflowState::start(
            repo.path().to_path_buf(),
            "small feature".into(),
            WorkflowKind::Feature,
            None,
            true,
            low_classification(),
        );
        state.status = WorkflowStatus::AwaitingApproval;
        save(&state_dir, &state, true).unwrap();

        let error = close(&state_dir, state, None).unwrap_err();
        assert!(error.to_string().contains("awaiting approval"), "{error}");
    }

    /// Issue #537 review: the COMMON case a proxy-started workflow hits, not
    /// an edge one -- `bugfix`'s own pack gates its `intent` step behind
    /// `approval = true` for anything Bounded-or-riskier, so a freshly
    /// started workflow at that classification is `AwaitingApproval` before
    /// anyone has seen or acted on the prompt. `close_unstarted` must still
    /// close it (nothing has completed, nothing is accepted); the same
    /// workflow, after a human actually approves that first gate, must
    /// refuse via this path exactly like `close` already refuses -- a human
    /// has now acted on it, so only `close` applies from here on.
    #[test]
    fn close_unstarted_closes_a_fresh_gate_but_refuses_once_a_human_has_approved_it() {
        let repo = tempdir().unwrap();
        let root = tempdir().unwrap();
        let state_dir = StateDir::from_root(root.path().to_path_buf());
        let mut classification = low_classification();
        classification.complexity = Complexity::Bounded;
        classification.risk = RiskBand::Medium;
        let state = WorkflowState::start(
            repo.path().to_path_buf(),
            "bounded bugfix".into(),
            WorkflowKind::Bugfix,
            None,
            true,
            classification,
        );
        assert_eq!(state.status, WorkflowStatus::AwaitingApproval);
        assert_eq!(state.current().unwrap().id, "intent");
        assert!(state.completed_steps.is_empty());

        // A clone taken before anything else happens: exactly the shape a
        // proxy-started workflow whose spawn immediately failed is in.
        let closed = close_unstarted(
            &state_dir,
            state.clone(),
            Some("proxy launch failed".to_string()),
        )
        .expect("closes a workflow that never progressed past its first gate");
        assert_eq!(closed.status, WorkflowStatus::Closed);
        assert_eq!(closed.closed_reason.as_deref(), Some("proxy launch failed"));

        // The same workflow, but a human has since approved the first gate:
        // `close_unstarted` must now refuse, the same as `close` already
        // does for every workflow it will not touch.
        ensure_current_artifact_template(&state).unwrap();
        std::fs::write(
            workflow_artifact_path(&state, ArtifactStage::Intent).unwrap(),
            "# Intent\n\n## Problem\nConcrete problem\n\n## Desired outcome\nConcrete result\n",
        )
        .unwrap();
        let approved = approve(&state_dir, state).expect("approve intent");
        assert!(
            !approved.completed_steps.is_empty(),
            "approving the first gate must record a completed step"
        );
        // Approving `intent` advances past it (the next step, `debug`, is
        // unconditional), so this no longer even reads as "awaiting
        // approval at the first gate" -- refused either way, but naming
        // which guard actually catches it keeps this test honest about why.
        assert_ne!(
            approved.status,
            WorkflowStatus::AwaitingApproval,
            "approving the only gated step must advance past it"
        );
        let error =
            close_unstarted(&state_dir, approved, Some("too late".to_string())).unwrap_err();
        assert!(
            error.to_string().contains("first gate"),
            "must refuse once a human has advanced the workflow: {error}"
        );
    }

    #[test]
    fn close_succeeds_after_residual_dispositions_and_clears_active() {
        let repo = tempdir().unwrap();
        let root = tempdir().unwrap();
        let state_dir = StateDir::from_root(root.path().to_path_buf());
        let mut state = WorkflowState::start(
            repo.path().to_path_buf(),
            "small feature".into(),
            WorkflowKind::Feature,
            None,
            true,
            low_classification(),
        );
        // Reached MAX_FIX_REVIEW_ROUNDS: every finding has a residual (or
        // otherwise resolved) disposition, none left Open.
        state.review_findings.push(review_finding(
            "finding-1",
            super::super::review::FindingDisposition::Residual,
            None,
        ));
        save(&state_dir, &state, true).unwrap();
        assert!(
            load_active(&state_dir, repo.path())
                .unwrap()
                .is_some_and(|active| active.id == state.id)
        );

        let closed = close(&state_dir, state, Some("hit MAX_FIX_REVIEW_ROUNDS".into())).unwrap();
        assert_eq!(closed.status, WorkflowStatus::Closed);
        assert_eq!(
            closed.closed_reason.as_deref(),
            Some("hit MAX_FIX_REVIEW_ROUNDS")
        );
        assert!(closed.closed_at.is_some());

        assert!(load_active(&state_dir, repo.path()).unwrap().is_none());
        // The state itself is still readable by id, just no longer active.
        let reloaded = load(&state_dir, repo.path(), &closed.id).unwrap();
        assert_eq!(reloaded.status, WorkflowStatus::Closed);
    }

    #[test]
    fn advance_with_evidence_refuses_a_closed_workflow_instead_of_resurrecting_it() {
        let repo = tempdir().unwrap();
        let root = tempdir().unwrap();
        let state_dir = StateDir::from_root(root.path().to_path_buf());
        let state = WorkflowState::start(
            repo.path().to_path_buf(),
            "small feature".into(),
            WorkflowKind::Feature,
            None,
            true,
            low_classification(),
        );
        save(&state_dir, &state, true).unwrap();
        let closed = close(&state_dir, state, Some("done".into())).unwrap();
        assert_eq!(closed.status, WorkflowStatus::Closed);

        let error = advance_with_evidence(&state_dir, closed, StepOutcome::Success, None, false)
            .unwrap_err();
        assert!(error.to_string().contains("Closed"), "{error}");
    }

    #[test]
    fn close_records_a_telemetry_event() {
        let repo = tempdir().unwrap();
        let root = tempdir().unwrap();
        let state_dir = StateDir::from_root(root.path().to_path_buf());
        let state = WorkflowState::start(
            repo.path().to_path_buf(),
            "small feature".into(),
            WorkflowKind::Feature,
            None,
            true,
            low_classification(),
        );
        save(&state_dir, &state, true).unwrap();
        let workflow_id = state.id.clone();

        close(&state_dir, state, None).unwrap();

        let events = super::super::telemetry::list(&state_dir, repo.path()).unwrap();
        assert!(
            events.iter().any(
                |event| event.kind == super::super::telemetry::TelemetryKind::Closed
                    && event.workflow_id.as_deref() == Some(workflow_id.as_str())
            ),
            "{events:?}"
        );
    }

    #[test]
    fn close_leaves_a_different_active_workflows_pointer_intact() {
        let repo = tempdir().unwrap();
        let root = tempdir().unwrap();
        let state_dir = StateDir::from_root(root.path().to_path_buf());

        let active_state = WorkflowState::start(
            repo.path().to_path_buf(),
            "active feature".into(),
            WorkflowKind::Feature,
            None,
            true,
            low_classification(),
        );
        let other_state = WorkflowState::start(
            repo.path().to_path_buf(),
            "other feature".into(),
            WorkflowKind::Feature,
            None,
            true,
            low_classification(),
        );
        // Persist both workflow files, then make sure the active pointer
        // ends up naming `active_state`: write `other_state` first (so its
        // own file exists on disk), then re-save `active_state` as active,
        // which repoints the pointer without touching `other_state`'s file.
        save(&state_dir, &other_state, true).unwrap();
        save(&state_dir, &active_state, true).unwrap();
        assert!(
            load_active(&state_dir, repo.path())
                .unwrap()
                .is_some_and(|state| state.id == active_state.id)
        );

        // `other_state` is Running but is NOT this repo's active workflow.
        let closed = close(&state_dir, other_state.clone(), None).unwrap();
        assert_eq!(closed.status, WorkflowStatus::Closed);

        let active = load_active(&state_dir, repo.path()).unwrap();
        assert!(
            active.is_some_and(|state| state.id == active_state.id),
            "closing a non-active workflow cleared a different workflow's active pointer"
        );
    }

    #[test]
    fn close_refuses_an_already_completed_workflow() {
        let repo = tempdir().unwrap();
        let root = tempdir().unwrap();
        let state_dir = StateDir::from_root(root.path().to_path_buf());
        let mut state = WorkflowState::start(
            repo.path().to_path_buf(),
            "small feature".into(),
            WorkflowKind::Feature,
            None,
            true,
            low_classification(),
        );
        state.status = WorkflowStatus::Completed;
        save(&state_dir, &state, false).unwrap();

        let error = close(&state_dir, state.clone(), None).unwrap_err();
        assert!(error.to_string().contains("Completed"), "{error}");

        let reloaded = load(&state_dir, repo.path(), &state.id).unwrap();
        assert_eq!(reloaded.status, WorkflowStatus::Completed);
    }

    #[test]
    fn close_refuses_an_already_failed_workflow() {
        let repo = tempdir().unwrap();
        let root = tempdir().unwrap();
        let state_dir = StateDir::from_root(root.path().to_path_buf());
        let mut state = WorkflowState::start(
            repo.path().to_path_buf(),
            "small feature".into(),
            WorkflowKind::Feature,
            None,
            true,
            low_classification(),
        );
        state.status = WorkflowStatus::Failed;
        save(&state_dir, &state, false).unwrap();

        let error = close(&state_dir, state.clone(), None).unwrap_err();
        assert!(error.to_string().contains("Failed"), "{error}");

        let reloaded = load(&state_dir, repo.path(), &state.id).unwrap();
        assert_eq!(reloaded.status, WorkflowStatus::Failed);
    }

    #[test]
    fn close_refuses_an_already_closed_workflow() {
        let repo = tempdir().unwrap();
        let root = tempdir().unwrap();
        let state_dir = StateDir::from_root(root.path().to_path_buf());
        let state = WorkflowState::start(
            repo.path().to_path_buf(),
            "small feature".into(),
            WorkflowKind::Feature,
            None,
            true,
            low_classification(),
        );
        save(&state_dir, &state, true).unwrap();
        let closed = close(&state_dir, state, None).unwrap();

        let error = close(&state_dir, closed.clone(), None).unwrap_err();
        assert!(error.to_string().contains("Closed"), "{error}");

        let reloaded = load(&state_dir, repo.path(), &closed.id).unwrap();
        assert_eq!(reloaded.status, WorkflowStatus::Closed);
    }

    #[test]
    fn resume_refuses_a_closed_workflow() {
        let repo = tempdir().unwrap();
        let root = tempdir().unwrap();
        let state_dir = StateDir::from_root(root.path().to_path_buf());
        let state = WorkflowState::start(
            repo.path().to_path_buf(),
            "small feature".into(),
            WorkflowKind::Feature,
            None,
            true,
            low_classification(),
        );
        save(&state_dir, &state, true).unwrap();
        let closed = close(&state_dir, state, None).unwrap();

        let _state_dir_env = crate::commands::ctx::testenv::VarGuard::set(&[(
            "ZIRV_CTX_STATE_DIR",
            Some(root.path().to_str().expect("utf-8 tempdir path")),
        )]);
        let args = WorkflowArgs {
            command: WorkflowSubcommand::Resume(StateIdArgs {
                id: closed.id.clone(),
                repo: Some(repo.path().to_path_buf()),
            }),
        };
        let mut out = Vec::new();
        let error = run(&args, &mut out).unwrap_err().to_string();
        assert!(
            error.contains("cannot resume") && error.contains("Closed"),
            "{error}"
        );
    }

    #[test]
    fn medium_risk_review_requires_independent_evidence() {
        let repo = tempdir().unwrap();
        let root = tempdir().unwrap();
        let state_dir = StateDir::from_root(root.path().to_path_buf());
        let mut classification = low_classification();
        classification.risk = RiskBand::Medium;
        let state = WorkflowState::start(
            repo.path().to_path_buf(),
            "review change".into(),
            WorkflowKind::Review,
            None,
            true,
            classification,
        );
        let error = advance_with_evidence(&state_dir, state, StepOutcome::Success, None, false)
            .unwrap_err();
        assert!(error.to_string().contains("independent review"));
    }

    /// A committed repository with a few pending (untracked) files, so
    /// `zirv workflow team plan`'s own undeclared classification has a real
    /// measured diff to size a Bounded team against.
    fn git_repo_with_pending_files(count: usize) -> tempfile::TempDir {
        let repo = tempdir().unwrap();
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
                .expect("run git");
            assert!(status.success(), "git {args:?} failed");
        };
        git(&["init", "-q"]);
        std::fs::write(repo.path().join("README.md"), "readme\n").unwrap();
        git(&["add", "."]);
        git(&["commit", "-q", "-m", "base"]);
        // All under one `src/` prefix (not scattered at the repository
        // root), so `classify`'s cross-module signal never fires here and
        // this stays a plain Bounded, Low-risk change regardless of
        // `count` -- the point of this fixture is a real measured diff
        // sized as Bounded, not an incidental risk escalation.
        std::fs::create_dir_all(repo.path().join("src")).unwrap();
        for index in 0..count {
            std::fs::write(
                repo.path().join(format!("src/pending-{index}.rs")),
                "fn work() {}\n".repeat(15),
            )
            .unwrap();
        }
        repo
    }

    /// Issue #541: `zirv workflow team plan --json`'s printed plan is
    /// exactly what got persisted onto the active workflow -- the CLI never
    /// prints a plan different from the one a later `team show`/`team
    /// brief` would read back.
    #[test]
    fn workflow_team_plan_json_matches_the_stored_plan() {
        let repo = git_repo_with_pending_files(0);
        let home = tempdir().unwrap();
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());
        let root = tempdir().unwrap();
        let state_dir = StateDir::from_root(root.path().to_path_buf());
        let state = WorkflowState::start(
            repo.path().to_path_buf(),
            "small feature".into(),
            WorkflowKind::Feature,
            None,
            true,
            low_classification(),
        );
        save(&state_dir, &state, true).unwrap();

        let _state_dir_env = crate::commands::ctx::testenv::VarGuard::set(&[(
            "ZIRV_CTX_STATE_DIR",
            Some(root.path().to_str().expect("utf-8 tempdir path")),
        )]);
        let args = WorkflowArgs {
            command: WorkflowSubcommand::Team(super::team::TeamArgs {
                command: super::team::TeamCommand::Plan(super::team::TeamPlanArgs {
                    objective: "add a small feature".into(),
                    workflow: None,
                    dry_run: false,
                    seat: None,
                    built_in_only: true,
                    repo: Some(repo.path().to_path_buf()),
                    json: true,
                }),
            }),
        };
        let mut out = Vec::new();
        let code = run(&args, &mut out).unwrap();
        assert_eq!(code, 0);
        let printed: super::team::TeamPlan = serde_json::from_slice(&out).unwrap();

        let stored = load_active(&state_dir, repo.path())
            .unwrap()
            .expect("workflow still active");
        assert_eq!(stored.team_plan, Some(printed));
    }

    /// Issue #541: `zirv workflow team brief <seat>` attaches only the
    /// skills that SEAT's own manifest references (the debugger's
    /// `systematic-debugging`), never the whole skill catalogue and never
    /// another seat's skills.
    #[test]
    fn workflow_team_brief_attaches_only_the_seats_skills() {
        let repo = git_repo_with_pending_files(3);
        let home = tempdir().unwrap();
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());
        let root = tempdir().unwrap();
        let state_dir = StateDir::from_root(root.path().to_path_buf());
        let state = WorkflowState::start(
            repo.path().to_path_buf(),
            "fix the crash".into(),
            WorkflowKind::Bugfix,
            None,
            true,
            low_classification(),
        );
        save(&state_dir, &state, true).unwrap();

        let _state_dir_env = crate::commands::ctx::testenv::VarGuard::set(&[(
            "ZIRV_CTX_STATE_DIR",
            Some(root.path().to_str().expect("utf-8 tempdir path")),
        )]);
        let plan_args = WorkflowArgs {
            command: WorkflowSubcommand::Team(super::team::TeamArgs {
                command: super::team::TeamCommand::Plan(super::team::TeamPlanArgs {
                    objective: "fix the crash".into(),
                    workflow: None,
                    dry_run: false,
                    seat: None,
                    built_in_only: true,
                    repo: Some(repo.path().to_path_buf()),
                    json: true,
                }),
            }),
        };
        let mut plan_out = Vec::new();
        run(&plan_args, &mut plan_out).unwrap();
        let plan: super::team::TeamPlan = serde_json::from_slice(&plan_out).unwrap();
        assert!(
            plan.seats.iter().any(|seat| seat.id == "debugger-1"),
            "{plan:?}"
        );
        assert!(
            plan.seats.iter().any(|seat| seat.id == "implementer-1"),
            "{plan:?}"
        );

        let brief = |seat_id: &str| -> serde_json::Value {
            let args = WorkflowArgs {
                command: WorkflowSubcommand::Team(super::team::TeamArgs {
                    command: super::team::TeamCommand::Brief(super::team::TeamBriefArgs {
                        seat_id: seat_id.to_string(),
                        workflow: None,
                        built_in_only: true,
                        repo: Some(repo.path().to_path_buf()),
                        json: true,
                    }),
                }),
            };
            let mut out = Vec::new();
            run(&args, &mut out).unwrap();
            serde_json::from_slice(&out).unwrap()
        };

        let debugger_brief = brief("debugger-1");
        let skills = debugger_brief["skills"].as_array().expect("skills array");
        assert_eq!(skills.len(), 1, "{debugger_brief}");
        assert_eq!(skills[0]["id"], "systematic-debugging");

        let implementer_brief = brief("implementer-1");
        assert_eq!(
            implementer_brief["skills"].as_array().unwrap().len(),
            0,
            "{implementer_brief}"
        );
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

    /// Issue #542 chunk 3a decision 1: every referenced agent role must
    /// resolve before any state is written, independent of whether an
    /// execution adapter was even given at `workflow start`.
    #[test]
    fn an_unknown_agent_role_fails_at_materialise_before_state_is_written() {
        let repo = tempdir().unwrap();
        let root = tempdir().unwrap();
        let _env = crate::commands::ctx::testenv::VarGuard::set(&[
            (
                "ZIRV_CTX_STATE_DIR",
                Some(root.path().to_str().expect("utf-8 tempdir path")),
            ),
            ("ZIRV_CTX_WORKFLOW_REPO_WORKFLOWS", Some("true")),
        ]);
        let dir = repo.path().join(".zirv/workflows");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("broken-agent.toml"),
            r#"
schema_version = 1
id = "broken-agent"
version = 1
title = "Broken agent"
description = "References an agent role nothing provides."
domains = ["testing"]
effects = "repository"

[[steps]]
id = "work"
title = "Work"
phase = "implement"
skills = ["implement"]
agent_role = "no-such-role"
condition = "always"

[failure]
escalate_to = "human"

[completion]
present_as = "summary"
"#,
        )
        .unwrap();

        let args = WorkflowArgs {
            command: WorkflowSubcommand::Start(StartArgs {
                id: Some("broken-agent".into()),
                task: "do work".into(),
                agent: None,
                built_in_only: false,
                repo: Some(repo.path().to_path_buf()),
                paths: vec![],
                // Declared, so classification never needs a real git
                // repository -- this test is about agent-role validation.
                changed_lines: Some(10),
                tests_changed: false,
                complexity: None,
                risk: None,
                branch: None,
                frontend_root: None,
                brainstorm: false,
                no_brainstorm: false,
                profile: None,
                json: false,
            }),
        };
        let mut out = Vec::new();
        let error = run(&args, &mut out).unwrap_err().to_string();
        assert!(error.contains("unknown agent role"), "{error}");
        assert!(error.contains("no-such-role"), "{error}");

        let state_dir = resolve_state().unwrap();
        assert!(
            load_active(&state_dir, repo.path()).unwrap().is_none(),
            "no state may be written when agent-role validation fails"
        );
    }

    /// Issue #542 chunk 3a decision 4: `workflow start <id>` for a
    /// non-legacy v2 pack (not one of the five built-in kinds) executes
    /// through the SAME path as a kind-based start, carrying
    /// `parallel_group` metadata through and advancing normally.
    #[test]
    fn a_v2_pack_with_parallel_steps_starts_and_advances_through_the_engine() {
        let repo = tempdir().unwrap();
        let root = tempdir().unwrap();
        let _env = crate::commands::ctx::testenv::VarGuard::set(&[
            (
                "ZIRV_CTX_STATE_DIR",
                Some(root.path().to_str().expect("utf-8 tempdir path")),
            ),
            ("ZIRV_CTX_WORKFLOW_REPO_WORKFLOWS", Some("true")),
        ]);
        let dir = repo.path().join(".zirv/workflows");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("parallel-fixture.toml"),
            r#"
schema_version = 1
id = "parallel-fixture"
version = 1
title = "Parallel fixture"
description = "Two independent steps sharing a parallel_group tag."
domains = ["testing"]
effects = "repository"

[[steps]]
id = "task-a"
title = "Task A"
phase = "implement"
skills = ["implement"]
parallel_group = "fanout"
condition = "always"

[[steps]]
id = "task-b"
title = "Task B"
phase = "implement"
skills = ["testing"]
parallel_group = "fanout"
condition = "always"

[[steps]]
id = "wrap-up"
title = "Wrap up"
phase = "present"
skills = ["verify"]
depends_on = ["task-a", "task-b"]
condition = "always"

[failure]
escalate_to = "human"

[completion]
present_as = "summary"
"#,
        )
        .unwrap();

        let args = WorkflowArgs {
            command: WorkflowSubcommand::Start(StartArgs {
                id: Some("parallel-fixture".into()),
                task: "do parallel work".into(),
                agent: None,
                built_in_only: false,
                repo: Some(repo.path().to_path_buf()),
                paths: vec![],
                // Declared, so classification never needs a real git
                // repository -- this test is about the v2-pack start path,
                // not about git-measured risk.
                changed_lines: Some(10),
                tests_changed: false,
                complexity: None,
                risk: None,
                branch: None,
                frontend_root: None,
                brainstorm: false,
                no_brainstorm: false,
                profile: None,
                json: false,
            }),
        };
        let mut out = Vec::new();
        run(&args, &mut out).expect("a v2-only pack must start through the same CLI path");

        let state_dir = resolve_state().unwrap();
        let state = load_active(&state_dir, repo.path())
            .unwrap()
            .expect("active workflow");
        assert_eq!(
            state
                .steps
                .iter()
                .map(|step| step.id.as_str())
                .collect::<Vec<_>>(),
            ["task-a", "task-b", "wrap-up"]
        );
        assert_eq!(state.steps[0].parallel_group.as_deref(), Some("fanout"));
        assert_eq!(state.steps[1].parallel_group.as_deref(), Some("fanout"));
        assert_eq!(state.steps[2].parallel_group, None);
        assert_eq!(state.current().unwrap().id, "task-a");
        assert!(
            state.definition.is_some(),
            "a v2-only start still pins a definition"
        );

        let advanced = advance_with_evidence(&state_dir, state, StepOutcome::Success, None, false)
            .expect("advancing the first of two parallel steps must succeed");
        assert_eq!(advanced.current().unwrap().id, "task-b");
        let after_b =
            advance_with_evidence(&state_dir, advanced, StepOutcome::Success, None, false)
                .expect("advancing the second parallel step must succeed");
        assert_eq!(after_b.current().unwrap().id, "wrap-up");
        let completed =
            advance_with_evidence(&state_dir, after_b, StepOutcome::Success, None, false)
                .expect("advancing the step downstream of both parallel steps must succeed");
        assert_eq!(completed.status, WorkflowStatus::Completed);
    }

    /// Issue #542 chunk 3a decision 5: `status`'s drift note reads the
    /// pinned `DefinitionRef.hash` off disk, and the pin survives a save/
    /// reload cycle unchanged even when it no longer matches what the
    /// registry would currently resolve for the same id.
    #[test]
    fn a_pinned_definition_survives_registry_drift() {
        let repo = tempdir().unwrap();
        let root = tempdir().unwrap();
        let state_dir = StateDir::from_root(root.path().to_path_buf());
        let mut state = WorkflowState::start(
            repo.path().to_path_buf(),
            "small feature".into(),
            WorkflowKind::Feature,
            None,
            true,
            low_classification(),
        );
        assert!(state.definition.is_some(), "start must pin a definition");

        // Simulate drift: the registry's current copy of "feature" no
        // longer hashes the same as what this run pinned (an operator
        // edit, or a future release shipping a different built-in).
        state.definition.as_mut().unwrap().hash = "0".repeat(64);
        save(&state_dir, &state, true).unwrap();

        let reloaded = load(&state_dir, repo.path(), &state.id).unwrap();
        assert_eq!(
            reloaded.definition.as_ref().unwrap().hash,
            "0".repeat(64),
            "the pinned hash survives resume even though it no longer matches the registry"
        );

        let mut out = Vec::new();
        write_definition_status(&mut out, &reloaded).unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("definition drifted from registry"), "{text}");
    }

    /// Issue #542 review finding 4: `a_pinned_definition_survives_registry_
    /// drift` (above) only proves the PIN itself survives save/reload for a
    /// BUILT-IN pack, with drift simulated by hand-mutating the hash field
    /// -- it never actually starts from a genuinely on-disk, editable
    /// repository-layer pack, nor proves a run RESUMES with its original
    /// inline steps after the on-disk file is edited out from under it, as
    /// opposed to merely carrying a stale hash string. This starts from
    /// `tests/fixtures/workflow/packs/drift-fixture.toml` (a real repository
    /// pack file, loaded exactly the way `registry.rs`'s own repository-
    /// layer tests do -- `WorkflowRegistry::load(..., true, ...)` directly,
    /// bypassing the operator's `repo_workflows_enabled` gate, which is a
    /// CLI-level concern proven independently and orthogonal to what this
    /// test is about), edits the on-disk copy after the run has started, and
    /// proves two things: (1) resuming and advancing still walks the
    /// ORIGINAL inline steps, never the edited ones, and (2) a fresh
    /// registry load of the edited file now hashes differently than the
    /// pin -- the exact comparison `write_definition_status` itself makes
    /// (`current_hash != reference.hash`) to print "definition drifted from
    /// registry".
    #[test]
    fn a_pinned_repository_definition_survives_registry_drift_inline() {
        const DRIFT_FIXTURE: &str =
            include_str!("../../../tests/fixtures/workflow/packs/drift-fixture.toml");

        let repo = tempdir().unwrap();
        let root = tempdir().unwrap();
        let state_dir = StateDir::from_root(root.path().to_path_buf());
        let workflows_dir = repo.path().join(".zirv/workflows");
        std::fs::create_dir_all(&workflows_dir).unwrap();
        let fixture_path = workflows_dir.join("drift-fixture.toml");
        std::fs::write(&fixture_path, DRIFT_FIXTURE).unwrap();

        let skills = SkillRegistry::load(repo.path(), None, false, false).unwrap();
        let registry = crate::commands::workflow::registry::WorkflowRegistry::load(
            repo.path(),
            None,
            true,
            true,
            &skills,
        )
        .expect("registry with the repository layer enabled");
        assert!(registry.warnings().is_empty(), "{:?}", registry.warnings());
        let pack = registry.get("drift-fixture").expect("drift-fixture pack");
        let pinned_hash = pack.hash.clone();
        assert_eq!(
            pack.source,
            super::super::registry::WorkflowSource::Repository
        );

        let mut state = WorkflowState::start_from_pack(
            repo.path().to_path_buf(),
            "run the drift fixture".into(),
            pack,
            None,
            true,
            low_classification(),
        );
        assert_eq!(state.current().unwrap().id, "first");
        assert!(
            state
                .definition
                .as_ref()
                .unwrap()
                .inline
                .as_ref()
                .is_some_and(|inline| inline.steps.iter().any(|step| step.id == "second")),
            "a repository-layer pack must pin its own inline copy"
        );
        save(&state_dir, &state, true).unwrap();

        // The on-disk copy drifts AFTER the run has already pinned its own
        // inline definition: "second"'s skill and title change, and a brand
        // new third step is added.
        let edited = DRIFT_FIXTURE
            .replace(
                "id = \"second\"\ntitle = \"Second\"\nphase = \"implement\"\nskills = [\"implement\"]",
                "id = \"second\"\ntitle = \"Second (edited)\"\nphase = \"implement\"\nskills = [\"testing\"]",
            )
            .replace(
                "[failure]",
                "[[steps]]\nid = \"third\"\ntitle = \"Third\"\nphase = \"verify\"\nskills = [\"verify\"]\ndepends_on = [\"second\"]\ncondition = \"always\"\n\n[failure]",
            );
        assert_ne!(
            edited, DRIFT_FIXTURE,
            "the fixture text must actually change"
        );
        std::fs::write(&fixture_path, &edited).unwrap();

        // Resume: reload from disk and advance past "first" -- the run must
        // still see the ORIGINAL "second" (skill "implement", no "third"
        // step at all), never the edited on-disk copy.
        state = load(&state_dir, repo.path(), &state.id).unwrap();
        state = advance_with_evidence(&state_dir, state, StepOutcome::Success, None, false)
            .expect("advance past 'first'");
        let second = state.current().expect("still-pinned 'second' step");
        assert_eq!(second.id, "second");
        assert_eq!(
            second.skill, "implement",
            "the pinned inline copy's original skill must survive the on-disk edit"
        );
        let completed = advance_with_evidence(&state_dir, state, StepOutcome::Success, None, false)
            .expect("advance past the original 'second'");
        assert_eq!(
            completed.status,
            WorkflowStatus::Completed,
            "the original two-step definition must still be the whole run -- the edited \
             on-disk 'third' step must never appear"
        );

        // Separately: a FRESH registry load of the now-edited file hashes
        // differently than what this run pinned -- exactly the comparison
        // `write_definition_status` performs to report "definition drifted
        // from registry" (proven directly against a built-in pack by
        // `a_pinned_definition_survives_registry_drift`, above; the CLI-
        // level `repo_workflows_enabled` gate that call site's own
        // `load_workflow_registry` also applies is a separate, already-
        // covered concern this test does not re-prove).
        let drifted_registry = crate::commands::workflow::registry::WorkflowRegistry::load(
            repo.path(),
            None,
            true,
            true,
            &skills,
        )
        .expect("registry re-load after the on-disk edit");
        let current_hash = drifted_registry.get("drift-fixture").unwrap().hash.clone();
        assert_ne!(
            current_hash, pinned_hash,
            "the edited on-disk copy must hash differently than the pin"
        );
    }

    /// Issue #542 chunk 3a: `native_completion_gate` (the native loop's own
    /// completion check) and `advance_with_evidence`'s Test-phase gate must
    /// keep agreeing after the v2 materialize rewrite -- both blocked with
    /// no evidence, both open once fresh passing evidence exists.
    #[test]
    fn native_completion_gate_and_advance_with_evidence_still_agree() {
        let repo = tempdir().unwrap();
        let root = tempdir().unwrap();
        let state_dir = StateDir::from_root(root.path().to_path_buf());
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
                .expect("run git");
            assert!(status.success(), "git {args:?} failed");
        };
        git(&["init", "-q"]);
        std::fs::write(repo.path().join("README.md"), "hello\n").unwrap();
        git(&["add", "."]);
        git(&["commit", "-q", "-m", "base"]);

        let mut state = WorkflowState::start(
            repo.path().to_path_buf(),
            "small feature".into(),
            WorkflowKind::Feature,
            None,
            true,
            low_classification(),
        );
        let test_index = state
            .steps
            .iter()
            .position(|step| step.phase == WorkflowPhase::Test)
            .expect("fixture has a Test step");
        state.completed_steps = state.steps[..test_index]
            .iter()
            .map(|step| step.id.clone())
            .collect();
        state.current_step = test_index;
        state.status = WorkflowStatus::Running;
        save(&state_dir, &state, true).unwrap();

        assert!(
            native_completion_gate(&state_dir, repo.path()).is_some(),
            "the gate must block with no evidence"
        );
        assert!(
            advance_with_evidence(&state_dir, state.clone(), StepOutcome::Success, None, false)
                .is_err(),
            "advance must also refuse with no evidence"
        );

        let fingerprint = super::super::verification::change_fingerprint(repo.path()).unwrap();
        let evidence_report = super::super::verification::VerificationReport {
            schema_version: super::super::verification::VERIFY_REPORT_SCHEMA_VERSION,
            id: "seeded".into(),
            mode: super::super::verification::VerificationMode::Changed,
            source: "configured".into(),
            repo: repo.path().to_path_buf(),
            branch: String::new(),
            head_sha: String::new(),
            change_fingerprint: fingerprint,
            changed_paths: vec![],
            fallback_to_full: false,
            narrowed_to: vec![],
            notes: vec![],
            started_at: 0,
            finished_at: 0,
            checks: vec![super::super::verification::CheckResult {
                id: "unit".into(),
                kind: super::super::verification::CheckKind::Unit,
                command: "true".into(),
                source: super::super::verification::CheckSource::DiscoveredToolchain,
                status: super::super::verification::CheckStatus::Passed,
                exit_code: Some(0),
                duration_ms: 1,
                failure_output: None,
                failure_test_names: Vec::new(),
                inconclusive_reason: None,
            }],
        };
        super::super::verification::save_report(&state_dir, &evidence_report).unwrap();

        assert!(
            native_completion_gate(&state_dir, repo.path()).is_none(),
            "the gate must open once fresh passing evidence exists"
        );
        let advanced = advance_with_evidence(&state_dir, state, StepOutcome::Success, None, false)
            .expect("advance must also succeed once fresh passing evidence exists");
        assert_eq!(advanced.current().unwrap().phase, WorkflowPhase::Verify);
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

    /// Issue #542 chunk 3b decision 3: an explicit id at `workflow start`
    /// always wins outright (no selection line printed, no `selection` in
    /// state), and that choice is durable across resume because it is
    /// simply what got pinned on `state.definition` -- selection never runs
    /// again on reload.
    #[test]
    fn an_explicit_id_overrides_selection_and_survives_resume() {
        let repo = tempdir().unwrap();
        let root = tempdir().unwrap();
        let _env = crate::commands::ctx::testenv::VarGuard::set(&[(
            "ZIRV_CTX_STATE_DIR",
            Some(root.path().to_str().expect("utf-8 tempdir path")),
        )]);
        let args = WorkflowArgs {
            command: WorkflowSubcommand::Start(StartArgs {
                id: Some("review".into()),
                task: "totally unrelated free-text objective".into(),
                agent: None,
                built_in_only: true,
                repo: Some(repo.path().to_path_buf()),
                paths: vec![],
                changed_lines: Some(5),
                tests_changed: false,
                complexity: None,
                risk: None,
                branch: None,
                frontend_root: None,
                brainstorm: false,
                no_brainstorm: false,
                profile: None,
                json: false,
            }),
        };
        let mut out = Vec::new();
        run(&args, &mut out).unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(
            !text.contains("selected:"),
            "an explicit id must never print a selection line: {text}"
        );

        let state_dir = resolve_state().unwrap();
        let state = load_active(&state_dir, repo.path()).unwrap().unwrap();
        assert_eq!(state.definition.as_ref().unwrap().id, "review");

        let resumed = load(&state_dir, repo.path(), &state.id).unwrap();
        assert_eq!(
            resumed.definition.as_ref().unwrap().id,
            "review",
            "the explicit override survives resume"
        );
    }

    /// Issue #542 review nit: `state.selection` persists the deterministic
    /// [`super::selection::Selection`] that chose a run's pack, so `zirv
    /// workflow status` can explain why a pack was chosen after the fact --
    /// not just at the moment `workflow start` printed it. Proven directly
    /// against a hand-set `Selection` (rather than driving it through
    /// `classify`'s own intent heuristic, which this test is not about) to
    /// isolate exactly the two things that matter: the field survives a
    /// save/reload cycle, and `write_state` renders it.
    #[test]
    fn a_persisted_selection_survives_resume_and_explains_status() {
        let repo = tempdir().unwrap();
        let root = tempdir().unwrap();
        let state_dir = StateDir::from_root(root.path().to_path_buf());
        let mut state = WorkflowState::start(
            repo.path().to_path_buf(),
            "small feature".into(),
            WorkflowKind::Feature,
            None,
            true,
            low_classification(),
        );
        assert!(
            state.selection.is_none(),
            "WorkflowState::start (the legacy, explicit-kind path) never runs selection"
        );
        state.selection = Some(super::super::selection::Selection {
            definition_id: "adaptive-work".into(),
            confidence: 0.42,
            reasons: vec!["a made-up reason for this test".into()],
            alternatives: vec![],
        });
        save(&state_dir, &state, true).unwrap();

        let reloaded = load(&state_dir, repo.path(), &state.id).unwrap();
        assert_eq!(
            reloaded
                .selection
                .as_ref()
                .map(|selection| selection.definition_id.as_str()),
            Some("adaptive-work"),
            "a persisted selection must survive resume"
        );

        let mut out = Vec::new();
        write_state(&mut out, &reloaded, false).unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(
            text.contains("selected: adaptive-work (a made-up reason for this test)"),
            "{text}"
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

    fn git_init_with_commit(repo: &Path) {
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
                .current_dir(repo)
                .status()
                .expect("run git");
            assert!(status.success(), "git {args:?} failed");
        };
        git(&["init", "-q"]);
        std::fs::write(repo.join("README.md"), "hello\n").expect("write");
        git(&["add", "."]);
        git(&["commit", "-q", "-m", "base"]);
    }

    fn seed_passing_verification(state_dir: &StateDir, state: &WorkflowState, final_only: bool) {
        let fingerprint =
            super::super::verification::change_fingerprint(&state.repo).expect("fingerprint");
        let report = super::super::verification::VerificationReport {
            schema_version: super::super::verification::VERIFY_REPORT_SCHEMA_VERSION,
            id: "seeded".into(),
            mode: if final_only {
                super::super::verification::VerificationMode::Final
            } else {
                super::super::verification::VerificationMode::Changed
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
            checks: vec![super::super::verification::CheckResult {
                id: "unit".into(),
                kind: super::super::verification::CheckKind::Unit,
                command: "true".into(),
                source: super::super::verification::CheckSource::DiscoveredToolchain,
                status: super::super::verification::CheckStatus::Passed,
                exit_code: Some(0),
                duration_ms: 1,
                failure_output: None,
                failure_test_names: Vec::new(),
                inconclusive_reason: None,
            }],
        };
        super::super::verification::save_report(state_dir, &report).expect("save report");
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
                            super::super::review::required_independent_reviews_for(&state);
                        if required > 0 {
                            let fingerprint =
                                super::super::verification::change_fingerprint(&state.repo)
                                    .expect("fingerprint");
                            for round in 0..required {
                                state.review_evidence.push(
                                    super::super::review::ReviewRunEvidence {
                                        id: format!("fixture-review-{round}"),
                                        change_fingerprint: fingerprint,
                                        adapter: "claude".into(),
                                        review_round: 1,
                                        completed_at: 0,
                                        head_sha: None,
                                        reviewed_tree_sha: None,
                                        finding_dispositions: std::collections::BTreeMap::new(),
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

        let events = super::super::telemetry::list(&state_dir, &state.repo).unwrap_or_default();
        assert!(
            events.iter().any(|event| {
                event.kind == super::super::telemetry::TelemetryKind::ArtifactAccepted
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
        use super::super::definition::{
            CompletionContract, EffectClass, EscalateTo, FailurePolicy, GateSpec, Limits,
            PresentAs, StepV2, WorkflowDefinitionV2,
        };
        let repo = tempdir().unwrap();
        let root = tempdir().unwrap();
        let state_dir = StateDir::from_root(root.path().to_path_buf());

        let definition = WorkflowDefinitionV2 {
            schema_version: super::super::definition::DEFINITION_SCHEMA_VERSION,
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

        let pack = super::super::registry::RegisteredWorkflow {
            definition: definition.clone(),
            source: super::super::registry::WorkflowSource::Repository,
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
                super::super::definition::EffectClass::None,
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
            vec!["scope", "implement", "test", "review", "verify", "deploy"]
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
            vec!["intent", "implement", "test", "review", "verify", "deploy"]
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
            let definition = super::super::registry::builtin_definition(pack_id)
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
                    super::super::definition::EffectClass::None,
                    "{pack_id}: step '{}' must stay read-only today",
                    step.id
                );
            }
            let definition = super::super::registry::builtin_definition(pack_id)
                .unwrap_or_else(|| panic!("{pack_id} pack"));
            assert_eq!(
                definition.effects,
                super::super::definition::EffectClass::External
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
