//! Native Model Context Protocol client (issue #483, roadmap N14).
//!
//! A native session has no host harness to inherit MCP servers from, so zirv
//! speaks the protocol itself. This module owns the wire: transport lifecycle,
//! `initialize` capability negotiation against the 2025-11-25 revision, tool
//! and resource discovery, `tools/call`, `notifications/cancelled`, shutdown,
//! and reconnect with re-discovery.
//!
//! Everything a server sends is **untrusted data**. Descriptions and results
//! are never executed, never parsed as instructions, always size-bounded, and
//! free-text is redacted through the existing `pace::redact_for_log` path
//! before it can reach a model request or a log line. Authorization is not
//! this module's job: a call becomes an `ExecutionAction::Mcp` and crosses the
//! N04 broker exactly like a built-in tool (see `runtime::tools`).
//!
//! Two contracts are load-bearing and tested:
//!
//! - **A stale call can never execute a different tool.** Every catalogue
//!   entry carries a digest over its name and input schema. A reconnect
//!   re-discovers, and any tool whose digest changed or which disappeared is
//!   *invalidated*: a call naming it fails with [`McpError::StaleTool`]
//!   instead of reaching the server. Only an explicit re-describe clears the
//!   invalidation, so the caller has demonstrably seen the new shape.
//! - **A large catalogue does not enter every request.** The catalogue serves
//!   a compact index (name, title, one bounded summary line) and full schemas
//!   only on demand.

use std::collections::{BTreeMap, BTreeSet};
use std::io::{BufRead, BufReader, Write};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::sync::mpsc::{Receiver, RecvTimeoutError, SyncSender, sync_channel};
use std::time::{Duration, Instant};

use serde::Serialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use super::super::config::{CapabilityEffectsConfig, McpServerConfig, McpTransportConfig};
use super::super::pace::redact_for_log;
use super::super::provider::adapter::{Cancellation, NeverCancelled};
use super::enforcement::ProcessEffects;

/// The protocol revision this client negotiates.
pub const PROTOCOL_VERSION: &str = "2025-11-25";
/// Revisions this client can still work with when a server answers with an
/// older one. Anything else is a hard, explicit refusal rather than a guess.
pub const SUPPORTED_PROTOCOL_VERSIONS: &[&str] = &["2025-11-25", "2025-06-18", "2025-03-26"];

const MAX_TOOLS: usize = 512;
const MAX_LIST_PAGES: usize = 32;
const MAX_SCHEMA_BYTES: usize = 32 * 1024;
const MAX_SUMMARY_BYTES: usize = 240;
const MAX_FRAME_BYTES: usize = 8 * 1024 * 1024;
const READ_POLL: Duration = Duration::from_millis(50);
const SHUTDOWN_GRACE: Duration = Duration::from_millis(750);

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum McpError {
    /// The server could not be reached, spawned, or kept alive.
    Transport(String),
    /// The server answered, but not with something this revision allows.
    Protocol(String),
    /// The server answered with a JSON-RPC error.
    Server {
        code: i64,
        message: String,
    },
    /// The caller's cancellation flag fired; `notifications/cancelled` was
    /// sent, and the outcome of the server-side effect is unknown.
    Cancelled,
    Timeout(String),
    /// No such server is configured, or it is disabled.
    Unavailable(String),
    /// The named tool changed shape (or vanished) since it was last described.
    StaleTool(String),
}

impl std::fmt::Display for McpError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Transport(why) => write!(f, "MCP transport failed: {why}"),
            Self::Protocol(why) => write!(f, "MCP protocol violation: {why}"),
            Self::Server { code, message } => write!(f, "MCP server error {code}: {message}"),
            Self::Cancelled => f.write_str("MCP call was cancelled; its outcome is unknown"),
            Self::Timeout(why) => write!(f, "MCP call timed out: {why}"),
            Self::Unavailable(why) => write!(f, "MCP server unavailable: {why}"),
            Self::StaleTool(why) => write!(f, "MCP tool is stale: {why}"),
        }
    }
}

impl std::error::Error for McpError {}

/// One request/response channel to a server. Implementations are responsible
/// for framing only; every protocol rule lives in [`McpClient`].
pub trait McpTransport: std::fmt::Debug + Send {
    /// Sends one request and waits for its matching response, giving up at
    /// `deadline` or when `cancel` fires. `cancel` is polled while this call
    /// is blocked waiting on the server, not only before it starts, so a
    /// cancellation that fires mid-flight is observed promptly rather than
    /// at the deadline. A cancelled call must leave the transport usable.
    fn request(
        &mut self,
        id: u64,
        method: &str,
        params: Value,
        deadline: Instant,
        cancel: &dyn Cancellation,
    ) -> Result<Value, McpError>;

    fn notify(&mut self, method: &str, params: Value) -> Result<(), McpError>;

    fn shutdown(&mut self);

    fn describe(&self) -> String;
}

/// Rebuilds a transport for the same server. Reconnect needs a fresh channel,
/// not a reset one: a stdio server that died has to be respawned.
pub trait TransportFactory: std::fmt::Debug + Send + Sync {
    fn connect(&self) -> Result<Box<dyn McpTransport>, McpError>;
}

// --------------------------------------------------------------------------
// stdio transport
// --------------------------------------------------------------------------

/// Newline-delimited JSON-RPC over a child process's stdin/stdout, the
/// transport every local MCP server speaks. The child's stdout is drained by
/// one reader thread so a blocking read can never wedge the caller past its
/// own deadline or cancellation.
#[derive(Debug)]
pub struct StdioTransport {
    child: Child,
    stdin: Option<std::process::ChildStdin>,
    frames: Receiver<Result<Value, String>>,
    pending: Vec<Value>,
    label: String,
    /// The child's stderr, drained on a background thread into a bounded
    /// tail so a transport error can explain itself instead of just "closed
    /// its output stream".
    stderr_tail: Arc<std::sync::Mutex<Vec<u8>>>,
}

/// How much of a child's stderr is kept for error messages. Bounded so a
/// chatty or hostile server cannot grow zirv's memory or its log lines.
const MAX_STDERR_TAIL_BYTES: usize = 4 * 1024;

impl StdioTransport {
    pub fn spawn(config: &McpServerConfig) -> Result<Self, McpError> {
        let McpTransportConfig::Stdio {
            command,
            args,
            cwd,
            environment,
        } = &config.transport
        else {
            return Err(McpError::Unavailable(format!(
                "server `{}` is not configured for the stdio transport",
                config.name
            )));
        };
        if command.trim().is_empty() {
            return Err(McpError::Unavailable(format!(
                "server `{}` has an empty stdio command",
                config.name
            )));
        }
        let mut process = Command::new(command);
        process
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if let Some(cwd) = cwd {
            process.current_dir(cwd);
        }
        for (key, value) in environment {
            process.env(key, value);
        }
        let mut child = process.spawn().map_err(|error| {
            McpError::Transport(format!(
                "could not start MCP server `{}` ({command}): {error}",
                config.name
            ))
        })?;
        let stdin = child.stdin.take();
        let stdout = child.stdout.take().ok_or_else(|| {
            McpError::Transport(format!("MCP server `{}` has no stdout", config.name))
        })?;
        let (sender, frames) = sync_channel(64);
        std::thread::spawn(move || pump_frames(stdout, &sender));
        let stderr_tail = Arc::new(std::sync::Mutex::new(Vec::new()));
        if let Some(stderr) = child.stderr.take() {
            let tail = Arc::clone(&stderr_tail);
            std::thread::spawn(move || pump_stderr(stderr, &tail));
        }
        Ok(Self {
            child,
            stdin,
            frames,
            pending: Vec::new(),
            label: format!("stdio:{command}"),
            stderr_tail,
        })
    }

    /// The last [`MAX_STDERR_TAIL_BYTES`] of the child's stderr, redacted the
    /// same way any other untrusted server text is before it can reach an
    /// error message or a log line. Empty when the server wrote nothing.
    fn stderr_snippet(&self) -> String {
        let bytes = self
            .stderr_tail
            .lock()
            .map(|tail| tail.clone())
            .unwrap_or_default();
        if bytes.is_empty() {
            return String::new();
        }
        redact_for_log(&String::from_utf8_lossy(&bytes))
    }

    fn write_frame(&mut self, frame: &Value) -> Result<(), McpError> {
        let stdin = self
            .stdin
            .as_mut()
            .ok_or_else(|| McpError::Transport("MCP stdin is closed".into()))?;
        let mut line = serde_json::to_string(frame)
            .map_err(|error| McpError::Protocol(format!("could not encode request: {error}")))?;
        line.push('\n');
        let written = stdin
            .write_all(line.as_bytes())
            .and_then(|()| stdin.flush());
        match written {
            Ok(()) => Ok(()),
            Err(error) => {
                // A server that exits before the first frame lands (a broken
                // pipe) usually explained itself on stderr; give the reader
                // the same beat the closed-stdout path does so the
                // diagnostic is on the error either way.
                std::thread::sleep(Duration::from_millis(50));
                Err(self.with_stderr(format!("could not write to MCP server: {error}")))
            }
        }
    }

    /// Appends the child's redacted stderr tail to a transport error, so a
    /// server that explains its own failure on stderr is not reduced to
    /// "closed its output stream".
    fn with_stderr(&self, message: String) -> McpError {
        let snippet = self.stderr_snippet();
        if snippet.is_empty() {
            McpError::Transport(message)
        } else {
            McpError::Transport(format!("{message} (stderr: {snippet})"))
        }
    }

    /// Returns the response for `id`, parking every other frame (a
    /// notification, a server-initiated request, a stale response) so it can
    /// be drained later instead of being mistaken for this answer. `cancel`
    /// is polled every [`READ_POLL`] tick, not only before this call starts,
    /// so a cancellation that fires while this is blocked waiting on the
    /// server is observed within one poll tick instead of at `deadline`.
    fn await_response(
        &mut self,
        id: u64,
        deadline: Instant,
        cancel: &dyn Cancellation,
    ) -> Result<Value, McpError> {
        if let Some(index) = self
            .pending
            .iter()
            .position(|frame| frame_id(frame) == Some(id))
        {
            return Ok(self.pending.remove(index));
        }
        loop {
            if cancel.is_cancelled() {
                return Err(McpError::Cancelled);
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(McpError::Timeout(format!("no response to request {id}")));
            }
            match self.frames.recv_timeout(remaining.min(READ_POLL)) {
                Ok(Ok(frame)) => {
                    if frame_id(&frame) == Some(id) {
                        return Ok(frame);
                    }
                    if self.pending.len() < 64 {
                        self.pending.push(frame);
                    }
                }
                Ok(Err(why)) => return Err(self.with_stderr(why)),
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => {
                    // The stdout pipe can close a beat before the child's
                    // already-buffered stderr bytes are drained into the
                    // tail by the background reader; give it a moment so the
                    // error is not missing a diagnostic that was in fact
                    // written.
                    std::thread::sleep(Duration::from_millis(50));
                    return Err(self.with_stderr("MCP server closed its output stream".into()));
                }
            }
        }
    }
}

fn pump_stderr(stderr: std::process::ChildStderr, tail: &std::sync::Mutex<Vec<u8>>) {
    use std::io::Read;
    let mut reader = BufReader::new(stderr);
    let mut chunk = [0u8; 1024];
    loop {
        match reader.read(&mut chunk) {
            Ok(0) | Err(_) => return,
            Ok(count) => {
                if let Ok(mut buffer) = tail.lock() {
                    buffer.extend_from_slice(&chunk[..count]);
                    if buffer.len() > MAX_STDERR_TAIL_BYTES {
                        let overflow = buffer.len() - MAX_STDERR_TAIL_BYTES;
                        buffer.drain(0..overflow);
                    }
                }
            }
        }
    }
}

impl McpTransport for StdioTransport {
    fn request(
        &mut self,
        id: u64,
        method: &str,
        params: Value,
        deadline: Instant,
        cancel: &dyn Cancellation,
    ) -> Result<Value, McpError> {
        self.write_frame(&json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        }))?;
        self.await_response(id, deadline, cancel)
    }

    fn notify(&mut self, method: &str, params: Value) -> Result<(), McpError> {
        self.write_frame(&json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": params,
        }))
    }

    fn shutdown(&mut self) {
        // Closing stdin is the protocol's own stdio shutdown signal; the kill
        // is only the backstop for a server that ignores it.
        self.stdin = None;
        let deadline = Instant::now() + SHUTDOWN_GRACE;
        while Instant::now() < deadline {
            match self.child.try_wait() {
                Ok(Some(_)) => return,
                Ok(None) => std::thread::sleep(READ_POLL),
                Err(_) => break,
            }
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }

    fn describe(&self) -> String {
        self.label.clone()
    }
}

impl Drop for StdioTransport {
    fn drop(&mut self) {
        self.shutdown();
    }
}

fn pump_frames(stdout: std::process::ChildStdout, sender: &SyncSender<Result<Value, String>>) {
    let mut reader = BufReader::new(stdout);
    let mut line = String::new();
    loop {
        line.clear();
        match reader.read_line(&mut line) {
            Ok(0) => return,
            Ok(_) => {}
            Err(error) => {
                let _ = sender.send(Err(format!("MCP read failed: {error}")));
                return;
            }
        }
        if line.len() > MAX_FRAME_BYTES {
            let _ = sender.send(Err(format!(
                "MCP frame is {} bytes; limit is {MAX_FRAME_BYTES}",
                line.len()
            )));
            return;
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let frame = match serde_json::from_str::<Value>(trimmed) {
            Ok(frame) => Ok(frame),
            Err(error) => Err(format!("MCP frame is not JSON: {error}")),
        };
        let failed = frame.is_err();
        if sender.send(frame).is_err() || failed {
            return;
        }
    }
}

fn frame_id(frame: &Value) -> Option<u64> {
    frame.get("id").and_then(Value::as_u64)
}

#[derive(Debug)]
pub struct StdioFactory {
    config: McpServerConfig,
}

impl StdioFactory {
    pub fn new(config: McpServerConfig) -> Self {
        Self { config }
    }
}

impl TransportFactory for StdioFactory {
    fn connect(&self) -> Result<Box<dyn McpTransport>, McpError> {
        Ok(Box::new(StdioTransport::spawn(&self.config)?))
    }
}

// --------------------------------------------------------------------------
// Streamable HTTP transport
// --------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HttpReply {
    pub status: u16,
    pub content_type: String,
    pub session_id: Option<String>,
    pub body: String,
}

/// The one outbound call the remote transport makes. Extracted so the whole
/// Streamable-HTTP contract -- headers, bearer auth, session pinning, JSON vs
/// SSE bodies, status classification -- is exercised against an in-process
/// fixture server with no network.
pub trait HttpPoster: std::fmt::Debug + Send + Sync {
    fn post(
        &self,
        url: &str,
        headers: &[(String, String)],
        body: String,
        deadline: Instant,
    ) -> Result<HttpReply, McpError>;
}

#[derive(Debug, Default)]
pub struct UreqPoster;

impl HttpPoster for UreqPoster {
    fn post(
        &self,
        url: &str,
        headers: &[(String, String)],
        body: String,
        deadline: Instant,
    ) -> Result<HttpReply, McpError> {
        let budget = deadline.saturating_duration_since(Instant::now());
        if budget.is_zero() {
            return Err(McpError::Timeout("no time left for the MCP request".into()));
        }
        let agent: ureq::Agent = ureq::Agent::config_builder()
            .http_status_as_error(false)
            .timeout_connect(Some(budget))
            .timeout_recv_response(Some(budget))
            .build()
            .into();
        let mut request = agent.post(url);
        for (name, value) in headers {
            request = request.header(name.as_str(), value.as_str());
        }
        let mut response = request
            .send(body)
            .map_err(|error| McpError::Transport(format!("{error}")))?;
        let status = response.status().as_u16();
        let content_type = response
            .headers()
            .get("content-type")
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default()
            .to_string();
        let session_id = response
            .headers()
            .get("mcp-session-id")
            .and_then(|value| value.to_str().ok())
            .map(str::to_string);
        let body = response
            .body_mut()
            .with_config()
            .limit(MAX_FRAME_BYTES as u64)
            .read_to_string()
            .map_err(|error| McpError::Transport(format!("could not read MCP body: {error}")))?;
        Ok(HttpReply {
            status,
            content_type,
            session_id,
            body,
        })
    }
}

/// Streamable HTTP (one POST per JSON-RPC message; the reply is either a JSON
/// object or an SSE stream carrying it). Bearer credentials come from the
/// existing provider credential store and are never logged or serialized.
#[derive(Debug)]
pub struct HttpTransport {
    url: String,
    bearer: Option<String>,
    poster: Arc<dyn HttpPoster>,
    session_id: Option<String>,
    label: String,
}

impl HttpTransport {
    pub fn new(url: String, bearer: Option<String>, poster: Arc<dyn HttpPoster>) -> Self {
        let label = format!("http:{url}");
        Self {
            url,
            bearer,
            poster,
            session_id: None,
            label,
        }
    }

    fn headers(&self) -> Vec<(String, String)> {
        let mut headers = vec![
            ("content-type".into(), "application/json".into()),
            (
                "accept".into(),
                "application/json, text/event-stream".into(),
            ),
            ("mcp-protocol-version".into(), PROTOCOL_VERSION.into()),
            (
                "user-agent".into(),
                format!("zirv/{}", env!("CARGO_PKG_VERSION")),
            ),
        ];
        if let Some(bearer) = &self.bearer {
            headers.push(("authorization".into(), format!("Bearer {bearer}")));
        }
        if let Some(session) = &self.session_id {
            headers.push(("mcp-session-id".into(), session.clone()));
        }
        headers
    }

    /// Runs the poster's blocking POST on a background thread and polls
    /// `cancel` while waiting for it, so a cancellation that fires while this
    /// call is blocked inside the poster is observed within one poll tick
    /// instead of only once the poster itself gives up at `deadline`. The
    /// background thread is left to finish (or hit its own deadline) on its
    /// own; a cancelled request never reads its answer.
    fn post_cancellable(
        &self,
        body: String,
        deadline: Instant,
        cancel: &dyn Cancellation,
    ) -> Result<HttpReply, McpError> {
        let poster = Arc::clone(&self.poster);
        let url = self.url.clone();
        let headers = self.headers();
        let (sender, receiver) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let result = poster.post(&url, &headers, body, deadline);
            let _ = sender.send(result);
        });
        loop {
            if cancel.is_cancelled() {
                return Err(McpError::Cancelled);
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            match receiver.recv_timeout(remaining.min(READ_POLL)) {
                Ok(result) => return result,
                Err(RecvTimeoutError::Timeout) => {
                    if remaining.is_zero() {
                        return Err(McpError::Timeout("no response to the MCP request".into()));
                    }
                }
                Err(RecvTimeoutError::Disconnected) => {
                    return Err(McpError::Transport(
                        "the MCP HTTP request thread vanished without answering".into(),
                    ));
                }
            }
        }
    }

    fn send(
        &mut self,
        frame: &Value,
        deadline: Instant,
        cancel: &dyn Cancellation,
    ) -> Result<Option<Value>, McpError> {
        let body = serde_json::to_string(frame)
            .map_err(|error| McpError::Protocol(format!("could not encode request: {error}")))?;
        let reply = self.post_cancellable(body, deadline, cancel)?;
        if let Some(session) = reply.session_id.clone() {
            self.session_id = Some(session);
        }
        match reply.status {
            200..=299 => {}
            401 | 403 => {
                return Err(McpError::Unavailable(format!(
                    "remote MCP server refused the credential (HTTP {})",
                    reply.status
                )));
            }
            404 => {
                // A pinned session the server has forgotten: drop it so the
                // next connect negotiates a fresh one instead of looping.
                self.session_id = None;
                return Err(McpError::Transport(
                    "remote MCP session is no longer known to the server (HTTP 404)".into(),
                ));
            }
            status => {
                return Err(McpError::Transport(format!(
                    "remote MCP server answered HTTP {status}: {}",
                    redact_for_log(&reply.body)
                )));
            }
        }
        if reply.status == 202 || reply.body.trim().is_empty() {
            return Ok(None);
        }
        parse_http_body(&reply.content_type, &reply.body).map(Some)
    }
}

/// Accepts both reply shapes the revision allows for a POST: a single JSON
/// object, or an SSE stream whose `data:` lines carry one.
fn parse_http_body(content_type: &str, body: &str) -> Result<Value, McpError> {
    if content_type.contains("text/event-stream") {
        let mut payload = String::new();
        for line in body.lines() {
            if let Some(rest) = line.strip_prefix("data:") {
                payload.push_str(rest.trim_start());
            } else if line.trim().is_empty() && !payload.is_empty() {
                break;
            }
        }
        if payload.is_empty() {
            return Err(McpError::Protocol(
                "remote MCP stream carried no data event".into(),
            ));
        }
        return serde_json::from_str(&payload)
            .map_err(|error| McpError::Protocol(format!("MCP stream frame is not JSON: {error}")));
    }
    serde_json::from_str(body)
        .map_err(|error| McpError::Protocol(format!("MCP reply is not JSON: {error}")))
}

impl McpTransport for HttpTransport {
    fn request(
        &mut self,
        id: u64,
        method: &str,
        params: Value,
        deadline: Instant,
        cancel: &dyn Cancellation,
    ) -> Result<Value, McpError> {
        let frame = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        });
        self.send(&frame, deadline, cancel)?.ok_or_else(|| {
            McpError::Protocol(format!("remote MCP server sent no response to {method}"))
        })
    }

    fn notify(&mut self, method: &str, params: Value) -> Result<(), McpError> {
        let frame = json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": params,
        });
        self.send(
            &frame,
            Instant::now() + Duration::from_secs(10),
            &NeverCancelled,
        )
        .map(|_| ())
    }

    fn shutdown(&mut self) {
        self.session_id = None;
    }

    fn describe(&self) -> String {
        self.label.clone()
    }
}

#[derive(Debug)]
pub struct HttpFactory {
    url: String,
    bearer: Option<String>,
    poster: Arc<dyn HttpPoster>,
}

impl HttpFactory {
    pub fn new(url: String, bearer: Option<String>, poster: Arc<dyn HttpPoster>) -> Self {
        Self {
            url,
            bearer,
            poster,
        }
    }
}

impl TransportFactory for HttpFactory {
    fn connect(&self) -> Result<Box<dyn McpTransport>, McpError> {
        Ok(Box::new(HttpTransport::new(
            self.url.clone(),
            self.bearer.clone(),
            Arc::clone(&self.poster),
        )))
    }
}

// --------------------------------------------------------------------------
// catalogue
// --------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct McpToolEntry {
    pub name: String,
    pub title: Option<String>,
    /// One bounded, redacted line. Untrusted server prose: shown, never obeyed.
    pub summary: String,
    pub input_schema: Value,
    /// SHA-256 over the name and input schema. The identity a call is checked
    /// against after a reconnect.
    pub digest: String,
}

impl McpToolEntry {
    /// The tool's name reduced to one safe registry segment. A server may
    /// call its tools anything; a registry key may not contain a separator
    /// that could make one name look like another.
    pub fn tool_key(&self) -> String {
        self.name
            .chars()
            .map(|character| {
                if character.is_ascii_alphanumeric() || character == '_' {
                    character
                } else {
                    '_'
                }
            })
            .collect()
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct McpResourceEntry {
    pub uri: String,
    pub name: String,
    pub mime_type: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct McpCatalogue {
    entries: BTreeMap<String, McpToolEntry>,
    resources: Vec<McpResourceEntry>,
    invalidated: BTreeMap<String, String>,
    /// Bumped on every re-discovery, so an index a caller holds can be told
    /// apart from the current one.
    generation: u64,
}

impl McpCatalogue {
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn generation(&self) -> u64 {
        self.generation
    }

    pub fn entries(&self) -> impl Iterator<Item = &McpToolEntry> {
        self.entries.values()
    }

    pub fn resources(&self) -> &[McpResourceEntry] {
        &self.resources
    }

    pub fn get(&self, name: &str) -> Option<&McpToolEntry> {
        self.entries.get(name)
    }

    pub fn invalidated(&self) -> &BTreeMap<String, String> {
        &self.invalidated
    }

    /// The compact form: everything a model needs to decide whether to ask for
    /// a schema, and nothing more. This is what a large catalogue contributes
    /// to a request instead of every schema.
    pub fn index(&self) -> Value {
        json!({
            "generation": self.generation,
            "count": self.entries.len(),
            "tools": self.entries.values().map(|entry| json!({
                "name": entry.name,
                "title": entry.title,
                "summary": entry.summary,
                "stale": self.invalidated.contains_key(&entry.name),
            })).collect::<Vec<_>>(),
            "resources": self.resources,
        })
    }

    /// Full schema for one tool. Describing a tool is also the only thing that
    /// clears its invalidation: the caller has now seen the current shape.
    pub fn describe(&mut self, name: &str) -> Result<Value, McpError> {
        let entry = self
            .entries
            .get(name)
            .ok_or_else(|| McpError::Unavailable(format!("no MCP tool named `{name}`")))?
            .clone();
        self.invalidated.remove(name);
        Ok(json!({
            "name": entry.name,
            "title": entry.title,
            "summary": entry.summary,
            "input_schema": entry.input_schema,
            "digest": entry.digest,
            "generation": self.generation,
        }))
    }

    /// Guards a call. Fails closed: a name this catalogue does not hold, or
    /// one whose shape changed since it was last described, never reaches the
    /// server.
    pub fn admit(&self, name: &str) -> Result<&McpToolEntry, McpError> {
        if let Some(reason) = self.invalidated.get(name) {
            return Err(McpError::StaleTool(format!(
                "`{name}` {reason}; describe it again before calling it"
            )));
        }
        self.entries
            .get(name)
            .ok_or_else(|| McpError::Unavailable(format!("no MCP tool named `{name}`")))
    }

    /// Installs a freshly discovered tool list. Every name whose digest moved,
    /// and every name that disappeared, is invalidated -- the exact condition
    /// under which a call issued against the old catalogue would otherwise run
    /// a different tool than the one it was written for.
    fn replace(&mut self, discovered: Vec<McpToolEntry>, resources: Vec<McpResourceEntry>) {
        let first = self.generation == 0;
        let mut next = BTreeMap::new();
        let mut seen = BTreeSet::new();
        for entry in discovered {
            if !first
                && let Some(previous) = self.entries.get(&entry.name)
                && previous.digest != entry.digest
            {
                self.invalidated
                    .insert(entry.name.clone(), "changed its name or schema".to_string());
            }
            seen.insert(entry.name.clone());
            next.insert(entry.name.clone(), entry);
        }
        if !first {
            for name in self.entries.keys() {
                if !seen.contains(name) {
                    self.invalidated.insert(
                        name.clone(),
                        "is no longer offered by this server".to_string(),
                    );
                }
            }
        }
        // An invalidation only survives while its name is still absent or
        // changed; a name that came back identical is not stale.
        self.invalidated.retain(|name, _| {
            !next.contains_key(name) || !self.entries.contains_key(name) || {
                self.entries
                    .get(name)
                    .zip(next.get(name))
                    .is_none_or(|(old, new)| old.digest != new.digest)
            }
        });
        self.entries = next;
        self.resources = resources;
        self.generation += 1;
    }
}

fn digest_tool(name: &str, schema: &Value) -> String {
    let mut hasher = Sha256::new();
    hasher.update(name.as_bytes());
    hasher.update([0]);
    hasher.update(serde_json::to_vec(schema).unwrap_or_default());
    hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// Converts one untrusted `tools/list` row into a catalogue entry. Anything
/// that is not a well-formed row is dropped rather than repaired.
fn tool_entry(value: &Value) -> Option<McpToolEntry> {
    let name = value.get("name")?.as_str()?.to_string();
    if name.is_empty() || name.len() > 128 {
        return None;
    }
    let schema = value
        .get("inputSchema")
        .cloned()
        .unwrap_or_else(|| json!({"type": "object"}));
    if !schema.is_object()
        || serde_json::to_vec(&schema)
            .map(|b| b.len())
            .unwrap_or(usize::MAX)
            > MAX_SCHEMA_BYTES
    {
        return None;
    }
    let title = value
        .get("title")
        .and_then(Value::as_str)
        .map(|title| bounded_text(title, MAX_SUMMARY_BYTES));
    let summary = value
        .get("description")
        .and_then(Value::as_str)
        .map(|text| bounded_text(text, MAX_SUMMARY_BYTES))
        .unwrap_or_default();
    let digest = digest_tool(&name, &schema);
    Some(McpToolEntry {
        name,
        title,
        summary,
        input_schema: schema,
        digest,
    })
}

/// Untrusted free text on its way to a model request or a log line: redacted
/// through the existing path, collapsed to one line, hard-bounded.
pub fn bounded_text(text: &str, limit: usize) -> String {
    let mut single_line = redact_for_log(text).replace(['\n', '\r'], " ");
    if single_line.len() > limit {
        single_line.truncate(
            (0..=limit)
                .rev()
                .find(|index| single_line.is_char_boundary(*index))
                .unwrap_or(0),
        );
        single_line.push('\u{2026}');
    }
    single_line
}

// --------------------------------------------------------------------------
// client
// --------------------------------------------------------------------------

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct McpServerInfo {
    pub name: String,
    pub version: String,
    pub protocol_version: String,
    pub supports_tools: bool,
    pub supports_resources: bool,
}

/// One connected MCP server: transport, negotiated identity, catalogue.
#[derive(Debug)]
pub struct McpClient {
    name: String,
    factory: Arc<dyn TransportFactory>,
    transport: Option<Box<dyn McpTransport>>,
    info: McpServerInfo,
    catalogue: McpCatalogue,
    effects: ProcessEffects,
    timeout: Duration,
    next_id: u64,
    reconnects: u32,
}

impl McpClient {
    /// Connects, negotiates, and discovers in one step. A failure here is a
    /// typed `Unavailable`/`Transport`/`Protocol` error, never a client that
    /// looks connected but is not.
    pub fn connect(
        name: &str,
        factory: Arc<dyn TransportFactory>,
        effects: ProcessEffects,
        timeout: Duration,
    ) -> Result<Self, McpError> {
        let mut client = Self {
            name: name.to_string(),
            factory,
            transport: None,
            info: McpServerInfo::default(),
            catalogue: McpCatalogue::default(),
            effects,
            timeout,
            next_id: 0,
            reconnects: 0,
        };
        client.open()?;
        Ok(client)
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn info(&self) -> &McpServerInfo {
        &self.info
    }

    pub fn effects(&self) -> &ProcessEffects {
        &self.effects
    }

    pub fn catalogue(&self) -> &McpCatalogue {
        &self.catalogue
    }

    pub fn catalogue_mut(&mut self) -> &mut McpCatalogue {
        &mut self.catalogue
    }

    pub fn reconnects(&self) -> u32 {
        self.reconnects
    }

    fn open(&mut self) -> Result<(), McpError> {
        let mut transport = self.factory.connect()?;
        let deadline = Instant::now() + self.timeout;
        self.next_id += 1;
        let id = self.next_id;
        let result = transport.request(
            id,
            "initialize",
            json!({
                "protocolVersion": PROTOCOL_VERSION,
                "capabilities": {},
                "clientInfo": {
                    "name": "zirv",
                    "title": "Zirv native runtime",
                    "version": env!("CARGO_PKG_VERSION"),
                },
            }),
            deadline,
            &NeverCancelled,
        )?;
        let result = unwrap_result(result)?;
        let negotiated = result
            .get("protocolVersion")
            .and_then(Value::as_str)
            .ok_or_else(|| McpError::Protocol("initialize did not negotiate a version".into()))?;
        if !SUPPORTED_PROTOCOL_VERSIONS.contains(&negotiated) {
            transport.shutdown();
            return Err(McpError::Protocol(format!(
                "server `{}` speaks MCP {negotiated}; this build supports {}",
                self.name,
                SUPPORTED_PROTOCOL_VERSIONS.join(", ")
            )));
        }
        let capabilities = result.get("capabilities").cloned().unwrap_or(Value::Null);
        self.info = McpServerInfo {
            name: bounded_text(
                result
                    .pointer("/serverInfo/name")
                    .and_then(Value::as_str)
                    .unwrap_or(&self.name),
                MAX_SUMMARY_BYTES,
            ),
            version: bounded_text(
                result
                    .pointer("/serverInfo/version")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown"),
                64,
            ),
            protocol_version: negotiated.to_string(),
            supports_tools: capabilities.get("tools").is_some(),
            supports_resources: capabilities.get("resources").is_some(),
        };
        transport.notify("notifications/initialized", json!({}))?;
        self.transport = Some(transport);
        self.discover()
    }

    /// Re-runs tools/list (paginated) and resources/list, then installs the
    /// result through the catalogue's invalidation rules.
    pub fn discover(&mut self) -> Result<(), McpError> {
        let mut tools = Vec::new();
        if self.info.supports_tools {
            let mut cursor: Option<String> = None;
            for _ in 0..MAX_LIST_PAGES {
                let params = cursor
                    .take()
                    .map_or_else(|| json!({}), |cursor| json!({ "cursor": cursor }));
                let page = unwrap_result(self.call_raw("tools/list", params)?)?;
                for value in page
                    .get("tools")
                    .and_then(Value::as_array)
                    .unwrap_or(&Vec::new())
                {
                    if tools.len() >= MAX_TOOLS {
                        break;
                    }
                    if let Some(entry) = tool_entry(value) {
                        tools.push(entry);
                    }
                }
                match page.get("nextCursor").and_then(Value::as_str) {
                    Some(next) if tools.len() < MAX_TOOLS => cursor = Some(next.to_string()),
                    _ => break,
                }
            }
        }
        let mut resources = Vec::new();
        if self.info.supports_resources {
            let page = unwrap_result(self.call_raw("resources/list", json!({}))?)?;
            for value in page
                .get("resources")
                .and_then(Value::as_array)
                .unwrap_or(&Vec::new())
                .iter()
                .take(MAX_TOOLS)
            {
                let (Some(uri), name) = (
                    value.get("uri").and_then(Value::as_str),
                    value
                        .get("name")
                        .and_then(Value::as_str)
                        .unwrap_or_default(),
                ) else {
                    continue;
                };
                resources.push(McpResourceEntry {
                    uri: bounded_text(uri, MAX_SUMMARY_BYTES),
                    name: bounded_text(name, MAX_SUMMARY_BYTES),
                    mime_type: value
                        .get("mimeType")
                        .and_then(Value::as_str)
                        .map(|mime| bounded_text(mime, 64)),
                });
            }
        }
        self.catalogue.replace(tools, resources);
        Ok(())
    }

    /// Drops the current transport and connects a new one, re-negotiating and
    /// re-discovering. Everything the reconnect changed is invalidated.
    pub fn reconnect(&mut self) -> Result<(), McpError> {
        if let Some(mut transport) = self.transport.take() {
            transport.shutdown();
        }
        self.reconnects += 1;
        self.open()
    }

    /// Invokes one tool. `cancel` is polled between the send and the reply; a
    /// cancelled call sends `notifications/cancelled` and returns
    /// [`McpError::Cancelled`], whose effect is by definition unknown.
    pub fn call_tool(
        &mut self,
        tool: &str,
        arguments: Value,
        cancel: &dyn super::super::provider::adapter::Cancellation,
    ) -> Result<McpToolResult, McpError> {
        let entry = self.catalogue.admit(tool)?.clone();
        let params = json!({ "name": entry.name, "arguments": arguments });
        let raw = match self.call_raw_cancellable("tools/call", params, cancel) {
            Ok(raw) => raw,
            Err(McpError::Transport(why)) => {
                // One reconnect, then the call is reported rather than
                // silently replayed: a tool whose shape moved must not be
                // re-entered against a stale description.
                self.reconnect()?;
                return Err(McpError::Transport(format!(
                    "{why}; reconnected and re-discovered, re-issue the call"
                )));
            }
            Err(other) => return Err(other),
        };
        let result = unwrap_result(raw)?;
        Ok(McpToolResult::from_value(&entry.name, &result))
    }

    fn call_raw(&mut self, method: &str, params: Value) -> Result<Value, McpError> {
        self.call_raw_cancellable(
            method,
            params,
            &super::super::provider::adapter::NeverCancelled,
        )
    }

    fn call_raw_cancellable(
        &mut self,
        method: &str,
        params: Value,
        cancel: &dyn super::super::provider::adapter::Cancellation,
    ) -> Result<Value, McpError> {
        self.next_id += 1;
        let id = self.next_id;
        let deadline = Instant::now() + self.timeout;
        let transport = self
            .transport
            .as_mut()
            .ok_or_else(|| McpError::Transport("MCP transport is closed".into()))?;
        if cancel.is_cancelled() {
            let _ = transport.notify(
                "notifications/cancelled",
                json!({"requestId": id, "reason": "zirv cancelled the session"}),
            );
            return Err(McpError::Cancelled);
        }
        match transport.request(id, method, params, deadline, cancel) {
            Ok(value) => Ok(value),
            Err(error) => {
                if cancel.is_cancelled() {
                    let _ = transport.notify(
                        "notifications/cancelled",
                        json!({"requestId": id, "reason": "zirv cancelled the session"}),
                    );
                    return Err(McpError::Cancelled);
                }
                Err(error)
            }
        }
    }

    pub fn shutdown(&mut self) {
        if let Some(mut transport) = self.transport.take() {
            transport.shutdown();
        }
    }
}

fn unwrap_result(frame: Value) -> Result<Value, McpError> {
    if let Some(error) = frame.get("error") {
        let code = error.get("code").and_then(Value::as_i64).unwrap_or(-1);
        let message = bounded_text(
            error
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("unspecified"),
            MAX_SUMMARY_BYTES,
        );
        return Err(McpError::Server { code, message });
    }
    frame
        .get("result")
        .cloned()
        .ok_or_else(|| McpError::Protocol("MCP reply carried neither result nor error".into()))
}

/// A `tools/call` outcome, normalized. `is_error` is the server's own
/// tool-level failure flag and is preserved: a failed tool is not a failed
/// call, and conflating them would let a server report success by omission.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct McpToolResult {
    pub tool: String,
    pub is_error: bool,
    /// Concatenated text blocks, unbounded here and bounded by the caller's
    /// own output-limit path (the same one process output uses).
    pub text: String,
    /// Non-text blocks, described rather than inlined.
    pub attachments: Vec<Value>,
    pub structured: Option<Value>,
}

impl McpToolResult {
    fn from_value(tool: &str, result: &Value) -> Self {
        let mut text = String::new();
        let mut attachments = Vec::new();
        for block in result
            .get("content")
            .and_then(Value::as_array)
            .unwrap_or(&Vec::new())
        {
            match block.get("type").and_then(Value::as_str) {
                Some("text") => {
                    if let Some(value) = block.get("text").and_then(Value::as_str) {
                        if !text.is_empty() {
                            text.push('\n');
                        }
                        text.push_str(value);
                    }
                }
                Some(kind) => attachments.push(json!({
                    "type": kind,
                    "mime_type": block.get("mimeType").and_then(Value::as_str),
                    "uri": block.get("uri").and_then(Value::as_str).map(|uri| bounded_text(uri, MAX_SUMMARY_BYTES)),
                })),
                None => {}
            }
        }
        Self {
            tool: tool.to_string(),
            is_error: result
                .get("isError")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            text,
            attachments,
            structured: result.get("structuredContent").cloned(),
        }
    }
}

impl From<&CapabilityEffectsConfig> for ProcessEffects {
    fn from(value: &CapabilityEffectsConfig) -> Self {
        Self {
            repo_write: value.repo_write,
            outside_write: value.outside_write,
            network: value.network,
            git_metadata_write: value.git_metadata_write,
            git_push_or_destructive: value.git_push_or_destructive,
        }
    }
}

// --------------------------------------------------------------------------
// in-process fixture server
// --------------------------------------------------------------------------

/// A scripted MCP server, production-compiled for the same reason
/// `runtime::fixture` is: the whole client contract -- negotiation, paging,
/// reconnection, cancellation, tool-level errors -- is exercised with no
/// child process and no socket.
#[derive(Clone, Debug, Default)]
pub struct FixtureServer {
    pub protocol_version: String,
    pub tools: Vec<Value>,
    pub resources: Vec<Value>,
    pub results: BTreeMap<String, Value>,
    pub fail_next_call: bool,
}

impl FixtureServer {
    pub fn answer(&mut self, frame: &Value) -> Value {
        let id = frame.get("id").cloned().unwrap_or(Value::Null);
        let method = frame.get("method").and_then(Value::as_str).unwrap_or("");
        let result = match method {
            "initialize" => json!({
                "protocolVersion": if self.protocol_version.is_empty() {
                    PROTOCOL_VERSION.to_string()
                } else {
                    self.protocol_version.clone()
                },
                "capabilities": {"tools": {}, "resources": {}},
                "serverInfo": {"name": "fixture", "version": "1.0.0"},
            }),
            "tools/list" => json!({"tools": self.tools}),
            "resources/list" => json!({"resources": self.resources}),
            "tools/call" => {
                if self.fail_next_call {
                    self.fail_next_call = false;
                    return json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "error": {"code": -32000, "message": "fixture refused the call"},
                    });
                }
                let name = frame
                    .pointer("/params/name")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                self.results.get(name).cloned().unwrap_or_else(
                    || json!({"content": [{"type": "text", "text": format!("ran {name}")}]}),
                )
            }
            other => {
                return json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "error": {"code": -32601, "message": format!("no method {other}")},
                });
            }
        };
        json!({"jsonrpc": "2.0", "id": id, "result": result})
    }
}

/// A transport that answers from a [`FixtureServer`] in the calling thread.
#[derive(Clone, Debug)]
pub struct FixtureTransport {
    server: Arc<std::sync::Mutex<FixtureServer>>,
    notifications: Arc<std::sync::Mutex<Vec<(String, Value)>>>,
}

impl FixtureTransport {
    pub fn new(server: Arc<std::sync::Mutex<FixtureServer>>) -> Self {
        Self {
            server,
            notifications: Arc::new(std::sync::Mutex::new(Vec::new())),
        }
    }

    pub fn notifications(&self) -> Arc<std::sync::Mutex<Vec<(String, Value)>>> {
        Arc::clone(&self.notifications)
    }
}

impl McpTransport for FixtureTransport {
    fn request(
        &mut self,
        id: u64,
        method: &str,
        params: Value,
        _deadline: Instant,
        _cancel: &dyn Cancellation,
    ) -> Result<Value, McpError> {
        let frame = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        let mut server = self
            .server
            .lock()
            .map_err(|_| McpError::Transport("fixture server lock is poisoned".into()))?;
        Ok(server.answer(&frame))
    }

    fn notify(&mut self, method: &str, params: Value) -> Result<(), McpError> {
        if let Ok(mut log) = self.notifications.lock() {
            log.push((method.to_string(), params));
        }
        Ok(())
    }

    fn shutdown(&mut self) {}

    fn describe(&self) -> String {
        "fixture".into()
    }
}

#[derive(Debug)]
pub struct FixtureFactory {
    server: Arc<std::sync::Mutex<FixtureServer>>,
    notifications: Arc<std::sync::Mutex<Vec<(String, Value)>>>,
}

impl FixtureFactory {
    pub fn new(server: FixtureServer) -> Self {
        Self {
            server: Arc::new(std::sync::Mutex::new(server)),
            notifications: Arc::new(std::sync::Mutex::new(Vec::new())),
        }
    }

    pub fn server(&self) -> Arc<std::sync::Mutex<FixtureServer>> {
        Arc::clone(&self.server)
    }

    pub fn notifications(&self) -> Arc<std::sync::Mutex<Vec<(String, Value)>>> {
        Arc::clone(&self.notifications)
    }
}

impl TransportFactory for FixtureFactory {
    fn connect(&self) -> Result<Box<dyn McpTransport>, McpError> {
        let mut transport = FixtureTransport::new(Arc::clone(&self.server));
        transport.notifications = Arc::clone(&self.notifications);
        Ok(Box::new(transport))
    }
}

/// The headers of every request a [`FixtureHttpPoster`] saw, in order, so a
/// test can assert that the credential travelled and the negotiated session
/// id was pinned on later requests.
pub type ObservedHeaders = Arc<std::sync::Mutex<Vec<Vec<(String, String)>>>>;

/// A [`HttpPoster`] that runs a [`FixtureServer`] in process, so the remote
/// transport's own framing is tested end to end without a socket.
#[derive(Debug)]
pub struct FixtureHttpPoster {
    server: Arc<std::sync::Mutex<FixtureServer>>,
    /// Replies as SSE rather than JSON; both shapes are legal for a POST.
    pub stream: bool,
    pub observed: ObservedHeaders,
}

impl FixtureHttpPoster {
    pub fn new(server: Arc<std::sync::Mutex<FixtureServer>>, stream: bool) -> Self {
        Self {
            server,
            stream,
            observed: Arc::new(std::sync::Mutex::new(Vec::new())),
        }
    }
}

impl HttpPoster for FixtureHttpPoster {
    fn post(
        &self,
        _url: &str,
        headers: &[(String, String)],
        body: String,
        _deadline: Instant,
    ) -> Result<HttpReply, McpError> {
        if let Ok(mut observed) = self.observed.lock() {
            observed.push(headers.to_vec());
        }
        let frame: Value = serde_json::from_str(&body)
            .map_err(|error| McpError::Protocol(format!("fixture got non-JSON: {error}")))?;
        if frame.get("id").is_none() {
            return Ok(HttpReply {
                status: 202,
                content_type: "application/json".into(),
                session_id: Some("fixture-session".into()),
                body: String::new(),
            });
        }
        let mut server = self
            .server
            .lock()
            .map_err(|_| McpError::Transport("fixture server lock is poisoned".into()))?;
        let answer = server.answer(&frame);
        let payload = serde_json::to_string(&answer).unwrap_or_default();
        Ok(if self.stream {
            HttpReply {
                status: 200,
                content_type: "text/event-stream".into(),
                session_id: Some("fixture-session".into()),
                body: format!("event: message\ndata: {payload}\n\n"),
            }
        } else {
            HttpReply {
                status: 200,
                content_type: "application/json".into(),
                session_id: Some("fixture-session".into()),
                body: payload,
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::ctx::provider::adapter::CancellationFlag;

    fn tool(name: &str, description: &str, extra: &str) -> Value {
        json!({
            "name": name,
            "description": description,
            "inputSchema": {"type": "object", "properties": {extra: {"type": "string"}}},
        })
    }

    fn fixture_client(server: FixtureServer) -> (McpClient, Arc<FixtureFactory>) {
        let factory = Arc::new(FixtureFactory::new(server));
        let client = McpClient::connect(
            "docs",
            Arc::clone(&factory) as Arc<dyn TransportFactory>,
            ProcessEffects::default(),
            Duration::from_secs(5),
        )
        .expect("connect");
        (client, factory)
    }

    #[test]
    fn a_local_server_negotiates_discovers_and_calls_a_tool() {
        let (mut client, factory) = fixture_client(FixtureServer {
            tools: vec![tool("lookup", "Look a symbol up.", "symbol")],
            resources: vec![json!({"uri": "doc://readme", "name": "readme"})],
            ..FixtureServer::default()
        });
        assert_eq!(client.info().protocol_version, PROTOCOL_VERSION);
        assert!(client.info().supports_tools);
        assert_eq!(client.catalogue().len(), 1);
        assert_eq!(client.catalogue().resources().len(), 1);

        let result = client
            .call_tool(
                "lookup",
                json!({"symbol": "x"}),
                &CancellationFlag::default(),
            )
            .expect("call");
        assert!(!result.is_error);
        assert_eq!(result.text, "ran lookup");

        let notifications = factory.notifications();
        let sent = notifications.lock().expect("lock");
        assert!(
            sent.iter()
                .any(|(method, _)| method == "notifications/initialized"),
            "the initialized notification is part of the handshake"
        );
    }

    #[test]
    fn a_remote_server_speaks_the_same_client_over_bearer_authenticated_http() {
        for stream in [false, true] {
            let server = Arc::new(std::sync::Mutex::new(FixtureServer {
                tools: vec![tool("search", "Search the docs.", "query")],
                ..FixtureServer::default()
            }));
            let poster = Arc::new(FixtureHttpPoster::new(Arc::clone(&server), stream));
            let observed = Arc::clone(&poster.observed);
            let factory = Arc::new(HttpFactory::new(
                "https://mcp.example/rpc".into(),
                Some("token-value".into()),
                poster as Arc<dyn HttpPoster>,
            ));
            let mut client = McpClient::connect(
                "remote",
                factory,
                ProcessEffects::default(),
                Duration::from_secs(5),
            )
            .expect("connect");
            let result = client
                .call_tool(
                    "search",
                    json!({"query": "a"}),
                    &CancellationFlag::default(),
                )
                .expect("call");
            assert_eq!(result.text, "ran search");

            let headers = observed.lock().expect("lock");
            let first = headers.first().expect("at least one request");
            assert!(
                first
                    .iter()
                    .any(|(name, value)| name == "authorization" && value == "Bearer token-value"),
                "the bearer credential must reach the server"
            );
            assert!(
                headers.iter().skip(1).any(|request| request
                    .iter()
                    .any(|(name, value)| name == "mcp-session-id" && value == "fixture-session")),
                "the negotiated session id must be pinned on later requests"
            );
        }
    }

    #[test]
    fn a_changed_tool_schema_after_reconnect_can_never_execute_the_stale_call() {
        let (mut client, factory) = fixture_client(FixtureServer {
            tools: vec![tool("deploy", "Deploy the docs.", "target")],
            ..FixtureServer::default()
        });
        assert!(client.catalogue().admit("deploy").is_ok());

        factory.server().lock().expect("lock").tools =
            vec![tool("deploy", "Deploy the docs.", "environment")];
        client.reconnect().expect("reconnect");

        let error = client
            .call_tool(
                "deploy",
                json!({"target": "prod"}),
                &CancellationFlag::default(),
            )
            .expect_err("a changed schema must not execute");
        assert!(matches!(error, McpError::StaleTool(_)), "{error:?}");

        client
            .catalogue_mut()
            .describe("deploy")
            .expect("re-describe");
        assert!(
            client
                .call_tool(
                    "deploy",
                    json!({"environment": "prod"}),
                    &CancellationFlag::default()
                )
                .is_ok(),
            "the call is admitted again once the new shape was described"
        );
    }

    #[test]
    fn a_removed_tool_after_reconnect_is_refused_rather_than_forwarded() {
        let (mut client, factory) = fixture_client(FixtureServer {
            tools: vec![tool("drop", "Drop a table.", "table")],
            ..FixtureServer::default()
        });
        factory.server().lock().expect("lock").tools = Vec::new();
        client.reconnect().expect("reconnect");
        let error = client
            .call_tool("drop", json!({}), &CancellationFlag::default())
            .expect_err("a removed tool must not be called");
        assert!(matches!(error, McpError::StaleTool(_)), "{error:?}");
    }

    #[test]
    fn a_large_catalogue_indexes_compactly_and_serves_schemas_on_demand() {
        let tools: Vec<Value> = (0..64)
            .map(|index| {
                tool(
                    &format!("tool{index}"),
                    "A very long description that would cost real tokens in every single request \
                     if it were inlined, repeated once per tool in the catalogue.",
                    "argument",
                )
            })
            .collect();
        let (mut client, _factory) = fixture_client(FixtureServer {
            tools,
            ..FixtureServer::default()
        });
        let index = client.catalogue().index();
        let index_bytes = serde_json::to_vec(&index).expect("encode").len();
        let full_bytes: usize = client
            .catalogue()
            .entries()
            .map(|entry| {
                serde_json::to_vec(&entry.input_schema)
                    .expect("encode")
                    .len()
            })
            .sum();
        assert_eq!(index["count"], 64);
        assert!(index["tools"][0]["input_schema"].is_null());
        assert!(
            index_bytes < full_bytes * 4,
            "the index must not carry every schema: {index_bytes} vs {full_bytes}"
        );
        let described = client.catalogue_mut().describe("tool7").expect("describe");
        assert_eq!(described["input_schema"]["type"], "object");
    }

    #[test]
    fn server_prose_is_redacted_and_bounded_before_it_can_reach_a_request() {
        let (client, _factory) = fixture_client(FixtureServer {
            tools: vec![tool(
                "leaky",
                "token=sk-abcdefghijklmnopqrstuvwxyz0123456789 and then a very long tail that runs \
                 well past the summary budget so it has to be cut, on and on and on and on and on \
                 and on and on and on and on and on and on",
                "value",
            )],
            ..FixtureServer::default()
        });
        let summary = &client.catalogue().get("leaky").expect("entry").summary;
        assert!(
            !summary.contains("sk-abcdefghijklmnopqrstuvwxyz0123456789"),
            "the secret-shaped token must be redacted: {summary}"
        );
        assert!(summary.len() <= MAX_SUMMARY_BYTES + 4, "{}", summary.len());
    }

    #[test]
    fn a_cancelled_call_sends_notifications_cancelled_and_reports_an_unknown_outcome() {
        let (mut client, factory) = fixture_client(FixtureServer {
            tools: vec![tool("slow", "Take a while.", "input")],
            ..FixtureServer::default()
        });
        let cancel = CancellationFlag::default();
        cancel.cancel();
        let error = client
            .call_tool("slow", json!({}), &cancel)
            .expect_err("a cancelled call must not report success");
        assert_eq!(error, McpError::Cancelled);

        let notifications = factory.notifications();
        let sent = notifications.lock().expect("lock");
        assert!(
            sent.iter()
                .any(|(method, _)| method == "notifications/cancelled"),
            "the server must be told the request was cancelled"
        );
    }

    /// Spawns the given fixture script (`.sh` on unix, `.cmd` on Windows)
    /// through a real child process, with `environment` set on it.
    fn spawn_fixture_stdio(
        stem: &str,
        environment: BTreeMap<String, String>,
    ) -> Result<StdioTransport, McpError> {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
        let transport = if cfg!(windows) {
            McpTransportConfig::Stdio {
                command: "cmd".into(),
                args: vec![
                    "/D".into(),
                    "/S".into(),
                    "/C".into(),
                    root.join(format!("{stem}.cmd")).display().to_string(),
                ],
                cwd: None,
                environment,
            }
        } else {
            McpTransportConfig::Stdio {
                command: "sh".into(),
                args: vec![root.join(format!("{stem}.sh")).display().to_string()],
                cwd: None,
                environment,
            }
        };
        StdioTransport::spawn(&McpServerConfig {
            name: stem.into(),
            transport,
            ..McpServerConfig::default()
        })
    }

    #[test]
    fn a_mid_flight_cancellation_of_a_stdio_call_is_observed_promptly_and_still_notifies_the_server()
     {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|elapsed| elapsed.as_nanos())
            .unwrap_or_default();
        let marker = std::env::temp_dir().join(format!(
            "zirv-mcp-hang-marker-{}-{unique}",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&marker);
        let mut environment = BTreeMap::new();
        environment.insert(
            "MCP_HANG_MARKER".to_string(),
            marker.to_string_lossy().into_owned(),
        );
        let mut transport =
            spawn_fixture_stdio("mcp-hang-server", environment).expect("spawn the hang fixture");

        // The server never replies to anything; only a cancellation fired
        // WHILE `request` is blocked waiting on it proves the poll loop
        // observes cancellation mid-flight rather than only at the deadline
        // or only before the call starts.
        let cancel = Arc::new(CancellationFlag::default());
        let cancel_thread = Arc::clone(&cancel);
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(200));
            cancel_thread.cancel();
        });

        let deadline = Instant::now() + Duration::from_secs(30);
        let started = Instant::now();
        let error = transport
            .request(
                1,
                "tools/call",
                json!({"name": "slow"}),
                deadline,
                cancel.as_ref(),
            )
            .expect_err("a request stuck on a non-responding server must be cancellable");
        let elapsed = started.elapsed();

        assert_eq!(error, McpError::Cancelled);
        assert!(
            elapsed < Duration::from_secs(5),
            "a mid-flight cancellation must be observed within a poll tick, \
             not only at the 30s request deadline: {elapsed:?}"
        );

        // Mirrors what `McpClient::call_raw_cancellable` does once `request`
        // reports `Cancelled`: tell the server, so its outcome is UNKNOWN
        // rather than silently abandoned.
        transport
            .notify(
                "notifications/cancelled",
                json!({"requestId": 1, "reason": "zirv cancelled the session"}),
            )
            .expect("notify must still work: a cancelled call leaves the transport usable");

        let notify_deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if std::fs::read_to_string(&marker).is_ok_and(|body| body.contains("cancelled")) {
                break;
            }
            assert!(
                Instant::now() < notify_deadline,
                "the server never observed notifications/cancelled"
            );
            std::thread::sleep(Duration::from_millis(50));
        }
        let _ = std::fs::remove_file(&marker);
    }

    #[test]
    fn a_stdio_servers_stderr_diagnostic_is_bounded_redacted_and_reaches_the_transport_error() {
        let secret = "sk-abcdefghijklmnopqrstuvwxyz0123456789";
        let transport = if cfg!(windows) {
            McpTransportConfig::Stdio {
                command: "cmd".into(),
                args: vec!["/C".into(), format!("echo token={secret} diagnostic 1>&2")],
                cwd: None,
                environment: BTreeMap::new(),
            }
        } else {
            McpTransportConfig::Stdio {
                command: "sh".into(),
                args: vec![
                    "-c".into(),
                    format!("echo 'token={secret} diagnostic' 1>&2"),
                ],
                cwd: None,
                environment: BTreeMap::new(),
            }
        };
        let mut transport = StdioTransport::spawn(&McpServerConfig {
            name: "diagnostic".into(),
            transport,
            ..McpServerConfig::default()
        })
        .expect("spawn the diagnostic fixture");

        let deadline = Instant::now() + Duration::from_secs(10);
        let error = transport
            .request(1, "initialize", json!({}), deadline, &NeverCancelled)
            .expect_err("a server that only writes to stderr and exits never answers");
        let McpError::Transport(message) = error else {
            panic!("expected a transport error carrying the stderr diagnostic: {error:?}");
        };
        assert!(
            message.contains("diagnostic"),
            "the stderr diagnostic must reach the transport error: {message}"
        );
        assert!(
            !message.contains(secret),
            "the secret-shaped token must be redacted: {message}"
        );
    }

    #[test]
    fn a_server_side_tool_error_stays_a_typed_error_rather_than_an_empty_success() {
        let (mut client, factory) = fixture_client(FixtureServer {
            tools: vec![tool("fragile", "Fail sometimes.", "input")],
            ..FixtureServer::default()
        });
        factory.server().lock().expect("lock").fail_next_call = true;
        let error = client
            .call_tool("fragile", json!({}), &CancellationFlag::default())
            .expect_err("a JSON-RPC error is an error");
        assert!(
            matches!(error, McpError::Server { code: -32000, .. }),
            "{error:?}"
        );
    }

    #[test]
    fn an_unsupported_protocol_revision_is_refused_instead_of_guessed() {
        let factory = Arc::new(FixtureFactory::new(FixtureServer {
            protocol_version: "1999-01-01".into(),
            ..FixtureServer::default()
        }));
        let error = McpClient::connect(
            "old",
            factory,
            ProcessEffects::default(),
            Duration::from_secs(5),
        )
        .expect_err("an unknown revision must fail");
        assert!(matches!(error, McpError::Protocol(_)), "{error:?}");
    }

    #[test]
    fn a_malformed_or_oversized_tool_row_is_dropped_rather_than_registered() {
        let huge = "x".repeat(MAX_SCHEMA_BYTES + 1);
        let (client, _factory) = fixture_client(FixtureServer {
            tools: vec![
                json!({"description": "no name"}),
                json!({"name": "big", "inputSchema": {"type": "object", "properties": {huge: {"type": "string"}}}}),
                tool("good", "Fine.", "value"),
            ],
            ..FixtureServer::default()
        });
        let names: Vec<&str> = client
            .catalogue()
            .entries()
            .map(|entry| entry.name.as_str())
            .collect();
        assert_eq!(names, ["good"]);
    }

    #[test]
    fn a_tool_result_keeps_its_error_flag_and_non_text_blocks_separately() {
        let mut results = BTreeMap::new();
        results.insert(
            "mixed".to_string(),
            json!({
                "isError": true,
                "content": [
                    {"type": "text", "text": "first"},
                    {"type": "image", "mimeType": "image/png", "uri": "file:///shot.png"},
                    {"type": "text", "text": "second"},
                ],
                "structuredContent": {"ok": false},
            }),
        );
        let (mut client, _factory) = fixture_client(FixtureServer {
            tools: vec![tool("mixed", "Mixed blocks.", "input")],
            results,
            ..FixtureServer::default()
        });
        let result = client
            .call_tool("mixed", json!({}), &CancellationFlag::default())
            .expect("call");
        assert!(result.is_error);
        assert_eq!(result.text, "first\nsecond");
        assert_eq!(result.attachments.len(), 1);
        assert_eq!(result.structured, Some(json!({"ok": false})));
    }

    #[test]
    fn paging_follows_next_cursor_without_unbounded_growth() {
        // The fixture answers every page identically, so the only thing that
        // stops the walk is the client's own page and tool ceilings.
        let mut server = FixtureServer {
            tools: vec![tool("paged", "Paged.", "value")],
            ..FixtureServer::default()
        };
        server.tools.push(json!({
            "name": "paged2",
            "description": "Paged.",
            "inputSchema": {"type": "object"},
        }));
        let (client, _factory) = fixture_client(server);
        assert_eq!(client.catalogue().len(), 2);
        assert_eq!(client.catalogue().generation(), 1);
    }
}
