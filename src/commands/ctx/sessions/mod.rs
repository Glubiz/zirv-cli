//! Session registry: `<state>/sessions/<short8>.json`, one file per live
//! supervisor, keyed by the same short id `StateDir::socket_for` names its
//! turn-signal socket after. Best-effort throughout, matching the rest of
//! the state dir's own housekeeping: a registry write, refresh or removal
//! that fails must never fail a launch, and a listing must never fail just
//! because one file on disk is unreadable or malformed.
//!
//! `state.rs` is shared with a concurrent change adding `memory()`; this
//! module only ever calls `StateDir::sessions()` from there rather than
//! reaching into its internals, so the two changes stay independent.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::config::{CtxConfig, EnvLookup, env_from_process};
use super::runtime::RuntimeKind;
use super::state::{self, StateDir};

mod bookkeeping;
mod nesting;
mod ops;

#[cfg(test)]
use super::testenv;
use super::{
    adapters, agent, announce, config, dash, mail, rollover, screen, seat, supervise, wrap,
};

pub(super) use bookkeeping::launch_consuming_interrupted;
#[cfg(test)]
pub use bookkeeping::set_last_screening;
pub use bookkeeping::{
    bind_workflow_id, last_screening, native_conversation, peek_interrupted_in_flight,
    record_conversation_on, record_native_conversation, record_screening,
    take_interrupted_in_flight, workflow_id_for,
};
use bookkeeping::{
    screening_path, sweep_orphaned_screening_summaries, sweep_orphaned_workflow_markers,
    workflow_path,
};
#[cfg(test)]
pub use nesting::{ALLOW_NESTED_ENV, SUPERVISION_ENV};
pub(crate) use nesting::{OwnerLiveness, dashboard_owner_liveness};
pub use nesting::{nesting_refusal, scrub_supervision_env, scrub_supervision_env_cmd};
pub(crate) use ops::notify_mail;
use ops::sweep_orphaned_markers;
pub use ops::{
    KillArgs, NudgeArgs, claim_nudge_marker, clear_stall_marker, delivery_filter, run_kill,
    run_nudge, run_nudge_with, stall_marker, write_stall_marker,
};

/// Mirrors `StateDir::socket_for`'s own derivation exactly: the first eight
/// ASCII-alphanumeric characters of the session id. Duplicated rather than
/// factored out of `state.rs` (the one file a concurrent change also
/// touches) -- `the_record_key_is_the_same_short_id_the_socket_is_named_
/// after` below pins the two derivations against each other so a future
/// edit to either cannot drift silently.
pub fn short_id(session: &str) -> String {
    session
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .take(8)
        .collect()
}

fn non_empty(value: Option<String>) -> Option<String> {
    value
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

/// Which supervisor filed a record. `Chat` is `wrap`'s own orchestrator
/// launch, threaded through as a distinct verb from `chat.rs` rather than
/// derived from `PromptRole`: the two are independent facts about a session
/// (role governs prompt injection permissions; verb only names the calling
/// verb for the registry).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Verb {
    Exec,
    Loop,
    Wrap,
    Chat,
    /// A dashboard worker pane (`zirv ctx dash`'s own supervised child):
    /// distinct from `Chat`, which stays the dashboard's own orchestrator
    /// pane, so a registry row can tell "the orchestrator" from "a pane the
    /// dashboard spawned" apart.
    Dash,
}

impl Verb {
    pub fn as_str(&self) -> &'static str {
        match self {
            Verb::Exec => "exec",
            Verb::Loop => "loop",
            Verb::Wrap => "wrap",
            Verb::Chat => "chat",
            Verb::Dash => "dash",
        }
    }
}

impl std::fmt::Display for Verb {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Record {
    pub session: String,
    pub short: String,
    pub agent: String,
    pub repo: PathBuf,
    pub repo_slug: String,
    pub verb: Verb,
    pub pid: u32,
    pub started_at: u64,
    /// NEW-3: whether this supervisor can actually *act* on a wake-up.
    ///
    /// A supervisor only claims nudge markers from its turn-signal arm, so
    /// one that never bound a `SignalServer` (`wrap --no-supervise`, or a
    /// bind that failed) can never notice a nudge -- not even to advise
    /// about it. Such a session used to be dropped from the registry
    /// entirely, which fixed the silent-nudge bug by making the session
    /// invisible: it vanished from `zirv ctx status` too, so an operator
    /// watching a bind-failed `wrap` simply could not see it was running.
    ///
    /// Recorded instead of hidden: `status` renders it as `unreachable`, and
    /// `nudge` refuses it with a reason. `#[serde(default = ...)]` returns
    /// `true` so a record written by an older build still parses as a normal
    /// reachable session.
    #[serde(default = "reachable_default")]
    pub reachable: bool,
    /// The process that registered this session, stamped by
    /// [`SessionGuard::register`] itself (unless a caller already set it) --
    /// so it is always `Some(pid)` for a record written by this build, and
    /// `None` only for one written by an older build before this field
    /// existed. A dashboard pane carries the *dashboard's* pid because
    /// `Pane::new` registers from inside the dashboard process itself; any
    /// other session simply carries the pid of whichever process actually
    /// called `register`. That is not always the dashboard even for a
    /// session a dashboard pane is morally responsible for: `zirv ctx
    /// agent`'s dashboard-refused-but-retryable fallback (`agent.rs`'s
    /// headless path, `agent.rs:126` onward into `exec::run_with`) runs in
    /// the *requester's* process -- a pane's own child shell, or a plain
    /// terminal -- never inside the dashboard's, so that fallback session
    /// registers the requester's pid and is not shown in that dashboard's
    /// sidebar. Accepted residual: pid-based ownership has no way to express
    /// "spawned on this dashboard's behalf, but from outside its process," so
    /// that session is only visible via mail (its own report-back) and `zirv
    /// ctx status`, same as any other unowned-by-this-dashboard record. The
    /// dashboard sidebar merge (`dash::assemble_sidebar`) keeps only records
    /// whose `owner_pid` matches its own pid, so a second, concurrently
    /// running dashboard's panes never bleed into this one's panel.
    /// `#[serde(default)]` so an on-disk record from an older build
    /// deserializes as `None` rather than failing to parse.
    #[serde(default)]
    pub owner_pid: Option<u32>,
    /// Issue #139: the `safety::policy_fingerprint` of the LAUNCH-TIME
    /// snapshot this session was pinned to (the same fingerprint value
    /// written to `POLICY_FINGERPRINT_ENV`/read back by `evaluate_with_
    /// attestation_evidence`), when the launch computed one at all. `None`
    /// for a record written by an older build, a launch that never
    /// attempted attestation (no adapter support, or the fingerprint could
    /// not be computed), or any session type this field is not yet threaded
    /// through to. `status.rs` compares this against a freshly loaded
    /// policy's own fingerprint for `record.repo` to surface a "policy
    /// snapshot stale" line -- see `Modules/Ctx Subsystem.md`.
    /// `#[serde(default)]` so an on-disk record from an older build
    /// deserializes as `None` rather than failing to parse.
    #[serde(default)]
    pub safety_policy_sha256: Option<String>,
    /// Issue #169: the `prompt::PromptRole` label (`"orchestrator"`,
    /// `"sub-orchestrator"` or `"worker"`) this session was ACTUALLY spawned
    /// with, stamped once by the server that spawned it (`Pane::spawn`,
    /// `wrap::run_with`) -- never by anything the session itself later
    /// claims. Plain `String`, not the `prompt::PromptRole` type itself: this
    /// module has no reason to depend on `prompt.rs`, and every other
    /// plain-vocabulary field here (`agent`, `roster::RosterPane::role`)
    /// already follows the same "label string, not an enum" convention.
    ///
    /// Read by `dash::mod::parent_role_for` (via [`load_record`]) for a
    /// requesting session this dashboard hosts no pane for -- an operator's
    /// own terminal, or a headless coordinator -- which is the only place a
    /// role can be recovered for such a session at all. `None` for a record
    /// written by an older build, or any session type this was never threaded
    /// through to; that reader then falls back to the verb (`Verb::Chat` is
    /// an orchestrator seat, anything else a worker), never to a wider role
    /// than the session could already have had.
    #[serde(default)]
    pub role: Option<String>,
    /// Issue #152: epoch seconds the process that registered this session
    /// itself started, stamped once by [`Record::new`] via
    /// [`process_start_secs`]. Exists so `record_is_alive` can tell the
    /// original process apart from an unrelated one the OS later recycles
    /// this record's `pid` to -- `is_alive`'s own `EPERM` branch has no way
    /// to make that distinction with the pid alone (see its doc comment).
    /// `None` for a record written by an older build, a non-unix platform
    /// (`process_start_secs` has no reader there), or any environment
    /// `process_start_secs` could not read (no `ps` on `PATH`, refused,
    /// unparsable output) -- every one of those degrades `record_is_alive`
    /// back to today's EPERM-is-alive behavior, never to a false "dead".
    /// `#[serde(default)]` so an on-disk record from an older build
    /// deserializes as `None` rather than failing to parse, the same
    /// back-compat pattern `owner_pid` already established.
    #[serde(default)]
    pub start_time: Option<u64>,
    /// Issue #281: set while a turn is actively being worked, cleared once it
    /// reaches a clean boundary -- see [`InFlight`]'s own doc comment. `None`
    /// for a record written by an older build, the ordinary "idle between
    /// turns" state, or once [`take_interrupted_in_flight`] has consumed it.
    /// `#[serde(default)]` so an on-disk record from before this field
    /// existed deserializes as `None`, the same back-compat pattern every
    /// other optional field on this struct already follows.
    #[serde(default)]
    pub in_flight: Option<InFlight>,
    /// Issue #470: which backend actually drives this session --
    /// `runtime::RuntimeKind::Harness` for every session this build spawns
    /// today. `#[serde(default)]` so a record written by an older build
    /// (before this field existed) deserializes as `Harness`, the same
    /// value `Record::new` itself always stamps right now -- the only
    /// runtime this codebase can actually run a session under yet.
    #[serde(default)]
    pub runtime: RuntimeKind,
}

/// Issue #281: the crash-interruption witness marker. Stamped by a
/// supervisor ([`SessionGuard::stamp_in_flight`]) when a turn starts and
/// cleared ([`SessionGuard::clear_in_flight`]) once that turn reaches a
/// clean boundary -- a turn signal, for both `wrap.rs` and `exec.rs`. A
/// record still carrying one after its own process has died
/// ([`record_is_alive`] is `false`) means that process stopped mid-turn
/// rather than at a clean boundary: [`take_interrupted_in_flight`] is what a
/// resumed session's injection reads this through.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InFlight {
    /// The supervisor verb doing the work (`Verb::as_str()` -- `"wrap"`,
    /// `"exec"`, ...), not a per-tool label: this witness only ever needs to
    /// say WHICH supervised run was interrupted, not what it was doing.
    pub verb: String,
    /// The turn number this supervisor last knew about when it stamped this
    /// marker -- `InjectionState::last_turn + 1` for `wrap`, the transcript's
    /// own turn count for `exec`. Display only, like `InjectionState::
    /// last_turn` itself.
    pub turn: u64,
    pub since: u64,
}

fn reachable_default() -> bool {
    true
}

impl Record {
    /// `pid` is always this process's own: a registry entry describes the
    /// supervisor that filed it, and every registration happens from inside
    /// that same process.
    pub fn new(session: &str, agent: &str, repo: &Path, verb: Verb) -> Self {
        Self {
            session: session.to_string(),
            short: short_id(session),
            agent: agent.to_string(),
            repo: repo.to_path_buf(),
            repo_slug: super::state::repo_slug(repo),
            verb,
            pid: std::process::id(),
            started_at: super::state::now_secs(),
            // Reachable unless a caller says otherwise: every headless
            // supervisor binds a socket as a matter of course, so only
            // `wrap` (which can run `--no-supervise`, or fail to bind) has
            // any reason to call `unreachable()` below.
            reachable: true,
            // Left unset here: `SessionGuard::register` stamps this
            // process's own pid on the way to disk, unless a caller has
            // already set one (see its own doc comment).
            owner_pid: None,
            // Left unset here too: only a caller that actually resolved a
            // launch-time policy snapshot (and its fingerprint) has
            // anything to record -- see `with_safety_policy_sha256`.
            safety_policy_sha256: None,
            // Left unset here too: only a caller that actually knows the
            // role it spawned (`with_role`) has anything to record.
            role: None,
            // Issue #152: this process's own start time, read the same way
            // `record_is_alive` will later re-read whoever holds this pid --
            // see the field's own doc comment. `None` wherever
            // `process_start_secs` cannot tell, which callers other than
            // `record_is_alive` never need to know about.
            start_time: process_start_secs(std::process::id()),
            // Left unset here too: nothing is in flight until a supervisor's
            // own `stamp_in_flight` call says otherwise.
            in_flight: None,
            // Issue #470: every session this build spawns runs on the
            // existing harness-process backend.
            runtime: RuntimeKind::Harness,
        }
    }

    /// Marks this record as one that can never act on a wake-up -- see the
    /// `reachable` field. Chained onto `new` at the one call site that knows
    /// whether a turn-signal socket actually bound.
    pub fn unreachable(mut self) -> Self {
        self.reachable = false;
        self
    }

    /// Issue #139: stamps the launch-time safety-policy fingerprint (see the
    /// field's own doc comment), chained onto `new` at whichever call site
    /// already resolved one for this launch. `None` is a legitimate value
    /// (leaves the field unset, the same as never calling this at all) so a
    /// caller that only sometimes has a fingerprint (e.g. attestation is
    /// disabled, or fingerprinting failed) does not need its own branch.
    pub fn with_safety_policy_sha256(mut self, fingerprint: Option<String>) -> Self {
        self.safety_policy_sha256 = fingerprint;
        self
    }

    /// Issue #169: stamps the role (a `prompt::PromptRole::label()` string)
    /// this session was actually spawned with, chained onto `new` at the
    /// call site that resolved one. Forgery-proof by construction: the
    /// caller is the server that decided what to spawn (`Pane::spawn`'s own
    /// `PaneSpec::role`, `wrap::run_with`'s own `role` parameter), never
    /// anything read back from the session's own request.
    pub fn with_role(mut self, role: &str) -> Self {
        self.role = Some(role.to_string());
        self
    }

    /// Issue #186 hardening: preserves a supervisor's already-established
    /// delivery address while the underlying vendor session changes. This is
    /// only used by Zirv's own cross-harness continuation path; callers must
    /// supply the short id of the logical supervisor that is being continued.
    pub fn with_stable_short(mut self, short: &str) -> Self {
        if !short.is_empty() {
            self.short = short.to_string();
        }
        self
    }
}

fn record_path(state: &StateDir, short: &str) -> PathBuf {
    state.sessions().join(format!("{short}.json"))
}

/// Best-effort write: a registry that cannot be written must never be the
/// reason a launch fails, matching every other piece of state-dir
/// housekeeping in this codebase.
fn write_record(state: &StateDir, record: &Record) -> PathBuf {
    let path = record_path(state, &record.short);
    let _ = super::state::create_private_dir_all(&state.sessions());
    if let Ok(json) = serde_json::to_string_pretty(record) {
        let _ = super::state::write_private(&path, &json);
    }
    path
}

/// Registered at spawn, best-effort, and removed when the supervisor exits.
/// `Drop` covers a panic-free early return; `release()` is called explicitly
/// in every arm that leaves the supervisor loop, the same explicit-arm
/// discipline `RawGuard` follows because this binary's release profile is
/// `panic = "abort"` and Drop is therefore not guaranteed to run.
#[derive(Debug)]
pub struct SessionGuard {
    state: StateDir,
    record: Record,
    path: PathBuf,
    released: bool,
}

impl SessionGuard {
    /// Stamps `owner_pid` with this process's own pid before writing the
    /// record, unless the caller already set one -- see the field's own doc
    /// comment. Every registration path goes through this one function, so
    /// this is the single seam: a dashboard pane and a standalone `wrap`/
    /// `exec`/`loop` session both end up attributed to whichever process
    /// actually called this, without each caller having to remember to stamp
    /// itself. That is the dashboard's own pid for a pane (registered from
    /// inside the dashboard process) and the calling process's own pid for
    /// everything else -- including `zirv ctx agent`'s headless fallback,
    /// which runs in the *requester's* process rather than the dashboard's
    /// even when the request named one (see the field's own doc comment for
    /// that residual).
    pub fn register(state: &StateDir, mut record: Record) -> Self {
        if record.owner_pid.is_none() {
            record.owner_pid = Some(std::process::id());
        }
        let path = write_record(state, &record);
        Self {
            state: state.clone(),
            record,
            path,
            released: false,
        }
    }

    // Issue #281: `wrap.rs`'s pump loop reads `.verb` off this to label its
    // `stamp_in_flight` calls with the actual verb this guard was
    // registered under (`Wrap` or `Chat` -- `chat.rs` reuses `wrap::
    // run_with`), rather than assuming `"wrap"` unconditionally.
    pub fn record(&self) -> &Record {
        &self.record
    }

    /// Points this run's record at a new session id: `loop`'s per-cycle
    /// refresh, and `exec`'s per-restart one. One guard, and one record, for
    /// the whole supervised run.
    ///
    /// C7: `short` and the record's path are deliberately **not** refreshed
    /// with it. The short id is this supervisor's *address* -- what
    /// `resolve_prefix` hands a sender, what `send --to-session` and `zirv
    /// ctx nudge` store on a message, and what `zirv ctx status` prints for
    /// a human to type. Rotating it every cycle or restart meant a message
    /// addressed to a live session became permanently undeliverable the
    /// moment that session was replaced, which is the whole "stranded mail"
    /// class of bug: the sender resolved a real address, and the supervisor
    /// then stopped answering to it. The session *id* rotates (that is the
    /// point of a fresh session); the address it can be reached at does not.
    pub fn refresh_session(&mut self, new_session: &str) {
        if self.released {
            return;
        }
        // Review round 1, finding 7: the OLD session id's own memory tier
        // (`memory::MemoryScope::Session`, issue #295) is not this run's
        // stable address -- `short` is -- so a `loop`/`exec` cycle that
        // rotates `record.session` would otherwise leave that tier's
        // directory behind forever, cleaned up only when `release()`
        // eventually fires for whichever session id happens to be current
        // at that point. Purged here instead, best-effort, the same way
        // every other memory cleanup in this module is: a failed removal
        // costs a leaked directory, never data loss for anything still
        // live (a no-op if the old id never wrote anything there).
        let old_session = std::mem::replace(&mut self.record.session, new_session.to_string());
        if old_session != new_session {
            let _ = super::memory::forget_session_all(
                &self.state,
                &self.record.repo_slug,
                &old_session,
            );
        }
        self.record.started_at = super::state::now_secs();
        self.path = write_record(&self.state, &self.record);
    }

    /// Points this run's record at the pid of the agent child the supervisor
    /// actually spawned, rather than at the supervisor's own pid.
    ///
    /// P5: `Record::new` stamps `std::process::id()`, which for `wrap` is
    /// zirv's pid -- so a `wrap` record stayed "alive" (and stayed offered
    /// for restore, and stayed nudge-targetable) for exactly as long as the
    /// *wrapper* lived, whether or not the agent underneath it was still
    /// there. `dash::pane::Pane::spawn` has always stamped the real child pid
    /// for the same reason; this is that same override, on the one seam
    /// `wrap` has for it. Called right after the spawn, and again after every
    /// relaunch: a record left pointing at a replaced child's dead pid would
    /// be swept by `list` and the live session would vanish from `zirv ctx
    /// status`.
    ///
    /// `owner_pid` is deliberately untouched -- it answers "which process
    /// filed this record", which is still zirv's own, and is what
    /// `dash::assemble_sidebar` scopes its panel by. Like every other write
    /// here, best-effort: `short` and the record's path do not move (see
    /// `refresh_session`), so a failed write costs a stale pid, never an
    /// address.
    ///
    /// Review round 2 finding 1 (issue #152): `start_time` MUST move with
    /// `pid`, not just `pid` alone. `record_is_alive`'s `EPERM` branch
    /// compares whoever currently holds `record.pid` against `record.
    /// start_time` -- leaving the old value in place after repointing `pid`
    /// at a fresh child would compare the CHILD's real start time against
    /// the SUPERVISOR's, which is a guaranteed mismatch (the child always
    /// starts meaningfully after the supervisor that goes on to spawn it).
    /// That is not a hypothetical: it is exactly the everyday sandboxed
    /// case issue #146 was written for -- a live child, probed with `EPERM`
    /// -- and would have `list` delete a perfectly live pane's record.
    /// Re-reading via `process_start_secs(pid)` keeps the two in lockstep;
    /// `None` (no usable `ps`) degrades `start_time` to `None` too, which
    /// is `record_is_alive`'s own "cannot tell" case, never a false mismatch.
    pub fn adopt_child_pid(&mut self, pid: u32) {
        if self.released || self.record.pid == pid {
            return;
        }
        self.record.pid = pid;
        self.record.start_time = process_start_secs(pid);
        self.path = write_record(&self.state, &self.record);
    }

    /// Issue #281: stamps `Record::in_flight`, marking a turn as started.
    /// Called from `wrap.rs`'s pty spawn/relaunch sites and its pump loop's
    /// `PumpEvent::Input` arm, and from `exec.rs`'s per-cycle spawn -- the
    /// existing edges each supervisor already observes for a turn beginning.
    /// Best-effort, like every other registry write here: a failed write
    /// costs a missed witness on a future crash, never this turn itself.
    /// Idempotent per turn: `wrap`'s `Input` arm fires on every keystroke
    /// chunk, so only the first chunk of a turn touches the disk.
    pub fn stamp_in_flight(&mut self, verb: &str, turn: u64) {
        if self.released
            || self
                .record
                .in_flight
                .as_ref()
                .is_some_and(|f| f.turn == turn && f.verb == verb)
        {
            return;
        }
        self.record.in_flight = Some(InFlight {
            verb: verb.to_string(),
            turn,
            since: super::state::now_secs(),
        });
        self.path = write_record(&self.state, &self.record);
    }

    /// The turn-boundary counterpart to `stamp_in_flight`: called from
    /// `wrap.rs`'s turn-signal arm and `exec.rs`'s tick loop the instant a
    /// turn signal lands, i.e. the moment a turn is known to have reached a
    /// clean boundary. A no-op when nothing is stamped, so calling it on
    /// every turn signal regardless of whether `stamp_in_flight` ran first is
    /// always safe.
    pub fn clear_in_flight(&mut self) {
        if self.released || self.record.in_flight.is_none() {
            return;
        }
        self.record.in_flight = None;
        self.path = write_record(&self.state, &self.record);
    }

    /// This run's stable delivery address -- see `refresh_session`. Every
    /// mail listing a supervisor performs on its own behalf is scoped to
    /// this, never to `short_id(current session)`.
    ///
    /// Read back by `exec`'s nudge-relaunch mail listing specifically because
    /// it is the one value demonstrably unaffected by the
    /// `refresh_session` call immediately above it.
    pub fn short(&self) -> &str {
        &self.record.short
    }

    /// Gives up this guard's claim on its registry record WITHOUT removing
    /// anything (issue #552).
    ///
    /// One case only: a rollover successor has registered under the SAME
    /// short id -- the seat's stable address, which by design does not move
    /// across a rollover -- and the source is retired afterwards. Letting the
    /// source's guard run its ordinary `release` there would delete the
    /// record file the successor has just written, so the address would
    /// answer for nobody. Disowning states the truth instead: this guard no
    /// longer speaks for that address, and something else does.
    pub fn disown(&mut self) {
        self.released = true;
    }

    /// Idempotent, like `RawGuard::restore`.
    pub fn release(&mut self) {
        if self.released {
            return;
        }
        self.released = true;
        let _ = std::fs::remove_file(&self.path);
        // Issue #243 (review round, F1): the screening sibling file
        // (`screening_path`) is this record's own, so it goes with it --
        // best-effort, like the record removal right above; a crashed
        // supervisor's own leftover is still cleaned up by `list()`'s own
        // `sweep_orphaned_screening_summaries`.
        let _ = std::fs::remove_file(screening_path(&self.state, &self.record.short));
        // The bound-workflow sibling file (`workflow_path`) goes with the
        // record the same way -- best-effort, and swept up by `list()`'s own
        // `sweep_orphaned_workflow_markers` for a crashed supervisor's leftover.
        let _ = std::fs::remove_file(workflow_path(&self.state, &self.record.short));
        // Issue #295: a session-tier memory entry must never outlive the
        // session it belongs to -- best-effort, like every other cleanup
        // here; a failed removal leaves an orphaned directory, never data
        // loss for anything still live. Keyed on `record.session` (the
        // CURRENT session id this guard's record carries at release time,
        // which may have rotated since `remember_session` was actually
        // called for a `loop`/`exec` supervisor's earlier cycle -- see
        // `memory::MemoryScope::Session`'s own doc comment for this
        // accepted residual): an id that never held any session-tier
        // entries removes nothing.
        let _ = super::memory::forget_session_all(
            &self.state,
            &self.record.repo_slug,
            &self.record.session,
        );
    }
}

impl Drop for SessionGuard {
    fn drop(&mut self) {
        self.release();
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Liveness {
    Live,
    Crashed,
    Stale,
}

/// The three possible outcomes of a `kill(pid, 0)` signal-0 probe, named so
/// `record_is_alive` (issue #152) can react to the middle one -- `EPERM` --
/// differently from the other two, which `is_alive` folds together (`EPERM`
/// reads as alive there, same as `CanSignal` -- see its own doc comment).
#[cfg(unix)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SignalProbe {
    CanSignal,
    NoSuchProcess,
    PermissionDenied,
    Unknown,
}

#[cfg(unix)]
fn probe_signal(pid: u32) -> SignalProbe {
    // SAFETY: signal 0 sends nothing; it only probes existence and
    // permission, the same check `kill -0` makes from a shell.
    if unsafe { libc::kill(pid as libc::pid_t, 0) } == 0 {
        return SignalProbe::CanSignal;
    }
    match std::io::Error::last_os_error().raw_os_error() {
        Some(libc::ESRCH) => SignalProbe::NoSuchProcess,
        Some(libc::EPERM) => SignalProbe::PermissionDenied,
        _ => SignalProbe::Unknown,
    }
}

/// Signal-0 liveness probe (`kill -0`, the same check a shell's own `kill -0
/// <pid>` makes: existence and permission only, nothing is actually sent).
///
/// Issue #146: a plain `kill() == 0` check reads a non-zero return as "dead"
/// outright, which conflates two very different errno values. `ESRCH` (no
/// such process) genuinely does mean dead. `EPERM` means the process exists
/// but this caller lacks permission to signal it -- exactly what a sandboxed
/// `zirv ctx send`/`zirv ctx nudge`, running as a Bash-tool child inside a
/// dash pane, gets when probing the very sessions it is trying to reach.
/// Treating that as "dead" made `list` sweep every live record as `Stale`,
/// so `resolve_prefix` saw zero `Live` candidates and every send/nudge
/// failed with "no sessions are registered" -- issue #146's exact symptom,
/// with genuinely live sessions sitting right there in the registry.
///
/// `pub(crate)`: the single liveness check shared by this whole module
/// (`dashboard_owner_liveness`, `short_is_live` before issue #152, `list`
/// before issue #152) and, since issue #145/#146's fix, by `dash::mod` too
/// (`sweep_stale_token_dirs` and its own discovery scan) -- which used to
/// carry an independent, identically EPERM-blind copy of this exact check
/// rather than importing this one.
///
/// Documented trade-off, not a bug: reading `EPERM` as alive means a pid the
/// kernel has recycled to an unrelated, foreign-uid process keeps a stale
/// session/dashboard record alive until that pid frees again, since this
/// bare pid-only check has no start-time (or any other) disambiguator to
/// tell the original process apart from its replacement. Issue #152
/// addresses this for the one caller that actually has more than a bare pid
/// to work with: a `Record` also carries a `start_time`, and `record_is_alive`
/// below uses it to make exactly that distinction for `short_is_live` and
/// `list`, which now call it instead of this function. This function keeps
/// its original bare-pid, EPERM-is-alive contract unchanged -- callers with
/// no `Record` (`supervise`, `permit`, `dashboard_owner_liveness`,
/// `dash::mod`'s sweeps) still have no disambiguator available and keep
/// today's behavior exactly.
#[cfg(unix)]
pub(crate) fn is_alive(pid: u32) -> bool {
    probe_signal(pid) != SignalProbe::NoSuchProcess
}

#[cfg(windows)]
pub(crate) fn is_alive(pid: u32) -> bool {
    use windows_sys::Win32::Foundation::{
        CloseHandle, ERROR_INVALID_PARAMETER, GetLastError, STILL_ACTIVE,
    };
    use windows_sys::Win32::System::Threading::{
        GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
    };
    // SAFETY: `handle` is checked for null before any further call, and
    // `code` is only read after a successful `GetExitCodeProcess`.
    unsafe {
        let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if handle.is_null() {
            return GetLastError() != ERROR_INVALID_PARAMETER;
        }
        let mut code: u32 = 0;
        let alive = GetExitCodeProcess(handle, &mut code) == 0 || code == STILL_ACTIVE as u32;
        CloseHandle(handle);
        alive
    }
}

#[cfg(not(any(unix, windows)))]
pub(crate) fn is_alive(_pid: u32) -> bool {
    // No portable liveness check: never sweep a record this platform cannot
    // actually verify.
    true
}

/// How far apart a record's stamped `start_time` and a freshly read one may
/// be before they are considered two different processes rather than the
/// same one read twice.
///
/// Review round 2 finding 2 (issue #152): 10s was too tight. This
/// disambiguator's whole job is to catch a pid the OS recycled to an
/// unrelated process well AFTER the original session died -- in practice
/// hours or days later, since a pid only frees once the original process is
/// long gone and the kernel's pid counter wraps back around to it. It has no
/// business firing on ordinary clock noise: an NTP correction or a manual
/// clock change between registration and a later check can plausibly move
/// either `now_secs()` reading by more than a few seconds without any
/// process having changed at all, and `record_is_alive`'s `EPERM` branch is
/// exactly the sandboxed-probe path issue #146 exists for -- a genuinely
/// live, unrelated-uid session, not a recycled one. 300s (5 minutes) absorbs
/// realistic NTP steps, `ps -o etime=` rounding, and this reader's own
/// now-minus-age derivation slack, while still being a small fraction of the
/// "hours or days" gap the actual recycled-pid failure mode produces. Never
/// sweep a live record on a difference the clock environment alone could
/// plausibly explain.
///
/// Compare `RECYCLED_PID_TOLERANCE_SECS` (`kill`'s own recycled-pid guard,
/// above): both absorb clock/reading slack, but they answer different
/// questions at different magnitudes and must not be unified. `kill`'s guard
/// compares a target's own freshly-read age against `registered_at` -- a
/// ONE-SIDED "is this process younger than its own record" heuristic, where
/// even a few seconds of slack (5s) is enough margin because a genuine
/// session's process always predates its record by a wide, predictable
/// margin (registration happens moments after the process starts). This
/// disambiguator instead compares two INDEPENDENT start-time readings of the
/// same claimed process, taken at different times, against each other --
/// exactly the kind of comparison a clock step disturbs, and with no
/// registration-order assumption to lean on, hence the much wider 300s.
///
/// Only `record_is_alive`'s `#[cfg(unix)]` branch reads this outside of
/// tests -- see its own `#[cfg_attr]`, matching `parse_etime`'s identical
/// non-unix dead-code allowance below.
#[cfg_attr(not(unix), allow(dead_code))]
const START_TIME_TOLERANCE_SECS: u64 = 300;

/// Pure: whether a mismatch between a record's stamped `start_time` and a
/// freshly read one is large enough to mean "a different process now holds
/// this pid" -- issue #152's disambiguator for `is_alive`'s `EPERM` branch
/// (see its own doc comment on the trade-off this closes for `Record`-based
/// liveness).
///
/// Either side missing degrades to "cannot tell", which must read as NOT
/// disambiguating -- i.e. still alive -- per `record_is_alive`'s contract: a
/// record from a build or platform that cannot stamp/read a start time keeps
/// today's EPERM-is-alive behavior exactly, and must never read as falsely
/// dead just because one side of the comparison is missing.
///
/// `pub(crate)` since audit finding G4: `reservation::is_owner_alive` needs
/// the identical comparison for a ledger entry that carries a stamped
/// `pid_start_time` but no whole `Record`, and duplicating it there would
/// fork `START_TIME_TOLERANCE_SECS` into two constants that could drift.
pub(crate) fn start_time_disambiguates_dead(recorded: Option<u64>, current: Option<u64>) -> bool {
    match (recorded, current) {
        (Some(recorded), Some(current)) => recorded.abs_diff(current) > START_TIME_TOLERANCE_SECS,
        _ => false,
    }
}

/// Epoch seconds the process holding `pid` started, if this platform and
/// environment can tell -- built directly on [`process_age_secs`] (`now -
/// age`), so it inherits that reader's exact "cannot tell" cases (`ps`
/// missing/refused, unparsable output) with no cfg split of its own:
/// `process_age_secs` is already `None` on every non-unix target, which is
/// exactly issue #152's own scope note -- Windows liveness stays on its
/// existing `OpenProcess`/`GetExitCodeProcess` mechanism, unaffected, since
/// `record_is_alive` never calls this off unix.
///
/// `pub(crate)`: every place `Record::pid` is ever repointed at a different
/// process after `Record::new` -- `SessionGuard::adopt_child_pid` here, and
/// `dash::pane::Pane::spawn`'s own `record.pid = child_pid` -- must re-derive
/// `start_time` for the NEW pid in the same breath, or `record_is_alive`
/// compares the new process against the old one's start time and reads a
/// guaranteed, false mismatch (review round 2 finding 1, issue #152).
pub(crate) fn process_start_secs(pid: u32) -> Option<u64> {
    let age = process_age_secs(pid)?;
    Some(super::state::now_secs().saturating_sub(age))
}

/// [`is_alive`], sharpened for a [`Record`]: unlike a bare pid, a record also
/// carries the `start_time` its own process stamped at registration, which
/// is exactly the disambiguator `is_alive`'s own doc comment says a bare
/// signal-0 probe cannot have -- issue #152.
///
/// Unix: a `kill(pid, 0)` that can signal the process answers alive
/// unconditionally (this caller reached it, full stop -- no reason to doubt
/// a start time on top of that), and `ESRCH` answers dead unconditionally,
/// identical to `is_alive`. Only `EPERM` -- "exists, but not one I may
/// signal" -- gets a second opinion: whether the process now holding this pid
/// started around the same time this record's own process did, or
/// meaningfully later (the kernel recycled the pid to something unrelated
/// after the original exited). Missing or unreadable start times on either
/// side degrade to alive, exactly matching `EPERM`'s treatment before this
/// existed.
///
/// Non-unix: identical to `is_alive(record.pid)` -- issue #152's acceptance
/// leaves the Windows liveness mechanism unchanged.
///
/// `short_is_live` and `list`'s sweep are this function's only callers;
/// every other liveness check in this module and `dash::mod` has no
/// `Record` to read a start time from and keeps calling bare `is_alive`.
#[cfg(unix)]
pub fn record_is_alive(record: &Record) -> bool {
    match probe_signal(record.pid) {
        SignalProbe::CanSignal | SignalProbe::Unknown => true,
        SignalProbe::NoSuchProcess => false,
        SignalProbe::PermissionDenied => {
            !start_time_disambiguates_dead(record.start_time, process_start_secs(record.pid))
        }
    }
}

#[cfg(not(unix))]
pub fn record_is_alive(record: &Record) -> bool {
    is_alive(record.pid)
}

/// Whether the registry still holds a record for `short` whose pid is alive.
///
/// P4: a dashboard that was killed (rather than quit) leaves both a restore
/// roster *and* the sessions its panes registered -- and on Windows, before
/// the job-object backstop, those panes' agents genuinely outlived it.
/// Restoring such a candidate spawns a second agent onto a conversation the
/// first one is still holding. Reads the record file directly rather than
/// going through `list`, which sweeps as a side effect: this is a question,
/// not a cleanup. A missing, unreadable or malformed record answers `false`
/// -- nothing to collide with, so the restore may proceed.
pub fn short_is_live(state: &StateDir, short: &str) -> bool {
    load_record(state, short).is_some_and(|record| record_is_alive(&record))
}

/// One registry record, read straight off disk by its short id -- a question,
/// never a cleanup: unlike [`list`], nothing is swept and no liveness is
/// judged here, so a caller that only wants what was RECORDED about a session
/// (`Record::role`, issue #169) does not have to walk, and mutate, the whole
/// registry to find it. `None` for a missing, unreadable or malformed record.
pub fn load_record(state: &StateDir, short: &str) -> Option<Record> {
    std::fs::read_to_string(record_path(state, short))
        .ok()
        .and_then(|contents| serde_json::from_str::<Record>(&contents).ok())
}

/// Every record currently on disk, alongside whether its own process is
/// still alive. Crash witnesses survive until consumed or past the dashboard
/// restore horizon. A stale record (its process is gone) is swept -- its file
/// removed -- as a side effect of this read, but is still reported in the
/// returned list so a caller can say what it just cleaned up. A file that
/// fails to parse is skipped outright: one malformed record must never fail
/// the whole listing.
pub fn list(state: &StateDir) -> Vec<(Record, Liveness)> {
    let cfg = CtxConfig::load(Path::new("."), &env_from_process()).unwrap_or_default();
    list_with_retention(state, cfg.dash.roster_max_age_secs)
}

pub fn list_with_retention(state: &StateDir, retention_secs: u64) -> Vec<(Record, Liveness)> {
    let mut found = Vec::new();
    let now = state::now_secs();
    // Issue #99 (2026-08-23): an absent `sessions/` directory used to make
    // this whole function return immediately, before `sweep_orphan_endpoints`
    // below ever ran. That is exactly the state a fresh install, or a
    // machine where every registry record has already been cleaned up some
    // other way, is in -- precisely the case a stray `*.sock` file left by an
    // older zirv build (which predates the registry entirely) needs the
    // sweep to still run. `state.sessions()` missing now only means "no
    // records to list", not "skip every other sweep this function does".
    if let Ok(entries) = std::fs::read_dir(state.sessions()) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            let Ok(contents) = std::fs::read_to_string(&path) else {
                continue;
            };
            let Ok(record) = serde_json::from_str::<Record>(&contents) else {
                continue;
            };
            if record_is_alive(&record) {
                found.push((record, Liveness::Live));
            } else if record
                .in_flight
                .as_ref()
                .is_some_and(|in_flight| now.saturating_sub(in_flight.since) <= retention_secs)
            {
                found.push((record, Liveness::Crashed));
            } else {
                let _ = std::fs::remove_file(&path);
                found.push((record, Liveness::Stale));
            }
        }
        // `read_dir` yields records in a filesystem-dependent order, so a
        // caller that indexes the list positionally (the dashboard sidebar
        // re-reads it every ~1s) would see rows reorder under the operator
        // whenever an unrelated session registers or exits. Sort by a stable
        // key -- the launch time, then the short id as a tiebreak -- so the
        // ordering is deterministic across refreshes regardless of how the
        // directory happened to enumerate.
        found.sort_by(|a, b| {
            a.0.started_at
                .cmp(&b.0.started_at)
                .then_with(|| a.0.short.cmp(&b.0.short))
        });
    }
    sweep_orphaned_markers(state, &found);
    sweep_orphan_endpoints(state, &found);
    sweep_orphan_socket_paths(state, &found);
    sweep_orphaned_screening_summaries(state, &found);
    sweep_orphaned_workflow_markers(state, &found);
    found
}

/// The same "no live record, so remove it" sweep [`sweep_orphan_endpoints`]
/// runs for `*.sock` markers, for the `<state>/socket-path-<short>` files
/// `wrap::publish_socket_path` writes. Only `wrap`'s own graceful exit
/// unpublishes one, and this binary is `panic = "abort"`, so every kill or
/// crash leaves one behind forever (46 of them on one real machine) --
/// and `wrap::read_socket_path` with no session picks the NEWEST published
/// file by mtime, with no liveness check of its own, so a dead session's
/// leftover can be handed to a reader as if it were current.
///
/// Same probe-before-remove rule as the endpoint sweep, on the socket path
/// the file NAMES: a supervisor that is alive but was never (or no longer)
/// recorded in the registry still answers, and must keep its file.
fn sweep_orphan_socket_paths(state: &StateDir, found: &[(Record, Liveness)]) {
    let Ok(entries) = std::fs::read_dir(state.root()) else {
        return;
    };
    let live: std::collections::BTreeSet<&str> = found
        .iter()
        .filter(|(_, liveness)| *liveness == Liveness::Live)
        .map(|(record, _)| record.short.as_str())
        .collect();
    for entry in entries.flatten() {
        let path = entry.path();
        let Some(short) = path
            .file_name()
            .and_then(|n| n.to_str())
            .and_then(|n| n.strip_prefix(super::wrap::SOCKET_PATH_PREFIX))
        else {
            continue;
        };
        if live.contains(short) {
            continue;
        }
        let answers = std::fs::read_to_string(&path)
            .map(|s| s.trim().to_string())
            .is_ok_and(|socket| {
                !socket.is_empty() && super::signal::probe(std::path::Path::new(&socket))
            });
        if !answers {
            let _ = std::fs::remove_file(&path);
        }
    }
}

/// C9 (issue #99, 2026-08-23): an orphaned turn-signal endpoint -- a
/// `*.sock` file in `state.sockets()` with no matching live session record.
/// `SignalServer::bind` writes one for every supervised session (a real Unix
/// domain socket, or on Windows a marker file naming the pipe), and only
/// `Drop for SignalServer` removes it, which never runs for a killed or
/// crashed process (this binary's release profile is `panic = "abort"`).
/// Left behind, these accumulate and `zirv ctx status` lists every one of
/// them forever as `(no record)` (`status.rs`'s own `sessions_lines`).
///
/// Only a marker whose endpoint fails a connection probe
/// (`signal::probe`) is removed: one that still answers belongs to a
/// supervisor that is alive but was never (or no longer) recorded in the
/// registry -- an older build, or a registry write that failed -- and must
/// stay both on disk and listed. `found` is the same list `list` just
/// computed, so this never calls back into `list` itself, and every record
/// with a live entry is skipped outright without ever touching the network
/// (matching `sweep_orphaned_markers`'s own "only markers with no live
/// record" rule immediately above).
fn sweep_orphan_endpoints(state: &StateDir, found: &[(Record, Liveness)]) {
    let Ok(entries) = std::fs::read_dir(state.sockets()) else {
        return;
    };
    let live: std::collections::BTreeSet<&str> = found
        .iter()
        .filter(|(_, liveness)| *liveness == Liveness::Live)
        .map(|(record, _)| record.short.as_str())
        .collect();
    for entry in entries.flatten() {
        let path = entry.path();
        if !is_endpoint_file(&path) {
            continue;
        }
        let Some(short) = path.file_stem().and_then(|s| s.to_str()) else {
            continue;
        };
        if live.contains(short) {
            continue;
        }
        if !super::signal::probe(&path) {
            let _ = std::fs::remove_file(&path);
        }
    }
}

/// A turn-signal endpoint file: a seat's live `<short>.sock`, or a staged
/// rollover successor's `<short>.<4-hex nonce>` (`dash::pane`'s
/// `staged_socket_path`), which a crashed dashboard leaves behind just the same.
pub(crate) fn is_endpoint_file(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|ext| {
            ext == "sock" || (ext.len() == 4 && ext.bytes().all(|b| b.is_ascii_hexdigit()))
        })
}

#[derive(Debug, Clone, PartialEq)]
pub enum ResolveError {
    /// No live session's short id or full session id starts with the given
    /// prefix. Names every currently known live session, so the caller
    /// learns what it could have typed instead.
    NotFound { existing: Vec<String> },
    /// More than one candidate matches; every candidate's short id is named
    /// so the caller can disambiguate. Issue #721: shared with
    /// `resolve_prefix_or_parked`'s own multi-parked-seat case, which has no
    /// live [`Record`] to name -- only the short id survives a `Seat`, so
    /// this holds ids rather than full records (`resolve_prefix`'s own
    /// `Display` text never used anything else from them).
    Ambiguous(Vec<String>),
}

impl std::fmt::Display for ResolveError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ResolveError::NotFound { existing } => {
                if existing.is_empty() {
                    write!(f, "no sessions are registered")
                } else {
                    write!(
                        f,
                        "no session matches; known sessions: {}",
                        existing.join(", ")
                    )
                }
            }
            ResolveError::Ambiguous(candidates) => {
                write!(f, "ambiguous prefix; candidates: {}", candidates.join(", "))
            }
        }
    }
}

impl std::error::Error for ResolveError {}

/// Extends a [`ResolveError`]'s own display text with where the registry was
/// actually checked -- the state dir's `sessions()` subdirectory -- and
/// whether `ZIRV_CTX_STATE_DIR` pinned it or the platform default was used.
///
/// Issue #146: "no sessions are registered" alone gives no way to tell "the
/// registry really is empty" apart from "this call resolved a different
/// state dir than the one the session actually registered under" -- which is
/// exactly the shape of the EPERM-blind liveness bug this same issue fixed
/// (see `is_alive`'s own doc comment): a caller and the supervisor it means
/// to reach can end up looking at different state dirs, or the same dir
/// while one of them can no longer confirm the other alive, and either way
/// the operator sees the identical unhelpful message. Appended, not
/// substituted: the existing `{err}` text stays the prefix, so anything
/// already asserting on it keeps passing (`send`/`nudge`'s own call sites).
pub fn resolve_error_with_diagnostics(
    err: &ResolveError,
    state: &StateDir,
    env: EnvLookup<'_>,
) -> String {
    let state_env_note = match non_empty(env(super::state::STATE_ENV)) {
        Some(_) => format!("{} is set", super::state::STATE_ENV),
        None => format!(
            "{} is not set (using the platform default state dir)",
            super::state::STATE_ENV
        ),
    };
    format!(
        "{err} (registry checked at {}; {state_env_note})",
        state.sessions().display()
    )
}

/// Resolves a short-id (or full session-id) prefix to the one live record it
/// names. Only live records are candidates: a stale one has already been
/// swept from disk by the time a caller could act on it.
pub fn resolve_prefix(state: &StateDir, prefix: &str) -> Result<Record, ResolveError> {
    let live: Vec<Record> = list(state)
        .into_iter()
        .filter(|(_, liveness)| *liveness == Liveness::Live)
        .map(|(record, _)| record)
        .collect();

    let matches: Vec<Record> = live
        .iter()
        .filter(|r| r.short.starts_with(prefix) || r.session.starts_with(prefix))
        .cloned()
        .collect();

    match matches.len() {
        0 => Err(ResolveError::NotFound {
            existing: live.into_iter().map(|r| r.short).collect(),
        }),
        1 => Ok(matches.into_iter().next().expect("checked len == 1")),
        _ => Err(ResolveError::Ambiguous(
            matches.into_iter().map(|r| r.short).collect(),
        )),
    }
}

/// What addressing a short id (or full session id) prefix finds: the one
/// live record [`resolve_prefix`] already resolves, or -- issue #721 -- a
/// still-parked seat whose owning session record no longer exists.
/// `rollover::forget` deliberately keeps `<short>.seat.json` alive past its
/// own session's teardown while the park's window has not yet elapsed (its
/// own doc comment calls this a "ghost park"), so a bare registry miss
/// cannot tell a genuinely unknown id apart from one whose supervisor
/// already exited but is still owed a wake-up.
#[derive(Debug)]
pub enum Addressed {
    Live(Box<Record>),
    Parked(Box<super::seat::Seat>),
}

/// [`resolve_prefix`], extended to recognize a ghost-parked seat (issue
/// #721) instead of reporting it as a bare [`ResolveError::NotFound`].
/// Every other outcome is untouched and byte-identical to calling
/// `resolve_prefix` directly: a live match, an ambiguous live prefix, and a
/// prefix that matches neither a live record nor any parked seat all fall
/// straight through unchanged -- only a `NotFound` whose prefix names one or
/// more parked seats is reinterpreted: exactly one becomes `Parked`, and two
/// or more become the identical [`ResolveError::Ambiguous`] a multi-match
/// among live records already returns, naming each candidate's short id so
/// the caller can disambiguate.
pub fn resolve_prefix_or_parked(state: &StateDir, prefix: &str) -> Result<Addressed, ResolveError> {
    match resolve_prefix(state, prefix) {
        Ok(record) => Ok(Addressed::Live(Box::new(record))),
        Err(ResolveError::NotFound { existing }) => {
            let mut parked = super::seat::find_parked_by_prefix(state, prefix);
            match parked.len() {
                0 => Err(ResolveError::NotFound { existing }),
                1 => Ok(Addressed::Parked(Box::new(parked.remove(0)))),
                _ => Err(ResolveError::Ambiguous(
                    parked.into_iter().map(|seat| seat.short).collect(),
                )),
            }
        }
        Err(other) => Err(other),
    }
}

/// Pure: POSIX `ps -o etime=` output (`[[dd-]hh:]mm:ss`) as seconds. `None`
/// for anything that does not parse, which every caller reads as "cannot
/// tell" rather than as any particular age.
///
/// Only called from the `#[cfg(unix)]` `process_age_secs` and from tests; the
/// non-unix `process_age_secs` never reaches it.
#[cfg_attr(not(unix), allow(dead_code))]
fn parse_etime(raw: &str) -> Option<u64> {
    let (days, clock) = match raw.trim().split_once('-') {
        Some((days, clock)) => (days.trim().parse::<u64>().ok()?, clock),
        None => (0, raw.trim()),
    };
    let mut fields = clock.split(':').rev();
    let seconds = fields.next()?.trim().parse::<u64>().ok()?;
    let minutes = fields.next()?.trim().parse::<u64>().ok()?;
    let hours = match fields.next() {
        Some(hours) => hours.trim().parse::<u64>().ok()?,
        None => 0,
    };
    if fields.next().is_some() {
        return None;
    }
    Some(days * 86_400 + hours * 3_600 + minutes * 60 + seconds)
}

/// How long the process holding `pid` has been running, in seconds.
///
/// POSIX `ps -o etime=`, deliberately rather than a platform-specific
/// interface (`/proc/<pid>/stat` plus `btime`, `sysctl KERN_PROC_PID`): this
/// answers one question, on one code path, and a platform-abstraction layer
/// for it would be more machinery than the question is worth. `None` --
/// `ps` missing, refused (a sandbox), or output this cannot parse -- means
/// "cannot tell", and every caller then behaves exactly as it did before this
/// check existed rather than refusing to act.
#[cfg(unix)]
fn process_age_secs(pid: u32) -> Option<u64> {
    let output = std::process::Command::new("ps")
        .args(["-o", "etime=", "-p", &pid.to_string()])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    parse_etime(&String::from_utf8_lossy(&output.stdout))
}

/// No portable start-time probe on this platform, so a recycled pid is
/// indistinguishable from the session's own -- see [`run_kill_with`]'s doc
/// comment for the residual this leaves.
#[cfg(not(unix))]
fn process_age_secs(_pid: u32) -> Option<u64> {
    None
}

#[cfg(test)]
mod tests {
    use super::super::testenv::dead_pid;
    use super::*;

    pub(super) fn state_in(root: &Path) -> StateDir {
        StateDir::from_root(root.to_path_buf())
    }

    pub(super) fn record_for(session: &str, repo: &Path, verb: Verb) -> Record {
        Record::new(session, "claude", repo, verb)
    }

    /// Issue #470: a session record written before the `runtime` field
    /// existed has to still parse (and default to `Harness`, the only
    /// runtime any build could have registered a session under before now),
    /// and a record written by this build must round-trip its `runtime`
    /// value exactly.
    #[test]
    fn a_record_without_a_runtime_field_still_parses_as_harness_and_round_trips_with_it() {
        let record = record_for(
            "11111111-2222-4333-8444-555555555555",
            Path::new("/repo"),
            Verb::Exec,
        );
        let mut without_runtime = serde_json::to_value(&record).expect("serialize");
        without_runtime
            .as_object_mut()
            .expect("record is a JSON object")
            .remove("runtime");
        let parsed: Record = serde_json::from_value(without_runtime).expect("deserialize");
        assert_eq!(parsed.runtime, RuntimeKind::Harness);

        let json = serde_json::to_string(&record).expect("serialize");
        let round_tripped: Record = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(round_tripped.runtime, RuntimeKind::Harness);
    }

    #[test]
    fn a_record_without_owner_pid_deserializes_as_unowned() {
        // A record written by a build that predates `owner_pid` has no such
        // key in its JSON at all -- not `null`, simply absent -- which is
        // what `#[serde(default)]` (rather than a required field) exists to
        // survive.
        let json = r#"{
            "session": "11111111-2222-4333-8444-555555555555",
            "short": "11111111",
            "agent": "claude",
            "repo": "/repo",
            "repo_slug": "-repo",
            "verb": "exec",
            "pid": 1,
            "started_at": 0,
            "reachable": true
        }"#;
        let record: Record = serde_json::from_str(json).expect("deserialize");
        assert_eq!(record.owner_pid, None);
    }

    #[test]
    fn a_record_without_start_time_deserializes_as_unset() {
        // Same back-compat pattern as `owner_pid` above, for issue #152's
        // new field: a record written by a build that predates `start_time`
        // has no such key in its JSON at all, which `#[serde(default)]`
        // exists to survive.
        let json = format!(
            r#"{{
            "session": "22222222-2222-4333-8444-555555555555",
            "short": "22222222",
            "agent": "claude",
            "repo": "/repo",
            "repo_slug": "-repo",
            "verb": "exec",
            "pid": {},
            "started_at": 0,
            "reachable": true
        }}"#,
            std::process::id()
        );
        let record: Record = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(record.start_time, None);
        // The degrade rule end to end: a record with nothing to compare
        // against must never read as falsely dead. `pid` here is this very
        // test process's own -- always alive -- so this exercises the whole
        // `record_is_alive` path, not just the comparator in isolation.
        assert!(
            record_is_alive(&record),
            "no start_time to compare -- must degrade to alive, never false-dead"
        );
    }

    #[test]
    fn a_record_is_written_at_spawn_and_removed_when_the_supervisor_exits() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = state_in(tmp.path());
        let repo = tmp.path().join("repo");
        let record = record_for("11111111-2222-4333-8444-555555555555", &repo, Verb::Exec);
        let path = record_path(&state, &record.short);

        let guard = SessionGuard::register(&state, record);
        assert!(path.is_file(), "the record file exists right after spawn");

        drop(guard);
        assert!(
            !path.exists(),
            "the record file is gone once the supervisor exits"
        );
    }

    /// Issue #295: a session-scoped memory entry must never outlive the
    /// session it belongs to -- `SessionGuard::release` (also reached via
    /// `Drop`) removes the whole `sessions/<session-id>/` tier for this
    /// record's `repo_slug`/`session` the moment the registry entry retires.
    #[test]
    fn retiring_a_session_removes_its_own_memory_tier_but_leaves_other_sessions_alone() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = state_in(tmp.path());
        let repo = tmp.path().join("repo");
        let session_id = "11111111-2222-4333-8444-555555555555";
        let record = record_for(session_id, &repo, Verb::Wrap);
        let slug = record.repo_slug.clone();
        let cfg = super::super::config::CtxConfig::default();

        let mut entry = super::super::memory::Entry {
            key: "retiring-key".to_string(),
            written_by: "claude".to_string(),
            written: 1,
            verified: 1,
            source: "explicit".to_string(),
            body: "gone once the session retires".to_string(),
            importance: None,
            confidence: None,
            tags: Vec::new(),
            paths: Vec::new(),
        };
        super::super::memory::remember_session(&state, &slug, session_id, &entry, &cfg)
            .expect("remember into the session tier");
        entry.key = "other-session-key".to_string();
        super::super::memory::remember_session(&state, &slug, "another-session", &entry, &cfg)
            .expect("remember into a different session's tier");

        let guard = SessionGuard::register(&state, record);
        assert_eq!(
            super::super::memory::list_session(&state, &slug, session_id)
                .expect("list before release")
                .len(),
            1
        );

        drop(guard);

        assert!(
            super::super::memory::list_session(&state, &slug, session_id)
                .expect("list after release")
                .is_empty(),
            "the retired session's own memory tier must be gone"
        );
        assert_eq!(
            super::super::memory::list_session(&state, &slug, "another-session")
                .expect("list the other session")
                .len(),
            1,
            "retiring one session must never touch another session's tier"
        );
    }

    /// Finding 3: `owner_pid` used to be stamped only by `dash/pane.rs`, so
    /// every *other* registration path -- a standalone `wrap`/`exec`/`loop`
    /// session in particular -- was written with `owner_pid: None`, an owner
    /// no dashboard could ever match, even though the registering process
    /// itself is a perfectly good owner to record. Moving the stamp into
    /// `register` itself fixes that uniformly: every registration is now
    /// attributed to whichever process actually called it. (This does not
    /// reach `zirv ctx agent`'s dashboard-refused-but-retryable fallback,
    /// which runs in the *requester's* process rather than the dashboard's
    /// even when it was dispatched on that dashboard's behalf -- a separate,
    /// accepted residual; see `owner_pid`'s own doc comment.)
    #[test]
    fn register_stamps_owner_pid_with_the_current_process_unless_already_set() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = state_in(tmp.path());
        let repo = tmp.path().join("repo");

        let record = record_for("11111111-2222-4333-8444-555555555555", &repo, Verb::Exec);
        assert_eq!(record.owner_pid, None, "unstamped before registration");
        let guard = SessionGuard::register(&state, record);
        assert_eq!(
            guard.record().owner_pid,
            Some(std::process::id()),
            "register stamps the current process's own pid"
        );

        let mut explicit = record_for("22222222-3333-4444-8555-666666666666", &repo, Verb::Exec);
        explicit.owner_pid = Some(999);
        let guard = SessionGuard::register(&state, explicit);
        assert_eq!(
            guard.record().owner_pid,
            Some(999),
            "an owner the caller already set is left alone"
        );
    }

    /// P5: `wrap` files its record before it has a child, so `Record::new`
    /// stamps zirv's own pid; `adopt_child_pid` re-points it at the agent the
    /// supervisor actually spawned, exactly as `dash::pane::Pane::spawn`
    /// already does at its own registration. The record's *address* (`short`,
    /// and therefore its path) must not move with it -- that is what mail and
    /// `zirv ctx nudge` resolve against.
    #[test]
    fn adopt_child_pid_repoints_the_record_at_the_agent_without_moving_its_address() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = state_in(tmp.path());
        let repo = tmp.path().join("repo");
        let record = record_for("11111111-2222-4333-8444-555555555555", &repo, Verb::Wrap);
        let short = record.short.clone();
        let path = record_path(&state, &short);

        let mut guard = SessionGuard::register(&state, record);
        assert_eq!(
            guard.record().pid,
            std::process::id(),
            "before the spawn there is no child pid to record"
        );

        guard.adopt_child_pid(4242);
        assert_eq!(guard.record().pid, 4242);
        assert_eq!(guard.short(), short, "the delivery address does not move");
        assert!(path.is_file(), "and neither does the record's own path");

        let on_disk: Record =
            serde_json::from_str(&std::fs::read_to_string(&path).expect("read")).expect("parse");
        assert_eq!(on_disk.pid, 4242, "the override reached disk");
        assert_eq!(
            on_disk.owner_pid,
            Some(std::process::id()),
            "owner_pid still answers 'which process filed this', untouched"
        );
    }

    /// Review round 2 finding 1 (issue #152): `adopt_child_pid` must re-stamp
    /// `start_time` for the NEW pid in the same breath it repoints `pid`
    /// itself, or the very next `EPERM` liveness probe against a perfectly
    /// live pane compares the CHILD's real start time against whatever the
    /// record's `start_time` was left at (the supervisor's own, from
    /// `Record::new`) and reads a guaranteed, false mismatch -- deleting a
    /// live session's record. Pid 1 stands in for "a real process this
    /// caller cannot signal", the same real-`EPERM` source the other pid-1
    /// tests in this module use, since forcing an `EPERM` against a pid this
    /// test owns outright is not possible.
    #[cfg(unix)]
    #[test]
    fn adopt_child_pid_re_stamps_start_time_so_the_new_pid_reads_live() {
        // SAFETY: `geteuid` takes no arguments and only reads process state.
        if unsafe { libc::geteuid() } == 0 {
            eprintln!("skipping: running as root, kill(1, 0) succeeds outright");
            return;
        }
        // SAFETY: signal 0 sends nothing; it only probes existence and
        // permission -- see finding 5's own note on why `geteuid` alone is
        // not a sufficient guard (a rootless/namespaced sandbox can still
        // let this uid signal pid 1 outright).
        if unsafe { libc::kill(1, 0) } == 0 {
            eprintln!("skipping: kill(1, 0) succeeds outright in this sandbox");
            return;
        }
        if process_start_secs(1).is_none() {
            eprintln!("skipping: no usable `ps` in this environment, so no start time to check");
            return;
        }

        let tmp = tempfile::tempdir().expect("tempdir");
        let state = state_in(tmp.path());
        let repo = tmp.path().join("repo");
        let mut record = record_for("77777777-2222-4333-8444-555555555555", &repo, Verb::Wrap);
        // Issue #218 fix round, defect 3: the 2s tolerance below (needed to
        // absorb `process_start_secs`'s own `now - elapsed` straddle) also
        // means a record that started life within 2s of pid 1's own start
        // time -- entirely possible in a fresh container, where pid 1 and
        // this test process can both be seconds old -- would pass the
        // tolerance check even if `adopt_child_pid` never re-stamped
        // `start_time` at all. Pin the record to an ancient sentinel first,
        // so the assertion below can only pass if `adopt_child_pid` actually
        // moved `start_time`, not merely if it happened to already be close.
        record.start_time = Some(1);
        let mut guard = SessionGuard::register(&state, record);

        guard.adopt_child_pid(1);

        // `process_start_secs` derives its answer as `now - elapsed`, so two
        // calls a few instructions apart can straddle a second boundary and
        // land one second off each other -- issue #218's flake. Compare
        // within a small tolerance instead of exact equality. That tolerance
        // still catches the real regression: pid 1's start time is the
        // machine's boot time, while a pinned, unmoved `start_time` would
        // still read this test process's own (recent) start -- hours or more
        // away from pid 1's, never within 2 seconds of it.
        let actual = guard.record().start_time;
        let expected = process_start_secs(1);
        assert_ne!(
            actual,
            Some(1),
            "adopt_child_pid must re-stamp start_time, not leave the ancient sentinel in place \
             (expected live={expected:?})"
        );
        match (actual, expected) {
            (Some(actual), Some(expected)) => {
                let diff = actual.abs_diff(expected);
                assert!(
                    diff <= 2,
                    "start_time must move with pid, not stay pinned to the supervisor's own \
                     (adopted={actual}, live={expected}, diff={diff}s)"
                );
            }
            _ => panic!(
                "start_time must move with pid, not stay pinned to the supervisor's own \
                 (adopted={actual:?}, live={expected:?})"
            ),
        }
        assert!(
            record_is_alive(guard.record()),
            "the repointed record must read live, not falsely dead from a stale start_time"
        );
    }

    /// P5, the restart window: the pid a `wrap` record names must never be a
    /// *dead* one, not even briefly.
    ///
    /// `list` sweeps any record whose pid is gone, and it runs on other
    /// processes' schedules -- `zirv ctx status`, `nudge`, `send
    /// --to-session`, a dashboard's ~1s registry refresh. So during a rot
    /// restart, where the old child is killed long before a replacement
    /// exists, `pump`'s restart arm parks the record on zirv's own pid first
    /// and adopts the fresh child's only once there is one. This pins that
    /// three-step sequence, which is all `pump` does to the guard: there is
    /// no seam to drive the pump's own restart arm from a unit test, and the
    /// two calls it makes are exactly these.
    #[test]
    fn a_restart_never_leaves_the_record_naming_a_dead_pid() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = state_in(tmp.path());
        let repo = tmp.path().join("repo");
        let record = record_for("44444444-2222-4333-8444-555555555555", &repo, Verb::Wrap);
        let path = record_path(&state, &record.short);
        let mut guard = SessionGuard::register(&state, record);

        let on_disk = || -> Record {
            serde_json::from_str(&std::fs::read_to_string(&path).expect("read")).expect("parse")
        };

        // 1. First spawn: the record names the agent child.
        guard.adopt_child_pid(4242);
        assert_eq!(on_disk().pid, 4242);

        // 2. Restart begins -- the child is about to be killed, so the record
        //    is parked on this process, which is alive by construction.
        guard.adopt_child_pid(std::process::id());
        assert_eq!(on_disk().pid, std::process::id());
        assert!(
            is_alive(on_disk().pid),
            "a concurrent `list` mid-restart must not sweep this record"
        );

        // 3. Respawn: back onto the fresh child.
        guard.adopt_child_pid(5353);
        assert_eq!(on_disk().pid, 5353);
        assert_eq!(
            on_disk().short,
            guard.short(),
            "and the delivery address never moved through any of it"
        );
    }

    /// Idempotent and inert after release, like every other guard write here:
    /// a relaunch calls it once per fresh child, and a released guard must not
    /// resurrect the record file it just removed.
    #[test]
    fn adopt_child_pid_is_a_no_op_on_a_released_guard() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = state_in(tmp.path());
        let repo = tmp.path().join("repo");
        let record = record_for("33333333-2222-4333-8444-555555555555", &repo, Verb::Wrap);
        let path = record_path(&state, &record.short);

        let mut guard = SessionGuard::register(&state, record);
        guard.release();
        guard.adopt_child_pid(4242);
        assert!(!path.exists(), "a released record stays released");
    }

    /// P4's production probe. A record naming *this* test process is live by
    /// construction; an absurd pid is not; and no record at all answers
    /// "nothing to collide with", so the restore may proceed.
    #[test]
    fn short_is_live_answers_from_the_record_the_short_names() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = state_in(tmp.path());
        let repo = tmp.path().join("repo");

        let mut alive = record_for("aaaaaaaa-2222-4333-8444-555555555555", &repo, Verb::Dash);
        alive.pid = std::process::id();
        let alive_short = alive.short.clone();
        let _alive_path = write_record(&state, &alive);

        let mut dead = record_for("bbbbbbbb-2222-4333-8444-555555555555", &repo, Verb::Dash);
        // Far above any pid Windows or Linux hands out, so it cannot collide
        // with a real process on the machine running these tests.
        dead.pid = 4_000_000_003;
        let dead_short = dead.short.clone();
        let _dead_path = write_record(&state, &dead);

        assert!(short_is_live(&state, &alive_short));
        assert!(!short_is_live(&state, &dead_short));
        assert!(
            !short_is_live(&state, "nosuchid"),
            "no record means nothing to collide with"
        );
    }

    #[test]
    fn the_record_key_is_the_same_short_id_the_socket_is_named_after() {
        let session = "abcdef12-3456-4789-8abc-def012345678";
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = state_in(tmp.path());

        let socket = state.socket_for(session);
        let socket_stem = socket
            .file_stem()
            .and_then(|s| s.to_str())
            .expect("socket file stem");

        assert_eq!(
            short_id(session),
            socket_stem,
            "the registry's own short id must match the socket's stem exactly"
        );
    }

    #[test]
    fn an_explicit_release_removes_the_record_even_though_drop_is_not_guaranteed() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = state_in(tmp.path());
        let repo = tmp.path().join("repo");
        let record = record_for("22222222-2222-4333-8444-555555555555", &repo, Verb::Wrap);
        let path = record_path(&state, &record.short);

        let mut guard = SessionGuard::register(&state, record);
        assert!(path.is_file());

        guard.release();
        assert!(!path.exists(), "an explicit release removes the file");

        // Idempotent: dropping after an explicit release must not error or
        // try to remove anything a second time.
        drop(guard);
    }

    #[test]
    fn a_record_whose_process_is_gone_is_reported_stale_and_swept_on_read() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = state_in(tmp.path());
        let repo = tmp.path().join("repo");
        let mut record = record_for("33333333-2222-4333-8444-555555555555", &repo, Verb::Loop);
        record.pid = dead_pid();
        let path = record_path(&state, &record.short);
        write_record(&state, &record);
        assert!(path.is_file(), "sanity: the record was written");

        let found = list(&state);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].1, Liveness::Stale);
        assert_eq!(found[0].0.session, record.session);

        assert!(
            !path.exists(),
            "a stale record is swept from disk as a side effect of listing"
        );
    }

    #[test]
    fn listing_keeps_crash_witnesses_until_consumed_or_past_retention() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = state_in(tmp.path());
        let mut record = record_for(
            "33333333-2222-4333-8444-555555555555",
            tmp.path(),
            Verb::Exec,
        );
        record.pid = dead_pid();
        record.in_flight = Some(InFlight {
            verb: "exec".into(),
            turn: 2,
            since: state::now_secs(),
        });
        let path = record_path(&state, &record.short);
        write_record(&state, &record);
        assert_eq!(list(&state)[0].1, Liveness::Crashed);
        assert_eq!(list(&state)[0].0.in_flight, record.in_flight);
        assert!(path.exists());
        launch_consuming_interrupted(&state, tmp.path(), || Ok(())).expect("resume");
        assert_eq!(list(&state)[0].1, Liveness::Stale);
        assert!(!path.exists());

        record.in_flight.as_mut().expect("witness").since =
            state::now_secs() - super::super::config::DashConfig::default().roster_max_age_secs - 1;
        write_record(&state, &record);
        assert_eq!(list(&state)[0].1, Liveness::Stale);
        assert!(!path.exists());

        record.in_flight = None;
        write_record(&state, &record);
        assert_eq!(list(&state)[0].1, Liveness::Stale);
        assert!(!path.exists());
    }

    #[test]
    fn listing_prunes_crash_witnesses_after_the_configured_retention() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let home = tmp.path().join("home");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);
        std::fs::create_dir_all(home.join(".zirv")).expect("config dir");
        std::fs::write(
            home.join(".zirv/ctx.toml"),
            "[dash]\nroster_max_age_secs = 60\n",
        )
        .expect("operator config");
        let state = state_in(tmp.path());
        let mut record = record_for(
            "33333333-2222-4333-8444-555555555555",
            tmp.path(),
            Verb::Exec,
        );
        record.pid = dead_pid();
        record.in_flight = Some(InFlight {
            verb: "exec".into(),
            turn: 2,
            since: state::now_secs() - 61,
        });
        let path = record_path(&state, &record.short);
        write_record(&state, &record);
        assert_eq!(list(&state)[0].1, Liveness::Stale);
        assert!(!path.exists());
    }

    #[test]
    fn a_live_record_is_reported_live_and_kept_on_disk() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = state_in(tmp.path());
        let repo = tmp.path().join("repo");
        // This test process's own pid is alive for as long as the test runs.
        let record = record_for("44444444-2222-4333-8444-555555555555", &repo, Verb::Exec);
        let path = record_path(&state, &record.short);
        write_record(&state, &record);

        let found = list(&state);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].1, Liveness::Live);
        assert!(path.is_file(), "a live record's file is untouched");
    }

    #[test]
    fn a_malformed_record_is_skipped_rather_than_failing_the_listing() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = state_in(tmp.path());
        let repo = tmp.path().join("repo");

        super::super::state::create_private_dir_all(&state.sessions()).expect("mkdir");
        std::fs::write(state.sessions().join("broken.json"), "{ not json").expect("write junk");

        let good = record_for("55555555-2222-4333-8444-555555555555", &repo, Verb::Exec);
        write_record(&state, &good);

        let found = list(&state);
        assert_eq!(
            found.len(),
            1,
            "the malformed file is skipped, not fatal: {found:?}"
        );
        assert_eq!(found[0].0.session, good.session);
    }

    /// MED: `list` sorts by a stable key (`started_at`, then `short`) rather
    /// than returning records in filesystem enumeration order, so a caller
    /// that indexes positionally (the dashboard sidebar) sees a deterministic
    /// ordering across refreshes. The `started_at` values here are chosen so
    /// the correct order is neither the shorts' alphabetical order nor any
    /// plausible directory order, pinning `started_at` as the primary key.
    #[test]
    fn list_returns_records_in_a_stable_sorted_order() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = state_in(tmp.path());
        let repo = tmp.path().join("repo");

        // (session, started_at): sorted by started_at gives ccc, bbb, aaa --
        // the reverse of the shorts' own alphabetical order.
        let seeds = [
            ("cccccccc-2222-4333-8444-555555555555", 100u64),
            ("aaaaaaaa-2222-4333-8444-555555555555", 300u64),
            ("bbbbbbbb-2222-4333-8444-555555555555", 200u64),
        ];
        for (session, started_at) in seeds {
            let mut record = record_for(session, &repo, Verb::Exec);
            record.started_at = started_at;
            write_record(&state, &record);
        }

        let order: Vec<String> = list(&state).into_iter().map(|(r, _)| r.short).collect();
        assert_eq!(
            order,
            vec![
                "cccccccc".to_string(),
                "bbbbbbbb".to_string(),
                "aaaaaaaa".to_string(),
            ],
            "records come back ordered by started_at, deterministically"
        );
    }

    #[test]
    fn listing_an_absent_sessions_directory_is_not_an_error() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = state_in(tmp.path());
        assert!(list(&state).is_empty());
    }

    /// The Windows named-pipe namespace is machine-global (unlike a unix
    /// domain socket, scoped to its own tempdir), so a hardcoded or
    /// small-space-derived short id in a test that touches
    /// `signal::probe`/`SignalServer::bind` risks colliding with an
    /// unrelated live pipe -- including one this very test binary leaked
    /// earlier in the same run (`Drop for SignalServer` on Windows only
    /// removes the marker file; the acceptor thread and its pipe instance
    /// keep answering for the rest of the process's life). A fresh random
    /// UUID per call, the same generator every real session id already uses
    /// (`event.rs`), keeps this from ever landing on a name anything else in
    /// the process could already own.
    fn unique_endpoint_session() -> String {
        uuid::Uuid::new_v4().to_string()
    }

    /// Issue #99 (2026-08-23): a `*.sock` marker with no matching session
    /// record and nothing listening behind it is a leftover from a killed or
    /// crashed supervisor (`Drop for SignalServer` never ran). `list`'s own
    /// sweep must remove it rather than let it accumulate forever.
    #[test]
    fn a_dead_endpoint_marker_with_no_record_is_swept() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = state_in(tmp.path());
        super::super::state::create_private_dir_all(&state.sockets()).expect("mkdir");
        let path = state.socket_for(&unique_endpoint_session());
        std::fs::write(&path, "leftover, nothing is listening").expect("write leftover marker");
        assert!(path.exists(), "sanity: the leftover marker exists");

        assert!(list(&state).is_empty(), "no registry record exists");
        assert!(
            !path.exists(),
            "a dead endpoint with no record must be swept: {}",
            path.display()
        );
    }

    /// A marker whose endpoint still answers belongs to a supervisor that is
    /// alive but simply has no registry record (an older build, or a
    /// registry write that failed) -- it must stay on disk and stay listed,
    /// not be swept just because nothing filed a `Record` for it.
    #[cfg(unix)]
    #[test]
    fn a_live_endpoint_marker_with_no_record_is_kept() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = state_in(tmp.path());
        let path = state.socket_for(&unique_endpoint_session());
        let _server = crate::commands::ctx::signal::SignalServer::bind(&path).expect("bind");
        assert!(path.exists(), "sanity: the live endpoint exists");

        assert!(list(&state).is_empty(), "no registry record exists");
        assert!(
            path.exists(),
            "a live endpoint with no record must be kept: {}",
            path.display()
        );
    }

    /// A live *registered* session's own endpoint marker must never be swept
    /// as a side effect of sweeping everyone else's orphans.
    #[cfg(unix)]
    #[test]
    fn a_live_registered_sessions_endpoint_marker_is_untouched() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = state_in(tmp.path());
        let repo = tmp.path().join("repo");
        let session = unique_endpoint_session();
        let record = record_for(&session, &repo, Verb::Wrap);
        let short = record.short.clone();
        let _guard = SessionGuard::register(&state, record);

        let path = state.socket_for(&session);
        let _server = crate::commands::ctx::signal::SignalServer::bind(&path).expect("bind");
        assert!(path.exists(), "sanity: the endpoint exists");

        let found = list(&state);
        assert!(
            found
                .iter()
                .any(|(r, liveness)| r.short == short && *liveness == Liveness::Live),
            "the registered session is still listed as live: {found:?}"
        );
        assert!(
            path.exists(),
            "a live registered session's own marker must be untouched: {}",
            path.display()
        );
    }

    #[test]
    fn resolving_a_unique_prefix_returns_the_one_record() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = state_in(tmp.path());
        let repo = tmp.path().join("repo");
        let record = record_for("bbbbbbbb-2222-4333-8444-555555555555", &repo, Verb::Wrap);
        write_record(&state, &record);

        let resolved = resolve_prefix(&state, "bbbb").expect("unique prefix resolves");
        assert_eq!(resolved.session, record.session);

        let resolved_full = resolve_prefix(&state, "bbbbbbbb").expect("the full short id too");
        assert_eq!(resolved_full.session, record.session);
    }

    #[test]
    fn an_ambiguous_prefix_is_an_error_that_names_every_candidate() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = state_in(tmp.path());
        let repo = tmp.path().join("repo");
        // Filtered-and-truncated to 8 chars, these two share the prefix
        // "aaaa" but are not otherwise identical: "aaaa1111" vs "aaaa2222".
        let one = record_for("aaaa1111-xxxx-4xxx-8xxx-xxxxxxxxxxxx", &repo, Verb::Exec);
        let two = record_for("aaaa2222-yyyy-4yyy-8yyy-yyyyyyyyyyyy", &repo, Verb::Loop);
        write_record(&state, &one);
        write_record(&state, &two);

        let err = resolve_prefix(&state, "aaaa").expect_err("two records share this prefix");
        match &err {
            ResolveError::Ambiguous(candidates) => {
                assert_eq!(candidates.len(), 2);
                assert!(candidates.contains(&one.short), "{candidates:?}");
                assert!(candidates.contains(&two.short), "{candidates:?}");
            }
            other => panic!("expected Ambiguous, got {other:?}"),
        }
        let msg = err.to_string();
        assert!(msg.contains(&one.short), "names the first candidate: {msg}");
        assert!(
            msg.contains(&two.short),
            "names the second candidate: {msg}"
        );
    }

    #[test]
    fn an_unknown_prefix_is_an_error_that_says_which_sessions_exist() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = state_in(tmp.path());
        let repo = tmp.path().join("repo");
        let record = record_for("cccccccc-2222-4333-8444-555555555555", &repo, Verb::Chat);
        write_record(&state, &record);

        let err = resolve_prefix(&state, "zzzz").expect_err("nothing starts with zzzz");
        match &err {
            ResolveError::NotFound { existing } => {
                assert_eq!(existing, &vec![record.short.clone()]);
            }
            other => panic!("expected NotFound, got {other:?}"),
        }
        assert!(err.to_string().contains(&record.short));
    }

    /// Issue #721: a "ghost park" -- a parked seat whose owning session
    /// record `rollover::forget` already removed -- is recognized by
    /// `resolve_prefix_or_parked` instead of falling through as a bare
    /// `NotFound`.
    #[test]
    fn resolve_prefix_or_parked_recognizes_a_ghost_parked_seat() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = state_in(tmp.path());
        let now = super::super::state::now_secs();
        crate::commands::ctx::seat::register(
            &state,
            "ghost123",
            "ghost-session",
            "claude",
            None,
            "anthropic",
            "orchestrator",
            false,
            now,
        )
        .expect("register");
        crate::commands::ctx::seat::park(&state, "ghost123", now + 3600, "5h", "rate limited", now)
            .expect("park");

        let addressed = resolve_prefix_or_parked(&state, "ghost").expect("recognized as parked");
        match addressed {
            Addressed::Parked(seat) => assert_eq!(seat.short, "ghost123"),
            Addressed::Live(record) => panic!("expected Parked, got Live({record:?})"),
        }
    }

    /// A parked seat whose session is still live must resolve exactly as
    /// `resolve_prefix` alone already does -- `resolve_prefix_or_parked`
    /// only reinterprets a `NotFound`, never a live match.
    #[test]
    fn resolve_prefix_or_parked_is_unchanged_for_a_live_parked_seat() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = state_in(tmp.path());
        let repo = tmp.path().join("repo");
        let now = super::super::state::now_secs();
        let record = record_for("eeeeeeee-2222-4333-8444-555555555555", &repo, Verb::Wrap);
        let short = record.short.clone();
        let _guard = SessionGuard::register(&state, record);
        crate::commands::ctx::seat::register(
            &state,
            &short,
            "eeeeeeee-2222-4333-8444-555555555555",
            "claude",
            None,
            "anthropic",
            "orchestrator",
            false,
            now,
        )
        .expect("register");
        crate::commands::ctx::seat::park(&state, &short, now + 3600, "5h", "rate limited", now)
            .expect("park");

        let addressed =
            resolve_prefix_or_parked(&state, &short[..4]).expect("live record still resolves");
        match addressed {
            Addressed::Live(resolved) => assert_eq!(resolved.short, short),
            Addressed::Parked(seat) => panic!("expected Live, got Parked({seat:?})"),
        }
    }

    /// A prefix matching neither a live record nor a parked seat still gets
    /// today's exact `NotFound`, unchanged.
    #[test]
    fn resolve_prefix_or_parked_is_unchanged_for_a_truly_unknown_prefix() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = state_in(tmp.path());
        let repo = tmp.path().join("repo");
        let record = record_for("ffffffff-2222-4333-8444-555555555555", &repo, Verb::Chat);
        write_record(&state, &record);

        let err = resolve_prefix_or_parked(&state, "zzzz").expect_err("nothing starts with zzzz");
        match &err {
            ResolveError::NotFound { existing } => {
                assert_eq!(existing, &vec![record.short.clone()]);
            }
            other => panic!("expected NotFound, got {other:?}"),
        }
    }

    /// Issue #721 review finding #2 (MEDIUM): a prefix naming two or more
    /// ghost-parked seats must not fold into a bare `NotFound` -- it is
    /// exactly as ambiguous as two live records sharing a prefix, so it
    /// gets the identical `ResolveError::Ambiguous`, naming each candidate.
    #[test]
    fn resolve_prefix_or_parked_is_ambiguous_for_two_ghost_parked_seats_sharing_a_prefix() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = state_in(tmp.path());
        let now = super::super::state::now_secs();
        crate::commands::ctx::seat::register(
            &state,
            "dupe1111",
            "dupe-session-1",
            "claude",
            None,
            "anthropic",
            "orchestrator",
            false,
            now,
        )
        .expect("register");
        crate::commands::ctx::seat::park(&state, "dupe1111", now + 3600, "5h", "rate limited", now)
            .expect("park");
        crate::commands::ctx::seat::register(
            &state,
            "dupe2222",
            "dupe-session-2",
            "claude",
            None,
            "anthropic",
            "orchestrator",
            false,
            now,
        )
        .expect("register");
        crate::commands::ctx::seat::park(&state, "dupe2222", now + 3600, "5h", "rate limited", now)
            .expect("park");

        let err = resolve_prefix_or_parked(&state, "dupe")
            .expect_err("two parked seats share this prefix");
        match &err {
            ResolveError::Ambiguous(candidates) => {
                assert_eq!(candidates.len(), 2);
                assert!(
                    candidates.contains(&"dupe1111".to_string()),
                    "{candidates:?}"
                );
                assert!(
                    candidates.contains(&"dupe2222".to_string()),
                    "{candidates:?}"
                );
            }
            other => panic!("expected Ambiguous, got {other:?}"),
        }
        let msg = err.to_string();
        assert!(
            msg.contains("dupe1111") && msg.contains("dupe2222"),
            "{msg}"
        );
    }

    #[test]
    fn a_stale_record_is_never_offered_as_a_resolution_candidate() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = state_in(tmp.path());
        let repo = tmp.path().join("repo");
        let mut stale = record_for("dddddddd-2222-4333-8444-555555555555", &repo, Verb::Exec);
        stale.pid = dead_pid();
        write_record(&state, &stale);

        let err = resolve_prefix(&state, "dddd")
            .expect_err("the only match is stale, so effectively gone");
        assert!(matches!(err, ResolveError::NotFound { existing } if existing.is_empty()));
    }

    /// C7: a refresh rotates the session *id* but keeps the record's short
    /// id -- the supervisor's stable delivery address -- and therefore its
    /// file. Rotating the address was what stranded mail addressed to a live
    /// session the moment the next cycle or restart replaced it.
    #[test]
    fn a_loop_keeps_one_record_and_refreshes_the_session_id_each_cycle() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = state_in(tmp.path());
        let repo = tmp.path().join("repo");
        let first = record_for("eeeeeeee-2222-4333-8444-555555555555", &repo, Verb::Loop);
        let stable_short = first.short.clone();
        let record_file = record_path(&state, &stable_short);

        let mut guard = SessionGuard::register(&state, first);
        assert!(record_file.is_file());

        let second_session = "ffffffff-2222-4333-8444-555555555555";
        guard.refresh_session(second_session);

        assert!(
            record_file.is_file(),
            "the record stays under its original short id -- that is its address"
        );
        assert_eq!(
            guard.short(),
            stable_short,
            "the delivery address survives a refresh"
        );
        assert!(
            !record_path(&state, &short_id(second_session)).exists(),
            "no second file appears under the new session's own short id"
        );
        assert_eq!(
            guard.record().session,
            second_session,
            "the session id itself does rotate"
        );
        assert_eq!(
            guard.record().verb,
            Verb::Loop,
            "the verb survives a refresh"
        );

        // Only one record for the whole run at any given time.
        let found = list(&state);
        assert_eq!(found.len(), 1, "one record, not one per cycle: {found:?}");

        guard.release();
        assert!(!record_file.exists());
    }

    /// Review round 1, finding 7: `refresh_session` used to overwrite
    /// `record.session` with no cleanup at all, leaving the OLD session
    /// id's own memory tier (`memory::MemoryScope::Session`) behind forever
    /// -- it is not addressed by `short` (the stable delivery address that
    /// survives a refresh, proven above) and so was never reachable by any
    /// later `release()`, which only ever cleans up the CURRENT session id.
    #[test]
    fn refresh_session_purges_the_previous_cycles_own_memory_tier() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = state_in(tmp.path());
        let repo = tmp.path().join("repo");
        let first_session = "11111111-2222-4333-8444-555555555555";
        let record = record_for(first_session, &repo, Verb::Exec);
        let slug = record.repo_slug.clone();
        let cfg = super::super::config::CtxConfig::default();

        let entry = super::super::memory::Entry {
            key: "cycle-one-key".to_string(),
            written_by: "claude".to_string(),
            written: 1,
            verified: 1,
            source: "explicit".to_string(),
            body: "left behind by the first cycle".to_string(),
            importance: None,
            confidence: None,
            tags: Vec::new(),
            paths: Vec::new(),
        };
        super::super::memory::remember_session(&state, &slug, first_session, &entry, &cfg)
            .expect("remember into the first cycle's session tier");

        let mut guard = SessionGuard::register(&state, record);
        assert_eq!(
            super::super::memory::list_session(&state, &slug, first_session)
                .expect("list before refresh")
                .len(),
            1
        );

        let second_session = "22222222-2222-4333-8444-555555555555";
        guard.refresh_session(second_session);

        assert!(
            super::super::memory::list_session(&state, &slug, first_session)
                .expect("list after refresh")
                .is_empty(),
            "the previous cycle's own memory tier must be purged on refresh"
        );
    }

    /// The point of the stable address, stated as the delivery property it
    /// exists to protect: a sender resolves a live session, addresses a
    /// message at the short id it got, and the supervisor still finds that
    /// message after its session has rotated underneath it.
    #[test]
    fn directed_mail_survives_a_session_rotation_and_still_reaches_the_supervisor() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let home = tempfile::tempdir().expect("tempdir");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());
        let state = state_in(&tmp.path().join("state"));
        let repo = tmp.path().join("repo");
        let cfg = CtxConfig::default();
        let slug = super::super::state::repo_slug(&repo);

        let record = record_for("aaaa1111-2222-4333-8444-555555555555", &repo, Verb::Exec);
        let mut guard = SessionGuard::register(&state, record);

        // A sender resolves the live session and addresses it, exactly as
        // `send --to-session` / `nudge` do.
        let addressed = super::super::sessions::resolve_prefix(&state, "aaaa1111")
            .expect("the live session resolves");
        let msg = super::super::mail::Message {
            from_session: "bbbb2222".to_string(),
            from_agent: "codex".to_string(),
            to: "claude".to_string(),
            to_session: Some(addressed.short.clone()),
            sent: super::super::state::now_secs(),
            body: "the webhook route moved".to_string(),
        };
        super::super::mail::store(&state, &slug, &msg, &cfg).expect("store");

        // ... and then the supervisor restarts, minting a fresh session id.
        guard.refresh_session("cccc3333-2222-4333-8444-555555555555");

        let delivered =
            super::super::mail::list(&state, &slug, Some("claude"), Some(guard.short()))
                .expect("list");
        assert_eq!(
            delivered.len(),
            1,
            "the message must still reach the supervisor it was addressed to"
        );
        assert_eq!(delivered[0].1.body, "the webhook route moved");

        // And it is still *only* reachable by that address: a different
        // supervisor must not pick it up.
        let other = super::super::mail::list(&state, &slug, Some("claude"), Some("zzzz9999"))
            .expect("list");
        assert!(
            other.is_empty(),
            "directed mail stays directed: {:?}",
            other
        );
    }

    #[test]
    fn refreshing_after_release_is_a_harmless_noop() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = state_in(tmp.path());
        let repo = tmp.path().join("repo");
        let record = record_for("12121212-2222-4333-8444-555555555555", &repo, Verb::Loop);
        let path = record_path(&state, &record.short);

        let mut guard = SessionGuard::register(&state, record);
        guard.release();
        assert!(!path.exists());

        guard.refresh_session("34343434-2222-4333-8444-555555555555");
        let new_path = record_path(&state, &short_id("34343434-2222-4333-8444-555555555555"));
        assert!(
            !new_path.exists(),
            "a released guard must not resurrect a record on refresh"
        );
    }

    #[test]
    fn verb_round_trips_through_json_as_a_lowercase_word() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = state_in(tmp.path());
        let repo = tmp.path().join("repo");
        for verb in [Verb::Exec, Verb::Loop, Verb::Wrap, Verb::Chat, Verb::Dash] {
            let record = record_for("00000000-2222-4333-8444-555555555555", &repo, verb);
            let path = write_record(&state, &record);
            let raw = std::fs::read_to_string(&path).expect("read");
            assert!(
                raw.contains(&format!("\"{}\"", verb.as_str())),
                "verb {verb} must serialize as its lowercase word: {raw}"
            );
        }
    }

    #[test]
    fn verb_dash_serializes_lowercase() {
        assert_eq!(Verb::Dash.as_str(), "dash");
        let json = serde_json::to_string(&Verb::Dash).expect("serialize");
        assert_eq!(json, "\"dash\"");
        let back: Verb = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, Verb::Dash);
    }

    // N4: `zirv ctx nudge`.

    pub(super) fn env_map(pairs: &[(&str, &str)]) -> std::collections::HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect()
    }

    /// `wrap::publish_socket_path` writes `<state>/socket-path-<short>` and
    /// only a graceful exit unpublishes it, so a killed or crashed
    /// supervisor leaves one behind forever (46 of them on one real machine)
    /// -- and `wrap::read_socket_path` with no session picks the NEWEST such
    /// file by mtime, with no liveness check at all. They are swept on the
    /// same read as every other orphan.
    #[test]
    fn a_dead_sessions_published_socket_path_is_swept() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = state_in(tmp.path());
        let repo = tmp.path().join("repo");
        let published = |short: &str| {
            state
                .root()
                .join(format!("{}{short}", super::super::wrap::SOCKET_PATH_PREFIX))
        };
        // The socket file's STEM is what `signal::probe` derives its
        // machine-global pipe name from on Windows, so it has to be unique
        // to this process or a sibling test's live endpoint can answer the
        // probe for a path this test only made up.
        let publish = |short: &str| {
            let socket = tmp
                .path()
                .join(format!("{short}-{}.sock", std::process::id()));
            super::super::state::write_private(&published(short), &socket.display().to_string())
                .expect("publish");
        };

        let live = record_for("11111111-2222-4333-8444-555555555555", &repo, Verb::Exec);
        let live_short = live.short.clone();
        write_record(&state, &live);
        publish(&live_short);

        let mut dead = record_for("22222222-2222-4333-8444-555555555555", &repo, Verb::Exec);
        dead.pid = dead_pid();
        let dead_short = dead.short.clone();
        write_record(&state, &dead);
        publish(&dead_short);

        publish("99999999");

        let _ = list(&state);

        assert!(
            published(&live_short).is_file(),
            "a live session's published socket path is left alone"
        );
        assert!(
            !published(&dead_short).exists(),
            "a dead session's published socket path is swept with its record"
        );
        assert!(
            !published("99999999").exists(),
            "a published socket path with no record at all is swept too"
        );
    }

    /// 3.25.0 startup freeze: `sweep_orphan_socket_paths` (new in 3.25.0)
    /// probes every leftover `socket-path-<short>` file through
    /// `signal::probe`, and the Windows `win::connect` behind it treats
    /// `ERROR_FILE_NOT_FOUND` -- "there is no such pipe", the definitive
    /// answer for a *probe* -- as transient and keeps retrying for a full
    /// `CONNECT_RETRY` second per file. `sessions::list` runs on the
    /// dashboard's startup path before the first frame is ever drawn, so a
    /// machine carrying the leftovers this sweep exists to clean (the sweep's
    /// own doc comment counts 46 on one real machine) paid one second each
    /// with nothing on screen. A probe must answer at once, on every
    /// platform: unix `connect` to a missing socket already returns ENOENT
    /// immediately, so this budget only ever bites on Windows.
    #[test]
    fn an_orphan_socket_path_sweep_does_not_block_on_dead_endpoints() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = state_in(tmp.path());
        let _ = super::super::state::create_private_dir_all(&state.sessions());
        const ORPHANS: usize = 8;
        let published = |i: usize| {
            state.root().join(format!(
                "{}dead{i:04}",
                super::super::wrap::SOCKET_PATH_PREFIX
            ))
        };
        for i in 0..ORPHANS {
            // A socket naming a pipe nothing has ever bound, unique to this
            // process so no sibling test's live endpoint can answer it.
            let socket = tmp
                .path()
                .join(format!("gone-{}-{i}.sock", std::process::id()));
            super::super::state::write_private(&published(i), &socket.display().to_string())
                .expect("publish");
        }

        let started = std::time::Instant::now();
        let _ = list(&state);
        let elapsed = started.elapsed();

        for i in 0..ORPHANS {
            assert!(!published(i).exists(), "orphan {i} must still be swept");
        }
        assert!(
            elapsed < std::time::Duration::from_millis(500),
            "{ORPHANS} dead endpoints took {elapsed:?} to probe; a probe must not retry a              nonexistent endpoint (one second each here blocks the dashboard before its              first frame)"
        );
    }

    /// #681 review: a staged rollover socket is named `<short>.<nonce>`, not
    /// `<short>.sock`, and a crashed dashboard leaves it behind all the same.
    #[test]
    fn a_dead_staged_rollover_endpoint_is_swept() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = state_in(tmp.path());
        super::super::state::create_private_dir_all(&state.sockets()).expect("sockets dir");
        let staged = state.sockets().join("deadbeef.ab12");
        std::fs::write(&staged, "").expect("staged endpoint");
        let _ = list(&state);
        assert!(!staged.exists(), "a dead staged endpoint must be swept");
    }

    /// Issue #146: `is_alive` must read `EPERM` (the process exists, this
    /// caller just cannot signal it) as alive, not dead. Pid 1 is owned by
    /// root, exists on every unix box, and -- for a non-root caller, which is
    /// what CI and a sandboxed `zirv ctx send` both run as -- `kill(1, 0)`
    /// returns exactly `EPERM`. Skipped only for the (rare, unsandboxed) case
    /// of running as root, where `kill(1, 0)` succeeds outright and the
    /// assertion holds anyway for a different reason -- so root does not
    /// need its own branch, only its own explanatory skip.
    #[cfg(unix)]
    #[test]
    fn eperm_against_a_real_process_reads_as_alive_not_dead() {
        // SAFETY: `geteuid` takes no arguments and only reads process state.
        if unsafe { libc::geteuid() } == 0 {
            eprintln!("skipping: running as root, kill(1, 0) succeeds outright");
            return;
        }
        assert!(
            is_alive(1),
            "pid 1 exists and is owned by root; a non-root caller's kill(1, 0) is EPERM, which \
             must read as alive"
        );
    }

    /// The other half: a pid that has genuinely exited (spawned, waited on)
    /// must still read as dead. `EPERM` must not have swallowed `ESRCH` too.
    #[test]
    fn a_waited_on_pid_reads_as_dead() {
        assert!(!is_alive(dead_pid()));
    }

    /// A record written by a build that predates the field must still parse,
    /// and must be treated as an ordinary reachable session.
    #[test]
    fn a_record_without_the_reachable_field_parses_as_reachable() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = state_in(tmp.path());
        super::super::state::create_private_dir_all(&state.sessions()).expect("mkdir");
        let legacy = format!(
            r#"{{"session":"abcdef12-3456-4789-8abc-def012345678","short":"abcdef12",
               "agent":"claude","repo":"/work/repo","repo_slug":"-work-repo",
               "verb":"wrap","pid":{},"started_at":1700000000}}"#,
            std::process::id()
        );
        std::fs::write(state.sessions().join("abcdef12.json"), legacy).expect("write");

        let found = list(&state);
        assert_eq!(found.len(), 1, "the legacy record still parses: {found:?}");
        assert!(
            found[0].0.reachable,
            "an older record has no opinion, so it is treated as reachable"
        );
    }

    pub(super) fn sh(script: &str) -> std::process::Command {
        let mut cmd = std::process::Command::new("sh");
        cmd.arg("-c").arg(script);
        cmd
    }

    /// Security review round 2 (Finding 7): the POSIX `ps -o etime=` shapes,
    /// and nothing else read as an age.
    #[test]
    fn parse_etime_reads_the_posix_elapsed_time_format() {
        assert_eq!(parse_etime("00:07"), Some(7));
        assert_eq!(parse_etime("       01:30"), Some(90));
        assert_eq!(parse_etime("02:03:04"), Some(7_384));
        assert_eq!(parse_etime("3-04:05:06\n"), Some(273_906));
        assert_eq!(parse_etime(""), None);
        assert_eq!(parse_etime("7"), None, "a bare number is not an etime");
        assert_eq!(parse_etime("a:bc"), None);
        assert_eq!(parse_etime("1:2:3:4"), None);
    }

    /// Issue #152's own pure comparator: only a start-time pair that both
    /// exist AND disagree by more than the tolerance means "a different
    /// process now holds this pid". No process spawning -- everything here
    /// is plain arithmetic.
    #[test]
    fn start_time_disambiguates_dead_only_flags_a_mismatch_beyond_tolerance() {
        assert!(
            !start_time_disambiguates_dead(Some(1_000), Some(1_000)),
            "identical start times are obviously the same process"
        );
        assert!(
            !start_time_disambiguates_dead(Some(1_000), Some(1_000 + START_TIME_TOLERANCE_SECS)),
            "the tolerance absorbs a coarse reading and a stepped clock"
        );
        assert!(
            start_time_disambiguates_dead(Some(1_000), Some(1_000 + START_TIME_TOLERANCE_SECS + 1)),
            "beyond the tolerance is a different process"
        );
        assert!(
            start_time_disambiguates_dead(Some(10_000), Some(1_000)),
            "the mismatch is symmetric -- either side reading later or earlier than the other \
             still means two different processes"
        );
        assert!(
            !start_time_disambiguates_dead(None, Some(1_000)),
            "no recorded start time -- cannot tell, degrade to alive"
        );
        assert!(
            !start_time_disambiguates_dead(Some(1_000), None),
            "no freshly-read start time -- cannot tell, degrade to alive"
        );
        assert!(
            !start_time_disambiguates_dead(None, None),
            "neither side known -- cannot tell, degrade to alive"
        );
    }

    /// `process_start_secs` smoke test: this test process's own start time
    /// is readable, and reading it twice gives (very close to) the same
    /// answer -- it is not approximated freshly relative to "now" in a way
    /// that would drift materially between two calls a moment apart.
    ///
    /// Review round 2 finding 4: exact equality was too strict. Each read is
    /// an independent `now_secs() - ps_etime_derived_age` derivation, and
    /// `ps`'s own `etime` field is whole-second/whole-minute granularity
    /// (rounding differently depending on exactly when within that window
    /// each `ps` invocation lands), so two reads a moment apart can
    /// legitimately land a second or two either side of each other without
    /// anything about the process's real start time having changed. `2`
    /// comfortably covers that rounding while still catching a reader that
    /// is actually broken (drifting with "now" rather than anchored to the
    /// process).
    #[cfg(unix)]
    #[test]
    fn process_start_secs_of_this_process_is_stable_across_two_reads() {
        let Some(first) = process_start_secs(std::process::id()) else {
            eprintln!("skipping: no usable `ps` in this environment, so no start time to read");
            return;
        };
        let Some(second) = process_start_secs(std::process::id()) else {
            eprintln!("skipping: `ps` became unusable between the two reads");
            return;
        };
        assert!(
            first.abs_diff(second) <= 2,
            "the same process read twice must report nearly the same start time: {first} vs \
             {second}"
        );
    }

    /// The one branch a bare `is_alive` cannot get right for a `Record`:
    /// `EPERM` alone cannot tell the session's own process apart from an
    /// unrelated one the OS later recycled its pid to (issue #152). Forcing
    /// a real `EPERM` against a process this test spawns itself is not
    /// possible -- a caller always has permission to signal its own child,
    /// so `kill(pid, 0)` on one always succeeds (see the next test for that
    /// branch instead). Pid 1 is the same real-`EPERM` source
    /// `eperm_against_a_real_process_reads_as_alive_not_dead` above already
    /// relies on: it exists, is owned by root, and (for a non-root caller)
    /// always answers `EPERM`.
    #[cfg(unix)]
    #[test]
    fn record_is_alive_disambiguates_an_eperm_pid_by_start_time() {
        // SAFETY: `geteuid` takes no arguments and only reads process state.
        if unsafe { libc::geteuid() } == 0 {
            eprintln!("skipping: running as root, kill(1, 0) succeeds outright");
            return;
        }
        // Review round 2 finding 5: `geteuid() == 0` alone is not a reliable
        // enough guard. Under `docker run --user <uid>`, or any other setup
        // where this test's own uid happens to already own pid 1 (a
        // namespaced/rootless container's pid 1 is not always root's), a
        // non-root euid can still get `kill(1, 0) == 0` -- `CanSignal`, not
        // `EPERM` -- which would make this test's `!record_is_alive`
        // assertion below deterministically false regardless of start_time.
        // Ask the same question `probe_signal` itself would, directly.
        // SAFETY: signal 0 sends nothing; it only probes existence and
        // permission.
        if unsafe { libc::kill(1, 0) } == 0 {
            eprintln!("skipping: kill(1, 0) succeeds outright in this sandbox");
            return;
        }
        let Some(pid1_start) = process_start_secs(1) else {
            eprintln!("skipping: no usable `ps` in this environment, so no start time to check");
            return;
        };

        let tmp = tempfile::tempdir().expect("tempdir");
        let repo = tmp.path().join("repo");

        let mut mismatched = record_for("33333333-2222-4333-8444-555555555555", &repo, Verb::Exec);
        mismatched.pid = 1;
        mismatched.start_time = Some(pid1_start.saturating_sub(3 * 3_600));
        assert!(
            !record_is_alive(&mismatched),
            "pid 1's own start time is hours off from the record's -- a different process now \
             holds it"
        );

        let mut matching = mismatched.clone();
        matching.start_time = Some(pid1_start);
        assert!(
            record_is_alive(&matching),
            "start times agree -- still the same process"
        );

        let mut unknown = mismatched.clone();
        unknown.start_time = None;
        assert!(
            record_is_alive(&unknown),
            "no recorded start time to compare -- degrade to EPERM's old alive answer"
        );
    }

    /// `record_is_alive`'s `kill(pid, 0)` success branch is unconditional by
    /// design (issue #152): a caller that can actually signal the process
    /// needs no second opinion from a start time, however far off a
    /// stale/fabricated one is. Proven through `list`'s own sweep -- the one
    /// production call site this matters for -- against a real, owned, live
    /// child process, which is exactly why the `EPERM` branch above has to
    /// be tested against pid 1 instead: this kind of process can never
    /// produce a real `EPERM` to exercise it.
    #[cfg(unix)]
    #[test]
    fn a_real_live_owned_pid_stays_live_even_with_a_wildly_wrong_start_time() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let home = tempfile::tempdir().expect("tempdir");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());
        let state_dir = tmp.path().join("state");
        let state = state_in(&state_dir);
        let repo = tmp.path().join("repo");

        let mut child = sh("sleep 30").spawn().expect("spawn a stand-in process");
        let pid = child.id();

        let mut record = record_for("55555555-2222-4333-8444-555555555555", &repo, Verb::Exec);
        record.pid = pid;
        record.start_time = Some(super::super::state::now_secs().saturating_sub(3 * 3_600));
        let short = record.short.clone();
        let path = record_path(&state, &short);
        write_record(&state, &record);

        assert!(
            record_is_alive(&record),
            "kill(pid, 0) succeeds for our own child regardless of start_time"
        );
        let found = list(&state);
        let (_, liveness) = found
            .iter()
            .find(|(r, _)| r.short == short)
            .expect("the record is still listed");
        assert_eq!(*liveness, Liveness::Live);
        assert!(path.exists(), "list must not have swept a live record");

        let _ = child.kill();
        let _ = child.wait();
    }
}
