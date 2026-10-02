//! `zirv ctx wrap`: the interactive supervisor. It spawns the operator's own
//! agent command behind a pty and passes bytes through byte for byte; wrap
//! must never make the session worse than an unwrapped one, so any
//! supervision failure degrades, once and for all, to pure passthrough.
//!
//! Wrap may submit compact/restart actions only at verified idle, and may
//! submit one labelled mail advisory at verified idle. Mail bodies never enter
//! the pty; only the session itself consumes mail with `zirv ctx inbox`.
//! Nudges stay advisory. Mail errors must not degrade the session.

use std::io::{IsTerminal, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use portable_pty::{CommandBuilder, PtySize, native_pty_system};

use super::adapters::AgentAdapter;
use super::announce::{Announcer, Event};
use super::config::{CtxConfig, EnvLookup, env_from_process};
use super::handoff::{self, Handoff};
use super::jev;
use super::jev_relay;
use super::pace;
use super::prompt::PromptRole;
use super::rot::Verdict;
use super::signal::TurnSignal;
use super::state::StateDir;
#[cfg(test)]
use super::supervise::COMPACT_FOCUS;
use super::supervise::{Watcher, compact_prompt, verify_compaction};
use super::term::{RawGuard, STDIN_FD, window_size};
use super::{CtxResult, INJECTION_SUBMIT_DELAY, adapters};

const PUMP_POLL: Duration = Duration::from_millis(100);
const DEFAULT_SIZE: (u16, u16) = (80, 24);
// Matches the grace period `supervise::terminate` already uses for the same
// ask-then-escalate shape.
const QUIT_GRACE: Duration = Duration::from_secs(5);

mod bar;
mod handover_swap;
mod launch;
mod mail_watch;
mod pty_input;
mod pump;
mod relaunch;
mod supervision;

pub use launch::{WrapArgs, run_with};
pub use mail_watch::{MailAction, MailWatch};
pub(in crate::commands::ctx) use pty_input::answer_inherit_cursor_probe;
pub use pty_input::{CprFilter, PasteGuard};
pub(in crate::commands::ctx) use relaunch::apply_interactive_gate;
pub use relaunch::{inject_compact, note_failure, quit_child, restart_prompt};
pub use supervision::{
    Action, InjectionState, PumpEvent, SOCKET_PATH_PREFIX, TRANSCRIPT_ENV, TranscriptSource,
    action_for, handover_may_act, mail_inject_ready, publish_socket_path, signal_less_mail_ready,
    unpublish_socket_path,
};
#[cfg(test)]
pub use supervision::{SOCKET_PATH_FILE, read_socket_path, socket_path_file_for};

use super::*;
use bar::*;
use handover_swap::*;
use mail_watch::*;
#[cfg(unix)]
use pty_input::stdin_ready;
use pump::*;
use relaunch::*;

/// `Orchestrator`, not `Worker`: passing `Worker` would inject
/// worker-conventions text and silently drop the operator's own
/// `~/.zirv/system-prompt.md` -- a session made worse by being wrapped,
/// which is the one thing wrap must never do. (#249)
pub fn run<W: Write>(args: &WrapArgs, _w: &mut W) -> CtxResult<i32> {
    let repo = std::env::current_dir()?;
    let ambient = env_from_process();
    // A direct wrap launch cannot trust inherited parent-session identity;
    // scrub it, or an unrelated sender's mail could be marked as steering
    // in this session's own advisory line. (#249/#250)
    let env = super::agent::parent_session_env(&ambient, None);
    run_with(
        args,
        &repo,
        &env,
        PromptRole::Orchestrator,
        None,
        super::sessions::Verb::Wrap,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    // Every pty-driven test is unix-only: the supervision it exercises needs a
    // unix socket for turn signals and raw mode for passthrough.
    #[cfg(unix)]
    use portable_pty::{CommandBuilder, PtySize, native_pty_system};

    #[cfg(unix)]
    use std::io::Read;
    use std::path::PathBuf;
    use std::time::{Duration, Instant};

    #[cfg(any(unix, windows))]
    pub(crate) fn zirv_bin() -> PathBuf {
        // cargo test builds the bin target, so it sits next to the test binary's
        // grandparent directory (target/debug/deps/<test> -> target/debug/zirv).
        std::env::current_exe()
            .expect("current_exe")
            .parent()
            .and_then(|p| p.parent())
            .expect("target dir")
            .join(if cfg!(windows) { "zirv.exe" } else { "zirv" })
    }

    #[cfg(unix)]
    pub(crate) fn fixture(name: &str) -> PathBuf {
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests")
            .join("fixtures")
            .join(name)
    }

    /// Drives `zirv ctx wrap` from inside an outer PTY, which is the only way to
    /// exercise raw-mode passthrough end to end.
    #[cfg(unix)]
    pub(crate) struct Harness {
        pub reader: ChunkReader,
        pub writer: Box<dyn Write + Send>,
        pub child: Box<dyn portable_pty::Child + Send + Sync>,
        /// C9: keeps the throwaway state/HOME directory alive for as long as
        /// the wrapped subprocess is. Dropped with the harness, which is
        /// after the child has been waited on in every test that uses one.
        _sandbox: tempfile::TempDir,
    }

    /// The general form: `flags` are wrap's own arguments, inserted before the
    /// `--` separator. `spawn_wrap` below is the common case (`--agent claude`)
    /// most tests want; this exists for the tests that need to vary wrap's own
    /// flags (`--no-supervise`, an omitted `--agent`, ...).
    #[cfg(unix)]
    pub(crate) fn spawn_wrap_with_flags(
        extra_env: &[(&str, String)],
        flags: &[&str],
        wrapped: &[&str],
    ) -> Harness {
        spawn_wrap_with_flags_in(
            &std::env::current_dir().expect("cwd"),
            extra_env,
            flags,
            wrapped,
        )
    }

    /// I2 regression (2026-08-23): as of the zirv-managed context migration
    /// this checkout carries its own committed `.zirv/context/{common,claude,
    /// codex}.md` and `.zirv/memory/*.md`. `spawn_wrap_with_flags` pins the
    /// spawned child's cwd to *this* process's cwd (see the comment below) so
    /// mail-slug tests agree on "repo" -- but during `cargo test` that cwd
    /// *is* the checkout root, so any test asserting the exact composed
    /// prompt now picks up the checkout's own repo layers on top of the
    /// built-in default, not just what it explicitly wrote into a fixture.
    /// This variant takes an explicit `cwd` so a prompt-composition test can
    /// point the child at an isolated, unrelated temp directory instead --
    /// keeping the mail-slug tests (which still want cwd == this process's
    /// cwd) on `spawn_wrap_with_flags` unchanged.
    #[cfg(unix)]
    pub(crate) fn spawn_wrap_with_flags_in(
        cwd: &std::path::Path,
        extra_env: &[(&str, String)],
        flags: &[&str],
        wrapped: &[&str],
    ) -> Harness {
        let pair = native_pty_system()
            .openpty(PtySize {
                rows: 24,
                cols: 80,
                pixel_width: 0,
                pixel_height: 0,
            })
            .expect("openpty");

        let mut cmd = CommandBuilder::new(zirv_bin());
        crate::commands::ctx::testenv::scrub_operator_profile_env_for_test(&mut cmd);
        cmd.arg("ctx");
        cmd.arg("wrap");
        for flag in flags {
            cmd.arg(flag);
        }
        cmd.arg("--");
        for arg in wrapped {
            cmd.arg(arg);
        }
        cmd.env("TERM", "xterm");
        // `zirv ctx wrap`'s own `run()` derives its "repo" from
        // `std::env::current_dir()` of the wrap process itself -- the same
        // call the test-side helpers (`store_mail_for_cwd`, `repo_slug`
        // lookups) make from *this* process to compute the slug they file
        // and expect mail under. Without pinning it here, portable-pty's
        // `CommandBuilder` (see the HOME comment just below) falls back to
        // `chdir`-ing into HOME instead, so the two processes silently
        // disagree on "repo": the spawned `wrap` never finds mail the test
        // stored, and a test waiting on the mail advisory hangs rather than
        // failing loudly, since nothing further ever arrives on the pty.
        // Callers that don't need slug agreement (no mail involved) can pass
        // an isolated `cwd` instead -- see `spawn_wrap_with_flags_in` above.
        cmd.cwd(cwd);
        // C9: a throwaway state dir and HOME by default. These tests spawn a
        // real `zirv ctx wrap`, which resolves its state dir from the
        // environment -- so without this they registered sessions, published
        // socket paths, wrote decision logs and dropped handoffs straight
        // into the developer's own `~/.zirv`/state directory, where a later
        // real session would then read them. Applied *before* `extra_env`,
        // so a test that pins its own `ZIRV_CTX_STATE_DIR` (most of them do,
        // because they read it back) still wins.
        let sandbox = tempfile::tempdir().expect("tempdir");
        cmd.env(
            crate::commands::ctx::state::STATE_ENV,
            sandbox.path().join("state").display().to_string(),
        );
        // The directory has to actually exist, not just be a plausible
        // path. Historically this was load-bearing for the spawn itself:
        // with no `.cwd()` set, portable-pty's CommandBuilder falls back to
        // `chdir`-ing into `HOME` before exec (see `as_command` in its
        // `cmdbuilder.rs`), and a HOME that only existed on paper made that
        // chdir fail with ENOENT -- surfaced by `spawn_command` as "No such
        // file or directory", indistinguishable at a glance from the
        // wrapped binary itself being missing. The `.cwd()` pin above now
        // takes precedence over that fallback, but the spawned `zirv` still
        // resolves real paths under HOME (`~/.zirv`, state fallbacks), so
        // the sandbox home stays a directory that exists, not a fiction.
        let home_dir = sandbox.path().join("home");
        std::fs::create_dir_all(&home_dir).expect("create sandbox home");
        for home in ["HOME", "USERPROFILE"] {
            cmd.env(home, home_dir.display().to_string());
        }
        // Hermetic against the developer's own environment (F2): this spawns
        // the real `zirv` binary, which reads the process environment, so a
        // suite run from inside an agent session would otherwise trip the
        // nesting guard instead of running the test. Removed *before*
        // `extra_env`, which several tests use to pin `ZIRV_CTX_TRANSCRIPT`
        // deliberately. T8: see `testenv::scrub_supervision_env_for_test`'s
        // own doc comment for why this is a test-side scrub, not an extended
        // production one.
        crate::commands::ctx::testenv::scrub_supervision_env_for_test(&mut cmd);
        // T10 fix regression (2026-08-23): every one of these harnesses spawns
        // a *real* `zirv ctx wrap` attached to a real pty on both stdin and
        // stdout, which is exactly the condition the launch-time interactive
        // pacing gate (`pace::interactive_gate`, applied in `run_with` right
        // before spawn) is gated on. With a fresh, isolated state dir (no
        // usage source has ever been observed) that gate resolves to a blind
        // `pace.blind_delay_secs` (60s) pause on every single test, which is
        // both why this suite took 31 minutes in CI and why the pty output
        // these tests assert on exactly (argv, prompt text, `--sandbox`/
        // `--ask-for-approval` flags) had the pause's own banner text mixed
        // into it -- `read_until`'s fixed budget elapses mid-pause and the
        // assertions see nothing, or the wrong thing. Neither the pacing gate
        // nor the shipped sandbox posture is what these tests exist to cover
        // (`force_pace_skips_the_wait_or_confirmation_for_both_pause_and_
        // refuse` and `a_supervised_wrap_carries_the_shipped_sandbox_posture_
        // for_codex` are, and opt back in explicitly via `extra_env`, applied
        // after these two and so free to override them), so both are off by
        // default here -- the same `base_env`-defaults-it-once shape
        // `exec.rs`/`run_loop.rs` already use to zero their own blind delay,
        // just disabling the gate outright rather than zeroing its delay,
        // since this harness spawns a real subprocess and cannot inject a
        // `FakeClock`.
        cmd.env("ZIRV_CTX_PACE", "false");
        cmd.env("ZIRV_CTX_SANDBOX", "false");
        for (key, value) in extra_env {
            cmd.env(key, value);
        }

        let child = pair.slave.spawn_command(cmd).expect("spawn wrap");
        drop(pair.slave);
        Harness {
            reader: ChunkReader::spawn(pair.master.try_clone_reader().expect("reader")),
            writer: pair.master.take_writer().expect("writer"),
            child,
            _sandbox: sandbox,
        }
    }

    #[cfg(unix)]
    pub(crate) fn spawn_wrap(extra_env: &[(&str, String)], wrapped: &[&str]) -> Harness {
        spawn_wrap_with_flags(extra_env, &["--agent", "claude"], wrapped)
    }

    /// Bounds `read_until`'s own blocking risk (issue #118): a raw pty
    /// reader's `.read()` call can block indefinitely once the child stops
    /// writing (a wedged stub, a hung child), and `read_until` used to check
    /// its own deadline only *between* calls to `.read()` -- so a single
    /// hung read blocked past the whole budget, turning a bounded assertion
    /// failure into CI's 180s hard kill instead of a fast, informative one.
    /// A dedicated thread owns the real reader and only ever talks back
    /// through a channel, so `read_until`'s own loop can bound its wait with
    /// `recv_timeout` regardless of what the blocking read on the other end
    /// is doing. The thread exits on its own once the read errors, hits
    /// EOF, or its send finds nobody left listening; if the read itself is
    /// permanently wedged, the thread simply outlives the test along with
    /// it, exactly as a direct blocking read would have.
    #[cfg(unix)]
    pub(crate) struct ChunkReader {
        rx: mpsc::Receiver<Vec<u8>>,
    }

    #[cfg(unix)]
    impl ChunkReader {
        pub(crate) fn spawn(mut reader: Box<dyn Read + Send>) -> Self {
            let (tx, rx) = mpsc::channel();
            std::thread::spawn(move || {
                let mut buf = [0u8; 1024];
                loop {
                    match reader.read(&mut buf) {
                        Ok(0) => break,
                        Ok(n) => {
                            if tx.send(buf[..n].to_vec()).is_err() {
                                break;
                            }
                        }
                        Err(_) => break,
                    }
                }
            });
            Self { rx }
        }
    }

    /// Reads until `needle` appears or the timeout expires. Bounded by
    /// `ChunkReader`'s own background thread: this loop only ever blocks on
    /// `recv_timeout` against the remaining budget, so a reader that never
    /// produces data still returns -- with whatever text had already
    /// accumulated -- within `timeout` rather than hanging past it.
    #[cfg(unix)]
    pub(crate) fn read_until(reader: &mut ChunkReader, needle: &str, timeout: Duration) -> String {
        let deadline = Instant::now() + timeout;
        let mut seen = String::new();
        loop {
            let now = Instant::now();
            if now >= deadline {
                break;
            }
            match reader.rx.recv_timeout(deadline - now) {
                Ok(chunk) => {
                    seen.push_str(&String::from_utf8_lossy(&chunk));
                    if seen.contains(needle) {
                        return seen;
                    }
                }
                Err(_) => break,
            }
        }
        seen
    }

    /// Issue #118: proves `read_until` cannot block past its own budget even
    /// against a reader that never produces data and never hits EOF -- the
    /// exact shape of a wedged child. A `UnixStream::pair()` gives a reader
    /// end and a write end that is simply held open and never written to or
    /// dropped, so `ChunkReader`'s background thread sits blocked in its own
    /// `.read()` for as long as the test runs, exactly like a hung real pty
    /// read would. Before this fix that block was `read_until`'s own: this
    /// test would have hung for the full budget (and, against a real hung
    /// child, past it) rather than returning.
    #[cfg(unix)]
    #[test]
    fn read_until_returns_within_its_budget_against_a_reader_that_never_produces_data() {
        let (never_writes, _held_open) =
            std::os::unix::net::UnixStream::pair().expect("socketpair");
        let mut reader = ChunkReader::spawn(Box::new(never_writes));
        let budget = Duration::from_secs(1);

        let started = Instant::now();
        let seen = read_until(&mut reader, "anything", budget);
        let elapsed = started.elapsed();

        assert_eq!(seen, "", "nothing was ever written");
        assert!(
            elapsed < budget + Duration::from_millis(500),
            "must return within budget plus a small epsilon, took {elapsed:?}"
        );
    }

    /// Finds `flag`'s value in a space-joined argv rendering (`stub-tui.sh`
    /// prints `argv: %s` from `"$*"`), on the assumption the value itself
    /// has no whitespace -- true for a prompt-file path under a state dir
    /// that is itself a plain tempdir.
    #[cfg(unix)]
    pub(crate) fn flag_value<'a>(seen: &'a str, flag: &str) -> Option<&'a str> {
        let mut tokens = seen.split_whitespace();
        while let Some(token) = tokens.next() {
            if token == flag {
                return tokens.next();
            }
        }
        None
    }

    #[cfg(unix)]
    #[test]
    fn flag_value_finds_the_token_right_after_the_flag() {
        let seen = "argv: --append-system-prompt-file /tmp/x/prompts/abc.md\r\nstub-tui ready\r\n";
        assert_eq!(
            flag_value(seen, "--append-system-prompt-file"),
            Some("/tmp/x/prompts/abc.md")
        );
        assert_eq!(flag_value(seen, "--session-id"), None);
    }

    use crate::commands::ctx::rot::Verdict;
    use crate::commands::ctx::signal::TurnSignal;

    pub(super) fn turn_signal(turn: u64, verdict: Verdict) -> TurnSignal {
        TurnSignal {
            session_id: "s".to_string(),
            turn,
            score: 64,
            verdict,
            transcript_path: None,
        }
    }

    /// The shape a real Stop hook sends: the verdict plus the file the agent
    /// is actually writing.
    #[cfg(unix)]
    fn turn_signal_for(turn: u64, verdict: Verdict, transcript: &std::path::Path) -> TurnSignal {
        TurnSignal {
            transcript_path: Some(transcript.display().to_string()),
            ..turn_signal(turn, verdict)
        }
    }

    /// Records each `write_all` call as its own chunk, shared via an inner
    /// `Arc<Mutex<_>>` so a clone can be boxed into a
    /// `writer: &Arc<Mutex<Box<dyn Write + Send>>>` (the pump loop's own
    /// type) while the test keeps a handle to inspect what landed --
    /// `dash::pane`'s own `RecordingWriter` test double, mirrored here
    /// because wrap's mail-advisory writes go through that `Arc<Mutex<_>>`
    /// seam rather than a bare `&mut dyn Write`.
    #[derive(Clone, Default)]
    pub(super) struct RecordingWriter {
        pub(super) chunks: std::sync::Arc<std::sync::Mutex<Vec<Vec<u8>>>>,
    }

    impl Write for RecordingWriter {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.chunks.lock().expect("lock").push(buf.to_vec());
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    pub(super) const DEBOUNCE: Duration = Duration::from_secs(3);

    #[cfg(unix)]
    #[test]
    fn a_compact_verdict_at_an_idle_turn_boundary_injects_into_the_tui() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let log = tmp.path().join("injected.log");
        let transcript = tmp.path().join("t.jsonl");
        // A transcript that scores `compact`: marker misses plus tool failures
        // at 165k tokens, which is above the ceiling but below the restart score.
        let mut text = String::new();
        for i in 0..12 {
            text.push_str("{\"type\":\"user\",\"message\":{\"content\":\"go\"}}\n");
            text.push_str("{\"type\":\"user\",\"message\":{\"content\":[{\"type\":\"tool_result\",\"content\":\"r\",\"is_error\":true}]}}\n");
            let block = if i < 2 { "[zirv] ok" } else { "sloppy" };
            text.push_str(&format!(
                "{{\"type\":\"assistant\",\"message\":{{\"content\":[{{\"type\":\"text\",\"text\":\"{block}\"}}],\"usage\":{{\"input_tokens\":165000}}}}}}\n"
            ));
        }
        std::fs::write(&transcript, &text).expect("write");

        let script = fixture("stub-tui.sh").display().to_string();
        let mut h = spawn_wrap(
            &[
                ("STUB_TUI_LOG", log.display().to_string()),
                ("STUB_TUI_TRANSCRIPT", transcript.display().to_string()),
                ("ZIRV_CTX_TRANSCRIPT", transcript.display().to_string()),
                ("ZIRV_CTX_DEBOUNCE_MS", "300".to_string()),
                (
                    "ZIRV_CTX_STATE_DIR",
                    tmp.path().join("state").display().to_string(),
                ),
            ],
            &["sh", &script],
        );
        let _ = read_until(&mut h.reader, "stub-tui ready", Duration::from_secs(10));

        // A turn boundary is what unlocks injection, and the hook is what
        // reports one, so drive it exactly the way the real hook does.
        let socket = read_socket_path(&StateDir::from_root(tmp.path().join("state")), None)
            .expect("wrap must publish its socket path");
        crate::commands::ctx::signal::send(
            std::path::Path::new(socket.trim()),
            &turn_signal(3, Verdict::Compact),
        )
        .expect("send turn signal");

        let seen = read_until(&mut h.reader, "compacted", Duration::from_secs(15));
        assert!(seen.contains("compacted"), "got {seen:?}");

        let injected = std::fs::read_to_string(&log).expect("injection log");
        assert!(injected.contains("/compact"), "got {injected:?}");
        assert!(
            injected.contains("Preserve"),
            "focus text was sent: {injected:?}"
        );
        assert_eq!(
            injected.lines().count(),
            1,
            "cooldown prevents a second injection"
        );

        h.writer.write_all(b"/exit\r").expect("write");
        let _ = h.child.wait();
    }

    // T8/T13: the mail advisory driven end to end through a real wrapped
    // session -- announced on stderr while there is no injection window, and
    // typed into the child as one labelled line once there is.

    #[cfg(unix)]
    fn store_mail_for_cwd(state_root: &std::path::Path, body: &str) {
        let repo = std::env::current_dir().expect("cwd");
        let state = crate::commands::ctx::state::StateDir::from_root(state_root.to_path_buf());
        let slug = crate::commands::ctx::state::repo_slug(&repo);
        crate::commands::ctx::mail::store(
            &state,
            &slug,
            &crate::commands::ctx::mail::Message {
                from_session: "other-session".to_string(),
                from_agent: "claude".to_string(),
                to: "any".to_string(),
                to_session: None,
                sent: 1,
                body: body.to_string(),
            },
            &CtxConfig::default(),
        )
        .expect("store mail");
    }

    /// T13: with no turn signal reported yet there is no injection window at
    /// all (`may_inject` needs `signals_seen > 0`), so the poll arm falls back
    /// to the announcement channel rather than typing into a child it cannot
    /// prove is idle -- and it keeps the injection owed for a later poll.
    #[cfg(unix)]
    #[test]
    fn mail_with_no_injection_window_yet_is_announced_on_stderr_instead() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = tmp.path().join("state");
        let script = fixture("stub-tui.sh").display().to_string();
        let mut h = spawn_wrap(
            &[
                ("ZIRV_CTX_DEBOUNCE_MS", "300".to_string()),
                ("ZIRV_CTX_STATE_DIR", state.display().to_string()),
            ],
            &["sh", &script],
        );
        let _ = read_until(&mut h.reader, "stub-tui ready", Duration::from_secs(10));

        store_mail_for_cwd(&state, "note");

        // `wrap`'s own stderr shares the outer pty in this harness (there is
        // no separate stderr stream to redirect it to), so it arrives on
        // `h.reader` exactly like the compact/restart tests' own log output
        // does; the point under test is the wording, not the transport.
        let seen = read_until(&mut h.reader, "zirv ctx inbox", Duration::from_secs(10));
        assert!(seen.contains("zirv ctx inbox"), "got {seen:?}");
        assert!(
            !seen.contains("[zirv \u{25b8} mail]"),
            "no turn boundary has been reported, so nothing may be typed into the child: {seen:?}"
        );

        h.writer.write_all(b"/exit\r").expect("write");
        h.writer.flush().expect("flush");
        let _ = read_until(&mut h.reader, "bye", Duration::from_secs(10));
        let _ = h.child.wait();
    }

    /// N4: an interactive session (`wrap`/`chat`) is named on stderr when it
    /// is nudged, and never restarted. Unlike `exec`'s headless worker there
    /// is no relaunch to fold the nudge's own message body into, so that text
    /// has nowhere to reach the pty at all; this pins that directly rather
    /// than only by absence of an injection call.
    ///
    /// T13 narrowed what this promises. A nudge rides on an ordinary mail
    /// message, so the mail poll arm may well type its own labelled advisory
    /// line into the child -- what must never travel is the *guidance body*,
    /// which is what is asserted below.
    #[cfg(unix)]
    #[test]
    fn an_interactive_session_never_receives_a_nudges_own_message_body() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state_root = tmp.path().join("state");
        let script = fixture("stub-tui.sh").display().to_string();
        let mut h = spawn_wrap(
            &[
                ("ZIRV_CTX_DEBOUNCE_MS", "300".to_string()),
                ("ZIRV_CTX_STATE_DIR", state_root.display().to_string()),
            ],
            &["sh", &script],
        );
        let _ = read_until(&mut h.reader, "stub-tui ready", Duration::from_secs(10));

        // Resolved from the registry rather than passed as an empty prefix:
        // there is exactly one live session, the wrap subprocess `spawn_wrap`
        // just started, and F6 makes `zirv ctx nudge` refuse any prefix
        // shorter than four characters -- an empty one most of all.
        //
        // P5: the record's `pid` is the *agent child's* now, not the wrap
        // subprocess's own -- which is still exactly as live here (the
        // stub TUI is up and waiting), so the `Liveness::Live` filter picks
        // it out the same way.
        let repo = std::env::current_dir().expect("cwd");
        let state = super::super::state::StateDir::from_root(state_root.clone());
        let short = super::super::sessions::list(&state)
            .into_iter()
            .find(|(_, liveness)| *liveness == super::super::sessions::Liveness::Live)
            .map(|(record, _)| record.short)
            .expect("the wrap subprocess registered a live session");
        let env: std::collections::HashMap<String, String> = [(
            crate::commands::ctx::state::STATE_ENV.to_string(),
            state_root.display().to_string(),
        )]
        .into();
        let args = crate::commands::ctx::sessions::NudgeArgs {
            prefix: short,
            message: Some("do-not-type-this-guidance".to_string()),
            message_file: None,
        };
        let mut nudge_out = Vec::new();
        let mut stdin = std::io::Cursor::new(Vec::<u8>::new());
        crate::commands::ctx::sessions::run_nudge_with(
            &args,
            &mut nudge_out,
            &repo,
            &|k| env.get(k).cloned(),
            &mut stdin,
        )
        .expect("nudge the live wrap session");

        let socket =
            read_socket_path(&StateDir::from_root(state_root.clone()), None).expect("socket path");
        crate::commands::ctx::signal::send(
            std::path::Path::new(socket.trim()),
            &turn_signal(3, Verdict::Healthy),
        )
        .expect("send turn signal");

        let seen = read_until(&mut h.reader, "nudged by", Duration::from_secs(10));
        assert!(seen.contains("nudged by"), "got {seen:?}");
        assert!(
            !seen.contains("do-not-type-this-guidance"),
            "the nudge's own message body must never reach the wrapped agent's pty: {seen:?}"
        );

        h.writer.write_all(b"/exit\r").expect("write");
        h.writer.flush().expect("flush");
        let _ = read_until(&mut h.reader, "bye", Duration::from_secs(10));
        let _ = h.child.wait();
    }

    /// T13: the whole mail contract, end to end. At a verified-idle turn
    /// boundary the session itself is told -- one labelled advisory line,
    /// typed into the child -- and the two things that must never happen
    /// still never happen: the body does not travel, and the message is not
    /// consumed.
    #[cfg(unix)]
    #[test]
    fn a_wrapped_session_is_advised_of_mail_without_its_body_and_without_consuming_it() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = tmp.path().join("state");
        let script = fixture("stub-tui.sh").display().to_string();
        let mut h = spawn_wrap(
            &[
                ("ZIRV_CTX_DEBOUNCE_MS", "300".to_string()),
                ("ZIRV_CTX_STATE_DIR", state.display().to_string()),
            ],
            &["sh", &script],
        );
        let _ = read_until(&mut h.reader, "stub-tui ready", Duration::from_secs(10));

        store_mail_for_cwd(&state, "do-not-type-this-body");

        let socket =
            read_socket_path(&StateDir::from_root(state.clone()), None).expect("socket path");
        crate::commands::ctx::signal::send(
            std::path::Path::new(socket.trim()),
            &turn_signal(3, Verdict::Healthy),
        )
        .expect("send turn signal");

        // The labelled marker only ever comes from the injected line: the
        // announcement channel's own fallback wording does not carry it.
        let seen = read_until(
            &mut h.reader,
            "[zirv \u{25b8} mail]",
            Duration::from_secs(15),
        );
        assert!(
            seen.contains("[zirv \u{25b8} mail]"),
            "the session itself is told at an idle turn boundary: {seen:?}"
        );
        assert!(seen.contains("zirv ctx inbox"), "advisory fired: {seen:?}");
        assert!(
            !seen.contains("do-not-type-this-body"),
            "the mail body must never be typed into the agent: {seen:?}"
        );

        // Still unread: wrap only advises. `zirv ctx inbox --consume` is
        // what moves a message into read/, and wrap never calls it.
        let repo = std::env::current_dir().expect("cwd");
        let state_dir = crate::commands::ctx::state::StateDir::from_root(state.clone());
        let slug = crate::commands::ctx::state::repo_slug(&repo);
        let unread = crate::commands::ctx::mail::list(&state_dir, &slug, None, None).expect("list");
        assert_eq!(unread.len(), 1, "wrap must never consume mail on its own");

        h.writer.write_all(b"/exit\r").expect("write");
        h.writer.flush().expect("flush");
        let _ = read_until(&mut h.reader, "bye", Duration::from_secs(10));
        let _ = h.child.wait();
    }

    /// Fix 2 (issue #249/#250 review): a direct `zirv ctx wrap` launch (the
    /// bare `run` entry, which reads `env_from_process()`) must not trust an
    /// inherited `PARENT_SESSION_ENV` off its own ambient process env -- only
    /// a supervisor spawn seam (`agent::run_with`'s fold, or dash's
    /// `verified_parent`) may establish parent lineage. `extra_env` here
    /// stands in for whatever this process's own ambient shell might have
    /// carried (e.g. a worker with a real parent running `zirv ctx wrap`
    /// directly rather than through `zirv agent`); end to end through a real
    /// spawned `zirv ctx wrap`, the live mail advisory for a message from
    /// that "parent" must still read as ordinary peer mail, never steering.
    #[cfg(unix)]
    #[test]
    fn a_direct_wrap_launch_ignores_an_inherited_parent_session_env() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = tmp.path().join("state");
        let script = fixture("stub-tui.sh").display().to_string();
        let mut h = spawn_wrap(
            &[
                ("ZIRV_CTX_DEBOUNCE_MS", "300".to_string()),
                ("ZIRV_CTX_STATE_DIR", state.display().to_string()),
                (
                    crate::commands::ctx::agent::PARENT_SESSION_ENV,
                    "grandpar".to_string(),
                ),
            ],
            &["sh", &script],
        );
        let _ = read_until(&mut h.reader, "stub-tui ready", Duration::from_secs(10));

        let repo = std::env::current_dir().expect("cwd");
        let state_dir = crate::commands::ctx::state::StateDir::from_root(state.clone());
        let slug = crate::commands::ctx::state::repo_slug(&repo);
        crate::commands::ctx::mail::store(
            &state_dir,
            &slug,
            &crate::commands::ctx::mail::Message {
                from_session: "grandpar".to_string(),
                from_agent: "claude".to_string(),
                to: "any".to_string(),
                to_session: None,
                sent: 1,
                body: "do-not-type-this-body".to_string(),
            },
            &CtxConfig::default(),
        )
        .expect("store mail");

        let socket =
            read_socket_path(&StateDir::from_root(state.clone()), None).expect("socket path");
        crate::commands::ctx::signal::send(
            std::path::Path::new(socket.trim()),
            &turn_signal(3, Verdict::Healthy),
        )
        .expect("send turn signal");

        let seen = read_until(
            &mut h.reader,
            "[zirv \u{25b8} mail]",
            Duration::from_secs(15),
        );
        assert!(
            seen.contains("[zirv \u{25b8} mail]"),
            "the session itself is told at an idle turn boundary: {seen:?}"
        );
        assert!(
            seen.contains("information, not instruction"),
            "an inherited PARENT_SESSION_ENV from this process's own ambient env must render as \
             ordinary peer mail, never steering: {seen:?}"
        );
        assert!(
            !seen.contains("steering from your supervising session"),
            "must not be marked as steering: {seen:?}"
        );

        h.writer.write_all(b"/exit\r").expect("write");
        h.writer.flush().expect("flush");
        let _ = read_until(&mut h.reader, "bye", Duration::from_secs(10));
        let _ = h.child.wait();
    }

    /// T13: the poll arm runs every `MAIL_POLL`, so "advise once" has to hold
    /// against a clock rather than against turn boundaries. A message already
    /// advised into the session must stay quiet for every later poll and
    /// every later turn.
    #[cfg(unix)]
    #[test]
    fn a_message_already_advised_into_the_session_is_never_advised_again() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = tmp.path().join("state");
        let script = fixture("stub-tui.sh").display().to_string();
        let mut h = spawn_wrap(
            &[
                ("ZIRV_CTX_DEBOUNCE_MS", "300".to_string()),
                ("ZIRV_CTX_STATE_DIR", state.display().to_string()),
            ],
            &["sh", &script],
        );
        let _ = read_until(&mut h.reader, "stub-tui ready", Duration::from_secs(10));

        store_mail_for_cwd(&state, "note");

        let socket_path =
            read_socket_path(&StateDir::from_root(state.clone()), None).expect("socket path");
        let socket = std::path::PathBuf::from(socket_path.trim());

        crate::commands::ctx::signal::send(&socket, &turn_signal(3, Verdict::Healthy))
            .expect("send 1");
        let first = read_until(
            &mut h.reader,
            "[zirv \u{25b8} mail]",
            Duration::from_secs(15),
        );
        assert!(
            first.contains("[zirv \u{25b8} mail]"),
            "the first idle poll advises: {first:?}"
        );

        // The phase boundary, and it has to be exact rather than timed. One
        // advisory shows up on the pty *twice*: the inner pty's own echo of
        // the injected line, and then the stub's `echo: <line>` answer to it.
        // The read above returns at whichever chunk carried the marker -- on a
        // slow runner that is the echo alone -- so the answer is still in
        // flight, and phase 2's own window is where it lands. That leftover,
        // not a second advisory, is what failed this test on CI twice.
        //
        // A sync line typed right after pins the boundary deterministically:
        // the advisory's bytes are already in the child's input queue (its
        // echo is what phase 1 just matched), the stub reads that queue in
        // order, so its answer to the sync line cannot arrive before its
        // answer to the advisory. Reading up to the sync answer therefore
        // consumes every surface of the advisory, at any runner speed.
        //
        // Asserted, not discarded: a read that quietly timed out here would
        // hand phase 2 exactly the leftover this exists to remove, and the
        // failure would then be reported as a bug in the dedupe.
        h.writer.write_all(b"sync-after-advisory\r").expect("write");
        h.writer.flush().expect("flush");
        let synced = read_until(
            &mut h.reader,
            "echo: sync-after-advisory",
            Duration::from_secs(15),
        );
        assert!(
            synced.contains("echo: sync-after-advisory"),
            "the phase boundary must be reached before phase 2 reads: {synced:?}"
        );

        // A second turn boundary, no new mail in between. Long enough for
        // several `MAIL_POLL` ticks to come and go.
        crate::commands::ctx::signal::send(&socket, &turn_signal(4, Verdict::Healthy))
            .expect("send 2");
        std::thread::sleep(MAIL_POLL * 2);
        h.writer.write_all(b"still here\r").expect("write");
        h.writer.flush().expect("flush");
        let after = read_until(&mut h.reader, "echo: still here", Duration::from_secs(10));

        assert!(
            !after.contains("[zirv \u{25b8} mail]"),
            "an already-advised message must not be advised again: {after:?}"
        );
        assert!(
            !after.contains("zirv ctx inbox"),
            "nor fall back to the announcement channel for it: {after:?}"
        );

        h.writer.write_all(b"/exit\r").expect("write");
        h.writer.flush().expect("flush");
        let _ = read_until(&mut h.reader, "bye", Duration::from_secs(10));
        let _ = h.child.wait();
    }

    /// Issue #118, codex variant of the two T13 tests above: for a
    /// `defer_injection_submit` adapter the mail advisory's own submitting
    /// `\r` is written by the pump loop's periodic drain
    /// (`MailWatch::pending_submit_due`), not in the same burst as the text.
    /// `--agent codex` also means no turn signal is ever sent -- codex's own
    /// adapter never registers one (`register_turn_signal` is a no-op) --
    /// so readiness here comes only from `signal_less_mail_ready`'s quiet
    /// window (`ZIRV_CTX_DASH_IDLE_QUIET_MS` below keeps that window short
    /// enough for a test).
    #[cfg(unix)]
    #[test]
    fn a_codex_wraps_deferred_mail_advisory_is_submitted_by_the_pump_loops_own_drain() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = tmp.path().join("state");
        let script = fixture("stub-tui.sh").display().to_string();
        let mut h = spawn_wrap_with_flags(
            &[
                ("ZIRV_CTX_DASH_IDLE_QUIET_MS", "300".to_string()),
                ("ZIRV_CTX_STATE_DIR", state.display().to_string()),
            ],
            &["--agent", "codex"],
            &["sh", &script],
        );
        let _ = read_until(&mut h.reader, "stub-tui ready", Duration::from_secs(10));

        store_mail_for_cwd(&state, "do-not-type-this-body");

        let seen = read_until(
            &mut h.reader,
            "[zirv \u{25b8} mail]",
            Duration::from_secs(15),
        );
        assert!(
            seen.contains("[zirv \u{25b8} mail]"),
            "the advisory text still reaches a signal-less, defer-capable adapter's child: {seen:?}"
        );

        // The advisory's own `[zirv ▸ mail]` marker above lands as soon as
        // phase 1's write is echoed by the child's own pty -- before its
        // deferred `\r` has necessarily been drained. Typing the sync line
        // immediately would race the drain: if it wins, its own `\r`
        // submits the still-open line early and both texts merge into one
        // (proving nothing). Sleeping past the drain's own worst-case
        // latency (`INJECTION_SUBMIT_DELAY` plus one more `PUMP_POLL` tick)
        // makes the ordering deterministic instead.
        std::thread::sleep(INJECTION_SUBMIT_DELAY + PUMP_POLL + Duration::from_millis(300));

        // Proof the drain actually ran, not just that the text arrived:
        // phase 1 alone (`write_mail_advisory_phase1`) carries no `\r`, so
        // the stub's `read -r` cannot complete on it alone. A sync line
        // typed now, after the sleep above, can only have its own `echo:`
        // answer arrive once *some* `\r` reached the child ahead of it in
        // the input queue -- and nothing but the pump loop's own periodic
        // drain could have written that one, since wrap never sends this
        // session a turn signal at all.
        h.writer.write_all(b"sync-after-advisory\r").expect("write");
        h.writer.flush().expect("flush");
        let synced = read_until(
            &mut h.reader,
            "echo: sync-after-advisory",
            Duration::from_secs(15),
        );
        assert!(
            synced.contains("echo: sync-after-advisory"),
            "the deferred `\\r` must have landed for the stub's read -r to reach the sync line: \
             {synced:?}"
        );
        assert!(
            !synced.contains("do-not-type-this-body"),
            "the mail body must never be typed into the agent: {synced:?}"
        );

        h.writer.write_all(b"/exit\r").expect("write");
        h.writer.flush().expect("flush");
        let _ = read_until(&mut h.reader, "bye", Duration::from_secs(10));
        let _ = h.child.wait();
    }

    #[cfg(unix)]
    #[test]
    fn a_mail_directory_that_cannot_be_read_leaves_the_session_untouched() {
        use std::os::unix::fs::PermissionsExt;

        let tmp = tempfile::tempdir().expect("tempdir");
        let state = tmp.path().join("state");
        let script = fixture("stub-tui.sh").display().to_string();
        let mut h = spawn_wrap(
            &[
                ("ZIRV_CTX_DEBOUNCE_MS", "300".to_string()),
                ("ZIRV_CTX_STATE_DIR", state.display().to_string()),
            ],
            &["sh", &script],
        );
        let _ = read_until(&mut h.reader, "stub-tui ready", Duration::from_secs(10));

        let repo = std::env::current_dir().expect("cwd");
        let state_dir = crate::commands::ctx::state::StateDir::from_root(state.clone());
        let slug = crate::commands::ctx::state::repo_slug(&repo);
        let mailbox = state_dir.mail().join(&slug);
        std::fs::create_dir_all(&mailbox).expect("mkdir");
        std::fs::set_permissions(&mailbox, std::fs::Permissions::from_mode(0o000)).expect("chmod");

        let socket =
            read_socket_path(&StateDir::from_root(state.clone()), None).expect("socket path");
        crate::commands::ctx::signal::send(
            std::path::Path::new(socket.trim()),
            &turn_signal(3, Verdict::Healthy),
        )
        .expect("send");

        // The session keeps going, unaffected: no advisory (nothing readable
        // to report), no crash, no degrade.
        h.writer.write_all(b"still here\r").expect("write");
        h.writer.flush().expect("flush");
        let seen = read_until(&mut h.reader, "echo: still here", Duration::from_secs(10));

        std::fs::set_permissions(&mailbox, std::fs::Permissions::from_mode(0o700))
            .expect("chmod back");

        assert!(
            seen.contains("echo: still here"),
            "the session must keep working: {seen:?}"
        );
        assert!(
            !seen.contains("zirv ctx inbox"),
            "nothing readable, so no advisory: {seen:?}"
        );

        let log = std::fs::read_to_string(state.join("logs/decisions.jsonl")).unwrap_or_default();
        assert!(
            !log.contains("\"action\":\"degrade\""),
            "an unreadable mailbox must not degrade the session: {log}"
        );

        h.writer.write_all(b"/exit\r").expect("write");
        h.writer.flush().expect("flush");
        let _ = read_until(&mut h.reader, "bye", Duration::from_secs(10));
        let _ = h.child.wait();
    }

    /// Polls the decision log, which a supervised session writes from another
    /// process, so a test never races it.
    #[cfg(unix)]
    fn wait_for_log(state: &std::path::Path, needle: &str, timeout: Duration) -> String {
        let path = state.join("logs/decisions.jsonl");
        let deadline = Instant::now() + timeout;
        loop {
            let text = std::fs::read_to_string(&path).unwrap_or_default();
            if text.contains(needle) || Instant::now() >= deadline {
                return text;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    /// The end-to-end case every other wrap test masked by pinning
    /// `ZIRV_CTX_TRANSCRIPT`: with nothing pinned, the only way wrap can know
    /// which file to verify the compaction in is the turn signal itself.
    #[cfg(unix)]
    #[test]
    fn the_transcript_is_learned_from_the_turn_signal() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = tmp.path().join("state");
        let log = tmp.path().join("injected.log");
        let transcript = tmp.path().join("t.jsonl");
        std::fs::write(
            &transcript,
            "{\"type\":\"user\",\"message\":{\"content\":\"go\"}}\n",
        )
        .expect("write");

        let script = fixture("stub-tui.sh").display().to_string();
        let mut h = spawn_wrap(
            &[
                ("STUB_TUI_LOG", log.display().to_string()),
                ("STUB_TUI_TRANSCRIPT", transcript.display().to_string()),
                ("ZIRV_CTX_DEBOUNCE_MS", "300".to_string()),
                ("ZIRV_CTX_INJECT_TIMEOUT_MS", "5000".to_string()),
                ("ZIRV_CTX_STATE_DIR", state.display().to_string()),
            ],
            &["sh", &script],
        );
        let _ = read_until(&mut h.reader, "stub-tui ready", Duration::from_secs(10));

        let socket =
            read_socket_path(&StateDir::from_root(state.clone()), None).expect("socket path");
        crate::commands::ctx::signal::send(
            std::path::Path::new(socket.trim()),
            &turn_signal_for(3, Verdict::Compact, &transcript),
        )
        .expect("send turn signal");

        let seen = read_until(&mut h.reader, "compacted", Duration::from_secs(15));
        assert!(seen.contains("compacted"), "got {seen:?}");

        // Not the generic "verb":"wrap" needle: the prompt-injection log entry
        // written at session start also carries that verb, and would satisfy
        // the wait before the compaction outcome this test cares about is
        // ever appended.
        let decisions = wait_for_log(&state, "\"action\":\"inject\"", Duration::from_secs(15));
        assert!(
            decisions.contains("\"action\":\"inject\""),
            "the compaction was verified in the reported transcript: {decisions}"
        );
        assert!(
            !decisions.contains("\"action\":\"degrade\""),
            "a verified injection must not degrade the session: {decisions}"
        );
        assert!(
            decisions.contains(&transcript.display().to_string()),
            "the log names the file the signal reported: {decisions}"
        );

        h.writer.write_all(b"/exit\r").expect("write");
        wait_or_kill(&mut h.child, Duration::from_secs(5));
    }

    /// Same bug class as the exec one fixed in d3f0ede: after a relaunch the
    /// old session's transcript is dead, and the new session reports its own.
    #[cfg(unix)]
    #[test]
    fn a_restart_reads_the_reported_transcript_and_then_follows_the_new_one() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = tmp.path().join("state");

        let first = tmp.path().join("first.jsonl");
        std::fs::write(
            &first,
            concat!(
                r#"{"type":"user","message":{"content":"wire the webhook"}}"#,
                "\n",
                r#"{"type":"assistant","message":{"content":[{"type":"tool_use","id":"a","name":"Read","input":{"file_path":"/work/src/hook.rs"}}],"usage":{"input_tokens":180000}}}"#,
                "\n"
            ),
        )
        .expect("write");

        // Already compacted: the relaunch stub never compacts on its own, so a
        // verified compaction can only mean wrap read this file and not the
        // one the dead session reported.
        let second = tmp.path().join("second.jsonl");
        std::fs::write(
            &second,
            "{\"type\":\"system\",\"subtype\":\"compact_boundary\",\"content\":\"x\"}\n",
        )
        .expect("write");

        let script = fixture("relaunch-stub.sh").display().to_string();
        let mut h = spawn_wrap(
            &[
                ("ZIRV_CTX_DEBOUNCE_MS", "300".to_string()),
                ("ZIRV_CTX_INJECT_TIMEOUT_MS", "5000".to_string()),
                ("ZIRV_CTX_STATE_DIR", state.display().to_string()),
                ("ZIRV_CTX_AGENT_BIN", format!("sh {script}")),
            ],
            &["sh", &script],
        );
        let _ = read_until(&mut h.reader, "stub-tui ready", Duration::from_secs(10));

        let socket_path =
            read_socket_path(&StateDir::from_root(state.clone()), None).expect("socket path");
        let socket = std::path::PathBuf::from(socket_path.trim());
        crate::commands::ctx::signal::send(&socket, &turn_signal_for(5, Verdict::Restart, &first))
            .expect("send");

        let seen = read_until(&mut h.reader, "stub-tui ready", Duration::from_secs(20));
        assert!(seen.contains("stub-tui ready"), "relaunched: {seen:?}");

        let handoffs = walk_md(&state.join("handoffs"));
        assert_eq!(handoffs.len(), 1, "one handoff per restart: {handoffs:?}");
        let note = std::fs::read_to_string(&handoffs[0]).expect("handoff");
        assert!(
            note.contains("wire the webhook"),
            "the handoff was distilled from the reported transcript, not from an empty read: {note}"
        );

        crate::commands::ctx::signal::send(&socket, &turn_signal_for(6, Verdict::Compact, &second))
            .expect("send");

        let decisions = wait_for_log(&state, "\"verdict\":\"compact\"", Duration::from_secs(20));
        let compaction = decisions
            .lines()
            .find(|line| line.contains("\"verdict\":\"compact\""))
            .unwrap_or_else(|| panic!("no compaction decision logged: {decisions}"));
        assert!(
            compaction.contains(&second.display().to_string()),
            "the new session's transcript is the one watched: {compaction}"
        );
        assert!(
            !compaction.contains(&first.display().to_string()),
            "the dead session's transcript must have been forgotten: {compaction}"
        );

        h.writer.write_all(b"/exit\r").expect("write");
        wait_or_kill(&mut h.child, Duration::from_secs(5));
    }

    #[cfg(unix)]
    #[test]
    fn a_restart_verdict_writes_a_handoff_and_relaunches_within_the_same_wrap_session() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = tmp.path().join("state");
        let transcript = tmp.path().join("t.jsonl");
        std::fs::write(
            &transcript,
            concat!(
                r#"{"type":"user","message":{"content":"wire the webhook"}}"#,
                "\n",
                r#"{"type":"assistant","message":{"content":[{"type":"tool_use","id":"a","name":"Read","input":{"file_path":"/work/src/hook.rs"}}],"usage":{"input_tokens":180000}}}"#,
                "\n"
            ),
        )
        .expect("write");

        // relaunch-stub.sh, not stub-tui.sh: on this platform, stub-tui.sh's
        // /compact branch (long JSON literals in an unreached case arm)
        // reproducibly makes the process spawned on the relaunch's fresh pty
        // exit immediately, even with its bracket-tests de-fragilized (see
        // batch10-report.md for the full bisection). This test never sends
        // /compact, so a minimal stub sidesteps the quirk while exercising
        // the identical wrap code path (greet, echo, exit on quit sequence).
        let script = fixture("relaunch-stub.sh").display().to_string();
        let mut h = spawn_wrap(
            &[
                ("ZIRV_CTX_TRANSCRIPT", transcript.display().to_string()),
                ("ZIRV_CTX_DEBOUNCE_MS", "300".to_string()),
                ("ZIRV_CTX_STATE_DIR", state.display().to_string()),
                // Relaunch runs the stub again instead of a real agent.
                ("ZIRV_CTX_AGENT_BIN", format!("sh {script}")),
            ],
            &["sh", &script],
        );
        let _ = read_until(&mut h.reader, "stub-tui ready", Duration::from_secs(10));

        let socket =
            read_socket_path(&StateDir::from_root(state.clone()), None).expect("socket path");
        crate::commands::ctx::signal::send(
            std::path::Path::new(socket.trim()),
            &turn_signal(5, Verdict::Restart),
        )
        .expect("send");

        // A fresh agent greets again through the same outer terminal
        // (h.reader/h.writer never change: the wrap session survives, only
        // the inner pty is replaced). The old agent's own output, including
        // its quit confirmation, is deliberately not forwarded once a
        // restart is underway (see spawn_output_thread) so it can never
        // interleave with the new generation's output on the same stdout;
        // that the old child actually quit is verified via the decision
        // log below instead of by watching for its suppressed text.
        let seen = read_until(&mut h.reader, "stub-tui ready", Duration::from_secs(20));
        assert!(seen.contains("stub-tui ready"), "relaunched: {seen:?}");

        let log = wait_for_log(&state, "\"action\":\"restart\"", Duration::from_secs(10));
        assert!(
            log.contains("\"action\":\"restart\""),
            "old child quit and relaunch succeeded: {log}"
        );

        let handoffs: Vec<_> = walk_md(&state.join("handoffs"));
        assert_eq!(handoffs.len(), 1, "one handoff per restart: {handoffs:?}");
        let note = std::fs::read_to_string(&handoffs[0]).expect("handoff");
        assert!(note.contains("wire the webhook"), "structural task: {note}");

        // The wrap session itself kept running: the user can still type into
        // the new agent through the very same outer pty used before the restart.
        h.writer.write_all(b"still here\r").expect("write");
        h.writer.flush().expect("flush");
        let echoed = read_until(&mut h.reader, "echo: still here", Duration::from_secs(10));
        assert!(echoed.contains("echo: still here"), "got {echoed:?}");

        h.writer.write_all(b"/exit\r").expect("write");
        // Plain cleanup, not an assertion: everything the restart contract
        // promises was already checked above (log, handoff, echo through the
        // same outer pty). On this platform a wrap process that has been
        // through a relaunch can independently get stuck in the kernel's own
        // exit-teardown path for a session-leader pty process (`ps` reports
        // it with the documented "E" = "trying to exit" state flag; see
        // batch10-report.md). That is orthogonal to whether the restart
        // itself worked, so bound this wait rather than let an unrelated
        // platform quirk hang the test.
        wait_or_kill(&mut h.child, Duration::from_secs(5));
    }

    /// Waits for a child to exit, killing it if it has not within `timeout`.
    /// See the restart test's cleanup for why a plain `.wait()` is not safe
    /// to use unconditionally on this platform.
    #[cfg(unix)]
    fn wait_or_kill(child: &mut Box<dyn portable_pty::Child + Send + Sync>, timeout: Duration) {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if matches!(child.try_wait(), Ok(Some(_))) {
                return;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        let _ = child.kill();
    }

    #[cfg(unix)]
    #[test]
    fn a_relaunch_that_cannot_spawn_degrades_the_session_cleanly() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = tmp.path().join("state");
        let transcript = tmp.path().join("t.jsonl");
        std::fs::write(
            &transcript,
            concat!(
                r#"{"type":"user","message":{"content":"wire the webhook"}}"#,
                "\n",
                r#"{"type":"assistant","message":{"content":[{"type":"tool_use","id":"a","name":"Read","input":{"file_path":"/work/src/hook.rs"}}],"usage":{"input_tokens":180000}}}"#,
                "\n"
            ),
        )
        .expect("write");

        // The initial spawn runs the wrapped argv directly (unaffected by
        // ZIRV_CTX_AGENT_BIN), so it starts fine on relaunch-stub.sh. relaunch()
        // instead goes through the adapter's interactive_cmd, which honors
        // ZIRV_CTX_AGENT_BIN: pointing it at a path that cannot exist makes
        // relaunch()'s own spawn_command fail, exactly like a real agent
        // binary going missing between sessions.
        let script = fixture("relaunch-stub.sh").display().to_string();
        let mut h = spawn_wrap(
            &[
                ("ZIRV_CTX_TRANSCRIPT", transcript.display().to_string()),
                ("ZIRV_CTX_DEBOUNCE_MS", "300".to_string()),
                ("ZIRV_CTX_STATE_DIR", state.display().to_string()),
                (
                    "ZIRV_CTX_AGENT_BIN",
                    "/nonexistent/zirv-ctx-test-agent-binary".to_string(),
                ),
            ],
            &["sh", &script],
        );
        let _ = read_until(&mut h.reader, "stub-tui ready", Duration::from_secs(10));

        let socket =
            read_socket_path(&StateDir::from_root(state.clone()), None).expect("socket path");
        crate::commands::ctx::signal::send(
            std::path::Path::new(socket.trim()),
            &turn_signal(5, Verdict::Restart),
        )
        .expect("send");

        // relaunch() cannot spawn, so the session ends: no hang, no crash,
        // and the process exits through the old (already-quit) child's own
        // exit path rather than the fresh-generation one.
        wait_or_kill(&mut h.child, Duration::from_secs(10));

        let log = std::fs::read_to_string(state.join("logs/decisions.jsonl")).expect("log");
        assert!(
            log.contains("\"action\":\"degrade\""),
            "note_failure logged: {log}"
        );
        // "Item 6 audit" (see the comment at `relaunch_error`'s
        // declaration) made `note_failure` name the *real* spawn error
        // instead of the old generic "relaunch failed" placeholder, which
        // that fallback string is now dead code for -- `relaunch_error` is
        // always `Some` by the time this arm runs. The nonexistent
        // `ZIRV_CTX_AGENT_BIN` path is what's actually stable across
        // portable-pty's own wording for "the binary is missing".
        assert!(
            log.contains("zirv-ctx-test-agent-binary"),
            "reason recorded: {log}"
        );
        assert!(
            log.contains("\"action\":\"restart-failed\""),
            "restart outcome logged: {log}"
        );
    }

    #[test]
    fn noting_a_failure_degrades_the_supervisor_once_and_for_all() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = crate::commands::ctx::state::StateDir::from_root(tmp.path().to_path_buf());
        let mut supervision = InjectionState::new();

        note_failure(
            &mut supervision,
            Some((&state, "sess")),
            "socket died",
            &Announcer::silent(),
        );
        assert!(supervision.degraded);

        // Even a fresh turn signal cannot re-enable injection.
        supervision.on_turn(&turn_signal(9, Verdict::Restart));
        supervision.last_output = Instant::now() - Duration::from_secs(30);
        assert_eq!(
            action_for(&supervision, Instant::now(), DEBOUNCE),
            Action::None
        );

        let log = std::fs::read_to_string(state.logs().join("decisions.jsonl")).expect("log");
        assert!(log.contains("degrade"), "got {log}");
        assert!(log.contains("socket died"), "record the reason: {log}");
    }

    #[test]
    fn note_failure_without_a_state_dir_still_degrades() {
        let mut supervision = InjectionState::new();
        note_failure(&mut supervision, None, "no state dir", &Announcer::silent());
        assert!(supervision.degraded);
    }

    /// Issue #310 parity: `exec` records every respawn on the cross-process
    /// restart chain and stands down once the breaker trips; `wrap`'s own
    /// `Action::Restart` arm recorded nothing and asked nothing, so a session
    /// that rots straight back into a restart relaunched forever, on no budget
    /// at all.
    #[test]
    fn a_tripped_restart_chain_stops_wraps_own_relaunches() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().join("state"));
        let repo = tmp.path().join("repo");
        let cfg = CtxConfig::default();

        assert_eq!(
            tripped_restart_chain(&state, &repo, &cfg, 1_000),
            None,
            "the first restart is never the pattern"
        );
        assert_eq!(tripped_restart_chain(&state, &repo, &cfg, 1_010), None);
        assert_eq!(
            tripped_restart_chain(&state, &repo, &cfg, 1_020),
            Some(cfg.supervise.chain_max_restarts),
            "three restarts inside the configured gap is the loop the breaker exists for"
        );
    }

    #[cfg(unix)]
    #[test]
    fn an_unbindable_socket_leaves_a_fully_transparent_wrapper() {
        let tmp = tempfile::tempdir().expect("tempdir");
        // A state dir path long enough that the socket path exceeds the limit.
        let long_state = tmp.path().join("s".repeat(120));
        let script = fixture("stub-tui.sh").display().to_string();

        let mut h = spawn_wrap(
            &[("ZIRV_CTX_STATE_DIR", long_state.display().to_string())],
            &["sh", &script],
        );
        let seen = read_until(&mut h.reader, "stub-tui ready", Duration::from_secs(10));
        assert!(seen.contains("stub-tui ready"), "got {seen:?}");

        h.writer.write_all(b"hello\r").expect("write");
        h.writer.flush().expect("flush");
        let echoed = read_until(&mut h.reader, "echo: hello", Duration::from_secs(10));
        assert!(
            echoed.contains("echo: hello"),
            "passthrough intact: {echoed:?}"
        );

        h.writer.write_all(b"/exit\r").expect("write");
        let status = h.child.wait().expect("wait");
        assert_eq!(status.exit_code(), 0);
    }

    /// Waits for `child`, killing it and returning `None` past `timeout` so
    /// a genuinely wedged child fails this one test fast instead of hanging
    /// it (and the suite behind it) forever. `h.child.wait()`'s own contract
    /// has no timeout at all -- fine for a child known to exit promptly, but
    /// `a_broken_transcript_path_never_stops_the_session` hung on it for real
    /// once the `DASH_REQUESTS_ENV` scrub above stopped short-circuiting the
    /// nesting guard before this child's own exit path was ever reached
    /// (reproduced twice, ~20+ min combined, before this fix). Mirrors
    /// `win::wait_bounded`'s identical shape for `std::process::Child`.
    #[cfg(unix)]
    fn wait_bounded(
        child: &mut dyn portable_pty::Child,
        timeout: Duration,
    ) -> Option<portable_pty::ExitStatus> {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            match child.try_wait() {
                Ok(Some(status)) => return Some(status),
                Ok(None) => std::thread::sleep(Duration::from_millis(50)),
                Err(_) => return None,
            }
        }
        let _ = child.kill();
        // The kill is not synchronous, so reap with the same bounded poll
        // rather than a blocking `wait()` -- a `wait()` here would hang this
        // helper forever on a child the kill somehow failed to actually
        // terminate, defeating the entire point of bounding it above. A
        // child left unreaped in a test process that is about to exit is
        // harmless (the OS reaps it); a wedged suite is not.
        let reap_deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < reap_deadline {
            match child.try_wait() {
                Ok(Some(_)) | Err(_) => break,
                Ok(None) => std::thread::sleep(Duration::from_millis(50)),
            }
        }
        None
    }

    #[cfg(unix)]
    #[test]
    fn a_broken_transcript_path_never_stops_the_session() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let script = fixture("stub-tui.sh").display().to_string();
        let mut h = spawn_wrap(
            &[
                (
                    "ZIRV_CTX_TRANSCRIPT",
                    "/nonexistent/dir/t.jsonl".to_string(),
                ),
                ("ZIRV_CTX_DEBOUNCE_MS", "200".to_string()),
                (
                    "ZIRV_CTX_STATE_DIR",
                    tmp.path().join("state").display().to_string(),
                ),
            ],
            &["sh", &script],
        );
        let _ = read_until(&mut h.reader, "stub-tui ready", Duration::from_secs(10));

        let socket =
            read_socket_path(&StateDir::from_root(tmp.path().join("state")), None).expect("path");
        crate::commands::ctx::signal::send(
            std::path::Path::new(socket.trim()),
            &turn_signal(2, Verdict::Compact),
        )
        .expect("send");

        // The injection is attempted and cannot be verified, so wrap degrades
        // while the session continues.
        h.writer.write_all(b"still here\r").expect("write");
        h.writer.flush().expect("flush");
        let echoed = read_until(&mut h.reader, "echo: still here", Duration::from_secs(20));
        assert!(echoed.contains("echo: still here"), "got {echoed:?}");

        h.writer.write_all(b"/exit\r").expect("write");
        let status = wait_bounded(h.child.as_mut(), Duration::from_secs(10))
            .expect("the session must exit within a bounded window, not hang");
        assert_eq!(status.exit_code(), 0);
    }

    #[cfg(unix)]
    #[test]
    fn no_supervise_skips_supervision_entirely() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let script = fixture("stub-tui.sh").display().to_string();

        let pair = native_pty_system()
            .openpty(PtySize {
                rows: 24,
                cols: 80,
                pixel_width: 0,
                pixel_height: 0,
            })
            .expect("openpty");
        let mut cmd = CommandBuilder::new(zirv_bin());
        crate::commands::ctx::testenv::scrub_operator_profile_env_for_test(&mut cmd);
        cmd.arg("ctx");
        cmd.arg("wrap");
        cmd.arg("--agent");
        cmd.arg("claude");
        cmd.arg("--no-supervise");
        cmd.arg("--");
        cmd.arg("sh");
        cmd.arg(&script);
        cmd.env("TERM", "xterm");
        cmd.env(
            "ZIRV_CTX_STATE_DIR",
            tmp.path().join("state").display().to_string(),
        );
        for home in ["HOME", "USERPROFILE"] {
            cmd.env(home, tmp.path().display().to_string());
        }
        // T8: hermetic against the developer's/agent's own environment --
        // see the identical comment on `spawn_wrap`'s pty harness above.
        // This test builds its own `CommandBuilder` rather than going
        // through that harness, so it needs the same scrub explicitly (see
        // `testenv::scrub_supervision_env_for_test`'s own doc comment).
        crate::commands::ctx::testenv::scrub_supervision_env_for_test(&mut cmd);
        let mut child = pair.slave.spawn_command(cmd).expect("spawn");
        drop(pair.slave);
        let mut reader = ChunkReader::spawn(pair.master.try_clone_reader().expect("reader"));
        let mut writer = pair.master.take_writer().expect("writer");

        let seen = read_until(&mut reader, "stub-tui ready", Duration::from_secs(10));
        assert!(seen.contains("stub-tui ready"));
        assert_eq!(
            read_socket_path(&StateDir::from_root(tmp.path().join("state")), None),
            None,
            "no socket is bound when supervision is off"
        );

        writer.write_all(b"/exit\r").expect("write");
        let status = child.wait().expect("wait");
        assert_eq!(status.exit_code(), 0);
    }

    /// Windows coverage for the pty deadlock. Every one of these bounds its own
    /// wait: a regression here used to hang forever, and a hanging test in CI
    /// is indistinguishable from a slow one.
    #[cfg(windows)]
    mod win {
        use super::*;

        /// Waits for `child`, killing it and returning `None` past `timeout` so
        /// a re-deadlocked `wrap` fails the test instead of wedging the suite.
        /// Mirrors the non-Windows `wait_bounded` above, reap loop included.
        fn wait_bounded(
            child: &mut std::process::Child,
            timeout: Duration,
        ) -> Option<std::process::ExitStatus> {
            let deadline = Instant::now() + timeout;
            while Instant::now() < deadline {
                match child.try_wait() {
                    Ok(Some(status)) => return Some(status),
                    Ok(None) => std::thread::sleep(Duration::from_millis(50)),
                    Err(_) => return None,
                }
            }
            let _ = child.kill();
            // The kill is not synchronous, so reap with the same bounded poll
            // rather than a blocking `wait()` -- see the identical comment on
            // the non-Windows `wait_bounded` for why an unbounded wait here
            // would defeat this helper's entire purpose.
            let reap_deadline = Instant::now() + Duration::from_secs(5);
            while Instant::now() < reap_deadline {
                match child.try_wait() {
                    Ok(Some(_)) | Err(_) => break,
                    Ok(None) => std::thread::sleep(Duration::from_millis(50)),
                }
            }
            None
        }

        fn spawn_wrap(
            state: &std::path::Path,
            flags: &[&str],
            wrapped: &[&str],
        ) -> std::process::Child {
            let mut cmd = std::process::Command::new(zirv_bin());
            crate::commands::ctx::testenv::scrub_operator_profile_env_for_test_cmd(&mut cmd);
            cmd.arg("ctx").arg("wrap");
            cmd.args(flags);
            cmd.arg("--");
            cmd.args(wrapped);
            cmd.env(crate::commands::ctx::state::STATE_ENV, state);
            // Hermetic against the *developer's* own environment (F2): these
            // tests spawn the real `zirv` binary, which reads the process
            // environment, so a suite run from inside an agent session would
            // otherwise trip the nesting guard and see exit code 1 instead of
            // whatever the test is actually about. The guard has its own
            // dedicated coverage above; here it is noise. T8: see the
            // identical fix/comment on `spawn_wrap`'s pty harness above, and
            // `testenv::scrub_supervision_env_for_test_cmd`'s own doc comment
            // for why this is a test-side scrub, not an extended production one.
            crate::commands::ctx::testenv::scrub_supervision_env_for_test_cmd(&mut cmd);
            // No terminal: this is also the CI/piped case, which is exactly
            // why the synthetic cursor report cannot be left to a real
            // terminal to send.
            cmd.stdin(std::process::Stdio::null());
            cmd.stdout(std::process::Stdio::null());
            // A file, not a pipe: like NUL it is not a console, and it cannot fill and block the wrapper.
            std::fs::create_dir_all(state).expect("state dir");
            let log = std::fs::File::create(stderr_log(state)).expect("stderr log");
            cmd.stderr(log);
            // Keep the developer's real ~/.zirv/ctx.toml out of the run.
            for home in ["HOME", "USERPROFILE"] {
                cmd.env(home, state);
            }
            cmd.spawn().expect("spawn zirv ctx wrap")
        }

        fn stderr_log(state: &std::path::Path) -> std::path::PathBuf {
            state.join("wrap-stderr.log")
        }

        fn wrap_stderr(state: &std::path::Path) -> String {
            std::fs::read_to_string(stderr_log(state)).unwrap_or_default()
        }

        /// The regression that shipped: portable-pty's pseudoconsole asks for a
        /// cursor position report and blocks until it gets one, so *every*
        /// wrapped command hung -- this one does nothing but exit.
        #[test]
        fn a_wrapped_command_that_exits_immediately_does_not_hang_the_wrapper() {
            let tmp = tempfile::tempdir().expect("tempdir");
            let mut child =
                spawn_wrap(tmp.path(), &["--no-supervise"], &["cmd", "/c", "exit", "0"]);
            let status = wait_bounded(&mut child, Duration::from_secs(30))
                .expect("wrap must exit, not deadlock on the console host's cursor probe");
            assert_eq!(
                status.code(),
                Some(0),
                "the child's own exit code; wrap stderr: {}",
                wrap_stderr(tmp.path())
            );
        }

        /// The wrapped command's exit code is the wrapper's, so a
        /// pseudoconsole that never ran the child cannot masquerade as success.
        #[test]
        fn a_wrapped_command_reports_its_own_failing_exit_code() {
            let tmp = tempfile::tempdir().expect("tempdir");
            let mut child =
                spawn_wrap(tmp.path(), &["--no-supervise"], &["cmd", "/c", "exit", "3"]);
            let status = wait_bounded(&mut child, Duration::from_secs(30))
                .expect("wrap must exit rather than deadlock");
            assert_eq!(
                status.code(),
                Some(3),
                "wrap stderr: {}",
                wrap_stderr(tmp.path())
            );
        }

        /// Supervision used to be off for the entire run on Windows: the turn
        /// signal had no transport, so `bind` failed and `wrap` degraded before
        /// the agent had even started. The named pipe is what fixes that, and a
        /// bound server leaves the same directory entry unix does.
        #[test]
        fn a_supervised_wrap_binds_a_turn_signal_transport() {
            let tmp = tempfile::tempdir().expect("tempdir");
            let state = tmp.path().join("state");
            let mut child = spawn_wrap(
                &state,
                &["--agent", "claude"],
                // Long enough to still be running when the assertion below
                // looks for the socket entry, short enough to reap itself if
                // the kill somehow misses.
                &["cmd", "/c", "ping -n 20 127.0.0.1"],
            );

            let deadline = Instant::now() + Duration::from_secs(30);
            let mut sockets = Vec::new();
            while Instant::now() < deadline && sockets.is_empty() {
                // Issue jev-relay: `state/s/` can now also hold this
                // session's own Jev relay endpoint (`jev_relay::start`,
                // whenever the OPERATOR's real config has a `[jev]` gate on
                // and a credential present -- this test spawns the real
                // `zirv` binary against the real `~/.zirv/ctx.toml`, not an
                // isolated one), an extensionless file `is_endpoint_file`
                // already excludes -- the same discriminator `status.rs`'s
                // own `orphan_sockets` scan of this same directory uses, so
                // this assertion and that production scan never drift on
                // what counts as a turn-signal endpoint here.
                sockets = std::fs::read_dir(state.join("s"))
                    .map(|entries| {
                        entries
                            .flatten()
                            .map(|e| e.path())
                            .filter(|path| crate::commands::ctx::sessions::is_endpoint_file(path))
                            .collect()
                    })
                    .unwrap_or_default();
                if sockets.is_empty() {
                    std::thread::sleep(Duration::from_millis(100));
                }
            }
            let _ = child.kill();
            let _ = child.wait();

            assert_eq!(
                sockets.len(),
                1,
                "a supervised wrap publishes exactly one turn-signal endpoint"
            );
            let published = std::fs::read_to_string(&sockets[0]).expect("read");
            assert!(
                published.starts_with(r"\\.\pipe\zirv-ctx-"),
                "the endpoint is a named pipe: {published}"
            );
        }
    }

    #[cfg(unix)]
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
}
