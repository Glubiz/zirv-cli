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

use std::collections::HashMap;
use std::io::Write;
use std::process::{Child, Command};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use super::approve::{self, Verdict};
use super::codec::{
    ClientRequest, CodecError, RequestId, ServerFrame, ServerNotification, ServerRequest,
};
use super::probe::{self, Conn, FallBackReason, Next};
use super::translate;
use crate::commands::ctx::approvals::{self, Decision, RequestDetails};
use crate::commands::ctx::event::TranscriptUsage;
use crate::commands::ctx::exec::accounting::TurnUsage;
use crate::commands::ctx::exec::launch::{ApprovalBridge, Control, TurnControl};
use crate::commands::ctx::log;
use crate::commands::ctx::state::{StateDir, now_secs};
use crate::commands::ctx::supervise::{ChildGuard, OutputTap};
use crate::commands::ctx::window;

/// Budget for `thread/start`'s answer, and for `turn/start`'s ack in the driver.
pub const START_TIMEOUT: Duration = Duration::from_secs(60);

/// How often the driver looks up from the server's frames for a `Control`.
const CONTROL_POLL: Duration = Duration::from_millis(100);

/// How long past its hold a request may stay pending before the driver declines it itself.
const HOLD_SLACK: Duration = Duration::from_secs(5);

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
    bridge: Option<ApprovalBridge>,
    state: &StateDir,
    session: &str,
) -> Result<Launched, FallBackReason> {
    let mut plan = translate::plan(exec_command, stdin_prompt)?;
    let bridge = approve::seed_policy(&mut plan.thread, plan.approval_explicit, bridge);
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
    let (verdicts_tx, verdicts) = mpsc::channel();
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
        bridge,
        pending: HashMap::new(),
        next_hold: 0,
        verdicts_tx,
        verdicts,
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
    /// The approvals bridge, when the operator opted in; otherwise every server request is refused.
    bridge: Option<ApprovalBridge>,
    /// Approval requests held for the operator, by the server's request id. An id leaves this map
    /// before its answer is written, so each is answered at most once.
    pending: HashMap<RequestId, Pending>,
    next_hold: u64,
    /// Holder threads report here; the driver stays the only writer to the server.
    verdicts_tx: Sender<Held>,
    verdicts: Receiver<Held>,
}

/// A request waiting on the operator.
struct Pending {
    /// Tells this hold from a later request that reuses the id.
    hold: u64,
    asked: Instant,
    /// The driver declines the request itself past this, whatever the holder does.
    deadline: Instant,
}

/// A holder thread's report: which hold it was, and the operator's decision if any.
struct Held {
    id: RequestId,
    hold: u64,
    decision: Option<Decision>,
}

impl Driver {
    fn run(mut self, mut conn: Conn) {
        // Cleared by `turn/start`'s ack; until then a quiet server fails the turn.
        let mut ack_deadline = Some(Instant::now() + START_TIMEOUT);
        let end = loop {
            self.drain_control(&mut conn);
            self.drain_verdicts(&mut conn);
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
                    self.on_request(&mut conn, id, request);
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
                            self.note("turn-acked", self.turn_id.as_deref().unwrap_or(""));
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
                Ok(ServerFrame::Notification(ServerNotification::RequestResolved(id))) => {
                    // Codex no longer waits: any answer for it is dropped.
                    if self.pending.remove(&id).is_some() {
                        self.note(
                            "approval-declined",
                            &format!("resolved-elsewhere id={}", approve::show(&id)),
                        );
                    }
                }
                Err(error) => {
                    let _ = writeln!(
                        std::io::stderr(),
                        "zirv ctx exec: skipped an app-server frame: {error}"
                    );
                }
            }
        };
        self.decline_pending(&mut conn, "error", "turn ended");
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

    /// Serve the supervisor's requests. Until `turn/start`'s answer there is
    /// no turn id, so `steer` and `interrupt` answer `false` at once.
    fn drain_control(&mut self, conn: &mut Conn) {
        if let Some((_, reply)) = self
            .interrupting
            .take_if(|(until, _)| Instant::now() >= *until)
        {
            self.note("interrupt-failed", "the turn did not end in time");
            let _ = reply.send(false);
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

    /// A server request: with the bridge on, command approvals are held for the operator and
    /// file changes declined; everything else, and everything with the bridge off, is refused.
    fn on_request(&mut self, conn: &mut Conn, id: RequestId, request: ServerRequest) {
        match request {
            ServerRequest::CommandApproval(params) if self.bridge.is_some() => {
                self.command_approval(conn, id, &params);
            }
            // The params carry no diff, so the operator could only approve blind.
            ServerRequest::FileChangeApproval(_) if self.bridge.is_some() => {
                self.answer(conn, &id, Verdict::Decline("policy"), "file change");
            }
            other => {
                let _ = conn.refuse(&id, &other);
            }
        }
    }

    fn command_approval(&mut self, conn: &mut Conn, id: RequestId, params: &Value) {
        let Some(bridge) = self.bridge.clone() else {
            return;
        };
        let ask = match approve::parse_command(params, &self.thread_id, self.turn_id.as_deref()) {
            Ok(ask) => ask,
            Err(why) => return self.answer(conn, &id, Verdict::Decline("error"), why),
        };
        let Some(parent) = bridge.parent else {
            return self.answer(conn, &id, Verdict::Decline("no-parent"), "");
        };
        if self.pending.len() >= approvals::MAX_PENDING {
            return self.answer(conn, &id, Verdict::Decline("error"), "too many pending");
        }
        self.next_hold += 1;
        let hold = self.next_hold;
        let now = Instant::now();
        self.note(
            "approval-requested",
            &format!(
                "id={} item={} parent={parent} command={}",
                approve::show(&id),
                ask.item_id,
                approvals::redacted_preview(&ask.command)
            ),
        );
        let (state, tx, held_id, key) = (
            self.state.clone(),
            self.verdicts_tx.clone(),
            id.clone(),
            approve::distinct(&id, &ask),
        );
        let spawned = std::thread::Builder::new()
            .name("zirv-codex-approval".into())
            .spawn(move || {
                let details = RequestDetails {
                    cwd: ask.cwd,
                    reason: ask.reason,
                    ..RequestDetails::default()
                };
                // A panicking holder still reports, as no answer.
                let decision = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    approvals::hold_for_dashboard(
                        &state,
                        &parent,
                        approve::TOOL,
                        &key,
                        &ask.command,
                        details,
                        bridge.hold,
                    )
                }))
                .ok()
                .flatten();
                let _ = tx.send(Held {
                    id: held_id,
                    hold,
                    decision,
                });
            });
        if spawned.is_err() {
            return self.answer(conn, &id, Verdict::Decline("error"), "no holder thread");
        }
        self.pending.insert(
            id,
            Pending {
                hold,
                asked: now,
                deadline: now + bridge.hold + HOLD_SLACK,
            },
        );
    }

    /// Write the holders' decisions, and decline whatever outlived its hold.
    fn drain_verdicts(&mut self, conn: &mut Conn) {
        while let Ok(held) = self.verdicts.try_recv() {
            let current = self.pending.get(&held.id).map(|p| (p.hold, p.asked));
            let Some((_, asked)) = current.filter(|(hold, _)| *hold == held.hold) else {
                // Resolved, timed out, or the id was reused: never applied to another request.
                self.note(
                    "approval-late",
                    &format!("id={} dropped", approve::show(&held.id)),
                );
                continue;
            };
            self.pending.remove(&held.id);
            let hold = self.bridge.as_ref().map_or(Duration::ZERO, |b| b.hold);
            let verdict = approve::decide(held.decision, asked.elapsed(), hold);
            self.answer(conn, &held.id, verdict, "");
        }
        let now = Instant::now();
        let expired: Vec<RequestId> = self
            .pending
            .iter()
            .filter(|(_, p)| now >= p.deadline)
            .map(|(id, _)| id.clone())
            .collect();
        for id in expired {
            self.pending.remove(&id);
            self.answer(conn, &id, Verdict::Decline("timeout"), "holder gone");
        }
    }

    /// Decline every request still waiting; the server may already be gone, so a failed write is only noted.
    fn decline_pending(&mut self, conn: &mut Conn, reason: &'static str, why: &str) {
        let mut waiting: Vec<_> = std::mem::take(&mut self.pending).into_iter().collect();
        waiting.sort_by_key(|(_, p)| p.hold);
        for (id, _) in waiting {
            self.answer(conn, &id, Verdict::Decline(reason), why);
        }
    }

    /// Write one request's answer and log it. The only place an approval answer is written.
    fn answer(&self, conn: &mut Conn, id: &RequestId, verdict: Verdict, extra: &str) {
        let (action, detail) = match verdict {
            Verdict::Accept => ("approval-decided", "allow".to_string()),
            Verdict::Deny => ("approval-decided", "deny".to_string()),
            Verdict::Decline(reason) => ("approval-declined", reason.to_string()),
        };
        let extra = if extra.is_empty() {
            String::new()
        } else {
            format!(" ({extra})")
        };
        match conn.answer(id, approve::wire(verdict)) {
            Ok(()) => self.note(action, &format!("{detail} id={}{extra}", approve::show(id))),
            Err(error) => self.note(
                "approval-declined",
                &format!("error id={} write failed: {error}", approve::show(id)),
            ),
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
        let (verdicts_tx, verdicts) = mpsc::channel();
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
            bridge: None,
            pending: HashMap::new(),
            next_hold: 0,
            verdicts_tx,
            verdicts,
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
        running_with(transcript, None, tempfile::tempdir().expect("tempdir"))
    }

    /// `running` with the approvals bridge `bridge`, in `dir` (which a stub dashboard may already use).
    fn running_with(
        transcript: &str,
        bridge: Option<ApprovalBridge>,
        dir: tempfile::TempDir,
    ) -> Running {
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
        let mut driver = driver_with_control(&state, receiver);
        driver.bridge = bridge;
        Running {
            control,
            driver: std::thread::spawn(move || driver.run(conn)),
            server,
            sent,
            state,
            _dir: dir,
        }
    }

    /// `running`, once the driver has taken `turn/start`'s answer; controls
    /// sent earlier fail at once, so tests that steer need this.
    fn running_acked(transcript: &str) -> Running {
        acked(running(transcript))
    }

    fn acked(run: Running) -> Running {
        let log = run.state.root().join("logs/decisions.jsonl");
        let deadline = Instant::now() + Duration::from_secs(5);
        while !std::fs::read_to_string(&log).is_ok_and(|text| text.contains("turn-acked")) {
            assert!(Instant::now() < deadline, "the driver never took the ack");
            std::thread::sleep(Duration::from_millis(10));
        }
        run
    }

    impl Running {
        fn sent(&self) -> Vec<String> {
            self.sent.lock().map(|s| s.clone()).unwrap_or_default()
        }

        /// `finish`, also returning every line the client wrote.
        fn finish_with_sent(self, frames: &[&str]) -> (String, Vec<String>) {
            let sent = self.sent.clone();
            let log = self.finish(frames);
            (log, sent.lock().map(|s| s.clone()).unwrap_or_default())
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
        let run = running_acked(&format!("{}\n{STEER}\n{ANSWER}\n< {COMPLETED}", head()));
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
        let run = running_acked(&format!("{}\n{STEER}\n{REJECTED}\n< {COMPLETED}", head()));
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
        let run = running_acked(&format!("{}\n{STEER}", head()));
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
        let run = running_acked(&no_id);
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
        let run = running_acked(TURN_OK);
        let control = run.control.clone();
        run.finish(&[]);
        let started = Instant::now();
        assert!(!control.steer("late", Duration::from_secs(5)));
        assert!(!control.interrupt(Duration::from_secs(5)));
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn an_interrupt_that_completes_as_interrupted_is_clean_not_a_failed_turn() {
        let run = running_acked(&format!(
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
        let run = running_acked(&format!("{}\n{INTERRUPT}", head()));
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
        let run = running_acked(&format!(
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

    #[test]
    fn a_late_steer_answer_never_decides_a_later_interrupt() {
        let interrupt = INTERRUPT.replace(r#""id":4"#, r#""id":5"#);
        // The steer's answer arrives late, in the middle of the interrupt's wait.
        let run = running_acked(&format!(
            "{}\n{STEER}\n{interrupt}\n{REJECTED}\n< {{\"id\":5,\"result\":{{}}}}\n{INTERRUPTED}",
            head()
        ));
        assert!(!run.control.steer("look at the mail", SHORT));
        assert!(
            run.control.interrupt(Duration::from_secs(5)),
            "the interrupt's own answer decides it, not the steer's late one"
        );
        let log = run.finish(&[]);
        assert!(
            log.contains("turn-interrupted") && !log.contains("turn-failed"),
            "{log}"
        );
    }

    #[test]
    fn controls_sent_before_the_turn_ack_fail_at_once() {
        // The server never answers `turn/start`, so there is no turn id yet.
        let mut run = running(&TURN_OK.lines().take(8).collect::<Vec<_>>().join("\n"));
        let started = Instant::now();
        assert!(!run.control.interrupt(Duration::from_secs(3)));
        assert!(!run.control.steer("early", Duration::from_secs(3)));
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "answered false at once, not after the bound: {:?}",
            started.elapsed()
        );
        writeln!(
            run.server,
            "{}",
            TURN_OK
                .lines()
                .nth(8)
                .expect("ack")
                .trim_start_matches("< ")
        )
        .expect("ack");
        run.finish(&[COMPLETED]);
    }
    const REFUSED: &str =
        r#""error":{"code":-32000,"message":"a headless zirv turn answers no requests"}"#;

    fn frame(text: &str) -> Value {
        serde_json::from_str(text).expect("json frame")
    }

    fn last_sent(sent: &[String]) -> Value {
        frame(sent.last().expect("a sent line"))
    }

    /// A server request frame, as the script emits it.
    fn server_request(id: u64, method: &str, params: &str) -> String {
        format!(r#"< {{"id":{id},"method":"{method}","params":{params}}}"#)
    }

    fn command_params(command: &str) -> String {
        json!({"threadId": "thr-1", "turnId": "turn-1", "itemId": "item-1", "startedAtMs": 1,
            "command": command, "cwd": "/repo"})
        .to_string()
    }

    #[test]
    fn bridge_off_refuses_an_approval_request_as_today() {
        let request = server_request(
            7,
            "item/commandExecution/requestApproval",
            &command_params("rm -rf /"),
        );
        let run = running_acked(&format!(
            "{}\n{request}\n> {{\"id\":7,{REFUSED}}}\n< {COMPLETED}",
            head()
        ));
        let sent = run.sent();
        let log = run.finish(&[]);
        assert_eq!(last_sent(&sent), frame(&format!(r#"{{"id":7,{REFUSED}}}"#)));
        assert!(log.contains("turn-completed"), "{log}");
    }

    #[test]
    fn other_server_requests_are_still_refused() {
        let cases = [
            ("item/permissions/requestApproval", None),
            ("applyPatchApproval", None),
            ("execCommandApproval", None),
            ("item/tool/call", None),
            ("attestation/generate", None),
            (
                "mcpServer/elicitation/request",
                Some(r#""result":{"action":"cancel"}"#),
            ),
        ];
        for bridge in [None, Some(bridge_to("abc123", Duration::from_secs(5)))] {
            for (method, answer) in cases {
                let request = server_request(9, method, &command_params("ls"));
                let wanted = answer.unwrap_or(REFUSED);
                let run = acked(running_with(
                    &format!(
                        "{}\n{request}\n> {{\"id\":9,{wanted}}}\n< {COMPLETED}",
                        head()
                    ),
                    bridge.clone(),
                    tempfile::tempdir().expect("tempdir"),
                ));
                let sent = run.sent();
                run.finish(&[]);
                assert_eq!(
                    last_sent(&sent),
                    frame(&format!(r#"{{"id":9,{wanted}}}"#)),
                    "{method} with the bridge {}",
                    if bridge.is_some() { "on" } else { "off" }
                );
            }
        }
    }
    // ---- the approvals bridge (#950 slice 6) ----

    fn bridge_to(parent: &str, hold: Duration) -> ApprovalBridge {
        ApprovalBridge {
            parent: Some(parent.to_string()),
            hold,
        }
    }

    /// A stand-in dashboard for the pane `short`: a registered session owned by `owner`, and a
    /// socket that records every request it is sent and answers them in order (`None` never answers).
    struct Stub {
        short: String,
        _session: crate::commands::ctx::sessions::SessionGuard,
        seen: Arc<Mutex<Vec<approvals::Request>>>,
    }

    fn stub_dashboard(state: &StateDir, owner: u32, replies: Vec<Option<&'static str>>) -> Stub {
        use crate::commands::ctx::sessions::{Record, SessionGuard, Verb};
        use std::io::BufRead;
        let mut record = Record::new(
            "pane-abc1",
            "claude",
            std::path::Path::new("/repo"),
            Verb::Dash,
        );
        record.owner_pid = Some(owner);
        let short = record.short.clone();
        let session = SessionGuard::register(state, record);
        std::fs::create_dir_all(state.sockets()).expect("sockets dir");
        let listener = std::os::unix::net::UnixListener::bind(approvals::socket_path(state, owner))
            .expect("bind");
        let seen: Arc<Mutex<Vec<approvals::Request>>> = Arc::default();
        let replies = Arc::new(Mutex::new(std::collections::VecDeque::from(replies)));
        let record_seen = seen.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let (seen, replies) = (record_seen.clone(), replies.clone());
                std::thread::spawn(move || {
                    let mut line = String::new();
                    if std::io::BufReader::new(&stream)
                        .read_line(&mut line)
                        .is_err()
                    {
                        return;
                    }
                    if let (Ok(request), Ok(mut seen)) =
                        (serde_json::from_str(line.trim()), seen.lock())
                    {
                        seen.push(request);
                    }
                    let reply = replies
                        .lock()
                        .ok()
                        .and_then(|mut r| r.pop_front())
                        .flatten();
                    match reply {
                        Some(reply) => {
                            let _ = (&stream).write_all(reply.as_bytes());
                            std::thread::sleep(Duration::from_millis(500));
                        }
                        // Never answered: the connection stays open until the hold ends.
                        None => std::thread::sleep(Duration::from_secs(10)),
                    }
                });
            }
        });
        Stub {
            short,
            _session: session,
            seen,
        }
    }

    impl Stub {
        fn requests(&self) -> Vec<approvals::Request> {
            self.seen.lock().map(|s| s.clone()).unwrap_or_default()
        }
    }

    const ALLOW: &str = "{\"decision\":\"allow\"}\n";
    const COMMAND: &str = "item/commandExecution/requestApproval";

    /// Frames in `sent` that answer server request `id`.
    fn answers(sent: &[String], id: u64) -> Vec<Value> {
        sent.iter()
            .map(|line| frame(line))
            .filter(|f| f["id"] == id && f.get("method").is_none())
            .collect()
    }

    fn accept(id: u64) -> String {
        format!(r#"> {{"id":{id},"result":{{"decision":"accept"}}}}"#)
    }

    fn decline(id: u64) -> String {
        format!(r#"> {{"id":{id},"result":{{"decision":"decline"}}}}"#)
    }

    fn assert_no_wider_scope(sent: &[String]) {
        for line in sent {
            for word in ["acceptForSession", "Amendment", "cancel"] {
                assert!(!line.contains(word), "{word} written: {line}");
            }
        }
    }

    /// One command approval, held against a stub that answers `reply`; returns the lines the client wrote and the log.
    fn one_approval(
        reply: Option<&'static str>,
        verdict: &str,
        hold: Duration,
    ) -> (Vec<String>, String) {
        let dir = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(dir.path().to_path_buf());
        let stub = stub_dashboard(&state, std::process::id(), vec![reply]);
        let transcript = format!(
            "{}\n{}\n{verdict}\n< {COMPLETED}",
            head(),
            server_request(7, COMMAND, &command_params("cargo test"))
        );
        let run = acked(running_with(
            &transcript,
            Some(bridge_to(&stub.short, hold)),
            dir,
        ));
        let (log, sent) = run.finish_with_sent(&[]);
        assert_no_wider_scope(&sent);
        (sent, log)
    }

    #[test]
    fn an_allow_from_the_parent_dashboard_writes_accept_with_the_servers_id() {
        let (sent, log) = one_approval(Some(ALLOW), &accept(7), Duration::from_secs(5));
        assert_eq!(
            answers(&sent, 7),
            vec![frame(r#"{"id":7,"result":{"decision":"accept"}}"#)]
        );
        assert!(log.contains("approval-requested"), "{log}");
        assert!(
            log.contains("approval-decided") && log.contains("allow id=7"),
            "{log}"
        );
    }

    #[test]
    fn deny_release_and_garbage_decisions_write_decline() {
        for (reply, noted) in [
            ("{\"decision\":\"deny\"}\n", "deny id=7"),
            ("{\"decision\":\"release\"}\n", "policy id=7"),
            ("{\"decision\":\"acceptForSession\"}\n", "no-dashboard id=7"),
            ("not json\n", "no-dashboard id=7"),
        ] {
            let (sent, log) = one_approval(Some(reply), &decline(7), Duration::from_secs(5));
            assert_eq!(
                answers(&sent, 7),
                vec![frame(r#"{"id":7,"result":{"decision":"decline"}}"#)],
                "{reply}"
            );
            assert!(log.contains(noted), "{reply}: {log}");
        }
    }

    #[test]
    fn allow_always_is_accepted_once_and_never_as_a_session_scope() {
        let (sent, _) = one_approval(
            Some("{\"decision\":\"allow_always\"}\n"),
            &accept(7),
            Duration::from_secs(5),
        );
        assert_eq!(
            answers(&sent, 7),
            vec![frame(r#"{"id":7,"result":{"decision":"accept"}}"#)]
        );
    }

    #[test]
    fn the_request_never_offers_an_always_rule() {
        let dir = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(dir.path().to_path_buf());
        let stub = stub_dashboard(&state, std::process::id(), vec![Some(ALLOW)]);
        let transcript = format!(
            "{}\n{}\n{}\n< {COMPLETED}",
            head(),
            server_request(7, COMMAND, &command_params("cargo test")),
            accept(7)
        );
        let run = acked(running_with(
            &transcript,
            Some(bridge_to(&stub.short, Duration::from_secs(5))),
            dir,
        ));
        run.finish(&[]);
        let requests = stub.requests();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].always, None);
        assert_eq!(requests[0].tool, "codex command");
        assert_eq!(requests[0].short, stub.short);
    }

    /// A run whose parent short is `bridge_parent` and whose only dashboard is `stub_for`, if any.
    fn declined_without_a_live_dashboard(
        setup: impl FnOnce(&StateDir) -> (Option<Stub>, Option<String>),
    ) -> (String, Vec<String>, Option<Stub>) {
        let dir = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(dir.path().to_path_buf());
        let (stub, parent) = setup(&state);
        let transcript = format!(
            "{}\n{}\n{}\n< {COMPLETED}",
            head(),
            server_request(7, COMMAND, &command_params("cargo test")),
            decline(7)
        );
        let bridge = ApprovalBridge {
            parent,
            hold: Duration::from_secs(5),
        };
        let started = Instant::now();
        let (log, sent) = acked(running_with(&transcript, Some(bridge), dir)).finish_with_sent(&[]);
        assert!(
            started.elapsed() < Duration::from_secs(4),
            "declined at once, not at the hold"
        );
        assert_eq!(
            answers(&sent, 7),
            vec![frame(r#"{"id":7,"result":{"decision":"decline"}}"#)]
        );
        (log, sent, stub)
    }

    #[test]
    fn no_dashboard_declines() {
        let (log, _, _) = declined_without_a_live_dashboard(|_| (None, Some("abc12345".into())));
        assert!(log.contains("no-dashboard id=7"), "{log}");
    }

    #[test]
    fn no_parent_short_declines_and_nothing_reaches_a_dashboard() {
        let (log, _, stub) = declined_without_a_live_dashboard(|state| {
            (
                Some(stub_dashboard(state, std::process::id(), vec![Some(ALLOW)])),
                None,
            )
        });
        assert!(log.contains("no-parent id=7"), "{log}");
        assert!(stub.expect("stub").requests().is_empty());
    }

    #[test]
    fn a_dead_owner_pid_declines() {
        let (log, _, stub) = declined_without_a_live_dashboard(|state| {
            let mut gone = Command::new("true").spawn().expect("spawn");
            gone.wait().expect("wait");
            let stub = stub_dashboard(state, gone.id(), vec![Some(ALLOW)]);
            let short = stub.short.clone();
            (Some(stub), Some(short))
        });
        assert!(log.contains("no-dashboard id=7"), "{log}");
        assert!(stub.expect("stub").requests().is_empty());
    }

    #[test]
    fn a_peer_pid_that_is_not_the_recorded_owner_declines_before_anything_is_sent() {
        let (log, _, stub) = declined_without_a_live_dashboard(|state| {
            // Alive, but not the process that owns the socket this test binds.
            let stub = stub_dashboard(
                state,
                std::os::unix::process::parent_id(),
                vec![Some(ALLOW)],
            );
            let short = stub.short.clone();
            (Some(stub), Some(short))
        });
        assert!(log.contains("no-dashboard id=7"), "{log}");
        assert!(
            stub.expect("stub").requests().is_empty(),
            "an unauthenticated peer gets no request"
        );
    }

    #[test]
    fn a_hold_that_times_out_declines() {
        let started = Instant::now();
        let (sent, log) = one_approval(None, &decline(7), Duration::from_millis(300));
        assert!(
            started.elapsed() < Duration::from_secs(4),
            "bounded by the hold"
        );
        assert_eq!(
            answers(&sent, 7),
            vec![frame(r#"{"id":7,"result":{"decision":"decline"}}"#)]
        );
        assert!(log.contains("timeout id=7"), "{log}");
    }

    /// A driver with no server: its writes land in the returned buffer.
    fn lone_driver(state: &StateDir, hold: Duration) -> (Driver, Conn, Arc<Mutex<Vec<u8>>>) {
        #[derive(Clone)]
        struct Capture(Arc<Mutex<Vec<u8>>>);
        impl Write for Capture {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                if let Ok(mut buf) = self.0.lock() {
                    buf.extend_from_slice(bytes);
                }
                Ok(bytes.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let written = Arc::new(Mutex::new(Vec::new()));
        let conn = Conn::new(Capture(written.clone()), std::io::empty());
        let mut driver = driver(state);
        driver.bridge = Some(bridge_to("abc12345", hold));
        driver.turn_id = Some("turn-1".into());
        (driver, conn, written)
    }

    fn written_lines(written: &Arc<Mutex<Vec<u8>>>) -> Vec<String> {
        let bytes = written.lock().map(|b| b.clone()).unwrap_or_default();
        String::from_utf8_lossy(&bytes)
            .lines()
            .map(str::to_string)
            .collect()
    }

    fn pending_at(hold: u64) -> Pending {
        Pending {
            hold,
            asked: Instant::now(),
            deadline: Instant::now() + Duration::from_secs(60),
        }
    }

    #[test]
    fn a_late_verdict_is_dropped_and_never_decides_a_different_request() {
        let dir = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(dir.path().to_path_buf());
        let (mut driver, mut conn, written) = lone_driver(&state, Duration::from_secs(5));
        let (n, next) = (RequestId::Num(7), RequestId::Num(8));
        // 7 already timed out and was declined; 8 is the live request.
        driver.pending.insert(next.clone(), pending_at(2));
        let late = |id: &RequestId, hold: u64| Held {
            id: id.clone(),
            hold,
            decision: Some(Decision::Allow),
        };
        // An allow for the answered id, an allow for the live id from an earlier hold, and one that
        // reuses the id: none may be written.
        for held in [late(&n, 1), late(&next, 1)] {
            driver.verdicts_tx.send(held).expect("send");
        }
        driver.drain_verdicts(&mut conn);
        assert!(
            written_lines(&written).is_empty(),
            "{:?}",
            written_lines(&written)
        );
        assert!(
            driver.pending.contains_key(&next),
            "the live request stays pending"
        );
        driver.verdicts_tx.send(late(&next, 2)).expect("send");
        driver.drain_verdicts(&mut conn);
        assert_eq!(
            written_lines(&written)
                .iter()
                .map(|l| frame(l))
                .collect::<Vec<_>>(),
            vec![frame(r#"{"id":8,"result":{"decision":"accept"}}"#)],
            "only its own hold decides it"
        );
        let log =
            std::fs::read_to_string(state.root().join("logs/decisions.jsonl")).unwrap_or_default();
        assert_eq!(log.matches("approval-late").count(), 2, "{log}");
    }

    #[test]
    fn a_pending_request_past_its_deadline_is_declined_by_the_driver() {
        let dir = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(dir.path().to_path_buf());
        let (mut driver, mut conn, written) = lone_driver(&state, Duration::from_secs(5));
        driver.pending.insert(
            RequestId::Num(7),
            Pending {
                hold: 1,
                asked: Instant::now(),
                deadline: Instant::now(),
            },
        );
        driver.drain_verdicts(&mut conn);
        assert_eq!(
            written_lines(&written)
                .iter()
                .map(|l| frame(l))
                .collect::<Vec<_>>(),
            vec![frame(r#"{"id":7,"result":{"decision":"decline"}}"#)]
        );
        assert!(driver.pending.is_empty());
    }

    #[test]
    fn the_pending_cap_declines_the_overflow_unheld() {
        let dir = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(dir.path().to_path_buf());
        let (mut driver, mut conn, written) = lone_driver(&state, Duration::from_secs(5));
        for n in 0..approvals::MAX_PENDING {
            driver
                .pending
                .insert(RequestId::Num(n as i64 + 100), pending_at(n as u64));
        }
        let request = ServerRequest::CommandApproval(frame(&command_params("ls")));
        driver.on_request(&mut conn, RequestId::Num(7), request);
        assert_eq!(
            written_lines(&written)
                .iter()
                .map(|l| frame(l))
                .collect::<Vec<_>>(),
            vec![frame(r#"{"id":7,"result":{"decision":"decline"}}"#)]
        );
        assert_eq!(driver.pending.len(), approvals::MAX_PENDING);
    }

    #[test]
    fn a_request_for_another_thread_or_turn_or_without_a_command_is_declined_unheld() {
        let dir = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(dir.path().to_path_buf());
        let stub = stub_dashboard(&state, std::process::id(), vec![Some(ALLOW)]);
        let (mut driver, mut conn, written) = lone_driver(&state, Duration::from_secs(5));
        driver.bridge = Some(bridge_to(&stub.short, Duration::from_secs(5)));
        for (n, patch) in [
            json!({"threadId": "thr-other"}),
            json!({"turnId": "turn-other"}),
            json!({"command": null}),
            json!({"itemId": null}),
        ]
        .into_iter()
        .enumerate()
        {
            let mut params = frame(&command_params("ls"));
            params
                .as_object_mut()
                .expect("object")
                .extend(patch.as_object().expect("object").clone());
            driver.on_request(
                &mut conn,
                RequestId::Num(n as i64),
                ServerRequest::CommandApproval(params),
            );
        }
        let lines = written_lines(&written);
        assert_eq!(lines.len(), 4, "{lines:?}");
        assert!(
            lines.iter().all(|l| l.contains(r#""decision":"decline""#)),
            "{lines:?}"
        );
        assert!(driver.pending.is_empty());
        assert!(stub.requests().is_empty(), "nothing was held");
    }

    #[test]
    fn server_request_resolved_drops_pending_without_a_write() {
        let dir = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(dir.path().to_path_buf());
        let stub = stub_dashboard(&state, std::process::id(), vec![None]);
        let transcript = format!(
            "{}\n{}",
            head(),
            server_request(7, COMMAND, &command_params("cargo test"))
        );
        let run = acked(running_with(
            &transcript,
            Some(bridge_to(&stub.short, Duration::from_secs(5))),
            dir,
        ));
        let resolved =
            r#"{"method":"serverRequest/resolved","params":{"threadId":"thr-1","requestId":7}}"#;
        let (log, sent) = run.finish_with_sent(&[resolved, COMPLETED]);
        assert!(
            answers(&sent, 7).is_empty(),
            "no answer for a resolved request: {sent:?}"
        );
        assert!(log.contains("resolved-elsewhere id=7"), "{log}");
        assert!(log.contains("turn-completed"), "{log}");
    }

    #[test]
    fn a_hold_does_not_block_a_steer() {
        let dir = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(dir.path().to_path_buf());
        let stub = stub_dashboard(&state, std::process::id(), vec![None]);
        let transcript = format!(
            "{}\n{}\n{STEER}\n{ANSWER}\n{}",
            head(),
            server_request(7, COMMAND, &command_params("cargo test")),
            decline(7)
        );
        let run = acked(running_with(
            &transcript,
            Some(bridge_to(&stub.short, Duration::from_secs(30))),
            dir,
        ));
        let started = Instant::now();
        assert!(
            run.control
                .steer("look at the mail", Duration::from_secs(5)),
            "the steer is answered while the approval is held"
        );
        assert!(started.elapsed() < Duration::from_secs(4));
        assert_eq!(
            stub.requests().len(),
            1,
            "the approval is held at the dashboard"
        );
        let (log, sent) = run.finish_with_sent(&[COMPLETED]);
        assert!(log.contains("turn-steered"), "{log}");
        assert_eq!(
            answers(&sent, 7),
            vec![frame(r#"{"id":7,"result":{"decision":"decline"}}"#)],
            "the turn's end declines it"
        );
    }

    #[test]
    fn turn_end_declines_every_pending_request() {
        let dir = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(dir.path().to_path_buf());
        let stub = stub_dashboard(&state, std::process::id(), vec![None, None]);
        let transcript = format!(
            "{}\n{}\n{}\n{}\n{}",
            head(),
            server_request(7, COMMAND, &command_params("cargo test")),
            server_request(8, COMMAND, &command_params("cargo test")),
            decline(7),
            decline(8),
        );
        let run = acked(running_with(
            &transcript,
            Some(bridge_to(&stub.short, Duration::from_secs(30))),
            dir,
        ));
        let (log, sent) = run.finish_with_sent(&[COMPLETED]);
        for id in [7, 8] {
            assert_eq!(
                answers(&sent, id),
                vec![frame(&format!(
                    r#"{{"id":{id},"result":{{"decision":"decline"}}}}"#
                ))],
                "{sent:?}"
            );
        }
        assert!(log.contains("turn ended"), "{log}");
        assert_no_wider_scope(&sent);
    }

    #[test]
    fn command_text_is_redacted_escape_free_and_truncation_makes_allow_inert() {
        let dir = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(dir.path().to_path_buf());
        let stub = stub_dashboard(&state, std::process::id(), vec![Some(ALLOW), Some(ALLOW)]);
        let secret = "echo ghp_1234567890abcdefghijklmnopqrstuvwx";
        let long = format!(
            "echo {}",
            (0..60).map(|n| format!("arg{n} ")).collect::<String>()
        );
        let transcript = format!(
            "{}\n{}\n{}\n{}\n{}\n< {COMPLETED}",
            head(),
            server_request(7, COMMAND, &command_params(secret)),
            server_request(
                8,
                COMMAND,
                &command_params(&format!("{long}\u{1b}[31m red")),
            ),
            accept(7),
            accept(8),
        );
        let run = running_with(
            &transcript,
            Some(bridge_to(&stub.short, Duration::from_secs(5))),
            dir,
        );
        let (log, _) = acked(run).finish_with_sent(&[]);
        assert!(
            !log.contains("ghp_1234567890"),
            "the log carries only the redacted command: {log}"
        );
        let requests = stub.requests();
        assert_eq!(requests.len(), 2);
        for request in &requests {
            assert_eq!(request.tool, "codex command");
            assert!(
                !request.preview.contains('\u{1b}') && !request.command.contains('\u{1b}'),
                "no escape reaches the operator"
            );
            assert!(!request.preview.contains("ghp_1234567890"));
            assert!(!request.command.contains("ghp_1234567890"));
        }
        assert_ne!(requests[0].id, requests[1].id);
        let truncated = requests
            .iter()
            .find(|r| r.preview.ends_with('\u{2026}'))
            .expect("the long one");
        assert!(
            !truncated.fully_shown,
            "a truncated command is not answerable"
        );
        let redacted = requests.iter().find(|r| r.preview.contains("redacted"));
        assert!(
            redacted.is_some_and(|r| !r.fully_shown),
            "the secret line is replaced by its summary, and cannot be answered"
        );
    }

    #[test]
    fn two_identical_commands_do_not_collide_on_the_dashboard() {
        let dir = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(dir.path().to_path_buf());
        let stub = stub_dashboard(&state, std::process::id(), vec![None, None]);
        let transcript = format!(
            "{}\n{}\n{}",
            head(),
            server_request(7, COMMAND, &command_params("cargo test")),
            server_request(8, COMMAND, &command_params("cargo test"))
        );
        let run = acked(running_with(
            &transcript,
            Some(bridge_to(&stub.short, Duration::from_secs(30))),
            dir,
        ));
        let deadline = Instant::now() + Duration::from_secs(5);
        while stub.requests().len() < 2 && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        let requests = stub.requests();
        assert_eq!(requests.len(), 2, "both reached the dashboard");
        assert_ne!(requests[0].id, requests[1].id);
        run.finish(&[COMPLETED]);
    }

    #[test]
    fn file_change_requests_are_declined_without_a_hold() {
        let dir = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(dir.path().to_path_buf());
        let stub = stub_dashboard(&state, std::process::id(), vec![Some(ALLOW)]);
        let transcript = format!(
            "{}\n{}\n{}\n< {COMPLETED}",
            head(),
            server_request(
                7,
                "item/fileChange/requestApproval",
                r#"{"threadId":"thr-1","turnId":"turn-1","itemId":"item-1","startedAtMs":1}"#
            ),
            decline(7)
        );
        let run = acked(running_with(
            &transcript,
            Some(bridge_to(&stub.short, Duration::from_secs(5))),
            dir,
        ));
        let (log, sent) = run.finish_with_sent(&[]);
        assert_eq!(
            answers(&sent, 7),
            vec![frame(r#"{"id":7,"result":{"decision":"decline"}}"#)]
        );
        assert!(
            log.contains("approval-declined") && log.contains("policy id=7"),
            "{log}"
        );
        assert!(stub.requests().is_empty(), "no dashboard contact");
    }
}
