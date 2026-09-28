//! supervision support for the interactive supervisor.

use super::*;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PumpEvent {
    Output(usize),
    Input(usize),
    PtyClosed,
}

#[derive(Debug, Clone)]
pub struct InjectionState {
    /// The turn number the agent last reported, for display only. It counts
    /// turns within one transcript, so it restarts at 1 after a relaunch and
    /// shrinks when a compaction rewrites the file -- which is why nothing
    /// that has to move forwards is keyed on it.
    pub last_turn: u64,
    /// Turn signals this supervisor has received, ever. Monotonic across
    /// relaunches and compactions by construction, because it counts what
    /// arrived here rather than what the transcript says about itself.
    pub signals_seen: u64,
    pub verdict: Verdict,
    pub score: u32,
    pub user_typed_since_turn: bool,
    pub last_output: Instant,
    /// The last time the operator's own keystroke reached this pty --
    /// `last_output`'s counterpart for input, tracked only so a signal-less
    /// adapter's own idleness ([`signal_less_mail_ready`]) can be measured
    /// from the *later* of the two, the same `dash::pane::latest_of` fold a
    /// signal-less dashboard pane already applies. A turn-signal-capable
    /// session never reads this field: its idleness is decided by the signal
    /// alone (`may_inject`), exactly as before this field existed.
    pub last_input: Instant,
    /// `signals_seen` at the moment an action fired. The next action waits for
    /// a strictly newer signal than that one.
    pub cooldown_at_signal: Option<u64>,
    pub degraded: bool,
}

impl Default for InjectionState {
    fn default() -> Self {
        Self::new()
    }
}

impl InjectionState {
    pub fn new() -> Self {
        let now = Instant::now();
        Self {
            last_turn: 0,
            signals_seen: 0,
            verdict: Verdict::Healthy,
            score: 0,
            user_typed_since_turn: false,
            last_output: now,
            last_input: now,
            cooldown_at_signal: None,
            degraded: false,
        }
    }

    pub fn on_event(&mut self, event: PumpEvent, now: Instant) {
        match event {
            PumpEvent::Output(_) => self.last_output = now,
            PumpEvent::Input(_) => {
                self.user_typed_since_turn = true;
                self.last_input = now;
            }
            PumpEvent::PtyClosed => {}
        }
    }

    pub fn on_turn(&mut self, signal: &TurnSignal) {
        self.last_turn = signal.turn;
        self.signals_seen += 1;
        self.verdict = signal.verdict;
        self.score = signal.score;
        self.user_typed_since_turn = false;
    }
}

/// Whether the last-armed cooldown has been cleared by a later turn. Shared
/// by `may_inject` and `action_for`'s `Advise` arm: an advisory needs none of
/// `may_inject`'s other preconditions (it only prints, never types into the
/// agent), but it still must not re-fire on every ~100ms poll tick within the
/// same turn once the pump has armed the cooldown for it.
///
/// Keyed on the supervisor's own signal count rather than the turn number the
/// transcript reports. A relaunch starts a fresh session whose turns count from
/// one again, and a compaction rewrites the transcript so its turn count
/// shrinks; a cooldown armed at "turn 30" then never cleared, and supervision
/// went silent for the rest of the run.
fn cooldown_cleared(state: &InjectionState) -> bool {
    state
        .cooldown_at_signal
        .is_none_or(|at| state.signals_seen > at)
}

/// Both spec preconditions, and nothing else: a turn boundary has been reported
/// and the user is idle. Everything about which verdict deserves which action
/// lives in the escalation ladder, not here.
pub fn may_inject(state: &InjectionState, now: Instant, debounce: Duration) -> bool {
    !state.degraded
        && state.signals_seen > 0
        && !state.user_typed_since_turn
        && now.duration_since(state.last_output) >= debounce
        && cooldown_cleared(state)
}

/// Issue #84: whether a pending `zirv ctx handover` request may be acted on
/// right now, versus refused with "mid-turn; retry once idle, or pass
/// --force". `force` always wins outright (the operator asked to interrupt
/// whatever the session is doing); otherwise this is exactly `may_inject`'s
/// own "verified-idle turn boundary" precondition -- the same quiesce check
/// every other injection in this module already gates on, reused rather than
/// reinvented for this seam.
pub fn handover_may_act(
    state: &InjectionState,
    now: Instant,
    debounce: Duration,
    force: bool,
) -> bool {
    force || may_inject(state, now, debounce)
}

/// Whether a session with no turn-signal mechanism at all (codex today) has
/// been quiet long enough for T13's live mail advisory to be typed into it.
///
/// `may_inject`'s `state.signals_seen > 0` precondition can never pass for
/// such a session: `register_turn_signal` is a no-op for it, so `on_turn` is
/// never called and `signals_seen` stays `0` for the session's entire life.
/// Before this, T13's poll arm therefore always fell back to `MailAction::
/// Announce` for it -- a documented residual (see Known Issues, "wrap's own
/// live mail advisory has no equivalent for a signal-less adapter"), and the
/// mirror image of the bug `dash::pane::pane_is_idle` already fixed for a
/// signal-less dashboard pane.
///
/// Mirrors `dash::pane::signal_less_quiescent` exactly: quiet is measured
/// from the *later* of the child's last output and the operator's own last
/// keystroke into it, not from output alone -- the same fold
/// `dash::pane::latest_of` applies, for the same reason (see that function's
/// own doc comment: an injection or a keystroke has to restart the quiet
/// window, or the very next poll tick reads the pane as idle again before the
/// child has had any real chance to respond). Finding 5 (review): the
/// elapsed-time check itself is `dash::pane::quiescent_since`, shared rather
/// than reimplemented here, so the two formulas cannot drift apart again.
/// Deliberately **not** folded
/// into `may_inject` itself: that function also gates `Compact`/`Restart`,
/// and a debounce-only idle guess is not something this codebase wants
/// deciding whether to type `/compact` into a session. `may_inject`'s own
/// `state.signals_seen > 0` precondition is what actually protects that path
/// for a signal-less adapter (codex today): `register_turn_signal` is a
/// no-op for it regardless of `capabilities().events` (issue #86 gave codex
/// real event *parsing*, which is a separate mechanism from turn-signal
/// *posting*), so `signals_seen` never advances and `may_inject` stays
/// permanently false -- this is scoped to the one caller that actually
/// needs the quiet-time-only question.
pub fn signal_less_mail_ready(state: &InjectionState, now: Instant, quiet: Duration) -> bool {
    !state.degraded
        && super::dash::pane::quiescent_since(state.last_output.max(state.last_input), now, quiet)
}

/// The T13 mail poll's own eligibility question, branching on
/// `turn_signal_capable` (`AgentAdapter::capabilities().turn_signal`)
/// exactly the way `dash::pane::pane_is_idle` already branches for a
/// dashboard pane: `may_inject` for an adapter that reports turn boundaries,
/// [`signal_less_mail_ready`] for one that cannot. Split out of the pump
/// loop's own call site so the branch is testable without a real pty.
pub fn mail_inject_ready(
    turn_signal_capable: bool,
    state: &InjectionState,
    now: Instant,
    debounce: Duration,
    idle_quiet: Duration,
) -> bool {
    if turn_signal_capable {
        may_inject(state, now, debounce)
    } else {
        signal_less_mail_ready(state, now, idle_quiet)
    }
}

pub const TRANSCRIPT_ENV: &str = "ZIRV_CTX_TRANSCRIPT";

/// The pre-F5 name: one global file under the state dir root. Still *read*
/// (see `read_socket_path`) so a supervisor started by an older build stays
/// discoverable, but never written any more: two concurrent supervisors
/// clobbered each other's entry, and whoever read it afterwards got a socket
/// belonging to somebody else's session.
#[allow(dead_code)] // read only by `read_socket_path`, itself test-only today
pub const SOCKET_PATH_FILE: &str = "socket-path";

/// `<state>/socket-path-<short8>`, one per supervisor, named after the same
/// short id the socket itself and the session registry record already use.
pub const SOCKET_PATH_PREFIX: &str = "socket-path-";

pub fn socket_path_file_for(session: &str) -> String {
    format!("{SOCKET_PATH_PREFIX}{}", super::sessions::short_id(session))
}

/// Publishes where this supervisor bound its turn-signal socket, so `zirv ctx
/// status`, external tooling and the pty tests can find it. Best-effort, like
/// every other piece of state-dir housekeeping: failing to publish must never
/// fail a launch.
pub fn publish_socket_path(state: &StateDir, session: &str, socket: &Path) {
    let _ = super::state::create_private_dir_all(state.root());
    let _ = super::state::write_private(
        &state.root().join(socket_path_file_for(session)),
        &socket.display().to_string(),
    );
}

/// Removes this supervisor's published socket path. Paired with
/// `publish_socket_path` at the one place `wrap` leaves the pump, so a dead
/// session's file does not linger to be picked as "the newest" by a later
/// reader with no session of its own.
pub fn unpublish_socket_path(state: &StateDir, session: &str) {
    let _ = std::fs::remove_file(state.root().join(socket_path_file_for(session)));
}

/// Resolves the socket path a reader should use.
///
/// `session` is the reader's own `ZIRV_CTX_SESSION` (or whichever session it
/// is asking about). When it is given, only *that* session's file is
/// considered: silently handing back a different live session's socket is
/// precisely the cross-session confusion F5 exists to end, so a named session
/// with no file of its own falls through to the legacy file and then to
/// `None`, never to a neighbour's socket.
///
/// With no session -- an operator at a shell, or a test that never learned
/// the id -- the newest published file wins, which is the closest honest
/// approximation of "the session I am looking at" available without one.
///
/// The legacy global file is the last fallback either way, so a supervisor
/// left over from a pre-F5 build is still reachable.
///
/// No production caller inside this binary reads it back today -- `wrap` is
/// the only writer, and everything downstream of it already holds the socket
/// path directly. It exists because publishing the file is a *contract with
/// external tooling* (that is why it is written at all), and shipping a
/// writer whose naming scheme has no canonical reader is how the pre-F5
/// global file's semantics got lost in the first place. The pty tests are its
/// in-tree consumer.
#[allow(dead_code)] // no production caller yet; the pty tests are its in-tree consumer
pub fn read_socket_path(state: &StateDir, session: Option<&str>) -> Option<String> {
    let read = |path: PathBuf| -> Option<String> {
        std::fs::read_to_string(path)
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
    };

    if let Some(session) = session {
        return read(state.root().join(socket_path_file_for(session)))
            .or_else(|| read(state.root().join(SOCKET_PATH_FILE)));
    }

    let mut newest: Option<(std::time::SystemTime, PathBuf)> = None;
    if let Ok(entries) = std::fs::read_dir(state.root()) {
        for entry in entries.flatten() {
            let path = entry.path();
            let is_published = path
                .file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with(SOCKET_PATH_PREFIX));
            if !is_published {
                continue;
            }
            let modified = entry
                .metadata()
                .and_then(|m| m.modified())
                .unwrap_or(std::time::UNIX_EPOCH);
            if newest.as_ref().is_none_or(|(seen, _)| modified >= *seen) {
                newest = Some((modified, path));
            }
        }
    }
    if let Some((_, path)) = newest
        && let Some(found) = read(path)
    {
        return Some(found);
    }

    read(state.root().join(SOCKET_PATH_FILE))
}

/// Which file wrap watches for compactions and reads handoff context from.
///
/// wrap cannot derive it. It spawns the user's own command, and the agent
/// mints its own session id inside that process, so the only party that knows
/// the path is the hook running in the agent: it travels on the turn signal.
/// A relaunch invalidates it, because the fresh session writes somewhere new.
/// `ZIRV_CTX_TRANSCRIPT` pins it for an agent whose hook cannot report one.
#[derive(Debug, Default)]
pub struct TranscriptSource {
    pinned: Option<PathBuf>,
    reported: Option<PathBuf>,
}

impl TranscriptSource {
    pub fn new(pinned: Option<PathBuf>) -> Self {
        Self {
            pinned,
            reported: None,
        }
    }

    /// `None` while no session has reported one, which is the honest answer:
    /// guessing a path means watching a file nobody writes.
    pub fn path(&self) -> Option<&Path> {
        self.pinned.as_deref().or(self.reported.as_deref())
    }

    pub fn adopt(&mut self, reported: Option<&str>) {
        if let Some(path) = reported.filter(|p| !p.is_empty()) {
            self.reported = Some(PathBuf::from(path));
        }
    }

    /// The relaunched agent is a new session writing a new file, so the old
    /// path must not be watched for one more poll.
    pub fn forget(&mut self) {
        self.reported = None;
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    None,
    Advise,
    Compact,
    Restart,
}

/// Advisories only print, so they need no injection window. Compaction and
/// restart type into the agent, so both preconditions apply.
pub fn action_for(state: &InjectionState, now: Instant, debounce: Duration) -> Action {
    if state.degraded {
        return Action::None;
    }
    match state.verdict {
        Verdict::Healthy => Action::None,
        Verdict::Advise if cooldown_cleared(state) => Action::Advise,
        Verdict::Compact if may_inject(state, now, debounce) => Action::Compact,
        Verdict::Restart if may_inject(state, now, debounce) => Action::Restart,
        _ => Action::None,
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::*;
    use super::*;
    fn ready_state(now: Instant) -> InjectionState {
        let mut state = InjectionState::new();
        state.on_turn(&turn_signal(3, Verdict::Compact));
        state.last_output = now - Duration::from_secs(10);
        state
    }

    #[test]
    fn a_fresh_state_never_injects() {
        let now = Instant::now();
        let state = InjectionState::new();
        assert!(
            !may_inject(&state, now, DEBOUNCE),
            "no turn boundary seen yet"
        );
    }

    #[test]
    fn an_idle_user_at_a_turn_boundary_may_be_injected_into() {
        let now = Instant::now();
        assert!(may_inject(&ready_state(now), now, DEBOUNCE));
    }

    /// Issue #84, acceptance: "a swap mid-turn is refused without --force".
    /// A fresh state (no turn boundary reported yet -- the same shape a
    /// session mid-turn actually has, since it never satisfies `may_inject`'s
    /// own `signals_seen > 0` precondition) refuses a plain handover request,
    /// but `--force` overrides the refusal outright.
    #[test]
    fn a_handover_is_refused_mid_turn_without_force() {
        let now = Instant::now();
        let mid_turn = InjectionState::new();
        assert!(
            !handover_may_act(&mid_turn, now, DEBOUNCE, false),
            "mid-turn, no --force: must refuse"
        );
        assert!(
            handover_may_act(&mid_turn, now, DEBOUNCE, true),
            "--force overrides the mid-turn refusal"
        );
    }

    /// The mirror case: an idle session at a verified turn boundary needs no
    /// `--force` at all.
    #[test]
    fn a_handover_at_an_idle_turn_boundary_needs_no_force() {
        let now = Instant::now();
        assert!(handover_may_act(&ready_state(now), now, DEBOUNCE, false));
    }

    #[test]
    fn typing_after_the_turn_blocks_injection() {
        let now = Instant::now();
        let mut state = ready_state(now);
        state.on_event(PumpEvent::Input(1), now);
        assert!(
            !may_inject(&state, now, DEBOUNCE),
            "the user is mid-thought"
        );
    }

    #[test]
    fn recent_output_blocks_injection_until_the_debounce_passes() {
        let now = Instant::now();
        let mut state = ready_state(now);
        state.on_event(PumpEvent::Output(120), now);
        assert!(!may_inject(&state, now, DEBOUNCE));
        assert!(
            may_inject(&state, now + Duration::from_secs(4), DEBOUNCE),
            "quiet for longer than the debounce"
        );
    }

    // Root cause 3 (this task): `may_inject` needs `signals_seen > 0`, which
    // a signal-less adapter (codex today) can never satisfy -- its
    // `register_turn_signal` is a no-op, so `on_turn` never runs and the mail
    // advisory (T13) fell back to `MailAction::Announce` forever, exactly as
    // the pre-fix `mail_with_no_injection_window_yet_is_announced_on_stderr_
    // instead` pins for a claude session that has not yet reported its first
    // turn. `signal_less_mail_ready`/`mail_inject_ready` close it for a
    // session that will *never* report one, mirroring
    // `dash::pane::signal_less_quiescent`.

    const IDLE_QUIET: Duration = Duration::from_secs(10);

    /// A fresh `InjectionState` with both `last_output` and `last_input`
    /// pinned to `now`, rather than left at whatever instant the constructor
    /// itself happened to read. `InjectionState::new()` stamps `last_input`
    /// with its own `Instant::now()` call, a few nanoseconds after a caller's
    /// own `let now = Instant::now()` -- close enough that most assertions
    /// never notice, but exactly the kind of sub-microsecond skew that makes
    /// a `duration_since(..) >= quiet` boundary check flaky. Pinning both
    /// fields to the same `now` the test already captured removes that
    /// skew entirely.
    fn signal_less_state(now: Instant) -> InjectionState {
        let mut state = InjectionState::new();
        state.last_output = now;
        state.last_input = now;
        state
    }

    #[test]
    fn a_fresh_signal_less_state_is_not_ready_yet() {
        let now = Instant::now();
        let state = signal_less_state(now);
        assert!(
            !signal_less_mail_ready(&state, now, IDLE_QUIET),
            "no time has passed since the state was created"
        );
    }

    #[test]
    fn a_signal_less_session_becomes_ready_once_quiet_for_the_idle_window() {
        let now = Instant::now();
        let state = signal_less_state(now);
        assert!(
            !signal_less_mail_ready(&state, now, IDLE_QUIET),
            "just produced output"
        );
        assert!(
            signal_less_mail_ready(&state, now + IDLE_QUIET, IDLE_QUIET),
            "quiet for the whole idle window, with no turn signal ever needed"
        );
    }

    #[test]
    fn a_signal_less_sessions_own_keystroke_restarts_the_quiet_window() {
        let now = Instant::now();
        let mut state = signal_less_state(now);
        let quiet_at = now + IDLE_QUIET;
        assert!(signal_less_mail_ready(&state, quiet_at, IDLE_QUIET));

        // A keystroke lands right as the pane would otherwise have gone
        // ready -- the same race `dash::pane`'s own `latest_of` fold closes
        // for a dashboard pane: measuring off output alone would still read
        // this as quiet.
        state.on_event(PumpEvent::Input(1), quiet_at);
        assert!(
            !signal_less_mail_ready(&state, quiet_at, IDLE_QUIET),
            "the operator's own keystroke must restart the quiet window"
        );
        assert!(
            signal_less_mail_ready(&state, quiet_at + IDLE_QUIET, IDLE_QUIET),
            "and it clears once quiet resumes for the full window"
        );
    }

    #[test]
    fn a_degraded_signal_less_session_is_never_ready() {
        let now = Instant::now();
        let mut state = signal_less_state(now);
        state.degraded = true;
        assert!(!signal_less_mail_ready(
            &state,
            now + IDLE_QUIET,
            IDLE_QUIET
        ));
    }

    #[test]
    fn mail_inject_ready_uses_may_inject_for_a_turn_signal_capable_adapter() {
        let now = Instant::now();
        // Idle by `signal_less_mail_ready`'s own rule (long quiet, no typing)
        // but with no turn ever reported -- `may_inject` must still say no
        // for a capable adapter, since that is the precise bug this task is
        // about for the *other* direction (claude waiting on its first turn).
        let mut state = signal_less_state(now);
        let later = now + IDLE_QUIET;
        assert!(
            !mail_inject_ready(true, &state, later, DEBOUNCE, IDLE_QUIET),
            "a capable adapter must still wait for a real turn boundary"
        );

        state.on_turn(&turn_signal(1, Verdict::Healthy));
        state.last_output = later - Duration::from_secs(10);
        assert!(mail_inject_ready(true, &state, later, DEBOUNCE, IDLE_QUIET));
    }

    #[test]
    fn mail_inject_ready_uses_the_signal_less_idle_window_for_an_incapable_adapter() {
        let now = Instant::now();
        let state = signal_less_state(now);
        assert!(
            !mail_inject_ready(false, &state, now, DEBOUNCE, IDLE_QUIET),
            "just produced output"
        );
        assert!(
            mail_inject_ready(false, &state, now + IDLE_QUIET, DEBOUNCE, IDLE_QUIET),
            "quiet for the idle window, with signals_seen still at 0 forever"
        );
        assert_eq!(state.signals_seen, 0, "sanity: no signal ever arrived");
    }

    #[test]
    fn a_new_turn_clears_the_typing_flag() {
        let now = Instant::now();
        let mut state = ready_state(now);
        state.on_event(PumpEvent::Input(1), now);
        state.on_turn(&turn_signal(4, Verdict::Compact));
        state.last_output = now - Duration::from_secs(10);
        assert!(may_inject(&state, now, DEBOUNCE));
        assert_eq!(state.last_turn, 4);
    }

    #[test]
    fn the_cooldown_blocks_until_a_later_turn_arrives() {
        let now = Instant::now();
        let mut state = ready_state(now);
        state.cooldown_at_signal = Some(state.signals_seen);
        assert!(
            !may_inject(&state, now, DEBOUNCE),
            "same turn as the cooldown"
        );

        state.on_turn(&turn_signal(4, Verdict::Compact));
        state.last_output = now - Duration::from_secs(10);
        assert!(
            may_inject(&state, now, DEBOUNCE),
            "a later turn releases it"
        );
    }

    /// The turn number is per-transcript: a relaunched session counts from one
    /// again, and a compaction rewrites the transcript so the count shrinks.
    /// A cooldown armed at the dead session's turn 30 then never cleared, and
    /// advise, compact and restart all went silent for the rest of the run.
    #[test]
    fn a_cooldown_outlives_neither_a_relaunch_nor_a_compaction() {
        let now = Instant::now();
        let mut state = ready_state(now);
        state.on_turn(&turn_signal(30, Verdict::Restart));
        state.cooldown_at_signal = Some(state.signals_seen);
        assert!(!cooldown_cleared(&state), "armed for the signal just seen");

        // The relaunched session's very first turn: number 1, far below 30.
        state.on_turn(&turn_signal(1, Verdict::Advise));
        assert!(
            cooldown_cleared(&state),
            "a fresh session's first turn is still progress this supervisor saw"
        );
    }

    #[test]
    fn a_degraded_supervisor_never_injects() {
        let now = Instant::now();
        let mut state = ready_state(now);
        state.degraded = true;
        assert!(!may_inject(&state, now, DEBOUNCE));
    }

    #[test]
    fn the_state_records_the_latest_verdict_and_score() {
        let mut state = InjectionState::new();
        state.on_turn(&TurnSignal {
            session_id: "s".to_string(),
            turn: 9,
            score: 91,
            verdict: Verdict::Restart,
            transcript_path: None,
        });
        assert_eq!(state.verdict, Verdict::Restart);
        assert_eq!(state.score, 91);
        assert_eq!(state.last_turn, 9);
    }

    #[test]
    fn the_ladder_maps_verdicts_to_actions() {
        let now = Instant::now();
        let mut state = ready_state(now);

        state.verdict = Verdict::Healthy;
        assert_eq!(action_for(&state, now, DEBOUNCE), Action::None);

        state.verdict = Verdict::Advise;
        assert_eq!(action_for(&state, now, DEBOUNCE), Action::Advise);

        state.verdict = Verdict::Compact;
        assert_eq!(action_for(&state, now, DEBOUNCE), Action::Compact);

        state.verdict = Verdict::Restart;
        assert_eq!(action_for(&state, now, DEBOUNCE), Action::Restart);
    }

    #[test]
    fn an_advisory_needs_no_injection_window() {
        let now = Instant::now();
        let mut state = ready_state(now);
        state.verdict = Verdict::Advise;
        state.on_event(PumpEvent::Input(1), now);
        assert_eq!(
            action_for(&state, now, DEBOUNCE),
            Action::Advise,
            "advice is written to the terminal, never typed into the agent"
        );
    }

    /// Bug (confirmed): the pump loop arms `cooldown_at_signal` after
    /// advising ("advise once per turn"), but `action_for`'s `Advise` arm
    /// never consulted it, so the same advisory reprinted on every ~100ms
    /// poll tick for the rest of the turn. Two consecutive evaluations with
    /// an unchanged `Advise` verdict, cooldown armed after the first, must
    /// not both advise.
    #[test]
    fn advise_is_not_repeated_every_poll_once_the_cooldown_is_armed() {
        let now = Instant::now();
        let mut state = ready_state(now);
        state.verdict = Verdict::Advise;
        assert_eq!(
            action_for(&state, now, DEBOUNCE),
            Action::Advise,
            "first tick advises"
        );

        // Mirrors what the pump loop does right after advising.
        state.cooldown_at_signal = Some(state.signals_seen);

        assert_eq!(
            action_for(&state, now, DEBOUNCE),
            Action::None,
            "the same turn must not advise again on every poll tick"
        );
    }

    #[test]
    fn compaction_and_restart_respect_the_injection_window() {
        let now = Instant::now();
        for verdict in [Verdict::Compact, Verdict::Restart] {
            let mut state = ready_state(now);
            state.verdict = verdict;
            state.on_event(PumpEvent::Input(1), now);
            assert_eq!(
                action_for(&state, now, DEBOUNCE),
                Action::None,
                "{verdict:?} must wait for an idle user"
            );
        }
    }

    #[test]
    fn a_degraded_supervisor_still_advises_but_never_injects() {
        let now = Instant::now();
        let mut state = ready_state(now);
        state.degraded = true;
        state.verdict = Verdict::Advise;
        assert_eq!(action_for(&state, now, DEBOUNCE), Action::None);
    }

    #[test]
    fn wrap_has_no_transcript_until_a_signal_names_one() {
        let mut source = TranscriptSource::new(None);
        assert_eq!(
            source.path(),
            None,
            "the agent minted its own session id, so there is nothing to derive"
        );

        source.adopt(Some("/tmp/a.jsonl"));
        assert_eq!(source.path(), Some(std::path::Path::new("/tmp/a.jsonl")));

        source.adopt(None);
        source.adopt(Some(""));
        assert_eq!(
            source.path(),
            Some(std::path::Path::new("/tmp/a.jsonl")),
            "a signal with nothing to report leaves the known path alone"
        );
    }

    #[test]
    fn an_explicit_transcript_override_outranks_every_signal() {
        let mut source = TranscriptSource::new(Some(PathBuf::from("/pinned.jsonl")));
        source.adopt(Some("/tmp/a.jsonl"));
        assert_eq!(source.path(), Some(std::path::Path::new("/pinned.jsonl")));
        source.forget();
        assert_eq!(
            source.path(),
            Some(std::path::Path::new("/pinned.jsonl")),
            "a pinned path outlives the session it was pinned for"
        );
    }

    #[test]
    fn a_relaunch_forgets_the_dead_sessions_transcript() {
        let mut source = TranscriptSource::new(None);
        source.adopt(Some("/tmp/old.jsonl"));
        source.forget();
        assert_eq!(
            source.path(),
            None,
            "the killed session's file must not be watched a moment longer"
        );

        source.adopt(Some("/tmp/new.jsonl"));
        assert_eq!(source.path(), Some(std::path::Path::new("/tmp/new.jsonl")));
    }
}
