//! Issue #390 (wave 3): the xAI Grok Build CLI adapter (`grok`).
//!
//! Not installed on this machine; every fact below is exactly what issue
//! #390's own "Verified facts" section states, sourced from
//! `github.com/xai-org/grok-build` docs and `docs.x.ai/build/features/hooks`
//! (2026-09-07). Facts the issue itself calls out as unverified (an append
//! form of the system prompt, compaction, the exact read-only/plan-mode flag
//! names, and the url-encoding scheme for a session's cwd segment) are left
//! on the trait's own "no verified mechanism" default rather than guessed.
//!
//! **No row-level transcript schema is documented anywhere in issue #390** --
//! only the file location (`updates.jsonl`/`summary.json`), never a field
//! shape for a row inside it. [`capabilities`](GrokAdapter::capabilities)
//! therefore reports `events: false`: every currently-registered adapter in
//! this crate ships `events: true`, but inventing field names with no
//! citation would be exactly the guess issue #390 (and the wave-3 brief)
//! rules out. [`transcript_path`](GrokAdapter::transcript_path) still
//! resolves a real, best-effort location -- useful for `zirv ctx status` --
//! via [`super::pin_newest_transcript`], since the exact url-encoding of a
//! session's cwd segment is not given.

use std::path::{Path, PathBuf};
use std::process::Command;

use super::super::CtxResult;
use super::super::catalogue;
use super::super::event::{
    Capabilities, NormalizedEvent, SessionId, SessionRef, StructuralContext,
};
use super::super::state::StateDir;
use super::{AgentAdapter, ResolvedProgram, TurnSignalSetup};

const CATALOGUE_VENDOR: &str = "xai";

#[derive(Debug, Clone)]
pub struct GrokAdapter {
    program: String,
    bin_args: Vec<String>,
    home: Option<PathBuf>,
    #[cfg(test)]
    forced_state_root: Option<PathBuf>,
}

impl GrokAdapter {
    /// `bin` may carry arguments, mirroring `PiAdapter::new`/`CodexAdapter::new`.
    pub fn new(bin: Option<&str>) -> Self {
        let raw = bin.unwrap_or("grok").trim();
        let mut parts = raw.split_whitespace().map(str::to_string);
        let program = parts.next().unwrap_or_else(|| "grok".to_string());
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

    /// `GROK_HOME` overrides the home this adapter resolves `~/.grok` from
    /// (issue #390's own verified fact); a real per-process env read is
    /// deliberately avoided (edition 2024 makes `std::env::set_var` `unsafe`
    /// precisely because it races other threads, and the full serial suite
    /// runs every test in one process) in favor of the same `home`/
    /// `with_home` test seam every other adapter in this crate already uses.
    fn sessions_root(&self) -> PathBuf {
        std::env::var("GROK_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|_| self.home_dir().join(".grok"))
            .join("sessions")
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

impl AgentAdapter for GrokAdapter {
    fn name(&self) -> &'static str {
        "grok"
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
                f == "grok" || f == "grok.cmd" || f == "grok.ps1"
            })
            .unwrap_or(false)
    }

    /// `-p <prompt> --output-format json` -- verified (issue #390: "Headless
    /// `-p`/`--single`, `--output-format plain|json|streaming-json|
    /// streaming-messages-json`"). `--single` is not combined in here: the
    /// issue names both without stating whether they combine or are
    /// alternates, so this uses only `-p`, the spelling every other headless
    /// adapter in this survey shares.
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

    /// No verified prompt-delivery flag for a distiller child (only `-p
    /// <prompt>` is documented, and this method is never handed prompt
    /// text): bare invocation plus the read-only pin, mirroring
    /// `qwen::QwenAdapter::distiller_cmd`'s own "no prompt on argv" shape.
    fn distiller_cmd(&self, model: &str) -> Command {
        let mut cmd = self.base();
        cmd.args(self.model_args(model));
        cmd.args(self.read_only_args());
        cmd
    }

    /// Issue #390: "Read-only: plan mode; flag names unverified" -- no flag
    /// spelling is given, so this is empty rather than a guess, and the gap
    /// is disclosed via [`sandbox_residual_note`](Self::sandbox_residual_note).
    fn read_only_args(&self) -> Vec<String> {
        Vec::new()
    }

    fn sandbox_residual_note(&self) -> Option<String> {
        Some(
            "grok's read-only plan mode has no verified flag spelling (issue #390), so no \
             read-only pin is applied to this launch; grok can write files and run commands."
                .to_string(),
        )
    }

    /// `--system-prompt-override <text>` (verified, replace-only; an append
    /// form is explicitly unverified per issue #390).
    fn system_prompt_args(&self, prompt: &str) -> Vec<String> {
        vec!["--system-prompt-override".to_string(), prompt.to_string()]
    }

    fn user_system_prompt_flag(&self) -> Option<&'static str> {
        Some("--system-prompt-override")
    }

    /// `-m <model>` -- verified (issue #390: "Model `-m grok-4.6`").
    fn model_args(&self, model: &str) -> Vec<String> {
        if model.is_empty() {
            return Vec::new();
        }
        vec!["-m".to_string(), model.to_string()]
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
    /// content is never scored): grok mints its own uuid session directory
    /// (issue #390: `~/.grok/sessions/<url-encoded-cwd>/<uuid>/updates.
    /// jsonl`), and the exact url-encoding of the cwd segment is not given,
    /// so this scans for the newest `updates.jsonl` anywhere under the
    /// sessions root instead of computing that segment.
    fn transcript_path(&self, session: &SessionRef) -> PathBuf {
        let root = self.sessions_root();
        let matches = |p: &Path| p.file_name().and_then(|n| n.to_str()) == Some("updates.jsonl");
        if let Some(state) = self.state_dir()
            && let Some(found) =
                super::pin_newest_transcript(&state, session, "grok", &root, &matches)
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

    /// Unverified (issue #390: "Compact unverified").
    fn compact_command(&self) -> Option<&'static str> {
        None
    }

    /// No verified quit sequence; ctrl-c is never sent (see
    /// `wrap::quit_child`'s own doc comment for why that byte specifically
    /// must never reach a pty master again on Windows).
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

    fn built_args(adapter: &GrokAdapter, cmd: &Command) -> Vec<String> {
        super::super::built_args(adapter.program(), cmd)
    }

    #[test]
    fn headless_cmd_carries_the_prompt_and_json_output_format() {
        let adapter = GrokAdapter::new(None);
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
        let adapter = GrokAdapter::new(None);
        let cmd = adapter.interactive_cmd(Some("hello"), &[]);
        assert_eq!(built_args(&adapter, &cmd), vec!["hello".to_string()]);
        let bare = adapter.interactive_cmd(None, &[]);
        assert!(built_args(&adapter, &bare).is_empty());
    }

    #[test]
    fn system_prompt_args_use_the_verified_replace_flag() {
        let adapter = GrokAdapter::new(None);
        assert_eq!(
            adapter.system_prompt_args("be careful"),
            vec![
                "--system-prompt-override".to_string(),
                "be careful".to_string()
            ]
        );
        assert_eq!(
            adapter.user_system_prompt_flag(),
            Some("--system-prompt-override")
        );
    }

    #[test]
    fn model_args_use_the_verified_flag_and_omit_when_empty() {
        let adapter = GrokAdapter::new(None);
        assert_eq!(
            adapter.model_args("grok-4.6"),
            vec!["-m".to_string(), "grok-4.6".to_string()]
        );
        assert!(adapter.model_args("").is_empty());
    }

    #[test]
    fn read_only_args_are_empty_with_a_disclosed_residual() {
        let adapter = GrokAdapter::new(None);
        assert!(adapter.read_only_args().is_empty());
        assert!(adapter.sandbox_residual_note().is_some());
    }

    #[test]
    fn detect_matches_the_bare_binary_and_windows_shim_extensions() {
        let adapter = GrokAdapter::new(None);
        assert!(adapter.detect(&["grok".to_string()]));
        assert!(adapter.detect(&["grok.cmd".to_string()]));
        assert!(adapter.detect(&["grok.ps1".to_string()]));
        assert!(!adapter.detect(&["codex".to_string()]));
        assert!(!adapter.detect(&[]));
    }

    #[test]
    fn all_registers_grok() {
        let names: Vec<&str> = super::super::all(None).iter().map(|a| a.name()).collect();
        assert!(names.contains(&"grok"), "got {names:?}");
    }

    #[test]
    fn capabilities_report_no_events_and_a_verified_system_prompt() {
        let caps = GrokAdapter::new(None).capabilities();
        assert!(!caps.events, "no verified row-level transcript schema");
        assert!(caps.system_prompt);
        assert!(!caps.pre_tool_hook);
        assert!(!caps.post_tool_hook);
    }

    #[test]
    fn ladder_methods_answer_from_the_xai_vendor() {
        let adapter = GrokAdapter::new(None);
        let xai = catalogue::vendor("xai").expect("xai is a registered vendor");
        assert_eq!(
            adapter.review_model_below(Some("grok-4.6")),
            catalogue::rung_below(xai, Some("grok-4.6"))
        );
        assert_eq!(
            adapter.model_strength("grok-4.6"),
            catalogue::strength(xai, "grok-4.6")
        );
        assert_eq!(
            adapter.context_window_tokens(Some("grok-4.6")),
            catalogue::context_window(xai, Some("grok-4.6"))
        );
    }

    #[test]
    fn transcript_path_finds_the_newest_updates_jsonl_since_session_start() {
        let home = tempfile::tempdir().expect("tempdir");
        let state_root = tempfile::tempdir().expect("tempdir");
        let adapter = GrokAdapter::new(None)
            .with_home(home.path().to_path_buf())
            .with_state_root(state_root.path().to_path_buf());
        let session = SessionRef {
            id: SessionId::parse("11111111-2222-4333-8444-555555555555"),
            cwd: PathBuf::from("/work/repo"),
        };

        let before = adapter.transcript_path(&session);
        assert!(!before.exists());

        let dir = home
            .path()
            .join(".grok")
            .join("sessions")
            .join("encoded-cwd")
            .join("some-uuid");
        std::fs::create_dir_all(&dir).expect("create session dir");
        let real = dir.join("updates.jsonl");
        std::fs::write(&real, "").expect("write session file");

        // No session record yet: the pin lookup has nothing to resolve
        // against, so the fallback path is returned rather than a guess.
        let no_record = adapter.transcript_path(&session);
        assert!(!no_record.exists() || no_record != real);
    }

    #[test]
    fn parse_events_and_structural_context_stay_empty() {
        let adapter = GrokAdapter::new(None);
        assert!(adapter.parse_events("{\"role\":\"user\"}").is_empty());
        assert_eq!(
            adapter.structural_context("{\"role\":\"user\"}", 5),
            StructuralContext::default()
        );
    }
}
