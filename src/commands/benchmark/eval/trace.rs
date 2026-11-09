//! Pure trace extraction: the tool calls and final assistant text of one transcript.
//!
//! Deliberately separate from `NormalizedEvent`, which carries only a tool name and an input
//! hash; behaviour grading needs the arguments.

use std::collections::HashMap;

use serde_json::{Value, json};

#[derive(Debug, Clone, PartialEq)]
pub struct TraceCall {
    pub name: String,
    pub args: Value,
    pub is_error: bool,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Trace {
    pub calls: Vec<TraceCall>,
    pub final_text: String,
}

const SHELL_NAMES: [&str; 6] = [
    "bash",
    "exec_command",
    "exec",
    "shell",
    "local_shell",
    "shell_command",
];

/// `Bash`/`exec_command` -> `shell`, `mcp__zirv__x` -> `zirv.x`; anything else is kept.
fn normalise_name(name: &str) -> String {
    if SHELL_NAMES.contains(&name.to_ascii_lowercase().as_str()) {
        return "shell".to_string();
    }
    match name
        .strip_prefix("mcp__")
        .and_then(|rest| rest.split_once("__"))
    {
        Some((server, tool)) => format!("{server}.{tool}"),
        None => name.to_string(),
    }
}

/// Parse a Claude transcript or Codex rollout (`harness` is `claude` or `codex`).
pub fn trace(harness: &str, jsonl: &str) -> Result<Trace, String> {
    let walk = match harness {
        "claude" => walk_claude_line,
        "codex" => walk_codex_line,
        other => {
            return Err(format!(
                "unknown harness '{other}': expected claude or codex"
            ));
        }
    };
    let mut walker = Walker::default();
    for line in jsonl.lines() {
        if let Ok(value) = serde_json::from_str::<Value>(line) {
            walk(&mut walker, &value);
        }
    }
    Ok(Trace {
        calls: walker.calls,
        final_text: walker.final_text,
    })
}

#[derive(Default)]
struct Walker {
    calls: Vec<TraceCall>,
    /// Call id -> index into `calls`, so a result can mark its call and a repeat is not counted twice.
    ids: HashMap<String, usize>,
    final_text: String,
}

impl Walker {
    fn push(&mut self, id: Option<&str>, name: &str, args: Value) {
        if let Some(id) = id {
            if self.ids.contains_key(id) {
                return;
            }
            self.ids.insert(id.to_string(), self.calls.len());
        }
        self.calls.push(TraceCall {
            name: normalise_name(name),
            args,
            is_error: false,
        });
    }

    fn mark_error(&mut self, id: &str) {
        if let Some(call) = self.ids.get(id).and_then(|i| self.calls.get_mut(*i)) {
            call.is_error = true;
        }
    }
}

fn str_field<'a>(value: &'a Value, key: &str) -> Option<&'a str> {
    value.get(key).and_then(Value::as_str)
}

fn joined_text(blocks: &Value, kind: &str) -> String {
    blocks
        .as_array()
        .into_iter()
        .flatten()
        .filter(|b| str_field(b, "type") == Some(kind))
        .filter_map(|b| str_field(b, "text"))
        .collect::<Vec<_>>()
        .join("\n")
}

fn walk_claude_line(walker: &mut Walker, line: &Value) {
    let Some(content) = line.get("message").and_then(|m| m.get("content")) else {
        return;
    };
    match str_field(line, "type") {
        Some("assistant") => {
            for block in content.as_array().into_iter().flatten() {
                if str_field(block, "type") == Some("tool_use") {
                    walker.push(
                        str_field(block, "id"),
                        str_field(block, "name").unwrap_or_default(),
                        block.get("input").cloned().unwrap_or(Value::Null),
                    );
                }
            }
            let text = joined_text(content, "text");
            if !text.is_empty() {
                walker.final_text = text;
            }
        }
        Some("user") => {
            for block in content.as_array().into_iter().flatten() {
                let errored = block.get("is_error").and_then(Value::as_bool) == Some(true);
                if let (true, Some(id)) = (errored, str_field(block, "tool_use_id")) {
                    walker.mark_error(id);
                }
            }
        }
        _ => {}
    }
}

/// A shell call's command as one string: a `["bash", "-lc", "<script>"]` array is unwrapped.
fn shell_command(value: &Value) -> Option<String> {
    if let Some(text) = value.as_str() {
        return Some(text.to_string());
    }
    let parts: Vec<&str> = value.as_array()?.iter().filter_map(Value::as_str).collect();
    let joined = parts.join(" ");
    Some(crate::commands::ctx::safety::unwrap_shell_wrapper(&joined).unwrap_or(joined))
}

/// Shell calls carry their command under `command`; Codex spells it `cmd` or an argv array.
fn codex_args(name: &str, mut args: Value) -> Value {
    if !SHELL_NAMES.contains(&name.to_ascii_lowercase().as_str()) {
        return args;
    }
    let Some(map) = args.as_object_mut() else {
        return args;
    };
    if let Some(cmd) = map.remove("cmd") {
        map.insert("command".to_string(), cmd);
    }
    if let Some(command) = map.get("command").and_then(shell_command) {
        map.insert("command".to_string(), Value::String(command));
    }
    args
}

fn codex_call_args(payload: &Value) -> Value {
    let name = str_field(payload, "name").unwrap_or_default();
    if str_field(payload, "type") == Some("function_call") {
        let raw = str_field(payload, "arguments").unwrap_or("{}");
        let parsed = serde_json::from_str(raw).unwrap_or_else(|_| json!({ "arguments": raw }));
        return codex_args(name, parsed);
    }
    if let Some(command) = payload.get("command").and_then(shell_command) {
        return json!({ "command": command });
    }
    match str_field(payload, "input") {
        Some(input) => match serde_json::from_str::<Value>(input) {
            Ok(parsed) if parsed.is_object() => codex_args(name, parsed),
            _ => json!({ "input": input }),
        },
        None => json!({}),
    }
}

fn codex_output_failed(output: &Value) -> bool {
    let Some(parsed) = output
        .as_str()
        .and_then(|s| serde_json::from_str::<Value>(s).ok())
    else {
        return false;
    };
    parsed
        .get("metadata")
        .and_then(|m| m.get("exit_code"))
        .and_then(Value::as_i64)
        .is_some_and(|code| code != 0)
}

fn walk_codex_line(walker: &mut Walker, line: &Value) {
    let Some(payload) = line.get("payload") else {
        return;
    };
    match (str_field(line, "type"), str_field(payload, "type")) {
        (Some("response_item"), Some("function_call" | "custom_tool_call")) => walker.push(
            str_field(payload, "call_id"),
            str_field(payload, "name").unwrap_or_default(),
            codex_call_args(payload),
        ),
        (Some("response_item"), Some("function_call_output" | "custom_tool_call_output")) => {
            if let (Some(id), Some(output)) = (str_field(payload, "call_id"), payload.get("output"))
                && codex_output_failed(output)
            {
                walker.mark_error(id);
            }
        }
        (Some("response_item"), Some("message"))
            if str_field(payload, "role") == Some("assistant") =>
        {
            let content = payload.get("content").unwrap_or(&Value::Null);
            let text = joined_text(content, "output_text");
            if !text.is_empty() {
                walker.final_text = text;
            }
        }
        (Some("event_msg"), Some("task_complete")) if walker.final_text.is_empty() => {
            walker.final_text = str_field(payload, "last_agent_message")
                .unwrap_or_default()
                .to_string();
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const CLAUDE_REAL: &str = include_str!("../../../../tests/fixtures/claude-real-session.jsonl");
    const CODEX_TOOLS: &str =
        include_str!("../../../../tests/fixtures/eval/codex-rollout-tools.jsonl");

    fn names(trace: &Trace) -> Vec<&str> {
        trace.calls.iter().map(|c| c.name.as_str()).collect()
    }

    #[test]
    fn claude_real_session_golden_names_and_args() {
        let t = trace("claude", CLAUDE_REAL).unwrap();
        assert_eq!(t.calls.len(), 18);
        assert_eq!(
            names(&t),
            [
                "Edit", "shell", "Edit", "shell", "Read", "shell", "shell", "shell", "Skill",
                "Read", "Read", "Read", "shell", "Write", "Write", "Write", "shell", "shell"
            ]
        );
        assert_eq!(t.calls[8].args["skill"], json!("superpowers:brainstorming"));
        assert!(
            t.calls[1].args["command"]
                .as_str()
                .unwrap()
                .starts_with("./zb test -only-testing")
        );
        assert_eq!(t.calls.iter().filter(|c| c.is_error).count(), 1);
        assert!(!t.final_text.is_empty());
    }

    #[test]
    fn codex_rollout_golden_names_args_and_errors() {
        let t = trace("codex", CODEX_TOOLS).unwrap();
        assert_eq!(
            names(&t),
            ["shell", "shell", "zirv.skill_load", "apply_patch"]
        );
        assert_eq!(
            t.calls[0].args,
            json!({"command": "zirv skill load implement", "workdir": "/repo"})
        );
        assert_eq!(t.calls[1].args, json!({"command": "cargo clippy"}));
        assert_eq!(t.calls[2].args, json!({"id": "implement"}));
        assert!(t.calls[2].is_error);
        assert!(!t.calls[0].is_error);
        assert_eq!(
            t.calls[3].args,
            json!({"input": "*** Begin Patch\n*** End Patch"})
        );
        assert_eq!(t.final_text, "All checks pass.");
    }

    #[test]
    fn claude_names_are_normalised() {
        let line = r#"{"type":"assistant","message":{"content":[{"type":"tool_use","id":"a","name":"mcp__zirv__skill_load","input":{"id":"x"}},{"type":"tool_use","id":"b","name":"Bash","input":{"command":"ls"}}]}}"#;
        let t = trace("claude", line).unwrap();
        assert_eq!(names(&t), ["zirv.skill_load", "shell"]);
    }

    #[test]
    fn a_repeated_tool_use_id_counts_once_and_bad_lines_are_skipped() {
        let line = r#"{"type":"assistant","message":{"content":[{"type":"tool_use","id":"a","name":"Read","input":{}}]}}"#;
        let t = trace("claude", &format!("{line}\nnot json\n{line}\n")).unwrap();
        assert_eq!(t.calls.len(), 1);
    }

    #[test]
    fn an_unknown_harness_is_an_error() {
        assert!(trace("goose", "").is_err());
    }
}
