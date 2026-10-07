mod admin;
mod agent;
mod checkpoints;
mod missing_tests_gate;
mod permission;
mod posttool;
mod pretool_guard;
mod pretool_run;
mod pretool_tier;
mod prompt;
mod scope_guard;
mod session_events;
mod stop;
mod stop_verify;
mod tool_failure;

use std::io::{Read, Write};
#[cfg(test)]
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::commands::ctx::CtxResult;
#[cfg(test)]
use crate::commands::ctx::adapters::{self, SESSION_ENV};
#[cfg(test)]
use crate::commands::ctx::config::CtxConfig;
use crate::commands::ctx::config::env_from_process;
#[cfg(test)]
use crate::commands::ctx::state::{StateDir, now_secs};

use self::admin::{run_audit, run_hook_install, run_hook_status};
use self::agent::run_posttool_for_agent;
use self::permission::run_permission;
use self::prompt::run_prompt;
use self::session_events::{run_notify, run_pre_compact, run_session_start, run_subagent_stop};
use self::stop::run_stop;

pub(crate) use self::agent::REHYDRATION_TOOLS;
pub use self::agent::run_pretool_for_agent;
#[cfg(test)]
pub(crate) use self::checkpoints::AdoptionRecord;
pub(crate) use self::checkpoints::adoption_record_path;
pub(crate) use self::checkpoints::load_adoption_record;
pub(crate) use self::checkpoints::record_shell_skill_load;
#[cfg(test)]
pub(crate) use self::checkpoints::save_adoption_record;
pub(crate) use self::checkpoints::session_has_modification;
pub(crate) use self::missing_tests_gate::MISSING_TESTS_DEFAULT_FLOOR;
pub(crate) use self::missing_tests_gate::missing_tests_action;
pub(crate) use self::missing_tests_gate::missing_tests_questions;
pub(crate) use self::permission::command_family;
pub(crate) use self::permission::transcript_tool_use_ids;
pub(crate) use self::pretool_guard::orchestrator_advisory_should_surface;
pub(crate) use self::pretool_tier::DISPATCH_TIER_FLOOR;
pub use self::pretool_tier::PreToolInput;
pub use self::pretool_tier::PreToolPayload;
pub(crate) use self::pretool_tier::dispatch_tier_action;
pub(crate) use self::pretool_tier::dispatch_tier_question;
pub(crate) use self::stop_verify::STOP_VERIFY_DEFAULT_FLOOR;
pub(crate) use self::stop_verify::stop_verify_action;
pub(crate) use self::stop_verify::stop_verify_questions;

#[derive(Debug, clap::Args)]
pub struct HookArgs {
    #[command(subcommand)]
    pub event: HookEvent,
}

#[derive(Debug, clap::Subcommand)]
pub enum HookEvent {
    /// Claude Stop hook: score the turn and forward or advise.
    Stop,
    /// Claude UserPromptSubmit hook: install the reply marker instruction.
    Prompt,
    /// Claude PreCompact hook: record that a compaction is starting.
    PreCompact,
    /// Claude PreToolUse hook: refuse a subagent dispatch that would inherit
    /// this seat's expensive model, and refuse an orchestrator seat's own
    /// direct edit of a repository file (issue #334).
    Pretool {
        /// Issue #418: project a non-claude agent's own native `PreToolUse`-
        /// equivalent payload onto this hook's claude shape before running
        /// the guard, then translate the verdict back into that agent's own
        /// response envelope. Omitted (or `claude`) leaves this byte-for-byte
        /// identical to the original claude-only hook.
        #[arg(long)]
        agent: Option<String>,
    },
    /// Claude PostToolUse hook: replace a large `Bash` tool result with a
    /// compact, reversible summary before the model ever sees it (issue
    /// #326). The original output is stored verbatim first.
    Posttool {
        /// Issue #418: same projection/translation as `pretool`'s own
        /// `--agent`, for copilot's `postToolUse` `modifiedResult` contract.
        /// Only `copilot` has a supported native compaction envelope; any
        /// other non-`claude` value exits 0 with nothing on stdout.
        #[arg(long)]
        agent: Option<String>,
    },
    /// Observe Claude permission requests and denials without changing their
    /// flow. Its `permission_prompt` `Notification` confirms a dialog is shown,
    /// which alone raises the approval latch (#864); sandboxed-command network
    /// prompts emit only that notification, never a `PermissionRequest`.
    Permission,
    /// Claude SessionStart hook: re-inject the latest handoff on resume/clear.
    SessionStart,
    /// Issue #774: claude's `SubagentStop` hook, fired once a native `Task`
    /// subagent's own turn ends -- gates a few cheap, deterministic result-
    /// contract checks against the SUBAGENT's own transcript before its
    /// report reaches the lead. See [`run_subagent_stop`]'s own doc comment.
    SubagentStop,
    /// Issue #832: claude's `SubagentStart` hook. A local append of one agent
    /// graph node; prints nothing and always exits 0.
    SubagentStart,
    /// Issue #836: claude's `PostToolUseFailure` hook. Off by default; with `[jev] retry` on, asks
    /// Jev once per failure streak and may add one advisory line.
    ToolFailure,
    /// Codex notify program: same role as Stop.
    Notify {
        /// Payload, when the agent passes it as an argument instead of stdin.
        payload: Option<String>,
    },
    /// Aggregate the main decision log, safety/orchestrator-write denials
    /// and the compaction ledger into one hook-health report (issue #424).
    Audit {
        /// Restrict to rows recorded within this window, e.g. `24h`, `7d`,
        /// `30d`, or a bare number of seconds.
        #[arg(long, default_value = "7d")]
        since: String,
    },
    /// Issue #420: report the hook-integrity baseline's verdict (Ok/
    /// Outdated/Missing/Modified/NoBaseline) for every hook slot the current
    /// binary would install, across both the claude and codex targets.
    Status {
        /// Replace any `Outdated` entry (byte-identical to a known previous
        /// zirv shape) with the current binary's own shape. Never touches an
        /// entry that differs by so much as a byte from every known
        /// zirv-authored shape.
        #[arg(long)]
        heal: bool,
    },
    /// Issue #418: install (or remove) zirv's own native hook entries into
    /// `<agent>`'s own user-level hooks configuration file -- copilot,
    /// droid or gemini; see `native_hooks::NativeHooks`/`AgentAdapter::
    /// native_hooks`. Idempotent: a second `install` with no flags reports
    /// what is already there and changes nothing.
    Install {
        /// A registered adapter name with a native hooks surface
        /// (`AgentAdapter::native_hooks` returning `Some`).
        agent: String,
        /// Print the target file and each entry's current state without
        /// writing anything.
        #[arg(long)]
        show: bool,
        /// Remove zirv's own entries instead of installing them.
        #[arg(long)]
        uninstall: bool,
        /// Print what would change without writing anything.
        #[arg(long = "dry-run")]
        dry_run: bool,
    },
}

/// Optional Stop fields, including the observed `stop_hook_active`;
/// serializable for Codex notify projection.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct HookPayload {
    pub session_id: String,
    pub transcript_path: String,
    pub cwd: String,
    pub stop_hook_active: bool,
    /// SessionStart only: `"startup" | "resume" | "clear" | "compact"`.
    pub source: String,
    /// Claude's unique SubagentStop dispatch ID; the lead session ID is
    /// shared by all its subagents and cannot identify this one (#774).
    #[serde(default)]
    pub agent_id: String,
    /// SubagentStop transcript path; the ordinary transcript path names the
    /// lead session, not the subagent (#774).
    #[serde(default)]
    pub agent_transcript_path: String,
}

impl HookPayload {
    pub fn parse(raw: &str) -> CtxResult<Self> {
        Ok(serde_json::from_str(raw)?)
    }

    fn repo(&self) -> std::path::PathBuf {
        if self.cwd.is_empty() {
            std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."))
        } else {
            std::path::PathBuf::from(&self.cwd)
        }
    }
}

fn read_stdin() -> String {
    let mut buffer = String::new();
    let _ = std::io::stdin().read_to_string(&mut buffer);
    buffer
}

pub fn run<W: Write>(args: &HookArgs, w: &mut W) -> CtxResult<i32> {
    let env = env_from_process();
    match &args.event {
        HookEvent::Stop => run_stop(w, &read_stdin(), &env),
        HookEvent::Prompt => run_prompt(w, &read_stdin(), &env),
        HookEvent::PreCompact => run_pre_compact(w, &read_stdin(), &env),
        HookEvent::Pretool { agent } => {
            run_pretool_for_agent(w, &read_stdin(), &env, agent.as_deref())
        }
        HookEvent::Posttool { agent } => {
            run_posttool_for_agent(w, &read_stdin(), &env, agent.as_deref())
        }
        HookEvent::Permission => run_permission(w, &read_stdin(), &env),
        HookEvent::SessionStart => run_session_start(w, &read_stdin(), &env),
        HookEvent::SubagentStop => run_subagent_stop(w, &read_stdin(), &env),
        HookEvent::SubagentStart => super::graph::run_subagent_start(&read_stdin(), &env),
        HookEvent::ToolFailure => tool_failure::run_tool_failure(w, &read_stdin(), &env),
        HookEvent::Notify { payload } => {
            let raw = match payload {
                Some(text) => text.clone(),
                None => read_stdin(),
            };
            run_notify(w, &raw, &env)
        }
        HookEvent::Audit { since } => run_audit(w, since, &env),
        HookEvent::Status { heal } => run_hook_status(w, *heal, &env),
        HookEvent::Install {
            agent,
            show,
            uninstall,
            dry_run,
        } => run_hook_install(w, agent, *show, *uninstall, *dry_run),
    }
}

#[cfg(test)]
pub(super) mod tests {

    use super::pretool_tier::{PreToolPayload, pretool_decision};
    use super::*;
    use crate::commands::ctx::rot::{Score, Signals, Verdict};

    pub(super) fn payload() -> HookPayload {
        HookPayload {
            session_id: "11111111-2222-4333-8444-555555555555".to_string(),
            transcript_path: "/tmp/t.jsonl".to_string(),
            cwd: "/work/repo".to_string(),
            stop_hook_active: false,
            source: String::new(),
            agent_id: String::new(),
            agent_transcript_path: String::new(),
        }
    }

    /// `turns` user/assistant pairs; the first `edit_calls.min(turns)` turns
    /// each carry one `Edit` tool call, so `adoption::signals` over the whole
    /// parse reports exactly `(edit_calls.min(turns), turns)`.
    pub(super) fn transcript_with_edits(
        dir: &std::path::Path,
        turns: usize,
        edit_calls: usize,
    ) -> std::path::PathBuf {
        let path = dir.join("adoption.jsonl");
        let mut text = String::new();
        let mut remaining = edit_calls;
        for _ in 0..turns {
            text.push_str("{\"type\":\"user\",\"message\":{\"content\":\"go\"}}\n");
            let mut content = "{\"type\":\"text\",\"text\":\"ok\"}".to_string();
            if remaining > 0 {
                content.push_str(
                    ",{\"type\":\"tool_use\",\"id\":\"t\",\"name\":\"Edit\",\"input\":{}}",
                );
                remaining -= 1;
            }
            text.push_str(&format!(
                "{{\"type\":\"assistant\",\"message\":{{\"content\":[{content}],\"usage\":{{\"input_tokens\":100}}}}}}\n"
            ));
        }
        std::fs::write(&path, text).expect("write");
        path
    }

    /// Minimal, valid `Score` for direct `adoption_stop_nudge` calls -- the
    /// function only reads `score.signals.turns`.
    pub(super) fn score_with_turns(turns: usize) -> Score {
        Score {
            score: 0,
            verdict: Verdict::Healthy,
            context_tokens: 0,
            signals: Signals {
                turns,
                tool_failure_rate: 0.0,
                repetition_hits: 0,
                max_repeat: 0,
                same_error_repeats: 0,
                provider_overflows: 0,
                marker_miss_rate: None,
            },
            model_change: None,
            window_breakdown: None,
        }
    }

    /// Five corrections, no tool failures, low context: enough to recommend
    /// via the corrections signal alone, and healthy enough that the verdict
    /// stays `Healthy`.
    pub(super) fn correction_heavy_transcript(dir: &std::path::Path) -> std::path::PathBuf {
        let path = dir.join("t.jsonl");
        let mut text = String::new();
        for i in 0..12 {
            // Tools never fail here; the user keeps correcting.
            let prompt = if i < 5 {
                "no, not like that"
            } else {
                "carry on"
            };
            text.push_str(&format!(
                "{{\"type\":\"user\",\"message\":{{\"content\":\"{prompt}\"}}}}\n"
            ));
            text.push_str("{\"type\":\"user\",\"message\":{\"content\":[{\"type\":\"tool_result\",\"content\":\"r\",\"is_error\":false}]}}\n");
            text.push_str("{\"type\":\"assistant\",\"message\":{\"content\":[{\"type\":\"text\",\"text\":\"[zirv] ok\"}],\"usage\":{\"input_tokens\":1000}}}\n");
        }
        std::fs::write(&path, text).expect("write");
        path
    }

    // -- PermissionRequest / PermissionDenied observation -----------------

    pub(super) fn permission_stdin(
        event: Option<&str>,
        tool_name: &str,
        tool_input: serde_json::Value,
    ) -> String {
        let mut payload = serde_json::json!({
            "session_id": "abc123",
            "transcript_path": "/tmp/t.jsonl",
            "cwd": "/work/repo",
            "permission_mode": "default",
            "tool_name": tool_name,
            "tool_input": tool_input,
        });
        if let Some(event) = event {
            payload["hook_event_name"] = serde_json::json!(event);
        }
        payload.to_string()
    }

    /// The `Notification` Claude sends once a permission dialog has waited about six seconds (#864).
    pub(super) fn permission_prompt_notification() -> String {
        serde_json::json!({
            "session_id": "abc123",
            "transcript_path": "/tmp/t.jsonl",
            "cwd": "/work/repo",
            "hook_event_name": "Notification",
            "notification_type": "permission_prompt",
            "message": "Claude needs your permission to use Bash",
        })
        .to_string()
    }

    // -- Issue #456: a resolved permission prompt must not stay `Approval` --

    pub(super) fn permission_env(state: &Path) -> std::collections::HashMap<String, String> {
        [(
            crate::commands::ctx::state::STATE_ENV.to_string(),
            state.display().to_string(),
        )]
        .into()
    }

    // -- PreToolUse: the expensive-seat inheritance guard ------------------

    /// Builds the PreToolUse stdin claude actually sends, so every rule below
    /// is exercised through the same parser the hook uses in production
    /// rather than through a hand-built struct.
    pub(super) fn pretool_stdin(tool_name: &str, tool_input: serde_json::Value) -> String {
        serde_json::json!({
            "session_id": "abc123",
            "transcript_path": "/tmp/t.jsonl",
            "cwd": "/work/repo",
            "permission_mode": "default",
            "hook_event_name": "PreToolUse",
            "tool_name": tool_name,
            "tool_input": tool_input,
            "tool_use_id": "toolu_01ABC123",
        })
        .to_string()
    }

    pub(super) fn decide(
        seat: Option<&str>,
        tool_name: &str,
        tool_input: serde_json::Value,
    ) -> Option<String> {
        let payload = PreToolPayload::parse(&pretool_stdin(tool_name, tool_input))
            .expect("the documented payload must parse");
        pretool_decision(seat, &payload)
    }

    pub(super) const SEAT: Option<&str> = Some("fable");

    // -- PreToolUse: the orchestrator-write guard (issues #328/#334) -------

    /// Builds the PreToolUse stdin claude sends for a file-modification
    /// tool, with a caller-chosen `cwd`/`session_id` -- `pretool_stdin`
    /// above hardcodes both, which this guard's own tests need to vary.
    pub(super) fn orchestrator_pretool_stdin(
        cwd: &str,
        session_id: &str,
        tool_name: &str,
        tool_input: serde_json::Value,
    ) -> String {
        serde_json::json!({
            "session_id": session_id,
            "transcript_path": "/tmp/t.jsonl",
            "cwd": cwd,
            "permission_mode": "default",
            "hook_event_name": "PreToolUse",
            "tool_name": tool_name,
            "tool_input": tool_input,
            "tool_use_id": "toolu_01ABC123",
        })
        .to_string()
    }

    /// A repo root with a `.git` directory -- the ordinary checkout shape
    /// `repo_root_for_target` and `orchestrator_write_decision` both need to
    /// be exercised against something real. Deliberately not canonicalized:
    /// macOS's `/var/folders` vs `/private/var` split means `cwd` and every
    /// `file_path` built from `repo.path()` must stay spelled the same way
    /// for `Path::starts_with` to see them as confined.
    pub(super) fn orchestrator_repo() -> tempfile::TempDir {
        let repo = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(repo.path().join(".git")).expect(".git dir");
        repo
    }

    // -- Issue #309: verify-on-stop -----------------------------------------

    /// A repository with one commit, mirroring `verification.rs`'s own
    /// `git_repo()` test helper -- `changed_paths`/`latest_is_fresh_and_
    /// passing` need something real to read.
    pub(super) fn git_repo() -> tempfile::TempDir {
        let repo = tempfile::tempdir().expect("tempdir");
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
                .current_dir(repo.path())
                .status()
                .expect("run git");
            assert!(status.success(), "git {args:?} failed");
        };
        git(&["init", "-q"]);
        std::fs::write(repo.path().join("tracked.txt"), "one\n").expect("write");
        git(&["add", "."]);
        git(&["commit", "-q", "-m", "base"]);
        repo
    }

    // -- issue #785: `[jev] inject` at the hook sites ---------------------

    pub(super) const INJECT_DEFER: &str = r#"{"model": "jev-latest", "answers": {
        "defer": {"type": "noul", "noul": 0.95}},
        "usage": {"input_tokens": 5, "output_tokens": 0}}"#;

    pub(super) fn inject_cfg(base_url: String, credential_env: &str) -> CtxConfig {
        let mut cfg = CtxConfig::default();
        cfg.jev.inject = true;
        cfg.jev.cache_ttl_secs = 0;
        cfg.proxy.typesafe.base_url = base_url;
        cfg.proxy.typesafe.credential_env = credential_env.to_string();
        cfg.proxy.typesafe.timeout_secs = 5;
        cfg
    }

    /// Stores one worker message for session `aaaa1111` and returns the
    /// env lookup a prompt hook in that session would see.
    pub(super) fn mail_waiting(tmp: &Path, state: &StateDir) -> impl Fn(&str) -> Option<String> {
        crate::commands::ctx::mail::store(
            state,
            &crate::commands::ctx::state::repo_slug(tmp),
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
        let root = state.root().display().to_string();
        move |key: &str| match key {
            crate::commands::ctx::state::STATE_ENV => Some(root.clone()),
            SESSION_ENV => Some("aaaa1111-2222-4333-8444-555555555555".to_string()),
            adapters::AGENT_ENV => Some("claude".to_string()),
            _ => None,
        }
    }

    // -- Scope-creep guard ---------------------------------------------------

    /// The real t24 step 4 benchmark prompt, verbatim: a benchmark agent
    /// treated its own pagination off-by-one as "an outright bug rather than
    /// something to preserve" and fixed it unasked, failing hidden tests that
    /// expected unchanged pagination.
    pub(super) const SCOPE_GUARD_T24_PROMPT: &str = r"I've got thousands of transactions in here now between imports and
recurring rules, and `list` showing the oldest ones first means I have to
page through everything to see what I did yesterday. Please make `list`
show the most recent transactions first by default (sort by date
descending; when two transactions share a date, keep the one with the
lower id first). Pagination (`--page`/`--page-size`) works the same as
always, just over this newly-ordered sequence.

I know some of my own scripts probably depend on the old oldest-first
order though, so keep it available: add a `--legacy-order` flag to `list`
that skips the new sorting and shows transactions in the old raw order
exactly as before.";

    pub(super) fn scope_guard_prompt_stdin(session: &str, cwd: &Path, prompt: &str) -> String {
        serde_json::json!({
            "session_id": session,
            "cwd": cwd.display().to_string(),
            "prompt": prompt,
        })
        .to_string()
    }

    // -- Scope guard: the shell-edit checkpoint (item 1) --------------------

    /// A temp home + a real git repo (one committed, tracked `tracked.txt` --
    /// `git_repo()`'s own layout) + a state dir, so `record_scope_guard_
    /// request`'s own baseline git query and `run_posttool`'s cfg load never
    /// touch the developer's own machine.
    pub(super) struct ScopeGuardShellRig {
        _home_dir: tempfile::TempDir,
        _home: crate::commands::ctx::testenv::HomeGuard,
        pub(super) repo: tempfile::TempDir,
        _state: tempfile::TempDir,
        pub(super) env: std::collections::HashMap<String, String>,
    }

    pub(super) fn scope_guard_shell_rig() -> ScopeGuardShellRig {
        let home_dir = tempfile::tempdir().expect("home");
        let home = crate::commands::ctx::testenv::HomeGuard::set(home_dir.path());
        let repo = git_repo();
        let state_dir = tempfile::tempdir().expect("state dir");
        let env: std::collections::HashMap<String, String> = [(
            crate::commands::ctx::state::STATE_ENV.to_string(),
            state_dir.path().display().to_string(),
        )]
        .into();
        ScopeGuardShellRig {
            _home_dir: home_dir,
            _home: home,
            repo,
            _state: state_dir,
            env,
        }
    }

    pub(super) fn scope_guard_bash_posttool_stdin(
        session: &str,
        cwd: &Path,
        command: &str,
    ) -> String {
        serde_json::json!({
            "session_id": session,
            "cwd": cwd.display().to_string(),
            "tool_name": "Bash",
            "tool_input": {"command": command},
            "tool_response": {
                "stdout": "",
                "stderr": "",
                "interrupted": false,
                "isImage": false,
            },
            "tool_use_id": "toolu_scope_guard_shell",
        })
        .to_string()
    }

    #[test]
    fn payload_parsing_tolerates_missing_fields() {
        let parsed = HookPayload::parse("{\"session_id\":\"s\"}").expect("parse");
        assert_eq!(parsed.session_id, "s");
        assert_eq!(parsed.transcript_path, "");
        assert!(!parsed.stop_hook_active);

        let full = HookPayload::parse(
            "{\"session_id\":\"s\",\"transcript_path\":\"/t.jsonl\",\"cwd\":\"/c\",\"stop_hook_active\":true}",
        )
        .expect("parse");
        assert!(full.stop_hook_active);
        assert_eq!(full.cwd, "/c");
    }
}
