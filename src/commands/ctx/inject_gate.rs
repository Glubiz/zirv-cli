//! Issue #785: the `[jev] inject` gate. Before an automatic injection
//! (PTY `/compact`, restart+handoff, live mail line, hook mail note, Stop rot
//! advisory) zirv may ask Jev, from bucketed numeric facts only, whether the
//! agent is mid-unit and the injection should wait. Jev may only DEFER, never
//! add: every kind has a hard deferral cap after which it injects as today,
//! operator mail and restart at the hard ceiling are never deferred, and any
//! error, timeout or indecisive answer injects as today. Safety/deny
//! messages have no [`InjectKind`] at all, so they can never reach this gate.

use std::path::PathBuf;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use super::config::CtxConfig;
use super::jev;
use super::state::{self, StateDir};

/// The site string every decision/effect row of this gate carries.
pub(crate) const SITE: &str = "inject";

/// Minimum `true` (defer) probability before a deferral is honoured. Basis:
/// `wakegate` skips a wakeup only when P(wake) < 0.2 (21/21 on its smoke
/// test), i.e. P(defer) > 0.8; facts-only state is untested, so no looser.
pub(crate) const DEFER_MIN_PROBABILITY: f64 = 0.8;

/// Mail (PTY line or hook note) is never held past this many turns...
pub(crate) const MAIL_MAX_DEFER_TURNS: u32 = 3;
/// ...or this many seconds since the first deferral / the oldest unread.
pub(crate) const MAIL_MAX_DEFER_SECS: u64 = 600;
/// A PTY `/compact` is never held past this many turns...
pub(crate) const COMPACT_MAX_DEFER_TURNS: u32 = 3;
/// ...or this many seconds.
pub(crate) const COMPACT_MAX_DEFER_SECS: u64 = 900;
/// A restart+handoff is held for at most one turn...
pub(crate) const RESTART_MAX_DEFER_TURNS: u32 = 1;
/// ...or this many seconds.
pub(crate) const RESTART_MAX_DEFER_SECS: u64 = 300;
/// Rot score at or above which a restart is the hard ceiling: never deferred.
pub(crate) const RESTART_HARD_CEILING_SCORE: u32 = 90;
/// The Stop rot advisory is never held past this many Stops...
pub(crate) const STOP_ADVISORY_MAX_DEFER_TURNS: u32 = 3;
/// ...or this many seconds.
pub(crate) const STOP_ADVISORY_MAX_DEFER_SECS: u64 = 1800;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum InjectKind {
    Compact,
    Restart,
    MailPty,
    MailNote,
    StopAdvisory,
}

impl InjectKind {
    fn code(self) -> u64 {
        match self {
            InjectKind::Compact => 0,
            InjectKind::Restart => 1,
            InjectKind::MailPty => 2,
            InjectKind::MailNote => 3,
            InjectKind::StopAdvisory => 4,
        }
    }

    fn label(self) -> &'static str {
        match self {
            InjectKind::Compact => "compact",
            InjectKind::Restart => "restart",
            InjectKind::MailPty => "mail_pty",
            InjectKind::MailNote => "mail_note",
            InjectKind::StopAdvisory => "stop_advisory",
        }
    }

    fn caps(self) -> (u32, u64) {
        match self {
            InjectKind::Compact => (COMPACT_MAX_DEFER_TURNS, COMPACT_MAX_DEFER_SECS),
            InjectKind::Restart => (RESTART_MAX_DEFER_TURNS, RESTART_MAX_DEFER_SECS),
            InjectKind::MailPty | InjectKind::MailNote => {
                (MAIL_MAX_DEFER_TURNS, MAIL_MAX_DEFER_SECS)
            }
            InjectKind::StopAdvisory => {
                (STOP_ADVISORY_MAX_DEFER_TURNS, STOP_ADVISORY_MAX_DEFER_SECS)
            }
        }
    }
}

/// Who sent the most privileged unread message. Ordered so `max` picks the
/// one deferral must respect most.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum SenderClass {
    #[default]
    None,
    System,
    Worker,
    Operator,
}

/// Classified locally from identity fields; only the class is ever sent.
/// The supervising session and an unidentified (shell) sender both count as
/// operator -- the conservative reading, since operator mail is never held.
pub(crate) fn sender_class(
    from_agent: &str,
    from_short: &str,
    parent_short: Option<&str>,
) -> SenderClass {
    if from_agent == "zirv" {
        return SenderClass::System;
    }
    if parent_short.is_some_and(|parent| parent == from_short)
        || from_agent.is_empty()
        || from_agent == "unknown"
    {
        return SenderClass::Operator;
    }
    SenderClass::Worker
}

/// Every numeric fact a call site may know; `None` is sent as null
/// ("unknown"). Deferral bookkeeping is filled in by the gate itself.
#[derive(Debug, Clone, Default)]
pub(crate) struct InjectFacts {
    pub context_pct: Option<u64>,
    pub rot_score: Option<u32>,
    /// `score.restart_at`: compact deferral ends once the score reaches it.
    pub restart_at: u32,
    pub stale_tool_tokens: Option<u64>,
    pub unread: Option<u64>,
    pub oldest_unread_age_secs: Option<u64>,
    pub sender: SenderClass,
    pub output_idle_ms: Option<u64>,
    pub tool_in_flight: Option<bool>,
    pub files_edited: Option<u64>,
    pub tests_run: Option<u64>,
    pub turns_since_user_prompt: Option<u64>,
    /// The three below are overwritten by the gate from its own ledger.
    pub turns_since_last: Option<u64>,
    pub deferred_turns: u32,
    pub deferred_secs: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Decision {
    InjectNow,
    Defer,
}

/// The hard bounds, checked before any Jev call: `true` means inject now no
/// matter what Jev would say.
pub(crate) fn cap_forces_inject(kind: InjectKind, facts: &InjectFacts) -> bool {
    let (max_turns, max_secs) = kind.caps();
    if facts.deferred_turns >= max_turns || facts.deferred_secs >= max_secs {
        return true;
    }
    match kind {
        InjectKind::MailPty | InjectKind::MailNote => {
            facts.sender == SenderClass::Operator
                || facts
                    .oldest_unread_age_secs
                    .is_some_and(|age| age >= MAIL_MAX_DEFER_SECS)
        }
        InjectKind::Compact | InjectKind::StopAdvisory => facts
            .rot_score
            .is_some_and(|score| facts.restart_at > 0 && score >= facts.restart_at),
        InjectKind::Restart => facts
            .rot_score
            .is_none_or(|score| score >= RESTART_HARD_CEILING_SCORE),
    }
}

/// Index of the first edge `value` is below -- coarse buckets keep the
/// request body (and so `jev::ask`'s cache key) stable per fact bucket.
fn bucket(value: u64, edges: &[u64]) -> u64 {
    edges.iter().take_while(|edge| value >= **edge).count() as u64
}

#[derive(Debug, Serialize)]
struct InjectAdviseState {
    #[serde(rename = "_zirv_metadata_only")]
    metadata_only: bool,
    facts: Vec<Vec<Option<u64>>>,
}

fn advise_state(kind: InjectKind, facts: &InjectFacts) -> InjectAdviseState {
    const IDLE_MS: [u64; 4] = [1_000, 5_000, 30_000, 120_000];
    const AGE_SECS: [u64; 4] = [30, 120, 300, 600];
    const TOKENS: [u64; 4] = [5_000, 20_000, 50_000, 100_000];
    let cap = |value: u64| value.min(20);
    let row = vec![
        Some(kind.code()),
        facts.context_pct.map(|pct| pct.min(100) / 10),
        facts.rot_score.map(|score| u64::from(score.min(100)) / 10),
        facts
            .stale_tool_tokens
            .map(|tokens| bucket(tokens, &TOKENS)),
        facts.unread.map(cap),
        facts
            .oldest_unread_age_secs
            .map(|age| bucket(age, &AGE_SECS)),
        Some(facts.sender as u64),
        facts.turns_since_last.map(cap),
        facts.output_idle_ms.map(|ms| bucket(ms, &IDLE_MS)),
        facts.tool_in_flight.map(u64::from),
        facts.files_edited.map(cap),
        facts.tests_run.map(cap),
        facts.turns_since_user_prompt.map(cap),
        Some(cap(u64::from(facts.deferred_turns))),
        Some(bucket(facts.deferred_secs, &AGE_SECS)),
    ];
    InjectAdviseState {
        metadata_only: true,
        facts: vec![row],
    }
}

fn questions() -> [jev::Question; 1] {
    [jev::Question::metadata_noul(
        "defer",
        "Facts [kind (0 compact, 1 restart, 2 mail line, 3 mail note, 4 stop advisory), context \
% /10, rot score /10, stale tool tokens bucket, unread, oldest unread age bucket, sender (1 \
system, 2 worker, 3 operator), turns since last such injection, output idle bucket, tool in \
flight, files edited this turn, tests run this turn, turns since user prompt, turns deferred, \
defer age bucket]; null is unknown. Is the agent mid-unit so this automatic injection should \
wait? Answer false if unsure.",
        "agent is mid-unit; wait",
        "inject now, or insufficient evidence",
    )]
}

/// One blocking decision. The caller has already checked `enabled()`.
/// Caps first (no call), then one `jev::advise`; anything but a decisive
/// `defer` is `InjectNow`.
fn decide(cfg: &CtxConfig, state: &StateDir, kind: InjectKind, facts: &InjectFacts) -> Decision {
    if cap_forces_inject(kind, facts) {
        return Decision::InjectNow;
    }
    let Some(answers) = jev::advise(
        cfg,
        state,
        SITE,
        cfg.jev.inject,
        &advise_state(kind, facts),
        &questions(),
    ) else {
        return Decision::InjectNow;
    };
    let decisive = answers.get("defer").is_some_and(|answer| {
        answer.decisive(0.0, jev::DEFAULT_MIN_MARGIN)
            && answer.as_noul().is_some_and(|p| p >= DEFER_MIN_PROBABILITY)
    });
    if !decisive {
        return Decision::InjectNow;
    }
    Decision::Defer
}

/// Records a deferral only once the caller has actually honoured it.
fn record_deferral(cfg: &CtxConfig, state: &StateDir, kind: InjectKind, facts: &InjectFacts) {
    let mut effect = jev::JevEffect::new(SITE, "deferred");
    effect.reason = Some(kind.label());
    effect.actual_count = Some(facts.deferred_turns.saturating_add(1));
    jev::record_effect(cfg, state, cfg.jev.inject, &effect);
}

/// Whether the gate is active at all. Off (or no credential) means every
/// call site runs its pre-existing path untouched.
pub(crate) fn enabled(cfg: &CtxConfig) -> bool {
    cfg.jev.inject && jev::available(&cfg.proxy.typesafe)
}

// -- hook sites: one fresh process per event, so deferral state persists --

#[derive(Debug, Default, Serialize, Deserialize)]
struct HookLedger {
    first_deferred_at: u64,
    deferred_turns: u32,
    /// Hook events left in the post-cap cooldown, during which no deferral
    /// is allowed at all.
    #[serde(default)]
    cooldown_turns: u32,
}

/// After a cap forces a hook-side injection, this many following events
/// inject without asking -- so a repeatedly-capped kind is shown at least
/// this often, never only once per cap cycle. Equal to the largest hook-kind
/// turn cap.
pub(crate) const POST_CAP_COOLDOWN_TURNS: u32 = 3;

fn write_ledger(path: &std::path::Path, ledger: &HookLedger) -> bool {
    let Some(dir) = path.parent() else {
        return false;
    };
    let Ok(text) = serde_json::to_string(ledger) else {
        return false;
    };
    state::create_private_dir_all(dir).is_ok() && state::write_private(path, &text).is_ok()
}

fn ledger_path(state: &StateDir, session_short: &str, kind: InjectKind) -> PathBuf {
    state
        .root()
        .join("jev-inject")
        .join(format!("{session_short}-{}.json", kind.label()))
}

/// Hook-side decision with persisted deferral bookkeeping. A deferral is
/// honoured only once its ledger entry is written (otherwise caps could never
/// accumulate); a present-but-unparsable ledger counts as at-cap. A cap-forced
/// injection starts a [`POST_CAP_COOLDOWN_TURNS`] cooldown; an ordinary
/// injection clears the ledger. Best-effort I/O that only ever errs toward
/// injecting.
pub(crate) fn decide_persisted(
    cfg: &CtxConfig,
    state: &StateDir,
    session_short: &str,
    kind: InjectKind,
    mut facts: InjectFacts,
    now: u64,
) -> Decision {
    let path = ledger_path(state, session_short, kind);
    let cooldown = HookLedger {
        cooldown_turns: POST_CAP_COOLDOWN_TURNS,
        ..HookLedger::default()
    };
    let ledger = match std::fs::read_to_string(&path) {
        Err(_) => None,
        Ok(text) => match serde_json::from_str::<HookLedger>(&text) {
            Ok(ledger) => Some(ledger),
            Err(_) => {
                write_ledger(&path, &cooldown);
                return Decision::InjectNow;
            }
        },
    };
    if let Some(ledger) = &ledger
        && ledger.cooldown_turns > 0
    {
        // The last cooldown event ends the episode: the next one starts fresh.
        match ledger.cooldown_turns - 1 {
            0 => {
                let _ = std::fs::remove_file(&path);
            }
            left => {
                let next = HookLedger {
                    cooldown_turns: left,
                    ..HookLedger::default()
                };
                write_ledger(&path, &next);
            }
        }
        return Decision::InjectNow;
    }
    // Only a ledger that actually recorded a deferral is an open episode.
    let episode = ledger.as_ref().filter(|ledger| ledger.deferred_turns > 0);
    if let Some(ledger) = episode {
        facts.deferred_turns = ledger.deferred_turns;
        facts.deferred_secs = now.saturating_sub(ledger.first_deferred_at);
        facts.turns_since_last = Some(u64::from(ledger.deferred_turns));
    }
    if cap_forces_inject(kind, &facts) {
        if episode.is_some() {
            write_ledger(&path, &cooldown);
        }
        return Decision::InjectNow;
    }
    if decide(cfg, state, kind, &facts) == Decision::InjectNow {
        if ledger.is_some() {
            let _ = std::fs::remove_file(&path);
        }
        return Decision::InjectNow;
    }
    let next = HookLedger {
        first_deferred_at: episode.map_or(now, |l| l.first_deferred_at),
        deferred_turns: facts.deferred_turns.saturating_add(1),
        cooldown_turns: 0,
    };
    if !write_ledger(&path, &next) {
        return Decision::InjectNow;
    }
    record_deferral(cfg, state, kind, &facts);
    Decision::Defer
}

// -- wrap sites: the pump must never block, so the call runs off-thread --

/// What the pump should do this tick.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Gate {
    Proceed,
    Hold,
}

struct Pending {
    turn: u64,
    started: Instant,
    rx: mpsc::Receiver<Decision>,
}

#[derive(Debug, Clone, Copy)]
struct Deferral {
    first_at: Instant,
    first_turn: u64,
}

/// Pump-local gate state for the wrap sites (compact, restart, mail line),
/// one slot per kind so concurrent kinds never evict each other. `turn` is
/// the supervisor's own signal count: a settled answer holds for the rest of
/// that turn and is re-asked on the next.
#[derive(Default)]
pub(crate) struct AsyncGate {
    pending: [Option<Pending>; 3],
    settled: [Option<(u64, Decision)>; 3],
    deferrals: [Option<Deferral>; 3],
    last_injected: [Option<u64>; 3],
}

fn slot(kind: InjectKind) -> usize {
    match kind {
        InjectKind::Compact => 0,
        InjectKind::Restart => 1,
        _ => 2,
    }
}

impl AsyncGate {
    /// Never blocks: a missing answer holds (exactly as `may_inject` false
    /// would) until `cfg.proxy.typesafe.timeout_secs`, then proceeds.
    pub(crate) fn check(
        &mut self,
        cfg: &CtxConfig,
        state: &StateDir,
        kind: InjectKind,
        turn: u64,
        mut facts: InjectFacts,
        now: Instant,
    ) -> Gate {
        if !enabled(cfg) {
            return Gate::Proceed;
        }
        let index = slot(kind);
        if let Some(deferral) = self.deferrals[index] {
            facts.deferred_turns =
                u32::try_from(turn.saturating_sub(deferral.first_turn)).unwrap_or(u32::MAX);
            facts.deferred_secs = now.saturating_duration_since(deferral.first_at).as_secs();
        }
        facts.turns_since_last = self.last_injected[index].map(|at| turn.saturating_sub(at));
        if cap_forces_inject(kind, &facts) {
            return Gate::Proceed;
        }
        if let Some((settled_turn, decision)) = self.settled[index]
            && settled_turn == turn
        {
            return match decision {
                Decision::InjectNow => Gate::Proceed,
                Decision::Defer => Gate::Hold,
            };
        }
        if let Some(pending) = self.pending[index].take_if(|pending| pending.turn == turn) {
            let decision = match pending.rx.try_recv() {
                Ok(decision) => decision,
                Err(mpsc::TryRecvError::Empty) => {
                    let timeout = Duration::from_secs(cfg.proxy.typesafe.timeout_secs);
                    if now.saturating_duration_since(pending.started) < timeout {
                        self.pending[index] = Some(pending);
                        return Gate::Hold;
                    }
                    Decision::InjectNow
                }
                Err(mpsc::TryRecvError::Disconnected) => Decision::InjectNow,
            };
            self.settled[index] = Some((turn, decision));
            if decision == Decision::InjectNow {
                return Gate::Proceed;
            }
            self.deferrals[index].get_or_insert(Deferral {
                first_at: now,
                first_turn: turn,
            });
            return Gate::Hold;
        }
        let (tx, rx) = mpsc::channel();
        let (cfg, state) = (cfg.clone(), state.clone());
        let spawned = std::thread::Builder::new()
            .name("zirv-jev-inject".to_string())
            .spawn(move || {
                let decision = decide(&cfg, &state, kind, &facts);
                if decision == Decision::Defer {
                    record_deferral(&cfg, &state, kind, &facts);
                }
                let _ = tx.send(decision);
            });
        if spawned.is_err() {
            return Gate::Proceed;
        }
        self.pending[index] = Some(Pending {
            turn,
            started: now,
            rx,
        });
        Gate::Hold
    }

    /// The injection landed: its deferral clock resets.
    pub(crate) fn injected(&mut self, kind: InjectKind, turn: u64) {
        let index = slot(kind);
        self.deferrals[index] = None;
        self.last_injected[index] = Some(turn);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg_with(base_url: String, credential_env: &str) -> CtxConfig {
        let mut cfg = CtxConfig::default();
        cfg.jev.inject = true;
        cfg.jev.cache_ttl_secs = 0;
        cfg.proxy.typesafe.base_url = base_url;
        cfg.proxy.typesafe.credential_env = credential_env.to_string();
        cfg.proxy.typesafe.timeout_secs = 5;
        cfg
    }

    fn mail_facts() -> InjectFacts {
        InjectFacts {
            unread: Some(1),
            oldest_unread_age_secs: Some(5),
            sender: SenderClass::Worker,
            ..InjectFacts::default()
        }
    }

    const DEFER: &str = r#"{"model": "jev-latest", "answers": {"defer": {"type": "noul", "noul": 0.95}},
        "usage": {"input_tokens": 5, "output_tokens": 0}}"#;

    #[test]
    fn inject_key_off_never_asks_and_always_proceeds() {
        let dir = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(dir.path().to_path_buf());
        let mut cfg = cfg_with("http://127.0.0.1:9".to_string(), "INJECT_GATE_TEST_OFF");
        cfg.jev.inject = false;
        // SAFETY (test-only): a unique env var name this test owns.
        unsafe { std::env::set_var("INJECT_GATE_TEST_OFF", "secret") };
        let mut gate = AsyncGate::default();
        let result = gate.check(
            &cfg,
            &state,
            InjectKind::MailPty,
            1,
            mail_facts(),
            Instant::now(),
        );
        unsafe { std::env::remove_var("INJECT_GATE_TEST_OFF") };
        assert_eq!(result, Gate::Proceed);
        assert!(!enabled(&cfg));
        assert!(!dir.path().join("jev-decisions.jsonl").exists());
    }

    #[test]
    fn inject_hard_caps_force_injection() {
        let mut facts = mail_facts();
        assert!(!cap_forces_inject(InjectKind::MailNote, &facts));
        facts.sender = SenderClass::Operator;
        assert!(cap_forces_inject(InjectKind::MailNote, &facts));
        let mut facts = mail_facts();
        facts.oldest_unread_age_secs = Some(MAIL_MAX_DEFER_SECS);
        assert!(cap_forces_inject(InjectKind::MailPty, &facts));
        let mut facts = mail_facts();
        facts.deferred_turns = MAIL_MAX_DEFER_TURNS;
        assert!(cap_forces_inject(InjectKind::MailPty, &facts));
        let mut facts = mail_facts();
        facts.deferred_secs = MAIL_MAX_DEFER_SECS;
        assert!(cap_forces_inject(InjectKind::MailPty, &facts));

        let compact = InjectFacts {
            rot_score: Some(79),
            restart_at: 80,
            ..InjectFacts::default()
        };
        assert!(!cap_forces_inject(InjectKind::Compact, &compact));
        let at_restart = InjectFacts {
            rot_score: Some(80),
            ..compact.clone()
        };
        assert!(cap_forces_inject(InjectKind::Compact, &at_restart));

        let restart = InjectFacts {
            rot_score: Some(85),
            ..InjectFacts::default()
        };
        assert!(!cap_forces_inject(InjectKind::Restart, &restart));
        let ceiling = InjectFacts {
            rot_score: Some(RESTART_HARD_CEILING_SCORE),
            ..InjectFacts::default()
        };
        assert!(cap_forces_inject(InjectKind::Restart, &ceiling));
        assert!(cap_forces_inject(
            InjectKind::Restart,
            &InjectFacts::default()
        ));
    }

    #[test]
    fn inject_sender_class_is_conservative() {
        assert_eq!(sender_class("zirv", "abcd1234", None), SenderClass::System);
        assert_eq!(
            sender_class("claude", "abcd1234", Some("abcd1234")),
            SenderClass::Operator
        );
        assert_eq!(sender_class("unknown", "x", None), SenderClass::Operator);
        assert_eq!(
            sender_class("codex", "abcd1234", Some("ffff0000")),
            SenderClass::Worker
        );
    }

    #[test]
    fn inject_request_passes_the_metadata_guard() {
        let facts = InjectFacts {
            context_pct: Some(250),
            rot_score: Some(9_999),
            stale_tool_tokens: Some(u64::MAX),
            unread: Some(u64::MAX),
            oldest_unread_age_secs: Some(u64::MAX),
            output_idle_ms: Some(u64::MAX),
            tool_in_flight: Some(true),
            files_edited: Some(u64::MAX),
            tests_run: Some(0),
            turns_since_user_prompt: None,
            deferred_turns: u32::MAX,
            deferred_secs: u64::MAX,
            ..mail_facts()
        };
        for kind in [
            InjectKind::Compact,
            InjectKind::Restart,
            InjectKind::MailPty,
            InjectKind::MailNote,
            InjectKind::StopAdvisory,
        ] {
            let value = serde_json::to_value(advise_state(kind, &facts)).expect("serialize");
            assert!(jev::safe_metadata_request(
                &value,
                &questions(),
                "jev-latest"
            ));
        }
    }

    /// Bucketing keeps the cache key per (kind, fact bucket): two idle
    /// times in the same bucket produce the same request body.
    #[test]
    fn inject_facts_in_one_bucket_share_a_request() {
        let a = InjectFacts {
            output_idle_ms: Some(6_000),
            ..mail_facts()
        };
        let b = InjectFacts {
            output_idle_ms: Some(29_000),
            ..mail_facts()
        };
        let body = |facts: &InjectFacts| {
            serde_json::to_string(&advise_state(InjectKind::MailPty, facts)).expect("json")
        };
        assert_eq!(body(&a), body(&b));
    }

    #[test]
    fn inject_async_gate_holds_then_defers_on_a_decisive_answer() {
        let (url, handle) = jev::tests::one_shot_server(200, DEFER);
        let env = "INJECT_GATE_TEST_DEFER";
        // SAFETY (test-only): a unique env var name this test owns.
        unsafe { std::env::set_var(env, "secret") };
        let cfg = cfg_with(url, env);
        let dir = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(dir.path().to_path_buf());
        let mut gate = AsyncGate::default();
        let started = Instant::now();
        let first = gate.check(&cfg, &state, InjectKind::MailPty, 1, mail_facts(), started);
        assert_eq!(first, Gate::Hold, "a pending call holds, never blocks");
        handle.join().expect("server thread");
        let mut result = Gate::Hold;
        for _ in 0..200 {
            if gate.settled[slot(InjectKind::MailPty)].is_some() {
                break;
            }
            result = gate.check(
                &cfg,
                &state,
                InjectKind::MailPty,
                1,
                mail_facts(),
                Instant::now(),
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        unsafe { std::env::remove_var(env) };
        assert_eq!(
            gate.settled[slot(InjectKind::MailPty)].map(|s| s.1),
            Some(Decision::Defer)
        );
        assert_eq!(result, Gate::Hold);
        let effects = std::fs::read_to_string(dir.path().join("jev-effects.jsonl"))
            .expect("a deferral records an effect");
        assert!(effects.contains("\"site\":\"inject\""), "{effects}");
    }

    /// A deferral can never outlive its cap: once the turn cap is spent the
    /// gate proceeds without asking again.
    #[test]
    fn inject_deferral_never_exceeds_its_cap() {
        let env = "INJECT_GATE_TEST_CAP";
        // SAFETY (test-only): a unique env var name this test owns.
        unsafe { std::env::set_var(env, "secret") };
        let cfg = cfg_with("http://127.0.0.1:9".to_string(), env);
        let dir = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(dir.path().to_path_buf());
        let mut gate = AsyncGate::default();
        let now = Instant::now();
        gate.deferrals[slot(InjectKind::MailPty)] = Some(Deferral {
            first_at: now,
            first_turn: 1,
        });
        let result = gate.check(
            &cfg,
            &state,
            InjectKind::MailPty,
            1 + u64::from(MAIL_MAX_DEFER_TURNS),
            mail_facts(),
            now,
        );
        unsafe { std::env::remove_var(env) };
        assert_eq!(result, Gate::Proceed);
        assert!(
            gate.pending.iter().all(Option::is_none),
            "a spent cap never asks Jev"
        );
    }

    #[test]
    fn inject_falls_back_to_injecting_on_a_500() {
        let (url, handle) = jev::tests::one_shot_server(500, "{}");
        let env = "INJECT_GATE_TEST_500";
        // SAFETY (test-only): a unique env var name this test owns.
        unsafe { std::env::set_var(env, "secret") };
        let cfg = cfg_with(url, env);
        let dir = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(dir.path().to_path_buf());
        let decision = decide_persisted(
            &cfg,
            &state,
            "abcd1234",
            InjectKind::MailNote,
            mail_facts(),
            100,
        );
        unsafe { std::env::remove_var(env) };
        handle.join().expect("server thread");
        assert_eq!(decision, Decision::InjectNow);
        assert!(!ledger_path(&state, "abcd1234", InjectKind::MailNote).exists());
    }

    /// Direction: Jev may only defer. An inject-now or indecisive answer is
    /// always `InjectNow`, never anything stronger.
    #[test]
    fn inject_indecisive_answer_injects() {
        let body = r#"{"model": "jev-latest", "answers": {"defer": {"type": "noul", "noul": 0.7}},
            "usage": {"input_tokens": 5, "output_tokens": 0}}"#;
        let (url, handle) = jev::tests::one_shot_server(200, body);
        let env = "INJECT_GATE_TEST_INDECISIVE";
        // SAFETY (test-only): a unique env var name this test owns.
        unsafe { std::env::set_var(env, "secret") };
        let cfg = cfg_with(url, env);
        let dir = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(dir.path().to_path_buf());
        let decision = decide(&cfg, &state, InjectKind::Compact, &InjectFacts::default());
        unsafe { std::env::remove_var(env) };
        handle.join().expect("server thread");
        assert_eq!(decision, Decision::InjectNow);
        assert!(!dir.path().join("jev-effects.jsonl").exists());
    }

    fn read_ledger(state: &StateDir, kind: InjectKind) -> HookLedger {
        let text = std::fs::read_to_string(ledger_path(state, "abcd1234", kind)).expect("ledger");
        serde_json::from_str(&text).expect("parse ledger")
    }

    /// A deferral bumps the ledger; a cap-forced injection starts the
    /// cooldown instead of wiping the count.
    #[test]
    fn inject_persisted_deferral_counts_turns_and_starts_a_cooldown_at_the_cap() {
        let (url, handle) = jev::tests::multi_shot_server(200, DEFER, 1);
        let env = "INJECT_GATE_TEST_LEDGER";
        // SAFETY (test-only): a unique env var name this test owns.
        unsafe { std::env::set_var(env, "secret") };
        let cfg = cfg_with(url, env);
        let dir = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(dir.path().to_path_buf());
        let kind = InjectKind::MailNote;
        let first = decide_persisted(&cfg, &state, "abcd1234", kind, mail_facts(), 100);
        handle.join().expect("server thread");
        assert_eq!(first, Decision::Defer);
        assert_eq!(read_ledger(&state, kind).deferred_turns, 1);
        let path = ledger_path(&state, "abcd1234", kind);
        let at_cap = HookLedger {
            first_deferred_at: 100,
            deferred_turns: MAIL_MAX_DEFER_TURNS,
            cooldown_turns: 0,
        };
        state::write_private(&path, &serde_json::to_string(&at_cap).expect("json"))
            .expect("write ledger");
        let second = decide_persisted(&cfg, &state, "abcd1234", kind, mail_facts(), 101);
        unsafe { std::env::remove_var(env) };
        assert_eq!(second, Decision::InjectNow);
        assert_eq!(
            read_ledger(&state, kind).cooldown_turns,
            POST_CAP_COOLDOWN_TURNS
        );
    }

    /// Finding 1: a corrupt ledger counts as at-cap -- inject, no Jev call.
    #[test]
    fn inject_corrupt_ledger_injects_without_asking() {
        let env = "INJECT_GATE_TEST_CORRUPT";
        // SAFETY (test-only): a unique env var name this test owns.
        unsafe { std::env::set_var(env, "secret") };
        let cfg = cfg_with("http://127.0.0.1:9".to_string(), env);
        let dir = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(dir.path().to_path_buf());
        let kind = InjectKind::StopAdvisory;
        let path = ledger_path(&state, "abcd1234", kind);
        state::create_private_dir_all(path.parent().expect("parent")).expect("dir");
        state::write_private(&path, "{not json").expect("write");
        let decision = decide_persisted(&cfg, &state, "abcd1234", kind, mail_facts(), 100);
        unsafe { std::env::remove_var(env) };
        assert_eq!(decision, Decision::InjectNow);
        assert!(!dir.path().join("jev-decisions.jsonl").exists());
    }

    /// Finding 1: a deferral whose ledger cannot be written is not honoured,
    /// so caps can never silently stop accumulating.
    #[test]
    fn inject_unpersistable_deferral_injects() {
        let (url, handle) = jev::tests::one_shot_server(200, DEFER);
        let env = "INJECT_GATE_TEST_UNWRITABLE";
        // SAFETY (test-only): a unique env var name this test owns.
        unsafe { std::env::set_var(env, "secret") };
        let cfg = cfg_with(url, env);
        let dir = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(dir.path().to_path_buf());
        // A plain file where the ledger directory belongs: every write fails.
        std::fs::write(dir.path().join("jev-inject"), "").expect("block dir");
        let decision = decide_persisted(
            &cfg,
            &state,
            "abcd1234",
            InjectKind::StopAdvisory,
            mail_facts(),
            100,
        );
        unsafe { std::env::remove_var(env) };
        handle.join().expect("server thread");
        assert_eq!(decision, Decision::InjectNow);
        assert!(
            !dir.path().join("jev-effects.jsonl").exists(),
            "an unhonoured deferral records no effect"
        );
    }

    /// Finding 2: after a cap-forced injection the next
    /// POST_CAP_COOLDOWN_TURNS events inject without asking; only then may
    /// Jev defer again.
    #[test]
    fn inject_cooldown_after_a_cap_blocks_deferral() {
        let (url, handle) = jev::tests::one_shot_server(200, DEFER);
        let env = "INJECT_GATE_TEST_COOLDOWN";
        // SAFETY (test-only): a unique env var name this test owns.
        unsafe { std::env::set_var(env, "secret") };
        let cfg = cfg_with(url, env);
        let dir = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(dir.path().to_path_buf());
        let kind = InjectKind::StopAdvisory;
        let path = ledger_path(&state, "abcd1234", kind);
        state::create_private_dir_all(path.parent().expect("parent")).expect("dir");
        let at_cap = HookLedger {
            first_deferred_at: 100,
            deferred_turns: STOP_ADVISORY_MAX_DEFER_TURNS,
            cooldown_turns: 0,
        };
        state::write_private(&path, &serde_json::to_string(&at_cap).expect("json"))
            .expect("write ledger");
        let facts = InjectFacts {
            rot_score: Some(60),
            restart_at: 80,
            ..InjectFacts::default()
        };
        let mut decisions = Vec::new();
        // A realistic clock: a zeroed `first_deferred_at` would read as a
        // decades-old episode and re-trip every seconds cap.
        let base = 1_700_000_000;
        for now in base..=(base + 1 + u64::from(POST_CAP_COOLDOWN_TURNS)) {
            decisions.push(decide_persisted(
                &cfg,
                &state,
                "abcd1234",
                kind,
                facts.clone(),
                now,
            ));
        }
        unsafe { std::env::remove_var(env) };
        handle.join().expect("server thread");
        let (forced_and_cooldown, after) = decisions.split_at(decisions.len() - 1);
        assert!(
            forced_and_cooldown
                .iter()
                .all(|d| *d == Decision::InjectNow),
            "{decisions:?}"
        );
        assert_eq!(after, [Decision::Defer], "{decisions:?}");
        let fresh = read_ledger(&state, kind);
        assert_eq!(
            (fresh.deferred_turns, fresh.first_deferred_at),
            (1, base + 1 + u64::from(POST_CAP_COOLDOWN_TURNS)),
            "the post-cooldown deferral opens a fresh episode"
        );
    }
}
