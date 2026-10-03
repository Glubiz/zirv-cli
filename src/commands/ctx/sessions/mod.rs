//! Session registry: `<state>/sessions/<short8>.json`, one file per live
//! supervisor, keyed by the socket short id. Registry writes are best-effort;
//! malformed entries cannot fail a listing or a launch.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::config::{CtxConfig, EnvLookup, env_from_process};
use super::runtime::RuntimeKind;
use super::state::{self, StateDir};

mod bookkeeping;
mod nesting;
mod ops;
pub(crate) mod secret_env;

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

/// First eight ASCII-alphanumeric session-id characters, matching the
/// socket address derivation.
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

/// Registry verb is independent of prompt role.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Verb {
    Exec,
    Loop,
    Wrap,
    Chat,
    /// Dashboard worker pane, distinct from its orchestrator pane.
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
    /// False when no turn-signal socket can claim nudges; status still shows
    /// the session as unreachable. Old records default to true.
    #[serde(default = "reachable_default")]
    pub reachable: bool,
    /// Registering process pid; panes belong to their dashboard process.
    /// Requester-side fallbacks remain outside its sidebar.
    #[serde(default)]
    pub owner_pid: Option<u32>,
    /// Launch policy fingerprint for later drift checks; absent when no
    /// attestation snapshot exists. (#139)
    #[serde(default)]
    pub safety_policy_sha256: Option<String>,
    /// Spawned prompt role, stamped by the supervisor rather than claimed
    /// by the session; missing roles fall back conservatively. (#169)
    #[serde(default)]
    pub role: Option<String>,
    /// Registered process start time for recycled-pid checks; unavailable
    /// probes leave liveness uncertain rather than falsely dead. (#152)
    #[serde(default)]
    pub start_time: Option<u64>,
    /// Active-turn crash witness, cleared at a clean boundary. (#281)
    #[serde(default)]
    pub in_flight: Option<InFlight>,
    /// Session backend; old records default to Harness. (#470)
    #[serde(default)]
    pub runtime: RuntimeKind,
}

/// A marker left by a dead supervisor identifies a turn interrupted before
/// its clean boundary. (#281)
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InFlight {
    /// Supervisor verb to name in an interruption witness.
    pub verb: String,
    /// Last known turn number for display only.
    pub turn: u64,
    pub since: u64,
}

fn reachable_default() -> bool {
    true
}

impl Record {
    /// Registry starts with the filing supervisor's own pid.
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
            // Default reachable; wrap alone may lack a signal socket.
            reachable: true,
            owner_pid: None,
            safety_policy_sha256: None,
            role: None,
            // Stamp this process's start time for later pid checks. (#152)
            start_time: process_start_secs(std::process::id()),
            in_flight: None,
            runtime: RuntimeKind::Harness,
        }
    }

    /// Mark a session unable to claim wake-up markers after failed bind.
    pub fn unreachable(mut self) -> Self {
        self.reachable = false;
        self
    }

    /// Stamp the launch policy fingerprint when available. (#139)
    pub fn with_safety_policy_sha256(mut self, fingerprint: Option<String>) -> Self {
        self.safety_policy_sha256 = fingerprint;
        self
    }

    /// Stamp the role chosen by the spawning server, never from a session
    /// request. (#169)
    pub fn with_role(mut self, role: &str) -> Self {
        self.role = Some(role.to_string());
        self
    }

    /// Keep the supervisor's delivery address through a harness change. (#186)
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

/// Registry write failure must never fail launch.
fn write_record(state: &StateDir, record: &Record) -> PathBuf {
    let path = record_path(state, &record.short);
    let _ = super::state::create_private_dir_all(&state.sessions());
    if let Ok(json) = serde_json::to_string_pretty(record) {
        let _ = super::state::write_private(&path, &json);
    }
    path
}

/// Explicitly release on every exit path; panic = "abort" makes Drop
/// unreliable for cleanup.
#[derive(Debug)]
pub struct SessionGuard {
    state: StateDir,
    record: Record,
    path: PathBuf,
    released: bool,
}

impl SessionGuard {
    /// Stamp the registering process as owner; panes register from their
    /// dashboard, while requester-side fallbacks own themselves.
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

    // Use the guard's actual verb when marking an in-flight turn. (#281)
    pub fn record(&self) -> &Record {
        &self.record
    }

    /// Refresh the native session id while retaining this supervisor's
    /// stable short delivery address across cycles and restarts.
    pub fn refresh_session(&mut self, new_session: &str) {
        if self.released {
            return;
        }
        // Remove the old session's memory tier on rotation; release only
        // sees the current id. Failure is best-effort. (#295)
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

    /// Adopt the child pid after spawn and each relaunch, keeping owner pid
    /// unchanged. Move start time with pid or liveness may reject a live
    /// child under EPERM. (#152, #146)
    pub fn adopt_child_pid(&mut self, pid: u32) {
        if self.released || self.record.pid == pid {
            return;
        }
        self.record.pid = pid;
        self.record.start_time = process_start_secs(pid);
        self.path = write_record(&self.state, &self.record);
    }

    /// Mark one turn in flight best-effort; repeated input chunks must not
    /// rewrite the witness. (#281)
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

    /// Clear the witness at a clean turn boundary.
    pub fn clear_in_flight(&mut self) {
        if self.released || self.record.in_flight.is_none() {
            return;
        }
        self.record.in_flight = None;
        self.path = write_record(&self.state, &self.record);
    }

    /// Stable short address for this supervisor's mail and nudges.
    pub fn short(&self) -> &str {
        &self.record.short
    }

    /// Give up the guard without removing its registry record. (#552)
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
        // Remove screening and workflow siblings with the record; orphan
        // sweeps handle crashed supervisors. (#243)
        let _ = std::fs::remove_file(screening_path(&self.state, &self.record.short));
        let _ = std::fs::remove_file(workflow_path(&self.state, &self.record.short));
        // Session-tier memory must not outlive its session; cleanup is
        // best-effort and keyed by the current session id. (#295)
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

/// Distinguish signal-0 permission denial from missing process. (#152)
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

/// Signal 0 proves existence when allowed; EPERM also means alive, since
/// sandboxed callers may lack permission to signal a live process. A bare
/// pid cannot distinguish an unrelated process that reused its number. (#146, #145, #152)
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

/// Allow clock steps and ps rounding when comparing independent start-time
/// readings; this wider tolerance must not be unified with kill's one-sided
/// registration-order check. (#152, #146)
#[cfg_attr(not(unix), allow(dead_code))]
const START_TIME_TOLERANCE_SECS: u64 = 300;

/// Compare recorded and current start times only when both exist; missing
/// data preserves EPERM-is-alive rather than falsely sweeping a record. (#152)
pub(crate) fn start_time_disambiguates_dead(recorded: Option<u64>, current: Option<u64>) -> bool {
    match (recorded, current) {
        (Some(recorded), Some(current)) => recorded.abs_diff(current) > START_TIME_TOLERANCE_SECS,
        _ => false,
    }
}

/// Derive start time where supported and available; repointing a record's
/// pid must also refresh this timestamp. (#152)
pub(crate) fn process_start_secs(pid: u32) -> Option<u64> {
    let age = process_age_secs(pid)?;
    Some(super::state::now_secs().saturating_sub(age))
}

/// On Unix, only EPERM requires start-time disambiguation; missing times
/// preserve alive. Other signal-0 outcomes and non-Unix follow is_alive. (#152)
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

/// Read liveness without sweeping the registry, so restore cannot duplicate
/// an agent whose pane process survived its dashboard.
pub fn short_is_live(state: &StateDir, short: &str) -> bool {
    load_record(state, short).is_some_and(|record| record_is_alive(&record))
}

/// Read one record without liveness judgment or registry cleanup. (#169)
pub fn load_record(state: &StateDir, short: &str) -> Option<Record> {
    std::fs::read_to_string(record_path(state, short))
        .ok()
        .and_then(|contents| serde_json::from_str::<Record>(&contents).ok())
}

/// List records with liveness and sweep stale files, still returning swept
/// records for reporting; malformed files cannot fail the listing.
pub fn list(state: &StateDir) -> Vec<(Record, Liveness)> {
    let cfg = CtxConfig::load(Path::new("."), &env_from_process()).unwrap_or_default();
    list_with_retention(state, cfg.dash.roster_max_age_secs)
}

pub fn list_with_retention(state: &StateDir, retention_secs: u64) -> Vec<(Record, Liveness)> {
    let mut found = Vec::new();
    let now = state::now_secs();
    // A missing sessions directory means no records, but endpoint and
    // marker sweeps must still run. (#99)
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
        // Sort by launch time and short id so dashboard rows stay stable
        // across filesystem enumeration orders.
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

/// Remove a published socket path only when no live record or answering
/// endpoint owns it; a live unregistered supervisor keeps its path.
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

/// Sweep orphan endpoints only after probing them; an answering socket
/// may belong to a live supervisor whose registry write failed. (#99)
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
        // An approvals inbox socket is `a<pid>.sock`: its owner's liveness decides, because a
        // sandboxed caller's denied connect would read as dead and delete a live inbox (#865).
        if short
            .strip_prefix('a')
            .and_then(|pid| pid.parse::<u32>().ok())
            .is_some_and(is_alive)
        {
            continue;
        }
        if !super::signal::probe(&path) {
            let _ = std::fs::remove_file(&path);
        }
    }
}

/// Live or staged turn-signal endpoint file.
pub(crate) fn is_endpoint_file(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|ext| {
            ext == "sock" || (ext.len() == 4 && ext.bytes().all(|b| b.is_ascii_hexdigit()))
        })
}

#[derive(Debug, Clone, PartialEq)]
pub enum ResolveError {
    /// No live prefix match; include available short ids.
    NotFound { existing: Vec<String> },
    /// Ambiguous prefix; report candidate short ids, including parked seats. (#721)
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

/// Add the checked state-dir path to a resolution error so an empty
/// registry can be distinguished from a mismatched location. (#146)
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

/// Resolve a prefix only among live records; listing sweeps stale ones.
pub fn resolve_prefix(state: &StateDir, prefix: &str) -> Result<Record, ResolveError> {
    let live: Vec<Record> = list(state)
        .into_iter()
        .filter(|(_, liveness)| *liveness == Liveness::Live)
        .map(|(record, _)| record)
        .collect();

    // An exact short or session id always wins over a worker name.
    if let Some(exact) = live
        .iter()
        .find(|r| r.short == prefix || r.session == prefix)
    {
        return Ok(exact.clone());
    }

    // A worker's name (`zirv agent --name`) addresses it too; a name and a prefix of
    // different sessions together are ambiguous.
    let names = if prefix.is_empty() {
        Default::default()
    } else {
        super::graph::agent_names(state)
    };
    let matches: Vec<Record> = live
        .iter()
        .filter(|r| {
            names
                .get(&r.short)
                .is_some_and(|n| n.eq_ignore_ascii_case(prefix))
                || r.short.starts_with(prefix)
                || r.session.starts_with(prefix)
        })
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

/// Address resolves to one live record or a ghost-parked seat. (#721)
#[derive(Debug)]
pub enum Addressed {
    Live(Box<Record>),
    Parked(Box<super::seat::Seat>),
}

/// Interpret a registry miss as a parked seat only for a unique parked
/// prefix; ambiguity still reports candidate short ids. (#721)
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

/// Parse POSIX elapsed time; malformed output means unknown age.
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

/// Read process age with ps; missing or refused probes mean unknown age,
/// never evidence that a record is stale.
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

/// This platform cannot distinguish a recycled pid by start time.
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
    fn a_worker_name_addresses_a_live_session_and_a_shared_name_is_ambiguous() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = state_in(tmp.path());
        let repo = tmp.path().join("repo");
        let launch = |session: &str, name: &str| {
            let mut record = record_for(session, &repo, Verb::Wrap);
            record.pid = std::process::id();
            write_record(&state, &record);
            super::super::graph::record_worker_launch(
                &state,
                &repo,
                &super::super::graph::Launch {
                    session,
                    origin: "pane",
                    parent_session: None,
                    harness: Some("codex"),
                    model: None,
                    task: None,
                    name: Some(name),
                    workdir: None,
                },
                1,
            );
        };
        launch("aaaaaaaa-1111-4222-8333-444444444444", "review-dash");
        launch("bbbbbbbb-1111-4222-8333-444444444444", "fix-tests");
        assert_eq!(
            resolve_prefix(&state, "Review-Dash")
                .expect("by name")
                .short,
            "aaaaaaaa"
        );
        assert_eq!(
            resolve_prefix(&state, "bbbb").expect("by prefix").short,
            "bbbbbbbb"
        );
        launch("cccccccc-1111-4222-8333-444444444444", "fix-tests");
        assert!(matches!(
            resolve_prefix(&state, "fix-tests"),
            Err(ResolveError::Ambiguous(shorts)) if shorts.len() == 2
        ));
    }

    #[test]
    fn a_hex_looking_worker_name_never_shadows_another_sessions_short_id() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = state_in(tmp.path());
        let repo = tmp.path().join("repo");
        let launch = |session: &str, name: Option<&str>| {
            let mut record = record_for(session, &repo, Verb::Wrap);
            record.pid = std::process::id();
            write_record(&state, &record);
            super::super::graph::record_worker_launch(
                &state,
                &repo,
                &super::super::graph::Launch {
                    session,
                    origin: "pane",
                    parent_session: None,
                    harness: Some("codex"),
                    model: None,
                    task: None,
                    name,
                    workdir: None,
                },
                1,
            );
        };
        launch("aaaaaaaa-1111-4222-8333-444444444444", Some("cafe"));
        launch("cafe12ab-1111-4222-8333-444444444444", None);
        assert!(matches!(
            resolve_prefix(&state, "cafe"),
            Err(ResolveError::Ambiguous(shorts)) if shorts.len() == 2
        ));
        assert_eq!(
            resolve_prefix(&state, "cafe12ab")
                .expect("full short id")
                .short,
            "cafe12ab"
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

    /// #865: an inbox socket whose owner pid is alive is kept even when no probe can connect to
    /// it (a sandboxed caller's EPERM, here a plain file nobody listens on); a dead owner's is swept.
    #[test]
    fn an_approvals_socket_is_kept_while_its_owner_pid_lives_and_swept_when_dead() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = state_in(tmp.path());
        super::super::state::create_private_dir_all(&state.sockets()).expect("sockets dir");
        let alive = state
            .sockets()
            .join(format!("a{}.sock", std::process::id()));
        // Above Linux/macOS pid_max (and within i32), so no process can own it.
        let dead = state.sockets().join("a2000000000.sock");
        std::fs::write(&alive, "").expect("alive endpoint");
        std::fs::write(&dead, "").expect("dead endpoint");
        let _ = list(&state);
        assert!(alive.exists(), "a live owner's inbox socket must survive");
        assert!(!dead.exists(), "a dead owner's inbox socket must be swept");
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
