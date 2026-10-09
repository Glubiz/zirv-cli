//! Claude's `PostToolUseFailure` hook: once the same tool call shape has
//! failed [`FAILURE_STREAK_THRESHOLD`] times in a row, ask Jev once whether to
//! retry, stop and ask, or change approach (#836). Narrow-only: a decisive
//! stop or change adds one advisory line; retry, a split answer or any error
//! adds nothing.

use std::io::Write;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use super::checkpoints::cfg_or_operator_only_gate;
use super::pretool_tier::DispatchAdviseState;
use crate::commands::ctx::config::{CtxConfig, EnvLookup};
use crate::commands::ctx::state::{self, StateDir};
use crate::commands::ctx::{CtxResult, jev, supervisor};

/// Consecutive failures of one tool call shape before Jev is asked.
const FAILURE_STREAK_THRESHOLD: u32 = 3;

/// Minimum Choice confidence before a stop or change-approach answer is acted on.
///
/// The 2026-09-18 probe (memory `jev-probe-results-2026-09-18`) found a crash-reason Choice
/// right 10 of 12 times, with one wrong answer at 0.88, so 0.9 is the lowest floor that probe
/// supports. This exact question has NOT been probed yet: run a probe before enabling
/// `[jev] retry`.
const RETRY_MIN_CONFIDENCE: f32 = 0.9;

/// Longest this site may block a tool failure, however large `[proxy.typesafe] timeout_secs` is.
const RETRY_TIMEOUT_CAP_SECS: u64 = 2;

const STREAK_DIR: &str = "jev-retry";

const ADVISORY_STOP: &str = "Advisory from zirv's decision model: this tool call has failed \
repeatedly. Stop and ask the user before trying it again.";

const ADVISORY_CHANGE: &str = "Advisory from zirv's decision model: this tool call has failed \
repeatedly. Repeating it unchanged is unlikely to work; change approach.";

/// Claude's `PostToolUseFailure` stdin, narrowed to what this hook reads.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct ToolFailurePayload {
    tool_name: String,
    tool_input: serde_json::Value,
    error: String,
    is_interrupt: bool,
    session_id: String,
    cwd: String,
    /// Non-empty inside a native subagent.
    agent_id: String,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct Streak {
    shape: String,
    count: u32,
    asked: bool,
}

pub(crate) fn retry_questions() -> [jev::Question; 1] {
    [jev::Question::metadata_choice(
        "retry_or_stop",
        "Facts [consecutive failures of the same tool call, tool class (0 other, 1 shell, 2 read, \
3 edit or write, 4 search, 5 web, 6 mcp, 7 subagent), error class (0 other, 1 timeout or network, \
2 permission denied, 3 not found, 4 invalid input or syntax, 5 nonzero exit), harness (0 claude)] \
describe an agent repeating a failing tool call. What should it do?",
        &[
            (
                "retry",
                "the failure looks transient, trying again is reasonable",
            ),
            ("stop_and_ask", "it needs the user's permission or input"),
            ("change_approach", "repeating the same call cannot succeed"),
        ],
    )]
}

/// Only a decisive stop or change-approach answer yields an advisory.
pub(crate) fn retry_action(answer: Option<&jev::Answer>) -> &'static str {
    let Some(answer) = answer else {
        return "none";
    };
    if !answer.decisive(RETRY_MIN_CONFIDENCE, jev::DEFAULT_MIN_MARGIN) {
        return "none";
    }
    match answer.as_choice() {
        Some("stop_and_ask") => "stop_and_ask",
        Some("change_approach") => "change_approach",
        _ => "none",
    }
}

fn capped_timeout_secs(configured: u64) -> u64 {
    configured.min(RETRY_TIMEOUT_CAP_SECS)
}

/// `f`'s result, or `None` once `limit` passes. A live relay waits `timeout_secs + 3`, so the
/// timeout cap alone does not bound the hook; an abandoned call dies with the hook process.
fn within<T: Send + 'static>(
    limit: std::time::Duration,
    f: impl FnOnce() -> T + Send + 'static,
) -> Option<T> {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::Builder::new()
        .name("retry-advise".to_string())
        .spawn(move || {
            let _ = tx.send(f());
        })
        .ok()?;
    rx.recv_timeout(limit).ok()
}

fn tool_class(name: &str) -> u32 {
    match name {
        "Bash" | "PowerShell" => 1,
        "Read" => 2,
        "Edit" | "Write" | "MultiEdit" | "NotebookEdit" => 3,
        "Grep" | "Glob" => 4,
        "WebFetch" | "WebSearch" => 5,
        "Task" | "Agent" => 7,
        _ if name.starts_with("mcp__") => 6,
        _ => 0,
    }
}

/// Classify the error text locally; the text itself never leaves this process.
fn error_class(error: &str) -> u32 {
    let lower = error.to_lowercase();
    let has = |needles: &[&str]| needles.iter().any(|needle| lower.contains(needle));
    if has(&["timed out", "timeout", "connection", "network", "econn"]) {
        return 1;
    }
    if has(&[
        "permission",
        "denied",
        "not allowed",
        "forbidden",
        "unauthorized",
    ]) {
        return 2;
    }
    if has(&["not found", "no such file", "does not exist", "enoent"]) {
        return 3;
    }
    if has(&[
        "invalid",
        "syntax",
        "parse error",
        "unexpected",
        "malformed",
    ]) {
        return 4;
    }
    if has(&[
        "exit code",
        "exited with",
        "failed with",
        "nonzero",
        "non-zero",
    ]) {
        return 5;
    }
    0
}

/// Tool name plus the sorted argument key names; values are never read.
fn call_shape(payload: &ToolFailurePayload) -> String {
    let mut keys: Vec<&str> = payload
        .tool_input
        .as_object()
        .map(|object| object.keys().map(String::as_str).collect())
        .unwrap_or_default();
    keys.sort_unstable();
    format!("{}:{}", payload.tool_name, keys.join(","))
}

fn streak_path(state: &StateDir, session: &str) -> Option<PathBuf> {
    let name = supervisor::session_file_stem(session)?;
    Some(state.root().join(STREAK_DIR).join(format!("{name}.json")))
}

/// A successful tool call ends any failure streak; called from the PostToolUse hook.
pub(super) fn reset_streak(cfg: &CtxConfig, env: EnvLookup<'_>, session: &str) {
    if !(cfg.jev.retry || cfg.supervisor.enabled) {
        return;
    }
    let Ok(state) = StateDir::resolve(env) else {
        return;
    };
    if let Some(path) = streak_path(&state, session) {
        let _ = std::fs::remove_file(path);
    }
    supervisor::resolve_retry_stop(&state, cfg, &supervisor::hook_session_short(env, session));
}

/// Record one failure and, on the first time the streak reaches the threshold, fire the
/// supervisor (when on) and ask Jev once (when on and available).
#[cfg(test)]
fn tool_failure_advisory(
    state: &StateDir,
    cfg: &CtxConfig,
    payload: &ToolFailurePayload,
) -> Option<&'static str> {
    tool_failure_advisory_with(state, cfg, payload, &|_| false)
}

fn tool_failure_advisory_with(
    state: &StateDir,
    cfg: &CtxConfig,
    payload: &ToolFailurePayload,
    supervisor_fire: &dyn Fn(&ToolFailurePayload) -> bool,
) -> Option<&'static str> {
    let jev_on = jev::gate_open(cfg, state, "retry", cfg.jev.retry);
    if !(jev_on || cfg.supervisor.enabled) || payload.is_interrupt {
        jev::exit(state, "retry", jev_on, "interrupt");
        return None;
    }
    let path = streak_path(state, &payload.session_id)?;
    let lock = supervisor::lock_beside(&path);
    let shape = call_shape(payload);
    let mut streak = std::fs::read_to_string(&path)
        .ok()
        .and_then(|text| serde_json::from_str::<Streak>(&text).ok())
        .filter(|streak| streak.shape == shape)
        .unwrap_or_default();
    streak.shape = shape;
    streak.count = streak.count.saturating_add(1);
    let ask_now = streak.count >= FAILURE_STREAK_THRESHOLD && !streak.asked;
    streak.asked |= ask_now;
    if let (Some(dir), Ok(text)) = (path.parent(), serde_json::to_string(&streak)) {
        let _ = state::create_private_dir_all(dir);
        let _ = state::write_private(&path, &text);
    }
    drop(lock);
    if !ask_now {
        jev::exit(state, "retry", jev_on, "streak_not_at_threshold");
        return None;
    }
    if cfg.supervisor.enabled {
        supervisor_fire(payload);
    }
    if !jev_on {
        return None;
    }
    let mut capped = cfg.clone();
    capped.proxy.typesafe.timeout_secs = capped_timeout_secs(cfg.proxy.typesafe.timeout_secs);
    let advise_state = DispatchAdviseState {
        metadata_only: true,
        facts: vec![vec![
            streak.count.min(1_000),
            tool_class(&payload.tool_name),
            error_class(&payload.error),
            0,
        ]],
    };
    let state = state.clone();
    let answers = within(
        std::time::Duration::from_secs(RETRY_TIMEOUT_CAP_SECS),
        move || {
            jev::advise(
                &capped,
                &state,
                "retry",
                capped.jev.retry,
                &advise_state,
                &retry_questions(),
            )
        },
    )??;
    match retry_action(answers.get("retry_or_stop")) {
        "stop_and_ask" => Some(ADVISORY_STOP),
        "change_approach" => Some(ADVISORY_CHANGE),
        _ => None,
    }
}

pub(super) fn run_tool_failure<W: Write>(
    w: &mut W,
    stdin: &str,
    env: EnvLookup<'_>,
) -> CtxResult<i32> {
    let Ok(payload) = serde_json::from_str::<ToolFailurePayload>(stdin) else {
        return Ok(0);
    };
    let cwd = if payload.cwd.is_empty() {
        match std::env::current_dir() {
            Ok(cwd) => cwd,
            Err(_) => return Ok(0),
        }
    } else {
        PathBuf::from(&payload.cwd)
    };
    let cfg = cfg_or_operator_only_gate(&cwd, env);
    let Ok(state) = StateDir::resolve(env) else {
        return Ok(0);
    };
    let short = super::permission::attention_short(env, &payload.session_id);
    crate::commands::ctx::approvals::clear_released(&state, &short, env);
    // A dialog answered "No" in the pane fires no other hook, so the failure is what ends its prompt.
    if let Ok(tool_input) = serde_json::from_value(payload.tool_input.clone()) {
        super::permission::clear_finished_call(
            &state,
            &short,
            &payload.tool_name,
            &tool_input,
            &payload.agent_id,
            format!("permission ended by failure: {}", payload.tool_name),
        );
    }
    let supervisor_fire = |payload: &ToolFailurePayload| {
        let session = supervisor::hook_session_short(env, &payload.session_id);
        let request =
            supervisor::error_repeats_request(&cwd, &session, &payload.tool_name, &payload.error);
        supervisor::fire(&state, &cfg, env, request, None, &supervisor::real_spawn)
    };
    // A standing supervisor `stop` ruling outranks Jev's answer: it is binding, and stop is the
    // narrowest of the two (a `retry` ruling never lifts Jev's stop).
    let session = supervisor::hook_session_short(env, &payload.session_id);
    let advisory = tool_failure_advisory_with(&state, &cfg, &payload, &supervisor_fire);
    let stop_note = supervisor::retry_stop_note(&state, &cfg, &session);
    if let Some(note) = stop_note.as_deref().or(advisory) {
        let _ = writeln!(
            w,
            "{}",
            serde_json::json!({
                "hookSpecificOutput": {
                    "hookEventName": "PostToolUseFailure",
                    "additionalContext": note
                }
            })
        );
    }
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    const STOP: &str = r#"{"model":"jev-latest","answers":{"retry_or_stop":{"type":"choice","choice":"stop_and_ask","probabilities":{"stop_and_ask":0.95,"retry":0.03,"change_approach":0.02},"confidence":0.95}},"usage":{"input_tokens":5,"output_tokens":1}}"#;
    const RETRY: &str = r#"{"model":"jev-latest","answers":{"retry_or_stop":{"type":"choice","choice":"retry","probabilities":{"retry":0.95,"stop_and_ask":0.03,"change_approach":0.02},"confidence":0.95}},"usage":{"input_tokens":5,"output_tokens":1}}"#;

    fn retry_cfg(base_url: String, credential_env: &str) -> CtxConfig {
        let mut cfg = CtxConfig::default();
        cfg.jev.retry = true;
        cfg.jev.cache_ttl_secs = 0;
        cfg.proxy.typesafe.base_url = base_url;
        cfg.proxy.typesafe.credential_env = credential_env.to_string();
        cfg.proxy.typesafe.timeout_secs = 1;
        cfg
    }

    fn failure(error: &str) -> ToolFailurePayload {
        ToolFailurePayload {
            tool_name: "Bash".to_string(),
            tool_input: serde_json::json!({"command": "cargo test"}),
            error: error.to_string(),
            session_id: "sess-1".to_string(),
            ..ToolFailurePayload::default()
        }
    }

    /// Fail the same call `times` times and return the last advisory.
    fn fail_times(state: &StateDir, cfg: &CtxConfig, times: u32) -> Option<&'static str> {
        let payload = failure("Command failed with exit code 1");
        (0..times).fold(None, |_, _| tool_failure_advisory(state, cfg, &payload))
    }

    fn decision_rows(state: &StateDir) -> usize {
        std::fs::read_to_string(state.root().join("jev-decisions.jsonl"))
            .map(|text| text.lines().count())
            .unwrap_or(0)
    }

    fn with_credential<T>(name: &str, body: impl FnOnce() -> T) -> T {
        // SAFETY (test-only): a unique env var name each test owns.
        unsafe { std::env::set_var(name, "secret") };
        let result = body();
        unsafe { std::env::remove_var(name) };
        result
    }

    #[test]
    fn a_decisive_stop_answer_adds_an_advisory_on_the_third_failure() {
        let (url, handle) = jev::tests::one_shot_server(200, STOP);
        let dir = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(dir.path().join("state"));
        let env = "HOOK_TEST_RETRY_STOP";
        let cfg = retry_cfg(url, env);
        let (early, third) = with_credential(env, || {
            let early = fail_times(&state, &cfg, 2);
            (early, fail_times(&state, &cfg, 1))
        });
        handle.join().expect("server thread");
        assert_eq!(early, None, "two failures never ask");
        assert_eq!(third, Some(ADVISORY_STOP));
        assert_eq!(decision_rows(&state), 1);
    }

    /// Direction: a permissive retry answer never adds output.
    #[test]
    fn a_retry_answer_adds_nothing_through_the_hook_output() {
        let (url, handle) = jev::tests::one_shot_server(200, RETRY);
        let dir = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(dir.path().join("state"));
        let env = "HOOK_TEST_RETRY_RETRY";
        let cfg = retry_cfg(url, env);
        let advisory = with_credential(env, || fail_times(&state, &cfg, 3));
        handle.join().expect("server thread");
        assert_eq!(advisory, None);
        assert_eq!(decision_rows(&state), 1, "the call was made and logged");
    }

    #[test]
    fn a_server_error_adds_nothing() {
        let (url, handle) = jev::tests::one_shot_server(500, "{}");
        let dir = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(dir.path().join("state"));
        let env = "HOOK_TEST_RETRY_500";
        let cfg = retry_cfg(url, env);
        let advisory = with_credential(env, || fail_times(&state, &cfg, 3));
        handle.join().expect("server thread");
        assert_eq!(advisory, None);
    }

    #[test]
    fn a_timeout_adds_nothing() {
        // Accepts into the backlog but never answers, so the 1s read timeout fires.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let url = format!("http://{}", listener.local_addr().expect("addr"));
        let dir = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(dir.path().join("state"));
        let env = "HOOK_TEST_RETRY_TIMEOUT";
        let cfg = retry_cfg(url, env);
        let advisory = with_credential(env, || fail_times(&state, &cfg, 3));
        drop(listener);
        assert_eq!(advisory, None);
    }

    #[test]
    fn key_off_or_no_credential_does_no_work() {
        let dir = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(dir.path().join("state"));
        let env = "HOOK_TEST_RETRY_OFF";
        let mut cfg = retry_cfg("http://127.0.0.1:9".to_string(), env);
        cfg.jev.retry = false;
        let off = with_credential(env, || fail_times(&state, &cfg, 3));
        cfg.jev.retry = true;
        let no_credential = fail_times(&state, &cfg, 3);
        assert_eq!(off, None);
        assert_eq!(no_credential, None);
        assert!(!state.root().join(STREAK_DIR).exists(), "no streak file");
        assert!(
            !state.root().join(jev::JEV_DECISIONS_FILE).exists(),
            "no decision log"
        );
    }

    #[test]
    fn the_hook_prints_nothing_when_the_key_is_off() {
        let home = tempfile::tempdir().expect("tempdir");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());
        let state_root = tempfile::tempdir().expect("tempdir");
        let env = std::collections::HashMap::from([(
            "ZIRV_CTX_STATE_DIR".to_string(),
            state_root.path().display().to_string(),
        )]);
        let stdin = serde_json::json!({
            "tool_name": "Bash",
            "tool_input": {"command": "x"},
            "error": "boom",
            "session_id": "s",
            "cwd": home.path().display().to_string()
        })
        .to_string();
        let mut out = Vec::new();
        for _ in 0..4 {
            run_tool_failure(&mut out, &stdin, &|k| env.get(k).cloned()).expect("hook");
        }
        assert!(out.is_empty());
    }

    /// A pane dialog answered "No" ends the call with a tool failure and no other hook: the request
    /// record, the open prompt and the Approval latch for that exact call must go with it.
    #[test]
    fn a_failed_call_ends_its_open_permission_prompt_and_request_record() {
        use crate::commands::ctx::{approvals, attention};
        let home = tempfile::tempdir().expect("tempdir");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());
        let state_root = tempfile::tempdir().expect("tempdir");
        let env = std::collections::HashMap::from([(
            "ZIRV_CTX_STATE_DIR".to_string(),
            state_root.path().display().to_string(),
        )]);
        let state = StateDir::resolve(&|k| env.get(k).cloned()).expect("state");
        let short = crate::commands::ctx::sessions::short_id("sess-1");
        let mut request = approvals::Request::new(&short, "Bash", "cargo test", "cargo test", 1);
        request.released = true;
        let record = approvals::write_record(&state, &request).expect("record");
        attention::open_prompt(
            &state,
            &short,
            attention::OpenPrompt {
                id: request.id.clone(),
                ..attention::OpenPrompt::default()
            },
        );
        attention::confirm_prompts(&state, &short, "Bash: cargo test", 2);
        let stdin = serde_json::json!({
            "tool_name": "Bash",
            "tool_input": {"command": "cargo test"},
            "error": "The user doesn't want to proceed with this tool use.",
            "session_id": "sess-1",
            "cwd": home.path().display().to_string()
        })
        .to_string();
        run_tool_failure(&mut Vec::new(), &stdin, &|k| env.get(k).cloned()).expect("hook");
        assert!(!record.exists(), "the request record outlived the call");
        assert!(!attention::prompt_open(&state, &short));
        assert_eq!(
            attention::load(&state, &short).attention,
            attention::Attention::None
        );
    }

    #[test]
    fn one_call_per_streak_and_a_success_starts_a_new_streak() {
        let (url, handle) = jev::tests::one_shot_server(200, STOP);
        let dir = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(dir.path().join("state"));
        let env = "HOOK_TEST_RETRY_ONCE";
        let cfg = retry_cfg(url, env);
        let (fourth, after_success) = with_credential(env, || {
            fail_times(&state, &cfg, 3);
            let fourth = fail_times(&state, &cfg, 2);
            let env_map = std::collections::HashMap::from([(
                "ZIRV_CTX_STATE_DIR".to_string(),
                state.root().display().to_string(),
            )]);
            reset_streak(&cfg, &|k| env_map.get(k).cloned(), "sess-1");
            (fourth, fail_times(&state, &cfg, 2))
        });
        handle.join().expect("server thread");
        assert_eq!(fourth, None);
        assert_eq!(after_success, None, "a reset streak is below the threshold");
        assert_eq!(decision_rows(&state), 1, "exactly one call");
    }

    /// #835: with the supervisor on and Jev off, the third failure of a streak fires the supervisor
    /// exactly once, a success starts a new streak, and no Jev call or advisory happens.
    #[test]
    fn the_supervisor_fires_once_per_streak_without_jev() {
        let dir = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(dir.path().join("state"));
        let mut cfg = CtxConfig::default();
        cfg.supervisor.enabled = true;
        cfg.supervisor.model = "m".to_string();
        let fired = std::cell::Cell::new(0);
        let fire = |_: &ToolFailurePayload| {
            fired.set(fired.get() + 1);
            true
        };
        let payload = failure("Command failed with exit code 1");
        for _ in 0..2 {
            assert_eq!(
                tool_failure_advisory_with(&state, &cfg, &payload, &fire),
                None
            );
        }
        assert_eq!(fired.get(), 0, "two failures never fire");
        for _ in 0..3 {
            assert_eq!(
                tool_failure_advisory_with(&state, &cfg, &payload, &fire),
                None
            );
        }
        assert_eq!(fired.get(), 1, "once per streak");
        let env_map = std::collections::HashMap::from([(
            "ZIRV_CTX_STATE_DIR".to_string(),
            state.root().display().to_string(),
        )]);
        reset_streak(&cfg, &|k| env_map.get(k).cloned(), "sess-1");
        for _ in 0..3 {
            tool_failure_advisory_with(&state, &cfg, &payload, &fire);
        }
        assert_eq!(fired.get(), 2, "a success starts a new streak");
        assert_eq!(decision_rows(&state), 0, "no Jev call");
    }

    #[test]
    fn the_supervisor_off_keeps_the_streak_files_unwritten() {
        let dir = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(dir.path().join("state"));
        let cfg = CtxConfig::default();
        let payload = failure("boom");
        for _ in 0..4 {
            tool_failure_advisory_with(&state, &cfg, &payload, &|_| panic!("must not fire"));
        }
        assert!(!state.root().exists());
    }

    #[test]
    fn a_slow_advisory_is_abandoned_at_the_limit() {
        use std::time::Duration;
        let slow = within(Duration::from_millis(50), || {
            std::thread::sleep(Duration::from_millis(1_000));
            1
        });
        assert_eq!(slow, None);
        assert_eq!(within(Duration::from_secs(5), || 2), Some(2));
    }

    #[test]
    fn the_timeout_is_capped_at_two_seconds() {
        assert_eq!(capped_timeout_secs(10), 2);
        assert_eq!(capped_timeout_secs(1), 1);
    }

    #[test]
    fn an_interrupt_is_not_a_failure() {
        let dir = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(dir.path().join("state"));
        let env = "HOOK_TEST_RETRY_INTERRUPT";
        let cfg = retry_cfg("http://127.0.0.1:9".to_string(), env);
        let mut payload = failure("interrupted");
        payload.is_interrupt = true;
        with_credential(env, || {
            for _ in 0..3 {
                assert_eq!(tool_failure_advisory(&state, &cfg, &payload), None);
            }
        });
        assert!(!state.root().join(STREAK_DIR).exists());
        assert!(!state.root().join(jev::JEV_DECISIONS_FILE).exists());
    }

    #[test]
    fn the_request_passes_the_metadata_guard_and_carries_no_text() {
        let advise_state = DispatchAdviseState {
            metadata_only: true,
            facts: vec![vec![1_000, 7, 5, 0]],
        };
        let value = serde_json::to_value(&advise_state).expect("json");
        assert!(jev::safe_metadata_request(
            &value,
            &retry_questions(),
            "jev-latest"
        ));
        assert_eq!(error_class("Operation timed out"), 1);
        assert_eq!(error_class("EACCES: permission denied"), 2);
        assert_eq!(error_class("cat: x: No such file or directory"), 3);
    }
}
