//! Stop-hook scope-creep guard: tracks the prompt's stated constraints and
//! the session's tracked-file baseline to flag unrequested changes.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

use regex::Regex;
use serde::{Deserialize, Serialize};

use super::missing_tests_gate::path_looks_like_test_file;
use super::pretool_guard::normalized_write_target;
use super::pretool_tier::PreToolPayload;
use super::stop_verify::STOP_VERIFY_TAIL_BYTES;
use crate::commands::ctx::adapters::{self, SESSION_ENV};
use crate::commands::ctx::config::{CtxConfig, EnvLookup};
use crate::commands::ctx::event::{NormalizedEvent, input_hash};
use crate::commands::ctx::state::StateDir;

// Surface the request's preservation constraints at edit time and
// check unrequested-fix claims at Stop. Prompt, tool and Stop hooks fail
// open on missing identity, state or configuration.

/// Bump on schema changes so missing fields do not masquerade as an empty
/// baseline and trigger a false shell-edit checkpoint.
const SCOPE_GUARD_RECORD_VERSION: u32 = 3;

/// Bound extracted constraints so a long prompt cannot inflate a hot hook's
/// checkpoint text without limit.
const SCOPE_GUARD_CONSTRAINT_BUDGET: usize = 400;

/// At most this many extracted constraint sentences.
const SCOPE_GUARD_MAX_CONSTRAINTS: usize = 3;

/// At most this many characters of the quoted "unrequested fix" sentence in
/// a Stop block reason.
const SCOPE_GUARD_QUOTE_BUDGET: usize = 200;

/// Per-session scope-guard state, one record per prompt: a new prompt (a
/// different `prompt_hash`) replaces the whole record rather than
/// accumulating. Mirrors `AdoptionRecord`'s own per-session file layout
/// (`state::scope_guard`, keyed the same way `adoption_record_path` is).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
struct ScopeGuardRecord {
    #[serde(default)]
    version: u32,
    /// `input_hash` of the prompt this record was built from.
    #[serde(default)]
    prompt_hash: u64,
    /// The extracted preservation/limitation sentences, in prompt order.
    #[serde(default)]
    constraints: Vec<String>,
    /// Whether the request itself already asks for a fix -- the Stop
    /// backstop never fires when it does: there is no "unrequested" fix to
    /// catch.
    #[serde(default)]
    asks_for_fix: bool,
    /// Whether `PreToolUse` has already shown the checkpoint for this
    /// prompt.
    #[serde(default)]
    checkpoint_shown: bool,
    /// Whether `Stop` has already checked (and possibly blocked) this
    /// prompt.
    #[serde(default)]
    stop_checked: bool,
    /// Baseline of tracked modified/deleted files and cheap fingerprints for
    /// detecting shell edits without retaining file contents.
    #[serde(default)]
    shell_baseline: Vec<ScopeGuardBaselineEntry>,
    /// Distinct checkable details from the request, kept in prompt order for
    /// the checkpoint checklist.
    #[serde(default)]
    stated_details: Vec<String>,
}

/// Size, mtime and existence distinguish tracked-file changes cheaply
/// without reading or hashing file contents.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct ScopeGuardBaselineEntry {
    /// Repo-relative slash-normalized path for portable record round trips.
    #[serde(default)]
    path: String,
    #[serde(default)]
    exists: bool,
    #[serde(default)]
    size: u64,
    /// Nanoseconds since `UNIX_EPOCH`, never whole seconds -- a same-size
    /// rewrite that lands within the same second as the baseline still
    /// changes this value, so it is never mistaken for "unchanged".
    #[serde(default)]
    mtime: u64,
}

/// `ScopeGuardBaselineEntry` for one repo-relative path already known to be
/// tracked-and-modified/deleted by `git status`: reads the file's current
/// size/mtime off disk, or (a delete) records `exists: false` when it is not
/// there at all. Never touches file contents.
fn scope_guard_baseline_entry(repo: &Path, rel: &str) -> ScopeGuardBaselineEntry {
    match std::fs::metadata(repo.join(rel)) {
        Ok(meta) => {
            let mtime = meta
                .modified()
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| u64::try_from(d.as_nanos()).unwrap_or(u64::MAX))
                .unwrap_or(0);
            ScopeGuardBaselineEntry {
                path: rel.to_string(),
                exists: true,
                size: meta.len(),
                mtime,
            }
        }
        Err(_) => ScopeGuardBaselineEntry {
            path: rel.to_string(),
            exists: false,
            size: 0,
            mtime: 0,
        },
    }
}

/// Snapshot tracked modified/deleted files at prompt time and compare
/// after shell calls. Ignore untracked files to avoid unrelated writes.
fn scope_guard_tracked_modified(repo: &Path) -> Option<Vec<ScopeGuardBaselineEntry>> {
    let output = std::process::Command::new("git")
        .args(["status", "--porcelain", "--no-renames"])
        .current_dir(repo)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let mut entries = Vec::new();
    for line in text.lines() {
        // Porcelain status `XY PATH`: ignore `??`/`!!`, which carry no
        // tracked-file change for this guard.
        if line.len() < 4 {
            continue;
        }
        let status = &line[..2];
        if status == "??" || status == "!!" {
            continue;
        }
        // A staged-new `A` file was not an existing tracked file, even when its
        // worktree column is also modified.
        if status.as_bytes()[0] == b'A' {
            continue;
        }
        if !status.contains('M') && !status.contains('D') {
            continue;
        }
        let rel = line[3..].trim();
        if rel.is_empty() {
            continue;
        }
        // Exclude workflow artifacts from the operator's change surface; they
        // may be written concurrently by zirv (#229, #232).
        if crate::commands::workflow::classify::is_workflow_work_path(Path::new(rel)) {
            continue;
        }
        entries.push(scope_guard_baseline_entry(repo, rel));
    }
    Some(entries)
}

/// Diffs `repo`'s CURRENT tracked modified/deleted snapshot against
/// `baseline`: every path that is new (absent from `baseline`) or whose
/// fingerprint differs, sorted. `None` when the current snapshot itself
/// cannot be read (see [`scope_guard_tracked_modified`]) -- the caller reads
/// that identically to "nothing changed" (silent), never as a real empty
/// result.
fn scope_guard_tracked_changes_since(
    repo: &Path,
    baseline: &[ScopeGuardBaselineEntry],
) -> Option<Vec<String>> {
    let current = scope_guard_tracked_modified(repo)?;
    let prior: std::collections::HashMap<&str, &ScopeGuardBaselineEntry> = baseline
        .iter()
        .map(|entry| (entry.path.as_str(), entry))
        .collect();
    let mut changed: Vec<String> = current
        .iter()
        .filter(|entry| {
            prior.get(entry.path.as_str()).is_none_or(|before| {
                before.exists != entry.exists
                    || before.size != entry.size
                    || before.mtime != entry.mtime
            })
        })
        .map(|entry| entry.path.clone())
        .collect();
    changed.sort();
    Some(changed)
}

/// One file per session id, named after a hash of it -- identical layout to
/// [`adoption_record_path`].
fn scope_guard_record_path(state: &StateDir, session: &str) -> PathBuf {
    state
        .scope_guard()
        .join(format!("{:016x}.json", input_hash(session)))
}

/// `None` on any doubt at all -- missing, corrupt, or a different schema
/// version -- deliberately unlike [`load_adoption_record`]'s always-`Default`
/// contract: an absent record here means no `UserPromptSubmit` ever ran for
/// this session (the guard disabled, no session identity, a write failure),
/// and the checkpoint/backstop must have nothing to say rather than
/// synthesizing an empty one from scratch.
fn load_scope_guard_record(path: &Path) -> Option<ScopeGuardRecord> {
    let record = std::fs::read_to_string(path)
        .ok()
        .and_then(|body| serde_json::from_str::<ScopeGuardRecord>(&body).ok())?;
    (record.version == SCOPE_GUARD_RECORD_VERSION).then_some(record)
}

/// Best-effort, like every other hook checkpoint write: a save that fails
/// costs the guard for this one prompt, never a hook failure.
fn save_scope_guard_record(path: &Path, record: &ScopeGuardRecord) {
    let Ok(json) = serde_json::to_string(record) else {
        return;
    };
    if let Some(dir) = path.parent() {
        let _ = crate::commands::ctx::state::create_private_dir_all(dir);
    }
    let _ = crate::commands::ctx::state::write_private(path, &json);
}

/// Splits `text` into naive sentences on `.`/`!`/`?` followed by whitespace
/// or end-of-text -- good enough for classifying a user's own prose prompt
/// or an assistant's own closing report (never source code), where an
/// abbreviation-heavy false split costs nothing worse than one extra,
/// harmless candidate sentence.
fn scope_guard_split_sentences(text: &str) -> Vec<String> {
    let mut sentences = Vec::new();
    let mut start = 0;
    for (i, ch) in text.char_indices() {
        if !matches!(ch, '.' | '!' | '?') {
            continue;
        }
        let end = i + ch.len_utf8();
        if !text[end..].chars().next().is_none_or(char::is_whitespace) {
            continue;
        }
        let sentence = text[start..end].trim();
        if !sentence.is_empty() {
            sentences.push(sentence.to_string());
        }
        start = end;
    }
    let tail = text[start..].trim();
    if !tail.is_empty() {
        sentences.push(tail.to_string());
    }
    sentences
}

/// Match preservation and limitation language conservatively so stated
/// constraints are visible at the checkpoint.
static SCOPE_GUARD_CONSTRAINT_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?xi)
        same\s+as\s+(?:always|before)
        | works\s+the\s+same
        | as\s+before
        | exactly\s+as
        | unchanged
        | keep\s
        | preserve
        | (?:don'?t|do\ not|never)\s+(?:change|touch|modify|alter)
        | only\s
        | backwards?[\s-]*compat
        | existing\s+behaviou?r
        | leave\b[\s\S]*?\balone\b
        ",
    )
    .expect("valid scope-guard constraint regex")
});

/// Prefer the last clarifying constraints in a prompt while bounding
/// count; preserve their original order in the final checklist.
fn scope_guard_select_capped(
    candidates: Vec<String>,
    max_count: usize,
    budget: usize,
) -> Vec<String> {
    let mut kept: Vec<String> = Vec::new();
    let mut total = 0usize;
    for sentence in candidates.into_iter().rev() {
        if kept.len() >= max_count {
            break;
        }
        let extra = sentence.chars().count() + usize::from(!kept.is_empty());
        if total + extra > budget {
            continue;
        }
        total += extra;
        kept.push(sentence);
    }
    kept.reverse();
    kept
}

/// Extracts up to [`SCOPE_GUARD_MAX_CONSTRAINTS`] preservation/limitation
/// sentences from `prompt`, trimmed, with the joined result capped at
/// [`SCOPE_GUARD_CONSTRAINT_BUDGET`] characters -- see
/// [`scope_guard_select_capped`] for the selection rule.
fn scope_guard_extract_constraints(prompt: &str) -> Vec<String> {
    let matched: Vec<String> = scope_guard_split_sentences(prompt)
        .into_iter()
        .filter(|sentence| SCOPE_GUARD_CONSTRAINT_RE.is_match(sentence))
        .collect();
    scope_guard_select_capped(
        matched,
        SCOPE_GUARD_MAX_CONSTRAINTS,
        SCOPE_GUARD_CONSTRAINT_BUDGET,
    )
}

/// At most this many characters across every extracted stated-detail
/// sentence, joined -- mirrors [`SCOPE_GUARD_CONSTRAINT_BUDGET`]'s own role.
const SCOPE_GUARD_STATED_DETAIL_BUDGET: usize = 700;

/// At most this many extracted stated-detail sentences.
const SCOPE_GUARD_MAX_STATED_DETAILS: usize = 8;

/// A stated, checkable detail: a backtick- or double-quoted literal (a
/// literal value, name, or message the request pins down exactly), or an
/// ordering/format/exactness word -- "sorted", "order", "ascending",
/// "descending", "exactly", "exact", "format", "exit status"/"exit code",
/// "stderr", "stdout", "print(s)", "message", "case-insensitive",
/// "comma-separated", "no spaces", "trailing", "leading". Deliberately
/// conservative like [`SCOPE_GUARD_CONSTRAINT_RE`]: favours catching a real
/// stated detail over precision.
static SCOPE_GUARD_STATED_DETAIL_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r#"(?xi)
        `[^`]+`
        | "[^"]+"
        | \bsorted\b
        | \border\b
        | \bascending\b
        | \bdescending\b
        | \bexactly\b
        | \bexact\b
        | \bformat\b
        | exit\s+status
        | exit\s+code
        | \bstderr\b
        | \bstdout\b
        | \bprints?\b
        | \bmessage\b
        | case-insensitive
        | comma-separated
        | no\s+spaces
        | \btrailing\b
        | \bleading\b
        "#,
    )
    .expect("valid scope-guard stated-detail regex")
});

/// Extracts up to [`SCOPE_GUARD_MAX_STATED_DETAILS`] stated-detail sentences
/// from `prompt` (see [`SCOPE_GUARD_STATED_DETAIL_RE`]), skipping any
/// sentence already captured in `constraints` (never duplicated between the
/// scope guard's own preservation language and this checklist), with the
/// joined result capped at [`SCOPE_GUARD_STATED_DETAIL_BUDGET`] characters --
/// see [`scope_guard_select_capped`] for the selection rule.
fn scope_guard_extract_stated_details(prompt: &str, constraints: &[String]) -> Vec<String> {
    let matched: Vec<String> = scope_guard_split_sentences(prompt)
        .into_iter()
        .filter(|sentence| {
            SCOPE_GUARD_STATED_DETAIL_RE.is_match(sentence) && !constraints.contains(sentence)
        })
        .collect();
    scope_guard_select_capped(
        matched,
        SCOPE_GUARD_MAX_STATED_DETAILS,
        SCOPE_GUARD_STATED_DETAIL_BUDGET,
    )
}

/// Detect an explicitly requested fix anywhere in the prompt so the Stop
/// backstop does not challenge work the operator asked for.
fn scope_guard_prompt_asks_for_fix(prompt: &str) -> bool {
    let lower = prompt.to_lowercase();
    [
        "fix",
        "bug",
        "broken",
        "error",
        "wrong",
        "crash",
        "regression",
        "off-by-one",
        "issue",
        "quirk",
        "problem",
        "resolve",
        "correct",
        "repair",
        "patch",
        "fail",
    ]
    .iter()
    .any(|keyword| lower.contains(keyword))
}

/// Replace the prior prompt's scope state so an old request's constraints
/// cannot govern a new edit; persistence failures remain silent.
pub(super) fn record_scope_guard_request(
    cfg: &CtxConfig,
    payload_session_id: &str,
    prompt: &str,
    repo: &Path,
    env: EnvLookup<'_>,
) {
    // Persist a prompt record when either scope guard or missing-tests
    // checkpoint needs it, even if the other gate is disabled.
    if !cfg.scope_guard.enabled && !cfg.missing_tests_gate.enabled {
        return;
    }
    // A harness notification is not the user's request and must not replace it.
    if super::prompt::is_harness_injected_prompt(prompt) {
        return;
    }
    let session = env(SESSION_ENV).unwrap_or_else(|| payload_session_id.to_string());
    if session.is_empty() {
        return;
    }
    let Ok(state) = StateDir::resolve(env) else {
        return;
    };
    let path = scope_guard_record_path(&state, &session);
    let prompt_hash = input_hash(prompt);
    if load_scope_guard_record(&path).is_some_and(|existing| existing.prompt_hash == prompt_hash) {
        // Preserve flags and baseline for an identical prompt so its
        // one-time checkpoint cannot fire twice.
        return;
    }
    let constraints = scope_guard_extract_constraints(prompt);
    let record = ScopeGuardRecord {
        version: SCOPE_GUARD_RECORD_VERSION,
        prompt_hash,
        stated_details: scope_guard_extract_stated_details(prompt, &constraints),
        constraints,
        asks_for_fix: scope_guard_prompt_asks_for_fix(prompt),
        checkpoint_shown: false,
        stop_checked: false,
        shell_baseline: scope_guard_tracked_modified(repo).unwrap_or_default(),
    };
    save_scope_guard_record(&path, &record);
    crate::commands::ctx::state::prune_to_newest(
        &state.scope_guard(),
        crate::commands::ctx::state::KEEP_NEWEST,
    );
}

/// Produce one non-blocking checkpoint for an eligible edit, including
/// a headless tests-owed note when applicable. Do not mark it shown until
/// the caller actually emits it; a later Deny must not spend the note.
pub(super) fn scope_checkpoint_note(
    payload: &PreToolPayload,
    cwd: &Path,
    cfg: &CtxConfig,
    env: EnvLookup<'_>,
) -> Option<String> {
    // A subagent shares the lead's session id, and so its prompt record; the record is the lead's (#849).
    if !payload.agent_id.is_empty()
        || !matches!(
            payload.tool_name.as_str(),
            "Edit" | "MultiEdit" | "NotebookEdit" | "Write"
        )
    {
        return None;
    }
    let target = normalized_write_target(payload, cwd);
    if payload.tool_name == "Write" && !target.as_deref().is_some_and(Path::is_file) {
        return None;
    }
    let session = env(SESSION_ENV).unwrap_or_else(|| payload.session_id.clone());
    if session.is_empty() {
        return None;
    }
    let state = StateDir::resolve(env).ok()?;
    let path = scope_guard_record_path(&state, &session);
    let record = load_scope_guard_record(&path)?;
    if record.checkpoint_shown {
        return None;
    }
    let headless = payload.permission_mode == "dontAsk";
    let scope_text = cfg
        .scope_guard
        .enabled
        .then(|| scope_checkpoint_text(&record.constraints, &record.stated_details, headless));
    let tests_owed = target
        .as_deref()
        .is_some_and(|target| missing_tests_owed(cfg, env, &[target]));
    scope_checkpoint_combine(scope_text, tests_owed)
}

/// Mark the checkpoint shown only after its text is emitted; failed
/// persistence leaves later hooks free to try again.
pub(super) fn scope_checkpoint_mark_shown(payload: &PreToolPayload, env: EnvLookup<'_>) {
    let session = env(SESSION_ENV).unwrap_or_else(|| payload.session_id.clone());
    if session.is_empty() {
        return;
    }
    let Ok(state) = StateDir::resolve(env) else {
        return;
    };
    let path = scope_guard_record_path(&state, &session);
    let Some(mut record) = load_scope_guard_record(&path) else {
        return;
    };
    if record.checkpoint_shown {
        return;
    }
    record.checkpoint_shown = true;
    save_scope_guard_record(&path, &record);
}

/// Fold tests-owed guidance into the first edit checkpoint so a headless
/// agent sees it before the Stop gate.
pub(super) const MISSING_TESTS_OWED_LINE: &str = "Write a focused test for each behaviour change in this same pass -- the run cannot finish \
     without one.";

/// Identify code changes for which the missing-tests gate expects tests;
/// test files and docs-only paths do not create that debt.
fn missing_tests_owed_by_path(path: &Path) -> bool {
    !path_looks_like_test_file(path)
        && !crate::commands::ctx::lifecycle::changes_are_doc_only(&[path.to_path_buf()])
}

/// Headless agents cannot ask whether a test is owed mid-turn; show this
/// guidance only when the gate is enabled and code changed.
fn missing_tests_owed(cfg: &CtxConfig, env: EnvLookup<'_>, paths: &[&Path]) -> bool {
    cfg.missing_tests_gate.enabled
        && env(adapters::HEADLESS_ENV).as_deref() == Some("1")
        && paths.iter().any(|path| missing_tests_owed_by_path(path))
}

/// Combines the scope guard's own checkpoint text (`None` when `cfg.
/// scope_guard.enabled` is off) with [`MISSING_TESTS_OWED_LINE`] (only when
/// [`missing_tests_owed`] says so) into the single note the checkpoint
/// actually shows. `None` only when NEITHER part applies -- the caller's own
/// "nothing to say" case; a record still gets read for this (see
/// `record_scope_guard_request`'s own doc comment), but nothing is ever
/// shown and `checkpoint_shown` is never set.
fn scope_checkpoint_combine(scope_text: Option<String>, tests_owed: bool) -> Option<String> {
    match (scope_text, tests_owed) {
        (Some(text), true) => Some(format!("{text} {MISSING_TESTS_OWED_LINE}")),
        (Some(text), false) => Some(text),
        (None, true) => Some(format!("Scope checkpoint: {MISSING_TESTS_OWED_LINE}")),
        (None, false) => None,
    }
}

fn scope_guard_quoted_constraints(constraints: &[String]) -> String {
    if constraints.is_empty() {
        String::new()
    } else {
        format!("the request says: \"{}\". ", constraints.join(" "))
    }
}

fn scope_guard_stated_details_line(details: &[String]) -> String {
    if details.is_empty() {
        return String::new();
    }
    let items = details
        .iter()
        .enumerate()
        .map(|(index, detail)| format!("({}) {detail}", index + 1))
        .collect::<Vec<_>>()
        .join(" ");
    format!(" Stated details to check before you finish: {items}")
}

/// Interactive advice asks before an unrequested fix; headless advice
/// tells the agent to defer because no operator can answer.
fn scope_checkpoint_text(
    constraints: &[String],
    stated_details: &[String],
    headless: bool,
) -> String {
    let action = if headless {
        "leave it and list it under 'Found, not changed' in your final report."
    } else {
        "ask the user first."
    };
    let quoted = scope_guard_quoted_constraints(constraints);
    let details = scope_guard_stated_details_line(stated_details);
    format!(
        "Scope checkpoint: {quoted}Before changing existing code, check this edit is needed for \
         what was asked. If it fixes a bug or makes an improvement you noticed but were not \
         asked for, don't make it: {action}{details}"
    )
}

/// At most this many changed paths named in the shell-edit checkpoint's own
/// text -- keeps it bounded regardless of how many files one shell command
/// touched.
const SCOPE_GUARD_SHELL_PATH_CAP: usize = 5;

/// Describe already-changed tracked files and request an undo, rather
/// than warning as if the edit had not happened.
fn scope_checkpoint_shell_text(
    constraints: &[String],
    changed: &[String],
    stated_details: &[String],
    headless: bool,
) -> String {
    let action = if headless {
        "list it under 'Found, not changed' in your final report."
    } else {
        "ask the user first."
    };
    let quoted = scope_guard_quoted_constraints(constraints);
    let listed = changed
        .iter()
        .take(SCOPE_GUARD_SHELL_PATH_CAP)
        .cloned()
        .collect::<Vec<_>>()
        .join(", ");
    let details = scope_guard_stated_details_line(stated_details);
    format!(
        "Scope checkpoint: {quoted}You just changed existing file(s) {listed}. Check the change \
         is needed for what was asked. If it fixes a bug or makes an improvement you noticed but \
         were not asked for, undo it and {action}{details}"
    )
}

/// After a shell call, compare tracked files against the prompt baseline
/// and emit the shared one-time checkpoint on the first change. Run cheap
/// eligibility/read-only gates before the git status query on this hot path;
/// uncertain commands still require rechecking. Tests-owed guidance applies
/// independently of scope-guard configuration.
pub(super) fn scope_guard_shell_checkpoint_note(
    tool_name: &str,
    cwd: &Path,
    cfg: &CtxConfig,
    payload_session_id: &str,
    command: &str,
    env: EnvLookup<'_>,
) -> Option<String> {
    if !matches!(tool_name, "Bash" | "PowerShell") {
        return None;
    }
    let session = env(SESSION_ENV).unwrap_or_else(|| payload_session_id.to_string());
    if session.is_empty() {
        return None;
    }
    let state = StateDir::resolve(env).ok()?;
    let path = scope_guard_record_path(&state, &session);
    let mut record = load_scope_guard_record(&path)?;
    if record.checkpoint_shown {
        return None;
    }
    // A positively read-only command (`git status`, `ls src`, ...) cannot
    // itself have produced the shell edit this checkpoint looks for, so skip
    // the `git status` re-query below entirely rather than run it after
    // EVERY `Bash`/`PowerShell` call. `&[]` scratchpad roots is the
    // conservative choice here (narrower than a caller's own configured
    // roots, never wider), and anything the classifier cannot positively
    // confirm still falls through to the re-query, unchanged.
    if crate::commands::ctx::safety::jev_approve_is_read_only_local(command, &[]) {
        return None;
    }
    let changed = scope_guard_tracked_changes_since(cwd, &record.shell_baseline)?;
    if changed.is_empty() {
        return None;
    }
    // Unlike `scope_checkpoint_note`'s own `payload.permission_mode`, claude's
    // documented `PostToolUse` payload carries no permission-mode field at
    // all, so this reads the same headless signal `scope_guard_stop_reason`
    // already uses for its own `PostToolUse`-adjacent (`Stop`) wording.
    let headless = env(adapters::HEADLESS_ENV).as_deref() == Some("1");
    let scope_text = cfg.scope_guard.enabled.then(|| {
        scope_checkpoint_shell_text(
            &record.constraints,
            &changed,
            &record.stated_details,
            headless,
        )
    });
    let changed_paths: Vec<&Path> = changed.iter().map(Path::new).collect();
    let tests_owed = missing_tests_owed(cfg, env, &changed_paths);
    let note = scope_checkpoint_combine(scope_text, tests_owed)?;
    record.checkpoint_shown = true;
    save_scope_guard_record(&path, &record);
    Some(note)
}

/// Match completed fixes only; infinitives and hypotheticals must not be
/// mistaken for a claim that the agent changed unrequested behaviour.
static SCOPE_GUARD_FIX_VERB_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\b(?:fixed|corrected|repaired|patched)\b")
        .expect("valid scope-guard fix-verb regex")
});

/// Whether `sentence` reads as conditional/hypothetical rather than a
/// completed action: "would", "could", "if". Checked alongside
/// [`SCOPE_GUARD_FIX_VERB_RE`]'s own past-tense-only restriction so a
/// hypothetical aside about what a fix WOULD do is never mistaken for a
/// claim that the agent actually made one.
static SCOPE_GUARD_HYPOTHETICAL_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\b(?:would|could|if)\b").expect("valid scope-guard hypothetical regex")
});

static SCOPE_GUARD_BUG_WORD_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\b(?:bugs?|off-by-one|quirk|broken|wrong|issue)\b")
        .expect("valid scope-guard bug-word regex")
});

/// "also fixed/changed/updated/refactored".
static SCOPE_GUARD_ALSO_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\balso\s+(?:fixed|changed|updated|refactored)\b")
        .expect("valid scope-guard also-fixed regex")
});

/// "while (I was) at it/there/here".
static SCOPE_GUARD_WHILE_AT_IT_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\bwhile\s+(?:i\s+was\s+)?(?:at\s+it|there|here)\b")
        .expect("valid scope-guard while-at-it regex")
});

/// Truncates `text` to [`SCOPE_GUARD_QUOTE_BUDGET`] characters.
fn scope_guard_truncate(text: &str) -> String {
    if text.chars().count() <= SCOPE_GUARD_QUOTE_BUDGET {
        return text.to_string();
    }
    text.chars().take(SCOPE_GUARD_QUOTE_BUDGET).collect()
}

/// Detect a claimed unrequested fix only when a completed-fix verb and
/// bug word occur together or in adjacent sentences.
fn scope_guard_unrequested_fix_sentence(closing: &str) -> Option<String> {
    let sentences = scope_guard_split_sentences(closing);
    for (index, sentence) in sentences.iter().enumerate() {
        if SCOPE_GUARD_HYPOTHETICAL_RE.is_match(sentence) {
            continue;
        }
        if !SCOPE_GUARD_FIX_VERB_RE.is_match(sentence) {
            continue;
        }
        let window = match sentences.get(index + 1) {
            Some(next) => format!("{sentence} {next}"),
            None => sentence.clone(),
        };
        if SCOPE_GUARD_BUG_WORD_RE.is_match(&window) {
            return Some(scope_guard_truncate(sentence));
        }
    }
    sentences
        .iter()
        .find(|sentence| {
            !SCOPE_GUARD_HYPOTHETICAL_RE.is_match(sentence)
                && (SCOPE_GUARD_ALSO_RE.is_match(sentence)
                    || SCOPE_GUARD_WHILE_AT_IT_RE.is_match(sentence))
        })
        .map(|sentence| scope_guard_truncate(sentence))
}

/// The last `budget` bytes of `path`'s content, lossily decoded and (unless
/// this IS the file's start) trimmed back to the next full line -- the
/// identical tail-read `stop_verify_reason` uses, so a Stop hook never
/// re-parses a whole long-running transcript on every turn.
fn scope_guard_tail_text(path: &Path, budget: u64) -> Option<String> {
    let mut file = std::fs::File::open(path).ok()?;
    let start = file.metadata().ok()?.len().saturating_sub(budget);
    std::io::Seek::seek(&mut file, std::io::SeekFrom::Start(start)).ok()?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes).ok()?;
    let text = String::from_utf8_lossy(&bytes).into_owned();
    Some(if start == 0 {
        text
    } else {
        text.split_once('\n')
            .map_or_else(String::new, |(_, rest)| rest.to_string())
    })
}

/// The current turn's closing assistant message -- everything after the last
/// `TurnStart`, the identical window `stop_verify_facts` uses -- or `None`
/// when there is none.
fn scope_guard_closing_text(events: &[NormalizedEvent]) -> Option<&str> {
    let start = events
        .iter()
        .rposition(|event| matches!(event, NormalizedEvent::TurnStart { .. }))
        .map_or(0, |index| index + 1);
    events[start..].iter().rev().find_map(|event| match event {
        NormalizedEvent::AssistantFinal { text, .. } if !text.trim().is_empty() => {
            Some(text.as_str())
        }
        _ => None,
    })
}

/// Block once when the closing report claims an unrequested fix and no
/// earlier Stop gate already blocked; uncertainty passes through.
pub(super) fn scope_guard_stop_reason(
    state: &StateDir,
    cfg: &CtxConfig,
    session: &str,
    transcript: &Path,
    env: EnvLookup<'_>,
) -> Option<String> {
    if !cfg.scope_guard.enabled || session.is_empty() {
        return None;
    }
    let path = scope_guard_record_path(state, session);
    let mut record = load_scope_guard_record(&path)?;
    if record.stop_checked || record.asks_for_fix {
        return None;
    }
    let adapter = adapters::select_for_identity(
        env(adapters::AGENT_ENV).as_deref().or(cfg.agent.as_deref()),
        &[],
        cfg,
    )
    .ok()?;
    let tail = scope_guard_tail_text(transcript, STOP_VERIFY_TAIL_BYTES)?;
    let events = adapter.parse_events(&tail);
    let closing = scope_guard_closing_text(&events)?;
    let sentence = scope_guard_unrequested_fix_sentence(closing)?;
    record.stop_checked = true;
    save_scope_guard_record(&path, &record);
    let headless = env(adapters::HEADLESS_ENV).as_deref() == Some("1");
    let suffix = if headless { "" } else { " or ask the user." };
    let constraints = record.constraints.join(" ");
    Some(format!(
        "Your report says you changed something the request did not ask for: \"{sentence}\". \
         The request said: \"{constraints}\". Unless that change was strictly required to \
         deliver the request, revert it and report it as found-not-changed{suffix}"
    ))
}

#[cfg(test)]
mod tests {
    use super::super::tests::{SCOPE_GUARD_T24_PROMPT, git_repo};
    use super::*;

    /// `git status --porcelain`'s `A `/`AM` INDEX codes mark a file created
    /// and staged THIS turn -- never one that existed before it -- so `AM`'s
    /// own `M` must not be read as an edit to an EXISTING tracked file.
    #[test]
    fn scope_guard_tracked_modified_excludes_a_staged_new_file() {
        let repo = git_repo();
        let git = |args: &[&str]| {
            let status = std::process::Command::new("git")
                .args(args)
                .current_dir(repo.path())
                .status()
                .expect("run git");
            assert!(status.success(), "git {args:?} failed");
        };
        std::fs::write(repo.path().join("brand_new.txt"), "one\n").expect("write");
        git(&["add", "brand_new.txt"]);
        std::fs::write(repo.path().join("brand_new.txt"), "one\ntwo\n").expect("modify staged");

        let modified = scope_guard_tracked_modified(repo.path()).expect("git status");
        assert!(
            modified.iter().all(|entry| entry.path != "brand_new.txt"),
            "a file created and staged this turn must not be reported as an existing-tracked \
             edit: {modified:?}"
        );
    }

    /// A harness notification arriving as a prompt must not replace the
    /// user's recorded request.
    #[test]
    fn a_harness_notification_never_replaces_the_users_scope_record() {
        let repo = git_repo();
        let state = tempfile::tempdir().expect("state");
        let env: std::collections::HashMap<String, String> = [(
            crate::commands::ctx::state::STATE_ENV.to_string(),
            state.path().display().to_string(),
        )]
        .into();
        let lookup = |k: &str| env.get(k).cloned();
        let mut cfg = CtxConfig::default();
        cfg.scope_guard.enabled = true;
        let store = StateDir::from_root(state.path().to_path_buf());
        let path = scope_guard_record_path(&store, "s1");

        record_scope_guard_request(&cfg, "s1", SCOPE_GUARD_T24_PROMPT, repo.path(), &lookup);
        let genuine = load_scope_guard_record(&path).expect("the user's request is recorded");

        record_scope_guard_request(
            &cfg,
            "s1",
            "<task-notification>\n<task-id>b8f</task-id>\n</task-notification>",
            repo.path(),
            &lookup,
        );
        let after = load_scope_guard_record(&path).expect("still recorded");
        assert_eq!(after.prompt_hash, genuine.prompt_hash);
    }

    /// Constraint extraction picks the two sentences that actually restrict
    /// what may change, and never the plain informational opener.
    #[test]
    fn scope_guard_extracts_the_preservation_sentences_from_the_t24_prompt() {
        let constraints = scope_guard_extract_constraints(SCOPE_GUARD_T24_PROMPT);
        assert!(
            constraints
                .iter()
                .any(|s| s.contains("works the same as") && s.contains("Pagination")),
            "must pick the pagination sentence: {constraints:?}"
        );
        assert!(
            constraints.iter().any(|s| s.contains("exactly as before")),
            "must pick the exactly-as-before sentence: {constraints:?}"
        );
        assert!(
            !constraints
                .iter()
                .any(|s| s.contains("thousands of transactions")),
            "must ignore the plain informational opener: {constraints:?}"
        );
    }

    // -- Scope guard: the stated-details checklist (queued item 2) ----------

    const SCOPE_GUARD_STATED_DETAILS_PROMPT: &str = "Please add a new export command. Print an \
         exact `DONE` marker when it finishes. The rows must come out sorted in ascending order. \
         This sentence is just plain descriptive prose about the feature with nothing special \
         stated.";

    /// Picks up a backtick-quoted literal AND an ordering word, and ignores
    /// plain prose that matches neither.
    #[test]
    fn scope_guard_extract_stated_details_picks_up_quoted_literals_and_ordering_words() {
        let details = scope_guard_extract_stated_details(SCOPE_GUARD_STATED_DETAILS_PROMPT, &[]);
        assert!(
            details.iter().any(|d| d.contains("`DONE`")),
            "must pick up the backtick-quoted literal: {details:?}"
        );
        assert!(
            details.iter().any(|d| d.contains("sorted")),
            "must pick up the ordering word: {details:?}"
        );
        assert!(
            !details
                .iter()
                .any(|d| d.contains("plain descriptive prose")),
            "must ignore plain prose: {details:?}"
        );
    }

    /// More than [`SCOPE_GUARD_MAX_STATED_DETAILS`] qualifying sentences
    /// still caps out at the limit.
    #[test]
    fn scope_guard_extract_stated_details_respects_the_cap() {
        let prompt: String = (1..=12)
            .map(|i| format!("Field {i} must be formatted exactly as \"value{i}\". "))
            .collect();
        let details = scope_guard_extract_stated_details(&prompt, &[]);
        assert_eq!(
            details.len(),
            SCOPE_GUARD_MAX_STATED_DETAILS,
            "must cap at {SCOPE_GUARD_MAX_STATED_DETAILS}: {details:?}"
        );
    }

    /// A sentence already captured as a scope constraint must never also
    /// appear in the stated-details list.
    #[test]
    fn scope_guard_extract_stated_details_skips_a_sentence_already_captured_as_a_constraint() {
        let prompt = "Keep the sort order exactly as before, unchanged and exact.";
        let constraints = scope_guard_extract_constraints(prompt);
        assert!(
            !constraints.is_empty(),
            "sanity: this sentence must match the constraint pattern too"
        );
        let details = scope_guard_extract_stated_details(prompt, &constraints);
        assert!(
            details.is_empty(),
            "a sentence already captured as a constraint must not duplicate into stated details: \
             {details:?}"
        );
    }

    // -- Scope guard Stop backstop: hypothetical-fix false positive fix -----

    /// Benchmark false positive: a hypothetical aside about what a fix WOULD
    /// do must never be read as a claimed, completed fix -- while the two
    /// true positives that fired correctly in the same benchmark run must
    /// still be caught.
    #[test]
    fn scope_guard_unrequested_fix_sentence_ignores_a_hypothetical_aside() {
        assert!(
            scope_guard_unrequested_fix_sentence("`page()` had two bugs, and I fixed both.")
                .is_some(),
            "a completed fix alongside a bug word must still block"
        );
        assert!(
            scope_guard_unrequested_fix_sentence(
                "I also changed `report.page()`, which you didn't ask for."
            )
            .is_some(),
            "an explicit 'also changed' claim must still block"
        );
        assert!(
            scope_guard_unrequested_fix_sentence(
                "Fixing it would change the `--legacy-order` output too."
            )
            .is_none(),
            "a hypothetical 'would' aside naming something the agent did NOT do must never block"
        );
    }
}
