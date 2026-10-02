//! PreToolUse entry point: bash/PowerShell rewriting and `run_pretool`
//! itself.

use std::io::Write;
use std::path::Path;

use super::checkpoints::cfg_or_operator_only_gate;
use super::permission::{attention_short, clear_resolved_approval};
use super::pretool_guard::{
    OrchestratorWriteOutcome, normalized_write_target, orchestrator_advisory_should_surface,
    orchestrator_write_decision, skill_pointer_override,
};
use super::pretool_tier::{PreToolPayload, dispatch_tier_override, pretool_decision, resolved_cwd};
use super::scope_guard::{scope_checkpoint_mark_shown, scope_checkpoint_note};
#[cfg(test)]
use crate::commands::ctx::adapters::SESSION_ENV;
use crate::commands::ctx::adapters::{self};
use crate::commands::ctx::config::{CtxConfig, EnvLookup};
use crate::commands::ctx::lifecycle::FILE_MODIFICATION_TOOLS;
use crate::commands::ctx::state::{StateDir, now_secs};
use crate::commands::ctx::{CtxResult, log};

/// The documented PreToolUse deny envelope. Printed on stdout with exit 0:
/// exit 2 would block too, but it blocks on stderr text and cannot be
/// overridden, and this hook must never be the reason a session cannot make
/// progress.
pub fn pretool_output(reason: &str) -> String {
    serde_json::json!({
        "hookSpecificOutput": {
            "hookEventName": "PreToolUse",
            "permissionDecision": "deny",
            "permissionDecisionReason": reason
        }
    })
    .to_string()
}

/// Appended to a safety decision made without the repo's `.zirv/ctx.toml`.
const REPO_CONFIG_REFUSED_NOTE: &str =
    "The repo .zirv/ctx.toml contains forbidden keys and was not applied.";

/// Mark a safety envelope as decided by the trusted policy alone, because the
/// repo config was refused.
fn note_repo_config_refused(envelope: &mut serde_json::Value) {
    let Some(hook_output) = envelope
        .get_mut("hookSpecificOutput")
        .and_then(serde_json::Value::as_object_mut)
    else {
        return;
    };
    let reason = match hook_output
        .get("permissionDecisionReason")
        .and_then(serde_json::Value::as_str)
    {
        Some(existing) => format!("{existing} {REPO_CONFIG_REFUSED_NOTE}"),
        None => REPO_CONFIG_REFUSED_NOTE.to_string(),
    };
    hook_output.insert("permissionDecisionReason".to_string(), reason.into());
}

/// The `OrchestratorWrites::Advise` envelope: the write is ALLOWED, and
/// `note` rides along in the same `additionalContext` channel `safety.rs`'s
/// own identical-command guard already uses for a non-blocking note on an
/// `Allow` verdict. Never emitted for `OrchestratorWrites::Allow`, which
/// surfaces nothing at all.
fn pretool_advise_output(note: &str) -> String {
    serde_json::json!({
        "hookSpecificOutput": {
            "hookEventName": "PreToolUse",
            "permissionDecision": "allow",
            "additionalContext": note
        }
    })
    .to_string()
}

/// The PreToolUse `ask` envelope: the harness prompts instead of deciding silently.
fn pretool_ask_output(reason: &str) -> String {
    serde_json::json!({
        "hookSpecificOutput": {
            "hookEventName": "PreToolUse",
            "permissionDecision": "ask",
            "permissionDecisionReason": reason
        }
    })
    .to_string()
}

/// Explicit headless Allow for a command cleared by safety policy but absent
/// from Claude's finite native allowed-tools list. Omit rewrite and advisory
/// text for this ordinary decision.
fn pretool_safety_allow_output() -> String {
    serde_json::json!({
        "hookSpecificOutput": {
            "hookEventName": "PreToolUse",
            "permissionDecision": "allow"
        }
    })
    .to_string()
}

/// Claude replaces the whole tool input with `updatedInput`, so the rewrite
/// envelope must preserve every original field it does not change (#419).
fn pretool_rewrite_output(command: &str, reason: &str) -> String {
    serde_json::json!({
        "hookSpecificOutput": {
            "hookEventName": "PreToolUse",
            "permissionDecision": "allow",
            "permissionDecisionReason": reason,
            "updatedInput": { "command": command }
        }
    })
    .to_string()
}

// -- PreToolUse: the bare `git log` rewrite (issue #419) -------------------

/// Recognize explicit `git log` count limits; formatting flags do not
/// bound output (#419).
fn is_git_log_limit_flag(token: &str) -> bool {
    token == "-n"
        || token == "--max-count"
        || token.starts_with("--max-count=")
        || (token.len() > 1
            && token.starts_with('-')
            && token[1..].bytes().all(|b| b.is_ascii_digit()))
}

/// Cap only a bare, unlimited `git log`: appending a limit to a compound
/// shell command could change what executes (#419).
fn rewrite_bare_git_log(command: &str) -> Option<String> {
    let trimmed = command.trim();
    if trimmed.contains(['|', '>', ';']) || trimmed.contains("&&") || trimmed.contains("||") {
        return None;
    }
    // Reject shell comments, substitution, escapes and quoting before
    // appending a limit: these can change what the shell executes (#419).
    if trimmed.contains(['#', '`', '$', '\\', '\'', '"', '(', '{']) {
        return None;
    }
    let mut tokens = trimmed.split_whitespace();
    if tokens.next() != Some("git") || tokens.next() != Some("log") {
        return None;
    }
    if tokens.any(is_git_log_limit_flag) {
        return None;
    }
    Some(format!("{trimmed} -n 50"))
}

/// Rewrite bare `git log` or explicitly allow a proven headless safety
/// Allow that static native tool lists would otherwise deny. Use the
/// stricter headless default when permission mode is absent, and fail open
/// on uncertain input (#419).
fn run_pretool_bash_rewrite<W: Write>(
    w: &mut W,
    payload: &PreToolPayload,
    env: EnvLookup<'_>,
    attested_verdict: Option<crate::commands::ctx::safety::Verdict>,
) -> CtxResult<i32> {
    let command = payload.tool_input.command.trim();
    if command.is_empty() {
        return Ok(0);
    }
    // Gate rewrites and explicit Allow on the full attested verdict; current
    // policy may have widened since the stricter launch snapshot was pinned.
    if attested_verdict != Some(crate::commands::ctx::safety::Verdict::Allow) {
        return Ok(0);
    }

    if let Some(rewritten) = rewrite_bare_git_log(command) {
        let reason = format!(
            "zirv rewrite: bare `git log` capped at 50 entries (`{command}` -> `{rewritten}`)"
        );
        let _ = writeln!(w, "{}", pretool_rewrite_output(&rewritten, &reason));

        // Best-effort, matching every other decision log write on this path:
        // a row that fails to write costs an operator one audit-log entry,
        // never a hook failure.
        if let Ok(state) = StateDir::resolve(env) {
            let session = crate::commands::ctx::mail::session_identity(env)
                .unwrap_or_else(|| payload.session_id.clone());
            let _ = log::append(
                &state,
                &log::Decision {
                    ts: now_secs(),
                    session: &session,
                    verb: "hook",
                    verdict: "n/a",
                    score: 0,
                    action: "rewrite",
                    detail: &format!("{command} -> {rewritten}"),
                    observed_at: None,
                },
            );
        }
        return Ok(0);
    }

    // Explicit Allow lets a headless command cleared by safety policy pass
    // Claude's finite native allowed-tools list. Repo config cannot widen
    // this policy default (#419).
    if env(crate::commands::ctx::adapters::HEADLESS_ENV).as_deref() == Some("1") {
        let _ = writeln!(w, "{}", pretool_safety_allow_output());

        // Best-effort, matching every other decision log write on this
        // path: a row that fails to write costs an operator one audit-log
        // entry, never a hook failure.
        if let Ok(state) = StateDir::resolve(env) {
            let session = crate::commands::ctx::mail::session_identity(env)
                .unwrap_or_else(|| payload.session_id.clone());
            let _ = log::append(
                &state,
                &log::Decision {
                    ts: now_secs(),
                    session: &session,
                    verb: "hook",
                    verdict: "allow",
                    score: 0,
                    action: "allow",
                    detail: command,
                    observed_at: None,
                },
            );
        }
    }
    Ok(0)
}

/// Run consolidated Bash/PowerShell safety in-process, keeping the safety
/// verdict authoritative. For Bash Allow, merge the rewrite into the one
/// JSON response; Ask/Deny remain safety decisions. Config errors fail open
/// and do not suppress other hook work (#769).
fn run_pretool_bash_or_powershell<W: Write>(
    w: &mut W,
    stdin: &str,
    payload: &PreToolPayload,
    env: EnvLookup<'_>,
) -> CtxResult<i32> {
    // A repo-forbidden config is a security refusal, never a reason to go
    // silent: evaluate with the trusted layers alone, and ask if even that
    // fails. Every other load error still fails open (#769).
    let (cfg, repo_config_refused) = match CtxConfig::load(Path::new("."), env) {
        Ok(cfg) => (Some(cfg), false),
        Err(err) if crate::commands::ctx::config::is_repo_forbidden(err.as_ref()) => {
            match CtxConfig::load_trusted_only(Path::new("."), env) {
                Ok(cfg) => (Some(cfg), true),
                Err(_) => {
                    let _ = writeln!(
                        w,
                        "{}",
                        pretool_ask_output(&format!(
                            "{REPO_CONFIG_REFUSED_NOTE} The trusted config could not be loaded either."
                        ))
                    );
                    return Ok(0);
                }
            }
        }
        Err(_) => (None, false),
    };
    // Capture the attested verdict alongside its rendered envelope so a
    // silent `dontAsk` Ask cannot trigger a separate explicit Allow.
    let mut safety_buf: Vec<u8> = Vec::new();
    let attested_verdict = cfg.as_ref().and_then(|cfg| {
        crate::commands::ctx::safety::run_check_hook_with_verdict(cfg, &mut safety_buf, stdin, env)
            .ok()
            .flatten()
    });
    let mut safety_envelope = parsed_json_envelope(safety_buf);
    if repo_config_refused && let Some(envelope) = safety_envelope.as_mut() {
        note_repo_config_refused(envelope);
    }

    let is_deny_or_ask = safety_envelope.as_ref().is_some_and(|value| {
        matches!(
            value
                .pointer("/hookSpecificOutput/permissionDecision")
                .and_then(serde_json::Value::as_str),
            Some("deny") | Some("ask")
        )
    });
    // A real `deny`/`ask` is the final word -- `run_pretool_bash_rewrite`'s
    // own, independent verdict would only ever have agreed (see this
    // function's own doc comment), so there is nothing else to layer on.
    if is_deny_or_ask || payload.tool_name != "Bash" {
        if let Some(envelope) = &safety_envelope {
            let _ = writeln!(w, "{envelope}");
        }
        return Ok(0);
    }

    let mut rewrite_buf: Vec<u8> = Vec::new();
    run_pretool_bash_rewrite(&mut rewrite_buf, payload, env, attested_verdict)?;
    let rewrite_envelope = parsed_json_envelope(rewrite_buf);

    match (safety_envelope, rewrite_envelope) {
        (Some(mut safety), Some(rewrite)) => {
            // Claude consumes one hook envelope; merge the bounded-log rewrite
            // with Allow so neither decision is lost to a competing response.
            if let Some(updated_input) = rewrite.pointer("/hookSpecificOutput/updatedInput") {
                safety["hookSpecificOutput"]["updatedInput"] = updated_input.clone();
            }
            if let Some(reason) = rewrite.pointer("/hookSpecificOutput/permissionDecisionReason") {
                safety["hookSpecificOutput"]["permissionDecisionReason"] = reason.clone();
                if repo_config_refused {
                    note_repo_config_refused(&mut safety);
                }
            }
            let _ = writeln!(w, "{safety}");
        }
        (Some(safety), None) => {
            let _ = writeln!(w, "{safety}");
        }
        (None, Some(rewrite)) => {
            let _ = writeln!(w, "{rewrite}");
        }
        (None, None) => {}
    }
    Ok(0)
}

/// Treat invalid or empty hook output as silence so a malformed optional
/// decision cannot interrupt the tool call.
fn parsed_json_envelope(buf: Vec<u8>) -> Option<serde_json::Value> {
    let text = String::from_utf8(buf).ok()?;
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return None;
    }
    serde_json::from_str(trimmed).ok()
}

/// Run independent dispatch, repo-write, safety and reuse guards against
/// one payload, preserving the strongest decision in a single response.
/// Fails open on every path. Nothing here may `unwrap`, `expect` or return
/// `Err` -- the release profile is `panic = "abort"`, and a hook that
/// aborts takes the tool call with it.
pub fn run_pretool<W: Write>(w: &mut W, stdin: &str, env: EnvLookup<'_>) -> CtxResult<i32> {
    let Ok(payload) = PreToolPayload::parse(stdin) else {
        return Ok(0);
    };

    // A new PreToolUse call proves any previous permission prompt ended;
    // clear its attention latch before guard-specific early returns (#456).
    if let Ok(state) = StateDir::resolve(env) {
        clear_resolved_approval(
            &state,
            &attention_short(env, &payload.session_id),
            format!("permission resolved: {}", payload.tool_name),
            now_secs(),
        );
    }

    if crate::commands::ctx::lifecycle::SUBAGENT_TOOLS.contains(&payload.tool_name.as_str()) {
        crate::commands::ctx::graph::record_agent_dispatch(
            env,
            &payload.session_id,
            &payload.tool_use_id,
            &payload.agent_id,
            resolved_cwd(&payload).as_deref(),
        );
    }

    if let Some(seat) = env(adapters::SEAT_MODEL_ENV)
        && let Some(reason) = pretool_decision(Some(&seat), &payload)
    {
        if let Some(output) = dispatch_tier_override(&seat, &payload, stdin, env) {
            let _ = writeln!(w, "{output}");
            return Ok(0);
        }
        let _ = writeln!(w, "{}", pretool_output(&reason));
        return Ok(0);
    }

    // Every supervised seat may dispatch a subagent that lacks its parent's
    // skill index; attach the pointer regardless of seat role (#539).
    if let Some(output) = skill_pointer_override(&payload, stdin, env) {
        let _ = writeln!(w, "{output}");
        return Ok(0);
    }

    // Bash/PowerShell take the dedicated safety path; file-write guards
    // inspect only structured modification tools (#419, #769).
    if matches!(payload.tool_name.as_str(), "Bash" | "PowerShell") {
        return run_pretool_bash_or_powershell(w, stdin, &payload, env);
    }

    if !FILE_MODIFICATION_TOOLS.contains(&payload.tool_name.as_str()) {
        return Ok(0);
    }
    let Some(cwd) = resolved_cwd(&payload) else {
        return Ok(0);
    };
    let role = env(adapters::SEAT_ROLE_ENV);
    let cfg = cfg_or_operator_only_gate(&cwd, env);
    let posture = crate::commands::ctx::lifecycle::orchestrator_write_posture(&cfg);
    let session = crate::commands::ctx::mail::session_identity(env)
        .unwrap_or_else(|| payload.session_id.clone());

    // Probe reuse before the orchestrator write guard returns for other
    // seat roles (#406).
    let reuse_note = reuse_advice(&payload, &cwd, &cfg, env, &session);

    // The scope-creep guard's own checkpoint is likewise independent of the
    // write guard below -- every seat gets it, orchestrator or not.
    let checkpoint_note = scope_checkpoint_note(&payload, &cwd, &cfg, env);

    let outcome = orchestrator_write_decision(role.as_deref(), &payload, &cwd, env, posture);
    match &outcome {
        // The deny path is untouched by the reuse probe: a refused write has
        // nothing to reuse yet, and an advisory riding along on a denial
        // would only dilute the reason.
        Some(OrchestratorWriteOutcome::Deny(reason)) => {
            let _ = writeln!(w, "{}", pretool_output(reason));
        }
        other => {
            let advisory = match other {
                Some(OrchestratorWriteOutcome::Advise(note))
                    if orchestrator_advisory_should_surface(env, &session) =>
                {
                    Some(note.as_str())
                }
                _ => None,
            };
            // One envelope, however many notes: claude reads a single
            // `additionalContext` per hook, so a second `writeln!` would
            // throw one of them away.
            if let Some(note) =
                join_advisory_notes(&[advisory, reuse_note.as_deref(), checkpoint_note.as_deref()])
            {
                let _ = writeln!(w, "{}", pretool_advise_output(&note));
            }
            // Persist the checkpoint only when emitted; a denied call must
            // not spend the one note an allowed edit still needs.
            if checkpoint_note.is_some() {
                scope_checkpoint_mark_shown(&payload, env);
            }
        }
    }

    // Best-effort: a block record that fails to write costs an operator one
    // audit-log row, never a hook failure -- the decision above already
    // stands regardless.
    if let Some(outcome) = &outcome
        && let Ok(state) = StateDir::resolve(env)
    {
        let target = normalized_write_target(&payload, &cwd).unwrap_or_default();
        let _ = log::append_orchestrator_block(
            &state,
            &log::OrchestratorBlock {
                ts: now_secs(),
                session: &session,
                tool: &payload.tool_name,
                target: &target.display().to_string(),
                reason: "repository write",
                outcome: outcome.log_label(),
            },
        );
    }
    Ok(0)
}

/// The non-blocking notes `run_pretool` can produce, joined into the one
/// `additionalContext` string claude reads -- `None` when none of them
/// fired. Never drops one for another: claude reads a single
/// `additionalContext` per hook, so every note that fired rides in the same
/// envelope, newline-separated.
fn join_advisory_notes(notes: &[Option<&str>]) -> Option<String> {
    let joined = notes
        .iter()
        .filter_map(|note| *note)
        .collect::<Vec<_>>()
        .join("\n");
    (!joined.is_empty()).then_some(joined)
}

/// Probe a prospective write for reusable work independently of seat
/// posture, and record one decision-log row (#406).
fn reuse_advice(
    payload: &PreToolPayload,
    cwd: &Path,
    cfg: &CtxConfig,
    env: EnvLookup<'_>,
    session: &str,
) -> Option<String> {
    let target = normalized_write_target(payload, cwd)?;
    let repo = crate::commands::ctx::lifecycle::repo_root_for_target(&target)?;
    let (action, detail, note) = match crate::commands::ctx::reuse::evaluate(
        &repo,
        &target,
        payload,
        &cfg.hooks.reuse_exclude,
    ) {
        crate::commands::ctx::reuse::Outcome::Nothing => return None,
        crate::commands::ctx::reuse::Outcome::Skipped(reason) => {
            ("reuse-probe-skipped", reason, None)
        }
        crate::commands::ctx::reuse::Outcome::Advice(note) => {
            ("reuse-probe", target.display().to_string(), Some(note))
        }
    };
    if let Ok(state) = StateDir::resolve(env) {
        let _ = log::append(
            &state,
            &log::Decision {
                ts: now_secs(),
                session,
                verb: "hook",
                verdict: "n/a",
                score: 0,
                action,
                detail: &detail,
                observed_at: None,
            },
        );
    }
    note
}

#[cfg(test)]
mod tests {
    use super::super::agent::run_pretool_for_agent;
    use super::super::pretool_guard::{ORCHESTRATOR_ADVISORY_RATE, SKILL_POINTER_NOTE};
    use super::super::prompt::run_prompt;
    use super::super::tests::{
        SCOPE_GUARD_T24_PROMPT, orchestrator_pretool_stdin, orchestrator_repo, pretool_stdin,
        scope_guard_prompt_stdin,
    };
    use super::*;

    #[test]
    fn the_deny_output_matches_the_documented_pretooluse_shape() {
        let out = pretool_output("because");
        let parsed: serde_json::Value = serde_json::from_str(&out).expect("valid json");
        assert_eq!(
            parsed["hookSpecificOutput"]["hookEventName"], "PreToolUse",
            "exact key casing matters: {out}"
        );
        assert_eq!(
            parsed["hookSpecificOutput"]["permissionDecision"], "deny",
            "got {out}"
        );
        assert_eq!(
            parsed["hookSpecificOutput"]["permissionDecisionReason"], "because",
            "got {out}"
        );
    }

    #[test]
    fn run_pretool_denies_a_fork_end_to_end_and_still_exits_zero() {
        let env: std::collections::HashMap<String, String> = [(
            crate::commands::ctx::adapters::SEAT_MODEL_ENV.to_string(),
            "fable".to_string(),
        )]
        .into();
        let mut out = Vec::new();
        let code = run_pretool(
            &mut out,
            &pretool_stdin(
                "Agent",
                serde_json::json!({"subagent_type": "fork", "prompt": "do the thing"}),
            ),
            &|k| env.get(k).cloned(),
        )
        .expect("never errors");
        assert_eq!(code, 0, "exit 2 would block on stderr text instead of json");

        let printed = String::from_utf8(out).expect("utf8");
        let parsed: serde_json::Value = serde_json::from_str(printed.trim()).expect("json");
        assert_eq!(parsed["hookSpecificOutput"]["permissionDecision"], "deny");
    }

    // (issue #539 chunk F): this used to assert plain silence for an allowed
    // dispatch -- see `run_pretool_allows_a_cheap_dispatch_with_the_skill_
    // pointer_appended` below, which covers the same scenario now that this
    // path appends the skill-library pointer.

    #[test]
    fn run_pretool_allows_a_cheap_dispatch_with_the_skill_pointer_appended() {
        let env: std::collections::HashMap<String, String> = [
            (
                crate::commands::ctx::adapters::SEAT_MODEL_ENV.to_string(),
                "fable".to_string(),
            ),
            (SESSION_ENV.to_string(), "zirv-sess-pointer".to_string()),
        ]
        .into();
        let mut out = Vec::new();
        let code = run_pretool(
            &mut out,
            &pretool_stdin(
                "Agent",
                serde_json::json!({"subagent_type": "general-purpose", "model": "sonnet", "prompt": "do the thing"}),
            ),
            &|k| env.get(k).cloned(),
        )
        .expect("never errors");
        assert_eq!(code, 0);
        let printed = String::from_utf8(out).expect("utf8");
        let parsed: serde_json::Value = serde_json::from_str(printed.trim()).expect("json");
        assert_eq!(parsed["hookSpecificOutput"]["permissionDecision"], "allow");
        assert!(
            parsed["hookSpecificOutput"]
                .get("additionalContext")
                .is_none()
        );
        let updated = &parsed["hookSpecificOutput"]["updatedInput"];
        assert_eq!(updated["subagent_type"], "general-purpose");
        assert_eq!(updated["model"], "sonnet");
        assert_eq!(
            updated["prompt"].as_str().expect("prompt string"),
            format!("do the thing{SKILL_POINTER_NOTE}")
        );
    }

    /// `run_pretool` never fires the pointer for a tool that is not
    /// `Agent`/`Task`, even if its `tool_input` happens to carry a `prompt`
    /// key -- `skill_pointer_override`'s own `SUBAGENT_TOOLS` gate excludes
    /// it before `append_skill_pointer` is ever reached.
    #[test]
    fn run_pretool_never_fires_the_pointer_for_a_non_subagent_tool() {
        let env: std::collections::HashMap<String, String> = [(
            SESSION_ENV.to_string(),
            "zirv-sess-non-subagent".to_string(),
        )]
        .into();
        let mut out = Vec::new();
        let code = run_pretool(
            &mut out,
            &pretool_stdin("Read", serde_json::json!({"prompt": "do the thing"})),
            &|k| env.get(k).cloned(),
        )
        .expect("never errors");
        assert_eq!(code, 0);
        assert!(out.is_empty(), "Read is not a subagent dispatch: {out:?}");
    }

    /// A dispatch with no `prompt` at all is schema drift, not a real
    /// dispatch (`PreToolInput::prompt`'s own doc comment) -- the pointer
    /// must not fire for it either.
    #[test]
    fn run_pretool_never_fires_the_pointer_when_the_dispatch_has_no_prompt() {
        let env: std::collections::HashMap<String, String> =
            [(SESSION_ENV.to_string(), "zirv-sess-no-prompt".to_string())].into();
        let mut out = Vec::new();
        let code = run_pretool(
            &mut out,
            &pretool_stdin(
                "Agent",
                serde_json::json!({"subagent_type": "general-purpose"}),
            ),
            &|k| env.get(k).cloned(),
        )
        .expect("never errors");
        assert_eq!(code, 0);
        assert!(out.is_empty(), "no prompt means no dispatch: {out:?}");
    }

    /// A prompt that already mentions "zirv skill" (a parent that briefed
    /// skills explicitly) must not get the pointer appended a second time,
    /// even though this dispatch is otherwise allowed outright.
    #[test]
    fn run_pretool_never_doubles_the_pointer_when_the_prompt_already_mentions_it() {
        let env: std::collections::HashMap<String, String> = [(
            SESSION_ENV.to_string(),
            "zirv-sess-already-briefed".to_string(),
        )]
        .into();
        let mut out = Vec::new();
        let code = run_pretool(
            &mut out,
            &pretool_stdin(
                "Agent",
                serde_json::json!({
                    "subagent_type": "general-purpose",
                    "model": "sonnet",
                    "prompt": "before starting, run zirv skill list --match \"...\""
                }),
            ),
            &|k| env.get(k).cloned(),
        )
        .expect("never errors");
        assert_eq!(code, 0);
        assert!(out.is_empty(), "already briefed on skills: {out:?}");
    }

    /// A Single/worker seat -- `SESSION_ENV` set, `SEAT_MODEL_ENV` absent
    /// (that env is orchestrator-only) -- gets the same pointer an
    /// orchestrator seat does. The whole `SEAT_MODEL_ENV` block is skipped
    /// entirely for this session, so `skill_pointer_override` is reached
    /// unconditionally on `pretool_decision` never even running.
    #[test]
    fn run_pretool_appends_the_pointer_for_a_single_or_worker_seat_with_no_seat_model_env() {
        let env: std::collections::HashMap<String, String> =
            [(SESSION_ENV.to_string(), "zirv-sess-single".to_string())].into();
        let mut out = Vec::new();
        let code = run_pretool(
            &mut out,
            &pretool_stdin(
                "Task",
                serde_json::json!({"subagent_type": "general-purpose", "prompt": "review the diff"}),
            ),
            &|k| env.get(k).cloned(),
        )
        .expect("never errors");
        assert_eq!(code, 0);
        let printed = String::from_utf8(out).expect("utf8");
        let parsed: serde_json::Value = serde_json::from_str(printed.trim()).expect("json");
        assert_eq!(parsed["hookSpecificOutput"]["permissionDecision"], "allow");
        assert_eq!(
            parsed["hookSpecificOutput"]["updatedInput"]["prompt"]
                .as_str()
                .expect("prompt string"),
            format!("review the diff{SKILL_POINTER_NOTE}")
        );
    }

    /// With neither `SEAT_MODEL_ENV` nor `SESSION_ENV` set at all, the hook
    /// stays completely silent -- a non-zirv session is never made worse, and
    /// the pointer's own gate (`SESSION_ENV`) never fires on an absent value.
    #[test]
    fn run_pretool_exits_zero_and_silent_without_the_seat_env() {
        let mut out = Vec::new();
        let code = run_pretool(
            &mut out,
            &pretool_stdin("Agent", serde_json::json!({"subagent_type": "fork"})),
            &|_| None,
        )
        .expect("never errors");
        assert_eq!(code, 0);
        assert!(out.is_empty(), "a non-zirv session is never made worse");
    }

    /// An empty `SESSION_ENV` value must read exactly like an absent one --
    /// `skill_pointer_override`'s own filter, not just `Option::is_some()`.
    #[test]
    fn run_pretool_treats_an_empty_session_env_as_absent() {
        let env: std::collections::HashMap<String, String> =
            [(SESSION_ENV.to_string(), String::new())].into();
        let mut out = Vec::new();
        let code = run_pretool(
            &mut out,
            &pretool_stdin(
                "Agent",
                serde_json::json!({"subagent_type": "general-purpose", "prompt": "do the thing"}),
            ),
            &|k| env.get(k).cloned(),
        )
        .expect("never errors");
        assert_eq!(code, 0);
        assert!(
            out.is_empty(),
            "an empty session id is not a real session: {out:?}"
        );
    }

    #[test]
    fn run_pretool_exits_zero_and_silent_on_garbage_stdin() {
        let env: std::collections::HashMap<String, String> = [(
            crate::commands::ctx::adapters::SEAT_MODEL_ENV.to_string(),
            "fable".to_string(),
        )]
        .into();
        for stdin in [
            "",
            "this is not json",
            "{",
            "[]",
            "null",
            "{\"tool_name\":\"Agent\",\"tool_input\":\"not an object\"}",
            "{\"tool_name\":42}",
        ] {
            let mut out = Vec::new();
            let code = run_pretool(&mut out, stdin, &|k| env.get(k).cloned())
                .unwrap_or_else(|_| panic!("must never error on {stdin:?}"));
            assert_eq!(code, 0, "must never block on {stdin:?}");
            assert!(out.is_empty(), "must stay silent on {stdin:?}: {out:?}");
        }
    }

    /// Issue #358 T8: the default posture is `advise`, not `deny` -- this
    /// end-to-end test pins `deny` explicitly via
    /// `ZIRV_CTX_SUPERVISE_ORCHESTRATOR_WRITES` so it keeps proving the
    /// original guard behaviour (issues #328/#334) regardless of the
    /// default. See `run_pretool_advises_an_orchestrator_edit_by_default`
    /// for the actual default-posture behaviour.
    #[test]
    fn run_pretool_denies_an_orchestrator_edit_with_no_seat_model_env_at_all() {
        let repo = orchestrator_repo();
        let env: std::collections::HashMap<String, String> = [
            (
                adapters::SEAT_ROLE_ENV.to_string(),
                "orchestrator".to_string(),
            ),
            (
                "ZIRV_CTX_SUPERVISE_ORCHESTRATOR_WRITES".to_string(),
                "deny".to_string(),
            ),
        ]
        .into();
        let mut out = Vec::new();
        let code = run_pretool(
            &mut out,
            &orchestrator_pretool_stdin(
                &repo.path().display().to_string(),
                "claude-session-id",
                "Edit",
                serde_json::json!({"file_path": repo.path().join("src/x.rs").display().to_string()}),
            ),
            &|k| env.get(k).cloned(),
        )
        .expect("never errors");
        assert_eq!(code, 0, "exit 2 would block on stderr text instead of json");

        let printed = String::from_utf8(out).expect("utf8");
        let parsed: serde_json::Value = serde_json::from_str(printed.trim()).expect("json");
        assert_eq!(parsed["hookSpecificOutput"]["permissionDecision"], "deny");
        assert!(
            parsed["hookSpecificOutput"]["permissionDecisionReason"]
                .as_str()
                .unwrap_or_default()
                .contains("orchestrator seat: dispatch a worker"),
            "a cheap-model orchestrator (no SEAT_MODEL_ENV) must still be guarded: {parsed}"
        );
    }

    #[test]
    fn run_pretool_stays_silent_for_a_worker_editing_a_repo_file() {
        let repo = orchestrator_repo();
        let env: std::collections::HashMap<String, String> =
            [(adapters::SEAT_ROLE_ENV.to_string(), "worker".to_string())].into();
        let mut out = Vec::new();
        let code = run_pretool(
            &mut out,
            &edit_payload_stdin(repo.path(), "src/x.rs"),
            &|k| env.get(k).cloned(),
        )
        .expect("never errors");
        assert_eq!(code, 0);
        assert!(out.is_empty(), "a worker must be free to edit: {out:?}");
    }

    /// Issue #537 (T3): mirrors `run_pretool_stays_silent_for_a_worker_
    /// editing_a_repo_file` for the proxy's own `PromptRole::Single` seat --
    /// `lifecycle::orchestrator_write_target`'s `role != Some("orchestrator")`
    /// gate already excludes any role but that exact string, so `"single"`
    /// falls outside the guard's scope the same way `"worker"` does, with no
    /// production code change required; this test just pins that a Single
    /// seat is never technically unable to edit files the way the operator's
    /// bug report showed it was when a direct/bounded decision was launched
    /// as a full orchestrator.
    #[test]
    fn run_pretool_stays_silent_for_a_single_seat_editing_a_repo_file() {
        let repo = orchestrator_repo();
        let env: std::collections::HashMap<String, String> =
            [(adapters::SEAT_ROLE_ENV.to_string(), "single".to_string())].into();
        let mut out = Vec::new();
        let code = run_pretool(
            &mut out,
            &edit_payload_stdin(repo.path(), "src/x.rs"),
            &|k| env.get(k).cloned(),
        )
        .expect("never errors");
        assert_eq!(code, 0);
        assert!(
            out.is_empty(),
            "a single seat must be free to edit: {out:?}"
        );
    }

    fn edit_payload_stdin(repo: &Path, relative_target: &str) -> String {
        orchestrator_pretool_stdin(
            &repo.display().to_string(),
            "claude-session-id",
            "Edit",
            serde_json::json!({"file_path": repo.join(relative_target).display().to_string()}),
        )
    }

    /// A deny appends exactly one row to `orchestrator-blocks.jsonl`, keyed
    /// by the zirv session short id (`SESSION_ENV`), not the harness's own
    /// `session_id` field.
    #[test]
    fn run_pretool_denial_appends_exactly_one_orchestrator_block_row() {
        let repo = orchestrator_repo();
        let state_dir = tempfile::tempdir().expect("state dir");
        let env: std::collections::HashMap<String, String> = [
            (
                adapters::SEAT_ROLE_ENV.to_string(),
                "orchestrator".to_string(),
            ),
            (
                crate::commands::ctx::state::STATE_ENV.to_string(),
                state_dir.path().display().to_string(),
            ),
            (SESSION_ENV.to_string(), "zirv-sess-42".to_string()),
        ]
        .into();
        let mut out = Vec::new();
        let code = run_pretool(
            &mut out,
            &edit_payload_stdin(repo.path(), "src/x.rs"),
            &|k| env.get(k).cloned(),
        )
        .expect("never errors");
        assert_eq!(code, 0);
        assert!(!out.is_empty(), "must still print the deny envelope");

        let state = StateDir::from_root(state_dir.path().to_path_buf());
        let rows = log::read_orchestrator_blocks(&state);
        assert_eq!(rows.len(), 1, "{rows:?}");
        assert_eq!(rows[0].tool, "Edit");
        assert_eq!(
            rows[0].session,
            crate::commands::ctx::sessions::short_id("zirv-sess-42")
        );
    }

    /// Issue #418: a copilot-shaped camelCase `preToolUse` payload for an
    /// `edit` by an orchestrator-role session yields the SAME
    /// `permissionDecisionReason` text claude's own `Edit` payload gets.
    /// Drives `run_pretool` (claude, unmodified) and `run_pretool_for_agent`
    /// (copilot, projected) against equivalent payloads and compares the
    /// reason strings, rather than a hand-written expectation, so a change
    /// to `orchestrator_write_deny_reason` cannot silently desync this test
    /// from production.
    #[test]
    fn copilot_projected_edit_deny_reason_matches_claude() {
        let repo = orchestrator_repo();
        let state_dir = tempfile::tempdir().expect("state dir");
        let env: std::collections::HashMap<String, String> = [
            (
                adapters::SEAT_ROLE_ENV.to_string(),
                "orchestrator".to_string(),
            ),
            (
                crate::commands::ctx::state::STATE_ENV.to_string(),
                state_dir.path().display().to_string(),
            ),
            (
                "ZIRV_CTX_SUPERVISE_ORCHESTRATOR_WRITES".to_string(),
                "deny".to_string(),
            ),
        ]
        .into();

        let mut claude_out = Vec::new();
        run_pretool(
            &mut claude_out,
            &edit_payload_stdin(repo.path(), "src/x.rs"),
            &|k| env.get(k).cloned(),
        )
        .expect("never errors");
        let claude_text = String::from_utf8(claude_out).expect("utf8");
        let claude_json: serde_json::Value =
            serde_json::from_str(claude_text.trim()).expect("json");
        assert_eq!(
            claude_json["hookSpecificOutput"]["permissionDecision"],
            "deny"
        );
        let reason = claude_json["hookSpecificOutput"]["permissionDecisionReason"]
            .as_str()
            .expect("a reason")
            .to_string();

        let target = repo.path().join("src/x.rs").display().to_string();
        let copilot_stdin = serde_json::json!({
            "sessionId": "copilot-session",
            "cwd": repo.path().display().to_string(),
            "toolName": "edit",
            "toolArgs": {"path": target},
        })
        .to_string();
        let mut copilot_out = Vec::new();
        run_pretool_for_agent(
            &mut copilot_out,
            &copilot_stdin,
            &|k| env.get(k).cloned(),
            Some("copilot"),
        )
        .expect("never errors");
        let copilot_text = String::from_utf8(copilot_out).expect("utf8");
        let copilot_json: serde_json::Value =
            serde_json::from_str(copilot_text.trim()).expect("json");
        assert_eq!(copilot_json["permissionDecision"], "deny");
        assert_eq!(copilot_json["permissionDecisionReason"], reason);
    }

    /// Issue #358 T8: the default posture is `advise`, not `deny` -- an
    /// orchestrator's own direct edit is ALLOWED, with a rate-limited
    /// advisory note riding along in `additionalContext`, and the logged
    /// row's own `outcome` is "advised". A fresh, empty home directory rules
    /// out an ambient `~/.zirv/ctx.toml` changing the posture under test.
    #[test]
    fn run_pretool_advises_an_orchestrator_edit_by_default() {
        let home = tempfile::tempdir().expect("home");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());
        let repo = orchestrator_repo();
        let state_dir = tempfile::tempdir().expect("state dir");
        let env: std::collections::HashMap<String, String> = [
            (
                adapters::SEAT_ROLE_ENV.to_string(),
                "orchestrator".to_string(),
            ),
            (
                crate::commands::ctx::state::STATE_ENV.to_string(),
                state_dir.path().display().to_string(),
            ),
        ]
        .into();
        let mut out = Vec::new();
        let code = run_pretool(
            &mut out,
            &edit_payload_stdin(repo.path(), "src/x.rs"),
            &|k| env.get(k).cloned(),
        )
        .expect("never errors");
        assert_eq!(code, 0);

        let printed = String::from_utf8(out).expect("utf8");
        let parsed: serde_json::Value = serde_json::from_str(printed.trim()).expect("json");
        assert_eq!(parsed["hookSpecificOutput"]["permissionDecision"], "allow");
        assert!(
            parsed["hookSpecificOutput"]["additionalContext"]
                .as_str()
                .unwrap_or_default()
                .contains("fine for a trivial edit; delegate substantial changes to a worker"),
            "got {parsed}"
        );

        let state = StateDir::from_root(state_dir.path().to_path_buf());
        let rows = log::read_orchestrator_blocks(&state);
        assert_eq!(rows.len(), 1, "{rows:?}");
        assert_eq!(rows[0].outcome, "advised");
    }

    // -- PreToolUse: the reuse probe (issue #406) --------------------------

    /// A checkout that already defines `foo_bar`, so the probe has something
    /// real to find.
    fn reuse_repo() -> tempfile::TempDir {
        let repo = orchestrator_repo();
        std::fs::create_dir_all(repo.path().join("src")).expect("src dir");
        std::fs::write(
            repo.path().join("src/x.rs"),
            "// header\npub fn foo_bar() -> u8 {\n    0\n}\n",
        )
        .expect("write x.rs");
        repo
    }

    fn write_payload_stdin(repo: &Path, relative_target: &str, content: &str) -> String {
        orchestrator_pretool_stdin(
            &repo.display().to_string(),
            "claude-session-id",
            "Write",
            serde_json::json!({
                "file_path": repo.join(relative_target).display().to_string(),
                "content": content,
            }),
        )
    }

    /// `run_pretool` on a plain (non-orchestrator) seat with an empty home,
    /// so only the reuse probe can print anything at all.
    fn run_pretool_stdout(stdin: &str, state_dir: &Path) -> String {
        let env: std::collections::HashMap<String, String> = [(
            crate::commands::ctx::state::STATE_ENV.to_string(),
            state_dir.display().to_string(),
        )]
        .into();
        let mut out = Vec::new();
        let code = run_pretool(&mut out, stdin, &|k| env.get(k).cloned()).expect("never errors");
        assert_eq!(code, 0);
        String::from_utf8(out).expect("utf8")
    }

    /// End to end: a `Write` that re-declares an existing `fn` is ALLOWED,
    /// with the existing definition's own `path:line` in
    /// `additionalContext`.
    #[test]
    fn run_pretool_names_an_existing_definition_a_write_re_adds() {
        let home = tempfile::tempdir().expect("home");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());
        let repo = reuse_repo();
        let state_dir = tempfile::tempdir().expect("state dir");

        let printed = run_pretool_stdout(
            &write_payload_stdin(
                repo.path(),
                "src/y.rs",
                "pub fn foo_bar() -> u8 {\n    1\n}\n",
            ),
            state_dir.path(),
        );
        let parsed: serde_json::Value = serde_json::from_str(printed.trim()).expect("json");
        assert_eq!(parsed["hookSpecificOutput"]["permissionDecision"], "allow");
        let note = parsed["hookSpecificOutput"]["additionalContext"]
            .as_str()
            .unwrap_or_default();
        assert!(
            note.contains("src/x.rs:") && note.contains("foo_bar"),
            "got {parsed}"
        );
    }

    /// A genuinely new name is not a duplication, so nothing is printed --
    /// the probe stays silent rather than reporting that it looked.
    #[test]
    fn run_pretool_says_nothing_about_a_genuinely_new_definition() {
        let home = tempfile::tempdir().expect("home");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());
        let repo = reuse_repo();
        let state_dir = tempfile::tempdir().expect("state dir");

        let printed = run_pretool_stdout(
            &write_payload_stdin(
                repo.path(),
                "src/y.rs",
                "pub fn quux_widget() -> u8 {\n    1\n}\n",
            ),
            state_dir.path(),
        );
        assert!(printed.is_empty(), "expected silence, got {printed}");
    }

    /// `hooks.reuse_exclude` narrows the probe's own scope: a write under an
    /// excluded prefix is neither probed nor advised on.
    #[test]
    fn run_pretool_skips_a_write_under_an_excluded_prefix() {
        let home = tempfile::tempdir().expect("home");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());
        let repo = reuse_repo();
        std::fs::create_dir_all(repo.path().join(".zirv")).expect("zirv dir");
        std::fs::write(
            repo.path().join(".zirv/ctx.toml"),
            "[hooks]\nreuse_exclude = [\"src\"]\n",
        )
        .expect("write ctx.toml");
        let state_dir = tempfile::tempdir().expect("state dir");

        let printed = run_pretool_stdout(
            &write_payload_stdin(
                repo.path(),
                "src/y.rs",
                "pub fn foo_bar() -> u8 {\n    1\n}\n",
            ),
            state_dir.path(),
        );
        assert!(printed.is_empty(), "expected silence, got {printed}");
    }

    /// `OrchestratorWrites::Allow`: the write is silent (nothing printed at
    /// all) but still logged, so `zirv ctx status` can still count it.
    #[test]
    fn run_pretool_allow_posture_is_silent_but_still_logs() {
        let repo = orchestrator_repo();
        let state_dir = tempfile::tempdir().expect("state dir");
        let env: std::collections::HashMap<String, String> = [
            (
                adapters::SEAT_ROLE_ENV.to_string(),
                "orchestrator".to_string(),
            ),
            (
                crate::commands::ctx::state::STATE_ENV.to_string(),
                state_dir.path().display().to_string(),
            ),
            (
                "ZIRV_CTX_SUPERVISE_ORCHESTRATOR_WRITES".to_string(),
                "allow".to_string(),
            ),
        ]
        .into();
        let mut out = Vec::new();
        let code = run_pretool(
            &mut out,
            &edit_payload_stdin(repo.path(), "src/x.rs"),
            &|k| env.get(k).cloned(),
        )
        .expect("never errors");
        assert_eq!(code, 0);
        assert!(out.is_empty(), "allow posture must print nothing: {out:?}");

        let state = StateDir::from_root(state_dir.path().to_path_buf());
        let rows = log::read_orchestrator_blocks(&state);
        assert_eq!(rows.len(), 1, "{rows:?}");
        assert_eq!(rows[0].outcome, "allowed");
    }

    /// Issue #358 T8: the advisory note surfaces on the 1st write, stays
    /// silent for the next `ORCHESTRATOR_ADVISORY_RATE - 1`, then surfaces
    /// again on the `ORCHESTRATOR_ADVISORY_RATE`th -- every write is still
    /// logged regardless.
    #[test]
    fn run_pretool_advisory_note_is_rate_limited_across_repeated_writes() {
        let repo = orchestrator_repo();
        let state_dir = tempfile::tempdir().expect("state dir");
        let env: std::collections::HashMap<String, String> = [
            (
                adapters::SEAT_ROLE_ENV.to_string(),
                "orchestrator".to_string(),
            ),
            (
                crate::commands::ctx::state::STATE_ENV.to_string(),
                state_dir.path().display().to_string(),
            ),
            (SESSION_ENV.to_string(), "zirv-sess-rate".to_string()),
        ]
        .into();

        let mut surfaced = Vec::new();
        for n in 0..ORCHESTRATOR_ADVISORY_RATE + 1 {
            let mut out = Vec::new();
            let code = run_pretool(
                &mut out,
                &edit_payload_stdin(repo.path(), &format!("src/x{n}.rs")),
                &|k| env.get(k).cloned(),
            )
            .expect("never errors");
            assert_eq!(code, 0);
            surfaced.push(!out.is_empty());
        }

        let mut expected = vec![false; ORCHESTRATOR_ADVISORY_RATE + 1];
        expected[0] = true;
        expected[ORCHESTRATOR_ADVISORY_RATE] = true;
        assert_eq!(
            surfaced,
            expected,
            "the note must surface on write 1 and write {}, silent between",
            ORCHESTRATOR_ADVISORY_RATE + 1
        );

        let state = StateDir::from_root(state_dir.path().to_path_buf());
        let rows = log::read_orchestrator_blocks(&state);
        assert_eq!(
            rows.len(),
            ORCHESTRATOR_ADVISORY_RATE + 1,
            "every write is logged regardless of whether the note surfaced: {rows:?}"
        );
        assert!(rows.iter().all(|row| row.outcome == "advised"));
    }

    // -- PreToolUse: the bare `git log` rewrite (issue #419) ----------------

    fn bash_pretool_stdin(cwd: &str, command: &str) -> String {
        orchestrator_pretool_stdin(
            cwd,
            "claude-session-id",
            "Bash",
            serde_json::json!({"command": command}),
        )
    }

    /// End to end: an unlimited `git log` gets `-n 50` appended via
    /// `updatedInput`, and the reason names the rewrite.
    #[test]
    fn run_pretool_rewrites_a_bare_git_log_with_a_cap() {
        let repo = orchestrator_repo();
        let mut out = Vec::new();
        let code = run_pretool(
            &mut out,
            &bash_pretool_stdin(&repo.path().display().to_string(), "git log"),
            &|_| None,
        )
        .expect("never errors");
        assert_eq!(code, 0);

        let printed = String::from_utf8(out).expect("utf8");
        let parsed: serde_json::Value = serde_json::from_str(printed.trim()).expect("json");
        assert_eq!(parsed["hookSpecificOutput"]["permissionDecision"], "allow");
        assert_eq!(
            parsed["hookSpecificOutput"]["updatedInput"]["command"],
            "git log -n 50"
        );
        assert!(
            parsed["hookSpecificOutput"]["permissionDecisionReason"]
                .as_str()
                .unwrap_or_default()
                .contains("rewrite"),
            "got {parsed}"
        );
    }

    /// F7 (wrapper-overhead benchmark, 2026-09-24): a heredoc -- one of the
    /// exact shapes the benchmark's 46 denials over 40 runs named -- matches
    /// no specific `safety.allow`/`safety.deny`/`safety.ask` rule at all, so
    /// its verdict is purely the operator's own configured `[safety]
    /// default`. Headlessly, with that default set to `allow`, claude's own
    /// `--permission-mode dontAsk` plus its static `--allowedTools` list
    /// would otherwise deny this outright for not being literally on that
    /// list -- this hook must instead name the decision explicitly, with no
    /// rewrite (`updatedInput` absent) since this command shape is untouched.
    ///
    /// Issue #769: `permission_mode: "dontAsk"` here (unlike `bash_pretool_
    /// stdin`'s own `"default"`) is deliberate -- it is what makes the merged
    /// safety layer itself go SILENT for this `Allow` verdict (`safety::
    /// hook_output_with_extras`'s own `dont_ask` branch), which is exactly
    /// what proves this test is still pinning `run_pretool_bash_rewrite`'s
    /// own F7 explicit-allow logic and not merely re-observing the safety
    /// layer's now-merged-in explicit allow (which fires regardless of F7,
    /// whenever `permission_mode` is not `"dontAsk"` -- see the interactive
    /// sibling test below).
    #[test]
    fn run_pretool_bash_names_an_explicit_allow_headlessly_when_the_operator_default_is_allow() {
        let repo = orchestrator_repo();
        let env: std::collections::HashMap<String, String> = [
            (
                crate::commands::ctx::adapters::HEADLESS_ENV.to_string(),
                "1".to_string(),
            ),
            ("ZIRV_CTX_SAFETY_DEFAULT".to_string(), "allow".to_string()),
        ]
        .into();
        let stdin = serde_json::json!({
            "session_id": "claude-session-id",
            "transcript_path": "/tmp/t.jsonl",
            "cwd": repo.path().display().to_string(),
            "permission_mode": "dontAsk",
            "hook_event_name": "PreToolUse",
            "tool_name": "Bash",
            "tool_input": {"command": "cat > f.py <<'EOF'\nprint(1)\nEOF"},
            "tool_use_id": "toolu_01ABC123",
        })
        .to_string();
        let mut out = Vec::new();
        let code = run_pretool(&mut out, &stdin, &|k| env.get(k).cloned()).expect("never errors");
        assert_eq!(code, 0);

        let printed = String::from_utf8(out).expect("utf8");
        let parsed: serde_json::Value = serde_json::from_str(printed.trim()).expect("json");
        assert_eq!(parsed["hookSpecificOutput"]["permissionDecision"], "allow");
        assert!(
            parsed["hookSpecificOutput"].get("updatedInput").is_none(),
            "a heredoc is never rewritten: {parsed}"
        );
    }

    /// F1 (codex review, cff7ff57 follow-up): a session launched with the
    /// pinned snapshot's `default = Ask` (issue #139's attestation), whose
    /// home policy was then widened to `allow` mid-session. The full
    /// attested check (`evaluate_with_attestation_evidence`) correctly keeps
    /// the snapshot's stricter `Ask` -- which `hook_output_with_extras` then
    /// silences under headless `dontAsk` by design (deferring to claude's
    /// own permission flow) -- so `run_pretool_bash_rewrite`'s own F7
    /// explicit-allow fallback must NOT independently re-evaluate against
    /// today's (widened) policy and print its own `allow`: that would bypass
    /// the pinned verdict entirely. Silence here is the correct, fail-closed
    /// outcome: claude's own `dontAsk` plus its static `--allowedTools` list
    /// denies anything not explicitly allowed.
    #[test]
    fn run_pretool_bash_headless_allow_never_bypasses_a_pinned_stricter_snapshot() {
        let repo = orchestrator_repo();
        let snapshot_dir = tempfile::tempdir().expect("tempdir");
        let snapshot_path = snapshot_dir.path().join("policy.json");
        // The launch-time snapshot: the shipped default policy, whose
        // headless `default` is `Ask` (see `SafetyPolicy::default`'s own doc
        // comment).
        let launch_policy = crate::commands::ctx::safety::SafetyPolicy::default();
        std::fs::write(
            &snapshot_path,
            serde_json::to_string(&launch_policy).expect("serializes"),
        )
        .expect("writes snapshot");
        let fingerprint =
            crate::commands::ctx::safety::policy_fingerprint(&launch_policy).expect("fingerprints");

        let env: std::collections::HashMap<String, String> = [
            (
                crate::commands::ctx::adapters::HEADLESS_ENV.to_string(),
                "1".to_string(),
            ),
            // The operator widened the home policy AFTER this session's own
            // launch pinned `ask` -- exactly issue #139's divergence case.
            ("ZIRV_CTX_SAFETY_DEFAULT".to_string(), "allow".to_string()),
            (
                crate::commands::ctx::safety::POLICY_FINGERPRINT_ENV.to_string(),
                fingerprint,
            ),
            (
                crate::commands::ctx::safety::POLICY_SNAPSHOT_ENV.to_string(),
                snapshot_path.to_str().expect("utf8 path").to_string(),
            ),
        ]
        .into();
        let stdin = serde_json::json!({
            "session_id": "claude-session-id",
            "transcript_path": "/tmp/t.jsonl",
            "cwd": repo.path().display().to_string(),
            "permission_mode": "dontAsk",
            "hook_event_name": "PreToolUse",
            "tool_name": "Bash",
            "tool_input": {"command": "cat > f.py <<'EOF'\nprint(1)\nEOF"},
            "tool_use_id": "toolu_01ABC123",
        })
        .to_string();
        let mut out = Vec::new();
        let code = run_pretool(&mut out, &stdin, &|k| env.get(k).cloned()).expect("never errors");
        assert_eq!(code, 0);

        let printed = String::from_utf8(out).expect("utf8");
        assert!(
            printed.trim().is_empty(),
            "a pinned-stricter Ask must stay silent (fail-closed under dontAsk), never an \
             explicit allow from the un-pinned fallback: {printed}"
        );
    }

    /// The same operator default, but NOT headless. Before issue #769 this
    /// meant `run_pretool_bash_rewrite`'s own F7 explicit-allow logic (gated
    /// on `HEADLESS_ENV`) stayed silent -- the SEPARATE `zirv ctx safety
    /// check` hook was the only thing that ever explained an interactive
    /// allow, invisible to `run_pretool` alone. Now that `run_pretool` runs
    /// that exact check itself for `Bash`/`PowerShell`, the merged decision
    /// is STILL an explicit `allow` with no `updatedInput` (a heredoc is
    /// never rewritten) -- sourced from the safety layer this time, not the
    /// F7 path, which independently stays silent here exactly as before.
    #[test]
    fn run_pretool_bash_names_an_explicit_allow_interactively_too_via_the_safety_check() {
        let repo = orchestrator_repo();
        let env: std::collections::HashMap<String, String> =
            [("ZIRV_CTX_SAFETY_DEFAULT".to_string(), "allow".to_string())].into();
        let mut out = Vec::new();
        let code = run_pretool(
            &mut out,
            &bash_pretool_stdin(
                &repo.path().display().to_string(),
                "cat > f.py <<'EOF'\nprint(1)\nEOF",
            ),
            &|k| env.get(k).cloned(),
        )
        .expect("never errors");
        assert_eq!(code, 0);
        let printed = String::from_utf8(out).expect("utf8");
        assert_allow_with_no_rewrite(
            &printed,
            "an interactive allow now comes from the merged safety check, not silence",
        );
    }

    /// A command the operator's own `[safety] deny`/`ask` rules (or
    /// `SHIPPED_POSTURE_DENY`) actually refuse must never receive the F7
    /// explicit allow, even headlessly with a permissive default -- a
    /// specific `deny`/`ask` rule always outranks the unmatched-command
    /// `default` (`safety::evaluate`'s own precedence). `rm -rf /` matches
    /// the shipped `[safety] ask` rule `rm -rf *` (not a hard `deny`), so the
    /// real verdict here is `ask`, carrying the `BLOCKED: rm /` instruction
    /// suffix `blocked_instruction_suffix` appends to any real `deny`/`ask`.
    /// Issue #769: this is exactly the "safety deny/ask path" consolidation
    /// must keep intact -- the merged hook now carries this REAL verdict
    /// itself (before, only the separate `zirv ctx safety check` process
    /// evaluated it, invisible to `run_pretool` alone), and it must never
    /// carry `updatedInput` alongside a real `ask`/`deny`.
    #[test]
    fn run_pretool_bash_never_allows_a_denied_command_even_headlessly() {
        let repo = orchestrator_repo();
        let env: std::collections::HashMap<String, String> = [
            (
                crate::commands::ctx::adapters::HEADLESS_ENV.to_string(),
                "1".to_string(),
            ),
            ("ZIRV_CTX_SAFETY_DEFAULT".to_string(), "allow".to_string()),
        ]
        .into();
        let mut out = Vec::new();
        let code = run_pretool(
            &mut out,
            &bash_pretool_stdin(&repo.path().display().to_string(), "rm -rf /"),
            &|k| env.get(k).cloned(),
        )
        .expect("never errors");
        assert_eq!(code, 0);
        let printed = String::from_utf8(out).expect("utf8");
        assert!(
            !printed.is_empty(),
            "a genuinely dangerous command must still be flagged, not silent: {printed}"
        );
        let parsed: serde_json::Value = serde_json::from_str(printed.trim()).expect("json");
        assert_eq!(
            parsed["hookSpecificOutput"]["permissionDecision"], "ask",
            "got {parsed}"
        );
        assert!(
            parsed["hookSpecificOutput"].get("updatedInput").is_none(),
            "a real ask/deny must never carry updatedInput: {parsed}"
        );
    }

    /// Issue #769: `PowerShell` used to get NOTHING from `zirv ctx hook
    /// pretool` -- only `Bash` ever reached `run_pretool_bash_rewrite`, and
    /// the separate `zirv ctx safety check` hook (matcher `Bash|PowerShell`)
    /// was the only thing that ever evaluated a `PowerShell` call at all.
    /// Now the consolidated hook runs that exact check for `PowerShell` too.
    /// `"git log"` as the command text is deliberate, not a claim about real
    /// PowerShell syntax: the safety layer pattern-matches command TEXT
    /// regardless of `tool_name`, and this is the same already-proven-Allow
    /// command every `git log` rewrite test above already relies on.
    #[test]
    fn run_pretool_now_evaluates_powershell_directly_where_it_used_to_stay_silent() {
        let repo = orchestrator_repo();
        let mut out = Vec::new();
        let code = run_pretool(
            &mut out,
            &orchestrator_pretool_stdin(
                &repo.path().display().to_string(),
                "claude-session-id",
                "PowerShell",
                serde_json::json!({"command": "git log"}),
            ),
            &|_| None,
        )
        .expect("never errors");
        assert_eq!(code, 0);
        let printed = String::from_utf8(out).expect("utf8");
        assert!(
            !printed.is_empty(),
            "issue #769: PowerShell now gets the safety check's own explicit decision, not \
             silence"
        );
        let parsed: serde_json::Value = serde_json::from_str(printed.trim()).expect("json");
        assert_eq!(parsed["hookSpecificOutput"]["permissionDecision"], "allow");
        assert!(
            parsed["hookSpecificOutput"].get("updatedInput").is_none(),
            "the Bash-only git-log rewrite must never apply to PowerShell: {parsed}"
        );
    }

    /// Asserts `printed` is an `allow` decision with no `updatedInput` --
    /// issue #769: since the consolidated hook now also runs the safety
    /// check itself, and `bash_pretool_stdin`'s `permission_mode: "default"`
    /// is not `"dontAsk"`, every `Allow` verdict now prints an explicit
    /// `allow` (the same "sole prompting gate" behavior the standalone
    /// safety hook already had interactively before this consolidation --
    /// see `safety::hook_output_with_extras`'s own doc comment). A shape the
    /// rewrite would touch NOT firing is what these tests actually pin: no
    /// `updatedInput` at all.
    fn assert_allow_with_no_rewrite(printed: &str, context: &str) {
        assert!(
            !printed.is_empty(),
            "{context}: expected an explicit allow, got silence"
        );
        let parsed: serde_json::Value = serde_json::from_str(printed.trim()).expect("json");
        assert_eq!(
            parsed["hookSpecificOutput"]["permissionDecision"], "allow",
            "{context}: {parsed}"
        );
        assert!(
            parsed["hookSpecificOutput"].get("updatedInput").is_none(),
            "{context}: no rewrite should have fired: {parsed}"
        );
    }

    /// A `git log` that already names its own limit -- `-n`, `--max-count=`,
    /// or a bare `-<digits>` -- gets no `updatedInput`: the rewrite never
    /// fires. It still gets the merged hook's own explicit `allow` (issue
    /// #769), the same as any other allowed `Bash` command now.
    #[test]
    fn run_pretool_leaves_an_already_limited_git_log_alone() {
        let repo = orchestrator_repo();
        for command in ["git log -n 5", "git log --max-count=3", "git log -3"] {
            let mut out = Vec::new();
            let code = run_pretool(
                &mut out,
                &bash_pretool_stdin(&repo.path().display().to_string(), command),
                &|_| None,
            )
            .expect("never errors");
            assert_eq!(code, 0);
            let printed = String::from_utf8(out).expect("utf8");
            assert_allow_with_no_rewrite(&printed, command);
        }
    }

    /// A piped `git log` is left alone entirely -- the author already shaped
    /// its output, so the whole command is skipped rather than only the
    /// `git log` part; no `updatedInput` ever appears for it.
    #[test]
    fn run_pretool_leaves_a_piped_git_log_alone() {
        let repo = orchestrator_repo();
        let mut out = Vec::new();
        let code = run_pretool(
            &mut out,
            &bash_pretool_stdin(&repo.path().display().to_string(), "git log | head"),
            &|_| None,
        )
        .expect("never errors");
        assert_eq!(code, 0);
        let printed = String::from_utf8(out).expect("utf8");
        assert_allow_with_no_rewrite(
            &printed,
            "a pipe means the author already shaped the output",
        );
    }

    /// A compound command is left alone entirely, even when one of its parts
    /// is a bare `git log` -- the rewrite only ever fires when the WHOLE
    /// trimmed command is a single `git log` invocation, never a piece of a
    /// chain.
    #[test]
    fn run_pretool_leaves_a_compound_git_log_alone() {
        let repo = orchestrator_repo();
        let mut out = Vec::new();
        let code = run_pretool(
            &mut out,
            &bash_pretool_stdin(&repo.path().display().to_string(), "git log && ls"),
            &|_| None,
        )
        .expect("never errors");
        assert_eq!(code, 0);
        let printed = String::from_utf8(out).expect("utf8");
        assert_allow_with_no_rewrite(
            &printed,
            "a compound command is not picked apart to rewrite one piece of it",
        );
    }

    /// Review finding F5: a trailing shell comment on an otherwise-bare
    /// `git log` must never be rewritten -- appending ` -n 50` after a `#`
    /// lands INSIDE the comment, so the command a shell actually runs stays
    /// exactly as unbounded as before the "fix".
    #[test]
    fn run_pretool_leaves_a_commented_git_log_alone() {
        let repo = orchestrator_repo();
        let mut out = Vec::new();
        let code = run_pretool(
            &mut out,
            &bash_pretool_stdin(
                &repo.path().display().to_string(),
                "git log # include all history",
            ),
            &|_| None,
        )
        .expect("never errors");
        assert_eq!(code, 0);
        let printed = String::from_utf8(out).expect("utf8");
        assert_allow_with_no_rewrite(&printed, "a trailing `#` comment must never be rewritten");
    }

    /// A denied command -- the expensive-seat guard's fork denial, and the
    /// orchestrator-write guard's edit denial -- never carries `updatedInput`
    /// alongside its deny envelope.
    #[test]
    fn run_pretool_never_attaches_updated_input_to_a_denied_command() {
        let env: std::collections::HashMap<String, String> = [(
            crate::commands::ctx::adapters::SEAT_MODEL_ENV.to_string(),
            "fable".to_string(),
        )]
        .into();
        let mut out = Vec::new();
        let code = run_pretool(
            &mut out,
            &pretool_stdin(
                "Agent",
                serde_json::json!({"subagent_type": "fork", "prompt": "do the thing"}),
            ),
            &|k| env.get(k).cloned(),
        )
        .expect("never errors");
        assert_eq!(code, 0);
        let printed = String::from_utf8(out).expect("utf8");
        let parsed: serde_json::Value = serde_json::from_str(printed.trim()).expect("json");
        assert_eq!(parsed["hookSpecificOutput"]["permissionDecision"], "deny");
        assert!(
            parsed["hookSpecificOutput"].get("updatedInput").is_none(),
            "a fork denial must never carry updatedInput: {parsed}"
        );

        let repo = orchestrator_repo();
        let env: std::collections::HashMap<String, String> = [
            (
                adapters::SEAT_ROLE_ENV.to_string(),
                "orchestrator".to_string(),
            ),
            (
                "ZIRV_CTX_SUPERVISE_ORCHESTRATOR_WRITES".to_string(),
                "deny".to_string(),
            ),
        ]
        .into();
        let mut out = Vec::new();
        let code = run_pretool(
            &mut out,
            &edit_payload_stdin(repo.path(), "src/x.rs"),
            &|k| env.get(k).cloned(),
        )
        .expect("never errors");
        assert_eq!(code, 0);
        let printed = String::from_utf8(out).expect("utf8");
        let parsed: serde_json::Value = serde_json::from_str(printed.trim()).expect("json");
        assert_eq!(parsed["hookSpecificOutput"]["permissionDecision"], "deny");
        assert!(
            parsed["hookSpecificOutput"].get("updatedInput").is_none(),
            "an orchestrator-write denial must never carry updatedInput: {parsed}"
        );
    }

    /// The decision-log row for a rewrite records BOTH the original and the
    /// rewritten command, action `"rewrite"`.
    #[test]
    fn run_pretool_rewrite_logs_the_original_and_rewritten_commands() {
        let dir = tempfile::tempdir().expect("tempdir");
        let repo = orchestrator_repo();
        let state = dir.path().join("state");
        let env: std::collections::HashMap<String, String> = [(
            crate::commands::ctx::state::STATE_ENV.to_string(),
            state.display().to_string(),
        )]
        .into();

        let mut out = Vec::new();
        let code = run_pretool(
            &mut out,
            &bash_pretool_stdin(&repo.path().display().to_string(), "git log"),
            &|k| env.get(k).cloned(),
        )
        .expect("never errors");
        assert_eq!(code, 0);
        assert!(!out.is_empty());

        let log = std::fs::read_to_string(state.join("logs/decisions.jsonl")).expect("log written");
        assert!(log.contains("\"action\":\"rewrite\""), "got {log}");
        assert!(log.contains("git log -> git log -n 50"), "got {log}");
    }

    // -- the seat env the orchestrator exports ------------------------------

    #[test]
    fn only_an_orchestrator_or_single_seat_with_a_configured_model_exports_the_seat() {
        use crate::commands::ctx::adapters::{SEAT_MODEL_ENV, seat_model_env};
        use crate::commands::ctx::prompt::PromptRole;

        assert_eq!(
            seat_model_env(PromptRole::Orchestrator, &[], Some("fable")),
            vec![(SEAT_MODEL_ENV.to_string(), "fable".to_string())]
        );
        // Issue #537 (T3): a Single seat is just as able to fork a native
        // subagent through its own harness as an Orchestrator, so it
        // discloses its own model through the identical path.
        assert_eq!(
            seat_model_env(PromptRole::Single, &[], Some("fable")),
            vec![(SEAT_MODEL_ENV.to_string(), "fable".to_string())]
        );
        assert!(
            seat_model_env(PromptRole::Worker, &[], Some("fable")).is_empty(),
            "a worker is not a seat that spawns subagents"
        );
        assert!(
            seat_model_env(PromptRole::SubOrchestrator, &[], Some("fable")).is_empty(),
            "a sub-orchestrator is not a seat that spawns subagents either"
        );
        assert!(
            seat_model_env(PromptRole::Orchestrator, &[], None).is_empty(),
            "nothing configured, nothing to inherit"
        );
        assert!(
            seat_model_env(PromptRole::Orchestrator, &[], Some("   ")).is_empty(),
            "a blank model names no tier"
        );
    }

    /// FIX 1: an operator passthrough `--model` with no `chat.model`
    /// configured must still disclose the seat it actually launches on --
    /// the guard used to fail open on exactly this shape.
    #[test]
    fn an_operator_passed_model_flag_exports_the_seat_with_no_config_at_all() {
        use crate::commands::ctx::adapters::{SEAT_MODEL_ENV, seat_model_env};
        use crate::commands::ctx::prompt::PromptRole;

        let flags = vec!["--model".to_string(), "fable".to_string()];
        assert_eq!(
            seat_model_env(PromptRole::Orchestrator, &flags, None),
            vec![(SEAT_MODEL_ENV.to_string(), "fable".to_string())]
        );
    }

    /// FIX 1: an operator passthrough overriding a configured `chat.model`
    /// must disclose the flag's own value, not the configured one -- the
    /// launch actually runs on the flag.
    #[test]
    fn an_operator_passed_model_flag_wins_over_a_configured_chat_model() {
        use crate::commands::ctx::adapters::{SEAT_MODEL_ENV, seat_model_env};
        use crate::commands::ctx::prompt::PromptRole;

        let flags = vec!["--model".to_string(), "sonnet".to_string()];
        assert_eq!(
            seat_model_env(PromptRole::Orchestrator, &flags, Some("fable")),
            vec![(SEAT_MODEL_ENV.to_string(), "sonnet".to_string())]
        );
    }

    /// With no operator passthrough at all, behavior is unchanged from
    /// before FIX 1: the configured `chat.model` alone decides.
    #[test]
    fn with_no_operator_flag_the_configured_model_still_decides() {
        use crate::commands::ctx::adapters::{SEAT_MODEL_ENV, seat_model_env};
        use crate::commands::ctx::prompt::PromptRole;

        assert_eq!(
            seat_model_env(PromptRole::Orchestrator, &[], Some("fable")),
            vec![(SEAT_MODEL_ENV.to_string(), "fable".to_string())]
        );
    }

    /// The `--model=<value>` joined form is recognised too, not just the
    /// two-token spelling.
    #[test]
    fn the_joined_equals_form_of_the_model_flag_is_recognised() {
        use crate::commands::ctx::adapters::{SEAT_MODEL_ENV, seat_model_env};
        use crate::commands::ctx::prompt::PromptRole;

        let flags = vec!["--model=opus".to_string()];
        assert_eq!(
            seat_model_env(PromptRole::Orchestrator, &flags, None),
            vec![(SEAT_MODEL_ENV.to_string(), "opus".to_string())]
        );
    }

    /// A repeated `--model` is CLI last-wins: the later occurrence in argv
    /// order is the one the real harness actually launches on.
    #[test]
    fn a_repeated_model_flag_resolves_to_the_last_occurrence() {
        use crate::commands::ctx::adapters::{SEAT_MODEL_ENV, seat_model_env};
        use crate::commands::ctx::prompt::PromptRole;

        let flags = vec![
            "--model".to_string(),
            "opus".to_string(),
            "--model".to_string(),
            "haiku".to_string(),
        ];
        assert_eq!(
            seat_model_env(PromptRole::Orchestrator, &flags, None),
            vec![(SEAT_MODEL_ENV.to_string(), "haiku".to_string())]
        );

        // Mixed spellings: the joined form arriving last still wins over an
        // earlier two-token occurrence.
        let mixed = vec![
            "--model".to_string(),
            "opus".to_string(),
            "--model=haiku".to_string(),
        ];
        assert_eq!(
            seat_model_env(PromptRole::Orchestrator, &mixed, None),
            vec![(SEAT_MODEL_ENV.to_string(), "haiku".to_string())]
        );
    }

    /// FIX 1 still respects the two guards `seat_model_env` already had: a
    /// `Worker` role never exports regardless of what the flags carry, and a
    /// blank resolved model (however it was resolved) exports nothing.
    #[test]
    fn fix_1_still_respects_the_role_gate_and_the_blank_model_suppression() {
        use crate::commands::ctx::adapters::seat_model_env;
        use crate::commands::ctx::prompt::PromptRole;

        let flags = vec!["--model".to_string(), "fable".to_string()];
        assert!(
            seat_model_env(PromptRole::Worker, &flags, None).is_empty(),
            "a worker pane must never export a seat, flags or not"
        );
        let blank = vec!["--model".to_string(), "   ".to_string()];
        assert!(
            seat_model_env(PromptRole::Orchestrator, &blank, None).is_empty(),
            "a blank flag value names no tier, same as a blank configured model"
        );
    }

    fn scope_guard_edit_stdin(session: &str, cwd: &Path, permission_mode: &str) -> String {
        serde_json::json!({
            "session_id": session,
            "cwd": cwd.display().to_string(),
            "tool_name": "Edit",
            "tool_input": {
                "file_path": cwd.join("src/lib.rs").display().to_string(),
                "old_string": "a",
                "new_string": "b",
            },
            "permission_mode": permission_mode,
        })
        .to_string()
    }

    /// End to end: the first `Edit` after a prompt gets the checkpoint, a
    /// second `Edit` in the same prompt does not, and a new prompt re-arms
    /// it.
    #[test]
    fn scope_guard_checkpoint_fires_once_then_a_new_prompt_rearms_it() {
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
        let session = "sess-checkpoint-1";

        run_prompt(
            &mut Vec::new(),
            &scope_guard_prompt_stdin(session, repo.path(), SCOPE_GUARD_T24_PROMPT),
            &lookup,
        )
        .expect("run_prompt");

        let edit_stdin = scope_guard_edit_stdin(session, repo.path(), "default");
        let mut first = Vec::new();
        run_pretool(&mut first, &edit_stdin, &lookup).expect("run_pretool");
        let first = String::from_utf8(first).expect("utf8");
        assert!(
            first.contains("Scope checkpoint"),
            "the first Edit must show the checkpoint: {first}"
        );
        assert!(
            first.contains("ask the user first."),
            "interactive wording: {first}"
        );

        let mut second = Vec::new();
        run_pretool(&mut second, &edit_stdin, &lookup).expect("run_pretool");
        let second = String::from_utf8(second).expect("utf8");
        assert!(
            !second.contains("Scope checkpoint"),
            "a second Edit in the same prompt must stay silent: {second}"
        );

        run_prompt(
            &mut Vec::new(),
            &scope_guard_prompt_stdin(session, repo.path(), "A different, unrelated request."),
            &lookup,
        )
        .expect("run_prompt");
        let mut third = Vec::new();
        run_pretool(&mut third, &edit_stdin, &lookup).expect("run_pretool");
        let third = String::from_utf8(third).expect("utf8");
        assert!(
            third.contains("Scope checkpoint"),
            "a new prompt must re-arm the checkpoint: {third}"
        );
    }

    /// A `Write` to a file that does not exist yet is a new file, not a
    /// change to existing code, so it must not consume the checkpoint -- a
    /// following `Edit` still gets it.
    #[test]
    fn scope_guard_checkpoint_ignores_a_write_to_a_new_file() {
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
        let session = "sess-checkpoint-2";

        run_prompt(
            &mut Vec::new(),
            &scope_guard_prompt_stdin(session, repo.path(), SCOPE_GUARD_T24_PROMPT),
            &lookup,
        )
        .expect("run_prompt");

        let write_stdin = serde_json::json!({
            "session_id": session,
            "cwd": repo.path().display().to_string(),
            "tool_name": "Write",
            "tool_input": {
                "file_path": repo.path().join("brand_new.rs").display().to_string(),
                "content": "fn x() {}\n",
            },
            "permission_mode": "default",
        })
        .to_string();
        let mut write_out = Vec::new();
        run_pretool(&mut write_out, &write_stdin, &lookup).expect("run_pretool");
        let write_out = String::from_utf8(write_out).expect("utf8");
        assert!(
            !write_out.contains("Scope checkpoint"),
            "a Write to a brand-new file must not trigger the checkpoint: {write_out}"
        );

        let edit_stdin = scope_guard_edit_stdin(session, repo.path(), "default");
        let mut edit_out = Vec::new();
        run_pretool(&mut edit_out, &edit_stdin, &lookup).expect("run_pretool");
        let edit_out = String::from_utf8(edit_out).expect("utf8");
        assert!(
            edit_out.contains("Scope checkpoint"),
            "the new-file Write must not have consumed the checkpoint: {edit_out}"
        );
    }

    /// Headless (`permission_mode == "dontAsk"`) gets the "found, not
    /// changed" wording instead of "ask the user first.".
    #[test]
    fn scope_guard_checkpoint_uses_headless_wording_under_dont_ask() {
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
        let session = "sess-checkpoint-3";

        run_prompt(
            &mut Vec::new(),
            &scope_guard_prompt_stdin(session, repo.path(), SCOPE_GUARD_T24_PROMPT),
            &lookup,
        )
        .expect("run_prompt");

        let edit_stdin = scope_guard_edit_stdin(session, repo.path(), "dontAsk");
        let mut out = Vec::new();
        run_pretool(&mut out, &edit_stdin, &lookup).expect("run_pretool");
        let out = String::from_utf8(out).expect("utf8");
        assert!(
            out.contains("Found, not changed"),
            "headless wording: {out}"
        );
        assert!(
            !out.contains("ask the user first."),
            "headless must not ask: {out}"
        );
    }

    /// A `Write`/`Edit` the orchestrator-write guard DENIES must not consume
    /// the checkpoint: nothing was ever shown to the model, so the very next
    /// actually-allowed edit in the same prompt still gets it.
    #[test]
    fn scope_guard_checkpoint_survives_a_denied_orchestrator_write() {
        let home = tempfile::tempdir().expect("home");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());
        let repo = orchestrator_repo();
        let state_dir = tempfile::tempdir().expect("state dir");
        let session = "sess-deny-1";
        let env: std::collections::HashMap<String, String> = [
            (
                crate::commands::ctx::state::STATE_ENV.to_string(),
                state_dir.path().display().to_string(),
            ),
            (
                adapters::SEAT_ROLE_ENV.to_string(),
                "orchestrator".to_string(),
            ),
            (
                "ZIRV_CTX_SUPERVISE_ORCHESTRATOR_WRITES".to_string(),
                "deny".to_string(),
            ),
        ]
        .into();
        let lookup = |k: &str| env.get(k).cloned();

        run_prompt(
            &mut Vec::new(),
            &scope_guard_prompt_stdin(session, repo.path(), SCOPE_GUARD_T24_PROMPT),
            &lookup,
        )
        .expect("run_prompt");

        let denied_edit = orchestrator_pretool_stdin(
            &repo.path().display().to_string(),
            session,
            "Edit",
            serde_json::json!({"file_path": repo.path().join("src/x.rs").display().to_string()}),
        );
        let mut denied_out = Vec::new();
        run_pretool(&mut denied_out, &denied_edit, &lookup).expect("run_pretool");
        let denied_out = String::from_utf8(denied_out).expect("utf8");
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(denied_out.trim()).expect("json")["hookSpecificOutput"]
                ["permissionDecision"],
            "deny",
            "{denied_out}"
        );
        assert!(
            !denied_out.contains("Scope checkpoint"),
            "a denied write must never show the checkpoint: {denied_out}"
        );

        // The same prompt's checkpoint must still be available for a real
        // edit -- here, no orchestrator role at all, so the write proceeds.
        let plain_env: std::collections::HashMap<String, String> = [(
            crate::commands::ctx::state::STATE_ENV.to_string(),
            state_dir.path().display().to_string(),
        )]
        .into();
        let plain_lookup = |k: &str| plain_env.get(k).cloned();
        let allowed_edit = scope_guard_edit_stdin(session, repo.path(), "default");
        let mut allowed_out = Vec::new();
        run_pretool(&mut allowed_out, &allowed_edit, &plain_lookup).expect("run_pretool");
        let allowed_out = String::from_utf8(allowed_out).expect("utf8");
        assert!(
            allowed_out.contains("Scope checkpoint"),
            "the denied attempt must not have consumed the checkpoint: {allowed_out}"
        );
    }

    /// `[scope_guard] enabled = false` AND `[missing_tests_gate] enabled =
    /// false` in the repo's own `.zirv/ctx.toml` (narrow-only, the
    /// operator's own home layer defaults both on): with NEITHER feature
    /// this checkpoint now also backs left on, no record is written at all,
    /// so no checkpoint ever shows. (`record_scope_guard_request` writes a
    /// record whenever either feature is on, since item 1's "tests owed"
    /// line can fire the checkpoint on its own -- see that function's own
    /// doc comment.)
    #[test]
    fn scope_guard_disabled_shows_no_checkpoint_and_writes_no_record() {
        let home = tempfile::tempdir().expect("home");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());
        let repo = tempfile::tempdir().expect("repo");
        std::fs::create_dir_all(repo.path().join(".zirv")).expect("mkdir");
        std::fs::write(
            repo.path().join(".zirv/ctx.toml"),
            "[scope_guard]\nenabled = false\n[missing_tests_gate]\nenabled = false\n",
        )
        .expect("write");
        let state_dir = tempfile::tempdir().expect("state dir");
        let env: std::collections::HashMap<String, String> = [(
            crate::commands::ctx::state::STATE_ENV.to_string(),
            state_dir.path().display().to_string(),
        )]
        .into();
        let lookup = |k: &str| env.get(k).cloned();
        let session = "sess-disabled-1";

        run_prompt(
            &mut Vec::new(),
            &scope_guard_prompt_stdin(session, repo.path(), SCOPE_GUARD_T24_PROMPT),
            &lookup,
        )
        .expect("run_prompt");

        let edit_stdin = scope_guard_edit_stdin(session, repo.path(), "default");
        let mut out = Vec::new();
        run_pretool(&mut out, &edit_stdin, &lookup).expect("run_pretool");
        let out = String::from_utf8(out).expect("utf8");
        assert!(
            !out.contains("Scope checkpoint"),
            "disabled must never show the checkpoint: {out}"
        );
        let record_dir = state_dir.path().join("scope-guard");
        assert!(
            !record_dir.exists() || std::fs::read_dir(&record_dir).unwrap().next().is_none(),
            "disabled must never write a scope-guard record"
        );
    }

    const SCOPE_GUARD_STATED_DETAILS_CHECKPOINT_PROMPT: &str = "Export contacts to CSV, but keep \
         the existing pagination behaviour exactly as before. Sort rows by last name in \
         ascending order. The header row must read exactly \"id,name,email\".";

    /// End to end: the first `Edit` after a prompt with stated details shows
    /// the numbered checklist exactly once; a second `Edit` in the same
    /// prompt does not repeat it.
    #[test]
    fn scope_checkpoint_shows_the_stated_details_list_once() {
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
        let session = "sess-details-1";

        run_prompt(
            &mut Vec::new(),
            &scope_guard_prompt_stdin(
                session,
                repo.path(),
                SCOPE_GUARD_STATED_DETAILS_CHECKPOINT_PROMPT,
            ),
            &lookup,
        )
        .expect("run_prompt");

        let edit_stdin = scope_guard_edit_stdin(session, repo.path(), "default");
        let mut first = Vec::new();
        run_pretool(&mut first, &edit_stdin, &lookup).expect("run_pretool");
        let first = String::from_utf8(first).expect("utf8");
        assert!(
            first.contains("Stated details to check before you finish:"),
            "must show the stated-details checklist: {first}"
        );
        assert!(first.contains("(1)"), "must number the first item: {first}");

        let mut second = Vec::new();
        run_pretool(&mut second, &edit_stdin, &lookup).expect("run_pretool");
        let second = String::from_utf8(second).expect("utf8");
        assert!(
            !second.contains("Stated details to check before you finish:"),
            "must appear at most once per prompt: {second}"
        );
    }

    /// Runs one Bash PreToolUse payload with the process cwd inside a repo whose
    /// `.zirv/ctx.toml` is `repo_toml`, and returns what the hook printed.
    fn bash_pretool_with_repo_config(repo_toml: &str, command: &str, mode: &str) -> String {
        bash_pretool_with_repo_config_env(repo_toml, command, mode, &|_| None)
    }

    fn bash_pretool_with_repo_config_env(
        repo_toml: &str,
        command: &str,
        mode: &str,
        env: EnvLookup<'_>,
    ) -> String {
        let home = tempfile::tempdir().expect("home");
        let repo = tempfile::tempdir().expect("repo");
        std::fs::create_dir_all(repo.path().join(".zirv")).expect("mkdir");
        std::fs::write(repo.path().join(".zirv/ctx.toml"), repo_toml).expect("write");
        let _env = crate::commands::ctx::testenv::EnvGuard::set(home.path(), Some(repo.path()));
        let stdin = serde_json::json!({
            "session_id": "abc123",
            "cwd": repo.path().display().to_string(),
            "permission_mode": mode,
            "hook_event_name": "PreToolUse",
            "tool_name": "Bash",
            "tool_input": { "command": command },
        })
        .to_string();
        let mut out = Vec::new();
        let code = run_pretool(&mut out, &stdin, env).expect("never errors");
        assert_eq!(code, 0);
        String::from_utf8(out).expect("utf8")
    }

    /// SECURITY: a repo `.zirv/ctx.toml` carrying a repo-forbidden key must not
    /// silence the Bash safety classifier; the trusted policy still denies.
    #[test]
    fn run_pretool_bash_still_denies_when_the_repo_config_has_forbidden_keys() {
        for repo_toml in [
            "[safety]\nallow = [\"cat *\"]\n",
            "[safety]\ndefault = \"allow\"\n",
        ] {
            for mode in ["default", "dontAsk"] {
                let printed = bash_pretool_with_repo_config(repo_toml, "cat ~/.ssh/id_rsa", mode);
                let parsed: serde_json::Value = serde_json::from_str(printed.trim())
                    .unwrap_or_else(|_| panic!("a forbidden repo config went silent: {printed:?}"));
                assert_eq!(
                    parsed["hookSpecificOutput"]["permissionDecision"], "deny",
                    "{repo_toml} / {mode}: {parsed}"
                );
                let reason = parsed["hookSpecificOutput"]["permissionDecisionReason"]
                    .as_str()
                    .unwrap_or_default();
                assert!(
                    reason.contains("forbidden keys") && reason.contains("not applied"),
                    "the reason must say the repo config was not applied: {reason}"
                );
            }
        }
    }

    /// A repo-forbidden config still lets the trusted policy clear an ordinary
    /// command; it is not turned into a blanket refusal.
    #[test]
    fn run_pretool_bash_with_forbidden_repo_config_still_clears_a_safe_command() {
        let printed = bash_pretool_with_repo_config(
            "[safety]\ndefault = \"allow\"\n",
            "git status",
            "default",
        );
        assert!(
            !printed.contains("\"deny\""),
            "a safe command must not be denied: {printed}"
        );
    }

    /// A refused repo config whose trusted fallback also fails to load (here an invalid numeric
    /// override) must still not go silent: the hook asks, naming both failures.
    #[test]
    fn run_pretool_bash_asks_when_the_trusted_config_cannot_load_either() {
        let printed = bash_pretool_with_repo_config_env(
            "[safety]\ndefault = \"allow\"\n",
            "git status",
            "default",
            &|key| (key == "ZIRV_CTX_WINDOW").then(|| "not-a-number".to_string()),
        );
        let parsed: serde_json::Value = serde_json::from_str(printed.trim())
            .unwrap_or_else(|_| panic!("the ask went silent: {printed:?}"));
        assert_eq!(parsed["hookSpecificOutput"]["permissionDecision"], "ask");
        let reason = parsed["hookSpecificOutput"]["permissionDecisionReason"]
            .as_str()
            .unwrap_or_default();
        assert!(
            reason.contains("forbidden keys")
                && reason.contains("trusted config could not be loaded"),
            "{reason}"
        );
    }

    /// Fail-open (#769) stays for every other load error: a schema error in
    /// the repo config is an internal fault, not a security refusal.
    #[test]
    fn run_pretool_bash_fails_open_on_a_non_forbidden_config_error() {
        let printed =
            bash_pretool_with_repo_config("[score]\nwindwo = 4\n", "cat ~/.ssh/id_rsa", "default");
        assert!(printed.trim().is_empty(), "must fail open: {printed:?}");
    }
}
