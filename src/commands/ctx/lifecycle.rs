//! Runtime-neutral session lifecycle decisions (issue #478, roadmap N09).
//!
//! Before this module every lifecycle decision zirv makes about a running
//! session -- may this tool run, should this tool result be replaced, what
//! rides along with a prompt, may this session stop, what does a notification
//! mean, is fresh verification owed -- was reachable only by feeding a
//! harness-shaped JSON payload to `hook.rs`. A native session has no harness
//! and therefore no hook process to shell out to, so those decisions have to
//! exist somewhere both paths can call.
//!
//! This module is that place. It owns the DECISIONS and nothing else:
//!
//! - `hook.rs` stays the payload translator it always was. It parses claude's
//!   (or a projected agent's) JSON, builds the neutral intents below, calls
//!   in here, and renders the answer back into the harness's own envelope.
//!   Every existing hook behaviour and every existing hook test is unchanged
//!   by construction -- the harness-shaped types, the envelopes, the state
//!   writes and the decision logging all stay in `hook.rs`.
//! - `runtime::native` calls the same functions directly, with intents built
//!   from its own journal/tool records. No hook process, no harness binary,
//!   no PATH probe -- which is what makes acceptance criterion (f) of issue
//!   #478 provable.
//!
//! Everything here is pure: no filesystem, clock, environment or network
//! access of its own. Where a decision genuinely needs the environment (the
//! harness-home exemption in [`orchestrator_write_target`]) the lookup is
//! passed in as `EnvLookup`, exactly as the hook path already did it, so both
//! callers and every test stay deterministic.

use std::path::{Path, PathBuf};

use super::config::{CtxConfig, EnvLookup, OrchestratorWrites};

// -- before-tool ---------------------------------------------------------

/// Tool names that write repository files, in the harness's own spelling.
/// The native tool registry's equivalents (`file_write`, `apply_patch`) are
/// classified by [`ToolIntent::write_target`] instead of by name, because a
/// native call already carries its resolved path.
pub const FILE_MODIFICATION_TOOLS: [&str; 4] = ["Edit", "Write", "MultiEdit", "NotebookEdit"];

/// Subagent-dispatch tool names, in the harness's own spelling.
pub const SUBAGENT_TOOLS: [&str; 2] = ["Agent", "Task"];

/// Subagent types that pin no model of their own, so an omitted `model`
/// parameter means "inherit the caller's". Matched exactly and
/// case-sensitively: these are literal values of the dispatch's own
/// `subagent_type` parameter, not free text.
pub const GENERIC_SUBAGENT_TYPES: [&str; 5] =
    ["fork", "claude", "general-purpose", "Explore", "Plan"];

/// Model-name fragments that mark a seat too expensive to inherit silently.
/// Matched case-insensitively as substrings, so a vendor-qualified id
/// (`us.anthropic.mythos-...`) or a suffixed one (`fable[1m]`) still lands.
pub const EXPENSIVE_TIERS: [&str; 2] = ["fable", "mythos"];

/// Whether `model` names an expensive tier.
pub fn names_expensive_tier(model: &str) -> bool {
    let model = model.to_ascii_lowercase();
    EXPENSIVE_TIERS.iter().any(|tier| model.contains(tier))
}

/// A subagent dispatch, as either path describes one: the task text, the
/// agent type asked for, and the model override (all possibly empty).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SubagentIntent {
    pub prompt: String,
    pub subagent_type: String,
    pub model: String,
}

/// One tool call about to run, described without reference to any harness's
/// payload shape. `write_target` is the already-resolved absolute write path
/// (the hook path resolves it from `file_path`/`notebook_path` against the
/// call's cwd; the native path takes it straight from the parsed tool
/// arguments); `delegated` is true when the call comes from a subagent rather
/// than the seat itself.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ToolIntent {
    pub tool: String,
    pub write_target: Option<PathBuf>,
    pub subagent: Option<SubagentIntent>,
    pub delegated: bool,
}

/// What the before-tool service decided. `Deny` blocks with a reason the
/// model reads; `Advise` allows and rides a non-blocking note along; `Allow`
/// says nothing at all.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolAdmission {
    Allow,
    Advise(String),
    Deny(String),
}

impl ToolAdmission {
    /// The audit-log label for this decision, matching
    /// `OrchestratorWrites::label`'s three postures one-for-one.
    pub fn log_label(&self) -> &'static str {
        match self {
            ToolAdmission::Deny(_) => "denied",
            ToolAdmission::Advise(_) => "advised",
            ToolAdmission::Allow => "allowed",
        }
    }
}

/// What the model is told when a dispatch is refused. The reason is the only
/// thing it sees, so it has to carry the whole remedy: naming the seat, the
/// cheaper models that are accepted, and the one option (a fork) that no
/// model parameter can rescue.
pub fn seat_deny_reason(seat: &str) -> String {
    format!(
        "zirv guard: this seat runs {seat}; re-dispatch with an explicit cheaper model \
         parameter (haiku for mechanical work, sonnet for standard work, opus for hard \
         work), or use an agent type that pins its own model. Forks are not allowed from \
         this seat: a fork always inherits the seat model and ignores a model override."
    )
}

/// The expensive-seat subagent guard, pure: `Some(reason)` denies, `None`
/// allows.
///
/// `seat` is the seat model this session runs on, absent for any session zirv
/// did not launch as an expensive orchestrator seat. Every gate below is a
/// reason to allow, so an unrecognised tool, an unset seat, a cheap seat, an
/// intent with no prompt (schema drift, not a dispatch), or an intent this
/// function does not understand at all fall through to allow. That is
/// deliberate: this decision runs in front of every tool call in the session,
/// and the cost of a wrong deny is far higher than the cost of a missed one.
pub fn subagent_admission(seat: Option<&str>, intent: &ToolIntent) -> Option<String> {
    let seat = seat?;
    if !names_expensive_tier(seat) {
        return None;
    }
    if !SUBAGENT_TOOLS.contains(&intent.tool.as_str()) {
        return None;
    }
    let subagent = intent.subagent.as_ref()?;
    if subagent.prompt.trim().is_empty() {
        return None;
    }

    let subagent_type = subagent.subagent_type.trim();
    let model = subagent.model.trim();

    // A fork inherits the seat model by construction and ignores `model`
    // outright, so naming a cheap one buys nothing and must not read as
    // though it did.
    let denied = if subagent_type == "fork" {
        true
    } else if !model.is_empty() {
        // An explicit model is honored, unless it asks for the seat tier
        // again by name, which is the exact spend being guarded.
        names_expensive_tier(model)
    } else {
        // No model named: only a subagent type that pins its own inherits.
        subagent_type.is_empty() || GENERIC_SUBAGENT_TYPES.contains(&subagent_type)
    };

    denied.then(|| seat_deny_reason(seat))
}

/// Resolves `path` lexically: `.` components drop, `..` pops the previous
/// component (or is kept literally once there is nothing left to pop, so a
/// relative path that climbs above its own root still reads as "outside").
/// Deliberately NOT `std::fs::canonicalize`: a write target may not exist
/// yet, and this must stay a pure path computation, no filesystem access.
pub fn normalize_lexically(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir if out.pop() => {}
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// The harness's operator-owned configuration directory. An explicit,
/// non-empty `CLAUDE_CONFIG_DIR` wins; otherwise the default beneath `HOME`
/// (or Windows' `USERPROFILE`) applies.
pub fn harness_home(env: EnvLookup<'_>) -> Option<PathBuf> {
    env("CLAUDE_CONFIG_DIR")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            env("HOME")
                .or_else(|| env("USERPROFILE"))
                .filter(|value| !value.is_empty())
                .map(|home| PathBuf::from(home).join(".claude"))
        })
}

/// Whether a write target belongs to the harness's own configuration tree.
/// Existing harness homes compare in canonical space so symlinked home/temp
/// paths agree; a not-yet-created harness home uses a component-aware lexical
/// comparison on the paths exactly as supplied.
pub fn target_is_under_harness_home(target: &Path, env: EnvLookup<'_>) -> bool {
    let Some(home) = harness_home(env) else {
        return false;
    };
    if !home.exists() {
        return target.starts_with(home);
    }
    let Ok(home) = std::fs::canonicalize(home) else {
        return false;
    };
    super::pathutil::canonicalize_with_missing_tail(target)
        .is_some_and(|target| target.starts_with(home))
}

/// The repository root a write target sits in: the nearest ancestor of the
/// target's own parent carrying a `.git` entry -- a DIRECTORY for an ordinary
/// checkout, a FILE for a linked worktree -- so both shapes resolve to the
/// same root. Pure apart from `Path::exists`.
pub fn repo_root_for_target(target: &Path) -> Option<PathBuf> {
    target
        .parent()?
        .ancestors()
        .find(|ancestor| ancestor.join(".git").exists())
        .map(Path::to_path_buf)
}

/// The resolved write TARGET when `intent` is an orchestrator seat's own
/// in-scope repository write, or `None` when it is outside this guard's scope
/// entirely (and so gets no [`ToolAdmission`] at all -- not even `Allow` --
/// because there is nothing here for a posture to act on).
///
/// Confinement is anchored on the resolved TARGET, never on a launch repo: an
/// orchestrator seat has no business editing source in ANY git repository,
/// including a sibling checkout or a linked worktree -- so
/// [`repo_root_for_target`] finds the repo the target itself sits in, and the
/// exemption is narrowed only against THAT repo's own `.zirv/work` and
/// `.zirv/memory`, the two roots a worker's own dispatch/handoff/memory
/// writes still need from this seat. The harness's own home is outside
/// repository-write classification even when an ancestor carries `.git`. A
/// target in no git repository at all is outside this guard's scope, as is a
/// non-orchestrator role and a delegated (subagent) call.
pub fn orchestrator_write_target(
    role: Option<&str>,
    intent: &ToolIntent,
    env: EnvLookup<'_>,
) -> Option<PathBuf> {
    if role != Some("orchestrator") {
        return None;
    }
    if intent.delegated {
        return None;
    }
    let target = intent.write_target.clone()?;
    if target_is_under_harness_home(&target, env) {
        return None;
    }
    let target_repo = repo_root_for_target(&target)?;
    let allowed_roots = [
        target_repo.join(".zirv/work"),
        target_repo.join(".zirv/memory"),
    ];
    if allowed_roots.iter().any(|root| target.starts_with(root)) {
        return None;
    }
    Some(target)
}

/// What the model is told when an orchestrator seat's own guard refuses a
/// repository write (`OrchestratorWrites::Deny`). Names the exact path so
/// the model can see why, and the remedy: dispatch a worker rather than
/// retry the same tool call.
pub fn orchestrator_write_deny_reason(target: &Path) -> String {
    format!(
        "orchestrator seat: dispatch a worker -- this seat coordinates and never edits \
         repository files itself ({}). Delegate the change to a worker: the native Agent \
         tool for this harness, `zirv agent <other-harness>` for another. Writes under \
         .zirv/work and .zirv/memory stay allowed.",
        target.display()
    )
}

/// What the model is told, non-blocking, when an orchestrator seat's own
/// guard lets a repository write through under `OrchestratorWrites::Advise`.
/// Never denies -- the write already proceeded -- only names the target and
/// the standing guidance to delegate anything larger than a trivial edit.
pub fn orchestrator_write_advise_note(target: &Path) -> String {
    format!(
        "orchestrator seat wrote to {}: fine for a trivial edit; delegate substantial changes \
         to a worker",
        target.display()
    )
}

/// The whole orchestrator-write guard decision: `None` when
/// [`orchestrator_write_target`] finds this call outside the guard's scope,
/// otherwise this seat's own posture applied to that target.
pub fn orchestrator_write_admission(
    role: Option<&str>,
    intent: &ToolIntent,
    env: EnvLookup<'_>,
    posture: OrchestratorWrites,
) -> Option<ToolAdmission> {
    let target = orchestrator_write_target(role, intent, env)?;
    Some(match posture {
        OrchestratorWrites::Deny => ToolAdmission::Deny(orchestrator_write_deny_reason(&target)),
        OrchestratorWrites::Advise => {
            ToolAdmission::Advise(orchestrator_write_advise_note(&target))
        }
        OrchestratorWrites::Allow => ToolAdmission::Allow,
    })
}

/// The full before-tool service both paths call: the expensive-seat subagent
/// guard first (a denial there is final), then the orchestrator-write guard.
/// `seat_model`/`role` are the seat's own model and role.
pub fn before_tool(
    seat_model: Option<&str>,
    role: Option<&str>,
    intent: &ToolIntent,
    env: EnvLookup<'_>,
    posture: OrchestratorWrites,
) -> ToolAdmission {
    if let Some(reason) = subagent_admission(seat_model, intent) {
        return ToolAdmission::Deny(reason);
    }
    orchestrator_write_admission(role, intent, env, posture).unwrap_or(ToolAdmission::Allow)
}

// -- after-tool ----------------------------------------------------------

/// What the after-tool service decided about one tool result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResultDisposition {
    /// Hand the model the original bytes.
    Keep,
    /// Replace them with a compact summary; the full text is already durable
    /// under the output store and retrievable by `retrieval_id`.
    Replace { retrieval_id: String },
}

/// The harness's own too-large-output notice. A result carrying one has
/// ALREADY been truncated and spilled to a file by the harness itself, so
/// compacting it again would summarize a truncation notice.
pub fn already_offloaded(text: &str) -> bool {
    text.contains("Output too large") && text.contains("saved to")
}

/// Whether a tool result of `bytes` bytes is worth replacing with a summary,
/// given the configured threshold. Pure, so both paths agree on the cutoff
/// without either re-deriving it.
pub fn should_compact_result(bytes: usize, enabled: bool, threshold_bytes: usize) -> bool {
    enabled && bytes > threshold_bytes
}

// -- prompt --------------------------------------------------------------

/// The per-turn health-marker instruction. Shared so `zirv ctx compile
/// --measure` can report this sentence's own byte cost without re-deriving
/// its wording a second way.
pub fn per_turn_context_text(marker: &str) -> String {
    format!(
        "Prefix each final answer with {marker} on line 1 (mid-turn exempt): zirv ctx health marker."
    )
}

/// The unread-mail note a prompt carries.
pub fn mail_note(unread: usize) -> String {
    format!("[zirv ▸ mail] {unread} unread -- run zirv ctx inbox")
}

/// Everything a prompt should carry, in order, as one joined block -- empty
/// when there is nothing to say. Both paths assemble the same notes the same
/// way; only the envelope around the result differs.
pub fn prompt_notes(marker: &str, extra: &[Option<String>]) -> String {
    let mut lines = Vec::new();
    if !marker.is_empty() {
        lines.push(per_turn_context_text(marker));
    }
    for note in extra.iter().flatten() {
        lines.push(note.clone());
    }
    lines.join("\n")
}

// -- stop and verification ----------------------------------------------

/// Whether the changes a session made are documentation only, and so owe no
/// fresh test evidence.
pub fn changes_are_doc_only(paths: &[PathBuf]) -> bool {
    paths.iter().all(|path| {
        path.starts_with("docs")
            || matches!(
                path.extension().and_then(|ext| ext.to_str()),
                Some("md" | "txt" | "rst")
            )
    })
}

/// The command that produces fresh verification evidence: `zirv verify` for a
/// final-only gate, `zirv test changed` otherwise. Mirrors
/// `engine::advance`'s own naming so a nudge and the gate it anticipates can
/// never name different commands.
pub fn verification_command(final_only: bool) -> &'static str {
    if final_only {
        "zirv verify"
    } else {
        "zirv test changed"
    }
}

/// What the verification service decided.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VerificationDecision {
    /// Nothing changed, or what changed owes no evidence.
    NotRequired,
    /// The active workflow step already gates on fresh evidence itself, so a
    /// second nudge here would only duplicate it.
    CoveredByWorkflow,
    /// Fresh evidence is owed; `command` is what should produce it.
    Required { command: &'static str },
}

/// Whether fresh verification evidence is owed after a session's work.
///
/// `covered_by_workflow` is true when the active workflow step is itself a
/// Test/Verify gate. `modified` is whether this session changed anything at
/// all; `changed_paths` is what it changed; `final_only` selects `zirv
/// verify` over `zirv test changed`, mirroring `engine::advance`'s own
/// naming.
///
/// The gate order is `hook::verify_on_stop_nudge`'s own, unchanged by the
/// extraction: modified, then doc-only, then the workflow step. The doc-only
/// test is deliberately [`changes_are_doc_only`] applied to `changed_paths`
/// as-is, with NO non-empty guard in front of it: an EMPTY change set is
/// vacuously doc-only, and a session that modified nothing git can see owes
/// no fresh evidence. Guarding on non-emptiness would turn "nothing changed"
/// into "evidence required", which is exactly backwards.
pub fn verification(
    modified: bool,
    changed_paths: &[PathBuf],
    covered_by_workflow: bool,
    final_only: bool,
) -> VerificationDecision {
    if !modified {
        return VerificationDecision::NotRequired;
    }
    if changes_are_doc_only(changed_paths) {
        return VerificationDecision::NotRequired;
    }
    if covered_by_workflow {
        return VerificationDecision::CoveredByWorkflow;
    }
    VerificationDecision::Required {
        command: verification_command(final_only),
    }
}

/// What the stop service decided about a session that says it is finished.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StopDecision {
    /// The session may stop.
    Allow,
    /// The session may stop, carrying `note` back to the operator.
    AllowWithNote(String),
    /// The session must keep working; `reason` says why.
    Block(String),
}

/// One reason a stop could be blocked, in the order they are evaluated.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StopSignals {
    /// A stop hook (or a native turn) that already blocked once. A second
    /// block would loop the session forever.
    pub already_blocked: bool,
    /// Tool executions that never reached a terminal state.
    pub incomplete_tools: Vec<String>,
    /// Verification the session still owes.
    pub verification: VerificationDecision,
    /// A workflow gate that refuses completion, with its own message.
    pub workflow_gate: Option<String>,
}

/// The stop service. A model's own "I am done" token is an INPUT here, never
/// the answer: incomplete tool executions, an unsatisfied workflow gate and
/// owed verification each outrank it. `already_blocked` is the loop breaker
/// -- once a session has been blocked, the next stop is allowed regardless,
/// exactly as the hook path's `stop_hook_active` already guarantees.
pub fn stop(signals: &StopSignals) -> StopDecision {
    if signals.already_blocked {
        return StopDecision::Allow;
    }
    if !signals.incomplete_tools.is_empty() {
        return StopDecision::Block(format!(
            "zirv: {} tool execution(s) never reported an outcome ({}); reconcile them before \
             finishing.",
            signals.incomplete_tools.len(),
            signals.incomplete_tools.join(", ")
        ));
    }
    if let Some(gate) = &signals.workflow_gate {
        return StopDecision::Block(gate.clone());
    }
    match &signals.verification {
        VerificationDecision::Required { command } => StopDecision::AllowWithNote(format!(
            "zirv: this session changed files -- run {command}"
        )),
        VerificationDecision::NotRequired | VerificationDecision::CoveredByWorkflow => {
            StopDecision::Allow
        }
    }
}

// -- notification --------------------------------------------------------

/// What a notification payload is allowed to leave behind in a log.
/// Diagnosing a field mismatch needs the field names, never their values: a
/// notification payload can carry tokens, prompts and file contents, and the
/// decision log is a plain file that outlives the session.
pub fn notification_shape(payload: &str) -> String {
    const MAX_KEYS: usize = 200;
    let Ok(serde_json::Value::Object(map)) = serde_json::from_str::<serde_json::Value>(payload)
    else {
        return format!("unparseable notify payload, {} bytes", payload.len());
    };

    let mut keys: String = map.keys().cloned().collect::<Vec<_>>().join(", ");
    keys.truncate(MAX_KEYS);
    format!("notify payload fields: {keys}")
}

/// The lifecycle meaning of a notification, independent of which harness (or
/// none) produced it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NotificationKind {
    /// The session is waiting on the operator for a permission decision.
    AwaitingApproval,
    /// The session has been idle waiting for input.
    AwaitingInput,
    /// Anything this build does not classify.
    Other,
}

/// Classifies a notification message. Deliberately substring-based and
/// fail-soft: an unrecognised message is `Other`, never a guess.
pub fn notification_kind(message: &str) -> NotificationKind {
    let message = message.to_ascii_lowercase();
    if message.contains("permission") || message.contains("approval") {
        NotificationKind::AwaitingApproval
    } else if message.contains("waiting for your input") || message.contains("idle") {
        NotificationKind::AwaitingInput
    } else {
        NotificationKind::Other
    }
}

// -- shared config reads -------------------------------------------------

/// This seat's own repository-write guard posture -- `cfg.supervise.
/// orchestrator_writes`, already narrowed (repo may only tighten) and
/// env-overridden by `CtxConfig::load`. One place every guard resolves it
/// from, so no two can read a different posture for the same session.
pub fn orchestrator_write_posture(cfg: &CtxConfig) -> OrchestratorWrites {
    cfg.supervise.orchestrator_writes
}

#[cfg(test)]
mod tests {
    use super::*;

    fn no_env(_: &str) -> Option<String> {
        None
    }

    #[test]
    fn subagent_admission_denies_a_fork_from_an_expensive_seat() {
        let intent = ToolIntent {
            tool: "Agent".to_string(),
            subagent: Some(SubagentIntent {
                prompt: "do the thing".to_string(),
                subagent_type: "fork".to_string(),
                model: "haiku".to_string(),
            }),
            ..ToolIntent::default()
        };
        let reason = subagent_admission(Some("claude-mythos-5"), &intent).expect("denied");
        assert!(reason.contains("Forks are not allowed"));
    }

    #[test]
    fn subagent_admission_allows_an_explicit_cheap_model() {
        let intent = ToolIntent {
            tool: "Agent".to_string(),
            subagent: Some(SubagentIntent {
                prompt: "do the thing".to_string(),
                subagent_type: "general-purpose".to_string(),
                model: "sonnet".to_string(),
            }),
            ..ToolIntent::default()
        };
        assert_eq!(subagent_admission(Some("claude-mythos-5"), &intent), None);
    }

    #[test]
    fn before_tool_needs_no_harness_binary_or_hook_payload() {
        // Acceptance criterion (f): the decision is reachable with nothing
        // but a neutral intent -- no PATH probe, no installed harness, no
        // JSON envelope.
        let intent = ToolIntent {
            tool: "Task".to_string(),
            subagent: Some(SubagentIntent {
                prompt: "go".to_string(),
                subagent_type: String::new(),
                model: String::new(),
            }),
            ..ToolIntent::default()
        };
        assert!(matches!(
            before_tool(
                Some("fable"),
                Some("orchestrator"),
                &intent,
                &no_env,
                OrchestratorWrites::Deny
            ),
            ToolAdmission::Deny(_)
        ));
    }

    #[test]
    fn orchestrator_write_target_ignores_a_delegated_call() {
        let intent = ToolIntent {
            tool: "Write".to_string(),
            write_target: Some(PathBuf::from("/repo/src/main.rs")),
            delegated: true,
            ..ToolIntent::default()
        };
        assert_eq!(
            orchestrator_write_target(Some("orchestrator"), &intent, &no_env),
            None
        );
    }

    #[test]
    fn stop_blocks_on_an_incomplete_tool_even_when_the_model_says_it_is_done() {
        let decision = stop(&StopSignals {
            already_blocked: false,
            incomplete_tools: vec!["call_1".to_string()],
            verification: VerificationDecision::NotRequired,
            workflow_gate: None,
        });
        assert!(matches!(decision, StopDecision::Block(reason) if reason.contains("call_1")));
    }

    #[test]
    fn stop_allows_once_it_has_already_blocked_once() {
        let decision = stop(&StopSignals {
            already_blocked: true,
            incomplete_tools: vec!["call_1".to_string()],
            verification: VerificationDecision::Required {
                command: "zirv test changed",
            },
            workflow_gate: Some("gate".to_string()),
        });
        assert_eq!(decision, StopDecision::Allow);
    }

    #[test]
    fn verification_is_not_owed_for_documentation_only_changes() {
        assert_eq!(
            verification(true, &[PathBuf::from("README.md")], false, false),
            VerificationDecision::NotRequired
        );
    }

    #[test]
    fn verification_is_not_owed_when_nothing_actually_changed() {
        // An empty change set is vacuously doc-only -- `changes_are_doc_only`
        // has always said so, and `hook::verify_on_stop_nudge` has always
        // returned `None` for it. A non-empty guard in front of that test
        // would turn "nothing changed" into "evidence required".
        assert_eq!(
            verification(true, &[], false, false),
            VerificationDecision::NotRequired
        );
        assert_eq!(
            verification(true, &[], true, true),
            VerificationDecision::NotRequired
        );
    }

    #[test]
    fn a_doc_only_change_set_outranks_the_workflow_step_check() {
        // Gate order is `hook::verify_on_stop_nudge`'s own: doc-only is
        // tested before the active step is consulted, so both answers stay
        // "nothing to say" for exactly the inputs they always did.
        assert_eq!(
            verification(true, &[PathBuf::from("docs/x.md")], true, false),
            VerificationDecision::NotRequired
        );
        assert_eq!(
            verification(true, &[PathBuf::from("src/main.rs")], true, false),
            VerificationDecision::CoveredByWorkflow
        );
    }

    #[test]
    fn verification_names_the_final_command_for_a_verify_step() {
        assert_eq!(
            verification(true, &[PathBuf::from("src/main.rs")], false, true),
            VerificationDecision::Required {
                command: "zirv verify"
            }
        );
    }

    #[test]
    fn notification_shape_never_echoes_values() {
        let shape = notification_shape(r#"{"message":"secret token abc","session_id":"s"}"#);
        assert!(shape.contains("message"));
        assert!(!shape.contains("secret"));
    }

    #[test]
    fn prompt_notes_join_the_marker_and_every_extra() {
        let notes = prompt_notes(
            "[zirv]",
            &[None, Some(mail_note(2)), Some("nudge".to_string())],
        );
        assert_eq!(notes.lines().count(), 3);
        assert!(notes.starts_with("Prefix each final answer with [zirv]"));
    }
}
