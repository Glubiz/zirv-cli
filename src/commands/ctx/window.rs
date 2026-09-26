use std::path::{Path, PathBuf};
use std::time::SystemTime;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::CtxResult;
use super::state::StateDir;

/// One subscription window as last reported by the collector. `resets_at` is a
/// unix epoch second; `0` means the field was absent and callers must fall back.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Window {
    pub used_percentage: f64,
    pub resets_at: u64,
    pub observed_at: u64,
    /// The vendor explicitly reports that it refused a request against this
    /// window. Additive for compatibility with readings stored before the
    /// codex rollout exposed this signal.
    #[serde(default)]
    pub limit_reached: bool,
    /// The vendor reports paid credits behind this window and has not actually
    /// refused a request against it, so being at 100% costs money rather than
    /// blocking work. Only the codex rollout/poller path ever sets it (issue
    /// #337); `#[serde(default)]` so every reading stored before this field
    /// existed loads as "not covered", which is the pre-existing behavior.
    #[serde(default)]
    pub overage_covered: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct UsageWindows {
    pub five_hour: Option<Window>,
    pub seven_day: Option<Window>,
}

/// A window needs a percentage to be useful. `resets_at` may be absent, and `0`
/// is the documented "unknown" marker callers fall back on.
fn window_at(node: Option<&Value>, observed_at: u64) -> Option<Window> {
    let node = node?;
    let used_percentage = node.get("used_percentage").and_then(Value::as_f64)?;
    Some(Window {
        used_percentage,
        resets_at: node.get("resets_at").and_then(Value::as_u64).unwrap_or(0),
        observed_at,
        overage_covered: false,
        limit_reached: false,
    })
}

/// Reads the documented statusline `rate_limits` block. `None` means there was
/// nothing to persist, which is the normal case for non-subscribers and for the
/// first statusline of a session, so it is never an error.
pub fn parse_statusline(json: &str, observed_at: u64) -> Option<UsageWindows> {
    let value: Value = serde_json::from_str(json).ok()?;
    let limits = value.get("rate_limits")?;
    if !limits.is_object() {
        return None;
    }

    let windows = UsageWindows {
        five_hour: window_at(limits.get("five_hour"), observed_at),
        seven_day: window_at(limits.get("seven_day"), observed_at),
    };
    if windows.five_hour.is_none() && windows.seven_day.is_none() {
        return None;
    }
    Some(windows)
}

/// `None` means there is no source at all for these windows -- no file, or one
/// that says nothing readable. Distinct from `Some(UsageWindows::default())`,
/// which is a real file that happens to report neither window: "unknown" and
/// "nothing used" are opposite things to say to an operator.
fn read_at(path: &Path) -> Option<UsageWindows> {
    serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()
}

/// Never fails: an absent or corrupt file reads as "nothing known", because a
/// statusline hook must not break on a half-written state file.
pub fn load(state: &StateDir) -> UsageWindows {
    read_at(&state.usage()).unwrap_or_default()
}

/// Atomic: every live session's statusline writes this file, so a reader must
/// never observe a truncated one.
fn store_at(path: &Path, windows: &UsageWindows) -> CtxResult<()> {
    if let Some(parent) = path.parent() {
        super::state::create_private_dir_all(parent)?;
    }
    let temp = path.with_extension(format!("tmp{}", std::process::id()));
    super::state::write_private(&temp, &serde_json::to_string(windows)?)?;
    std::fs::rename(&temp, path)?;
    Ok(())
}

pub fn store(state: &StateDir, windows: &UsageWindows) -> CtxResult<()> {
    store_at(&state.usage(), windows)
}

/// The account the legacy global `usage.json` holds readings for. Its only
/// writer is `usage::run_tee`, which is Claude Code's own statusline hook, so
/// whatever is in that file is Anthropic subscription data stored before
/// there was anywhere provider-specific to put it. Stated here as a fact
/// about a file already on disk rather than read off the adapter registry --
/// the file outlives any particular registry -- and pinned against
/// `ClaudeAdapter::provider` by a test so the two cannot drift.
pub const LEGACY_USAGE_PROVIDER: &str = "anthropic";

/// Per-provider counterpart of [`store`], written to
/// `StateDir::usage_for(provider)` with the same temp-plus-rename atomicity.
pub fn store_for(state: &StateDir, provider: &str, windows: &UsageWindows) -> CtxResult<()> {
    store_at(&state.usage_for(provider), windows)
}

/// This provider's usage windows, or `None` when nothing has ever recorded
/// any for it -- which is the honest answer for a provider with no collector
/// (codex/openai today), and must render as "no source", never as 0%.
///
/// [`LEGACY_USAGE_PROVIDER`] falls back to the legacy global file when it has
/// no provider file of its own yet, so an operator upgrading into this layout
/// keeps the reading their statusline has been collecting all along. The
/// legacy file is only read here, never moved or deleted: `load`, `zirv ctx
/// usage` and `wrap`'s status bar still read it directly.
pub fn load_for(state: &StateDir, provider: &str) -> Option<UsageWindows> {
    if let Some(windows) = read_at(&state.usage_for(provider)) {
        return Some(windows);
    }
    if super::state::provider_slug(provider) == LEGACY_USAGE_PROVIDER {
        return read_at(&state.usage());
    }
    None
}

/// True when nothing has ever been recorded for this provider. Since the
/// codex collector and the poller exist, no provider is structurally exempt
/// any more — callers refresh sources first, then ask.
pub fn has_no_usage_source(state: &StateDir, provider: &str) -> bool {
    load_for(state, provider).is_none()
}

fn newer(existing: Option<Window>, fresh: Option<Window>) -> Option<Window> {
    match (existing, fresh) {
        (Some(existing), Some(fresh)) if fresh.observed_at >= existing.observed_at => Some(fresh),
        (Some(existing), Some(_)) => Some(existing),
        (None, fresh) => fresh,
        (existing, None) => existing,
    }
}

/// Per-window merge. Each window may be independently absent from any given
/// statusline payload, so an absent window never erases a known one.
pub fn merge(existing: UsageWindows, fresh: UsageWindows) -> UsageWindows {
    UsageWindows {
        five_hour: newer(existing.five_hour, fresh.five_hour),
        seven_day: newer(existing.seven_day, fresh.seven_day),
    }
}

pub fn age_secs(window: &Window, now: u64) -> u64 {
    now.saturating_sub(window.observed_at)
}

/// Keeps each window slot only if it is still *available*, dropping the rest.
/// Two independent rules both have to hold:
///
/// 1. A window whose `resets_at` has certainly passed says nothing about
///    current usage -- the vendor has already rolled it over -- so it is
///    dropped. The boundary is `resets_at > now`, i.e. `resets_at == now` is
///    already treated as rolled over. This deliberately matches pace.rs's own
///    `reset_passed` convention (`resets_at != 0 && resets_at <= now`) so the
///    two never disagree about whether the reset second itself has passed.
///    A `resets_at` of `0` means the field was never reported, so this rule
///    does not apply to it.
/// 2. A reading inside a live window can be at most one span old, so age is
///    always checked too, regardless of `resets_at`: a live-looking but
///    implausibly old observation (e.g. a bogus far-future `resets_at`
///    persisted once and never refreshed) must not stay displayable forever.
///
/// So a window is kept only when it has not certainly reset *and* it is no
/// older than its own span. Each slot is judged independently, so a stale
/// five_hour reading never drops a still-live seven_day one, or vice versa.
/// Pure: `now` is always the caller's own unix second, never read internally.
pub fn available(windows: &UsageWindows, now: u64) -> UsageWindows {
    fn keep(window: Option<Window>, span: u64, now: u64) -> Option<Window> {
        let w = window?;
        let not_reset = w.resets_at == 0 || w.resets_at > now;
        let is_available = not_reset && age_secs(&w, now) <= span;
        is_available.then_some(w)
    }
    UsageWindows {
        five_hour: keep(windows.five_hour, FIVE_HOUR_SECS, now),
        seven_day: keep(windows.seven_day, SEVEN_DAY_SECS, now),
    }
}

pub const FIVE_HOUR_SECS: u64 = 5 * 3600;
pub const SEVEN_DAY_SECS: u64 = 7 * 24 * 3600;

/// How far into the future an event timestamp may sit before it is treated as
/// bogus rather than merely clock-skewed. Within this tolerance a future
/// timestamp still clamps to age-zero via `saturating_sub`, same as before;
/// beyond it, the event is skipped rather than inflating the freshest usage
/// bucket until wall-clock time catches up.
const FUTURE_SKEW_TOLERANCE_SECS: u64 = 5 * 60;

/// Days from the unix epoch for a civil date, valid for any year in range.
/// Howard Hinnant's `days_from_civil`, which is why no date crate is needed.
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = if year >= 0 { year } else { year - 399 } / 400;
    let year_of_era = year - era * 400;
    let day_of_year = (153 * (if month > 2 { month - 3 } else { month + 9 }) + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

/// Parses the exact shape claude writes: `2026-07-31T14:15:15.968Z`. Fractional
/// seconds and the offset suffix are ignored; anything else returns `None` so a
/// malformed line is skipped rather than counted at the wrong time.
pub fn parse_iso8601_utc(ts: &str) -> Option<u64> {
    let bytes = ts.as_bytes();
    if bytes.len() < 19 || bytes[4] != b'-' || bytes[7] != b'-' || bytes[10] != b'T' {
        return None;
    }
    if bytes[13] != b':' || bytes[16] != b':' {
        return None;
    }

    let field = |from: usize, to: usize| ts.get(from..to)?.parse::<i64>().ok();
    let year = field(0, 4)?;
    let month = field(5, 7)?;
    let day = field(8, 10)?;
    let hour = field(11, 13)?;
    let minute = field(14, 16)?;
    let second = field(17, 19)?;

    if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }
    if hour > 23 || minute > 59 || second > 60 {
        return None;
    }

    let days = days_from_civil(year, month, day);
    let total = days * 86_400 + hour * 3600 + minute * 60 + second;
    u64::try_from(total).ok()
}

/// Millisecond-precision sibling of [`parse_iso8601_utc`] (issue #293):
/// reuses that parser's own date/time bounds for the whole-second part
/// (which stays untouched, still returning whole unix seconds for every
/// existing caller), then reads a fractional-seconds component when one
/// follows the seconds field as `.` plus digits -- optional, so
/// `2026-07-31T14:15:15Z` and `2026-07-31T14:15:15.968Z` both parse. Only
/// the first three fractional digits become milliseconds (more are still
/// accepted, just truncated, the same way the whole-second parser already
/// ignores anything past the seconds field); fewer than three are
/// right-padded with zeros (`.5` -> 500ms). `None` for exactly the inputs
/// [`parse_iso8601_utc`] itself rejects.
pub fn parse_iso8601_utc_ms(ts: &str) -> Option<u64> {
    let secs = parse_iso8601_utc(ts)?;
    let millis = if ts.as_bytes().get(19) == Some(&b'.') {
        let mut digits: Vec<char> = ts[20..].chars().take_while(char::is_ascii_digit).collect();
        if digits.is_empty() {
            0
        } else {
            digits.truncate(3);
            while digits.len() < 3 {
                digits.push('0');
            }
            let ms_str: String = digits.into_iter().collect();
            ms_str.parse::<u64>().unwrap_or(0)
        }
    } else {
        0
    };
    secs.checked_mul(1000)?.checked_add(millis)
}

/// Cache reads are excluded by default: they are the dominant class in a cached
/// session and are discounted by the API, and the notes file records that the
/// limiter's real weighting is undocumented.
///
/// Issue #779: production no longer calls this (or `sum_file` below) --
/// `sum_transcripts` folds cached `CachedUsageEvent`s through `fold_events_
/// into_sums` instead, which applies the identical `count_cache_reads`
/// formula inline. Kept `#[cfg(test)]`, not deleted: it is the uncached
/// reference implementation `the_cache_matches_a_full_reparse_for_both_
/// readers` checks the cache against.
#[cfg(test)]
pub fn usage_tokens_of(usage: &Value, count_cache_reads: bool) -> u64 {
    let field = |key: &str| usage.get(key).and_then(Value::as_u64).unwrap_or(0);
    let mut total =
        field("input_tokens") + field("cache_creation_input_tokens") + field("output_tokens");
    if count_cache_reads {
        total += field("cache_read_input_tokens");
    }
    total
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct TokenSums {
    pub five_hour: u64,
    pub seven_day: u64,
    /// Unix second of the oldest event counted in each window, or `0` when the
    /// window counted nothing. Used to estimate when the window frees up.
    pub oldest_in_five_hour: u64,
    pub oldest_in_seven_day: u64,
    pub files_scanned: usize,
    pub events_counted: usize,
}

fn note_oldest(slot: &mut u64, at: u64) {
    if *slot == 0 || at < *slot {
        *slot = at;
    }
}

/// Whether `row` repeats the API response the previous assistant row already
/// contributed. Claude Code >= 2.1.209 writes one row per content block of one
/// response, each carrying that response's identical `usage` object, so a
/// per-row sum multiplies real spend by the block count -- the same defect
/// `claude::fold_assistant_usage` deduplicates, applied here so the trailing
/// usage windows and the per-session spend breakdown agree with it. Advances
/// `last_id` as a side effect; a row with no response identity at all counts
/// on its own, exactly as before the split.
fn same_api_response(row: &Value, last_id: &mut Option<String>) -> bool {
    let id = super::adapters::claude::response_identity(row).map(str::to_string);
    if id.is_some() && id == *last_id {
        return true;
    }
    *last_id = id;
    false
}

/// Accumulates one transcript's assistant usage into the trailing windows.
/// Events without a parseable timestamp cannot be placed in a window and are
/// skipped rather than counted at the wrong time.
///
/// Issue #779: kept `#[cfg(test)]`, not deleted -- see `usage_tokens_of`'s
/// matching doc comment just above. This is the uncached, single-pass
/// reference `sum_transcripts` is checked against, not a second production
/// code path.
#[cfg(test)]
pub fn sum_file(jsonl: &str, now: u64, count_cache_reads: bool, into: &mut TokenSums) {
    let mut last_id: Option<String> = None;
    for line in jsonl.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(row) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if row.get("type").and_then(Value::as_str) != Some("assistant") {
            continue;
        }
        if same_api_response(&row, &mut last_id) {
            continue;
        }
        let Some(at) = row
            .get("timestamp")
            .and_then(Value::as_str)
            .and_then(parse_iso8601_utc)
        else {
            continue;
        };
        if at > now.saturating_add(FUTURE_SKEW_TOLERANCE_SECS) {
            continue;
        }
        let age = now.saturating_sub(at);
        if age > SEVEN_DAY_SECS {
            continue;
        }

        let Some(usage) = row.get("message").and_then(|m| m.get("usage")) else {
            continue;
        };
        let tokens = usage_tokens_of(usage, count_cache_reads);

        into.events_counted += 1;
        into.seven_day += tokens;
        note_oldest(&mut into.oldest_in_seven_day, at);
        if age <= FIVE_HOUR_SECS {
            into.five_hour += tokens;
            note_oldest(&mut into.oldest_in_five_hour, at);
        }
    }
}

pub fn projects_root() -> CtxResult<PathBuf> {
    Ok(crate::utils::home_dir()?.join(".claude").join("projects"))
}

// Issue #779: `sum_transcripts`/`session_spend` used to `std::fs::read_to_
// string` and re-parse every line of EVERY transcript under `projects_root`
// on every single call -- no matter that `zirv ctx usage`/`status` are run
// dozens of times an hour against the same, mostly-unchanged files. On a
// machine with a few thousand transcripts (hundreds of benchmark runs plus
// one long-lived orchestrator transcript) that is gigabytes of JSON re-
// parsed from scratch every time, which is exactly what made both commands
// take tens of seconds to minutes instead of the sub-second reads they
// actually need to do (issue #779).
//
// The fix caches each transcript's own parsed, deduplicated usage EVENTS
// (`CachedUsageEvent`: a unix-second timestamp plus the four raw token
// counts) on disk, keyed by the transcript's path -- never a pre-summed
// total, because a total already commits to one `now`, one window length and
// one `count_cache_reads` choice, and this cache has to answer all of
// `session_spend`'s 24h window, `sum_transcripts`' 5h/7d windows, and the
// `count_cache_reads` toggle alike. Only `now`/the window/`count_cache_reads`
// are ever applied when FOLDING cached events (`fold_events_into_sums`/
// `fold_events_for_session`), never baked into the cache itself, so the
// numbers this reports are identical to a fresh full re-parse for any of
// those choices -- see `the_cache_matches_a_full_reparse_for_both_readers`.
//
// A cache entry records `parsed_len`, the byte offset its `events` already
// account for, so a transcript that grew since the last read is re-parsed
// only from that offset onward -- an actively-written transcript is never
// more than one turn's worth of new lines behind. Every byte read is treated
// as consumed (`sum_file`/`session_spend_of`'s own `str::lines` contract: a
// final line with no trailing `\n` still counts), so a finished transcript
// whose last write never appended a trailing newline is still fully counted,
// not held back forever waiting for one. A transcript whose length has gone
// BACKWARDS since the cache was written (log rotation, not ordinary growth)
// invalidates the whole entry: `load_transcript_cache` refuses it and the
// file is re-parsed from byte 0, exactly the pre-cache behaviour for that
// one file.
//
// On top of the incremental read, a transcript whose own mtime is already
// older than the longest window any caller folds against cannot contain a
// single event any such window would still count -- see `sum_file`'s and
// `session_spend_of`'s own per-row age checks -- so it is skipped without
// being opened at all (`is_older_than_retention`), which is what makes a
// machine with hundreds of long-finished benchmark transcripts cheap to
// scan: only transcripts touched inside the window are ever read.

/// Bumped when [`CachedTranscript`]'s on-disk shape changes: an older cache
/// entry is discarded and the transcript re-parsed from byte 0 rather than
/// resumed under a format this build no longer writes.
///
/// Issue #779: bumped to 2 when `mtime_nanos` was added, so a pre-existing
/// entry written under version 1 (no mtime recorded) is discarded rather than
/// silently trusted as "unchanged" by length alone.
const TRANSCRIPT_CACHE_VERSION: u32 = 2;

/// `mtime`, keyed as nanoseconds since the Unix epoch for exact equality
/// comparison and JSON storage. `None` (metadata unreadable, or a clock
/// before the epoch) never matches any stored value -- see this module's
/// "on any doubt" caching policy -- so a transcript whose mtime cannot be
/// read is always treated as changed rather than risking a false "unchanged".
fn mtime_key(mtime: SystemTime) -> Option<u128> {
    mtime
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .map(|d| d.as_nanos())
}

/// One already-parsed, already-deduplicated assistant-usage row, independent
/// of any `now`/window/`count_cache_reads` choice: `at` is the row's own
/// unix-second timestamp, and the four counts are exactly
/// `adapters::claude::usage_categories`'s fields (the same raw
/// `usage.*_tokens` fields `sum_file`'s `usage_tokens_of` reads). See this
/// section's own module-level comment for why nothing "now"-dependent is
/// ever folded in before caching.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
struct CachedUsageEvent {
    at: u64,
    input_tokens: u64,
    cache_creation_input_tokens: u64,
    cache_read_input_tokens: u64,
    output_tokens: u64,
}

/// One transcript's cache entry. `parsed_len` is always a line boundary (0,
/// or one past a `\n` this entry has already folded), so resuming from it
/// never splits a JSON row across two reads.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct CachedTranscript {
    version: u32,
    /// The transcript this entry describes; a cache that outlived its file
    /// (a hash collision, or a stale entry copied by hand) is never applied
    /// to a different path.
    path: String,
    parsed_len: u64,
    /// Issue #779: the transcript's mtime (as of the last successful parse),
    /// alongside `parsed_len`. A file rewritten to exactly the same byte
    /// length (log rotation, a rewritten checkpoint) changes its mtime
    /// without changing `parsed_len`, and `parsed_len` alone cannot tell that
    /// apart from an untouched file -- see [`transcript_events`].
    mtime_nanos: Option<u128>,
    #[serde(default)]
    last_response_id: Option<String>,
    events: Vec<CachedUsageEvent>,
}

/// One file per transcript, named after a hash of its path -- the same
/// scheme `score.rs`'s `checkpoint_path` uses for its own per-transcript
/// checkpoints, and for the same reason: the path itself carries the session
/// id and is far too long to be a filename.
fn transcript_cache_path(state: &StateDir, transcript: &Path) -> PathBuf {
    state.usage_scan_cache().join(format!(
        "{:016x}.json",
        super::event::input_hash(&transcript.display().to_string())
    ))
}

/// `None` on any doubt at all -- unreadable, corrupt, the wrong schema
/// version, a different transcript (hash collision), or a `parsed_len` past
/// the file's CURRENT length (the file is shorter now than when this entry
/// was written: rotation or truncation, never ordinary growth) -- which
/// sends the caller back to a full parse from byte 0, exactly the pre-cache
/// behaviour.
fn load_transcript_cache(
    cache_path: &Path,
    transcript: &Path,
    current_len: u64,
) -> Option<CachedTranscript> {
    let cached: CachedTranscript =
        serde_json::from_str(&std::fs::read_to_string(cache_path).ok()?).ok()?;
    let usable = cached.version == TRANSCRIPT_CACHE_VERSION
        && cached.path == transcript.display().to_string()
        && cached.parsed_len <= current_len;
    usable.then_some(cached)
}

/// Best-effort, like `score.rs`'s `save_checkpoint`: a cache entry that fails
/// to write just costs the next call a full re-parse of this one file, which
/// is exactly what happened before there was a cache at all. Written via a
/// temp-sibling-then-`rename` so a process killed mid-write leaves the
/// previous entry intact rather than a truncated one.
fn save_transcript_cache(cache_path: &Path, cached: &CachedTranscript) {
    let Ok(json) = serde_json::to_string(cached) else {
        return;
    };
    let Some(dir) = cache_path.parent() else {
        return;
    };
    if super::state::create_private_dir_all(dir).is_err() {
        return;
    }
    let staged = dir.join(format!("{}.tmp", std::process::id()));
    if super::state::write_private(&staged, &json).is_ok() {
        let _ = std::fs::rename(&staged, cache_path);
    }
}

/// Issue #779: `save_transcript_cache` never pruned `<state>/usage-scan/`, so
/// a deleted or renamed transcript, or one that has simply aged out of every
/// window this module ever folds against, left its cache entry on disk
/// forever. Called at most once per [`sum_transcripts`]/[`session_spend`]
/// call -- never per file scanned -- so the extra directory read costs one
/// call, not one per transcript.
///
/// An entry is removed when either:
/// - its source transcript (the cache's own recorded `path`) no longer
///   exists, or
/// - the CACHE FILE itself (not the transcript) is older than the longest
///   retention window any caller in this module ever folds against
///   (`SEVEN_DAY_SECS`) -- by then `is_older_than_retention` would skip the
///   transcript unread anyway, so the entry is dead weight regardless of
///   whether the transcript still exists.
///
/// Best-effort, like every other cache access in this module: a directory
/// that cannot be read, an entry that cannot be parsed, or a file that cannot
/// be removed, is simply left alone.
fn prune_transcript_cache_dir(dir: &Path, now: u64) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(meta) = entry.metadata() else {
            continue;
        };
        if !meta.is_file() {
            continue;
        }
        if is_older_than_retention(&meta, now, SEVEN_DAY_SECS) {
            let _ = std::fs::remove_file(&path);
            continue;
        }
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        let Ok(cached) = serde_json::from_str::<CachedTranscript>(&text) else {
            continue;
        };
        if !Path::new(&cached.path).exists() {
            let _ = std::fs::remove_file(&path);
        }
    }
}

/// Parses `text` -- assumed to start exactly at a line boundary -- into
/// `events`, extending it and advancing `last_id`'s dedup state. The same
/// per-row logic `sum_file`/`session_spend_of` apply (including their own
/// `str::lines` contract: a final line with no trailing `\n` still counts,
/// exactly as `std::fs::read_to_string(...).lines()` already treated it),
/// minus the `now`-dependent future-skew/window filters, which stay
/// query-time-only (see this section's own module-level comment). The whole
/// of `text` is always considered consumed -- see [`transcript_events`]'s own
/// doc comment for why a chunk is never held back waiting for a trailing
/// newline.
fn extract_usage_events(
    text: &str,
    last_id: &mut Option<String>,
    events: &mut Vec<CachedUsageEvent>,
) {
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(row) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if row.get("type").and_then(Value::as_str) != Some("assistant") {
            continue;
        }
        if same_api_response(&row, last_id) {
            continue;
        }
        let Some(at) = row
            .get("timestamp")
            .and_then(Value::as_str)
            .and_then(parse_iso8601_utc)
        else {
            continue;
        };
        let Some(usage) = row.get("message").and_then(|m| m.get("usage")) else {
            continue;
        };
        let categories = super::adapters::claude::usage_categories(usage);
        events.push(CachedUsageEvent {
            at,
            input_tokens: categories.input_tokens,
            cache_creation_input_tokens: categories.cache_creation_input_tokens,
            cache_read_input_tokens: categories.cache_read_input_tokens,
            output_tokens: categories.output_tokens,
        });
    }
}

/// The cached, deduplicated usage events for one transcript, doing the
/// minimum I/O its growth since the last call requires: nothing at all when
/// `current_len` already matches the cached `parsed_len` (the common,
/// steady-state case), only the bytes appended since otherwise, and a full
/// re-read only when [`load_transcript_cache`] refuses a shrunk entry.
///
/// The whole of what gets read is always treated as consumed, matching
/// `sum_file`/`session_spend_of`'s own `str::lines` contract exactly -- a
/// transcript is not required to end its last line with `\n` (a finished
/// session's final write commonly does not), and a cache that instead held
/// such a chunk back waiting for a trailing newline that will never arrive
/// would never count that transcript's last row at all. `parsed_len` is
/// advanced by the ACTUAL number of bytes read (`cached.parsed_len +
/// text.len()`), not derived from `current_len`, so a file that grew again
/// in the gap between this function's caller stat-ing it and this function
/// opening it is still accounted for correctly next call, never double- or
/// under-counted.
fn transcript_events(
    state: &StateDir,
    transcript: &Path,
    current_len: u64,
    current_mtime: Option<SystemTime>,
) -> Vec<CachedUsageEvent> {
    let cache_path = transcript_cache_path(state, transcript);
    let current_mtime_key = current_mtime.and_then(mtime_key);
    let fresh = || CachedTranscript {
        version: TRANSCRIPT_CACHE_VERSION,
        path: transcript.display().to_string(),
        parsed_len: 0,
        mtime_nanos: current_mtime_key,
        last_response_id: None,
        events: Vec::new(),
    };
    let mut cached =
        load_transcript_cache(&cache_path, transcript, current_len).unwrap_or_else(fresh);

    // Issue #779: `parsed_len` alone treats a same-length rewrite as
    // "unchanged" and serves stale events. Both must match for that; a same
    // length with a different (or newly unreadable) mtime means the file was
    // rewritten under our feet, so nothing cached can be trusted -- start
    // over from byte 0 rather than resuming from an offset that would read
    // zero new bytes and never notice.
    if cached.parsed_len == current_len {
        if cached.mtime_nanos == current_mtime_key {
            return cached.events;
        }
        cached = fresh();
    }

    use std::io::{Read, Seek, SeekFrom};
    let Ok(mut file) = std::fs::File::open(transcript) else {
        return cached.events;
    };
    // F3 (codex review fix): `current_len`/`current_mtime` above are the
    // CALLER's own stat of `transcript`'s path, taken before this
    // `File::open` -- a window in which the path can be unlinked and
    // replaced (log rotation, a rewritten checkpoint) before the open
    // resolves it, landing this handle on a DIFFERENT underlying file than
    // the one `cached` was validated against. Re-checked here from the OPEN
    // HANDLE's own metadata instead (a rename never affects an already-open
    // file description, so this is immune to a later replace of the path):
    // a handle length shorter than `cached.parsed_len` proves a seek to
    // that offset would not land on a continuation of the cached file at
    // all, and a handle mtime that no longer matches what the caller
    // observed just before the open proves the file underneath the path
    // moved between that stat and this open -- either way, this restarts
    // from a fresh, empty entry (never blindly seeks a stranger file's tail
    // onto the old file's cached events) exactly like an ordinary
    // shrunk-file rotation.
    let handle_meta = file.metadata().ok();
    let handle_len = handle_meta.as_ref().map(std::fs::Metadata::len);
    let handle_mtime_key = handle_meta
        .as_ref()
        .and_then(|meta| meta.modified().ok())
        .and_then(mtime_key);
    if handle_len.is_some_and(|len| len < cached.parsed_len)
        || handle_mtime_key != current_mtime_key
    {
        cached = fresh();
    }
    if file.seek(SeekFrom::Start(cached.parsed_len)).is_err() {
        return cached.events;
    }
    let mut buf = Vec::new();
    if file.read_to_end(&mut buf).is_err() {
        return cached.events;
    }
    if buf.is_empty() {
        return cached.events;
    }
    // A torn read racing an in-progress append (a multi-byte UTF-8 character
    // split across two writes) or genuinely corrupt bytes: try again next
    // call rather than guessing at partial content. `parsed_len` is left
    // untouched, so the next call re-reads this same chunk in full.
    let Ok(text) = String::from_utf8(buf) else {
        return cached.events;
    };

    // F2 (codex review fix): a live transcript can be read mid-append, so
    // `text` may end with a partial JSON row with no trailing `\n` yet.
    // `extract_usage_events`'s own `str::lines()` still yields that trailing
    // partial row as its own line, fails to parse it as JSON, and silently
    // skips it -- but `consumed` used to cover it regardless, permanently
    // losing that row's usage once the rest of it lands (the next read
    // starts AFTER it, so only the suffix is ever parsed). Held back here
    // instead: a trailing, `\n`-less line that fails to parse as a complete
    // JSON value is trimmed off `text` before anything is counted as
    // consumed, so the next call re-reads it whole. A trailing `\n`-less
    // line that DOES parse as complete JSON (an ordinary finished session's
    // last write, which commonly omits the final newline) is unaffected --
    // still consumed immediately, exactly as before.
    let mut text = text.as_str();
    if !text.ends_with('\n') {
        let last_line_start = text.rfind('\n').map_or(0, |idx| idx + 1);
        let last_line = text[last_line_start..].trim();
        if !last_line.is_empty() && serde_json::from_str::<Value>(last_line).is_err() {
            text = &text[..last_line_start];
        }
    }
    if text.is_empty() {
        return cached.events;
    }

    let mut last_id = cached.last_response_id.take();
    let consumed = text.len() as u64;
    extract_usage_events(text, &mut last_id, &mut cached.events);
    cached.parsed_len += consumed;
    cached.mtime_nanos = current_mtime_key;
    cached.last_response_id = last_id;
    cached.version = TRANSCRIPT_CACHE_VERSION;
    cached.path = transcript.display().to_string();
    save_transcript_cache(&cache_path, &cached);
    cached.events
}

/// Folds cached events into `into`, applying exactly the `now`-dependent
/// filters `sum_file` applies inline (the future-skew tolerance and the 5h/7d
/// windows) plus `count_cache_reads` -- the only things a cached event's raw
/// numbers still need decided at query time. Byte-for-byte the same
/// accumulation `sum_file` does, just reading a pre-parsed event instead of
/// a raw JSON row.
fn fold_events_into_sums(
    events: &[CachedUsageEvent],
    now: u64,
    count_cache_reads: bool,
    into: &mut TokenSums,
) {
    for e in events {
        if e.at > now.saturating_add(FUTURE_SKEW_TOLERANCE_SECS) {
            continue;
        }
        let age = now.saturating_sub(e.at);
        if age > SEVEN_DAY_SECS {
            continue;
        }
        let mut tokens = e.input_tokens + e.cache_creation_input_tokens + e.output_tokens;
        if count_cache_reads {
            tokens += e.cache_read_input_tokens;
        }

        into.events_counted += 1;
        into.seven_day += tokens;
        note_oldest(&mut into.oldest_in_seven_day, e.at);
        if age <= FIVE_HOUR_SECS {
            into.five_hour += tokens;
            note_oldest(&mut into.oldest_in_five_hour, e.at);
        }
    }
}

/// Folds one transcript's cached events into a `SessionSpend`, applying
/// exactly the `now`-dependent filters `session_spend_of` applies inline.
/// Mirrors `session_spend_of`'s own "no in-window rows, no entry" contract.
fn fold_events_for_session(
    session: &str,
    events: &[CachedUsageEvent],
    now: u64,
    window_secs: u64,
) -> Option<SessionSpend> {
    let mut spend = SessionSpend {
        session: session.to_string(),
        input_tokens: 0,
        cache_creation_input_tokens: 0,
        cache_read_input_tokens: 0,
        output_tokens: 0,
        events: 0,
        newest_at: 0,
    };
    for e in events {
        if e.at > now.saturating_add(FUTURE_SKEW_TOLERANCE_SECS) {
            continue;
        }
        let age = now.saturating_sub(e.at);
        if age > window_secs {
            continue;
        }
        spend.input_tokens = spend.input_tokens.saturating_add(e.input_tokens);
        spend.cache_creation_input_tokens = spend
            .cache_creation_input_tokens
            .saturating_add(e.cache_creation_input_tokens);
        spend.cache_read_input_tokens = spend
            .cache_read_input_tokens
            .saturating_add(e.cache_read_input_tokens);
        spend.output_tokens = spend.output_tokens.saturating_add(e.output_tokens);
        spend.events += 1;
        if e.at > spend.newest_at {
            spend.newest_at = e.at;
        }
    }
    (spend.events > 0).then_some(spend)
}

/// Unix seconds `modified` sits behind `now` by, or `None` when the mtime
/// itself cannot be read (never treated as "old" in that case -- a doubt
/// about a file's age must never be the reason its content goes unread).
fn mtime_age_secs(meta: &std::fs::Metadata, now: u64) -> Option<u64> {
    let modified = meta.modified().ok()?;
    let modified_secs = modified
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?
        .as_secs();
    Some(now.saturating_sub(modified_secs))
}

/// Whether `meta`'s own mtime is already older than `retention_secs` -- in
/// which case the file cannot contain a single event any window that short
/// or shorter would still count (`sum_file`/`session_spend_of` both drop an
/// event once its age exceeds the window), so it is skipped without being
/// opened at all. `retention_secs` is the caller's own longest window
/// (`SEVEN_DAY_SECS` for `sum_transcripts`, `window_secs` for
/// `session_spend`), never a fixed constant, so this can only ever skip a
/// file the caller could not have counted anyway.
fn is_older_than_retention(meta: &std::fs::Metadata, now: u64, retention_secs: u64) -> bool {
    mtime_age_secs(meta, now).is_some_and(|age| age > retention_secs)
}

/// One transcript's spend in the four raw classes, over a trailing window.
/// `session` is the file stem, `events` counts how many in-window assistant
/// rows contributed, and `newest_at` is the newest counted row's unix second
/// -- used to sort the breakdown by recency of activity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionSpend {
    pub session: String,
    pub input_tokens: u64,
    pub cache_creation_input_tokens: u64,
    pub cache_read_input_tokens: u64,
    pub output_tokens: u64,
    pub events: usize,
    pub newest_at: u64,
}

/// Folds one transcript's in-window assistant rows into a `SessionSpend` for
/// it, or `None` when nothing in the file falls inside the window -- a file
/// contributing no in-window rows produces no entry, never a zeroed one.
/// Applies the same three guards `sum_file` does: a row counts only when its
/// `timestamp` parses, is not more than `FUTURE_SKEW_TOLERANCE_SECS` in the
/// future, and is within `window_secs` of `now`. The four classes come from
/// `super::adapters::claude::usage_categories`, so this function and
/// `TranscriptUsage` can never disagree about what a class is.
///
/// Issue #779: kept `#[cfg(test)]`, not deleted -- see `usage_tokens_of`'s
/// matching doc comment above `sum_file`. This is the uncached, single-pass
/// reference `session_spend` is checked against, not a second production
/// code path.
#[cfg(test)]
fn session_spend_of(
    session: &str,
    jsonl: &str,
    now: u64,
    window_secs: u64,
) -> Option<SessionSpend> {
    let mut spend = SessionSpend {
        session: session.to_string(),
        input_tokens: 0,
        cache_creation_input_tokens: 0,
        cache_read_input_tokens: 0,
        output_tokens: 0,
        events: 0,
        newest_at: 0,
    };

    let mut last_id: Option<String> = None;
    for line in jsonl.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(row) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if row.get("type").and_then(Value::as_str) != Some("assistant") {
            continue;
        }
        if same_api_response(&row, &mut last_id) {
            continue;
        }
        let Some(at) = row
            .get("timestamp")
            .and_then(Value::as_str)
            .and_then(parse_iso8601_utc)
        else {
            continue;
        };
        if at > now.saturating_add(FUTURE_SKEW_TOLERANCE_SECS) {
            continue;
        }
        let age = now.saturating_sub(at);
        if age > window_secs {
            continue;
        }

        let Some(usage) = row.get("message").and_then(|m| m.get("usage")) else {
            continue;
        };
        let categories = super::adapters::claude::usage_categories(usage);

        spend.input_tokens = spend.input_tokens.saturating_add(categories.input_tokens);
        spend.cache_creation_input_tokens = spend
            .cache_creation_input_tokens
            .saturating_add(categories.cache_creation_input_tokens);
        spend.cache_read_input_tokens = spend
            .cache_read_input_tokens
            .saturating_add(categories.cache_read_input_tokens);
        spend.output_tokens = spend.output_tokens.saturating_add(categories.output_tokens);
        spend.events += 1;
        if at > spend.newest_at {
            spend.newest_at = at;
        }
    }

    (spend.events > 0).then_some(spend)
}

/// Per-session spend in the four raw classes, over a trailing window. Reuses
/// `sum_transcripts`'s directory walk verbatim (including descending into
/// `subagents/`, for the same reason: those tokens are charged), but folds
/// per file instead of into one combined total. The session name is the
/// file stem.
///
/// Issue #779: each file's own cached, deduplicated events
/// (`transcript_events`) are read incrementally rather than re-parsed whole
/// on every call -- see this module's own "Issue #779" comment above
/// `TRANSCRIPT_CACHE_VERSION` for the full design and the correctness
/// argument. `state` is where that cache lives; a caller with no `StateDir`
/// has no persistent cache to consult, not a reason to fail the walk.
pub fn session_spend(
    state: &StateDir,
    projects_root: &Path,
    now: u64,
    window_secs: u64,
) -> Vec<SessionSpend> {
    prune_transcript_cache_dir(&state.usage_scan_cache(), now);
    let mut out = Vec::new();
    let mut stack = vec![projects_root.to_path_buf()];

    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
                continue;
            }
            if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
                continue;
            }
            let Ok(meta) = entry.metadata() else {
                continue;
            };
            if is_older_than_retention(&meta, now, window_secs) {
                continue;
            }
            let session = path
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("unknown")
                .to_string();
            let events = transcript_events(state, &path, meta.len(), meta.modified().ok());
            if let Some(spend) = fold_events_for_session(&session, &events, now, window_secs) {
                out.push(spend);
            }
        }
    }
    out
}

/// Walks every transcript under the projects root, including the `subagents/`
/// subdirectories, because subagent turns live in their own files and still
/// spend the account's budget.
///
/// Issue #779: same incremental-cache treatment as `session_spend` above,
/// folded against `SEVEN_DAY_SECS` -- the longest window this function itself
/// ever counts against, regardless of what a caller passes as `now`.
pub fn sum_transcripts(
    state: &StateDir,
    projects_root: &Path,
    now: u64,
    count_cache_reads: bool,
) -> TokenSums {
    prune_transcript_cache_dir(&state.usage_scan_cache(), now);
    let mut sums = TokenSums::default();
    let mut stack = vec![projects_root.to_path_buf()];

    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
                continue;
            }
            if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
                continue;
            }
            let Ok(meta) = entry.metadata() else {
                continue;
            };
            if is_older_than_retention(&meta, now, SEVEN_DAY_SECS) {
                continue;
            }
            let events = transcript_events(state, &path, meta.len(), meta.modified().ok());
            sums.files_scanned += 1;
            fold_events_into_sums(&events, now, count_cache_reads, &mut sums);
        }
    }
    sums
}

fn estimated_window(used: u64, budget: u64, oldest: u64, span: u64, now: u64) -> Option<Window> {
    if budget == 0 {
        return None;
    }
    let percent = ((used as f64 / budget as f64) * 100.0).clamp(0.0, 100.0);
    let resets_at = if oldest == 0 { now } else { oldest + span };
    Some(Window {
        used_percentage: percent,
        resets_at,
        observed_at: now,
        overage_covered: false,
        limit_reached: false,
    })
}

/// Percentages only exist once the operator configures a budget: the notes file
/// records that a plan's real token allowance is undocumented, so a default
/// would be a guess presented as data.
pub fn estimate_windows(
    sums: &TokenSums,
    now: u64,
    five_hour_budget: u64,
    seven_day_budget: u64,
) -> UsageWindows {
    UsageWindows {
        five_hour: estimated_window(
            sums.five_hour,
            five_hour_budget,
            sums.oldest_in_five_hour,
            FIVE_HOUR_SECS,
            now,
        ),
        seven_day: estimated_window(
            sums.seven_day,
            seven_day_budget,
            sums.oldest_in_seven_day,
            SEVEN_DAY_SECS,
            now,
        ),
    }
}

/// Parses an RFC 3339 timestamp ("2026-08-16T20:49:59.785342+00:00", trailing
/// "Z" or "+/-HH:MM", fraction ignored) to unix seconds. None on anything
/// malformed or pre-epoch. Used by the codex collector and the rollout parser.
#[allow(dead_code)]
pub fn parse_rfc3339_utc(s: &str) -> Option<u64> {
    let (date, rest) = s.split_once('T')?;
    let mut dp = date.split('-');
    // Bounded the same way `parse_iso8601_utc` is bounded: exactly 4 digits,
    // so `days_from_civil(y, ..) * 86_400` can never see a year absurd enough
    // to overflow `i64` and panic in debug builds. Reachable from wrap's
    // status-bar redraw via the codex rollout scan, so this must degrade to
    // `None`, never panic.
    let y_field = dp.next()?;
    if y_field.len() != 4 || !y_field.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let y: i64 = y_field.parse().ok()?;
    if !(1970..=9999).contains(&y) {
        return None;
    }
    let mo: u64 = dp.next()?.parse().ok()?;
    let d: u64 = dp.next()?.parse().ok()?;
    if dp.next().is_some() || !(1..=12).contains(&mo) || !(1..=31).contains(&d) {
        return None;
    }
    // Split the time from the offset: "Z", or the last '+'/'-' in the string.
    let (time, offset_secs) = if let Some(t) = rest.strip_suffix('Z') {
        (t, 0i64)
    } else {
        let idx = rest.rfind(['+', '-'])?;
        let (t, off) = rest.split_at(idx);
        let sign = if off.starts_with('-') { -1i64 } else { 1i64 };
        let (oh, om) = off[1..].split_once(':')?;
        let oh: i64 = oh.parse().ok()?;
        let om: i64 = om.parse().ok()?;
        (t, sign * (oh * 3600 + om * 60))
    };
    let time = time.split_once('.').map_or(time, |(t, _frac)| t);
    let mut tp = time.split(':');
    let h: i64 = tp.next()?.parse().ok()?;
    let mi: i64 = tp.next()?.parse().ok()?;
    let sec: i64 = tp.next()?.parse().ok()?;
    if tp.next().is_some()
        || !(0..24).contains(&h)
        || !(0..60).contains(&mi)
        || !(0..61).contains(&sec)
    {
        return None;
    }
    let total =
        days_from_civil(y, mo as i64, d as i64) * 86_400 + h * 3600 + mi * 60 + sec - offset_secs;
    u64::try_from(total).ok()
}

/// Which UsageWindows slot a window of this length belongs to: the nearest of
/// 5h/7d, accepted only within a factor of two — anything else is a window
/// shape we do not understand and must drop, never guess.
fn window_slot(window_secs: u64) -> Option<bool /* true = five_hour */> {
    if (FIVE_HOUR_SECS / 2..=FIVE_HOUR_SECS * 2).contains(&window_secs) {
        Some(true)
    } else if (SEVEN_DAY_SECS / 2..=SEVEN_DAY_SECS * 2).contains(&window_secs) {
        Some(false)
    } else {
        None
    }
}

/// Maps a codex `rate_limits` object (primary/secondary with used_percent,
/// window_minutes, resets_at in unix seconds) onto UsageWindows. Shared by the
/// rollout collector and the codex poller.
///
/// Issue #337: `credits.has_credits` plus a null/absent `rate_limit_reached_type`
/// is the vendor saying, on the same payload, "this window is full, and I served
/// the request anyway out of paid credits". Both halves are required -- a
/// non-null `rate_limit_reached_type` is the vendor reporting it actually
/// refused -- and every other payload shape leaves `overage_covered` false, so
/// an absent `credits` node can never soften a real refusal.
#[allow(dead_code)]
pub fn windows_from_rate_limits(
    limits: &serde_json::Value,
    observed_at: u64,
) -> Option<UsageWindows> {
    let has_credits = limits
        .pointer("/credits/has_credits")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let limit_reached = limits
        .get("rate_limit_reached_type")
        .is_some_and(|value| !value.is_null());
    let overage_covered = has_credits && !limit_reached;
    let mut out = UsageWindows::default();
    for key in ["primary", "secondary"] {
        let Some(w) = limits.get(key).filter(|w| w.is_object()) else {
            continue;
        };
        let Some(used) = w.get("used_percent").and_then(|p| p.as_f64()) else {
            continue;
        };
        let minutes = w
            .get("window_minutes")
            .and_then(|m| m.as_u64())
            .unwrap_or(0);
        let resets_at = w.get("resets_at").and_then(|r| r.as_u64()).unwrap_or(0);
        // Saturating: `window_minutes` comes straight from untrusted JSON, and
        // a value near u64::MAX would otherwise panic in debug builds instead
        // of falling through to the slot rejection below.
        let Some(five_hour) = window_slot(minutes.saturating_mul(60)) else {
            continue;
        };
        let win = Window {
            used_percentage: used,
            resets_at,
            observed_at,
            overage_covered,
            limit_reached,
        };
        if five_hour {
            out.five_hour = Some(win);
        } else {
            out.seven_day = Some(win);
        }
    }
    (out.five_hour.is_some() || out.seven_day.is_some()).then_some(out)
}

/// The cumulative token totals codex reports on a `token_count` event's
/// `info.total_token_usage` node, when that node is present at all (it is
/// `null` on some snapshots -- verified in the real rate-limits fixture).
/// Individual subfields default to `0` when the node exists but a specific
/// field is missing, matching `CodexAdapter::transcript_usage`'s
/// pre-existing behavior.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct RolloutTokenTotals {
    pub input_tokens: u64,
    pub output_tokens: u64,
}

/// One decoded record from a single codex rollout JSONL line, shared by the
/// usage-window collector below (`parse_rollout_line`/
/// `windows_from_rate_limits`) and `CodexAdapter::parse_events`/
/// `structural_context` (issue #86), so the file is parsed once per line
/// instead of independently by two separate readers that could drift apart.
///
/// Only the shapes verified in
/// `docs/superpowers/notes/2026-07-31-codex-cli-facts.md` are represented:
/// "Turn boundary" (`task_started`/`task_complete` bracket a turn and share
/// a `turn_id`) and "Token usage" (`token_count`'s `rate_limits`/
/// `info.total_token_usage`). Assistant text outside `last_agent_message`,
/// tool calls, tool results, and any compaction/summarization boundary have
/// no verified rollout shape and are deliberately not modeled here -- see
/// that note and Known Issues.
#[derive(Debug, Clone, PartialEq)]
pub enum RolloutRecord {
    /// `event_msg` / `payload.type == "token_count"`.
    TokenCount {
        /// `0` when the line carries no parseable `timestamp` -- the token
        /// totals below need no timestamp at all, only the usage windows do,
        /// so a missing timestamp does not disqualify the whole record the
        /// way it disqualifies `windows`.
        observed_at: u64,
        /// `None` whenever `observed_at` could not be parsed, or the
        /// `rate_limits` node is absent/unrecognized; distinct from
        /// `windows_from_rate_limits` returning `None` for a shape it does
        /// not understand.
        windows: Option<UsageWindows>,
        totals: Option<RolloutTokenTotals>,
        /// `info.last_token_usage`, the SINGLE most recent request's usage
        /// rather than the session's running total. Its `input_tokens` is
        /// the prompt this request actually sent, i.e. the live context
        /// occupancy -- the figure rot's token gate needs, as opposed to
        /// `totals` above, which only ever climbs and describes cumulative
        /// spend (what `transcript_usage` reports). `None` when the node is
        /// absent.
        last: Option<RolloutTokenTotals>,
        /// `info.model_context_window`: the seat's real capacity as the
        /// harness itself reports it, for rot's capacity-aware gates. `None`
        /// -- never a guess -- when the line does not state one.
        context_window: Option<u64>,
    },
    /// `event_msg` / `payload.type == "task_started"`.
    TaskStarted,
    /// `event_msg` / `payload.type == "task_complete"`; `last_agent_message`
    /// is `None` on a failed turn (observed as JSON `null`).
    TaskComplete { last_agent_message: Option<String> },
}

/// Parses one rollout line into the record shapes above. `None` for garbage
/// JSON, a non-`event_msg` top-level type, or a `payload.type` this codebase
/// has no verified mapping for.
#[allow(dead_code)]
pub fn parse_rollout_record(line: &str) -> Option<RolloutRecord> {
    let v: Value = serde_json::from_str(line.trim()).ok()?;
    if v.get("type")?.as_str()? != "event_msg" {
        return None;
    }
    let payload = v.get("payload")?;
    match payload.get("type")?.as_str()? {
        "token_count" => {
            let observed_at = v
                .get("timestamp")
                .and_then(Value::as_str)
                .and_then(parse_rfc3339_utc);
            let windows = observed_at.and_then(|at| {
                payload
                    .get("rate_limits")
                    .and_then(|rl| windows_from_rate_limits(rl, at))
            });
            let usage_at = |pointer: &str| {
                payload.pointer(pointer).map(|t| RolloutTokenTotals {
                    input_tokens: t.get("input_tokens").and_then(Value::as_u64).unwrap_or(0),
                    output_tokens: t.get("output_tokens").and_then(Value::as_u64).unwrap_or(0),
                })
            };
            Some(RolloutRecord::TokenCount {
                observed_at: observed_at.unwrap_or(0),
                windows,
                totals: usage_at("/info/total_token_usage"),
                last: usage_at("/info/last_token_usage"),
                context_window: payload
                    .pointer("/info/model_context_window")
                    .and_then(Value::as_u64),
            })
        }
        "task_started" => Some(RolloutRecord::TaskStarted),
        "task_complete" => Some(RolloutRecord::TaskComplete {
            last_agent_message: payload
                .get("last_agent_message")
                .and_then(Value::as_str)
                .map(str::to_string),
        }),
        _ => None,
    }
}

/// One codex session-rollout JSONL line -> usage windows, if it is a
/// token_count event carrying rate limits and a parseable timestamp.
#[allow(dead_code)]
pub fn parse_rollout_line(line: &str) -> Option<UsageWindows> {
    match parse_rollout_record(line)? {
        RolloutRecord::TokenCount { windows, .. } => windows,
        RolloutRecord::TaskStarted | RolloutRecord::TaskComplete { .. } => None,
    }
}

/// The account the codex provider's usage is attributed to.
#[allow(dead_code)]
pub const CODEX_USAGE_PROVIDER: &str = "openai";

/// Floor between codex rollout-tree scan *attempts*, shared by
/// `pace::refresh_sources` (item 5: a parked codex session's wait loop must
/// not re-walk `~/.codex/sessions` on every 30s recheck) and `wrap.rs`'s
/// status-bar refresh (`redraw_bar_if_due`, formerly its own private
/// `CODEX_BAR_SCAN_SECS`) -- one constant so the two floors cannot drift.
pub(crate) const CODEX_SCAN_FLOOR_SECS: u64 = 60;

/// Rollout files grow large; only the tail can hold the newest snapshot.
#[allow(dead_code)]
const ROLLOUT_TAIL_BYTES: u64 = 64 * 1024;
#[allow(dead_code)]
pub(crate) const ROLLOUT_SCAN_FILES: usize = 3;

#[allow(dead_code)]
fn collect_jsonl(
    dir: &Path,
    depth: u8,
    out: &mut Vec<(std::time::SystemTime, std::path::PathBuf)>,
) {
    if depth > 4 {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_jsonl(&path, depth + 1, out);
        } else if path.extension().is_some_and(|e| e == "jsonl")
            && let Ok(meta) = entry.metadata()
            && let Ok(modified) = meta.modified()
        {
            out.push((modified, path));
        }
    }
}

#[allow(dead_code)]
fn last_snapshot_in(path: &Path, now: u64) -> Option<UsageWindows> {
    use std::io::{Read, Seek, SeekFrom};
    let mut f = std::fs::File::open(path).ok()?;
    let len = f.metadata().ok()?.len();
    f.seek(SeekFrom::Start(len.saturating_sub(ROLLOUT_TAIL_BYTES)))
        .ok()?;
    let mut buf = Vec::new();
    f.read_to_end(&mut buf).ok()?;
    // The tail may start mid-line/mid-char; lossy decode + rev line scan copes.
    let text = String::from_utf8_lossy(&buf);
    // Collect all valid snapshots (skew-valid), then return the one with max timestamp
    text.lines()
        .filter_map(parse_rollout_line)
        .filter(|w| newest_observation(w) <= now.saturating_add(FUTURE_SKEW_TOLERANCE_SECS))
        .max_by_key(newest_observation)
}

/// Newest rate-limit snapshot across the most recently modified rollout files.
#[allow(dead_code)]
pub fn scan_codex_rollouts(
    sessions_dir: &Path,
    max_files: usize,
    now: u64,
) -> Option<UsageWindows> {
    let mut files = Vec::new();
    collect_jsonl(sessions_dir, 0, &mut files);
    files.sort_by_key(|f| std::cmp::Reverse(f.0));
    files
        .into_iter()
        .take(max_files)
        .filter_map(|(_, p)| last_snapshot_in(&p, now))
        .max_by_key(newest_observation)
}

#[allow(dead_code)]
pub(crate) fn newest_observation(windows: &UsageWindows) -> u64 {
    windows
        .five_hour
        .iter()
        .chain(windows.seven_day.iter())
        .map(|w| w.observed_at)
        .max()
        .unwrap_or(0)
}

/// The freshness a refresh gate should actually trust: the newest observation
/// among only the slots `available` still considers live. A window whose
/// `resets_at` has rolled over (or that has aged past its own span) is
/// dropped by `available` before the display ever sees it, so a gate that
/// judged freshness off the raw `newest_observation` instead would keep
/// refusing to refresh for up to the staleness budget even though the
/// operator is already looking at a blank reading. Returns `0` -- "stale" --
/// when nothing survives the filter, same as `newest_observation` on an empty
/// set.
///
/// Both windows are always written from a single snapshot (`parse_statusline`,
/// `parse_anthropic_usage`, and `windows_from_rate_limits` all stamp both
/// slots with the same `observed_at`), so a plain `newest_observation` over
/// `available`'s output is not enough: when five_hour rolls over but
/// seven_day is still live, `available` drops five_hour and keeps seven_day
/// -- and seven_day's shared `observed_at` would still read as recent, even
/// though five_hour has nothing to show. So any slot `available` dropped
/// counts as staleness (`0`) here, not just an empty result overall. While a
/// dropped slot has no newer data to replace it, this makes the gate report
/// stale continuously, so the poll re-fires every `poll_min_interval_secs`
/// and the codex scan every `CODEX_SCAN_FLOOR_SECS` -- the same behavior the
/// code already had when *both* slots drop. `refresh_codex_usage`'s
/// merged-equals-existing no-op guard keeps the state file from being
/// rewritten on each of those re-fires, so the added cost is bounded and
/// accepted.
pub(crate) fn freshest_available_observation(windows: &UsageWindows, now: u64) -> u64 {
    let avail = available(windows, now);
    let dropped = (windows.five_hour.is_some() && avail.five_hour.is_none())
        || (windows.seven_day.is_some() && avail.seven_day.is_none());
    if dropped {
        0
    } else {
        newest_observation(&avail)
    }
}

/// Opportunistic passive refresh for codex: scan its session rollouts only
/// when the stored reading is stale. Best-effort by design — every failure
/// leaves the stored state exactly as it was.
#[allow(dead_code)]
pub fn refresh_codex_usage(
    state: &StateDir,
    sessions_dir: Option<&Path>,
    now: u64,
    max_age_secs: u64,
) {
    let existing = load_for(state, CODEX_USAGE_PROVIDER);
    if let Some(w) = &existing
        && now.saturating_sub(freshest_available_observation(w, now)) <= max_age_secs
    {
        return;
    }
    let default_dir = dirs::home_dir().map(|h| h.join(".codex").join("sessions"));
    let Some(dir) = sessions_dir.or(default_dir.as_deref()) else {
        return;
    };
    let Some(fresh) = scan_codex_rollouts(dir, ROLLOUT_SCAN_FILES, now) else {
        return;
    };
    let merged = merge(existing.clone().unwrap_or_default(), fresh);
    if Some(&merged) == existing.as_ref() {
        // The scan produced nothing newer than what is already stored:
        // rewriting an identical file on every refresh is pure churn.
        return;
    }
    let _ = store_for(state, CODEX_USAGE_PROVIDER, &merged);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::ctx::state::StateDir;

    fn fixture(name: &str) -> std::path::PathBuf {
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests")
            .join("fixtures")
            .join(name)
    }

    #[test]
    fn documented_rate_limit_fields_are_parsed() {
        let json =
            std::fs::read_to_string(fixture("statusline-with-limits.json")).expect("fixture");
        let windows = parse_statusline(&json, 1_784_999_000).expect("rate_limits present");

        let five = windows.five_hour.expect("five_hour");
        assert_eq!(five.used_percentage, 87.5);
        assert_eq!(five.resets_at, 1_785_000_000);
        assert_eq!(five.observed_at, 1_784_999_000);

        let seven = windows.seven_day.expect("seven_day");
        assert_eq!(
            seven.used_percentage, 31.0,
            "integer percentages parse as floats"
        );
        assert_eq!(seven.resets_at, 1_785_400_000);
    }

    #[test]
    fn a_statusline_without_rate_limits_yields_nothing_to_persist() {
        let json = std::fs::read_to_string(fixture("statusline-no-limits.json")).expect("fixture");
        assert_eq!(
            parse_statusline(&json, 1_784_999_000),
            None,
            "non-subscriber and pre-first-response sessions are normal, not errors"
        );
    }

    #[test]
    fn each_window_may_be_independently_absent() {
        let only_five =
            "{\"rate_limits\":{\"five_hour\":{\"used_percentage\":10,\"resets_at\":5}}}";
        let windows = parse_statusline(only_five, 1).expect("five_hour present");
        assert!(windows.five_hour.is_some());
        assert!(windows.seven_day.is_none());
    }

    #[test]
    fn a_window_missing_resets_at_is_still_usable_for_its_percentage() {
        let json = "{\"rate_limits\":{\"five_hour\":{\"used_percentage\":99.9}}}";
        let five = parse_statusline(json, 7)
            .expect("parsed")
            .five_hour
            .expect("five");
        assert_eq!(five.used_percentage, 99.9);
        assert_eq!(
            five.resets_at, 0,
            "zero means unknown, callers use the fallback delay"
        );
    }

    #[test]
    fn available_drops_a_window_whose_resets_at_has_certainly_passed() {
        let windows = UsageWindows {
            five_hour: Some(Window {
                used_percentage: 14.0,
                resets_at: 1000,
                observed_at: 500,
                overage_covered: false,
                limit_reached: false,
            }),
            seven_day: None,
        };
        let out = available(&windows, 1001);
        assert_eq!(
            out.five_hour, None,
            "resets_at in the past means the reading says nothing about now"
        );
    }

    #[test]
    fn available_keeps_a_window_whose_resets_at_is_still_in_the_future() {
        let windows = UsageWindows {
            five_hour: Some(Window {
                used_percentage: 14.0,
                resets_at: 1000,
                observed_at: 500,
                overage_covered: false,
                limit_reached: false,
            }),
            seven_day: None,
        };
        let out = available(&windows, 999);
        assert_eq!(out.five_hour, windows.five_hour);
    }

    #[test]
    fn available_keeps_a_zero_resets_at_window_that_is_still_fresh() {
        let windows = UsageWindows {
            five_hour: None,
            seven_day: Some(Window {
                used_percentage: 20.0,
                resets_at: 0,
                observed_at: 1000,
                overage_covered: false,
                limit_reached: false,
            }),
        };
        // Just inside the seven_day span from observation.
        let out = available(&windows, 1000 + SEVEN_DAY_SECS);
        assert_eq!(out.seven_day, windows.seven_day);
    }

    #[test]
    fn available_drops_a_zero_resets_at_window_older_than_its_span() {
        let windows = UsageWindows {
            five_hour: Some(Window {
                used_percentage: 20.0,
                resets_at: 0,
                observed_at: 1000,
                overage_covered: false,
                limit_reached: false,
            }),
            seven_day: None,
        };
        // One second past the five_hour span from observation.
        let out = available(&windows, 1000 + FIVE_HOUR_SECS + 1);
        assert_eq!(
            out.five_hour, None,
            "no resets_at and older than the window's own span is stale, not honest"
        );
    }

    #[test]
    fn available_judges_each_slot_independently() {
        let windows = UsageWindows {
            five_hour: Some(Window {
                used_percentage: 14.0,
                resets_at: 100,
                observed_at: 0,
                overage_covered: false,
                limit_reached: false,
            }),
            seven_day: Some(Window {
                used_percentage: 20.0,
                resets_at: 100_000,
                observed_at: 0,
                overage_covered: false,
                limit_reached: false,
            }),
        };
        let out = available(&windows, 5000);
        assert_eq!(
            out.five_hour, None,
            "the expired five_hour reading must not survive"
        );
        assert_eq!(
            out.seven_day, windows.seven_day,
            "a stale five_hour slot must not drop a still-live seven_day slot"
        );
    }

    #[test]
    fn available_drops_a_window_exactly_at_its_reset_second() {
        // Matches pace.rs's own `reset_passed` convention: `resets_at == now`
        // is already rolled over, not the last still-live second.
        let windows = UsageWindows {
            five_hour: Some(Window {
                used_percentage: 14.0,
                resets_at: 1000,
                observed_at: 999,
                overage_covered: false,
                limit_reached: false,
            }),
            seven_day: None,
        };
        let out = available(&windows, 1000);
        assert_eq!(
            out.five_hour, None,
            "resets_at == now must read as already rolled over"
        );
    }

    #[test]
    fn available_keeps_a_window_one_second_before_its_reset() {
        let windows = UsageWindows {
            five_hour: Some(Window {
                used_percentage: 14.0,
                resets_at: 1000,
                observed_at: 999,
                overage_covered: false,
                limit_reached: false,
            }),
            seven_day: None,
        };
        let out = available(&windows, 999);
        assert_eq!(out.five_hour, windows.five_hour);
    }

    #[test]
    fn available_drops_a_far_future_resets_at_once_the_observation_outlives_its_span() {
        // A bogus far-future `resets_at`, persisted once and never refreshed,
        // must not keep an arbitrarily old reading displayable forever -- a
        // reading inside a live window can be at most one span old.
        let windows = UsageWindows {
            five_hour: Some(Window {
                used_percentage: 14.0,
                resets_at: 4_102_444_800, // year 2100, i.e. "certainly not reset"
                observed_at: 0,
                overage_covered: false,
                limit_reached: false,
            }),
            seven_day: None,
        };
        let out = available(&windows, FIVE_HOUR_SECS + 1);
        assert_eq!(
            out.five_hour, None,
            "a far-future resets_at must not make an ancient observation immortal"
        );
    }

    /// Both windows are always written from one snapshot, so they share an
    /// `observed_at` in practice. When five_hour rolls over but seven_day is
    /// still live, `available` drops five_hour and keeps seven_day -- and
    /// `newest_observation` on that surviving set would still report the
    /// shared recent timestamp, wrongly reading as fresh even though
    /// five_hour has nothing to show. `freshest_available_observation` must
    /// treat a dropped slot as staleness (`0`) rather than let the surviving
    /// slot's shared timestamp paper over it.
    #[test]
    fn freshest_available_observation_is_stale_when_one_of_two_shared_observation_windows_rolled_over()
     {
        let now = 1_000_000u64;
        let windows = UsageWindows {
            five_hour: Some(Window {
                used_percentage: 90.0,
                resets_at: now - 1,
                observed_at: now - 10,
                overage_covered: false,
                limit_reached: false,
            }),
            seven_day: Some(Window {
                used_percentage: 30.0,
                resets_at: now + SEVEN_DAY_SECS,
                observed_at: now - 10,
                overage_covered: false,
                limit_reached: false,
            }),
        };
        assert_eq!(
            freshest_available_observation(&windows, now),
            0,
            "a dropped five_hour slot must make the reading read as stale, \
             even though seven_day's shared observed_at is recent"
        );
    }

    #[test]
    fn garbage_input_parses_to_nothing_rather_than_erroring() {
        assert_eq!(parse_statusline("not json at all", 1), None);
        assert_eq!(parse_statusline("", 1), None);
        assert_eq!(parse_statusline("{\"rate_limits\":\"nope\"}", 1), None);
    }

    #[test]
    fn state_round_trips_through_the_file() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().to_path_buf());

        assert_eq!(
            load(&state),
            UsageWindows::default(),
            "absent file is empty state"
        );

        let windows = UsageWindows {
            five_hour: Some(Window {
                used_percentage: 50.0,
                resets_at: 100,
                observed_at: 10,
                overage_covered: false,
                limit_reached: false,
            }),
            seven_day: None,
        };
        store(&state, &windows).expect("store");
        assert_eq!(load(&state), windows);
    }

    #[test]
    fn a_corrupt_state_file_reads_as_empty_instead_of_failing() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().to_path_buf());
        std::fs::create_dir_all(state.root()).expect("mkdir");
        std::fs::write(state.usage(), "{ this is not json").expect("write");
        assert_eq!(load(&state), UsageWindows::default());
    }

    #[test]
    fn store_leaves_no_partial_file_behind() {
        // Concurrent live sessions all write this file, so the write is atomic:
        // a temp file plus rename, never a truncate-then-write.
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().to_path_buf());
        store(&state, &UsageWindows::default()).expect("store");
        let strays: Vec<_> = std::fs::read_dir(state.root())
            .expect("read_dir")
            .flatten()
            .map(|e| e.file_name().to_string_lossy().to_string())
            .filter(|name| name != "usage.json")
            .collect();
        assert!(
            strays.is_empty(),
            "temp file was not cleaned up: {strays:?}"
        );
    }

    fn windows_at(pct: f64, observed_at: u64) -> UsageWindows {
        UsageWindows {
            five_hour: Some(Window {
                used_percentage: pct,
                resets_at: 1000,
                observed_at,
                overage_covered: false,
                limit_reached: false,
            }),
            seven_day: None,
        }
    }

    /// The slug the legacy file's data is attributed to has to be the same
    /// slug the claude adapter reports, or an upgrading user's readings would
    /// be filed under a provider nothing ever asks about.
    #[test]
    fn the_legacy_file_is_attributed_to_the_claude_adapters_own_provider() {
        use crate::commands::ctx::adapters::AgentAdapter;
        assert_eq!(
            crate::commands::ctx::adapters::claude::ClaudeAdapter::new(None).provider(),
            LEGACY_USAGE_PROVIDER
        );
    }

    #[test]
    fn per_provider_state_round_trips_through_its_own_file() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().to_path_buf());

        assert_eq!(
            load_for(&state, "openai"),
            None,
            "a provider with no collector has no source, which is not zero"
        );

        let windows = windows_at(50.0, 10);
        store_for(&state, "openai", &windows).expect("store");
        assert_eq!(load_for(&state, "openai"), Some(windows.clone()));
        assert_eq!(
            load(&state),
            UsageWindows::default(),
            "a provider write must not touch the legacy global file"
        );
    }

    /// E: codex/openai has no possible source (no tee writes for it, ever,
    /// today), so a fresh state dir with nothing written must read that way
    /// -- even after storing data for a *different* provider, which must
    /// never leak into this one's answer.
    #[test]
    fn has_no_usage_source_is_true_for_a_provider_with_no_collector() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().to_path_buf());
        assert!(has_no_usage_source(&state, "openai"));

        store_for(&state, "anthropic", &windows_at(50.0, 10)).expect("store");
        assert!(
            has_no_usage_source(&state, "openai"),
            "another provider's own data must not count as this one's source"
        );
    }

    /// E: the codex collector and poller now exist, so no provider is
    /// structurally exempt any more. Callers refresh sources first, then ask
    /// whether one is available.
    #[test]
    fn has_no_usage_source_is_true_for_any_provider_when_nothing_recorded() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().to_path_buf());
        assert!(
            has_no_usage_source(&state, "anthropic"),
            "no provider is exempt: all are 'nothing recorded' when no file exists"
        );
    }

    /// The upgrade case, and the one that matters most: a user who has been
    /// collecting into the legacy global file since before per-provider files
    /// existed must not see their readout go blank. The legacy file is read,
    /// never moved or deleted.
    #[test]
    fn an_upgrading_users_legacy_reading_still_shows_under_anthropic() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().to_path_buf());
        let legacy = windows_at(87.5, 10);
        store(&state, &legacy).expect("store the legacy file, as an older zirv did");
        assert!(
            !state.usage_for("anthropic").exists(),
            "no provider file exists yet: this is exactly the upgrade moment"
        );

        assert_eq!(
            load_for(&state, "anthropic"),
            Some(legacy),
            "the legacy file backs anthropic until a provider file exists"
        );
        assert!(
            state.usage().exists(),
            "the legacy file is read, never moved: other readers still use it"
        );
    }

    /// Once a provider file exists it is the answer; the legacy file is only
    /// ever the fallback, so a fresh reading is never shadowed by an old one.
    #[test]
    fn a_provider_file_wins_over_the_legacy_fallback() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().to_path_buf());
        store(&state, &windows_at(10.0, 10)).expect("legacy");
        store_for(&state, "anthropic", &windows_at(90.0, 99)).expect("provider");

        let five = load_for(&state, "anthropic")
            .expect("present")
            .five_hour
            .expect("five");
        assert_eq!(five.used_percentage, 90.0);
    }

    #[test]
    fn a_provider_store_leaves_no_partial_file_behind() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().to_path_buf());
        store_for(&state, "anthropic", &UsageWindows::default()).expect("store");
        let strays: Vec<_> = std::fs::read_dir(state.root())
            .expect("read_dir")
            .flatten()
            .map(|e| e.file_name().to_string_lossy().to_string())
            .filter(|name| name != "usage-anthropic.json")
            .collect();
        assert!(
            strays.is_empty(),
            "temp file was not cleaned up: {strays:?}"
        );
    }

    #[test]
    fn merging_keeps_the_newest_observation_per_window() {
        let old = UsageWindows {
            five_hour: Some(Window {
                used_percentage: 10.0,
                resets_at: 100,
                observed_at: 10,
                overage_covered: false,
                limit_reached: false,
            }),
            seven_day: Some(Window {
                used_percentage: 20.0,
                resets_at: 200,
                observed_at: 10,
                overage_covered: false,
                limit_reached: false,
            }),
        };
        let fresh = UsageWindows {
            five_hour: Some(Window {
                used_percentage: 90.0,
                resets_at: 300,
                observed_at: 50,
                overage_covered: false,
                limit_reached: false,
            }),
            seven_day: None,
        };

        let merged = merge(old, fresh);
        assert_eq!(merged.five_hour.expect("five").used_percentage, 90.0);
        assert_eq!(
            merged.seven_day.expect("seven").used_percentage,
            20.0,
            "an absent window in a fresh reading must not erase what is known"
        );
    }

    #[test]
    fn merging_never_moves_a_window_backwards_in_time() {
        let newer = UsageWindows {
            five_hour: Some(Window {
                used_percentage: 90.0,
                resets_at: 300,
                observed_at: 50,
                overage_covered: false,
                limit_reached: false,
            }),
            seven_day: None,
        };
        let stale = UsageWindows {
            five_hour: Some(Window {
                used_percentage: 5.0,
                resets_at: 100,
                observed_at: 10,
                overage_covered: false,
                limit_reached: false,
            }),
            seven_day: None,
        };
        let merged = merge(newer, stale);
        assert_eq!(
            merged.five_hour.expect("five").used_percentage,
            90.0,
            "a late-arriving stale sample must not win"
        );
    }

    #[test]
    fn age_is_measured_from_the_observation() {
        let window = Window {
            used_percentage: 1.0,
            resets_at: 0,
            observed_at: 100,
            overage_covered: false,
            limit_reached: false,
        };
        assert_eq!(age_secs(&window, 160), 60);
        assert_eq!(
            age_secs(&window, 90),
            0,
            "clock skew reads as fresh, not negative"
        );
    }

    #[test]
    fn real_transcript_timestamps_parse_to_unix_seconds() {
        // Exact format observed in ~/.claude/projects/**/*.jsonl.
        assert_eq!(
            parse_iso8601_utc("2026-07-31T14:15:15.968Z"),
            Some(1_785_507_315),
        );
        assert_eq!(parse_iso8601_utc("1970-01-01T00:00:00.000Z"), Some(0));
        assert_eq!(parse_iso8601_utc("1970-01-02T00:00:01.000Z"), Some(86_401));
        // Leap-year handling, since the window arithmetic depends on it.
        assert_eq!(
            parse_iso8601_utc("2024-02-29T00:00:00.000Z"),
            Some(1_709_164_800)
        );
    }

    #[test]
    fn malformed_timestamps_are_skipped_not_guessed() {
        assert_eq!(parse_iso8601_utc(""), None);
        assert_eq!(parse_iso8601_utc("yesterday"), None);
        assert_eq!(parse_iso8601_utc("2026-13-01T00:00:00Z"), None);
        assert_eq!(parse_iso8601_utc("2026-07-31"), None);
    }

    /// Issue #293: the millisecond variant keeps the same whole-second value
    /// the existing parser already returns, just scaled and with the
    /// fractional component folded in.
    #[test]
    fn ms_parser_matches_the_seconds_parser_scaled_up() {
        assert_eq!(
            parse_iso8601_utc_ms("2026-07-31T14:15:15.968Z"),
            Some(1_785_507_315_968)
        );
        assert_eq!(parse_iso8601_utc_ms("1970-01-01T00:00:00.000Z"), Some(0));
        assert_eq!(
            parse_iso8601_utc_ms("1970-01-02T00:00:01.000Z"),
            Some(86_401_000)
        );
    }

    /// Fractional seconds are optional -- a claude-shaped timestamp always
    /// has them, but a codex rollout `timestamp` field does not.
    #[test]
    fn ms_parser_treats_a_missing_fractional_part_as_zero_milliseconds() {
        assert_eq!(
            parse_iso8601_utc_ms("2026-07-31T14:15:15Z"),
            Some(1_785_507_315_000)
        );
    }

    /// Fewer than three fractional digits are right-padded; more than three
    /// are truncated, not rounded -- matching the whole-second parser's own
    /// "ignore anything past what it understands" stance.
    #[test]
    fn ms_parser_pads_short_and_truncates_long_fractional_digits() {
        assert_eq!(
            parse_iso8601_utc_ms("2026-07-31T14:15:15.5Z"),
            Some(1_785_507_315_500)
        );
        assert_eq!(
            parse_iso8601_utc_ms("2026-07-31T14:15:15.9681234Z"),
            Some(1_785_507_315_968)
        );
    }

    /// Every input the seconds parser rejects, the ms parser rejects too --
    /// never a guess.
    #[test]
    fn ms_parser_rejects_exactly_what_the_seconds_parser_rejects() {
        assert_eq!(parse_iso8601_utc_ms(""), None);
        assert_eq!(parse_iso8601_utc_ms("yesterday"), None);
        assert_eq!(parse_iso8601_utc_ms("2026-13-01T00:00:00Z"), None);
        assert_eq!(parse_iso8601_utc_ms("2026-07-31"), None);
    }

    #[test]
    fn cache_reads_are_excluded_by_default_and_optional() {
        // The usage block of a real cached assistant event.
        let usage = serde_json::json!({
            "input_tokens": 2,
            "cache_creation_input_tokens": 457,
            "cache_read_input_tokens": 108_427,
            "output_tokens": 577
        });
        assert_eq!(
            usage_tokens_of(&usage, false),
            1036,
            "input + cache_creation + output, cache reads excluded"
        );
        assert_eq!(usage_tokens_of(&usage, true), 109_463);
        assert_eq!(usage_tokens_of(&serde_json::json!({}), false), 0);
    }

    /// Builds a transcript whose assistant events sit at given ages in seconds.
    fn transcript_with_ages(now: u64, ages: &[u64], tokens: u64) -> String {
        let mut text = String::new();
        for age in ages {
            let at = now - age;
            text.push_str(&format!(
                "{{\"type\":\"assistant\",\"timestamp\":\"{}\",\"message\":{{\"usage\":{{\"input_tokens\":{tokens},\"cache_read_input_tokens\":999999}}}}}}\n",
                iso_of(at)
            ));
        }
        text
    }

    /// Inverse of `parse_iso8601_utc`, for building fixtures only.
    fn iso_of(unix: u64) -> String {
        let days = (unix / 86_400) as i64;
        let secs = unix % 86_400;
        let (year, month, day) = civil_from_days_for_tests(days);
        format!(
            "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}.000Z",
            secs / 3600,
            (secs % 3600) / 60,
            secs % 60
        )
    }

    fn civil_from_days_for_tests(days: i64) -> (i64, i64, i64) {
        let z = days + 719_468;
        let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
        let doe = z - era * 146_097;
        let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
        let y = yoe + era * 400;
        let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
        let mp = (5 * doy + 2) / 153;
        let d = doy - (153 * mp + 2) / 5 + 1;
        let m = if mp < 10 { mp + 3 } else { mp - 9 };
        (if m <= 2 { y + 1 } else { y }, m, d)
    }

    #[test]
    fn the_fixture_timestamp_helper_round_trips() {
        for unix in [0_u64, 1_785_507_315, 1_709_164_800] {
            assert_eq!(
                parse_iso8601_utc(&iso_of(unix)),
                Some(unix),
                "round trip {unix}"
            );
        }
    }

    #[test]
    fn a_future_dated_timestamp_is_excluded_from_every_bucket() {
        // Clock skew or corrupt data can date an event a day in the future.
        // `now.saturating_sub` would otherwise read that as age-zero and
        // inflate the freshest usage bucket until wall-clock time catches up.
        let now = 1_785_507_315;
        let far_future_at = now + 86_400;
        let jsonl = format!(
            "{{\"type\":\"assistant\",\"timestamp\":\"{}\",\"message\":{{\"usage\":{{\"input_tokens\":100}}}}}}\n",
            iso_of(far_future_at)
        );

        let mut sums = TokenSums::default();
        sum_file(&jsonl, now, false, &mut sums);

        assert_eq!(sums.five_hour, 0);
        assert_eq!(sums.seven_day, 0);
        assert_eq!(
            sums.events_counted, 0,
            "a far-future-dated event must not be counted at all"
        );
    }

    #[test]
    fn a_timestamp_within_the_skew_tolerance_still_clamps_to_age_zero() {
        // A few seconds of clock skew, well inside the tolerance, keeps today's
        // behavior: it counts, clamped to age zero.
        let now = 1_785_507_315;
        let slightly_future_at = now + 30;
        let jsonl = format!(
            "{{\"type\":\"assistant\",\"timestamp\":\"{}\",\"message\":{{\"usage\":{{\"input_tokens\":100}}}}}}\n",
            iso_of(slightly_future_at)
        );

        let mut sums = TokenSums::default();
        sum_file(&jsonl, now, false, &mut sums);

        assert_eq!(
            sums.five_hour, 100,
            "small skew still clamps to age zero, as before"
        );
        assert_eq!(sums.events_counted, 1);
    }

    #[test]
    fn only_events_inside_each_window_are_summed() {
        let now = 1_785_507_315;
        // 1h ago (both windows), 6h ago (7d only), 8d ago (neither).
        let jsonl = transcript_with_ages(now, &[3600, 21_600, 691_200], 100);

        let mut sums = TokenSums::default();
        sum_file(&jsonl, now, false, &mut sums);

        assert_eq!(sums.five_hour, 100, "one event within 5h");
        assert_eq!(sums.seven_day, 200, "two events within 7d");
        assert_eq!(sums.events_counted, 2);
    }

    #[test]
    fn the_oldest_counted_event_is_tracked_for_reset_estimation() {
        let now = 1_785_507_315;
        let jsonl = transcript_with_ages(now, &[3600, 7200], 10);
        let mut sums = TokenSums::default();
        sum_file(&jsonl, now, false, &mut sums);
        assert_eq!(sums.oldest_in_five_hour, now - 7200);
        assert_eq!(sums.oldest_in_seven_day, now - 7200);
    }

    #[test]
    fn non_assistant_and_malformed_lines_are_ignored() {
        let now = 1_785_507_315;
        let mut jsonl = String::new();
        jsonl.push_str("{\"type\":\"user\",\"message\":{\"content\":\"hi\"}}\n");
        jsonl.push_str("not json\n\n");
        jsonl.push_str("{\"type\":\"assistant\",\"message\":{\"usage\":{\"input_tokens\":5}}}\n");
        jsonl.push_str(&transcript_with_ages(now, &[60], 7));

        let mut sums = TokenSums::default();
        sum_file(&jsonl, now, false, &mut sums);
        assert_eq!(
            sums.five_hour, 7,
            "the event with no timestamp cannot be placed"
        );
        assert_eq!(sums.events_counted, 1);
    }

    /// The same per-content-block row split `claude::fold_assistant_usage`
    /// deduplicates: one API response, one usage object, repeated across the
    /// rows for its thinking/text/tool_use blocks. Summing per row inflates
    /// the trailing usage windows by however many blocks a response carried.
    #[test]
    fn one_api_responses_repeated_rows_are_counted_once() {
        let now = 1_785_507_315;
        let at = iso_of(now - 600);
        let row = |id: &str, tokens: u64| {
            format!(
                "{{\"type\":\"assistant\",\"timestamp\":\"{at}\",\"message\":{{\"id\":\"{id}\",\"usage\":{{\"input_tokens\":{tokens}}}}}}}\n"
            )
        };
        let jsonl = format!(
            "{}{}{}{}",
            row("msg_a", 100),
            row("msg_a", 100),
            row("msg_a", 100),
            row("msg_b", 7)
        );

        let mut sums = TokenSums::default();
        sum_file(&jsonl, now, false, &mut sums);
        assert_eq!(sums.five_hour, 107);
        assert_eq!(sums.events_counted, 2);

        let spend = session_spend_of("sess", &jsonl, now, FIVE_HOUR_SECS).expect("spend");
        assert_eq!(spend.input_tokens, 107);
        assert_eq!(spend.events, 2);
    }

    #[test]
    fn the_walk_includes_subagent_files() {
        let now = 1_785_507_315;
        let tmp = tempfile::tempdir().expect("tempdir");
        let projects = tmp.path().join("projects");
        let session_dir = projects.join("-home-testuser-repo");
        std::fs::create_dir_all(session_dir.join("subagents")).expect("mkdir");

        std::fs::write(
            session_dir.join("main.jsonl"),
            transcript_with_ages(now, &[600], 100),
        )
        .expect("write main");
        std::fs::write(
            session_dir.join("subagents").join("sub.jsonl"),
            transcript_with_ages(now, &[600], 25),
        )
        .expect("write subagent");
        // A non-transcript file must not be parsed.
        std::fs::write(session_dir.join("notes.txt"), "ignore me").expect("write txt");

        let state = StateDir::from_root(tmp.path().join("state"));
        let sums = sum_transcripts(&state, &projects, now, false);
        assert_eq!(sums.files_scanned, 2, "main plus subagent, not the txt");
        assert_eq!(
            sums.five_hour, 125,
            "subagent turns live in their own files and must be counted"
        );
    }

    #[test]
    fn an_absent_projects_root_sums_to_zero() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().to_path_buf());
        let sums = sum_transcripts(
            &state,
            std::path::Path::new("/nonexistent/projects"),
            100,
            false,
        );
        assert_eq!(sums, TokenSums::default());
    }

    #[test]
    fn percentages_need_a_configured_budget() {
        let now = 1_785_507_315;
        let sums = TokenSums {
            five_hour: 500,
            seven_day: 2000,
            oldest_in_five_hour: now - 3600,
            oldest_in_seven_day: now - 86_400,
            files_scanned: 1,
            events_counted: 4,
        };

        assert_eq!(
            estimate_windows(&sums, now, 0, 0),
            UsageWindows::default(),
            "no budget means no honest percentage"
        );

        let windows = estimate_windows(&sums, now, 1000, 8000);
        let five = windows.five_hour.expect("five_hour");
        assert_eq!(five.used_percentage, 50.0);
        assert_eq!(five.observed_at, now);
        assert_eq!(
            five.resets_at,
            now - 3600 + FIVE_HOUR_SECS,
            "a rolling window frees up when its oldest counted event ages out"
        );

        let seven = windows.seven_day.expect("seven_day");
        assert_eq!(seven.used_percentage, 25.0);
        assert_eq!(seven.resets_at, now - 86_400 + SEVEN_DAY_SECS);
    }

    #[test]
    fn percentages_are_capped_at_one_hundred() {
        let now = 1_000_000;
        let sums = TokenSums {
            five_hour: 5000,
            seven_day: 0,
            oldest_in_five_hour: now - 60,
            oldest_in_seven_day: 0,
            files_scanned: 1,
            events_counted: 1,
        };
        let five = estimate_windows(&sums, now, 1000, 0)
            .five_hour
            .expect("five");
        assert_eq!(five.used_percentage, 100.0);
    }

    #[test]
    fn a_window_with_no_events_reports_zero_and_resets_now() {
        let now = 1_000_000;
        let windows = estimate_windows(&TokenSums::default(), now, 1000, 1000);
        let five = windows.five_hour.expect("five");
        assert_eq!(five.used_percentage, 0.0);
        assert_eq!(five.resets_at, now, "nothing to wait for");
    }

    #[test]
    fn rfc3339_utc_parses_fraction_and_offset() {
        // 2026-02-26T18:52:21.222Z -> known epoch; verify against a precomputed value.
        let z = parse_rfc3339_utc("2026-02-26T18:52:21.222Z").unwrap();
        let plus = parse_rfc3339_utc("2026-02-26T18:52:21.222+00:00").unwrap();
        assert_eq!(z, plus);
        // +01:00 is one hour EARLIER in UTC
        let cet = parse_rfc3339_utc("2026-02-26T19:52:21+01:00").unwrap();
        assert_eq!(z, cet);
        assert_eq!(parse_rfc3339_utc("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(parse_rfc3339_utc("not a time"), None);
        assert_eq!(parse_rfc3339_utc("2026-13-40T99:00:00Z"), None);
    }

    #[test]
    fn rollout_snapshot_maps_primary_and_secondary_by_window_length() {
        let lines: Vec<&str> =
            include_str!("../../../tests/fixtures/codex-rollout-rate-limits.jsonl")
                .lines()
                .collect();
        let w = parse_rollout_line(lines[0]).unwrap();
        let fh = w.five_hour.unwrap();
        assert_eq!(fh.used_percentage, 10.0);
        assert_eq!(fh.resets_at, 1772135737);
        let sd = w.seven_day.unwrap();
        assert_eq!(sd.used_percentage, 3.0);
        assert_eq!(sd.resets_at, 1772722537);
        // observed_at comes from the line's own timestamp, not scan time
        assert_eq!(
            fh.observed_at,
            parse_rfc3339_utc("2026-02-26T18:52:21.222Z").unwrap()
        );
        // populated-info shape parses identically
        assert!(parse_rollout_line(lines[1]).is_some());
        // non-token_count lines and garbage yield None
        assert!(parse_rollout_line(lines[2]).is_none());
        assert!(parse_rollout_line("{broken").is_none());
        // a 1-minute window maps to neither slot -> dropped -> no windows -> None
        assert!(parse_rollout_line(lines[3]).is_none());
        // an absurd window_minutes must reject, never overflow-panic (review
        // finding on 4a44eb2: `minutes * 60` panicked in debug builds)
        let huge = format!(
            "{{\"timestamp\":\"2026-02-26T18:52:21.222Z\",\"type\":\"event_msg\",\"payload\":{{\"type\":\"token_count\",\"rate_limits\":{{\"primary\":{{\"used_percent\":1.0,\"window_minutes\":{},\"resets_at\":1}}}}}}}}",
            u64::MAX
        );
        assert!(parse_rollout_line(&huge).is_none());
    }

    /// Issue #227 (operator follow-up): a premium-tier codex account reports
    /// only a weekly window (no 5-hour window at all) -- `windows_from_rate_
    /// limits` must classify it as `seven_day` by its own reported length
    /// rather than assuming "primary" always means five_hour, and must leave
    /// `five_hour` genuinely `None` rather than inventing a reading for it.
    #[test]
    fn a_weekly_only_rate_limits_block_never_invents_a_five_hour_window() {
        let json = serde_json::json!({
            "primary": {
                "used_percent": 42.0,
                "window_minutes": 10_080, // 7 days
                "resets_at": 1_772_722_537u64,
            }
        });
        let windows = windows_from_rate_limits(&json, 1_772_000_000).expect("seven_day present");
        assert!(
            windows.five_hour.is_none(),
            "no 5-hour data was reported, so none must be invented: {windows:?}"
        );
        let seven = windows.seven_day.expect("seven_day");
        assert_eq!(seven.used_percentage, 42.0);
        assert_eq!(seven.resets_at, 1_772_722_537);
    }

    /// The Pro-tier shape (both windows reported) still maps each slot by its
    /// own length, unaffected by the weekly-only case above.
    #[test]
    fn a_pro_tier_rate_limits_block_maps_both_windows() {
        let json = serde_json::json!({
            "primary": {
                "used_percent": 10.0,
                "window_minutes": 300, // 5 hours
                "resets_at": 1,
            },
            "secondary": {
                "used_percent": 3.0,
                "window_minutes": 10_080, // 7 days
                "resets_at": 2,
            }
        });
        let windows = windows_from_rate_limits(&json, 1).expect("both present");
        assert_eq!(windows.five_hour.expect("five_hour").used_percentage, 10.0);
        assert_eq!(windows.seven_day.expect("seven_day").used_percentage, 3.0);
    }

    #[test]
    fn a_window_stored_before_overage_coverage_deserializes_as_uncovered() {
        let window: Window = serde_json::from_str(
            r#"{"used_percentage":99.0,"resets_at":1788758370,"observed_at":1788423353}"#,
        )
        .expect("old window shape");

        assert!(!window.overage_covered);
        assert!(!window.limit_reached);
    }

    #[test]
    fn rollout_reached_type_is_retained() {
        let record = |reached: serde_json::Value| {
            serde_json::json!({
                "timestamp": "2026-02-26T18:52:21.222Z",
                "type": "event_msg",
                "payload": {
                    "type": "token_count",
                    "rate_limits": {
                        "rate_limit_reached_type": reached,
                        "primary": {
                            "used_percent": 2.0,
                            "window_minutes": 300,
                            "resets_at": 1_772_135_737u64,
                        }
                    }
                }
            })
            .to_string()
        };
        let reading = |line: String| match parse_rollout_record(&line) {
            Some(RolloutRecord::TokenCount {
                windows: Some(windows),
                ..
            }) => windows.five_hour.expect("five-hour reading"),
            other => panic!("unexpected rollout record: {other:?}"),
        };

        assert!(reading(record(serde_json::json!("primary"))).limit_reached);
        assert!(!reading(record(serde_json::Value::Null)).limit_reached);
    }

    #[test]
    fn scan_finds_newest_by_timestamp_not_mtime() {
        let dir = tempfile::tempdir().unwrap();
        let day = dir.path().join("2026").join("02").join("26");
        std::fs::create_dir_all(&day).unwrap();
        let fixture = include_str!("../../../tests/fixtures/codex-rollout-rate-limits.jsonl");
        let lines: Vec<&str> = fixture.lines().collect();
        // lines[0]: 10% snapshot at 2026-02-26T18:52:21.222Z
        // lines[1]: 12% snapshot at 2026-02-26T18:52:27.310Z (newer timestamp)
        // rollout-a.jsonl: mtime older, holds 12% (newer timestamp) -> should win
        // rollout-b.jsonl: mtime newer, holds 10% (older timestamp) -> should lose
        std::fs::write(day.join("rollout-a.jsonl"), format!("{}\n", lines[1])).unwrap();
        std::fs::write(day.join("rollout-b.jsonl"), format!("{}\n", lines[0])).unwrap();
        // make b's mtime strictly newer (but it has the older timestamp)
        let newer = std::time::SystemTime::now() + std::time::Duration::from_secs(5);
        let f = std::fs::File::options()
            .append(true)
            .open(day.join("rollout-b.jsonl"))
            .unwrap();
        f.set_modified(newer).unwrap();
        let now = 1_784_999_000u64;
        let w = scan_codex_rollouts(dir.path(), 3, now).unwrap();
        assert_eq!(
            w.five_hour.unwrap().used_percentage,
            12.0,
            "should pick the snapshot with the newest embedded timestamp, not mtime"
        );
    }

    #[test]
    fn scan_of_missing_or_empty_dir_is_none() {
        let dir = tempfile::tempdir().unwrap();
        let now = 1_784_999_000u64;
        assert!(scan_codex_rollouts(&dir.path().join("nope"), 3, now).is_none());
        assert!(scan_codex_rollouts(dir.path(), 3, now).is_none());
    }

    #[test]
    fn scan_finds_newest_snapshot_among_out_of_order_lines() {
        let dir = tempfile::tempdir().unwrap();
        let day = dir.path().join("2026").join("02").join("26");
        std::fs::create_dir_all(&day).unwrap();
        let fixture = include_str!("../../../tests/fixtures/codex-rollout-rate-limits.jsonl");
        let lines: Vec<&str> = fixture.lines().collect();
        // Write lines in reverse order: 12% first, then 10%, so the first one is NOT the max
        // Verifies we pick the max timestamp, not just the first or last line
        std::fs::write(
            day.join("rollout.jsonl"),
            format!("{}\n{}\n", lines[1], lines[0]),
        )
        .unwrap();
        let now = 1_784_999_000u64;
        let w = scan_codex_rollouts(dir.path(), 3, now).unwrap();
        assert_eq!(
            w.five_hour.unwrap().used_percentage,
            12.0,
            "should pick snapshot with newest timestamp despite line order"
        );
    }

    #[test]
    fn scan_skips_far_future_dated_snapshot() {
        let dir = tempfile::tempdir().unwrap();
        let day = dir.path().join("2026").join("02").join("26");
        std::fs::create_dir_all(&day).unwrap();
        let now = 1_784_999_000u64;
        // Create a line with far-future timestamp beyond the skew tolerance
        let far_future_json = r#"{"timestamp":"2099-12-31T23:59:59Z","type":"event_msg","payload":{"type":"token_count","rate_limits":{"primary":{"used_percent":50.0,"window_minutes":300,"resets_at":1772135737},"secondary":{"used_percent":3.0,"window_minutes":10080,"resets_at":1772722537}}}}"#;
        std::fs::write(day.join("rollout.jsonl"), format!("{}\n", far_future_json)).unwrap();
        // Should find no snapshot (far-future is skipped)
        assert!(
            scan_codex_rollouts(dir.path(), 3, now).is_none(),
            "far-future snapshot should be skipped"
        );
    }

    #[test]
    fn scan_uses_valid_snapshot_after_skipping_future_dated_line() {
        let dir = tempfile::tempdir().unwrap();
        let day = dir.path().join("2026").join("02").join("26");
        std::fs::create_dir_all(&day).unwrap();
        let fixture = include_str!("../../../tests/fixtures/codex-rollout-rate-limits.jsonl");
        let lines: Vec<&str> = fixture.lines().collect();
        let now = 1_784_999_000u64;
        // Create a line with far-future timestamp
        let far_future_json = r#"{"timestamp":"2099-12-31T23:59:59Z","type":"event_msg","payload":{"type":"token_count","rate_limits":{"primary":{"used_percent":99.0,"window_minutes":300,"resets_at":1772135737}}}}"#;
        // Write future line first, then valid 12% line
        std::fs::write(
            day.join("rollout.jsonl"),
            format!("{}\n{}\n", far_future_json, lines[1]),
        )
        .unwrap();
        let w = scan_codex_rollouts(dir.path(), 3, now).unwrap();
        assert_eq!(
            w.five_hour.unwrap().used_percentage,
            12.0,
            "should use the valid (non-future) snapshot when future-dated line is present"
        );
    }

    #[test]
    fn refresh_skips_when_stored_reading_is_fresh_and_stores_when_stale() {
        let tmp = tempfile::tempdir().unwrap();
        let state = StateDir::from_root(tmp.path().to_path_buf());

        // Create a codex sessions dir with a test file
        let sessions_dir = tmp.path().join("codex_sessions");
        let day = sessions_dir.join("2026").join("02").join("26");
        std::fs::create_dir_all(&day).unwrap();

        // A rollout line just before `now`, so a scan of it is genuinely
        // newer than the "stale" stored reading below and the merge must
        // prefer it -- the review of c3c7fe9 caught this test asserting
        // nothing when the line's timestamp predated the stale reading.
        let test_json = r#"{"timestamp":"2026-02-26T18:52:21Z","type":"event_msg","payload":{"type":"token_count","rate_limits":{"primary":{"used_percent":12.0,"window_minutes":300,"resets_at":1772135737}}}}"#;
        let line_ts = parse_rfc3339_utc("2026-02-26T18:52:21Z").expect("test timestamp parses");
        let now = line_ts + 60;
        let max_age = 900u64;
        std::fs::write(day.join("rollout.jsonl"), format!("{}\n", test_json)).unwrap();

        let scanned = scan_codex_rollouts(sessions_dir.as_path(), ROLLOUT_SCAN_FILES, now);
        assert!(
            scanned.is_some(),
            "scan should find snapshot with compatible timestamp"
        );
        let scanned_val = scanned.unwrap();
        assert_eq!(
            scanned_val.five_hour.unwrap().used_percentage,
            12.0,
            "scan should find 12%"
        );

        // Pre-store a fresh openai reading (observed_at close to now). Its
        // `resets_at` must sit safely in the future: the refresh gate now
        // filters through `window::available` before judging freshness (Fix
        // 3), so a `resets_at` in the past -- as this fixture used to have --
        // would read as already rolled over and force a rescan regardless of
        // how recent `observed_at` is, defeating the point of this case.
        let fresh = UsageWindows {
            five_hour: Some(Window {
                used_percentage: 50.0,
                resets_at: now + 100_000,
                observed_at: now - 100, // well within max_age
                overage_covered: false,
                limit_reached: false,
            }),
            seven_day: None,
        };
        store_for(&state, CODEX_USAGE_PROVIDER, &fresh).expect("store fresh");

        refresh_codex_usage(&state, Some(sessions_dir.as_path()), now, max_age);
        let after_fresh_refresh = load_for(&state, CODEX_USAGE_PROVIDER);
        assert_eq!(
            after_fresh_refresh,
            Some(fresh),
            "fresh reading should not be updated"
        );

        // Now pre-store a stale reading (observed_at = now - 10_000)
        let stale = UsageWindows {
            five_hour: Some(Window {
                used_percentage: 20.0,
                resets_at: 500,
                observed_at: now - 10_000, // well beyond max_age
                overage_covered: false,
                limit_reached: false,
            }),
            seven_day: None,
        };
        store_for(&state, CODEX_USAGE_PROVIDER, &stale).expect("store stale");

        refresh_codex_usage(&state, Some(sessions_dir.as_path()), now, max_age);
        let after_stale_refresh = load_for(&state, CODEX_USAGE_PROVIDER);

        // The scanned 12% (observed_at = now - 60) is newer than the stale
        // 20% (observed_at = now - 10_000), so the merge must replace it.
        let merged = after_stale_refresh.expect("merged present");
        let five = merged.five_hour.expect("five_hour after refresh");
        assert_eq!(
            five.used_percentage, 12.0,
            "a stale stored reading is replaced by the fresher scan"
        );
        assert_eq!(five.resets_at, 1772135737);
    }

    /// Fix 3: the same rule `maybe_poll`'s gate now applies. A stored reading
    /// whose `resets_at` has already passed must not count as fresh just
    /// because `observed_at` is recent -- `available` blanks it from the
    /// display, so the passive-refresh gate has to agree, or the operator
    /// sees a blank reading for up to `max_age_secs` at every reset
    /// boundary even though a rescan would find something.
    ///
    /// To prove the rescan actually ran (not just that it was harmless), the
    /// stored reading's `observed_at` is set slightly *older* than the
    /// scanned rollout line's own timestamp: `merge` always keeps whichever
    /// side has the newer `observed_at`, so if the gate wrongly short-circuited
    /// on staleness the stored 20% would remain; only an actual rescan lets
    /// the scanned 12% win the merge and reach storage.
    #[test]
    fn refresh_proceeds_when_the_recently_observed_reading_has_already_rolled_over() {
        let tmp = tempfile::tempdir().unwrap();
        let state = StateDir::from_root(tmp.path().to_path_buf());

        let sessions_dir = tmp.path().join("codex_sessions");
        let day = sessions_dir.join("2026").join("02").join("26");
        std::fs::create_dir_all(&day).unwrap();
        let test_json = r#"{"timestamp":"2026-02-26T18:52:21Z","type":"event_msg","payload":{"type":"token_count","rate_limits":{"primary":{"used_percent":12.0,"window_minutes":300,"resets_at":1772135737}}}}"#;
        std::fs::write(day.join("rollout.jsonl"), format!("{}\n", test_json)).unwrap();
        let line_ts = parse_rfc3339_utc("2026-02-26T18:52:21Z").expect("test timestamp parses");
        // `now` is only 15s after the scanned line's own timestamp, so both
        // the scanned reading and the stored one below are "recently
        // observed" in real terms.
        let now = line_ts + 15;
        let max_age = 900u64;

        // Stored reading: observed 20 seconds before `now` (well inside
        // max_age, and older than the scanned line so the merge only prefers
        // it if the scan never actually ran), but its own window rolled over
        // 1 second before `now` -- `available` must drop it, so the gate
        // must not treat it as fresh.
        let rolled_over = UsageWindows {
            five_hour: Some(Window {
                used_percentage: 20.0,
                resets_at: now - 1,
                observed_at: now - 20,
                overage_covered: false,
                limit_reached: false,
            }),
            seven_day: None,
        };
        store_for(&state, CODEX_USAGE_PROVIDER, &rolled_over).expect("store rolled-over reading");

        refresh_codex_usage(&state, Some(sessions_dir.as_path()), now, max_age);
        let after = load_for(&state, CODEX_USAGE_PROVIDER).expect("stored after refresh");
        let five = after.five_hour.expect("five_hour after refresh");
        assert_eq!(
            five.used_percentage, 12.0,
            "a rolled-over-but-recently-observed reading must not gate out the rescan"
        );
    }

    #[test]
    fn no_usage_source_is_now_a_plain_no_data_check() {
        let tmp = tempfile::tempdir().unwrap();
        let state = StateDir::from_root(tmp.path().to_path_buf());

        // openai has nothing stored -> true
        assert!(has_no_usage_source(&state, CODEX_USAGE_PROVIDER));

        // Store something for openai -> false
        store_for(
            &state,
            CODEX_USAGE_PROVIDER,
            &UsageWindows {
                five_hour: Some(Window {
                    used_percentage: 50.0,
                    resets_at: 1000,
                    observed_at: 10,
                    overage_covered: false,
                    limit_reached: false,
                }),
                seven_day: None,
            },
        )
        .expect("store");
        assert!(!has_no_usage_source(&state, CODEX_USAGE_PROVIDER));

        // anthropic with nothing stored -> true (previously hardcoded false)
        let tmp2 = tempfile::tempdir().unwrap();
        let state2 = StateDir::from_root(tmp2.path().to_path_buf());
        assert!(has_no_usage_source(&state2, "anthropic"));

        // Store something for anthropic -> false
        store_for(
            &state2,
            "anthropic",
            &UsageWindows {
                five_hour: Some(Window {
                    used_percentage: 75.0,
                    resets_at: 2000,
                    observed_at: 20,
                    overage_covered: false,
                    limit_reached: false,
                }),
                seven_day: None,
            },
        )
        .expect("store");
        assert!(!has_no_usage_source(&state2, "anthropic"));
    }

    /// Item 1 (review): an absurd year must never reach `days_from_civil`'s
    /// multiplication, which overflows `i64` and panics in debug builds.
    /// Reachable from wrap's status-bar redraw via the codex rollout scan.
    #[test]
    fn an_absurd_year_is_rejected_not_overflowed() {
        assert_eq!(
            parse_rfc3339_utc("999999999999-01-01T00:00:00Z"),
            None,
            "must reject, never panic"
        );
        assert_eq!(parse_rfc3339_utc("99999-01-01T00:00:00Z"), None);
        assert_eq!(parse_rfc3339_utc("1969-12-31T23:59:59Z"), None);
        assert_eq!(parse_rfc3339_utc("1970-01-01T00:00:00Z"), Some(0));
        assert!(parse_rfc3339_utc("9999-12-31T23:59:59Z").is_some());

        // Same absurd year, carried through a full rollout line: must yield
        // `None` end to end, never panic.
        let line = r#"{"timestamp":"999999999999-01-01T00:00:00Z","type":"event_msg","payload":{"type":"token_count","rate_limits":{"primary":{"used_percent":1.0,"window_minutes":300,"resets_at":1}}}}"#;
        assert_eq!(parse_rollout_line(line), None);
    }

    /// Item 2 (review): `refresh_codex_usage` must not rewrite the stored
    /// file when a scan produces exactly what is already on disk -- that is
    /// pure churn on every passive refresh. Proven via the file's mtime: a
    /// real `store_for` always renames a fresh temp file over the target,
    /// which would move the mtime forward.
    #[test]
    fn refresh_skips_the_store_when_the_merge_produces_no_change() {
        let tmp = tempfile::tempdir().unwrap();
        let state = StateDir::from_root(tmp.path().to_path_buf());

        let sessions_dir = tmp.path().join("codex_sessions");
        let day = sessions_dir.join("2026").join("02").join("26");
        std::fs::create_dir_all(&day).unwrap();
        let test_json = r#"{"timestamp":"2026-02-26T18:52:21Z","type":"event_msg","payload":{"type":"token_count","rate_limits":{"primary":{"used_percent":12.0,"window_minutes":300,"resets_at":1772135737}}}}"#;
        std::fs::write(day.join("rollout.jsonl"), format!("{}\n", test_json)).unwrap();
        let line_ts = parse_rfc3339_utc("2026-02-26T18:52:21Z").expect("test timestamp parses");

        // Seed the store with exactly what a scan of this file produces, so
        // the merge that follows is a genuine no-op.
        let scanned = scan_codex_rollouts(&sessions_dir, ROLLOUT_SCAN_FILES, line_ts + 60)
            .expect("scan finds the seeded snapshot");
        store_for(&state, CODEX_USAGE_PROVIDER, &scanned).expect("seed store");

        let usage_path = state.usage_for(CODEX_USAGE_PROVIDER);
        // Back-date the file's mtime so a rewrite would be observable.
        let old_mtime = std::time::SystemTime::now() - std::time::Duration::from_secs(3600);
        let f = std::fs::File::options()
            .append(true)
            .open(&usage_path)
            .unwrap();
        f.set_modified(old_mtime).unwrap();
        let before = std::fs::metadata(&usage_path).unwrap().modified().unwrap();

        // `now` is far enough past the seeded observation that the existing
        // reading counts as stale, forcing the function past the early
        // freshness return and into the scan-and-merge path.
        let now = line_ts + 60 + 10_000;
        refresh_codex_usage(&state, Some(sessions_dir.as_path()), now, 900);

        let after = std::fs::metadata(&usage_path).unwrap().modified().unwrap();
        assert_eq!(
            before, after,
            "store_for must be skipped when the merge is unchanged"
        );
    }

    /// Issue #337: the two rollout files copied verbatim off the machine that
    /// reported the bug, laid out the way codex itself writes them
    /// (`<sessions>/<yyyy>/<mm>/<dd>/rollout-<local-ts>-<uuid>.jsonl`).
    /// Returns the sessions root; `stale_openai_reading` provides the stored
    /// reading that was on disk there.
    fn real_rollout_sessions_dir(root: &Path, days: &[&str]) -> PathBuf {
        const ROLLOUTS: [(&str, &str, &str); 2] = [
            (
                "03",
                "rollout-2026-09-03T12-29-19-01a066d0-df70-7c21-9a93-920c7f9df2b6.jsonl",
                include_str!(
                    "../../../tests/fixtures/codex-rollouts/2026/09/03/rollout-2026-09-03T12-29-19-01a066d0-df70-7c21-9a93-920c7f9df2b6.jsonl"
                ),
            ),
            (
                "04",
                "rollout-2026-09-04T07-56-26-01a06afd-63c2-7061-8bdf-2798fe10b9e2.jsonl",
                include_str!(
                    "../../../tests/fixtures/codex-rollouts/2026/09/04/rollout-2026-09-04T07-56-26-01a06afd-63c2-7061-8bdf-2798fe10b9e2.jsonl"
                ),
            ),
        ];
        let sessions = root.join("codex_sessions");
        for (day, name, body) in ROLLOUTS {
            if !days.contains(&day) {
                continue;
            }
            let dir = sessions.join("2026").join("09").join(day);
            std::fs::create_dir_all(&dir).expect("rollout day dir");
            std::fs::write(dir.join(name), body).expect("rollout file");
        }
        sessions
    }

    /// The stale reading `usage-openai.json` actually held on the machine in
    /// issue #337: seven_day 99% observed 2026-09-03T08:15:53Z.
    fn stale_openai_reading() -> UsageWindows {
        UsageWindows {
            five_hour: None,
            seven_day: Some(Window {
                used_percentage: 99.0,
                resets_at: 1_788_758_370,
                observed_at: 1_788_423_353,
                overage_covered: false,
                limit_reached: false,
            }),
        }
    }

    #[test]
    fn a_real_codex_rollout_tree_replaces_a_day_old_stored_reading() {
        let tmp = tempfile::tempdir().unwrap();
        let state = StateDir::from_root(tmp.path().to_path_buf());
        let sessions = real_rollout_sessions_dir(tmp.path(), &["03", "04"]);
        store_for(&state, CODEX_USAGE_PROVIDER, &stale_openai_reading()).expect("seed stale");

        // Just after the 2026-09-04T05:56:31Z event in the newer rollout.
        refresh_codex_usage(&state, Some(sessions.as_path()), 1_788_501_500, 900);

        let seven = load_for(&state, CODEX_USAGE_PROVIDER)
            .expect("stored")
            .seven_day
            .expect("seven_day");
        assert_eq!(seven.used_percentage, 0.0);
        assert_eq!(seven.resets_at, 1_789_106_188);
        assert_eq!(seven.observed_at, 1_788_501_391);
    }

    #[test]
    fn a_real_codex_rollout_tree_holding_only_the_older_day_stores_that_day() {
        let tmp = tempfile::tempdir().unwrap();
        let state = StateDir::from_root(tmp.path().to_path_buf());
        let sessions = real_rollout_sessions_dir(tmp.path(), &["03"]);
        store_for(&state, CODEX_USAGE_PROVIDER, &stale_openai_reading()).expect("seed stale");

        refresh_codex_usage(&state, Some(sessions.as_path()), 1_788_501_500, 900);

        let seven = load_for(&state, CODEX_USAGE_PROVIDER)
            .expect("stored")
            .seven_day
            .expect("seven_day");
        assert_eq!(seven.used_percentage, 100.0);
        assert_eq!(seven.resets_at, 1_788_758_371);
    }

    /// Issue #155, Phase 2: per-session spend in the four raw classes, over a
    /// trailing window. `sum_transcripts` already walks every transcript
    /// including `subagents/`, because those tokens are charged too -- this
    /// keeps the same walk and stops throwing the file identity away.
    #[test]
    fn session_spend_reports_each_transcript_separately_in_raw_classes() {
        let root = tempfile::tempdir().expect("tempdir");
        std::fs::write(
            root.path().join("sess-a.jsonl"),
            concat!(
                r#"{"type":"assistant","timestamp":"2026-08-26T10:00:00Z","message":{"usage":"#,
                r#"{"input_tokens":10,"cache_creation_input_tokens":100,"#,
                r#""cache_read_input_tokens":900,"output_tokens":5}}}"#,
            ),
        )
        .expect("write");
        std::fs::write(
            root.path().join("sess-b.jsonl"),
            concat!(
                r#"{"type":"assistant","timestamp":"2026-08-26T10:00:00Z","message":{"usage":"#,
                r#"{"input_tokens":7,"output_tokens":1}}}"#,
            ),
        )
        .expect("write");

        let now = parse_iso8601_utc("2026-08-26T11:00:00Z").expect("now");
        let state_dir = tempfile::tempdir().expect("state tempdir");
        let state = StateDir::from_root(state_dir.path().to_path_buf());
        let mut spend = session_spend(&state, root.path(), now, 86_400);
        spend.sort_by(|a, b| a.session.cmp(&b.session));
        assert_eq!(spend.len(), 2);
        assert_eq!(spend[0].session, "sess-a");
        assert_eq!(spend[0].cache_read_input_tokens, 900);
        assert_eq!(spend[0].cache_creation_input_tokens, 100);
        assert_eq!(spend[1].session, "sess-b");
        assert_eq!(spend[1].cache_read_input_tokens, 0);
    }

    /// A row older than the window is not counted -- and a session whose rows
    /// are ALL outside it does not appear at all, rather than appearing as a
    /// zero.
    #[test]
    fn session_spend_drops_a_session_with_nothing_inside_the_window() {
        let root = tempfile::tempdir().expect("tempdir");
        std::fs::write(
            root.path().join("stale.jsonl"),
            concat!(
                r#"{"type":"assistant","timestamp":"2026-08-01T10:00:00Z","message":{"usage":"#,
                r#"{"input_tokens":10,"output_tokens":5}}}"#,
            ),
        )
        .expect("write");
        let now = parse_iso8601_utc("2026-08-26T11:00:00Z").expect("now");
        let state_dir = tempfile::tempdir().expect("state tempdir");
        let state = StateDir::from_root(state_dir.path().to_path_buf());
        assert!(session_spend(&state, root.path(), now, 86_400).is_empty());
    }

    /// Issue #779: the whole point of the cache is that `sum_transcripts`/
    /// `session_spend` report EXACTLY what a full, uncached re-parse
    /// (`sum_file`/`session_spend_of` on the raw text directly) would --
    /// never a shortcut that happens to look close. Also proves a second,
    /// fully-cached call reproduces the first call's own numbers, which is
    /// the failure mode a caching layer would actually introduce (answering
    /// differently once warm).
    #[test]
    fn the_cache_matches_a_full_reparse_for_both_readers() {
        let sess_a = concat!(
            "{\"type\":\"assistant\",\"timestamp\":\"2026-08-26T09:00:00Z\",\"message\":{\"usage\":",
            "{\"input_tokens\":10,\"cache_creation_input_tokens\":100,",
            "\"cache_read_input_tokens\":900,\"output_tokens\":5}}}\n",
            "{\"type\":\"assistant\",\"timestamp\":\"2026-08-26T10:30:00Z\",\"message\":{\"usage\":",
            "{\"input_tokens\":3,\"output_tokens\":2}}}\n",
        );
        // Outside `session_spend`'s 24h window but inside `sum_transcripts`'
        // 7-day one -- exercises both readers' own window boundary alike.
        let sess_b = concat!(
            "{\"type\":\"assistant\",\"timestamp\":\"2026-08-20T10:00:00Z\",\"message\":{\"usage\":",
            "{\"input_tokens\":7,\"output_tokens\":1}}}\n",
        );

        let tmp = tempfile::tempdir().expect("tempdir");
        let projects = tmp.path().join("projects");
        std::fs::create_dir_all(&projects).expect("mkdir");
        std::fs::write(projects.join("sess-a.jsonl"), sess_a).expect("write a");
        std::fs::write(projects.join("sess-b.jsonl"), sess_b).expect("write b");

        let now = parse_iso8601_utc("2026-08-26T11:00:00Z").expect("now");
        let state_dir = tempfile::tempdir().expect("state tempdir");
        let state = StateDir::from_root(state_dir.path().to_path_buf());

        // Reference: a direct, uncached full re-parse. `sum_file` itself
        // never touches `files_scanned` (its caller, `sum_transcripts`, is
        // the one that counts files), so the reference increments it the
        // same way, once per file, to stay comparable.
        let mut expected_sums = TokenSums::default();
        sum_file(sess_a, now, false, &mut expected_sums);
        expected_sums.files_scanned += 1;
        sum_file(sess_b, now, false, &mut expected_sums);
        expected_sums.files_scanned += 1;
        let mut expected_spend: Vec<SessionSpend> = [
            session_spend_of("sess-a", sess_a, now, 86_400),
            session_spend_of("sess-b", sess_b, now, 86_400),
        ]
        .into_iter()
        .flatten()
        .collect();
        expected_spend.sort_by(|a, b| a.session.cmp(&b.session));

        // Under test: the cached, directory-walking readers.
        let got_sums = sum_transcripts(&state, &projects, now, false);
        let mut got_spend = session_spend(&state, &projects, now, 86_400);
        got_spend.sort_by(|a, b| a.session.cmp(&b.session));

        assert_eq!(got_sums, expected_sums);
        assert_eq!(got_spend, expected_spend);

        // Warm second call, nothing on disk changed: identical numbers.
        let got_sums_again = sum_transcripts(&state, &projects, now, false);
        let mut got_spend_again = session_spend(&state, &projects, now, 86_400);
        got_spend_again.sort_by(|a, b| a.session.cmp(&b.session));
        assert_eq!(got_sums_again, got_sums);
        assert_eq!(got_spend_again, got_spend);
    }

    /// The incremental half of the cache (issue #779): a transcript that
    /// grows between two calls -- exactly what an actively-written session
    /// does -- must have its cache entry advance to the new length and gain
    /// the new row, not just its old length and row.
    #[test]
    fn a_transcripts_cache_entry_advances_its_parsed_offset_as_the_file_grows() {
        let now = 1_785_507_315;
        let tmp = tempfile::tempdir().expect("tempdir");
        let projects = tmp.path().join("projects");
        std::fs::create_dir_all(&projects).expect("mkdir");
        let transcript = projects.join("sess.jsonl");
        std::fs::write(&transcript, transcript_with_ages(now, &[600], 100)).expect("write first");

        let state_dir = tempfile::tempdir().expect("state tempdir");
        let state = StateDir::from_root(state_dir.path().to_path_buf());

        let first = sum_transcripts(&state, &projects, now, false);
        assert_eq!(first.five_hour, 100);

        let cache_path = transcript_cache_path(&state, &transcript);
        let cached: CachedTranscript =
            serde_json::from_str(&std::fs::read_to_string(&cache_path).expect("cache written"))
                .expect("cache parses");
        let first_len = std::fs::metadata(&transcript).expect("meta").len();
        assert_eq!(
            cached.parsed_len, first_len,
            "a fresh parse consumes the whole file"
        );
        assert_eq!(cached.events.len(), 1);

        // Grow the file the way an actively-written transcript does: append,
        // never rewrite what is already there.
        let mut appended = std::fs::read_to_string(&transcript).expect("read");
        appended.push_str(&transcript_with_ages(now, &[500], 50));
        std::fs::write(&transcript, &appended).expect("write grown");

        let second = sum_transcripts(&state, &projects, now, false);
        assert_eq!(
            second.five_hour, 150,
            "both rows must be counted after growth"
        );

        let cached_after: CachedTranscript = serde_json::from_str(
            &std::fs::read_to_string(&cache_path).expect("cache written again"),
        )
        .expect("cache parses");
        let grown_len = std::fs::metadata(&transcript).expect("meta").len();
        assert_eq!(
            cached_after.parsed_len, grown_len,
            "the cache advances to the file's new length, not just its old one"
        );
        assert_eq!(
            cached_after.events.len(),
            2,
            "the appended row extends, rather than replaces, the cached events"
        );
    }

    /// The invalidation half (issue #779): a transcript that is now SHORTER
    /// than the cached `parsed_len` (log rotation or truncation, never
    /// ordinary growth) must be reparsed from byte 0, not resumed from an
    /// offset that no longer exists in the file.
    #[test]
    fn a_shrunk_transcript_invalidates_its_cache_and_is_reparsed_from_scratch() {
        let now = 1_785_507_315;
        let tmp = tempfile::tempdir().expect("tempdir");
        let projects = tmp.path().join("projects");
        std::fs::create_dir_all(&projects).expect("mkdir");
        let transcript = projects.join("sess.jsonl");
        let long = transcript_with_ages(now, &[600, 500, 400], 1000);
        std::fs::write(&transcript, &long).expect("write long");

        let state_dir = tempfile::tempdir().expect("state tempdir");
        let state = StateDir::from_root(state_dir.path().to_path_buf());

        let first = sum_transcripts(&state, &projects, now, false);
        assert_eq!(first.five_hour, 3000, "three rows of 1000 each");

        // Replace with SHORTER, entirely different content.
        let rotated = transcript_with_ages(now, &[100], 7);
        assert!(
            (rotated.len() as u64) < (long.len() as u64),
            "the replacement must actually be shorter for this test to prove anything"
        );
        std::fs::write(&transcript, &rotated).expect("write rotated");

        let second = sum_transcripts(&state, &projects, now, false);
        assert_eq!(
            second.five_hour, 7,
            "the rotated file's own single row, not the old total plus or instead of it"
        );

        let cache_path = transcript_cache_path(&state, &transcript);
        let cached: CachedTranscript =
            serde_json::from_str(&std::fs::read_to_string(&cache_path).expect("cache written"))
                .expect("cache parses");
        let rotated_len = std::fs::metadata(&transcript).expect("meta").len();
        assert_eq!(cached.parsed_len, rotated_len);
        assert_eq!(
            cached.events.len(),
            1,
            "the stale pre-rotation events must not survive"
        );
    }

    /// The same-length rewrite (issue #779): `parsed_len` alone cannot tell a
    /// transcript rewritten to the EXACT same byte length apart from an
    /// untouched one, and serving the old cache entry in that case would
    /// report the pre-rewrite numbers forever. The mtime is bumped explicitly
    /// rather than relying on real-clock/filesystem resolution, so the test
    /// is deterministic.
    #[test]
    fn a_same_length_rewrite_with_a_new_mtime_is_reparsed_not_served_stale() {
        let now = 1_785_507_315;
        let tmp = tempfile::tempdir().expect("tempdir");
        let projects = tmp.path().join("projects");
        std::fs::create_dir_all(&projects).expect("mkdir");
        let transcript = projects.join("sess.jsonl");
        let original = transcript_with_ages(now, &[600], 100);
        std::fs::write(&transcript, &original).expect("write first");

        let state_dir = tempfile::tempdir().expect("state tempdir");
        let state = StateDir::from_root(state_dir.path().to_path_buf());

        let first = sum_transcripts(&state, &projects, now, false);
        assert_eq!(first.five_hour, 100);

        // Rewrite with different content of the EXACT same byte length (same
        // digit width on the token count), then bump the mtime forward
        // explicitly so the test does not depend on the filesystem's mtime
        // resolution or clock granularity.
        let rewritten = transcript_with_ages(now, &[600], 900);
        assert_eq!(
            rewritten.len(),
            original.len(),
            "the rewrite must be same-length for this test to prove anything"
        );
        std::fs::write(&transcript, &rewritten).expect("write rewritten");
        let file = std::fs::OpenOptions::new()
            .write(true)
            .open(&transcript)
            .expect("reopen for mtime bump");
        let bumped = std::time::SystemTime::now() + std::time::Duration::from_secs(120);
        file.set_modified(bumped).expect("set mtime");

        let second = sum_transcripts(&state, &projects, now, false);
        assert_eq!(
            second.five_hour, 900,
            "a same-length rewrite with a new mtime must be reparsed, not served from the stale cache"
        );
    }

    /// F3 (codex review, cff7ff57 follow-up): a stat/open race. A caller
    /// stats `transcript`'s path (file A) and passes that snapshot in as
    /// `current_len`/`current_mtime`, but by the time this function's own
    /// `File::open` resolves the path, it has been replaced with a
    /// DIFFERENT, unrelated file (B, log rotation or a rewritten
    /// checkpoint). Before this fix, the stale `cached.parsed_len` from A
    /// was seeked into the freshly-opened B regardless -- when B is at
    /// least that long (as here), the bytes read from that offset are B's
    /// own unrelated content, not a continuation of A, and got appended
    /// onto A's already-cached events as if they were. Simulated
    /// deterministically (no real race needed) by calling `transcript_events`
    /// directly with a stale, pre-replace `current_mtime` against a path
    /// that has since been overwritten.
    #[test]
    fn transcript_events_restarts_from_zero_when_the_open_handle_disagrees_with_the_caller_stat() {
        let now = 1_785_507_315;
        let tmp = tempfile::tempdir().expect("tempdir");
        let projects = tmp.path().join("projects");
        std::fs::create_dir_all(&projects).expect("mkdir");
        let transcript = projects.join("sess.jsonl");
        let original = transcript_with_ages(now, &[600], 100);
        std::fs::write(&transcript, &original).expect("write original");
        let stale_mtime = std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1);
        std::fs::OpenOptions::new()
            .write(true)
            .open(&transcript)
            .expect("reopen for mtime pin")
            .set_modified(stale_mtime)
            .expect("pin mtime");

        let state_dir = tempfile::tempdir().expect("state tempdir");
        let state = StateDir::from_root(state_dir.path().to_path_buf());

        let original_len = original.len() as u64;
        let first = transcript_events(&state, &transcript, original_len, Some(stale_mtime));
        assert_eq!(first.len(), 1);

        // Replace the path with an entirely unrelated, LONGER file -- three
        // rows nothing to do with the original, the way a rewritten
        // checkpoint might land. Its real mtime is left as "now" (whatever
        // this write sets it to), deliberately NOT pinned, so it provably
        // differs from `stale_mtime` below.
        let replacement = transcript_with_ages(now, &[900, 800, 700], 9999);
        assert!(
            (replacement.len() as u64) >= original_len,
            "the replacement must be at least as long as the original for this test to exercise \
             a successful seek into it, not just an empty read past EOF"
        );
        std::fs::write(&transcript, &replacement).expect("write replacement");

        // Deliberately pass a `current_len` that forces the incremental
        // (open+seek) path -- not the "nothing changed" fast path that
        // never opens the file at all -- alongside the STALE, pre-replace
        // mtime: exactly what a caller that stat'd path A just before the
        // replace, and is only now asking this function to process it,
        // would still be holding.
        let events = transcript_events(&state, &transcript, original_len + 1, Some(stale_mtime));
        assert_eq!(
            events.len(),
            3,
            "the replacement file's own three fresh rows, never the stale first row glued to a \
             slice of the replacement's unrelated tail bytes: {events:?}"
        );
        assert!(
            events.iter().all(|event| event.input_tokens == 9999),
            "every counted row must be the replacement's own, not a stale or spliced one: \
             {events:?}"
        );
    }

    /// F2 (codex review, cff7ff57 follow-up): a live transcript can be read
    /// mid-append, leaving a partial, unterminated JSON row as the last
    /// bytes on disk. Before this fix, that row was silently skipped (it
    /// fails to parse) but `parsed_len` still advanced past it, so once the
    /// writer finished appending the rest of the row, the next read started
    /// AFTER it and only ever saw the row's suffix -- that row's usage was
    /// lost for good, not merely delayed.
    #[test]
    fn a_partial_trailing_row_is_held_back_until_it_completes_not_lost() {
        let now = 1_785_507_315;
        let tmp = tempfile::tempdir().expect("tempdir");
        let projects = tmp.path().join("projects");
        std::fs::create_dir_all(&projects).expect("mkdir");
        let transcript = projects.join("sess.jsonl");

        let complete = transcript_with_ages(now, &[600], 100);
        let full_second_row = transcript_with_ages(now, &[500], 50);
        // Simulate a writer mid-append: the second row's bytes are cut
        // short, with no closing brace and no trailing newline.
        let partial_row = &full_second_row[..full_second_row.len() - 15];
        assert!(
            !partial_row.ends_with('\n'),
            "the fixture itself must be a genuinely unterminated partial row"
        );
        std::fs::write(&transcript, format!("{complete}{partial_row}")).expect("write partial");

        let state_dir = tempfile::tempdir().expect("state tempdir");
        let state = StateDir::from_root(state_dir.path().to_path_buf());

        let first = sum_transcripts(&state, &projects, now, false);
        assert_eq!(
            first.five_hour, 100,
            "the partial trailing row must not be counted while it is still incomplete"
        );

        // The writer finishes the row and appends its trailing newline.
        std::fs::write(&transcript, format!("{complete}{full_second_row}"))
            .expect("write completed");

        let second = sum_transcripts(&state, &projects, now, false);
        assert_eq!(
            second.five_hour, 150,
            "the row must be counted once it completes, not lost because parsed_len already \
             skipped past its partial bytes"
        );
    }

    /// Issue #779: `<state>/usage-scan/` used to grow forever -- a cache
    /// entry for a transcript that no longer exists (deleted, or a fixture
    /// cleaned up between runs) had nothing to ever remove it.
    #[test]
    fn an_orphaned_cache_entry_for_a_deleted_transcript_is_pruned() {
        let now = 1_785_507_315;
        let tmp = tempfile::tempdir().expect("tempdir");
        let projects = tmp.path().join("projects");
        std::fs::create_dir_all(&projects).expect("mkdir");

        let state_dir = tempfile::tempdir().expect("state tempdir");
        let state = StateDir::from_root(state_dir.path().to_path_buf());

        // A cache entry for a transcript that has since been deleted (or
        // never existed under this exact path).
        let ghost_transcript = projects.join("deleted-session.jsonl");
        let cache_path = transcript_cache_path(&state, &ghost_transcript);
        std::fs::create_dir_all(cache_path.parent().expect("cache dir")).expect("mkdir cache dir");
        let orphan = CachedTranscript {
            version: TRANSCRIPT_CACHE_VERSION,
            path: ghost_transcript.display().to_string(),
            parsed_len: 42,
            mtime_nanos: Some(1),
            last_response_id: None,
            events: Vec::new(),
        };
        std::fs::write(
            &cache_path,
            serde_json::to_string(&orphan).expect("serialize orphan"),
        )
        .expect("write orphan cache entry");
        assert!(
            cache_path.exists(),
            "precondition: the orphaned cache entry is on disk"
        );

        // No transcripts exist at all; the walk still runs its once-per-call
        // prune of the cache directory.
        let _ = sum_transcripts(&state, &projects, now, false);

        assert!(
            !cache_path.exists(),
            "a cache entry whose source transcript no longer exists must be pruned"
        );
    }
}
