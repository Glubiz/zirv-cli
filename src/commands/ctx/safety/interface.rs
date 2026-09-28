//! Interface rules for command safety.

use super::*;

// ---------------------------------------------------------------------
// CLI: `zirv ctx safety check|list|explain`
// ---------------------------------------------------------------------

#[derive(Debug, clap::Args)]
pub struct SafetyArgs {
    #[command(subcommand)]
    pub verb: SafetyVerb,
}

#[derive(Debug, clap::Subcommand)]
pub enum SafetyVerb {
    /// Evaluate one command against the effective safety policy (`-- <command>`),
    /// or -- with no trailing command -- read a claude PreToolUse hook payload
    /// from stdin. This is what `zirv setup apply` wires into the harness hook.
    Check(CheckArgs),
    /// Show the effective merged policy, with the layer each rule came from.
    List(ListArgs),
    /// Explain why a command received its verdict.
    Explain(ExplainArgs),
}

#[derive(Debug, clap::Args)]
pub struct CheckArgs {
    #[arg(long, default_value = ".")]
    pub repo: PathBuf,
    /// Which launch posture to check under. Only affects a command that
    /// matches no rule: interactive allows it, headless asks.
    #[arg(long, value_enum, default_value = "interactive")]
    pub mode: super::adapters::LaunchMode,
    /// The command to check, after `--`. Omitted entirely when this is
    /// invoked as a PreToolUse hook (the command then comes from the JSON
    /// payload on stdin).
    #[arg(allow_hyphen_values = true, last = true)]
    pub command: Vec<String>,
    /// Issue #418: hook mode only -- project a non-claude agent's own native
    /// `PreToolUse`-equivalent payload onto this hook's claude shape before
    /// evaluating, then translate the verdict back into that agent's own
    /// response envelope. Omitted (or `claude`) leaves this byte-for-byte
    /// identical to the original claude-only hook.
    #[arg(long)]
    pub agent: Option<String>,
}

#[derive(Debug, clap::Args)]
pub struct ListArgs {
    #[arg(long, default_value = ".")]
    pub repo: PathBuf,
    #[arg(long, default_value_t = false)]
    pub json: bool,
}

#[derive(Debug, clap::Args)]
pub struct ExplainArgs {
    #[arg(long, default_value = ".")]
    pub repo: PathBuf,
    /// Which launch posture to explain the verdict under. An unmatched
    /// command is allowed interactively and asked headlessly, and an `ask`
    /// verdict prompts interactively and fails closed headlessly -- so the
    /// same rule means two different things.
    #[arg(long, value_enum, default_value = "interactive")]
    pub mode: super::adapters::LaunchMode,
    #[arg(allow_hyphen_values = true, last = true)]
    pub command: Vec<String>,
}

pub(super) fn read_stdin() -> String {
    use std::io::Read;
    let mut buffer = String::new();
    let _ = std::io::stdin().read_to_string(&mut buffer);
    buffer
}

pub(super) fn render_outcome(command: &str, outcome: &Outcome) -> String {
    let head = match &outcome.matched {
        Some(rule) => format!(
            "{}: matched `{}` [{}]",
            outcome.verdict.label(),
            rule.pattern,
            rule.origin.label()
        ),
        None => format!(
            "{}: no rule matched; using the configured default",
            outcome.verdict.label()
        ),
    };
    format!("{head} (`{command}`)")
}

/// What the verdict actually DOES to a launch in `mode` -- the half an
/// operator cannot read off the matched rule alone (2026-08-24). Naming the
/// concrete flag in each sentence is deliberate: an operator debugging "why
/// did that just prompt" needs the flag to search their own scrollback for.
fn mode_consequence(verdict: Verdict, mode: super::adapters::LaunchMode) -> &'static str {
    use super::adapters::LaunchMode;
    match (verdict, mode) {
        (Verdict::Allow, LaunchMode::Interactive) => {
            "It runs with no prompt: on an interactive launch the safety hook states an explicit \
             `allow` decision, which is what keeps everyday and unclassified commands silent."
        }
        (Verdict::Allow, LaunchMode::Headless) => {
            "It runs with no prompt: it is pre-approved in the launch's own --allowedTools set."
        }
        (Verdict::Ask, LaunchMode::Interactive) => {
            "On an interactive launch (zirv chat, zirv ctx wrap, a dashboard pane) this prompts \
             you: claude runs under `--permission-mode default` with the safety hook as the sole \
             gate, and codex under `--ask-for-approval on-request` where the installed CLI \
             supports it."
        }
        (Verdict::Ask, LaunchMode::Headless) => {
            "On a headless launch (zirv ctx exec, zirv ctx loop, zirv ctx agent) nobody is present \
             to answer, so this fails closed: claude runs under `--permission-mode dontAsk` with \
             the ask set folded into --disallowedTools, and codex under `--ask-for-approval never`."
        }
        (Verdict::Deny, _) => "It is refused in every launch mode.",
    }
}

/// Issue #139: `divergence` names, in words, when `outcome` is stricter than
/// what the current policy would produce for the same command -- see
/// [`SnapshotDivergence`]'s own doc comment for why this exists. `Unchanged`
/// (every pre-existing caller, and any attested one whose snapshot agrees
/// with today's policy) leaves this function's output byte-for-byte what it
/// was before this parameter existed.
/// Code review fix: `status` (`AttestedEvaluation.status`) now also drives an
/// explicit note when it is `"self-healed"` -- previously this function only
/// ever read `divergence`, so a self-healed evaluation (an invalid, missing,
/// or hash-mismatched launch attestation, `self_healed_evaluation`) produced
/// an explanation indistinguishable from an ordinary, fully-verified one.
/// Both callers -- `hook_output`'s own `permissionDecisionReason` (what an
/// operator/transcript viewer actually sees for a live decision) and
/// `run_explain` (`zirv ctx safety explain`, run separately after the fact)
/// -- now surface it. `"not-present"`/`"valid"` add nothing, matching
/// today's behavior exactly.
pub(super) fn explain_text(
    command: &str,
    outcome: &Outcome,
    mode: super::adapters::LaunchMode,
    divergence: SnapshotDivergence,
    status: &str,
    envelope: Option<&envelope::WorkerEnvelope>,
) -> String {
    let head = match &outcome.matched {
        Some(rule) => format!(
            "`{command}` is {} because it matched the {} rule `{}` from {}.",
            outcome.verdict.label(),
            outcome.verdict.label(),
            rule.pattern,
            rule.origin.label()
        ),
        None => format!(
            "`{command}` is {} because no deny, ask or allow rule matched; the {} default ({}) \
             applies.",
            outcome.verdict.label(),
            mode.label(),
            outcome.verdict.label()
        ),
    };
    let mut text = format!("{head} {}", mode_consequence(outcome.verdict, mode));
    if let SnapshotDivergence::SnapshotStricter { current_verdict } = divergence {
        text.push_str(&format!(
            " Note: the launch snapshot (pinned at session start) is stricter than your current \
             policy, which would {}; restart the session (zirv chat) to adopt the widened \
             policy.",
            current_verdict.label()
        ));
    }
    if status == "self-healed" {
        text.push_str(
            " Note: the launch attestation self-healed -- the pinned launch-snapshot file was \
             missing, unreadable, or did not match its expected fingerprint, so this reflects \
             your current policy directly rather than an unverifiable launch snapshot.",
        );
    }
    // Issue #262: the delegation envelope's own contribution, when one
    // applies -- printed unconditionally rather than only on the commands it
    // actually denied, so `zirv ctx safety explain` also answers "what would
    // this worker's own scope allow" for a command it happens to already
    // allow for other reasons.
    if let Some(envelope) = envelope {
        let paths = if envelope.paths.is_empty() {
            "none".to_string()
        } else {
            envelope
                .paths
                .iter()
                .map(|scope| scope.0.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        };
        text.push_str(&format!(
            " Delegation envelope `{}`: paths [{paths}], network {}, destructive {}, depth {}.",
            envelope.principal, envelope.network, envelope.destructive, envelope.delegation_depth
        ));
    }
    text
}

pub fn run_list<W: Write>(args: &ListArgs, w: &mut W, env: EnvLookup<'_>) -> CtxResult<i32> {
    let cfg = CtxConfig::load(&args.repo, env)?;
    if args.json {
        writeln!(w, "{}", serde_json::to_string_pretty(&cfg.safety)?)?;
        return Ok(0);
    }
    writeln!(w, "default (headless): {}", cfg.safety.default.label())?;
    writeln!(
        w,
        "default (interactive): {}",
        cfg.safety.interactive_default.label()
    )?;
    writeln!(w, "sql classifier: {}", cfg.safety.sql.label())?;
    for (label, rules) in [
        ("deny", &cfg.safety.deny),
        ("ask", &cfg.safety.ask),
        ("allow", &cfg.safety.allow),
    ] {
        writeln!(w, "{label}:")?;
        for rule in rules {
            writeln!(w, "  {}  [{}]", rule.pattern, rule.origin.label())?;
        }
    }
    Ok(0)
}

/// Issue #139: previously bypassed attestation entirely, evaluating only the
/// currently-resolved policy -- which is exactly why this command and the
/// hook (`run_check_hook_mode_with_env`, which always goes through
/// `evaluate_with_attestation_evidence`) could disagree about the identical
/// command: the hook would report the pinned launch snapshot's stricter
/// verdict while this reported today's wider one, with no way for the
/// operator to see why.
///
/// Now routed through the SAME evidence function the hook uses, always --
/// not merely when the attestation env vars happen to be set, since
/// `evaluate_with_attestation_evidence` itself already degrades correctly
/// when they are absent (`status: "not-present"`, `divergence: Unchanged`,
/// `outcome` a plain `evaluate` call): the two ARE the "without the env vars"
/// case, so this command's behavior is byte-for-byte unchanged when they are
/// not set, and now agrees with the hook when they are.
pub fn run_explain<W: Write>(args: &ExplainArgs, w: &mut W, env: EnvLookup<'_>) -> CtxResult<i32> {
    let cfg = CtxConfig::load(&args.repo, env)?;
    let command = args.command.join(" ");
    // Same scratchpad roots the hook computes (`run_check_hook_mode_with_
    // env`), so this command's VCS narrowing (issue #306) agrees with what
    // the hook actually decided for the identical command.
    let scratchpad_roots = scratchpad_write_roots(&std::env::temp_dir());
    let cwd = std::env::current_dir().ok();
    let cwd = cwd.as_deref();
    let evidence = evaluate_with_attestation_evidence(
        &cfg.safety,
        &command,
        args.mode,
        env,
        &scratchpad_roots,
        cwd,
    );
    // Issue #262: re-parsed here (rather than threaded out of `evidence`)
    // purely to print its own contribution -- `evaluate_with_attestation_
    // evidence` already applied it to `evidence.outcome` internally.
    let envelope = parse_envelope_env(env);
    writeln!(
        w,
        "{}",
        explain_text(
            &command,
            &evidence.outcome,
            args.mode,
            evidence.divergence,
            evidence.status,
            envelope.as_ref(),
        )
    )?;
    Ok(evidence.outcome.verdict.exit_code())
}

pub fn run<W: Write>(args: &SafetyArgs, w: &mut W) -> CtxResult<i32> {
    let env = env_from_process();
    match &args.verb {
        SafetyVerb::Check(a) => run_check(a, w, &env),
        SafetyVerb::List(a) => run_list(a, w, &env),
        SafetyVerb::Explain(a) => run_explain(a, w, &env),
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::*;
    use super::*;

    // -- CLI ------------------------------------------------------------

    #[test]
    fn check_cli_mode_prints_the_verdict_and_exits_the_matching_code() {
        let repo = tempfile::tempdir().expect("tempdir");
        let home = tempfile::tempdir().expect("tempdir");
        let _home = super::super::testenv::HomeGuard::set(home.path());
        let empty: HashMap<String, String> = HashMap::new();
        let args = CheckArgs {
            repo: repo.path().to_path_buf(),
            mode: LaunchMode::Interactive,
            command: vec!["rm".to_string(), "-rf".to_string(), "/".to_string()],
            agent: None,
        };
        let mut out = Vec::new();
        let code = run_check(&args, &mut out, &|k| empty.get(k).cloned()).expect("runs");
        assert_eq!(code, Verdict::Ask.exit_code());
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("ask"), "got {text}");
    }

    // -- Issue #769: hook-mode self-suppression on a duplicate install ------

    /// The standalone `zirv ctx safety check` hook must not re-evaluate a
    /// tool call the consolidated `zirv ctx hook pretool` entry already
    /// covers -- an un-migrated `~/.claude/settings.json` (an operator who
    /// has not re-run `zirv setup apply` since #769) still carries both. The
    /// suppression check runs BEFORE `read_stdin()`, so this needs no real
    /// process stdin to prove: `command` is empty (hook mode), and the
    /// consolidated entry being present on disk is reason enough for `run_
    /// check` to exit having read nothing and printed nothing, whatever a
    /// genuine invocation's stdin might have said.
    #[test]
    fn run_check_hook_mode_self_suppresses_when_the_consolidated_pretool_hook_is_also_installed() {
        let home = tempfile::tempdir().expect("home");
        let _home = super::super::testenv::HomeGuard::set(home.path());
        let settings_dir = home.path().join(".claude");
        std::fs::create_dir_all(&settings_dir).expect("mkdir");
        std::fs::write(
            settings_dir.join("settings.json"),
            serde_json::json!({
                "hooks": {
                    "PreToolUse": [{
                        "matcher": "Bash|PowerShell|Edit|Write|MultiEdit|NotebookEdit",
                        "hooks": [{"type": "command", "command": "zirv ctx hook pretool"}]
                    }]
                }
            })
            .to_string(),
        )
        .expect("write settings");

        let repo = tempfile::tempdir().expect("repo");
        let args = CheckArgs {
            repo: repo.path().to_path_buf(),
            mode: LaunchMode::Interactive,
            command: Vec::new(),
            agent: None,
        };
        let empty: HashMap<String, String> = HashMap::new();
        let mut out = Vec::new();
        let code = run_check(&args, &mut out, &|k| empty.get(k).cloned()).expect("runs");
        assert_eq!(code, 0);
        assert!(
            out.is_empty(),
            "a leftover standalone safety hook must stay silent once the consolidated \
             pretool hook also covers Bash|PowerShell: {out:?}"
        );
    }

    /// Sibling of the test above, in CLI mode: `-- <command>` must never
    /// self-suppress even when the consolidated hook is ALSO installed --
    /// only hook mode (empty `command`, reading a claude payload from stdin)
    /// is what a stale duplicate registration could ever invoke, and an
    /// operator running `zirv ctx safety check -- <command>` by hand always
    /// wants a real answer.
    #[test]
    fn run_check_cli_mode_never_self_suppresses_even_with_the_consolidated_hook_installed() {
        let home = tempfile::tempdir().expect("home");
        let _home = super::super::testenv::HomeGuard::set(home.path());
        let settings_dir = home.path().join(".claude");
        std::fs::create_dir_all(&settings_dir).expect("mkdir");
        std::fs::write(
            settings_dir.join("settings.json"),
            serde_json::json!({
                "hooks": {
                    "PreToolUse": [{
                        "matcher": "Bash|PowerShell|Edit|Write|MultiEdit|NotebookEdit",
                        "hooks": [{"type": "command", "command": "zirv ctx hook pretool"}]
                    }]
                }
            })
            .to_string(),
        )
        .expect("write settings");

        let repo = tempfile::tempdir().expect("repo");
        let args = CheckArgs {
            repo: repo.path().to_path_buf(),
            mode: LaunchMode::Interactive,
            command: vec!["rm".to_string(), "-rf".to_string(), "/".to_string()],
            agent: None,
        };
        let empty: HashMap<String, String> = HashMap::new();
        let mut out = Vec::new();
        let code = run_check(&args, &mut out, &|k| empty.get(k).cloned()).expect("runs");
        assert_eq!(code, Verdict::Ask.exit_code());
        assert!(!out.is_empty(), "CLI mode must never go silent");
    }

    #[test]
    fn the_hook_audits_a_policy_fingerprint_without_storing_the_raw_command() {
        let repo = tempfile::tempdir().expect("tempdir");
        let home = tempfile::tempdir().expect("tempdir");
        let state = tempfile::tempdir().expect("state");
        let _home = super::super::testenv::HomeGuard::set(home.path());
        let env = env_from(&[(
            super::super::state::STATE_ENV,
            state.path().to_str().expect("utf8 state"),
        )]);
        let cfg = CtxConfig::load(repo.path(), &|key| env.get(key).cloned()).expect("loads");
        let stdin = r#"{"session_id":"abc","tool_name":"Bash","tool_input":{"command":"echo secret-value-from-command"},"permission_mode":"default"}"#;
        let mut out = Vec::new();
        run_check_hook_mode_with_env(&cfg, &mut out, stdin, &|key| env.get(key).cloned())
            .expect("runs");

        let dir = state.path().join("logs/safety-decisions");
        let file = std::fs::read_dir(dir)
            .expect("audit dir")
            .next()
            .expect("one file")
            .expect("entry")
            .path();
        let text = std::fs::read_to_string(file).expect("audit");
        assert!(text.contains("\"policy_sha256\":"), "got {text}");
        assert!(text.contains("\"command_sha256\":"), "got {text}");
        assert!(!text.contains("secret-value-from-command"), "got {text}");
    }

    /// Change 5a: `safety_family` names a known dispatcher's subcommand
    /// (matching the Change 5 spec's own worked examples: `docker exec`,
    /// `glab mr`), but a program NOT on `SAFETY_FAMILY_DISPATCHER_PROGRAMS`
    /// -- `sudo`, and every plain Unix tool -- gets the program name alone,
    /// even when its first bare argument would otherwise look like a
    /// subcommand to `hook::command_family`.
    #[test]
    fn safety_family_only_names_a_subcommand_for_known_dispatchers() {
        assert_eq!(
            safety_family("docker exec db psql -c \"select 1\""),
            "docker exec"
        );
        assert_eq!(safety_family("glab mr merge 5"), "glab mr");
        assert_eq!(safety_family("git push origin main"), "git push");
        assert_eq!(safety_family("sudo rm -rf /"), "sudo");
        assert_eq!(
            safety_family("echo secret-value-from-command"),
            "echo",
            "a bare positional argument must never be reported as a subcommand"
        );
        assert_eq!(safety_family("rm -rf /tmp/data"), "rm");
        assert_eq!(safety_family(""), "");
    }

    #[test]
    fn list_reports_the_origin_of_every_rule() {
        let repo = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(repo.path().join(".zirv")).expect("mkdir");
        std::fs::write(
            repo.path().join(".zirv/ctx.toml"),
            "[safety]\ndeny = [\"terraform destroy*\"]\n",
        )
        .expect("write");
        let home = tempfile::tempdir().expect("tempdir");
        let _home = super::super::testenv::HomeGuard::set(home.path());
        let empty: HashMap<String, String> = HashMap::new();
        let args = ListArgs {
            repo: repo.path().to_path_buf(),
            json: false,
        };
        let mut out = Vec::new();
        let code = run_list(&args, &mut out, &|k| empty.get(k).cloned()).expect("runs");
        assert_eq!(code, 0);
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("terraform destroy*"));
        assert!(text.contains("repo .zirv/ctx.toml"));
        assert!(text.contains("built-in"));
    }

    /// The same rule means two different things now, so `explain` has to say
    /// which launch it is talking about (2026-08-24).
    #[test]
    fn explain_states_what_the_verdict_does_in_each_launch_mode() {
        let ask = Outcome {
            verdict: Verdict::Ask,
            matched: Some(Rule {
                pattern: "git push*--force*".to_string(),
                origin: Origin::BuiltIn,
            }),
        };
        let interactive = explain_text(
            "git push --force x",
            &ask,
            LaunchMode::Interactive,
            SnapshotDivergence::Unchanged,
            "not-present",
            None,
        );
        assert!(interactive.contains("built-in"), "got {interactive}");
        assert!(interactive.contains("prompts"), "got {interactive}");

        let headless = explain_text(
            "git push --force x",
            &ask,
            LaunchMode::Headless,
            SnapshotDivergence::Unchanged,
            "not-present",
            None,
        );
        assert!(headless.contains("fails closed"), "got {headless}");
        assert!(
            headless.contains("dontAsk"),
            "the headless consequence must name the mode that produces it: {headless}"
        );
    }

    /// Issue #139: when the launch snapshot is stricter than the current
    /// policy for this command, the explanation must say so explicitly --
    /// not silently describe the stricter verdict as if it were the
    /// configured posture.
    #[test]
    fn explain_names_the_snapshot_divergence_when_the_launch_snapshot_is_stricter() {
        let ask = Outcome {
            verdict: Verdict::Ask,
            matched: None,
        };
        let text = explain_text(
            "some-tool-zirv-has-never-heard-of",
            &ask,
            LaunchMode::Interactive,
            SnapshotDivergence::SnapshotStricter {
                current_verdict: Verdict::Allow,
            },
            "valid",
            None,
        );
        assert!(
            text.contains("launch snapshot"),
            "must name the snapshot as the source of the divergence: {text}"
        );
        assert!(
            text.contains("stricter"),
            "must say the snapshot is stricter, not just different: {text}"
        );
        assert!(
            text.contains("allow"),
            "must name what the current policy would actually do: {text}"
        );
        assert!(
            text.to_lowercase().contains("restart"),
            "must name the remedy (restart the session): {text}"
        );
    }

    /// The `Unchanged` case (the overwhelming majority: no attestation, or
    /// one whose snapshot agrees with today's policy) must never mention the
    /// snapshot at all -- the divergence note is additive, not a permanent
    /// fixture of every explanation.
    #[test]
    fn explain_says_nothing_about_the_snapshot_when_unchanged() {
        let ask = Outcome {
            verdict: Verdict::Ask,
            matched: None,
        };
        let text = explain_text(
            "some-tool-zirv-has-never-heard-of",
            &ask,
            LaunchMode::Interactive,
            SnapshotDivergence::Unchanged,
            "not-present",
            None,
        );
        assert!(
            !text.contains("launch snapshot"),
            "must not mention a snapshot that never diverged: {text}"
        );
    }

    /// Code review fix: `status: "self-healed"` must add an explicit note;
    /// every other status (`"not-present"`/`"valid"`) must add nothing, the
    /// same additive-only contract the divergence note already has.
    #[test]
    fn explain_text_names_a_self_healed_attestation_but_only_that_status() {
        let allow = Outcome {
            verdict: Verdict::Allow,
            matched: None,
        };
        let healed = explain_text(
            "cargo test",
            &allow,
            LaunchMode::Interactive,
            SnapshotDivergence::Unchanged,
            "self-healed",
            None,
        );
        assert!(healed.to_lowercase().contains("self-heal"), "got {healed}");

        for status in ["not-present", "valid"] {
            let text = explain_text(
                "cargo test",
                &allow,
                LaunchMode::Interactive,
                SnapshotDivergence::Unchanged,
                status,
                None,
            );
            assert!(
                !text.to_lowercase().contains("self-heal"),
                "status {status} must not mention self-heal: {text}"
            );
        }
    }

    /// An unmatched command explains the DIFFERENT default it hit per mode --
    /// the single most confusing thing about the new posture if it is not
    /// spelled out.
    #[test]
    fn explain_names_the_mode_specific_default_for_an_unmatched_command() {
        let repo = tempfile::tempdir().expect("tempdir");
        let home = tempfile::tempdir().expect("tempdir");
        let _home = super::super::testenv::HomeGuard::set(home.path());
        let empty: HashMap<String, String> = HashMap::new();
        for (mode, expected) in [
            (LaunchMode::Interactive, "allow"),
            (LaunchMode::Headless, "ask"),
        ] {
            let args = ExplainArgs {
                repo: repo.path().to_path_buf(),
                mode,
                command: vec!["some-unknown-tool".to_string(), "--flag".to_string()],
            };
            let mut out = Vec::new();
            run_explain(&args, &mut out, &|k| empty.get(k).cloned()).expect("runs");
            let text = String::from_utf8(out).unwrap();
            assert!(text.contains(expected), "{mode:?}: got {text}");
            assert!(
                text.contains("no deny, ask or allow rule matched"),
                "got {text}"
            );
        }
    }

    /// The SQL classifier's synthetic rule has to explain itself too, or an
    /// operator sees a verdict with a pattern they cannot find in
    /// `zirv ctx safety list`.
    #[test]
    fn explain_names_the_sql_classifier_when_it_is_what_decided() {
        let repo = tempfile::tempdir().expect("tempdir");
        let home = tempfile::tempdir().expect("tempdir");
        let _home = super::super::testenv::HomeGuard::set(home.path());
        let empty: HashMap<String, String> = HashMap::new();
        let args = ExplainArgs {
            repo: repo.path().to_path_buf(),
            mode: LaunchMode::Interactive,
            command: vec![
                "psql".to_string(),
                "-c".to_string(),
                "DROP TABLE users".to_string(),
            ],
        };
        let mut out = Vec::new();
        let code = run_explain(&args, &mut out, &|k| empty.get(k).cloned()).expect("runs");
        assert_eq!(code, Verdict::Ask.exit_code());
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("sql"), "got {text}");
        assert!(text.contains("prompts"), "got {text}");
    }

    /// Issue #102's suppression, re-scoped (2026-08-24). A hook `ask` under
    /// `dontAsk` is still an unsatisfiable prompt claude converts into a
    /// denial that would strip the operator's own `permissions.allow`, so the
    /// fall-through rule itself is unchanged. What changed is WHICH launches
    /// can reach it: zirv no longer pins `dontAsk` on an interactive launch,
    /// so the only two remaining populations are a headless zirv launch and
    /// an operator who pinned `dontAsk` in their own trailing flags. Pinned
    /// end to end against the argv the adapter actually builds, not a
    /// hand-written mode string.
    ///
    /// Issue #701 (2026-09-20): an interactive launch with no
    /// `chat.claude_permission_mode` configured now pins NO `--permission-
    /// mode` at all -- zirv stopped overriding the operator's own
    /// `permissions.defaultMode`, which a CLI flag outranks. The effective
    /// mode is then whatever claude itself resolves, whose own shipped
    /// default is `"default"`; that is what this test feeds `hook_output`
    /// for the interactive case, and the absence of the token is asserted
    /// directly rather than inferred.
    #[test]
    fn the_dont_ask_suppression_is_reachable_only_from_the_headless_posture() {
        use crate::commands::ctx::adapters::{AgentAdapter, claude::ClaudeAdapter};

        /// Claude Code's own shipped `permissions.defaultMode`, which applies
        /// whenever zirv pins nothing.
        const CLAUDE_OWN_DEFAULT: &str = "default";

        let adapter = ClaudeAdapter::new(None);
        let pinned_mode_of = |mode| -> Option<String> {
            let args =
                adapter.default_sandbox_args(&Default::default(), &Default::default(), &[], mode);
            let position = args.iter().position(|a| a == "--permission-mode")?;
            Some(args[position + 1].clone())
        };

        assert_eq!(
            pinned_mode_of(LaunchMode::Interactive),
            None,
            "an unconfigured interactive launch must pin no --permission-mode"
        );
        let mode_of = |mode| -> String {
            pinned_mode_of(mode).unwrap_or_else(|| CLAUDE_OWN_DEFAULT.to_string())
        };

        let ask = Outcome {
            verdict: Verdict::Ask,
            matched: Some(Rule {
                pattern: "git push*--force*".to_string(),
                origin: Origin::BuiltIn,
            }),
        };

        let emitted = hook_output(
            "git push --force x",
            &ask,
            &mode_of(LaunchMode::Interactive),
            SnapshotDivergence::Unchanged,
            "not-present",
        )
        .expect("an interactive launch must genuinely prompt");
        assert!(
            emitted.contains("\"permissionDecision\":\"ask\""),
            "got {emitted}"
        );

        assert!(
            hook_output(
                "git push --force x",
                &ask,
                &mode_of(LaunchMode::Headless),
                SnapshotDivergence::Unchanged,
                "not-present",
            )
            .is_none(),
            "a headless launch has nobody to prompt: the hook must fall through"
        );

        // The operator's own pin, unchanged: zirv never overrides an explicit
        // operator choice, so the suppression still applies there.
        assert!(
            hook_output(
                "git push --force x",
                &ask,
                "dontAsk",
                SnapshotDivergence::Unchanged,
                "not-present"
            )
            .is_none()
        );
    }

    /// Deny is unaffected by mode, in every posture.
    #[test]
    fn hook_output_deny_still_denies_in_every_permission_mode() {
        let deny = Outcome {
            verdict: Verdict::Deny,
            matched: Some(Rule {
                pattern: "sudo *".to_string(),
                origin: Origin::BuiltIn,
            }),
        };
        for mode in ["dontAsk", "default", ""] {
            let output = hook_output(
                "sudo rm -rf /",
                &deny,
                mode,
                SnapshotDivergence::Unchanged,
                "not-present",
            )
            .expect("deny still denies");
            assert!(
                output.contains("\"permissionDecision\":\"deny\""),
                "mode {mode}: got {output}"
            );
        }
    }

    #[test]
    fn explain_names_the_matched_rule_and_its_origin() {
        let repo = tempfile::tempdir().expect("tempdir");
        let home = tempfile::tempdir().expect("tempdir");
        let _home = super::super::testenv::HomeGuard::set(home.path());
        let empty: HashMap<String, String> = HashMap::new();
        let args = ExplainArgs {
            repo: repo.path().to_path_buf(),
            mode: LaunchMode::Interactive,
            // The built-in ask pattern catches force-pushes in any argument
            // position and still reports its built-in origin.
            command: vec![
                "git".to_string(),
                "push".to_string(),
                "--force".to_string(),
                "origin".to_string(),
                "main".to_string(),
            ],
        };
        let mut out = Vec::new();
        let code = run_explain(&args, &mut out, &|k| empty.get(k).cloned()).expect("runs");
        assert_eq!(code, Verdict::Ask.exit_code());
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("ask"));
        assert!(text.contains("built-in"));
    }
}
