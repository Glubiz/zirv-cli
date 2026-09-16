//! Issue #392 (wave 3): the Cursor CLI adapter (`cursor-agent`).
//!
//! Not installed on this machine; every fact below is exactly what issue
//! #392's own "Verified facts" section states, sourced from
//! `cursor.com/docs/cli/{headless,reference/output-format,reference/
//! permissions}` and `cursor.com/docs/hooks` (2026-09-07). Headless
//! `--model`, compaction, and the exact hooks schema are explicitly called
//! out in the issue as unverified and are left on the trait's own "no
//! verified mechanism" default rather than guessed.
//!
//! Cursor's read-only posture is a CONFIG FILE (`.cursor/cli.json` /
//! `~/.cursor/cli-config.json`'s `permissions.allow`/`deny`), not a CLI flag
//! -- unlike every adapter this crate ships today, whose `read_only_args`
//! returns argv tokens. Materializing that file (the same "I/O inside a
//! method that looks pure" shape `opencode::OpenCodeAdapter::system_prompt_
//! args` already sets precedent for) is a real feature this pass does not
//! attempt, since the exact JSON schema for `Shell()`/`Read()`/`Write()`/
//! `WebFetch()`/`Mcp()` entries is not spelled out in the issue beyond their
//! bare names -- see [`read_only_args`](CursorAdapter::read_only_args)'s own
//! doc comment.
//!
//! **No row-level transcript schema is documented anywhere in issue #392**
//! (only the file location). [`capabilities`](CursorAdapter::capabilities)
//! therefore reports `events: false` -- see `grok.rs`'s own doc comment for
//! why that is the honest answer rather than an invented schema.
//! [`transcript_path`](CursorAdapter::transcript_path) still resolves a
//! real, best-effort location via [`super::pin_newest_transcript`], since
//! the exact per-project directory naming is not given.

use std::path::{Path, PathBuf};
use std::process::Command;

use super::super::CtxResult;
use super::super::event::{
    Capabilities, NormalizedEvent, SessionId, SessionRef, StructuralContext,
};
use super::super::state::StateDir;
use super::{AgentAdapter, ResolvedProgram, TurnSignalSetup};

#[derive(Debug, Clone)]
pub struct CursorAdapter {
    program: String,
    bin_args: Vec<String>,
    home: Option<PathBuf>,
    #[cfg(test)]
    forced_state_root: Option<PathBuf>,
}

impl CursorAdapter {
    /// `bin` may carry arguments, mirroring `PiAdapter::new`/`CodexAdapter::new`.
    /// Defaults to `cursor-agent`, the binary name issue #392 and its own
    /// acceptance criteria (`--agent cursor-agent`) both use; `agent` is
    /// documented as an alternate name but never assumed here.
    pub fn new(bin: Option<&str>) -> Self {
        let raw = bin.unwrap_or("cursor-agent").trim();
        let mut parts = raw.split_whitespace().map(str::to_string);
        let program = parts.next().unwrap_or_else(|| "cursor-agent".to_string());
        Self {
            program,
            bin_args: parts.collect(),
            home: None,
            #[cfg(test)]
            forced_state_root: None,
        }
    }

    #[cfg(test)]
    pub fn with_home(mut self, home: PathBuf) -> Self {
        self.home = Some(home);
        self
    }

    #[cfg(test)]
    pub fn with_state_root(mut self, root: PathBuf) -> Self {
        self.forced_state_root = Some(root);
        self
    }

    fn base(&self) -> Command {
        let resolved = super::resolve_program(&self.program)
            .unwrap_or_else(|_| ResolvedProgram::direct(&self.program));
        let mut cmd = Command::new(&resolved.program);
        cmd.args(&resolved.prefix);
        cmd.args(&self.bin_args);
        cmd
    }

    fn home_dir(&self) -> PathBuf {
        self.home
            .clone()
            .or_else(|| crate::utils::home_dir().ok())
            .unwrap_or_else(|| PathBuf::from("."))
    }

    fn projects_root(&self) -> PathBuf {
        self.home_dir().join(".cursor").join("projects")
    }

    #[cfg(test)]
    fn state_dir(&self) -> Option<StateDir> {
        self.forced_state_root.clone().map(StateDir::from_root)
    }

    #[cfg(not(test))]
    fn state_dir(&self) -> Option<StateDir> {
        StateDir::resolve(&super::super::config::env_from_process()).ok()
    }
}

/// Whether `path` sits inside some ancestor directory literally named
/// `agent-transcripts` (issue #392: `.../agent-transcripts/<session-id>/
/// <session-id>.jsonl`) and carries a `.jsonl` extension -- the predicate
/// [`CursorAdapter::transcript_path`] scans with, since the per-project
/// directory segment above `agent-transcripts` is not a computable fact.
fn is_agent_transcript(path: &Path) -> bool {
    if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
        return false;
    }
    path.ancestors()
        .any(|a| a.file_name().and_then(|n| n.to_str()) == Some("agent-transcripts"))
}

impl AgentAdapter for CursorAdapter {
    fn name(&self) -> &'static str {
        "cursor-agent"
    }

    fn program(&self) -> &str {
        &self.program
    }

    fn provider(&self) -> &'static str {
        "cursor"
    }

    fn ready(&self) -> CtxResult<()> {
        super::resolve_program(&self.program)?;
        Ok(())
    }

    fn detect(&self, command: &[String]) -> bool {
        command
            .first()
            .and_then(|p| Path::new(p).file_name())
            .map(|f| {
                let f = f.to_string_lossy();
                f == "cursor-agent" || f == "cursor-agent.cmd" || f == "cursor-agent.ps1"
            })
            .unwrap_or(false)
    }

    /// `-p <prompt> --output-format json` -- verified (issue #392: "Headless
    /// `-p`/`--print`, `--output-format json|stream-json|text`").
    fn headless_cmd(&self, prompt: &str, _session: &SessionId, extra: &[String]) -> Command {
        let mut cmd = self.base();
        cmd.arg("-p")
            .arg(prompt)
            .arg("--output-format")
            .arg("json")
            .args(extra);
        cmd
    }

    fn interactive_cmd(&self, initial_prompt: Option<&str>, extra: &[String]) -> Command {
        let mut cmd = self.base();
        if let Some(prompt) = initial_prompt {
            cmd.arg(prompt);
        }
        cmd.args(extra);
        cmd
    }

    /// No verified prompt-delivery flag for a distiller child: bare
    /// invocation, mirroring `grok::GrokAdapter::distiller_cmd`'s own "no
    /// prompt on argv" shape. `read_only_args` is empty (see its own doc
    /// comment), so there is no pin to append either.
    fn distiller_cmd(&self, _model: &str) -> Command {
        self.base()
    }

    /// Empty: Cursor's read-only posture is a config file
    /// (`permissions.allow`/`deny` in `.cursor/cli.json` /
    /// `~/.cursor/cli-config.json`), not a CLI flag this method can return --
    /// see this module's own doc comment. Disclosed via
    /// [`sandbox_residual_note`](Self::sandbox_residual_note).
    fn read_only_args(&self) -> Vec<String> {
        Vec::new()
    }

    fn sandbox_residual_note(&self) -> Option<String> {
        Some(
            "cursor-agent's read-only posture is a permissions.allow/deny config file (issue \
             #392), which this pass does not materialize, so no read-only pin is applied to \
             this launch; cursor-agent can write files and run commands."
                .to_string(),
        )
    }

    /// No verified flag (issue #392: "no flag (rules `.cursor/rules/*.mdc`,
    /// reads `AGENTS.md`/`CLAUDE.md`) -> `system_prompt_supported` false").
    fn system_prompt_args(&self, _prompt: &str) -> Vec<String> {
        Vec::new()
    }

    fn system_prompt_supported(&self, _launch: &[String]) -> bool {
        false
    }

    /// Best-effort discovery only (see this module's own doc comment for why
    /// content is never scored): cursor-agent mints its own session id
    /// (issue #392: `~/.cursor/projects/<project>/agent-transcripts/
    /// <session-id>/<session-id>.jsonl`), and the per-project directory
    /// segment is not a computable fact, so this scans for the newest
    /// `.jsonl` file under any `agent-transcripts` directory instead of
    /// computing that segment.
    fn transcript_path(&self, session: &SessionRef) -> PathBuf {
        let root = self.projects_root();
        if let Some(state) = self.state_dir()
            && let Some(found) = super::pin_newest_transcript(
                &state,
                session,
                "cursor-agent",
                &root,
                &is_agent_transcript,
            )
        {
            return found;
        }
        root.join(format!(
            "pending-{}.jsonl",
            super::super::sessions::short_id(session.id.as_str())
        ))
    }

    /// No verified row-level transcript schema (see this module's own doc
    /// comment) -- never a guess at field names.
    fn parse_events(&self, _jsonl: &str) -> Vec<NormalizedEvent> {
        Vec::new()
    }

    fn structural_context(&self, _jsonl: &str, _last_n: usize) -> StructuralContext {
        StructuralContext::default()
    }

    /// Unverified (issue #392: "Compact unverified").
    fn compact_command(&self) -> Option<&'static str> {
        None
    }

    fn quit_sequence(&self) -> &'static str {
        ""
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            system_prompt: false,
            events: false,
            ..Capabilities::default()
        }
    }

    fn register_turn_signal(&self, _session: &SessionRef, _socket: &Path) -> TurnSignalSetup {
        TurnSignalSetup {
            env: Vec::new(),
            instructions: String::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn built_args(adapter: &CursorAdapter, cmd: &Command) -> Vec<String> {
        super::super::built_args(adapter.program(), cmd)
    }

    #[test]
    fn headless_cmd_carries_the_prompt_and_json_output_format() {
        let adapter = CursorAdapter::new(None);
        let session = SessionId::parse("11111111-2222-4333-8444-555555555555");
        let cmd = adapter.headless_cmd("do the thing", &session, &[]);
        assert_eq!(
            built_args(&adapter, &cmd),
            vec![
                "-p".to_string(),
                "do the thing".to_string(),
                "--output-format".to_string(),
                "json".to_string(),
            ]
        );
    }

    #[test]
    fn interactive_cmd_carries_an_optional_positional_prompt() {
        let adapter = CursorAdapter::new(None);
        let cmd = adapter.interactive_cmd(Some("hello"), &[]);
        assert_eq!(built_args(&adapter, &cmd), vec!["hello".to_string()]);
        let bare = adapter.interactive_cmd(None, &[]);
        assert!(built_args(&adapter, &bare).is_empty());
    }

    #[test]
    fn read_only_args_are_empty_with_a_disclosed_residual() {
        let adapter = CursorAdapter::new(None);
        assert!(adapter.read_only_args().is_empty());
        assert!(adapter.sandbox_residual_note().is_some());
    }

    #[test]
    fn system_prompt_is_reported_unsupported() {
        let adapter = CursorAdapter::new(None);
        assert!(adapter.system_prompt_args("x").is_empty());
        assert!(!adapter.system_prompt_supported(&[]));
        assert!(!adapter.capabilities().system_prompt);
    }

    #[test]
    fn detect_matches_the_bare_binary_and_windows_shim_extensions() {
        let adapter = CursorAdapter::new(None);
        assert!(adapter.detect(&["cursor-agent".to_string()]));
        assert!(adapter.detect(&["cursor-agent.cmd".to_string()]));
        assert!(!adapter.detect(&["codex".to_string()]));
        assert!(!adapter.detect(&[]));
    }

    #[test]
    fn all_registers_cursor_agent() {
        let names: Vec<&str> = super::super::all(None).iter().map(|a| a.name()).collect();
        assert!(names.contains(&"cursor-agent"), "got {names:?}");
    }

    #[test]
    fn capabilities_report_no_events_and_no_system_prompt() {
        let caps = CursorAdapter::new(None).capabilities();
        assert!(!caps.events, "no verified row-level transcript schema");
        assert!(!caps.system_prompt);
    }

    #[test]
    fn is_agent_transcript_matches_only_jsonl_under_the_named_directory() {
        assert!(is_agent_transcript(Path::new(
            "/home/x/.cursor/projects/p/agent-transcripts/sess/sess.jsonl"
        )));
        assert!(!is_agent_transcript(Path::new(
            "/home/x/.cursor/chats/hash/id/store.db"
        )));
        assert!(!is_agent_transcript(Path::new(
            "/home/x/.cursor/projects/p/other/sess.jsonl"
        )));
    }

    #[test]
    fn transcript_path_falls_back_to_a_pending_marker_with_no_session_record() {
        let home = tempfile::tempdir().expect("tempdir");
        let state_root = tempfile::tempdir().expect("tempdir");
        let adapter = CursorAdapter::new(None)
            .with_home(home.path().to_path_buf())
            .with_state_root(state_root.path().to_path_buf());
        let session = SessionRef {
            id: SessionId::parse("11111111-2222-4333-8444-555555555555"),
            cwd: PathBuf::from("/work/repo"),
        };
        let path = adapter.transcript_path(&session);
        assert!(!path.exists());
        assert!(path.to_string_lossy().contains("pending"));
    }

    #[test]
    fn parse_events_and_structural_context_stay_empty() {
        let adapter = CursorAdapter::new(None);
        assert!(adapter.parse_events("{\"role\":\"user\"}").is_empty());
        assert_eq!(
            adapter.structural_context("{\"role\":\"user\"}", 5),
            StructuralContext::default()
        );
    }
}
