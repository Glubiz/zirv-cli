//! Approvals inbox (#840): a Claude `PermissionRequest` hook holds its request for the owning
//! dashboard's operator, who answers on the dashboard instead of visiting the pane.
//!
//! Trust boundary: a decision travels ONLY from the dashboard process to the hook, on the hook's
//! own unix-socket connection, and the hook checks that the peer's pid is the session's recorded
//! `owner_pid`. Request records on disk are display-only data; any same-user process can write
//! them. The dashboard never accepts a decision from a client, and there is deliberately no CLI,
//! MCP or mail path that answers a request.

use std::collections::HashSet;
use std::path::PathBuf;
use std::time::Instant;

use serde::{Deserialize, Serialize};

use super::state::{StateDir, now_secs};

#[cfg(unix)]
use std::io::{Read, Write};
#[cfg(unix)]
use std::os::unix::net::{UnixListener, UnixStream};
#[cfg(unix)]
use std::path::Path;
#[cfg(unix)]
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
#[cfg(unix)]
use std::sync::{Arc, mpsc};
#[cfg(unix)]
use std::time::Duration;

/// Longest request or decision line either side will read.
#[cfg(unix)]
const MAX_LINE_BYTES: u64 = 8 * 1024;
/// Most requests one dashboard holds or serves connections for at once.
#[cfg(unix)]
const MAX_PENDING: usize = 32;
const PREVIEW_COLS: usize = 200;
/// The redacted full command or input summary a request carries for the details view.
const COMMAND_COLS: usize = 600;
/// Per-request hold lock files live here, beside (not inside) the per-session record directories.
const SINGLE_FLIGHT_DIR: &str = ".hold";
const FIELD_COLS: usize = 64;
/// How long a connected client may take to send its one request line.
#[cfg(unix)]
const REQUEST_READ_TIMEOUT: Duration = Duration::from_secs(2);

/// One held permission request. The same shape is the on-disk display record and the wire request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Request {
    /// Match key (session, tool, command): `PostToolUse` finds the record again from it.
    pub id: String,
    pub short: String,
    pub tool: String,
    /// Redacted, control-character-free, length-capped; displayed as data, never executed.
    pub preview: String,
    pub ts: u64,
    pub dash_pid: u32,
    /// The hold ended without a decision: the native dialog is waiting in the pane.
    #[serde(default)]
    pub released: bool,
    /// Distinguishes two holds of one match key (the hook's own pid).
    #[serde(default)]
    pub nonce: u32,
    /// The preview is the whole input: nothing was redacted or capped. `^A y` is inert otherwise.
    /// Absent on the wire means not shown in full.
    #[serde(default)]
    pub fully_shown: bool,
    /// The full command or tool input summary: redacted, control-character-free, at most `COMMAND_COLS`.
    #[serde(default)]
    pub command: String,
    #[serde(default)]
    pub cwd: Option<String>,
    /// A plain-language why: Claude's Bash `description`, or the Codex dialog's reason line.
    #[serde(default)]
    pub reason: Option<String>,
    /// The call runs outside the sandbox: Claude `dangerouslyDisableSandbox`, or a Codex escalation dialog.
    #[serde(default)]
    pub outside_sandbox: bool,
    /// A short label for the "always allow" rule the harness itself offers; `None` when it offers none.
    #[serde(default)]
    pub always: Option<String>,
    /// The session transcript and the call's `tool_use` id in it: a `tool_result` for that id there proves the
    /// call was answered, wherever it was answered. Display-data only; a read failure leaves the request listed.
    #[serde(default)]
    pub transcript_path: Option<String>,
    #[serde(default)]
    pub tool_use_id: Option<String>,
}

/// What a request carries beyond its tool and input: set by the hook or the Codex dialog reader.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RequestDetails {
    pub cwd: Option<String>,
    pub reason: Option<String>,
    pub outside_sandbox: bool,
    pub always: Option<String>,
    pub transcript_path: Option<String>,
    pub tool_use_id: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    Allow,
    /// Allow this call and apply the harness-offered "always allow" rule.
    AllowAlways,
    Deny,
    /// Show the native dialog now.
    Release,
}

impl Decision {
    #[cfg(unix)]
    fn wire(self) -> &'static str {
        match self {
            Decision::Allow => "allow",
            Decision::AllowAlways => "allow_always",
            Decision::Deny => "deny",
            Decision::Release => "release",
        }
    }

    #[cfg(unix)]
    fn parse(line: &str) -> Option<Self> {
        let value: serde_json::Value = serde_json::from_str(line.trim()).ok()?;
        match value.get("decision")?.as_str()? {
            "allow" => Some(Decision::Allow),
            "allow_always" => Some(Decision::AllowAlways),
            "deny" => Some(Decision::Deny),
            "release" => Some(Decision::Release),
            _ => None,
        }
    }
}

/// The rule Claude itself offered for this call: its own `permission_suggestions` entry, kept verbatim.
#[derive(Debug, Clone, PartialEq)]
pub struct AlwaysRule {
    entry: serde_json::Value,
    pub label: String,
}

/// The first suggestion that only adds allow rules, as Claude sent it. Mode changes, directory grants and
/// anything else a suggestion can carry are never offered, and no rule is ever built here.
pub fn always_rule(suggestions: &[serde_json::Value]) -> Option<AlwaysRule> {
    suggestions.iter().find_map(|entry| {
        let adds_allow = entry.get("type")?.as_str()? == "addRules"
            && entry.get("behavior")?.as_str()? == "allow";
        let rules = entry.get("rules")?.as_array()?;
        // The whole entry is echoed back, so a multi-rule entry would grant more than the label says.
        let [rule] = rules.as_slice() else {
            return None;
        };
        let tool = rule.get("toolName")?.as_str()?;
        let content = rule
            .get("ruleContent")
            .and_then(|c| c.as_str())
            .map(|c| c.trim_end_matches(['*', ':', ' ']))
            .filter(|c| !c.is_empty());
        let label = match (tool, content) {
            ("Bash" | "PowerShell", Some(content)) => format!("{content} commands"),
            (tool, Some(content)) => format!("{tool} {content}"),
            (tool, None) => format!("every {tool} call"),
        };
        adds_allow.then(|| AlwaysRule {
            entry: entry.clone(),
            label: clean(&super::snapshot::redact_text(&label), FIELD_COLS),
        })
    })
}

/// Exactly the hook output Claude honours for one call. `updatedPermissions` appears only for
/// `AllowAlways`, and then holds the one suggestion Claude sent; without one the answer is "show the dialog".
pub fn decision_json(decision: Decision, rule: Option<&AlwaysRule>) -> Option<String> {
    let decision_object = match decision {
        Decision::Allow => serde_json::json!({ "behavior": "allow" }),
        Decision::AllowAlways => serde_json::json!({
            "behavior": "allow",
            "updatedPermissions": [rule?.entry.clone()]
        }),
        Decision::Deny => serde_json::json!({ "behavior": "deny" }),
        Decision::Release => return None,
    };
    Some(
        serde_json::json!({
            "hookSpecificOutput": {
                "hookEventName": "PermissionRequest",
                "decision": decision_object
            }
        })
        .to_string(),
    )
}

pub fn approvals_dir(state: &StateDir) -> PathBuf {
    state.root().join("approvals")
}

/// One socket per dashboard process, named by pid so a hook derives it from the session's `owner_pid`.
#[cfg(unix)]
pub fn socket_path(state: &StateDir, pid: u32) -> PathBuf {
    state.sockets().join(format!("a{pid}.sock"))
}

pub fn request_id(short: &str, tool: &str, command: &str, preview_source: &str) -> String {
    let digest = super::safety::sha256_hex(
        format!("{short}\0{tool}\0{command}\0{preview_source}").as_bytes(),
    );
    digest.chars().take(16).collect()
}

fn is_safe_name(text: &str) -> bool {
    !text.is_empty()
        && text.len() <= 32
        && text
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

/// Strip control characters (terminal escapes included) and cap the width of untrusted text.
pub fn clean(text: &str, max_cols: usize) -> String {
    let flat: String = text
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    let squeezed = flat.split_whitespace().collect::<Vec<_>>().join(" ");
    if squeezed.chars().count() <= max_cols {
        return squeezed;
    }
    let kept: String = squeezed.chars().take(max_cols.saturating_sub(1)).collect();
    format!("{kept}\u{2026}")
}

/// The preview the operator sees: always redacted with the snapshot scrubber, never the raw input.
pub fn redacted_preview(raw: &str) -> String {
    clean(&super::snapshot::redact_text(raw), PREVIEW_COLS)
}

impl Request {
    pub fn new(
        short: &str,
        tool: &str,
        command: &str,
        preview_source: &str,
        dash_pid: u32,
    ) -> Self {
        let preview = redacted_preview(preview_source);
        Self {
            id: request_id(short, tool, command, preview_source),
            short: short.to_string(),
            tool: clean(tool, FIELD_COLS),
            fully_shown: preview == clean(preview_source, usize::MAX)
                && !preview_source.trim().contains('\n'),
            preview,
            ts: now_secs(),
            dash_pid,
            released: false,
            nonce: std::process::id(),
            command: clean(&super::snapshot::redact_text(preview_source), COMMAND_COLS),
            cwd: None,
            reason: None,
            outside_sandbox: false,
            always: None,
            transcript_path: None,
            tool_use_id: None,
        }
    }

    pub fn with_details(mut self, details: RequestDetails) -> Self {
        self.transcript_path = details.transcript_path;
        self.tool_use_id = details.tool_use_id;
        self.cwd = details.cwd;
        self.reason = details.reason;
        self.outside_sandbox = details.outside_sandbox;
        self.always = details.always;
        self.cleaned_details()
    }

    /// Detail text is display data: redacted, flattened and capped, whoever sent it.
    fn cleaned_details(mut self) -> Self {
        let tidy = |text: String, cols: usize| {
            Some(clean(&super::snapshot::redact_text(&text), cols)).filter(|t| !t.is_empty())
        };
        self.command = clean(&self.command, COMMAND_COLS);
        self.cwd = self.cwd.and_then(|t| tidy(t, PREVIEW_COLS));
        self.reason = self.reason.and_then(|t| tidy(t, PREVIEW_COLS));
        self.always = self.always.and_then(|t| tidy(t, FIELD_COLS));
        self
    }

    /// Re-clean a request that arrived from a client; `None` when it cannot name a record safely.
    #[cfg(unix)]
    fn revalidated(mut self, dash_pid: u32) -> Option<Self> {
        let id_ok = self.id.len() == 16 && self.id.chars().all(|c| c.is_ascii_hexdigit());
        if !id_ok || !is_safe_name(&self.short) {
            return None;
        }
        self.tool = clean(&self.tool, FIELD_COLS);
        let preview = clean(&self.preview, PREVIEW_COLS);
        self.fully_shown &= preview == self.preview;
        self.preview = preview;
        self.ts = now_secs();
        self.dash_pid = dash_pid;
        self.released = false;
        Some(self.cleaned_details())
    }

    pub fn waited_secs(&self) -> u64 {
        now_secs().saturating_sub(self.ts)
    }
}

pub fn record_path(state: &StateDir, request: &Request) -> PathBuf {
    approvals_dir(state)
        .join(&request.short)
        .join(format!("{}-{}.json", request.id, request.nonce))
}

#[cfg_attr(not(unix), allow(dead_code))]
pub fn write_record(state: &StateDir, request: &Request) -> std::io::Result<PathBuf> {
    let path = record_path(state, request);
    if let Some(parent) = path.parent() {
        super::state::create_private_dir_all(parent)?;
    }
    let body = serde_json::to_string(request).map_err(std::io::Error::other)?;
    super::state::write_private(&path, &body)?;
    Ok(path)
}

/// `PostToolUse` / `PermissionDenied`: the prompt is over, so its record goes. Cheap and silent when nothing exists.
pub fn clear_for_tool(
    state: &StateDir,
    short: &str,
    tool: &str,
    command: &str,
    preview_source: &str,
) {
    if !is_safe_name(short) {
        return;
    }
    let dir = approvals_dir(state).join(short);
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return;
    };
    let prefix = format!("{}-", request_id(short, tool, command, preview_source));
    for entry in entries.flatten() {
        if entry.file_name().to_string_lossy().starts_with(&prefix) {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

/// A session's next Stop, UserPromptSubmit or PostToolUseFailure proves its released prompts are over, so those
/// records go. Costs one failed `read_dir` when the session has none, and nothing while the inbox is off.
pub fn clear_released(state: &StateDir, short: &str, env: super::config::EnvLookup<'_>) {
    if !is_safe_name(short) {
        return;
    }
    let Ok(entries) = std::fs::read_dir(approvals_dir(state).join(short)) else {
        return;
    };
    if !super::config::ApprovalsConfig::load_operator_only(env).is_ok_and(|cfg| cfg.inbox) {
        return;
    }
    for entry in entries.flatten() {
        let released = std::fs::read_to_string(entry.path())
            .ok()
            .and_then(|text| serde_json::from_str::<Request>(&text).ok())
            .is_some_and(|request| request.released);
        if released {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

/// The most of a transcript's end a sweep reads: a pending call's result is among its last lines.
const TRANSCRIPT_TAIL_BYTES: u64 = 256 * 1024;

pub(crate) fn transcript_tail(path: &std::path::Path) -> Option<String> {
    super::models::read_transcript_tail(path, TRANSCRIPT_TAIL_BYTES)
}

/// Whether the transcript's tail shows `request`'s call answered: a `tool_result` for its `tool_use_id`, which
/// without a recorded one is the newest same-tool, same-input `tool_use` stamped no later than the request.
fn transcript_shows_answered(request: &Request) -> bool {
    let Some(tail) = request
        .transcript_path
        .as_deref()
        .and_then(|path| transcript_tail(std::path::Path::new(path)))
    else {
        return false;
    };
    let Some(tool_use_id) = request.tool_use_id.clone().or_else(|| {
        super::hook::transcript_tool_use_ids(&tail, &request.tool, Some(request.ts), |input| {
            request_id(
                &request.short,
                &request.tool,
                &input.command,
                &input.preview_source(&request.tool),
            ) == request.id
        })
        .into_iter()
        .next()
    }) else {
        return false;
    };
    tail.lines()
        .filter(|line| line.contains(&tool_use_id))
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .any(|line| {
            line["message"]["content"].as_array().is_some_and(|parts| {
                parts.iter().any(|part| {
                    part["type"] == "tool_result" && part["tool_use_id"] == tool_use_id.as_str()
                })
            })
        })
}

/// Every live request record across all dashboards, oldest first. Records of dead dashboards are swept.
pub fn list_all(state: &StateDir, pid_alive: &dyn Fn(u32) -> bool) -> Vec<Request> {
    let mut out = Vec::new();
    let Ok(sessions) = std::fs::read_dir(approvals_dir(state)) else {
        return out;
    };
    for session in sessions.flatten() {
        if session.file_name() == SINGLE_FLIGHT_DIR {
            continue;
        }
        let Ok(files) = std::fs::read_dir(session.path()) else {
            continue;
        };
        for file in files.flatten() {
            let path = file.path();
            let Some(request) = std::fs::read_to_string(&path)
                .ok()
                .and_then(|text| serde_json::from_str::<Request>(&text).ok())
            else {
                continue;
            };
            if !pid_alive(request.dash_pid) {
                let _ = std::fs::remove_file(&path);
                continue;
            }
            out.push(request);
        }
    }
    out.sort_by_key(|request| request.ts);
    out
}

/// The pid of the process on the other end of a connected unix socket, where the platform says so.
#[cfg(target_os = "macos")]
pub fn peer_pid(stream: &UnixStream) -> Option<u32> {
    use std::os::fd::AsRawFd;
    let mut pid: libc::pid_t = 0;
    let mut len = std::mem::size_of::<libc::pid_t>() as libc::socklen_t;
    // SAFETY: `pid` and `len` are live, correctly sized locals; the fd is owned by `stream`.
    let rc = unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_LOCAL,
            libc::LOCAL_PEERPID,
            std::ptr::addr_of_mut!(pid).cast(),
            &mut len,
        )
    };
    (rc == 0 && pid > 0).then_some(pid as u32)
}

#[cfg(any(target_os = "linux", target_os = "android"))]
pub fn peer_pid(stream: &UnixStream) -> Option<u32> {
    use std::os::fd::AsRawFd;
    let mut cred: libc::ucred = unsafe { std::mem::zeroed() };
    let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    // SAFETY: `cred` and `len` are live, correctly sized locals; the fd is owned by `stream`.
    let rc = unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            std::ptr::addr_of_mut!(cred).cast(),
            &mut len,
        )
    };
    (rc == 0 && cred.pid > 0).then_some(cred.pid as u32)
}

/// A platform that cannot name the peer cannot authenticate the dashboard, so the hold is refused.
#[cfg(all(
    unix,
    not(any(target_os = "macos", target_os = "linux", target_os = "android"))
))]
pub fn peer_pid(_stream: &UnixStream) -> Option<u32> {
    None
}

/// Hook side: send one request to the dashboard at `sock` and wait for its decision.
/// `None` for every failure, so the caller prints nothing and the native dialog shows.
#[cfg(unix)]
pub fn hold(sock: &Path, owner_pid: u32, request: &Request, hold: Duration) -> Option<Decision> {
    let stream = UnixStream::connect(sock).ok()?;
    // Anything on this path can bind a socket; only the dashboard that owns the pane may answer.
    if peer_pid(&stream)? != owner_pid {
        return None;
    }
    stream.set_write_timeout(Some(REQUEST_READ_TIMEOUT)).ok()?;
    let mut line = serde_json::to_string(request).ok()?;
    line.push('\n');
    (&stream).write_all(line.as_bytes()).ok()?;
    let deadline = Instant::now() + hold;
    stream
        .set_read_timeout(Some(hold.max(Duration::from_millis(1))))
        .ok()?;
    let mut buf = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        let remaining = deadline.checked_duration_since(Instant::now())?;
        // A peer that already answered and closed makes macOS refuse a new timeout; the first one still bounds the read.
        let _ = stream.set_read_timeout(Some(remaining.max(Duration::from_millis(1))));
        match (&stream).read(&mut byte) {
            Ok(1) if byte[0] == b'\n' => break,
            Ok(1) if (buf.len() as u64) < MAX_LINE_BYTES => buf.push(byte[0]),
            _ => return None,
        }
    }
    Decision::parse(std::str::from_utf8(&buf).ok()?)
}

/// Everything the dashboard's accept loop reports to its event loop.
#[cfg(unix)]
pub enum Event {
    Request {
        conn: u64,
        request: Box<Request>,
        reply: mpsc::Sender<Decision>,
    },
    Gone(u64),
}

/// The dashboard's listener. Clients only ever send ONE request line; whatever else they write is read and dropped.
#[cfg(unix)]
pub struct Server {
    path: PathBuf,
    rx: mpsc::Receiver<Event>,
}

#[cfg(unix)]
impl Server {
    pub fn bind(path: &Path, dash_pid: u32) -> super::CtxResult<Self> {
        if path.as_os_str().len() > super::signal::MAX_SOCKET_PATH {
            return Err(format!("approvals socket path is too long: {}", path.display()).into());
        }
        if let Some(parent) = path.parent() {
            super::state::create_private_dir_all(parent)?;
        }
        if path.exists() {
            std::fs::remove_file(path)?;
        }
        let listener = UnixListener::bind(path)?;
        let (tx, rx) = mpsc::channel();
        let active = Arc::new(AtomicUsize::new(0));
        let next_conn = Arc::new(AtomicU64::new(1));
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(stream) = stream else { continue };
                if active.fetch_add(1, Ordering::SeqCst) >= MAX_PENDING {
                    active.fetch_sub(1, Ordering::SeqCst);
                    continue;
                }
                let tx = tx.clone();
                let active = Arc::clone(&active);
                let conn = next_conn.fetch_add(1, Ordering::SeqCst);
                std::thread::spawn(move || {
                    serve(stream, conn, dash_pid, &tx);
                    active.fetch_sub(1, Ordering::SeqCst);
                });
            }
        });
        Ok(Self {
            path: path.to_path_buf(),
            rx,
        })
    }

    pub fn try_recv(&self) -> Option<Event> {
        self.rx.try_recv().ok()
    }
}

#[cfg(unix)]
impl Drop for Server {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

#[cfg(unix)]
fn read_request_line(stream: &UnixStream) -> Option<String> {
    stream.set_read_timeout(Some(REQUEST_READ_TIMEOUT)).ok()?;
    let mut buf = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        match (&*stream).read(&mut byte) {
            Ok(1) if byte[0] == b'\n' => break,
            Ok(1) if (buf.len() as u64) < MAX_LINE_BYTES => buf.push(byte[0]),
            _ => return None,
        }
    }
    String::from_utf8(buf).ok()
}

/// One connection: read one request, hand it to the event loop, then relay at most one decision back.
#[cfg(unix)]
fn serve(stream: UnixStream, conn: u64, dash_pid: u32, events: &mpsc::Sender<Event>) {
    let Some(line) = read_request_line(&stream) else {
        return;
    };
    let Some(request) = serde_json::from_str::<Request>(&line)
        .ok()
        .and_then(|request| request.revalidated(dash_pid))
    else {
        return;
    };
    let (reply, decisions) = mpsc::channel();
    if events
        .send(Event::Request {
            conn,
            request: Box::new(request),
            reply,
        })
        .is_err()
    {
        return;
    }
    if stream.set_nonblocking(true).is_err() {
        let _ = events.send(Event::Gone(conn));
        return;
    }
    loop {
        match decisions.recv_timeout(Duration::from_millis(250)) {
            Ok(decision) => {
                let _ = stream.set_nonblocking(false);
                let _ = stream.set_write_timeout(Some(REQUEST_READ_TIMEOUT));
                let _ = (&stream)
                    .write_all(format!("{{\"decision\":\"{}\"}}\n", decision.wire()).as_bytes());
                return;
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => return,
            Err(mpsc::RecvTimeoutError::Timeout) => {}
        }
        // Anything the client sends after its request is discarded: it can never be a decision.
        let mut sink = [0u8; 256];
        match (&stream).read(&mut sink) {
            Ok(0) => {
                let _ = events.send(Event::Gone(conn));
                return;
            }
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
            Err(_) => {
                let _ = events.send(Event::Gone(conn));
                return;
            }
        }
    }
}

/// A request the dashboard is holding or has released to its pane.
pub struct Item {
    pub conn: u64,
    pub request: Request,
    /// `None` once released: the hook is gone and only the pane can answer.
    #[cfg(unix)]
    reply: Option<mpsc::Sender<Decision>>,
    pub since: Instant,
}

/// What the strip last showed the operator: a key answers THAT request, never whatever now sits at its index.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Drawn {
    /// Nothing has drawn yet (headless callers and tests): fall back to the selection.
    #[cfg_attr(not(unix), allow(dead_code))]
    Never,
    Nothing,
    Item {
        conn: u64,
        answerable: bool,
    },
}

/// What `Hub::resolve_current` did, for the operator's notice line.
#[derive(Debug, PartialEq, Eq)]
pub enum Resolved {
    Sent,
    /// The preview was redacted, capped or clipped, so allowing it blind is refused.
    NotFullyShown,
    /// The harness offered no "always allow" rule for this request, so there is nothing to apply.
    NoAlways,
    /// The hold already ended; the pane's own dialog is the only way to answer.
    #[cfg_attr(not(unix), allow(dead_code))]
    InPane,
    Nothing,
}

/// The dashboard's inbox: its listener plus the requests it is holding.
pub struct Hub {
    state: StateDir,
    #[cfg(unix)]
    server: Server,
    items: Vec<Item>,
    /// The selected request's connection id; `0` while none is.
    selected_conn: u64,
    drawn: std::cell::Cell<Drawn>,
}

impl Hub {
    #[cfg(unix)]
    pub fn bind(state: &StateDir) -> super::CtxResult<Self> {
        let pid = std::process::id();
        Ok(Self {
            state: state.clone(),
            server: Server::bind(&socket_path(state, pid), pid)?,
            items: Vec::new(),
            selected_conn: 0,
            drawn: std::cell::Cell::new(Drawn::Never),
        })
    }

    #[cfg(not(unix))]
    pub fn bind(_state: &StateDir) -> super::CtxResult<Self> {
        Err("the approvals inbox needs unix sockets".into())
    }

    /// Drain the listener and drop requests whose record or pane is gone.
    /// `pane_exists` is this dashboard's own pane list: the dashboard revalidates every request against it.
    pub fn poll(&mut self, pane_exists: &dyn Fn(&str) -> bool) {
        #[cfg(unix)]
        while let Some(event) = self.server.try_recv() {
            match event {
                Event::Request {
                    conn,
                    request,
                    reply,
                } => {
                    let request = *request;
                    if self.items.len() >= MAX_PENDING || !pane_exists(&request.short) {
                        let _ = reply.send(Decision::Release);
                        continue;
                    }
                    // The hook wrote the record first; a client that did not is still shown, but its record is ours to make.
                    let _ = write_record(&self.state, &request);
                    self.items.push(Item {
                        conn,
                        request,
                        reply: Some(reply),
                        since: Instant::now(),
                    });
                }
                Event::Gone(conn) => {
                    if let Some(item) = self.items.iter_mut().find(|item| item.conn == conn) {
                        item.reply = None;
                        item.request.released = true;
                    }
                }
            }
        }
        let state = &self.state;
        self.items.retain(|item| {
            let keep =
                pane_exists(&item.request.short) && record_path(state, &item.request).exists();
            if !keep {
                let _ = std::fs::remove_file(record_path(state, &item.request));
            }
            keep
        });
        if !self
            .items
            .iter()
            .any(|item| item.conn == self.selected_conn)
        {
            self.selected_conn = self.items.first().map_or(0, |item| item.conn);
        }
    }

    /// Drop released requests whose pane no longer shows an open approval: the prompt was answered there.
    pub fn drop_released_unless(&mut self, approval_open: &dyn Fn(&str) -> bool) {
        let state = &self.state;
        self.items.retain(|item| {
            let keep = !item.request.released || approval_open(&item.request.short);
            if !keep {
                let _ = std::fs::remove_file(record_path(state, &item.request));
            }
            keep
        });
        if !self
            .items
            .iter()
            .any(|item| item.conn == self.selected_conn)
        {
            self.selected_conn = self.items.first().map_or(0, |item| item.conn);
        }
    }

    /// Drop released requests whose call the session transcript shows answered, in the pane or anywhere else
    /// (an allow, or a deny, both write a `tool_result`), and end their prompt and latch. Read errors keep the item.
    pub fn drop_answered_released(&mut self) {
        let state = &self.state;
        self.items.retain(|item| {
            let request = &item.request;
            let answered = request.released && transcript_shows_answered(request);
            if answered {
                let _ = std::fs::remove_file(record_path(state, request));
                let now = now_secs();
                super::attention::resolve_prompts(
                    state,
                    &request.short,
                    |open| open.id == request.id,
                    super::attention::Observation::new(
                        super::attention::Authority::Transcript,
                        format!("permission answered in the pane: {}", request.tool),
                        100,
                        now,
                    )
                    .with_attention(super::attention::Attention::None),
                    now,
                );
            }
            !answered
        });
        if !self
            .items
            .iter()
            .any(|item| item.conn == self.selected_conn)
        {
            self.selected_conn = self.items.first().map_or(0, |item| item.conn);
        }
    }

    pub fn count(&self) -> usize {
        self.items.len()
    }

    pub fn selected_index(&self) -> usize {
        self.items
            .iter()
            .position(|item| item.conn == self.selected_conn)
            .unwrap_or(0)
    }

    /// The oldest first, then in arrival order: the strip shows the selected item, starting at the oldest.
    pub fn current(&self) -> Option<&Item> {
        self.items.get(self.selected_index())
    }

    pub fn next(&mut self) {
        if let Some(item) = self
            .items
            .get((self.selected_index() + 1) % self.items.len().max(1))
        {
            self.selected_conn = item.conn;
        }
    }

    /// Show the oldest pending request of this session; false when it has none.
    pub fn select_short(&mut self, short: &str) -> bool {
        let Some(conn) = self
            .items
            .iter()
            .find(|item| item.request.short == short)
            .map(|item| item.conn)
        else {
            return false;
        };
        self.selected_conn = conn;
        true
    }

    /// Show exactly this request, by its hook connection; false once it is gone.
    pub fn select_conn(&mut self, conn: u64, short: &str) -> bool {
        let found = self
            .items
            .iter()
            .any(|item| item.conn == conn && item.request.short == short);
        if found {
            self.selected_conn = conn;
        }
        found
    }

    /// Record what the strip just drew (`None`: nothing), so a later key answers exactly that request.
    pub fn mark_drawn(&self, shown: Option<(u64, bool)>) {
        self.drawn.set(
            shown.map_or(Drawn::Nothing, |(conn, answerable)| Drawn::Item {
                conn,
                answerable,
            }),
        );
    }

    /// The request the last frame showed; the selection itself before any frame was drawn.
    pub fn drawn_item(&self) -> Option<&Item> {
        match self.drawn.get() {
            Drawn::Never => self.current(),
            Drawn::Nothing => None,
            Drawn::Item { conn, .. } => self.items.iter().find(|item| item.conn == conn),
        }
    }

    /// Every pending request in arrival order, for the orchestrator dashboard's NEEDS YOU list.
    pub fn items(&self) -> &[Item] {
        &self.items
    }

    pub fn shorts(&self) -> HashSet<&str> {
        self.items
            .iter()
            .map(|item| item.request.short.as_str())
            .collect()
    }

    /// Answer the request the last frame showed on its own hook connection; inert once it is gone.
    pub fn resolve_current(&mut self, decision: Decision) -> Resolved {
        let Some(conn) = self.drawn_item().map(|item| item.conn) else {
            return Resolved::Nothing;
        };
        let answerable = match self.drawn.get() {
            Drawn::Item { answerable, .. } => answerable,
            _ => self
                .drawn_item()
                .is_some_and(|item| item.request.fully_shown),
        };
        if matches!(decision, Decision::Allow | Decision::AllowAlways) && !answerable {
            return Resolved::NotFullyShown;
        }
        let Some(pos) = self.items.iter().position(|item| item.conn == conn) else {
            return Resolved::Nothing;
        };
        if decision == Decision::AllowAlways && self.items[pos].request.always.is_none() {
            return Resolved::NoAlways;
        }
        let item = &mut self.items[pos];
        #[cfg(unix)]
        {
            let Some(reply) = item.reply.take() else {
                return Resolved::InPane;
            };
            let _ = reply.send(decision);
        }
        if decision == Decision::Release {
            item.request.released = true;
            return Resolved::Sent;
        }
        let item = self.items.remove(pos);
        let _ = std::fs::remove_file(record_path(&self.state, &item.request));
        if let Some(next) = self.items.get(pos.min(self.items.len().saturating_sub(1))) {
            self.selected_conn = next.conn;
        }
        Resolved::Sent
    }
}

/// At most one live hold per request id; `None` when another hook already holds it.
#[cfg(unix)]
fn single_flight_path(state: &StateDir, short: &str, id: &str) -> PathBuf {
    approvals_dir(state)
        .join(SINGLE_FLIGHT_DIR)
        .join(format!("{short}-{id}.lock"))
}

#[cfg(unix)]
fn single_flight(state: &StateDir, short: &str, id: &str) -> Option<super::state::FileLock> {
    let path = single_flight_path(state, short, id);
    super::state::create_private_dir_all(path.parent()?).ok()?;
    super::state::try_acquire_lock(&path).ok()
}

/// Hook side, one call: should this request be held, and what did the operator say?
/// `None` for every reason not to hold, with no file or socket touched beyond the registry read.
#[cfg(unix)]
pub fn hold_for_dashboard(
    state: &StateDir,
    short: &str,
    tool: &str,
    command: &str,
    preview_source: &str,
    details: RequestDetails,
    hold_for: Duration,
) -> Option<Decision> {
    if !is_safe_name(short) {
        return None;
    }
    let owner_pid = super::sessions::load_record(state, short)?.owner_pid?;
    if !super::sessions::is_alive(owner_pid) {
        return None;
    }
    let sock = socket_path(state, owner_pid);
    if !sock.exists() {
        return None;
    }
    let mut request =
        Request::new(short, tool, command, preview_source, owner_pid).with_details(details);
    // Claude may run two identical handlers for one call; only the first holds, the other prints nothing.
    let single_flight = single_flight(state, short, &request.id)?;
    let record = write_record(state, &request).ok()?;
    let decision = hold(&sock, owner_pid, &request, hold_for);
    match decision {
        Some(Decision::Allow | Decision::AllowAlways | Decision::Deny) => {
            let _ = std::fs::remove_file(&record);
        }
        // Timed out, released or failed: the native dialog now shows, and the record says so until `PostToolUse` clears it.
        _ => {
            request.released = true;
            let _ = write_record(state, &request);
        }
    }
    // Release first, then tidy: a later hold for the same id simply recreates the file.
    drop(single_flight);
    let _ = std::fs::remove_file(single_flight_path(state, short, &request.id));
    decision
}

#[cfg(not(unix))]
pub fn hold_for_dashboard(
    _state: &StateDir,
    _short: &str,
    _tool: &str,
    _command: &str,
    _preview_source: &str,
    _details: RequestDetails,
    _hold_for: std::time::Duration,
) -> Option<Decision> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(short: &str) -> Request {
        Request::new(
            short,
            "Bash",
            "cargo test",
            "cargo test",
            std::process::id(),
        )
    }

    fn suggestion() -> serde_json::Value {
        serde_json::json!({
            "type": "addRules",
            "rules": [{"toolName": "Bash", "ruleContent": "cargo nextest run:*"}],
            "behavior": "allow",
            "destination": "localSettings"
        })
    }

    #[test]
    fn decision_json_is_exactly_the_one_call_decision_and_never_updates_permissions() {
        for (decision, behavior) in [(Decision::Allow, "allow"), (Decision::Deny, "deny")] {
            let text = decision_json(decision, None).expect("decision");
            assert!(!text.contains("updatedPermissions"), "{text}");
            let value: serde_json::Value = serde_json::from_str(&text).expect("json");
            assert_eq!(
                value,
                serde_json::json!({"hookSpecificOutput": {
                    "hookEventName": "PermissionRequest",
                    "decision": {"behavior": behavior}
                }})
            );
        }
        assert_eq!(decision_json(Decision::Release, None), None);
        // A rule in hand never leaks into a plain allow or deny.
        let rule = always_rule(&[suggestion()]).expect("rule");
        for decision in [Decision::Allow, Decision::Deny] {
            let text = decision_json(decision, Some(&rule)).expect("decision");
            assert!(!text.contains("updatedPermissions"), "{text}");
        }
    }

    #[test]
    fn allow_always_applies_exactly_the_suggestion_claude_sent_and_nothing_else() {
        let rule = always_rule(&[suggestion()]).expect("rule");
        assert_eq!(rule.label, "cargo nextest run commands");
        let text = decision_json(Decision::AllowAlways, Some(&rule)).expect("decision");
        let value: serde_json::Value = serde_json::from_str(&text).expect("json");
        assert_eq!(
            value,
            serde_json::json!({"hookSpecificOutput": {
                "hookEventName": "PermissionRequest",
                "decision": {"behavior": "allow", "updatedPermissions": [suggestion()]}
            }})
        );
        assert_eq!(
            decision_json(Decision::AllowAlways, None),
            None,
            "no suggestion: the native dialog shows, no rule is built"
        );
    }

    #[test]
    fn only_a_plain_allow_rule_suggestion_is_ever_offered() {
        assert_eq!(always_rule(&[]), None);
        let mode =
            serde_json::json!({"type": "setMode", "mode": "acceptEdits", "destination": "session"});
        let dirs = serde_json::json!({"type": "addDirectories", "directories": ["/x"], "destination": "session"});
        let deny = serde_json::json!({
            "type": "addRules", "behavior": "deny", "destination": "session",
            "rules": [{"toolName": "Bash", "ruleContent": "ls"}]
        });
        assert_eq!(always_rule(&[mode.clone(), dirs, deny]), None);
        let whole_tool = serde_json::json!({
            "type": "addRules", "behavior": "allow", "destination": "session",
            "rules": [{"toolName": "WebFetch"}]
        });
        let picked = always_rule(&[mode, whole_tool]).expect("rule");
        assert_eq!(picked.label, "every WebFetch call");
    }

    #[test]
    fn a_multi_rule_suggestion_offers_no_always() {
        let two = serde_json::json!({
            "type": "addRules", "behavior": "allow", "destination": "session",
            "rules": [{"toolName": "Bash", "ruleContent": "ls"}, {"toolName": "Bash", "ruleContent": "rm"}]
        });
        assert_eq!(always_rule(&[two]), None);
    }

    #[test]
    fn untrusted_text_loses_control_characters_and_secrets_before_display() {
        let preview = redacted_preview("echo \u{1b}[31mhi\u{7}\nnext");
        assert!(
            !preview.contains('\u{1b}') && !preview.contains('\n'),
            "{preview:?}"
        );
        let long = clean(&"x".repeat(500), 40);
        assert_eq!(long.chars().count(), 40);
    }

    #[test]
    fn clearing_a_tool_removes_only_its_own_record() {
        let tmp = tempfile::tempdir().expect("tmp");
        let state = StateDir::resolve(&|_| Some(tmp.path().display().to_string())).expect("state");
        let ours = request("abc123");
        let other = Request::new("abc123", "Bash", "ls", "ls", 1);
        let ours_path = write_record(&state, &ours).expect("write");
        let other_path = write_record(&state, &other).expect("write");
        clear_for_tool(&state, "abc123", "Bash", "cargo test", "cargo test");
        assert!(!ours_path.exists(), "the resolved tool's record must go");
        assert!(other_path.exists(), "a different pending tool must stay");
    }

    #[cfg(unix)]
    #[test]
    fn a_second_hold_for_the_same_request_id_is_refused_while_the_first_lives() {
        let tmp = tempfile::tempdir().expect("tmp");
        let state = StateDir::resolve(&|_| Some(tmp.path().display().to_string())).expect("state");
        let first = single_flight(&state, "abc123", "0123456789abcdef");
        assert!(first.is_some());
        assert!(single_flight(&state, "abc123", "0123456789abcdef").is_none());
        assert!(single_flight(&state, "abc123", "fedcba9876543210").is_some());
        drop(first);
        assert!(single_flight(&state, "abc123", "0123456789abcdef").is_some());
    }

    #[cfg(unix)]
    #[test]
    fn list_all_skips_the_lock_directory() {
        let tmp = tempfile::tempdir().expect("tmp");
        let state = StateDir::resolve(&|_| Some(tmp.path().display().to_string())).expect("state");
        let lock = single_flight(&state, "abc123", "0123456789abcdef").expect("lock");
        // A stray request-shaped file in the lock directory must never be listed as a session's.
        let stray = request("abc123");
        let dir = approvals_dir(&state).join(SINGLE_FLIGHT_DIR);
        std::fs::write(
            dir.join("x.json"),
            serde_json::to_string(&stray).expect("json"),
        )
        .expect("write");
        assert!(list_all(&state, &|_| true).is_empty());
        drop(lock);
    }

    #[test]
    fn a_redacted_or_capped_preview_is_not_fully_shown_and_allow_is_inert() {
        let plain = Request::new("abc123", "Bash", "ls", "ls", 1);
        assert!(plain.fully_shown);
        let secret = Request::new(
            "abc123",
            "Bash",
            "echo ghp_1234567890abcdefghijklmnopqrstuvwx",
            "echo ghp_1234567890abcdefghijklmnopqrstuvwx",
            1,
        );
        assert!(!secret.fully_shown);
        let long = "x".repeat(PREVIEW_COLS + 5);
        assert!(!Request::new("abc123", "Bash", &long, &long, 1).fully_shown);
        let wire: Request = serde_json::from_str(
            &serde_json::to_string(&plain)
                .expect("json")
                .replace(",\"fully_shown\":true", ""),
        )
        .expect("old wire shape");
        assert!(!wire.fully_shown, "a client that omits it is not trusted");
    }

    #[test]
    fn distinct_inputs_of_one_command_less_tool_have_distinct_ids_and_clear_separately() {
        let tmp = tempfile::tempdir().expect("tmp");
        let state = StateDir::resolve(&|_| Some(tmp.path().display().to_string())).expect("state");
        let a = Request::new("abc123", "Edit", "", "/a.rs", 1);
        let b = Request::new("abc123", "Edit", "", "/b.rs", 1);
        assert_ne!(a.id, b.id);
        let (pa, pb) = (
            write_record(&state, &a).expect("a"),
            write_record(&state, &b).expect("b"),
        );
        clear_for_tool(&state, "abc123", "Edit", "", "/a.rs");
        assert!(!pa.exists() && pb.exists());
    }

    #[test]
    fn released_requests_are_dropped_once_the_pane_no_longer_shows_an_approval() {
        let tmp = tempfile::tempdir().expect("tmp");
        let state = StateDir::resolve(&|_| Some(tmp.path().display().to_string())).expect("state");
        let mut released = request("abc123");
        released.released = true;
        let path = write_record(&state, &released).expect("record");
        let env = {
            let root = tmp.path().display().to_string();
            move |key: &str| match key {
                "ZIRV_CTX_APPROVALS_INBOX" => Some("true".to_string()),
                super::super::state::STATE_ENV => Some(root.clone()),
                _ => None,
            }
        };
        clear_released(&state, "abc123", &env);
        assert!(!path.exists(), "the next Stop/prompt/failure drops it");
    }

    #[cfg(unix)]
    #[test]
    fn a_selection_is_followed_by_request_not_by_index_and_a_gone_request_is_inert() {
        let tmp = tempfile::tempdir().expect("tmp");
        let state = StateDir::resolve(&|_| Some(tmp.path().display().to_string())).expect("state");
        let mut hub = Hub {
            state: state.clone(),
            #[cfg(unix)]
            server: Server::bind(&tmp.path().join("a.sock"), 1).expect("server"),
            items: Vec::new(),
            selected_conn: 0,
            drawn: std::cell::Cell::new(Drawn::Never),
        };
        for (conn, short) in [(1, "aaaa"), (2, "bbbb")] {
            hub.items.push(Item {
                conn,
                request: request(short),
                reply: None,
                since: Instant::now(),
            });
        }
        hub.selected_conn = 2;
        hub.mark_drawn(Some((2, true)));
        // The drawn request leaves between draw and keypress; the other one shifts into its index.
        hub.items.remove(1);
        assert_eq!(hub.resolve_current(Decision::Deny), Resolved::Nothing);
        assert_eq!(hub.count(), 1, "the survivor was not answered");
        hub.mark_drawn(Some((1, false)));
        assert_eq!(
            hub.resolve_current(Decision::Allow),
            Resolved::NotFullyShown
        );
    }

    /// The agent tree shows one session's request by selecting it; answering still goes
    /// through the request the last frame drew.
    #[cfg(unix)]
    #[test]
    fn select_short_shows_that_sessions_request_and_answers_only_what_was_drawn() {
        let tmp = tempfile::tempdir().expect("tmp");
        let state = StateDir::resolve(&|_| Some(tmp.path().display().to_string())).expect("state");
        let mut hub = Hub {
            state: state.clone(),
            server: Server::bind(&tmp.path().join("a.sock"), 1).expect("server"),
            items: Vec::new(),
            selected_conn: 0,
            drawn: std::cell::Cell::new(Drawn::Never),
        };
        for (conn, short) in [(1, "aaaa"), (2, "bbbb")] {
            hub.items.push(Item {
                conn,
                request: request(short),
                reply: None,
                since: Instant::now(),
            });
        }
        hub.selected_conn = 1;
        hub.mark_drawn(Some((1, true)));
        assert!(!hub.select_short("cccc"), "no request for that session");
        assert!(hub.select_short("bbbb"));
        assert!(
            !hub.select_conn(9, "bbbb"),
            "a gone connection answers nothing"
        );
        assert!(
            !hub.select_conn(1, "bbbb"),
            "the short must match the connection"
        );
        assert!(hub.select_conn(2, "bbbb"));
        assert_eq!(
            hub.current().map(|i| i.request.short.as_str()),
            Some("bbbb")
        );
        assert_eq!(
            hub.drawn_item().map(|i| i.request.short.as_str()),
            Some("aaaa"),
            "the answerable request is still the one a frame drew"
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_released_item_goes_when_its_pane_stops_showing_an_approval() {
        let tmp = tempfile::tempdir().expect("tmp");
        let state = StateDir::resolve(&|_| Some(tmp.path().display().to_string())).expect("state");
        let mut hub = Hub {
            state: state.clone(),
            server: Server::bind(&tmp.path().join("a.sock"), 1).expect("server"),
            items: Vec::new(),
            selected_conn: 0,
            drawn: std::cell::Cell::new(Drawn::Never),
        };
        let mut released = request("aaaa");
        released.released = true;
        for (conn, request) in [(1, released), (2, request("bbbb"))] {
            write_record(&state, &request).expect("record");
            hub.items.push(Item {
                conn,
                request,
                reply: None,
                since: Instant::now(),
            });
        }
        hub.drop_released_unless(&|_| true);
        assert_eq!(hub.count(), 2, "the approval latch still reads open");
        hub.drop_released_unless(&|_| false);
        assert_eq!(
            hub.count(),
            1,
            "answered No in the pane: the strip item goes"
        );
        assert_eq!(
            hub.current().map(|i| i.conn),
            Some(2),
            "a held item is never dropped here"
        );
    }

    #[test]
    fn the_cross_dashboard_list_sees_every_dashboards_requests_and_sweeps_dead_ones() {
        let tmp = tempfile::tempdir().expect("tmp");
        let state = StateDir::resolve(&|_| Some(tmp.path().display().to_string())).expect("state");
        let mut here = request("aaaa1111");
        here.dash_pid = 100;
        let mut there = Request::new("bbbb2222", "Edit", "", "/x", 200);
        there.ts += 5;
        let mut dead = Request::new("cccc3333", "Bash", "ls", "ls", 300);
        dead.nonce = 9;
        for r in [&here, &there, &dead] {
            write_record(&state, r).expect("write");
        }
        let listed = list_all(&state, &|pid| pid != 300);
        let pids: Vec<u32> = listed.iter().map(|r| r.dash_pid).collect();
        assert_eq!(pids, vec![100, 200]);
        assert!(
            !record_path(&state, &dead).exists(),
            "a dead dashboard's record is swept"
        );
    }

    #[test]
    fn a_request_carries_redacted_capped_details_and_old_records_still_parse() {
        let long = (0..120)
            .map(|i| format!("--flag{i}"))
            .collect::<Vec<_>>()
            .join(" ");
        let request =
            Request::new("abc123", "Bash", &long, &long, 1).with_details(RequestDetails {
                cwd: Some("/work/repo".to_string()),
                reason: Some("Run the tests\nthen stop".to_string()),
                outside_sandbox: true,
                always: Some("cargo nextest run commands".to_string()),
                ..RequestDetails::default()
            });
        assert_eq!(request.command.chars().count(), COMMAND_COLS);
        let secret = "echo ghp_1234567890abcdefghijklmnopqrstuvwx";
        let redacted = Request::new("abc123", "Bash", secret, secret, 1);
        assert!(
            !redacted.command.contains("ghp_1234567890"),
            "{}",
            redacted.command
        );
        assert_eq!(request.reason.as_deref(), Some("Run the tests then stop"));
        assert!(request.outside_sandbox);
        let bare: Request = serde_json::from_str(
            r#"{"id":"0123456789abcdef","short":"abc123","tool":"Bash","preview":"ls","ts":1,"dash_pid":1}"#,
        )
        .expect("a record from before the details existed");
        assert!(bare.command.is_empty() && bare.cwd.is_none() && bare.always.is_none());
        assert!(!bare.outside_sandbox);
    }

    #[cfg(unix)]
    fn hub_with(requests: Vec<(u64, Request)>, dir: &Path) -> Hub {
        let state = StateDir::resolve(&|_| Some(dir.display().to_string())).expect("state");
        let mut hub = Hub {
            state,
            server: Server::bind(&dir.join("a.sock"), 1).expect("server"),
            items: Vec::new(),
            selected_conn: 0,
            drawn: std::cell::Cell::new(Drawn::Never),
        };
        for (conn, request) in requests {
            hub.items.push(Item {
                conn,
                request,
                reply: None,
                since: Instant::now(),
            });
        }
        hub
    }

    #[cfg(unix)]
    #[test]
    fn allow_always_keeps_every_guard_allow_has_and_needs_an_offered_rule() {
        let tmp = tempfile::tempdir().expect("tmp");
        let with_rule = request("aaaa").with_details(RequestDetails {
            always: Some("cargo test commands".to_string()),
            ..RequestDetails::default()
        });
        let mut hub = hub_with(vec![(1, with_rule), (2, request("bbbb"))], tmp.path());
        // The drawn request leaves between draw and keypress: the survivor is not answered.
        hub.mark_drawn(Some((3, true)));
        assert_eq!(
            hub.resolve_current(Decision::AllowAlways),
            Resolved::Nothing
        );
        assert_eq!(hub.count(), 2);
        // A request that was not shown in full is never allowed blind, always or not.
        hub.mark_drawn(Some((1, false)));
        assert_eq!(
            hub.resolve_current(Decision::AllowAlways),
            Resolved::NotFullyShown
        );
        // No offered rule: refused, and the request stays pending.
        hub.mark_drawn(Some((2, true)));
        assert_eq!(
            hub.resolve_current(Decision::AllowAlways),
            Resolved::NoAlways
        );
        assert_eq!(hub.count(), 2);
        // The same request with a rule reaches its (here already released) hook.
        hub.mark_drawn(Some((1, true)));
        assert_eq!(hub.resolve_current(Decision::AllowAlways), Resolved::InPane);
    }

    #[cfg(unix)]
    mod socket {
        use super::*;
        use std::io::{BufRead, BufReader};
        use std::time::Duration;

        fn sock_dir() -> tempfile::TempDir {
            tempfile::Builder::new()
                .prefix("ap")
                .tempdir_in(std::env::temp_dir())
                .expect("tmp")
        }

        /// A stand-in dashboard: accepts one connection, reads the request, answers with `reply`.
        fn fake_dashboard(path: &Path, reply: Option<&'static str>) -> std::thread::JoinHandle<()> {
            let listener = UnixListener::bind(path).expect("bind");
            std::thread::spawn(move || {
                let (stream, _) = listener.accept().expect("accept");
                let mut line = String::new();
                BufReader::new(&stream)
                    .read_line(&mut line)
                    .expect("request");
                if let Some(reply) = reply {
                    (&stream).write_all(reply.as_bytes()).expect("reply");
                }
                std::thread::sleep(Duration::from_millis(600));
            })
        }

        #[test]
        fn a_held_request_returns_allow_then_deny() {
            for (wire, expected) in [
                ("{\"decision\":\"allow\"}\n", Decision::Allow),
                ("{\"decision\":\"deny\"}\n", Decision::Deny),
            ] {
                let dir = sock_dir();
                let sock = dir.path().join("a.sock");
                let server = fake_dashboard(&sock, Some(wire));
                let got = hold(
                    &sock,
                    std::process::id(),
                    &request("abc123"),
                    Duration::from_secs(5),
                );
                assert_eq!(got, Some(expected));
                server.join().expect("server");
            }
        }

        #[test]
        fn a_hold_that_gets_no_answer_times_out_to_nothing() {
            let dir = sock_dir();
            let sock = dir.path().join("a.sock");
            let server = fake_dashboard(&sock, None);
            let started = Instant::now();
            let got = hold(
                &sock,
                std::process::id(),
                &request("abc123"),
                Duration::from_millis(300),
            );
            assert_eq!(got, None);
            assert!(started.elapsed() < Duration::from_secs(5));
            server.join().expect("server");
        }

        #[test]
        fn a_server_whose_peer_pid_is_wrong_is_rejected_before_anything_is_sent() {
            let dir = sock_dir();
            let sock = dir.path().join("a.sock");
            let listener = UnixListener::bind(&sock).expect("bind");
            let server = std::thread::spawn(move || {
                let (stream, _) = listener.accept().expect("accept");
                let mut line = String::new();
                BufReader::new(&stream).read_line(&mut line).unwrap_or(0)
            });
            let wrong = std::process::id().wrapping_add(1);
            let got = hold(&sock, wrong, &request("abc123"), Duration::from_secs(5));
            assert_eq!(got, None);
            assert_eq!(
                server.join().expect("server"),
                0,
                "no request may reach an unauthenticated server"
            );
        }

        #[test]
        fn the_hook_with_no_dashboard_socket_returns_nothing_and_writes_no_record() {
            let tmp = tempfile::tempdir().expect("tmp");
            let state =
                StateDir::resolve(&|_| Some(tmp.path().display().to_string())).expect("state");
            let got = hold_for_dashboard(
                &state,
                "abc123",
                "Bash",
                "ls",
                "ls",
                RequestDetails::default(),
                Duration::from_secs(5),
            );
            assert_eq!(got, None);
            assert!(!approvals_dir(&state).exists());
        }

        fn live_hub() -> (tempfile::TempDir, Hub) {
            let tmp = sock_dir();
            let state =
                StateDir::resolve(&|_| Some(tmp.path().display().to_string())).expect("state");
            let hub = Hub::bind(&state).expect("hub");
            (tmp, hub)
        }

        fn wait_for(hub: &mut Hub, want: usize) {
            let deadline = Instant::now() + Duration::from_secs(5);
            while hub.count() != want && Instant::now() < deadline {
                hub.poll(&|_| true);
                std::thread::sleep(Duration::from_millis(20));
            }
            assert_eq!(hub.count(), want);
        }

        #[test]
        fn the_dashboard_ignores_a_decision_sent_by_a_client() {
            let (tmp, mut hub) = live_hub();
            let state =
                StateDir::resolve(&|_| Some(tmp.path().display().to_string())).expect("state");
            let sock = socket_path(&state, std::process::id());
            let req = request("abc123");
            write_record(&state, &req).expect("record");
            let mut client = UnixStream::connect(&sock).expect("connect");
            client
                .write_all(
                    format!(
                        "{}\n{{\"decision\":\"allow\"}}\n",
                        serde_json::to_string(&req).expect("json")
                    )
                    .as_bytes(),
                )
                .expect("send");
            wait_for(&mut hub, 1);
            std::thread::sleep(Duration::from_millis(600));
            hub.poll(&|_| true);
            assert_eq!(
                hub.count(),
                1,
                "a client-sent decision must leave the request pending"
            );
            assert!(hub.current().is_some_and(|item| !item.request.released));
            client
                .set_read_timeout(Some(Duration::from_millis(200)))
                .expect("timeout");
            let mut buf = [0u8; 16];
            assert!(
                client.read(&mut buf).is_err(),
                "nothing may be sent back until the operator answers"
            );
        }

        #[test]
        fn resolving_allow_and_deny_travels_on_the_hooks_own_connection() {
            for (decision, expected) in [(Decision::Allow, "allow"), (Decision::Deny, "deny")] {
                let (tmp, mut hub) = live_hub();
                let state =
                    StateDir::resolve(&|_| Some(tmp.path().display().to_string())).expect("state");
                let sock = socket_path(&state, std::process::id());
                let req = request("abc123");
                write_record(&state, &req).expect("record");
                let waiter = std::thread::spawn(move || {
                    hold(&sock, std::process::id(), &req, Duration::from_secs(5))
                });
                wait_for(&mut hub, 1);
                assert_eq!(hub.resolve_current(decision), Resolved::Sent);
                assert_eq!(
                    waiter.join().expect("hook"),
                    Some(if expected == "allow" {
                        Decision::Allow
                    } else {
                        Decision::Deny
                    })
                );
                assert_eq!(hub.count(), 0);
            }
        }

        #[test]
        fn a_request_for_a_pane_this_dashboard_does_not_own_is_released_not_held() {
            let (tmp, mut hub) = live_hub();
            let state =
                StateDir::resolve(&|_| Some(tmp.path().display().to_string())).expect("state");
            let sock = socket_path(&state, std::process::id());
            let req = request("zzzz9999");
            let waiter = std::thread::spawn(move || {
                hold(&sock, std::process::id(), &req, Duration::from_secs(5))
            });
            let deadline = Instant::now() + Duration::from_secs(5);
            while !waiter.is_finished() && Instant::now() < deadline {
                hub.poll(&|_| false);
                std::thread::sleep(Duration::from_millis(20));
            }
            assert_eq!(waiter.join().expect("hook"), Some(Decision::Release));
            assert_eq!(hub.count(), 0);
        }

        /// A released request whose hook has gone and whose dialog is open in the pane; its prompt is confirmed.
        fn released_request_with_transcript(
            hub: &mut Hub,
            state: &StateDir,
            transcript: &Path,
            tool_use_id: Option<&str>,
        ) -> Request {
            let sock = socket_path(state, std::process::id());
            let req = request("abc123").with_details(RequestDetails {
                transcript_path: Some(transcript.display().to_string()),
                tool_use_id: tool_use_id.map(str::to_string),
                ..RequestDetails::default()
            });
            write_record(state, &req).expect("record");
            crate::commands::ctx::attention::open_prompt(
                state,
                "abc123",
                crate::commands::ctx::attention::OpenPrompt {
                    id: req.id.clone(),
                    ..Default::default()
                },
            );
            let client = {
                let mut c = UnixStream::connect(&sock).expect("connect");
                c.write_all(format!("{}\n", serde_json::to_string(&req).expect("json")).as_bytes())
                    .expect("send");
                c
            };
            wait_for(hub, 1);
            drop(client);
            let deadline = Instant::now() + Duration::from_secs(5);
            while !hub.items()[0].request.released && Instant::now() < deadline {
                hub.poll(&|_| true);
                std::thread::sleep(Duration::from_millis(20));
            }
            assert!(hub.items()[0].request.released);
            crate::commands::ctx::attention::confirm_prompts(state, "abc123", "x", 2);
            req
        }

        /// The per-tick sweep of dash/mod.rs without the transcript check, as it ran before.
        fn hook_only_sweep(hub: &mut Hub, state: &StateDir) {
            hub.poll(&|_| true);
            hub.drop_released_unless(&|short| {
                crate::commands::ctx::attention::load(state, short).attention
                    == crate::commands::ctx::attention::Attention::Approval
                    || crate::commands::ctx::attention::prompt_open(state, short)
            });
        }

        #[test]
        fn a_pane_denied_released_request_leaves_once_the_transcript_shows_its_rejection() {
            let (tmp, mut hub) = live_hub();
            let state =
                StateDir::resolve(&|_| Some(tmp.path().display().to_string())).expect("state");
            let transcript = tmp.path().join("session.jsonl");
            std::fs::write(&transcript, "").expect("transcript");
            released_request_with_transcript(&mut hub, &state, &transcript, Some("toolu_pending"));
            // The operator denies in the pane: Claude fires no hook, and writes this line.
            std::fs::write(
                &transcript,
                r#"{"type":"user","message":{"role":"user","content":[{"type":"tool_result","content":"The user doesn't want to proceed with this tool use.","is_error":true,"tool_use_id":"toolu_pending"}]}}
"#,
            )
            .expect("transcript");
            hook_only_sweep(&mut hub, &state);
            assert_eq!(
                hub.count(),
                1,
                "the hook-only sweep never learns of the deny"
            );
            hub.drop_answered_released();
            assert_eq!(
                hub.count(),
                0,
                "handled in the pane, still listed in NEEDS YOU"
            );
            assert!(!crate::commands::ctx::attention::prompt_open(
                &state, "abc123"
            ));
            assert_eq!(
                crate::commands::ctx::attention::load(&state, "abc123").attention,
                crate::commands::ctx::attention::Attention::None
            );
        }

        #[test]
        fn a_released_request_with_no_tool_result_yet_stays_listed() {
            let (tmp, mut hub) = live_hub();
            let state =
                StateDir::resolve(&|_| Some(tmp.path().display().to_string())).expect("state");
            let transcript = tmp.path().join("session.jsonl");
            std::fs::write(
                &transcript,
                r#"{"type":"user","message":{"role":"user","content":[{"type":"tool_result","content":"ok","tool_use_id":"toolu_other"}]}}
"#,
            )
            .expect("transcript");
            released_request_with_transcript(&mut hub, &state, &transcript, Some("toolu_pending"));
            hub.drop_answered_released();
            assert_eq!(hub.count(), 1);
            // An unreadable transcript leaves it alone too.
            std::fs::remove_file(&transcript).expect("remove");
            hub.drop_answered_released();
            assert_eq!(hub.count(), 1);
        }

        #[test]
        fn a_released_request_without_a_recorded_tool_use_id_is_matched_in_the_transcript() {
            let (tmp, mut hub) = live_hub();
            let state =
                StateDir::resolve(&|_| Some(tmp.path().display().to_string())).expect("state");
            let transcript = tmp.path().join("session.jsonl");
            // An older run of the same command (answered long ago) must not be taken for the pending call.
            std::fs::write(
                &transcript,
                r#"{"timestamp":"2020-01-01T00:00:00.000Z","message":{"content":[{"type":"tool_use","id":"toolu_pending","name":"Bash","input":{"command":"cargo test"}}]}}
{"timestamp":"2020-01-01T00:00:05.000Z","message":{"content":[{"type":"tool_result","is_error":true,"tool_use_id":"toolu_pending","content":"The user doesn't want to proceed with this tool use."}]}}
"#,
            )
            .expect("transcript");
            released_request_with_transcript(&mut hub, &state, &transcript, None);
            assert!(hub.items()[0].request.tool_use_id.is_none());
            hub.drop_answered_released();
            assert_eq!(hub.count(), 0, "the sweep must find the call itself");
        }

        #[test]
        fn a_record_cleared_by_posttool_drops_the_pending_item() {
            let (tmp, mut hub) = live_hub();
            let state =
                StateDir::resolve(&|_| Some(tmp.path().display().to_string())).expect("state");
            let sock = socket_path(&state, std::process::id());
            let req = request("abc123");
            write_record(&state, &req).expect("record");
            let _client = {
                let mut c = UnixStream::connect(&sock).expect("connect");
                c.write_all(format!("{}\n", serde_json::to_string(&req).expect("json")).as_bytes())
                    .expect("send");
                c
            };
            wait_for(&mut hub, 1);
            clear_for_tool(&state, "abc123", "Bash", "cargo test", "cargo test");
            hub.poll(&|_| true);
            assert_eq!(hub.count(), 0);
        }

        /// Issue #854: two hooks that ask at the same moment are both held and both counted.
        fn simultaneous_requests(shorts: [&str; 2]) {
            let (tmp, mut hub) = live_hub();
            let state =
                StateDir::resolve(&|_| Some(tmp.path().display().to_string())).expect("state");
            let sock = socket_path(&state, std::process::id());
            let barrier = Arc::new(std::sync::Barrier::new(2));
            let waiters: Vec<_> = shorts
                .iter()
                .enumerate()
                .map(|(n, short)| {
                    let mut req = Request::new(
                        short,
                        "Bash",
                        &format!("cargo test {n}"),
                        &format!("cargo test {n}"),
                        std::process::id(),
                    );
                    req.nonce = n as u32 + 1;
                    write_record(&state, &req).expect("record");
                    let (sock, barrier) = (sock.clone(), Arc::clone(&barrier));
                    std::thread::spawn(move || {
                        barrier.wait();
                        hold(&sock, std::process::id(), &req, Duration::from_secs(5))
                    })
                })
                .collect();
            wait_for(&mut hub, 2);
            for _ in 0..5 {
                std::thread::sleep(Duration::from_millis(100));
                hub.poll(&|_| true);
                assert_eq!(hub.count(), 2, "both requests stay pending");
            }
            // Resolving one leaves the other pending and counted.
            assert_eq!(hub.resolve_current(Decision::Allow), Resolved::Sent);
            hub.poll(&|_| true);
            assert_eq!(hub.count(), 1);
            assert_eq!(hub.resolve_current(Decision::Deny), Resolved::Sent);
            for waiter in waiters {
                assert!(waiter.join().expect("hook").is_some());
            }
            assert_eq!(hub.count(), 0);
        }

        #[test]
        fn two_simultaneous_requests_from_one_session_are_both_pending() {
            simultaneous_requests(["abc123", "abc123"]);
        }

        #[test]
        fn two_simultaneous_requests_from_different_sessions_are_both_pending() {
            simultaneous_requests(["abc123", "def456"]);
        }
    }
}
