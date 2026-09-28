//! Terminal lifecycle (panic hook, resize) and per-turn env/token setup.
use super::*;

/// Best-effort kitty keyboard-enhancement negotiation, requesting only
/// `DISAMBIGUATE_ESCAPE_CODES` -- never event-type/release reporting, which
/// would flood the per-tick input drain with a keydown+keyup pair for every
/// keystroke nothing here reads. Without this, a unix terminal sends a plain
/// `\r` for Shift+Enter and `encode_key`'s Shift+Enter branch can never see
/// the modifier at all: it is simply not on the wire.
///
/// Must run before anything starts reading stdin: `supports_keyboard_enhancement`'s
/// own docs say it blocks on the same terminal query/reply cycle `event::read`/
/// `poll` use, so calling it once the dashboard's own event loop (below) has
/// started would have the two race over the same bytes. Nothing else reads
/// stdin before `run_dashboard` calls this during setup.
///
/// Any probe or push failure is silent and leaves the terminal exactly as it
/// was -- this is an enhancement, never a requirement, matching this
/// dashboard's rule that a supervision/UI failure must never make a session
/// worse. Returns whether the push actually happened, so the caller knows
/// whether teardown owes the terminal a matching pop. On success also arms
/// `term::set_kbd_enhanced`, so a panic or an external kill that never
/// reaches `teardown_terminal` still knows to pop the stack entry it pushed.
pub(super) fn push_keyboard_enhancement() -> bool {
    let pushed = match supports_keyboard_enhancement() {
        Ok(true) => execute!(
            io::stdout(),
            PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES)
        )
        .is_ok(),
        _ => false,
    };
    if pushed {
        term::set_kbd_enhanced(true);
    }
    pushed
}

/// Restores the shared terminal on the way out of `run_dashboard`: disables
/// raw mode, then writes `term::dash_reset_bytes()` -- cursor shown, scroll
/// region un-fenced, alternate screen left -- to **stdout**, which is the
/// stream the alternate screen was entered on.
///
/// Showing the cursor is not optional and is not implied by leaving the
/// alternate screen: ratatui hides it on every frame it draws, and
/// `LeaveAlternateScreen` says nothing about cursor visibility, so before F4
/// every clean exit handed the operator a shell with an invisible cursor.
///
/// Idempotent, and called from every exit arm, matching the `RawGuard`/
/// `SessionGuard` precedent this plan's Global Constraints call for --
/// `panic = "abort"` in the release profile means `Drop` is not a safety
/// net here either. `keyboard_enhancement_pushed` is whatever
/// `push_keyboard_enhancement` returned during setup -- `false` at any call
/// site that could not have pushed yet (an abort before that point).
pub(super) fn teardown_terminal(keyboard_enhancement_pushed: bool) {
    term::set_dash_active(false);
    let _ = disable_raw_mode();
    if keyboard_enhancement_pushed {
        let _ = execute!(io::stdout(), PopKeyboardEnhancementFlags);
        term::set_kbd_enhanced(false);
    }
    let mut stdout = io::stdout();
    let _ = stdout.write_all(term::dash_reset_bytes());
    let _ = stdout.flush();
    // Belt and braces: crossterm's own sequence for the same thing, in case
    // a future crossterm emits something extra alongside `\x1b[?1049l`.
    // Leaving an alternate screen twice is a no-op. Mouse reporting needs no
    // equivalent here -- `term::dash_reset_bytes` above already turns off all
    // four modes, which is more than this dashboard ever turns on.
    let _ = execute!(stdout, LeaveAlternateScreen);
}

/// The hook that was installed before the dashboard replaced it, shared
/// between the dashboard's own hook (which chains into it) and
/// `restore_panic_hook` (which puts it back).
type PanicHook = Box<dyn Fn(&std::panic::PanicHookInfo<'_>) + Sync + Send + 'static>;

/// Puts the terminal back before the previous hook prints its message and the
/// process aborts, and hands back the hook it displaced so `restore_panic_hook`
/// can put exactly that one back. Three things the pre-F4 hook got wrong, all
/// of which left a panicking dashboard's operator with an unusable console:
///
/// 1. Raw mode was never disabled, so the shell that inherited the console
///    had no echo and no line editing.
/// 2. It wrote `term::emergency_reset_bytes(false)`, which is the **empty**
///    slice (see `term.rs`) -- so nothing was reset and the cursor was never
///    shown again.
/// 3. It wrote to stderr, but the alternate screen was entered on stdout.
///
/// A fourth: if `push_keyboard_enhancement` had succeeded, the kitty
/// keyboard-enhancement stack entry it pushed was never popped either --
/// `term::kbd_enhanced()` records whether that push happened, since this
/// hook is installed before the push and so cannot close over the answer.
pub(super) fn install_panic_hook() -> Arc<PanicHook> {
    let previous: Arc<PanicHook> = Arc::new(std::panic::take_hook());
    let chained = Arc::clone(&previous);
    std::panic::set_hook(Box::new(move |info| {
        let _ = disable_raw_mode();
        let mut stdout = io::stdout();
        let _ = stdout.write_all(term::dash_reset_bytes());
        if term::kbd_enhanced() {
            let _ = stdout.write_all(term::kbd_enhancement_pop_bytes());
        }
        let _ = stdout.flush();
        chained(info);
    }));
    previous
}

/// Puts back the hook that was in place before `install_panic_hook` ran.
///
/// N1: every exit arm used to call a bare `std::panic::take_hook()`, which
/// removes the dashboard's hook but installs **std's default** in its place --
/// so any hook the process had already chained in before the dashboard opened
/// (an outer supervisor's terminal restore, a test harness's own) was silently
/// dropped for the rest of the process's life. Taking and then re-setting the
/// captured one is what makes the dashboard's hook a genuine push/pop.
pub(super) fn restore_panic_hook(previous: &Arc<PanicHook>) {
    let _ = std::panic::take_hook();
    let previous = Arc::clone(previous);
    std::panic::set_hook(Box::new(move |info| previous(info)));
}

/// Dash refresh PR1: THE one effective-width value for `dash.sidebar_cols`
/// -- `0` (hidden) below 100 total columns unless `forced_visible`, else
/// the operator's own configured width. Every call site that used to read
/// `cfg.dash.sidebar_cols` directly for a geometry decision (pty resize,
/// `ui::layout`, `effective_main`) goes through this instead, so a narrow
/// terminal and a forced-visible toggle can never disagree about how wide
/// the sidebar actually is this frame.
pub(super) fn effective_sidebar_cols(
    cfg: &CtxConfig,
    frame_width: u16,
    forced_visible: bool,
) -> u16 {
    if ui::sidebar_hidden(frame_width, forced_visible) {
        0
    } else {
        cfg.dash.sidebar_cols
    }
}

/// The area a pane's grid actually renders into this frame: the full
/// terminal when `zoomed` (header and sidebar skipped entirely), otherwise
/// `ui::layout`'s own `main` rect.
pub(super) fn effective_main(area: Rect, sidebar_cols: u16, zoomed: bool) -> Rect {
    if zoomed {
        area
    } else {
        ui::layout(area, sidebar_cols).main
    }
}

/// Applies a new terminal size: stores it (`term_cols`/`term_rows`/`full`,
/// which the zoom handler and every `terminal::size` fallback read) and
/// resizes every pane's pty+parser to this size's effective main geometry.
///
/// M6: factored out of the `Event::Resize` arm so the render loop can call it
/// too. crossterm can coalesce or miss a resize event (a tmux SIGWINCH race, a
/// conhost buffer change), which used to leave the ptys pinned at the old
/// geometry forever; the renderer now compares the freshly-queried size to the
/// stored one every frame and reconciles through here when they differ.
#[allow(clippy::too_many_arguments)]
pub(super) fn apply_terminal_resize(
    cols: u16,
    rows: u16,
    sidebar_cols: u16,
    zoomed: bool,
    term_cols: &mut u16,
    term_rows: &mut u16,
    full: &mut Rect,
    panes: &mut [Pane],
    errors: &mut ErrorLog,
    selection: &mut Option<Selection>,
) {
    *term_cols = cols;
    *term_rows = rows;
    *full = Rect::new(0, 0, cols, rows);
    let m = effective_main(*full, sidebar_cols, zoomed);
    let new_size = (m.height.max(1), m.width.max(1));
    // MEDIUM (review): a resize is one of the ways a selection's grid
    // coordinates go stale -- see `cancel_selection_on_resize`. Read before
    // any pane is actually resized, since it compares against each pane's
    // *current* size.
    cancel_selection_on_resize(selection, panes, new_size);
    for pane in panes.iter_mut() {
        if let Err(e) = pane.resize(new_size.0, new_size.1) {
            push_error(errors, format!("resize: {e}"));
        }
    }
}

/// Builds the env a freshly spawned pane's child needs to report its own
/// turn boundaries: `adapter.register_turn_signal` against the pane's own
/// deterministic socket path (`state.socket_for`, the same derivation
/// `Pane::spawn` binds to internally), plus `AGENT_ENV` so a nested `zirv
/// ctx ...` call inside the pane's own children defaults to this pane's own
/// harness. Mirrors `wrap.rs`'s own `turn_env` assembly
/// (`wrap.rs:1072-1086`) faithfully.
///
/// A resolution failure degrades to `AGENT_ENV` alone (the pane still
/// spawns, still gets its own socket bound by `Pane::spawn`, but the child
/// is never told where to post -- exactly the same "unsupervised, never
/// supervised by somebody else" degrade every other supervisor in this
/// codebase already accepts) and is reported back as an error string for the
/// header rather than failing the spawn.
/// Whether the adapter resolved for `agent_name` reports a real turn-signal
/// mechanism (`AgentAdapter::capabilities().turn_signal`) -- what `Pane::spawn`
/// needs to pick between `pane::pane_is_idle`'s two branches (Task A: a
/// signal-less pane, codex today, is instead read by output quiescence).
///
/// A resolution failure -- the same failure `build_turn_env` right below
/// already reports back as an error string, and degrades to no signal
/// registration for -- reports `false` rather than `true`: no signal is ever
/// going to reach such a pane either way, so `false` at least leaves it
/// reachable once its output goes quiet, where `true` would read it as
/// signal-capable and leave it `Working` forever with nothing that could ever
/// clear that.
pub(super) fn turn_signal_capable_for(cfg: &CtxConfig, agent_name: &str) -> bool {
    adapters::select(Some(agent_name), &[], cfg)
        .map(|adapter| adapter.capabilities().turn_signal)
        .unwrap_or(false)
}

/// Builds a fresh pane's `turn_env`: the adapter's own turn-signal
/// registration (or its resolution-failure fallback), the pane's session
/// identity, and -- security review round (2026-08-28), review of issue
/// #160's own fix -- the durable interactive-launch pin, ALWAYS pushed here
/// rather than left to each of the three call sites to remember on their
/// own. Before this, `fulfill_spawn_request`, `run_dashboard`'s first pane,
/// and `spawn_restored_pane` each pushed `adapters::launch_mode_pin_env`
/// separately after calling this function -- three independent chances to
/// forget the pin, and issue #160 finding 1 was exactly that: the third
/// occurrence of the forgotten-pin bug class. `mode` is now a MANDATORY
/// parameter so a call site that forgets to decide it is a compile error,
/// not a silently-headless pane; `LaunchMode::Headless` already reads as
/// "no pin" through `launch_mode_pin_env`, so no separate `Option` is
/// needed to make "no pin" explicit -- the enum already has that variant.
pub(crate) fn build_turn_env(
    cfg: &CtxConfig,
    state: &StateDir,
    repo: &Path,
    agent_name: &str,
    session_id: &str,
    mode: adapters::LaunchMode,
) -> (Vec<(String, String)>, Option<String>) {
    let pin = adapters::launch_mode_pin_env(mode);
    // 2026-09-06: the mirror marker. A pane nobody vouched for is
    // `LaunchMode::Headless` -- fail-closed on permission prompts -- and
    // `workflow::engine::refusal_for` must read it the same way it reads an
    // `exec` child's, or the interactive `brainstorm` skill runs in a pane
    // with nobody able to answer it.
    let headless = adapters::headless_marker_env(mode);
    match adapters::select(Some(agent_name), &[], cfg) {
        Ok(adapter) => {
            let socket = state.socket_for(session_id);
            let setup = adapter.register_turn_signal(
                &SessionRef {
                    id: SessionId::parse(session_id),
                    cwd: repo.to_path_buf(),
                },
                &socket,
            );
            let mut env = setup.env;
            env.push((adapters::AGENT_ENV.to_string(), adapter.name().to_string()));
            // Issue #30, item 1: a worker pane's own session identity must
            // not depend on whether its adapter has a turn-signal mechanism
            // to register at all. `register_turn_signal` legitimately
            // returns an empty `env` for an adapter with no such mechanism
            // (codex today, `capabilities().turn_signal == false`) -- that
            // silence is correct for the socket/signal env it owns, but it
            // used to also leave `SESSION_ENV` entirely unset, so any `zirv
            // ctx send` such a pane ran recorded `identity_or_unknown`'s
            // `"unknown"` as its sender and had no address of its own for a
            // reply to be `--to-session`-directed at. A turn-signal-capable
            // adapter (claude) already sets this as part of its own `setup.
            // env`, so it is added here only when not already present,
            // rather than risking a duplicate entry.
            if !env.iter().any(|(k, _)| k == adapters::SESSION_ENV) {
                env.push((adapters::SESSION_ENV.to_string(), session_id.to_string()));
            }
            if let Some(pair) = pin {
                env.push(pair);
            }
            if let Some(pair) = headless {
                env.push(pair);
            }
            (env, None)
        }
        Err(e) => {
            let mut env = vec![(adapters::AGENT_ENV.to_string(), agent_name.to_string())];
            if let Some(pair) = pin {
                env.push(pair);
            }
            if let Some(pair) = headless {
                env.push(pair);
            }
            (
                env,
                Some(format!(
                    "dashboard: could not resolve adapter '{agent_name}' for turn signals: {e}"
                )),
            )
        }
    }
}

// Task 10: the spawn-request channel. A pane's own `zirv ctx agent`
// invocation (inheriting `DASH_REQUESTS_ENV` from its own turn_env, set up
// below) writes a `spawnreq::SpawnRequest` rather than running headless in
// the pane's own subshell; this dashboard fulfils it as a fresh worker pane
// using exactly the composed-prompt recipe `exec::run_with` uses for its own
// first launch (memory, then mail, then `with_mail_layer`), and answers with
// a `spawnreq::SpawnAck`.

/// A 16-hex-character capability token for this dashboard's own
/// spawn-request directory (`spawnreq::request_dir_for`). Freshly minted per
/// launch: unpredictable enough that a process never told this directory's
/// path cannot guess it, so only a pane that actually inherited
/// `DASH_REQUESTS_ENV` from this dashboard can reach its spawn channel.
pub(super) fn spawn_token() -> String {
    uuid::Uuid::new_v4().simple().to_string()[..16].to_string()
}

/// Security review Finding 1 (2026-08-28): one freshly minted intake
/// directory for a pane that is about to spawn -- its own capability token,
/// its own channel, nobody else's. The dashboard drains each pane's channel
/// separately, so "this request was in that directory" is what identifies the
/// requesting session; nothing about the requester is ever read out of the
/// request itself (see `fulfill_spawn_request`'s own lineage gate).
///
/// Created eagerly rather than left to `spawnreq::write_request`'s own lazy
/// `create_private_dir_all`: a pane's `agent::live_join_target` refuses a
/// `DASH_REQUESTS_ENV` directory that does not exist yet, and would then scan
/// for another live dashboard instead. A creation failure is therefore
/// narrated, not fatal -- the pane simply falls back to that scan (finding
/// this dashboard's own shared channel, where it can still ask for a plain
/// worker), which is the never-make-it-worse degradation this module holds
/// everywhere else.
pub(super) fn mint_pane_channel(requests_dir: &Path, errors: &mut ErrorLog) -> PathBuf {
    let dir = spawnreq::pane_request_dir_for(requests_dir, &spawn_token());
    if let Err(e) = super::state::create_private_dir_all(&dir) {
        push_error(
            errors,
            format!(
                "dashboard: could not create the spawn-request channel {}: {e}; this pane can \
                 only ask for plain worker panes",
                dir.display()
            ),
        );
    }
    dir
}

/// CROSS-CUTTING (shared with the supervisor): removes every
/// `<state>/dash/<short>-<token>` token directory whose `owner.pid` names a
/// process no longer alive -- a leak from a dashboard that exited abnormally
/// (external kill, closed window, panic). Left behind, such a dir (and the
/// `ZIRV_CTX_DASH_REQUESTS` a surviving pane shell still carries) reads to
/// `nested_session_evidence` as a live dashboard owning this terminal, and
/// refuses every future `zirv chat`.
///
/// Best-effort throughout: a dir with no `owner.pid` (a roster file, a token
/// dir still mid-creation), an unreadable or non-numeric pid, or a live one is
/// left untouched, and every filesystem error is ignored. Run at startup after
/// this dashboard has written its own `owner.pid`, so its own live dir is
/// always kept.
pub(super) fn sweep_stale_token_dirs(state: &StateDir) {
    let Ok(entries) = std::fs::read_dir(state.dash()) else {
        return;
    };
    for entry in entries.flatten() {
        let dir = entry.path();
        if !dir.is_dir() {
            continue;
        }
        let Ok(contents) = std::fs::read_to_string(dir.join("owner.pid")) else {
            continue;
        };
        let Ok(pid) = contents.trim().parse::<u32>() else {
            continue;
        };
        if !sessions::is_alive(pid) {
            let _ = std::fs::remove_dir_all(&dir);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::*;
    use super::*;

    #[test]
    fn effective_main_returns_the_full_area_when_zoomed() {
        let area = Rect::new(0, 0, 100, 30);
        let zoomed = effective_main(area, 24, true);
        assert_eq!(zoomed, area);
        let unzoomed = effective_main(area, 24, false);
        assert_eq!(unzoomed, ui::layout(area, 24).main);
    }

    /// F5: the draw target itself, not just the pty resize. Zoom used to
    /// resize every pane's pty to the full terminal and then keep drawing
    /// into the un-zoomed `main` rect, leaving the header and sidebar
    /// columns blank and the grid clipped to a fraction of what the child
    /// had just re-laid itself out for.
    #[test]
    fn the_zoomed_draw_target_is_the_whole_frame_not_the_sidebar_inset() {
        let frame = Rect::new(0, 0, 100, 30);
        let sidebar_cols = 24;

        let zoomed_target = effective_main(frame, sidebar_cols, true);
        assert_eq!(zoomed_target, frame, "zoom draws into the whole frame");

        let plain_target = effective_main(frame, sidebar_cols, false);
        assert_ne!(
            plain_target, zoomed_target,
            "and that is genuinely different from the un-zoomed rect"
        );
        assert_eq!(plain_target, ui::layout(frame, sidebar_cols).main);
    }

    /// Issue #30, item 1: `codex::register_turn_signal` returns an empty
    /// `env` (codex has no turn-signal mechanism at all --
    /// `capabilities().turn_signal == false`), which used to mean a codex
    /// worker pane's `ZIRV_CTX_SESSION` went entirely unset. Any `zirv ctx
    /// send` such a pane ran then recorded `identity_or_unknown`'s
    /// `"unknown"` as its sender and had no address of its own to be
    /// `--to-session`-replied to. A worker pane's own session identity must
    /// not depend on whether its adapter happens to support turn signals.
    #[test]
    fn build_turn_env_sets_session_identity_even_for_an_adapter_with_no_turn_signal_env() {
        let tmp = crate::commands::ctx::testenv::repo();
        let home = tmp.path().join("home");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);
        let state = StateDir::from_root(tmp.path().join("state"));
        let repo = tmp.path().to_path_buf();
        let cfg = CtxConfig::default();
        let session_id = "11112222-3333-4444-8555-666677778888";

        let (env, err) = build_turn_env(
            &cfg,
            &state,
            &repo,
            "codex",
            session_id,
            adapters::LaunchMode::Headless,
        );

        assert!(err.is_none(), "codex resolves fine, so no error: {err:?}");
        assert!(
            env.iter()
                .any(|(k, v)| k == adapters::SESSION_ENV && v == session_id),
            "a worker pane must always carry its own session identity, \
             regardless of turn-signal support: {env:?}"
        );
    }

    /// The same guarantee for an adapter that *does* have a turn-signal
    /// mechanism (claude): its own `register_turn_signal` already sets
    /// `SESSION_ENV`, and this must not end up duplicated or dropped.
    #[test]
    fn build_turn_env_carries_exactly_one_session_identity_for_a_turn_signal_capable_adapter() {
        let tmp = crate::commands::ctx::testenv::repo();
        let home = tmp.path().join("home");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);
        let state = StateDir::from_root(tmp.path().join("state"));
        let repo = tmp.path().to_path_buf();
        let cfg = CtxConfig::default();
        let session_id = "22223333-4444-5555-8666-777788889999";

        let (env, err) = build_turn_env(
            &cfg,
            &state,
            &repo,
            "claude",
            session_id,
            adapters::LaunchMode::Headless,
        );

        assert!(err.is_none());
        let matches: Vec<_> = env
            .iter()
            .filter(|(k, v)| k == adapters::SESSION_ENV && v == session_id)
            .collect();
        assert_eq!(
            matches.len(),
            1,
            "exactly one SESSION_ENV entry, not duplicated: {env:?}"
        );
    }

    /// Issue #160 finding 2 (2026-08-28): `build_turn_env` now pushes the
    /// durable interactive-launch pin itself, from the mandatory `mode`
    /// parameter, rather than leaving it to each of its three call sites --
    /// this pins that push at its actual source, independent of any one
    /// call site remembering to add it separately.
    #[test]
    fn build_turn_env_pushes_the_interactive_launch_mode_pin_only_when_asked() {
        let tmp = crate::commands::ctx::testenv::repo();
        let home = tmp.path().join("home");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);
        let state = StateDir::from_root(tmp.path().join("state"));
        let repo = tmp.path().to_path_buf();
        let cfg = CtxConfig::default();
        let session_id = "33334444-5555-6666-8777-888899990000";

        let (interactive_env, _) = build_turn_env(
            &cfg,
            &state,
            &repo,
            "claude",
            session_id,
            adapters::LaunchMode::Interactive,
        );
        assert!(
            interactive_env.contains(&(
                adapters::LAUNCH_MODE_ENV.to_string(),
                adapters::LAUNCH_MODE_INTERACTIVE_VALUE.to_string()
            )),
            "LaunchMode::Interactive must push the pin: {interactive_env:?}"
        );

        let (headless_env, _) = build_turn_env(
            &cfg,
            &state,
            &repo,
            "claude",
            session_id,
            adapters::LaunchMode::Headless,
        );
        assert!(
            !headless_env
                .iter()
                .any(|(k, _)| k == adapters::LAUNCH_MODE_ENV),
            "LaunchMode::Headless must never push the pin: {headless_env:?}"
        );
    }

    /// N1: teardown used to call a bare `take_hook()`, which installs **std's
    /// default** rather than whatever was there before -- silently discarding
    /// any hook the process had already chained in.
    #[test]
    fn the_panic_hook_is_restored_to_whatever_was_installed_before() {
        use std::sync::atomic::{AtomicBool, Ordering};
        static OUTER_HOOK_RAN: AtomicBool = AtomicBool::new(false);

        let original = std::panic::take_hook();
        std::panic::set_hook(Box::new(|_| OUTER_HOOK_RAN.store(true, Ordering::SeqCst)));

        let previous = install_panic_hook();
        restore_panic_hook(&previous);

        OUTER_HOOK_RAN.store(false, Ordering::SeqCst);
        let _ = std::panic::catch_unwind(|| panic!("deliberate: exercising the restored hook"));
        let ran = OUTER_HOOK_RAN.load(Ordering::SeqCst);

        std::panic::set_hook(original);
        assert!(
            ran,
            "the hook installed before the dashboard must be the one back in place afterwards"
        );
    }

    /// Dash refresh PR1: the grid rect (`ui::layout`'s own `main`) and the
    /// pty size `apply_terminal_resize` actually applies must agree in
    /// every one of the three regimes `effective_sidebar_cols` decides
    /// between -- hidden below the narrow-terminal floor, forced back on
    /// below it, and shown outright at/above it. All three go through the
    /// SAME `sidebar_cols` value here, exactly as the real render loop's
    /// own `effective_sidebar_cols` call feeds both `ui::layout` and
    /// `apply_terminal_resize` from one place.
    #[test]
    fn pane_geometry_matches_layouts_main_rect_hidden_forced_and_shown() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().join("state"));
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(&repo).expect("mkdir repo");
        let cfg = CtxConfig::default();
        assert_eq!(
            cfg.dash.sidebar_cols, 28,
            "sanity: the default this test exercises"
        );

        for (width, height, forced_visible, expect_hidden) in [
            (80u16, 24u16, false, true),
            (80u16, 24u16, true, false),
            (120u16, 40u16, false, false),
        ] {
            let spec = PaneSpec {
                agent_name: "test-agent".to_string(),
                argv: super::pane::tests::long_lived_argv(),
                role: prompt::PromptRole::Worker,
                verb: sessions::Verb::Dash,
                session_id: "bbbbbbbb-2222-4333-8444-555555555555".to_string(),
                title: "wrk resize".to_string(),
            };
            let mut panes = vec![
                Pane::spawn(
                    spec,
                    &state,
                    &repo,
                    &repo,
                    (80, 24),
                    &[],
                    true,
                    pane::DEFAULT_IDLE_QUIET,
                )
                .expect("spawn"),
            ];
            let mut term_cols = 80u16;
            let mut term_rows = 24u16;
            let mut full = Rect::new(0, 0, 80, 24);
            let mut errors = ErrorLog::default();

            let sidebar_cols = effective_sidebar_cols(&cfg, width, forced_visible);
            assert_eq!(
                sidebar_cols == 0,
                expect_hidden,
                "sanity: hidden decision at {width}x{height} forced={forced_visible}"
            );

            apply_terminal_resize(
                width,
                height,
                sidebar_cols,
                false,
                &mut term_cols,
                &mut term_rows,
                &mut full,
                &mut panes,
                &mut errors,
                &mut None,
            );

            let expected_main = ui::layout(Rect::new(0, 0, width, height), sidebar_cols).main;
            assert_eq!(
                panes[0].screen().size(),
                (expected_main.height.max(1), expected_main.width.max(1)),
                "pty size must match ui::layout's own main rect at {width}x{height} \
                 forced={forced_visible} (sidebar_cols={sidebar_cols})"
            );

            for pane in panes.iter_mut() {
                let _ = pane.finish_shutdown();
            }
        }
    }

    /// CROSS-CUTTING: the stale-token-dir sweep removes a token dir whose
    /// `owner.pid` names a dead process, and keeps one naming a live process.
    #[test]
    fn sweep_stale_token_dirs_removes_only_dead_owners() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().join("state"));
        std::fs::create_dir_all(state.dash()).expect("mkdir dash");

        let dead = state.dash().join("aaaa1111-deadtoken");
        let live = state.dash().join("bbbb2222-livetoken");
        std::fs::create_dir_all(&dead).expect("mkdir dead");
        std::fs::create_dir_all(&live).expect("mkdir live");
        std::fs::write(dead.join("owner.pid"), dead_pid().to_string()).expect("write dead pid");
        std::fs::write(live.join("owner.pid"), std::process::id().to_string())
            .expect("write live pid");

        sweep_stale_token_dirs(&state);

        assert!(!dead.exists(), "the dead-owner token dir is swept");
        assert!(live.exists(), "the live-owner token dir is kept");
    }
}
