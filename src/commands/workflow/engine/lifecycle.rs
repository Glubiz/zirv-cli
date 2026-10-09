//! Workflow lifecycle transitions that are not step-by-step evidence
//! advances: approval, close, reclassify, recommended-disposition
//! application, and rendering the current-step context (issue #542-split).

use std::collections::BTreeSet;
use std::path::Path;

use crate::commands::ctx::CtxResult;
use crate::commands::ctx::state::{StateDir, now_secs};
use crate::commands::workflow::skill::{SkillRegistry, WorkflowPhase};

use super::cli::*;
use super::definitions::*;
use super::state::*;
use super::transition::*;
/// Mark compiler-emitted skill headers with a sentinel stripped from untrusted bodies, so repository text cannot forge provenance. (#557)
pub const SKILL_HEADER_SENTINEL: char = '\u{1}';

/// Strip the header sentinel from untrusted skill bodies to prevent forged provenance headers. (#557)
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

/// Move a just-started workflow from its first gate to running that step,
/// for a start nobody is present to approve. A first step with an External
/// effect keeps its gate.
pub fn waive_first_gate(
    state_dir: &StateDir,
    mut state: WorkflowState,
) -> CtxResult<WorkflowState> {
    if state.status != WorkflowStatus::AwaitingApproval || !state.completed_steps.is_empty() {
        return Ok(state);
    }
    let Some(step) = state.current() else {
        return Ok(state);
    };
    if step.effect == crate::commands::workflow::definition::EffectClass::External {
        return Ok(state);
    }
    state.current_step_approved = Some(step.id.clone());
    state.status = WorkflowStatus::Running;
    state.updated_at = now_secs();
    save(state_dir, &state, true)?;
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
        let accepted = pin_current_artifact(&mut state)?;
        // A re-approval without a new awaiting-approval span has no honest wait duration to report. (#699)
        let mut approval_wait_ms = None;
        if !state.completed_steps.contains(&completed.id) {
            approval_wait_ms = Some(record_step_duration_ms(&mut state, &completed.id));
            state.completed_steps.push(completed.id);
        }
        state.current_step += 1;
        reclassify_at_gate(state_dir, &mut state, jev_cfg.as_ref());
        sync_artifact_records(&mut state);
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
        let _ = crate::commands::workflow::outcomes::record_terminal(state_dir, &state);

        let mut event = crate::commands::workflow::telemetry::TelemetryEvent::new(
            crate::commands::workflow::telemetry::TelemetryKind::ArtifactAccepted,
        );
        event.workflow_id = Some(state.id.clone());
        event.phase = Some(completed.phase);
        event.intent = Some(state.classification.intent);
        event.complexity = Some(state.classification.complexity);
        event.risk = Some(state.classification.risk);
        event.work_domain = Some(state.classification.work_domain.domain);
        event.succeeded = Some(true);
        event.artifact_stage = Some(accepted.to_string());
        // Record the whole awaiting-approval span because state cannot separate drafting from operator review. (#699)
        event.approval_wait_ms = approval_wait_ms;
        let _ = crate::commands::workflow::telemetry::record(
            state_dir,
            &state.repo,
            &event,
            &crate::commands::workflow::telemetry::TelemetryConfig::for_repo(&state.repo),
        );
        // A chained approval step still needs an operator; clear attention only after Running or Completed. (#349)
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

    // Record gate-only approval against the still-current step id so status recomputation cannot immediately re-open the gate. (#542)
    let approved_phase = state.current().map(|step| step.phase);
    // Gate-only approval does not finish the step; record wait separately and reset its clock before execution time starts. (#699)
    let gate_wait_ms = phase_elapsed_ms(&state);
    if let Some(step) = state.current() {
        state.current_step_approved = Some(step.id.clone());
    }
    state.status = WorkflowStatus::Running;
    state.updated_at = now_secs();
    state.phase_started_at = state.updated_at;
    save(state_dir, &state, true)?;

    // Gate-only approval also emits acceptance telemetry, without an artifact stage. (#542)
    let mut event = crate::commands::workflow::telemetry::TelemetryEvent::new(
        crate::commands::workflow::telemetry::TelemetryKind::ArtifactAccepted,
    );
    event.workflow_id = Some(state.id.clone());
    event.phase = approved_phase;
    event.intent = Some(state.classification.intent);
    event.complexity = Some(state.classification.complexity);
    event.risk = Some(state.classification.risk);
    event.work_domain = Some(state.classification.work_domain.domain);
    event.succeeded = Some(true);
    // Record the same whole approval span as the artifact gate. (#699)
    event.approval_wait_ms = Some(gate_wait_ms);
    let _ = crate::commands::workflow::telemetry::record(
        state_dir,
        &state.repo,
        &event,
        &crate::commands::workflow::telemetry::TelemetryConfig::for_repo(&state.repo),
    );

    record_workflow_attention(
        crate::commands::ctx::attention::Attention::None,
        "step approved",
    );
    Ok(state)
}

/// Close only an in-flight workflow with no open finding or pending approval; clear the active pointer only if it names this workflow.
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
        .filter(|finding| {
            finding.disposition == crate::commands::workflow::review::FindingDisposition::Open
        })
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

/// Close after spawn failure only at the first gate, before any step or artifact was completed; later state requires ordinary close. (#537)
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

/// Close this workflow and clear the active pointer only when it names this workflow; record the Closed event once.
pub(super) fn finish_close(
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
    let _ = crate::commands::workflow::outcomes::record_terminal(state_dir, &state);

    let mut event = crate::commands::workflow::telemetry::TelemetryEvent::new(
        crate::commands::workflow::telemetry::TelemetryKind::Closed,
    );
    event.workflow_id = Some(state.id.clone());
    event.intent = Some(state.classification.intent);
    event.complexity = Some(state.classification.complexity);
    event.risk = Some(state.classification.risk);
    event.work_domain = Some(state.classification.work_domain.domain);
    let _ = crate::commands::workflow::telemetry::record(
        state_dir,
        &state.repo,
        &event,
        &crate::commands::workflow::telemetry::TelemetryConfig::for_repo(&state.repo),
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
    pub applied: Option<crate::commands::workflow::review::FindingDisposition>,
    /// A Critical or Major dismissal was withheld from bulk application.
    pub requires_explicit_disposition: bool,
}

/// Apply recommendations only to open findings; Critical/Major dismissals still require explicit disposition, and missing recommendations stay open.
pub fn apply_recommended_dispositions(
    state_dir: &StateDir,
    mut state: WorkflowState,
) -> CtxResult<(WorkflowState, Vec<AppliedDisposition>)> {
    let mut results = Vec::new();
    for finding in &mut state.review_findings {
        if finding.disposition != crate::commands::workflow::review::FindingDisposition::Open {
            continue;
        }
        let requires_explicit_disposition = finding.recommended_disposition
            == Some(crate::commands::workflow::review::FindingDisposition::Dismissed)
            && matches!(
                finding.severity,
                crate::commands::workflow::review::FindingSeverity::Critical
                    | crate::commands::workflow::review::FindingSeverity::Major
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
        crate::commands::workflow::telemetry::finding_counts(&state.review_findings);
    let mut event = crate::commands::workflow::telemetry::TelemetryEvent::new(
        crate::commands::workflow::telemetry::TelemetryKind::FindingUpdated,
    );
    event.workflow_id = Some(state.id.clone());
    event.phase = Some(WorkflowPhase::Review);
    event.intent = Some(state.classification.intent);
    event.complexity = Some(state.classification.complexity);
    event.risk = Some(state.classification.risk);
    event.work_domain = Some(state.classification.work_domain.domain);
    event.findings_total = findings_total;
    event.findings_meaningful = findings_meaningful;
    event.findings_dismissed = findings_dismissed;
    let _ = crate::commands::workflow::telemetry::record(
        state_dir,
        &state.repo,
        &event,
        &crate::commands::workflow::telemetry::TelemetryConfig::for_repo(&state.repo),
    );

    Ok((state, results))
}

/// Change the methodology overlay in place, preserving completed steps and accepted artifacts; profile changes do not reorder steps. (#255)
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
    let _ = crate::commands::workflow::outcomes::record_terminal(state_dir, &state);
    Ok(state)
}

/// A headless worker must not answer `brainstorm`'s clarifying questions or
/// write the intent artifact on the operator's behalf.
pub(super) const BRAINSTORM_HEADLESS_REFUSAL: &str = "This step needs an interactive operator. Do not answer the clarifying questions on their behalf or write the intent artifact; stop and report that the workflow is waiting for the operator.";

pub(super) fn refusal_for(skill_id: &str, headless: bool) -> Option<&'static str> {
    (headless && skill_id == "brainstorm").then_some(BRAINSTORM_HEADLESS_REFUSAL)
}

/// Only the exact value `"1"` means headless -- `ZIRV_CTX_HEADLESS=0`, an
/// empty string, or any other value must not trip the refusal. Split out of
/// the `std::env::var` call site so the value comparison is testable without
/// a real (racy) environment variable.
pub(super) fn is_headless_env(raw: Option<&str>) -> bool {
    raw == Some("1")
}

/// Cap context at the head and show how much was cut; header lines outrank later skill text. (#326)
pub(super) fn cap_workflow_context(rendered: String, max_bytes: usize) -> String {
    if rendered.len() <= max_bytes {
        return rendered;
    }
    let omitted = rendered.len() - max_bytes;
    let marker = format!(
        "\n[workflow context truncated -- {omitted} bytes omitted, cap \
         workflow.max_context_bytes={max_bytes}]\n"
    );
    // When the marker alone exceeds the cap, truncate it too so output never exceeds the configured ceiling.
    if marker.len() >= max_bytes {
        return crate::utils::truncate_bytes(marker, Some(max_bytes));
    }
    let keep = max_bytes - marker.len();
    let mut truncated = crate::utils::truncate_bytes(rendered, Some(keep));
    truncated.push_str(&marker);
    truncated
}

/// Gate native completion on fresh passing Test/Verify evidence from the workflow branch. Once the step needs evidence, read failures block completion. (#484, #599)
pub fn native_completion_gate(state_dir: &StateDir, repo: &Path) -> Option<String> {
    // Read live at every completion attempt, and the same predicate as advance_with_evidence's Test/Verify arm: a second definition of done could drift. An unreadable state dir or absent workflow means nothing to gate on. (#484)
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
        crate::commands::workflow::skill::WorkflowPhase::Test
            | crate::commands::workflow::skill::WorkflowPhase::Verify
    ) {
        return None;
    }
    let final_only = step.phase == crate::commands::workflow::skill::WorkflowPhase::Verify;
    let command = if final_only {
        "zirv verify"
    } else {
        "zirv test changed"
    };
    // Once Test/Verify requires fresh evidence, an evidence read error must fail closed rather than count as passing. (#599)
    let fresh = match crate::commands::workflow::verification::latest_is_fresh_and_passing(
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
        let record = state
            .artifacts
            .get(stage.key())
            .ok_or("current workflow artifact record is missing")?;
        // An unfilled artifact exposes its path and template in context without writing to the worktree or index.
        let path = workflow_artifact_path(state, stage)?;
        if path.exists() {
            rendered.push_str(&format!(
                "artifact: {} ({stage}; fill this committed work product, then wait for acceptance)\n",
                record.rel_path
            ));
        } else {
            rendered.push_str(&format!(
                "artifact: {} ({stage}; not yet created -- write it yourself with this starting \
                 template, then wait for acceptance)\n--- {stage} template ---\n{}\n--- end {stage} template ---\n",
                record.rel_path,
                stage.template(),
            ));
        }
    }
    append_accepted_artifacts(state, &mut rendered)?;
    if state.profile == WorkflowProfile::Frontend {
        let state_dir = StateDir::resolve(&|key| std::env::var(key).ok())?;
        let profile = crate::commands::workflow::frontend::ensure_profile(&state_dir, repo)?;
        rendered.push('\n');
        rendered.push_str(&crate::commands::workflow::frontend::render_profile(
            &profile,
        ));
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
    // Cap selected skills at the shared context source; unreadable config uses the built-in cap, never unbounded output. (#326)
    let max_context_bytes =
        crate::commands::ctx::config::CtxConfig::load(repo, &|key| std::env::var(key).ok())
            .map_or_else(
                |_| crate::commands::ctx::config::WorkflowConfig::default().max_context_bytes,
                |cfg| cfg.workflow.max_context_bytes,
            );
    Ok(Some(cap_workflow_context(rendered, max_context_bytes)))
}

/// Only the workflow bound to `session` is rendered; with no session, or none bound, the repo's active pointer (possibly another session's finished task) is never injected.
pub fn active_skill_context(repo: &Path, session: Option<&str>) -> CtxResult<Option<String>> {
    let Some(short) = session else {
        return Ok(None);
    };
    let state_dir = StateDir::resolve(&|key| std::env::var(key).ok())?;
    if crate::commands::ctx::sessions::workflow_id_for(&state_dir, short).is_none() {
        return Ok(None);
    }
    let Some(state) = load_active_for_session(&state_dir, repo, short)? else {
        return Ok(None);
    };
    match render_current_context(&state, repo, dirs::home_dir().as_deref()) {
        Ok(context) => {
            // The composed prompt carries this step, so the first prompt hook must not repeat it (#878).
            if context.is_some() {
                super::session_context::record_injected(&state_dir, short, &state);
            }
            Ok(context)
        }
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

pub(super) fn announce_degradation(repo: &Path, reason: &str) {
    let enabled =
        crate::commands::ctx::config::CtxConfig::load(repo, &|key| std::env::var(key).ok())
            .map_or(true, |cfg| cfg.chrome.events);
    crate::commands::ctx::announce::Announcer::new(enabled, false).emit(
        &crate::commands::ctx::announce::Event::WorkflowLayerSkipped {
            reason: reason.to_string(),
        },
    );
}

#[cfg(test)]
mod tests {
    use tempfile::tempdir;

    use crate::commands::ctx::state::{StateDir, create_private_dir_all, repo_slug, write_private};

    use crate::commands::workflow::classify::{Complexity, RiskBand};

    use crate::commands::workflow::skill::{SkillRegistry, WorkflowPhase};

    use super::*;

    use super::super::tests::{low_classification, review_finding, skip_leading_artifact_steps};
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

    #[test]
    fn apply_recommended_dispositions_requires_explicit_major_or_critical_dismissal() {
        use crate::commands::workflow::review::{
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

    /// Every frontend phase skill's injected context (header, frontend
    /// profile, resolved skill stack) must fit the default
    /// `workflow.max_context_bytes` without the truncation marker. The
    /// uncapped render is measured first so the byte count is reported.
    #[test]
    fn every_frontend_phase_skill_context_fits_the_default_workflow_cap() {
        let greenfield = tempdir().unwrap();
        // Existing-system branch at the scan's caps: 8 fonts of 96 bytes, 8
        // colours, and more evidence files than the path cap.
        let existing = tempdir().unwrap();
        let mut css = String::new();
        for index in 0..12 {
            css.push_str(&format!(
                "a{index} {{ font-family: '{index:02}{}'; color: #{index:08x}; }}\n",
                "f".repeat(94)
            ));
        }
        for index in 0..40 {
            std::fs::write(existing.path().join(format!("s{index}.css")), &css).unwrap();
        }
        let root = tempdir().unwrap();
        let _vars = crate::commands::ctx::testenv::VarGuard::set(&[
            (
                "ZIRV_CTX_STATE_DIR",
                Some(root.path().to_str().expect("utf-8 tempdir path")),
            ),
            ("ZIRV_CTX_WORKFLOW_MAX_CONTEXT_BYTES", Some("10000000")),
        ]);
        let default_cap = crate::commands::ctx::config::WorkflowConfig::default().max_context_bytes;
        let mut over = Vec::new();
        for (label, repo) in [("greenfield", &greenfield), ("existing", &existing)] {
            for skill in [
                "frontend-design",
                "frontend-plan",
                "frontend-implement",
                "frontend-debug",
                "frontend-test",
                "frontend-review",
                "frontend-verify",
            ] {
                let mut state = skip_leading_artifact_steps(WorkflowState::start(
                    repo.path().to_path_buf(),
                    "small feature".into(),
                    WorkflowKind::Feature,
                    None,
                    true,
                    low_classification(),
                ));
                state.profile = WorkflowProfile::Frontend;
                let index = state.current_step;
                state.steps[index].skill = skill.to_string();
                let uncapped = render_current_context(&state, repo.path(), None)
                    .unwrap()
                    .unwrap();
                let capped = cap_workflow_context(uncapped.clone(), default_cap);
                if label == "existing" {
                    assert!(
                        uncapped.contains("basis: ExistingSystem"),
                        "the seeded repo must take the observed-evidence branch"
                    );
                }
                eprintln!(
                    "workflow-context bytes {label} {skill}: {} / {default_cap}",
                    uncapped.len()
                );
                if capped.contains("workflow context truncated") {
                    over.push(format!("{label} {skill}={}", uncapped.len()));
                }
            }
        }
        assert!(
            over.is_empty(),
            "frontend contexts over the default {default_cap}-byte cap: {over:?}"
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
    fn the_simplify_step_does_not_inherit_execute_plan_context() {
        let repo = tempdir().unwrap();
        let mut classification = low_classification();
        classification.complexity = Complexity::Substantial;
        classification.risk = RiskBand::Medium;
        let mut state = WorkflowState::start(
            repo.path().to_path_buf(),
            "substantial feature".into(),
            WorkflowKind::Feature,
            None,
            true,
            classification,
        );
        state.current_step = state
            .steps
            .iter()
            .position(|step| step.id == "simplify")
            .expect("medium-risk feature carries a simplify step");
        state.status = WorkflowStatus::Running;
        let context = render_current_context(&state, repo.path(), None)
            .unwrap()
            .unwrap();
        assert!(context.contains("[skill simplify@1"), "got: {context}");
        assert!(!context.contains("[skill execute-plan@1"), "got: {context}");
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
            crate::commands::workflow::review::FindingDisposition::Open,
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

        let events = crate::commands::workflow::telemetry::list(&state_dir, repo.path()).unwrap();
        assert!(
            events.iter().any(|event| event.kind
                == crate::commands::workflow::telemetry::TelemetryKind::Closed
                && event.workflow_id.as_deref() == Some(workflow_id.as_str())),
            "{events:?}"
        );
    }

    /// Issue #757: a terminal transition appends exactly one metadata-only
    /// outcome row; a refused second close appends nothing.
    #[test]
    fn a_terminal_transition_appends_exactly_one_outcome_row() {
        let repo = tempdir().unwrap();
        let root = tempdir().unwrap();
        let state_dir = StateDir::from_root(root.path().to_path_buf());
        let state = WorkflowState::start(
            repo.path().to_path_buf(),
            "secret task text".into(),
            WorkflowKind::Feature,
            None,
            true,
            low_classification(),
        );
        save(&state_dir, &state, true).unwrap();
        assert!(crate::commands::workflow::outcomes::read_all(&state_dir).is_empty());

        let closed = close(&state_dir, state, Some("abandoned".into())).unwrap();
        assert!(close(&state_dir, closed.clone(), None).is_err());

        let dir = state_dir
            .logs()
            .join(crate::commands::workflow::outcomes::OUTCOMES_DIR);
        let lines: Vec<String> = std::fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .flat_map(|entry| {
                std::fs::read_to_string(entry.path())
                    .unwrap()
                    .lines()
                    .map(str::to_string)
                    .collect::<Vec<_>>()
            })
            .collect();
        assert_eq!(lines.len(), 1, "{lines:?}");
        assert!(!lines[0].contains("secret task text"), "{}", lines[0]);
        let row: crate::commands::workflow::outcomes::OutcomeRow =
            serde_json::from_str(&lines[0]).unwrap();
        assert_eq!(row.workflow_id, closed.id);
        assert_eq!(row.pack, "feature");
        assert_eq!(row.terminal, WorkflowStatus::Closed);
        assert_eq!(row.complexity, closed.classification.complexity);
        assert_eq!(row.review_rounds, 0);
        assert_eq!(row.verification_first_attempt, None);
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
}
