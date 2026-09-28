//! Stop-hook verification-claim guard: flags a turn that claims
//! completion or a passing test run without fresh evidence.

use std::io::Read;
use std::path::Path;
#[cfg(test)]
use std::path::PathBuf;

use super::pretool_tier::{DispatchAdviseState, capped_u32, count_keyword_class};
use crate::commands::ctx::adapters::{self};
use crate::commands::ctx::config::CtxConfig;
use crate::commands::ctx::event::NormalizedEvent;
use crate::commands::ctx::state::StateDir;
use crate::commands::workflow::adoption::{self};

/// Issue #786: minimum `true` probability before `stop_verify` blocks a Stop.
/// Basis: jev-belay reached 1% false blocks with the closing TEXT; facts-only
/// state is untested, so the floor sits well above the default margin.
const STOP_VERIFY_MIN_PROBABILITY: f64 = 0.9;

/// [`stop_verify_reason`]'s own default `(min_confidence, min_margin)`
/// `decisive()` floor -- named (issue: `zirv ctx jev probe`) so a later
/// retune targets exactly this constant, the same way every other tunable
/// site's default floor is now named. Not routed through `jev::floor`/
/// `[jev.floors]` today: this stays the same fixed pair production has
/// always used.
pub(crate) const STOP_VERIFY_DEFAULT_FLOOR: (f32, f32) =
    (0.0, crate::commands::ctx::jev::DEFAULT_MIN_MARGIN);

/// How much of the transcript tail `stop_verify` parses for the closing turn.
pub(super) const STOP_VERIFY_TAIL_BYTES: u64 = 512 * 1024;

const STOP_VERIFY_REASON: &str = "zirv: this turn edited files and presents the work as finished, \
but nothing verified it since. Run the relevant tests or checks, then finish.";

/// Completion claims in a closing message, matched lowercase, locally only.
const COMPLETION_CLAIM_PHRASES: [&str; 10] = [
    "done",
    "complete",
    "implemented",
    "fixed",
    "finished",
    "resolved",
    "all set",
    "ready to",
    "now works",
    "is now",
];

/// "Tests pass"-style verification claims.
const TEST_PASS_CLAIM_PHRASES: [&str; 8] = [
    "tests pass",
    "all tests",
    "all green",
    "passing",
    "passes",
    "verified",
    "build succeeds",
    "compiles",
];

/// Hedges that make a message NOT a finished-work claim.
const HEDGE_PHRASES: [&str; 12] = [
    "should ",
    "might",
    "may ",
    "probably",
    "likely",
    "i think",
    "not sure",
    "untested",
    "not tested",
    "haven't",
    "have not",
    "unverified",
];

/// Local facts for the closing turn (events after the last human prompt):
/// `[completion claims, hedges, test-pass claims, question marks, length
/// bucket, edit calls, shell calls]`. `None` when the turn edited nothing or
/// the closing message claims nothing -- neither can be a false "done".
fn stop_verify_facts(events: &[NormalizedEvent]) -> Option<Vec<u32>> {
    let start = events
        .iter()
        .rposition(|event| matches!(event, NormalizedEvent::TurnStart { .. }))
        .map_or(0, |index| index + 1);
    let turn = &events[start..];
    let edits = adoption::signals(turn).edit_like_calls;
    if edits == 0 {
        return None;
    }
    let shell_calls = turn
        .iter()
        .filter(|event| {
            matches!(event, NormalizedEvent::ToolCall { name, .. }
                if ["bash", "powershell", "shell", "exec_command", "local_shell"]
                    .iter()
                    .any(|shell| name.eq_ignore_ascii_case(shell)))
        })
        .count();
    let closing = turn.iter().rev().find_map(|event| match event {
        NormalizedEvent::AssistantFinal { text, .. } if !text.trim().is_empty() => Some(text),
        _ => None,
    })?;
    let lower = closing.to_lowercase();
    let completion = count_keyword_class(&lower, &COMPLETION_CLAIM_PHRASES);
    let test_claims = count_keyword_class(&lower, &TEST_PASS_CLAIM_PHRASES);
    if completion == 0 && test_claims == 0 {
        return None;
    }
    let length_bucket = match closing.len() {
        0..200 => 0,
        200..1000 => 1,
        1000..4000 => 2,
        _ => 3,
    };
    Some(vec![
        completion,
        count_keyword_class(&lower, &HEDGE_PHRASES),
        test_claims,
        capped_u32(closing.matches('?').count()),
        length_bucket,
        capped_u32(edits),
        capped_u32(shell_calls),
    ])
}

pub(crate) fn stop_verify_questions() -> [crate::commands::ctx::jev::Question; 1] {
    [crate::commands::ctx::jev::Question::metadata_noul(
        "unverified_done",
        "Facts [completion-claim phrases, hedge phrases, tests-pass-style claims, question marks, \
length bucket (0 <200B, 1 <1KB, 2 <4KB, 3 larger), files-edit calls this turn, shell commands this \
turn] describe an agent's closing message after a turn that edited files with no passing check \
since. Does it present unverified work as finished? Answer false if unsure.",
        "presents unverified work as finished",
        "hedged, partial, or insufficient evidence",
    )]
}

/// [`stop_verify_reason`]'s own per-call decision: `"block"` only for a
/// DECISIVE noul at or above [`STOP_VERIFY_MIN_PROBABILITY`], `"allow"`
/// otherwise (missing answer, indecisive, unparseable, or a decisive answer
/// below the probability floor) -- the Stop hook's own fallback outcome
/// (proceed as if `stop_verify` never ran). Shared with `zirv ctx jev
/// probe`, which reports exactly this outcome.
pub(crate) fn stop_verify_action(
    answer: Option<&crate::commands::ctx::jev::Answer>,
    min_confidence: f32,
    min_margin: f32,
) -> &'static str {
    let Some(answer) = answer else {
        return "allow";
    };
    if !answer.decisive(min_confidence, min_margin) {
        return "allow";
    }
    match answer.as_noul() {
        Some(probability) if probability >= STOP_VERIFY_MIN_PROBABILITY => "block",
        _ => "allow",
    }
}

/// Issue #786 (`[jev] stop_verify`, facts-only stage): only when a check is
/// owed (`verify_owed`, the verify-on-stop signal) and this turn edited files,
/// asks one Noul from local counts of the closing message. A decisive `true`
/// returns the block reason; anything else -- gate off, no credential, no
/// claim, error, indecisive -- is `None` and the Stop proceeds as today.
/// `stop_hook_active` is handled by `run_stop`'s own early return, so this
/// can never block twice in a row.
pub(super) fn stop_verify_reason(
    state: &StateDir,
    cfg: &CtxConfig,
    verify_owed: bool,
    transcript: &Path,
) -> Option<&'static str> {
    if !verify_owed
        || !cfg.jev.stop_verify
        || !crate::commands::ctx::jev::available(&cfg.proxy.typesafe)
    {
        return None;
    }
    let adapter = adapters::select_for_identity(cfg.agent.as_deref(), &[], cfg).ok()?;
    let mut file = std::fs::File::open(transcript).ok()?;
    let start = file
        .metadata()
        .ok()?
        .len()
        .saturating_sub(STOP_VERIFY_TAIL_BYTES);
    std::io::Seek::seek(&mut file, std::io::SeekFrom::Start(start)).ok()?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes).ok()?;
    let text = String::from_utf8_lossy(&bytes);
    let lines = match start {
        0 => &text[..],
        _ => text.split_once('\n').map_or("", |(_, rest)| rest),
    };
    let facts = stop_verify_facts(&adapter.parse_events(lines))?;
    let advise_state = DispatchAdviseState {
        metadata_only: true,
        facts: vec![facts],
    };
    let answers = crate::commands::ctx::jev::advise(
        cfg,
        state,
        "stop_verify",
        cfg.jev.stop_verify,
        &advise_state,
        &stop_verify_questions(),
    )?;
    let answer = answers.get("unverified_done");
    let (min_confidence, min_margin) = STOP_VERIFY_DEFAULT_FLOOR;
    if stop_verify_action(answer, min_confidence, min_margin) != "block" {
        return None;
    }
    let effect = crate::commands::ctx::jev::JevEffect::new("stop_verify", "stop_blocked");
    crate::commands::ctx::jev::record_effect(cfg, state, cfg.jev.stop_verify, &effect);
    Some(STOP_VERIFY_REASON)
}

#[cfg(test)]
mod tests {
    use super::super::pretool_tier::DispatchAdviseState;
    use super::super::stop::with_stop_block;
    use super::super::tests::transcript_with_edits;
    use super::*;

    // -- issue #786: `[jev] stop_verify` --------------------------------

    fn claiming_transcript(dir: &Path, closing: &str) -> PathBuf {
        let path = dir.join("stop-verify.jsonl");
        let text = format!(
            "{{\"type\":\"user\",\"message\":{{\"content\":\"go\"}}}}\n\
             {{\"type\":\"assistant\",\"message\":{{\"content\":[{{\"type\":\"tool_use\",\"id\":\"t\",\"name\":\"Edit\",\"input\":{{}}}}],\"usage\":{{\"input_tokens\":100}}}}}}\n\
             {{\"type\":\"assistant\",\"message\":{{\"content\":[{{\"type\":\"text\",\"text\":\"{closing}\"}}],\"usage\":{{\"input_tokens\":100}}}}}}\n"
        );
        std::fs::write(&path, text).expect("write transcript");
        path
    }

    fn stop_verify_cfg(base_url: String, credential_env: &str) -> CtxConfig {
        let mut cfg = CtxConfig::default();
        cfg.jev.stop_verify = true;
        cfg.jev.cache_ttl_secs = 0;
        cfg.proxy.typesafe.base_url = base_url;
        cfg.proxy.typesafe.credential_env = credential_env.to_string();
        cfg.proxy.typesafe.timeout_secs = 5;
        cfg
    }

    const CLAIM: &str = "Done: implemented the fix and all tests pass.";
    const UNVERIFIED_DONE: &str = r#"{"model": "jev-latest", "answers": {
        "unverified_done": {"type": "noul", "noul": 0.97}},
        "usage": {"input_tokens": 5, "output_tokens": 0}}"#;

    #[test]
    fn stop_verify_facts_count_claims_only_for_an_editing_turn() {
        let dir = tempfile::tempdir().expect("tempdir");
        let adapter =
            adapters::select_for_identity(None, &[], &CtxConfig::default()).expect("adapter");
        let events = |closing: &str| {
            let path = claiming_transcript(dir.path(), closing);
            adapter.parse_events(&std::fs::read_to_string(path).expect("read"))
        };
        let facts = stop_verify_facts(&events(CLAIM)).expect("an editing, claiming turn");
        assert_eq!(facts.len(), 7);
        assert!(facts[0] >= 2 && facts[2] >= 1, "{facts:?}");
        assert_eq!(facts[5], 1, "one edit call this turn");
        assert_eq!(
            stop_verify_facts(&events("Here is what I changed")),
            None,
            "a message that claims nothing is never asked about"
        );
        let read_only = transcript_with_edits(dir.path(), 1, 0);
        let read_only = adapter.parse_events(&std::fs::read_to_string(read_only).expect("read"));
        assert_eq!(stop_verify_facts(&read_only), None);
    }

    #[test]
    fn stop_verify_key_off_never_asks() {
        let dir = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(dir.path().join("state"));
        let transcript = claiming_transcript(dir.path(), CLAIM);
        let env = "HOOK_TEST_STOP_VERIFY_OFF";
        // SAFETY (test-only): a unique env var name this test owns.
        unsafe { std::env::set_var(env, "secret") };
        let mut cfg = stop_verify_cfg("http://127.0.0.1:9".to_string(), env);
        cfg.jev.stop_verify = false;
        let reason = stop_verify_reason(&state, &cfg, true, &transcript);
        cfg.jev.stop_verify = true;
        let not_owed = stop_verify_reason(&state, &cfg, false, &transcript);
        unsafe { std::env::remove_var(env) };
        assert_eq!(reason, None);
        assert_eq!(not_owed, None, "no owed check means no call at all");
        assert!(!state.root().join("jev-decisions.jsonl").exists());
    }

    #[test]
    fn stop_verify_blocks_on_a_decisive_answer_and_keeps_the_advisory() {
        let (url, handle) = crate::commands::ctx::jev::tests::one_shot_server(200, UNVERIFIED_DONE);
        let dir = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(dir.path().join("state"));
        let transcript = claiming_transcript(dir.path(), CLAIM);
        let env = "HOOK_TEST_STOP_VERIFY_BLOCK";
        // SAFETY (test-only): a unique env var name this test owns.
        unsafe { std::env::set_var(env, "secret") };
        let cfg = stop_verify_cfg(url, env);
        let reason = stop_verify_reason(&state, &cfg, true, &transcript);
        unsafe { std::env::remove_var(env) };
        handle.join().expect("server thread");
        let reason = reason.expect("a decisive answer blocks");
        let merged: serde_json::Value = serde_json::from_str(&with_stop_block(
            Some(r#"{"systemMessage":"zirv ctx: advisory"}"#),
            reason,
        ))
        .expect("json");
        assert_eq!(merged["decision"], "block");
        assert_eq!(merged["systemMessage"], "zirv ctx: advisory");
        let effects = std::fs::read_to_string(state.root().join("jev-effects.jsonl"))
            .expect("effect recorded");
        assert!(effects.contains("stop_blocked"), "{effects}");
    }

    /// Direction: an answer below the floor never blocks.
    #[test]
    fn stop_verify_never_blocks_below_the_floor() {
        let body = r#"{"model": "jev-latest", "answers": {
            "unverified_done": {"type": "noul", "noul": 0.8}},
            "usage": {"input_tokens": 5, "output_tokens": 0}}"#;
        let (url, handle) = crate::commands::ctx::jev::tests::one_shot_server(200, body);
        let dir = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(dir.path().join("state"));
        let transcript = claiming_transcript(dir.path(), CLAIM);
        let env = "HOOK_TEST_STOP_VERIFY_FLOOR";
        // SAFETY (test-only): a unique env var name this test owns.
        unsafe { std::env::set_var(env, "secret") };
        let reason = stop_verify_reason(&state, &stop_verify_cfg(url, env), true, &transcript);
        unsafe { std::env::remove_var(env) };
        handle.join().expect("server thread");
        assert_eq!(reason, None);
    }

    #[test]
    fn stop_verify_falls_back_on_a_500() {
        let (url, handle) = crate::commands::ctx::jev::tests::one_shot_server(500, "{}");
        let dir = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(dir.path().join("state"));
        let transcript = claiming_transcript(dir.path(), CLAIM);
        let env = "HOOK_TEST_STOP_VERIFY_500";
        // SAFETY (test-only): a unique env var name this test owns.
        unsafe { std::env::set_var(env, "secret") };
        let reason = stop_verify_reason(&state, &stop_verify_cfg(url, env), true, &transcript);
        unsafe { std::env::remove_var(env) };
        handle.join().expect("server thread");
        assert_eq!(reason, None);
        assert!(!state.root().join("jev-effects.jsonl").exists());
    }

    #[test]
    fn stop_verify_request_passes_the_metadata_guard() {
        let advise_state = DispatchAdviseState {
            metadata_only: true,
            facts: vec![vec![1_000_000; 7]],
        };
        let value = serde_json::to_value(&advise_state).expect("json");
        assert!(crate::commands::ctx::jev::safe_metadata_request(
            &value,
            &stop_verify_questions(),
            "jev-latest"
        ));
    }

    fn stop_verify_noul_answer(probability: f64) -> crate::commands::ctx::jev::Answer {
        crate::commands::ctx::jev::Answer {
            value: crate::commands::ctx::jev::AnswerValue::Noul(probability),
            confidence: probability as f32,
            probabilities: std::collections::BTreeMap::new(),
        }
    }

    /// A decisive answer at or above [`STOP_VERIFY_MIN_PROBABILITY`] blocks;
    /// the same answer one step below the value threshold, and a missing
    /// answer, both fall back to "allow" -- proves the `>=` edge, not just a
    /// comfortably-clear case.
    #[test]
    fn stop_verify_action_decides_on_the_probability_edge() {
        let (min_confidence, min_margin) = STOP_VERIFY_DEFAULT_FLOOR;
        let at_floor = stop_verify_noul_answer(STOP_VERIFY_MIN_PROBABILITY);
        assert_eq!(
            stop_verify_action(Some(&at_floor), min_confidence, min_margin),
            "block"
        );
        let just_below = stop_verify_noul_answer(STOP_VERIFY_MIN_PROBABILITY - 0.01);
        assert_eq!(
            stop_verify_action(Some(&just_below), min_confidence, min_margin),
            "allow"
        );
        assert_eq!(
            stop_verify_action(None, min_confidence, min_margin),
            "allow"
        );
    }
}
