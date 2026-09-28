//! bar support for the interactive supervisor.

use super::*;

/// Mutable T12b bookkeeping for the reserved status bar, bundled into one
/// struct rather than further widening `pump`'s already-long parameter list.
/// `disabled` is a one-way switch exactly like `InjectionState::degraded`:
/// once a probe, lock or write failure sets it, nothing here ever clears it
/// again, and the child is never touched by that decision (see
/// `chrome::after_redraw_attempt`). `recovered` (B1) is a second, later
/// one-way switch: it tracks whether the *disable* has already been reacted
/// to (row cleared, pty widened back to full size), so that reaction runs
/// exactly once no matter which caller noticed the disable first.
pub(super) struct BarRuntime {
    pub(super) chrome: super::chrome::Chrome,
    pub(super) disabled: bool,
    recovered: bool,
    pub(super) last_text: Option<String>,
    last_draw: Instant,
    pub(super) harness: String,
    /// The resolved adapter's own `AgentAdapter::provider()` (`"anthropic"`
    /// for claude, `"openai"` for codex), so `redraw_bar_if_due` reads the
    /// usage window for *this* session's account rather than the legacy
    /// unscoped file every session used to share. See `window::has_no_usage_
    /// source`/`load_for` and [[Usage and Pacing]].
    pub(super) provider: String,
    /// This session's own short id (`sessions::short_id`'s vocabulary), used
    /// to scope the mail count `unread_mail_count` reads to messages this
    /// session may actually see, not every session's mail in the repo.
    pub(super) session_short: String,
    pub(super) mail_enabled: bool,
    pub(super) stdout_lock: std::sync::Arc<std::sync::Mutex<()>>,
    pub(super) rows: u16,
    pub(super) cols: u16,
    /// Unix-seconds timestamp of the last passive codex rollout scan this
    /// bar ran (`0` means never), throttled to `CODEX_BAR_SCAN_SECS` --
    /// wrap's own version of the freshness a wrapped codex session would
    /// otherwise only get from an inner session's own pacing gate or a
    /// statusline tee it does not have.
    last_codex_scan: u64,
    /// Copied from `cfg.pace.collector_max_age_secs` at construction: how
    /// stale a stored codex reading has to be before `refresh_codex_usage`
    /// bothers scanning at all (its own internal staleness gate, separate
    /// from `CODEX_BAR_SCAN_SECS`'s call-rate floor).
    collector_max_age_secs: u64,
}

impl BarRuntime {
    #[allow(clippy::too_many_arguments)]
    pub(super) fn new(
        chrome: super::chrome::Chrome,
        harness: String,
        provider: String,
        session_short: String,
        mail_enabled: bool,
        stdout_lock: std::sync::Arc<std::sync::Mutex<()>>,
        size: (u16, u16),
        collector_max_age_secs: u64,
    ) -> Self {
        Self {
            disabled: !chrome.bar,
            chrome,
            recovered: false,
            // Never drawn yet, so the first due check always draws. Process
            // uptime under a second (a fast test run, a fresh container)
            // would make a bare subtraction panic (an abort, on the release
            // profile this ships with); `checked_sub` degrades to "draw
            // immediately" instead, which is the same outcome a real elapsed
            // second would have produced anyway.
            last_text: None,
            last_draw: Instant::now()
                .checked_sub(BAR_THROTTLE)
                .unwrap_or_else(Instant::now),
            harness,
            provider,
            session_short,
            mail_enabled,
            stdout_lock,
            cols: size.0,
            rows: size.1,
            // Never scanned yet, so the very first redraw due is eligible
            // immediately (`now.saturating_sub(0) >= CODEX_BAR_SCAN_SECS` is
            // true for any real `now`).
            last_codex_scan: 0,
            collector_max_age_secs,
        }
    }

    pub(super) fn active(&self) -> bool {
        self.chrome.bar && !self.disabled
    }
}

/// The 1s redraw throttle: usage and mail are read from disk only when this
/// has elapsed, never on the byte-pump path.
const BAR_THROTTLE: Duration = Duration::from_secs(1);

/// Floor between a wrapped codex session's own passive rollout scans
/// (`BarRuntime::last_codex_scan`), independent of `BAR_THROTTLE`'s 1s
/// redraw cadence: a rollout scan is a real filesystem walk, not a single
/// stat call, so it must not run on every redraw tick just because the bar
/// text happened to change. Never HTTP -- see `redraw_bar_if_due`.
///
/// Item 5: shared with `pace::refresh_sources`'s own codex scan floor via
/// `window::CODEX_SCAN_FLOOR_SECS` rather than a second, independently
/// numbered constant.
use super::window::CODEX_SCAN_FLOOR_SECS as CODEX_BAR_SCAN_SECS;

/// Item 2 (regression fix): applies a `ResizeDecision::disables_bar` outcome
/// to `bar`'s own bookkeeping. The dims move to the *current* size *before*
/// `disabled` flips, not after or never: `reset_bar`'s later `bar_reset_
/// sequence(bar.rows)` (both the recovery call right after a disabling
/// resize, and the final session cleanup) addresses `bar.rows` to clear the
/// reserved row, and a stale, larger row number left over from before the
/// shrink points past the now-smaller terminal. A real terminal clamps an
/// out-of-range cursor move to its own last row and blanks a line of the
/// child's own live output there instead of the row that actually used to
/// hold the bar.
pub(super) fn disable_bar_at_current_size(bar: &mut BarRuntime, size: (u16, u16)) {
    bar.cols = size.0;
    bar.rows = size.1;
    bar.disabled = true;
}

/// Writes the reset sequence: region cleared, reserved row blanked. Called
/// from two places -- the final cleanup alongside `RawGuard::restore`, and
/// (B1) `recover_bar_to_full_size` the moment the bar degrades mid-session --
/// so callers decide when it is due; this just performs it. A no-op when the
/// bar was never eligible in the first place.
pub(super) fn reset_bar(bar: &BarRuntime) {
    if !bar.chrome.bar {
        return;
    }
    let sequence = super::chrome::bar_reset_sequence(bar.rows);
    let mut reset_ok = false;
    if let Ok(_guard) = bar.stdout_lock.lock() {
        let mut stdout = std::io::stdout();
        reset_ok = stdout
            .write_all(sequence.as_bytes())
            .and_then(|()| stdout.flush())
            .is_ok();
    }
    // F4: `BAR_ACTIVE` means "the real console currently has a scroll region
    // we set", so it is cleared only once the undo actually landed. A reset
    // that failed leaves the console still fenced, and the emergency handler
    // still owes it a `CSI r`.
    if reset_ok {
        super::term::set_bar_active(false);
    }
}

/// B1 (blocking fix): the moment the bar shows disabled -- whichever caller
/// noticed first, a too-small resize or a redraw/lock failure with no resize
/// event of its own -- this clears the reserved row exactly once and widens
/// the child pty to the full current size. Without it the pty stayed pinned
/// at its last reserved height forever, and a later widen never reached the
/// child: a degraded session must behave exactly like a bar-less one from
/// here on, including tracking every resize after this at full size.
/// Idempotent (`bar.recovered` guards it), so calling this every tick is
/// cheap and safe.
pub(super) fn recover_bar_to_full_size(
    bar: &mut BarRuntime,
    pair: &mut portable_pty::PtyPair,
    size: (u16, u16),
) {
    if !super::chrome::bar_needs_recovery(bar.chrome.bar, bar.disabled, bar.recovered) {
        return;
    }
    bar.recovered = true;
    reset_bar(bar);
    let _ = pair.master.resize(PtySize {
        rows: size.1,
        cols: size.0,
        pixel_width: 0,
        pixel_height: 0,
    });
}

/// Redraws the bar when it is active, its rendered text actually changed,
/// and the 1s throttle has elapsed. Usage and mail are read from disk only
/// here, gated by that same throttle, never from the byte-pump path. Any
/// lock or write failure disables the bar permanently and leaves the child
/// untouched; `now` is threaded in rather than read internally so the
/// throttle itself stays testable without a real clock.
pub(super) fn redraw_bar_if_due(
    bar: &mut BarRuntime,
    supervision: &InjectionState,
    state_dir: &super::state::StateDir,
    repo: &Path,
    now: Instant,
) {
    if !bar.active() || now.duration_since(bar.last_draw) < BAR_THROTTLE {
        return;
    }
    bar.last_draw = now;

    // Passive refresh only: a wrapped codex session has no statusline tee, so
    // the bar would otherwise stay a permanent placeholder for its entire
    // life. Scans are floored to once per `CODEX_BAR_SCAN_SECS`; HTTP stays
    // off this path entirely -- `wrap` must never make a session worse, and
    // a network call on the redraw path could stall it.
    if bar.provider == super::window::CODEX_USAGE_PROVIDER {
        let now_secs = super::state::now_secs();
        if now_secs.saturating_sub(bar.last_codex_scan) >= CODEX_BAR_SCAN_SECS {
            bar.last_codex_scan = now_secs;
            // Resolved via `crate::utils::home_dir()`, the same as `pace::
            // refresh_sources` and `usage.rs`'s own refresh -- not left to
            // `refresh_codex_usage`'s internal `dirs::home_dir()` fallback,
            // which calls `SHGetKnownFolderPath` directly on Windows and so
            // ignores `HOME`/`USERPROFILE`, the one thing a test's
            // `HomeGuard` can actually override.
            let sessions_dir = crate::utils::home_dir()
                .ok()
                .map(|h| h.join(".codex").join("sessions"));
            super::window::refresh_codex_usage(
                state_dir,
                sessions_dir.as_deref(),
                now_secs,
                bar.collector_max_age_secs,
            );
        }
    }

    // Per-provider since this fix: `window::load` is the legacy machine-wide
    // file every session used to share, so a wrapped codex session's bar
    // used to render whatever Anthropic numbers a claude session happened to
    // leave there. `load_for` falls back to that same legacy file for
    // claude's own provider (`anthropic`), so this is a no-op for the common
    // case; for a provider with no usage source at all (codex/openai today)
    // it now reads as `UsageWindows::default()`, which `status_bar` already
    // renders as the placeholder dash, never a misleading `0%`. No renderer
    // change needed at all, only which windows are read.
    //
    // `window::available` drops any window whose `resets_at` has provably
    // passed (or, absent a `resets_at`, that has outlived its own span)
    // before the reading ever reaches the bar: a reading that old is a stale
    // number pretending to be current, exactly what motivated this filter.
    // Still a pure in-memory/file read -- no scan, no network -- on this
    // throttled redraw path.
    let windows = super::window::available(
        &super::window::load_for(state_dir, &bar.provider).unwrap_or_default(),
        super::state::now_secs(),
    );
    let usage_five_hour = windows.five_hour.map(|w| w.used_percentage);
    let usage_seven_day = windows.seven_day.map(|w| w.used_percentage);
    let unread_mail = unread_mail_counts(
        state_dir,
        repo,
        &bar.harness,
        &bar.session_short,
        bar.mail_enabled,
    );

    let state = super::chrome::BarState {
        harness: bar.harness.clone(),
        score: (supervision.signals_seen > 0).then_some(supervision.score),
        verdict: (supervision.signals_seen > 0).then_some(supervision.verdict),
        usage_five_hour,
        usage_seven_day,
        unread_mail,
        degraded: supervision.degraded,
    };
    let text = super::chrome::status_bar(&state, bar.cols, bar.chrome.colour);
    if !super::chrome::bar_text_changed(bar.last_text.as_deref(), &text) {
        return;
    }

    let sequence = super::chrome::bar_redraw_sequence(bar.rows, &text);
    let wrote = match bar.stdout_lock.lock() {
        Ok(_guard) => {
            let mut stdout = std::io::stdout();
            stdout
                .write_all(sequence.as_bytes())
                .and_then(|()| stdout.flush())
        }
        Err(_) => Err(std::io::Error::other("stdout lock poisoned")),
    };
    bar.disabled = super::chrome::after_redraw_attempt(bar.disabled, wrote.is_ok());
    if wrote.is_ok() {
        bar.last_text = Some(text);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn test_bar(size: (u16, u16)) -> BarRuntime {
        BarRuntime::new(
            super::super::chrome::Chrome {
                banner: true,
                bar: true,
                colour: false,
            },
            "claude".to_string(),
            "anthropic".to_string(),
            "sess0000".to_string(),
            true,
            std::sync::Arc::new(std::sync::Mutex::new(())),
            size,
            900,
        )
    }

    /// Finding 5 (2026-08-24 review): `run_with` used to hardcode
    /// `LaunchMode::Interactive` for both `compile()` and `policy_launch_
    /// args()` regardless of whether stdio was actually a terminal. This
    /// tests the corrected pure mapping directly -- no pty, no process
    /// stdio -- since that is the one piece of the fix that does not
    /// require an actual terminal to exercise.
    #[test]
    fn launch_mode_from_interactive_maps_the_boolean_to_the_right_mode() {
        assert_eq!(
            launch_mode_from_interactive(true),
            super::super::adapters::LaunchMode::Interactive
        );
        assert_eq!(
            launch_mode_from_interactive(false),
            super::super::adapters::LaunchMode::Headless
        );
    }

    /// Item 2 (Usage and Pacing follow-up): the status bar used to read the
    /// single legacy `usage.json` regardless of which adapter it was
    /// wrapping, so a codex (`openai`) session's bar rendered whatever
    /// Anthropic numbers a claude session happened to leave there. It now
    /// reads `window::load_for(state_dir, &bar.provider)`, so a codex
    /// session's usage segment must show the placeholder dash (no usage
    /// source) even while a real claude reading sits in the legacy file --
    /// and a claude session must still see it, via the same legacy-file
    /// fallback `load_for` already gives `anthropic`.
    #[test]
    fn a_codex_sessions_bar_does_not_inherit_claudes_own_legacy_usage_reading() {
        let tmp = tempfile::tempdir().expect("tempdir");
        // The codex bar below is due for its own passive rollout scan (never
        // scanned yet), which resolves `~/.codex/sessions` via `HOME`/
        // `USERPROFILE` -- must not be left pointed at this machine's real
        // home directory. An empty one yields no rollouts, so the assertions
        // below still hold: the passive scan itself is exercised (it runs,
        // finds nothing, leaves the stored state untouched), not bypassed.
        let home = tmp.path().join("home");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);
        let state = StateDir::from_root(tmp.path().join("state"));
        super::super::window::store(
            &state,
            &super::super::window::UsageWindows {
                five_hour: Some(super::super::window::Window {
                    used_percentage: 42.0,
                    resets_at: 0,
                    // Fresh, not epoch-1: this test is about the legacy-file
                    // fallback, not about `window::available`'s own staleness
                    // filter (covered separately), so the reading must still
                    // be inside its own five_hour span.
                    observed_at: super::super::state::now_secs(),
                    overage_covered: false,
                    limit_reached: false,
                }),
                seven_day: None,
            },
        )
        .expect("store the legacy (claude) reading");

        let mut codex_bar = test_bar((80, 1));
        codex_bar.provider = "openai".to_string();
        redraw_bar_if_due(
            &mut codex_bar,
            &InjectionState::new(),
            &state,
            tmp.path(),
            Instant::now(),
        );
        let codex_text = codex_bar.last_text.expect("the bar drew something");
        assert!(
            !codex_text.contains("42%"),
            "codex must not inherit claude's own legacy reading: {codex_text}"
        );
        assert!(
            codex_text.contains("\u{25d4} \u{2013}\u{b7}\u{2013}"),
            "and must show the placeholder, not a fabricated zero: {codex_text}"
        );

        let mut claude_bar = test_bar((80, 1));
        redraw_bar_if_due(
            &mut claude_bar,
            &InjectionState::new(),
            &state,
            tmp.path(),
            Instant::now(),
        );
        let claude_text = claude_bar.last_text.expect("the bar drew something");
        assert!(
            claude_text.contains("42%"),
            "claude keeps reading the legacy file via its own provider fallback: {claude_text}"
        );
    }

    /// A reading whose window has certainly reset (`resets_at` long past)
    /// must not render as a live percentage just because it is the newest
    /// thing on disk -- that is exactly the stale-14%-months-old bug this
    /// fix addresses. The bar must fall back to its usual placeholder, same
    /// as the true no-source case.
    #[test]
    fn a_bar_with_an_expired_window_shows_the_placeholder_not_a_stale_percent() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().join("state"));
        super::super::window::store(
            &state,
            &super::super::window::UsageWindows {
                five_hour: Some(super::super::window::Window {
                    used_percentage: 14.0,
                    resets_at: 1, // long past any real wall clock
                    observed_at: 1,
                    overage_covered: false,
                    limit_reached: false,
                }),
                seven_day: None,
            },
        )
        .expect("store an expired reading");

        let mut bar = test_bar((80, 1));
        redraw_bar_if_due(
            &mut bar,
            &InjectionState::new(),
            &state,
            tmp.path(),
            Instant::now(),
        );
        let text = bar.last_text.expect("the bar drew something");
        assert!(
            !text.contains("14%"),
            "an expired window must not render as a current percent: {text}"
        );
        assert!(
            text.contains("\u{25d4} \u{2013}\u{b7}\u{2013}"),
            "expired-and-nothing-else renders the placeholder: {text}"
        );
    }

    /// Item 2 (regression): a shrink that disables the bar must move
    /// `bar.rows`/`bar.cols` to the *current* size before disabling, so the
    /// reset sequence that follows (recovery, or the final session cleanup)
    /// addresses the row that is actually still on screen. Left stale (the
    /// old, larger row number), the real terminal clamps the out-of-range
    /// cursor move to its own last row and blanks a line of the child's own
    /// live output there.
    #[test]
    fn reset_after_a_shrink_degrade_targets_the_current_last_row() {
        let mut bar = test_bar((100, 30));
        assert_eq!(bar.rows, 30, "launched tall");

        disable_bar_at_current_size(&mut bar, (100, 5));

        assert_eq!(
            bar.rows, 5,
            "the dims move to the current size before disabling"
        );
        assert_eq!(bar.cols, 100);
        assert!(bar.disabled);

        let reset = super::super::chrome::bar_reset_sequence(bar.rows);
        assert!(
            reset.contains("\x1b[5;1H"),
            "the reset must address the CURRENT last row: {reset:?}"
        );
        assert!(
            !reset.contains("\x1b[30;1H"),
            "must not address the stale, now out-of-range row: {reset:?}"
        );
    }

    /// Item 5 (regression): a restart's fresh pty has to reserve the bottom
    /// row while the bar is still alive, exactly like the initial launch
    /// and an ordinary resize -- otherwise the freshly relaunched child gets
    /// the full terminal height and the bar immediately starts drawing over
    /// its own last line.
    #[test]
    fn a_relaunch_while_the_bar_is_active_keeps_the_reserved_row() {
        let bar = test_bar((100, 30));
        assert!(bar.active(), "sanity: freshly constructed and eligible");
        assert_eq!(
            relaunch_size(&bar, (100, 30)),
            (100, 29),
            "the relaunch must reserve the same row the live session already has"
        );
    }

    /// And once the bar has degraded, a restart is ordinary full-size
    /// forwarding, exactly like every other pty operation once `bar.active()`
    /// is false (B1).
    #[test]
    fn a_relaunch_after_the_bar_has_degraded_uses_the_full_size() {
        let mut bar = test_bar((100, 30));
        bar.disabled = true;
        assert_eq!(relaunch_size(&bar, (100, 30)), (100, 30));
    }
}
