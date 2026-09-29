//! bar support for the interactive supervisor.

use super::*;

/// Bar state degrades once; recovery clears its row and restores child size exactly once.
pub(super) struct BarRuntime {
    pub(super) chrome: super::chrome::Chrome,
    pub(super) disabled: bool,
    recovered: bool,
    pub(super) last_text: Option<String>,
    last_draw: Instant,
    pub(super) harness: String,
    /// Provider-specific usage source for this session.
    pub(super) provider: String,
    /// Short session id used to scope its unread mail count.
    pub(super) session_short: String,
    pub(super) mail_enabled: bool,
    pub(super) stdout_lock: std::sync::Arc<std::sync::Mutex<()>>,
    pub(super) rows: u16,
    pub(super) cols: u16,
    /// Last passive codex scan; zero permits an immediate first scan.
    last_codex_scan: u64,
    /// Stored reading age required before a passive codex scan.
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
            // A fresh process can have less than one second of uptime; subtraction
            // must not panic when the first draw is due.
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
            // The first scan is due immediately.
            last_codex_scan: 0,
            collector_max_age_secs,
        }
    }

    pub(super) fn active(&self) -> bool {
        self.chrome.bar && !self.disabled
    }
}

/// Disk-backed bar data is read at most once per redraw interval.
const BAR_THROTTLE: Duration = Duration::from_secs(1);

/// Passive rollout scans are costlier than redraws and never use HTTP here.
/// Share the scan floor with `pace::refresh_sources`.
use super::window::CODEX_SCAN_FLOOR_SECS as CODEX_BAR_SCAN_SECS;

/// Update dimensions before disabling: reset must address the current row,
/// or a terminal may clamp the cursor and erase child output.
pub(super) fn disable_bar_at_current_size(bar: &mut BarRuntime, size: (u16, u16)) {
    bar.cols = size.0;
    bar.rows = size.1;
    bar.disabled = true;
}

/// Clear the reserved row on degradation or final cleanup; no-op if unused.
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
    // A failed reset leaves the scroll region active; the emergency handler
    // must still clear it.
    if reset_ok {
        super::term::set_bar_active(false);
    }
}

/// On degradation, clear the bar and restore the full child pty size once;
/// subsequent resizes must also use the full size.
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

/// Redraws changed bar text after the throttle; disk reads stay off the byte
/// pump, and a write failure disables only the bar.
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

    // A wrapped codex session lacks a statusline tee; scan rollouts at a
    // bounded rate, with no network call on the redraw path.
    if bar.provider == super::window::CODEX_USAGE_PROVIDER {
        let now_secs = super::state::now_secs();
        if now_secs.saturating_sub(bar.last_codex_scan) >= CODEX_BAR_SCAN_SECS {
            bar.last_codex_scan = now_secs;
            // Use the overridable home path; Windows known-folder lookup ignores
            // HOME and USERPROFILE overrides.
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

    // Read this provider's usage only; expired windows must not render as
    // current readings. Keep this redraw path free of scans and network calls.
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
