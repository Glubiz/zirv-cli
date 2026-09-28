//! Terminal lifecycle (panic hook, resize) and per-turn env/token setup.
use super::*;

/// Probe before the input loop: the query shares stdin with event reads.
/// Request only escape disambiguation for Shift+Enter; release events would
/// flood input. Pop only after success, including on panic; failures are silent.
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

/// Restore raw mode and reset stdout on every exit; leaving the alternate
/// screen does not show the cursor, and `panic = "abort"` skips `Drop`.
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

/// Reset raw mode, cursor, stdout's alternate screen and any keyboard stack
/// entry before the previous hook prints; return that hook for restoration.
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

/// Restore the displaced hook; `take_hook()` alone installs the default hook.
pub(super) fn restore_panic_hook(previous: &Arc<PanicHook>) {
    let _ = std::panic::take_hook();
    let previous = Arc::clone(previous);
    std::panic::set_hook(Box::new(move |info| previous(info)));
}

/// Resolve sidebar width once for layout and PTY sizing: hide below 100
/// columns unless visibility was forced.
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

/// Store terminal size and resize pane PTYs and parsers. The render loop also
/// checks size because crossterm can coalesce or miss resize events.
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
    // Check selection against each pane's current size before resizing it.
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

/// Build turn environment with session identity and a mandatory launch mode,
/// so every spawn path carries the same interactive pin policy (#160).
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
            // Supply session identity even when an adapter has no turn signal;
            // avoid duplicating an identity already provided by the adapter (#30).
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

// Panes send spawn requests through their inherited channel; the dashboard
// owns worker launch and preserves prompt order: memory, mail, mail layer.

/// A 16-hex-character capability token for this dashboard's own
/// spawn-request directory (`spawnreq::request_dir_for`). Freshly minted per
/// launch: unpredictable enough that a process never told this directory's
/// path cannot guess it, so only a pane that actually inherited
/// `DASH_REQUESTS_ENV` from this dashboard can reach its spawn channel.
pub(super) fn spawn_token() -> String {
    uuid::Uuid::new_v4().simple().to_string()[..16].to_string()
}

/// Create each pane's private request directory before spawn so the directory
/// identifies its requester and `live_join_target` can find it. Failure falls
/// back to the shared channel.
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
