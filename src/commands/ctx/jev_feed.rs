//! The dashboard's Jev decision feed: the newest decisions of the `[jev]` sites and the harness
//! proxy's TypeSafe intake, each as a one-line plain-language verdict.
//!
//! Read-only and bounded like `graph_steps`: each log is tail-read (at most `TAIL_BYTES`) and its
//! parsed rows are cached on (mtime, len). Meant for the dashboard's background gather only.

use std::collections::BTreeMap;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::SystemTime;

use serde::Serialize;
use serde_json::Value;

use super::approvals::clean;
use super::config::{CtxConfig, JevConfig};
use super::event::TranscriptUsage;
use super::snapshot::redact_text;
use super::state::StateDir;
use super::{jev, memory, sessions, task};
use crate::commands::workflow::{engine, review};

const TAIL_BYTES: u64 = 256 * 1024;
const KEEP: usize = 40;
/// A Jev call this close to a seat's start belongs to the request that launched it.
pub const INTAKE_GRACE_SECS: u64 = 120;
const TEXT_COLS: usize = 60;
/// The floor of a site that has none of its own named in the Jev code.
const DEFAULT_FLOOR: f32 = 0.5;
/// Two rows of one intake call (the Jev row and the proxy row) land within this many seconds.
const SAME_CALL_SECS: u64 = 2;
/// The catalogue id the `typesafe` vendor prices on (`catalogue::TYPESAFE_RUNGS`).
const PRICED_MODEL: &str = "jev-latest";

/// One Jev call, as the feed shows it.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct JevDecision {
    pub ts: u64,
    pub site: String,
    /// Plain-language verdict, redacted, at most 60 characters.
    pub text: String,
    pub confidence: f64,
    /// `confidence` is at or above the site's floor.
    pub sure: bool,
    pub cost_usd: f64,
    pub cached: bool,
}

/// The decisions the dashboard shows and the `[jev]` sites that are switched on.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct JevFeed {
    /// The newest `KEEP` (40) decisions in scope, newest first.
    pub decisions: Vec<JevDecision>,
    /// The names of the `[jev]` gates that are on.
    pub enabled: Vec<&'static str>,
}

/// What the feed is scoped to. Jev rows carry a session and proxy rows a repository, so a session
/// scope takes its Jev rows by session and its proxy rows by the session's repository, and a
/// repository scope takes Jev rows of the registered sessions that run there.
pub enum JevScope<'a> {
    /// The seat's session and its delegated children, whose Jev rows count as the seat's own.
    Session {
        session: &'a str,
        children: &'a [String],
        repo: &'a Path,
    },
    Repo(&'a Path),
}

type Cache = Mutex<BTreeMap<PathBuf, ((SystemTime, u64), Vec<Value>)>>;

fn cache() -> &'static Cache {
    static CACHE: OnceLock<Cache> = OnceLock::new();
    CACHE.get_or_init(Default::default)
}

/// The parsed rows of the last `TAIL_BYTES` of a JSONL log; empty when it is missing.
fn rows(path: &Path) -> Vec<Value> {
    let Some(key) = std::fs::metadata(path)
        .ok()
        .and_then(|meta| Some((meta.modified().ok()?, meta.len())))
    else {
        return Vec::new();
    };
    let mut cache = cache()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if let Some((cached, rows)) = cache.get(path)
        && *cached == key
    {
        return rows.clone();
    }
    let start = key.1.saturating_sub(TAIL_BYTES);
    let mut bytes = Vec::new();
    let read = std::fs::File::open(path).and_then(|mut file| {
        file.seek(SeekFrom::Start(start))?;
        file.take(TAIL_BYTES).read_to_end(&mut bytes)
    });
    if read.is_err() {
        return Vec::new();
    }
    let text = String::from_utf8_lossy(&bytes);
    let mut lines = text.lines();
    // A read that starts mid-file begins inside a line.
    if start > 0 {
        lines.next();
    }
    let parsed: Vec<Value> = lines
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect();
    if cache.len() >= 8 {
        cache.clear();
    }
    cache.insert(path.to_path_buf(), (key, parsed.clone()));
    parsed
}

/// The confidence floor each site's code decides by, from the constants that code uses.
fn floor_of(site: &str, cfg: &CtxConfig) -> f32 {
    match site {
        "memory" => memory::MEMORY_RELEVANCE_FLOOR as f32,
        "dispatch" => super::hook::DISPATCH_TIER_FLOOR,
        "crash" => task::CRASH_TRIAGE_FLOOR,
        "intake" | "proxy" => cfg.proxy.min_confidence,
        site if site == review::REVIEW_DISPOSITION_LABEL => review::JEV_DISPOSITION_CONFIDENCE,
        site if site == review::REVIEW_DEDUP_LABEL => review::JEV_DEDUP_PROBABILITY as f32,
        site if site == engine::GATE_RECLASS_LABEL => engine::GATE_RECLASS_NOUL_DEFAULT_FLOOR.0,
        _ => DEFAULT_FLOOR,
    }
}

/// Floors are f32 constants, so compare in f32: widening one to f64 puts 0.6 above a parsed 0.6.
fn at_or_above(confidence: f64, floor: f32) -> bool {
    confidence as f32 >= floor
}

fn cost_usd(input_tokens: u64) -> f64 {
    let usage = TranscriptUsage {
        input_tokens,
        ..Default::default()
    };
    super::price::price(PRICED_MODEL, &usage, &super::price::built_in_table())
        .map_or(input_tokens as f64 * 0.042 / 1e6, |micros| {
            micros as f64 / 1e6
        })
}

fn line_of(text: &str) -> String {
    clean(&redact_text(text), TEXT_COLS)
}

fn number(value: &Value) -> Option<f64> {
    value
        .get("Noul")
        .or_else(|| value.get("Score"))
        .and_then(Value::as_f64)
}

/// The answer a site is summarised by: its most confident one.
fn top_answer(answers: &serde_json::Map<String, Value>) -> Option<(&String, &Value, f64)> {
    answers
        .iter()
        .map(|(id, answer)| {
            let confidence = answer
                .get("confidence")
                .and_then(Value::as_f64)
                .unwrap_or(0.0);
            (id, answer, confidence)
        })
        .max_by(|a, b| a.2.total_cmp(&b.2))
}

/// The verdict of one `jev-decisions.jsonl` row and the confidence it rests on.
fn jev_verdict(site: &str, row: &Value) -> (String, f64) {
    let answers = row.get("answers").and_then(Value::as_object);
    let Some((id, top, confidence)) = answers.and_then(top_answer) else {
        return (format!("{site}: no answer, fell back"), 0.0);
    };
    let value = top.get("value").unwrap_or(&Value::Null);
    let text = match site {
        "memory" => {
            let all = answers.map_or(0, |a| a.len());
            let best = answers
                .into_iter()
                .flatten()
                .filter_map(|(_, a)| number(a.get("value")?))
                .fold(0.0_f64, f64::max);
            format!("ranked {all} notes, top {best:.2}")
        }
        site if site == engine::GATE_RECLASS_LABEL => format!("gate risk: {id} {confidence:.2}"),
        // The retired artifact-substance site: rows written before it was removed still render.
        "workflow-artifact-substance" => {
            let verdict = value
                .get("Choice")
                .and_then(Value::as_str)
                .unwrap_or("unclear");
            format!("artifact is {verdict}")
        }
        "intake" => match answers
            .and_then(|a| a.get("needs_clarification"))
            .and_then(|a| number(a.get("value")?))
        {
            Some(score) if score >= 0.5 => "request needs clarifying".to_string(),
            Some(_) => "request is clear enough".to_string(),
            None => format!("intake: {id}"),
        },
        _ => {
            let shown = value
                .get("Choice")
                .and_then(Value::as_str)
                .map(str::to_string)
                .or_else(|| number(value).map(|n| format!("{n:.2}")))
                .unwrap_or_else(|| id.clone());
            format!("{site}: {shown}")
        }
    };
    (text, confidence)
}

fn proxy_decision(row: &Value, cfg: &CtxConfig) -> Option<JevDecision> {
    let text = |key: &str| row.get(key).and_then(Value::as_str).unwrap_or("?");
    let confidence = row
        .get("confidence")
        .and_then(Value::as_object)
        .and_then(|map| {
            map.values()
                .filter_map(Value::as_f64)
                .min_by(f64::total_cmp)
        })
        .unwrap_or(0.0);
    Some(JevDecision {
        ts: row.get("created_at")?.as_u64()?,
        site: "intake".to_string(),
        text: line_of(&format!(
            "request: {} \u{b7} {} \u{b7} {} risk",
            text("intent"),
            text("complexity"),
            text("risk")
        )),
        confidence,
        sure: at_or_above(confidence, floor_of("proxy", cfg)),
        cost_usd: cost_usd(
            row.pointer("/usage/input_tokens")
                .and_then(Value::as_u64)
                .unwrap_or(0),
        ),
        cached: false,
    })
}

fn jev_decision(row: &Value, cfg: &CtxConfig) -> Option<JevDecision> {
    let site = row.get("site")?.as_str()?;
    let (text, confidence) = jev_verdict(site, row);
    Some(JevDecision {
        ts: row.get("ts")?.as_u64()?,
        site: line_of(site),
        text: line_of(&text),
        confidence,
        sure: at_or_above(confidence, floor_of(site, cfg)),
        cost_usd: cost_usd(
            row.pointer("/usage/input_tokens")
                .and_then(Value::as_u64)
                .unwrap_or(0),
        ),
        cached: row.get("cached").and_then(Value::as_bool).unwrap_or(false),
    })
}

/// The `[jev]` gates that are on, by their config key.
pub fn enabled_sites(jev: &JevConfig) -> Vec<&'static str> {
    super::jev::gate_list(jev)
        .into_iter()
        .filter_map(|(name, on)| on.then_some(name))
        .collect()
}

/// The newest 40 decisions in `scope` at or after `since` (epoch seconds), plus the enabled `[jev]`
/// sites. A proxy row names no session, so a session scope takes only the one inside the intake window
/// around `since` (the seat's start minus the grace), and a repository scope takes any at or after it.
pub fn jev_feed(state: &StateDir, cfg: &CtxConfig, scope: JevScope<'_>, since: u64) -> JevFeed {
    let proxy_until =
        matches!(scope, JevScope::Session { .. }).then(|| since + 2 * INTAKE_GRACE_SECS);
    let (session_ids, repo): (Vec<String>, &Path) = match scope {
        JevScope::Session {
            session,
            children,
            repo,
        } => {
            let mut ids = vec![session.to_string()];
            ids.extend(jev::alias_sources(state, |to| to == session));
            ids.extend(children.iter().cloned());
            (ids, repo)
        }
        JevScope::Repo(repo) => (
            super::graph::read_session_records(state)
                .into_iter()
                .filter(|(record, _)| record.repo == repo)
                .map(|(record, _)| record.session)
                .collect(),
            repo,
        ),
    };
    let in_scope = |row_session: &str| {
        !row_session.is_empty()
            && session_ids.iter().any(|id| {
                id == row_session || sessions::short_id(id) == sessions::short_id(row_session)
            })
    };

    let mut proxy: Vec<JevDecision> = rows(&state.root().join("proxy-decisions.jsonl"))
        .iter()
        .filter(|row| row.get("decider").and_then(Value::as_str) == Some("typesafe"))
        .filter(|row| row.get("repo").and_then(Value::as_str).map(Path::new) == Some(repo))
        .filter(|row| {
            let created = row.get("created_at").and_then(Value::as_u64).unwrap_or(0);
            match row.get("session").and_then(Value::as_str) {
                Some(session) if !session.is_empty() => in_scope(session),
                // Sessionless: only the intake window around the seat's start is this seat's.
                _ => proxy_until.is_none_or(|end| created <= end),
            }
        })
        .filter_map(|row| proxy_decision(row, cfg))
        .filter(|decision| decision.ts >= since)
        .collect();
    let mut decisions: Vec<JevDecision> = rows(&state.root().join(jev::JEV_DECISIONS_FILE))
        .iter()
        .filter(|row| {
            in_scope(
                row.get("session")
                    .and_then(Value::as_str)
                    .unwrap_or_default(),
            )
        })
        .filter_map(|row| jev_decision(row, cfg))
        .filter(|decision| decision.ts >= since)
        // The proxy row of an intake call says more than the Jev row it also left.
        .filter(|decision| {
            decision.site != "intake"
                || !proxy
                    .iter()
                    .any(|row| row.ts.abs_diff(decision.ts) <= SAME_CALL_SECS)
        })
        .collect();
    decisions.append(&mut proxy);
    decisions.sort_by_key(|decision| std::cmp::Reverse(decision.ts));
    decisions.truncate(KEEP);
    JevFeed {
        decisions,
        enabled: enabled_sites(&cfg.jev),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SESSION: &str = "aaaa1111-0000-4000-8000-000000000001";

    fn fixture() -> (tempfile::TempDir, StateDir) {
        let tmp = tempfile::tempdir().expect("tmp");
        let state = StateDir::resolve(&|_| Some(tmp.path().display().to_string())).expect("state");
        let fixtures = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/jev-feed");
        for name in ["jev-decisions.jsonl", "proxy-decisions.jsonl"] {
            std::fs::copy(fixtures.join(name), tmp.path().join(name)).expect("fixture");
        }
        (tmp, state)
    }

    fn feed(state: &StateDir, scope: JevScope<'_>) -> JevFeed {
        // The fixture's intake rows sit just after a seat that started at 1000 (grace 120 s before it).
        let since = if matches!(scope, JevScope::Session { .. }) {
            900
        } else {
            0
        };
        jev_feed(state, &CtxConfig::default(), scope, since)
    }

    fn by_site<'a>(feed: &'a JevFeed, site: &str) -> &'a JevDecision {
        feed.decisions
            .iter()
            .find(|d| d.site == site)
            .unwrap_or_else(|| panic!("no {site} in {:?}", feed.decisions))
    }

    #[test]
    fn each_site_kind_reads_as_plain_language_with_its_floor_and_cost() {
        let (_tmp, state) = fixture();
        let feed = feed(
            &state,
            JevScope::Session {
                session: SESSION,
                children: &[],
                repo: Path::new("/work/repo"),
            },
        );
        let memory = by_site(&feed, "memory");
        assert_eq!(memory.text, "ranked 3 notes, top 0.72");
        assert!(memory.sure && memory.cached && memory.cost_usd == 0.0);
        assert_eq!(
            by_site(&feed, "workflow-gate-reclassification").text,
            "gate risk: architecture 0.71"
        );
        let artifact = by_site(&feed, "workflow-artifact-substance");
        assert_eq!(artifact.text, "artifact is substantive");
        assert!(artifact.sure, "1.0 clears the 0.9 artifact floor");
        assert!((artifact.cost_usd - 1751.0 * 0.042 / 1e6).abs() < 1e-6);
        let proxy = feed
            .decisions
            .iter()
            .find(|d| d.text.starts_with("request:"))
            .expect("proxy row");
        assert_eq!(
            proxy.text,
            "request: feature \u{b7} bounded \u{b7} low risk"
        );
        assert_eq!(by_site(&feed, "dispatch").text, "dispatch: standard");
        let unsure = by_site(&feed, "crash");
        assert_eq!(unsure.text, "crash: access");
        assert!(!unsure.sure, "0.85 is under the 0.9 crash floor");
        // The intake call that left both a Jev row and a proxy row shows once, as the proxy row.
        assert_eq!(
            feed.decisions.iter().filter(|d| d.site == "intake").count(),
            1
        );
        assert!(feed.decisions.windows(2).all(|w| w[0].ts >= w[1].ts));
    }

    #[test]
    fn an_intake_row_alone_reads_as_a_clarification_verdict() {
        let (_tmp, state) = fixture();
        let feed = feed(
            &state,
            JevScope::Session {
                session: SESSION,
                children: &[],
                repo: Path::new("/elsewhere"),
            },
        );
        assert_eq!(by_site(&feed, "intake").text, "request needs clarifying");
    }

    #[test]
    fn the_scope_filter_keeps_only_that_sessions_jev_rows_and_that_repos_proxy_rows() {
        let (_tmp, state) = fixture();
        let other = feed(
            &state,
            JevScope::Session {
                session: "bbbb2222-0000-4000-8000-000000000002",
                children: &[],
                repo: Path::new("/work/other"),
            },
        );
        assert_eq!(other.decisions.len(), 1, "{:?}", other.decisions);
        assert_eq!(other.decisions[0].site, "memory");
        let nobody = feed(
            &state,
            JevScope::Session {
                session: "cccc3333-0000-4000-8000-000000000003",
                children: &[],
                repo: Path::new("/nowhere"),
            },
        );
        assert!(nobody.decisions.is_empty());
        // A repository scope takes the Jev rows of the registered sessions that run there.
        let mut record = sessions::Record::new(
            SESSION,
            "claude",
            Path::new("/work/repo"),
            sessions::Verb::Wrap,
        );
        record.pid = std::process::id();
        std::fs::create_dir_all(state.sessions()).expect("sessions");
        std::fs::write(
            state.sessions().join(format!("{}.json", record.short)),
            serde_json::to_string(&record).expect("json"),
        )
        .expect("record");
        let repo = feed(&state, JevScope::Repo(Path::new("/work/repo")));
        assert!(repo.decisions.iter().any(|d| d.site == "dispatch"));
        assert!(
            !repo
                .decisions
                .iter()
                .any(|d| d.text == "ranked 1 notes, top 0.40"),
            "another session's row stays out"
        );
    }

    #[test]
    fn since_drops_older_rows_and_a_childs_rows_count_as_the_seats() {
        let (_tmp, state) = fixture();
        let child = ["bbbb2222-0000-4000-8000-000000000002".to_string()];
        let scope = |children| JevScope::Session {
            session: SESSION,
            children,
            repo: Path::new("/work/repo"),
        };
        let cfg = CtxConfig::default();
        // A concurrent seat's intake, ten minutes after this seat started, is not this seat's.
        let mut proxy = std::fs::read_to_string(state.root().join("proxy-decisions.jsonl"))
            .expect("proxy rows");
        proxy.push_str("{\"repo\": \"/work/repo\", \"intent\": \"feature\", \"complexity\": \"bounded\", \"risk\": \"low\", \"decider\": \"typesafe\", \"confidence\": {}, \"usage\": {}, \"created_at\": 1650}\n");
        std::fs::write(state.root().join("proxy-decisions.jsonl"), proxy).expect("write");
        // A session-tagged row of this seat is never cut by the launch window.
        let mut proxy = std::fs::read_to_string(state.root().join("proxy-decisions.jsonl"))
            .expect("proxy rows");
        proxy.push_str(&format!("{{\"repo\": \"/work/repo\", \"session\": \"{SESSION}\", \"intent\": \"feature\", \"complexity\": \"bounded\", \"risk\": \"low\", \"decider\": \"typesafe\", \"confidence\": {{}}, \"usage\": {{}}, \"created_at\": 1655}}\n"));
        std::fs::write(state.root().join("proxy-decisions.jsonl"), proxy).expect("write");
        let all = jev_feed(&state, &cfg, scope(&[]), 900);
        assert!(all.decisions.iter().any(|d| d.ts == 1655), "{all:?}");
        assert!(all.decisions.iter().any(|d| d.ts == 1050), "{all:?}");
        assert!(!all.decisions.iter().any(|d| d.ts == 1650), "{all:?}");
        let repo_wide = jev_feed(&state, &cfg, JevScope::Repo(Path::new("/work/repo")), 900);
        assert!(repo_wide.decisions.iter().any(|d| d.ts == 1650));
        assert!(
            !all.decisions
                .iter()
                .any(|d| d.text == "ranked 1 notes, top 0.40")
        );
        let recent = jev_feed(&state, &cfg, scope(&[]), 1051);
        assert!(recent.decisions.iter().all(|d| d.ts >= 1051), "{recent:?}");
        let with_child = jev_feed(&state, &cfg, scope(&child), 900);
        assert!(
            with_child
                .decisions
                .iter()
                .any(|d| d.text == "ranked 1 notes, top 0.40"),
            "{with_child:?}"
        );
    }

    #[test]
    fn the_feed_is_capped_at_forty_and_lists_the_enabled_sites() {
        let tmp = tempfile::tempdir().expect("tmp");
        let state = StateDir::resolve(&|_| Some(tmp.path().display().to_string())).expect("state");
        let row = |ts: u64| {
            format!(
                "{{\"site\":\"dispatch\",\"ts\":{ts},\"answers\":{{\"tier\":{{\"value\":{{\"Choice\":\"cheap\"}},\"confidence\":0.9,\"margin\":0.5}}}},\"usage\":{{\"input_tokens\":0,\"output_tokens\":0}},\"wall_ms\":1,\"fallbacks\":[],\"cached\":false,\"session\":\"{SESSION}\"}}\n"
            )
        };
        let body: String = (1..=60).map(row).collect();
        std::fs::write(tmp.path().join("jev-decisions.jsonl"), body).expect("write");
        let mut cfg = CtxConfig::default();
        cfg.jev.supervisor = true;
        cfg.jev.stop_verify = true;
        let feed = jev_feed(
            &state,
            &cfg,
            JevScope::Session {
                session: SESSION,
                children: &[],
                repo: Path::new("/x"),
            },
            0,
        );
        assert_eq!(feed.decisions.len(), KEEP);
        assert_eq!(feed.decisions[0].ts, 60);
        assert_eq!(feed.enabled, vec!["supervisor", "stop_verify"]);
    }

    #[test]
    fn a_confidence_exactly_at_the_floor_is_sure() {
        let mut cfg = CtxConfig::default();
        for floor in [0.6_f32, 0.3] {
            cfg.proxy.min_confidence = floor;
            let row = |c: f64| {
                serde_json::json!({
                    "created_at": 1, "intent": "i", "complexity": "c", "risk": "r",
                    "confidence": {"intent": c}
                })
            };
            let at = proxy_decision(&row(f64::from(floor)), &cfg).expect("row");
            assert!(at.sure, "floor {floor}");
            let text = floor.to_string().parse::<f64>().expect("parse");
            assert!(
                proxy_decision(&row(text), &cfg).expect("row").sure,
                "{text}"
            );
            assert!(!proxy_decision(&row(text - 0.01), &cfg).expect("row").sure);
        }
    }

    #[test]
    fn text_is_redacted_and_capped_at_sixty_characters() {
        let long = format!("word{}", " more".repeat(40));
        let (text, _) = jev_verdict(
            &long,
            &serde_json::json!({"answers": {"q": {"value": {"Choice": "x"}, "confidence": 0.9}}}),
        );
        assert!(line_of(&text).chars().count() <= TEXT_COLS);
        assert!(!line_of("ghp_1234567890abcdefghijklmnopqrstuvwx").contains("ghp_1234567890"));
    }
}
