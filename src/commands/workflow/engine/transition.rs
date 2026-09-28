//! Advancing a workflow through evidence (`advance_with_evidence`), the Jev
//! advisory gate, and cumulative usage/telemetry bookkeeping
//! (issue #542-split).

use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use clap::ValueEnum;
use serde::Serialize;

use crate::commands::ctx::CtxResult;
use crate::commands::ctx::jev::{self, AnswerValue, Question};
use crate::commands::ctx::state::{StateDir, now_secs};
use crate::commands::workflow::classify::{self, Classification, Complexity, RiskBand, WorkDomain};
use crate::commands::workflow::deploy::DeployTier;
use crate::commands::workflow::skill::WorkflowPhase;

use super::cli::*;
use super::definitions::*;
use super::state::*;
const MAX_JEV_GATE_TASK_BYTES: usize = 4 * 1024;

const MAX_JEV_GATE_PATHS: usize = 200;

/// Minimum sensitive-surface probability from the 2026-09-18 probe.
const JEV_SENSITIVE_PROBABILITY: f64 = 0.7;

/// Minimum frontend choice confidence from the 2026-09-18 probe.
const JEV_FRONTEND_CONFIDENCE: f32 = 0.9;

/// Minimum additive-tag probability from the 2026-09-18 probe.
const JEV_TAG_PROBABILITY: f64 = 0.7;

/// [`apply_jev_gate_advice`]'s own production advise-site LABEL.
pub(crate) const GATE_RECLASS_LABEL: &str = "workflow-gate-reclassification";

/// [`apply_jev_gate_advice`]'s own `sensitive_surface` and per-tag questions'
/// default `(min_confidence, min_margin)` `decisive()` floor -- named (issue:
/// `zirv ctx jev probe`) so a later retune targets exactly this constant. Not
/// routed through `jev::floor`/`[jev.floors]` today: this stays the same
/// fixed pair production has always used.
pub(crate) const GATE_RECLASS_NOUL_DEFAULT_FLOOR: (f32, f32) = (0.0, jev::DEFAULT_MIN_MARGIN);

/// [`apply_jev_gate_advice`]'s own `work_domain` question's default
/// `(min_confidence, min_margin)` `decisive()` floor.
pub(crate) const GATE_RECLASS_WORK_DOMAIN_DEFAULT_FLOOR: (f32, f32) =
    (JEV_FRONTEND_CONFIDENCE, jev::DEFAULT_MIN_MARGIN);

const MAX_PHASE_TRANSCRIPT_BYTES: u64 = 16 * 1024 * 1024;

const USAGE_SNAPSHOT_TAIL_BYTES: u64 = 256 * 1024;

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

pub(super) fn session_identity() -> Option<(String, String)> {
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
pub(super) fn record_workflow_attention(
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

/// Dash refresh PR1: best-effort companion to [`record_workflow_attention`]
/// -- same env-only lookup (deliberately just `SESSION_ENV`, not the
/// stricter [`session_identity`], for the identical reason: a workflow start
/// with a malformed or missing adapter-name env must not lose its binding),
/// same "quietly do nothing" fallback for a headless caller or a test.
pub(super) fn bind_started_workflow_to_calling_session(state_dir: &StateDir, workflow_id: &str) {
    let Ok(session_id) = std::env::var(crate::commands::ctx::adapters::SESSION_ENV) else {
        return;
    };
    if session_id.is_empty() {
        return;
    }
    let short = crate::commands::ctx::sessions::short_id(&session_id);
    crate::commands::ctx::sessions::bind_workflow_id(state_dir, &short, workflow_id);
}

pub(super) fn adapter_by_name(
    name: &str,
) -> Option<Box<dyn crate::commands::ctx::adapters::AgentAdapter>> {
    crate::commands::ctx::adapters::ADAPTERS
        .iter()
        .find(|(adapter_name, _)| *adapter_name == name)
        .map(|(_, constructor)| constructor(None))
}

pub(super) fn transcript_path(repo: &Path, session_id: &str, adapter: &str) -> Option<PathBuf> {
    let adapter = adapter_by_name(adapter)?;
    Some(
        adapter.transcript_path(&crate::commands::ctx::event::SessionRef {
            id: crate::commands::ctx::event::SessionId::parse(session_id),
            cwd: repo.to_path_buf(),
        }),
    )
}

pub(super) fn read_transcript_range(path: &Path, start: u64, end: u64) -> Option<String> {
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

pub(super) fn cumulative_snapshot(
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

pub(super) fn usage_checkpoint(repo: &Path) -> Option<UsageCheckpoint> {
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

pub(super) fn usage_since(
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
pub(super) fn sidechain_usage_since(
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

pub(super) fn enrich_transition_evidence(
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
pub(super) struct JevGateState {
    task: String,
    changed_paths: Vec<String>,
    current_complexity: Complexity,
    current_risk: RiskBand,
    current_domain: WorkDomain,
}

/// [`apply_jev_gate_advice`]'s own full question set, factored out so `zirv
/// ctx jev probe` can ask the exact same questions from a fixture's own
/// state -- static, no per-call inputs.
pub(crate) fn gate_reclass_questions() -> Vec<Question> {
    vec![
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
    ]
}

/// [`apply_jev_gate_advice`]'s own `sensitive_surface` decision: `"raise"`
/// for a decisive noul at or above [`JEV_SENSITIVE_PROBABILITY`], `"none"`
/// otherwise (missing, indecisive, or below the threshold) -- the gate's own
/// fallback outcome (risk left unescalated by this question). Shared with
/// `zirv ctx jev probe`, which reports exactly this outcome.
pub(crate) fn gate_sensitive_surface_action(
    answer: Option<&jev::Answer>,
    min_confidence: f32,
    min_margin: f32,
) -> &'static str {
    let Some(answer) = answer else {
        return "none";
    };
    if !answer.decisive(min_confidence, min_margin) {
        return "none";
    }
    match answer.as_noul() {
        Some(probability) if probability >= JEV_SENSITIVE_PROBABILITY => "raise",
        _ => "none",
    }
}

/// [`apply_jev_gate_advice`]'s own `work_domain` decision: `"frontend"` for a
/// decisive `frontend` choice at or above [`JEV_FRONTEND_CONFIDENCE`],
/// `"none"` otherwise (missing, indecisive, or any other choice) -- the
/// gate's own fallback outcome (domain left unreclassified by this
/// question). Shared with `zirv ctx jev probe`, which reports exactly this
/// outcome.
pub(crate) fn gate_work_domain_action(
    answer: Option<&jev::Answer>,
    min_confidence: f32,
    min_margin: f32,
) -> &'static str {
    let Some(answer) = answer else {
        return "none";
    };
    if !answer.decisive(min_confidence, min_margin) {
        return "none";
    }
    match &answer.value {
        AnswerValue::Choice(choice) if choice == "frontend" => "frontend",
        _ => "none",
    }
}

/// [`apply_jev_gate_advice`]'s own per-tag decision (shared by `security`,
/// `data`, `docs_only`, `devops`, `architecture`): `"tag"` for a decisive
/// noul at or above [`JEV_TAG_PROBABILITY`], `"none"` otherwise (missing,
/// indecisive, or below the threshold) -- the gate's own fallback outcome
/// (no tag added for this question). Shared with `zirv ctx jev probe`, which
/// reports exactly this outcome per tag id.
pub(crate) fn gate_tag_action(
    answer: Option<&jev::Answer>,
    min_confidence: f32,
    min_margin: f32,
) -> &'static str {
    let Some(answer) = answer else {
        return "none";
    };
    if !answer.decisive(min_confidence, min_margin) {
        return "none";
    }
    match answer.as_noul() {
        Some(probability) if probability >= JEV_TAG_PROBABILITY => "tag",
        _ => "none",
    }
}

pub(super) fn apply_jev_gate_advice(
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
    let questions = gate_reclass_questions();
    let Some(answers) = jev::advise(
        cfg,
        state_dir,
        GATE_RECLASS_LABEL,
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

    let (sensitive_min_confidence, sensitive_min_margin) = GATE_RECLASS_NOUL_DEFAULT_FLOOR;
    if gate_sensitive_surface_action(
        answers.get("sensitive_surface"),
        sensitive_min_confidence,
        sensitive_min_margin,
    ) == "raise"
    {
        measured.risk = measured.risk.max(RiskBand::High);
        measured.risk_score = measured.risk_score.max(45);
    }
    let (domain_min_confidence, domain_min_margin) = GATE_RECLASS_WORK_DOMAIN_DEFAULT_FLOOR;
    if measured.work_domain.domain == WorkDomain::General
        && gate_work_domain_action(
            answers.get("work_domain"),
            domain_min_confidence,
            domain_min_margin,
        ) == "frontend"
    {
        measured.work_domain.domain = WorkDomain::Frontend;
        measured.work_domain.score = measured.work_domain.score.max(90);
    }
    let (tag_min_confidence, tag_min_margin) = GATE_RECLASS_NOUL_DEFAULT_FLOOR;
    for (question, tag) in [
        ("security", "security"),
        ("data", "data"),
        ("docs_only", "docs-only"),
        ("devops", "devops"),
        ("architecture", "architecture"),
    ] {
        if gate_tag_action(answers.get(question), tag_min_confidence, tag_min_margin) == "tag"
            && !state.jev_tags.iter().any(|existing| existing == tag)
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
pub(super) fn reclassify_at_gate(
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
pub(super) fn rematerialize_after_risk_increase(state: &mut WorkflowState) {
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

pub(super) fn apply_effective_deploy_tier(state: &mut WorkflowState, effective: DeployTier) {
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

pub(super) fn refresh_deploy_tier(state: &mut WorkflowState) -> CtxResult<()> {
    let effective = crate::commands::workflow::deploy::effective_tier(&state.repo)?;
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
                && !crate::commands::workflow::frontend_detector::latest_is_fresh_and_passing(
                    state_dir,
                    &frontend_root,
                    matches!(current.phase, WorkflowPhase::Review | WorkflowPhase::Verify),
                )?
            {
                let report = crate::commands::workflow::frontend_detector::detect_for_workflow(
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
                && !crate::commands::workflow::frontend_render::latest_visual_is_fresh_and_passing(
                    state_dir,
                    &frontend_root,
                )?
            {
                let render =
                    crate::commands::workflow::frontend_render::render(state_dir, &frontend_root)?;
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
                let review = crate::commands::workflow::frontend_render::review(
                    state_dir,
                    &frontend_root,
                    &crate::commands::workflow::frontend_render::VisualReviewArgs {
                        repo: Some(frontend_root.to_path_buf()),
                        agent: None,
                        model: None,
                        runtime: crate::commands::ctx::runtime::RuntimeKind::Harness
                            .as_str()
                            .to_string(),
                        json: false,
                    },
                )?;
                if review.verdict != crate::commands::workflow::frontend_render::VisualVerdict::Pass
                {
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
                let gate =
                    crate::commands::workflow::deploy::production_gate_satisfied(state_dir, &state);
                let mut event = crate::commands::workflow::telemetry::TelemetryEvent::new(
                    crate::commands::workflow::telemetry::TelemetryKind::DeployGateEvaluated,
                );
                event.workflow_id = Some(state.id.clone());
                event.phase = Some(current.phase);
                event.intent = Some(state.classification.intent);
                event.complexity = Some(state.classification.complexity);
                event.risk = Some(state.classification.risk);
                event.work_domain = Some(state.classification.work_domain.domain);
                event.deploy_tier = Some(state.deploy_tier.to_string());
                event.succeeded = Some(gate.is_ok());
                let _ = crate::commands::workflow::telemetry::record(
                    state_dir,
                    &state.repo,
                    &event,
                    &crate::commands::workflow::telemetry::TelemetryConfig::for_repo(&state.repo),
                );
                gate?;
            }
            if current.phase == WorkflowPhase::Review {
                if state.review_findings.iter().any(|finding| {
                    finding.disposition
                        == crate::commands::workflow::review::FindingDisposition::Open
                }) {
                    return Err(
                        "review findings must have a final disposition before the review step can pass"
                            .into(),
                    );
                }
                let required =
                    crate::commands::workflow::review::required_independent_reviews_for(&state);
                if required > 0 {
                    let remedy = format!(
                        "run `zirv workflow review run --agent <agent> {id}`, or record a \
                         completed native-subagent review with `zirv workflow review record \
                         {id} --model <model>`",
                        id = state.id,
                    );
                    if state.review_evidence.is_empty() {
                        return Err(format!(
                            "review step requires {required} fresh independent review run(s); found 0; {remedy}"
                        )
                        .into());
                    }
                    let fingerprint =
                        crate::commands::workflow::verification::change_fingerprint(&state.repo)?;
                    let completed = state
                        .review_evidence
                        .iter()
                        .filter(|evidence| evidence.change_fingerprint == fingerprint)
                        .count();
                    if completed < required {
                        return Err(format!(
                            "review step requires {required} fresh independent review run(s); found {completed}; {remedy}"
                        )
                        .into());
                    }
                }
            }
            if matches!(current.phase, WorkflowPhase::Test | WorkflowPhase::Verify) {
                let final_only = current.phase == WorkflowPhase::Verify;
                if !crate::commands::workflow::verification::latest_is_fresh_and_passing(
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
                    let announcement = crate::commands::workflow::verification::gate_announcement(
                        state_dir,
                        &state.repo,
                        final_only,
                    );
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
        crate::commands::workflow::telemetry::finding_counts(&state.review_findings);
    let mut event = crate::commands::workflow::telemetry::TelemetryEvent::new(match outcome {
        StepOutcome::Success => crate::commands::workflow::telemetry::TelemetryKind::PhaseCompleted,
        StepOutcome::Failure => crate::commands::workflow::telemetry::TelemetryKind::PhaseFailed,
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
            let report =
                crate::commands::workflow::verification::load_latest(state_dir, &state.repo)
                    .ok()
                    .flatten();
            crate::commands::workflow::telemetry::classify_fix_round_cause(
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
    let _ = crate::commands::workflow::telemetry::record(
        state_dir,
        &state.repo,
        &event,
        &crate::commands::workflow::telemetry::TelemetryConfig::for_repo(&state.repo),
    );
    if state.status == WorkflowStatus::Completed {
        let mut completed = crate::commands::workflow::telemetry::TelemetryEvent::new(
            crate::commands::workflow::telemetry::TelemetryKind::WorkflowCompleted,
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
        let _ = crate::commands::workflow::telemetry::record(
            state_dir,
            &state.repo,
            &completed,
            &crate::commands::workflow::telemetry::TelemetryConfig::for_repo(&state.repo),
        );
    }
    // Issue #757: `advance` only runs from `Running`, so a terminal status
    // here is always a fresh transition -- exactly one outcome row.
    let _ = crate::commands::workflow::outcomes::record_terminal(state_dir, &state);
    if outcome == StepOutcome::Success {
        try_auto_spawn(state_dir, &state);
    }
    Ok(state)
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::io::Write;
    use std::path::PathBuf;

    use tempfile::tempdir;

    use crate::commands::ctx::state::{StateDir, now_secs};

    use crate::commands::workflow::classify::{
        self, Classification, Complexity, RiskBand, WorkDomain,
    };
    use crate::commands::workflow::deploy::DeployTier;
    use crate::commands::workflow::skill::WorkflowPhase;

    use super::*;

    use super::super::lifecycle::*;

    use super::super::tests::{
        choice_answer, jev_gate_config, low_classification, skip_leading_artifact_steps,
    };
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
        let fingerprint =
            crate::commands::workflow::verification::change_fingerprint(repo.path()).unwrap();
        let evidence_report = crate::commands::workflow::verification::VerificationReport {
            schema_version: crate::commands::workflow::verification::VERIFY_REPORT_SCHEMA_VERSION,
            id: "seeded".into(),
            mode: crate::commands::workflow::verification::VerificationMode::Changed,
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
        crate::commands::workflow::verification::save_report(&state_dir, &evidence_report).unwrap();

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
        let fingerprint =
            crate::commands::workflow::verification::change_fingerprint(&worktree_path).unwrap();
        let evidence_report = crate::commands::workflow::verification::VerificationReport {
            schema_version: crate::commands::workflow::verification::VERIFY_REPORT_SCHEMA_VERSION,
            id: "worktree-evidence".into(),
            mode: crate::commands::workflow::verification::VerificationMode::Changed,
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
        crate::commands::workflow::verification::save_report(&state_dir, &evidence_report).unwrap();

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
                crate::commands::workflow::verification::change_fingerprint(&worktree_path)
                    .unwrap();
            let report = crate::commands::workflow::verification::VerificationReport {
                schema_version:
                    crate::commands::workflow::verification::VERIFY_REPORT_SCHEMA_VERSION,
                id: "worktree-evidence".into(),
                mode: crate::commands::workflow::verification::VerificationMode::Changed,
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
                checks: vec![crate::commands::workflow::verification::CheckResult {
                    id: "unit".into(),
                    kind: crate::commands::workflow::verification::CheckKind::Unit,
                    command: "true".into(),
                    source:
                        crate::commands::workflow::verification::CheckSource::DiscoveredToolchain,
                    status: crate::commands::workflow::verification::CheckStatus::Passed,
                    exit_code: Some(0),
                    duration_ms: 1,
                    failure_output: None,
                    failure_test_names: Vec::new(),
                    inconclusive_reason: None,
                }],
            };
            crate::commands::workflow::verification::save_report(&state_dir, &report).unwrap();
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
        let profile =
            crate::commands::workflow::frontend::ensure_profile(&state_dir, repo.path()).unwrap();
        let detector = crate::commands::workflow::frontend_detector::DetectorReport {
            schema_version:
                crate::commands::workflow::frontend_detector::DETECTOR_REPORT_SCHEMA_VERSION,
            id: uuid::Uuid::new_v4().to_string(),
            repo: repo.path().canonicalize().unwrap(),
            change_fingerprint: crate::commands::workflow::verification::change_fingerprint(
                repo.path(),
            )
            .unwrap(),
            profile_fingerprint: profile.source_fingerprint,
            scope: crate::commands::workflow::frontend_detector::DetectorScope::Changed,
            generated_at: now_secs(),
            analyzed_files: vec![PathBuf::from("App.tsx")],
            analyzed_bytes: 64,
            truncated: false,
            findings: Vec::new(),
            waivers_loaded: 0,
            waivers_rejected: 0,
            not_applicable: false,
        };
        crate::commands::workflow::frontend_detector::save_report(&state_dir, &detector).unwrap();
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
            crate::commands::workflow::frontend::ensure_profile(&state_dir, target_repo.path())
                .unwrap();
        let detector = crate::commands::workflow::frontend_detector::DetectorReport {
            schema_version:
                crate::commands::workflow::frontend_detector::DETECTOR_REPORT_SCHEMA_VERSION,
            id: uuid::Uuid::new_v4().to_string(),
            repo: target_repo.path().canonicalize().unwrap(),
            change_fingerprint: crate::commands::workflow::verification::change_fingerprint(
                target_repo.path(),
            )
            .unwrap(),
            profile_fingerprint: profile.source_fingerprint,
            scope: crate::commands::workflow::frontend_detector::DetectorScope::Changed,
            generated_at: now_secs(),
            analyzed_files: vec![PathBuf::from("App.tsx")],
            analyzed_bytes: 64,
            truncated: false,
            findings: Vec::new(),
            waivers_loaded: 0,
            waivers_rejected: 0,
            not_applicable: false,
        };
        crate::commands::workflow::frontend_detector::save_report(&state_dir, &detector).unwrap();

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
    fn gate_freeform_state_cannot_raise_risk_through_jev() {
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
        assert!(
            request
                .recv_timeout(std::time::Duration::from_millis(100))
                .is_err()
        );
        assert_eq!(measured.risk, RiskBand::Medium);
        assert!(!state_dir.root().join("jev-decisions.jsonl").exists());
    }

    #[test]
    fn gate_freeform_state_cannot_set_frontend_domain_through_jev() {
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
        assert!(
            request
                .recv_timeout(std::time::Duration::from_millis(100))
                .is_err()
        );
        assert_eq!(measured.work_domain.domain, WorkDomain::General);
    }

    #[test]
    fn gate_freeform_state_preserves_an_unset_frontend_domain() {
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
        assert!(
            request
                .recv_timeout(std::time::Duration::from_millis(100))
                .is_err()
        );

        assert_ne!(measured.work_domain.domain, WorkDomain::Frontend);
    }

    #[test]
    fn gate_freeform_state_preserves_existing_high_risk_and_frontend() {
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
        assert!(
            request
                .recv_timeout(std::time::Duration::from_millis(100))
                .is_err()
        );

        assert_eq!(measured.risk, RiskBand::High);
        assert_eq!(measured.work_domain.domain, WorkDomain::Frontend);
        assert!(state.jev_tags.is_empty());
    }

    #[test]
    fn gate_freeform_state_cannot_add_jev_tags() {
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
        assert!(
            request
                .recv_timeout(std::time::Duration::from_millis(100))
                .is_err()
        );
        assert!(state.jev_tags.is_empty());
        let mut output = Vec::new();
        write_state(&mut output, &state, false).unwrap();
        let output = String::from_utf8(output).unwrap();
        assert!(!output.contains("jev tags:"), "{output}");
    }

    #[test]
    fn gate_freeform_state_matches_gate_off() {
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
        assert!(
            request
                .recv_timeout(std::time::Duration::from_millis(100))
                .is_err()
        );

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
            .push(crate::commands::workflow::review::ReviewFinding {
                id: "finding-1".into(),
                severity: crate::commands::workflow::review::FindingSeverity::Major,
                summary: "concrete defect".into(),
                path: None,
                line: None,
                disposition: crate::commands::workflow::review::FindingDisposition::Open,
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

    /// Issue #685: a same-harness orchestrator seat is refused from `review
    /// run --agent <its own harness>` and must record a native-subagent
    /// review instead -- the missing-run error must name that remedy, not
    /// only `review run`.
    #[test]
    fn missing_review_evidence_error_names_the_review_record_command() {
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
        let id = state.id.clone();
        let error = advance_with_evidence(&state_dir, state, StepOutcome::Success, None, false)
            .unwrap_err()
            .to_string();
        assert!(
            error.contains(&format!("zirv workflow review record {id} --model <model>")),
            "the missing-run error must name the native-subagent record remedy: {error}"
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

        let fingerprint =
            crate::commands::workflow::verification::change_fingerprint(repo.path()).unwrap();
        let evidence_report = crate::commands::workflow::verification::VerificationReport {
            schema_version: crate::commands::workflow::verification::VERIFY_REPORT_SCHEMA_VERSION,
            id: "seeded".into(),
            mode: crate::commands::workflow::verification::VerificationMode::Changed,
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
        crate::commands::workflow::verification::save_report(&state_dir, &evidence_report).unwrap();

        assert!(
            native_completion_gate(&state_dir, repo.path()).is_none(),
            "the gate must open once fresh passing evidence exists"
        );
        let advanced = advance_with_evidence(&state_dir, state, StepOutcome::Success, None, false)
            .expect("advance must also succeed once fresh passing evidence exists");
        assert_eq!(advanced.current().unwrap().phase, WorkflowPhase::Verify);
    }

    fn noul_answer(probability: f64) -> jev::Answer {
        jev::Answer {
            value: jev::AnswerValue::Noul(probability),
            confidence: probability as f32,
            probabilities: BTreeMap::new(),
        }
    }

    /// A decisive noul at or above [`JEV_SENSITIVE_PROBABILITY`] raises; the
    /// same answer one step below the value threshold, and a missing
    /// answer, both fall back to "none".
    #[test]
    fn gate_sensitive_surface_action_decides_on_the_probability_edge() {
        let (min_confidence, min_margin) = GATE_RECLASS_NOUL_DEFAULT_FLOOR;
        let at_floor = noul_answer(JEV_SENSITIVE_PROBABILITY);
        assert_eq!(
            gate_sensitive_surface_action(Some(&at_floor), min_confidence, min_margin),
            "raise"
        );
        let just_below = noul_answer(JEV_SENSITIVE_PROBABILITY - 0.01);
        assert_eq!(
            gate_sensitive_surface_action(Some(&just_below), min_confidence, min_margin),
            "none"
        );
        assert_eq!(
            gate_sensitive_surface_action(None, min_confidence, min_margin),
            "none"
        );
    }

    /// A decisive `frontend` choice at or above [`JEV_FRONTEND_CONFIDENCE`]
    /// reclassifies; the same confidence one step below the threshold, and a
    /// missing answer, both fall back to "none".
    #[test]
    fn gate_work_domain_action_decides_on_the_confidence_edge() {
        let (_, min_margin) = GATE_RECLASS_WORK_DOMAIN_DEFAULT_FLOOR;
        let at_floor = choice_answer("frontend", JEV_FRONTEND_CONFIDENCE);
        assert_eq!(
            gate_work_domain_action(Some(&at_floor), JEV_FRONTEND_CONFIDENCE, min_margin),
            "frontend"
        );
        let just_below = choice_answer("frontend", JEV_FRONTEND_CONFIDENCE - 0.01);
        assert_eq!(
            gate_work_domain_action(Some(&just_below), JEV_FRONTEND_CONFIDENCE, min_margin),
            "none"
        );
        assert_eq!(
            gate_work_domain_action(None, JEV_FRONTEND_CONFIDENCE, min_margin),
            "none"
        );
    }

    /// A decisive noul at or above [`JEV_TAG_PROBABILITY`] tags; the same
    /// answer one step below the value threshold, and a missing answer, both
    /// fall back to "none".
    #[test]
    fn gate_tag_action_decides_on_the_probability_edge() {
        let (min_confidence, min_margin) = GATE_RECLASS_NOUL_DEFAULT_FLOOR;
        let at_floor = noul_answer(JEV_TAG_PROBABILITY);
        assert_eq!(
            gate_tag_action(Some(&at_floor), min_confidence, min_margin),
            "tag"
        );
        let just_below = noul_answer(JEV_TAG_PROBABILITY - 0.01);
        assert_eq!(
            gate_tag_action(Some(&just_below), min_confidence, min_margin),
            "none"
        );
        assert_eq!(gate_tag_action(None, min_confidence, min_margin), "none");
    }
}
