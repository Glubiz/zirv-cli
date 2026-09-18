//! The harness proxy's first-choice decider (issue #537 seam): a thin
//! wrapper over the shared Jev client at `jev.rs` (issue #537 seam
//! extraction, task A1), kept as its own module purely so `proxy::mod`'s
//! existing `typesafe::decide` call site needs no change. The wire
//! contract, request/response shapes, and every real code path now live in
//! `jev::ask`; see that module's own doc comment for the documented shape.

pub use crate::commands::ctx::jev::JevError as TypesafeError;

use super::decision::{Answers, IntakeState, Question, Usage};
use crate::commands::ctx::config::ProxyTypesafeConfig;
use crate::commands::ctx::jev;

/// Runs one bounded `/systemone` call via [`jev::ask`] and converts its
/// answers into the neutral [`Answers`] shape.
pub fn decide(
    cfg: &ProxyTypesafeConfig,
    intake: &IntakeState,
    questions: &[Question],
) -> Result<(Answers, Usage), TypesafeError> {
    jev::ask(cfg, intake, questions)
}
