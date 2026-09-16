//! Issue #391 (wave 3): the Moonshot Kimi Code CLI adapter (`kimi`).
//!
//! Not installed on this machine; every fact below is exactly what issue
//! #391's own "Verified facts" section states, sourced from
//! `MoonshotAI/kimi-cli` docs (`docs/en/reference/kimi-command.md`,
//! `docs/en/configuration/data-locations.md`, 2026-09-07). The model flag,
//! compaction, and the exact hook schema are explicitly called out in the
//! issue as unverified and are left on the trait's own "no verified
//! mechanism" default rather than guessed.
//!
//! **No row-level transcript schema is documented anywhere in issue #391**
//! (only file names/locations: `context.jsonl`, `wire.jsonl`, `state.json`).
//! [`capabilities`](KimiAdapter::capabilities) therefore reports
//! `events: false` -- see `grok.rs`'s own doc comment for why that is the
//! honest answer rather than an invented schema.
//! [`transcript_path`](KimiAdapter::transcript_path) still resolves a real,
//! best-effort location via [`super::pin_newest_transcript`], since the
//! exact md5-of-cwd digest input (raw string? trailing slash? case folding?)
//! is not given, so this scans for the newest `context.jsonl` under the
//! sessions root rather than computing that digest.

use std::path::{Path, PathBuf};
use std::process::Command;

use super::super::CtxResult;
use super::super::catalogue;
use super::super::event::{
    Capabilities, NormalizedEvent, SessionId, SessionRef, StructuralContext,
};
use super::super::state::StateDir;
use super::{AgentAdapter, ResolvedProgram, TurnSignalSetup};

const CATALOGUE_VENDOR: &str = "moonshot";

#[derive(Debug, Clone)]
pub struct KimiAdapter {
    program: String,
    bin_args: Vec<String>,
    home: Option<PathBuf>,
    #[cfg(test)]
    forced_state_root: Option<PathBuf>,
}

impl KimiAdapter {
    /// `bin` may carry arguments, mirroring `PiAdapter::new`/`CodexAdapter::new`.
    pub fn new(bin: Option<&str>) -> Self {
        let raw = bin.unwrap_or("kimi").trim();
        let mut parts = raw.split_whitespace().map(str::to_string);
        let program = parts.next().unwrap_or_else(|| "kimi".to_string());
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

    fn sessions_root(&self) -> PathBuf {
        self.home_dir().join(".kimi").join("sessions")
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

impl AgentAdapter for KimiAdapter {
    fn name(&self) -> &'static str {
        "kimi"
    }

    fn program(&self) -> &str {
        &self.program
    }

    fn provider(&self) -> &'static str {
        CATALOGUE_VENDOR
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
                f == "kimi" || f == "kimi.cmd" || f == "kimi.ps1"
            })
            .unwrap_or(false)
    }

    /// `-p <prompt>` -- verified (issue #391: "Headless `-p` (stdout answer,
    /// stderr progress)").
    fn headless_cmd(&self, prompt: &str, _session: &SessionId, extra: &[String]) -> Command {
        let mut cmd = self.base();
        cmd.arg("-p").arg(prompt).args(extra);
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
    /// invocation plus the read-only pin, mirroring `grok::GrokAdapter::
    /// distiller_cmd`'s own "no prompt on argv" shape.
    fn distiller_cmd(&self, _model: &str) -> Command {
        let mut cmd = self.base();
        cmd.args(self.read_only_args());
        cmd
    }

    /// `--plan` -- verified (issue #391: "Read-only `--plan`"). The issue
    /// also warns that `--auto`/`--yolo` auto-approve tools in headless mode,
    /// which is exactly why neither is ever emitted here.
    fn read_only_args(&self) -> Vec<String> {
        vec!["--plan".to_string()]
    }

    /// `--system-prompt <text>` -- verified (issue #391: "`--system-prompt`
    /// (highest priority)").
    fn system_prompt_args(&self, prompt: &str) -> Vec<String> {
        vec!["--system-prompt".to_string(), prompt.to_string()]
    }

    fn user_system_prompt_flag(&self) -> Option<&'static str> {
        Some("--system-prompt")
    }

    fn review_model_below(&self, seat: Option<&str>) -> &'static str {
        catalogue::vendor(CATALOGUE_VENDOR)
            .map(|v| catalogue::rung_below(v, seat))
            .unwrap_or("")
    }

    fn model_strength(&self, model: &str) -> Option<u8> {
        catalogue::vendor(CATALOGUE_VENDOR).and_then(|v| catalogue::strength(v, model))
    }

    fn context_window_tokens(&self, model: Option<&str>) -> Option<u64> {
        catalogue::vendor(CATALOGUE_VENDOR).and_then(|v| catalogue::context_window(v, model))
    }

    /// Best-effort discovery only (see this module's own doc comment for why
    /// content is never scored): kimi mints its own session id (issue #391:
    /// `~/.kimi/sessions/<md5-of-cwd>/<session-id>/context.jsonl`), and the
    /// exact md5 input is not given, so this scans for the newest
    /// `context.jsonl` anywhere under the sessions root instead of computing
    /// that digest.
    fn transcript_path(&self, session: &SessionRef) -> PathBuf {
        let root = self.sessions_root();
        let matches = |p: &Path| p.file_name().and_then(|n| n.to_str()) == Some("context.jsonl");
        if let Some(state) = self.state_dir()
            && let Some(found) =
                super::pin_newest_transcript(&state, session, "kimi", &root, &matches)
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

    /// Unverified (issue #391: "Compact: automatic only" -- no injectable
    /// command).
    fn compact_command(&self) -> Option<&'static str> {
        None
    }

    fn quit_sequence(&self) -> &'static str {
        ""
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            system_prompt: true,
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

    fn built_args(adapter: &KimiAdapter, cmd: &Command) -> Vec<String> {
        super::super::built_args(adapter.program(), cmd)
    }

    #[test]
    fn headless_cmd_carries_the_prompt() {
        let adapter = KimiAdapter::new(None);
        let session = SessionId::parse("11111111-2222-4333-8444-555555555555");
        let cmd = adapter.headless_cmd("do the thing", &session, &[]);
        assert_eq!(
            built_args(&adapter, &cmd),
            vec!["-p".to_string(), "do the thing".to_string()]
        );
    }

    #[test]
    fn interactive_cmd_carries_an_optional_positional_prompt() {
        let adapter = KimiAdapter::new(None);
        let cmd = adapter.interactive_cmd(Some("hello"), &[]);
        assert_eq!(built_args(&adapter, &cmd), vec!["hello".to_string()]);
        let bare = adapter.interactive_cmd(None, &[]);
        assert!(built_args(&adapter, &bare).is_empty());
    }

    #[test]
    fn read_only_args_use_the_verified_plan_flag_only() {
        let adapter = KimiAdapter::new(None);
        assert_eq!(adapter.read_only_args(), vec!["--plan".to_string()]);
        for dangerous in ["--auto", "--yolo"] {
            assert!(!adapter.read_only_args().iter().any(|a| a == dangerous));
        }
    }

    #[test]
    fn system_prompt_args_use_the_verified_flag() {
        let adapter = KimiAdapter::new(None);
        assert_eq!(
            adapter.system_prompt_args("be careful"),
            vec!["--system-prompt".to_string(), "be careful".to_string()]
        );
        assert_eq!(adapter.user_system_prompt_flag(), Some("--system-prompt"));
    }

    #[test]
    fn detect_matches_the_bare_binary_and_windows_shim_extensions() {
        let adapter = KimiAdapter::new(None);
        assert!(adapter.detect(&["kimi".to_string()]));
        assert!(adapter.detect(&["kimi.cmd".to_string()]));
        assert!(!adapter.detect(&["codex".to_string()]));
        assert!(!adapter.detect(&[]));
    }

    #[test]
    fn all_registers_kimi() {
        let names: Vec<&str> = super::super::all(None).iter().map(|a| a.name()).collect();
        assert!(names.contains(&"kimi"), "got {names:?}");
    }

    #[test]
    fn capabilities_report_no_events_and_a_verified_system_prompt() {
        let caps = KimiAdapter::new(None).capabilities();
        assert!(!caps.events, "no verified row-level transcript schema");
        assert!(caps.system_prompt);
    }

    #[test]
    fn ladder_methods_answer_from_the_moonshot_vendor() {
        let adapter = KimiAdapter::new(None);
        let moonshot = catalogue::vendor("moonshot").expect("moonshot is a registered vendor");
        assert_eq!(
            adapter.model_strength("kimi-k3"),
            catalogue::strength(moonshot, "kimi-k3")
        );
    }

    #[test]
    fn transcript_path_falls_back_to_a_pending_marker_with_no_session_record() {
        let home = tempfile::tempdir().expect("tempdir");
        let state_root = tempfile::tempdir().expect("tempdir");
        let adapter = KimiAdapter::new(None)
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
        let adapter = KimiAdapter::new(None);
        assert!(adapter.parse_events("{\"role\":\"user\"}").is_empty());
        assert_eq!(
            adapter.structural_context("{\"role\":\"user\"}", 5),
            StructuralContext::default()
        );
    }
}
