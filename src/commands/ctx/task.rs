//! Durable task cards for delegated work (issue #317): a human- and
//! agent-readable record of each unit of delegated work, with state,
//! claim/heartbeat/TTL, block-with-reason, dependency gating and an atomic
//! swarm helper -- so a fresh orchestrator (or a human) can list and pick up
//! in-flight work after a crash, the same durability goal `group.rs`'s work
//! groups and `objective.rs`'s durable objective already give their own
//! slice of a delegation's state.
//!
//! Source of truth is an append-only event log, one per repository
//! (`<state>/tasks/<repo-slug>/events.jsonl`, `StateDir::tasks`): every
//! mutation is recorded as an [`Event`] carrying the fully-decided result of
//! a pure transition, and [`materialize`] folds that log forward into the
//! current [`Card`] for every id, the same "replay to reconstruct" shape a
//! crash-resilient log demands. `materialize` is pure (no fs/clock/env/net)
//! and tolerant of a corrupt or out-of-order line, mirroring `log::
//! read_delegations`'s own best-effort contract; I/O (reading/appending the
//! log, locking, resolving `now`/pid liveness) lives only in the functions
//! below `materialize`.
//!
//! Every state transition below `materialize` (`claim`, `heartbeat`, `reap`,
//! `complete`, `block`, `unblock`, `archive`, `ready_when_parents_done`,
//! `respawn_decision`) is a pure function over a `Card` (and, where a
//! liveness question is involved, a caller-supplied `bool` -- the same
//! "caller supplies liveness" testability seam `group::is_abandoned` already
//! uses) -- never a clock read or a process probe of its own.
//! `respawn_decision_with_jev` (issue #537 A4) is the one exception: a
//! gated, best-effort wrapper that may call out to Jev, but only ever to
//! NARROW `respawn_decision`'s own pure verdict, never to override a
//! `Refuse` or to soften an `AutoBlock`/`Respawn` the other way.

use std::collections::BTreeMap;
use std::io::Write;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use super::CtxResult;
use super::config::{CtxConfig, EnvLookup};
use super::jev;
use super::state::{StateDir, create_private_dir_all, write_private};

pub const EVENTS_FILE: &str = "events.jsonl";

/// A claim's default lifetime before it is eligible for reaping, absent an
/// explicit `--ttl-secs`.
pub const DEFAULT_CLAIM_TTL_SECS: u64 = 900;

/// [`respawn_decision`]'s own default retry ceiling: a worker gets its first
/// attempt (the claim that started it) plus this many respawns before the
/// recovery policy gives up and auto-blocks the card.
pub const DEFAULT_MAX_ATTEMPTS: u32 = 2;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, clap::ValueEnum)]
#[serde(rename_all = "snake_case")]
pub enum State {
    Todo,
    Ready,
    Running,
    Blocked,
    Review,
    Done,
    Archived,
}

impl std::fmt::Display for State {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Todo => "todo",
            Self::Ready => "ready",
            Self::Running => "running",
            Self::Blocked => "blocked",
            Self::Review => "review",
            Self::Done => "done",
            Self::Archived => "archived",
        })
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Claim {
    pub session: String,
    pub pid: u32,
    /// When this platform/environment can tell (`sessions::process_start_secs`),
    /// the same recycled-pid disambiguator `sessions::Record::start_time`
    /// carries -- kept here for parity, though [`claimant_alive`] does not
    /// consult it (see that function's own doc comment for the scope cut).
    #[serde(default)]
    pub pid_start_time: Option<u64>,
    pub host: String,
    /// Refreshed by every successful [`heartbeat`] -- the "last known alive"
    /// timestamp [`is_ttl_expired`] measures a TTL from, not the original
    /// claim time.
    pub claimed_at: u64,
    pub ttl_secs: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Block {
    pub reason: String,
    pub by: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Comment {
    pub by: String,
    pub text: String,
    pub at: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Card {
    pub id: String,
    pub repo_slug: String,
    pub title: String,
    pub brief: String,
    pub state: State,
    #[serde(default)]
    pub parents: Vec<String>,
    #[serde(default)]
    pub claim: Option<Claim>,
    #[serde(default)]
    pub block: Option<Block>,
    #[serde(default)]
    pub comments: Vec<Comment>,
    #[serde(default)]
    pub workdir: Option<PathBuf>,
    #[serde(default)]
    pub group_id: Option<String>,
    #[serde(default)]
    pub outcome: Option<String>,
    #[serde(default)]
    pub attempts: u32,
    pub created_at: u64,
    pub updated_at: u64,
}

/// One durable fact appended to a repository's `events.jsonl`. Every variant
/// carries the fully-decided RESULT of a pure transition (the new claim, the
/// new outcome, ...) rather than the inputs that produced it, so
/// [`materialize`] is a dumb fold -- it never re-derives a decision (a
/// liveness probe, a clock read) that the writer already made once, honestly,
/// at append time.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Event {
    Created {
        id: String,
        repo_slug: String,
        title: String,
        brief: String,
        #[serde(default)]
        parents: Vec<String>,
        #[serde(default)]
        group_id: Option<String>,
        #[serde(default)]
        workdir: Option<PathBuf>,
        at: u64,
    },
    /// A card became `Ready` because every parent is `Done` (`ready_when_
    /// parents_done`), or because an operator forced it.
    Readied {
        id: String,
        at: u64,
    },
    Claimed {
        id: String,
        claim: Claim,
        attempts: u32,
        at: u64,
    },
    Heartbeat {
        id: String,
        claimed_at: u64,
        at: u64,
    },
    Completed {
        id: String,
        outcome: String,
        at: u64,
    },
    Blocked {
        id: String,
        reason: String,
        by: String,
        at: u64,
    },
    Unblocked {
        id: String,
        at: u64,
    },
    Commented {
        id: String,
        comment: Comment,
    },
    Archived {
        id: String,
        at: u64,
    },
    /// A claim was reaped: its pid was confirmed dead (`reap`). Returns the
    /// card to `Ready` so a fresh claim can pick it back up.
    Crash {
        id: String,
        at: u64,
    },
    /// A protocol violation short of a crash -- today, a worker exiting `0`
    /// without ever sending a report-back mail (`respawn_decision`'s
    /// `ExitKind::SilentZero`). Same effect as `Crash` (back to `Ready`), a
    /// distinct label purely for an operator reading the log to tell the two
    /// apart.
    Protocol {
        id: String,
        detail: String,
        at: u64,
    },
    /// Audit marker only: logged right after a `Crash`/`Protocol` event when
    /// the recovery policy decided to retry rather than auto-block. Carries
    /// no state of its own -- the preceding event already returned the card
    /// to `Ready`.
    Respawned {
        id: String,
        at: u64,
    },
}

fn events_path(state: &StateDir, repo_slug: &str) -> PathBuf {
    state.tasks().join(repo_slug).join(EVENTS_FILE)
}

/// One advisory OS lock per repository's event log, shared via `state::
/// acquire_lock` (issue #728) rather than a hand-rolled guard.
fn lock_tasks(state: &StateDir, repo_slug: &str) -> CtxResult<super::state::FileLock> {
    let dir = state.tasks().join(repo_slug);
    create_private_dir_all(&dir)?;
    super::state::acquire_lock(&dir.join(".lock"))
}

/// Reads every parseable line in `events.jsonl`, oldest first -- a missing
/// file is an empty list, not an error, and a corrupt line is skipped rather
/// than fatal, the same best-effort contract `log::read_delegations` gives
/// its own file.
pub fn read_events(state: &StateDir, repo_slug: &str) -> Vec<Event> {
    let Ok(text) = std::fs::read_to_string(events_path(state, repo_slug)) else {
        return Vec::new();
    };
    text.lines()
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect()
}

/// Writes one event, assuming the repository's task lock is ALREADY held by
/// the caller -- the shared body [`append_event`] and every locked
/// read-then-append helper below ([`with_task_lock`], [`write_events_atomic_
/// locked`]) call once they hold that lock, so a nested caller never has to
/// acquire the (non-reentrant) lock a second time.
fn write_event_locked(state: &StateDir, repo_slug: &str, event: &Event) -> CtxResult<()> {
    let dir = state.tasks().join(repo_slug);
    create_private_dir_all(&dir)?;
    let mut file = super::state::open_private_append(&dir.join(EVENTS_FILE))?;
    writeln!(file, "{}", serde_json::to_string(event)?)?;
    Ok(())
}

/// Appends one event under the repository's lock.
pub fn append_event(state: &StateDir, repo_slug: &str, event: &Event) -> CtxResult<()> {
    let _lock = lock_tasks(state, repo_slug)?;
    write_event_locked(state, repo_slug, event)
}

/// Appends every event in `events` as ONE atomic batch: builds the whole new
/// file contents (existing lines plus the new ones) in memory, then writes it
/// via `write_private`'s temp-sibling-then-rename swap -- so a crash or a
/// failure partway through BUILDING the batch (before this is ever called)
/// touches disk not at all, and a crash during the swap itself leaves either
/// the whole old file or the whole new one, never a partial mix of the two.
/// `zirv ctx swarm` is the one caller that needs this: a root, N workers, a
/// verifier and a synthesizer must all land together, or none of them, so an
/// interruption never leaves an orphaned partial batch on disk.
pub fn append_events_atomic(state: &StateDir, repo_slug: &str, events: &[Event]) -> CtxResult<()> {
    let _lock = lock_tasks(state, repo_slug)?;
    write_events_atomic_locked(state, repo_slug, events)
}

/// The body of [`append_events_atomic`], assuming the repository's task lock
/// is ALREADY held -- see [`write_event_locked`]'s own doc comment for why
/// this split exists.
fn write_events_atomic_locked(
    state: &StateDir,
    repo_slug: &str,
    events: &[Event],
) -> CtxResult<()> {
    let dir = state.tasks().join(repo_slug);
    create_private_dir_all(&dir)?;
    let path = dir.join(EVENTS_FILE);
    // E-4: a missing file is legitimately empty (nothing has ever been
    // appended yet), but any OTHER read error -- e.g. one invalid UTF-8
    // byte from a torn append -- must not be treated the same way.
    // `unwrap_or_default()` used to collapse both cases to "empty", so a
    // single corrupt byte anywhere in the existing log silently wiped
    // every prior event the next time this ran.
    let mut content = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => {
            return Err(format!(
                "{}: exists but cannot be read ({e}); refusing to overwrite the existing event \
                 log with only this batch",
                path.display()
            )
            .into());
        }
    };
    // A prior write that did not end in a newline (e.g. a torn append)
    // would otherwise glue its last line to the first event appended here.
    if !content.is_empty() && !content.ends_with('\n') {
        content.push('\n');
    }
    for event in events {
        content.push_str(&serde_json::to_string(event)?);
        content.push('\n');
    }
    write_private(&path, &content)?;
    Ok(())
}

/// Folds a repository's event log into the current [`Card`] for every id.
/// Pure: no fs/clock/env/net, and tolerant of a line that references an
/// unknown id (skipped) or a duplicate `Created` for an id already seen
/// (skipped, first write wins) -- the same "a corrupt or out-of-order write
/// must never break every OTHER card's reconstruction" discipline `group::
/// list`/`log::read_delegations` already hold for their own on-disk state.
pub fn materialize(events: &[Event]) -> BTreeMap<String, Card> {
    let mut cards: BTreeMap<String, Card> = BTreeMap::new();
    for event in events {
        match event {
            Event::Created {
                id,
                repo_slug,
                title,
                brief,
                parents,
                group_id,
                workdir,
                at,
            } => {
                if cards.contains_key(id) {
                    continue;
                }
                let state = if parents.is_empty() {
                    State::Ready
                } else {
                    State::Todo
                };
                cards.insert(
                    id.clone(),
                    Card {
                        id: id.clone(),
                        repo_slug: repo_slug.clone(),
                        title: title.clone(),
                        brief: brief.clone(),
                        state,
                        parents: parents.clone(),
                        claim: None,
                        block: None,
                        comments: Vec::new(),
                        workdir: workdir.clone(),
                        group_id: group_id.clone(),
                        outcome: None,
                        attempts: 0,
                        created_at: *at,
                        updated_at: *at,
                    },
                );
            }
            Event::Readied { id, at } => {
                if let Some(card) = cards.get_mut(id) {
                    card.state = State::Ready;
                    card.updated_at = *at;
                }
            }
            Event::Claimed {
                id,
                claim,
                attempts,
                at,
            } => {
                if let Some(card) = cards.get_mut(id) {
                    card.state = State::Running;
                    card.claim = Some(claim.clone());
                    card.attempts = *attempts;
                    card.updated_at = *at;
                }
            }
            Event::Heartbeat { id, claimed_at, at } => {
                if let Some(card) = cards.get_mut(id) {
                    if let Some(claim) = card.claim.as_mut() {
                        claim.claimed_at = *claimed_at;
                    }
                    card.updated_at = *at;
                }
            }
            Event::Completed { id, outcome, at } => {
                if let Some(card) = cards.get_mut(id) {
                    card.state = State::Done;
                    card.outcome = Some(outcome.clone());
                    card.claim = None;
                    card.updated_at = *at;
                }
            }
            Event::Blocked { id, reason, by, at } => {
                if let Some(card) = cards.get_mut(id) {
                    card.state = State::Blocked;
                    card.block = Some(Block {
                        reason: reason.clone(),
                        by: by.clone(),
                    });
                    card.claim = None;
                    card.updated_at = *at;
                }
            }
            Event::Unblocked { id, at } => {
                if let Some(card) = cards.get_mut(id) {
                    card.state = State::Ready;
                    card.block = None;
                    card.updated_at = *at;
                }
            }
            Event::Commented { id, comment } => {
                if let Some(card) = cards.get_mut(id) {
                    card.updated_at = comment.at;
                    card.comments.push(comment.clone());
                }
            }
            Event::Archived { id, at } => {
                if let Some(card) = cards.get_mut(id) {
                    card.state = State::Archived;
                    card.updated_at = *at;
                }
            }
            Event::Crash { id, at } | Event::Protocol { id, at, .. } => {
                if let Some(card) = cards.get_mut(id) {
                    card.state = State::Ready;
                    card.claim = None;
                    card.updated_at = *at;
                }
            }
            Event::Respawned { id, at } => {
                if let Some(card) = cards.get_mut(id) {
                    card.updated_at = *at;
                }
            }
        }
    }
    cards
}

/// [`read_events`] then [`materialize`] -- the read path every verb below
/// uses.
pub fn load_cards(state: &StateDir, repo_slug: &str) -> BTreeMap<String, Card> {
    materialize(&read_events(state, repo_slug))
}

// -- Pure transitions -----------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refusal {
    NotReady(State),
    ParentsUnmet(Vec<String>),
    NotClaimed,
    WrongClaimant(String),
    ClaimantDead,
    /// [`block`]: refused off anything but Todo/Ready/Running/Review.
    CannotBlock(State),
    /// [`unblock`]: refused off anything but Blocked.
    CannotUnblock(State),
    /// [`archive`]: refused off anything but Done or Blocked.
    CannotArchive(State),
}

impl std::fmt::Display for Refusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotReady(state) => write!(f, "not ready (state is {state})"),
            Self::ParentsUnmet(parents) => {
                write!(f, "parents not all done: {}", parents.join(", "))
            }
            Self::NotClaimed => write!(f, "not claimed"),
            Self::WrongClaimant(session) => {
                write!(f, "claimed by a different session ({session})")
            }
            Self::ClaimantDead => write!(f, "claimant is no longer alive"),
            Self::CannotBlock(state) => write!(f, "cannot block from state {state}"),
            Self::CannotUnblock(state) => write!(f, "cannot unblock from state {state}"),
            Self::CannotArchive(state) => write!(f, "cannot archive from state {state}"),
        }
    }
}

impl std::error::Error for Refusal {}

/// Refuses unless `card.state` is `Ready` AND `parents_done` -- both are
/// checked independently of one another, so a card whose `state` field says
/// `Ready` (however it got there -- an automatic `ready_when_parents_done`
/// promotion, or an operator forcing it by hand) is still refused if the
/// caller's own resolution of its parents says otherwise. `parents_done` is
/// the caller's job to compute (materialize the parents, check every one is
/// `Done`) -- this function never reads another card itself.
#[allow(clippy::too_many_arguments)]
pub fn claim(
    card: &Card,
    session: &str,
    pid: u32,
    pid_start_time: Option<u64>,
    host: &str,
    now: u64,
    ttl_secs: u64,
    parents_done: bool,
) -> Result<Card, Refusal> {
    if card.state != State::Ready {
        return Err(Refusal::NotReady(card.state));
    }
    if !parents_done {
        return Err(Refusal::ParentsUnmet(card.parents.clone()));
    }
    let mut next = card.clone();
    next.state = State::Running;
    next.claim = Some(Claim {
        session: session.to_string(),
        pid,
        pid_start_time,
        host: host.to_string(),
        claimed_at: now,
        ttl_secs,
    });
    next.attempts = next.attempts.saturating_add(1);
    next.updated_at = now;
    Ok(next)
}

/// Extends the claim's freshness (`claimed_at = now`) only when `session`
/// matches the current claimant AND `claimant_alive` -- a dead claimant's
/// heartbeat is refused rather than resuscitating a claim [`reap`] would
/// otherwise be free to take back.
pub fn heartbeat(
    card: &Card,
    session: &str,
    now: u64,
    claimant_alive: bool,
) -> Result<Card, Refusal> {
    let Some(existing) = &card.claim else {
        return Err(Refusal::NotClaimed);
    };
    if existing.session != session {
        return Err(Refusal::WrongClaimant(existing.session.clone()));
    }
    if !claimant_alive {
        return Err(Refusal::ClaimantDead);
    }
    let mut next = card.clone();
    if let Some(claim) = next.claim.as_mut() {
        claim.claimed_at = now;
    }
    next.updated_at = now;
    Ok(next)
}

/// Whether a claim's TTL has elapsed since its last heartbeat -- pure
/// arithmetic over `now`/`claim.claimed_at`/`claim.ttl_secs`, with no
/// bearing on whether [`reap`] will actually act (that depends solely on
/// liveness; see its own doc comment).
pub fn is_ttl_expired(claim: &Claim, now: u64) -> bool {
    now.saturating_sub(claim.claimed_at) > claim.ttl_secs
}

/// Reaps a `Running` card's claim back to `Ready` when its claimant is
/// confirmed DEAD -- and only then: a live claimant is never reaped here no
/// matter how far past its TTL it is (a caller decides WHEN it is worth
/// probing liveness at all, typically by checking [`is_ttl_expired`] first;
/// this function's own gate is liveness, full stop, so a slow-but-alive
/// worker is never yanked out from under itself).
pub fn reap(card: &Card, now: u64, claimant_alive: bool) -> Option<Card> {
    if card.state != State::Running || card.claim.is_none() || claimant_alive {
        return None;
    }
    let mut next = card.clone();
    next.state = State::Ready;
    next.claim = None;
    next.updated_at = now;
    Some(next)
}

pub fn complete(card: &Card, outcome: &str, now: u64) -> Card {
    let mut next = card.clone();
    next.state = State::Done;
    next.outcome = Some(outcome.to_string());
    next.claim = None;
    next.updated_at = now;
    next
}

/// Refuses unless `card.state` is one of Todo/Ready/Running/Review -- an
/// already-`Blocked`, `Done`, or `Archived` card cannot be blocked again (or
/// at all, once terminal).
pub fn block(card: &Card, reason: &str, by: &str, now: u64) -> Result<Card, Refusal> {
    if !matches!(
        card.state,
        State::Todo | State::Ready | State::Running | State::Review
    ) {
        return Err(Refusal::CannotBlock(card.state));
    }
    let mut next = card.clone();
    next.state = State::Blocked;
    next.block = Some(Block {
        reason: reason.to_string(),
        by: by.to_string(),
    });
    next.claim = None;
    next.updated_at = now;
    Ok(next)
}

/// Refuses unless `card.state` is `Blocked` -- there is nothing to clear off
/// any other state.
pub fn unblock(card: &Card, now: u64) -> Result<Card, Refusal> {
    if card.state != State::Blocked {
        return Err(Refusal::CannotUnblock(card.state));
    }
    let mut next = card.clone();
    next.state = State::Ready;
    next.block = None;
    next.updated_at = now;
    Ok(next)
}

/// Refuses unless `card.state` is `Done` or `Blocked` -- a card still
/// actively in flight (Todo/Ready/Running/Review) is never archived out from
/// under it.
pub fn archive(card: &Card, now: u64) -> Result<Card, Refusal> {
    if !matches!(card.state, State::Done | State::Blocked) {
        return Err(Refusal::CannotArchive(card.state));
    }
    let mut next = card.clone();
    next.state = State::Archived;
    next.updated_at = now;
    Ok(next)
}

pub fn add_comment(card: &Card, by: &str, text: &str, now: u64) -> Card {
    let mut next = card.clone();
    next.comments.push(Comment {
        by: by.to_string(),
        text: text.to_string(),
        at: now,
    });
    next.updated_at = now;
    next
}

/// Promotes a `Todo` card to `Ready` once every one of its `parents` has
/// resolved (`parents` must be the FULL set the caller looked up -- a
/// missing lookup for any one of `card.parents` reads as "not done", never
/// as vacuously satisfied) and is itself `Done`. `None` for anything else:
/// a card not in `Todo`, or with an unmet or unresolved parent.
pub fn ready_when_parents_done(card: &Card, parents: &[&Card], now: u64) -> Option<Card> {
    if card.state != State::Todo {
        return None;
    }
    if parents.len() != card.parents.len() {
        return None;
    }
    if !parents.iter().all(|p| p.state == State::Done) {
        return None;
    }
    let mut next = card.clone();
    next.state = State::Ready;
    next.updated_at = now;
    Some(next)
}

// -- Recovery policy --------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExitKind {
    Crash,
    SilentZero,
    Reported,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RespawnVerdict {
    Respawn,
    AutoBlock(String),
    Refuse(String),
}

/// Whether a worker whose supervised run just ended should be respawned onto
/// its task card, auto-blocked, or refused outright -- named `respawn_
/// decision` here (the shape Hermes calls `respawn_guarded`): `SilentZero`
/// (exited `0` without ever sending a report-back) and `Crash` both retry up
/// to `max_attempts` times before giving up with `AutoBlock` -- NEVER marking
/// the card `Done` silently, which is the whole point: only an explicit
/// [`complete`] from a real report-back does that. `Refuse` covers the two
/// cases retrying can only make worse: the card already succeeded (`Done`),
/// or it is already blocked on something a respawn cannot fix (an auth/
/// credentials reason).
pub fn respawn_decision(card: &Card, exit: ExitKind, max_attempts: u32) -> RespawnVerdict {
    if card.state == State::Done {
        return RespawnVerdict::Refuse("card already completed successfully".to_string());
    }
    if let Some(block) = &card.block {
        let lower = block.reason.to_lowercase();
        if lower.contains("auth") || lower.contains("credential") {
            return RespawnVerdict::Refuse(format!(
                "blocked on '{}': not retryable by respawning",
                block.reason
            ));
        }
    }
    match exit {
        ExitKind::Reported => {
            RespawnVerdict::Refuse("the worker already reported an outcome".to_string())
        }
        ExitKind::Crash => {
            if card.attempts >= max_attempts {
                RespawnVerdict::AutoBlock(format!(
                    "crashed, retried the maximum of {max_attempts} times"
                ))
            } else {
                RespawnVerdict::Respawn
            }
        }
        ExitKind::SilentZero => {
            if card.attempts >= max_attempts {
                RespawnVerdict::AutoBlock(format!(
                    "exited 0 without a report-back, retried the maximum of {max_attempts} times"
                ))
            } else {
                RespawnVerdict::Respawn
            }
        }
    }
}

fn exit_kind_code(exit: ExitKind) -> u32 {
    match exit {
        ExitKind::Crash => 1,
        ExitKind::SilentZero => 2,
        ExitKind::Reported => 3,
    }
}

/// Issue #537 (A4): from a live 2026-09-18 probe -- 10/12 correct, the
/// highest-scoring wrong answer at 0.88.
const CRASH_TRIAGE_FLOOR: f32 = 0.9;

#[derive(Debug, Serialize)]
struct CrashAdviseState {
    #[serde(rename = "_zirv_metadata_only")]
    metadata_only: bool,
    facts: Vec<Vec<u32>>,
}

enum CrashAdvice {
    AutoBlock(&'static str),
    Baseline(&'static str),
}

#[derive(Clone, Copy)]
pub(crate) struct CrashSignals {
    access: bool,
    configuration: bool,
    missing_file: bool,
    transient: bool,
}

impl CrashSignals {
    pub(crate) fn from_text(text: &str) -> Self {
        let lower: String = text
            .chars()
            .take(4096)
            .collect::<String>()
            .to_ascii_lowercase();
        let contains_any = |phrases: &[&str]| phrases.iter().any(|phrase| lower.contains(phrase));
        Self {
            access: contains_any(&[
                "token expired",
                "login again",
                "permission denied",
                "access denied",
            ]),
            configuration: contains_any(&[
                "invalid config",
                "invalid setup",
                "parse error",
                "syntax error",
            ]) || (lower.contains("setup") && lower.contains("invalid")),
            missing_file: contains_any(&["missing file", "file not found"]),
            transient: contains_any(&[
                "timeout",
                "network",
                "rate limit",
                "out of memory",
                "connection reset",
            ]),
        }
    }
}

/// Asks Jev only when local, repeatable failure signals support an early
/// block. The request contains numeric signal categories, not the worker's
/// freeform failure reason; uncertainty preserves the baseline retry.
fn jev_crash_cause(
    cfg: &CtxConfig,
    state: &StateDir,
    signals: CrashSignals,
    exit: ExitKind,
    attempt: u32,
    max_attempts: u32,
) -> CrashAdvice {
    if !cfg.jev.supervisor {
        return CrashAdvice::Baseline("disabled");
    }
    if !jev::available(&cfg.proxy.typesafe) {
        return CrashAdvice::Baseline("missing_credential");
    }
    let access_signal = signals.access;
    let configuration_signal = signals.configuration;
    let missing_file_signal = signals.missing_file;
    let transient_signal = signals.transient;
    if transient_signal || !(access_signal || configuration_signal || missing_file_signal) {
        return CrashAdvice::Baseline("insufficient_repeatable_signal");
    }
    let advise_state = CrashAdviseState {
        metadata_only: true,
        facts: vec![vec![
            exit_kind_code(exit),
            attempt.min(1_000_000),
            max_attempts.min(1_000_000),
            u32::from(access_signal),
            u32::from(configuration_signal),
            u32::from(missing_file_signal),
            u32::from(transient_signal),
        ]],
    };
    let questions = [jev::Question::metadata_choice(
        "cause",
        "Classify the failure from coarse local signals only; uncertainty means transient.",
        &[
            (
                "transient",
                "network, timeout, out of memory, crash or rate limit that a fresh attempt may pass",
            ),
            (
                "access",
                "authentication, authorization, credential, login, quota or permission problem a \
                 retry cannot fix",
            ),
            (
                "deterministic",
                "a bug, compile error or missing file that will recur identically",
            ),
        ],
    )];
    let answers = match jev::advise_detailed(
        cfg,
        state,
        "crash",
        cfg.jev.supervisor,
        &advise_state,
        &questions,
    ) {
        jev::AdvisoryStatus::Answered(answers) => answers,
        jev::AdvisoryStatus::Disabled => return CrashAdvice::Baseline("disabled"),
        jev::AdvisoryStatus::MissingCredential => {
            return CrashAdvice::Baseline("missing_credential");
        }
        jev::AdvisoryStatus::Failed => return CrashAdvice::Baseline("error"),
    };
    let Some(answer) = answers.get("cause") else {
        return CrashAdvice::Baseline("partial_answer");
    };
    if !answer.decisive(CRASH_TRIAGE_FLOOR, jev::DEFAULT_MIN_MARGIN) {
        return CrashAdvice::Baseline("uncertain");
    }
    match answer.as_choice() {
        Some("access") if access_signal => CrashAdvice::AutoBlock("access"),
        Some("deterministic") if configuration_signal || missing_file_signal => {
            CrashAdvice::AutoBlock("deterministic")
        }
        Some("transient") => CrashAdvice::Baseline("transient"),
        _ => CrashAdvice::Baseline("signal_mismatch"),
    }
}

/// Preserves every deterministic refusal and attempt cap before asking Jev.
/// Jev can narrow an eligible respawn to an early block, never permit one.
pub(crate) fn respawn_decision_with_jev(
    cfg: &CtxConfig,
    state: &StateDir,
    card: &Card,
    exit: ExitKind,
    max_attempts: u32,
) -> RespawnVerdict {
    respawn_decision_with_jev_signals(cfg, state, card, exit, max_attempts, None)
}

pub(crate) fn respawn_decision_with_jev_signals(
    cfg: &CtxConfig,
    state: &StateDir,
    card: &Card,
    exit: ExitKind,
    max_attempts: u32,
    failure_signals: Option<CrashSignals>,
) -> RespawnVerdict {
    let base = respawn_decision(card, exit, max_attempts);
    if !matches!(base, RespawnVerdict::Respawn) {
        return base;
    }
    let Some(signals) = card
        .block
        .as_ref()
        .map(|block| CrashSignals::from_text(&block.reason))
        .or(failure_signals)
    else {
        return base;
    };
    let advice = jev_crash_cause(cfg, state, signals, exit, card.attempts, max_attempts);
    let mut effect = jev::JevEffect::new("crash", "retry_decision");
    effect.subject_id = Some(&card.id);
    effect.baseline_count = Some(1);
    match advice {
        CrashAdvice::AutoBlock(cause) => {
            effect.reason = Some(cause);
            effect.outcome = Some("auto_block");
            jev::record_effect(cfg, state, cfg.jev.supervisor, &effect);
            RespawnVerdict::AutoBlock(format!(
                "blocked after a likely {cause} failure that a retry cannot fix; \
                 inspect with `zirv ctx task show {}` and resume with `zirv ctx task unblock {}`",
                card.id, card.id
            ))
        }
        CrashAdvice::Baseline(reason) => {
            effect.reason = Some(reason);
            effect.outcome = Some("baseline_retry");
            jev::record_effect(cfg, state, cfg.jev.supervisor, &effect);
            base
        }
    }
}

/// Formats `--task`'s own labelled block: `card.brief` plus every resolved
/// parent's own `outcome` -- appended after the operator's own prompt text by
/// `agent::attach_task_context_to_prompt`. Pure formatting only; `parents` is
/// whatever the caller already resolved (a parent id with no matching card is
/// simply absent from the list, same "caller resolves, this only formats"
/// split `ready_when_parents_done` draws).
///
/// Issue #326 B1: every ancestor's `outcome` used to be appended verbatim,
/// uncapped -- a task tree a few levels deep could inject an unbounded amount
/// of prior prose into a fresh worker's very first turn. `max_parent_
/// outcome_bytes` (`cfg.task.max_parent_outcome_bytes`, the caller's own
/// resolved config) now bounds the combined block: parents are rendered most
/// recently updated (`Card::updated_at`) first, in full, for as long as the
/// budget lasts. Once one parent's own line would not fit what remains, EVERY
/// parent from that point on is left out too, even an older one whose own
/// line happens to be small enough to have fit on its own -- recency order is
/// a priority, not a bin-packing problem -- and a single trailing line names
/// exactly how many bytes and how many parents were cut, never a silent one.
pub fn compile_task_prompt(
    card: &Card,
    parents: &[&Card],
    max_parent_outcome_bytes: usize,
) -> String {
    let mut out = format!("\n\n## TASK CARD {}\n{}\n", card.id, card.brief);
    if !parents.is_empty() {
        out.push_str("\n## PARENT OUTCOMES\n");
        let mut ordered: Vec<&Card> = parents.to_vec();
        ordered.sort_by_key(|c| std::cmp::Reverse(c.updated_at));
        let mut spent = 0usize;
        let mut cut_off = false;
        let mut omitted_count = 0usize;
        let mut omitted_bytes = 0usize;
        for parent in ordered {
            let line = format!(
                "- {} ({}): {}\n",
                parent.id,
                parent.title,
                parent.outcome.as_deref().unwrap_or("(no outcome recorded)")
            );
            if !cut_off && spent.saturating_add(line.len()) <= max_parent_outcome_bytes {
                spent += line.len();
                out.push_str(&line);
            } else {
                cut_off = true;
                omitted_count += 1;
                omitted_bytes += line.len();
            }
        }
        if omitted_count > 0 {
            out.push_str(&format!(
                "- [truncated {omitted_bytes} bytes: {omitted_count} older parent outcome(s) omitted to fit the {max_parent_outcome_bytes} byte budget]\n"
            ));
        }
    }
    out
}

// -- I/O-facing helpers -----------------------------------------------------

pub(crate) fn local_host() -> String {
    std::env::var("COMPUTERNAME")
        .or_else(|_| std::env::var("HOSTNAME"))
        .unwrap_or_else(|_| "local".to_string())
}

fn resolve_session(explicit: &Option<String>, env: EnvLookup<'_>) -> String {
    explicit
        .clone()
        .or_else(|| super::mail::session_identity(env))
        .unwrap_or_else(|| {
            super::sessions::short_id(&super::event::SessionId::new_v4().to_string())
        })
}

/// Whether a claim's process is still alive, per `sessions::is_alive`'s bare
/// pid check. Deliberately does NOT add `sessions::Record`'s own recycled-pid
/// start-time disambiguation (issue #152): that machinery lives on `Record`,
/// which a task claim does not carry the rest of, and a task claim's own
/// `pid_start_time` is kept only for parity today (see [`Claim::pid_start_
/// time`]'s doc comment) rather than wired into a second, drifting
/// disambiguator.
fn claimant_alive(claim: &Claim) -> bool {
    super::sessions::is_alive(claim.pid)
}

/// Issue #317: one `task:<id> <state> -- <reason>` line per still-open card
/// (anything short of `Done`/`Archived`) in `repo`, for `handoff.rs`'s own
/// Blocked section (`with_open_task_cards`) and `status.rs`'s tasks section.
/// `<reason>` is the block reason when the card is actually `Blocked`, else a
/// short description of why it is still open -- never a guess at what will
/// unblock it, only what state it is actually in. Sorted by id (the same
/// order `load_cards`'s `BTreeMap` already gives), so the output is
/// deterministic across calls.
pub fn open_card_lines(state: &StateDir, repo: &std::path::Path) -> Vec<String> {
    let repo_slug = super::state::repo_slug(repo);
    load_cards(state, &repo_slug)
        .into_values()
        .filter(|card| !matches!(card.state, State::Done | State::Archived))
        .map(|card| {
            let reason = match (&card.state, &card.block) {
                (State::Blocked, Some(block)) => block.reason.clone(),
                (State::Todo, _) => "waiting on parents".to_string(),
                (State::Ready, _) => "not yet claimed".to_string(),
                (State::Running, _) => "in progress".to_string(),
                (State::Review, _) => "awaiting review".to_string(),
                (State::Blocked, None) => "blocked".to_string(),
                (State::Done | State::Archived, _) => unreachable!("filtered out above"),
            };
            format!("task:{} {} -- {reason}", card.id, card.state)
        })
        .collect()
}

fn resolve_repo_slug() -> CtxResult<String> {
    Ok(super::state::repo_slug(&std::env::current_dir()?))
}

// -- CLI ---------------------------------------------------------------------

#[derive(Debug, clap::Args)]
pub struct TaskArgs {
    #[command(subcommand)]
    pub command: TaskVerb,
}

#[derive(Debug, clap::Subcommand)]
pub enum TaskVerb {
    /// Create a new task card.
    Create(CreateArgs),
    /// List every task card for this repository.
    List(ListArgs),
    /// Show one task card.
    Show(ShowArgs),
    /// Claim a `Ready` card whose parents are all `Done`.
    Claim(ClaimArgs),
    /// Extend a held claim's TTL.
    Heartbeat(HeartbeatArgs),
    /// Mark a card `Done` with its outcome.
    Complete(CompleteArgs),
    /// Block a card with a reason.
    Block(BlockArgs),
    /// Clear a card's block, returning it to `Ready`.
    Unblock(UnblockArgs),
    /// Leave a comment on a card.
    Comment(CommentArgs),
    /// Archive a card.
    Archive(ArchiveArgs),
}

#[derive(Debug, clap::Args)]
pub struct CreateArgs {
    /// A short human title for the card.
    pub title: String,
    /// The full task brief -- what a worker claiming this card is told to do.
    #[arg(long)]
    pub brief: String,
    /// A parent card id this card depends on, repeatable. Every parent must
    /// be `Done` before this card can be claimed.
    #[arg(long = "parent")]
    pub parents: Vec<String>,
    #[arg(long)]
    pub group: Option<String>,
    #[arg(long)]
    pub workdir: Option<PathBuf>,
}

#[derive(Debug, clap::Args)]
pub struct ListArgs {
    #[arg(long, default_value_t = false)]
    pub json: bool,
}

#[derive(Debug, clap::Args)]
pub struct ShowArgs {
    pub id: String,
    #[arg(long, default_value_t = false)]
    pub json: bool,
}

#[derive(Debug, clap::Args)]
pub struct ClaimArgs {
    pub id: String,
    /// Overrides the session identity this claim is recorded under; unstated
    /// resolves from the environment, else a freshly minted id.
    #[arg(long)]
    pub session: Option<String>,
    #[arg(long)]
    pub ttl_secs: Option<u64>,
}

#[derive(Debug, clap::Args)]
pub struct HeartbeatArgs {
    pub id: String,
    #[arg(long)]
    pub session: Option<String>,
}

#[derive(Debug, clap::Args)]
pub struct CompleteArgs {
    pub id: String,
    pub outcome: String,
}

#[derive(Debug, clap::Args)]
pub struct BlockArgs {
    pub id: String,
    pub reason: String,
    #[arg(long)]
    pub by: Option<String>,
}

#[derive(Debug, clap::Args)]
pub struct UnblockArgs {
    pub id: String,
}

#[derive(Debug, clap::Args)]
pub struct CommentArgs {
    pub id: String,
    pub text: String,
    #[arg(long)]
    pub by: Option<String>,
}

#[derive(Debug, clap::Args)]
pub struct ArchiveArgs {
    pub id: String,
}

#[derive(Debug, clap::Args)]
pub struct SwarmArgs {
    /// What this swarm of delegated work is for.
    pub scope: String,
    #[arg(long, default_value_t = 1)]
    pub workers: u32,
    #[arg(long)]
    pub group: Option<String>,
}

/// Mints one card and appends its `Created` event, returning the new id.
///
/// Split out of [`run_create`] for issue #485 (roadmap N16): a native
/// coordinator's `task_create` tool mints a card for a repository it knows by
/// path, not by the process's current directory, and both surfaces have to
/// write the same event through the same append. `repo_slug` is therefore a
/// parameter rather than resolved here.
pub fn create_card(
    state: &StateDir,
    repo_slug: &str,
    args: &CreateArgs,
    now: u64,
) -> CtxResult<String> {
    let id = format!("task-{}", uuid::Uuid::new_v4());
    let event = Event::Created {
        id: id.clone(),
        repo_slug: repo_slug.to_string(),
        title: args.title.clone(),
        brief: args.brief.clone(),
        parents: args.parents.clone(),
        group_id: args.group.clone(),
        workdir: args.workdir.clone(),
        at: now,
    };
    append_event(state, repo_slug, &event)?;
    Ok(id)
}

pub fn run_create<W: Write>(
    state: &StateDir,
    w: &mut W,
    args: &CreateArgs,
    now: u64,
) -> CtxResult<String> {
    super::seat::fence(state)?;
    let repo_slug = resolve_repo_slug()?;
    let id = create_card(state, &repo_slug, args, now)?;
    writeln!(w, "{id}")?;
    Ok(id)
}

fn print_card_line<W: Write>(w: &mut W, card: &Card, now: u64) -> CtxResult<()> {
    write!(w, "{} [{}] {}", card.id, card.state, card.title)?;
    if let Some(claim) = &card.claim {
        write!(
            w,
            " claimed-by={} age={}s",
            claim.session,
            now.saturating_sub(claim.claimed_at)
        )?;
    }
    if let Some(block) = &card.block {
        write!(w, " blocked: {}", block.reason)?;
    }
    writeln!(w)?;
    Ok(())
}

pub fn run_list<W: Write>(
    state: &StateDir,
    w: &mut W,
    args: &ListArgs,
    now: u64,
) -> CtxResult<i32> {
    let repo_slug = resolve_repo_slug()?;
    let cards = load_cards(state, &repo_slug);
    if args.json {
        let list: Vec<&Card> = cards.values().collect();
        writeln!(w, "{}", serde_json::to_string(&list)?)?;
        return Ok(0);
    }
    if cards.is_empty() {
        writeln!(w, "no tasks")?;
        return Ok(0);
    }
    for card in cards.values() {
        print_card_line(w, card, now)?;
    }
    Ok(0)
}

pub fn run_show<W: Write>(
    state: &StateDir,
    w: &mut W,
    args: &ShowArgs,
    now: u64,
) -> CtxResult<i32> {
    let repo_slug = resolve_repo_slug()?;
    let cards = load_cards(state, &repo_slug);
    let Some(card) = cards.get(&args.id) else {
        writeln!(w, "no task '{}'", args.id)?;
        return Ok(1);
    };
    if args.json {
        writeln!(w, "{}", serde_json::to_string(card)?)?;
        return Ok(0);
    }
    print_card_line(w, card, now)?;
    if !card.brief.is_empty() {
        writeln!(w, "brief: {}", card.brief)?;
    }
    if !card.parents.is_empty() {
        writeln!(w, "parents: {}", card.parents.join(", "))?;
    }
    if let Some(outcome) = &card.outcome {
        writeln!(w, "outcome: {outcome}")?;
    }
    for comment in &card.comments {
        writeln!(w, "comment ({}): {}", comment.by, comment.text)?;
    }
    Ok(0)
}

/// Auto-promotes `card` from `Todo` to `Ready` first (appending a `Readied`
/// event) when every parent is now `Done` -- a fresh orchestrator picking up
/// work after a crash must not have to run a separate "ready" step by hand
/// before `claim` can succeed.
/// Pure: [`promote_if_ready`]'s own decision, minus the I/O -- appends a
/// [`Event::Readied`] to `events` (for the caller to persist) instead of
/// writing it itself, so a locked caller ([`with_task_lock`]) can fold this
/// into the SAME atomic batch as whatever it decides next, rather than each
/// step taking and releasing the lock separately.
fn promote_if_ready_pure(
    cards: &BTreeMap<String, Card>,
    card: &Card,
    now: u64,
    events: &mut Vec<Event>,
) -> Card {
    if card.state != State::Todo {
        return card.clone();
    }
    let parents: Vec<&Card> = card.parents.iter().filter_map(|id| cards.get(id)).collect();
    match ready_when_parents_done(card, &parents, now) {
        Some(promoted) => {
            events.push(Event::Readied {
                id: card.id.clone(),
                at: now,
            });
            promoted
        }
        None => card.clone(),
    }
}

/// Pure: [`reap_if_stale`]'s own decision, minus the I/O -- see [`promote_
/// if_ready_pure`]'s doc comment for why this split exists. Reaps `card`'s
/// own claim back to `Ready` (recording an [`Event::Crash`] into `events`)
/// when it is `Running`, its TTL has elapsed, and its claimant is confirmed
/// dead -- so a fresh `claim` attempt against a card a crashed worker left
/// stuck `Running` succeeds without a separate maintenance step. A live
/// claimant, or one whose TTL has not yet elapsed, is left untouched (`reap`'s
/// own doc comment).
fn reap_if_stale_pure(card: &Card, now: u64, events: &mut Vec<Event>) -> Card {
    let Some(existing) = &card.claim else {
        return card.clone();
    };
    if card.state != State::Running || !is_ttl_expired(existing, now) {
        return card.clone();
    }
    match reap(card, now, claimant_alive(existing)) {
        Some(reaped) => {
            events.push(Event::Crash {
                id: card.id.clone(),
                at: now,
            });
            reaped
        }
        None => card.clone(),
    }
}

/// The one locked entry point every read-then-append task verb goes
/// through: acquires `repo_slug`'s task lock, loads a FRESH view of every
/// card in the repository under that lock (never a caller's own, possibly
/// stale, earlier read), hands it to the pure `decide` closure, and durably
/// appends whatever events `decide` returns -- all before the lock is
/// released. This is the fix for issue #317's own claim race: two concurrent
/// callers deciding against the same id can never both observe a
/// pre-mutation card and both append a conflicting event, because the
/// loser's own fresh read (taken only once IT holds the lock) already
/// reflects the winner's write.
///
/// `decide` returns `Ok(None)` when `id` names no card in this repository
/// (nothing is appended); `Ok(Some((Err(refusal), events)))` when the pure
/// transition itself refuses (any events already decided along the way --
/// e.g. a stale-claim reap -- are still appended, since those are honest
/// facts regardless of whether the verb's own transition succeeds);
/// `Ok(Some((Ok(result), events)))` once `events` has been durably appended.
fn with_task_lock<T>(
    state: &StateDir,
    repo_slug: &str,
    decide: impl FnOnce(&BTreeMap<String, Card>) -> Option<(Result<T, Refusal>, Vec<Event>)>,
) -> CtxResult<Option<Result<T, Refusal>>> {
    let _lock = lock_tasks(state, repo_slug)?;
    let cards = load_cards(state, repo_slug);
    let Some((outcome, events)) = decide(&cards) else {
        return Ok(None);
    };
    if !events.is_empty() {
        write_events_atomic_locked(state, repo_slug, &events)?;
    }
    Ok(Some(outcome))
}

/// [`claim`]'s own locked entry point, going through [`with_task_lock`]: the
/// self-healing reap/promote steps [`run_claim`] and `zirv ctx agent --task`
/// (`agent::claim_task_for_delegation`) both need before attempting a claim,
/// then the claim attempt itself, all decided against ONE fresh read and
/// appended as one atomic batch under one hold of the lock. `Ok(None)` when
/// no card with `id` exists in this repository.
#[allow(clippy::too_many_arguments)]
pub fn claim_locked(
    state: &StateDir,
    repo_slug: &str,
    id: &str,
    session: &str,
    pid: u32,
    pid_start_time: Option<u64>,
    host: &str,
    now: u64,
    ttl_secs: u64,
) -> CtxResult<Option<Result<Card, Refusal>>> {
    with_task_lock(state, repo_slug, |cards| {
        let card = cards.get(id)?;
        let mut events = Vec::new();
        let card = reap_if_stale_pure(card, now, &mut events);
        let card = promote_if_ready_pure(cards, &card, now, &mut events);
        let parents_done = card
            .parents
            .iter()
            .all(|pid| cards.get(pid).is_some_and(|p| p.state == State::Done));
        match claim(
            &card,
            session,
            pid,
            pid_start_time,
            host,
            now,
            ttl_secs,
            parents_done,
        ) {
            Ok(claimed) => {
                let claim_val = claimed.claim.clone().expect("claim always sets it");
                events.push(Event::Claimed {
                    id: card.id.clone(),
                    claim: claim_val,
                    attempts: claimed.attempts,
                    at: now,
                });
                Some((Ok(claimed), events))
            }
            Err(refusal) => Some((Err(refusal), events)),
        }
    })
}

/// Issue #720 (the state-reconcile pass): [`reap_if_stale_pure`]'s own
/// decision applied to EVERY card in `cards`, not just the one id
/// [`claim_locked`] reaps in passing -- a stuck `Running` card nobody
/// happens to retry by its exact id sits forever otherwise (`run_list`
/// never reaps at all). Returns the ids actually reaped, in `cards`' own
/// `BTreeMap` order, alongside the events a caller must persist to make it
/// durable; a caller that only wants the PREVIEW (no persistence) reads the
/// first element and drops the second.
fn reap_all_decide(cards: &BTreeMap<String, Card>, now: u64) -> (Vec<String>, Vec<Event>) {
    let mut events = Vec::new();
    let mut reaped_ids = Vec::new();
    for card in cards.values() {
        let before = events.len();
        let _ = reap_if_stale_pure(card, now, &mut events);
        if events.len() > before {
            reaped_ids.push(card.id.clone());
        }
    }
    (reaped_ids, events)
}

/// Dry-run preview of [`reap_all_locked`]: the identical decision, against a
/// fresh load, with nothing appended or written. Safe to call with no lock
/// held -- it never mutates -- so a `--dry-run` reconcile pass can report
/// exactly what a live pass would reap without ever taking the task lock.
pub(crate) fn reap_all_dry(state: &StateDir, repo_slug: &str, now: u64) -> Vec<String> {
    let cards = load_cards(state, repo_slug);
    reap_all_decide(&cards, now).0
}

/// The repo-wide counterpart of [`claim_locked`]'s own reap-in-passing: reaps
/// every stale `Running` card in `repo_slug` under a SINGLE hold of the task
/// lock, appending every resulting [`Event::Crash`] as one atomic batch.
/// Returns the ids actually reaped (empty when nothing was stale, in which
/// case nothing is written at all).
pub(crate) fn reap_all_locked(
    state: &StateDir,
    repo_slug: &str,
    now: u64,
) -> CtxResult<Vec<String>> {
    let _lock = lock_tasks(state, repo_slug)?;
    let cards = load_cards(state, repo_slug);
    let (reaped_ids, events) = reap_all_decide(&cards, now);
    if !events.is_empty() {
        write_events_atomic_locked(state, repo_slug, &events)?;
    }
    Ok(reaped_ids)
}

pub fn run_claim<W: Write>(
    state: &StateDir,
    w: &mut W,
    args: &ClaimArgs,
    env: EnvLookup<'_>,
    now: u64,
) -> CtxResult<i32> {
    super::seat::fence(state)?;
    let repo_slug = resolve_repo_slug()?;
    let session = resolve_session(&args.session, env);
    let pid = std::process::id();
    let pid_start_time = super::sessions::process_start_secs(pid);
    let ttl_secs = args.ttl_secs.unwrap_or(DEFAULT_CLAIM_TTL_SECS);
    let host = local_host();
    match claim_locked(
        state,
        &repo_slug,
        &args.id,
        &session,
        pid,
        pid_start_time,
        &host,
        now,
        ttl_secs,
    )? {
        None => {
            writeln!(w, "no task '{}'", args.id)?;
            Ok(1)
        }
        Some(Ok(claimed)) => {
            writeln!(w, "claimed {}", claimed.id)?;
            Ok(0)
        }
        Some(Err(refusal)) => {
            writeln!(w, "cannot claim {}: {refusal}", args.id)?;
            Ok(1)
        }
    }
}

pub fn run_heartbeat<W: Write>(
    state: &StateDir,
    w: &mut W,
    args: &HeartbeatArgs,
    env: EnvLookup<'_>,
    now: u64,
) -> CtxResult<i32> {
    super::seat::fence(state)?;
    let repo_slug = resolve_repo_slug()?;
    let session = resolve_session(&args.session, env);
    let outcome = with_task_lock(state, &repo_slug, |cards| {
        let card = cards.get(&args.id)?;
        let alive = card.claim.as_ref().is_some_and(claimant_alive);
        match heartbeat(card, &session, now, alive) {
            Ok(next) => {
                let event = Event::Heartbeat {
                    id: args.id.clone(),
                    claimed_at: now,
                    at: now,
                };
                Some((Ok(next), vec![event]))
            }
            Err(refusal) => Some((Err(refusal), Vec::new())),
        }
    })?;
    match outcome {
        None => {
            writeln!(w, "no task '{}'", args.id)?;
            Ok(1)
        }
        Some(Ok(_)) => {
            writeln!(w, "heartbeat {}", args.id)?;
            Ok(0)
        }
        Some(Err(refusal)) => {
            writeln!(w, "cannot heartbeat {}: {refusal}", args.id)?;
            Ok(1)
        }
    }
}

pub fn run_complete<W: Write>(
    state: &StateDir,
    w: &mut W,
    args: &CompleteArgs,
    now: u64,
) -> CtxResult<i32> {
    super::seat::fence(state)?;
    let repo_slug = resolve_repo_slug()?;
    let outcome = with_task_lock(state, &repo_slug, |cards| {
        let card = cards.get(&args.id)?;
        let completed = complete(card, &args.outcome, now);
        let event = Event::Completed {
            id: args.id.clone(),
            outcome: completed.outcome.clone().unwrap_or_default(),
            at: completed.updated_at,
        };
        Some((Ok(completed), vec![event]))
    })?;
    match outcome {
        None => {
            writeln!(w, "no task '{}'", args.id)?;
            Ok(1)
        }
        Some(Ok(_)) => {
            writeln!(w, "completed {}", args.id)?;
            Ok(0)
        }
        Some(Err(refusal)) => {
            // `complete` never refuses today, but the shape is kept uniform
            // with every other locked verb rather than special-cased away.
            writeln!(w, "cannot complete {}: {refusal}", args.id)?;
            Ok(1)
        }
    }
}

pub fn run_block<W: Write>(
    state: &StateDir,
    w: &mut W,
    args: &BlockArgs,
    now: u64,
) -> CtxResult<i32> {
    super::seat::fence(state)?;
    let repo_slug = resolve_repo_slug()?;
    let by = args.by.clone().unwrap_or_else(|| "operator".to_string());
    let outcome = with_task_lock(state, &repo_slug, |cards| {
        let card = cards.get(&args.id)?;
        match block(card, &args.reason, &by, now) {
            Ok(blocked) => {
                let b = blocked.block.clone().expect("block always sets it");
                let event = Event::Blocked {
                    id: args.id.clone(),
                    reason: b.reason,
                    by: b.by,
                    at: blocked.updated_at,
                };
                Some((Ok(blocked), vec![event]))
            }
            Err(refusal) => Some((Err(refusal), Vec::new())),
        }
    })?;
    match outcome {
        None => {
            writeln!(w, "no task '{}'", args.id)?;
            Ok(1)
        }
        Some(Ok(_)) => {
            writeln!(w, "blocked {}", args.id)?;
            Ok(0)
        }
        Some(Err(refusal)) => {
            writeln!(w, "cannot block {}: {refusal}", args.id)?;
            Ok(1)
        }
    }
}

pub fn run_unblock<W: Write>(
    state: &StateDir,
    w: &mut W,
    args: &UnblockArgs,
    now: u64,
) -> CtxResult<i32> {
    super::seat::fence(state)?;
    let repo_slug = resolve_repo_slug()?;
    let outcome = with_task_lock(state, &repo_slug, |cards| {
        let card = cards.get(&args.id)?;
        match unblock(card, now) {
            Ok(unblocked) => {
                let recovered_jev_block = card
                    .block
                    .as_ref()
                    .is_some_and(|block| block.by == "system:jev-crash");
                let event = Event::Unblocked {
                    id: args.id.clone(),
                    at: unblocked.updated_at,
                };
                Some((Ok((unblocked, recovered_jev_block)), vec![event]))
            }
            Err(refusal) => Some((Err(refusal), Vec::new())),
        }
    })?;
    match outcome {
        None => {
            writeln!(w, "no task '{}'", args.id)?;
            Ok(1)
        }
        Some(Ok((_, recovered_jev_block))) => {
            if recovered_jev_block
                && let Ok(repo) = std::env::current_dir()
                && let Ok(cfg) = CtxConfig::load(&repo, &|key| std::env::var(key).ok())
            {
                let mut effect = jev::JevEffect::new("crash", "task_unblocked");
                effect.subject_id = Some(&args.id);
                effect.outcome = Some("recovered");
                jev::record_effect(&cfg, state, cfg.jev.supervisor, &effect);
            }
            writeln!(w, "unblocked {}", args.id)?;
            Ok(0)
        }
        Some(Err(refusal)) => {
            writeln!(w, "cannot unblock {}: {refusal}", args.id)?;
            Ok(1)
        }
    }
}

pub fn run_comment<W: Write>(
    state: &StateDir,
    w: &mut W,
    args: &CommentArgs,
    now: u64,
) -> CtxResult<i32> {
    super::seat::fence(state)?;
    let repo_slug = resolve_repo_slug()?;
    let by = args.by.clone().unwrap_or_else(|| "operator".to_string());
    let outcome = with_task_lock(state, &repo_slug, |cards| {
        let card = cards.get(&args.id)?;
        let commented = add_comment(card, &by, &args.text, now);
        let comment = commented.comments.last().expect("just pushed").clone();
        let event = Event::Commented {
            id: args.id.clone(),
            comment,
        };
        Some((Ok(commented), vec![event]))
    })?;
    match outcome {
        None => {
            writeln!(w, "no task '{}'", args.id)?;
            Ok(1)
        }
        Some(Ok(_)) => {
            writeln!(w, "commented on {}", args.id)?;
            Ok(0)
        }
        Some(Err(refusal)) => {
            // `add_comment` never refuses today, but the shape is kept
            // uniform with every other locked verb rather than special-cased
            // away.
            writeln!(w, "cannot comment on {}: {refusal}", args.id)?;
            Ok(1)
        }
    }
}

pub fn run_archive<W: Write>(
    state: &StateDir,
    w: &mut W,
    args: &ArchiveArgs,
    now: u64,
) -> CtxResult<i32> {
    super::seat::fence(state)?;
    let repo_slug = resolve_repo_slug()?;
    let outcome = with_task_lock(state, &repo_slug, |cards| {
        let card = cards.get(&args.id)?;
        match archive(card, now) {
            Ok(archived) => {
                let event = Event::Archived {
                    id: args.id.clone(),
                    at: archived.updated_at,
                };
                Some((Ok(archived), vec![event]))
            }
            Err(refusal) => Some((Err(refusal), Vec::new())),
        }
    })?;
    match outcome {
        None => {
            writeln!(w, "no task '{}'", args.id)?;
            Ok(1)
        }
        Some(Ok(_)) => {
            writeln!(w, "archived {}", args.id)?;
            Ok(0)
        }
        Some(Err(refusal)) => {
            writeln!(w, "cannot archive {}: {refusal}", args.id)?;
            Ok(1)
        }
    }
}

pub fn run<W: Write>(args: &TaskArgs, w: &mut W) -> CtxResult<i32> {
    let env = super::config::env_from_process();
    let state = StateDir::resolve(&env)?;
    let now = super::state::now_secs();
    match &args.command {
        TaskVerb::Create(a) => {
            run_create(&state, w, a, now)?;
            Ok(0)
        }
        TaskVerb::List(a) => run_list(&state, w, a, now),
        TaskVerb::Show(a) => run_show(&state, w, a, now),
        TaskVerb::Claim(a) => run_claim(&state, w, a, &env, now),
        TaskVerb::Heartbeat(a) => run_heartbeat(&state, w, a, &env, now),
        TaskVerb::Complete(a) => run_complete(&state, w, a, now),
        TaskVerb::Block(a) => run_block(&state, w, a, now),
        TaskVerb::Unblock(a) => run_unblock(&state, w, a, now),
        TaskVerb::Comment(a) => run_comment(&state, w, a, now),
        TaskVerb::Archive(a) => run_archive(&state, w, a, now),
    }
}

// -- Swarm --------------------------------------------------------------

pub struct SwarmIds {
    pub root: String,
    pub workers: Vec<String>,
    pub verifier: String,
    pub synthesizer: String,
}

/// Builds the event batch for `zirv ctx swarm`: a root card, `workers` sibling
/// worker cards parented on the root, a verifier gated on every worker, and a
/// synthesizer gated on the verifier. Pure and fallible up front (`workers ==
/// 0` is refused before anything is built) -- nothing here touches disk, so a
/// caller that bails on this `Err` never has to unwind a partial write: there
/// never was one. [`append_events_atomic`] is what actually persists the
/// result, as a single all-or-nothing batch.
pub fn build_swarm_events(
    scope: &str,
    repo_slug: &str,
    workers: u32,
    group_id: Option<&str>,
    now: u64,
) -> Result<(Vec<Event>, SwarmIds), String> {
    if workers == 0 {
        return Err("--workers must be at least 1".to_string());
    }
    let mint = || format!("task-{}", uuid::Uuid::new_v4());
    let mut events = Vec::new();
    let root_id = mint();
    events.push(Event::Created {
        id: root_id.clone(),
        repo_slug: repo_slug.to_string(),
        title: format!("swarm root: {scope}"),
        brief: scope.to_string(),
        parents: Vec::new(),
        group_id: group_id.map(str::to_string),
        workdir: None,
        at: now,
    });
    let mut worker_ids = Vec::new();
    for i in 0..workers {
        let id = mint();
        events.push(Event::Created {
            id: id.clone(),
            repo_slug: repo_slug.to_string(),
            title: format!("{scope} -- worker {}/{workers}", i + 1),
            brief: scope.to_string(),
            parents: vec![root_id.clone()],
            group_id: group_id.map(str::to_string),
            workdir: None,
            at: now,
        });
        worker_ids.push(id);
    }
    let verifier_id = mint();
    events.push(Event::Created {
        id: verifier_id.clone(),
        repo_slug: repo_slug.to_string(),
        title: format!("{scope} -- verify"),
        brief: format!("verify every worker's result for: {scope}"),
        parents: worker_ids.clone(),
        group_id: group_id.map(str::to_string),
        workdir: None,
        at: now,
    });
    let synthesizer_id = mint();
    events.push(Event::Created {
        id: synthesizer_id.clone(),
        repo_slug: repo_slug.to_string(),
        title: format!("{scope} -- synthesize"),
        brief: format!("synthesize the verified results for: {scope}"),
        parents: vec![verifier_id.clone()],
        group_id: group_id.map(str::to_string),
        workdir: None,
        at: now,
    });
    Ok((
        events,
        SwarmIds {
            root: root_id,
            workers: worker_ids,
            verifier: verifier_id,
            synthesizer: synthesizer_id,
        },
    ))
}

pub fn run_swarm_with<W: Write>(
    state: &StateDir,
    w: &mut W,
    args: &SwarmArgs,
    now: u64,
) -> CtxResult<i32> {
    super::seat::fence(state)?;
    let repo_slug = resolve_repo_slug()?;
    let (events, ids) = match build_swarm_events(
        &args.scope,
        &repo_slug,
        args.workers,
        args.group.as_deref(),
        now,
    ) {
        Ok(built) => built,
        Err(e) => {
            writeln!(w, "swarm: {e}")?;
            return Ok(1);
        }
    };
    append_events_atomic(state, &repo_slug, &events)?;
    writeln!(w, "root: {}", ids.root)?;
    for (i, id) in ids.workers.iter().enumerate() {
        writeln!(w, "worker {}: {id}", i + 1)?;
    }
    writeln!(w, "verifier: {}", ids.verifier)?;
    writeln!(w, "synthesizer: {}", ids.synthesizer)?;
    Ok(0)
}

pub fn run_swarm<W: Write>(args: &SwarmArgs, w: &mut W) -> CtxResult<i32> {
    let env = super::config::env_from_process();
    let state = StateDir::resolve(&env)?;
    let now = super::state::now_secs();
    run_swarm_with(&state, w, args, now)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_card(id: &str, state: State, parents: Vec<String>) -> Card {
        Card {
            id: id.to_string(),
            repo_slug: "repo".to_string(),
            title: "title".to_string(),
            brief: "brief".to_string(),
            state,
            parents,
            claim: None,
            block: None,
            comments: Vec::new(),
            workdir: None,
            group_id: None,
            outcome: None,
            attempts: 0,
            created_at: 1_700_000_000,
            updated_at: 1_700_000_000,
        }
    }

    // -- materialize ------------------------------------------------------

    #[test]
    fn a_created_card_with_no_parents_starts_ready() {
        let events = vec![Event::Created {
            id: "t1".to_string(),
            repo_slug: "repo".to_string(),
            title: "t".to_string(),
            brief: "b".to_string(),
            parents: Vec::new(),
            group_id: None,
            workdir: None,
            at: 1,
        }];
        let cards = materialize(&events);
        assert_eq!(cards["t1"].state, State::Ready);
    }

    #[test]
    fn a_created_card_with_parents_starts_todo() {
        let events = vec![Event::Created {
            id: "t1".to_string(),
            repo_slug: "repo".to_string(),
            title: "t".to_string(),
            brief: "b".to_string(),
            parents: vec!["p1".to_string()],
            group_id: None,
            workdir: None,
            at: 1,
        }];
        let cards = materialize(&events);
        assert_eq!(cards["t1"].state, State::Todo);
    }

    #[test]
    fn materialize_skips_a_mutation_naming_an_unknown_id() {
        let events = vec![Event::Completed {
            id: "ghost".to_string(),
            outcome: "ok".to_string(),
            at: 1,
        }];
        assert!(materialize(&events).is_empty(), "no card to mutate");
    }

    #[test]
    fn materialize_skips_a_duplicate_created_event_first_write_wins() {
        let events = vec![
            Event::Created {
                id: "t1".to_string(),
                repo_slug: "repo".to_string(),
                title: "first".to_string(),
                brief: "b".to_string(),
                parents: Vec::new(),
                group_id: None,
                workdir: None,
                at: 1,
            },
            Event::Created {
                id: "t1".to_string(),
                repo_slug: "repo".to_string(),
                title: "second".to_string(),
                brief: "b".to_string(),
                parents: Vec::new(),
                group_id: None,
                workdir: None,
                at: 2,
            },
        ];
        let cards = materialize(&events);
        assert_eq!(cards["t1"].title, "first");
    }

    #[test]
    fn read_events_skips_a_corrupt_line() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().to_path_buf());
        append_event(
            &state,
            "repo",
            &Event::Created {
                id: "t1".to_string(),
                repo_slug: "repo".to_string(),
                title: "t".to_string(),
                brief: "b".to_string(),
                parents: Vec::new(),
                group_id: None,
                workdir: None,
                at: 1,
            },
        )
        .expect("append");
        {
            let mut file = super::super::state::open_private_append(&events_path(&state, "repo"))
                .expect("open");
            writeln!(file, "not json").expect("write corrupt line");
        }
        let events = read_events(&state, "repo");
        assert_eq!(events.len(), 1, "the corrupt line is skipped: {events:?}");
    }

    #[test]
    fn read_events_before_any_exist_is_empty_not_an_error() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().to_path_buf());
        assert!(read_events(&state, "repo").is_empty());
    }

    /// E-4: `write_events_atomic_locked` used to do
    /// `read_to_string(&path).unwrap_or_default()`, collapsing a genuine
    /// read error (a torn append leaving one invalid UTF-8 byte) to the
    /// SAME "empty" result a missing file gets -- silently discarding
    /// every prior event on the next atomic append. The fix must either
    /// fail the append outright or preserve the prior event, never write
    /// only the new batch as if the file had never existed.
    #[test]
    fn a_torn_utf8_byte_never_silently_wipes_the_prior_event_log() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().to_path_buf());
        let repo_slug = "repo";
        let path = events_path(&state, repo_slug);
        create_private_dir_all(path.parent().expect("parent")).expect("mkdir");

        let prior = Event::Created {
            id: "t1".to_string(),
            repo_slug: repo_slug.to_string(),
            title: "t".to_string(),
            brief: "b".to_string(),
            parents: Vec::new(),
            group_id: None,
            workdir: None,
            at: 1,
        };
        let mut bytes = serde_json::to_vec(&prior).expect("serialize");
        bytes.push(b'\n');
        bytes.push(0xFF); // a torn/invalid UTF-8 byte, no trailing newline
        std::fs::write(&path, &bytes).expect("write raw bytes");

        let new_event = Event::Readied {
            id: "t2".to_string(),
            at: 2,
        };
        match append_events_atomic(&state, repo_slug, std::slice::from_ref(&new_event)) {
            Err(_) => {
                let raw = std::fs::read(&path).expect("read raw");
                assert_eq!(
                    raw, bytes,
                    "a refused append must leave the prior (corrupt) file untouched"
                );
            }
            Ok(()) => {
                let events = read_events(&state, repo_slug);
                assert!(
                    events
                        .iter()
                        .any(|e| matches!(e, Event::Created { id, .. } if id == "t1")),
                    "the prior event must survive a successful append too: {events:?}"
                );
            }
        }
    }

    #[test]
    fn an_append_after_a_log_with_no_trailing_newline_keeps_both_events_readable() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().to_path_buf());
        let repo_slug = "repo";
        let path = events_path(&state, repo_slug);
        create_private_dir_all(path.parent().expect("parent")).expect("mkdir");

        let prior = Event::Created {
            id: "t1".to_string(),
            repo_slug: repo_slug.to_string(),
            title: "t".to_string(),
            brief: "b".to_string(),
            parents: Vec::new(),
            group_id: None,
            workdir: None,
            at: 1,
        };
        // No trailing newline -- simulates a torn append that stopped
        // right after the JSON but before its own newline.
        std::fs::write(&path, serde_json::to_string(&prior).expect("serialize"))
            .expect("write without trailing newline");

        let new_event = Event::Readied {
            id: "t1".to_string(),
            at: 2,
        };
        append_events_atomic(&state, repo_slug, std::slice::from_ref(&new_event)).expect("append");

        let events = read_events(&state, repo_slug);
        assert_eq!(
            events.len(),
            2,
            "both the prior and the newly-appended event must be independently readable: {events:?}"
        );
    }

    // -- claim --------------------------------------------------------------

    #[test]
    fn claim_succeeds_on_a_ready_card_with_no_parents() {
        let card = sample_card("t1", State::Ready, Vec::new());
        let claimed = claim(&card, "sess-1", 42, None, "host", 100, 900, true).expect("claim");
        assert_eq!(claimed.state, State::Running);
        assert_eq!(claimed.attempts, 1);
        let claim = claimed.claim.expect("claim set");
        assert_eq!(claim.session, "sess-1");
        assert_eq!(claim.pid, 42);
        assert_eq!(claim.claimed_at, 100);
        assert_eq!(claim.ttl_secs, 900);
    }

    #[test]
    fn claim_refuses_a_card_that_is_not_ready() {
        let card = sample_card("t1", State::Todo, Vec::new());
        let err = claim(&card, "sess-1", 42, None, "host", 100, 900, true).expect_err("not ready");
        assert_eq!(err, Refusal::NotReady(State::Todo));
    }

    /// Issue #317 acceptance: a card with unmet parents cannot be claimed
    /// EVEN when its own `state` field was manually set to `Ready` -- `claim`
    /// independently trusts only the `parents_done` the caller computed, not
    /// whatever the card's own persisted state claims.
    #[test]
    fn claim_refuses_unmet_parents_even_when_the_card_state_says_ready() {
        let card = sample_card("t1", State::Ready, vec!["p1".to_string()]);
        let err =
            claim(&card, "sess-1", 42, None, "host", 100, 900, false).expect_err("parents unmet");
        assert_eq!(err, Refusal::ParentsUnmet(vec!["p1".to_string()]));
    }

    #[test]
    fn claim_increments_attempts_on_every_successful_claim() {
        let mut card = sample_card("t1", State::Ready, Vec::new());
        card.attempts = 1;
        let claimed = claim(&card, "sess-1", 42, None, "host", 100, 900, true).expect("claim");
        assert_eq!(claimed.attempts, 2);
    }

    // -- heartbeat / reap -----------------------------------------------------

    fn claimed_card(session: &str, pid: u32, claimed_at: u64, ttl_secs: u64) -> Card {
        let mut card = sample_card("t1", State::Running, Vec::new());
        card.claim = Some(Claim {
            session: session.to_string(),
            pid,
            pid_start_time: None,
            host: "host".to_string(),
            claimed_at,
            ttl_secs,
        });
        card
    }

    #[test]
    fn heartbeat_extends_the_claim_only_for_the_live_claimant() {
        let card = claimed_card("sess-1", 42, 100, 900);
        let extended = heartbeat(&card, "sess-1", 500, true).expect("heartbeat");
        assert_eq!(extended.claim.expect("claim").claimed_at, 500);
    }

    #[test]
    fn heartbeat_refuses_a_dead_claimant() {
        let card = claimed_card("sess-1", 42, 100, 900);
        let err = heartbeat(&card, "sess-1", 500, false).expect_err("dead claimant");
        assert_eq!(err, Refusal::ClaimantDead);
    }

    #[test]
    fn heartbeat_refuses_a_session_that_is_not_the_claimant() {
        let card = claimed_card("sess-1", 42, 100, 900);
        let err = heartbeat(&card, "sess-2", 500, true).expect_err("wrong claimant");
        assert_eq!(err, Refusal::WrongClaimant("sess-1".to_string()));
    }

    #[test]
    fn heartbeat_refuses_an_unclaimed_card() {
        let card = sample_card("t1", State::Ready, Vec::new());
        let err = heartbeat(&card, "sess-1", 500, true).expect_err("not claimed");
        assert_eq!(err, Refusal::NotClaimed);
    }

    /// Issue #317 acceptance: a dead pid's claim reaps to `Ready` with the
    /// state reset, regardless of how far past its TTL the claim is.
    #[test]
    fn reap_returns_a_dead_claim_to_ready() {
        let card = claimed_card("sess-1", 42, 100, 900);
        let reaped = reap(&card, 5_000, false).expect("reaped");
        assert_eq!(reaped.state, State::Ready);
        assert!(reaped.claim.is_none());
        assert_eq!(reaped.updated_at, 5_000);
    }

    /// Issue #317 acceptance: a live pid past its TTL is left alone -- reap
    /// never fires on liveness alone.
    #[test]
    fn reap_leaves_a_live_claim_alone_no_matter_how_far_past_ttl() {
        let card = claimed_card("sess-1", 42, 100, 900);
        assert!(reap(&card, 999_999, true).is_none());
    }

    #[test]
    fn reap_does_nothing_to_a_card_that_is_not_running() {
        let card = sample_card("t1", State::Ready, Vec::new());
        assert!(reap(&card, 100, false).is_none());
    }

    #[test]
    fn is_ttl_expired_is_pure_arithmetic_over_now_and_claimed_at() {
        let claim = Claim {
            session: "s".to_string(),
            pid: 1,
            pid_start_time: None,
            host: "h".to_string(),
            claimed_at: 100,
            ttl_secs: 900,
        };
        assert!(!is_ttl_expired(&claim, 100 + 900));
        assert!(is_ttl_expired(&claim, 100 + 901));
    }

    // -- complete / block / unblock / archive / comment ----------------------

    #[test]
    fn complete_marks_done_and_clears_the_claim() {
        let card = claimed_card("sess-1", 42, 100, 900);
        let done = complete(&card, "shipped", 200);
        assert_eq!(done.state, State::Done);
        assert_eq!(done.outcome.as_deref(), Some("shipped"));
        assert!(done.claim.is_none());
    }

    #[test]
    fn block_and_unblock_round_trip() {
        let card = sample_card("t1", State::Ready, Vec::new());
        let blocked = block(&card, "missing credential", "sess-1", 100).expect("block from ready");
        assert_eq!(blocked.state, State::Blocked);
        assert_eq!(
            blocked.block.as_ref().expect("block").reason,
            "missing credential"
        );

        let unblocked = unblock(&blocked, 200).expect("unblock from blocked");
        assert_eq!(unblocked.state, State::Ready);
        assert!(unblocked.block.is_none());
    }

    #[test]
    fn archive_sets_the_archived_state() {
        let card = sample_card("t1", State::Done, Vec::new());
        assert_eq!(
            archive(&card, 100).expect("archive from done").state,
            State::Archived
        );
    }

    /// Issue #317 review finding: `block` applies unconditionally today --
    /// it must refuse a card that is already terminal (`Done`/`Archived`) or
    /// already `Blocked`, since blocking an already-blocked card would
    /// silently overwrite its existing reason/by without an `Unblocked` in
    /// between.
    #[test]
    fn block_refuses_a_card_that_is_already_done() {
        let card = sample_card("t1", State::Done, Vec::new());
        let err = block(&card, "reason", "sess-1", 100).expect_err("done cannot be blocked");
        assert_eq!(err, Refusal::CannotBlock(State::Done));
    }

    /// Issue #317 review finding: `unblock` applies unconditionally today --
    /// it must refuse anything but `Blocked` (there is nothing to clear off
    /// e.g. a `Ready` or `Running` card).
    #[test]
    fn unblock_refuses_a_card_that_is_not_blocked() {
        let card = sample_card("t1", State::Ready, Vec::new());
        let err = unblock(&card, 100).expect_err("ready is not blocked");
        assert_eq!(err, Refusal::CannotUnblock(State::Ready));
    }

    /// Issue #317 review finding: `archive` applies unconditionally today --
    /// it must refuse a card still actively in flight (only `Done`/`Blocked`
    /// may be archived).
    #[test]
    fn archive_refuses_a_card_that_is_still_running() {
        let card = sample_card("t1", State::Running, Vec::new());
        let err = archive(&card, 100).expect_err("running cannot be archived");
        assert_eq!(err, Refusal::CannotArchive(State::Running));
    }

    #[test]
    fn add_comment_appends_without_changing_state() {
        let card = sample_card("t1", State::Running, Vec::new());
        let commented = add_comment(&card, "sess-1", "making progress", 100);
        assert_eq!(commented.comments.len(), 1);
        assert_eq!(commented.comments[0].text, "making progress");
        assert_eq!(commented.state, State::Running);
    }

    // -- ready_when_parents_done ----------------------------------------------

    #[test]
    fn ready_when_parents_done_promotes_once_every_parent_is_done() {
        let card = sample_card("t1", State::Todo, vec!["p1".to_string(), "p2".to_string()]);
        let p1 = sample_card("p1", State::Done, Vec::new());
        let p2 = sample_card("p2", State::Done, Vec::new());
        let promoted = ready_when_parents_done(&card, &[&p1, &p2], 500).expect("all parents done");
        assert_eq!(promoted.state, State::Ready);
        assert_eq!(promoted.updated_at, 500);
    }

    #[test]
    fn ready_when_parents_done_refuses_when_a_parent_is_not_done() {
        let card = sample_card("t1", State::Todo, vec!["p1".to_string()]);
        let p1 = sample_card("p1", State::Running, Vec::new());
        assert!(ready_when_parents_done(&card, &[&p1], 500).is_none());
    }

    #[test]
    fn ready_when_parents_done_refuses_a_missing_parent_lookup() {
        let card = sample_card("t1", State::Todo, vec!["p1".to_string(), "p2".to_string()]);
        let p1 = sample_card("p1", State::Done, Vec::new());
        // Only one of two parents resolved -- must not read as vacuously done.
        assert!(ready_when_parents_done(&card, &[&p1], 500).is_none());
    }

    #[test]
    fn ready_when_parents_done_is_a_no_op_off_todo() {
        let card = sample_card("t1", State::Running, Vec::new());
        assert!(ready_when_parents_done(&card, &[], 500).is_none());
    }

    #[test]
    fn compile_task_prompt_labels_the_brief_and_every_parent_outcome() {
        let card = sample_card("t1", State::Ready, vec!["p1".to_string()]);
        let mut p1 = sample_card("p1", State::Done, Vec::new());
        p1.outcome = Some("shipped the migration".to_string());
        let text = compile_task_prompt(&card, &[&p1], 4096);
        assert!(text.contains("TASK CARD t1"));
        assert!(text.contains("brief"));
        assert!(text.contains("p1"));
        assert!(text.contains("shipped the migration"));
    }

    /// Issue #326 B1: a parent tree whose combined outcomes exceed the byte
    /// budget keeps the most recently updated parents in full and drops the
    /// rest with an explicit "[truncated N bytes]"-shaped note -- never a
    /// silent cut.
    #[test]
    fn compile_task_prompt_caps_the_aggregate_parent_outcome_bytes() {
        let card = sample_card(
            "t1",
            State::Ready,
            vec!["old".to_string(), "new".to_string()],
        );
        let mut old = sample_card("old", State::Done, Vec::new());
        old.updated_at = 100;
        old.outcome = Some("a".repeat(200));
        let mut new = sample_card("new", State::Done, Vec::new());
        new.updated_at = 200;
        new.outcome = Some("b".repeat(200));

        // A budget that fits the more recently updated parent's own line in
        // full, but not both.
        let text = compile_task_prompt(&card, &[&old, &new], 220);

        assert!(
            text.contains(&"b".repeat(200)),
            "the most recently updated parent's outcome must survive in full: {text}"
        );
        assert!(
            !text.contains(&"a".repeat(200)),
            "the older parent's outcome must not appear once it no longer fits: {text}"
        );
        assert!(
            text.contains("[truncated"),
            "an omission must be noted explicitly, never silent: {text}"
        );
        assert!(
            text.contains("1 older parent outcome"),
            "the note must say how many parents were cut: {text}"
        );
    }

    /// Issue #326 B1: nothing is cut, and no truncation note appears, when
    /// every parent's own outcome already fits the budget.
    #[test]
    fn compile_task_prompt_omits_the_truncation_note_when_everything_fits() {
        let card = sample_card("t1", State::Ready, vec!["p1".to_string()]);
        let mut p1 = sample_card("p1", State::Done, Vec::new());
        p1.outcome = Some("shipped the migration".to_string());
        let text = compile_task_prompt(&card, &[&p1], 4096);
        assert!(!text.contains("[truncated"), "got {text}");
    }

    // -- respawn_decision -----------------------------------------------------

    #[test]
    fn respawn_decision_retries_a_crash_below_the_attempt_ceiling() {
        let mut card = sample_card("t1", State::Running, Vec::new());
        card.attempts = 1;
        assert_eq!(
            respawn_decision(&card, ExitKind::Crash, DEFAULT_MAX_ATTEMPTS),
            RespawnVerdict::Respawn
        );
    }

    #[test]
    fn respawn_decision_auto_blocks_a_crash_at_the_attempt_ceiling() {
        let mut card = sample_card("t1", State::Running, Vec::new());
        card.attempts = DEFAULT_MAX_ATTEMPTS;
        assert!(matches!(
            respawn_decision(&card, ExitKind::Crash, DEFAULT_MAX_ATTEMPTS),
            RespawnVerdict::AutoBlock(_)
        ));
    }

    /// Issue #317 acceptance: a worker exiting 0 without a report-back
    /// retries at most twice, then auto-blocks -- never `Done`.
    #[test]
    fn respawn_decision_auto_blocks_silent_zero_after_the_max_retries() {
        let mut card = sample_card("t1", State::Running, Vec::new());
        card.attempts = 1;
        assert_eq!(
            respawn_decision(&card, ExitKind::SilentZero, DEFAULT_MAX_ATTEMPTS),
            RespawnVerdict::Respawn,
            "first retry still allowed"
        );
        card.attempts = DEFAULT_MAX_ATTEMPTS;
        assert!(
            matches!(
                respawn_decision(&card, ExitKind::SilentZero, DEFAULT_MAX_ATTEMPTS),
                RespawnVerdict::AutoBlock(_)
            ),
            "the maximum is reached: auto-block, never a silent Done"
        );
    }

    #[test]
    fn respawn_decision_refuses_a_card_that_already_succeeded() {
        let card = sample_card("t1", State::Done, Vec::new());
        assert!(matches!(
            respawn_decision(&card, ExitKind::Crash, DEFAULT_MAX_ATTEMPTS),
            RespawnVerdict::Refuse(_)
        ));
    }

    #[test]
    fn respawn_decision_refuses_an_auth_blocked_card() {
        let mut card = sample_card("t1", State::Blocked, Vec::new());
        card.block = Some(Block {
            reason: "missing AUTH token".to_string(),
            by: "sess-1".to_string(),
        });
        assert!(matches!(
            respawn_decision(&card, ExitKind::Crash, DEFAULT_MAX_ATTEMPTS),
            RespawnVerdict::Refuse(_)
        ));
    }

    // -- respawn_decision_with_jev (issue #537 A4) ----------------------------

    fn jev_test_cfg(base_url: String, credential_env: &str) -> CtxConfig {
        let mut cfg = CtxConfig::default();
        cfg.jev.supervisor = true;
        cfg.proxy.typesafe.base_url = base_url;
        cfg.proxy.typesafe.credential_env = credential_env.to_string();
        cfg.proxy.typesafe.timeout_secs = 5;
        cfg
    }

    /// The keyword check on `card.block.reason` (`"missing AUTH token"`)
    /// matches and refuses -- with the gate ON and a credential set, so a
    /// bug that called Jev before checking keywords would still attempt a
    /// call. No `jev-decisions.jsonl` file must exist afterward: proof no
    /// call was ever even attempted, not merely that one failed silently.
    #[test]
    fn respawn_decision_with_jev_never_calls_out_when_the_keyword_check_already_matched() {
        let mut card = sample_card("t1", State::Blocked, Vec::new());
        card.block = Some(Block {
            reason: "missing AUTH token".to_string(),
            by: "sess-1".to_string(),
        });
        let credential_env = "TASK_TEST_JEV_KEYWORD_MATCH";
        // SAFETY (test-only): a unique env var name this test owns.
        unsafe {
            std::env::set_var(credential_env, "secret");
        }
        let cfg = jev_test_cfg("http://127.0.0.1:0".to_string(), credential_env);
        let state_dir = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(state_dir.path().to_path_buf());

        let verdict =
            respawn_decision_with_jev(&cfg, &state, &card, ExitKind::Crash, DEFAULT_MAX_ATTEMPTS);

        unsafe {
            std::env::remove_var(credential_env);
        }
        assert!(matches!(verdict, RespawnVerdict::Refuse(_)));
        assert!(
            !state_dir.path().join("jev-decisions.jsonl").exists(),
            "the keyword match must short-circuit before any Jev call is attempted"
        );
    }

    #[test]
    fn an_exhausted_retry_never_asks_jev_or_replaces_the_attempt_cap_reason() {
        let mut card = sample_card("t1", State::Blocked, Vec::new());
        card.attempts = DEFAULT_MAX_ATTEMPTS;
        card.block = Some(Block {
            reason: "local setup is invalid".to_string(),
            by: "worker".to_string(),
        });
        let credential_env = "TASK_TEST_JEV_ATTEMPT_CAP";
        let _credential =
            crate::commands::ctx::testenv::VarGuard::set(&[(credential_env, Some("secret"))]);
        let cfg = jev_test_cfg("http://127.0.0.1:0".to_string(), credential_env);
        let state_dir = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(state_dir.path().to_path_buf());

        let baseline = respawn_decision(&card, ExitKind::Crash, DEFAULT_MAX_ATTEMPTS);
        let actual =
            respawn_decision_with_jev(&cfg, &state, &card, ExitKind::Crash, DEFAULT_MAX_ATTEMPTS);

        assert_eq!(actual, baseline);
        assert!(!state_dir.path().join("jev-decisions.jsonl").exists());
    }

    /// A block reason with no keyword hit ("token expired, run login
    /// again") and a confident `access` answer (0.95, at or above
    /// `CRASH_TRIAGE_FLOOR`) auto-blocks -- the same path the keyword match
    /// takes, reached here through Jev instead. `attempts` is well below the
    /// ceiling, so a bare `Respawn` would otherwise result: the `AutoBlock`
    /// is attributable only to Jev.
    #[test]
    fn respawn_decision_with_jev_auto_blocks_on_a_confident_access_answer() {
        let mut card = sample_card("t1", State::Blocked, Vec::new());
        card.attempts = 1;
        card.block = Some(Block {
            reason: "token expired, run login again".to_string(),
            by: "sess-1".to_string(),
        });
        let body = r#"{"model": "jev-latest", "answers": {
            "cause": {"type": "choice", "choice": "access",
                      "probabilities": {"access": 0.95, "other": 0.05}, "confidence": 0.95}
        }, "usage": {"input_tokens": 5, "output_tokens": 0}}"#;
        let (url, handle) = crate::commands::ctx::jev::tests::one_shot_server(200, body);
        let credential_env = "TASK_TEST_JEV_ACCESS_095";
        // SAFETY (test-only): a unique env var name this test owns.
        unsafe {
            std::env::set_var(credential_env, "secret");
        }
        let cfg = jev_test_cfg(url, credential_env);
        let state_dir = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(state_dir.path().to_path_buf());

        let verdict =
            respawn_decision_with_jev(&cfg, &state, &card, ExitKind::Crash, DEFAULT_MAX_ATTEMPTS);

        unsafe {
            std::env::remove_var(credential_env);
        }
        handle.join().expect("server thread must not panic");
        assert!(
            matches!(verdict, RespawnVerdict::AutoBlock(_)),
            "{verdict:?}"
        );
    }

    /// Jev determinism fix: the identical `access` answer at a confidence
    /// (0.95) above `CRASH_TRIAGE_FLOOR`, but with a thin margin (0.51/0.49)
    /// between its own top and runner-up probability, must fall through to
    /// today's attempt-count logic exactly like a low-confidence answer.
    #[test]
    fn respawn_decision_with_jev_falls_through_on_a_thin_margin_answer() {
        let mut card = sample_card("t1", State::Blocked, Vec::new());
        card.attempts = 1;
        card.block = Some(Block {
            reason: "token expired, run login again".to_string(),
            by: "sess-1".to_string(),
        });
        let body = r#"{"model": "jev-latest", "answers": {
            "cause": {"type": "choice", "choice": "access",
                      "probabilities": {"access": 0.51, "transient": 0.49}, "confidence": 0.95}
        }, "usage": {"input_tokens": 5, "output_tokens": 0}}"#;
        let (url, handle) = crate::commands::ctx::jev::tests::one_shot_server(200, body);
        let credential_env = "TASK_TEST_JEV_THIN_MARGIN";
        // SAFETY (test-only): a unique env var name this test owns.
        unsafe {
            std::env::set_var(credential_env, "secret");
        }
        let cfg = jev_test_cfg(url, credential_env);
        let state_dir = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(state_dir.path().to_path_buf());

        let verdict =
            respawn_decision_with_jev(&cfg, &state, &card, ExitKind::Crash, DEFAULT_MAX_ATTEMPTS);

        unsafe {
            std::env::remove_var(credential_env);
        }
        handle.join().expect("server thread must not panic");
        assert_eq!(verdict, RespawnVerdict::Respawn);
    }

    /// The identical `access` answer at 0.85 -- below `CRASH_TRIAGE_FLOOR`
    /// (0.9) -- must fall through to today's attempt-count logic
    /// unchanged: `Respawn`, since `attempts` is below the ceiling.
    #[test]
    fn respawn_decision_with_jev_falls_through_below_the_confidence_floor() {
        let mut card = sample_card("t1", State::Blocked, Vec::new());
        card.attempts = 1;
        card.block = Some(Block {
            reason: "token expired, run login again".to_string(),
            by: "sess-1".to_string(),
        });
        let body = r#"{"model": "jev-latest", "answers": {
            "cause": {"type": "choice", "choice": "access",
                      "probabilities": {"access": 0.85}, "confidence": 0.85}
        }, "usage": {"input_tokens": 5, "output_tokens": 0}}"#;
        let (url, handle) = crate::commands::ctx::jev::tests::one_shot_server(200, body);
        let credential_env = "TASK_TEST_JEV_ACCESS_085";
        // SAFETY (test-only): a unique env var name this test owns.
        unsafe {
            std::env::set_var(credential_env, "secret");
        }
        let cfg = jev_test_cfg(url, credential_env);
        let state_dir = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(state_dir.path().to_path_buf());

        let verdict =
            respawn_decision_with_jev(&cfg, &state, &card, ExitKind::Crash, DEFAULT_MAX_ATTEMPTS);

        unsafe {
            std::env::remove_var(credential_env);
        }
        handle.join().expect("server thread must not panic");
        assert_eq!(verdict, RespawnVerdict::Respawn);
    }

    /// A transport/HTTP error (a 500) must fall through to today's
    /// attempt-count logic unchanged.
    #[test]
    fn respawn_decision_with_jev_falls_through_on_a_500() {
        let mut card = sample_card("t1", State::Blocked, Vec::new());
        card.attempts = 1;
        card.block = Some(Block {
            reason: "token expired, run login again".to_string(),
            by: "sess-1".to_string(),
        });
        let (url, handle) = crate::commands::ctx::jev::tests::one_shot_server(500, "{}");
        let credential_env = "TASK_TEST_JEV_500";
        // SAFETY (test-only): a unique env var name this test owns.
        unsafe {
            std::env::set_var(credential_env, "secret");
        }
        let cfg = jev_test_cfg(url, credential_env);
        let state_dir = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(state_dir.path().to_path_buf());

        let verdict =
            respawn_decision_with_jev(&cfg, &state, &card, ExitKind::Crash, DEFAULT_MAX_ATTEMPTS);

        unsafe {
            std::env::remove_var(credential_env);
        }
        handle.join().expect("server thread must not panic");
        assert_eq!(verdict, RespawnVerdict::Respawn);
    }

    /// The gate off must be byte-identical to calling `respawn_decision`
    /// directly -- no call even attempted, despite a block reason with no
    /// keyword hit and a credential that looks available.
    #[test]
    fn respawn_decision_with_jev_is_identical_to_the_pure_function_when_the_gate_is_off() {
        let mut card = sample_card("t1", State::Blocked, Vec::new());
        card.attempts = 1;
        card.block = Some(Block {
            reason: "token expired, run login again".to_string(),
            by: "sess-1".to_string(),
        });
        let credential_env = "TASK_TEST_JEV_GATE_OFF";
        // The credential looks available, so a bug that ignored the gate
        // would still attempt a call rather than short-circuiting on it.
        unsafe {
            std::env::set_var(credential_env, "secret");
        }
        let mut cfg = CtxConfig::default();
        assert!(!cfg.jev.supervisor, "the gate defaults off");
        cfg.proxy.typesafe.credential_env = credential_env.to_string();
        let state_dir = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(state_dir.path().to_path_buf());

        let baseline = respawn_decision(&card, ExitKind::Crash, DEFAULT_MAX_ATTEMPTS);
        let verdict =
            respawn_decision_with_jev(&cfg, &state, &card, ExitKind::Crash, DEFAULT_MAX_ATTEMPTS);

        unsafe {
            std::env::remove_var(credential_env);
        }
        assert_eq!(verdict, baseline);
        assert!(!state_dir.path().join("jev-decisions.jsonl").exists());
        assert!(!state_dir.path().join("jev-effects.jsonl").exists());
    }

    #[test]
    fn missing_credential_ignores_a_warm_crash_cache_and_writes_no_new_jev_rows() {
        let mut card = sample_card("t1", State::Blocked, Vec::new());
        card.attempts = 1;
        card.block = Some(Block {
            reason: "token expired, login again".into(),
            by: "sess-1".into(),
        });
        let body = r#"{"model":"jev-latest","answers":{"cause":{"type":"choice","choice":"access","probabilities":{"access":0.95,"transient":0.05},"confidence":0.95}},"usage":{"input_tokens":5,"output_tokens":0}}"#;
        let (url, server) = crate::commands::ctx::jev::tests::one_shot_server(200, body);
        let credential_env = "TASK_TEST_WARM_CACHE_MISSING_KEY";
        unsafe { std::env::set_var(credential_env, "fixture-key") };
        let cfg = jev_test_cfg(url, credential_env);
        let root = tempfile::tempdir().unwrap();
        let state = StateDir::from_root(root.path().to_path_buf());
        assert!(matches!(
            respawn_decision_with_jev(&cfg, &state, &card, ExitKind::Crash, DEFAULT_MAX_ATTEMPTS),
            RespawnVerdict::AutoBlock(_)
        ));
        server.join().unwrap();
        let before = std::fs::read_to_string(root.path().join("jev-decisions.jsonl")).unwrap();
        let effects_before =
            std::fs::read_to_string(root.path().join("jev-effects.jsonl")).unwrap();
        unsafe { std::env::remove_var(credential_env) };

        assert_eq!(
            respawn_decision_with_jev(&cfg, &state, &card, ExitKind::Crash, DEFAULT_MAX_ATTEMPTS),
            RespawnVerdict::Respawn
        );
        assert_eq!(
            std::fs::read_to_string(root.path().join("jev-decisions.jsonl")).unwrap(),
            before
        );
        assert_eq!(
            std::fs::read_to_string(root.path().join("jev-effects.jsonl")).unwrap(),
            effects_before
        );
    }

    // -- CLI verbs, end to end ------------------------------------------------

    #[test]
    fn create_then_claim_then_complete_round_trips_through_the_event_log() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().to_path_buf());
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(&repo).expect("mkdir");
        let _cwd = super::super::testenv::CwdGuard::enter(&repo).expect("enter repo");

        let mut out = Vec::new();
        let id = run_create(
            &state,
            &mut out,
            &CreateArgs {
                title: "do the thing".to_string(),
                brief: "do it well".to_string(),
                parents: Vec::new(),
                group: None,
                workdir: None,
            },
            1_000,
        )
        .expect("create");

        let env: std::collections::HashMap<String, String> = std::collections::HashMap::new();
        let lookup: EnvLookup<'_> = &|k| env.get(k).cloned();

        let mut claim_out = Vec::new();
        let code = run_claim(
            &state,
            &mut claim_out,
            &ClaimArgs {
                id: id.clone(),
                session: Some("sess-1".to_string()),
                ttl_secs: None,
            },
            lookup,
            1_100,
        )
        .expect("claim");
        assert_eq!(code, 0);

        let repo_slug = super::super::state::repo_slug(&repo);
        let cards = load_cards(&state, &repo_slug);
        assert_eq!(cards[&id].state, State::Running);

        let mut complete_out = Vec::new();
        run_complete(
            &state,
            &mut complete_out,
            &CompleteArgs {
                id: id.clone(),
                outcome: "shipped".to_string(),
            },
            1_200,
        )
        .expect("complete");

        let cards = load_cards(&state, &repo_slug);
        assert_eq!(cards[&id].state, State::Done);
        assert_eq!(cards[&id].outcome.as_deref(), Some("shipped"));
    }

    #[test]
    fn claim_refuses_a_card_with_a_parent_that_is_not_yet_done() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().to_path_buf());
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(&repo).expect("mkdir");
        let _cwd = super::super::testenv::CwdGuard::enter(&repo).expect("enter repo");

        let mut out = Vec::new();
        let parent_id = run_create(
            &state,
            &mut out,
            &CreateArgs {
                title: "parent".to_string(),
                brief: "b".to_string(),
                parents: Vec::new(),
                group: None,
                workdir: None,
            },
            1_000,
        )
        .expect("create parent");
        let child_id = run_create(
            &state,
            &mut out,
            &CreateArgs {
                title: "child".to_string(),
                brief: "b".to_string(),
                parents: vec![parent_id.clone()],
                group: None,
                workdir: None,
            },
            1_000,
        )
        .expect("create child");

        let env: std::collections::HashMap<String, String> = std::collections::HashMap::new();
        let lookup: EnvLookup<'_> = &|k| env.get(k).cloned();
        let mut claim_out = Vec::new();
        let code = run_claim(
            &state,
            &mut claim_out,
            &ClaimArgs {
                id: child_id,
                session: Some("sess-1".to_string()),
                ttl_secs: None,
            },
            lookup,
            1_100,
        )
        .expect("claim attempt");
        assert_eq!(code, 1, "the parent has not completed yet");
        assert!(
            String::from_utf8(claim_out)
                .expect("utf8")
                .contains("cannot claim")
        );
    }

    /// Issue #317 review finding: the tasks file lock used to be held only
    /// inside `append_event`/`append_events_atomic`, so two concurrent
    /// claims of the same `Ready` card could both read the pre-claim state,
    /// both pass the pure `claim()` check, and both append `Claimed`. This
    /// proves `claim_locked` (the fix: one locked read -> decide -> append
    /// helper, `with_task_lock`) closes that window -- a caller's own
    /// earlier, now-stale read of the card is NEVER what a second claim
    /// attempt decides against: `claim_locked` always re-reads under the
    /// lock, so of two claims through it, only the first can ever succeed.
    #[test]
    fn claim_locked_refuses_when_the_card_was_already_claimed_since_an_earlier_stale_read() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().to_path_buf());
        let repo_slug = "repo".to_string();

        append_event(
            &state,
            &repo_slug,
            &Event::Created {
                id: "t1".to_string(),
                repo_slug: repo_slug.clone(),
                title: "t".to_string(),
                brief: "b".to_string(),
                parents: Vec::new(),
                group_id: None,
                workdir: None,
                at: 1_000,
            },
        )
        .expect("create");

        // A caller takes an early, now-stale snapshot -- the shape of the
        // read a naive read-then-append implementation would have decided
        // against.
        let stale_view = load_cards(&state, &repo_slug);
        assert_eq!(stale_view["t1"].state, State::Ready, "stale view is Ready");

        // A first caller claims the card through the locked helper.
        let first = claim_locked(
            &state,
            &repo_slug,
            "t1",
            "sess-first",
            111,
            None,
            "host",
            1_100,
            900,
        )
        .expect("claim_locked io")
        .expect("card exists")
        .expect("the first claim succeeds");
        assert_eq!(first.state, State::Running);
        assert_eq!(first.claim.expect("claim set").session, "sess-first");

        // A second caller, deciding from the SAME stale (pre-claim) view a
        // naive implementation would have trusted, attempts to claim the
        // same card through the SAME locked helper. It must be refused --
        // never a second `Claimed` event on top of the first.
        assert_eq!(stale_view["t1"].state, State::Ready, "still says Ready");
        let second = claim_locked(
            &state,
            &repo_slug,
            "t1",
            "sess-second",
            222,
            None,
            "host",
            1_200,
            900,
        )
        .expect("claim_locked io")
        .expect("card exists");
        assert_eq!(
            second,
            Err(Refusal::NotReady(State::Running)),
            "the locked re-read sees the first claim, never the caller's stale view"
        );

        // Exactly one `Claimed` event was ever appended.
        let claimed_events = read_events(&state, &repo_slug)
            .into_iter()
            .filter(|e| matches!(e, Event::Claimed { .. }))
            .count();
        assert_eq!(claimed_events, 1, "only the first claim ever wrote Claimed");
    }

    /// Issue #720 acceptance: `reap_all_locked` heals every stuck `Running`
    /// card with a dead claimant in ONE pass -- unlike `claim_locked`, which
    /// only ever reaps the one id being claimed -- while a card whose
    /// claimant is genuinely alive is left completely untouched, no matter
    /// how far past its TTL it is (`reap`'s own liveness-only gate).
    #[test]
    fn reap_all_locked_heals_every_stale_card_in_one_pass_and_leaves_a_live_claimant_alone() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().to_path_buf());
        let repo_slug = "repo".to_string();

        let dead_pid = super::super::testenv::dead_pid();
        let live_pid = std::process::id();

        for (id, pid) in [("t1", dead_pid), ("t2", dead_pid), ("t3", live_pid)] {
            append_event(
                &state,
                &repo_slug,
                &Event::Created {
                    id: id.to_string(),
                    repo_slug: repo_slug.clone(),
                    title: id.to_string(),
                    brief: "b".to_string(),
                    parents: Vec::new(),
                    group_id: None,
                    workdir: None,
                    at: 1_000,
                },
            )
            .expect("create");
            append_event(
                &state,
                &repo_slug,
                &Event::Claimed {
                    id: id.to_string(),
                    claim: Claim {
                        session: "sess-1".to_string(),
                        pid,
                        pid_start_time: None,
                        host: "h".to_string(),
                        claimed_at: 1_000,
                        ttl_secs: 900,
                    },
                    attempts: 1,
                    at: 1_000,
                },
            )
            .expect("claimed");
        }

        // Well past every claim's TTL, and no `claim`/`heartbeat` call for
        // any of these ids happens anywhere in this test -- the whole point
        // is that this heals without one.
        let now = 1_000 + 900 + 1;
        let reaped = reap_all_locked(&state, &repo_slug, now).expect("reap_all_locked io");
        assert_eq!(
            reaped,
            vec!["t1".to_string(), "t2".to_string()],
            "both dead-claimant cards are reaped, in id order"
        );

        let cards = load_cards(&state, &repo_slug);
        assert_eq!(cards["t1"].state, State::Ready);
        assert!(cards["t1"].claim.is_none());
        assert_eq!(cards["t2"].state, State::Ready);
        assert!(cards["t2"].claim.is_none());
        assert_eq!(
            cards["t3"].state,
            State::Running,
            "a live claimant is never reaped, no matter how far past its TTL"
        );
        assert!(cards["t3"].claim.is_some());

        // Idempotent: a second pass over the now-healed ledger reaps nothing
        // further.
        assert!(
            reap_all_locked(&state, &repo_slug, now)
                .expect("second pass io")
                .is_empty()
        );
    }

    /// The dry-run preview makes the identical decision as the locked pass,
    /// without ever taking the lock or writing anything.
    #[test]
    fn reap_all_dry_previews_the_same_ids_reap_all_locked_would_reap() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().to_path_buf());
        let repo_slug = "repo".to_string();
        let dead_pid = super::super::testenv::dead_pid();

        append_event(
            &state,
            &repo_slug,
            &Event::Created {
                id: "t1".to_string(),
                repo_slug: repo_slug.clone(),
                title: "t".to_string(),
                brief: "b".to_string(),
                parents: Vec::new(),
                group_id: None,
                workdir: None,
                at: 1_000,
            },
        )
        .expect("create");
        append_event(
            &state,
            &repo_slug,
            &Event::Claimed {
                id: "t1".to_string(),
                claim: Claim {
                    session: "sess-1".to_string(),
                    pid: dead_pid,
                    pid_start_time: None,
                    host: "h".to_string(),
                    claimed_at: 1_000,
                    ttl_secs: 900,
                },
                attempts: 1,
                at: 1_000,
            },
        )
        .expect("claimed");

        let now = 1_000 + 900 + 1;
        let events_before = read_events(&state, &repo_slug);
        assert_eq!(
            reap_all_dry(&state, &repo_slug, now),
            vec!["t1".to_string()]
        );
        assert_eq!(
            read_events(&state, &repo_slug),
            events_before,
            "the dry-run preview must never append anything"
        );
        assert_eq!(
            load_cards(&state, &repo_slug)["t1"].state,
            State::Running,
            "the dry-run preview must never mutate the card"
        );
    }

    #[test]
    fn block_and_unblock_verbs_round_trip_through_the_event_log() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().to_path_buf());
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(&repo).expect("mkdir");
        let _cwd = super::super::testenv::CwdGuard::enter(&repo).expect("enter repo");

        let mut out = Vec::new();
        let id = run_create(
            &state,
            &mut out,
            &CreateArgs {
                title: "t".to_string(),
                brief: "b".to_string(),
                parents: Vec::new(),
                group: None,
                workdir: None,
            },
            1_000,
        )
        .expect("create");

        let repo_slug = super::super::state::repo_slug(&repo);
        run_block(
            &state,
            &mut Vec::new(),
            &BlockArgs {
                id: id.clone(),
                reason: "waiting on review".to_string(),
                by: None,
            },
            1_100,
        )
        .expect("block");
        assert_eq!(load_cards(&state, &repo_slug)[&id].state, State::Blocked);

        run_unblock(
            &state,
            &mut Vec::new(),
            &UnblockArgs { id: id.clone() },
            1_200,
        )
        .expect("unblock");
        assert_eq!(load_cards(&state, &repo_slug)[&id].state, State::Ready);
    }

    #[test]
    fn comment_and_archive_verbs_round_trip_through_the_event_log() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().to_path_buf());
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(&repo).expect("mkdir");
        let _cwd = super::super::testenv::CwdGuard::enter(&repo).expect("enter repo");

        let mut out = Vec::new();
        let id = run_create(
            &state,
            &mut out,
            &CreateArgs {
                title: "t".to_string(),
                brief: "b".to_string(),
                parents: Vec::new(),
                group: None,
                workdir: None,
            },
            1_000,
        )
        .expect("create");

        run_comment(
            &state,
            &mut Vec::new(),
            &CommentArgs {
                id: id.clone(),
                text: "looking into it".to_string(),
                by: Some("sess-1".to_string()),
            },
            1_100,
        )
        .expect("comment");
        // `archive` only accepts Done/Blocked -- complete it first so the
        // archive below is a valid transition.
        run_complete(
            &state,
            &mut Vec::new(),
            &CompleteArgs {
                id: id.clone(),
                outcome: "shipped".to_string(),
            },
            1_150,
        )
        .expect("complete");
        run_archive(
            &state,
            &mut Vec::new(),
            &ArchiveArgs { id: id.clone() },
            1_200,
        )
        .expect("archive");

        let repo_slug = super::super::state::repo_slug(&repo);
        let cards = load_cards(&state, &repo_slug);
        assert_eq!(cards[&id].comments.len(), 1);
        assert_eq!(cards[&id].state, State::Archived);
    }

    #[test]
    fn heartbeat_verb_refuses_when_the_claimant_pid_is_dead() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().to_path_buf());
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(&repo).expect("mkdir");
        let _cwd = super::super::testenv::CwdGuard::enter(&repo).expect("enter repo");

        let repo_slug = super::super::state::repo_slug(&repo);
        append_event(
            &state,
            &repo_slug,
            &Event::Created {
                id: "t1".to_string(),
                repo_slug: repo_slug.clone(),
                title: "t".to_string(),
                brief: "b".to_string(),
                parents: Vec::new(),
                group_id: None,
                workdir: None,
                at: 1_000,
            },
        )
        .expect("create");
        append_event(
            &state,
            &repo_slug,
            &Event::Claimed {
                id: "t1".to_string(),
                claim: Claim {
                    session: "sess-1".to_string(),
                    pid: super::super::testenv::dead_pid(),
                    pid_start_time: None,
                    host: "h".to_string(),
                    claimed_at: 1_000,
                    ttl_secs: 900,
                },
                attempts: 1,
                at: 1_000,
            },
        )
        .expect("claimed");

        let env: std::collections::HashMap<String, String> = std::collections::HashMap::new();
        let lookup: EnvLookup<'_> = &|k| env.get(k).cloned();
        let mut out = Vec::new();
        let code = run_heartbeat(
            &state,
            &mut out,
            &HeartbeatArgs {
                id: "t1".to_string(),
                session: Some("sess-1".to_string()),
            },
            lookup,
            2_000,
        )
        .expect("heartbeat attempt");
        assert_eq!(code, 1);
        assert!(
            String::from_utf8(out)
                .expect("utf8")
                .contains("no longer alive")
        );
    }

    // -- swarm ----------------------------------------------------------------

    #[test]
    fn build_swarm_events_creates_root_plus_n_workers_plus_verifier_plus_synthesizer() {
        let (events, ids) = build_swarm_events("ship it", "repo", 3, None, 1_000).expect("build");
        assert_eq!(events.len(), 1 + 3 + 1 + 1);
        assert_eq!(ids.workers.len(), 3);

        let cards = materialize(&events);
        assert_eq!(cards[&ids.root].state, State::Ready, "no parents");
        for worker in &ids.workers {
            assert_eq!(cards[worker].parents, vec![ids.root.clone()]);
            assert_eq!(cards[worker].state, State::Todo);
        }
        assert_eq!(cards[&ids.verifier].parents, ids.workers);
        assert_eq!(cards[&ids.synthesizer].parents, vec![ids.verifier.clone()]);
    }

    #[test]
    fn build_swarm_events_refuses_zero_workers() {
        assert!(build_swarm_events("scope", "repo", 0, None, 1_000).is_err());
    }

    #[test]
    fn swarm_writes_every_card_in_one_atomic_batch() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().to_path_buf());
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(&repo).expect("mkdir");
        let _cwd = super::super::testenv::CwdGuard::enter(&repo).expect("enter repo");

        let mut out = Vec::new();
        let code = run_swarm_with(
            &state,
            &mut out,
            &SwarmArgs {
                scope: "ship it".to_string(),
                workers: 2,
                group: None,
            },
            1_000,
        )
        .expect("swarm");
        assert_eq!(code, 0);

        let repo_slug = super::super::state::repo_slug(&repo);
        let cards = load_cards(&state, &repo_slug);
        // root + 2 workers + verifier + synthesizer.
        assert_eq!(cards.len(), 5);
    }

    /// Issue #317 acceptance: a simulated failure partway through building
    /// the swarm batch (before the single atomic write) leaves no orphaned
    /// cards on disk at all.
    #[test]
    fn a_failed_swarm_build_leaves_no_orphaned_cards() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().to_path_buf());
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(&repo).expect("mkdir");
        let _cwd = super::super::testenv::CwdGuard::enter(&repo).expect("enter repo");

        let mut out = Vec::new();
        let code = run_swarm_with(
            &state,
            &mut out,
            &SwarmArgs {
                scope: "ship it".to_string(),
                workers: 0,
                group: None,
            },
            1_000,
        )
        .expect("swarm refusal is not an error");
        assert_eq!(code, 1);

        let repo_slug = super::super::state::repo_slug(&repo);
        assert!(
            load_cards(&state, &repo_slug).is_empty(),
            "nothing must be written when the batch build itself fails"
        );
    }

    #[test]
    fn list_json_round_trips_a_card() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().to_path_buf());
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(&repo).expect("mkdir");
        let _cwd = super::super::testenv::CwdGuard::enter(&repo).expect("enter repo");

        let mut out = Vec::new();
        run_create(
            &state,
            &mut out,
            &CreateArgs {
                title: "t".to_string(),
                brief: "b".to_string(),
                parents: Vec::new(),
                group: None,
                workdir: None,
            },
            1_000,
        )
        .expect("create");

        let mut json_out = Vec::new();
        run_list(&state, &mut json_out, &ListArgs { json: true }, 1_100).expect("list");
        let text = String::from_utf8(json_out).expect("utf8");
        let value: serde_json::Value = serde_json::from_str(&text).expect("json");
        assert_eq!(value.as_array().expect("array").len(), 1);
        assert_eq!(value[0]["title"], "t");
    }
}
