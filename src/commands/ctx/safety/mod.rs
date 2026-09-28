//! zirv's own harness-neutral command safety policy (issue #83).
//!
//! Every harness zirv wraps has its own, incompatible way of deciding
//! whether a command is safe to run unattended (claude's `permissions.allow`/
//! `permissions.deny` globs plus hooks; codex's `--sandbox`/`--ask-for-
//! approval` flags plus `.rules` execpolicy files). This module gives zirv a
//! single, harness-neutral classification (`SafetyPolicy`, [`evaluate`]) that
//! every adapter then projects onto its own native mechanism, so one
//! operator setting produces equivalent behaviour everywhere -- the same
//! shape `policy.rs` already established for the seven-capability
//! `[policy]` table, applied here to concrete command strings instead of
//! abstract capabilities.
//!
//! ## Layering, and why it is not `ctx.toml`'s deep merge
//!
//! `[safety]` cannot use the ordinary deep merge (a later layer's array
//! would simply *replace* an earlier one's) or `REPO_FORBIDDEN`'s
//! all-or-nothing rejection (issue #83 requires a repo to be able to
//! *narrow* the policy -- add more `deny`/`ask` entries -- while never
//! widening it). So [`resolve`] folds the layers the way `policy::resolve`
//! folds `[policy]`, lifted whole out of `ctx.toml` by `config::CtxConfig::
//! load` before its own deep merge:
//!
//! - **`deny`/`ask`** are additive across layers: the built-in set (derived
//!   from `adapters::SHIPPED_POSTURE_ASK`/`_DENY`, the same live-verified
//!   claude posture PR #96 shipped) plus the operator's own `~/.zirv/
//!   ctx.toml` entries plus the repo's own `.zirv/ctx.toml` entries, all
//!   unioned. Adding a `deny`/`ask` entry can only ever make a command
//!   *stricter* to evaluate (deny and ask are both checked before allow --
//!   see [`evaluate`]), so a repo checkout contributing to either list is
//!   always safe, the identical reasoning `SandboxConfig::extra_deny`
//!   already uses.
//! - **`allow`** may be extended only by the operator's own home layer.
//!   `config.rs`'s `REPO_FORBIDDEN` table rejects a repo `ctx.toml` that
//!   sets `safety.allow` at all -- there is no narrowing reading of adding
//!   an allow entry (unlike `deny`/`ask`, evaluated *after* both), so it is
//!   forbidden outright rather than folded, mirroring `sandbox.extra_allow`.
//! - **`escape_allow`** (issue #147) is the identical operator-home-layer-
//!   only story, one narrower domain down: it clears a family for a
//!   `--dangerously-disable-sandbox` retry specifically, not the ordinary
//!   sandboxed path `allow` governs. Also `REPO_FORBIDDEN`, for the same
//!   widening-only reason. Unlike `allow`, it carries a built-in seed
//!   (`builtin_escape_allow`) -- the read-only shell-utility families most
//!   sandbox-escape prompts turned out to be -- gated behind a per-segment
//!   credential/root-scan screen (`escape_denied_by_screen`) that a family
//!   match alone can never bypass.
//! - **`default`** (the verdict for a command matching nothing) is
//!   `REPO_FORBIDDEN` outright too, for the same reason: it is a single
//!   scalar with no narrowing direction of its own.
//! - **Environment** (`ZIRV_CTX_SAFETY_DENY`/`_ASK`/`_ALLOW`/`_DEFAULT`)
//!   sits above the fold and wins outright, the operator's own escape
//!   hatch, mirroring `ZIRV_CTX_SANDBOX_EXTRA_DENY`/`_ALLOW`. It replaces
//!   the operator+repo *contribution* to a list, never the built-in set
//!   itself: there is no environment variable that removes a built-in
//!   protection, only ones that add to or replace what an operator/repo
//!   contributed on top of it.
//!
//! ## The matcher is pure
//!
//! [`evaluate`] and [`glob_match`] read no clock, filesystem or
//! environment -- the same discipline `rot.rs` holds its scoring functions
//! to. [`resolve`] (the layering step, one level up) takes its environment
//! as an injected closure, exactly like `policy::resolve`, so it stays
//! deterministic and testable without touching real process state.
//!
//! ## Two loop breakers (issue #313)
//!
//! A policy verdict alone cannot tell an agent stuck retrying variations of
//! the same blocked command, or re-running the identical failing command
//! over and over, to stop -- it can only keep saying "no" the same way each
//! time. Two additive, narrowing-only breakers sit in the PreToolUse hook
//! path (`run_check_hook_mode_with_env`), after the ordinary verdict is
//! final, and change only the TEXT a hook decision carries, never the
//! verdict family of any existing command:
//!
//! - The **consecutive-denial breaker** (`denial_breaker_threshold`) counts
//!   this session's own trailing run of `Ask`/`Deny` verdicts (via the
//!   bounded `log::read_recent_safety_decisions`) and, past the threshold,
//!   prefixes the hook's `permissionDecisionReason` with an explicit "stop
//!   retrying" instruction -- the original policy explanation stays, joined
//!   by ` -- `.
//! - The **identical-failing-command guard** (`identical_command_warn_after`/
//!   `_refuse_after`) parses the session's own transcript
//!   (`trailing_same_command_failure_run`) for a trailing run of failures of
//!   the EXACT SAME command and, past `warn_after`, adds a
//!   `hookSpecificOutput.additionalContext` warning to an otherwise-`Allow`
//!   verdict; past `refuse_after`, ONLY in a headless launch, turns that
//!   `Allow` into a `Deny`. A command already `is_read_only_escape_safe`
//!   (benign, repeatable inspection) is never guarded.
//!
//! Both thresholds fold across layers via `narrow_threshold`: unlike
//! `allow`/`default`/`sql` (`REPO_FORBIDDEN`, operator-home-layer only,
//! since there is no narrowing reading of widening either), a repo MAY
//! lower one of these three -- narrowing is always safe -- but never raise
//! it above the operator's own ceiling, and an operator's `0` (disabled)
//! can never be re-enabled by a repo. See `narrow_threshold`'s own doc
//! comment for the exact fold.

use std::io::Write;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::CtxResult;
use super::config::{CtxConfig, EnvLookup, env_from_process, split_csv_list};
use super::envelope;

/// One of the three things zirv's safety policy can say about a command.
/// Deliberately unrelated to `policy::Stance`: a `Stance` is a *capability*
/// posture ("may this session write outside the repo"), while a `Verdict` is
/// a per-command classification -- two different questions issue #83 and
/// issue #43 each answer.
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

    /// The `zirv ctx safety check`/`explain` exit code for this verdict --
    /// distinct per verdict so a caller can branch on the exit code alone
    /// without parsing output. `Deny` gets the conventional "blocked" code
    /// a PreToolUse hook would also use for a hard block (see `hook_output`
    /// below, though the wired hook itself always exits 0 -- see its own
    /// doc comment for why).
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

/// The fully resolved policy `evaluate` matches a command against --
/// `resolve`'s output, and what `CtxConfig::safety` holds after `load`.
/// `Clone`/`PartialEq` mirror `CtxConfig`'s own derives, which this type is
/// a field of.
/// Whether the SQL statement classifier ([`sql_outcome`]) participates in
/// [`evaluate`].
///
/// `On` is the shipped default. `Off` is the operator's own escape hatch for
/// a workflow the classifier prompts on too often, and it is `REPO_FORBIDDEN`
/// (`config.rs`) for the same reason `safety.allow`/`safety.default`/
/// `safety.interactive_default` are: turning it off removes the classifier's
/// `Ask` narrowing, which can only ever make the effective policy looser, so
/// there is no narrowing reading of `off` for a repo layer to be trusted with.
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
    /// Issue #147: patterns an operator has pre-cleared for a `--dangerously-
    /// disable-sandbox` retry specifically -- NOT folded into `allow`, which
    /// governs the sandboxed default path. Matched against every executable
    /// segment of the retried command (see `escape_allow_matches`), so a
    /// compound where only one segment qualifies still falls through to the
    /// ordinary Ask/Deny escalation. Operator-home-layer only, the same
    /// widening-only reasoning as `allow` -- see the module doc and
    /// `REPO_FORBIDDEN`'s `safety.escape_allow` entry. Empty by default:
    /// unlike `allow`, there is no built-in escape set.
    pub escape_allow: Vec<Rule>,
    /// The verdict for a command matching no rule on a HEADLESS launch.
    /// Unchanged: `Ask`, which claude's `dontAsk` mode turns into a refusal.
    /// Nobody is present to answer, so an unclassified command is an
    /// unsupervised risk.
    pub default: Verdict,
    /// The verdict for a command matching no rule on an INTERACTIVE launch
    /// (2026-08-24, primary acceptance criterion). `Allow`: an operator is
    /// watching, and prompting on every command zirv has not enumerated is
    /// precisely the endless-prompting failure this whole round exists to
    /// remove. Operator-overridable (`[safety] interactive_default`,
    /// `ZIRV_CTX_SAFETY_INTERACTIVE_DEFAULT`) and `REPO_FORBIDDEN`: `Allow`
    /// is the loosest verdict there is, so a checkout that could set it
    /// could silence every prompt for the session it sits in.
    pub interactive_default: Verdict,
    pub sql: SqlMode,
    /// Issue #313 (consecutive-denial breaker): how many trailing consecutive
    /// `Ask`/`Deny` verdicts in ONE session (this decision included) before
    /// the hook's `permissionDecisionReason` stops explaining the policy and
    /// instead tells the agent outright to stop retrying variations of the
    /// same command. `0` disables the breaker entirely -- the loosest
    /// setting, since it means a stuck agent gets no nudge at all. Folded
    /// narrowing-only across layers (see [`resolve`]'s own fold): a repo may
    /// LOWER this (fire the breaker sooner) but never raise it or turn a
    /// disabled breaker back on.
    pub denial_breaker_threshold: u32,
    /// Issue #313 (identical-failing-command guard): how many trailing
    /// consecutive failures of the EXACT SAME Bash command (same command
    /// text, from the session's own transcript) before an otherwise-`Allow`
    /// verdict also carries a warning in `hookSpecificOutput.
    /// additionalContext`. `0` disables the warning. Same narrowing-only fold
    /// as `denial_breaker_threshold`.
    pub identical_command_warn_after: u32,
    /// Issue #313: the same identical-failing-command count at which a
    /// HEADLESS launch (only) turns the verdict itself into `Deny` instead of
    /// merely warning -- interactive launches never auto-refuse (an operator
    /// is watching and can decide for themselves). `0` disables the refusal;
    /// same narrowing-only fold.
    pub identical_command_refuse_after: u32,
}

impl Default for SafetyPolicy {
    /// The built-in policy alone: what an operator who has written no
    /// `[safety]` table at all gets. "A fresh install already blocks the
    /// obvious destructive families ... without anyone writing config"
    /// (issue #83's acceptance) is this, unmodified.
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
