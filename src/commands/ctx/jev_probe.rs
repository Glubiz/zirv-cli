//! `zirv ctx jev probe` (issue: autoresearch Jev-floor determinism
//! campaign): asks ONE Jev site's real production question(s) for a fixture
//! input K times with the cache disabled, applies that site's production
//! floor (`jev::floor`, itself honouring the operator's `[jev.floors.<site>]`
//! config and `ZIRV_CTX_JEV_FLOOR_<SITE>_MIN_CONFIDENCE|_MIN_MARGIN` env
//! overlays -- see `config::JevFloorsConfig` -- for the nine sites that have
//! one; the remaining sites have no `[jev.floors]` entry at all and keep
//! their own compiled constant, exactly as production does) and production
//! answer-to-action rule, and prints what production would have DONE on
//! each rep.
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
//! Two env vars are read ONLY here, never by production (see
//! `probe_floor_override`'s own doc comment): `ZIRV_CTX_JEV_PROBE_MIN_
//! CONFIDENCE`/`ZIRV_CTX_JEV_PROBE_MIN_MARGIN`, each a float in `[0, 1]`.
//! When set, each REPLACES the corresponding field of whatever site's
//! resolved floor is being probed (every site this module measures) after
//! any `[jev.floors]`/`ZIRV_CTX_JEV_FLOOR_*` overlay is already applied; an
//! invalid value (not a float, or outside `[0, 1]`) exits 2 before any Jev
//! call is made. The output's own `floor` object always reports the
//! post-override effective values.
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
use super::{
    compile, exec, handoff, hook, inject_gate, inject_screen, jev, memory, run_loop, safety, task,
};
use crate::commands::ctx::CtxResult;
use crate::commands::workflow::{profile, team};

/// One measurable site. The first twelve variants (`MemoryRerank` through
/// `Inject`) are the original twelve named in the probe's own CLI contract;
/// `Crash` through `InjectScreen` are six more added by the autoresearch
/// probe-contract extension (six further sites -- `missing-tests`,
/// `stop-verify`, `review-disposition`, `review-dedup`,
/// `artifact-substance`, `gate-reclass` -- are wired in by a later change).
/// `MemoryRerank`/`MemoryHarvest` and `ContextReport`/`ContextSkill` share a
/// production advise-site LABEL and/or `jev::FloorSite`, but are distinct
/// SITEs here: each has its own default floor constant a later retune
/// targets independently.
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
    Crash,
    Judge,
    ApproveEscalate,
    ApproveLower,
    IntakePlan,
    InjectScreen,
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
            "crash" => Self::Crash,
            "judge" => Self::Judge,
            "approve-escalate" => Self::ApproveEscalate,
            "approve-lower" => Self::ApproveLower,
            "intake-plan" => Self::IntakePlan,
            "inject-screen" => Self::InjectScreen,
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
            Self::Crash => "crash",
            Self::Judge => "judge",
            Self::ApproveEscalate | Self::ApproveLower => "approve",
            Self::IntakePlan => "intake_plan",
            Self::InjectScreen => "inject_screen",
        }
    }

    /// The `jev::FloorSite` this site's floor is resolved through, and its
    /// `[jev.floors.<name>]`/`ZIRV_CTX_JEV_FLOOR_<NAME>_*` config name --
    /// `None` for a site whose floor is a fixed compiled constant with no
    /// operator overlay at all (every site added by the probe-contract
    /// extension; production never calls `jev::floor` for these either), in
    /// which case the name is reported as `floor_site` purely for the
    /// output's own readability.
    fn floor_site(self) -> (Option<jev::FloorSite>, &'static str) {
        match self {
            Self::MemoryRerank | Self::MemoryHarvest => (Some(jev::FloorSite::Memory), "memory"),
            Self::ContextReport | Self::ContextSkill => (Some(jev::FloorSite::Context), "context"),
            Self::HarvestScreen => (Some(jev::FloorSite::HarvestScreen), "harvest_screen"),
            Self::HandoffThin | Self::HandoffSelect => {
                (Some(jev::FloorSite::HandoffSelect), "handoff_select")
            }
            Self::CompactionSelect => (Some(jev::FloorSite::CompactionSelect), "compaction_select"),
            Self::Dispatch => (Some(jev::FloorSite::Dispatch), "dispatch"),
            Self::LaunchEffort => (Some(jev::FloorSite::LaunchEffort), "launch_effort"),
            Self::ClassifyDomain => (Some(jev::FloorSite::Classify), "classify"),
            Self::Inject => (Some(jev::FloorSite::Inject), "inject"),
            Self::Crash => (None, "crash"),
            Self::Judge => (None, "judge"),
            Self::ApproveEscalate => (None, "approve_escalate"),
            Self::ApproveLower => (None, "approve_lower"),
            Self::IntakePlan => (None, "intake_plan"),
            Self::InjectScreen => (None, "inject_screen"),
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
            Self::Crash => (task::CRASH_TRIAGE_FLOOR, jev::DEFAULT_MIN_MARGIN),
            Self::Judge => (run_loop::JUDGE_CONTINUE_FLOOR, jev::DEFAULT_MIN_MARGIN),
            Self::ApproveEscalate => (
                safety::APPROVE_ESCALATE_MIN_CONFIDENCE,
                jev::DEFAULT_MIN_MARGIN,
            ),
            Self::ApproveLower => (
                safety::APPROVE_ALLOW_MIN_CONFIDENCE,
                safety::APPROVE_ALLOW_MIN_MARGIN,
            ),
            Self::IntakePlan => (
                team::INTAKE_PLAN_MIN_CONFIDENCE,
                team::INTAKE_PLAN_MIN_MARGIN,
            ),
            Self::InjectScreen => (
                inject_screen::INJECT_SCREEN_MIN_CONFIDENCE,
                jev::DEFAULT_MIN_MARGIN,
            ),
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
            Self::Crash => Ok((
                vec![task::crash_cause_question()],
                vec!["cause".to_string()],
            )),
            Self::Judge => Ok((
                vec![run_loop::judge_continue_question()],
                vec!["verdict".to_string()],
            )),
            Self::ApproveEscalate => Ok((
                vec![safety::approve_escalate_question()],
                vec!["risk".to_string()],
            )),
            Self::ApproveLower => Ok((
                vec![safety::approve_lower_question()],
                vec!["safe".to_string()],
            )),
            Self::IntakePlan => Ok((
                vec![team::intake_plan_question()],
                vec!["planner_distinct".to_string()],
            )),
            Self::InjectScreen => Ok((
                vec![inject_screen::inject_screen_question()],
                vec!["injection".to_string()],
            )),
        }
    }

    /// The exact production answer-to-action rule for this site, applied to
    /// one item's answer (or `None` when the item id is missing from the
    /// response). `case` is used only by [`Site::Crash`], whose rule also
    /// reads the local `access`/`configuration`/`missing_file` signal facts
    /// production sends alongside the question -- see [`facts_row0`].
    fn action(
        self,
        case: &Case,
        answer: Option<&jev::Answer>,
        min_confidence: f32,
        min_margin: f32,
    ) -> String {
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
            Self::Crash => {
                // facts[0] layout (see task.rs's own `jev_crash_cause`):
                // [exit_kind, attempt, max_attempts, access, configuration,
                // missing_file, transient] -- indices 3, 4, 5 are the three
                // signal bools the classification rule also reads.
                let facts = facts_row0(case);
                let access = facts.get(3).copied().unwrap_or(0) != 0;
                let configuration = facts.get(4).copied().unwrap_or(0) != 0;
                let missing_file = facts.get(5).copied().unwrap_or(0) != 0;
                match task::crash_cause_classify(
                    answer,
                    min_confidence,
                    min_margin,
                    access,
                    configuration,
                    missing_file,
                ) {
                    Some("access") | Some("deterministic") => "auto_block",
                    _ => "baseline",
                }
            }
            Self::Judge => run_loop::judge_continue_action(answer, min_confidence, min_margin),
            Self::ApproveEscalate => {
                safety::approve_escalate_action(answer, min_confidence, min_margin)
            }
            Self::ApproveLower => safety::approve_lower_action(answer, min_confidence, min_margin),
            Self::IntakePlan => team::intake_plan_action(answer, min_confidence, min_margin),
            Self::InjectScreen => {
                inject_screen::inject_screen_action(answer, min_confidence, min_margin)
            }
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
            Self::Crash => "baseline",
            Self::Judge => "helper",
            Self::ApproveEscalate => "allow",
            Self::ApproveLower => "ask",
            Self::IntakePlan => "keep_planner",
            Self::InjectScreen => "pass",
        }
    }
}

fn numbered_ids(prefix: &str, n: usize) -> Vec<String> {
    (0..n).map(|i| format!("{prefix}{i}")).collect()
}

/// The `facts[0]` row of a `case.state` shaped like a `_zirv_metadata_only`
/// advise-state struct (`{"_zirv_metadata_only": ..., "facts": [[...]]}`) --
/// used only by [`Site::Crash`]'s action rule, which (per the probe
/// contract) reads local signal facts back out of the same `state` object
/// the case already sends to Jev, rather than the probe recomputing them
/// independently. Any missing/malformed shape yields an empty row, which
/// [`Site::action`]'s `Crash` arm reads as every signal bool being unset.
fn facts_row0(case: &Case) -> Vec<u32> {
    case.state
        .get("facts")
        .and_then(serde_json::Value::as_array)
        .and_then(|rows| rows.first())
        .and_then(serde_json::Value::as_array)
        .map(|values| {
            values
                .iter()
                .filter_map(serde_json::Value::as_u64)
                .map(|value| value as u32)
                .collect()
        })
        .unwrap_or_default()
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

/// Parses one of the two probe-only floor overrides -- `ZIRV_CTX_JEV_PROBE_
/// MIN_CONFIDENCE`/`ZIRV_CTX_JEV_PROBE_MIN_MARGIN` (see this module's own
/// doc comment) -- through the same `env` closure [`run_probe`] already
/// threads to `CtxConfig::load`, never `std::env` directly in this
/// function, so a test can inject any value without touching real process
/// env. `Ok(None)` when the key is unset; `Err` is the exact message to
/// print before exiting 2 for a value that is not a float, or outside
/// `[0, 1]`.
fn probe_floor_override(
    env: &impl Fn(&str) -> Option<String>,
    key: &str,
) -> Result<Option<f32>, String> {
    let Some(raw) = env(key) else {
        return Ok(None);
    };
    let value: f32 = raw
        .trim()
        .parse()
        .map_err(|_| format!("jev probe: {key} must be a number in [0, 1], got {raw:?}"))?;
    if !(0.0..=1.0).contains(&value) {
        return Err(format!(
            "jev probe: {key} must be within [0, 1], got {raw:?}"
        ));
    }
    Ok(Some(value))
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

    let env = |key: &str| std::env::var(key).ok();
    let probe_min_confidence = match probe_floor_override(&env, "ZIRV_CTX_JEV_PROBE_MIN_CONFIDENCE")
    {
        Ok(value) => value,
        Err(message) => {
            writeln!(writer, "{message}")?;
            return Ok(2);
        }
    };
    let probe_min_margin = match probe_floor_override(&env, "ZIRV_CTX_JEV_PROBE_MIN_MARGIN") {
        Ok(value) => value,
        Err(message) => {
            writeln!(writer, "{message}")?;
            return Ok(2);
        }
    };

    let resolved_repo = match repo {
        Some(repo) => repo.to_path_buf(),
        None => std::env::current_dir()?,
    };
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
    let (mut min_confidence, mut min_margin) = match floor_site {
        Some(floor_site) => jev::floor(&cfg, floor_site, default_confidence, default_margin),
        None => (default_confidence, default_margin),
    };
    if let Some(value) = probe_min_confidence {
        min_confidence = value;
    }
    if let Some(value) = probe_min_margin {
        min_margin = value;
    }

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
                        site.action(&case, answers.get(id), min_confidence, min_margin),
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

    // -- Probe-contract extension: crash, judge, approve-escalate,
    // approve-lower, intake-plan, inject-screen ------------------------

    // `serde_json::json!`'s object keys must be string literals (a runtime
    // `item_id` needs the `answers` map built by hand instead of the macro's
    // `{ item_id: ... }` shorthand, which would otherwise take `item_id`
    // literally as the key text).
    fn choice_response_body(item_id: &str, choice: &str, probabilities: &[(&str, f64)]) -> String {
        let probs: serde_json::Map<String, serde_json::Value> = probabilities
            .iter()
            .map(|(key, value)| ((*key).to_string(), serde_json::json!(value)))
            .collect();
        let confidence = probabilities
            .iter()
            .find(|(key, _)| *key == choice)
            .map(|(_, value)| *value)
            .unwrap_or(0.0);
        let answer = serde_json::json!({
            "type": "choice",
            "choice": choice,
            "confidence": confidence,
            "probabilities": probs,
        });
        let mut answers = serde_json::Map::new();
        answers.insert(item_id.to_string(), answer);
        serde_json::json!({
            "model": "jev-1.13.0",
            "answers": answers,
            "usage": {"input_tokens": 5, "output_tokens": 1},
        })
        .to_string()
    }

    fn noul_response_body(item_id: &str, value: f64) -> String {
        let mut answers = serde_json::Map::new();
        answers.insert(
            item_id.to_string(),
            serde_json::json!({"type": "noul", "noul": value}),
        );
        serde_json::json!({
            "model": "jev-1.13.0",
            "answers": answers,
            "usage": {"input_tokens": 5, "output_tokens": 1},
        })
        .to_string()
    }

    // facts[0] layout: [exit_kind, attempt, max_attempts, access,
    // configuration, missing_file, transient] -- see task.rs's own
    // `jev_crash_cause`/`CrashAdviseState`.
    fn crash_case(access: u32, configuration: u32, missing_file: u32) -> serde_json::Value {
        serde_json::json!({
            "id": "c1",
            "state": {
                "_zirv_metadata_only": true,
                "facts": [[1, 1, 3, access, configuration, missing_file, 0]],
            },
        })
    }

    #[test]
    fn crash_reports_auto_block_for_a_decisive_corroborated_access_answer_and_baseline_for_transient()
     {
        let dir = tempfile::tempdir().expect("tempdir");
        let body = choice_response_body(
            "cause",
            "access",
            &[
                ("access", 0.95),
                ("transient", 0.03),
                ("deterministic", 0.02),
            ],
        );
        let (url, handle) = multi_shot_server(200, Box::leak(body.into_boxed_str()), 1);
        let case = write_case(dir.path(), "case.json", &crash_case(1, 0, 0));
        with_env(&base_env(dir.path(), &url, "CRASH_ACCESS_1091"), || {
            let mut out = Vec::new();
            let code = run_probe("crash", &case, 1, Some(dir.path()), &mut out).expect("run");
            assert_eq!(code, 0, "{}", String::from_utf8_lossy(&out));
            let value: serde_json::Value = serde_json::from_slice(&out).expect("json");
            assert_eq!(value["reps"][0]["actions"]["cause"], "auto_block");
        });
        handle.join().expect("server thread");

        let dir = tempfile::tempdir().expect("tempdir");
        let body = choice_response_body(
            "cause",
            "transient",
            &[
                ("transient", 0.95),
                ("access", 0.03),
                ("deterministic", 0.02),
            ],
        );
        let (url, handle) = multi_shot_server(200, Box::leak(body.into_boxed_str()), 1);
        let case = write_case(dir.path(), "case.json", &crash_case(1, 0, 0));
        with_env(&base_env(dir.path(), &url, "CRASH_TRANSIENT_1091"), || {
            let mut out = Vec::new();
            let code = run_probe("crash", &case, 1, Some(dir.path()), &mut out).expect("run");
            assert_eq!(code, 0, "{}", String::from_utf8_lossy(&out));
            let value: serde_json::Value = serde_json::from_slice(&out).expect("json");
            assert_eq!(value["reps"][0]["actions"]["cause"], "baseline");
        });
        handle.join().expect("server thread");
    }

    fn judge_case() -> serde_json::Value {
        serde_json::json!({
            "id": "c1",
            "state": {
                "_zirv_metadata_only": true,
                "facts": [[1, 1, 0, 1, 1, 3]],
            },
        })
    }

    #[test]
    fn judge_reports_continue_for_a_decisive_continue_answer_and_helper_otherwise() {
        let dir = tempfile::tempdir().expect("tempdir");
        let body = choice_response_body(
            "verdict",
            "continue",
            &[
                ("continue", 0.9),
                ("done", 0.05),
                ("blocked", 0.03),
                ("wait", 0.02),
            ],
        );
        let (url, handle) = multi_shot_server(200, Box::leak(body.into_boxed_str()), 1);
        let case = write_case(dir.path(), "case.json", &judge_case());
        with_env(&base_env(dir.path(), &url, "JUDGE_CONTINUE_1091"), || {
            let mut out = Vec::new();
            let code = run_probe("judge", &case, 1, Some(dir.path()), &mut out).expect("run");
            assert_eq!(code, 0, "{}", String::from_utf8_lossy(&out));
            let value: serde_json::Value = serde_json::from_slice(&out).expect("json");
            assert_eq!(value["reps"][0]["actions"]["verdict"], "continue");
        });
        handle.join().expect("server thread");

        let dir = tempfile::tempdir().expect("tempdir");
        let body = choice_response_body(
            "verdict",
            "done",
            &[
                ("done", 0.9),
                ("continue", 0.05),
                ("blocked", 0.03),
                ("wait", 0.02),
            ],
        );
        let (url, handle) = multi_shot_server(200, Box::leak(body.into_boxed_str()), 1);
        let case = write_case(dir.path(), "case.json", &judge_case());
        with_env(&base_env(dir.path(), &url, "JUDGE_DONE_1091"), || {
            let mut out = Vec::new();
            let code = run_probe("judge", &case, 1, Some(dir.path()), &mut out).expect("run");
            assert_eq!(code, 0, "{}", String::from_utf8_lossy(&out));
            let value: serde_json::Value = serde_json::from_slice(&out).expect("json");
            assert_eq!(value["reps"][0]["actions"]["verdict"], "helper");
        });
        handle.join().expect("server thread");
    }

    // facts[0] layout: [program class, subcommand class, writes, deletes,
    // network, privilege-escalation, path-scope class, pipe count, redirect
    // count, substitution count, secret-placeholder count, wrapper flag] --
    // see safety.rs's own `jev_approve_facts`.
    fn approve_case() -> serde_json::Value {
        serde_json::json!({
            "id": "c1",
            "state": {
                "_zirv_metadata_only": true,
                "facts": [[1, 0, 1, 0, 0, 0, 2, 0, 0, 0, 0, 0]],
            },
        })
    }

    #[test]
    fn approve_escalate_reports_ask_for_a_decisive_risky_answer_and_allow_otherwise() {
        let dir = tempfile::tempdir().expect("tempdir");
        let body = choice_response_body("risk", "risky", &[("risky", 0.9), ("safe", 0.1)]);
        let (url, handle) = multi_shot_server(200, Box::leak(body.into_boxed_str()), 1);
        let case = write_case(dir.path(), "case.json", &approve_case());
        with_env(
            &base_env(dir.path(), &url, "APPROVE_ESCALATE_ASK_1091"),
            || {
                let mut out = Vec::new();
                let code = run_probe("approve-escalate", &case, 1, Some(dir.path()), &mut out)
                    .expect("run");
                assert_eq!(code, 0, "{}", String::from_utf8_lossy(&out));
                let value: serde_json::Value = serde_json::from_slice(&out).expect("json");
                assert_eq!(value["reps"][0]["actions"]["risk"], "ask");
            },
        );
        handle.join().expect("server thread");

        let dir = tempfile::tempdir().expect("tempdir");
        let body = choice_response_body("risk", "safe", &[("safe", 0.9), ("risky", 0.1)]);
        let (url, handle) = multi_shot_server(200, Box::leak(body.into_boxed_str()), 1);
        let case = write_case(dir.path(), "case.json", &approve_case());
        with_env(
            &base_env(dir.path(), &url, "APPROVE_ESCALATE_ALLOW_1091"),
            || {
                let mut out = Vec::new();
                let code = run_probe("approve-escalate", &case, 1, Some(dir.path()), &mut out)
                    .expect("run");
                assert_eq!(code, 0, "{}", String::from_utf8_lossy(&out));
                let value: serde_json::Value = serde_json::from_slice(&out).expect("json");
                assert_eq!(value["reps"][0]["actions"]["risk"], "allow");
            },
        );
        handle.join().expect("server thread");
    }

    #[test]
    fn approve_lower_reports_allow_for_a_decisive_safe_answer_and_ask_otherwise() {
        let dir = tempfile::tempdir().expect("tempdir");
        let body = choice_response_body("safe", "safe", &[("safe", 0.95), ("unsafe", 0.05)]);
        let (url, handle) = multi_shot_server(200, Box::leak(body.into_boxed_str()), 1);
        let case = write_case(dir.path(), "case.json", &approve_case());
        with_env(
            &base_env(dir.path(), &url, "APPROVE_LOWER_ALLOW_1091"),
            || {
                let mut out = Vec::new();
                let code =
                    run_probe("approve-lower", &case, 1, Some(dir.path()), &mut out).expect("run");
                assert_eq!(code, 0, "{}", String::from_utf8_lossy(&out));
                let value: serde_json::Value = serde_json::from_slice(&out).expect("json");
                assert_eq!(value["reps"][0]["actions"]["safe"], "allow");
            },
        );
        handle.join().expect("server thread");

        let dir = tempfile::tempdir().expect("tempdir");
        let body = choice_response_body("safe", "unsafe", &[("unsafe", 0.95), ("safe", 0.05)]);
        let (url, handle) = multi_shot_server(200, Box::leak(body.into_boxed_str()), 1);
        let case = write_case(dir.path(), "case.json", &approve_case());
        with_env(
            &base_env(dir.path(), &url, "APPROVE_LOWER_ASK_1091"),
            || {
                let mut out = Vec::new();
                let code =
                    run_probe("approve-lower", &case, 1, Some(dir.path()), &mut out).expect("run");
                assert_eq!(code, 0, "{}", String::from_utf8_lossy(&out));
                let value: serde_json::Value = serde_json::from_slice(&out).expect("json");
                assert_eq!(value["reps"][0]["actions"]["safe"], "ask");
            },
        );
        handle.join().expect("server thread");
    }

    // facts[0] layout: [site=1, intent, risk, seats, dependency edges,
    // implementers, independent review, any implementer has scoped claims]
    // -- see workflow/team.rs's own `plan_advisory_state`.
    fn intake_plan_case() -> serde_json::Value {
        serde_json::json!({
            "id": "c1",
            "state": {
                "_zirv_metadata_only": true,
                "facts": [[1, 0, 1, 3, 2, 2, 1, 1]],
            },
        })
    }

    #[test]
    fn intake_plan_reports_omit_planner_for_a_decisive_low_noul_and_keep_planner_otherwise() {
        let dir = tempfile::tempdir().expect("tempdir");
        let body = noul_response_body("planner_distinct", 0.02);
        let (url, handle) = multi_shot_server(200, Box::leak(body.into_boxed_str()), 1);
        let case = write_case(dir.path(), "case.json", &intake_plan_case());
        with_env(&base_env(dir.path(), &url, "INTAKE_PLAN_OMIT_1091"), || {
            let mut out = Vec::new();
            let code = run_probe("intake-plan", &case, 1, Some(dir.path()), &mut out).expect("run");
            assert_eq!(code, 0, "{}", String::from_utf8_lossy(&out));
            let value: serde_json::Value = serde_json::from_slice(&out).expect("json");
            assert_eq!(
                value["reps"][0]["actions"]["planner_distinct"],
                "omit_planner"
            );
        });
        handle.join().expect("server thread");

        let dir = tempfile::tempdir().expect("tempdir");
        let body = noul_response_body("planner_distinct", 0.98);
        let (url, handle) = multi_shot_server(200, Box::leak(body.into_boxed_str()), 1);
        let case = write_case(dir.path(), "case.json", &intake_plan_case());
        with_env(&base_env(dir.path(), &url, "INTAKE_PLAN_KEEP_1091"), || {
            let mut out = Vec::new();
            let code = run_probe("intake-plan", &case, 1, Some(dir.path()), &mut out).expect("run");
            assert_eq!(code, 0, "{}", String::from_utf8_lossy(&out));
            let value: serde_json::Value = serde_json::from_slice(&out).expect("json");
            assert_eq!(
                value["reps"][0]["actions"]["planner_distinct"],
                "keep_planner"
            );
        });
        handle.join().expect("server thread");
    }

    // facts[0] layout: [override-instruction marker count,
    // role/tag-lookalike marker count, imperative-line count, URL count,
    // credential-path mention count, long opaque blob count, content size
    // bucket 0-4, source 0=mail/1=worker-result] -- see inject_screen.rs's
    // own `screen_for_injection`.
    fn inject_screen_case() -> serde_json::Value {
        serde_json::json!({
            "id": "c1",
            "state": {
                "_zirv_metadata_only": true,
                "facts": [[3, 2, 1, 0, 0, 0, 2, 0]],
            },
        })
    }

    #[test]
    fn inject_screen_reports_warn_for_a_decisive_high_noul_and_pass_for_an_uncertain_answer() {
        let dir = tempfile::tempdir().expect("tempdir");
        let body = noul_response_body("injection", 0.95);
        let (url, handle) = multi_shot_server(200, Box::leak(body.into_boxed_str()), 1);
        let case = write_case(dir.path(), "case.json", &inject_screen_case());
        with_env(
            &base_env(dir.path(), &url, "INJECT_SCREEN_WARN_1091"),
            || {
                let mut out = Vec::new();
                let code =
                    run_probe("inject-screen", &case, 1, Some(dir.path()), &mut out).expect("run");
                assert_eq!(code, 0, "{}", String::from_utf8_lossy(&out));
                let value: serde_json::Value = serde_json::from_slice(&out).expect("json");
                assert_eq!(value["reps"][0]["actions"]["injection"], "warn");
            },
        );
        handle.join().expect("server thread");

        let dir = tempfile::tempdir().expect("tempdir");
        let body = noul_response_body("injection", 0.5);
        let (url, handle) = multi_shot_server(200, Box::leak(body.into_boxed_str()), 1);
        let case = write_case(dir.path(), "case.json", &inject_screen_case());
        with_env(
            &base_env(dir.path(), &url, "INJECT_SCREEN_PASS_1091"),
            || {
                let mut out = Vec::new();
                let code =
                    run_probe("inject-screen", &case, 1, Some(dir.path()), &mut out).expect("run");
                assert_eq!(code, 0, "{}", String::from_utf8_lossy(&out));
                let value: serde_json::Value = serde_json::from_slice(&out).expect("json");
                assert_eq!(value["reps"][0]["actions"]["injection"], "pass");
            },
        );
        handle.join().expect("server thread");
    }

    /// `approve-escalate` has no `[jev.floors.<site>]` entry at all (its
    /// floor is a fixed compiled constant -- see `Site::floor_site`'s own
    /// doc comment), proving the probe-only override applies even to a site
    /// the existing `[jev.floors]`/`ZIRV_CTX_JEV_FLOOR_*` overlay can never
    /// reach.
    #[test]
    fn probe_min_margin_override_flips_a_decisive_approve_escalate_answer_to_fallback() {
        let case_state = approve_case();
        let body = choice_response_body("risk", "risky", &[("risky", 0.9), ("safe", 0.1)]);

        let default_dir = tempfile::tempdir().expect("tempdir");
        let (url, handle) = multi_shot_server(200, Box::leak(body.clone().into_boxed_str()), 1);
        let case = write_case(default_dir.path(), "case.json", &case_state);
        with_env(
            &base_env(default_dir.path(), &url, "PROBE_OVERRIDE_DEFAULT_1091"),
            || {
                let mut out = Vec::new();
                let code = run_probe(
                    "approve-escalate",
                    &case,
                    1,
                    Some(default_dir.path()),
                    &mut out,
                )
                .expect("run");
                assert_eq!(code, 0, "{}", String::from_utf8_lossy(&out));
                let value: serde_json::Value = serde_json::from_slice(&out).expect("json");
                assert_eq!(value["reps"][0]["actions"]["risk"], "ask");
                let min_margin = value["floor"]["min_margin"].as_f64().expect("min_margin");
                assert!((min_margin - 0.2).abs() < 0.001, "{min_margin}");
            },
        );
        handle.join().expect("server thread");

        let raised_dir = tempfile::tempdir().expect("tempdir");
        let (url, handle) = multi_shot_server(200, Box::leak(body.into_boxed_str()), 1);
        let case = write_case(raised_dir.path(), "case.json", &case_state);
        let mut vars = base_env(raised_dir.path(), &url, "PROBE_OVERRIDE_RAISED_1091");
        vars.push(("ZIRV_CTX_JEV_PROBE_MIN_MARGIN", "0.95".to_string()));
        with_env(&vars, || {
            let mut out = Vec::new();
            let code = run_probe(
                "approve-escalate",
                &case,
                1,
                Some(raised_dir.path()),
                &mut out,
            )
            .expect("run");
            assert_eq!(code, 0, "{}", String::from_utf8_lossy(&out));
            let value: serde_json::Value = serde_json::from_slice(&out).expect("json");
            assert_eq!(value["reps"][0]["actions"]["risk"], "allow");
            let min_margin = value["floor"]["min_margin"].as_f64().expect("min_margin");
            assert!((min_margin - 0.95).abs() < 0.001, "{min_margin}");
        });
        handle.join().expect("server thread");
    }

    #[test]
    fn probe_floor_override_env_rejects_an_out_of_range_or_unparsable_value_with_exit_2() {
        let dir = tempfile::tempdir().expect("tempdir");
        let case = write_case(dir.path(), "case.json", &approve_case());

        with_env(
            &[(
                "ZIRV_CTX_JEV_PROBE_MIN_CONFIDENCE",
                "not-a-float".to_string(),
            )],
            || {
                let mut out = Vec::new();
                let code = run_probe("approve-escalate", &case, 1, Some(dir.path()), &mut out)
                    .expect("run");
                assert_eq!(code, 2, "{}", String::from_utf8_lossy(&out));
                let text = String::from_utf8(out).expect("utf8");
                assert!(text.contains("ZIRV_CTX_JEV_PROBE_MIN_CONFIDENCE"), "{text}");
            },
        );

        with_env(
            &[("ZIRV_CTX_JEV_PROBE_MIN_MARGIN", "1.5".to_string())],
            || {
                let mut out = Vec::new();
                let code = run_probe("approve-escalate", &case, 1, Some(dir.path()), &mut out)
                    .expect("run");
                assert_eq!(code, 2, "{}", String::from_utf8_lossy(&out));
                let text = String::from_utf8(out).expect("utf8");
                assert!(text.contains("ZIRV_CTX_JEV_PROBE_MIN_MARGIN"), "{text}");
            },
        );
    }
}
