//! Prompt extraction, restart flags, and launch prompt propagation.

/// Claude conversation pins to strip on restart; apply only to Claude,
/// because Codex uses `-c` for a value-bearing configuration flag. (#143)
pub(crate) const RESUME_FLAGS_WITH_VALUE: [&str; 2] = ["--session-id", "--resume"];

pub(crate) const RESUME_FLAGS_BARE: [&str; 3] = ["-c", "--continue", "--fork-session"];

/// Restrict Claude resume flags to the adapter that owns them.
fn adapter_has_resume_flags(adapter_name: &str) -> bool {
    adapter_name.eq_ignore_ascii_case("claude")
}

/// Detect existing conversation pins before adding another; conflicting ids
/// can make a launch fail before the operator sees it.
pub(crate) fn pins_an_existing_conversation(args: &[String], adapter_name: &str) -> bool {
    if !adapter_has_resume_flags(adapter_name) {
        return false;
    }
    args.iter().any(|arg| {
        RESUME_FLAGS_WITH_VALUE.contains(&arg.as_str())
            || RESUME_FLAGS_BARE.contains(&arg.as_str())
            || is_joined_form(arg, &RESUME_FLAGS_WITH_VALUE)
            || is_joined_form(arg, &RESUME_FLAGS_BARE)
    })
}

/// True for `--resume=abc` when `--resume` is in `flags`: the CLIs accept both
/// spellings, so stripping only the two-token form leaves the other behind.
fn is_joined_form(arg: &str, flags: &[&str]) -> bool {
    arg.split_once('=')
        .is_some_and(|(name, _)| flags.contains(&name))
}

/// Return an operator-supplied conversation pin verbatim, plus an explicit
/// id if present; bare continuation has no id to recover. (#778)
pub(super) fn resume_pin(command: &[String], adapter_name: &str) -> (Vec<String>, Option<String>) {
    if !adapter_has_resume_flags(adapter_name) {
        return (Vec::new(), None);
    }
    for (index, arg) in command.iter().enumerate() {
        if let Some((name, value)) = arg.split_once('=') {
            if RESUME_FLAGS_WITH_VALUE.contains(&name) {
                return (vec![arg.clone()], Some(value.to_string()));
            }
            if RESUME_FLAGS_BARE.contains(&name) {
                return (vec![arg.clone()], None);
            }
        }
        if RESUME_FLAGS_WITH_VALUE.contains(&arg.as_str()) {
            return match command.get(index + 1).filter(|next| !next.starts_with('-')) {
                Some(value) => (vec![arg.clone(), value.clone()], Some(value.clone())),
                None => (vec![arg.clone()], None),
            };
        }
        if RESUME_FLAGS_BARE.contains(&arg.as_str()) {
            return (vec![arg.clone()], None);
        }
    }
    (Vec::new(), None)
}

/// Locate the prompt by known value when available, since a prompt may start
/// with `-`; without it, prefer refusing a restart to guessing.
/// A bare `-p` consumes no following flag because the prompt comes from stdin.
pub(super) fn locate_prompt(
    command: &[String],
    prefix: usize,
    known: Option<&str>,
) -> Option<(usize, Option<String>)> {
    for (index, arg) in command.iter().enumerate().skip(prefix) {
        let is_prompt_flag = arg == "-p" || arg == "--print";
        let is_subcommand = arg == "exec";
        if !is_prompt_flag && !is_subcommand {
            continue;
        }
        let Some(next) = command.get(index + 1) else {
            return Some((index, None));
        };
        if Some(next.as_str()) == known {
            return Some((index, Some(next.clone())));
        }
        if next.starts_with('-') {
            return Some((index, None));
        }
        return Some((index, Some(next.clone())));
    }
    None
}

/// Finds the prompt in a headless agent command. Returns `None` rather than
/// guessing: a restart with the wrong prompt is worse than no restart.
pub fn extract_prompt(command: &[String]) -> Option<String> {
    locate_prompt(command, 1, None).and_then(|(_, prompt)| prompt)
}

/// Preserve operator flags across restart, removing only the regenerated
/// prompt, conversation pins, and launch prefix. Apply pin removal only to
/// adapters that recognize those flags. (#143)
pub fn extra_launch_flags(
    command: &[String],
    prefix: usize,
    known_prompt: Option<&str>,
    adapter_name: &str,
) -> Vec<String> {
    let recognizes_resume_flags = adapter_has_resume_flags(adapter_name);
    let located = locate_prompt(command, prefix, known_prompt);
    let prompt_at = located.as_ref().map(|(index, _)| *index);
    let prompt_takes_value = located.is_some_and(|(_, value)| value.is_some());

    let mut out = Vec::with_capacity(command.len());
    let mut skip_next = false;
    let mut in_prefix = true;
    for (index, arg) in command.iter().enumerate().skip(prefix) {
        if skip_next {
            skip_next = false;
            continue;
        }
        if Some(index) == prompt_at {
            skip_next = prompt_takes_value;
            in_prefix = false;
            continue;
        }
        if in_prefix && !arg.starts_with('-') {
            continue;
        }
        in_prefix = false;

        if recognizes_resume_flags {
            if is_joined_form(arg, &RESUME_FLAGS_WITH_VALUE)
                || is_joined_form(arg, &RESUME_FLAGS_BARE)
            {
                continue;
            }
            if RESUME_FLAGS_WITH_VALUE.contains(&arg.as_str()) {
                // A bare `--resume` with a flag after it takes no value, so
                // the next token belongs to the operator and has to survive.
                skip_next = command
                    .get(index + 1)
                    .is_some_and(|next| !next.starts_with('-'));
                continue;
            }
            if RESUME_FLAGS_BARE.contains(&arg.as_str()) {
                continue;
            }
        }
        out.push(arg.clone());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::super::tests::*;
    use super::super::*;
    use super::*;

    #[test]
    fn prompt_extraction_finds_the_dash_p_argument() {
        let cmd = vec![
            "claude".to_string(),
            "-p".to_string(),
            "fix the bug".to_string(),
            "--session-id".to_string(),
            "x".to_string(),
        ];
        assert_eq!(extract_prompt(&cmd), Some("fix the bug".to_string()));
    }

    #[test]
    fn prompt_extraction_handles_print_and_positional_forms() {
        assert_eq!(
            extract_prompt(&[
                "claude".to_string(),
                "--print".to_string(),
                "go".to_string()
            ]),
            Some("go".to_string())
        );
        assert_eq!(
            extract_prompt(&["codex".to_string(), "exec".to_string(), "go".to_string()]),
            Some("go".to_string())
        );
    }

    #[test]
    fn prompt_extraction_gives_up_rather_than_guessing() {
        assert_eq!(
            extract_prompt(&["claude".to_string(), "-p".to_string()]),
            None
        );
        assert_eq!(
            extract_prompt(&[
                "claude".to_string(),
                "--resume".to_string(),
                "abc".to_string()
            ]),
            None
        );
        assert_eq!(extract_prompt(&[]), None);
    }

    /// M8: only the prompt and `--session-id` (both regenerated fresh on
    /// every restart) are stripped; everything else the operator passed
    /// survives.
    #[test]
    fn extra_launch_flags_strips_only_the_prompt_and_session_id() {
        let cmd = vec![
            "claude".to_string(),
            "-p".to_string(),
            "fix the bug".to_string(),
            "--session-id".to_string(),
            "x".to_string(),
            "--model".to_string(),
            "opus".to_string(),
        ];
        assert_eq!(
            extra_launch_flags(&cmd, 1, None, "claude"),
            vec!["--model".to_string(), "opus".to_string()]
        );
    }

    #[test]
    fn extra_launch_flags_is_empty_when_the_command_is_only_prompt_and_session_id() {
        let cmd = vec![
            "claude".to_string(),
            "-p".to_string(),
            "fix the bug".to_string(),
            "--session-id".to_string(),
            "x".to_string(),
        ];
        assert!(extra_launch_flags(&cmd, 1, None, "claude").is_empty());
    }

    /// A markdown bullet list is an ordinary prompt. Reading it as a flag left
    /// the `-p` pair in the operator's flags, so every restart passed the
    /// prompt twice: once with the handoff, once without, and the second one
    /// won.
    #[test]
    fn a_prompt_that_starts_with_a_dash_is_still_stripped_from_the_restart_flags() {
        let prompt = "- fix the failing tests\n- then run cargo fmt";
        let cmd = vec![
            "claude".to_string(),
            "-p".to_string(),
            prompt.to_string(),
            "--model".to_string(),
            "opus".to_string(),
        ];
        assert_eq!(
            extra_launch_flags(&cmd, 1, Some(prompt), "claude"),
            vec!["--model".to_string(), "opus".to_string()],
            "the prompt zirv already holds is recognised by value, not by shape"
        );
    }

    /// Without the prompt to compare against, a value shaped like a flag still
    /// reads as one -- but only the flag is dropped, never the token after it,
    /// which belongs to the operator.
    #[test]
    fn a_bare_prompt_flag_drops_itself_and_keeps_what_follows() {
        let cmd = vec![
            "claude".to_string(),
            "-p".to_string(),
            "--verbose".to_string(),
        ];
        assert_eq!(
            extra_launch_flags(&cmd, 1, None, "claude"),
            vec!["--verbose".to_string()]
        );
    }

    /// `headless_cmd` rebuilds the program invocation on every relaunch, so a
    /// launcher in front of the agent (or a positional prompt) must not come
    /// back as a stray argument the agent reads as a second prompt.
    #[test]
    fn the_program_invocation_is_never_carried_into_the_restart_flags() {
        let via_npx = vec![
            "npx".to_string(),
            "claude".to_string(),
            "-p".to_string(),
            "task".to_string(),
        ];
        assert!(
            extra_launch_flags(&via_npx, 1, Some("task"), "claude").is_empty(),
            "the launcher's own argument is part of the invocation, not a flag"
        );

        // `agent_bin = "/usr/bin/env claude"`: the adapter reports a prefix of
        // two, because that is how many tokens it spends before the flags.
        let via_env = vec![
            "/usr/bin/env".to_string(),
            "claude".to_string(),
            "-p".to_string(),
            "task".to_string(),
        ];
        assert!(extra_launch_flags(&via_env, 2, Some("task"), "claude").is_empty());

        let positional = vec!["claude".to_string(), "task".to_string()];
        assert!(extra_launch_flags(&positional, 1, Some("task"), "claude").is_empty());
    }

    /// A restart exists to escape the conversation that rotted. Every spelling
    /// that would pin it back to that conversation has to go.
    #[test]
    fn nothing_that_pins_the_launch_to_the_dead_session_survives_a_restart() {
        let cmd = vec![
            "claude".to_string(),
            "-p".to_string(),
            "task".to_string(),
            "--session-id=OLD".to_string(),
            "--continue".to_string(),
            "--resume".to_string(),
            "abc".to_string(),
            "--fork-session".to_string(),
            "--model".to_string(),
            "opus".to_string(),
        ];
        assert_eq!(
            extra_launch_flags(&cmd, 1, Some("task"), "claude"),
            vec!["--model".to_string(), "opus".to_string()]
        );
    }

    /// D3: the shared predicate `chat::dash_orchestrator_pane` asks before
    /// appending a session pin of its own. Both spellings of every
    /// value-carrying flag, and every bare one.
    #[test]
    fn pins_an_existing_conversation_recognises_every_resume_spelling() {
        let yes = [
            vec!["claude", "--resume", "abc"],
            vec!["claude", "--resume=abc"],
            vec!["claude", "--session-id", "abc"],
            vec!["claude", "--session-id=abc"],
            vec!["claude", "-c"],
            vec!["claude", "--continue"],
            vec!["claude", "--fork-session"],
        ];
        for argv in yes {
            let owned: Vec<String> = argv.iter().map(|s| s.to_string()).collect();
            assert!(
                pins_an_existing_conversation(&owned, "claude"),
                "must be recognised as a pin: {argv:?}"
            );
        }

        let no = [
            vec!["claude", "--model", "opus"],
            vec!["claude", "-p", "resume the migration"],
            vec!["claude"],
        ];
        for argv in no {
            let owned: Vec<String> = argv.iter().map(|s| s.to_string()).collect();
            assert!(
                !pins_an_existing_conversation(&owned, "claude"),
                "must not be mistaken for a pin: {argv:?}"
            );
        }
    }

    /// Issue #143: codex's own `-c, --config <key>=<value>` must never be
    /// mistaken for claude's bare resume shorthand -- codex has no verified
    /// pin flag at all (it always mints its own session id), so nothing in
    /// its own argv can mean "this pins an existing conversation".
    #[test]
    fn pins_an_existing_conversation_is_always_false_for_an_adapter_with_no_resume_flags() {
        let argv = vec![
            "codex".to_string(),
            "-c".to_string(),
            "approval_policy=never".to_string(),
        ];
        assert!(!pins_an_existing_conversation(&argv, "codex"));
    }

    /// Issue #778: `resume_pin` is what `run_with_clock_inner`'s very first
    /// launch reads to honour an operator's own `-- --resume <id>` instead of
    /// silently minting an unrelated fresh session -- both the flag(s) to put
    /// back on `extra` and, when the flag names one directly, the id itself
    /// for zirv's own bookkeeping (`session_raw`). Both spellings of the
    /// value-carrying flags recover the id; the bare pins carry no id at all,
    /// same as `pins_an_existing_conversation` already treats them, but still
    /// return their own token so the launch still tells claude to resume.
    #[test]
    fn resume_pin_recovers_the_flag_and_the_id_when_one_is_named() {
        let with_id = [
            (
                vec!["--resume".to_string(), "abc".to_string()],
                vec!["--resume".to_string(), "abc".to_string()],
            ),
            (
                vec!["--resume=abc".to_string()],
                vec!["--resume=abc".to_string()],
            ),
            (
                vec!["--session-id".to_string(), "abc".to_string()],
                vec!["--session-id".to_string(), "abc".to_string()],
            ),
        ];
        for (command, expected_tokens) in with_id {
            assert_eq!(
                resume_pin(&command, "claude"),
                (expected_tokens, Some("abc".to_string())),
                "got a mismatch for {command:?}"
            );
        }

        for bare in [["-c"], ["--continue"], ["--fork-session"]] {
            let command = vec![bare[0].to_string()];
            assert_eq!(
                resume_pin(&command, "claude"),
                (vec![bare[0].to_string()], None),
                "a bare pin carries the token but no id: {bare:?}"
            );
        }

        assert_eq!(
            resume_pin(&["--model".to_string(), "opus".to_string()], "claude"),
            (Vec::new(), None),
            "no pinning flag at all means nothing to report"
        );
    }

    /// Codex mints its own session id and has no verified pin flag -- see
    /// `pins_an_existing_conversation_is_always_false_for_an_adapter_with_
    /// no_resume_flags`'s identical reasoning. `resume_pin` must be an
    /// equally total no-op for it, or a codex launch would start forwarding
    /// claude-shaped flags it never asked for.
    #[test]
    fn resume_pin_is_always_a_no_op_for_an_adapter_with_no_resume_flags() {
        let command = vec!["--resume".to_string(), "abc".to_string()];
        assert_eq!(resume_pin(&command, "codex"), (Vec::new(), None));
    }

    /// `--resume` with a flag after it took no value (same shape `a_valueless_
    /// resume_does_not_swallow_the_next_flag` already pins for `extra_launch_
    /// flags`): the next token belongs to the operator, so `resume_pin` must
    /// not report it as the id or swallow it into the returned tokens.
    #[test]
    fn resume_pin_does_not_swallow_the_next_flag_after_a_valueless_resume() {
        let command = vec![
            "--resume".to_string(),
            "--model".to_string(),
            "opus".to_string(),
        ];
        assert_eq!(
            resume_pin(&command, "claude"),
            (vec!["--resume".to_string()], None)
        );
    }

    /// `--resume` with a flag after it took no value, so swallowing the next
    /// token would eat one of the operator's own flags.
    #[test]
    fn a_valueless_resume_does_not_swallow_the_next_flag() {
        let cmd = vec![
            "claude".to_string(),
            "--resume".to_string(),
            "--model".to_string(),
            "opus".to_string(),
        ];
        assert_eq!(
            extra_launch_flags(&cmd, 1, None, "claude"),
            vec!["--model".to_string(), "opus".to_string()]
        );
    }

    #[test]
    fn extra_launch_flags_keeps_everything_when_there_is_no_prompt_or_session_id() {
        let cmd = vec![
            "codex".to_string(),
            "--model".to_string(),
            "gpt".to_string(),
        ];
        assert_eq!(
            extra_launch_flags(&cmd, 1, None, "codex"),
            vec!["--model".to_string(), "gpt".to_string()]
        );
    }

    /// Issue #143: codex's own `-c` (`-c, --config <key>=<value>`) collided
    /// with claude's bare `-c`/`--continue` resume shorthand. `RESUME_FLAGS_
    /// BARE` used to be matched regardless of adapter, so a bare `-c` was
    /// dropped as claude's resume flag -- which takes no value -- while the
    /// very next token (codex's own config VALUE, e.g.
    /// `approval_policy=never`) survived untouched, landing on argv detached
    /// from its own flag. Real codex-cli then rejects it outright: `error:
    /// unexpected argument 'approval_policy=never' found`.
    #[test]
    fn extra_launch_flags_keeps_codexs_own_c_flag_paired_with_its_value() {
        let cmd = vec![
            "--sandbox".to_string(),
            "workspace-write".to_string(),
            "-c".to_string(),
            "approval_policy=never".to_string(),
        ];
        assert_eq!(
            extra_launch_flags(&cmd, 0, None, "codex"),
            cmd,
            "codex's own -c/--config flag must never be mistaken for claude's bare resume \
             shorthand, which does not exist on this adapter at all"
        );
    }

    /// The same collision, the other direction: claude's own bare `-c` must
    /// still be recognised and stripped exactly as before -- this fix must
    /// not weaken the resume-flag guard for the adapter it actually protects.
    #[test]
    fn extra_launch_flags_still_strips_claudes_own_bare_c_flag() {
        let cmd = vec!["-c".to_string(), "--model".to_string(), "opus".to_string()];
        assert_eq!(
            extra_launch_flags(&cmd, 0, None, "claude"),
            vec!["--model".to_string(), "opus".to_string()]
        );
    }

    /// `FAKE_AGENT_MODE` applies to every invocation, so both the original child
    /// and the restarted one rot and the budget runs out.
    #[test]
    fn a_rotted_run_is_killed_restarted_and_capped() {
        let tmp = crate::commands::ctx::testenv::repo();
        let home = tmp.path().join("home");
        let state = tmp.path().join("state");
        let session = "33333333-2222-4333-8444-555555555555";
        let env = base_env(&state);

        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);
        unsafe {
            std::env::set_var("FAKE_AGENT_MODE", "rot");
            std::env::set_var("FAKE_AGENT_SLEEP", "30");
        }
        let args = ExecArgs {
            agent: Some("claude".to_string()),
            session_id: Some(session.to_string()),
            transcript: Some(transcript_for(&home, tmp.path(), session)),
            prompt: Some("do the work".to_string()),
            max_restarts: Some(1),
            budget_tokens: None,
            max_tool_calls: None,
            objective: None,
            timeout_secs: Some(60),
            simple: false,
            reservation_id: None,
            command: fake_agent_command(session),
            ..Default::default()
        };
        let mut out = Vec::new();
        let code = run_with(&args, &mut out, tmp.path(), &|k| env.get(k).cloned());
        unsafe {
            std::env::remove_var("FAKE_AGENT_MODE");
            std::env::remove_var("FAKE_AGENT_SLEEP");
        }

        assert_eq!(
            code.expect("runs"),
            EXIT_ROT_EXHAUSTED,
            "the caller applies its own policy after the budget is spent"
        );

        let log = std::fs::read_to_string(state.join("logs/decisions.jsonl")).expect("log");
        assert!(log.contains("\"verb\":\"exec\""), "got {log}");
        assert!(
            log.contains("\"action\":\"restart\""),
            "a restart was attempted: {log}"
        );
        assert!(
            log.contains("\"action\":\"give-up\""),
            "and then it stopped: {log}"
        );

        let handoffs = state.join("handoffs");
        let stored: Vec<_> = walk_md(&handoffs);
        assert!(
            !stored.is_empty(),
            "a handoff is written before each restart"
        );
    }

    fn walk_md(dir: &std::path::Path) -> Vec<PathBuf> {
        let mut found = Vec::new();
        let Ok(entries) = std::fs::read_dir(dir) else {
            return found;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                found.extend(walk_md(&path));
            } else if path.extension().and_then(|e| e.to_str()) == Some("md") {
                found.push(path);
            }
        }
        found
    }

    /// The restarted child is a new session writing to a new transcript, so
    /// supervision must follow it there. If the watcher kept polling the killed
    /// child's rotted file, this healthy second child would be killed too and
    /// the run would exit 75 instead of 0.
    #[test]
    fn a_restart_supervises_the_new_sessions_transcript() {
        let tmp = crate::commands::ctx::testenv::repo();
        let home = tmp.path().join("home");
        let session = "88888888-2222-4333-8444-555555555555";
        let env = base_env(&tmp.path().join("state"));

        // First child rots, second is healthy.
        let modes = tmp.path().join("modes.txt");
        std::fs::write(&modes, "rot\nhealthy\n").expect("write modes");

        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);
        unsafe {
            std::env::set_var("FAKE_AGENT_MODE_FILE", &modes);
            std::env::set_var("FAKE_AGENT_SLEEP", "30");
            std::env::set_var("FAKE_AGENT_TURNS", "12");
        }
        let args = ExecArgs {
            agent: Some("claude".to_string()),
            session_id: Some(session.to_string()),
            transcript: Some(transcript_for(&home, tmp.path(), session)),
            prompt: Some("do the work".to_string()),
            max_restarts: Some(1),
            budget_tokens: None,
            max_tool_calls: None,
            objective: None,
            timeout_secs: Some(60),
            simple: false,
            reservation_id: None,
            command: fake_agent_command(session),
            ..Default::default()
        };
        let mut out = Vec::new();
        let code = run_with(&args, &mut out, tmp.path(), &|k| env.get(k).cloned());
        unsafe {
            std::env::remove_var("FAKE_AGENT_MODE_FILE");
            std::env::remove_var("FAKE_AGENT_SLEEP");
            std::env::remove_var("FAKE_AGENT_TURNS");
        }

        assert_eq!(
            code.expect("runs"),
            0,
            "the healthy restarted child must be allowed to finish"
        );

        let found = transcripts_in(&home);
        assert_eq!(found.len(), 2, "one transcript per session: {found:?}");
        let first = transcript_for(&home, tmp.path(), session);
        assert!(
            found.contains(&first),
            "the original session's transcript: {found:?}"
        );
        assert!(
            found.iter().any(|p| *p != first),
            "the restarted session wrote its own transcript: {found:?}"
        );
    }

    #[test]
    fn a_run_with_no_discoverable_prompt_refuses_to_restart() {
        let tmp = crate::commands::ctx::testenv::repo();
        let home = tmp.path().join("home");
        let session = "44444444-2222-4333-8444-555555555555";
        let env = base_env(&tmp.path().join("state"));

        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);
        unsafe {
            std::env::set_var("FAKE_AGENT_MODE", "rot");
            // Keep the child alive past the first scoring tick so rot is seen.
            std::env::set_var("FAKE_AGENT_SLEEP", "30");
        }
        let mut command = fake_agent_command(session);
        command.retain(|a| a != "-p" && a != "do the work");
        let args = ExecArgs {
            agent: Some("claude".to_string()),
            session_id: Some(session.to_string()),
            transcript: Some(transcript_for(&home, tmp.path(), session)),
            prompt: None,
            max_restarts: Some(2),
            budget_tokens: None,
            max_tool_calls: None,
            objective: None,
            timeout_secs: Some(60),
            simple: false,
            reservation_id: None,
            command,
            ..Default::default()
        };
        let mut out = Vec::new();
        let code = run_with(&args, &mut out, tmp.path(), &|k| env.get(k).cloned());
        unsafe {
            std::env::remove_var("FAKE_AGENT_MODE");
            std::env::remove_var("FAKE_AGENT_SLEEP");
        }

        assert_eq!(
            code.expect("runs"),
            EXIT_ROT_EXHAUSTED,
            "rot was detected but no restart was possible"
        );
        let text = String::from_utf8(out).expect("utf8");
        assert!(
            text.contains("cannot restart"),
            "say why supervision stood down: {text}"
        );
    }

    /// The old warning about a missing prompt only ever surfaced once a
    /// restart was already needed (see `a_run_with_no_discoverable_prompt_
    /// refuses_to_restart` above), so a healthy run that never rots gave the
    /// operator no signal at all that restarts were a dead end for this
    /// invocation. It must appear upfront, regardless of whether the run
    /// ever actually needs to restart.
    #[test]
    fn an_upfront_warning_appears_even_when_the_run_never_needs_to_restart() {
        let tmp = crate::commands::ctx::testenv::repo();
        let home = tmp.path().join("home");
        let session = "eeeeeeee-2222-4333-8444-555555555555";
        let env = base_env(&tmp.path().join("state"));

        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);
        unsafe {
            std::env::set_var("FAKE_AGENT_MODE", "healthy");
        }
        let mut command = fake_agent_command(session);
        command.retain(|a| a != "-p" && a != "do the work");
        let args = ExecArgs {
            agent: Some("claude".to_string()),
            session_id: Some(session.to_string()),
            transcript: Some(transcript_for(&home, tmp.path(), session)),
            prompt: None,
            max_restarts: Some(2),
            budget_tokens: None,
            max_tool_calls: None,
            objective: None,
            timeout_secs: Some(60),
            simple: false,
            reservation_id: None,
            command,
            ..Default::default()
        };
        let mut out = Vec::new();
        let code = run_with(&args, &mut out, tmp.path(), &|k| env.get(k).cloned());
        unsafe {
            std::env::remove_var("FAKE_AGENT_MODE");
        }

        assert_eq!(
            code.expect("runs"),
            0,
            "a healthy run that never rots must still succeed"
        );
        let text = String::from_utf8(out).expect("utf8");
        assert!(
            text.contains("--prompt") && text.to_lowercase().contains("restart"),
            "an upfront warning must appear even though this run never needed to restart: {text}"
        );
    }

    /// Every restart is a new session, and the hook inside it reports whatever
    /// `ZIRV_CTX_SESSION` says. Leave that pinned to the dead session's id and
    /// the session check above rejects every signal the restart produces.
    #[test]
    fn a_restarted_child_is_told_its_own_session_id() {
        let tmp = crate::commands::ctx::testenv::repo();
        let home = tmp.path().join("home");
        let session = "cccccccc-2222-4333-8444-555555555555";
        let env = base_env(&tmp.path().join("state"));
        let seen = tmp.path().join("sessions.txt");

        let modes = tmp.path().join("modes.txt");
        std::fs::write(&modes, "rot\nhealthy\n").expect("write modes");

        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);
        unsafe {
            std::env::set_var("FAKE_AGENT_MODE_FILE", &modes);
            std::env::set_var("FAKE_AGENT_SESSION_ENV_LOG", &seen);
            std::env::set_var("FAKE_AGENT_SLEEP", "30");
            std::env::set_var("FAKE_AGENT_TURNS", "12");
        }
        let args = ExecArgs {
            agent: Some("claude".to_string()),
            session_id: Some(session.to_string()),
            transcript: Some(transcript_for(&home, tmp.path(), session)),
            prompt: Some("do the work".to_string()),
            max_restarts: Some(1),
            budget_tokens: None,
            max_tool_calls: None,
            objective: None,
            timeout_secs: Some(60),
            simple: false,
            reservation_id: None,
            command: fake_agent_command(session),
            ..Default::default()
        };
        let mut out = Vec::new();
        let code = run_with(&args, &mut out, tmp.path(), &|k| env.get(k).cloned());
        unsafe {
            std::env::remove_var("FAKE_AGENT_MODE_FILE");
            std::env::remove_var("FAKE_AGENT_SESSION_ENV_LOG");
            std::env::remove_var("FAKE_AGENT_SLEEP");
            std::env::remove_var("FAKE_AGENT_TURNS");
        }
        assert_eq!(code.expect("runs"), 0);

        let logged: Vec<String> = std::fs::read_to_string(&seen)
            .expect("the children recorded their session env")
            .lines()
            .map(str::to_string)
            .collect();
        assert_eq!(logged.len(), 2, "one line per child: {logged:?}");
        assert_eq!(logged[0], session, "the first child owns the given id");

        let first = transcript_for(&home, tmp.path(), session);
        let restarted = transcripts_in(&home)
            .into_iter()
            .find(|path| *path != first)
            .expect("the restarted child wrote its own transcript");
        let restarted_session = restarted
            .file_stem()
            .and_then(|s| s.to_str())
            .expect("session id from the transcript name");
        assert_eq!(
            logged[1], restarted_session,
            "the restart must export the new session id, not the dead one's"
        );
    }

    /// Bug B seam coverage (2026-08-22, fix round 3): `exec.rs` is one of
    /// the three seams that had only full-suite-green plus log inspection
    /// backing its own `policy_extra` wiring, not a dedicated exact-argv
    /// test -- exactly the shape of regression that would not fail any
    /// existing test if this seam silently lost its policy prefix. Asserts
    /// the real argv the launched child receives (`FAKE_AGENT_ARGV_LOG`),
    /// not merely that the run succeeded.
    #[test]
    fn the_initial_launch_carries_the_shipped_sandbox_posture() {
        let tmp = crate::commands::ctx::testenv::repo();
        let home = tmp.path().join("home");
        let state = tmp.path().join("state");
        let argv_log = tmp.path().join("argv.log");
        let session = "cccccccc-3333-4333-8444-555555555555";
        let mut env = base_env(&state);
        env.insert("ZIRV_CTX_PACE".to_string(), "false".to_string());

        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);
        unsafe {
            std::env::set_var("FAKE_AGENT_ARGV_LOG", &argv_log);
        }
        let args = ExecArgs {
            agent: Some("claude".to_string()),
            session_id: Some(session.to_string()),
            transcript: Some(transcript_for(&home, tmp.path(), session)),
            prompt: Some("do the work".to_string()),
            max_restarts: Some(0),
            budget_tokens: None,
            max_tool_calls: None,
            objective: None,
            timeout_secs: Some(60),
            simple: false,
            reservation_id: None,
            command: fake_agent_command(session),
            ..Default::default()
        };
        let mut out = Vec::new();
        let code = run_with(&args, &mut out, tmp.path(), &|k| env.get(k).cloned());
        unsafe {
            std::env::remove_var("FAKE_AGENT_ARGV_LOG");
        }
        assert_eq!(code.expect("runs"), 0);

        let argv = std::fs::read_to_string(&argv_log).expect("argv recorded");
        assert!(
            argv.contains("--permission-mode") && argv.contains("dontAsk"),
            "the shipped-default posture must reach the real launched argv: {argv}"
        );
        assert!(
            argv.contains("--allowedTools=") && argv.contains("Edit(./**)"),
            "the generated permission set must reach it too: {argv}"
        );
        assert!(
            argv.contains("--mcp-config=") && argv.contains("cccccccc"),
            "the supervised launch must register the bridge with its stable inbox: {argv}"
        );
    }

    #[test]
    fn a_restart_relaunches_with_the_system_prompt_too() {
        let tmp = crate::commands::ctx::testenv::repo();
        let home = tmp.path().join("home");
        let state = tmp.path().join("state");
        let argv_log = tmp.path().join("argv.log");
        let session = "cccccccc-2222-4333-8444-555555555555";
        let mut env = base_env(&state);
        env.insert("ZIRV_CTX_PACE".to_string(), "false".to_string());

        let modes = tmp.path().join("modes.txt");
        std::fs::write(&modes, "rot\nhealthy\n").expect("write modes");

        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);
        unsafe {
            std::env::set_var("FAKE_AGENT_MODE_FILE", &modes);
            std::env::set_var("FAKE_AGENT_SLEEP", "30");
            std::env::set_var("FAKE_AGENT_ARGV_LOG", &argv_log);
        }
        let args = ExecArgs {
            agent: Some("claude".to_string()),
            session_id: Some(session.to_string()),
            transcript: Some(transcript_for(&home, tmp.path(), session)),
            prompt: Some("do the work".to_string()),
            max_restarts: Some(1),
            budget_tokens: None,
            max_tool_calls: None,
            objective: None,
            timeout_secs: Some(60),
            simple: false,
            reservation_id: None,
            command: fake_agent_command(session),
            ..Default::default()
        };
        let mut out = Vec::new();
        let code = run_with(&args, &mut out, tmp.path(), &|k| env.get(k).cloned());
        unsafe {
            std::env::remove_var("FAKE_AGENT_MODE_FILE");
            std::env::remove_var("FAKE_AGENT_SLEEP");
            std::env::remove_var("FAKE_AGENT_ARGV_LOG");
        }
        assert_eq!(code.expect("runs"), 0);

        let argv = std::fs::read_to_string(&argv_log).expect("argv recorded");
        assert!(
            argv.contains("--append-system-prompt"),
            "the restarted child must carry the prompt too: {argv}"
        );
    }

    /// M8: a restart used to rebuild the headless command from scratch with
    /// only zirv's own added flags (the system prompt), silently dropping any
    /// extra flag the operator themselves had passed after `--`. Only lines
    /// carrying `--session-id` are real agent invocations (a `--help` probe,
    /// if any ran, never gets one), so filtering on it keeps this assertion
    /// meaningful regardless of what else shares the log.
    #[test]
    fn a_restart_preserves_the_users_own_extra_flags_not_just_zirvs() {
        let tmp = crate::commands::ctx::testenv::repo();
        let home = tmp.path().join("home");
        let state = tmp.path().join("state");
        let argv_log = tmp.path().join("argv.log");
        let session = "12121212-2222-4333-8444-555555555555";
        let mut env = base_env(&state);
        env.insert("ZIRV_CTX_PACE".to_string(), "false".to_string());

        let modes = tmp.path().join("modes.txt");
        std::fs::write(&modes, "rot\nhealthy\n").expect("write modes");

        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);
        unsafe {
            std::env::set_var("FAKE_AGENT_MODE_FILE", &modes);
            std::env::set_var("FAKE_AGENT_SLEEP", "30");
            std::env::set_var("FAKE_AGENT_ARGV_LOG", &argv_log);
        }
        let mut command = fake_agent_command(session);
        command.push("--zzz-custom-flag".to_string());
        command.push("custom-value".to_string());
        let args = ExecArgs {
            agent: Some("claude".to_string()),
            session_id: Some(session.to_string()),
            transcript: Some(transcript_for(&home, tmp.path(), session)),
            prompt: Some("do the work".to_string()),
            max_restarts: Some(1),
            budget_tokens: None,
            max_tool_calls: None,
            objective: None,
            timeout_secs: Some(60),
            simple: false,
            reservation_id: None,
            command,
            ..Default::default()
        };
        let mut out = Vec::new();
        let code = run_with(&args, &mut out, tmp.path(), &|k| env.get(k).cloned());
        unsafe {
            std::env::remove_var("FAKE_AGENT_MODE_FILE");
            std::env::remove_var("FAKE_AGENT_SLEEP");
            std::env::remove_var("FAKE_AGENT_ARGV_LOG");
        }
        assert_eq!(code.expect("runs"), 0);

        let argv = std::fs::read_to_string(&argv_log).expect("argv recorded");
        let invocations: Vec<&str> = argv
            .lines()
            .filter(|line| line.contains("--session-id"))
            .collect();
        assert_eq!(
            invocations.len(),
            2,
            "one real invocation per child: {argv:?}"
        );
        for line in &invocations {
            assert!(
                line.contains("--zzz-custom-flag") && line.contains("custom-value"),
                "the user's own extra flag must survive every restart, not just the first spawn: {argv}"
            );
        }
    }

    /// M2: README promises that "whether a prompt was injected, and from
    /// which layers, is recorded in the decision log at every session
    /// start". A restart mints a new session id, so its own attribution
    /// entry must be logged under that id too, not only the first session's.
    #[test]
    fn injection_is_logged_again_for_each_restarts_own_session_id() {
        let tmp = crate::commands::ctx::testenv::repo();
        let home = tmp.path().join("home");
        let state = tmp.path().join("state");
        let session = "ffffffff-2222-4333-8444-555555555555";
        let mut env = base_env(&state);
        env.insert("ZIRV_CTX_PACE".to_string(), "false".to_string());

        let modes = tmp.path().join("modes.txt");
        std::fs::write(&modes, "rot\nhealthy\n").expect("write modes");

        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);
        unsafe {
            std::env::set_var("FAKE_AGENT_MODE_FILE", &modes);
            std::env::set_var("FAKE_AGENT_SLEEP", "30");
        }
        let args = ExecArgs {
            agent: Some("claude".to_string()),
            session_id: Some(session.to_string()),
            transcript: Some(transcript_for(&home, tmp.path(), session)),
            prompt: Some("do the work".to_string()),
            max_restarts: Some(1),
            budget_tokens: None,
            max_tool_calls: None,
            objective: None,
            timeout_secs: Some(60),
            simple: false,
            reservation_id: None,
            command: fake_agent_command(session),
            ..Default::default()
        };
        let mut out = Vec::new();
        let code = run_with(&args, &mut out, tmp.path(), &|k| env.get(k).cloned());
        unsafe {
            std::env::remove_var("FAKE_AGENT_MODE_FILE");
            std::env::remove_var("FAKE_AGENT_SLEEP");
        }
        assert_eq!(code.expect("runs"), 0);

        let log = std::fs::read_to_string(state.join("logs/decisions.jsonl")).expect("log");
        let injected_sessions: Vec<&str> = log
            .lines()
            .filter(|l| l.contains("\"action\":\"prompt-injected\""))
            .filter_map(|l| {
                let key = "\"session\":\"";
                let start = l.find(key)? + key.len();
                let end = l[start..].find('"')? + start;
                Some(&l[start..end])
            })
            .collect();
        assert_eq!(
            injected_sessions.len(),
            2,
            "one attribution entry per actual session id, including the restart: {log}"
        );
        assert_ne!(
            injected_sessions[0], injected_sessions[1],
            "the restart mints a new session id and must be logged under it: {log}"
        );
    }

    /// T7: unread mail addressed to this session's agent is folded into the
    /// composed system prompt at launch, the same way the repo layer is.
    #[test]
    fn unread_mail_is_delivered_into_the_launch_system_prompt() {
        let tmp = crate::commands::ctx::testenv::repo();
        let home = tmp.path().join("home");
        let state_dir = tmp.path().join("state");
        let argv_log = tmp.path().join("argv.log");
        let session = "abababab-2222-4333-8444-555555555555";
        let mut env = base_env(&state_dir);
        env.insert("ZIRV_CTX_PACE".to_string(), "false".to_string());

        let state = crate::commands::ctx::state::StateDir::from_root(state_dir.clone());
        let slug = crate::commands::ctx::state::repo_slug(tmp.path());
        crate::commands::ctx::mail::store(
            &state,
            &slug,
            &crate::commands::ctx::mail::Message {
                from_session: "other-session".to_string(),
                from_agent: "claude".to_string(),
                to: "any".to_string(),
                to_session: None,
                sent: 1,
                body: "heads up: the webhook route moved".to_string(),
            },
            &CtxConfig::default(),
        )
        .expect("store mail");

        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);
        unsafe {
            std::env::set_var("FAKE_AGENT_MODE", "healthy");
            std::env::set_var("FAKE_AGENT_ARGV_LOG", &argv_log);
        }
        let args = ExecArgs {
            agent: Some("claude".to_string()),
            session_id: Some(session.to_string()),
            transcript: Some(transcript_for(&home, tmp.path(), session)),
            prompt: Some("do the work".to_string()),
            max_restarts: Some(0),
            budget_tokens: None,
            max_tool_calls: None,
            objective: None,
            timeout_secs: Some(60),
            simple: false,
            reservation_id: None,
            command: fake_agent_command(session),
            ..Default::default()
        };
        let mut out = Vec::new();
        let code = run_with(&args, &mut out, tmp.path(), &|k| env.get(k).cloned());
        unsafe {
            std::env::remove_var("FAKE_AGENT_MODE");
            std::env::remove_var("FAKE_AGENT_ARGV_LOG");
        }
        assert_eq!(code.expect("runs"), 0);

        let argv = std::fs::read_to_string(&argv_log).expect("argv recorded");
        assert!(
            argv.contains("heads up: the webhook route moved"),
            "the mail must reach the launch's composed prompt: {argv}"
        );
        assert!(
            argv.contains("another agent session"),
            "labeled as mail, not as an operator instruction: {argv}"
        );
    }

    /// codex has no system-prompt injection mechanism at all
    /// (`capabilities().system_prompt == false`), so `injection_args_for_
    /// session` always returns an empty argv for it -- folding mail into
    /// `composed` the way `unread_mail_is_delivered_into_the_launch_system_
    /// prompt` proves for claude would silently destroy the message for
    /// codex. `task_prompt_with_mail_fallback` is what rescues it: the mail
    /// block lands on the task prompt text itself instead, which is always
    /// delivered (here, as the `exec` positional argv token; on a Windows
    /// shim launch it would be stdin instead, same mechanism). Mail is
    /// consumed only because it was genuinely delivered this way -- the same
    /// Item 3 discipline the claude path already follows.
    #[test]
    fn a_codex_worker_receives_mail_in_its_task_prompt_since_it_cannot_be_injected() {
        let tmp = crate::commands::ctx::testenv::repo();
        let home = tmp.path().join("home");
        let state_dir = tmp.path().join("state");
        let argv_log = tmp.path().join("argv.log");
        let mut env = base_env(&state_dir);
        env.insert("ZIRV_CTX_PACE".to_string(), "false".to_string());
        env.insert(
            "ZIRV_CTX_AGENT_BIN".to_string(),
            format!("sh {}", fixture("fake-codex-agent.sh").display()),
        );

        let state = crate::commands::ctx::state::StateDir::from_root(state_dir.clone());
        let slug = crate::commands::ctx::state::repo_slug(tmp.path());
        crate::commands::ctx::mail::store(
            &state,
            &slug,
            &crate::commands::ctx::mail::Message {
                from_session: "other-session".to_string(),
                from_agent: "claude".to_string(),
                to: "any".to_string(),
                to_session: None,
                sent: 1,
                body: "heads up: the webhook route moved".to_string(),
            },
            &CtxConfig::default(),
        )
        .expect("store mail");

        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);
        unsafe {
            std::env::set_var("FAKE_AGENT_ARGV_LOG", &argv_log);
        }
        let args = ExecArgs {
            agent: Some("codex".to_string()),
            session_id: None,
            transcript: None,
            prompt: Some("do the work".to_string()),
            max_restarts: Some(0),
            budget_tokens: None,
            max_tool_calls: None,
            objective: None,
            timeout_secs: Some(60),
            simple: false,
            reservation_id: None,
            // No trailing command: zirv builds the launch itself
            // (`adapter_builds_launch`), which is the shape both
            // `zirv ctx agent codex <prompt>` and a bare `zirv ctx exec
            // --agent codex --prompt <text>` produce, and the one shape
            // `task_prompt_with_mail_fallback` can actually append to. See
            // `explicit_command_mail_is_left_untouched_for_an_uninjectable_
            // adapter` below for the other shape (an explicit `-- <command>`),
            // where there is no such text to append to at all.
            command: Vec::new(),
            ..Default::default()
        };
        let mut out = Vec::new();
        let code = run_with(&args, &mut out, tmp.path(), &|k| env.get(k).cloned());
        unsafe {
            std::env::remove_var("FAKE_AGENT_ARGV_LOG");
        }
        assert_eq!(code.expect("runs"), 0);

        let argv = std::fs::read_to_string(&argv_log).expect("argv recorded");
        assert!(
            argv.contains("heads up: the webhook route moved"),
            "the mail must reach codex's task prompt text: {argv}"
        );
        assert!(
            argv.contains("another agent session"),
            "still labeled as mail, not as an operator instruction: {argv}"
        );
        let task_at = argv.find("do the work").expect("the task prompt itself");
        let mail_at = argv
            .find("heads up: the webhook route moved")
            .expect("checked above");
        assert!(
            task_at < mail_at,
            "the mail must be appended after the operator's own task prompt, not before it: {argv}"
        );

        let unread = crate::commands::ctx::mail::list(&state, &slug, None, None).expect("list");
        assert!(
            unread.is_empty(),
            "mail actually delivered into the task prompt must be consumed: {unread:?}"
        );
    }

    /// Item 14: `--simple` (`skip_injection`) makes `composed` always `None`,
    /// for either adapter -- but codex's real mail channel, the task-prompt
    /// text `task_prompt_with_mail_fallback` appends to, has nothing to do
    /// with `composed` at all. Before this fix, gating mail listing on
    /// `composed.is_some()` withheld mail from codex under `--simple` for a
    /// reason that only ever applied to claude.
    #[test]
    fn simple_mode_does_not_withhold_mail_from_codex() {
        let tmp = crate::commands::ctx::testenv::repo();
        let home = tmp.path().join("home");
        let state_dir = tmp.path().join("state");
        let argv_log = tmp.path().join("argv.log");
        let mut env = base_env(&state_dir);
        env.insert("ZIRV_CTX_PACE".to_string(), "false".to_string());
        env.insert(
            "ZIRV_CTX_AGENT_BIN".to_string(),
            format!("sh {}", fixture("fake-codex-agent.sh").display()),
        );

        let state = crate::commands::ctx::state::StateDir::from_root(state_dir.clone());
        let slug = crate::commands::ctx::state::repo_slug(tmp.path());
        crate::commands::ctx::mail::store(
            &state,
            &slug,
            &crate::commands::ctx::mail::Message {
                from_session: "other-session".to_string(),
                from_agent: "claude".to_string(),
                to: "any".to_string(),
                to_session: None,
                sent: 1,
                body: "heads up: the webhook route moved".to_string(),
            },
            &CtxConfig::default(),
        )
        .expect("store mail");

        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);
        unsafe {
            std::env::set_var("FAKE_AGENT_ARGV_LOG", &argv_log);
        }
        let args = ExecArgs {
            agent: Some("codex".to_string()),
            session_id: None,
            transcript: None,
            prompt: Some("do the work".to_string()),
            max_restarts: Some(0),
            budget_tokens: None,
            max_tool_calls: None,
            objective: None,
            timeout_secs: Some(60),
            simple: true,
            reservation_id: None,
            command: Vec::new(),
            ..Default::default()
        };
        let mut out = Vec::new();
        let code = run_with(&args, &mut out, tmp.path(), &|k| env.get(k).cloned());
        unsafe {
            std::env::remove_var("FAKE_AGENT_ARGV_LOG");
        }
        assert_eq!(code.expect("runs"), 0);

        let argv = std::fs::read_to_string(&argv_log).expect("argv recorded");
        assert!(
            argv.contains("heads up: the webhook route moved"),
            "--simple must not withhold mail from an adapter whose channel does not need \
             composed: {argv}"
        );

        let unread = crate::commands::ctx::mail::list(&state, &slug, None, None).expect("list");
        assert!(
            unread.is_empty(),
            "mail actually delivered into the task prompt must be consumed: {unread:?}"
        );
    }

    /// Direct codex launches support `developer_instructions`, including the
    /// explicit `-- <command>` shape. Zirv therefore delivers and consumes
    /// mail through the same configuration override without rewriting the
    /// caller's task prompt.
    #[test]
    fn explicit_command_mail_uses_codex_developer_instructions() {
        let tmp = crate::commands::ctx::testenv::repo();
        let home = tmp.path().join("home");
        let state_dir = tmp.path().join("state");
        let argv_log = tmp.path().join("argv.log");
        let mut env = base_env(&state_dir);
        env.insert("ZIRV_CTX_PACE".to_string(), "false".to_string());

        let state = crate::commands::ctx::state::StateDir::from_root(state_dir.clone());
        let slug = crate::commands::ctx::state::repo_slug(tmp.path());
        crate::commands::ctx::mail::store(
            &state,
            &slug,
            &crate::commands::ctx::mail::Message {
                from_session: "other-session".to_string(),
                from_agent: "claude".to_string(),
                to: "any".to_string(),
                to_session: None,
                sent: 1,
                body: "heads up: the webhook route moved".to_string(),
            },
            &CtxConfig::default(),
        )
        .expect("store mail");

        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);
        unsafe {
            std::env::set_var("FAKE_AGENT_ARGV_LOG", &argv_log);
        }
        let args = ExecArgs {
            agent: Some("codex".to_string()),
            session_id: None,
            transcript: None,
            // Extracted from the explicit command below (`locate_prompt`
            // recognises codex's own `exec <prompt>` shape), the same way a
            // hand-typed `zirv ctx exec --agent codex -- codex exec "..."`
            // would resolve it.
            prompt: None,
            max_restarts: Some(0),
            budget_tokens: None,
            max_tool_calls: None,
            objective: None,
            timeout_secs: Some(60),
            simple: false,
            reservation_id: None,
            command: vec![
                "sh".to_string(),
                fixture("fake-codex-agent.sh").display().to_string(),
                "exec".to_string(),
                "do the work".to_string(),
            ],
            ..Default::default()
        };
        let mut out = Vec::new();
        let code = run_with(&args, &mut out, tmp.path(), &|k| env.get(k).cloned());
        unsafe {
            std::env::remove_var("FAKE_AGENT_ARGV_LOG");
        }
        assert_eq!(code.expect("runs"), 0);

        let argv = std::fs::read_to_string(&argv_log).expect("argv recorded");
        assert!(
            argv.contains("-c developer_instructions=")
                && argv.contains("heads up: the webhook route moved"),
            "direct codex must receive mail through developer instructions: {argv}"
        );

        let unread = crate::commands::ctx::mail::list(&state, &slug, None, None).expect("list");
        assert!(
            unread.is_empty(),
            "mail delivered through developer instructions must be consumed: {unread:?}"
        );
    }

    /// B3: `mail.enabled = false` must gate delivery at every seam that folds
    /// mail into a composed prompt, not just `send`/`inbox`.
    #[test]
    fn disabled_mail_is_not_delivered_into_a_headless_prompt() {
        let tmp = crate::commands::ctx::testenv::repo();
        let home = tmp.path().join("home");
        let state_dir = tmp.path().join("state");
        let argv_log = tmp.path().join("argv.log");
        let session = "eeeeeeee-2222-4333-8444-555555555555";
        let mut env = base_env(&state_dir);
        env.insert("ZIRV_CTX_PACE".to_string(), "false".to_string());
        env.insert("ZIRV_CTX_MAIL".to_string(), "false".to_string());

        let state = crate::commands::ctx::state::StateDir::from_root(state_dir.clone());
        let slug = crate::commands::ctx::state::repo_slug(tmp.path());
        crate::commands::ctx::mail::store(
            &state,
            &slug,
            &crate::commands::ctx::mail::Message {
                from_session: "other-session".to_string(),
                from_agent: "claude".to_string(),
                to: "any".to_string(),
                to_session: None,
                sent: 1,
                body: "heads up: the webhook route moved".to_string(),
            },
            &CtxConfig::default(),
        )
        .expect("store mail");

        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);
        unsafe {
            std::env::set_var("FAKE_AGENT_MODE", "healthy");
            std::env::set_var("FAKE_AGENT_ARGV_LOG", &argv_log);
        }
        let args = ExecArgs {
            agent: Some("claude".to_string()),
            session_id: Some(session.to_string()),
            transcript: Some(transcript_for(&home, tmp.path(), session)),
            prompt: Some("do the work".to_string()),
            max_restarts: Some(0),
            budget_tokens: None,
            max_tool_calls: None,
            objective: None,
            timeout_secs: Some(60),
            simple: false,
            reservation_id: None,
            command: fake_agent_command(session),
            ..Default::default()
        };
        let mut out = Vec::new();
        let code = run_with(&args, &mut out, tmp.path(), &|k| env.get(k).cloned());
        unsafe {
            std::env::remove_var("FAKE_AGENT_MODE");
            std::env::remove_var("FAKE_AGENT_ARGV_LOG");
        }
        assert_eq!(code.expect("runs"), 0);

        let argv = std::fs::read_to_string(&argv_log).expect("argv recorded");
        assert!(
            !argv.contains("heads up: the webhook route moved"),
            "mail.enabled = false must gate delivery, not just send/inbox: {argv}"
        );

        let unread = crate::commands::ctx::mail::list(&state, &slug, None, None).expect("list");
        assert_eq!(
            unread.len(),
            1,
            "a delivery that never happened must not consume the message either"
        );
    }

    /// S3: mail delivered into a launch prompt is consumed right after, so a
    /// later launch does not redeliver it.
    #[test]
    fn delivered_mail_is_not_delivered_a_second_time() {
        let tmp = crate::commands::ctx::testenv::repo();
        let home = tmp.path().join("home");
        let state_dir = tmp.path().join("state");
        let mut env = base_env(&state_dir);
        env.insert("ZIRV_CTX_PACE".to_string(), "false".to_string());

        let state = crate::commands::ctx::state::StateDir::from_root(state_dir.clone());
        let slug = crate::commands::ctx::state::repo_slug(tmp.path());
        crate::commands::ctx::mail::store(
            &state,
            &slug,
            &crate::commands::ctx::mail::Message {
                from_session: "other-session".to_string(),
                from_agent: "claude".to_string(),
                to: "any".to_string(),
                to_session: None,
                sent: 1,
                body: "heads up: the webhook route moved".to_string(),
            },
            &CtxConfig::default(),
        )
        .expect("store mail");

        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);

        let session1 = "abababab-2222-4333-8444-555555555555";
        let argv_log1 = tmp.path().join("argv1.log");
        // NEW-1: a guard. Three panicking statements sit between the old
        // set and its restore, so any of them leaked `FAKE_AGENT_*` into
        // every later test in this process.
        let _fake_agent = crate::commands::ctx::testenv::VarGuard::set(&[
            ("FAKE_AGENT_MODE", Some("healthy")),
            ("FAKE_AGENT_ARGV_LOG", argv_log1.to_str()),
        ]);
        let args1 = ExecArgs {
            agent: Some("claude".to_string()),
            session_id: Some(session1.to_string()),
            transcript: Some(transcript_for(&home, tmp.path(), session1)),
            prompt: Some("do the work".to_string()),
            max_restarts: Some(0),
            budget_tokens: None,
            max_tool_calls: None,
            objective: None,
            timeout_secs: Some(60),
            simple: false,
            reservation_id: None,
            command: fake_agent_command(session1),
            ..Default::default()
        };
        let mut out1 = Vec::new();
        let code1 = run_with(&args1, &mut out1, tmp.path(), &|k| env.get(k).cloned());
        assert_eq!(code1.expect("first launch runs"), 0);
        let argv1 = std::fs::read_to_string(&argv_log1).expect("argv recorded");
        assert!(
            argv1.contains("heads up: the webhook route moved"),
            "the first launch must see the mail: {argv1}"
        );

        let session2 = "cdcdcdcd-2222-4333-8444-555555555555";
        let argv_log2 = tmp.path().join("argv2.log");
        // Nested guard: restores to `argv_log1` on drop, and the outer guard
        // then restores whatever the process had before the test.
        let _second_argv_log = crate::commands::ctx::testenv::VarGuard::set(&[(
            "FAKE_AGENT_ARGV_LOG",
            argv_log2.to_str(),
        )]);
        let args2 = ExecArgs {
            agent: Some("claude".to_string()),
            session_id: Some(session2.to_string()),
            transcript: Some(transcript_for(&home, tmp.path(), session2)),
            prompt: Some("do more work".to_string()),
            max_restarts: Some(0),
            budget_tokens: None,
            max_tool_calls: None,
            objective: None,
            timeout_secs: Some(60),
            simple: false,
            reservation_id: None,
            command: fake_agent_command(session2),
            ..Default::default()
        };
        let mut out2 = Vec::new();
        let code2 = run_with(&args2, &mut out2, tmp.path(), &|k| env.get(k).cloned());
        assert_eq!(code2.expect("second launch runs"), 0);
        let argv2 = std::fs::read_to_string(&argv_log2).expect("argv recorded");
        assert!(
            !argv2.contains("heads up: the webhook route moved"),
            "the mail was already delivered once and must not be redelivered: {argv2}"
        );
    }

    /// Issue #30, item 3: mail consumed on a session's behalf -- here, an
    /// exec cycle folding it into its own launch prompt, never in answer to
    /// that session's own explicit `zirv ctx inbox` -- must leave a
    /// decision-log trail naming the mail file and who claimed it.
    #[test]
    fn consuming_mail_into_the_launch_prompt_is_logged() {
        let tmp = crate::commands::ctx::testenv::repo();
        let home = tmp.path().join("home");
        let state_dir = tmp.path().join("state");
        let mut env = base_env(&state_dir);
        env.insert("ZIRV_CTX_PACE".to_string(), "false".to_string());

        let state = crate::commands::ctx::state::StateDir::from_root(state_dir.clone());
        let slug = crate::commands::ctx::state::repo_slug(tmp.path());
        let path = crate::commands::ctx::mail::store(
            &state,
            &slug,
            &crate::commands::ctx::mail::Message {
                from_session: "other-session".to_string(),
                from_agent: "claude".to_string(),
                to: "any".to_string(),
                to_session: None,
                sent: 1,
                body: "heads up: the webhook route moved".to_string(),
            },
            &CtxConfig::default(),
        )
        .expect("store mail");
        let file_id = path
            .file_name()
            .and_then(|n| n.to_str())
            .expect("utf8")
            .to_string();

        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);
        let session = "10111011-2222-4333-8444-555555555555";
        let argv_log = tmp.path().join("argv.log");
        let _fake_agent = crate::commands::ctx::testenv::VarGuard::set(&[
            ("FAKE_AGENT_MODE", Some("healthy")),
            ("FAKE_AGENT_ARGV_LOG", argv_log.to_str()),
        ]);
        let args = ExecArgs {
            agent: Some("claude".to_string()),
            session_id: Some(session.to_string()),
            transcript: Some(transcript_for(&home, tmp.path(), session)),
            prompt: Some("do the work".to_string()),
            max_restarts: Some(0),
            budget_tokens: None,
            max_tool_calls: None,
            objective: None,
            timeout_secs: Some(60),
            simple: false,
            reservation_id: None,
            command: fake_agent_command(session),
            ..Default::default()
        };
        let mut out = Vec::new();
        let code = run_with(&args, &mut out, tmp.path(), &|k| env.get(k).cloned());
        assert_eq!(code.expect("runs"), 0);

        let log = std::fs::read_to_string(state_dir.join("logs/decisions.jsonl")).expect("log");
        assert!(log.contains("\"action\":\"mail-consumed\""), "got {log}");
        assert!(
            log.contains(&file_id),
            "the entry names the mail file: {log}"
        );
    }

    /// Item 3 (regression): a launch that never actually spawns must not
    /// consume the mail it would have delivered -- no session ever saw it,
    /// so it must stay unread for whichever later invocation actually gets
    /// one running. The old ordering consumed mail immediately after
    /// composing the prompt, well before `spawn_tapped` (and the pacing
    /// gate ahead of it) ever ran.
    #[test]
    fn mail_is_not_consumed_when_the_launch_fails_before_spawning() {
        let tmp = crate::commands::ctx::testenv::repo();
        let home = tmp.path().join("home");
        let state_dir = tmp.path().join("state");
        let mut env = base_env(&state_dir);
        env.insert("ZIRV_CTX_PACE".to_string(), "false".to_string());

        let state = crate::commands::ctx::state::StateDir::from_root(state_dir.clone());
        let slug = crate::commands::ctx::state::repo_slug(tmp.path());
        crate::commands::ctx::mail::store(
            &state,
            &slug,
            &crate::commands::ctx::mail::Message {
                from_session: "other-session".to_string(),
                from_agent: "claude".to_string(),
                to: "any".to_string(),
                to_session: None,
                sent: 1,
                body: "must stay unread".to_string(),
            },
            &CtxConfig::default(),
        )
        .expect("store mail");

        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);

        let session = "12312312-2222-4333-8444-555555555555";
        let args = ExecArgs {
            agent: Some("claude".to_string()),
            session_id: Some(session.to_string()),
            transcript: Some(transcript_for(&home, tmp.path(), session)),
            prompt: Some("do the work".to_string()),
            max_restarts: Some(0),
            budget_tokens: None,
            max_tool_calls: None,
            objective: None,
            timeout_secs: Some(60),
            simple: false,
            reservation_id: None,
            // `adapters::select` still resolves and readies "claude" (via
            // `ZIRV_CTX_AGENT_BIN` in `base_env`, unaffected by this); only
            // the actual spawn of *this* program has to fail, deterministically
            // and without depending on any real binary's own behavior.
            command: vec![
                "zirv-test-binary-that-does-not-exist-anywhere".to_string(),
                "-p".to_string(),
                "do the work".to_string(),
                "--session-id".to_string(),
                session.to_string(),
            ],
            ..Default::default()
        };
        let mut out = Vec::new();
        let result = run_with(&args, &mut out, tmp.path(), &|k| env.get(k).cloned());
        assert!(result.is_err(), "the launch must fail to spawn: {result:?}");

        let unread = crate::commands::ctx::mail::list(&state, &slug, None, None).expect("list");
        assert_eq!(
            unread.len(),
            1,
            "a launch that never spawned must not have consumed the mail"
        );
    }

    /// S3: a consume failure (e.g. `read/` cannot be created) must not sink
    /// the launch -- the mail already reached the prompt either way.
    #[test]
    fn a_failed_consume_does_not_stop_the_launch() {
        let tmp = crate::commands::ctx::testenv::repo();
        let home = tmp.path().join("home");
        let state_dir = tmp.path().join("state");
        let argv_log = tmp.path().join("argv.log");
        let session = "ffffffff-2222-4333-8444-555555555555";
        let mut env = base_env(&state_dir);
        env.insert("ZIRV_CTX_PACE".to_string(), "false".to_string());

        let state = crate::commands::ctx::state::StateDir::from_root(state_dir.clone());
        let slug = crate::commands::ctx::state::repo_slug(tmp.path());
        crate::commands::ctx::mail::store(
            &state,
            &slug,
            &crate::commands::ctx::mail::Message {
                from_session: "other-session".to_string(),
                from_agent: "claude".to_string(),
                to: "any".to_string(),
                to_session: None,
                sent: 1,
                body: "heads up: the webhook route moved".to_string(),
            },
            &CtxConfig::default(),
        )
        .expect("store mail");

        // Block `mail::consume`'s own `read/` directory creation by putting
        // an ordinary file where it needs a directory: a deterministic way
        // to force the consume step to fail without racing a real
        // filesystem deletion mid-flight.
        std::fs::write(state.mail().join(&slug).join("read"), b"not a directory")
            .expect("write blocker");

        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);
        unsafe {
            std::env::set_var("FAKE_AGENT_MODE", "healthy");
            std::env::set_var("FAKE_AGENT_ARGV_LOG", &argv_log);
        }
        let args = ExecArgs {
            agent: Some("claude".to_string()),
            session_id: Some(session.to_string()),
            transcript: Some(transcript_for(&home, tmp.path(), session)),
            prompt: Some("do the work".to_string()),
            max_restarts: Some(0),
            budget_tokens: None,
            max_tool_calls: None,
            objective: None,
            timeout_secs: Some(60),
            simple: false,
            reservation_id: None,
            command: fake_agent_command(session),
            ..Default::default()
        };
        let mut out = Vec::new();
        let code = run_with(&args, &mut out, tmp.path(), &|k| env.get(k).cloned());
        unsafe {
            std::env::remove_var("FAKE_AGENT_MODE");
            std::env::remove_var("FAKE_AGENT_ARGV_LOG");
        }
        assert_eq!(code.expect("a failed consume must not fail the launch"), 0);

        let argv = std::fs::read_to_string(&argv_log).expect("argv recorded");
        assert!(
            argv.contains("heads up: the webhook route moved"),
            "the mail still had to reach the prompt even though consuming it afterward failed: {argv}"
        );
    }

    /// I2: a user's own --append-system-prompt inside the `--` command must
    /// not be silently discarded by zirv's own occurrence of the same flag.
    #[test]
    fn a_users_own_append_system_prompt_is_merged_into_the_first_spawn() {
        let tmp = crate::commands::ctx::testenv::repo();
        let home = tmp.path().join("home");
        let state = tmp.path().join("state");
        let argv_log = tmp.path().join("argv.log");
        let session = "dddddddd-2222-4333-8444-555555555555";
        let mut env = base_env(&state);
        env.insert("ZIRV_CTX_PACE".to_string(), "false".to_string());

        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);
        unsafe {
            std::env::set_var("FAKE_AGENT_MODE", "healthy");
            std::env::set_var("FAKE_AGENT_ARGV_LOG", &argv_log);
        }
        let mut command = fake_agent_command(session);
        command.push("--append-system-prompt".to_string());
        command.push("always answer in Danish".to_string());
        let args = ExecArgs {
            agent: Some("claude".to_string()),
            session_id: Some(session.to_string()),
            transcript: Some(transcript_for(&home, tmp.path(), session)),
            prompt: Some("do the work".to_string()),
            max_restarts: Some(1),
            budget_tokens: None,
            max_tool_calls: None,
            objective: None,
            timeout_secs: Some(60),
            simple: false,
            reservation_id: None,
            command,
            ..Default::default()
        };
        let mut out = Vec::new();
        let code = run_with(&args, &mut out, tmp.path(), &|k| env.get(k).cloned());
        unsafe {
            std::env::remove_var("FAKE_AGENT_MODE");
            std::env::remove_var("FAKE_AGENT_ARGV_LOG");
        }
        assert_eq!(code.expect("runs"), 0);

        let argv = std::fs::read_to_string(&argv_log).expect("argv recorded");
        assert_eq!(
            argv.matches("--append-system-prompt").count(),
            1,
            "exactly one flag must reach the agent: {argv}"
        );
        assert!(
            argv.contains("always answer in Danish"),
            "the user's own instruction must survive: {argv}"
        );
        assert!(
            argv.contains("zirv engineering standard"),
            "zirv's own layer is still present: {argv}"
        );
    }
}
