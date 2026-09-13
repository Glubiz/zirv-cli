//! The runtime service's terminals (issue #352).
//!
//! This is the module that owns what the dashboard used to own: the
//! pty/ConPTY pair, the child process, the `vt100::Parser` that turns its
//! bytes into a screen, and the attachment bookkeeping that decides who may
//! type into it. It deliberately reuses the primitives `dash::pane` and
//! `wrap` already established -- `portable_pty::native_pty_system`,
//! `sessions::scrub_supervision_env`, `wrap::answer_inherit_cursor_probe`,
//! `supervise::ChildGuard`, `wrap::quit_child` -- rather than growing a
//! second spawn path: a pty zirv owns is a pty zirv already knows how to
//! spawn, supervise and terminate, and the new thing here is only WHO holds
//! it.
//!
//! Three rules shape the whole file:
//!
//! - **A client is not a lifetime.** Attaching, detaching, crashing and
//!   reconnecting all move entries in `clients`/`controller` and touch
//!   nothing else. The only thing that ends a session is [`RuntimeHost::stop`],
//!   reached only from `session.stop`.
//! - **One controller, many observers.** The seat is granted when free,
//!   refused with `busy` when taken, and moved only by an explicit
//!   `takeover`. Every change is announced by the protocol server, not here:
//!   this module reports state, it does not emit events.
//! - **The screen lives in memory.** Reattachment repaints from the live
//!   `vt100::Parser`, so the original process never restarts and no terminal
//!   output has to be written to disk. Tier 3 (`[session] history`) is the
//!   only thing that would, which is why it is off by default and warned
//!   about where it is turned on.

use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, TryRecvError};
use std::sync::{Arc, Mutex, mpsc};
use std::time::Duration;

use portable_pty::{CommandBuilder, PtySize, native_pty_system};
use serde::{Deserialize, Serialize};

use super::super::CtxResult;
use super::super::api::server::SessionHost;
use super::super::api::wire::{
    ApiError, AttachMode, AttachRole, Attachment, ErrorCode, ScreenView, SessionFacts, SessionState,
};
use super::super::prompt::PromptRole;
use super::super::runtime::{RuntimeKind, SessionSpec, UiSurface};
use super::super::state::{self, StateDir};
use super::super::{adapters, priority, sessions, signal, supervise, wrap};

/// How long a stopped child gets to exit politely before the ladder
/// escalates. The same 3s `dash::pane` uses for its own quit.
const QUIT_GRACE: Duration = Duration::from_secs(3);

/// The most bytes one pump pass feeds a single session's parser before it
/// yields, so one noisy session cannot starve the others. Mirrors
/// `dash::pane`'s own per-tick budget discipline.
const PUMP_BUDGET_BYTES: usize = 512 * 1024;

/// What a caller must supply to have the runtime own a session's terminal.
/// `argv` is already resolved (adapter, model flags, prompt, sandbox flags):
/// building it is the launch path's job, and duplicating that here would be
/// a second place for a launch to drift.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpawnSpec {
    pub session_id: String,
    pub agent: String,
    pub role: String,
    pub cwd: PathBuf,
    /// The repository the registry record is filed against. Usually `cwd`;
    /// separate because `sessions::Record` keys `repo_slug` off it and a
    /// worktree launch can legitimately differ.
    pub repo: PathBuf,
    /// The registry verb this session is recorded under, so `zirv ctx status`
    /// tells a runtime-owned chat seat apart from a dashboard pane exactly as
    /// it already tells `wrap` from `dash`.
    pub verb: sessions::Verb,
    pub argv: Vec<String>,
    pub env: Vec<(String, String)>,
    pub rows: u16,
    pub cols: u16,
    /// The harness's own conversation reference, when the launch pinned one.
    /// This -- not the zirv session id -- is what a tier-2 restore resumes.
    pub conversation: Option<String>,
    /// Set on a restore: the session id of the predecessor this one continues
    /// from. Recorded, never reused as an identity.
    pub restored_from: Option<String>,
}

/// One entry of the durable topology: enough to put a session back in the
/// same place, and an honest record of whether it can be RESUMED or merely
/// relaunched.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct TopologyEntry {
    pub session_id: String,
    pub short: String,
    pub agent: String,
    pub role: String,
    pub cwd: String,
    pub rows: u16,
    pub cols: u16,
    /// The harness conversation to resume. `None` means there is nothing
    /// verified to resume -- see [`TopologyEntry::is_resumable`].
    #[serde(default)]
    pub conversation: Option<String>,
    /// The instance of the service that owned this session. A restore under
    /// a different instance is a NEW session continuing an old conversation,
    /// never the same session id revived.
    #[serde(default)]
    pub instance: String,
}

impl TopologyEntry {
    /// Issue #352's tier-2 honesty rule in one predicate: a session is
    /// resumable only when there is a verified native conversation reference
    /// to hand the harness. Everything else -- an arbitrary process the
    /// operator happened to run under a pty, a harness with no resume flag --
    /// is topology that can be RECREATED, never a process that survived, and
    /// nothing in zirv may claim otherwise.
    pub fn is_resumable(&self) -> bool {
        self.conversation
            .as_ref()
            .is_some_and(|reference| !reference.trim().is_empty())
    }
}

/// The whole durable topology of one namespace.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Topology {
    pub written: u64,
    pub instance: String,
    pub sessions: Vec<TopologyEntry>,
}

pub fn topology_path(state: &StateDir, namespace: &str) -> PathBuf {
    super::namespace::runtime_dir(state).join(format!(
        "{}-topology.json",
        state::provider_slug(namespace)
    ))
}

pub fn write_topology(state: &StateDir, namespace: &str, topology: &Topology) -> CtxResult<()> {
    state::create_private_dir_all(&super::namespace::runtime_dir(state))?;
    let body = serde_json::to_string_pretty(topology)?;
    state::write_private(&topology_path(state, namespace), &body)?;
    Ok(())
}

pub fn read_topology(state: &StateDir, namespace: &str) -> Option<Topology> {
    let body = std::fs::read_to_string(topology_path(state, namespace)).ok()?;
    serde_json::from_str(&body).ok()
}

/// Splits a stored topology into what may be RESUMED and what may only be
/// reported. Pure, so the honesty rule is testable without a pty: the caller
/// relaunches the first list and tells the operator about the second rather
/// than silently respawning agents onto conversations they never had.
pub fn partition_resumable(topology: &Topology) -> (Vec<TopologyEntry>, Vec<TopologyEntry>) {
    topology
        .sessions
        .iter()
        .cloned()
        .partition(TopologyEntry::is_resumable)
}

/// One server-owned terminal.
struct HostSession {
    id: String,
    short: String,
    agent: String,
    role: String,
    cwd: PathBuf,
    conversation: Option<String>,
    restored_from: Option<String>,
    started_at: u64,
    parser: vt100::Parser,
    master: Box<dyn portable_pty::MasterPty + Send>,
    writer: Box<dyn Write + Send>,
    output: Receiver<Vec<u8>>,
    child: Box<dyn portable_pty::Child + Send + Sync>,
    lifecycle: supervise::ChildGuard,
    /// The registry record this session is filed under. Held by the SERVICE,
    /// not by any client, which is the mechanical reason pacing, budgets, rot
    /// scoring, mail addressing and writer permits keep seeing a session
    /// nobody is watching: every one of them reads the registry, and the
    /// registry entry outlives every attachment.
    guard: sessions::SessionGuard,
    /// The turn-signal endpoint the harness's own hook posts to. Bound here
    /// for the same reason `dash::pane::Pane::spawn` binds one: without it a
    /// session is `unreachable()` in the registry and no turn boundary is
    /// ever observed.
    signal: Option<signal::SignalServer>,
    /// When the last turn signal arrived. Read by `facts` so a detached
    /// session still reports Working/Idle honestly.
    last_signal_at: Option<u64>,
    /// Whether a turn is currently in flight, from the last signal.
    working: bool,
    /// Completed turns, for `stamp_in_flight`'s turn number -- the same
    /// counter `wrap`'s pump loop keeps.
    turns: u64,
    /// Every attached client id, sorted. A client appears here exactly once
    /// regardless of how many times it re-attaches, so a crashed-and-
    /// reconnected client resumes its place rather than accumulating ghosts.
    clients: Vec<String>,
    controller: Option<String>,
    rows: u16,
    cols: u16,
    ended: bool,
}

impl HostSession {
    fn attachment_for(&self, caller: &str) -> Attachment {
        Attachment {
            controller: self.controller.clone(),
            clients: self.clients.clone(),
            rows: self.rows,
            cols: self.cols,
            role: if self.controller.as_deref() == Some(caller) {
                AttachRole::Controller
            } else if self.clients.iter().any(|id| id == caller) {
                AttachRole::Observer
            } else {
                AttachRole::Detached
            },
        }
    }

    fn facts(&self) -> SessionFacts {
        SessionFacts {
            session_id: self.id.clone(),
            short: self.short.clone(),
            runtime: RuntimeKind::Harness,
            generation: 1,
            // The surface reflects whether a CLIENT is looking, and nothing
            // else about the session: detaching the last one leaves a
            // perfectly healthy headless session, which is the entire point
            // of the feature.
            surface: if self.clients.is_empty() {
                UiSurface::Headless
            } else {
                UiSurface::Terminal
            },
            state: match (self.ended, self.working) {
                (true, _) => SessionState::Ended,
                (false, true) => SessionState::Working,
                (false, false) => SessionState::Idle,
            },
            role: Some(self.role.clone()),
            agent: Some(self.agent.clone()),
            repo_slug: Some(state::repo_slug(&self.cwd)),
            started_at: Some(self.started_at),
            reachable: !self.ended,
        }
    }

    fn topology_entry(&self, instance: &str) -> TopologyEntry {
        TopologyEntry {
            session_id: self.id.clone(),
            short: self.short.clone(),
            agent: self.agent.clone(),
            role: self.role.clone(),
            cwd: self.cwd.to_string_lossy().into_owned(),
            rows: self.rows,
            cols: self.cols,
            conversation: self.conversation.clone(),
            instance: instance.to_string(),
        }
    }

    /// Feeds queued pty bytes into the parser. Bounded per pass, and a closed
    /// channel marks the session ended rather than spinning on it.
    fn pump(&mut self) {
        let mut spent = 0usize;
        while spent < PUMP_BUDGET_BYTES {
            match self.output.try_recv() {
                Ok(chunk) => {
                    spent += chunk.len();
                    self.parser.process(&chunk);
                }
                Err(TryRecvError::Empty) => return,
                Err(TryRecvError::Disconnected) => {
                    self.ended = true;
                    return;
                }
            }
        }
    }

    /// Drains the harness's own turn signals. The same edge `wrap`'s pump
    /// loop and `dash::pane::on_turn_signal` observe, taken here so a session
    /// with NO client attached still closes its turns: the in-flight witness
    /// is cleared, the rot verdict the signal carries is recorded by the
    /// hook's own path, and `zirv ctx status` stops showing a turn that ended
    /// while nobody was watching.
    fn drain_signals(&mut self) {
        let Some(server) = &self.signal else {
            return;
        };
        let mut seen = false;
        while server.try_recv().is_some() {
            seen = true;
            self.turns += 1;
        }
        if seen {
            self.last_signal_at = Some(state::now_secs());
            self.working = false;
            self.guard.clear_in_flight();
        }
    }

    fn screen_view(&self) -> ScreenView {
        let screen = self.parser.screen();
        let (cursor_row, cursor_col) = screen.cursor_position();
        ScreenView {
            rows: self.rows,
            cols: self.cols,
            cursor_row,
            cursor_col,
            cursor_visible: !screen.hide_cursor(),
            alternate: screen.alternate_screen(),
            contents: String::from_utf8_lossy(&screen.contents_formatted()).into_owned(),
        }
    }
}

/// The runtime service's session table. Shared by every connection thread,
/// so every method takes `&self` and locks for the shortest possible span --
/// never across a blocking pty write.
#[derive(Debug)]
pub struct RuntimeHost {
    state: StateDir,
    namespace: String,
    /// The owning service's instance identity. Stamped into the topology so a
    /// successor can see whose sessions it is looking at.
    instance: String,
    scrollback_rows: usize,
    /// Tier 3. Off by default; see [`RuntimeHost::history_warning`].
    history: bool,
    sessions: Mutex<BTreeMap<String, HostSession>>,
}

impl std::fmt::Debug for HostSession {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HostSession")
            .field("id", &self.id)
            .field("agent", &self.agent)
            .field("clients", &self.clients)
            .field("controller", &self.controller)
            .field("ended", &self.ended)
            .finish()
    }
}

/// The one-line warning an operator sees whenever tier-3 history is enabled.
/// A constant rather than an inline string so `zirv session serve`, `zirv
/// session list` and the design note cannot word it differently.
pub const HISTORY_WARNING: &str =
    "[session] history = true: this runtime writes each session's rendered terminal output to \
     disk, including anything an agent printed -- API keys, tokens, file contents. It is off by \
     default for that reason; turn it off again with `ZIRV_CTX_SESSION_HISTORY=false`.";

impl RuntimeHost {
    pub fn new(
        state: StateDir,
        namespace: &str,
        instance: &str,
        scrollback_rows: usize,
        history: bool,
    ) -> Arc<Self> {
        Arc::new(Self {
            state,
            namespace: namespace.to_string(),
            instance: instance.to_string(),
            scrollback_rows,
            history,
            sessions: Mutex::new(BTreeMap::new()),
        })
    }

    /// `Some(warning)` exactly when tier-3 history is on, so no caller has to
    /// remember to check the flag before printing it.
    pub fn history_warning(&self) -> Option<&'static str> {
        self.history.then_some(HISTORY_WARNING)
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, BTreeMap<String, HostSession>> {
        // Same reasoning as `ApiServer::lock`: the state behind this mutex is
        // a session table, not a half-written invariant, and refusing every
        // later call would turn one panic into a dead runtime.
        match self.sessions.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        }
    }

    /// Drains every session's pty output into its parser. Called on a timer
    /// by the service, so a session keeps rendering with no client attached
    /// at all -- which is what makes notifications, rot scoring and attention
    /// keep working while nobody is watching.
    pub fn pump(&self) {
        for session in self.lock().values_mut() {
            session.pump();
            session.drain_signals();
            // Cheap and exact: the child's own exit is the authority on
            // whether the session ended, not the output channel alone.
            if !session.ended && matches!(session.child.try_wait(), Ok(Some(_))) {
                session.ended = true;
            }
        }
    }

    /// Spawns a session whose terminal this runtime owns.
    pub fn spawn(&self, spec: SpawnSpec) -> CtxResult<String> {
        let (program, rest) = spec
            .argv
            .split_first()
            .ok_or("zirv session: empty argv, nothing to spawn")?;
        // The same cmd.exe argv-reparse guard `dash::pane::spawn` applies:
        // this is a second `CommandBuilder` assembled outside
        // `supervise::spawn_tapped`, so it needs the policy explicitly.
        super::super::adapters::guard_cmd_shim_reparse(program, rest)?;

        let pair = native_pty_system().openpty(PtySize {
            rows: spec.rows,
            cols: spec.cols,
            pixel_width: 0,
            pixel_height: 0,
        })?;

        let mut command = CommandBuilder::new(program);
        for arg in rest {
            command.arg(arg);
        }
        command.cwd(&spec.cwd);
        sessions::scrub_supervision_env(&mut command);
        for (key, value) in &spec.env {
            command.env(key, value);
        }

        // Taken and answered before the spawn: on Windows the console host
        // has to be answered before it will service the child at all.
        let mut writer = pair.master.take_writer()?;
        wrap::answer_inherit_cursor_probe(&mut *writer);

        let child = pair.slave.spawn_command(command)?;
        let lifecycle = supervise::ChildGuard::adopt(child.process_id());
        if let Some(pid) = child.process_id() {
            priority::apply_to_child(
                pid,
                priority::posture_for(
                    PromptRole::from_label(&spec.role).unwrap_or(PromptRole::Worker),
                ),
            );
        }
        drop(pair.slave);
        let master = pair.master;

        let mut reader = master.try_clone_reader()?;
        let (tx, rx) = mpsc::channel::<Vec<u8>>();
        std::thread::spawn(move || {
            let mut buf = [0u8; 8192];
            loop {
                match reader.read(&mut buf) {
                    Ok(0) | Err(_) => return,
                    Ok(n) => {
                        if tx.send(buf[..n].to_vec()).is_err() {
                            return;
                        }
                    }
                }
            }
        });

        // The registry half, lifted from `dash::pane::Pane::spawn` rather
        // than reinvented: the same socket, the same published path, the same
        // record with the CHILD's pid and start time (not the supervisor's,
        // or every liveness probe compares the wrong process). The difference
        // that matters for issue #352 is only whose process holds the guard --
        // the service's, so detaching every client leaves the registry entry,
        // and therefore pacing, budgets, rot, mail and permits, exactly where
        // they were.
        let signal = signal::SignalServer::bind(&self.state.socket_for(&spec.session_id)).ok();
        if let Some(server) = &signal {
            wrap::publish_socket_path(&self.state, &spec.session_id, server.path());
        }
        let mut record =
            sessions::Record::new(&spec.session_id, &spec.agent, &spec.repo, spec.verb)
                .with_role(&spec.role);
        if let Some(child_pid) = child.process_id() {
            record.pid = child_pid;
            record.start_time = sessions::process_start_secs(child_pid);
        }
        let record = if signal.is_some() {
            record
        } else {
            record.unreachable()
        };
        let guard = sessions::SessionGuard::register(&self.state, record);

        let session = HostSession {
            short: sessions::short_id(&spec.session_id),
            id: spec.session_id.clone(),
            agent: spec.agent,
            role: spec.role,
            cwd: spec.cwd,
            conversation: spec.conversation,
            restored_from: spec.restored_from,
            started_at: state::now_secs(),
            parser: vt100::Parser::new(spec.rows, spec.cols, self.scrollback_rows),
            master,
            writer,
            output: rx,
            child,
            lifecycle,
            guard,
            signal,
            last_signal_at: None,
            working: false,
            turns: 0,
            clients: Vec::new(),
            controller: None,
            rows: spec.rows,
            cols: spec.cols,
            ended: false,
        };
        self.lock().insert(spec.session_id.clone(), session);
        self.persist_topology();
        Ok(spec.session_id)
    }

    /// The session this runtime restored `session_id` from, if any -- the
    /// only place a predecessor id is ever read back, and never as an
    /// identity.
    pub fn restored_from(&self, session_id: &str) -> Option<String> {
        self.lock()
            .get(session_id)
            .and_then(|session| session.restored_from.clone())
    }

    /// Writes the durable topology. Called on every structural change (a
    /// spawn, a stop) and at shutdown, so a crash loses at most the sessions
    /// started since the last one.
    pub fn persist_topology(&self) {
        let sessions = self.lock();
        let topology = Topology {
            written: state::now_secs(),
            instance: self.instance.clone(),
            sessions: sessions
                .values()
                .filter(|session| !session.ended)
                .map(|session| session.topology_entry(&self.instance))
                .collect(),
        };
        drop(sessions);
        let _ = write_topology(&self.state, &self.namespace, &topology);
    }

    /// The operator's explicit shutdown: every session is drained to the
    /// topology first, then only the ones named are put through the
    /// termination ladder. `stop_all = false` is the ordinary case -- the
    /// service exits, the agents do not.
    pub fn shutdown(&self, stop_all: bool) {
        self.persist_topology();
        if !stop_all {
            return;
        }
        let ids: Vec<String> = self.lock().keys().cloned().collect();
        for id in ids {
            let _ = self.stop(&id);
        }
    }

    fn locked<T>(
        &self,
        session_id: &str,
        call: impl FnOnce(&mut HostSession) -> Result<T, ApiError>,
    ) -> Result<T, ApiError> {
        let mut sessions = self.lock();
        let Some(session) = sessions.get_mut(session_id) else {
            return Err(ApiError::new(
                ErrorCode::UnknownSession,
                format!("no session {session_id} on this runtime"),
            ));
        };
        call(session)
    }
}

impl SessionHost for RuntimeHost {
    fn sessions(&self) -> Vec<SessionFacts> {
        self.lock().values().map(HostSession::facts).collect()
    }

    fn start(&self, spec: &SessionSpec) -> Result<SessionFacts, ApiError> {
        // A fresh id, always. Even a restore mints one (see
        // `session::namespace`'s module doc): the identity a previous service
        // published is never re-published, only recorded as the conversation
        // this new session continues.
        let session_id = uuid::Uuid::new_v4().to_string();
        let spawn = launch_spec(spec, &session_id, &self.state)
            .map_err(|error| ApiError::new(ErrorCode::InvalidParams, error.to_string()))?;
        self.spawn(spawn)
            .map_err(|error| ApiError::new(ErrorCode::Internal, error.to_string()))?;
        self.lock()
            .get(&session_id)
            .map(HostSession::facts)
            .ok_or_else(|| {
                ApiError::new(ErrorCode::Internal, "the session vanished as it was opened")
            })
    }

    fn attach(
        &self,
        session_id: &str,
        client_id: &str,
        mode: AttachMode,
        size: Option<(u16, u16)>,
    ) -> Result<Attachment, ApiError> {
        self.locked(session_id, |session| {
            if !session.clients.iter().any(|id| id == client_id) {
                session.clients.push(client_id.to_string());
                session.clients.sort();
            }
            if mode == AttachMode::Controller {
                match session.controller.clone() {
                    Some(current) if current != client_id => {
                        // Refused, never silently taken: the operator typing
                        // into this session somewhere else must not lose the
                        // keyboard because another client connected.
                        return Err(ApiError::new(
                            ErrorCode::Busy,
                            format!(
                                "{current} already controls this session; `session.takeover` \
                                 takes the seat explicitly"
                            ),
                        ));
                    }
                    _ => session.controller = Some(client_id.to_string()),
                }
                if let Some((rows, cols)) = size {
                    resize_session(session, rows, cols);
                }
            }
            Ok(session.attachment_for(client_id))
        })
    }

    fn detach(&self, session_id: &str, client_id: &str) -> Result<Attachment, ApiError> {
        self.locked(session_id, |session| {
            session.clients.retain(|id| id != client_id);
            if session.controller.as_deref() == Some(client_id) {
                session.controller = None;
            }
            // Nothing else. The child, the pty, the parser and the supervisor
            // are untouched, which is the acceptance criterion this method
            // exists to satisfy.
            Ok(session.attachment_for(client_id))
        })
    }

    fn takeover(&self, session_id: &str, client_id: &str) -> Result<Attachment, ApiError> {
        self.locked(session_id, |session| {
            if !session.clients.iter().any(|id| id == client_id) {
                session.clients.push(client_id.to_string());
                session.clients.sort();
            }
            session.controller = Some(client_id.to_string());
            Ok(session.attachment_for(client_id))
        })
    }

    fn resize(
        &self,
        session_id: &str,
        client_id: &str,
        rows: u16,
        cols: u16,
    ) -> Result<Attachment, ApiError> {
        self.locked(session_id, |session| {
            if session.controller.as_deref() != Some(client_id) {
                return Err(ApiError::new(
                    ErrorCode::Denied,
                    "only the controller resizes a session's terminal; an observer renders a \
                     clipped view of it instead",
                ));
            }
            resize_session(session, rows, cols);
            Ok(session.attachment_for(client_id))
        })
    }

    fn screen(&self, session_id: &str, client_id: &str) -> Result<ScreenView, ApiError> {
        self.locked(session_id, |session| {
            if !session.clients.iter().any(|id| id == client_id) {
                return Err(ApiError::new(
                    ErrorCode::Denied,
                    "attach to this session before reading its screen",
                ));
            }
            // Drained here as well as on the timer, so a client that asks
            // immediately after typing sees the result of its own keystroke
            // rather than the frame before it.
            session.pump();
            Ok(session.screen_view())
        })
    }

    fn write_raw(&self, session_id: &str, client_id: &str, bytes: &[u8]) -> Result<(), ApiError> {
        self.locked(session_id, |session| {
            if session.controller.as_deref() != Some(client_id) {
                return Err(ApiError::new(
                    ErrorCode::Denied,
                    "only the session's controller may type into it",
                ));
            }
            if session.ended {
                return Err(ApiError::new(
                    ErrorCode::UnknownSession,
                    "this session has ended",
                ));
            }
            session
                .writer
                .write_all(bytes)
                .and_then(|()| session.writer.flush())
                .map_err(|error| ApiError::new(ErrorCode::Internal, error.to_string()))?;
            // Issue #281's edge, unchanged: the operator's own keystroke is
            // what reliably starts a turn. Stamped here so a crash of the
            // SERVICE (not of a client) still leaves the in-flight witness a
            // recovering supervisor reads.
            if !session.working {
                session.working = true;
                let verb = session.guard.record().verb.as_str().to_string();
                session.guard.stamp_in_flight(&verb, session.turns + 1);
            }
            Ok(())
        })
    }

    fn stop(&self, session_id: &str) -> Result<bool, ApiError> {
        let stopped = self.locked(session_id, |session| {
            if session.ended {
                return Ok(false);
            }
            // The EXISTING ladder, unchanged: the harness's own quit sequence
            // first, then escalation. Reached only from `session.stop`, i.e.
            // only for a session the operator chose to stop.
            let quit = adapter_by_name(&session.agent)
                .map(|adapter| adapter.quit_sequence().to_string())
                .unwrap_or_default();
            let quit = quit.as_str();
            let sink: &mut dyn Write = &mut *session.writer;
            let _ = wrap::quit_child(sink, &mut session.child, quit, QUIT_GRACE);
            session.ended = true;
            session.working = false;
            session.lifecycle.release();
            // The registry entry goes with the process it described -- but
            // only here, on the operator's explicit stop. `detach` does not
            // reach this, and neither does the service's own shutdown unless
            // the operator asked for `--stop-sessions`.
            session.guard.release();
            wrap::unpublish_socket_path(&self.state, &session.id);
            session.signal = None;
            session.clients.clear();
            session.controller = None;
            Ok(true)
        })?;
        if stopped {
            self.persist_topology();
        }
        Ok(stopped)
    }
}

/// The size a runtime-owned terminal is opened at before any client has said
/// how big its window is. A controller's `session.attach` carries its real
/// size and resizes immediately (see [`SessionHost::attach`]), so this is
/// only ever the geometry of the first few milliseconds -- but it has to be a
/// usable one, because a session nobody ever attaches to still has to render.
pub const DEFAULT_ROWS: u16 = 24;
pub const DEFAULT_COLS: u16 = 80;

/// Turns a protocol [`SessionSpec`] into a launch this runtime is willing to
/// make.
///
/// Everything here is the EXISTING chat launch path -- `chat::
/// resolve_adapter`, `chat::build_launch`, `chat::dash_orchestrator_pane`
/// (context compilation, prompt injection, the sandbox posture, the
/// conversation pin) and `dash::build_turn_env` -- called in the order the
/// dashboard already calls it. Issue #352 changes WHO holds the resulting
/// pty, not how a session is composed, so composing one differently here
/// would be a second launch path to keep in step with the first.
///
/// The conversation reference this returns is the one tier 2 later resumes,
/// and it is deliberately derived from the adapter's own verified pin flag:
/// an adapter with no `session_pin_args` yields `None`, which is what makes
/// `TopologyEntry::is_resumable` answer honestly rather than optimistically.
pub fn launch_spec(spec: &SessionSpec, session_id: &str, state: &StateDir) -> CtxResult<SpawnSpec> {
    let role = if spec.role.trim().is_empty() {
        PromptRole::Orchestrator
    } else {
        PromptRole::from_label(&spec.role)
            .ok_or_else(|| format!("unknown session role '{}'", spec.role))?
    };
    if role != PromptRole::Orchestrator {
        // Honest refusal rather than a silently different session: a worker
        // pane carries a task prompt, a work group, a budget and a report
        // address that the dashboard assembles (`dash::fulfill_spawn_request`).
        // Hosting those in the runtime is step N20 (#489), not this change.
        return Err(format!(
            "the persistent runtime opens orchestrator seats; '{}' sessions stay with the \
             dashboard until #489",
            spec.role
        )
        .into());
    }
    let repo = spec.cwd.clone();
    let cfg = super::super::config::CtxConfig::load_for_launch(
        &repo,
        &super::super::config::env_from_process(),
    )?;
    let (adapter, _rule) = super::super::chat::resolve_adapter(&cfg, spec.agent.as_deref())?;
    let prompt = spec.prompt.trim();
    let launch = super::super::chat::build_launch(
        adapter.as_ref(),
        (!prompt.is_empty()).then_some(prompt),
        &spec.extra_args,
    );
    let pane = super::super::chat::dash_orchestrator_pane(
        adapter.as_ref(),
        launch,
        &cfg,
        state,
        &repo,
        session_id,
        false,
    )?;
    let (env, _turn_env_error) = super::super::dash::build_turn_env(
        &cfg,
        state,
        &repo,
        &pane.agent_name,
        session_id,
        adapters::LaunchMode::Interactive,
    );
    let conversation = (!adapter.session_pin_args(session_id).is_empty())
        .then(|| session_id.to_string());
    Ok(SpawnSpec {
        session_id: session_id.to_string(),
        agent: pane.agent_name,
        role: pane.role.label().to_string(),
        cwd: repo.clone(),
        repo,
        verb: pane.verb,
        argv: pane.argv,
        env,
        rows: DEFAULT_ROWS,
        cols: DEFAULT_COLS,
        conversation,
        restored_from: None,
    })
}

/// Resizes both the pty and the parser together, so the two can never
/// disagree about the terminal's shape (the bug class `dash::pane::resize`
/// guards against for the same reason).
fn resize_session(session: &mut HostSession, rows: u16, cols: u16) {
    let _ = session.master.resize(PtySize {
        rows,
        cols,
        pixel_width: 0,
        pixel_height: 0,
    });
    session.parser.screen_mut().set_size(rows, cols);
    session.rows = rows;
    session.cols = cols;
}

/// The adapter registered under `agent`, by name alone. `adapters::all`'s
/// `bin` override only changes where a harness binary is found, and nothing
/// here launches one -- the quit sequence and the resume flags are static
/// facts of the adapter.
fn adapter_by_name(agent: &str) -> Option<Box<dyn super::super::adapters::AgentAdapter>> {
    super::super::adapters::all(None)
        .into_iter()
        .find(|adapter| adapter.name() == agent)
}

/// A verified-resume argv for one topology entry, or `None` when the harness
/// has no verified resume mechanism for it. The adapter's own `resume_args`
/// is the authority -- the same one `dash::roster::restore_argv` uses -- so
/// "resumable" means exactly what it already means everywhere else in zirv.
pub fn resume_argv(entry: &TopologyEntry) -> Option<Vec<String>> {
    let conversation = entry.conversation.as_deref()?;
    let adapter = adapter_by_name(&entry.agent)?;
    let resume = adapter.resume_args(conversation)?;
    if resume.is_empty() {
        return None;
    }
    Some(super::super::dash::flatten_command(
        adapter.interactive_cmd(None, &resume),
    ))
}

/// Where a restored session's working directory comes from. Kept separate so
/// a topology entry naming a directory that no longer exists degrades to the
/// operator's current one with a visible note rather than failing the whole
/// restore.
pub fn restore_cwd(entry: &TopologyEntry, fallback: &Path) -> PathBuf {
    let recorded = PathBuf::from(&entry.cwd);
    if recorded.is_dir() {
        recorded
    } else {
        fallback.to_path_buf()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(agent: &str, conversation: Option<&str>) -> TopologyEntry {
        TopologyEntry {
            session_id: "11111111-2222-4333-8444-555555555555".to_string(),
            short: "11111111".to_string(),
            agent: agent.to_string(),
            role: "orchestrator".to_string(),
            cwd: "/work/repo".to_string(),
            rows: 24,
            cols: 80,
            conversation: conversation.map(str::to_string),
            instance: "inst-1".to_string(),
        }
    }

    /// Tier 2's honesty rule, as a predicate: only a session with a verified
    /// conversation reference may be claimed as resumable. An arbitrary
    /// process gets no such claim, which is what keeps "restore topology"
    /// from being sold as "the process survived".
    #[test]
    fn only_a_session_with_a_verified_conversation_is_resumable() {
        assert!(entry("claude", Some("conv-1")).is_resumable());
        assert!(!entry("claude", None).is_resumable());
        assert!(
            !entry("claude", Some("   ")).is_resumable(),
            "a blank reference is not a reference"
        );
    }

    #[test]
    fn partition_resumable_separates_what_may_be_relaunched_from_what_may_not() {
        let topology = Topology {
            written: 10,
            instance: "inst-1".to_string(),
            sessions: vec![
                entry("claude", Some("conv-1")),
                entry("bash", None),
                entry("codex", Some("conv-2")),
            ],
        };
        let (resumable, reportable) = partition_resumable(&topology);
        assert_eq!(resumable.len(), 2);
        assert_eq!(reportable.len(), 1);
        assert_eq!(reportable[0].agent, "bash");
    }

    /// The resume argv comes from the adapter's own verified flag, and an
    /// agent with no such flag yields `None` rather than a guessed command
    /// line -- so a restore never invents a way to "resume" something.
    #[test]
    fn resume_argv_uses_the_adapters_verified_flag_and_refuses_to_guess() {
        let argv = resume_argv(&entry("claude", Some("conv-1"))).expect("claude resumes");
        assert!(argv.iter().any(|token| token == "--resume"), "{argv:?}");
        assert!(argv.iter().any(|token| token == "conv-1"), "{argv:?}");
        assert!(resume_argv(&entry("claude", None)).is_none());
        assert!(
            resume_argv(&entry("no-such-harness", Some("conv-1"))).is_none(),
            "an unknown agent has no verified resume path"
        );
    }

    #[test]
    fn a_topology_round_trips_through_the_state_directory() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().to_path_buf());
        assert!(read_topology(&state, "default").is_none());
        let topology = Topology {
            written: 10,
            instance: "inst-1".to_string(),
            sessions: vec![entry("claude", Some("conv-1"))],
        };
        write_topology(&state, "default", &topology).expect("write");
        assert_eq!(read_topology(&state, "default"), Some(topology));
    }

    /// An old topology file, written before a field existed, still parses:
    /// the same `#[serde(default)]` discipline `dash::roster` holds, and for
    /// the same reason -- a hard parse failure would silently discard the
    /// whole restore.
    #[test]
    fn a_topology_entry_from_an_older_build_loads_with_defaults() {
        let entry: TopologyEntry = serde_json::from_str(
            r#"{"session_id":"s","short":"s","agent":"claude","role":"worker",
                "cwd":"/w","rows":24,"cols":80}"#,
        )
        .expect("parse");
        assert_eq!(entry.conversation, None);
        assert!(!entry.is_resumable());
    }

    #[test]
    fn restore_cwd_falls_back_when_the_recorded_directory_is_gone() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let mut present = entry("claude", Some("c"));
        present.cwd = tmp.path().to_string_lossy().into_owned();
        assert_eq!(restore_cwd(&present, Path::new("/fallback")), tmp.path());

        let missing = entry("claude", Some("c"));
        assert_eq!(
            restore_cwd(&missing, tmp.path()),
            tmp.path(),
            "a vanished checkout must not fail the whole restore"
        );
    }

    /// Tier 3 is off unless the operator turned it on, and when it is on the
    /// warning is unconditional -- there is no path that enables history
    /// quietly.
    #[test]
    fn terminal_history_is_off_by_default_and_warns_when_it_is_not() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().to_path_buf());
        let quiet = RuntimeHost::new(state, "default", "inst-1", 2000, false);
        assert_eq!(quiet.history_warning(), None);

        let tmp2 = tempfile::tempdir().expect("tempdir");
        let state2 = StateDir::from_root(tmp2.path().to_path_buf());
        let loud = RuntimeHost::new(state2, "default", "inst-1", 2000, true);
        let warning = loud.history_warning().expect("a warning is mandatory");
        assert!(warning.contains("terminal output"), "{warning}");
        assert!(warning.contains("off by default"), "{warning}");
    }

    #[test]
    fn an_unknown_session_is_refused_by_code_rather_than_panicking() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().to_path_buf());
        let host = RuntimeHost::new(state, "default", "inst-1", 2000, false);
        let failure = host
            .attach("nope", "c1", AttachMode::Observer, None)
            .expect_err("no such session");
        assert_eq!(failure.code, ErrorCode::UnknownSession);
        assert!(host.sessions().is_empty());
    }
}
