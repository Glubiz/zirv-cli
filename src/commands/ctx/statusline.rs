//! `zirv ctx statusline`: Claude Code's `statusLine` command (issue #833).
//!
//! Reads the statusline JSON on stdin and prints one line: the operator's own
//! statusline output (the command after `--`, or the built-in fallback line
//! when none is given) with a short zirv segment appended -- agents in use,
//! supervisor state, Jev call count and session spend. It makes no network
//! call, and the segment has a hard time limit; on any error or timeout the
//! output is exactly what the chained command (or the fallback) printed.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::Duration;

use serde_json::Value;

use super::config::{CtxConfig, env_from_process};
use super::state::StateDir;
use super::{CtxResult, graph, jev, usage};

/// The segment is dropped, not waited for, past this.
const SEGMENT_BUDGET: Duration = Duration::from_millis(40);
/// A larger Jev decision log is not re-read on a statusline tick.
const JEV_LOG_MAX_BYTES: u64 = 2 * 1024 * 1024;
const JEV_WINDOW_SECS: u64 = 24 * 60 * 60;

#[derive(Debug, clap::Args)]
pub struct StatuslineArgs {
    /// The operator's own statusline command, after `--`. Its output is kept
    /// and zirv's segment appended to its last line.
    #[arg(allow_hyphen_values = true, last = true)]
    pub command: Vec<String>,
}

/// The working directory Claude Code reports, when it reports one.
fn cwd_of(value: &Value) -> Option<PathBuf> {
    let text = value
        .pointer("/workspace/current_dir")
        .or_else(|| value.get("cwd"))?
        .as_str()?;
    Some(PathBuf::from(text))
}

fn jev_calls(cfg: &CtxConfig, state: &StateDir, now: u64) -> Option<u64> {
    if !jev::any_gate_enabled(&cfg.jev) || !jev::credential_present(cfg) {
        return None;
    }
    let log = state.root().join(jev::JEV_DECISIONS_FILE);
    if std::fs::metadata(log).is_ok_and(|m| m.len() > JEV_LOG_MAX_BYTES) {
        return None;
    }
    let _ = now;
    let rollup = jev::usage_rollup(state, JEV_WINDOW_SECS, None);
    Some(rollup.sites.values().map(|u| u.calls).sum())
}

/// The zirv segment for one statusline payload. `None` when the payload is not
/// JSON, so a broken input never produces a half-built segment.
pub fn segment(stdin: &str, cfg: &CtxConfig, state: &StateDir, now: u64) -> Option<String> {
    let value: Value = serde_json::from_str(stdin).ok()?;
    let cwd = cwd_of(&value);
    let agents = graph::read_session_records(state)
        .iter()
        .filter(|(record, alive)| *alive && cwd.as_deref().is_none_or(|c| record.repo == c))
        .count();
    let jev = jev_calls(cfg, state, now).map_or("off".to_string(), |calls| calls.to_string());
    let mut parts = vec![
        format!("agents {agents}/{}", cfg.dash.max_panes),
        format!(
            "supervisor {}",
            if cfg.supervisor.enabled { "on" } else { "off" }
        ),
        format!("jev {jev}"),
    ];
    if let Some(cost) = value
        .pointer("/cost/total_cost_usd")
        .and_then(Value::as_f64)
    {
        parts.push(format!("${cost:.2}"));
    }
    Some(parts.join(" \u{b7} "))
}

/// Append `segment` to the last non-empty line of `base`.
pub fn compose(base: &str, segment: Option<&str>) -> String {
    let Some(segment) = segment else {
        return base.to_string();
    };
    let trimmed = base.trim_end_matches(['\n', '\r']);
    if trimmed.is_empty() {
        return format!("{segment}\n");
    }
    format!("{trimmed} | {segment}\n")
}

fn segment_in_budget(stdin: &str, cwd: &Path, now: u64) -> Option<String> {
    let (tx, rx) = mpsc::channel();
    let (stdin, cwd) = (stdin.to_string(), cwd.to_path_buf());
    std::thread::Builder::new()
        .name("zirv-statusline".into())
        .spawn(move || {
            let env = env_from_process();
            let cwd = serde_json::from_str::<Value>(&stdin)
                .ok()
                .and_then(|v| cwd_of(&v))
                .unwrap_or(cwd);
            let found = CtxConfig::load(&cwd, &env).ok().and_then(|cfg| {
                let state = StateDir::resolve(&env).ok()?;
                segment(&stdin, &cfg, &state, now)
            });
            let _ = tx.send(found);
        })
        .ok()?;
    rx.recv_timeout(SEGMENT_BUDGET).ok().flatten()
}

pub fn run<W: Write>(args: &StatuslineArgs, w: &mut W) -> CtxResult<i32> {
    let mut stdin = String::new();
    let _ = std::io::stdin().read_to_string(&mut stdin);
    let base = usage::run_chained(&stdin, &args.command)
        .filter(|out| !out.trim().is_empty())
        .unwrap_or_else(|| format!("{}\n", usage::fallback_line(&stdin)));
    let cwd = std::env::current_dir().unwrap_or_default();
    let found = segment_in_budget(&stdin, &cwd, super::state::now_secs());
    let _ = write!(w, "{}", compose(&base, found.as_deref()));
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state() -> (tempfile::TempDir, StateDir) {
        let dir = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(dir.path().join("state"));
        (dir, state)
    }

    #[test]
    fn statusline_json_in_one_line_out() {
        let (_dir, state) = state();
        let payload = r#"{"model":{"display_name":"Fable"},"workspace":{"current_dir":"/nowhere"},"cost":{"total_cost_usd":3.1}}"#;
        let seg = segment(payload, &CtxConfig::default(), &state, 0).expect("segment");
        assert_eq!(
            seg,
            "agents 0/9 \u{b7} supervisor off \u{b7} jev off \u{b7} $3.10"
        );
        let line = compose(&format!("{}\n", usage::fallback_line(payload)), Some(&seg));
        assert_eq!(
            line,
            "Fable | agents 0/9 \u{b7} supervisor off \u{b7} jev off \u{b7} $3.10\n"
        );
        assert_eq!(line.lines().count(), 1);
    }

    #[test]
    fn the_supervisor_state_follows_the_config() {
        let (_dir, state) = state();
        let mut cfg = CtxConfig::default();
        cfg.supervisor.enabled = true;
        let seg = segment("{}", &cfg, &state, 0).expect("segment");
        assert!(seg.contains("supervisor on"), "{seg}");
    }

    #[test]
    fn the_operators_own_line_is_kept_and_only_extended() {
        assert_eq!(compose("my line\n", Some("zirv")), "my line | zirv\n");
        assert_eq!(
            compose("a\nb\n", Some("zirv")),
            "a\nb | zirv\n",
            "the segment joins the last line only"
        );
    }

    #[test]
    fn bad_input_falls_back_to_the_unextended_output() {
        let (_dir, state) = state();
        assert_eq!(segment("not json", &CtxConfig::default(), &state, 0), None);
        assert_eq!(compose("base\n", None), "base\n");
    }

    #[test]
    fn agents_count_only_live_sessions_in_the_reported_directory() {
        let (_dir, state) = state();
        std::fs::create_dir_all(state.sessions()).expect("sessions dir");
        let record = |short: &str, repo: &str| {
            serde_json::json!({
                "session": short, "short": short, "agent": "claude", "repo": repo,
                "repo_slug": "r", "verb": "wrap", "pid": std::process::id(), "started_at": 1,
            })
        };
        for (short, repo) in [("s1", "/work/a"), ("s2", "/work/b")] {
            std::fs::write(
                state.sessions().join(format!("{short}.json")),
                record(short, repo).to_string(),
            )
            .expect("write record");
        }
        let seg = segment(
            r#"{"workspace":{"current_dir":"/work/a"}}"#,
            &CtxConfig::default(),
            &state,
            0,
        )
        .expect("segment");
        assert!(seg.starts_with("agents 1/9"), "{seg}");
    }
}
