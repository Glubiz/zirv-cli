//! Issue #537 seam: the harness proxy's decision core.
//!
//! This module holds the pure building blocks `proxy::decide` (in `mod.rs`)
//! chains together: a deterministic [`baseline`] (reusing the existing
//! classifier/profile/selection seams, never a model call), the neutral
//! [`Question`]/[`Answer`] shapes (from the shared `jev` module) both model
//! deciders (`typesafe.rs`, `llm.rs`) answer into, [`merge`] (confidence-gated, monotonic on
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
// Issue #537 seam extraction (A1): `Question`/`Criteria`/`QuestionKind`/
// `AnswerValue`/`Answer`/`Answers`/`Usage`/`MAX_CHOICE_OPTIONS` now live in
// the shared `jev` module (any future Jev-consuming site needs the same
// neutral shapes); re-exported here so every existing `decision::<Name>`
// path in this crate keeps compiling unchanged.
pub use crate::commands::ctx::jev::{
    Answer, AnswerValue, Answers, Criteria, MAX_CHOICE_OPTIONS, Question, QuestionKind, Usage,
};
use crate::commands::workflow::classify::{self, Classification, Complexity, Intent, RiskBand};
use crate::commands::workflow::engine;
use crate::commands::workflow::profile::{ExecutionMode, ExecutionProfile, ValidationProfile};
use crate::commands::workflow::selection;

/// The deterministic classifier's own task-text bound is smaller than
/// `[proxy] request_max_bytes`'s default; a request longer than this is
/// truncated before it ever reaches `classify::from_args`, so a long prompt
/// degrades to "classified from a prefix" rather than failing baseline
/// construction outright.
const CLASSIFY_TASK_MAX_BYTES: usize = 4000;

/// Issue #537 (A2): the additive domain tags a confident `Noul` answer may
/// add to [`ProxyDecision::domains`] -- also the exact question ids
/// [`questions`] asks and [`merge`] reads back, so the two can never drift
/// out of sync with each other.
pub(crate) const DOMAIN_QUESTION_IDS: [&str; 6] = [
    "security",
    "data",
    "docs_only",
    "devops",
    "architecture",
    "frontend",
];

/// The six domain Noul questions themselves ([`DOMAIN_QUESTION_IDS`]'s own
/// `(instructions, when_true, when_false)`, in the same order), promoted out
/// of [`questions`] so the workflow module's own metadata-only classify
/// refinement (issue #782, `workflow::profile::refine_via_jev`) can ask the
/// identical six questions instead of writing a second copy.
pub(crate) const DOMAIN_NOUL_QUESTIONS: [(&str, &str, &str); 6] = [
    (
        "Does this request touch authentication, credentials, permissions, or a trust \
         boundary?",
        "yes, a security-sensitive surface",
        "no security-sensitive surface",
    ),
    (
        "Does this request touch a data schema, a migration, or stored data?",
        "yes, a data surface",
        "no data surface",
    ),
    (
        "Does this request change only documentation or comments, with no other code \
         change?",
        "yes, documentation/comments only",
        "no, it changes other code too",
    ),
    (
        "Does this request touch CI, deployment, packaging, or infrastructure?",
        "yes, a deployment/operations surface",
        "no deployment/operations surface",
    ),
    (
        "Does this request involve cross-module design or a new subsystem?",
        "yes, cross-module design or a new subsystem",
        "no, contained to one place",
    ),
    (
        "Does this request touch UI, rendering, or visual behavior?",
        "yes, a UI/visual surface",
        "no UI/visual surface",
    ),
];

/// The orchestrator seat a decision names: a harness registry name
/// (`"claude"`, `"codex"`, ...) plus a model alias or id on that harness's
/// own vendor ladder.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Seat {
    pub harness: String,
    pub model: String,
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
/// asked, from [`SeatTier::from_execution_complexity_risk`]. `Frontier` is
/// the top-of-fleet tier `worker_tier`/[`super::catalogue::Tier`]
/// deliberately has no equivalent of: a delegated worker is never the
/// orchestrator seat compiling the team, so it never needs the top rung.
///
/// Frontier seat gate (wrapper-overhead benchmark, 2026-09-22): a 36-run
/// replay of the proxy intake found `Substantial` complexity alone routing
/// two six-step feature tasks to a frontier orchestrator seat at 1.7-2.2x
/// cost with no correctness gain. `Orchestrated` execution no longer implies
/// `Frontier` by itself -- see [`SeatTier::from_execution_complexity_risk`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SeatTier {
    Cheap,
    Standard,
    Deep,
    Frontier,
}

impl SeatTier {
    /// Issue #537, revised by the wrapper-overhead benchmark: `Direct` needs
    /// no more than a cheap seat and `Bounded` a standard one, exactly as
    /// before. `Orchestrated` (a real compiled team) no longer earns the
    /// frontier rung on complexity alone -- it earns `Frontier` only when the
    /// complexity is `Architectural` (an architectural-scope task always
    /// gets the top seat) OR the risk is `High`/`Critical` (a sensitive
    /// surface always gets the top seat regardless of scope); a `Substantial`
    /// task at `Low`/`Medium` risk gets a `Standard` seat instead. `execution`,
    /// `seat_role` (still `SeatRole::from_execution`), and `worker_tier`
    /// (still `worker_tier_from_execution`) are untouched by this rule.
    fn from_execution_complexity_risk(
        execution: ExecutionMode,
        complexity: Complexity,
        risk: RiskBand,
    ) -> Self {
        match execution {
            ExecutionMode::Direct => SeatTier::Cheap,
            ExecutionMode::Bounded => SeatTier::Standard,
            ExecutionMode::Orchestrated => {
                if complexity == Complexity::Architectural || risk >= RiskBand::High {
                    SeatTier::Frontier
                } else {
                    SeatTier::Standard
                }
            }
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
    /// Whether the `needs_clarification` answer above was itself
    /// [`Answer::decisive`] (margin-gated -- see that method's own doc
    /// comment) at merge time. `needs_clarification` always keeps the raw
    /// value regardless; a consumer that would act on it (`chat.rs::
    /// maybe_clarify`'s interactive round, `prompt_layer`'s `clarify:` line)
    /// checks THIS flag too, so a confident-looking but unstable "ambiguous"
    /// reading never interrupts a launch on its own. `#[serde(default)]` so
    /// a decision persisted before this field existed still deserializes, as
    /// `false` (never fires a clarify round retroactively).
    #[serde(default)]
    pub needs_clarification_decisive: bool,
    /// A fixed clarification question selected by the optional intake
    /// advisor. Absent on the original path, including persisted output.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub clarification_category: Option<String>,
    /// Additive domain tags a confident Jev/helper `Noul` answer added
    /// (issue #537 A2) -- `security`, `data`, `docs_only`, `devops`,
    /// `architecture`, `frontend`. Never removed once added; `#[serde(
    /// default)]` so a decision persisted before this field existed still
    /// deserializes, as an empty list.
    #[serde(default)]
    pub domains: Vec<String>,
    pub decider: Decider,
    pub confidence: BTreeMap<String, f32>,
    pub reasons: Vec<String>,
    pub fallbacks: Vec<String>,
    pub elapsed_ms: u64,
    pub usage: Option<Usage>,
    pub created_at: u64,
}

// `QuestionKind`/`Criteria`/`Question`/`AnswerValue`/`Answer`/`Answers` --
// the neutral question/answer shapes both `typesafe.rs` (via `jev::ask`) and
// `llm.rs` consume/produce, so [`merge`] never needs to know which decider
// answered -- now live in the shared `jev` module; re-exported at the top
// of this file.

#[derive(Debug, Clone, Serialize)]
pub struct IntakeWorkflow {
    pub id: String,
    pub description: String,
}

/// Issue #537 determinism fix (2026-09-18 replay): this used to also carry
/// `uncommitted_or_branch_changes`, `active_workflow` and `primary_
/// extensions` -- live-measured repository facts that differ between runs
/// and worktrees. Stripping every one of them changed no answer's accuracy
/// in that replay, so the request body (state + `questions()`) now depends
/// only on the request text, the workflow registry ([`IntakeState::
/// workflows`]) and the policy -- nothing that can silently drift the
/// intake between two calls for the same request.
#[derive(Debug, Clone, Serialize)]
pub struct IntakeRepository {
    pub name: String,
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
///
/// Issue #537 (A2): the harness/model catalogue (names, readiness, headroom,
/// prices) used to ride along here too, even though no question ever reads
/// it -- TypeSafe's own guidance is that irrelevant state degrades answer
/// accuracy, so it was dropped. The harness roster stays exactly where it
/// already lived for its one real job: [`validate`] polices the winning
/// decision's `orchestrator.harness` against the live [`Roster`] directly,
/// never against anything carried in this state.
#[derive(Debug, Clone, Serialize)]
pub struct IntakeState {
    pub request: String,
    pub repository: IntakeRepository,
    pub workflows: Vec<IntakeWorkflow>,
    pub policy: IntakePolicy,
}

/// One harness's proxy-relevant roster facts: whether it is currently
/// enabled+ready (`settings::AgentGate::is_enabled` plus `AgentAdapter::
/// ready`, the same `chat.rs::harness_list` shape without needing that
/// private function).
#[derive(Debug, Clone)]
pub struct RosterHarness {
    pub name: String,
    pub ready: bool,
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
    try_classify_request(request)
        .expect("task truncated below classify's own byte limit; classify() cannot fail here")
}

/// [`classify_request`] without its `expect`, for a caller that must never
/// panic (issue #753: the `UserPromptSubmit` hook's intake discipline, where
/// any failure means "inject nothing"). Pure CPU on the request text.
pub fn try_classify_request(request: &str) -> Option<Classification> {
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
    .ok()?;
    classification.reasons.push(
        "classification: request text only; repository diff not measured at intake".to_string(),
    );
    // With no paths or lines, `classify` can only ever answer `Trivial`, and
    // a metadata-only Jev intake sees nothing better -- so every multi-part
    // spec was routed to the cheap seat (wrapped-vs-vanilla benchmark,
    // 2026-09-23: large tasks lost points on haiku). The request's own size
    // is the one scope signal intake has.
    let size_floor = request_size_floor(request);
    if size_floor > classification.complexity {
        classification.complexity = size_floor;
        classification.reasons.push(format!(
            "complexity: request size floors it at {}",
            format!("{size_floor:?}").to_ascii_lowercase()
        ));
    }
    classification.reasons.sort();
    Some(classification)
}

/// A long or enumerated request describes several requirements; never
/// `Architectural`, which needs real paths to justify.
fn request_size_floor(request: &str) -> Complexity {
    let words = request.split_whitespace().count();
    let items = request
        .lines()
        .map(str::trim_start)
        .filter(|line| {
            line.starts_with("- ")
                || line.starts_with("* ")
                || line.split_once(['.', ')']).is_some_and(|(n, rest)| {
                    (1..=2).contains(&n.len())
                        && n.bytes().all(|b| b.is_ascii_digit())
                        && rest.starts_with(' ')
                })
        })
        .count();
    if words >= 300 || items >= 8 {
        Complexity::Substantial
    } else if words >= 120 || items >= 3 {
        Complexity::Bounded
    } else {
        Complexity::Trivial
    }
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
        needs_clarification_decisive: false,
        clarification_category: None,
        domains: Vec::new(),
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
    apply_orchestration_request_complexity_floor(&mut decision, request);
    apply_explicit_workflow_request_floor(&mut decision, request, classification, roster);
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

/// How far back of `text` a negation is allowed to reach and still be read
/// as negating a phrase that follows it. Four words covers the ordinary
/// forms ("do not parallelize", "no need to parallelize this") without
/// letting an unrelated "not" earlier in a long sentence silence a genuine
/// request two clauses later.
const NEGATION_LOOKBEHIND_WORDS: usize = 4;

/// Pure: whether `phrase` occurs in `text` as something the request ASKS
/// for, rather than something it rules out.
///
/// Review finding: matching a bare substring escalated "do not parallelize
/// this" and "no need to spawn workers" exactly as if they had asked for a
/// team. Only the few words immediately before an occurrence are examined,
/// and only for the ordinary negating words -- this is a floor over
/// operator-authored text, so the cost of missing an exotic negation is one
/// seat too many, never one too few.
fn phrase_is_asserted(text: &str, phrase: &str) -> bool {
    text.match_indices(phrase).any(|(at, _)| {
        let preceding = &text[..at];
        !preceding
            .split_whitespace()
            .rev()
            .take(NEGATION_LOOKBEHIND_WORDS)
            .any(|word| {
                matches!(
                    word.trim_matches(|c: char| !c.is_alphanumeric() && c != '\''),
                    "not" | "don't" | "dont" | "no" | "never" | "avoid" | "without" | "skip"
                )
            })
    })
}

/// Pure: whether `text` names harness `name` in its own prose, rather than
/// inside a path or URL.
///
/// Review finding: tokenizing the whole request on every non-alphanumeric
/// character turned `src/codex/client.rs` into a bare `codex` token, so
/// "fix the delegate method in src/codex/client.rs" -- one file, one method,
/// no team -- floored to `Substantial`. Any whitespace-delimited word
/// carrying a `/` is a path or a URL, never prose naming a harness to
/// delegate to, so it is dropped before the finer tokenization that finds
/// the name itself.
fn names_harness_in_prose(text: &str, name: &str) -> bool {
    prose_words(text).any(|token| token == name)
}

/// Pure: the words of `text` that are prose, in order -- every
/// whitespace-delimited word carrying a `/` dropped as a path or a URL, the
/// rest split on punctuation so `codex,` and `(codex)` still read as
/// `codex`.
fn prose_words(text: &str) -> impl Iterator<Item = &str> {
    text.split_whitespace()
        .filter(|word| !word.contains('/'))
        .flat_map(|word| word.split(|c: char| !c.is_alphanumeric() && c != '-'))
        .filter(|token| !token.is_empty())
}

/// Words that mean work is being handed off when they stand next to the name
/// of another harness. Each is worthless on its own -- "handle" is half of
/// "signal handler" and "split" is what a function does to a string -- so
/// they only count within [`DELEGATION_CUE_DISTANCE_WORDS`] of the name.
const DELEGATION_CUES: &[&str] = &[
    "delegate",
    "delegates",
    "delegated",
    "delegating",
    "handle",
    "handles",
    "split",
    "splitting",
    "across",
    "between",
    "offload",
    "assign",
];

/// How many prose words may separate a delegation cue from the harness name
/// it hands work to. Six spans the ordinary phrasings ("split this across
/// claude and codex") without letting a cue at the other end of a paragraph
/// pair up with an unrelated mention.
const DELEGATION_CUE_DISTANCE_WORDS: usize = 6;

/// Pure: whether `text` hands part of the work to harness `name`.
///
/// Review finding: matching only "delegate"-shaped wording missed the
/// ordinary ways of asking ("have codex handle the frontend part", "split
/// this across claude and codex"), so a request for a team got one seat.
/// Pairing a cue anywhere in the request with a name anywhere else is the
/// opposite mistake, hence the proximity window. It is still deliberately
/// generous -- a cue that happens to stand near an incidental mention floors
/// the request -- because the failure this floor exists to prevent is one
/// seat too few, and the deciders that can actually read intent are exactly
/// what is unavailable when it runs.
fn delegates_work_to_harness(text: &str, name: &str) -> bool {
    let words: Vec<&str> = prose_words(text).collect();
    words
        .iter()
        .enumerate()
        .filter(|(_, word)| **word == name)
        .any(|(at, _)| {
            let from = at.saturating_sub(DELEGATION_CUE_DISTANCE_WORDS);
            let to = (at + DELEGATION_CUE_DISTANCE_WORDS + 1).min(words.len());
            words[from..to]
                .iter()
                .any(|word| DELEGATION_CUES.contains(word))
        })
}

/// An explicit request for parallel or delegated multi-agent work is itself
/// a coordination requirement, even when intake has no diff to measure and
/// every model decider is unavailable. Without this floor, the text-only
/// deterministic baseline classifies such requests as `Trivial`, so a Jev
/// authentication failure followed by a helper timeout silently collapses
/// the requested team to one cheap seat. The one place this rule lives,
/// called from the tail of both [`baseline`] and [`merge`].
fn apply_orchestration_request_complexity_floor(decision: &mut ProxyDecision, request: &str) {
    let text = request.to_ascii_lowercase();
    let explicitly_parallel = [
        "parallelize",
        "parallelise",
        "in parallel",
        "parallel agents",
        "parallel workers",
    ]
    .iter()
    .any(|signal| phrase_is_asserted(&text, signal));
    let explicitly_multi_agent = [
        "multiple agents",
        "multiple workers",
        "multiple harnesses",
        "spawn agents",
        "spawn workers",
    ]
    .iter()
    .any(|signal| phrase_is_asserted(&text, signal));
    // A request that hands work to another harness by name: "have codex
    // handle the frontend part", "split this across claude and codex".
    let delegates_to_another_harness = adapters::ADAPTERS.iter().any(|(name, _)| {
        *name != decision.orchestrator.harness && delegates_work_to_harness(&text, name)
    });
    // ...or that says a share of the work goes elsewhere and names the
    // harness somewhere else in the sentence: "use codex (sol / astra) for
    // some of the work".
    let hands_off_a_share = ["some of the work", "part of the work", "split the work"]
        .iter()
        .any(|signal| phrase_is_asserted(&text, signal));
    let names_another_harness = adapters::ADAPTERS.iter().any(|(name, _)| {
        *name != decision.orchestrator.harness && names_harness_in_prose(&text, name)
    });

    if decision.complexity < Complexity::Substantial
        && (explicitly_parallel
            || explicitly_multi_agent
            || delegates_to_another_harness
            || (hands_off_a_share && names_another_harness))
    {
        decision.complexity = Complexity::Substantial;
        decision.validation.independent_test = true;
        decision.reasons.push(
            "complexity: raised to substantial because the request explicitly asks for parallel or delegated multi-agent work"
                .to_string(),
        );
    }
}

/// Governing verb/preposition immediately before a bare "a workflow" --
/// [`explicit_workflow_requests`]'s no-id templates ("start a workflow",
/// "use a workflow", "through a workflow", "with a workflow").
const WORKFLOW_VERB_A: &[&str] = &["start", "use", "through", "with"];

/// Governing verb + article immediately before a single id token and then
/// "workflow" -- [`explicit_workflow_requests`]'s id-bearing templates
/// ("start the/a <id> workflow", "use the <id> workflow", "run the <id>
/// workflow").
const WORKFLOW_VERB_ARTICLE_ID: &[(&str, &str)] = &[
    ("start", "the"),
    ("start", "a"),
    ("use", "the"),
    ("run", "the"),
];

/// Negation words that aren't themselves a contraction of an auxiliary verb
/// -- [`is_negation_word`]'s other arms generalise every "n't" form instead
/// of listing them.
const NEGATION_WORDS: &[&str] = &["not", "no", "never", "avoid", "without", "skip"];

/// Review finding: auxiliary/modal stems whose "n't" contraction is a
/// negator, checked against a word with a trailing "nt" stripped --
/// generalises "dont"/"cant"/"wont"/"isnt"/"doesnt"/"shouldnt"/... (typed
/// with no apostrophe at all) from this one small list of STEMS, rather
/// than needing an entry for every contracted FORM.
const NEGATION_AUX_STEMS: &[&str] = &[
    "do", "does", "did", "is", "are", "was", "were", "has", "have", "had", "ca", "wo", "could",
    "would", "should", "must", "need",
];

/// Whether `word` (already lowercased, a whole raw word -- see
/// [`clause_words`]) negates what follows. Review finding: rather than
/// listing every negating stem, this generalises "any `<word>n't` is a
/// negator" -- covers the straight apostrophe, the curly one (`\u{2019}`,
/// U+2019), and the same contraction typed with no apostrophe at all
/// ("dont", "cant", "wont", "isnt", "doesnt", ...) via [`NEGATION_AUX_STEMS`].
fn is_negation_word(word: &str) -> bool {
    if NEGATION_WORDS.contains(&word) {
        return true;
    }
    if let Some(stem) = word
        .strip_suffix("n't")
        .or_else(|| word.strip_suffix("n\u{2019}t"))
    {
        return !stem.is_empty();
    }
    word.strip_suffix("nt")
        .is_some_and(|stem| NEGATION_AUX_STEMS.contains(&stem))
}

/// The same [`NEGATION_LOOKBEHIND_WORDS`] distance [`phrase_is_asserted`]
/// uses, applied to [`clause_words`] rather than a raw-text byte offset --
/// [`explicit_workflow_requests`]'s id-bearing templates have a
/// variable-width slot a literal substring search can't express. Review
/// finding: scoped to ONE CLAUSE's own words -- the caller never passes a
/// window that could reach across a clause boundary into another one.
fn words_negate_before(words: &[String], before: usize) -> bool {
    words[before.saturating_sub(NEGATION_LOOKBEHIND_WORDS)..before]
        .iter()
        .any(|word| is_negation_word(word))
}

/// Review finding: a period ending one of these (compared case-
/// insensitively) closes an abbreviation, not a sentence -- "e.g."/"i.e."/
/// "etc." and the rest never end a clause, even immediately before a
/// workflow request.
const KNOWN_ABBREVIATIONS: &[&str] = &["e.g", "i.e", "etc", "vs", "cf", "approx"];

fn ends_with_known_abbreviation(word: &str) -> bool {
    KNOWN_ABBREVIATIONS
        .iter()
        .any(|abbreviation| word.eq_ignore_ascii_case(abbreviation))
}

/// The raw whitespace-delimited word ending at byte offset `end_byte` in
/// `text` -- from just after the nearest preceding whitespace character (or
/// the start of `text`) up to `end_byte`. [`split_into_clauses`] reads the
/// word a candidate sentence-ending period completes with this, so it can
/// tell a real sentence end ("Upgraded to v1.2.") from an abbreviation
/// ("e.g.").
fn word_ending_at(text: &str, end_byte: usize) -> &str {
    let word_start = text[..end_byte]
        .char_indices()
        .rev()
        .find(|&(_, c)| c.is_whitespace())
        .map(|(pos, c)| pos + c.len_utf8())
        .unwrap_or(0);
    &text[word_start..end_byte]
}

/// Splits `text` into clauses at `,`/`;`/`:`, a newline, a RUN of one or
/// more `.`/`!`/`?` immediately followed by whitespace or the end of the
/// text, or a run of two or more `-` or an em/en dash (`\u{2013}`/
/// `\u{2014}`) anywhere, whitespace or not. Review finding: consuming the
/// WHOLE punctuation run keeps "workflow..." from leaving any of the dots
/// glued to the word before it, and the dash rule keeps "workflow--let me
/// know" separated even with no surrounding whitespace at all.
/// [`explicit_workflow_requests`] matches and negates within one clause's
/// own words at a time, so negation lookback never crosses a clause
/// boundary ("fix the bug, do not refactor; start a bugfix workflow" must
/// still fire on the third clause). A single `.` NOT followed by
/// whitespace/end -- as in a registered id like "sre.postmortem" -- is never
/// a boundary candidate at all, and one that IS followed by whitespace but
/// ends a known abbreviation (see [`ends_with_known_abbreviation`]) is
/// still not a boundary, so a negation before it stays in scope.
fn split_into_clauses(text: &str) -> Vec<&str> {
    let chars: Vec<(usize, char)> = text.char_indices().collect();
    let mut clauses = Vec::new();
    let mut start = 0usize;
    let mut i = 0usize;
    while i < chars.len() {
        let (byte_pos, ch) = chars[i];
        match ch {
            ',' | ';' | ':' | '\n' => {
                clauses.push(&text[start..byte_pos]);
                start = byte_pos + ch.len_utf8();
                i += 1;
            }
            '.' | '!' | '?' => {
                let mut run_end = i;
                while run_end < chars.len() && matches!(chars[run_end].1, '.' | '!' | '?') {
                    run_end += 1;
                }
                let run_end_byte = chars.get(run_end).map_or(text.len(), |&(pos, _)| pos);
                let followed_by_whitespace_or_end = chars
                    .get(run_end)
                    .is_none_or(|&(_, next)| next.is_whitespace());
                let is_abbreviation = ch == '.'
                    && run_end == i + 1
                    && ends_with_known_abbreviation(word_ending_at(text, byte_pos));
                if followed_by_whitespace_or_end && !is_abbreviation {
                    clauses.push(&text[start..byte_pos]);
                    start = run_end_byte;
                    i = run_end;
                } else {
                    i += 1;
                }
            }
            '-' if chars.get(i + 1).map(|&(_, next)| next) == Some('-') => {
                let mut run_end = i;
                while run_end < chars.len() && chars[run_end].1 == '-' {
                    run_end += 1;
                }
                let run_end_byte = chars.get(run_end).map_or(text.len(), |&(pos, _)| pos);
                clauses.push(&text[start..byte_pos]);
                start = run_end_byte;
                i = run_end;
            }
            '\u{2013}' | '\u{2014}' => {
                clauses.push(&text[start..byte_pos]);
                start = byte_pos + ch.len_utf8();
                i += 1;
            }
            _ => i += 1,
        }
    }
    clauses.push(&text[start..]);
    clauses
}

/// Raw whitespace-delimited words for one clause, lowercased and trimmed of
/// leading/trailing non-alphanumeric characters -- backticks, quotes,
/// ordinary punctuation, and any stray `.`/`-`/`_` a clause boundary left
/// glued to a word's edge. Review finding: unlike `prose_words` (used
/// elsewhere in this module), this keeps each whitespace-delimited word
/// WHOLE apart from that edge trim, so the exact raw word adjacent to
/// "workflow" -- a registered id like `sre.postmortem`/`team_review`
/// included, since `.`/`-`/`_` INTERIOR to a word are never trimmed -- can
/// be looked up in the registry unmodified.
fn clause_words(clause: &str) -> Vec<String> {
    clause
        .split_whitespace()
        .map(|word| {
            word.trim_matches(|c: char| !c.is_alphanumeric())
                .to_ascii_lowercase()
        })
        .filter(|word| !word.is_empty())
        .collect()
}

/// Every explicit, asserted workflow-request match within one clause's own
/// words. Review finding: scans the WHOLE clause rather than returning
/// at the first match. `None` per match with no id slot ("start a
/// workflow"), `Some(word)` with the raw word found in an id-bearing slot
/// (`<id> workflow`, or `workflow start <id>`) -- that word need not itself
/// be a registered pack id; [`apply_explicit_workflow_request_floor`] is
/// what falls through to `selection::select_definition` when none of the
/// matches name one.
fn clause_matches(clause: &str) -> Vec<Option<String>> {
    let words = clause_words(clause);
    let mut matches = Vec::new();
    for i in 0..words.len() {
        if words[i] != "workflow" {
            continue;
        }
        // "<verb> a workflow" / "through a workflow" / "with a workflow".
        if i >= 2
            && WORKFLOW_VERB_A.contains(&words[i - 2].as_str())
            && words[i - 1] == "a"
            && !words_negate_before(&words, i - 2)
        {
            matches.push(None);
            continue;
        }
        // "<verb> the/a <id> workflow" -- the id is words[i - 1].
        if i >= 3
            && WORKFLOW_VERB_ARTICLE_ID.contains(&(words[i - 3].as_str(), words[i - 2].as_str()))
            && !words_negate_before(&words, i - 3)
        {
            matches.push(Some(words[i - 1].clone()));
            continue;
        }
        // "run this through a zirv workflow".
        if i >= 5
            && words[i - 5..i]
                .iter()
                .map(String::as_str)
                .eq(["run", "this", "through", "a", "zirv"])
            && !words_negate_before(&words, i - 5)
        {
            matches.push(None);
            continue;
        }
        // Literal "zirv workflow start", optionally followed by an id
        // ("workflow start <id>").
        if i >= 1
            && words[i - 1] == "zirv"
            && words.get(i + 1).map(String::as_str) == Some("start")
            && !words_negate_before(&words, i - 1)
        {
            matches.push(words.get(i + 2).cloned());
            continue;
        }
    }
    matches
}

/// Lead words that unconditionally mark a request as explanatory, whatever
/// follows -- "Explain what `zirv workflow start bugfix` does" describes,
/// it doesn't assert.
const EXPLANATORY_LEAD_WORDS: &[&str] = &["explain", "describe"];

/// Interrogative lead words that mark a request as explanatory only when
/// the request is actually a question (see [`is_explanatory_request`]).
/// Review finding: a leading "what"/"is"/... does not by itself mean the
/// request only describes or asks -- "What I need: start a bugfix workflow
/// for the login crash" and "Is broken -- start a bugfix workflow" both
/// assert one, and must still fire.
const INTERROGATIVE_LEAD_WORDS: &[&str] =
    &["what", "how", "why", "when", "where", "which", "does", "is"];

/// Whether `request_lower` (already lowercased) is explanatory or
/// interrogative -- checked once, against the WHOLE request's own first
/// word, never per clause. `true` when the first word is one of
/// [`EXPLANATORY_LEAD_WORDS`] outright, or one of
/// [`INTERROGATIVE_LEAD_WORDS`] AND the request (trimmed) ends with `?`.
/// "How do I start a workflow for a refactor?" is suppressed this way;
/// "Can you start a bugfix workflow for the login crash?" is not --
/// can/could/would/will/please are deliberately excluded from both lists,
/// so a polite request still fires.
fn is_explanatory_request(request_lower: &str) -> bool {
    let Some(first_word) = request_lower
        .split_whitespace()
        .next()
        .map(|word| word.trim_matches(|c: char| !c.is_alphanumeric()))
    else {
        return false;
    };
    EXPLANATORY_LEAD_WORDS.contains(&first_word)
        || (INTERROGATIVE_LEAD_WORDS.contains(&first_word)
            && request_lower.trim_end().ends_with('?'))
}

/// Every explicit, assertively-requested workflow match in `request_lower`
/// (already lowercased). Empty when the request's own first word is
/// explanatory/interrogative, or when no clause has an asserted match
/// at all. Otherwise, [`split_into_clauses`] keeps negation lookback
/// from crossing a clause boundary, and [`clause_matches`] scans every
/// match in every clause rather than stopping at the first -- in the
/// returned order, the FIRST entry naming a REGISTERED pack id is what
/// [`apply_explicit_workflow_request_floor`]/[`explicit_registered_workflow_id`]
/// use; a registered id anywhere always beats an unnamed match.
///
/// Deliberately narrow: "Fix the bug in the workflow status command" and
/// "Refactor the workflow registry loader" mention "workflow" with no
/// governing verb or preposition directly next to it, so neither clause
/// matches anything here.
fn explicit_workflow_requests(request_lower: &str) -> Vec<Option<String>> {
    if is_explanatory_request(request_lower) {
        return Vec::new();
    }
    split_into_clauses(request_lower)
        .into_iter()
        .flat_map(clause_matches)
        .collect()
}

/// The registered pack id an operator named adjacent to "workflow" in
/// `request` (`<id> workflow`, or `workflow start <id>`), when
/// [`explicit_workflow_requests`] finds at least one match. `None` when the
/// request doesn't assert an explicit workflow request at all, or none of
/// its matches name a word that is actually a registered pack id -- both
/// are for [`apply_explicit_workflow_request_floor`] to fall through to
/// `selection::select_definition` for, not this function's concern. The
/// FIRST (leftmost) match naming a registered id wins when several matches
/// exist.
fn explicit_registered_workflow_id(request: &str, roster: &Roster) -> Option<String> {
    let registry = roster.registry.as_ref()?;
    explicit_workflow_requests(&request.to_ascii_lowercase())
        .into_iter()
        .flatten()
        .find(|candidate| registry.get(candidate).is_ok())
}

/// An operator who explicitly asks for a workflow gets one even when every
/// model decider is off or unavailable -- `classify_request`'s deliberately
/// text-only baseline (issue #537) otherwise classifies most such requests
/// `Trivial`, and a `Trivial` baseline never sets `workflow` at all (see
/// [`baseline`]'s own early `match classification.complexity`).
///
/// Fires only when [`explicit_workflow_requests`] finds at least one match.
/// When it does: `complexity` is raised to at least `Bounded` (never
/// lowered) so [`apply_direct_execution_workflow_rule`] doesn't wipe the
/// `workflow` this function is about to set right back out, and `workflow`
/// becomes the REGISTERED pack id named adjacent to "workflow" when there is
/// one (`explicit_registered_workflow_id`), else whatever
/// `selection::select_definition` picks for the request text against
/// `classification`. No registry at all in `roster` leaves `workflow`
/// untouched either way. The one place this rule lives, called from the
/// tail of both [`baseline`] and [`merge`], before
/// [`finalize_derived_fields`] derives `execution` from the (possibly
/// just-raised) `complexity`.
fn apply_explicit_workflow_request_floor(
    decision: &mut ProxyDecision,
    request: &str,
    classification: &Classification,
    roster: &Roster,
) {
    let text = request.to_ascii_lowercase();
    if explicit_workflow_requests(&text).is_empty() {
        return;
    }
    if let Some(registry) = roster.registry.as_ref() {
        let resolved = explicit_registered_workflow_id(request, roster).unwrap_or_else(|| {
            selection::select_definition(classification, registry, request).definition_id
        });
        decision.workflow = Some(resolved);
    }
    if decision.complexity < Complexity::Bounded {
        decision.complexity = Complexity::Bounded;
        decision.reasons.push(
            "complexity: raised to bounded because the request explicitly asks for a workflow"
                .to_string(),
        );
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
/// direct-execution/workflow rule, `seat_tier` (from the FINAL `execution`
/// plus `complexity`/`risk` -- see [`SeatTier::from_execution_complexity_risk`]),
/// `worker_tier`/`seat_role` (from the final `execution` alone, unchanged),
/// and the orchestrator's own resolved `model` (from `seat_tier` via
/// `handover::resolve_model`, see [`model_for_tier`]). The ONE place all of
/// this is computed, called at the tail of both [`baseline`] and [`merge`],
/// after [`apply_security_risk_floor`] has already had its say on `risk`, so
/// the frontier gate below always sees the final, floor-raised risk.
fn finalize_derived_fields(decision: &mut ProxyDecision, cfg: &CtxConfig) {
    decision.execution = execution_from_complexity(decision.complexity);
    let complexity_label = format!("{:?}", decision.complexity).to_lowercase();
    decision.reasons.push(format!(
        "execution: derived from complexity {complexity_label}"
    ));
    apply_risk_execution_floor(decision);
    apply_direct_execution_workflow_rule(decision);
    decision.seat_tier = SeatTier::from_execution_complexity_risk(
        decision.execution,
        decision.complexity,
        decision.risk,
    );
    decision.worker_tier = worker_tier_from_execution(decision.execution);
    decision.seat_role = SeatRole::from_execution(decision.execution);
    decision.orchestrator.model =
        model_for_tier(cfg, &decision.orchestrator.harness, decision.seat_tier);
}

/// Builds the Jev `state`/`questions()` input: the request (truncated to
/// `cfg.proxy.request_max_bytes`), the repository's own name, the registered
/// workflow ids/descriptions, and whether the native runtime is available.
/// Issue #537 determinism fix (2026-09-18 replay): no longer measures the
/// repository at all -- see [`IntakeRepository`]'s own doc comment for why;
/// `state_dir` is accepted only for call-site parity with every other
/// `build_*`-shaped seam in this crate and is not read. Issue #537 (A2):
/// also no longer carries the harness/model catalogue -- see [`IntakeState`]'s
/// own doc comment for why.
pub fn build_intake(
    cfg: &CtxConfig,
    repo: &Path,
    _state_dir: &Path,
    request: &str,
    roster: &Roster,
) -> IntakeState {
    let repo_name = repo
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| repo.display().to_string());
    IntakeState {
        request: truncate_bytes(request, cfg.proxy.request_max_bytes.max(1)),
        repository: IntakeRepository { name: repo_name },
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
        metadata_signature: None,
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
        metadata_signature: None,
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
        metadata_signature: None,
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
        metadata_signature: None,
        instructions: "Is this request too ambiguous to start without asking one clarifying \
                       question first?"
            .to_string(),
        criteria: Criteria::Noul {
            when_true: Some("too ambiguous; ask one question before starting".to_string()),
            when_false: Some("clear enough to start now".to_string()),
        },
    });

    // Issue #537 (A2): additive domain tags, one Noul question per tag
    // (`DOMAIN_QUESTION_IDS`, the single source of truth `merge` reads back
    // by the same ids). A substring keyword match (`ExecutionProfile::
    // derive`'s own domain detection) misses phrasing that never uses one of
    // its fixed keywords -- "rotate the shared token" names no keyword in
    // its `security` list at all -- so these ask the model directly instead.
    // `security`'s own confident `true` answer floors risk/execution the
    // same way the keyword trigger already does (see `merge`); the other
    // five are informational only.
    for (id, (what, when_true, when_false)) in
        DOMAIN_QUESTION_IDS.into_iter().zip(DOMAIN_NOUL_QUESTIONS)
    {
        out.push(Question::noul(id, what, when_true, when_false));
    }

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
/// rules the spec's "Decision fields" table sets: an answer that is not
/// [`Answer::decisive`] (either its confidence is below `min_confidence`, or
/// its margin is below `cfg.proxy.min_margin` -- see that method's own doc
/// comment for why margin, not confidence alone, is what catches the
/// 2026-09-18 replay's instability) resolves complexity/risk to the higher
/// of its two most probable levels -- but only when it is the MARGIN that
/// fell short, see [`Answer::near_tie_score`] for why a below-floor
/// confidence keeps the baseline instead. Complexity/risk only ever rise, so
/// that resolved level is taken only when it is above the baseline. The
/// recorded reason names the outcome that actually happened (`resolved
/// upward to <label>` or `kept baseline`), never a level the comparison
/// discarded. Every other
/// ASKED field (`intent`, `workflow`, a domain tag) is replaced/added outright
/// when decisive. Existence checks against the live roster (a workflow id, a
/// harness/model pair) are deferred to [`validate`], which runs right after
/// this and has the `Roster` this function does not need.
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
/// `cfg` is needed for [`finalize_derived_fields`]'s own resolution of the
/// orchestrator's model via `handover::resolve_model`, and for
/// `cfg.proxy.min_margin`.
pub fn merge(
    cfg: &CtxConfig,
    baseline: &ProxyDecision,
    request: &str,
    answers: &Answers,
    min_confidence: f32,
    roster: &Roster,
) -> ProxyDecision {
    let min_margin = cfg.proxy.min_margin;
    let mut decision = baseline.clone();
    decision.confidence = answers
        .iter()
        .map(|(id, answer)| (id.clone(), answer.confidence))
        .collect();

    let mut record_not_decisive = |id: &str, answer: &Answer, resolved: Option<String>| {
        let outcome = resolved.map_or_else(
            || "kept baseline".to_string(),
            |label| format!("resolved upward to {label}"),
        );
        let reason = if answer.confidence < min_confidence {
            format!(
                "{id}: confidence {:.2} < {:.2}, {outcome}",
                answer.confidence, min_confidence
            )
        } else {
            format!(
                "{id}: margin {:.2} < {:.2}, {outcome}",
                answer.margin(),
                min_margin
            )
        };
        decision.reasons.push(reason);
    };

    if let Some(answer) = answers.get("intent") {
        if !answer.decisive(min_confidence, min_margin) {
            record_not_decisive("intent", answer, None);
        } else if let AnswerValue::Choice(value) = &answer.value
            && let Some(intent) = parse_intent(value)
        {
            decision.intent = intent;
        }
    }

    if let Some(answer) = answers.get("complexity") {
        if !answer.decisive(min_confidence, min_margin) {
            let raised = answer
                .near_tie_score(min_confidence)
                .map(complexity_from_index)
                .filter(|resolved| *resolved > decision.complexity);
            if let Some(complexity) = raised {
                decision.complexity = complexity;
            }
            record_not_decisive(
                "complexity",
                answer,
                raised.map(|value| format!("{value:?}").to_lowercase()),
            );
        } else if let AnswerValue::Score(value) = answer.value {
            decision.complexity = decision.complexity.max(complexity_from_index(value));
        }
    }

    if let Some(answer) = answers.get("risk") {
        if !answer.decisive(min_confidence, min_margin) {
            let raised = answer
                .near_tie_score(min_confidence)
                .map(risk_from_index)
                .filter(|resolved| *resolved > decision.risk);
            if let Some(risk) = raised {
                decision.risk = risk;
            }
            record_not_decisive(
                "risk",
                answer,
                raised.map(|value| format!("{value:?}").to_lowercase()),
            );
        } else if let AnswerValue::Score(value) = answer.value {
            decision.risk = decision.risk.max(risk_from_index(value));
        }
    }

    if let Some(answer) = answers.get("workflow") {
        if !answer.decisive(min_confidence, min_margin) {
            record_not_decisive("workflow", answer, None);
        } else if let AnswerValue::Choice(value) = &answer.value {
            // An operator-named, REGISTERED workflow id (baseline already
            // carries it, via `apply_explicit_workflow_request_floor`)
            // survives even a decisive model answer -- the operator said it
            // in words. A
            // baseline id `select_definition` merely guessed at is still
            // fair game for a confident model answer to replace, as today.
            if explicit_registered_workflow_id(request, roster).is_none() {
                decision.workflow = if value == "none" {
                    None
                } else {
                    Some(value.clone())
                };
            }
        }
    }

    // Advisory only: the raw value is always kept (never gated), since the
    // value itself IS the model's own confidence in "this is ambiguous" (see
    // the noul conversions in `typesafe.rs`/`llm.rs`). Whether a CONSUMER
    // (`chat.rs::maybe_clarify`, `prompt_layer`) actually acts on it -- fires
    // the interactive clarify round, or adds the `clarify:` context line --
    // is gated separately, on `needs_clarification_decisive`: `Answer::
    // decisive` ignores `min_confidence` for a `Noul` (see its own doc
    // comment), so this is a margin-only check.
    if let Some(answer) = answers.get("needs_clarification")
        && let AnswerValue::Noul(value) = answer.value
    {
        decision.needs_clarification = value as f32;
        decision.needs_clarification_decisive = answer.decisive(min_confidence, min_margin);
    }

    // Issue #537 (A2): additive domain tags -- a decisive `true` noul answer
    // adds that domain; nothing ever removes one. A confident-but-thin-margin
    // `true` answer now falls back to "not added" (the deterministic
    // baseline never has a domain tag of its own), recorded the same way a
    // discarded intent/workflow answer is. `security`'s own
    // tag sets the same validation flags the keyword-based `ExecutionProfile
    // ::derive` detection sets below, so `apply_security_risk_floor` floors
    // risk/execution the same way regardless of which detector caught it.
    for id in DOMAIN_QUESTION_IDS {
        if let Some(answer) = answers.get(id)
            && let AnswerValue::Noul(value) = answer.value
            && value >= 0.5
        {
            if !answer.decisive(min_confidence, min_margin) {
                record_not_decisive(id, answer, None);
            } else if !decision.domains.iter().any(|domain| domain == id) {
                decision.domains.push(id.to_string());
            }
        }
    }
    if decision.domains.iter().any(|domain| domain == "security") {
        decision.validation.independent_review = true;
        decision.validation.security_review = true;
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
    apply_orchestration_request_complexity_floor(&mut decision, request);
    apply_explicit_workflow_request_floor(
        &mut decision,
        request,
        &recomputed_classification,
        roster,
    );
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
            needs_clarification_decisive: false,
            clarification_category: None,
            domains: Vec::new(),
            decider: Decider::Deterministic,
            confidence: BTreeMap::new(),
            reasons: Vec::new(),
            fallbacks: Vec::new(),
            elapsed_ms: 0,
            usage: None,
            created_at: 0,
        }
    }

    /// A `Roster` with no known harnesses and no loaded registry -- the
    /// right fixture for any `merge` test that isn't itself exercising
    /// registry-aware behaviour (`apply_explicit_workflow_request_floor`
    /// and its merge-survival rule both no-op without a registry).
    fn empty_roster() -> Roster {
        Roster {
            harnesses: Vec::new(),
            registry: None,
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

    /// A `Choice`/`Score` answer's `probabilities` gets a synthetic
    /// two-entry distribution whose margin tracks `confidence` (`top -
    /// runner_up = 2 * top.max(0.5) - 1`, always non-negative): high
    /// confidence gives a wide margin, so every EXISTING test below that
    /// means "a confident answer" stays decisive under `Answer::decisive`'s
    /// margin gate without hand-building a full distribution of its own. A
    /// test that means to exercise a THIN margin specifically constructs its
    /// own `Answer` instead (see the merge tests below this helper). `Noul`
    /// needs no probabilities at all -- its margin comes from the raw value.
    fn answers(pairs: &[(&str, AnswerValue, f32)]) -> Answers {
        pairs
            .iter()
            .map(|(id, value, confidence)| {
                let probabilities = match value {
                    AnswerValue::Choice(_) | AnswerValue::Score(_) => {
                        let top = confidence.max(0.5);
                        BTreeMap::from([("top".to_string(), top), ("rest".to_string(), 1.0 - top)])
                    }
                    AnswerValue::Noul(_) => BTreeMap::new(),
                };
                (
                    (*id).to_string(),
                    Answer {
                        value: value.clone(),
                        confidence: *confidence,
                        probabilities,
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
        let merged = merge(
            &cfg,
            &baseline,
            "implement the feature",
            &ans,
            0.5,
            &empty_roster(),
        );
        assert_eq!(merged.complexity, Complexity::Architectural);
    }

    #[test]
    fn a_confident_lower_complexity_never_lowers_the_baseline() {
        let cfg = CtxConfig::default();
        let mut baseline = sample_decision();
        baseline.complexity = Complexity::Substantial;
        let ans = answers(&[("complexity", AnswerValue::Score(0.0), 0.95)]);
        let merged = merge(
            &cfg,
            &baseline,
            "implement the feature",
            &ans,
            0.5,
            &empty_roster(),
        );
        assert_eq!(
            merged.complexity,
            Complexity::Substantial,
            "a model's lower reading must never lower the baseline"
        );
    }

    #[test]
    fn a_low_confidence_score_without_parseable_indices_keeps_the_baseline() {
        let cfg = CtxConfig::default();
        let baseline = sample_decision();
        let ans = answers(&[("risk", AnswerValue::Score(3.0), 0.2)]);
        let merged = merge(
            &cfg,
            &baseline,
            "implement the feature",
            &ans,
            0.5,
            &empty_roster(),
        );
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

    #[test]
    fn a_thin_margin_risk_answer_resolves_upward_and_records_a_margin_reason() {
        let cfg = CtxConfig::default();
        let baseline = sample_decision();
        let mut ans = Answers::new();
        ans.insert(
            "risk".to_string(),
            Answer {
                value: AnswerValue::Score(3.0),
                confidence: 0.9,
                probabilities: BTreeMap::from([
                    ("2".to_string(), 0.51_f32),
                    ("3".to_string(), 0.49_f32),
                ]),
            },
        );
        let merged = merge(
            &cfg,
            &baseline,
            "implement the feature",
            &ans,
            0.5,
            &empty_roster(),
        );
        assert_eq!(merged.risk, RiskBand::Critical);
        assert!(
            merged
                .reasons
                .iter()
                .any(|reason| reason.starts_with("risk: margin 0.02 < ")
                    && reason.contains("resolved upward to critical")),
            "{:?}",
            merged.reasons
        );
    }

    /// Renamed by the wrapper-overhead benchmark's frontier seat gate: this
    /// exact near-tie shape (a multi-step production investigation) is one
    /// of the two cases the benchmark found riding a frontier seat for no
    /// correctness gain -- `Substantial` complexity at `Low` risk now earns
    /// a `Standard` orchestrator seat instead, never `Frontier`.
    #[test]
    fn the_production_complexity_near_tie_selects_a_standard_orchestrator() {
        let cfg = CtxConfig::default();
        let mut baseline = sample_decision();
        baseline.complexity = Complexity::Trivial;
        baseline.risk = RiskBand::Low;
        baseline.execution = ExecutionMode::Direct;
        baseline.seat_tier = SeatTier::Cheap;
        baseline.orchestrator.model = "haiku".to_string();
        let ans = Answers::from([(
            "complexity".to_string(),
            Answer {
                value: AnswerValue::Score(1.0),
                confidence: 0.57,
                probabilities: BTreeMap::from([
                    ("0".to_string(), 0.0_f32),
                    ("1".to_string(), 0.57_f32),
                    ("2".to_string(), 0.43_f32),
                    ("3".to_string(), 0.0_f32),
                ]),
            },
        )]);
        let merged = merge(
            &cfg,
            &baseline,
            "Do an exhaustive investigation of why some contacts received emails and sms one day too late on the ortto journey; check Kibana and the kafka topic",
            &ans,
            0.5,
            &empty_roster(),
        );
        assert_eq!(merged.complexity, Complexity::Substantial);
        assert_eq!(merged.execution, ExecutionMode::Orchestrated);
        assert_eq!(merged.seat_role, SeatRole::Orchestrator);
        assert_eq!(merged.seat_tier, SeatTier::Standard);
        assert_eq!(merged.orchestrator.model, "sonnet");
        assert!(
            merged
                .reasons
                .iter()
                .any(|reason| reason.starts_with("complexity: margin 0.14 < ")
                    && reason.ends_with("resolved upward to substantial")),
            "{:?}",
            merged.reasons
        );
    }

    #[test]
    fn a_thin_margin_lower_complexity_never_lowers_the_baseline() {
        let cfg = CtxConfig::default();
        let mut baseline = sample_decision();
        baseline.complexity = Complexity::Substantial;
        let ans = Answers::from([(
            "complexity".to_string(),
            Answer {
                value: AnswerValue::Score(0.0),
                confidence: 0.5,
                probabilities: BTreeMap::from([
                    ("0".to_string(), 0.5_f32),
                    ("1".to_string(), 0.5_f32),
                ]),
            },
        )]);
        let merged = merge(
            &cfg,
            &baseline,
            "implement the feature",
            &ans,
            0.5,
            &empty_roster(),
        );
        assert_eq!(merged.complexity, Complexity::Substantial);
        assert!(
            merged
                .reasons
                .iter()
                .any(|reason| reason.starts_with("complexity: margin ")
                    && reason.ends_with("kept baseline")),
            "a resolution the baseline outranks must not claim it raised the field: {:?}",
            merged.reasons
        );
    }

    /// A confidence below the floor is not the near-tie this path resolves:
    /// the model has no opinion, so the baseline stands. Escalating here
    /// instead over-sized the live battery's `bump-timeout` and `ambiguous`
    /// cases (trivial/direct/cheap) into bounded work on a standard seat.
    #[test]
    fn a_low_confidence_complexity_answer_keeps_the_baseline() {
        let cfg = CtxConfig::default();
        let baseline = sample_decision();
        let ans = Answers::from([(
            "complexity".to_string(),
            Answer {
                value: AnswerValue::Score(1.0),
                confidence: 0.45,
                probabilities: BTreeMap::from([
                    ("0".to_string(), 0.1_f32),
                    ("1".to_string(), 0.45_f32),
                    ("2".to_string(), 0.4_f32),
                    ("3".to_string(), 0.05_f32),
                ]),
            },
        )]);
        let merged = merge(
            &cfg,
            &baseline,
            "implement the feature",
            &ans,
            0.5,
            &empty_roster(),
        );
        assert_eq!(merged.complexity, baseline.complexity);
        assert!(
            merged
                .reasons
                .iter()
                .any(|reason| reason == "complexity: confidence 0.45 < 0.50, kept baseline"),
            "{:?}",
            merged.reasons
        );
    }

    /// Issue #537 (A2): a confident `true` noul answer for a domain question
    /// adds that tag to `domains`; an unconfident/`false` one does not, and
    /// tags accumulate rather than replace each other.
    #[test]
    fn confident_domain_answers_accumulate_and_others_are_skipped() {
        let cfg = CtxConfig::default();
        let baseline = sample_decision();
        let ans = answers(&[
            ("security", AnswerValue::Noul(0.9), 0.9),
            ("data", AnswerValue::Noul(0.8), 0.8),
            ("docs_only", AnswerValue::Noul(0.1), 0.9),
        ]);
        let merged = merge(
            &cfg,
            &baseline,
            "rotate the shared token",
            &ans,
            0.5,
            &empty_roster(),
        );
        assert_eq!(
            merged.domains,
            vec!["security".to_string(), "data".to_string()]
        );
    }

    /// Issue #537 (A2): a confident `security` domain answer floors risk/
    /// execution exactly like the keyword-based `ExecutionProfile::derive`
    /// trigger already does -- the whole point of asking the model directly
    /// is to catch phrasing the fixed keyword list misses ("token" names no
    /// keyword in that list at all).
    #[test]
    fn a_confident_security_domain_answer_floors_risk_and_execution() {
        let cfg = CtxConfig::default();
        let mut baseline = sample_decision();
        baseline.complexity = Complexity::Trivial;
        baseline.risk = RiskBand::Low;
        baseline.execution = ExecutionMode::Direct;
        let ans = answers(&[("security", AnswerValue::Noul(0.95), 0.95)]);
        let merged = merge(
            &cfg,
            &baseline,
            "rotate the shared token",
            &ans,
            0.5,
            &empty_roster(),
        );
        assert_eq!(merged.domains, vec!["security".to_string()]);
        assert!(merged.validation.security_review);
        assert!(merged.validation.independent_review);
        assert_eq!(merged.risk, RiskBand::High);
        assert_eq!(merged.execution, ExecutionMode::Bounded);
    }

    /// Jev determinism fix: a `true`-labeled `security` domain answer (0.55,
    /// margin 0.1 -- below `jev::DEFAULT_MIN_MARGIN`) must not add the tag or
    /// floor risk/execution -- a noul has no separate confidence to check
    /// (`Answer::decisive` ignores `min_confidence` for it), so margin alone
    /// governs, and a barely-over-half reading is exactly the kind of
    /// unstable answer the floor exists to catch.
    #[test]
    fn a_thin_margin_security_domain_answer_is_not_added_and_never_floors_anything() {
        let cfg = CtxConfig::default();
        let mut baseline = sample_decision();
        baseline.complexity = Complexity::Trivial;
        baseline.risk = RiskBand::Low;
        baseline.execution = ExecutionMode::Direct;
        let ans = answers(&[("security", AnswerValue::Noul(0.55), 0.95)]);
        let merged = merge(
            &cfg,
            &baseline,
            "rotate the shared token",
            &ans,
            0.5,
            &empty_roster(),
        );
        assert!(merged.domains.is_empty(), "{:?}", merged.domains);
        assert!(!merged.validation.security_review);
        assert!(!merged.validation.independent_review);
        assert_eq!(merged.risk, RiskBand::Low);
        assert_eq!(merged.execution, ExecutionMode::Direct);
        assert!(
            merged
                .reasons
                .iter()
                .any(|reason| reason.starts_with("security: margin 0.10 < ")),
            "{:?}",
            merged.reasons
        );
    }

    /// Issue #537 (A2)/Jev determinism fix: `needs_clarification` always
    /// keeps the model's raw value regardless of margin, but `needs_
    /// clarification_decisive` reflects `Answer::decisive` (margin-only for
    /// a noul) -- a wide-margin answer (0.9) is decisive, a thin-margin one
    /// (0.52, margin 0.04) is not, even though both keep the same raw value
    /// semantics a consumer would otherwise read as "confidently ambiguous".
    #[test]
    fn needs_clarification_keeps_the_raw_value_but_decisive_follows_margin_only() {
        let cfg = CtxConfig::default();
        let baseline = sample_decision();
        let wide = answers(&[("needs_clarification", AnswerValue::Noul(0.9), 0.9)]);
        let merged = merge(&cfg, &baseline, "a request", &wide, 0.5, &empty_roster());
        assert_eq!(merged.needs_clarification, 0.9);
        assert!(merged.needs_clarification_decisive);

        let thin = answers(&[("needs_clarification", AnswerValue::Noul(0.52), 0.52)]);
        let merged = merge(&cfg, &baseline, "a request", &thin, 0.5, &empty_roster());
        assert_eq!(
            merged.needs_clarification, 0.52,
            "the raw value is kept regardless of decisiveness"
        );
        assert!(!merged.needs_clarification_decisive);
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
        let merged = merge(&cfg, &baseline, "fix the typo", &ans, 0.5, &empty_roster());
        assert_eq!(
            merged.execution,
            ExecutionMode::Direct,
            "an 'execution' answer must never move execution on its own"
        );

        let ans = answers(&[("complexity", AnswerValue::Score(3.0), 0.9)]);
        let merged = merge(&cfg, &baseline, "fix the typo", &ans, 0.5, &empty_roster());
        assert_eq!(merged.complexity, Complexity::Architectural);
        assert_eq!(
            merged.execution,
            ExecutionMode::Orchestrated,
            "execution rises when a confident complexity answer raises it"
        );
    }

    /// Issue #537 design revision, revised by the wrapper-overhead
    /// benchmark's frontier seat gate: `execution`/`worker_tier`/`seat_role`
    /// still follow `complexity` alone, exercised across the whole ladder --
    /// `Trivial` a single cheap seat, `Bounded` a single standard seat,
    /// `Substantial`/`Architectural` an orchestrator with standard-tier
    /// workers. `seat_tier` no longer follows complexity alone: at the
    /// `Low` risk every case in this ladder carries (from `sample_decision`),
    /// `Substantial` earns only a `Standard` orchestrator seat, while
    /// `Architectural` still earns `Frontier` unconditionally -- see
    /// `frontier_requires_architectural_complexity_or_high_risk` for the
    /// risk-gated half of the rule.
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
                SeatTier::Standard,
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
            let merged = merge(&cfg, &baseline, "a request", &ans, 0.5, &empty_roster());
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
        let merged = merge(
            &cfg,
            &baseline,
            "change the background color",
            &ans,
            0.5,
            &empty_roster(),
        );
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

    /// Issue #537, revised by the wrapper-overhead benchmark: the baseline
    /// maps `seat_tier` from `execution` alone for `Direct`/`Bounded`;
    /// `Orchestrated` additionally needs `complexity`/`risk` -- see
    /// `frontier_requires_architectural_complexity_or_high_risk` for that
    /// half of the rule.
    #[test]
    fn baseline_seat_tier_follows_execution_complexity_and_risk() {
        assert_eq!(
            SeatTier::from_execution_complexity_risk(
                ExecutionMode::Direct,
                Complexity::Trivial,
                RiskBand::Low
            ),
            SeatTier::Cheap
        );
        assert_eq!(
            SeatTier::from_execution_complexity_risk(
                ExecutionMode::Bounded,
                Complexity::Bounded,
                RiskBand::Medium
            ),
            SeatTier::Standard
        );
        assert_eq!(
            SeatTier::from_execution_complexity_risk(
                ExecutionMode::Orchestrated,
                Complexity::Architectural,
                RiskBand::Low
            ),
            SeatTier::Frontier
        );
    }

    /// Change 1 (frontier seat gate): the wrapper-overhead benchmark found
    /// `Substantial` complexity alone routing to a frontier orchestrator
    /// seat at 1.7-2.2x cost with no correctness gain. `Frontier` now
    /// requires either `Architectural` complexity or `High`+ risk while
    /// `Orchestrated`; a `Substantial` task at lower risk gets `Standard`.
    #[test]
    fn frontier_requires_architectural_complexity_or_high_risk() {
        assert_eq!(
            SeatTier::from_execution_complexity_risk(
                ExecutionMode::Orchestrated,
                Complexity::Substantial,
                RiskBand::Low
            ),
            SeatTier::Standard,
            "substantial + low risk must not earn the frontier seat"
        );
        assert_eq!(
            SeatTier::from_execution_complexity_risk(
                ExecutionMode::Orchestrated,
                Complexity::Substantial,
                RiskBand::High
            ),
            SeatTier::Frontier,
            "substantial + high risk still earns the frontier seat"
        );
        assert_eq!(
            SeatTier::from_execution_complexity_risk(
                ExecutionMode::Orchestrated,
                Complexity::Architectural,
                RiskBand::Low
            ),
            SeatTier::Frontier,
            "architectural complexity earns the frontier seat regardless of risk"
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
            &roster,
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
        let raised = merge(
            &cfg,
            &plain_baseline,
            plain_request,
            &high_risk_answer,
            0.5,
            &roster,
        );
        assert_eq!(raised.risk, RiskBand::High);
        assert!(raised.validation.security_review);
        assert!(raised.validation.independent_review);
    }

    #[test]
    fn explicit_parallel_delegation_floors_baseline_and_merge_to_orchestrated() {
        let request = concat!(
            "We need to add a way of creating / adding short links to sms' within the marketing ",
            "backoffice. We have some existing logic regarding short links in the monolith at ",
            "the moment, but i dont know how this works. Linear card: ",
            "https://linear.app/cego/issue/MARKAU-131/undersog-hvordan-vi-skal-gore-i-forhold-til-shortlinks-til-bla-smser. ",
            "Parallelize as much of the work as possible, and use codex (sol / astra) for some ",
            "of the work."
        );
        let classification = classify_request(request);
        assert_eq!(classification.complexity, Complexity::Trivial);

        let repo = tempfile::tempdir().expect("tempdir");
        let cfg = CtxConfig::default();
        let roster = Roster {
            harnesses: Vec::new(),
            registry: None,
        };
        let decision = baseline(&cfg, repo.path(), request, &classification, &roster);
        assert_eq!(decision.complexity, Complexity::Substantial);
        assert_eq!(decision.execution, ExecutionMode::Orchestrated);
        assert_eq!(decision.seat_role, SeatRole::Orchestrator);
        assert!(decision.validation.independent_test);
        assert!(decision.reasons.iter().any(|reason| {
            reason.contains("request explicitly asks for parallel or delegated multi-agent work")
        }));

        let mut unfloored_baseline = sample_decision();
        unfloored_baseline.complexity = Complexity::Trivial;
        unfloored_baseline.execution = ExecutionMode::Direct;
        unfloored_baseline.seat_role = SeatRole::Single;
        unfloored_baseline.validation = ValidationProfile::default();
        let merged = merge(
            &cfg,
            &unfloored_baseline,
            request,
            &Answers::new(),
            0.5,
            &roster,
        );
        assert_eq!(merged.complexity, Complexity::Substantial);
        assert_eq!(merged.execution, ExecutionMode::Orchestrated);
        assert_eq!(merged.seat_role, SeatRole::Orchestrator);
        assert!(merged.validation.independent_test);
    }

    #[test]
    fn classify_request_floors_complexity_by_request_size() {
        assert_eq!(
            classify_request("Fix the typo in the README").complexity,
            Complexity::Trivial
        );
        let enumerated = "Please fix these:\n1. paging skips a row\n2. amounts lose their sign\n3) regex rules are case-sensitive\n";
        assert_eq!(classify_request(enumerated).complexity, Complexity::Bounded);
        assert_eq!(
            classify_request(
                "Outage:
2026.09 sync failed
4.26.0 rollback
10.2 timed out
"
            )
            .complexity,
            Complexity::Trivial,
            "version- and date-shaped lines are not list items"
        );
        let spec = format!(
            "Add recurring transactions.\n{}",
            (1..=8)
                .map(|n| format!("- requirement {n}\n"))
                .collect::<String>()
        );
        let classification = classify_request(&spec);
        assert_eq!(classification.complexity, Complexity::Substantial);
        assert!(
            classification
                .reasons
                .iter()
                .any(|reason| reason.contains("request size"))
        );
        assert_eq!(
            classify_request(&"word ".repeat(2000)).complexity,
            Complexity::Substantial
        );
    }

    /// A `Roster` whose registry is the real built-in one (loaded against an
    /// otherwise-empty repository, so only the built-ins are present) --
    /// the right fixture for any test that needs `apply_explicit_workflow_
    /// request_floor`'s registered-id lookup to actually resolve something.
    fn roster_with_builtin_registry() -> (tempfile::TempDir, Roster) {
        let repo = tempfile::tempdir().expect("tempdir");
        let cfg = CtxConfig::default();
        let roster = Roster::gather(&cfg, repo.path());
        assert!(
            roster.registry.is_some(),
            "built-in packs must always load, even against an empty repo"
        );
        (repo, roster)
    }

    /// A REGISTERED pack id named adjacent to "workflow" wins outright,
    /// with no selection scoring at all -- and the deliberately text-only
    /// baseline (issue #537) is raised off `Trivial` so the workflow this
    /// sets survives `apply_direct_execution_workflow_rule`.
    #[test]
    fn baseline_sets_a_named_registered_workflow_id_outright() {
        let (repo, roster) = roster_with_builtin_registry();
        let cfg = CtxConfig::default();
        let request = "Start a bugfix workflow for the scheduler crash";
        let classification = classify_request(request);
        assert_eq!(classification.complexity, Complexity::Trivial);
        let decision = baseline(&cfg, repo.path(), request, &classification, &roster);
        assert_eq!(decision.workflow.as_deref(), Some("bugfix"));
        assert!(decision.complexity >= Complexity::Bounded);
        assert!(
            decision
                .reasons
                .iter()
                .any(|reason| reason.contains("explicitly asks for a workflow")),
            "{:?}",
            decision.reasons
        );
    }

    /// A hyphenated registered id (`sre-postmortem`) is matched as a single
    /// word -- `clause_words` keeps hyphens (and dots/underscores) that are
    /// interior to a word, so it never gets split.
    #[test]
    fn baseline_matches_a_hyphenated_registered_workflow_id() {
        let (repo, roster) = roster_with_builtin_registry();
        let cfg = CtxConfig::default();
        let request = "start the sre-postmortem workflow for last night's outage";
        let classification = classify_request(request);
        let decision = baseline(&cfg, repo.path(), request, &classification, &roster);
        assert_eq!(decision.workflow.as_deref(), Some("sre-postmortem"));
    }

    /// No id is named adjacent to "workflow" ("use a workflow"), so the
    /// floor falls through to `selection::select_definition` against the
    /// SAME request text -- here landing on `security-remediation` via its
    /// own "security vulnerability" trigger, exactly as `zirv workflow
    /// start` with no explicit id would.
    #[test]
    fn baseline_falls_through_to_selection_when_no_id_is_named() {
        let (repo, roster) = roster_with_builtin_registry();
        let cfg = CtxConfig::default();
        let request = "Use a workflow to fix the security vulnerability in auth";
        let classification = classify_request(request);
        assert_eq!(classification.intent, Intent::Bugfix);
        let decision = baseline(&cfg, repo.path(), request, &classification, &roster);
        assert_eq!(decision.workflow.as_deref(), Some("security-remediation"));
    }

    /// A word in the id slot that ISN'T actually a registered pack id still
    /// falls through to selection -- it does not refuse, and it does not
    /// literally use the unregistered name.
    #[test]
    fn baseline_falls_through_to_selection_for_an_unregistered_name() {
        let (repo, roster) = roster_with_builtin_registry();
        let cfg = CtxConfig::default();
        let request = "start the launch workflow to add a CSV export";
        let classification = classify_request(request);
        let decision = baseline(&cfg, repo.path(), request, &classification, &roster);
        assert_eq!(decision.workflow.as_deref(), Some("feature"));
    }

    /// Negation-aware: "do not start a workflow" must never fire the floor,
    /// so a `Trivial` request stays workflow-less exactly as it does today.
    #[test]
    fn baseline_does_not_fire_on_a_negated_workflow_request() {
        let (repo, roster) = roster_with_builtin_registry();
        let cfg = CtxConfig::default();
        let request = "do not start a workflow, just rename the flag";
        let classification = classify_request(request);
        assert_eq!(classification.complexity, Complexity::Trivial);
        let decision = baseline(&cfg, repo.path(), request, &classification, &roster);
        assert_eq!(decision.workflow, None);
        assert_eq!(decision.complexity, Complexity::Trivial);
    }

    /// A bare mention of the workflow SUBSYSTEM, with no governing verb or
    /// preposition directly next to "workflow", must never fire the floor
    /// either -- otherwise ordinary engineering requests about zirv's own
    /// workflow code would spuriously gate themselves.
    #[test]
    fn baseline_does_not_fire_on_a_mere_mention_of_the_subsystem() {
        let (repo, roster) = roster_with_builtin_registry();
        let cfg = CtxConfig::default();
        for request in [
            "Explain how the workflow engine persists state",
            "Fix the bug in the workflow status command",
            "Refactor the workflow registry loader",
        ] {
            let classification = classify_request(request);
            let decision = baseline(&cfg, repo.path(), request, &classification, &roster);
            assert_eq!(decision.workflow, None, "{request:?}");
            assert_eq!(decision.complexity, Complexity::Trivial, "{request:?}");
        }
    }

    /// The complexity floor never LOWERS an already-higher complexity --
    /// mirrors `apply_orchestration_request_complexity_floor`'s own
    /// never-lower guarantee.
    #[test]
    fn explicit_workflow_request_floor_never_lowers_an_already_substantial_complexity() {
        let (repo, roster) = roster_with_builtin_registry();
        let classification = classification_with(RiskBand::Low, Complexity::Substantial);
        let mut decision = sample_decision();
        decision.complexity = Complexity::Substantial;
        apply_explicit_workflow_request_floor(
            &mut decision,
            "start a workflow for the migration",
            &classification,
            &roster,
        );
        assert_eq!(decision.complexity, Complexity::Substantial);
        let _ = repo; // keeps the registry-backed roster alive for the call above
    }

    /// Merge-survival rule: an operator-named, REGISTERED workflow id
    /// (baseline already resolved it) survives even a decisive model
    /// `workflow` answer naming something else -- they said it in words. A
    /// selection-derived baseline id is still fair game for the model to
    /// replace, exactly as before this change.
    #[test]
    fn merge_keeps_an_explicitly_named_workflow_over_a_decisive_model_answer() {
        let (repo, roster) = roster_with_builtin_registry();
        let cfg = CtxConfig::default();
        let request = "Start a bugfix workflow for the scheduler crash";
        let classification = classify_request(request);
        let decision = baseline(&cfg, repo.path(), request, &classification, &roster);
        assert_eq!(decision.workflow.as_deref(), Some("bugfix"));

        let ans = answers(&[("workflow", AnswerValue::Choice("feature".to_string()), 0.9)]);
        let merged = merge(&cfg, &decision, request, &ans, 0.5, &roster);
        assert_eq!(
            merged.workflow.as_deref(),
            Some("bugfix"),
            "an explicitly named registered id must survive a decisive model answer"
        );
    }

    /// The counterpart: a selection-derived (not explicitly named) baseline
    /// workflow is still replaced by a decisive model answer, unchanged
    /// from before this rule existed.
    #[test]
    fn merge_still_replaces_a_selection_derived_workflow_with_a_decisive_model_answer() {
        let cfg = CtxConfig::default();
        let mut baseline = sample_decision();
        baseline.workflow = Some("feature".to_string());
        let ans = answers(&[("workflow", AnswerValue::Choice("bugfix".to_string()), 0.9)]);
        let merged = merge(
            &cfg,
            &baseline,
            "implement the feature",
            &ans,
            0.5,
            &empty_roster(),
        );
        assert_eq!(merged.workflow.as_deref(), Some("bugfix"));
    }

    /// Review finding: an explanatory or interrogative framing must
    /// never fire the floor, even when the sentence names both "workflow"
    /// and a registered id -- it describes or asks, it doesn't request.
    /// can/could/would/will/please are deliberately NOT explanatory: a
    /// polite request must still fire.
    #[test]
    fn an_explanatory_or_interrogative_lead_word_suppresses_the_floor() {
        for request in [
            "Explain what `zirv workflow start bugfix` does",
            "How do I start a workflow for a refactor?",
        ] {
            assert!(
                explicit_workflow_requests(&request.to_ascii_lowercase()).is_empty(),
                "{request:?} must not fire"
            );
        }
        assert!(
            !explicit_workflow_requests(
                &"Can you start a bugfix workflow for the login crash?".to_ascii_lowercase()
            )
            .is_empty(),
            "a polite request must still fire"
        );
    }

    /// Review finding: a negation contraction suppresses the floor
    /// regardless of spelling -- a straight apostrophe, the curly one
    /// (U+2019), and typed with no apostrophe at all.
    #[test]
    fn a_negation_contraction_suppresses_the_floor_in_every_spelling() {
        for request in [
            "You shouldn't start a workflow for this typo",
            "The build script doesn't start a workflow, it just compiles",
            "You shouldn\u{2019}t start a workflow for this typo",
            "The build script doesnt start a workflow, it just compiles",
        ] {
            assert!(
                explicit_workflow_requests(&request.to_ascii_lowercase()).is_empty(),
                "{request:?} must not fire"
            );
        }
    }

    /// Review finding: the id slot is read from the RAW whitespace-
    /// delimited word next to "workflow", not a `prose_words` token --
    /// `prose_words` splits on `.`/`_`, which would otherwise break a
    /// registered id like `sre.postmortem`/`team_review` into two tokens
    /// and never find it.
    #[test]
    fn explicit_registered_workflow_id_reads_the_raw_word_not_a_split_token() {
        let skills_repo = tempfile::tempdir().expect("tempdir");
        let skills = crate::commands::workflow::skill::SkillRegistry::load(
            skills_repo.path(),
            None,
            false,
            false,
        )
        .unwrap();
        let home = tempfile::tempdir().expect("tempdir");
        let home_dir = home.path().join(".zirv/workflows");
        std::fs::create_dir_all(&home_dir).unwrap();
        for id in ["team_review", "sre.postmortem"] {
            std::fs::write(
                home_dir.join(format!("{id}.toml")),
                format!(
                    r#"
schema_version = 1
id = "{id}"
version = 1
title = "{id}"
description = "fixture"
domains = ["testing"]
triggers = ["do the {id} thing"]
effects = "none"

[[steps]]
id = "only"
title = "Only"
phase = "implement"
skills = ["implement"]
condition = "always"

[failure]
escalate_to = "human"

[completion]
present_as = "summary"
"#
                ),
            )
            .unwrap();
        }
        let repo = tempfile::tempdir().expect("tempdir");
        let registry = crate::commands::workflow::registry::WorkflowRegistry::load(
            repo.path(),
            Some(home.path()),
            true,
            false,
            &skills,
        )
        .unwrap();
        let roster = Roster {
            harnesses: Vec::new(),
            registry: Some(registry),
        };

        assert_eq!(
            explicit_registered_workflow_id(
                "start the team_review workflow for the release",
                &roster
            ),
            Some("team_review".to_string())
        );
        assert_eq!(
            explicit_registered_workflow_id(
                "start the sre.postmortem workflow for last night's outage",
                &roster
            ),
            Some("sre.postmortem".to_string())
        );
    }

    /// Review finding: the matcher scans EVERY clause rather than
    /// stopping at the first match, and a registered named id anywhere
    /// wins over an earlier unnamed match.
    #[test]
    fn all_clauses_are_scanned_and_a_registered_named_id_anywhere_wins() {
        let matches = explicit_workflow_requests(
            &"Start a workflow; use the review workflow for the fix".to_ascii_lowercase(),
        );
        assert_eq!(matches, vec![None, Some("review".to_string())]);

        let (repo, roster) = roster_with_builtin_registry();
        let cfg = CtxConfig::default();
        let request = "Start a workflow; use the review workflow for the fix";
        let classification = classify_request(request);
        let decision = baseline(&cfg, repo.path(), request, &classification, &roster);
        assert_eq!(decision.workflow.as_deref(), Some("review"));
    }

    /// Review finding: negation lookback must never cross a clause
    /// boundary -- the "not" in "do not refactor" belongs to its own
    /// clause and must not suppress the assertion in the clause after it.
    #[test]
    fn negation_in_an_earlier_clause_never_suppresses_a_later_one() {
        let (repo, roster) = roster_with_builtin_registry();
        let cfg = CtxConfig::default();
        let request = "fix the bug, do not refactor; start a bugfix workflow";
        let classification = classify_request(request);
        let decision = baseline(&cfg, repo.path(), request, &classification, &roster);
        assert_eq!(decision.workflow.as_deref(), Some("bugfix"));
    }

    /// Review finding: glued trailing punctuation must never hide the word
    /// "workflow" -- an ellipsis run and a double hyphen both leave a clean
    /// clause boundary, and a registered id with interior punctuation
    /// (`sre-postmortem`) still resolves once its clause is split out.
    #[test]
    fn glued_trailing_punctuation_never_hides_the_word_workflow() {
        let (repo, roster) = roster_with_builtin_registry();
        let cfg = CtxConfig::default();

        let ellipsis_request = "Maybe we should start a workflow... for tidying my notes";
        let ellipsis_classification = classify_request(ellipsis_request);
        let ellipsis_decision = baseline(
            &cfg,
            repo.path(),
            ellipsis_request,
            &ellipsis_classification,
            &roster,
        );
        assert_eq!(
            ellipsis_decision.workflow.as_deref(),
            Some("adaptive-work"),
            "an ellipsis must not swallow the word workflow"
        );

        let dash_request = "start a workflow--let me know";
        let dash_classification = classify_request(dash_request);
        let dash_decision = baseline(
            &cfg,
            repo.path(),
            dash_request,
            &dash_classification,
            &roster,
        );
        assert!(
            dash_decision.workflow.is_some(),
            "a double hyphen with no surrounding whitespace must still split"
        );

        let hyphenated_id_request = "start the sre-postmortem workflow for last night's outage";
        let hyphenated_id_classification = classify_request(hyphenated_id_request);
        let hyphenated_id_decision = baseline(
            &cfg,
            repo.path(),
            hyphenated_id_request,
            &hyphenated_id_classification,
            &roster,
        );
        assert_eq!(
            hyphenated_id_decision.workflow.as_deref(),
            Some("sre-postmortem"),
            "a single interior hyphen in a registered id must survive"
        );
    }

    /// Review finding: a leading interrogative word alone must not veto the
    /// whole request -- only an ACTUAL question (ending in `?`) is
    /// suppressed; a statement that merely starts with "what"/"is" still
    /// fires.
    #[test]
    fn an_interrogative_lead_word_only_suppresses_an_actual_question() {
        let (repo, roster) = roster_with_builtin_registry();
        let cfg = CtxConfig::default();

        let question = "How do I start a workflow for a refactor?";
        assert!(
            explicit_workflow_requests(&question.to_ascii_lowercase()).is_empty(),
            "a real question must stay suppressed"
        );

        for statement in [
            "What I need: start a bugfix workflow for the login crash",
            "Is broken -- start a bugfix workflow",
        ] {
            let classification = classify_request(statement);
            let decision = baseline(&cfg, repo.path(), statement, &classification, &roster);
            assert_eq!(
                decision.workflow.as_deref(),
                Some("bugfix"),
                "{statement:?} is a statement, not a question, and must fire"
            );
        }
    }

    /// Review finding: an abbreviation's period is never a clause boundary
    /// (so a negation before it stays in scope), but an ordinary sentence-
    /// ending period -- even right after a version-shaped token -- still is.
    #[test]
    fn an_abbreviation_period_is_not_a_boundary_but_a_version_period_is() {
        let (repo, roster) = roster_with_builtin_registry();
        let cfg = CtxConfig::default();

        let abbreviation_request = "we shouldn't just e.g. start a workflow for typos";
        let abbreviation_classification = classify_request(abbreviation_request);
        let abbreviation_decision = baseline(
            &cfg,
            repo.path(),
            abbreviation_request,
            &abbreviation_classification,
            &roster,
        );
        assert_eq!(
            abbreviation_decision.workflow, None,
            "the negation before \"e.g.\" must still reach the request after it"
        );

        let version_request = "Upgraded to v1.2. Start a bugfix workflow for the crash";
        let version_classification = classify_request(version_request);
        let version_decision = baseline(
            &cfg,
            repo.path(),
            version_request,
            &version_classification,
            &roster,
        );
        assert_eq!(
            version_decision.workflow.as_deref(),
            Some("bugfix"),
            "a version-shaped token's period is a real sentence end"
        );
    }

    fn floored_complexity(request: &str) -> Complexity {
        let mut decision = sample_decision();
        decision.complexity = Complexity::Trivial;
        apply_orchestration_request_complexity_floor(&mut decision, request);
        decision.complexity
    }

    /// Review finding: the floor matched its signals as bare substrings, so
    /// a request that explicitly RULES OUT a team read as one asking for
    /// it -- the one direction a floor over the operator's own words must
    /// never get wrong, since it cannot be lowered again afterwards.
    #[test]
    fn wording_that_rules_out_a_team_does_not_floor_to_substantial() {
        for request in [
            "Do not parallelize this, it is a one-line typo fix.",
            "No need to spawn workers for this one.",
            "Fix the retry loop without multiple agents.",
        ] {
            assert_eq!(
                floored_complexity(request),
                Complexity::Trivial,
                "negated wording must not floor: {request}"
            );
        }
        assert_eq!(
            floored_complexity("Parallelize the migration as much as possible."),
            Complexity::Substantial,
            "the same signal, asserted, still floors"
        );
    }

    /// Review finding: tokenizing the whole request on punctuation turned the
    /// `codex` in a path or a URL into a harness the request was supposedly
    /// delegating to, so a one-method fix in one file asked for a team.
    #[test]
    fn a_harness_named_only_inside_a_path_or_url_is_not_a_delegation() {
        for request in [
            "Fix the delegate method in src/codex/client.rs.",
            "Split the work described in https://github.com/openai/codex/issues/12.",
        ] {
            assert_eq!(
                floored_complexity(request),
                Complexity::Trivial,
                "a path or URL is not prose naming a harness: {request}"
            );
        }
    }

    /// Review finding: only "delegate"-shaped wording counted, so the
    /// ordinary ways of handing work to another harness by name landed a
    /// single cheap seat.
    #[test]
    fn handing_work_to_another_harness_by_name_floors_to_substantial() {
        for request in [
            "Have codex handle the frontend part while you take the API.",
            "Split this across claude and codex.",
            "Delegate the schema migration to codex.",
            "Use codex for some of the work.",
        ] {
            assert_eq!(
                floored_complexity(request),
                Complexity::Substantial,
                "delegation to a named harness must floor: {request}"
            );
        }
    }

    /// The orchestrator's own harness is not another seat: naming it is how
    /// a request refers to the session it is already talking to.
    #[test]
    fn naming_only_the_orchestrators_own_harness_is_not_a_delegation() {
        assert_eq!(
            floored_complexity("Have claude handle the rename in one pass."),
            Complexity::Trivial
        );
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
        assert_eq!(plain_decision.seat_role, SeatRole::Single);
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
                },
                RosterHarness {
                    name: "codex".to_string(),
                    ready: false,
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
            },
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
