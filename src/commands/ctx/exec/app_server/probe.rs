//! Probe, handshake and fall-back decision for the app-server transport.
//!
//! `connect` never fails: any probe, spawn, handshake or decode problem comes
//! back as `Transport::FallBack`, and the caller runs `codex exec` as before.

use std::io::{Read, Write};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::time::{Duration, Instant};

use super::codec::{
    ClientRequest, CodecError, Correlator, RpcError, ServerFrame, decode_line, encode_initialized,
};
use crate::commands::ctx::adapters::{ResolvedProgram, codex, resolve_program};

/// Whole handshake budget: spawn to `initialize` response.
pub const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug, Clone, PartialEq)]
pub enum FallBackReason {
    /// `codex app-server --help` does not name `--listen`.
    ListenFlagMissing,
    Spawn(String),
    Timeout,
    /// The server closed stdout or exited before answering `initialize`.
    ServerExited,
    Write(String),
    Decode(CodecError),
    Rpc(RpcError),
}

/// What the supervisor should run.
#[derive(Debug)]
pub enum Transport {
    AppServer(Box<Session>),
    /// Use the existing `codex exec` path.
    FallBack(FallBackReason),
}

enum Inbound {
    Frame(Result<ServerFrame, CodecError>),
    Closed,
}

/// A handshaken app-server child. Dropping it kills and reaps the child.
#[derive(Debug)]
pub struct Session {
    child: Child,
    stdin: ChildStdin,
    rx: Receiver<Inbound>,
    correlator: Correlator,
}

impl Drop for Session {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// `codex app-server --help` names `--listen`. Reuses the codex adapter's
/// bounded help probe; no version gate.
pub fn probe_listen_support(program: &str, bin_args: &[String]) -> bool {
    codex::detect_help_markers(program, bin_args, &["app-server"], &["--listen"])
}

fn spawn_reader(mut stdout: impl Read + Send + 'static) -> Receiver<Inbound> {
    let (tx, rx) = mpsc::channel();
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

/// Probe, spawn `codex app-server`, send `initialize` then `initialized`.
pub fn connect(
    program: &str,
    bin_args: &[String],
    client_version: &str,
    timeout: Duration,
) -> Transport {
    if !probe_listen_support(program, bin_args) {
        return Transport::FallBack(FallBackReason::ListenFlagMissing);
    }
    match handshake(program, bin_args, client_version, timeout) {
        Ok(session) => Transport::AppServer(Box::new(session)),
        Err(reason) => Transport::FallBack(reason),
    }
}

fn handshake(
    program: &str,
    bin_args: &[String],
    client_version: &str,
    timeout: Duration,
) -> Result<Session, FallBackReason> {
    let deadline = Instant::now() + timeout;
    let resolved = resolve_program(program).unwrap_or_else(|_| ResolvedProgram::direct(program));
    let mut child = Command::new(&resolved.program)
        .args(&resolved.prefix)
        .args(bin_args)
        .args(["app-server", "--listen", "stdio://"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| FallBackReason::Spawn(e.to_string()))?;
    let (Some(stdin), Some(stdout)) = (child.stdin.take(), child.stdout.take()) else {
        let _ = child.kill();
        let _ = child.wait();
        return Err(FallBackReason::Spawn("child pipes unavailable".into()));
    };
    // From here the Session's Drop reaps the child on every early return.
    let mut session = Session {
        child,
        stdin,
        rx: spawn_reader(stdout),
        correlator: Correlator::default(),
    };
    session.initialize(client_version, deadline)?;
    Ok(session)
}

impl Session {
    fn write_line(&mut self, line: &str) -> Result<(), FallBackReason> {
        self.stdin
            .write_all(line.as_bytes())
            .and_then(|()| self.stdin.flush())
            .map_err(|e| FallBackReason::Write(e.to_string()))
    }

    fn initialize(
        &mut self,
        client_version: &str,
        deadline: Instant,
    ) -> Result<(), FallBackReason> {
        let request = ClientRequest::Initialize {
            client_name: "zirv".into(),
            client_version: client_version.into(),
            experimental_api: false,
        };
        let (_, line) = self
            .correlator
            .send(&request)
            .map_err(FallBackReason::Decode)?;
        self.write_line(&line)?;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            let inbound = match self.rx.recv_timeout(remaining) {
                Ok(inbound) => inbound,
                Err(RecvTimeoutError::Timeout) => return Err(FallBackReason::Timeout),
                Err(RecvTimeoutError::Disconnected) => return Err(FallBackReason::ServerExited),
            };
            let frame = match inbound {
                Inbound::Closed => return Err(FallBackReason::ServerExited),
                Inbound::Frame(frame) => frame.map_err(FallBackReason::Decode)?,
            };
            // Notifications and server requests before the answer are ignored.
            let Some(resolved) = self
                .correlator
                .resolve(&frame)
                .map_err(FallBackReason::Decode)?
            else {
                continue;
            };
            if let Err(rpc) = resolved.outcome {
                return Err(FallBackReason::Rpc(rpc));
            }
            let initialized = encode_initialized().map_err(FallBackReason::Decode)?;
            return self.write_line(&initialized);
        }
    }
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
    fn run(mode: &str, help: &str, transcript: &str, timeout: Duration) -> Transport {
        // `sh <script>` is the "program"; env is per-process, so pass via `env`.
        let script = format!(
            "FAKE_APPSERVER_MODE={mode} FAKE_APPSERVER_HELP={help} FAKE_APPSERVER_TRANSCRIPT={} exec sh {} \"$@\"",
            fixture(transcript),
            fixture("fake-app-server.sh"),
        );
        let wrapper = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(wrapper.path(), script).unwrap();
        let bin_args = vec![wrapper.path().display().to_string()];
        connect("sh", &bin_args, "0.0.0", timeout)
    }

    const OK: &str = "app-server-handshake.jsonl";

    fn reason(t: Transport) -> FallBackReason {
        match t {
            Transport::FallBack(r) => r,
            Transport::AppServer(_) => panic!("expected fallback"),
        }
    }

    #[test]
    fn healthy_server_handshakes() {
        let t = run("ok", "listen", OK, Duration::from_secs(5));
        let Transport::AppServer(session) = t else {
            panic!("expected app-server, got {t:?}");
        };
        assert_eq!(session.correlator.in_flight(), 0);
    }

    #[test]
    fn missing_listen_flag_falls_back_without_spawning() {
        let r = reason(run("ok", "nolisten", OK, Duration::from_secs(5)));
        assert_eq!(r, FallBackReason::ListenFlagMissing);
    }

    #[test]
    fn missing_binary_falls_back() {
        let t = connect(
            "/nonexistent/codex-zirv-test",
            &[],
            "0.0.0",
            Duration::from_secs(1),
        );
        assert_eq!(reason(t), FallBackReason::ListenFlagMissing);
        let r = handshake(
            "/nonexistent/codex-zirv-test",
            &[],
            "0.0.0",
            Duration::from_secs(1),
        );
        assert!(matches!(r.err(), Some(FallBackReason::Spawn(_))));
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
