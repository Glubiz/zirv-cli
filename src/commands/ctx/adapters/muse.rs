//! Issue #394 (wave 3): the Meta Muse Code CLI adapter (`muse`),
//! macOS/Linux only.
//!
//! Not installed on this machine; every fact below is exactly what issue
//! #394's own "Verified facts" section states, sourced from
//! `dev.meta.ai/docs/muse-code/{configuration,permissions,extending,
//! rewind}.md` (2026-09-07, proprietary beta). The CLI form of `/resume`,
//! compaction, and the exact hook payload schema are explicitly called out
//! in the issue as unverified and are left on the trait's own "no verified
//! mechanism" default rather than guessed.
//!
//! Issue #394 itself flags muse's transcript LOCATION as unverified --
//! "path ... only from third parties, verify on a Linux box first" -- unlike
//! every other wave-3 issue, where at least the file location came from
//! official docs. [`transcript_path`](MuseAdapter::transcript_path)
//! therefore returns an empty path rather than hard-coding a third-party,
//! unconfirmed claim as if it were fact, and
//! [`capabilities`](MuseAdapter::capabilities) reports `events: false`.
//!
//! Muse is proprietary beta, macOS/Linux only (issue #394): [`ready`]
//! (MuseAdapter::ready) refuses outright on Windows, since no binary can
//! ever exist there for this harness. That refusal is a permanent platform
//! fact rather than a fixable "not installed yet" state, so
//! [`platform_unsupported`](MuseAdapter::platform_unsupported) overrides the
//! trait's default `false` to say so -- it keeps `readiness_note()` (`zirv
//! ctx --help`) from filing muse under its "Not ready yet" (go-install)
//! clause on Windows.

use std::path::{Path, PathBuf};
use std::process::Command;

use super::super::CtxResult;
use super::super::catalogue;
use super::super::event::{
    Capabilities, NormalizedEvent, SessionId, SessionRef, StructuralContext,
};
use super::{AgentAdapter, ResolvedProgram, TurnSignalSetup};

const CATALOGUE_VENDOR: &str = "meta";

#[derive(Debug, Clone)]
pub struct MuseAdapter {
    program: String,
    bin_args: Vec<String>,
}

impl MuseAdapter {
    /// `bin` may carry arguments, mirroring `PiAdapter::new`/`CodexAdapter::new`.
    pub fn new(bin: Option<&str>) -> Self {
        let raw = bin.unwrap_or("muse").trim();
        let mut parts = raw.split_whitespace().map(str::to_string);
        let program = parts.next().unwrap_or_else(|| "muse".to_string());
        Self {
            program,
            bin_args: parts.collect(),
        }
    }

    fn base(&self) -> Command {
        let resolved = super::resolve_program(&self.program)
            .unwrap_or_else(|_| ResolvedProgram::direct(&self.program));
        let mut cmd = Command::new(&resolved.program);
        cmd.args(&resolved.prefix);
        cmd.args(&self.bin_args);
        cmd
    }
}

impl AgentAdapter for MuseAdapter {
    fn name(&self) -> &'static str {
        "muse"
    }

    fn program(&self) -> &str {
        &self.program
    }

    fn provider(&self) -> &'static str {
        CATALOGUE_VENDOR
    }

    /// Issue #394: "macOS/Linux only" -- refuses outright on Windows rather
    /// than attempting (and failing) to resolve a binary that can never
    /// exist there.
    fn ready(&self) -> CtxResult<()> {
        if cfg!(windows) {
            return Err(
                "muse (Meta Muse Code) is macOS/Linux only (issue #394); not available on Windows"
                    .into(),
            );
        }
        super::resolve_program(&self.program)?;
        Ok(())
    }

    /// Issue #394: the Windows `ready()` refusal above is a permanent fact
    /// of the platform, never a "not installed yet" state -- no binary can
    /// ever exist for muse there. Distinguishing the two keeps
    /// `readiness_note()`'s "Not ready yet: ... (see issue #11)" clause
    /// (which reads as "go install this") from wrongly claiming that about
    /// muse on Windows.
    fn platform_unsupported(&self) -> bool {
        cfg!(windows)
    }

    fn detect(&self, command: &[String]) -> bool {
        command
            .first()
            .and_then(|p| Path::new(p).file_name())
            .map(|f| f.to_string_lossy() == "muse")
            .unwrap_or(false)
    }

    /// `muse exec "<prompt>"` -- verified (issue #394: "Headless `muse exec
    /// \"<prompt>\"`").
    fn headless_cmd(&self, prompt: &str, _session: &SessionId, extra: &[String]) -> Command {
        let mut cmd = self.base();
        cmd.arg("exec").arg(prompt).args(extra);
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

    /// `muse exec` with no positional prompt: the caller (`handoff::
    /// run_model`) pipes the prompt in on stdin, mirroring every other
    /// adapter's own `distiller_cmd` shape.
    fn distiller_cmd(&self, model: &str) -> Command {
        let mut cmd = self.base();
        cmd.arg("exec");
        cmd.args(self.model_args(model));
        cmd
    }

    /// No verified deny-write flag: `--approval-mode on-request|untrusted|
    /// never`, `--yolo`, `--disable-sandbox`, and `--sandbox-network
    /// proxy-only|restricted|enabled` (issue #394) all govern approval
    /// prompting or network reach, never a "no writes at all" pin. Disclosed
    /// via [`sandbox_residual_note`](Self::sandbox_residual_note).
    fn read_only_args(&self) -> Vec<String> {
        Vec::new()
    }

    fn sandbox_residual_note(&self) -> Option<String> {
        Some(
            "muse's documented flags (issue #394: --approval-mode, --yolo, --disable-sandbox, \
             --sandbox-network) govern approval prompting and network reach, not a verified \
             deny-write pin, so no read-only restriction is applied to this launch."
                .to_string(),
        )
    }

    /// No flag (issue #394: "no flag; `AGENTS.md`/`CLAUDE.md`/
    /// `.agents/AGENTS.md` with workspace trust gating").
    fn system_prompt_args(&self, _prompt: &str) -> Vec<String> {
        Vec::new()
    }

    fn system_prompt_supported(&self, _launch: &[String]) -> bool {
        false
    }

    /// `--model muse-spark-1.2` -- verified (issue #394).
    fn model_args(&self, model: &str) -> Vec<String> {
        if model.is_empty() {
            return Vec::new();
        }
        vec!["--model".to_string(), model.to_string()]
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

    /// Empty (see this module's own doc comment): issue #394 itself flags
    /// muse's transcript path as unverified third-party information, never
    /// confirmed against official docs -- unlike every other wave-3 adapter,
    /// this is not even a best-effort discovery, since there is no verified
    /// root directory to scan under in the first place.
    fn transcript_path(&self, _session: &SessionRef) -> PathBuf {
        PathBuf::new()
    }

    fn parse_events(&self, _jsonl: &str) -> Vec<NormalizedEvent> {
        Vec::new()
    }

    fn structural_context(&self, _jsonl: &str, _last_n: usize) -> StructuralContext {
        StructuralContext::default()
    }

    /// Unverified (issue #394: "Compact unverified (PreCompact/PostCompact
    /// hooks exist)" -- hooks are not an injectable slash command).
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

    fn built_args(adapter: &MuseAdapter, cmd: &Command) -> Vec<String> {
        super::super::built_args(adapter.program(), cmd)
    }

    #[test]
    fn headless_cmd_uses_the_verified_exec_subcommand() {
        let adapter = MuseAdapter::new(None);
        let session = SessionId::parse("11111111-2222-4333-8444-555555555555");
        let cmd = adapter.headless_cmd("do the thing", &session, &[]);
        assert_eq!(
            built_args(&adapter, &cmd),
            vec!["exec".to_string(), "do the thing".to_string()]
        );
    }

    #[test]
    fn distiller_cmd_carries_no_positional_prompt() {
        let adapter = MuseAdapter::new(None);
        let args = built_args(&adapter, &adapter.distiller_cmd("muse-spark-1.2"));
        assert_eq!(args[0], "exec");
        assert!(args.contains(&"--model".to_string()));
        assert!(args.contains(&"muse-spark-1.2".to_string()));
        assert!(!args.contains(&"do the thing".to_string()));
    }

    #[test]
    fn ready_refuses_on_windows() {
        let adapter = MuseAdapter::new(None);
        if cfg!(windows) {
            assert!(adapter.ready().is_err());
        }
    }

    /// Pins the platform-permanent-vs-transient distinction itself, on
    /// BOTH platforms: `true` only on Windows (where `ready()` also always
    /// fails), `false` everywhere else -- muse is the only adapter that
    /// overrides the trait's default `false` at all.
    #[test]
    fn platform_unsupported_matches_windows_only() {
        let adapter = MuseAdapter::new(None);
        assert_eq!(adapter.platform_unsupported(), cfg!(windows));
    }

    #[test]
    fn read_only_args_are_empty_with_a_disclosed_residual() {
        let adapter = MuseAdapter::new(None);
        assert!(adapter.read_only_args().is_empty());
        assert!(adapter.sandbox_residual_note().is_some());
    }

    #[test]
    fn system_prompt_is_reported_unsupported() {
        let adapter = MuseAdapter::new(None);
        assert!(adapter.system_prompt_args("x").is_empty());
        assert!(!adapter.system_prompt_supported(&[]));
    }

    #[test]
    fn transcript_path_is_empty_because_the_location_itself_is_unverified() {
        let adapter = MuseAdapter::new(None);
        let session = SessionRef {
            id: SessionId::parse("11111111-2222-4333-8444-555555555555"),
            cwd: PathBuf::from("/work/repo"),
        };
        assert_eq!(adapter.transcript_path(&session), PathBuf::new());
    }

    #[test]
    fn detect_matches_the_bare_binary_only() {
        let adapter = MuseAdapter::new(None);
        assert!(adapter.detect(&["muse".to_string()]));
        assert!(!adapter.detect(&["codex".to_string()]));
        assert!(!adapter.detect(&[]));
    }

    #[test]
    fn all_registers_muse() {
        let names: Vec<&str> = super::super::all(None).iter().map(|a| a.name()).collect();
        assert!(names.contains(&"muse"), "got {names:?}");
    }

    #[test]
    fn capabilities_report_no_events_and_no_system_prompt() {
        let caps = MuseAdapter::new(None).capabilities();
        assert!(!caps.events, "transcript location itself is unverified");
        assert!(!caps.system_prompt);
    }

    #[test]
    fn ladder_methods_answer_from_the_meta_vendor() {
        let adapter = MuseAdapter::new(None);
        let meta = catalogue::vendor("meta").expect("meta is a registered vendor");
        assert_eq!(
            adapter.model_strength("muse-spark-1.2"),
            catalogue::strength(meta, "muse-spark-1.2")
        );
    }

    #[test]
    fn parse_events_and_structural_context_stay_empty() {
        let adapter = MuseAdapter::new(None);
        assert!(adapter.parse_events("{\"role\":\"user\"}").is_empty());
        assert_eq!(
            adapter.structural_context("{\"role\":\"user\"}", 5),
            StructuralContext::default()
        );
    }
}
