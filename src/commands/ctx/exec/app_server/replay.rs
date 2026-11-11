//! Scripted replay harness: a transcript of `expect_outbound` and
//! `emit_inbound` steps (the shape of T3's `replay.ts`) driven against the
//! codec as a fake server. Transcript text uses one step per line: `> <json>`
//! expects an outbound frame, `< <json>` emits an inbound one. The same file
//! feeds `tests/fixtures/fake-app-server.sh`, which only plays the `<` lines.

use std::collections::VecDeque;
use std::fmt;

use serde_json::Value;

#[derive(Debug, Clone, PartialEq)]
pub enum Step {
    ExpectOutbound(Value),
    EmitInbound(Value),
}

#[derive(Debug, Clone, PartialEq)]
pub enum ReplayError {
    /// A transcript line is not `> <json>` / `< <json>`.
    BadTranscript { line: usize, reason: String },
    /// The client sent something that is not JSON.
    BadOutbound(String),
    /// The client sent a different frame than the script expects.
    Mismatch {
        step: usize,
        expected: Value,
        got: Value,
    },
    /// The client sent a frame after the script wanted to emit or had ended.
    Unexpected { step: usize, got: Value },
    /// The script ended with steps the client never reached.
    Unfinished { remaining: usize },
}

impl fmt::Display for ReplayError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::BadTranscript { line, reason } => write!(f, "transcript line {line}: {reason}"),
            Self::BadOutbound(e) => write!(f, "outbound frame is not JSON: {e}"),
            Self::Mismatch {
                step,
                expected,
                got,
            } => write!(f, "step {step}: expected {expected}, got {got}"),
            Self::Unexpected { step, got } => write!(f, "step {step}: unexpected frame {got}"),
            Self::Unfinished { remaining } => write!(f, "{remaining} step(s) never reached"),
        }
    }
}

impl std::error::Error for ReplayError {}

pub fn parse_transcript(text: &str) -> Result<Vec<Step>, ReplayError> {
    let mut steps = Vec::new();
    for (i, raw) in text.lines().enumerate() {
        let raw = raw.trim();
        if raw.is_empty() || raw.starts_with('#') {
            continue;
        }
        let bad = |reason: &str| ReplayError::BadTranscript {
            line: i + 1,
            reason: reason.to_string(),
        };
        let mut chars = raw.chars();
        let marker = chars.next();
        let value: Value =
            serde_json::from_str(chars.as_str().trim()).map_err(|e| bad(&e.to_string()))?;
        steps.push(match marker {
            Some('>') => Step::ExpectOutbound(value),
            Some('<') => Step::EmitInbound(value),
            _ => return Err(bad("line must start with '>' or '<'")),
        });
    }
    Ok(steps)
}

/// Plays the server side of a transcript.
#[derive(Debug)]
pub struct FakeServer {
    steps: VecDeque<(usize, Step)>,
}

impl FakeServer {
    pub fn new(steps: Vec<Step>) -> Self {
        Self {
            steps: steps.into_iter().enumerate().collect(),
        }
    }

    /// Inbound lines the script emits next, before waiting for the client.
    pub fn drain_emits(&mut self) -> Vec<String> {
        let mut out = Vec::new();
        while let Some((_, Step::EmitInbound(_))) = self.steps.front() {
            if let Some((_, Step::EmitInbound(v))) = self.steps.pop_front() {
                out.push(format!("{v}\n"));
            }
        }
        out
    }

    /// Check one outbound line against the next expectation, then return the
    /// inbound lines the script emits in answer.
    pub fn on_outbound(&mut self, line: &str) -> Result<Vec<String>, ReplayError> {
        let got: Value = serde_json::from_str(line.trim())
            .map_err(|e| ReplayError::BadOutbound(e.to_string()))?;
        match self.steps.pop_front() {
            Some((_, Step::ExpectOutbound(expected))) if expected == got => Ok(self.drain_emits()),
            Some((step, Step::ExpectOutbound(expected))) => Err(ReplayError::Mismatch {
                step,
                expected,
                got,
            }),
            Some((step, Step::EmitInbound(_))) => Err(ReplayError::Unexpected { step, got }),
            None => Err(ReplayError::Unexpected { step: 0, got }),
        }
    }

    pub fn finish(&self) -> Result<(), ReplayError> {
        match self.steps.len() {
            0 => Ok(()),
            remaining => Err(ReplayError::Unfinished { remaining }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::codec::{
        ClientRequest, Correlator, ServerFrame, decode_line, encode_initialized,
    };
    use super::*;

    const HANDSHAKE: &str =
        include_str!("../../../../../tests/fixtures/app-server-handshake.jsonl");

    fn init() -> ClientRequest {
        ClientRequest::Initialize {
            client_name: "zirv".into(),
            client_version: "0.0.0".into(),
            experimental_api: false,
        }
    }

    #[test]
    fn handshake_transcript_replays_through_the_codec() {
        let mut server = FakeServer::new(parse_transcript(HANDSHAKE).unwrap());
        assert!(server.drain_emits().is_empty());
        let mut corr = Correlator::default();
        let (_, line) = corr.send(&init()).unwrap();
        let replies = server.on_outbound(&line).unwrap();
        assert_eq!(replies.len(), 1);
        let frame = decode_line(&replies[0]).unwrap();
        assert!(matches!(frame, ServerFrame::Response { .. }));
        let resolved = corr.resolve(&frame).unwrap().unwrap();
        assert_eq!(resolved.method, "initialize");
        assert!(resolved.outcome.is_ok());
        assert!(
            server
                .on_outbound(&encode_initialized().unwrap())
                .unwrap()
                .is_empty()
        );
        server.finish().unwrap();
    }

    #[test]
    fn mismatched_outbound_frame_is_detected() {
        let mut server = FakeServer::new(parse_transcript(HANDSHAKE).unwrap());
        let wrong = ClientRequest::Initialize {
            client_name: "zirv".into(),
            client_version: "9.9.9".into(),
            experimental_api: false,
        };
        let (_, line) = Correlator::default().send(&wrong).unwrap();
        assert!(matches!(
            server.on_outbound(&line),
            Err(ReplayError::Mismatch { step: 0, .. })
        ));
    }

    #[test]
    fn extra_frame_and_unfinished_script_are_detected() {
        let steps = parse_transcript("> {\"a\":1}\n< {\"b\":2}\n> {\"c\":3}\n").unwrap();
        let mut server = FakeServer::new(steps);
        assert_eq!(
            server.on_outbound("{\"a\":1}").unwrap(),
            vec!["{\"b\":2}\n".to_string()]
        );
        assert_eq!(
            server.finish(),
            Err(ReplayError::Unfinished { remaining: 1 })
        );
        server.on_outbound("{\"c\":3}").unwrap();
        assert!(matches!(
            server.on_outbound("{\"d\":4}"),
            Err(ReplayError::Unexpected { .. })
        ));
        assert!(matches!(
            server.on_outbound("nope"),
            Err(ReplayError::BadOutbound(_))
        ));
    }

    #[test]
    fn leading_emits_are_server_initiated() {
        let mut server =
            FakeServer::new(parse_transcript("< {\"method\":\"hi\"}\n> {}\n").unwrap());
        assert_eq!(
            server.drain_emits(),
            vec!["{\"method\":\"hi\"}\n".to_string()]
        );
    }

    #[test]
    fn bad_transcript_lines_are_typed_errors() {
        assert!(matches!(
            parse_transcript("? {}"),
            Err(ReplayError::BadTranscript { line: 1, .. })
        ));
        assert!(matches!(
            parse_transcript("# c\n> nope"),
            Err(ReplayError::BadTranscript { line: 2, .. })
        ));
    }
}
