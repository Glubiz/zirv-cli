//! `zirv ctx jev probe` (issue: autoresearch Jev-floor determinism
//! campaign): asks ONE Jev site's real production question(s) for a fixture
//! input K times with the cache disabled, applies that site's production
//! floor (`jev::floor`, itself honouring the operator's `[jev.floors.<site>]`
//! config and `ZIRV_CTX_JEV_FLOOR_<SITE>_MIN_CONFIDENCE|_MIN_MARGIN` env
//! overlays -- see `config::JevFloorsConfig`) and production answer-to-action
//! rule, and prints what production would have DONE on each rep.
//!
//! This is a MEASUREMENT verb: it calls the exact same `jev::advise_detailed`
//! entry point every `[jev]`-gated site calls, with the site's own production
//! advise-site label, so `jev-decisions.jsonl`/`jev-effects.jsonl` and the
//! spend ledger see exactly what a real production call would write. It has
//! no other side effect -- no repository write, no memory/handoff/context
//! mutation, no catalogue/model rewrite. Every action-decision rule below is
//! the exact same `pub(crate) fn` production itself calls (see each site
//! module's own doc comment on its extracted fn), so this can never drift
//! from what production actually does.
//!
//! CLI contract (an external worker's backend depends on this exactly):
//! `zirv ctx jev probe --site <SITE> --case <case.json> --reps <K> [--repo
//! <dir>]` (stdout is always JSON, no `--json` flag). `K` must be in
//! `1..=20`; an unknown `SITE` or a missing
//! Jev credential both exit 2 with a message. `case.json` is `{"id": "<case
//! id>", "state": <the exact JSON state object production sends>, "n":
//! <candidate count, required only for per-candidate sites>}` -- `state` is
//! sent verbatim (still subject to `jev::safe_metadata_request`; a refusal
//! exits 2). stdout is one JSON object: `{"site", "floor_site", "label",
//! "floor": {"min_confidence", "min_margin"}, "reps": [{"actions": {"<item
//! id>": "<action>"}, "error": "<string or null>"}], "calls", "errors"}`. A
//! rep whose call failed gets every item's FALLBACK action (what production
//! does on failure) plus its error string.

use std::collections::BTreeMap;
use std::io::Write;
use std::path::Path;

use serde::Deserialize;

use super::config::CtxConfig;
use super::proxy::decision::DOMAIN_QUESTION_IDS;
use super::state::StateDir;
use super::{compile, exec, handoff, hook, inject_gate, jev, memory};
use crate::commands::ctx::CtxResult;
use crate::commands::workflow::profile;

/// One measurable site, exactly the twelve named in the probe's own CLI
/// contract. `MemoryRerank`/`MemoryHarvest` and `ContextReport`/
/// `ContextSkill` share a production advise-site LABEL and/or `jev::
/// FloorSite`, but are distinct SITEs here: each has its own default floor
/// constant a later retune targets independently.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Site {
    MemoryRerank,
    MemoryHarvest,
    ContextReport,
    ContextSkill,
    HarvestScreen,
    HandoffThin,
    HandoffSelect,
    CompactionSelect,
    Dispatch,
    LaunchEffort,
    ClassifyDomain,
    Inject,
}

impl Site {
    fn parse(raw: &str) -> Option<Self> {
        Some(match raw {
            "memory-rerank" => Self::MemoryRerank,
            "memory-harvest" => Self::MemoryHarvest,
            "context-report" => Self::ContextReport,
            "context-skill" => Self::ContextSkill,
            "harvest-screen" => Self::HarvestScreen,
            "handoff-thin" => Self::HandoffThin,
            "handoff-select" => Self::HandoffSelect,
            "compaction-select" => Self::CompactionSelect,
            "dispatch" => Self::Dispatch,
            "launch-effort" => Self::LaunchEffort,
            "classify-domain" => Self::ClassifyDomain,
            "inject" => Self::Inject,
            _ => return None,
        })
    }

    /// The production advise-site LABEL this site's production call site
    /// passes to `jev::advise`/`jev::advise_detailed` -- what actually
    /// appears in `jev-decisions.jsonl`/`jev-effects.jsonl` and the spend
    /// ledger.
    fn production_label(self) -> &'static str {
        match self {
            Self::MemoryRerank | Self::MemoryHarvest => "memory",
            Self::ContextReport => "context-parent-reports",
            Self::ContextSkill => "context-skill-descriptions",
            Self::HarvestScreen => "harvest",
            Self::HandoffThin => "handoff",
            Self::HandoffSelect => "handoff_select",
            Self::CompactionSelect => "compaction_select",
            Self::Dispatch => "dispatch",
            Self::LaunchEffort => "launch_effort",
            Self::ClassifyDomain => "classify",
            Self::Inject => "inject",
        }
    }

    /// The `jev::FloorSite` this site's floor is resolved through, and its
    /// `[jev.floors.<name>]`/`ZIRV_CTX_JEV_FLOOR_<NAME>_*` config name.
    fn floor_site(self) -> (jev::FloorSite, &'static str) {
        match self {
            Self::MemoryRerank | Self::MemoryHarvest => (jev::FloorSite::Memory, "memory"),
            Self::ContextReport | Self::ContextSkill => (jev::FloorSite::Context, "context"),
            Self::HarvestScreen => (jev::FloorSite::HarvestScreen, "harvest_screen"),
            Self::HandoffThin | Self::HandoffSelect => {
                (jev::FloorSite::HandoffSelect, "handoff_select")
            }
            Self::CompactionSelect => (jev::FloorSite::CompactionSelect, "compaction_select"),
            Self::Dispatch => (jev::FloorSite::Dispatch, "dispatch"),
            Self::LaunchEffort => (jev::FloorSite::LaunchEffort, "launch_effort"),
            Self::ClassifyDomain => (jev::FloorSite::Classify, "classify"),
            Self::Inject => (jev::FloorSite::Inject, "inject"),
        }
    }

    /// The default `(min_confidence, min_margin)` production passes to
    /// `jev::floor` for this site -- the exact named constant(s) each
    /// production call site uses, so an operator `[jev.floors]`/env override
    /// is applied identically here.
    fn default_floor(self) -> (f32, f32) {
        match self {
            Self::MemoryRerank => compile::MEMORY_RERANK_DEFAULT_FLOOR,
            Self::MemoryHarvest => memory::MEMORY_HARVEST_DEFAULT_FLOOR,
            Self::ContextReport | Self::ContextSkill => compile::CONTEXT_DEFAULT_FLOOR,
            Self::HarvestScreen => (
                memory::HARVEST_SCREEN_MIN_CONFIDENCE,
                jev::DEFAULT_MIN_MARGIN,
            ),
            Self::HandoffThin => (handoff::HANDOFF_THIN_FLOOR, jev::DEFAULT_MIN_MARGIN),
            Self::HandoffSelect => handoff::HANDOFF_SELECT_DEFAULT_FLOOR,
            Self::CompactionSelect => handoff::COMPACTION_SELECT_DEFAULT_FLOOR,
            Self::Dispatch => (hook::DISPATCH_TIER_FLOOR, jev::DEFAULT_MIN_MARGIN),
            Self::LaunchEffort => exec::LAUNCH_EFFORT_DEFAULT_FLOOR,
            Self::ClassifyDomain => profile::CLASSIFY_DEFAULT_FLOOR,
            Self::Inject => inject_gate::INJECT_DEFAULT_FLOOR,
        }
    }

    /// Whether `case.json` must set `"n"` (the per-candidate item count) for
    /// this site.
    fn requires_n(self) -> bool {
        matches!(
            self,
            Self::MemoryRerank
                | Self::MemoryHarvest
                | Self::ContextReport
                | Self::ContextSkill
                | Self::HandoffSelect
                | Self::CompactionSelect
        )
    }

    /// Builds `(questions to send, item ids to report an action for)`.
    /// These differ only for `ClassifyDomain`: production asks the full
    /// intent+domain question set, but the probe reports actions for the
    /// six domain-tag ids only (the intent item uses a different, proxy
    /// floor and is out of scope here).
    fn build_request(self, case: &Case) -> Result<(Vec<jev::Question>, Vec<String>), String> {
        match self {
            Self::MemoryRerank => {
                let ids = numbered_ids("c", require_n(case)?);
                let questions = compile::memory_rerank_questions(&ids);
                Ok((questions, ids))
            }
            Self::MemoryHarvest => {
                let ids = numbered_ids("c", require_n(case)?);
                let questions = memory::memory_harvest_questions(&ids);
                Ok((questions, ids))
            }
            Self::ContextReport => {
                let ids = numbered_ids("p", require_n(case)?);
                let questions = ids
                    .iter()
                    .map(|id| compile::context_report_question(id))
                    .collect();
                Ok((questions, ids))
            }
            Self::ContextSkill => {
                let ids = numbered_ids("s", require_n(case)?);
                let questions = ids
                    .iter()
                    .map(|id| compile::context_skill_question(id))
                    .collect();
                Ok((questions, ids))
            }
            Self::HarvestScreen => Ok((
                memory::harvest_screen_question().to_vec(),
                vec!["novel".to_string()],
            )),
            Self::HandoffThin => Ok((
                vec![handoff::handoff_quality_question()],
                vec!["quality".to_string()],
            )),
            Self::HandoffSelect => {
                let ids = numbered_ids("c", require_n(case)?);
                let questions = handoff::handoff_select_questions(&ids);
                Ok((questions, ids))
            }
            Self::CompactionSelect => {
                let ids = numbered_ids("k", require_n(case)?);
                let questions = handoff::compaction_select_questions(&ids);
                Ok((questions, ids))
            }
            Self::Dispatch => Ok((
                vec![hook::dispatch_tier_question()],
                vec!["tier".to_string()],
            )),
            Self::LaunchEffort => Ok((
                exec::launch_effort_question().to_vec(),
                vec!["launch_effort_high".to_string()],
            )),
            Self::ClassifyDomain => Ok((
                profile::classify_jev_questions(),
                DOMAIN_QUESTION_IDS
                    .iter()
                    .map(|id| id.to_string())
                    .collect(),
            )),
            Self::Inject => Ok((inject_gate::questions().to_vec(), vec!["defer".to_string()])),
        }
    }

    /// The exact production answer-to-action rule for this site, applied to
    /// one item's answer (or `None` when the item id is missing from the
    /// response).
    fn action(self, answer: Option<&jev::Answer>, min_confidence: f32, min_margin: f32) -> String {
        let action = match self {
            Self::MemoryRerank => compile::memory_rerank_action(answer, min_confidence, min_margin),
            Self::MemoryHarvest => {
                memory::memory_harvest_action(answer, min_confidence, min_margin)
            }
            Self::ContextReport | Self::ContextSkill => {
                if compile::parent_report_omit(answer, min_confidence, min_margin) {
                    "omit"
                } else {
                    "keep"
                }
            }
            Self::HarvestScreen => {
                if memory::harvest_screen_skip(answer, min_confidence, min_margin) {
                    "skip"
                } else {
                    "run"
                }
            }
            Self::HandoffThin => handoff::handoff_thin_action(answer, min_confidence, min_margin),
            Self::HandoffSelect => {
                handoff::handoff_select_action(answer, min_confidence, min_margin)
            }
            Self::CompactionSelect => {
                handoff::compaction_select_action(answer, min_confidence, min_margin)
            }
            Self::Dispatch => hook::dispatch_tier_action(answer, min_confidence, min_margin),
            Self::LaunchEffort => exec::launch_effort_action(answer, min_confidence, min_margin),
            Self::ClassifyDomain => {
                profile::classify_domain_action(answer, min_confidence, min_margin)
            }
            Self::Inject => inject_gate::inject_action(answer, min_confidence, min_margin),
        };
        action.to_string()
    }

    /// The action every item gets on a FAILED call -- what production does
    /// when `jev::advise`/`jev::advise_detailed` returns no answer at all
    /// (gate treated as on but the call itself errored): the exact same
    /// value [`Site::action`] returns for a missing answer, named here once
    /// rather than re-derived per rep.
    fn fallback_action(self) -> &'static str {
        match self {
            Self::MemoryRerank
            | Self::MemoryHarvest
            | Self::ContextReport
            | Self::ContextSkill
            | Self::HandoffSelect
            | Self::HandoffThin => "keep",
            Self::HarvestScreen => "run",
            Self::CompactionSelect => "omit",
            Self::Dispatch => "deny",
            Self::LaunchEffort => "classifier",
            Self::ClassifyDomain => "none",
            Self::Inject => "inject_now",
        }
    }
}

fn numbered_ids(prefix: &str, n: usize) -> Vec<String> {
    (0..n).map(|i| format!("{prefix}{i}")).collect()
}

fn require_n(case: &Case) -> Result<usize, String> {
    match case.n {
        Some(n) if n >= 1 => Ok(n),
        Some(_) => Err("case.json \"n\" must be at least 1".to_string()),
        None => Err("case.json must set \"n\" (candidate count) for this site".to_string()),
    }
}

/// The fixture case `--case` names: `state` is sent verbatim (still subject
/// to `jev::safe_metadata_request`); `n` is the per-candidate item count,
/// required only for a per-candidate site ([`Site::requires_n`]).
#[derive(Debug, Deserialize)]
struct Case {
    #[allow(dead_code)]
    id: String,
    state: serde_json::Value,
    #[serde(default)]
    n: Option<usize>,
}

/// Reads the fallback error text for the rep whose `jev-decisions.jsonl`
/// line count grew from `before_lines` to `before_lines + 1` during the just
/// -completed `advise_detailed` call: its own `fallbacks` array, joined.
/// `advise_detailed` skips this record entirely for one specific failure
/// (`JevError::UnsafeState`, screened out here before any rep runs by the
/// upfront `jev::safe_metadata_request` check -- see [`run_probe`]), so an
/// unchanged line count falls back to that error's own `Display` text rather
/// than reading a line that was never written.
fn last_decision_error(path: &Path, before_lines: usize) -> Option<String> {
    let text = std::fs::read_to_string(path).ok()?;
    let line = text.lines().nth(before_lines)?;
    let value: serde_json::Value = serde_json::from_str(line).ok()?;
    let fallbacks = value.get("fallbacks")?.as_array()?;
    if fallbacks.is_empty() {
        return None;
    }
    Some(
        fallbacks
            .iter()
            .filter_map(serde_json::Value::as_str)
            .collect::<Vec<_>>()
            .join("; "),
    )
}

fn line_count(path: &Path) -> usize {
    std::fs::read_to_string(path)
        .map(|text| text.lines().count())
        .unwrap_or(0)
}

/// `zirv ctx jev probe`'s entry point -- see this module's own doc comment
/// for the full CLI contract. Prints one JSON object to `writer` and returns
/// the process exit code (`0` on success, `2` on any validation refusal).
pub(crate) fn run_probe(
    site_arg: &str,
    case_path: &Path,
    reps: u32,
    repo: Option<&Path>,
    writer: &mut impl Write,
) -> CtxResult<i32> {
    let Some(site) = Site::parse(site_arg) else {
        writeln!(writer, "jev probe: unknown site {site_arg:?}")?;
        return Ok(2);
    };
    if !(1..=20).contains(&reps) {
        writeln!(writer, "jev probe: --reps must be between 1 and 20")?;
        return Ok(2);
    }

    let case_text = match std::fs::read_to_string(case_path) {
        Ok(text) => text,
        Err(err) => {
            writeln!(
                writer,
                "jev probe: cannot read case file {}: {err}",
                case_path.display()
            )?;
            return Ok(2);
        }
    };
    let case: Case = match serde_json::from_str(&case_text) {
        Ok(case) => case,
        Err(err) => {
            writeln!(writer, "jev probe: invalid case JSON: {err}")?;
            return Ok(2);
        }
    };
    if site.requires_n() && case.n.is_none() {
        writeln!(
            writer,
            "jev probe: case.json must set \"n\" (candidate count) for site {site_arg:?}"
        )?;
        return Ok(2);
    }

    let resolved_repo = match repo {
        Some(repo) => repo.to_path_buf(),
        None => std::env::current_dir()?,
    };
    let env = |key: &str| std::env::var(key).ok();
    let mut cfg = CtxConfig::load(&resolved_repo, &env)?;
    // Measurement must be uncached regardless of the operator's own
    // `[jev] cache_ttl_secs`: K reps must be K real HTTP requests.
    cfg.jev.cache_ttl_secs = 0;
    let state = StateDir::resolve(&env)?;

    if !jev::credential_present(&cfg) {
        writeln!(
            writer,
            "jev probe: no Jev credential set ({})",
            jev::credential_env_name(&cfg)
        )?;
        return Ok(2);
    }

    let (questions, report_ids) = match site.build_request(&case) {
        Ok(pair) => pair,
        Err(reason) => {
            writeln!(writer, "jev probe: {reason}")?;
            return Ok(2);
        }
    };

    if !jev::safe_metadata_request(&case.state, &questions, &cfg.proxy.typesafe.model) {
        writeln!(
            writer,
            "jev probe: case \"state\"/question set refused by the metadata-only egress boundary"
        )?;
        return Ok(2);
    }

    let (floor_site, floor_site_name) = site.floor_site();
    let (default_confidence, default_margin) = site.default_floor();
    let (min_confidence, min_margin) =
        jev::floor(&cfg, floor_site, default_confidence, default_margin);

    let label = site.production_label();
    let decisions_path = state.root().join("jev-decisions.jsonl");
    let mut reps_out = Vec::with_capacity(reps as usize);
    let mut errors = 0u32;

    for _ in 0..reps {
        let before_lines = line_count(&decisions_path);
        let status = jev::advise_detailed(&cfg, &state, label, true, &case.state, &questions);
        match status {
            jev::AdvisoryStatus::Answered(answers) => {
                let mut actions = BTreeMap::new();
                for id in &report_ids {
                    actions.insert(
                        id.clone(),
                        site.action(answers.get(id), min_confidence, min_margin),
                    );
                }
                reps_out.push(serde_json::json!({ "actions": actions, "error": null }));
            }
            jev::AdvisoryStatus::Failed
            | jev::AdvisoryStatus::Disabled
            | jev::AdvisoryStatus::MissingCredential => {
                errors += 1;
                let fallback = site.fallback_action();
                let actions: BTreeMap<String, String> = report_ids
                    .iter()
                    .map(|id| (id.clone(), fallback.to_string()))
                    .collect();
                let error_text = last_decision_error(&decisions_path, before_lines)
                    .unwrap_or_else(|| "unsafe Jev metadata projection".to_string());
                reps_out.push(serde_json::json!({ "actions": actions, "error": error_text }));
            }
        }
    }

    let output = serde_json::json!({
        "site": site_arg,
        "floor_site": floor_site_name,
        "label": label,
        "floor": { "min_confidence": min_confidence, "min_margin": min_margin },
        "reps": reps_out,
        "calls": reps,
        "errors": errors,
    });
    writeln!(writer, "{}", serde_json::to_string(&output)?)?;
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::ctx::jev::tests::multi_shot_server;
    use crate::commands::ctx::state::STATE_ENV;

    /// Sets every `(key, value)` pair, runs `body`, then unsets them all --
    /// nextest's per-test-process isolation (this repo's own convention;
    /// see `jev.rs`'s own `with_credential`) is what makes touching real
    /// process env vars safe here.
    fn with_env<T>(vars: &[(&str, String)], body: impl FnOnce() -> T) -> T {
        unsafe {
            for (key, value) in vars {
                std::env::set_var(key, value);
            }
        }
        let result = body();
        unsafe {
            for (key, _) in vars {
                std::env::remove_var(key);
            }
        }
        result
    }

    fn write_case(dir: &Path, name: &str, body: &serde_json::Value) -> std::path::PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, serde_json::to_vec(body).expect("serialize case"))
            .expect("write case");
        path
    }

    /// The env vars every test that actually reaches a Jev call needs:
    /// an isolated state dir, a uniquely-named credential env pointing at
    /// `url`, and a short timeout. `tag` keeps each test's own credential
    /// env name distinct so parallel nextest processes never race on the
    /// same key.
    fn base_env(state_dir: &Path, url: &str, tag: &str) -> Vec<(&'static str, String)> {
        vec![
            (STATE_ENV, state_dir.display().to_string()),
            (
                "ZIRV_CTX_PROXY_TYPESAFE_CREDENTIAL_ENV",
                format!("JEV_PROBE_TEST_CRED_{tag}"),
            ),
            ("ZIRV_CTX_PROXY_TYPESAFE_BASE_URL", url.to_string()),
            ("ZIRV_CTX_PROXY_TYPESAFE_TIMEOUT_SECS", "5".to_string()),
            (
                Box::leak(format!("JEV_PROBE_TEST_CRED_{tag}").into_boxed_str()),
                "secret".to_string(),
            ),
        ]
    }

    fn inject_case() -> serde_json::Value {
        serde_json::json!({
            "id": "c1",
            "state": {
                "_zirv_metadata_only": true,
                "facts": [[0, 50, 80, 30, 60, 3, 0, 5, 0, 2, 1, 0, 0, 0, 1]],
            },
        })
    }

    fn defer_response_body() -> String {
        serde_json::json!({
            "model": "jev-1.13.0",
            "answers": {"defer": {"type": "noul", "noul": 0.85}},
            "usage": {"input_tokens": 5, "output_tokens": 1},
        })
        .to_string()
    }

    #[test]
    fn unknown_site_and_out_of_range_reps_both_refuse_with_exit_2() {
        let dir = tempfile::tempdir().expect("tempdir");
        let case = write_case(dir.path(), "case.json", &inject_case());

        let mut out = Vec::new();
        let code = run_probe("not-a-real-site", &case, 3, Some(dir.path()), &mut out).expect("run");
        assert_eq!(code, 2);

        let mut out = Vec::new();
        let code = run_probe("inject", &case, 0, Some(dir.path()), &mut out).expect("run");
        assert_eq!(code, 2);

        let mut out = Vec::new();
        let code = run_probe("inject", &case, 21, Some(dir.path()), &mut out).expect("run");
        assert_eq!(code, 2);
    }

    #[test]
    fn missing_n_on_a_per_candidate_site_refuses_with_exit_2() {
        let dir = tempfile::tempdir().expect("tempdir");
        let case = write_case(
            dir.path(),
            "case.json",
            &serde_json::json!({"id": "c1", "state": {"_zirv_metadata_only": true, "facts": []}}),
        );
        let mut out = Vec::new();
        let code = run_probe("memory-rerank", &case, 1, Some(dir.path()), &mut out).expect("run");
        assert_eq!(code, 2);
        let text = String::from_utf8(out).expect("utf8");
        assert!(text.contains("\"n\""), "{text}");
    }

    #[test]
    fn missing_credential_refuses_with_exit_2() {
        let dir = tempfile::tempdir().expect("tempdir");
        let case = write_case(dir.path(), "case.json", &inject_case());
        with_env(
            &[(
                "ZIRV_CTX_PROXY_TYPESAFE_CREDENTIAL_ENV",
                "JEV_PROBE_TEST_NEVER_SET_1091".to_string(),
            )],
            || {
                let mut out = Vec::new();
                let code = run_probe("inject", &case, 1, Some(dir.path()), &mut out).expect("run");
                assert_eq!(code, 2);
                let text = String::from_utf8(out).expect("utf8");
                assert!(text.contains("credential"), "{text}");
            },
        );
    }

    /// Inject's noul question decides "defer" at p >= DEFER_MIN_PROBABILITY
    /// (0.8). A fixed noul response of 0.85 is a decisive "defer" under the
    /// compiled default (0.0, DEFAULT_MIN_MARGIN) (margin (0.85-0.5)*2=0.7
    /// clears 0.2), but a ZIRV_CTX_JEV_FLOOR_INJECT_MIN_MARGIN override
    /// raised past 0.7 makes the same answer indecisive, flipping the
    /// reported action to the site's own fallback ("inject_now") -- proving
    /// the probe reads the SAME `[jev.floors.inject]`/env-overlay floor
    /// production would.
    #[test]
    fn a_floor_override_flips_a_borderline_answer_from_decisive_to_fallback() {
        let case_state = inject_case();

        let default_dir = tempfile::tempdir().expect("tempdir");
        let (url, handle) =
            multi_shot_server(200, Box::leak(defer_response_body().into_boxed_str()), 1);
        let case = write_case(default_dir.path(), "case.json", &case_state);
        with_env(
            &base_env(default_dir.path(), &url, "FLOOR_DEFAULT_1091"),
            || {
                let mut out = Vec::new();
                let code =
                    run_probe("inject", &case, 1, Some(default_dir.path()), &mut out).expect("run");
                assert_eq!(code, 0, "{}", String::from_utf8_lossy(&out));
                let value: serde_json::Value = serde_json::from_slice(&out).expect("json");
                assert_eq!(value["reps"][0]["actions"]["defer"], "defer");
            },
        );
        handle.join().expect("server thread");

        let raised_dir = tempfile::tempdir().expect("tempdir");
        let (url, handle) =
            multi_shot_server(200, Box::leak(defer_response_body().into_boxed_str()), 1);
        let case = write_case(raised_dir.path(), "case.json", &case_state);
        let mut vars = base_env(raised_dir.path(), &url, "FLOOR_RAISED_1091");
        vars.push(("ZIRV_CTX_JEV_FLOOR_INJECT_MIN_MARGIN", "0.9".to_string()));
        with_env(&vars, || {
            let mut out = Vec::new();
            let code =
                run_probe("inject", &case, 1, Some(raised_dir.path()), &mut out).expect("run");
            assert_eq!(code, 0, "{}", String::from_utf8_lossy(&out));
            let value: serde_json::Value = serde_json::from_slice(&out).expect("json");
            assert_eq!(value["reps"][0]["actions"]["defer"], "inject_now");
            let min_margin = value["floor"]["min_margin"].as_f64().expect("min_margin");
            assert!((min_margin - 0.9).abs() < 0.001, "{min_margin}");
        });
        handle.join().expect("server thread");
    }

    #[test]
    fn a_failed_call_yields_fallback_actions_and_a_counted_error() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (url, handle) = multi_shot_server(500, "server error", 1);
        let case = write_case(dir.path(), "case.json", &inject_case());
        with_env(&base_env(dir.path(), &url, "FAILED_CALL_1091"), || {
            let mut out = Vec::new();
            let code = run_probe("inject", &case, 1, Some(dir.path()), &mut out).expect("run");
            assert_eq!(code, 0, "{}", String::from_utf8_lossy(&out));
            let value: serde_json::Value = serde_json::from_slice(&out).expect("json");
            assert_eq!(value["errors"], 1);
            assert_eq!(value["reps"][0]["actions"]["defer"], "inject_now");
            assert!(value["reps"][0]["error"].is_string());
        });
        handle.join().expect("server thread");
    }

    #[test]
    fn cache_disabled_means_k_reps_is_k_http_requests() {
        let dir = tempfile::tempdir().expect("tempdir");
        let body = serde_json::json!({
            "model": "jev-1.13.0",
            "answers": {"defer": {"type": "noul", "noul": 0.1}},
            "usage": {"input_tokens": 5, "output_tokens": 1},
        })
        .to_string();
        // 3 reps must mean 3 HTTP requests -- multi_shot_server only accepts
        // exactly `calls` connections and its thread never returns early, so
        // a cache hit (fewer than 3 real requests) would leave this .join()
        // hanging until the harness's own test timeout; a cache_ttl_secs
        // regression that starts reading/writing the cache would surface
        // here first.
        let (url, handle) = multi_shot_server(200, Box::leak(body.into_boxed_str()), 3);
        let case = write_case(dir.path(), "case.json", &inject_case());
        // A nonzero CONFIGURED cache_ttl_secs must still be forced to 0 by
        // the probe -- proves the override, not just the compiled default.
        let mut vars = base_env(dir.path(), &url, "CACHE_DISABLED_1091");
        vars.push(("ZIRV_CTX_JEV_CACHE_TTL_SECS", "86400".to_string()));
        with_env(&vars, || {
            let mut out = Vec::new();
            let code = run_probe("inject", &case, 3, Some(dir.path()), &mut out).expect("run");
            assert_eq!(code, 0, "{}", String::from_utf8_lossy(&out));
            let value: serde_json::Value = serde_json::from_slice(&out).expect("json");
            assert_eq!(value["calls"], 3);
            assert_eq!(value["errors"], 0);
            assert_eq!(value["reps"].as_array().expect("reps").len(), 3);
        });
        handle.join().expect("server thread");
    }
}
