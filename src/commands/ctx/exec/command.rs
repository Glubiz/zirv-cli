//! Headless commands, resume launches, and platform-safe prompt delivery.

use super::*;

/// Review round 1 (issue #303): `session` must carry the launch's own `cwd`,
/// not just its id -- `adapter.resume_target` needs both to recover an
/// adapter-minted id (codex) that has nothing to do with zirv's own. Fails
/// closed (`None`) whenever `resume_target` cannot recover a target at all,
/// exactly like the pre-existing honest-refusal default on `headless_resume_
/// cmd` itself: never resume a conversation this session did not verifiably
/// start.
pub(crate) fn headless_resume_launch(
    adapter: &dyn adapters::AgentAdapter,
    prompt: &str,
    session: &SessionRef,
    extra: &[String],
    prompt_via_stdin: bool,
) -> Option<(Command, Option<String>)> {
    let target = adapter.resume_target(session)?;
    let probe = adapter.headless_resume_cmd(Some(prompt), &target, extra)?;
    let argv_total_len = headless_argv_len(&probe);
    if headless_prompt_via_stdin(prompt_via_stdin, argv_total_len)
        && let Some(command) = adapter.headless_resume_cmd(None, &target, extra)
    {
        return Some((command, Some(prompt.to_string())));
    }
    Some((probe, None))
}

pub(super) fn build_command(command: &[String], repo: &Path) -> CtxResult<Command> {
    let (program, rest) = command
        .split_first()
        .ok_or("no command to supervise; pass it after --")?;
    let mut cmd = Command::new(program);
    cmd.args(rest).current_dir(repo);
    Ok(cmd)
}

/// Whether this run's own headless launch reparses its downstream argv on a
/// Windows launcher -- `cmd.exe /c <shim>` (an npm-installed `.cmd`) or
/// `powershell -NoProfile -File <script>` (a `.ps1`) -- so the prompt has to
/// go on stdin instead of argv (FIX B). `adapter.launches_through_cmd_shim()`
/// only recognises the `cmd.exe` form; probing the real launcher prefix this
/// run's headless spawn will use (`headless_cmd("", ...)`, no prompt token
/// yet) and asking `adapters::launch_reparses_through_shim` covers both,
/// matching the M1 fix `dash/mod.rs`'s `task_prompt_fallback_is_safe` made
/// for the pty path. Split out for the same reason that one was: testable
/// without spawning anything.
pub(crate) fn prompt_delivery_via_stdin(
    adapter: &dyn adapters::AgentAdapter,
    session: &SessionId,
) -> bool {
    let probe = adapters::flatten_command(adapter.headless_cmd("", session, &[]));
    adapters::launch_reparses_through_shim(&probe)
}

/// Issue #220: whether `build_headless` should route THIS launch's prompt to
/// stdin rather than argv. `shim` is [`prompt_delivery_via_stdin`]'s own
/// answer -- a Windows `cmd.exe`/`powershell -File` reparse, which forces
/// stdin regardless of size, exactly as before this issue. `argv_total_len`
/// is the second, independent reason: `adapter.headless_cmd` puts the
/// prompt on argv verbatim on every platform, and the WHOLE resulting
/// command line -- program, prompt, and every other argument riding beside
/// it, not the prompt token in isolation -- overflows `CreateProcessW`'s
/// ~32KB command-line limit outright (`os error 206`) on a perfectly
/// ordinary, non-shim launch once it exceeds
/// [`super::prompt::INLINE_ARGV_PROMPT_BUDGET_BYTES`] (issue #213's own
/// Windows-safe figure, reused rather than duplicated). Checked on every
/// platform, not only Windows, so the same launch always takes the same
/// delivery path regardless of where zirv runs.
///
/// Correctness follow-up (post-merge review): measuring the prompt alone
/// missed a real overflow -- the #213 system-prompt layer (folded into
/// `extra` as `--append-system-prompt <text>`/`-c developer_instructions=
/// <json>`) rides on this SAME command line and can itself occupy close to
/// the whole budget, so a prompt safely under budget by itself could still
/// leave the total argv over it. The caller ([`headless_argv_len`]) now
/// measures the fully assembled command `adapter.headless_cmd` would
/// actually emit, so this function's own logic did not need to change --
/// only what its second argument measures.
pub(super) fn headless_prompt_via_stdin(shim: bool, argv_total_len: usize) -> bool {
    shim || argv_total_len > super::prompt::INLINE_ARGV_PROMPT_BUDGET_BYTES
}

/// The total bytes `command`'s argv would actually put on the OS command
/// line: the program plus every argument, with one separator byte counted
/// between each token, and each argument's own length inflated for the
/// worst case of Windows' `CreateProcessW` quoting -- an under-count here
/// would let a launch through that still overflows.
/// [`headless_prompt_via_stdin`]'s own doc comment explains why this has to
/// be the WHOLE command, not just the prompt argument.
///
/// Review follow-up (post-merge): the raw byte length alone is not a safe
/// proxy for what actually lands on the command line. `std::process::
/// Command` on Windows builds a UTF-16 command line using the same quoting
/// `CommandLineToArgvW` expects: every `"` is escaped to `\"`, a run of
/// backslashes immediately before a quote doubles, and any argument
/// containing whitespace (a composed prompt almost always does) is wrapped
/// in a surrounding pair of quotes. A quote-and-backslash-heavy prompt can
/// therefore measure safely under `INLINE_ARGV_PROMPT_BUDGET_BYTES` in raw
/// bytes and still expand past `CreateProcessW`'s 32,767-char limit,
/// reproducing os error 206 despite this function's own budget check.
/// Per argument this counts `len + count('"') + count('\\') + 2` --
/// `len` for the literal bytes, one extra byte per `"`/`\\` for the
/// worst case where every one of them needs escaping, and `+ 2` for a
/// surrounding pair of quotes -- which over-counts an argument with no
/// quotes/backslashes/whitespace at all (no quoting needed) but never
/// under-counts one that does, which is the direction that matters: this
/// is deliberately a conservative, platform-independent estimate rather
/// than a byte-exact reproduction of `CreateProcessW`'s own algorithm.
pub(super) fn headless_argv_len(command: &Command) -> usize {
    let mut total = command.get_program().to_string_lossy().len();
    for arg in command.get_args() {
        let arg = arg.to_string_lossy();
        let quotes = arg.matches('"').count();
        let backslashes = arg.matches('\\').count();
        total += 1;
        total += arg.len() + quotes + backslashes + 2;
    }
    total
}

#[cfg(test)]
mod tests {
    use super::super::tests::*;
    use super::*;

    /// Issue #220: a non-shim launch (`shim == false`) whose total argv is
    /// safely under the budget keeps the prompt on argv -- byte-for-byte the
    /// pre-#220 behavior, so an ordinary short task prompt never starts
    /// taking the stdin path it never needed.
    #[test]
    fn headless_prompt_via_stdin_stays_on_argv_when_short_and_no_shim() {
        assert!(!headless_prompt_via_stdin(false, 100));
        assert!(!headless_prompt_via_stdin(
            false,
            super::super::prompt::INLINE_ARGV_PROMPT_BUDGET_BYTES
        ));
    }

    /// Issue #220's actual fix: a total argv over the budget routes to
    /// stdin even with no shim in play at all -- the class of overflow a
    /// `zirv workflow review run` package (full diff embedded) or a long
    /// `zirv agent codex "<...>"` prompt hits on a perfectly ordinary,
    /// direct `.exe` launch.
    #[test]
    fn headless_prompt_via_stdin_switches_to_stdin_once_the_total_argv_exceeds_the_budget() {
        assert!(headless_prompt_via_stdin(
            false,
            super::super::prompt::INLINE_ARGV_PROMPT_BUDGET_BYTES + 1
        ));
    }

    /// The shim reason (issue #213/FIX B) must keep forcing stdin regardless
    /// of size, including for a total argv far under the budget -- this
    /// function must never regress that existing guarantee while adding the
    /// new one.
    #[test]
    fn headless_prompt_via_stdin_still_forces_stdin_for_a_shim_launch_regardless_of_size() {
        assert!(headless_prompt_via_stdin(true, 1));
    }

    /// Post-merge correctness follow-up: the #213 system-prompt layer
    /// (`--append-system-prompt <text>`/`-c developer_instructions=<json>`,
    /// folded into `extra`) rides the SAME command line as the task prompt.
    /// A prompt safely under the budget by itself must still route to
    /// stdin once that other argument pushes the WHOLE argv over budget --
    /// `headless_argv_len` is what has to catch this, not `prompt_text.
    /// len()` alone (the bug this follow-up fixes).
    #[test]
    fn headless_argv_len_counts_every_argument_not_just_the_prompt() {
        let prompt = "short task prompt";
        assert!(prompt.len() < super::super::prompt::INLINE_ARGV_PROMPT_BUDGET_BYTES);

        let mut under_budget = Command::new("claude");
        under_budget
            .arg("-p")
            .arg(prompt)
            .arg("--session-id")
            .arg("abc");
        assert!(!headless_prompt_via_stdin(
            false,
            headless_argv_len(&under_budget)
        ));

        // The system-prompt layer alone is large enough to push the total
        // over budget, even though the prompt itself stayed small.
        let large_system_prompt = "y".repeat(super::super::prompt::INLINE_ARGV_PROMPT_BUDGET_BYTES);
        let mut over_budget = Command::new("claude");
        over_budget
            .arg("-p")
            .arg(prompt)
            .arg("--session-id")
            .arg("abc")
            .arg("--append-system-prompt")
            .arg(&large_system_prompt);
        let total = headless_argv_len(&over_budget);
        assert!(
            total > super::super::prompt::INLINE_ARGV_PROMPT_BUDGET_BYTES,
            "the extra system-prompt argument must be counted toward the total: {total}"
        );
        assert!(
            headless_prompt_via_stdin(false, total),
            "a prompt safely under budget on its own must still route to stdin once the other \
             arguments on the same command line push the WHOLE argv over budget"
        );
    }

    /// Review follow-up regression: raw byte length alone under-counts a
    /// quote-and-backslash-heavy prompt. Windows' `CreateProcessW`/
    /// `CommandLineToArgvW` quoting escapes every `"` and doubles a run of
    /// backslashes ahead of one, and wraps a whitespace-bearing argument in
    /// its own surrounding quotes, so a prompt built almost entirely of `"`
    /// and `\` characters can measure comfortably UNDER the raw-byte budget
    /// and still expand past the real 32,767-char Windows command-line
    /// limit -- reproducing os error 206 despite `headless_argv_len`'s own
    /// budget check. This prompt is constructed to sit just under the
    /// raw-byte budget by itself; the escaping-aware estimate must still
    /// route it to stdin.
    #[test]
    fn a_quote_and_backslash_heavy_prompt_under_the_raw_budget_still_routes_to_stdin() {
        let budget = super::super::prompt::INLINE_ARGV_PROMPT_BUDGET_BYTES;
        let half = (budget - 200) / 2;
        let prompt = format!("{}{}", "\"".repeat(half), "\\".repeat(half));
        assert!(
            prompt.len() < budget,
            "the raw prompt must sit under the raw-byte budget by construction: {} vs {budget}",
            prompt.len()
        );

        let mut command = Command::new("claude");
        command
            .arg("-p")
            .arg(&prompt)
            .arg("--session-id")
            .arg("abc");
        let total = headless_argv_len(&command);
        assert!(
            headless_prompt_via_stdin(false, total),
            "a prompt under the raw-byte budget but heavy on quote/backslash characters must \
             still route to stdin once Windows' own command-line quoting is estimated: raw {} \
             vs budget {budget}, escaping-aware total {total}",
            prompt.len()
        );
    }

    /// Final wave item 1: `adapter.launches_through_cmd_shim()` only
    /// recognises the `cmd.exe /c <shim>` form -- a `.ps1`-resolved
    /// `agent_bin` used to report "safe" here (prompt stays on argv) while
    /// still actually launching through `powershell -File`, which reparses
    /// that argv exactly like a `.cmd` shim does. `prompt_delivery_via_
    /// stdin` must report `true` for it too, mirroring the `.cmd` case and
    /// the same fix dash/mod.rs already got for the pty path.
    #[cfg(windows)]
    #[test]
    fn prompt_delivery_via_stdin_recognises_a_powershell_shim_not_just_a_cmd_one() {
        let dir = tempfile::tempdir().expect("tempdir");

        let cmd_shim = dir.path().join("codex.cmd");
        std::fs::write(&cmd_shim, "@echo off\r\n").expect("write cmd shim");
        let cmd_adapter = crate::commands::ctx::adapters::codex::CodexAdapter::new(Some(
            &cmd_shim.display().to_string(),
        ));
        let session = SessionId::parse("11111111-2222-4333-8444-555555555555");
        assert!(
            prompt_delivery_via_stdin(&cmd_adapter, &session),
            "the .cmd shim shape must still be recognised"
        );

        let ps_shim = dir.path().join("codex.ps1");
        std::fs::write(&ps_shim, "exit 0\r\n").expect("write ps1 shim");
        let ps_adapter = crate::commands::ctx::adapters::codex::CodexAdapter::new(Some(
            &ps_shim.display().to_string(),
        ));
        assert!(
            prompt_delivery_via_stdin(&ps_adapter, &session),
            "the .ps1 shim shape must also route the prompt to stdin"
        );

        let direct = crate::commands::ctx::adapters::codex::CodexAdapter::new(Some(
            "/tmp/fake-codex-not-a-real-path",
        ));
        assert!(
            !prompt_delivery_via_stdin(&direct, &session),
            "a non-shim program must keep the prompt on argv"
        );
    }

    /// The compiler seam used by `run_with` must carry the bounded memory core.
    #[test]
    fn compose_worker_launch_prompt_carries_the_memory_layer_under_its_configured_cap() {
        let repo = crate::commands::ctx::testenv::repo();
        let home = repo.path().join("home");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);
        let state = StateDir::from_root(repo.path().join("state"));
        let mut cfg = CtxConfig::default();
        cfg.memory.core_max_bytes = 40;
        // Issue #155: the merged memory layer is capped by the SUM of the two
        // budgets now, not `core_max_bytes` alone -- zero the retrieval half
        // out so this test's tiny budget still actually bounds what gets
        // delivered.
        cfg.memory.retrieval_max_bytes = 0;
        let slug = crate::commands::ctx::state::repo_slug(repo.path());

        crate::commands::ctx::memory::remember(
            &state,
            &slug,
            &crate::commands::ctx::memory::Entry {
                key: "seam-fact".to_string(),
                written_by: "test".to_string(),
                written: 1,
                verified: 1,
                source: "explicit".to_string(),
                body: format!("{}TAIL_MARKER_NOT_TRUNCATED", "z".repeat(200)),
                importance: None,
                confidence: None,
                tags: Vec::new(),
                paths: Vec::new(),
            },
            &cfg,
        )
        .expect("remember");

        let adapter = crate::commands::ctx::adapters::claude::ClaudeAdapter::new(None);
        let composed = crate::commands::ctx::compile::compile(
            Some(&home),
            repo.path(),
            false,
            &cfg,
            &adapter,
            super::super::prompt::PromptRole::Worker,
            &state,
            1,
            crate::commands::ctx::adapters::LaunchMode::Headless,
            false,
        )
        .composed
        .expect("a worker launch still composes a prompt");

        assert!(
            composed.text.contains("seam-fact"),
            "the memory core layer must reach the composed prompt: {}",
            composed.text
        );
        assert!(
            !composed.text.contains("TAIL_MARKER_NOT_TRUNCATED"),
            "a tiny core_max_bytes must actually bound the delivered memory layer: {}",
            composed.text
        );
        assert!(
            composed.text.contains("[memory truncated:"),
            "the truncation must be visible, not silent: {}",
            composed.text
        );
    }

    /// Issue #220, end to end: `zirv workflow review run`'s compact review
    /// package embeds the FULL diff as the task prompt, and a plain `zirv
    /// agent codex "<...>"` can just be handed a long string -- either way,
    /// the old code always put that text on argv (`adapter.headless_cmd`),
    /// which overflows `CreateProcessW`'s ~32KB command-line limit on
    /// Windows (`os error 206`) even on a perfectly ordinary, non-shim
    /// launch (`sh <fixture>`, never `cmd.exe /c <shim>` -- so the ONLY
    /// reason this prompt can land on stdin here is the new size-based
    /// routing, not `prompt_delivery_via_stdin`'s pre-existing shim check).
    /// `fake-codex-agent.sh` drains and logs stdin exactly so a test like
    /// this one can tell the two delivery paths apart.
    #[test]
    fn an_oversized_prompt_is_delivered_on_stdin_instead_of_argv() {
        let tmp = crate::commands::ctx::testenv::repo();
        let home = tmp.path().join("home");
        let state_dir = tmp.path().join("state");
        let argv_log = tmp.path().join("argv.log");
        let mut env = base_env(&state_dir);
        env.insert("ZIRV_CTX_PACE".to_string(), "false".to_string());
        env.insert("ZIRV_CTX_POLL_MS".to_string(), "50".to_string());
        env.insert(
            "ZIRV_CTX_AGENT_BIN".to_string(),
            format!("sh {}", fixture("fake-codex-agent.sh").display()),
        );

        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);
        let _fake_agent = crate::commands::ctx::testenv::VarGuard::set(&[(
            "FAKE_AGENT_ARGV_LOG",
            argv_log.to_str(),
        )]);

        let marker = "OVERSIZED_PROMPT_MARKER_9f3c1a";
        let oversized_prompt = format!(
            "{marker}{}",
            "x".repeat(super::super::prompt::INLINE_ARGV_PROMPT_BUDGET_BYTES + 500)
        );

        let args = ExecArgs {
            agent: Some("codex".to_string()),
            session_id: None,
            transcript: None,
            prompt: Some(oversized_prompt.clone()),
            max_restarts: Some(0),
            budget_tokens: None,
            max_tool_calls: None,
            objective: None,
            timeout_secs: Some(30),
            simple: true,
            reservation_id: None,
            command: Vec::new(),
            ..Default::default()
        };
        let mut out = Vec::new();
        let code = run_with(&args, &mut out, tmp.path(), &|k| env.get(k).cloned());
        assert_eq!(
            code.expect("an oversized prompt must not fail the launch"),
            0
        );

        let argv = std::fs::read_to_string(&argv_log).unwrap_or_default();
        for line in argv.lines().filter(|line| !line.starts_with("stdin: ")) {
            assert!(
                !line.contains(marker),
                "an oversized prompt must never be encoded onto argv: {line}"
            );
        }
        assert!(
            argv.contains(&format!("stdin: {oversized_prompt}")),
            "an oversized prompt must instead reach the child on stdin: {argv}"
        );
    }
}
