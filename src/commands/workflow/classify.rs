//! Deterministic intent, complexity, and risk classification.

use std::collections::BTreeSet;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use clap::{Args, ValueEnum};
use serde::{Deserialize, Serialize};

use crate::commands::ctx::CtxResult;

use super::selection::word_tokens_ordered;

const MAX_TASK_BYTES: usize = 8 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ValueEnum)]
#[serde(rename_all = "kebab-case")]
pub enum Intent {
    Feature,
    Bugfix,
    Refactor,
    Spike,
    Review,
    Other,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, ValueEnum)]
#[serde(rename_all = "kebab-case")]
pub enum Complexity {
    Trivial,
    Bounded,
    Substantial,
    Architectural,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, ValueEnum)]
#[serde(rename_all = "kebab-case")]
pub enum RiskBand {
    Low,
    Medium,
    High,
    Critical,
}

/// Distinguish unavailable Git measurement from a measured low-risk change; unavailable measurement cannot validate a declared risk band.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum RiskMeasurement {
    #[default]
    Measured,
    Unavailable {
        reason: String,
    },
}

/// The safer default when the net cannot run at all: raise the band one
/// step rather than trust an unmeasured declaration. `Critical` has nowhere
/// further to go.
pub(crate) fn escalate_one_band(band: RiskBand) -> RiskBand {
    match band {
        RiskBand::Low => RiskBand::Medium,
        RiskBand::Medium => RiskBand::High,
        RiskBand::High => RiskBand::Critical,
        RiskBand::Critical => RiskBand::Critical,
    }
}

fn score_floor_for_band(band: RiskBand) -> u16 {
    match band {
        RiskBand::Low => 0,
        RiskBand::Medium => 20,
        RiskBand::High => 45,
        RiskBand::Critical => 70,
    }
}

/// Marks a classification's risk measurement as unavailable and applies the
/// fail-safe policy: escalate the risk band one step (the ceiling, unmoved,
/// when already `Critical`). Returns whether the band actually moved, so a
/// caller that also re-materializes workflow steps on a risk increase can
/// share that path instead of duplicating it. See the Decision Log entry
/// "Unmeasurable risk fails safe, not open" for why escalation was chosen
/// over "keep the declared/prior band but demand its evidence".
pub(crate) fn mark_unavailable(
    classification: &mut Classification,
    reason: impl Into<String>,
) -> bool {
    let reason = reason.into();
    let previous = classification.risk;
    let escalated = escalate_one_band(previous);
    let raised = escalated != previous;
    if raised {
        classification.reasons.push(format!(
            "risk escalated to {escalated:?}: measurement unavailable ({reason})"
        ));
        classification.risk = escalated;
        classification.risk_score = classification
            .risk_score
            .max(score_floor_for_band(escalated));
    } else {
        classification.reasons.push(format!(
            "measurement unavailable ({reason}); risk already at the Critical ceiling"
        ));
    }
    classification.risk_measurement = RiskMeasurement::Unavailable { reason };
    classification.reasons.sort();
    raised
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, ValueEnum)]
#[serde(rename_all = "kebab-case")]
pub enum WorkDomain {
    #[default]
    General,
    Frontend,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DomainClassification {
    pub domain: WorkDomain,
    pub score: u8,
    pub reasons: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Classification {
    pub intent: Intent,
    pub complexity: Complexity,
    pub risk: RiskBand,
    pub risk_score: u16,
    pub changed_files: usize,
    pub changed_lines: usize,
    /// Keep real path boundaries for team seat claims, capped to bound durable state; `changed_files` remains the true total. Missing older data falls back to count bucketing. (#541)
    #[serde(default)]
    pub changed_paths: Vec<String>,
    /// The change surface was declared on the command line (`--path`/
    /// `--changed-lines`) rather than measured from Git. Consumers can tell a
    /// measured classification from a stated one; the risk band itself is
    /// never *lower* than the measured tree would give (see
    /// [`from_args`]).
    #[serde(default)]
    pub declared_scope: bool,
    /// Orthogonal work-domain classification. This chooses methodology, not
    /// permissions; older durable state defaults safely to `general`.
    #[serde(default)]
    pub work_domain: DomainClassification,
    /// Git re-measurement status; older state defaults to `Measured` to preserve its existing interpretation.
    #[serde(default)]
    pub risk_measurement: RiskMeasurement,
    pub reasons: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct ClassificationInput {
    pub task: String,
    pub paths: Vec<PathBuf>,
    pub changed_lines: usize,
    pub tests_changed: bool,
    pub intent_override: Option<Intent>,
    pub complexity_override: Option<Complexity>,
    pub risk_override: Option<RiskBand>,
}

pub fn classify(input: &ClassificationInput) -> CtxResult<Classification> {
    if input.task.len() > MAX_TASK_BYTES {
        return Err(format!("task summary exceeds {MAX_TASK_BYTES} bytes").into());
    }
    let task = input.task.to_ascii_lowercase();
    let intent = input.intent_override.unwrap_or_else(|| infer_intent(&task));
    let work_domain = infer_work_domain(&task, &input.paths);
    let changed_files = input.paths.len();
    let inferred_complexity = infer_complexity(changed_files, input.changed_lines, &input.paths);
    let complexity = input.complexity_override.unwrap_or(inferred_complexity);

    let mut score = 0u16;
    let mut reasons = Vec::new();
    let lowered_paths: Vec<String> = input
        .paths
        .iter()
        .map(|path| {
            path.to_string_lossy()
                .replace('\\', "/")
                .to_ascii_lowercase()
        })
        .collect();
    let mut sensitive_floor: Option<RiskBand> = None;
    // Take the maximum signal floor directly from each signal; later signals and wording changes must never lower it.
    let raise_floor = |band: RiskBand, floor: &mut Option<RiskBand>| {
        *floor = Some(floor.map_or(band, |current: RiskBand| current.max(band)));
    };
    if add_path_signal(
        &lowered_paths,
        &["auth", "security", "permission", "credential", "secret"],
        30,
        "authentication/security surface",
        &mut score,
        &mut reasons,
    ) {
        raise_floor(RiskBand::High, &mut sensitive_floor);
    }
    if add_path_signal(
        &lowered_paths,
        &["migration", "schema", "database", "sql"],
        25,
        "database/schema surface",
        &mut score,
        &mut reasons,
    ) {
        raise_floor(RiskBand::High, &mut sensitive_floor);
    }
    add_path_signal(
        &lowered_paths,
        &[
            "deploy",
            "docker",
            ".github/workflows",
            "terraform",
            "config",
        ],
        20,
        "deployment/configuration surface",
        &mut score,
        &mut reasons,
    );
    add_path_signal(
        &lowered_paths,
        &["concurr", "thread", "async", "lock", "atomic"],
        20,
        "concurrency-sensitive surface",
        &mut score,
        &mut reasons,
    );
    add_path_signal(
        &lowered_paths,
        &["api", "public", "lib.rs", "mod.rs"],
        15,
        "public API/module boundary",
        &mut score,
        &mut reasons,
    );

    let line_points = match input.changed_lines {
        0..=20 => 0,
        21..=150 => 8,
        151..=500 => 18,
        _ => 30,
    };
    if line_points > 0 {
        score += line_points;
        reasons.push(format!(
            "{} changed lines (+{line_points})",
            input.changed_lines
        ));
    }
    if changed_files > 8 {
        score += 15;
        reasons.push(format!("cross-file change: {changed_files} files (+15)"));
    }
    let modules: BTreeSet<_> = lowered_paths
        .iter()
        .filter_map(|path| path.split('/').next())
        .collect();
    if modules.len() > 2 {
        score += 10;
        reasons.push(format!(
            "cross-module impact: {} roots (+10)",
            modules.len()
        ));
    }
    if !input.tests_changed && !matches!(intent, Intent::Spike | Intent::Review) {
        score += 10;
        reasons.push("no changed test path (+10)".to_string());
    }
    score = score.min(100);
    let inferred_risk = band_for_score(score);
    let mut risk = input.risk_override.unwrap_or(inferred_risk);
    if let Some(floor) = sensitive_floor
        && risk < floor
    {
        if input.risk_override.is_some() {
            return Err(format!(
                "risk override '{risk:?}' is below the required High floor for a sensitive surface"
            )
            .into());
        }
        risk = floor;
        reasons.push("sensitive-surface risk floor: High".to_string());
    }
    if let Some(override_band) = input.risk_override {
        reasons.push(format!("operator risk override: {override_band:?}"));
        risk = override_band;
    }
    if let Some(override_complexity) = input.complexity_override {
        reasons.push(format!(
            "operator complexity override: {override_complexity:?}"
        ));
    }
    if reasons.is_empty() {
        reasons.push("small, isolated deterministic change".to_string());
    }
    reasons.sort();

    Ok(Classification {
        intent,
        complexity,
        risk,
        risk_score: score,
        changed_files,
        changed_lines: input.changed_lines,
        changed_paths: lowered_paths
            .iter()
            .take(MAX_CHANGED_PATHS)
            .cloned()
            .collect(),
        declared_scope: false,
        work_domain,
        risk_measurement: RiskMeasurement::Measured,
        reasons,
    })
}

/// Cap paths so a huge diff cannot inflate a durable classification or `TeamPlan`. (#541)
pub const MAX_CHANGED_PATHS: usize = 200;

/// Shared frontend keywords keep domain classification and Jev refinement consistent. (#782)
pub(crate) const FRONTEND_TASK_TERMS: [&str; 10] = [
    "frontend",
    "front-end",
    "user interface",
    " ui ",
    "component",
    "responsive",
    "accessibility",
    "landing page",
    "dashboard",
    "design system",
];

fn infer_work_domain(task: &str, paths: &[PathBuf]) -> DomainClassification {
    let mut score = 0u8;
    let mut reasons = Vec::new();
    // Task text alone stays below the Frontend threshold; require a real frontend path because the word also appears in non-UI work. (#255)
    if FRONTEND_TASK_TERMS.iter().any(|term| task.contains(term))
        || task.starts_with("ui ")
        || task.ends_with(" ui")
    {
        score = score.saturating_add(40);
        reasons.push("task describes a frontend or visual surface (+40)".into());
    }

    let lowered = paths
        .iter()
        .map(|path| {
            path.to_string_lossy()
                .replace('\\', "/")
                .to_ascii_lowercase()
        })
        .collect::<Vec<_>>();
    if lowered.iter().any(|path| {
        matches!(
            Path::new(path).extension().and_then(|value| value.to_str()),
            Some("css" | "scss" | "sass" | "less" | "tsx" | "jsx" | "vue" | "svelte" | "html")
        )
    }) {
        score = score.saturating_add(45);
        reasons.push("changed path uses a frontend file type (+45)".into());
    }
    if lowered.iter().any(|path| {
        path.contains("/components/")
            || path.contains("/ui/")
            || path.contains("/styles/")
            || path.starts_with("components/")
            || path.starts_with("ui/")
            || path.starts_with("styles/")
    }) {
        score = score.saturating_add(25);
        reasons.push("changed path is in a component, UI, or styles boundary (+25)".into());
    }
    score = score.min(100);
    reasons.sort();
    DomainClassification {
        domain: if score >= 45 {
            WorkDomain::Frontend
        } else {
            WorkDomain::General
        },
        score,
        reasons,
    }
}

/// `true` when the signal matched, so a caller can act on it structurally
/// rather than by searching the reasons it wrote.
fn add_path_signal(
    paths: &[String],
    needles: &[&str],
    points: u16,
    reason: &str,
    score: &mut u16,
    reasons: &mut Vec<String>,
) -> bool {
    if !paths
        .iter()
        .any(|path| needles.iter().any(|needle| path.contains(needle)))
    {
        return false;
    }
    *score += points;
    reasons.push(reason.to_string());
    true
}

/// Leading tokens skipped when looking for the task's leading verb (tier 1
/// of [`infer_intent`]) -- politeness/hedging words that carry no intent
/// signal of their own ("Please can you fix the crash" must still see "fix"
/// as the lead).
const LEADING_FILLER: &[&str] = &[
    "please", "can", "could", "would", "you", "we", "i", "need", "needs", "want", "like", "to",
    "should", "must", "let", "lets", "s", "us", "kindly", "just", "help", "me", "go", "ahead",
    "and", "also", "then",
];

const BUGFIX_LEAD: &[&str] = &[
    "fix",
    "fixes",
    "fixing",
    "bug",
    "bugfix",
    "hotfix",
    "regression",
    "repair",
    "resolve",
    "debug",
    "crash",
    "patch",
];
const REFACTOR_LEAD: &[&str] = &[
    "refactor",
    "refactoring",
    "cleanup",
    "rename",
    "extract",
    "simplify",
    "restructure",
    "reorganize",
    "reorganise",
    "deduplicate",
    "dedupe",
    "consolidate",
    "tidy",
    "split",
];
const SPIKE_LEAD: &[&str] = &[
    "spike",
    "prototype",
    "explore",
    "research",
    "investigate",
    "analyse",
    "analyze",
    "evaluate",
    "experiment",
    "poc",
];
const REVIEW_LEAD: &[&str] = &["review", "audit"];
const FEATURE_LEAD: &[&str] = &[
    "add",
    "adds",
    "adding",
    "implement",
    "build",
    "create",
    "introduce",
    "support",
    "enable",
    "allow",
    "improve",
    "enhance",
    "extend",
    "feat",
];

const BUGFIX_ANYWHERE: &[&str] = &[
    "fix",
    "fixes",
    "fixed",
    "bug",
    "bugs",
    "bugfix",
    "hotfix",
    "broken",
    "breaks",
    "broke",
    "regression",
    "crash",
    "crashes",
    "crashing",
    "panic",
    "panics",
    "fails",
    "failing",
    "failure",
    "failures",
    "error",
    "errors",
    "cannot",
    "exception",
    "exceptions",
    "leak",
    "leaks",
    "wrong",
    "incorrect",
    "flaky",
    "hang",
    "hangs",
    "deadlock",
    "corrupt",
    "corrupted",
];
/// Error words a feature request uses for the behaviour it asks for ("print an error to stderr"); a bare "error" is a symptom.
const CONTRACT_ERROR_WORDS: &[&str] = &["error", "errors", "exception", "exceptions"];
/// A feature word right after one of these is a symptom ("cannot add"), not a request.
const NEGATED_FEATURE_PRECEDERS: &[&str] =
    &["cannot", "can", "t", "not", "unable", "fails", "failed"];
const REFACTOR_ANYWHERE: &[&str] = &[
    "refactor",
    "refactoring",
    "cleanup",
    "duplication",
    "deduplicate",
];
const SPIKE_ANYWHERE: &[&str] = &[
    "spike",
    "prototype",
    "research",
    "feasibility",
    "feasible",
    "poc",
    "exploring",
];
const REVIEW_ANYWHERE: &[&str] = &["review", "reviews", "reviewing", "audit", "auditing"];
const FEATURE_ANYWHERE: &[&str] = &[
    "add",
    "implement",
    "implementing",
    "feature",
    "introduce",
    "create",
];

/// Tier 1 of [`infer_intent`]: the task's leading verb, once leading filler
/// is skipped, starting at `tokens[start]`. `None` when the leading token
/// matches none of the lead word lists, so the caller falls back to tier 2.
fn leading_intent(tokens: &[&str], start: usize) -> Option<Intent> {
    let lead = tokens[start];

    if BUGFIX_LEAD.contains(&lead) {
        return Some(Intent::Bugfix);
    }
    // The bigram "clean up" (refactor lead) before the single-word refactor
    // list, so "up" never needs its own entry there.
    if lead == "clean" && tokens.get(start + 1) == Some(&"up") {
        return Some(Intent::Refactor);
    }
    if REFACTOR_LEAD.contains(&lead) {
        return Some(Intent::Refactor);
    }
    // The phrase "proof of concept" (spike lead) before the single-word
    // spike list, for the same reason.
    if lead == "proof"
        && tokens.get(start + 1) == Some(&"of")
        && tokens.get(start + 2) == Some(&"concept")
    {
        return Some(Intent::Spike);
    }
    if lead == "investigate" {
        // Adjustment (b): "investigate" alone is Bugfix when a tier-2
        // bugfix word appears anywhere in the task ("Investigate why the
        // scheduler crashes"), else Spike ("Investigate whether we can drop
        // the tokio dependency").
        return Some(
            if tokens.iter().any(|token| BUGFIX_ANYWHERE.contains(token)) {
                Intent::Bugfix
            } else {
                Intent::Spike
            },
        );
    }
    if SPIKE_LEAD.contains(&lead) {
        return Some(Intent::Spike);
    }
    if REVIEW_LEAD.contains(&lead) {
        return Some(Intent::Review);
    }
    if FEATURE_LEAD.contains(&lead) {
        // A nearby fix term changes a feature to Bugfix only at task end or before for/to/in/on; inspect every term in the window so an earlier nonqualifying one cannot hide a later match.
        let window_end = (start + 4).min(tokens.len());
        let triggers_bugfix = (start + 1..window_end).any(|index| {
            matches!(tokens[index], "fix" | "bugfix" | "hotfix") && {
                let is_last_token = index == tokens.len() - 1;
                let followed_by_preposition = tokens
                    .get(index + 1)
                    .is_some_and(|next| matches!(*next, "for" | "to" | "in" | "on"));
                is_last_token || followed_by_preposition
            }
        });
        if triggers_bugfix {
            return Some(Intent::Bugfix);
        }
        return Some(Intent::Feature);
    }
    None
}

/// Tier 2 of [`infer_intent`]: a whole-word scan anywhere in the task, in a
/// fixed priority order, only reached when tier 1 found no leading verb.
fn tier2_intent(tokens: &[&str]) -> Option<Intent> {
    let has_does_nothing = tokens.windows(2).any(|pair| pair == ["does", "nothing"]);
    let bug_words = || {
        tokens
            .iter()
            .filter(|token| BUGFIX_ANYWHERE.contains(token))
    };
    // A feature request often names its error contract ("an error to stderr", "exit 2",
    // "not an error", "ties broken", "not corrupted", "a fixed amount"); only those uses are not a bug report.
    let is_contract_use = |index: usize| {
        let token = tokens[index];
        let before = |back: usize| index.checked_sub(back).map(|at| tokens[at]);
        if CONTRACT_ERROR_WORDS.contains(&token) {
            let to_stderr = (1..=3).any(|ahead| {
                tokens
                    .get(index + ahead)
                    .is_some_and(|t| matches!(*t, "stderr" | "exit"))
            });
            let negated =
                matches!(before(1), Some("a" | "an")) && matches!(before(2), Some("not" | "as"));
            return to_stderr || negated;
        }
        (token == "fixed" && matches!(before(1), Some("a" | "an")))
            || (token == "broken" && before(1) == Some("ties"))
            || (matches!(token, "corrupt" | "corrupted") && before(1) == Some("not"))
    };
    let only_contract_words = tokens
        .iter()
        .enumerate()
        .filter(|(_, token)| BUGFIX_ANYWHERE.contains(token))
        .all(|(index, _)| is_contract_use(index));
    let asks_for_feature = tokens.iter().enumerate().any(|(index, token)| {
        FEATURE_ANYWHERE.contains(token)
            && !(index > 0 && NEGATED_FEATURE_PRECEDERS.contains(&tokens[index - 1]))
    });
    if has_does_nothing || bug_words().next().is_some() {
        return Some(
            if only_contract_words && !has_does_nothing && asks_for_feature {
                Intent::Feature
            } else {
                Intent::Bugfix
            },
        );
    }
    let has_clean_up = tokens.windows(2).any(|pair| pair == ["clean", "up"]);
    let has_dead_code = tokens.windows(2).any(|pair| pair == ["dead", "code"]);
    if has_clean_up || has_dead_code || tokens.iter().any(|token| REFACTOR_ANYWHERE.contains(token))
    {
        return Some(Intent::Refactor);
    }
    let has_proof_of_concept = tokens
        .windows(3)
        .any(|triple| triple == ["proof", "of", "concept"]);
    let has_try_out = tokens.windows(2).any(|pair| pair == ["try", "out"]);
    if has_proof_of_concept
        || has_try_out
        || tokens.iter().any(|token| SPIKE_ANYWHERE.contains(token))
    {
        return Some(Intent::Spike);
    }
    if tokens.iter().any(|token| REVIEW_ANYWHERE.contains(token)) {
        return Some(Intent::Review);
    }
    if tokens.iter().any(|token| FEATURE_ANYWHERE.contains(token)) {
        return Some(Intent::Feature);
    }
    None
}

/// Classify whole-word intent tokens by leading verb, then fixed-priority fallback; substrings and filler-only tasks do not match.
fn infer_intent(task: &str) -> Intent {
    let tokens = word_tokens_ordered(task);

    if let Some(start) = tokens
        .iter()
        .position(|token| !LEADING_FILLER.contains(token))
        && let Some(intent) = leading_intent(&tokens, start)
    {
        return intent;
    }

    tier2_intent(&tokens).unwrap_or(Intent::Other)
}

fn infer_complexity(files: usize, lines: usize, paths: &[PathBuf]) -> Complexity {
    let architectural_path = paths.iter().any(|path| {
        let value = path.to_string_lossy().to_ascii_lowercase();
        value.contains("architecture") || value.contains("migration")
    });
    if architectural_path || files > 15 || lines > 800 {
        Complexity::Architectural
    } else if files > 5 || lines > 250 {
        Complexity::Substantial
    } else if files > 2 || lines > 30 {
        Complexity::Bounded
    } else {
        Complexity::Trivial
    }
}

fn band_for_score(score: u16) -> RiskBand {
    match score {
        0..=19 => RiskBand::Low,
        20..=44 => RiskBand::Medium,
        45..=69 => RiskBand::High,
        _ => RiskBand::Critical,
    }
}

/// Exclude `.zirv` bookkeeping from classification; untracked work products must never determine the operator’s work domain.
fn is_zirv_owned_path(path: &Path) -> bool {
    matches!(path.components().next(), Some(std::path::Component::Normal(name)) if name == ".zirv")
}

/// Ignore only `.zirv/work` bookkeeping in review and fingerprints; excluding all `.zirv` would hide real config or script changes. (#229, #232)
pub(crate) fn is_workflow_work_path(path: &Path) -> bool {
    let mut components = path.components();
    matches!(components.next(), Some(std::path::Component::Normal(name)) if name == ".zirv")
        && matches!(components.next(), Some(std::path::Component::Normal(name)) if name == "work")
}

/// Parses `git diff --numstat`'s output into changed paths and total added
/// plus removed lines. Shared by [`git_change_input`] (working tree vs a
/// base) and [`git_change_input_for_branch`] (one named branch vs its own
/// base, as pure refs).
fn numstat_paths_and_lines(
    repo: &Path,
    args: &[&str],
    budget: Duration,
) -> CtxResult<(Vec<PathBuf>, usize)> {
    let output = git_output_within(repo, args, budget)?;
    if !output.status.success() {
        return Err(format!(
            "cannot inspect changed paths: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )
        .into());
    }
    let mut paths = Vec::new();
    let mut lines = 0usize;
    for line in String::from_utf8_lossy(&output.stdout).lines() {
        let mut fields = line.splitn(3, '\t');
        let added = fields
            .next()
            .and_then(|value| value.parse().ok())
            .unwrap_or(0);
        let removed = fields
            .next()
            .and_then(|value| value.parse().ok())
            .unwrap_or(0);
        let Some(path) = fields.next() else { continue };
        lines = lines.saturating_add(added).saturating_add(removed);
        paths.push(PathBuf::from(path));
    }
    Ok((paths, lines))
}

pub fn git_change_input(repo: &Path, task: String) -> CtxResult<ClassificationInput> {
    git_change_input_with(repo, task, true, GIT_MEASURE_BUDGET)
}

/// Upper bound on each git call classification makes. A checkout whose HEAD
/// is far from the base (thousands of committed build artifacts) can make
/// `git diff --numstat` run for 30s+, past Claude Code's hook timeout (#878).
const GIT_MEASURE_BUDGET: Duration = Duration::from_secs(5);

/// Runs `git -C repo args...` bounded by `budget`; a timeout surfaces as an
/// `io::ErrorKind::TimedOut` error (see [`is_git_timeout`]).
fn git_output_within(
    repo: &Path,
    args: &[&str],
    budget: Duration,
) -> CtxResult<std::process::Output> {
    let mut command = Command::new("git");
    command.arg("-C").arg(repo).args(args);
    Ok(crate::utils::run_with_timeout(&mut command, budget)?)
}

fn is_git_timeout(error: &(dyn std::error::Error + 'static)) -> bool {
    error
        .downcast_ref::<std::io::Error>()
        .is_some_and(|error| error.kind() == std::io::ErrorKind::TimedOut)
}

fn git_timeout_reason(budget: Duration) -> String {
    format!(
        "git measurement timed out after {}s; classified from the task text alone",
        budget.as_secs_f32()
    )
}

/// `git_change_input`, optionally leaving out untracked files: a workflow
/// that has not started yet has not written them, so they are noise.
fn git_change_input_with(
    repo: &Path,
    task: String,
    include_untracked: bool,
    budget: Duration,
) -> CtxResult<ClassificationInput> {
    // Measure against the same merge base as review so committed branch changes remain visible to classification.
    let base = super::review::default_base(repo)?;
    let (mut paths, mut lines) =
        numstat_paths_and_lines(repo, &["diff", "--numstat", &base], budget)?;
    let untracked_stdout = if include_untracked {
        let untracked = git_output_within(
            repo,
            &["ls-files", "--others", "--exclude-standard"],
            budget,
        )?;
        if !untracked.status.success() {
            return Err(format!(
                "cannot inspect untracked paths: {}",
                String::from_utf8_lossy(&untracked.stderr).trim()
            )
            .into());
        }
        untracked.stdout
    } else {
        Vec::new()
    };
    for path in String::from_utf8_lossy(&untracked_stdout)
        .lines()
        .filter(|line| !line.is_empty())
        .map(PathBuf::from)
        .filter(|path| !is_zirv_owned_path(path))
    {
        let absolute = repo.join(&path);
        if let Ok(metadata) = std::fs::symlink_metadata(&absolute)
            && metadata.is_file()
            && !metadata.file_type().is_symlink()
        {
            let mut sample = Vec::new();
            std::fs::File::open(&absolute)?
                .take(1024 * 1024)
                .read_to_end(&mut sample)?;
            let sampled_lines = sample.iter().filter(|byte| **byte == b'\n').count()
                + usize::from(!sample.is_empty() && sample.last() != Some(&b'\n'));
            let size_estimate = usize::try_from(metadata.len() / 80)
                .unwrap_or(usize::MAX)
                .saturating_add(1);
            lines = lines.saturating_add(sampled_lines.max(size_estimate).min(10_000));
        }
        paths.push(path);
    }
    paths.sort();
    paths.dedup();
    let tests_changed = paths.iter().any(|path| {
        let value = path.to_string_lossy().to_ascii_lowercase();
        value.contains("test") || value.contains("spec")
    });
    Ok(ClassificationInput {
        task,
        paths,
        changed_lines: lines,
        tests_changed,
        intent_override: None,
        complexity_override: None,
        risk_override: None,
    })
}

/// Diff the named branch against its base as refs because `repo` need not have it checked out. There is no worktree to scan for untracked or uncommitted content. (#467)
pub fn git_change_input_for_branch(
    repo: &Path,
    branch: &str,
    task: String,
) -> CtxResult<ClassificationInput> {
    let base = super::review::default_base_for(repo, branch)?;
    let (mut paths, lines) = numstat_paths_and_lines(
        repo,
        &["diff", "--numstat", &base, branch],
        GIT_MEASURE_BUDGET,
    )?;
    paths.sort();
    paths.dedup();
    let tests_changed = paths.iter().any(|path| {
        let value = path.to_string_lossy().to_ascii_lowercase();
        value.contains("test") || value.contains("spec")
    });
    Ok(ClassificationInput {
        task,
        paths,
        changed_lines: lines,
        tests_changed,
        intent_override: None,
        complexity_override: None,
        risk_override: None,
    })
}

#[derive(Debug, Args)]
pub struct ClassifyArgs {
    /// Task summary used for deterministic intent inference.
    #[arg(long, default_value = "")]
    pub task: String,
    /// Changed path; repeat to supply the change surface explicitly.
    #[arg(long = "path")]
    pub paths: Vec<PathBuf>,
    /// Total added plus removed lines for explicit inputs.
    #[arg(long)]
    pub changed_lines: Option<usize>,
    /// Declare that the change includes a test/spec path.
    #[arg(long)]
    pub tests_changed: bool,
    #[arg(long, value_enum)]
    pub intent: Option<Intent>,
    #[arg(long, value_enum)]
    pub complexity: Option<Complexity>,
    #[arg(long, value_enum)]
    pub risk: Option<RiskBand>,
    #[arg(long)]
    pub repo: Option<PathBuf>,
    /// Diff this branch against its own base as refs, instead of `--repo`'s
    /// working tree (issue #467: classifying a branch `--repo` does not
    /// have checked out).
    #[arg(long)]
    pub branch: Option<String>,
    #[arg(long)]
    pub json: bool,
}

/// `git_change_input`, or its branch-scoped sibling when `--branch` was
/// given -- the one seam both of `from_args`'s two measurement points
/// (the undeclared path, and the declared-input measured floor below) share.
fn measured_input(
    repo: &Path,
    branch: Option<&str>,
    task: String,
    include_untracked: bool,
    budget: Duration,
) -> CtxResult<ClassificationInput> {
    match branch {
        Some(branch) => git_change_input_for_branch(repo, branch, task),
        None => git_change_input_with(repo, task, include_untracked, budget),
    }
}

pub fn from_args(args: &ClassifyArgs) -> CtxResult<Classification> {
    classify_args(args, true)
}

/// `from_args` for team planning: like `workflow start`, planning precedes the work, so untracked files are not part of the change.
pub fn from_plan_args(args: &ClassifyArgs) -> CtxResult<Classification> {
    classify_args(args, false)
}

/// `from_args` for `zirv workflow start`: nothing has been written yet, so
/// untracked files are not part of the change, and the task text can only
/// raise complexity (an explicit `--complexity` stands).
pub fn from_start_args(args: &ClassifyArgs) -> CtxResult<Classification> {
    let mut classification = classify_args(args, false)?;
    let text_only = crate::commands::ctx::proxy::decision::try_classify_request(&args.task);
    if let Some(text_only) = text_only
        && args.complexity.is_none()
        && text_only.complexity > classification.complexity
    {
        classification.complexity = text_only.complexity;
        classification
            .reasons
            .push(format!("task text complexity: {:?}", text_only.complexity));
        classification.reasons.sort();
    }
    Ok(classification)
}

fn classify_args(args: &ClassifyArgs, include_untracked: bool) -> CtxResult<Classification> {
    classify_args_within(args, include_untracked, GIT_MEASURE_BUDGET)
}

/// `classify_args` with an explicit per-git-call `budget`. A measurement that
/// times out never fails or hangs classification: it falls back to the task
/// text with no measured paths and records the measurement as unavailable.
fn classify_args_within(
    args: &ClassifyArgs,
    include_untracked: bool,
    budget: Duration,
) -> CtxResult<Classification> {
    let repo = args.repo.clone().unwrap_or(std::env::current_dir()?);
    let declared = !args.paths.is_empty() || args.changed_lines.is_some();
    let mut timed_out = None;
    let mut input = if declared {
        ClassificationInput {
            task: args.task.clone(),
            paths: args.paths.clone(),
            changed_lines: args.changed_lines.unwrap_or(0),
            tests_changed: args.tests_changed,
            intent_override: None,
            complexity_override: None,
            risk_override: None,
        }
    } else {
        match measured_input(
            &repo,
            args.branch.as_deref(),
            args.task.clone(),
            include_untracked,
            budget,
        ) {
            Ok(input) => input,
            Err(error) if is_git_timeout(error.as_ref()) => {
                timed_out = Some(git_timeout_reason(budget));
                ClassificationInput {
                    task: args.task.clone(),
                    paths: Vec::new(),
                    changed_lines: 0,
                    tests_changed: false,
                    intent_override: None,
                    complexity_override: None,
                    risk_override: None,
                }
            }
            Err(error) => return Err(error),
        }
    };
    input.intent_override = args.intent;
    input.complexity_override = args.complexity;
    input.risk_override = args.risk;
    let mut classification = classify(&input)?;
    if let Some(reason) = timed_out {
        mark_unavailable(&mut classification, reason);
        return Ok(classification);
    }
    if !declared {
        return Ok(classification);
    }
    classification.declared_scope = true;
    // Combine declared and Git-measured risk at the higher band; if Git cannot be measured, record that state and raise risk one step so declarations cannot bypass review.
    let mut measured = match measured_input(
        &repo,
        args.branch.as_deref(),
        args.task.clone(),
        include_untracked,
        budget,
    ) {
        Ok(measured) => measured,
        Err(error) => {
            let reason = if is_git_timeout(error.as_ref()) {
                git_timeout_reason(budget)
            } else {
                "git measurement unavailable (not a repository, or no commits)".to_string()
            };
            mark_unavailable(&mut classification, reason);
            return Ok(classification);
        }
    };
    measured.intent_override = args.intent;
    measured.complexity_override = args.complexity;
    let measured = classify(&measured)?;
    let mut raised = false;
    if measured.risk > classification.risk {
        classification
            .reasons
            .push(format!("measured-tree risk floor: {:?}", measured.risk));
        classification.risk = measured.risk;
        classification.risk_score = classification.risk_score.max(measured.risk_score);
        raised = true;
    }
    // Complexity as well as risk. Complexity selects the plan step on its own
    // (and the design gate together with risk), so a declared `--path
    // README.md` over substantial real work dropped planning even where the
    // risk band was unaffected. An explicit `--complexity` is the operator
    // speaking and stands.
    if args.complexity.is_none() && measured.complexity > classification.complexity {
        classification.reasons.push(format!(
            "measured-tree complexity: {:?}",
            measured.complexity
        ));
        classification.complexity = measured.complexity;
        raised = true;
    }
    if measured.work_domain.score > classification.work_domain.score {
        classification.work_domain = measured.work_domain;
        raised = true;
    }
    if !raised {
        classification
            .reasons
            .push("declared change scope".to_string());
    }
    classification.reasons.sort();
    Ok(classification)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input(paths: &[&str], lines: usize) -> ClassificationInput {
        ClassificationInput {
            task: "implement feature".to_string(),
            paths: paths.iter().map(PathBuf::from).collect(),
            changed_lines: lines,
            tests_changed: false,
            intent_override: None,
            complexity_override: None,
            risk_override: None,
        }
    }

    #[test]
    fn identical_inputs_produce_identical_classification() {
        let value = input(&["src/lib.rs"], 12);
        assert_eq!(classify(&value).unwrap(), classify(&value).unwrap());
    }

    /// `infer_intent` matches WHOLE words only, never a substring, and reads
    /// the task's leading verb before falling back to a scan of the rest of
    /// the words. Table-driven over the cases the old substring matcher got
    /// wrong plus the tiering rules themselves.
    #[test]
    fn infer_intent_matches_whole_words_leading_verb_first() {
        let cases: &[(&str, Intent)] = &[
            // Substring traps the old `contains` matcher fell into.
            ("Add a prefix option to the log formatter", Intent::Feature),
            ("Add a suffix to generated file names", Intent::Feature),
            ("Add test fixtures for the parser", Intent::Feature),
            ("Address the duplication in the adapters", Intent::Refactor),
            ("padding looks off", Intent::Other),
            ("Add an explorer view for files", Intent::Feature),
            ("Implement a debug flag", Intent::Feature),
            // Leading verb wins over a later keyword.
            ("add a review command to the CLI", Intent::Feature),
            ("review the fix for the bug", Intent::Review),
            ("refactor the bug tracker", Intent::Refactor),
            ("fix the refactor that broke imports", Intent::Bugfix),
            // Filler skipping.
            ("Please add a --verbose flag", Intent::Feature),
            ("We need to implement SSO login", Intent::Feature),
            // Conventional-commit prefixes.
            ("feat: add shell completions", Intent::Feature),
            ("fix(ctx): stop repeated permission prompts", Intent::Bugfix),
            ("BUGFIX: wrong timezone in reports", Intent::Bugfix),
            // Symptom-only reports, no leading verb at all.
            ("the build fails on windows", Intent::Bugfix),
            ("login is broken on Safari", Intent::Bugfix),
            // A feature lead immediately followed by "fix".
            ("Implement a fix for the login crash", Intent::Bugfix),
            // Both `investigate` branches.
            (
                "Investigate why the scheduler crashes on startup",
                Intent::Bugfix,
            ),
            (
                "Investigate whether we can drop the tokio dependency",
                Intent::Spike,
            ),
            // A follow-up message opens with "also"/"then"; the real lead verb follows.
            ("Also investigate how other people use Jev", Intent::Spike),
            ("Analyse zirv response quality", Intent::Spike),
            ("Also fix the login crash", Intent::Bugfix),
            ("Also add a flag", Intent::Feature),
            ("Then analyze the slow startup path", Intent::Spike),
            // "clean up" bigram.
            ("Clean up the adapters module", Intent::Refactor),
            // Adjustment (a), narrowed: a feature lead's nearby fix/bugfix/
            // hotfix word only flips to Bugfix when it is the task's last
            // token or is directly followed by for/to/in/on -- a bare noun
            // mention stays Feature.
            ("Add a bugfix changelog section", Intent::Feature),
            ("Add a hotfix for the login crash", Intent::Bugfix),
            // Review finding: every fix/bugfix/hotfix token in the
            // window is checked, not just the first -- the leading "fix"
            // alone doesn't qualify (next token "hotfix" isn't a
            // preposition), but "hotfix" does (followed by "for").
            ("Implement a fix/hotfix for startup", Intent::Bugfix),
            // "try" is deliberately NOT a lead word -- it falls through to
            // tier 2, where "fix" still wins.
            ("Try to fix the login bug", Intent::Bugfix),
            // "try out" bigram (tier 2 only).
            ("Try out the new clap derive API", Intent::Spike),
            // Bugfix tier-2 symptom words and the "does nothing" phrase.
            ("Null pointer exception when saving a draft", Intent::Bugfix),
            ("The export button does nothing", Intent::Bugfix),
            ("Memory leak in the websocket handler", Intent::Bugfix),
            ("Wrong total shown in the cart", Intent::Bugfix),
            ("Patch the off-by-one in pagination", Intent::Bugfix),
            // "dead code" bigram (refactor tier 2).
            ("Remove dead code from the scheduler", Intent::Refactor),
            // "feasible" (spike tier 2).
            ("Is it feasible to stream tool output?", Intent::Spike),
            // No signal at all.
            ("", Intent::Other),
        ];
        for (task, expected) in cases {
            let lowered = task.to_ascii_lowercase();
            assert_eq!(
                infer_intent(&lowered),
                *expected,
                "task {task:?} should classify as {expected:?}"
            );
        }
    }

    /// Benchmark finding: a feature request that specifies its error contract
    /// ("print an error", "ties broken", "not corrupted") is a Feature, while a
    /// report of broken behaviour stays a Bugfix.
    #[test]
    fn a_feature_request_with_an_error_contract_is_a_feature() {
        let cases: &[(&str, Intent)] = &[
            (
                "The ledger needs a way to export. Please add an export command. An unknown format prints an error to stderr and exits 2.",
                Intent::Feature,
            ),
            (
                "I'd like tags on transactions. Add a tagging module; a blank tag prints an error to stderr, and a malformed store is rejected.",
                Intent::Feature,
            ),
            (
                "Add an export command; values with commas are quoted correctly, not corrupted.",
                Intent::Feature,
            ),
            (
                "Add a goals list command sorted by percent complete, ties broken alphabetically by name.",
                Intent::Feature,
            ),
            (
                "Add a monthly report; a missing flag is a usage error on stderr, exit 2. Please add tests.",
                Intent::Feature,
            ),
            (
                "Add envelopes that each budget a fixed amount per month. Rejects (error to stderr, exit 2) an unknown name.",
                Intent::Feature,
            ),
            (
                "Silently skipped months are fine, not an error. Please add a monthly report.",
                Intent::Feature,
            ),
            // An indicative verb in a symptom report reads like a spec but is a bug.
            (
                "The API returns an error when the cart is empty. Add a guard.",
                Intent::Bugfix,
            ),
            (
                "The parser throws an exception if the input has a BOM, add handling for it",
                Intent::Bugfix,
            ),
            (
                "The CLI exits with an error. Add a --force flag so it can proceed",
                Intent::Bugfix,
            ),
            (
                "It returns an error when the user has no email; add a default",
                Intent::Bugfix,
            ),
            (
                "The import raises an exception on blank rows. Please add skipping of blank rows.",
                Intent::Bugfix,
            ),
            (
                "The job exits with an exception when disk is full, add a retry",
                Intent::Bugfix,
            ),
            (
                "When I click save it prints an error. Add the missing handler",
                Intent::Bugfix,
            ),
            (
                "The service emits an error log on startup, please add the missing config",
                Intent::Bugfix,
            ),
            // Symptom words stay bug words even when the report also asks to add something.
            (
                "Login fails with an error when the password is empty. Add a check for it.",
                Intent::Bugfix,
            ),
            (
                "Sorting is broken for dates. Add a comparator that handles timezones.",
                Intent::Bugfix,
            ),
            (
                "The CSV export is incorrect when a field contains a comma; it should be quoted. Add quoting.",
                Intent::Bugfix,
            ),
            (
                "The dashboard shows the wrong total after a refund. Please add the refund amount to it.",
                Intent::Bugfix,
            ),
            (
                "Users cannot log in. Add a fallback to the old auth path",
                Intent::Bugfix,
            ),
            (
                "Users get an error when they add an item to the cart",
                Intent::Bugfix,
            ),
            (
                "Intermittent failures in the nightly job; add logging to find out why",
                Intent::Bugfix,
            ),
            (
                "The tool cannot parse quoted fields. Add support.",
                Intent::Bugfix,
            ),
            (
                "Users cannot add items to the cart and get a wrong total",
                Intent::Bugfix,
            ),
            (
                "Something is off with the pagination. Can you find and fix whatever is causing this? Please add a test.",
                Intent::Bugfix,
            ),
            (
                "The import crashes on 1,234.50 and gives an error. Please add support for thousands separators.",
                Intent::Bugfix,
            ),
            (
                "tests/test_rules.py::test_regex is failing on this repo, please find the root cause",
                Intent::Bugfix,
            ),
        ];
        for (task, expected) in cases {
            assert_eq!(
                infer_intent(&task.to_ascii_lowercase()),
                *expected,
                "task {task:?} should classify as {expected:?}"
            );
        }
    }

    /// Issue #541 chunk C, decision 3: the team compiler needs the REAL
    /// changed-path list to split implementer seats by claim boundary, not
    /// just a count -- `changed_files` alone (a count) cannot do that.
    #[test]
    fn classification_keeps_the_changed_paths() {
        let value = input(&["src/A.rs", "docs/readme.md"], 10);
        let classification = classify(&value).unwrap();
        assert_eq!(classification.changed_files, 2);
        // Lowercased and forward-slashed, matching the path signal matching
        // the rest of this function already does -- one normalized form, not
        // a second one only this field uses.
        assert_eq!(
            classification.changed_paths,
            vec!["src/a.rs".to_string(), "docs/readme.md".to_string()]
        );
    }

    /// A huge diff never inflates a durable classification or a persisted
    /// `TeamPlan`: `changed_paths` is capped at [`MAX_CHANGED_PATHS`] while
    /// `changed_files` keeps the true total.
    #[test]
    fn changed_paths_is_bounded_but_changed_files_keeps_the_true_total() {
        let paths: Vec<String> = (0..(MAX_CHANGED_PATHS + 20))
            .map(|n| format!("src/file{n}.rs"))
            .collect();
        let refs: Vec<&str> = paths.iter().map(String::as_str).collect();
        let value = input(&refs, 10);
        let classification = classify(&value).unwrap();
        assert_eq!(classification.changed_files, MAX_CHANGED_PATHS + 20);
        assert_eq!(classification.changed_paths.len(), MAX_CHANGED_PATHS);
    }

    #[test]
    fn sensitive_paths_raise_risk_and_cannot_be_downgraded() {
        let mut value = input(&["src/auth/permissions.rs"], 20);
        let classification = classify(&value).unwrap();
        assert!(classification.risk >= RiskBand::High);
        value.risk_override = Some(RiskBand::Low);
        assert!(
            classify(&value)
                .unwrap_err()
                .to_string()
                .contains("High floor")
        );
    }

    #[test]
    fn trivial_change_takes_the_low_risk_fast_path() {
        let mut value = input(&["README.md"], 5);
        value.tests_changed = true;
        let classification = classify(&value).unwrap();
        assert_eq!(classification.complexity, Complexity::Trivial);
        assert_eq!(classification.risk, RiskBand::Low);
    }

    /// A committed repository with `file` in the working tree but not in
    /// `HEAD`, so a Git measurement has something real to see.
    fn repo_with_pending_file(file: &str) -> tempfile::TempDir {
        let repo = tempfile::tempdir().expect("tempdir");
        let git = |args: &[&str]| {
            let status = Command::new("git")
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
        std::fs::write(repo.path().join("README.md"), "readme\n").expect("write");
        git(&["add", "."]);
        git(&["commit", "-q", "-m", "base"]);
        let path = repo.path().join(file);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("create parent");
        }
        std::fs::write(&path, "fn check_password() {}\n").expect("write");
        repo
    }

    #[test]
    fn declared_inputs_cannot_talk_a_measured_sensitive_surface_down() {
        let repo = repo_with_pending_file("src/auth/session.rs");
        let args = ClassifyArgs {
            task: "implement feature".into(),
            paths: vec![PathBuf::from("README.md")],
            changed_lines: Some(2),
            tests_changed: true,
            intent: None,
            complexity: None,
            risk: None,
            branch: None,
            repo: Some(repo.path().to_path_buf()),
            json: false,
        };
        let declared_only = classify(&ClassificationInput {
            task: args.task.clone(),
            paths: args.paths.clone(),
            changed_lines: 2,
            tests_changed: true,
            intent_override: None,
            complexity_override: None,
            risk_override: None,
        })
        .unwrap();
        assert_eq!(declared_only.risk, RiskBand::Low);

        let classification = from_args(&args).unwrap();
        assert!(classification.declared_scope);
        assert!(
            classification.risk >= RiskBand::High,
            "the measured auth surface must floor the declared band: {classification:?}"
        );
        assert!(
            classification
                .reasons
                .iter()
                .any(|reason| reason.contains("measured-tree risk floor"))
        );
    }

    fn start_args(repo: &Path, task: &str) -> ClassifyArgs {
        ClassifyArgs {
            task: task.into(),
            paths: Vec::new(),
            changed_lines: None,
            tests_changed: false,
            intent: None,
            complexity: None,
            risk: None,
            repo: Some(repo.to_path_buf()),
            branch: None,
            json: false,
        }
    }

    /// #878: a git measurement that outlives its budget must not fail or hang
    /// classification -- it falls back to the task text and records the
    /// measurement as unavailable with a timeout reason.
    #[test]
    fn start_classification_degrades_when_git_measurement_times_out() {
        let repo = repo_with_pending_file("src/auth/session.rs");
        let classification = classify_args_within(
            &start_args(repo.path(), "add a flag"),
            false,
            Duration::ZERO,
        )
        .expect("a timed-out measurement must not fail classification");
        match classification.risk_measurement {
            RiskMeasurement::Unavailable { reason } => {
                assert!(reason.contains("timed out"), "reason: {reason}");
            }
            other => panic!("expected Unavailable, got {other:?}"),
        }
    }

    /// A start has written nothing yet: untracked root-level files are noise,
    /// not a change surface, and must not inflate risk or complexity.
    #[test]
    fn start_classification_ignores_untracked_files() {
        let repo = repo_with_pending_file("src/auth/session.rs");
        for index in 0..8 {
            std::fs::write(repo.path().join(format!("extra-{index}.rs")), "x\n").unwrap();
        }
        let args = start_args(repo.path(), "fix the dashboard");

        let at_start = from_start_args(&args).unwrap();
        assert_eq!(at_start.complexity, Complexity::Trivial, "{at_start:?}");
        assert!(at_start.changed_paths.is_empty(), "{at_start:?}");
        let measured = from_args(&args).unwrap();
        assert!(measured.complexity > Complexity::Trivial, "{measured:?}");
    }

    /// With no diff yet, a long enumerated task is still sized by its text.
    #[test]
    fn start_classification_takes_the_heavier_of_task_text_and_tracked_diff() {
        let repo = repo_with_pending_file("src/lib.rs");
        let task = (1..=8)
            .map(|n| format!("- fix issue {n}"))
            .collect::<Vec<_>>()
            .join("\n");

        let classification = from_start_args(&start_args(repo.path(), &task)).unwrap();
        assert_eq!(classification.complexity, Complexity::Substantial);

        let mut explicit = start_args(repo.path(), &task);
        explicit.complexity = Some(Complexity::Trivial);
        let classification = from_start_args(&explicit).unwrap();
        assert_eq!(classification.complexity, Complexity::Trivial);
    }

    /// Risk is not the only band a declared scope could talk down: complexity
    /// selects the plan step on its own, so a declared one-file scope over
    /// substantial real work dropped planning even where the risk band was
    /// unaffected.
    #[test]
    fn declared_inputs_cannot_talk_a_measured_complexity_down() {
        let repo = repo_with_pending_file("src/one.rs");
        for index in 0..8 {
            std::fs::write(
                repo.path().join(format!("src/extra-{index}.rs")),
                "fn extra() {}\n",
            )
            .unwrap();
        }
        let classification = from_args(&ClassifyArgs {
            task: "implement feature".into(),
            paths: vec![PathBuf::from("README.md")],
            changed_lines: Some(2),
            tests_changed: true,
            intent: None,
            complexity: None,
            risk: None,
            branch: None,
            repo: Some(repo.path().to_path_buf()),
            json: false,
        })
        .unwrap();
        assert!(
            classification.complexity >= Complexity::Substantial,
            "the measured tree is substantial: {classification:?}"
        );
        assert!(
            classification
                .reasons
                .iter()
                .any(|reason| reason.contains("measured-tree complexity"))
        );
    }

    /// #88: outside a git repository the old safety net silently kept
    /// whatever band the declared inputs alone produced. It must now report
    /// the unmeasured state and escalate the band one step rather than trust
    /// the declaration.
    #[test]
    fn declared_inputs_fail_safe_when_git_is_unavailable_outside_a_repository() {
        let dir = tempfile::tempdir().expect("tempdir");
        let classification = from_args(&ClassifyArgs {
            task: "implement feature".into(),
            paths: vec![PathBuf::from("README.md")],
            changed_lines: Some(2),
            tests_changed: true,
            intent: None,
            complexity: None,
            risk: None,
            branch: None,
            repo: Some(dir.path().to_path_buf()),
            json: false,
        })
        .unwrap();
        assert!(classification.declared_scope);
        assert!(
            matches!(
                classification.risk_measurement,
                RiskMeasurement::Unavailable { .. }
            ),
            "{classification:?}"
        );
        // The declared scope alone (README.md, 2 lines, tests changed) scores
        // Low; fail-safe escalates it one band rather than trusting a
        // declaration the net could not check.
        assert_eq!(classification.risk, RiskBand::Medium);
        assert!(
            classification
                .reasons
                .iter()
                .any(|reason| reason.contains("risk escalated"))
        );
    }

    /// #88: a repository that exists but has no commits fails `git rev-parse
    /// HEAD` the same way a non-repository does, and must fail the same safe
    /// way.
    #[test]
    fn declared_inputs_fail_safe_when_the_repository_has_no_commits() {
        let dir = tempfile::tempdir().expect("tempdir");
        let status = Command::new("git")
            .args(["init", "-q"])
            .current_dir(dir.path())
            .status()
            .expect("git init");
        assert!(status.success());
        let classification = from_args(&ClassifyArgs {
            task: "implement feature".into(),
            paths: vec![PathBuf::from("README.md")],
            changed_lines: Some(2),
            tests_changed: true,
            intent: None,
            complexity: None,
            risk: None,
            branch: None,
            repo: Some(dir.path().to_path_buf()),
            json: false,
        })
        .unwrap();
        assert!(
            matches!(
                classification.risk_measurement,
                RiskMeasurement::Unavailable { .. }
            ),
            "{classification:?}"
        );
        assert_eq!(classification.risk, RiskBand::Medium);
    }

    /// No change to behavior when measurement succeeds: the existing
    /// measured-tree tests already cover the risk-band outcome, this pins
    /// that the new field stays `Measured` alongside them.
    #[test]
    fn risk_measurement_stays_measured_when_git_succeeds() {
        let repo = repo_with_pending_file("src/one.rs");
        let classification = from_args(&ClassifyArgs {
            task: "implement feature".into(),
            paths: vec![PathBuf::from("README.md")],
            changed_lines: Some(2),
            tests_changed: true,
            intent: None,
            complexity: None,
            risk: None,
            branch: None,
            repo: Some(repo.path().to_path_buf()),
            json: false,
        })
        .unwrap();
        assert_eq!(classification.risk_measurement, RiskMeasurement::Measured);
    }

    /// Issue #467, acceptance 3: undeclared (no `--path`/`--changed-lines`)
    /// classification measures whichever repository it is given via `git
    /// diff --numstat <base>` (`git_change_input`) -- so pointing `zirv
    /// workflow start` at a linked `git worktree add` sibling sees that
    /// worktree's own branch diff against its base, not an empty diff off
    /// the main checkout it shares a `.git` with (which never touched the
    /// feature branch's files at all).
    #[test]
    fn git_change_input_sees_a_linked_worktrees_branch_diff_against_its_base() {
        let main_repo = tempfile::tempdir().expect("tempdir");
        let git = |dir: &std::path::Path, args: &[&str]| {
            let status = Command::new("git")
                .args([
                    "-c",
                    "user.email=t@example.com",
                    "-c",
                    "user.name=t",
                    "-c",
                    "commit.gpgsign=false",
                ])
                .args(args)
                .current_dir(dir)
                .status()
                .expect("run git");
            assert!(status.success(), "git {args:?} failed");
        };
        git(main_repo.path(), &["init", "-q"]);
        std::fs::write(main_repo.path().join("README.md"), "readme\n").expect("write");
        git(main_repo.path(), &["add", "."]);
        git(main_repo.path(), &["commit", "-q", "-m", "base"]);

        let worktree_dir = tempfile::tempdir().expect("tempdir");
        let worktree_path = worktree_dir.path().to_path_buf();
        std::fs::remove_dir(&worktree_path).expect("remove placeholder dir");
        git(
            main_repo.path(),
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                "feature",
                worktree_path.to_str().expect("utf-8 path"),
            ],
        );
        for index in 0..8 {
            std::fs::write(
                worktree_path.join(format!("src-{index}.rs")),
                "fn work() {}\n",
            )
            .unwrap();
        }
        git(&worktree_path, &["add", "."]);
        git(&worktree_path, &["commit", "-q", "-m", "feature work"]);

        // The main checkout was never touched after "base": its own diff
        // against its own resolvable history is empty.
        let from_main = git_change_input(main_repo.path(), "small feature".into()).unwrap();
        assert!(
            from_main.paths.is_empty(),
            "the main checkout was never touched: {from_main:?}"
        );

        // The worktree's diff against the shared base is real, even though
        // it shares its `.git` common dir with the (clean) main checkout.
        let from_worktree = git_change_input(&worktree_path, "small feature".into()).unwrap();
        assert_eq!(from_worktree.paths.len(), 8, "{from_worktree:?}");

        let measured = classify(&from_worktree).unwrap();
        assert!(measured.complexity > Complexity::Trivial, "{measured:?}");
    }

    #[test]
    fn classification_task_is_bounded() {
        let mut value = input(&["README.md"], 5);
        value.task = "x".repeat(MAX_TASK_BYTES + 1);
        assert!(
            classify(&value)
                .unwrap_err()
                .to_string()
                .contains("exceeds")
        );
    }

    #[test]
    fn frontend_domain_is_inferred_from_task_and_a_frontend_path_without_an_init_flag() {
        // #255: task text alone is capped below the selection threshold, so
        // this now needs one real frontend path signal alongside it.
        let classification = classify(&ClassificationInput {
            task: "Build a responsive billing dashboard UI".into(),
            paths: vec![PathBuf::from("src/dashboard/Billing.tsx")],
            changed_lines: 12,
            tests_changed: true,
            intent_override: None,
            complexity_override: None,
            risk_override: None,
        })
        .expect("classification");

        assert_eq!(classification.work_domain.domain, WorkDomain::Frontend);
        assert!(classification.work_domain.score >= 45);
    }

    /// #255 repro: a task that only *mentions* "frontend" in passing (here,
    /// documenting a permission family named after it) must not select the
    /// Frontend methodology when the actual changed paths are not frontend
    /// surfaces at all.
    #[test]
    fn task_text_mentioning_frontend_without_a_frontend_path_stays_general() {
        let classification = classify(&ClassificationInput {
            task: "Document the zirv frontend permission family boundaries".into(),
            paths: vec![PathBuf::from("src/commands/ctx/safety.rs")],
            changed_lines: 40,
            tests_changed: true,
            intent_override: None,
            complexity_override: None,
            risk_override: None,
        })
        .expect("classification");

        assert_eq!(classification.work_domain.domain, WorkDomain::General);
    }

    #[test]
    fn frontend_domain_is_inferred_from_changed_file_types() {
        let classification = classify(&ClassificationInput {
            task: "Adjust the settings experience".into(),
            paths: vec![PathBuf::from("src/settings/Panel.tsx")],
            changed_lines: 12,
            tests_changed: true,
            intent_override: None,
            complexity_override: None,
            risk_override: None,
        })
        .expect("classification");

        assert_eq!(classification.work_domain.domain, WorkDomain::Frontend);
    }

    #[test]
    fn mixed_monorepository_backend_only_work_does_not_select_frontend_methodology() {
        let classification = classify(&ClassificationInput {
            task: "Fix database retry handling in the API service".into(),
            paths: vec![
                PathBuf::from("services/api/src/retry.rs"),
                PathBuf::from("services/api/tests/retry.rs"),
            ],
            changed_lines: 24,
            tests_changed: true,
            intent_override: None,
            complexity_override: None,
            risk_override: None,
        })
        .expect("classification");

        assert_eq!(classification.work_domain.domain, WorkDomain::General);
    }

    #[test]
    fn is_zirv_owned_path_matches_only_a_leading_zirv_component() {
        assert!(is_zirv_owned_path(Path::new(".zirv/work/abc/mock.html")));
        assert!(is_zirv_owned_path(Path::new(".zirv/ctx.toml")));
        assert!(!is_zirv_owned_path(Path::new("src/x.tsx")));
        assert!(!is_zirv_owned_path(Path::new("docs/.zirv/notes.md")));
    }

    /// #229/#232: narrower than `is_zirv_owned_path` -- only the workflow's
    /// own `.zirv/work/**` bookkeeping must be excluded from a review
    /// package or its staleness fingerprint, not sibling `.zirv/` config
    /// like `.zirv/ctx.toml` or `.zirv/commands/*`, which are real
    /// repository content a reviewer needs to see.
    #[test]
    fn is_workflow_work_path_matches_only_zirv_work_not_other_zirv_state() {
        assert!(is_workflow_work_path(Path::new(".zirv/work/abc/plan.md")));
        assert!(is_workflow_work_path(Path::new(".zirv/work")));
        assert!(!is_workflow_work_path(Path::new(".zirv/ctx.toml")));
        assert!(!is_workflow_work_path(Path::new(
            ".zirv/commands/build.yaml"
        )));
        assert!(!is_workflow_work_path(Path::new("src/x.tsx")));
        assert!(!is_workflow_work_path(Path::new(
            "docs/.zirv/work/notes.md"
        )));
    }

    #[test]
    fn git_change_input_ignores_untracked_zirv_state_but_keeps_other_untracked_paths() {
        let repo = tempfile::tempdir().expect("tempdir");
        let git = |args: &[&str]| {
            let status = Command::new("git")
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
        std::fs::write(repo.path().join("README.md"), "readme\n").expect("write");
        git(&["add", "."]);
        git(&["commit", "-q", "-m", "base"]);

        let zirv_work_dir = repo.path().join(".zirv").join("work").join("old-id");
        std::fs::create_dir_all(&zirv_work_dir).expect("create .zirv/work dir");
        std::fs::write(
            zirv_work_dir.join("dash-v3-mock.html"),
            "<html>\n".repeat(50),
        )
        .expect("write stale workflow artifact");

        std::fs::create_dir_all(repo.path().join("src")).expect("create src dir");
        std::fs::write(
            repo.path().join("src").join("x.tsx"),
            "export const X = () => null;\n",
        )
        .expect("write untracked tsx");

        let input = git_change_input(repo.path(), "unrelated change".into()).expect("input");

        assert!(
            !input.paths.iter().any(|path| path.starts_with(".zirv")),
            "expected no .zirv paths in {:?}",
            input.paths
        );
        assert!(
            input
                .paths
                .iter()
                .any(|path| path == Path::new("src/x.tsx")),
            "expected src/x.tsx in {:?}",
            input.paths
        );
        // Only the tsx contributes lines; the stale .zirv/work html must not.
        assert!(
            input.changed_lines < 50,
            "expected .zirv/work content excluded from changed_lines, got {}",
            input.changed_lines
        );
    }
}
