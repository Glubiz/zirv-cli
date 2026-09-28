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

// -- Scope-creep guard ------------------------------------------------------
//
// Operator-requested guard: a hidden benchmark task asked for a sort-order
// change plus "pagination works the same as always" and a `--legacy-order`
// flag preserving "the old raw order exactly as before"; one agent noticed a
// pre-existing pagination off-by-one, decided it "looked like an outright
// bug" and fixed it unasked, and hidden tests expecting unchanged pagination
// failed. This never blocks scope creep outright (that would need real
// review); it only makes the request's own preservation language visible at
// the moment of editing (`PreToolUse`) and catches an unrequested-fix claim
// once at the end (`Stop`) as a backstop.
//
// `UserPromptSubmit` records the request's own state; `PreToolUse` reads it
// for the checkpoint; `Stop` reads it for the backstop. All three degrade to
// a silent no-op on any doubt at all (config off, no session identity, no
// state dir, an I/O failure) -- a hook must never break a session over this.

/// `ScopeGuardRecord`'s own schema version -- bumped if the shape ever
/// changes, so an old record on disk reads back as "no record" rather than a
/// deserialize failure or (worse) a wrongly-interpreted new field.
///
/// v2 (the shell-edit checkpoint): adds `shell_baseline`, the tracked
/// modified/deleted file snapshot `record_scope_guard_request` takes at
/// `UserPromptSubmit`. Bumped rather than defaulted in place because an old
/// v1 record on disk has no baseline at all -- reading it back as "no
/// record" (forcing the next `UserPromptSubmit` to rebuild one) is safer
/// than silently treating an absent baseline as "nothing was ever modified",
/// which would make the very first shell edit after an upgrade look like a
/// change against an empty baseline and fire the checkpoint immediately.
///
/// v3 (the stated-details checklist): adds `stated_details`.
const SCOPE_GUARD_RECORD_VERSION: u32 = 3;

/// At most this many characters across every extracted constraint sentence,
/// joined -- keeps the checkpoint/backstop text bounded regardless of how
/// verbose the request was.
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
    /// The repo's tracked modified/deleted files (never untracked) at the
    /// moment this prompt was recorded, each with a cheap size/mtime
    /// fingerprint (never file contents) -- the baseline the `PostToolUse`
    /// shell-edit checkpoint diffs its own re-query against, so a shell
    /// command (`sed -i`, `python -c "open(p,'w')..."`, `cat > file`) that
    /// changes an existing tracked file is visible even though it never
    /// goes through `Edit`/`Write` at all. Empty when git failed, this is
    /// not a git repo, or the guard was disabled -- see
    /// `scope_guard_tracked_modified`'s own doc comment.
    #[serde(default)]
    shell_baseline: Vec<ScopeGuardBaselineEntry>,
    /// The request's own stated, checkable details -- a quoted literal or an
    /// ordering/format/exactness word (see
    /// [`SCOPE_GUARD_STATED_DETAIL_RE`]) -- in prompt order, never
    /// duplicating a sentence already captured in `constraints`. Shown in
    /// the checkpoint as a numbered "Stated details to check before you
    /// finish" list, governed by `cfg.scope_guard.enabled` the same as
    /// `constraints` itself.
    #[serde(default)]
    stated_details: Vec<String>,
}

/// One tracked file's cheap fingerprint for the shell-edit checkpoint's own
/// before/after comparison: never a content hash (CLAUDE.md: "a cheap
/// fingerprint (size + mtime is fine; do not hash large file contents)").
/// `exists` distinguishes a tracked file `git status` reports as deleted
/// (no size/mtime to read) from one that is merely absent from a snapshot
/// entirely -- so a delete, and a later re-create with different content,
/// both still count as a change.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct ScopeGuardBaselineEntry {
    /// Repo-relative, forward-slashed (`git status --porcelain`'s own
    /// spelling) -- never a platform `PathBuf`, so the record round-trips
    /// identically on every OS this hook runs on.
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

/// The repo's currently tracked, modified-or-deleted files -- the shell-edit
/// checkpoint's own snapshot, taken once at `UserPromptSubmit` as the
/// baseline and re-taken on every qualifying `PostToolUse` shell call to
/// diff against it. Deliberately narrower than
/// `workflow::verification::changed_paths` (which also folds in untracked
/// `??` files via `git ls-files --others`): an untracked file is a NEW file,
/// never an edit to "existing code", so including it here would make the
/// checkpoint fire for a shell command that only ever created something.
///
/// `None` on any doubt at all -- not a git repo, git missing, git failing
/// for any other reason -- so both the baseline write and the later
/// re-query degrade to silence together (see this guard's own module-level
/// doc comment: "all three degrade to a silent no-op on any doubt at all").
/// `Some(vec![])` is a real, successful "nothing is modified" answer, never
/// conflated with the failure case.
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
        // `git status --porcelain` lines are `XY PATH`, `XY` exactly two
        // status characters, a space, then the path -- `??` (untracked) and
        // `!!` (ignored) are the only two-letter codes with no tracked
        // meaning at all; every other code names a real index/worktree
        // change to a file git already tracks.
        if line.len() < 4 {
            continue;
        }
        let status = &line[..2];
        if status == "??" || status == "!!" {
            continue;
        }
        // The INDEX column (`status`'s first byte) is `A` for a file
        // created and staged THIS turn, never for one that existed before
        // it -- `AM` (staged-new, then edited again) contains an `M` that
        // would otherwise be read as an edit to an EXISTING tracked file.
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
        // Issue #229/#232's own exclusion, mirrored from `changed_paths`:
        // the workflow's own `.zirv/work/<id>/*` artifacts are not the
        // operator's change surface, and this benchmark's own transcripts
        // are full of concurrent writes to them.
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

/// Preservation/limitation phrasing (deliberately conservative -- favours
/// catching a real constraint over precision): "same as always/before",
/// "works the same", "as before", "exactly as", "unchanged", "keep ",
/// "preserve", "don't/do not/never change/touch/modify/alter", "only ",
/// "backward(s) compat[ible]", "existing behavio(u)r", "leave ... alone".
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

/// Shared selection algorithm for [`scope_guard_extract_constraints`]/
/// [`scope_guard_extract_stated_details`]: keeps at most `max_count` of
/// `candidates`, closest to the END of the prompt first -- a closing,
/// clarifying sentence (e.g. "...exactly as before.") wins over an earlier
/// one that only incidentally matches the same conservative pattern (e.g. a
/// feature sentence that happens to use the word "keep" for an unrelated
/// tie-break rule -- see this guard's own worked example, where exactly that
/// happens) -- with the joined result capped at `budget` characters.
/// Returned in the prompt's own original order.
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

/// Whether the request itself already asks for a fix, anywhere in the
/// prompt, case-insensitive. A superset of every word the Stop backstop's
/// own [`SCOPE_GUARD_FIX_VERB_RE`]/[`SCOPE_GUARD_BUG_WORD_RE`] look for, so
/// a request phrased as "resolve the pagination issue" can never have its
/// own requested fix blocked as unrequested.
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

/// `UserPromptSubmit`: records this session's scope-guard state for the
/// CURRENT prompt, replacing any record from an earlier one (a different
/// `prompt_hash`). Never emits anything -- the checkpoint/backstop are the
/// only channels that speak; this only persists state for them to read.
/// Every gate below (config off, no session identity, no state dir) is a
/// silent skip: a hook must never fail a prompt over this.
///
/// `repo` also seeds `shell_baseline` (the `PostToolUse` shell-edit
/// checkpoint's own before-snapshot, [`scope_guard_tracked_modified`]) --
/// best-effort like everything else here: a failed git query just leaves it
/// empty, which reads downstream as "no baseline to diff against" and keeps
/// that checkpoint silent too, never as a hook failure.
pub(super) fn record_scope_guard_request(
    cfg: &CtxConfig,
    payload_session_id: &str,
    prompt: &str,
    repo: &Path,
    env: EnvLookup<'_>,
) {
    // The record now also backs the missing-tests "tests owed" line folded
    // into this same checkpoint (see `missing_tests_owed`/`scope_checkpoint_
    // combine`), which fires independently of `scope_guard.enabled` -- so a
    // record must exist whenever EITHER feature is on, not only when the
    // scope guard itself is.
    if !cfg.scope_guard.enabled && !cfg.missing_tests_gate.enabled {
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
        // The identical prompt was already recorded -- leave the flags
        // (`checkpoint_shown`/`stop_checked`) and the shell baseline exactly
        // as they are.
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

/// `PreToolUse`, `Edit`/`MultiEdit`/`NotebookEdit`/an existing-file `Write`
/// only: the non-blocking scope checkpoint's own TEXT, shown once per prompt
/// (the persisted `checkpoint_shown` flag). `None` on every gate below (a
/// tool this guard does not cover, a `Write` to a file that does not exist
/// yet, no session identity, no recorded prompt at all, already shown for
/// this prompt, and -- since neither `cfg.scope_guard.enabled` nor
/// `missing_tests_owed` has anything to say -- both features off or neither
/// applying to this edit) -- a silent skip, like every other advisory in
/// this file. Never changes `payload`'s own permission outcome: this only
/// ever rides as a non-blocking `additionalContext` note.
///
/// Folds in the missing-tests gate's own "tests owed" line
/// ([`missing_tests_owed`]/[`scope_checkpoint_combine`]) alongside the scope
/// guard's own text: a headless session that would otherwise only learn it
/// owes a test once the missing-tests Stop gate blocks it -- after the whole
/// turn is already done -- sees it here instead, at the FIRST edit, in the
/// same one-time note. Independent of `cfg.scope_guard.enabled`: the tests-
/// owed line can fire this checkpoint on its own even with the scope guard
/// itself turned off.
///
/// Deliberately a pure read -- it never marks the checkpoint shown itself.
/// `run_pretool`'s own orchestrator-write guard can still DENY this exact
/// call after this function returns `Some`, in which case nothing is ever
/// actually surfaced to the model; the caller commits the flag with
/// [`scope_checkpoint_mark_shown`] only once it knows the text is really
/// going out, so a denied write never silently spends the one checkpoint a
/// later, actually-allowed edit still needed.
pub(super) fn scope_checkpoint_note(
    payload: &PreToolPayload,
    cwd: &Path,
    cfg: &CtxConfig,
    env: EnvLookup<'_>,
) -> Option<String> {
    if !matches!(
        payload.tool_name.as_str(),
        "Edit" | "MultiEdit" | "NotebookEdit" | "Write"
    ) {
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

/// Commits [`scope_checkpoint_note`]'s own `checkpoint_shown` flag -- called
/// only once its text is actually about to reach the model (see that
/// function's own doc comment for why this is split out). Best-effort, like
/// every other state write in this file: a save that fails costs the guard
/// for this one prompt, never a hook failure.
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

/// zirv's own tests-owed sentence, folded into the same one-time checkpoint
/// as the scope guard's own text (see [`missing_tests_owed`]/
/// [`scope_checkpoint_combine`]) rather than waiting for the missing-tests
/// Stop gate ([`missing_tests_gate_reason`]) to say it after the whole turn
/// has already finished -- that costs a whole extra round for a headless
/// session that never touched a test file.
pub(super) const MISSING_TESTS_OWED_LINE: &str = "Write a focused test for each behaviour change in this same pass -- the run cannot finish \
     without one.";

/// Whether `path` is the kind of change the missing-tests gate itself cares
/// about: not a test file ([`path_looks_like_test_file`]) and not doc-only
/// ([`crate::commands::ctx::lifecycle::changes_are_doc_only`]). Shared by
/// `missing_tests_gate_reason` (which classifies every path the WHOLE turn
/// changed) and [`missing_tests_owed`] (which classifies only the path(s) a
/// single checkpoint call already knows about); unlike
/// `missing_tests_gate_reason`'s own `rust_change_touches_cfg_test` check,
/// this never shells out to `git diff` -- the checkpoint fires before
/// (`PreToolUse`) or immediately after (`PostToolUse`, already cheap on its
/// own hot path) an edit, so it only ever has a filename shape to go on, not
/// a diff.
fn missing_tests_owed_by_path(path: &Path) -> bool {
    !path_looks_like_test_file(path)
        && !crate::commands::ctx::lifecycle::changes_are_doc_only(&[path.to_path_buf()])
}

/// Whether the checkpoint's own "tests owed" line
/// ([`MISSING_TESTS_OWED_LINE`]) applies: the missing-tests gate is enabled,
/// this is a HEADLESS session (`adapters::HEADLESS_ENV == "1"`, the same
/// condition `missing_tests_gate_reason` itself checks), and at least one of
/// `paths` is a non-test, non-doc source file
/// ([`missing_tests_owed_by_path`]). Independent of `cfg.scope_guard.
/// enabled` -- this can fire the checkpoint on its own even with the scope
/// guard itself turned off, and never changes `missing_tests_gate_reason`'s
/// own Stop-hook logic, which stays the backstop it always was.
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

/// Shared by [`scope_checkpoint_text`] and [`scope_checkpoint_shell_text`]:
/// the request's own quoted preservation sentences, or empty when none were
/// extracted.
fn scope_guard_quoted_constraints(constraints: &[String]) -> String {
    if constraints.is_empty() {
        String::new()
    } else {
        format!("the request says: \"{}\". ", constraints.join(" "))
    }
}

/// Shared by [`scope_checkpoint_text`] and [`scope_checkpoint_shell_text`]:
/// the stated-details checklist itself -- a numbered "(1) ... (2) ..." list
/// appended to the checkpoint, or empty when nothing was extracted. Leads
/// with a space so the caller can splice it straight onto the end of its own
/// sentence.
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

/// The checkpoint's own wording: interactive asks the user before an
/// unrequested fix/improvement; headless (`permission_mode == "dontAsk"`,
/// the same signal `safety.rs`'s `hook_output` reads for the identical
/// purpose on its own payload) has no one to ask, so it defers to the final
/// report instead. `stated_details` appends the queued item 2 checklist
/// ([`scope_guard_stated_details_line`]) when the request pinned down any
/// checkable detail.
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

/// The `PostToolUse`, after-the-fact counterpart to [`scope_checkpoint_text`]
/// -- fires once the shell command has already changed `changed` (capped at
/// [`SCOPE_GUARD_SHELL_PATH_CAP`] paths), so it names what changed and asks
/// for an undo rather than warning before an edit. Shares
/// [`scope_checkpoint_text`]'s own headless/interactive split, minus that
/// variant's leading "leave it and" -- this sentence already opens with
/// "undo it and".
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

/// `PostToolUse`, `Bash`/`PowerShell` only: the tool-agnostic, AFTER-the-fact
/// counterpart to [`scope_checkpoint_note`]. Benchmark transcripts show a
/// headless agent makes most of its edits to an existing file through the
/// SHELL (`python -c "open(p,'w').write(...)"`, `sed -i`, `cat > file`),
/// never touching `Edit`/`MultiEdit`/`NotebookEdit`/`Write` at all -- so that
/// checkpoint never fires for it. This re-checks the repo's tracked-file
/// state against the baseline `record_scope_guard_request` snapshotted at
/// `UserPromptSubmit` ([`ScopeGuardRecord::shell_baseline`]), and the FIRST
/// time anything differs, surfaces the same one-time note worded for a
/// change that already happened ([`scope_checkpoint_shell_text`]). Shares
/// `checkpoint_shown` with [`scope_checkpoint_note`]/
/// [`scope_checkpoint_mark_shown`]: whichever path fires first is the only
/// one that ever speaks for a given prompt, so an agent that mixes `Edit`
/// and shell edits never sees the note twice.
///
/// `None` on every gate below (not a shell tool, no session identity, no
/// state dir, no recorded prompt, already shown, a positively read-only
/// command, nothing changed, and -- since neither `cfg.scope_guard.enabled`
/// nor `missing_tests_owed` has anything to say -- both features off or
/// neither applying to what changed) -- a silent skip, like every other
/// advisory in this guard. Deliberately ordered cheapest-first: the `git
/// status` re-query -- this function's only non-trivial cost -- only ever
/// runs once every cheaper gate above it (most of all `checkpoint_shown` and
/// the read-only check, which reuses `safety::jev_approve_is_read_only_
/// local`) has already passed, since this runs after EVERY `Bash`/
/// `PowerShell` call. A command the classifier cannot positively confirm
/// read-only still runs the re-query below, unchanged from before.
///
/// Folds in the missing-tests gate's own "tests owed" line the same way
/// [`scope_checkpoint_note`] does ([`missing_tests_owed`]/
/// [`scope_checkpoint_combine`]), classified against `changed` (every path
/// this call found different from the baseline) rather than a single
/// target -- independent of `cfg.scope_guard.enabled`.
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

/// A COMPLETED fix verb, past tense only: `fixed`/`corrected`/`repaired`/
/// `patched`. Bare `fix`/`fixing` are deliberately excluded (a benchmark
/// false positive: "Fixing it would change the `--legacy-order` output
/// too." names a hypothetical the agent explicitly did NOT do, not a claimed
/// change) -- see [`SCOPE_GUARD_HYPOTHETICAL_RE`] for the second, general
/// guard against a conditional/hypothetical sentence being read as a claim
/// at all.
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

/// A word naming what the fix was for: `bug(s)`/`off-by-one`/`quirk`/
/// `broken`/`wrong`/`issue`. Plural `bugs` alongside the design's own
/// singular `bug`, since a real closing report ("It had two bugs") uses it.
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

/// Whether a `Stop` closing message claims a fix the request never asked
/// for: a completed fix verb ([`SCOPE_GUARD_FIX_VERB_RE`]) alongside a bug
/// word ([`SCOPE_GUARD_BUG_WORD_RE`]) in the same sentence OR the very next
/// one -- a closing report often splits the claim and what it was for across
/// two short adjacent sentences (this guard's own worked example: "`page()`
/// had two bugs, and I fixed both.") -- OR an explicit "also fixed/changed/
/// updated/refactored", OR "while (I was) at it/there/here". Every candidate
/// sentence is first checked against [`SCOPE_GUARD_HYPOTHETICAL_RE`] and
/// skipped if it reads as conditional/hypothetical ("Fixing it would change
/// the `--legacy-order` output too." names something the agent did NOT do).
/// Returns the first matching sentence, truncated, or `None`.
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

/// `Stop` backstop: blocks once, after every other Stop gate/backstop has
/// already had its chance (see `run_stop`'s own call site -- this runs only
/// when `stop_verify_block` did not already fire, so at most one block per
/// Stop), when the closing report claims a fix the request never asked for.
/// `None` on every gate below (config off, no session identity, no recorded
/// prompt, already checked this prompt, the request itself asks for a fix,
/// no adapter, an unreadable transcript, no unrequested-fix sentence found)
/// -- a silent skip, like every other Stop-hook check in this file.
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
