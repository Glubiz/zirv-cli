//! `zirv ctx proxy stats` (#537): per-task-class token cost and verified-pass rate, read-only, tokens only.
//!
//! Join: `proxy-profile/<short>.json` (the seat's final decision) -> seat transcript tokens
//! (`session_spend::resolve_transcript`) + `delegations.jsonl` rows whose `parent_session` is that seat.
//! Quality is the last verification run in the seat transcript; a class with none reports it unavailable.

use std::collections::BTreeMap;
use std::io::Write;

use clap::Args;
use serde::Serialize;

use super::store;
use crate::commands::ctx::event::VerificationStatus;
use crate::commands::ctx::state::StateDir;
use crate::commands::ctx::{CtxResult, adapters, config, log, session_spend};

/// Delegation rows read per run; the ledger is append-only and older rows add little.
const DELEGATION_ROW_CAP: usize = 100_000;

#[derive(Debug, Args)]
pub struct StatsArgs {
    /// Print the report as JSON.
    #[arg(long)]
    pub json: bool,
}

/// One seat's measured facts; `None` means the source did not exist, never zero.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SeatRow {
    pub class: String,
    pub seat_tokens: Option<[u64; 4]>,
    pub delegation_tokens: Option<u64>,
    pub delegations: Option<u64>,
    pub verification: Option<VerificationStatus>,
}

/// Why delegation figures are `None`: the ledger is a source, and an absent source is not zero.
const DELEGATIONS_UNAVAILABLE: &str = "unavailable: delegations.jsonl is missing or unreadable";

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ClassStats {
    pub class: String,
    pub seats: u64,
    pub seats_with_tokens: u64,
    pub median_input_tokens: Option<u64>,
    pub median_output_tokens: Option<u64>,
    pub median_cache_tokens: Option<u64>,
    pub delegation_tokens: Option<u64>,
    pub delegations: Option<u64>,
    /// The reason `delegation_tokens` and `delegations` are null, when they are.
    pub delegations_note: Option<String>,
    pub verified_seats: u64,
    pub verified_passed: u64,
    pub verified_pass_rate: Option<f64>,
    pub quality: String,
}

fn median(mut values: Vec<u64>) -> Option<u64> {
    if values.is_empty() {
        return None;
    }
    values.sort_unstable();
    Some(values[values.len() / 2])
}

/// Pure grouping: identical rows give an identical report.
pub fn compute(rows: &[SeatRow]) -> Vec<ClassStats> {
    let mut by_class: BTreeMap<&str, Vec<&SeatRow>> = BTreeMap::new();
    for row in rows {
        by_class.entry(&row.class).or_default().push(row);
    }
    by_class
        .into_iter()
        .map(|(class, seats)| {
            let tokens: Vec<[u64; 4]> = seats.iter().filter_map(|row| row.seat_tokens).collect();
            let verified: Vec<VerificationStatus> =
                seats.iter().filter_map(|row| row.verification).collect();
            let decided = verified
                .iter()
                .filter(|status| **status != VerificationStatus::Unknown)
                .count() as u64;
            let passed = verified
                .iter()
                .filter(|status| **status == VerificationStatus::Passed)
                .count() as u64;
            let rate = (decided > 0).then(|| passed as f64 / decided as f64);
            ClassStats {
                class: class.to_string(),
                seats: seats.len() as u64,
                seats_with_tokens: tokens.len() as u64,
                median_input_tokens: median(tokens.iter().map(|t| t[0]).collect()),
                median_output_tokens: median(tokens.iter().map(|t| t[3]).collect()),
                median_cache_tokens: median(
                    tokens.iter().map(|t| t[1].saturating_add(t[2])).collect(),
                ),
                delegation_tokens: seats.iter().map(|row| row.delegation_tokens).sum(),
                delegations: seats.iter().map(|row| row.delegations).sum(),
                delegations_note: seats
                    .iter()
                    .any(|row| row.delegations.is_none())
                    .then(|| DELEGATIONS_UNAVAILABLE.to_string()),
                verified_seats: decided,
                verified_passed: passed,
                verified_pass_rate: rate,
                quality: if rate.is_some() {
                    "verified-pass rate".to_string()
                } else {
                    "unavailable: no attributable verification record in these seats' transcripts"
                        .to_string()
                },
            }
        })
        .collect()
}

/// Reads every stored profile and joins it with token and verification sources.
pub fn collect(state: &StateDir) -> Vec<SeatRow> {
    // A missing or unreadable ledger is `None`, which `tail_delegations` alone would report as an empty list.
    let delegations = state
        .logs()
        .join(log::DELEGATION_FILE)
        .exists()
        .then(|| log::read_delegations(state, DELEGATION_ROW_CAP))
        .filter(|_| log::tail_delegations(state, 0).is_ok());
    store::load_all(state.root())
        .into_iter()
        .map(|(short, profile)| {
            let d = &profile.decision;
            let mut row = SeatRow {
                class: format!(
                    "{}/{}",
                    format!("{:?}", d.intent).to_lowercase(),
                    format!("{:?}", d.complexity).to_lowercase()
                ),
                ..SeatRow::default()
            };
            if let Some(ledger) = &delegations {
                let (mut runs, mut tokens) = (0u64, 0u64);
                for delegation in ledger.iter().filter(|r| r.parent_session == short) {
                    runs += 1;
                    tokens = tokens
                        .saturating_add(delegation.input_tokens)
                        .saturating_add(delegation.cache_creation_input_tokens)
                        .saturating_add(delegation.cache_read_input_tokens)
                        .saturating_add(delegation.output_tokens);
                }
                row.delegations = Some(runs);
                row.delegation_tokens = Some(tokens);
            }
            if let Some(path) = session_spend::resolve_transcript(state, &short) {
                let fold = session_spend::session_transcript_usage(Some(&path), None);
                if fold.source_present {
                    let mut sum = [0u64; 4];
                    for bucket in &fold.buckets {
                        sum[0] = sum[0].saturating_add(bucket.usage.input_tokens);
                        sum[1] = sum[1].saturating_add(bucket.usage.cache_creation_input_tokens);
                        sum[2] = sum[2].saturating_add(bucket.usage.cache_read_input_tokens);
                        sum[3] = sum[3].saturating_add(bucket.usage.output_tokens);
                    }
                    row.seat_tokens = Some(sum);
                }
                if let Ok(text) = std::fs::read_to_string(&path) {
                    row.verification = adapters::claude::structural_context(&text, 0)
                        .last_verification
                        .map(|outcome| outcome.status);
                }
            }
            row
        })
        .collect()
}

fn cell(value: Option<u64>) -> String {
    value.map_or_else(|| "--".to_string(), |v| v.to_string())
}

pub fn run<W: Write>(args: &StatsArgs, w: &mut W) -> CtxResult<i32> {
    let env = config::env_from_process();
    run_in(&StateDir::resolve(&env)?, args, w)
}

fn run_in<W: Write>(state: &StateDir, args: &StatsArgs, w: &mut W) -> CtxResult<i32> {
    let stats = compute(&collect(state));
    if args.json {
        writeln!(w, "{}", serde_json::to_string_pretty(&stats)?)?;
        return Ok(0);
    }
    if stats.is_empty() {
        writeln!(w, "no proxy-decided seats recorded yet")?;
        return Ok(0);
    }
    writeln!(
        w,
        "class (intent/complexity): seats, median tokens in/out/cache, delegation tokens, quality"
    )?;
    for s in &stats {
        let delegation = match (s.delegation_tokens, s.delegations) {
            (Some(tokens), Some(runs)) => format!("{tokens} delegation tokens in {runs} runs"),
            _ => format!(
                "delegation tokens {}",
                s.delegations_note
                    .as_deref()
                    .unwrap_or(DELEGATIONS_UNAVAILABLE)
            ),
        };
        let quality = s.verified_pass_rate.map_or_else(
            || format!("quality {}", s.quality),
            |rate| {
                format!(
                    "{}/{} verified pass ({:.0}%)",
                    s.verified_passed,
                    s.verified_seats,
                    rate * 100.0
                )
            },
        );
        writeln!(
            w,
            "{}: {} seats ({} with tokens), {}/{}/{}, {}, {}",
            s.class,
            s.seats,
            s.seats_with_tokens,
            cell(s.median_input_tokens),
            cell(s.median_output_tokens),
            cell(s.median_cache_tokens),
            delegation,
            quality
        )?;
    }
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(class: &str, tokens: Option<[u64; 4]>, v: Option<VerificationStatus>) -> SeatRow {
        SeatRow {
            class: class.to_string(),
            seat_tokens: tokens,
            verification: v,
            ..SeatRow::default()
        }
    }

    #[test]
    fn compute_groups_by_class_and_never_turns_missing_sources_into_zero() {
        let stats = compute(&[
            row(
                "bugfix/trivial",
                Some([10, 0, 5, 20]),
                Some(VerificationStatus::Passed),
            ),
            row(
                "bugfix/trivial",
                Some([30, 0, 5, 40]),
                Some(VerificationStatus::Failed),
            ),
            row("bugfix/trivial", None, Some(VerificationStatus::Unknown)),
            row("feature/bounded", None, None),
        ]);
        assert_eq!(stats.len(), 2);
        let bug = &stats[0];
        assert_eq!((bug.seats, bug.seats_with_tokens), (3, 2));
        assert_eq!(bug.median_input_tokens, Some(30));
        assert_eq!((bug.verified_seats, bug.verified_passed), (2, 1));
        assert_eq!(bug.verified_pass_rate, Some(0.5));
        let feature = &stats[1];
        assert_eq!(feature.median_input_tokens, None);
        assert_eq!(feature.verified_pass_rate, None);
        assert!(
            feature.quality.starts_with("unavailable"),
            "{}",
            feature.quality
        );
    }

    #[test]
    fn collect_joins_stored_profiles_with_delegation_tokens_without_a_transcript() {
        let tmp = tempfile::tempdir().expect("tmp");
        let state = StateDir::from_path(tmp.path().to_path_buf());
        store::save(
            tmp.path(),
            "abcd1234-1",
            &store::StoredProfile {
                decision: super::super::tests::sample_decision(),
                operator_override: None,
                started_workflow_id: None,
            },
        )
        .expect("save");
        log::append_delegation(
            &state,
            &log::Delegation {
                ts: 1,
                session: "child001",
                parent_session: "abcd1234",
                work_group_id: None,
                agent: "codex",
                model: None,
                input_tokens: 100,
                cache_creation_input_tokens: 0,
                cache_read_input_tokens: 10,
                output_tokens: 50,
                wall_ms: 1,
                exit_code: 0,
                outcome: "ok",
                mode: None,
                task_class: None,
                principal: "root",
                envelope_sha256: None,
            },
        )
        .expect("append");
        let stats = compute(&collect(&state));
        assert_eq!(stats.len(), 1);
        assert_eq!(stats[0].class, "feature/substantial");
        assert_eq!(
            (stats[0].delegations, stats[0].delegation_tokens),
            (Some(1), Some(160))
        );
        assert_eq!(stats[0].delegations_note, None);
        assert_eq!(stats[0].seats_with_tokens, 0);
        assert_eq!(stats[0].median_input_tokens, None);
    }

    /// An absent ledger is unavailable with its reason in text and JSON, and quality prints its reason too.
    #[test]
    fn a_missing_delegation_ledger_is_unavailable_not_zero_and_quality_prints_its_reason() {
        let tmp = tempfile::tempdir().expect("tmp");
        let state = StateDir::from_path(tmp.path().to_path_buf());
        store::save(
            tmp.path(),
            "abcd1234-1",
            &store::StoredProfile {
                decision: super::super::tests::sample_decision(),
                operator_override: None,
                started_workflow_id: None,
            },
        )
        .expect("save");
        assert!(!state.logs().join(log::DELEGATION_FILE).exists());

        let stats = compute(&collect(&state));
        assert_eq!(
            (stats[0].delegations, stats[0].delegation_tokens),
            (None, None)
        );
        assert_eq!(
            stats[0].delegations_note.as_deref(),
            Some(DELEGATIONS_UNAVAILABLE)
        );

        let mut json = Vec::new();
        run_in(&state, &StatsArgs { json: true }, &mut json).expect("json");
        let json: serde_json::Value = serde_json::from_slice(&json).expect("parse");
        assert!(json[0]["delegation_tokens"].is_null(), "{json}");
        assert!(json[0]["delegations"].is_null(), "{json}");
        assert_eq!(json[0]["delegations_note"], DELEGATIONS_UNAVAILABLE);

        let mut text = Vec::new();
        run_in(&state, &StatsArgs { json: false }, &mut text).expect("text");
        let text = String::from_utf8(text).expect("utf8");
        assert!(text.contains(DELEGATIONS_UNAVAILABLE), "{text}");
        assert!(!text.contains("0 delegation tokens"), "{text}");
        assert!(
            text.contains("quality unavailable: no attributable verification record"),
            "{text}"
        );
    }
}
