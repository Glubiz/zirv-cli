//! The UserPromptSubmit hook: mail/adoption/intake nudges and the admin
//! (`zirv:`-prefixed) prompt dispatch.

use std::io::Write;
use std::path::{Path, PathBuf};

use super::HookPayload;
use super::checkpoints::{adoption_record_path, load_adoption_record, save_adoption_record};
use super::permission::{attention_short, finding_kinds, hook_obfuscation_options};
use super::scope_guard::record_scope_guard_request;
use crate::commands::ctx::adapters::{self, SESSION_ENV};
use crate::commands::ctx::config::{CtxConfig, EnvLookup};
use crate::commands::ctx::event::input_hash;
use crate::commands::ctx::state::{StateDir, now_secs};
use crate::commands::ctx::{CtxResult, log};
use crate::commands::workflow::adoption::{self, AdoptionPolicy, AdoptionSignals};
use crate::commands::workflow::engine;

/// Add marker and due adoption context through UserPromptSubmit. Preserve
/// the marker line because session rot tracking depends on it (#223).
pub fn prompt_output(
    marker: &str,
    adoption_nudge: Option<&str>,
    repo: &Path,
    env: EnvLookup<'_>,
    cfg: &CtxConfig,
) -> String {
    let mail = crate::commands::ctx::mail::session_identity(env)
        .and_then(|short| {
            let state = StateDir::resolve(env).ok()?;
            let messages = crate::commands::ctx::mail::list(
                &state,
                &crate::commands::ctx::state::repo_slug(repo),
                env(adapters::AGENT_ENV).as_deref(),
                Some(&short),
            )
            .ok()?;
            (!messages.is_empty() && !mail_note_deferred(&state, &short, &messages, cfg, env))
                .then_some(messages)
        })
        .map(|messages| crate::commands::ctx::lifecycle::mail_note(messages.len()));
    // Use shared prompt assembly so native and hooked sessions inject notes
    // in the same order (#478).
    let context = crate::commands::ctx::lifecycle::prompt_notes(
        marker,
        &[adoption_nudge.map(str::to_string), mail],
    );
    if context.is_empty() {
        return String::new();
    }
    serde_json::json!({
        "hookSpecificOutput": {
            "hookEventName": "UserPromptSubmit",
            "additionalContext": context
        }
    })
    .to_string()
}

/// Decide whether Jev injection defers this mail note using loaded config
/// before building facts (#785).
fn mail_note_deferred(
    state: &StateDir,
    short: &str,
    messages: &[(PathBuf, crate::commands::ctx::mail::Message)],
    cfg: &CtxConfig,
    env: EnvLookup<'_>,
) -> bool {
    use crate::commands::ctx::inject_gate::{self, Decision, InjectFacts, InjectKind};
    if !inject_gate::enabled(cfg) {
        return false;
    }
    let now = now_secs();
    let parent = crate::commands::ctx::agent::parent_identity(env);
    let facts = InjectFacts {
        unread: Some(messages.len() as u64),
        oldest_unread_age_secs: messages
            .iter()
            .map(|(_, message)| now.saturating_sub(message.sent))
            .max(),
        sender: messages
            .iter()
            .map(|(_, message)| {
                inject_gate::sender_class(
                    &message.from_agent,
                    &crate::commands::ctx::sessions::short_id(&message.from_session),
                    parent.as_deref(),
                )
            })
            .max()
            .unwrap_or_default(),
        turns_since_user_prompt: Some(0),
        ..InjectFacts::default()
    };
    inject_gate::decide_persisted(cfg, state, short, InjectKind::MailNote, facts, now)
        == Decision::Defer
}

pub(super) fn run_prompt<W: Write>(w: &mut W, stdin: &str, env: EnvLookup<'_>) -> CtxResult<i32> {
    let payload = HookPayload::parse(stdin).unwrap_or_default();
    let repo = payload.repo();
    // Config failure must not block a prompt or lose unrelated adoption and
    // attention signals; optional masking degrades to passthrough (#466).
    // A repo-forbidden config is a refusal, not a failure: keep the operator's own masking settings.
    let cfg = crate::commands::ctx::config::CtxConfig::load_refusal_safe(&repo, env);

    // Scope-creep guard: records this prompt's own preservation/limitation
    // language (if any) for the `PreToolUse` checkpoint and `Stop` backstop
    // to read back later. Never emits anything and never affects the rest of
    // this handler -- see `record_scope_guard_request`'s own doc comment for
    // every gate that silently skips it.
    record_scope_guard_request(
        &cfg,
        &payload.session_id,
        &prompt_text_from(stdin),
        &repo,
        env,
    );

    // Check exact read-only administrative requests first so they can be
    // answered before model input and without unrelated hook effects (#745).
    if let Some(block) = admin_dispatch_block(&cfg, &repo, env, &prompt_text_from(stdin)) {
        let _ = writeln!(w, "{block}");
        return Ok(0);
    }

    if cfg.obfuscate.mode != crate::commands::ctx::config::ObfuscateMode::Off {
        let prompt = serde_json::from_str::<serde_json::Value>(stdin)
            .ok()
            .and_then(|value| {
                value
                    .get("prompt")
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_string)
            })
            .unwrap_or_default();
        let mut options = match hook_obfuscation_options(&cfg) {
            Ok(options) => options,
            Err(_) => {
                writeln!(
                    w,
                    "{}",
                    serde_json::json!({
                        "decision": "block",
                        "reason": "zirv could not load sensitive-data literals; refusing prompt"
                    })
                )?;
                return Ok(0);
            }
        };
        options.mode = crate::commands::ctx::obfuscate::Mode::Flag;
        let mut vault = crate::commands::ctx::obfuscate::Vault::default();
        let (_, findings) = crate::commands::ctx::obfuscate::obfuscate(
            &prompt,
            &mut vault,
            &options,
            "user_prompt_submit",
        );
        if !findings.is_empty() {
            let kinds = finding_kinds(&findings);
            if let Ok(state) = StateDir::resolve(env) {
                let session = env(SESSION_ENV).unwrap_or_else(|| payload.session_id.clone());
                let _ = log::append(
                    &state,
                    &log::Decision {
                        ts: now_secs(),
                        session: &session,
                        verb: "hook",
                        verdict: "n/a",
                        score: 0,
                        action: "obfuscate-prompt-flag",
                        detail: &kinds,
                        observed_at: None,
                    },
                );
            }
            if cfg.obfuscate.prompt == crate::commands::ctx::config::ObfuscatePrompt::Block {
                let _ = writeln!(
                    w,
                    "{}",
                    serde_json::json!({
                        "decision": "block",
                        "reason": format!("zirv blocked sensitive values in the prompt ({kinds}); remove them or set obfuscate.prompt = \"flag\"")
                    })
                );
                return Ok(0);
            }
        }
    }

    let adoption_nudge = prompt_adoption_nudge(&repo, &cfg, env);
    let intake_note = intake_discipline_note(&cfg, &payload.session_id, stdin, env);
    let workflow_note = auto_start_workflow_note(&cfg, &payload.session_id, stdin, &repo, env);
    let extra = [intake_note, workflow_note, adoption_nudge]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>();
    let extra = (!extra.is_empty()).then(|| {
        extra.join(
            "
",
        )
    });
    let output = prompt_output(&cfg.score.marker, extra.as_deref(), &repo, env, &cfg);
    if !output.is_empty() {
        let _ = writeln!(w, "{output}");
    }
    if let Ok(state) = StateDir::resolve(env) {
        let session_id = env(SESSION_ENV).unwrap_or_default();
        let _ = crate::commands::ctx::attention::record(
            &state,
            &attention_short(env, &session_id),
            crate::commands::ctx::attention::Observation::new(
                crate::commands::ctx::attention::Authority::AdapterHook,
                "user prompt submitted",
                100,
                now_secs(),
            )
            .with_lifecycle(crate::commands::ctx::attention::Lifecycle::Working),
            now_secs(),
        );
        crate::commands::ctx::approvals::clear_released(
            &state,
            &attention_short(env, &session_id),
            env,
        );
    }
    Ok(0)
}

/// One-turn discipline note for a substantial first prompt, capped to
/// avoid recurring context cost (#753).
pub(crate) const INTAKE_DISCIPLINE_TEXT: &str = "[zirv intake] Substantial task. Plan ordered, verifiable steps before editing. Write or extend tests first for behaviour changes. Never modify or weaken existing or protected tests to make them pass. Run the full test suite before declaring done. Skills: zirv skill load plan / tdd / verify.";

/// Pure: the intake note for `prompt` -- the proxy's own deterministic,
/// text-only classifier (`decision::try_classify_request`, no Git, no
/// network), `Some` only for a `Substantial`+ complexity or `High`+ risk.
fn intake_discipline_for(prompt: &str) -> Option<&'static str> {
    let classification = crate::commands::ctx::proxy::decision::try_classify_request(prompt)?;
    (classification.complexity >= crate::commands::workflow::classify::Complexity::Substantial
        || classification.risk >= crate::commands::workflow::classify::RiskBand::High)
        .then_some(INTAKE_DISCIPLINE_TEXT)
}

/// Whether this session's launch already decided (the harness proxy) or it
/// is a delegated seat that follows its parent's plan -- either way, intake
/// is not this hook's call.
fn intake_skipped_for_launch(env: EnvLookup<'_>) -> bool {
    let set = |key: &str| env(key).is_some_and(|value| !value.is_empty());
    env(adapters::PROXY_DECIDED_ENV).as_deref() == Some("1")
        || matches!(
            env(adapters::SEAT_ROLE_ENV).as_deref(),
            Some("worker" | "sub-orchestrator" | "single")
        )
        || set(crate::commands::ctx::agent::WORK_GROUP_ENV)
        || set(crate::commands::ctx::agent::PARENT_SESSION_ENV)
}

/// Atomically claim the first prompt once per session; I/O uncertainty
/// suppresses the note to avoid repetition.
fn claim_first_prompt(state: &StateDir, session: &str) -> bool {
    let dir = state.intake();
    if crate::commands::ctx::state::create_private_dir_all(&dir).is_err() {
        return false;
    }
    let path = dir.join(format!("{:016x}", input_hash(session)));
    let claimed = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)
        .is_ok();
    if claimed {
        crate::commands::ctx::state::prune_to_newest(
            &dir,
            crate::commands::ctx::state::KEEP_NEWEST,
        );
    }
    claimed
}

/// Return a first-turn note only when all launch and classification gates
/// are known; uncertainty leaves the prompt unchanged (#753).
fn intake_discipline_note(
    cfg: &CtxConfig,
    payload_session: &str,
    stdin: &str,
    env: EnvLookup<'_>,
) -> Option<String> {
    if !cfg.prompt.intake_discipline || intake_skipped_for_launch(env) {
        return None;
    }
    let session = env(SESSION_ENV)
        .filter(|value| !value.is_empty())
        .or_else(|| (!payload_session.is_empty()).then(|| payload_session.to_string()))?;
    let state = StateDir::resolve(env).ok()?;
    if !claim_first_prompt(&state, &session) {
        return None;
    }
    let note = intake_discipline_for(&prompt_text_from(stdin))?;
    let _ = log::append(
        &state,
        &log::Decision {
            ts: now_secs(),
            session: &session,
            verb: "hook",
            verdict: "n/a",
            score: 0,
            action: "intake-discipline",
            detail: "substantial first prompt",
            observed_at: None,
        },
    );
    Some(note.to_string())
}

/// Start a workflow for a session whose prompt is programming or
/// investigation work, once per session, and tell the agent. A prompt that is
/// not such work leaves the session unclaimed so a later one can start it.
/// Every failure leaves the prompt unchanged.
fn auto_start_workflow_note(
    cfg: &CtxConfig,
    payload_session: &str,
    stdin: &str,
    repo: &Path,
    env: EnvLookup<'_>,
) -> Option<String> {
    use crate::commands::ctx::proxy::{decision, launch};
    let policy = cfg.workflow.auto_start;
    if policy == adoption::AutoStartPolicy::Off
        || intake_skipped_for_launch(env)
        || env(crate::commands::ctx::supervisor::CONSULT_ENV).is_some_and(|v| !v.is_empty())
    {
        return None;
    }
    let session = env(SESSION_ENV)
        .filter(|value| !value.is_empty())
        .or_else(|| (!payload_session.is_empty()).then(|| payload_session.to_string()))?;
    let prompt = prompt_text_from(stdin);
    let declined = decision::declines_workflow(&prompt);
    if !declined && !decision::auto_start_wanted(&prompt, policy) {
        return None;
    }
    let state = StateDir::resolve(env).ok()?;
    let short = crate::commands::ctx::sessions::short_id(&session);
    if engine::load_active_for_session(&state, repo, &short)
        .ok()?
        .is_some()
    {
        return None;
    }
    if !claim_first_prompt(&state, &format!("{session}#workflow")) || declined {
        return None;
    }
    let task = crate::utils::truncate_bytes(prompt, Some(4000));
    let launch::WorkflowStart::Started { id } =
        launch::start_named_workflow(None, None, state.root(), repo, &task, Some(&short)).ok()?
    else {
        return None;
    };
    let started = engine::load(&state, repo, &id).ok()?;
    let _ = engine::waive_first_gate(&state, started);
    let _ = log::append(
        &state,
        &log::Decision {
            ts: now_secs(),
            session: &session,
            verb: "hook",
            verdict: "n/a",
            score: 0,
            action: "workflow-auto-start",
            detail: &id,
            observed_at: None,
        },
    );
    Some(format!(
        "[zirv workflow] Started workflow {id} for this session. Follow `zirv workflow status` and its artifacts."
    ))
}

/// Closed set of read-only administrative operations that need no model
/// request. Match exact normalized text and use only trusted repo/env input
/// (#745).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AdminOp {
    CtxStatus,
    JevStatus,
    Inbox,
}

impl AdminOp {
    /// The static op name recorded on the effect row and shown in the block
    /// reason's prefix line -- never the raw prompt text, which this
    /// closed-set path never logs, records, or sends anywhere (including to
    /// Jev; see [`admin_dispatch_block`]'s own doc comment).
    fn name(self) -> &'static str {
        match self {
            AdminOp::CtxStatus => "zirv ctx status",
            AdminOp::JevStatus => "zirv jev status",
            AdminOp::Inbox => "zirv inbox",
        }
    }

    fn matching(normalized: &str) -> Option<Self> {
        match normalized {
            "zirv status" | "zirv ctx status" => Some(AdminOp::CtxStatus),
            "zirv jev status" => Some(AdminOp::JevStatus),
            "zirv inbox" => Some(AdminOp::Inbox),
            _ => None,
        }
    }

    /// Renders this operation's output in-process -- the same function the
    /// matching CLI verb itself calls -- with no shell, no arguments beyond
    /// `repo`/`env`, and no side effect beyond what that renderer already
    /// performs as a read (`inbox --peek` reads mail without consuming it;
    /// `status` is called here with `diff: false` so it never writes a
    /// per-session snapshot). `None` means "could not render"; the caller
    /// falls back to today's path rather than ever failing the prompt.
    fn render(self, cfg: &CtxConfig, repo: &Path, env: EnvLookup<'_>) -> Option<Vec<u8>> {
        let mut out = Vec::new();
        match self {
            AdminOp::CtxStatus => {
                let args = crate::commands::ctx::status::StatusArgs {
                    decisions: 10,
                    brief: true,
                    diff: false,
                    breakdown: None,
                    json: false,
                    agents: false,
                    full: false,
                };
                crate::commands::ctx::status::run_with(&args, &mut out, repo, env, false).ok()?;
            }
            AdminOp::JevStatus => {
                let state = StateDir::resolve(env).ok()?;
                crate::commands::ctx::jev::status(cfg, &state, &mut out).ok()?;
            }
            AdminOp::Inbox => {
                let args = crate::commands::ctx::mail::InboxArgs {
                    peek: true,
                    ..Default::default()
                };
                crate::commands::ctx::mail::run_inbox_with(&args, &mut out, repo, env).ok()?;
            }
        }
        Some(out)
    }
}

/// Normalize only case, whitespace and one leading slash; exact equality
/// prevents arguments or extra words from changing an admin operation.
fn normalize_admin_prompt(prompt: &str) -> String {
    let trimmed = prompt.trim();
    let trimmed = trimmed.strip_prefix('/').unwrap_or(trimmed);
    trimmed
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

/// The raw `prompt` field straight out of the hook's stdin JSON -- not
/// carried on [`HookPayload`] because nothing else in this handler needs it.
fn prompt_text_from(stdin: &str) -> String {
    serde_json::from_str::<serde_json::Value>(stdin)
        .ok()
        .and_then(|value| {
            value
                .get("prompt")
                .and_then(serde_json::Value::as_str)
                .map(str::to_string)
        })
        .unwrap_or_default()
}

/// Answer only exact, authorized read-only admin requests in-process and
/// block their model prompt; other inputs continue unchanged (#745).
/// Selection stays deterministic exact-match only: #746's egress boundary
/// forbids sending prompt text to Jev, so this never makes a Jev call.
fn admin_dispatch_block(
    cfg: &CtxConfig,
    repo: &Path,
    env: EnvLookup<'_>,
    prompt: &str,
) -> Option<String> {
    if !cfg.jev.admin_dispatch || !crate::commands::ctx::jev::available(&cfg.proxy.typesafe) {
        return None;
    }
    let op = AdminOp::matching(&normalize_admin_prompt(prompt))?;
    let rendered = op.render(cfg, repo, env)?;
    let body =
        crate::utils::truncate_bytes(String::from_utf8_lossy(&rendered).into_owned(), Some(8192));
    let reason = format!(
        "zirv: answered \"{}\" locally (no model turn)\n\n{}",
        op.name(),
        body.trim_end()
    );

    if let Ok(state) = StateDir::resolve(env) {
        let effect = crate::commands::ctx::jev::JevEffect {
            item_id: Some(op.name()),
            reason: Some(op.name()),
            ..crate::commands::ctx::jev::JevEffect::new("admin_dispatch", "llm_turn_avoided")
        };
        crate::commands::ctx::jev::record_effect(cfg, &state, cfg.jev.admin_dispatch, &effect);
    }

    Some(
        serde_json::json!({
            "decision": "block",
            "reason": reason
        })
        .to_string(),
    )
}

/// Read the persisted adoption record and current workflow state; only
/// a due, relevant nudge rides this prompt. Do not rescan the transcript
/// in the hot hook path (#223).
pub(super) fn prompt_adoption_nudge(
    repo: &Path,
    cfg: &CtxConfig,
    env: EnvLookup<'_>,
) -> Option<String> {
    if env(crate::commands::ctx::agent::WORK_GROUP_ENV)
        .filter(|v| !v.is_empty())
        .is_some()
    {
        return None;
    }
    let session = env(SESSION_ENV)?;
    let state = StateDir::resolve(env).ok()?;
    let path = adoption_record_path(&state, &session);
    let mut record = load_adoption_record(&path);
    if !record.substantial {
        return None;
    }
    let signals = AdoptionSignals {
        edit_like_calls: record.edit_like_calls,
        turns: record.turns,
        skill_loads: record.skill_loads,
    };

    let workflow_text = (cfg.workflow.adoption >= AdoptionPolicy::Nudge)
        .then(|| {
            // Live re-check: a workflow may have started in another pane
            // since the last Stop hook wrote this record.
            let workflow_active_now = engine::load_active(&state, repo).ok().flatten().is_some();
            adoption::nudge_due(
                cfg.workflow.adoption,
                record.substantial,
                workflow_active_now,
                record.turns,
                record.last_nudged_turn,
            )
            .then(|| {
                record.last_nudged_turn = Some(record.turns);
                adoption::nudge_text(&signals, cfg.workflow.adoption)
            })
        })
        .flatten();

    // Explicit rather than incidental -- under `off` the Stop hook never
    // rescans the transcript (`adoption_stop_nudge`'s own early return), so a
    // persisted `substantial: true` record can go stale instead of ever
    // becoming false again. This function only re-reads that record, so
    // without this check a stale one would still fire the skill nudge here
    // even after an operator turned workflow adoption off.
    let skill_text = (cfg.workflow.adoption != AdoptionPolicy::Off
        && cfg.prompt.skill_index
        && adoption::skill_nudge_due(
            record.substantial,
            record.skill_loads + record.shell_skill_loads,
            record.turns,
            record.last_skill_nudged_turn,
        ))
    .then(|| {
        record.last_skill_nudged_turn = Some(record.turns);
        adoption::skill_nudge_text(&signals)
    });

    if workflow_text.is_none() && skill_text.is_none() {
        return None;
    }
    save_adoption_record(&path, &record);
    let combined = [workflow_text, skill_text]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>();
    Some(combined.join("\n"))
}

#[cfg(test)]
mod tests {
    use super::super::posttool::run_posttool;
    use super::super::pretool_run::run_pretool;
    use super::super::scope_guard::MISSING_TESTS_OWED_LINE;
    use super::super::tests::{
        INJECT_DEFER, SCOPE_GUARD_T24_PROMPT, inject_cfg, mail_waiting,
        scope_guard_bash_posttool_stdin, scope_guard_prompt_stdin, scope_guard_shell_rig,
    };
    use super::*;

    /// `prompt_output` keeps the marker line intact and adds the nudge as a
    /// second line.
    #[test]
    fn prompt_output_is_empty_when_no_context_is_available() {
        assert!(
            prompt_output("", None, Path::new("."), &|_| None, &CtxConfig::default()).is_empty()
        );
    }

    #[test]
    fn prompt_output_signals_session_mail_without_consuming() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().join("state"));
        let env = |key: &str| match key {
            crate::commands::ctx::state::STATE_ENV => Some(state.root().display().to_string()),
            SESSION_ENV => Some("aaaa1111-2222-4333-8444-555555555555".to_string()),
            adapters::AGENT_ENV => Some("claude".to_string()),
            _ => None,
        };
        assert!(
            !prompt_output("[zirv]", None, tmp.path(), &env, &CtxConfig::default())
                .contains("[zirv ▸ mail]")
        );
        let path = crate::commands::ctx::mail::store(
            &state,
            &crate::commands::ctx::state::repo_slug(tmp.path()),
            &crate::commands::ctx::mail::Message {
                from_session: "bbbb2222".to_string(),
                from_agent: "codex".to_string(),
                to: "claude".to_string(),
                to_session: Some("aaaa1111".to_string()),
                sent: now_secs(),
                body: "done".to_string(),
            },
            &CtxConfig::default(),
        )
        .expect("store");
        for marker in ["", "[zirv]"] {
            let out = prompt_output(marker, None, tmp.path(), &env, &CtxConfig::default());
            assert!(
                out.contains("[zirv ▸ mail] 1 unread -- run zirv ctx inbox"),
                "{out}"
            );
            assert!(path.exists());
        }
    }

    #[test]
    fn prompt_output_keeps_the_marker_line_and_appends_the_nudge() {
        let out = prompt_output(
            "[zirv]",
            Some("[zirv workflow] substantial work detected"),
            Path::new("."),
            &|_| None,
            &CtxConfig::default(),
        );
        let parsed: serde_json::Value = serde_json::from_str(&out).expect("valid json");
        let context = parsed["hookSpecificOutput"]["additionalContext"]
            .as_str()
            .expect("additionalContext");
        let mut lines = context.lines();
        assert!(
            lines.next().unwrap_or_default().contains("[zirv]"),
            "the marker line must stay first: {context}"
        );
        assert!(
            lines
                .next()
                .unwrap_or_default()
                .contains("substantial work detected"),
            "the nudge must ride as a second line: {context}"
        );
    }

    #[test]
    fn prompt_hook_emits_the_documented_injection_shape() {
        let out = prompt_output(
            "[zirv]",
            None,
            Path::new("."),
            &|_| None,
            &CtxConfig::default(),
        );
        let parsed: serde_json::Value = serde_json::from_str(&out).expect("valid json");
        assert_eq!(
            parsed["hookSpecificOutput"]["hookEventName"], "UserPromptSubmit",
            "exact key casing matters: {out}"
        );
        let context = parsed["hookSpecificOutput"]["additionalContext"]
            .as_str()
            .expect("additionalContext");
        assert!(
            context.contains("[zirv]"),
            "the marker must appear: {context}"
        );
        assert!(
            context.contains("final"),
            "only final answers carry the marker: {context}"
        );
        assert!(parsed.get("decision").is_none(), "never block a prompt");
        assert!(!context.contains('\u{2014}'));
    }

    #[test]
    fn prompt_hook_uses_the_configured_marker() {
        let out = prompt_output(
            "[acme]",
            None,
            Path::new("."),
            &|_| None,
            &CtxConfig::default(),
        );
        assert!(out.contains("[acme]"));
        assert!(
            !out.contains("[zirv]"),
            "nothing user-specific is hardcoded"
        );
    }

    /// Issue #225: this `additionalContext` is paid, uncached, on every user
    /// turn -- unlike the once-per-session prompt layers in `prompt.rs`, so it
    /// carries a hard byte budget the way `HARNESS_PROMPT`'s own doc comment
    /// tracks a shape budget. Pinned against the raw sentence, not the
    /// wrapping JSON, so growth in `additionalContext`'s own text is caught
    /// even if `hookSpecificOutput`'s envelope grows for an unrelated reason.
    #[test]
    fn prompt_hook_context_stays_under_the_ninety_byte_steady_state_budget() {
        let parsed: serde_json::Value = serde_json::from_str(&prompt_output(
            "[zirv]",
            None,
            Path::new("."),
            &|_| None,
            &CtxConfig::default(),
        ))
        .expect("valid json");
        let context = parsed["hookSpecificOutput"]["additionalContext"]
            .as_str()
            .expect("additionalContext")
            .to_string();
        assert!(
            context.len() <= 90,
            "steady-state per-turn context must stay <= 90 bytes for the default marker, \
             got {} bytes: {context}",
            context.len()
        );
        // Same contract the old 170-byte sentence carried: start every FINAL
        // answer with the marker on line 1, mid-turn notes are exempt, and
        // it is a context-health marker read by zirv ctx -- just shorter.
        for claim in ["final", "[zirv]", "mid-turn", "zirv ctx"] {
            assert!(
                context.contains(claim),
                "the trimmed sentence must still say '{claim}': {context}"
            );
        }
    }

    #[test]
    fn user_prompt_block_names_only_detected_kinds() {
        let home = tempfile::tempdir().expect("home");
        let repo = tempfile::tempdir().expect("repo");
        std::fs::create_dir_all(home.path().join(".zirv")).expect("config dir");
        // Issue #466: masking is opt-in (`obfuscate.mode` defaults to
        // `off`, which this handler now skips entirely); this test is
        // exercising the block decision, so it opts in explicitly.
        std::fs::write(
            home.path().join(".zirv/ctx.toml"),
            "[obfuscate]\nmode = \"obfuscate\"\nprompt = \"block\"\n",
        )
        .expect("config");
        let _guard = crate::commands::ctx::testenv::EnvGuard::set(home.path(), None);
        let stdin = serde_json::json!({
            "session_id":"s1", "cwd":repo.path(),
            "prompt":"use ghp_abcdefghijklmnopqrstuvwxyz123456"
        })
        .to_string();
        let mut out = Vec::new();
        run_prompt(&mut out, &stdin, &|_| None).expect("hook");
        let value: serde_json::Value = serde_json::from_slice(&out).expect("block");
        assert_eq!(value["decision"], "block");
        let reason = value["reason"].as_str().expect("reason");
        assert!(reason.contains("GITHUB_TOKEN:1"), "{reason}");
        assert!(!reason.contains("ghp_"), "{reason}");
    }

    // -- Issue #745: closed-set administrative dispatch ---------------------

    /// The operator env-override keys [`admin_dispatch_block`] reads through
    /// `CtxConfig::load`'s `env` closure -- `jev::available` itself reads
    /// the credential straight off the REAL process environment (never this
    /// closure), so every admin-dispatch test also sets `credential_var`
    /// via [`crate::commands::ctx::testenv::VarGuard`], not here.
    fn admin_dispatch_env(
        state: &std::path::Path,
        credential_var: &str,
        gate_on: bool,
    ) -> std::collections::HashMap<String, String> {
        let mut env: std::collections::HashMap<String, String> = [
            (
                crate::commands::ctx::state::STATE_ENV.to_string(),
                state.display().to_string(),
            ),
            (
                "ZIRV_CTX_PROXY_TYPESAFE_CREDENTIAL_ENV".to_string(),
                credential_var.to_string(),
            ),
        ]
        .into();
        if gate_on {
            env.insert(
                "ZIRV_CTX_JEV_ADMIN_DISPATCH".to_string(),
                "true".to_string(),
            );
        }
        env
    }

    fn run_prompt_captured(
        cwd: &str,
        prompt: &str,
        env: &std::collections::HashMap<String, String>,
    ) -> Vec<u8> {
        let stdin = serde_json::json!({
            "session_id": "s1",
            "cwd": cwd,
            "prompt": prompt
        })
        .to_string();
        let mut out = Vec::new();
        run_prompt(&mut out, &stdin, &|k| env.get(k).cloned()).expect("hook");
        out
    }

    /// Gate on, credential present, exact match (case/whitespace normalized)
    /// -- blocks the prompt before any model request with the rendered
    /// `zirv ctx status` output, and records exactly one effect row. No Jev
    /// HTTP call is ever made on this path (issue #746's egress boundary
    /// forbids sending prompt text to Jev at all), so no decision row and no
    /// cache entry exist either.
    #[test]
    fn admin_dispatch_answers_zirv_ctx_status_without_a_model_turn() {
        let home = tempfile::tempdir().expect("home");
        let repo = tempfile::tempdir().expect("repo");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());
        let state = repo.path().join("state");
        StateDir::from_root(state.clone())
            .ensure()
            .expect("ensure state dir");
        let credential_var = "JEV_TEST_ADMIN_DISPATCH_CTX_STATUS_745";
        let _cred =
            crate::commands::ctx::testenv::VarGuard::set(&[(credential_var, Some("secret"))]);
        let env = admin_dispatch_env(&state, credential_var, true);

        let out = run_prompt_captured(
            repo.path().to_str().expect("utf8 repo path"),
            "  Zirv   CTX   Status  ",
            &env,
        );
        let value: serde_json::Value = serde_json::from_slice(&out).expect("block json");
        assert_eq!(value["decision"], "block");
        let reason = value["reason"].as_str().expect("reason");
        assert!(
            reason.starts_with("zirv: answered \"zirv ctx status\" locally (no model turn)"),
            "got {reason}"
        );
        assert!(reason.contains("state dir:"), "got {reason}");

        let effects =
            std::fs::read_to_string(state.join("jev-effects.jsonl")).expect("effects file");
        assert!(
            effects.contains("\"site\":\"admin_dispatch\""),
            "got {effects}"
        );
        assert!(
            effects.contains("\"action\":\"llm_turn_avoided\""),
            "got {effects}"
        );
        assert!(
            effects.contains("\"item_id\":\"zirv ctx status\""),
            "got {effects}"
        );
        assert_eq!(
            effects.lines().count(),
            1,
            "exactly one effect row, no other write: {effects}"
        );
        assert!(
            !state.join("jev-decisions.jsonl").exists(),
            "an exact-match admin dispatch makes no Jev call, so it writes no decision row"
        );
        assert!(
            !state.join("jev-cache").exists(),
            "an exact-match admin dispatch makes no Jev call, so no cache entry can exist"
        );
    }

    /// Same contract, the `zirv jev status` operation.
    #[test]
    fn admin_dispatch_answers_zirv_jev_status_without_a_model_turn() {
        let home = tempfile::tempdir().expect("home");
        let repo = tempfile::tempdir().expect("repo");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());
        let state = repo.path().join("state");
        let credential_var = "JEV_TEST_ADMIN_DISPATCH_JEV_STATUS_745";
        let _cred =
            crate::commands::ctx::testenv::VarGuard::set(&[(credential_var, Some("secret"))]);
        let env = admin_dispatch_env(&state, credential_var, true);

        let out = run_prompt_captured(
            repo.path().to_str().expect("utf8 repo path"),
            "/zirv jev status",
            &env,
        );
        let value: serde_json::Value = serde_json::from_slice(&out).expect("block json");
        assert_eq!(value["decision"], "block");
        let reason = value["reason"].as_str().expect("reason");
        assert!(
            reason.starts_with("zirv: answered \"zirv jev status\" locally (no model turn)"),
            "got {reason}"
        );
        assert!(reason.contains("jev.admin_dispatch on"), "got {reason}");
        assert!(reason.contains("status        active"), "got {reason}");
        assert!(
            std::fs::read_to_string(state.join("jev-effects.jsonl"))
                .expect("effects file")
                .contains("\"item_id\":\"zirv jev status\"")
        );
    }

    /// `zirv inbox` -- the rendered output must be a PEEK: the message stays
    /// unread (still readable by an ordinary `inbox --peek` afterward), not
    /// moved into `read/` as a normal consuming `zirv ctx inbox` would.
    #[test]
    fn admin_dispatch_answers_zirv_inbox_without_consuming_mail() {
        let home = tempfile::tempdir().expect("home");
        let repo = tempfile::tempdir().expect("repo");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());
        let state = repo.path().join("state");
        let send_env: std::collections::HashMap<String, String> = [(
            crate::commands::ctx::state::STATE_ENV.to_string(),
            state.display().to_string(),
        )]
        .into();
        let mut send_out = Vec::new();
        let mut send_stdin = std::io::Cursor::new(Vec::<u8>::new());
        crate::commands::ctx::mail::run_send_with(
            &crate::commands::ctx::mail::SendArgs {
                claim_once: true,
                message: Some("admin dispatch peek marker: still unread".to_string()),
                ..crate::commands::ctx::mail::SendArgs::default()
            },
            &mut send_out,
            repo.path(),
            &|k| send_env.get(k).cloned(),
            &mut send_stdin,
        )
        .expect("seed mail");

        let credential_var = "JEV_TEST_ADMIN_DISPATCH_INBOX_745";
        let _cred =
            crate::commands::ctx::testenv::VarGuard::set(&[(credential_var, Some("secret"))]);
        let env = admin_dispatch_env(&state, credential_var, true);

        let out = run_prompt_captured(
            repo.path().to_str().expect("utf8 repo path"),
            "zirv inbox",
            &env,
        );
        let value: serde_json::Value = serde_json::from_slice(&out).expect("block json");
        assert_eq!(value["decision"], "block");
        let reason = value["reason"].as_str().expect("reason");
        assert!(
            reason.starts_with("zirv: answered \"zirv inbox\" locally (no model turn)"),
            "got {reason}"
        );
        assert!(
            reason.contains("admin dispatch peek marker: still unread"),
            "got {reason}"
        );

        // Still there, unconsumed: an ordinary peek after the hook ran must
        // see it exactly as before.
        let mut check_out = Vec::new();
        crate::commands::ctx::mail::run_inbox_with(
            &crate::commands::ctx::mail::InboxArgs {
                peek: true,
                ..Default::default()
            },
            &mut check_out,
            repo.path(),
            &|k| send_env.get(k).cloned(),
        )
        .expect("inbox check");
        let text = String::from_utf8(check_out).expect("utf8");
        assert!(
            text.contains("admin dispatch peek marker: still unread"),
            "the peek must not have consumed the message: {text}"
        );
    }

    /// Extra words after an otherwise-recognized command are a near-miss,
    /// not a match: the closed set is exact-match only. Falls straight
    /// through to today's path, byte-identical to a run with the gate off,
    /// and records no effect row.
    #[test]
    fn admin_dispatch_near_miss_falls_through_byte_identical() {
        let prompt = "zirv ctx status --json";
        let cwd = "/repo";

        let home_on = tempfile::tempdir().expect("home");
        let _home_on = crate::commands::ctx::testenv::HomeGuard::set(home_on.path());
        let state_on_dir = tempfile::tempdir().expect("state");
        let state_on = state_on_dir.path().to_path_buf();
        let credential_var = "JEV_TEST_ADMIN_DISPATCH_NEAR_MISS_745";
        let _cred =
            crate::commands::ctx::testenv::VarGuard::set(&[(credential_var, Some("secret"))]);
        let env_on = admin_dispatch_env(&state_on, credential_var, true);
        let out_on = run_prompt_captured(cwd, prompt, &env_on);
        drop(_home_on);

        let home_off = tempfile::tempdir().expect("home");
        let _home_off = crate::commands::ctx::testenv::HomeGuard::set(home_off.path());
        let state_off_dir = tempfile::tempdir().expect("state");
        let state_off = state_off_dir.path().to_path_buf();
        // "Today" -- no jev overrides in the environment at all.
        let env_off: std::collections::HashMap<String, String> = [(
            crate::commands::ctx::state::STATE_ENV.to_string(),
            state_off.display().to_string(),
        )]
        .into();
        let out_off = run_prompt_captured(cwd, prompt, &env_off);

        assert_eq!(
            out_on, out_off,
            "a near-miss must fall through byte-identical to today"
        );
        assert!(!state_on.join("jev-effects.jsonl").exists());
    }

    /// Gate off, credential present, otherwise-exact match -- byte-identical
    /// to today, and no effect row.
    #[test]
    fn admin_dispatch_gate_off_falls_through_byte_identical() {
        let prompt = "zirv jev status";
        let cwd = "/repo";
        let credential_var = "JEV_TEST_ADMIN_DISPATCH_GATE_OFF_745";
        let _cred =
            crate::commands::ctx::testenv::VarGuard::set(&[(credential_var, Some("secret"))]);

        let home_off = tempfile::tempdir().expect("home");
        let _home_off = crate::commands::ctx::testenv::HomeGuard::set(home_off.path());
        let state_off_dir = tempfile::tempdir().expect("state");
        let state_off = state_off_dir.path().to_path_buf();
        let env_off = admin_dispatch_env(&state_off, credential_var, false);
        let out_off = run_prompt_captured(cwd, prompt, &env_off);
        drop(_home_off);

        let home_today = tempfile::tempdir().expect("home");
        let _home_today = crate::commands::ctx::testenv::HomeGuard::set(home_today.path());
        let state_today_dir = tempfile::tempdir().expect("state");
        let state_today = state_today_dir.path().to_path_buf();
        let env_today: std::collections::HashMap<String, String> = [(
            crate::commands::ctx::state::STATE_ENV.to_string(),
            state_today.display().to_string(),
        )]
        .into();
        let out_today = run_prompt_captured(cwd, prompt, &env_today);

        assert_eq!(
            out_off, out_today,
            "gate off must fall through byte-identical to today"
        );
        assert!(!state_off.join("jev-effects.jsonl").exists());
    }

    /// Gate on, exact match, but the credential env var is unset -- same
    /// contract as gate off: byte-identical to today, no effect row. Proves
    /// `jev::available` (which reads the REAL process environment, not the
    /// `env` closure `CtxConfig::load` uses) is actually consulted, not just
    /// the gate.
    #[test]
    fn admin_dispatch_missing_credential_falls_through_byte_identical() {
        let prompt = "zirv ctx status";
        let cwd = "/repo";
        let credential_var = "JEV_TEST_ADMIN_DISPATCH_MISSING_KEY_745";
        // Never set: `available` must see it as absent.

        let home_on = tempfile::tempdir().expect("home");
        let _home_on = crate::commands::ctx::testenv::HomeGuard::set(home_on.path());
        let state_on_dir = tempfile::tempdir().expect("state");
        let state_on = state_on_dir.path().to_path_buf();
        let env_on = admin_dispatch_env(&state_on, credential_var, true);
        let out_on = run_prompt_captured(cwd, prompt, &env_on);
        drop(_home_on);

        let home_today = tempfile::tempdir().expect("home");
        let _home_today = crate::commands::ctx::testenv::HomeGuard::set(home_today.path());
        let state_today_dir = tempfile::tempdir().expect("state");
        let state_today = state_today_dir.path().to_path_buf();
        let env_today: std::collections::HashMap<String, String> = [(
            crate::commands::ctx::state::STATE_ENV.to_string(),
            state_today.display().to_string(),
        )]
        .into();
        let out_today = run_prompt_captured(cwd, prompt, &env_today);

        assert_eq!(
            out_on, out_today,
            "a missing credential must fall through byte-identical to today"
        );
        assert!(!state_on.join("jev-effects.jsonl").exists());
    }

    /// Issue #753: a prompt the proxy's own size floor calls `Substantial`
    /// (eight enumerated requirements), and a one-liner that stays trivial.
    const SUBSTANTIAL_PROMPT: &str = "Build the importer:\n1. parse csv\n2. validate rows\n\
        3. dedupe keys\n4. map columns\n5. write rows\n6. report errors\n7. add a cli flag\n\
        8. document it";
    const TRIVIAL_PROMPT: &str = "fix the typo in README";

    fn intake_env(
        state: &Path,
        extra: &[(&str, &str)],
    ) -> std::collections::HashMap<String, String> {
        let mut env: std::collections::HashMap<String, String> = [
            (
                crate::commands::ctx::state::STATE_ENV.to_string(),
                state.display().to_string(),
            ),
            (SESSION_ENV.to_string(), "sess-intake".to_string()),
        ]
        .into();
        for (key, value) in extra {
            env.insert((*key).to_string(), (*value).to_string());
        }
        env
    }

    fn intake_stdin(prompt: &str) -> String {
        serde_json::json!({ "session_id": "s1", "prompt": prompt }).to_string()
    }

    fn git_repo_with_commit() -> tempfile::TempDir {
        let repo = tempfile::tempdir().expect("repo");
        for args in [
            vec!["init", "-q"],
            vec!["add", "."],
            vec![
                "-c",
                "user.email=t@example.com",
                "-c",
                "user.name=t",
                "-c",
                "commit.gpgsign=false",
                "commit",
                "-q",
                "--allow-empty",
                "-m",
                "base",
            ],
        ] {
            let status = std::process::Command::new("git")
                .args(&args)
                .current_dir(repo.path())
                .status()
                .expect("run git");
            assert!(status.success(), "git {args:?} failed");
        }
        repo
    }

    const CODING_PROMPT: &str = "fix the crash when the dashboard pane is clicked";

    #[test]
    fn auto_start_starts_a_running_workflow_for_coding_work_once() {
        let state = tempfile::tempdir().expect("state");
        let repo = git_repo_with_commit();
        let env = intake_env(state.path(), &[]);
        let lookup = |k: &str| env.get(k).cloned();
        let cfg = CtxConfig::default();

        let note = auto_start_workflow_note(
            &cfg,
            "s1",
            &intake_stdin(CODING_PROMPT),
            repo.path(),
            &lookup,
        )
        .expect("a coding prompt starts a workflow");
        assert!(
            note.starts_with("[zirv workflow] Started workflow "),
            "{note}"
        );

        let store = StateDir::from_root(state.path().to_path_buf());
        let active = engine::load_active(&store, repo.path())
            .expect("readable")
            .expect("a workflow is active");
        assert_eq!(active.status, engine::WorkflowStatus::Running);
        assert_eq!(active.task, CODING_PROMPT);
        assert_eq!(
            auto_start_workflow_note(
                &cfg,
                "s1",
                &intake_stdin(CODING_PROMPT),
                repo.path(),
                &lookup
            ),
            None,
            "a session that has its workflow starts no second one"
        );
    }

    #[test]
    fn auto_start_ignores_questions_without_claiming_the_session() {
        let state = tempfile::tempdir().expect("state");
        let repo = git_repo_with_commit();
        let env = intake_env(state.path(), &[]);
        let lookup = |k: &str| env.get(k).cloned();
        let cfg = CtxConfig::default();

        assert_eq!(
            auto_start_workflow_note(
                &cfg,
                "s1",
                &intake_stdin("how does the dashboard pane click handler work?"),
                repo.path(),
                &lookup
            ),
            None
        );
        assert!(
            auto_start_workflow_note(
                &cfg,
                "s1",
                &intake_stdin(CODING_PROMPT),
                repo.path(),
                &lookup
            )
            .is_some(),
            "a later coding prompt still starts one"
        );
    }

    #[test]
    fn auto_start_skips_opt_outs_off_and_delegated_launches() {
        let repo = git_repo_with_commit();
        let off = {
            let mut cfg = CtxConfig::default();
            cfg.workflow.auto_start = adoption::AutoStartPolicy::Off;
            cfg
        };
        let cases: Vec<_> = vec![
            (
                CtxConfig::default(),
                "fix the crash, no workflow please",
                vec![],
            ),
            (off, CODING_PROMPT, vec![]),
            (
                CtxConfig::default(),
                CODING_PROMPT,
                vec![(adapters::SEAT_ROLE_ENV, "worker")],
            ),
            (
                CtxConfig::default(),
                CODING_PROMPT,
                vec![(adapters::SEAT_ROLE_ENV, "sub-orchestrator")],
            ),
            (
                CtxConfig::default(),
                CODING_PROMPT,
                vec![(crate::commands::ctx::agent::WORK_GROUP_ENV, "g")],
            ),
            (
                CtxConfig::default(),
                CODING_PROMPT,
                vec![(crate::commands::ctx::agent::PARENT_SESSION_ENV, "p")],
            ),
            (
                CtxConfig::default(),
                CODING_PROMPT,
                vec![(crate::commands::ctx::supervisor::CONSULT_ENV, "1")],
            ),
        ];
        for (cfg, prompt, extra) in cases {
            let state = tempfile::tempdir().expect("state");
            let env = intake_env(state.path(), &extra);
            assert_eq!(
                auto_start_workflow_note(&cfg, "s1", &intake_stdin(prompt), repo.path(), &|k| {
                    env.get(k).cloned()
                }),
                None,
                "{prompt:?} {extra:?} must start nothing"
            );
            let store = StateDir::from_root(state.path().to_path_buf());
            assert!(
                engine::load_active(&store, repo.path())
                    .expect("readable")
                    .is_none()
            );
        }
    }

    #[test]
    fn intake_discipline_text_stays_compact() {
        assert!(
            INTAKE_DISCIPLINE_TEXT.len() <= 400,
            "{} bytes",
            INTAKE_DISCIPLINE_TEXT.len()
        );
    }

    #[test]
    fn intake_discipline_fires_on_a_substantial_first_prompt_only_once() {
        let state = tempfile::tempdir().expect("state");
        let env = intake_env(state.path(), &[]);
        let lookup = |k: &str| env.get(k).cloned();
        let cfg = CtxConfig::default();
        let first = intake_discipline_note(&cfg, "s1", &intake_stdin(SUBSTANTIAL_PROMPT), &lookup);
        assert_eq!(first.as_deref(), Some(INTAKE_DISCIPLINE_TEXT));
        let second = intake_discipline_note(&cfg, "s1", &intake_stdin(SUBSTANTIAL_PROMPT), &lookup);
        assert_eq!(second, None, "second turn must carry nothing");
    }

    #[test]
    fn intake_discipline_is_absent_for_a_trivial_first_prompt_and_after_it() {
        let state = tempfile::tempdir().expect("state");
        let env = intake_env(state.path(), &[]);
        let lookup = |k: &str| env.get(k).cloned();
        let cfg = CtxConfig::default();
        assert_eq!(
            intake_discipline_note(&cfg, "s1", &intake_stdin(TRIVIAL_PROMPT), &lookup),
            None
        );
        assert_eq!(
            intake_discipline_note(&cfg, "s1", &intake_stdin(SUBSTANTIAL_PROMPT), &lookup),
            None,
            "only the FIRST prompt is classified"
        );
    }

    #[test]
    fn intake_discipline_respects_the_opt_out() {
        let state = tempfile::tempdir().expect("state");
        let env = intake_env(state.path(), &[]);
        let mut cfg = CtxConfig::default();
        cfg.prompt.intake_discipline = false;
        assert_eq!(
            intake_discipline_note(&cfg, "s1", &intake_stdin(SUBSTANTIAL_PROMPT), &|k| {
                env.get(k).cloned()
            }),
            None
        );
    }

    #[test]
    fn intake_discipline_skips_proxy_decided_and_delegated_launches() {
        for extra in [
            (adapters::PROXY_DECIDED_ENV, "1"),
            (adapters::SEAT_ROLE_ENV, "worker"),
            (adapters::SEAT_ROLE_ENV, "single"),
            (crate::commands::ctx::agent::PARENT_SESSION_ENV, "parent"),
        ] {
            let state = tempfile::tempdir().expect("state");
            let env = intake_env(state.path(), &[extra]);
            assert_eq!(
                intake_discipline_note(
                    &CtxConfig::default(),
                    "s1",
                    &intake_stdin(SUBSTANTIAL_PROMPT),
                    &|k| env.get(k).cloned()
                ),
                None,
                "{extra:?} must skip intake"
            );
        }
    }

    /// End to end through the real `UserPromptSubmit` handler: the note
    /// lands in `additionalContext` beside the marker line.
    #[test]
    fn run_prompt_injects_the_intake_note_on_a_substantial_first_prompt() {
        let home = tempfile::tempdir().expect("home");
        let repo = tempfile::tempdir().expect("repo");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());
        let state = repo.path().join("state");
        let env = intake_env(&state, &[]);
        let cwd = repo.path().display().to_string();
        let out =
            String::from_utf8(run_prompt_captured(&cwd, SUBSTANTIAL_PROMPT, &env)).expect("utf8");
        let parsed: serde_json::Value = serde_json::from_str(out.trim()).expect("json");
        let context = parsed["hookSpecificOutput"]["additionalContext"]
            .as_str()
            .expect("context");
        assert!(context.contains("[zirv intake]"), "{context}");
        let again =
            String::from_utf8(run_prompt_captured(&cwd, SUBSTANTIAL_PROMPT, &env)).expect("utf8");
        assert!(!again.contains("[zirv intake]"), "{again}");
    }

    #[test]
    fn inject_gate_on_defers_the_mail_note() {
        let (url, handle) = crate::commands::ctx::jev::tests::one_shot_server(200, INJECT_DEFER);
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().join("state"));
        let credential_env = "HOOK_TEST_INJECT_MAIL_DEFER";
        // SAFETY (test-only): a unique env var name this test owns.
        unsafe { std::env::set_var(credential_env, "secret") };
        let cfg = inject_cfg(url, credential_env);
        let env = mail_waiting(tmp.path(), &state);
        let out = prompt_output("[zirv]", None, tmp.path(), &env, &cfg);
        unsafe { std::env::remove_var(credential_env) };
        handle.join().expect("server thread");
        assert!(!out.contains("[zirv ▸ mail]"), "{out}");
        assert!(
            out.contains("[zirv]"),
            "the marker line is never deferred: {out}"
        );
    }

    // -- Scope guard: the "tests owed" checkpoint line (queued item 1) ------

    fn scope_guard_edit_stdin_for(
        session: &str,
        cwd: &Path,
        permission_mode: &str,
        target_rel: &str,
    ) -> String {
        serde_json::json!({
            "session_id": session,
            "cwd": cwd.display().to_string(),
            "tool_name": "Edit",
            "tool_input": {
                "file_path": cwd.join(target_rel).display().to_string(),
                "old_string": "a",
                "new_string": "b",
            },
            "permission_mode": permission_mode,
        })
        .to_string()
    }

    /// A headless session editing a non-test source file gets BOTH the
    /// scope-guard text and the tests-owed line, in the same one-time note.
    #[test]
    fn scope_checkpoint_combines_tests_owed_with_scope_text_for_a_headless_edit() {
        let home = tempfile::tempdir().expect("home");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());
        let repo = tempfile::tempdir().expect("repo");
        let state_dir = tempfile::tempdir().expect("state dir");
        let env: std::collections::HashMap<String, String> = [
            (
                crate::commands::ctx::state::STATE_ENV.to_string(),
                state_dir.path().display().to_string(),
            ),
            (adapters::HEADLESS_ENV.to_string(), "1".to_string()),
        ]
        .into();
        let lookup = |k: &str| env.get(k).cloned();
        let session = "sess-owed-1";

        run_prompt(
            &mut Vec::new(),
            &scope_guard_prompt_stdin(session, repo.path(), SCOPE_GUARD_T24_PROMPT),
            &lookup,
        )
        .expect("run_prompt");

        let edit_stdin =
            scope_guard_edit_stdin_for(session, repo.path(), "dontAsk", "src/feature.rs");
        let mut out = Vec::new();
        run_pretool(&mut out, &edit_stdin, &lookup).expect("run_pretool");
        let out = String::from_utf8(out).expect("utf8");
        assert!(
            out.contains("Scope checkpoint") && out.contains("Before changing existing code"),
            "must still carry the scope-guard text: {out}"
        );
        assert!(
            out.contains(MISSING_TESTS_OWED_LINE),
            "must also carry the tests-owed line: {out}"
        );
    }

    /// #849: a native subagent shares the lead's session id, so it must neither read the lead's
    /// prompt record nor spend the lead's one-time checkpoint.
    #[test]
    fn a_subagent_edit_never_gets_or_spends_the_leads_scope_checkpoint() {
        let home = tempfile::tempdir().expect("home");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());
        let repo = tempfile::tempdir().expect("repo");
        let state_dir = tempfile::tempdir().expect("state dir");
        let env: std::collections::HashMap<String, String> = [(
            crate::commands::ctx::state::STATE_ENV.to_string(),
            state_dir.path().display().to_string(),
        )]
        .into();
        let lookup = |k: &str| env.get(k).cloned();
        let session = "sess-849";
        run_prompt(
            &mut Vec::new(),
            &scope_guard_prompt_stdin(session, repo.path(), SCOPE_GUARD_T24_PROMPT),
            &lookup,
        )
        .expect("run_prompt");

        let mut edit: serde_json::Value = serde_json::from_str(&scope_guard_edit_stdin_for(
            session,
            repo.path(),
            "default",
            "src/feature.rs",
        ))
        .expect("json");
        edit["agent_id"] = serde_json::json!("sub-a");
        let mut out = Vec::new();
        run_pretool(&mut out, &edit.to_string(), &lookup).expect("run_pretool");
        assert!(out.is_empty(), "a subagent gets no lead checkpoint");

        let lead_edit =
            scope_guard_edit_stdin_for(session, repo.path(), "default", "src/feature.rs");
        let mut out = Vec::new();
        run_pretool(&mut out, &lead_edit, &lookup).expect("run_pretool");
        assert!(
            String::from_utf8(out)
                .expect("utf8")
                .contains("Scope checkpoint"),
            "the lead's checkpoint was not spent by the subagent"
        );
    }

    /// `[scope_guard] enabled = false` with the missing-tests gate left on
    /// (its default): the checkpoint still fires, carrying ONLY the
    /// tests-owed line.
    #[test]
    fn scope_checkpoint_shows_tests_owed_alone_when_scope_guard_is_disabled() {
        let home = tempfile::tempdir().expect("home");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());
        let repo = tempfile::tempdir().expect("repo");
        std::fs::create_dir_all(repo.path().join(".zirv")).expect("mkdir");
        std::fs::write(
            repo.path().join(".zirv/ctx.toml"),
            "[scope_guard]\nenabled = false\n",
        )
        .expect("write");
        let state_dir = tempfile::tempdir().expect("state dir");
        let env: std::collections::HashMap<String, String> = [
            (
                crate::commands::ctx::state::STATE_ENV.to_string(),
                state_dir.path().display().to_string(),
            ),
            (adapters::HEADLESS_ENV.to_string(), "1".to_string()),
        ]
        .into();
        let lookup = |k: &str| env.get(k).cloned();
        let session = "sess-owed-2";

        run_prompt(
            &mut Vec::new(),
            &scope_guard_prompt_stdin(session, repo.path(), SCOPE_GUARD_T24_PROMPT),
            &lookup,
        )
        .expect("run_prompt");

        let edit_stdin =
            scope_guard_edit_stdin_for(session, repo.path(), "dontAsk", "src/feature.rs");
        let mut out = Vec::new();
        run_pretool(&mut out, &edit_stdin, &lookup).expect("run_pretool");
        let out = String::from_utf8(out).expect("utf8");
        assert!(
            out.contains(MISSING_TESTS_OWED_LINE),
            "must carry the tests-owed line even with scope_guard disabled: {out}"
        );
        assert!(
            !out.contains("Before changing existing code"),
            "must not carry the scope-guard's own body text: {out}"
        );
    }

    /// An interactive session (no `ZIRV_CTX_HEADLESS=1`) never gets the
    /// tests-owed line, even though the missing-tests gate itself is on.
    #[test]
    fn scope_checkpoint_never_shows_tests_owed_for_an_interactive_session() {
        let home = tempfile::tempdir().expect("home");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());
        let repo = tempfile::tempdir().expect("repo");
        let state_dir = tempfile::tempdir().expect("state dir");
        let env: std::collections::HashMap<String, String> = [(
            crate::commands::ctx::state::STATE_ENV.to_string(),
            state_dir.path().display().to_string(),
        )]
        .into();
        let lookup = |k: &str| env.get(k).cloned();
        let session = "sess-owed-3";

        run_prompt(
            &mut Vec::new(),
            &scope_guard_prompt_stdin(session, repo.path(), SCOPE_GUARD_T24_PROMPT),
            &lookup,
        )
        .expect("run_prompt");

        let edit_stdin =
            scope_guard_edit_stdin_for(session, repo.path(), "default", "src/feature.rs");
        let mut out = Vec::new();
        run_pretool(&mut out, &edit_stdin, &lookup).expect("run_pretool");
        let out = String::from_utf8(out).expect("utf8");
        assert!(
            !out.contains(MISSING_TESTS_OWED_LINE),
            "an interactive session must never see the tests-owed line: {out}"
        );
    }

    /// Editing a file that already looks like a test file never owes the
    /// tests-owed line, even headlessly.
    #[test]
    fn scope_checkpoint_never_shows_tests_owed_for_a_test_file_edit() {
        let home = tempfile::tempdir().expect("home");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());
        let repo = tempfile::tempdir().expect("repo");
        let state_dir = tempfile::tempdir().expect("state dir");
        let env: std::collections::HashMap<String, String> = [
            (
                crate::commands::ctx::state::STATE_ENV.to_string(),
                state_dir.path().display().to_string(),
            ),
            (adapters::HEADLESS_ENV.to_string(), "1".to_string()),
        ]
        .into();
        let lookup = |k: &str| env.get(k).cloned();
        let session = "sess-owed-4";

        run_prompt(
            &mut Vec::new(),
            &scope_guard_prompt_stdin(session, repo.path(), SCOPE_GUARD_T24_PROMPT),
            &lookup,
        )
        .expect("run_prompt");

        let edit_stdin =
            scope_guard_edit_stdin_for(session, repo.path(), "dontAsk", "tests/feature_test.rs");
        let mut out = Vec::new();
        run_pretool(&mut out, &edit_stdin, &lookup).expect("run_pretool");
        let out = String::from_utf8(out).expect("utf8");
        assert!(
            !out.contains(MISSING_TESTS_OWED_LINE),
            "editing a test file must never owe a test: {out}"
        );
        assert!(
            out.contains("Scope checkpoint"),
            "the scope-guard part must still fire: {out}"
        );
    }

    /// The `PostToolUse` shell path folds in the same tests-owed line when a
    /// headless shell command changes an existing non-test source file.
    #[test]
    fn scope_guard_shell_checkpoint_combines_tests_owed_for_a_headless_shell_edit() {
        let rig = scope_guard_shell_rig();
        let mut env = rig.env.clone();
        env.insert(adapters::HEADLESS_ENV.to_string(), "1".to_string());
        let lookup = |k: &str| env.get(k).cloned();
        let session = "sess-owed-5";

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
                .current_dir(rig.repo.path())
                .status()
                .expect("run git");
            assert!(status.success(), "git {args:?} failed");
        };
        std::fs::write(rig.repo.path().join("src.rs"), "fn main() {}\n").expect("write");
        git(&["add", "src.rs"]);
        git(&["commit", "-q", "-m", "add src.rs"]);

        run_prompt(
            &mut Vec::new(),
            &scope_guard_prompt_stdin(session, rig.repo.path(), SCOPE_GUARD_T24_PROMPT),
            &lookup,
        )
        .expect("run_prompt");

        std::fs::write(
            rig.repo.path().join("src.rs"),
            "fn main() { println!(\"x\"); }\n",
        )
        .expect("simulate a shell edit");

        let stdin = scope_guard_bash_posttool_stdin(session, rig.repo.path(), "sed -i ... src.rs");
        let mut out = Vec::new();
        run_posttool(&mut out, &stdin, &lookup).expect("run_posttool");
        let out = String::from_utf8(out).expect("utf8");
        assert!(
            out.contains(MISSING_TESTS_OWED_LINE),
            "a headless shell edit to a non-test source file must owe a test: {out}"
        );
    }
}
