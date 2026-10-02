//! The last few tool calls of one agent, tail-read from its own transcript or rollout.
//!
//! Read-only and bounded: at most `TAIL_BYTES` of the file, cached on (mtime, len), and called
//! only from the graph's background gather, never per frame. Claude transcripts (a session, a
//! pane or a native subagent) and Codex rollouts share one entry point, told apart by row shape.

use std::collections::BTreeMap;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::SystemTime;

use serde::Serialize;
use serde_json::Value;

use super::approvals::clean;
use super::snapshot::redact_text;

const TAIL_BYTES: u64 = 128 * 1024;
const KEEP_STEPS: usize = 5;
const ARG_COLS: usize = 120;
const CACHE_LIMIT: usize = 512;

/// One tool call: when, which tool, and a one-line redacted summary of its input.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Step {
    pub ts: u64,
    pub tool: String,
    pub arg: String,
}

type Cache = Mutex<BTreeMap<PathBuf, ((SystemTime, u64), Vec<Step>)>>;

fn cache() -> &'static Cache {
    static CACHE: OnceLock<Cache> = OnceLock::new();
    CACHE.get_or_init(Default::default)
}

/// The last `KEEP_STEPS` tool calls in `path`, newest last; empty when the file is missing or has none.
pub fn latest(path: &Path) -> Vec<Step> {
    let Some(key) = std::fs::metadata(path)
        .ok()
        .and_then(|meta| Some((meta.modified().ok()?, meta.len())))
    else {
        return Vec::new();
    };
    let mut cache = cache()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if let Some((cached, steps)) = cache.get(path)
        && *cached == key
    {
        return steps.clone();
    }
    let steps = read_tail(path, key.1);
    if cache.len() >= CACHE_LIMIT {
        cache.clear();
    }
    cache.insert(path.to_path_buf(), (key, steps.clone()));
    steps
}

fn read_tail(path: &Path, len: u64) -> Vec<Step> {
    let start = len.saturating_sub(TAIL_BYTES);
    let mut bytes = Vec::new();
    let read = std::fs::File::open(path).and_then(|mut file| {
        file.seek(SeekFrom::Start(start))?;
        file.take(TAIL_BYTES).read_to_end(&mut bytes)
    });
    if read.is_err() {
        return Vec::new();
    }
    let text = String::from_utf8_lossy(&bytes);
    let mut lines = text.lines();
    // A read that starts mid-file begins inside a line.
    if start > 0 {
        lines.next();
    }
    let mut steps: Vec<Step> = lines
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .flat_map(|row| steps_of(&row))
        .collect();
    steps.drain(..steps.len().saturating_sub(KEEP_STEPS));
    steps
}

fn row_ts(row: &Value) -> u64 {
    row.get("timestamp")
        .and_then(Value::as_str)
        .and_then(super::window::parse_iso8601_utc)
        .unwrap_or(0)
}

fn steps_of(row: &Value) -> Vec<Step> {
    let ts = row_ts(row);
    match row.get("type").and_then(Value::as_str) {
        Some("assistant") => row
            .pointer("/message/content")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter(|part| part.get("type").and_then(Value::as_str) == Some("tool_use"))
            .map(|part| claude_step(ts, part))
            .collect(),
        Some("response_item") => row
            .get("payload")
            .and_then(|payload| codex_step(ts, payload))
            .into_iter()
            .collect(),
        _ => Vec::new(),
    }
}

fn text_of<'a>(input: &'a Value, key: &str) -> &'a str {
    input.get(key).and_then(Value::as_str).unwrap_or_default()
}

fn basename(path: &str) -> &str {
    path.rsplit(['/', '\\']).next().unwrap_or(path)
}

/// First line of a shell command, without a leading `cd <dir> && `.
fn shell_summary(command: &str) -> &str {
    let first = command.lines().next().unwrap_or_default().trim();
    match first
        .strip_prefix("cd ")
        .and_then(|rest| rest.split_once(" && "))
    {
        Some((_, after)) => after.trim(),
        None => first,
    }
}

/// Redact first so a secret cut by the cap is never half-shown, then flatten and cap.
fn line_of(text: &str) -> String {
    clean(&redact_text(text), ARG_COLS)
}

fn claude_step(ts: u64, part: &Value) -> Step {
    let tool = text_of(part, "name");
    let input = part.get("input").unwrap_or(&Value::Null);
    let arg = match tool {
        "Bash" | "PowerShell" => line_of(shell_summary(text_of(input, "command"))),
        "Read" | "Edit" | "Write" | "MultiEdit" | "NotebookEdit" => {
            line_of(basename(text_of(input, "file_path")))
        }
        "Grep" => line_of(&format!("/{}/", text_of(input, "pattern"))),
        "Glob" => line_of(text_of(input, "pattern")),
        "SubagentHandback" => "reported back".to_string(),
        _ => line_of(tool),
    };
    Step {
        ts,
        tool: line_of(tool),
        arg,
    }
}

/// The shell command inside a Codex tool call's arguments (`cmd` or `command`, text or argv).
fn codex_command(arguments: &str) -> Option<String> {
    let value: Value = serde_json::from_str(arguments).ok()?;
    ["cmd", "command"]
        .iter()
        .find_map(|key| match value.get(key)? {
            Value::String(text) => Some(text.clone()),
            Value::Array(argv) => Some(
                argv.iter()
                    .filter_map(Value::as_str)
                    .collect::<Vec<_>>()
                    .join(" "),
            ),
            _ => None,
        })
}

/// The `cmd:"..."` literal of a code-mode `exec` call's JavaScript, when it holds a double-quoted one.
fn code_mode_command(source: &str) -> Option<String> {
    let after = &source[source.find("cmd:")? + 4..];
    let literal = after.trim_start();
    if !literal.starts_with('"') {
        return None;
    }
    let mut escaped = false;
    let end = literal.char_indices().skip(1).find_map(|(i, c)| {
        let hit = c == '"' && !escaped;
        escaped = c == '\\' && !escaped;
        hit.then_some(i)
    })?;
    serde_json::from_str(&literal[..=end]).ok()
}

fn codex_step(ts: u64, payload: &Value) -> Option<Step> {
    let name = text_of(payload, "name");
    let command = match payload.get("type").and_then(Value::as_str)? {
        "function_call" => codex_command(text_of(payload, "arguments")),
        "custom_tool_call" => {
            let input = text_of(payload, "input");
            if name == "apply_patch" {
                let file = input
                    .lines()
                    .find_map(|line| line.strip_prefix("*** ")?.split_once(" File: "))?
                    .1;
                return Some(Step {
                    ts,
                    tool: line_of(name),
                    arg: line_of(basename(file)),
                });
            }
            code_mode_command(input)
        }
        "local_shell_call" => {
            return Some(Step {
                ts,
                tool: "exec_command".to_string(),
                arg: line_of(shell_summary(
                    &payload
                        .pointer("/action/command")
                        .and_then(Value::as_array)
                        .map(|argv| {
                            argv.iter()
                                .filter_map(Value::as_str)
                                .collect::<Vec<_>>()
                                .join(" ")
                        })
                        .unwrap_or_default(),
                )),
            });
        }
        _ => return None,
    };
    Some(match command {
        Some(command) => Step {
            ts,
            tool: "exec_command".to_string(),
            arg: line_of(shell_summary(&command)),
        },
        None => Step {
            ts,
            tool: line_of(name),
            arg: line_of(name),
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(name: &str) -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/graph-steps")
            .join(name)
    }

    fn step(tool: &str, arg: &str) -> (String, String) {
        (tool.to_string(), arg.to_string())
    }

    fn pairs(steps: &[Step]) -> Vec<(String, String)> {
        steps
            .iter()
            .map(|s| (s.tool.clone(), s.arg.clone()))
            .collect()
    }

    #[test]
    fn a_native_subagent_transcript_gives_its_last_five_calls_newest_last() {
        let steps = latest(&fixture("subagent.jsonl"));
        assert_eq!(
            pairs(&steps),
            vec![
                step("Read", "lib.rs"),
                step("Edit", "lib.rs"),
                step("Grep", "/fn main/"),
                step("Glob", "src/**/*.rs"),
                step("SubagentHandback", "reported back"),
            ]
        );
        assert!(steps.windows(2).all(|pair| pair[0].ts <= pair[1].ts));
        assert!(steps[0].ts > 0);
    }

    #[test]
    fn a_claude_session_transcript_summarises_bash_without_its_cd_prefix() {
        let steps = latest(&fixture("claude-session.jsonl"));
        assert_eq!(
            pairs(&steps),
            vec![
                step("Bash", "cargo test --lib"),
                step("Write", "notes.md"),
                step("mcp__demo__lookup", "mcp__demo__lookup"),
            ]
        );
    }

    #[test]
    fn a_codex_rollout_gives_exec_calls_patches_and_other_tools() {
        let steps = latest(&fixture("codex-rollout.jsonl"));
        assert_eq!(
            pairs(&steps),
            vec![
                step("exec_command", "git status --short"),
                step("exec_command", "sed -n '1,40p' src/lib.rs"),
                step("apply_patch", "lib.rs"),
                step("wait_agent", "wait_agent"),
            ]
        );
    }

    #[test]
    fn args_are_redacted_and_capped_at_120_characters() {
        let steps = latest(&fixture("redaction.jsonl"));
        let secret = &steps[0].arg;
        assert!(
            !secret.contains("ghp_1234567890abcdefghijklmnopqrstuvwx"),
            "{secret}"
        );
        let long = &steps[1].arg;
        assert_eq!(long.chars().count(), ARG_COLS, "{long}");
    }

    #[test]
    fn a_missing_file_and_a_grown_file_are_handled_by_the_cache_key() {
        let dir = tempfile::tempdir().expect("tmp");
        let path = dir.path().join("t.jsonl");
        assert!(latest(&path).is_empty());
        let row = |name: &str| {
            format!(
                "{{\"type\":\"assistant\",\"timestamp\":\"2026-10-01T10:00:00Z\",\"message\":{{\"content\":[{{\"type\":\"tool_use\",\"name\":\"{name}\",\"input\":{{}}}}]}}}}\n"
            )
        };
        std::fs::write(&path, row("A")).expect("write");
        assert_eq!(latest(&path).len(), 1);
        std::fs::write(&path, format!("{}{}", row("A"), row("B"))).expect("write");
        assert_eq!(pairs(&latest(&path)).last(), Some(&step("B", "B")));
    }
}
