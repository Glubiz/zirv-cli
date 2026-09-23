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
/// answers into the neutral [`Answers`] shape. The `cached` half of `ask`'s
/// own result is dropped here: the harness proxy's own persistence
/// (`proxy::persist`, a different file from `jev::record`'s) does not yet
/// carry that flag, so a caller wanting it should call `jev::ask` directly.
pub fn decide(
    cfg: &ProxyTypesafeConfig,
    state_dir: &std::path::Path,
    cache_ttl_secs: u64,
    intake: &IntakeState,
    questions: &[Question],
) -> Result<(Answers, Usage), TypesafeError> {
    jev::ask(cfg, state_dir, cache_ttl_secs, intake, questions)
        .map(|(answers, usage, _cached)| (answers, usage))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::ctx::proxy::decision::{
        Criteria, IntakePolicy, IntakeRepository, QuestionKind,
    };

    fn sample_intake() -> IntakeState {
        IntakeState {
            request: "fix the typo".to_string(),
            repository: IntakeRepository {
                name: "repo".to_string(),
            },
            workflows: Vec::new(),
            policy: IntakePolicy {
                native_available: false,
            },
        }
    }

    fn sample_questions() -> Vec<Question> {
        vec![
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
        ]
    }

    /// The legacy freeform proxy state cannot cross the metadata-only Jev
    /// boundary, even with a credential. The gated intake path sends its own
    /// coarse facts through the shared client instead.
    #[test]
    fn decide_rejects_legacy_freeform_intake_before_network_or_cache() {
        // SAFETY (test-only): a unique env var name this test owns.
        unsafe {
            std::env::set_var("JEV_TEST_KEY_TYPESAFE_DECIDE", "secret");
        }
        let cfg = ProxyTypesafeConfig {
            base_url: "http://127.0.0.1:1".to_string(),
            credential_env: "JEV_TEST_KEY_TYPESAFE_DECIDE".to_string(),
            model: "jev-latest".to_string(),
            timeout_secs: 5,
        };
        let state_dir = tempfile::tempdir().expect("tempdir");

        let result = decide(
            &cfg,
            state_dir.path(),
            0,
            &sample_intake(),
            &sample_questions(),
        );

        // SAFETY (test-only): removing the same unique env var set above.
        unsafe {
            std::env::remove_var("JEV_TEST_KEY_TYPESAFE_DECIDE");
        }
        assert!(matches!(result, Err(TypesafeError::UnsafeState)));
        assert!(!state_dir.path().join("jev-cache").exists());
    }
}
