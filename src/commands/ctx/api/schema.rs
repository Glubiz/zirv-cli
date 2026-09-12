//! `zirv ctx api schema [--json]` (issue #353): the protocol v1 contract,
//! generated from this binary's own types rather than hand-maintained next
//! to them.
//!
//! "Generated from the binary" means three concrete things here:
//!
//! - the method list, their parameter/result fields and which capability
//!   gates each one come from [`wire::METHODS`], which
//!   `wire::method_specs_cover_every_method` pins against the [`wire::Method`]
//!   enum itself;
//! - every published vocabulary's values come from `serde_json` serializing
//!   the real enum variants, so a rename on the type changes this output;
//! - the envelope field lists below are checked against a fully populated
//!   example of each real struct by `envelope_definitions_match_the_types`,
//!   so a field added to a frame cannot land undocumented.

use std::io::Write;

use serde_json::{Map, Value, json};

use super::wire::{
    self, ADVERTISED, ApiEvent, Capability, ErrorCode, FieldSpec, InputMode, Method,
    PROTOCOL_VERSION, SERVER_NAME, SessionFacts, SessionState, WaitUntil,
};
use crate::commands::ctx::CtxResult;
use crate::commands::ctx::runtime::{RuntimeKind, UiSurface};

/// The wire name of one serde-serializable unit enum variant. Panics are
/// impossible: every vocabulary in this module serializes to a JSON string.
fn variant_name<T: serde::Serialize>(value: &T) -> String {
    serde_json::to_value(value)
        .ok()
        .and_then(|value| value.as_str().map(str::to_string))
        .unwrap_or_else(|| "unknown".to_string())
}

fn names<T: serde::Serialize>(values: &[T]) -> Vec<String> {
    values.iter().map(variant_name).collect()
}

/// Every published vocabulary, in the order the human output prints them.
/// Each list deliberately ENDS with its `unknown` fallback so a reader can
/// see the forward-compat rule holds for all of them at a glance.
pub fn vocabularies() -> Vec<(&'static str, Vec<String>)> {
    vec![
        ("method", names(&all_methods())),
        ("capability", names(&all_capabilities())),
        ("error_code", names(&all_error_codes())),
        (
            "session_state",
            names(&[
                SessionState::Starting,
                SessionState::Idle,
                SessionState::Working,
                SessionState::Ended,
                SessionState::Unknown,
            ]),
        ),
        (
            "wait_until",
            names(&[WaitUntil::Idle, WaitUntil::Ended, WaitUntil::Unknown]),
        ),
        (
            "input_mode",
            names(&[InputMode::Submit, InputMode::Steer, InputMode::Unknown]),
        ),
        (
            "runtime_kind",
            names(&[
                RuntimeKind::Harness,
                RuntimeKind::Native,
                RuntimeKind::Unknown,
            ]),
        ),
        (
            "ui_surface",
            names(&[
                UiSurface::Headless,
                UiSurface::Terminal,
                UiSurface::DashboardPane,
                UiSurface::Unknown,
            ]),
        ),
        ("event_kind", event_kinds()),
    ]
}

fn all_methods() -> Vec<Method> {
    let mut methods: Vec<Method> = wire::METHODS.iter().map(|spec| spec.method).collect();
    methods.push(Method::Unknown);
    methods
}

fn all_capabilities() -> Vec<Capability> {
    let mut capabilities = ADVERTISED.to_vec();
    capabilities.push(Capability::Unknown);
    capabilities
}

fn all_error_codes() -> Vec<ErrorCode> {
    vec![
        ErrorCode::VersionMismatch,
        ErrorCode::UnknownMethod,
        ErrorCode::InvalidParams,
        ErrorCode::UnknownSession,
        ErrorCode::StaleGeneration,
        ErrorCode::Busy,
        ErrorCode::Unsupported,
        ErrorCode::Timeout,
        ErrorCode::Denied,
        ErrorCode::Internal,
        ErrorCode::Unknown,
    ]
}

/// The `kind` tag values [`ApiEvent`] publishes, read off real values so a
/// renamed variant changes this list.
fn event_kinds() -> Vec<String> {
    [
        ApiEvent::SessionStarted {
            session: SessionFacts::new("s"),
        },
        ApiEvent::SessionUpdated {
            session: SessionFacts::new("s"),
        },
        ApiEvent::SessionEnded {
            session_id: "s".to_string(),
        },
        ApiEvent::Heartbeat,
        ApiEvent::Unknown,
    ]
    .iter()
    .map(|event| {
        serde_json::to_value(event)
            .ok()
            .and_then(|value| {
                value
                    .get("kind")
                    .and_then(Value::as_str)
                    .map(str::to_string)
            })
            .unwrap_or_else(|| "unknown".to_string())
    })
    .collect()
}

// ---------------------------------------------------------------------------
// Envelope definitions
// ---------------------------------------------------------------------------

const REQUEST_FIELDS: &[FieldSpec] = &[
    FieldSpec {
        name: "v",
        ty: "integer",
        required: true,
        doc: "protocol version; a request with any other value is refused with version_mismatch",
    },
    FieldSpec {
        name: "id",
        ty: "string",
        required: true,
        doc: "caller-chosen request id, echoed on the reply",
    },
    FieldSpec {
        name: "method",
        ty: "method",
        required: true,
        doc: "",
    },
    FieldSpec {
        name: "params",
        ty: "object",
        required: false,
        doc: "method-specific; absent is the same as null",
    },
    FieldSpec {
        name: "idempotency_key",
        ty: "string",
        required: false,
        doc: "on a mutation, a retry with the same key returns the first result instead of repeating the work",
    },
];

const HELLO_FIELDS: &[FieldSpec] = &[
    FieldSpec {
        name: "v",
        ty: "integer",
        required: true,
        doc: "protocol version this server speaks",
    },
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
        name: "revision",
        ty: "integer",
        required: true,
        doc: "server revision at connect time",
    },
    FieldSpec {
        name: "capabilities",
        ty: "array",
        required: true,
        doc: "capability names; a client disables locally whatever is missing",
    },
];

const RESPONSE_FIELDS: &[FieldSpec] = &[
    FieldSpec {
        name: "v",
        ty: "integer",
        required: true,
        doc: "",
    },
    FieldSpec {
        name: "id",
        ty: "string",
        required: true,
        doc: "the request id this answers",
    },
    FieldSpec {
        name: "revision",
        ty: "integer",
        required: true,
        doc: "server revision when the reply was produced",
    },
    FieldSpec {
        name: "outcome",
        ty: "object",
        required: true,
        doc: "{\"status\":\"ok\",\"result\":{..}} or {\"status\":\"error\",\"error\":{\"code\":..,\"message\":..}}",
    },
];

const EVENT_FIELDS: &[FieldSpec] = &[
    FieldSpec {
        name: "v",
        ty: "integer",
        required: true,
        doc: "",
    },
    FieldSpec {
        name: "revision",
        ty: "integer",
        required: true,
        doc: "server-wide, +1 per event; a gap means the subscriber must refresh session.snapshot",
    },
    FieldSpec {
        name: "session_id",
        ty: "string",
        required: false,
        doc: "absent on server-wide events",
    },
    FieldSpec {
        name: "generation",
        ty: "integer",
        required: false,
        doc: "the session generation this event was recorded against",
    },
    FieldSpec {
        name: "payload",
        ty: "object",
        required: true,
        doc: "an event object tagged by \"kind\"",
    },
];

const SESSION_FACTS_FIELDS: &[FieldSpec] = &[
    FieldSpec {
        name: "session_id",
        ty: "string",
        required: true,
        doc: "stable across panes, tabs, worktrees and client attachment",
    },
    FieldSpec {
        name: "short",
        ty: "string",
        required: true,
        doc: "the same eight-character id the session registry uses",
    },
    FieldSpec {
        name: "runtime",
        ty: "runtime_kind",
        required: true,
        doc: "",
    },
    FieldSpec {
        name: "generation",
        ty: "integer",
        required: true,
        doc: "bumped by a resume; pinned by waits and mutations",
    },
    FieldSpec {
        name: "surface",
        ty: "ui_surface",
        required: true,
        doc: "which UI is attached; changing it changes nothing else",
    },
    FieldSpec {
        name: "state",
        ty: "session_state",
        required: true,
        doc: "",
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
        name: "repo_slug",
        ty: "string",
        required: false,
        doc: "sanitised repository slug; the absolute path is never published",
    },
    FieldSpec {
        name: "started_at",
        ty: "integer",
        required: false,
        doc: "epoch seconds",
    },
    FieldSpec {
        name: "reachable",
        ty: "boolean",
        required: true,
        doc: "whether the session can act on a wake-up",
    },
];

/// The transport half of the contract -- stated once, here, so a client
/// author does not have to read the README to connect.
fn transport() -> Value {
    json!({
        "framing": "ndjson",
        "encoding": "utf-8",
        "unix": "unix domain socket at <state>/s/api.sock, directory 0700, socket 0600, peer uid must equal the server's",
        "windows": "named pipe \\\\.\\pipe\\zirv-api-<state-hash>, created with an owner-only DACL",
        "endpoint_source": "always derived from the operator-owned zirv state directory; a repository can never name it",
        "handshake": "the server writes one hello frame before reading anything; the client disables locally whatever it does not find in hello.capabilities"
    })
}

fn field_schema(field: &FieldSpec) -> Value {
    let mut schema = match field.ty {
        "string" => json!({"type": "string"}),
        "integer" => json!({"type": "integer", "minimum": 0}),
        "boolean" => json!({"type": "boolean"}),
        "array" => json!({"type": "array"}),
        "object" => json!({"type": "object"}),
        other => json!({"$ref": format!("#/$defs/{other}")}),
    };
    if !field.doc.is_empty()
        && let Some(map) = schema.as_object_mut()
    {
        map.insert("description".to_string(), json!(field.doc));
    }
    schema
}

fn object_schema(fields: &[FieldSpec]) -> Value {
    let mut properties = Map::new();
    let mut required = Vec::new();
    for field in fields {
        properties.insert(field.name.to_string(), field_schema(field));
        if field.required {
            required.push(json!(field.name));
        }
    }
    json!({
        "type": "object",
        "properties": Value::Object(properties),
        "required": Value::Array(required),
        // Issue #353: "unknown fields ignored". Saying so in the schema keeps
        // a generated client from rejecting a frame a later build extended.
        "additionalProperties": true
    })
}

/// The whole contract as one JSON document.
pub fn json_schema() -> Value {
    let mut defs = Map::new();
    for (name, values) in vocabularies() {
        defs.insert(
            name.to_string(),
            json!({"type": "string", "enum": values}),
        );
    }
    defs.insert("request".to_string(), object_schema(REQUEST_FIELDS));
    defs.insert("hello".to_string(), object_schema(HELLO_FIELDS));
    defs.insert("response".to_string(), object_schema(RESPONSE_FIELDS));
    defs.insert("event_frame".to_string(), object_schema(EVENT_FIELDS));
    defs.insert(
        "session_facts".to_string(),
        object_schema(SESSION_FACTS_FIELDS),
    );

    let methods: Vec<Value> = wire::METHODS
        .iter()
        .map(|spec| {
            json!({
                "name": spec.name,
                "summary": spec.summary,
                "mutation": spec.mutation,
                "capability": spec.capability.as_str(),
                "params": object_schema(spec.params),
                "result": object_schema(spec.result),
            })
        })
        .collect();

    json!({
        "$schema": "https://json-schema.org/draft/2020-12/schema",
        "title": "zirv runtime protocol",
        "protocol": PROTOCOL_VERSION,
        "server": SERVER_NAME,
        "server_version": env!("CARGO_PKG_VERSION"),
        "transport": transport(),
        "capabilities": names(&all_capabilities()),
        "methods": methods,
        "$defs": Value::Object(defs),
    })
}

/// The human rendering: the same content, in the order a person reads it.
pub fn render_human(w: &mut dyn Write) -> CtxResult<()> {
    writeln!(
        w,
        "zirv runtime protocol v{PROTOCOL_VERSION} (server {} {})",
        SERVER_NAME,
        env!("CARGO_PKG_VERSION")
    )?;
    writeln!(w)?;
    writeln!(w, "transport")?;
    if let Some(map) = transport().as_object() {
        for (key, value) in map {
            writeln!(
                w,
                "  {key}: {}",
                value.as_str().unwrap_or_default()
            )?;
        }
    }
    writeln!(w)?;
    writeln!(w, "capabilities advertised by this build")?;
    for capability in ADVERTISED {
        writeln!(w, "  {capability}")?;
    }
    writeln!(w)?;
    writeln!(w, "methods")?;
    for spec in wire::METHODS {
        writeln!(
            w,
            "  {}{}",
            spec.name,
            if spec.mutation { "  [mutation]" } else { "" }
        )?;
        writeln!(w, "    {}", spec.summary)?;
        writeln!(w, "    capability: {}", spec.capability)?;
        writeln!(w, "    params:")?;
        render_fields(w, spec.params)?;
        writeln!(w, "    result:")?;
        render_fields(w, spec.result)?;
    }
    writeln!(w)?;
    writeln!(w, "frames")?;
    for (name, fields) in [
        ("request (client -> server)", REQUEST_FIELDS),
        ("hello (server -> client, first frame)", HELLO_FIELDS),
        ("response (server -> client)", RESPONSE_FIELDS),
        ("event (server -> client)", EVENT_FIELDS),
        ("session_facts", SESSION_FACTS_FIELDS),
    ] {
        writeln!(w, "  {name}")?;
        render_fields(w, fields)?;
    }
    writeln!(w)?;
    writeln!(w, "vocabularies (every one ends with its unknown fallback)")?;
    for (name, values) in vocabularies() {
        writeln!(w, "  {name}: {}", values.join(", "))?;
    }
    Ok(())
}

fn render_fields(w: &mut dyn Write, fields: &[FieldSpec]) -> CtxResult<()> {
    if fields.is_empty() {
        writeln!(w, "      (none)")?;
        return Ok(());
    }
    for field in fields {
        let required = if field.required {
            "required"
        } else {
            "optional"
        };
        if field.doc.is_empty() {
            writeln!(w, "      {} : {} ({required})", field.name, field.ty)?;
        } else {
            writeln!(
                w,
                "      {} : {} ({required}) -- {}",
                field.name, field.ty, field.doc
            )?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use super::super::wire::{ApiError, EventFrame, Hello, Outcome, Request, Response};

    /// A fully populated example of each real frame type, so the field
    /// lists this module publishes can be compared against what serde
    /// actually writes.
    fn populated_examples() -> Vec<(&'static str, &'static [FieldSpec], Value)> {
        let facts = SessionFacts {
            session_id: "11111111-2222-4333-8444-555555555555".to_string(),
            short: "11112222".to_string(),
            runtime: RuntimeKind::Harness,
            generation: 2,
            surface: UiSurface::DashboardPane,
            state: SessionState::Idle,
            role: Some("worker".to_string()),
            agent: Some("claude".to_string()),
            repo_slug: Some("zirv-cli".to_string()),
            started_at: Some(1_757_000_000),
            reachable: true,
        };
        vec![
            (
                "request",
                REQUEST_FIELDS,
                serde_json::to_value(
                    Request::new("r1", Method::ServerPing, json!({})).with_idempotency_key("k1"),
                )
                .expect("serialize request"),
            ),
            (
                "hello",
                HELLO_FIELDS,
                serde_json::to_value(Hello {
                    version: PROTOCOL_VERSION,
                    server: SERVER_NAME.to_string(),
                    server_version: "0.0.0".to_string(),
                    revision: 3,
                    capabilities: ADVERTISED.to_vec(),
                })
                .expect("serialize hello"),
            ),
            (
                "response",
                RESPONSE_FIELDS,
                serde_json::to_value(Response {
                    version: PROTOCOL_VERSION,
                    id: "r1".to_string(),
                    revision: 3,
                    outcome: Outcome::Error {
                        error: ApiError::new(ErrorCode::Busy, "busy"),
                    },
                })
                .expect("serialize response"),
            ),
            (
                "event_frame",
                EVENT_FIELDS,
                serde_json::to_value(EventFrame {
                    version: PROTOCOL_VERSION,
                    revision: 4,
                    session_id: Some(facts.session_id.clone()),
                    generation: Some(facts.generation),
                    payload: ApiEvent::SessionUpdated {
                        session: facts.clone(),
                    },
                })
                .expect("serialize event"),
            ),
            (
                "session_facts",
                SESSION_FACTS_FIELDS,
                serde_json::to_value(&facts).expect("serialize facts"),
            ),
        ]
    }

    /// The schema is only "generated from the binary's types" if a field
    /// added to, removed from or renamed on a frame struct changes it. The
    /// field lists here are static data, so this is what ties them to the
    /// real types: every key serde writes must be documented, and every
    /// documented key must be one serde can write.
    #[test]
    fn envelope_definitions_match_the_types() {
        for (name, fields, value) in populated_examples() {
            let actual: Vec<String> = value
                .as_object()
                .unwrap_or_else(|| panic!("{name} serializes to an object"))
                .keys()
                .cloned()
                .collect();
            let documented: Vec<String> =
                fields.iter().map(|field| field.name.to_string()).collect();
            for key in &actual {
                assert!(
                    documented.contains(key),
                    "{name}.{key} is on the wire but not in the published schema"
                );
            }
            for key in &documented {
                assert!(
                    actual.contains(key),
                    "{name}.{key} is published but the type never writes it"
                );
            }
        }
    }

    /// Every vocabulary's last value is its `unknown` fallback -- the
    /// forward-compat promise the human output makes in as many words.
    #[test]
    fn every_published_vocabulary_ends_with_unknown() {
        for (name, values) in vocabularies() {
            assert_eq!(
                values.last().map(String::as_str),
                Some("unknown"),
                "{name} must end with its unknown fallback, got {values:?}"
            );
        }
    }

    #[test]
    fn the_json_schema_documents_every_method_with_its_capability() {
        let schema = json_schema();
        let methods = schema["methods"].as_array().expect("methods array");
        assert_eq!(methods.len(), wire::METHODS.len());
        for (value, spec) in methods.iter().zip(wire::METHODS) {
            assert_eq!(value["name"], json!(spec.name));
            assert_eq!(value["capability"], json!(spec.capability.as_str()));
            assert_eq!(value["mutation"], json!(spec.mutation));
        }
        assert_eq!(schema["protocol"], json!(PROTOCOL_VERSION));
        // Unknown fields being ignored is part of the contract, so the
        // generated schema must not tell a client to reject them.
        assert_eq!(schema["$defs"]["request"]["additionalProperties"], json!(true));
    }

    #[test]
    fn the_human_schema_names_every_method_and_the_transport() {
        let mut out = Vec::new();
        render_human(&mut out).expect("render");
        let text = String::from_utf8(out).expect("utf-8");
        for spec in wire::METHODS {
            assert!(text.contains(spec.name), "{} is missing", spec.name);
        }
        assert!(text.contains("ndjson"));
        assert!(text.contains("named pipe"));
        assert!(text.contains("unix domain socket"));
        assert!(text.contains("[mutation]"));
    }
}
