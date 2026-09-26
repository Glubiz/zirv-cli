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

fn is_valid_id(id: &str) -> bool {
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

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StageSpec {
    pub split: Split,
    pub reps: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HoldoutStageSpec {
    pub split: Split,
    pub reps: u32,
    #[serde(default = "default_holdout_max_uses")]
    pub max_uses: u32,
}

fn default_holdout_max_uses() -> u32 {
    1
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
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
            // Coverage note only -- `plan`/`run` still validate the rest of
            // the campaign; the campaign-level verdict is forced to
            // Unmeasured downstream (issue #802's `runtime = native` clause).
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
