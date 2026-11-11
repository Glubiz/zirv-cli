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
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use super::codec::{ClientRequest, CodecError, ServerFrame, ServerNotification};
use super::probe::{self, Conn, FallBackReason, Next};
use super::translate;
use crate::commands::ctx::event::TranscriptUsage;
use crate::commands::ctx::exec::accounting::TurnUsage;
use crate::commands::ctx::log;
use crate::commands::ctx::state::{StateDir, now_secs};
use crate::commands::ctx::supervise::{ChildGuard, OutputTap};

/// Budget for `thread/start`'s answer, and for `turn/start`'s ack in the driver.
pub const START_TIMEOUT: Duration = Duration::from_secs(60);

/// A turn running over app-server: the child exec supervises and its handles.
pub struct Launched {
    pub child: Child,
    pub guard: ChildGuard,
    pub tap: OutputTap,
    pub usage: TurnUsage,
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
}

impl Driver {
    fn run(mut self, mut conn: Conn) {
        // Cleared by `turn/start`'s ack; until then a quiet server fails the turn.
        let mut ack_deadline = Some(Instant::now() + START_TIMEOUT);
        let end = loop {
            let frame = match conn.next_frame(ack_deadline) {
                Next::Frame(frame) => frame,
                Next::TimedOut => break End::Failed("turn/start was not acknowledged".into()),
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
                    // `turn/start` is the only request still in flight.
                    let Ok(Some(resolved)) = conn.resolve(&frame) else {
                        continue;
                    };
                    match resolved.outcome {
                        Ok(_) => ack_deadline = None,
                        Err(rpc) => {
                            break End::Failed(format!(
                                "turn/start error {}: {}",
                                rpc.code, rpc.message
                            ));
                        }
                    }
                }
                Ok(ServerFrame::Notification(_)) => {}
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
            End::Failed(message) => {
                self.print_error(&message);
                self.note("turn-failed", &message);
                fail_child(self.group);
            }
        }
        // Drain to EOF so the server never writes into a closed pipe.
        while let Next::Frame(_) = conn.next_frame(None) {}
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
    use std::io::{PipeWriter, pipe};

    const TURN_OK: &str = include_str!("../../../../../tests/fixtures/app-server-turn-ok.jsonl");

    /// Checks each client line against the script and feeds back its answer.
    struct Script {
        server: FakeServer,
        out: PipeWriter,
        buf: Vec<u8>,
    }

    impl Write for Script {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.buf.extend_from_slice(bytes);
            while let Some(end) = self.buf.iter().position(|b| *b == b'\n') {
                let line: Vec<u8> = self.buf.drain(..=end).collect();
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

    #[test]
    fn turn_usage_is_split_like_the_codex_rollout() {
        let total = json!({"inputTokens": 1200, "cachedInputTokens": 200, "outputTokens": 34,
            "reasoningOutputTokens": 10, "totalTokens": 1234});
        let rollout = r#"{"type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":1200,"cached_input_tokens":200,"output_tokens":34,"reasoning_output_tokens":10,"total_tokens":1234}}}}"#;
        let codex = CodexAdapter::new(None).transcript_usage(rollout);
        assert_eq!(codex.map(|u| u.input_tokens), Some(1000));
        assert_eq!(usage_of(&total), codex);
    }
}
