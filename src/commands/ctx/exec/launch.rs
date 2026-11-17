//! Child launch for one supervised exec cycle: `codex exec`, or, for an
//! opted-in codex first launch on unix, the same turn over `codex app-server`
//! (#950). A failure before `turn/start` is sent launches the unchanged command;
//! any later one is a failed child, never a second run through `codex exec`.

use super::accounting::TurnUsage;
use super::*;

/// The child, its output tap and guard, an app-server turn's usage slot, and
/// the handle that steers or interrupts it.
pub(super) type Launched = (
    std::process::Child,
    supervise::OutputTap,
    supervise::ChildGuard,
    Option<TurnUsage>,
    Option<TurnControl>,
);

/// Budget for a `turn/steer` answer.
#[cfg_attr(not(unix), allow(dead_code))]
pub(super) const STEER_WAIT: Duration = Duration::from_secs(5);
/// Budget for a `turn/interrupt` to end the turn before the server is killed.
#[cfg_attr(not(unix), allow(dead_code))]
pub(super) const INTERRUPT_WAIT: Duration = Duration::from_secs(10);
/// How long past its own bound a caller waits for the driver's verdict.
const REPLY_SLACK: Duration = Duration::from_secs(1);

/// A request to the app-server driver, which alone writes to the server.
/// `until` is the caller's deadline: the driver drops a request it reaches
/// later than that, and the reply says whether it took effect.
#[cfg_attr(not(unix), allow(dead_code))]
pub(super) enum Control {
    Steer {
        text: String,
        until: Instant,
        reply: std::sync::mpsc::Sender<bool>,
    },
    Interrupt {
        until: Instant,
        reply: std::sync::mpsc::Sender<bool>,
    },
}

/// Steers or interrupts the running app-server turn. Every method is `false`
/// when the driver is gone or does not confirm in time, and the caller then
/// does what it did before app-server existed.
#[derive(Clone)]
#[cfg_attr(not(unix), allow(dead_code))]
pub(super) struct TurnControl(std::sync::mpsc::Sender<Control>);

#[cfg_attr(not(unix), allow(dead_code))]
impl TurnControl {
    pub(super) fn channel() -> (Self, std::sync::mpsc::Receiver<Control>) {
        let (tx, rx) = std::sync::mpsc::channel();
        (Self(tx), rx)
    }

    /// Add `text` to the running turn; `true` once the server accepted it.
    pub(super) fn steer(&self, text: &str, wait: Duration) -> bool {
        let (reply, answer) = std::sync::mpsc::channel();
        let control = Control::Steer {
            text: text.to_string(),
            until: Instant::now() + wait,
            reply,
        };
        self.0.send(control).is_ok() && answer.recv_timeout(wait + REPLY_SLACK).unwrap_or(false)
    }

    /// Ask the server to end the turn; `true` once it has, and exits on its own.
    pub(super) fn interrupt(&self, wait: Duration) -> bool {
        let (reply, answer) = std::sync::mpsc::channel();
        let control = Control::Interrupt {
            until: Instant::now() + wait,
            reply,
        };
        self.0.send(control).is_ok() && answer.recv_timeout(wait + REPLY_SLACK).unwrap_or(false)
    }
}

/// The operator opted in and zirv builds this codex launch itself.
pub(super) fn wants_app_server(
    cfg: &CtxConfig,
    adapter: &dyn adapters::AgentAdapter,
    adapter_builds_launch: bool,
) -> bool {
    cfg.headless.codex_app_server && adapter_builds_launch && adapter.name() == "codex"
}

/// Spawn this cycle's child; `app_server` only ever takes effect on unix.
pub(super) fn spawn<W: Write>(
    command: Command,
    stdin_prompt: Option<String>,
    app_server: bool,
    state: &StateDir,
    session: &str,
    w: &mut W,
) -> CtxResult<Launched> {
    #[cfg(not(unix))]
    let _ = (app_server, state, session, &w);
    #[cfg(unix)]
    if app_server {
        use super::app_server::turn;
        match turn::begin(&command, stdin_prompt.as_deref(), state, session) {
            Ok(launched) => {
                return Ok((
                    launched.child,
                    launched.tap,
                    launched.guard,
                    Some(launched.usage),
                    Some(launched.control),
                ));
            }
            Err(reason) => {
                turn::note(state, session, "fallback", &reason.to_string());
                let _ = writeln!(
                    w,
                    "zirv ctx exec: codex app-server unavailable ({reason}); launching codex exec"
                );
            }
        }
    }
    let (child, tap, guard) = supervise::spawn_tapped(command, stdin_prompt)?;
    Ok((child, tap, guard, None, None))
}

#[cfg(test)]
#[cfg(unix)]
mod tests {
    use super::super::tests::{base_env, fixture};
    use super::super::{ExecArgs, ExecutionReport, run_with_report};
    use crate::commands::ctx::event::TranscriptUsage;
    use crate::commands::ctx::log::{self, DecisionRecord};
    use crate::commands::ctx::state::StateDir;
    use crate::commands::ctx::testenv;

    struct Run {
        code: i32,
        report: ExecutionReport,
        decisions: Vec<DecisionRecord>,
        /// Every `codex exec` the run launched (fake-codex-agent.sh's log).
        exec_argv: String,
        /// Every `codex app-server` invocation, the `--help` probe included.
        server_argv: String,
    }

    /// One `ctx exec --prompt` codex run against `fake-app-server.sh`.
    fn run(transcript: &str, mode: &str, opt_in: bool) -> Run {
        run_for(transcript, mode, opt_in, 30)
    }

    fn run_for(transcript: &str, mode: &str, opt_in: bool, timeout_secs: u64) -> Run {
        let tmp = testenv::repo();
        let home = tmp.path().join("home");
        let state_dir = tmp.path().join("state");
        let exec_log = tmp.path().join("exec-argv.log");
        let server_log = tmp.path().join("server-argv.log");
        let mut env = base_env(&state_dir);
        env.insert("ZIRV_CTX_PACE".to_string(), "false".to_string());
        env.insert("ZIRV_CTX_POLL_MS".to_string(), "50".to_string());
        env.insert(
            "ZIRV_CTX_AGENT_BIN".to_string(),
            format!("sh {}", fixture("fake-app-server.sh").display()),
        );
        if opt_in {
            env.insert(
                "ZIRV_CTX_HEADLESS_CODEX_APP_SERVER".to_string(),
                "true".to_string(),
            );
        }
        let transcript = fixture(transcript);
        let _home = testenv::HomeGuard::set(&home);
        let _vars = testenv::VarGuard::set(&[
            ("FAKE_AGENT_ARGV_LOG", exec_log.to_str()),
            ("FAKE_APPSERVER_LOG", server_log.to_str()),
            ("FAKE_APPSERVER_TRANSCRIPT", transcript.to_str()),
            ("FAKE_APPSERVER_MODE", Some(mode)),
            ("FAKE_APPSERVER_HELP", None),
        ]);
        let args = ExecArgs {
            agent: Some("codex".to_string()),
            prompt: Some("do the work".to_string()),
            max_restarts: Some(0),
            timeout_secs: Some(timeout_secs),
            simple: true,
            command: Vec::new(),
            ..Default::default()
        };
        let mut out = Vec::new();
        let (code, report) =
            run_with_report(&args, &mut out, tmp.path(), &|k| env.get(k).cloned()).expect("runs");
        Run {
            code,
            report,
            decisions: log::read_decisions(&StateDir::from_root(state_dir)),
            exec_argv: std::fs::read_to_string(&exec_log).unwrap_or_default(),
            server_argv: std::fs::read_to_string(&server_log).unwrap_or_default(),
        }
    }

    fn app_server_actions(run: &Run) -> Vec<(String, String)> {
        run.decisions
            .iter()
            .filter(|d| d.verdict == "app-server")
            .map(|d| (d.action.clone(), d.detail.clone()))
            .collect()
    }

    /// The main agent's own `codex exec` launches, not the `--help` probes
    /// or a clean-exit memory harvest's read-only distiller.
    fn main_exec_launches(run: &Run) -> usize {
        run.exec_argv
            .lines()
            .filter(|l| l.starts_with("exec ") && !l.contains("--help") && !l.contains("read-only"))
            .count()
    }

    #[test]
    fn an_opted_in_codex_turn_runs_over_app_server_and_reports_its_usage() {
        let run = run("app-server-turn-ok.jsonl", "ok", true);
        assert_eq!(run.code, 0, "a completed turn exits like codex exec: 0");
        assert!(
            run.server_argv.contains("app-server --listen stdio://"),
            "{}",
            run.server_argv
        );
        assert_eq!(main_exec_launches(&run), 0, "{}", run.exec_argv);
        assert_eq!(run.report.segments.len(), 1, "{:?}", run.report.segments);
        // The last cumulative total, split like codex's rollout usage.
        assert_eq!(
            run.report.segments[0].usage,
            TranscriptUsage {
                input_tokens: 1000,
                cache_creation_input_tokens: 0,
                cache_read_input_tokens: 200,
                output_tokens: 34,
            }
        );
        let actions = app_server_actions(&run);
        assert!(
            actions.iter().any(|(a, _)| a == "turn-completed"),
            "{actions:?}"
        );
    }

    #[test]
    fn a_handshake_failure_falls_back_to_codex_exec_and_logs_why() {
        let run = run("app-server-init-error.jsonl", "ok", true);
        assert_eq!(run.code, 0);
        assert_eq!(
            main_exec_launches(&run),
            1,
            "the unchanged codex exec command must run: {}",
            run.exec_argv
        );
        let actions = app_server_actions(&run);
        assert!(
            actions
                .iter()
                .any(|(a, detail)| a == "fallback" && detail.contains("-32600")),
            "the fallback and its reason must be logged: {actions:?}"
        );
    }

    #[test]
    fn an_error_after_the_turn_starts_is_a_child_failure() {
        let run = run("app-server-turn-error.jsonl", "ok", true);
        assert_ne!(run.code, 0, "a failed turn must surface as a failing child");
        assert_eq!(
            main_exec_launches(&run),
            0,
            "a started turn never falls back to exec: {}",
            run.exec_argv
        );
        let actions = app_server_actions(&run);
        assert!(
            actions.iter().any(|(a, detail)| a == "turn-failed"
                && detail.contains("stream disconnected before completion")),
            "{actions:?}"
        );
    }

    /// Once `turn/start` is written the prompt may be running, so a missing
    /// ack never re-launches it through `codex exec`.
    #[test]
    fn a_turn_start_that_is_never_acknowledged_is_a_child_failure_not_a_fallback() {
        let run = run_for("app-server-turn-no-ack.jsonl", "ok", true, 5);
        assert_ne!(run.code, 0, "an unacknowledged turn must fail the child");
        assert_eq!(
            main_exec_launches(&run),
            0,
            "a written turn/start never falls back to exec: {}",
            run.exec_argv
        );
        let actions = app_server_actions(&run);
        assert!(actions.iter().any(|(a, _)| a == "launch"), "{actions:?}");
        assert!(!actions.iter().any(|(a, _)| a == "fallback"), "{actions:?}");
    }

    #[test]
    fn with_the_key_off_the_launch_is_the_unchanged_exec_command() {
        let run = run("app-server-turn-ok.jsonl", "ok", false);
        assert_eq!(run.code, 0);
        assert!(
            run.server_argv.is_empty(),
            "no app-server probe or spawn when off: {}",
            run.server_argv
        );
        assert_eq!(main_exec_launches(&run), 1, "{}", run.exec_argv);
        assert!(app_server_actions(&run).is_empty());
    }

    #[test]
    fn a_codex_exec_child_has_no_steer_control() {
        let dir = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(dir.path().to_path_buf());
        let mut command = std::process::Command::new("sh");
        command.args(["-c", "exit 0"]);
        let (mut child, _tap, _guard, usage, control) =
            super::spawn(command, None, false, &state, "s", &mut Vec::new()).expect("spawn");
        assert!(
            usage.is_none() && control.is_none(),
            "a relaunch is codex exec"
        );
        let _ = child.wait();
    }
}
