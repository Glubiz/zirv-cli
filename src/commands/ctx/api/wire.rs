//! Runtime protocol v1 wire shapes (issue #353).
//!
//! This is the PUBLIC, versioned local API -- the one a durable subscriber or
//! an alternate client is allowed to depend on. It is deliberately separate
//! from `runtime::protocol`, which is the in-process seam between zirv's own
//! supervision code and a [`crate::commands::ctx::runtime::RuntimeBackend`]:
//! that module's envelopes carry backend-shaped values (a whole
//! `SessionHandle`, a `SessionSpec` with a prompt in it) that a third party
//! has no business seeing or forging. `api::wire` carries only the redacted
//! session FACTS ([`SessionFacts`]) and the small method vocabulary
//! [`METHODS`] pins.
//!
//! Compatibility rules, all enforced by tests in this module and in
//! `super::server`:
//!
//! - Unknown fields are ignored (nothing here sets `deny_unknown_fields`).
//! - Every published enum vocabulary has an `unknown` fallback
//!   (`#[serde(other)]`), so a frame written by a newer build still parses.
//! - Every frame carries `v` (the protocol version) and every server frame
//!   carries the server `revision` it was produced at.
//! - `#[serde(flatten)]` is not used anywhere on this wire: the frame kind is
//!   an ordinary internally-tagged `type` field over plain nested structs, so
//!   the shapes stay trivially parseable by a non-serde client too.

use serde::{Deserialize, Serialize};

use crate::commands::ctx::runtime::{RuntimeKind, UiSurface};

/// The one version this build speaks. Bumped only by a breaking change to
/// the shapes in this module; additive fields and additive
/// [`Capability`]/[`Method`] values are v1-compatible by construction
/// because every reader ignores unknown fields and every vocabulary has an
/// `unknown` fallback.
pub const PROTOCOL_VERSION: u32 = 1;

/// `server` in a [`Hello`]: what a client is talking to, so a stray
/// connection to somebody else's socket is obvious rather than confusing.
pub const SERVER_NAME: &str = "zirv";

// ---------------------------------------------------------------------------
// Vocabularies
// ---------------------------------------------------------------------------

/// Every method v1 publishes. Deliberately narrow (issue #353: "Add mail,
/// memory, work-group, workflow, layout, and plugin methods only as concrete
/// clients need them").
// `rename_all` only reaches `Unknown`: every real variant carries its own
// explicit wire name, and the fallback must still serialize as the
// lowercase `unknown` every other published vocabulary uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Method {
    #[serde(rename = "server.ping")]
    ServerPing,
    #[serde(rename = "server.capabilities")]
    ServerCapabilities,
    #[serde(rename = "session.snapshot")]
    SessionSnapshot,
    #[serde(rename = "session.list")]
    SessionList,
    #[serde(rename = "session.get")]
    SessionGet,
    #[serde(rename = "session.start")]
    SessionStart,
    #[serde(rename = "session.stop")]
    SessionStop,
    #[serde(rename = "session.read")]
    SessionRead,
    #[serde(rename = "session.send_input")]
    SessionSendInput,
    #[serde(rename = "session.wait")]
    SessionWait,
    #[serde(rename = "session.report_status")]
    SessionReportStatus,
    /// Issue #352: the four attachment verbs plus the screen read. All five
    /// need a runtime that OWNS the session's terminal, so all five are
    /// gated on [`Capability::SessionAttach`], which a server without a
    /// [`super::server::SessionHost`] does not advertise.
    #[serde(rename = "session.attach")]
    SessionAttach,
    #[serde(rename = "session.detach")]
    SessionDetach,
    #[serde(rename = "session.takeover")]
    SessionTakeover,
    #[serde(rename = "session.resize")]
    SessionResize,
    #[serde(rename = "session.screen")]
    SessionScreen,
    #[serde(rename = "events.subscribe")]
    EventsSubscribe,
    /// Forward-compat fallback: a method name this build has never heard of.
    /// Answered with [`ErrorCode::UnknownMethod`], never guessed at.
    #[serde(other)]
    Unknown,
}

impl Method {
    pub fn as_str(self) -> &'static str {
        match self {
            Method::ServerPing => "server.ping",
            Method::ServerCapabilities => "server.capabilities",
            Method::SessionSnapshot => "session.snapshot",
            Method::SessionList => "session.list",
            Method::SessionGet => "session.get",
            Method::SessionStart => "session.start",
            Method::SessionStop => "session.stop",
            Method::SessionRead => "session.read",
            Method::SessionSendInput => "session.send_input",
            Method::SessionWait => "session.wait",
            Method::SessionReportStatus => "session.report_status",
            Method::SessionAttach => "session.attach",
            Method::SessionDetach => "session.detach",
            Method::SessionTakeover => "session.takeover",
            Method::SessionResize => "session.resize",
            Method::SessionScreen => "session.screen",
            Method::EventsSubscribe => "events.subscribe",
            Method::Unknown => "unknown",
        }
    }
}

impl std::fmt::Display for Method {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for Method {
    type Err = std::convert::Infallible;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(METHODS
            .iter()
            .find(|spec| spec.name == s)
            .map(|spec| spec.method)
            .unwrap_or(Method::Unknown))
    }
}

/// A named feature group a server advertises in its [`Hello`] and a client
/// intersects with its own supported set. The negotiation is LOCAL: a client
/// that does not know a capability simply never calls the methods under it,
/// and a client whose capability the server did not advertise disables that
/// feature in itself rather than calling and failing (see
/// `super::client::Negotiated`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Capability {
    /// `server.ping`, `server.capabilities`, `session.snapshot|list|get`,
    /// `session.read`. Always advertised; a server without it is not a v1
    /// server at all.
    #[serde(rename = "session.read")]
    SessionRead,
    /// `session.start|stop|send_input` -- the mutating control surface.
    #[serde(rename = "session.control")]
    SessionControl,
    /// `session.wait`.
    #[serde(rename = "session.wait")]
    SessionWait,
    /// `session.report_status`.
    #[serde(rename = "session.report_status")]
    SessionReportStatus,
    /// Issue #352: `session.attach|detach|takeover|resize|screen` -- the
    /// surface a client needs when the SERVER owns the terminal rather than
    /// the client. Advertised only by a server with a runtime host attached
    /// (`zirv session serve`), never by the bounded in-process reference
    /// server, so a client negotiates it away instead of discovering the
    /// difference through a failed round trip.
    #[serde(rename = "session.attach")]
    SessionAttach,
    /// `events.subscribe`.
    #[serde(rename = "events.subscribe")]
    EventsSubscribe,
    /// The server honours `idempotency_key` on mutations (a retry returns the
    /// first result instead of doing the work twice).
    #[serde(rename = "idempotency")]
    Idempotency,
    /// Forward-compat fallback: a capability name this build has never heard
    /// of. A client MUST treat it as "not supported by me" rather than as an
    /// error.
    #[serde(other)]
    Unknown,
}

impl Capability {
    pub fn as_str(self) -> &'static str {
        match self {
            Capability::SessionRead => "session.read",
            Capability::SessionControl => "session.control",
            Capability::SessionWait => "session.wait",
            Capability::SessionReportStatus => "session.report_status",
            Capability::SessionAttach => "session.attach",
            Capability::EventsSubscribe => "events.subscribe",
            Capability::Idempotency => "idempotency",
            Capability::Unknown => "unknown",
        }
    }
}

impl std::fmt::Display for Capability {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Structured failure classes. A client branches on `code`, never on
/// `message` text.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    /// `v` on the request is not [`PROTOCOL_VERSION`].
    VersionMismatch,
    /// The `method` name is not one this build knows.
    UnknownMethod,
    /// The method is known but its `params` could not be read.
    InvalidParams,
    /// No such session id in server state.
    UnknownSession,
    /// The caller pinned a generation the session has since moved past.
    StaleGeneration,
    /// A turn is already in flight for this session.
    Busy,
    /// A real method whose backing capability this server does not have --
    /// e.g. a mutation on a server with no runtime backend attached.
    Unsupported,
    /// A bounded wait ran out.
    Timeout,
    /// Refused by the endpoint's own access rules (the peer is not the
    /// endpoint's owner).
    Denied,
    /// Anything else, including a server-side I/O failure.
    Internal,
    /// Forward-compat fallback for a code this build has never heard of.
    #[serde(other)]
    Unknown,
}

/// The redacted lifecycle state of a session, as the protocol publishes it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionState {
    Starting,
    Idle,
    Working,
    Ended,
    #[default]
    #[serde(other)]
    Unknown,
}

/// What `session.wait` is waiting for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WaitUntil {
    /// The session has no turn in flight ([`SessionState::Idle`]).
    #[default]
    Idle,
    /// The session has ended.
    Ended,
    #[serde(other)]
    Unknown,
}

/// How `session.send_input` delivers its text.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InputMode {
    /// A new turn.
    #[default]
    Submit,
    /// Mid-turn steering, where the backend supports it.
    Steer,
    /// Issue #352: the literal bytes go to the session's terminal, exactly as
    /// typed -- control characters, arrow keys and all. Only the CONTROLLER
    /// of an attached session may send this, and only a server with a
    /// runtime host has a terminal to send it to; every other server answers
    /// `unsupported`. It is a separate mode rather than a separate method
    /// because a raw keystroke is still "input for this session", and giving
    /// it its own method would mean a second authorization path to keep in
    /// step with this one.
    Raw,
    #[serde(other)]
    Unknown,
}

// ---------------------------------------------------------------------------
// Attachment (issue #352)
// ---------------------------------------------------------------------------

/// What a client asks to be when it attaches. Many clients may observe one
/// session; at most one may control it, and taking control away from another
/// client is `session.takeover`, never a side effect of attaching.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AttachMode {
    /// Read the screen, send nothing. The default, deliberately: an
    /// attachment that silently seized the keyboard from whoever was already
    /// typing would be the opposite of "takeover is explicit and visible".
    #[default]
    Observer,
    /// Ask for the controller seat. Granted only when the session has no
    /// controller; an occupied seat is refused with [`ErrorCode::Busy`] and
    /// the caller can then choose `session.takeover`.
    Controller,
    #[serde(other)]
    Unknown,
}

/// Who is attached to one session right now. Returned by every attachment
/// method so a client always learns the outcome of its own call and the
/// current occupant in the same breath.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Attachment {
    /// The client id holding terminal input and resize, if any.
    #[serde(default)]
    pub controller: Option<String>,
    /// Every attached client id, controller included, sorted.
    #[serde(default)]
    pub clients: Vec<String>,
    /// The size the server-owned terminal is currently at. A client whose own
    /// window is smaller renders a clipped view rather than resizing a
    /// terminal it does not control.
    pub rows: u16,
    pub cols: u16,
    /// What the CALLER ended up as after this call.
    pub role: AttachRole,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AttachRole {
    Controller,
    #[default]
    Observer,
    /// Not attached at all -- what `session.detach` reports back.
    Detached,
    #[serde(other)]
    Unknown,
}

/// The rendered terminal state of one server-owned session: exactly what a
/// reattaching client must paint to look like it never left.
///
/// `contents` is the vt100 parser's own formatted rendering (cells plus the
/// SGR escapes that colour them), not a transcript and not scrollback
/// history. It is reachable ONLY through `session.screen`, only on a server
/// with a runtime host, and only for a session the caller is attached to --
/// it is deliberately NOT part of [`SessionFacts`], so no snapshot, list or
/// event ever carries terminal output.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct ScreenView {
    pub rows: u16,
    pub cols: u16,
    pub cursor_row: u16,
    pub cursor_col: u16,
    pub cursor_visible: bool,
    /// Whether the child has switched to the alternate screen (a full-screen
    /// TUI). A client must not paint its own chrome over one.
    pub alternate: bool,
    pub contents: String,
}

// ---------------------------------------------------------------------------
// Session facts: what a client is allowed to see
// ---------------------------------------------------------------------------

/// The ONLY session shape this protocol publishes. Deliberately carries no
/// transcript path or body, no prompt, no mail body, no terminal history, no
/// absolute repository path and no credential: issue #353's "do not expose
/// transcript bodies, secrets, mail bodies, or terminal history in snapshots
/// by default" is enforced by this type existing rather than by a filter
/// somebody has to remember to apply. `repo_slug` is the same sanitised slug
/// `state::repo_slug` already derives, never the path itself.
///
/// `session_id` is the stable zirv session uuid. It is independent of panes,
/// tabs, workspaces, cwd slugs and client attachment: nothing in this struct
/// is derived from a client, and `surface` (which UI is looking at it) is a
/// separate field precisely so attaching or detaching a client changes only
/// that field.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionFacts {
    pub session_id: String,
    pub short: String,
    pub runtime: RuntimeKind,
    pub generation: u64,
    pub surface: UiSurface,
    pub state: SessionState,
    #[serde(default)]
    pub role: Option<String>,
    #[serde(default)]
    pub agent: Option<String>,
    #[serde(default)]
    pub repo_slug: Option<String>,
    #[serde(default)]
    pub started_at: Option<u64>,
    #[serde(default)]
    pub reachable: bool,
}

impl SessionFacts {
    /// A minimal fact record for `session_id`, everything else defaulted.
    /// Used by the server's own seeding paths and by tests.
    pub fn new(session_id: impl Into<String>) -> Self {
        let session_id = session_id.into();
        let short = crate::commands::ctx::sessions::short_id(&session_id);
        Self {
            session_id,
            short,
            runtime: RuntimeKind::Harness,
            generation: 1,
            surface: UiSurface::Headless,
            state: SessionState::Unknown,
            role: None,
            agent: None,
            repo_slug: None,
            started_at: None,
            reachable: false,
        }
    }
}

// ---------------------------------------------------------------------------
// Frames
// ---------------------------------------------------------------------------

/// One client -> server frame. NDJSON: exactly one of these per line.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Request {
    #[serde(rename = "v")]
    pub version: u32,
    pub id: String,
    pub method: Method,
    /// Method-specific parameters, parsed by the method itself so the
    /// envelope shape never has to change to add one. Absent is the same as
    /// `null`.
    #[serde(default)]
    pub params: serde_json::Value,
    /// Optional on mutations: a retry carrying the same key returns the
    /// first attempt's result instead of doing the work a second time.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub idempotency_key: Option<String>,
}

impl Request {
    pub fn new(id: impl Into<String>, method: Method, params: serde_json::Value) -> Self {
        Self {
            version: PROTOCOL_VERSION,
            id: id.into(),
            method,
            params,
            idempotency_key: None,
        }
    }

    pub fn with_idempotency_key(mut self, key: impl Into<String>) -> Self {
        self.idempotency_key = Some(key.into());
        self
    }
}

/// The first frame a server writes on every accepted connection, before it
/// reads anything: the version handshake plus the advertised capabilities.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Hello {
    #[serde(rename = "v")]
    pub version: u32,
    pub server: String,
    pub server_version: String,
    pub revision: u64,
    pub capabilities: Vec<Capability>,
}

/// Either half of a [`Response`], internally tagged by `status`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum Outcome {
    Ok {
        result: serde_json::Value,
    },
    Error {
        error: ApiError,
    },
    /// Forward-compat fallback for a status this build has never heard of.
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ApiError {
    pub code: ErrorCode,
    pub message: String,
}

impl ApiError {
    pub fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

impl std::fmt::Display for ApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.code_name(), self.message)
    }
}

impl ApiError {
    pub fn code_name(&self) -> String {
        serde_json::to_value(self.code)
            .ok()
            .and_then(|value| value.as_str().map(str::to_string))
            .unwrap_or_else(|| "unknown".to_string())
    }
}

impl std::error::Error for ApiError {}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Response {
    #[serde(rename = "v")]
    pub version: u32,
    /// Echoes the request's own `id`, so a client can match a reply to its
    /// call on a connection carrying interleaved event frames.
    pub id: String,
    /// The server revision at the moment this reply was produced.
    pub revision: u64,
    pub outcome: Outcome,
}

/// The payload half of an [`EventFrame`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ApiEvent {
    SessionStarted {
        session: SessionFacts,
    },
    SessionUpdated {
        session: SessionFacts,
    },
    SessionEnded {
        session_id: String,
    },
    /// Issue #352: the controller seat for a session changed hands. Emitted
    /// on every grant, release and takeover, so a takeover is VISIBLE to
    /// every observer rather than only to the two clients involved.
    /// `controller` is `null` when the seat is now empty.
    ControllerChanged {
        #[serde(default)]
        controller: Option<String>,
    },
    /// Emitted only when a subscriber asks for it; exists so a long-idle
    /// subscription proves the connection is still alive.
    Heartbeat,
    /// Forward-compat fallback for an event kind this build has never heard
    /// of.
    #[serde(other)]
    Unknown,
}

/// One server -> client event. `revision` is the SERVER-WIDE revision, and
/// the server increments it by exactly one per emitted event: a subscriber
/// that sees `revision != last + 1` has missed events and must refresh a
/// snapshot (`session.snapshot`) rather than drift silently. See
/// [`super::client::GapTracker`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EventFrame {
    #[serde(rename = "v")]
    pub version: u32,
    pub revision: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    /// The session generation this event was recorded against, so a
    /// subscriber can tell an event belonging to a superseded session apart
    /// from a current one even when both share `session_id`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub generation: Option<u64>,
    pub payload: ApiEvent,
}

/// Everything a server can write. Internally tagged by `type`, so a client
/// reading NDJSON can classify a line before it knows anything else.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerFrame {
    Hello(Hello),
    Response(Response),
    Event(EventFrame),
    /// Forward-compat fallback for a frame type this build has never heard
    /// of. A client MUST skip it, not fail the connection.
    #[serde(other)]
    Unknown,
}

// ---------------------------------------------------------------------------
// The method table the schema is generated from
// ---------------------------------------------------------------------------

/// One parameter or result field, as `zirv ctx api schema` publishes it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FieldSpec {
    pub name: &'static str,
    /// A wire type name: `string`, `integer`, `boolean`, `object`, `array`,
    /// or one of the published vocabularies by name (`session_state`,
    /// `wait_until`, `input_mode`, `runtime_kind`, `ui_surface`,
    /// `session_facts`).
    pub ty: &'static str,
    pub required: bool,
    pub doc: &'static str,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MethodSpec {
    pub method: Method,
    pub name: &'static str,
    pub summary: &'static str,
    /// Whether this method changes server state -- i.e. whether
    /// `idempotency_key` is honoured on it.
    pub mutation: bool,
    pub capability: Capability,
    pub params: &'static [FieldSpec],
    pub result: &'static [FieldSpec],
}

const SESSION_ID: FieldSpec = FieldSpec {
    name: "session_id",
    ty: "string",
    required: true,
    doc: "stable zirv session id",
};
const GENERATION_IN: FieldSpec = FieldSpec {
    name: "generation",
    ty: "integer",
    required: false,
    doc: "pin the call to this session generation; a newer one is refused with stale_generation",
};
/// Issue #352. Caller-chosen and caller-owned: the server never invents one,
/// so a client that reconnects under the same id resumes its own attachment
/// rather than accumulating ghosts.
const CLIENT_ID: FieldSpec = FieldSpec {
    name: "client_id",
    ty: "string",
    required: true,
    doc: "the calling client's own stable id",
};
const ATTACH_MODE: FieldSpec = FieldSpec {
    name: "mode",
    ty: "attach_mode",
    required: false,
    doc: "observer (default) or controller",
};
const ROWS_IN: FieldSpec = FieldSpec {
    name: "rows",
    ty: "integer",
    required: false,
    doc: "the controller's own terminal height; ignored for an observer",
};
const COLS_IN: FieldSpec = FieldSpec {
    name: "cols",
    ty: "integer",
    required: false,
    doc: "the controller's own terminal width; ignored for an observer",
};
const ATTACHMENT_OUT: FieldSpec = FieldSpec {
    name: "attachment",
    ty: "attachment",
    required: true,
    doc: "controller, every attached client id, the terminal size, and the caller's own role",
};

/// Every method this build serves, in the order `zirv ctx api schema` prints
/// them. `method_specs_cover_every_method` pins this table against the
/// [`Method`] enum, so a method cannot be added without a published shape.
pub static METHODS: &[MethodSpec] = &[
    MethodSpec {
        method: Method::ServerPing,
        name: "server.ping",
        summary: "Liveness plus the server's own identity and protocol version.",
        mutation: false,
        capability: Capability::SessionRead,
        params: &[],
        result: &[
            FieldSpec {
                name: "server",
                ty: "string",
                required: true,
                doc: "always \"zirv\"",
            },
            FieldSpec {
                name: "server_version",
                ty: "string",
                required: true,
                doc: "the binary's crate version",
            },
            FieldSpec {
                name: "protocol",
                ty: "integer",
                required: true,
                doc: "protocol version, 1",
            },
        ],
    },
    MethodSpec {
        method: Method::ServerCapabilities,
        name: "server.capabilities",
        summary: "The capabilities this server advertises, and the runtime backend's own.",
        mutation: false,
        capability: Capability::SessionRead,
        params: &[],
        result: &[
            FieldSpec {
                name: "protocol",
                ty: "integer",
                required: true,
                doc: "protocol version, 1",
            },
            FieldSpec {
                name: "capabilities",
                ty: "array",
                required: true,
                doc: "capability names",
            },
            FieldSpec {
                name: "methods",
                ty: "array",
                required: true,
                doc: "method names this server serves",
            },
            FieldSpec {
                name: "runtime",
                ty: "object",
                required: false,
                doc: "the attached RuntimeBackend's capabilities, absent when none is attached",
            },
        ],
    },
    MethodSpec {
        method: Method::SessionSnapshot,
        name: "session.snapshot",
        summary: "The full session set plus the revision it is current as of -- the recovery call after an event gap.",
        mutation: false,
        capability: Capability::SessionRead,
        params: &[],
        result: &[
            FieldSpec {
                name: "revision",
                ty: "integer",
                required: true,
                doc: "server revision this snapshot is current as of",
            },
            FieldSpec {
                name: "sessions",
                ty: "array",
                required: true,
                doc: "session_facts objects",
            },
        ],
    },
    MethodSpec {
        method: Method::SessionList,
        name: "session.list",
        summary: "Session facts, optionally filtered by state.",
        mutation: false,
        capability: Capability::SessionRead,
        params: &[FieldSpec {
            name: "state",
            ty: "session_state",
            required: false,
            doc: "keep only sessions in this state",
        }],
        result: &[FieldSpec {
            name: "sessions",
            ty: "array",
            required: true,
            doc: "session_facts objects",
        }],
    },
    MethodSpec {
        method: Method::SessionGet,
        name: "session.get",
        summary: "One session's facts.",
        mutation: false,
        capability: Capability::SessionRead,
        params: &[SESSION_ID],
        result: &[FieldSpec {
            name: "session",
            ty: "session_facts",
            required: true,
            doc: "",
        }],
    },
    MethodSpec {
        method: Method::SessionStart,
        name: "session.start",
        summary: "Start a session on the attached runtime backend.",
        mutation: true,
        capability: Capability::SessionControl,
        params: &[
            FieldSpec {
                name: "runtime",
                ty: "runtime_kind",
                required: false,
                doc: "which backend, default harness",
            },
            FieldSpec {
                name: "role",
                ty: "string",
                required: false,
                doc: "seat role label",
            },
            FieldSpec {
                name: "agent",
                ty: "string",
                required: false,
                doc: "harness name",
            },
            FieldSpec {
                name: "model",
                ty: "string",
                required: false,
                doc: "",
            },
            FieldSpec {
                name: "surface",
                ty: "ui_surface",
                required: false,
                doc: "which UI is attached",
            },
            FieldSpec {
                name: "cwd",
                ty: "string",
                required: true,
                doc: "working directory",
            },
            FieldSpec {
                name: "prompt",
                ty: "string",
                required: true,
                doc: "the launch prompt; never echoed back in session facts",
            },
            // Issue #352: what an operator wrote after `--`. Optional and
            // defaulted, so every v1 caller that never sent it -- including
            // the frozen fixtures -- is unaffected; a server that ignored it
            // would silently drop the flags a `zirv chat -- --foo` launch
            // depends on.
            FieldSpec {
                name: "extra_args",
                ty: "string[]",
                required: false,
                doc: "extra arguments for the agent, after the adapter's own",
            },
        ],
        result: &[FieldSpec {
            name: "session",
            ty: "session_facts",
            required: true,
            doc: "",
        }],
    },
    MethodSpec {
        method: Method::SessionStop,
        name: "session.stop",
        summary: "Interrupt a session and mark it ended.",
        mutation: true,
        capability: Capability::SessionControl,
        params: &[SESSION_ID, GENERATION_IN],
        result: &[FieldSpec {
            name: "stopped",
            ty: "boolean",
            required: true,
            doc: "false when the session was already ended",
        }],
    },
    MethodSpec {
        method: Method::SessionRead,
        name: "session.read",
        summary: "Replay the events recorded for one session after a revision. Never transcript bodies.",
        mutation: false,
        capability: Capability::SessionRead,
        params: &[
            SESSION_ID,
            FieldSpec {
                name: "after_revision",
                ty: "integer",
                required: false,
                doc: "return events strictly newer than this, default 0",
            },
        ],
        result: &[
            FieldSpec {
                name: "revision",
                ty: "integer",
                required: true,
                doc: "current server revision",
            },
            FieldSpec {
                name: "events",
                ty: "array",
                required: true,
                doc: "event frames",
            },
        ],
    },
    MethodSpec {
        method: Method::SessionSendInput,
        name: "session.send_input",
        summary: "Send text to a session as a new turn or as mid-turn steering.",
        mutation: true,
        capability: Capability::SessionControl,
        params: &[
            SESSION_ID,
            GENERATION_IN,
            FieldSpec {
                name: "input",
                ty: "string",
                required: true,
                doc: "the text to deliver",
            },
            FieldSpec {
                name: "mode",
                ty: "input_mode",
                required: false,
                doc: "submit (default), steer, or raw (the controller's literal terminal bytes)",
            },
            FieldSpec {
                name: "client_id",
                ty: "string",
                required: false,
                doc: "required for mode=raw: only the session's controller may type into it",
            },
        ],
        result: &[FieldSpec {
            name: "accepted",
            ty: "boolean",
            required: true,
            doc: "",
        }],
    },
    MethodSpec {
        method: Method::SessionWait,
        name: "session.wait",
        summary: "Block until a session reaches a state, pinned to the generation resolved at call time.",
        mutation: false,
        capability: Capability::SessionWait,
        params: &[
            SESSION_ID,
            GENERATION_IN,
            FieldSpec {
                name: "until",
                ty: "wait_until",
                required: false,
                doc: "idle (default) or ended",
            },
            FieldSpec {
                name: "timeout_ms",
                ty: "integer",
                required: false,
                doc: "bounded wait, default 30000",
            },
        ],
        result: &[
            FieldSpec {
                name: "outcome",
                ty: "string",
                required: true,
                doc: "matched or timeout",
            },
            FieldSpec {
                name: "state",
                ty: "session_state",
                required: true,
                doc: "the state observed when the wait returned",
            },
            FieldSpec {
                name: "generation",
                ty: "integer",
                required: true,
                doc: "the generation the wait was pinned to",
            },
        ],
    },
    MethodSpec {
        method: Method::SessionReportStatus,
        name: "session.report_status",
        summary: "A client reports the lifecycle state it observes for a session it drives.",
        mutation: true,
        capability: Capability::SessionReportStatus,
        params: &[
            SESSION_ID,
            GENERATION_IN,
            FieldSpec {
                name: "state",
                ty: "session_state",
                required: true,
                doc: "the state to record",
            },
        ],
        result: &[FieldSpec {
            name: "recorded",
            ty: "boolean",
            required: true,
            doc: "",
        }],
    },
    MethodSpec {
        method: Method::SessionAttach,
        name: "session.attach",
        summary: "Attach a client to a server-owned session as an observer or (if the seat is free) its controller.",
        mutation: true,
        capability: Capability::SessionAttach,
        params: &[
            SESSION_ID,
            GENERATION_IN,
            CLIENT_ID,
            ATTACH_MODE,
            ROWS_IN,
            COLS_IN,
        ],
        result: &[ATTACHMENT_OUT],
    },
    MethodSpec {
        method: Method::SessionDetach,
        name: "session.detach",
        summary: "Detach one client. The session, its process and its supervisor keep running.",
        mutation: true,
        capability: Capability::SessionAttach,
        params: &[SESSION_ID, CLIENT_ID],
        result: &[ATTACHMENT_OUT],
    },
    MethodSpec {
        method: Method::SessionTakeover,
        name: "session.takeover",
        summary: "Take the controller seat from whoever holds it. Always emits controller_changed.",
        mutation: true,
        capability: Capability::SessionAttach,
        params: &[SESSION_ID, CLIENT_ID],
        result: &[ATTACHMENT_OUT],
    },
    MethodSpec {
        method: Method::SessionResize,
        name: "session.resize",
        summary: "Resize the server-owned terminal. Controller only -- an observer's window never moves it.",
        mutation: true,
        capability: Capability::SessionAttach,
        params: &[
            SESSION_ID,
            CLIENT_ID,
            FieldSpec {
                name: "rows",
                ty: "integer",
                required: true,
                doc: "",
            },
            FieldSpec {
                name: "cols",
                ty: "integer",
                required: true,
                doc: "",
            },
        ],
        result: &[ATTACHMENT_OUT],
    },
    MethodSpec {
        method: Method::SessionScreen,
        name: "session.screen",
        summary: "The session's current rendered terminal state, for a client that just (re)attached.",
        mutation: false,
        capability: Capability::SessionAttach,
        params: &[SESSION_ID, CLIENT_ID],
        result: &[
            FieldSpec {
                name: "revision",
                ty: "integer",
                required: true,
                doc: "current server revision",
            },
            FieldSpec {
                name: "screen",
                ty: "screen_view",
                required: true,
                doc: "rendered cells plus cursor; never scrollback, a transcript or a prompt",
            },
        ],
    },
    MethodSpec {
        method: Method::EventsSubscribe,
        name: "events.subscribe",
        summary: "Stream event frames on this connection from after_revision onward until it closes.",
        mutation: false,
        capability: Capability::EventsSubscribe,
        params: &[FieldSpec {
            name: "after_revision",
            ty: "integer",
            required: false,
            doc: "replay events strictly newer than this first; default 0 replays everything retained",
        }],
        result: &[
            FieldSpec {
                name: "subscribed",
                ty: "boolean",
                required: true,
                doc: "",
            },
            FieldSpec {
                name: "revision",
                ty: "integer",
                required: true,
                doc: "server revision at subscription time",
            },
        ],
    },
];

/// Every capability protocol v1 defines. NOT what a given server advertises:
/// [`Capability::SessionAttach`] is advertised only by a server that actually
/// owns terminals (issue #352's `zirv session serve`), which is why
/// [`super::server::ApiServer::advertised`] filters this list rather than
/// handing it out wholesale. The schema prints this one, because the schema
/// documents the PROTOCOL; a `hello` frame carries the filtered one, because
/// it documents the SERVER.
pub static ADVERTISED: &[Capability] = &[
    Capability::SessionRead,
    Capability::SessionControl,
    Capability::SessionWait,
    Capability::SessionReportStatus,
    Capability::SessionAttach,
    Capability::EventsSubscribe,
    Capability::Idempotency,
];

/// What a server with no [`super::server::SessionHost`] advertises: every
/// capability except the attachment surface it has no terminal to serve.
pub static ADVERTISED_WITHOUT_HOST: &[Capability] = &[
    Capability::SessionRead,
    Capability::SessionControl,
    Capability::SessionWait,
    Capability::SessionReportStatus,
    Capability::EventsSubscribe,
    Capability::Idempotency,
];

pub fn spec_for(method: Method) -> Option<&'static MethodSpec> {
    METHODS.iter().find(|spec| spec.method == method)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The schema is generated from [`METHODS`], so a method the enum knows
    /// but the table does not would be an undocumented method on a
    /// documented protocol. The `Unknown` fallback is the one deliberate
    /// exclusion: it is not a method, it is what an unrecognized name parses
    /// to.
    #[test]
    fn method_specs_cover_every_method() {
        let every = [
            Method::ServerPing,
            Method::ServerCapabilities,
            Method::SessionSnapshot,
            Method::SessionList,
            Method::SessionGet,
            Method::SessionStart,
            Method::SessionStop,
            Method::SessionRead,
            Method::SessionSendInput,
            Method::SessionWait,
            Method::SessionReportStatus,
            Method::SessionAttach,
            Method::SessionDetach,
            Method::SessionTakeover,
            Method::SessionResize,
            Method::SessionScreen,
            Method::EventsSubscribe,
        ];
        for method in every {
            let spec = spec_for(method).unwrap_or_else(|| panic!("no spec for {method}"));
            assert_eq!(
                spec.name,
                method.as_str(),
                "spec name must be the wire name"
            );
        }
        assert_eq!(METHODS.len(), every.len(), "the table has an extra entry");
        assert!(spec_for(Method::Unknown).is_none());
    }

    #[test]
    fn every_method_name_round_trips_through_from_str_and_serde() {
        for spec in METHODS {
            let parsed: Method = spec.name.parse().expect("infallible");
            assert_eq!(parsed, spec.method);
            let json = serde_json::to_string(&spec.method).expect("serialize");
            assert_eq!(json, format!("\"{}\"", spec.name));
        }
        assert_eq!("no.such.method".parse::<Method>(), Ok(Method::Unknown));
    }

    /// Issue #353's forward-compat rule, proven on each published vocabulary
    /// separately because each is a separate derived impl.
    #[test]
    fn every_published_vocabulary_has_an_unknown_fallback() {
        assert_eq!(
            serde_json::from_str::<Method>("\"levitate\"").expect("parse"),
            Method::Unknown
        );
        assert_eq!(
            serde_json::from_str::<Capability>("\"levitate\"").expect("parse"),
            Capability::Unknown
        );
        assert_eq!(
            serde_json::from_str::<ErrorCode>("\"levitate\"").expect("parse"),
            ErrorCode::Unknown
        );
        assert_eq!(
            serde_json::from_str::<SessionState>("\"levitate\"").expect("parse"),
            SessionState::Unknown
        );
        assert_eq!(
            serde_json::from_str::<WaitUntil>("\"levitate\"").expect("parse"),
            WaitUntil::Unknown
        );
        assert_eq!(
            serde_json::from_str::<InputMode>("\"levitate\"").expect("parse"),
            InputMode::Unknown
        );
        assert_eq!(
            serde_json::from_str::<AttachMode>("\"levitate\"").expect("parse"),
            AttachMode::Unknown
        );
        assert_eq!(
            serde_json::from_str::<AttachRole>("\"levitate\"").expect("parse"),
            AttachRole::Unknown
        );
        assert_eq!(
            serde_json::from_str::<ApiEvent>(r#"{"kind":"levitated","extra":1}"#).expect("parse"),
            ApiEvent::Unknown
        );
        assert_eq!(
            serde_json::from_str::<ServerFrame>(r#"{"type":"levitation","extra":1}"#)
                .expect("parse"),
            ServerFrame::Unknown
        );
        assert_eq!(
            serde_json::from_str::<Outcome>(r#"{"status":"levitating"}"#).expect("parse"),
            Outcome::Unknown
        );
    }

    /// Unknown fields are ignored on the envelopes too, not just the
    /// vocabularies: a v1 client must keep parsing a frame a later build
    /// added a field to.
    #[test]
    fn unknown_fields_on_a_request_are_ignored() {
        let request: Request = serde_json::from_str(
            r#"{"v":1,"id":"r1","method":"server.ping","tomorrows_field":"surprise"}"#,
        )
        .expect("parse");
        assert_eq!(request.method, Method::ServerPing);
        assert_eq!(request.params, serde_json::Value::Null);
        assert_eq!(request.idempotency_key, None);
    }

    /// `SessionFacts` is the privacy boundary: the wire never carries a
    /// transcript, a prompt, a mail body, a terminal buffer or an absolute
    /// repository path. Pinned as a property over the serialized key set so
    /// adding such a field to the struct fails here rather than in review.
    #[test]
    fn session_facts_publish_no_bodies_secrets_or_absolute_paths() {
        let facts = SessionFacts::new("11111111-2222-4333-8444-555555555555");
        let value = serde_json::to_value(&facts).expect("serialize");
        let keys: Vec<String> = value.as_object().expect("object").keys().cloned().collect();
        for forbidden in [
            "transcript",
            "prompt",
            "body",
            "message",
            "history",
            "screen",
            "token",
            "secret",
            "credential",
            "cwd",
            "path",
            "env",
        ] {
            assert!(
                !keys.iter().any(|key| key.contains(forbidden)),
                "session facts must not publish a {forbidden} field; keys were {keys:?}"
            );
        }
        assert!(
            keys.iter().any(|key| key == "repo_slug"),
            "the sanitised slug is allowed"
        );
        assert!(
            !keys.iter().any(|key| key == "repo"),
            "the absolute repository path is not"
        );
    }

    /// Issue #352: the attachment surface added a way to READ a terminal, and
    /// the privacy boundary above must stay exactly where it was -- the
    /// screen travels only as `session.screen`'s own result, never folded
    /// into the shape every snapshot, list and event carries.
    #[test]
    fn the_screen_is_reachable_only_through_its_own_method() {
        let attach = spec_for(Method::SessionScreen).expect("spec");
        assert_eq!(attach.capability, Capability::SessionAttach);
        assert!(
            attach.result.iter().any(|field| field.name == "screen"),
            "session.screen is where a rendered terminal is published"
        );
        for method in [
            Method::SessionSnapshot,
            Method::SessionList,
            Method::SessionGet,
            Method::SessionRead,
        ] {
            let spec = spec_for(method).expect("spec");
            assert!(
                !spec.result.iter().any(|field| field.name == "screen"),
                "{method} must not publish terminal contents"
            );
        }
    }

    /// Every attachment method is gated on the one capability a hostless
    /// server does not advertise, so a client's LOCAL negotiation is enough
    /// to know none of them is callable -- no failed round trip required.
    #[test]
    fn every_attachment_method_is_gated_on_the_attach_capability() {
        for method in [
            Method::SessionAttach,
            Method::SessionDetach,
            Method::SessionTakeover,
            Method::SessionResize,
            Method::SessionScreen,
        ] {
            let spec = spec_for(method).expect("spec");
            assert_eq!(spec.capability, Capability::SessionAttach, "{method}");
        }
        assert!(
            !ADVERTISED_WITHOUT_HOST.contains(&Capability::SessionAttach),
            "a server with no terminals must not advertise the attachment surface"
        );
        assert!(ADVERTISED.contains(&Capability::SessionAttach));
        for capability in ADVERTISED_WITHOUT_HOST {
            assert!(
                ADVERTISED.contains(capability),
                "the hostless set must be a subset of the protocol's own: {capability}"
            );
        }
    }

    #[test]
    fn session_facts_short_id_matches_the_session_registry_derivation() {
        let facts = SessionFacts::new("11111111-2222-4333-8444-555555555555");
        assert_eq!(
            facts.short,
            crate::commands::ctx::sessions::short_id("11111111-2222-4333-8444-555555555555")
        );
    }
}
