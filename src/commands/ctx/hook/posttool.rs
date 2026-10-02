//! The PostToolUse hook: payload types, obfuscation, and `run_posttool`.

use std::io::Write;
#[cfg(test)]
use std::path::Path;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use super::checkpoints::cfg_or_operator_only_gate;
use super::permission::{
    attention_short, clear_resolved_approval, finding_kinds, hook_obfuscation_options,
};
use super::scope_guard::scope_guard_shell_checkpoint_note;
use crate::commands::ctx::config::EnvLookup;
use crate::commands::ctx::state::{StateDir, now_secs, repo_slug};
use crate::commands::ctx::{CtxResult, log};

// -- PostToolUse: compact output (issue #326) ------------------------------

/// Claude's `PostToolUse` stdin, narrowed to what the compact-output hook
/// reads. Every field is optional with a zero default, the same rule every
/// other payload in this file follows: a hook that fails to parse is a hook
/// that silently stops working, so nothing here may be mandatory.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct PostToolPayload {
    pub tool_name: String,
    pub tool_input: super::permission::PermissionToolInput,
    pub tool_response: BashToolOutput,
    pub cwd: String,
    /// Claude session ID recorded in the compaction ledger (#422).
    pub session_id: String,
    /// Claude tool-call ID for correlating this compaction row (#422).
    pub tool_use_id: String,
    /// Non-empty inside a native subagent, whose tool calls must not receive the lead's mail.
    pub agent_id: String,
}

/// The documented shape of claude's `Bash` tool result -- and therefore the
/// exact shape a replacement must match. Claude Code validates a built-in
/// tool's `updatedToolOutput` against its own schema and IGNORES a value that
/// does not match, using the original instead, so this type is what makes the
/// replacement take effect at all.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default, rename_all = "camelCase")]
pub struct BashToolOutput {
    pub stdout: String,
    pub stderr: String,
    pub interrupted: bool,
    pub is_image: bool,
}

/// Claude Code's own too-large-output notice. A result carrying one has
/// ALREADY been truncated and spilled to a file by the harness itself, so
/// compacting it again would summarize a truncation notice and hand back a
/// retrieval id for text zirv never actually holds.
fn already_offloaded(text: &str) -> bool {
    crate::commands::ctx::lifecycle::already_offloaded(text)
}

/// Replace the tool result and clear stderr because the summary already
/// includes both merged streams; leaving stderr would restore removed bytes.
pub(crate) fn posttool_output(summary: &str, interrupted: bool) -> String {
    serde_json::json!({
        "hookSpecificOutput": {
            "hookEventName": "PostToolUse",
            "updatedToolOutput": {
                "stdout": summary,
                "stderr": "",
                "interrupted": interrupted,
                "isImage": false
            }
        }
    })
    .to_string()
}

fn posttool_value_output(value: serde_json::Value) -> String {
    serde_json::json!({
        "hookSpecificOutput": {
            "hookEventName": "PostToolUse",
            "updatedToolOutput": value
        }
    })
    .to_string()
}

/// A standalone `additionalContext` envelope for `PostToolUse` -- used when
/// this hook has nothing else to say for this call (no compaction, no
/// obfuscation) but the scope-guard shell checkpoint fired anyway. Claude
/// Code's `PostToolUse` schema accepts `additionalContext` in the same
/// `hookSpecificOutput` object as `updatedToolOutput`, alongside or instead
/// of it -- see [`posttool_envelope_with_context`], which merges it into an
/// envelope that already carries one.
fn posttool_additional_context_output(note: &str) -> String {
    serde_json::json!({
        "hookSpecificOutput": {
            "hookEventName": "PostToolUse",
            "additionalContext": note
        }
    })
    .to_string()
}

/// Merge a scope checkpoint note into the sole PostToolUse JSON envelope;
/// emitting a second object would make the hook response invalid.
fn posttool_envelope_with_context(envelope: String, note: Option<&str>) -> String {
    let Some(note) = note else {
        return envelope;
    };
    // Fails safe: an envelope that does not round-trip through JSON is
    // returned as-is, dropping the note rather than corrupting the response.
    let Ok(mut value) = serde_json::from_str::<serde_json::Value>(&envelope) else {
        return envelope;
    };
    if let Some(hook_output) = value
        .get_mut("hookSpecificOutput")
        .and_then(serde_json::Value::as_object_mut)
    {
        hook_output.insert(
            "additionalContext".to_string(),
            serde_json::Value::String(note.to_string()),
        );
    }
    value.to_string()
}

/// Emit the scope note alone only when no compaction envelope exists;
/// PostToolUse accepts one JSON response per call.
fn posttool_finish<W: Write>(w: &mut W, note: Option<&str>) -> CtxResult<i32> {
    if let Some(note) = note {
        let _ = writeln!(w, "{}", posttool_additional_context_output(note));
    }
    Ok(0)
}

/// Append mid-turn mail (#834) to the additionalContext note. The cheap gates live in
/// `mail::mid_turn_context`; an unparsable payload or a missing cwd delivers nothing.
fn with_mid_turn_mail(
    note: Option<String>,
    shared: Option<(&PathBuf, &crate::commands::ctx::config::CtxConfig)>,
    env: EnvLookup<'_>,
) -> Option<String> {
    let mail =
        shared.and_then(|(cwd, cfg)| crate::commands::ctx::mail::mid_turn_context(cwd, cfg, env));
    match (note, mail) {
        (Some(note), Some(mail)) => Some(format!("{note}\n\n{mail}")),
        (note, mail) => note.or(mail),
    }
}

fn withhold_strings(value: &mut serde_json::Value) {
    match value {
        serde_json::Value::String(text) => {
            *text = "[zirv withheld sensitive tool output: obfuscation vault unavailable]".into()
        }
        serde_json::Value::Array(values) => values.iter_mut().for_each(withhold_strings),
        serde_json::Value::Object(values) => {
            values.values_mut().for_each(withhold_strings);
        }
        serde_json::Value::Null | serde_json::Value::Bool(_) | serde_json::Value::Number(_) => {}
    }
}

fn json_needs_obfuscation(
    value: &serde_json::Value,
    options: &crate::commands::ctx::obfuscate::Options,
) -> bool {
    match value {
        serde_json::Value::String(text) => crate::commands::ctx::obfuscate::obfuscate(
            text,
            &mut crate::commands::ctx::obfuscate::Vault::default(),
            options,
            "claude_post_tool_use",
        )
        .1
        .iter()
        .any(|finding| finding.replaced),
        serde_json::Value::Array(values) => values
            .iter()
            .any(|value| json_needs_obfuscation(value, options)),
        serde_json::Value::Object(values) => values
            .values()
            .any(|value| json_needs_obfuscation(value, options)),
        serde_json::Value::Null | serde_json::Value::Bool(_) | serde_json::Value::Number(_) => {
            false
        }
    }
}

fn label_sensitive_json(value: &mut serde_json::Value, kinds: &str) {
    let notice = format!("[zirv sensitive data detected: {kinds}]");
    match value {
        serde_json::Value::String(text) => {
            text.push('\n');
            text.push_str(&notice);
        }
        serde_json::Value::Array(values) => values.push(serde_json::Value::String(notice)),
        serde_json::Value::Object(values) => {
            values.insert(
                "zirv_sensitive_data_notice".to_string(),
                serde_json::Value::String(notice),
            );
        }
        serde_json::Value::Null | serde_json::Value::Bool(_) | serde_json::Value::Number(_) => {}
    }
}

fn obfuscated_posttool_response(
    stdin: &str,
    env: EnvLookup<'_>,
    shared: Option<&crate::commands::ctx::config::CtxConfig>,
) -> Option<serde_json::Value> {
    let mut raw = serde_json::from_str::<serde_json::Value>(stdin).ok()?;
    let original = raw.get("tool_response")?.clone();
    let cwd = raw
        .get("cwd")
        .and_then(serde_json::Value::as_str)
        .filter(|cwd| !cwd.is_empty())
        .map(PathBuf::from)
        .or_else(|| std::env::current_dir().ok())?;
    let loaded;
    let cfg = match shared {
        Some(cfg) => cfg,
        None => {
            loaded = cfg_or_operator_only_gate(&cwd, env);
            &loaded
        }
    };
    if cfg.obfuscate.mode == crate::commands::ctx::config::ObfuscateMode::Off {
        return None;
    }
    let options = match hook_obfuscation_options(cfg) {
        Ok(options) => options,
        Err(_) => {
            let mut response = original;
            withhold_strings(&mut response);
            return Some(response);
        }
    };
    let mut response = original.clone();
    let findings = match StateDir::resolve(env) {
        Ok(state) => crate::commands::ctx::obfuscate_store::obfuscate_json(
            state.root(),
            &cwd,
            &mut response,
            &options,
            "claude_post_tool_use",
        ),
        Err(error) => Err(error),
    };
    let findings = match findings {
        Ok(findings) => findings,
        Err(_) => {
            if json_needs_obfuscation(&original, &options) {
                withhold_strings(&mut response);
                return Some(response);
            }
            return None;
        }
    };
    if !findings.is_empty() {
        let detail = finding_kinds(&findings);
        if !findings.iter().any(|finding| finding.replaced) {
            label_sensitive_json(&mut response, &detail);
        }
        raw["tool_response"] = response.clone();
        if let Ok(state) = StateDir::resolve(env) {
            let session = raw
                .get("session_id")
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default();
            let _ = log::append(
                &state,
                &log::Decision {
                    ts: now_secs(),
                    session,
                    verb: "hook",
                    verdict: "n/a",
                    score: 0,
                    action: "obfuscate-tool-output",
                    detail: &detail,
                    observed_at: None,
                },
            );
        }
        if response != original {
            return Some(response);
        }
    }
    None
}

/// Store original Bash output before replacing a large result with a
/// retrievable summary. Any parse, storage or rendering failure passes the
/// original output through unchanged (#326). Nothing here may `unwrap`,
/// `expect` or return `Err`: the release profile is `panic = "abort"`, and
/// a hook that aborts takes the tool result with it.
pub fn run_posttool<W: Write>(w: &mut W, stdin: &str, env: EnvLookup<'_>) -> CtxResult<i32> {
    run_posttool_with(w, stdin, env, true)
}

/// [`run_posttool`]; `mid_turn_mail` is false for an agent whose envelope cannot carry `additionalContext`.
pub fn run_posttool_with<W: Write>(
    w: &mut W,
    stdin: &str,
    env: EnvLookup<'_>,
    mid_turn_mail: bool,
) -> CtxResult<i32> {
    // Load config once and share it with every stage below; each used to load its own.
    let parsed = serde_json::from_str::<PostToolPayload>(stdin).ok();
    let shared = parsed.as_ref().and_then(|payload| {
        let cwd = if payload.cwd.is_empty() {
            std::env::current_dir().ok()?
        } else {
            PathBuf::from(&payload.cwd)
        };
        let cfg = cfg_or_operator_only_gate(&cwd, env);
        Some((cwd, cfg))
    });

    // A successful tool call ends any `[jev] retry` failure streak (#836).
    if let (Some(payload), Some((_, cfg))) = (&parsed, &shared) {
        super::tool_failure::reset_streak(cfg, env, &payload.session_id);
    }

    // Compute the shell checkpoint before replacement paths branch so its
    // note joins their sole JSON envelope; PostToolUse accepts one object.
    let shell_checkpoint = parsed
        .as_ref()
        .filter(|payload| matches!(payload.tool_name.as_str(), "Bash" | "PowerShell"))
        // The prompt record belongs to the lead; a subagent shares its session id (#849).
        .filter(|payload| payload.agent_id.is_empty())
        .and_then(|payload| {
            let (cwd, cfg) = shared.as_ref()?;
            scope_guard_shell_checkpoint_note(
                &payload.tool_name,
                cwd,
                cfg,
                &payload.session_id,
                &payload.tool_input.command,
                env,
            )
        });

    // A subagent's PostToolUse carries `agent_id`: nothing is delivered to it and nothing consumed.
    let lead_turn = mid_turn_mail && parsed.as_ref().is_some_and(|p| p.agent_id.is_empty());
    let shell_checkpoint = with_mid_turn_mail(
        shell_checkpoint,
        shared
            .as_ref()
            .filter(|_| lead_turn)
            .map(|(cwd, cfg)| (cwd, cfg)),
        env,
    );

    // A tool call proves any pending permission prompt has resolved; clear attention and the
    // inbox record before every later early return, the masked branch included (#456).
    if let (Some(payload), Ok(state)) = (&parsed, StateDir::resolve(env)) {
        let short = attention_short(env, &payload.session_id);
        let permission_id = crate::commands::ctx::approvals::request_id(
            &short,
            &payload.tool_name,
            &payload.tool_input.command,
            &payload.tool_input.preview_source(&payload.tool_name),
        );
        clear_resolved_approval(
            &state,
            &short,
            format!("permission resolved: {}", payload.tool_name),
            now_secs(),
            |open| open.id == permission_id && open.agent == payload.agent_id,
        );
        crate::commands::ctx::approvals::clear_for_tool(
            &state,
            &short,
            &payload.tool_name,
            &payload.tool_input.command,
            &payload.tool_input.preview_source(&payload.tool_name),
        );
    }

    // Mask non-Bash results before Bash-only parsing; their response schemas
    // differ but may still carry sensitive strings.
    if let Some(masked) =
        obfuscated_posttool_response(stdin, env, shared.as_ref().map(|(_, cfg)| cfg))
    {
        let _ = writeln!(
            w,
            "{}",
            posttool_envelope_with_context(
                posttool_value_output(masked),
                shell_checkpoint.as_deref()
            )
        );
        return Ok(0);
    }
    let Ok(payload) = serde_json::from_str::<PostToolPayload>(stdin) else {
        return Ok(0);
    };

    if payload.tool_name != "Bash" || payload.tool_response.is_image {
        return posttool_finish(w, shell_checkpoint.as_deref());
    }
    let response = &payload.tool_response;
    let combined = if response.stderr.is_empty() {
        response.stdout.clone()
    } else if response.stdout.is_empty() {
        response.stderr.clone()
    } else {
        format!("{}\n{}", response.stdout, response.stderr)
    };

    // Resolve cwd and state before recording a compaction row at any return
    // path; skip the row if either cannot be resolved (#422).
    let cwd = if payload.cwd.is_empty() {
        let Ok(cwd) = std::env::current_dir() else {
            return posttool_finish(w, shell_checkpoint.as_deref());
        };
        cwd
    } else {
        PathBuf::from(&payload.cwd)
    };
    let Ok(state) = StateDir::resolve(env) else {
        return posttool_finish(w, shell_checkpoint.as_deref());
    };
    let program = payload
        .tool_input
        .command
        .split_whitespace()
        .next()
        .map(crate::commands::ctx::output::bare_program)
        .unwrap_or_default();
    let repo = repo_slug(&cwd);
    let bytes_in = combined.len() as u64;
    let record = |outcome: crate::commands::ctx::ledger::Outcome,
                  bytes_out: u64,
                  retrieval_id: Option<&str>| {
        crate::commands::ctx::ledger::record(
            &state,
            &crate::commands::ctx::ledger::CompactionRow {
                ts: now_secs(),
                tool_use_id: &payload.tool_use_id,
                session: &payload.session_id,
                repo: &repo,
                program: &program,
                bytes_in,
                bytes_out,
                outcome,
                retrieval_id,
            },
        );
    };

    if already_offloaded(&combined) {
        record(
            crate::commands::ctx::ledger::Outcome::Offloaded,
            bytes_in,
            None,
        );
        return posttool_finish(w, shell_checkpoint.as_deref());
    }
    let loaded;
    let cfg = match shared.as_ref() {
        Some((_, cfg)) => cfg,
        None => {
            loaded = cfg_or_operator_only_gate(&cwd, env);
            &loaded
        }
    };
    if !cfg.output.compact {
        record(
            crate::commands::ctx::ledger::Outcome::Disabled,
            bytes_in,
            None,
        );
        return posttool_finish(w, shell_checkpoint.as_deref());
    }
    // Compact only output safe to summarize; readers and retrieval commands
    // need verbatim bytes before the model edits against them.
    let scope = crate::commands::ctx::output::classify_compaction(
        &payload.tool_input.command,
        &cfg.output.verbatim,
        cfg.output.compact_search,
    );
    let threshold = match scope {
        crate::commands::ctx::output::CompactionScope::Verbatim => {
            record(
                crate::commands::ctx::ledger::Outcome::Verbatim,
                bytes_in,
                None,
            );
            return posttool_finish(w, shell_checkpoint.as_deref());
        }
        crate::commands::ctx::output::CompactionScope::Known => cfg.output.compact_min_bytes,
        crate::commands::ctx::output::CompactionScope::Generic => {
            cfg.output.compact_generic_min_bytes
        }
        // Use the diff-specific threshold because oversized diffs get a
        // per-file listing, not a generic head/tail summary (#412).
        crate::commands::ctx::output::CompactionScope::Diff => cfg.output.diff_max_bytes,
        // Use the generic cutoff for unrecognized output shapes (#414).
        crate::commands::ctx::output::CompactionScope::Shape => {
            cfg.output.compact_generic_min_bytes
        }
    };
    // Use the shared cutoff so native and hooked sessions do not compact
    // identical output at different sizes (#478).
    if !crate::commands::ctx::lifecycle::should_compact_result(
        combined.len(),
        true,
        threshold.saturating_sub(1),
    ) {
        record(
            crate::commands::ctx::ledger::Outcome::BelowThreshold,
            bytes_in,
            None,
        );
        return posttool_finish(w, shell_checkpoint.as_deref());
    }
    let command = if payload.tool_input.command.trim().is_empty() {
        vec!["(bash)".to_string()]
    } else {
        vec![payload.tool_input.command.clone()]
    };
    let Ok((id, summary)) = crate::commands::ctx::output::capture_text_with_filters(
        &state,
        &cwd,
        &command,
        None,
        &combined,
        cfg.output.max_summary_bytes,
        scope,
        &cfg.output.filter,
    ) else {
        // Do not replace output unless its original was stored for retrieval.
        record(
            crate::commands::ctx::ledger::Outcome::PersistFailed,
            bytes_in,
            None,
        );
        return posttool_finish(w, shell_checkpoint.as_deref());
    };
    // `None` means the summary could not carry its own MANDATORY failure
    // content inside the cap. Replacing a result with a summary that had
    // silently dropped a `fatal:` is strictly worse than not replacing it, so
    // this fails open like every other path in here.
    let Some(summary) = summary else {
        record(
            crate::commands::ctx::ledger::Outcome::PersistFailed,
            bytes_in,
            Some(&id),
        );
        return posttool_finish(w, shell_checkpoint.as_deref());
    };
    record(
        crate::commands::ctx::ledger::Outcome::Compacted,
        summary.len() as u64,
        Some(&id),
    );
    let _ = writeln!(
        w,
        "{}",
        posttool_envelope_with_context(
            posttool_output(&summary, response.interrupted),
            shell_checkpoint.as_deref()
        )
    );
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::super::agent::run_posttool_for_agent;
    use super::super::permission::run_permission;
    use super::super::prompt::run_prompt;
    use super::super::tests::{
        SCOPE_GUARD_T24_PROMPT, SEAT, decide, permission_env, permission_stdin,
        scope_guard_bash_posttool_stdin, scope_guard_prompt_stdin, scope_guard_shell_rig,
    };
    use super::*;

    /// A `PermissionRequest` raises `Attention::Approval`; a `PostToolUse` for
    /// the SAME tool that just ran is proof the prompt is gone.
    #[test]
    fn posttool_after_a_permission_request_clears_approval() {
        let dir = tempfile::tempdir().expect("tempdir");
        let state_path = dir.path().join("state");
        let env = permission_env(&state_path);
        let lookup = |k: &str| env.get(k).cloned();
        let state = StateDir::resolve(&lookup).expect("state dir");
        let short = crate::commands::ctx::sessions::short_id("abc123");

        let mut out = Vec::new();
        run_permission(
            &mut out,
            &permission_stdin(
                Some("PermissionRequest"),
                "Bash",
                serde_json::json!({"command": "echo hi"}),
            ),
            &lookup,
        )
        .expect("never errors");
        assert_eq!(
            crate::commands::ctx::attention::load(&state, &short).attention,
            crate::commands::ctx::attention::Attention::Approval,
            "the request must raise the latch before the assertion below means anything"
        );

        let posttool_stdin = serde_json::json!({
            "session_id": "abc123",
            "tool_name": "Bash",
            "tool_input": {"command": "echo hi"},
            "tool_response": {"stdout": "hi", "stderr": "", "interrupted": false, "isImage": false},
            "cwd": "/work/repo",
            "tool_use_id": "toolu_01ABC123",
        })
        .to_string();
        let mut out = Vec::new();
        let code = run_posttool(&mut out, &posttool_stdin, &lookup).expect("never errors");
        assert_eq!(code, 0);

        let status = crate::commands::ctx::attention::load(&state, &short);
        assert_eq!(
            status.attention,
            crate::commands::ctx::attention::Attention::None
        );
        assert!(
            crate::commands::ctx::attention::reason(&status).contains("permission resolved: Bash"),
            "explain-status must name what cleared it: {}",
            crate::commands::ctx::attention::reason(&status)
        );
    }

    /// Two parallel subagents each hold a prompt (#854): an unrelated call finishing, or one prompt
    /// resolving, leaves the session `Approval` until the last one resolves.
    #[test]
    fn concurrent_permission_prompts_keep_approval_until_the_last_resolves() {
        use crate::commands::ctx::attention::{Attention, load};
        let dir = tempfile::tempdir().expect("tempdir");
        let state_path = dir.path().join("state");
        let env = permission_env(&state_path);
        let lookup = |k: &str| env.get(k).cloned();
        let state = StateDir::resolve(&lookup).expect("state dir");
        let short = crate::commands::ctx::sessions::short_id("abc123");
        let post = |tool: &str, command: &str| {
            let stdin = serde_json::json!({
                "session_id": "abc123", "tool_name": tool, "tool_input": {"command": command},
                "tool_response": {"stdout": "", "stderr": "", "interrupted": false, "isImage": false},
                "cwd": "/work/repo", "tool_use_id": "toolu_x",
            })
            .to_string();
            run_posttool(&mut Vec::new(), &stdin, &lookup).expect("never errors");
        };
        for command in ["echo a", "echo b"] {
            run_permission(
                &mut Vec::new(),
                &permission_stdin(
                    Some("PermissionRequest"),
                    "Bash",
                    serde_json::json!({ "command": command }),
                ),
                &lookup,
            )
            .expect("never errors");
        }

        post("Read", "");
        assert_eq!(load(&state, &short).attention, Attention::Approval);
        post("Bash", "echo a");
        assert_eq!(load(&state, &short).attention, Attention::Approval);
        post("Bash", "echo b");
        assert_eq!(load(&state, &short).attention, Attention::None);
    }

    /// A guard, not an unconditional clear: a `PostToolUse` firing while the
    /// session is blocked on something OTHER than an approval (a `Supervisor`
    /// `WriterConflict`, say) must leave that latch alone -- `AdapterHook`
    /// already outranks every other authority on the attention axis, so an
    /// unconditional `Attention::None` write here would erase it just because
    /// a tool happened to run.
    #[test]
    fn posttool_never_clears_a_non_approval_attention() {
        let dir = tempfile::tempdir().expect("tempdir");
        let state_path = dir.path().join("state");
        let env = permission_env(&state_path);
        let lookup = |k: &str| env.get(k).cloned();
        let state = StateDir::resolve(&lookup).expect("state dir");
        let short = crate::commands::ctx::sessions::short_id("abc123");

        crate::commands::ctx::attention::record(
            &state,
            &short,
            crate::commands::ctx::attention::Observation::new(
                crate::commands::ctx::attention::Authority::Supervisor,
                "writer permit held elsewhere",
                80,
                1,
            )
            .with_attention(crate::commands::ctx::attention::Attention::WriterConflict),
            1,
        );

        let posttool_stdin = serde_json::json!({
            "session_id": "abc123",
            "tool_name": "Bash",
            "tool_input": {"command": "echo hi"},
            "tool_response": {"stdout": "hi", "stderr": "", "interrupted": false, "isImage": false},
            "cwd": "/work/repo",
            "tool_use_id": "toolu_01ABC123",
        })
        .to_string();
        let mut out = Vec::new();
        run_posttool(&mut out, &posttool_stdin, &lookup).expect("never errors");

        assert_eq!(
            crate::commands::ctx::attention::load(&state, &short).attention,
            crate::commands::ctx::attention::Attention::WriterConflict,
            "a posttool must never clear an attention it did not itself raise"
        );
    }

    /// Perf: the fast unlocked pre-check in
    /// `clear_resolved_approval` must skip `record_if`'s lock entirely when
    /// there is no `Approval` latch to clear at all -- the common case on
    /// every `PreToolUse`/`PostToolUse` call. Proven directly: a session with
    /// no attention recorded yet (so nothing under `state.attention()` exists
    /// for its short id) gets a plain `PostToolUse` hook call, and neither
    /// the status file nor the lock file `record_if`'s own `lock_status`
    /// would otherwise create is ever written.
    #[test]
    fn posttool_never_touches_the_attention_lock_when_nothing_is_pending() {
        let dir = tempfile::tempdir().expect("tempdir");
        let state_path = dir.path().join("state");
        let env = permission_env(&state_path);
        let lookup = |k: &str| env.get(k).cloned();
        let state = StateDir::resolve(&lookup).expect("state dir");
        let short = crate::commands::ctx::sessions::short_id("abc123");

        // Nothing recorded yet: `state.attention()` may not even exist.
        assert!(
            !state.attention().join(format!("{short}.json")).exists(),
            "test setup: no status file should exist before the hook runs"
        );
        assert!(
            !state.attention().join(format!("{short}.lock")).exists(),
            "test setup: no lock file should exist before the hook runs"
        );

        let posttool_stdin = serde_json::json!({
            "session_id": "abc123",
            "tool_name": "Bash",
            "tool_input": {"command": "echo hi"},
            "tool_response": {"stdout": "hi", "stderr": "", "interrupted": false, "isImage": false},
            "cwd": "/work/repo",
            "tool_use_id": "toolu_01ABC123",
        })
        .to_string();
        let mut out = Vec::new();
        run_posttool(&mut out, &posttool_stdin, &lookup).expect("never errors");

        assert!(
            !state.attention().join(format!("{short}.lock")).exists(),
            "the fast unlocked pre-check must skip record_if's lock entirely when there is \
             nothing to clear"
        );
        assert!(
            !state.attention().join(format!("{short}.json")).exists(),
            "nothing to clear must never write a status file either"
        );
    }

    /// Issue #840: a finished tool call ends its pending approval record, and only that call's.
    #[test]
    fn posttool_clears_the_approval_record_of_the_finished_call_only() {
        let dir = tempfile::tempdir().expect("tempdir");
        let env = permission_env(&dir.path().join("state"));
        let lookup = |k: &str| env.get(k).cloned();
        let state = StateDir::resolve(&lookup).expect("state dir");
        let short = crate::commands::ctx::sessions::short_id("abc123");
        let finished =
            crate::commands::ctx::approvals::Request::new(&short, "Bash", "echo hi", "echo hi", 1);
        let other = crate::commands::ctx::approvals::Request::new(&short, "Bash", "ls", "ls", 1);
        let finished_path =
            crate::commands::ctx::approvals::write_record(&state, &finished).expect("record");
        let other_path =
            crate::commands::ctx::approvals::write_record(&state, &other).expect("record");
        let stdin = serde_json::json!({
            "session_id": "abc123",
            "tool_name": "Bash",
            "tool_input": {"command": "echo hi"},
            "tool_response": {"stdout": "hi", "stderr": "", "interrupted": false, "isImage": false},
            "cwd": "/work/repo",
            "tool_use_id": "toolu_01ABC123",
        })
        .to_string();
        run_posttool(&mut Vec::new(), &stdin, &lookup).expect("never errors");
        assert!(!finished_path.exists());
        assert!(other_path.exists());
    }

    #[test]
    fn a_fork_is_denied_because_it_always_inherits_the_seat_model() {
        let reason = decide(
            SEAT,
            "Agent",
            serde_json::json!({"subagent_type": "fork", "prompt": "do the thing"}),
        )
        .expect("a fork from an expensive seat must be blocked");
        assert!(reason.contains("fable"), "name the seat: {reason}");
        assert!(reason.contains("Fork"), "say forks are out: {reason}");
        assert!(!reason.contains('\u{2014}'), "no em dashes in user copy");
    }

    /// A fork ignores the `model` parameter entirely, so naming a cheap one
    /// must not buy a way past the rule.
    #[test]
    fn a_fork_is_denied_even_when_it_names_a_cheap_model() {
        assert!(
            decide(
                SEAT,
                "Agent",
                serde_json::json!({"subagent_type": "fork", "model": "haiku", "prompt": "do the thing"})
            )
            .is_some()
        );
    }

    #[test]
    fn an_explicit_seat_tier_model_is_denied() {
        assert!(
            decide(
                SEAT,
                "Agent",
                serde_json::json!({"subagent_type": "general-purpose", "model": "fable", "prompt": "do the thing"})
            )
            .is_some(),
            "re-asking for the seat tier by name is the exact spend being guarded"
        );
    }

    #[test]
    fn an_explicit_cheaper_model_is_allowed() {
        for model in ["haiku", "sonnet", "opus"] {
            assert_eq!(
                decide(
                    SEAT,
                    "Agent",
                    serde_json::json!({"subagent_type": "general-purpose", "model": model})
                ),
                None,
                "{model} is the whole point of the escape hatch"
            );
        }
    }

    #[test]
    fn an_omitted_model_on_a_generic_subagent_type_is_denied() {
        for kind in ["fork", "claude", "general-purpose", "Explore", "Plan"] {
            assert!(
                decide(
                    SEAT,
                    "Agent",
                    serde_json::json!({"subagent_type": kind, "prompt": "do the thing"})
                )
                .is_some(),
                "{kind} pins no model of its own, so it inherits the seat"
            );
        }
    }

    /// A real dispatch (a non-empty `prompt`) with both `model` and
    /// `subagent_type` omitted/blank is denied exactly like an explicit
    /// generic type -- empty is the same as absent, once the payload is
    /// recognised as a real dispatch at all.
    #[test]
    fn an_omitted_model_and_an_omitted_subagent_type_is_denied() {
        assert!(
            decide(
                SEAT,
                "Agent",
                serde_json::json!({"subagent_type": "", "model": "", "prompt": "do the thing"})
            )
            .is_some(),
            "empty is the same as absent"
        );
    }

    // -- PostToolUse: compact output (issue #326) --------------------------

    /// A verbose `cargo test`-shaped result: a lot of filler wrapped around
    /// the handful of lines a compact summary must never lose.
    fn noisy_output() -> String {
        let mut text: String = (1..=400).map(|i| format!("filler line {i}\n")).collect();
        text.push_str("error[E0308]: mismatched types\n");
        text.push_str("  --> src/lib.rs:42:9\n");
        text.push_str("warning: unused variable: `x`\n");
        text.push_str("failures:\n\n");
        text.push_str("    module::tests::alpha\n");
        text.push_str("    module::tests::beta\n\n");
        text.push_str(
            "test result: FAILED. 3 passed; 2 failed; 0 ignored; 0 measured; 0 filtered out\n",
        );
        text
    }

    fn posttool_stdin(
        cwd: &Path,
        tool_name: &str,
        command: &str,
        response: serde_json::Value,
    ) -> String {
        serde_json::json!({
            "session_id": "abc123",
            "transcript_path": "/tmp/t.jsonl",
            "cwd": cwd.display().to_string(),
            "hook_event_name": "PostToolUse",
            "tool_name": tool_name,
            "tool_input": {"command": command},
            "tool_response": response,
            "tool_use_id": "toolu_01ABC123",
        })
        .to_string()
    }

    /// A temp home + repo + state dir, so the config load and the output
    /// store inside `run_posttool` never touch the developer's own machine.
    struct PostToolRig {
        _dir: tempfile::TempDir,
        repo: PathBuf,
        state: PathBuf,
        env: std::collections::HashMap<String, String>,
        _home: crate::commands::ctx::testenv::HomeGuard,
    }

    fn posttool_rig(extra_env: &[(&str, &str)]) -> PostToolRig {
        let dir = tempfile::tempdir().expect("tempdir");
        let home = dir.path().join("home");
        let repo = dir.path().join("repo");
        let state = dir.path().join("state");
        std::fs::create_dir_all(&home).expect("mkdir home");
        std::fs::create_dir_all(&repo).expect("mkdir repo");
        let guard = crate::commands::ctx::testenv::HomeGuard::set(&home);
        let mut env: std::collections::HashMap<String, String> = [(
            crate::commands::ctx::state::STATE_ENV.to_string(),
            state.display().to_string(),
        )]
        .into();
        env.extend(
            extra_env
                .iter()
                .map(|(k, v)| ((*k).to_string(), (*v).to_string())),
        );
        PostToolRig {
            _dir: dir,
            repo,
            state,
            env,
            _home: guard,
        }
    }

    fn run_post(rig: &PostToolRig, stdin: &str) -> String {
        let mut out = Vec::new();
        let code = run_posttool(&mut out, stdin, &|k| rig.env.get(k).cloned())
            .expect("the compact-output hook must never error");
        assert_eq!(code, 0, "the compact-output hook must never block");
        String::from_utf8(out).expect("utf8")
    }

    fn run_post_for_agent(rig: &PostToolRig, stdin: &str, agent: Option<&str>) -> String {
        let mut out = Vec::new();
        let code = run_posttool_for_agent(&mut out, stdin, &|k| rig.env.get(k).cloned(), agent)
            .expect("the compact-output hook must never error");
        assert_eq!(code, 0, "the compact-output hook must never block");
        String::from_utf8(out).expect("utf8")
    }

    /// The headline behaviour: a large `Bash` result is replaced by a
    /// shape-correct `updatedToolOutput` whose `stdout` keeps every line that
    /// carried signal, and the original is on disk byte for byte.
    #[test]
    fn posttool_replaces_a_large_bash_result_with_a_shape_correct_summary() {
        let rig = posttool_rig(&[]);
        let original = noisy_output();
        let out = run_post(
            &rig,
            &posttool_stdin(
                &rig.repo,
                "Bash",
                "cargo test",
                serde_json::json!({
                    "stdout": original,
                    "stderr": "",
                    "interrupted": false,
                    "isImage": false,
                }),
            ),
        );

        let parsed: serde_json::Value = serde_json::from_str(out.trim()).expect("json");
        let hook = &parsed["hookSpecificOutput"];
        assert_eq!(hook["hookEventName"], "PostToolUse");
        let replacement = &hook["updatedToolOutput"];
        // The exact `Bash` output shape: claude ignores a value that does not
        // match its own schema and uses the original instead.
        assert_eq!(replacement["stderr"], "");
        assert_eq!(replacement["interrupted"], false);
        assert_eq!(replacement["isImage"], false);
        let summary = replacement["stdout"].as_str().expect("a summary");

        assert!(summary.contains("test result: FAILED"), "{summary}");
        for name in ["module::tests::alpha", "module::tests::beta"] {
            assert!(summary.contains(name), "must name {name}: {summary}");
        }
        assert!(
            summary.contains("error[E0308]") && summary.contains("--> src/lib.rs:42:9"),
            "{summary}"
        );
        assert!(
            summary.contains("full output: zirv ctx output show"),
            "{summary}"
        );
        assert!(summary.len() <= 4096, "{} bytes", summary.len());
        assert!(
            summary.len() < original.len() / 4,
            "the summary must be a fraction of the original: {} vs {}",
            summary.len(),
            original.len()
        );

        // The stored file is byte-identical to what claude handed the hook.
        let dir = rig
            .state
            .join("outputs")
            .join(crate::commands::ctx::state::repo_slug(&rig.repo));
        let log = std::fs::read_dir(&dir)
            .expect("outputs dir")
            .flatten()
            .map(|e| e.path())
            .find(|p| p.extension().is_some_and(|ext| ext == "log"))
            .expect("a stored log");
        assert_eq!(
            std::fs::read(&log).expect("read log"),
            original.as_bytes(),
            "the persisted output must be byte-identical to the original"
        );
    }

    /// Below the threshold there is nothing to save, so the original stands.
    #[test]
    fn posttool_leaves_a_small_result_alone() {
        let rig = posttool_rig(&[]);
        let out = run_post(
            &rig,
            &posttool_stdin(
                &rig.repo,
                "Bash",
                "git status",
                serde_json::json!({
                    "stdout": "On branch main\nnothing to commit\n",
                    "stderr": "",
                    "interrupted": false,
                    "isImage": false,
                }),
            ),
        );
        assert!(out.is_empty(), "{out}");
    }

    /// Issue #418: a copilot `postToolUse` payload carrying a large bash
    /// result is compacted exactly like claude's own `PostToolUse` payload
    /// is -- the output is `modifiedResult.textResultForLlm`, and it names a
    /// retrieval id (`zirv ctx output show <id>`) rather than dropping the
    /// original.
    #[test]
    fn copilot_posttool_compaction_carries_a_retrieval_id() {
        let rig = posttool_rig(&[]);
        let original = noisy_output();
        let copilot_stdin = serde_json::json!({
            "sessionId": "copilot-session",
            "cwd": rig.repo.display().to_string(),
            "toolArgs": {"command": "cargo test"},
            "toolResult": {"resultType": "success", "textResultForLlm": original},
        })
        .to_string();
        let out = run_post_for_agent(&rig, &copilot_stdin, Some("copilot"));
        let parsed: serde_json::Value = serde_json::from_str(out.trim()).expect("json");
        let summary = parsed["modifiedResult"]["textResultForLlm"]
            .as_str()
            .expect("a summary");
        assert_eq!(parsed["modifiedResult"]["resultType"], "success");
        assert!(
            summary.contains("full output: zirv ctx output show"),
            "must carry a retrieval id: {summary}"
        );
        assert!(summary.len() < original.len() / 4, "{summary}");
    }

    /// Issue #418: droid has no verified result-replacement contract at all
    /// (`Capabilities::post_tool_hook` is `false`), so `posttool --agent
    /// droid` must print nothing and still exit 0 rather than attempt a
    /// projection that could never produce a usable envelope.
    #[test]
    fn posttool_droid_is_unsupported_and_silent() {
        let rig = posttool_rig(&[]);
        let droid_stdin = posttool_stdin(
            &rig.repo,
            "Execute",
            "cargo test",
            serde_json::json!({"command": "cargo test"}),
        );
        let out = run_post_for_agent(&rig, &droid_stdin, Some("droid"));
        assert!(out.is_empty(), "{out}");
    }

    /// Issue #422: a large `Bash` result that gets replaced also records
    /// exactly one `compacted` row to the compaction ledger, with `bytes_in`
    /// the original size and `bytes_out` the (much smaller) summary size.
    #[test]
    fn posttool_records_a_compacted_row_in_the_ledger() {
        let rig = posttool_rig(&[]);
        let original = noisy_output();
        run_post(
            &rig,
            &posttool_stdin(
                &rig.repo,
                "Bash",
                "cargo test",
                serde_json::json!({
                    "stdout": original,
                    "stderr": "",
                    "interrupted": false,
                    "isImage": false,
                }),
            ),
        );

        let state = crate::commands::ctx::state::StateDir::from_root(rig.state.clone());
        let rows = crate::commands::ctx::ledger::rows_for_test(&state);
        assert_eq!(rows.len(), 1, "exactly one row, {rows:?}");
        let (outcome, bytes_in, bytes_out) = &rows[0];
        assert_eq!(outcome, "compacted");
        assert_eq!(*bytes_in, original.len() as u64);
        assert!(
            *bytes_out < *bytes_in,
            "a compacted row's bytes_out must be smaller: {bytes_out} vs {bytes_in}"
        );
    }

    /// Below-threshold results also get a ledger row -- `below_threshold`,
    /// with `bytes_out` equal to `bytes_in` since nothing was replaced.
    #[test]
    fn posttool_records_a_below_threshold_row_in_the_ledger() {
        let rig = posttool_rig(&[]);
        let small = "On branch main\nnothing to commit\n";
        run_post(
            &rig,
            &posttool_stdin(
                &rig.repo,
                "Bash",
                "git status",
                serde_json::json!({
                    "stdout": small,
                    "stderr": "",
                    "interrupted": false,
                    "isImage": false,
                }),
            ),
        );

        let state = crate::commands::ctx::state::StateDir::from_root(rig.state.clone());
        let rows = crate::commands::ctx::ledger::rows_for_test(&state);
        assert_eq!(rows.len(), 1, "exactly one row, {rows:?}");
        let (outcome, bytes_in, bytes_out) = &rows[0];
        assert_eq!(outcome, "below_threshold");
        assert_eq!(*bytes_in, small.len() as u64);
        assert_eq!(*bytes_out, *bytes_in);
    }

    /// Every fail-open path: a tool this hook knows nothing about, an image
    /// result, a result claude already offloaded itself, and stdin that is
    /// not the documented payload at all.
    #[test]
    fn posttool_fails_open_on_everything_it_does_not_understand() {
        let rig = posttool_rig(&[]);
        let big = noisy_output();

        for stdin in [
            posttool_stdin(
                &rig.repo,
                "Read",
                "",
                serde_json::json!({"stdout": big.clone()}),
            ),
            posttool_stdin(
                &rig.repo,
                "Bash",
                "cargo test",
                serde_json::json!({
                    "stdout": big.clone(),
                    "stderr": "",
                    "interrupted": false,
                    "isImage": true,
                }),
            ),
            posttool_stdin(
                &rig.repo,
                "Bash",
                "cargo test",
                serde_json::json!({
                    "stdout": format!("Output too large, saved to /tmp/x.txt\n{big}"),
                    "stderr": "",
                    "interrupted": false,
                    "isImage": false,
                }),
            ),
            "this is not json".to_string(),
            "{".to_string(),
            "null".to_string(),
            "{\"tool_name\":\"Bash\",\"tool_response\":\"not an object\"}".to_string(),
        ] {
            let out = run_post(&rig, &stdin);
            assert!(out.is_empty(), "must stay silent on {stdin:.60}: {out}");
        }
    }

    /// The operator's own switch is real, and it is the operator's alone --
    /// `[output] compact` is `REPO_FORBIDDEN` in both directions (see
    /// `config::OutputConfig`).
    #[test]
    fn posttool_respects_the_operator_switch() {
        let rig = posttool_rig(&[("ZIRV_CTX_OUTPUT_COMPACT", "false")]);
        let out = run_post(
            &rig,
            &posttool_stdin(
                &rig.repo,
                "Bash",
                "cargo test",
                serde_json::json!({
                    "stdout": noisy_output(),
                    "stderr": "",
                    "interrupted": false,
                    "isImage": false,
                }),
            ),
        );
        assert!(out.is_empty(), "{out}");
    }

    /// Review finding 6: a model reads a READER's output verbatim before
    /// editing against it, so head/tail there does not cost tokens, it
    /// corrupts the edit. `cat`/`sed`/`rg`/piped output are never compacted
    /// at any size; `git diff`/`show` moved to a bounded `Diff` scope (issue
    /// #412), so this fixture -- comfortably below the default
    /// `diff_max_bytes` -- still reaches the model untouched, exercising
    /// that generous threshold rather than an unconditional exemption. See
    /// `posttool_compacts_a_diff_only_past_diff_max_bytes` for what happens
    /// once a diff clears it. `rg` needs `compact_search` explicitly turned
    /// off here: it defaults `true`, under which `rg` is `Shape`-compacted
    /// instead of left verbatim -- see
    /// `posttool_compacts_rg_results_only_when_compact_search_is_enabled`.
    #[test]
    fn posttool_never_compacts_a_reader_command() {
        let rig = posttool_rig(&[("ZIRV_CTX_OUTPUT_COMPACT_SEARCH", "false")]);
        let big: String = (1..=2000)
            .map(|i| format!("line {i} of the file\n"))
            .collect();
        assert!(
            big.len() > 20_000,
            "the fixture must be past every threshold"
        );
        for command in [
            "cat src/lib.rs",
            "sed -n '1,400p' src/lib.rs",
            "rg TODO src",
            "git diff HEAD~1",
            "git show HEAD",
            "cargo test | tail -20",
        ] {
            let out = run_post(
                &rig,
                &posttool_stdin(
                    &rig.repo,
                    "Bash",
                    command,
                    serde_json::json!({
                        "stdout": big.clone(),
                        "stderr": "",
                        "interrupted": false,
                        "isImage": false,
                    }),
                ),
            );
            assert!(
                out.is_empty(),
                "{command} must reach the model verbatim: {out}"
            );
        }
    }

    /// Review finding 4: zirv's own retrieval surface must never be
    /// compacted. It used to be, so `output show --range 1-1` on one huge
    /// captured line came back as a SECOND summary and no range could ever
    /// reach the original.
    #[test]
    fn posttool_never_compacts_its_own_retrieval_surface() {
        let rig = posttool_rig(&[]);
        let big: String = (1..=2000).map(|i| format!("stored line {i}\n")).collect();
        for command in [
            "zirv ctx output show abc123 --range 1-1",
            "zirv ctx output list",
            "zirv ctx run --full -- cargo test",
        ] {
            let out = run_post(
                &rig,
                &posttool_stdin(
                    &rig.repo,
                    "Bash",
                    command,
                    serde_json::json!({
                        "stdout": big.clone(),
                        "stderr": "",
                        "interrupted": false,
                        "isImage": false,
                    }),
                ),
            );
            assert!(
                out.is_empty(),
                "{command} hands back text on purpose: {out}"
            );
        }
    }

    /// A synthetic unified diff of `file_count` files, each with one hunk of
    /// `lines_per_file` `+`/`-` pairs -- big enough, at a large `file_count`,
    /// to comfortably clear both `diff_max_bytes` and `max_summary_bytes`.
    fn fake_diff(file_count: usize, lines_per_file: usize) -> String {
        let mut text = String::new();
        for i in 0..file_count {
            text.push_str(&format!("diff --git a/src/gen{i}.rs b/src/gen{i}.rs\n"));
            text.push_str("index 1111111..2222222 100644\n");
            text.push_str(&format!("--- a/src/gen{i}.rs\n"));
            text.push_str(&format!("+++ b/src/gen{i}.rs\n"));
            text.push_str(&format!("@@ -1,{lines_per_file} +1,{lines_per_file} @@\n"));
            for line in 0..lines_per_file {
                text.push_str(&format!("-old body line {line} of the generated file\n"));
                text.push_str(&format!("+new body line {line} of the generated file\n"));
            }
        }
        text
    }

    /// Issue #412, acceptance criterion: a 20 KB diff is left untouched (its
    /// bytes are well below the default `diff_max_bytes`), and a diff that
    /// clears `diff_max_bytes` is replaced with a bounded per-file listing --
    /// never a head/tail, and never a `@@` hunk header.
    #[test]
    fn posttool_compacts_a_diff_only_past_diff_max_bytes() {
        let rig = posttool_rig(&[]);
        let small_diff = fake_diff(20, 20);
        assert!(
            small_diff.len() < 65536,
            "fixture must stay below the default diff_max_bytes: {} bytes",
            small_diff.len()
        );
        let out = run_post(
            &rig,
            &posttool_stdin(
                &rig.repo,
                "Bash",
                "git diff main...HEAD",
                serde_json::json!({
                    "stdout": small_diff,
                    "stderr": "",
                    "interrupted": false,
                    "isImage": false,
                }),
            ),
        );
        assert!(
            out.is_empty(),
            "a diff well below diff_max_bytes must reach the model untouched: {out}"
        );

        let big_diff = fake_diff(400, 60);
        assert!(
            big_diff.len() > 65536,
            "fixture must clear the default diff_max_bytes: {} bytes",
            big_diff.len()
        );
        let out = run_post(
            &rig,
            &posttool_stdin(
                &rig.repo,
                "Bash",
                "git diff main...HEAD",
                serde_json::json!({
                    "stdout": big_diff.clone(),
                    "stderr": "",
                    "interrupted": false,
                    "isImage": false,
                }),
            ),
        );
        let parsed: serde_json::Value = serde_json::from_str(out.trim()).expect("json");
        let summary = parsed["hookSpecificOutput"]["updatedToolOutput"]["stdout"]
            .as_str()
            .expect("a summary");
        assert!(!summary.contains("@@"), "{summary}");
        assert!(summary.contains("totals:"), "{summary}");
        assert!(summary.contains("400 files changed"), "{summary}");
        assert!(
            summary.contains("full output: zirv ctx output show"),
            "{summary}"
        );
        assert!(summary.len() <= 4096, "{} bytes", summary.len());

        // The stored file is still byte-identical to what claude handed the
        // hook -- the diff's own bytes were never touched, only the summary.
        let dir = rig
            .state
            .join("outputs")
            .join(crate::commands::ctx::state::repo_slug(&rig.repo));
        let log = std::fs::read_dir(&dir)
            .expect("outputs dir")
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|ext| ext == "log"))
            .max_by_key(|p| std::fs::metadata(p).map(|m| m.len()).unwrap_or(0))
            .expect("a stored log");
        assert_eq!(
            std::fs::read(&log).expect("read log"),
            big_diff.as_bytes(),
            "the persisted diff must be byte-identical to the original"
        );
    }

    /// The narrow-only operator knob actually gates the hook: lowering
    /// `diff_max_bytes` compacts a diff that the default would have left
    /// alone.
    #[test]
    fn posttool_honours_a_lowered_diff_max_bytes() {
        let rig = posttool_rig(&[("ZIRV_CTX_OUTPUT_DIFF_MAX_BYTES", "1024")]);
        let diff = fake_diff(5, 5);
        assert!(diff.len() > 1024, "{}", diff.len());
        let out = run_post(
            &rig,
            &posttool_stdin(
                &rig.repo,
                "Bash",
                "git show HEAD",
                serde_json::json!({
                    "stdout": diff,
                    "stderr": "",
                    "interrupted": false,
                    "isImage": false,
                }),
            ),
        );
        assert!(
            !out.is_empty(),
            "a lowered diff_max_bytes must still compact a diff above it"
        );
    }

    /// Issue #413 end to end: a `pytest` result over the compaction
    /// threshold gets the family extractor's exact names and locations
    /// (`testrun::extract_pytest`), not the generic scan's diagnostic-line
    /// guesswork.
    #[test]
    fn posttool_compacts_a_pytest_failure_with_names_and_locations() {
        let rig = posttool_rig(&[]);
        let mut output: String = (1..=400)
            .map(|i| format!("collecting item {i}\n"))
            .collect();
        output.push_str(
            "=========================== short test summary info ===========================\n",
        );
        output.push_str("FAILED test_foo.py::test_alpha - AssertionError: 1 != 2\n");
        output.push_str(
            "========================= 1 failed, 400 passed in 1.2s =========================\n",
        );
        assert!(output.len() >= 4096, "{}", output.len());

        let out = run_post(
            &rig,
            &posttool_stdin(
                &rig.repo,
                "Bash",
                "pytest -q",
                serde_json::json!({
                    "stdout": output,
                    "stderr": "",
                    "interrupted": false,
                    "isImage": false,
                }),
            ),
        );
        let parsed: serde_json::Value = serde_json::from_str(out.trim()).expect("json");
        let summary = parsed["hookSpecificOutput"]["updatedToolOutput"]["stdout"]
            .as_str()
            .expect("a summary");
        assert!(summary.contains("test_foo.py::test_alpha"), "{summary}");
        assert!(summary.contains("test_foo.py"), "{summary}");
        assert!(summary.contains("AssertionError: 1 != 2"), "{summary}");
    }

    /// Issue #413, "green is never red": a clean `pytest` run past the
    /// compaction threshold must never surface a failure block, even though
    /// the generic scan's own diagnostic heuristic (`error:`/`warning:`)
    /// never runs a pytest-specific check to rule that out on its own.
    #[test]
    fn posttool_never_reports_a_clean_pytest_run_as_red() {
        let rig = posttool_rig(&[]);
        let mut output: String = (1..=400)
            .map(|i| format!("test_foo.py::test_{i} PASSED\n"))
            .collect();
        output.push_str(
            "============================== 400 passed in 1.2s ===============================\n",
        );
        assert!(output.len() >= 4096, "{}", output.len());

        let out = run_post(
            &rig,
            &posttool_stdin(
                &rig.repo,
                "Bash",
                "pytest -q",
                serde_json::json!({
                    "stdout": output,
                    "stderr": "",
                    "interrupted": false,
                    "isImage": false,
                }),
            ),
        );
        let parsed: serde_json::Value = serde_json::from_str(out.trim()).expect("json");
        let summary = parsed["hookSpecificOutput"]["updatedToolOutput"]["stdout"]
            .as_str()
            .expect("a summary");
        assert!(
            !summary.to_ascii_uppercase().contains("FAILED"),
            "a clean pytest run must never be reported red: {summary}"
        );
        assert!(summary.contains("all tests passed"), "{summary}");
    }

    /// Review finding 6b/6c: a modelled build/test family is compacted from
    /// the low threshold, because the summary provably keeps the lines that
    /// matter; an unrecognised producer only past the much higher generic
    /// threshold, and its summary says which lines it dropped.
    #[test]
    fn posttool_paces_a_known_family_and_an_unknown_one_differently() {
        let rig = posttool_rig(&[]);
        let five_kb: String = (1..=250)
            .map(|i| format!("compiling crate number {i}\n"))
            .collect();
        assert!((4096..16384).contains(&five_kb.len()), "{}", five_kb.len());

        let known = run_post(
            &rig,
            &posttool_stdin(
                &rig.repo,
                "Bash",
                "cargo test",
                serde_json::json!({
                    "stdout": five_kb.clone(),
                    "stderr": "",
                    "interrupted": false,
                    "isImage": false,
                }),
            ),
        );
        assert!(!known.is_empty(), "cargo test at 5 KB must be compacted");

        let unknown = run_post(
            &rig,
            &posttool_stdin(
                &rig.repo,
                "Bash",
                "some-tool --report",
                serde_json::json!({
                    "stdout": five_kb,
                    "stderr": "",
                    "interrupted": false,
                    "isImage": false,
                }),
            ),
        );
        assert!(
            unknown.is_empty(),
            "an unrecognised 5 KB result stays verbatim: {unknown}"
        );

        // One `warning:` line keeps this fixture out of issue #409b's
        // clean-run one-liner (this hook path has no exit code of its own,
        // so "nothing flagged at all" is what that shape looks for) -- this
        // test is about the omitted-range message a non-clean generic
        // summary states, not about the clean path.
        let mut twenty_kb: String = (1..=1000)
            .map(|i| format!("some tool output line {i}\n"))
            .collect();
        twenty_kb.push_str("warning: something noteworthy happened\n");
        assert!(twenty_kb.len() > 16384);
        let big_unknown = run_post(
            &rig,
            &posttool_stdin(
                &rig.repo,
                "Bash",
                "some-tool --report",
                serde_json::json!({
                    "stdout": twenty_kb,
                    "stderr": "",
                    "interrupted": false,
                    "isImage": false,
                }),
            ),
        );
        let parsed: serde_json::Value = serde_json::from_str(big_unknown.trim()).expect("json");
        let summary = parsed["hookSpecificOutput"]["updatedToolOutput"]["stdout"]
            .as_str()
            .expect("a summary");
        assert!(
            summary.contains("lines omitted between line"),
            "an unrecognised shape's summary must say what it cut: {summary}"
        );
        assert!(summary.contains("zirv ctx output show"), "{summary}");
    }

    /// `interrupted` is a fact about the run, not about the output, so it
    /// survives the replacement rather than being reset to the default.
    #[test]
    fn posttool_carries_the_interrupted_flag_through() {
        let rig = posttool_rig(&[]);
        let out = run_post(
            &rig,
            &posttool_stdin(
                &rig.repo,
                "Bash",
                "cargo test",
                serde_json::json!({
                    "stdout": noisy_output(),
                    "stderr": "",
                    "interrupted": true,
                    "isImage": false,
                }),
            ),
        );
        let parsed: serde_json::Value = serde_json::from_str(out.trim()).expect("json");
        assert_eq!(
            parsed["hookSpecificOutput"]["updatedToolOutput"]["interrupted"],
            true
        );
    }

    // -- Issue #416: gh/glab template boilerplate stripping ----------------

    /// A small `gh pr view` body stays below `compact_generic_min_bytes` (gh
    /// is not a `KNOWN_PROGRAMS` member) and must reach the model untouched,
    /// exactly like any other below-threshold `Generic` result.
    #[test]
    fn posttool_leaves_a_small_gh_pr_view_body_untouched() {
        let rig = posttool_rig(&[]);
        let small = "## Description\n\nA short PR body.\n";
        let out = run_post(
            &rig,
            &posttool_stdin(
                &rig.repo,
                "Bash",
                "gh pr view 123",
                serde_json::json!({
                    "stdout": small,
                    "stderr": "",
                    "interrupted": false,
                    "isImage": false,
                }),
            ),
        );
        assert!(
            out.is_empty(),
            "a below-threshold body must be left alone: {out}"
        );
    }

    // -- Issue #414: opt-in shape-aware search/listing compaction ----------

    /// `[output] compact_search` end to end: off (via
    /// `ZIRV_CTX_OUTPUT_COMPACT_SEARCH=false`, since the operator turned it
    /// off), a large `rg` result reaches the model untouched, same as any
    /// other reader; on (the default, no override needed), the identical
    /// result is replaced with a grouped, bounded summary.
    #[test]
    fn posttool_compacts_rg_results_only_when_compact_search_is_enabled() {
        let mut big = String::new();
        for f in 0..20 {
            for m in 0..30 {
                big.push_str(&format!(
                    "src/file{f}.rs:{}:    let todo_{m} = 1; // TODO fix this\n",
                    m + 1
                ));
            }
        }
        assert!(big.len() > 16384, "{}", big.len());

        let off_rig = posttool_rig(&[("ZIRV_CTX_OUTPUT_COMPACT_SEARCH", "false")]);
        let out = run_post(
            &off_rig,
            &posttool_stdin(
                &off_rig.repo,
                "Bash",
                "rg TODO src",
                serde_json::json!({
                    "stdout": big.clone(),
                    "stderr": "",
                    "interrupted": false,
                    "isImage": false,
                }),
            ),
        );
        assert!(
            out.is_empty(),
            "compact_search off must leave rg untouched: {out}"
        );

        let on_rig = posttool_rig(&[]);
        let out = run_post(
            &on_rig,
            &posttool_stdin(
                &on_rig.repo,
                "Bash",
                "rg TODO src",
                serde_json::json!({
                    "stdout": big,
                    "stderr": "",
                    "interrupted": false,
                    "isImage": false,
                }),
            ),
        );
        assert!(
            !out.is_empty(),
            "compact_search on (the default) must compact the same rg result"
        );
        let parsed: serde_json::Value = serde_json::from_str(out.trim()).expect("json");
        let summary = parsed["hookSpecificOutput"]["updatedToolOutput"]["stdout"]
            .as_str()
            .expect("a summary");
        assert!(summary.contains("matches in"), "{summary}");
    }

    #[test]
    fn posttool_masks_generic_nested_output_before_the_model_sees_it() {
        let state_dir = tempfile::tempdir().expect("state");
        let repo = tempfile::tempdir().expect("repo");
        let state_root = state_dir.path().display().to_string();
        // Issue #466: masking is opt-in (`obfuscate.mode` defaults to
        // `off`); this test is exercising the masking behavior itself, so
        // it opts in explicitly rather than relying on the default.
        let env = |key: &str| match key {
            crate::commands::ctx::state::STATE_ENV => Some(state_root.clone()),
            "ZIRV_CTX_OBFUSCATE_MODE" => Some("obfuscate".to_string()),
            _ => None,
        };
        let stdin = serde_json::json!({
            "session_id": "s1", "tool_use_id": "t1", "cwd": repo.path(),
            "tool_name": "Read", "tool_input": {"file_path":"notes.txt"},
            "tool_response": {
                "content": [{"type":"text","text":"owner jane@company.dk token ghp_abcdefghijklmnopqrstuvwxyz123456"}],
                "metadata": {"source":"notes.txt"}
            }
        }).to_string();
        let mut out = Vec::new();
        run_posttool(&mut out, &stdin, &env).expect("hook");
        let value: serde_json::Value = serde_json::from_slice(&out).expect("replacement");
        let rendered = value.to_string();
        assert!(!rendered.contains("jane@company.dk"), "{rendered}");
        assert!(
            !rendered.contains("ghp_abcdefghijklmnopqrstuvwxyz123456"),
            "{rendered}"
        );
        assert!(
            rendered.contains("ZIRV_PII_EMAIL_1@company.dk"),
            "{rendered}"
        );
        assert!(
            rendered.contains("ZIRV_SECRET_GITHUB_TOKEN_1"),
            "{rendered}"
        );
        assert_eq!(
            value["hookSpecificOutput"]["updatedToolOutput"]["metadata"]["source"],
            "notes.txt"
        );
    }

    #[test]
    fn posttool_withholding_preserves_the_bash_output_schema() {
        let home = tempfile::tempdir().expect("home");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());
        let state = tempfile::NamedTempFile::new().expect("unavailable state directory");
        let repo = tempfile::tempdir().expect("repo");
        let env = |key: &str| match key {
            crate::commands::ctx::state::STATE_ENV => Some(state.path().display().to_string()),
            "ZIRV_CTX_OBFUSCATE_MODE" => Some("obfuscate".into()),
            _ => None,
        };
        let stdin = serde_json::json!({
            "cwd": repo.path(), "tool_name": "Bash", "tool_input": {"command": "echo token"},
            "tool_response": {"stdout": "ghp_abcdefghijklmnopqrstuvwxyz123456", "stderr": "details", "interrupted": true, "isImage": false},
        }).to_string();
        let mut out = Vec::new();
        run_posttool(&mut out, &stdin, &env).expect("hook");
        let envelope: serde_json::Value = serde_json::from_slice(&out).expect("replacement");
        let output = &envelope["hookSpecificOutput"]["updatedToolOutput"];
        assert_eq!(output.as_object().expect("object").len(), 4);
        for key in ["stdout", "stderr"] {
            assert!(
                output[key]
                    .as_str()
                    .expect("schema string")
                    .contains("withheld")
            );
        }
        assert_eq!(output["interrupted"], true);
        assert_eq!(output["isImage"], false);
        assert!(!output.to_string().contains("ghp_"));
    }

    #[test]
    fn posttool_vault_failure_scans_anchored_patterns_in_nested_strings() {
        let home = tempfile::tempdir().expect("home");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());
        std::fs::create_dir(home.path().join(".zirv")).expect("config directory");
        std::fs::write(home.path().join(".zirv/ctx.toml"),
            "[obfuscate]\nmode = \"obfuscate\"\n[[obfuscate.patterns]]\nkind = \"CUSTOMER\"\nregex = '^CUST-[0-9]{8}$'\n",
        ).expect("config");
        let state = tempfile::NamedTempFile::new().expect("unavailable state directory");
        let repo = tempfile::tempdir().expect("repo");
        let env = |key: &str| {
            (key == crate::commands::ctx::state::STATE_ENV)
                .then(|| state.path().display().to_string())
        };
        let stdin = serde_json::json!({
            "cwd": repo.path(), "tool_name": "Read", "tool_input": {"file_path": "notes.txt"},
            "tool_response": {"results": [{"stdout": "CUST-12345678", "count": 1}]},
        })
        .to_string();
        let mut out = Vec::new();
        run_posttool(&mut out, &stdin, &env).expect("hook");
        let envelope: serde_json::Value = serde_json::from_slice(&out).expect("replacement");
        let output = &envelope["hookSpecificOutput"]["updatedToolOutput"];
        assert!(
            output["results"][0]["stdout"]
                .as_str()
                .expect("stdout")
                .contains("withheld")
        );
        assert_eq!(output["results"][0]["count"], 1);
        assert!(!output.to_string().contains("CUST-12345678"));
    }

    /// Behaviour (a) -- the headline case: a shell command that changes an
    /// EXISTING tracked file after the prompt gets the same one-time
    /// checkpoint the `PreToolUse` `Edit`/`Write` path would have shown, even
    /// though it never touches `Edit`/`Write` at all, worded for a change
    /// that already happened; a second shell call in the same prompt stays
    /// silent.
    #[test]
    fn scope_guard_shell_checkpoint_fires_once_after_an_existing_tracked_file_changes() {
        let rig = scope_guard_shell_rig();
        let lookup = |k: &str| rig.env.get(k).cloned();
        let session = "sess-shell-1";

        run_prompt(
            &mut Vec::new(),
            &scope_guard_prompt_stdin(session, rig.repo.path(), SCOPE_GUARD_T24_PROMPT),
            &lookup,
        )
        .expect("run_prompt");

        // The shell command has ALREADY run by the time `PostToolUse` fires
        // -- this simulates that after-the-fact state (`sed -i`/`cat >`/a
        // Python `open(p, "w")`), not something `run_posttool` itself does.
        std::fs::write(
            rig.repo.path().join("tracked.txt"),
            "one\nedited by shell\n",
        )
        .expect("simulate a shell edit");

        let stdin = scope_guard_bash_posttool_stdin(
            session,
            rig.repo.path(),
            "sed -i 's/one/ONE/' tracked.txt",
        );
        let mut first = Vec::new();
        run_posttool(&mut first, &stdin, &lookup).expect("run_posttool");
        let first = String::from_utf8(first).expect("utf8");
        assert!(
            first.contains("Scope checkpoint"),
            "a shell edit to an existing tracked file must show the checkpoint: {first}"
        );
        assert!(
            first.contains("tracked.txt"),
            "must name the changed file: {first}"
        );
        assert!(
            first.contains("You just changed existing file(s)"),
            "must use the after-the-fact wording: {first}"
        );
        assert!(
            first.contains("ask the user first."),
            "interactive wording by default: {first}"
        );

        let mut second = Vec::new();
        run_posttool(&mut second, &stdin, &lookup).expect("run_posttool");
        let second = String::from_utf8(second).expect("utf8");
        assert!(
            !second.contains("Scope checkpoint"),
            "a second shell call in the same prompt must stay silent: {second}"
        );
    }

    /// Behaviour (b): a tracked file already modified BEFORE the prompt --
    /// captured in `record_scope_guard_request`'s own baseline -- that is
    /// never touched again must not trigger the checkpoint.
    #[test]
    fn scope_guard_shell_checkpoint_ignores_a_file_already_modified_before_the_prompt() {
        let rig = scope_guard_shell_rig();
        let lookup = |k: &str| rig.env.get(k).cloned();
        let session = "sess-shell-2";

        std::fs::write(rig.repo.path().join("tracked.txt"), "one\nalready dirty\n")
            .expect("pre-existing modification");

        run_prompt(
            &mut Vec::new(),
            &scope_guard_prompt_stdin(session, rig.repo.path(), SCOPE_GUARD_T24_PROMPT),
            &lookup,
        )
        .expect("run_prompt");

        let stdin = scope_guard_bash_posttool_stdin(session, rig.repo.path(), "echo unrelated");
        let mut out = Vec::new();
        run_posttool(&mut out, &stdin, &lookup).expect("run_posttool");
        let out = String::from_utf8(out).expect("utf8");
        assert!(
            !out.contains("Scope checkpoint"),
            "a file already modified before the prompt, untouched since, must not trigger: {out}"
        );
    }

    /// Behaviour (c): creating only a brand-new UNTRACKED file must not
    /// trigger the checkpoint -- a new file is not a change to existing
    /// code.
    #[test]
    fn scope_guard_shell_checkpoint_ignores_a_brand_new_untracked_file() {
        let rig = scope_guard_shell_rig();
        let lookup = |k: &str| rig.env.get(k).cloned();
        let session = "sess-shell-3";

        run_prompt(
            &mut Vec::new(),
            &scope_guard_prompt_stdin(session, rig.repo.path(), SCOPE_GUARD_T24_PROMPT),
            &lookup,
        )
        .expect("run_prompt");

        std::fs::write(rig.repo.path().join("brand_new.txt"), "new\n").expect("new file");

        let stdin =
            scope_guard_bash_posttool_stdin(session, rig.repo.path(), "touch brand_new.txt");
        let mut out = Vec::new();
        run_posttool(&mut out, &stdin, &lookup).expect("run_posttool");
        let out = String::from_utf8(out).expect("utf8");
        assert!(
            !out.contains("Scope checkpoint"),
            "creating only a new untracked file must not trigger: {out}"
        );
    }

    /// A positively read-only command (`safety::jev_approve_is_read_only_
    /// local`) must skip the `git status` re-query entirely, even when a
    /// real tracked-file edit is sitting there unreported -- and the
    /// checkpoint must still fire on the very next NON-read-only call, since
    /// skipping the query must never mark the checkpoint as shown.
    #[test]
    fn scope_guard_shell_checkpoint_skips_the_requery_for_a_read_only_command() {
        let rig = scope_guard_shell_rig();
        let lookup = |k: &str| rig.env.get(k).cloned();
        let session = "sess-shell-readonly";

        run_prompt(
            &mut Vec::new(),
            &scope_guard_prompt_stdin(session, rig.repo.path(), SCOPE_GUARD_T24_PROMPT),
            &lookup,
        )
        .expect("run_prompt");

        // A real shell edit to an existing tracked file, exactly like
        // behaviour (a) above -- but this time the FIRST `PostToolUse` call
        // is a read-only command that cannot have produced it.
        std::fs::write(
            rig.repo.path().join("tracked.txt"),
            "one\nedited by shell\n",
        )
        .expect("simulate a shell edit");

        let readonly_stdin =
            scope_guard_bash_posttool_stdin(session, rig.repo.path(), "git status");
        let mut readonly_out = Vec::new();
        run_posttool(&mut readonly_out, &readonly_stdin, &lookup).expect("run_posttool");
        let readonly_out = String::from_utf8(readonly_out).expect("utf8");
        assert!(
            !readonly_out.contains("Scope checkpoint"),
            "a read-only command must skip the re-query and stay silent: {readonly_out}"
        );

        let stdin = scope_guard_bash_posttool_stdin(
            session,
            rig.repo.path(),
            "sed -i 's/one/ONE/' tracked.txt",
        );
        let mut out = Vec::new();
        run_posttool(&mut out, &stdin, &lookup).expect("run_posttool");
        let out = String::from_utf8(out).expect("utf8");
        assert!(
            out.contains("Scope checkpoint"),
            "the next non-read-only call must still find the change and fire: {out}"
        );
    }

    /// Behaviour (d): when the SAME `Bash` call also has output large enough
    /// to compact, the checkpoint's `additionalContext` and the compaction's
    /// own `updatedToolOutput` must both ride in the single envelope this
    /// hook writes -- never two JSON objects, never one dropped for the
    /// other.
    #[test]
    fn scope_guard_shell_checkpoint_merges_into_the_compaction_envelope() {
        let rig = scope_guard_shell_rig();
        let lookup = |k: &str| rig.env.get(k).cloned();
        let session = "sess-shell-4";

        run_prompt(
            &mut Vec::new(),
            &scope_guard_prompt_stdin(session, rig.repo.path(), SCOPE_GUARD_T24_PROMPT),
            &lookup,
        )
        .expect("run_prompt");

        std::fs::write(
            rig.repo.path().join("tracked.txt"),
            "one\nedited by shell\n",
        )
        .expect("simulate a shell edit");

        let stdin = serde_json::json!({
            "session_id": session,
            "cwd": rig.repo.path().display().to_string(),
            "tool_name": "Bash",
            "tool_input": {"command": "cargo test"},
            "tool_response": {
                "stdout": noisy_output(),
                "stderr": "",
                "interrupted": false,
                "isImage": false,
            },
            "tool_use_id": "toolu_scope_guard_compact",
        })
        .to_string();

        let mut out = Vec::new();
        run_posttool(&mut out, &stdin, &lookup).expect("run_posttool");
        let out = String::from_utf8(out).expect("utf8");
        let parsed: serde_json::Value = serde_json::from_str(out.trim()).expect("json");
        let hook = &parsed["hookSpecificOutput"];
        assert_eq!(hook["hookEventName"], "PostToolUse");
        assert!(
            hook.get("updatedToolOutput")
                .and_then(|v| v.get("stdout"))
                .is_some(),
            "the compaction envelope must still be present: {out}"
        );
        assert!(
            hook["additionalContext"]
                .as_str()
                .is_some_and(|note| note.contains("Scope checkpoint")),
            "the same envelope must also carry the checkpoint: {out}"
        );
    }

    /// #834: with the key on and nothing addressed to the session, the hook output is
    /// byte-identical to the key-off output.
    #[test]
    fn mid_turn_mail_leaves_an_empty_inbox_output_byte_identical() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let home = tempfile::tempdir().expect("home");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());
        let state = tmp.path().join("state");
        std::fs::create_dir_all(state.join("mail")).expect("mail dir");
        let stdin = serde_json::json!({
            "session_id": "abcdef12-3456-4789-8abc-def012345678",
            "cwd": tmp.path().display().to_string(),
            "tool_name": "Read",
            "tool_input": {},
            "tool_response": {},
        })
        .to_string();
        let run = |mid_turn: &str| {
            let mut env = std::collections::HashMap::new();
            env.insert(
                crate::commands::ctx::state::STATE_ENV.to_string(),
                state.display().to_string(),
            );
            env.insert(
                crate::commands::ctx::adapters::SESSION_ENV.to_string(),
                "abcdef12-3456-4789-8abc-def012345678".to_string(),
            );
            env.insert("ZIRV_CTX_MAIL_MID_TURN".to_string(), mid_turn.to_string());
            let mut out = Vec::new();
            run_posttool(&mut out, &stdin, &|k| env.get(k).cloned()).expect("posttool");
            out
        };
        assert_eq!(run("true"), run("false"));
    }

    #[test]
    fn a_subagent_payload_is_recognised_by_its_agent_id() {
        let lead: PostToolPayload = serde_json::from_str(r#"{"tool_name":"Read"}"#).expect("lead");
        let sub: PostToolPayload =
            serde_json::from_str(r#"{"tool_name":"Read","agent_id":"a1b2"}"#).expect("sub");
        assert!(lead.agent_id.is_empty());
        assert_eq!(sub.agent_id, "a1b2");
    }
}
