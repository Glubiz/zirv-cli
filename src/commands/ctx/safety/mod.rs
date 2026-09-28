//! zirv's own harness-neutral command safety policy (issue #83).
//!
//! Harness adapters project this shared command verdict onto their native
//! permission mechanisms.
//!
//! `[safety]` layers use a narrowing fold: built-in, home and repo `deny`/`ask`
//! rules accumulate; only the operator may add `allow` or `escape_allow` or
//! choose defaults. Environment overrides replace contributions, never built-in
//! protections. Deny and ask take precedence over allow (#83, #147).
//!
//! Evaluation is pure; resolution receives environment through an injected
//! closure. The hook applies denial and repeated-failure breakers after the
//! ordinary verdict. Breakers may add guidance or refuse an otherwise allowed
//! headless command, but a repo can only lower nonzero thresholds (#313).

use std::io::Write;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::CtxResult;
use super::config::{CtxConfig, EnvLookup, env_from_process, split_csv_list};
use super::envelope;

/// A per-command verdict, distinct from capability posture (#83).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Verdict {
    /// Safe to run unattended.
    Allow,
    /// Needs a human's attention before running.
    Ask,
    /// Must not run at all.
    Deny,
}

impl Verdict {
    pub fn label(self) -> &'static str {
        match self {
            Verdict::Allow => "allow",
            Verdict::Ask => "ask",
            Verdict::Deny => "deny",
        }
    }

    /// Exit code for CLI safety checks; the hook itself exits zero and carries
    /// its decision in the response payload.
    pub fn exit_code(self) -> i32 {
        match self {
            Verdict::Allow => 0,
            Verdict::Ask => 1,
            Verdict::Deny => 2,
        }
    }

    fn parse(raw: &str) -> Option<Self> {
        match raw.trim() {
            "allow" => Some(Verdict::Allow),
            "ask" => Some(Verdict::Ask),
            "deny" => Some(Verdict::Deny),
            _ => None,
        }
    }
}

/// Which layer contributed one rule -- what `zirv ctx safety list` renders
/// per entry so an operator can see what a repo checkout narrowed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Origin {
    /// Derived from the shipped posture constants and the reserved built-in
    /// names in `utils::RESERVED_COMMANDS`, always present regardless of
    /// configuration.
    BuiltIn,
    /// The operator's own `~/.zirv/ctx.toml`.
    Operator,
    /// A checked-out repository's `.zirv/ctx.toml` (`deny`/`ask` only --
    /// `allow`/`default` can never carry this origin, see the module doc).
    Repo,
    /// `ZIRV_CTX_SAFETY_*`, the operator's escape hatch above the fold.
    Env,
}

impl Origin {
    pub fn label(self) -> &'static str {
        match self {
            Origin::BuiltIn => "built-in",
            Origin::Operator => "~/.zirv/ctx.toml",
            Origin::Repo => "repo .zirv/ctx.toml",
            Origin::Env => "environment",
        }
    }
}

/// One glob-style command pattern (`*` matches any run of characters,
/// including none) plus where it came from.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct Rule {
    pub pattern: String,
    pub origin: Origin,
}

/// Whether SQL classification contributes `Ask`; only the operator may
/// disable it because a repo may not remove a safety narrowing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum SqlMode {
    #[default]
    On,
    Off,
}

impl SqlMode {
    pub fn label(self) -> &'static str {
        match self {
            SqlMode::On => "on",
            SqlMode::Off => "off",
        }
    }

    fn parse(raw: &str) -> Option<Self> {
        match raw.trim() {
            "on" => Some(SqlMode::On),
            "off" => Some(SqlMode::Off),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct SafetyPolicy {
    pub deny: Vec<Rule>,
    pub ask: Vec<Rule>,
    pub allow: Vec<Rule>,
    /// Operator-cleared sandbox escape patterns, separate from ordinary `allow`;
    /// every executable segment must match or the retry still prompts (#147).
    pub escape_allow: Vec<Rule>,
    /// Headless unmatched-command verdict; `Ask` fails closed without an operator.
    pub default: Verdict,
    /// Interactive unmatched-command verdict; only the operator may set it
    /// because `Allow` can suppress every unmatched prompt.
    pub interactive_default: Verdict,
    pub sql: SqlMode,
    /// Consecutive denied decisions before telling the agent to stop retrying;
    /// `0` disables the breaker and repo layers may only lower it (#313).
    pub denial_breaker_threshold: u32,
    /// Identical Bash failures before warning on an otherwise allowed command;
    /// `0` disables the warning and repo layers may only lower it (#313).
    pub identical_command_warn_after: u32,
    /// Identical Bash failures before denying headless retries; interactive
    /// launches only warn, and `0` disables refusal (#313).
    pub identical_command_refuse_after: u32,
}

impl Default for SafetyPolicy {
    /// Keep destructive built-in guards active even with no operator
    /// `[safety]` table (#83).
    fn default() -> Self {
        SafetyPolicy {
            deny: builtin_deny(),
            ask: builtin_ask(),
            allow: builtin_allow(),
            escape_allow: builtin_escape_allow(),
            default: Verdict::Ask,
            interactive_default: Verdict::Allow,
            sql: SqlMode::On,
            denial_breaker_threshold: 3,
            identical_command_warn_after: 2,
            identical_command_refuse_after: 5,
        }
    }
}

impl SafetyPolicy {
    /// The unmatched-command verdict for `mode` -- the one place the two
    /// defaults are chosen between, so no caller can pick the wrong one.
    pub fn default_verdict(&self, mode: super::adapters::LaunchMode) -> Verdict {
        if mode.is_interactive() {
            self.interactive_default
        } else {
            self.default
        }
    }
}

/// One evaluated command: the verdict, and the rule that produced it
/// (`None` means no rule matched and `policy.default` applied).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Outcome {
    pub verdict: Verdict,
    pub matched: Option<Rule>,
}

#[cfg(test)]
use super::testenv;
use super::{
    adapters, agent, config, hook, hook_project, jev, lifecycle, log, mail, obfuscate, pathutil,
    state,
};

mod attestation;
mod classifiers;
mod evaluation;
mod hook_core;
mod interface;
mod jev_approve;
mod policy;
mod readonly;
mod retry;
mod rules;
mod shell;
mod writes;

pub(crate) use attestation::sha256_hex;
pub use attestation::{POLICY_FINGERPRINT_ENV, POLICY_SNAPSHOT_ENV, policy_fingerprint};
pub use classifiers::sql_outcome;
pub(crate) use classifiers::sql_program_name;
pub use evaluation::evaluate;
pub(crate) use evaluation::{evaluate_with_scratchpad_roots, parse_envelope_env, pipeline_stages};
pub use hook_core::run_check;
#[cfg(test)]
use hook_core::run_check_hook_mode;
pub(crate) use hook_core::run_check_hook_with_verdict;
#[cfg(test)]
pub(crate) use hook_core::safety_family;
pub use interface::{CheckArgs, SafetyArgs, run};
pub(crate) use jev_approve::{
    APPROVE_ALLOW_MIN_CONFIDENCE, APPROVE_ALLOW_MIN_MARGIN, APPROVE_ESCALATE_MIN_CONFIDENCE,
    approve_escalate_action, approve_escalate_question, approve_lower_action,
    approve_lower_question, jev_approve_is_read_only_local,
};
pub use policy::{glob_match, resolve};
pub(crate) use readonly::is_read_only_escape_safe;
pub(crate) use retry::{
    SANDBOX_DENY_READ_HOME_PATHS, command_fails_escape_screen, is_reserved_zirv_escape_safe,
    text_names_credential_material,
};
pub use rules::{builtin_allow, builtin_ask, builtin_deny};
pub(crate) use rules::{
    command_pattern_from_bash_rule, reserved_zirv_command_patterns,
    reserved_zirv_sandbox_exclusion_patterns,
};
pub(crate) use shell::{
    collapse_whitespace, is_shell_identifier_assignment, normalize_segments, split_segments,
    strip_program_dir, tokenize_quoted, unwrap_compact_run_wrapper, unwrap_env_prefix,
    unwrap_launcher_prefix, unwrap_shell_wrapper,
};
pub(crate) use writes::{orchestrator_repo_write_target, write_targets_confined};

use attestation::*;
use classifiers::*;
use evaluation::*;
use hook_core::*;
use interface::*;
use jev_approve::*;
use readonly::*;
use retry::*;
use rules::*;
use shell::*;
use writes::*;

#[cfg(test)]
mod tests {
    use super::*;
    pub(super) use crate::commands::ctx::adapters::LaunchMode;
    pub(super) use std::collections::HashMap;

    pub(super) fn env_from(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect()
    }

    #[cfg(unix)]
    pub(super) fn real_claude_scratchpad_root() -> String {
        scratchpad_write_roots(&std::env::temp_dir())
            .into_iter()
            .find(|root| root.starts_with("/tmp/claude-"))
            .expect("real claude scratchpad root")
    }

    pub(super) fn table(text: &str) -> Option<toml::Value> {
        Some(toml::from_str::<toml::Value>(text).expect("test toml parses"))
    }

    pub(super) fn audited_unsandboxed_retry(
        cfg: &CtxConfig,
        command: &str,
        permission_mode: &str,
    ) -> (String, String) {
        let state = tempfile::tempdir().expect("state");
        let env = env_from(&[(
            super::super::state::STATE_ENV,
            state.path().to_str().expect("utf8 state"),
        )]);
        let stdin = serde_json::json!({
            "session_id": format!("retry-{permission_mode}"),
            "tool_name": "Bash",
            "tool_input": {
                "command": command,
                "dangerouslyDisableSandbox": true
            },
            "permission_mode": permission_mode
        })
        .to_string();
        let mut out = Vec::new();
        run_check_hook_mode_with_env(cfg, &mut out, &stdin, &|key| env.get(key).cloned())
            .expect("runs");
        let output = String::from_utf8(out).expect("utf8");
        let audit_dir = state.path().join("logs/safety-decisions");
        let audit_path = std::fs::read_dir(audit_dir)
            .expect("audit dir")
            .next()
            .expect("one audit file")
            .expect("audit entry")
            .path();
        let audit = std::fs::read_to_string(audit_path).expect("audit");
        (output, audit)
    }

    // -- orchestrator_repo_write_target (issues #328/#334) -----------------

    /// Test fake for `repo_root_of`: `/work/repo` and `/work/sibling` are
    /// two distinct git repositories (the launch repo and a sibling
    /// checkout/linked worktree); anything else names no repository at
    /// all. Mirrors `filesystem_repo_root_of`'s own root-plus-separator
    /// boundary rule, just without touching a real filesystem.
    pub(super) fn fake_repo_root_of(path: &str) -> Option<String> {
        for root in ["/work/repo", "/work/sibling"] {
            if path == root || path.starts_with(&format!("{root}/")) {
                return Some(root.to_string());
            }
        }
        None
    }

    // -- evaluate: destructive families the issue lists --------------

    pub(super) fn policy_with(
        deny: &[&str],
        ask: &[&str],
        allow: &[&str],
        default: Verdict,
    ) -> SafetyPolicy {
        let rule = |p: &str| Rule {
            pattern: p.to_string(),
            origin: Origin::Operator,
        };
        SafetyPolicy {
            deny: deny.iter().map(|p| rule(p)).collect(),
            ask: ask.iter().map(|p| rule(p)).collect(),
            allow: allow.iter().map(|p| rule(p)).collect(),
            escape_allow: Vec::new(),
            default,
            interactive_default: Verdict::Allow,
            sql: SqlMode::On,
            ..SafetyPolicy::default()
        }
    }

    // -- Issue #326: `zirv ctx run --compact` is a transparent launcher -----

    /// The shipped policy, exactly as a real launch resolves it -- the
    /// built-in allow/ask/deny sets are what make "same verdict as bare"
    /// mean anything at all (a bare `SafetyPolicy::default()` carries no
    /// allow rules, so every ordinary command would fall to the same
    /// unmatched default with or without a wrapper).
    pub(super) fn shipped_policy() -> SafetyPolicy {
        resolve(None, None, &|_| None).expect("the shipped policy must resolve")
    }

    pub(super) fn safety_test_envelope() -> envelope::WorkerEnvelope {
        envelope::WorkerEnvelope {
            principal: "audit/worker".into(),
            paths: vec![envelope::PathScope::new("allowed")],
            tools: envelope::ToolSet::all(),
            network: true,
            destructive: false,
            delegation_depth: 0,
            expires_at: u64::MAX,
            token_budget: Some(100),
        }
    }

    /// Shared PoC runner for review round 3 (2026-08-27, CRITICAL):
    /// `is_root_wide_find_scan` used to compare a starting-point token
    /// against an EXACT literal set (`"/"`/`"~"`/`"~/"`), so every
    /// textually-different but lexically-identical root spelling sailed
    /// through -- `//`, `/.`, `/./`, `/..`, `/../`, and a bare `~user`
    /// (someone else's whole home, the identical unbounded-scan shape as a
    /// bare `~`) all still reached a silent `Allow` via the seeded `find *`
    /// family on the previous round's fix. Each PoC here is a real,
    /// shell-equivalent spelling of the filesystem root or a whole home
    /// directory.
    pub(super) fn assert_find_command_asks(command: &str) {
        let repo = tempfile::tempdir().expect("tempdir");
        let home = tempfile::tempdir().expect("tempdir");
        let _home = super::super::testenv::HomeGuard::set(home.path());
        let empty: HashMap<String, String> = HashMap::new();
        let cfg = CtxConfig::load(repo.path(), &|k| empty.get(k).cloned()).expect("loads");

        let stdin = format!(
            r#"{{"tool_name":"Bash","tool_input":{{"command":"{command}","dangerouslyDisableSandbox":true}},"permission_mode":"default"}}"#
        );
        let mut out = Vec::new();
        run_check_hook_mode(&cfg, &mut out, &stdin).expect("runs");
        let text = String::from_utf8(out).expect("utf8");
        assert!(
            text.contains(r#""permissionDecision":"ask""#),
            "`{command}` lexically resolves to a root-wide (or whole-home) scan and must not \
             escape: got {text}"
        );
    }

    pub(super) fn literal_retry_hook(command: &str, retry: bool, cwd: &str) -> serde_json::Value {
        let stdin = serde_json::json!({
            "tool_name": "Bash",
            "tool_input": {"command": command, "dangerouslyDisableSandbox": retry},
            "permission_mode": "default",
            "cwd": cwd,
        })
        .to_string();
        let mut out = Vec::new();
        run_check_hook_mode_with_env(&CtxConfig::default(), &mut out, &stdin, &|_| None).unwrap();
        serde_json::from_slice::<serde_json::Value>(&out).unwrap()["hookSpecificOutput"].clone()
    }

    // -- issue #313: consecutive-denial breaker (hook integration) -------

    /// Runs the hook once against a persistent `state` dir/session, the same
    /// shape `audited_unsandboxed_retry` above uses but reusable across
    /// several sequential calls (the breaker needs THIS session's own prior
    /// decisions to already be on disk).
    pub(super) fn run_hook_for_loop_breaker(
        cfg: &CtxConfig,
        state_root: &std::path::Path,
        session: &str,
        command: &str,
        permission_mode: &str,
    ) -> String {
        let env = env_from(&[(
            super::super::state::STATE_ENV,
            state_root.to_str().expect("utf8 state"),
        )]);
        let stdin = serde_json::json!({
            "session_id": session,
            "tool_name": "Bash",
            "tool_input": { "command": command },
            "permission_mode": permission_mode
        })
        .to_string();
        let mut out = Vec::new();
        run_check_hook_mode_with_env(cfg, &mut out, &stdin, &|key| env.get(key).cloned())
            .expect("runs");
        String::from_utf8(out).expect("utf8")
    }

    pub(super) fn default_cfg_for_loop_breaker_tests() -> CtxConfig {
        let repo = tempfile::tempdir().expect("tempdir");
        let home = tempfile::tempdir().expect("tempdir");
        let _home = super::super::testenv::HomeGuard::set(home.path());
        let empty: HashMap<String, String> = HashMap::new();
        CtxConfig::load(repo.path(), &|k| empty.get(k).cloned()).expect("loads")
    }

    // -- issue #313: identical-failing-command guard ----------------------

    /// One `(tool_use, tool_result)` Bash pair as two transcript JSONL lines,
    /// the identical shape claude's own real transcripts use (and the
    /// existing `structural_context`/`last_verification_run` fixtures in
    /// `adapters::claude` already exercise): `id` links the pair, `is_error`
    /// marks a failure.
    pub(super) fn transcript_bash_pair(id: &str, command: &str, is_error: bool) -> String {
        format!(
            "{}\n{}\n",
            serde_json::json!({
                "type": "assistant",
                "message": {
                    "content": [
                        {"type": "tool_use", "id": id, "name": "Bash", "input": {"command": command}}
                    ]
                }
            }),
            serde_json::json!({
                "type": "user",
                "message": {
                    "content": [
                        {"type": "tool_result", "tool_use_id": id, "is_error": is_error, "content": "x"}
                    ]
                }
            })
        )
    }

    pub(super) fn transcript_jsonl(entries: &[(&str, &str, bool)]) -> String {
        entries
            .iter()
            .map(|(id, command, is_error)| transcript_bash_pair(id, command, *is_error))
            .collect()
    }

    pub(super) fn write_transcript(dir: &tempfile::TempDir, jsonl: &str) -> String {
        let path = dir.path().join("transcript.jsonl");
        std::fs::write(&path, jsonl).expect("write transcript");
        path.to_str().expect("utf8 path").to_string()
    }

    /// Every call gets a FRESH state dir: these guard tests care only about
    /// what the transcript produces, and an isolated, empty state dir keeps
    /// the (session-history-driven) denial breaker from this same PR ever
    /// contributing to the output -- the same isolation `audited_
    /// unsandboxed_retry` above already gives its own single-call tests.
    pub(super) fn run_hook_with_transcript(
        cfg: &CtxConfig,
        command: &str,
        permission_mode: &str,
        transcript_path: &str,
    ) -> String {
        let state = tempfile::tempdir().expect("state");
        let env = env_from(&[(
            super::super::state::STATE_ENV,
            state.path().to_str().expect("utf8 state"),
        )]);
        let stdin = serde_json::json!({
            "session_id": "guard-session",
            "tool_name": "Bash",
            "tool_input": { "command": command },
            "permission_mode": permission_mode,
            "transcript_path": transcript_path,
        })
        .to_string();
        let mut out = Vec::new();
        run_check_hook_mode_with_env(cfg, &mut out, &stdin, &|key| env.get(key).cloned())
            .expect("runs");
        String::from_utf8(out).expect("utf8")
    }

    // -- Issue #781: `[jev] approve`/`approve_allow` safety-hook risk check --

    pub(super) fn jev_approve_test_cfg(base_url: String, credential_env: &str) -> CtxConfig {
        let mut cfg = CtxConfig::default();
        cfg.jev.approve = true;
        cfg.jev.approve_allow = true;
        cfg.proxy.typesafe.base_url = base_url;
        cfg.proxy.typesafe.credential_env = credential_env.to_string();
        cfg.proxy.typesafe.timeout_secs = 5;
        cfg
    }

    pub(super) fn jev_approve_stdin(command: &str, permission_mode: &str) -> String {
        serde_json::json!({
            "session_id": "jev-approve-test",
            "tool_name": "Bash",
            "tool_input": { "command": command },
            "permission_mode": permission_mode
        })
        .to_string()
    }

    pub(super) fn run_jev_approve_hook(
        cfg: &CtxConfig,
        command: &str,
        permission_mode: &str,
        state_root: &std::path::Path,
    ) -> Option<Verdict> {
        let env = env_from(&[(
            super::super::state::STATE_ENV,
            state_root.to_str().expect("utf8 state root"),
        )]);
        let stdin = jev_approve_stdin(command, permission_mode);
        let mut out = Vec::new();
        run_check_hook_with_verdict(cfg, &mut out, &stdin, &|k| env.get(k).cloned())
            .expect("hook runs")
    }
}
