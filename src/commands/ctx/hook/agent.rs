//! Subagent-context variants of the PreToolUse/PostToolUse hooks, used
//! when a dispatched worker's own harness re-enters through this binary.

use std::io::Write;
use std::path::{Path, PathBuf};

use super::checkpoints::cfg_or_operator_only_gate;
use super::posttool::{run_posttool, run_posttool_with};
use super::pretool_run::{pretool_output, run_pretool};
use crate::commands::ctx::CtxResult;
use crate::commands::ctx::config::{CtxConfig, EnvLookup};
use crate::commands::ctx::state::StateDir;

// -- Non-Claude agent payload projection (#418) -------------------------

/// Project non-Claude PreToolUse payloads through the shared guard and
/// translate verdicts back; unknown or malformed payloads fail open (#418).
pub fn run_pretool_for_agent<W: Write>(
    w: &mut W,
    stdin: &str,
    env: EnvLookup<'_>,
    agent: Option<&str>,
) -> CtxResult<i32> {
    match agent {
        None | Some("claude") => run_pretool_with_rehydration(w, stdin, env),
        Some(name) => {
            let Some(projected) = crate::commands::ctx::hook_project::project_pretool(name, stdin)
            else {
                return Ok(0);
            };
            let mut buf: Vec<u8> = Vec::new();
            let code = run_pretool_with_rehydration(&mut buf, &projected, env)?;
            let claude_envelope = String::from_utf8(buf)
                .ok()
                .map(|text| text.trim().to_string())
                .filter(|text| !text.is_empty());
            if let Some(translated) = crate::commands::ctx::hook_project::translate_pretool_envelope(
                name,
                claude_envelope,
            ) {
                let _ = writeln!(w, "{translated}");
            }
            Ok(code)
        }
    }
}

// Model-dispatch tools must not receive local vault values (#466).
pub(crate) const REHYDRATION_TOOLS: &[&str] = &[
    "Bash",
    "PowerShell",
    "Edit",
    "Write",
    "MultiEdit",
    "NotebookEdit",
    "Read",
    "Grep",
    "Glob",
];

fn run_pretool_with_rehydration<W: Write>(
    w: &mut W,
    stdin: &str,
    env: EnvLookup<'_>,
) -> CtxResult<i32> {
    let Ok(mut raw) = serde_json::from_str::<serde_json::Value>(stdin) else {
        return run_pretool(w, stdin, env);
    };
    let tool = raw
        .get("tool_name")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default()
        .to_string();
    if !REHYDRATION_TOOLS.contains(&tool.as_str()) {
        return run_pretool(w, stdin, env);
    }
    let cwd = raw
        .get("cwd")
        .and_then(serde_json::Value::as_str)
        .filter(|cwd| !cwd.is_empty())
        .map(PathBuf::from)
        .or_else(|| std::env::current_dir().ok())
        .unwrap_or_else(|| PathBuf::from("."));
    let cfg = cfg_or_operator_only_gate(&cwd, env);
    if cfg.obfuscate.mode == crate::commands::ctx::config::ObfuscateMode::Off {
        return run_pretool(w, stdin, env);
    }
    let Some(original_input) = raw.get("tool_input").cloned() else {
        return run_pretool(w, stdin, env);
    };
    if shared_placeholder_artifact(&raw, &cwd) {
        return run_pretool(w, stdin, env);
    }
    let had_placeholder =
        crate::commands::ctx::obfuscate::contains_placeholder(&original_input.to_string());
    let Ok(state) = StateDir::resolve(env) else {
        if had_placeholder {
            let _ = writeln!(
                w,
                "{}",
                pretool_output(
                    "zirv refused the tool call because its placeholder vault is unavailable"
                )
            );
            return Ok(0);
        }
        return run_pretool(w, stdin, env);
    };
    let mut rehydrated = original_input.clone();
    if let Err(error) =
        crate::commands::ctx::obfuscate_store::rehydrate_json(state.root(), &cwd, &mut rehydrated)
    {
        if had_placeholder {
            let session = env(crate::commands::ctx::adapters::SESSION_ENV)
                .unwrap_or_else(|| "unknown".to_string());
            let _ = crate::commands::ctx::log::append(
                &state,
                &crate::commands::ctx::log::Decision {
                    ts: crate::commands::ctx::state::now_secs(),
                    session: &session,
                    verb: "pretool",
                    verdict: "blocked",
                    score: 0,
                    action: "obfuscate-rehydration-miss",
                    detail: "placeholder could not be resolved locally",
                    observed_at: None,
                },
            );
            let _ = writeln!(
                w,
                "{}",
                pretool_output(&format!(
                    "zirv refused the tool call because its placeholder vault could not be read: {error}"
                ))
            );
            return Ok(0);
        }
        return run_pretool(w, stdin, env);
    }
    if rehydrated == original_input {
        return run_pretool(w, stdin, env);
    }
    raw["tool_input"] = rehydrated.clone();
    let prepared = raw.to_string();
    let mut inner = Vec::new();
    let code = run_pretool(&mut inner, &prepared, env)?;
    let existing = String::from_utf8(inner).unwrap_or_default();
    let trimmed = existing.trim();
    let mut envelope = if trimmed.is_empty() {
        serde_json::json!({
            "hookSpecificOutput": {
                "hookEventName": "PreToolUse"
            }
        })
    } else if let Ok(value) = serde_json::from_str::<serde_json::Value>(trimmed) {
        value
    } else {
        let _ = write!(w, "{existing}");
        return Ok(code);
    };
    // The base guard needs rehydrated commands, but its safety reason may quote
    // them. Drop that reason for Bash/PowerShell and use the generic recheck
    // below so vault values never reach a model-facing envelope (#466, #769).
    if matches!(tool.as_str(), "Bash" | "PowerShell")
        && let Some(hook_output) = envelope
            .get_mut("hookSpecificOutput")
            .and_then(serde_json::Value::as_object_mut)
    {
        hook_output.remove("permissionDecisionReason");
    }
    if envelope
        .pointer("/hookSpecificOutput/permissionDecision")
        .and_then(serde_json::Value::as_str)
        == Some("deny")
        && !matches!(tool.as_str(), "Bash" | "PowerShell")
    {
        let _ = writeln!(w, "{envelope}");
        return Ok(code);
    }
    if let Some(overrides) = envelope
        .pointer("/hookSpecificOutput/updatedInput")
        .and_then(serde_json::Value::as_object)
        .cloned()
        && let Some(target) = rehydrated.as_object_mut()
    {
        for (key, value) in overrides {
            target.insert(key, value);
        }
    }
    if matches!(tool.as_str(), "Bash" | "PowerShell") {
        // Recheck the final command; parallel hooks only saw placeholders (#466).
        raw["tool_input"] = rehydrated.clone();
        let verdict = CtxConfig::load(&cwd, env).and_then(|cfg| {
            crate::commands::ctx::safety::run_check_hook_with_verdict(
                &cfg,
                &mut std::io::sink(),
                &raw.to_string(),
                env,
            )
        });
        match verdict {
            Ok(Some(crate::commands::ctx::safety::Verdict::Allow)) => {
                if envelope
                    .pointer("/hookSpecificOutput/permissionDecision")
                    .is_none()
                {
                    envelope["hookSpecificOutput"]["permissionDecision"] = "allow".into();
                }
            }
            Ok(Some(crate::commands::ctx::safety::Verdict::Ask)) => {
                envelope["hookSpecificOutput"]["permissionDecision"] = "ask".into();
                envelope["hookSpecificOutput"]["permissionDecisionReason"] =
                    "zirv safety requires approval of the rehydrated command".into();
            }
            Ok(Some(crate::commands::ctx::safety::Verdict::Deny)) | Ok(None) | Err(_) => {
                let _ = writeln!(
                    w,
                    "{}",
                    pretool_output("zirv safety refused the rehydrated command")
                );
                return Ok(code);
            }
        }
    }
    envelope["hookSpecificOutput"]["updatedInput"] = rehydrated;
    let _ = writeln!(w, "{envelope}");
    Ok(code)
}

fn shared_placeholder_artifact(payload: &serde_json::Value, cwd: &Path) -> bool {
    let tool = payload
        .get("tool_name")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default();
    let Some(input) = payload.get("tool_input") else {
        return false;
    };
    if matches!(tool, "Bash" | "PowerShell") {
        // Shell writes have no structured path: conservatively test every token
        // against the shared artifact rule before expanding placeholders (#466).
        return input
            .get("command")
            .and_then(serde_json::Value::as_str)
            .is_some_and(|command| {
                command
                    .split(|c: char| c.is_whitespace() || "<>|;&()'\"`".contains(c))
                    .filter(|token| !token.is_empty())
                    .any(|token| {
                        crate::commands::ctx::obfuscate_store::is_shared_placeholder_path(
                            cwd,
                            Path::new(token),
                        )
                    })
            });
    }
    if !matches!(tool, "Write" | "Edit" | "MultiEdit" | "NotebookEdit") {
        return false;
    }
    ["file_path", "notebook_path"]
        .iter()
        .filter_map(|key| input.get(*key).and_then(serde_json::Value::as_str))
        .map(Path::new)
        .any(|path| crate::commands::ctx::obfuscate_store::is_shared_placeholder_path(cwd, path))
}

/// Project posttool results only where the adapter supports result
/// replacement; unsupported agents emit no unusable envelope (#418).
pub fn run_posttool_for_agent<W: Write>(
    w: &mut W,
    stdin: &str,
    env: EnvLookup<'_>,
    agent: Option<&str>,
) -> CtxResult<i32> {
    match agent {
        None | Some("claude") => run_posttool(w, stdin, env),
        Some("copilot") => {
            let Some(projected) =
                crate::commands::ctx::hook_project::project_posttool_copilot(stdin)
            else {
                return Ok(0);
            };
            let mut buf: Vec<u8> = Vec::new();
            // Copilot's envelope drops `additionalContext`, so mid-turn mail would be consumed unseen.
            let code = run_posttool_with(&mut buf, &projected, env, false)?;
            let claude_envelope = String::from_utf8(buf)
                .ok()
                .map(|text| text.trim().to_string())
                .filter(|text| !text.is_empty());
            if let Some(translated) =
                crate::commands::ctx::hook_project::translate_posttool_envelope(
                    "copilot",
                    stdin,
                    claude_envelope,
                )
            {
                let _ = writeln!(w, "{translated}");
            }
            Ok(code)
        }
        Some(name) => {
            eprintln!("zirv: posttool compaction is not supported for agent `{name}`");
            Ok(0)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pretool_keeps_subagent_prompt_placeholders_intact() {
        let home = tempfile::tempdir().expect("home");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());
        let state = tempfile::tempdir().expect("state");
        let repo = tempfile::tempdir().expect("repo");
        let secret = "ghp_abcdefghijklmnopqrstuvwxyz123456";
        let (masked, _) = crate::commands::ctx::obfuscate_store::obfuscate_text(
            state.path(),
            repo.path(),
            secret,
            &crate::commands::ctx::obfuscate::Options::default(),
            "test",
        )
        .expect("seed vault");
        assert_eq!(masked, "ZIRV_SECRET_GITHUB_TOKEN_1");
        let env = |key: &str| match key {
            crate::commands::ctx::state::STATE_ENV => Some(state.path().display().to_string()),
            "ZIRV_CTX_OBFUSCATE_MODE" => Some("obfuscate".into()),
            _ => None,
        };
        for tool in ["Agent", "Task", "WebFetch"] {
            let input = serde_json::json!({"prompt": format!("use {masked}")});
            let stdin = serde_json::json!({
                "cwd": repo.path(), "tool_name": tool, "tool_input": input,
            });
            let mut out = Vec::new();
            run_pretool_for_agent(&mut out, &stdin.to_string(), &env, None).expect("hook");
            let envelope = serde_json::from_slice::<serde_json::Value>(&out).unwrap_or_default();
            let effective = envelope
                .pointer("/hookSpecificOutput/updatedInput")
                .unwrap_or(&input);
            assert_eq!(effective, &input, "{tool}");
            assert!(!String::from_utf8_lossy(&out).contains(secret), "{tool}");
        }
    }

    #[test]
    fn pretool_checks_safety_after_rehydrating_the_command() {
        let home = tempfile::tempdir().expect("home");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());
        let state = tempfile::tempdir().expect("state");
        let repo = tempfile::tempdir().expect("repo");
        let command = "rm -rf /";
        let options = crate::commands::ctx::obfuscate::Options {
            literals: vec![command.into()],
            ..Default::default()
        };
        let (masked, _) = crate::commands::ctx::obfuscate_store::obfuscate_text(
            state.path(),
            repo.path(),
            command,
            &options,
            "test",
        )
        .expect("seed vault");
        for tool in ["Bash", "PowerShell"] {
            for (mode, deny, expected) in [
                ("default", false, "ask"),
                ("dontAsk", false, "ask"),
                ("default", true, "deny"),
            ] {
                let env = |key: &str| match key {
                    crate::commands::ctx::state::STATE_ENV => {
                        Some(state.path().display().to_string())
                    }
                    "ZIRV_CTX_OBFUSCATE_MODE" => Some("obfuscate".into()),
                    "ZIRV_CTX_SAFETY_DENY" if deny => Some("rm *".into()),
                    _ => None,
                };
                let stdin = serde_json::json!({
                    "cwd": repo.path(), "tool_name": tool, "permission_mode": mode,
                    "tool_input": {"command": masked},
                })
                .to_string();
                let mut out = Vec::new();
                run_pretool_for_agent(&mut out, &stdin, &env, None).expect("hook");
                let envelope: serde_json::Value = serde_json::from_slice(&out).expect("decision");
                assert_eq!(
                    envelope["hookSpecificOutput"]["permissionDecision"], expected,
                    "{tool} {mode}: {envelope}"
                );
                if expected == "ask" {
                    assert_eq!(
                        envelope["hookSpecificOutput"]["updatedInput"]["command"],
                        command
                    );
                } else {
                    assert!(envelope["hookSpecificOutput"]["updatedInput"].is_null());
                }
                assert!(
                    !envelope["hookSpecificOutput"]["permissionDecisionReason"]
                        .to_string()
                        .contains(command),
                    "{tool} {mode} deny={deny}: {envelope}"
                );
            }
        }
    }

    #[test]
    fn pretool_keeps_placeholders_in_shell_writes_to_shared_artifacts() {
        let home = tempfile::tempdir().expect("home");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());
        let state = tempfile::tempdir().expect("state");
        let repo = tempfile::tempdir().expect("repo");
        let (masked, _) = crate::commands::ctx::obfuscate_store::obfuscate_text(
            state.path(),
            repo.path(),
            "ghp_abcdefghijklmnopqrstuvwxyz123456",
            &crate::commands::ctx::obfuscate::Options::default(),
            "test",
        )
        .expect("seed vault");
        let env = |key: &str| match key {
            crate::commands::ctx::state::STATE_ENV => Some(state.path().display().to_string()),
            "ZIRV_CTX_OBFUSCATE_MODE" => Some("obfuscate".into()),
            _ => None,
        };
        for tool in ["Bash", "PowerShell"] {
            for path in [
                ".zirv/memory/example.md",
                "'.zirv/work/task/output.md'",
                ".zirv\\memory\\example.md",
            ] {
                let input = serde_json::json!({"command": format!("printf %s {masked} > {path}")});
                let stdin =
                    serde_json::json!({"cwd": repo.path(), "tool_name": tool, "tool_input": input})
                        .to_string();
                let mut out = Vec::new();
                run_pretool_for_agent(&mut out, &stdin, &env, None).expect("hook");
                let envelope =
                    serde_json::from_slice::<serde_json::Value>(&out).unwrap_or_default();
                assert_eq!(
                    envelope
                        .pointer("/hookSpecificOutput/updatedInput")
                        .unwrap_or(&input),
                    &input,
                    "{tool}: {path}"
                );
            }
        }
    }

    #[test]
    fn pretool_rehydrates_nested_json_and_merges_the_complete_input() {
        let state_dir = tempfile::tempdir().expect("state");
        let repo = tempfile::tempdir().expect("repo");
        let state = StateDir::from_root(state_dir.path().to_path_buf());
        let (masked, _) = crate::commands::ctx::obfuscate_store::obfuscate_text(
            state.root(),
            repo.path(),
            "ghp_abcdefghijklmnopqrstuvwxyz123456",
            &crate::commands::ctx::obfuscate::Options::default(),
            "test",
        )
        .expect("seed vault");
        let state_root = state.root().display().to_string();
        // Issue #466: masking is opt-in (`obfuscate.mode` defaults to
        // `off`); this test is exercising rehydration itself, so it opts in
        // explicitly rather than relying on the default.
        let env = |key: &str| match key {
            crate::commands::ctx::state::STATE_ENV => Some(state_root.clone()),
            "ZIRV_CTX_OBFUSCATE_MODE" => Some("obfuscate".to_string()),
            _ => None,
        };
        let stdin = serde_json::json!({
            "session_id":"s1", "cwd":repo.path(), "tool_name":"Bash",
            "tool_input":{
                "command":format!("printf %s {masked}"), "timeout":1234,
                "nested":{"value":masked}
            }
        })
        .to_string();
        let mut out = Vec::new();
        run_pretool_for_agent(&mut out, &stdin, &env, None).expect("hook");
        let value: serde_json::Value = serde_json::from_slice(&out).expect("rewrite");
        let input = &value["hookSpecificOutput"]["updatedInput"];
        assert_eq!(input["timeout"], 1234);
        assert_eq!(
            input["nested"]["value"],
            "ghp_abcdefghijklmnopqrstuvwxyz123456"
        );
        assert_eq!(
            input["command"],
            "printf %s ghp_abcdefghijklmnopqrstuvwxyz123456"
        );
    }
}
