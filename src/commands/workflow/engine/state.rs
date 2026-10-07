//! [`WorkflowState`] and its durable, on-disk persistence (issue #542-split).

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::commands::ctx::CtxResult;
use crate::commands::ctx::jev::{self, AnswerValue, Question};
use crate::commands::ctx::state::{
    StateDir, create_private_dir_all, now_secs, repo_slug, write_private,
};
use crate::commands::workflow::classify::Classification;
use crate::commands::workflow::deploy::DeployTier;

use super::definitions::*;
/// Schema 5 pins the workflow definition; load upgrades older state without changing in-flight semantics. (#542)
pub const WORKFLOW_SCHEMA_VERSION: u32 = 5;

/// The previous state schema `load` still accepts and upgrades in place.
const WORKFLOW_SCHEMA_VERSION_V4: u32 = 4;

const MAX_WORK_ARTIFACT_CONTEXT_BYTES: usize = 24 * 1024;

/// Minimum artifact-substance confidence from the 2026-09-18 probe.
const JEV_ARTIFACT_CONFIDENCE: f32 = 0.9;

/// [`pin_current_artifact_with_config`]'s own production advise-site LABEL.
pub(crate) const ARTIFACT_SUBSTANCE_LABEL: &str = "workflow-artifact-substance";

/// [`pin_current_artifact_with_config`]'s own default `(min_confidence,
/// min_margin)` `decisive()` floor -- named (issue: `zirv ctx jev probe`) so
/// a later retune targets exactly this constant.
pub(crate) const ARTIFACT_SUBSTANCE_DEFAULT_FLOOR: (f32, f32) =
    (JEV_ARTIFACT_CONFIDENCE, jev::DEFAULT_MIN_MARGIN);

fn default_true() -> bool {
    true
}

// `Selection` contains `f64` confidence, so workflow state supports `PartialEq` but not `Eq`. (#542)
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WorkflowState {
    pub schema_version: u32,
    pub id: String,
    pub repo: PathBuf,
    /// An unresolved branch is empty and cannot match sibling-worktree evidence; older state defaults to empty. (#467)
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
    /// Pin the definition used at start; older state without one retains legacy kind semantics. (#542)
    #[serde(default)]
    pub definition: Option<DefinitionRef>,
    #[serde(default)]
    pub review_findings: Vec<crate::commands::workflow::review::ReviewFinding>,
    #[serde(default)]
    pub review_evidence: Vec<crate::commands::workflow::review::ReviewRunEvidence>,
    #[serde(default)]
    pub usage_checkpoint: Option<UsageCheckpoint>,
    /// Optional sibling repository for frontend evidence; absent means this repository.
    #[serde(default)]
    pub frontend_target_root: Option<PathBuf>,
    /// Track whether profile was classified or operator-forced; older state defaults to classified.
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
    /// Remember gate-only approval by step id so recomputation cannot reopen the current gate; unique ids prevent stale approval matching another step. (#542)
    #[serde(default)]
    pub current_step_approved: Option<String>,
    /// Intent steps use interactive brainstorming or autonomous write-intent; older state defaults to interactive.
    #[serde(default = "default_true")]
    pub brainstorm: bool,
    pub status: WorkflowStatus,
    /// Operator-supplied close reason; older state defaults to absent.
    #[serde(default)]
    pub closed_reason: Option<String>,
    /// When this workflow was closed (`WorkflowStatus::Closed`), `now_secs()`
    /// at that moment. `None` for a workflow never closed.
    #[serde(default)]
    pub closed_at: Option<u64>,
    /// Most recently compiled team plan, absent until one is saved or in older state. (#541)
    #[serde(default)]
    pub team_plan: Option<crate::commands::workflow::team::TeamPlan>,
    /// Persist deterministic pack selection so status can explain it; explicit-id starts have no selection. (#542)
    #[serde(default)]
    pub selection: Option<crate::commands::workflow::selection::Selection>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub jev_tags: Vec<String>,
    pub created_at: u64,
    pub updated_at: u64,
}

impl WorkflowState {
    pub fn current(&self) -> Option<&WorkflowStep> {
        self.steps.get(self.current_step)
    }

    /// External effects require approval even if the pack omits its flag; gate-only approval applies only to this exact step id. (#542)
    pub(super) fn step_requires_approval(&self, step: &WorkflowStep) -> bool {
        (step.approval
            || step.effect == crate::commands::workflow::definition::EffectClass::External)
            && self.current_step_approved.as_deref() != Some(step.id.as_str())
    }

    /// Start one of the five legacy kinds through its materialized definition, preserving the caller contract. (#542)
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

    /// Start any resolved registry pack; `definition` is authoritative while legacy `kind` remains for older readers. (#542)
    pub(crate) fn start_from_pack(
        repo: PathBuf,
        task: String,
        pack: &crate::commands::workflow::registry::RegisteredWorkflow,
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
        definition: &crate::commands::workflow::definition::WorkflowDefinitionV2,
        hash: String,
        source: crate::commands::workflow::registry::WorkflowSource,
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
        // An External-effect first step must gate even when its pack omitted the step approval flag. (#542)
        let status = if steps.first().is_some_and(|step| {
            step.approval
                || step.effect == crate::commands::workflow::definition::EffectClass::External
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
                inline: (source != crate::commands::workflow::registry::WorkflowSource::BuiltIn)
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

pub(super) fn initial_artifact_records(
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

pub(super) fn sync_artifact_records(state: &mut WorkflowState) {
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
pub(super) fn refuse_symlinked_artifact_path(
    repo: &Path,
    workflow_id: &str,
    path: &Path,
) -> CtxResult<()> {
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
pub(super) fn work_dir_is_gitignored(repo: &Path) -> bool {
    std::process::Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["check-ignore", "--quiet", ".zirv/work"])
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

/// Keeps zirv's own bookkeeping out of a repository's `git status`: in a repo
/// that does not track anything under `.zirv/`, adds `.zirv/work/` (and
/// `.zirv/verify.toml`, only while that file does not exist yet, so a file the
/// operator authored is never hidden) to the repo-local `.git/info/exclude`.
/// Never touches `.gitignore` or any tracked file; idempotent; best-effort
/// (any git/IO failure is a silent no-op). A repo that already tracks `.zirv/`
/// (zirv's own, which commits `.zirv/work/` on purpose) is left unchanged.
pub(crate) fn exclude_zirv_artifacts_from_git(repo: &Path) {
    // Runs on every state persist: skip the git shell-outs once a plain `.git` dir already lists everything wanted.
    let verify_exists = repo.join(".zirv").join("verify.toml").exists();
    if std::fs::read_to_string(repo.join(".git").join("info").join("exclude")).is_ok_and(|text| {
        let has = |want: &str| text.lines().any(|have| have.trim() == want);
        has(".zirv/work/") && (verify_exists || has(".zirv/verify.toml"))
    }) {
        return;
    }
    let git = |args: &[&str]| {
        std::process::Command::new("git")
            .arg("-C")
            .arg(repo)
            .args(args)
            .output()
            .ok()
            .filter(|out| out.status.success())
    };
    let Some(tracked) = git(&["ls-files", "--", ".zirv"]) else {
        return;
    };
    if !tracked.stdout.is_empty() {
        return;
    }
    let Some(path) = git(&["rev-parse", "--git-path", "info/exclude"]) else {
        return;
    };
    let rel = String::from_utf8_lossy(&path.stdout).trim().to_string();
    if rel.is_empty() {
        return;
    }
    let exclude = repo.join(rel);
    let mut wanted = vec![".zirv/work/"];
    if !repo.join(".zirv").join("verify.toml").exists() {
        wanted.push(".zirv/verify.toml");
    }
    let existing = std::fs::read_to_string(&exclude).unwrap_or_default();
    let missing: Vec<&str> = wanted
        .into_iter()
        .filter(|line| !existing.lines().any(|have| have.trim() == *line))
        .collect();
    if missing.is_empty() {
        return;
    }
    let mut out = existing;
    if !out.is_empty() && !out.ends_with('\n') {
        out.push('\n');
    }
    for line in missing {
        out.push_str(line);
        out.push('\n');
    }
    if let Some(parent) = exclude.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(&exclude, out);
}

pub(super) fn workflow_artifact_path(
    state: &WorkflowState,
    stage: ArtifactStage,
) -> CtxResult<PathBuf> {
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

/// F6 (blind-review finding, 2026-09-24): no production path calls this any
/// more -- `zirv workflow start`/advance/resume/render used to call it to
/// pre-create an unfilled template file in the worktree, which a downstream
/// `git add .`/`git commit -a` then swept in as a stray addition (16/20
/// runs in a blind review) despite nobody having written anything into it.
/// The path and template text stay discoverable without writing anything
/// (`render_current_context`'s own doc comment), and every reader of the
/// artifact (`pin_current_artifact_with_config`, `artifact_drift`,
/// `read_accepted_artifact`, `workflow_artifact_statuses`) already treats a
/// missing file as "not filled"/"not accepted", not an error. Kept
/// `#[cfg(test)]`-only as a fixture-setup helper: a test that wants to
/// exercise the ACCEPT/ADVANCE paths against a real on-disk artifact still
/// needs a quick way to materialize the untouched template first.
#[cfg(test)]
pub(super) fn ensure_current_artifact_template(state: &WorkflowState) -> CtxResult<()> {
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

pub(crate) fn hash_bytes(bytes: &[u8]) -> String {
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

pub(super) fn rfc3339_now() -> String {
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

/// Metadata-only facts for the artifact substance advisory; must satisfy `jev::safe_metadata_request`. No artifact text is sent.
/// Row: [site=5, stage, size bucket, template size bucket, lines, lines absent from the template, headings, bullets,
/// checkboxes, code fences, placeholder hits, prose lines].
pub(crate) fn artifact_jev_facts(stage: ArtifactStage, body: &str) -> serde_json::Value {
    let template = stage.template();
    let template_lines: BTreeSet<&str> = template.lines().map(str::trim).collect();
    let lines: Vec<&str> = body.lines().map(str::trim).collect();
    let count = |predicate: &dyn Fn(&str) -> bool| {
        lines.iter().filter(|line| predicate(line)).count() as u64
    };
    let headings = count(&|line| line.starts_with('#'));
    let bullets = count(&|line| line.starts_with("- ") || line.starts_with("* "));
    let checkboxes = count(&|line| line.starts_with("- [") || line.starts_with("* ["));
    let fences = count(&|line| line.starts_with("```"));
    let lower = body.to_ascii_lowercase();
    let placeholders = ["todo", "tbd", "fixme", "<describe", "lorem"]
        .iter()
        .map(|needle| lower.matches(needle).count() as u64)
        .sum::<u64>();
    let prose = count(&|line| {
        !line.is_empty()
            && !line.starts_with('#')
            && !line.starts_with("- ")
            && !line.starts_with("* ")
    });
    let new_lines = count(&|line| !line.is_empty() && !template_lines.contains(line));
    serde_json::json!({
        "_zirv_metadata_only": true,
        "facts": [[
            5,
            stage as u64,
            (body.len() as u64 / 256).min(1000),
            (template.len() as u64 / 256).min(1000),
            (lines.len() as u64).min(10_000),
            new_lines.min(10_000),
            headings.min(1000),
            bullets.min(1000),
            checkboxes.min(1000),
            fences.min(1000),
            placeholders.min(1000),
            prose.min(10_000),
        ]],
    })
}

macro_rules! artifact_facts {
    () => {
        "From facts [site=5, artifact stage (0 intent, 1 spec, 2 plan), size and template size in 256-byte units, line count, lines absent from the template, heading count, bullet count, checkbox count, code fence count, placeholder hits (todo/tbd/fixme), prose line count]"
    };
}

/// [`pin_current_artifact_with_config`]'s own single Choice question,
/// factored out so `zirv ctx jev probe` can ask the exact same question from
/// a fixture's own state.
pub(crate) fn artifact_substance_questions() -> [Question; 1] {
    [Question::metadata_choice(
        "substance",
        concat!(
            artifact_facts!(),
            ", assess whether this artifact has substantive content for its section headings. Choose substantive if the metadata is insufficient."
        ),
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
    )]
}

/// [`pin_current_artifact_with_config`]'s own per-call decision: `"refuse"`
/// for a decisive `template_copy`, `"warn"` for a decisive `thin`, `"pass"`
/// otherwise (missing answer, indecisive, or a decisive `substantive`) --
/// `pin_current_artifact_with_config`'s own fallback outcome (pin with no
/// warning). Shared with `zirv ctx jev probe`, which reports exactly this
/// outcome.
pub(crate) fn artifact_substance_action(
    answer: Option<&jev::Answer>,
    min_confidence: f32,
    min_margin: f32,
) -> &'static str {
    let Some(answer) = answer else {
        return "pass";
    };
    if !answer.decisive(min_confidence, min_margin) {
        return "pass";
    }
    let AnswerValue::Choice(choice) = &answer.value else {
        return "pass";
    };
    match choice.as_str() {
        "template_copy" => "refuse",
        "thin" => "warn",
        _ => "pass",
    }
}

pub(super) fn pin_current_artifact_with_config(
    state_dir: &StateDir,
    state: &mut WorkflowState,
    cfg: Option<&crate::commands::ctx::config::CtxConfig>,
) -> CtxResult<(ArtifactStage, Option<String>)> {
    let stage = state
        .current()
        .and_then(|step| step.artifact)
        .ok_or("current workflow step has no artifact to approve")?;
    let path = workflow_artifact_path(state, stage)?;
    // A missing unfilled artifact is equivalent to an untouched template and must trigger the same refusal, not an I/O error.
    let body = std::fs::read_to_string(&path).unwrap_or_else(|_| stage.template().to_string());
    if body.trim() == stage.template().trim() {
        return Err(format!(
            "{stage} artifact is still the untouched template: {}",
            path.display()
        )
        .into());
    }
    let mut warning = None;
    if let Some(cfg) = cfg {
        let advice_state = artifact_jev_facts(stage, &body);
        let questions = artifact_substance_questions();
        if let Some(answers) = jev::advise(
            cfg,
            state_dir,
            ARTIFACT_SUBSTANCE_LABEL,
            cfg.jev.gates,
            &advice_state,
            &questions,
        ) {
            let answer = answers.get("substance");
            let (min_confidence, min_margin) = ARTIFACT_SUBSTANCE_DEFAULT_FLOOR;
            match artifact_substance_action(answer, min_confidence, min_margin) {
                "refuse" => {
                    let confidence = answer.map_or(0.0, |answer| answer.confidence);
                    return Err(format!(
                        "{stage} artifact refused by the template_copy advisory at {:.2} confidence: {}",
                        confidence,
                        path.display()
                    )
                    .into());
                }
                "warn" => {
                    let confidence = answer.map_or(0.0, |answer| answer.confidence);
                    warning = Some(format!(
                        "{stage} artifact substance advisory is thin at {:.2} confidence; pinning anyway",
                        confidence
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

pub(super) fn load_workflow_jev_config(
    repo: &Path,
) -> Option<crate::commands::ctx::config::CtxConfig> {
    crate::commands::ctx::config::CtxConfig::load(repo, &|key| std::env::var(key).ok()).ok()
}

pub(super) fn artifact_drift(state: &WorkflowState) -> CtxResult<Option<ArtifactStage>> {
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

pub(super) fn reopen_artifact_gate(
    state: &mut WorkflowState,
    stage: ArtifactStage,
) -> CtxResult<()> {
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
    // Clear gate-only approvals for rewound steps so they cannot remain granted when execution reaches them again. (#542)
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
    Ok(())
}

pub(super) fn append_accepted_artifacts(
    state: &WorkflowState,
    rendered: &mut String,
) -> CtxResult<()> {
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
    // Never hand changed bytes on as an accepted artifact; re-check the pinned hash before appending.
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

pub(super) fn repo_dir(state: &StateDir, repo: &Path) -> PathBuf {
    // Keep workflow state keyed to the literal checkout so sibling worktrees retain separate active pointers; cross-checkout lookup is explicit. (#467)
    state.workflows().join(repo_slug(repo))
}

pub(super) fn state_path(state: &StateDir, repo: &Path, id: &str) -> CtxResult<PathBuf> {
    state_path_in(&repo_dir(state, repo), id)
}

pub(super) fn state_path_in(dir: &Path, id: &str) -> CtxResult<PathBuf> {
    if !id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') {
        return Err(format!("invalid workflow id '{id}'").into());
    }
    Ok(dir.join(format!("{id}.json")))
}

pub(super) fn active_path(state: &StateDir, repo: &Path) -> PathBuf {
    repo_dir(state, repo).join("active")
}

/// Every workflow persist (CLI start, hook auto-start, maintenance, advance) funnels through here, so the
/// git-status exclusion for zirv's `.zirv/work/` artifacts covers every entry point.
pub(super) fn write_state_file(state_dir: &StateDir, state: &WorkflowState) -> CtxResult<()> {
    exclude_zirv_artifacts_from_git(&state.repo);
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

/// Persist state without changing any active pointer; annotating a non-active workflow must not deactivate another. (#541)
pub(crate) fn save_preserving_active(state_dir: &StateDir, state: &WorkflowState) -> CtxResult<()> {
    write_state_file(state_dir, state)
}

/// Persists `state` and clears this repository's active pointer only when it
/// currently names `state.id` -- unlike `save(state_dir, state, false)`,
/// which clears the pointer unconditionally regardless of which workflow it
/// names. Used by [`close`] so closing an older, non-active workflow never
/// deactivates a different, currently-running workflow for the same repo.
pub(super) fn save_inactive_if_active(
    state_dir: &StateDir,
    state: &WorkflowState,
) -> CtxResult<()> {
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

/// Search the literal checkout first, then sibling checkouts for an explicit workflow id; the id makes this widening unambiguous. (#467)
pub(super) fn resolve_state_path_for_id(
    state: &StateDir,
    repo: &Path,
    id: &str,
) -> CtxResult<PathBuf> {
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

/// Use state-file mtime as the completed-run timestamp; unreadable metadata yields none, never fabricated recency.
pub fn state_mtime_secs(state: &StateDir, repo: &Path, id: &str) -> Option<u64> {
    let path = resolve_state_path_for_id(state, repo, id).ok()?;
    let modified = std::fs::metadata(&path).ok()?.modified().ok()?;
    modified
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .map(|d| d.as_secs())
}

pub(super) fn load_from_path(path: &Path, id: &str) -> CtxResult<WorkflowState> {
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
            // Upgrade only the version marker for unpinned legacy state; kind-only semantics remain unchanged. (#542)
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

pub(super) fn read_active_pointer(state: &StateDir, repo: &Path) -> CtxResult<Option<String>> {
    let path = active_path(state, repo);
    if !path.exists() {
        return Ok(None);
    }
    Ok(Some(std::fs::read_to_string(path)?.trim().to_string()))
}

/// Use the literal checkout’s active pointer, falling back only to the main checkout; never inherit an arbitrary sibling’s workflow. (#467)
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

/// The live workflow bound to session `short`, never another session's. A
/// session with no registry record (a plain harness run) has nothing to bind
/// to, so it falls back to the checkout's active pointer.
pub fn load_active_for_session(
    state: &StateDir,
    repo: &Path,
    short: &str,
) -> CtxResult<Option<WorkflowState>> {
    if crate::commands::ctx::sessions::load_record(state, short).is_none() {
        return load_active(state, repo);
    }
    let Some(id) = crate::commands::ctx::sessions::workflow_id_for(state, short) else {
        return Ok(None);
    };
    let Ok(bound) = load(state, repo, &id) else {
        return Ok(None);
    };
    Ok(matches!(
        bound.status,
        WorkflowStatus::Running | WorkflowStatus::AwaitingApproval
    )
    .then_some(bound))
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

#[cfg(test)]
mod tests {
    use std::path::Path;

    use tempfile::tempdir;

    use crate::commands::ctx::state::StateDir;

    use crate::commands::workflow::classify::{Classification, Complexity, RiskBand};

    use crate::commands::workflow::skill::{SkillRegistry, WorkflowPhase};

    use super::super::cli::*;
    use super::*;

    use super::super::lifecycle::*;

    use super::super::tests::{choice_answer, jev_gate_config, low_classification, review_finding};
    use super::super::transition::*;

    /// A repo-wide active pointer must not make an unrelated, registered
    /// session look like it is inside the workflow; an unregistered one has
    /// no binding to read and keeps the checkout-level answer.
    #[test]
    fn active_for_session_is_the_sessions_own_binding_not_the_repo_pointer() {
        use crate::commands::ctx::sessions::{Record, SessionGuard, Verb, bind_workflow_id};
        let repo = tempdir().unwrap();
        let root = tempdir().unwrap();
        let state_dir = StateDir::from_root(root.path().to_path_buf());
        let wf = WorkflowState::start(
            repo.path().to_path_buf(),
            "a task".into(),
            WorkflowKind::Feature,
            None,
            true,
            low_classification(),
        );
        save(&state_dir, &wf, true).unwrap();
        let record = |id: &str| Record::new(id, "claude", repo.path(), Verb::Chat);
        let _owner = SessionGuard::register(&state_dir, record("aaaa1111bbbb2222cccc3333dddd4444"));
        let _other = SessionGuard::register(&state_dir, record("eeee5555ffff6666aaaa7777bbbb8888"));
        let owner = crate::commands::ctx::sessions::short_id("aaaa1111bbbb2222cccc3333dddd4444");
        let other = crate::commands::ctx::sessions::short_id("eeee5555ffff6666aaaa7777bbbb8888");
        bind_workflow_id(&state_dir, &owner, &wf.id);

        let bound = load_active_for_session(&state_dir, repo.path(), &owner).unwrap();
        assert_eq!(bound.map(|w| w.id), Some(wf.id.clone()));
        assert!(
            load_active_for_session(&state_dir, repo.path(), &other)
                .unwrap()
                .is_none(),
            "a registered session with no binding is not in the repo's workflow"
        );
        let unregistered = load_active_for_session(&state_dir, repo.path(), "00000000").unwrap();
        assert_eq!(unregistered.map(|w| w.id), Some(wf.id));
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
        let events = crate::commands::workflow::telemetry::list(&state_dir, repo.path()).unwrap();
        assert!(
            events.iter().any(|event| event.kind
                == crate::commands::workflow::telemetry::TelemetryKind::PhaseFailed
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

    /// #260-adjacent (T3, bulk dispose): three open findings with mixed
    /// recommendations must each land on their own recommended disposition
    /// in one call; a finding with no recommendation stays `Open` and is
    /// still reported (not silently dropped); an already-resolved finding is
    /// left alone and not reported at all.
    #[test]
    fn apply_recommended_dispositions_applies_each_open_findings_own_recommendation() {
        use crate::commands::workflow::review::FindingDisposition as Disposition;
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
        minor_dismissal.severity = crate::commands::workflow::review::FindingSeverity::Minor;
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
    fn artifact_metadata_state_refuses_a_decisive_template_copy() {
        let body = r#"{"model":"jev-latest","answers":{"substance":{"type":"choice","choice":"template_copy","probabilities":{"template_copy":0.95,"substantive":0.05},"confidence":0.95}},"usage":{"input_tokens":10,"output_tokens":1}}"#;
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
        assert!(error.contains("template_copy advisory"), "{error}");
        let sent = request
            .recv_timeout(std::time::Duration::from_secs(2))
            .expect("the metadata state reaches Jev");
        assert!(
            !sent.contains("Changed wording") && !sent.contains("A sentence"),
            "{sent}"
        );
        assert!(state.artifacts["intent"].accepted_hash.is_none());
    }

    #[test]
    fn artifact_metadata_state_pins_on_a_thin_margin() {
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
        let sent = request
            .recv_timeout(std::time::Duration::from_secs(2))
            .expect("the metadata state reaches Jev");
        assert!(
            !sent.contains("Changed wording") && !sent.contains("A sentence"),
            "{sent}"
        );

        assert_eq!(stage, ArtifactStage::Intent);
        assert!(warning.is_none(), "{warning:?}");
        assert_eq!(
            state.artifacts["intent"].accepted_hash.as_deref(),
            Some(hash_bytes(body_text.as_bytes()).as_str())
        );
    }

    #[test]
    fn artifact_metadata_state_warns_on_decisive_thin_content() {
        let body = r#"{"model":"jev-latest","answers":{"substance":{"type":"choice","choice":"thin","probabilities":{"thin":0.95,"substantive":0.05},"confidence":0.95}},"usage":{"input_tokens":10,"output_tokens":1}}"#;
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
        let sent = request
            .recv_timeout(std::time::Duration::from_secs(2))
            .expect("the metadata state reaches Jev");
        assert!(
            !sent.contains("Changed wording") && !sent.contains("A sentence"),
            "{sent}"
        );
        assert!(warning.expect("thin advisory").contains("thin"));
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
    fn artifact_request_passes_the_metadata_only_guard_without_text() {
        let facts = artifact_jev_facts(
            ArtifactStage::Intent,
            "# Intent\n\n## Problem\nSECRET_TOKEN_TEXT\n",
        );
        assert!(crate::commands::ctx::jev::safe_metadata_request(
            &facts,
            &artifact_substance_questions(),
            "jev-latest"
        ));
        assert!(!facts.to_string().contains("SECRET_TOKEN_TEXT"));
    }

    #[test]
    fn artifact_metadata_state_http_error_still_pins() {
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
        let sent = request
            .recv_timeout(std::time::Duration::from_secs(2))
            .expect("the metadata state reaches Jev");
        assert!(
            !sent.contains("Changed wording") && !sent.contains("A sentence"),
            "{sent}"
        );

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
    fn git_init(dir: &Path) {
        std::process::Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(["init", "-q"])
            .output()
            .unwrap();
    }

    fn git_porcelain(dir: &Path) -> String {
        let out = std::process::Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(["status", "--porcelain"])
            .output()
            .unwrap();
        String::from_utf8_lossy(&out.stdout).into_owned()
    }

    #[test]
    fn exclude_adds_zirv_paths_in_a_fresh_repo_idempotently() {
        let repo = tempdir().unwrap();
        git_init(repo.path());
        exclude_zirv_artifacts_from_git(repo.path());
        exclude_zirv_artifacts_from_git(repo.path());
        let text = std::fs::read_to_string(repo.path().join(".git/info/exclude")).unwrap();
        assert_eq!(text.matches(".zirv/work/\n").count(), 1);
        assert_eq!(text.matches(".zirv/verify.toml\n").count(), 1);
        std::fs::create_dir_all(repo.path().join(".zirv/work/w")).unwrap();
        std::fs::write(repo.path().join(".zirv/work/w/intent.md"), "x").unwrap();
        std::fs::write(repo.path().join(".zirv/verify.toml"), "x").unwrap();
        assert_eq!(git_porcelain(repo.path()), "");
        assert!(!repo.path().join(".gitignore").exists());
    }

    #[test]
    fn exclude_changes_nothing_when_repo_tracks_zirv() {
        let repo = tempdir().unwrap();
        git_init(repo.path());
        std::fs::create_dir_all(repo.path().join(".zirv")).unwrap();
        std::fs::write(repo.path().join(".zirv/ctx.toml"), "").unwrap();
        std::process::Command::new("git")
            .arg("-C")
            .arg(repo.path())
            .args(["add", ".zirv/ctx.toml"])
            .output()
            .unwrap();
        let exclude = repo.path().join(".git/info/exclude");
        let before = std::fs::read_to_string(&exclude).unwrap_or_default();
        exclude_zirv_artifacts_from_git(repo.path());
        assert_eq!(
            before,
            std::fs::read_to_string(&exclude).unwrap_or_default()
        );
    }

    /// Persisting any workflow state excludes zirv artifacts, whichever entry point created it.
    #[test]
    fn saving_a_workflow_excludes_zirv_artifacts_from_git() {
        let repo = tempdir().unwrap();
        git_init(repo.path());
        let root = tempdir().unwrap();
        let state_dir = StateDir::from_root(root.path().to_path_buf());
        let wf = WorkflowState::start(
            repo.path().to_path_buf(),
            "a task".into(),
            WorkflowKind::Feature,
            None,
            true,
            low_classification(),
        );
        save(&state_dir, &wf, true).unwrap();
        let text = std::fs::read_to_string(repo.path().join(".git/info/exclude")).unwrap();
        assert_eq!(text.matches(".zirv/work/\n").count(), 1);
        save(&state_dir, &wf, true).unwrap();
        let again = std::fs::read_to_string(repo.path().join(".git/info/exclude")).unwrap();
        assert_eq!(text, again);
    }

    #[test]
    fn exclude_does_not_hide_an_existing_verify_toml() {
        let repo = tempdir().unwrap();
        git_init(repo.path());
        std::fs::create_dir_all(repo.path().join(".zirv")).unwrap();
        std::fs::write(repo.path().join(".zirv/verify.toml"), "x").unwrap();
        exclude_zirv_artifacts_from_git(repo.path());
        let text = std::fs::read_to_string(repo.path().join(".git/info/exclude")).unwrap();
        assert!(!text.contains("verify.toml"));
    }

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
            include_str!("../../../../tests/fixtures/workflow/state-v4/feature.json");

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
            crate::commands::workflow::review::FindingDisposition::Residual,
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
            include_str!("../../../../tests/fixtures/workflow/packs/drift-fixture.toml");

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
            crate::commands::workflow::registry::WorkflowSource::Repository
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

    /// Issue #542 review nit: `state.selection` persists the deterministic
    /// [`crate::commands::workflow::selection::Selection`] that chose a run's pack, so `zirv
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
        state.selection = Some(crate::commands::workflow::selection::Selection {
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

    /// A decisive `template_copy` refuses and a decisive `thin` warns; the
    /// same confidence one step below [`JEV_ARTIFACT_CONFIDENCE`], and a
    /// missing answer, both fall back to "pass" -- proves the `>=` edge, not
    /// just a comfortably-clear case.
    #[test]
    fn artifact_substance_action_decides_on_the_confidence_edge() {
        let (_, min_margin) = ARTIFACT_SUBSTANCE_DEFAULT_FLOOR;
        let refuse = choice_answer("template_copy", JEV_ARTIFACT_CONFIDENCE);
        assert_eq!(
            artifact_substance_action(Some(&refuse), JEV_ARTIFACT_CONFIDENCE, min_margin),
            "refuse"
        );
        let warn = choice_answer("thin", JEV_ARTIFACT_CONFIDENCE);
        assert_eq!(
            artifact_substance_action(Some(&warn), JEV_ARTIFACT_CONFIDENCE, min_margin),
            "warn"
        );
        let just_below = choice_answer("template_copy", JEV_ARTIFACT_CONFIDENCE - 0.01);
        assert_eq!(
            artifact_substance_action(Some(&just_below), JEV_ARTIFACT_CONFIDENCE, min_margin),
            "pass"
        );
        assert_eq!(
            artifact_substance_action(None, JEV_ARTIFACT_CONFIDENCE, min_margin),
            "pass"
        );
    }
}
