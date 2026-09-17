//! TypeSafe's Jev decider (issue #537 seam), verified 2026-09-17 against
//! `docs.typesafe.ai`: `POST {base_url}/systemone`, bearer-authenticated,
//! one bounded request/response, no retries, no streaming.
//!
//! Request body:
//!
//! ```json
//! {"state": <string|object|array>, "model": "jev-latest",
//!  "questions": {"<id>": {"type": "choice", "instructions": "...",
//!                         "criteria": {"opt": "description or null", ...}},
//!                "<id>": {"type": "score", "instructions": "...",
//!                         "criteria": ["level 0 desc", "level 1 desc", ...]},
//!                "<id>": {"type": "noul", "instructions": "...",
//!                         "criteria": {"true": "...", "false": "..."}}}}
//! ```
//!
//! Response body:
//!
//! ```json
//! {"model": "jev-latest",
//!  "answers": {"<id>": {"type": "choice", "choice": "technical",
//!                       "probabilities": {"billing": 0.159, "technical": 0.84, "sales": 0.001},
//!                       "confidence": 0.596},
//!              "<id>": {"type": "score", "score": 1.035,
//!                       "legend": {"0": "...", "1": "...", "2": "..."},
//!                       "probabilities": {"0": 0.1, "1": 0.8, "2": 0.1},
//!                       "confidence": 0.842},
//!              "<id>": {"type": "noul", "noul": 0.999}},
//!  "usage": {"input_tokens": 312, "output_tokens": 48}}
//! ```
//!
//! Errors: 401 -> `Auth`, 422 -> `Invalid`, 429 -> `RateLimited`,
//! 529 -> `Overloaded`, any other status -> `Status(code)`; a connect/read
//! timeout -> `Timeout`; any other transport failure -> `Transport`; an
//! unparsable response body -> `Malformed`; the credential env unset or
//! empty -> `NoCredential`, checked BEFORE opening any connection.

use std::collections::BTreeMap;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use super::decision::{Answer, AnswerValue, Answers, Criteria, IntakeState, Question, Usage};
use crate::commands::ctx::config::ProxyTypesafeConfig;

#[derive(Debug)]
pub enum TypesafeError {
    /// The environment variable named by `credential_env` is unset or
    /// empty. Checked before any connection is opened.
    NoCredential(String),
    Auth,
    Invalid,
    RateLimited,
    Overloaded,
    Status(u16),
    Timeout,
    Transport(String),
    Malformed(String),
}

impl std::fmt::Display for TypesafeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoCredential(var) => write!(f, "credential env {var} unset"),
            Self::Auth => write!(f, "authentication rejected (401)"),
            Self::Invalid => write!(f, "invalid request (422)"),
            Self::RateLimited => write!(f, "rate limited (429)"),
            Self::Overloaded => write!(f, "overloaded (529)"),
            Self::Status(status) => write!(f, "unexpected status {status}"),
            Self::Timeout => write!(f, "timed out"),
            Self::Transport(detail) => write!(f, "transport error: {detail}"),
            Self::Malformed(detail) => write!(f, "malformed response: {detail}"),
        }
    }
}

impl std::error::Error for TypesafeError {}

#[derive(Debug, Serialize)]
struct NoulCriteria {
    #[serde(rename = "true", skip_serializing_if = "Option::is_none")]
    when_true: Option<String>,
    #[serde(rename = "false", skip_serializing_if = "Option::is_none")]
    when_false: Option<String>,
}

#[derive(Debug, Serialize)]
#[serde(tag = "type", rename_all = "lowercase")]
enum QuestionSpec {
    Choice {
        instructions: String,
        criteria: BTreeMap<String, Option<String>>,
    },
    Score {
        instructions: String,
        criteria: Vec<String>,
    },
    Noul {
        instructions: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        criteria: Option<NoulCriteria>,
    },
}

#[derive(Debug, Serialize)]
pub struct SystemOneRequest {
    state: serde_json::Value,
    model: String,
    questions: BTreeMap<String, QuestionSpec>,
}

#[derive(Debug, Deserialize)]
pub struct SystemOneUsage {
    pub input_tokens: u64,
    pub output_tokens: u64,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum SystemOneAnswer {
    Choice {
        choice: String,
        // Part of the documented wire shape (asserted on directly in this
        // module's own tests); `to_answer` derives the neutral `Answer`
        // from `choice`/`confidence` alone and does not read the
        // distribution itself.
        #[serde(default)]
        #[allow(dead_code)]
        probabilities: BTreeMap<String, f64>,
        confidence: f32,
    },
    Score {
        score: f64,
        #[serde(default)]
        legend: BTreeMap<String, String>,
        #[serde(default)]
        #[allow(dead_code)]
        probabilities: BTreeMap<String, f64>,
        confidence: f32,
    },
    Noul {
        noul: f64,
    },
}

#[derive(Debug, Deserialize)]
pub struct SystemOneResponse {
    // Kept for parity with the documented response shape and asserted on in
    // this module's own tests; `decide` reads `answers`/`usage` only.
    #[allow(dead_code)]
    pub model: String,
    pub answers: BTreeMap<String, SystemOneAnswer>,
    pub usage: SystemOneUsage,
}

fn build_request(intake: &IntakeState, questions: &[Question], model: &str) -> SystemOneRequest {
    let mut mapped = BTreeMap::new();
    for question in questions {
        let spec = match &question.criteria {
            Criteria::Choice(options) => QuestionSpec::Choice {
                instructions: question.instructions.clone(),
                criteria: options.iter().cloned().collect(),
            },
            Criteria::Score(levels) => QuestionSpec::Score {
                instructions: question.instructions.clone(),
                criteria: levels.clone(),
            },
            Criteria::Noul {
                when_true,
                when_false,
            } => QuestionSpec::Noul {
                instructions: question.instructions.clone(),
                criteria: if when_true.is_some() || when_false.is_some() {
                    Some(NoulCriteria {
                        when_true: when_true.clone(),
                        when_false: when_false.clone(),
                    })
                } else {
                    None
                },
            },
        };
        mapped.insert(question.id.clone(), spec);
    }
    SystemOneRequest {
        state: serde_json::to_value(intake).unwrap_or(serde_json::Value::Null),
        model: model.to_string(),
        questions: mapped,
    }
}

/// Converts one documented answer into the neutral [`Answer`] shape: for
/// `score`, the level index is `round(score)` clamped into the legend's own
/// range and `confidence` is the answer's own `confidence`; for `noul`,
/// there is no `confidence` field in the wire format at all, so the raw
/// value doubles as its own confidence (a reading near 0 or 1 is a
/// confident answer either way; near 0.5 is not).
fn to_answer(raw: &SystemOneAnswer) -> Answer {
    match raw {
        SystemOneAnswer::Choice {
            choice, confidence, ..
        } => Answer {
            value: AnswerValue::Choice(choice.clone()),
            confidence: *confidence,
        },
        SystemOneAnswer::Score {
            score,
            legend,
            confidence,
            ..
        } => {
            let max_level = legend
                .keys()
                .filter_map(|key| key.parse::<i64>().ok())
                .max()
                .unwrap_or(0);
            let index = score.round().clamp(0.0, max_level as f64);
            Answer {
                value: AnswerValue::Score(index),
                confidence: *confidence,
            }
        }
        SystemOneAnswer::Noul { noul } => Answer {
            value: AnswerValue::Noul(*noul),
            confidence: *noul as f32,
        },
    }
}

fn to_answers(questions: &[Question], raw: &BTreeMap<String, SystemOneAnswer>) -> Answers {
    let mut out = Answers::new();
    for question in questions {
        if let Some(answer) = raw.get(&question.id) {
            out.insert(question.id.clone(), to_answer(answer));
        }
    }
    out
}

fn status_error(status: u16) -> TypesafeError {
    match status {
        401 => TypesafeError::Auth,
        422 => TypesafeError::Invalid,
        429 => TypesafeError::RateLimited,
        529 => TypesafeError::Overloaded,
        other => TypesafeError::Status(other),
    }
}

/// Runs one bounded `/systemone` call and converts its answers into the
/// neutral [`Answers`] shape. `credential_env` is read fresh every call and
/// never logged; an unset or empty value refuses before any connection is
/// opened, matching every other credential-by-env-name seam in this crate
/// (`EndpointTarget`, `AgentAdapter::ready`).
pub fn decide(
    cfg: &ProxyTypesafeConfig,
    intake: &IntakeState,
    questions: &[Question],
) -> Result<(Answers, Usage), TypesafeError> {
    let credential = match std::env::var(&cfg.credential_env) {
        Ok(value) if !value.is_empty() => value,
        _ => return Err(TypesafeError::NoCredential(cfg.credential_env.clone())),
    };

    let request = build_request(intake, questions, &cfg.model);
    let payload = serde_json::to_string(&request)
        .map_err(|error| TypesafeError::Transport(format!("failed to encode request: {error}")))?;

    let agent: ureq::Agent = ureq::Agent::config_builder()
        .max_redirects(0)
        .timeout_connect(Some(Duration::from_secs(cfg.timeout_secs)))
        .timeout_global(Some(Duration::from_secs(cfg.timeout_secs)))
        .build()
        .into();

    let url = format!("{}/systemone", cfg.base_url.trim_end_matches('/'));
    let response = agent
        .post(&url)
        .header("authorization", format!("Bearer {credential}"))
        .header("content-type", "application/json")
        .header("user-agent", format!("zirv/{}", env!("CARGO_PKG_VERSION")))
        .send(payload);

    let mut response = match response {
        Ok(response) => response,
        Err(ureq::Error::StatusCode(status)) => return Err(status_error(status)),
        Err(ureq::Error::Timeout(_)) => return Err(TypesafeError::Timeout),
        Err(error) => return Err(TypesafeError::Transport(error.to_string())),
    };

    let status = response.status().as_u16();
    if status != 200 {
        return Err(status_error(status));
    }

    let body = response
        .body_mut()
        .read_to_string()
        .map_err(|error| TypesafeError::Transport(error.to_string()))?;
    let parsed: SystemOneResponse =
        serde_json::from_str(&body).map_err(|error| TypesafeError::Malformed(error.to_string()))?;

    let answers = to_answers(questions, &parsed.answers);
    Ok((
        answers,
        Usage {
            input_tokens: parsed.usage.input_tokens,
            output_tokens: parsed.usage.output_tokens,
        },
    ))
}

#[cfg(test)]
mod tests {
    use super::super::decision::QuestionKind;
    use super::*;
    use std::io::{Read, Write};
    use std::net::{TcpListener, TcpStream};
    use std::sync::mpsc;

    fn fixture(name: &str) -> std::path::PathBuf {
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests")
            .join("fixtures")
            .join(name)
    }

    fn sample_intake() -> IntakeState {
        IntakeState {
            request: "fix the typo".to_string(),
            repository: super::super::decision::IntakeRepository {
                name: "repo".to_string(),
                uncommitted_or_branch_changes: super::super::decision::BranchChanges {
                    files: 1,
                    lines: 2,
                },
                active_workflow: None,
                primary_extensions: vec!["md".to_string()],
            },
            harnesses: Vec::new(),
            workflows: Vec::new(),
            policy: super::super::decision::IntakePolicy {
                native_available: false,
            },
        }
    }

    fn sample_questions() -> Vec<Question> {
        vec![
            Question {
                id: "intent".to_string(),
                kind: QuestionKind::Choice,
                instructions: "pick one".to_string(),
                criteria: Criteria::Choice(vec![
                    ("feature".to_string(), Some("adds behavior".to_string())),
                    ("other".to_string(), None),
                ]),
            },
            Question {
                id: "complexity".to_string(),
                kind: QuestionKind::Score,
                instructions: "how complex".to_string(),
                criteria: Criteria::Score(vec!["trivial".to_string(), "bounded".to_string()]),
            },
            Question {
                id: "needs_clarification".to_string(),
                kind: QuestionKind::Noul,
                instructions: "ambiguous?".to_string(),
                criteria: Criteria::Noul {
                    when_true: Some("yes".to_string()),
                    when_false: Some("no".to_string()),
                },
            },
        ]
    }

    fn config(base_url: String, credential_env: &str, timeout_secs: u64) -> ProxyTypesafeConfig {
        ProxyTypesafeConfig {
            base_url,
            credential_env: credential_env.to_string(),
            model: "jev-latest".to_string(),
            timeout_secs,
        }
    }

    #[test]
    fn request_serde_matches_the_documented_shape() {
        let request = build_request(&sample_intake(), &sample_questions(), "jev-latest");
        let value = serde_json::to_value(&request).expect("serialize");
        assert_eq!(value["model"], "jev-latest");
        assert_eq!(value["questions"]["intent"]["type"], "choice");
        assert_eq!(
            value["questions"]["intent"]["criteria"]["feature"],
            "adds behavior"
        );
        assert!(value["questions"]["intent"]["criteria"]["other"].is_null());
        assert_eq!(value["questions"]["complexity"]["type"], "score");
        assert_eq!(value["questions"]["complexity"]["criteria"][0], "trivial");
        assert_eq!(value["questions"]["needs_clarification"]["type"], "noul");
        assert_eq!(
            value["questions"]["needs_clarification"]["criteria"]["true"],
            "yes"
        );
        assert!(value["state"]["request"] == "fix the typo");
    }

    #[test]
    fn the_fixture_response_deserializes_and_converts() {
        let text = std::fs::read_to_string(fixture("proxy/jev-response.json")).expect("fixture");
        let parsed: SystemOneResponse = serde_json::from_str(&text).expect("parse fixture");
        assert_eq!(parsed.model, "jev-latest");
        assert_eq!(parsed.usage.input_tokens, 312);
        assert_eq!(parsed.usage.output_tokens, 48);
        let questions = vec![
            Question {
                id: "category".to_string(),
                kind: QuestionKind::Choice,
                instructions: String::new(),
                criteria: Criteria::Choice(Vec::new()),
            },
            Question {
                id: "urgency".to_string(),
                kind: QuestionKind::Score,
                instructions: String::new(),
                criteria: Criteria::Score(Vec::new()),
            },
            Question {
                id: "needs_human".to_string(),
                kind: QuestionKind::Noul,
                instructions: String::new(),
                criteria: Criteria::Noul {
                    when_true: None,
                    when_false: None,
                },
            },
        ];
        match &parsed.answers["category"] {
            SystemOneAnswer::Choice { probabilities, .. } => {
                assert_eq!(probabilities["billing"], 0.159);
                assert_eq!(probabilities["technical"], 0.84);
                assert_eq!(probabilities["sales"], 0.001);
            }
            other => panic!("expected a choice answer, got {other:?}"),
        }
        match &parsed.answers["urgency"] {
            SystemOneAnswer::Score { probabilities, .. } => {
                assert_eq!(probabilities["1"], 0.8);
            }
            other => panic!("expected a score answer, got {other:?}"),
        }
        let answers = to_answers(&questions, &parsed.answers);
        match &answers["category"].value {
            AnswerValue::Choice(value) => assert_eq!(value, "technical"),
            other => panic!("expected a choice answer, got {other:?}"),
        }
        assert_eq!(answers["category"].confidence, 0.596);
        match &answers["urgency"].value {
            AnswerValue::Score(value) => assert_eq!(*value, 1.0),
            other => panic!("expected a score answer, got {other:?}"),
        }
        assert_eq!(answers["urgency"].confidence, 0.842);
        match &answers["needs_human"].value {
            AnswerValue::Noul(value) => assert_eq!(*value, 0.999),
            other => panic!("expected a noul answer, got {other:?}"),
        }
        assert_eq!(answers["needs_human"].confidence, 0.999);
    }

    /// The byte offset right after the first `needle` in `haystack`, or
    /// `None` when it never appears.
    fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
        haystack
            .windows(needle.len())
            .position(|window| window == needle)
    }

    /// Drains `stream` until the full HTTP request has been read: every
    /// header line, then exactly `Content-Length` body bytes (0 when the
    /// header is absent or unparsable). Without this, a fake server that
    /// writes its response and drops the connection after only a partial
    /// `read()` races the client, which may still be mid-`write` of the
    /// request headers/body -- on some platforms that looks like a genuine
    /// transport error (`Transport("io: Invalid argument ...")`) rather than
    /// the successful response the test means to simulate.
    fn read_full_request(stream: &mut TcpStream) {
        let mut buf = Vec::new();
        let mut chunk = [0u8; 4096];
        let header_end = loop {
            match find_subslice(&buf, b"\r\n\r\n") {
                Some(pos) => break pos + 4,
                None => {
                    let Ok(n) = stream.read(&mut chunk) else {
                        return;
                    };
                    if n == 0 {
                        return; // client closed early; nothing more to read.
                    }
                    buf.extend_from_slice(&chunk[..n]);
                }
            }
        };
        let content_length: usize = String::from_utf8_lossy(&buf[..header_end])
            .lines()
            .find_map(|line| {
                let (name, value) = line.split_once(':')?;
                name.eq_ignore_ascii_case("content-length")
                    .then(|| value.trim().parse().ok())
                    .flatten()
            })
            .unwrap_or(0);
        let mut body_read = buf.len() - header_end;
        while body_read < content_length {
            let Ok(n) = stream.read(&mut chunk) else {
                return;
            };
            if n == 0 {
                return;
            }
            body_read += n;
        }
    }

    /// A one-shot fake HTTP server: reads the full request (see
    /// `read_full_request`), writes a complete, flushed response, and only
    /// THEN lets `stream` drop -- so it never closes the connection out
    /// from under a client still writing or reading. Returns a
    /// `JoinHandle` the caller must `.join()` after its own `decide()` call
    /// returns, so the accepted connection -- and this thread -- is
    /// guaranteed to outlive the whole exchange rather than racing it.
    fn one_shot_server(status: u16, body: &'static str) -> (String, std::thread::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let handle = std::thread::spawn(move || {
            let Ok((mut stream, _)) = listener.accept() else {
                return;
            };
            read_full_request(&mut stream);
            let reason = if status == 200 { "OK" } else { "Error" };
            let response = format!(
                "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: \
                 {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = stream.write_all(response.as_bytes());
            let _ = stream.flush();
        });
        (format!("http://{address}"), handle)
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

    #[test]
    fn a_200_response_converts_to_answers() {
        let text = std::fs::read_to_string(fixture("proxy/jev-response.json")).expect("fixture");
        let body: &'static str = Box::leak(text.into_boxed_str());
        let (url, handle) = one_shot_server(200, body);
        with_credential("TYPESAFE_TEST_KEY_OK", "secret", || {
            let cfg = config(url, "TYPESAFE_TEST_KEY_OK", 5);
            let questions = vec![Question {
                id: "category".to_string(),
                kind: QuestionKind::Choice,
                instructions: String::new(),
                criteria: Criteria::Choice(Vec::new()),
            }];
            let (answers, usage) = decide(&cfg, &sample_intake(), &questions).expect("decide");
            assert_eq!(usage.input_tokens, 312);
            assert!(answers.contains_key("category"));
        });
        handle.join().expect("server thread must not panic");
    }

    #[test]
    fn error_statuses_map_to_the_matching_variant() {
        for (status, expect_variant) in [
            (401, "auth"),
            (422, "invalid"),
            (429, "rate-limited"),
            (529, "overloaded"),
            (500, "status"),
        ] {
            let (url, handle) = one_shot_server(status, "{}");
            let env_name = format!("TYPESAFE_TEST_KEY_{status}");
            with_credential(&env_name, "secret", || {
                let cfg = config(url, &env_name, 5);
                let error = decide(&cfg, &sample_intake(), &[]).expect_err("must fail");
                let matched = matches!(
                    (&error, expect_variant),
                    (TypesafeError::Auth, "auth")
                        | (TypesafeError::Invalid, "invalid")
                        | (TypesafeError::RateLimited, "rate-limited")
                        | (TypesafeError::Overloaded, "overloaded")
                        | (TypesafeError::Status(500), "status")
                );
                assert!(matched, "status {status} gave {error:?}");
            });
            handle.join().expect("server thread must not panic");
        }
    }

    #[test]
    fn a_server_that_never_responds_times_out_within_the_configured_budget() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            // Accept and hold the connection open, never writing a response.
            // The accepted stream must stay bound (not `_`) -- dropping it
            // immediately closes the connection, which looks like a reset
            // to the client rather than a stalled server.
            if let Ok((_stream, _)) = listener.accept() {
                std::thread::sleep(Duration::from_secs(10));
            }
        });
        with_credential("TYPESAFE_TEST_KEY_TIMEOUT", "secret", || {
            let cfg = config(format!("http://{address}"), "TYPESAFE_TEST_KEY_TIMEOUT", 1);
            let started = std::time::Instant::now();
            let error = decide(&cfg, &sample_intake(), &[]).expect_err("must time out");
            assert!(matches!(error, TypesafeError::Timeout), "{error:?}");
            assert!(
                started.elapsed() < Duration::from_secs(5),
                "took {:?}",
                started.elapsed()
            );
        });
    }

    #[test]
    fn an_unset_credential_env_refuses_without_opening_a_connection() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let accepted = listener.accept();
            let _ = tx.send(accepted.is_ok());
        });
        // Deliberately never set: a unique name this process never exports.
        let cfg = config(
            format!("http://{address}"),
            "TYPESAFE_TEST_KEY_NEVER_SET_537",
            1,
        );
        let error = decide(&cfg, &sample_intake(), &[]).expect_err("must refuse");
        assert!(matches!(error, TypesafeError::NoCredential(_)), "{error:?}");
        assert!(
            rx.recv_timeout(Duration::from_millis(300)).is_err(),
            "no connection should have been attempted"
        );
    }
}
