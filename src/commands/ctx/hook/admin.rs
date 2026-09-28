//! `zirv ctx hook {install,audit,status}` -- non-payload-driven subcommands.

use std::io::Write;

use crate::commands::ctx::config::EnvLookup;
use crate::commands::ctx::state::{StateDir, now_secs};
use crate::commands::ctx::{CtxResult, log};

/// Install or remove an adapter's native hooks without requiring its
/// binary or operator gate; unknown or unsupported adapters exit 1 (#418).
/// Resolves the agent via the adapter registry directly, never
/// `adapters::select`, which requires both to be true.
pub(super) fn run_hook_install<W: Write>(
    w: &mut W,
    agent: &str,
    show: bool,
    uninstall: bool,
    dry_run: bool,
) -> CtxResult<i32> {
    let Some(adapter) = crate::commands::ctx::adapters::ADAPTERS
        .iter()
        .find(|(name, _)| *name == agent)
        .map(|(_, ctor)| ctor(None))
    else {
        writeln!(
            w,
            "unknown agent '{agent}'; known adapters: {}",
            crate::commands::ctx::adapters::ADAPTERS
                .iter()
                .map(|(name, _)| *name)
                .collect::<Vec<_>>()
                .join(", ")
        )?;
        return Ok(1);
    };
    let home = crate::utils::home_dir()?;
    let Some(hooks) = adapter.native_hooks(&home) else {
        writeln!(w, "agent '{agent}' has no native hook seam")?;
        return Ok(1);
    };

    if show {
        writeln!(w, "{}", hooks.file.display())?;
        for (label, state) in crate::commands::ctx::native_hooks::status(&hooks)? {
            let state = match state {
                crate::commands::ctx::native_hooks::InstallState::Installed => "installed",
                crate::commands::ctx::native_hooks::InstallState::Missing => "missing",
            };
            writeln!(w, "  {label}: {state}")?;
        }
        return Ok(0);
    }

    if uninstall {
        if dry_run {
            for (label, state) in crate::commands::ctx::native_hooks::status(&hooks)? {
                if state == crate::commands::ctx::native_hooks::InstallState::Installed {
                    writeln!(w, "would remove: {label}")?;
                }
            }
            return Ok(0);
        }
        let report = crate::commands::ctx::native_hooks::uninstall(&hooks)?;
        if report.file_removed {
            writeln!(w, "removed {}", hooks.file.display())?;
        } else if report.removed.is_empty() {
            writeln!(w, "nothing to remove")?;
        } else {
            for label in &report.removed {
                writeln!(w, "removed: {label}")?;
            }
        }
        return Ok(0);
    }

    if dry_run {
        for (label, state) in crate::commands::ctx::native_hooks::status(&hooks)? {
            if state == crate::commands::ctx::native_hooks::InstallState::Missing {
                writeln!(w, "would install: {label}")?;
            }
        }
        return Ok(0);
    }

    let report = crate::commands::ctx::native_hooks::install(&hooks)?;
    if report.written.is_empty() {
        writeln!(w, "already installed: {}", hooks.file.display())?;
    } else {
        writeln!(w, "installed into {}", hooks.file.display())?;
        for label in &report.written {
            writeln!(w, "  {label}")?;
        }
    }
    Ok(0)
}

/// Summarize hook decisions, safety families and compaction outcomes from
/// existing logs without exposing raw commands (#424).
pub(super) fn run_audit<W: Write>(w: &mut W, since: &str, env: EnvLookup<'_>) -> CtxResult<i32> {
    let state = StateDir::resolve(env)?;
    let since_secs = crate::commands::ctx::spend::parse_since(since).ok_or_else(|| {
        format!(
            "--since '{since}': expected a duration like 30m, 24h, or 7d (or a bare number of \
             seconds)"
        )
    })?;
    let since_ts = now_secs().saturating_sub(since_secs);

    let decisions: Vec<log::DecisionRecord> = log::read_decisions(&state)
        .into_iter()
        .filter(|d| d.ts >= since_ts)
        .collect();
    let mut by_verb: std::collections::BTreeMap<String, std::collections::BTreeMap<String, u64>> =
        Default::default();
    for d in &decisions {
        *by_verb
            .entry(d.verb.clone())
            .or_default()
            .entry(d.verdict.clone())
            .or_insert(0) += 1;
    }
    writeln!(w, "hook decision audit, --since {since}")?;
    writeln!(w, "decisions: {} rows", decisions.len())?;
    for (verb, verdicts) in &by_verb {
        let parts: Vec<String> = verdicts.iter().map(|(v, n)| format!("{v}: {n}")).collect();
        writeln!(w, "  {verb:<10} {}", parts.join(", "))?;
    }

    let mut skip_reasons: std::collections::BTreeMap<&str, u64> = Default::default();
    for d in &decisions {
        if d.action == "reuse-probe-skipped" {
            *skip_reasons.entry(d.detail.as_str()).or_insert(0) += 1;
        }
    }
    if !skip_reasons.is_empty() {
        writeln!(w, "\nskip reasons:")?;
        for (reason, n) in &skip_reasons {
            writeln!(w, "  {reason:<30} {n}")?;
        }
    }

    let orchestrator_blocks: Vec<_> = log::read_orchestrator_blocks(&state)
        .into_iter()
        .filter(|b| b.ts >= since_ts)
        .collect();
    let mut denied_programs: std::collections::BTreeMap<String, u64> = Default::default();
    let mut other_denials = 0u64;
    for b in &orchestrator_blocks {
        if b.outcome != "denied" {
            continue;
        }
        if matches!(b.tool.as_str(), "Bash" | "PowerShell") {
            *denied_programs.entry(b.target.clone()).or_insert(0) += 1;
        } else {
            other_denials += 1;
        }
    }
    writeln!(
        w,
        "\ndenied programs (orchestrator write guard, Bash/PowerShell only):"
    )?;
    if denied_programs.is_empty() {
        writeln!(w, "  none")?;
    } else {
        let mut sorted: Vec<_> = denied_programs.into_iter().collect();
        sorted.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        for (program, n) in sorted.into_iter().take(10) {
            writeln!(w, "  {program:<30} {n}")?;
        }
    }
    writeln!(
        w,
        "other denied tool calls (file path, not a program): {other_denials}"
    )?;

    let safety_denials: Vec<_> = log::read_safety_decisions(&state)
        .into_iter()
        .filter(|d| d.ts >= since_ts && d.verdict == "deny")
        .collect();
    writeln!(w, "\nsafety-policy denials: {}", safety_denials.len())?;
    writeln!(w, "blocked families (commands themselves stay hashed):")?;
    let mut blocked_families: std::collections::BTreeMap<&str, u64> = Default::default();
    for d in &safety_denials {
        let family = if d.family.is_empty() {
            "unknown"
        } else {
            d.family.as_str()
        };
        *blocked_families.entry(family).or_insert(0) += 1;
    }
    if blocked_families.is_empty() {
        writeln!(w, "  none")?;
    } else {
        let mut sorted: Vec<_> = blocked_families.into_iter().collect();
        sorted.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(b.0)));
        for (family, n) in sorted.into_iter().take(10) {
            writeln!(w, "  {family:<30} {n}")?;
        }
    }

    let ledger_outcomes = crate::commands::ctx::ledger::outcome_counts_since(&state, since_ts);
    writeln!(w, "\nledger outcomes:")?;
    if ledger_outcomes.is_empty() {
        writeln!(w, "  no rows")?;
    } else {
        for (outcome, n) in &ledger_outcomes {
            writeln!(w, "  {outcome:<15} {n}")?;
        }
    }
    Ok(0)
}

/// Report hook status inline and exit zero even when classification or
/// healing fails; status is advisory (#420).
pub(super) fn run_hook_status<W: Write>(
    w: &mut W,
    heal: bool,
    env: EnvLookup<'_>,
) -> CtxResult<i32> {
    let home = crate::utils::home_dir()?;
    let state = StateDir::resolve(env)?;
    if heal {
        match crate::commands::ctx::hook_integrity::heal_outdated(&state, &home) {
            Ok(summary) => {
                writeln!(
                    w,
                    "healed {} hook entr{}",
                    summary.healed,
                    if summary.healed == 1 { "y" } else { "ies" }
                )?;
                for reason in &summary.refused {
                    writeln!(w, "{reason}")?;
                }
            }
            Err(error) => writeln!(w, "heal failed: {error}")?,
        }
    }
    match crate::commands::ctx::hook_integrity::report(&state, &home) {
        Ok(rows) => {
            let (mut ok, mut outdated, mut missing, mut modified, mut no_baseline) =
                (0, 0, 0, 0, 0);
            for row in &rows {
                writeln!(
                    w,
                    "{:<7} {}{} {}",
                    row.provider,
                    row.event,
                    crate::commands::ctx::hook_integrity::matcher_suffix(row.matcher),
                    row.state.label()
                )?;
                match row.state {
                    crate::commands::ctx::hook_integrity::HookState::Ok => ok += 1,
                    crate::commands::ctx::hook_integrity::HookState::Outdated => outdated += 1,
                    crate::commands::ctx::hook_integrity::HookState::Missing => missing += 1,
                    crate::commands::ctx::hook_integrity::HookState::Modified => modified += 1,
                    crate::commands::ctx::hook_integrity::HookState::NoBaseline => no_baseline += 1,
                }
            }
            writeln!(
                w,
                "summary: {ok} ok, {outdated} outdated, {missing} missing, {modified} modified, \
                 {no_baseline} no-baseline"
            )?;
        }
        Err(error) => writeln!(w, "hook status unavailable: {error}")?,
    }
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::super::HookArgs;
    use super::super::HookEvent;
    use super::*;

    /// Issue #420: `zirv ctx hook status` before anything was ever installed
    /// reports every slot `no-baseline` (never installed, never a
    /// regression) and still prints a summary line, never an error.
    #[test]
    fn hook_status_reports_no_baseline_before_any_install() {
        let dir = tempfile::tempdir().expect("tempdir");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(dir.path());
        let state = dir.path().join("state");
        let env: std::collections::HashMap<String, String> = [(
            crate::commands::ctx::state::STATE_ENV.to_string(),
            state.display().to_string(),
        )]
        .into();

        let mut out = Vec::new();
        run_hook_status(&mut out, false, &|k| env.get(k).cloned()).expect("status");
        let report = String::from_utf8(out).expect("utf8");
        assert!(report.contains("no-baseline"), "got {report}");
        assert!(report.contains("summary:"), "got {report}");
    }

    /// Issue #418: `--show` never creates the file, and reports every entry
    /// missing before any install has happened.
    #[test]
    fn hook_install_show_writes_nothing() {
        let dir = tempfile::tempdir().expect("tempdir");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(dir.path());
        let mut out = Vec::new();
        let code = run_hook_install(&mut out, "copilot", true, false, false).expect("runs");
        assert_eq!(code, 0);
        let report = String::from_utf8(out).expect("utf8");
        assert!(report.contains("missing"), "got {report}");
        assert!(
            !dir.path()
                .join(".copilot")
                .join("hooks")
                .join("zirv.json")
                .exists(),
            "--show must never create the file"
        );
    }

    /// A first `install` writes the entries and reports them; a second,
    /// identical call reports "already installed" instead -- idempotent,
    /// matching `native_hooks::install`'s own contract.
    #[test]
    fn hook_install_then_a_second_run_reports_already_installed() {
        let dir = tempfile::tempdir().expect("tempdir");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(dir.path());

        let mut first = Vec::new();
        let code = run_hook_install(&mut first, "droid", false, false, false).expect("runs");
        assert_eq!(code, 0);
        let first_report = String::from_utf8(first).expect("utf8");
        assert!(
            first_report.contains("installed into"),
            "got {first_report}"
        );

        let mut second = Vec::new();
        let code = run_hook_install(&mut second, "droid", false, false, false).expect("runs");
        assert_eq!(code, 0);
        let second_report = String::from_utf8(second).expect("utf8");
        assert!(
            second_report.contains("already installed"),
            "got {second_report}"
        );
    }

    #[test]
    fn hook_install_refuses_an_unknown_agent() {
        let dir = tempfile::tempdir().expect("tempdir");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(dir.path());
        let mut out = Vec::new();
        let code =
            run_hook_install(&mut out, "not-a-real-agent", false, false, false).expect("runs");
        assert_eq!(code, 1);
        let report = String::from_utf8(out).expect("utf8");
        assert!(report.contains("unknown agent"), "got {report}");
    }

    #[test]
    fn hook_install_refuses_an_agent_with_no_native_hooks() {
        let dir = tempfile::tempdir().expect("tempdir");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(dir.path());
        let mut out = Vec::new();
        let code = run_hook_install(&mut out, "codex", false, false, false).expect("runs");
        assert_eq!(code, 1);
        let report = String::from_utf8(out).expect("utf8");
        assert!(report.contains("no native hook seam"), "got {report}");
    }

    /// `--heal` is a no-op (and still reports) when there is nothing to heal
    /// (`LEGACY_HOOK_SHAPES` is empty, or the target was never installed).
    #[test]
    fn hook_status_heal_is_a_no_op_with_no_legacy_shapes_to_heal_from() {
        let dir = tempfile::tempdir().expect("tempdir");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(dir.path());
        let state = dir.path().join("state");
        let env: std::collections::HashMap<String, String> = [(
            crate::commands::ctx::state::STATE_ENV.to_string(),
            state.display().to_string(),
        )]
        .into();

        let mut out = Vec::new();
        run_hook_status(&mut out, true, &|k| env.get(k).cloned()).expect("status --heal");
        let report = String::from_utf8(out).expect("utf8");
        assert!(report.contains("healed 0 hook entries"), "got {report}");
    }

    // -- hook audit (issue #424) -------------------------------------------

    #[test]
    fn hook_audit_parses_with_its_default_since() {
        use clap::Parser as _;
        let cli = crate::commands::ctx::CtxCli::try_parse_from(["zirv ctx", "hook", "audit"])
            .expect("hook audit should parse");
        match cli.verb {
            crate::commands::ctx::CtxVerb::Hook(HookArgs {
                event: HookEvent::Audit { since },
            }) => assert_eq!(since, "7d"),
            other => panic!("expected Hook(Audit), got {other:?}"),
        }

        let cli = crate::commands::ctx::CtxCli::try_parse_from([
            "zirv ctx", "hook", "audit", "--since", "24h",
        ])
        .expect("hook audit --since should parse");
        match cli.verb {
            crate::commands::ctx::CtxVerb::Hook(HookArgs {
                event: HookEvent::Audit { since },
            }) => assert_eq!(since, "24h"),
            other => panic!("expected Hook(Audit), got {other:?}"),
        }
    }

    /// A fixture log with mixed outcomes (issue #424) aggregates correctly:
    /// verb/verdict counts, a skip reason, a denied Bash program, a
    /// safety-log denial count with its blocked family named (Change 5a),
    /// and the ledger's own outcome counts.
    #[test]
    fn hook_audit_aggregates_a_mixed_fixture_correctly() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().to_path_buf());
        let now = now_secs();

        log::append(
            &state,
            &log::Decision {
                ts: now - 10,
                session: "sess-1",
                verb: "hook",
                verdict: "n/a",
                score: 0,
                action: "reuse-probe-skipped",
                detail: "no diff",
                observed_at: None,
            },
        )
        .expect("append");
        log::append(
            &state,
            &log::Decision {
                ts: now - 9,
                session: "sess-1",
                verb: "hook",
                verdict: "deny",
                score: 80,
                action: "dispatch-denied",
                detail: "",
                observed_at: None,
            },
        )
        .expect("append");
        // Outside the `--since 7d` window -- must not be counted.
        log::append(
            &state,
            &log::Decision {
                ts: now - 20 * 86_400,
                session: "sess-1",
                verb: "hook",
                verdict: "deny",
                score: 80,
                action: "dispatch-denied",
                detail: "",
                observed_at: None,
            },
        )
        .expect("append");

        log::append_orchestrator_block(
            &state,
            &log::OrchestratorBlock {
                ts: now - 5,
                session: "sess-1",
                tool: "Bash",
                target: "sed -i",
                reason: "orchestrator seats may not edit repository files",
                outcome: "denied",
            },
        )
        .expect("append");
        log::append_orchestrator_block(
            &state,
            &log::OrchestratorBlock {
                ts: now - 4,
                session: "sess-1",
                tool: "Edit",
                target: "/work/repo/src/main.rs",
                reason: "orchestrator seats may not edit repository files",
                outcome: "denied",
            },
        )
        .expect("append");

        log::append_safety(
            &state,
            &log::SafetyDecision {
                ts: now - 3,
                session: "sess-1",
                mode: "interactive",
                verdict: "deny",
                family: "rm -rf",
                command_sha256: "aaa",
                policy_sha256: "p",
                launch_policy_sha256: None,
                attestation: "not-present",
                matched_pattern: None,
                origin: Some("built-in"),
                platform: "linux",
            },
        )
        .expect("append");

        crate::commands::ctx::ledger::record(
            &state,
            &crate::commands::ctx::ledger::CompactionRow {
                ts: now - 2,
                tool_use_id: "toolu_1",
                session: "sess-1",
                repo: "repo-a",
                program: "cargo",
                bytes_in: 10_000,
                bytes_out: 500,
                outcome: crate::commands::ctx::ledger::Outcome::Compacted,
                retrieval_id: Some("r1"),
            },
        );

        let state_root = state.root().to_path_buf();
        let env = move |key: &str| {
            (key == crate::commands::ctx::state::STATE_ENV)
                .then(|| state_root.display().to_string())
        };
        let mut out = Vec::new();
        let code = run_audit(&mut out, "7d", &env).expect("runs");
        assert_eq!(code, 0);
        let text = String::from_utf8(out).expect("utf8");
        assert!(text.contains("decisions: 2 rows"), "{text}");
        assert!(text.contains("hook       deny: 1, n/a: 1"), "{text}");
        assert!(text.contains("no diff"), "{text}");
        assert!(text.contains("sed -i"), "{text}");
        assert!(
            text.contains("other denied tool calls (file path, not a program): 1"),
            "{text}"
        );
        assert!(text.contains("safety-policy denials: 1"), "{text}");
        assert!(
            text.contains("rm -rf"),
            "blocked family must be named: {text}"
        );
        assert!(text.contains("compacted"), "{text}");
    }
}
