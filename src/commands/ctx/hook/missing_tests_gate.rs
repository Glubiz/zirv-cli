//! Stop-hook gate: whether this turn's diff owes a test, and the jev
//! question/action pair that can override a borderline verdict.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::pretool_tier::{DispatchAdviseState, capped_u32};
use crate::commands::ctx::adapters::{self};
use crate::commands::ctx::config::{CtxConfig, EnvLookup};
use crate::commands::ctx::event::input_hash;
use crate::commands::ctx::state::StateDir;
use crate::commands::workflow::verification;

/// Q1: whether this session has already been BLOCKED once by the
/// missing-tests gate. A separate, persisted fact from `stop_hook_active`:
/// that flag only breaks the loop within a single stop ATTEMPT (the harness
/// re-invoking Stop immediately after a block), never across a session's
/// later, genuinely new stop attempts -- and this gate must fire at most
/// once per session, full stop, per the task's own contract.
const MISSING_TESTS_GATE_RECORD_VERSION: u32 = 1;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct MissingTestsGateRecord {
    #[serde(default)]
    version: u32,
    blocked: bool,
}

// F6 (codex review fix): keyed by `stable_short` (the socket-derived
// identifier `run_stop`'s own caller already computes -- issue #243, see its
// doc comment there), NOT the rotating session id -- a supervised restart
// mints a fresh `SESSION_ENV`/`payload.session_id`, and this gate's own
// contract above ("at most once per session, full stop") means the whole
// supervised run, which `stable_short` -- unlike the rotating id -- actually
// tracks across a restart.
fn missing_tests_gate_record_path(state: &StateDir, stable_short: &str) -> PathBuf {
    // Mirrors `verify_on_stop_record_path`'s own naming/hash scheme, in the
    // same scoring directory.
    state.scoring().join(format!(
        "{:016x}-missing-tests-gate.json",
        input_hash(stable_short)
    ))
}

/// `Default` (never yet blocked) on any doubt at all -- unreadable, corrupt,
/// or a different schema version -- like every other hook state read.
fn load_missing_tests_gate_record(path: &Path) -> MissingTestsGateRecord {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|body| serde_json::from_str::<MissingTestsGateRecord>(&body).ok())
        .filter(|record| record.version == MISSING_TESTS_GATE_RECORD_VERSION)
        .unwrap_or_default()
}

/// Best-effort, like every other hook checkpoint write: a save that fails
/// costs (at most) one extra block later, never a hook failure now.
fn save_missing_tests_gate_record(path: &Path, record: &MissingTestsGateRecord) {
    let Ok(json) = serde_json::to_string(record) else {
        return;
    };
    if let Some(dir) = path.parent() {
        let _ = crate::commands::ctx::state::create_private_dir_all(dir);
    }
    let _ = crate::commands::ctx::state::write_private(path, &json);
}

/// Whether `path` is itself a test file by name/location alone --
/// language-agnostic: any path component literally named `test`/`tests`
/// (case-insensitive: a `tests/` directory, Rust's own `tests/` integration
/// dir, `src/test/java/...`, ...), or a filename matching pytest's
/// `test_*.py`, Go/Ruby/PHP/etc.'s `*_test.*`, or JS/TS's `*.test.*`.
pub(crate) fn path_looks_like_test_file(path: &Path) -> bool {
    if path.components().any(|component| {
        component.as_os_str().to_str().is_some_and(|name| {
            name.eq_ignore_ascii_case("test") || name.eq_ignore_ascii_case("tests")
        })
    }) {
        return true;
    }
    let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
        return false;
    };
    let lower = name.to_ascii_lowercase();
    lower.starts_with("test_") || lower.contains("_test.") || lower.contains(".test.")
}

/// Pure: whether a unified diff's text contains at least one hunk whose
/// new-file side starts at or after `line`. Split out of
/// `rust_change_touches_cfg_test` so the hunk-header parsing itself is
/// directly unit-testable without shelling out to git.
fn diff_touches_line_at_or_after(diff_text: &str, line: usize) -> bool {
    diff_text
        .lines()
        .filter_map(hunk_new_start)
        .any(|start| start >= line)
}

/// Parses a unified-diff hunk header's new-file start line: `"@@ -a,b +c,d
/// @@"` -> `c` (and the single-line-hunk shorthand `"@@ -a +c @@"` -> `c`,
/// since splitting `"c"` on `,` yields `["c"]`). `None` for anything that is
/// not a hunk header at all.
fn hunk_new_start(line: &str) -> Option<usize> {
    let rest = line.strip_prefix("@@ ")?;
    let plus = rest.split(' ').find(|part| part.starts_with('+'))?;
    plus.trim_start_matches('+').split(',').next()?.parse().ok()
}

/// Rust-specific: whether a change to `path` (relative to `repo`, already
/// known to exist in the working tree) landed on or after that file's own
/// `#[cfg(test)]` line. This repo's own convention (CLAUDE.md: "Tests stay
/// inline in `#[cfg(test)] mod tests`") and idiomatic Rust generally both
/// keep unit tests in the same file as the code they cover, so
/// [`path_looks_like_test_file`] alone would never see a test-only change to
/// an existing source file -- every source file in a Rust project potentially
/// carries its own inline test module. Never a false positive from an
/// unrelated pre-existing test module elsewhere in a large file: the check is
/// position-sensitive, not just "does this file contain `#[cfg(test)]`
/// anywhere", because the file's tests conventionally sit at the bottom (last
/// in the file), so a hunk landing at or after that line is, in practice, a
/// change to the test module rather than to unrelated code above it.
///
/// `false` on any doubt at all -- an unreadable file, no `#[cfg(test)]`
/// marker in it, or a `git diff` that fails for any reason.
fn rust_change_touches_cfg_test(repo: &Path, path: &Path) -> bool {
    let Ok(text) = std::fs::read_to_string(repo.join(path)) else {
        return false;
    };
    let Some(test_line) = text
        .lines()
        .position(|line| line.trim_start().starts_with("#[cfg(test)]"))
        .map(|zero_based| zero_based + 1)
    else {
        return false;
    };
    let Some(path_str) = path.to_str() else {
        return false;
    };
    let Ok(output) = std::process::Command::new("git")
        .current_dir(repo)
        .args(["diff", "--unified=0", "HEAD", "--", path_str])
        .output()
    else {
        return false;
    };
    if !output.status.success() {
        return false;
    }
    diff_touches_line_at_or_after(&String::from_utf8_lossy(&output.stdout), test_line)
}

/// Q1 (blind-review completion quality): the Stop-hook supervision check for
/// headless sessions. A blind reviewer scoring 24 headless runs made the same
/// deduction on ~70% of them regardless of condition -- "the agent added no
/// tests of its own for the change" -- even though zirv's own engineering
/// standard already asks for one focused test per behaviour change plus the
/// unhappy path. This makes a headless session that skips it stop with a
/// concrete reason to fix that, exactly once, rather than relying on the
/// prompt alone.
///
/// `None` on any doubt at all -- like every other Stop-hook advisory in this
/// file, a supervision failure here is pure passthrough, never a reason to
/// fail the hook or the session (CLAUDE.md: "supervision failure is
/// passthrough"). Fires only when `cfg.missing_tests_gate.enabled`, only for
/// a HEADLESS session (`adapters::HEADLESS_ENV == "1"` -- an interactive
/// session, which never sets it, is never blocked by this), and only once
/// per session (the persisted [`MissingTestsGateRecord`] -- a later call
/// that finds `blocked` already `true` returns `None` regardless of what
/// changed since). F6 (codex review fix): "session" here means the whole
/// supervised run, so `stable_short` is keyed on, not the rotating
/// `SESSION_ENV`/`payload.session_id` -- see [`missing_tests_gate_record_path`]'s
/// own doc comment.
///
/// F6 residual (review round 2): `stable_short` is `short_id(&payload.
/// session_id)` (`sessions::short_id`, ASCII-alphanumeric only) whenever no
/// socket was ever bound -- an unsupervised/`--no-supervise` launch, or the
/// codex `Notify` path -- and `short_id` degrades to the EMPTY string for a
/// session id that is itself empty or carries no ASCII-alphanumeric
/// character at all. Keying `missing_tests_gate_record_path` on that empty
/// string would hash every such identity-less session onto the SAME record,
/// letting one unrelated session's `blocked = true` silently suppress the
/// gate for every other one. `raw_session_id` (`payload.session_id`,
/// unfiltered -- `input_hash` hashes arbitrary UTF-8 bytes, not just ASCII)
/// is the fallback key when `stable_short` is empty; when BOTH are empty
/// there is no identity to key a shared, persisted record on at all, so the
/// gate is skipped outright -- never blocks, never reads or writes a record
/// -- rather than risk a cross-session collision. Supervision must never
/// worsen a session.
pub(super) fn missing_tests_gate_reason(
    state: &StateDir,
    repo: &Path,
    stable_short: &str,
    raw_session_id: &str,
    cfg: &CtxConfig,
    env: EnvLookup<'_>,
) -> Option<String> {
    if !cfg.missing_tests_gate.enabled {
        return None;
    }
    if env(adapters::HEADLESS_ENV).as_deref() != Some("1") {
        return None;
    }
    let gate_key = if !stable_short.is_empty() {
        stable_short
    } else if !raw_session_id.is_empty() {
        raw_session_id
    } else {
        return None;
    };
    let path = missing_tests_gate_record_path(state, gate_key);
    if load_missing_tests_gate_record(&path).blocked {
        return None;
    }
    // Any doubt here (no git, no repo, ...) reads as "nothing changed": a
    // block is a hard stop, never worth risking on an unreadable repo state.
    let changed = verification::changed_paths(repo).ok()?;
    let mut has_test_change = false;
    let mut has_non_test_source_change = false;
    for candidate in &changed {
        let is_test = path_looks_like_test_file(candidate)
            || (candidate.extension().and_then(|ext| ext.to_str()) == Some("rs")
                && rust_change_touches_cfg_test(repo, candidate));
        if is_test {
            has_test_change = true;
        } else if !crate::commands::ctx::lifecycle::changes_are_doc_only(std::slice::from_ref(
            candidate,
        )) {
            has_non_test_source_change = true;
        }
    }
    if !has_non_test_source_change || has_test_change {
        return None;
    }
    // Issue 6a (`[jev] missing_tests`, off by default): a decisive "not
    // owed" answer skips this ONE block without ever persisting it as
    // blocked -- the record stays untouched, so a later, still-test-less
    // turn in the same session can still be asked/blocked. Everything else
    // (gate off, no credential, indecisive, an error, or a decisive but not
    // strongly "not owed" answer) blocks exactly as the deterministic gate
    // already does above.
    if missing_tests_owed_jev_says_skip(state, cfg, repo, &changed) {
        return None;
    }
    let mut record = load_missing_tests_gate_record(&path);
    record.version = MISSING_TESTS_GATE_RECORD_VERSION;
    record.blocked = true;
    save_missing_tests_gate_record(&path, &record);
    Some(
        "zirv: this turn edited source files with no test of its own -- add a focused test for \
         each behaviour change the request asks for, asserting the exact formats, orderings and \
         messages it states and the invalid-input/unhappy path, then run the test suite and \
         finish."
            .to_string(),
    )
}

// -- Issue 6a: `[jev] missing_tests` -----------------------------------------

/// At most this many tracked test files actually READ for the missing-
/// tests-owed Jev gate's own "mentions" fact -- keeps a huge test suite from
/// turning one Stop-hook call into an unbounded scan.
const MISSING_TESTS_OWED_TEST_SCAN_CAP: usize = 50;

/// The noul-probability floor a decisive answer must sit AT OR BELOW before
/// `missing_tests_owed_jev_says_skip` treats it as "not owed" and skips the
/// deterministic block. Mirrors [`STOP_VERIFY_MIN_PROBABILITY`]'s own
/// conservative stance but inverted, and if anything stricter: skipping a
/// real gate is riskier than one extra (already rare, once-per-session)
/// false block, so only a strong "not owed" signal -- never merely "leaning
/// no" -- may skip it.
const MISSING_TESTS_OWED_SKIP_MAX_PROBABILITY: f64 = 0.1;

/// [`missing_tests_owed_jev_says_skip`]'s own default `(min_confidence,
/// min_margin)` `decisive()` floor -- named (issue: `zirv ctx jev probe`) so
/// a later retune targets exactly this constant, the same way every other
/// tunable site's default floor is now named. Not routed through `jev::
/// floor`/`[jev.floors]` today: this stays the same fixed pair production
/// has always used.
pub(crate) const MISSING_TESTS_DEFAULT_FLOOR: (f32, f32) =
    (0.0, crate::commands::ctx::jev::DEFAULT_MIN_MARGIN);

/// Every tracked path `git ls-files` reports that itself looks like a test
/// file ([`path_looks_like_test_file`]) -- the missing-tests-owed Jev gate's
/// own "does this repo even have tests" and "mentions" facts both read off
/// this same listing. Empty on any doubt at all (git failure, no repo).
fn missing_tests_owed_tracked_test_paths(repo: &Path) -> Vec<PathBuf> {
    let Ok(output) = std::process::Command::new("git")
        .args(["ls-files", "-z"])
        .current_dir(repo)
        .output()
    else {
        return Vec::new();
    };
    if !output.status.success() {
        return Vec::new();
    }
    output
        .stdout
        .split(|&byte| byte == 0)
        .filter(|chunk| !chunk.is_empty())
        .filter_map(|chunk| std::str::from_utf8(chunk).ok())
        .map(PathBuf::from)
        .filter(|path| path_looks_like_test_file(path))
        .collect()
}

/// Total added+removed lines across the working tree's own uncommitted
/// changes (`git diff --numstat HEAD`) -- the missing-tests-owed Jev gate's
/// own "how big is this change" fact. `0` on any doubt (git failure): the
/// gate simply falls back to the coarsest bucket, never a hook failure.
fn missing_tests_owed_changed_lines(repo: &Path) -> usize {
    let Ok(output) = std::process::Command::new("git")
        .args(["diff", "--numstat", "HEAD"])
        .current_dir(repo)
        .output()
    else {
        return 0;
    };
    if !output.status.success() {
        return 0;
    }
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| {
            let mut fields = line.splitn(3, '\t');
            let added: usize = fields.next()?.parse().ok()?;
            let removed: usize = fields.next()?.parse().ok()?;
            Some(added + removed)
        })
        .sum()
}

/// How many of `test_paths` (capped at [`MISSING_TESTS_OWED_TEST_SCAN_CAP`]
/// files actually read) contain a plain substring match for at least one of
/// `stems` (a changed non-test file's own file stem, e.g. `score` for
/// `score.rs`) -- a cheap, deliberately approximate stand-in for "an
/// existing test already imports/mentions this module". `0` when `stems` is
/// empty or no test file matches.
fn missing_tests_owed_mentions(repo: &Path, test_paths: &[PathBuf], stems: &[String]) -> usize {
    if stems.is_empty() {
        return 0;
    }
    test_paths
        .iter()
        .take(MISSING_TESTS_OWED_TEST_SCAN_CAP)
        .filter(|path| {
            std::fs::read_to_string(repo.join(path))
                .ok()
                .is_some_and(|text| stems.iter().any(|stem| text.contains(stem.as_str())))
        })
        .count()
}

/// The missing-tests-owed Jev gate's own local, numeric-only facts, folded
/// from the SAME `changed` list the deterministic gate above already
/// computed (never a fresh `changed_paths` re-query): `[non-test source
/// files changed, changed-lines bucket (0 <10, 1 <100, 2 <500, 3 larger),
/// whether the repo has any test files at all (0/1), how many existing test
/// files mention a changed module's own name, doc-only share of the change
/// (0-4, quarters)]`. Every cell a small capped integer -- never a path,
/// filename, or file content.
fn missing_tests_owed_facts(repo: &Path, changed: &[PathBuf]) -> Vec<u32> {
    let non_test_source: Vec<&PathBuf> = changed
        .iter()
        .filter(|path| {
            !path_looks_like_test_file(path)
                && !crate::commands::ctx::lifecycle::changes_are_doc_only(std::slice::from_ref(
                    path,
                ))
        })
        .collect();
    let doc_only_count = changed
        .iter()
        .filter(|path| {
            crate::commands::ctx::lifecycle::changes_are_doc_only(std::slice::from_ref(path))
        })
        .count();
    let lines_bucket = match missing_tests_owed_changed_lines(repo) {
        0..10 => 0,
        10..100 => 1,
        100..500 => 2,
        _ => 3,
    };
    let test_paths = missing_tests_owed_tracked_test_paths(repo);
    let has_tests = u32::from(!test_paths.is_empty());
    let stems: Vec<String> = non_test_source
        .iter()
        .filter_map(|path| path.file_stem().and_then(|stem| stem.to_str()))
        .map(str::to_string)
        .collect();
    let mentions = missing_tests_owed_mentions(repo, &test_paths, &stems);
    let doc_only_share = if changed.is_empty() {
        0
    } else {
        capped_u32(doc_only_count.saturating_mul(4) / changed.len())
    };
    vec![
        capped_u32(non_test_source.len()),
        lines_bucket,
        has_tests,
        capped_u32(mentions),
        doc_only_share,
    ]
}

pub(crate) fn missing_tests_questions() -> [crate::commands::ctx::jev::Question; 1] {
    [crate::commands::ctx::jev::Question::metadata_noul(
        "tests_owed",
        // Kept under `safe_metadata_request`'s own 512-char instructions cap
        // (checked once by `stop_verify_request_passes_the_metadata_guard`'s
        // own sibling test below): an oversized instructions string fails
        // that guard and `ask` never even reaches the network, which reads
        // as a silent, permanent no-op for this whole gate.
        "Facts [non-test source files changed, changed-lines bucket (0 <10, 1 <100, 2 <500, 3 \
larger), repo has test files (0/1), test files mentioning a changed module, doc-only share (0-4, \
quarters)] describe an uncommitted change with no test update. Is a new test owed? Answer false \
only if clearly not owed. Answer true if unsure.",
        "a new or updated test is owed for this change",
        "no new test is owed for this change",
    )]
}

/// [`missing_tests_owed_jev_says_skip`]'s own per-call decision: `"skip"`
/// only for a DECISIVE, strongly "not owed" noul (at or below
/// [`MISSING_TESTS_OWED_SKIP_MAX_PROBABILITY`]), `"owed"` otherwise (missing
/// answer, indecisive, unparseable, or a decisive answer that is not
/// strongly "not owed") -- the deterministic gate's own fallback outcome.
/// Shared with `zirv ctx jev probe`, which reports exactly this outcome.
pub(crate) fn missing_tests_action(
    answer: Option<&crate::commands::ctx::jev::Answer>,
    min_confidence: f32,
    min_margin: f32,
) -> &'static str {
    let Some(answer) = answer else {
        return "owed";
    };
    if !answer.decisive(min_confidence, min_margin) {
        return "owed";
    }
    match answer.as_noul() {
        Some(probability) if probability <= MISSING_TESTS_OWED_SKIP_MAX_PROBABILITY => "skip",
        _ => "owed",
    }
}

/// Issue 6a: `[jev] missing_tests` (off by default). Asks Jev one metadata-
/// only Noul question from [`missing_tests_owed_facts`] and returns `true`
/// only for a DECISIVE, strongly "not owed" answer (at or below
/// [`MISSING_TESTS_OWED_SKIP_MAX_PROBABILITY`]) -- the one case
/// `missing_tests_gate_reason` reads as "skip this block". `false` on every
/// other outcome (the key off, no `[proxy.typesafe]` credential, no answer,
/// an indecisive answer, or a decisive answer that is not strongly "not
/// owed"): the deterministic gate then blocks exactly as it always has.
fn missing_tests_owed_jev_says_skip(
    state: &StateDir,
    cfg: &CtxConfig,
    repo: &Path,
    changed: &[PathBuf],
) -> bool {
    if !cfg.jev.missing_tests || !crate::commands::ctx::jev::available(&cfg.proxy.typesafe) {
        return false;
    }
    let facts = missing_tests_owed_facts(repo, changed);
    let advise_state = DispatchAdviseState {
        metadata_only: true,
        facts: vec![facts],
    };
    let Some(answers) = crate::commands::ctx::jev::advise(
        cfg,
        state,
        "missing_tests",
        cfg.jev.missing_tests,
        &advise_state,
        &missing_tests_questions(),
    ) else {
        return false;
    };
    let answer = answers.get("tests_owed");
    let (min_confidence, min_margin) = MISSING_TESTS_DEFAULT_FLOOR;
    if missing_tests_action(answer, min_confidence, min_margin) != "skip" {
        return false;
    }
    let effect = crate::commands::ctx::jev::JevEffect::new("missing_tests", "gate_skipped");
    crate::commands::ctx::jev::record_effect(cfg, state, cfg.jev.missing_tests, &effect);
    true
}

#[cfg(test)]
mod tests {
    use super::super::pretool_tier::DispatchAdviseState;
    use super::super::tests::git_repo;
    use super::*;

    // -- Q1: missing-tests Stop-hook gate -----------------------------------

    #[test]
    fn path_looks_like_test_file_matches_the_documented_patterns() {
        for path in [
            "tests/foo.rs",
            "src/test/bar.py",
            "Tests/Bar.cs",
            "test_widget.py",
            "widget_test.go",
            "widget.test.ts",
        ] {
            assert!(
                path_looks_like_test_file(Path::new(path)),
                "{path} should be recognised as a test file"
            );
        }
        for path in ["src/lib.rs", "README.md", "src/testing_helpers.rs"] {
            assert!(
                !path_looks_like_test_file(Path::new(path)),
                "{path} should not be recognised as a test file"
            );
        }
    }

    #[test]
    fn hunk_new_start_parses_standard_and_single_line_headers() {
        assert_eq!(hunk_new_start("@@ -1,2 +3,4 @@ fn main() {"), Some(3));
        assert_eq!(hunk_new_start("@@ -1 +7 @@"), Some(7));
        assert_eq!(hunk_new_start("not a hunk header"), None);
    }

    #[test]
    fn diff_touches_line_at_or_after_is_true_only_at_or_past_the_marker() {
        let diff = "@@ -1,0 +1,2 @@\n+a\n+b\n";
        assert!(diff_touches_line_at_or_after(diff, 1));
        assert!(!diff_touches_line_at_or_after(diff, 5));
    }

    /// Commits whatever is currently in the working tree, on top of
    /// `git_repo()`'s own base commit -- `rust_change_touches_cfg_test` reads
    /// `git diff HEAD`, which shows nothing for an untracked file, so these
    /// two tests need a real baseline commit before their "edit" write.
    fn commit_all(repo: &Path, message: &str) {
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
                .current_dir(repo)
                .status()
                .expect("run git");
            assert!(status.success(), "git {args:?} failed");
        };
        git(&["add", "."]);
        git(&["commit", "-q", "-m", message]);
    }

    /// A change to the bottom-of-file `#[cfg(test)] mod tests` block (this
    /// repo's own convention) is recognised as a test change even though the
    /// path itself is an ordinary `.rs` source file.
    #[test]
    fn rust_change_touches_cfg_test_is_true_for_a_change_inside_the_test_module() {
        let repo = git_repo();
        let path = repo.path().join("lib.rs");
        std::fs::write(
            &path,
            "pub fn add(a: i32, b: i32) -> i32 {\n    a + b\n}\n\n#[cfg(test)]\nmod tests {\n    use super::*;\n\n    #[test]\n    fn adds() {\n        assert_eq!(add(1, 1), 2);\n    }\n}\n",
        )
        .expect("write");
        commit_all(repo.path(), "add lib.rs");
        std::fs::write(
            &path,
            "pub fn add(a: i32, b: i32) -> i32 {\n    a + b\n}\n\n#[cfg(test)]\nmod tests {\n    use super::*;\n\n    #[test]\n    fn adds() {\n        assert_eq!(add(1, 1), 2);\n    }\n\n    #[test]\n    fn adds_negative() {\n        assert_eq!(add(-1, -1), -2);\n    }\n}\n",
        )
        .expect("write");
        assert!(rust_change_touches_cfg_test(
            repo.path(),
            Path::new("lib.rs")
        ));
    }

    /// A change above the `#[cfg(test)]` line -- to the production code, not
    /// the test module -- must not be mistaken for a test change just
    /// because the file happens to carry one.
    #[test]
    fn rust_change_touches_cfg_test_is_false_for_a_change_above_the_test_module() {
        let repo = git_repo();
        let path = repo.path().join("lib.rs");
        std::fs::write(
            &path,
            "pub fn add(a: i32, b: i32) -> i32 {\n    a + b\n}\n\n#[cfg(test)]\nmod tests {\n    use super::*;\n\n    #[test]\n    fn adds() {\n        assert_eq!(add(1, 1), 2);\n    }\n}\n",
        )
        .expect("write");
        commit_all(repo.path(), "add lib.rs");
        std::fs::write(
            &path,
            "pub fn add(a: i32, b: i32) -> i32 {\n    a + b + 0\n}\n\n#[cfg(test)]\nmod tests {\n    use super::*;\n\n    #[test]\n    fn adds() {\n        assert_eq!(add(1, 1), 2);\n    }\n}\n",
        )
        .expect("write");
        assert!(!rust_change_touches_cfg_test(
            repo.path(),
            Path::new("lib.rs")
        ));
    }

    /// Behaviour 1: a headless session that edited a non-test source file and
    /// touched no test file blocks once, with a reason naming tests.
    #[test]
    fn missing_tests_gate_blocks_once_when_source_is_edited_without_tests() {
        let dir = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(dir.path().to_path_buf());
        let repo = git_repo();
        std::fs::write(repo.path().join("src.rs"), "fn main() {}\n").expect("write");
        let cfg = CtxConfig::default();
        let env: std::collections::HashMap<String, String> =
            [(adapters::HEADLESS_ENV.to_string(), "1".to_string())].into();

        let reason =
            missing_tests_gate_reason(&state, repo.path(), "sess-q1-a", "sess-q1-a", &cfg, &|k| {
                env.get(k).cloned()
            })
            .expect("non-test source change with no test change must block");
        assert!(reason.contains("test"), "{reason}");
    }

    /// Behaviour 2: a session that also touched a test file for the same
    /// change is never blocked.
    #[test]
    fn missing_tests_gate_does_not_block_when_tests_are_also_edited() {
        let dir = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(dir.path().to_path_buf());
        let repo = git_repo();
        std::fs::write(repo.path().join("src.rs"), "fn main() {}\n").expect("write");
        std::fs::create_dir_all(repo.path().join("tests")).expect("mkdir");
        std::fs::write(repo.path().join("tests/src_test.rs"), "// test\n").expect("write");
        let cfg = CtxConfig::default();
        let env: std::collections::HashMap<String, String> =
            [(adapters::HEADLESS_ENV.to_string(), "1".to_string())].into();

        assert_eq!(
            missing_tests_gate_reason(&state, repo.path(), "sess-q1-b", "sess-q1-b", &cfg, &|k| {
                env.get(k).cloned()
            }),
            None,
            "a test file changed alongside the source change must not block"
        );
    }

    /// Behaviour 3: once blocked, the same session never blocks again, even
    /// though nothing about the (still test-less) change set changed.
    #[test]
    fn missing_tests_gate_never_blocks_twice_in_the_same_session() {
        let dir = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(dir.path().to_path_buf());
        let repo = git_repo();
        std::fs::write(repo.path().join("src.rs"), "fn main() {}\n").expect("write");
        let cfg = CtxConfig::default();
        let env: std::collections::HashMap<String, String> =
            [(adapters::HEADLESS_ENV.to_string(), "1".to_string())].into();
        let lookup = |k: &str| env.get(k).cloned();

        assert!(
            missing_tests_gate_reason(&state, repo.path(), "sess-q1-c", "sess-q1-c", &cfg, &lookup)
                .is_some(),
            "first stop with missing tests must block"
        );
        assert_eq!(
            missing_tests_gate_reason(&state, repo.path(), "sess-q1-c", "sess-q1-c", &cfg, &lookup),
            None,
            "a second stop in the same session must never block again"
        );
    }

    /// Behaviour 4: an interactive session (no `ZIRV_CTX_HEADLESS=1`) is
    /// never blocked, even with the identical qualifying change set.
    #[test]
    fn missing_tests_gate_does_not_block_interactive_sessions() {
        let dir = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(dir.path().to_path_buf());
        let repo = git_repo();
        std::fs::write(repo.path().join("src.rs"), "fn main() {}\n").expect("write");
        let cfg = CtxConfig::default();

        assert_eq!(
            missing_tests_gate_reason(&state, repo.path(), "sess-q1-d", "sess-q1-d", &cfg, &|_| {
                None
            }),
            None,
            "an interactive session (no ZIRV_CTX_HEADLESS=1) must never block"
        );
    }

    /// F6 residual (review round 2): with NEITHER `stable_short` nor the raw
    /// session id carrying any identity (both empty -- an unsupervised
    /// launch whose session id is itself empty or has no ASCII-alphanumeric
    /// character at all), the gate must skip entirely: never block, and
    /// never persist a record at the shared empty-string hash -- otherwise
    /// one such identity-less session's block would silently suppress the
    /// gate for every other one.
    #[test]
    fn missing_tests_gate_skips_entirely_with_no_identity_at_all() {
        let dir = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(dir.path().to_path_buf());
        let repo = git_repo();
        std::fs::write(repo.path().join("src.rs"), "fn main() {}\n").expect("write");
        let cfg = CtxConfig::default();
        let env: std::collections::HashMap<String, String> =
            [(adapters::HEADLESS_ENV.to_string(), "1".to_string())].into();

        assert_eq!(
            missing_tests_gate_reason(&state, repo.path(), "", "", &cfg, &|k| {
                env.get(k).cloned()
            }),
            None,
            "no identity at all must never block"
        );
        let collision_path = missing_tests_gate_record_path(&state, "");
        assert!(
            !collision_path.exists(),
            "a no-identity session must never persist a record at the shared empty-key path"
        );
    }

    /// F6 residual (review round 2): when `stable_short` is empty (no
    /// socket bound and the rotating session id filtered to nothing) but the
    /// raw session id is non-empty, the gate keys on the raw id instead of
    /// collapsing every such session onto the shared empty-string record --
    /// two different raw ids get two independent records, neither inheriting
    /// the other's block.
    #[test]
    fn missing_tests_gate_falls_back_to_the_raw_session_id_when_stable_short_is_empty() {
        let dir = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(dir.path().to_path_buf());
        let repo = git_repo();
        std::fs::write(repo.path().join("src.rs"), "fn main() {}\n").expect("write");
        let cfg = CtxConfig::default();
        let env: std::collections::HashMap<String, String> =
            [(adapters::HEADLESS_ENV.to_string(), "1".to_string())].into();
        let lookup = |k: &str| env.get(k).cloned();

        assert!(
            missing_tests_gate_reason(&state, repo.path(), "", "raw-session-one", &cfg, &lookup)
                .is_some(),
            "the raw session id must still key a real, blockable identity"
        );
        assert!(
            missing_tests_gate_reason(&state, repo.path(), "", "raw-session-two", &cfg, &lookup)
                .is_some(),
            "an unrelated raw session id must not inherit another session's own block"
        );
    }

    /// The operator-configurable toggle: off means never blocked, even for an
    /// otherwise-qualifying headless change set.
    #[test]
    fn missing_tests_gate_is_silent_when_disabled() {
        let dir = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(dir.path().to_path_buf());
        let repo = git_repo();
        std::fs::write(repo.path().join("src.rs"), "fn main() {}\n").expect("write");
        let mut cfg = CtxConfig::default();
        cfg.missing_tests_gate.enabled = false;
        let env: std::collections::HashMap<String, String> =
            [(adapters::HEADLESS_ENV.to_string(), "1".to_string())].into();

        assert_eq!(
            missing_tests_gate_reason(&state, repo.path(), "sess-q1-e", "sess-q1-e", &cfg, &|k| {
                env.get(k).cloned()
            }),
            None
        );
    }

    /// A doc-only change set (the same exemption `verify_on_stop_nudge`
    /// already uses) has no source change to demand a test for.
    #[test]
    fn missing_tests_gate_does_not_block_a_doc_only_change_set() {
        let dir = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(dir.path().to_path_buf());
        let repo = git_repo();
        std::fs::write(repo.path().join("README.md"), "docs\n").expect("write");
        let cfg = CtxConfig::default();
        let env: std::collections::HashMap<String, String> =
            [(adapters::HEADLESS_ENV.to_string(), "1".to_string())].into();

        assert_eq!(
            missing_tests_gate_reason(&state, repo.path(), "sess-q1-f", "sess-q1-f", &cfg, &|k| {
                env.get(k).cloned()
            }),
            None
        );
    }

    // -- Issue 6a: `[jev] missing_tests` -------------------------------------

    fn missing_tests_owed_cfg(base_url: String, credential_env: &str) -> CtxConfig {
        let mut cfg = CtxConfig::default();
        cfg.jev.missing_tests = true;
        cfg.jev.cache_ttl_secs = 0;
        cfg.proxy.typesafe.base_url = base_url;
        cfg.proxy.typesafe.credential_env = credential_env.to_string();
        cfg.proxy.typesafe.timeout_secs = 5;
        cfg
    }

    const TESTS_OWED_FALSE: &str = r#"{"model": "jev-latest", "answers": {
        "tests_owed": {"type": "noul", "noul": 0.02}},
        "usage": {"input_tokens": 5, "output_tokens": 0}}"#;

    const TESTS_OWED_INDECISIVE: &str = r#"{"model": "jev-latest", "answers": {
        "tests_owed": {"type": "noul", "noul": 0.5}},
        "usage": {"input_tokens": 5, "output_tokens": 0}}"#;

    /// Guards against the exact bug this gate shipped with once already: an
    /// oversized `instructions` string fails `safe_metadata_request` and
    /// `ask` never even reaches the network -- a silent, permanent no-op for
    /// the whole gate that no other test here would ever catch, since every
    /// mocked-server test below only proves the RESPONSE path.
    #[test]
    fn missing_tests_owed_request_passes_the_metadata_guard() {
        let advise_state = DispatchAdviseState {
            metadata_only: true,
            facts: vec![vec![1_000_000; 5]],
        };
        let value = serde_json::to_value(&advise_state).expect("json");
        assert!(crate::commands::ctx::jev::safe_metadata_request(
            &value,
            &missing_tests_questions(),
            "jev-latest"
        ));
    }

    fn missing_tests_noul_answer(probability: f64) -> crate::commands::ctx::jev::Answer {
        crate::commands::ctx::jev::Answer {
            value: crate::commands::ctx::jev::AnswerValue::Noul(probability),
            confidence: probability as f32,
            probabilities: std::collections::BTreeMap::new(),
        }
    }

    /// A decisive answer at or below [`MISSING_TESTS_OWED_SKIP_MAX_PROBABILITY`]
    /// skips the gate; the same answer one step above the value threshold
    /// falls back to "owed" -- proves the `<=` edge, not just a comfortably
    /// -clear case.
    #[test]
    fn missing_tests_action_decides_on_the_probability_edge() {
        let (min_confidence, min_margin) = MISSING_TESTS_DEFAULT_FLOOR;
        let at_floor = missing_tests_noul_answer(MISSING_TESTS_OWED_SKIP_MAX_PROBABILITY);
        assert_eq!(
            missing_tests_action(Some(&at_floor), min_confidence, min_margin),
            "skip"
        );
        let just_above = missing_tests_noul_answer(MISSING_TESTS_OWED_SKIP_MAX_PROBABILITY + 0.01);
        assert_eq!(
            missing_tests_action(Some(&just_above), min_confidence, min_margin),
            "owed"
        );
        assert_eq!(
            missing_tests_action(None, min_confidence, min_margin),
            "owed"
        );
    }

    /// Gate off (the default): behaviour is byte-identical to before this
    /// feature existed -- the deterministic gate still blocks, and no Jev
    /// call is ever made (a bad/unreachable `base_url` would otherwise hang
    /// or fail this test).
    #[test]
    fn missing_tests_gate_unaffected_when_jev_missing_tests_is_off() {
        let dir = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(dir.path().to_path_buf());
        let repo = git_repo();
        std::fs::write(repo.path().join("src.rs"), "fn main() {}\n").expect("write");
        let cfg = CtxConfig::default();
        let env: std::collections::HashMap<String, String> =
            [(adapters::HEADLESS_ENV.to_string(), "1".to_string())].into();

        let reason = missing_tests_gate_reason(
            &state,
            repo.path(),
            "sess-jev-a",
            "sess-jev-a",
            &cfg,
            &|k| env.get(k).cloned(),
        );
        assert!(
            reason.is_some(),
            "gate off must behave exactly as before (still blocks): {reason:?}"
        );
    }

    /// A decisive, strongly "not owed" answer skips the block, and the skip
    /// is never persisted as a block -- a later, still-test-less turn in the
    /// same session can still be asked/blocked.
    #[test]
    fn missing_tests_gate_skips_the_block_on_a_decisive_not_owed_jev_answer() {
        let (url, handle) =
            crate::commands::ctx::jev::tests::one_shot_server(200, TESTS_OWED_FALSE);
        let dir = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(dir.path().to_path_buf());
        let repo = git_repo();
        std::fs::write(repo.path().join("src.rs"), "fn main() {}\n").expect("write");
        let env_name = "HOOK_TEST_MISSING_TESTS_JEV_SKIP";
        // SAFETY (test-only): a unique env var name this test owns.
        unsafe { std::env::set_var(env_name, "secret") };
        let cfg = missing_tests_owed_cfg(url, env_name);
        let env: std::collections::HashMap<String, String> =
            [(adapters::HEADLESS_ENV.to_string(), "1".to_string())].into();

        let reason = missing_tests_gate_reason(
            &state,
            repo.path(),
            "sess-jev-b",
            "sess-jev-b",
            &cfg,
            &|k| env.get(k).cloned(),
        );
        unsafe { std::env::remove_var(env_name) };
        handle.join().expect("server thread");
        assert_eq!(
            reason, None,
            "a decisive not-owed answer must skip the block"
        );
        assert!(
            !load_missing_tests_gate_record(&missing_tests_gate_record_path(&state, "sess-jev-b"))
                .blocked,
            "a skipped block must never be persisted as blocked"
        );
    }

    /// An indecisive answer (thin margin) blocks exactly as today.
    #[test]
    fn missing_tests_gate_still_blocks_on_an_indecisive_jev_answer() {
        let (url, handle) =
            crate::commands::ctx::jev::tests::one_shot_server(200, TESTS_OWED_INDECISIVE);
        let dir = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(dir.path().to_path_buf());
        let repo = git_repo();
        std::fs::write(repo.path().join("src.rs"), "fn main() {}\n").expect("write");
        let env_name = "HOOK_TEST_MISSING_TESTS_JEV_INDECISIVE";
        // SAFETY (test-only): a unique env var name this test owns.
        unsafe { std::env::set_var(env_name, "secret") };
        let cfg = missing_tests_owed_cfg(url, env_name);
        let env: std::collections::HashMap<String, String> =
            [(adapters::HEADLESS_ENV.to_string(), "1".to_string())].into();

        let reason = missing_tests_gate_reason(
            &state,
            repo.path(),
            "sess-jev-c",
            "sess-jev-c",
            &cfg,
            &|k| env.get(k).cloned(),
        );
        unsafe { std::env::remove_var(env_name) };
        handle.join().expect("server thread");
        assert!(
            reason.is_some(),
            "an indecisive answer must block exactly as today: {reason:?}"
        );
    }
}
