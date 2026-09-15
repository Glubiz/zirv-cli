//! Versioned, provider-neutral `WorkflowDefinition v2`: a declarative,
//! validated DAG of steps that composes skills (`skill.rs`), agent roles
//! (`agents.rs`) and typed capabilities (`capability.rs`) instead of adding a
//! new Rust match arm per workflow (issue #542, chunks 1+2).
//!
//! This schema is independent of [`super::engine::WORKFLOW_SCHEMA_VERSION`]
//! (the *state* schema): a running workflow pins a definition's id/version/
//! hash so update, resume and rollover cannot change its meaning silently
//! (see `engine::DefinitionRef`).
//!
//! Validation here is purely structural (id shape, uniqueness, DAG,
//! reachability, gate references, size). It deliberately does NOT resolve
//! skill ids against a live [`super::skill::SkillRegistry`] or agent roles
//! against a live [`super::agents::AgentRegistry`] -- those live registries
//! are a loading-time concern owned by `registry.rs` (skills) and a
//! materialize-time concern owned by `engine.rs` (agent roles), per the
//! issue's own decision to keep the "unknown seat" refusal separate from the
//! definition's own self-contained shape check. [`WorkflowDefinitionV2::
//! validate`] takes the caller's known skill ids so this module stays
//! testable without constructing a real registry.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::capability::CapabilityId;
use super::engine::{ArtifactStage, StepCondition};
use super::skill::WorkflowPhase;
use crate::commands::ctx::CtxResult;

/// The definition FORMAT schema version -- independent of the workflow
/// STATE schema (`engine::WORKFLOW_SCHEMA_VERSION`).
pub const DEFINITION_SCHEMA_VERSION: u32 = 1;
const MAX_DEFINITION_BYTES: usize = 32 * 1024;
const DEFAULT_MAX_ATTEMPTS: u8 = 3;

fn valid_id(id: &str) -> bool {
    let mut chars = id.chars();
    chars
        .next()
        .is_some_and(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
        && chars
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '.' | '_' | '-'))
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

fn default_condition() -> StepCondition {
    StepCondition::Always
}

fn default_max_attempts() -> u8 {
    DEFAULT_MAX_ATTEMPTS
}

fn default_effect() -> EffectClass {
    EffectClass::None
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum InputKind {
    Text,
    Path,
    Url,
    Json,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TypedInput {
    pub name: String,
    pub kind: InputKind,
    #[serde(default)]
    pub required: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum OutputKind {
    Artifact,
    Report,
    Patch,
    Decision,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TypedOutput {
    pub name: String,
    pub kind: OutputKind,
    #[serde(default)]
    pub schema: Option<String>,
}

/// Approval/validation/independent-review gates, each a set of step ids that
/// carry that gate. Redundant with (but explicit about) a step's own
/// `approval: bool` -- `gates` is the definition-level summary a status
/// reader or `an_unmatched_task`-style selector can inspect without walking
/// every step, while a step's own fields remain the execution-time source of
/// truth. `validate` requires every id here to resolve to a real step.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct GateSpec {
    pub approval: Vec<String>,
    pub validation: Vec<String>,
    pub independent_review: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Limits {
    pub max_attempts: u8,
    pub timeout_minutes: Option<u32>,
    pub spend_class: Option<String>,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_attempts: DEFAULT_MAX_ATTEMPTS,
            timeout_minutes: None,
            spend_class: None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum EscalateTo {
    Human,
    Coordinator,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FailurePolicy {
    pub escalate_to: EscalateTo,
    #[serde(default)]
    pub retry: bool,
}

/// External-effect classification (issue #542 architecture §1): whether
/// completing this definition (or one of its steps) can leave a durable
/// mark outside the repository/workflow state itself. `None` -- no effect at
/// all, e.g. a read-only investigation or report -- `Repository`, or
/// `External` (Linear/Kibana/cloud/a repository host action). Steps default
/// to `None`; a definition's own top-level `effects` is the ceiling none of
/// its steps may exceed unless the definition itself says otherwise (not
/// enforced here -- see `registry.rs`'s widening refusal for the repository
/// trust layer).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum EffectClass {
    #[default]
    None,
    Repository,
    External,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum PresentAs {
    Summary,
    Report,
    Patch,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CompletionContract {
    #[serde(default)]
    pub required_outputs: Vec<String>,
    pub present_as: PresentAs,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StepV2 {
    pub id: String,
    pub title: String,
    pub phase: WorkflowPhase,
    pub skills: Vec<String>,
    /// A provider-neutral organizational role (`agents::AgentManifest::role`),
    /// resolved against the live [`super::agents::AgentRegistry`] at
    /// MATERIALISE time, not here -- kept separate from the dispatch-time
    /// writable-seat refusal per the issue's own decision.
    #[serde(default)]
    pub agent_role: Option<String>,
    #[serde(default)]
    pub capabilities: Vec<CapabilityId>,
    #[serde(default)]
    pub depends_on: Vec<String>,
    #[serde(default)]
    pub parallel_group: Option<String>,
    #[serde(default = "default_condition")]
    pub condition: StepCondition,
    #[serde(default)]
    pub approval: bool,
    #[serde(default)]
    pub artifact: Option<ArtifactStage>,
    #[serde(default = "default_max_attempts")]
    pub max_attempts: u8,
    #[serde(default = "default_effect")]
    pub effect: EffectClass,
    #[serde(default)]
    pub reason: Option<String>,
    /// Free-form domain tags this step's data applies to (matched against
    /// the classified [`super::classify::WorkDomain`]/[`super::engine::
    /// WorkflowProfile`], e.g. `"frontend"`) -- issue #542 chunk 3a. Empty
    /// (the default) means "the general/default data for this step's slot".
    /// Only meaningful together with `overrides_step`: a step with a
    /// non-empty `domains` and no `overrides_step` is still a normal
    /// standalone DAG node, just one `materialize` never prunes for domain
    /// reasons (domain-based selection only ever happens between an
    /// `overrides_step` variant and the step it names).
    #[serde(default)]
    pub domains: Vec<String>,
    /// When set, this step is not its own DAG node: it is a domain-scoped
    /// DATA VARIANT of the step named here (which must exist in the same
    /// definition and must not itself be a variant). At materialize time,
    /// exactly one variant (or the overridden step's own data, if none
    /// matches) supplies `skills`/`agent_role`/`capabilities`/`effect`/
    /// `approval`/`artifact`/`max_attempts` for the overridden step's id --
    /// `id`/`phase`/`depends_on`/`parallel_group`/`condition` always come
    /// from the overridden (canonical) step, so the canonical id is stable
    /// across a profile change (`zirv workflow reclassify`) the same way it
    /// was before this schema existed. Replaces the old hardcoded
    /// `WorkflowProfile`-keyed Rust match table: a new pack adds a
    /// `domains = ["frontend"]`, `overrides_step = "<id>"` entry instead of
    /// a new engine.rs match arm.
    #[serde(default)]
    pub overrides_step: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkflowDefinitionV2 {
    pub schema_version: u32,
    pub id: String,
    pub version: u32,
    pub title: String,
    pub description: String,
    #[serde(default)]
    pub domains: Vec<String>,
    #[serde(default)]
    pub triggers: Vec<String>,
    #[serde(default)]
    pub inputs: Vec<TypedInput>,
    #[serde(default)]
    pub outputs: Vec<TypedOutput>,
    pub steps: Vec<StepV2>,
    #[serde(default)]
    pub gates: GateSpec,
    #[serde(default)]
    pub limits: Limits,
    pub failure: FailurePolicy,
    pub effects: EffectClass,
    #[serde(default)]
    pub idempotency: Option<String>,
    pub completion: CompletionContract,
    #[serde(default)]
    pub presentation: Option<String>,
    /// Operator-global layer only: explicit consent to replace a built-in id
    /// with this document instead of silently adding a new one. Ignored (and
    /// never sufficient on its own) for a repository-layer document -- see
    /// `registry.rs`'s loader, which is the only reader of this field.
    #[serde(default, rename = "override")]
    pub override_builtin: bool,
}

impl WorkflowDefinitionV2 {
    /// Structural validation only -- see this module's own doc comment for
    /// why skill ids are checked against a caller-supplied set rather than a
    /// live registry, and why agent roles are not checked here at all.
    pub fn validate(&self, known_skill_ids: &BTreeSet<&str>) -> CtxResult<()> {
        if self.schema_version != DEFINITION_SCHEMA_VERSION {
            return Err(format!(
                "workflow definition '{}': unsupported schema_version {}; supported version is {}",
                self.id, self.schema_version, DEFINITION_SCHEMA_VERSION
            )
            .into());
        }
        if !valid_id(&self.id) {
            return Err(format!(
                "workflow definition id '{}' must match [a-z0-9][a-z0-9._-]*",
                self.id
            )
            .into());
        }
        if self.version == 0 {
            return Err(format!(
                "workflow definition '{}': version must be at least 1",
                self.id
            )
            .into());
        }
        if self.title.trim().is_empty() || self.description.trim().is_empty() {
            return Err(format!(
                "workflow definition '{}': title and description are required",
                self.id
            )
            .into());
        }
        if self.steps.is_empty() {
            return Err(format!(
                "workflow definition '{}': at least one step is required",
                self.id
            )
            .into());
        }
        let size = serde_json::to_vec(self)?.len();
        if size > MAX_DEFINITION_BYTES {
            return Err(format!(
                "workflow definition '{}' is {size} bytes; limit is {MAX_DEFINITION_BYTES}",
                self.id
            )
            .into());
        }

        let mut seen_ids = BTreeSet::new();
        for step in &self.steps {
            if !valid_id(&step.id) {
                return Err(format!(
                    "workflow definition '{}': step id '{}' must match [a-z0-9][a-z0-9._-]*",
                    self.id, step.id
                )
                .into());
            }
            if !seen_ids.insert(step.id.as_str()) {
                return Err(format!(
                    "workflow definition '{}': duplicate step id '{}'",
                    self.id, step.id
                )
                .into());
            }
            if step.skills.is_empty() {
                return Err(format!(
                    "workflow definition '{}': step '{}' must reference at least one skill",
                    self.id, step.id
                )
                .into());
            }
            for skill in &step.skills {
                if !known_skill_ids.contains(skill.as_str()) {
                    return Err(format!(
                        "workflow definition '{}': step '{}' references unknown skill '{}'",
                        self.id, step.id, skill
                    )
                    .into());
                }
            }
        }

        for step in &self.steps {
            for dependency in &step.depends_on {
                if dependency == &step.id {
                    return Err(format!(
                        "workflow definition '{}': step '{}' depends on itself",
                        self.id, step.id
                    )
                    .into());
                }
                if !seen_ids.contains(dependency.as_str()) {
                    return Err(format!(
                        "workflow definition '{}': step '{}' depends on unknown step '{}'",
                        self.id, step.id, dependency
                    )
                    .into());
                }
            }
        }

        // Issue #542 chunk 3a: a step with `overrides_step` is a domain-
        // scoped data variant, not its own DAG node -- it must name a real,
        // non-variant sibling and carry no dependency edges of its own
        // (those live on the canonical step it overrides).
        let variant_targets: std::collections::BTreeMap<&str, &str> = self
            .steps
            .iter()
            .filter_map(|step| {
                step.overrides_step
                    .as_deref()
                    .map(|target| (step.id.as_str(), target))
            })
            .collect();
        for (variant_id, target) in &variant_targets {
            if target == variant_id {
                return Err(format!(
                    "workflow definition '{}': step '{variant_id}' overrides itself",
                    self.id
                )
                .into());
            }
            if !seen_ids.contains(target) {
                return Err(format!(
                    "workflow definition '{}': step '{variant_id}' overrides unknown step '{target}'",
                    self.id
                )
                .into());
            }
            if variant_targets.contains_key(target) {
                return Err(format!(
                    "workflow definition '{}': step '{variant_id}' overrides '{target}', which is \
                     itself a variant -- overrides_step must name a canonical (non-variant) step",
                    self.id
                )
                .into());
            }
        }
        for step in &self.steps {
            if step.overrides_step.is_some() && !step.depends_on.is_empty() {
                return Err(format!(
                    "workflow definition '{}': variant step '{}' must not declare its own \
                     depends_on -- dependencies live on the step it overrides",
                    self.id, step.id
                )
                .into());
            }
        }

        self.reject_cycles()?;
        self.reject_unreachable_steps()?;

        for gate_step in self
            .gates
            .approval
            .iter()
            .chain(&self.gates.validation)
            .chain(&self.gates.independent_review)
        {
            if !seen_ids.contains(gate_step.as_str()) {
                return Err(format!(
                    "workflow definition '{}': gate references unknown step '{}'",
                    self.id, gate_step
                )
                .into());
            }
        }

        // Issue #542 review finding 2: before this fix, a step's own
        // `effect` field was purely descriptive -- nothing anywhere actually
        // gated on it, so an `External`-effect step (a genuine durable mark
        // outside the repository/workflow state itself: a ticket, a cloud
        // action, a deployment) could be authored to run completely
        // unattended simply by leaving its own `approval` unset. A step
        // whose OWN effect is `External` must now be recorded as an
        // approval gate -- either the step's own `approval = true`, or its
        // id listed in `gates.approval`. `Repository`-effect steps are
        // deliberately NOT included here: every built-in pack already gates
        // its actual repository mutation points (intent/plan/deploy) via
        // the existing convention while leaving implement/test/verify
        // ungated by design, so widening this to `Repository` would gate
        // steps the issue never asked to gate and break the entire built-in
        // catalogue. (The engine's own `WorkflowState::step_requires_
        // approval` separately refuses to ENTER an unapproved `External`
        // step even if this authoring-time check were somehow bypassed --
        // e.g. an operator-global override, which also runs through this
        // same `validate()`.)
        for step in &self.steps {
            if step.effect == EffectClass::External
                && !step.approval
                && !self.gates.approval.iter().any(|id| id == &step.id)
            {
                return Err(format!(
                    "workflow definition '{}': step '{}' has effect 'external' but is not gated \
                     by approval (set `approval = true` on the step or list it in \
                     `gates.approval`)",
                    self.id, step.id
                )
                .into());
            }
        }

        Ok(())
    }

    fn reject_cycles(&self) -> CtxResult<()> {
        #[derive(Clone, Copy, PartialEq, Eq)]
        enum Mark {
            Visiting,
            Done,
        }
        let steps: BTreeMap<&str, &StepV2> = self
            .steps
            .iter()
            .map(|step| (step.id.as_str(), step))
            .collect();

        fn visit<'a>(
            id: &'a str,
            steps: &BTreeMap<&'a str, &'a StepV2>,
            marks: &mut BTreeMap<&'a str, Mark>,
        ) -> Option<&'a str> {
            match marks.get(id) {
                Some(Mark::Visiting) => return Some(id),
                Some(Mark::Done) => return None,
                None => {}
            }
            marks.insert(id, Mark::Visiting);
            if let Some(step) = steps.get(id) {
                for dependency in &step.depends_on {
                    if let Some(cycle_id) = visit(dependency.as_str(), steps, marks) {
                        return Some(cycle_id);
                    }
                }
            }
            marks.insert(id, Mark::Done);
            None
        }

        let mut marks = BTreeMap::new();
        for id in steps.keys() {
            if let Some(cycle_id) = visit(id, &steps, &mut marks) {
                return Err(format!(
                    "workflow definition '{}': dependency cycle involving step '{cycle_id}'",
                    self.id
                )
                .into());
            }
        }
        Ok(())
    }

    /// Given a cycle-free graph with every `depends_on` already resolved to a
    /// real step (both checked before this runs), forward reachability from
    /// "every step with an empty `depends_on`" can never actually fail:
    /// finite + acyclic means walking any step's `depends_on` backward always
    /// terminates at such a root. A stray, forgotten step instead shows up as
    /// a step with NEITHER a `depends_on` entry NOR any other step naming it
    /// as one -- structurally valid, but disconnected from the rest of the
    /// definition's step graph. This treats `depends_on` as undirected for
    /// that purpose and requires every step to share a connected component
    /// with `steps[0]`, the definition's anchor: a workflow may still have
    /// several genuine parallel entry points (several empty-`depends_on`
    /// steps), as long as something downstream eventually depends on each of
    /// them, tying them back into the one graph a status reader walks.
    fn reject_unreachable_steps(&self) -> CtxResult<()> {
        // Variant (`overrides_step`) steps are data donors, not DAG nodes --
        // never a candidate anchor and never required to be connected.
        let Some(anchor) = self
            .steps
            .iter()
            .find(|step| step.overrides_step.is_none())
            .map(|step| step.id.as_str())
        else {
            return Ok(());
        };
        let mut undirected: BTreeMap<&str, BTreeSet<&str>> = BTreeMap::new();
        for step in self
            .steps
            .iter()
            .filter(|step| step.overrides_step.is_none())
        {
            undirected.entry(step.id.as_str()).or_default();
            for dependency in &step.depends_on {
                undirected
                    .entry(step.id.as_str())
                    .or_default()
                    .insert(dependency.as_str());
                undirected
                    .entry(dependency.as_str())
                    .or_default()
                    .insert(step.id.as_str());
            }
        }

        let mut reached: BTreeSet<&str> = BTreeSet::new();
        let mut queue: Vec<&str> = vec![anchor];
        while let Some(id) = queue.pop() {
            if !reached.insert(id) {
                continue;
            }
            if let Some(neighbors) = undirected.get(id) {
                queue.extend(neighbors.iter().copied());
            }
        }

        for step in self
            .steps
            .iter()
            .filter(|step| step.overrides_step.is_none())
        {
            if !reached.contains(step.id.as_str()) {
                return Err(format!(
                    "workflow definition '{}': step '{}' is unreachable -- it neither depends \
                     on, nor is depended on by, any step connected to '{anchor}'",
                    self.id, step.id
                )
                .into());
            }
        }
        Ok(())
    }

    /// Sha256 hex of the canonical serialized form: `serde_json`'s default
    /// object map is `BTreeMap`-backed (this crate never enables the
    /// `preserve_order` feature), so keys are always emitted sorted --
    /// making this hash a function of the deserialized VALUE, not of
    /// whatever key order the source TOML/YAML happened to use.
    pub fn hash(&self) -> CtxResult<String> {
        let value = serde_json::to_value(self)?;
        let canonical = serde_json::to_string(&value)?;
        Ok(hash_bytes(canonical.as_bytes()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn known_skills() -> BTreeSet<&'static str> {
        ["write-intent", "implement", "verify", "frontend-implement"]
            .into_iter()
            .collect()
    }

    fn minimal_definition() -> WorkflowDefinitionV2 {
        WorkflowDefinitionV2 {
            schema_version: DEFINITION_SCHEMA_VERSION,
            id: "adaptive-work".into(),
            version: 1,
            title: "Adaptive work".into(),
            description: "Generic fallback for an unmatched task.".into(),
            domains: vec!["software".into()],
            triggers: vec!["do this".into()],
            inputs: vec![TypedInput {
                name: "task".into(),
                kind: InputKind::Text,
                required: true,
            }],
            outputs: vec![TypedOutput {
                name: "result".into(),
                kind: OutputKind::Report,
                schema: None,
            }],
            steps: vec![
                StepV2 {
                    id: "understand".into(),
                    title: "Understand".into(),
                    phase: WorkflowPhase::Intent,
                    skills: vec!["write-intent".into()],
                    agent_role: None,
                    capabilities: vec![CapabilityId::RepoRead],
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
                    id: "execute".into(),
                    title: "Execute".into(),
                    phase: WorkflowPhase::Implement,
                    skills: vec!["implement".into()],
                    agent_role: Some("implementer".into()),
                    capabilities: vec![CapabilityId::RepoWrite],
                    depends_on: vec!["understand".into()],
                    parallel_group: None,
                    condition: StepCondition::Always,
                    approval: false,
                    artifact: None,
                    max_attempts: 3,
                    effect: EffectClass::Repository,
                    reason: None,
                    domains: vec![],
                    overrides_step: None,
                },
                StepV2 {
                    id: "present".into(),
                    title: "Present".into(),
                    phase: WorkflowPhase::Present,
                    skills: vec!["verify".into()],
                    agent_role: None,
                    capabilities: vec![],
                    depends_on: vec!["execute".into()],
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
            ],
            gates: GateSpec {
                approval: vec![],
                validation: vec!["execute".into()],
                independent_review: vec![],
            },
            limits: Limits::default(),
            failure: FailurePolicy {
                escalate_to: EscalateTo::Human,
                retry: false,
            },
            effects: EffectClass::Repository,
            idempotency: None,
            completion: CompletionContract {
                required_outputs: vec!["result".into()],
                present_as: PresentAs::Summary,
            },
            presentation: None,
            override_builtin: false,
        }
    }

    /// Hand-authored TOML, matching the shape a real `packs/*.toml` file
    /// uses (parsed the same way `registry.rs` parses one), rather than a
    /// round trip through `toml::to_string` -- nothing else in this crate
    /// serializes TO toml (every existing manifest loader only ever reads
    /// hand-authored files), and this is the representative case.
    const MINIMAL_TOML: &str = r#"
schema_version = 1
id = "adaptive-work"
version = 1
title = "Adaptive work"
description = "Generic fallback for an unmatched task."
domains = ["software"]
triggers = ["do this"]
effects = "repository"

[[inputs]]
name = "task"
kind = "text"
required = true

[[outputs]]
name = "result"
kind = "report"

[[steps]]
id = "understand"
title = "Understand"
phase = "intent"
skills = ["write-intent"]
capabilities = ["repo.read"]
condition = "always"

[[steps]]
id = "execute"
title = "Execute"
phase = "implement"
skills = ["implement"]
agent_role = "implementer"
capabilities = ["repo.write"]
depends_on = ["understand"]
condition = "always"
effect = "repository"

[[steps]]
id = "present"
title = "Present"
phase = "present"
skills = ["verify"]
depends_on = ["execute"]
condition = "always"

[gates]
validation = ["execute"]

[limits]
max_attempts = 3

[failure]
escalate_to = "human"

[completion]
required_outputs = ["result"]
present_as = "summary"
"#;

    #[test]
    fn a_definition_round_trips_through_toml_and_yaml() {
        let expected = minimal_definition();
        expected.validate(&known_skills()).expect("valid");

        let from_toml: WorkflowDefinitionV2 = toml::from_str(MINIMAL_TOML).expect("parse toml");
        assert_eq!(from_toml, expected);
        from_toml.validate(&known_skills()).expect("valid");

        let yaml_text = serde_yaml_ng::to_string(&expected).expect("serialize yaml");
        let from_yaml: WorkflowDefinitionV2 =
            serde_yaml_ng::from_str(&yaml_text).expect("parse yaml");
        assert_eq!(from_yaml, expected);
    }

    #[test]
    fn a_dependency_cycle_is_rejected() {
        let mut definition = minimal_definition();
        // Make "understand" depend on "present", closing a cycle across all
        // three steps.
        definition.steps[0].depends_on = vec!["present".into()];
        let error = definition
            .validate(&known_skills())
            .expect_err("a cycle must be rejected");
        assert!(error.to_string().contains("dependency cycle"), "{error}");
    }

    #[test]
    fn an_unreachable_step_is_rejected() {
        let mut definition = minimal_definition();
        // "orphan" neither depends on anything nor is depended on by
        // anything -- structurally valid (it IS a root, per the empty-
        // `depends_on` definition used by cycle/dependency checks) but
        // disconnected from the rest of the definition's step graph.
        definition.steps.push(StepV2 {
            id: "orphan".into(),
            title: "Orphan".into(),
            phase: WorkflowPhase::Present,
            skills: vec!["verify".into()],
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
        });
        let error = definition
            .validate(&known_skills())
            .expect_err("an unreachable step must be rejected");
        assert!(error.to_string().contains("unreachable"), "{error}");
    }

    fn variant(id: &str, overrides_step: &str, skills: &[&str]) -> StepV2 {
        StepV2 {
            id: id.into(),
            title: "Variant".into(),
            phase: WorkflowPhase::Implement,
            skills: skills.iter().map(|s| (*s).to_string()).collect(),
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
            domains: vec!["frontend".into()],
            overrides_step: Some(overrides_step.into()),
        }
    }

    #[test]
    fn an_override_step_must_target_a_real_non_variant_step_with_no_dependencies_of_its_own() {
        let mut definition = minimal_definition();
        definition.steps.push(variant(
            "execute-frontend",
            "no-such-step",
            &["frontend-implement"],
        ));
        let error = definition
            .validate(&known_skills())
            .expect_err("overriding an unknown step must be rejected");
        assert!(
            error.to_string().contains("overrides unknown step"),
            "{error}"
        );

        let mut definition = minimal_definition();
        definition.steps.push(variant(
            "execute-frontend",
            "execute",
            &["frontend-implement"],
        ));
        definition.steps.push(variant(
            "execute-frontend-2",
            "execute-frontend",
            &["frontend-implement"],
        ));
        let error = definition
            .validate(&known_skills())
            .expect_err("overriding a variant must be rejected");
        assert!(error.to_string().contains("itself a variant"), "{error}");

        let mut definition = minimal_definition();
        let mut with_deps = variant("execute-frontend", "execute", &["frontend-implement"]);
        with_deps.depends_on = vec!["understand".into()];
        definition.steps.push(with_deps);
        let error = definition
            .validate(&known_skills())
            .expect_err("a variant with its own depends_on must be rejected");
        assert!(
            error
                .to_string()
                .contains("must not declare its own depends_on"),
            "{error}"
        );

        // A well-formed variant validates fine and does not need to be
        // connected to the reachability graph itself.
        let mut definition = minimal_definition();
        definition.steps.push(variant(
            "execute-frontend",
            "execute",
            &["frontend-implement"],
        ));
        definition.validate(&known_skills()).expect("valid");
    }

    #[test]
    fn an_unknown_skill_capability_or_dependency_is_rejected() {
        let mut definition = minimal_definition();
        definition.steps[0].skills = vec!["no-such-skill".into()];
        let error = definition
            .validate(&known_skills())
            .expect_err("unknown skill must be rejected");
        assert!(error.to_string().contains("unknown skill"), "{error}");

        let mut definition = minimal_definition();
        definition.steps[0].depends_on = vec!["no-such-step".into()];
        let error = definition
            .validate(&known_skills())
            .expect_err("unknown dependency must be rejected");
        assert!(error.to_string().contains("unknown step"), "{error}");

        // Capabilities are a closed enum: an unrecognized value is rejected
        // by deserialization itself rather than by `validate`.
        let bad_capability = toml::from_str::<WorkflowDefinitionV2>(
            &MINIMAL_TOML.replace("repo.read", "repo.teleport"),
        );
        assert!(
            bad_capability.is_err(),
            "an unrecognized capability id must fail to parse"
        );
    }

    /// Issue #542 review finding 2: a step whose own `effect` is `External`
    /// must be gated -- either `approval = true` on the step itself, or its
    /// id listed in `gates.approval` -- or `validate` rejects the
    /// definition. Before this fix `effect` was purely descriptive and
    /// nothing checked it at all.
    #[test]
    fn a_definition_with_an_ungated_external_step_is_rejected() {
        let mut definition = minimal_definition();
        definition.steps[1].effect = EffectClass::External;
        assert!(!definition.steps[1].approval);
        let error = definition
            .validate(&known_skills())
            .expect_err("an ungated external-effect step must be rejected");
        assert!(error.to_string().contains("effect 'external'"), "{error}");
        assert!(error.to_string().contains("execute"), "{error}");

        // Gating it via the step's own `approval` is sufficient.
        let mut approved = definition.clone();
        approved.steps[1].approval = true;
        approved.validate(&known_skills()).expect("valid");

        // Gating it via `gates.approval` alone (no `approval` field on the
        // step itself) is also sufficient.
        let mut gated = definition.clone();
        gated.gates.approval.push("execute".into());
        gated.validate(&known_skills()).expect("valid");
    }

    #[test]
    fn the_definition_hash_is_stable_across_key_order() {
        let definition = minimal_definition();
        let hash_a = definition.hash().expect("hash");

        let yaml_sorted = serde_yaml_ng::to_string(&definition).expect("serialize");
        let from_sorted: WorkflowDefinitionV2 =
            serde_yaml_ng::from_str(&yaml_sorted).expect("parse");
        let hash_b = from_sorted.hash().expect("hash");
        assert_eq!(hash_a, hash_b);

        // The same logical definition as `MINIMAL_TOML`, but with its root
        // keys given in a completely different order -- proves `hash()`
        // hashes the canonical (sorted) form of the parsed VALUE, never the
        // source text's own key order.
        const REORDERED_TOML: &str = r#"
title = "Adaptive work"
description = "Generic fallback for an unmatched task."
id = "adaptive-work"
effects = "repository"
triggers = ["do this"]
domains = ["software"]
schema_version = 1
version = 1

[[outputs]]
name = "result"
kind = "report"

[[inputs]]
name = "task"
kind = "text"
required = true

[completion]
present_as = "summary"
required_outputs = ["result"]

[failure]
escalate_to = "human"

[limits]
max_attempts = 3

[gates]
validation = ["execute"]

[[steps]]
id = "understand"
title = "Understand"
phase = "intent"
skills = ["write-intent"]
capabilities = ["repo.read"]
condition = "always"

[[steps]]
id = "execute"
title = "Execute"
phase = "implement"
skills = ["implement"]
agent_role = "implementer"
capabilities = ["repo.write"]
depends_on = ["understand"]
condition = "always"
effect = "repository"

[[steps]]
id = "present"
title = "Present"
phase = "present"
skills = ["verify"]
depends_on = ["execute"]
condition = "always"
"#;
        let from_reordered: WorkflowDefinitionV2 =
            toml::from_str(REORDERED_TOML).expect("parse reordered");
        assert_eq!(from_reordered, definition);
        assert_eq!(from_reordered.hash().expect("hash"), hash_a);
    }
}
