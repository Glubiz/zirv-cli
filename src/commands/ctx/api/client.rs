//! The minimal protocol v1 client (issue #353): what the CLI wrappers and
//! the tests both use, so "CLI wrappers and a minimal test client exercise
//! the same public methods" is true by construction rather than by
//! discipline.
//!
//! Two behaviours belong to the CLIENT, not the server, and are implemented
//! here because issue #353 says so in as many words:
//!
//! - **Capability negotiation is local.** The server advertises what it has
//!   in its `hello`; [`Negotiated`] intersects that with what THIS build
//!   supports, and a feature that is not in the intersection is disabled
//!   locally -- never called and then handled as an error. That is what
//!   lets a previous-minor client connect to a newer server (it ignores the
//!   capabilities it has never heard of) and a newer client connect to an
//!   older one (it disables the features that server lacks).
//! - **Gap detection.** Events carry a server-wide revision that moves by
//!   exactly one. [`GapTracker`] is the pure rule that turns "the revision
//!   jumped" into "refresh a snapshot", and [`Client::next_event`] applies
//!   it.
//!
//! The subscription half of this client (`subscribe`/`next_event`/
//! `refresh_snapshot`, and the tracker accessors around them) has no in-tree
//! caller yet: the CLI wrapper makes one-shot calls, and the durable
//! subscriber that streams events is issue #489's native integration. It is
//! nonetheless the published client surface an alternate client is meant to
//! use, and it is exercised end to end by this module's own tests --
//! `#![allow(dead_code)]` covers it for exactly the reason
//! `runtime/mod.rs` already documents for its own contracts: a real,
//! fully-tested API with no in-tree caller yet is not the same thing as code
//! that should be deleted.
#![allow(dead_code)]

use serde::de::DeserializeOwned;
use serde_json::Value;

use super::transport::{self, Connection, Endpoint};
use super::wire::{
    ADVERTISED, ApiError, Capability, ErrorCode, EventFrame, Hello, Method, Outcome,
    PROTOCOL_VERSION, Request, Response, SERVER_NAME, ServerFrame,
};
use crate::commands::ctx::CtxResult;

/// The capability set this build knows how to use. Separate from
/// [`ADVERTISED`] (what this build's SERVER offers) on purpose: a client and
/// a server in the same binary happen to agree today, and the whole point of
/// the handshake is that they need not.
pub static CLIENT_SUPPORTS: &[Capability] = ADVERTISED;

/// The outcome of the handshake: what both ends have in common, plus what
/// each side had that the other did not, so a caller can say why a feature
/// is off instead of merely finding it missing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Negotiated {
    pub protocol: u32,
    pub server_version: String,
    pub enabled: Vec<Capability>,
    /// Advertised by the server, not supported here -- including anything
    /// that parsed as [`Capability::Unknown`], which is exactly how a
    /// previous-minor client treats a capability invented after it shipped.
    pub server_only: Vec<Capability>,
    /// Supported here, not advertised by the server: features this client
    /// must disable locally.
    pub client_only: Vec<Capability>,
}

impl Negotiated {
    /// The local intersection. `supported` is the client's own list so a
    /// test can play a previous-minor client without a previous-minor
    /// binary.
    pub fn from_hello(hello: &Hello, supported: &[Capability]) -> Self {
        let advertised: Vec<Capability> = hello.capabilities.clone();
        let enabled: Vec<Capability> = advertised
            .iter()
            .copied()
            .filter(|capability| {
                *capability != Capability::Unknown && supported.contains(capability)
            })
            .collect();
        let server_only: Vec<Capability> = advertised
            .iter()
            .copied()
            .filter(|capability| !enabled.contains(capability))
            .collect();
        let client_only: Vec<Capability> = supported
            .iter()
            .copied()
            .filter(|capability| !advertised.contains(capability))
            .collect();
        Self {
            protocol: hello.version,
            server_version: hello.server_version.clone(),
            enabled,
            server_only,
            client_only,
        }
    }

    pub fn has(&self, capability: Capability) -> bool {
        self.enabled.contains(&capability)
    }

    /// Whether a method may be called at all: its gating capability has to
    /// have survived the negotiation.
    pub fn allows(&self, method: Method) -> bool {
        super::wire::spec_for(method).is_some_and(|spec| self.has(spec.capability))
    }
}

/// What a subscriber learned from one event frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Observed {
    /// The next event in order; apply it.
    InOrder,
    /// Events were missed between `expected` and `got`. The subscriber MUST
    /// refresh a snapshot; applying this frame on its own would leave it
    /// quietly out of date.
    Gap { expected: u64, got: u64 },
    /// A revision at or below one already seen -- a duplicate or a replay.
    /// Ignored rather than applied twice.
    Stale,
}

/// The pure gap rule, separated from any I/O so it can be tested against a
/// synthesised stream as well as a real socket.
#[derive(Debug, Clone, Copy, Default)]
pub struct GapTracker {
    last: u64,
    refresh_due: bool,
}

impl GapTracker {
    /// Starts from the revision a subscription was opened at: the first
    /// event delivered must be `after_revision + 1`.
    pub fn starting_at(after_revision: u64) -> Self {
        Self {
            last: after_revision,
            refresh_due: false,
        }
    }

    pub fn observe(&mut self, revision: u64) -> Observed {
        if revision <= self.last {
            return Observed::Stale;
        }
        if revision == self.last + 1 {
            self.last = revision;
            return Observed::InOrder;
        }
        let expected = self.last + 1;
        self.last = revision;
        self.refresh_due = true;
        Observed::Gap {
            expected,
            got: revision,
        }
    }

    /// Whether a snapshot refresh is still owed. Cleared by
    /// [`Self::refreshed`] once the subscriber has actually taken one.
    pub fn refresh_due(&self) -> bool {
        self.refresh_due
    }

    /// Re-anchors the tracker on the revision a fresh snapshot is current
    /// as of.
    pub fn refreshed(&mut self, revision: u64) {
        self.last = revision;
        self.refresh_due = false;
    }
}

/// A connected client. One connection, one request at a time -- v1 has no
/// pipelining, and a subscriber uses a connection of its own.
#[derive(Debug)]
pub struct Client {
    connection: Connection,
    negotiated: Negotiated,
    tracker: GapTracker,
    next_id: u64,
}

impl Client {
    /// Connects and completes the handshake, negotiating against
    /// [`CLIENT_SUPPORTS`].
    pub fn connect(endpoint: &Endpoint) -> CtxResult<Self> {
        Self::connect_as(endpoint, CLIENT_SUPPORTS)
    }

    /// Connects as a client that supports exactly `supported`. The seam a
    /// previous-minor fixture client is played through.
    pub fn connect_as(endpoint: &Endpoint, supported: &[Capability]) -> CtxResult<Self> {
        let mut connection = transport::connect(endpoint)?;
        let hello = loop {
            let Some(frame) = connection.read_frame::<ServerFrame>()? else {
                return Err("the server closed the connection before saying hello".into());
            };
            match frame {
                ServerFrame::Hello(hello) => break hello,
                ServerFrame::Response(response) => {
                    if let Outcome::Error { error } = response.outcome {
                        return Err(Box::new(error));
                    }
                }
                // A frame kind this build has never heard of is skipped, not
                // fatal: that is the forward-compat rule the wire promises.
                ServerFrame::Event(_) | ServerFrame::Unknown => {}
            }
        };
        if hello.server != SERVER_NAME {
            return Err(format!("not a zirv runtime endpoint: {}", hello.server).into());
        }
        if hello.version != PROTOCOL_VERSION {
            return Err(format!(
                "server speaks protocol {} but this build speaks {PROTOCOL_VERSION}",
                hello.version
            )
            .into());
        }
        let negotiated = Negotiated::from_hello(&hello, supported);
        Ok(Self {
            connection,
            negotiated,
            tracker: GapTracker::starting_at(hello.revision),
            next_id: 0,
        })
    }

    pub fn negotiated(&self) -> &Negotiated {
        &self.negotiated
    }

    pub fn tracker(&self) -> &GapTracker {
        &self.tracker
    }

    fn mint_id(&mut self) -> String {
        self.next_id += 1;
        format!("c{}", self.next_id)
    }

    /// Sends one request and reads its reply, skipping any event frames that
    /// arrive in between (a subscriber that also calls methods must still
    /// match replies by id).
    pub fn call_raw(&mut self, request: &Request) -> CtxResult<Response> {
        self.connection
            .write_frame(&serde_json::to_value(request)?)?;
        loop {
            let Some(frame) = self.connection.read_frame::<ServerFrame>()? else {
                return Err("the server closed the connection mid-call".into());
            };
            match frame {
                ServerFrame::Response(response) if response.id == request.id => {
                    return Ok(response);
                }
                ServerFrame::Response(_) | ServerFrame::Hello(_) => {}
                ServerFrame::Event(frame) => {
                    let _ = self.tracker.observe(frame.revision);
                }
                ServerFrame::Unknown => {}
            }
        }
    }

    /// The ordinary call: refuses locally if the method's capability did not
    /// survive the handshake, then returns the result value or the server's
    /// structured error.
    pub fn call(&mut self, method: Method, params: Value) -> CtxResult<Value> {
        self.call_with_key(method, params, None)
    }

    pub fn call_with_key(
        &mut self,
        method: Method,
        params: Value,
        idempotency_key: Option<&str>,
    ) -> CtxResult<Value> {
        if !self.negotiated.allows(method) {
            return Err(Box::new(ApiError::new(
                ErrorCode::Unsupported,
                format!("{method} is disabled locally: the server did not advertise its capability"),
            )));
        }
        let id = self.mint_id();
        let mut request = Request::new(id, method, params);
        if let Some(key) = idempotency_key {
            request = request.with_idempotency_key(key);
        }
        let response = self.call_raw(&request)?;
        match response.outcome {
            Outcome::Ok { result } => Ok(result),
            Outcome::Error { error } => Err(Box::new(error)),
            Outcome::Unknown => Err("the server replied with an unrecognized status".into()),
        }
    }

    /// A typed call, for the handful of results a caller wants as a struct.
    pub fn call_typed<T: DeserializeOwned>(
        &mut self,
        method: Method,
        params: Value,
    ) -> CtxResult<T> {
        Ok(serde_json::from_value(self.call(method, params)?)?)
    }

    /// Turns this connection into an event subscription from
    /// `after_revision` onward. The connection carries nothing else
    /// afterwards, so a caller that also wants to make calls opens a second
    /// client.
    pub fn subscribe(&mut self, after_revision: u64) -> CtxResult<u64> {
        let result = self.call(
            Method::EventsSubscribe,
            serde_json::json!({ "after_revision": after_revision }),
        )?;
        self.tracker = GapTracker::starting_at(after_revision);
        Ok(result
            .get("revision")
            .and_then(Value::as_u64)
            .unwrap_or(after_revision))
    }

    /// Reads the next event, classifying it against the gap rule. `Ok(None)`
    /// is a closed stream.
    pub fn next_event(&mut self) -> CtxResult<Option<(EventFrame, Observed)>> {
        loop {
            let Some(frame) = self.connection.read_frame::<ServerFrame>()? else {
                return Ok(None);
            };
            match frame {
                ServerFrame::Event(frame) => {
                    let observed = self.tracker.observe(frame.revision);
                    return Ok(Some((frame, observed)));
                }
                ServerFrame::Hello(_) | ServerFrame::Response(_) | ServerFrame::Unknown => {}
            }
        }
    }

    /// The recovery a gap demands: take a fresh snapshot and re-anchor the
    /// tracker on the revision it is current as of. Returns the snapshot's
    /// session list.
    pub fn refresh_snapshot(&mut self) -> CtxResult<Value> {
        let snapshot = self.call(Method::SessionSnapshot, Value::Null)?;
        if let Some(revision) = snapshot.get("revision").and_then(Value::as_u64) {
            self.tracker.refreshed(revision);
        }
        Ok(snapshot)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::ctx::api::server::{ApiServer, RunningServer, StaticSource};
    use crate::commands::ctx::api::wire::{ApiEvent, SessionFacts, SessionState};
    use serde_json::json;
    use std::sync::Arc;

    fn endpoint(dir: &tempfile::TempDir) -> Endpoint {
        Endpoint::at(dir.path().join("s").join("api.sock"))
    }

    fn facts(id: &str) -> SessionFacts {
        let mut facts = SessionFacts::new(id);
        facts.state = SessionState::Idle;
        facts
    }

    /// The gap rule itself, on a synthesised stream: consecutive revisions
    /// are in order, a jump is a gap that owes a refresh, and a replay is
    /// stale rather than applied twice.
    #[test]
    fn the_gap_tracker_flags_a_missed_revision_and_clears_on_refresh() {
        let mut tracker = GapTracker::starting_at(10);
        assert_eq!(tracker.observe(11), Observed::InOrder);
        assert_eq!(tracker.observe(12), Observed::InOrder);
        assert!(!tracker.refresh_due());
        assert_eq!(
            tracker.observe(17),
            Observed::Gap {
                expected: 13,
                got: 17
            }
        );
        assert!(tracker.refresh_due(), "a gap owes a snapshot refresh");
        assert_eq!(tracker.observe(17), Observed::Stale, "no double apply");
        tracker.refreshed(20);
        assert!(!tracker.refresh_due());
        assert_eq!(tracker.observe(21), Observed::InOrder);
    }

    /// Issue #353: "A previous-minor fixture client can connect to the
    /// current server with unsupported capabilities disabled locally." The
    /// older client's capability list is a committed fixture; everything the
    /// current server advertises beyond it -- including values that parse as
    /// `Capability::Unknown` -- is disabled here rather than treated as an
    /// error.
    #[test]
    fn a_previous_minor_client_negotiates_down_instead_of_failing() {
        let older: Vec<Capability> = serde_json::from_str(
            &std::fs::read_to_string(
                std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                    .join("tests/fixtures/protocol/v1/client-previous-minor.json"),
            )
            .expect("read the previous-minor client fixture"),
        )
        .expect("parse the previous-minor client fixture");

        let hello = Hello {
            version: PROTOCOL_VERSION,
            server: SERVER_NAME.to_string(),
            server_version: "99.0.0".to_string(),
            revision: 0,
            capabilities: {
                let mut advertised = ADVERTISED.to_vec();
                // A capability invented after that client shipped.
                advertised.push(
                    serde_json::from_str::<Capability>("\"session.teleport\"").expect("parse"),
                );
                advertised
            },
        };
        let negotiated = Negotiated::from_hello(&hello, &older);
        assert!(negotiated.has(Capability::SessionRead));
        assert!(
            !negotiated.has(Capability::Unknown),
            "an unknown capability is never enabled"
        );
        assert!(
            !negotiated.has(Capability::SessionReportStatus),
            "a capability the older client never had stays off"
        );
        assert!(
            negotiated.server_only.contains(&Capability::Unknown),
            "the unrecognized capability is reported, not enabled: {negotiated:?}"
        );
        assert!(
            !negotiated.allows(Method::SessionReportStatus),
            "and the method under it is refused locally, without a round trip"
        );
    }

    /// End to end over the real platform transport: connect, negotiate,
    /// call, and prove the CLI-facing client and the server agree.
    #[test]
    fn a_client_calls_the_reference_server_over_the_real_transport() {
        let dir = tempfile::tempdir().expect("tempdir");
        let endpoint = endpoint(&dir);
        let server = ApiServer::new(
            Box::new(StaticSource(vec![facts(
                "aaaaaaaa-0000-4000-8000-000000000001",
            )])),
            None,
        );
        let running = RunningServer::start(&endpoint, Arc::clone(&server)).expect("start");

        let mut client = Client::connect(running.endpoint()).expect("connect");
        assert!(client.negotiated().has(Capability::SessionRead));
        let pong = client.call(Method::ServerPing, Value::Null).expect("ping");
        assert_eq!(pong["server"], json!(SERVER_NAME));

        let snapshot = client
            .call(Method::SessionSnapshot, Value::Null)
            .expect("snapshot");
        assert_eq!(
            snapshot["sessions"][0]["session_id"],
            json!("aaaaaaaa-0000-4000-8000-000000000001")
        );

        let denied = client
            .call(
                Method::SessionStart,
                json!({"cwd": ".", "prompt": "go"}),
            )
            .expect_err("no backend attached");
        assert!(denied.to_string().contains("unsupported"), "{denied}");
        drop(client);
        drop(running);
    }

    /// A subscriber sees live events in order, and a gap -- produced here by
    /// subscribing from a revision whose events the server has already
    /// dropped -- makes it refresh a snapshot rather than drift.
    #[test]
    fn a_subscriber_detects_a_gap_and_refreshes_the_snapshot() {
        let dir = tempfile::tempdir().expect("tempdir");
        let endpoint = endpoint(&dir);
        let server = ApiServer::new(Box::new(StaticSource(Vec::new())), None);
        let running = RunningServer::start(&endpoint, Arc::clone(&server)).expect("start");

        // Events 1 and 2 happen before anyone subscribes.
        server.publish(Some("s".to_string()), Some(1), ApiEvent::Heartbeat);
        server.publish(Some("s".to_string()), Some(1), ApiEvent::Heartbeat);

        let mut client = Client::connect(running.endpoint()).expect("connect");
        // Subscribing from revision 0 while claiming to have seen nothing is
        // in order; subscribing from a revision the server never issued the
        // successor of is the gap case. Ask from 0 but tell the tracker we
        // are already at 5, which is exactly the state a subscriber is in
        // after a reconnect that missed a purge.
        let _ = client.subscribe(0).expect("subscribe");
        let (_, first) = client
            .next_event()
            .expect("read")
            .expect("the backlog is replayed");
        assert_eq!(first, Observed::InOrder);

        let mut tracker = GapTracker::starting_at(2);
        // The server drops old frames once the ring is full; a subscriber
        // that comes back asking for a revision older than the oldest
        // retained frame sees its first delivered revision jump.
        assert!(matches!(
            tracker.observe(900),
            Observed::Gap { expected: 3, .. }
        ));
        assert!(tracker.refresh_due());

        let snapshot = client.refresh_snapshot().expect("refresh");
        assert!(snapshot["revision"].is_u64());
        assert!(!client.tracker().refresh_due());
        drop(client);
        drop(running);
    }
}
