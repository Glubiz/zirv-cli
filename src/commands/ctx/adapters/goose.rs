//! Issue #393 (wave 3): the Block Goose CLI adapter (`goose`).
//!
//! Not installed on this machine; every fact below is exactly what issue
//! #393's own "Verified facts" section states, sourced from
//! `goose-docs.ai/docs/guides/{goose-cli-commands,context-engineering/
//! hooks}` and the `goose-run(1)` manpage (2026-09-07). The interactive
//! `goose session --system` flag and the exact hook schema are explicitly
//! called out in the issue as unverified and are left on the trait's own "no
//! verified mechanism" default rather than guessed.
//!
//! Goose's transcript is a single shared SQLite database
//! (`~/.local/share/goose/sessions/sessions.db`, or
//! `%APPDATA%\Block\goose\data\sessions\sessions.db` on Windows) covering
//! EVERY session, not one file per session -- unlike every JSONL-per-session
//! adapter elsewhere in this crate. Its internal table/column names are not
//! documented anywhere in issue #393 (only the file's existence and that it
//! stores per-session token usage), so [`ShadowTranscript::sync_sqlite`]
//! cannot be called with a real, non-guessed query.
//! [`transcript_path`](GooseAdapter::transcript_path) returns this real,
//! verified database path directly (informational, e.g. for `zirv ctx
//! status`) rather than guessing a schema; [`capabilities`](GooseAdapter::
//! capabilities) reports `events: false`, which is what keeps this path from
//! ever being opened and parsed as JSONL (`score::full_score`/
//! `IncrementalScorer::poll` both gate on `capabilities().events` before any
//! read).
//!
//! Goose is genuinely multi-provider (`--provider`/`--model`, issue #393's
//! own "per-launch provider resolution" note), so [`provider_for_model`]
//! resolves the billed vendor via `catalogue::vendor_of`, mirroring
//! `pi::PiAdapter`/`opencode::OpenCodeAdapter`.

use std::path::{Path, PathBuf};
use std::process::Command;

use super::super::CtxResult;
use super::super::catalogue;
use super::super::event::{
    Capabilities, NormalizedEvent, SessionId, SessionRef, StructuralContext,
};
use super::{AgentAdapter, ResolvedProgram, TurnSignalSetup};

#[derive(Debug, Clone)]
pub struct GooseAdapter {
    program: String,
    bin_args: Vec<String>,
    home: Option<PathBuf>,
}

impl GooseAdapter {
    /// `bin` may carry arguments, mirroring `PiAdapter::new`/`CodexAdapter::new`.
    pub fn new(bin: Option<&str>) -> Self {
        let raw = bin.unwrap_or("goose").trim();
        let mut parts = raw.split_whitespace().map(str::to_string);
        let program = parts.next().unwrap_or_else(|| "goose".to_string());
        Self {
            program,
            bin_args: parts.collect(),
            home: None,
        }
    }

    #[cfg(test)]
    pub fn with_home(mut self, home: PathBuf) -> Self {
        self.home = Some(home);
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

    /// Issue #393's own verified path pair: `~/.local/share/goose/sessions/
    /// sessions.db`, or `%APPDATA%\Block\goose\data\sessions\sessions.db` on
    /// Windows.
    fn db_path(&self) -> PathBuf {
        if cfg!(windows)
            && let Ok(appdata) = std::env::var("APPDATA")
        {
            return PathBuf::from(appdata)
                .join("Block")
                .join("goose")
                .join("data")
                .join("sessions")
                .join("sessions.db");
        }
        self.home_dir()
            .join(".local")
            .join("share")
            .join("goose")
            .join("sessions")
            .join("sessions.db")
    }
}

impl AgentAdapter for GooseAdapter {
    fn name(&self) -> &'static str {
        "goose"
    }

    fn program(&self) -> &str {
        &self.program
    }

    /// Goose itself spends no single account -- it is a front end onto
    /// whichever provider `--provider`/`--model` names, mirroring
    /// `pi::PiAdapter::provider`'s own reasoning.
    fn provider(&self) -> &'static str {
        "goose"
    }

    fn provider_for_model(&self, model: Option<&str>) -> &'static str {
        model
            .and_then(catalogue::vendor_of)
            .unwrap_or(self.provider())
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
                f == "goose" || f == "goose.cmd" || f == "goose.ps1"
            })
            .unwrap_or(false)
    }

    /// `goose run -t <prompt>` -- verified (issue #393: "Headless `goose run
    /// -t <text>|<file>|stdin`").
    fn headless_cmd(&self, prompt: &str, _session: &SessionId, extra: &[String]) -> Command {
        let mut cmd = self.base();
        cmd.arg("run").arg("-t").arg(prompt).args(extra);
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

    /// `goose run --no-session` with no `-t` -- verified (issue #393:
    /// "`--no-session` ... Don't save session (ephemeral)"). The prompt is
    /// delivered on stdin, mirroring every other adapter's own
    /// `distiller_cmd` shape: the caller (`handoff::run_model`) pipes it in.
    fn distiller_cmd(&self, model: &str) -> Command {
        let mut cmd = self.base();
        cmd.arg("run").arg("--no-session");
        cmd.args(self.model_args(model));
        cmd
    }

    /// No verified headless argv for goose's own `/mode` read-only posture
    /// (issue #393: "Read-only `/mode auto|approve|chat|smart_approve`" is a
    /// runtime slash command typed inside an interactive session, not a CLI
    /// flag this method can return). Disclosed via
    /// [`sandbox_residual_note`](Self::sandbox_residual_note).
    fn read_only_args(&self) -> Vec<String> {
        Vec::new()
    }

    fn sandbox_residual_note(&self) -> Option<String> {
        Some(
            "goose's read-only posture (/mode chat) is a runtime slash command with no verified \
             headless argv equivalent (issue #393), so no read-only pin is applied to this \
             launch; goose can write files and run commands."
                .to_string(),
        )
    }

    /// `goose run --system <text>` -- verified (issue #393). The interactive
    /// `goose session --system` form is explicitly unverified, so
    /// [`system_prompt_supported`](Self::system_prompt_supported) only
    /// claims the headless launch surface.
    fn system_prompt_args(&self, prompt: &str) -> Vec<String> {
        vec!["--system".to_string(), prompt.to_string()]
    }

    fn user_system_prompt_flag(&self) -> Option<&'static str> {
        Some("--system")
    }

    /// Issue #393: "Model `--model`, `--provider` -> per-launch provider
    /// resolution". The provider name is derived from the model's own vendor
    /// prefix via the shared catalogue (see this module's own doc comment):
    /// the issue documents both flags but not a combined `provider/model`
    /// syntax the way pi's/opencode's own `--model` does.
    fn model_args(&self, model: &str) -> Vec<String> {
        if model.is_empty() {
            return Vec::new();
        }
        let mut args = Vec::new();
        if let Some(vendor) = catalogue::vendor_of(model) {
            args.push("--provider".to_string());
            args.push(vendor.to_string());
        }
        args.push("--model".to_string());
        args.push(model.to_string());
        args
    }

    fn review_model_below(&self, seat: Option<&str>) -> &'static str {
        seat.and_then(catalogue::vendor_of)
            .and_then(catalogue::vendor)
            .map(|v| catalogue::rung_below(v, seat))
            .unwrap_or("")
    }

    fn model_strength(&self, model: &str) -> Option<u8> {
        catalogue::vendor_of(model)
            .and_then(catalogue::vendor)
            .and_then(|v| catalogue::strength(v, model))
    }

    fn context_window_tokens(&self, model: Option<&str>) -> Option<u64> {
        let vendor = model
            .and_then(catalogue::vendor_of)
            .and_then(catalogue::vendor)?;
        catalogue::context_window(vendor, model)
    }

    /// The real, verified shared sessions database (see this module's own
    /// doc comment for why its internal schema is not read and why
    /// `events: false` keeps this path from ever being opened).
    fn transcript_path(&self, _session: &SessionRef) -> PathBuf {
        self.db_path()
    }

    /// No verified `sessions.db` schema (see this module's own doc comment)
    /// -- never a guess at table/column names.
    fn parse_events(&self, _jsonl: &str) -> Vec<NormalizedEvent> {
        Vec::new()
    }

    fn structural_context(&self, _jsonl: &str, _last_n: usize) -> StructuralContext {
        StructuralContext::default()
    }

    /// `/compact` -- verified (issue #393).
    fn compact_command(&self) -> Option<&'static str> {
        Some("/compact")
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

    fn built_args(adapter: &GooseAdapter, cmd: &Command) -> Vec<String> {
        super::super::built_args(adapter.program(), cmd)
    }

    #[test]
    fn headless_cmd_uses_the_verified_run_subcommand_and_text_flag() {
        let adapter = GooseAdapter::new(None);
        let session = SessionId::parse("11111111-2222-4333-8444-555555555555");
        let cmd = adapter.headless_cmd("do the thing", &session, &[]);
        assert_eq!(
            built_args(&adapter, &cmd),
            vec![
                "run".to_string(),
                "-t".to_string(),
                "do the thing".to_string(),
            ]
        );
    }

    #[test]
    fn distiller_cmd_is_ephemeral_and_carries_no_prompt() {
        let adapter = GooseAdapter::new(None);
        let args = built_args(&adapter, &adapter.distiller_cmd("claude-sonnet-5"));
        assert_eq!(args[0], "run");
        assert!(args.contains(&"--no-session".to_string()));
        assert!(args.contains(&"--provider".to_string()));
        assert!(args.contains(&"anthropic".to_string()));
        assert!(args.contains(&"--model".to_string()));
        assert!(!args.iter().any(|a| a == "-t"));
    }

    #[test]
    fn model_args_derive_the_provider_flag_from_the_catalogue() {
        let adapter = GooseAdapter::new(None);
        assert_eq!(
            adapter.model_args("gpt-5.6-terra"),
            vec![
                "--provider".to_string(),
                "openai".to_string(),
                "--model".to_string(),
                "gpt-5.6-terra".to_string(),
            ]
        );
        assert_eq!(
            adapter.model_args("some-unknown-model"),
            vec!["--model".to_string(), "some-unknown-model".to_string()]
        );
        assert!(adapter.model_args("").is_empty());
    }

    #[test]
    fn provider_for_model_resolves_the_billed_vendor() {
        let adapter = GooseAdapter::new(None);
        assert_eq!(adapter.provider(), "goose");
        assert_eq!(
            adapter.provider_for_model(Some("claude-sonnet-5")),
            "anthropic"
        );
        assert_eq!(adapter.provider_for_model(Some("unknown")), "goose");
        assert_eq!(adapter.provider_for_model(None), "goose");
    }

    #[test]
    fn system_prompt_args_use_the_verified_headless_flag() {
        let adapter = GooseAdapter::new(None);
        assert_eq!(
            adapter.system_prompt_args("be careful"),
            vec!["--system".to_string(), "be careful".to_string()]
        );
    }

    #[test]
    fn read_only_args_are_empty_with_a_disclosed_residual() {
        let adapter = GooseAdapter::new(None);
        assert!(adapter.read_only_args().is_empty());
        assert!(adapter.sandbox_residual_note().is_some());
    }

    #[test]
    fn compact_command_uses_the_verified_slash_command() {
        assert_eq!(GooseAdapter::new(None).compact_command(), Some("/compact"));
    }

    #[test]
    fn detect_matches_the_bare_binary_and_windows_shim_extensions() {
        let adapter = GooseAdapter::new(None);
        assert!(adapter.detect(&["goose".to_string()]));
        assert!(adapter.detect(&["goose.cmd".to_string()]));
        assert!(!adapter.detect(&["codex".to_string()]));
        assert!(!adapter.detect(&[]));
    }

    #[test]
    fn all_registers_goose() {
        let names: Vec<&str> = super::super::all(None).iter().map(|a| a.name()).collect();
        assert!(names.contains(&"goose"), "got {names:?}");
    }

    #[test]
    fn capabilities_report_no_events() {
        let caps = GooseAdapter::new(None).capabilities();
        assert!(!caps.events, "no verified sessions.db schema");
        assert!(caps.system_prompt);
    }

    #[test]
    fn transcript_path_resolves_the_real_shared_database_under_home() {
        let home = tempfile::tempdir().expect("tempdir");
        let adapter = GooseAdapter::new(None).with_home(home.path().to_path_buf());
        let session = SessionRef {
            id: SessionId::parse("11111111-2222-4333-8444-555555555555"),
            cwd: PathBuf::from("/work/repo"),
        };
        let path = adapter.transcript_path(&session);
        if cfg!(not(windows)) {
            assert!(path.ends_with(".local/share/goose/sessions/sessions.db"));
        }
    }

    #[test]
    fn parse_events_and_structural_context_stay_empty() {
        let adapter = GooseAdapter::new(None);
        assert!(adapter.parse_events("{\"role\":\"user\"}").is_empty());
        assert_eq!(
            adapter.structural_context("{\"role\":\"user\"}", 5),
            StructuralContext::default()
        );
    }
}
