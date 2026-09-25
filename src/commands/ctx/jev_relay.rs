//! Session-scoped Jev relay (issue jev-relay, follow-up to #537's client
//! extraction): moves the HTTP round trip for hook-side Jev calls into the
//! long-lived `zirv ctx exec`/`zirv ctx wrap` supervisor process, so a fresh
//! `zirv ctx hook ...`/`zirv ctx safety check` process -- one per tool call,
//! never warm -- can reuse THIS process's own keep-alive `ureq::Agent`
//! (`jev::send_request`'s [`std::sync::OnceLock`]) instead of paying a fresh
//! TCP+TLS handshake (measured on the operator's machine: ~190ms connect +
//! ~200ms TLS, dwarfing Jev's own ~100-150ms of actual inference) on every
//! single gated call.
//!
//! Reuses the owner-only duplex NDJSON transport in `api::transport`
//! verbatim: no new IPC mechanism, no new endpoint-naming rule. The endpoint
//! path ([`transport::Endpoint::for_jev_relay`]) is derived from nothing but
//! the operator's own state directory and the session id -- never an env
//! var, a flag, or anything repository-controlled; see `api::transport`'s
//! own module doc comment for why that discipline exists at all.
//!
//! Wire protocol, one exchange per connection:
//!
//! ```json
//! // request
//! {"body": "<already fully-encoded SystemOneRequest JSON string>"}
//! // response, forwarded successfully (any HTTP status, 200 or otherwise)
//! {"status": 200, "body": "<raw response body>"}
//! // response, the relay itself could not forward it
//! {"error": "<what went wrong>"}
//! ```
//!
//! A client that gets an `{"error": ...}` frame back -- or fails to connect,
//! times out, or reads a malformed frame -- falls back to a direct call
//! (`jev::ask`'s own relay step, `jev::relay_send`): a broken or absent relay
//! can only ever cost a little latency, never a wrong or missing answer. A
//! `{"status", "body"}` frame, by contrast, is treated exactly like a direct
//! call's own response for that same status: [`try_via_relay`] returns
//! `Some(Ok(body))` for `200`, `Some(Err(jev::status_error(status)))`
//! otherwise -- never a silent fallback for an authentic (if unhappy) answer
//! from Jev itself.
//!
//! The relay never sees the CLIENT's credential: [`forward`] reads and
//! attaches THIS process's own `cfg.credential_env`/`cfg.base_url`, so the
//! credential crossing the socket is never a concern -- there is none to
//! cross.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, mpsc};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::commands::ctx::api::transport::{self, Connection, Endpoint, Listener};
use crate::commands::ctx::config::ProxyTypesafeConfig;
use crate::commands::ctx::jev::{self, JevError};
use crate::commands::ctx::state::StateDir;

/// How long [`try_via_relay`] waits for the whole client-side round trip --
/// connect, write, read -- before giving up and falling back to a direct
/// call. Comfortably above the ~300ms a warm relay round trip should take
/// (this module's own doc comment), comfortably below
/// `ProxyTypesafeConfig::timeout_secs`'s own default (10s): a wedged relay
/// costs at most a few seconds of extra latency, never turns an
/// otherwise-successful direct call into a timeout of its own.
const CLIENT_ROUND_TRIP_TIMEOUT: Duration = Duration::from_secs(3);

#[derive(Debug, Serialize, Deserialize)]
struct RelayRequestFrame {
    body: String,
}

#[derive(Debug, Serialize, Deserialize, Default)]
struct RelayResponseFrame {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    status: Option<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    body: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

/// Set for the lifetime of a process that has itself started a relay
/// ([`start`]), so that SAME process's own `jev::ask` calls never dial their
/// own relay: there is no separate warm connection to save (the direct path
/// already uses the identical shared agent), and a supervisor's own hook
/// invocations calling back into itself over a duplex socket would be an
/// unnecessary hop at best.
static IS_RELAY_HOST: AtomicBool = AtomicBool::new(false);

pub(crate) fn is_relay_host() -> bool {
    IS_RELAY_HOST.load(Ordering::Relaxed)
}

/// A running relay. Dropping it stops the accept thread and removes the
/// endpoint (`Listener`'s own `Drop`) -- the handle is the whole lifetime
/// contract: hold it for as long as the relay should exist, drop it (or let
/// it fall out of scope) to stop.
pub(crate) struct Handle {
    stop: Arc<AtomicBool>,
    listener: Arc<Listener>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Drop for Handle {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        // Wakes the accept thread out of a blocking `accept()` so it notices
        // `stop` and returns instead of waiting for the next connection that
        // may never come.
        self.listener.wake();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
        IS_RELAY_HOST.store(false, Ordering::SeqCst);
    }
}

/// Starts the relay for `session` if useful, `None` otherwise: no `[jev]`
/// gate on (`jev_enabled`, the caller's own `jev::any_gate_enabled(&cfg.
/// jev)`), no credential (`jev::available`), or any bind failure at all --
/// the relay is an optimisation, never required, so every failure mode here
/// is silent rather than propagated. Never blocks the caller's own I/O loop:
/// binding is fast local filesystem/pipe setup, and the accept loop moves to
/// its own thread before this returns.
pub(crate) fn start(
    cfg: &ProxyTypesafeConfig,
    jev_enabled: bool,
    state: &StateDir,
    session: &str,
) -> Option<Handle> {
    if !jev_enabled || !jev::available(cfg) {
        return None;
    }
    let endpoint = Endpoint::for_jev_relay(state, session);
    let listener = Arc::new(Listener::bind(&endpoint).ok()?);
    let stop = Arc::new(AtomicBool::new(false));
    let cfg = cfg.clone();

    let thread_listener = Arc::clone(&listener);
    let thread_stop = Arc::clone(&stop);
    let thread = std::thread::Builder::new()
        .name("jev-relay".to_string())
        .spawn(move || accept_loop(&thread_listener, &thread_stop, &cfg))
        .ok()?;

    IS_RELAY_HOST.store(true, Ordering::SeqCst);
    Some(Handle {
        stop,
        listener,
        thread: Some(thread),
    })
}

fn accept_loop(listener: &Listener, stop: &AtomicBool, cfg: &ProxyTypesafeConfig) {
    while !stop.load(Ordering::SeqCst) {
        let connection = match listener.accept() {
            Ok(connection) => connection,
            // `wake()` connecting to (and immediately dropping) our own
            // endpoint is exactly what unblocks this on shutdown; any other
            // accept failure is treated the same way -- re-check `stop` and
            // either exit or try again, never propagate or panic on this
            // thread.
            Err(_) => continue,
        };
        if stop.load(Ordering::SeqCst) {
            return;
        }
        serve_one(connection, cfg);
    }
}

fn serve_one(mut connection: Connection, cfg: &ProxyTypesafeConfig) {
    // Same owner check as the API server's `serve_connection`: the endpoint's
    // own permissions already restrict it, this is the independent check.
    if !connection.peer().is_same_user(transport::server_uid()) {
        return;
    }
    let Ok(Some(frame)) = connection.read_frame::<RelayRequestFrame>() else {
        return;
    };
    let response = forward(cfg, &frame.body);
    let _ = connection.write_frame(&response);
}

/// The status codes [`jev::send_request`]'s own error mapping can still
/// trace back to a real HTTP status -- the inverse of `jev::status_error`.
/// `None` for every error kind that never HAD one (`Timeout`, `Transport`,
/// `Malformed`, `NoCredential`, `UnsafeState`): those become an `{"error":
/// ...}` frame instead, which the client treats as "fall back to a direct
/// call" rather than a genuine answer -- see this module's own doc comment.
fn error_status(error: &JevError) -> Option<u16> {
    match error {
        JevError::Auth => Some(401),
        JevError::Invalid => Some(422),
        JevError::RateLimited => Some(429),
        JevError::Overloaded => Some(529),
        JevError::Status(status) => Some(*status),
        JevError::NoCredential(_)
        | JevError::UnsafeState
        | JevError::Timeout
        | JevError::Transport(_)
        | JevError::Malformed(_) => None,
    }
}

/// Validates and forwards one already-encoded request body with THIS
/// process's own credential/`base_url` through the shared keep-alive agent
/// (`jev::send_request`) -- never the client's, which never crosses the
/// socket at all.
fn forward(cfg: &ProxyTypesafeConfig, body: &str) -> RelayResponseFrame {
    if !jev::safe_wire_request(body) {
        return RelayResponseFrame {
            error: Some("request failed the relay's own safety check".to_string()),
            ..Default::default()
        };
    }
    let credential = match std::env::var(&cfg.credential_env) {
        Ok(value) if !value.is_empty() => value,
        _ => {
            return RelayResponseFrame {
                error: Some("relay process has no credential".to_string()),
                ..Default::default()
            };
        }
    };
    match jev::send_request(
        &cfg.base_url,
        &credential,
        cfg.timeout_secs,
        body.to_string(),
    ) {
        Ok(response_body) => RelayResponseFrame {
            status: Some(200),
            body: Some(response_body),
            ..Default::default()
        },
        Err(error) => match error_status(&error) {
            Some(status) => RelayResponseFrame {
                status: Some(status),
                body: Some(String::new()),
                ..Default::default()
            },
            None => RelayResponseFrame {
                error: Some(error.to_string()),
                ..Default::default()
            },
        },
    }
}

/// Tries to answer one already-encoded request via `session`'s relay
/// endpoint, if one is running for it. `None` whenever the relay cannot (or
/// should not) be used for ANY reason -- this process IS the relay host, no
/// endpoint is listening, a connect/write/read failure, a round trip that
/// exceeds [`CLIENT_ROUND_TRIP_TIMEOUT`], a malformed frame, or the relay's
/// own `{"error": ...}` frame -- so the caller (`jev::ask`, via `jev::
/// relay_send`) can fall straight through to its own direct call.
/// `Some(Ok(body))`/`Some(Err(_))` is a genuine answer from Jev (a 200 or a
/// mapped HTTP status), identical either way to what a direct call would
/// have produced for that same status.
pub(crate) fn try_via_relay(
    state: &StateDir,
    session: &str,
    payload: &str,
) -> Option<Result<String, JevError>> {
    if is_relay_host() {
        return None;
    }
    let endpoint = Endpoint::for_jev_relay(state, session);
    // A cheap, non-retrying check first: `transport::connect` retries a
    // missing/busy endpoint for up to two seconds, which would turn "no
    // relay running" into a slow path instead of an instant fallback.
    if !transport::probe(&endpoint) {
        return None;
    }

    let (tx, rx) = mpsc::channel();
    let payload = payload.to_string();
    let spawned = std::thread::Builder::new()
        .name("jev-relay-client".to_string())
        .spawn(move || {
            let outcome = (|| -> Option<RelayResponseFrame> {
                let mut connection = transport::connect(&endpoint).ok()?;
                connection
                    .write_frame(&RelayRequestFrame { body: payload })
                    .ok()?;
                connection.read_frame::<RelayResponseFrame>().ok()?
            })();
            let _ = tx.send(outcome);
        });
    let Ok(spawned) = spawned else {
        return None;
    };

    match rx.recv_timeout(CLIENT_ROUND_TRIP_TIMEOUT) {
        Ok(Some(frame)) => {
            let _ = spawned.join();
            if frame.error.is_some() {
                return None;
            }
            match frame.status {
                Some(200) => frame.body.map(Ok),
                Some(status) => Some(Err(jev::status_error(status))),
                None => None,
            }
        }
        Ok(None) => {
            let _ = spawned.join();
            None
        }
        // Timed out: the spawned thread is left to finish (or stay blocked
        // on I/O) on its own rather than joined here -- this call must never
        // block longer than `CLIENT_ROUND_TRIP_TIMEOUT` itself.
        Err(_) => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::ctx::jev::tests::one_shot_server;

    fn config(base_url: String, credential_env: &str) -> ProxyTypesafeConfig {
        ProxyTypesafeConfig {
            base_url,
            credential_env: credential_env.to_string(),
            model: "jev-latest".to_string(),
            timeout_secs: 5,
        }
    }

    fn with_credential<T>(name: &str, value: &str, body: impl FnOnce() -> T) -> T {
        // SAFETY (test-only): each test uses its own unique env var name, so
        // parallel nextest processes never race on the same key.
        unsafe {
            std::env::set_var(name, value);
        }
        let result = body();
        unsafe {
            std::env::remove_var(name);
        }
        result
    }

    fn sample_state() -> serde_json::Value {
        serde_json::json!({"_zirv_metadata_only": true, "facts": [[1, 2, true]]})
    }

    fn sample_questions() -> Vec<jev::Question> {
        vec![jev::Question::metadata_choice(
            "intent",
            "Pick a category from coarse metadata only.",
            &[("feature", "adds behavior"), ("other", "other category")],
        )]
    }

    /// Accepts connections, discarding any that never yield a full request
    /// frame, until one does -- exactly what production's own `accept_loop`/
    /// `serve_one` already tolerate (a `read_frame` error or a clean EOF is
    /// just "drop this connection, accept the next one"). Needed here
    /// because [`try_via_relay`]'s own `transport::probe` call connects to
    /// (and immediately drops without sending anything) the endpoint before
    /// dialling for real: on Windows that can hand a test's very first
    /// `accept()` the probe's own dead connection instead of a real client's
    /// (the exact interaction `transport`'s own
    /// `probe_reports_a_live_endpoint_and_then_a_dead_one` test already
    /// tolerates, by discarding whatever its own single `accept()` returns).
    fn accept_one_real_request(listener: &Listener) -> (Connection, RelayRequestFrame) {
        loop {
            let Ok(mut connection) = listener.accept() else {
                continue;
            };
            match connection.read_frame::<RelayRequestFrame>() {
                Ok(Some(frame)) => return (connection, frame),
                _ => continue,
            }
        }
    }

    /// A request sent through a started relay reaches the stub server and
    /// the caller gets the same answers a direct call would have.
    #[test]
    fn a_relay_forwards_a_request_and_the_caller_gets_the_same_answer() {
        let text = std::fs::read_to_string(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("tests")
                .join("fixtures")
                .join("proxy")
                .join("jev-response.json"),
        )
        .expect("fixture");
        let body: &'static str = Box::leak(text.into_boxed_str());
        let (stub_url, stub_handle) = one_shot_server(200, body);

        let state_tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(state_tmp.path().to_path_buf());
        let session = "relay-forward-session-1";

        with_credential("JEV_RELAY_TEST_FORWARD", "secret", || {
            let cfg = config(stub_url, "JEV_RELAY_TEST_FORWARD");
            let handle = start(&cfg, true, &state, session).expect("relay must start");
            assert!(is_relay_host(), "this process just started a relay");

            // Encode the request exactly the way `ask` itself does, then
            // dial the relay endpoint directly -- `is_relay_host()` is true
            // for THIS process now, so this goes around `try_via_relay`'s
            // own host guard, the same way a DIFFERENT process's `ask` would
            // reach this relay.
            let request = jev::encode_for_test(&sample_state(), &sample_questions(), &cfg.model)
                .expect("encode");

            let endpoint = Endpoint::for_jev_relay(&state, session);
            let mut connection = transport::connect(&endpoint).expect("connect to relay");
            connection
                .write_frame(&RelayRequestFrame {
                    body: request.clone(),
                })
                .expect("write");
            let response: RelayResponseFrame = connection
                .read_frame()
                .expect("read")
                .expect("a frame, not EOF");
            assert_eq!(response.status, Some(200));
            let response_body = response.body.expect("body");

            let parsed: serde_json::Value = serde_json::from_str(&response_body).expect("json");
            assert_eq!(parsed["answers"]["category"]["choice"], "technical");

            drop(handle);
            assert!(!is_relay_host(), "dropping the handle un-hosts it");
        });
        stub_handle
            .join()
            .expect("stub server thread must not panic");
    }

    /// With no relay endpoint bound for the session, the client-side helper
    /// reports "nothing to relay through" immediately.
    #[test]
    fn no_relay_endpoint_reports_nothing_to_relay_through() {
        let state_tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(state_tmp.path().to_path_buf());
        let result = try_via_relay(&state, "no-such-relay-session", "{}");
        assert!(result.is_none());
    }

    /// A relay that returns an `{"error": ...}` frame is treated the same as
    /// no relay at all by the client-side helper: `None`, so the caller
    /// falls back to a direct call.
    #[test]
    fn a_relay_error_frame_reports_nothing_to_relay_through() {
        let state_tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(state_tmp.path().to_path_buf());
        let session = "relay-error-frame-session";
        let endpoint = Endpoint::for_jev_relay(&state, session);
        let listener = Listener::bind(&endpoint).expect("bind");
        let server = std::thread::spawn(move || {
            let (mut connection, _frame) = accept_one_real_request(&listener);
            connection
                .write_frame(&RelayResponseFrame {
                    error: Some("upstream unreachable".to_string()),
                    ..Default::default()
                })
                .expect("write");
            drop(connection);
            drop(listener);
        });

        let result = try_via_relay(&state, session, "{}");
        assert!(
            result.is_none(),
            "an error frame must fall back, not answer"
        );
        server.join().expect("server thread must not panic");
    }

    /// A relay that forwards a non-200 status is a genuine answer, not a
    /// fallback trigger: the client gets the same mapped `JevError` a direct
    /// call would have produced for that status.
    #[test]
    fn a_relay_forwarded_error_status_maps_identically_to_a_direct_call() {
        let state_tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(state_tmp.path().to_path_buf());
        let session = "relay-status-session";
        let endpoint = Endpoint::for_jev_relay(&state, session);
        let listener = Listener::bind(&endpoint).expect("bind");
        let server = std::thread::spawn(move || {
            let (mut connection, _frame) = accept_one_real_request(&listener);
            connection
                .write_frame(&RelayResponseFrame {
                    status: Some(401),
                    body: Some(String::new()),
                    ..Default::default()
                })
                .expect("write");
            drop(connection);
            drop(listener);
        });

        let result = try_via_relay(&state, session, "{}");
        assert!(matches!(result, Some(Err(JevError::Auth))));
        server.join().expect("server thread must not panic");
    }

    /// A dead relay (accepted the connection, then dropped it without ever
    /// writing a frame) is indistinguishable from any other relay failure:
    /// the client falls back.
    #[test]
    fn a_relay_that_dies_mid_request_falls_back() {
        let state_tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(state_tmp.path().to_path_buf());
        let session = "relay-dies-session";
        let endpoint = Endpoint::for_jev_relay(&state, session);
        let listener = Listener::bind(&endpoint).expect("bind");
        let server = std::thread::spawn(move || {
            let (connection, _frame) = accept_one_real_request(&listener);
            drop(connection);
            drop(listener);
        });

        let result = try_via_relay(&state, session, "{}");
        assert!(result.is_none());
        server.join().expect("server thread must not panic");
    }

    /// `start` refuses outright when no `[jev]` gate is on or no credential
    /// is present -- the relay must never bind for a session that could
    /// never call `ask` anyway.
    #[test]
    fn start_refuses_without_a_gate_or_a_credential() {
        let state_tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(state_tmp.path().to_path_buf());
        let cfg = config(
            "http://127.0.0.1:0".to_string(),
            "JEV_RELAY_TEST_START_GATE",
        );
        // SAFETY (test-only): a unique env var name this test owns.
        unsafe {
            std::env::remove_var(&cfg.credential_env);
        }
        assert!(start(&cfg, false, &state, "s1").is_none(), "no gate on");
        assert!(
            start(&cfg, true, &state, "s1").is_none(),
            "gate on but no credential"
        );
    }
}
