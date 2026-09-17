//! Issue #537 seam: the harness proxy's decision core.
//!
//! This module holds the pure building blocks `proxy::decide` (in `mod.rs`)
//! chains together: a deterministic [`baseline`] (reusing the existing
//! classifier/profile/selection seams, never a model call), the neutral
//! [`Question`]/[`Answer`] shapes both model deciders (`typesafe.rs`,
//! `llm.rs`) answer into, [`merge`] (confidence-gated, monotonic on
//! complexity/risk/execution), and [`validate`] (the roster-backed defense
//! that reverts an invalid harness/model/workflow choice to the baseline).
//!
//! Nothing here performs I/O beyond what the deterministic classifier
//! already does (`classify::from_args`'s own bounded Git measurement) and
//! the best-effort headroom/active-workflow reads [`build_intake`] folds in --
//! every one of those is wrapped so a failure degrades to an honest `None`
//! rather than propagating, since `proxy::decide` must never fail.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::commands::ctx::adapters;
use crate::commands::ctx::catalogue::{self, Tier};
use crate::commands::ctx::config::CtxConfig;
use crate::commands::ctx::handover;
use crate::commands::workflow::classify::{self, Classification, Complexity, Intent, RiskBand};
use crate::commands::workflow::engine;
use crate::commands::workflow::profile::{ExecutionMode, ExecutionProfile, ValidationProfile};
use crate::commands::workflow::selection;

/// Every choice-kind question is capped here, plus a reserved catch-all slot
/// (see [`cap_choice_options`]) -- the Jev API's own documented limit.
pub const MAX_CHOICE_OPTIONS: usize = 255;

/// The deterministic classifier's own task-text bound is smaller than
/// `[proxy] request_max_bytes`'s default; a request longer than this is
/// truncated before it ever reaches `classify::from_args`, so a long prompt
/// degrades to "classified from a prefix" rather than failing baseline
/// construction outright.
const CLASSIFY_TASK_MAX_BYTES: usize = 4000;

/// The orchestrator seat a decision names: a harness registry name
/// (`"claude"`, `"codex"`, ...) plus a model alias or id on that harness's
/// own vendor ladder.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Seat {
    pub harness: String,
    pub model: String,
}

/// Raw token counts from a model decider's own response, when it reported
/// one (`typesafe.rs` always does; `llm.rs`/the deterministic path never
/// do). Mirrors `event::TranscriptUsage`'s input/output split narrowly --
/// Jev bills input only, so there is no cache class to carry.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Usage {
    pub input_tokens: u64,
    pub output_tokens: u64,
}

/// Which decider actually produced this decision's non-baseline fields.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Decider {
    Typesafe,
    Helper,
    Deterministic,
}

/// Whether this decision routes to one seat doing the work directly, or a
/// full orchestrator setup (an orchestrator seat plus delegated workers).
/// Issue #537 field evidence: an operator experienced both a trivial,
/// one-place color change AND a bounded bugfix investigation as "the full
/// orchestrator setup", because nothing named the difference plainly. This
/// is that name -- derived once, in [`finalize_derived_fields`], from
/// `execution` alone (never asked as its own question): `Orchestrated` is
/// the only mode that actually compiles a team, so it alone maps to
/// `Orchestrator`; `Direct` and `Bounded` both stay on one seat.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SeatRole {
    Single,
    Orchestrator,
}

impl SeatRole {
    fn from_execution(execution: ExecutionMode) -> Self {
        match execution {
            ExecutionMode::Direct | ExecutionMode::Bounded => SeatRole::Single,
            ExecutionMode::Orchestrated => SeatRole::Orchestrator,
        }
    }
}

/// The generic tier the orchestrator SEAT ITSELF runs at -- issue #537 field
/// evidence problem (a): asking a `seat` question over every enabled
/// `harness/alias` pair spread probability across too many similar-looking
/// options for any answer to ever clear the confidence floor, so the launch
/// always fell back to the configured orchestrator model regardless of how
/// small the request was. A live 24-case Jev battery then showed that even a
/// four-option `seat_tier` question fared no better (any many-option seat/
/// tier question never cleared the floor, while its `execution` answers were
/// themselves unreliable, 17-74 confidence, calling architectural work
/// "direct") -- so `seat_tier` (like `execution`) is now derived, never
/// asked, from [`SeatTier::from_execution`]. `Frontier` is the top-of-fleet
/// tier `worker_tier`/[`super::catalogue::Tier`] deliberately has no
/// equivalent of: a delegated worker is never the orchestrator seat
/// compiling the team, so it never needs the top rung.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SeatTier {
    Cheap,
    Standard,
    Deep,
    Frontier,
}

impl SeatTier {
    /// Issue #537: the baseline maps directly from `execution` -- `Direct`
    /// needs no more than a cheap seat, `Bounded` a standard one, and only
    /// `Orchestrated` (a real compiled team) earns the frontier rung.
    fn from_execution(execution: ExecutionMode) -> Self {
        match execution {
            ExecutionMode::Direct => SeatTier::Cheap,
            ExecutionMode::Bounded => SeatTier::Standard,
            ExecutionMode::Orchestrated => SeatTier::Frontier,
        }
    }

    /// A short, stable, human-readable label -- used in `announce_line`/
    /// `prompt_layer` and matched against a Jev/helper choice answer.
    pub fn label(self) -> &'static str {
        match self {
            SeatTier::Cheap => "cheap",
            SeatTier::Standard => "standard",
            SeatTier::Deep => "deep",
            SeatTier::Frontier => "frontier",
        }
    }
}

/// One inspectable decision, printed by `zirv ctx proxy` and (T2) applied to
/// a `zirv chat` launch. Every field traces to a reason; `fallbacks` names
/// every decider that was tried and skipped before `decider` won.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProxyDecision {
    pub request_sha256: String,
    pub repo: PathBuf,
    pub intent: Intent,
    pub complexity: Complexity,
    pub risk: RiskBand,
    pub execution: ExecutionMode,
    pub seat_role: SeatRole,
    pub validation: ValidationProfile,
    pub workflow: Option<String>,
    pub orchestrator: Seat,
    pub seat_tier: SeatTier,
    pub worker_tier: Tier,
    pub needs_clarification: f32,
    pub decider: Decider,
    pub confidence: BTreeMap<String, f32>,
    pub reasons: Vec<String>,
    pub fallbacks: Vec<String>,
    pub elapsed_ms: u64,
    pub usage: Option<Usage>,
    pub created_at: u64,
}

/// The three question shapes the Jev API and the helper-model contract both
/// speak, per the docs (`typesafe.rs`'s own module doc has the wire shape).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QuestionKind {
    Choice,
    Score,
    Noul,
}

/// A question's own criteria, one shape per [`QuestionKind`]. `Choice`
/// pairs an option with an optional one-line description (`None` is a valid
/// Jev value); `Score` is an ordered list of level descriptions (index 0
/// first); `Noul` optionally names what `true`/`false` mean.
#[derive(Debug, Clone)]
pub enum Criteria {
    Choice(Vec<(String, Option<String>)>),
    Score(Vec<String>),
    Noul {
        when_true: Option<String>,
        when_false: Option<String>,
    },
}

/// A neutral, decider-agnostic question. Both `typesafe.rs` and `llm.rs`
/// consume the same `Vec<Question>` (from [`questions`]) and produce the
/// same [`Answers`] shape, so [`merge`] never needs to know which decider
/// answered.
#[derive(Debug, Clone)]
pub struct Question {
    pub id: String,
    pub kind: QuestionKind,
    pub instructions: String,
    pub criteria: Criteria,
}

/// One decider's answer to one question, already reduced to a single value
/// plus a confidence in `[0, 1]`. `Score`'s value is a continuous level
/// index (not necessarily an integer -- see `typesafe::to_answers`'s own
/// rounding rule); `Noul`'s value is the raw `true`-probability.
#[derive(Debug, Clone, PartialEq)]
pub enum AnswerValue {
    Choice(String),
    Score(f64),
    Noul(f64),
}

#[derive(Debug, Clone, PartialEq)]
pub struct Answer {
    pub value: AnswerValue,
    pub confidence: f32,
}

/// A full set of answers, keyed by [`Question::id`].
pub type Answers = BTreeMap<String, Answer>;

#[derive(Debug, Clone, Serialize)]
pub struct IntakeModel {
    pub alias: String,
    pub tier: Option<String>,
    pub strength: u8,
    pub input_usd_per_mtok: f64,
}

#[derive(Debug, Clone, Serialize)]
pub struct IntakeHarness {
    pub name: String,
    pub ready: bool,
    pub headroom_pct: Option<f64>,
    pub models: Vec<IntakeModel>,
}

#[derive(Debug, Clone, Serialize)]
pub struct IntakeWorkflow {
    pub id: String,
    pub description: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct IntakeRepository {
    pub name: String,
    /// The repository's own measured diff (uncommitted working-tree changes,
    /// or committed changes on this branch since its base) -- informational
    /// context for a model decider only, named so it is never mistaken for
    /// "how big is the request": the baseline classification never measures
    /// this (see `classify_request`'s own doc comment for why).
    pub uncommitted_or_branch_changes: BranchChanges,
    pub active_workflow: Option<String>,
    pub primary_extensions: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct BranchChanges {
    pub files: usize,
    pub lines: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct IntakePolicy {
    pub native_available: bool,
}

/// The Jev `state` payload (also what `questions` itself is built from).
/// Bounded and repository-neutral by construction: `request` is truncated to
/// `request_max_bytes`, `repository` carries counts and extensions rather
/// than paths or diffs, and neither this type nor anything that builds it
/// ever touches file contents or environment values.
#[derive(Debug, Clone, Serialize)]
pub struct IntakeState {
    pub request: String,
    pub repository: IntakeRepository,
    pub harnesses: Vec<IntakeHarness>,
    pub workflows: Vec<IntakeWorkflow>,
    pub policy: IntakePolicy,
}

/// One harness's proxy-relevant roster facts: whether it is currently
/// enabled+ready (`settings::AgentGate::is_enabled` plus `AgentAdapter::
/// ready`, the same `chat.rs::harness_list` shape without needing that
/// private function), and the vendor slug its catalogue rungs live under.
#[derive(Debug, Clone)]
pub struct RosterHarness {
    pub name: String,
    pub ready: bool,
    pub vendor: &'static str,
}

/// The enabled/ready harness roster and the loaded workflow registry, both
/// gathered once per `decide()` call and reused by [`baseline`],
/// [`build_intake`] and [`validate`]. Best-effort: a registry load failure
/// (a broken `.zirv/workflows/` layer, say) degrades to "no workflows known"
/// rather than propagating -- `decide()` must never fail.
#[derive(Debug, Clone)]
pub struct Roster {
    pub harnesses: Vec<RosterHarness>,
    pub registry: Option<crate::commands::workflow::registry::WorkflowRegistry>,
}

impl Roster {
    /// Gathers the roster for `repo` under `cfg`. Never fails: a harness
    /// whose readiness cannot be determined is simply not ready, and a
    /// registry that cannot load leaves `registry` at `None`.
    pub fn gather(cfg: &CtxConfig, repo: &Path) -> Self {
        let harnesses = adapters::ADAPTERS
            .iter()
            .map(|(name, ctor)| RosterHarness {
                name: (*name).to_string(),
                ready: cfg.agents.is_enabled(name)
                    && ctor(cfg.agent_bin.as_deref()).ready().is_ok(),
                vendor: adapters::provider_for_agent_name(Some(name)),
            })
            .collect();
        // `built_in_only: false`, matching `zirv workflow start`'s own
        // default: a proxy running against a repo with custom workflow packs
        // should see them, the same roster a real `workflow start` would.
        let registry = engine::load_workflow_registry(repo, false).ok();
        Self {
            harnesses,
            registry,
        }
    }

    pub fn harness(&self, name: &str) -> Option<&RosterHarness> {
        self.harnesses.iter().find(|h| h.name == name)
    }

    /// Every registered workflow id and its description, for the `workflow`
    /// question's criteria and for [`IntakeState::workflows`]. Empty when
    /// the registry failed to load.
    pub fn workflow_summaries(&self) -> Vec<IntakeWorkflow> {
        self.registry
            .as_ref()
            .map(|registry| {
                registry
                    .list()
                    .map(|entry| IntakeWorkflow {
                        id: entry.definition.id.clone(),
                        description: entry.definition.description.clone(),
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    pub fn workflow_exists(&self, id: &str) -> bool {
        self.registry
            .as_ref()
            .is_some_and(|registry| registry.get(id).is_ok())
    }
}

fn sha256_hex(text: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(text.as_bytes());
    hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// Truncates `text` to at most `max` bytes, at a `char` boundary -- never
/// splitting a multi-byte UTF-8 sequence.
fn truncate_bytes(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.to_string();
    }
    let mut end = max;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    text[..end].to_string()
}

fn tier_str(tier: Tier) -> &'static str {
    match tier {
        Tier::Cheap => "cheap",
        Tier::Standard => "standard",
        Tier::Deep => "deep",
    }
}

/// The deterministic classification behind a decision's baseline --
/// TEXT ONLY (issue #537 fix): classifies via `classify::classify` directly
/// with zero declared changed files/lines, never `classify::from_args`.
/// `from_args` always measures the repository's own diff against its base,
/// even on its "declared" branch (there only as a floor a declared scope
/// cannot talk down) -- so on a feature branch carrying thousands of lines
/// unrelated to the request being decided on right now, every request's
/// complexity/risk got inflated by that unrelated diff, and the merge
/// chain's own monotonic floor then forbade a model decider from ever
/// lowering it back down, defeating the point of asking one at all. `intent`
/// still comes from the request text (`classify::classify`'s own
/// `infer_intent`); `work_domain` and the path-based risk floor need real
/// paths and so stay at their text-only/no-signal defaults here -- `zirv ctx
/// proxy` decides BEFORE any code exists to measure, not after.
pub fn classify_request(request: &str) -> Classification {
    let task = truncate_bytes(request, CLASSIFY_TASK_MAX_BYTES);
    let mut classification = classify::classify(&classify::ClassificationInput {
        task,
        paths: Vec::new(),
        changed_lines: 0,
        tests_changed: true,
        intent_override: None,
        complexity_override: None,
        risk_override: None,
    })
    .expect("task truncated below classify's own byte limit; classify() cannot fail here");
    classification.reasons.push(
        "classification: request text only; repository diff not measured at intake".to_string(),
    );
    classification.reasons.sort();
    classification
}

/// The repository's own measured change surface (paths + total lines), for
/// [`IntakeState`]'s informational `uncommitted_or_branch_changes` field
/// ONLY -- never fed into [`classify_request`]'s baseline classification
/// (see that function's own doc comment for why). Best-effort: empty/`0`
/// outside a repository, on an unborn/no-commit repo, or on any other git
/// failure, the same fail-soft posture every other measurement in this
/// module holds to.
fn measured_branch_changes(repo: &Path) -> (Vec<PathBuf>, usize) {
    classify::git_change_input(repo, String::new())
        .map(|input| (input.paths, input.changed_lines))
        .unwrap_or_default()
}

/// Issue #537 field evidence problem (a): the orchestrator seat's model is
/// resolved from `seat_tier` alone, never asked or chosen as its own
/// harness/model question -- `harness` is always the baseline default
/// (unchanged by any decider); only the tier varies. `cheap`/`standard`/
/// `deep` go through `handover::resolve_model` (the same tier ladder,
/// operator overrides included, `zirv ctx handover` itself uses); `frontier`
/// is the operator's own configured `chat.model` when set, else the vendor's
/// own top rung -- there is no "frontier" tier in `handover`'s own
/// cheap/standard/deep ladder because that ladder is for delegated workers,
/// which never need the orchestrator's own top-of-fleet rung.
/// `handover::resolve_model` failing (an adapter with no tier ladder at all)
/// degrades to the same top-rung alias `frontier` itself falls back to,
/// rather than propagating -- `proxy::decide` must never fail.
fn model_for_tier(cfg: &CtxConfig, harness: &str, tier: SeatTier) -> String {
    match tier {
        SeatTier::Frontier => cfg
            .chat
            .model
            .clone()
            .filter(|model| !model.is_empty())
            .unwrap_or_else(|| top_rung_alias(harness)),
        SeatTier::Cheap | SeatTier::Standard | SeatTier::Deep => {
            handover::resolve_model(harness, tier.label(), cfg)
                .unwrap_or_else(|_| top_rung_alias(harness))
        }
    }
}

fn baseline_seat(cfg: &CtxConfig, seat_tier: SeatTier) -> Seat {
    match adapters::resolve_default(cfg) {
        Ok((adapter, _origin)) => {
            let harness = adapter.name().to_string();
            let model = model_for_tier(cfg, &harness, seat_tier);
            Seat { harness, model }
        }
        Err(_) => Seat {
            harness: cfg.agent.clone().unwrap_or_else(|| "claude".to_string()),
            model: cfg.chat.model.clone().unwrap_or_default(),
        },
    }
}

fn top_rung_alias(harness: &str) -> String {
    let vendor_slug = adapters::provider_for_agent_name(Some(harness));
    catalogue::vendor(vendor_slug)
        .and_then(|vendor| vendor.rungs.first())
        .map(|rung| rung.alias.to_string())
        .unwrap_or_default()
}

/// The pure, always-succeeding baseline: today's classifier/profile/
/// selection/adapter-default seam, with no model call. `merge` starts from
/// a clone of this and only ever raises complexity/risk/execution or
/// replaces a field with a confident, later-validated model answer.
pub fn baseline(
    cfg: &CtxConfig,
    repo: &Path,
    request: &str,
    classification: &Classification,
    roster: &Roster,
) -> ProxyDecision {
    let profile = ExecutionProfile::derive(request, classification);
    let workflow = match classification.complexity {
        Complexity::Trivial => None,
        _ => roster.registry.as_ref().map(|registry| {
            selection::select_definition(classification, registry, request).definition_id
        }),
    };
    let mut decision = ProxyDecision {
        request_sha256: sha256_hex(request),
        repo: repo.to_path_buf(),
        intent: classification.intent,
        complexity: classification.complexity,
        risk: classification.risk,
        // Placeholders: `finalize_derived_fields` overwrites every one of
        // execution/seat_role/seat_tier/worker_tier/orchestrator.model at
        // this function's own tail, from `complexity` alone -- see that
        // function's own doc comment for why derivation lives there and
        // nowhere else.
        execution: ExecutionMode::Direct,
        seat_role: SeatRole::Single,
        validation: profile.validation,
        workflow,
        orchestrator: baseline_seat(cfg, SeatTier::Cheap),
        seat_tier: SeatTier::Cheap,
        worker_tier: Tier::Cheap,
        needs_clarification: 0.0,
        decider: Decider::Deterministic,
        confidence: BTreeMap::new(),
        // Carries `classification.reasons` (including `classify_request`'s
        // own "request text only" note) so an operator reading `zirv ctx
        // proxy`'s output sees why the baseline landed where it did, not
        // just the deterministic decider silently disagreeing with what a
        // human might expect.
        reasons: classification.reasons.clone(),
        fallbacks: Vec::new(),
        elapsed_ms: 0,
        usage: None,
        created_at: 0,
    };
    apply_security_risk_floor(&mut decision);
    finalize_derived_fields(&mut decision, cfg);
    decision
}

/// Issue #537: with the baseline now text-only (see `classify_request`'s
/// own doc comment), the path-based sensitive-surface risk floor
/// `classify::classify` used to apply can no longer see any paths at
/// intake time -- so a request like "rotate the shared credential
/// constant" would otherwise obtain the fast path from wording alone,
/// exactly the property #537 must not lose. `validation.security_review`
/// is already text-driven (`ExecutionProfile::derive`'s own domain-signal
/// detection, independent of `risk`); when it is `true` and `risk` has not
/// already reached `High` some other way, this raises it -- which then
/// feeds [`apply_risk_execution_floor`] right after it in both call sites,
/// lifting `execution` to `Bounded` too. The one place this rule lives,
/// called from the tail of both [`baseline`] and [`merge`].
fn apply_security_risk_floor(decision: &mut ProxyDecision) {
    if decision.validation.security_review && decision.risk < RiskBand::High {
        decision.risk = RiskBand::High;
        decision
            .reasons
            .push("risk: raised to high because the request names a security surface".to_string());
    }
}

/// Issue #537 (battery finding): a sensitive-surface risk floor must also
/// floor execution, so a "small" request touching a sensitive path (a
/// one-line auth change, say) can never route as `Direct` on wording or
/// diff size alone -- `risk >= High` alone already forces independent/
/// security review in `validation`, but until this rule existed nothing
/// stopped `execution` from staying `Direct` regardless. The one place this
/// rule lives: called from [`finalize_derived_fields`] right after
/// `execution` is (re)derived from `complexity`, so it holds no matter which
/// decider produced `risk`/`complexity`.
fn apply_risk_execution_floor(decision: &mut ProxyDecision) {
    if decision.risk >= RiskBand::High
        && execution_rank(decision.execution) < execution_rank(ExecutionMode::Bounded)
    {
        decision.execution = ExecutionMode::Bounded;
        decision.reasons.push(
            "execution: raised to bounded because risk is high (sensitive paths)".to_string(),
        );
    }
}

/// Issue #537 field evidence problem (b): a `direct` execution answer must
/// never coexist with a gated `workflow` -- both of the operator's own live
/// complaints were exactly this pairing (a one-place color change and a
/// bounded bugfix investigation, each landing a `workflow` a `Direct`
/// execution has no business gating). One function, applied at the tail of
/// both [`baseline`] and [`merge`] (after every floor above it, so it reads
/// the FINAL `execution`): `Direct` clears `workflow` to `None` with a
/// recorded reason; `Bounded`/`Orchestrated` keep whatever the model or
/// baseline already chose.
fn apply_direct_execution_workflow_rule(decision: &mut ProxyDecision) {
    if decision.execution == ExecutionMode::Direct && decision.workflow.is_some() {
        decision.workflow = None;
        decision
            .reasons
            .push("workflow: none because execution is direct".to_string());
    }
}

/// Issue #537 design revision, from a live 24-case Jev battery run against
/// this decider: Jev's own `complexity`/`workflow` answers were reliable,
/// but its `execution` answers were not (17-74 confidence, calling
/// architectural work "direct"), and neither `execution` nor any many-option
/// seat/tier question ever cleared the confidence floor. `execution` is
/// therefore never asked at all -- it is this one deterministic mapping from
/// the (already merged/floored) `complexity`, the same mapping
/// `ExecutionProfile::derive` itself already used to compute its own
/// `execution` field.
fn execution_from_complexity(complexity: Complexity) -> ExecutionMode {
    match complexity {
        Complexity::Trivial => ExecutionMode::Direct,
        Complexity::Bounded => ExecutionMode::Bounded,
        Complexity::Substantial | Complexity::Architectural => ExecutionMode::Orchestrated,
    }
}

/// Issue #537 design revision: delegated workers only ever need a step up
/// from cheap when there is a real compiled team coordinating them
/// (`Orchestrated`) -- `Direct`/`Bounded` both stay on the cheap tier, since
/// a single seat handling its own bounded work has no delegated workers to
/// tier up in the first place.
fn worker_tier_from_execution(execution: ExecutionMode) -> Tier {
    match execution {
        ExecutionMode::Orchestrated => Tier::Standard,
        ExecutionMode::Direct | ExecutionMode::Bounded => Tier::Cheap,
    }
}

/// Derives every field that follows deterministically from the merged,
/// floor-raised `complexity`/`risk` alone: `execution` (from `complexity`,
/// then floored by `risk` via [`apply_risk_execution_floor`]), the
/// direct-execution/workflow rule, `seat_tier`/`worker_tier`/`seat_role`
/// (from the final `execution`), and the orchestrator's own resolved
/// `model` (from `seat_tier` via `handover::resolve_model`, see
/// [`model_for_tier`]). The ONE place all of this is computed, called at
/// the tail of both [`baseline`] and [`merge`], after
/// [`apply_security_risk_floor`] has already had its say on `risk`.
fn finalize_derived_fields(decision: &mut ProxyDecision, cfg: &CtxConfig) {
    decision.execution = execution_from_complexity(decision.complexity);
    let complexity_label = format!("{:?}", decision.complexity).to_lowercase();
    decision.reasons.push(format!(
        "execution: derived from complexity {complexity_label}"
    ));
    apply_risk_execution_floor(decision);
    apply_direct_execution_workflow_rule(decision);
    decision.seat_tier = SeatTier::from_execution(decision.execution);
    decision.worker_tier = worker_tier_from_execution(decision.execution);
    decision.seat_role = SeatRole::from_execution(decision.execution);
    decision.orchestrator.model =
        model_for_tier(cfg, &decision.orchestrator.harness, decision.seat_tier);
}

/// Top file extensions by count among `paths` -- shared by the (now always
/// empty, per `classify_request`'s own text-only baseline) classification
/// path list and the real measured branch diff `build_intake` reads for
/// `IntakeState`'s informational `uncommitted_or_branch_changes` context.
fn primary_extensions<P: AsRef<Path>>(paths: &[P]) -> Vec<String> {
    let mut counts: BTreeMap<String, usize> = BTreeMap::new();
    for path in paths {
        if let Some(ext) = path.as_ref().extension().and_then(|value| value.to_str()) {
            *counts.entry(ext.to_ascii_lowercase()).or_default() += 1;
        }
    }
    let mut pairs: Vec<(String, usize)> = counts.into_iter().collect();
    pairs.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    pairs.into_iter().map(|(ext, _)| ext).take(8).collect()
}

fn active_workflow_id(
    state: &crate::commands::ctx::state::StateDir,
    repo: &Path,
) -> Option<String> {
    let workflow = engine::load_active(state, repo).ok().flatten()?;
    Some(
        workflow
            .definition
            .map(|definition| definition.id)
            .unwrap_or_else(|| workflow.kind.as_str().to_string()),
    )
}

/// Best-effort spawn headroom for `harness`, from the exact source `zirv ctx
/// status`'s own `fallback:` line reads (`pace::current_windows` +
/// `pace::spawn_headroom`, see `status.rs`). `None` on any missing signal --
/// an absent reading is honest uncertainty, never a guess.
fn headroom_for_harness(
    cfg: &CtxConfig,
    state: &crate::commands::ctx::state::StateDir,
    harness: &str,
) -> Option<f64> {
    let provider = adapters::provider_for_agent_name(Some(harness));
    let now = crate::commands::ctx::state::now_secs();
    let (collector, estimator) =
        crate::commands::ctx::pace::current_windows(state, &cfg.pace, now, provider);
    crate::commands::ctx::pace::spawn_headroom(&collector, estimator.as_ref(), now, &cfg.pace)
        .map(|reading| reading.headroom_pct)
}

fn intake_harnesses(
    roster: &Roster,
    cfg: &CtxConfig,
    state: &crate::commands::ctx::state::StateDir,
) -> Vec<IntakeHarness> {
    roster
        .harnesses
        .iter()
        .map(|harness| {
            let models = catalogue::vendor(harness.vendor)
                .map(|vendor| {
                    vendor
                        .rungs
                        .iter()
                        .map(|rung| IntakeModel {
                            alias: rung.alias.to_string(),
                            tier: rung.tier.map(tier_str).map(str::to_string),
                            strength: rung.strength,
                            input_usd_per_mtok: rung
                                .price
                                .map(|price| price.input_micros as f64 / 1_000_000.0)
                                .unwrap_or(0.0),
                        })
                        .collect()
                })
                .unwrap_or_default();
            let headroom_pct = if harness.ready {
                headroom_for_harness(cfg, state, &harness.name)
            } else {
                None
            };
            IntakeHarness {
                name: harness.name.clone(),
                ready: harness.ready,
                headroom_pct,
                models,
            }
        })
        .collect()
}

/// Builds the Jev `state`/`questions()` input: the request (truncated to
/// `cfg.proxy.request_max_bytes`), the repository's own measured branch/
/// uncommitted change counts and extensions (labeled
/// `uncommitted_or_branch_changes` -- informational context about the
/// repository, never a stand-in for the request's own size; see
/// `classify_request`'s doc comment for why the baseline never measures
/// this), the enabled+ready harness roster with catalogue pricing and
/// best-effort headroom, the registered workflow ids/descriptions, and
/// whether the native runtime is available.
pub fn build_intake(
    cfg: &CtxConfig,
    repo: &Path,
    state_dir: &Path,
    request: &str,
    roster: &Roster,
) -> IntakeState {
    let state = crate::commands::ctx::state::StateDir::from_path(state_dir.to_path_buf());
    let repo_name = repo
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| repo.display().to_string());
    let (branch_paths, branch_lines) = measured_branch_changes(repo);
    IntakeState {
        request: truncate_bytes(request, cfg.proxy.request_max_bytes.max(1)),
        repository: IntakeRepository {
            name: repo_name,
            uncommitted_or_branch_changes: BranchChanges {
                files: branch_paths.len(),
                lines: branch_lines,
            },
            active_workflow: active_workflow_id(&state, repo),
            primary_extensions: primary_extensions(&branch_paths),
        },
        harnesses: intake_harnesses(roster, cfg, &state),
        workflows: roster.workflow_summaries(),
        policy: IntakePolicy {
            native_available: crate::commands::ctx::runtime::native_available(),
        },
    }
}

/// Truncates to [`MAX_CHOICE_OPTIONS`] total with `catch_all` always the
/// LAST entry: any prior occurrence of it is dropped first, so a truncation
/// can never cut it off regardless of where it sat in `options`.
fn cap_choice_options(
    mut options: Vec<(String, Option<String>)>,
    catch_all: (&str, &str),
) -> Vec<(String, Option<String>)> {
    options.retain(|(key, _)| key != catch_all.0);
    options.truncate(MAX_CHOICE_OPTIONS - 1);
    options.push((catch_all.0.to_string(), Some(catch_all.1.to_string())));
    options
}

fn choice(id: &str, instructions: &str, options: Vec<(String, Option<String>)>) -> Question {
    Question {
        id: id.to_string(),
        kind: QuestionKind::Choice,
        instructions: instructions.to_string(),
        criteria: Criteria::Choice(options),
    }
}

/// Builds one option/level description from a structured `what`/`not_for`/
/// `examples` triple -- TypeSafe's own guidance for separating options a
/// model could otherwise confuse (see the spec's "Decision fields" table).
/// The Jev wire format's own `criteria` shape is a single description string
/// per option (`typesafe.rs`'s own module doc has the shape), so this is a
/// Rust-side structuring aid rather than a wire-level change: it renders to
/// exactly the string that lands in that map. `not_for`/`examples` are
/// skipped when empty, so a level that has nothing to add (most `Score`
/// levels) stays a single sentence.
fn describe(what: &str, not_for: &str, examples: &[&str]) -> String {
    let mut text = what.to_string();
    if !not_for.is_empty() {
        text.push_str(&format!(" Not for: {not_for}."));
    }
    if !examples.is_empty() {
        text.push_str(&format!(" Examples: {}.", examples.join("; ")));
    }
    text
}

/// The neutral question set both model deciders answer -- self-contained
/// from `intake` alone, so neither `typesafe.rs` nor `llm.rs` needs the
/// `Roster`/`CtxConfig` this was built from.
pub fn questions(intake: &IntakeState) -> Vec<Question> {
    let mut out = vec![choice(
        "intent",
        "What kind of work is this request? Pick the single best match.",
        cap_choice_options(
            vec![
                (
                    "feature".to_string(),
                    Some(describe(
                        "Adds new capability or behavior that did not exist before.",
                        "fixing something broken, or restructuring without changing behavior",
                        &["a new export button", "a new API endpoint"],
                    )),
                ),
                (
                    "bugfix".to_string(),
                    Some(describe(
                        "Fixes a defect or regression -- something that should work but does not.",
                        "adding new capability",
                        &["a crash on startup", "a wrong calculation"],
                    )),
                ),
                (
                    "refactor".to_string(),
                    Some(describe(
                        "Restructures existing code without changing its observable behavior.",
                        "adding features or fixing bugs",
                        &["renaming", "extracting a function", "simplifying logic"],
                    )),
                ),
                (
                    "spike".to_string(),
                    Some(describe(
                        "Explores, prototypes, or researches an approach before committing to \
                         it.",
                        "shipping a final implementation",
                        &[
                            "try an approach and see if it works",
                            "a throwaway experiment",
                        ],
                    )),
                ),
                (
                    "review".to_string(),
                    Some(describe(
                        "Reviews or audits existing work rather than changing it outright.",
                        "implementing a fix or feature",
                        &["review this PR", "audit for security issues"],
                    )),
                ),
                (
                    "other".to_string(),
                    Some("Anything that does not fit the other five.".to_string()),
                ),
            ],
            ("other", "Anything that does not fit the other five."),
        ),
    )];

    out.push(Question {
        id: "complexity".to_string(),
        kind: QuestionKind::Score,
        instructions: "How complex is this request, from the request text and the repository \
                       facts given?"
            .to_string(),
        criteria: Criteria::Score(vec![
            describe(
                "Trivial: one obvious change in one place, or no code change at all.",
                "anything that needs investigation",
                &[
                    "a typo",
                    "a colour or constant",
                    "a default value",
                    "answering a question",
                    "explaining a command",
                ],
            ),
            describe(
                "Bounded: one area with a clear goal that needs some reading or investigation.",
                "cross-module work",
                &[
                    "fixing one reported bug (with or without a backtrace)",
                    "adding a flag or a small verb",
                    "a refactor within one module",
                    "a spike or research report",
                    "reviewing one change",
                    "writing one runbook",
                    "one CI job",
                ],
            ),
            describe(
                "Substantial: several areas, a real design choice, or a wide mechanical change.",
                "one bug",
                &[
                    "a major dependency upgrade across many call sites",
                    "a new subsystem in one crate area",
                    "a performance investigation spanning modules",
                ],
            ),
            describe(
                "Architectural: a cross-cutting redesign, or a migration of a store or protocol.",
                "",
                &[
                    "a plugin system",
                    "a new adapter with full parity",
                    "a TUI redesign",
                ],
            ),
        ]),
    });

    out.push(Question {
        id: "risk".to_string(),
        kind: QuestionKind::Score,
        instructions: "How risky is this request if it goes wrong?".to_string(),
        criteria: Criteria::Score(vec![
            "Low: no sensitive surface; isolated, well-tested change.".to_string(),
            "Medium: moderate blast radius; some cross-module impact.".to_string(),
            "High: touches authentication/security, database migration/schema, deployment/\
             configuration, or a public API boundary."
                .to_string(),
            "Critical: touches several sensitive surfaces at once (auth/security, migration, \
             deploy, public API, concurrency), or is otherwise catastrophic if wrong."
                .to_string(),
        ]),
    });

    // Issue #537 design revision: `execution`/`seat_tier`/`worker_tier` are
    // no longer asked at all -- a live 24-case Jev battery showed
    // `execution` answers were unreliable (17-74 confidence, calling
    // architectural work "direct") and any many-option seat/tier question
    // never cleared the confidence floor. All three are now derived from
    // `complexity` alone (see `execution_from_complexity`,
    // `finalize_derived_fields`); the model's influence on them flows
    // entirely through its `complexity` answer.
    const NONE_WORKFLOW_DESCRIPTION: &str = "Direct work that needs no gated workflow: \
                                              one-place changes, tiny fixes, questions.";
    // Issue #537 (this design revision): the registry's own `refactor` pack
    // description does not spell out that it covers a pure deletion/removal
    // (no new behavior) -- sharpened here, at the one place this question is
    // built, rather than in the pack's own definition this module does not
    // own.
    const REFACTOR_COVERS_DELETIONS: &str = " Explicitly covers deletions or removals of code \
                                              and docs with no new behavior.";
    let mut workflow_options: Vec<(String, Option<String>)> = intake
        .workflows
        .iter()
        .map(|workflow| {
            let mut description = workflow.description.clone();
            if workflow.id == "refactor" {
                description.push_str(REFACTOR_COVERS_DELETIONS);
            }
            (workflow.id.clone(), Some(description))
        })
        .collect();
    workflow_options.push((
        "none".to_string(),
        Some(NONE_WORKFLOW_DESCRIPTION.to_string()),
    ));
    out.push(choice(
        "workflow",
        &format!(
            "Which registered workflow, if any, should gate this request? \"none\" is {}",
            NONE_WORKFLOW_DESCRIPTION.to_lowercase()
        ),
        cap_choice_options(workflow_options, ("none", NONE_WORKFLOW_DESCRIPTION)),
    ));

    out.push(Question {
        id: "needs_clarification".to_string(),
        kind: QuestionKind::Noul,
        instructions: "Is this request too ambiguous to start without asking one clarifying \
                       question first?"
            .to_string(),
        criteria: Criteria::Noul {
            when_true: Some("too ambiguous; ask one question before starting".to_string()),
            when_false: Some("clear enough to start now".to_string()),
        },
    });

    out
}

fn parse_intent(value: &str) -> Option<Intent> {
    match value {
        "feature" => Some(Intent::Feature),
        "bugfix" => Some(Intent::Bugfix),
        "refactor" => Some(Intent::Refactor),
        "spike" => Some(Intent::Spike),
        "review" => Some(Intent::Review),
        "other" => Some(Intent::Other),
        _ => None,
    }
}

fn execution_rank(mode: ExecutionMode) -> u8 {
    match mode {
        ExecutionMode::Direct => 0,
        ExecutionMode::Bounded => 1,
        ExecutionMode::Orchestrated => 2,
    }
}

fn complexity_from_index(index: f64) -> Complexity {
    const LEVELS: [Complexity; 4] = [
        Complexity::Trivial,
        Complexity::Bounded,
        Complexity::Substantial,
        Complexity::Architectural,
    ];
    let idx = index.round().clamp(0.0, (LEVELS.len() - 1) as f64) as usize;
    LEVELS[idx]
}

fn risk_from_index(index: f64) -> RiskBand {
    const LEVELS: [RiskBand; 4] = [
        RiskBand::Low,
        RiskBand::Medium,
        RiskBand::High,
        RiskBand::Critical,
    ];
    let idx = index.round().clamp(0.0, (LEVELS.len() - 1) as f64) as usize;
    LEVELS[idx]
}

/// Merges `answers` onto `baseline`'s own fields, applying the per-field
/// rules the spec's "Decision fields" table sets: a low-confidence answer is
/// discarded (with a reason recorded); complexity/risk only ever rise
/// (`max(model, baseline)`); every other ASKED field (`intent`, `workflow`)
/// is replaced outright when confident. Existence checks against the live
/// roster (a workflow id, a harness/model pair) are deferred to [`validate`],
/// which runs right after this and has the `Roster` this function does not
/// need.
///
/// Issue #537 design revision, from a live 24-case Jev battery: `execution`,
/// `seat_tier` and `worker_tier` are no longer questions at all (see
/// [`finalize_derived_fields`]'s own doc comment for why) -- a model's only
/// influence on them is indirect, through however it moved `complexity`.
///
/// `request` is the same text `baseline` was itself derived from -- passed
/// through (never re-truncated or substituted with `""`) so the validation
/// recompute below can still see request-text-driven flags
/// (`ExecutionProfile::derive`'s own security-domain detection from words
/// like "credential"/"auth"/"secret") instead of silently losing them the
/// moment a model answers.
///
/// `cfg` is needed only for [`finalize_derived_fields`]'s own resolution of
/// the orchestrator's model via `handover::resolve_model`.
pub fn merge(
    cfg: &CtxConfig,
    baseline: &ProxyDecision,
    request: &str,
    answers: &Answers,
    min_confidence: f32,
) -> ProxyDecision {
    let mut decision = baseline.clone();
    decision.confidence = answers
        .iter()
        .map(|(id, answer)| (id.clone(), answer.confidence))
        .collect();

    let mut record_low = |id: &str, answer: &Answer| {
        decision.reasons.push(format!(
            "{id}: confidence {:.2} < {:.2}, kept baseline",
            answer.confidence, min_confidence
        ));
    };

    if let Some(answer) = answers.get("intent") {
        if answer.confidence < min_confidence {
            record_low("intent", answer);
        } else if let AnswerValue::Choice(value) = &answer.value
            && let Some(intent) = parse_intent(value)
        {
            decision.intent = intent;
        }
    }

    if let Some(answer) = answers.get("complexity") {
        if answer.confidence < min_confidence {
            record_low("complexity", answer);
        } else if let AnswerValue::Score(value) = answer.value {
            decision.complexity = decision.complexity.max(complexity_from_index(value));
        }
    }

    if let Some(answer) = answers.get("risk") {
        if answer.confidence < min_confidence {
            record_low("risk", answer);
        } else if let AnswerValue::Score(value) = answer.value {
            decision.risk = decision.risk.max(risk_from_index(value));
        }
    }

    if let Some(answer) = answers.get("workflow") {
        if answer.confidence < min_confidence {
            record_low("workflow", answer);
        } else if let AnswerValue::Choice(value) = &answer.value {
            decision.workflow = if value == "none" {
                None
            } else {
                Some(value.clone())
            };
        }
    }

    // Advisory only: never gated on confidence, since the value itself IS
    // the model's own confidence in "this is ambiguous" (see the noul
    // conversions in `typesafe.rs`/`llm.rs`).
    if let Some(answer) = answers.get("needs_clarification")
        && let AnswerValue::Noul(value) = answer.value
    {
        decision.needs_clarification = value as f32;
    }

    // Recomputed from the real request text (never `""` -- see this
    // function's own doc comment) and the (possibly raised) merged
    // complexity/risk. OR'd onto the baseline's own `validation` (already
    // sitting in `decision.validation` from the `baseline.clone()` above)
    // rather than overwriting it outright: `ExecutionProfile::derive`'s
    // rules are themselves monotonic in complexity/risk, so in practice
    // this recompute alone already only ever matches or extends the
    // baseline's flags, but OR-ing makes "never lower a validation flag" a
    // hard invariant of this function rather than an emergent property of
    // `profile.rs`'s own construction.
    let mut recomputed_classification = decision.classification_for_validation();
    recomputed_classification.complexity = decision.complexity;
    recomputed_classification.risk = decision.risk;
    let recomputed_validation =
        ExecutionProfile::derive(request, &recomputed_classification).validation;
    decision.validation.independent_review |= recomputed_validation.independent_review;
    decision.validation.independent_test |= recomputed_validation.independent_test;
    decision.validation.security_review |= recomputed_validation.security_review;
    apply_security_risk_floor(&mut decision);
    finalize_derived_fields(&mut decision, cfg);

    decision
}

impl ProxyDecision {
    /// A minimal, valid [`Classification`] carrying this decision's own
    /// intent/complexity/risk, for [`ExecutionProfile::derive`]'s
    /// validation recompute in [`merge`]. Every other field is the
    /// harmless default `derive` never reads when computing `validation`
    /// from complexity/risk alone (see that function: `validation` depends
    /// only on `risk`, `complexity` and `work_domain`/text-derived domains,
    /// none of which this recompute needs to reproduce exactly since it is
    /// never lower than the baseline's own).
    fn classification_for_validation(&self) -> Classification {
        Classification {
            intent: self.intent,
            complexity: self.complexity,
            risk: self.risk,
            risk_score: 0,
            changed_files: 0,
            changed_lines: 0,
            changed_paths: Vec::new(),
            declared_scope: false,
            work_domain: classify::DomainClassification::default(),
            risk_measurement: classify::RiskMeasurement::default(),
            reasons: Vec::new(),
        }
    }
}

/// Reverts `decision`'s `orchestrator`/`workflow` fields to `baseline`'s own
/// when the roster proves them invalid.
///
/// Review finding (round 3): the orchestrator's `model` is NEVER policed
/// against the catalogue here -- only `harness` readiness is. `model` is
/// always the tier-derived result of `model_for_tier` (`handover::
/// resolve_model`, see that function's own doc comment), the same trusted
/// resolver `zirv ctx handover` itself uses, and it already honors an
/// operator's own free-form override (`[handover.claude] standard =
/// "my-team/internal-model"`, a documented value with no catalogue rung of
/// its own at all). Policing it here used to silently revert exactly that
/// kind of decision to the BASELINE's own model -- which, whenever a merge
/// had raised `seat_tier` above the baseline's own (a confident `complexity`
/// answer, say), was a DIFFERENT tier's model: the decision then announced
/// one seat tier while quietly launching another. When the harness itself is
/// not enabled+ready, `orchestrator` still reverts -- but to the baseline's
/// harness with the model RE-DERIVED for it at `decision`'s own (unreverted)
/// `seat_tier`, so the decision stays internally consistent rather than
/// falling back to whatever tier the baseline itself happened to be at.
pub fn validate(
    decision: &mut ProxyDecision,
    baseline: &ProxyDecision,
    roster: &Roster,
    cfg: &CtxConfig,
) {
    if !harness_is_ready(&decision.orchestrator.harness, roster) {
        if decision.orchestrator.harness != baseline.orchestrator.harness {
            decision.reasons.push(format!(
                "seat: harness '{}' is not an enabled+ready harness; kept baseline harness '{}'",
                decision.orchestrator.harness, baseline.orchestrator.harness,
            ));
        }
        decision.orchestrator.harness = baseline.orchestrator.harness.clone();
        decision.orchestrator.model =
            model_for_tier(cfg, &decision.orchestrator.harness, decision.seat_tier);
    }

    if let Some(id) = decision.workflow.clone()
        && !roster.workflow_exists(&id)
    {
        decision
            .reasons
            .push(format!("workflow: unknown id '{id}'; kept baseline"));
        decision.workflow = baseline.workflow.clone();
    }
}

fn harness_is_ready(harness: &str, roster: &Roster) -> bool {
    roster.harness(harness).is_some_and(|h| h.ready)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_decision() -> ProxyDecision {
        ProxyDecision {
            request_sha256: "x".repeat(64),
            repo: PathBuf::from("/tmp/repo"),
            intent: Intent::Feature,
            complexity: Complexity::Bounded,
            risk: RiskBand::Low,
            execution: ExecutionMode::Bounded,
            seat_role: SeatRole::Single,
            validation: ValidationProfile::default(),
            workflow: Some("feature".to_string()),
            orchestrator: Seat {
                harness: "claude".to_string(),
                model: "sonnet".to_string(),
            },
            seat_tier: SeatTier::Standard,
            worker_tier: Tier::Cheap,
            needs_clarification: 0.0,
            decider: Decider::Deterministic,
            confidence: BTreeMap::new(),
            reasons: Vec::new(),
            fallbacks: Vec::new(),
            elapsed_ms: 0,
            usage: None,
            created_at: 0,
        }
    }

    fn classification_with(risk: RiskBand, complexity: Complexity) -> Classification {
        Classification {
            intent: Intent::Feature,
            complexity,
            risk,
            risk_score: 0,
            changed_files: 0,
            changed_lines: 0,
            changed_paths: Vec::new(),
            declared_scope: false,
            work_domain: classify::DomainClassification::default(),
            risk_measurement: classify::RiskMeasurement::default(),
            reasons: Vec::new(),
        }
    }

    fn answers(pairs: &[(&str, AnswerValue, f32)]) -> Answers {
        pairs
            .iter()
            .map(|(id, value, confidence)| {
                (
                    (*id).to_string(),
                    Answer {
                        value: value.clone(),
                        confidence: *confidence,
                    },
                )
            })
            .collect()
    }

    #[test]
    fn a_confident_higher_complexity_raises_the_baseline() {
        let cfg = CtxConfig::default();
        let baseline = sample_decision();
        let ans = answers(&[("complexity", AnswerValue::Score(3.0), 0.9)]);
        let merged = merge(&cfg, &baseline, "implement the feature", &ans, 0.5);
        assert_eq!(merged.complexity, Complexity::Architectural);
    }

    #[test]
    fn a_confident_lower_complexity_never_lowers_the_baseline() {
        let cfg = CtxConfig::default();
        let mut baseline = sample_decision();
        baseline.complexity = Complexity::Substantial;
        let ans = answers(&[("complexity", AnswerValue::Score(0.0), 0.95)]);
        let merged = merge(&cfg, &baseline, "implement the feature", &ans, 0.5);
        assert_eq!(
            merged.complexity,
            Complexity::Substantial,
            "a model's lower reading must never lower the baseline"
        );
    }

    #[test]
    fn a_low_confidence_answer_keeps_the_baseline_and_records_a_reason() {
        let cfg = CtxConfig::default();
        let baseline = sample_decision();
        let ans = answers(&[("risk", AnswerValue::Score(3.0), 0.2)]);
        let merged = merge(&cfg, &baseline, "implement the feature", &ans, 0.5);
        assert_eq!(merged.risk, baseline.risk);
        assert!(
            merged
                .reasons
                .iter()
                .any(|reason| reason.contains("risk: confidence 0.20 < 0.50, kept baseline")),
            "{:?}",
            merged.reasons
        );
    }

    /// Issue #537 design revision (a live 24-case Jev battery showed its own
    /// `execution` answers were unreliable, 17-74 confidence, calling
    /// architectural work "direct"): `execution` is no longer a question at
    /// all -- an answer under that id, however confident, must be ignored
    /// entirely, and `execution` must rise ONLY as a side effect of a raised
    /// `complexity` (`execution_from_complexity`).
    #[test]
    fn execution_is_derived_from_complexity_and_an_execution_answer_is_ignored() {
        let cfg = CtxConfig::default();
        let mut baseline = sample_decision();
        baseline.complexity = Complexity::Trivial;
        let ans = answers(&[(
            "execution",
            AnswerValue::Choice("orchestrated".to_string()),
            0.99,
        )]);
        let merged = merge(&cfg, &baseline, "fix the typo", &ans, 0.5);
        assert_eq!(
            merged.execution,
            ExecutionMode::Direct,
            "an 'execution' answer must never move execution on its own"
        );

        let ans = answers(&[("complexity", AnswerValue::Score(3.0), 0.9)]);
        let merged = merge(&cfg, &baseline, "fix the typo", &ans, 0.5);
        assert_eq!(merged.complexity, Complexity::Architectural);
        assert_eq!(
            merged.execution,
            ExecutionMode::Orchestrated,
            "execution rises when a confident complexity answer raises it"
        );
    }

    /// Issue #537 design revision: `execution`/`seat_tier`/`worker_tier`/
    /// `seat_role` follow `complexity` alone, exercised across the whole
    /// ladder -- `Trivial` a single cheap seat, `Bounded` a single standard
    /// seat, `Substantial`/`Architectural` a frontier orchestrator with
    /// standard-tier workers.
    #[test]
    fn the_whole_seat_ladder_follows_the_merged_complexity() {
        let cfg = CtxConfig::default();
        for (complexity, execution, seat_tier, worker_tier, seat_role) in [
            (
                Complexity::Trivial,
                ExecutionMode::Direct,
                SeatTier::Cheap,
                Tier::Cheap,
                SeatRole::Single,
            ),
            (
                Complexity::Bounded,
                ExecutionMode::Bounded,
                SeatTier::Standard,
                Tier::Cheap,
                SeatRole::Single,
            ),
            (
                Complexity::Substantial,
                ExecutionMode::Orchestrated,
                SeatTier::Frontier,
                Tier::Standard,
                SeatRole::Orchestrator,
            ),
            (
                Complexity::Architectural,
                ExecutionMode::Orchestrated,
                SeatTier::Frontier,
                Tier::Standard,
                SeatRole::Orchestrator,
            ),
        ] {
            let mut baseline = sample_decision();
            baseline.complexity = Complexity::Trivial;
            let index = match complexity {
                Complexity::Trivial => 0.0,
                Complexity::Bounded => 1.0,
                Complexity::Substantial => 2.0,
                Complexity::Architectural => 3.0,
            };
            let ans = answers(&[("complexity", AnswerValue::Score(index), 0.9)]);
            let merged = merge(&cfg, &baseline, "a request", &ans, 0.5);
            assert_eq!(merged.complexity, complexity, "{complexity:?}: complexity");
            assert_eq!(merged.execution, execution, "{complexity:?}: execution");
            assert_eq!(merged.seat_tier, seat_tier, "{complexity:?}: seat_tier");
            assert_eq!(
                merged.worker_tier, worker_tier,
                "{complexity:?}: worker_tier"
            );
            assert_eq!(merged.seat_role, seat_role, "{complexity:?}: seat_role");
        }
    }

    /// Issue #537: `SeatRole` is a name for what `execution` already
    /// decided, derived once at the tail of `baseline`/`merge` -- `Direct`
    /// and `Bounded` both stay on one seat; only `Orchestrated` compiles a
    /// team.
    #[test]
    fn seat_role_follows_execution() {
        assert_eq!(
            SeatRole::from_execution(ExecutionMode::Direct),
            SeatRole::Single
        );
        assert_eq!(
            SeatRole::from_execution(ExecutionMode::Bounded),
            SeatRole::Single
        );
        assert_eq!(
            SeatRole::from_execution(ExecutionMode::Orchestrated),
            SeatRole::Orchestrator
        );
    }

    /// Issue #537: a `direct` execution answer must never coexist with a
    /// gated workflow, even when the model was confident about both --
    /// exactly the operator's own two live-decision complaints (a trivial
    /// colour change and a bounded bugfix investigation, each landing a
    /// gated `workflow` a `Direct` execution has no business gating).
    #[test]
    fn direct_execution_clears_a_confident_workflow_answer() {
        let cfg = CtxConfig::default();
        let mut baseline = sample_decision();
        // `execution` is derived from `complexity` alone (issue #537 design
        // revision) -- `Trivial` is what actually makes this `Direct`.
        baseline.complexity = Complexity::Trivial;
        baseline.execution = ExecutionMode::Direct;
        baseline.workflow = None;
        let ans = answers(&[("workflow", AnswerValue::Choice("feature".to_string()), 0.79)]);
        let merged = merge(&cfg, &baseline, "change the background color", &ans, 0.5);
        assert_eq!(merged.complexity, Complexity::Trivial);
        assert_eq!(merged.execution, ExecutionMode::Direct);
        assert_eq!(merged.workflow, None, "{:?}", merged);
        assert_eq!(merged.seat_role, SeatRole::Single);
        assert!(
            merged
                .reasons
                .iter()
                .any(|reason| reason == "workflow: none because execution is direct"),
            "{:?}",
            merged.reasons
        );
    }

    /// Issue #537: `seat_tier`/`worker_tier` resolve to concrete models
    /// through `handover::resolve_model` -- never guessed in this module --
    /// so the merged decision's `orchestrator.model` always matches what
    /// `zirv ctx handover` itself would resolve for that harness/tier.
    #[test]
    fn seat_tier_resolves_to_a_concrete_model_via_handover_for_claude() {
        let cfg = CtxConfig::default();
        for (tier, expected) in [
            (SeatTier::Cheap, "haiku"),
            (SeatTier::Standard, "sonnet"),
            (SeatTier::Deep, "opus"),
        ] {
            assert_eq!(model_for_tier(&cfg, "claude", tier), expected);
        }
        // Frontier: the operator's own configured `chat.model` when set,
        // else the vendor's own top rung.
        assert_eq!(model_for_tier(&cfg, "claude", SeatTier::Frontier), "fable");
        let mut with_chat_model = cfg.clone();
        with_chat_model.chat.model = Some("mythos".to_string());
        assert_eq!(
            model_for_tier(&with_chat_model, "claude", SeatTier::Frontier),
            "mythos"
        );
    }

    /// Issue #537: the baseline maps `seat_tier` from `execution` alone.
    #[test]
    fn baseline_seat_tier_follows_execution() {
        assert_eq!(
            SeatTier::from_execution(ExecutionMode::Direct),
            SeatTier::Cheap
        );
        assert_eq!(
            SeatTier::from_execution(ExecutionMode::Bounded),
            SeatTier::Standard
        );
        assert_eq!(
            SeatTier::from_execution(ExecutionMode::Orchestrated),
            SeatTier::Frontier
        );
    }

    /// Review finding: `merge`'s validation recompute used to call
    /// `ExecutionProfile::derive` with an empty request text, silently
    /// dropping text-driven review flags (`security_review`/
    /// `independent_review` from words like "credential") the moment any
    /// model answer merged. Covers all three angles: the baseline itself
    /// sets the flags from text alone at Low risk; a merge that keeps risk
    /// Low must not lose them; and a merge that raises risk to High must
    /// turn them on even when the text said nothing sensitive.
    #[test]
    fn merge_preserves_text_driven_validation_flags_and_raises_them_with_risk() {
        let repo = tempfile::tempdir().expect("tempdir");
        let cfg = CtxConfig::default();
        let roster = Roster {
            harnesses: Vec::new(),
            registry: None,
        };

        let sensitive_request = "rotate the shared credential constant";
        let classification = classification_with(RiskBand::Low, Complexity::Trivial);
        let baseline_decision = baseline(
            &cfg,
            repo.path(),
            sensitive_request,
            &classification,
            &roster,
        );
        // `apply_security_risk_floor` already raises this to `High` inside
        // `baseline` itself, from the text alone -- see that function's own
        // dedicated test (`security_text_at_intake_floors_risk_high_and_
        // execution_bounded`).
        assert_eq!(baseline_decision.risk, RiskBand::High);
        assert!(
            baseline_decision.validation.security_review,
            "text alone must set security_review"
        );
        assert!(baseline_decision.validation.independent_review);

        // A merge with a confident LOW risk model answer must never lower
        // what the baseline already (correctly) floored, and must keep the
        // text-driven validation flags.
        let low_risk_answer = answers(&[("risk", AnswerValue::Score(0.0), 0.9)]);
        let merged = merge(
            &cfg,
            &baseline_decision,
            sensitive_request,
            &low_risk_answer,
            0.5,
        );
        assert_eq!(merged.risk, RiskBand::High);
        assert!(merged.validation.security_review);
        assert!(merged.validation.independent_review);

        // A merge that raises risk to High must turn the flags on even when
        // the request text itself named nothing sensitive.
        let plain_request = "add a small feature to the dashboard";
        let plain_classification = classification_with(RiskBand::Low, Complexity::Trivial);
        let plain_baseline = baseline(
            &cfg,
            repo.path(),
            plain_request,
            &plain_classification,
            &roster,
        );
        assert!(!plain_baseline.validation.security_review);
        assert!(!plain_baseline.validation.independent_review);
        let high_risk_answer = answers(&[("risk", AnswerValue::Score(2.0), 0.9)]);
        let raised = merge(&cfg, &plain_baseline, plain_request, &high_risk_answer, 0.5);
        assert_eq!(raised.risk, RiskBand::High);
        assert!(raised.validation.security_review);
        assert!(raised.validation.independent_review);
    }

    /// Issue #537 battery finding: a sensitive-surface risk floor must also
    /// floor execution, so a one-line auth change can never route as
    /// `Direct` on wording or diff size alone.
    #[test]
    fn risk_high_floors_execution_to_at_least_bounded() {
        let mut decision = sample_decision();
        decision.execution = ExecutionMode::Direct;
        decision.risk = RiskBand::High;
        apply_risk_execution_floor(&mut decision);
        assert_eq!(decision.execution, ExecutionMode::Bounded);
        assert!(
            decision
                .reasons
                .iter()
                .any(|reason| reason.contains("execution: raised to bounded because risk is high")),
            "{:?}",
            decision.reasons
        );

        // Already at or above the floor: untouched, no spurious reason.
        let mut decision = sample_decision();
        decision.execution = ExecutionMode::Orchestrated;
        decision.risk = RiskBand::Critical;
        apply_risk_execution_floor(&mut decision);
        assert_eq!(decision.execution, ExecutionMode::Orchestrated);
        assert!(decision.reasons.is_empty());

        // Low/Medium risk never floors execution.
        let mut decision = sample_decision();
        decision.execution = ExecutionMode::Direct;
        decision.risk = RiskBand::Medium;
        apply_risk_execution_floor(&mut decision);
        assert_eq!(decision.execution, ExecutionMode::Direct);
    }

    /// Issue #537: with the baseline now text-only, the path-based
    /// sensitive-surface risk floor can no longer see any paths at intake
    /// time -- `apply_security_risk_floor` is what keeps "a sensitive
    /// 'small' request cannot obtain the fast path from wording alone" true
    /// anyway, from `validation.security_review`'s own text-driven
    /// detection. Exercises the real `classify_request` + `baseline` path
    /// (not the helper function in isolation), on both a sensitive and a
    /// plain request.
    #[test]
    fn security_text_at_intake_floors_risk_high_and_execution_bounded() {
        let repo = tempfile::tempdir().expect("tempdir");
        let cfg = CtxConfig::default();
        let roster = Roster {
            harnesses: Vec::new(),
            registry: None,
        };

        let sensitive_request = "rotate the shared credential constant used by session auth";
        let sensitive_classification = classify_request(sensitive_request);
        let sensitive_decision = baseline(
            &cfg,
            repo.path(),
            sensitive_request,
            &sensitive_classification,
            &roster,
        );
        assert_eq!(sensitive_decision.risk, RiskBand::High);
        assert_eq!(sensitive_decision.execution, ExecutionMode::Bounded);
        assert!(
            sensitive_decision.reasons.iter().any(|reason| reason
                .contains("risk: raised to high because the request names a security surface")),
            "{:?}",
            sensitive_decision.reasons
        );

        let plain_request = "fix the typo in the README";
        let plain_classification = classify_request(plain_request);
        let plain_decision = baseline(
            &cfg,
            repo.path(),
            plain_request,
            &plain_classification,
            &roster,
        );
        assert_eq!(plain_decision.risk, RiskBand::Low);
        assert_eq!(plain_decision.execution, ExecutionMode::Direct);
    }

    /// Issue #537 fix: a feature branch can carry thousands of lines that
    /// have nothing to do with the request being decided on right now. The
    /// old baseline measured the repository's own diff at intake
    /// (`classify::from_args`, even on its "declared" branch, floors risk/
    /// complexity from a measured tree) -- so on a branch like this one,
    /// EVERY request escalated regardless of what it actually asked for,
    /// and the merge chain's own monotonic floor then forbade a model
    /// decider from ever lowering it back down. `classify_request`/
    /// `baseline` must ignore this large diff entirely.
    #[test]
    fn baseline_ignores_a_large_unrelated_repository_diff() {
        let repo = tempfile::tempdir().expect("tempdir");
        let git = |args: &[&str]| {
            let status = std::process::Command::new("git")
                .args([
                    "-c",
                    "user.email=t@example.com",
                    "-c",
                    "user.name=t",
                    "-c",
                    "commit.gpgsign=false",
                ])
                .args(args)
                .current_dir(repo.path())
                .status()
                .expect("run git");
            assert!(status.success(), "git {args:?} failed");
        };
        git(&["init", "-q"]);
        std::fs::write(repo.path().join("README.md"), "base\n").expect("write base");
        git(&["add", "."]);
        git(&["commit", "-q", "-m", "base"]);
        // A large, unrelated committed diff on this branch relative to its
        // own base -- exactly the shape a real feature branch carries.
        std::fs::create_dir_all(repo.path().join("src")).expect("mkdir src");
        for n in 0..20 {
            std::fs::write(
                repo.path().join(format!("src/unrelated-{n}.rs")),
                "line\n".repeat(400),
            )
            .expect("write unrelated file");
        }
        git(&["add", "."]);
        git(&["commit", "-q", "-m", "unrelated feature work"]);

        let request = "fix the typo in README";
        let classification = classify_request(request);
        assert_eq!(classification.complexity, Complexity::Trivial);
        assert_eq!(classification.changed_files, 0);
        assert_eq!(classification.changed_lines, 0);

        let cfg = CtxConfig::default();
        let roster = Roster {
            harnesses: Vec::new(),
            registry: None,
        };
        let decision = baseline(&cfg, repo.path(), request, &classification, &roster);
        assert_eq!(decision.execution, ExecutionMode::Direct);
        assert_eq!(decision.complexity, Complexity::Trivial);
        assert_eq!(decision.workflow, None);
        assert!(
            decision
                .reasons
                .iter()
                .any(|reason| reason.contains("request text only")),
            "{:?}",
            decision.reasons
        );
    }

    /// Review finding (round 3): when the harness itself is not enabled+
    /// ready, `orchestrator` still reverts -- but to the baseline's harness
    /// with the model RE-DERIVED for it at `decision`'s OWN (unreverted)
    /// `seat_tier`, never copied verbatim from the baseline (which may sit
    /// at a different tier entirely).
    #[test]
    fn validate_reverts_an_unready_harness_and_rederives_the_model_at_the_decisions_own_seat_tier()
    {
        let cfg = CtxConfig::default();
        let baseline = sample_decision();
        let mut decision = baseline.clone();
        decision.orchestrator = Seat {
            harness: "codex".to_string(),
            model: "gpt-5.6-sol".to_string(),
        };
        // A merge raised `seat_tier` above the baseline's own `standard` --
        // the reverted harness's model must reflect THIS tier.
        decision.seat_tier = SeatTier::Deep;
        let roster = Roster {
            harnesses: vec![
                RosterHarness {
                    name: "claude".to_string(),
                    ready: true,
                    vendor: "anthropic",
                },
                RosterHarness {
                    name: "codex".to_string(),
                    ready: false,
                    vendor: "openai",
                },
            ],
            registry: None,
        };
        validate(&mut decision, &baseline, &roster, &cfg);
        assert_eq!(decision.orchestrator.harness, "claude");
        assert_eq!(
            decision.orchestrator.model,
            model_for_tier(&cfg, "claude", SeatTier::Deep)
        );
        assert_ne!(
            decision.orchestrator.model, baseline.orchestrator.model,
            "must not silently copy a different tier's model from the baseline"
        );
        assert!(
            decision
                .reasons
                .iter()
                .any(|reason| reason.contains("not an enabled+ready harness"))
        );
    }

    /// Review finding (round 3): a tier-derived model absent from the
    /// catalogue must never be reverted -- `handover::resolve_model` is
    /// already the trusted resolver for it, operator free-form overrides
    /// (`[handover.claude] standard = "my-team/internal-model"`, a
    /// documented value with no catalogue rung of its own) included.
    /// Reproduces the exact bug: a merge raises `seat_tier` from the
    /// baseline's own `cheap` to `standard`, where the operator has
    /// overridden claude's `standard` tier to such a model -- the old
    /// catalogue check silently reverted this to the baseline's `cheap`
    /// model while leaving `seat_tier` at `standard`, announcing one tier
    /// and launching another.
    #[test]
    fn validate_never_reverts_a_trusted_tier_derived_model_absent_from_the_catalogue() {
        let mut cfg = CtxConfig::default();
        cfg.handover.claude.standard = Some("my-team/internal-model".to_string());
        let roster = Roster {
            harnesses: vec![RosterHarness {
                name: "claude".to_string(),
                ready: true,
                vendor: "anthropic",
            }],
            registry: None,
        };

        let mut baseline = sample_decision();
        baseline.complexity = Complexity::Trivial;
        baseline.execution = ExecutionMode::Direct;
        baseline.seat_tier = SeatTier::Cheap;
        baseline.orchestrator.model = model_for_tier(&cfg, "claude", SeatTier::Cheap);

        let mut decision = baseline.clone();
        decision.complexity = Complexity::Bounded;
        decision.execution = ExecutionMode::Bounded;
        decision.seat_tier = SeatTier::Standard;
        decision.orchestrator.model = model_for_tier(&cfg, "claude", SeatTier::Standard);
        assert_eq!(decision.orchestrator.model, "my-team/internal-model");

        validate(&mut decision, &baseline, &roster, &cfg);

        assert_eq!(decision.seat_tier, SeatTier::Standard);
        assert_eq!(decision.orchestrator.model, "my-team/internal-model");
        assert!(
            decision
                .reasons
                .iter()
                .all(|reason| !reason.starts_with("seat:")),
            "no revert reason expected: {:?}",
            decision.reasons
        );
    }

    #[test]
    fn validate_rejects_an_unknown_workflow_id() {
        let cfg = CtxConfig::default();
        let baseline = sample_decision();
        let mut decision = baseline.clone();
        decision.workflow = Some("no-such-workflow".to_string());
        let roster = Roster {
            harnesses: vec![RosterHarness {
                name: "claude".to_string(),
                ready: true,
                vendor: "anthropic",
            }],
            registry: None,
        };
        validate(&mut decision, &baseline, &roster, &cfg);
        assert_eq!(decision.workflow, baseline.workflow);
        assert!(
            decision
                .reasons
                .iter()
                .any(|reason| reason.contains("unknown id 'no-such-workflow'"))
        );
    }

    #[test]
    fn questions_never_exceed_the_choice_cap_and_every_choice_has_a_catch_all() {
        let intake = IntakeState {
            request: "x".to_string(),
            repository: IntakeRepository {
                name: "repo".to_string(),
                uncommitted_or_branch_changes: BranchChanges { files: 0, lines: 0 },
                active_workflow: None,
                primary_extensions: Vec::new(),
            },
            harnesses: (0..40)
                .map(|n| IntakeHarness {
                    name: format!("harness-{n}"),
                    ready: true,
                    headroom_pct: Some(50.0),
                    models: (0..8)
                        .map(|m| IntakeModel {
                            alias: format!("model-{m}"),
                            tier: Some("standard".to_string()),
                            strength: 1,
                            input_usd_per_mtok: 1.0,
                        })
                        .collect(),
                })
                .collect(),
            workflows: (0..300)
                .map(|n| IntakeWorkflow {
                    id: format!("workflow-{n}"),
                    description: "desc".to_string(),
                })
                .collect(),
            policy: IntakePolicy {
                native_available: false,
            },
        };
        for question in questions(&intake) {
            if let Criteria::Choice(options) = &question.criteria {
                assert!(
                    options.len() <= MAX_CHOICE_OPTIONS,
                    "{}: {} options",
                    question.id,
                    options.len()
                );
                assert!(
                    options
                        .iter()
                        .any(|(key, _)| key == "none" || key == "other"),
                    "{} has no none/other catch-all: {:?}",
                    question.id,
                    options.iter().map(|(k, _)| k).collect::<Vec<_>>()
                );
            }
        }
    }

    #[test]
    fn intake_state_never_names_an_env_var_and_truncates_the_request() {
        let cfg = CtxConfig {
            proxy: crate::commands::ctx::config::ProxyConfig {
                request_max_bytes: 8,
                ..Default::default()
            },
            ..CtxConfig::default()
        };
        let repo = tempfile::tempdir().expect("tempdir");
        let state_dir = tempfile::tempdir().expect("tempdir");
        let roster = Roster {
            harnesses: Vec::new(),
            registry: None,
        };
        let intake = build_intake(
            &cfg,
            repo.path(),
            state_dir.path(),
            "a very long request indeed",
            &roster,
        );
        assert_eq!(intake.request.len(), 8);

        let value = serde_json::to_value(&intake).expect("serialize");
        let text = value.to_string();
        for env_like in ["TYPESAFE_API_KEY", "credential_env", "HOME", "PATH"] {
            assert!(
                !text.contains(env_like),
                "intake state must never name an env var: found {env_like} in {text}"
            );
        }
    }
}
