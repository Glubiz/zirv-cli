//! The harness proxy's second-choice decider (issue #537 seam): renders the
//! same neutral [`Question`]s as `typesafe.rs`'s Jev call into a JSON
//! contract prompt, and answers through the existing helper-model
//! chokepoint (`handoff::helper_answer`, issue #484) instead of a fresh HTTP
//! client. One repair prompt on a parse failure, then `Err` -- this decider
//! never retries beyond that, matching the "no retries" posture the whole
//! proxy chain holds to.

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use serde::Deserialize;

use super::decision::{Answer, AnswerValue, Answers, Criteria, Question, QuestionKind};
use crate::commands::ctx::adapters::AgentAdapter;
use crate::commands::ctx::handoff;

#[derive(Debug, Deserialize)]
struct ContractAnswer {
    #[serde(default)]
    probabilities: BTreeMap<String, f64>,
}

#[derive(Debug, Deserialize)]
struct Contract {
    answers: BTreeMap<String, ContractAnswer>,
}

fn render_prompt(questions: &[Question]) -> String {
    let mut prompt = String::from(
        "You are answering intake questions for a coding-task router. For EACH question below, \
         give a probability distribution over its listed options (choice questions), over its \
         ordered levels by index (score questions), or over {\"true\", \"false\"} (noul \
         questions). Answer with EXACTLY one JSON object and nothing else, no prose, no markdown \
         fences, in this shape:\n\n\
         {\"answers\": {\"<id>\": {\"probabilities\": {\"<option-or-index-or-true/false>\": \
         <0..1>, ...}}, ...}}\n\n\
         Probabilities for one question should sum to approximately 1. Every question id below \
         must appear as a key in \"answers\".\n\n",
    );
    for question in questions {
        prompt.push_str(&format!("### {}\n{}\n", question.id, question.instructions));
        match &question.criteria {
            Criteria::Choice(options) => {
                for (option, description) in options {
                    match description {
                        Some(text) => prompt.push_str(&format!("- {option}: {text}\n")),
                        None => prompt.push_str(&format!("- {option}\n")),
                    }
                }
            }
            Criteria::Score(levels) => {
                for (index, text) in levels.iter().enumerate() {
                    prompt.push_str(&format!("- {index}: {text}\n"));
                }
            }
            Criteria::Noul {
                when_true,
                when_false,
            } => {
                if let Some(text) = when_true {
                    prompt.push_str(&format!("- true: {text}\n"));
                }
                if let Some(text) = when_false {
                    prompt.push_str(&format!("- false: {text}\n"));
                }
            }
        }
        prompt.push('\n');
    }
    prompt
}

fn repair_prompt(original_answer: &str) -> String {
    format!(
        "Your previous answer was not valid JSON matching the requested contract:\n\n{original_answer}\n\n\
         Reply again with ONLY the JSON object `{{\"answers\": {{\"<id>\": {{\"probabilities\": \
         {{...}}}}, ...}}}}` -- no prose, no markdown fences, nothing else."
    )
}

fn parse_contract(text: &str) -> Option<Contract> {
    let trimmed = text.trim();
    let start = trimmed.find('{')?;
    let end = trimmed.rfind('}')?;
    if end < start {
        return None;
    }
    serde_json::from_str(&trimmed[start..=end]).ok()
}

/// Converts one question's answer probabilities into the neutral [`Answer`]
/// shape: `confidence` is always the winning probability, matching the
/// contract's own instruction. `Score` derives a continuous level index as
/// the probability-weighted average of the numbered levels; `Noul` reads
/// the `true` probability (falling back to the winning probability when the
/// model answered with a different key shape).
fn to_answer(question: &Question, probabilities: &BTreeMap<String, f64>) -> Option<Answer> {
    let (top_key, top_probability) = probabilities
        .iter()
        .max_by(|a, b| a.1.partial_cmp(b.1).unwrap_or(std::cmp::Ordering::Equal))?;
    let confidence = *top_probability as f32;
    let value = match question.kind {
        QuestionKind::Choice => AnswerValue::Choice(top_key.clone()),
        QuestionKind::Score => {
            let weighted: f64 = probabilities
                .iter()
                .filter_map(|(key, probability)| {
                    key.parse::<f64>().ok().map(|idx| idx * probability)
                })
                .sum();
            AnswerValue::Score(weighted)
        }
        QuestionKind::Noul => {
            let true_probability = probabilities
                .get("true")
                .copied()
                .unwrap_or(*top_probability);
            AnswerValue::Noul(true_probability)
        }
    };
    Some(Answer { value, confidence })
}

fn to_answers(questions: &[Question], contract: &Contract) -> Answers {
    let mut out = Answers::new();
    for question in questions {
        if let Some(raw) = contract.answers.get(&question.id)
            && let Some(answer) = to_answer(question, &raw.probabilities)
        {
            out.insert(question.id.clone(), answer);
        }
    }
    out
}

/// Below this, a repair call is not worth attempting at all -- there is no
/// plausible answer in under a second, and skipping it here is what keeps
/// [`decide`]'s total wall time bounded by `timeout` overall rather than by
/// `timeout` per call.
const MIN_REPAIR_BUDGET: Duration = Duration::from_secs(1);

/// The repair call's own timeout given the FULL `total` budget `decide` was
/// given and how much of it `elapsed` during the first call: `total -
/// elapsed`, or `None` when that leaves less than [`MIN_REPAIR_BUDGET`].
/// Pure (no clock read) so the arithmetic is unit-testable on its own.
fn repair_timeout(total: Duration, elapsed: Duration) -> Option<Duration> {
    let remaining = total.saturating_sub(elapsed);
    if remaining < MIN_REPAIR_BUDGET {
        None
    } else {
        Some(remaining)
    }
}

/// Renders `questions` as a JSON contract prompt, answers through
/// `handoff::helper_answer(role, adapter, model, prompt, timeout)`, parses
/// the contract, and repairs once on a parse failure before giving up.
/// `timeout` is the OVERALL budget for both calls together, not each: the
/// repair call gets whatever remains of it, and is skipped (returning `Err`
/// immediately) once less than [`MIN_REPAIR_BUDGET`] is left, so this
/// function's total wall time is bounded by `timeout` rather than by up to
/// `2 * timeout`.
pub fn decide(
    role: &str,
    adapter: &dyn AgentAdapter,
    model: &str,
    questions: &[Question],
    timeout: Duration,
) -> Result<Answers, String> {
    let started = Instant::now();
    let prompt = render_prompt(questions);
    let first = handoff::helper_answer(role, adapter, model, &prompt, timeout)
        .map_err(|error| format!("helper call failed: {error}"))?;
    if let Some(contract) = parse_contract(&first) {
        return Ok(to_answers(questions, &contract));
    }

    let Some(remaining) = repair_timeout(timeout, started.elapsed()) else {
        return Err(
            "helper answer did not parse as the JSON contract; no budget left for a repair \
             attempt"
                .to_string(),
        );
    };

    let repair = repair_prompt(&first);
    let second = handoff::helper_answer(role, adapter, model, &repair, remaining)
        .map_err(|error| format!("helper repair call failed: {error}"))?;
    match parse_contract(&second) {
        Some(contract) => Ok(to_answers(questions, &contract)),
        None => {
            Err("helper answer did not parse as the JSON contract after one repair".to_string())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::ctx::adapters::claude::ClaudeAdapter;

    const TEST_TIMEOUT: Duration = Duration::from_secs(20);

    fn fixture(name: &str) -> std::path::PathBuf {
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests")
            .join("fixtures")
            .join(name)
    }

    fn fake_model_adapter() -> ClaudeAdapter {
        ClaudeAdapter::new(Some(&format!("sh {}", fixture("fake-model.sh").display())))
    }

    fn sample_questions() -> Vec<Question> {
        vec![
            Question {
                id: "intent".to_string(),
                kind: QuestionKind::Choice,
                instructions: "pick one".to_string(),
                criteria: Criteria::Choice(vec![
                    ("feature".to_string(), Some("adds behavior".to_string())),
                    ("bugfix".to_string(), Some("fixes a defect".to_string())),
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

    fn with_mode<T>(mode: &str, body: impl FnOnce() -> T) -> T {
        // SAFETY (test-only): nextest runs each test in its own process.
        unsafe {
            std::env::set_var("FAKE_MODEL_MODE", mode);
        }
        let result = body();
        unsafe {
            std::env::remove_var("FAKE_MODEL_MODE");
        }
        result
    }

    #[test]
    fn a_well_formed_contract_parses_on_the_first_try() {
        with_mode("proxy", || {
            let adapter = fake_model_adapter();
            let answers = decide(
                "proxy",
                &adapter,
                "haiku",
                &sample_questions(),
                TEST_TIMEOUT,
            )
            .expect("decide");
            assert!(answers.contains_key("intent"));
            assert!(answers.contains_key("complexity"));
            assert!(answers.contains_key("needs_clarification"));
        });
    }

    #[test]
    fn garbage_both_times_is_an_error_after_one_repair() {
        with_mode("proxy_garbage", || {
            let adapter = fake_model_adapter();
            let error = decide(
                "proxy",
                &adapter,
                "haiku",
                &sample_questions(),
                TEST_TIMEOUT,
            )
            .expect_err("must fail");
            assert!(error.contains("repair"), "{error}");
        });
    }

    #[test]
    fn parse_contract_extracts_the_json_object_from_surrounding_prose() {
        let text = "here you go:\n{\"answers\": {\"intent\": {\"probabilities\": {\"feature\": 0.9}}}}\nthanks";
        let contract = parse_contract(text).expect("parse");
        assert!(contract.answers.contains_key("intent"));
    }

    #[test]
    fn parse_contract_rejects_non_json_prose() {
        assert!(parse_contract("I had a look and things seem mostly fine.").is_none());
    }

    #[test]
    fn repair_timeout_returns_the_remaining_budget() {
        assert_eq!(
            repair_timeout(Duration::from_secs(10), Duration::from_secs(3)),
            Some(Duration::from_secs(7))
        );
    }

    #[test]
    fn repair_timeout_boundary_is_exactly_the_minimum_budget() {
        assert_eq!(
            repair_timeout(Duration::from_secs(10), Duration::from_secs(9)),
            Some(MIN_REPAIR_BUDGET)
        );
    }

    #[test]
    fn repair_timeout_is_none_below_the_minimum_budget_or_past_the_deadline() {
        assert_eq!(
            repair_timeout(Duration::from_secs(10), Duration::from_millis(9500)),
            None
        );
        assert_eq!(
            repair_timeout(Duration::from_secs(10), Duration::from_secs(10)),
            None
        );
        assert_eq!(
            repair_timeout(Duration::from_secs(10), Duration::from_secs(20)),
            None,
            "elapsed beyond the total budget must not underflow"
        );
    }
}
