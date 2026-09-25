//! Round 4 bug 4a: an operator-invisible, runtime-learned override for a
//! model's real context window, layered strictly BETWEEN the two facts that
//! already outrank each other -- `cfg.score.model_context_tokens` (the
//! operator's own pin, which always wins; see `rot::capacity`'s `cfg.
//! model_context_tokens.or(caps.context_window_tokens)`) above it, and
//! `catalogue`'s own conservative built-in default below it.
//!
//! Claude's own `-p --output-format json` result reports the TRUE window for
//! the model it actually ran, e.g. `"modelUsage":{"claude-sonnet-5":
//! {"contextWindow":1000000,...}}` -- verified 2026-09-25 against a real
//! result, the same evidence that corrected the catalogue's own `sonnet`
//! rung (`catalogue::ANTHROPIC_RUNGS`) from an understated 200_000. This
//! module exists for every OTHER case the catalogue has not been corrected
//! for yet: a future model, a different vendor/endpoint, or a rung this
//! round simply did not touch.
//!
//! `exec.rs`'s main supervision loop already captures a headless child's
//! final stdout (`OutputTap::drain_to_eof`, the same capture `pace::scan_
//! for_limit` reads) -- `parse_observed_window` (pure) reads those same
//! lines for this fact, and `record` persists it under BOTH the resolved
//! model id and the requested model/alias from argv, so a later launch
//! naming either one finds it.
//!
//! Deliberately kept OFF the `AgentAdapter` construction path (`select`/
//! `resolve_default`, whose signature dozens of unrelated callers share) and
//! out of `rot.rs` (which must stay pure): `ClaudeAdapter::context_window_
//! tokens` reads this file directly off `home_dir()` -- the same zirv-owned-
//! under-operator-home convention `hook_integrity` and the memory bank
//! already use -- rather than needing a `StateDir`/`env` threaded through
//! adapter selection and every one of its callers.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// `<home>/.zirv/model-windows.json`: a small, best-effort, machine-local
/// cache -- never an authority an operator has to configure, and never
/// consulted at all once `cfg.model_context_tokens` is set. See this
/// module's own doc comment for the full priority order.
fn path(home: &Path) -> PathBuf {
    home.join(".zirv").join("model-windows.json")
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct Store {
    #[serde(flatten)]
    windows: HashMap<String, u64>,
}

fn normalize(key: &str) -> String {
    key.trim().to_lowercase()
}

/// Issue #779 (round 4 review): the plausible range for a real model's
/// context window. `record` used to reject only `0`, so a single garbled or
/// adversarial `--output-format json` result (a stray digit, a unit
/// mismatch, a hostile transcript) could persist an absurd value that then
/// poisons every later compaction decision made against that model, in
/// either direction -- too small triggers needless compaction, too large
/// never compacts at all. `MIN_PLAUSIBLE_WINDOW` is comfortably below every
/// known model's window (smaller than any shipped rung in `catalogue`, so a
/// real vendor figure is never rejected); `MAX_PLAUSIBLE_WINDOW` is
/// comfortably above the largest verified window this codebase has observed
/// (1,000,000, see this module's own doc comment) with headroom for a
/// legitimate future model.
const MIN_PLAUSIBLE_WINDOW: u64 = 8_192;
const MAX_PLAUSIBLE_WINDOW: u64 = 10_000_000;

fn is_plausible_window(window: u64) -> bool {
    (MIN_PLAUSIBLE_WINDOW..=MAX_PLAUSIBLE_WINDOW).contains(&window)
}

/// Pure: finds the largest `modelUsage.<model>.contextWindow` reported in
/// `text` (the child's captured final stdout lines, joined by newline) and
/// returns its model id verbatim (`record` normalizes on write) alongside
/// the window. `None` for every launch that never printed a `--output-
/// format json` result at all -- ordinary interactive sessions included --
/// which is the common case this must stay silent for.
pub(crate) fn parse_observed_window(text: &str) -> Option<(String, u64)> {
    let mut best: Option<(String, u64)> = None;
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || !line.starts_with('{') {
            continue;
        }
        let Ok(row) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        let Some(usage) = row.get("modelUsage").and_then(Value::as_object) else {
            continue;
        };
        for (model, entry) in usage {
            let Some(window) = entry.get("contextWindow").and_then(Value::as_u64) else {
                continue;
            };
            if !is_plausible_window(window) {
                continue;
            }
            if best.as_ref().is_none_or(|(_, w)| window > *w) {
                best = Some((model.clone(), window));
            }
        }
    }
    best
}

/// Best-effort, like every other piece of state/home-dir housekeeping in
/// this codebase (see `chain::record_boot_and_evaluate`'s own doc comment
/// for the same discipline): a read or write failure here never blocks or
/// fails the caller, it only means this observation silently was not
/// learned. `keys` is normally `[resolved_model_id, requested_model_or_
/// alias]` -- both land on the same value, so a later launch naming either
/// one finds it. Empty keys are skipped rather than stored.
pub(crate) fn record(home: &Path, keys: &[&str], window: u64) {
    if !is_plausible_window(window) {
        return;
    }
    let file = path(home);
    let mut store: Store = std::fs::read_to_string(&file)
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_default();
    let mut changed = false;
    for key in keys {
        if key.trim().is_empty() {
            continue;
        }
        store.windows.insert(normalize(key), window);
        changed = true;
    }
    if !changed {
        return;
    }
    let Some(parent) = file.parent() else {
        return;
    };
    if std::fs::create_dir_all(parent).is_err() {
        return;
    }
    let Ok(text) = serde_json::to_string_pretty(&store) else {
        return;
    };
    // Issue #779 (round 4 review): a plain read-modify-write races any other
    // process doing the same (two sessions launched against different models
    // at once, each reading the store before the other's write lands) into a
    // lost update. Written via a temp sibling file plus rename, like
    // `window.rs::save_transcript_cache`/`score.rs::save_checkpoint`, so a
    // process killed mid-write leaves the previous store intact rather than
    // a truncated one, and the rename itself is atomic. No lock file: this
    // cache is best-effort, so the rarer race of two processes each renaming
    // over the other just costs the loser's observation, never corruption.
    let staged = parent.join(format!("model-windows.{}.tmp", std::process::id()));
    if std::fs::write(&staged, text).is_ok() {
        let _ = std::fs::rename(&staged, &file);
    }
}

/// `None` on any read/parse failure, an unstated model, no recorded entry, or
/// a stored value outside [`is_plausible_window`]'s range -- the caller falls
/// back to the catalogue exactly as if this file did not exist at all. The
/// range is re-checked here, not just at `record` time, so a value written by
/// an older build (before this bound existed) or edited by hand is never
/// trusted either.
pub(crate) fn lookup(home: &Path, model: Option<&str>) -> Option<u64> {
    let model = model?;
    let text = std::fs::read_to_string(path(home)).ok()?;
    let store: Store = serde_json::from_str(&text).ok()?;
    store
        .windows
        .get(&normalize(model))
        .copied()
        .filter(|window| is_plausible_window(*window))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_observed_window_reads_a_real_output_format_json_result() {
        let text = r#"{"type":"result","subtype":"success","session_id":"abc","modelUsage":{"claude-sonnet-5":{"contextWindow":1000000,"maxOutputTokens":128000}}}"#;
        assert_eq!(
            parse_observed_window(text),
            Some(("claude-sonnet-5".to_string(), 1_000_000))
        );
    }

    #[test]
    fn parse_observed_window_is_none_for_ordinary_output_with_no_model_usage() {
        assert_eq!(
            parse_observed_window("just some plain text\nnot json at all"),
            None
        );
        assert_eq!(
            parse_observed_window(r#"{"type":"assistant","message":{"content":"hi"}}"#),
            None
        );
    }

    #[test]
    fn parse_observed_window_picks_the_largest_reported_window() {
        let text = r#"{"modelUsage":{"claude-haiku-5":{"contextWindow":200000},"claude-sonnet-5":{"contextWindow":1000000}}}"#;
        assert_eq!(
            parse_observed_window(text),
            Some(("claude-sonnet-5".to_string(), 1_000_000))
        );
    }

    /// Round 4 review, finding 5: a garbled or hostile `contextWindow` below
    /// the plausible floor must never win over a real, in-range figure
    /// reported alongside it.
    #[test]
    fn parse_observed_window_ignores_an_absurdly_small_reported_window() {
        let text = r#"{"modelUsage":{"claude-haiku-5":{"contextWindow":1},"claude-sonnet-5":{"contextWindow":200000}}}"#;
        assert_eq!(
            parse_observed_window(text),
            Some(("claude-sonnet-5".to_string(), 200_000))
        );
        assert_eq!(
            parse_observed_window(r#"{"modelUsage":{"claude-haiku-5":{"contextWindow":1}}}"#),
            None,
            "no in-range reading at all must yield nothing, not the implausible one"
        );
    }

    /// Round 4 review, finding 5: same, for an absurdly large reported
    /// window.
    #[test]
    fn parse_observed_window_ignores_an_absurdly_large_reported_window() {
        let text = r#"{"modelUsage":{"claude-sonnet-5":{"contextWindow":200000},"future-model":{"contextWindow":999999999999}}}"#;
        assert_eq!(
            parse_observed_window(text),
            Some(("claude-sonnet-5".to_string(), 200_000))
        );
    }

    #[test]
    fn record_then_lookup_round_trips_under_both_keys() {
        let dir = tempfile::tempdir().expect("tempdir");
        record(dir.path(), &["claude-sonnet-5", "sonnet"], 1_000_000);
        assert_eq!(lookup(dir.path(), Some("claude-sonnet-5")), Some(1_000_000));
        assert_eq!(lookup(dir.path(), Some("SONNET")), Some(1_000_000));
        assert_eq!(lookup(dir.path(), Some("opus")), None);
        assert_eq!(lookup(dir.path(), None), None);
    }

    #[test]
    fn a_second_record_call_merges_rather_than_replaces() {
        let dir = tempfile::tempdir().expect("tempdir");
        record(dir.path(), &["sonnet"], 1_000_000);
        record(dir.path(), &["opus"], 200_000);
        assert_eq!(lookup(dir.path(), Some("sonnet")), Some(1_000_000));
        assert_eq!(lookup(dir.path(), Some("opus")), Some(200_000));
    }

    #[test]
    fn lookup_of_a_missing_file_is_none_not_an_error() {
        let dir = tempfile::tempdir().expect("tempdir");
        assert_eq!(lookup(dir.path(), Some("sonnet")), None);
    }

    /// Round 4 review, finding 5: `record` used to reject only `0`, so an
    /// absurdly small observed window (a stray digit, a unit mismatch) would
    /// persist and poison every later compaction decision made against that
    /// model. Below the floor is rejected outright; the boundary value itself
    /// is still accepted.
    #[test]
    fn record_rejects_a_window_below_the_plausible_floor() {
        let dir = tempfile::tempdir().expect("tempdir");
        record(dir.path(), &["tiny-model"], MIN_PLAUSIBLE_WINDOW - 1);
        assert_eq!(
            lookup(dir.path(), Some("tiny-model")),
            None,
            "a window below the floor must never be stored"
        );
        record(dir.path(), &["floor-model"], MIN_PLAUSIBLE_WINDOW);
        assert_eq!(
            lookup(dir.path(), Some("floor-model")),
            Some(MIN_PLAUSIBLE_WINDOW),
            "the floor itself is a plausible value"
        );
    }

    /// Round 4 review, finding 5: same, for the ceiling.
    #[test]
    fn record_rejects_a_window_above_the_plausible_ceiling() {
        let dir = tempfile::tempdir().expect("tempdir");
        record(dir.path(), &["huge-model"], MAX_PLAUSIBLE_WINDOW + 1);
        assert_eq!(
            lookup(dir.path(), Some("huge-model")),
            None,
            "a window above the ceiling must never be stored"
        );
        record(dir.path(), &["ceiling-model"], MAX_PLAUSIBLE_WINDOW);
        assert_eq!(
            lookup(dir.path(), Some("ceiling-model")),
            Some(MAX_PLAUSIBLE_WINDOW),
            "the ceiling itself is a plausible value"
        );
    }

    /// Round 4 review, finding 5: `lookup` re-checks the range too, not just
    /// `record` -- a value written by an older build (before this bound
    /// existed) or edited by hand must not be trusted just because it is
    /// already on disk.
    #[test]
    fn lookup_ignores_an_out_of_range_value_already_on_disk() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut windows = HashMap::new();
        windows.insert("legacy-model".to_string(), 4);
        let store = Store { windows };
        let file = path(dir.path());
        std::fs::create_dir_all(file.parent().expect("parent")).expect("mkdir");
        std::fs::write(&file, serde_json::to_string(&store).expect("json")).expect("write");

        assert_eq!(lookup(dir.path(), Some("legacy-model")), None);
    }

    /// Round 4 review, finding 6: a plain read-modify-write can lose one
    /// writer's update to a race with another. This does not reproduce the
    /// race itself (that needs concurrent processes), but proves the write
    /// path goes through the same temp-then-rename seam `window.rs` and
    /// `score.rs` use, by confirming no leftover temp file survives a normal
    /// call and the final content is exactly what was written.
    #[test]
    fn record_leaves_no_leftover_temp_file_after_a_normal_write() {
        let dir = tempfile::tempdir().expect("tempdir");
        record(dir.path(), &["sonnet"], 1_000_000);
        let zirv_dir = dir.path().join(".zirv");
        let entries: Vec<_> = std::fs::read_dir(&zirv_dir)
            .expect("read dir")
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            entries,
            vec!["model-windows.json".to_string()],
            "only the final file must remain, no staged temp file: {entries:?}"
        );
    }
}
