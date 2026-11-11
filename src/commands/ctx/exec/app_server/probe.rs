//! Probe, handshake and fall-back decision for the app-server transport.
//!
//! `connect` hands back a handshaken child, or the reason to run `codex exec`
//! instead: any probe, spawn, handshake or decode problem is a
//! `FallBackReason`, with the child already reaped.

use std::collections::VecDeque;
use std::fmt;
use std::io::{Read, Write};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use super::codec::{
    ClientRequest, CodecError, Correlator, RequestId, Resolved, RpcError, ServerFrame,
    ServerRequest, decode_line, encode_error_response, encode_initialized, encode_response,
};
use crate::commands::ctx::adapters::codex;
use crate::commands::ctx::supervise::{self, ChildGuard};

/// Whole handshake budget: spawn to `initialize` response.
pub const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);

/// Frames the reader queues ahead of the client; past it, the server blocks on its stdout pipe.
pub const READER_QUEUE_FRAMES: usize = 64;

/// SIGTERM-to-SIGKILL grace when reaping a server that never got `turn/start`.
const REAP_GRACE: Duration = Duration::from_secs(1);

#[derive(Debug, Clone, PartialEq)]
pub enum FallBackReason {
    /// `codex app-server --help` does not name `--listen`.
    ListenFlagMissing,
    Spawn(String),
    Timeout,
    /// The server closed stdout or exited before answering.
    ServerExited,
    Write(String),
    Decode(CodecError),
    Rpc(RpcError),
    /// The `codex exec` launch carries something with no app-server equivalent.
    Unsupported(String),
}

impl fmt::Display for FallBackReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ListenFlagMissing => write!(f, "`app-server --help` does not offer --listen"),
            Self::Spawn(e) => write!(f, "spawn failed: {e}"),
            Self::Timeout => write!(f, "no answer in time"),
            Self::ServerExited => write!(f, "the server exited before answering"),
            Self::Write(e) => write!(f, "write failed: {e}"),
            Self::Decode(e) => write!(f, "{e}"),
            Self::Rpc(e) => write!(f, "error {}: {}", e.code, e.message),
            Self::Unsupported(what) => write!(f, "no app-server equivalent for {what}"),
        }
    }
}

enum Inbound {
    Frame(Result<ServerFrame, CodecError>),
    Closed,
}

/// What `Conn::next_frame` saw.
pub enum Next {
    Frame(Result<ServerFrame, CodecError>),
    /// The server closed stdout.
    Closed,
    TimedOut,
}

/// `codex app-server --help` names `--listen`. Reuses the codex adapter's
/// bounded help probe; no version gate.
pub fn probe_listen_support(program: &str, bin_args: &[String]) -> bool {
    codex::detect_help_markers(program, bin_args, &["app-server"], &["--listen"])
}

fn spawn_reader(mut stdout: impl Read + Send + 'static) -> Receiver<Inbound> {
    // Bounded so a flooding server blocks on its pipe instead of growing this queue.
    let (tx, rx) = mpsc::sync_channel(READER_QUEUE_FRAMES);
    std::thread::spawn(move || {
        let mut splitter = super::codec::LineSplitter::default();
        let mut chunk = [0u8; 8192];
        loop {
            let n = match stdout.read(&mut chunk) {
                Ok(0) | Err(_) => break,
                Ok(n) => n,
            };
            for line in splitter.push(chunk.get(..n).unwrap_or_default()) {
                let frame = line.and_then(|l| decode_line(&l));
                if tx.send(Inbound::Frame(frame)).is_err() {
                    return;
                }
            }
        }
        let _ = tx.send(Inbound::Closed);
    });
    rx
}

/// The client half of an app-server connection: lines out, frames in.
pub struct Conn {
    stdin: Box<dyn Write + Send>,
    rx: Receiver<Inbound>,
    correlator: Correlator,
    /// Notifications that arrived while a request awaited its answer.
    backlog: VecDeque<ServerFrame>,
}

impl fmt::Debug for Conn {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Conn")
            .field("correlator", &self.correlator)
            .field("backlog", &self.backlog.len())
            .finish_non_exhaustive()
    }
}

impl Conn {
    pub fn new(stdin: impl Write + Send + 'static, stdout: impl Read + Send + 'static) -> Self {
        Self {
            stdin: Box::new(stdin),
            rx: spawn_reader(stdout),
            correlator: Correlator::default(),
            backlog: VecDeque::new(),
        }
    }

    fn write_line(&mut self, line: &str) -> Result<(), FallBackReason> {
        self.stdin
            .write_all(line.as_bytes())
            .and_then(|()| self.stdin.flush())
            .map_err(|e| FallBackReason::Write(e.to_string()))
    }

    /// Write `request` without waiting; `resolve` matches its answer later.
    pub fn send(&mut self, request: &ClientRequest) -> Result<(), FallBackReason> {
        let (_, line) = self
            .correlator
            .send(request)
            .map_err(FallBackReason::Decode)?;
        self.write_line(&line)
    }

    /// The reader's next frame, waiting until `deadline`, or for good without one.
    fn recv(&self, deadline: Option<Instant>) -> Next {
        let inbound = match deadline {
            None => self.rx.recv().ok(),
            Some(deadline) => {
                match self
                    .rx
                    .recv_timeout(deadline.saturating_duration_since(Instant::now()))
                {
                    Err(RecvTimeoutError::Timeout) => return Next::TimedOut,
                    result => result.ok(),
                }
            }
        };
        match inbound {
            Some(Inbound::Frame(frame)) => Next::Frame(frame),
            Some(Inbound::Closed) | None => Next::Closed,
        }
    }

    /// Send `request` and wait until `deadline` for its answer. Notifications
    /// on the way are kept for `next_frame`; server requests are refused.
    pub fn request(
        &mut self,
        request: &ClientRequest,
        deadline: Instant,
    ) -> Result<Value, FallBackReason> {
        self.send(request)?;
        loop {
            let frame = match self.recv(Some(deadline)) {
                Next::Frame(frame) => frame.map_err(FallBackReason::Decode)?,
                Next::Closed => return Err(FallBackReason::ServerExited),
                Next::TimedOut => return Err(FallBackReason::Timeout),
            };
            match frame {
                ServerFrame::Request { id, request } => self.refuse(&id, &request)?,
                ServerFrame::Notification(_) => {
                    // Bounded like the reader queue; the newest frames, which end a turn, stay.
                    if self.backlog.len() == READER_QUEUE_FRAMES {
                        self.backlog.pop_front();
                    }
                    self.backlog.push_back(frame);
                }
                ServerFrame::Response { .. } | ServerFrame::Error { .. } => {
                    if let Some(resolved) = self
                        .correlator
                        .resolve(&frame)
                        .map_err(FallBackReason::Decode)?
                    {
                        return resolved.outcome.map_err(FallBackReason::Rpc);
                    }
                }
            }
        }
    }

    /// `initialize`, then the `initialized` notification.
    pub fn initialize(
        &mut self,
        client_version: &str,
        deadline: Instant,
    ) -> Result<(), FallBackReason> {
        let request = ClientRequest::Initialize {
            client_name: "zirv".into(),
            client_version: client_version.into(),
            experimental_api: false,
        };
        self.request(&request, deadline)?;
        let initialized = encode_initialized().map_err(FallBackReason::Decode)?;
        self.write_line(&initialized)
    }

    /// The next frame, queued ones first, waiting until `deadline` or for good.
    pub fn next_frame(&mut self, deadline: Option<Instant>) -> Next {
        match self.backlog.pop_front() {
            Some(frame) => Next::Frame(Ok(frame)),
            None => self.recv(deadline),
        }
    }

    /// The request a response or error frame answers, if it answers one.
    pub fn resolve(&mut self, frame: &ServerFrame) -> Result<Option<Resolved>, CodecError> {
        self.correlator.resolve(frame)
    }

    /// Decline a server request the way `codex exec` does: an MCP elicitation
    /// is cancelled, anything else gets an error.
    pub fn refuse(
        &mut self,
        id: &RequestId,
        request: &ServerRequest,
    ) -> Result<(), FallBackReason> {
        let line = match request {
            ServerRequest::McpElicitation(_) => encode_response(id, json!({"action": "cancel"})),
            _ => encode_error_response(id, -32000, "a headless zirv turn answers no requests"),
        }
        .map_err(FallBackReason::Decode)?;
        self.write_line(&line)
    }

    /// EOF on the server's stdin; a stdio app-server then exits 0.
    pub fn close_stdin(&mut self) {
        self.stdin = Box::new(std::io::sink());
    }
}

/// A handshaken app-server child that the caller now supervises.
#[derive(Debug)]
pub struct Connected {
    pub child: Child,
    pub guard: ChildGuard,
    pub conn: Conn,
}

impl Connected {
    /// Kill and reap the server; it never got `turn/start`.
    pub fn abandon(mut self) {
        let _ = supervise::terminate_group(&mut self.child, REAP_GRACE);
    }
}

/// Probe `<program> <bin_args> app-server --help`, spawn `command` (an
/// `app-server --listen stdio://` launch) isolated and adopted like every
/// exec child, then handshake within `timeout`.
pub fn connect(
    program: &str,
    bin_args: &[String],
    mut command: Command,
    client_version: &str,
    timeout: Duration,
) -> Result<Connected, FallBackReason> {
    if !probe_listen_support(program, bin_args) {
        return Err(FallBackReason::ListenFlagMissing);
    }
    let deadline = Instant::now() + timeout;
    supervise::guard_cmd_shim_reparse(&command)
        .map_err(|e| FallBackReason::Spawn(e.to_string()))?;
    supervise::isolate_process_tree(&mut command);
    let mut child = command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .map_err(|e| FallBackReason::Spawn(e.to_string()))?;
    let guard = ChildGuard::adopt(Some(child.id()));
    let (Some(stdin), Some(stdout)) = (child.stdin.take(), child.stdout.take()) else {
        let _ = supervise::terminate_group(&mut child, REAP_GRACE);
        return Err(FallBackReason::Spawn("child pipes unavailable".into()));
    };
    let mut connected = Connected {
        child,
        guard,
        conn: Conn::new(stdin, stdout),
    };
    if let Err(reason) = connected.conn.initialize(client_version, deadline) {
        connected.abandon();
        return Err(reason);
    }
    Ok(connected)
}

#[cfg(test)]
#[cfg(unix)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn fixture(name: &str) -> String {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures")
            .join(name)
            .display()
            .to_string()
    }

    /// Runs `connect` against the fake server with the given env settings.
    fn run(
        mode: &str,
        help: &str,
        transcript: &str,
        timeout: Duration,
    ) -> Result<Connected, FallBackReason> {
        // `sh <script>` is the "program"; env is per-process, so pass via `env`.
        let script = format!(
            "FAKE_APPSERVER_MODE={mode} FAKE_APPSERVER_HELP={help} FAKE_APPSERVER_TRANSCRIPT={} exec sh {} \"$@\"",
            fixture(transcript),
            fixture("fake-app-server.sh"),
        );
        let wrapper = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(wrapper.path(), script).unwrap();
        let bin_args = vec![wrapper.path().display().to_string()];
        let mut command = Command::new("sh");
        command
            .args(&bin_args)
            .args(["app-server", "--listen", "stdio://"]);
        connect("sh", &bin_args, command, "0.0.0", timeout)
    }

    const OK: &str = "app-server-handshake.jsonl";

    fn reason(t: Result<Connected, FallBackReason>) -> FallBackReason {
        match t {
            Err(r) => r,
            Ok(connected) => {
                connected.abandon();
                panic!("expected fallback")
            }
        }
    }

    #[test]
    fn healthy_server_handshakes() {
        let connected = run("ok", "listen", OK, Duration::from_secs(5)).expect("app-server");
        assert_eq!(connected.conn.correlator.in_flight(), 0);
        connected.abandon();
    }

    #[test]
    fn missing_listen_flag_falls_back_without_spawning() {
        let r = reason(run("ok", "nolisten", OK, Duration::from_secs(5)));
        assert_eq!(r, FallBackReason::ListenFlagMissing);
    }

    #[test]
    fn missing_binary_falls_back() {
        let missing = "/nonexistent/codex-zirv-test";
        let t = connect(
            missing,
            &[],
            Command::new(missing),
            "0.0.0",
            Duration::from_secs(1),
        );
        assert_eq!(reason(t), FallBackReason::ListenFlagMissing);
        // The probe passes, then the spawn itself fails.
        let wrapper = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(
            wrapper.path(),
            format!("exec sh {} \"$@\"", fixture("fake-app-server.sh")),
        )
        .unwrap();
        let bin_args = vec![wrapper.path().display().to_string()];
        let r = reason(connect(
            "sh",
            &bin_args,
            Command::new(missing),
            "0.0.0",
            Duration::from_secs(1),
        ));
        assert!(matches!(r, FallBackReason::Spawn(_)), "{r:?}");
    }

    #[test]
    fn garbage_reply_falls_back_with_decode_error() {
        let r = reason(run("garbage", "listen", OK, Duration::from_secs(5)));
        assert!(
            matches!(r, FallBackReason::Decode(CodecError::Malformed(_))),
            "{r:?}"
        );
    }

    #[test]
    fn server_exit_falls_back() {
        let r = reason(run("exit", "listen", OK, Duration::from_secs(5)));
        assert!(
            matches!(r, FallBackReason::ServerExited | FallBackReason::Write(_)),
            "{r:?}"
        );
    }

    #[test]
    fn silent_server_times_out() {
        let r = reason(run("hang", "listen", OK, Duration::from_millis(400)));
        assert_eq!(r, FallBackReason::Timeout);
    }

    #[test]
    fn rpc_error_on_initialize_falls_back() {
        let r = reason(run(
            "ok",
            "listen",
            "app-server-init-error.jsonl",
            Duration::from_secs(5),
        ));
        assert!(
            matches!(r, FallBackReason::Rpc(RpcError { code: -32600, .. })),
            "{r:?}"
        );
        assert_eq!(r.to_string(), "error -32600: Invalid request");
    }

    /// A server that never stops talking: one frame per `read`, counted.
    struct Flood(std::sync::Arc<std::sync::atomic::AtomicUsize>);

    impl Read for Flood {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            use std::sync::atomic::Ordering::SeqCst;
            let line = b"{\"method\":\"item/agentMessage/delta\",\"params\":{}}\n";
            // Finite only so an unbounded reader cannot exhaust memory.
            if self.0.fetch_add(1, SeqCst) >= 100_000 {
                return Ok(0);
            }
            buf[..line.len()].copy_from_slice(line);
            Ok(line.len())
        }
    }

    #[test]
    fn a_flooding_server_blocks_the_reader_at_the_queue_bound() {
        use std::sync::atomic::{AtomicUsize, Ordering::SeqCst};
        let reads = std::sync::Arc::new(AtomicUsize::new(0));
        let rx = spawn_reader(Flood(reads.clone()));
        let wait_for = |n: usize| {
            let deadline = Instant::now() + Duration::from_secs(10);
            while reads.load(SeqCst) < n && Instant::now() < deadline {
                std::thread::yield_now();
            }
            // Room for an unbounded reader to run ahead; a bounded one cannot.
            std::thread::sleep(Duration::from_millis(200));
            reads.load(SeqCst)
        };
        // A full queue plus the one frame the blocked send holds.
        let full = READER_QUEUE_FRAMES + 1;
        assert_eq!(wait_for(full), full, "the reader must block at the bound");
        assert!(matches!(rx.recv(), Ok(Inbound::Frame(Ok(_)))));
        assert_eq!(wait_for(full + 1), full + 1, "one slot frees one read");
    }

    #[test]
    fn unmatched_response_id_falls_back() {
        let r = reason(run(
            "ok",
            "listen",
            "app-server-wrong-id.jsonl",
            Duration::from_secs(5),
        ));
        assert!(
            matches!(r, FallBackReason::Decode(CodecError::UnmatchedResponse(_))),
            "{r:?}"
        );
    }
}
