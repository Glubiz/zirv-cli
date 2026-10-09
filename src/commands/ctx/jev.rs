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

use std::collections::{BTreeMap, BTreeSet};
use std::io::Write;
use std::path::Path;
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::commands::ctx::adapters;
use crate::commands::ctx::agent;
use crate::commands::ctx::attribution::Attribution;
use crate::commands::ctx::config::{CtxConfig, JevConfig, JevSiteFloor, ProxyTypesafeConfig};
use crate::commands::ctx::jev_counters;
use crate::commands::ctx::jev_relay;
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

/// The eight sites [`JevFloorsConfig`](crate::commands::ctx:: config::JevFloorsConfig) makes
/// tunable -- every other `decisive()` call site in this crate (safety/verification gates,
/// the harness proxy's own intake thresholds) keeps its compiled constant and has no
/// [`floor`] call at all (#803).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FloorSite {
    Context,
    HarvestScreen,
    HandoffSelect,
    CompactionSelect,
    Dispatch,
    LaunchEffort,
    Classify,
    Inject,
}

/// Use the site override field by field, falling back to the configured default for unset
/// floors.
pub(crate) fn floor(
    cfg: &CtxConfig,
    site: FloorSite,
    default_confidence: f32,
    default_margin: f32,
) -> (f32, f32) {
    let configured: &JevSiteFloor = match site {
        FloorSite::Context => &cfg.jev.floors.context,
        FloorSite::HarvestScreen => &cfg.jev.floors.harvest_screen,
        FloorSite::HandoffSelect => &cfg.jev.floors.handoff_select,
        FloorSite::CompactionSelect => &cfg.jev.floors.compaction_select,
        FloorSite::Dispatch => &cfg.jev.floors.dispatch,
        FloorSite::LaunchEffort => &cfg.jev.floors.launch_effort,
        FloorSite::Classify => &cfg.jev.floors.classify,
        FloorSite::Inject => &cfg.jev.floors.inject,
    };
    (
        configured.min_confidence.unwrap_or(default_confidence),
        configured.min_margin.unwrap_or(default_margin),
    )
}

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
    pub(crate) metadata_signature: Option<String>,
}

impl Question {
    /// A choice question from `(option, description)` pairs. For a
    /// caller-composed catch-all or an optional description, build
    /// [`Criteria::Choice`] directly instead -- this constructor is for the
    /// common case where every option has one.
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
            metadata_signature: None,
        }
    }

    /// A score question from an ordered list of level descriptions (index 0
    /// first). Only `metadata_score` (test-only) calls this directly today;
    /// production code builds `Score` questions as struct literals.
    #[allow(dead_code)]
    pub(crate) fn score(id: &str, instructions: &str, levels: &[&str]) -> Self {
        Question {
            id: id.to_string(),
            kind: QuestionKind::Score,
            instructions: instructions.to_string(),
            criteria: Criteria::Score(levels.iter().map(|level| level.to_string()).collect()),
            metadata_signature: None,
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
            metadata_signature: None,
        }
    }

    pub(crate) fn metadata_choice(
        id: &str,
        instructions: &'static str,
        options: &'static [(&'static str, &'static str)],
    ) -> Self {
        let mut question = Self::choice(id, instructions, options);
        question.metadata_signature = Some(question_signature(&question));
        question
    }

    pub(crate) fn metadata_noul(
        id: &str,
        instructions: &'static str,
        when_true: &'static str,
        when_false: &'static str,
    ) -> Self {
        let mut question = Self::noul(id, instructions, when_true, when_false);
        question.metadata_signature = Some(question_signature(&question));
        question
    }

    #[cfg(test)]
    fn metadata_score(
        id: &str,
        instructions: &'static str,
        levels: &'static [&'static str],
    ) -> Self {
        let mut question = Self::score(id, instructions, levels);
        question.metadata_signature = Some(question_signature(&question));
        question
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
    pub(crate) fn as_choice(&self) -> Option<&str> {
        match &self.value {
            AnswerValue::Choice(value) => Some(value.as_str()),
            AnswerValue::Score(_) | AnswerValue::Noul(_) => None,
        }
    }

    /// `Some(probability)` for a `Noul` answer, `None` for any other kind.
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

    /// The higher of a Score answer's two most probable level indices, with
    /// probability ties broken toward the higher level; `None` for other
    /// answer kinds, for fewer than two parseable indices, or when the
    /// answer's own `confidence` is below `min_confidence`.
    ///
    /// Only a thin MARGIN is the near-tie this resolves: two levels are both
    /// real candidates and the argmax would flip on an identical re-ask, so
    /// taking the higher keeps the same result whichever side wins -- as
    /// deterministic as discarding the answer, and it lets a monotonic field
    /// rise instead of dropping to the text-only baseline. A confidence
    /// BELOW its floor is the opposite situation: the model has no opinion
    /// and its mass is spread, so there is no pair to break and the baseline
    /// is the better signal. Resolving those upward too over-sized the
    /// `bump-timeout` and `ambiguous` cases of the live battery (trivial and
    /// direct on a cheap seat) into bounded work on a standard one -- the
    /// mirror of the under-sizing this whole path exists to prevent.
    pub(crate) fn near_tie_score(&self, min_confidence: f32) -> Option<f64> {
        if !matches!(self.value, AnswerValue::Score(_)) || self.confidence < min_confidence {
            return None;
        }
        let mut levels: Vec<(u32, f32)> = self
            .probabilities
            .iter()
            .filter_map(|(level, probability)| {
                level.parse().ok().map(|level| (level, *probability))
            })
            .collect();
        if levels.len() < 2 {
            return None;
        }
        levels.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| b.0.cmp(&a.0)));
        Some(f64::from(levels[0].0.max(levels[1].0)))
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
    UnsafeState,
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
            Self::UnsafeState => write!(f, "unsafe Jev metadata projection"),
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

#[derive(Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct NoulCriteria {
    #[serde(rename = "true", skip_serializing_if = "Option::is_none")]
    when_true: Option<String>,
    #[serde(rename = "false", skip_serializing_if = "Option::is_none")]
    when_false: Option<String>,
}

#[derive(Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase", deny_unknown_fields)]
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
        #[serde(default, skip_serializing_if = "Option::is_none")]
        criteria: Option<NoulCriteria>,
    },
}

/// Also `Deserialize` (issue jev-relay), unlike every other outgoing-only
/// wire type in this module: the relay (`jev_relay::forward`) parses an
/// already-encoded body it did NOT build itself back into this exact shape
/// to cheaply re-validate it (see [`safe_wire_request`]) before spending its
/// own credential forwarding it anywhere.
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
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
    // this module's own tests; `ask` reads `answers`/`usage` only. The
    // assertion lives inside `assert_eq!` in a `#[cfg(test)]` fn, which the
    // dead_code lint does not see through.
    #[allow(dead_code)]
    model: String,
    answers: BTreeMap<String, SystemOneAnswer>,
    usage: SystemOneUsage,
}

fn question_spec(question: &Question) -> QuestionSpec {
    match &question.criteria {
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
    }
}

fn build_request(state: &impl Serialize, questions: &[Question], model: &str) -> SystemOneRequest {
    let mapped = questions
        .iter()
        .map(|question| (question.id.clone(), question_spec(question)))
        .collect();
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

/// Shared by the direct call's own status handling and the relay's own
/// forwarding (`jev_relay::forward`), so a caller gets the identical
/// `JevError` variant for a given HTTP status whichever path answered.
pub(crate) fn status_error(status: u16) -> JevError {
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

fn question_signature(question: &Question) -> String {
    hash_hex(
        format!(
            "{:?}|{:?}|{:?}|{:?}",
            question.id, question.kind, question.instructions, question.criteria
        )
        .as_bytes(),
    )
}

fn encoded_request(
    state: &impl Serialize,
    questions: &[Question],
    model: &str,
) -> Result<(String, String), JevError> {
    let request = build_request(state, questions, model);
    let payload = serde_json::to_string(&request)
        .map_err(|error| JevError::Transport(format!("failed to encode request: {error}")))?;
    let cache_key = hash_hex(payload.as_bytes());
    Ok((payload, cache_key))
}

/// The `state` half of [`safe_metadata_request`]'s check, split out (issue
/// jev-relay) so [`safe_wire_request`] can re-run exactly this on a
/// wire-format body's own `state` field without needing a local `Question`
/// list at all.
fn safe_metadata_state(state: &serde_json::Value) -> bool {
    let Some(object) = state.as_object() else {
        return false;
    };
    if object.len() != 2
        || object.get("_zirv_metadata_only") != Some(&serde_json::Value::Bool(true))
    {
        return false;
    }
    let Some(rows) = object.get("facts").and_then(serde_json::Value::as_array) else {
        return false;
    };
    if rows.len() > 32
        || rows.iter().any(|row| {
            row.as_array().is_none_or(|cells| {
                cells.len() > 32
                    || cells.iter().any(|cell| match cell {
                        serde_json::Value::Null | serde_json::Value::Bool(_) => false,
                        serde_json::Value::Number(number) => {
                            number.as_u64().is_none_or(|value| value > 1_000_000)
                        }
                        _ => true,
                    })
            })
        })
    {
        return false;
    }
    serde_json::to_vec(state).is_ok_and(|bytes| bytes.len() <= 8 * 1024)
}

/// A bounded, single-word identifier: a question id or a choice option key.
/// Split out (issue jev-relay) so [`safe_wire_request`] can apply the exact
/// same charset/length rule to a wire-format question id without a local
/// `Question` to read it off.
fn valid_atom(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 32
        && value.bytes().enumerate().all(|(index, byte)| {
            if index == 0 {
                byte.is_ascii_alphabetic()
            } else {
                byte.is_ascii_alphanumeric() || byte == b'_'
            }
        })
}

/// The charset a Jev `model` name must stay inside -- split out (issue
/// jev-relay) for the same reason as [`valid_atom`].
fn valid_model_charset(model: &str) -> bool {
    model
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
}

pub(crate) fn safe_metadata_request(
    state: &serde_json::Value,
    questions: &[Question],
    model: &str,
) -> bool {
    if !safe_metadata_state(state) {
        return false;
    }
    if questions.is_empty() || questions.len() > 32 || model.len() > 64 {
        return false;
    }
    if !valid_model_charset(model) {
        return false;
    }
    questions.iter().all(|question| {
        question
            .metadata_signature
            .as_deref()
            .is_some_and(|signature| signature == question_signature(question))
            && valid_atom(&question.id)
            && !question.instructions.is_empty()
            && question.instructions.len() <= 512
            && match &question.criteria {
                Criteria::Choice(options) => {
                    !options.is_empty()
                        && options.len() <= 16
                        && options.iter().all(|(key, description)| {
                            valid_atom(key)
                                && description
                                    .as_deref()
                                    .is_some_and(|value| value.len() <= 512)
                        })
                }
                Criteria::Noul {
                    when_true,
                    when_false,
                } => [when_true, when_false]
                    .into_iter()
                    .all(|value| value.as_deref().is_some_and(|value| value.len() <= 512)),
                Criteria::Score(levels) => {
                    !levels.is_empty()
                        && levels.len() <= 16
                        && levels.iter().all(|level| level.len() <= 512)
                }
            }
    })
}

/// The relay's own cheap re-validation (issue jev-relay) of an
/// already-encoded request body it did not build itself: parses it back into
/// the exact wire shape `encoded_request` produces (`SystemOneRequest`, now
/// also `Deserialize` for this one purpose) and re-runs every structural
/// check [`safe_metadata_request`] applies to `state`/`model`/each
/// question's id and criteria -- everything BUT the `metadata_signature`
/// provenance check, which hashes a question's own static instructions/
/// criteria at construction time and has no wire-format equivalent to check
/// against. A relay forwards only a body that passes this; anything else
/// gets an `{"error": ...}` frame back, which `jev::ask`'s own relay step
/// treats as "fall back to a direct call", never as a forwarded answer.
fn safe_wire_request(parsed: &SystemOneRequest) -> bool {
    if !safe_metadata_state(&parsed.state) {
        return false;
    }
    if parsed.questions.is_empty() || parsed.questions.len() > 32 || parsed.model.len() > 64 {
        return false;
    }
    if !valid_model_charset(&parsed.model) {
        return false;
    }
    parsed.questions.iter().all(|(id, spec)| {
        valid_atom(id)
            && match spec {
                QuestionSpec::Choice {
                    instructions,
                    criteria,
                } => {
                    !instructions.is_empty()
                        && instructions.len() <= 512
                        && !criteria.is_empty()
                        && criteria.len() <= 16
                        && criteria.iter().all(|(key, description)| {
                            valid_atom(key)
                                && description
                                    .as_deref()
                                    .is_some_and(|value| value.len() <= 512)
                        })
                }
                QuestionSpec::Score {
                    instructions,
                    criteria,
                } => {
                    !instructions.is_empty()
                        && instructions.len() <= 512
                        && !criteria.is_empty()
                        && criteria.len() <= 16
                        && criteria.iter().all(|level| level.len() <= 512)
                }
                QuestionSpec::Noul {
                    instructions,
                    criteria,
                } => {
                    !instructions.is_empty()
                        && instructions.len() <= 512
                        && criteria.as_ref().is_none_or(|value| {
                            [&value.when_true, &value.when_false]
                                .into_iter()
                                .all(|side| side.as_deref().is_none_or(|s| s.len() <= 512))
                        })
                }
            }
    })
}

/// The body the relay forwards for `payload`, or `None` unless it parses strictly (no unknown
/// fields), passes [`safe_wire_request`], and every question's spec (instructions and options,
/// whatever its id) equals one of the `compiled` questions. The body is re-serialised from the
/// parsed value, so none of the client's bytes are forwarded.
pub(crate) fn compiled_wire_body(payload: &str, compiled: &[Question]) -> Option<String> {
    let parsed = serde_json::from_str::<SystemOneRequest>(payload).ok()?;
    if !safe_wire_request(&parsed) {
        return None;
    }
    let specs: Vec<QuestionSpec> = compiled.iter().map(question_spec).collect();
    if !parsed.questions.values().all(|spec| specs.contains(spec)) {
        return None;
    }
    serde_json::to_string(&parsed).ok()
}

#[cfg(test)]
pub(crate) fn cache_key_for(
    state: &impl Serialize,
    questions: &[Question],
    model: &str,
) -> Result<String, JevError> {
    encoded_request(state, questions, model).map(|(_, cache_key)| cache_key)
}

/// Test seam for `jev_relay`'s own tests: the exact encoded request body
/// [`ask`] would send, so a test can dial a relay directly with a
/// byte-identical payload without duplicating [`build_request`]'s encoding.
#[cfg(test)]
pub(crate) fn encode_for_test(
    state: &impl Serialize,
    questions: &[Question],
    model: &str,
) -> Result<String, JevError> {
    encoded_request(state, questions, model).map(|(payload, _)| payload)
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
/// callers pass only the audited numeric metadata envelope. Legacy text
/// states return `UnsafeState` before a cache read or network call.
///
/// Cache: the request body (`{state, model, questions}`, the exact bytes
/// this function would otherwise send) is hashed with SHA-256 and looked up
/// at `<state_dir>/jev-cache/<hash>.json` AFTER the credential and privacy
/// checks but before any network attempt -- an identical request gives an
/// identical answer by construction, with zero `Usage`, up to
/// `cache_ttl_secs` old. A miss (including an expired entry, or a corrupt/
/// unreadable file) falls through to the real call exactly as before; only
/// a genuine `200` response is stored, never an error. `cache_ttl_secs ==
/// 0` disables the cache entirely: no lookup, no write, every call reaches
/// the network. The returned `bool` is whether this answer was served from
/// the cache -- callers that record a decision line (`record`, below) pass
/// it through as `cached`.
/// The process-wide keep-alive `ureq::Agent` every Jev call now shares
/// (issue jev-relay), built once: constructing a fresh `Agent` means a fresh
/// rustls config/root store, and -- the whole point -- a fresh TCP+TLS
/// handshake on every call, roughly 400ms of fixed network setup dwarfing
/// Jev's own ~100-150ms of actual inference. This amortises that cost away
/// for any process that makes more than one Jev call: several `[jev]` gates
/// firing in the same short-lived hook process, or (the bigger win) the
/// relay's own supervisor process forwarding many hook-side calls, one warm
/// connection reused throughout. Timeouts are NOT baked in here -- they vary
/// per call (`cfg.timeout_secs`, operator-only via `REPO_FORBIDDEN`, but
/// still not a compile-time constant) -- they are applied per REQUEST
/// instead, in [`send_request`], via ureq 3's own `RequestBuilder::config()`
/// override; the connection pool this agent owns is unaffected either way.
static SHARED_AGENT: OnceLock<ureq::Agent> = OnceLock::new();

fn shared_agent() -> &'static ureq::Agent {
    SHARED_AGENT.get_or_init(|| {
        ureq::Agent::config_builder()
            .max_redirects(0)
            .build()
            .into()
    })
}

#[cfg(test)]
fn is_loopback_url(url: &str) -> bool {
    let host = crate::commands::ctx::health::dependency_from_base_url(url).unwrap_or_default();
    matches!(host.split(':').next(), Some("127.0.0.1" | "localhost"))
}

/// Posts one already-encoded Jev request body to `{base_url}/systemone`
/// through the process-wide [`shared_agent`], honouring `timeout_secs` for
/// THIS call only (see that function's own doc comment), and maps the
/// result the same way a direct call always has. Shared by [`ask`]'s own
/// direct-call fallback and the relay's own forwarding (`jev_relay::
/// forward`), so a caller gets byte-identical error mapping whichever path
/// answered. The credential is a plain owned `&str` -- never logged, and
/// this function is the only place it is ever attached to a request.
pub(crate) fn send_request(
    base_url: &str,
    credential: &str,
    timeout_secs: u64,
    payload: String,
) -> Result<String, JevError> {
    // A test run inherits the developer's real key and ~/.zirv config; it must never spend real requests.
    #[cfg(test)]
    if !is_loopback_url(base_url) {
        return Err(JevError::Transport(
            "tests may only reach a loopback Jev endpoint".to_string(),
        ));
    }
    let url = format!("{}/systemone", base_url.trim_end_matches('/'));
    let response = shared_agent()
        .post(&url)
        .config()
        .timeout_connect(Some(Duration::from_secs(timeout_secs)))
        .timeout_global(Some(Duration::from_secs(timeout_secs)))
        .build()
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

    response
        .body_mut()
        .read_to_string()
        .map_err(|error| JevError::Transport(error.to_string()))
}

/// Tries this session's own relay (issue jev-relay) before falling back to a
/// direct call: `None` whenever there is nothing useful to relay through --
/// no `ZIRV_CTX_SESSION` (never a supervised call at all, e.g. a bare `zirv
/// ctx jev status`), this very process IS the relay host (see
/// `jev_relay::is_relay_host`'s own doc comment for why that must never dial
/// itself), or the relay could not answer for any other reason at all (see
/// `jev_relay::try_via_relay`'s own doc comment for the full list). The
/// caller (`ask`, below) then falls straight through to [`send_request`]
/// exactly as it always has -- a relay is an optimisation, never required.
fn relay_send(state_dir: &Path, payload: &str, wait: Duration) -> Option<Result<String, JevError>> {
    let session = relay_session()?;
    jev_relay::try_via_relay(
        &StateDir::from_path(state_dir.to_path_buf()),
        &session,
        payload,
        wait,
    )
}

fn relay_session() -> Option<String> {
    std::env::var(adapters::SESSION_ENV)
        .ok()
        .filter(|value| !value.trim().is_empty())
}

/// A scrubbed worker holds no credential but its supervisor's relay answers for it.
fn relay_reachable(state: &StateDir) -> bool {
    relay_session().is_some_and(|session| jev_relay::is_reachable(state, &session))
}

pub(crate) fn ask(
    cfg: &ProxyTypesafeConfig,
    state_dir: &Path,
    cache_ttl_secs: u64,
    state: &impl Serialize,
    questions: &[Question],
) -> Result<(Answers, Usage, bool), JevError> {
    let credential = std::env::var(&cfg.credential_env)
        .ok()
        .filter(|value| !value.is_empty());
    if credential.is_none() && !relay_reachable(&StateDir::from_path(state_dir.to_path_buf())) {
        return Err(JevError::NoCredential(cfg.credential_env.clone()));
    }
    let safe_state = serde_json::to_value(state).map_err(|_| JevError::UnsafeState)?;
    if !safe_metadata_request(&safe_state, questions, &cfg.model) {
        return Err(JevError::UnsafeState);
    }
    let (payload, cache_key) = encoded_request(&safe_state, questions, &cfg.model)?;
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

    // Checked before choosing relay or direct: a live relay would otherwise send it from a non-test process.
    #[cfg(test)]
    if !is_loopback_url(&cfg.base_url) {
        return Err(JevError::Transport(
            "tests may only reach a loopback Jev endpoint".to_string(),
        ));
    }
    // Wait out the relay for as long as a direct call could take: giving up sooner re-sends the
    // request directly while the relay's copy is still in flight, a second live request.
    let relay_wait = Duration::from_secs(cfg.timeout_secs).saturating_add(Duration::from_secs(3));
    let body = match relay_send(state_dir, &payload, relay_wait) {
        Some(result) => result?,
        None => match &credential {
            Some(credential) => send_request(&cfg.base_url, credential, cfg.timeout_secs, payload)?,
            None => return Err(JevError::NoCredential(cfg.credential_env.clone())),
        },
    };
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
    has_credential(cfg)
        || StateDir::resolve(&crate::commands::ctx::config::env_from_process())
            .is_ok_and(|state| relay_reachable(&state))
}

/// Only this process's own env: what a relay host needs, since it forwards with that credential.
pub(crate) fn has_credential(cfg: &ProxyTypesafeConfig) -> bool {
    std::env::var(&cfg.credential_env).is_ok_and(|value| !value.is_empty())
}

/// Whether at least one `[jev]` gate is on -- the other half of
/// [`available`]'s own check (see that function's doc comment) for deciding
/// whether a `[jev]`-gated site would ever call [`ask`] at all, and (issue
/// jev-relay) now also whether hosting a relay for a session
/// (`jev_relay::start`) could possibly be useful: a relay bound for a
/// session with every gate off, or no credential, would sit there accepting
/// connections that never come, for no benefit at all.
pub(crate) fn any_gate_enabled(cfg: &JevConfig) -> bool {
    gate_list(cfg).iter().any(|(_, on)| *on)
}

pub(crate) const JEV_DECISIONS_FILE: &str = "jev-decisions.jsonl";
pub(crate) const JEV_EFFECTS_FILE: &str = "jev-effects.jsonl";
/// `{site, ts, reason, session}` rows for an enabled site that never asked (no credential, or a
/// state the metadata guard refused); a separate file so decision-log readers see only calls (#452).
pub(crate) const JEV_SKIPS_FILE: &str = "jev-skips.jsonl";
/// `{ts, from, to}` rows naming a launch's pre-minted session (`from`) that became another
/// session (`to`); a separate file so decision-log readers never see a non-decision row (#827).
const JEV_ALIASES_FILE: &str = "jev-session-aliases.jsonl";
/// The catalogue id `log::Delegation`/`price::price` prices the spend row
/// on -- see `catalogue.rs`'s `typesafe` vendor. Every `[jev]`-gated site
/// spends through the same vendor as the harness proxy, regardless of which
/// site asked.
const JEV_SPEND_AGENT: &str = "typesafe";

/// The session a supervisor (wrap, exec) runs on behalf of: it exports `ZIRV_CTX_SESSION` to its
/// child but not to itself, so its own Jev calls (handoff, harvest) would otherwise read "proxy".
static PROCESS_SESSION: Mutex<Option<String>> = Mutex::new(None);

/// Remember the session in a supervisor's child env, replacing any earlier one (handover swaps it).
pub(crate) fn adopt_session(turn_env: &[(String, String)]) {
    if let Some((_, session)) = turn_env
        .iter()
        .find(|(key, _)| key == adapters::SESSION_ENV)
    {
        adopt_session_id(session);
    }
}

/// [`adopt_session`] for a caller that holds the id itself (exec/chat before any Jev call).
pub(crate) fn adopt_session_id(session: &str) {
    if let Ok(mut current) = PROCESS_SESSION.lock() {
        *current = Some(session.to_string());
    }
}

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
        .or_else(|| PROCESS_SESSION.lock().ok().and_then(|s| s.clone()))
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
    /// The zirv version that wrote the row.
    v: &'static str,
    answers: BTreeMap<&'a str, AnswerRecord<'a>>,
    usage: &'a Usage,
    wall_ms: u64,
    fallbacks: &'a [String],
    /// Whether these answers were served from [`ask`]'s own decision cache
    /// rather than a real call -- `usage` is `0`/`0` whenever this is `true`.
    cached: bool,
    /// Session-scoped dash follow-up: this process's own [`session_and_
    /// principal`] session id (`"proxy"` for an intake-proxy call made
    /// before a harness session exists) -- what a session-scoped rollup
    /// filters on. `EffectRecord` already carried this; decisions did not,
    /// which is exactly why a session-scoped view could not be built from
    /// them until now.
    session: &'a str,
    /// This process's own campaign/candidate/trial/task ids, so `zirv workflow spend
    /// --campaign <id>` can count this decision. Never serialized when unset -- an existing
    /// row's JSON is byte-identical (#800).
    #[serde(default, skip_serializing_if = "Attribution::is_empty")]
    attribution: Attribution,
}

/// Append one decision row to `jev-decisions.jsonl` and return the `(ts, session, principal)` it
/// carries, for a caller that also writes the matching spend row. Best-effort like every recorder.
pub(crate) fn record_decision_row(
    state: &StateDir,
    site: &str,
    answers: &Answers,
    usage: &Usage,
    wall_ms: u64,
    fallbacks: &[String],
    cached: bool,
) -> (u64, String, String) {
    let ts = state::now_secs();
    let (session, principal) = session_and_principal();
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
        v: jev_counters::VERSION,
        answers: answer_records,
        usage,
        wall_ms,
        fallbacks,
        cached,
        session: &session,
        attribution: Attribution::from_env(),
    };
    if let Ok(line) = serde_json::to_string(&record)
        && state::create_private_dir_all(state.root()).is_ok()
        && let Ok(mut file) = state::open_private_append(&state.root().join(JEV_DECISIONS_FILE))
    {
        let _ = writeln!(file, "{line}");
    }
    (ts, session, principal)
}

/// Same (site, reason) skips closer together than this share one row.
const SKIP_DEDUP_SECS: u64 = 3600;
/// How much of the skips file's end the dedup check reads.
const SKIP_TAIL_BYTES: u64 = 64 * 1024;

/// Whether the newest row for (`site`, `reason`) in the file's tail is under [`SKIP_DEDUP_SECS`] old.
fn skip_recently_recorded(path: &Path, site: &str, reason: &str, now: u64) -> bool {
    use std::io::{Read, Seek, SeekFrom};
    let Ok(mut file) = std::fs::File::open(path) else {
        return false;
    };
    let len = file.metadata().map(|meta| meta.len()).unwrap_or(0);
    let start = len.saturating_sub(SKIP_TAIL_BYTES);
    let mut tail = Vec::new();
    if file.seek(SeekFrom::Start(start)).is_err() || file.read_to_end(&mut tail).is_err() {
        return false;
    }
    String::from_utf8_lossy(&tail)
        .lines()
        .rev()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .find(|row| row["site"] == site && row["reason"] == reason)
        .and_then(|row| row["ts"].as_u64())
        .is_some_and(|ts| now.saturating_sub(ts) < SKIP_DEDUP_SECS)
}

/// Append one skip row for `site`, unless the same (site, reason) was recorded within the last
/// hour. Best-effort like every recorder.
fn record_skip(state: &StateDir, site: &str, reason: &str) {
    let path = state.root().join(JEV_SKIPS_FILE);
    if skip_recently_recorded(&path, site, reason, state::now_secs()) {
        return;
    }
    let row = serde_json::json!({
        "site": site,
        "ts": state::now_secs(),
        "v": jev_counters::VERSION,
        "reason": reason,
        "session": session_and_principal().0,
    });
    if state::create_private_dir_all(state.root()).is_ok()
        && let Ok(mut file) = state::open_private_append(&state.root().join(JEV_SKIPS_FILE))
    {
        let _ = writeln!(file, "{row}");
    }
}

/// Record that rows written under `from` belong to `to`, e.g. intake rows written under the id a
/// launch minted before the persistent runtime chose its own. Best-effort like every recorder.
pub(crate) fn record_session_alias(state: &StateDir, from: &str, to: &str) {
    if from == to {
        return;
    }
    let row = serde_json::json!({"ts": state::now_secs(), "from": from, "to": to});
    if state::create_private_dir_all(state.root()).is_ok()
        && let Ok(mut file) = state::open_private_append(&state.root().join(JEV_ALIASES_FILE))
    {
        let _ = writeln!(file, "{row}");
    }
}

/// Every alias `from` whose `to` satisfies `is_target` (one hop). A missing file or torn line
/// contributes nothing.
pub(crate) fn alias_sources(state: &StateDir, is_target: impl Fn(&str) -> bool) -> Vec<String> {
    let Ok(text) = std::fs::read_to_string(state.root().join(JEV_ALIASES_FILE)) else {
        return Vec::new();
    };
    text.lines()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .filter_map(|row| match (row["from"].as_str(), row["to"].as_str()) {
            (Some(from), Some(to)) if is_target(to) => Some(from.to_string()),
            _ => None,
        })
        .collect()
}

/// `sessions` plus every alias `from` whose `to` is in it.
fn with_aliases(state: &StateDir, sessions: &BTreeSet<String>) -> BTreeSet<String> {
    let mut expanded = sessions.clone();
    expanded.extend(alias_sources(state, |to| sessions.contains(to)));
    expanded
}

/// Record decision and spend together so usage remains attributable to the verdict.
// Appends via state::open_private_append (O_APPEND): a read-then-rewrite lost lines when two zirv
// processes recorded at once. Best-effort: a write failure must never break the caller's decision.
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
    let (ts, session, principal) =
        record_decision_row(state, site, answers, usage, wall_ms, fallbacks, cached);
    let _ = log::append_delegation_cached(
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
        cached,
    );
}

#[derive(Debug, Clone, Copy, Default, Serialize)]
pub(crate) struct ObservedUsage {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fresh_input_tokens: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_creation_input_tokens: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_read_input_tokens: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output_tokens: Option<u64>,
}

/// Observed action following advice. An answer alone never counts as an effect.
#[derive(Debug, Serialize)]
pub(crate) struct JevEffect<'a> {
    pub site: &'static str,
    pub action: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub subject_id: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub item_id: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub outcome: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub baseline_count: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub actual_count: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub removed_bytes: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub elapsed_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub usage: Option<ObservedUsage>,
}

impl JevEffect<'_> {
    pub(crate) fn new(site: &'static str, action: &'static str) -> Self {
        Self {
            site,
            action,
            subject_id: None,
            item_id: None,
            reason: None,
            outcome: None,
            baseline_count: None,
            actual_count: None,
            removed_bytes: None,
            elapsed_ms: None,
            usage: None,
        }
    }
}

#[derive(Serialize)]
struct EffectRecord<'a> {
    ts: u64,
    v: &'static str,
    session: &'a str,
    principal: &'a str,
    #[serde(flatten)]
    effect: &'a JevEffect<'a>,
    #[serde(default, skip_serializing_if = "Attribution::is_empty")]
    attribution: Attribution,
}

/// Appends an effect only for an active site, without reading an answer cache.
pub(crate) fn record_effect(
    cfg: &CtxConfig,
    state: &StateDir,
    enabled: bool,
    effect: &JevEffect<'_>,
) {
    if !enabled || !available(&cfg.proxy.typesafe) {
        return;
    }
    let (session, principal) = session_and_principal();
    jev_counters::count(
        state,
        jev_counters::gate_for_site(effect.site),
        true,
        jev_counters::Stage::Acted,
        None,
    );
    let record = EffectRecord {
        ts: state::now_secs(),
        v: jev_counters::VERSION,
        session: &session,
        principal: &principal,
        effect,
        attribution: Attribution::from_env(),
    };
    if let Ok(line) = serde_json::to_string(&record)
        && state::create_private_dir_all(state.root()).is_ok()
        && let Ok(mut file) = state::open_private_append(&state.root().join(JEV_EFFECTS_FILE))
    {
        let _ = writeln!(file, "{line}");
    }
}

/// How far back [`usage_rollup`] looks when folding `jev-decisions.jsonl`
/// and `jev-effects.jsonl` into a per-site usage summary for `zirv ctx jev
/// status`: the last 7 days. A wall-clock window is chosen over an N-rows
/// cap because both logs are low-volume, best-effort advisory telemetry
/// (one line per gated call or observed effect, not a hot-path log) --
/// "usage this week" is what an operator deciding whether a gate earns its
/// keep actually wants, and it doesn't depend on how busy the week was.
const ROLLUP_WINDOW_SECS: u64 = 7 * 24 * 60 * 60;

/// The subset of a `jev-decisions.jsonl` line (see [`DecisionRecord`])
/// [`usage_rollup`] needs. Fields it doesn't list (`answers`, `usage`, the
/// fallback reason text) are simply ignored by `serde_json` -- this is
/// never `deny_unknown_fields`.
#[derive(Debug, Deserialize)]
struct DecisionRollupRow {
    site: String,
    ts: u64,
    wall_ms: u64,
    #[serde(default)]
    cached: bool,
    #[serde(default)]
    fallbacks: Vec<String>,
    /// A missing session id deserializes as empty and never matches a real session.
    #[serde(default)]
    session: String,
}

/// One `jev-skips.jsonl` line (see [`record_skip`]).
#[derive(Debug, Deserialize)]
struct SkipRollupRow {
    site: String,
    ts: u64,
    reason: String,
    #[serde(default)]
    session: String,
}

/// The subset of a `jev-effects.jsonl` line (see [`EffectRecord`])
/// [`usage_rollup`] needs.
#[derive(Debug, Deserialize)]
struct EffectRollupRow {
    site: String,
    ts: u64,
    #[serde(default)]
    removed_bytes: Option<u64>,
    /// See [`DecisionRollupRow::session`]'s own doc comment -- same
    /// back-compat default, same exclusion-not-parse-failure behavior for a
    /// pre-existing row.
    #[serde(default)]
    session: String,
}

/// The single most recent `jev-decisions.jsonl` row in the rollup window,
/// across every site -- [`JevRollup::last_call`]'s own payload. Dash refresh
/// PR2's JEV sidebar section `last` line.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct JevLastCall {
    pub site: String,
    pub ts: u64,
}

/// How many of [`JevRollup::recent_errors`]' own rows are kept -- click
/// affordance follow-up: the sidebar's `errors N \u{b7} <reason>` line opens
/// a dialog listing these, and both logs are low-volume advisory telemetry,
/// so 50 is generous headroom rather than a real cap on a busy window.
const MAX_KEPT_JEV_ERRORS: usize = 50;

/// One errored `jev-decisions.jsonl` row kept for the JEV errors dialog
/// (click affordance follow-up: the sidebar's `errors N \u{b7} <reason>`
/// line is clickable) -- the same timestamp/site/reason
/// [`JevRollup::latest_error_reason`] already derives, just kept for more
/// than the single newest row so the dialog has something to list.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct JevErrorEntry {
    pub ts: u64,
    pub site: String,
    pub reason: String,
}

/// [`usage_rollup`]'s full result: the per-site fold plus two "most recent
/// row in the window" facts that only make sense taken across every site at
/// once, gathered in the SAME read pass rather than a second scan of
/// `jev-decisions.jsonl` (dash refresh PR2's JEV sidebar section needs both
/// its `last` line and its `errors` line's latest reason, and the dashboard
/// reads this on a throttled cadence, never per frame).
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct JevRollup {
    pub sites: BTreeMap<String, JevSiteUsage>,
    /// `None` when no decision row fell inside the window at all.
    pub last_call: Option<JevLastCall>,
    /// The newest of [`Self::recent_errors`]' own reasons, or `None` when
    /// nothing in the window errored. Derived from `recent_errors` rather
    /// than tracked separately -- the newest-first list already names its
    /// own head.
    pub latest_error_reason: Option<String>,
    /// Every errored row in the window, newest first, bounded to
    /// [`MAX_KEPT_JEV_ERRORS`] -- click affordance follow-up: cached here so
    /// opening the JEV errors dialog on a click never reads the log files
    /// itself, only the last refresh's own rollup.
    pub recent_errors: Vec<JevErrorEntry>,
}

/// One site's folded usage over the rollup window: call volume, cache-hit
/// rate and latency from `jev-decisions.jsonl` (every site that went
/// through the shared [`ask`] client), plus observed-effect volume and
/// bytes removed from `jev-effects.jsonl` (every site that recorded a
/// [`JevEffect`]). The grouping key is the log's own `site` field, which is
/// finer-grained than a `[jev]` config gate in a few places (`context` gates
/// both `context-parent-reports` and `context-skill-descriptions`;
/// `supervisor` gates `crash`, `judge` and `handoff`) -- grouping by site
/// rather than by a hand-maintained site-to-gate table means a new call
/// site shows up here automatically instead of silently going uncounted. A
/// site that appears in only one of the two logs (e.g. `admin_dispatch`,
/// which records an effect without ever calling `ask`) simply leaves the
/// other half at its zero/`None` default.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub(crate) struct JevSiteUsage {
    pub calls: u64,
    pub cache_hit_rate: Option<f64>,
    pub wall_ms_p50: Option<u64>,
    pub wall_ms_p95: Option<u64>,
    pub errors: u64,
    pub effect_rows: u64,
    pub removed_bytes: u64,
}

#[derive(Default)]
struct JevSiteUsageBuilder {
    calls: u64,
    cache_hits: u64,
    errors: u64,
    wall_ms_samples: Vec<u64>,
    effect_rows: u64,
    removed_bytes: u64,
}

impl JevSiteUsageBuilder {
    fn finish(mut self) -> JevSiteUsage {
        self.wall_ms_samples.sort_unstable();
        JevSiteUsage {
            calls: self.calls,
            cache_hit_rate: if self.calls > 0 {
                Some(self.cache_hits as f64 / self.calls as f64)
            } else {
                None
            },
            wall_ms_p50: percentile(&self.wall_ms_samples, 0.50),
            wall_ms_p95: percentile(&self.wall_ms_samples, 0.95),
            errors: self.errors,
            effect_rows: self.effect_rows,
            removed_bytes: self.removed_bytes,
        }
    }
}

/// Nearest-rank percentile over an already-sorted slice. `None` for an empty
/// slice rather than a misleading `0`, so a site with zero decision rows
/// renders as "no data" instead of "instant".
fn percentile(sorted: &[u64], pct: f64) -> Option<u64> {
    if sorted.is_empty() {
        return None;
    }
    let idx = (((sorted.len() - 1) as f64) * pct).round() as usize;
    Some(sorted[idx.min(sorted.len() - 1)])
}

/// An empty stored session id must never match a requested session filter.
// Read-only and best-effort: a missing file contributes nothing, and an invalid or torn line is skipped,
// since several writers append with no cross-process lock.
pub(crate) fn usage_rollup(
    state: &StateDir,
    window_secs: u64,
    sessions: Option<&BTreeSet<String>>,
) -> JevRollup {
    let now = state::now_secs();
    let cutoff = now.saturating_sub(window_secs);
    let scope = sessions.map(|set| with_aliases(state, set));
    let in_scope = |session: &str| scope.as_ref().is_none_or(|set| set.contains(session));
    let mut builders: BTreeMap<String, JevSiteUsageBuilder> = BTreeMap::new();
    let mut last_call: Option<JevLastCall> = None;
    let mut recent_errors: Vec<JevErrorEntry> = Vec::new();

    if let Ok(text) = std::fs::read_to_string(state.root().join(JEV_DECISIONS_FILE)) {
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            let Ok(row) = serde_json::from_str::<DecisionRollupRow>(line) else {
                continue;
            };
            if row.ts < cutoff || !in_scope(&row.session) {
                continue;
            }
            if last_call.as_ref().is_none_or(|last| row.ts >= last.ts) {
                last_call = Some(JevLastCall {
                    site: row.site.clone(),
                    ts: row.ts,
                });
            }
            if !row.fallbacks.is_empty() {
                recent_errors.push(JevErrorEntry {
                    ts: row.ts,
                    site: row.site.clone(),
                    reason: row.fallbacks.join(", "),
                });
            }
            let entry = builders.entry(row.site).or_default();
            entry.calls += 1;
            if row.cached {
                entry.cache_hits += 1;
            } else {
                entry.wall_ms_samples.push(row.wall_ms);
            }
            if !row.fallbacks.is_empty() {
                entry.errors += 1;
            }
        }
    }

    // A skip is an error the operator must see, never a call: no latency, no cache-hit share.
    if let Ok(text) = std::fs::read_to_string(state.root().join(JEV_SKIPS_FILE)) {
        for line in text.lines() {
            let Ok(row) = serde_json::from_str::<SkipRollupRow>(line.trim()) else {
                continue;
            };
            if row.ts < cutoff || !in_scope(&row.session) {
                continue;
            }
            recent_errors.push(JevErrorEntry {
                ts: row.ts,
                site: row.site.clone(),
                reason: row.reason,
            });
            builders.entry(row.site).or_default().errors += 1;
        }
    }

    if let Ok(text) = std::fs::read_to_string(state.root().join(JEV_EFFECTS_FILE)) {
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            let Ok(row) = serde_json::from_str::<EffectRollupRow>(line) else {
                continue;
            };
            if row.ts < cutoff || !in_scope(&row.session) {
                continue;
            }
            let entry = builders.entry(row.site).or_default();
            entry.effect_rows += 1;
            entry.removed_bytes += row.removed_bytes.unwrap_or(0);
        }
    }

    // Newest first, bounded: lines are ordinarily already in append order,
    // but nothing here may rely on that (see this function's own doc
    // comment on torn/out-of-order writes from several call sites), so this
    // sorts explicitly rather than just reversing.
    recent_errors.sort_by_key(|e| std::cmp::Reverse(e.ts));
    recent_errors.truncate(MAX_KEPT_JEV_ERRORS);
    let latest_error_reason = recent_errors.first().map(|e| e.reason.clone());

    JevRollup {
        sites: builders
            .into_iter()
            .map(|(site, builder)| (site, builder.finish()))
            .collect(),
        last_call,
        latest_error_reason,
        recent_errors,
    }
}

fn fmt_rate(rate: Option<f64>) -> String {
    match rate {
        Some(r) => format!("{:.0}%", r * 100.0),
        None => "n/a".to_string(),
    }
}

fn fmt_ms(ms: Option<u64>) -> String {
    match ms {
        Some(v) => format!("{v}ms"),
        None => "n/a".to_string(),
    }
}

pub(crate) enum AdvisoryStatus {
    Disabled,
    MissingCredential,
    Answered(Answers),
    Failed,
}

/// The one advisory entry point a `[jev]`-gated site calls: short-circuits
/// to `None` with no network call when `enabled` is false (no log line) or
/// the `[proxy.typesafe]` credential is not set ([`available`], one
/// `jev-skips.jsonl` row) -- either way the caller's own pre-existing
/// deterministic path runs byte-identical to today. Otherwise runs one bounded [`ask`] call and
/// [`record`]s the outcome either way: a successful call's own answers, or
/// an empty answer set carrying the error's `Display` as the one fallback
/// reason. Never panics, never propagates -- same posture as every other
/// best-effort seam in this crate.
pub(crate) fn advise(
    cfg: &CtxConfig,
    state_dir: &StateDir,
    site: &str,
    enabled: bool,
    state: &impl Serialize,
    questions: &[Question],
) -> Option<Answers> {
    match advise_detailed(cfg, state_dir, site, enabled, state, questions) {
        AdvisoryStatus::Answered(answers) => Some(answers),
        AdvisoryStatus::Disabled | AdvisoryStatus::MissingCredential | AdvisoryStatus::Failed => {
            None
        }
    }
}

/// Like [`advise`], with explicit no-call and failed-call outcomes for telemetry.
pub(crate) fn advise_detailed(
    cfg: &CtxConfig,
    state_dir: &StateDir,
    site: &str,
    enabled: bool,
    state: &impl Serialize,
    questions: &[Question],
) -> AdvisoryStatus {
    advise_gated(
        jev_counters::gate_for_site(site),
        cfg,
        state_dir,
        site,
        enabled,
        state,
        questions,
    )
}

/// [`advise_detailed`] counting its funnel under an explicit `[jev]` gate key, for a site whose
/// name does not identify the gate (`approve` asks under both `approve` and `approve_allow`).
pub(crate) fn advise_gated(
    gate: &str,
    cfg: &CtxConfig,
    state_dir: &StateDir,
    site: &str,
    enabled: bool,
    state: &impl Serialize,
    questions: &[Question],
) -> AdvisoryStatus {
    use jev_counters::{Stage, count};
    if !enabled {
        return AdvisoryStatus::Disabled;
    }
    count(state_dir, gate, true, Stage::Reached, None);
    count(state_dir, gate, true, Stage::Eligible, None);
    if !available(&cfg.proxy.typesafe) {
        let reason = JevError::NoCredential(cfg.proxy.typesafe.credential_env.clone());
        record_skip(state_dir, site, &reason.to_string());
        count(state_dir, gate, true, Stage::Exit, Some("no_credential"));
        return AdvisoryStatus::MissingCredential;
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
            count(
                state_dir,
                gate,
                true,
                if cached {
                    Stage::SentCache
                } else {
                    Stage::SentNet
                },
                None,
            );
            count(state_dir, gate, true, Stage::Answered, None);
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
            AdvisoryStatus::Answered(answers)
        }
        Err(error) => {
            if matches!(error, JevError::UnsafeState) {
                record_skip(state_dir, site, &error.to_string());
                count(state_dir, gate, true, Stage::Exit, Some("unsafe_state"));
                return AdvisoryStatus::Failed;
            }
            count(state_dir, gate, true, Stage::SentNet, None);
            count(state_dir, gate, true, Stage::Exit, Some("request_failed"));
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
            AdvisoryStatus::Failed
        }
    }
}

/// A gated site's caller-side early return: counts one pass with its reason code. Nothing for an
/// off gate, so a gate that is off costs one branch.
pub(crate) fn exit(state: &StateDir, gate: &str, enabled: bool, reason: &'static str) {
    jev_counters::count(state, gate, enabled, jev_counters::Stage::Reached, None);
    jev_counters::count(
        state,
        gate,
        enabled,
        jev_counters::Stage::Exit,
        Some(reason),
    );
}

/// A gated site with no [`advise`] call of its own (it only records effects) passed its
/// preconditions: counts it as reached and eligible.
pub(crate) fn pass(state: &StateDir, gate: &str) {
    jev_counters::count(state, gate, true, jev_counters::Stage::Reached, None);
    jev_counters::count(state, gate, true, jev_counters::Stage::Eligible, None);
}

/// The preamble of a gated site: false when its gate is off, or when Jev is unavailable (counted
/// and recorded as a skip row). A site that passes this and still returns early calls [`exit`].
pub(crate) fn gate_open(cfg: &CtxConfig, state: &StateDir, gate: &str, enabled: bool) -> bool {
    if !enabled {
        return false;
    }
    if available(&cfg.proxy.typesafe) {
        return true;
    }
    let reason = JevError::NoCredential(cfg.proxy.typesafe.credential_env.clone());
    record_skip(state, gate, &reason.to_string());
    exit(state, gate, true, "no_credential");
    false
}

/// Returns the environment variable name currently configured for the Jev
/// credential, as set in `cfg.proxy.typesafe.credential_env`. Exposed so
/// the setup wizard can tell the operator exactly which variable to export.
pub fn credential_env_name(cfg: &CtxConfig) -> String {
    cfg.proxy.typesafe.credential_env.clone()
}

/// Whether the configured credential environment variable is set and
/// non-empty, without ever returning or logging its value. Exposed so
/// the setup wizard can distinguish "credential missing" from "gate off".
pub fn credential_present(cfg: &CtxConfig) -> bool {
    available(&cfg.proxy.typesafe)
}

/// Every live `[jev]` gate name and whether it is on: the one list `status`, `status_json`,
/// [`any_gate_enabled`] and the dashboard feed read. The deprecated `memory` key is not a gate.
pub(crate) fn gate_list(cfg: &JevConfig) -> [(&'static str, bool); 20] {
    [
        ("supervisor", cfg.supervisor),
        ("dispatch", cfg.dispatch),
        ("review", cfg.review),
        ("gates", cfg.gates),
        ("context", cfg.context),
        ("intake_savings", cfg.intake_savings),
        ("review_reuse", cfg.review_reuse),
        ("harvest_screen", cfg.harvest_screen),
        ("admin_dispatch", cfg.admin_dispatch),
        ("approve", cfg.approve),
        ("approve_allow", cfg.approve_allow),
        ("classify", cfg.classify),
        ("handoff_select", cfg.handoff_select),
        ("compaction_select", cfg.compaction_select),
        ("inject_screen", cfg.inject_screen),
        ("inject", cfg.inject),
        ("stop_verify", cfg.stop_verify),
        ("missing_tests", cfg.missing_tests),
        ("launch_effort", cfg.launch_effort),
        ("retry", cfg.retry),
    ]
}

use clap::{Args, Subcommand};

/// Subcommands for `zirv ctx jev`.
#[derive(Debug, Subcommand)]
pub enum JevCommand {
    /// Report Jev client status: gates, credential presence, endpoint/model,
    /// and why it is or is not active. Reads configuration only; never makes
    /// network calls or reads credential values.
    Status {
        #[arg(long)]
        repo: Option<std::path::PathBuf>,
        #[arg(long)]
        json: bool,
    },
    /// Measurement verb: asks one Jev site's real production question(s) for
    /// a fixture input K times with the cache disabled, applies that site's
    /// production floor and answer-to-action rule, and prints what
    /// production would have DONE on each rep -- see `jev_probe.rs` for the
    /// full contract. Spends real Jev calls and writes the normal decision/
    /// spend log rows; never any other side effect.
    Probe {
        #[arg(long)]
        site: String,
        #[arg(long)]
        case: std::path::PathBuf,
        #[arg(long)]
        reps: u32,
        #[arg(long)]
        repo: Option<std::path::PathBuf>,
    },
}

/// Arguments for `zirv ctx jev`.
#[derive(Debug, Args)]
pub struct JevArgs {
    #[command(subcommand)]
    pub command: JevCommand,
}

/// Handler for the `zirv ctx jev` verb.
pub fn run_jev(args: &JevArgs, writer: &mut impl Write) -> crate::commands::ctx::CtxResult<i32> {
    match &args.command {
        JevCommand::Status { repo, json } => {
            let resolved_repo = match repo {
                Some(repo) => repo.clone(),
                None => std::env::current_dir()?,
            };
            let cfg = CtxConfig::load(&resolved_repo, &|key| std::env::var(key).ok())?;
            let state = StateDir::resolve(&|key| std::env::var(key).ok())?;

            if *json {
                let rollup = usage_rollup(&state, ROLLUP_WINDOW_SECS, None);
                let now = state::now_secs();
                let mut json_output = status_json(&cfg, &rollup.sites);
                json_output["gate_status"] =
                    jev_counters::gate_rows(&cfg.jev, &jev_counters::rollup(&state, now))
                        .into_iter()
                        .map(|row| {
                            serde_json::json!({
                                "gate": row.key,
                                "on": row.on,
                                "label": row.label.as_str(),
                                "note": row.note,
                                "idle_reason": row.idle_reason,
                                "counts": row.counts,
                            })
                        })
                        .collect();
                json_output["warnings"] =
                    version_warnings(&state, now, std::env::current_dir().ok().as_deref()).into();
                writeln!(writer, "{}", serde_json::to_string_pretty(&json_output)?)?;
                Ok(0)
            } else {
                status(&cfg, &state, writer)?;
                Ok(0)
            }
        }
        JevCommand::Probe {
            site,
            case,
            reps,
            repo,
        } => super::jev_probe::run_probe(site, case, *reps, repo.as_deref(), writer),
    }
}

/// Builds the `--json` payload for [`run_jev`]'s `Status` subcommand: the
/// same gates/credential/endpoint/verdict shape as before, plus a `usage`
/// object carrying [`usage_rollup`]'s window and its per-site map. Factored
/// out of `run_jev` so a test can assert its shape without going through
/// `CtxConfig::load`/`StateDir::resolve`'s real filesystem and env lookups.
fn status_json(cfg: &CtxConfig, rollup: &BTreeMap<String, JevSiteUsage>) -> serde_json::Value {
    let any_gate_on = any_gate_enabled(&cfg.jev);
    let cred_present = available(&cfg.proxy.typesafe);
    let cred_env = &cfg.proxy.typesafe.credential_env;

    let verdict = if any_gate_on && cred_present {
        "active"
    } else if any_gate_on && !cred_present {
        "inactive_credential_missing"
    } else if !any_gate_on && cred_present {
        "inactive_no_gate"
    } else {
        "inactive_both"
    };

    let gates: serde_json::Map<String, serde_json::Value> = gate_list(&cfg.jev)
        .into_iter()
        .map(|(name, on)| (name.to_string(), on.into()))
        .collect();
    serde_json::json!({
        "gates": gates,
        "credential_env": cred_env,
        "credential_present": cred_present,
        "endpoint": cfg.proxy.typesafe.base_url,
        "model": cfg.proxy.typesafe.model,
        "status": verdict,
        "usage": {
            "window_days": ROLLUP_WINDOW_SECS / 86_400,
            "sites": rollup,
        },
    })
}

/// Warnings that the ledger or the working tree was written by a zirv other than this binary.
fn version_warnings(state: &StateDir, now: u64, cwd: Option<&Path>) -> Vec<String> {
    let running = jev_counters::VERSION;
    let mut warnings = Vec::new();
    let foreign = jev_counters::foreign_versions(state, now, running);
    if !foreign.is_empty() {
        warnings.push(format!(
            "ledger rows in the last {} days were written by zirv {} but this binary is {running}",
            jev_counters::STATUS_WINDOW_DAYS,
            foreign.into_iter().collect::<Vec<_>>().join(", "),
        ));
    }
    if let Some(tree) = cwd.and_then(jev_counters::source_tree_version)
        && tree != running
    {
        warnings.push(format!(
            "this directory is the zirv source tree at {tree} but the running binary is {running}; hooks run the installed binary"
        ));
    }
    warnings
}

/// Prints a read-only status report of whether Jev is enabled and why or why
/// not: each operator gate,
/// the credential env var name and presence, the endpoint base URL, model, and
/// a one-line verdict. Never makes a network call, never reads the credential
/// value, never writes config or creates directories. The output format
/// matches the style of `zirv ctx capabilities` (available/unavailable lines
/// with diagnosis underneath), followed by a [`usage_rollup`] section so an
/// operator can see call volume, cache-hit rate, latency and effect size per
/// site without a separate command.
pub fn status(
    cfg: &CtxConfig,
    state: &StateDir,
    writer: &mut impl Write,
) -> Result<(), Box<dyn std::error::Error>> {
    use std::fmt::Write as FmtWrite;

    // Determine if any gate is on
    let gates = gate_list(&cfg.jev);
    let any_gate_on = any_gate_enabled(&cfg.jev);

    // Check credential
    let cred_env = &cfg.proxy.typesafe.credential_env;
    let cred_present = available(&cfg.proxy.typesafe);

    // Determine verdict and diagnosis
    let mut diagnosis = String::new();
    let verdict = if any_gate_on && cred_present {
        "active"
    } else if any_gate_on && !cred_present {
        let _ = writeln!(diagnosis, "credential {} not set", cred_env);
        let _ = writeln!(diagnosis, "remedy: export {}=<api-key>", cred_env);
        "inactive"
    } else if !any_gate_on && cred_present {
        let _ = writeln!(diagnosis, "no gate enabled");
        let enabled_gates = gates
            .iter()
            .filter(|(_, on)| *on)
            .map(|(name, _)| format!("jev.{}", name))
            .collect::<Vec<_>>()
            .join(", ");
        if enabled_gates.is_empty() {
            let _ = writeln!(diagnosis, "remedy: zirv ctx config set jev.review true");
        }
        "inactive"
    } else {
        let _ = writeln!(
            diagnosis,
            "no gate enabled; credential {} not set",
            cred_env
        );
        let _ = writeln!(
            diagnosis,
            "remedy: zirv ctx config set jev.review true && export {}=<api-key>",
            cred_env
        );
        "inactive"
    };

    let now = state::now_secs();
    let counters = jev_counters::rollup(state, now);
    for row in jev_counters::gate_rows(&cfg.jev, &counters) {
        let counts = &row.counts;
        writeln!(
            writer,
            "jev.{:<12} {:<3} {:<11} reached={} eligible={} sent={}net+{}cache answered={} acted={}",
            row.key,
            if row.on { "on" } else { "off" },
            row.label.as_str(),
            counts.reached,
            counts.eligible,
            counts.sent_net,
            counts.sent_cache,
            counts.answered,
            counts.acted,
        )?;
        if let Some(note) = row.note {
            writeln!(writer, "      {note}")?;
        }
        if let Some(idle) = row.idle_reason {
            writeln!(writer, "      idle: waits on {idle}")?;
        }
        for (reason, n) in &counts.exits {
            writeln!(writer, "      exit {reason}: {n}")?;
        }
    }
    for warning in version_warnings(state, now, std::env::current_dir().ok().as_deref()) {
        writeln!(writer, "warning: {warning}")?;
    }

    // Print credential info
    if cred_present {
        writeln!(writer, "credential    present ({}) set", cred_env)?;
    } else {
        writeln!(writer, "credential    missing")?;
        writeln!(writer, "              {}", cred_env)?;
    }

    // Print endpoint and model
    writeln!(writer, "endpoint      {}", cfg.proxy.typesafe.base_url)?;
    writeln!(writer, "model         {}", cfg.proxy.typesafe.model)?;

    // Print verdict
    writeln!(writer, "status        {}", verdict)?;
    if !diagnosis.is_empty() {
        for line in diagnosis.trim().lines() {
            writeln!(writer, "              {}", line)?;
        }
    }

    // Print the usage rollup
    let rollup = usage_rollup(state, ROLLUP_WINDOW_SECS, None);
    let window_days = ROLLUP_WINDOW_SECS / 86_400;
    writeln!(writer)?;
    writeln!(writer, "usage (last {window_days}d)")?;
    if rollup.sites.is_empty() {
        writeln!(writer, "  (no calls or effects recorded)")?;
    } else {
        for (site, usage) in &rollup.sites {
            writeln!(
                writer,
                "  {:<30} calls={:<5} cache_hit={:<6} wall_p50={:<8} wall_p95={:<8} errors={:<4} effects={:<5} removed_bytes={}",
                site,
                usage.calls,
                fmt_rate(usage.cache_hit_rate),
                fmt_ms(usage.wall_ms_p50),
                fmt_ms(usage.wall_ms_p95),
                usage.errors,
                usage.effect_rows,
                usage.removed_bytes,
            )?;
        }
    }

    Ok(())
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

    fn legacy_state() -> SampleState {
        SampleState {
            request: "fix the typo".to_string(),
        }
    }

    fn legacy_questions() -> Vec<Question> {
        vec![
            Question {
                id: "intent".to_string(),
                kind: QuestionKind::Choice,
                metadata_signature: None,
                instructions: "pick one".to_string(),
                criteria: Criteria::Choice(vec![
                    ("feature".to_string(), Some("adds behavior".to_string())),
                    ("other".to_string(), None),
                ]),
            },
            Question {
                id: "complexity".to_string(),
                kind: QuestionKind::Score,
                metadata_signature: None,
                instructions: "how complex".to_string(),
                criteria: Criteria::Score(vec!["trivial".to_string(), "bounded".to_string()]),
            },
            Question {
                id: "needs_clarification".to_string(),
                kind: QuestionKind::Noul,
                metadata_signature: None,
                instructions: "ambiguous?".to_string(),
                criteria: Criteria::Noul {
                    when_true: Some("yes".to_string()),
                    when_false: Some("no".to_string()),
                },
            },
        ]
    }

    fn sample_state() -> serde_json::Value {
        serde_json::json!({"_zirv_metadata_only": true, "facts": [[1, 2, true]]})
    }

    fn sample_questions() -> Vec<Question> {
        vec![
            Question::metadata_choice(
                "intent",
                "Pick a category from coarse metadata only.",
                &[("feature", "adds behavior"), ("other", "other category")],
            ),
            Question::metadata_score(
                "complexity",
                "How complex is the category?",
                &["trivial", "bounded"],
            ),
            Question::metadata_noul(
                "needs_clarification",
                "Is clarification required from coarse metadata?",
                "yes",
                "no",
            ),
        ]
    }

    #[test]
    fn detailed_advice_distinguishes_disabled_and_missing_key_without_writes() {
        let dir = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_path(dir.path().to_path_buf());
        let mut cfg = CtxConfig::default();
        cfg.proxy.typesafe.credential_env = "JEV_TEST_DETAILED_MISSING_KEY_737".into();
        let disabled = advise_detailed(
            &cfg,
            &state,
            "context",
            false,
            &sample_state(),
            &sample_questions(),
        );
        let missing = advise_detailed(
            &cfg,
            &state,
            "context",
            true,
            &sample_state(),
            &sample_questions(),
        );
        assert!(matches!(disabled, AdvisoryStatus::Disabled));
        assert!(matches!(missing, AdvisoryStatus::MissingCredential));
        assert!(!dir.path().join(JEV_DECISIONS_FILE).exists());
        assert!(!dir.path().join(JEV_CACHE_DIR).exists());
    }

    #[test]
    fn effect_record_requires_gate_and_key_and_preserves_unknown_usage() {
        let dir = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_path(dir.path().to_path_buf());
        let mut cfg = CtxConfig::default();
        cfg.proxy.typesafe.credential_env = "JEV_TEST_EFFECT_GATE_737".into();
        let mut effect = JevEffect::new("context", "description_removed");
        effect.subject_id = Some("task-fingerprint");
        effect.item_id = Some("skill-id");
        effect.baseline_count = Some(1);
        effect.actual_count = Some(0);
        effect.removed_bytes = Some(42);
        record_effect(&cfg, &state, false, &effect);
        record_effect(&cfg, &state, true, &effect);
        assert!(!dir.path().join(JEV_EFFECTS_FILE).exists());
        with_credential(&cfg.proxy.typesafe.credential_env, "secret", || {
            record_effect(&cfg, &state, true, &effect);
        });
        let text = std::fs::read_to_string(dir.path().join(JEV_EFFECTS_FILE)).expect("effect");
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 1);
        let value: serde_json::Value = serde_json::from_str(lines[0]).expect("json");
        assert_eq!(value["site"], "context");
        assert_eq!(value["action"], "description_removed");
        assert_eq!(value["removed_bytes"], 42);
        assert!(value.get("usage").is_none());
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
        let request = build_request(&legacy_state(), &legacy_questions(), "jev-latest");
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
                metadata_signature: None,
                instructions: String::new(),
                criteria: Criteria::Choice(Vec::new()),
            },
            Question {
                id: "urgency".to_string(),
                kind: QuestionKind::Score,
                metadata_signature: None,
                instructions: String::new(),
                criteria: Criteria::Score(Vec::new()),
            },
            Question {
                id: "needs_human".to_string(),
                kind: QuestionKind::Noul,
                metadata_signature: None,
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
                Question::metadata_choice(
                    "category",
                    "Pick category from coarse facts.",
                    &[("technical", "technical")],
                ),
                Question::metadata_score(
                    "urgency",
                    "Pick urgency from coarse facts.",
                    &["low", "high"],
                ),
                Question::metadata_noul("needs_human", "Is human input needed?", "yes", "no"),
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

    /// Session-scoped dash follow-up: a decision row now carries the calling
    /// session id (`ZIRV_CTX_SESSION`), the same value `record_effect`'s own
    /// `EffectRecord` already wrote -- without this, a session-scoped rollup
    /// had nothing on a decision row to filter.
    #[test]
    fn record_writes_the_calling_sessions_id() {
        let state_dir = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_path(state_dir.path().to_path_buf());
        let cfg = CtxConfig::default();
        let usage = Usage {
            input_tokens: 0,
            output_tokens: 0,
        };
        unsafe {
            std::env::set_var(adapters::SESSION_ENV, "sess-under-test");
        }
        record(
            &state,
            &cfg,
            "memory",
            &Answers::new(),
            &usage,
            1,
            &[],
            false,
        );
        unsafe {
            std::env::remove_var(adapters::SESSION_ENV);
        }

        let text = std::fs::read_to_string(state_dir.path().join(JEV_DECISIONS_FILE))
            .expect("jev-decisions.jsonl");
        let value: serde_json::Value =
            serde_json::from_str(text.lines().next().expect("one line")).expect("parse json");
        assert_eq!(value["session"], "sess-under-test");
    }

    /// Tests inherit the developer's real credential and `~/.zirv` config, so the default
    /// endpoint must be unreachable from a test build (it once spent real vendor requests).
    #[test]
    fn a_test_build_refuses_to_send_to_the_real_endpoint() {
        let default = ProxyTypesafeConfig::default();
        let result = send_request(&default.base_url, "dummy-not-a-key", 1, "{}".to_string());
        assert!(
            matches!(&result, Err(JevError::Transport(reason)) if reason.contains("loopback")),
            "got {result:?}"
        );
    }

    /// A loopback name in the userinfo must not make the real host look local.
    #[test]
    fn the_loopback_check_reads_the_host_not_the_userinfo() {
        assert!(is_loopback_url("http://127.0.0.1:8080/v1"));
        assert!(is_loopback_url("http://localhost/v1"));
        assert!(!is_loopback_url("https://127.0.0.1@api.typesafe.ai/v1"));
        assert!(!is_loopback_url("https://localhost:80@api.typesafe.ai"));
    }

    /// An exec restart adopts a new session id; later calls must follow it.
    #[test]
    fn adopting_a_new_session_replaces_the_previous_one() {
        unsafe {
            std::env::remove_var(adapters::SESSION_ENV);
        }
        adopt_session_id("first-session");
        adopt_session_id("second-session");
        let (session, _) = session_and_principal();
        if let Ok(mut current) = PROCESS_SESSION.lock() {
            *current = None;
        }
        assert_eq!(session, "second-session");
    }

    /// A supervisor process never has `ZIRV_CTX_SESSION` itself (only its child does), so its own
    /// Jev calls were logged as "proxy" and dropped by every session-scoped rollup.
    #[test]
    fn supervisor_calls_are_attributed_to_the_session_it_adopted() {
        let state_dir = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_path(state_dir.path().to_path_buf());
        let cfg = CtxConfig::default();
        let usage = Usage {
            input_tokens: 0,
            output_tokens: 0,
        };
        unsafe {
            std::env::remove_var(adapters::SESSION_ENV);
        }
        adopt_session(&[(
            adapters::SESSION_ENV.to_string(),
            "sess-adopted".to_string(),
        )]);
        record(
            &state,
            &cfg,
            "memory",
            &Answers::new(),
            &usage,
            1,
            &[],
            false,
        );
        if let Ok(mut current) = PROCESS_SESSION.lock() {
            *current = None;
        }

        let scope: BTreeSet<String> = ["sess-adopted".to_string()].into();
        let rollup = usage_rollup(&state, ROLLUP_WINDOW_SECS, Some(&scope));
        assert_eq!(rollup.sites.get("memory").map(|u| u.calls), Some(1));
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
            let questions = vec![Question::metadata_choice(
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
    fn the_funnel_counts_one_call_from_reached_to_answered_then_a_cache_hit() {
        let text = std::fs::read_to_string(fixture("proxy/jev-response.json")).expect("fixture");
        let body: &'static str = Box::leak(text.into_boxed_str());
        let (url, handle) = one_shot_server(200, body);
        let state_dir = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_path(state_dir.path().to_path_buf());
        with_credential("JEV_TEST_KEY_FUNNEL", "secret", || {
            let mut cfg = CtxConfig::default();
            cfg.proxy.typesafe = config(url, "JEV_TEST_KEY_FUNNEL", 5);
            cfg.jev.cache_ttl_secs = 60;
            let questions = vec![Question::metadata_choice(
                "category",
                "pick one",
                &[("technical", "a technical question")],
            )];
            for _ in 0..2 {
                assert!(
                    advise(&cfg, &state, "classify", true, &sample_state(), &questions).is_some()
                );
            }
        });
        handle.join().expect("server thread must not panic");

        let counts = jev_counters::rollup(&state, state::now_secs());
        let classify = &counts["classify"];
        assert_eq!(
            (
                classify.reached,
                classify.eligible,
                classify.sent_net,
                classify.sent_cache,
                classify.answered
            ),
            (2, 2, 1, 1, 2)
        );
        let decisions =
            std::fs::read_to_string(state_dir.path().join(JEV_DECISIONS_FILE)).expect("rows");
        for line in decisions.lines() {
            let row: serde_json::Value = serde_json::from_str(line).expect("json");
            assert_eq!(row["v"], jev_counters::VERSION);
        }
    }

    #[test]
    fn a_caller_side_exit_is_counted_with_its_reason_and_an_off_gate_is_not() {
        let state_dir = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_path(state_dir.path().to_path_buf());
        let mut cfg = CtxConfig::default();
        cfg.proxy.typesafe.credential_env = "JEV_TEST_KEY_EXIT_REASON".to_string();

        assert!(!gate_open(&cfg, &state, "review", false));
        exit(&state, "review", false, "open_findings");
        assert!(!state_dir.path().join("jev-counters").exists());

        assert!(!gate_open(&cfg, &state, "review", true));
        exit(&state, "review", true, "open_findings");
        let counts = jev_counters::rollup(&state, state::now_secs());
        let review = &counts["review"];
        assert_eq!(review.reached, 2);
        assert_eq!(review.exits["no_credential"], 1);
        assert_eq!(review.exits["open_findings"], 1);
        let skips =
            std::fs::read_to_string(state_dir.path().join(JEV_SKIPS_FILE)).expect("skip row");
        let row: serde_json::Value =
            serde_json::from_str(skips.lines().next().expect("row")).expect("json");
        assert_eq!(
            (row["site"].as_str(), row["v"].as_str()),
            (Some("review"), Some(jev_counters::VERSION))
        );
    }

    #[test]
    fn status_lists_an_on_gate_with_zero_reach_as_dormant_and_warns_on_a_foreign_version() {
        let state_dir = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_path(state_dir.path().to_path_buf());
        let mut cfg = CtxConfig::default();
        cfg.jev.review = true;
        cfg.proxy.typesafe.credential_env = "JEV_TEST_KEY_STATUS_DORMANT".to_string();
        std::fs::write(
            state_dir.path().join(JEV_DECISIONS_FILE),
            format!(
                "{{\"site\":\"x\",\"ts\":{},\"v\":\"0.0.1\"}}\n",
                state::now_secs()
            ),
        )
        .expect("old row");
        let mut out = Vec::new();
        status(&cfg, &state, &mut out).expect("status");
        let text = String::from_utf8(out).expect("utf8");
        assert!(text.contains("jev.review"), "{text}");
        assert!(
            text.lines()
                .any(|line| line.starts_with("jev.review") && line.contains("dormant")),
            "{text}"
        );
        assert!(
            text.contains("idle: waits on `zirv workflow review run`"),
            "{text}"
        );
        assert!(text.contains("jev.memory       off retired"), "{text}");
        assert!(text.contains("written by zirv 0.0.1"), "{text}");
    }

    #[test]
    fn a_source_tree_at_another_version_than_the_binary_is_warned_about() {
        let state_dir = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_path(state_dir.path().to_path_buf());
        let tree = tempfile::tempdir().expect("tempdir");
        std::fs::write(
            tree.path().join("Cargo.toml"),
            "[package]\nname = \"zirv\"\nversion = \"9.9.9\"\n",
        )
        .expect("manifest");
        let warnings = version_warnings(&state, state::now_secs(), Some(tree.path()));
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert!(warnings[0].contains("9.9.9"), "{warnings:?}");
        assert!(version_warnings(&state, state::now_secs(), None).is_empty());
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
        multi_shot_server(status, body, 1)
    }

    /// Serves exactly `calls` requests; callers disable the Jev answer cache
    /// when they need a separate response for each decision cycle.
    pub(crate) fn multi_shot_server(
        status: u16,
        body: &'static str,
        calls: usize,
    ) -> (String, std::thread::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let handle = std::thread::spawn(move || {
            for _ in 0..calls {
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
            }
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
    fn unsafe_text_and_mutated_question_never_read_cache_or_reach_http() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        listener.set_nonblocking(true).expect("nonblocking");
        let url = format!("http://{}", listener.local_addr().expect("address"));
        let dir = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_path(dir.path().to_path_buf());
        let unsafe_state = serde_json::json!({
            "_zirv_metadata_only": true,
            "facts": [["sensitive task text /private/secret.key"]]
        });
        let questions = sample_questions();
        let key = cache_key_for(&unsafe_state, &questions, "jev-latest").expect("cache key");
        let cache_dir = dir.path().join(JEV_CACHE_DIR);
        std::fs::create_dir(&cache_dir).expect("cache dir");
        let entry = CacheEntry {
            answers: Answers::new(),
            usage: Usage {
                input_tokens: 1,
                output_tokens: 0,
            },
            stored_at: state::now_secs(),
            model: "jev-latest".into(),
        };
        std::fs::write(
            cache_dir.join(format!("{key}.json")),
            serde_json::to_vec(&entry).expect("entry"),
        )
        .expect("cache file");

        with_credential("JEV_TEST_PRIVACY_GUARD_746", "secret", || {
            let cfg = config(url, "JEV_TEST_PRIVACY_GUARD_746", 1);
            assert!(matches!(
                ask(&cfg, dir.path(), 86_400, &unsafe_state, &questions),
                Err(JevError::UnsafeState)
            ));
            let mut mutated = sample_questions();
            mutated[0].instructions = "send /private/secret.key".into();
            assert!(matches!(
                ask(&cfg, dir.path(), 86_400, &sample_state(), &mutated),
                Err(JevError::UnsafeState)
            ));
            let mut full_cfg = CtxConfig::default();
            full_cfg.proxy.typesafe = cfg;
            assert!(matches!(
                advise_detailed(
                    &full_cfg,
                    &state,
                    "privacy",
                    true,
                    &unsafe_state,
                    &questions
                ),
                AdvisoryStatus::Failed
            ));
        });
        assert!(
            matches!(listener.accept(), Err(error) if error.kind() == std::io::ErrorKind::WouldBlock)
        );
        assert!(!dir.path().join(JEV_DECISIONS_FILE).exists());
        assert!(!dir.path().join("spend.jsonl").exists());
    }

    #[test]
    fn a_cached_answer_still_requires_the_credential() {
        let dir = tempfile::tempdir().expect("tempdir");
        let cfg = config(
            "http://127.0.0.1:9".into(),
            "JEV_TEST_NEVER_SET_CACHE_CREDENTIAL_746",
            1,
        );
        let state = sample_state();
        let questions = sample_questions();
        let key = cache_key_for(&state, &questions, &cfg.model).expect("key");
        let cache_dir = dir.path().join(JEV_CACHE_DIR);
        std::fs::create_dir(&cache_dir).expect("cache dir");
        let entry = CacheEntry {
            answers: Answers::new(),
            usage: Usage {
                input_tokens: 1,
                output_tokens: 0,
            },
            stored_at: state::now_secs(),
            model: cfg.model.clone(),
        };
        std::fs::write(
            cache_dir.join(format!("{key}.json")),
            serde_json::to_vec(&entry).expect("entry"),
        )
        .expect("cache file");

        assert!(matches!(
            ask(&cfg, dir.path(), 86_400, &state, &questions),
            Err(JevError::NoCredential(_))
        ));
    }

    #[test]
    fn a_200_response_converts_to_answers() {
        let text = std::fs::read_to_string(fixture("proxy/jev-response.json")).expect("fixture");
        let body: &'static str = Box::leak(text.into_boxed_str());
        let (url, handle) = one_shot_server(200, body);
        let state_dir = tempfile::tempdir().expect("tempdir");
        with_credential("JEV_TEST_KEY_OK", "secret", || {
            let cfg = config(url, "JEV_TEST_KEY_OK", 5);
            let questions = vec![Question::metadata_choice(
                "category",
                "Select category from coarse facts.",
                &[("technical", "technical")],
            )];
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
                let error = ask(
                    &cfg,
                    state_dir.path(),
                    0,
                    &sample_state(),
                    &sample_questions(),
                )
                .expect_err("must fail");
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
            let error = ask(
                &cfg,
                state_dir.path(),
                0,
                &sample_state(),
                &sample_questions(),
            )
            .expect_err("must time out");
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
        let error = ask(
            &cfg,
            state_dir.path(),
            0,
            &sample_state(),
            &sample_questions(),
        )
        .expect_err("must refuse");
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
    fn near_tie_score_returns_the_higher_of_the_two_most_probable_levels() {
        for (value, bounded, substantial) in [(1.0, 0.57, 0.43), (2.0, 0.43, 0.57)] {
            let answer = Answer {
                value: AnswerValue::Score(value),
                confidence: 0.57,
                probabilities: BTreeMap::from([
                    ("0".to_string(), 0.0_f32),
                    ("1".to_string(), bounded),
                    ("2".to_string(), substantial),
                    ("3".to_string(), 0.0_f32),
                ]),
            };
            assert_eq!(answer.near_tie_score(0.5), Some(2.0));
        }
    }

    #[test]
    fn near_tie_score_breaks_probability_ties_upward() {
        for probabilities in [
            BTreeMap::from([("1".to_string(), 0.5_f32), ("2".to_string(), 0.5_f32)]),
            BTreeMap::from([
                ("0".to_string(), 0.6_f32),
                ("1".to_string(), 0.2_f32),
                ("2".to_string(), 0.2_f32),
            ]),
        ] {
            let answer = Answer {
                value: AnswerValue::Score(1.0),
                confidence: 0.5,
                probabilities,
            };
            assert_eq!(answer.near_tie_score(0.5), Some(2.0));
        }
    }

    #[test]
    fn near_tie_score_is_none_with_fewer_than_two_parseable_indices() {
        for probabilities in [
            BTreeMap::new(),
            BTreeMap::from([("1".to_string(), 1.0_f32)]),
            BTreeMap::from([("1".to_string(), 0.5_f32), ("other".to_string(), 0.5_f32)]),
        ] {
            let answer = Answer {
                value: AnswerValue::Score(1.0),
                confidence: 0.5,
                probabilities,
            };
            assert_eq!(answer.near_tie_score(0.5), None);
        }
    }

    /// A confidence below the floor is the model having no opinion, not a
    /// near-tie between two candidate levels: there is nothing to resolve
    /// upward, and the deterministic baseline is the better signal.
    #[test]
    fn near_tie_score_is_none_below_the_confidence_floor() {
        let answer = Answer {
            value: AnswerValue::Score(1.0),
            confidence: 0.45,
            probabilities: BTreeMap::from([
                ("0".to_string(), 0.1_f32),
                ("1".to_string(), 0.45_f32),
                ("2".to_string(), 0.4_f32),
                ("3".to_string(), 0.05_f32),
            ]),
        };
        assert_eq!(answer.near_tie_score(0.5), None);
        assert_eq!(answer.near_tie_score(0.45), Some(2.0));
    }

    #[test]
    fn near_tie_score_is_none_for_choice_and_noul() {
        for value in [AnswerValue::Choice("1".to_string()), AnswerValue::Noul(0.5)] {
            let answer = Answer {
                value,
                confidence: 0.5,
                probabilities: BTreeMap::from([
                    ("1".to_string(), 0.5_f32),
                    ("2".to_string(), 0.5_f32),
                ]),
            };
            assert_eq!(answer.near_tie_score(0.5), None);
        }
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

    // -- issue #803: jev::floor -----------------------------------------

    #[test]
    fn floor_unset_returns_exactly_the_callers_own_defaults() {
        let cfg = CtxConfig::default();
        assert_eq!(
            floor(&cfg, FloorSite::Context, 0.0, DEFAULT_MIN_MARGIN),
            (0.0, DEFAULT_MIN_MARGIN)
        );
        assert_eq!(
            floor(&cfg, FloorSite::Dispatch, 0.6, DEFAULT_MIN_MARGIN),
            (0.6, DEFAULT_MIN_MARGIN)
        );
    }

    #[test]
    fn floor_configured_overrides_only_the_set_field() {
        let mut cfg = CtxConfig::default();
        cfg.jev.floors.dispatch.min_confidence = Some(0.9);
        assert_eq!(
            floor(&cfg, FloorSite::Dispatch, 0.6, DEFAULT_MIN_MARGIN),
            (0.9, DEFAULT_MIN_MARGIN),
            "min_margin must still fall back to the caller's own default"
        );
        cfg.jev.floors.dispatch.min_margin = Some(0.35);
        assert_eq!(
            floor(&cfg, FloorSite::Dispatch, 0.6, DEFAULT_MIN_MARGIN),
            (0.9, 0.35)
        );
        // A different site's floor is untouched.
        assert_eq!(
            floor(&cfg, FloorSite::Context, 0.0, DEFAULT_MIN_MARGIN),
            (0.0, DEFAULT_MIN_MARGIN)
        );
    }

    #[test]
    fn credential_env_name_returns_the_configured_variable_name() {
        let cfg = config("http://127.0.0.1:0".to_string(), "CUSTOM_ENV_VAR", 5);
        let ctx_cfg = {
            let mut ctx = CtxConfig::default();
            ctx.proxy.typesafe = cfg;
            ctx
        };
        assert_eq!(credential_env_name(&ctx_cfg), "CUSTOM_ENV_VAR");
    }

    #[test]
    fn credential_env_name_defaults_to_typesafe_api_key() {
        let ctx_cfg = CtxConfig::default();
        assert_eq!(credential_env_name(&ctx_cfg), "TYPESAFE_API_KEY");
    }

    #[test]
    fn credential_present_returns_true_when_env_is_set_and_nonempty() {
        let ctx_cfg = {
            let mut ctx = CtxConfig::default();
            ctx.proxy.typesafe.credential_env = "JEV_TEST_CRED_PRESENT".to_string();
            ctx
        };
        with_credential("JEV_TEST_CRED_PRESENT", "secret", || {
            assert!(credential_present(&ctx_cfg));
        });
    }

    #[test]
    fn credential_present_returns_false_when_env_is_unset() {
        let ctx_cfg = {
            let mut ctx = CtxConfig::default();
            ctx.proxy.typesafe.credential_env = "JEV_TEST_CRED_UNSET_537".to_string();
            ctx
        };
        // SAFETY (test-only): unique env var name
        unsafe {
            std::env::remove_var(&ctx_cfg.proxy.typesafe.credential_env);
        }
        assert!(!credential_present(&ctx_cfg));
    }

    #[test]
    fn credential_present_returns_false_when_env_is_empty() {
        let ctx_cfg = {
            let mut ctx = CtxConfig::default();
            ctx.proxy.typesafe.credential_env = "JEV_TEST_CRED_EMPTY_537".to_string();
            ctx
        };
        with_credential("JEV_TEST_CRED_EMPTY_537", "", || {
            assert!(!credential_present(&ctx_cfg));
        });
    }

    #[test]
    fn credential_never_appears_in_status_output() {
        let state_dir = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_path(state_dir.path().to_path_buf());
        let mut ctx_cfg = CtxConfig::default();
        ctx_cfg.proxy.typesafe.credential_env = "JEV_TEST_CRED_SENTINEL_537".to_string();

        with_credential(
            "JEV_TEST_CRED_SENTINEL_537",
            "sentinel-do-not-print-this-value",
            || {
                let mut output = Vec::new();
                let _ = status(&ctx_cfg, &state, &mut output);
                let output_str = String::from_utf8_lossy(&output);
                assert!(
                    !output_str.contains("sentinel-do-not-print-this-value"),
                    "credential value must never appear in output: {output_str}"
                );
                assert!(
                    !output_str.contains("sentinel"),
                    "credential value prefix must never appear in output: {output_str}"
                );
            },
        );
    }

    #[test]
    fn status_with_all_gates_off_names_both_reasons() {
        let state_dir = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_path(state_dir.path().to_path_buf());
        let mut ctx_cfg = CtxConfig::default();
        ctx_cfg.proxy.typesafe.credential_env = "JEV_TEST_STATUS_BOTH_OFF".to_string();

        unsafe {
            std::env::remove_var(&ctx_cfg.proxy.typesafe.credential_env);
        }

        let mut output = Vec::new();
        let _ = status(&ctx_cfg, &state, &mut output);
        let output_str = String::from_utf8_lossy(&output);
        assert!(
            output_str.contains("inactive"),
            "status should name the condition: {output_str}"
        );
        assert!(
            output_str.contains("no gate enabled"),
            "status should mention gates are off: {output_str}"
        );
    }

    #[test]
    fn status_with_gate_on_but_credential_missing_names_credential() {
        let state_dir = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_path(state_dir.path().to_path_buf());
        let mut ctx_cfg = CtxConfig::default();
        ctx_cfg.proxy.typesafe.credential_env = "JEV_TEST_STATUS_CRED_MISSING".to_string();
        ctx_cfg.jev.review = true;

        unsafe {
            std::env::remove_var(&ctx_cfg.proxy.typesafe.credential_env);
        }

        let mut output = Vec::new();
        let _ = status(&ctx_cfg, &state, &mut output);
        let output_str = String::from_utf8_lossy(&output);
        assert!(
            output_str.contains("JEV_TEST_STATUS_CRED_MISSING"),
            "status should name the credential env var: {output_str}"
        );
        assert!(
            output_str.contains("not set") || output_str.contains("missing"),
            "status should indicate credential is missing: {output_str}"
        );
    }

    #[test]
    fn status_with_gate_on_and_credential_present_is_active() {
        let state_dir = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_path(state_dir.path().to_path_buf());
        let mut ctx_cfg = CtxConfig::default();
        ctx_cfg.proxy.typesafe.credential_env = "JEV_TEST_STATUS_ACTIVE".to_string();
        ctx_cfg.jev.review = true;

        with_credential("JEV_TEST_STATUS_ACTIVE", "secret", || {
            let mut output = Vec::new();
            let _ = status(&ctx_cfg, &state, &mut output);
            let output_str = String::from_utf8_lossy(&output);
            assert!(
                output_str.contains("active"),
                "status should indicate active when gate is on and credential is present: {output_str}"
            );
        });
    }

    /// A cache hit never reached the model, so its near-zero wall time must not drag the
    /// latency percentiles of the calls that did.
    #[test]
    fn usage_rollup_latency_percentiles_exclude_cached_rows() {
        let dir = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_path(dir.path().to_path_buf());
        state::create_private_dir_all(state.root()).expect("create state dir");
        let now = state::now_secs();
        let row = |wall_ms: u64, cached: bool| {
            format!(
                "{{\"site\":\"memory\",\"ts\":{now},\"wall_ms\":{wall_ms},\"cached\":{cached},\"fallbacks\":[]}}\n"
            )
        };
        let decisions = [
            row(0, true),
            row(0, true),
            row(300, false),
            row(400, false),
            row(500, false),
        ]
        .concat();
        std::fs::write(state.root().join("jev-decisions.jsonl"), decisions)
            .expect("write decisions");

        let rollup = usage_rollup(&state, ROLLUP_WINDOW_SECS, None);
        let usage = rollup.sites.get("memory").expect("memory site present");
        assert_eq!(usage.calls, 5);
        assert_eq!(usage.wall_ms_p50, Some(400));
    }

    /// Issue #758: [`usage_rollup`] folds both logs, keyed by their shared
    /// `site` field, over the rollup window.
    #[test]
    fn usage_rollup_folds_decisions_and_effects_per_site() {
        let dir = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_path(dir.path().to_path_buf());
        state::create_private_dir_all(state.root()).expect("create state dir");
        let now = state::now_secs();

        let decisions = format!(
            "{{\"site\":\"memory\",\"ts\":{now},\"wall_ms\":100,\"cached\":false,\"fallbacks\":[]}}\n\
             {{\"site\":\"memory\",\"ts\":{now},\"wall_ms\":200,\"cached\":true,\"fallbacks\":[]}}\n\
             {{\"site\":\"memory\",\"ts\":{now},\"wall_ms\":300,\"cached\":false,\"fallbacks\":[\"boom\"]}}\n"
        );
        std::fs::write(state.root().join("jev-decisions.jsonl"), decisions)
            .expect("write decisions");

        let effects = format!(
            "{{\"ts\":{now},\"site\":\"memory\",\"action\":\"candidates_pruned\",\"removed_bytes\":500}}\n\
             {{\"ts\":{now},\"site\":\"memory\",\"action\":\"candidates_pruned\",\"removed_bytes\":250}}\n"
        );
        std::fs::write(state.root().join("jev-effects.jsonl"), effects).expect("write effects");

        let rollup = usage_rollup(&state, ROLLUP_WINDOW_SECS, None);
        let usage = rollup.sites.get("memory").expect("memory site present");
        assert_eq!(usage.calls, 3);
        assert_eq!(
            usage.errors, 1,
            "one row carried a non-empty fallbacks list"
        );
        assert_eq!(usage.effect_rows, 2);
        assert_eq!(usage.removed_bytes, 750);
        assert_eq!(
            usage.wall_ms_p50,
            Some(300),
            "the cached 200 ms row is excluded"
        );
        assert_eq!(usage.wall_ms_p95, Some(300));
        let cache_hit_rate = usage.cache_hit_rate.expect("cache hit rate present");
        assert!(
            (cache_hit_rate - (1.0 / 3.0)).abs() < 1e-9,
            "expected 1/3 cache hit rate, got {cache_hit_rate}"
        );
        assert_eq!(
            rollup.last_call,
            Some(JevLastCall {
                site: "memory".to_string(),
                ts: now
            }),
            "the last (highest-ts, latest-in-file) decision row wins"
        );
        assert_eq!(
            rollup.latest_error_reason,
            Some("boom".to_string()),
            "the one row with a fallback names it as the latest error"
        );
    }

    /// Dash refresh PR2: `last_call`/`latest_error_reason` are keyed across
    /// EVERY site, not per-site -- the most recent row overall wins even
    /// when it belongs to a different site than the busiest one, and the
    /// latest ERROR is tracked independently of the latest call.
    #[test]
    fn usage_rollup_last_call_and_latest_error_span_every_site() {
        let dir = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_path(dir.path().to_path_buf());
        state::create_private_dir_all(state.root()).expect("create state dir");
        let now = state::now_secs();

        let decisions = format!(
            "{{\"site\":\"memory\",\"ts\":{t0},\"wall_ms\":10,\"cached\":false,\"fallbacks\":[\"rate limited\"]}}\n\
             {{\"site\":\"approve\",\"ts\":{t1},\"wall_ms\":10,\"cached\":false,\"fallbacks\":[]}}\n",
            t0 = now - 20,
            t1 = now - 5,
        );
        std::fs::write(state.root().join("jev-decisions.jsonl"), decisions)
            .expect("write decisions");

        let rollup = usage_rollup(&state, ROLLUP_WINDOW_SECS, None);
        assert_eq!(
            rollup.last_call,
            Some(JevLastCall {
                site: "approve".to_string(),
                ts: now - 5,
            }),
            "approve's row is the most recent even though memory's row errored"
        );
        assert_eq!(
            rollup.latest_error_reason,
            Some("rate limited".to_string()),
            "memory's older row is still the only (and therefore latest) error"
        );
    }

    /// Click affordance follow-up: [`JevRollup::recent_errors`] keeps every
    /// errored row newest first, bounded to [`MAX_KEPT_JEV_ERRORS`] -- out of
    /// order in the file (never guaranteed given no cross-process locking)
    /// and past the cap both still resolve to the right newest-first,
    /// truncated list.
    #[test]
    fn usage_rollup_recent_errors_are_bounded_and_newest_first() {
        let dir = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_path(dir.path().to_path_buf());
        state::create_private_dir_all(state.root()).expect("create state dir");
        let now = state::now_secs();

        let mut lines = Vec::new();
        // One well-formed row per age, written newest-last in the file
        // (append order) except the very first line, which is deliberately
        // the OLDEST of the batch -- out-of-order writes are expected, not
        // exceptional (see `record`'s own doc comment), so the sort must not
        // just trust file order.
        for age in (0..(MAX_KEPT_JEV_ERRORS + 5) as u64).rev() {
            lines.push(format!(
                "{{\"site\":\"memory\",\"ts\":{ts},\"wall_ms\":10,\"cached\":false,\"fallbacks\":[\"err{age}\"]}}",
                ts = now - age
            ));
        }
        lines.swap(0, 1);
        std::fs::write(state.root().join("jev-decisions.jsonl"), lines.join("\n"))
            .expect("write decisions");

        let rollup = usage_rollup(&state, ROLLUP_WINDOW_SECS, None);
        assert_eq!(
            rollup.recent_errors.len(),
            MAX_KEPT_JEV_ERRORS,
            "the list is capped even though more errors fell in the window"
        );
        assert_eq!(
            rollup.recent_errors.first().map(|e| e.reason.as_str()),
            Some("err0"),
            "the newest error (age 0) leads the list"
        );
        assert!(
            rollup
                .recent_errors
                .windows(2)
                .all(|pair| pair[0].ts >= pair[1].ts),
            "the list is newest first throughout: {:?}",
            rollup.recent_errors
        );
        assert_eq!(
            rollup.latest_error_reason,
            Some("err0".to_string()),
            "derived from the newest-first list's own head"
        );
    }

    /// Session-scoped dash follow-up: a `Some(set)` filter counts only rows
    /// whose own `session` field is in that set -- the orchestrator's row
    /// and its worker's row both count, an unrelated session's row (present
    /// in the very same machine-wide file) does not.
    #[test]
    fn usage_rollup_filtered_to_a_session_set_counts_only_those_sessions() {
        let dir = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_path(dir.path().to_path_buf());
        state::create_private_dir_all(state.root()).expect("create state dir");
        let now = state::now_secs();

        let decisions = format!(
            "{{\"site\":\"memory\",\"ts\":{now},\"wall_ms\":10,\"cached\":false,\"fallbacks\":[],\"session\":\"orch\"}}\n\
             {{\"site\":\"memory\",\"ts\":{now},\"wall_ms\":10,\"cached\":false,\"fallbacks\":[],\"session\":\"worker\"}}\n\
             {{\"site\":\"memory\",\"ts\":{now},\"wall_ms\":10,\"cached\":false,\"fallbacks\":[],\"session\":\"unrelated-other-repo\"}}\n"
        );
        std::fs::write(state.root().join("jev-decisions.jsonl"), decisions)
            .expect("write decisions");

        let sessions: BTreeSet<String> = ["orch".to_string(), "worker".to_string()].into();
        let scoped = usage_rollup(&state, ROLLUP_WINDOW_SECS, Some(&sessions));
        assert_eq!(
            scoped.sites.get("memory").map(|u| u.calls),
            Some(2),
            "only orch's and worker's own rows count"
        );

        let unscoped = usage_rollup(&state, ROLLUP_WINDOW_SECS, None);
        assert_eq!(
            unscoped.sites.get("memory").map(|u| u.calls),
            Some(3),
            "`zirv ctx jev status`'s own all-sessions view is unchanged"
        );
    }

    /// Session-scoped dash follow-up: a row written before `session` existed
    /// (`DecisionRollupRow::session`'s own `#[serde(default)]`) must still
    /// parse -- and, filtered to any real session set, drop out silently
    /// rather than either failing to parse or (worse) counting toward every
    /// session because an empty string matched something.
    #[test]
    fn usage_rollup_a_row_with_no_session_field_parses_and_is_excluded_from_a_session_view() {
        let dir = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_path(dir.path().to_path_buf());
        state::create_private_dir_all(state.root()).expect("create state dir");
        let now = state::now_secs();

        let decisions = format!(
            "{{\"site\":\"memory\",\"ts\":{now},\"wall_ms\":10,\"cached\":false,\"fallbacks\":[]}}\n"
        );
        std::fs::write(state.root().join("jev-decisions.jsonl"), decisions)
            .expect("write decisions");

        let unscoped = usage_rollup(&state, ROLLUP_WINDOW_SECS, None);
        assert_eq!(
            unscoped.sites.get("memory").map(|u| u.calls),
            Some(1),
            "an old, session-less row still parses and counts in the all-sessions view"
        );

        let sessions: BTreeSet<String> = ["orch".to_string()].into();
        let scoped = usage_rollup(&state, ROLLUP_WINDOW_SECS, Some(&sessions));
        assert!(
            scoped.sites.is_empty(),
            "the same row is silently excluded from a session-scoped view: {scoped:?}"
        );
    }

    /// A gated call that keeps skipping for the same reason writes one row an hour, not one per call.
    #[test]
    fn record_skip_dedups_a_repeated_site_and_reason_within_the_hour() {
        let dir = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_path(dir.path().to_path_buf());
        let rows = || {
            std::fs::read_to_string(state.root().join(JEV_SKIPS_FILE))
                .unwrap_or_default()
                .lines()
                .count()
        };
        record_skip(&state, "memory", "no key");
        record_skip(&state, "memory", "no key");
        assert_eq!(rows(), 1, "a repeat inside the hour adds nothing");
        record_skip(&state, "memory", "other reason");
        assert_eq!(rows(), 2, "a different reason is its own row");

        let old = state::now_secs() - SKIP_DEDUP_SECS - 1;
        std::fs::write(
            state.root().join(JEV_SKIPS_FILE),
            format!("{{\"site\":\"memory\",\"ts\":{old},\"reason\":\"no key\"}}\n"),
        )
        .expect("write");
        record_skip(&state, "memory", "no key");
        assert_eq!(rows(), 2, "a repeat after the hour adds a second row");
    }

    /// Issue #758: a state dir with neither log file must roll up to empty,
    /// never an error -- `zirv ctx jev status` is read-only diagnostics.
    #[test]
    fn usage_rollup_with_no_log_files_is_empty() {
        let dir = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_path(dir.path().to_path_buf());
        let rollup = usage_rollup(&state, ROLLUP_WINDOW_SECS, None);
        assert!(
            rollup.sites.is_empty(),
            "no log files should yield an empty rollup: {rollup:?}"
        );
        assert_eq!(rollup.last_call, None);
        assert_eq!(rollup.latest_error_reason, None);
        assert!(rollup.recent_errors.is_empty());
    }

    /// Issue #758: both logs are appended by several call sites with no
    /// cross-process locking, so a torn/corrupt line is expected. It must be
    /// skipped, not abort the fold or drop the well-formed rows around it.
    #[test]
    fn usage_rollup_skips_a_corrupt_line() {
        let dir = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_path(dir.path().to_path_buf());
        state::create_private_dir_all(state.root()).expect("create state dir");
        let now = state::now_secs();
        let decisions = format!(
            "not json at all\n{{\"site\":\"dispatch\",\"ts\":{now},\"wall_ms\":50,\"cached\":false,\"fallbacks\":[]}}\n"
        );
        std::fs::write(state.root().join("jev-decisions.jsonl"), decisions)
            .expect("write decisions");

        let rollup = usage_rollup(&state, ROLLUP_WINDOW_SECS, None);
        let usage = rollup
            .sites
            .get("dispatch")
            .expect("dispatch site present despite the corrupt line above it");
        assert_eq!(usage.calls, 1);
    }

    /// Dash refresh PR2: the window is a parameter now -- a row just outside
    /// a NARROW window (the dashboard's 24h JEV section) must not count,
    /// even though the same row is well inside the wide 7-day window `zirv
    /// ctx jev status` keeps.
    #[test]
    fn usage_rollup_window_is_a_parameter() {
        let dir = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_path(dir.path().to_path_buf());
        state::create_private_dir_all(state.root()).expect("create state dir");
        let now = state::now_secs();
        let two_days_secs = 2 * 24 * 60 * 60;
        let decisions = format!(
            "{{\"site\":\"memory\",\"ts\":{ts},\"wall_ms\":10,\"cached\":false,\"fallbacks\":[]}}\n",
            ts = now.saturating_sub(two_days_secs)
        );
        std::fs::write(state.root().join("jev-decisions.jsonl"), decisions)
            .expect("write decisions");

        let one_day_secs = 24 * 60 * 60;
        let narrow = usage_rollup(&state, one_day_secs, None);
        assert!(
            narrow.sites.is_empty(),
            "a 2-day-old row must not count in a 1-day window: {narrow:?}"
        );

        let wide = usage_rollup(&state, ROLLUP_WINDOW_SECS, None);
        assert_eq!(
            wide.sites.get("memory").map(|u| u.calls),
            Some(1),
            "the same row counts in the wider 7-day window"
        );
    }

    /// Issue #758: the `--json` payload carries a `usage` object with the
    /// window and the per-site rollup, alongside the existing gates/
    /// credential/endpoint/verdict shape.
    #[test]
    fn status_json_includes_a_usage_section_with_the_rollup() {
        let mut ctx_cfg = CtxConfig::default();
        ctx_cfg.proxy.typesafe.credential_env = "JEV_TEST_STATUS_JSON_USAGE".to_string();
        ctx_cfg.jev.review = true;

        let mut rollup = BTreeMap::new();
        rollup.insert(
            "memory".to_string(),
            JevSiteUsage {
                calls: 3,
                cache_hit_rate: Some(1.0 / 3.0),
                wall_ms_p50: Some(200),
                wall_ms_p95: Some(300),
                errors: 1,
                effect_rows: 2,
                removed_bytes: 750,
            },
        );

        let value = status_json(&ctx_cfg, &rollup);
        assert_eq!(value["usage"]["window_days"], 7);
        assert_eq!(value["usage"]["sites"]["memory"]["calls"], 3);
        assert_eq!(value["usage"]["sites"]["memory"]["errors"], 1);
        assert_eq!(value["usage"]["sites"]["memory"]["effect_rows"], 2);
        assert_eq!(value["usage"]["sites"]["memory"]["removed_bytes"], 750);
        assert_eq!(value["usage"]["sites"]["memory"]["wall_ms_p50"], 200);
        assert_eq!(value["usage"]["sites"]["memory"]["wall_ms_p95"], 300);
        assert!(value["gates"]["review"].as_bool().unwrap());
    }

    /// #452: an enabled site that never asks (no credential, or a state the metadata guard
    /// refuses) leaves a skip row the rollup reports as an error, without counting a call.
    #[test]
    fn a_skipped_advisory_leaves_a_row_the_status_rollup_reports() {
        let dir = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_path(dir.path().to_path_buf());
        let mut cfg = CtxConfig::default();
        cfg.proxy.typesafe.credential_env = "JEV_TEST_SKIP_ROW_452".into();
        unsafe { std::env::remove_var("JEV_TEST_SKIP_ROW_452") };
        let missing = advise_detailed(
            &cfg,
            &state,
            "context",
            true,
            &sample_state(),
            &sample_questions(),
        );
        assert!(matches!(missing, AdvisoryStatus::MissingCredential));
        with_credential("JEV_TEST_SKIP_ROW_452", "secret", || {
            let unsafe_state = advise_detailed(
                &cfg,
                &state,
                "review",
                true,
                &legacy_state(),
                &sample_questions(),
            );
            assert!(matches!(unsafe_state, AdvisoryStatus::Failed));
        });

        let rollup = usage_rollup(&state, ROLLUP_WINDOW_SECS, None);
        let context = rollup.sites.get("context").expect("context skip counted");
        assert_eq!((context.calls, context.errors), (0, 1));
        assert_eq!(rollup.sites.get("review").map(|s| s.errors), Some(1));
        let reasons: Vec<&str> = rollup
            .recent_errors
            .iter()
            .map(|e| e.reason.as_str())
            .collect();
        assert!(
            reasons.contains(&"credential env JEV_TEST_SKIP_ROW_452 unset"),
            "{reasons:?}"
        );
        assert!(
            reasons.contains(&"unsafe Jev metadata projection"),
            "{reasons:?}"
        );
        assert!(!dir.path().join(JEV_DECISIONS_FILE).exists());
    }

    /// #452: status lists every operator gate, not 17 of the 20.
    #[test]
    fn status_lists_every_jev_gate() {
        let state_dir = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_path(state_dir.path().to_path_buf());
        let cfg = CtxConfig::default();
        let mut output = Vec::new();
        status(&cfg, &state, &mut output).expect("status");
        let text = String::from_utf8_lossy(&output);
        let gates = status_json(&cfg, &BTreeMap::new())["gates"].clone();
        for key in [
            "compaction_select",
            "missing_tests",
            "launch_effort",
            "retry",
        ] {
            assert!(text.contains(&format!("jev.{key}")), "{key}: {text}");
            assert_eq!(gates[key], false, "{key}: {gates}");
        }
        assert_eq!(gates.as_object().map(|g| g.len()), Some(20), "{gates}");
    }

    /// #452: `launch_effort` alone is a real gate, so it must start the relay a sandboxed seat needs.
    #[test]
    fn launch_effort_alone_counts_as_an_enabled_gate() {
        let jev = JevConfig {
            launch_effort: true,
            ..JevConfig::default()
        };
        assert!(any_gate_enabled(&jev));
    }

    /// #452: the deprecated `memory` key is ignored everywhere, so it must not keep a relay alive.
    #[test]
    fn the_deprecated_memory_key_alone_enables_no_gate() {
        let jev = JevConfig {
            memory: true,
            ..JevConfig::default()
        };
        assert!(!any_gate_enabled(&jev));
    }

    /// #827: intake rows written under a launch's pre-minted session count for the runtime
    /// session it became once an alias exists, and for nobody before.
    #[test]
    fn usage_rollup_counts_rows_under_an_aliased_session_only_once_aliased() {
        let dir = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_path(dir.path().to_path_buf());
        state::create_private_dir_all(state.root()).expect("create state dir");
        let now = state::now_secs();
        std::fs::write(
            state.root().join("jev-decisions.jsonl"),
            format!(
                "{{\"site\":\"intake\",\"ts\":{now},\"wall_ms\":1,\"cached\":false,\"fallbacks\":[],\"session\":\"minted\"}}\n"
            ),
        )
        .expect("write decisions");
        let runtime: BTreeSet<String> = ["runtime".to_string()].into();
        let calls = |state: &StateDir| {
            usage_rollup(state, ROLLUP_WINDOW_SECS, Some(&runtime))
                .sites
                .get("intake")
                .map_or(0, |usage| usage.calls)
        };

        assert_eq!(
            calls(&state),
            0,
            "no alias: the minted session is not the runtime's"
        );
        record_session_alias(&state, "minted", "runtime");
        assert_eq!(calls(&state), 1);
    }

    /// #827: the alias is written only when the runtime chose a different id.
    #[test]
    fn session_alias_is_written_only_when_the_ids_differ() {
        let dir = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_path(dir.path().to_path_buf());
        record_session_alias(&state, "same", "same");
        assert!(!dir.path().join("jev-session-aliases.jsonl").exists());
        record_session_alias(&state, "minted", "runtime");
        let text = std::fs::read_to_string(dir.path().join("jev-session-aliases.jsonl"))
            .expect("alias row");
        assert_eq!(text.lines().count(), 1);
        assert!(text.contains("\"from\":\"minted\"") && text.contains("\"to\":\"runtime\""));
    }
}
