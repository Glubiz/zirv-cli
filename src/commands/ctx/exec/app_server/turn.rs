//! One headless turn over `codex app-server`, for exec to supervise like a
//! `codex exec` child (#950 slice 4).
//!
//! `begin` probes, spawns and handshakes the server, sends `thread/start`, then
//! writes `turn/start(prompt)`. Every failure before that write is a fall-back
//! reason, with the server reaped. Once it is written the prompt may be
//! running, so every later failure (no ack in time, an error answer, EOF, a
//! failed or interrupted turn) prints `ERROR: <message>` and SIGTERMs the
//! server: exec sees a failed child and never re-runs the prompt through
//! `codex exec`. A completed turn prints its final message and closes stdin,
//! so the server exits 0. Token usage lands in the shared slot exec's report reads.

use std::io::Write;
use std::process::{Child, Command};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use super::codec::{ClientRequest, CodecError, ServerFrame, ServerNotification};
use super::probe::{self, Conn, FallBackReason, Next};
use super::translate;
use crate::commands::ctx::event::TranscriptUsage;
use crate::commands::ctx::exec::accounting::TurnUsage;
use crate::commands::ctx::exec::launch::{Control, TurnControl};
use crate::commands::ctx::log;
use crate::commands::ctx::state::{StateDir, now_secs};
use crate::commands::ctx::supervise::{ChildGuard, OutputTap};
use crate::commands::ctx::window;

/// Budget for `thread/start`'s answer, and for `turn/start`'s ack in the driver.
pub const START_TIMEOUT: Duration = Duration::from_secs(60);

/// How often the driver looks up from the server's frames for a `Control`.
const CONTROL_POLL: Duration = Duration::from_millis(100);

/// A turn running over app-server: the child exec supervises and its handles.
pub struct Launched {
    pub child: Child,
    pub guard: ChildGuard,
    pub tap: OutputTap,
    pub usage: TurnUsage,
    pub control: TurnControl,
}

/// One `app-server` decision-log entry.
pub fn note(state: &StateDir, session: &str, action: &str, detail: &str) {
    let _ = log::append(
        state,
        &log::Decision {
            ts: now_secs(),
            session,
            verb: "exec",
            verdict: "app-server",
            score: 0,
            action,
            detail,
            observed_at: None,
        },
    );
}

/// `thread/start`, then write `turn/start(prompt)` without waiting for its
/// ack; returns the thread id.
pub fn start(
    conn: &mut Conn,
    thread: Value,
    prompt: &str,
    deadline: Instant,
) -> Result<String, FallBackReason> {
    let answer = conn.request(&ClientRequest::ThreadStart(thread), deadline)?;
    let thread_id = answer
        .pointer("/thread/id")
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| FallBackReason::Decode(CodecError::Malformed("no /thread/id".into())))?;
    let turn = json!({
        "threadId": thread_id,
        "input": [{"type": "text", "text": prompt, "text_elements": []}],
    });
    conn.send(&ClientRequest::TurnStart(turn))?;
    Ok(thread_id)
}

/// Run `exec_command`'s turn over `codex app-server` instead.
pub fn begin(
    exec_command: &Command,
    stdin_prompt: Option<&str>,
    state: &StateDir,
    session: &str,
) -> Result<Launched, FallBackReason> {
    let plan = translate::plan(exec_command, stdin_prompt)?;
    let mut connected = probe::connect(
        &exec_command.get_program().to_string_lossy(),
        &plan.prefix,
        translate::server_command(exec_command, &plan),
        env!("CARGO_PKG_VERSION"),
        probe::HANDSHAKE_TIMEOUT,
    )?;
    let thread_id = match start(
        &mut connected.conn,
        plan.thread,
        &plan.prompt,
        Instant::now() + START_TIMEOUT,
    ) {
        Ok(thread_id) => thread_id,
        Err(reason) => {
            connected.abandon();
            return Err(reason);
        }
    };
    note(state, session, "launch", &format!("thread {thread_id}"));
    let probe::Connected { child, guard, conn } = connected;
    let (tap, tap_rx) = mpsc::channel();
    let (done, done_rx) = mpsc::channel();
    let slot = Arc::new(Mutex::new(None));
    let (control, control_rx) = TurnControl::channel();
    let driver = Driver {
        group: child.id(),
        thread_id,
        tap,
        usage: slot.clone(),
        _done: done,
        state: state.clone(),
        session: session.to_string(),
        last_message: None,
        error: None,
        control: control_rx,
        turn_id: None,
        interrupting: None,
    };
    std::thread::spawn(move || driver.run(conn));
    Ok(Launched {
        child,
        guard,
        tap: OutputTap::from_channel(tap_rx),
        usage: TurnUsage {
            slot,
            done: done_rx,
        },
        control,
    })
}

/// A turn's cumulative usage, split like `CodexAdapter::transcript_usage`
/// splits the rollout's: codex's input includes its cached part.
fn usage_of(total: &Value) -> Option<TranscriptUsage> {
    let tokens = |key: &str| total.get(key).and_then(Value::as_u64);
    let cached = tokens("cachedInputTokens").unwrap_or(0);
    Some(TranscriptUsage {
        input_tokens: tokens("inputTokens")?.saturating_sub(cached),
        cache_creation_input_tokens: 0,
        cache_read_input_tokens: cached,
        output_tokens: tokens("outputTokens")?,
    })
}

/// An app-server `RateLimitSnapshot` notification mapped through the shared
/// codex mapper, which reads the rollout's snake_case shape.
fn rate_limits_windows(params: &Value, observed_at: u64) -> Option<window::UsageWindows> {
    let snapshot = params.get("rateLimits").filter(|v| v.is_object())?;
    let slot = |key: &str| {
        let node = snapshot.get(key).filter(|v| v.is_object())?;
        Some(json!({
            "used_percent": node.get("usedPercent")?,
            "window_minutes": node.get("windowDurationMins"),
            "resets_at": node.get("resetsAt"),
        }))
    };
    let rollout_shaped = json!({
        "primary": slot("primary"),
        "secondary": slot("secondary"),
        "credits": {"has_credits": snapshot.pointer("/credits/hasCredits")},
        "rate_limit_reached_type": snapshot.get("rateLimitReachedType"),
    });
    window::windows_from_rate_limits(&rollout_shaped, observed_at)
}

/// SIGTERM the server's process group. A stdio app-server installs no
/// handler for it (codex app-server/src/lib.rs:793-795, rust-v0.163.0), so it
/// dies by the signal and exec sees a failed child.
fn fail_child(group: u32) {
    // 0 or 1 would signal this process's own group, or every process.
    let Some(group) = libc::pid_t::try_from(group).ok().filter(|g| *g > 1) else {
        return;
    };
    // SAFETY: kill(2) takes plain integers; `group` is the server's own group.
    unsafe { libc::kill(-group, libc::SIGTERM) };
}

enum End {
    Completed,
    /// Ended by our own `turn/interrupt`; not a failure.
    Interrupted,
    Failed(String),
}

struct Driver {
    /// The server's pid, which is its process group (isolated at spawn).
    group: u32,
    thread_id: String,
    tap: Sender<String>,
    usage: Arc<Mutex<Option<TranscriptUsage>>>,
    /// Dropped when the driver exits, which makes `usage` final.
    _done: Sender<()>,
    state: StateDir,
    session: String,
    last_message: Option<String>,
    /// The fatal error already printed, so it is printed once.
    error: Option<String>,
    /// Steer and interrupt requests from the supervisor; only this thread writes to the server.
    control: Receiver<Control>,
    /// The running turn's id, from `turn/start`'s answer.
    turn_id: Option<String>,
    /// A sent `turn/interrupt` still waiting for the turn to end: its bound and who to tell.
    interrupting: Option<(Instant, Sender<bool>)>,
}

impl Driver {
    fn run(mut self, mut conn: Conn) {
        // Cleared by `turn/start`'s ack; until then a quiet server fails the turn.
        let mut ack_deadline = Some(Instant::now() + START_TIMEOUT);
        let end = loop {
            self.drain_control(&mut conn, ack_deadline.is_some());
            let poll = Instant::now() + CONTROL_POLL;
            let wait = ack_deadline.map_or(poll, |deadline| deadline.min(poll));
            let frame = match conn.next_frame(Some(wait)) {
                Next::Frame(frame) => frame,
                Next::TimedOut if ack_deadline.is_some_and(|d| Instant::now() >= d) => {
                    break End::Failed("turn/start was not acknowledged".into());
                }
                Next::TimedOut => continue,
                Next::Closed => {
                    break End::Failed("the server closed stdout before the turn ended".into());
                }
            };
            match frame {
                Ok(ServerFrame::Notification(ServerNotification::Other { method, params })) => {
                    if let Some(end) = self.on(&method, &params) {
                        break end;
                    }
                }
                Ok(ServerFrame::Request { id, request }) => {
                    let _ = conn.refuse(&id, &request);
                }
                Ok(frame @ (ServerFrame::Response { .. } | ServerFrame::Error { .. })) => {
                    // A steer or interrupt that timed out may still answer late; only
                    // `turn/start`'s answer decides the turn.
                    let Ok(Some(resolved)) = conn.resolve(&frame) else {
                        continue;
                    };
                    if resolved.method != "turn/start" {
                        continue;
                    }
                    match resolved.outcome {
                        Ok(ack) => {
                            ack_deadline = None;
                            self.turn_id = ack
                                .pointer("/turn/id")
                                .and_then(Value::as_str)
                                .map(str::to_string);
                        }
                        Err(rpc) => {
                            break End::Failed(format!(
                                "turn/start error {}: {}",
                                rpc.code, rpc.message
                            ));
                        }
                    }
                }
                Ok(ServerFrame::Notification(ServerNotification::RateLimitsUpdated(params))) => {
                    self.record_rate_limits(&params);
                }
                Err(error) => {
                    let _ = writeln!(
                        std::io::stderr(),
                        "zirv ctx exec: skipped an app-server frame: {error}"
                    );
                }
            }
        };
        match end {
            End::Completed => {
                if let Some(message) = self.last_message.take() {
                    self.emit(message, false);
                }
                self.note("turn-completed", &self.thread_id);
                conn.close_stdin();
            }
            End::Interrupted => {
                self.note("turn-interrupted", &self.thread_id);
                conn.close_stdin();
            }
            End::Failed(ref message) => {
                self.print_error(message);
                self.note("turn-failed", message);
                fail_child(self.group);
            }
        }
        if let Some((_, reply)) = self.interrupting.take() {
            let _ = reply.send(!matches!(end, End::Failed(_)));
        }
        // Drain to EOF so the server never writes into a closed pipe.
        while let Next::Frame(_) = conn.next_frame(None) {}
    }

    /// Serve the supervisor's requests. They wait for `turn/start`'s answer,
    /// which carries the turn id they need.
    fn drain_control(&mut self, conn: &mut Conn, ack_pending: bool) {
        if let Some((_, reply)) = self
            .interrupting
            .take_if(|(until, _)| Instant::now() >= *until)
        {
            self.note("interrupt-failed", "the turn did not end in time");
            let _ = reply.send(false);
        }
        if ack_pending {
            return;
        }
        while let Ok(control) = self.control.try_recv() {
            match control {
                Control::Steer { text, until, reply } => {
                    let _ = reply.send(self.steer(conn, &text, until));
                }
                Control::Interrupt { until, reply } => self.interrupt(conn, until, reply),
            }
        }
    }

    fn steer(&self, conn: &mut Conn, text: &str, until: Instant) -> bool {
        let Some(turn_id) = self.turn_id.as_deref() else {
            return false;
        };
        if self.interrupting.is_some() || Instant::now() >= until {
            return false;
        }
        let params = json!({
            "threadId": self.thread_id,
            "expectedTurnId": turn_id,
            "input": [{"type": "text", "text": text, "text_elements": []}],
        });
        match conn.request(&ClientRequest::TurnSteer(params), until) {
            Ok(_) => {
                self.note("turn-steered", turn_id);
                true
            }
            Err(reason) => {
                self.note("steer-failed", &reason.to_string());
                false
            }
        }
    }

    fn interrupt(&mut self, conn: &mut Conn, until: Instant, reply: Sender<bool>) {
        let Some(turn_id) = self.turn_id.clone() else {
            let _ = reply.send(false);
            return;
        };
        if self.interrupting.is_some() || Instant::now() >= until {
            let _ = reply.send(false);
            return;
        }
        let params = json!({"threadId": self.thread_id, "turnId": turn_id});
        match conn.request(&ClientRequest::TurnInterrupt(params), until) {
            // The turn's own `turn/completed` settles it.
            Ok(_) => self.interrupting = Some((until, reply)),
            Err(reason) => {
                self.note("interrupt-failed", &reason.to_string());
                let _ = reply.send(false);
            }
        }
    }

    fn on(&mut self, method: &str, params: &Value) -> Option<End> {
        let text = |pointer: &str| params.pointer(pointer).and_then(Value::as_str);
        // One turn per thread, so its thread id names it.
        if text("/threadId") != Some(self.thread_id.as_str()) {
            return None;
        }
        match method {
            "thread/tokenUsage/updated" => {
                if let Some(usage) = params.pointer("/tokenUsage/total").and_then(usage_of)
                    && let Ok(mut slot) = self.usage.lock()
                {
                    *slot = Some(usage);
                }
            }
            "item/completed" if text("/item/type") == Some("agentMessage") => {
                self.last_message = text("/item/text").map(str::to_string);
            }
            "error" if params.get("willRetry") != Some(&Value::Bool(true)) => {
                let message = text("/error/message").unwrap_or("turn error").to_string();
                self.print_error(&message);
            }
            "turn/completed" => {
                let status = text("/turn/status").unwrap_or_default();
                if status == "completed" {
                    return Some(End::Completed);
                }
                if status == "interrupted" && self.interrupting.is_some() {
                    return Some(End::Interrupted);
                }
                let message = text("/turn/error/message")
                    .map(str::to_string)
                    .or_else(|| self.error.clone())
                    .unwrap_or_else(|| format!("turn {status}"));
                return Some(End::Failed(message));
            }
            _ => {}
        }
        None
    }

    /// Persist an `account/rateLimits/updated` snapshot into the codex usage
    /// windows. Params are `{rateLimits: RateLimitSnapshot}` (camelCase,
    /// codex app-server schema v2/AccountRateLimitsUpdatedNotification.json);
    /// the snapshot carries no timestamp, so receipt time is its observation
    /// time. `window::merge` keeps whichever reading is newer per slot, so a
    /// rollout reading stamped later is never overwritten. Anything
    /// unrecognized is logged and ignored.
    fn record_rate_limits(&self, params: &Value) {
        let now = now_secs();
        let Some(fresh) = rate_limits_windows(params, now) else {
            self.note("rate-limits-ignored", "no usable windows");
            return;
        };
        let existing = window::load_for(&self.state, window::CODEX_USAGE_PROVIDER);
        let merged = window::merge(existing.clone().unwrap_or_default(), fresh);
        if Some(&merged) == existing.as_ref() {
            return;
        }
        if let Err(error) = window::store_for(&self.state, window::CODEX_USAGE_PROVIDER, &merged) {
            self.note("rate-limits-ignored", &format!("store failed: {error}"));
        }
    }

    /// `codex exec` prints `ERROR: <message>` to stderr; the tap scans it the same way.
    fn print_error(&mut self, message: &str) {
        if self.error.as_deref() == Some(message) {
            return;
        }
        self.error = Some(message.to_string());
        self.emit(format!("ERROR: {message}"), true);
    }

    /// Mirror a line like `supervise::forward` does a child's, and tap it.
    fn emit(&self, line: String, stderr: bool) {
        let _ = if stderr {
            writeln!(std::io::stderr(), "{line}")
        } else {
            let mut stdout = std::io::stdout();
            writeln!(stdout, "{line}").and_then(|()| stdout.flush())
        };
        let _ = self.tap.send(line);
    }

    fn note(&self, action: &str, detail: &str) {
        note(&self.state, &self.session, action, detail);
    }
}

#[cfg(test)]
mod tests {
    use super::super::replay::{FakeServer, parse_transcript};
    use super::*;
    use crate::commands::ctx::adapters::AgentAdapter;
    use crate::commands::ctx::adapters::codex::CodexAdapter;
    use crate::commands::ctx::exec::launch::TurnControl;
    use std::io::{PipeWriter, pipe};

    const TURN_OK: &str = include_str!("../../../../../tests/fixtures/app-server-turn-ok.jsonl");
    const TURN_RATE_LIMITS: &str =
        include_str!("../../../../../tests/fixtures/app-server-turn-rate-limits.jsonl");

    /// Checks each client line against the script and feeds back its answer.
    struct Script {
        server: FakeServer,
        out: PipeWriter,
        buf: Vec<u8>,
        sent: Arc<Mutex<Vec<String>>>,
    }

    impl Write for Script {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.buf.extend_from_slice(bytes);
            while let Some(end) = self.buf.iter().position(|b| *b == b'\n') {
                let line: Vec<u8> = self.buf.drain(..=end).collect();
                if let Ok(mut sent) = self.sent.lock() {
                    sent.push(String::from_utf8_lossy(&line).trim().to_string());
                }
                let replies = self
                    .server
                    .on_outbound(&String::from_utf8_lossy(&line))
                    .map_err(|e| std::io::Error::other(e.to_string()))?;
                for reply in replies {
                    self.out.write_all(reply.as_bytes())?;
                }
            }
            Ok(bytes.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn a_planned_exec_launch_replays_the_turn_transcript_through_turn_start() {
        let mut command = Command::new("codex");
        command
            .args(["exec", "do the work", "--sandbox", "workspace-write"])
            .current_dir("/repo");
        let plan = translate::plan(&command, None).expect("plan");
        let (reader, writer) = pipe().expect("pipe");
        let script = Script {
            server: FakeServer::new(parse_transcript(TURN_OK).expect("transcript")),
            out: writer,
            buf: Vec::new(),
            sent: Arc::default(),
        };
        let mut conn = Conn::new(script, reader);
        let deadline = Instant::now() + Duration::from_secs(5);
        conn.initialize("0.0.0", deadline).expect("handshake");
        let thread_id =
            start(&mut conn, plan.thread, &plan.prompt, deadline).expect("turn/start written");
        assert_eq!(thread_id, "thr-1");
        // `start` returns before the ack; the driver matches it to `turn/start`.
        let ack = loop {
            match conn.next_frame(Some(deadline)) {
                Next::Frame(Ok(frame @ ServerFrame::Response { .. })) => {
                    break conn.resolve(&frame);
                }
                Next::Frame(_) => continue,
                Next::Closed | Next::TimedOut => panic!("no turn/start ack"),
            }
        };
        assert_eq!(ack.expect("matched").map(|r| r.method), Some("turn/start"));
    }

    fn driver(state: &StateDir) -> Driver {
        driver_with_control(state, mpsc::channel().1)
    }

    fn driver_with_control(state: &StateDir, control: mpsc::Receiver<Control>) -> Driver {
        let (tap, _rx) = mpsc::channel();
        let (done, _done_rx) = mpsc::channel();
        Driver {
            group: 0,
            thread_id: "thr-1".into(),
            tap,
            usage: Arc::new(Mutex::new(None)),
            _done: done,
            state: state.clone(),
            session: "s".into(),
            last_message: None,
            error: None,
            control,
            turn_id: None,
            interrupting: None,
        }
    }

    /// Replays `transcript` through a full driver run; returns whether the turn completed.
    fn run_turn(transcript: &str, state: &StateDir) -> bool {
        let mut command = Command::new("codex");
        command
            .args(["exec", "do the work", "--sandbox", "workspace-write"])
            .current_dir("/repo");
        let plan = translate::plan(&command, None).expect("plan");
        let (reader, writer) = pipe().expect("pipe");
        let script = Script {
            server: FakeServer::new(parse_transcript(transcript).expect("transcript")),
            out: writer,
            buf: Vec::new(),
            sent: Arc::default(),
        };
        let mut conn = Conn::new(script, reader);
        let deadline = Instant::now() + Duration::from_secs(5);
        conn.initialize("0.0.0", deadline).expect("handshake");
        start(&mut conn, plan.thread, &plan.prompt, deadline).expect("turn/start written");
        let driver = driver(state);
        let usage = driver.usage.clone();
        driver.run(conn);
        usage.lock().is_ok_and(|u| u.is_some())
    }

    fn five_hour_percent(state: &StateDir) -> Option<f64> {
        window::load_for(state, window::CODEX_USAGE_PROVIDER)?
            .five_hour
            .map(|w| w.used_percentage)
    }

    fn stored(state: &StateDir, five_hour_used: f64, observed_at: u64) {
        let reading = window::UsageWindows {
            five_hour: Some(window::Window {
                used_percentage: five_hour_used,
                resets_at: 4_102_444_800,
                observed_at,
                limit_reached: false,
                overage_covered: false,
            }),
            seven_day: None,
        };
        window::store_for(state, window::CODEX_USAGE_PROVIDER, &reading).expect("store");
    }

    fn update(used: u64) -> Value {
        json!({"rateLimits": {"primary": {"usedPercent": used, "windowDurationMins": 300,
            "resetsAt": 4_102_444_800_u64}}})
    }

    #[test]
    fn a_rate_limits_notification_updates_the_persisted_codex_windows() {
        let dir = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(dir.path().to_path_buf());
        assert!(run_turn(TURN_RATE_LIMITS, &state), "turn completes");
        let stored = window::load_for(&state, window::CODEX_USAGE_PROVIDER).expect("persisted");
        assert_eq!(stored.five_hour.map(|w| w.used_percentage), Some(42.0));
        assert_eq!(stored.seven_day.map(|w| w.used_percentage), Some(7.0));
    }

    #[test]
    fn the_notification_and_a_rollout_record_map_to_identical_windows() {
        let at = window::parse_rfc3339_utc("2026-02-26T18:52:21Z").expect("time");
        let rollout = r#"{"timestamp":"2026-02-26T18:52:21Z","type":"event_msg","payload":{"type":"token_count","rate_limits":{"primary":{"used_percent":42.0,"window_minutes":300,"resets_at":4102444800},"secondary":{"used_percent":7.0,"window_minutes":10080,"resets_at":4102876800},"credits":{"has_credits":true},"rate_limit_reached_type":null}}}"#;
        let notification = json!({"rateLimits": {
            "primary": {"usedPercent": 42, "windowDurationMins": 300, "resetsAt": 4_102_444_800_u64},
            "secondary": {"usedPercent": 7, "windowDurationMins": 10080, "resetsAt": 4_102_876_800_u64},
            "credits": {"hasCredits": true, "unlimited": false, "balance": null},
            "rateLimitReachedType": null}});
        let from_rollout = window::parse_rollout_line(rollout).expect("rollout");
        assert_eq!(rate_limits_windows(&notification, at), Some(from_rollout));
    }

    #[test]
    fn the_newer_reading_wins_in_both_directions() {
        let dir = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(dir.path().to_path_buf());
        let now = now_secs();
        // A rollout reading stamped after the snapshot arrives is kept.
        stored(&state, 11.0, now + 1000);
        driver(&state).record_rate_limits(&update(50));
        assert_eq!(five_hour_percent(&state), Some(11.0));
        // An older reading is replaced by the snapshot.
        stored(&state, 11.0, now - 1000);
        driver(&state).record_rate_limits(&update(50));
        assert_eq!(five_hour_percent(&state), Some(50.0));
    }

    #[test]
    fn a_malformed_notification_changes_nothing_and_the_turn_completes() {
        let dir = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(dir.path().to_path_buf());
        stored(&state, 11.0, 5);
        let before = window::load_for(&state, window::CODEX_USAGE_PROVIDER);
        let bad = r#"< {"method":"account/rateLimits/updated","params":{"rateLimits":{"primary":{"usedPercent":"lots","windowDurationMins":300}}}}"#;
        let transcript = TURN_OK.replace(
            "< {\"method\":\"turn/completed\"",
            &format!("{bad}\n< {{\"method\":\"turn/completed\""),
        );
        assert!(transcript.contains("lots"), "malformed line inserted");
        assert!(run_turn(&transcript, &state), "turn completes");
        assert_eq!(
            window::load_for(&state, window::CODEX_USAGE_PROVIDER),
            before
        );
        for params in [json!({}), json!({"rateLimits": 3}), json!(null)] {
            driver(&state).record_rate_limits(&params);
        }
        assert_eq!(
            window::load_for(&state, window::CODEX_USAGE_PROVIDER),
            before
        );
    }

    #[test]
    fn turn_usage_is_split_like_the_codex_rollout() {
        let total = json!({"inputTokens": 1200, "cachedInputTokens": 200, "outputTokens": 34,
            "reasoningOutputTokens": 10, "totalTokens": 1234});
        let rollout = r#"{"type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":1200,"cached_input_tokens":200,"output_tokens":34,"reasoning_output_tokens":10,"total_tokens":1234}}}}"#;
        let codex = CodexAdapter::new(None).transcript_usage(rollout);
        assert_eq!(codex.map(|u| u.input_tokens), Some(1000));
        assert_eq!(usage_of(&total), codex);
    }

    /// The handshake through `turn/start`'s ack, which carries the turn id.
    fn head() -> String {
        TURN_OK.lines().take(9).collect::<Vec<_>>().join("\n")
    }

    const STEER: &str = r#"> {"id":4,"method":"turn/steer","params":{"threadId":"thr-1","expectedTurnId":"turn-1","input":[{"type":"text","text":"look at the mail","text_elements":[]}]}}"#;
    const INTERRUPT: &str =
        r#"> {"id":4,"method":"turn/interrupt","params":{"threadId":"thr-1","turnId":"turn-1"}}"#;
    const ANSWER: &str = r#"< {"id":4,"result":{"turnId":"turn-1"}}"#;
    const REJECTED: &str =
        r#"< {"id":4,"error":{"code":-32600,"message":"expected turn id does not match"}}"#;
    const INTERRUPTED: &str = r#"< {"method":"turn/completed","params":{"threadId":"thr-1","turn":{"id":"turn-1","items":[],"status":"interrupted"}}}"#;
    const COMPLETED: &str = r#"{"method":"turn/completed","params":{"threadId":"thr-1","turn":{"id":"turn-1","items":[],"status":"completed"}}}"#;
    const SHORT: Duration = Duration::from_millis(300);

    /// A driver already past `turn/start`, run on its own thread against `transcript`.
    struct Running {
        control: TurnControl,
        driver: std::thread::JoinHandle<()>,
        /// Lets a test play a server frame the script does not.
        server: PipeWriter,
        sent: Arc<Mutex<Vec<String>>>,
        state: StateDir,
        _dir: tempfile::TempDir,
    }

    fn running(transcript: &str) -> Running {
        let dir = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(dir.path().to_path_buf());
        let (reader, writer) = pipe().expect("pipe");
        let server = writer.try_clone().expect("clone");
        let sent = Arc::new(Mutex::new(Vec::new()));
        let script = Script {
            server: FakeServer::new(parse_transcript(transcript).expect("transcript")),
            out: writer,
            buf: Vec::new(),
            sent: sent.clone(),
        };
        let mut conn = Conn::new(script, reader);
        let deadline = Instant::now() + Duration::from_secs(5);
        conn.initialize("0.0.0", deadline).expect("handshake");
        start(
            &mut conn,
            json!({"approvalPolicy": "never", "cwd": "/repo", "sandbox": "workspace-write"}),
            "do the work",
            deadline,
        )
        .expect("turn/start written");
        let (control, receiver) = TurnControl::channel();
        let driver = driver_with_control(&state, receiver);
        Running {
            control,
            driver: std::thread::spawn(move || driver.run(conn)),
            server,
            sent,
            state,
            _dir: dir,
        }
    }

    impl Running {
        fn sent(&self) -> Vec<String> {
            self.sent.lock().map(|s| s.clone()).unwrap_or_default()
        }

        /// Ends the turn from the server side, then waits for the driver; returns the decision log.
        fn finish(self, frames: &[&str]) -> String {
            let mut server = self.server;
            for frame in frames {
                writeln!(server, "{frame}").expect("server frame");
            }
            drop(server);
            self.driver.join().expect("driver exits");
            std::fs::read_to_string(self.state.root().join("logs/decisions.jsonl"))
                .unwrap_or_default()
        }
    }

    #[test]
    fn a_steer_carries_the_turn_id_from_the_ack_and_the_text() {
        let run = running(&format!("{}\n{STEER}\n{ANSWER}\n< {COMPLETED}", head()));
        assert!(
            run.control
                .steer("look at the mail", Duration::from_secs(5))
        );
        let sent: Value = serde_json::from_str(run.sent().last().expect("sent")).expect("json");
        let expected: Value =
            serde_json::from_str(STEER.strip_prefix("> ").expect("marker")).expect("json");
        assert_eq!(sent, expected);
        let log = run.finish(&[]);
        assert!(log.contains("turn-completed"), "{log}");
    }

    #[test]
    fn a_rejected_steer_is_false_and_the_turn_still_completes() {
        let run = running(&format!("{}\n{STEER}\n{REJECTED}\n< {COMPLETED}", head()));
        assert!(
            !run.control
                .steer("look at the mail", Duration::from_secs(5))
        );
        let log = run.finish(&[]);
        assert!(
            log.contains("turn-completed") && !log.contains("turn-failed"),
            "{log}"
        );
    }

    #[test]
    fn a_steer_with_no_answer_times_out_and_a_late_error_does_not_fail_the_turn() {
        let run = running(&format!("{}\n{STEER}", head()));
        let started = Instant::now();
        assert!(!run.control.steer("look at the mail", SHORT));
        assert!(
            started.elapsed() < Duration::from_secs(4),
            "bounded by the wait"
        );
        let late = r#"{"id":4,"error":{"code":-32600,"message":"late"}}"#;
        let log = run.finish(&[late, COMPLETED]);
        assert!(
            log.contains("turn-completed") && !log.contains("turn-failed"),
            "{log}"
        );
    }

    #[test]
    fn a_steer_without_a_turn_id_writes_nothing() {
        let no_id = head().replacen(
            r#""turn":{"id":"turn-1","items":[],"status":"inProgress"}"#,
            r#""turn":{}"#,
            1,
        );
        let run = running(&no_id);
        assert!(!run.control.steer("look at the mail", SHORT));
        assert_eq!(
            run.sent().len(),
            4,
            "nothing after turn/start: {:?}",
            run.sent()
        );
        run.finish(&[COMPLETED]);
    }

    #[test]
    fn a_steer_after_the_driver_exits_is_false_at_once() {
        let run = running(TURN_OK);
        let control = run.control.clone();
        run.finish(&[]);
        let started = Instant::now();
        assert!(!control.steer("late", Duration::from_secs(5)));
        assert!(!control.interrupt(Duration::from_secs(5)));
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn an_interrupt_that_completes_as_interrupted_is_clean_not_a_failed_turn() {
        let run = running(&format!(
            "{}\n{INTERRUPT}\n< {{\"id\":4,\"result\":{{}}}}\n{INTERRUPTED}",
            head()
        ));
        assert!(run.control.interrupt(Duration::from_secs(5)));
        let log = run.finish(&[]);
        assert!(log.contains("turn-interrupted"), "{log}");
        assert!(!log.contains("turn-failed"), "{log}");
    }

    #[test]
    fn an_interrupt_with_no_reply_is_false_within_its_bound() {
        let run = running(&format!("{}\n{INTERRUPT}", head()));
        let started = Instant::now();
        assert!(!run.control.interrupt(SHORT));
        assert!(
            started.elapsed() < Duration::from_secs(4),
            "bounded by the wait"
        );
        run.finish(&[COMPLETED]);
    }

    #[test]
    fn an_interrupt_answered_but_never_completed_is_false_within_its_bound() {
        let run = running(&format!(
            "{}\n{INTERRUPT}\n< {{\"id\":4,\"result\":{{}}}}",
            head()
        ));
        let started = Instant::now();
        assert!(!run.control.interrupt(SHORT));
        assert!(
            started.elapsed() < Duration::from_secs(4),
            "bounded by the wait"
        );
        run.finish(&[COMPLETED]);
    }
}
