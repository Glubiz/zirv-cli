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
//! The relay forwards only this binary's own compiled questions ([`compiled_questions`]): the
//! frame and request are parsed strictly (unknown fields at any level are refused), every
//! question's instructions and options must equal a compiled one's, and the body sent to the
//! vendor is re-serialised from the validated value, never the client's bytes. Each connection
//! is served on its own thread (at most [`MAX_CONNECTIONS`] at once), so a stalled peer holds
//! only its own; on unix it is also dropped if the whole request frame has not arrived within
//! [`READ_DEADLINE`] of acceptance, however slowly its bytes trickle in. The request's `model` is
//! replaced by the relay's own configured one.
//!
//! A client that cannot reach the relay (no endpoint, or connect/write of the
//! request frame fails) falls back to a direct call (`jev::ask`'s own relay
//! step, `jev::relay_send`). Once the request frame is written it never does:
//! an `{"error": ...}` frame, a lost frame or a timeout become a `JevError`
//! (recorded as a fallback row), because the vendor may already have the
//! request and a direct re-send would bill a second one. A
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

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, OnceLock, mpsc};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::commands::ctx::api::transport::{self, Connection, Endpoint, Listener};
use crate::commands::ctx::config::ProxyTypesafeConfig;
use crate::commands::ctx::jev::{self, JevError};
use crate::commands::ctx::state::StateDir;
use crate::commands::ctx::{
    compile, exec, handoff, hook, inject_gate, inject_screen, memory, proxy, run_loop, safety, task,
};
use crate::commands::workflow::{engine, profile, review, team};

/// How long a connected peer may take to deliver its request frame before the relay drops it.
const READ_DEADLINE: Duration = Duration::from_secs(3);

/// Connections served at once; a hook process makes one short call per tool use.
const MAX_CONNECTIONS: usize = 8;

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
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
    path: std::path::PathBuf,
}

impl Drop for Handle {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        // Wakes the accept thread out of a blocking `accept()` so it notices
        // `stop` and returns instead of waiting for the next connection that
        // may never come.
        self.listener.wake();
        // Never joined: an in-flight forward can run for the whole request timeout, and a dashboard
        // reaping a pane must not freeze on it. The thread exits on its own once that forward ends.
        let _ = std::fs::remove_file(&self.path);
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
    if !jev_enabled || !jev::has_credential(cfg) {
        return None;
    }
    let endpoint = Endpoint::for_jev_relay(state, session);
    let listener = Arc::new(Listener::bind(&endpoint).ok()?);
    let stop = Arc::new(AtomicBool::new(false));
    let cfg = cfg.clone();

    let thread_listener = Arc::clone(&listener);
    let thread_stop = Arc::clone(&stop);
    std::thread::Builder::new()
        .name("jev-relay".to_string())
        .spawn(move || accept_loop(&thread_listener, &thread_stop, &cfg))
        .ok()?;

    IS_RELAY_HOST.store(true, Ordering::SeqCst);
    Some(Handle {
        stop,
        listener,
        path: endpoint.path().to_path_buf(),
    })
}

/// Decrements the live-connection count when its serving thread ends.
struct ConnectionSlot(Arc<AtomicUsize>);

impl Drop for ConnectionSlot {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

fn accept_loop(listener: &Listener, stop: &AtomicBool, cfg: &ProxyTypesafeConfig) {
    let live = Arc::new(AtomicUsize::new(0));
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
        // Each connection gets its own thread so a stalled peer holds only that thread; past the
        // cap a connection is dropped, which its client sees as a lost answer.
        if live.fetch_add(1, Ordering::SeqCst) >= MAX_CONNECTIONS {
            live.fetch_sub(1, Ordering::SeqCst);
            continue;
        }
        let slot = ConnectionSlot(Arc::clone(&live));
        let cfg = cfg.clone();
        let spawned = std::thread::Builder::new()
            .name("jev-relay-conn".to_string())
            .spawn(move || {
                let _slot = slot;
                serve_one(connection, &cfg);
            });
        // A failed spawn drops the closure, and with it the slot and the connection.
        drop(spawned);
    }
}

fn serve_one(mut connection: Connection, cfg: &ProxyTypesafeConfig) {
    // Same owner check as the API server's `serve_connection`: the endpoint's
    // own permissions already restrict it, this is the independent check.
    if !connection.peer().is_same_user(transport::server_uid()) {
        return;
    }
    connection.set_read_deadline(Some(READ_DEADLINE));
    let Ok(Some(frame)) = connection.read_frame::<RelayRequestFrame>() else {
        return;
    };
    connection.set_read_deadline(None);
    let response = forward(cfg, &frame.body);
    let _ = connection.write_frame(&response);
}

/// The status codes [`jev::send_request`]'s own error mapping can still
/// trace back to a real HTTP status -- the inverse of `jev::status_error`.
/// `None` for every error kind that never HAD one (`Timeout`, `Transport`,
/// `Malformed`, `NoCredential`, `UnsafeState`): those become an `{"error":
/// ...}` frame instead, which the client turns into a `JevError` without a
/// direct re-send -- see this module's own doc comment.
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

/// Every question a call site of this binary can ask, built once from the site constructors
/// (the same set `zirv ctx jev probe` measures). Ids are caller-chosen, so only a question's
/// spec is compared; a new site must be listed here to be answerable through a relay.
fn compiled_questions() -> &'static [jev::Question] {
    static COMPILED: OnceLock<Vec<jev::Question>> = OnceLock::new();
    COMPILED.get_or_init(|| {
        let ids = ["q".to_string()];
        let mut questions = vec![
            compile::context_report_question("q"),
            compile::context_skill_question("q"),
            hook::dispatch_tier_question(),
            task::crash_cause_question(),
            run_loop::judge_continue_question(),
            safety::approve_escalate_question(),
            safety::approve_lower_question(),
            team::intake_plan_question(),
            inject_screen::inject_screen_question(),
        ];
        questions.extend(handoff::handoff_select_questions(&ids));
        questions.extend(handoff::compaction_select_questions(&ids));
        questions.extend(review::review_disposition_questions(1));
        questions.extend(review::review_dedup_questions(&ids));
        questions.extend(memory::harvest_screen_question());
        questions.extend(exec::launch_effort_question());
        questions.extend(inject_gate::questions());
        questions.extend(hook::missing_tests_questions());
        questions.extend(hook::stop_verify_questions());
        questions.extend(hook::retry_questions());
        questions.extend(engine::gate_reclass_questions());
        questions.extend(profile::classify_jev_questions());
        questions.extend(proxy::safe_intake_questions());
        questions
    })
}

/// Validates one already-encoded request body against [`compiled_questions`] and forwards its
/// canonical re-serialisation with THIS process's own credential/`base_url` through the shared
/// keep-alive agent (`jev::send_request`) -- never the client's, which never crosses the socket
/// at all, and never the client's own bytes.
fn forward(cfg: &ProxyTypesafeConfig, body: &str) -> RelayResponseFrame {
    let Some(body) = jev::compiled_wire_body(body, compiled_questions(), &cfg.model) else {
        return RelayResponseFrame {
            error: Some("request is not one of this binary's own questions".to_string()),
            ..Default::default()
        };
    };
    let credential = match std::env::var(&cfg.credential_env) {
        Ok(value) if !value.is_empty() => value,
        _ => {
            return RelayResponseFrame {
                error: Some("relay process has no credential".to_string()),
                ..Default::default()
            };
        }
    };
    match jev::send_request(&cfg.base_url, &credential, cfg.timeout_secs, body) {
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

/// Whether `session`'s relay endpoint is listening right now, so a process without the credential
/// (a worker whose env was scrubbed) can still reach Jev through its supervisor.
pub(crate) fn is_reachable(state: &StateDir, session: &str) -> bool {
    !is_relay_host() && transport::probe(&Endpoint::for_jev_relay(state, session))
}

/// Tries to answer one already-encoded request via `session`'s relay
/// endpoint, if one is running for it. `None` only when nothing was sent --
/// this process IS the relay host, no endpoint is listening, or connect/write
/// of the request frame failed -- so the caller (`jev::ask`, via `jev::
/// relay_send`) can fall through to its own direct call. Once the frame is
/// written the result is `Some`: a 200 or mapped HTTP status as a direct call
/// would give, or a `Transport`/`Timeout` error for an error frame, a lost
/// frame or a round trip exceeding `wait`.
pub(crate) fn try_via_relay(
    state: &StateDir,
    session: &str,
    payload: &str,
    wait: Duration,
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
            let outcome = (|| {
                let Ok(mut connection) = transport::connect(&endpoint) else {
                    return RelayOutcome::Unreachable;
                };
                if connection
                    .write_frame(&RelayRequestFrame { body: payload })
                    .is_err()
                {
                    return RelayOutcome::Unreachable;
                }
                match connection.read_frame::<RelayResponseFrame>() {
                    Ok(Some(frame)) => RelayOutcome::Answered(frame),
                    _ => RelayOutcome::Lost,
                }
            })();
            let _ = tx.send(outcome);
        });
    let Ok(spawned) = spawned else {
        return None;
    };

    // Past this point the request may have reached the vendor: never fall back to a direct send.
    match rx.recv_timeout(wait) {
        Ok(RelayOutcome::Unreachable) => {
            let _ = spawned.join();
            None
        }
        Ok(RelayOutcome::Answered(frame)) => {
            let _ = spawned.join();
            if let Some(error) = frame.error {
                return Some(Err(JevError::Transport(format!("relay: {error}"))));
            }
            match frame.status {
                Some(200) => frame.body.map(Ok),
                Some(status) => Some(Err(jev::status_error(status))),
                None => Some(Err(JevError::Transport("relay: empty answer".to_string()))),
            }
        }
        Ok(RelayOutcome::Lost) => {
            let _ = spawned.join();
            Some(Err(JevError::Transport(
                "relay: connection lost before an answer".to_string(),
            )))
        }
        // The client thread is left to finish on its own; this call must never block longer than `wait`.
        Err(_) => Some(Err(JevError::Timeout)),
    }
}

/// How one client-side relay round trip ended.
enum RelayOutcome {
    /// Connect or write of the request frame failed: nothing was sent, so a direct call is safe.
    Unreachable,
    Answered(RelayResponseFrame),
    /// The request frame was written but no answer frame came back.
    Lost,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::ctx::config::CtxConfig;
    use crate::commands::ctx::jev::tests::one_shot_server;

    const TEST_WAIT: Duration = Duration::from_secs(3);

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
        vec![safety::approve_escalate_question()]
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
        let result = try_via_relay(&state, "no-such-relay-session", "{}", TEST_WAIT);
        assert!(result.is_none());
    }

    /// Config, key and session for an `advise_detailed` call whose relay is a stand-in and whose
    /// direct endpoint is a listener that must never be contacted.
    fn relay_case(
        tag: &str,
    ) -> (
        CtxConfig,
        StateDir,
        tempfile::TempDir,
        std::net::TcpListener,
    ) {
        let dir = tempfile::tempdir().expect("tempdir");
        let vendor = std::net::TcpListener::bind("127.0.0.1:0").expect("vendor stub");
        vendor.set_nonblocking(true).expect("nonblocking");
        let mut cfg = CtxConfig::default();
        cfg.jev.cache_ttl_secs = 0;
        cfg.proxy.typesafe = config(
            format!("http://{}", vendor.local_addr().expect("addr")),
            &format!("JEV_RELAY_TEST_{tag}"),
        );
        let state = StateDir::from_root(dir.path().to_path_buf());
        (cfg, state, dir, vendor)
    }

    fn assert_one_fallback_row_and_no_direct_send(
        state: &StateDir,
        vendor: &std::net::TcpListener,
        status: jev::AdvisoryStatus,
    ) {
        assert!(matches!(status, jev::AdvisoryStatus::Failed));
        assert!(
            vendor.accept().is_err(),
            "no direct request may follow a written frame"
        );
        let text = std::fs::read_to_string(state.root().join("jev-decisions.jsonl"))
            .expect("decision row");
        assert_eq!(text.lines().count(), 1, "{text}");
        assert!(
            !text.contains("\"fallbacks\":[]"),
            "the row must carry the fallback: {text}"
        );
    }

    /// A scrubbed worker holds no key: `available` and `ask` still work through its supervisor's relay.
    #[test]
    fn a_worker_without_the_key_is_available_and_asks_through_the_relay() {
        let text = std::fs::read_to_string(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("tests")
                .join("fixtures")
                .join("proxy")
                .join("jev-response.json"),
        )
        .expect("fixture");
        let (cfg, state, _dir, vendor) = relay_case("NOKEY870");
        let session = "relay-no-key-session-870";
        // Without a relay endpoint there is nothing to reach.
        // SAFETY (test-only): unique names owned by this test.
        unsafe {
            std::env::set_var(crate::commands::ctx::adapters::SESSION_ENV, session);
            std::env::set_var(crate::commands::ctx::state::STATE_ENV, state.root());
        }
        assert!(!jev::available(&cfg.proxy.typesafe));
        let listener = Listener::bind(&Endpoint::for_jev_relay(&state, session)).expect("bind");
        let server = std::thread::spawn(move || {
            let (mut connection, _frame) = accept_one_real_request(&listener);
            connection
                .write_frame(&RelayResponseFrame {
                    status: Some(200),
                    body: Some(text),
                    ..Default::default()
                })
                .expect("write");
        });
        let available = jev::available(&cfg.proxy.typesafe);
        let answered = jev::ask(
            &cfg.proxy.typesafe,
            state.root(),
            0,
            &sample_state(),
            &sample_questions(),
        );
        unsafe {
            std::env::remove_var(crate::commands::ctx::adapters::SESSION_ENV);
            std::env::remove_var(crate::commands::ctx::state::STATE_ENV);
        }
        server.join().expect("server thread must not panic");
        assert!(available, "a reachable relay makes Jev available");
        assert!(answered.is_ok(), "{answered:?}");
        assert!(vendor.accept().is_err(), "no direct request");
    }

    /// A relay host with no key of its own must not bind, even when a parent's relay is reachable.
    #[test]
    fn start_refuses_without_a_key_even_when_a_parent_relay_is_reachable() {
        let (cfg, state, _dir, _vendor) = relay_case("NOKEYSTART870");
        let parent = "relay-parent-session-870";
        let _listener = Listener::bind(&Endpoint::for_jev_relay(&state, parent)).expect("bind");
        // SAFETY (test-only): unique names owned by this test.
        unsafe {
            std::env::remove_var(&cfg.proxy.typesafe.credential_env);
            std::env::set_var(crate::commands::ctx::adapters::SESSION_ENV, parent);
            std::env::set_var(crate::commands::ctx::state::STATE_ENV, state.root());
        }
        assert!(jev::available(&cfg.proxy.typesafe));
        let started = start(&cfg.proxy.typesafe, true, &state, "relay-child-session-870");
        unsafe {
            std::env::remove_var(crate::commands::ctx::adapters::SESSION_ENV);
            std::env::remove_var(crate::commands::ctx::state::STATE_ENV);
        }
        assert!(
            started.is_none(),
            "a key-less process must not host a relay"
        );
    }

    /// Dropping a relay while a forward is in flight must not wait for the upstream.
    #[test]
    fn dropping_a_relay_mid_forward_returns_promptly() {
        let upstream = std::net::TcpListener::bind("127.0.0.1:0").expect("upstream");
        let base_url = format!("http://{}", upstream.local_addr().expect("addr"));
        let (accepted_tx, accepted_rx) = mpsc::channel();
        let slow = std::thread::spawn(move || {
            let Ok((stream, _)) = upstream.accept() else {
                return;
            };
            let _ = accepted_tx.send(());
            std::thread::sleep(Duration::from_secs(3));
            drop(stream);
        });
        let state_tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(state_tmp.path().to_path_buf());
        let session = "relay-drop-mid-forward-870";
        with_credential("JEV_RELAY_TEST_DROP870", "secret", || {
            let cfg = config(base_url, "JEV_RELAY_TEST_DROP870");
            let handle = start(&cfg, true, &state, session).expect("relay must start");
            let request = jev::encode_for_test(&sample_state(), &sample_questions(), &cfg.model)
                .expect("encode");
            let mut connection =
                transport::connect(&Endpoint::for_jev_relay(&state, session)).expect("connect");
            connection
                .write_frame(&RelayRequestFrame { body: request })
                .expect("write");
            accepted_rx
                .recv_timeout(Duration::from_secs(5))
                .expect("the forward reached the upstream");
            let started = std::time::Instant::now();
            drop(handle);
            assert!(
                started.elapsed() < Duration::from_secs(1),
                "drop blocked on the in-flight forward: {:?}",
                started.elapsed()
            );
        });
        slow.join().expect("slow upstream must not panic");
    }

    /// The relay reported a vendor failure (an error frame) after the request was written: the
    /// client must not re-send it directly.
    #[test]
    fn a_relay_error_frame_is_not_retried_directly() {
        let (cfg, state, _dir, vendor) = relay_case("ERRFRAME");
        let session = "relay-error-frame-session";
        let listener = Listener::bind(&Endpoint::for_jev_relay(&state, session)).expect("bind");
        let server = std::thread::spawn(move || {
            let (mut connection, _frame) = accept_one_real_request(&listener);
            connection
                .write_frame(&RelayResponseFrame {
                    error: Some("upstream timed out".to_string()),
                    ..Default::default()
                })
                .expect("write");
        });
        // SAFETY (test-only): unique names owned by this test.
        unsafe {
            std::env::set_var("JEV_RELAY_TEST_ERRFRAME", "secret");
            std::env::set_var(crate::commands::ctx::adapters::SESSION_ENV, session);
        }
        let status = jev::advise_detailed(
            &cfg,
            &state,
            "memory",
            true,
            &sample_state(),
            &sample_questions(),
        );
        unsafe {
            std::env::remove_var("JEV_RELAY_TEST_ERRFRAME");
            std::env::remove_var(crate::commands::ctx::adapters::SESSION_ENV);
        }
        server.join().expect("server thread must not panic");
        assert_one_fallback_row_and_no_direct_send(&state, &vendor, status);
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

        let result = try_via_relay(&state, session, "{}", TEST_WAIT);
        assert!(matches!(result, Some(Err(JevError::Auth))));
        server.join().expect("server thread must not panic");
    }

    /// A relay that answers after the old fixed 3s client wait made `ask` re-send the request
    /// directly while the relay's own copy was still in flight: two live vendor requests, one row.
    #[test]
    fn a_slow_relay_answer_is_awaited_instead_of_re_sent_directly() {
        let text = std::fs::read_to_string(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("tests")
                .join("fixtures")
                .join("proxy")
                .join("jev-response.json"),
        )
        .expect("fixture");
        let state_tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(state_tmp.path().to_path_buf());
        let session = "relay-slow-session";
        let listener = Listener::bind(&Endpoint::for_jev_relay(&state, session)).expect("bind");
        let server = std::thread::spawn(move || {
            let (mut connection, _frame) = accept_one_real_request(&listener);
            std::thread::sleep(Duration::from_millis(3500));
            let _ = connection.write_frame(&RelayResponseFrame {
                status: Some(200),
                body: Some(text),
                ..Default::default()
            });
        });

        // The direct fallback would fail (nothing listens on port 1), so an answer proves the relay's was awaited.
        let cfg = ProxyTypesafeConfig {
            timeout_secs: 10,
            ..config("http://127.0.0.1:1".to_string(), "JEV_RELAY_TEST_SLOW")
        };
        // SAFETY (test-only): unique names owned by this test.
        unsafe {
            std::env::set_var("JEV_RELAY_TEST_SLOW", "secret");
            std::env::set_var(crate::commands::ctx::adapters::SESSION_ENV, session);
        }
        let result = jev::ask(
            &cfg,
            state_tmp.path(),
            0,
            &sample_state(),
            &sample_questions(),
        );
        unsafe {
            std::env::remove_var("JEV_RELAY_TEST_SLOW");
            std::env::remove_var(crate::commands::ctx::adapters::SESSION_ENV);
        }
        server.join().expect("relay stand-in must not panic");
        assert!(result.is_ok(), "got {result:?}");
    }

    /// A test build must refuse a non-loopback endpoint before it even looks for a relay: a live
    /// relay from the developer's own session would send the request from a non-test process.
    #[test]
    fn a_test_build_never_contacts_the_relay_for_a_real_endpoint() {
        let state_tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(state_tmp.path().to_path_buf());
        let session = "relay-guard-session";
        let endpoint = Endpoint::for_jev_relay(&state, session);
        let listener = Listener::bind(&endpoint).expect("bind");
        let server = std::thread::spawn(move || accept_one_real_request(&listener).1.body);

        let cfg = config(
            ProxyTypesafeConfig::default().base_url,
            "JEV_RELAY_TEST_GUARD",
        );
        // SAFETY (test-only): unique names owned by this test.
        unsafe {
            std::env::set_var("JEV_RELAY_TEST_GUARD", "secret");
            std::env::set_var(crate::commands::ctx::adapters::SESSION_ENV, session);
        }
        let result = jev::ask(
            &cfg,
            state_tmp.path(),
            0,
            &sample_state(),
            &sample_questions(),
        );
        unsafe {
            std::env::remove_var("JEV_RELAY_TEST_GUARD");
            std::env::remove_var(crate::commands::ctx::adapters::SESSION_ENV);
        }
        assert!(
            matches!(result, Err(JevError::Transport(_))),
            "got {result:?}"
        );

        // The first request the stand-in sees must be this marker, not `ask`'s payload.
        let mut connection = transport::connect(&endpoint).expect("connect");
        connection
            .write_frame(&RelayRequestFrame {
                body: "marker".to_string(),
            })
            .expect("write");
        assert_eq!(server.join().expect("stand-in must not panic"), "marker");
    }

    /// The relay took the request frame and then vanished (supervisor exit mid-flight): the vendor
    /// may already have it, so the client records a fallback and never re-sends directly.
    #[test]
    fn a_lost_relay_answer_is_not_retried_directly() {
        let (cfg, state, _dir, vendor) = relay_case("LOSTFRAME");
        let session = "relay-dies-session";
        let listener = Listener::bind(&Endpoint::for_jev_relay(&state, session)).expect("bind");
        let server = std::thread::spawn(move || {
            let (connection, _frame) = accept_one_real_request(&listener);
            drop(connection);
        });
        // SAFETY (test-only): unique names owned by this test.
        unsafe {
            std::env::set_var("JEV_RELAY_TEST_LOSTFRAME", "secret");
            std::env::set_var(crate::commands::ctx::adapters::SESSION_ENV, session);
        }
        let status = jev::advise_detailed(
            &cfg,
            &state,
            "memory",
            true,
            &sample_state(),
            &sample_questions(),
        );
        unsafe {
            std::env::remove_var("JEV_RELAY_TEST_LOSTFRAME");
            std::env::remove_var(crate::commands::ctx::adapters::SESSION_ENV);
        }
        server.join().expect("server thread must not panic");
        assert_one_fallback_row_and_no_direct_send(&state, &vendor, status);
    }

    /// Starts a relay whose vendor is a listener that records contact, sends `frame` to it and
    /// returns the relay's answer plus whether the vendor was contacted.
    fn offer_to_relay(tag: &str, frame: &serde_json::Value) -> (Option<RelayResponseFrame>, bool) {
        let (cfg, state, _dir, vendor) = relay_case(tag);
        let session = format!("relay-offer-{tag}");
        let answer = with_credential(&cfg.proxy.typesafe.credential_env, "secret", || {
            let handle = start(&cfg.proxy.typesafe, true, &state, &session).expect("relay starts");
            let mut connection =
                transport::connect(&Endpoint::for_jev_relay(&state, &session)).expect("connect");
            connection.write_frame(frame).expect("write");
            let answer = connection.read_frame::<RelayResponseFrame>().ok().flatten();
            drop(handle);
            answer
        });
        (answer, vendor.accept().is_ok())
    }

    /// Refused means an error frame or a dropped connection, and never a vendor request.
    fn assert_refused(tag: &str, frame: &serde_json::Value) {
        let (answer, contacted) = offer_to_relay(tag, frame);
        assert!(
            answer.as_ref().is_none_or(|frame| frame.error.is_some()),
            "must be refused: {answer:?}"
        );
        assert!(!contacted, "the vendor must never be contacted");
    }

    fn sample_request() -> String {
        let cfg = config(String::new(), "UNUSED");
        jev::encode_for_test(&sample_state(), &sample_questions(), &cfg.model).expect("encode")
    }

    #[test]
    fn a_question_that_is_not_compiled_is_refused() {
        let free_text = vec![jev::Question::metadata_choice(
            "intent",
            "Summarise whatever text the caller put here.",
            &[("feature", "adds behavior"), ("other", "other category")],
        )];
        let body = jev::encode_for_test(&sample_state(), &free_text, "jev-latest").expect("encode");
        assert_refused("FREETEXT", &serde_json::json!({ "body": body }));
    }

    #[test]
    fn a_compiled_question_with_other_options_is_refused() {
        let compiled = safety::approve_escalate_question();
        let altered = vec![jev::Question::choice(
            &compiled.id,
            &compiled.instructions,
            &[("safe", "anything"), ("risky", "else")],
        )];
        let body = jev::encode_for_test(&sample_state(), &altered, "jev-latest").expect("encode");
        assert_refused("OPTIONS", &serde_json::json!({ "body": body }));
    }

    #[test]
    fn unknown_fields_are_refused_in_the_frame_and_in_the_request() {
        let body = sample_request();
        assert_refused(
            "FRAMEFIELD",
            &serde_json::json!({ "body": body, "extra": true }),
        );
        let mut request: serde_json::Value = serde_json::from_str(&body).expect("json");
        request["extra"] = serde_json::json!("x");
        assert_refused(
            "REQFIELD",
            &serde_json::json!({ "body": request.to_string() }),
        );
        let mut request: serde_json::Value = serde_json::from_str(&body).expect("json");
        request["questions"]["risk"]["extra"] = serde_json::json!("x");
        assert_refused(
            "SPECFIELD",
            &serde_json::json!({ "body": request.to_string() }),
        );
    }

    #[test]
    fn the_forwarded_body_is_the_canonical_serialisation() {
        let body = sample_request();
        let pretty: serde_json::Value = serde_json::from_str(&body).expect("json");
        let reformatted = serde_json::to_string_pretty(&pretty).expect("pretty");
        assert_ne!(reformatted, body);
        assert_eq!(
            jev::compiled_wire_body(&reformatted, compiled_questions(), "jev-latest"),
            Some(body)
        );
    }

    #[test]
    fn the_forwarded_body_carries_the_relays_own_model() {
        let sent = jev::encode_for_test(&sample_state(), &sample_questions(), "client-chosen")
            .expect("encode");
        assert_eq!(
            jev::compiled_wire_body(&sent, compiled_questions(), "jev-latest"),
            Some(sample_request())
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_client_trickling_bytes_is_cut_off_within_one_deadline() {
        use std::io::Write;
        let (cfg, state, _dir, _vendor) = relay_case("TRICKLE");
        with_credential(&cfg.proxy.typesafe.credential_env, "secret", || {
            let _handle =
                start(&cfg.proxy.typesafe, true, &state, "relay-trickle").expect("starts");
            let endpoint = Endpoint::for_jev_relay(&state, "relay-trickle");
            let mut stream =
                std::os::unix::net::UnixStream::connect(endpoint.path()).expect("connect");
            let started = std::time::Instant::now();
            // One byte every half second, never a newline: each read finishes inside the deadline.
            while started.elapsed() < READ_DEADLINE * 2 {
                if stream.write_all(b"x").is_err() {
                    break;
                }
                std::thread::sleep(Duration::from_millis(500));
            }
            assert!(
                started.elapsed() < READ_DEADLINE + Duration::from_secs(2),
                "the relay held a trickling peer for {:?}",
                started.elapsed()
            );
        });
    }

    #[test]
    fn every_compiled_question_passes_the_metadata_check() {
        for question in compiled_questions() {
            assert!(
                jev::safe_metadata_request(
                    &sample_state(),
                    std::slice::from_ref(question),
                    "jev-latest"
                ),
                "{}",
                question.id
            );
        }
    }

    #[test]
    fn a_stalled_client_does_not_block_the_next_one() {
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
        let session = "relay-stalled-client";
        let request = sample_request();
        let (tx, rx) = mpsc::channel();
        with_credential("JEV_RELAY_TEST_STALLED", "secret", || {
            let cfg = config(stub_url, "JEV_RELAY_TEST_STALLED");
            let _handle = start(&cfg, true, &state, session).expect("relay starts");
            let endpoint = Endpoint::for_jev_relay(&state, session);
            // Connected, never writes a frame.
            let _stalled = transport::connect(&endpoint).expect("connect");
            std::thread::spawn(move || {
                let mut connection = transport::connect(&endpoint).expect("connect");
                connection
                    .write_frame(&RelayRequestFrame { body: request })
                    .expect("write");
                let answer = connection.read_frame::<RelayResponseFrame>();
                let _ = tx.send(answer.ok().flatten());
            });
            // Well inside the read deadline, so the stalled peer's timeout is not what unblocks it.
            let answer = rx
                .recv_timeout(READ_DEADLINE / 2)
                .expect("the second client must be answered while the stalled one still holds");
            assert_eq!(answer.expect("a frame").status, Some(200));
        });
        stub_handle.join().expect("stub server must not panic");
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
