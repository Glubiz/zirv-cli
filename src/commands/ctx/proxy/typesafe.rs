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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::ctx::proxy::decision::{
        AnswerValue, BranchChanges, Criteria, IntakePolicy, IntakeRepository, QuestionKind,
    };

    fn fixture(name: &str) -> std::path::PathBuf {
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests")
            .join("fixtures")
            .join(name)
    }

    fn sample_intake() -> IntakeState {
        IntakeState {
            request: "fix the typo".to_string(),
            repository: IntakeRepository {
                name: "repo".to_string(),
                uncommitted_or_branch_changes: BranchChanges { files: 0, lines: 0 },
                active_workflow: None,
                primary_extensions: Vec::new(),
            },
            harnesses: Vec::new(),
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
        ]
    }

    /// Proves this module still wires up to [`jev::ask`]: a canned
    /// `/systemone` response (the shared fixture `jev.rs`'s own tests read)
    /// comes back through [`decide`] as the same neutral [`Answers`]/
    /// [`Usage`] shape `jev::ask` itself is proven to produce.
    #[test]
    fn decide_delegates_to_jev_ask_and_returns_its_answers() {
        let text = std::fs::read_to_string(fixture("proxy/jev-response.json")).expect("fixture");
        let body: &'static str = Box::leak(text.into_boxed_str());
        let (base_url, handle) = jev::tests::one_shot_server(200, body);
        // SAFETY (test-only): a unique env var name this test owns.
        unsafe {
            std::env::set_var("JEV_TEST_KEY_TYPESAFE_DECIDE", "secret");
        }
        let cfg = ProxyTypesafeConfig {
            base_url,
            credential_env: "JEV_TEST_KEY_TYPESAFE_DECIDE".to_string(),
            model: "jev-latest".to_string(),
            timeout_secs: 5,
        };

        let result = decide(&cfg, &sample_intake(), &sample_questions());

        // SAFETY (test-only): removing the same unique env var set above.
        unsafe {
            std::env::remove_var("JEV_TEST_KEY_TYPESAFE_DECIDE");
        }
        let (answers, usage) = result.expect("decide");
        handle.join().expect("server thread must not panic");

        assert_eq!(usage.input_tokens, 312);
        assert_eq!(usage.output_tokens, 48);
        match &answers["category"].value {
            AnswerValue::Choice(value) => assert_eq!(value, "technical"),
            other => panic!("expected a choice answer, got {other:?}"),
        }
        match &answers["urgency"].value {
            AnswerValue::Score(value) => assert_eq!(*value, 1.0),
            other => panic!("expected a score answer, got {other:?}"),
        }
        match &answers["needs_human"].value {
            AnswerValue::Noul(value) => assert_eq!(*value, 0.999),
            other => panic!("expected a noul answer, got {other:?}"),
        }
    }
}
