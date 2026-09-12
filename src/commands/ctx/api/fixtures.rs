//! Frozen wire fixtures for protocol v1 (issue #353).
//!
//! `tests/fixtures/protocol/v1/` holds one committed request and one
//! committed response per published method, plus a `hello` frame and one
//! event frame per event kind. This module replays every request against a
//! DETERMINISTIC reference server and asserts the reply is byte-identical to
//! the frozen response. That is the wire-compatibility guard: a change to a
//! field name, a field's position, an enum's wire value, an error code or
//! the revision discipline fails here, in the same change that made it,
//! rather than in somebody's client months later.
//!
//! Determinism comes from three places and nowhere else: a fixed
//! [`StaticSource`] session set, [`FakeNativeBackend`] (which mints
//! `fake-N` ids from a counter and touches no clock, filesystem or network),
//! and a fixed call ORDER -- the revision on every reply is a function of
//! how many events the calls before it emitted.
//!
//! The one value that is deliberately NOT frozen is `server_version`: it
//! changes on every release, and a fixture that had to be re-blessed on
//! every version bump would stop being evidence of anything. It is replaced
//! with `<version>` on both sides before the comparison, by
//! [`normalize`].

use std::path::PathBuf;

use serde_json::{Value, json};

use super::server::{ApiServer, StaticSource};
use super::wire::{
    ApiEvent, Method, PROTOCOL_VERSION, Request, ServerFrame, SessionFacts, SessionState,
};
use crate::commands::ctx::runtime::fake::FakeNativeBackend;
use crate::commands::ctx::runtime::{RuntimeKind, UiSurface};

fn fixtures_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/protocol/v1")
}

const ORCHESTRATOR: &str = "11111111-1111-4111-8111-111111111111";
const WORKER: &str = "22222222-2222-4222-8222-222222222222";

fn seeded_sessions() -> Vec<SessionFacts> {
    vec![
        SessionFacts {
            session_id: ORCHESTRATOR.to_string(),
            short: "11111111".to_string(),
            runtime: RuntimeKind::Harness,
            generation: 1,
            surface: UiSurface::Terminal,
            state: SessionState::Idle,
            role: Some("orchestrator".to_string()),
            agent: Some("claude".to_string()),
            repo_slug: Some("zirv-cli".to_string()),
            started_at: Some(1_757_000_000),
            reachable: true,
        },
        SessionFacts {
            session_id: WORKER.to_string(),
            short: "22222222".to_string(),
            runtime: RuntimeKind::Harness,
            generation: 1,
            surface: UiSurface::DashboardPane,
            state: SessionState::Working,
            role: Some("worker".to_string()),
            agent: Some("codex".to_string()),
            repo_slug: Some("zirv-cli".to_string()),
            started_at: Some(1_757_000_100),
            reachable: true,
        },
    ]
}

/// The exact call sequence the fixtures were frozen from. The order is part
/// of the contract under test: every reply's `revision` depends on it.
fn scenario() -> Vec<(&'static str, Request)> {
    vec![
        (
            "server.ping",
            Request::new("req-ping", Method::ServerPing, Value::Null),
        ),
        (
            "server.capabilities",
            Request::new("req-capabilities", Method::ServerCapabilities, Value::Null),
        ),
        (
            "session.snapshot",
            Request::new("req-snapshot", Method::SessionSnapshot, Value::Null),
        ),
        (
            "session.list",
            Request::new("req-list", Method::SessionList, json!({"state": "idle"})),
        ),
        (
            "session.get",
            Request::new(
                "req-get",
                Method::SessionGet,
                json!({"session_id": ORCHESTRATOR}),
            ),
        ),
        (
            "session.start",
            Request::new(
                "req-start",
                Method::SessionStart,
                json!({
                    "runtime": "native",
                    "role": "worker",
                    "agent": "native",
                    "surface": "headless",
                    "cwd": "/work/repo",
                    "prompt": "summarise the failing test"
                }),
            )
            .with_idempotency_key("start-worker-1"),
        ),
        (
            "session.send_input",
            Request::new(
                "req-send-input",
                Method::SessionSendInput,
                json!({"session_id": "fake-1", "generation": 1, "input": "keep going", "mode": "submit"}),
            )
            .with_idempotency_key("send-1"),
        ),
        (
            "session.read",
            Request::new(
                "req-read",
                Method::SessionRead,
                json!({"session_id": "fake-1", "after_revision": 0}),
            ),
        ),
        (
            "session.report_status",
            Request::new(
                "req-report-status",
                Method::SessionReportStatus,
                json!({"session_id": "fake-1", "generation": 1, "state": "idle"}),
            )
            .with_idempotency_key("report-1"),
        ),
        (
            "session.wait",
            Request::new(
                "req-wait",
                Method::SessionWait,
                json!({"session_id": "fake-1", "generation": 1, "until": "idle", "timeout_ms": 100}),
            ),
        ),
        (
            "session.stop",
            Request::new(
                "req-stop",
                Method::SessionStop,
                json!({"session_id": "fake-1", "generation": 1}),
            )
            .with_idempotency_key("stop-1"),
        ),
        (
            "events.subscribe",
            Request::new(
                "req-subscribe",
                Method::EventsSubscribe,
                json!({"after_revision": 0}),
            ),
        ),
        (
            "error.unknown_session",
            Request::new(
                "req-unknown-session",
                Method::SessionGet,
                json!({"session_id": "00000000-0000-4000-8000-000000000000"}),
            ),
        ),
    ]
}

/// The v1 wire carries exactly one value that legitimately changes without
/// the protocol changing.
fn normalize(value: &mut Value) {
    match value {
        Value::Object(map) => {
            for (key, entry) in map.iter_mut() {
                if key == "server_version" {
                    *entry = json!("<version>");
                } else {
                    normalize(entry);
                }
            }
        }
        Value::Array(items) => items.iter_mut().for_each(normalize),
        _ => {}
    }
}

fn canonical(value: &Value) -> String {
    let mut value = value.clone();
    normalize(&mut value);
    let mut text = serde_json::to_string_pretty(&value).expect("serialize fixture value");
    text.push('\n');
    text
}

fn read_fixture(name: &str) -> String {
    let path = fixtures_dir().join(name);
    std::fs::read_to_string(&path).unwrap_or_else(|error| panic!("read fixture {path:?}: {error}"))
}

fn assert_fixture(name: &str, actual: &Value) {
    let expected = read_fixture(name);
    let actual = canonical(actual);
    assert_eq!(
        expected, actual,
        "fixture {name} no longer matches what this build produces -- the v1 wire changed"
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One deterministic server, the frozen call order, and a byte-for-byte
    /// comparison of every request and every reply against what is committed
    /// under `tests/fixtures/protocol/v1/`.
    #[test]
    fn every_frozen_request_replays_to_its_frozen_response() {
        let server = ApiServer::new(
            Box::new(StaticSource(seeded_sessions())),
            Some(Box::new(FakeNativeBackend::new())),
        );
        for (name, request) in scenario() {
            assert_fixture(
                &format!("request-{name}.json"),
                &serde_json::to_value(&request).expect("serialize request"),
            );
            let response = server.handle(&request);
            assert_fixture(
                &format!("response-{name}.json"),
                &serde_json::to_value(&response).expect("serialize response"),
            );
        }
    }

    /// The handshake frame and one frame per event kind, frozen separately:
    /// a subscriber never sees them as a reply, so the replay above cannot
    /// cover them.
    #[test]
    fn the_hello_and_event_frames_match_their_fixtures() {
        let server = ApiServer::new(
            Box::new(StaticSource(seeded_sessions())),
            Some(Box::new(FakeNativeBackend::new())),
        );
        assert_fixture(
            "hello.json",
            &serde_json::to_value(ServerFrame::Hello(server.hello())).expect("serialize hello"),
        );
        for (name, request) in scenario() {
            if name.starts_with("error.") {
                continue;
            }
            let _ = server.handle(&request);
        }
        server.publish(None, None, ApiEvent::Heartbeat);

        let events = server.frozen_events();
        // 1-2 seed the two source sessions, 3 starts `fake-1`, 4 is the
        // input that put it to work, 5 the reported status, 6 the stop, 7
        // the heartbeat published above.
        for (name, revision) in [
            ("session_started", 1u64),
            ("session_updated", 4),
            ("session_ended", 6),
            ("heartbeat", 7),
        ] {
            let frame = events
                .iter()
                .find(|frame| frame.revision == revision)
                .unwrap_or_else(|| {
                    panic!(
                        "no event at revision {revision}; the log was {:?}",
                        events.iter().map(|f| f.revision).collect::<Vec<_>>()
                    )
                });
            assert_fixture(
                &format!("event-{name}.json"),
                &serde_json::to_value(ServerFrame::Event(frame.clone())).expect("serialize event"),
            );
        }
    }

    /// A response fixture written by an older build must still PARSE here,
    /// even where this build would produce different bytes: that is the
    /// half of wire compatibility the byte comparison above cannot show.
    #[test]
    fn a_frozen_response_from_an_older_build_still_parses() {
        let older = r#"{
            "type": "response",
            "v": 1,
            "id": "req-ping",
            "revision": 2,
            "outcome": {"status": "ok", "result": {"server": "zirv", "protocol": 1}},
            "a_field_this_build_has_never_heard_of": {"nested": true}
        }"#;
        let frame: ServerFrame = serde_json::from_str(older).expect("parse");
        match frame {
            ServerFrame::Response(response) => {
                assert_eq!(response.version, PROTOCOL_VERSION);
                assert_eq!(response.id, "req-ping");
            }
            other => panic!("expected a response frame, got {other:?}"),
        }
    }
}
