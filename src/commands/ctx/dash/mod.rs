//! `zirv chat`'s session multiplexer: a dashboard process owning N
//! interactive ConPTY harness sessions, each rendered through its own
//! embedded `vt100` screen model.
//!
//! This module carries the event loop itself (`run_dashboard`) plus the pure
//! input filter (`filter_key`/`encode_key`) that decides, for every
//! keystroke, whether it goes straight to the active pane's child or gets
//! swallowed as a dashboard command behind the `Ctrl+A` prefix.
//! `chat.rs::run_with` calls `run_dashboard` once `chrome::dash_eligible`
//! says the terminal can carry it; ineligible terminals (`--simple`, non-terminal
//! stdio, too small, or dashboard disabled) use `wrap::run_with` passthrough.

pub mod actions;
pub mod hit;
pub mod link;
pub mod native_pane;
pub mod native_ux;
pub mod notify;
pub mod pane;
pub mod roster;
pub mod spawnreq;
pub mod ui;

mod approvals_view;
mod delivery;
mod facts_cache;
mod input;
mod overlays;
mod pane_rollover;
mod reap;
mod selection_clipboard;
pub mod settings_view;
mod sidebar_facts;
mod spawn_policy;
mod subagent_focus;
mod terminal_turn;
mod tree_view;

use std::collections::{BTreeSet, HashMap, HashSet, VecDeque};
use std::io::{self, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant};

use crossterm::event::{
    self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, KeyboardEnhancementFlags,
    MouseButton, MouseEventKind, PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
    supports_keyboard_enhancement,
};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Position, Rect};

use super::CtxResult;
use super::adapters;
use super::adapters::AgentAdapter;
use super::config::{CtxConfig, EnvLookup, validate_model_str};
use super::envelope;
use super::event::{SessionId, SessionRef};
use super::policy;
use super::state::StateDir;
use super::term;
#[cfg(test)]
use super::transcript_source;
use super::window;
use super::{
    agent, allocator, attention, compile, config, exec, group, jev, log, obfuscate_store, pace,
    permit, price, reservation, result_schema, rollover, screen, session_spend, state,
};
use super::{fallback, handoff, handover, mail, memory, prompt, runtime, score, seat, sessions};
use crate::commands::workflow;
use crate::style;
use actions::{MENU_NO_CWD, MENU_NO_REQUEST};
use hit::{HintId, Hit};

pub(crate) use pane::{Pane, PaneBudgetNotice, PaneSpec, PaneState, ScrollOutcome};

use delivery::*;
use facts_cache::*;
use input::*;
use overlays::*;
use pane_rollover::*;
use reap::*;
use selection_clipboard::*;
use sidebar_facts::*;
use spawn_policy::*;
use terminal_turn::*;

// Re-exports keep every pre-split `dash::<name>` path valid.
pub(crate) use delivery::{Injector, advise_one_pane, is_delivery_eligible, sweep_one_pane};
pub use input::{DashAction, InputVerdict, encode_key, filter_key};
pub(crate) use pane_rollover::ErrorLog;
pub(crate) use spawn_policy::{
    CandidateStatus, DashCandidate, argv_unsafe_prompt, discover_live_dash_dirs, flatten_command,
    resolved_spawn_cwd, select_live_dash_dir, workdir_roots,
};
pub(crate) use terminal_turn::build_turn_env;

/// Ctrl+A is the dashboard prefix key.
pub const PREFIX: (KeyModifiers, KeyCode) = (KeyModifiers::CONTROL, KeyCode::Char('a'));

#[allow(clippy::too_many_arguments)]
pub fn run_dashboard(
    cfg: &CtxConfig,
    repo: &Path,
    env: EnvLookup<'_>,
    state: &StateDir,
    first: PaneSpec,
    first_native: Option<native_pane::NativeDashboardSpec>,
    force_pace: bool,
    first_workflow_id: Option<String>,
) -> CtxResult<i32> {
    run_dashboard_inner(
        cfg,
        repo,
        env,
        state,
        first,
        first_native,
        force_pace,
        None,
        first_workflow_id,
    )
}

/// Takes over an already-live successor pane after another terminal host has
/// restored its own modes. The pane is not respawned: its native session,
/// stable seat address and generation are the ones the successor seam opened.
pub(crate) fn run_dashboard_with_first_pane(
    cfg: &CtxConfig,
    repo: &Path,
    env: EnvLookup<'_>,
    state: &StateDir,
    first_pane: Pane,
    force_pace: bool,
) -> CtxResult<i32> {
    let first = PaneSpec {
        agent_name: first_pane.agent().to_string(),
        argv: Vec::new(),
        role: first_pane.role(),
        verb: first_pane.verb(),
        // Dashboard-level request and facts paths are keyed by this field's
        // short form. A rollover successor's logical conversation is fresh,
        // but its stable seat address must remain the dashboard address.
        session_id: first_pane.short().to_string(),
        title: first_pane.title().to_string(),
    };
    run_dashboard_inner(
        cfg,
        repo,
        env,
        state,
        first,
        None,
        force_pace,
        Some(first_pane),
        // A rollover successor takeover, never a fresh workflow start.
        None,
    )
}

#[allow(clippy::too_many_arguments)]
fn run_dashboard_inner(
    cfg: &CtxConfig,
    repo: &Path,
    env: EnvLookup<'_>,
    state: &StateDir,
    first: PaneSpec,
    first_native: Option<native_pane::NativeDashboardSpec>,
    force_pace: bool,
    first_prebuilt: Option<Pane>,
    first_workflow_id: Option<String>,
) -> CtxResult<i32> {
    let mut errors = ErrorLog::default();
    // Start uptime before setup so it includes startup work (#354).
    let launched_at = Instant::now();

    // Reclaim worktrees owned by dead sessions at startup without blocking dashboard launch (#319).
    let _ = super::worktree::gc(state, repo, &sessions::is_alive, cfg.worktree.idle_ttl_secs);

    // Keep terminal geometry current for zoom and pane resizing.
    let (mut term_cols, mut term_rows) = crossterm::terminal::size().unwrap_or((80, 24));
    // Dash refresh PR1: below 100 total columns the session column hides
    // (`ui::sidebar_hidden`) -- `^A b` forces it back regardless of width.
    // `sidebar_cols` is THE one effective-width value every geometry read
    // below (pty resize, `ui::layout`, `effective_main`, the inspector's own
    // display line) goes through; kept current at every point the real
    // terminal width can change (the `Event::Resize` arm, and the top-of-
    // loop reconciliation against a resize crossterm coalesced or dropped),
    // never read stale from a keystroke that only toggled the config value.
    let mut sidebar_forced_visible = false;
    let mut sidebar_cols = effective_sidebar_cols(cfg, term_cols, sidebar_forced_visible);
    let mut full = Rect::new(0, 0, term_cols, term_rows);
    let main = effective_main(full, sidebar_cols, false);

    let agent_name = first.agent_name.clone();
    // The header's own standing disclosure of a configured model
    // (`harness_label`, built once): `chat.model` is repo-settable on the
    // strength of the choice being visible, and the dashboard's header is the
    // surface that stays on screen for the whole session. `chat.rs` announces
    // it once on the events channel as well -- that is the repo-unsilenceable
    // half; this is the persistent half.
    let harness_label = match &cfg.chat.model {
        Some(model) => format!("{agent_name} ({model})"),
        None => agent_name.clone(),
    };
    let session_id = first.session_id.clone();
    // Treat the first dashboard pane as human-attended for launch policy (#147).
    let prebuilt_native = first_prebuilt.as_ref().is_some_and(Pane::is_native);
    let (mut turn_env, turn_env_err) = if first_native.is_some() || prebuilt_native {
        (Vec::new(), None)
    } else {
        build_turn_env(
            cfg,
            state,
            repo,
            &agent_name,
            &session_id,
            super::adapters::LaunchMode::Interactive,
        )
    };
    if let Some(e) = turn_env_err {
        push_error(&mut errors, e);
    }
    // The seat this pane sits in, for the `zirv ctx hook pretool` guard
    // running inside it. This `turn_env` belongs to the first pane and only
    // the first pane -- `fulfill_spawn_request` and the restore path each
    // build their own from scratch -- so a worker pane never picks it up,
    // and `Pane::spawn`'s own `scrub_supervision_env` clears any copy the
    // dashboard process itself might have inherited. `first.argv` is the
    // exact launch argv `build_launch`/`extra_with_model` built (config
    // model folded in, then the operator's own trailing flags appended after
    // it), so a passthrough `--model`/`--model=` in it is preferred over
    // `cfg.chat.model` the same way `wrap.rs`'s own orchestrator arm prefers
    // its own `rest`; see `adapters::seat_model_env`.
    turn_env.extend(super::adapters::seat_model_env(
        first.role,
        &first.argv,
        cfg.chat.model.as_deref(),
    ));
    // Issues #328/#334: which seat role this pane runs as, for the same
    // guard -- unlike `seat_model_env`, unconditional for every role.
    turn_env.extend(super::adapters::seat_role_env(first.role));
    // Skip hook intake for a proxy-decided launch already handled by the chat branch (#753).
    if env(super::adapters::PROXY_DECIDED_ENV).as_deref() == Some("1") {
        turn_env.push((
            super::adapters::PROXY_DECIDED_ENV.to_string(),
            "1".to_string(),
        ));
    }
    // Fence automatic rollover by seat generation so a superseded session cannot keep coordinating (#358).
    if first.role == prompt::PromptRole::Orchestrator {
        let generation = super::seat::load(state, &sessions::short_id(&session_id))
            .map(|seat| seat.generation)
            .unwrap_or(1);
        turn_env.push((
            super::seat::GENERATION_ENV.to_string(),
            generation.to_string(),
        ));
    }

    // Create the request channel before panes spawn; the dashboard short ID is already known.
    let dashboard_short = sessions::short_id(&session_id);
    let requests_token = spawn_token();
    let requests_dir = spawnreq::request_dir_for(state, &dashboard_short, &requests_token);
    if let Err(e) = super::state::create_private_dir_all(&requests_dir) {
        push_error(
            &mut errors,
            format!("dashboard: could not create the spawn-request directory: {e}"),
        );
    }
    // Give the orchestrator its own request channel so workers cannot claim its identity.
    let first_pane_channel = mint_pane_channel(&requests_dir, &mut errors);
    turn_env.push((
        spawnreq::DASH_REQUESTS_ENV.to_string(),
        first_pane_channel.display().to_string(),
    ));
    // Record the owner PID so leaked token directories cannot masquerade as live dashboards.
    if let Err(e) = super::state::write_private(
        &spawnreq::owner_pid_path(&requests_dir),
        &std::process::id().to_string(),
    ) {
        push_error(
            &mut errors,
            format!(
                "dashboard: could not write {}, so no delegated agent can join this dashboard \
                 (it will still run headless instead): {e}",
                spawnreq::owner_pid_path(&requests_dir).display()
            ),
        );
    }
    // Remove dead sibling token directories while keeping this launch's own directory.
    sweep_stale_token_dirs(state);

    // Apply the interactive pacing gate before spawning the first orchestrator pane.
    if first_prebuilt.is_none() {
        // The `SEAT_MODEL_ENV` `seat_model_env` just pushed onto `turn_env`
        // above -- this pane's own resolved model, for the same reason
        // `wrap::run_with`'s matching gate call resolves one.
        let first_pane_model = turn_env
            .iter()
            .find(|(key, _)| key == super::adapters::SEAT_MODEL_ENV)
            .map(|(_, value)| value.as_str());
        let provider =
            super::adapters::provider_for_agent_and_model(Some(&agent_name), first_pane_model);
        // Read the pacing response before the dashboard event loop begins reading stdin.
        let gate = super::pace::interactive_gate(state, cfg, provider, true);
        super::wrap::apply_interactive_gate(gate, force_pace)?;
    }

    // Refuse a competing dashboard PTY when the persistent runtime already owns this seat (#489).
    if first_prebuilt.is_none()
        && let Some(mut link) = link::RuntimeLink::connect(state, cfg.session.persistent)
    {
        let slug = super::state::repo_slug(repo);
        match link.seat_for(&slug, &agent_name) {
            Ok(Some(seat)) => {
                remove_request_dir(&requests_dir);
                return Err(format!("{} ({})", link::RUNTIME_OWNS_IT, seat.short).into());
            }
            Ok(None) => {}
            // A runtime that cannot be read is not a reason to refuse to
            // start: the dashboard owns its own terminals in that case,
            // which is the pre-runtime behaviour and a working one.
            Err(error) => push_error(&mut errors, format!("runtime link: {error}")),
        }
    }

    let size = (main.width.max(1), main.height.max(1));
    // Clean up the request directory if startup fails after creating it.
    let first_spawn = match first_prebuilt {
        Some(pane) => Ok(pane),
        None => match first_native {
            Some(spec) => Pane::spawn_native(
                cfg,
                state,
                env,
                repo,
                first.verb,
                first.title.clone(),
                size,
                spec,
            ),
            None => Pane::spawn(
                first,
                state,
                repo,
                repo,
                size,
                &turn_env,
                turn_signal_capable_for(cfg, &agent_name),
                Duration::from_millis(cfg.dash.idle_quiet_ms),
            ),
        },
    };
    let first_pane = match first_spawn {
        Ok(mut pane) => {
            pane.set_intake_dir(first_pane_channel);
            pane
        }
        Err(e) => {
            remove_request_dir(&requests_dir);
            return Err(e);
        }
    };
    // Dash refresh PR1: binds the proxy's already-started workflow onto this
    // fresh orchestrator pane's own just-registered record, so the dashboard
    // resolves ITS workflow step from this pane's session rather than the
    // one repo-wide `engine::load_active` pointer. Best-effort, and only
    // ever `Some` for a freshly spawned (non-native, non-prebuilt) pane --
    // `chat.rs`'s only caller that ever has a `started_workflow_id` is the
    // wrapped-launch proxy intake, which always takes this exact branch.
    if let Some(workflow_id) = &first_workflow_id {
        sessions::bind_workflow_id(state, first_pane.short(), workflow_id);
    }
    let mut panes = vec![first_pane];
    // Task 9: one FIFO nudge queue per pane, kept the same length as `panes`.
    // Nothing in this task's scope ever grows `panes` after this point (a
    // future spawn seam -- Tasks 10/11 -- must push a matching
    // `VecDeque::new()` here too whenever it pushes a new pane).
    let mut nudge_queues: Vec<VecDeque<String>> = vec![VecDeque::new(); panes.len()];
    // Approvals inbox (#840): off unless the operator turned it on; then this dashboard serves its hooks' held requests.
    let mut approvals_hub = if cfg.approvals.inbox {
        match super::approvals::Hub::bind(state) {
            Ok(hub) => Some(hub),
            Err(e) => {
                push_error(&mut errors, format!("approvals inbox: {e}"));
                None
            }
        }
    } else {
        None
    };

    let previous_panic_hook = install_panic_hook();
    // F4(c): an external kill (`taskkill`, a Ctrl-Break, a closed window)
    // reaches neither the panic hook nor any exit arm below. `RawGuard::
    // enter` arms this for `wrap`; the dashboard drives raw mode through
    // crossterm instead, so it has to stash the pre-raw console modes and
    // install the same handler itself. Both are write-once/idempotent, and
    // the return values are advisory only -- a process with no console of
    // its own stashes nothing and still starts.
    let _ = term::stash_current_console();
    let _ = term::install_console_restore_handler();
    // Raise dashboard thread priority while it owns input and frame rendering (#330).
    super::priority::raise_current_thread();
    if let Err(e) = enable_raw_mode() {
        abort_setup(&mut panes, cfg, &requests_dir);
        restore_panic_hook(&previous_panic_hook);
        return Err(format!("dashboard: enable_raw_mode failed: {e}").into());
    }
    if let Err(e) = execute!(io::stdout(), EnterAlternateScreen) {
        // Nothing has pushed the keyboard-enhancement flags yet -- that
        // happens further down, once the alternate screen and mouse
        // reporting are both up -- so teardown owes no matching pop here.
        teardown_terminal(false);
        abort_setup(&mut panes, cfg, &requests_dir);
        restore_panic_hook(&previous_panic_hook);
        return Err(format!("dashboard: EnterAlternateScreen failed: {e}").into());
    }
    // From here on the emergency handler owes the terminal the alternate
    // screen back, not just the console modes. Cleared by `teardown_terminal`
    // on every exit arm below.
    term::set_dash_active(true);

    // Enable drag and wheel mouse reporting without motion-only flood.
    if cfg.dash.mouse {
        let mut stdout = io::stdout();
        if let Err(e) = stdout
            .write_all(term::dash_mouse_on_bytes())
            .and_then(|()| stdout.flush())
        {
            push_error(
                &mut errors,
                format!("dashboard: mouse reporting could not be enabled: {e}"),
            );
        }
    }

    // Negotiate keyboard enhancement before the input loop reads stdin.
    let keyboard_enhancement_pushed = push_keyboard_enhancement();

    // Task 2: the opt-in input diagnostic. `None`, and entirely inert, unless
    // `ZIRV_CTX_DASH_KEYLOG` names a path.
    let mut keylog = KeyLog::from_env();
    if let Some(log) = keylog.as_mut() {
        log.startup(
            cfg,
            (term_cols, term_rows),
            io::stdin().is_terminal(),
            io::stdout().is_terminal(),
        );
    }

    let backend = CrosstermBackend::new(io::stdout());
    let mut terminal = match Terminal::new(backend) {
        Ok(t) => t,
        Err(e) => {
            teardown_terminal(keyboard_enhancement_pushed);
            abort_setup(&mut panes, cfg, &requests_dir);
            restore_panic_hook(&previous_panic_hook);
            return Err(format!("dashboard: could not attach to the terminal: {e}").into());
        }
    };

    // Offer a fresh consumed roster once at startup.
    let repo_slug = super::state::repo_slug(repo);
    let taken_candidates: Vec<roster::RosterPane> = roster::take_roster(
        state,
        &repo_slug,
        super::state::now_secs(),
        cfg.dash.roster_max_age_secs,
    )
    .map(restorable_candidates)
    .unwrap_or_default();
    // Filter live roster sessions before offering restoration, avoiding duplicate agents.
    let (restore_candidates, still_live) = roster::partition_live(taken_candidates, &|short| {
        super::sessions::short_is_live(state, short)
    });

    // Two indices, not one (F7): `selected` walks the combined sidebar
    // (panes plus view-only registry rows) and is what a nudge is aimed at;
    // `focused` is the pane on screen and under the keyboard, and only ever
    // names a pane. See `apply_navigation`.
    let mut selected: usize = 0;
    let mut focused: usize = 0;
    // Keep sidebar viewport scroll independent of the selected row (#354).
    let mut sidebar_offset = 0usize;
    let mut chrome_selection: Option<Hit> = None;
    let mut collapsed_groups = HashSet::new();
    // Retain ended rows and acknowledge their unread output only after a visible render (#354).
    let mut retained_ended: VecDeque<EndedRow> = VecDeque::new();
    let mut done_unread_ack = DoneUnreadAck::default();
    // Deduplicate attention notices by session episode on the facts cadence (#354).
    let mut attention_notices = notify::NoticeReducer::new();
    let mut frame_snapshot = hit::FrameSnapshot::default();
    // Keep overlay identity synchronized with the frame snapshot so stale mouse routes can be rejected (#354).
    let mut frame_snapshot_overlay_ident = overlay_identity(&ui::Overlay::None);
    let mut reveal_sidebar = true;
    // Retain each pane's original spawn request for restore and retry (#354).
    let mut kept_requests: HashMap<String, (spawnreq::SpawnRequest, Option<String>)> =
        HashMap::new();
    // Keep the last dialog click with a clock for double-click activation (#354).
    let mut last_overlay_click: Option<(usize, Instant)> = None;
    // Key double-click state by dialog identity and target so a reused row index cannot activate another action.
    let mut last_overlay_ident = overlay_identity(&ui::Overlay::None);
    let mut zoomed = false;
    let mut prefix_armed = false;
    // Keep mouse-capture state for the session after its initial config choice (#697).
    let mouse_capture = cfg.dash.mouse;
    // Tmux-style in-dashboard click-drag text selection (`Selection`'s own
    // doc comment). `None` whenever nothing is selected or highlighted;
    // `Some` both while a drag is in progress and, after release, for
    // whatever stays highlighted until the next `Down` clears it.
    let mut selection: Option<Selection> = None;
    // Defer a pane press until click or drag is known, preserving child clicks (#697).
    let mut pending_press: Option<PendingPress> = None;
    // Receive clipboard fallback results without blocking the render loop (#697).
    let (clipboard_tx, clipboard_rx) = mpsc::channel::<ClipboardOutcome>();
    // Hold native Ctrl+C confirmation in dashboard state across pane changes (#490).
    let mut native_ctrl_c: Option<Instant> = None;
    // Refresh hot-poll time only on operator input, not streaming pane output.
    let mut last_activity = Instant::now();
    // Show the first-run tip until a prefixed key or Esc dismisses it (#354).
    let mut first_run_tip = !first_run_tip_seen(state);
    let mut overlay = if restore_candidates.is_empty() {
        ui::Overlay::None
    } else {
        ui::Overlay::Restore(build_restore_view(&restore_candidates))
    };
    let mut facts_cache = FactsCache::new(Instant::now());
    // The agent-tree view (#833): hidden by default, and then it computes nothing.
    let mut tree_view = tree_view::TreeView::default();
    // When the tree last drew: while it shows it redraws at its own cadence, not on every tick.
    let mut last_tree_draw: Option<Instant> = None;
    // Whether any-motion tracking is on: only while the orchestrator FLOW shows (never on Windows).
    let mut hover_on = false;
    // Refresh machine-wide facts off the UI thread because registry and provider scans can block.
    let facts_refresher = FactsRefresher::spawn(
        state,
        FactsOwner {
            repo,
            agent_name: &agent_name,
            session_short: &dashboard_short,
        },
        cfg,
    );
    // Keep transient confirmations separate from sticky errors so successful actions do not pin a warning.
    let mut notices: Vec<Notice> = Vec::new();
    // Advance spinner frames with ticks so a throttled draw does not freeze animation.
    #[allow(unused_assignments)]
    let mut render_tick: usize = 0;
    // Dash refresh PR2: `dash.motion`, converted once at launch (see
    // `dash_motion_of`) -- presentation only, so unlike `sidebar_cols` this
    // is read straight off `cfg` rather than needing a live-reload seam.
    let motion = dash_motion_of(cfg);
    // Dash refresh PR2: the one wall clock every animation in this loop
    // reads from -- spinners, shimmer and pending-rollover breathing all
    // want a continuously advancing phase, never one tied to a disk-backed
    // "since" timestamp (those only have whole-second resolution). Flashes
    // and the toast track their OWN start `Instant` instead (below), since
    // those need to measure elapsed time from a specific edge, not just
    // oscillate forever.
    let dash_start = Instant::now();
    // Dash refresh PR2: the footer rot track's own eased fill value --
    // lags the real score by up to ~300ms so the gauge never jumps once a
    // second (`ease_toward`); the numeric label stays instant. Seeded from
    // whatever the very first tick's own score turns out to be, the same
    // "no fabricated history" rule `FactsCache::state_since` follows.
    let mut eased_rot_score: Option<f64> = None;
    let mut last_ease_tick = dash_start;
    // Coordinator follow-up: LIMITS bars and JEV site bars ease the same
    // way the rot track does, off the same per-frame `ease_dt_ms` step --
    // keyed by a caller-chosen string (`"{harness}:{window}"` for LIMITS,
    // `"jev:{site}"` for JEV) so unrelated bars never share state, and
    // pruned each frame to whatever bars are actually still drawn (a stale
    // entry for a harness/site that stopped showing must not linger and
    // then "ease" a completely different bar that later reuses the key).
    let mut eased_bars: HashMap<String, f64> = HashMap::new();
    // Dash refresh PR2: per-pane flash-start times (new mail, or a worker
    // finishing) -- `Instant`, not the disk-backed facts cache, since a
    // flash (900ms) needs sub-second precision the facts refresh's own
    // whole-second clock cannot give it. Cleared for any short no longer on
    // screen by the same throttled tick that populates it.
    let mut flash_started: HashMap<String, Instant> = HashMap::new();
    // Dash refresh PR2: the single most recent toast, dashboard-wide (a
    // rollover committing, or a worker finishing) -- "at most one visible;
    // newest wins" (the spec's own words), so one slot, not one per pane.
    let mut toast: Option<(String, Instant)> = None;
    // Dash refresh PR2: the orchestrator seat's own rollover-runtime
    // settlement, as last seen on the facts-refresh cadence -- compared
    // against the freshly-read one each throttled tick to edge-trigger the
    // "rolled over" toast (a `Committed` settlement that was not there, or
    // was a different generation, last time).
    let mut last_rollover_settlement: Option<super::rollover::runtime::Settlement> = None;
    // Suppress first-observation mail and rollover notifications; only transitions after startup count.
    let mut seen_first_facts_refresh = false;
    // Dash refresh PR2: the JEV sidebar section's own (much coarser) 10s
    // refresh cadence -- `jev::usage_rollup` is a plain read of two small
    // append-only logs, cheap enough on its own, but there is no reason to
    // pay it every second when the section only ever needs a 24h rollup.
    // Seeded a full interval in the past, same reasoning as the mail sweep.
    let mut last_jev_refresh = Instant::now()
        .checked_sub(JEV_THROTTLE)
        .unwrap_or_else(Instant::now);
    // Dash refresh PR2: when the JEV section's own `last` line last changed
    // which call it names -- the flash-start clock for that line.
    let mut jev_last_flash_started: Option<Instant> = None;
    let mut jev_last_seen: Option<(String, u64)> = None;
    // Keep all session IDs hosted this run so reaped workers remain in session-scoped totals.
    let mut jev_dashboard_sessions: BTreeSet<String> = BTreeSet::new();
    // Cache the exact evaluated seat headroom for the focused footer.
    let mut seat_headroom_pct: Option<SeatHeadroom> = None;
    // Dash refresh PR1: the pane header's own `cwd` field is `~`-shortened
    // against the operator's home directory, resolved once here (an env
    // lookup, not a per-frame read) rather than inside the render loop.
    let home_dir_display = crate::utils::home_dir()
        .ok()
        .map(|p| p.display().to_string());
    // Rotate the first unfocused pane sharing the drain budget each tick (#330).
    let mut drain_rotation: usize = 0;
    // Record tick start for input diagnostic duration (#330).
    let mut last_tick_started = Instant::now();
    // Report each live roster candidate withheld from restore.
    for pane in &still_live {
        push_notice(
            &mut notices,
            Instant::now(),
            format!(
                "not restoring {} ({}): that session is still running (kept for next launch)",
                pane.title, pane.short
            ),
        );
    }
    // Throttle disk-backed mail sweep outside the fast input tick.
    let mut last_mail_sweep = Instant::now()
        .checked_sub(FACTS_THROTTLE)
        .unwrap_or_else(Instant::now);
    // The spawn-request intake's own (much tighter) cadence, seeded the same
    // way so the first tick reads immediately -- see `SPAWN_REQUEST_POLL`.
    let mut last_spawn_intake = Instant::now()
        .checked_sub(SPAWN_REQUEST_POLL)
        .unwrap_or_else(Instant::now);
    // Run transcript budget parsing on a throttled disk cadence, not the render tick.
    let mut last_budget_sweep = Instant::now()
        .checked_sub(FACTS_THROTTLE)
        .unwrap_or_else(Instant::now);
    // 2026-09-06: the wall-clock sweep beside it, on the same cadence and
    // seeded the same way.
    let mut last_deadline_sweep = Instant::now()
        .checked_sub(FACTS_THROTTLE)
        .unwrap_or_else(Instant::now);
    // Task B: per-pane dedup for the orchestrator mail advisory
    // (`advise_one_pane`), keyed by a pane's own zirv session id. Lives for
    // the whole dashboard run, not just one tick, so an unchanged inbox is
    // advised once and then left alone until genuinely new mail arrives.
    let mut advised_mail: HashMap<String, mail::AdvisedIds> = HashMap::new();
    // Evaluate automatic orchestrator rollover on its own slower cadence (#358).
    let mut last_rollover_eval = Instant::now();
    // Recheck the auto-rollover config switch on cadence because startup config is held for the session (#780).
    let mut auto_rollover =
        super::rollover::LiveAutoRollover::new(repo, env, cfg.auto_orchestrator_rollover());
    let mut reactive_pending = panes
        .iter()
        .find(|pane| pane.role() == prompt::PromptRole::Orchestrator)
        .and_then(|pane| super::seat::load(state, pane.short()))
        .and_then(|seat| seat.pending)
        .is_some_and(|pending| matches!(pending.cause, super::seat::Cause::Reactive { .. }));
    let mut pending_rollover: Option<(String, u64, Instant)> = None;
    // Quit after an unbroken run of input errors; a successful read resets the count.
    let mut input_errors: usize = 0;
    // Record the all-panes-ended outcome for the closing line after terminal teardown.
    let mut all_panes_ended = false;
    // Fold exit status over every reaped pane's recorded code.
    let mut reaped_codes: Vec<i32> = Vec::new();
    // Accumulate restore candidates skipped by the pane cap for the next roster.
    let mut deferred_restore: Vec<roster::RosterPane> = still_live;
    // Exclude recently reaped shorts from a stale registry snapshot until refresh drops them.
    let mut reaped_recent: HashSet<String> = HashSet::new();
    // Keep the last exited pane's footer facts available after reap (#209).
    let mut last_exited: Option<LastExited> = None;
    // Track the last quiet-heuristic observation per pane to avoid redundant attention writes (#349).
    let mut quiet_lifecycle: HashMap<String, super::attention::Lifecycle> = HashMap::new();

    let exit_code: i32 = 'main: loop {
        // Log only changed loop state each tick.
        let tick_started = Instant::now();
        let previous_tick = tick_started.saturating_duration_since(last_tick_started);
        last_tick_started = tick_started;
        if let Some(log) = keylog.as_mut() {
            log.tick(
                LoopState {
                    prefix_armed,
                    overlay: overlay_name(&overlay),
                    panes: panes.len(),
                    focused,
                    focused_alt: panes.get(focused).is_some_and(Pane::alternate_screen),
                },
                previous_tick,
            );
        }
        // Snapshot selected text before draining output so cancellation compares the affected rows.
        let selection_before = selection.as_ref().and_then(|sel| {
            panes
                .iter()
                .find(|pane| pane.short() == sel.pane_short)
                .and_then(|pane| selection_snapshot(pane.screen(), sel))
        });
        // Share one vt100 byte budget across all panes, focused first with rotating unfocused order (#330).
        let produced_output = drain_shared_budget(
            panes.len(),
            focused,
            drain_rotation,
            pane::DRAIN_BUDGET_BYTES,
            |idx, remaining| {
                let (any, _more, used) = panes[idx].drain_with_budget(remaining);
                (any, used)
            },
        );
        drain_rotation = drain_rotation.wrapping_add(1);
        for idx in produced_output {
            // Cancel selection only when child output changes the selected grid rows.
            if let Some(sel) = selection.as_ref()
                && output_cancels_selection(sel, panes[idx].short())
            {
                let unchanged = selection_before.as_ref().is_some_and(|before| {
                    selection_snapshot(panes[idx].screen(), sel).as_ref() == Some(before)
                });
                if !unchanged {
                    selection = None;
                }
            }
        }
        for pane in panes.iter_mut() {
            pane.on_turn_signal();
        }
        // Read native multi-agent UI facts from durable runtime records (#490).
        {
            let now = super::state::now_secs();
            for pane in panes.iter_mut() {
                if let Some(native) = pane.native_mut()
                    && now.saturating_sub(native.ux().refreshed_at) >= NATIVE_RECORD_REFRESH_SECS
                {
                    native.refresh_records(cfg, env, now);
                }
            }
        }
        // Settle before shutdown or reap can forget the seat, or abort cannot
        // restore a dead successor. Keep settling even if auto-rollover is disabled
        // mid-transaction; the switch only stops new preparations (#440, #780).
        settle_pending_rollover(
            &mut panes,
            cfg,
            repo,
            state,
            &mut pending_rollover,
            &mut errors,
        );
        enforce_pane_token_budgets(
            &mut panes,
            cfg,
            &mut errors,
            &mut last_budget_sweep,
            Instant::now(),
        );
        enforce_pane_deadlines(
            &mut panes,
            cfg,
            &mut errors,
            &mut last_deadline_sweep,
            Instant::now(),
        );
        // Remove exited panes after releasing their registry and socket state.
        confirm_pane_submissions(
            &mut panes,
            state,
            cfg,
            &mut errors,
            &mut notices,
            Instant::now(),
        );
        let reap_confirmations = reap_ended_panes(
            &mut panes,
            &mut nudge_queues,
            cfg,
            state,
            repo,
            &mut focused,
            &mut selected,
            &mut errors,
            &mut reaped_codes,
            &mut reaped_recent,
            &mut last_exited,
            &mut retained_ended,
            &mut kept_requests,
        );
        for line in reap_confirmations {
            push_notice(&mut notices, Instant::now(), line);
        }

        // Approvals inbox (#840): drain requests, then give the strip its rows (none while nothing is pending).
        if let Some(hub) = approvals_hub.as_mut() {
            hub.poll(&|short| panes.iter().any(|pane| pane.short() == short));
            // A pane answer, allow or deny, leaves a tool_result in the transcript; no hook need fire for it.
            hub.drop_answered_released();
            // A released request stays while its prompt is open, latched or still unconfirmed (#864).
            hub.drop_released_unless(&|short| {
                super::attention::load(state, short).attention
                    == super::attention::Attention::Approval
                    || super::attention::prompt_open(state, short)
            });
        }
        let tick_term = crossterm::terminal::size().unwrap_or((term_cols, term_rows));
        // The orchestrator dashboard answers approvals in NEEDS YOU, so it takes no strip.
        let approvals_strip_h = if tree_view.hides_approvals_strip(tick_term) {
            0
        } else {
            approvals_view::current_strip_rows(approvals_hub.as_ref())
        };
        // Use current terminal and zoom geometry for panes spawned during this tick.
        let pane_size = {
            let now_size = tick_term;
            let now_size = (
                now_size.0,
                now_size.1.saturating_sub(
                    approvals_strip_h
                        + tree_view.chat_rows(tick_term, approvals_strip_h)
                        + tree_view.chat_bottom_rows(tick_term, approvals_strip_h),
                ),
            );
            let m = effective_main(
                Rect::new(0, 0, now_size.0, now_size.1),
                sidebar_cols,
                zoomed || tree_view.in_chat(),
            );
            (m.width.max(1), m.height.max(1))
        };
        // Drain pending spawn requests before deciding that an empty dashboard should exit.
        let panes_before_requests = panes.len();
        // ...on its own `SPAWN_REQUEST_POLL` cadence rather than every tick:
        // the directory reads are what cost, and they must not sit between
        // the operator's keystroke and the `event::poll` below.
        let intake_now = Instant::now();
        if spawn_intake_due(last_spawn_intake, intake_now, panes.is_empty()) {
            last_spawn_intake = intake_now;
            handle_spawn_requests(
                &requests_dir,
                &mut panes,
                &mut nudge_queues,
                cfg,
                state,
                repo,
                pane_size,
                &mut errors,
                &mut notices,
                &mut kept_requests,
            );
        }
        // Shift view-only row selection when a new pane is appended.
        selected = insert_fixup(panes_before_requests, panes.len(), selected);

        // Quit when no panes remain to supervise, draw or receive input.
        if should_exit_empty(panes.len(), matches!(overlay, ui::Overlay::Restore(_))) {
            on_quit(
                &panes,
                unoffered_candidates(&overlay, &restore_candidates),
                &deferred_restore,
                &requests_dir,
                state,
                repo,
            );
            shutdown_all(&mut panes, cfg, &mut errors);
            all_panes_ended = true;
            break empty_exit_code(&reaped_codes);
        }

        // H3: the disk-backed sweep and the nudge-marker claim run at most
        // once per `FACTS_THROTTLE`, not on every tick (the tick rate itself
        // is adaptive -- see `input_poll_wait`). The in-memory nudge-queue
        // drain below stays every tick: it is cheap and delivers an
        // operator's queued nudge the moment its pane goes idle.
        let sweep_now = Instant::now();
        if due(last_mail_sweep, sweep_now, FACTS_THROTTLE) {
            last_mail_sweep = sweep_now;
            latch_codex_approval(&mut panes, state);
            mail_sweep(&mut panes, cfg, state, repo, &mut advised_mail, &mut errors);
            claim_pane_nudges(&panes, state, &mut notices, sweep_now);
            // Run the one-shot report reminder on the mail sweep cadence (#115).
            report_back_reminder_sweep(&mut panes, state, &mut errors);
            let slug = super::state::repo_slug(repo);
            for pane in &mut panes {
                report_idle_unread_mail(
                    pane,
                    state,
                    cfg,
                    &slug,
                    &mut errors,
                    &mut notices,
                    super::state::now_secs(),
                    facts_cache.disk.mail_by_session.get(pane.short()).copied(),
                );
            }
        }
        // Dash refresh PR2: the JEV sidebar section, on its own coarser
        // cadence -- never the render path, never `FACTS_THROTTLE` either
        // (the section only ever needs 24h-rollup freshness).
        if due(last_jev_refresh, sweep_now, JEV_THROTTLE) {
            last_jev_refresh = sweep_now;
            let jev_sessions = jev_session_snapshot(&mut jev_dashboard_sessions, &panes);
            facts_cache.disk.jev = jev_section_fact(cfg, state, &jev_sessions);
            let last_now = facts_cache.disk.jev.as_ref().and_then(|fact| match fact {
                ui::JevSectionFact::Active { last, .. } => {
                    last.as_ref().map(|l| (l.site.clone(), l.age_secs))
                }
                ui::JevSectionFact::NoKey { .. } => None,
            });
            // Edge-triggered on the SITE changing, not the age (age moves
            // every refresh regardless): a new call landing is a different
            // site/timestamp pair from what the last refresh saw.
            if last_now.as_ref().map(|(site, _)| site)
                != jev_last_seen.as_ref().map(|(site, _)| site)
                && last_now.is_some()
            {
                jev_last_flash_started = Some(Instant::now());
            }
            jev_last_seen = last_now;
        }
        // Evaluate rollover on a slower cadence but check an open transaction for readiness each tick (#358).
        if pending_rollover.is_none() {
            let eval_due = due_advancing(
                &mut last_rollover_eval,
                sweep_now,
                super::rollover::evaluate_interval(cfg, reactive_pending),
            );
            if eval_due && auto_rollover.is_enabled() {
                let live_cfg = auto_rollover.patched(cfg);
                rollover_sweep(
                    &mut panes,
                    &live_cfg,
                    repo,
                    state,
                    &mut pending_rollover,
                    &mut errors,
                    &mut seat_headroom_pct,
                );
                reactive_pending = panes
                    .iter()
                    .find(|pane| pane.role() == prompt::PromptRole::Orchestrator)
                    .and_then(|pane| super::seat::load(state, pane.short()))
                    .and_then(|seat| seat.pending)
                    .is_some_and(|pending| {
                        matches!(pending.cause, super::seat::Cause::Reactive { .. })
                    });
            }
        }
        deliver_queued_nudges(&mut panes, &mut nudge_queues, &mut errors);
        // Drain deferred submit deadlines every tick so carriage returns do not wait for the mail cadence.
        drain_pending_submits(&mut panes, &mut errors, state, cfg, &mut notices);

        // Build rows before input: nudge routing and selection clamps must use
        // this tick's rows, not a snapshot drawn after the keystroke.
        if let Some(short) = done_unread_ack.take_due() {
            let acked = super::attention::mark_seen_io(state, &short);
            // Update cached attention immediately after acknowledgement so the glyph clears in this frame.
            facts_cache.disk.attention.insert(short, acked);
        }
        // Read clipboard fallback results asynchronously so a missing helper cannot stall drawing (#697).
        while let Ok(outcome) = clipboard_rx.try_recv() {
            match outcome {
                ClipboardOutcome::Confirmed | ClipboardOutcome::Unconfirmed => push_notice(
                    &mut notices,
                    Instant::now(),
                    "copied selection to clipboard".to_string(),
                ),
                ClipboardOutcome::Failed => push_error(
                    &mut errors,
                    "clipboard: copy could not be delivered (OSC 52 and the platform \
                     clipboard command both failed)"
                        .to_string(),
                ),
            }
        }
        let facts_now = Instant::now();
        // Snapshot prior mail counts before refresh to detect only newly increased counts.
        let mail_before_refresh = facts_cache.disk.mail_by_session.clone();
        let facts_refreshed = facts_cache.refresh_if_due(
            cfg,
            state,
            FactsOwner {
                repo,
                agent_name: &agent_name,
                session_short: &dashboard_short,
            },
            &panes,
            facts_now,
            || facts_refresher.take_latest(facts_now),
        );
        tree_view.poll();
        if let Some(why) = tree_view.drive_focus(&mut panes, Instant::now()) {
            push_notice(&mut notices, Instant::now(), why);
        }
        if tree_view.due(facts_now) {
            let (tree_state, tree_repo, tree_cfg) =
                (state.clone(), repo.to_path_buf(), cfg.clone());
            let codex_root =
                std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".codex/sessions"));
            let seat_session = facts_cache
                .disk
                .seat_full
                .as_ref()
                .map(|s| s.session.clone());
            tree_view.start(facts_now, move || {
                tree_view::compute(
                    &tree_state,
                    &tree_repo,
                    codex_root.as_deref(),
                    seat_session.as_deref(),
                    &tree_cfg,
                )
            });
        }
        if facts_refreshed {
            // What the refresher's next cycle should load groups for: this
            // tick's live panes, deduped. One lock per throttled tick, held
            // for a `Vec` swap and nothing else.
            let group_ids: HashSet<&str> = panes.iter().filter_map(Pane::work_group_id).collect();
            facts_refresher.publish_group_ids(group_ids.into_iter().map(str::to_string).collect());
            // Exactly the rows the sidebar can draw, and only on the tick the
            // rest of the facts were re-read: the glyph column must never put
            // a file read on a frame.
            let pane_shorts: Vec<String> = panes.iter().map(|p| p.short().to_string()).collect();
            let shorts = attention_row_shorts(
                &pane_shorts,
                &retained_ended,
                &facts_cache.registry,
                std::process::id(),
            );
            let previous_attention = facts_cache
                .refresh_attention(&shorts, &|short| super::attention::load(state, short));
            // Emit one attention notice per transition on the facts cadence, never per frame (#354).
            let focused_short = panes.get(focused).map(|p| p.short().to_string());
            let sample_now = super::state::now_secs();
            for short in &shorts {
                let Some(status) = facts_cache.disk.attention.get(short) else {
                    continue;
                };
                let reason = super::attention::reason(status);
                let sample = notify::AttentionSample {
                    short,
                    previous: previous_attention.get(short).map(super::attention::project),
                    next: super::attention::project(status),
                    reason: &reason,
                    revision: status.revision,
                    last_transition: status.last_transition,
                    focused: focused_short.as_deref() == Some(short.as_str()),
                };
                if let Some(text) = attention_notices.observe(&sample, sample_now, NOTICE_MAX_COLS)
                {
                    push_notice(&mut notices, Instant::now(), text);
                }
                // Dash refresh PR2: a worker finishing is the SAME transition
                // the notice above already computed (`DoneUnread`, not
                // focused) -- reused here rather than re-derived, for the
                // row's own flash and the dashboard-wide toast. Unlike the
                // notice, a flash/toast is not suppressed for the focused
                // pane: those exist to be *noticed*, not to interrupt.
                if sample.next == super::attention::Projection::DoneUnread
                    && sample.previous.is_some_and(|prev| prev != sample.next)
                {
                    flash_started.insert(short.clone(), Instant::now());
                    toast = Some((format!("\u{23fa} {short} finished"), Instant::now()));
                }
            }
            attention_notices.retain(&shorts);
            // Flash only when a session's unread mail count rises.
            for short in mail_flash_targets(
                &mail_before_refresh,
                &facts_cache.disk.mail_by_session,
                seen_first_facts_refresh,
            ) {
                flash_started.insert(short, Instant::now());
            }
            flash_started.retain(|short, _| shorts.iter().any(|s| s == short));
            // Announce only a rollover that committed after this dashboard began observing it.
            let current_settlement = facts_cache
                .disk
                .rollover_record
                .as_ref()
                .and_then(|record| record.settlement.clone());
            if let Some(text) = facts_cache
                .disk
                .rollover_record
                .as_ref()
                .and_then(|record| {
                    rollover_committed_toast(
                        &current_settlement,
                        &last_rollover_settlement,
                        seen_first_facts_refresh,
                        &record.source_agent,
                    )
                })
            {
                toast = Some((text, Instant::now()));
            }
            last_rollover_settlement = current_settlement;
            seen_first_facts_refresh = true;
        }
        // Prune recently reaped shorts once the registry snapshot no longer lists them.
        reaped_recent.retain(|short| {
            facts_cache
                .registry
                .iter()
                .any(|(record, _)| &record.short == short)
        });
        let visible_registry: Vec<(sessions::Record, sessions::Liveness)> = facts_cache
            .registry
            .iter()
            .filter(|(record, _)| !reaped_recent.contains(&record.short))
            .cloned()
            .collect();
        // Clamp focus and selection after pane arrivals or removals.
        focused = focused.min(panes.len().saturating_sub(1));
        // Sync quiet pane state once per tick before building sidebar rows (#349).
        sync_quiet_heuristic_attention(&panes, state, &mut quiet_lifecycle);
        let tick_now = super::state::now_secs();
        for pane in &mut panes {
            report_settled_pane(pane, state, cfg, &mut errors);
            report_stalled_compaction(pane, state, cfg, &mut errors, tick_now);
        }
        let rows = assemble_sidebar(
            &build_pane_rows(&panes, &retained_ended),
            &visible_registry,
            &facts_cache.disk.scores,
            selected,
            focused,
            std::process::id(),
            super::state::now_secs(),
        );
        let total_rows = rows.len();
        selected = selected.min(total_rows.saturating_sub(1));

        // Drain a bounded batch of queued input after the first poll, then run maintenance and redraw.
        let mut drained = 0usize;
        // Pointer motion is coalesced: only the last position of a burst is looked at, and a
        // frame is drawn for it only if the hover target changed.
        let mut moved_only = 0usize;
        let mut pending_move: Option<crossterm::event::MouseEvent> = None;
        while drained < MAX_INPUT_DRAIN_PER_TICK {
            let wait = if drained == 0 {
                frame_poll_wait(
                    input_poll_wait(last_activity.elapsed()),
                    tree_view.frame_interval(),
                    last_tree_draw.map(|at| at.elapsed()),
                )
            } else {
                Duration::ZERO
            };
            match event::poll(wait) {
                Ok(true) => {
                    let mut read = event::read();
                    let mut mouse_action = None;
                    // Carry menu effects outside overlay reduction before dispatching them (#354).
                    let mut apply_menu_action: Option<(String, ui::MenuAction)> = None;
                    // Clear pending dialog clicks whenever that dialog opens, closes or changes.
                    let overlay_ident = overlay_identity(&overlay);
                    if overlay_ident != last_overlay_ident {
                        last_overlay_click = None;
                        last_overlay_ident = overlay_ident;
                    }
                    // Activity, for the adaptive poll wait above: a keyboard
                    // or mouse event, whatever `filter_key`/the overlay below
                    // goes on to decide it means.
                    if matches!(read, Ok(Event::Key(_)) | Ok(Event::Mouse(_))) {
                        last_activity = Instant::now();
                    }
                    // Log each input event before dispatch with its prior prefix and overlay state.
                    if let Some(log) = keylog.as_mut() {
                        log.observe(&read, prefix_armed, &overlay);
                    }
                    if let Ok(Event::Mouse(mouse)) = read.as_ref()
                        && matches!(mouse.kind, MouseEventKind::Moved)
                        && matches!(overlay, ui::Overlay::None)
                        && (tree_view.captures_plain_keys() || tree_view.in_chat())
                    {
                        pending_move = Some(*mouse);
                        moved_only += 1;
                        read = Ok(Event::FocusGained);
                    }
                    // Route each pointer event against the frame geometry that was drawn (#354).
                    if let Ok(Event::Mouse(mouse)) = read.as_ref() {
                        let mouse = *mouse;
                        // A fresh press clears prior selection and pending gestures regardless of where it lands.
                        if mouse_capture
                            && matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left))
                        {
                            selection = None;
                            pending_press = None;
                        }
                        let route = route_mouse(
                            &frame_snapshot,
                            mouse,
                            mouse_capture,
                            !matches!(overlay, ui::Overlay::None),
                            selection.is_some() || pending_press.is_some(),
                        );
                        // The tree view owns the pointer: clicks select and open boxes, the wheel scrolls.
                        // An open chat keeps the pane's pointer except on its own chrome.
                        let tree_owns = matches!(overlay, ui::Overlay::None)
                            && (tree_view.captures_plain_keys() || tree_view.in_chat());
                        let tree_outcome = if tree_owns {
                            let tree_facts = build_tree_facts(
                                &facts_cache,
                                approvals_hub.as_ref(),
                                &panes,
                                focused,
                                cfg,
                                &rows,
                                &retained_ended,
                                &kept_requests,
                                repo,
                            );
                            if tree_view.in_chat() {
                                tree_view.chat_mouse(mouse, &tree_facts)
                            } else {
                                Some(tree_view.mouse(
                                    mouse,
                                    tree_view::Surface::page(Rect::new(0, 0, term_cols, term_rows)),
                                    &tree_facts,
                                    Instant::now(),
                                ))
                            }
                        } else {
                            None
                        };
                        let route = match tree_outcome {
                            Some(outcome) => {
                                apply_tree_outcome(
                                    outcome,
                                    &mut tree_view,
                                    approvals_hub.as_mut(),
                                    state,
                                    repo,
                                    &rows,
                                    &mut TreeDash {
                                        selected: &mut selected,
                                        focused: &mut focused,
                                        chrome_selection: &mut chrome_selection,
                                        reveal_sidebar: &mut reveal_sidebar,
                                        overlay: &mut overlay,
                                        notices: &mut notices,
                                        errors: &mut errors,
                                        panes: &mut panes,
                                        nudge_queues: &mut nudge_queues,
                                        retained: &mut retained_ended,
                                        kept: &mut kept_requests,
                                        cfg,
                                        pane_size,
                                        requests_dir: &requests_dir,
                                    },
                                );
                                MouseRoute::Consume
                            }
                            None => route,
                        };
                        // Reject overlay routes from a frame whose dialog identity is no longer current (#354).
                        let route = if overlay_route_is_current(
                            &route,
                            &overlay,
                            &frame_snapshot_overlay_ident,
                        ) {
                            route
                        } else {
                            MouseRoute::Consume
                        };
                        if route != MouseRoute::Grid {
                            read = Ok(Event::FocusGained);
                        }
                        match route {
                            MouseRoute::Grid | MouseRoute::Consume => {}
                            MouseRoute::Select(id) => {
                                // Selecting a pane cannot acknowledge unread output; only showing it can (#354).
                                (selected, focused) = select_row(&id, &rows, selected, focused);
                                chrome_selection = None;
                            }
                            MouseRoute::Summary => chrome_selection = Some(Hit::SidebarSummary),
                            MouseRoute::Toggle(id) => {
                                fold_group(&mut collapsed_groups, &id, GroupFold::Toggle);
                                chrome_selection = Some(Hit::GroupToggle(id));
                            }
                            MouseRoute::ScrollRoster(delta) => {
                                // The frame's own clamp (`ui::reveal_offset`)
                                // trims this to the last screenful; the cap
                                // here only keeps a held wheel from running
                                // the counter away from the list entirely.
                                sidebar_offset = sidebar_offset
                                    .saturating_add_signed(delta)
                                    .min(frame_snapshot.roster.len());
                                reveal_sidebar = false;
                            }
                            MouseRoute::Action(action) => {
                                mouse_action = Some(action);
                                read = Ok(Event::Key(KeyEvent::new(
                                    KeyCode::Null,
                                    KeyModifiers::NONE,
                                )));
                            }
                            // Activate a dialog row only on a second click of that same row within the double-click window (#354).
                            MouseRoute::OverlayRow(index) => {
                                let now = Instant::now();
                                let double = last_overlay_click.is_some_and(|(last, at)| {
                                    last == index
                                        && now.saturating_duration_since(at) <= DOUBLE_CLICK
                                });
                                if let Some((_, offset, len)) = overlay.list_state() {
                                    overlay.set_list_state(
                                        index.min(len.saturating_sub(1)),
                                        ui::list_scroll(
                                            len,
                                            frame_snapshot.overlay_capacity,
                                            index,
                                            offset,
                                        ),
                                    );
                                }
                                last_overlay_click = Some((index, now));
                                if double {
                                    last_overlay_click = None;
                                    read = Ok(Event::Key(KeyEvent::new(
                                        KeyCode::Enter,
                                        KeyModifiers::NONE,
                                    )));
                                }
                            }
                            // A hint on the dialog's pinned hint row IS its
                            // key: the reducer sees the same keystroke the
                            // keyboard would have delivered, so a pointer can
                            // never reach a path the keyboard could not.
                            MouseRoute::OverlayKey(code) => {
                                last_overlay_click = None;
                                read = Ok(Event::Key(KeyEvent::new(code, KeyModifiers::NONE)));
                            }
                            MouseRoute::ScrollOverlay(delta) => {
                                if let Some((cursor, offset, len)) = overlay.list_state() {
                                    let capacity = frame_snapshot.overlay_capacity;
                                    let next = cursor
                                        .saturating_add_signed(delta * WHEEL_STEP)
                                        .min(len.saturating_sub(1));
                                    overlay.set_list_state(
                                        next,
                                        ui::list_scroll(len, capacity, next, offset),
                                    );
                                }
                            }
                        }
                    }
                    match read {
                        Ok(Event::Key(key)) if key.kind == KeyEventKind::Press => {
                            input_errors = 0;
                            // Apply shared viewport keys before individual list reducers (#354).
                            let key = match (ui::list_page_move(key), overlay.list_state()) {
                                (Some(mv), Some((cursor, offset, len))) => {
                                    let capacity = frame_snapshot.overlay_capacity;
                                    let next = ui::list_move(cursor, len, capacity, mv);
                                    overlay.set_list_state(
                                        next,
                                        ui::list_scroll(len, capacity, next, offset),
                                    );
                                    KeyEvent::new(KeyCode::Null, KeyModifiers::NONE)
                                }
                                _ => key,
                            };
                            // Keep overlay ownership of input while allowing a palette action to dispatch once (#354).
                            let overlay_was_open = !matches!(overlay, ui::Overlay::None);
                            if overlay_was_open {
                                let current = std::mem::take(&mut overlay);
                                // Task 2: what the slot held on the way in, so
                                // the `OVERLAY` line below can report the swap
                                // rather than only the result.
                                let took = overlay_name(&current);
                                match current {
                                    ui::Overlay::None => {}
                                    ui::Overlay::QuitConfirm(working) => {
                                        let (next, effect) = quit_confirm_reduce(working, key);
                                        overlay = match next {
                                            Some(w) => ui::Overlay::QuitConfirm(w),
                                            None => ui::Overlay::None,
                                        };
                                        if matches!(effect, Some(QuitConfirmEffect::Confirm)) {
                                            // `overlay` was taken above, so this is the
                                            // one quit path that cannot have a restore
                                            // dialog pending: it *is* the open overlay.
                                            // `deferred_restore` (G3) is independent of
                                            // that and still owed regardless.
                                            on_quit(
                                                &panes,
                                                &[],
                                                &deferred_restore,
                                                &requests_dir,
                                                state,
                                                repo,
                                            );
                                            render_shutting_down(&mut terminal, panes.len());
                                            shutdown_all(&mut panes, cfg, &mut errors);
                                            break 'main 0;
                                        }
                                    }
                                    ui::Overlay::Spawn(draft) => {
                                        let (next, effect) = spawn_overlay_reduce(draft, key);
                                        overlay = match next {
                                            Some(d) => ui::Overlay::Spawn(d),
                                            None => ui::Overlay::None,
                                        };
                                        match effect {
                                            Some(SpawnEffect::Notice(note)) => {
                                                push_error(&mut errors, note)
                                            }
                                            // Straight through the same validation
                                            // and spawn path a pane's own `zirv ctx
                                            // agent` request takes -- argv guard,
                                            // repo check, pane cap, agent gate --
                                            // rather than a second, parallel one.
                                            Some(SpawnEffect::Submit { agent, prompt }) => {
                                                let req = spawnreq::SpawnRequest {
                                                    kill: None,
                                                    name: None,
                                                    agent,
                                                    prompt,
                                                    cwd: repo.to_path_buf(),
                                                    requested_by: dashboard_short.clone(),
                                                    // The overlay asks for an agent and
                                                    // a prompt, nothing else, so this
                                                    // spawn takes the operator's own
                                                    // configured worker default.
                                                    model: None,
                                                    // Only a direct action in this live dashboard can assert a human is present for a spawned pane.
                                                    interactive: true,
                                                    // The overlay asks for an agent and
                                                    // a prompt, nothing else: no role,
                                                    // no group, and no parent session --
                                                    // this spawn IS the delegation root,
                                                    // the same as an operator typing
                                                    // `zirv ctx agent` at a plain
                                                    // terminal (`parent_role_for` reads
                                                    // an absent `parent_session` as
                                                    // `PromptRole::Orchestrator`).
                                                    role: None,
                                                    parent_session: None,
                                                    work_group_id: None,
                                                    budget_tokens: None,
                                                    // The Spawn overlay has no
                                                    // `--force` of its own (see
                                                    // `fulfill_spawn_request`'s
                                                    // own gate comment): an
                                                    // operator who wants to
                                                    // override the ceiling from
                                                    // here raises `pace.
                                                    // spawn_hard_pct`, or runs
                                                    // `zirv ctx agent --force`
                                                    // directly instead.
                                                    force: false,
                                                    // Overlay spawns use this dashboard's repo as their workdir (#228).
                                                    workdir: None,
                                                    // Overlay spawns use ordinary writing mode (#267).
                                                    mode: super::permit::WorkerMode::Writing,
                                                    owns_workdir: false,
                                                    // Overlay spawns declare no result contract (#318).
                                                    result_schema: None,
                                                    // Overlay spawns inherit the dashboard root envelope without extra path or network flags (#262).
                                                    envelope: None,
                                                    path_scope: Vec::new(),
                                                    no_network: false,
                                                    depth: None,
                                                    // Overlay spawns carry only the chosen agent and prompt.
                                                    max_restarts: None,
                                                    timeout_secs: None,
                                                    max_tool_calls: None,
                                                    flags: Vec::new(),
                                                    // And no seat instructions
                                                    // of its own (R1-4): this
                                                    // pane gets exactly the
                                                    // prompt this dashboard
                                                    // composes for it.
                                                    system_prompt: None,
                                                    // An overlay spawn is a root delegation with no requester seat to fence (#543).
                                                    parent_seat_generation: None,
                                                };
                                                let panes_before_spawn = panes.len();
                                                // `trusted_interactive: true` --
                                                // this exact call is the
                                                // dashboard's own live Spawn
                                                // overlay, a human's keypress in
                                                // this process's own event loop
                                                // this instant; see
                                                // `fulfill_spawn_request`'s own
                                                // doc comment.
                                                let fulfilled = fulfill_spawn_request(
                                                    &req,
                                                    true,
                                                    // Use the dashboard-derived session ID for parent lineage, never request JSON (#249, #250).
                                                    Some(&dashboard_short),
                                                    &mut panes,
                                                    &mut nudge_queues,
                                                    cfg,
                                                    state,
                                                    repo,
                                                    pane_size,
                                                    &requests_dir,
                                                    &mut errors,
                                                );
                                                // Keep a view-only selection on the same logical row after pane insertion.
                                                selected = insert_fixup(
                                                    panes_before_spawn,
                                                    panes.len(),
                                                    selected,
                                                );
                                                match fulfilled {
                                                    // Report a successful spawn as a transient notice.
                                                    Ok((short, _, advisory)) => {
                                                        push_notice(
                                                            &mut notices,
                                                            Instant::now(),
                                                            format!(
                                                                "spawned {} as {short}",
                                                                req.agent
                                                            ),
                                                        );
                                                        // Report spawn success through the transient notice channel (#399).
                                                        if let Some(text) = advisory {
                                                            push_notice(
                                                                &mut notices,
                                                                Instant::now(),
                                                                text,
                                                            );
                                                        }
                                                    }
                                                    Err(refusal) => {
                                                        push_error(&mut errors, refusal.reason)
                                                    }
                                                }
                                            }
                                            None => {}
                                        }
                                    }
                                    ui::Overlay::Restore(view) => {
                                        let (next, effect) = restore_overlay_reduce(view, key);
                                        overlay = match next {
                                            Some(v) => ui::Overlay::Restore(v),
                                            None => ui::Overlay::None,
                                        };
                                        if let Some(RestoreEffect::Confirm(indices)) = effect {
                                            // Apply the live-pane cap to restores as to fresh spawns.
                                            let (take, skipped) = restore_budget(
                                                panes.len(),
                                                cfg.dash.max_panes,
                                                indices.len(),
                                            );
                                            // Carry cap-skipped restore candidates into the next roster rather than losing them when the dialog closes.
                                            let (to_spawn, deferred) = partition_restore_selection(
                                                indices,
                                                &restore_candidates,
                                                take,
                                            );
                                            let panes_before_restore = panes.len();
                                            for candidate in &to_spawn {
                                                spawn_restored_pane(
                                                    candidate,
                                                    &mut panes,
                                                    &mut nudge_queues,
                                                    cfg,
                                                    state,
                                                    repo,
                                                    pane_size,
                                                    &requests_dir,
                                                    &mut errors,
                                                    &mut deferred_restore,
                                                );
                                            }
                                            // Shift view-only selection after restored panes are appended.
                                            selected = insert_fixup(
                                                panes_before_restore,
                                                panes.len(),
                                                selected,
                                            );
                                            deferred_restore.extend(deferred);
                                            if skipped > 0 {
                                                push_error(
                                                    &mut errors,
                                                    format!(
                                                        "restore: pane limit reached (dash.max_panes = \
                                                 {}); {skipped} session(s) not restored",
                                                        cfg.dash.max_panes
                                                    ),
                                                );
                                            }
                                        }
                                    }
                                    ui::Overlay::Mail(view) => {
                                        let (next, effect) = mail_overlay_reduce(view, key);
                                        overlay = match next {
                                            Some(v) => ui::Overlay::Mail(v),
                                            None => ui::Overlay::None,
                                        };
                                        if let Some(effect) = effect {
                                            apply_mail_effect(
                                                effect,
                                                state,
                                                repo,
                                                cfg,
                                                &dashboard_short,
                                                &agent_name,
                                                &mut errors,
                                            );
                                        }
                                    }
                                    ui::Overlay::Memory(view) => {
                                        let (next, effect) = memory_overlay_reduce(view, key);
                                        overlay = match next {
                                            Some(v) => ui::Overlay::Memory(v),
                                            None => ui::Overlay::None,
                                        };
                                        if let Some(effect) = effect {
                                            apply_memory_effect(
                                                effect,
                                                state,
                                                repo,
                                                cfg,
                                                &agent_name,
                                                &mut errors,
                                            );
                                        }
                                    }
                                    ui::Overlay::Nudge(draft) => {
                                        let (next, submit) = nudge_overlay_reduce(draft, key);
                                        overlay = match next {
                                            Some(d) => ui::Overlay::Nudge(d),
                                            None => ui::Overlay::None,
                                        };
                                        if let Some(NudgeSubmit { target, text }) = submit {
                                            submit_nudge(
                                                target,
                                                &text,
                                                &mut panes,
                                                &mut nudge_queues,
                                                repo,
                                                env,
                                                &mut errors,
                                                &mut notices,
                                                Instant::now(),
                                            );
                                        }
                                    }
                                    // Use the focused pane's handover picker (#84).
                                    ui::Overlay::Handover(draft) => {
                                        let (next, effect) = handover_overlay_reduce(draft, key);
                                        overlay = match next {
                                            Some(d) => ui::Overlay::Handover(d),
                                            None => ui::Overlay::None,
                                        };
                                        if let Some(HandoverEffect::Swap {
                                            target_short,
                                            target_agent,
                                            target_model,
                                        }) = effect
                                        {
                                            let idx = panes
                                                .iter()
                                                .position(|p| p.short() == target_short);
                                            match idx {
                                                Some(idx)
                                                    if panes[idx].state() == PaneState::Idle =>
                                                {
                                                    // Abort an already-prepared automatic seat transaction before a manual swap of that pane (#358).
                                                    if pending_rollover.as_ref().is_some_and(
                                                        |(short, _, _)| short == panes[idx].short(),
                                                    ) && let Some((short, generation, _)) =
                                                        pending_rollover.take()
                                                    {
                                                        let _ = super::rollover::fail(
                                                            state,
                                                            "dash",
                                                            &short,
                                                            generation,
                                                            "superseded by a manual handover \
                                                             request",
                                                            super::state::now_secs(),
                                                        );
                                                    }
                                                    handover_pane(
                                                        &mut panes[idx],
                                                        &handover::HandoverRequest {
                                                            target_agent: target_agent.clone(),
                                                            target_model: Some(
                                                                target_model.clone(),
                                                            ),
                                                            force: false,
                                                            requested_at: super::state::now_secs(),
                                                            // Reached only from the
                                                            // dashboard's own Handover
                                                            // overlay: a human at this
                                                            // live TUI just chose it.
                                                            interactive: true,
                                                            automatic: false,
                                                            generation: None,
                                                            structural_only: false,
                                                            resume_session: None,
                                                            target_runtime: None,
                                                            target_route: None,
                                                        },
                                                        cfg,
                                                        repo,
                                                        state,
                                                        &mut errors,
                                                        None,
                                                    );
                                                }
                                                Some(_) => push_error(
                                                    &mut errors,
                                                    format!(
                                                        "handover: pane {target_short} is not \
                                                         idle; retry once it is"
                                                    ),
                                                ),
                                                None => push_error(
                                                    &mut errors,
                                                    "handover: target pane no longer exists"
                                                        .to_string(),
                                                ),
                                            }
                                        }
                                    }
                                    // Run palette actions through the same dispatch as their keyboard and menu bindings (#354).
                                    ui::Overlay::Palette(view) => {
                                        let (next, effect) = palette_overlay_reduce(view, key);
                                        overlay = match next {
                                            Some(v) => ui::Overlay::Palette(v),
                                            None => ui::Overlay::None,
                                        };
                                        if let Some(PaletteEffect(id)) = effect
                                            && let Some(descriptor) = actions::descriptor(id)
                                        {
                                            match (descriptor.dash_action(), descriptor.menu) {
                                                (Some(action), _) => mouse_action = Some(action),
                                                (None, Some(menu)) => {
                                                    match rows
                                                        .get(selected)
                                                        .map(|row| row.short.clone())
                                                    {
                                                        Some(target) => {
                                                            apply_menu_action =
                                                                Some((target, menu));
                                                        }
                                                        None => push_notice(
                                                            &mut notices,
                                                            Instant::now(),
                                                            "no session row is selected".into(),
                                                        ),
                                                    }
                                                }
                                                (None, None) => {}
                                            }
                                        }
                                    }
                                    // Acknowledge displayed errors on close without deleting them (#354).
                                    ui::Overlay::Errors(view) => {
                                        let (next, ack) = errors_overlay_reduce(view, key);
                                        overlay = match next {
                                            Some(v) => ui::Overlay::Errors(v),
                                            None => ui::Overlay::None,
                                        };
                                        if let Some(ack) = ack {
                                            errors.acknowledge(ack.mark);
                                        }
                                    }
                                    // Closing the read-only inspector restores input to the previously focused pane (#354).
                                    ui::Overlay::Inspector(view) => {
                                        overlay = match inspector_overlay_reduce(view, key) {
                                            Some(v) => ui::Overlay::Inspector(v),
                                            None => ui::Overlay::None,
                                        };
                                    }
                                    // Click affordance follow-up: same
                                    // read-only shape as `Inspector` above --
                                    // a rollup snapshot has nothing to
                                    // acknowledge, only browse and close.
                                    ui::Overlay::JevErrors(view) => {
                                        overlay = match jev_errors_overlay_reduce(view, key) {
                                            Some(v) => ui::Overlay::JevErrors(v),
                                            None => ui::Overlay::None,
                                        };
                                    }
                                    // Approvals list (#840): the same read-only browse-and-close shape.
                                    ui::Overlay::Approvals(view) => {
                                        overlay = match jev_errors_overlay_reduce(view, key) {
                                            Some(v) => ui::Overlay::Approvals(v),
                                            None => ui::Overlay::None,
                                        };
                                    }
                                    ui::Overlay::Menu(view) => {
                                        let (next, effect) = menu_overlay_reduce(view, key);
                                        overlay = match next {
                                            Some(v) => ui::Overlay::Menu(v),
                                            None => ui::Overlay::None,
                                        };
                                        if let Some(MenuEffect { target, action }) = effect {
                                            apply_menu_action = Some((target, action));
                                        }
                                    }
                                }
                                // Log overlay take and assign separately so a dialog opened then cleared in one event is observable.
                                if let Some(log) = keylog.as_mut() {
                                    log.overlay_swap(took, &overlay);
                                }
                                // Dispatch context-menu effects through the same action paths as keyboard bindings (#354).
                                if let Some((target, action)) = apply_menu_action.take() {
                                    let now = Instant::now();
                                    let row = rows.iter().find(|r| r.short == target).cloned();
                                    let cwd = row_cwd(&target, &panes, &retained_ended);
                                    // Only dashboard-wide actions can run from the summary line's menu (#354).
                                    if target == DASHBOARD_TARGET {
                                        match action {
                                            ui::MenuAction::Inspect => {
                                                overlay = ui::Overlay::Inspector(
                                                    build_dashboard_inspector(&dashboard_facts(
                                                        &harness_label,
                                                        &dashboard_short,
                                                        &rows,
                                                        &panes,
                                                        &facts_cache,
                                                        state,
                                                        launched_at,
                                                        mouse_capture,
                                                        sidebar_cols,
                                                        now,
                                                    )),
                                                );
                                            }
                                            ui::MenuAction::Mail => {
                                                overlay =
                                                    ui::Overlay::Mail(build_mail_view(state, repo));
                                            }
                                            _ => {}
                                        }
                                    } else {
                                        match action {
                                            ui::MenuAction::Inspect
                                            | ui::MenuAction::Evidence
                                            | ui::MenuAction::OpenWorktree => {
                                                match row.as_ref() {
                                                    Some(row) => {
                                                        if action == ui::MenuAction::OpenWorktree {
                                                            push_notice(
                                                                &mut notices,
                                                                now,
                                                                match cwd.as_deref() {
                                                                    Some(path) => {
                                                                        format!(
                                                                            "{target} runs in {path}"
                                                                        )
                                                                    }
                                                                    None => format!(
                                                                        "{target}: {MENU_NO_CWD}"
                                                                    ),
                                                                },
                                                            );
                                                        }
                                                        let mut view = build_inspector_view(
                                                            row,
                                                            cwd.as_deref(),
                                                            &errors,
                                                        );
                                                        if action == ui::MenuAction::Evidence {
                                                            view.cursor = view
                                                                .section_start(INSPECT_EVIDENCE);
                                                        }
                                                        // Opening the inspector on
                                                        // a retained done-unread
                                                        // row IS reading it -- the
                                                        // render path's own rule
                                                        // needs focus, which such a
                                                        // row can never have.
                                                        if let Some(short) = done_unread_ack
                                                            .acknowledge(inspect_ack_candidate(row))
                                                        {
                                                            let acked =
                                                                super::attention::mark_seen_io(
                                                                    state, &short,
                                                                );
                                                            facts_cache
                                                                .disk
                                                                .attention
                                                                .insert(short, acked);
                                                        }
                                                        overlay = ui::Overlay::Inspector(view);
                                                    }
                                                    None => push_notice(
                                                        &mut notices,
                                                        now,
                                                        format!(
                                                            "{target} is no longer on the roster"
                                                        ),
                                                    ),
                                                }
                                            }
                                            ui::MenuAction::Focus => {
                                                (selected, focused) =
                                                    select_row(&target, &rows, selected, focused);
                                                chrome_selection = None;
                                                reveal_sidebar = true;
                                            }
                                            ui::MenuAction::Nudge => {
                                                overlay = ui::Overlay::Nudge(nudge_draft(
                                                    &target, &panes,
                                                ));
                                            }
                                            ui::MenuAction::Mail => {
                                                overlay =
                                                    ui::Overlay::Mail(build_mail_view(state, repo));
                                            }
                                            ui::MenuAction::Handover => {
                                                let mut items = Vec::new();
                                                for agent in adapters::available_adapter_names(cfg)
                                                {
                                                    for tier in handover::TIERS {
                                                        if let Ok(model) = handover::resolve_model(
                                                            agent, tier, cfg,
                                                        ) {
                                                            items.push((
                                                                agent.to_string(),
                                                                tier.to_string(),
                                                                model,
                                                            ));
                                                        }
                                                    }
                                                }
                                                overlay =
                                                    ui::Overlay::Handover(ui::HandoverDraft {
                                                        items,
                                                        cursor: 0,
                                                        offset: 0,
                                                        target_short: target.clone(),
                                                    });
                                            }
                                            ui::MenuAction::Stop => {
                                                stop_pane(
                                                    &target,
                                                    &mut panes,
                                                    cfg,
                                                    &mut errors,
                                                    &mut notices,
                                                    now,
                                                );
                                            }
                                            ui::MenuAction::Restore | ui::MenuAction::Retry => {
                                                restore_ended_row(
                                                    &target,
                                                    &mut panes,
                                                    &mut nudge_queues,
                                                    &mut retained_ended,
                                                    &mut kept_requests,
                                                    cfg,
                                                    state,
                                                    repo,
                                                    pane_size,
                                                    &requests_dir,
                                                    &mut errors,
                                                    &mut notices,
                                                    now,
                                                    &rows,
                                                    &mut selected,
                                                );
                                            }
                                            ui::MenuAction::Dismiss => {
                                                let before = retained_ended.len();
                                                retained_ended.retain(|row| row.short != target);
                                                if retained_ended.len() < before {
                                                    // The row left the middle of
                                                    // the combined roster, so the
                                                    // cursor has to come back
                                                    // inside it.
                                                    selected =
                                                        selected.min(rows.len().saturating_sub(2));
                                                    reveal_sidebar = true;
                                                    push_notice(
                                                        &mut notices,
                                                        now,
                                                        format!("dismissed {target}"),
                                                    );
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                            if !overlay_was_open || mouse_action.is_some() {
                                let (armed, verdict) = mouse_action
                                    .take()
                                    .map(|action| (false, InputVerdict::Dash(action)))
                                    .unwrap_or_else(|| filter_key(prefix_armed, key));
                                let armed_before = prefix_armed;
                                prefix_armed = armed;
                                // Dismiss the first-run tip after a prefixed key or Esc (#354).
                                let tip_dismissed_by_esc = first_run_tip
                                    && key.code == KeyCode::Esc
                                    && !matches!(verdict, InputVerdict::Dash(_));
                                if first_run_tip
                                    && (matches!(verdict, InputVerdict::Dash(_))
                                        || key.code == KeyCode::Esc)
                                {
                                    first_run_tip = false;
                                    // Persist tip dismissal once, when the operator dismisses it, rather than at launch.
                                    mark_first_run_tip_seen(state);
                                }
                                // Empty `ToChild`, not `Pending`: the prefix
                                // itself is not armed by this Esc (`armed` is
                                // still whatever `filter_key` decided above),
                                // this is the same "swallowed, nothing to
                                // forward" shape `filter_key` already uses for
                                // an armed-but-unbound key.
                                let verdict = if tip_dismissed_by_esc {
                                    InputVerdict::ToChild(Vec::new())
                                } else {
                                    verdict
                                };
                                // Log the actual stored prefix state and dispatched action after each event.
                                if let Some(log) = keylog.as_mut() {
                                    log.dispatch(armed_before, prefix_armed, &verdict);
                                }
                                match verdict {
                                    // Bind a right-click menu to the row hit by that gesture, not the sidebar selection (#354).
                                    InputVerdict::Dash(
                                        action @ (DashAction::ContextMenu(_)
                                        | DashAction::ContextActions),
                                    ) => {
                                        // Keep session actions visible but inert in the dashboard summary menu (#354).
                                        let summary_selected = action == DashAction::ContextActions
                                            && chrome_selection == Some(Hit::SidebarSummary);
                                        let target = match action {
                                            DashAction::ContextMenu(id) => Some(id),
                                            _ => session_target(
                                                chrome_selection.as_ref(),
                                                &rows,
                                                selected,
                                            ),
                                        };
                                        if summary_selected {
                                            overlay = ui::Overlay::Menu(build_summary_menu_view());
                                        } else {
                                            match target
                                                .as_ref()
                                                .and_then(|id| rows.iter().find(|r| r.short == *id))
                                            {
                                                Some(row) => {
                                                    let facts = menu_facts_for(
                                                        row,
                                                        &panes,
                                                        &retained_ended,
                                                    );
                                                    overlay =
                                                        ui::Overlay::Menu(build_menu_view(&facts));
                                                }
                                                None => push_notice(
                                                    &mut notices,
                                                    Instant::now(),
                                                    "no session row is selected".into(),
                                                ),
                                            }
                                        }
                                    }
                                    // Opening an inspector acknowledges a retained done-unread row; summary selection opens dashboard inspection (#354).
                                    InputVerdict::Dash(DashAction::Inspect)
                                        if chrome_selection == Some(Hit::SidebarSummary) =>
                                    {
                                        overlay = ui::Overlay::Inspector(
                                            build_dashboard_inspector(&dashboard_facts(
                                                &harness_label,
                                                &dashboard_short,
                                                &rows,
                                                &panes,
                                                &facts_cache,
                                                state,
                                                launched_at,
                                                mouse_capture,
                                                sidebar_cols,
                                                Instant::now(),
                                            )),
                                        );
                                    }
                                    InputVerdict::Dash(DashAction::Inspect) => {
                                        match session_target(
                                            chrome_selection.as_ref(),
                                            &rows,
                                            selected,
                                        )
                                        .and_then(|id| rows.iter().find(|r| r.short == id))
                                        {
                                            Some(row) => {
                                                let cwd =
                                                    row_cwd(&row.short, &panes, &retained_ended);
                                                let view = build_inspector_view(
                                                    row,
                                                    cwd.as_deref(),
                                                    &errors,
                                                );
                                                if let Some(short) = done_unread_ack
                                                    .acknowledge(inspect_ack_candidate(row))
                                                {
                                                    let acked = super::attention::mark_seen_io(
                                                        state, &short,
                                                    );
                                                    facts_cache.disk.attention.insert(short, acked);
                                                }
                                                overlay = ui::Overlay::Inspector(view);
                                            }
                                            None => push_notice(
                                                &mut notices,
                                                Instant::now(),
                                                "no session row is selected".into(),
                                            ),
                                        }
                                    }
                                    // Restore only a retained row whose original spawn request is available (#354).
                                    InputVerdict::Dash(DashAction::RestoreRow) => {
                                        match session_target(
                                            chrome_selection.as_ref(),
                                            &rows,
                                            selected,
                                        ) {
                                            Some(short) => restore_ended_row(
                                                &short,
                                                &mut panes,
                                                &mut nudge_queues,
                                                &mut retained_ended,
                                                &mut kept_requests,
                                                cfg,
                                                state,
                                                repo,
                                                pane_size,
                                                &requests_dir,
                                                &mut errors,
                                                &mut notices,
                                                Instant::now(),
                                                &rows,
                                                &mut selected,
                                            ),
                                            None => push_notice(
                                                &mut notices,
                                                Instant::now(),
                                                "no session row is selected".into(),
                                            ),
                                        }
                                    }
                                    // In an open chat the arrows step to the previous or next agent's chat.
                                    InputVerdict::Dash(
                                        action @ (DashAction::CollapseGroup
                                        | DashAction::ExpandGroup),
                                    ) if tree_view.in_chat() => {
                                        let tree_facts = build_tree_facts(
                                            &facts_cache,
                                            approvals_hub.as_ref(),
                                            &panes,
                                            focused,
                                            cfg,
                                            &rows,
                                            &retained_ended,
                                            &kept_requests,
                                            repo,
                                        );
                                        let delta = if action == DashAction::CollapseGroup {
                                            -1
                                        } else {
                                            1
                                        };
                                        let outcome = tree_view.chat_step(&tree_facts, delta);
                                        apply_tree_outcome(
                                            outcome,
                                            &mut tree_view,
                                            approvals_hub.as_mut(),
                                            state,
                                            repo,
                                            &rows,
                                            &mut TreeDash {
                                                selected: &mut selected,
                                                focused: &mut focused,
                                                chrome_selection: &mut chrome_selection,
                                                reveal_sidebar: &mut reveal_sidebar,
                                                overlay: &mut overlay,
                                                notices: &mut notices,
                                                errors: &mut errors,
                                                panes: &mut panes,
                                                nudge_queues: &mut nudge_queues,
                                                retained: &mut retained_ended,
                                                kept: &mut kept_requests,
                                                cfg,
                                                pane_size,
                                                requests_dir: &requests_dir,
                                            },
                                        );
                                    }
                                    InputVerdict::Dash(
                                        action @ (DashAction::CollapseGroup
                                        | DashAction::ExpandGroup),
                                    ) => {
                                        let id = group_under_cursor(
                                            chrome_selection.as_ref(),
                                            &rows,
                                            selected,
                                        );
                                        // The same reducer a click on the
                                        // header's disclosure triangle goes
                                        // through, so the two can never drift.
                                        // A flat row (no group) is a no-op.
                                        if let Some(id) = id {
                                            let fold = if action == DashAction::CollapseGroup {
                                                GroupFold::Collapse
                                            } else {
                                                GroupFold::Expand
                                            };
                                            if fold_group(&mut collapsed_groups, &id, fold) {
                                                // Collapsing folds the cursor's
                                                // own row away, so the cursor
                                                // moves up onto the header --
                                                // never onto another session,
                                                // and never touching focus.
                                                chrome_selection = Some(Hit::GroupToggle(id));
                                            }
                                            reveal_sidebar = true;
                                        }
                                    }
                                    InputVerdict::Pending => {}
                                    // Keys never reach a pane the operator cannot see; a plain key is the tree's own.
                                    InputVerdict::ToChild(_) if tree_view.captures_plain_keys() => {
                                        if !armed_before {
                                            let tree_facts = build_tree_facts(
                                                &facts_cache,
                                                approvals_hub.as_ref(),
                                                &panes,
                                                focused,
                                                cfg,
                                                &rows,
                                                &retained_ended,
                                                &kept_requests,
                                                repo,
                                            );
                                            let outcome = tree_view.key(
                                                key,
                                                tree_view::Surface::page(Rect::new(
                                                    0, 0, term_cols, term_rows,
                                                )),
                                                &tree_facts,
                                            );
                                            apply_tree_outcome(
                                                outcome,
                                                &mut tree_view,
                                                approvals_hub.as_mut(),
                                                state,
                                                repo,
                                                &rows,
                                                &mut TreeDash {
                                                    selected: &mut selected,
                                                    focused: &mut focused,
                                                    chrome_selection: &mut chrome_selection,
                                                    reveal_sidebar: &mut reveal_sidebar,
                                                    overlay: &mut overlay,
                                                    notices: &mut notices,
                                                    errors: &mut errors,
                                                    panes: &mut panes,
                                                    nudge_queues: &mut nudge_queues,
                                                    retained: &mut retained_ended,
                                                    kept: &mut kept_requests,
                                                    cfg,
                                                    pane_size,
                                                    requests_dir: &requests_dir,
                                                },
                                            );
                                        }
                                    }
                                    // Send unprefixed input only to the focused pane, and mark operator typing so idle-gated injection waits for the next turn.
                                    InputVerdict::ToChild(bytes) => {
                                        // Route wrapped input as PTY bytes and native input through its composer contract (#490).
                                        let routed_native = panes
                                            .get_mut(focused)
                                            .and_then(|pane| pane.native_mut())
                                            .map(|native| {
                                                native_pane::handle_native_key(
                                                    native,
                                                    key,
                                                    &mut native_ctrl_c,
                                                    false,
                                                    cfg.session.persistent,
                                                    cfg,
                                                )
                                            });
                                        match routed_native {
                                            // Ctrl+Q (or a double Ctrl+C)
                                            // inside a native pane closes THAT
                                            // pane, not the dashboard: the
                                            // dashboard has its own prefixed
                                            // quit, and a pane's own quit
                                            // binding must never take the
                                            // whole fleet with it.
                                            Some(native_pane::NativeKey::Quit) => {
                                                if let Some(pane) = panes.get_mut(focused)
                                                    && let Err(e) = pane.stop_now(0)
                                                {
                                                    push_error(
                                                        &mut errors,
                                                        format!("native pane quit: {e}"),
                                                    );
                                                }
                                            }
                                            Some(native_pane::NativeKey::Consumed) => {}
                                            None => {
                                                if !bytes.is_empty()
                                                    && let Some(pane) = panes.get_mut(focused)
                                                    && let Err(e) =
                                                        pane.write_operator_input(&bytes)
                                                {
                                                    push_error(
                                                        &mut errors,
                                                        format!("write_input: {e}"),
                                                    );
                                                }
                                            }
                                        }
                                    }
                                    InputVerdict::Dash(DashAction::LiteralPrefix) => {
                                        if let Some(pane) = panes.get_mut(focused)
                                            && let Err(e) =
                                                pane.write_operator_input(&literal_prefix_bytes())
                                        {
                                            push_error(&mut errors, format!("write_input: {e}"));
                                        }
                                    }
                                    // Switch/NextPane address panes, so they move
                                    // both indices; SelectUp/SelectDown only walk the
                                    // combined sidebar. All four in one pure place.
                                    InputVerdict::Dash(
                                        action @ (DashAction::Switch(_)
                                        | DashAction::NextPane
                                        | DashAction::SelectUp
                                        | DashAction::SelectDown),
                                    ) => {
                                        reveal_sidebar = true;
                                        (selected, focused) = navigate_roster(
                                            action,
                                            &rows,
                                            &frame_snapshot.roster,
                                            selected,
                                            focused,
                                            &mut chrome_selection,
                                        );
                                        // Navigation alone cannot acknowledge unread output; only a completed visible render can (#354).
                                    }
                                    // Scrollback, on the focused pane, for every
                                    // terminal that does not deliver wheel events
                                    // (or every operator who turned `dash.mouse`
                                    // off to keep native text selection).
                                    InputVerdict::Dash(
                                        action @ (DashAction::ScrollPageUp
                                        | DashAction::ScrollPageDown
                                        | DashAction::ScrollTop
                                        | DashAction::ScrollLive),
                                    ) => {
                                        if let Some(pane) = panes.get_mut(focused) {
                                            let alt = pane.alternate_screen();
                                            let mouse = pane.wants_mouse();
                                            let before = pane.scrollback();
                                            let (name, outcome) = match action {
                                                DashAction::ScrollPageUp => {
                                                    ("page-up", pane.scroll_page(true))
                                                }
                                                DashAction::ScrollPageDown => {
                                                    ("page-down", pane.scroll_page(false))
                                                }
                                                DashAction::ScrollTop => {
                                                    ("top", pane.scroll_to_top())
                                                }
                                                _ => ("live", pane.scroll_to_live()),
                                            };
                                            let after = pane.scrollback();
                                            // A selection's coordinates are only
                                            // meaningful against this pane's
                                            // scrollback offset at the moment
                                            // they were captured; a keyboard
                                            // scroll (`Ctrl+A PageUp`/`Home`/
                                            // `End`) moves it exactly like the
                                            // wheel does, so it translates the
                                            // same way (`translate_selection`).
                                            translate_selection(
                                                &mut selection,
                                                pane.short(),
                                                after as i64 - before as i64,
                                            );
                                            if let Some(log) = keylog.as_mut() {
                                                log.scroll(
                                                    name, alt, mouse, before, after, outcome,
                                                );
                                            }
                                            push_notice(
                                                &mut notices,
                                                Instant::now(),
                                                scroll_notice(outcome),
                                            );
                                        }
                                    }
                                    InputVerdict::Dash(DashAction::Zoom) => {
                                        zoomed = !zoomed;
                                        let m = effective_main(
                                            full,
                                            sidebar_cols,
                                            zoomed || tree_view.in_chat(),
                                        );
                                        let new_size = (m.height.max(1), m.width.max(1));
                                        // Cancel a selection before resizing changes its grid coordinates.
                                        cancel_selection_on_resize(
                                            &mut selection,
                                            &panes,
                                            new_size,
                                        );
                                        for pane in panes.iter_mut() {
                                            if let Err(e) = pane.resize(new_size.0, new_size.1) {
                                                push_error(&mut errors, format!("resize: {e}"));
                                            }
                                        }
                                    }
                                    InputVerdict::Dash(DashAction::ToggleSidebar) => {
                                        // Dash refresh PR1: flips whether the
                                        // narrow-terminal hide is overridden.
                                        // `sidebar_cols` itself is recomputed
                                        // (and every pane resized to match)
                                        // at the top of the next iteration --
                                        // see the loop's own reconciliation,
                                        // which fires on a sidebar-only
                                        // change exactly as it does on a real
                                        // terminal resize.
                                        sidebar_forced_visible = !sidebar_forced_visible;
                                    }
                                    InputVerdict::Dash(DashAction::ToggleTree) => {
                                        tree_view.chord_toggle()
                                    }
                                    InputVerdict::Dash(DashAction::Quit) => {
                                        let working: Vec<String> = panes
                                            .iter()
                                            .filter(|p| matches!(p.state(), PaneState::Working))
                                            .map(|p| p.title().to_string())
                                            .collect();
                                        if working.is_empty() {
                                            // Reached only with no overlay open (this
                                            // arm is the no-overlay branch), so there
                                            // is nothing unoffered to hand back --
                                            // `deferred_restore` (G3) aside, which is
                                            // still owed regardless of overlay state.
                                            on_quit(
                                                &panes,
                                                &[],
                                                &deferred_restore,
                                                &requests_dir,
                                                state,
                                                repo,
                                            );
                                            render_shutting_down(&mut terminal, panes.len());
                                            shutdown_all(&mut panes, cfg, &mut errors);
                                            break 'main 0;
                                        }
                                        overlay = ui::Overlay::QuitConfirm(working);
                                    }
                                    // Opens the corresponding overlay seam Task 4
                                    // defined. Spawn's own reducer is Task 10/11's;
                                    // Esc (handled in the overlay-active branch
                                    // above) closes it in the meantime.
                                    InputVerdict::Dash(DashAction::Spawn) => {
                                        overlay = ui::Overlay::Spawn(ui::SpawnDraft::default());
                                    }
                                    InputVerdict::Dash(DashAction::Nudge) => {
                                        // Only a row backed by a live pane can receive child input; view-only rows affect selection but not focus.
                                        let target = match session_target(
                                            chrome_selection.as_ref(),
                                            &rows,
                                            selected,
                                        ) {
                                            Some(short) if selected < panes.len() => {
                                                ui::NudgeTarget::AttachedPane(short)
                                            }
                                            Some(short) => ui::NudgeTarget::ViewOnlySession(short),
                                            None => ui::NudgeTarget::None,
                                        };
                                        overlay = ui::Overlay::Nudge(ui::NudgeDraft {
                                            target,
                                            input: String::new(),
                                        });
                                    }
                                    // Handover targets the focused pane whose grid and child are on screen (#84).
                                    InputVerdict::Dash(DashAction::Handover) => {
                                        match panes.get(focused).map(|p| p.short().to_string()) {
                                            Some(target_short) => {
                                                let mut items = Vec::new();
                                                for agent in adapters::available_adapter_names(cfg)
                                                {
                                                    for tier in handover::TIERS {
                                                        // Omit a picker tier when an adapter has neither that tier nor an override.
                                                        if let Ok(model) = handover::resolve_model(
                                                            agent, tier, cfg,
                                                        ) {
                                                            items.push((
                                                                agent.to_string(),
                                                                tier.to_string(),
                                                                model,
                                                            ));
                                                        }
                                                    }
                                                }
                                                overlay =
                                                    ui::Overlay::Handover(ui::HandoverDraft {
                                                        items,
                                                        cursor: 0,
                                                        offset: 0,
                                                        target_short,
                                                    });
                                            }
                                            None => push_error(
                                                &mut errors,
                                                "handover: no focused pane".to_string(),
                                            ),
                                        }
                                    }
                                    InputVerdict::Dash(DashAction::Mail) => {
                                        overlay = ui::Overlay::Mail(build_mail_view(state, repo));
                                    }
                                    InputVerdict::Dash(DashAction::Memory) => {
                                        overlay =
                                            ui::Overlay::Memory(build_memory_view(state, repo));
                                    }
                                    InputVerdict::Dash(DashAction::ShowErrors) => {
                                        overlay = ui::Overlay::Errors(build_errors_view(
                                            &errors,
                                            Instant::now(),
                                        ));
                                    }
                                    InputVerdict::Dash(DashAction::Approvals(approval_key)) => {
                                        let outcome = approvals_view::handle_key(
                                            approvals_hub.as_mut(),
                                            approval_key,
                                            state,
                                        );
                                        if let Some(text) = outcome.notice {
                                            push_notice(&mut notices, Instant::now(), text);
                                        }
                                        if let Some(next) = outcome.overlay {
                                            overlay = next;
                                        }
                                        if let Some(short) = outcome.goto {
                                            reveal_sidebar = true;
                                            chrome_selection = None;
                                            (selected, focused) =
                                                select_row(&short, &rows, selected, focused);
                                            // From the tree the pane opens as a chat, not as the dashboard.
                                            tree_view.open_chat();
                                        }
                                    }
                                    InputVerdict::Dash(DashAction::ShowJevErrors) => {
                                        overlay = ui::Overlay::JevErrors(build_jev_errors_view(
                                            &facts_cache.disk.jev,
                                        ));
                                    }
                                    // Help and palette share the same action table and snapshot row availability on opening (#354).
                                    InputVerdict::Dash(
                                        action @ (DashAction::Help | DashAction::Palette),
                                    ) => {
                                        let mode = if action == DashAction::Help {
                                            ui::PaletteMode::Help
                                        } else {
                                            ui::PaletteMode::Run
                                        };
                                        let ctx = selected_action_context(
                                            &rows,
                                            selected,
                                            &panes,
                                            &retained_ended,
                                            chrome_selection == Some(Hit::SidebarSummary),
                                        );
                                        overlay =
                                            ui::Overlay::Palette(build_palette_view(mode, ctx));
                                    }
                                }
                            }
                        }
                        Ok(Event::Resize(cols, term_h)) => {
                            input_errors = 0;
                            // Store the new terminal size for zoom and later fallback sizing.
                            sidebar_cols =
                                effective_sidebar_cols(cfg, cols, sidebar_forced_visible);
                            apply_terminal_resize(
                                cols,
                                term_h.saturating_sub(
                                    approvals_strip_h
                                        + tree_view.chat_rows((cols, term_h), approvals_strip_h)
                                        + tree_view
                                            .chat_bottom_rows((cols, term_h), approvals_strip_h),
                                ),
                                sidebar_cols,
                                zoomed || tree_view.in_chat(),
                                &mut term_cols,
                                &mut term_rows,
                                &mut full,
                                &mut panes,
                                &mut errors,
                                &mut selection,
                            );
                        }
                        // There is only one drawn grid, so wheel input always
                        // scrolls focus; sidebar position cannot name another grid.
                        // Mouse events arrive only while terminal reporting is on.
                        Ok(Event::Mouse(mouse)) => {
                            input_errors = 0;
                            // Map native overview clicks to the agent whose rendered row was hit (#490).
                            if let Some(native) = panes.get_mut(focused).and_then(Pane::native_mut)
                            {
                                match mouse.kind {
                                    MouseEventKind::Down(_) => {
                                        let main = effective_main(
                                            full,
                                            sidebar_cols,
                                            zoomed || tree_view.in_chat(),
                                        );
                                        native_pane::click_overview_row(
                                            native,
                                            main,
                                            mouse.column,
                                            mouse.row,
                                        );
                                        continue;
                                    }
                                    MouseEventKind::ScrollUp => {
                                        native_pane::wheel_scroll(native, WHEEL_STEP);
                                        continue;
                                    }
                                    MouseEventKind::ScrollDown => {
                                        native_pane::wheel_scroll(native, -WHEEL_STEP);
                                        continue;
                                    }
                                    _ => {}
                                }
                            }
                            let delta = match mouse.kind {
                                MouseEventKind::ScrollUp => WHEEL_STEP,
                                MouseEventKind::ScrollDown => -WHEEL_STEP,
                                _ => 0,
                            };
                            if delta != 0
                                && let Some(pane) = panes.get_mut(focused)
                            {
                                let alt = pane.alternate_screen();
                                let wants_mouse = pane.wants_mouse();
                                let before = pane.scrollback();
                                // Pane-local and 1-based: the child believes
                                // its own top-left is the terminal's, and the
                                // sidebar means `main.x` is genuinely not 0.
                                let main = effective_main(
                                    full,
                                    sidebar_cols,
                                    zoomed || tree_view.in_chat(),
                                );
                                let (col, row) = pane_local_mouse(main, mouse.column, mouse.row);
                                match pane.scroll_wheel(delta, col, row) {
                                    Ok(outcome) => {
                                        let after = pane.scrollback();
                                        // Translate an active selection by the wheel's scroll delta, including mid-drag (#697).
                                        translate_selection(
                                            &mut selection,
                                            pane.short(),
                                            after as i64 - before as i64,
                                        );
                                        if let Some(log) = keylog.as_mut() {
                                            log.scroll(
                                                "wheel",
                                                alt,
                                                wants_mouse,
                                                before,
                                                after,
                                                outcome,
                                            );
                                        }
                                        push_notice(
                                            &mut notices,
                                            Instant::now(),
                                            scroll_notice(outcome),
                                        );
                                    }
                                    Err(e) => push_error(&mut errors, format!("scroll: {e}")),
                                }
                            }
                            // Forward a child mouse click only when it lands inside that pane's grid.
                            let button = match mouse.kind {
                                MouseEventKind::Down(b) if b != MouseButton::Left => {
                                    Some((mouse_button_code(b), true))
                                }
                                MouseEventKind::Up(b) if b != MouseButton::Left => {
                                    Some((mouse_button_code(b), false))
                                }
                                _ => None,
                            };
                            if let Some((code, press)) = button {
                                let main = effective_main(
                                    full,
                                    sidebar_cols,
                                    zoomed || tree_view.in_chat(),
                                );
                                if main.contains(Position::new(mouse.column, mouse.row))
                                    && let Some(pane) = panes.get_mut(focused)
                                {
                                    let (col, row) =
                                        pane_local_mouse(main, mouse.column, mouse.row);
                                    if let Err(e) = pane.forward_mouse_button(code, press, col, row)
                                    {
                                        push_error(&mut errors, format!("mouse: {e}"));
                                    }
                                }
                            }

                            // Defer a press so a drag becomes dashboard selection while a plain click still reaches the child (#697).
                            match mouse.kind {
                                MouseEventKind::Down(MouseButton::Left) => {
                                    // Clear stale selection and pending press on every fresh left press.
                                    selection = None;
                                    pending_press = None;
                                    let main = effective_main(
                                        full,
                                        sidebar_cols,
                                        zoomed || tree_view.in_chat(),
                                    );
                                    if let Some(pane) = panes.get(focused)
                                        && press_starts_selection(main, mouse.column, mouse.row)
                                    {
                                        let (rows, cols) = pane.screen().size();
                                        if let Some(cell) = pane_local_cell(
                                            main,
                                            mouse.column,
                                            mouse.row,
                                            rows,
                                            cols,
                                        ) {
                                            pending_press = Some(PendingPress {
                                                pane_short: pane.short().to_string(),
                                                column: mouse.column,
                                                row: mouse.row,
                                                anchor_cell: cell,
                                            });
                                        }
                                    }
                                }
                                MouseEventKind::Drag(MouseButton::Left) => {
                                    let main = effective_main(
                                        full,
                                        sidebar_cols,
                                        zoomed || tree_view.in_chat(),
                                    );
                                    if selection.is_some() {
                                        // Already past the threshold: extend
                                        // the drag. Also covers the pointer
                                        // running past either edge of the
                                        // pane -- auto-scroll it one row in
                                        // that direction per drag event and
                                        // translate the selection the same
                                        // way an operator-driven scroll would
                                        // (`translate_selection`), so it
                                        // stays correct once scrolled back
                                        // into view.
                                        let pane_short =
                                            selection.as_ref().map(|s| s.pane_short.clone());
                                        if let Some(pane_short) = pane_short
                                            && let Some(pane) = panes.get_mut(focused)
                                            && pane.short() == pane_short
                                        {
                                            let before = pane.scrollback();
                                            if let Some(dir) =
                                                drag_autoscroll_direction(main, mouse.row)
                                            {
                                                pane.scroll_by(dir);
                                            }
                                            let after = pane.scrollback();
                                            translate_selection(
                                                &mut selection,
                                                &pane_short,
                                                after as i64 - before as i64,
                                            );
                                            let (rows, cols) = pane.screen().size();
                                            if let Some(cell) = pane_local_cell(
                                                main,
                                                mouse.column,
                                                mouse.row,
                                                rows,
                                                cols,
                                            ) && let Some(sel) = selection.as_mut()
                                            {
                                                sel.end = (i64::from(cell.0), cell.1);
                                            }
                                        }
                                    } else if let Some(pending) = pending_press.take() {
                                        let matches_focus = panes
                                            .get(focused)
                                            .is_some_and(|pane| pane.short() == pending.pane_short);
                                        if matches_focus
                                            && past_drag_threshold(
                                                pending.column,
                                                pending.row,
                                                mouse.column,
                                                mouse.row,
                                            )
                                        {
                                            if let Some(pane) = panes.get(focused) {
                                                let (rows, cols) = pane.screen().size();
                                                let end_cell = pane_local_cell(
                                                    main,
                                                    mouse.column,
                                                    mouse.row,
                                                    rows,
                                                    cols,
                                                )
                                                .unwrap_or(pending.anchor_cell);
                                                selection =
                                                    Some(promote_pending_drag(pending, end_cell));
                                            }
                                            // else: the focused pane vanished
                                            // between the press and this
                                            // drag -- drop it silently, the
                                            // same "cannot use stale
                                            // coordinates" rule every other
                                            // cancel in this module follows.
                                        } else if matches_focus {
                                            // Still within the threshold:
                                            // keep waiting.
                                            pending_press = Some(pending);
                                        }
                                        // else: focus changed since the press;
                                        // drop it.
                                    }
                                }
                                MouseEventKind::Up(MouseButton::Left) => {
                                    if let Some(pending) = pending_press.take() {
                                        // Replay an unmoved press and release to the same child as a click.
                                        if let Some(pane) = panes.get_mut(focused)
                                            && pane.short() == pending.pane_short
                                        {
                                            let main = effective_main(
                                                full,
                                                sidebar_cols,
                                                zoomed || tree_view.in_chat(),
                                            );
                                            let code = mouse_button_code(MouseButton::Left);
                                            let (press_coords, release_coords) =
                                                deferred_click_coords(
                                                    &pending,
                                                    main,
                                                    mouse.column,
                                                    mouse.row,
                                                );
                                            if let Err(e) = pane.forward_mouse_button(
                                                code,
                                                true,
                                                press_coords.0,
                                                press_coords.1,
                                            ) {
                                                push_error(&mut errors, format!("mouse: {e}"));
                                            }
                                            if let Err(e) = pane.forward_mouse_button(
                                                code,
                                                false,
                                                release_coords.0,
                                                release_coords.1,
                                            ) {
                                                push_error(&mut errors, format!("mouse: {e}"));
                                            }
                                        }
                                    } else if let Some(sel) = selection.take()
                                        && let Some(pane) = panes.get(focused)
                                        && pane.short() == sel.pane_short
                                    {
                                        // Drop a selection if focus changed before its release, since the selected pane no longer owns the gesture.
                                        let (kept, copy) = selection_on_release(sel);
                                        if copy && let Some(s) = kept.as_ref() {
                                            let (rows, cols) = pane.screen().size();
                                            let (start, end) =
                                                resolve_selection_range(s, rows, cols);
                                            let text = pane
                                                .screen()
                                                .contents_between(start.0, start.1, end.0, end.1);
                                            let text = trim_trailing_whitespace_per_line(&text);
                                            copy_selection(text, &clipboard_tx);
                                        }
                                        selection = kept;
                                    }
                                }
                                _ => {}
                            }
                        }
                        // Insert native bracketed paste as one composer operation, preserving multiline text (#490).
                        Ok(Event::Paste(text)) => {
                            input_errors = 0;
                            if let Some(native) = panes.get_mut(focused).and_then(Pane::native_mut)
                            {
                                native.handle_composer_action(
                                    native_pane::ComposerAction::InsertText(text),
                                );
                            }
                        }
                        Ok(_) => input_errors = 0,
                        Err(e) => {
                            input_errors = input_errors.saturating_add(1);
                            push_error(&mut errors, format!("event read: {e}"));
                        }
                    }
                }
                Ok(false) => {
                    // No event ready within the wait: the queue is drained (or
                    // was empty). Stop the drain and go do the tick's work.
                    input_errors = 0;
                    break;
                }
                Err(e) => {
                    input_errors = input_errors.saturating_add(1);
                    push_error(&mut errors, format!("event poll: {e}"));
                    break;
                }
            }
            drained += 1;
        }

        let mut hover_changed = false;
        if let Some(mouse) = pending_move.take() {
            let tree_facts = build_tree_facts(
                &facts_cache,
                approvals_hub.as_ref(),
                &panes,
                focused,
                cfg,
                &rows,
                &retained_ended,
                &kept_requests,
                repo,
            );
            let before = tree_view.hover_signature();
            if tree_view.in_chat() {
                let _ = tree_view.chat_mouse(mouse, &tree_facts);
            } else {
                let page = tree_view::Surface::page(Rect::new(0, 0, term_cols, term_rows));
                let _ = tree_view.mouse(mouse, page, &tree_facts, Instant::now());
            }
            hover_changed = tree_view.hover_signature() != before;
        }
        let want_hover = cfg.dash.mouse && tree_view.wants_hover(tick_term);
        if want_hover != hover_on {
            hover_on = want_hover;
            let mut stdout = io::stdout();
            let _ = stdout
                .write_all(term::dash_hover_bytes(hover_on))
                .and_then(|()| stdout.flush());
        }

        // After persistent console-read errors, quit through normal roster and terminal cleanup instead of spinning.
        if input_stream_is_dead(input_errors) {
            push_error(
                &mut errors,
                "dashboard: the input stream stopped answering; quitting".to_string(),
            );
            // Return unanswered restore candidates to the next roster on exit.
            on_quit(
                &panes,
                unoffered_candidates(&overlay, &restore_candidates),
                &deferred_restore,
                &requests_dir,
                state,
                repo,
            );
            render_shutting_down(&mut terminal, panes.len());
            shutdown_all(&mut panes, cfg, &mut errors);
            break 0;
        }

        let term_size = crossterm::terminal::size().unwrap_or((term_cols, term_rows));
        let full_term = term_size;
        // The strip's rows belong to the inbox, not to the panes: every geometry read below sees the shorter terminal.
        let approvals_strip_h = if tree_view.hides_approvals_strip(full_term) {
            0
        } else {
            approvals_view::current_strip_rows(approvals_hub.as_ref())
        };
        // An open chat keeps the dashboard's header above its pane and the others strip below it;
        // its pane is the zoomed single pane between them.
        let chat_top = tree_view.chat_rows(full_term, approvals_strip_h);
        let chat_bottom = tree_view.chat_bottom_rows(full_term, approvals_strip_h);
        let zoomed_now = zoomed || chat_top > 0;
        let term_size = (
            term_size.0,
            term_size
                .1
                .saturating_sub(approvals_strip_h + chat_top + chat_bottom),
        );
        // Compute sidebar width from this frame's terminal size.
        let next_sidebar_cols = effective_sidebar_cols(cfg, term_size.0, sidebar_forced_visible);
        // Reconcile terminal size every frame because resize events may be coalesced or missed.
        if term_size != (term_cols, term_rows) || next_sidebar_cols != sidebar_cols {
            sidebar_cols = next_sidebar_cols;
            apply_terminal_resize(
                term_size.0,
                term_size.1,
                sidebar_cols,
                zoomed_now,
                &mut term_cols,
                &mut term_rows,
                &mut full,
                &mut panes,
                &mut errors,
                &mut selection,
            );
        } else {
            sidebar_cols = next_sidebar_cols;
        }
        // Mouse mapping reads `full`: below the chat bar when one is open.
        full.y = chat_top;
        let frame_area = Rect::new(0, chat_top, term_size.0, term_size.1);
        let layout = ui::layout(frame_area, sidebar_cols);
        // Draw the grid and overlay in the effective main rect so zoomed PTY and display geometry agree.
        let main_area = effective_main(frame_area, sidebar_cols, zoomed_now);

        // Rebuild sidebar rows after input changes selection so its highlight is current.
        let rows = assemble_sidebar(
            &build_pane_rows(&panes, &retained_ended),
            &visible_registry,
            &facts_cache.disk.scores,
            selected,
            focused,
            std::process::id(),
            super::state::now_secs(),
        );
        let mut rows = rows;
        enrich_sidebar(&mut rows, &facts_cache.disk, super::state::now_secs());
        if let Some(hub) = approvals_hub.as_ref() {
            let waiting = hub.shorts();
            for row in rows.iter_mut() {
                row.approval_pending = waiting.contains(row.short.as_str());
            }
        }
        // Dash refresh PR2: clock-driven, not per-drawn-frame -- see this
        // variable's own doc comment above the loop.
        render_tick = if motion.is_full() {
            (dash_start.elapsed().as_millis() / 80) as usize
        } else {
            0
        };
        // Share one cached seat rollover state across footer and sidebar in this frame.
        let seat_headroom_for_current = seat_headroom_for_current(
            seat_headroom_pct.as_ref(),
            facts_cache.disk.seat_full.as_ref(),
        );
        let rollover_state_now = rollover_state(
            cfg,
            facts_cache.disk.seat_full.as_ref(),
            facts_cache.disk.rollover_record.as_ref(),
            seat_headroom_for_current,
        );
        if let Some(state) = &rollover_state_now
            && let Some(seat) = facts_cache.disk.seat_full.as_ref()
            && let Some(row) = rows.iter_mut().find(|r| r.short == seat.short)
        {
            row.rollover_badge = rollover_badge_of(state);
        }
        for row in rows.iter_mut() {
            row.flash = flash_started
                .get(&row.short)
                .map(|started| ui::row_flash_style(started.elapsed().as_millis() as u64, motion))
                .unwrap_or(None);
        }
        flash_started.retain(|_, started| started.elapsed().as_millis() < 900);
        // Dash refresh PR2: the rot track's own eased fill -- lags the
        // focused row's real score by up to ~300ms (`ease_toward`); reset to
        // the raw score outright whenever nothing was there to ease FROM
        // (no focused row last tick, or it had no score) rather than easing
        // from a stale, unrelated pane's reading.
        let ease_now = Instant::now();
        let ease_dt_ms = ease_now.duration_since(last_ease_tick).as_millis() as u64;
        last_ease_tick = ease_now;
        let focused_score_now = rows
            .iter()
            .find(|r| r.focused)
            .and_then(|r| r.score)
            .map(|s| s as f64);
        eased_rot_score = focused_score_now
            .map(|target| ease_toward_score(eased_rot_score, target, ease_dt_ms, motion));

        // Show transient notices before sticky errors, then reveal errors after notices expire.
        let total_live = rows
            .iter()
            .filter(|r| r.state != ui::RowState::Dead)
            .count();
        // Derive header counts from the same live session set to keep subsets consistent.
        let header_working = rows
            .iter()
            .filter(|r| r.state == ui::RowState::Working)
            .count();
        let header_needs_you = rows
            .iter()
            .filter(|r| ui::glyph_for(r) == ui::Glyph::NeedsAction)
            .count();
        let mut facts = assemble_header_facts(
            total_live,
            header_working,
            header_needs_you,
            errors.sticky_count(),
            errors.sticky_line(),
            live_notice(&notices, Instant::now()).map(str::to_string),
        );
        facts.approvals = approvals_hub
            .as_ref()
            .map_or(0, super::approvals::Hub::count);
        // Build the focused pane's footer from this tick's cached sidebar row (#209).
        facts.tip = first_run_tip.then(|| ui::FIRST_RUN_TIP.as_str());
        facts.hints.alive = rows
            .get(selected)
            .is_some_and(|r| r.state != ui::RowState::Dead);
        // Choose action hints from the selected row's actual glyph and availability (#354).
        facts.hints.needs_action =
            rows.get(selected).map(ui::glyph_for) == Some(ui::Glyph::NeedsAction);
        facts.hints.ended = rows
            .get(selected)
            .is_some_and(|r| r.state == ui::RowState::Dead);
        // Draw restore only when the row retains a relaunchable request (#354).
        facts.hints.restorable = facts.hints.ended
            && rows.get(selected).is_some_and(|r| {
                retained_ended
                    .iter()
                    .any(|e| e.short == r.short && e.request.is_some())
            });
        // Offer dashboard inspection when the summary line is selected (#354).
        facts.hints.summary = chrome_selection == Some(Hit::SidebarSummary);
        let focused_row = rows.iter().find(|r| r.focused);
        // Read the focused pane's own mail count, not the launch pane's count.
        let focused_mail =
            focused_row.and_then(|row| facts_cache.disk.mail_by_session.get(&row.short).copied());
        // Read the focused pane's own stall latch (#310).
        let focused_stalled =
            focused_row.is_some_and(|row| facts_cache.disk.stalled.contains(&row.short));
        // Show rollover distance only while the orchestrator seat is focused.
        let footer_rollover = panes
            .get(focused)
            .filter(|p| p.role() == prompt::PromptRole::Orchestrator)
            .and(rollover_state_now.as_ref())
            .map(rollover_footer_fact_of);
        let footer_facts = assemble_footer_facts(
            focused_row,
            focused_mail,
            facts_cache.disk.workflow.as_ref(),
            last_exited.as_ref().map(|info| {
                (
                    info.harness.as_str(),
                    Some(
                        Instant::now()
                            .saturating_duration_since(info.exited_at)
                            .as_secs(),
                    ),
                )
            }),
            focused_stalled,
            eased_rot_score,
            footer_rollover,
        );

        let bands = (cfg.score.advise_at, cfg.score.compact_at);
        // Repin sidebar viewport to selection only when keyboard navigation moves it; wheel scrolling is independent (#354).
        if reveal_sidebar
            && chrome_selection.is_none()
            && let Some(group) = rows.get(selected).and_then(|row| row.group.as_ref())
        {
            collapsed_groups.remove(&group.id);
        }
        let mut view = ui::RosterView {
            collapsed: &collapsed_groups,
            chrome_selection: chrome_selection.as_ref(),
            offset: sidebar_offset,
            tick: render_tick,
            bands,
        };
        let mut roster = ui::roster_frame(layout.sidebar, &rows, &view);
        // Use tree row IDs for sidebar viewport positions because group headers and collapsed groups alter visible rows.
        let capacity = layout.sidebar.height as usize;
        let reveal_index = reveal_sidebar
            .then(|| {
                chrome_selection.clone().or_else(|| {
                    rows.get(selected)
                        .map(|row| Hit::SidebarRow(row.short.clone()))
                })
            })
            .flatten()
            .and_then(|target| roster.row_ids.iter().position(|id| *id == target));
        reveal_sidebar = false;
        // With no reveal target this is just the end-of-list clamp, so a
        // roster that shrank under a scrolled viewport snaps back on its own.
        let next_offset = ui::reveal_offset(
            roster.row_ids.len(),
            capacity,
            reveal_index.unwrap_or(sidebar_offset),
            sidebar_offset,
        );
        if next_offset != sidebar_offset {
            sidebar_offset = next_offset;
            view.offset = sidebar_offset;
            roster = ui::roster_frame(layout.sidebar, &rows, &view);
        }
        // Click affordance follow-up: mutated once more below, after the JEV
        // section's own geometry is worked out (`jev_area`), to add the
        // errors line's own hit region -- both live in `next_snapshot`
        // regardless, so this is still the one `FrameSnapshot` the frame
        // ends up drawn from and hit-tested against.
        let mut next_snapshot = ui::frame_snapshot(
            frame_area,
            &layout,
            zoomed_now,
            &roster,
            &facts,
            &overlay,
            render_tick,
        );
        // Captured in the same breath as `next_snapshot` itself, from the
        // same `&overlay` it was built from -- see `overlay_route_is_current`.
        let next_snapshot_overlay_ident = overlay_identity(&overlay);
        let focus_cwd = panes.get(focused).map(|p| p.cwd().display().to_string());
        // Dash refresh PR2: drop the toast once it has fully faded (5s) --
        // harmless to keep, but there is no reason to.
        if toast
            .as_ref()
            .is_some_and(|(_, started)| started.elapsed().as_millis() >= 5_000)
        {
            toast = None;
        }
        let pane_header_toast = toast.as_ref().and_then(|(text, started)| {
            ui::toast_style(started.elapsed().as_millis() as u64, motion)
                .map(|style| (text.clone(), style))
        });
        // Draw focused pane identity and workflow in its pane header; draw nothing without focus.
        let pane_header_facts = focused_row.map(|row| ui::PaneHeaderFacts {
            name: row.name.clone(),
            harness: row.harness.clone(),
            role: row.role.clone(),
            model: row.model.clone(),
            cwd: focus_cwd
                .as_deref()
                .map(|cwd| shorten_home(cwd, home_dir_display.as_deref()))
                .unwrap_or_else(|| style::PLACEHOLDER.into()),
            workflow: row.workflow.clone(),
            glyph: ui::glyph_for(row),
            state_word: row.fact_state.clone(),
            age_secs: row.fact_since_secs,
            toast: pane_header_toast,
        });
        // Dash refresh PR1: the LIMITS block, pinned to the bottom of the
        // session column -- session rows win the space (`roster.lines` is
        // never shortened for it), so this only ever draws into whatever
        // `layout.sidebar` the roster left blank, dropping whole windows
        // from the bottom (never a half block) when even that is not
        // enough room. `disk.usage` is already filtered to enabled
        // harnesses (`FactsCache::refresh_if_due`'s own `cfg.agents.
        // is_enabled` gate), so a disabled harness never reaches here.
        // Coordinator follow-up: LIMITS/JEV bars ease the same way the rot
        // track does -- `touched_bar_keys` is every key either one used
        // this frame, so a harness/site that stopped showing drops out of
        // `eased_bars` instead of lingering to "ease" whatever later reuses
        // its key.
        let mut touched_bar_keys: HashSet<String> = HashSet::new();
        let mut limits_blocks = ui::limits_blocks_from_usage(&facts_cache.disk.usage);
        for block in &mut limits_blocks {
            let key = format!("limits:{}:{}", block.harness, block.window_label);
            block.eased_pct = ease_bar(&mut eased_bars, &key, block.pct, ease_dt_ms, motion);
            touched_bar_keys.insert(key);
        }
        let limits_available_rows = layout
            .sidebar
            .height
            .saturating_sub(roster.drawn_rows() as u16);
        let limits_shown = ui::limits_blocks_fitting(limits_blocks.len(), limits_available_rows);
        let limits_height = ui::limits_rows_for(limits_shown);
        let limits_area = Rect {
            y: layout.sidebar.y + layout.sidebar.height - limits_height,
            height: limits_height,
            ..layout.sidebar
        };
        // Preserve vertical priority for sessions and limits before JEV by giving limits its own reserved space.
        let jev_available_rows = layout
            .sidebar
            .height
            .saturating_sub(roster.drawn_rows() as u16)
            .saturating_sub(limits_height);
        let (jev_shown_sites, jev_height) = match &facts_cache.disk.jev {
            Some(fact) => {
                let jev_fixed_rows: u16 = match fact {
                    ui::JevSectionFact::NoKey { .. } => 2 + 1,
                    ui::JevSectionFact::Active { .. } => 2 + 4,
                };
                if jev_available_rows < jev_fixed_rows {
                    (0, 0)
                } else {
                    let site_count = match fact {
                        ui::JevSectionFact::NoKey { .. } => 0,
                        ui::JevSectionFact::Active { sites, .. } => sites.len(),
                    };
                    let shown =
                        ui::jev_sites_fitting(site_count, jev_available_rows - jev_fixed_rows);
                    (shown, ui::jev_rows_for(fact, shown))
                }
            }
            None => (0, 0),
        };
        let jev_area = Rect {
            y: layout.sidebar.y + roster.drawn_rows() as u16,
            height: jev_height,
            ..layout.sidebar
        };
        // Dash refresh PR2: the JEV `last` line flashes on a NEW call
        // landing -- edge-triggered the same way a sidebar row's mail flash
        // is (`jev_last_flash_started`, set where the 10s Jev refresh runs).
        let jev_last_flash = jev_last_flash_started
            .map(|started| ui::row_flash_style(started.elapsed().as_millis() as u64, motion))
            .unwrap_or(None);
        // Coordinator follow-up: the JEV section's own site bars ease too --
        // a rendering-only clone (`facts_cache.disk.jev` itself stays the
        // raw target from the last 10s refresh; see `eased_jev_fact`'s own
        // doc comment for why).
        let jev_eased_fact = facts_cache.disk.jev.as_ref().map(|fact| {
            eased_jev_fact(
                fact,
                &mut eased_bars,
                ease_dt_ms,
                motion,
                &mut touched_bar_keys,
            )
        });
        // Click affordance follow-up: the errors line's own hit region --
        // only when the section actually fit on screen (`jev_height > 0`;
        // otherwise `jev_area` names rows that were never drawn) and only
        // when `jev_errors_hit_rect` says the line means something (`Active`
        // with `errors > 0`; see its own doc comment for the zero-errors
        // and `NoKey` cases, which add no hit region at all).
        if jev_height > 0
            && let Some(fact) = &jev_eased_fact
            && let Some(rect) = ui::jev_errors_hit_rect(jev_area, fact)
        {
            next_snapshot.rows.push((rect, Hit::JevErrors));
        }
        eased_bars.retain(|key, _| touched_bar_keys.contains(key));
        // Dash refresh PR1: below the narrow-terminal floor `layout.sidebar`
        // is 0-wide (`sidebar_cols` is 0), and the two rules must draw a
        // plain line with no `┬`/`┼`/`┴` junction at all -- there is no
        // divider column to meet. `u16::MAX` never satisfies either rule's
        // own `divider_col < area.width` check, so it degrades to a bare
        // rule exactly like a frame with no separator column already does.
        let sidebar_hidden_now = layout.sidebar.width == 0;
        // Read local wall clock once for all LIMITS reset times in this frame.
        let local_offset = *chrono::Local::now().offset();
        let rule_divider_col = if sidebar_hidden_now {
            u16::MAX
        } else {
            layout.sidebar.width
        };
        let mut native_approval_rendered = false;
        let tree_facts = tree_view.is_visible().then(|| {
            build_tree_facts(
                &facts_cache,
                approvals_hub.as_ref(),
                &panes,
                focused,
                cfg,
                &rows,
                &retained_ended,
                &kept_requests,
                repo,
            )
        });
        let strip_facts = (approvals_strip_h > 0)
            .then(|| {
                approvals_hub
                    .as_ref()
                    .and_then(|hub| approvals_view::strip_facts(hub, &rows, frame_area.width))
            })
            .flatten();
        if approvals_strip_h == 0
            && let Some(hub) = approvals_hub.as_ref()
        {
            hub.mark_drawn(None);
        }
        // The orchestrator dashboard animates, so it draws about 30 times a second while it shows
        // and straight away after input; the classic dashboard's tick is untouched.
        let skip_draw = tree_view.frame_interval().is_some_and(|every| {
            drained == moved_only
                && !hover_changed
                && matches!(overlay, ui::Overlay::None)
                && last_tree_draw.is_some_and(|at| at.elapsed() < every)
        });
        if let Some(tree_facts) = &tree_facts {
            if !skip_draw {
                tree_view.observe(tree_facts, Instant::now());
                last_tree_draw = Some(Instant::now());
            }
        } else {
            last_tree_draw = None;
        }
        let draw = (!skip_draw).then(|| {
            terminal.draw(|f| {
                if let Some(tree_facts) = &tree_facts {
                    let area = if tree_view.in_chat() {
                        Rect::new(
                            0,
                            0,
                            frame_area.width,
                            chat_top + frame_area.height + chat_bottom,
                        )
                    } else {
                        frame_area
                    };
                    tree_view::render(f, area, &tree_view, tree_facts);
                }
                if !zoomed_now && tree_facts.is_none() {
                    if sidebar_hidden_now {
                        ui::render_header_tabs(f, layout.header, &facts, &rows, render_tick);
                    } else {
                        ui::render_header(f, layout.header, &facts);
                    }
                    ui::render_rule(f, layout.rule_top, rule_divider_col, true);
                    if !sidebar_hidden_now {
                        ui::render_sidebar_title(f, layout.sidebar_title, rows.len());
                    }
                    if let Some(pane_header_facts) = &pane_header_facts {
                        ui::render_pane_header(
                            f,
                            layout.pane_header,
                            pane_header_facts,
                            render_tick,
                            dash_start.elapsed().as_millis() as u64,
                            motion,
                        );
                    }
                    ui::render_mid_rule(f, layout.mid_rule, rule_divider_col);
                    if !sidebar_hidden_now {
                        ui::render_roster(f, layout.sidebar, &roster);
                        if jev_height > 0
                            && let Some(fact) = &jev_eased_fact
                        {
                            ui::render_jev(f, jev_area, fact, jev_shown_sites, jev_last_flash);
                        }
                        if limits_height > 0 {
                            ui::render_limits(
                                f,
                                limits_area,
                                &limits_blocks[..limits_shown],
                                super::state::now_secs(),
                                local_offset,
                            );
                        }
                        // Straight from the snapshot the click will be tested
                        // against, so the drawn divider and `Hit::Divider` can
                        // never describe different columns.
                        ui::render_sidebar_divider(
                            f,
                            Rect {
                                y: layout.sidebar_title.y,
                                height: 1,
                                ..next_snapshot.divider
                            },
                        );
                        ui::render_sidebar_divider(f, next_snapshot.divider);
                    }
                    ui::render_rule(f, layout.rule_bottom, rule_divider_col, false);
                    if sidebar_hidden_now {
                        let focused_usage = focused_row.and_then(|row| {
                            facts_cache
                                .disk
                                .usage
                                .iter()
                                .find(|u| u.name == row.harness)
                        });
                        ui::render_footer_narrow_usage(
                            f,
                            layout.footer,
                            focused_usage,
                            super::state::now_secs(),
                            local_offset,
                        );
                    } else {
                        ui::render_footer(
                            f,
                            layout.footer,
                            &footer_facts,
                            cfg.score.advise_at,
                            cfg.score.compact_at,
                            cfg.score.restart_at,
                            super::state::now_secs(),
                            local_offset,
                            dash_start.elapsed().as_millis() as u64,
                            motion,
                        );
                    }
                }
                if let Some(pane) = panes
                    .get(focused)
                    .filter(|_| tree_facts.is_none() || tree_view.in_chat())
                {
                    // A selection only ever names the pane it started on
                    // (`Selection::pane_short`); a focus change since then simply
                    // stops it from rendering here rather than needing an
                    // explicit clear anywhere else.
                    let selection_range = selection
                        .as_ref()
                        .filter(|sel| sel.pane_short == pane.short())
                        .map(|sel| {
                            let (rows, cols) = pane.screen().size();
                            resolve_selection_range(sel, rows, cols)
                        });
                    // Draw native conversation inside the dashboard's existing chrome (#490).
                    if let Some(native) = pane.native() {
                        let facts = native.status_facts();
                        let (view, presentation) = native.view();
                        // The whole native frame INSIDE the dashboard's main area:
                        // the conversation, the agent/task overview beside it, the
                        // usage/health provenance strip beneath it, and whichever
                        // modal is open. Which of those exist at all is
                        // `native_ux::resolve_layout`'s decision against the area
                        // it is actually given, so the same code draws every
                        // terminal size with no size-specific branch here.
                        native_approval_rendered = native_pane::render_native_dashboard(
                            f,
                            main_area,
                            view,
                            presentation,
                            &facts,
                            native.ux(),
                        );
                    } else {
                        ui::render_grid(f, main_area, pane.screen(), selection_range);
                        // Draw a scrollback notice above the grid but below overlays.
                        ui::render_scroll_marker(f, main_area, pane.scrollback());
                        // HIGH-1: the focused pane's own caret. ratatui hides the
                        // cursor on every frame whose `cursor_position` is left
                        // unset, so without this there is no caret anywhere for the
                        // whole session. An overlay is drawn on top below, but the
                        // caret is only set for the bare grid: an open dialog owns
                        // the screen.
                        //
                        // Suppressed while scrolled back as well: `cursor_position`
                        // is the *live* cursor and knows nothing about the
                        // scrollback offset, so a caret drawn from it would land on
                        // an unrelated row of history. tmux hides the cursor in
                        // copy mode for the same reason.
                        if matches!(overlay, ui::Overlay::None)
                            && pane.scrollback() == 0
                            && let Some(pos) = ui::grid_cursor_position(main_area, pane.screen())
                        {
                            f.set_cursor_position(pos);
                        }
                    }
                }
                ui::render_overlay(f, main_area, &overlay, render_tick);
                if let Some(strip) = &strip_facts {
                    let below = Rect::new(
                        0,
                        frame_area.y + frame_area.height + chat_bottom,
                        frame_area.width,
                        approvals_strip_h,
                    );
                    approvals_view::render_strip(f, below, strip);
                }
            })
        });
        if let Some(Err(e)) = draw {
            push_error(&mut errors, format!("draw: {e}"));
        } else if draw.is_some() {
            if native_approval_rendered
                && matches!(overlay, ui::Overlay::None)
                && let Some(native) = panes.get_mut(focused).and_then(Pane::native_mut)
            {
                native.ux_mut().mark_approval_visible();
            }
            frame_snapshot = next_snapshot;
            frame_snapshot_overlay_ident = next_snapshot_overlay_ident;
            // Acknowledge unread output only after a completed frame visibly showed the focused pane without an overlay (#354).
            let focused_pane = panes
                .get(focused)
                .map(|pane| (pane.short(), pane.scrollback()));
            done_unread_ack.observe(ack_candidate(
                !matches!(overlay, ui::Overlay::None)
                    || (tree_facts.is_some() && !tree_view.in_chat()),
                focused_pane,
                focused_pane.and_then(|(short, _)| facts_cache.disk.attention.get(short)),
            ));
        }
    };

    teardown_terminal(keyboard_enhancement_pushed);
    restore_panic_hook(&previous_panic_hook);
    // Print teardown messages after leaving the alternate screen so they remain in shell scrollback.
    if all_panes_ended {
        // Print retained errors into shell scrollback after the alternate screen closes so shutdown failures remain visible.
        for notice in errors.messages() {
            eprintln!("{notice}");
        }
        eprintln!("all sessions ended; dashboard closed");
    }
    Ok(exit_code)
}

/// The dashboard facts the agent tree reads, from what the event loop already holds.
#[allow(clippy::too_many_arguments)]
fn build_tree_facts<'a>(
    facts_cache: &'a FactsCache,
    approvals_hub: Option<&super::approvals::Hub>,
    panes: &[Pane],
    focused: usize,
    cfg: &CtxConfig,
    rows: &[ui::SidebarRow],
    retained: &VecDeque<EndedRow>,
    kept: &HashMap<String, (spawnreq::SpawnRequest, Option<String>)>,
    repo: &Path,
) -> tree_view::TreeFacts<'a> {
    let seat = facts_cache.disk.seat_full.as_ref();
    let pane_shorts: Vec<String> = panes.iter().map(|p| p.short().to_string()).collect();
    let attention_since = |short: &str| {
        facts_cache
            .disk
            .attention
            .get(short)
            .map_or(0, |status| status.last_transition)
    };
    let mut waits: Vec<tree_view::WaitFact> = facts_cache
        .disk
        .attention
        .iter()
        .filter(|(short, _)| pane_shorts.contains(short))
        .filter_map(|(short, status)| {
            use super::attention::Attention;
            let kind = match status.attention {
                Attention::Question => tree_view::WaitKind::Question,
                Attention::Permission => tree_view::WaitKind::Permission,
                Attention::Approval => tree_view::WaitKind::Approval,
                Attention::WorkflowGate => tree_view::WaitKind::WorkflowGate,
                _ => return None,
            };
            Some(tree_view::WaitFact {
                short: short.clone(),
                kind,
                since: status.last_transition,
                evidence: tree_view::wait_evidence(&status.evidence),
            })
        })
        .collect();
    waits.sort_by(|a, b| a.short.cmp(&b.short));
    let mut stalled: Vec<(String, u64)> = facts_cache
        .disk
        .stalled
        .iter()
        .filter(|short| pane_shorts.contains(short))
        .map(|short| (short.clone(), attention_since(short)))
        .collect();
    stalled.sort();
    let retryable = rows
        .iter()
        .filter(|row| {
            let ctx = menu_facts_for(row, panes, retained).action_context();
            actions::menu_actions(&ctx)
                .iter()
                .any(|(action, availability)| {
                    *action == ui::MenuAction::Retry
                        && matches!(availability, actions::Availability::Enabled)
                })
        })
        .map(|row| row.short.clone())
        .collect();
    tree_view::TreeFacts {
        repo_name: repo
            .file_name()
            .map_or_else(String::new, |n| n.to_string_lossy().into_owned()),
        usage_5h: seat.and_then(|s| {
            facts_cache
                .disk
                .usage
                .iter()
                .find(|u| u.name == s.agent)
                .and_then(|u| u.five_hour)
        }),
        approval_items: approvals_hub.map_or_else(Vec::new, |hub| {
            hub.items()
                .iter()
                .map(|item| tree_view::ApprovalFact {
                    short: item.request.short.clone(),
                    conn: item.conn,
                    tool: item.request.tool.clone(),
                    preview: item.request.preview.clone(),
                    waited_secs: item.since.elapsed().as_secs(),
                    fully_shown: item.request.fully_shown,
                    released: item.request.released,
                    view: tree_view::approval_view(&item.request),
                })
                .collect()
        }),
        waits,
        stalled,
        retryable,
        pane_meta: panes
            .iter()
            .enumerate()
            .map(|(index, pane)| tree_view::PaneMeta {
                short: pane.short().to_string(),
                number: index + 1,
                worktree: pane
                    .cwd()
                    .file_name()
                    .map_or_else(String::new, |n| n.to_string_lossy().into_owned()),
                brief: kept
                    .get(pane.short())
                    .map(|(request, _)| tree_view::capped_first_line(&request.prompt, 80))
                    .unwrap_or_default(),
            })
            .collect(),
        term_size: crossterm::terminal::size().unwrap_or((0, 0)),
        strip_rows: approvals_view::current_strip_rows(approvals_hub),
        seat_harness: seat.map(|s| s.agent.as_str()),
        seat_model: seat.and_then(|s| s.model.as_deref()),
        seat_role: seat.map(|s| s.role.as_str()),
        seat_session: seat.map(|s| s.session.as_str()),
        rot: seat.and_then(|s| facts_cache.disk.scores.get(&s.short).copied()),
        jev: facts_cache.disk.jev.as_ref(),
        workflow: facts_cache.disk.workflow.as_ref(),
        approvals: approvals_hub.map_or(0, super::approvals::Hub::count),
        approvals_inbox: approvals_hub.is_some(),
        approval_shorts: approvals_hub.map_or_else(Vec::new, |hub| {
            hub.shorts().into_iter().map(str::to_string).collect()
        }),
        pane_shorts,
        focused: panes.get(focused).map(|p| {
            (
                p.short().to_string(),
                p.title().to_string(),
                p.agent().to_string(),
            )
        }),
        panes_used: panes.len(),
        max_panes: cfg.dash.max_panes,
        max_writers: cfg.supervise.max_writers,
        now: super::state::now_secs(),
        utc_offset: *chrono::Local::now().offset(),
    }
}

/// The approval key a decision is answered with.
fn approval_key(decision: super::approvals::Decision) -> input::ApprovalKey {
    use super::approvals::Decision;
    match decision {
        Decision::Allow => input::ApprovalKey::Allow,
        Decision::AllowAlways => input::ApprovalKey::AllowAlways,
        Decision::Deny | Decision::Release => input::ApprovalKey::Deny,
    }
}

/// The dashboard state a tree action can change.
struct TreeDash<'a> {
    selected: &'a mut usize,
    focused: &'a mut usize,
    chrome_selection: &'a mut Option<Hit>,
    reveal_sidebar: &'a mut bool,
    overlay: &'a mut ui::Overlay,
    notices: &'a mut Vec<Notice>,
    errors: &'a mut ErrorLog,
    panes: &'a mut Vec<Pane>,
    nudge_queues: &'a mut Vec<VecDeque<String>>,
    retained: &'a mut VecDeque<EndedRow>,
    kept: &'a mut HashMap<String, (spawnreq::SpawnRequest, Option<String>)>,
    cfg: &'a CtxConfig,
    pane_size: (u16, u16),
    requests_dir: &'a Path,
}

/// Carry out what the agent tree asked for after a key or a click.
fn apply_tree_outcome(
    outcome: tree_view::Outcome,
    tree_view: &mut tree_view::TreeView,
    hub: Option<&mut super::approvals::Hub>,
    state: &StateDir,
    repo: &Path,
    rows: &[ui::SidebarRow],
    dash: &mut TreeDash,
) {
    use tree_view::Outcome;
    let notice =
        |dash: &mut TreeDash, text: String| push_notice(dash.notices, Instant::now(), text);
    match outcome {
        Outcome::None => {}
        Outcome::Leave => tree_view.toggle(),
        Outcome::Notice(text) => notice(dash, text),
        Outcome::OpenPane(short) => {
            let attachable = rows
                .iter()
                .any(|r| r.short == short && r.attached && r.state != ui::RowState::Dead);
            if !attachable {
                return notice(dash, "that pane has ended".into());
            }
            *dash.reveal_sidebar = true;
            *dash.chrome_selection = None;
            (*dash.selected, *dash.focused) =
                select_row(&short, rows, *dash.selected, *dash.focused);
            // The chat opens inside the tree, under its bar, with the pane taking every key.
            tree_view.open_chat();
        }
        Outcome::OpenSubagent {
            id,
            session,
            host,
            agent_type,
            description,
            title,
            siblings,
        } => {
            let path = session
                .as_deref()
                .and_then(|session| super::graph::native_subagent_path(state, session, &id));
            let fallback = path.map(|path| (title, path));
            let target = subagent_focus::Target {
                agent_type,
                description,
                siblings,
            };
            let host_short = host.filter(|short| {
                dash.panes
                    .iter()
                    .any(|p| p.short() == short && subagent_focus::drivable(p, &target))
            });
            let Some(short) = host_short else {
                match fallback {
                    Some((title, path)) => tree_view.open_subagent_view(title, path),
                    None => notice(dash, "no transcript was found for this subagent".into()),
                }
                return;
            };
            apply_tree_outcome(
                Outcome::OpenPane(short.clone()),
                tree_view,
                None,
                state,
                repo,
                rows,
                dash,
            );
            if tree_view.in_chat() {
                let focus = subagent_focus::Focus::new(short, target, Instant::now());
                tree_view.start_focus(focus, fallback);
            } else if let Some((title, path)) = fallback {
                tree_view.open_subagent_view(title, path);
            }
        }
        Outcome::Mail { to } => {
            let mut view = build_mail_view(state, repo);
            view.compose = Some(ui::ComposeDraft {
                to,
                body: String::new(),
            });
            *dash.overlay = ui::Overlay::Mail(view);
        }
        Outcome::MailSubagent { to, body } => {
            let mut view = build_mail_view(state, repo);
            view.compose = Some(ui::ComposeDraft { to, body });
            *dash.overlay = ui::Overlay::Mail(view);
        }
        Outcome::BackToFlow => tree_view.close_chat(),
        Outcome::Spawn => *dash.overlay = ui::Overlay::Spawn(ui::SpawnDraft::default()),
        Outcome::Nudge { short } => {
            *dash.overlay = ui::Overlay::Nudge(nudge_draft(&short, dash.panes));
        }
        Outcome::Stop { short } => stop_pane(
            &short,
            dash.panes,
            dash.cfg,
            dash.errors,
            dash.notices,
            Instant::now(),
        ),
        Outcome::Retry { short } => restore_ended_row(
            &short,
            dash.panes,
            dash.nudge_queues,
            dash.retained,
            dash.kept,
            dash.cfg,
            state,
            repo,
            dash.pane_size,
            dash.requests_dir,
            dash.errors,
            dash.notices,
            Instant::now(),
            rows,
            dash.selected,
        ),
        Outcome::AnswerShown {
            short,
            conn,
            decision,
        } => {
            let Some(hub) = hub else {
                return notice(dash, "the approvals inbox is off".into());
            };
            // The dashboard drew this request with its answer keys, which is what makes it answerable.
            if !hub.select_conn(conn, &short) {
                return notice(dash, "that approval is already gone".into());
            }
            let shown = hub.current().map(|item| {
                (
                    item.conn,
                    item.request.fully_shown && !item.request.released,
                )
            });
            hub.mark_drawn(shown);
            let key = approval_key(decision);
            if let Some(text) = approvals_view::handle_key(Some(hub), key, state).notice {
                notice(dash, text);
            }
        }
        Outcome::OverrideRuling { id, short } => {
            // The dashboard is the operator's own process, so it calls the lever directly; the
            // CLI refuses inside an agent session.
            match super::supervisor::override_ruling(state, &id, None) {
                Ok(_) => tree_view.override_done(&id, &short),
                Err(e) => notice(dash, format!("override failed: {e}")),
            }
        }
        Outcome::Answer { short, decision } => {
            let Some(hub) = hub else {
                return notice(dash, "the approvals inbox is off".into());
            };
            // Only the request the last frame drew can be answered; showing it first keeps that rule.
            if hub
                .drawn_item()
                .is_some_and(|item| item.request.short == short)
            {
                let key = approval_key(decision);
                if let Some(text) = approvals_view::handle_key(Some(hub), key, state).notice {
                    notice(dash, text);
                }
            } else if hub.select_short(&short) {
                notice(
                    dash,
                    "showing its request below; press again to answer".into(),
                );
            } else {
                notice(dash, "that approval is already gone".into());
            }
        }
    }
}

/// On setup failure, finish spawned panes explicitly: Unix has no Windows job
/// guard, and `panic = "abort"` skips Drop. Flush transcripts and release records.
fn abort_setup(panes: &mut [Pane], cfg: &CtxConfig, requests_dir: &Path) {
    let mut discarded = ErrorLog::default();
    shutdown_all(panes, cfg, &mut discarded);
    remove_request_dir(requests_dir);
}

/// Share one grace across panes so shutdown duration does not grow per child.
const SHUTDOWN_GRACE: Duration = Duration::from_secs(5);

/// Draw shutdown progress best-effort before pane teardown.
fn render_shutting_down(terminal: &mut Terminal<CrosstermBackend<io::Stdout>>, pane_count: usize) {
    let msg = format!("shutting down {pane_count} pane(s)\u{2026}");
    let _ = terminal.draw(|f| {
        let area = f.area();
        ui::render_center_message(f, area, &msg);
    });
}

/// Ask every pane to quit before waiting on one shared grace window;
/// serial per-pane waits could hold the alternate screen for N timeouts.
fn shutdown_all(panes: &mut [Pane], cfg: &CtxConfig, errors: &mut ErrorLog) {
    for pane in panes.iter_mut() {
        let quit_sequence = adapters::select(Some(pane.agent()), &[], cfg)
            .map(|adapter| adapter.quit_sequence())
            .unwrap_or("");
        pane.request_quit(quit_sequence);
    }
    let deadline = Instant::now() + SHUTDOWN_GRACE;
    while Instant::now() < deadline {
        if panes.iter_mut().all(|pane| pane.try_exited()) {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    for pane in panes.iter_mut() {
        if let Err(e) = pane.finish_shutdown() {
            push_error(errors, format!("shutdown {}: {e}", pane.short()));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    pub(super) fn key(code: KeyCode, modifiers: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, modifiers)
    }

    /// One never-repeated, never-acknowledged errors-dialog row.
    pub(super) fn err_item(text: &str) -> ui::ErrorItem {
        ui::ErrorItem {
            text: text.to_string(),
            count: 1,
            age_secs: 0,
            acked: false,
        }
    }

    // Task 7: sidebar row assembly + header facts.

    pub(super) fn pane_row(short: &str, harness: &str) -> PaneRowMeta {
        PaneRowMeta {
            role: "worker".into(),
            model: None,
            group_id: None,
            parent: None,
            budget: style::PLACEHOLDER.into(),
            writer: style::PLACEHOLDER.into(),
            short: short.to_string(),
            harness: harness.to_string(),
            state: ui::RowState::Idle,
            supervised: true,
            ended: None,
        }
    }

    /// Issue #354 phase 2: the same row, retained after its pane exited with
    /// `code` -- what `build_pane_rows` appends for an `EndedRow`.
    pub(super) fn ended_pane_row(short: &str, code: i32, exited_at: u64) -> PaneRowMeta {
        PaneRowMeta {
            state: ui::RowState::Dead,
            supervised: false,
            ended: Some(EndedMeta {
                exit_code: code,
                exited_at,
                age_secs: Some(300),
            }),
            ..pane_row(short, "claude")
        }
    }

    pub(super) fn owner(repo: &Path) -> FactsOwner<'_> {
        FactsOwner {
            repo,
            agent_name: "claude",
            session_short: "sess0000",
        }
    }

    /// Issue #330: the snapshot [`FactsRefresher`] would have published,
    /// computed synchronously right here. The reads themselves are unchanged
    /// -- only which thread makes them is -- so a cache test stays a test of
    /// the cache's own throttle rather than of a background thread's timing.
    pub(super) fn snapshot(
        state: &StateDir,
        repo: &Path,
        cfg: &CtxConfig,
    ) -> Option<FactsSnapshot> {
        let owner = owner(repo);
        Some(collect_facts_snapshot(
            &FactsInputs {
                state: state.clone(),
                repo: repo.to_path_buf(),
                agent_name: owner.agent_name.to_string(),
                session_short: owner.session_short.to_string(),
                mail_enabled: cfg.mail.enabled,
                cfg: cfg.clone(),
            },
            &[],
        ))
    }

    /// A stand-in for [`FactsRefresher`] inside a cache test. The tick asks on
    /// every frame, exactly as it does in production; only a cycle the test
    /// has armed answers, so "what the refresher published" and "what the tick
    /// read for itself" stay distinguishable. The snapshot it hands over is
    /// the real one ([`snapshot`]).
    pub(super) struct FakeRefresher<'a> {
        state: &'a StateDir,
        repo: &'a Path,
        cfg: &'a CtxConfig,
        armed: std::cell::Cell<bool>,
    }

    impl<'a> FakeRefresher<'a> {
        /// Armed: a dashboard always starts with the refresher's first cycle
        /// pending.
        pub(super) fn new(state: &'a StateDir, repo: &'a Path, cfg: &'a CtxConfig) -> Self {
            Self {
                state,
                repo,
                cfg,
                armed: std::cell::Cell::new(true),
            }
        }

        /// The refresher has finished another cycle.
        pub(super) fn arm(&self) {
            self.armed.set(true);
        }

        /// What `FactsRefresher::take_latest` hands the tick.
        pub(super) fn take(&self) -> Option<FactsSnapshot> {
            if !self.armed.replace(false) {
                return None;
            }
            snapshot(self.state, self.repo, self.cfg)
        }
    }

    /// This test module's stand-in for "the running dashboard's own pid" --
    /// arbitrary, since these tests never spawn a real process, but shared
    /// across every fixture/call so "owned by this dashboard" and "owned by
    /// a different one" are unambiguous.
    pub(super) const DASHBOARD_PID: u32 = 424242;

    pub(super) fn registry_record(
        short: &str,
        agent: &str,
        owner_pid: Option<u32>,
    ) -> sessions::Record {
        sessions::Record {
            session: format!("session-{short}"),
            short: short.to_string(),
            agent: agent.to_string(),
            repo: std::path::PathBuf::from("/repo"),
            repo_slug: "-repo".to_string(),
            verb: sessions::Verb::Wrap,
            pid: 1,
            started_at: 0,
            reachable: true,
            owner_pid,
            safety_policy_sha256: None,
            role: None,
            start_time: None,
            in_flight: None,
            runtime: runtime::RuntimeKind::Harness,
        }
    }

    // ------------------------------------------------------------------
    // Issue #354 phase 2: attention glyphs, done-unread acknowledgement,
    // retained completed-worker rows, group collapse keys.
    // ------------------------------------------------------------------

    /// A `SessionStatus` that projects `Blocked(WorkflowGate)` with recorded
    /// evidence -- the approved frame's own `approval · workflow gate` case.
    pub(super) fn blocked_status(revision: u64) -> super::super::attention::SessionStatus {
        super::super::attention::SessionStatus {
            lifecycle: super::super::attention::Lifecycle::Waiting,
            attention: super::super::attention::Attention::Approval,
            authority: super::super::attention::Authority::Workflow,
            evidence: "workflow gate".into(),
            last_transition: 200,
            revision,
            ..Default::default()
        }
    }

    pub(super) fn done_unread_status(revision: u64) -> super::super::attention::SessionStatus {
        super::super::attention::SessionStatus {
            lifecycle: super::super::attention::Lifecycle::Settled,
            visibility: super::super::attention::Visibility::Unseen,
            last_transition: 200,
            revision,
            ..Default::default()
        }
    }

    // Task 8: mail + memory overlay reducers -- pure, no I/O.

    pub(super) fn press(c: char) -> KeyEvent {
        key(KeyCode::Char(c), KeyModifiers::NONE)
    }

    // F2/F9/F13: what `fulfill_spawn_request` refuses before anything is
    // spawned. A request is data, never authority.

    pub(super) fn spawn_request(prompt: &str, cwd: &Path) -> spawnreq::SpawnRequest {
        spawnreq::SpawnRequest {
            kill: None,
            name: None,
            agent: "claude".to_string(),
            prompt: prompt.to_string(),
            cwd: cwd.to_path_buf(),
            requested_by: "aaaa1111".to_string(),
            model: None,
            // Every existing caller of this fixture models the ordinary
            // human-at-the-dashboard spawn overlay; a test that needs the
            // scripted/headless shape builds its own request literal with
            // `interactive: false` instead of going through this helper.
            interactive: true,
            // No lineage: matches the overlay's own real construction (see
            // its call site above), which is the delegation root, not a
            // spawn on some other session's behalf.
            role: None,
            parent_session: None,
            work_group_id: None,
            budget_tokens: None,
            force: false,
            workdir: None,
            mode: super::super::permit::WorkerMode::Writing,
            owns_workdir: false,
            result_schema: None,
            envelope: None,
            path_scope: Vec::new(),
            no_network: false,
            depth: None,
            max_restarts: None,
            timeout_secs: None,
            max_tool_calls: None,
            flags: Vec::new(),
            system_prompt: None,
            parent_seat_generation: None,
        }
    }

    /// Whether `git` is on `PATH` at all in this test environment -- the
    /// worktree-acceptance tests below need a real `git` binary to shell out
    /// to (same precedent as `compile::changed_repo_paths`'s own tests), and
    /// must skip gracefully rather than fail on a machine that somehow lacks
    /// one, exactly like every other conditionally-skipped test in this
    /// module (`#[cfg(windows)]`, the pty-needing tests, etc).
    pub(super) fn git_available() -> bool {
        std::process::Command::new("git")
            .arg("--version")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    }

    /// A real temp git repo (`git init`, one commit so `git worktree add` has
    /// something to branch a linked sibling from) plus one linked worktree
    /// created with `git worktree add`. Returns `None` (callers skip) if
    /// `git` itself is unavailable or any setup step fails -- these are
    /// integration tests against the real binary, not something a broken
    /// local git install should be able to fail loudly on.
    pub(super) fn git_repo_with_linked_worktree() -> Option<(tempfile::TempDir, PathBuf, PathBuf)> {
        if !git_available() {
            eprintln!("skipping: git not found on PATH");
            return None;
        }
        let root = tempfile::tempdir().ok()?;
        let main = root.path().join("main");
        std::fs::create_dir_all(&main).ok()?;
        let linked = root.path().join("linked");

        let run = |args: &[&str], cwd: &Path| -> bool {
            std::process::Command::new("git")
                .arg("-C")
                .arg(cwd)
                .args(args)
                .output()
                .map(|o| o.status.success())
                .unwrap_or(false)
        };

        if !run(&["init", "-q"], &main) {
            return None;
        }
        if !run(&["config", "user.email", "test@example.com"], &main) {
            return None;
        }
        if !run(&["config", "user.name", "test"], &main) {
            return None;
        }
        std::fs::write(main.join("README.md"), "hello\n").ok()?;
        if !run(&["add", "README.md"], &main) {
            return None;
        }
        if !run(&["commit", "-q", "-m", "initial"], &main) {
            return None;
        }
        let linked_str = linked.to_string_lossy().to_string();
        if !run(
            &["worktree", "add", &linked_str, "-b", "feature-branch"],
            &main,
        ) {
            return None;
        }

        Some((root, main, linked))
    }

    /// Creates a real temp git repo at `dir` (`git init -q`), for the
    /// workdir-roots tests below that need more than one real repository.
    pub(super) fn git_init_repo(dir: &Path) {
        std::fs::create_dir_all(dir).expect("mkdir");
        assert!(
            std::process::Command::new("git")
                .arg("-C")
                .arg(dir)
                .arg("init")
                .arg("-q")
                .output()
                .expect("git init")
                .status
                .success(),
            "git init must succeed in {dir:?}"
        );
    }

    pub(super) fn entry(entries: &[ui::MenuEntry], action: ui::MenuAction) -> &ui::MenuEntry {
        entries
            .iter()
            .find(|e| e.action == action)
            .unwrap_or_else(|| panic!("{action:?} must always be offered"))
    }

    #[cfg(windows)]
    pub(super) fn trivial_argv() -> Vec<String> {
        vec![
            "cmd".to_string(),
            "/c".to_string(),
            "exit".to_string(),
            "0".to_string(),
        ]
    }

    #[cfg(unix)]
    pub(super) fn trivial_argv() -> Vec<String> {
        vec!["sh".to_string(), "-c".to_string(), "exit 0".to_string()]
    }

    /// A worker pane with no durable turn signal (the codex shape issue
    /// #115 is actually about) that has printed its startup output and then
    /// gone quiet -- `report_back_reminder_sweep`'s own two preconditions,
    /// `has_produced_output` and `injectable`, both genuinely true rather
    /// than assumed. Mirrors `pane.rs`'s own `a_signal_less_pane_becomes_
    /// idle_after_the_quiet_period_and_not_before`, reused here because four
    /// tests below all need the identical setup.
    pub(super) fn spawn_idle_signal_less_worker_pane(
        state: &StateDir,
        repo: &Path,
        session_id: &str,
    ) -> Pane {
        use super::pane::tests::silent_after_first_line_argv;

        let spec = PaneSpec {
            agent_name: "test-agent".to_string(),
            argv: silent_after_first_line_argv(),
            role: prompt::PromptRole::Worker,
            verb: sessions::Verb::Dash,
            session_id: session_id.to_string(),
            title: "wrk report-back".to_string(),
        };
        let mut pane = Pane::spawn(
            spec,
            state,
            repo,
            repo,
            (80, 24),
            &[],
            false,
            Duration::from_millis(200),
        )
        .expect("spawn");

        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while std::time::Instant::now() < deadline {
            pane.drain();
            if pane.last_line().contains("hello") {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(
            pane.last_line().contains("hello"),
            "the startup line must land before this test can mean anything: {:?}",
            pane.last_line()
        );

        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        let mut became_injectable = false;
        while std::time::Instant::now() < deadline {
            pane.drain();
            if pane.injectable() {
                became_injectable = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(
            became_injectable,
            "the pane must become injectable once its quiet window closes, with no turn \
             signal ever sent"
        );
        pane
    }

    /// A long-lived child that produces no pty output of its own at all
    /// (unlike `pane::tests::long_lived_argv`'s `ping`, which prints a reply
    /// line once a second): needed here because the advisory assertion below
    /// reads `Pane::last_line`, and a periodic `ping` reply would otherwise
    /// race the injected line out of that position.
    #[cfg(windows)]
    pub(super) fn silent_long_lived_argv() -> Vec<String> {
        vec![
            "cmd".to_string(),
            "/c".to_string(),
            "ping -n 60 127.0.0.1 >nul".to_string(),
        ]
    }

    #[cfg(unix)]
    pub(super) fn silent_long_lived_argv() -> Vec<String> {
        vec!["sh".to_string(), "-c".to_string(), "sleep 60".to_string()]
    }

    /// Issue #403: the one request kind on this channel that is not a spawn.
    pub(super) fn kill_request(short: &str) -> spawnreq::SpawnRequest {
        spawnreq::SpawnRequest {
            kill: Some(short.to_string()),
            requested_by: "ctx kill".to_string(),
            ..Default::default()
        }
    }

    pub(super) fn kept(parent: Option<&str>) -> (spawnreq::SpawnRequest, Option<String>) {
        (
            spawnreq::SpawnRequest::default(),
            parent.map(str::to_string),
        )
    }

    pub(super) fn restore_pane(short: &str, session_id: &str) -> roster::RosterPane {
        roster::RosterPane {
            agent: "claude".to_string(),
            session_id: session_id.to_string(),
            role: prompt::PromptRole::Worker.label().to_string(),
            short: short.to_string(),
            title: format!("wrk {short}"),
            ..Default::default()
        }
    }

    /// A pid guaranteed dead by the time it is used: a real child, waited on.
    pub(super) fn dead_pid() -> u32 {
        let argv = trivial_argv();
        let mut cmd = std::process::Command::new(&argv[0]);
        cmd.args(&argv[1..]);
        let mut child = cmd.spawn().expect("spawn trivial child");
        let pid = child.id();
        let _ = child.wait();
        pid
    }

    pub(super) fn at(kind: MouseEventKind, column: u16, row: u16) -> event::MouseEvent {
        event::MouseEvent {
            kind,
            column,
            row,
            modifiers: KeyModifiers::NONE,
        }
    }

    // ------------------------------------------------------------------
    // Issue #354 phase 4: the palette/help overlay, the Esc/Enter contract
    // and the first-run tip.
    // ------------------------------------------------------------------

    pub(super) fn live_ctx() -> actions::ActionContext {
        actions::ActionContext {
            selected: true,
            attached: true,
            alive: true,
            ..actions::ActionContext::default()
        }
    }

    pub(super) fn open_palette(mode: ui::PaletteMode) -> ui::PaletteView {
        build_palette_view(mode, live_ctx())
    }

    pub(super) fn typed(mut view: ui::PaletteView, text: &str) -> ui::PaletteView {
        for c in text.chars() {
            let (next, effect) =
                palette_overlay_reduce(view, key(KeyCode::Char(c), KeyModifiers::NONE));
            assert!(effect.is_none(), "typing must never run an action");
            view = next.expect("typing never closes the palette");
        }
        view
    }

    /// R4: the terminal-setup failure arms cannot be driven without a real
    /// terminal, so they all call one helper -- this is that helper, against a
    /// real child: the already-spawned orchestrator pane is shut down and its
    /// registry record released rather than orphaned behind a returned `Err`.
    #[test]
    fn abort_setup_shuts_down_an_already_spawned_pane() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().join("state"));
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(&repo).expect("mkdir repo");

        let spec = PaneSpec {
            agent_name: "test-agent".to_string(),
            argv: trivial_argv(),
            role: prompt::PromptRole::Orchestrator,
            verb: sessions::Verb::Chat,
            session_id: "66666666-2222-4333-8444-555555555555".to_string(),
            title: "orch".to_string(),
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
        let short = panes[0].short().to_string();
        let record = state.sessions().join(format!("{short}.json"));
        assert!(record.exists(), "registered while it runs");

        // O7: the request directory this startup had already created must go
        // too, rather than leaking one capability-token directory per failed
        // launch under `<state>/dash/`.
        let requests_dir = state.dash().join("aaaa1111-token").join("requests");
        std::fs::create_dir_all(&requests_dir).expect("mkdir requests");

        abort_setup(&mut panes, &CtxConfig::default(), &requests_dir);

        assert!(
            !record.exists(),
            "a failed terminal setup releases the pane it had already spawned"
        );
        assert!(
            !requests_dir
                .parent()
                .expect("requests dir has a parent")
                .exists(),
            "and removes the spawn-request directory it had created"
        );
        // Idempotent, exactly like the quit path it shares: the caller may
        // already have shut a pane down.
        abort_setup(&mut panes, &CtxConfig::default(), &requests_dir);
    }
}
