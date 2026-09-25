use clap::{Parser, Subcommand};

pub mod adapters;
pub mod agent;
pub mod agent_manifest;
pub mod allocator;
pub mod announce;
pub mod api;
pub mod ask;
pub mod attention;
pub mod breakdown;
pub mod capabilities_cmd;
pub mod catalogue;
pub mod chain;
pub mod chat;
pub mod chrome;
pub mod compile;
pub mod config;
pub mod config_cmd;
pub mod context;
pub mod context_cli;
pub mod context_lint;
pub mod context_status;
/// Issue #485 (roadmap N16): the native coordinator's durable task graph and
/// the one place a delegation's identity-decidable bounds are judged.
pub mod coordinator;
pub mod dash;
pub mod delegation;
pub mod diagnostics;
pub mod discover;
pub mod doctor;
pub mod drift;
pub mod envelope;
pub mod event;
pub mod exec;
pub mod fallback;
pub mod group;
pub mod handoff;
pub mod handover;
pub mod health;
pub mod health_store;
pub mod helper;
pub mod hook;
pub(crate) mod hook_integrity;
pub(crate) mod hook_project;
pub(crate) mod inject_gate;
pub mod inject_screen;
pub mod jev;
pub mod judge;
pub mod learn;
pub mod ledger;
pub mod lifecycle;
pub mod log;
pub mod mail;
pub mod mcp;
pub mod measure;
pub mod memory;
pub mod memory_cli;
pub mod memory_optimize;
pub(crate) mod native_account;
pub mod native_hooks;
pub mod native_worker;
pub mod obfuscate;
pub mod obfuscate_store;
pub mod objective;
pub mod optimize;
pub mod output;
pub(crate) mod output_diff;
pub(crate) mod output_filters;
pub(crate) mod output_markdown;
pub(crate) mod output_search;
pub(crate) mod output_shape;
pub mod pace;
pub(crate) mod pathutil;
pub mod permissions;
pub mod permit;
pub mod policy;
pub mod poll;
pub mod pool;
pub mod price;
pub mod priority;
pub mod prompt;
pub mod provider;
pub mod provider_cmd;
pub mod proxy;
pub mod reconcile;
pub mod reservation;
pub mod result_schema;
pub mod resume;
pub mod retrieval;
pub mod reuse;
pub mod rollover;
pub mod rollover_runtime;
pub mod rot;
pub mod route;
pub mod run_loop;
pub mod runtime;
pub mod safety;
pub mod score;
pub mod screen;
pub mod search;
pub mod search_index;
pub mod seat;
/// Issue #352: the persistent runtime service -- the process that owns
/// pty/ConPTY sessions so they outlive the client looking at them.
pub mod session;
pub mod session_spend;
pub mod sessions;
pub mod signal;
pub mod snapshot;
pub mod spend;
pub mod stall;
pub mod state;
pub mod status;
pub mod supervise;
pub mod surface;
pub mod task;
/// Issue #485 (roadmap N16): the native team's roles, the operator-configured
/// route each one spends, and the authority a role carries on its own.
pub mod team;
pub mod term;
pub(crate) mod testrun;
pub mod transcript_source;
pub mod usage;
pub mod window;
pub mod workspace;
pub mod worktree;
pub mod wrap;

/// The minimum gap a deferred injection leaves between writing its text and
/// the lone `\r` that submits it (issue #114, PR #116). Shared by
/// `dash::pane` (the dashboard's own visible injections) and `wrap` (the T13
/// mail-advisory injection into a `Capabilities::defer_injection_submit`
/// adapter, issue #118): both write the injected text first, flush, then
/// write the submitting `\r` no sooner than this gap later, because a
/// same-burst text+`\r` reads to a codex-shaped composer as a paste and
/// folds the `\r` into the pasted text instead of submitting it. See
/// `dash::pane::write_injection_phase1`'s own doc comment for the full
/// story, and `dash::pane::write_submit_cr`/`dash::pane::submit_is_due` for
/// the write and the deadline check both callers share too.
pub(crate) const INJECTION_SUBMIT_DELAY: std::time::Duration = std::time::Duration::from_millis(50);

/// Shared helpers for the supervisor tests, which drive real child processes
/// and therefore have to steer process-wide state carefully.
#[cfg(test)]
pub(crate) mod testenv {
    use std::ffi::OsString;
    use std::path::{Path, PathBuf};

    /// A temp directory whose path carries no symlink. Production hands the
    /// supervisors `std::env::current_dir`, which is always fully resolved, and
    /// the agent files its transcript under a slug of its own working
    /// directory. On macOS the temp dir sits behind the `/var` to
    /// `/private/var` symlink, so an unresolved repo path leaves the supervisor
    /// watching a slug the agent never writes to.
    pub(crate) struct TestRepo {
        _dir: tempfile::TempDir,
        path: PathBuf,
    }

    impl TestRepo {
        pub(crate) fn path(&self) -> &Path {
            &self.path
        }
    }

    pub(crate) fn repo() -> TestRepo {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = std::fs::canonicalize(dir.path()).expect("resolve tempdir");
        #[cfg(windows)]
        let path = {
            let rendered = path.to_string_lossy();
            PathBuf::from(rendered.strip_prefix(r"\\?\").unwrap_or(rendered.as_ref()))
        };
        TestRepo { _dir: dir, path }
    }

    /// Points the home directory (and optionally the working directory) at a
    /// test directory, putting every one of them back on drop.
    ///
    /// All of this is process-wide, so restoring has to survive a panicking
    /// assertion: a test that leaks `HOME` naming a deleted temp dir breaks
    /// every later pty spawn in the same run, because portable-pty starts its
    /// child in `$HOME` unless the caller sets a working directory and the
    /// `chdir` fails before the program is ever reached. A leaked working
    /// directory is worse still. Restoring on the happy path only -- which is
    /// what a `let result = test(); restore; result` helper does -- gets this
    /// exactly backwards: the failing test is the one that leaks.
    pub(crate) struct EnvGuard {
        home: Option<OsString>,
        userprofile: Option<OsString>,
        cwd: Option<PathBuf>,
    }

    impl EnvGuard {
        pub(crate) fn set(home: &Path, cwd: Option<&Path>) -> Self {
            let guard = Self {
                home: std::env::var_os("HOME"),
                userprofile: std::env::var_os("USERPROFILE"),
                cwd: cwd.and(std::env::current_dir().ok()),
            };
            // SAFETY: CI runs tests single-threaded.
            unsafe {
                std::env::set_var("HOME", home);
                std::env::set_var("USERPROFILE", home);
            }
            if let Some(cwd) = cwd {
                std::env::set_current_dir(cwd).expect("enter the test working directory");
            }
            guard
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            if let Some(previous) = self.cwd.take() {
                let _ = std::env::set_current_dir(previous);
            }
            // SAFETY: CI runs tests single-threaded.
            unsafe {
                for (key, previous) in [
                    ("HOME", self.home.take()),
                    ("USERPROFILE", self.userprofile.take()),
                ] {
                    match previous {
                        Some(previous) => std::env::set_var(key, previous),
                        None => std::env::remove_var(key),
                    }
                }
            }
        }
    }

    /// Sets (or clears) arbitrary environment variables for the duration of a
    /// test, putting every one of them back on drop -- including on a
    /// panicking assertion, for the same reason `EnvGuard` restores there.
    ///
    /// Needed by the tests that pin *inheritance* behavior: what a child
    /// process inherits is a fact about the real process environment, and
    /// `portable_pty::CommandBuilder::new` reads `std::env::vars_os` directly
    /// rather than through any injectable lookup, so there is nothing to fake.
    pub(crate) struct VarGuard(Vec<(String, Option<OsString>)>);

    impl VarGuard {
        pub(crate) fn set(vars: &[(&str, Option<&str>)]) -> Self {
            let previous = vars
                .iter()
                .map(|(key, _)| ((*key).to_string(), std::env::var_os(key)))
                .collect();
            // SAFETY: CI runs tests single-threaded.
            unsafe {
                for (key, value) in vars {
                    match value {
                        Some(value) => std::env::set_var(key, value),
                        None => std::env::remove_var(key),
                    }
                }
            }
            Self(previous)
        }
    }

    impl Drop for VarGuard {
        fn drop(&mut self) {
            // SAFETY: CI runs tests single-threaded.
            unsafe {
                for (key, previous) in self.0.drain(..) {
                    match previous {
                        Some(previous) => std::env::set_var(&key, previous),
                        None => std::env::remove_var(&key),
                    }
                }
            }
        }
    }

    /// Stubs an executable-named file for every entry in
    /// [`super::adapters::ADAPTERS`] on a fresh, otherwise-empty `PATH`, so
    /// issue #298's liveness probe (`adapters::liveness_probe`) confirms
    /// every registered adapter `Live` no matter what the host running the
    /// test actually has installed. Without this, any test that needs a
    /// non-empty harness roster is really asserting on the developer
    /// machine's own `PATH` -- true where `claude`/`codex` happen to be
    /// installed, false on a CI runner that carries neither, which is
    /// exactly the split issue #298 introduced (see its own roster-omission
    /// tests earlier in `adapters::tests` for the pattern this factors out).
    ///
    /// Returns both guards: the `TempDir` must outlive the `VarGuard` (drop
    /// order matters only in that dropping the directory first would delete
    /// the stub files while `PATH` still names it), so bind the whole tuple
    /// for the caller's scope rather than discarding either half.
    pub(crate) fn stub_live_adapters_on_path() -> (tempfile::TempDir, VarGuard) {
        let dir = tempfile::tempdir().expect("tempdir");
        for (name, _) in super::adapters::ADAPTERS {
            std::fs::write(dir.path().join(name), "").expect("write stub");
        }
        let guard = VarGuard::set(&[(
            "PATH",
            Some(dir.path().to_str().expect("utf8 tempdir path")),
        )]);
        (dir, guard)
    }

    /// Issue #609 (roadmap N22, review of #493): the install-proof
    /// counterpart of [`stub_live_adapters_on_path`] above. That helper
    /// drops an empty placeholder file per adapter, which is enough to prove
    /// PRESENCE but nothing about INVOCATION. This drops one CANARY
    /// executable per [`super::adapters::ADAPTERS`] entry -- enumerated from
    /// the same registry, so a ninth adapter needs no test rewritten -- and
    /// each canary appends its own name to `invoked_log` and exits non-zero
    /// if the OS ever actually runs it. A test can then assert `invoked_log`
    /// never came to exist: direct evidence that no registered harness
    /// executable was spawned, not merely that `PATH` came up empty (which a
    /// regression that resolved a harness by full path, or via `PATHEXT`,
    /// could still slip past).
    ///
    /// Returns both guards for the same reason `stub_live_adapters_on_path`
    /// does: the `TempDir` must outlive the `VarGuard`.
    pub(crate) fn canary_path_for_every_registered_harness(
        invoked_log: &Path,
    ) -> (tempfile::TempDir, VarGuard) {
        let dir = tempfile::tempdir().expect("tempdir");
        for (name, _) in super::adapters::ADAPTERS {
            write_canary(dir.path(), name, invoked_log);
        }
        let guard = VarGuard::set(&[(
            "PATH",
            Some(dir.path().to_str().expect("utf8 tempdir path")),
        )]);
        (dir, guard)
    }

    /// Windows canary: a `.cmd` script. `std::process::Command` on Windows
    /// resolves a bare program name (no extension) against `PATHEXT`-style
    /// candidates on each `PATH` directory, the same resolution a real
    /// `claude`/`codex` npm-shim install relies on, so `Command::new(name)`
    /// finds this exactly as it would a genuine install.
    #[cfg(windows)]
    fn write_canary(dir: &Path, name: &str, invoked_log: &Path) {
        let script = dir.join(format!("{name}.cmd"));
        std::fs::write(
            &script,
            format!(
                "@echo off\r\necho {name}>>\"{}\"\r\nexit /b 7\r\n",
                invoked_log.display()
            ),
        )
        .expect("write canary");
    }

    /// Unix canary: a `chmod +x` shell script with the bare adapter name --
    /// mirrors `wrap.rs`'s own `#[cfg(unix)]` stub-executable tests (see
    /// CLAUDE.md). Not exercised on this Windows dev machine; CI's Linux and
    /// macOS legs of the `Native Install` job are what actually run it.
    #[cfg(unix)]
    fn write_canary(dir: &Path, name: &str, invoked_log: &Path) {
        use std::os::unix::fs::PermissionsExt;
        let script = dir.join(name);
        std::fs::write(
            &script,
            format!(
                "#!/bin/sh\necho {name} >> \"{}\"\nexit 7\n",
                invoked_log.display()
            ),
        )
        .expect("write canary");
        let mut perms = std::fs::metadata(&script)
            .expect("canary metadata")
            .permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&script, perms).expect("chmod +x canary");
    }

    /// Enters `dir` and returns to the previous working directory on drop --
    /// including on a panicking assertion, which is the whole point.
    ///
    /// The process-wide working directory is the single most damaging thing a
    /// test can leak: every later test that resolves a relative path, and
    /// every child process spawned without an explicit `current_dir`, picks
    /// it up. A `set_current_dir(original)` written at the *end* of a test
    /// body restores it only when the test passes, which gets it exactly
    /// backwards -- the failing test is the one that leaks, and the leak then
    /// shows up as a cascade of unrelated failures (often against a temp
    /// directory that no longer exists).
    pub(crate) struct CwdGuard(Option<PathBuf>);

    impl CwdGuard {
        pub(crate) fn enter(dir: &Path) -> std::io::Result<Self> {
            let previous = std::env::current_dir().ok();
            std::env::set_current_dir(dir)?;
            Ok(Self(previous))
        }
    }

    impl Drop for CwdGuard {
        fn drop(&mut self) {
            if let Some(previous) = self.0.take() {
                let _ = std::env::set_current_dir(previous);
            }
        }
    }

    /// `EnvGuard` without the working directory, which is all most tests need.
    pub(crate) struct HomeGuard(#[allow(dead_code)] EnvGuard);

    impl HomeGuard {
        pub(crate) fn set(home: &Path) -> Self {
            Self(EnvGuard::set(home, None))
        }
    }

    /// The full scrub a test harness needs before spawning a *real* `zirv`
    /// subprocess: `sessions::SUPERVISION_ENV` plus `CLAUDE_PID`/`CLAUDECODE`
    /// plus `DASH_REQUESTS_ENV`.
    ///
    /// Production's own `sessions::scrub_supervision_env`/`_cmd` deliberately
    /// stop short of `DASH_REQUESTS_ENV` -- a dashboard pane's own child must
    /// still be able to reach the spawn-request channel (see
    /// `nested_session_evidence`'s own doc comment) -- so this cannot just
    /// widen those. It delegates to the unmodified production helper and
    /// layers the extra test-only scrubs on top, purely so a suite spawned
    /// from inside a dashboard pane (this whole suite's own environment, when
    /// run via `zirv ctx dash`) does not trip its own nesting guard.
    ///
    /// `#[cfg(unix)]`: every current caller is one of wrap.rs's real-PTY
    /// `CommandBuilder` harnesses, which are themselves `#[cfg(unix)]` --
    /// see CLAUDE.md. `scrub_supervision_env_for_test_cmd` below is the
    /// cross-platform counterpart `mod win`'s `std::process::Command`
    /// harness uses instead.
    #[cfg(unix)]
    pub(crate) fn scrub_supervision_env_for_test(builder: &mut portable_pty::CommandBuilder) {
        super::sessions::scrub_supervision_env(builder);
        builder.env_remove("CLAUDE_PID");
        builder.env_remove("CLAUDECODE");
        builder.env_remove(super::dash::spawnreq::DASH_REQUESTS_ENV);
    }

    /// The `std::process::Command` counterpart of `scrub_supervision_env_for_test`.
    pub(crate) fn scrub_supervision_env_for_test_cmd(command: &mut std::process::Command) {
        super::sessions::scrub_supervision_env_cmd(command);
        command.env_remove("CLAUDE_PID");
        command.env_remove("CLAUDECODE");
        command.env_remove(super::dash::spawnreq::DASH_REQUESTS_ENV);
    }

    #[cfg(unix)]
    pub(crate) fn scrub_operator_profile_env_for_test(builder: &mut portable_pty::CommandBuilder) {
        for (key, _) in std::env::vars_os() {
            if key.to_string_lossy().starts_with("ZIRV_CTX_")
                || key.to_string_lossy().starts_with("ZIRV_AGENT_")
            {
                builder.env_remove(key);
            }
        }
    }

    pub(crate) fn scrub_operator_profile_env_for_test_cmd(command: &mut std::process::Command) {
        for (key, _) in std::env::vars_os() {
            if key.to_string_lossy().starts_with("ZIRV_CTX_")
                || key.to_string_lossy().starts_with("ZIRV_AGENT_")
            {
                command.env_remove(key);
            }
        }
    }

    /// A pid guaranteed dead by the time it is used: a real child process,
    /// spawned and waited on, so its exit is deterministic rather than a
    /// hardcoded number that might collide with something alive on this
    /// machine. Shared by every test standing in for a dashboard that exited
    /// abnormally, leaving its `owner.pid` naming a process that is gone.
    pub(crate) fn dead_pid() -> u32 {
        let mut cmd = if cfg!(windows) {
            let mut c = std::process::Command::new("cmd");
            c.args(["/C", "exit", "0"]);
            c
        } else {
            std::process::Command::new("true")
        };
        let mut child = cmd.spawn().expect("spawn a short-lived process");
        let pid = child.id();
        let _ = child.wait();
        pid
    }
}

/// Every ctx entry point returns this. Matches the error style used by the
/// rest of the crate (`Box<dyn std::error::Error>`).
pub type CtxResult<T> = Result<T, Box<dyn std::error::Error>>;

// Item 7: named here, in the text `zirv ctx --help` actually prints, so
// nothing implies an unready adapter works today. `readiness_note` generates
// the not-ready clause from the registry's own `ready()` calls rather than a
// literal, so it never drifts from adapters::codex::CodexAdapter::ready --
// the same wording a user hits directly via `--agent codex`.
//
// Perf: `clap`'s derive bakes `about` into `CtxCli::command()`, which runs on
// *every* `try_parse_from` -- i.e. every `dispatch()` call, whether or not
// help text is ever displayed. `readiness_note()` calls `ready()` on every
// registered adapter, and on Windows that walks `PATH`/`PATHEXT` per
// adapter, so an ordinary `ctx hook pretool` (fired by Claude Code on every
// tool call) or `ctx usage tee` (once per statusline render) used to pay
// ~275ms for text nobody was about to read. `dispatch` now decides from raw
// argv, before `try_parse_from` ever builds `CtxCli::command()`, whether this
// invocation will actually render `zirv ctx`'s own help (see
// `ctx_will_render_help`, which mirrors `main.rs`'s pre-clap
// `is_top_level_help`) and only then flips `SHOW_READINESS_NOTE`.
// `ctx_about()` skips the probe entirely otherwise, and once the note *has*
// been computed for a help render it stays cached for the rest of the
// process -- free within one process (tests, in particular, call `dispatch`
// hundreds of times) even though a fresh `zirv ctx ...` invocation is still
// its own process either way. The flag only ever moves false -> true: a
// process that renders help after already having dispatched a plain verb
// must still show the note, so nothing resets it back to false.
static SHOW_READINESS_NOTE: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// True when `args` (`args[0]` is the literal "ctx", matching `dispatch`'s
/// own convention) will make clap render `zirv ctx`'s own top-level help --
/// no verb at all, or `help`/`-h`/`--help` immediately after it. A help flag
/// on a *subcommand* (`zirv ctx hook --help`) renders that subcommand's own
/// help, not `CtxCli`'s `about`, so it is deliberately not matched here.
/// Conservative on the "no verb" arm: clap's missing-required-subcommand
/// error may not always print the full about paragraph, but treating it as a
/// help render costs nothing (it was already going to fail) and keeps this
/// simple enough to trust by inspection.
fn ctx_will_render_help(args: &[String]) -> bool {
    matches!(
        args.get(1).map(String::as_str),
        None | Some("help") | Some("-h") | Some("--help")
    )
}

fn ctx_about() -> String {
    if !SHOW_READINESS_NOTE.load(std::sync::atomic::Ordering::Relaxed) {
        return "Autonomous context management for AI coding agents.".to_string();
    }
    static ABOUT_WITH_NOTE: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    ABOUT_WITH_NOTE
        .get_or_init(|| {
            format!(
                "Autonomous context management for AI coding agents. {}",
                adapters::readiness_note()
            )
        })
        .clone()
}

#[derive(Debug, Parser)]
#[command(name = "zirv ctx", about = ctx_about(), disable_help_subcommand = true)]
pub struct CtxCli {
    #[command(subcommand)]
    pub verb: CtxVerb,
}

#[derive(Debug, Subcommand)]
pub enum CtxVerb {
    /// Serve read-only, repository-scoped tools over MCP stdio.
    Mcp(mcp::McpArgs),
    /// Show or edit operator ~/.zirv/ctx.toml; set/add ask for approval in wrapped sessions.
    Config(config_cmd::ConfigArgs),
    /// Native provider routes are coming soon and cannot be enabled in this release.
    Provider(provider_cmd::ProviderArgs),
    /// Rot-score a session transcript and print JSON.
    Score(score::ScoreArgs),
    /// Distill a handoff from a transcript.
    Handoff(handoff::HandoffArgs),
    /// Start a clean interactive session with the latest handoff injected.
    Resume(resume::ResumeArgs),
    /// Agent hook entrypoints.
    Hook(hook::HookArgs),
    /// Show supervised sessions, scores and handoffs. The `spend:` line's
    /// "this session"/"this 5h window" figures include this session's own
    /// seat transcript, its native subagent transcripts, and every `zirv
    /// agent` delegation row -- never delegations alone (issue #457).
    Status(status::StatusArgs),
    /// Explain one session's composed attention projection: what it is,
    /// why, which authority decided, and every fallback that was suppressed
    /// (issue #349).
    #[command(name = "explain-status")]
    ExplainStatus(attention::ExplainStatusArgs),
    /// Block until a session's attention projection matches, or time out
    /// (issue #349).
    Wait(attention::WaitArgs),
    /// Block on a session or a delegation until it reaches a terminal state,
    /// streaming one line per distinct transition observed along the way;
    /// `--since <revision>` resumes without re-printing what an earlier
    /// `watch` already reported (issue #724).
    Watch(attention::WatchArgs),
    /// Stateless loop runner: a fresh headless session per cycle.
    #[command(name = "loop")]
    Loop(run_loop::LoopArgs),
    /// Supervise one headless run.
    Exec(exec::ExecArgs),
    /// Supervise an interactive TUI through a PTY.
    Wrap(wrap::WrapArgs),
    /// Run one command directly (no shell), store its full output under the
    /// state dir, and print a compact, reversible summary instead of the
    /// output itself (issue #326).
    Run(output::RunArgs),
    /// Retrieve output `zirv ctx run` stored: `show <id> [--range A-B]`,
    /// `list` (issue #326).
    Output(output::OutputArgs),
    /// Report usage windows, or tee the statusline to record them.
    Usage(usage::UsageArgs),
    /// Analyse the configuration surfaces that steer every session.
    Optimize(optimize::OptimizeArgs),
    /// Audit, reveal or purge this repository's local sensitive-value vault.
    Obfuscate(obfuscate_store::ObfuscateArgs),
    /// Start an interactive orchestrator session on the resolved adapter.
    Chat(chat::ChatArgs),
    /// Run a supervised headless worker on another enabled harness.
    Agent(agent::AgentArgs),
    /// Leave a note for other agent sessions on this machine.
    Send(mail::SendArgs),
    /// Read notes other agent sessions left for this one.
    Inbox(mail::InboxArgs),
    /// Store a durable fact in this repository's memory bank.
    Remember(memory::RememberArgs),
    /// List durable facts from this repository's memory bank.
    Recall(memory::RecallArgs),
    /// Remove one or all facts from this repository's memory bank.
    Forget(memory::ForgetArgs),
    /// Interrupt a live session with a message: durable mail plus a wake-up.
    Nudge(sessions::NudgeArgs),
    /// Distill a read-only answer to a question from a LIVE worker
    /// session's own transcript (issue #310, 3c). Never sends input to that
    /// session -- no pty/stdin write, no nudge, no mail -- and never
    /// modifies its transcript or registry record.
    Ask(ask::AskArgs),
    /// Terminate a registered session's process outright: SIGTERM, escalating
    /// to SIGKILL, then deregister it -- unlike `nudge`, this never depends
    /// on the target being able to notice or act on anything. On unix a pid
    /// the OS has recycled since the session registered is deregistered
    /// without being signalled; where that start-time check cannot run
    /// (Windows, or no `ps`) the registered pid is signalled as-is.
    Kill(sessions::KillArgs),
    /// Evaluate, list or explain zirv's harness-neutral command safety policy.
    Safety(safety::SafetyArgs),
    /// Swap the orchestrator seat's model or harness in place, same session id.
    Handover(handover::HandoverArgs),
    /// Audit recent transcripts for escalated/denied permission requests.
    Permissions(permissions::PermissionsArgs),
    /// Open, inspect or close a bounded group of delegated work.
    Group(group::GroupArgs),
    /// Compose the session prompt for the current repo/role/harness -- print
    /// it, or measure its per-layer byte/token cost with `--measure`.
    Compile(compile::CompileArgs),
    /// Set, show or close this repository's durable objective (issue #285).
    Objective(objective::ObjectiveArgs),
    /// Aggregate delegation spend from the cost ledger (issue #264).
    Spend(spend::SpendArgs),
    /// Decide the harness proxy's routing for a request and print it --
    /// never launches (issue #537 seam).
    Proxy(proxy::ProxyArgs),
    /// Report how much `zirv ctx run`'s compact-output hook has actually
    /// saved: rows, bytes in/out, saved bytes/percent, an outcome breakdown
    /// and a dollar estimate (issue #422).
    Savings(ledger::SavingsArgs),
    /// Print a redacted, capped diagnostic-state summary (issue #320).
    Snapshot(snapshot::SnapshotArgs),
    /// Zero-model cross-session recall: rank transcripts, handoffs, work
    /// artifacts and mail against a query, or scroll a specific session with
    /// `--session`/`--around` (issue #315).
    Search(search::SearchArgs),
    /// List, finalize or prune `zirv ctx agent --worktree`'s own linked
    /// worktrees (issue #319): proof-required reclaim, so nothing is ever
    /// removed without affirmative evidence it carries no unrecoverable work.
    Worktree(worktree::WorktreeArgs),
    /// One level-triggered pass over every opportunistic sweep this crate
    /// already runs (stuck task claims, dead-owner permits/reservations,
    /// worktree GC) plus the one resource with no automatic reclaim at all,
    /// an abandoned work group (issue #720). `--dry-run` mutates nothing;
    /// `--json` prints one object per resource kind.
    Reconcile(reconcile::ReconcileArgs),
    /// Durable task cards for delegated work: create/list/show/claim/
    /// heartbeat/complete/block/unblock/comment/archive (issue #317).
    Task(task::TaskArgs),
    /// Mints a root + N worker + verifier + synthesizer task-card batch, all
    /// as one atomic write (issue #317).
    Swarm(task::SwarmArgs),
    /// Transcript-derived proportionality metrics -- partial-read rate, edit
    /// inflation, tool-result token share, context utilisation, compaction
    /// rate, turns per user message -- with an optional committed baseline
    /// to diff against (issue #294). Strictly read-only outside `measure
    /// baseline`.
    Measure(measure::MeasureArgs),
    /// List the largest `Bash` tool results in recent sessions that reached
    /// the model uncompacted, bucketed by reason -- measured against the
    /// compaction ledger where a row exists, estimated from today's config
    /// otherwise (issue #423). Read-only: never changes config.
    Discover(discover::DiscoverArgs),
    /// Promotes a recurring fail-then-fix command correction from recent
    /// transcripts into one `learned:`-prefixed private memory entry
    /// (issue #425). Read-only with `--dry-run`.
    Learn(learn::LearnArgs),
    /// The versioned local runtime protocol (issue #353): `schema` prints
    /// the v1 contract, `serve` binds the local endpoint, `call` invokes one
    /// method over it.
    Api(api::ApiArgs),
    /// Report every configured integration -- MCP servers, web search/fetch,
    /// browser, diagnostics, artifact and frontend rendering -- as available,
    /// unavailable or unverified, with the diagnosis for anything missing
    /// (issue #483). `--probe` contacts each MCP server to verify it.
    Capabilities(capabilities_cmd::CapabilitiesArgs),
    /// Diagnose native readiness (issue #491): for every role, which backend
    /// an unflagged session gets and why, which route it would spend, and
    /// every problem sorted into exactly one named class -- missing auth
    /// material, inaccessible model, missing tool, unsupported isolation,
    /// service failure or upstream entitlement limit. Redacted like `ctx
    /// snapshot`, so the output is safe to paste into a bug report. Exits 1
    /// when a role that would run natively has no usable route. `--live`
    /// additionally contacts each provider's model-list endpoint.
    Doctor(doctor::DoctorArgs),
    /// Jev client operations: status reports whether it is enabled, the five
    /// advisory gates, the credential and its presence, and the endpoint/model.
    /// Reads configuration only; never makes network calls, never reads
    /// credential values. Distinguishes "no gate enabled" from "gate enabled
    /// but credential missing", so the operator can identify exactly what is
    /// blocking Jev when `advise` silently returns `None`.
    Jev(jev::JevArgs),
}

/// What a clap parse failure costs, which is not the same for every verb.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParseFailure {
    /// The ordinary case: clap printed the error, the caller sees exit 2.
    Reject,
    /// Claude Code reads a Stop hook's exit 2 as "block the stop", so a
    /// mistyped hook invocation would wedge the agent it is meant to watch.
    Hook,
    /// Claude Code renders whatever the statusline command prints, so exiting
    /// without a line looks like a broken terminal to the user.
    Statusline,
}

/// Reads the verb straight from argv, because by the time this runs clap has
/// already refused to tell us what was meant.
pub fn classify_parse_failure(args: &[String]) -> ParseFailure {
    match (
        args.get(1).map(String::as_str),
        args.get(2).map(String::as_str),
    ) {
        (Some("hook"), _) => ParseFailure::Hook,
        (Some("usage"), Some("tee")) => ParseFailure::Statusline,
        _ => ParseFailure::Reject,
    }
}

fn read_stdin() -> String {
    use std::io::Read;
    let mut buffer = String::new();
    let _ = std::io::stdin().read_to_string(&mut buffer);
    buffer
}

/// `args[0]` is the literal "ctx" as it appeared in argv.
pub fn dispatch(args: &[String]) -> i32 {
    // The private MCP relay must not probe harness readiness, load repository
    // configuration, or construct the ordinary CLI before authenticating.
    if args.get(1).is_some_and(|verb| verb == "provider") && !runtime::native_available() {
        eprintln!("{}", runtime::NATIVE_COMING_SOON);
        return 1;
    }
    if args.len() == 3 && args[1] == "provider" && args[2] == "bridge" {
        return runtime::execution::bridge_stdio().unwrap_or(1);
    }
    if ctx_will_render_help(args) {
        SHOW_READINESS_NOTE.store(true, std::sync::atomic::Ordering::Relaxed);
    }
    let argv = std::iter::once("zirv ctx".to_string()).chain(args.iter().skip(1).cloned());
    let cli = match CtxCli::try_parse_from(argv) {
        Ok(cli) => cli,
        Err(err) => {
            let _ = err.print();
            // clap represents `--help`/`--version` as an `Err` too, since
            // printing and exiting is the caller's job here; both are
            // informational, not a rejected invocation, and must exit 0 like
            // top-level `zirv --help` already does via `Parser::parse()`'s
            // own exit path. A genuine parse error keeps falling through to
            // `classify_parse_failure` below.
            if matches!(
                err.kind(),
                clap::error::ErrorKind::DisplayHelp | clap::error::ErrorKind::DisplayVersion
            ) {
                return 0;
            }
            let mut out = std::io::stdout();
            return match classify_parse_failure(args) {
                ParseFailure::Reject => 2,
                ParseFailure::Hook => 0,
                // The same fallback the tee itself uses when its chained
                // command is missing or broken.
                ParseFailure::Statusline => usage::run_tee(&mut out, &read_stdin(), &[], None, 0),
            };
        }
    };

    // Issue #330: the one place a `zirv ctx` PROCESS learns which kind of
    // session it is about to become, and the last one before any of it runs.
    // `exec`, `loop` and `agent` are the three verbs that supervise delegated
    // work (`PromptRole::Worker` throughout those modules -- none of them
    // takes a role parameter to get wrong), so this process takes the worker
    // posture here and every child it goes on to spawn, harness and cargo
    // alike, inherits it. `zirv agent ...` and `zirv chat` arrive here too:
    // `main`'s top-level aliases rewrite argv and route through this same
    // dispatch. Deliberately at the dispatch rather than inside those
    // modules' own `run()` functions -- unit tests call those directly, and a
    // test binary must never lower a process it does not own. `wrap`/`chat`
    // are not listed: the interactive posture belongs to the launch itself
    // (see `wrap::run_with`) and only ever raises a thread.
    if matches!(
        &cli.verb,
        CtxVerb::Exec(_) | CtxVerb::Loop(_) | CtxVerb::Agent(_)
    ) {
        priority::apply_process(priority::posture_for(prompt::PromptRole::Worker));
    }

    let mut out = std::io::stdout();
    let result = match &cli.verb {
        CtxVerb::Mcp(a) => mcp::run(a),
        CtxVerb::Config(a) => config_cmd::run(a, &mut out),
        CtxVerb::Provider(a) => provider_cmd::run(a, &mut out),
        CtxVerb::Score(a) => score::run(a, &mut out),
        CtxVerb::Handoff(a) => handoff::run(a, &mut out),
        CtxVerb::Resume(a) => resume::run(a, &mut out),
        CtxVerb::Hook(a) => hook::run(a, &mut out),
        CtxVerb::Status(a) => status::run(a, &mut out),
        CtxVerb::ExplainStatus(a) => attention::run_explain_status(a, &mut out),
        CtxVerb::Wait(a) => attention::run_wait(a, &mut out),
        CtxVerb::Watch(a) => attention::run_watch(a, &mut out),
        CtxVerb::Loop(a) => run_loop::run(a, &mut out),
        CtxVerb::Exec(a) => exec::run(a, &mut out),
        CtxVerb::Wrap(a) => wrap::run(a, &mut out),
        CtxVerb::Run(a) => output::run(a, &mut out),
        CtxVerb::Output(a) => output::run_output(a, &mut out),
        CtxVerb::Usage(a) => usage::run(a, &mut out),
        CtxVerb::Optimize(a) => optimize::run(a, &mut out),
        CtxVerb::Obfuscate(a) => obfuscate_store::run(a, &mut out),
        CtxVerb::Chat(a) => chat::run(a, &mut out),
        CtxVerb::Agent(a) => agent::run(a, &mut out),
        CtxVerb::Send(a) => mail::run_send(a, &mut out),
        CtxVerb::Inbox(a) => mail::run_inbox(a, &mut out),
        CtxVerb::Remember(a) => memory::run_remember(a, &mut out),
        CtxVerb::Recall(a) => memory::run_recall(a, &mut out),
        CtxVerb::Forget(a) => memory::run_forget(a, &mut out),
        CtxVerb::Nudge(a) => sessions::run_nudge(a, &mut out),
        CtxVerb::Ask(a) => ask::run(a, &mut out),
        CtxVerb::Kill(a) => sessions::run_kill(a, &mut out),
        CtxVerb::Safety(a) => safety::run(a, &mut out),
        CtxVerb::Handover(a) => handover::run(a, &mut out),
        CtxVerb::Permissions(a) => permissions::run(a, &mut out),
        CtxVerb::Group(a) => group::run(a, &mut out),
        CtxVerb::Compile(a) => compile::run(a, &mut out),
        CtxVerb::Objective(a) => objective::run(a, &mut out),
        CtxVerb::Spend(a) => spend::run(a, &mut out),
        CtxVerb::Proxy(a) => proxy::run(a, &mut out),
        CtxVerb::Savings(a) => ledger::run(a, &mut out),
        CtxVerb::Snapshot(a) => snapshot::run(a, &mut out),
        CtxVerb::Search(a) => search::run(a, &mut out),
        CtxVerb::Worktree(a) => worktree::run(a, &mut out),
        CtxVerb::Reconcile(a) => reconcile::run(a, &mut out),
        CtxVerb::Task(a) => task::run(a, &mut out),
        CtxVerb::Swarm(a) => task::run_swarm(a, &mut out),
        CtxVerb::Measure(a) => measure::run(a, &mut out),
        CtxVerb::Discover(a) => discover::run(a, &mut out),
        CtxVerb::Learn(a) => learn::run(a, &mut out),
        CtxVerb::Api(a) => api::run(a, &mut out),
        CtxVerb::Capabilities(a) => capabilities_cmd::run(a, &mut out),
        CtxVerb::Doctor(a) => doctor::run(a, &mut out),
        CtxVerb::Jev(a) => jev::run_jev(a, &mut out),
    };

    match result {
        Ok(code) => code,
        Err(e) => {
            crate::output::error(e);
            1
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Item 7: `zirv ctx --help` is the first thing a curious user reads, so
    /// it must say plainly which adapters are not ready yet, the same
    /// honesty an adapter's own `ready()` gives a user who tries `--agent
    /// <name>` directly. Pinned as a property over the registry (every
    /// adapter whose own `ready()` fails must be named, with a not-ready
    /// indication, and -- the other direction -- nothing is named not-ready
    /// when every adapter's own `ready()` succeeds, true today now that
    /// `CodexAdapter::ready` mirrors claude's) rather than a literal
    /// sentence, so wiring up a real adapter -- or adding a third one that is
    /// not ready -- keeps this test honest without an edit.
    #[test]
    fn the_top_level_help_names_every_adapter_that_is_not_ready() {
        use clap::CommandFactory;
        // `ctx_about()` only pays for the readiness probe when `dispatch`
        // has decided (from raw argv) that help is actually about to render;
        // calling `CtxCli::command()` directly bypasses that decision, so
        // this test makes it itself rather than going through `dispatch`.
        SHOW_READINESS_NOTE.store(true, std::sync::atomic::Ordering::Relaxed);
        let about = CtxCli::command()
            .get_about()
            .map(|s| s.to_string())
            .unwrap_or_default();

        let not_ready: Vec<_> = adapters::all(None)
            .into_iter()
            .filter(|a| a.ready().is_err())
            .collect();
        for adapter in &not_ready {
            assert!(
                about.contains(adapter.name()),
                "about must name not-ready adapter '{}': {about}",
                adapter.name()
            );
        }
        let claims_not_ready = about.to_lowercase().contains("not implemented yet")
            || about.to_lowercase().contains("not ready");
        assert_eq!(
            claims_not_ready,
            !not_ready.is_empty(),
            "about's not-ready claim must match the registry's own ready() calls: {about}"
        );
    }

    /// Item 16: the property test above reads `about` off `ctx_about()`'s
    /// process-wide `OnceLock` -- on any real machine both adapters are
    /// `ready()`, so its `for adapter in &not_ready` loop body has never
    /// once executed, and by the time this test runs the cache may already
    /// be warmed by an *earlier* test's own `CtxCli::try_parse_from` call
    /// (this module has several, and so do `optimize.rs`/`usage.rs`), which
    /// a same-test PATH rig cannot retroactively change. Rigged directly
    /// against `adapters::readiness_note()` instead -- the exact function
    /// `ctx_about()` wraps and caches, so this is the same substance without
    /// the caching hazard -- using the identical PATH/PATHEXT rig `adapters::
    /// tests::readiness_note_and_the_fallback_skip_both_stay_covered_when_
    /// an_adapter_is_genuinely_unready` already established for exactly this
    /// "force claude genuinely unready" shape.
    #[cfg(windows)]
    #[test]
    fn readiness_note_names_a_genuinely_unready_adapter() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("claude.py"), "print('x')\n").expect("write");

        let path = std::env::var("PATH").unwrap_or_default();
        let _path_guard = crate::commands::ctx::testenv::VarGuard::set(&[
            (
                "PATH",
                Some(format!("{};{}", dir.path().display(), path).as_str()),
            ),
            ("PATHEXT", Some(".EXE;.CMD;.PY")),
        ]);

        let not_ready: Vec<_> = adapters::all(None)
            .into_iter()
            .filter(|a| a.ready().is_err())
            .collect();
        assert!(
            !not_ready.is_empty(),
            "the rig must genuinely make claude unready"
        );

        let note = adapters::readiness_note();
        for adapter in &not_ready {
            assert!(
                note.contains(adapter.name()),
                "readiness_note (what ctx_about's cached `about` is built from) must name \
                 not-ready adapter '{}': {note}",
                adapter.name()
            );
        }
        assert!(note.to_lowercase().contains("not ready"), "got {note}");
    }

    /// Perf regression guard for the fix above: a hook invocation (fired by
    /// Claude Code on every single tool call) must never pay for
    /// `adapters::readiness_note()`'s PATH walk. `SHOW_READINESS_NOTE` starts
    /// this test process false; `ctx_will_render_help` must say a hook argv
    /// does not render help, and parsing it -- the exact `CtxCli::
    /// try_parse_from` call `dispatch` makes, which is what bakes `about`
    /// into `CtxCli::command()` regardless of whether help is shown -- must
    /// not bump `adapters::READINESS_NOTE_CALLS`.
    #[test]
    fn hook_argv_does_not_invoke_the_readiness_probe() {
        let hook_argv = vec!["ctx".to_string(), "hook".to_string(), "pretool".to_string()];
        assert!(
            !ctx_will_render_help(&hook_argv),
            "a hook verb must never be classified as a help render"
        );

        let calls_before =
            adapters::READINESS_NOTE_CALLS.load(std::sync::atomic::Ordering::Relaxed);
        let cli = CtxCli::try_parse_from(["zirv ctx", "hook", "pretool"])
            .expect("hook pretool should parse");
        assert!(matches!(cli.verb, CtxVerb::Hook(_)));
        assert_eq!(
            adapters::READINESS_NOTE_CALLS.load(std::sync::atomic::Ordering::Relaxed),
            calls_before,
            "parsing a hook argv must not walk PATH probing adapter readiness"
        );
    }

    /// The other half of the same guard: `zirv ctx --help` (and the no-verb
    /// and bare-`help` shapes) must still open the readiness gate and still
    /// carry the note, so the fix above never regresses `--help`'s own
    /// output. `SHOW_READINESS_NOTE` only ever moves false -> true, so
    /// asserting it is set is enough to know `ctx_about()` recomputes with
    /// the probe the next time it is read (the property test above already
    /// pins the exact wording).
    #[test]
    fn help_argv_still_opens_the_readiness_note_gate() {
        for help_argv in [
            vec!["ctx".to_string()],
            vec!["ctx".to_string(), "help".to_string()],
            vec!["ctx".to_string(), "-h".to_string()],
            vec!["ctx".to_string(), "--help".to_string()],
        ] {
            assert!(
                ctx_will_render_help(&help_argv),
                "{help_argv:?} must be classified as a help render"
            );
        }

        SHOW_READINESS_NOTE.store(true, std::sync::atomic::Ordering::Relaxed);
        let about = ctx_about();
        assert!(
            about.starts_with("Autonomous context management for AI coding agents."),
            "got {about}"
        );
        assert_ne!(
            about, "Autonomous context management for AI coding agents.",
            "with the gate open, about must carry the readiness note, not the bare sentence: {about}"
        );

        // A subcommand's own `--help` (e.g. `zirv ctx hook --help`) renders
        // that subcommand's help, not `CtxCli`'s `about` -- so it must not be
        // classified as a top-level help render.
        assert!(!ctx_will_render_help(&[
            "ctx".to_string(),
            "hook".to_string(),
            "--help".to_string(),
        ]));
    }

    #[test]
    fn parses_score_verb() {
        let cli = CtxCli::try_parse_from(["zirv ctx", "score", "--transcript", "/tmp/t.jsonl"])
            .expect("score should parse");
        match cli.verb {
            CtxVerb::Score(args) => {
                assert_eq!(args.transcript, std::path::PathBuf::from("/tmp/t.jsonl"));
                assert_eq!(args.agent, None);
            }
            other => panic!("expected Score, got {other:?}"),
        }
    }

    #[test]
    fn loop_verb_keeps_its_cli_name() {
        let cli = CtxCli::try_parse_from(["zirv ctx", "loop", "--prompt", "go"])
            .expect("loop should parse");
        assert!(matches!(cli.verb, CtxVerb::Loop(_)));
    }

    /// Issue #285: `zirv ctx objective set|show|close` all parse.
    #[test]
    fn objective_verb_parses_set_show_and_close() {
        let cli = CtxCli::try_parse_from(["zirv ctx", "objective", "set", "ship the thing"])
            .expect("objective set should parse");
        match cli.verb {
            CtxVerb::Objective(a) => match a.command {
                objective::ObjectiveVerb::Set(set) => {
                    assert_eq!(set.objective, "ship the thing");
                }
                other => panic!("expected Set, got {other:?}"),
            },
            other => panic!("expected Objective, got {other:?}"),
        }

        let cli = CtxCli::try_parse_from(["zirv ctx", "objective", "show"])
            .expect("objective show should parse");
        assert!(matches!(
            cli.verb,
            CtxVerb::Objective(objective::ObjectiveArgs {
                command: objective::ObjectiveVerb::Show(_)
            })
        ));

        let cli = CtxCli::try_parse_from(["zirv ctx", "objective", "close"])
            .expect("objective close should parse");
        assert!(matches!(
            cli.verb,
            CtxVerb::Objective(objective::ObjectiveArgs {
                command: objective::ObjectiveVerb::Close(_)
            })
        ));
    }

    /// Issue #310 (3c): `zirv ctx ask <session> "<question>"` parses, and its
    /// `--json` flag defaults off.
    #[test]
    fn ask_verb_parses_session_and_question() {
        let cli = CtxCli::try_parse_from(["zirv ctx", "ask", "abc", "what is the worker doing"])
            .expect("ask should parse");
        match cli.verb {
            CtxVerb::Ask(a) => {
                assert_eq!(a.session, "abc");
                assert_eq!(a.question, "what is the worker doing");
                assert!(!a.json);
            }
            other => panic!("expected Ask, got {other:?}"),
        }
    }

    /// `exec`'s own flags come before `--`, the headless agent command after.
    /// This exercises real argv parsing (not a struct literal), which is the
    /// only way clap's `trailing_var_arg` + `last` debug assertion is checked:
    /// a bad attribute combination on `ExecArgs::command` panics here instead
    /// of surfacing as a normal parse error, taking the whole process down.
    #[test]
    fn exec_verb_parses_own_flags_before_the_separator_and_command_after() {
        let cli = CtxCli::try_parse_from([
            "zirv ctx",
            "exec",
            "--agent",
            "claude",
            "--max-restarts",
            "2",
            "--",
            "claude",
            "-p",
            "hi",
        ])
        .expect("exec should parse flags before -- and a command after it");
        match cli.verb {
            CtxVerb::Exec(args) => {
                assert_eq!(args.agent, Some("claude".to_string()));
                assert_eq!(args.max_restarts, Some(2));
                assert_eq!(
                    args.command,
                    vec!["claude".to_string(), "-p".to_string(), "hi".to_string()]
                );
            }
            other => panic!("expected Exec, got {other:?}"),
        }
    }

    /// Issue #326, the same hazard `exec_verb_parses_own_flags_before_the_
    /// separator_and_command_after` above exists for: `run`'s own flags come
    /// before `--`, the command after, and only real argv parsing checks
    /// clap's `trailing_var_arg` + `last` debug assertion -- a bad attribute
    /// combination PANICS the whole process here rather than surfacing as a
    /// parse error, which is exactly how this was caught the first time.
    #[test]
    fn run_verb_parses_own_flags_before_the_separator_and_command_after() {
        let cli = CtxCli::try_parse_from([
            "zirv ctx",
            "run",
            "--compact",
            "--",
            "cargo",
            "test",
            "--",
            "--test-threads=1",
        ])
        .expect("run should parse flags before -- and a command after it");
        match cli.verb {
            CtxVerb::Run(args) => {
                assert!(args.compact);
                assert!(!args.full);
                assert_eq!(
                    args.command,
                    vec![
                        "cargo".to_string(),
                        "test".to_string(),
                        "--".to_string(),
                        "--test-threads=1".to_string()
                    ]
                );
            }
            other => panic!("expected Run, got {other:?}"),
        }
    }

    /// Issue #326: the retrieval surface parses as printed on the summary's
    /// own last line.
    #[test]
    fn output_verb_parses_show_with_a_range_and_list() {
        let cli =
            CtxCli::try_parse_from(["zirv ctx", "output", "show", "abc123", "--range", "5-9"])
                .expect("output show should parse");
        match cli.verb {
            CtxVerb::Output(a) => match a.command {
                output::OutputVerb::Show(show) => {
                    assert_eq!(show.id, "abc123");
                    assert_eq!(show.range.as_deref(), Some("5-9"));
                }
                other => panic!("expected Show, got {other:?}"),
            },
            other => panic!("expected Output, got {other:?}"),
        }

        let cli = CtxCli::try_parse_from(["zirv ctx", "output", "list"])
            .expect("output list should parse");
        assert!(matches!(
            cli.verb,
            CtxVerb::Output(output::OutputArgs {
                command: output::OutputVerb::List(_)
            })
        ));
    }

    /// Issue #422: `savings` defaults `--since` to `7d` and parses
    /// `--project` as a bare flag.
    #[test]
    fn savings_verb_parses_with_its_default_since_and_the_project_flag() {
        let cli = CtxCli::try_parse_from(["zirv ctx", "savings"]).expect("savings should parse");
        match cli.verb {
            CtxVerb::Savings(a) => {
                assert_eq!(a.since, "7d");
                assert!(!a.project);
            }
            other => panic!("expected Savings, got {other:?}"),
        }

        let cli = CtxCli::try_parse_from(["zirv ctx", "savings", "--since", "24h", "--project"])
            .expect("savings --since --project should parse");
        match cli.verb {
            CtxVerb::Savings(a) => {
                assert_eq!(a.since, "24h");
                assert!(a.project);
            }
            other => panic!("expected Savings, got {other:?}"),
        }
    }

    /// Issue #267: `--mode` unstated defaults to `Writing` -- a wrong
    /// `read-only` silently drops real edits, which is worse than a wrong
    /// `writing` holding a writer-permit slot it did not need.
    #[test]
    fn agent_verb_mode_defaults_to_writing() {
        let cli = CtxCli::try_parse_from(["zirv ctx", "agent", "claude", "go"])
            .expect("agent should parse with no --mode at all");
        match cli.verb {
            CtxVerb::Agent(args) => {
                assert_eq!(args.mode, permit::WorkerMode::Writing);
                assert!(!args.worktree);
            }
            other => panic!("expected Agent, got {other:?}"),
        }
    }

    /// `--mode read-only` and `--worktree` both parse as ordinary flags
    /// ahead of the trailing `-- <flags>` separator, the same as every
    /// other `AgentArgs` flag.
    #[test]
    fn agent_verb_parses_mode_and_worktree() {
        let cli = CtxCli::try_parse_from([
            "zirv ctx",
            "agent",
            "claude",
            "go",
            "--mode",
            "read-only",
            "--worktree",
        ])
        .expect("agent should parse --mode and --worktree");
        match cli.verb {
            CtxVerb::Agent(args) => {
                assert_eq!(args.mode, permit::WorkerMode::ReadOnly);
                assert!(args.worktree);
            }
            other => panic!("expected Agent, got {other:?}"),
        }
    }

    /// An unrecognised `--mode` value is a clap parse error, not a silent
    /// fallback to the default -- the same discipline every other `value_
    /// enum` flag in this codebase holds.
    #[test]
    fn agent_verb_rejects_an_unknown_mode() {
        let err =
            CtxCli::try_parse_from(["zirv ctx", "agent", "claude", "go", "--mode", "readonly"])
                .expect_err("an unrecognised --mode spelling must not silently parse");
        assert!(
            err.to_string().contains("mode"),
            "the error should name the offending flag: {err}"
        );
    }

    /// The trailing command can itself contain flag-shaped tokens (`--session-id`,
    /// `-p`) that must land in `command` verbatim, not be consumed as `exec`'s
    /// own flags: they appear after the `--` separator.
    #[test]
    fn exec_verb_preserves_hyphen_values_inside_the_trailing_command() {
        let cli = CtxCli::try_parse_from([
            "zirv ctx",
            "exec",
            "--timeout-secs",
            "30",
            "--",
            "claude",
            "--session-id",
            "abc",
            "-p",
            "hi",
        ])
        .expect("hyphen-prefixed values after -- must not be reparsed as exec flags");
        match cli.verb {
            CtxVerb::Exec(args) => {
                assert_eq!(args.timeout_secs, Some(30));
                assert_eq!(
                    args.command,
                    vec![
                        "claude".to_string(),
                        "--session-id".to_string(),
                        "abc".to_string(),
                        "-p".to_string(),
                        "hi".to_string()
                    ]
                );
            }
            other => panic!("expected Exec, got {other:?}"),
        }
    }

    /// `wrap`'s own flags come before `--`, the interactive agent command after.
    /// This exercises real argv parsing (not a struct literal), which is the
    /// only way clap's `trailing_var_arg` + `last` debug assertion is checked:
    /// a bad attribute combination on `WrapArgs::command` panics here instead
    /// of surfacing as a normal parse error, taking the whole process down.
    /// (`ExecArgs::command` hit exactly this bug; see d3f0ede.)
    #[test]
    fn wrap_verb_parses_own_flags_before_the_separator_and_command_after() {
        let cli = CtxCli::try_parse_from([
            "zirv ctx", "wrap", "--agent", "claude", "--", "claude", "-p", "hi",
        ])
        .expect("wrap should parse flags before -- and a command after it");
        match cli.verb {
            CtxVerb::Wrap(args) => {
                assert_eq!(args.agent, Some("claude".to_string()));
                assert!(!args.no_supervise);
                assert_eq!(
                    args.command,
                    vec!["claude".to_string(), "-p".to_string(), "hi".to_string()]
                );
            }
            other => panic!("expected Wrap, got {other:?}"),
        }
    }

    /// The trailing command can itself contain flag-shaped tokens (`--session-id`,
    /// `-p`) that must land in `command` verbatim, not be consumed as `wrap`'s
    /// own flags: they appear after the `--` separator.
    #[test]
    fn wrap_verb_preserves_hyphen_values_inside_the_trailing_command() {
        let cli = CtxCli::try_parse_from([
            "zirv ctx",
            "wrap",
            "--no-supervise",
            "--",
            "claude",
            "--session-id",
            "abc",
            "-p",
            "hi",
        ])
        .expect("hyphen-prefixed values after -- must not be reparsed as wrap flags");
        match cli.verb {
            CtxVerb::Wrap(args) => {
                assert!(args.no_supervise);
                assert_eq!(
                    args.command,
                    vec![
                        "claude".to_string(),
                        "--session-id".to_string(),
                        "abc".to_string(),
                        "-p".to_string(),
                        "hi".to_string()
                    ]
                );
            }
            other => panic!("expected Wrap, got {other:?}"),
        }
    }

    /// `--extra` carries the agent's own flags, which are hyphen-shaped almost
    /// by definition. Same clap bug class as the `ExecArgs::command` fix.
    #[test]
    fn loop_and_resume_accept_hyphen_shaped_extra_values() {
        let cli = CtxCli::try_parse_from([
            "zirv ctx", "loop", "--prompt", "go", "--extra", "--model", "--extra", "opus",
        ])
        .expect("loop --extra must accept the agent's own flags");
        match cli.verb {
            CtxVerb::Loop(args) => {
                assert_eq!(args.extra, vec!["--model".to_string(), "opus".to_string()])
            }
            other => panic!("expected Loop, got {other:?}"),
        }

        let cli = CtxCli::try_parse_from(["zirv ctx", "resume", "--extra", "--continue"])
            .expect("resume --extra must accept the agent's own flags");
        match cli.verb {
            CtxVerb::Resume(args) => assert_eq!(args.extra, vec!["--continue".to_string()]),
            other => panic!("expected Resume, got {other:?}"),
        }
    }

    /// Issue #317: `zirv ctx task create/claim/...` and `zirv ctx swarm` both
    /// parse.
    #[test]
    fn task_and_swarm_verbs_parse() {
        let cli = CtxCli::try_parse_from([
            "zirv ctx",
            "task",
            "create",
            "do the thing",
            "--brief",
            "do it well",
        ])
        .expect("task create should parse");
        match cli.verb {
            CtxVerb::Task(a) => match a.command {
                task::TaskVerb::Create(create) => {
                    assert_eq!(create.title, "do the thing");
                    assert_eq!(create.brief, "do it well");
                }
                other => panic!("expected Create, got {other:?}"),
            },
            other => panic!("expected Task, got {other:?}"),
        }

        let cli = CtxCli::try_parse_from(["zirv ctx", "task", "claim", "task-1"])
            .expect("task claim should parse");
        assert!(matches!(
            cli.verb,
            CtxVerb::Task(task::TaskArgs {
                command: task::TaskVerb::Claim(_)
            })
        ));

        let cli = CtxCli::try_parse_from(["zirv ctx", "swarm", "ship it", "--workers", "3"])
            .expect("swarm should parse");
        match cli.verb {
            CtxVerb::Swarm(a) => {
                assert_eq!(a.scope, "ship it");
                assert_eq!(a.workers, 3);
            }
            other => panic!("expected Swarm, got {other:?}"),
        }
    }

    /// Issue #720: `zirv ctx reconcile` parses with `--dry-run`/`--json`
    /// both unset by default, and with both flags together.
    #[test]
    fn reconcile_verb_parses_with_and_without_its_flags() {
        let cli =
            CtxCli::try_parse_from(["zirv ctx", "reconcile"]).expect("reconcile should parse");
        match cli.verb {
            CtxVerb::Reconcile(a) => {
                assert!(!a.dry_run);
                assert!(!a.json);
            }
            other => panic!("expected Reconcile, got {other:?}"),
        }

        let cli = CtxCli::try_parse_from(["zirv ctx", "reconcile", "--dry-run", "--json"])
            .expect("reconcile --dry-run --json should parse");
        match cli.verb {
            CtxVerb::Reconcile(a) => {
                assert!(a.dry_run);
                assert!(a.json);
            }
            other => panic!("expected Reconcile, got {other:?}"),
        }
    }

    #[test]
    fn unknown_verb_exits_two() {
        let code = dispatch(&["ctx".to_string(), "nope".to_string()]);
        assert_eq!(code, 2, "clap parse failure must map to exit code 2");
    }

    /// Bug (2026-08-02 validation of 2.5.0): clap represents `--help` as an
    /// `Err(...)` from `try_parse_from` (printing and exiting is the caller's
    /// job), and `dispatch` collapsed every parse failure to `classify_parse_
    /// failure`'s verdict, which only special-cases `hook` and `usage tee`.
    /// Every other verb's `--help` exited 2 instead of 0, breaking scripts
    /// that treat `--help` as success. Top-level `zirv --help` was never
    /// affected: it goes through `Parser::parse()`, which exits correctly on
    /// its own before any of this code runs.
    #[test]
    fn help_exits_zero_on_every_verb_and_bare_ctx() {
        for argv in [
            vec!["ctx", "--help"],
            vec!["ctx", "score", "--help"],
            vec!["ctx", "optimize", "--help"],
            vec!["ctx", "usage", "--help"],
            vec!["ctx", "status", "--help"],
            vec!["ctx", "wrap", "--help"],
            vec!["ctx", "exec", "--help"],
            vec!["ctx", "handoff", "--help"],
            vec!["ctx", "resume", "--help"],
            vec!["ctx", "loop", "--help"],
            vec!["ctx", "wrap", "-h"],
            vec!["ctx", "hook", "--help"],
            vec!["ctx", "ask", "--help"],
        ] {
            let args: Vec<String> = argv.iter().map(|a| (*a).to_string()).collect();
            assert_eq!(dispatch(&args), 0, "--help must exit 0: {argv:?}");
        }
    }

    /// The invariant is "a hook always exits 0", and clap's own error path is
    /// part of the hook: exit 2 from a Stop hook blocks the agent's stop.
    #[test]
    fn a_hook_invocation_clap_rejects_still_exits_zero() {
        for argv in [
            vec!["ctx", "hook", "Stop"],
            vec!["ctx", "hook", "stop", "--bogus"],
            vec!["ctx", "hook", "notify", "-x"],
            vec!["ctx", "hook"],
        ] {
            let args: Vec<String> = argv.iter().map(|a| (*a).to_string()).collect();
            assert_eq!(
                dispatch(&args),
                0,
                "a hook must never block the agent: {argv:?}"
            );
        }
    }

    #[test]
    fn a_rejected_statusline_tee_still_exits_zero() {
        let args: Vec<String> = ["ctx", "usage", "tee", "--bogus"]
            .iter()
            .map(|a| (*a).to_string())
            .collect();
        assert_eq!(dispatch(&args), 0, "a statusline must never fail loudly");
    }

    #[test]
    fn only_hooks_and_the_statusline_survive_a_parse_failure() {
        let argv =
            |parts: &[&str]| -> Vec<String> { parts.iter().map(|p| (*p).to_string()).collect() };
        assert_eq!(
            classify_parse_failure(&argv(&["ctx", "hook", "Stop"])),
            ParseFailure::Hook
        );
        assert_eq!(
            classify_parse_failure(&argv(&["ctx", "usage", "tee", "--bogus"])),
            ParseFailure::Statusline
        );
        assert_eq!(
            classify_parse_failure(&argv(&["ctx", "usage", "--bogus"])),
            ParseFailure::Reject,
            "only the tee has a statusline to keep alive"
        );
        assert_eq!(
            classify_parse_failure(&argv(&["ctx", "exec", "--bogus"])),
            ParseFailure::Reject
        );
        assert_eq!(
            classify_parse_failure(&argv(&["ctx"])),
            ParseFailure::Reject
        );
    }

    #[test]
    fn ctx_is_intercepted_before_script_lookup() {
        // A repo with .zirv/ctx.toml must still route `zirv ctx ...` to the
        // built-in, never to a YAML/TOML script named "ctx".
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(dir.path().join(".zirv")).expect("mkdir");
        std::fs::write(dir.path().join(".zirv/ctx.toml"), "not = \"a script\"\n").expect("write");

        let exe = std::env::current_exe().expect("current_exe");
        let bin = exe
            .parent()
            .and_then(|p| p.parent())
            .expect("target/debug")
            .join("zirv");

        let out = std::process::Command::new(&bin)
            .args(["ctx", "score", "--help"])
            .current_dir(dir.path())
            .output()
            .expect("run zirv");

        let text = String::from_utf8_lossy(&out.stdout);
        assert!(
            text.contains("--transcript"),
            "built-in ctx help expected, got: {text}"
        );
    }
}
