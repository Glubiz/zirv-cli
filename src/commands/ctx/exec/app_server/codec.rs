//! Pure line codec and typed frames for the codex app-server protocol.
//!
//! Frames are one JSON object per line with NO `jsonrpc` field. Method names
//! and shapes were checked against `codex app-server generate-json-schema`
//! (codex-cli 0.163.0). No I/O and no clock in this file.

use std::collections::HashMap;
use std::fmt;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

/// Longest accepted line, newline excluded. A line over this is a typed error.
pub const MAX_LINE_BYTES: usize = 4 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CodecError {
    Oversize { limit: usize },
    NotUtf8,
    Malformed(String),
    NotAnObject,
    UnknownShape,
    UnmatchedResponse(RequestId),
    Encode(String),
}

impl fmt::Display for CodecError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Oversize { limit } => write!(f, "line exceeds {limit} bytes"),
            Self::NotUtf8 => write!(f, "line is not valid UTF-8"),
            Self::Malformed(e) => write!(f, "malformed JSON: {e}"),
            Self::NotAnObject => write!(f, "frame is not a JSON object"),
            Self::UnknownShape => write!(f, "frame is not a request, response or notification"),
            Self::UnmatchedResponse(id) => write!(f, "response for unknown request id {id:?}"),
            Self::Encode(e) => write!(f, "cannot encode frame: {e}"),
        }
    }
}

impl std::error::Error for CodecError {}

/// JSON-RPC request id: the schema allows a string or an int64.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(untagged)]
pub enum RequestId {
    Num(i64),
    Str(String),
}

#[derive(Debug, Clone, PartialEq)]
pub struct RpcError {
    pub code: i64,
    pub message: String,
    pub data: Option<Value>,
}

/// Client requests zirv uses. Params of everything but `initialize` stay
/// untyped until the slices that send them.
#[derive(Debug, Clone, PartialEq)]
pub enum ClientRequest {
    Initialize {
        client_name: String,
        client_version: String,
        experimental_api: bool,
    },
    ThreadStart(Value),
    // Resume, steer, interrupt and rate limits are sent by later #950 slices.
    #[allow(dead_code)]
    ThreadResume(Value),
    TurnStart(Value),
    #[allow(dead_code)]
    TurnSteer(Value),
    #[allow(dead_code)]
    TurnInterrupt(Value),
    /// No params in the schema.
    #[allow(dead_code)]
    RateLimitsRead,
}

impl ClientRequest {
    pub fn method(&self) -> &'static str {
        match self {
            Self::Initialize { .. } => "initialize",
            Self::ThreadStart(_) => "thread/start",
            Self::ThreadResume(_) => "thread/resume",
            Self::TurnStart(_) => "turn/start",
            Self::TurnSteer(_) => "turn/steer",
            Self::TurnInterrupt(_) => "turn/interrupt",
            Self::RateLimitsRead => "account/rateLimits/read",
        }
    }

    fn params(&self) -> Option<Value> {
        match self {
            Self::Initialize {
                client_name,
                client_version,
                experimental_api,
            } => Some(json!({
                "clientInfo": {"name": client_name, "version": client_version},
                "capabilities": {"experimentalApi": experimental_api},
            })),
            Self::ThreadStart(p)
            | Self::ThreadResume(p)
            | Self::TurnStart(p)
            | Self::TurnSteer(p)
            | Self::TurnInterrupt(p) => Some(p.clone()),
            Self::RateLimitsRead => None,
        }
    }
}

fn to_line(frame: Map<String, Value>) -> Result<String, CodecError> {
    let mut line = serde_json::to_string(&Value::Object(frame))
        .map_err(|e| CodecError::Encode(e.to_string()))?;
    line.push('\n');
    Ok(line)
}

/// One request as a newline-terminated line.
pub fn encode_request(id: &RequestId, req: &ClientRequest) -> Result<String, CodecError> {
    let id = serde_json::to_value(id).map_err(|e| CodecError::Encode(e.to_string()))?;
    let mut frame = Map::new();
    frame.insert("id".into(), id);
    frame.insert("method".into(), Value::String(req.method().into()));
    if let Some(params) = req.params() {
        frame.insert("params".into(), params);
    }
    to_line(frame)
}

/// The `initialized` client notification (the only one in the schema).
pub fn encode_initialized() -> Result<String, CodecError> {
    let mut frame = Map::new();
    frame.insert("method".into(), Value::String("initialized".into()));
    to_line(frame)
}

fn answer(id: &RequestId, key: &str, value: Value) -> Result<String, CodecError> {
    let id = serde_json::to_value(id).map_err(|e| CodecError::Encode(e.to_string()))?;
    let mut frame = Map::new();
    frame.insert("id".into(), id);
    frame.insert(key.into(), value);
    to_line(frame)
}

/// Answer to a server request, echoing the server's id.
pub fn encode_response(id: &RequestId, result: Value) -> Result<String, CodecError> {
    answer(id, "result", result)
}

/// Error answer to a server request, echoing the server's id.
pub fn encode_error_response(
    id: &RequestId,
    code: i64,
    message: &str,
) -> Result<String, CodecError> {
    answer(id, "error", json!({"code": code, "message": message}))
}

#[derive(Debug, Clone, PartialEq)]
pub enum ServerNotification {
    RateLimitsUpdated(Value),
    Other { method: String, params: Value },
}

/// Server-initiated requests: the approval family from the schema, plus a
/// catch-all so a new method never breaks decoding.
#[derive(Debug, Clone, PartialEq)]
pub enum ServerRequest {
    CommandApproval(Value),
    FileChangeApproval(Value),
    PermissionsApproval(Value),
    ToolUserInput(Value),
    McpElicitation(Value),
    LegacyApplyPatchApproval(Value),
    LegacyExecCommandApproval(Value),
    Other { method: String, params: Value },
}

#[derive(Debug, Clone, PartialEq)]
pub enum ServerFrame {
    Response {
        id: RequestId,
        result: Value,
    },
    Error {
        id: RequestId,
        error: RpcError,
    },
    Notification(ServerNotification),
    Request {
        id: RequestId,
        request: ServerRequest,
    },
}

fn notification_from(method: String, params: Value) -> ServerNotification {
    match method.as_str() {
        "account/rateLimits/updated" => ServerNotification::RateLimitsUpdated(params),
        _ => ServerNotification::Other { method, params },
    }
}

fn request_from(method: String, params: Value) -> ServerRequest {
    match method.as_str() {
        "item/commandExecution/requestApproval" => ServerRequest::CommandApproval(params),
        "item/fileChange/requestApproval" => ServerRequest::FileChangeApproval(params),
        "item/permissions/requestApproval" => ServerRequest::PermissionsApproval(params),
        "item/tool/requestUserInput" => ServerRequest::ToolUserInput(params),
        "mcpServer/elicitation/request" => ServerRequest::McpElicitation(params),
        "applyPatchApproval" => ServerRequest::LegacyApplyPatchApproval(params),
        "execCommandApproval" => ServerRequest::LegacyExecCommandApproval(params),
        _ => ServerRequest::Other { method, params },
    }
}

/// Decode one line (no trailing newline needed) into a typed server frame.
pub fn decode_line(line: &str) -> Result<ServerFrame, CodecError> {
    if line.len() > MAX_LINE_BYTES {
        return Err(CodecError::Oversize {
            limit: MAX_LINE_BYTES,
        });
    }
    let value: Value =
        serde_json::from_str(line).map_err(|e| CodecError::Malformed(e.to_string()))?;
    let Value::Object(mut obj) = value else {
        return Err(CodecError::NotAnObject);
    };
    let id = match obj.remove("id") {
        None => None,
        Some(v) => Some(
            serde_json::from_value::<RequestId>(v)
                .map_err(|e| CodecError::Malformed(format!("bad id: {e}")))?,
        ),
    };
    let method = match obj.remove("method") {
        None => None,
        Some(Value::String(m)) => Some(m),
        Some(_) => return Err(CodecError::Malformed("method is not a string".into())),
    };
    let params = obj.remove("params").unwrap_or(Value::Null);
    if let Some(method) = method {
        return Ok(match id {
            Some(id) => ServerFrame::Request {
                id,
                request: request_from(method, params),
            },
            None => ServerFrame::Notification(notification_from(method, params)),
        });
    }
    let Some(id) = id else {
        return Err(CodecError::UnknownShape);
    };
    if let Some(result) = obj.remove("result") {
        return Ok(ServerFrame::Response { id, result });
    }
    let Some(Value::Object(mut err)) = obj.remove("error") else {
        return Err(CodecError::UnknownShape);
    };
    let (Some(code), Some(Value::String(message))) = (
        err.remove("code").and_then(|c| c.as_i64()),
        err.remove("message"),
    ) else {
        return Err(CodecError::Malformed("error needs code and message".into()));
    };
    Ok(ServerFrame::Error {
        id,
        error: RpcError {
            code,
            message,
            data: err.remove("data"),
        },
    })
}

/// A resolved client request: its method and the server's answer.
#[derive(Debug, Clone, PartialEq)]
pub struct Resolved {
    pub method: &'static str,
    pub outcome: Result<Value, RpcError>,
}

/// Allocates request ids and matches responses back to the request.
#[derive(Debug, Default)]
pub struct Correlator {
    next: i64,
    pending: HashMap<RequestId, &'static str>,
}

impl Correlator {
    /// Allocate an id, remember the method, return the id and the line.
    pub fn send(&mut self, req: &ClientRequest) -> Result<(RequestId, String), CodecError> {
        self.next += 1;
        let id = RequestId::Num(self.next);
        let line = encode_request(&id, req)?;
        self.pending.insert(id.clone(), req.method());
        Ok((id, line))
    }

    /// `Ok(None)` for notifications and server requests; for a response or
    /// error, the matched request. An id nobody asked for is a typed error.
    pub fn resolve(&mut self, frame: &ServerFrame) -> Result<Option<Resolved>, CodecError> {
        let (id, outcome) = match frame {
            ServerFrame::Response { id, result } => (id, Ok(result.clone())),
            ServerFrame::Error { id, error } => (id, Err(error.clone())),
            ServerFrame::Notification(_) | ServerFrame::Request { .. } => return Ok(None),
        };
        let Some(method) = self.pending.remove(id) else {
            return Err(CodecError::UnmatchedResponse(id.clone()));
        };
        Ok(Some(Resolved { method, outcome }))
    }

    #[cfg(test)]
    pub fn in_flight(&self) -> usize {
        self.pending.len()
    }
}

/// Splits a byte stream into lines, bounding line length. Pure: the caller
/// feeds chunks. An oversize line yields one error and is discarded through
/// its newline, so the stream resynchronises on the next line.
#[derive(Debug)]
pub struct LineSplitter {
    limit: usize,
    buf: Vec<u8>,
    discarding: bool,
}

impl Default for LineSplitter {
    fn default() -> Self {
        Self::with_limit(MAX_LINE_BYTES)
    }
}

impl LineSplitter {
    pub fn with_limit(limit: usize) -> Self {
        Self {
            limit,
            buf: Vec::new(),
            discarding: false,
        }
    }

    pub fn push(&mut self, chunk: &[u8]) -> Vec<Result<String, CodecError>> {
        let mut out = Vec::new();
        for &b in chunk {
            if b == b'\n' {
                if self.discarding {
                    self.discarding = false;
                } else if let Some(line) = self.take_line() {
                    out.push(line);
                }
                self.buf.clear();
                continue;
            }
            if self.discarding {
                continue;
            }
            self.buf.push(b);
            if self.buf.len() > self.limit {
                out.push(Err(CodecError::Oversize { limit: self.limit }));
                self.buf.clear();
                self.discarding = true;
            }
        }
        out
    }

    fn take_line(&self) -> Option<Result<String, CodecError>> {
        let bytes = self.buf.strip_suffix(b"\r").unwrap_or(&self.buf);
        if bytes.is_empty() {
            return None;
        }
        Some(
            std::str::from_utf8(bytes)
                .map(str::to_owned)
                .map_err(|_| CodecError::NotUtf8),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn init() -> ClientRequest {
        ClientRequest::Initialize {
            client_name: "zirv".into(),
            client_version: "1.2.3".into(),
            experimental_api: false,
        }
    }

    #[test]
    fn request_encodes_one_line_without_jsonrpc_field() {
        let line = encode_request(&RequestId::Num(7), &init()).unwrap();
        assert!(line.ends_with('\n'));
        assert_eq!(line.matches('\n').count(), 1);
        let v: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(
            v,
            json!({"id":7,"method":"initialize","params":{
                "clientInfo":{"name":"zirv","version":"1.2.3"},
                "capabilities":{"experimentalApi":false}}})
        );
        assert!(v.get("jsonrpc").is_none());
    }

    #[test]
    fn every_client_request_has_its_schema_method() {
        let p = json!({});
        let cases = [
            (ClientRequest::ThreadStart(p.clone()), "thread/start"),
            (ClientRequest::ThreadResume(p.clone()), "thread/resume"),
            (ClientRequest::TurnStart(p.clone()), "turn/start"),
            (ClientRequest::TurnSteer(p.clone()), "turn/steer"),
            (ClientRequest::TurnInterrupt(p), "turn/interrupt"),
            (ClientRequest::RateLimitsRead, "account/rateLimits/read"),
        ];
        for (req, method) in cases {
            let v: Value =
                serde_json::from_str(&encode_request(&RequestId::Num(1), &req).unwrap()).unwrap();
            assert_eq!(v["method"], method);
        }
        let v: Value = serde_json::from_str(
            &encode_request(&RequestId::Num(1), &ClientRequest::RateLimitsRead).unwrap(),
        )
        .unwrap();
        assert!(v.get("params").is_none(), "rateLimits/read has no params");
    }

    #[test]
    fn initialized_and_response_lines() {
        assert_eq!(
            encode_initialized().unwrap(),
            "{\"method\":\"initialized\"}\n"
        );
        let line =
            encode_response(&RequestId::Str("a".into()), json!({"decision":"decline"})).unwrap();
        let v: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(v, json!({"id":"a","result":{"decision":"decline"}}));
        let line = encode_error_response(&RequestId::Num(4), -32000, "no").unwrap();
        assert_eq!(line.matches('\n').count(), 1);
        let v: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(v, json!({"id":4,"error":{"code":-32000,"message":"no"}}));
    }

    #[test]
    fn decodes_response_error_notification_and_requests() {
        assert_eq!(
            decode_line(r#"{"id":1,"result":{"ok":true}}"#).unwrap(),
            ServerFrame::Response {
                id: RequestId::Num(1),
                result: json!({"ok":true})
            }
        );
        let ServerFrame::Error { id, error } =
            decode_line(r#"{"id":"x","error":{"code":-32600,"message":"bad","data":[1]}}"#)
                .unwrap()
        else {
            panic!("expected error frame");
        };
        assert_eq!(id, RequestId::Str("x".into()));
        assert_eq!(error.code, -32600);
        assert_eq!(error.message, "bad");
        assert_eq!(error.data, Some(json!([1])));
        assert_eq!(
            decode_line(r#"{"method":"account/rateLimits/updated","params":{"a":1}}"#).unwrap(),
            ServerFrame::Notification(ServerNotification::RateLimitsUpdated(json!({"a":1})))
        );
        assert_eq!(
            decode_line(r#"{"method":"turn/started","params":{}}"#).unwrap(),
            ServerFrame::Notification(ServerNotification::Other {
                method: "turn/started".into(),
                params: json!({})
            })
        );
        let approvals = [
            ("item/commandExecution/requestApproval", "CommandApproval"),
            ("item/fileChange/requestApproval", "FileChangeApproval"),
            ("item/permissions/requestApproval", "PermissionsApproval"),
            ("item/tool/requestUserInput", "ToolUserInput"),
            ("mcpServer/elicitation/request", "McpElicitation"),
            ("applyPatchApproval", "LegacyApplyPatchApproval"),
            ("execCommandApproval", "LegacyExecCommandApproval"),
            ("item/tool/call", "Other"),
        ];
        for (method, variant) in approvals {
            let line = format!(r#"{{"id":9,"method":"{method}","params":{{}}}}"#);
            let ServerFrame::Request { id, request } = decode_line(&line).unwrap() else {
                panic!("expected request for {method}");
            };
            assert_eq!(id, RequestId::Num(9));
            assert!(
                format!("{request:?}").starts_with(variant),
                "{method}: {request:?}"
            );
        }
    }

    #[test]
    fn malformed_lines_are_typed_errors() {
        assert!(matches!(
            decode_line("not json"),
            Err(CodecError::Malformed(_))
        ));
        assert_eq!(decode_line("[1]"), Err(CodecError::NotAnObject));
        assert_eq!(decode_line("{}"), Err(CodecError::UnknownShape));
        assert_eq!(decode_line(r#"{"id":1}"#), Err(CodecError::UnknownShape));
        assert!(matches!(
            decode_line(r#"{"id":1.5,"result":1}"#),
            Err(CodecError::Malformed(_))
        ));
        assert!(matches!(
            decode_line(r#"{"method":3}"#),
            Err(CodecError::Malformed(_))
        ));
        assert!(matches!(
            decode_line(r#"{"id":1,"error":{"code":1}}"#),
            Err(CodecError::Malformed(_))
        ));
        assert!(matches!(
            decode_line(r#"{"id":1,"error":"x"}"#),
            Err(CodecError::UnknownShape)
        ));
        let huge = format!("{{\"method\":\"{}\"}}", "a".repeat(MAX_LINE_BYTES));
        assert!(matches!(
            decode_line(&huge),
            Err(CodecError::Oversize { .. })
        ));
    }

    #[test]
    fn correlator_matches_by_id_and_rejects_strangers() {
        let mut c = Correlator::default();
        let (a, _) = c.send(&init()).unwrap();
        let (b, _) = c.send(&ClientRequest::RateLimitsRead).unwrap();
        assert_ne!(a, b);
        assert_eq!(c.in_flight(), 2);
        // Out-of-order answer resolves to the right method.
        let r = c
            .resolve(&ServerFrame::Response {
                id: b.clone(),
                result: json!(1),
            })
            .unwrap()
            .unwrap();
        assert_eq!(r.method, "account/rateLimits/read");
        assert_eq!(r.outcome, Ok(json!(1)));
        let e = RpcError {
            code: 1,
            message: "m".into(),
            data: None,
        };
        let r = c
            .resolve(&ServerFrame::Error {
                id: a.clone(),
                error: e.clone(),
            })
            .unwrap()
            .unwrap();
        assert_eq!(r.method, "initialize");
        assert_eq!(r.outcome, Err(e));
        assert_eq!(c.in_flight(), 0);
        // Answered twice, or never asked.
        assert_eq!(
            c.resolve(&ServerFrame::Response {
                id: a.clone(),
                result: json!(0)
            }),
            Err(CodecError::UnmatchedResponse(a))
        );
        let note = decode_line(r#"{"method":"x"}"#).unwrap();
        assert_eq!(c.resolve(&note), Ok(None));
    }

    #[test]
    fn splitter_handles_partial_chunks_crlf_and_blank_lines() {
        let mut s = LineSplitter::default();
        assert!(s.push(b"{\"a\"").is_empty());
        let out = s.push(b":1}\r\n\n{\"b\":2}\nrest");
        assert_eq!(
            out,
            vec![Ok("{\"a\":1}".to_string()), Ok("{\"b\":2}".to_string())]
        );
        assert_eq!(s.push(b"\n"), vec![Ok("rest".to_string())]);
    }

    #[test]
    fn splitter_bounds_line_length_and_resyncs() {
        let mut s = LineSplitter::with_limit(4);
        let out = s.push(b"abcd\nabcdefgh\nok\n");
        assert_eq!(
            out,
            vec![
                Ok("abcd".to_string()),
                Err(CodecError::Oversize { limit: 4 }),
                Ok("ok".to_string()),
            ]
        );
        // The oversize line spans chunks: still one error, then resync.
        let mut s = LineSplitter::with_limit(4);
        let mut out = s.push(b"abcdef");
        out.extend(s.push(b"ghij"));
        out.extend(s.push(b"\nz\n"));
        assert_eq!(
            out,
            vec![Err(CodecError::Oversize { limit: 4 }), Ok("z".to_string())]
        );
    }

    #[test]
    fn splitter_reports_invalid_utf8() {
        let mut s = LineSplitter::default();
        assert_eq!(
            s.push(b"\xff\xfe\nok\n"),
            vec![Err(CodecError::NotUtf8), Ok("ok".to_string())]
        );
    }
}
