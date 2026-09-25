//! The harness proxy (issue #537 seam): one inspectable decision --
//! intent, complexity, risk, execution mode, workflow, orchestrator seat,
//! worker tier -- computed before any provider turn, printed by `zirv ctx
//! proxy` and (T2) applied to a `zirv chat` launch.
//!
//! [`decide`] always succeeds: it computes a pure, deterministic
//! [`decision::baseline`] first, then tries at most one model decider
//! (`typesafe` -> `helper`, per `[proxy] decider`), merges a confident
//! answer over the baseline (never lowering complexity/risk/execution),
//! validates the result against the live harness/workflow roster, and
//! persists it. Disabled by default (`[proxy] enabled = false`); every
//! launch path is unaffected until an operator opts in.

pub mod decision;
pub mod launch;
pub mod llm;
pub mod native;
pub mod typesafe;

use std::io::{BufRead, IsTerminal, Write};
use std::path::Path;
use std::time::{Duration, Instant};

use clap::Args;

use self::decision::{Answers, Decider, ProxyDecision, Question, SeatRole};
use super::config::{self, CtxConfig, ProxyDecider};
use super::{CtxResult, adapters, helper, jev, log, state};

const PROXY_DECISIONS_FILE: &str = "proxy-decisions.jsonl";
/// The catalogue id `log::Delegation`/`price::price` key the proxy's own
/// spend row on -- see `catalogue.rs`'s `typesafe` vendor.
const TYPESAFE_MODEL_ID: &str = "jev-latest";
const CLARIFICATION_CATEGORY_ID: &str = "clarification_category";

/// Issue #537 (A2): a `decision.needs_clarification` at or above this floor
/// is worth interrupting an interactive launch for one round of follow-up
/// (`chat.rs::proxy_intake`); the same floor also gates the `clarify:` line
/// [`prompt_layer`] adds for a session launched non-interactively (e.g. a
/// resumed clarification a dashboard pane never got to ask). Chosen as the
/// midpoint of the noul's own `[0, 1]` confidence range -- above it, "too
/// ambiguous" is the more likely reading than "clear enough".
pub(crate) const CLARIFY_THRESHOLD: f32 = 0.5;

#[derive(Debug, Args)]
pub struct ProxyArgs {
    /// Print the full `ProxyDecision` as JSON instead of the human summary.
    #[arg(long)]
    pub json: bool,
    /// The request to decide on. Read from stdin (multi-line, until a blank
    /// line or EOF) when omitted and stdin is not a terminal.
    pub request: Option<String>,
}

fn tier_str(tier: super::catalogue::Tier) -> &'static str {
    match tier {
        super::catalogue::Tier::Cheap => "cheap",
        super::catalogue::Tier::Standard => "standard",
        super::catalogue::Tier::Deep => "deep",
    }
}

fn decider_label(decider: Decider) -> &'static str {
    match decider {
        Decider::Typesafe => "typesafe",
        Decider::Helper => "helper",
        Decider::Deterministic => "deterministic",
    }
}

fn lower_debug<T: std::fmt::Debug>(value: T) -> String {
    format!("{value:?}").to_lowercase()
}

/// Whether the harness proxy should take over a `zirv chat` launch (open the
/// intake view before starting the orchestrator harness). Distinct from
/// `decide()`'s own fallback chain: this predicate only decides whether the
/// proxy is even worth trying for THIS launch, so a chat session backed by
/// a `deterministic` decider does not silently insert an intake step that
/// can only ever answer from the baseline. `Ok(())` when a usable model
/// exists for the configured decider; `Err(reason)` (one line, in the same
/// tone as other `zirv \u{25b8}` advisories) otherwise.
pub fn activation(cfg: &CtxConfig) -> Result<(), String> {
    if !cfg.proxy.enabled {
        return Err("proxy: disabled; starting the orchestrator harness".to_string());
    }
    match cfg.proxy.decider {
        ProxyDecider::Typesafe => {
            if cfg.proxy.typesafe.model.is_empty() {
                return Err(
                    "proxy: enabled but proxy.typesafe.model is empty; starting the orchestrator \
                     harness"
                        .to_string(),
                );
            }
            if !jev::available(&cfg.proxy.typesafe) {
                return Err(format!(
                    "proxy: enabled but {} is unset; starting the orchestrator harness",
                    cfg.proxy.typesafe.credential_env
                ));
            }
            Ok(())
        }
        ProxyDecider::Helper => adapters::resolve_default(cfg).map(|_| ()).map_err(|error| {
            format!(
                "proxy: enabled but no helper adapter is ready ({error}); starting the \
                 orchestrator harness"
            )
        }),
        ProxyDecider::Deterministic => {
            Err("proxy: decider is deterministic; starting the orchestrator harness".to_string())
        }
    }
}

fn try_helper(cfg: &CtxConfig, questions: &[Question]) -> Result<Answers, String> {
    let (adapter, _origin) = adapters::resolve_default(cfg)
        .map_err(|error| format!("helper: no adapter ready ({error})"))?;
    let model = super::handoff::resolve_distiller_model(None, adapter.as_ref());
    // `config.rs::CtxConfig::load` already floors `timeout_secs` to at
    // least 1 at load time (see `ProxyConfig`'s own doc comment); no ad-hoc
    // `.max(1)` needed here.
    let timeout = Duration::from_secs(cfg.proxy.typesafe.timeout_secs);
    llm::decide(
        helper::ROLE_PROXY,
        adapter.as_ref(),
        &model,
        questions,
        timeout,
    )
    .map_err(|error| format!("helper: {error}"))
}

fn protected_model_intake(
    cfg: &CtxConfig,
    state_dir: &Path,
    repo: &Path,
    request: &str,
    roster: &decision::Roster,
) -> CtxResult<(decision::IntakeState, Vec<Question>)> {
    let state = state::StateDir::from_path(state_dir.to_path_buf());
    let request =
        super::obfuscate_store::protect_text(&state, repo, cfg, request, "proxy_jev_request")?.0;
    let intake = decision::build_intake(cfg, repo, state_dir, &request, roster);
    let questions = decision::questions(&intake);
    Ok((intake, questions))
}

/// A word that looks like it names a file or path -- shared with the
/// workflow module's own metadata-only classify refinement (issue #782,
/// `workflow::profile::refine_via_jev`) so the two never carry diverging
/// path-like-token predicates for the same signal.
pub(crate) fn is_path_like_token(word: &str) -> bool {
    word.contains('/') || word.contains('\\') || word.ends_with(".rs") || word.ends_with(".md")
}

/// Whether lowercased text names a stated outcome ("should", "expected",
/// "return", "display", "show", "produce"). Shared the same way as
/// [`is_path_like_token`].
pub(crate) fn text_has_outcome_terms(lower: &str) -> bool {
    ["should", "expected", "return", "display", "show", "produce"]
        .iter()
        .any(|term| lower.contains(term))
}

/// Whether lowercased text names a stated constraint ("without",
/// "compatible", "preserve", "only", "must", "don't"). Shared the same way
/// as [`is_path_like_token`].
pub(crate) fn text_has_constraint_terms(lower: &str) -> bool {
    ["without", "compatible", "preserve", "only", "must", "don't"]
        .iter()
        .any(|term| lower.contains(term))
}

/// Buckets a word count into `0..=4` (one bucket per 8 words, capped).
/// Shared the same way as [`is_path_like_token`].
pub(crate) fn word_count_bucket(word_count: usize) -> u64 {
    word_count.div_ceil(8).min(4) as u64
}

fn safe_intake_metadata(request: &str, baseline: &ProxyDecision) -> serde_json::Value {
    let lower = request.to_ascii_lowercase();
    let has_target = request.split_whitespace().any(is_path_like_token);
    let has_outcome = text_has_outcome_terms(&lower);
    let has_constraint = text_has_constraint_terms(&lower);
    serde_json::json!({
        "_zirv_metadata_only": true,
        // [site=2, intent, complexity, risk, word-count bucket, named
        // target, stated outcome, stated constraint]. No request text,
        // repository name, workflow description, path, or secret is sent.
        "facts": [[
            2,
            baseline.intent as u8,
            baseline.complexity as u8,
            baseline.risk as u8,
            word_count_bucket(request.split_whitespace().count()),
            has_target as u8,
            has_outcome as u8,
            has_constraint as u8,
        ]],
    })
}

fn safe_intake_questions() -> Vec<Question> {
    vec![
        Question::metadata_noul(
            "needs_clarification",
            "From facts [site=2, intent (0 feature, 1 bugfix, 2 refactor, 3 spike, 4 review, 5 other), complexity (0 trivial to 3 architectural), risk (0 low to 3 critical), word-count bucket (0 to 4), target/outcome/constraint flags (0 absent, 1 present)], is material information missing before implementation? Answer false if the metadata is insufficient to judge.",
            "material information is missing",
            "clear enough to start or insufficient evidence",
        ),
        Question::metadata_choice(
            CLARIFICATION_CATEGORY_ID,
            "Given the same coarse facts, which single category of missing information should be clarified? Choose other if the metadata is insufficient.",
            &[
                (
                    "target",
                    "The exact target service, file, component, or scope is missing.",
                ),
                (
                    "behavior",
                    "The expected behavior or acceptance result is missing.",
                ),
                (
                    "constraint",
                    "A required constraint or compatibility boundary is missing.",
                ),
                (
                    "other",
                    "No specific category is clear; use the generic clarification prompt.",
                ),
            ],
        ),
    ]
}

fn clarification_category(answers: &Answers, cfg: &CtxConfig) -> Option<String> {
    let answer = answers.get(CLARIFICATION_CATEGORY_ID)?;
    if !answer.decisive(cfg.proxy.min_confidence.max(0.7), cfg.proxy.min_margin) {
        return None;
    }
    match answer.as_choice()? {
        "target" | "behavior" | "constraint" => answer.as_choice().map(str::to_string),
        _ => None,
    }
}

/// Computes one [`ProxyDecision`] for `request`, in `repo`, under `cfg`.
/// Never fails: every I/O-touching step inside is best-effort, and the
/// deterministic baseline is always a valid answer on its own. Persists the
/// decision (and a spend row, when a model call reported usage) to
/// `<state_dir>/proxy-decisions.jsonl` before returning.
pub fn decide(cfg: &CtxConfig, state_dir: &Path, repo: &Path, request: &str) -> ProxyDecision {
    let started = Instant::now();
    let classification = decision::classify_request(request);
    let roster = decision::Roster::gather(cfg, repo);
    let baseline = decision::baseline(cfg, repo, request, &classification, &roster);
    let safe_intake = cfg.jev.intake_savings
        && matches!(cfg.proxy.decider, ProxyDecider::Typesafe)
        && jev::available(&cfg.proxy.typesafe);
    let safe_input = safe_intake.then(|| {
        (
            safe_intake_metadata(request, &baseline),
            safe_intake_questions(),
        )
    });
    let model_input = (!safe_intake && !matches!(cfg.proxy.decider, ProxyDecider::Deterministic))
        .then(|| protected_model_intake(cfg, state_dir, repo, request, &roster));

    let mut fallbacks = Vec::new();
    let mut winner = Decider::Deterministic;
    let mut usage = None;
    let mut result = baseline.clone();
    let mut ran_model = false;

    if let Some(Err(error)) = &model_input {
        fallbacks.push(format!("sensitive-data masking: {error}"));
    }

    let typesafe_result = if let Some((input, questions)) = &safe_input {
        Some(
            jev::ask(
                &cfg.proxy.typesafe,
                state_dir,
                cfg.jev.cache_ttl_secs,
                input,
                questions,
            )
            .map(|(answers, usage, _)| (answers, usage)),
        )
    } else if matches!(cfg.proxy.decider, ProxyDecider::Typesafe)
        && let Some(Ok((intake, questions))) = &model_input
    {
        Some(typesafe::decide(
            &cfg.proxy.typesafe,
            state_dir,
            cfg.jev.cache_ttl_secs,
            intake,
            questions,
        ))
    } else {
        None
    };
    if let Some(typesafe_result) = typesafe_result {
        match typesafe_result {
            Ok((answers, model_usage)) => {
                result = decision::merge(
                    cfg,
                    &baseline,
                    request,
                    &answers,
                    cfg.proxy.min_confidence,
                    &roster,
                );
                if safe_intake
                    && result.needs_clarification >= CLARIFY_THRESHOLD
                    && result.needs_clarification_decisive
                {
                    result.clarification_category = clarification_category(&answers, cfg);
                }
                winner = Decider::Typesafe;
                usage = Some(model_usage);
                ran_model = true;
            }
            Err(error) => fallbacks.push(format!("typesafe: {error}")),
        }
    }

    if !ran_model
        && !safe_intake
        && matches!(
            cfg.proxy.decider,
            ProxyDecider::Typesafe | ProxyDecider::Helper
        )
        && let Some(Ok((_, questions))) = &model_input
    {
        match try_helper(cfg, questions) {
            Ok(answers) => {
                result = decision::merge(
                    cfg,
                    &baseline,
                    request,
                    &answers,
                    cfg.proxy.min_confidence,
                    &roster,
                );
                winner = Decider::Helper;
            }
            Err(reason) => fallbacks.push(reason),
        }
    }

    decision::validate(&mut result, &baseline, &roster, cfg);
    result.decider = winner;
    result.fallbacks = fallbacks;
    result.elapsed_ms = started.elapsed().as_millis().min(u128::from(u64::MAX)) as u64;
    result.usage = usage;
    result.created_at = state::now_secs();

    let _ = persist(state_dir, &result);
    result
}

/// One line, shaped by `decision.seat_role` -- issue #537 field evidence
/// problem (c): the operator experienced both a single-seat and a full team
/// launch as "the full orchestrator setup", so this now says which one it
/// actually is, and names the resolved seat tier alongside the model:
///
/// - `Single`: `proxy: <execution> \u{b7} single seat \u{b7} <harness>/<model> (<seat-tier>) \u{b7}
///   <workflow-or-none> \u{b7} <decider> [\u{b7} domains: <tag>, ...] [<mean-confidence>]`
/// - `Orchestrator`: `proxy: <execution> \u{b7} orchestrator <harness>/<model> (<seat-tier>) \u{b7}
///   workers <worker-tier> \u{b7} <workflow-or-none> \u{b7} <decider> [\u{b7} domains: <tag>, ...]
///   [<mean-confidence>]`
///
/// `<workflow-or-none>` is `no workflow` when `decision.workflow` is `None`
/// (the common case now that a `Direct` execution always clears it, see
/// `decision::apply_direct_execution_workflow_rule`), else `workflow <id>
/// (<complexity>/<risk>)`. The `domains` segment (issue #537 A2) is omitted
/// entirely when `decision.domains` is empty, the common case.
pub fn announce_line(decision: &ProxyDecision) -> String {
    let execution = lower_debug(decision.execution);
    let seat = format!(
        "{}/{}",
        decision.orchestrator.harness, decision.orchestrator.model
    );
    let seat_tier = decision.seat_tier.label();
    let decider = decider_label(decision.decider);
    let workflow_segment = match &decision.workflow {
        Some(id) => format!(
            "workflow {id} ({}/{})",
            lower_debug(decision.complexity),
            lower_debug(decision.risk)
        ),
        None => "no workflow".to_string(),
    };

    let mut line = match decision.seat_role {
        SeatRole::Single => format!(
            "proxy: {execution} \u{b7} single seat \u{b7} {seat} ({seat_tier}) \u{b7} \
             {workflow_segment} \u{b7} {decider}"
        ),
        SeatRole::Orchestrator => format!(
            "proxy: {execution} \u{b7} orchestrator {seat} ({seat_tier}) \u{b7} workers {} \u{b7} \
             {workflow_segment} \u{b7} {decider}",
            tier_str(decision.worker_tier)
        ),
    };
    if !decision.domains.is_empty() {
        line.push_str(&format!(" \u{b7} domains: {}", decision.domains.join(", ")));
    }
    if let Some(confidence) = mean_confidence(decision) {
        line.push_str(&format!(" {confidence:.2}"));
    }
    line
}

/// Prints before `decide()` runs, naming the decider that will run first for
/// `cfg.proxy.decider` -- so an operator watching `zirv ctx proxy` (or a
/// `zirv chat` launch the proxy took over) sees the request go out rather
/// than a silent pause. `Deterministic` asks nothing at all: the baseline is
/// the whole answer, so there is no model to announce.
pub fn asking_line(cfg: &CtxConfig) -> String {
    match cfg.proxy.decider {
        ProxyDecider::Typesafe => {
            format!(
                "proxy: asking typesafe ({})\u{2026}",
                cfg.proxy.typesafe.model
            )
        }
        ProxyDecider::Helper => "proxy: asking helper model\u{2026}".to_string(),
        ProxyDecider::Deterministic => {
            "proxy: using the deterministic baseline\u{2026}".to_string()
        }
    }
}

fn mean_confidence(decision: &ProxyDecision) -> Option<f32> {
    if decision.confidence.is_empty() {
        return None;
    }
    let total: f32 = decision.confidence.values().sum();
    Some(total / decision.confidence.len() as f32)
}

/// The bounded `[zirv proxy]` context layer (T2 folds this into the compiled
/// prompt): at most 7 lines -- a header, execution/complexity/risk, the
/// seat(s), the workflow, (issue #537 A2, both conditional) the domain tags
/// and a clarify instruction, and (`Single` only) one line telling the
/// session plainly that it is the one doing the work, not an orchestrator.
///
/// `started_workflow_id` (wrapper-overhead benchmark, 2026-09-22 change 2):
/// the instance id `proxy::launch::start_workflow_for` actually started for
/// this launch, when it did -- `chat.rs` threads it through from the SAME
/// start call that names `decision.workflow`'s kind, so the workflow line
/// can point the seat at that concrete instance instead of only naming the
/// kind (the field evidence gap: a workflow started in 27/36 replayed runs,
/// never consulted, because nothing named the running id or what to do with
/// it). `None` when the intake never decided, the start was skipped
/// (already-active workflow) or failed, or a decision names no workflow --
/// every one of those keeps today's plain `workflow: <kind or none>` line.
// T2 is the first caller (folds this into `compile.rs`'s composed context);
// exercised here only by this module's own tests in the meantime.
#[allow(dead_code)]
pub fn prompt_layer(decision: &ProxyDecision, started_workflow_id: Option<&str>) -> String {
    let mut lines = vec!["[zirv proxy]".to_string()];
    lines.push(format!(
        "execution: {} (complexity {}, risk {})",
        lower_debug(decision.execution),
        lower_debug(decision.complexity),
        lower_debug(decision.risk),
    ));
    lines.push(match decision.seat_role {
        SeatRole::Single => format!(
            "seat: {}/{} ({})",
            decision.orchestrator.harness,
            decision.orchestrator.model,
            decision.seat_tier.label(),
        ),
        SeatRole::Orchestrator => format!(
            "seats: orchestrator {}/{} ({}) \u{b7} workers {}",
            decision.orchestrator.harness,
            decision.orchestrator.model,
            decision.seat_tier.label(),
            tier_str(decision.worker_tier),
        ),
    });
    lines.push(match (decision.workflow.as_deref(), started_workflow_id) {
        (Some(kind), Some(id)) => format!(
            "workflow: {kind} (started {id}) -- run `zirv workflow status` and follow its \
             current step"
        ),
        (Some(kind), None) => format!("workflow: {kind}"),
        (None, _) => "workflow: none".to_string(),
    });
    if !decision.domains.is_empty() {
        lines.push(format!("domains: {}", decision.domains.join(", ")));
    }
    if decision.needs_clarification >= CLARIFY_THRESHOLD && decision.needs_clarification_decisive {
        lines.push("clarify: ask the user one precise question before acting".to_string());
    }
    if decision.seat_role == SeatRole::Single {
        lines.push(
            "You are the single seat for this request: do the work here yourself; do not \
             delegate."
                .to_string(),
        );
    }
    lines.join("\n")
}

/// Appends `d` to `<state_dir>/proxy-decisions.jsonl`, and (when `d.usage`
/// is `Some`) a `log::Delegation` spend row -- agent `"typesafe"`, model
/// `"jev-latest"`, input/output tokens from `usage`, outcome `"ok"` -- so
/// `zirv ctx spend` prices the call through `catalogue`'s `typesafe` vendor.
/// `session`/`principal` come from `jev::session_and_principal` -- this
/// process's own `ZIRV_CTX_SESSION` (the same identity `mail::
/// session_identity`/`hook.rs` read) and `ZIRV_PRINCIPAL` (`agent::
/// PRINCIPAL_ENV`), falling back to `"proxy"`/`"root"` only when unset, the
/// same "root session, no inherited envelope" convention `agent::
/// root_envelope` establishes -- shared with `jev::record` so the two
/// spend-adjacent recorders never drift on what an absent value means.
/// Best-effort like every other append in this crate's flat logs: the
/// caller (`decide`) never propagates a write failure.
pub fn persist(state_dir: &Path, d: &ProxyDecision) -> CtxResult<()> {
    state::create_private_dir_all(state_dir)?;
    let mut file = state::open_private_append(&state_dir.join(PROXY_DECISIONS_FILE))?;
    writeln!(file, "{}", serde_json::to_string(d)?)?;

    if let Some(usage) = &d.usage {
        let wrapped = state::StateDir::from_path(state_dir.to_path_buf());
        let (session, principal) = jev::session_and_principal();
        let _ = log::append_delegation(
            &wrapped,
            &log::Delegation {
                ts: d.created_at,
                session: &session,
                parent_session: "",
                work_group_id: None,
                agent: "typesafe",
                model: Some(TYPESAFE_MODEL_ID),
                input_tokens: usage.input_tokens,
                cache_creation_input_tokens: 0,
                cache_read_input_tokens: 0,
                output_tokens: usage.output_tokens,
                wall_ms: d.elapsed_ms,
                exit_code: 0,
                outcome: "ok",
                mode: None,
                task_class: None,
                principal: &principal,
                envelope_sha256: None,
            },
        );
    }
    Ok(())
}

/// The most recent persisted decision for `repo`, or `None` when nothing is
/// stored yet or the log cannot be read. Best-effort: a corrupt line is
/// skipped, never fatal.
pub fn latest_for_repo(state_dir: &Path, repo: &Path) -> Option<ProxyDecision> {
    let text = std::fs::read_to_string(state_dir.join(PROXY_DECISIONS_FILE)).ok()?;
    let canonical_repo = repo.canonicalize().unwrap_or_else(|_| repo.to_path_buf());
    text.lines()
        .filter_map(|line| serde_json::from_str::<ProxyDecision>(line).ok())
        .rfind(|decision| {
            let candidate = decision
                .repo
                .canonicalize()
                .unwrap_or_else(|_| decision.repo.clone());
            candidate == canonical_repo
        })
}

/// Reads a request from `reader`: every line up to (not including) the
/// first blank line or EOF. `None` when nothing but whitespace was read.
pub fn read_request(reader: &mut (impl BufRead + ?Sized)) -> Option<String> {
    let mut lines = Vec::new();
    let mut line = String::new();
    loop {
        line.clear();
        let read = reader.read_line(&mut line).unwrap_or(0);
        if read == 0 {
            break;
        }
        let trimmed = line.trim_end_matches(['\n', '\r']);
        if trimmed.is_empty() {
            break;
        }
        lines.push(trimmed.to_string());
    }
    let joined = lines.join("\n");
    if joined.trim().is_empty() {
        None
    } else {
        Some(joined)
    }
}

fn human_fields(decision: &ProxyDecision, min_confidence: f32) -> Vec<(&'static str, String)> {
    let source = |field: &str| -> &'static str {
        if decision.decider == Decider::Deterministic {
            return "baseline";
        }
        match decision.confidence.get(field) {
            Some(confidence) if *confidence >= min_confidence => decider_label(decision.decider),
            _ => "baseline",
        }
    };
    let confidence_text = |field: &str| -> String {
        decision
            .confidence
            .get(field)
            .map(|value| format!("{value:.2}"))
            .unwrap_or_else(|| "n/a".to_string())
    };
    let row = |field: &'static str, value: String| -> (&'static str, String) {
        (
            field,
            format!("{value} ({}, {})", source(field), confidence_text(field)),
        )
    };
    vec![
        row("intent", lower_debug(decision.intent)),
        row("complexity", lower_debug(decision.complexity)),
        row("risk", lower_debug(decision.risk)),
        row("execution", lower_debug(decision.execution)),
        row("seat_role", lower_debug(decision.seat_role)),
        row(
            "workflow",
            decision
                .workflow
                .clone()
                .unwrap_or_else(|| "none".to_string()),
        ),
        row(
            "seat",
            format!(
                "{}/{}",
                decision.orchestrator.harness, decision.orchestrator.model
            ),
        ),
        row("seat_tier", decision.seat_tier.label().to_string()),
        row("worker_tier", tier_str(decision.worker_tier).to_string()),
        row(
            "needs_clarification",
            format!("{:.2}", decision.needs_clarification),
        ),
        // Issue #537 (A2): a plain field, not `row()` -- `domains` aggregates
        // up to six separate Noul confidences (one per tag question), which
        // does not fit `row()`'s one-field-one-confidence shape.
        (
            "domains",
            if decision.domains.is_empty() {
                "none".to_string()
            } else {
                decision.domains.join(", ")
            },
        ),
    ]
}

/// `zirv ctx proxy [--json] [REQUEST]`: decides and prints, never launches.
pub fn run<W: Write>(args: &ProxyArgs, w: &mut W) -> CtxResult<i32> {
    let env = config::env_from_process();
    let repo = std::env::current_dir()?;
    let cfg = CtxConfig::load(&repo, &env)?;
    let state = state::StateDir::resolve(&env)?;
    run_with(&cfg, state.root(), &repo, args, w)
}

pub fn run_with<W: Write>(
    cfg: &CtxConfig,
    state_dir: &Path,
    repo: &Path,
    args: &ProxyArgs,
    w: &mut W,
) -> CtxResult<i32> {
    let request = match &args.request {
        Some(request) => request.clone(),
        None => {
            if std::io::stdin().is_terminal() {
                return Err(
                    "zirv ctx proxy: no REQUEST given and stdin is a terminal; pass a request or \
                     pipe one in"
                        .into(),
                );
            }
            let stdin = std::io::stdin();
            let mut locked = stdin.lock();
            match read_request(&mut locked) {
                Some(request) => request,
                None => return Err("zirv ctx proxy: no request given on stdin".into()),
            }
        }
    };

    if !args.json {
        eprintln!("{}", asking_line(cfg));
    }
    let decision = decide(cfg, state_dir, repo, &request);

    if args.json {
        writeln!(w, "{}", serde_json::to_string_pretty(&decision)?)?;
        return Ok(0);
    }

    writeln!(w, "{}", announce_line(&decision))?;
    for (field, rendered) in human_fields(&decision, cfg.proxy.min_confidence) {
        writeln!(w, "{field}: {rendered}")?;
    }
    for reason in &decision.reasons {
        writeln!(w, "reason: {reason}")?;
    }
    for fallback in &decision.fallbacks {
        writeln!(w, "fallback: {fallback}")?;
    }
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::ctx::catalogue::Tier;
    use crate::commands::workflow::classify::{Complexity, Intent, RiskBand};
    use crate::commands::workflow::profile::{ExecutionMode, ValidationProfile};
    use std::collections::BTreeMap;
    use std::path::PathBuf;

    fn sample_decision() -> ProxyDecision {
        ProxyDecision {
            request_sha256: "x".repeat(64),
            repo: PathBuf::from("/tmp/repo"),
            intent: Intent::Feature,
            complexity: Complexity::Substantial,
            risk: RiskBand::Medium,
            execution: ExecutionMode::Orchestrated,
            seat_role: SeatRole::Orchestrator,
            validation: ValidationProfile::default(),
            workflow: Some("feature".to_string()),
            orchestrator: decision::Seat {
                harness: "claude".to_string(),
                model: "fable".to_string(),
            },
            seat_tier: decision::SeatTier::Frontier,
            worker_tier: Tier::Standard,
            needs_clarification: 0.0,
            needs_clarification_decisive: false,
            clarification_category: None,
            domains: Vec::new(),
            decider: Decider::Typesafe,
            confidence: BTreeMap::from([("seat_tier".to_string(), 0.81_f32)]),
            reasons: Vec::new(),
            fallbacks: Vec::new(),
            elapsed_ms: 12,
            usage: None,
            created_at: 0,
        }
    }

    #[test]
    fn intake_savings_batches_material_ambiguity_category_in_existing_call() {
        let repo = crate::commands::ctx::testenv::repo();
        let state_tmp = tempfile::tempdir().expect("state");
        let mut cfg = CtxConfig::default();
        cfg.jev.intake_savings = true;
        cfg.proxy.decider = ProxyDecider::Typesafe;
        cfg.proxy.typesafe.credential_env = "JEV_TEST_KEY_INTAKE_CATEGORY".to_string();
        cfg.jev.cache_ttl_secs = 0;
        let body = r#"{"model":"jev-latest","answers":{"needs_clarification":{"type":"noul","noul":0.99},"clarification_category":{"type":"choice","choice":"target","probabilities":{"target":0.96,"behavior":0.02,"constraint":0.01,"other":0.01},"confidence":0.96}},"usage":{"input_tokens":23,"output_tokens":3}}"#;
        let (base_url, server) = jev::tests::one_shot_server(200, body);
        cfg.proxy.typesafe.base_url = base_url;
        unsafe { std::env::set_var("JEV_TEST_KEY_INTAKE_CATEGORY", "test-key") };
        let decision = decide(&cfg, state_tmp.path(), repo.path(), "change the service");
        unsafe { std::env::remove_var("JEV_TEST_KEY_INTAKE_CATEGORY") };
        server.join().expect("one batched request");
        assert_eq!(decision.clarification_category.as_deref(), Some("target"));
        assert!(decision.needs_clarification_decisive);
    }

    #[test]
    fn intake_savings_http_error_keeps_the_deterministic_decision() {
        let repo = crate::commands::ctx::testenv::repo();
        let state_tmp = tempfile::tempdir().expect("state");
        let mut cfg = CtxConfig::default();
        cfg.jev.intake_savings = true;
        cfg.proxy.decider = ProxyDecider::Typesafe;
        cfg.proxy.typesafe.credential_env = "JEV_TEST_KEY_INTAKE_ERROR".to_string();
        cfg.jev.cache_ttl_secs = 0;
        let (base_url, server) = jev::tests::one_shot_server(503, "unavailable");
        cfg.proxy.typesafe.base_url = base_url;
        unsafe { std::env::set_var("JEV_TEST_KEY_INTAKE_ERROR", "test-key") };
        let decision = decide(&cfg, state_tmp.path(), repo.path(), "change the service");
        unsafe { std::env::remove_var("JEV_TEST_KEY_INTAKE_ERROR") };
        server.join().expect("one request");
        assert_eq!(decision.decider, Decider::Deterministic);
        assert!(decision.clarification_category.is_none());
        assert_eq!(decision.needs_clarification, 0.0);
        assert!(
            decision
                .fallbacks
                .iter()
                .any(|reason| reason.contains("503"))
        );
    }

    #[test]
    fn missing_or_uncertain_category_keeps_generic_clarification() {
        let cfg = CtxConfig::default();
        assert!(clarification_category(&Answers::new(), &cfg).is_none());
        let mut answers = Answers::new();
        answers.insert(
            CLARIFICATION_CATEGORY_ID.to_string(),
            decision::Answer {
                value: decision::AnswerValue::Choice("target".to_string()),
                confidence: 0.2,
                probabilities: BTreeMap::from([
                    ("target".to_string(), 0.51),
                    ("behavior".to_string(), 0.49),
                ]),
            },
        );
        assert!(clarification_category(&answers, &cfg).is_none());
        let serialized = serde_json::to_value(sample_decision()).expect("decision JSON");
        assert!(serialized.get("clarification_category").is_none());
    }

    #[test]
    fn intake_savings_projects_only_coarse_request_metadata() {
        let mut baseline = sample_decision();
        baseline.intent = crate::commands::workflow::classify::Intent::Feature;
        let request = "change PRIVATE_CUSTOMER_SERVICE in src/private.rs without downtime";
        let metadata = safe_intake_metadata(request, &baseline).to_string();
        assert!(!metadata.contains("PRIVATE_CUSTOMER_SERVICE"));
        assert!(!metadata.contains("src/private.rs"));
        assert!(!metadata.contains("without downtime"));
        assert!(metadata.contains("_zirv_metadata_only"));
        assert_eq!(safe_intake_questions().len(), 2);
    }

    #[test]
    fn announce_line_matches_the_documented_shape_for_an_orchestrator_seat() {
        let line = announce_line(&sample_decision());
        assert_eq!(
            line,
            "proxy: orchestrated \u{b7} orchestrator claude/fable (frontier) \u{b7} workers \
             standard \u{b7} workflow feature (substantial/medium) \u{b7} typesafe 0.81"
        );
    }

    #[test]
    fn announce_line_matches_the_documented_shape_for_a_single_seat() {
        let mut decision = sample_decision();
        decision.execution = ExecutionMode::Direct;
        decision.seat_role = SeatRole::Single;
        decision.seat_tier = decision::SeatTier::Cheap;
        decision.orchestrator = decision::Seat {
            harness: "claude".to_string(),
            model: "sonnet".to_string(),
        };
        decision.workflow = None;
        decision.confidence = BTreeMap::new();
        decision.decider = Decider::Typesafe;
        decision.confidence.insert("execution".to_string(), 0.75);
        let line = announce_line(&decision);
        assert_eq!(
            line,
            "proxy: direct \u{b7} single seat \u{b7} claude/sonnet (cheap) \u{b7} no workflow \u{b7} \
             typesafe 0.75"
        );
    }

    /// Issue #537 (A2): the `domains` segment sits between the decider and
    /// the trailing mean-confidence, and is omitted entirely on the plain
    /// `sample_decision` (already covered by the two tests above).
    #[test]
    fn announce_line_shows_domains_when_present() {
        let mut decision = sample_decision();
        decision.execution = ExecutionMode::Direct;
        decision.seat_role = SeatRole::Single;
        decision.seat_tier = decision::SeatTier::Cheap;
        decision.orchestrator = decision::Seat {
            harness: "claude".to_string(),
            model: "sonnet".to_string(),
        };
        decision.workflow = None;
        decision.confidence = BTreeMap::new();
        decision.decider = Decider::Typesafe;
        decision.domains = vec!["security".to_string(), "data".to_string()];
        let line = announce_line(&decision);
        assert_eq!(
            line,
            "proxy: direct \u{b7} single seat \u{b7} claude/sonnet (cheap) \u{b7} no workflow \u{b7} \
             typesafe \u{b7} domains: security, data"
        );
    }

    /// Issue #537 (A2): `human_fields`' `domains` row is a plain field (not
    /// `row()`'s source/confidence shape -- `domains` aggregates up to six
    /// separate answers, which does not fit one field's single confidence),
    /// showing `none` when empty and the joined list otherwise.
    #[test]
    fn human_fields_shows_domains_as_a_plain_field() {
        let empty = human_fields(&sample_decision(), 0.5);
        assert_eq!(
            empty.iter().find(|(field, _)| *field == "domains"),
            Some(&("domains", "none".to_string()))
        );

        let mut decision = sample_decision();
        decision.domains = vec!["security".to_string(), "data".to_string()];
        let filled = human_fields(&decision, 0.5);
        assert_eq!(
            filled.iter().find(|(field, _)| *field == "domains"),
            Some(&("domains", "security, data".to_string()))
        );
    }

    #[test]
    fn prompt_layer_is_bounded_and_starts_with_the_header() {
        let layer = prompt_layer(&sample_decision(), None);
        let lines: Vec<&str> = layer.lines().collect();
        assert!(lines.len() <= 8, "{lines:?}");
        assert_eq!(lines[0], "[zirv proxy]");
    }

    /// Issue #537 (A2): both new lines are conditional -- present together
    /// on a decision that carries domains and a clarification signal, and
    /// absent on the plain `sample_decision` (empty domains, `0.0`
    /// clarification) the test above already covers.
    #[test]
    fn prompt_layer_shows_domains_and_a_clarify_instruction_when_present() {
        let mut decision = sample_decision();
        decision.domains = vec!["security".to_string(), "data".to_string()];
        decision.needs_clarification = 0.9;
        decision.needs_clarification_decisive = true;
        let layer = prompt_layer(&decision, None);
        assert!(layer.contains("domains: security, data"), "{layer}");
        assert!(
            layer.contains("clarify: ask the user one precise question before acting"),
            "{layer}"
        );

        let mut clear_decision = sample_decision();
        clear_decision.needs_clarification = 0.1;
        let clear_layer = prompt_layer(&clear_decision, None);
        assert!(!clear_layer.contains("domains:"), "{clear_layer}");
        assert!(!clear_layer.contains("clarify:"), "{clear_layer}");
    }

    /// Jev determinism fix: a `needs_clarification` reading at or above the
    /// threshold that was NOT decisive at merge time must never add the
    /// `clarify:` line -- the raw value alone is not enough.
    #[test]
    fn prompt_layer_omits_clarify_when_needs_clarification_is_not_decisive() {
        let mut decision = sample_decision();
        decision.needs_clarification = 0.9;
        decision.needs_clarification_decisive = false;
        let layer = prompt_layer(&decision, None);
        assert!(!layer.contains("clarify:"), "{layer}");
    }

    #[test]
    fn prompt_layer_tells_a_single_seat_not_to_delegate() {
        let mut decision = sample_decision();
        decision.execution = ExecutionMode::Direct;
        decision.seat_role = SeatRole::Single;
        assert!(
            prompt_layer(&decision, None)
                .contains("You are the single seat for this request: do the work here yourself")
        );

        decision.execution = ExecutionMode::Orchestrated;
        decision.seat_role = SeatRole::Orchestrator;
        assert!(!prompt_layer(&decision, None).contains("You are the single seat"));
    }

    /// Change 2 (started workflow reaches the seat): a decision that named a
    /// workflow AND actually started one names the concrete running instance
    /// and tells the seat what to do with it, instead of only the kind.
    #[test]
    fn prompt_layer_names_the_started_workflow_instance_when_one_started() {
        let decision = sample_decision();
        assert_eq!(decision.workflow.as_deref(), Some("feature"));
        let layer = prompt_layer(&decision, Some("feature-3f2a"));
        assert!(
            layer.contains(
                "workflow: feature (started feature-3f2a) -- run `zirv workflow status` and \
                 follow its current step"
            ),
            "{layer}"
        );
    }

    /// Change 2: when the intake decided a workflow KIND but no instance
    /// actually started (skipped -- an active workflow already exists, or
    /// the start failed), the line stays exactly what it is today: the kind
    /// alone, with no instruction to check a status that doesn't exist.
    #[test]
    fn prompt_layer_names_only_the_kind_when_no_workflow_started() {
        let decision = sample_decision();
        assert_eq!(decision.workflow.as_deref(), Some("feature"));
        let layer = prompt_layer(&decision, None);
        assert!(
            layer.lines().any(|line| line == "workflow: feature"),
            "{layer}"
        );
        assert!(!layer.contains("started"), "{layer}");
        assert!(!layer.contains("zirv workflow status"), "{layer}");
    }

    /// Change 2: a decision that names no workflow at all keeps `workflow:
    /// none`, regardless of `started_workflow_id` (which should never be
    /// `Some` in that case in practice, but the formatter must not fabricate
    /// a workflow line if it were).
    #[test]
    fn prompt_layer_names_no_workflow_when_the_decision_named_none() {
        let mut decision = sample_decision();
        decision.workflow = None;
        let layer = prompt_layer(&decision, None);
        assert!(layer.contains("workflow: none"), "{layer}");
    }

    #[test]
    fn asking_line_names_typesafe_and_its_model() {
        let mut cfg = CtxConfig::default();
        cfg.proxy.decider = ProxyDecider::Typesafe;
        cfg.proxy.typesafe.model = "jev-latest".to_string();
        assert_eq!(
            asking_line(&cfg),
            "proxy: asking typesafe (jev-latest)\u{2026}"
        );
    }

    #[test]
    fn asking_line_names_the_helper_decider() {
        let mut cfg = CtxConfig::default();
        cfg.proxy.decider = ProxyDecider::Helper;
        assert_eq!(asking_line(&cfg), "proxy: asking helper model\u{2026}");
    }

    #[test]
    fn asking_line_names_no_decider_for_the_deterministic_baseline() {
        let mut cfg = CtxConfig::default();
        cfg.proxy.decider = ProxyDecider::Deterministic;
        assert_eq!(
            asking_line(&cfg),
            "proxy: using the deterministic baseline\u{2026}"
        );
    }

    #[test]
    fn read_request_reads_until_a_blank_line() {
        let mut input = std::io::Cursor::new(b"line one\nline two\n\nnever read\n".to_vec());
        let request = read_request(&mut input).expect("some request");
        assert_eq!(request, "line one\nline two");
    }

    #[test]
    fn read_request_reads_until_eof_with_no_blank_line() {
        let mut input = std::io::Cursor::new(b"only line".to_vec());
        assert_eq!(read_request(&mut input), Some("only line".to_string()));
    }

    #[test]
    fn read_request_is_none_for_whitespace_only_input() {
        let mut input = std::io::Cursor::new(b"   \n\n".to_vec());
        assert_eq!(read_request(&mut input), None);
    }

    #[test]
    fn persist_and_latest_for_repo_round_trip() {
        let state_dir = tempfile::tempdir().expect("tempdir");
        let repo = tempfile::tempdir().expect("tempdir");
        let mut decision = sample_decision();
        decision.repo = repo.path().to_path_buf();
        persist(state_dir.path(), &decision).expect("persist");
        let found = latest_for_repo(state_dir.path(), repo.path()).expect("found");
        assert_eq!(found.request_sha256, decision.request_sha256);
    }

    #[test]
    fn persist_appends_a_delegation_row_only_when_usage_is_present() {
        let state_dir = tempfile::tempdir().expect("tempdir");
        let mut decision = sample_decision();
        decision.usage = Some(decision::Usage {
            input_tokens: 100,
            output_tokens: 5,
        });
        persist(state_dir.path(), &decision).expect("persist");
        let delegations = log::read_delegations(
            &state::StateDir::from_path(state_dir.path().to_path_buf()),
            10,
        );
        assert_eq!(delegations.len(), 1);
        assert_eq!(delegations[0].agent, "typesafe");
        assert_eq!(delegations[0].model.as_deref(), Some(TYPESAFE_MODEL_ID));
        assert_eq!(delegations[0].input_tokens, 100);
    }

    #[test]
    fn persist_uses_this_processs_session_and_principal_when_set_else_the_documented_fallbacks() {
        use crate::commands::ctx::adapters::SESSION_ENV;
        use crate::commands::ctx::agent::PRINCIPAL_ENV;

        let had_session = std::env::var(SESSION_ENV).ok();
        let had_principal = std::env::var(PRINCIPAL_ENV).ok();
        // SAFETY (test-only): restored at the end of this test regardless
        // of outcome.
        unsafe {
            std::env::remove_var(SESSION_ENV);
            std::env::remove_var(PRINCIPAL_ENV);
        }

        let mut decision = sample_decision();
        decision.usage = Some(decision::Usage {
            input_tokens: 10,
            output_tokens: 1,
        });

        let unset_dir = tempfile::tempdir().expect("tempdir");
        persist(unset_dir.path(), &decision).expect("persist");
        let rows = log::read_delegations(
            &state::StateDir::from_path(unset_dir.path().to_path_buf()),
            10,
        );
        assert_eq!(rows[0].session, "proxy");
        assert_eq!(rows[0].principal, "root");

        unsafe {
            std::env::set_var(SESSION_ENV, "sess-537");
            std::env::set_var(PRINCIPAL_ENV, "root/child-537");
        }
        let set_dir = tempfile::tempdir().expect("tempdir");
        persist(set_dir.path(), &decision).expect("persist");
        let rows = log::read_delegations(
            &state::StateDir::from_path(set_dir.path().to_path_buf()),
            10,
        );
        assert_eq!(rows[0].session, "sess-537");
        assert_eq!(rows[0].principal, "root/child-537");

        unsafe {
            match had_session {
                Some(value) => std::env::set_var(SESSION_ENV, value),
                None => std::env::remove_var(SESSION_ENV),
            }
            match had_principal {
                Some(value) => std::env::set_var(PRINCIPAL_ENV, value),
                None => std::env::remove_var(PRINCIPAL_ENV),
            }
        }
    }

    #[test]
    fn activation_is_err_when_disabled() {
        let cfg = CtxConfig::default();
        assert!(activation(&cfg).is_err());
    }

    #[test]
    fn activation_is_err_for_the_deterministic_decider() {
        let mut cfg = CtxConfig::default();
        cfg.proxy.enabled = true;
        cfg.proxy.decider = ProxyDecider::Deterministic;
        let error = activation(&cfg).expect_err("must be err");
        assert!(error.contains("deterministic"), "{error}");
    }

    #[test]
    fn activation_names_the_unset_credential_env() {
        let mut cfg = CtxConfig::default();
        cfg.proxy.enabled = true;
        cfg.jev.intake_savings = true;
        cfg.proxy.decider = ProxyDecider::Typesafe;
        cfg.proxy.typesafe.credential_env = "PROXY_TEST_NEVER_SET_537".to_string();
        let error = activation(&cfg).expect_err("must be err");
        assert!(error.contains("PROXY_TEST_NEVER_SET_537"), "{error}");
    }

    #[test]
    fn activation_is_ok_for_typesafe_with_the_credential_set() {
        let mut cfg = CtxConfig::default();
        cfg.proxy.enabled = true;
        cfg.proxy.decider = ProxyDecider::Typesafe;
        cfg.proxy.typesafe.credential_env = "PROXY_TEST_KEY_SET_537".to_string();
        // SAFETY (test-only): a unique env var name this test owns.
        unsafe {
            std::env::set_var("PROXY_TEST_KEY_SET_537", "secret");
        }
        let result = activation(&cfg);
        unsafe {
            std::env::remove_var("PROXY_TEST_KEY_SET_537");
        }
        assert!(result.is_ok(), "{result:?}");
    }

    #[test]
    fn decide_never_panics_and_falls_back_to_deterministic_without_a_credential() {
        // SAFETY (test-only): ensures this well-known var is unset for the
        // duration of this test, regardless of the outer environment.
        let had = std::env::var("TYPESAFE_API_KEY").ok();
        unsafe {
            std::env::remove_var("TYPESAFE_API_KEY");
        }
        let repo = tempfile::tempdir().expect("tempdir");
        let state_dir = tempfile::tempdir().expect("tempdir");
        // No credential AND no adapter to fall back to (an unknown agent
        // name fails `resolve_default` immediately, without spawning or
        // probing anything real) -- otherwise this test would reach for
        // whatever coding harness happens to be installed on the machine
        // running it.
        let cfg = CtxConfig {
            agent: Some("no-such-adapter-537".to_string()),
            ..CtxConfig::default()
        };
        let decision = decide(
            &cfg,
            state_dir.path(),
            repo.path(),
            "fix the typo in README",
        );
        assert_eq!(decision.decider, Decider::Deterministic);
        assert!(
            decision
                .fallbacks
                .iter()
                .any(|line| line.contains("credential env")),
            "{:?}",
            decision.fallbacks
        );
        if let Some(value) = had {
            unsafe {
                std::env::set_var("TYPESAFE_API_KEY", value);
            }
        }
    }

    #[test]
    fn proxy_model_intake_masks_the_request_with_a_stable_cache_input() {
        let repo = crate::commands::ctx::testenv::repo();
        let home = repo.path().join("home");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);
        let state_dir = repo.path().join("state");
        let mut cfg = CtxConfig::default();
        cfg.obfuscate.mode = super::super::config::ObfuscateMode::Obfuscate;
        let roster = decision::Roster::gather(&cfg, repo.path());
        let secret = "ghp_abcdefghijklmnopqrstuvwxyz123456";

        let first = protected_model_intake(
            &cfg,
            &state_dir,
            repo.path(),
            &format!("review token {secret}"),
            &roster,
        )
        .expect("first protected intake");
        let second = protected_model_intake(
            &cfg,
            &state_dir,
            repo.path(),
            &format!("review token {secret}"),
            &roster,
        )
        .expect("second protected intake");

        assert!(!first.0.request.contains(secret), "{}", first.0.request);
        assert!(
            first.0.request.contains("ZIRV_SECRET_GITHUB_TOKEN_1"),
            "{}",
            first.0.request
        );
        assert_eq!(
            super::super::jev::cache_key_for(&first.0, &first.1, &cfg.proxy.typesafe.model)
                .expect("first cache key"),
            super::super::jev::cache_key_for(&second.0, &second.1, &cfg.proxy.typesafe.model)
                .expect("second cache key"),
            "stable placeholders must preserve Jev's serialized-payload cache key"
        );
    }

    #[test]
    fn proxy_falls_back_to_deterministic_when_request_masking_fails() {
        let repo = crate::commands::ctx::testenv::repo();
        let home = repo.path().join("home");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);
        let state_dir = repo.path().join("state");
        let mut cfg = CtxConfig::default();
        cfg.proxy.decider = ProxyDecider::Typesafe;
        cfg.obfuscate.mode = super::super::config::ObfuscateMode::Obfuscate;
        cfg.obfuscate.literals_file = Some("missing-sensitive-literals.txt".to_string());

        let decision = decide(
            &cfg,
            &state_dir,
            repo.path(),
            "review ghp_abcdefghijklmnopqrstuvwxyz123456",
        );

        assert_eq!(decision.decider, Decider::Deterministic);
        assert!(decision.usage.is_none());
        assert!(
            decision
                .fallbacks
                .iter()
                .any(|line| line.contains("sensitive-data masking")),
            "{:?}",
            decision.fallbacks
        );
    }

    #[derive(Debug, serde::Deserialize)]
    struct BatteryFile {
        path: String,
        lines: usize,
    }

    #[derive(Debug, serde::Deserialize)]
    struct BatteryCase {
        name: String,
        request: String,
        #[serde(default)]
        files: Vec<BatteryFile>,
        execution: String,
        min_complexity: String,
        #[serde(default = "default_min_risk")]
        min_risk: String,
        workflow: Option<String>,
    }

    fn default_min_risk() -> String {
        "low".to_string()
    }

    #[derive(Debug, serde::Deserialize)]
    struct Battery {
        cases: Vec<BatteryCase>,
    }

    fn fixture(name: &str) -> std::path::PathBuf {
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests")
            .join("fixtures")
            .join(name)
    }

    fn parse_execution_label(label: &str) -> ExecutionMode {
        match label {
            "direct" => ExecutionMode::Direct,
            "bounded" => ExecutionMode::Bounded,
            "orchestrated" => ExecutionMode::Orchestrated,
            other => panic!("unknown execution label '{other}' in battery fixture"),
        }
    }

    fn parse_complexity_label(label: &str) -> Complexity {
        match label {
            "trivial" => Complexity::Trivial,
            "bounded" => Complexity::Bounded,
            "substantial" => Complexity::Substantial,
            "architectural" => Complexity::Architectural,
            other => panic!("unknown complexity label '{other}' in battery fixture"),
        }
    }

    fn parse_risk_label(label: &str) -> RiskBand {
        match label {
            "low" => RiskBand::Low,
            "medium" => RiskBand::Medium,
            "high" => RiskBand::High,
            "critical" => RiskBand::Critical,
            other => panic!("unknown risk label '{other}' in battery fixture"),
        }
    }

    /// A throwaway git repo with one committed baseline file plus the
    /// battery case's own UNTRACKED shape (`files`): this is exactly what
    /// `classify::git_change_input` measures via `git diff --numstat` (the
    /// committed baseline) and `git ls-files --others` (the untracked
    /// shape).
    ///
    /// Issue #537 fix: the baseline classification (`decision::
    /// classify_request`) no longer measures the repository at all -- a
    /// case's `files` shape therefore no longer drives its own `execution`/
    /// `min_complexity`/`workflow` expectation (every case's deterministic
    /// baseline is `direct`/`trivial`/`none` now, regardless of shape; see
    /// the `"large-unrelated-branch-diff"` case, whose entire point is a
    /// large shape that must NOT move the outcome). Jev determinism fix
    /// (2026-09-18 replay): `build_intake` no longer measures the repository
    /// at all either (see `decision::IntakeRepository`'s own doc comment) --
    /// `shaped_repo` is kept solely for that regression case, proving the
    /// large shape it still creates on disk truly moves nothing.
    fn shaped_repo(files: &[BatteryFile]) -> tempfile::TempDir {
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

        for file in files {
            let full = repo.path().join(&file.path);
            if let Some(parent) = full.parent() {
                std::fs::create_dir_all(parent).expect("mkdir");
            }
            let content: String = (0..file.lines).map(|n| format!("line {n}\n")).collect();
            std::fs::write(&full, content).expect("write case file");
        }
        repo
    }

    /// Issue #537 seam: the deterministic decider's own floor, exercised
    /// against a small battery of realistic requests (`tests/fixtures/
    /// proxy/battery.json`) rather than one hand-picked example. Every
    /// expectation here is what `decide()` with `[proxy] decider =
    /// "deterministic"` ACTUALLY returns (verified by running this test,
    /// not guessed) -- see this task's own report for the human-expectation
    /// gaps this surfaced, which is exactly what the model deciders exist
    /// to close.
    ///
    /// Issue #537 fix: the baseline classifies from request TEXT ONLY now
    /// (never the repository's own diff, which used to inflate every
    /// request's complexity/risk on a feature branch carrying unrelated
    /// changes, and the monotonic floor then forbade a model decider from
    /// ever lowering it back down). Every case's deterministic `execution`/
    /// `min_complexity`/`workflow` is therefore `direct`/`trivial`/`none`
    /// regardless of its `files` shape; the `"large-unrelated-branch-diff"`
    /// case exists specifically to pin that a large on-disk diff cannot
    /// move it.
    #[test]
    fn deterministic_battery_matches_recorded_expectations() {
        let text = std::fs::read_to_string(fixture("proxy/battery.json")).expect("battery fixture");
        let battery: Battery = serde_json::from_str(&text).expect("parse battery fixture");
        assert!(
            battery.cases.len() >= 8,
            "battery must carry at least 8 cases, got {}",
            battery.cases.len()
        );

        let cfg = CtxConfig {
            proxy: crate::commands::ctx::config::ProxyConfig {
                decider: ProxyDecider::Deterministic,
                ..crate::commands::ctx::config::ProxyConfig::default()
            },
            ..CtxConfig::default()
        };
        let state_dir = tempfile::tempdir().expect("tempdir");

        for case in &battery.cases {
            let repo = shaped_repo(&case.files);
            let decision = decide(&cfg, state_dir.path(), repo.path(), &case.request);
            assert_eq!(
                decision.decider,
                Decider::Deterministic,
                "{}: decider",
                case.name
            );
            assert_eq!(
                decision.execution,
                parse_execution_label(&case.execution),
                "{}: execution was {:?}",
                case.name,
                decision.execution
            );
            let min_complexity = parse_complexity_label(&case.min_complexity);
            assert!(
                decision.complexity >= min_complexity,
                "{}: complexity {:?} is below the recorded minimum {:?}",
                case.name,
                decision.complexity,
                min_complexity
            );
            let min_risk = parse_risk_label(&case.min_risk);
            assert!(
                decision.risk >= min_risk,
                "{}: risk {:?} is below the recorded minimum {:?}",
                case.name,
                decision.risk,
                min_risk
            );
            assert_eq!(decision.workflow, case.workflow, "{}: workflow", case.name);
            // Issue #537: every battery case is `direct` or `bounded`, never
            // `orchestrated`, so `seat_role` must always be `Single` -- and
            // `seat_tier` must follow `execution` exactly for those two
            // modes. (The `orchestrated` arm below mirrors the frontier seat
            // gate -- `SeatTier::from_execution_complexity_risk`, private to
            // `decision` -- for defense in depth; no case here reaches it.)
            assert_eq!(
                decision.seat_role,
                decision::SeatRole::Single,
                "{}: seat_role",
                case.name
            );
            let expected_seat_tier = match decision.execution {
                ExecutionMode::Direct => decision::SeatTier::Cheap,
                ExecutionMode::Bounded => decision::SeatTier::Standard,
                ExecutionMode::Orchestrated => {
                    if decision.complexity == Complexity::Architectural
                        || decision.risk >= RiskBand::High
                    {
                        decision::SeatTier::Frontier
                    } else {
                        decision::SeatTier::Standard
                    }
                }
            };
            assert_eq!(
                decision.seat_tier, expected_seat_tier,
                "{}: seat_tier",
                case.name
            );
            let expected_worker_tier = match decision.execution {
                ExecutionMode::Orchestrated => Tier::Standard,
                ExecutionMode::Direct | ExecutionMode::Bounded => Tier::Cheap,
            };
            assert_eq!(
                decision.worker_tier, expected_worker_tier,
                "{}: worker_tier",
                case.name
            );
        }
    }

    /// Review finding: exercises `decide()` end-to-end with `decider =
    /// helper` against the real `fake-model.sh` `proxy` fixture (not just
    /// `decision::merge` in isolation), on a sensitive-surface request --
    /// the fake model's own `proxy` answers never touch `risk`, so the only
    /// way `security_review`/`independent_review` stay `true` on the merged
    /// decision is if the baseline's own text-driven flags survive the
    /// merge, which is exactly what this task's fix restores.
    #[test]
    fn decide_with_the_helper_decider_keeps_baseline_validation_flags() {
        // SAFETY (test-only): restored at the end of this test regardless
        // of outcome.
        let had_mode = std::env::var("FAKE_MODEL_MODE").ok();
        unsafe {
            std::env::set_var("FAKE_MODEL_MODE", "proxy");
        }

        let repo = tempfile::tempdir().expect("tempdir");
        let state_dir = tempfile::tempdir().expect("tempdir");
        let cfg = CtxConfig {
            agent: Some("claude".to_string()),
            agent_bin: Some(format!("sh {}", fixture("fake-model.sh").display())),
            proxy: crate::commands::ctx::config::ProxyConfig {
                decider: ProxyDecider::Helper,
                ..crate::commands::ctx::config::ProxyConfig::default()
            },
            ..CtxConfig::default()
        };
        let request = "rotate the shared credential constant used by session auth";

        let decision = decide(&cfg, state_dir.path(), repo.path(), request);

        unsafe {
            match had_mode {
                Some(value) => std::env::set_var("FAKE_MODEL_MODE", value),
                None => std::env::remove_var("FAKE_MODEL_MODE"),
            }
        }

        assert_eq!(
            decision.decider,
            Decider::Helper,
            "expected the helper decider to win: {:?}",
            decision.fallbacks
        );
        assert!(
            decision.validation.security_review,
            "{:?}",
            decision.validation
        );
        assert!(
            decision.validation.independent_review,
            "{:?}",
            decision.validation
        );
    }

    #[derive(Debug, serde::Deserialize)]
    struct JevBatteryExpect {
        execution: String,
        complexity: String,
        workflow: String,
        seat_tier: String,
        // Jev's `intent` answer is advisory only -- this battery asserts
        // execution/complexity/workflow/seat_tier, never intent -- but the
        // field stays on the fixture (and this type) for a human reading
        // `jev-battery.json` to see what Jev actually said.
        #[allow(dead_code)]
        intent: String,
    }

    #[derive(Debug, serde::Deserialize)]
    struct JevBatteryCase {
        id: String,
        request: String,
        expect: JevBatteryExpect,
    }

    /// One case's `(execution, complexity, workflow, seat_tier)` tuple, the
    /// same four fields the fixture records rulings for.
    fn battery_fields(decision: &ProxyDecision) -> (String, String, String, String) {
        (
            lower_debug(decision.execution),
            lower_debug(decision.complexity),
            decision
                .workflow
                .clone()
                .unwrap_or_else(|| "none".to_string()),
            decision.seat_tier.label().to_string(),
        )
    }

    /// Replays the committed `tests/fixtures/proxy/jev-battery.json` against
    /// the REAL TypeSafe Jev API TWICE per case -- issue #537's own recorded
    /// rulings, re-verified against the pinned `jev-1.13.0` model and the
    /// `min_margin` gate: four live double-run invocations on 2026-09-18
    /// agreed on 22 of 24 cases every time, after re-recording four rulings
    /// that were STABLY different from the old fixture (never flipping, just
    /// consistently a new answer): `plugin-system`/`tui-redesign` workflow to
    /// `none` (their own workflow answer's margin no longer clears the
    /// floor, so both fall to the baseline's own `none`); `security-
    /// credential` complexity to `bounded` and `perf-investigation` down a
    /// full tier to `bounded` (both a stable, decisive, DIFFERENT answer from
    /// the pinned model, not a margin-gate artifact -- `merge`'s own
    /// monotonic floor only ever raises a decisive model answer over the
    /// baseline, so recording the higher, stable value here is always
    /// consistent with it). The remaining two, `bump-timeout` and `bug-
    /// backtrace`, genuinely straddle the margin floor: across those four
    /// runs each produced its recorded ruling at least once but also an
    /// instability or a mismatch at least once, in no consistent direction --
    /// real residual model noise `min_margin`'s current default does not
    /// fully suppress for these two request shapes, left as their
    /// originally-recorded ruling rather than loosened to tolerate either
    /// outcome.
    ///
    /// Frontier seat gate (wrapper-overhead benchmark, 2026-09-22): `seat_
    /// tier` no longer follows `execution`/`complexity` alone -- see
    /// `decision::SeatTier::from_execution_complexity_risk`. A live re-run
    /// against this fixture confirmed every recorded `orchestrated` case's
    /// `seat_tier` is unchanged under the new rule: the four `architectural`
    /// cases (`plugin-system`, `sqlite-migration`, `tui-redesign`, `new-
    /// adapter`) earn `frontier` unconditionally, and the one `substantial`
    /// case (`dependency-upgrade`) also stays `frontier` because Jev's own
    /// risk answer for it is `high` (the request touches provider-client
    /// credentials), not because complexity alone would clear the gate.
    ///
    /// Jev determinism fix (2026-09-18 replay): with `build_intake` no
    /// longer measuring the repository at all (see `decision::
    /// IntakeRepository`'s own doc comment), the request body is now fixed
    /// by construction for a given `case.request`, so calling twice replays
    /// the IDENTICAL request -- any difference between the two runs is Jev's
    /// own answer-to-answer instability, not a body that drifted underneath
    /// it. A flip between the two runs is reported as an INSTABILITY,
    /// distinct from a MISMATCH against the recorded ruling (checked against
    /// the first run only): the two failure modes have different causes and
    /// different fixes (a mismatch means the fixture's own recorded ruling
    /// is stale; an instability means `[proxy] min_margin` may need
    /// raising, or the question wording sharpening).
    ///
    /// Skips (passes, printing one line) when `TYPESAFE_API_KEY` is unset:
    /// this test never touches the Keychain and never fails just because a
    /// key is absent, so it stays green in CI and on a machine with no
    /// TypeSafe credential. Run it with a key: `TYPESAFE_API_KEY=... cargo
    /// nextest run jev_live_battery`. Costs roughly 50 calls (two per case)
    /// at about 6k input tokens each (TypeSafe's own published $0.042/MTok
    /// input rate -- about two cents total). `state_dir` points at a temp
    /// dir, never the real `<state>/proxy-decisions.jsonl`, so a real run's
    /// own persisted decisions and spend rows are untouched -- it persists
    /// exactly like any other `decide()` call, just into a throwaway
    /// directory.
    #[test]
    fn jev_live_battery_matches_recorded_rulings() {
        let key = std::env::var("TYPESAFE_API_KEY").unwrap_or_default();
        if key.trim().is_empty() {
            println!("skipped: TYPESAFE_API_KEY unset");
            return;
        }

        let text = std::fs::read_to_string(fixture("proxy/jev-battery.json"))
            .expect("jev-battery fixture");
        let cases: Vec<JevBatteryCase> =
            serde_json::from_str(&text).expect("parse jev-battery fixture");
        assert!(!cases.is_empty(), "jev-battery fixture must not be empty");

        let mut cfg = CtxConfig::default();
        cfg.proxy.enabled = true;
        cfg.proxy.decider = ProxyDecider::Typesafe;
        // The whole point of the double call below is two INDEPENDENT live
        // answers to the identical body -- `jev::ask`'s own decision cache
        // would otherwise replay the first call's answer for the second,
        // making a real instability undetectable.
        cfg.jev.cache_ttl_secs = 0;

        let repo = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let state_dir = tempfile::tempdir().expect("tempdir");

        let mut instabilities = Vec::new();
        let mut mismatches = Vec::new();
        for case in &cases {
            let first = decide(&cfg, state_dir.path(), repo, &case.request);
            let second = decide(&cfg, state_dir.path(), repo, &case.request);

            if first.decider != Decider::Typesafe {
                mismatches.push(format!(
                    "{}: decider fell back to {:?} on the first run ({:?})",
                    case.id, first.decider, first.fallbacks
                ));
                continue;
            }
            if second.decider != Decider::Typesafe {
                mismatches.push(format!(
                    "{}: decider fell back to {:?} on the second run ({:?})",
                    case.id, second.decider, second.fallbacks
                ));
                continue;
            }

            let (first_execution, first_complexity, first_workflow, first_seat_tier) =
                battery_fields(&first);
            let (second_execution, second_complexity, second_workflow, second_seat_tier) =
                battery_fields(&second);

            for (field, before, after) in [
                ("execution", &first_execution, &second_execution),
                ("complexity", &first_complexity, &second_complexity),
                ("workflow", &first_workflow, &second_workflow),
                ("seat_tier", &first_seat_tier, &second_seat_tier),
            ] {
                if before != after {
                    instabilities.push(format!(
                        "{}: {field} flipped between two identical calls: {before} then {after}",
                        case.id
                    ));
                }
            }

            if first_execution != case.expect.execution {
                mismatches.push(format!(
                    "{}: execution expected {} got {first_execution}",
                    case.id, case.expect.execution
                ));
            }
            if first_complexity != case.expect.complexity {
                mismatches.push(format!(
                    "{}: complexity expected {} got {first_complexity}",
                    case.id, case.expect.complexity
                ));
            }
            if first_workflow != case.expect.workflow {
                mismatches.push(format!(
                    "{}: workflow expected {} got {first_workflow}",
                    case.id, case.expect.workflow
                ));
            }
            if first_seat_tier != case.expect.seat_tier {
                mismatches.push(format!(
                    "{}: seat_tier expected {} got {first_seat_tier}",
                    case.id, case.expect.seat_tier
                ));
            }
        }

        // Reported together, in one assertion, so a single run surfaces
        // both failure modes at once -- each line is already labeled
        // "instability" or carries its own "expected .. got .." shape, so
        // the two causes stay distinguishable in the combined message.
        assert!(
            instabilities.is_empty() && mismatches.is_empty(),
            "{} instability(ies) and {} mismatch(es) out of {} cases:\n{}",
            instabilities.len(),
            mismatches.len(),
            cases.len(),
            instabilities
                .iter()
                .map(|line| format!("instability: {line}"))
                .chain(mismatches.iter().cloned())
                .collect::<Vec<_>>()
                .join("\n")
        );
    }
}
