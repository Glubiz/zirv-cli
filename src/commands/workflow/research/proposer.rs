//! The optional `[proposer]` round: prompt construction (candidate-space
//! schema + dev-split aggregates only, never a validation/holdout task id),
//! the production spawn argv, and validating whatever it proposes exactly
//! like a declared candidate -- split out of `run.rs`. No behaviour change
//! from the code that used to live here.

use std::collections::BTreeMap;
use std::path::Path;
use std::process::Command;

use serde::Deserialize;

use super::backend;
use super::guard;
use super::ledger::{Ledger, LedgerEvent};
use super::manifest::{self, Candidate, Manifest};
use super::run::{CandidateRuntime, TrialRecord};
use super::schedule::strategy_json;
use crate::commands::ctx::CtxResult;
use crate::commands::ctx::state::{create_private_dir_all, now_secs};

#[derive(Debug, Deserialize)]
struct ProposedCandidateJson {
    id: String,
    hypothesis: String,
    #[serde(default)]
    mechanism: Option<String>,
    #[serde(default)]
    env: BTreeMap<String, String>,
    #[serde(default)]
    patch: Option<String>,
}

fn candidate_space_schema_text(manifest: &Manifest) -> String {
    let keys: Vec<String> = manifest
        .candidate_space
        .allow_env
        .iter()
        .map(|key| match guard::classify_env_key(key) {
            Ok(kind) => format!("{key} ({kind:?})"),
            Err(_) => format!("{key} (refused)"),
        })
        .collect();
    format!(
        "allowed env keys: [{}]\nallowed models: {:?}",
        keys.join(", "),
        manifest.candidate_space.allowed_models
    )
}

/// Dev-split baseline aggregate metrics only -- no per-task breakdown, so
/// this alone cannot leak which specific dev task drove a number.
pub(crate) fn dev_aggregate_summary(records: &[TrialRecord]) -> String {
    let n = records.len();
    if n == 0 {
        return "no dev-split baseline trials yet".to_string();
    }
    let ok = records
        .iter()
        .filter(|r| r.status == backend::TrialStatus::Ok)
        .count();
    let correctness_vals: Vec<f64> = records.iter().filter_map(|r| r.correctness).collect();
    let correctness_mean = if correctness_vals.is_empty() {
        0.0
    } else {
        correctness_vals.iter().sum::<f64>() / correctness_vals.len() as f64
    };
    let cost_vals: Vec<f64> = records.iter().filter_map(|r| r.cost_usd).collect();
    let cost_mean = if cost_vals.is_empty() {
        0.0
    } else {
        cost_vals.iter().sum::<f64>() / cost_vals.len() as f64
    };
    let wall_mean = records.iter().map(|r| r.wall_ms).sum::<u64>() as f64 / n as f64;
    format!(
        "n={n} success_rate={:.3} correctness_mean={correctness_mean:.3} cost_mean_usd={cost_mean:.4} wall_mean_ms={wall_mean:.0}",
        ok as f64 / n as f64
    )
}

/// The proposer's ENTIRE prompt: the candidate-space schema and dev-split
/// aggregate metrics only -- deliberately never a per-task result, a
/// validation/holdout task id, or any other campaign detail, so a proposer
/// round can never see the evidence its own proposal will later be judged
/// against.
pub(crate) fn proposer_prompt(manifest: &Manifest, dev_summary: &str) -> String {
    format!(
        "Propose ONE bounded environment-overlay candidate for campaign '{}'.\n\n\
         Candidate space:\n{}\n\n\
         Dev-split baseline aggregate metrics (no per-task detail):\n{}\n\n\
         Respond with exactly one JSON object on its own line, as the LAST line of output:\n\
         {{\"id\": \"...\", \"hypothesis\": \"...\", \"mechanism\": \"...\", \"env\": {{...}}}}\n\
         A proposal may not set a \"patch\" field or any key outside the candidate space above.",
        manifest.id,
        candidate_space_schema_text(manifest),
        dev_summary,
    )
}

/// The production spawn argv: `current_exe agent <harness> "<prompt>" --
/// --model <model>` (unchanged from the design). Tests substitute an
/// entirely different argv (e.g. a stub `python -c "print(...)"`) directly
/// into `spawn_and_validate_proposal` instead of calling this.
fn production_proposer_argv(harness: &str, model: &str, prompt: &str) -> Vec<String> {
    let exe = std::env::current_exe()
        .map(|path| path.to_string_lossy().into_owned())
        .unwrap_or_else(|_| "zirv".to_string());
    vec![
        exe,
        "agent".to_string(),
        harness.to_string(),
        prompt.to_string(),
        "--".to_string(),
        "--model".to_string(),
        model.to_string(),
    ]
}

/// The last valid single-line JSON object among `stdout`'s trailing lines
/// (checked over at most the last 50 lines, most recent first) -- a
/// well-behaved proposer prints its answer as the final line of output.
fn parse_last_json_object(stdout: &str) -> Option<serde_json::Value> {
    stdout
        .lines()
        .rev()
        .take(50)
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .find_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
}

/// Spawns `argv` (production: [`production_proposer_argv`]; tests
/// substitute their own stub command) with `cwd`/`extra_env`, and validates
/// the parsed candidate exactly like a declared one: its env must be in
/// both `allow_env` and the compiled allowlist, and it may not carry a
/// `patch` (source-patch candidates are only ever operator-declared).
/// `Ok(None)` when the process ran but printed nothing parseable as a
/// candidate -- a proposer round proposing nothing is a normal outcome, not
/// an error.
pub(crate) fn spawn_and_validate_proposal(
    argv: &[String],
    cwd: &Path,
    extra_env: &[(String, String)],
    manifest: &Manifest,
) -> Result<Option<Candidate>, String> {
    let [program, args @ ..] = argv else {
        return Err("proposer argv is empty".to_string());
    };
    let mut command = Command::new(program);
    command.args(args);
    command.current_dir(cwd);
    command.stdin(std::process::Stdio::null());
    for (key, value) in extra_env {
        command.env(key, value);
    }
    let output = command
        .output()
        .map_err(|err| format!("could not run the proposer command: {err}"))?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    let Some(value) = parse_last_json_object(&stdout) else {
        return Ok(None);
    };
    let proposed: ProposedCandidateJson = serde_json::from_value(value)
        .map_err(|err| format!("proposer output did not parse as a candidate: {err}"))?;
    if proposed.patch.is_some() {
        return Err("a proposed candidate may not carry a patch".to_string());
    }
    if !manifest::is_valid_id(&proposed.id) {
        return Err(format!(
            "proposed candidate id '{}' must match [A-Za-z0-9._-]{{1,48}}",
            proposed.id
        ));
    }
    if proposed.id == "baseline" {
        return Err("a proposed candidate may not use the reserved id 'baseline'".to_string());
    }
    guard::validate_candidate_env(
        &proposed.env,
        &manifest.candidate_space.allow_env,
        &manifest.candidate_space.allowed_models,
    )?;
    Ok(Some(Candidate {
        id: proposed.id,
        hypothesis: proposed.hypothesis,
        mechanism: proposed.mechanism,
        env: proposed.env,
        patch: None,
        requires_receipts: Vec::new(),
        strategy: None,
    }))
}

/// One full proposer round: builds the prompt and production argv, gives
/// the proposer its own empty cwd and its own `<campaign>/proposer/<round>/
/// state` state dir (attributed `candidate = "proposer"`, so its own spend
/// counts as overhead, never a candidate's execution cost), then validates
/// whatever it proposed.
pub(crate) fn run_proposer_round(
    campaign_dir: &Path,
    manifest: &Manifest,
    proposer_cfg: &manifest::Proposer,
    dev_summary: &str,
    round: u32,
) -> Result<Option<Candidate>, String> {
    let prompt = proposer_prompt(manifest, dev_summary);
    let argv = production_proposer_argv(&proposer_cfg.harness, &proposer_cfg.model, &prompt);
    let round_dir = campaign_dir.join("proposer").join(round.to_string());
    let cwd = round_dir.join("cwd");
    let state_dir = round_dir.join("state");
    create_private_dir_all(&cwd)
        .map_err(|err| format!("could not create the proposer's own cwd: {err}"))?;
    let extra_env = vec![
        (
            "ZIRV_CTX_STATE_DIR".to_string(),
            state_dir.to_string_lossy().to_string(),
        ),
        ("ZIRV_ATTR_CAMPAIGN".to_string(), manifest.id.clone()),
        ("ZIRV_ATTR_CANDIDATE".to_string(), "proposer".to_string()),
    ];
    spawn_and_validate_proposal(&argv, &cwd, &extra_env, manifest)
}

/// Folds one proposer round's outcome into the campaign's live candidate
/// set: a validated proposal is appended to `all_candidates` and given a
/// `CandidateRuntime` in `candidates_map` -- from this point on it is
/// "scheduled" exactly like a declared candidate, screened and (if it
/// survives) validated in the very next loop -- plus a `candidate_proposed`
/// ledger event. `Ok(None)` (nothing parseable) is silently a no-op, same as
/// a declared candidate list simply not growing; a validation failure is
/// recorded as `candidate_rejected` under a synthetic `proposal-<round>` id
/// rather than aborting the campaign over one bad proposal.
pub(crate) fn apply_proposal_outcome(
    outcome: Result<Option<Candidate>, String>,
    round: u32,
    ledger: &mut Ledger,
    candidates_map: &mut BTreeMap<String, CandidateRuntime>,
    all_candidates: &mut Vec<Candidate>,
) -> CtxResult<()> {
    match outcome {
        Ok(Some(candidate)) => {
            // `candidates_map` already holds "baseline" and every declared
            // candidate before the first proposer round ever runs, and
            // accumulates each accepted proposal as rounds proceed -- so a
            // membership check here alone catches a proposal that would
            // overwrite the baseline runtime, or any other candidate
            // (declared or already-proposed) sharing its id, before it
            // clobbers that entry in the map below.
            if candidates_map.contains_key(&candidate.id) {
                let seq = ledger.next_seq();
                ledger.append(&LedgerEvent::CandidateRejected {
                    seq,
                    ts: now_secs(),
                    candidate: candidate.id.clone(),
                    reason: format!(
                        "proposed id '{}' collides with an existing candidate",
                        candidate.id
                    ),
                })?;
                return Ok(());
            }
            let seq = ledger.next_seq();
            ledger.append(&LedgerEvent::CandidateProposed {
                seq,
                ts: now_secs(),
                candidate: candidate.id.clone(),
                hypothesis: candidate.hypothesis.clone(),
            })?;
            candidates_map.insert(
                candidate.id.clone(),
                CandidateRuntime {
                    env: candidate.env.clone(),
                    zirv_dir: None,
                    strategy: candidate.strategy.as_ref().map(strategy_json),
                    patch_lines: 0,
                },
            );
            all_candidates.push(candidate);
        }
        Ok(None) => {}
        Err(reason) => {
            let seq = ledger.next_seq();
            ledger.append(&LedgerEvent::CandidateRejected {
                seq,
                ts: now_secs(),
                candidate: format!("proposal-{round}"),
                reason,
            })?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::production_proposer_argv;

    /// The production proposer spawn is a self-recursion into this very
    /// executable's own path (`std::env::current_exe()`, falling back to the
    /// bare `"zirv"` name if that ever fails): `zirv agent <harness>
    /// "<prompt>" -- --model <model>`, the same harness-invocation shape a
    /// declared candidate's own trial dispatch uses. Never actually spawned
    /// here -- that would need a real harness binary on PATH;
    /// `spawn_and_validate_proposal`'s own tests (in `run.rs`, alongside the
    /// rest of this module's execute()-level coverage) substitute a
    /// portable stub argv instead and cover the spawn+validate half of this
    /// seam.
    #[test]
    fn production_proposer_argv_shapes_a_self_recursive_agent_invocation() {
        let argv = production_proposer_argv("claude", "sonnet", "propose one candidate");
        assert!(
            !argv[0].is_empty(),
            "argv[0] must be this executable's own path (or the `zirv` fallback)"
        );
        assert_eq!(
            argv[1..],
            [
                "agent".to_string(),
                "claude".to_string(),
                "propose one candidate".to_string(),
                "--".to_string(),
                "--model".to_string(),
                "sonnet".to_string(),
            ]
        );
    }
}
