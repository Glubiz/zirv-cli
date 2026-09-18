//! Shared TypeSafe Jev client (issue #537 seam extraction): one HTTP call
//! contract (`POST {base_url}/systemone`, bearer-authenticated, one bounded
//! request/response, no retries, no streaming), used today by the harness
//! proxy (`proxy::typesafe`, now a thin wrapper over [`ask`]) and available
//! to any future advisory site gated by its own `[jev]` key (see
//! `config::JevConfig`). [`ask`] also caches: an identical request body
//! (hashed with SHA-256) gives an identical answer by construction, up to
//! `[jev] cache_ttl_secs` old, so the same question asked twice for the same
//! state never depends on Jev's own answer-to-answer variance at all -- see
//! [`ask`]'s own doc comment for the cache contract.
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
use std::io::Write;
use std::path::Path;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::commands::ctx::adapters;
use crate::commands::ctx::agent;
use crate::commands::ctx::config::{CtxConfig, ProxyTypesafeConfig};
use crate::commands::ctx::log;
use crate::commands::ctx::state::{self, StateDir};

/// Every choice-kind question is capped here, plus a reserved catch-all slot
/// (a caller-provided `("none"|"other", ...)` entry) -- the Jev API's own
/// documented limit.
pub const MAX_CHOICE_OPTIONS: usize = 255;

/// The default margin floor [`Answer::decisive`] checks alongside a caller's
/// own confidence floor. From the completed 2026-09-18 measurement (497 live
/// calls across the intake battery): flipped `intent`/`workflow`/
/// `architecture` answers had a margin of at most 0.14, while their own
/// stable answers sat at 0.17 or higher -- `0.2` clears every flip with a
/// little room to spare. `complexity` is the exception this floor does NOT
/// fix: one stable (non-flipping) but factually wrong answer
/// (`perf-investigation`, see `proxy::jev_live_battery_matches_recorded_
/// rulings`'s own doc comment) sits at margin 0.18-0.24, and one genuinely
/// ambiguous prompt flipped at margin 0.54. So this floor is a DETERMINISM
/// tool -- the same request body keeps giving the same answer, backed
/// further by [`ask`]'s own cache -- never an ACCURACY tool: a decisive
/// answer can still be wrong, and a truly ambiguous prompt can still flip at
/// any margin.
pub(crate) const DEFAULT_MIN_MARGIN: f32 = 0.2;
// A compile-time check (clippy flags a runtime `assert!` on a `const` as
// always-true) so the build itself fails if this constant ever drops below
// the highest flip margin the 2026-09-18 measurement found for intent/
// workflow/architecture (0.14) -- complexity's own flip/stable bands
// overlap and are not a clean bound, see this constant's own doc comment.
const _: () = assert!(
    DEFAULT_MIN_MARGIN > 0.14,
    "must clear every general-field flip margin (<= 0.14)"
);

/// The three question shapes the Jev API speaks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QuestionKind {
    Choice,
    Score,
    Noul,
}

/// A question's own criteria, one shape per [`QuestionKind`]. `Choice`
/// pairs an option with an optional one-line description (`None` is a valid
/// Jev value); `Score` is an ordered list of level descriptions (index 0
/// first); `Noul` optionally names what `true`/`false` mean.
#[derive(Debug, Clone)]
pub enum Criteria {
    Choice(Vec<(String, Option<String>)>),
    Score(Vec<String>),
    Noul {
        when_true: Option<String>,
        when_false: Option<String>,
    },
}

/// One neutral question a caller asks Jev. `id` is the key both the request
/// and the returned [`Answers`] are keyed by.
#[derive(Debug, Clone)]
pub struct Question {
    pub id: String,
    pub kind: QuestionKind,
    pub instructions: String,
    pub criteria: Criteria,
}

impl Question {
    /// A choice question from `(option, description)` pairs. For a
    /// caller-composed catch-all or an optional description, build
    /// [`Criteria::Choice`] directly instead -- this constructor is for the
    /// common case where every option has one.
    #[allow(dead_code)]
    pub(crate) fn choice(id: &str, instructions: &str, options: &[(&str, &str)]) -> Self {
        Question {
            id: id.to_string(),
            kind: QuestionKind::Choice,
            instructions: instructions.to_string(),
            criteria: Criteria::Choice(
                options
                    .iter()
                    .map(|(option, description)| {
                        (option.to_string(), Some(description.to_string()))
                    })
                    .collect(),
            ),
        }
    }

    /// A score question from an ordered list of level descriptions (index 0
    /// first).
    #[allow(dead_code)]
    pub(crate) fn score(id: &str, instructions: &str, levels: &[&str]) -> Self {
        Question {
            id: id.to_string(),
            kind: QuestionKind::Score,
            instructions: instructions.to_string(),
            criteria: Criteria::Score(levels.iter().map(|level| level.to_string()).collect()),
        }
    }

    /// A yes/no question naming what `true`/`false` each mean.
    pub(crate) fn noul(id: &str, instructions: &str, when_true: &str, when_false: &str) -> Self {
        Question {
            id: id.to_string(),
            kind: QuestionKind::Noul,
            instructions: instructions.to_string(),
            criteria: Criteria::Noul {
                when_true: Some(when_true.to_string()),
                when_false: Some(when_false.to_string()),
            },
        }
    }
}

/// One decider's answer to one question, already reduced to a single value
/// plus a confidence in `[0, 1]`. `Score`'s value is a continuous level
/// index (not necessarily an integer -- see [`to_answer`]'s own rounding
/// rule); `Noul`'s value is the raw `true`-probability. `Deserialize` is for
/// [`ask`]'s own decision cache, which round-trips a whole [`Answers`]
/// through JSON on disk.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum AnswerValue {
    Choice(String),
    Score(f64),
    Noul(f64),
}

/// A full answer: `value`/`confidence` keep the exact semantics `proxy::
/// decision::merge` already relies on (`Score` uses the reported
/// confidence; `Noul`'s raw value doubles as its own confidence, since the
/// wire format carries no separate `confidence` field for it), plus the raw
/// probability distribution -- empty for `Noul`, which has none on the wire.
/// `Deserialize` is for [`ask`]'s own decision cache (see [`AnswerValue`]'s
/// own doc comment).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Answer {
    pub value: AnswerValue,
    pub confidence: f32,
    pub probabilities: BTreeMap<String, f32>,
}

impl Answer {
    /// `Some(choice)` for a `Choice` answer, `None` for any other kind.
    #[allow(dead_code)]
    pub(crate) fn as_choice(&self) -> Option<&str> {
        match &self.value {
            AnswerValue::Choice(value) => Some(value.as_str()),
            AnswerValue::Score(_) | AnswerValue::Noul(_) => None,
        }
    }

    /// `Some(index)` for a `Score` answer, `None` for any other kind.
    #[allow(dead_code)]
    pub(crate) fn as_score(&self) -> Option<f64> {
        match self.value {
            AnswerValue::Score(value) => Some(value),
            AnswerValue::Choice(_) | AnswerValue::Noul(_) => None,
        }
    }

    /// `Some(probability)` for a `Noul` answer, `None` for any other kind.
    #[allow(dead_code)]
    pub(crate) fn as_noul(&self) -> Option<f64> {
        match self.value {
            AnswerValue::Noul(value) => Some(value),
            AnswerValue::Choice(_) | AnswerValue::Score(_) => None,
        }
    }

    /// The gap between this answer's most likely value and its runner-up, in
    /// `[0, 1]` -- large when the model was decisively between one option and
    /// the rest, near `0` when two (or more) options were nearly tied. A
    /// `Choice`/`Score` answer with fewer than two entries in `probabilities`
    /// (an empty distribution, or a caller-composed answer with none) has no
    /// runner-up to compare against, so its margin is `0.0`. `Noul` carries
    /// no probability distribution on the wire at all; its margin is instead
    /// how far its own raw value sits from the maximally uncertain `0.5`,
    /// doubled into the same `[0, 1]` range every other margin uses (`p =
    /// 0.52` gives `0.04`, `p = 0.95` gives `0.9`).
    pub(crate) fn margin(&self) -> f32 {
        match &self.value {
            AnswerValue::Choice(_) | AnswerValue::Score(_) => {
                let mut probabilities: Vec<f32> = self.probabilities.values().copied().collect();
                if probabilities.len() < 2 {
                    return 0.0;
                }
                probabilities.sort_by(|a, b| b.partial_cmp(a).unwrap_or(std::cmp::Ordering::Equal));
                probabilities[0] - probabilities[1]
            }
            AnswerValue::Noul(value) => (*value as f32 - 0.5).abs() * 2.0,
        }
    }

    /// Whether this answer clears BOTH floors a caller wants before acting on
    /// it, rather than falling back to its own deterministic path: the
    /// reported `confidence` at or above `min_confidence`, AND `margin` at or
    /// above `min_margin` (see [`DEFAULT_MIN_MARGIN`]'s own doc comment for
    /// why confidence alone is not enough). `Noul` carries no independently
    /// reported confidence on the wire at all -- its own `confidence` field
    /// IS the raw value (see this struct's own doc comment) -- so
    /// `min_confidence` is ignored for it, and only `margin` governs.
    pub(crate) fn decisive(&self, min_confidence: f32, min_margin: f32) -> bool {
        let confidence_ok = match self.value {
            AnswerValue::Noul(_) => true,
            AnswerValue::Choice(_) | AnswerValue::Score(_) => self.confidence >= min_confidence,
        };
        confidence_ok && self.margin() >= min_margin
    }
}

/// A full set of answers, keyed by [`Question::id`].
pub type Answers = BTreeMap<String, Answer>;

/// Raw token counts from a Jev response. Jev bills input only, so there is
/// no output price and no cache class to carry.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Usage {
    pub input_tokens: u64,
    pub output_tokens: u64,
}

#[derive(Debug)]
pub enum JevError {
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

impl std::fmt::Display for JevError {
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

impl std::error::Error for JevError {}

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
struct SystemOneRequest {
    state: serde_json::Value,
    model: String,
    questions: BTreeMap<String, QuestionSpec>,
}

#[derive(Debug, Deserialize)]
struct SystemOneUsage {
    input_tokens: u64,
    output_tokens: u64,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
enum SystemOneAnswer {
    Choice {
        choice: String,
        #[serde(default)]
        probabilities: BTreeMap<String, f64>,
        confidence: f32,
    },
    Score {
        score: f64,
        #[serde(default)]
        legend: BTreeMap<String, String>,
        #[serde(default)]
        probabilities: BTreeMap<String, f64>,
        confidence: f32,
    },
    Noul {
        noul: f64,
    },
}

#[derive(Debug, Deserialize)]
struct SystemOneResponse {
    // Kept for parity with the documented response shape and asserted on in
    // this module's own tests; `ask` reads `answers`/`usage` only.
    #[allow(dead_code)]
    model: String,
    answers: BTreeMap<String, SystemOneAnswer>,
    usage: SystemOneUsage,
}

fn build_request(state: &impl Serialize, questions: &[Question], model: &str) -> SystemOneRequest {
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
        state: serde_json::to_value(state).unwrap_or(serde_json::Value::Null),
        model: model.to_string(),
        questions: mapped,
    }
}

/// Converts one documented answer into the neutral [`Answer`] shape: for
/// `score`, the level index is `round(score)` clamped into the legend's own
/// range and `confidence` is the answer's own `confidence`; for `noul`,
/// there is no `confidence` field in the wire format at all, so the raw
/// value doubles as its own confidence (a reading near 0 or 1 is a
/// confident answer either way; near 0.5 is not). `probabilities` carries
/// the raw distribution through unchanged (empty for `noul`, which has
/// none).
fn to_answer(raw: &SystemOneAnswer) -> Answer {
    let as_f32_map = |probabilities: &BTreeMap<String, f64>| -> BTreeMap<String, f32> {
        probabilities
            .iter()
            .map(|(key, value)| (key.clone(), *value as f32))
            .collect()
    };
    match raw {
        SystemOneAnswer::Choice {
            choice,
            probabilities,
            confidence,
        } => Answer {
            value: AnswerValue::Choice(choice.clone()),
            confidence: *confidence,
            probabilities: as_f32_map(probabilities),
        },
        SystemOneAnswer::Score {
            score,
            legend,
            probabilities,
            confidence,
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
                probabilities: as_f32_map(probabilities),
            }
        }
        SystemOneAnswer::Noul { noul } => Answer {
            value: AnswerValue::Noul(*noul),
            confidence: *noul as f32,
            probabilities: BTreeMap::new(),
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

fn status_error(status: u16) -> JevError {
    match status {
        401 => JevError::Auth,
        422 => JevError::Invalid,
        429 => JevError::RateLimited,
        529 => JevError::Overloaded,
        other => JevError::Status(other),
    }
}

/// The decision cache's own subdirectory under a state dir:
/// `<state_dir>/jev-cache/<sha256-of-the-request-body>.json`. See [`ask`]'s
/// own doc comment for the cache contract.
const JEV_CACHE_DIR: &str = "jev-cache";

/// One cached call, exactly what a cache HIT restores and a cache MISS (on a
/// 200) writes.
#[derive(Debug, Serialize, Deserialize)]
struct CacheEntry {
    answers: Answers,
    usage: Usage,
    stored_at: u64,
    model: String,
}

fn hash_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// `Some(entry)` for a cache file that parses and is younger than
/// `ttl_secs`; `None` for anything else -- missing, corrupt/unparseable
/// (treated as a miss, never a hard error), or expired. Never deletes an
/// expired file: the next successful call overwrites it via the same
/// atomic `state::write_private` every other cache write uses.
fn read_cache_entry(path: &Path, ttl_secs: u64) -> Option<CacheEntry> {
    let text = std::fs::read_to_string(path).ok()?;
    let entry: CacheEntry = serde_json::from_str(&text).ok()?;
    (state::now_secs().saturating_sub(entry.stored_at) < ttl_secs).then_some(entry)
}

/// Best-effort, like every other cache/log write in this crate: a failure to
/// create the directory or write the file never fails the caller's own
/// (already-computed) answer.
fn write_cache_entry(
    state_dir: &Path,
    cache_key: &str,
    answers: &Answers,
    usage: &Usage,
    model: &str,
) {
    let dir = state_dir.join(JEV_CACHE_DIR);
    if state::create_private_dir_all(&dir).is_err() {
        return;
    }
    let entry = CacheEntry {
        answers: answers.clone(),
        usage: *usage,
        stored_at: state::now_secs(),
        model: model.to_string(),
    };
    if let Ok(text) = serde_json::to_string(&entry) {
        let _ = state::write_private(&dir.join(format!("{cache_key}.json")), &text);
    }
}

/// Runs one bounded `/systemone` call and converts its answers into the
/// neutral [`Answers`] shape, or serves an identical prior call from the
/// on-disk decision cache. `credential_env` is read fresh every call and
/// never logged; an unset or empty value refuses before any connection is
/// opened, matching every other credential-by-env-name seam in this crate
/// (`EndpointTarget`, `AgentAdapter::ready`). `state` is any bounded,
/// repository-neutral value a caller wants Jev's opinion on -- the harness
/// proxy passes its own `IntakeState`; a future site passes its own shape.
///
/// Cache: the request body (`{state, model, questions}`, the exact bytes
/// this function would otherwise send) is hashed with SHA-256 and looked up
/// at `<state_dir>/jev-cache/<hash>.json` BEFORE the credential check or any
/// network attempt -- an identical request gives an identical answer by
/// construction, with zero `Usage` and no credential needed at all, up to
/// `cache_ttl_secs` old. A miss (including an expired entry, or a corrupt/
/// unreadable file) falls through to the real call exactly as before; only
/// a genuine `200` response is stored, never an error. `cache_ttl_secs ==
/// 0` disables the cache entirely: no lookup, no write, every call reaches
/// the network. The returned `bool` is whether this answer was served from
/// the cache -- callers that record a decision line (`record`, below) pass
/// it through as `cached`.
pub(crate) fn ask(
    cfg: &ProxyTypesafeConfig,
    state_dir: &Path,
    cache_ttl_secs: u64,
    state: &impl Serialize,
    questions: &[Question],
) -> Result<(Answers, Usage, bool), JevError> {
    let request = build_request(state, questions, &cfg.model);
    let payload = serde_json::to_string(&request)
        .map_err(|error| JevError::Transport(format!("failed to encode request: {error}")))?;
    let cache_key = hash_hex(payload.as_bytes());
    let cache_path = state_dir
        .join(JEV_CACHE_DIR)
        .join(format!("{cache_key}.json"));

    if cache_ttl_secs > 0
        && let Some(entry) = read_cache_entry(&cache_path, cache_ttl_secs)
    {
        return Ok((
            entry.answers,
            Usage {
                input_tokens: 0,
                output_tokens: 0,
            },
            true,
        ));
    }

    let credential = match std::env::var(&cfg.credential_env) {
        Ok(value) if !value.is_empty() => value,
        _ => return Err(JevError::NoCredential(cfg.credential_env.clone())),
    };

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
        Err(ureq::Error::Timeout(_)) => return Err(JevError::Timeout),
        Err(error) => return Err(JevError::Transport(error.to_string())),
    };

    let status = response.status().as_u16();
    if status != 200 {
        return Err(status_error(status));
    }

    let body = response
        .body_mut()
        .read_to_string()
        .map_err(|error| JevError::Transport(error.to_string()))?;
    let parsed: SystemOneResponse =
        serde_json::from_str(&body).map_err(|error| JevError::Malformed(error.to_string()))?;

    let answers = to_answers(questions, &parsed.answers);
    let usage = Usage {
        input_tokens: parsed.usage.input_tokens,
        output_tokens: parsed.usage.output_tokens,
    };
    if cache_ttl_secs > 0 {
        write_cache_entry(state_dir, &cache_key, &answers, &usage, &cfg.model);
    }
    Ok((answers, usage, false))
}

/// Whether `cfg`'s credential env is set and non-empty -- the cheap half of
/// deciding whether a `[jev]`-gated site should bother calling [`ask`] at
/// all (the other half is that site's own `[jev]` key; see
/// `config::JevConfig`'s own doc comment). Also what `proxy::activation`
/// checks for the harness proxy's own `typesafe` decider, so the two never
/// drift on what "usable" means.
pub(crate) fn available(cfg: &ProxyTypesafeConfig) -> bool {
    std::env::var(&cfg.credential_env)
        .map(|value| !value.is_empty())
        .unwrap_or(false)
}

#[allow(dead_code)]
const JEV_DECISIONS_FILE: &str = "jev-decisions.jsonl";
/// The catalogue id `log::Delegation`/`price::price` prices the spend row
/// on -- see `catalogue.rs`'s `typesafe` vendor. Every `[jev]`-gated site
/// spends through the same vendor as the harness proxy, regardless of which
/// site asked.
#[allow(dead_code)]
const JEV_SPEND_AGENT: &str = "typesafe";

/// This process's own `(session, principal)` -- `ZIRV_CTX_SESSION`/
/// `ZIRV_PRINCIPAL`, falling back to `"proxy"`/`"root"` only when unset, the
/// same "root session, no inherited envelope" convention `agent::
/// root_envelope` establishes. Shared by [`record`] and `proxy::persist` so
/// the two spend-adjacent recorders never drift on what an absent value
/// means.
pub(crate) fn session_and_principal() -> (String, String) {
    let session = std::env::var(adapters::SESSION_ENV)
        .ok()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| "proxy".to_string());
    let principal = std::env::var(agent::PRINCIPAL_ENV)
        .ok()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| "root".to_string());
    (session, principal)
}

/// One answer plus its own [`Answer::margin`] -- `#[serde(flatten)]` so the
/// JSONL shape stays exactly what it was before margin recording (`value`/
/// `confidence`/`probabilities` at the top level of each answer object),
/// with `margin` simply joining them, rather than nesting the original
/// answer under its own key.
#[derive(Debug, Serialize)]
struct AnswerRecord<'a> {
    #[serde(flatten)]
    answer: &'a Answer,
    margin: f32,
}

#[derive(Debug, Serialize)]
struct DecisionRecord<'a> {
    site: &'a str,
    ts: u64,
    answers: BTreeMap<&'a str, AnswerRecord<'a>>,
    usage: &'a Usage,
    wall_ms: u64,
    fallbacks: &'a [String],
    /// Whether these answers were served from [`ask`]'s own decision cache
    /// rather than a real call -- `usage` is `0`/`0` whenever this is `true`.
    cached: bool,
}

/// Appends one JSON line -- `site`, a timestamp, every answer's value/
/// confidence/probabilities/margin, `usage`, `wall_ms`, `fallbacks` and
/// `cached` -- to `<state_dir>/jev-decisions.jsonl`, and records a
/// `log::Delegation` spend row (agent `"typesafe"`, model from `cfg.proxy.
/// typesafe.model`, `usage`'s input/output tokens, `wall_ms`) so `zirv ctx
/// spend` prices the call through the same catalogue vendor the harness
/// proxy already does -- a cache hit's own `0`/`0` `usage` naturally prices
/// as free. The harness proxy keeps recording its own `proxy-decisions.jsonl`
/// and spend row via `proxy::persist` -- this is for every OTHER
/// `[jev]`-gated site, never a second record for the proxy's own call.
/// Appends via `state::open_private_append`, the same `O_APPEND`-backed
/// write `proxy::persist` itself uses for its own decisions file -- a prior
/// version read the whole file, appended in memory, and rewrote it with
/// `state::write_private`, which lost a line whenever two zirv processes
/// recorded at the same time (review finding). Best-effort like every other
/// append in this crate's flat logs: a write failure here must never break
/// the caller's own (already-computed) decision.
#[allow(clippy::too_many_arguments)]
pub(crate) fn record(
    state: &StateDir,
    cfg: &CtxConfig,
    site: &str,
    answers: &Answers,
    usage: &Usage,
    wall_ms: u64,
    fallbacks: &[String],
    cached: bool,
) {
    let ts = state::now_secs();
    let answer_records: BTreeMap<&str, AnswerRecord> = answers
        .iter()
        .map(|(id, answer)| {
            (
                id.as_str(),
                AnswerRecord {
                    answer,
                    margin: answer.margin(),
                },
            )
        })
        .collect();
    let record = DecisionRecord {
        site,
        ts,
        answers: answer_records,
        usage,
        wall_ms,
        fallbacks,
        cached,
    };
    if let Ok(line) = serde_json::to_string(&record)
        && state::create_private_dir_all(state.root()).is_ok()
        && let Ok(mut file) = state::open_private_append(&state.root().join(JEV_DECISIONS_FILE))
    {
        let _ = writeln!(file, "{line}");
    }

    let (session, principal) = session_and_principal();
    let _ = log::append_delegation(
        state,
        &log::Delegation {
            ts,
            session: &session,
            parent_session: "",
            work_group_id: None,
            agent: JEV_SPEND_AGENT,
            model: Some(cfg.proxy.typesafe.model.as_str()),
            input_tokens: usage.input_tokens,
            cache_creation_input_tokens: 0,
            cache_read_input_tokens: 0,
            output_tokens: usage.output_tokens,
            wall_ms,
            exit_code: 0,
            outcome: "ok",
            mode: None,
            task_class: None,
            principal: &principal,
            envelope_sha256: None,
        },
    );
}

/// The one advisory entry point a `[jev]`-gated site calls: short-circuits
/// to `None`, with no network call and no log line at all, when `enabled`
/// is false or the `[proxy.typesafe]` credential is not set ([`available`])
/// -- either way the caller's own pre-existing deterministic path runs
/// byte-identical to today. Otherwise runs one bounded [`ask`] call and
/// [`record`]s the outcome either way: a successful call's own answers, or
/// an empty answer set carrying the error's `Display` as the one fallback
/// reason. Never panics, never propagates -- same posture as every other
/// best-effort seam in this crate.
#[allow(dead_code)]
pub(crate) fn advise(
    cfg: &CtxConfig,
    state_dir: &StateDir,
    site: &str,
    enabled: bool,
    state: &impl Serialize,
    questions: &[Question],
) -> Option<Answers> {
    if !enabled || !available(&cfg.proxy.typesafe) {
        return None;
    }
    let started = std::time::Instant::now();
    let wall_ms = |started: std::time::Instant| -> u64 {
        started.elapsed().as_millis().min(u128::from(u64::MAX)) as u64
    };
    match ask(
        &cfg.proxy.typesafe,
        state_dir.root(),
        cfg.jev.cache_ttl_secs,
        state,
        questions,
    ) {
        Ok((answers, usage, cached)) => {
            record(
                state_dir,
                cfg,
                site,
                &answers,
                &usage,
                wall_ms(started),
                &[],
                cached,
            );
            Some(answers)
        }
        Err(error) => {
            record(
                state_dir,
                cfg,
                site,
                &Answers::new(),
                &Usage {
                    input_tokens: 0,
                    output_tokens: 0,
                },
                wall_ms(started),
                &[error.to_string()],
                false,
            );
            None
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
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

    #[derive(Serialize)]
    struct SampleState {
        request: String,
    }

    fn sample_state() -> SampleState {
        SampleState {
            request: "fix the typo".to_string(),
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
        let request = build_request(&sample_state(), &sample_questions(), "jev-latest");
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

    /// New behaviour (issue #537 seam extraction): the neutral `Answer` now
    /// carries the raw probability distribution through for `choice`/
    /// `score`, and leaves it empty for `noul`, which has none on the wire.
    #[test]
    fn ask_fills_probabilities_for_choice_and_score_and_leaves_noul_empty() {
        let text = std::fs::read_to_string(fixture("proxy/jev-response.json")).expect("fixture");
        let body: &'static str = Box::leak(text.into_boxed_str());
        let (url, handle) = one_shot_server(200, body);
        let state_dir = tempfile::tempdir().expect("tempdir");
        with_credential("JEV_TEST_KEY_PROBABILITIES", "secret", || {
            let cfg = config(url, "JEV_TEST_KEY_PROBABILITIES", 5);
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
            let (answers, _usage, cached) =
                ask(&cfg, state_dir.path(), 0, &sample_state(), &questions).expect("ask");
            assert!(!cached);
            assert_eq!(answers["category"].probabilities["technical"], 0.84);
            assert_eq!(answers["urgency"].probabilities["1"], 0.8);
            assert!(
                answers["needs_human"].probabilities.is_empty(),
                "{:?}",
                answers["needs_human"].probabilities
            );
        });
        handle.join().expect("server thread must not panic");
    }

    #[test]
    fn available_is_false_when_the_credential_env_is_unset_or_empty() {
        let cfg = config(
            "http://127.0.0.1:0".to_string(),
            "JEV_TEST_KEY_AVAILABLE_537",
            5,
        );
        // SAFETY (test-only): a unique env var name this test owns.
        unsafe {
            std::env::remove_var(&cfg.credential_env);
        }
        assert!(!available(&cfg));

        unsafe {
            std::env::set_var(&cfg.credential_env, "");
        }
        assert!(!available(&cfg));

        unsafe {
            std::env::set_var(&cfg.credential_env, "secret");
        }
        assert!(available(&cfg));

        unsafe {
            std::env::remove_var(&cfg.credential_env);
        }
    }

    #[test]
    fn record_writes_a_parseable_line_with_the_site_and_probabilities() {
        let state_dir = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_path(state_dir.path().to_path_buf());
        let cfg = CtxConfig::default();

        let mut answers = Answers::new();
        answers.insert(
            "intent".to_string(),
            Answer {
                value: AnswerValue::Choice("feature".to_string()),
                confidence: 0.9,
                probabilities: BTreeMap::from([
                    ("feature".to_string(), 0.9_f32),
                    ("bugfix".to_string(), 0.1_f32),
                ]),
            },
        );
        let usage = Usage {
            input_tokens: 10,
            output_tokens: 2,
        };

        record(&state, &cfg, "memory", &answers, &usage, 42, &[], false);

        let text = std::fs::read_to_string(state_dir.path().join(JEV_DECISIONS_FILE))
            .expect("jev-decisions.jsonl");
        let line = text.lines().next().expect("one line");
        let value: serde_json::Value = serde_json::from_str(line).expect("parse json");
        assert_eq!(value["site"], "memory");
        assert_eq!(value["answers"]["intent"]["probabilities"]["feature"], 0.9);
        assert!(
            (value["answers"]["intent"]["margin"].as_f64().unwrap() - 0.8).abs() < 1e-6,
            "{value}"
        );
        assert_eq!(value["usage"]["input_tokens"], 10);
        assert_eq!(value["wall_ms"], 42);
        assert_eq!(value["cached"], false);

        let delegations = log::read_delegations(&state, 10);
        assert_eq!(delegations.len(), 1);
        assert_eq!(delegations[0].agent, JEV_SPEND_AGENT);
        assert_eq!(
            delegations[0].model.as_deref(),
            Some(cfg.proxy.typesafe.model.as_str())
        );
        assert_eq!(delegations[0].input_tokens, 10);
    }

    /// Review finding: `record` used to read the whole file, append in
    /// memory, and rewrite it with `state::write_private` -- a lost-update
    /// race between two concurrent zirv processes recording at once. Now an
    /// `O_APPEND` write (`state::open_private_append`), the same primitive
    /// `proxy::persist` already uses for its own decisions file: two
    /// sequential calls must leave exactly two lines, in call order, with
    /// the first line byte-identical to what it was before the second call
    /// ever ran (never truncated, never rewritten).
    #[test]
    fn record_called_twice_appends_two_lines_and_never_truncates() {
        let state_dir = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_path(state_dir.path().to_path_buf());
        let cfg = CtxConfig::default();
        let usage = Usage {
            input_tokens: 1,
            output_tokens: 0,
        };

        record(
            &state,
            &cfg,
            "memory",
            &Answers::new(),
            &usage,
            5,
            &[],
            false,
        );
        let path = state_dir.path().join(JEV_DECISIONS_FILE);
        let after_first = std::fs::read_to_string(&path).expect("jev-decisions.jsonl");
        let first_line = after_first.lines().next().expect("one line").to_string();

        record(
            &state,
            &cfg,
            "supervisor",
            &Answers::new(),
            &usage,
            7,
            &[],
            true,
        );
        let after_second = std::fs::read_to_string(&path).expect("jev-decisions.jsonl");
        let lines: Vec<&str> = after_second.lines().collect();

        assert_eq!(
            lines.len(),
            2,
            "two record() calls must leave two lines, never truncate: {after_second:?}"
        );
        assert_eq!(
            lines[0], first_line,
            "the first call's own line must survive byte-identical"
        );
        let first: serde_json::Value = serde_json::from_str(lines[0]).expect("parse first line");
        let second: serde_json::Value = serde_json::from_str(lines[1]).expect("parse second line");
        assert_eq!(first["site"], "memory");
        assert_eq!(first["cached"], false);
        assert_eq!(second["site"], "supervisor");
        assert_eq!(second["cached"], true);
    }

    #[test]
    fn advise_with_the_gate_off_makes_no_call_and_returns_none() {
        let state_dir = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_path(state_dir.path().to_path_buf());
        let mut cfg = CtxConfig::default();
        cfg.proxy.typesafe.credential_env = "JEV_TEST_KEY_GATE_OFF_537".to_string();
        // The credential looks available, so a bug that ignored `enabled`
        // would still attempt a call rather than short-circuiting on it.
        with_credential(&cfg.proxy.typesafe.credential_env.clone(), "secret", || {
            let result = advise(
                &cfg,
                &state,
                "memory",
                false,
                &sample_state(),
                &sample_questions(),
            );
            assert!(result.is_none());
        });
        assert!(
            !state_dir.path().join(JEV_DECISIONS_FILE).exists(),
            "advise must not write a record when the gate is off"
        );
    }

    #[test]
    fn advise_on_a_200_response_returns_some_and_records_wall_ms() {
        let text = std::fs::read_to_string(fixture("proxy/jev-response.json")).expect("fixture");
        let body: &'static str = Box::leak(text.into_boxed_str());
        let (url, handle) = one_shot_server(200, body);
        let state_dir = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_path(state_dir.path().to_path_buf());
        with_credential("JEV_TEST_KEY_ADVISE_OK", "secret", || {
            let mut cfg = CtxConfig::default();
            cfg.proxy.typesafe = config(url, "JEV_TEST_KEY_ADVISE_OK", 5);
            let questions = vec![Question::choice(
                "category",
                "pick one",
                &[("technical", "a technical question")],
            )];
            let result = advise(&cfg, &state, "memory", true, &sample_state(), &questions);
            assert!(result.is_some());
        });
        handle.join().expect("server thread must not panic");

        let text = std::fs::read_to_string(state_dir.path().join(JEV_DECISIONS_FILE))
            .expect("jev-decisions.jsonl");
        let line = text.lines().next().expect("one line");
        let value: serde_json::Value = serde_json::from_str(line).expect("parse json");
        assert_eq!(value["site"], "memory");
        assert!(
            value["wall_ms"].as_u64().is_some(),
            "wall_ms must be a recorded number: {value}"
        );
        assert!(
            value["fallbacks"]
                .as_array()
                .expect("fallbacks array")
                .is_empty()
        );
    }

    #[test]
    fn advise_on_a_500_response_returns_none_and_records_the_fallback() {
        let (url, handle) = one_shot_server(500, "{}");
        let state_dir = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_path(state_dir.path().to_path_buf());
        with_credential("JEV_TEST_KEY_ADVISE_ERR", "secret", || {
            let mut cfg = CtxConfig::default();
            cfg.proxy.typesafe = config(url, "JEV_TEST_KEY_ADVISE_ERR", 5);
            let result = advise(
                &cfg,
                &state,
                "memory",
                true,
                &sample_state(),
                &sample_questions(),
            );
            assert!(result.is_none());
        });
        handle.join().expect("server thread must not panic");

        let text = std::fs::read_to_string(state_dir.path().join(JEV_DECISIONS_FILE))
            .expect("jev-decisions.jsonl");
        let line = text.lines().next().expect("one line");
        let value: serde_json::Value = serde_json::from_str(line).expect("parse json");
        assert_eq!(value["site"], "memory");
        let fallbacks = value["fallbacks"].as_array().expect("fallbacks array");
        assert_eq!(fallbacks.len(), 1);
        assert!(
            fallbacks[0]
                .as_str()
                .unwrap()
                .contains("unexpected status 500"),
            "{fallbacks:?}"
        );
        assert!(
            value["answers"]
                .as_object()
                .expect("answers object")
                .is_empty()
        );
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
    /// `JoinHandle` the caller must `.join()` after its own [`ask`] call
    /// returns, so the accepted connection -- and this thread -- is
    /// guaranteed to outlive the whole exchange rather than racing it.
    /// `pub(crate)` so any future `[jev]`-gated site's own tests can drive
    /// [`ask`] against a canned response without reimplementing this.
    pub(crate) fn one_shot_server(
        status: u16,
        body: &'static str,
    ) -> (String, std::thread::JoinHandle<()>) {
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
        let state_dir = tempfile::tempdir().expect("tempdir");
        with_credential("JEV_TEST_KEY_OK", "secret", || {
            let cfg = config(url, "JEV_TEST_KEY_OK", 5);
            let questions = vec![Question {
                id: "category".to_string(),
                kind: QuestionKind::Choice,
                instructions: String::new(),
                criteria: Criteria::Choice(Vec::new()),
            }];
            let (answers, usage, cached) =
                ask(&cfg, state_dir.path(), 0, &sample_state(), &questions).expect("ask");
            assert!(!cached);
            assert_eq!(usage.input_tokens, 312);
            assert!(answers.contains_key("category"));
        });
        handle.join().expect("server thread must not panic");
    }

    #[test]
    fn error_statuses_map_to_the_matching_variant() {
        let state_dir = tempfile::tempdir().expect("tempdir");
        for (status, expect_variant) in [
            (401, "auth"),
            (422, "invalid"),
            (429, "rate-limited"),
            (529, "overloaded"),
            (500, "status"),
        ] {
            let (url, handle) = one_shot_server(status, "{}");
            let env_name = format!("JEV_TEST_KEY_{status}");
            with_credential(&env_name, "secret", || {
                let cfg = config(url, &env_name, 5);
                let error =
                    ask(&cfg, state_dir.path(), 0, &sample_state(), &[]).expect_err("must fail");
                let matched = matches!(
                    (&error, expect_variant),
                    (JevError::Auth, "auth")
                        | (JevError::Invalid, "invalid")
                        | (JevError::RateLimited, "rate-limited")
                        | (JevError::Overloaded, "overloaded")
                        | (JevError::Status(500), "status")
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
        let state_dir = tempfile::tempdir().expect("tempdir");
        with_credential("JEV_TEST_KEY_TIMEOUT", "secret", || {
            let cfg = config(format!("http://{address}"), "JEV_TEST_KEY_TIMEOUT", 1);
            let started = std::time::Instant::now();
            let error =
                ask(&cfg, state_dir.path(), 0, &sample_state(), &[]).expect_err("must time out");
            assert!(matches!(error, JevError::Timeout), "{error:?}");
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
        let cfg = config(format!("http://{address}"), "JEV_TEST_KEY_NEVER_SET_537", 1);
        let state_dir = tempfile::tempdir().expect("tempdir");
        let error = ask(&cfg, state_dir.path(), 0, &sample_state(), &[]).expect_err("must refuse");
        assert!(matches!(error, JevError::NoCredential(_)), "{error:?}");
        assert!(
            rx.recv_timeout(Duration::from_millis(300)).is_err(),
            "no connection should have been attempted"
        );
    }

    /// A second call with the identical request body is served from the
    /// cache: the one-shot server only ever accepts ONE connection, so a
    /// second real attempt would fail with a transport error rather than
    /// this test's own `expect("second ask")` succeeding.
    #[test]
    fn a_second_identical_call_is_served_from_the_cache_with_no_network() {
        let text = std::fs::read_to_string(fixture("proxy/jev-response.json")).expect("fixture");
        let body: &'static str = Box::leak(text.into_boxed_str());
        let (url, handle) = one_shot_server(200, body);
        let state_dir = tempfile::tempdir().expect("tempdir");
        with_credential("JEV_TEST_KEY_CACHE_HIT", "secret", || {
            let cfg = config(url, "JEV_TEST_KEY_CACHE_HIT", 5);
            let questions = sample_questions();

            let (first_answers, first_usage, first_cached) =
                ask(&cfg, state_dir.path(), 86_400, &sample_state(), &questions)
                    .expect("first ask");
            assert!(!first_cached, "the first call must hit the network");
            assert!(first_usage.input_tokens > 0);

            let (second_answers, second_usage, second_cached) =
                ask(&cfg, state_dir.path(), 86_400, &sample_state(), &questions)
                    .expect("second ask");
            assert!(
                second_cached,
                "an identical second call must be served from the cache"
            );
            assert_eq!(second_usage.input_tokens, 0);
            assert_eq!(second_usage.output_tokens, 0);
            assert_eq!(second_answers, first_answers);
        });
        handle.join().expect("server thread must not panic");
    }

    /// `cache_ttl_secs == 0` disables the cache entirely: each of two
    /// otherwise-identical calls must reach its OWN one-shot server (a
    /// cache hit would mean the second one is never even attempted).
    #[test]
    fn ttl_zero_disables_the_cache_and_always_calls() {
        let text = std::fs::read_to_string(fixture("proxy/jev-response.json")).expect("fixture");
        let state_dir = tempfile::tempdir().expect("tempdir");
        with_credential("JEV_TEST_KEY_CACHE_TTL_ZERO", "secret", || {
            for _ in 0..2 {
                let body: &'static str = Box::leak(text.clone().into_boxed_str());
                let (url, handle) = one_shot_server(200, body);
                let cfg = config(url, "JEV_TEST_KEY_CACHE_TTL_ZERO", 5);
                let (_, usage, cached) = ask(
                    &cfg,
                    state_dir.path(),
                    0,
                    &sample_state(),
                    &sample_questions(),
                )
                .expect("ask");
                assert!(!cached, "ttl 0 must never serve from the cache");
                assert!(usage.input_tokens > 0);
                handle.join().expect("server thread must not panic");
            }
        });
    }

    /// An entry older than `ttl_secs` is a miss: pre-populating one that is
    /// already expired must still reach the network below, exactly like no
    /// cache file existing at all.
    #[test]
    fn an_expired_cache_entry_calls_again() {
        let text = std::fs::read_to_string(fixture("proxy/jev-response.json")).expect("fixture");
        let body: &'static str = Box::leak(text.into_boxed_str());
        let (url, handle) = one_shot_server(200, body);
        let state_dir = tempfile::tempdir().expect("tempdir");
        with_credential("JEV_TEST_KEY_CACHE_EXPIRED", "secret", || {
            let cfg = config(url, "JEV_TEST_KEY_CACHE_EXPIRED", 5);
            let questions = sample_questions();
            let request = build_request(&sample_state(), &questions, &cfg.model);
            let payload = serde_json::to_string(&request).expect("serialize");
            let cache_key = hash_hex(payload.as_bytes());
            let cache_dir = state_dir.path().join(JEV_CACHE_DIR);
            std::fs::create_dir_all(&cache_dir).expect("mkdir cache dir");
            let stale_entry = serde_json::json!({
                "answers": {},
                "usage": {"input_tokens": 1, "output_tokens": 0},
                "stored_at": 1,
                "model": cfg.model,
            });
            std::fs::write(
                cache_dir.join(format!("{cache_key}.json")),
                stale_entry.to_string(),
            )
            .expect("write stale cache entry");

            let (_, usage, cached) =
                ask(&cfg, state_dir.path(), 5, &sample_state(), &questions).expect("ask");
            assert!(!cached, "an expired entry must not be served");
            assert!(
                usage.input_tokens > 0,
                "an expired entry must reach the network"
            );
        });
        handle.join().expect("server thread must not panic");
    }

    /// An error response is never cached: the directory it would have lived
    /// in must not even exist afterward.
    #[test]
    fn a_500_response_leaves_no_cache_file() {
        let (url, handle) = one_shot_server(500, "{}");
        let state_dir = tempfile::tempdir().expect("tempdir");
        with_credential("JEV_TEST_KEY_CACHE_500", "secret", || {
            let cfg = config(url, "JEV_TEST_KEY_CACHE_500", 5);
            let error = ask(
                &cfg,
                state_dir.path(),
                86_400,
                &sample_state(),
                &sample_questions(),
            )
            .expect_err("must fail");
            assert!(matches!(error, JevError::Status(500)), "{error:?}");
        });
        handle.join().expect("server thread must not panic");
        assert!(
            !state_dir.path().join(JEV_CACHE_DIR).exists(),
            "an error response must never create a cache file"
        );
    }

    #[test]
    fn margin_is_the_gap_between_top_and_runner_up_for_choice_and_score() {
        let choice = Answer {
            value: AnswerValue::Choice("technical".to_string()),
            confidence: 0.6,
            probabilities: BTreeMap::from([
                ("technical".to_string(), 0.6_f32),
                ("billing".to_string(), 0.3_f32),
                ("sales".to_string(), 0.1_f32),
            ]),
        };
        assert!((choice.margin() - 0.3).abs() < 1e-6, "{}", choice.margin());

        let score = Answer {
            value: AnswerValue::Score(1.0),
            confidence: 0.7,
            probabilities: BTreeMap::from([
                ("0".to_string(), 0.1_f32),
                ("1".to_string(), 0.7_f32),
                ("2".to_string(), 0.2_f32),
            ]),
        };
        assert!((score.margin() - 0.5).abs() < 1e-6, "{}", score.margin());
    }

    #[test]
    fn margin_for_noul_is_distance_from_maximal_uncertainty_doubled() {
        let barely_over_half = Answer {
            value: AnswerValue::Noul(0.52),
            confidence: 0.52,
            probabilities: BTreeMap::new(),
        };
        assert!(
            (barely_over_half.margin() - 0.04).abs() < 1e-4,
            "{}",
            barely_over_half.margin()
        );

        let near_certain = Answer {
            value: AnswerValue::Noul(0.95),
            confidence: 0.95,
            probabilities: BTreeMap::new(),
        };
        assert!(
            (near_certain.margin() - 0.9).abs() < 1e-4,
            "{}",
            near_certain.margin()
        );
    }

    #[test]
    fn margin_is_zero_with_fewer_than_two_reported_probabilities() {
        let single = Answer {
            value: AnswerValue::Choice("only".to_string()),
            confidence: 0.9,
            probabilities: BTreeMap::from([("only".to_string(), 1.0_f32)]),
        };
        assert_eq!(single.margin(), 0.0);

        let none = Answer {
            value: AnswerValue::Score(0.0),
            confidence: 0.9,
            probabilities: BTreeMap::new(),
        };
        assert_eq!(none.margin(), 0.0);
    }

    #[test]
    fn decisive_requires_both_confidence_and_margin_for_choice_and_score() {
        let decisive = Answer {
            value: AnswerValue::Choice("technical".to_string()),
            confidence: 0.8,
            probabilities: BTreeMap::from([
                ("technical".to_string(), 0.8_f32),
                ("billing".to_string(), 0.2_f32),
            ]),
        };
        assert!(decisive.decisive(0.5, 0.2));

        let thin_margin = Answer {
            value: AnswerValue::Choice("technical".to_string()),
            confidence: 0.8,
            probabilities: BTreeMap::from([
                ("technical".to_string(), 0.51_f32),
                ("billing".to_string(), 0.49_f32),
            ]),
        };
        assert!(
            !thin_margin.decisive(0.5, 0.2),
            "margin 0.02 must fail the floor even at high confidence"
        );

        let low_confidence = Answer {
            value: AnswerValue::Choice("technical".to_string()),
            confidence: 0.3,
            probabilities: BTreeMap::from([
                ("technical".to_string(), 0.9_f32),
                ("billing".to_string(), 0.1_f32),
            ]),
        };
        assert!(
            !low_confidence.decisive(0.5, 0.2),
            "confidence 0.3 must fail the floor even at a wide margin"
        );
    }

    #[test]
    fn decisive_for_noul_ignores_confidence_and_checks_margin_only() {
        let thin = Answer {
            value: AnswerValue::Noul(0.52),
            confidence: 0.52,
            probabilities: BTreeMap::new(),
        };
        assert!(!thin.decisive(0.0, 0.2), "margin 0.04 must fail the floor");
        assert!(
            !thin.decisive(1.0, 0.2),
            "min_confidence must be ignored for noul"
        );

        let decisive = Answer {
            value: AnswerValue::Noul(0.95),
            confidence: 0.95,
            probabilities: BTreeMap::new(),
        };
        assert!(decisive.decisive(0.0, 0.2));
        assert!(
            decisive.decisive(1.0, 0.2),
            "min_confidence must be ignored for noul"
        );
    }
}
