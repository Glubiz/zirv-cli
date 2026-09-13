//! The protocol v1 reference server (issue #353).
//!
//! Issue #353 explicitly allows an in-process reference server for this
//! stage: "No runtime daemon is required to merge this issue". So this is
//! not a daemon. It is the one implementation of the v1 method set, holding
//! the SHARED RUNTIME FACTS (which sessions exist, their stable ids,
//! generations, lifecycle state and the event log) and nothing else --
//! layout, colour, sidebar selection, mouse state and modals stay in
//! whichever client is drawing them, and no method here can read or write
//! any of that.
//!
//! Two deliberate limits, both of which belong to later issues rather than
//! to a weaker version of this one:
//!
//! - PTY ownership stays in the dashboard (issue #352). A server with no
//!   [`RuntimeBackend`] attached serves every read method off the session
//!   registry and refuses the three mutating ones with
//!   [`ErrorCode::Unsupported`], naming the issue -- never a silent success.
//! - Session facts come from a [`SessionSource`], which today is either the
//!   real session registry or a fixed list. Nothing else in zirv is
//!   reachable through the protocol.

use std::collections::{BTreeMap, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex};

use serde::Deserialize;
use serde_json::{Value, json};

use super::transport::{Connection, Endpoint, Listener, server_uid};
use super::wire::{
    ADVERTISED, ADVERTISED_WITHOUT_HOST, ApiError, ApiEvent, AttachMode, Attachment, Capability,
    ErrorCode, EventFrame, Hello, InputMode, Method, Outcome, PROTOCOL_VERSION, Request, Response,
    SERVER_NAME, ScreenView, SessionFacts, SessionState, WaitUntil, spec_for,
};
use crate::commands::ctx::CtxResult;
use crate::commands::ctx::runtime::{
    RuntimeBackend, RuntimeError, RuntimeKind, SessionHandle, SessionSpec, UiSurface,
};
use crate::commands::ctx::sessions::{self, Liveness};
use crate::commands::ctx::state::StateDir;

/// How many event frames the server keeps for replay. A subscriber that
/// asks for a revision older than the oldest retained frame gets a visible
/// GAP (the first frame it receives is not `after_revision + 1`), which is
/// exactly the signal issue #353 asks for: refresh a snapshot rather than
/// drift.
const MAX_EVENTS: usize = 512;

/// How many idempotency keys the server remembers, evicted oldest-first.
const MAX_IDEMPOTENCY: usize = 256;

const DEFAULT_WAIT_MS: u64 = 30_000;
const MAX_WAIT_MS: u64 = 600_000;
const WAIT_POLL: std::time::Duration = std::time::Duration::from_millis(10);
const SUBSCRIBE_POLL: std::time::Duration = std::time::Duration::from_millis(100);

/// Where the server's session facts come from. A trait so the deterministic
/// tests and the frozen-fixture replay can seed exact facts, and so nothing
/// but the implementations in this file can decide what the protocol
/// publishes.
pub trait SessionSource: Send + Sync + std::fmt::Debug {
    fn sessions(&self) -> Vec<SessionFacts>;
}

/// The real one: the existing session registry, projected down to the
/// redacted [`SessionFacts`] shape.
#[derive(Debug)]
pub struct RegistrySource {
    state: StateDir,
}

impl RegistrySource {
    pub fn new(state: StateDir) -> Self {
        Self { state }
    }
}

impl SessionSource for RegistrySource {
    fn sessions(&self) -> Vec<SessionFacts> {
        sessions::list(&self.state)
            .iter()
            .map(|(record, liveness)| facts_from_record(record, *liveness))
            .collect()
    }
}

/// Issue #352: the seam to a runtime that OWNS the sessions' terminals.
///
/// `ApiServer` deliberately holds no pty, no child process and no vt100
/// parser of its own: it is the protocol, and the protocol must not grow a
/// second, private implementation of the thing `session::host` already does.
/// A server with no host attached is exactly the server issue #353 shipped --
/// every read method answers off the session source, and the five attachment
/// methods are refused with a structured `unsupported` that names #352.
///
/// `&self` throughout (not `&mut self`) because a host is shared by every
/// connection thread and does its own interior locking; the server's single
/// mutex must never be held across a pty write.
pub trait SessionHost: Send + Sync + std::fmt::Debug {
    /// Facts for the sessions this host owns, in the same redacted shape a
    /// registry record projects to.
    fn sessions(&self) -> Vec<SessionFacts>;
    /// Opens a NEW server-owned terminal for `spec`. Reached from
    /// `session.start` whenever a host is attached, so a client asks for a
    /// session with the same method whether a pty or a native backend ends up
    /// carrying it -- issue #352 adds no second "start" verb, because a
    /// client that had to know which kind of runtime it was talking to before
    /// it could ask for a session would not be speaking one protocol.
    ///
    /// The host, not the caller, turns the spec into a command line: the
    /// endpoint is owner-only, but "owner-only" is not a reason to accept an
    /// arbitrary argv over a socket when the only launches a runtime ever
    /// needs to make are an adapter's own.
    fn start(&self, spec: &SessionSpec) -> Result<SessionFacts, ApiError>;
    fn attach(
        &self,
        session_id: &str,
        client_id: &str,
        mode: AttachMode,
        size: Option<(u16, u16)>,
    ) -> Result<Attachment, ApiError>;
    fn detach(&self, session_id: &str, client_id: &str) -> Result<Attachment, ApiError>;
    fn takeover(&self, session_id: &str, client_id: &str) -> Result<Attachment, ApiError>;
    fn resize(
        &self,
        session_id: &str,
        client_id: &str,
        rows: u16,
        cols: u16,
    ) -> Result<Attachment, ApiError>;
    fn screen(&self, session_id: &str, client_id: &str) -> Result<ScreenView, ApiError>;
    /// The controller's literal keystrokes. Refused for anyone else.
    fn write_raw(
        &self,
        session_id: &str,
        client_id: &str,
        bytes: &[u8],
    ) -> Result<(), ApiError>;
    /// The operator's explicit `zirv session stop`: terminate the child
    /// through the existing ladder. Detaching a client never reaches this.
    fn stop(&self, session_id: &str) -> Result<bool, ApiError>;
}

/// A fixed list. The deterministic source the frozen-fixture replay and the
/// unit tests seed exact facts through; production always uses
/// [`RegistrySource`], hence the allow.
#[allow(dead_code)]
#[derive(Debug, Default)]
pub struct StaticSource(pub Vec<SessionFacts>);

impl SessionSource for StaticSource {
    fn sessions(&self) -> Vec<SessionFacts> {
        self.0.clone()
    }
}

/// The registry -> protocol projection, in one place so the redaction rule
/// is auditable: the absolute `repo` path, the transcript path, the in-flight
/// witness's own details and the owner pid are all dropped here.
///
/// `generation` is 1 for every registry record: the registry has no
/// generation of its own (the orchestrator seat does, and a persistent
/// runtime will -- issues #352/#489). Publishing a constant is honest;
/// inventing one from, say, a restart count would let a client believe a
/// pin means something it does not.
pub fn facts_from_record(record: &sessions::Record, liveness: Liveness) -> SessionFacts {
    let state = match (liveness, record.in_flight.is_some()) {
        (Liveness::Live, true) => SessionState::Working,
        (Liveness::Live, false) => SessionState::Idle,
        _ => SessionState::Ended,
    };
    SessionFacts {
        session_id: record.session.clone(),
        short: record.short.clone(),
        runtime: record.runtime,
        generation: 1,
        surface: UiSurface::Headless,
        state,
        role: record.role.clone(),
        agent: Some(record.agent.clone()),
        repo_slug: Some(record.repo_slug.clone()),
        started_at: Some(record.started_at),
        reachable: record.reachable,
    }
}

#[derive(Debug)]
struct Inner {
    revision: u64,
    sessions: BTreeMap<String, SessionFacts>,
    /// Backend handles for sessions this server itself started. A session
    /// that only came from the registry has none, so a mutation on it is
    /// refused rather than sent to a backend that never heard of it.
    handles: BTreeMap<String, SessionHandle>,
    events: VecDeque<EventFrame>,
    idempotency: BTreeMap<String, Value>,
    idempotency_order: VecDeque<String>,
    subscribers: Vec<Sender<EventFrame>>,
    /// Lifecycle states a CLIENT reported (`session.report_status`) or this
    /// server itself caused (`session.send_input`, `session.stop`). A
    /// refresh from the session source must not silently undo them: the
    /// registry projection is a coarse "is the process alive and is a turn
    /// in flight", and the client driving a session knows better. Every
    /// other field still comes from the source on every refresh.
    reported: BTreeMap<String, SessionState>,
    /// Issue #352: the controller seat this server last ANNOUNCED for each
    /// session, so `controller_changed` is emitted once per real change
    /// rather than once per attachment call.
    controllers: BTreeMap<String, Option<String>>,
}

impl Inner {
    /// The one place `revision` moves. Exactly +1 per emitted event, which
    /// is what makes `revision != last + 1` a reliable gap signal for a
    /// subscriber.
    fn emit(&mut self, session_id: Option<String>, generation: Option<u64>, payload: ApiEvent) {
        self.revision += 1;
        let frame = EventFrame {
            version: PROTOCOL_VERSION,
            revision: self.revision,
            session_id,
            generation,
            payload,
        };
        self.events.push_back(frame.clone());
        while self.events.len() > MAX_EVENTS {
            self.events.pop_front();
        }
        self.subscribers
            .retain(|subscriber| subscriber.send(frame.clone()).is_ok());
    }

    fn remember(&mut self, key: String, result: Value) {
        if self.idempotency.contains_key(&key) {
            return;
        }
        self.idempotency.insert(key.clone(), result);
        self.idempotency_order.push_back(key);
        while self.idempotency_order.len() > MAX_IDEMPOTENCY
            && let Some(oldest) = self.idempotency_order.pop_front()
        {
            self.idempotency.remove(&oldest);
        }
    }
}

/// The server itself. Shared behind an [`Arc`]: one accept loop, one thread
/// per connection, all serialised on the single mutex below. v1 is a local
/// control plane with a handful of clients, so a single lock is the right
/// amount of machinery -- every method is a map lookup or one backend call.
#[derive(Debug)]
pub struct ApiServer {
    inner: Mutex<Inner>,
    backend: Mutex<Option<Box<dyn RuntimeBackend + Send>>>,
    source: Box<dyn SessionSource>,
    /// Issue #352. `None` for every server issue #353 shipped, which is why
    /// the attachment capability is negotiated away rather than advertised
    /// and then refused.
    host: Mutex<Option<Arc<dyn SessionHost>>>,
    stopping: Arc<AtomicBool>,
    owner_uid: Option<u32>,
}

impl ApiServer {
    pub fn new(
        source: Box<dyn SessionSource>,
        backend: Option<Box<dyn RuntimeBackend + Send>>,
    ) -> Arc<Self> {
        let server = Arc::new(Self {
            inner: Mutex::new(Inner {
                revision: 0,
                sessions: BTreeMap::new(),
                handles: BTreeMap::new(),
                events: VecDeque::new(),
                idempotency: BTreeMap::new(),
                idempotency_order: VecDeque::new(),
                subscribers: Vec::new(),
                reported: BTreeMap::new(),
                controllers: BTreeMap::new(),
            }),
            backend: Mutex::new(backend),
            source,
            host: Mutex::new(None),
            stopping: Arc::new(AtomicBool::new(false)),
            owner_uid: server_uid(),
        });
        server.refresh_from_source();
        server
    }

    /// Issue #352: hands this server the runtime that owns the terminals.
    /// Called once, by `session::service`, before the listener binds --
    /// attaching a host mid-flight would let two connections disagree about
    /// which capabilities were advertised to them.
    pub fn attach_host(&self, host: Arc<dyn SessionHost>) {
        match self.host.lock() {
            Ok(mut guard) => *guard = Some(host),
            Err(poisoned) => *poisoned.into_inner() = Some(host),
        }
    }

    fn host(&self) -> Option<Arc<dyn SessionHost>> {
        match self.host.lock() {
            Ok(guard) => guard.clone(),
            Err(poisoned) => poisoned.into_inner().clone(),
        }
    }

    /// What THIS server advertises, as opposed to what the protocol defines:
    /// the attachment surface only when there is a runtime host behind it.
    /// The filtering is here rather than at the call sites so `hello`,
    /// `server.capabilities` and any future advertiser cannot disagree.
    pub fn advertised(&self) -> Vec<Capability> {
        if self.host().is_some() {
            ADVERTISED.to_vec()
        } else {
            ADVERTISED_WITHOUT_HOST.to_vec()
        }
    }

    fn with_host<T>(
        &self,
        call: impl FnOnce(&dyn SessionHost) -> Result<T, ApiError>,
    ) -> Result<T, ApiError> {
        let Some(host) = self.host() else {
            return Err(ApiError::new(
                ErrorCode::Unsupported,
                "this server owns no terminals: attach, detach, takeover, resize and screen \
                 need the persistent runtime (issue #352), started with `zirv session serve`",
            ));
        };
        call(host.as_ref())
    }

    /// Pulls the current session facts from the source, emitting one event
    /// per added, changed or removed session. Called once at construction
    /// and again before every snapshot/list, so a client never sees a
    /// registry change only after some unrelated call happened to notice it.
    pub fn refresh_from_source(&self) {
        let fresh = self.source.sessions();
        let mut inner = self.lock();
        let mut seen: Vec<String> = Vec::new();
        for mut facts in fresh {
            seen.push(facts.session_id.clone());
            if let Some(reported) = inner.reported.get(&facts.session_id) {
                facts.state = *reported;
            }
            match inner.sessions.get(&facts.session_id) {
                Some(existing) if *existing == facts => {}
                Some(_) => {
                    inner
                        .sessions
                        .insert(facts.session_id.clone(), facts.clone());
                    inner.emit(
                        Some(facts.session_id.clone()),
                        Some(facts.generation),
                        ApiEvent::SessionUpdated { session: facts },
                    );
                }
                None => {
                    inner
                        .sessions
                        .insert(facts.session_id.clone(), facts.clone());
                    inner.emit(
                        Some(facts.session_id.clone()),
                        Some(facts.generation),
                        ApiEvent::SessionStarted { session: facts },
                    );
                }
            }
        }
        // A session this server started itself is not in the registry, so it
        // must not be swept by a refresh that cannot see it.
        let gone: Vec<String> = inner
            .sessions
            .keys()
            .filter(|id| !seen.contains(id) && !inner.handles.contains_key(*id))
            .cloned()
            .collect();
        for id in gone {
            inner.sessions.remove(&id);
            inner.emit(
                Some(id.clone()),
                None,
                ApiEvent::SessionEnded { session_id: id },
            );
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        // A poisoned lock means a previous holder panicked mid-method. The
        // state behind it is a session map and an event log, not a
        // half-written invariant, and refusing every later call would turn
        // one panic into a dead control plane -- so the guard is recovered.
        match self.inner.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        }
    }

    pub fn revision(&self) -> u64 {
        self.lock().revision
    }

    pub fn hello(&self) -> Hello {
        Hello {
            version: PROTOCOL_VERSION,
            server: SERVER_NAME.to_string(),
            server_version: env!("CARGO_PKG_VERSION").to_string(),
            revision: self.revision(),
            capabilities: self.advertised(),
        }
    }

    pub fn stop(&self) {
        self.stopping.store(true, Ordering::SeqCst);
    }

    fn stopping(&self) -> bool {
        self.stopping.load(Ordering::SeqCst)
    }

    // -----------------------------------------------------------------
    // Dispatch
    // -----------------------------------------------------------------

    /// Answers one request. Never panics and never blocks on the mutex for
    /// longer than one map operation -- `session.wait` releases the lock
    /// between polls on purpose, or one waiting client would freeze every
    /// other one.
    pub fn handle(&self, request: &Request) -> Response {
        if request.version != PROTOCOL_VERSION {
            return self.error(
                request,
                ApiError::new(
                    ErrorCode::VersionMismatch,
                    format!(
                        "protocol version {} does not match {PROTOCOL_VERSION}",
                        request.version
                    ),
                ),
            );
        }
        let Some(spec) = spec_for(request.method) else {
            return self.error(
                request,
                ApiError::new(ErrorCode::UnknownMethod, "unrecognized method"),
            );
        };

        // Idempotent replay: a mutation retried with the same key returns
        // the first attempt's result without touching the backend again.
        let cache_key = request
            .idempotency_key
            .as_ref()
            .filter(|_| spec.mutation)
            .map(|key| format!("{}:{key}", spec.name));
        // The lookup is its own statement on purpose: an `if let` chain
        // would hold the guard across the `self.ok` call in its body, and
        // `self.ok` locks again to read the revision.
        let cached = cache_key
            .as_ref()
            .and_then(|key| self.lock().idempotency.get(key).cloned());
        if let Some(cached) = cached {
            return self.ok(request, cached);
        }

        let result = match request.method {
            Method::ServerPing => Ok(json!({
                "server": SERVER_NAME,
                "server_version": env!("CARGO_PKG_VERSION"),
                "protocol": PROTOCOL_VERSION,
            })),
            Method::ServerCapabilities => Ok(self.capabilities_result()),
            Method::SessionSnapshot => Ok(self.snapshot_result()),
            Method::SessionList => self.list_result(&request.params),
            Method::SessionGet => self.get_result(&request.params),
            Method::SessionStart => self.start_result(&request.params),
            Method::SessionStop => self.stop_result(&request.params),
            Method::SessionRead => self.read_result(&request.params),
            Method::SessionSendInput => self.send_input_result(&request.params),
            Method::SessionWait => self.wait_result(&request.params),
            Method::SessionReportStatus => self.report_status_result(&request.params),
            Method::SessionAttach => self.attach_result(&request.params),
            Method::SessionDetach => self.detach_result(&request.params),
            Method::SessionTakeover => self.takeover_result(&request.params),
            Method::SessionResize => self.resize_result(&request.params),
            Method::SessionScreen => self.screen_result(&request.params),
            // The reply is produced here; the streaming half lives in
            // `serve_connection`, which is the only place that owns a
            // connection to stream on.
            Method::EventsSubscribe => Ok(json!({
                "subscribed": true,
                "revision": self.revision(),
            })),
            Method::Unknown => Err(ApiError::new(
                ErrorCode::UnknownMethod,
                "unrecognized method",
            )),
        };

        match result {
            Ok(value) => {
                if let Some(key) = cache_key {
                    self.lock().remember(key, value.clone());
                }
                self.ok(request, value)
            }
            Err(error) => self.error(request, error),
        }
    }

    fn ok(&self, request: &Request, result: Value) -> Response {
        Response {
            version: PROTOCOL_VERSION,
            id: request.id.clone(),
            revision: self.revision(),
            outcome: Outcome::Ok { result },
        }
    }

    fn error(&self, request: &Request, error: ApiError) -> Response {
        Response {
            version: PROTOCOL_VERSION,
            id: request.id.clone(),
            revision: self.revision(),
            outcome: Outcome::Error { error },
        }
    }

    // -----------------------------------------------------------------
    // Methods
    // -----------------------------------------------------------------

    fn capabilities_result(&self) -> Value {
        let runtime = match self.backend.lock() {
            Ok(guard) => guard
                .as_ref()
                .and_then(|backend| serde_json::to_value(backend.capabilities()).ok()),
            Err(_) => None,
        };
        let advertised = self.advertised();
        // Only the methods this server's own capabilities cover: a client
        // reading `methods` must not find one it could never call.
        let methods: Vec<&'static str> = super::wire::METHODS
            .iter()
            .filter(|spec| advertised.contains(&spec.capability))
            .map(|spec| spec.name)
            .collect();
        let mut value = json!({
            "protocol": PROTOCOL_VERSION,
            "capabilities": advertised.iter().map(|c| c.as_str()).collect::<Vec<_>>(),
            "methods": methods,
        });
        if let Some(runtime) = runtime
            && let Some(map) = value.as_object_mut()
        {
            map.insert("runtime".to_string(), runtime);
        }
        value
    }

    fn snapshot_result(&self) -> Value {
        self.refresh_from_source();
        let inner = self.lock();
        json!({
            "revision": inner.revision,
            "sessions": inner.sessions.values().cloned().collect::<Vec<_>>(),
        })
    }

    fn list_result(&self, params: &Value) -> Result<Value, ApiError> {
        #[derive(Debug, Default, Deserialize)]
        struct Params {
            #[serde(default)]
            state: Option<SessionState>,
        }
        let params: Params = parse_params(params)?;
        self.refresh_from_source();
        let inner = self.lock();
        let sessions: Vec<SessionFacts> = inner
            .sessions
            .values()
            .filter(|facts| params.state.is_none_or(|state| facts.state == state))
            .cloned()
            .collect();
        Ok(json!({ "sessions": sessions }))
    }

    fn get_result(&self, params: &Value) -> Result<Value, ApiError> {
        #[derive(Debug, Deserialize)]
        struct Params {
            session_id: String,
        }
        let params: Params = parse_params(params)?;
        self.refresh_from_source();
        let inner = self.lock();
        let facts = inner
            .sessions
            .get(&params.session_id)
            .cloned()
            .ok_or_else(|| unknown_session(&params.session_id))?;
        Ok(json!({ "session": facts }))
    }

    fn start_result(&self, params: &Value) -> Result<Value, ApiError> {
        #[derive(Debug, Deserialize)]
        struct Params {
            #[serde(default)]
            runtime: RuntimeKind,
            #[serde(default)]
            role: String,
            #[serde(default)]
            agent: Option<String>,
            #[serde(default)]
            model: Option<String>,
            #[serde(default)]
            surface: UiSurface,
            cwd: String,
            prompt: String,
        }
        let params: Params = parse_params(params)?;
        let spec = SessionSpec {
            runtime: params.runtime,
            role: params.role.clone(),
            agent: params.agent.clone(),
            provider_route: None,
            model: params.model.clone(),
            surface: params.surface,
            cwd: std::path::PathBuf::from(params.cwd),
            prompt: params.prompt,
            extra_args: Vec::new(),
        };
        // Issue #352: a runtime that owns terminals answers `session.start`
        // itself. Checked before the backend, and only when a host is
        // attached at all, so the server issue #353 shipped is unaffected.
        if let Some(host) = self.host() {
            let facts = host.start(&spec)?;
            let mut inner = self.lock();
            inner
                .sessions
                .insert(facts.session_id.clone(), facts.clone());
            inner.emit(
                Some(facts.session_id.clone()),
                Some(facts.generation),
                ApiEvent::SessionStarted {
                    session: facts.clone(),
                },
            );
            return Ok(json!({ "session": facts }));
        }
        let handle = self.with_backend(|backend| backend.start(&spec))?;
        let facts = SessionFacts {
            session_id: handle.logical_id.clone(),
            short: handle.short.clone(),
            runtime: handle.runtime,
            generation: handle.generation,
            surface: handle.surface,
            state: SessionState::Starting,
            role: Some(handle.role.clone()),
            agent: params.agent,
            repo_slug: None,
            started_at: None,
            reachable: true,
        };
        let mut inner = self.lock();
        inner
            .handles
            .insert(handle.logical_id.clone(), handle.clone());
        inner
            .sessions
            .insert(facts.session_id.clone(), facts.clone());
        inner.emit(
            Some(facts.session_id.clone()),
            Some(facts.generation),
            ApiEvent::SessionStarted {
                session: facts.clone(),
            },
        );
        Ok(json!({ "session": facts }))
    }

    fn stop_result(&self, params: &Value) -> Result<Value, ApiError> {
        let params: TargetParams = parse_params(params)?;
        let (facts, handle) = self.resolve(&params)?;
        if facts.state == SessionState::Ended {
            return Ok(json!({ "stopped": false }));
        }
        // Issue #352: a session the runtime host owns is stopped through the
        // host's own child-termination ladder. This is the ONE path that ends
        // a session -- `session.detach` and a dropped connection never reach
        // it, which is what "client disconnection never terminates an agent"
        // means in code rather than in prose.
        match (self.host(), handle) {
            (Some(host), _) if host.sessions().iter().any(|f| f.session_id == facts.session_id) => {
                host.stop(&facts.session_id)?;
            }
            (_, Some(handle)) => self.with_backend(|backend| backend.interrupt(&handle))?,
            (_, None) => return Err(not_this_servers_session(&facts.session_id)),
        }
        let mut inner = self.lock();
        if let Some(entry) = inner.sessions.get_mut(&facts.session_id) {
            entry.state = SessionState::Ended;
        }
        inner
            .reported
            .insert(facts.session_id.clone(), SessionState::Ended);
        inner.emit(
            Some(facts.session_id.clone()),
            Some(facts.generation),
            ApiEvent::SessionEnded {
                session_id: facts.session_id.clone(),
            },
        );
        Ok(json!({ "stopped": true }))
    }

    fn read_result(&self, params: &Value) -> Result<Value, ApiError> {
        #[derive(Debug, Deserialize)]
        struct Params {
            session_id: String,
            #[serde(default)]
            after_revision: u64,
        }
        let params: Params = parse_params(params)?;
        let inner = self.lock();
        if !inner.sessions.contains_key(&params.session_id) {
            return Err(unknown_session(&params.session_id));
        }
        let events: Vec<EventFrame> = inner
            .events
            .iter()
            .filter(|frame| {
                frame.revision > params.after_revision
                    && frame.session_id.as_deref() == Some(params.session_id.as_str())
            })
            .cloned()
            .collect();
        Ok(json!({ "revision": inner.revision, "events": events }))
    }

    fn send_input_result(&self, params: &Value) -> Result<Value, ApiError> {
        #[derive(Debug, Deserialize)]
        struct Params {
            session_id: String,
            #[serde(default)]
            generation: Option<u64>,
            input: String,
            #[serde(default)]
            mode: InputMode,
            #[serde(default)]
            client_id: Option<String>,
        }
        let params: Params = parse_params(params)?;
        let target = TargetParams {
            session_id: params.session_id.clone(),
            generation: params.generation,
        };
        let (facts, handle) = self.resolve(&target)?;
        // Issue #352: raw bytes belong to the terminal, so they go to the
        // runtime host and never to a `RuntimeBackend` -- a backend has no
        // keyboard. Handled before the handle lookup below, because a
        // host-owned session has no backend handle at all.
        if params.mode == InputMode::Raw {
            let Some(client_id) = params.client_id.as_deref() else {
                return Err(ApiError::new(
                    ErrorCode::InvalidParams,
                    "mode=raw needs client_id: only the session's controller may type into it",
                ));
            };
            self.with_host(|host| {
                host.write_raw(&params.session_id, client_id, params.input.as_bytes())
            })?;
            return Ok(json!({ "accepted": true }));
        }
        let handle = handle.ok_or_else(|| not_this_servers_session(&facts.session_id))?;
        match params.mode {
            InputMode::Submit => {
                self.with_backend(|backend| backend.submit(&handle, &params.input))
            }
            InputMode::Steer => self.with_backend(|backend| backend.steer(&handle, &params.input)),
            // Handled above; repeated here only because the match is
            // exhaustive over the vocabulary.
            InputMode::Raw => Ok(()),
            InputMode::Unknown => Err(ApiError::new(
                ErrorCode::InvalidParams,
                "mode must be submit, steer or raw",
            )),
        }?;
        let mut inner = self.lock();
        if let Some(entry) = inner.sessions.get_mut(&facts.session_id) {
            entry.state = SessionState::Working;
            let updated = entry.clone();
            inner
                .reported
                .insert(facts.session_id.clone(), SessionState::Working);
            inner.emit(
                Some(facts.session_id.clone()),
                Some(facts.generation),
                ApiEvent::SessionUpdated { session: updated },
            );
        }
        Ok(json!({ "accepted": true }))
    }

    fn report_status_result(&self, params: &Value) -> Result<Value, ApiError> {
        #[derive(Debug, Deserialize)]
        struct Params {
            session_id: String,
            #[serde(default)]
            generation: Option<u64>,
            state: SessionState,
        }
        let params: Params = parse_params(params)?;
        let target = TargetParams {
            session_id: params.session_id.clone(),
            generation: params.generation,
        };
        let (facts, _) = self.resolve(&target)?;
        let mut inner = self.lock();
        let Some(entry) = inner.sessions.get_mut(&facts.session_id) else {
            return Err(unknown_session(&facts.session_id));
        };
        entry.state = params.state;
        let updated = entry.clone();
        inner
            .reported
            .insert(facts.session_id.clone(), params.state);
        inner.emit(
            Some(facts.session_id.clone()),
            Some(facts.generation),
            ApiEvent::SessionUpdated { session: updated },
        );
        Ok(json!({ "recorded": true }))
    }

    // -----------------------------------------------------------------
    // Attachment (issue #352)
    // -----------------------------------------------------------------

    /// Every attachment method takes the same three things, so they parse the
    /// same struct: which session, which client, and (for the two that can
    /// move a terminal) how big the caller's own window is.
    fn attach_result(&self, params: &Value) -> Result<Value, ApiError> {
        #[derive(Debug, Deserialize)]
        struct Params {
            session_id: String,
            #[serde(default)]
            generation: Option<u64>,
            client_id: String,
            #[serde(default)]
            mode: AttachMode,
            #[serde(default)]
            rows: Option<u16>,
            #[serde(default)]
            cols: Option<u16>,
        }
        let params: Params = parse_params(params)?;
        if params.mode == AttachMode::Unknown {
            return Err(ApiError::new(
                ErrorCode::InvalidParams,
                "mode must be observer or controller",
            ));
        }
        // The generation pin is enforced against the server's own view before
        // the host is touched at all, exactly like every other mutation: an
        // attach to a session that has been replaced must fail rather than
        // silently land on the replacement.
        let facts = self.pinned_facts(&params.session_id, params.generation)?;
        let size = match (params.rows, params.cols) {
            (Some(rows), Some(cols)) => Some((rows, cols)),
            _ => None,
        };
        let attachment = self.with_host(|host| {
            host.attach(&params.session_id, &params.client_id, params.mode, size)
        })?;
        self.publish_controller(&facts, &attachment);
        Ok(json!({ "attachment": attachment }))
    }

    fn detach_result(&self, params: &Value) -> Result<Value, ApiError> {
        let params: ClientParams = parse_params(params)?;
        let facts = self.pinned_facts(&params.session_id, None)?;
        // Deliberately nothing else: detaching is a CLIENT lifecycle event.
        // The session keeps its process, its pty, its supervisor and its
        // state -- `session.stop` is the only method that ends one.
        let attachment =
            self.with_host(|host| host.detach(&params.session_id, &params.client_id))?;
        self.publish_controller(&facts, &attachment);
        Ok(json!({ "attachment": attachment }))
    }

    fn takeover_result(&self, params: &Value) -> Result<Value, ApiError> {
        let params: ClientParams = parse_params(params)?;
        let facts = self.pinned_facts(&params.session_id, None)?;
        let attachment =
            self.with_host(|host| host.takeover(&params.session_id, &params.client_id))?;
        self.publish_controller(&facts, &attachment);
        Ok(json!({ "attachment": attachment }))
    }

    fn resize_result(&self, params: &Value) -> Result<Value, ApiError> {
        #[derive(Debug, Deserialize)]
        struct Params {
            session_id: String,
            client_id: String,
            rows: u16,
            cols: u16,
        }
        let params: Params = parse_params(params)?;
        self.pinned_facts(&params.session_id, None)?;
        let attachment = self.with_host(|host| {
            host.resize(
                &params.session_id,
                &params.client_id,
                params.rows,
                params.cols,
            )
        })?;
        Ok(json!({ "attachment": attachment }))
    }

    fn screen_result(&self, params: &Value) -> Result<Value, ApiError> {
        let params: ClientParams = parse_params(params)?;
        self.pinned_facts(&params.session_id, None)?;
        let screen =
            self.with_host(|host| host.screen(&params.session_id, &params.client_id))?;
        Ok(json!({ "revision": self.revision(), "screen": screen }))
    }

    /// The session the server knows under `session_id`, with the caller's
    /// generation pin enforced. A host-owned session is always in the
    /// server's own map, because the host is its session source.
    fn pinned_facts(
        &self,
        session_id: &str,
        generation: Option<u64>,
    ) -> Result<SessionFacts, ApiError> {
        let target = TargetParams {
            session_id: session_id.to_string(),
            generation,
        };
        self.resolve(&target).map(|(facts, _)| facts)
    }

    /// One `controller_changed` per ACTUAL change, never per call: a client
    /// that re-attaches as an observer while somebody else is typing must not
    /// make every other observer redraw a takeover banner.
    fn publish_controller(&self, facts: &SessionFacts, attachment: &Attachment) {
        let mut inner = self.lock();
        let previous = inner.controllers.get(&facts.session_id).cloned().flatten();
        if previous == attachment.controller {
            return;
        }
        inner
            .controllers
            .insert(facts.session_id.clone(), attachment.controller.clone());
        inner.emit(
            Some(facts.session_id.clone()),
            Some(facts.generation),
            ApiEvent::ControllerChanged {
                controller: attachment.controller.clone(),
            },
        );
    }

    /// Waits are PINNED: the generation resolved at call time is compared on
    /// every poll, so a session that is replaced while the wait is running
    /// fails the wait with `stale_generation` instead of letting the
    /// replacement satisfy it.
    fn wait_result(&self, params: &Value) -> Result<Value, ApiError> {
        #[derive(Debug, Deserialize)]
        struct Params {
            session_id: String,
            #[serde(default)]
            generation: Option<u64>,
            #[serde(default)]
            until: WaitUntil,
            #[serde(default)]
            timeout_ms: Option<u64>,
        }
        let params: Params = parse_params(params)?;
        if params.until == WaitUntil::Unknown {
            return Err(ApiError::new(
                ErrorCode::InvalidParams,
                "until must be idle or ended",
            ));
        }
        let target = TargetParams {
            session_id: params.session_id.clone(),
            generation: params.generation,
        };
        let (facts, _) = self.resolve(&target)?;
        let pinned = facts.generation;
        let budget = params
            .timeout_ms
            .unwrap_or(DEFAULT_WAIT_MS)
            .min(MAX_WAIT_MS);
        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(budget);

        loop {
            let observed = {
                let inner = self.lock();
                inner.sessions.get(&params.session_id).cloned()
            };
            let Some(observed) = observed else {
                return Err(unknown_session(&params.session_id));
            };
            if observed.generation != pinned {
                return Err(ApiError::new(
                    ErrorCode::StaleGeneration,
                    format!(
                        "session {} moved from generation {pinned} to {} while the wait was running",
                        params.session_id, observed.generation
                    ),
                ));
            }
            let matched = match params.until {
                WaitUntil::Idle => observed.state == SessionState::Idle,
                WaitUntil::Ended => observed.state == SessionState::Ended,
                WaitUntil::Unknown => false,
            };
            if matched {
                return Ok(json!({
                    "outcome": "matched",
                    "state": observed.state,
                    "generation": pinned,
                }));
            }
            if std::time::Instant::now() >= deadline {
                return Ok(json!({
                    "outcome": "timeout",
                    "state": observed.state,
                    "generation": pinned,
                }));
            }
            std::thread::sleep(WAIT_POLL);
        }
    }

    // -----------------------------------------------------------------
    // Shared helpers
    // -----------------------------------------------------------------

    /// Resolves a session and enforces the generation pin. A caller that
    /// names a generation which is not the session's current one is refused
    /// with `stale_generation` in BOTH directions: an older pin means the
    /// session has been replaced, a newer one means the caller is talking
    /// about a session this server has never seen.
    fn resolve(
        &self,
        params: &TargetParams,
    ) -> Result<(SessionFacts, Option<SessionHandle>), ApiError> {
        let inner = self.lock();
        let facts = inner
            .sessions
            .get(&params.session_id)
            .cloned()
            .ok_or_else(|| unknown_session(&params.session_id))?;
        if let Some(pinned) = params.generation
            && pinned != facts.generation
        {
            return Err(ApiError::new(
                ErrorCode::StaleGeneration,
                format!(
                    "session {} is at generation {}, not {pinned}",
                    params.session_id, facts.generation
                ),
            ));
        }
        let handle = inner.handles.get(&params.session_id).cloned();
        Ok((facts, handle))
    }

    fn with_backend<T>(
        &self,
        call: impl FnOnce(&mut dyn RuntimeBackend) -> CtxResult<T>,
    ) -> Result<T, ApiError> {
        let mut guard = match self.backend.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        let Some(backend) = guard.as_mut() else {
            return Err(ApiError::new(
                ErrorCode::Unsupported,
                "this server has no runtime backend attached: session mutations arrive with the \
                 persistent runtime (issue #352) and its native integration (issue #489)",
            ));
        };
        call(backend.as_mut()).map_err(|error| backend_error(error.as_ref()))
    }

    // -----------------------------------------------------------------
    // Serving a connection
    // -----------------------------------------------------------------

    /// Drives one accepted connection to completion: the hello handshake,
    /// then request/reply, then -- if the client subscribed -- the event
    /// stream until it disconnects or the server stops.
    pub fn serve_connection(self: &Arc<Self>, mut connection: Connection) -> CtxResult<()> {
        if !connection.peer().is_same_user(self.owner_uid) {
            // Answered rather than dropped silently, so a legitimate client
            // that somehow reaches the wrong endpoint sees why.
            let _ = connection.write_frame(&super::wire::ServerFrame::Response(Response {
                version: PROTOCOL_VERSION,
                id: String::new(),
                revision: self.revision(),
                outcome: Outcome::Error {
                    error: ApiError::new(
                        ErrorCode::Denied,
                        "this endpoint serves only the user that owns it",
                    ),
                },
            }));
            return Ok(());
        }
        connection.write_frame(&super::wire::ServerFrame::Hello(self.hello()))?;

        while let Some(request) = connection.read_frame::<Request>()? {
            let subscribing =
                request.method == Method::EventsSubscribe && request.version == PROTOCOL_VERSION;
            let response = self.handle(&request);
            let accepted = matches!(response.outcome, Outcome::Ok { .. });
            let after_revision = subscription_start(&request.params);
            let receiver = if subscribing && accepted {
                Some(self.subscribe(after_revision))
            } else {
                None
            };
            connection.write_frame(&super::wire::ServerFrame::Response(response))?;
            if let Some((receiver, backlog)) = receiver {
                for frame in backlog {
                    connection.write_frame(&super::wire::ServerFrame::Event(frame))?;
                }
                return self.pump(&mut connection, receiver);
            }
        }
        Ok(())
    }

    /// Registers a subscriber and returns its channel plus everything it
    /// missed, both computed under ONE lock so no event can slip between
    /// the backlog and the live stream.
    fn subscribe(&self, after_revision: u64) -> (Receiver<EventFrame>, Vec<EventFrame>) {
        let (tx, rx) = channel();
        let mut inner = self.lock();
        inner.subscribers.push(tx);
        let backlog = inner
            .events
            .iter()
            .filter(|frame| frame.revision > after_revision)
            .cloned()
            .collect();
        (rx, backlog)
    }

    fn pump(&self, connection: &mut Connection, receiver: Receiver<EventFrame>) -> CtxResult<()> {
        loop {
            if self.stopping() {
                return Ok(());
            }
            match receiver.recv_timeout(SUBSCRIBE_POLL) {
                Ok(frame) => connection.write_frame(&super::wire::ServerFrame::Event(frame))?,
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => return Ok(()),
            }
        }
    }

    /// The retained event log, for the frozen-fixture guard in
    /// `super::fixtures`, which has to freeze event frames a reply never
    /// carries.
    #[cfg(test)]
    pub fn frozen_events(&self) -> Vec<EventFrame> {
        self.lock().events.iter().cloned().collect()
    }

    /// The emit seam a runtime owner drives events through without going via
    /// a mutation method -- the tests below, and issue #489's native session
    /// integration. Nothing inside this module calls it, hence the allow.
    #[allow(dead_code)]
    pub fn publish(&self, session_id: Option<String>, generation: Option<u64>, event: ApiEvent) {
        self.lock().emit(session_id, generation, event);
    }
}

#[derive(Debug, Deserialize)]
struct TargetParams {
    session_id: String,
    #[serde(default)]
    generation: Option<u64>,
}

/// Issue #352: what `detach`, `takeover` and `screen` all take.
#[derive(Debug, Deserialize)]
struct ClientParams {
    session_id: String,
    client_id: String,
}

fn subscription_start(params: &Value) -> u64 {
    params
        .get("after_revision")
        .and_then(Value::as_u64)
        .unwrap_or(0)
}

/// `params` is optional on the wire, so a missing value must read as an
/// empty object rather than as a parse failure.
fn parse_params<T: serde::de::DeserializeOwned>(params: &Value) -> Result<T, ApiError> {
    let value = if params.is_null() {
        Value::Object(serde_json::Map::new())
    } else {
        params.clone()
    };
    serde_json::from_value(value)
        .map_err(|error| ApiError::new(ErrorCode::InvalidParams, error.to_string()))
}

fn unknown_session(session_id: &str) -> ApiError {
    ApiError::new(
        ErrorCode::UnknownSession,
        format!("no session {session_id}"),
    )
}

fn not_this_servers_session(session_id: &str) -> ApiError {
    ApiError::new(
        ErrorCode::Unsupported,
        format!(
            "session {session_id} is not driven by this server: it came from the session registry, \
             and PTY ownership stays with the dashboard until the persistent runtime (issue #352)"
        ),
    )
}

/// Maps a backend failure onto a structured code, reusing the same four
/// `RuntimeError` classes `runtime::protocol::dispatch` already
/// distinguishes so the public protocol and the in-process one cannot
/// disagree about what a given failure means.
fn backend_error(error: &(dyn std::error::Error + 'static)) -> ApiError {
    let code = match error.downcast_ref::<RuntimeError>() {
        Some(RuntimeError::Unsupported(_)) => ErrorCode::Unsupported,
        Some(RuntimeError::UnknownSession(_)) => ErrorCode::UnknownSession,
        Some(RuntimeError::Busy(_)) => ErrorCode::Busy,
        Some(RuntimeError::StaleGeneration { .. }) => ErrorCode::StaleGeneration,
        None => ErrorCode::Internal,
    };
    ApiError::new(code, error.to_string())
}

// ---------------------------------------------------------------------------
// The listening half
// ---------------------------------------------------------------------------

/// A bound endpoint with an accept loop behind it. Dropping it stops the
/// loop and removes the endpoint.
#[derive(Debug)]
pub struct RunningServer {
    server: Arc<ApiServer>,
    listener: Arc<Listener>,
    endpoint: Endpoint,
    accept: Option<std::thread::JoinHandle<()>>,
}

impl RunningServer {
    pub fn start(endpoint: &Endpoint, server: Arc<ApiServer>) -> CtxResult<Self> {
        let listener = Arc::new(Listener::bind(endpoint)?);
        let accept_listener = Arc::clone(&listener);
        let accept_server = Arc::clone(&server);
        let accept = std::thread::spawn(move || {
            loop {
                if accept_server.stopping() {
                    return;
                }
                let Ok(connection) = accept_listener.accept() else {
                    // A failed accept on a stopping server is the wake-up
                    // connection; on a live one it is a transient OS error
                    // worth one more attempt rather than a dead endpoint.
                    if accept_server.stopping() {
                        return;
                    }
                    continue;
                };
                if accept_server.stopping() {
                    return;
                }
                let connection_server = Arc::clone(&accept_server);
                std::thread::spawn(move || {
                    let _ = connection_server.serve_connection(connection);
                });
            }
        });
        Ok(Self {
            server,
            listener,
            endpoint: endpoint.clone(),
            accept: Some(accept),
        })
    }

    pub fn endpoint(&self) -> &Endpoint {
        &self.endpoint
    }
}

impl Drop for RunningServer {
    fn drop(&mut self) {
        self.server.stop();
        self.listener.wake();
        if let Some(accept) = self.accept.take() {
            let _ = accept.join();
        }
    }
}

/// The endpoint the CLI wrappers use: always derived from the resolved state
/// directory, never from anything a repository controls.
pub fn endpoint_for(state: &StateDir) -> Endpoint {
    Endpoint::for_state(state)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::ctx::runtime::fake::FakeNativeBackend;

    fn facts(id: &str, state: SessionState) -> SessionFacts {
        let mut facts = SessionFacts::new(id);
        facts.state = state;
        facts.agent = Some("claude".to_string());
        facts.repo_slug = Some("zirv-cli".to_string());
        facts.reachable = true;
        facts
    }

    fn server_with(sessions: Vec<SessionFacts>) -> Arc<ApiServer> {
        ApiServer::new(
            Box::new(StaticSource(sessions)),
            Some(Box::new(FakeNativeBackend::new())),
        )
    }

    fn call(server: &Arc<ApiServer>, method: Method, params: Value) -> Response {
        server.handle(&Request::new("r", method, params))
    }

    fn result(response: &Response) -> Value {
        match &response.outcome {
            Outcome::Ok { result } => result.clone(),
            other => panic!("expected ok, got {other:?}"),
        }
    }

    fn error(response: &Response) -> ApiError {
        match &response.outcome {
            Outcome::Error { error } => error.clone(),
            other => panic!("expected an error, got {other:?}"),
        }
    }

    #[test]
    fn ping_reports_the_server_identity_and_protocol_version() {
        let server = server_with(Vec::new());
        let value = result(&call(&server, Method::ServerPing, Value::Null));
        assert_eq!(value["server"], json!(SERVER_NAME));
        assert_eq!(value["protocol"], json!(PROTOCOL_VERSION));
    }

    #[test]
    fn a_request_at_another_protocol_version_is_refused() {
        let server = server_with(Vec::new());
        let mut request = Request::new("r", Method::ServerPing, Value::Null);
        request.version = 99;
        assert_eq!(
            error(&server.handle(&request)).code,
            ErrorCode::VersionMismatch
        );
    }

    #[test]
    fn an_unknown_method_is_refused_rather_than_guessed_at() {
        let server = server_with(Vec::new());
        let request: Request =
            serde_json::from_str(r#"{"v":1,"id":"r","method":"session.levitate"}"#).expect("parse");
        assert_eq!(
            error(&server.handle(&request)).code,
            ErrorCode::UnknownMethod
        );
    }

    /// Issue #353: "Session IDs remain stable across focus, layout, worktree,
    /// and client changes." Nothing a client does can change one: the only
    /// client-owned axis on the wire is `surface`, and changing it leaves the
    /// id, short id and generation alone.
    #[test]
    fn session_ids_are_stable_across_client_and_surface_changes() {
        let mut first = facts("11111111-2222-4333-8444-555555555555", SessionState::Idle);
        let server = ApiServer::new(Box::new(StaticSource(vec![first.clone()])), None);
        let before = result(&call(&server, Method::SessionSnapshot, Value::Null));

        // The same session, now looked at by a dashboard pane in a different
        // worktree, reported by the source on a later refresh.
        first.surface = UiSurface::DashboardPane;
        first.repo_slug = Some("some-other-worktree".to_string());
        let server = ApiServer::new(Box::new(StaticSource(vec![first.clone()])), None);
        let after = result(&call(&server, Method::SessionSnapshot, Value::Null));

        assert_eq!(
            before["sessions"][0]["session_id"],
            after["sessions"][0]["session_id"]
        );
        assert_eq!(
            before["sessions"][0]["short"],
            after["sessions"][0]["short"]
        );
        assert_eq!(
            before["sessions"][0]["generation"],
            after["sessions"][0]["generation"]
        );
        assert_ne!(
            before["sessions"][0]["surface"], after["sessions"][0]["surface"],
            "the surface is the one axis a client change moves"
        );
    }

    #[test]
    fn get_reports_an_unknown_session_by_code() {
        let server = server_with(Vec::new());
        let response = call(
            &server,
            Method::SessionGet,
            json!({"session_id": "nobody-home"}),
        );
        assert_eq!(error(&response).code, ErrorCode::UnknownSession);
    }

    #[test]
    fn list_filters_by_state() {
        let server = server_with(vec![
            facts("aaaaaaaa-0000-4000-8000-000000000001", SessionState::Idle),
            facts(
                "bbbbbbbb-0000-4000-8000-000000000002",
                SessionState::Working,
            ),
        ]);
        let value = result(&call(
            &server,
            Method::SessionList,
            json!({"state": "idle"}),
        ));
        let sessions = value["sessions"].as_array().expect("array");
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0]["state"], json!("idle"));
    }

    /// Every emitted event moves the revision by exactly one, which is what
    /// makes a gap detectable at all.
    #[test]
    fn every_event_advances_the_revision_by_exactly_one() {
        let server = server_with(Vec::new());
        let before = server.revision();
        server.publish(Some("s".to_string()), Some(1), ApiEvent::Heartbeat);
        server.publish(Some("s".to_string()), Some(1), ApiEvent::Heartbeat);
        assert_eq!(server.revision(), before + 2);
        let inner = server.lock();
        let revisions: Vec<u64> = inner.events.iter().map(|frame| frame.revision).collect();
        for pair in revisions.windows(2) {
            assert_eq!(pair[1], pair[0] + 1, "revisions must be consecutive");
        }
    }

    /// Issue #353: "Mutations are idempotent where retrying could otherwise
    /// duplicate work." The retry must not reach the backend a second time,
    /// which is proven by the event log rather than by the reply alone.
    #[test]
    fn a_retried_mutation_with_the_same_key_does_not_duplicate_work() {
        let server = server_with(vec![facts(
            "aaaaaaaa-0000-4000-8000-000000000001",
            SessionState::Idle,
        )]);
        let start = Request::new(
            "r1",
            Method::SessionStart,
            json!({"cwd": ".", "prompt": "go", "role": "worker"}),
        )
        .with_idempotency_key("start-once");
        let first = server.handle(&start);
        let created = result(&first)["session"].clone();
        let revision_after_first = server.revision();

        let retry = Request::new(
            "r2",
            Method::SessionStart,
            json!({"cwd": ".", "prompt": "go", "role": "worker"}),
        )
        .with_idempotency_key("start-once");
        let second = server.handle(&retry);
        assert_eq!(
            result(&second)["session"],
            created,
            "same session, not a new one"
        );
        assert_eq!(
            server.revision(),
            revision_after_first,
            "the retry must emit no second session_started event"
        );
        let inner = server.lock();
        assert_eq!(
            inner.handles.len(),
            1,
            "the backend must have been asked to start exactly one session"
        );
    }

    /// The same retry without a key is a genuinely new call -- idempotency is
    /// opt-in, so a client that does not ask for it is not silently given it.
    #[test]
    fn a_mutation_without_an_idempotency_key_is_not_deduplicated() {
        let server = server_with(Vec::new());
        let params = json!({"cwd": ".", "prompt": "go", "role": "worker"});
        let _ = call(&server, Method::SessionStart, params.clone());
        let _ = call(&server, Method::SessionStart, params);
        assert_eq!(server.lock().handles.len(), 2);
    }

    /// Issue #353: "Waits pin the resolved session generation so a
    /// replacement cannot satisfy an old wait."
    #[test]
    fn a_wait_is_refused_once_the_session_generation_moves() {
        let id = "aaaaaaaa-0000-4000-8000-000000000001";
        let server = server_with(vec![facts(id, SessionState::Working)]);
        let waiting = Arc::clone(&server);
        let session = id.to_string();
        // The pin is explicit, so this test cannot race: whether the
        // replacement lands before the wait resolves (refused at resolve
        // time) or after (refused by the poll), the answer is the same
        // stale_generation.
        let waiter = std::thread::spawn(move || {
            waiting.handle(&Request::new(
                "w",
                Method::SessionWait,
                json!({"session_id": session, "generation": 1, "until": "idle", "timeout_ms": 5000}),
            ))
        });
        // The replacement: same id, new generation, and immediately idle --
        // exactly the state the wait was asked for. It must NOT satisfy it.
        loop {
            let mut inner = server.lock();
            if let Some(entry) = inner.sessions.get_mut(id) {
                entry.generation = 2;
                entry.state = SessionState::Idle;
                break;
            }
            drop(inner);
            std::thread::sleep(WAIT_POLL);
        }
        let response = waiter.join().expect("wait thread");
        assert_eq!(error(&response).code, ErrorCode::StaleGeneration);
    }

    #[test]
    fn a_wait_that_is_already_satisfied_returns_matched_with_its_pinned_generation() {
        let id = "aaaaaaaa-0000-4000-8000-000000000001";
        let server = server_with(vec![facts(id, SessionState::Idle)]);
        let value = result(&call(
            &server,
            Method::SessionWait,
            json!({"session_id": id, "until": "idle", "timeout_ms": 50}),
        ));
        assert_eq!(value["outcome"], json!("matched"));
        assert_eq!(value["generation"], json!(1));
    }

    #[test]
    fn a_wait_that_does_not_come_true_times_out_rather_than_hanging() {
        let id = "aaaaaaaa-0000-4000-8000-000000000001";
        let server = server_with(vec![facts(id, SessionState::Working)]);
        let value = result(&call(
            &server,
            Method::SessionWait,
            json!({"session_id": id, "until": "idle", "timeout_ms": 30}),
        ));
        assert_eq!(value["outcome"], json!("timeout"));
        assert_eq!(value["state"], json!("working"));
    }

    #[test]
    fn a_mutation_pinned_to_a_stale_generation_is_refused() {
        let id = "aaaaaaaa-0000-4000-8000-000000000001";
        let server = server_with(vec![facts(id, SessionState::Idle)]);
        let response = call(
            &server,
            Method::SessionReportStatus,
            json!({"session_id": id, "generation": 7, "state": "working"}),
        );
        assert_eq!(error(&response).code, ErrorCode::StaleGeneration);
    }

    #[test]
    fn report_status_records_the_state_and_emits_one_event() {
        let id = "aaaaaaaa-0000-4000-8000-000000000001";
        let server = server_with(vec![facts(id, SessionState::Idle)]);
        let before = server.revision();
        let value = result(&call(
            &server,
            Method::SessionReportStatus,
            json!({"session_id": id, "state": "working"}),
        ));
        assert_eq!(value["recorded"], json!(true));
        assert_eq!(server.revision(), before + 1);
        let get = result(&call(
            &server,
            Method::SessionGet,
            json!({"session_id": id}),
        ));
        assert_eq!(get["session"]["state"], json!("working"));
    }

    /// Without a backend, a mutation must say so with a structured code --
    /// never quietly succeed, and never claim the session does not exist.
    #[test]
    fn a_server_without_a_backend_refuses_mutations_by_code() {
        let server = ApiServer::new(Box::new(StaticSource(Vec::new())), None);
        let response = call(
            &server,
            Method::SessionStart,
            json!({"cwd": ".", "prompt": "go"}),
        );
        let error = error(&response);
        assert_eq!(error.code, ErrorCode::Unsupported);
        assert!(error.message.contains("#352"), "{}", error.message);
    }

    /// A registry session has no backend handle, so input for it is refused
    /// rather than sent to a backend that never started it.
    #[test]
    fn input_for_a_session_this_server_did_not_start_is_refused() {
        let id = "aaaaaaaa-0000-4000-8000-000000000001";
        let server = server_with(vec![facts(id, SessionState::Idle)]);
        let response = call(
            &server,
            Method::SessionSendInput,
            json!({"session_id": id, "input": "hello"}),
        );
        assert_eq!(error(&response).code, ErrorCode::Unsupported);
    }

    #[test]
    fn input_for_a_session_this_server_started_reaches_the_backend() {
        let server = server_with(Vec::new());
        let started = result(&call(
            &server,
            Method::SessionStart,
            json!({"cwd": ".", "prompt": "go"}),
        ));
        let id = started["session"]["session_id"].as_str().expect("id");
        let value = result(&call(
            &server,
            Method::SessionSendInput,
            json!({"session_id": id, "input": "hello"}),
        ));
        assert_eq!(value["accepted"], json!(true));
        let read = result(&call(
            &server,
            Method::SessionRead,
            json!({"session_id": id}),
        ));
        assert!(
            !read["events"].as_array().expect("array").is_empty(),
            "the session's own events must be replayable"
        );
    }

    /// Snapshots carry only redacted facts. Proven on the serialized
    /// snapshot, not just on the struct, because that is what a client sees.
    #[test]
    fn a_snapshot_carries_no_transcript_prompt_or_absolute_path() {
        let server = server_with(Vec::new());
        let _ = call(
            &server,
            Method::SessionStart,
            json!({"cwd": "/home/somebody/secret-repo", "prompt": "the secret prompt"}),
        );
        let snapshot = result(&call(&server, Method::SessionSnapshot, Value::Null));
        let text = serde_json::to_string(&snapshot).expect("serialize");
        assert!(!text.contains("the secret prompt"), "{text}");
        assert!(!text.contains("secret-repo"), "{text}");
    }

    #[test]
    fn capabilities_report_the_attached_backends_own_capabilities() {
        let server = server_with(Vec::new());
        let value = result(&call(&server, Method::ServerCapabilities, Value::Null));
        assert_eq!(value["protocol"], json!(PROTOCOL_VERSION));
        assert!(value["runtime"].is_object(), "{value}");
        let without = ApiServer::new(Box::new(StaticSource(Vec::new())), None);
        let value = result(&call(&without, Method::ServerCapabilities, Value::Null));
        assert!(value["runtime"].is_null(), "{value}");
    }

    // -----------------------------------------------------------------
    // Attachment (issue #352)
    // -----------------------------------------------------------------

    /// The attachment rules with no pty in the way: observers are unbounded,
    /// the controller seat holds one client, and the seat only ever changes
    /// through an explicit grant, release or takeover. `session::host` is the
    /// production implementation of the same trait; this one exists so the
    /// PROTOCOL's own enforcement is provable without spawning a process.
    #[derive(Debug, Default)]
    struct FakeHost {
        state: Mutex<FakeHostState>,
    }

    #[derive(Debug, Default)]
    struct FakeHostState {
        facts: Vec<SessionFacts>,
        clients: Vec<String>,
        controller: Option<String>,
        rows: u16,
        cols: u16,
        typed: Vec<u8>,
        stopped: Vec<String>,
    }

    impl FakeHost {
        fn with(facts: Vec<SessionFacts>) -> Arc<Self> {
            Arc::new(Self {
                state: Mutex::new(FakeHostState {
                    facts,
                    rows: 24,
                    cols: 80,
                    ..FakeHostState::default()
                }),
            })
        }

        fn lock(&self) -> std::sync::MutexGuard<'_, FakeHostState> {
            match self.state.lock() {
                Ok(guard) => guard,
                Err(poisoned) => poisoned.into_inner(),
            }
        }

        fn snapshot(&self, caller: &str) -> Attachment {
            let state = self.lock();
            Attachment {
                controller: state.controller.clone(),
                clients: state.clients.clone(),
                rows: state.rows,
                cols: state.cols,
                role: if state.controller.as_deref() == Some(caller) {
                    super::super::wire::AttachRole::Controller
                } else if state.clients.iter().any(|id| id == caller) {
                    super::super::wire::AttachRole::Observer
                } else {
                    super::super::wire::AttachRole::Detached
                },
            }
        }
    }

    impl SessionHost for FakeHost {
        fn sessions(&self) -> Vec<SessionFacts> {
            self.lock().facts.clone()
        }

        /// The double owns no pty, so it opens nothing: every test here
        /// exercises the ATTACHMENT surface over sessions seeded by `with`.
        /// `session::host`'s own tests cover the spawn.
        fn start(&self, _spec: &SessionSpec) -> Result<SessionFacts, ApiError> {
            Err(ApiError::new(
                ErrorCode::Unsupported,
                "the test host opens no terminals",
            ))
        }

        fn attach(
            &self,
            _session_id: &str,
            client_id: &str,
            mode: AttachMode,
            size: Option<(u16, u16)>,
        ) -> Result<Attachment, ApiError> {
            {
                let mut state = self.lock();
                if !state.clients.iter().any(|id| id == client_id) {
                    state.clients.push(client_id.to_string());
                    state.clients.sort();
                }
                if mode == AttachMode::Controller {
                    if let Some(current) = state.controller.clone()
                        && current != client_id
                    {
                        return Err(ApiError::new(
                            ErrorCode::Busy,
                            format!("{current} already controls this session"),
                        ));
                    }
                    state.controller = Some(client_id.to_string());
                    if let Some((rows, cols)) = size {
                        state.rows = rows;
                        state.cols = cols;
                    }
                }
            }
            Ok(self.snapshot(client_id))
        }

        fn detach(&self, _session_id: &str, client_id: &str) -> Result<Attachment, ApiError> {
            {
                let mut state = self.lock();
                state.clients.retain(|id| id != client_id);
                if state.controller.as_deref() == Some(client_id) {
                    state.controller = None;
                }
            }
            Ok(self.snapshot(client_id))
        }

        fn takeover(&self, _session_id: &str, client_id: &str) -> Result<Attachment, ApiError> {
            {
                let mut state = self.lock();
                if !state.clients.iter().any(|id| id == client_id) {
                    state.clients.push(client_id.to_string());
                    state.clients.sort();
                }
                state.controller = Some(client_id.to_string());
            }
            Ok(self.snapshot(client_id))
        }

        fn resize(
            &self,
            _session_id: &str,
            client_id: &str,
            rows: u16,
            cols: u16,
        ) -> Result<Attachment, ApiError> {
            {
                let mut state = self.lock();
                if state.controller.as_deref() != Some(client_id) {
                    return Err(ApiError::new(
                        ErrorCode::Denied,
                        "only the controller resizes the terminal",
                    ));
                }
                state.rows = rows;
                state.cols = cols;
            }
            Ok(self.snapshot(client_id))
        }

        fn screen(&self, _session_id: &str, client_id: &str) -> Result<ScreenView, ApiError> {
            let state = self.lock();
            if !state.clients.iter().any(|id| id == client_id) {
                return Err(ApiError::new(
                    ErrorCode::Denied,
                    "attach before reading the screen",
                ));
            }
            Ok(ScreenView {
                rows: state.rows,
                cols: state.cols,
                contents: String::from_utf8_lossy(&state.typed).into_owned(),
                ..ScreenView::default()
            })
        }

        fn write_raw(
            &self,
            _session_id: &str,
            client_id: &str,
            bytes: &[u8],
        ) -> Result<(), ApiError> {
            let mut state = self.lock();
            if state.controller.as_deref() != Some(client_id) {
                return Err(ApiError::new(
                    ErrorCode::Denied,
                    "only the controller may type into this session",
                ));
            }
            state.typed.extend_from_slice(bytes);
            Ok(())
        }

        fn stop(&self, session_id: &str) -> Result<bool, ApiError> {
            self.lock().stopped.push(session_id.to_string());
            Ok(true)
        }
    }

    const HOSTED: &str = "33333333-3333-4333-8333-333333333333";

    fn hosted_server() -> (Arc<ApiServer>, Arc<FakeHost>) {
        let host = FakeHost::with(vec![facts(HOSTED, SessionState::Idle)]);
        let server = ApiServer::new(Box::new(StaticSource(host.sessions())), None);
        server.attach_host(host.clone() as Arc<dyn SessionHost>);
        (server, host)
    }

    /// Capability negotiation, the direction that matters: a server with no
    /// terminals never advertises the attachment surface, so a client turns
    /// the feature off LOCALLY rather than learning about it from a failed
    /// call. Attaching a host turns it on in the same breath.
    #[test]
    fn the_attach_capability_is_advertised_only_by_a_server_that_owns_terminals() {
        let hostless = ApiServer::new(Box::new(StaticSource(Vec::new())), None);
        assert!(
            !hostless
                .hello()
                .capabilities
                .contains(&Capability::SessionAttach),
            "issue #353's reference server has no terminal to attach to"
        );
        let methods = result(&call(&hostless, Method::ServerCapabilities, Value::Null));
        let listed = serde_json::to_string(&methods["methods"]).expect("serialize");
        assert!(
            !listed.contains("session.attach"),
            "a method a client could never call must not be advertised: {listed}"
        );

        let (server, _host) = hosted_server();
        assert!(
            server
                .hello()
                .capabilities
                .contains(&Capability::SessionAttach)
        );
    }

    /// A hostless server refuses every attachment method loudly, naming the
    /// issue -- the same discipline `session.start` already follows, and for
    /// the same reason: a silent no-op would be worse than no method.
    #[test]
    fn a_server_without_a_host_refuses_every_attachment_method() {
        let server = ApiServer::new(
            Box::new(StaticSource(vec![facts(HOSTED, SessionState::Idle)])),
            None,
        );
        for method in [
            Method::SessionAttach,
            Method::SessionDetach,
            Method::SessionTakeover,
            Method::SessionScreen,
        ] {
            let failure = error(&call(
                &server,
                method,
                json!({"session_id": HOSTED, "client_id": "c1"}),
            ));
            assert_eq!(failure.code, ErrorCode::Unsupported, "{method}");
            assert!(failure.message.contains("#352"), "{method}: {failure}");
        }
    }

    /// The core rule: many observers, at most one controller, and a second
    /// client asking for the seat is refused rather than silently promoted.
    #[test]
    fn many_clients_may_observe_but_only_one_may_control() {
        let (server, _host) = hosted_server();
        let first = result(&call(
            &server,
            Method::SessionAttach,
            json!({"session_id": HOSTED, "client_id": "dash", "mode": "controller", "rows": 40, "cols": 120}),
        ));
        assert_eq!(first["attachment"]["role"], json!("controller"));
        assert_eq!(first["attachment"]["controller"], json!("dash"));

        let watcher = result(&call(
            &server,
            Method::SessionAttach,
            json!({"session_id": HOSTED, "client_id": "watch-1"}),
        ));
        assert_eq!(watcher["attachment"]["role"], json!("observer"));
        let second_watcher = result(&call(
            &server,
            Method::SessionAttach,
            json!({"session_id": HOSTED, "client_id": "watch-2"}),
        ));
        assert_eq!(
            second_watcher["attachment"]["clients"],
            json!(["dash", "watch-1", "watch-2"]),
            "observers are unbounded"
        );
        assert_eq!(second_watcher["attachment"]["controller"], json!("dash"));

        let refused = error(&call(
            &server,
            Method::SessionAttach,
            json!({"session_id": HOSTED, "client_id": "watch-1", "mode": "controller"}),
        ));
        assert_eq!(
            refused.code,
            ErrorCode::Busy,
            "an occupied seat is refused, never quietly handed over: {refused}"
        );
    }

    /// Takeover is explicit (its own method) and visible (one
    /// `controller_changed` event every observer sees). One event per real
    /// change, not per call.
    #[test]
    fn a_takeover_is_explicit_and_announced_to_every_observer() {
        let (server, _host) = hosted_server();
        let _ = call(
            &server,
            Method::SessionAttach,
            json!({"session_id": HOSTED, "client_id": "dash", "mode": "controller"}),
        );
        let before = server.revision();
        // Re-attaching as an observer changes nothing, so it announces
        // nothing.
        let _ = call(
            &server,
            Method::SessionAttach,
            json!({"session_id": HOSTED, "client_id": "watch"}),
        );
        assert_eq!(
            server.revision(),
            before,
            "an attachment that did not move the seat must not emit an event"
        );

        let taken = result(&call(
            &server,
            Method::SessionTakeover,
            json!({"session_id": HOSTED, "client_id": "watch"}),
        ));
        assert_eq!(taken["attachment"]["controller"], json!("watch"));
        let announced = server
            .frozen_events()
            .into_iter()
            .filter(|frame| {
                matches!(frame.payload, ApiEvent::ControllerChanged { .. })
                    && frame.session_id.as_deref() == Some(HOSTED)
            })
            .collect::<Vec<_>>();
        assert_eq!(
            announced.len(),
            2,
            "one grant and one takeover, each announced exactly once: {announced:?}"
        );
        assert!(matches!(
            &announced[1].payload,
            ApiEvent::ControllerChanged { controller } if controller.as_deref() == Some("watch")
        ));
    }

    /// Detaching is a CLIENT lifecycle event: the seat empties, but the
    /// session is never stopped. Proven on the host's own stop log, not on a
    /// status field a caller could have set for some other reason.
    #[test]
    fn detaching_releases_the_seat_and_never_stops_the_session() {
        let (server, host) = hosted_server();
        let _ = call(
            &server,
            Method::SessionAttach,
            json!({"session_id": HOSTED, "client_id": "dash", "mode": "controller"}),
        );
        let detached = result(&call(
            &server,
            Method::SessionDetach,
            json!({"session_id": HOSTED, "client_id": "dash"}),
        ));
        assert_eq!(detached["attachment"]["role"], json!("detached"));
        assert_eq!(detached["attachment"]["controller"], Value::Null);
        assert!(
            host.lock().stopped.is_empty(),
            "a detach must never reach the termination ladder"
        );

        // ... and `stop` is what does, distinctly.
        let stopped = result(&call(
            &server,
            Method::SessionStop,
            json!({"session_id": HOSTED}),
        ));
        assert_eq!(stopped["stopped"], json!(true));
        assert_eq!(host.lock().stopped, vec![HOSTED.to_string()]);
    }

    /// Raw keystrokes and resize both belong to the controller alone: an
    /// observer that tries either is refused, and after a takeover the roles
    /// swap without either client having to reattach.
    #[test]
    fn only_the_controller_types_into_or_resizes_a_session() {
        let (server, host) = hosted_server();
        let _ = call(
            &server,
            Method::SessionAttach,
            json!({"session_id": HOSTED, "client_id": "dash", "mode": "controller"}),
        );
        let _ = call(
            &server,
            Method::SessionAttach,
            json!({"session_id": HOSTED, "client_id": "watch"}),
        );

        let refused = error(&call(
            &server,
            Method::SessionSendInput,
            json!({"session_id": HOSTED, "client_id": "watch", "input": "ls\r", "mode": "raw"}),
        ));
        assert_eq!(refused.code, ErrorCode::Denied, "{refused}");
        let refused = error(&call(
            &server,
            Method::SessionResize,
            json!({"session_id": HOSTED, "client_id": "watch", "rows": 10, "cols": 10}),
        ));
        assert_eq!(refused.code, ErrorCode::Denied, "{refused}");

        let accepted = result(&call(
            &server,
            Method::SessionSendInput,
            json!({"session_id": HOSTED, "client_id": "dash", "input": "ls\r", "mode": "raw"}),
        ));
        assert_eq!(accepted["accepted"], json!(true));
        assert_eq!(host.lock().typed, b"ls\r".to_vec());

        let _ = call(
            &server,
            Method::SessionTakeover,
            json!({"session_id": HOSTED, "client_id": "watch"}),
        );
        let resized = result(&call(
            &server,
            Method::SessionResize,
            json!({"session_id": HOSTED, "client_id": "watch", "rows": 50, "cols": 200}),
        ));
        assert_eq!(resized["attachment"]["rows"], json!(50));
    }

    /// `mode: raw` without a `client_id` is a parameter error, not an
    /// unauthenticated write: there is no "the caller must be the controller"
    /// check that can pass when nobody said who the caller is.
    #[test]
    fn raw_input_without_a_client_id_is_refused_rather_than_attributed() {
        let (server, host) = hosted_server();
        let _ = call(
            &server,
            Method::SessionAttach,
            json!({"session_id": HOSTED, "client_id": "dash", "mode": "controller"}),
        );
        let failure = error(&call(
            &server,
            Method::SessionSendInput,
            json!({"session_id": HOSTED, "input": "rm -rf /\r", "mode": "raw"}),
        ));
        assert_eq!(failure.code, ErrorCode::InvalidParams, "{failure}");
        assert!(host.lock().typed.is_empty(), "nothing may have been typed");
    }

    /// The screen is reachable only by a client that actually attached, and
    /// what comes back is the rendered terminal -- not a snapshot field.
    #[test]
    fn the_screen_needs_an_attachment_and_never_leaks_into_a_snapshot() {
        let (server, _host) = hosted_server();
        let failure = error(&call(
            &server,
            Method::SessionScreen,
            json!({"session_id": HOSTED, "client_id": "stranger"}),
        ));
        assert_eq!(failure.code, ErrorCode::Denied, "{failure}");

        let _ = call(
            &server,
            Method::SessionAttach,
            json!({"session_id": HOSTED, "client_id": "dash", "mode": "controller"}),
        );
        let _ = call(
            &server,
            Method::SessionSendInput,
            json!({"session_id": HOSTED, "client_id": "dash", "input": "secret", "mode": "raw"}),
        );
        let view = result(&call(
            &server,
            Method::SessionScreen,
            json!({"session_id": HOSTED, "client_id": "dash"}),
        ));
        assert_eq!(view["screen"]["contents"], json!("secret"));

        let snapshot = serde_json::to_string(&result(&call(
            &server,
            Method::SessionSnapshot,
            Value::Null,
        )))
        .expect("serialize");
        assert!(
            !snapshot.contains("secret"),
            "terminal contents must never ride a snapshot: {snapshot}"
        );
    }

    /// An attach pinned to a generation the session has moved past is refused
    /// before the host is touched -- the same rule every other mutation
    /// follows, so a client cannot attach to a replacement by accident.
    #[test]
    fn an_attach_pinned_to_a_stale_generation_never_reaches_the_host() {
        let (server, host) = hosted_server();
        let failure = error(&call(
            &server,
            Method::SessionAttach,
            json!({"session_id": HOSTED, "client_id": "dash", "generation": 7}),
        ));
        assert_eq!(failure.code, ErrorCode::StaleGeneration, "{failure}");
        assert!(host.lock().clients.is_empty());
    }
}
