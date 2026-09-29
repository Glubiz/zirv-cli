//! The versioned campaign manifest (`#802`): parsed with
//! `deny_unknown_fields` everywhere so an unrecognized key is a load-time
//! refusal, never a silently ignored typo -- a campaign manifest authorizes
//! spend and, for source-patch candidates, a build, so the same fail-loud
//! posture `CtxConfig` uses for operator config applies here too.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::promote::Criteria;
use crate::commands::ctx::CtxResult;

pub const SCHEMA_VERSION: u32 = 1;

pub(crate) fn is_valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 48
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '_' || c == '-')
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Runtime {
    Meta,
    Native,
}

impl Runtime {
    pub fn as_str(self) -> &'static str {
        match self {
            Runtime::Meta => "meta",
            Runtime::Native => "native",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SeatMode {
    Single,
    Orchestration,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CacheMode {
    #[default]
    Cold,
    Warm,
}

impl CacheMode {
    pub fn as_str(self) -> &'static str {
        match self {
            CacheMode::Cold => "cold",
            CacheMode::Warm => "warm",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Billing {
    Metered,
    Subscription,
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Baseline {
    #[serde(default = "default_baseline_commit")]
    pub commit: String,
}

fn default_baseline_commit() -> String {
    "HEAD".to_string()
}

impl Default for Baseline {
    fn default() -> Self {
        Self {
            commit: default_baseline_commit(),
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Evaluator {
    pub version: Option<String>,
    pub protected: Vec<String>,
    pub self_check: Option<Vec<String>>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Corpus {
    pub file: PathBuf,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BackendKind {
    Command,
    Fixture,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Backend {
    pub kind: BackendKind,
    #[serde(default)]
    pub command: Option<Vec<String>>,
    #[serde(default)]
    pub file: Option<PathBuf>,
    pub per_trial_ceiling_usd: f64,
    pub calls_per_trial: u32,
    pub timeout_secs: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Route {
    pub harness: String,
    pub model: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Budgets {
    pub max_spend_usd: f64,
    pub max_wall_secs: u64,
    pub max_calls: u64,
    pub max_trials: u64,
    #[serde(default)]
    pub max_retries: u32,
    #[serde(default = "default_concurrency")]
    pub concurrency: usize,
}

fn default_concurrency() -> usize {
    1
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Split {
    Dev,
    Validation,
    Holdout,
}

impl Split {
    pub fn as_str(self) -> &'static str {
        match self {
            Split::Dev => "dev",
            Split::Validation => "validation",
            Split::Holdout => "holdout",
        }
    }
}

/// Every task `class` the corpus contract (`CONTRACT.md`'s "Task corpus and
/// splits") recognizes -- a `[stages.*] classes` entry outside this set is
/// refused at manifest-validate time rather than silently matching nothing.
pub const KNOWN_TASK_CLASSES: &[&str] = &[
    "mechanical",
    "bounded",
    "bug",
    "feature",
    "architecture",
    "ambiguous",
    "sensitive",
    "long_session",
];

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StageSpec {
    pub split: Split,
    pub reps: u32,
    /// Restrict this stage to named corpus classes; absent means every class. (#804)
    #[serde(default)]
    pub classes: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HoldoutStageSpec {
    pub split: Split,
    pub reps: u32,
    #[serde(default = "default_holdout_max_uses")]
    pub max_uses: u32,
    #[serde(default)]
    pub classes: Vec<String>,
}

fn default_holdout_max_uses() -> u32 {
    1
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Stages {
    pub screen: StageSpec,
    pub validate: StageSpec,
    pub holdout: HoldoutStageSpec,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Cohort {
    pub pressure: Pressure,
    pub env: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Pressure {
    #[default]
    Natural,
    Forced,
}

impl Pressure {
    pub fn as_str(self) -> &'static str {
        match self {
            Pressure::Natural => "natural",
            Pressure::Forced => "forced",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SourcePatch {
    pub allowed_paths: Vec<String>,
    pub build: Vec<String>,
    #[serde(default = "default_bin_dir")]
    pub bin_dir: String,
}

fn default_bin_dir() -> String {
    "target/release".to_string()
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct CandidateSpace {
    pub allow_env: Vec<String>,
    pub allowed_models: Vec<String>,
    pub source_patch: Option<SourcePatch>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Strategy {
    pub kind: String,
    #[serde(default)]
    pub to_model: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Candidate {
    pub id: String,
    pub hypothesis: String,
    #[serde(default)]
    pub mechanism: Option<String>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    #[serde(default)]
    pub patch: Option<String>,
    #[serde(default)]
    pub requires_receipts: Vec<String>,
    #[serde(default)]
    pub strategy: Option<Strategy>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Proposer {
    pub harness: String,
    pub model: String,
    pub max_proposals: u32,
    pub per_call_ceiling_usd: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    pub schema: u32,
    pub id: String,
    #[serde(default)]
    pub description: String,
    pub runtime: Runtime,
    pub seat_mode: SeatMode,
    #[serde(default)]
    pub cache_mode: CacheMode,
    pub billing: Billing,
    #[serde(default)]
    pub baseline: Baseline,
    #[serde(default)]
    pub evaluator: Evaluator,
    pub corpus: Corpus,
    pub backend: Backend,
    pub route: Route,
    pub budgets: Budgets,
    pub stages: Stages,
    #[serde(default)]
    pub criteria: Criteria,
    #[serde(default)]
    pub cohort: Cohort,
    #[serde(default)]
    pub candidate_space: CandidateSpace,
    #[serde(default)]
    pub candidates: Vec<Candidate>,
    #[serde(default)]
    pub proposer: Option<Proposer>,
    /// Class stratification evaluates each class separately so gains cannot mask regressions in another class. (#804)
    #[serde(default)]
    pub stratify: Stratify,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Stratify {
    #[default]
    None,
    Class,
}

impl Manifest {
    pub fn parse(text: &str) -> CtxResult<Self> {
        let manifest: Manifest = toml::from_str(text)?;
        manifest.validate()?;
        Ok(manifest)
    }

    pub fn load(path: &Path) -> CtxResult<Self> {
        let text = std::fs::read_to_string(path)
            .map_err(|err| format!("could not read manifest '{}': {err}", path.display()))?;
        Self::parse(&text)
    }

    /// Structural validation that does not require a repository, corpus file,
    /// or any provider/backend call -- everything `plan` and `run` both need
    /// before touching disk beyond the manifest itself.
    pub fn validate(&self) -> CtxResult<()> {
        if self.schema != SCHEMA_VERSION {
            return Err(format!(
                "manifest schema {} is unsupported (expected {SCHEMA_VERSION})",
                self.schema
            )
            .into());
        }
        if !is_valid_id(&self.id) {
            return Err(format!(
                "manifest id '{}' must match [A-Za-z0-9._-]{{1,48}}",
                self.id
            )
            .into());
        }
        match self.backend.kind {
            BackendKind::Command => {
                if self
                    .backend
                    .command
                    .as_ref()
                    .map(|c| c.is_empty())
                    .unwrap_or(true)
                {
                    return Err(
                        "backend.command is required when backend.kind = \"command\"".into(),
                    );
                }
            }
            BackendKind::Fixture => {
                if self.backend.file.is_none() {
                    return Err("backend.file is required when backend.kind = \"fixture\"".into());
                }
            }
        }
        if self.budgets.concurrency == 0 {
            return Err("budgets.concurrency must be at least 1".into());
        }

        let mut seen_ids = std::collections::BTreeSet::new();
        for candidate in &self.candidates {
            if !is_valid_id(&candidate.id) {
                return Err(format!(
                    "candidate id '{}' must match [A-Za-z0-9._-]{{1,48}}",
                    candidate.id
                )
                .into());
            }
            if !seen_ids.insert(candidate.id.clone()) {
                return Err(format!("duplicate candidate id '{}'", candidate.id).into());
            }
            if candidate.patch.is_some() && self.candidate_space.source_patch.is_none() {
                return Err(format!(
                    "candidate '{}' declares a patch but [candidate_space.source_patch] is absent",
                    candidate.id
                )
                .into());
            }
            super::guard::validate_candidate_env(
                &candidate.env,
                &self.candidate_space.allow_env,
                &self.candidate_space.allowed_models,
            )
            .map_err(|reason| format!("candidate '{}': {reason}", candidate.id))?;
            super::guard::validate_requires_receipts(&candidate.requires_receipts)
                .map_err(|reason| format!("candidate '{}': {reason}", candidate.id))?;
            if let Some(strategy) = &candidate.strategy
                && let Some(to_model) = &strategy.to_model
                && !self
                    .candidate_space
                    .allowed_models
                    .iter()
                    .any(|m| m == to_model)
            {
                return Err(format!(
                    "candidate '{}': strategy.to_model '{to_model}' is not in candidate_space.allowed_models",
                    candidate.id
                )
                .into());
            }
        }

        super::guard::validate_cohort_env(&self.cohort.env, &self.candidate_space.allowed_models)
            .map_err(|reason| format!("[cohort].env: {reason}"))?;

        if matches!(self.runtime, Runtime::Native) {
            // Native runtime coverage remains unmeasured until the campaign is validated downstream. (#802)
        }

        for (label, classes) in [
            ("screen", &self.stages.screen.classes),
            ("validate", &self.stages.validate.classes),
            ("holdout", &self.stages.holdout.classes),
        ] {
            for class in classes {
                if !KNOWN_TASK_CLASSES.contains(&class.as_str()) {
                    return Err(format!(
                        "stages.{label}.classes: '{class}' is not a known task class (expected one of {KNOWN_TASK_CLASSES:?})"
                    )
                    .into());
                }
            }
        }

        // Screen and validate must use different splits; holdout is reserved for final confirmation. (#801)
        if self.stages.screen.split == self.stages.validate.split {
            return Err(format!(
                "stages.screen and stages.validate must use different splits (both use {:?}): validate would re-measure exactly what screen already saw",
                self.stages.screen.split
            )
            .into());
        }
        if self.stages.screen.split == Split::Holdout {
            return Err(
                "stages.screen.split may not be \"holdout\": holdout is reserved for the single final confirmation"
                    .into(),
            );
        }
        if self.stages.validate.split == Split::Holdout {
            return Err(
                "stages.validate.split may not be \"holdout\": holdout is reserved for the single final confirmation"
                    .into(),
            );
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::workflow::research::promote::Objective;

    fn minimal_toml(extra: &str) -> String {
        format!(
            r#"
schema = 1
id = "demo"
description = "demo campaign"
runtime = "meta"
seat_mode = "single"
cache_mode = "cold"
billing = "subscription"

[baseline]
commit = "HEAD"

[corpus]
file = "corpus.toml"

[backend]
kind = "fixture"
file = "fixture.toml"
per_trial_ceiling_usd = 1.0
calls_per_trial = 4
timeout_secs = 60

[route]
harness = "claude"
model = "sonnet"

[budgets]
max_spend_usd = 10.0
max_wall_secs = 3600
max_calls = 100
max_trials = 20
max_retries = 1
concurrency = 2

[stages.screen]
split = "dev"
reps = 1

[stages.validate]
split = "validation"
reps = 2

[stages.holdout]
split = "holdout"
reps = 1
max_uses = 1
{extra}
"#
        )
    }

    #[test]
    fn a_well_formed_manifest_parses_and_validates() {
        let manifest = Manifest::parse(&minimal_toml("")).expect("minimal manifest must parse");
        assert_eq!(manifest.id, "demo");
        assert_eq!(manifest.budgets.concurrency, 2);
    }

    #[test]
    fn an_unknown_top_level_field_is_rejected() {
        let text = minimal_toml("\nnot_a_real_field = true\n");
        let err = Manifest::parse(&text).expect_err("unknown field must be refused");
        assert!(err.to_string().contains("not_a_real_field"), "got: {err}");
    }

    /// Absent `[criteria] objective` must keep today's behaviour --
    /// the promotion gate stays tuned to cost/wall efficiency.
    #[test]
    fn criteria_objective_defaults_to_efficiency_when_absent() {
        let manifest = Manifest::parse(&minimal_toml("")).expect("minimal manifest must parse");
        assert_eq!(manifest.criteria.objective, Objective::Efficiency);
    }

    #[test]
    fn an_unknown_criteria_objective_is_refused() {
        let text = minimal_toml("\n[criteria]\nobjective = \"speed\"\n");
        let err = Manifest::parse(&text).expect_err("an unrecognized objective must be refused");
        assert!(err.to_string().contains("objective"), "got: {err}");
    }

    #[test]
    fn criteria_objective_quality_is_accepted() {
        let text = minimal_toml("\n[criteria]\nobjective = \"quality\"\n");
        let manifest = Manifest::parse(&text).expect("objective = \"quality\" must parse");
        assert_eq!(manifest.criteria.objective, Objective::Quality);
    }

    #[test]
    fn a_candidate_cannot_carry_a_budgets_field() {
        let text = minimal_toml(
            r#"
[[candidates]]
id = "cand-a"
hypothesis = "test"
budgets = { max_spend_usd = 5.0 }
"#,
        );
        let err = Manifest::parse(&text).expect_err("candidate budgets field must be refused");
        assert!(err.to_string().contains("budgets"), "got: {err}");
    }

    #[test]
    fn a_requires_receipts_entry_with_an_unknown_prefix_is_rejected() {
        let text = minimal_toml(
            r#"
[[candidates]]
id = "cand-a"
hypothesis = "test"
requires_receipts = ["totally-made-up:thing"]
"#,
        );
        let err =
            Manifest::parse(&text).expect_err("an unrecognized receipt prefix must be refused");
        assert!(
            err.to_string().contains("totally-made-up:thing"),
            "got: {err}"
        );
    }

    fn manifest_with_stage_splits(screen_split: &str, validate_split: &str) -> String {
        format!(
            r#"
schema = 1
id = "demo"
runtime = "meta"
seat_mode = "single"
cache_mode = "cold"
billing = "subscription"

[baseline]
commit = "HEAD"

[corpus]
file = "corpus.toml"

[backend]
kind = "fixture"
file = "fixture.toml"
per_trial_ceiling_usd = 1.0
calls_per_trial = 4
timeout_secs = 60

[route]
harness = "claude"
model = "sonnet"

[budgets]
max_spend_usd = 10.0
max_wall_secs = 3600
max_calls = 100
max_trials = 20
max_retries = 1
concurrency = 2

[stages.screen]
split = "{screen_split}"
reps = 1

[stages.validate]
split = "{validate_split}"
reps = 2

[stages.holdout]
split = "holdout"
reps = 1
max_uses = 1
"#
        )
    }

    #[test]
    fn screen_and_validate_on_the_same_split_are_refused() {
        let err = Manifest::parse(&manifest_with_stage_splits("dev", "dev"))
            .expect_err("screen and validate sharing a split must be refused");
        assert!(err.to_string().contains("different splits"), "got: {err}");
    }

    #[test]
    fn screen_or_validate_on_holdout_is_refused() {
        let err = Manifest::parse(&manifest_with_stage_splits("holdout", "validation"))
            .expect_err("stages.screen.split = holdout must be refused");
        assert!(
            err.to_string().contains("stages.screen.split"),
            "got: {err}"
        );

        let err = Manifest::parse(&manifest_with_stage_splits("dev", "holdout"))
            .expect_err("stages.validate.split = holdout must be refused");
        assert!(
            err.to_string().contains("stages.validate.split"),
            "got: {err}"
        );
    }

    #[test]
    fn an_unknown_stage_class_is_refused() {
        let text = manifest_with_stage_splits("dev", "validation").replacen(
            "[stages.screen]\nsplit = \"dev\"\nreps = 1\n",
            "[stages.screen]\nsplit = \"dev\"\nreps = 1\nclasses = [\"not-a-real-class\"]\n",
            1,
        );
        let err = Manifest::parse(&text).expect_err("an unrecognized class must be refused");
        assert!(err.to_string().contains("not-a-real-class"), "got: {err}");
    }

    #[test]
    fn a_known_stage_class_is_accepted() {
        let text = manifest_with_stage_splits("dev", "validation").replacen(
            "[stages.screen]\nsplit = \"dev\"\nreps = 1\n",
            "[stages.screen]\nsplit = \"dev\"\nreps = 1\nclasses = [\"long_session\"]\n",
            1,
        );
        let manifest = Manifest::parse(&text).expect("a known class must parse and validate");
        assert_eq!(
            manifest.stages.screen.classes,
            vec!["long_session".to_string()]
        );
    }
}
